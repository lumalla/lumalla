use std::collections::{HashMap, VecDeque};

use anyhow::Context;
use lumalla_shared::Comms;
use lumalla_wayland_protocol::registry::InterfaceIndex;

use crate::{
    output::OutputManager, seat::SeatManager, shm::ShmManager, surface::SurfaceManager,
};

mod protocols;
mod seat;
mod shm;
mod surface;
mod output;

pub use lumalla_wayland_protocol::{ClientConnection, ClientId, Wayland};
pub use seat::KeyboardModifiers;
pub use output::OutputInfo;

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
}

#[derive(Debug)]
pub enum SurfaceUpdate {
    Frame(CommittedFrame),
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
    seat_manager: SeatManager,
    output_manager: OutputManager,
    surface_updates: VecDeque<SurfaceUpdate>,
}

impl DisplayState {
    pub fn new(comms: Comms) -> anyhow::Result<Self> {
        let mut globals = Globals::default();
        let mut output_manager = OutputManager::default();
        output_manager.add_output(OutputInfo::default(), &mut globals, [].into_iter());
        Ok(Self {
            _comms: comms,
            globals,
            surface_manager: SurfaceManager::default(),
            shm_manager: ShmManager::default(),
            seat_manager: SeatManager::default(),
            output_manager,
            surface_updates: VecDeque::new(),
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

    pub fn remove_client(&mut self, client_id: ClientId) {
        self.shm_manager.delete_client(client_id);
        self.surface_manager.delete_client(client_id);
        self.seat_manager.remove_client(client_id);
        self.output_manager.remove_client(client_id);
        self.surface_updates.retain(|update| match update {
            SurfaceUpdate::Frame(frame) => frame.client_id != client_id,
            SurfaceUpdate::Unmapped {
                client_id: owner, ..
            } => *owner != client_id,
        });
    }

    pub fn take_surface_updates(&mut self) -> impl Iterator<Item = SurfaceUpdate> + '_ {
        self.surface_updates.drain(..)
    }

    pub fn add_output<'connection>(
        &mut self,
        info: OutputInfo,
        client_connections: impl Iterator<Item = &'connection mut ClientConnection>,
    ) -> GlobalId {
        self.output_manager
            .add_output(info, &mut self.globals, client_connections)
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

    fn register_version<'connection>(
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

    fn get(&self, id: u32) -> Option<&Global> {
        self.globals.get(&id)
    }
}
