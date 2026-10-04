use std::collections::{HashMap, VecDeque};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc;

use anyhow::Context;
use lumalla_shared::{
    DisplayHost, PresentationNotify, PrimaryModeApply, PrimaryOutputMode, RenderSink,
    SurfaceDmabuf, SurfaceSubmit, SurfaceSubmitRole, View, WindowGeometryUpdate, WindowRule,
    WindowState, map_dest_to_source, map_source_to_dest,
};
use lumalla_wayland_protocol::buffer::MessageHeader;
use lumalla_wayland_protocol::protocols::presentation_time::{
    WP_PRESENTATION_FEEDBACK_KIND_HW_CLOCK, WP_PRESENTATION_FEEDBACK_KIND_HW_COMPLETION,
    WP_PRESENTATION_FEEDBACK_KIND_VSYNC,
};
use lumalla_wayland_protocol::registry::InterfaceIndex;
use lumalla_wayland_protocol::{
    Ctx, ObjectId, buffer::Writer, registry::ObjectMetadata, registry::Registry,
    registry::RequestHandler,
};
use stumpalo::Arena;

use crate::{
    data_device::DataDeviceManager,
    dmabuf::DmabufManager,
    layer_shell::LayerShellManager,
    output::OutputManager,
    pointer_constraints::PointerConstraintsManager,
    relative_pointer::RelativePointerManager,
    seat::SeatManager,
    shm::ShmManager,
    surface::SurfaceManager,
    window_manager::WindowManager,
    xdg::{ActivationConfigure, XdgManager},
};

mod clients;
mod data_device;
mod dmabuf;
mod layer_shell;
mod output;
mod pointer_constraints;
mod protocols;
mod recording_sink;
mod relative_pointer;
mod seat;
mod seat_input;
mod shm;
mod surface;
mod window_manager;
mod xdg;

pub use clients::ConnectedClients;
pub use dmabuf::ExportedDmabuf;
pub use lumalla_shared::PresentationFlipInfo;
pub use lumalla_wayland_protocol::{ClientConnection, ClientId, Wayland, buffer::ReadResult};
pub use output::OutputInfo;
pub use recording_sink::RecordingRenderSink;
pub use seat::{ActiveCursor, KeyboardModifiers, PointerCursor};
pub use seat_input::SeatInputHandler;
pub use surface::{Rectangle, SceneSurface};
pub use window_manager::{WindowError, WindowGeometryChange};

/// Short-lived protocol handler that gives display a [`RenderSink`] for the dispatch phase.
///
/// Protocol impls stay on [`DisplayState`]; this wrapper installs `render` for the duration of
/// each request so commit/layout paths can call the sink in-place.
pub struct DisplayHandler<'a> {
    pub state: &'a mut DisplayState,
    pub render: &'a mut dyn RenderSink,
}

impl RequestHandler for DisplayHandler<'_> {
    fn handle_request(
        &mut self,
        object: ObjectMetadata,
        ctx: &mut Ctx,
        header: &MessageHeader,
        data: &[u8],
        fds: &mut VecDeque<OwnedFd>,
    ) -> anyhow::Result<()> {
        let DisplayHandler { state, render } = self;
        // SAFETY: `render` is exclusively borrowed for this call; the pointer is cleared before
        // returning and never escapes DisplayState.
        unsafe {
            state.enter_render(&mut **render);
        }
        let result = state.handle_request(object, ctx, header, data, fds);
        state.exit_render();
        result
    }
}

/// Adapter that completes Wayland frame/presentation objects after renderer present/flip.
pub struct DisplayPresentationNotify<'a> {
    pub state: &'a mut DisplayState,
    pub clients: &'a mut ConnectedClients,
}

impl PresentationNotify for DisplayPresentationNotify<'_> {
    fn presentation_completed(&mut self, info: PresentationFlipInfo) {
        self.state
            .complete_presentation_feedbacks(self.clients, info);
    }

    fn frames_completed(&mut self, time_msec: u32) {
        self.state
            .complete_frame_callbacks(self.clients, time_msec);
    }

    fn pending_presentation_feedback(&self) -> bool {
        self.state.pending_presentation_feedback_count() > 0
    }

    fn pending_frame_callbacks(&self) -> bool {
        self.state.pending_frame_callback_count() > 0
    }
}

/// Adapter for renderer → display config updates (e.g. linux-dmabuf formats).
pub struct DisplayConfigHost<'a> {
    pub state: &'a mut DisplayState,
    pub clients: &'a mut ConnectedClients,
}

