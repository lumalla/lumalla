use std::collections::{HashMap, VecDeque};

use anyhow::Context;
use lumalla_shared::{Comms, WindowGeometryUpdate, WindowRule, WindowState};
use lumalla_wayland_protocol::protocols::presentation_time::{
    WP_PRESENTATION_FEEDBACK_KIND_HW_CLOCK, WP_PRESENTATION_FEEDBACK_KIND_HW_COMPLETION,
    WP_PRESENTATION_FEEDBACK_KIND_VSYNC,
};
use lumalla_wayland_protocol::registry::InterfaceIndex;
use lumalla_wayland_protocol::{ObjectId, buffer::Writer, registry::Registry};

use crate::{
    data_device::DataDeviceManager, dmabuf::DmabufManager, output::OutputManager,
    seat::SeatManager, shm::ShmManager, surface::SurfaceManager, window_manager::WindowManager,
    xdg::XdgManager,
};

mod data_device;
mod dmabuf;
mod output;
mod protocols;
mod seat;
mod shm;
mod surface;
mod window_manager;
mod xdg;

pub use dmabuf::ExportedDmabuf;
pub use lumalla_wayland_protocol::{ClientConnection, ClientId, Wayland, buffer::ReadResult};
pub use output::OutputInfo;
pub use seat::{ActiveCursor, KeyboardModifiers};
pub use surface::Rectangle;
pub use window_manager::{WindowError, WindowGeometryChange};

/// Presentation timing for a completed DRM page-flip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentationFlipInfo {
    pub tv_sec: u32,
    pub tv_usec: u32,
    pub sequence: u32,
    pub refresh_ns: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingPresentationFeedback {
    client_id: ClientId,
    surface_id: ObjectId,
    feedback_id: ObjectId,
}

pub struct DisplayMessage;

#[derive(Debug)]
pub struct CommittedFrame {
    pub client_id: ClientId,
    pub surface_id: lumalla_wayland_protocol::ObjectId,
    pub buffer_id: lumalla_wayland_protocol::ObjectId,
    pub pixels: Vec<u8>,
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
    /// Surface-local size after viewport destination / crop (or buffer/scale).
    pub surface_width: i32,
    pub surface_height: i32,
    /// Viewport source rectangle in post-scale coords, if set.
    pub viewport_src: Option<(f32, f32, f32, f32)>,
    /// Populated for linux-dmabuf commits; renderer imports this FD on the GPU.
    pub dmabuf: Option<ExportedDmabuf>,
    /// Output-space regions that changed this commit.
    pub damage: Vec<Rectangle>,
    /// Buffer-space regions that changed this commit (for GPU texture uploads).
    pub buffer_damage: Vec<Rectangle>,
    /// When true, the entire surface area must be recomposited.
    pub full_surface: bool,
}

#[derive(Debug)]
pub enum SurfaceUpdate {
    Frame(CommittedFrame),
    Cursor(CommittedFrame),
    Unmapped {
        client_id: ClientId,
        surface_id: lumalla_wayland_protocol::ObjectId,
    },
}

/// Renderer position update after a window move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RendererLayoutSync {
    pub owner_id: u32,
    pub surface_id: u32,
    pub x: i32,
    pub y: i32,
}

pub struct DisplayState {
    _comms: Comms,
    globals: Globals,
    surface_manager: SurfaceManager,
    shm_manager: ShmManager,
    dmabuf_manager: DmabufManager,
    seat_manager: SeatManager,
    output_manager: OutputManager,
    data_device_manager: DataDeviceManager,
    xdg_manager: XdgManager,
    window_manager: WindowManager,
    surface_updates: VecDeque<SurfaceUpdate>,
    pending_geometry_changes: Vec<WindowGeometryChange>,
    pending_frame_callbacks: VecDeque<(ClientId, lumalla_wayland_protocol::ObjectId)>,
    pending_presentation_feedbacks: VecDeque<PendingPresentationFeedback>,
}

