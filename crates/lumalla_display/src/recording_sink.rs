//! In-process [`RenderSink`] used when no live renderer is installed (unit tests).

use anyhow::Result;
use lumalla_shared::{RenderSink, SurfaceSubmit};

#[derive(Debug, Default)]
pub struct RecordingRenderSink {
    pub submits: Vec<SurfaceSubmit>,
    pub removed_surfaces: Vec<(u32, u32)>,
    pub removed_buffers: Vec<(u32, u32)>,
    pub removed_clients: Vec<u32>,
    pub desktop_scenes: Vec<Vec<(u32, u32, i32, i32)>>,
    pub layer_scenes: Vec<Vec<(String, Vec<(u32, u32, i32, i32, u8)>)>>,
    pub positions: Vec<(u32, u32, i32, i32)>,
    pub cursor_hotspots: Vec<(i32, i32)>,
    pub hide_cursor_count: u32,
    pub clear_cursor_count: u32,
    pub present_requests: u32,
}

impl RecordingRenderSink {
    pub fn take_submits(&mut self) -> Vec<SurfaceSubmit> {
        std::mem::take(&mut self.submits)
    }

    pub fn take_removed_surfaces(&mut self) -> Vec<(u32, u32)> {
        std::mem::take(&mut self.removed_surfaces)
    }
}

impl RenderSink for RecordingRenderSink {
    fn submit_surface(&mut self, submit: SurfaceSubmit) -> Result<()> {
        self.submits.push(submit);
        Ok(())
    }

    fn remove_surface(&mut self, owner_id: u32, surface_id: u32) -> Result<()> {
        self.removed_surfaces.push((owner_id, surface_id));
        Ok(())
    }

    fn remove_buffer(&mut self, owner_id: u32, buffer_id: u32) -> Result<()> {
        self.removed_buffers.push((owner_id, buffer_id));
        Ok(())
    }

    fn remove_client(&mut self, owner_id: u32) -> Result<()> {
        self.removed_clients.push(owner_id);
        Ok(())
    }

    fn set_desktop_scene(&mut self, scene: &[(u32, u32, i32, i32)]) {
        self.desktop_scenes.push(scene.to_vec());
    }

    fn set_output_layer_scenes(&mut self, scenes: &[(String, Vec<(u32, u32, i32, i32, u8)>)]) {
        self.layer_scenes.push(scenes.to_vec());
    }

    fn update_surface_position(
        &mut self,
        owner_id: u32,
        surface_id: u32,
        x: i32,
        y: i32,
    ) -> Result<()> {
        self.positions.push((owner_id, surface_id, x, y));
        Ok(())
    }

    fn update_cursor_hotspot(&mut self, hotspot_x: i32, hotspot_y: i32) -> Result<()> {
        self.cursor_hotspots.push((hotspot_x, hotspot_y));
        Ok(())
    }

    fn hide_cursor(&mut self) -> Result<()> {
        self.hide_cursor_count += 1;
        Ok(())
    }

    fn clear_cursor(&mut self) -> Result<()> {
        self.clear_cursor_count += 1;
        Ok(())
    }

    fn request_present(&mut self) {
        self.present_requests += 1;
    }
}
