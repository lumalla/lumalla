use std::collections::HashMap;

use anyhow::Context;
use lumalla_shared::Comms;
use lumalla_wayland_protocol::registry::InterfaceIndex;

use crate::{seat::SeatManager, shm::ShmManager, surface::SurfaceManager};

mod protocols;
mod seat;
mod shm;
mod surface;

pub use lumalla_wayland_protocol::{ClientConnection, ClientId, Wayland};
pub use seat::KeyboardModifiers;

pub struct DisplayMessage;

pub struct DisplayState {
    _comms: Comms,
    globals: Globals,
    surface_manager: SurfaceManager,
    shm_manager: ShmManager,
    seat_manager: SeatManager,
}

impl DisplayState {
    pub fn new(comms: Comms) -> anyhow::Result<Self> {
        Ok(Self {
            _comms: comms,
            globals: Globals::default(),
            surface_manager: SurfaceManager::default(),
            shm_manager: ShmManager::default(),
            seat_manager: SeatManager::default(),
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
    }

    #[allow(dead_code)]
    pub(crate) fn snapshot_shm_buffer(
        &self,
        client_id: ClientId,
        buffer_id: lumalla_wayland_protocol::ObjectId,
    ) -> Result<shm::ShmBufferSnapshot, shm::ShmError> {
        self.shm_manager.snapshot_buffer(client_id, buffer_id)
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
        globals.register(InterfaceIndex::WlCompositor, [].into_iter());
        globals.register(InterfaceIndex::WlShm, [].into_iter());
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
        let id = self.next_id;
        self.next_id += 1;
        self.globals.insert(
            id,
            Global {
                name: interface_index.interface_name(),
                version: interface_index.interface_version(),
                interface_index,
            },
        );
        for client in client_connections {
            client.broadcast_global(id, interface_index, interface_index.interface_version());
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
