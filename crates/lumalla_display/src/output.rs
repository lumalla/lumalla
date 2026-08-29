use std::collections::{HashMap, HashSet};

use anyhow::{Context, bail};
use lumalla_wayland_protocol::{
    ClientConnection, ClientId, ObjectId,
    buffer::Writer,
    protocols::wayland::{
        WL_OUTPUT_MODE_CURRENT, WL_OUTPUT_MODE_PREFERRED, WL_OUTPUT_SUBPIXEL_UNKNOWN,
        WL_OUTPUT_TRANSFORM_NORMAL,
    },
    registry::InterfaceIndex,
};

use crate::{GlobalId, Globals};

#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub name: String,
    pub description: String,
    pub x: i32,
    pub y: i32,
    pub physical_width_mm: i32,
    pub physical_height_mm: i32,
    pub width: i32,
    pub height: i32,
    pub refresh_mhz: i32,
    pub scale: i32,
    pub is_virtual: bool,
}

impl Default for OutputInfo {
    fn default() -> Self {
        Self {
            name: "WL-1".to_owned(),
            description: "Lumalla virtual output".to_owned(),
            x: 0,
            y: 0,
            physical_width_mm: 300,
            physical_height_mm: 200,
            width: 800,
            height: 600,
            refresh_mhz: 60_000,
            scale: 1,
            is_virtual: true,
        }
    }
}

impl From<&OutputInfo> for lumalla_shared::Output {
    fn from(info: &OutputInfo) -> Self {
        Self {
            name: info.name.clone(),
            description: info.description.clone(),
            location: (info.x, info.y),
            size: (info.width, info.height),
            scale: info.scale,
            refresh_mhz: info.refresh_mhz,
            physical_width_mm: info.physical_width_mm,
            physical_height_mm: info.physical_height_mm,
            is_virtual: info.is_virtual,
        }
    }
}

impl From<&lumalla_shared::Output> for OutputInfo {
    fn from(output: &lumalla_shared::Output) -> Self {
        Self {
            name: output.name.clone(),
            description: output.description.clone(),
            x: output.location.0,
            y: output.location.1,
            physical_width_mm: output.physical_width_mm,
            physical_height_mm: output.physical_height_mm,
            width: output.size.0,
            height: output.size.1,
            refresh_mhz: output.refresh_mhz,
            scale: output.scale,
            is_virtual: output.is_virtual,
        }
    }
}

#[derive(Debug, Default)]
pub struct OutputManager {
    outputs: HashMap<GlobalId, OutputInfo>,
    /// Stable output name -> global id
    by_name: HashMap<String, GlobalId>,
    /// Bound wl_output objects: (client, object) -> global id
    bindings: HashMap<(ClientId, ObjectId), GlobalId>,
    client_bindings: HashMap<ClientId, HashSet<ObjectId>>,
}

