use std::collections::{HashMap, HashSet};

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
        }
    }
}

#[derive(Debug, Default)]
pub struct OutputManager {
    outputs: HashMap<GlobalId, OutputInfo>,
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
    ) -> GlobalId {
        let id = globals.register_version(InterfaceIndex::WlOutput, 4, client_connections);
        self.outputs.insert(id, info);
        id
    }

    #[allow(dead_code)]
    pub fn get(&self, global_id: GlobalId) -> Option<&OutputInfo> {
        self.outputs.get(&global_id)
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