impl DisplayState {
    pub fn new(comms: Comms) -> anyhow::Result<Self> {
        Ok(Self {
            _comms: comms,
            globals: Globals::default(),
            surface_manager: SurfaceManager::default(),
            shm_manager: ShmManager::default(),
            dmabuf_manager: DmabufManager::default(),
            seat_manager: SeatManager::default(),
            output_manager: OutputManager::default(),
            data_device_manager: DataDeviceManager::default(),
            xdg_manager: XdgManager::default(),
            window_manager: WindowManager::default(),
            surface_updates: VecDeque::new(),
            pending_geometry_changes: Vec::new(),
            pending_frame_callbacks: VecDeque::new(),
            pending_presentation_feedbacks: VecDeque::new(),
        })
    }

    pub fn set_keyboard_keymap(&mut self, keymap: lumalla_shared::KeymapMemfd) {
        self.seat_manager.set_keymap(keymap);
    }

    pub fn set_keyboard_modifiers(&mut self, modifiers: seat::KeyboardModifiers) {
        self.seat_manager.set_modifiers(modifiers);
    }

    /// Configure linux-dmabuf format/modifier pairs and main DRM device advertised to clients.
    pub fn set_dmabuf_formats(
        &mut self,
        formats: Vec<(u32, u64)>,
        device_path: Option<&std::path::Path>,
        clients: &mut HashMap<ClientId, ClientConnection>,
    ) {
        self.dmabuf_manager
            .set_supported_formats(formats, device_path);
        self.dmabuf_manager.send_all_feedback(clients.values_mut());
    }

    pub fn flush_pending_keyboard_leaves(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
    ) {
        self.seat_manager.flush_pending_keyboard_leaves(clients);
    }

    pub fn handle_keyboard_key(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        time_msec: u32,
        key: u32,
        pressed: bool,
    ) {
        self.seat_manager
            .handle_key(clients, time_msec, key, pressed);
    }

    pub fn handle_keyboard_modifiers(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        modifiers: seat::KeyboardModifiers,
    ) {
        self.seat_manager.handle_modifiers(clients, modifiers);
    }

    pub fn handle_pointer_motion(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        time_msec: u32,
        dx: f64,
        dy: f64,
    ) {
        self.seat_manager
            .handle_pointer_motion(clients, &self.surface_manager, time_msec, dx, dy);
    }

    pub fn handle_pointer_absolute(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        time_msec: u32,
        x: f64,
        y: f64,
    ) {
        self.seat_manager
            .handle_pointer_absolute(clients, &self.surface_manager, time_msec, x, y);
    }

    /// Recompute pointer enter/leave from current coordinates and stacking.
    ///
    /// Call after client dispatch or when mapping changes under a stationary cursor.
    pub fn refresh_pointer_focus(&mut self, clients: &mut HashMap<ClientId, ClientConnection>) {
        self.seat_manager.update_pointer_focus_and_motion(
            clients,
            &self.surface_manager,
            0,
            false,
        );
    }

    pub fn set_output_geometry(&mut self, width: u32, height: u32) {
        self.seat_manager.set_output_geometry(width, height);
    }

    pub fn pointer_position(&self) -> (f64, f64) {
        self.seat_manager.pointer_position()
    }

    pub fn active_cursor(&self) -> Option<ActiveCursor> {
        self.seat_manager.active_cursor()
    }

    pub fn handle_pointer_button(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        time_msec: u32,
        button: u32,
        pressed: bool,
    ) {
        self.seat_manager.handle_pointer_button(
            clients,
            &self.surface_manager,
            time_msec,
            button,
            pressed,
        );
        if pressed && let Some((client_id, surface)) = self.seat_manager.focused_keyboard_surface()
        {
            self.on_surface_focused(client_id, surface);
        }
    }

    pub fn handle_pointer_axis(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        time_msec: u32,
        axis: u32,
        value: f32,
    ) {
        self.seat_manager
            .handle_pointer_axis(clients, time_msec, axis, value);
    }

    pub fn handle_touch_down(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        time_msec: u32,
        touch_id: i32,
        x: f64,
        y: f64,
    ) {
        self.seat_manager.handle_touch_down(
            clients,
            &self.surface_manager,
            time_msec,
            touch_id,
            x,
            y,
        );
    }

    pub fn handle_touch_up(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        time_msec: u32,
        touch_id: i32,
    ) {
        self.seat_manager
            .handle_touch_up(clients, time_msec, touch_id);
    }