impl OutputManager {
    pub fn add_output<'connection>(
        &mut self,
        info: OutputInfo,
        globals: &mut Globals,
        client_connections: impl Iterator<Item = &'connection mut ClientConnection>,
    ) -> anyhow::Result<GlobalId> {
        if self.by_name.contains_key(&info.name) {
            bail!("Output already exists: {}", info.name);
        }
        let name = info.name.clone();
        let id = globals.register_version(InterfaceIndex::WlOutput, 4, client_connections);
        self.outputs.insert(id, info);
        self.by_name.insert(name, id);
        Ok(id)
    }

    pub fn remove_output<'connection>(
        &mut self,
        name: &str,
        globals: &mut Globals,
        client_connections: impl Iterator<Item = &'connection mut ClientConnection>,
    ) -> anyhow::Result<()> {
        let global_id = self
            .by_name
            .remove(name)
            .with_context(|| format!("Unknown output: {name}"))?;
        self.outputs.remove(&global_id);
        let removed_objects: HashSet<(ClientId, ObjectId)> = self
            .bindings
            .iter()
            .filter_map(|(&(client_id, object_id), bound)| {
                (*bound == global_id).then_some((client_id, object_id))
            })
            .collect();
        for (client_id, object_id) in removed_objects {
            self.bindings.remove(&(client_id, object_id));
            if let Some(set) = self.client_bindings.get_mut(&client_id) {
                set.remove(&object_id);
                if set.is_empty() {
                    self.client_bindings.remove(&client_id);
                }
            }
        }
        globals.unregister(global_id, client_connections);
        Ok(())
    }

    pub fn outputs(&self) -> impl Iterator<Item = &OutputInfo> {
        self.outputs.values()
    }

    #[cfg(test)]
    fn get_by_name(&self, name: &str) -> Option<&OutputInfo> {
        self.by_name.get(name).and_then(|id| self.outputs.get(id))
    }

    pub fn bind_output(
        &mut self,
        client_id: ClientId,
        global_id: GlobalId,
        object_id: ObjectId,
        version: u32,
        writer: &mut Writer,
    ) -> bool {
        let Some(info) = self.outputs.get(&global_id).cloned() else {
            return false;
        };
        self.bindings.insert((client_id, object_id), global_id);
        self.client_bindings
            .entry(client_id)
            .or_default()
            .insert(object_id);
        Self::send_events(writer, object_id, version, &info);
        true
    }

    pub fn release(&mut self, client_id: ClientId, object_id: ObjectId) {
        self.bindings.remove(&(client_id, object_id));
        if let Some(set) = self.client_bindings.get_mut(&client_id) {
            set.remove(&object_id);
            if set.is_empty() {
                self.client_bindings.remove(&client_id);
            }
        }
    }

    pub fn primary_global_id(&self) -> Option<GlobalId> {
        self.outputs.keys().copied().min()
    }

    pub fn update_output(
        &mut self,
        global_id: GlobalId,
        info: OutputInfo,
        clients: &mut HashMap<ClientId, ClientConnection>,
    ) -> bool {
        if !self.outputs.contains_key(&global_id) {
            return false;
        }
        self.outputs.insert(global_id, info.clone());
        for ((client_id, object_id), bound_global) in &self.bindings {
            if *bound_global != global_id {
                continue;
            }
            let Some(client) = clients.get_mut(client_id) else {
                continue;
            };
            let (registry, writer) = client.registry_and_writer_mut();
            let version = registry
                .object_metadata(*object_id)
                .map(|meta| meta.version)
                .unwrap_or(1);
            Self::send_events(writer, *object_id, version, &info);
        }
        true
    }

    /// Bound wl_output object ids for a client, sorted.
    pub fn bound_outputs_for_client(&self, client_id: ClientId) -> Vec<ObjectId> {
        let mut ids: Vec<ObjectId> = self
            .client_bindings
            .get(&client_id)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        ids.sort_by_key(|id| id.get());
        ids
    }

    pub fn remove_client(&mut self, client_id: ClientId) {
        if let Some(ids) = self.client_bindings.remove(&client_id) {
            for object_id in ids {
                self.bindings.remove(&(client_id, object_id));
            }
        }
    }

    fn send_events(writer: &mut Writer, object_id: ObjectId, version: u32, info: &OutputInfo) {
        writer
            .wl_output_geometry(object_id)
            .x(info.x)
            .y(info.y)
            .physical_width(info.physical_width_mm)
            .physical_height(info.physical_height_mm)
            .subpixel(WL_OUTPUT_SUBPIXEL_UNKNOWN as i32)
            .make("Lumalla")
            .model(&info.name)
            .transform(WL_OUTPUT_TRANSFORM_NORMAL as i32);
        writer
            .wl_output_mode(object_id)
            .flags(WL_OUTPUT_MODE_CURRENT | WL_OUTPUT_MODE_PREFERRED)
            .width(info.width)
            .height(info.height)
            .refresh(info.refresh_mhz);
        if version >= 2 {
            writer.wl_output_scale(object_id).factor(info.scale);
        }
        if version >= 4 {
            writer.wl_output_name(object_id).name(&info.name);
            writer
                .wl_output_description(object_id)
                .description(&info.description);
        }
        if version >= 2 {
            writer.wl_output_done(object_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Read,
        num::NonZeroU32,
        os::{fd::AsRawFd, unix::net::UnixStream},
    };

    use lumalla_wayland_protocol::{ClientId, ObjectId, buffer::Writer};

    use super::*;
    use crate::Globals;

    fn object_id(id: u32) -> ObjectId {
        ObjectId::new(NonZeroU32::new(id).unwrap())
    }

    #[test]
    fn add_physical_and_virtual_outputs_rejects_duplicates() {
        let mut globals = Globals::default();
        let mut manager = OutputManager::default();
        let physical = OutputInfo {
            name: "HDMI-A-1".to_owned(),
            description: "Main".to_owned(),
            x: 0,
            y: 0,
            physical_width_mm: 500,
            physical_height_mm: 300,
            width: 1920,
            height: 1080,
            refresh_mhz: 60_000,
            scale: 1,
            is_virtual: false,
        };
        let virtual_output = OutputInfo {
            name: "VIRTUAL-1".to_owned(),
            description: "Headless".to_owned(),
            x: 1920,
            y: 0,
            physical_width_mm: 0,
            physical_height_mm: 0,
            width: 800,
            height: 600,
            refresh_mhz: 60_000,
            scale: 1,
            is_virtual: true,
        };
        let physical_id = manager
            .add_output(physical, &mut globals, [].into_iter())
            .unwrap();
        let virtual_id = manager
            .add_output(virtual_output, &mut globals, [].into_iter())
            .unwrap();
        assert_ne!(physical_id, virtual_id);
        assert_eq!(manager.outputs().count(), 2);
        assert!(!manager.get_by_name("HDMI-A-1").unwrap().is_virtual);
        assert!(manager.get_by_name("VIRTUAL-1").unwrap().is_virtual);
        assert!(
            manager
                .add_output(
                    OutputInfo {
                        name: "HDMI-A-1".to_owned(),
                        ..OutputInfo::default()
                    },
                    &mut globals,
                    [].into_iter()
                )
                .is_err()
        );
    }

    #[test]
    fn bind_output_sends_geometry_mode_and_done() {
        let mut globals = Globals::default();
        let mut manager = OutputManager::default();
        let global_id = manager
            .add_output(
                OutputInfo {
                    name: "HDMI-A-1".to_owned(),
                    description: "Main".to_owned(),
                    x: 10,
                    y: 20,
                    physical_width_mm: 500,
                    physical_height_mm: 300,
                    width: 1920,
                    height: 1080,
                    refresh_mhz: 60_000,
                    scale: 2,
                    is_virtual: false,
                },
                &mut globals,
                [].into_iter(),
            )
            .unwrap();

        let (mut receiver, sender) = UnixStream::pair().unwrap();
        let mut writer = Writer::new(sender.as_raw_fd());
        let client_id = ClientId::new(NonZeroU32::new(1).unwrap());
        let bound_id = object_id(10);
        assert!(manager.bind_output(client_id, global_id, bound_id, 4, &mut writer));
        writer.flush().unwrap();
        drop(writer);
        drop(sender);

        let mut bytes = Vec::new();
        receiver.read_to_end(&mut bytes).unwrap();
        assert!(bytes.len() >= 8);
        // geometry opcode 0, mode opcode 1, scale opcode 3, name 4, description 5, done 2
        assert!(bytes.windows(8).any(|w| {
            u32::from_ne_bytes(w[0..4].try_into().unwrap()) == 10
                && u16::from_ne_bytes(w[4..6].try_into().unwrap()) == 0
        }));
        assert!(bytes.windows(8).any(|w| {
            u32::from_ne_bytes(w[0..4].try_into().unwrap()) == 10
                && u16::from_ne_bytes(w[4..6].try_into().unwrap()) == 1
        }));
        assert!(bytes.windows(8).any(|w| {
            u32::from_ne_bytes(w[0..4].try_into().unwrap()) == 10
                && u16::from_ne_bytes(w[4..6].try_into().unwrap()) == 2
        }));
    }

    #[test]
    fn remove_output_clears_name_and_rejects_unknown() {
        let mut globals = Globals::default();
        let mut manager = OutputManager::default();
        let global_id = manager
            .add_output(
                OutputInfo {
                    name: "VIRTUAL-1".to_owned(),
                    ..OutputInfo::default()
                },
                &mut globals,
                [].into_iter(),
            )
            .unwrap();
        assert!(globals.get(global_id).is_some());
        manager
            .remove_output("VIRTUAL-1", &mut globals, [].into_iter())
            .unwrap();
        assert!(manager.get_by_name("VIRTUAL-1").is_none());
        assert!(globals.get(global_id).is_none());
        assert!(
            manager
                .remove_output("VIRTUAL-1", &mut globals, [].into_iter())
                .is_err()
        );
    }
}
