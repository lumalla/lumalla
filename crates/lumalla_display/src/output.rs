use std::collections::{HashMap, HashSet};

use anyhow::{Context, bail};
use lumalla_shared::View;
use lumalla_wayland_protocol::{
    ClientConnection, ClientId, ObjectId,
    buffer::Writer,
    protocols::wayland::{
        WL_OUTPUT_MODE_CURRENT, WL_OUTPUT_MODE_PREFERRED, WL_OUTPUT_SUBPIXEL_UNKNOWN,
        WL_OUTPUT_TRANSFORM_NORMAL,
    },
    registry::InterfaceIndex,
};

use crate::{ConnectedClients, GlobalId, Globals};

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
    pub views: Vec<View>,
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
            views: Vec::new(),
        }
    }
}

impl OutputInfo {
    /// Sync advertised `x`/`y` from the first view's source origin.
    pub fn sync_location_from_views(&mut self) {
        let (x, y) = self
            .views
            .first()
            .map(|view| (view.source.0, view.source.1))
            .unwrap_or((0, 0));
        self.x = x;
        self.y = y;
    }
}

impl From<&OutputInfo> for lumalla_shared::Output {
    fn from(info: &OutputInfo) -> Self {
        Self {
            name: info.name.clone(),
            description: info.description.clone(),
            views: info.views.clone(),
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
        let (x, y) = output.location();
        Self {
            name: output.name.clone(),
            description: output.description.clone(),
            x,
            y,
            physical_width_mm: output.physical_width_mm,
            physical_height_mm: output.physical_height_mm,
            width: output.size.0,
            height: output.size.1,
            refresh_mhz: output.refresh_mhz,
            scale: output.scale,
            is_virtual: output.is_virtual,
            views: output.views.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct XdgOutputBinding {
    global_id: GlobalId,
    wl_output: ObjectId,
}

#[derive(Debug, Default)]
pub struct OutputManager {
    outputs: HashMap<GlobalId, OutputInfo>,
    /// Stable output name -> global id
    by_name: HashMap<String, GlobalId>,
    /// Bound wl_output objects: (client, object) -> global id
    bindings: HashMap<(ClientId, ObjectId), GlobalId>,
    client_bindings: HashMap<ClientId, HashSet<ObjectId>>,
    /// Bound zxdg_output_v1 objects: (client, object) -> binding
    xdg_bindings: HashMap<(ClientId, ObjectId), XdgOutputBinding>,
    xdg_client_bindings: HashMap<ClientId, HashSet<ObjectId>>,
}

impl OutputManager {
    pub fn add_output<'connection>(
        &mut self,
        mut info: OutputInfo,
        globals: &mut Globals,
        client_connections: impl Iterator<Item = &'connection mut ClientConnection>,
    ) -> anyhow::Result<GlobalId> {
        if self.by_name.contains_key(&info.name) {
            bail!("Output already exists: {}", info.name);
        }
        info.sync_location_from_views();
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
        let removed_xdg: HashSet<(ClientId, ObjectId)> = self
            .xdg_bindings
            .iter()
            .filter_map(|(&(client_id, object_id), binding)| {
                (binding.global_id == global_id).then_some((client_id, object_id))
            })
            .collect();
        for (client_id, object_id) in removed_xdg {
            self.xdg_bindings.remove(&(client_id, object_id));
            if let Some(set) = self.xdg_client_bindings.get_mut(&client_id) {
                set.remove(&object_id);
                if set.is_empty() {
                    self.xdg_client_bindings.remove(&client_id);
                }
            }
        }
        globals.unregister(global_id, client_connections);
        Ok(())
    }

    pub fn add_view(
        &mut self,
        output_name: &str,
        view: View,
        clients: &mut ConnectedClients,
    ) -> anyhow::Result<bool> {
        let global_id = *self
            .by_name
            .get(output_name)
            .with_context(|| format!("Unknown output: {output_name}"))?;
        let info = self
            .outputs
            .get_mut(&global_id)
            .with_context(|| format!("Unknown output: {output_name}"))?;
        if let Some(existing) = info.views.iter_mut().find(|v| v.name == view.name) {
            if *existing == view {
                return Ok(false);
            }
            *existing = view;
        } else {
            info.views.push(view);
        }
        info.sync_location_from_views();
        let info = info.clone();
        self.update_output(global_id, info, clients);
        Ok(true)
    }

    pub fn remove_view(
        &mut self,
        output_name: &str,
        view_name: &str,
        clients: &mut ConnectedClients,
    ) -> anyhow::Result<()> {
        let global_id = *self
            .by_name
            .get(output_name)
            .with_context(|| format!("Unknown output: {output_name}"))?;
        let info = self
            .outputs
            .get_mut(&global_id)
            .with_context(|| format!("Unknown output: {output_name}"))?;
        let before = info.views.len();
        info.views.retain(|v| v.name != view_name);
        if info.views.len() == before {
            bail!("Unknown view: {output_name}/{view_name}");
        }
        info.sync_location_from_views();
        let info = info.clone();
        self.update_output(global_id, info, clients);
        Ok(())
    }

    pub fn outputs(&self) -> impl Iterator<Item = &OutputInfo> {
        self.outputs.values()
    }

    /// Logical output bounds containing a point, or the first output as fallback.
    pub fn work_area_for_point(&self, x: i32, y: i32) -> Option<(i32, i32, i32, i32)> {
        let contains = |output: &&OutputInfo| {
            x >= output.x
                && y >= output.y
                && x < output.x.saturating_add(output.width)
                && y < output.y.saturating_add(output.height)
        };
        self.outputs()
            .find(contains)
            .or_else(|| self.outputs().next())
            .map(|output| (output.x, output.y, output.width, output.height))
    }

    pub fn logical_size_for_client_output(
        &self,
        client_id: ClientId,
        output_id: Option<ObjectId>,
    ) -> Option<(i32, i32)> {
        let selected = output_id
            .and_then(|id| self.bindings.get(&(client_id, id)))
            .and_then(|global| self.outputs.get(global));
        selected
            .or_else(|| self.outputs().next())
            .map(|output| (output.width, output.height))
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
        Self::send_wl_output_properties(writer, object_id, version, &info, true);
        if version >= 2 {
            writer.wl_output_done(object_id);
        }
        true
    }

    /// Bind a `zxdg_output_v1` for an already-bound `wl_output` and send initial events.
    pub fn bind_xdg_output(
        &mut self,
        client_id: ClientId,
        xdg_output: ObjectId,
        wl_output: ObjectId,
        version: u32,
        writer: &mut Writer,
    ) -> bool {
        let Some(&global_id) = self.bindings.get(&(client_id, wl_output)) else {
            return false;
        };
        let Some(info) = self.outputs.get(&global_id).cloned() else {
            return false;
        };
        self.xdg_bindings.insert(
            (client_id, xdg_output),
            XdgOutputBinding {
                global_id,
                wl_output,
            },
        );
        self.xdg_client_bindings
            .entry(client_id)
            .or_default()
            .insert(xdg_output);
        Self::send_xdg_output_properties(writer, xdg_output, version, &info, true, None);
        if version < 3 {
            writer.zxdg_output_v1_done(xdg_output);
        } else {
            writer.wl_output_done(wl_output);
        }
        true
    }

    pub fn destroy_xdg_output(&mut self, client_id: ClientId, object_id: ObjectId) {
        self.xdg_bindings.remove(&(client_id, object_id));
        if let Some(set) = self.xdg_client_bindings.get_mut(&client_id) {
            set.remove(&object_id);
            if set.is_empty() {
                self.xdg_client_bindings.remove(&client_id);
            }
        }
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

    pub fn get(&self, global_id: GlobalId) -> Option<&OutputInfo> {
        self.outputs.get(&global_id)
    }

    pub fn global_by_name(&self, name: &str) -> Option<GlobalId> {
        self.by_name.get(name).copied()
    }

    pub fn global_for_binding(
        &self,
        client_id: ClientId,
        output: ObjectId,
    ) -> Option<GlobalId> {
        self.bindings.get(&(client_id, output)).copied()
    }

    pub fn binding_for_global(
        &self,
        client_id: ClientId,
        global_id: GlobalId,
    ) -> Option<ObjectId> {
        self.bindings.iter().find_map(|(&(cid, object_id), gid)| {
            (cid == client_id && *gid == global_id).then_some(object_id)
        })
    }

    pub fn update_output(
        &mut self,
        global_id: GlobalId,
        mut info: OutputInfo,
        clients: &mut ConnectedClients,
    ) -> bool {
        let Some(previous) = self.outputs.get(&global_id) else {
            return false;
        };
        let previous_description = previous.description.clone();
        info.sync_location_from_views();
        self.outputs.insert(global_id, info.clone());

        let mut client_ids: HashSet<ClientId> = self
            .bindings
            .iter()
            .filter_map(|(&(client_id, _), bound)| (*bound == global_id).then_some(client_id))
            .collect();
        client_ids.extend(
            self.xdg_bindings
                .iter()
                .filter_map(|(&(client_id, _), binding)| {
                    (binding.global_id == global_id).then_some(client_id)
                }),
        );

        for client_id in client_ids {
            let Some(client) = clients.get_mut(&client_id) else {
                continue;
            };
            let (registry, writer) = client.registry_and_writer_mut();

            let wl_objects: Vec<(ObjectId, u32)> = self
                .bindings
                .iter()
                .filter_map(|(&(cid, object_id), bound)| {
                    (cid == client_id && *bound == global_id).then(|| {
                        let version = registry
                            .object_metadata(object_id)
                            .map(|meta| meta.version)
                            .unwrap_or(1);
                        (object_id, version)
                    })
                })
                .collect();
            let xdg_objects: Vec<(ObjectId, ObjectId, u32)> = self
                .xdg_bindings
                .iter()
                .filter_map(|(&(cid, object_id), binding)| {
                    (cid == client_id && binding.global_id == global_id).then(|| {
                        let version = registry
                            .object_metadata(object_id)
                            .map(|meta| meta.version)
                            .unwrap_or(1);
                        (object_id, binding.wl_output, version)
                    })
                })
                .collect();

            for &(object_id, version) in &wl_objects {
                Self::send_wl_output_properties(writer, object_id, version, &info, false);
            }
            for &(xdg_id, _, version) in &xdg_objects {
                Self::send_xdg_output_properties(
                    writer,
                    xdg_id,
                    version,
                    &info,
                    false,
                    Some(previous_description.as_str()),
                );
            }
            for &(object_id, version) in &wl_objects {
                if version >= 2 {
                    writer.wl_output_done(object_id);
                }
            }
            for &(xdg_id, wl_output, version) in &xdg_objects {
                if version < 3 {
                    writer.zxdg_output_v1_done(xdg_id);
                } else if wl_objects.is_empty() {
                    // No wl_output property batch this round; still finish the xdg v3 batch.
                    writer.wl_output_done(wl_output);
                }
            }
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
        if let Some(ids) = self.xdg_client_bindings.remove(&client_id) {
            for object_id in ids {
                self.xdg_bindings.remove(&(client_id, object_id));
            }
        }
    }

    fn send_wl_output_properties(
        writer: &mut Writer,
        object_id: ObjectId,
        version: u32,
        info: &OutputInfo,
        is_create: bool,
    ) {
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
        // wl_output.name/description are bind-once; never resend after the first done.
        if is_create && version >= 4 {
            writer.wl_output_name(object_id).name(&info.name);
            writer
                .wl_output_description(object_id)
                .description(&info.description);
        }
    }

    fn send_xdg_output_properties(
        writer: &mut Writer,
        xdg_output: ObjectId,
        version: u32,
        info: &OutputInfo,
        is_create: bool,
        previous_description: Option<&str>,
    ) {
        writer
            .zxdg_output_v1_logical_position(xdg_output)
            .x(info.x)
            .y(info.y);
        writer
            .zxdg_output_v1_logical_size(xdg_output)
            .width(info.width)
            .height(info.height);
        if version >= 2 {
            if is_create {
                writer.zxdg_output_v1_name(xdg_output).name(&info.name);
                writer
                    .zxdg_output_v1_description(xdg_output)
                    .description(&info.description);
            } else if version >= 3
                && previous_description.is_some_and(|prev| prev != info.description.as_str())
            {
                writer
                    .zxdg_output_v1_description(xdg_output)
                    .description(&info.description);
            }
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
            views: Vec::new(),
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
            views: Vec::new(),
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
                    views: vec![View {
                        name: "main".to_owned(),
                        source: (10, 20, 1920, 1080),
                        dest: (0, 0, 1920, 1080),
                    }],
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
        assert_eq!(manager.get_by_name("HDMI-A-1").unwrap().x, 10);
        assert_eq!(manager.get_by_name("HDMI-A-1").unwrap().y, 20);
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

    #[test]
    fn add_view_updates_location_from_first_view() {
        let mut globals = Globals::default();
        let mut manager = OutputManager::default();
        let mut clients = ConnectedClients::new();
        manager
            .add_output(
                OutputInfo {
                    name: "VIRTUAL-1".to_owned(),
                    ..OutputInfo::default()
                },
                &mut globals,
                [].into_iter(),
            )
            .unwrap();
        assert_eq!(manager.get_by_name("VIRTUAL-1").unwrap().x, 0);
        manager
            .add_view(
                "VIRTUAL-1",
                View {
                    name: "main".to_owned(),
                    source: (100, 200, 800, 600),
                    dest: (0, 0, 800, 600),
                },
                &mut clients,
            )
            .unwrap();
        let output = manager.get_by_name("VIRTUAL-1").unwrap();
        assert_eq!(output.x, 100);
        assert_eq!(output.y, 200);
        assert_eq!(output.views.len(), 1);
    }

    fn has_event(bytes: &[u8], object: u32, opcode: u16) -> bool {
        bytes.windows(8).any(|w| {
            u32::from_ne_bytes(w[0..4].try_into().unwrap()) == object
                && u16::from_ne_bytes(w[4..6].try_into().unwrap()) == opcode
        })
    }

    #[test]
    fn bind_xdg_output_v3_sends_properties_and_wl_output_done() {
        let mut globals = Globals::default();
        let mut manager = OutputManager::default();
        let global_id = manager
            .add_output(
                OutputInfo {
                    name: "HDMI-A-1".to_owned(),
                    description: "Main".to_owned(),
                    x: 10,
                    y: 20,
                    width: 1920,
                    height: 1080,
                    ..OutputInfo::default()
                },
                &mut globals,
                [].into_iter(),
            )
            .unwrap();

        let (mut receiver, sender) = UnixStream::pair().unwrap();
        let mut writer = Writer::new(sender.as_raw_fd());
        let client_id = ClientId::new(NonZeroU32::new(1).unwrap());
        let wl_id = object_id(10);
        let xdg_id = object_id(11);
        assert!(manager.bind_output(client_id, global_id, wl_id, 4, &mut writer));
        // Drain bind_output events so the next assertions only see xdg-output traffic.
        writer.flush().unwrap();
        drop(writer);
        drop(sender);
        let mut discard = Vec::new();
        receiver.read_to_end(&mut discard).unwrap();

        let (mut receiver, sender) = UnixStream::pair().unwrap();
        let mut writer = Writer::new(sender.as_raw_fd());
        assert!(manager.bind_xdg_output(client_id, xdg_id, wl_id, 3, &mut writer));
        writer.flush().unwrap();
        drop(writer);
        drop(sender);

        let mut bytes = Vec::new();
        receiver.read_to_end(&mut bytes).unwrap();
        // logical_position 0, logical_size 1, name 3, description 4, then wl_output.done (opcode 2)
        assert!(has_event(&bytes, 11, 0));
        assert!(has_event(&bytes, 11, 1));
        assert!(has_event(&bytes, 11, 3));
        assert!(has_event(&bytes, 11, 4));
        assert!(!has_event(&bytes, 11, 2)); // no deprecated zxdg_output.done
        assert!(has_event(&bytes, 10, 2)); // wl_output.done
    }

    #[test]
    fn bind_xdg_output_v2_sends_zxdg_done() {
        let mut globals = Globals::default();
        let mut manager = OutputManager::default();
        let global_id = manager
            .add_output(
                OutputInfo {
                    name: "HDMI-A-1".to_owned(),
                    ..OutputInfo::default()
                },
                &mut globals,
                [].into_iter(),
            )
            .unwrap();

        let (mut receiver, sender) = UnixStream::pair().unwrap();
        let mut writer = Writer::new(sender.as_raw_fd());
        let client_id = ClientId::new(NonZeroU32::new(1).unwrap());
        let wl_id = object_id(10);
        let xdg_id = object_id(11);
        assert!(manager.bind_output(client_id, global_id, wl_id, 4, &mut writer));
        assert!(manager.bind_xdg_output(client_id, xdg_id, wl_id, 2, &mut writer));
        writer.flush().unwrap();
        drop(writer);
        drop(sender);

        let mut bytes = Vec::new();
        receiver.read_to_end(&mut bytes).unwrap();
        assert!(has_event(&bytes, 11, 2)); // zxdg_output.done
    }

    #[test]
    fn bind_xdg_output_rejects_unbound_wl_output() {
        let mut manager = OutputManager::default();
        let (receiver, sender) = UnixStream::pair().unwrap();
        let mut writer = Writer::new(sender.as_raw_fd());
        let client_id = ClientId::new(NonZeroU32::new(1).unwrap());
        assert!(!manager.bind_xdg_output(client_id, object_id(11), object_id(10), 3, &mut writer));
        drop(writer);
        drop(sender);
        drop(receiver);
    }

    #[test]
    fn destroy_xdg_output_clears_binding() {
        let mut globals = Globals::default();
        let mut manager = OutputManager::default();
        let global_id = manager
            .add_output(
                OutputInfo {
                    name: "HDMI-A-1".to_owned(),
                    ..OutputInfo::default()
                },
                &mut globals,
                [].into_iter(),
            )
            .unwrap();

        let (_receiver, sender) = UnixStream::pair().unwrap();
        let mut writer = Writer::new(sender.as_raw_fd());
        let client_id = ClientId::new(NonZeroU32::new(1).unwrap());
        let wl_id = object_id(10);
        let xdg_id = object_id(11);
        assert!(manager.bind_output(client_id, global_id, wl_id, 4, &mut writer));
        assert!(manager.bind_xdg_output(client_id, xdg_id, wl_id, 3, &mut writer));
        assert!(manager.xdg_bindings.contains_key(&(client_id, xdg_id)));
        manager.destroy_xdg_output(client_id, xdg_id);
        assert!(!manager.xdg_bindings.contains_key(&(client_id, xdg_id)));
        assert!(!manager
            .xdg_client_bindings
            .get(&client_id)
            .is_some_and(|set| set.contains(&xdg_id)));
    }

    #[test]
    fn remove_output_clears_xdg_bindings() {
        let mut globals = Globals::default();
        let mut manager = OutputManager::default();
        let global_id = manager
            .add_output(
                OutputInfo {
                    name: "HDMI-A-1".to_owned(),
                    ..OutputInfo::default()
                },
                &mut globals,
                [].into_iter(),
            )
            .unwrap();

        let (_receiver, sender) = UnixStream::pair().unwrap();
        let mut writer = Writer::new(sender.as_raw_fd());
        let client_id = ClientId::new(NonZeroU32::new(1).unwrap());
        let wl_id = object_id(10);
        let xdg_id = object_id(11);
        assert!(manager.bind_output(client_id, global_id, wl_id, 4, &mut writer));
        assert!(manager.bind_xdg_output(client_id, xdg_id, wl_id, 3, &mut writer));
        manager
            .remove_output("HDMI-A-1", &mut globals, [].into_iter())
            .unwrap();
        assert!(!manager.xdg_bindings.contains_key(&(client_id, xdg_id)));
    }

    #[test]
    fn update_output_reemits_xdg_logical_geometry_and_wl_done() {
        use std::os::fd::IntoRawFd;

        use lumalla_wayland_protocol::{NewObjectId, Wayland, registry::InterfaceIndex};

        let socket_path = std::env::temp_dir().join(format!(
            "lumalla-xdg-output-update-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket_path);
        let mut wayland = Wayland::new(socket_path.clone()).unwrap();
        let (mut receiver, server) = UnixStream::pair().unwrap();
        receiver.set_nonblocking(true).unwrap();
        let client = wayland
            .client_from_accepted_fd(server.into_raw_fd())
            .expect("client connection");
        let client_id = client.client_id();

        let mut globals = Globals::default();
        let mut manager = OutputManager::default();
        let global_id = manager
            .add_output(
                OutputInfo {
                    name: "HDMI-A-1".to_owned(),
                    description: "Main".to_owned(),
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    ..OutputInfo::default()
                },
                &mut globals,
                [].into_iter(),
            )
            .unwrap();

        let mut clients = ConnectedClients::new();
        clients.insert(client);
        {
            let client = clients.get_mut(&client_id).unwrap();
            let (registry, writer) = client.registry_and_writer_mut();
            registry
                .register_client_object_with_version(
                    NewObjectId::new(object_id(10)),
                    InterfaceIndex::WlOutput,
                    4,
                )
                .unwrap();
            registry
                .register_client_object_with_version(
                    NewObjectId::new(object_id(11)),
                    InterfaceIndex::ZxdgOutputV1,
                    3,
                )
                .unwrap();
            let wl_id = object_id(10);
            let xdg_id = object_id(11);
            assert!(manager.bind_output(client_id, global_id, wl_id, 4, writer));
            assert!(manager.bind_xdg_output(client_id, xdg_id, wl_id, 3, writer));
            writer.flush().unwrap();
        }
        // Discard initial bind traffic.
        let mut discard = [0u8; 4096];
        while receiver.read(&mut discard).is_ok() {}

        let updated = OutputInfo {
            name: "HDMI-A-1".to_owned(),
            description: "Main".to_owned(),
            x: 100,
            y: 200,
            width: 1920,
            height: 1080,
            views: vec![View {
                name: "main".to_owned(),
                source: (100, 200, 1920, 1080),
                dest: (0, 0, 1920, 1080),
            }],
            ..OutputInfo::default()
        };
        assert!(manager.update_output(global_id, updated, &mut clients));
        clients
            .get_mut(&client_id)
            .unwrap()
            .writer_mut()
            .flush()
            .unwrap();

        let mut bytes = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match receiver.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => bytes.extend_from_slice(&buf[..n]),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("read failed: {err}"),
            }
        }
        drop(clients);
        drop(wayland);
        let _ = std::fs::remove_file(&socket_path);

        assert!(has_event(&bytes, 11, 0)); // logical_position
        assert!(has_event(&bytes, 11, 1)); // logical_size
        assert!(!has_event(&bytes, 11, 3)); // xdg name only on create
        assert!(!has_event(&bytes, 10, 4)); // wl_output.name only on create
        assert!(!has_event(&bytes, 10, 5)); // wl_output.description only on create
        assert!(has_event(&bytes, 10, 2)); // wl_output.done
    }
}