    pub fn handle_touch_motion(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        time_msec: u32,
        touch_id: i32,
        x: f64,
        y: f64,
    ) {
        self.seat_manager.handle_touch_motion(
            clients,
            &self.surface_manager,
            time_msec,
            touch_id,
            x,
            y,
        );
    }

    pub fn handle_touch_frame(&mut self, clients: &mut HashMap<ClientId, ClientConnection>) {
        self.seat_manager.handle_touch_frame(clients);
    }

    pub fn handle_touch_cancel(&mut self, clients: &mut HashMap<ClientId, ClientConnection>) {
        self.seat_manager.handle_touch_cancel(clients);
    }

    /// Drive an active drag's motion for tests / compositor input.
    pub fn drag_motion(
        &mut self,
        client_id: ClientId,
        clients: &mut HashMap<ClientId, ClientConnection>,
        time_msec: u32,
        x: f64,
        y: f64,
    ) {
        let target = self
            .surface_manager
            .global_pointer_target(Some(client_id), x, y)
            .filter(|(owner, _)| *owner == client_id)
            .map(|(_, surface)| surface);
        let Some(client) = clients.get_mut(&client_id) else {
            return;
        };
        let (registry, writer) = client.registry_and_writer_mut();
        self.data_device_manager.drag_motion(
            client_id, time_msec, x as f32, y as f32, target, registry, writer,
        );
    }

    /// Complete an active drag with a drop for tests / compositor input.
    pub fn drag_drop(
        &mut self,
        client_id: ClientId,
        clients: &mut HashMap<ClientId, ClientConnection>,
    ) {
        let Some(client) = clients.get_mut(&client_id) else {
            return;
        };
        self.data_device_manager
            .drag_drop(client_id, client.writer_mut());
    }

    pub fn remove_client(&mut self, client_id: ClientId) {
        self.shm_manager.delete_client(client_id);
        self.dmabuf_manager.delete_client(client_id);
        self.surface_manager.delete_client(client_id);
        self.seat_manager.remove_client(client_id);
        self.output_manager.remove_client(client_id);
        self.data_device_manager.remove_client(client_id);
        self.xdg_manager.delete_client(client_id);
        self.window_manager.delete_client(client_id);
        self.pending_frame_callbacks
            .retain(|(owner, _)| *owner != client_id);
        self.pending_presentation_feedbacks
            .retain(|pending| pending.client_id != client_id);
        self.surface_updates.retain(|update| match update {
            SurfaceUpdate::Frame(frame) | SurfaceUpdate::Cursor(frame) => {
                frame.client_id != client_id
            }
            SurfaceUpdate::Unmapped {
                client_id: owner, ..
            } => *owner != client_id,
        });
    }

    pub fn take_surface_updates(&mut self) -> impl Iterator<Item = SurfaceUpdate> + '_ {
        self.surface_updates.drain(..)
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

    /// Completes deferred `wl_surface.frame` callbacks after presentation.
    pub fn complete_frame_callbacks(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        time_msec: u32,
    ) {
        while let Some((client_id, callback)) = self.pending_frame_callbacks.pop_front() {
            let Some(client) = clients.get_mut(&client_id) else {
                continue;
            };
            let (registry, writer) = client.registry_and_writer_mut();
            writer.wl_callback_done(callback).callback_data(time_msec);
            registry.free_object(callback, writer);
        }
    }

    /// Completes pending `wp_presentation_feedback` objects after a DRM page-flip.
    pub fn complete_presentation_feedbacks(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
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
    pub fn update_primary_output(
        &mut self,
        info: OutputInfo,
        clients: &mut HashMap<ClientId, ClientConnection>,
    ) {
        if let Some(global_id) = self.output_manager.primary_global_id() {
            self.output_manager.update_output(global_id, info, clients);
        }
    }

    pub fn add_output<'connection>(
        &mut self,
        info: OutputInfo,
        client_connections: impl Iterator<Item = &'connection mut ClientConnection>,
    ) -> anyhow::Result<GlobalId> {
        self.output_manager
            .add_output(info, &mut self.globals, client_connections)
    }

