use std::collections::{HashMap, VecDeque};

use anyhow::Context;
use lumalla_shared::Comms;
use lumalla_wayland_protocol::registry::InterfaceIndex;

use crate::{
    data_device::DataDeviceManager, dmabuf::DmabufManager, output::OutputManager,
    seat::SeatManager, shm::ShmManager, surface::SurfaceManager, xdg::XdgManager,
};

mod data_device;
mod dmabuf;
mod protocols;
mod seat;
mod shm;
mod surface;
mod output;
mod xdg;

pub use lumalla_wayland_protocol::{ClientConnection, ClientId, Wayland};
pub use seat::{ActiveCursor, KeyboardModifiers};
pub use output::OutputInfo;
pub use surface::Rectangle;

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
    /// Output-space regions that changed this commit.
    pub damage: Vec<Rectangle>,
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
    surface_updates: VecDeque<SurfaceUpdate>,
    pending_frame_callbacks: VecDeque<(ClientId, lumalla_wayland_protocol::ObjectId)>,
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
            surface_updates: VecDeque::new(),
            pending_frame_callbacks: VecDeque::new(),
        })
    }

    pub fn set_keyboard_keymap(&mut self, keymap: lumalla_shared::KeymapMemfd) {
        self.seat_manager.set_keymap(keymap);
    }

    pub fn set_keyboard_modifiers(&mut self, modifiers: seat::KeyboardModifiers) {
        self.seat_manager.set_modifiers(modifiers);
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
        self.seat_manager.handle_pointer_motion(
            clients,
            &self.surface_manager,
            time_msec,
            dx,
            dy,
        );
    }

    pub fn handle_pointer_absolute(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        time_msec: u32,
        x: f64,
        y: f64,
    ) {
        self.seat_manager.handle_pointer_absolute(
            clients,
            &self.surface_manager,
            time_msec,
            x,
            y,
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
            client_id,
            time_msec,
            x as f32,
            y as f32,
            target,
            registry,
            writer,
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
        self.pending_frame_callbacks
            .retain(|(owner, _)| *owner != client_id);
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
        // Stable linux-dmabuf keeps the zwp_ interface name; advertise v3 for
        // format/modifier events + create_immed without requiring feedback.
        globals.register_version(InterfaceIndex::ZwpLinuxDmabufV1, 3, [].into_iter());
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
