use std::{
    collections::HashMap,
    sync::{Arc, mpsc},
};

use anyhow::Context;
use log::{debug, error, info, warn};
use lumalla_shared::{Comms, DisplayMessage, GlobalArgs, MESSAGE_CHANNEL_TOKEN, MessageRunner};
use lumalla_wayland_protocol::{
    ClientConnection, ClientId, ObjectId, Wayland, registry::InterfaceIndex,
};
use mio::{Interest, Poll, Token};

use crate::{seat::SeatManager, shm::ShmManager};

mod protocols;
mod seat;
mod shm;

pub const WAYLAND_SOCKET_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 1);
pub const CLIENT_TOKEN_START: Token = Token(WAYLAND_SOCKET_TOKEN.0 + 1);

pub struct DisplayState {
    _comms: Comms,
    event_loop: Poll,
    channel: mpsc::Receiver<DisplayMessage>,
    shutting_down: bool,
    args: Arc<GlobalArgs>,
    globals: Globals,
    _surfaces: HashMap<(ClientId, ObjectId), SurfaceState>,
    shm_manager: ShmManager,
    seat_manager: SeatManager,
}

impl DisplayState {
    fn handle_message<'connection>(
        &mut self,
        message: DisplayMessage,
        connected_clients: impl Iterator<Item = &'connection mut ClientConnection>,
    ) -> anyhow::Result<()> {
        match message {
            DisplayMessage::Shutdown => {
                self.shutting_down = true;
            }
            DisplayMessage::ActivateSeat(seat_name) => {
                self.seat_manager
                    .add_seat(seat_name, &mut self.globals, connected_clients);
            }
            message => {
                warn!("Message not handled: {message:?}");
            }
        }

        Ok(())
    }
}

impl MessageRunner for DisplayState {
    type Message = DisplayMessage;

    fn new(
        comms: Comms,
        event_loop: Poll,
        channel: mpsc::Receiver<Self::Message>,
        args: Arc<GlobalArgs>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            _comms: comms,
            event_loop,
            channel,
            shutting_down: false,
            args,
            globals: Globals::default(),
            _surfaces: HashMap::new(),
            shm_manager: ShmManager::default(),
            seat_manager: SeatManager::default(),
        })
    }

    fn run(&mut self) -> anyhow::Result<()> {
        let mut wayland = self.create_wayland_display()?;
        info!(
            "Created wayland display socket at: {}",
            wayland.socket_path()
        );
        self.event_loop
            .registry()
            .register(&mut wayland, WAYLAND_SOCKET_TOKEN, Interest::READABLE)
            .context("Unable to listen on wayland display socket")?;

        let mut connected_clients = HashMap::<ClientId, ClientConnection>::new();
        let mut events = mio::Events::with_capacity(128);
        loop {
            if let Err(err) = self.event_loop.poll(&mut events, None) {
                error!("Unable to poll event loop: {err}");
            }

            for event in events.iter() {
                match event.token() {
                    MESSAGE_CHANNEL_TOKEN => {
                        while let Ok(msg) = self.channel.try_recv() {
                            if let Err(err) =
                                self.handle_message(msg, connected_clients.values_mut())
                            {
                                error!("Unable to handle message: {err}");
                            }
                        }
                    }
                    WAYLAND_SOCKET_TOKEN => {
                        if let Some(mut client) = wayland.next_client() {
                            let client_id = client.client_id();
                            info!("New client connected with id {}", client_id);
                            if let Err(err) = self.event_loop.registry().register(
                                &mut client,
                                Token(CLIENT_TOKEN_START.0 + client_id as usize),
                                Interest::READABLE,
                            ) {
                                error!(
                                    "Unable to listen on client socket with client id {}: {err}",
                                    client_id
                                );
                            } else {
                                connected_clients.insert(client.client_id(), client);
                            }
                        }
                    }
                    token => {
                        let client_id: ClientId = (token.0 - CLIENT_TOKEN_START.0) as ClientId;
                        if let Some(client) = connected_clients.get_mut(&client_id) {
                            if let Err(err) = client.handle_messages(self) {
                                error!("Unable to handle messages for client {}: {err}", client_id);
                                // Flush any remaining messages
                                if let Err(err) = client.flush() {
                                    error!("Unable to flush client {}: {err}", client_id);
                                }
                                if let Err(err) = self.event_loop.registry().deregister(client) {
                                    error!("Unable to deregister client {}: {err}", client_id);
                                }
                                connected_clients.remove(&client_id);
                            }
                        } else {
                            debug!("Received message for unknown client {}", client_id);
                        }
                    }
                }
            }

            let mut clients_to_remove = Vec::new();
            for (&client_id, client) in connected_clients.iter_mut() {
                if let Err(err) = client.flush() {
                    error!("Unable to flush client {}: {err}", client_id);
                    if let Err(err) = self.event_loop.registry().deregister(client) {
                        error!("Unable to deregister client {}: {err}", client_id);
                    }
                    clients_to_remove.push(client_id);
                }
            }
            for client_id in clients_to_remove {
                connected_clients.remove(&client_id);
            }

            if self.shutting_down {
                break;
            }
        }

        Ok(())
    }
}

impl DisplayState {
    fn create_wayland_display(&self) -> anyhow::Result<Wayland> {
        if let Some(socket_path) = &self.args.socket_path {
            Wayland::new(socket_path.clone())
                .context("Failed to create Wayland display at given socket path")
        } else {
            let xdg_runtime_dir = std::env::var("XDG_RUNTIME_DIR").context(
                "XDG_RUNTIME_DIR not set. Set the socket path manually using --socket-path",
            )?;
            for i in 0..10 {
                let socket_path = format!("{xdg_runtime_dir}/wayland-{i}");
                if let Ok(wayland) = Wayland::new(socket_path) {
                    return Ok(wayland);
                }
            }
            anyhow::bail!("Failed to create Wayland display");
        }
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
    _name: &'static str,
    _version: u32,
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
                _name: interface_index.interface_name(),
                _version: interface_index.interface_version(),
                interface_index,
            },
        );
        for client in client_connections {
            client.broadcast_global(id, interface_index);
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

struct SurfaceState {}
