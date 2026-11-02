//! `ext_foreign_toplevel_list_v1` state: per-client lists and handles for mapped windows.

use std::collections::{HashMap, HashSet};

use lumalla_wayland_protocol::{
    ClientId, ObjectId,
    buffer::Writer,
    registry::{InterfaceIndex, Registry},
};

use crate::ConnectedClients;

type ResourceKey = (ClientId, ObjectId);

#[derive(Debug, Clone)]
struct WindowMeta {
    identifier: String,
    title: String,
    app_id: String,
    mapped: bool,
}

#[derive(Debug, Clone)]
struct ListBinding {
    /// Handles created for this list: handle object → window id.
    handles: HashMap<ObjectId, u32>,
    stopped: bool,
}

#[derive(Debug, Default)]
pub struct ForeignToplevelListManager {
    next_generation: u64,
    windows: HashMap<u32, WindowMeta>,
    lists: HashMap<ResourceKey, ListBinding>,
    /// Reverse lookup: handle → window id (across lists).
    handle_to_window: HashMap<ResourceKey, u32>,
    /// window id → live handles watching it.
    window_handles: HashMap<u32, HashSet<ResourceKey>>,
}

impl ForeignToplevelListManager {
    pub fn ensure_window(&mut self, window_id: u32, title: &str, app_id: &str) {
        if !self.windows.contains_key(&window_id) {
            self.next_generation = self.next_generation.saturating_add(1);
            let generation = self.next_generation;
            self.windows.insert(
                window_id,
                WindowMeta {
                    identifier: format!("w{window_id}g{generation}"),
                    title: title.to_owned(),
                    app_id: app_id.to_owned(),
                    mapped: false,
                },
            );
            return;
        }
        if let Some(meta) = self.windows.get_mut(&window_id) {
            meta.title = title.to_owned();
            meta.app_id = app_id.to_owned();
        }
    }

    pub fn set_title(&mut self, window_id: u32, title: String) {
        if let Some(meta) = self.windows.get_mut(&window_id) {
            if meta.title == title {
                return;
            }
            meta.title = title;
        }
    }

    pub fn set_app_id(&mut self, window_id: u32, app_id: String) {
        if let Some(meta) = self.windows.get_mut(&window_id) {
            if meta.app_id == app_id {
                return;
            }
            meta.app_id = app_id;
        }
    }

    pub fn resolve_handle(&self, client_id: ClientId, handle: ObjectId) -> Option<u32> {
        self.handle_to_window.get(&(client_id, handle)).copied()
    }

    pub fn delete_client(&mut self, client_id: ClientId) {
        let list_keys: Vec<_> = self
            .lists
            .keys()
            .filter(|(cid, _)| *cid == client_id)
            .copied()
            .collect();
        for key in list_keys {
            self.destroy_list(key.0, key.1);
        }
        let handle_keys: Vec<_> = self
            .handle_to_window
            .keys()
            .filter(|(cid, _)| *cid == client_id)
            .copied()
            .collect();
        for key in handle_keys {
            self.destroy_handle(key.0, key.1);
        }
    }

    pub fn destroy_list(&mut self, client_id: ClientId, list: ObjectId) {
        let Some(binding) = self.lists.remove(&(client_id, list)) else {
            return;
        };
        for handle in binding.handles.keys().copied() {
            self.detach_handle(client_id, handle);
        }
    }

    pub fn destroy_handle(&mut self, client_id: ClientId, handle: ObjectId) {
        self.detach_handle(client_id, handle);
    }

    fn detach_handle(&mut self, client_id: ClientId, handle: ObjectId) {
        let key = (client_id, handle);
        let Some(window_id) = self.handle_to_window.remove(&key) else {
            return;
        };
        if let Some(set) = self.window_handles.get_mut(&window_id) {
            set.remove(&key);
            if set.is_empty() {
                self.window_handles.remove(&window_id);
            }
        }
        for binding in self.lists.values_mut() {
            binding.handles.remove(&handle);
        }
    }

    /// Bind a new list object and advertise currently mapped windows.
    pub fn bind_list(
        &mut self,
        client_id: ClientId,
        list: ObjectId,
        registry: &mut Registry,
        writer: &mut Writer,
    ) {
        self.lists.insert(
            (client_id, list),
            ListBinding {
                handles: HashMap::new(),
                stopped: false,
            },
        );
        let mapped: Vec<u32> = self
            .windows
            .iter()
            .filter(|(_, meta)| meta.mapped)
            .map(|(id, _)| *id)
            .collect();
        for window_id in mapped {
            let _ = self.emit_toplevel_for_list(client_id, list, window_id, registry, writer);
        }
    }

    pub fn stop(&mut self, client_id: ClientId, list: ObjectId, writer: &mut Writer) {
        let Some(binding) = self.lists.get_mut(&(client_id, list)) else {
            return;
        };
        if binding.stopped {
            return;
        }
        binding.stopped = true;
        writer.ext_foreign_toplevel_list_v1_finished(list);
    }