    pub fn remove_output<'connection>(
        &mut self,
        name: &str,
        client_connections: impl Iterator<Item = &'connection mut ClientConnection>,
    ) -> anyhow::Result<()> {
        self.output_manager
            .remove_output(name, &mut self.globals, client_connections)
    }

    pub fn outputs(&self) -> impl Iterator<Item = &OutputInfo> {
        self.output_manager.outputs()
    }

    pub fn activate_main_seat<'connection>(
        &mut self,
        seat_name: String,
        client_connections: impl Iterator<Item = &'connection mut ClientConnection>,
    ) -> anyhow::Result<()> {
        self.seat_manager
            .add_main_seat(seat_name, &mut self.globals, client_connections)?;
        Ok(())
    }

    pub fn set_window(
        &mut self,
        id: Option<u32>,
        geometry: WindowGeometryUpdate,
        user_initiated: bool,
        clients: &mut HashMap<ClientId, ClientConnection>,
    ) -> Result<Vec<RendererLayoutSync>, WindowError> {
        let changes = self.window_manager.set_window(
            id,
            geometry,
            user_initiated,
            &self.surface_manager,
            &mut self.xdg_manager,
        )?;
        Ok(self.apply_geometry_changes(changes, clients))
    }

    pub fn add_window_rule(&mut self, rule: WindowRule) {
        self.window_manager.add_rule(rule);
    }

    pub fn clear_window_rules(&mut self) {
        self.window_manager.clear_rules();
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

    pub fn drain_pending_geometry(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
    ) -> Vec<RendererLayoutSync> {
        if self.pending_geometry_changes.is_empty() {
            return Vec::new();
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
        self.window_manager
            .set_toplevel_title(client_id, toplevel, title);
    }

    pub(crate) fn on_surface_focused(&mut self, client_id: ClientId, wl_surface: ObjectId) {
        self.window_manager
            .set_focus_from_surface(client_id, wl_surface);
    }

    pub(crate) fn flush_pending_window_configures(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
    ) {
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
        let Ok(serial) = self
            .xdg_manager
            .send_configure_serial(client_id, xdg_surface_id)
        else {
            return;
        };
        let (width, height) = self
            .xdg_manager
            .toplevel_configure_size(client_id, toplevel_id)
            .unwrap_or((
                window_manager::DEFAULT_WINDOW_WIDTH,
                window_manager::DEFAULT_WINDOW_HEIGHT,
            ));
        writer
            .xdg_toplevel_configure(toplevel_id)
            .width(width)
            .height(height)
            .states(&[]);
        writer.xdg_surface_configure(xdg_surface_id).serial(serial);
    }

    fn apply_geometry_changes(
        &mut self,
        changes: Vec<WindowGeometryChange>,
        clients: &mut HashMap<ClientId, ClientConnection>,
    ) -> Vec<RendererLayoutSync> {
        let mut renderer_syncs = Vec::new();
        for change in changes {
            if let Some((x, y)) = change.position {
                let _ = self.surface_manager.set_surface_layout(
                    change.client_id,
                    change.wl_surface,
                    x,
                    y,
                );
                renderer_syncs.push(RendererLayoutSync {
                    owner_id: change.client_id.get(),
                    surface_id: change.wl_surface.get(),
                    x,
                    y,
                });
            }
        }
        self.flush_pending_window_configures(clients);
        renderer_syncs
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

pub fn create_wayland_display(socket_path: Option<String>) -> anyhow::Result<Wayland> {
    if let Some(socket_path) = socket_path {
        Wayland::new(socket_path).context("Failed to create Wayland display at given socket path")
    } else {
        let xdg_runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .context("XDG_RUNTIME_DIR not set. Set the socket path manually using --socket-path")?;
        for i in 0..10 {
            let socket_path = format!("{xdg_runtime_dir}/wayland-{i}");
            if let Ok(wayland) = Wayland::new(socket_path) {
                return Ok(wayland);
            }
        }
        anyhow::bail!("Failed to create Wayland display");
    }
}

type GlobalId = u32;

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
        globals.register_version(InterfaceIndex::XdgWmBase, 1, [].into_iter());
        // Stable linux-dmabuf keeps the zwp_ interface name; advertise v4 for
        // format/modifier events, create_immed, and feedback format_table.
        globals.register_version(InterfaceIndex::ZwpLinuxDmabufV1, 4, [].into_iter());
        globals.register_version(InterfaceIndex::WpPresentation, 2, [].into_iter());
        globals.register_version(InterfaceIndex::WpViewporter, 1, [].into_iter());
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