impl DisplayHost for DisplayConfigHost<'_> {
    fn set_dmabuf_formats(
        &mut self,
        formats: Vec<(u32, u64)>,
        device_path: Option<&std::path::Path>,
    ) {
        self.state
            .set_dmabuf_formats(formats, device_path, self.clients);
    }

    fn apply_primary_output_mode(&mut self, mode: PrimaryOutputMode) -> PrimaryModeApply {
        let width_u = mode.width.max(1) as u32;
        let height_u = mode.height.max(1) as u32;
        self.state.set_output_geometry(width_u, height_u);
        let (px, py) = self.state.pointer_position();
        let pointer_x = px.round() as i32;
        let pointer_y = py.round() as i32;

        // Only rewrite the primary Wayland output when it already exists and is physical
        // (DRM hotplug sync). Virtual outputs are owned entirely by config `add_output`.
        let existing = self.state.outputs().find(|o| o.name == mode.name);
        let is_virtual_primary = existing.map(|o| o.is_virtual).unwrap_or(true);
        if is_virtual_primary {
            return PrimaryModeApply {
                pointer_x,
                pointer_y,
                updated_views: None,
            };
        }

        let old_size = existing
            .map(|o| (o.width, o.height))
            .unwrap_or((mode.width, mode.height));
        let mut views = existing.map(|o| o.views.clone()).unwrap_or_default();
        for view in &mut views {
            view.resize_for_output_mode(old_size, (mode.width, mode.height));
        }
        let (x, y) = views
            .first()
            .map(|view| (view.source.0, view.source.1))
            .unwrap_or((0, 0));
        let info = OutputInfo {
            name: mode.name.clone(),
            description: format!("Lumalla output {}", mode.name),
            x,
            y,
            physical_width_mm: 300,
            physical_height_mm: 200,
            width: mode.width,
            height: mode.height,
            refresh_mhz: mode.refresh_mhz,
            scale: 1,
            is_virtual: false,
            views: views.clone(),
        };
        self.state.update_primary_output(info, self.clients);
        PrimaryModeApply {
            pointer_x,
            pointer_y,
            updated_views: Some((mode.name, views)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingPresentationFeedback {
    client_id: ClientId,
    surface_id: ObjectId,
    feedback_id: ObjectId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PendingFrameCallback {
    pub(crate) client_id: ClientId,
    pub(crate) surface_id: ObjectId,
    pub(crate) callback_id: ObjectId,
}

pub struct DisplayMessage;

/// Built during a surface commit before submitting to [`RenderSink`].
#[derive(Debug)]
pub(crate) struct CommittedFrame {
    pub client_id: ClientId,
    pub surface_id: lumalla_wayland_protocol::ObjectId,
    pub buffer_id: lumalla_wayland_protocol::ObjectId,
    pub pixels: Rc<Vec<u8>>,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub format: u32,
    pub buffer_scale: i32,
    pub buffer_transform: u32,
    pub offset_x: i32,
    pub offset_y: i32,
    pub x: i32,
    pub y: i32,
    pub surface_width: i32,
    pub surface_height: i32,
    pub viewport_src: Option<(f32, f32, f32, f32)>,
    pub dmabuf: Option<ExportedDmabuf>,
    pub damage: Option<Rectangle>,
    pub buffer_damage: Option<Rectangle>,
    pub full_surface: bool,
}

pub struct DisplayState {
    globals: Globals,
    surface_manager: SurfaceManager,
    shm_manager: ShmManager,
    dmabuf_manager: DmabufManager,
    seat_manager: SeatManager,
    pointer_constraints_manager: PointerConstraintsManager,
    relative_pointer_manager: RelativePointerManager,
    output_manager: OutputManager,
    data_device_manager: DataDeviceManager,
    xdg_manager: XdgManager,
    layer_shell_manager: LayerShellManager,
    window_manager: WindowManager,
    pending_geometry_changes: Vec<WindowGeometryChange>,
    pub(crate) pending_frame_callbacks: VecDeque<PendingFrameCallback>,
    pending_presentation_feedbacks: VecDeque<PendingPresentationFeedback>,
    /// Activation configures for clients other than the one currently writing.
    pending_activation_configures: Vec<ActivationConfigure>,
    /// Installed only while a [`DisplayHandler`] is dispatching a request.
    ///
    /// Stored as `'static` via transmute for the duration of `enter_render`…`exit_render` only.
    active_render: Option<NonNull<dyn RenderSink + 'static>>,
    /// Used when no live renderer is installed (unit tests / early init).
    fallback_sink: RecordingRenderSink,
    /// True when a commit/unmap/buffer change was pushed to the render sink this cycle.
    render_content_changed: bool,
}

impl Default for DisplayState {
    fn default() -> Self {
        Self {
            globals: Globals::default(),
            surface_manager: SurfaceManager::default(),
            shm_manager: ShmManager::default(),
            dmabuf_manager: DmabufManager::default(),
            seat_manager: SeatManager::default(),
            pointer_constraints_manager: PointerConstraintsManager::default(),
            relative_pointer_manager: RelativePointerManager::default(),
            output_manager: OutputManager::default(),
            data_device_manager: DataDeviceManager::default(),
            xdg_manager: XdgManager::default(),
            layer_shell_manager: LayerShellManager::default(),
            window_manager: WindowManager::default(),
            pending_geometry_changes: Vec::new(),
            pending_frame_callbacks: VecDeque::new(),
            pending_presentation_feedbacks: VecDeque::new(),
            pending_activation_configures: Vec::new(),
            active_render: None,
            fallback_sink: RecordingRenderSink::default(),
            render_content_changed: false,
        }
    }
}

impl DisplayState {
    /// # Safety
    /// `render` must remain exclusively borrowed and valid until [`Self::exit_render`].
    pub(crate) unsafe fn enter_render(&mut self, render: &mut dyn RenderSink) {
        let ptr: *mut dyn RenderSink = render;
        // SAFETY: pointer is only used until `exit_render`, while `render` is borrowed.
        let ptr: *mut (dyn RenderSink + 'static) = unsafe { std::mem::transmute(ptr) };
        self.active_render = Some(unsafe { NonNull::new_unchecked(ptr) });
    }

    pub(crate) fn exit_render(&mut self) {
        self.active_render = None;
    }

    /// Active [`RenderSink`]: live renderer when installed, else the recording fallback.
    pub(crate) fn render_mut(&mut self) -> &mut dyn RenderSink {
        if let Some(mut ptr) = self.active_render {
            // SAFETY: pointer is valid between enter_render/exit_render.
            return unsafe { ptr.as_mut() as &mut dyn RenderSink };
        }
        &mut self.fallback_sink
    }

    /// Run `f` with a render sink installed (for layout/config paths outside protocol dispatch).
    pub fn with_render<R: RenderSink, T>(
        &mut self,
        render: &mut R,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        // SAFETY: `render` is borrowed for the duration of `f`.
        unsafe {
            self.enter_render(render);
        }
        let result = f(self);
        self.exit_render();
        result
    }

    /// Test/helper: drain submits recorded on the fallback sink.
    pub fn take_recorded_submits(&mut self) -> Vec<SurfaceSubmit> {
        self.fallback_sink.take_submits()
    }

    /// Test/helper: drain surface removals recorded on the fallback sink.
    pub fn take_recorded_removals(&mut self) -> Vec<(u32, u32)> {
        self.fallback_sink.take_removed_surfaces()
    }

    /// Whether protocol handling pushed content to the render sink since the last clear.
    pub fn take_render_content_changed(&mut self) -> bool {
        std::mem::take(&mut self.render_content_changed)
    }

    pub(crate) fn submit_committed_frame(&mut self, frame: CommittedFrame, is_cursor: bool) {
        let hotspot = if is_cursor {
            self.active_cursor()
                .filter(|cursor| {
                    cursor.client_id == frame.client_id && cursor.surface_id == frame.surface_id
                })
                .map(|cursor| (cursor.hotspot_x, cursor.hotspot_y))
                .unwrap_or((0, 0))
        } else {
            (0, 0)
        };
        let buffer_id = frame.buffer_id.get();
        let submit = SurfaceSubmit {
            owner_id: frame.client_id.get(),
            surface_id: frame.surface_id.get(),
            buffer_id,
            pixels: frame.pixels,
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            format: frame.format,
            x: frame.x,
            y: frame.y,
            buffer_scale: frame.buffer_scale,
            buffer_transform: frame.buffer_transform,
            surface_width: frame.surface_width,
            surface_height: frame.surface_height,
            viewport_src: frame.viewport_src,
            dmabuf: frame.dmabuf.map(|exported| SurfaceDmabuf {
                buffer_id,
                fd: exported.fd,
                drm_fourcc: exported.drm_fourcc,
                offset: exported.offset,
                modifier: exported.modifier,
            }),
            damage: frame.damage.map(|rect| lumalla_shared::DamageRect {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
            }),
            buffer_damage: frame.buffer_damage.map(|rect| lumalla_shared::DamageRect {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
            }),
            full_surface: frame.full_surface,
            role: if is_cursor {
                SurfaceSubmitRole::Cursor {
                    hotspot_x: hotspot.0,
                    hotspot_y: hotspot.1,
                }
            } else {
                SurfaceSubmitRole::Content
            },
        };
        if let Err(err) = self.render_mut().submit_surface(submit) {
            log::error!("Unable to submit surface to renderer: {err:#}");
        }
        self.render_mut().request_present();
        self.render_content_changed = true;
    }

    pub(crate) fn emit_surface_unmapped(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
    ) {
        if let Err(err) = self
            .render_mut()
            .remove_surface(client_id.get(), surface_id.get())
        {
            log::error!("Unable to remove surface from renderer: {err:#}");
        }
        self.render_mut().request_present();
        self.render_content_changed = true;
    }

    pub(crate) fn emit_buffer_destroyed(&mut self, client_id: ClientId, buffer_id: ObjectId) {
        if let Err(err) = self
            .render_mut()
            .remove_buffer(client_id.get(), buffer_id.get())
        {
            log::error!("Unable to drop destroyed buffer in renderer: {err:#}");
        }
    }

    /// Push pointer cursor policy (hotspot / hide / default) into the active render sink.
    pub fn sync_pointer_cursor_to_render(&mut self) {
        match self.pointer_cursor() {
            PointerCursor::Surface(active) => {
                if let Err(err) = self
                    .render_mut()
                    .update_cursor_hotspot(active.hotspot_x, active.hotspot_y)
                {
                    log::error!("Unable to update cursor hotspot: {err:#}");
                }
            }
            PointerCursor::Hidden => {
                if let Err(err) = self.render_mut().hide_cursor() {
                    log::error!("Unable to hide pointer cursor: {err:#}");
                }
            }
            PointerCursor::Default => {
                if let Err(err) = self.render_mut().clear_cursor() {
                    log::error!("Unable to restore default cursor: {err:#}");
                }
            }
        }
    }

    /// Push the authoritative desktop + layer scenes into the active render sink.
    pub fn push_renderer_scenes(&mut self) {
        let layer_scenes = self.collect_output_layer_scenes();
        self.render_mut().set_output_layer_scenes(&layer_scenes);

        let surfaces = self.scene_surfaces();
        let scene: Vec<(u32, u32, i32, i32)> = surfaces
            .iter()
            .map(|surface| {
                (
                    surface.client_id.get(),
                    surface.surface_id.get(),
                    surface.x,
                    surface.y,
                )
            })
            .collect();
        self.render_mut().set_desktop_scene(&scene);
    }

    pub fn set_keyboard_keymap(&mut self, keymap: lumalla_shared::KeymapMemfd) {
        self.seat_manager.set_keymap(keymap);
    }

    /// Replace the keymap and re-advertise it to all existing `wl_keyboard` objects.
    pub fn update_keyboard_keymap(
        &mut self,
        clients: &mut ConnectedClients,
        keymap: lumalla_shared::KeymapMemfd,
    ) -> anyhow::Result<()> {
        self.seat_manager.update_keymap(clients, keymap)
    }

    pub fn set_keyboard_modifiers(&mut self, modifiers: seat::KeyboardModifiers) {
        self.seat_manager.set_modifiers(modifiers);
    }

    /// Configure linux-dmabuf format/modifier pairs and main DRM device advertised to clients.
    pub fn set_dmabuf_formats(
        &mut self,
        formats: Vec<(u32, u64)>,
        device_path: Option<&std::path::Path>,
        clients: &mut ConnectedClients,
    ) {
        self.dmabuf_manager
            .set_supported_formats(formats, device_path);
        self.dmabuf_manager.send_all_feedback(clients.values_mut());
    }

    pub fn flush_pending_keyboard_leaves(&mut self, clients: &mut ConnectedClients) {
        let left = self.seat_manager.flush_pending_keyboard_leaves(clients);
        for client_id in left {
            let Some(client) = clients.get_mut(&client_id) else {
                continue;
            };
            let _ = self
                .data_device_manager
                .on_keyboard_leave(client_id, client.writer_mut());
            clients.mark_send_needed(client_id);
        }
    }

    /// Focus keyboards on `surface`, advertising clipboard selection when the
    /// client newly gains keyboard focus (not on same-client surface switches).
    pub(crate) fn focus_keyboards_on_surface(
        &mut self,
        client_id: ClientId,
        surface: ObjectId,
        registry: &mut Registry,
        writer: &mut Writer,
    ) {
        let previous_client = self.seat_manager.focused_keyboard_surface().map(|(c, _)| c);
        let newly_focused = previous_client != Some(client_id);
        self.seat_manager
            .focus_keyboards_on_surface(client_id, surface, writer);
        if newly_focused {
            let _ = self
                .data_device_manager
                .on_keyboard_enter(client_id, registry, writer);
        }
    }

    /// Leave keyboards on `surface`, clearing the selection offer if the client
    /// no longer has any keyboard focus.
    pub(crate) fn leave_keyboards_on_surface(
        &mut self,
        client_id: ClientId,
        surface: ObjectId,
        writer: &mut Writer,
    ) {
        self.seat_manager
            .leave_keyboards_on_surface(client_id, surface, writer);
        let still_focused = self
            .seat_manager
            .focused_keyboard_surface()
            .is_some_and(|(c, _)| c == client_id);
        if !still_focused {
            let _ = self.data_device_manager.on_keyboard_leave(client_id, writer);
        }
    }

    pub fn flush_pending_data_device(&mut self, clients: &mut ConnectedClients) {
        self.data_device_manager.flush_pending(clients);
    }

    pub fn flush_pending_activation_configures(&mut self, clients: &mut ConnectedClients) {
        let pending = std::mem::take(&mut self.pending_activation_configures);
        for configure in pending {
            let Some(client) = clients.get_mut(&configure.client_id) else {
                continue;
            };
            protocols::xdg_shell::write_configure_snapshot(
                client.writer_mut(),
                configure.xdg_surface,
                configure.snapshot,
            );
        }
    }

    pub fn handle_keyboard_key(
        &mut self,
        clients: &mut ConnectedClients,
        time_msec: u32,
        key: u32,
        pressed: bool,
        arena: &Arena,
    ) {
        self.seat_manager
            .handle_key(clients, time_msec, key, pressed, arena);
    }

    pub fn handle_keyboard_modifiers(
        &mut self,
        clients: &mut ConnectedClients,
        modifiers: seat::KeyboardModifiers,
    ) {
        self.seat_manager.handle_modifiers(clients, modifiers);
    }

    /// Recompute pointer enter/leave from current coordinates and stacking.
    ///
    /// Call after client dispatch or when mapping changes under a stationary cursor.
    /// Skipped while a DnD grab owns the pointer — re-entering would steal the grab.
    pub fn refresh_pointer_focus(&mut self, clients: &mut ConnectedClients, arena: &Arena) {
        if self.data_device_manager.has_active_drag_grab() {
            return;
        }
        let views = self.pointer_views();
        self.seat_manager.update_pointer_focus_and_motion(
            clients,
            &self.surface_manager,
            &mut self.pointer_constraints_manager,
            &views,
            0,
            false,
            arena,
        );
    }

    pub fn set_output_geometry(&mut self, width: u32, height: u32) {
        self.seat_manager.set_output_geometry(width, height);
    }

    /// Output-local (monitor) pointer position.
    pub fn pointer_position(&self) -> (f64, f64) {
        self.seat_manager.pointer_position()
    }

    /// Move the compositor pointer without notifying Wayland clients.
    pub fn nudge_pointer(&mut self, dx: f64, dy: f64) {
        self.seat_manager.nudge_pointer(dx, dy);
    }

    /// Set the compositor pointer without notifying Wayland clients.
    pub fn set_pointer_position(&mut self, x: f64, y: f64) {
        self.seat_manager.set_pointer_position(x, y);
    }

    /// Map global compositor coordinates into output-local pointer space.
    pub fn map_scene_to_pointer(&self, x: f64, y: f64) -> (f64, f64) {
        map_source_to_dest(&self.pointer_views(), x, y)
    }

    /// Views used for pointer dest↔source mapping (primary/first output with views).
    pub fn pointer_views(&self) -> Vec<View> {
        self.pointer_output()
            .map(|output| output.views.clone())
            .unwrap_or_default()
    }

    fn pointer_output(&self) -> Option<&crate::output::OutputInfo> {
        self.output_manager
            .outputs()
            .find(|output| !output.views.is_empty())
            .or_else(|| self.output_manager.outputs().next())
    }

    /// Resolve pointer target in output-local coordinates: overlay/top layers,
    /// then desktop via views, then bottom/background layers.
    pub fn pointer_target_at_output_local(
        &self,
        px: f64,
        py: f64,
    ) -> Option<(ClientId, ObjectId)> {
        let output = self.pointer_output()?;
        let global = self.output_manager.global_by_name(&output.name)?;
        let roots = self.layer_shell_manager.mapped_roots_for_output(global);

        let mut above = Vec::new();
        let mut below = Vec::new();
        for (client_id, root, band) in &roots {
            let mut tree = allocator_api2::vec::Vec::new();
            self.surface_manager
                .collect_mapped_tree(*client_id, *root, &mut tree);
            use crate::layer_shell::LayerBand;
            match band {
                LayerBand::Overlay | LayerBand::Top => above.extend(tree),
                LayerBand::Bottom | LayerBand::Background => below.extend(tree),
            }
        }
        if let Some(target) = self.surface_manager.hit_test_scene(&above, px, py) {
            return Some(target);
        }
        let views = self.pointer_views();
        let (scene_x, scene_y) = map_dest_to_source(&views, px, py);
        if let Some(target) = self
            .surface_manager
            .global_pointer_target(None, scene_x, scene_y)
        {
            return Some(target);
        }
        self.surface_manager.hit_test_scene(&below, px, py)
    }

    pub fn pointer_cursor(&self) -> PointerCursor {
        self.seat_manager.pointer_cursor()
    }

    pub fn active_cursor(&self) -> Option<ActiveCursor> {
        self.seat_manager.active_cursor()
    }

    pub fn handle_pointer_motion(
        &mut self,
        clients: &mut ConnectedClients,
        time_msec: u32,
        dx: f64,
        dy: f64,
        dx_unaccel: f64,
        dy_unaccel: f64,
        arena: &Arena,
    ) {
        if self.data_device_manager.has_active_drag_grab() {
            self.seat_manager.nudge_pointer(dx, dy);
            self.drive_active_drag_motion(clients, time_msec);
            return;
        }
        let views = self.pointer_views();
        let (px, py) = self.seat_manager.pointer_position();
        let predicted = (px + dx, py + dy);
        let target = self.pointer_target_at_output_local(predicted.0, predicted.1);
        self.seat_manager.handle_pointer_motion_with_target(
            clients,
            &self.surface_manager,
            &mut self.pointer_constraints_manager,
            &self.relative_pointer_manager,
            &views,
            time_msec,
            dx,
            dy,
            dx_unaccel,
            dy_unaccel,
            target,
            arena,
        );
    }

    pub fn handle_pointer_absolute(
        &mut self,
        clients: &mut ConnectedClients,
        time_msec: u32,
        x: f64,
        y: f64,
        arena: &Arena,
    ) {
        if self.data_device_manager.has_active_drag_grab() {
            self.seat_manager.set_pointer_position(x, y);
            self.drive_active_drag_motion(clients, time_msec);
            return;
        }
        let views = self.pointer_views();
        let target = self.pointer_target_at_output_local(x, y);
        self.seat_manager.handle_pointer_absolute_with_target(
            clients,
            &self.surface_manager,
            &mut self.pointer_constraints_manager,
            &self.relative_pointer_manager,
            &views,
            time_msec,
            x,
            y,
            target,
            arena,
        );
    }

    fn drive_active_drag_motion(&mut self, clients: &mut ConnectedClients, time_msec: u32) {
        let views = self.pointer_views();
        let (px, py) = self.seat_manager.pointer_position();
        let (scene_x, scene_y) = map_dest_to_source(&views, px, py);
        let target = self
            .surface_manager
            .global_pointer_target(None, scene_x, scene_y);
        let (x, y) = match target {
            Some((tid, surface)) => self
                .surface_manager
                .surface_local_coords(tid, surface, scene_x, scene_y)
                .unwrap_or((scene_x as f32, scene_y as f32)),
            None => (scene_x as f32, scene_y as f32),
        };
        self.data_device_manager
            .drag_motion(time_msec, x, y, target, clients);
    }

    pub fn handle_pointer_button(
        &mut self,
        clients: &mut ConnectedClients,
        time_msec: u32,
        button: u32,
        pressed: bool,
        arena: &Arena,
    ) {
        if self.data_device_manager.has_active_drag_grab() {
            if !pressed {
                let icon = self.data_device_manager.take_drag_icon();
                self.data_device_manager.drag_drop(clients);
                if let Some((client_id, icon)) = icon {
                    let _ = self.surface_manager.clear_dnd_icon_role(client_id, icon);
                }
                // Grab ends on drop even if the offer lives until finish().
                self.refresh_pointer_focus(clients, arena);
            }
            return;
        }
        if pressed {
            // Resolve the top-most surface under the cursor before focusing; do not
            // trust sticky pointer focus from a covered window.
            let views = self.pointer_views();
            let (px, py) = self.seat_manager.pointer_position();
            let target = self.pointer_target_at_output_local(px, py);
            self.seat_manager.update_pointer_focus_and_motion_with_target(
                clients,
                &self.surface_manager,
                &mut self.pointer_constraints_manager,
                &views,
                time_msec,
                false,
                target,
                arena,
            );
            let click_target = self.seat_manager.focused_pointer_surface();
            if self.should_dismiss_popup_grab(click_target) {
                self.dismiss_popup_grabs(clients);
            }
            // Apply popup-aware keyboard focus once before the button event so
            // clients never see a leave/enter churn on non-grabbed popups.
            if let Some((client_id, surface)) = click_target {
                let is_layer = self
                    .surface_manager
                    .surface_in_layer_tree(client_id, surface);
                let allow_keyboard = self
                    .layer_shell_manager
                    .can_take_keyboard_focus(client_id, surface)
                    .unwrap_or(true);
                if allow_keyboard {
                    let focus_surface = self.keyboard_focus_for_pointer_target(client_id, surface);
                    if let Some(client) = clients.get_mut(&client_id) {
                        let (registry, writer) = client.registry_and_writer_mut();
                        self.focus_keyboards_on_surface(
                            client_id,
                            focus_surface,
                            registry,
                            writer,
                        );
                    }
                    self.flush_pending_keyboard_leaves(clients);
                    self.data_device_manager.flush_pending(clients);
                    if !is_layer {
                        self.on_surface_focused(client_id, focus_surface);
                        if let Some(client) = clients.get_mut(&client_id) {
                            self.apply_activation(client_id, focus_surface, client.writer_mut());
                        }
                        self.flush_pending_activation_configures(clients);
                    }
                }
                if !is_layer {
                    // Click-to-raise: move the window's paint-order root to the top.
                    let raise_surface = self.stack_root_for_pointer_target(client_id, surface);
                    self.surface_manager
                        .record_painted_surface(client_id, raise_surface);
                }
            }
        }
        self.seat_manager.handle_pointer_button(
            clients,
            &self.surface_manager,
            time_msec,
            button,
            pressed,
            arena,
        );
    }

    /// Keyboard focus target for a pointer hit, accounting for xdg popups.
    ///
    /// Non-grabbed popups (tooltips, some dropdowns) keep parent focus so a click
    /// does not briefly steal keyboard focus and trigger toolkit dismiss logic.
    fn keyboard_focus_for_pointer_target(
        &self,
        client_id: ClientId,
        surface: ObjectId,
    ) -> ObjectId {
        let mut current = surface;
        for _ in 0..32 {
            match self.xdg_manager.popup_info_for_wl(client_id, current) {
                Some(popup) if !popup.grabbed => {
                    let Some(parent) = self.xdg_manager.popup_parent_wl(client_id, popup.popup_id)
                    else {
                        break;
                    };
                    if parent == current {
                        break;
                    }
                    current = parent;
                }
                _ => break,
            }
        }
        current
    }

    /// Paint-order root for a pointer hit (subsurface / popup → toplevel).
    fn stack_root_for_pointer_target(
        &self,
        client_id: ClientId,
        surface: ObjectId,
    ) -> ObjectId {
        let mut current = surface;
        for _ in 0..32 {
            match self.surface_manager.parent_surface(client_id, current) {
                Some(parent) if parent != current => current = parent,
                _ => break,
            }
        }
        self.xdg_manager
            .activation_root_wl(client_id, current)
            .unwrap_or(current)
    }

    pub fn handle_pointer_axis(
        &mut self,
        clients: &mut ConnectedClients,
        time_msec: u32,
        axis: u32,
        value: f32,
        arena: &Arena,
    ) {
        self.seat_manager
            .handle_pointer_axis(clients, time_msec, axis, value, arena);
    }

    pub fn handle_touch_down(
        &mut self,
        clients: &mut ConnectedClients,
        time_msec: u32,
        touch_id: i32,
        x: f64,
        y: f64,
        arena: &Arena,
    ) {
        let views = self.pointer_views();
        self.seat_manager.handle_touch_down(
            clients,
            &self.surface_manager,
            &views,
            time_msec,
            touch_id,
            x,
            y,
            arena,
        );
    }

    pub fn handle_touch_up(
        &mut self,
        clients: &mut ConnectedClients,
        time_msec: u32,
        touch_id: i32,
        arena: &Arena,
    ) {
        self.seat_manager
            .handle_touch_up(clients, time_msec, touch_id, arena);
    }

    pub fn handle_touch_motion(
        &mut self,
        clients: &mut ConnectedClients,
        time_msec: u32,
        touch_id: i32,
        x: f64,
        y: f64,
        arena: &Arena,
    ) {
        let views = self.pointer_views();
        self.seat_manager.handle_touch_motion(
            clients,
            &self.surface_manager,
            &views,
            time_msec,
            touch_id,
            x,
            y,
            arena,
        );
    }

    pub fn handle_touch_frame(&mut self, clients: &mut ConnectedClients, arena: &Arena) {
        self.seat_manager.handle_touch_frame(clients, arena);
    }

    pub fn handle_touch_cancel(&mut self, clients: &mut ConnectedClients, arena: &Arena) {
        self.seat_manager.handle_touch_cancel(clients, arena);
    }

    /// Drive an active drag's motion for tests / compositor input.
    pub fn drag_motion(
        &mut self,
        _client_id: ClientId,
        clients: &mut ConnectedClients,
        time_msec: u32,
        x: f64,
        y: f64,
    ) {
        let target = self.surface_manager.global_pointer_target(None, x, y);
        self.data_device_manager
            .drag_motion(time_msec, x as f32, y as f32, target, clients);
    }

    /// Complete an active drag with a drop for tests / compositor input.
    pub fn drag_drop(&mut self, _client_id: ClientId, clients: &mut ConnectedClients) {
        self.data_device_manager.drag_drop(clients);
    }

    pub fn remove_client(&mut self, client_id: ClientId, clients: &mut ConnectedClients) {
        self.shm_manager.delete_client(client_id);
        self.dmabuf_manager.delete_client(client_id);
        self.surface_manager.delete_client(client_id);
        self.seat_manager.remove_client(client_id);
        self.pointer_constraints_manager.delete_client(client_id);
        self.relative_pointer_manager.delete_client(client_id);
        self.output_manager.remove_client(client_id);
        self.data_device_manager.remove_client(client_id);
        self.data_device_manager.flush_pending(clients);
        self.xdg_manager.delete_client(client_id);
        self.layer_shell_manager.delete_client(client_id);
        self.window_manager.delete_client(client_id);
        self.pending_frame_callbacks
            .retain(|pending| pending.client_id != client_id);
        self.pending_presentation_feedbacks
            .retain(|pending| pending.client_id != client_id);
        self.fallback_sink
            .submits
            .retain(|submit| submit.owner_id != client_id.get());
        self.fallback_sink
            .removed_surfaces
            .retain(|(owner, _)| *owner != client_id.get());
        if let Err(err) = self.render_mut().remove_client(client_id.get()) {
            log::error!("Unable to remove client frames from renderer: {err:#}");
        } else {
            self.render_mut().request_present();
            self.render_content_changed = true;
        }
    }

    /// Current mapped scene in authoritative back-to-front order.
    pub fn scene_surfaces(&self) -> Vec<SceneSurface> {
        self.surface_manager.scene_surfaces()
    }

    /// Append the mapped scene into `scene` using the provided allocator.
    pub fn collect_scene_surfaces<A: allocator_api2::alloc::Allocator>(
        &self,
        scene: &mut allocator_api2::vec::Vec<SceneSurface, A>,
    ) {
        self.surface_manager.collect_scene_surfaces(scene);
    }

    /// Per-output layer-shell scenes in paint order (background → overlay).
    ///
    /// Each entry is `(output_name, [(owner, surface, x, y, band), ...])` with
    /// coordinates in output-local space and band `0..=3`.
    pub fn collect_output_layer_scenes(&self) -> Vec<(String, Vec<(u32, u32, i32, i32, u8)>)> {
        let mut scenes = Vec::new();
        for info in self.output_manager.outputs() {
            let Some(global) = self.output_manager.global_by_name(&info.name) else {
                continue;
            };
            let roots = self.layer_shell_manager.mapped_roots_for_output(global);
            if roots.is_empty() {
                scenes.push((info.name.clone(), Vec::new()));
                continue;
            }
            let mut entries = Vec::new();
            for (client_id, root, band) in roots {
                let mut tree = allocator_api2::vec::Vec::new();
                self.surface_manager
                    .collect_mapped_tree(client_id, root, &mut tree);
                for surface in tree {
                    entries.push((
                        surface.client_id.get(),
                        surface.surface_id.get(),
                        surface.x,
                        surface.y,
                        band.as_u8(),
                    ));
                }
            }
            scenes.push((info.name.clone(), entries));
        }
        scenes
    }

    /// Work area in global compositor space, inset by layer-shell exclusive zones.
    pub fn exclusive_work_area_for_point(&self, x: i32, y: i32) -> Option<(i32, i32, i32, i32)> {
        let output = self.output_manager.outputs().find(|output| {
            x >= output.x
                && y >= output.y
                && x < output.x.saturating_add(output.width)
                && y < output.y.saturating_add(output.height)
        })
        .or_else(|| self.output_manager.outputs().next())?;
        let global = self.output_manager.global_by_name(&output.name)?;
        let insets = self.layer_shell_manager.exclusive_insets_for_output(global);
        Some(insets.inset_rect(output.x, output.y, output.width, output.height))
    }

    /// Maximized size for an output, inset by exclusive zones.
    pub fn exclusive_logical_size_for_client_output(
        &self,
        client_id: ClientId,
        output_id: Option<ObjectId>,
    ) -> Option<(i32, i32)> {
        let global = output_id
            .and_then(|id| self.output_manager.global_for_binding(client_id, id))
            .or_else(|| self.output_manager.primary_global_id())?;
        let info = self.output_manager.get(global)?;
        let insets = self.layer_shell_manager.exclusive_insets_for_output(global);
        Some(insets.inset_size(info.width, info.height))
    }

    /// Capture layers for a managed window: content AABB and surface tree.
    ///
    /// Returns `(window_id, origin_x, origin_y, width, height, layers)`. `window_id`
    /// of `0` / `None` resolves to the focused window.
    pub fn window_capture_layers(
        &self,
        window_id: Option<u32>,
    ) -> Option<(u32, i32, i32, i32, i32, Vec<SceneSurface>)> {
        let resolved = self.window_manager.resolve_window_id(window_id).ok()?;
        let (client_id, root) = self.window_manager.resolve_surface(Some(resolved)).ok()?;
        let layers = self.surface_manager.collect_surface_tree(client_id, root);
        let (x, y, width, height) = self.surface_manager.bounds_of_scene_surfaces(&layers)?;
        if width <= 0 || height <= 0 {
            return None;
        }
        Some((resolved, x, y, width, height, layers))
    }

    pub fn pending_frame_callback_count(&self) -> usize {
        self.pending_frame_callbacks.len()
    }

    pub fn pending_presentation_feedback_count(&self) -> usize {
        self.pending_presentation_feedbacks.len()
    }

    /// Queues presentation feedback for a committed surface, discarding any prior
    /// in-flight feedback for the same surface (superseded content).
    pub(crate) fn queue_presentation_feedbacks(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
        feedbacks: Vec<ObjectId>,
        writer: &mut Writer,
        registry: &mut Registry,
    ) {
        self.discard_in_flight_presentation_feedbacks(client_id, surface_id, writer, registry);
        for feedback_id in feedbacks {
            self.pending_presentation_feedbacks
                .push_back(PendingPresentationFeedback {
                    client_id,
                    surface_id,
                    feedback_id,
                });
        }
    }

    /// Discards in-flight feedback for a surface, plus any still-pending object IDs
    /// returned from surface destroy (not yet committed).
    pub(crate) fn discard_presentation_feedbacks_for_surface(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
        pending_on_surface: Vec<ObjectId>,
        writer: &mut Writer,
        registry: &mut Registry,
    ) {
        self.discard_in_flight_presentation_feedbacks(client_id, surface_id, writer, registry);
        for feedback_id in pending_on_surface {
            send_presentation_discarded(writer, registry, feedback_id);
        }
    }

    fn discard_in_flight_presentation_feedbacks(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
        writer: &mut Writer,
        registry: &mut Registry,
    ) {
        let mut remaining = VecDeque::new();
        while let Some(pending) = self.pending_presentation_feedbacks.pop_front() {
            if pending.client_id == client_id && pending.surface_id == surface_id {
                send_presentation_discarded(writer, registry, pending.feedback_id);
            } else {
                remaining.push_back(pending);
            }
        }
        self.pending_presentation_feedbacks = remaining;
    }

    /// Cancels in-flight frame callbacks for a surface, plus any still-pending object IDs
    /// returned from surface destroy (not yet committed). Sends `delete_id` without `done`.
    pub(crate) fn discard_frame_callbacks_for_surface(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
        pending_on_surface: Vec<ObjectId>,
        writer: &mut Writer,
        registry: &mut Registry,
    ) {
        self.discard_in_flight_frame_callbacks(client_id, surface_id, writer, registry);
        for callback_id in pending_on_surface {
            registry.free_object(callback_id, writer);
        }
    }

    fn discard_in_flight_frame_callbacks(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
        writer: &mut Writer,
        registry: &mut Registry,
    ) {
        let mut remaining = VecDeque::new();
        while let Some(pending) = self.pending_frame_callbacks.pop_front() {
            if pending.client_id == client_id && pending.surface_id == surface_id {
                registry.free_object(pending.callback_id, writer);
            } else {
                remaining.push_back(pending);
            }
        }
        self.pending_frame_callbacks = remaining;
    }

    /// Frees frame/presentation objects taken by a commit that failed before queuing.
    pub(crate) fn abandon_commit_timing_objects(
        writer: &mut Writer,
        registry: &mut Registry,
        frame_callbacks: impl IntoIterator<Item = ObjectId>,
        presentation_feedbacks: impl IntoIterator<Item = ObjectId>,
    ) {
        for callback_id in frame_callbacks {
            registry.free_object(callback_id, writer);
        }
        for feedback_id in presentation_feedbacks {
            send_presentation_discarded(writer, registry, feedback_id);
        }
    }

    /// Completes deferred `wl_surface.frame` callbacks after presentation.
    pub fn complete_frame_callbacks(&mut self, clients: &mut ConnectedClients, time_msec: u32) {
        while let Some(pending) = self.pending_frame_callbacks.pop_front() {
            let Some(client) = clients.get_mut(&pending.client_id) else {
                continue;
            };
            let (registry, writer) = client.registry_and_writer_mut();
            if registry.interface_index(pending.callback_id) != Some(InterfaceIndex::WlCallback) {
                continue;
            }
            writer
                .wl_callback_done(pending.callback_id)
                .callback_data(time_msec);
            registry.free_object(pending.callback_id, writer);
        }
    }

    /// Completes pending `wp_presentation_feedback` objects after a DRM page-flip.
    pub fn complete_presentation_feedbacks(
        &mut self,
        clients: &mut ConnectedClients,
        flip: PresentationFlipInfo,
    ) {
        let flags = WP_PRESENTATION_FEEDBACK_KIND_VSYNC
            | WP_PRESENTATION_FEEDBACK_KIND_HW_CLOCK
            | WP_PRESENTATION_FEEDBACK_KIND_HW_COMPLETION;
        let tv_nsec = flip.tv_usec.saturating_mul(1000);
        while let Some(pending) = self.pending_presentation_feedbacks.pop_front() {
            let Some(client) = clients.get_mut(&pending.client_id) else {
                continue;
            };
            let outputs = self
                .output_manager
                .bound_outputs_for_client(pending.client_id);
            let (registry, writer) = client.registry_and_writer_mut();
            for output in outputs {
                writer
                    .wp_presentation_feedback_sync_output(pending.feedback_id)
                    .output(output);
            }
            writer
                .wp_presentation_feedback_presented(pending.feedback_id)
                .tv_sec_hi(0)
                .tv_sec_lo(flip.tv_sec)
                .tv_nsec(tv_nsec)
                .refresh(flip.refresh_ns)
                .seq_hi(0)
                .seq_lo(flip.sequence)
                .flags(flags);
            registry.free_object(pending.feedback_id, writer);
        }
    }

    /// Updates the primary `wl_output` geometry (e.g. from DRM mode) and notifies binders.
    pub fn update_primary_output(&mut self, info: OutputInfo, clients: &mut ConnectedClients) {
        if let Some(global_id) = self.output_manager.primary_global_id() {
            self.output_manager.update_output(global_id, info, clients);
        }
    }

    pub fn add_output(
        &mut self,
        info: OutputInfo,
        clients: &mut ConnectedClients,
    ) -> anyhow::Result<GlobalId> {
        self.output_manager
            .add_output(info, &mut self.globals, clients.values_mut())
    }

    pub fn remove_output(
        &mut self,
        name: &str,
        clients: &mut ConnectedClients,
    ) -> anyhow::Result<()> {
        if let Some(global) = self.output_manager.global_by_name(name) {
            let closed = self.layer_shell_manager.close_output(global);
            for (client_id, layer_id, wl_surface) in closed {
                let _ = self
                    .surface_manager
                    .set_xdg_map_ready(client_id, wl_surface, false);
                self.emit_surface_unmapped(client_id, wl_surface);
                if let Some(client) = clients.get_mut(&client_id) {
                    client
                        .writer_mut()
                        .zwlr_layer_surface_v1_closed(layer_id);
                }
            }
        }
        self.output_manager
            .remove_output(name, &mut self.globals, clients.values_mut())
    }

    pub fn add_view(
        &mut self,
        output_name: &str,
        view: lumalla_shared::View,
        clients: &mut ConnectedClients,
    ) -> anyhow::Result<bool> {
        self.output_manager.add_view(output_name, view, clients)
    }

    pub fn remove_view(
        &mut self,
        output_name: &str,
        view_name: &str,
        clients: &mut ConnectedClients,
    ) -> anyhow::Result<()> {
        self.output_manager
            .remove_view(output_name, view_name, clients)
    }

    pub fn outputs(&self) -> impl Iterator<Item = &OutputInfo> {
        self.output_manager.outputs()
    }

    pub fn activate_main_seat(
        &mut self,
        seat_name: String,
        clients: &mut ConnectedClients,
    ) -> anyhow::Result<()> {
        self.seat_manager
            .add_main_seat(seat_name, &mut self.globals, clients.values_mut())?;
        Ok(())
    }

    pub fn set_window(
        &mut self,
        id: Option<u32>,
        geometry: WindowGeometryUpdate,
        user_initiated: bool,
        clients: &mut ConnectedClients,
    ) -> Result<bool, WindowError> {
        let changes = self.window_manager.set_window(
            id,
            geometry,
            user_initiated,
            &self.surface_manager,
            &mut self.xdg_manager,
        )?;
        Ok(self.apply_geometry_changes(changes, clients))
    }

    /// Give keyboard focus and xdg activation to a window.
    ///
    /// When `raise` is true, also move the window to the top of paint order and
    /// push the updated scene to the active [`RenderSink`].
    pub fn focus_window(
        &mut self,
        id: Option<u32>,
        raise: bool,
        clients: &mut ConnectedClients,
    ) -> Result<bool, WindowError> {
        let (client_id, wl_surface) = self.window_manager.resolve_surface(id)?;
        if let Some(client) = clients.get_mut(&client_id) {
            let (registry, writer) = client.registry_and_writer_mut();
            self.focus_keyboards_on_surface(client_id, wl_surface, registry, writer);
        }
        self.flush_pending_keyboard_leaves(clients);
        self.data_device_manager.flush_pending(clients);
        self.on_surface_focused(client_id, wl_surface);
        if let Some(client) = clients.get_mut(&client_id) {
            self.apply_activation(client_id, wl_surface, client.writer_mut());
        }
        self.flush_pending_activation_configures(clients);

        let mut raised = false;
        if raise {
            self.surface_manager
                .record_painted_surface(client_id, wl_surface);
            self.push_renderer_scenes();
            self.render_mut().request_present();
            raised = true;
        }
        Ok(raised)
    }

    /// Raise a window to the top of paint order without changing keyboard focus.
    pub fn raise_window(&mut self, id: Option<u32>) -> Result<(), WindowError> {
        let (client_id, wl_surface) = self.window_manager.resolve_surface(id)?;
        self.surface_manager
            .record_painted_surface(client_id, wl_surface);
        self.push_renderer_scenes();
        self.render_mut().request_present();
        Ok(())
    }

    /// Ask a client to close a window via `xdg_toplevel.close`.
    ///
    /// This is a request only; the client may ignore it or prompt the user.
    pub fn close_window(
        &mut self,
        id: Option<u32>,
        clients: &mut ConnectedClients,
    ) -> Result<(), WindowError> {
        let (client_id, toplevel) = self.window_manager.resolve_toplevel(id)?;
        if let Some(client) = clients.get_mut(&client_id) {
            client.writer_mut().xdg_toplevel_close(toplevel);
        }
        Ok(())
    }

    pub fn add_window_rule(&mut self, rule: WindowRule) {
        self.window_manager.add_rule(rule);
    }

    pub fn clear_window_rules(&mut self) {
        self.window_manager.clear_rules();
    }

    pub fn add_zone(&mut self, zone: lumalla_shared::Zone) {
        self.window_manager.add_zone(zone);
    }

    pub fn remove_zone(&mut self, name: &str) -> bool {
        self.window_manager.remove_zone(name)
    }

    pub fn add_window_to_zone(
        &mut self,
        id: Option<u32>,
        zone: &str,
        clients: &mut ConnectedClients,
    ) -> Result<bool, WindowError> {
        let changes = self.window_manager.add_window_to_zone(
            id,
            zone,
            &self.surface_manager,
            &mut self.xdg_manager,
        )?;
        Ok(self.apply_geometry_changes(changes, clients))
    }

    pub fn remove_window_from_zone(&mut self, id: Option<u32>) -> Result<(), WindowError> {
        self.window_manager.remove_window_from_zone(id)
    }

    pub fn window_states(&self) -> Vec<WindowState> {
        self.window_manager
            .window_states(&self.surface_manager, &self.xdg_manager)
    }

    pub fn focused_window_id(&self) -> Option<u32> {
        self.window_manager.focused_window_id()
    }

    pub(crate) fn register_toplevel(
        &mut self,
        client_id: ClientId,
        toplevel: ObjectId,
        xdg_surface: ObjectId,
        wl_surface: ObjectId,
    ) -> (i32, i32) {
        self.window_manager.register_toplevel(
            client_id,
            toplevel,
            xdg_surface,
            wl_surface,
            &mut self.surface_manager,
        )
    }

    pub(crate) fn unregister_toplevel(&mut self, client_id: ClientId, toplevel: ObjectId) {
        self.window_manager.unregister_toplevel(client_id, toplevel);
    }

    pub fn drain_pending_geometry(&mut self, clients: &mut ConnectedClients) -> bool {
        if self.pending_geometry_changes.is_empty() {
            return false;
        }
        let changes = std::mem::take(&mut self.pending_geometry_changes);
        self.apply_geometry_changes(changes, clients)
    }

    pub(crate) fn queue_rule_geometry_for_toplevel(
        &mut self,
        client_id: ClientId,
        toplevel: ObjectId,
        app_id: String,
    ) {
        let surface_manager = &self.surface_manager;
        let changes = self.window_manager.on_app_id_set(
            client_id,
            toplevel,
            app_id,
            surface_manager,
            &mut self.xdg_manager,
        );
        self.pending_geometry_changes.extend(changes);
    }

    pub(crate) fn on_toplevel_title_set(
        &mut self,
        client_id: ClientId,
        toplevel: ObjectId,
        title: String,
    ) {
        let surface_manager = &self.surface_manager;
        let changes = self.window_manager.on_title_set(
            client_id,
            toplevel,
            title,
            surface_manager,
            &mut self.xdg_manager,
        );
        self.pending_geometry_changes.extend(changes);
    }

    pub(crate) fn on_surface_focused(&mut self, client_id: ClientId, wl_surface: ObjectId) {
        let focus_wl = self
            .xdg_manager
            .activation_root_wl(client_id, wl_surface)
            .unwrap_or(wl_surface);
        self.window_manager
            .set_focus_from_surface(client_id, focus_wl);
    }

    /// Apply keyboard/activation policy for a newly mapped surface.
    ///
    /// Non-grabbed popups keep parent focus. Grabbed popups take keyboard focus
    /// while the parent toplevel stays activated. Subsurfaces never take focus —
    /// clients (e.g. Chromium omnibox) map short-lived overlays that must not
    /// steal keyboard enter/leave from the parent.
    pub(crate) fn focus_newly_mapped_surface(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
        registry: &mut Registry,
        writer: &mut Writer,
    ) {
        if self
            .surface_manager
            .surface_role_is_subsurface(client_id, surface_id)
        {
            return;
        }

        if self.surface_manager.shell_mode(client_id, surface_id) == Some(surface::ShellMode::Popup)
        {
            return;
        }

        if let Some(info) = self.layer_shell_manager.info_for_wl(client_id, surface_id) {
            use crate::layer_shell::KeyboardInteractivity;
            match info.keyboard_interactivity {
                // none / on_demand: no automatic keyboard focus on map.
                KeyboardInteractivity::None | KeyboardInteractivity::OnDemand => return,
                KeyboardInteractivity::Exclusive => {
                    if matches!(
                        info.band,
                        crate::layer_shell::LayerBand::Top | crate::layer_shell::LayerBand::Overlay
                    ) {
                        self.focus_keyboards_on_surface(client_id, surface_id, registry, writer);
                    }
                    return;
                }
            }
        }

        if let Some(popup) = self.xdg_manager.popup_info_for_wl(client_id, surface_id) {
            if !popup.grabbed {
                return;
            }
            self.focus_keyboards_on_surface(client_id, surface_id, registry, writer);
            if let Some(parent_wl) = self.xdg_manager.popup_parent_wl(client_id, popup.popup_id) {
                self.on_surface_focused(client_id, parent_wl);
                self.apply_activation(client_id, parent_wl, writer);
            }
            return;
        }

        self.focus_keyboards_on_surface(client_id, surface_id, registry, writer);
        self.on_surface_focused(client_id, surface_id);
        self.apply_activation(client_id, surface_id, writer);
    }

    /// Leave keyboard focus on `surface_id` if it currently has it, restoring
    /// focus to a parent when possible (subsurface / popup child).
    ///
    /// Computes the restore target before leaving so parent links can still be
    /// walked (call before clearing role/subsurface parent relationships).
    pub(crate) fn release_keyboard_focus_from_surface(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
        registry: &mut Registry,
        writer: &mut Writer,
    ) {
        let had_focus = self.seat_manager.focused_keyboard_surface().is_some_and(
            |(focus_client, focus_surface)| {
                focus_client == client_id && focus_surface == surface_id
            },
        );
        let restore = had_focus
            .then(|| self.keyboard_focus_restore_target(client_id, surface_id))
            .flatten();
        self.leave_keyboards_on_surface(client_id, surface_id, writer);
        let Some(restore) = restore else {
            return;
        };
        self.focus_keyboards_on_surface(client_id, restore, registry, writer);
        self.on_surface_focused(client_id, restore);
        self.apply_activation(client_id, restore, writer);
    }

    /// Prefer a still-usable parent for keyboard focus after `surface_id` unmaps.
    fn keyboard_focus_restore_target(
        &self,
        client_id: ClientId,
        surface_id: ObjectId,
    ) -> Option<ObjectId> {
        let mut current = self.surface_manager.parent_surface(client_id, surface_id)?;
        for _ in 0..32 {
            // Non-grabbed popups are not keyboard targets; keep walking up.
            if let Some(popup) = self.xdg_manager.popup_info_for_wl(client_id, current) {
                if !popup.grabbed {
                    current = self
                        .xdg_manager
                        .popup_parent_wl(client_id, popup.popup_id)?;
                    continue;
                }
            }
            if self
                .surface_manager
                .surface_role_is_subsurface(client_id, current)
            {
                current = self.surface_manager.parent_surface(client_id, current)?;
                continue;
            }
            return Some(current);
        }
        None
    }

    pub(crate) fn apply_activation(
        &mut self,
        client_id: ClientId,
        wl_surface: ObjectId,
        writer: &mut Writer,
    ) {
        let target = self
            .xdg_manager
            .toplevel_for_wl(client_id, wl_surface)
            .map(|(toplevel, _)| (client_id, toplevel));
        let configures = self.xdg_manager.set_activated(target);
        for configure in configures {
            if configure.client_id == client_id {
                protocols::xdg_shell::write_configure_snapshot(
                    writer,
                    configure.xdg_surface,
                    configure.snapshot,
                );
            } else {
                self.pending_activation_configures.push(configure);
            }
        }
    }

    fn should_dismiss_popup_grab(&self, click_target: Option<(ClientId, ObjectId)>) -> bool {
        if !self.xdg_manager.has_popup_grab() {
            return false;
        }
        let Some((client_id, target_wl)) = click_target else {
            return true;
        };
        !self
            .xdg_manager
            .pointer_target_in_popup_grab(client_id, target_wl, |popup_wl, target| {
                self.surface_manager
                    .is_descendant_of(client_id, popup_wl, target)
            })
    }

    fn dismiss_popup_grabs(&mut self, clients: &mut ConnectedClients) {
        let parent_focus =
            self.xdg_manager
                .bottom_popup_grab()
                .and_then(|(client_id, popup_id)| {
                    self.xdg_manager
                        .popup_parent_wl(client_id, popup_id)
                        .map(|parent_wl| (client_id, parent_wl))
                });
        let dismissed = self.xdg_manager.dismiss_all_popup_grabs();
        for (client_id, popup_id) in dismissed {
            let Some(client) = clients.get_mut(&client_id) else {
                continue;
            };
            client.writer_mut().xdg_popup_popup_done(popup_id);
        }
        if let Some((client_id, parent_wl)) = parent_focus
            && let Some(client) = clients.get_mut(&client_id)
        {
            let (registry, writer) = client.registry_and_writer_mut();
            self.focus_keyboards_on_surface(client_id, parent_wl, registry, writer);
            self.on_surface_focused(client_id, parent_wl);
            if let Some(client) = clients.get_mut(&client_id) {
                self.apply_activation(client_id, parent_wl, client.writer_mut());
            }
        }
        self.flush_pending_keyboard_leaves(clients);
        self.flush_pending_data_device(clients);
        self.flush_pending_activation_configures(clients);
    }

    pub(crate) fn flush_pending_window_configures(&mut self, clients: &mut ConnectedClients) {
        let pending = self.window_manager.take_pending_configures();
        for configure in pending {
            let Some(client) = clients.get_mut(&configure.client_id) else {
                continue;
            };
            self.emit_toplevel_configure(
                configure.client_id,
                client.writer_mut(),
                configure.xdg_surface,
                configure.toplevel,
            );
        }
    }

    pub(crate) fn emit_toplevel_configure(
        &mut self,
        client_id: ClientId,
        writer: &mut Writer,
        xdg_surface_id: ObjectId,
        toplevel_id: ObjectId,
    ) {
        let Ok(snapshot) = self.xdg_manager.toplevel_configure(client_id, toplevel_id) else {
            return;
        };
        protocols::xdg_shell::write_configure_snapshot(writer, xdg_surface_id, snapshot);
    }

    fn apply_geometry_changes(
        &mut self,
        changes: Vec<WindowGeometryChange>,
        clients: &mut ConnectedClients,
    ) -> bool {
        let mut changed = false;
        for change in changes {
            if let Some((x, y)) = change.position {
                let _ = self.surface_manager.set_surface_layout(
                    change.client_id,
                    change.wl_surface,
                    x,
                    y,
                );
                changed = true;
            }
        }
        self.flush_pending_window_configures(clients);
        if changed {
            self.push_renderer_scenes();
            self.render_mut().request_present();
        }
        changed
    }
}

fn send_presentation_discarded(
    writer: &mut Writer,
    registry: &mut Registry,
    feedback_id: ObjectId,
) {
    writer.wp_presentation_feedback_discarded(feedback_id);
    registry.free_object(feedback_id, writer);
}

pub fn create_wayland_display(socket_path: Option<PathBuf>) -> anyhow::Result<Wayland> {
    if let Some(socket_path) = socket_path {
        Wayland::new(socket_path).context("Failed to create Wayland display at given socket path")
    } else {
        let xdg_runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .context("XDG_RUNTIME_DIR not set. Set the socket path manually using --socket-path")?;
        let xdg_runtime_dir = PathBuf::from(xdg_runtime_dir);
        for i in 0..10 {
            let socket_path = xdg_runtime_dir.join(format!("wayland-{i}"));
            if let Ok(wayland) = Wayland::new(socket_path) {
                return Ok(wayland);
            }
        }
        anyhow::bail!("Failed to create Wayland display");
    }
}

pub(crate) type GlobalId = u32;

#[derive(Debug)]
struct Globals {
    globals: HashMap<GlobalId, Global>,
    next_id: GlobalId,
}

#[derive(Debug)]
struct Global {
    name: &'static str,
    version: u32,
    interface_index: InterfaceIndex,
}

impl Default for Globals {
    fn default() -> Self {
        let mut globals = Self {
            globals: HashMap::new(),
            next_id: 1,
        };
        globals.register_version(InterfaceIndex::WlCompositor, 5, [].into_iter());
        globals.register_version(InterfaceIndex::WlShm, 2, [].into_iter());
        globals.register_version(InterfaceIndex::WlShell, 1, [].into_iter());
        globals.register_version(InterfaceIndex::WlSubcompositor, 1, [].into_iter());
        globals.register_version(InterfaceIndex::WlFixes, 1, [].into_iter());
        globals.register_version(InterfaceIndex::WlDataDeviceManager, 3, [].into_iter());
        // Advertise at least v2 so clients like xwayland-satellite (binds 2..=6) can
        // connect; v3 covers popup reposition / reactive positioner. Do not claim v4+
        // until configure_bounds / wm_capabilities are emitted.
        globals.register_version(InterfaceIndex::XdgWmBase, 3, [].into_iter());
        // Stable linux-dmabuf keeps the zwp_ interface name; advertise v4 for
        // format/modifier events, create_immed, and feedback format_table.
        globals.register_version(InterfaceIndex::ZwpLinuxDmabufV1, 4, [].into_iter());
        globals.register_version(InterfaceIndex::WpPresentation, 2, [].into_iter());
        globals.register_version(InterfaceIndex::WpViewporter, 1, [].into_iter());
        globals.register_version(InterfaceIndex::ZwpPointerConstraintsV1, 1, [].into_iter());
        globals.register_version(
            InterfaceIndex::ZwpRelativePointerManagerV1,
            1,
            [].into_iter(),
        );
        globals.register_version(InterfaceIndex::ZwlrLayerShellV1, 5, [].into_iter());
        globals
    }
}

impl Globals {
    /// Registers a global with the given interface index and returns the global id.
    /// Additionally, makes sure to broadcast the global to all connected clients.
    fn register<'connection>(
        &mut self,
        interface_index: InterfaceIndex,
        client_connections: impl Iterator<Item = &'connection mut ClientConnection>,
    ) -> GlobalId {
        self.register_version(
            interface_index,
            interface_index.interface_version(),
            client_connections,
        )
    }

    pub(crate) fn register_version<'connection>(
        &mut self,
        interface_index: InterfaceIndex,
        version: u32,
        client_connections: impl Iterator<Item = &'connection mut ClientConnection>,
    ) -> GlobalId {
        debug_assert!(version > 0 && version <= interface_index.interface_version());
        let id = self.next_id;
        self.next_id += 1;
        self.globals.insert(
            id,
            Global {
                name: interface_index.interface_name(),
                version,
                interface_index,
            },
        );
        for client in client_connections {
            client.broadcast_global(id, interface_index, version);
        }
        id
    }

    fn iter(&self) -> impl Iterator<Item = (&u32, &Global)> {
        self.globals.iter()
    }

    pub(crate) fn get(&self, id: u32) -> Option<&Global> {
        self.globals.get(&id)
    }

    pub(crate) fn unregister<'connection>(
        &mut self,
        id: GlobalId,
        client_connections: impl Iterator<Item = &'connection mut ClientConnection>,
    ) {
        if self.globals.remove(&id).is_none() {
            return;
        }
        for client in client_connections {
            client.broadcast_global_remove(id);
        }
    }
}