    /// Mark a window mapped and advertise it to all active lists.
    pub fn notify_mapped(
        &mut self,
        window_id: u32,
        title: &str,
        app_id: &str,
        clients: &mut ConnectedClients,
    ) {
        self.ensure_window(window_id, title, app_id);
        let Some(meta) = self.windows.get_mut(&window_id) else {
            return;
        };
        if meta.mapped {
            self.broadcast_properties(window_id, clients);
            return;
        }
        meta.mapped = true;
        let list_keys: Vec<_> = self
            .lists
            .iter()
            .filter(|(_, b)| !b.stopped)
            .map(|(k, _)| *k)
            .collect();
        for (client_id, list) in list_keys {
            let Some(client) = clients.get_mut(&client_id) else {
                continue;
            };
            let (registry, writer) = client.registry_and_writer_mut();
            let _ = self.emit_toplevel_for_list(client_id, list, window_id, registry, writer);
        }
    }

    /// Mark a window unmapped and send `closed` to all handles.
    pub fn notify_unmapped(&mut self, window_id: u32, clients: &mut ConnectedClients) {
        let Some(meta) = self.windows.get_mut(&window_id) else {
            return;
        };
        if !meta.mapped {
            return;
        }
        meta.mapped = false;
        let handles: Vec<ResourceKey> = self
            .window_handles
            .get(&window_id)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default();
        for (client_id, handle) in handles {
            if let Some(client) = clients.get_mut(&client_id) {
                client
                    .writer_mut()
                    .ext_foreign_toplevel_handle_v1_closed(handle);
            }
            self.detach_handle(client_id, handle);
        }
        // Drop window meta so a future remap gets a fresh identifier.
        self.windows.remove(&window_id);
    }

    pub fn broadcast_properties(&mut self, window_id: u32, clients: &mut ConnectedClients) {
        let Some(meta) = self.windows.get(&window_id).cloned() else {
            return;
        };
        if !meta.mapped {
            return;
        }
        let handles: Vec<ResourceKey> = self
            .window_handles
            .get(&window_id)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default();
        for (client_id, handle) in handles {
            let Some(client) = clients.get_mut(&client_id) else {
                continue;
            };
            Self::send_properties(client.writer_mut(), handle, &meta);
        }
    }

    fn emit_toplevel_for_list(
        &mut self,
        client_id: ClientId,
        list: ObjectId,
        window_id: u32,
        registry: &mut Registry,
        writer: &mut Writer,
    ) -> anyhow::Result<()> {
        let Some(meta) = self.windows.get(&window_id).cloned() else {
            return Ok(());
        };
        if !meta.mapped {
            return Ok(());
        }
        let Some(binding) = self.lists.get(&(client_id, list)) else {
            return Ok(());
        };
        if binding.stopped {
            return Ok(());
        }
        let handle = registry.create_object(InterfaceIndex::ExtForeignToplevelHandleV1, 1)?;
        writer
            .ext_foreign_toplevel_list_v1_toplevel(list)
            .toplevel(handle);
        Self::send_properties(writer, handle, &meta);
        self.lists
            .get_mut(&(client_id, list))
            .expect("list exists")
            .handles
            .insert(handle, window_id);
        self.handle_to_window.insert((client_id, handle), window_id);
        self.window_handles
            .entry(window_id)
            .or_default()
            .insert((client_id, handle));
        Ok(())
    }

    fn send_properties(writer: &mut Writer, handle: ObjectId, meta: &WindowMeta) {
        writer
            .ext_foreign_toplevel_handle_v1_identifier(handle)
            .identifier(&meta.identifier);
        if !meta.title.is_empty() {
            writer
                .ext_foreign_toplevel_handle_v1_title(handle)
                .title(&meta.title);
        }
        if !meta.app_id.is_empty() {
            writer
                .ext_foreign_toplevel_handle_v1_app_id(handle)
                .app_id(&meta.app_id);
        }
        writer.ext_foreign_toplevel_handle_v1_done(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    use lumalla_wayland_protocol::buffer::Writer;

    fn client() -> ClientId {
        ClientId::new(NonZeroU32::new(1).unwrap())
    }

    fn oid(id: u32) -> ObjectId {
        ObjectId::new(NonZeroU32::new(id).unwrap())
    }

    #[test]
    fn ensure_window_builds_stable_identifier() {
        let mut mgr = ForeignToplevelListManager::default();
        mgr.ensure_window(7, "Title", "app");
        let meta = mgr.windows.get(&7).unwrap();
        assert!(meta.identifier.starts_with("w7g"));
        assert_eq!(meta.title, "Title");
        assert_eq!(meta.app_id, "app");
    }

    #[test]
    fn unmap_clears_window_and_allows_fresh_identifier() {
        let mut mgr = ForeignToplevelListManager::default();
        let mut clients = ConnectedClients::new();
        mgr.ensure_window(3, "a", "b");
        mgr.notify_mapped(3, "a", "b", &mut clients);
        assert!(mgr.windows.get(&3).unwrap().mapped);
        mgr.notify_unmapped(3, &mut clients);
        assert!(!mgr.windows.contains_key(&3));
        mgr.ensure_window(3, "a", "b");
        let id1_gen = mgr.next_generation;
        assert!(mgr.windows.get(&3).unwrap().identifier.contains(&format!("g{id1_gen}")));
    }
}
