use std::{
    collections::HashMap,
    sync::{Arc, mpsc},
};

use anyhow::Context;
use log::{debug, error, info, warn};
use lumalla_shared::{Comms, DisplayMessage, GlobalArgs, MESSAGE_CHANNEL_TOKEN, MessageRunner};
use lumalla_wayland_protocol::{
    ClientConnection, ClientId, ObjectId, Wayland,
    protocols::wayland::{WL_COMPOSITOR_NAME, WL_COMPOSITOR_VERSION},
    registry::InterfaceIndex,
};
use mio::{Interest, Poll, Token};

mod protocols;

pub const WAYLAND_SOCKET_TOKEN: Token = Token(MESSAGE_CHANNEL_TOKEN.0 + 1);
pub const CLIENT_TOKEN_START: Token = Token(WAYLAND_SOCKET_TOKEN.0 + 1);

pub struct DisplayState {
    _comms: Comms,
    event_loop: Poll,
    channel: mpsc::Receiver<DisplayMessage>,
    shutting_down: bool,
    args: Arc<GlobalArgs>,
    globals: Globals,
    surfaces: HashMap<(ClientId, ObjectId), SurfaceState>,
}

impl DisplayState {
    fn handle_message(&mut self, message: DisplayMessage) -> anyhow::Result<()> {
        match message {
            DisplayMessage::Shutdown => {
                self.shutting_down = true;
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
            surfaces: HashMap::new(),
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
            .context("Unable to listend on wayland display socket")?;

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
                            if let Err(err) = self.handle_message(msg) {
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
                                client.flush();
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

            for (_, client) in connected_clients.iter_mut() {
                client.flush();
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

#[derive(Debug)]
struct Globals {
    globals: HashMap<u32, Global>,
    next_id: u32,
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
        globals.register(InterfaceIndex::WlCompositor);
        globals
    }
}

impl Globals {
    fn register(&mut self, interface_index: InterfaceIndex) -> u32 {
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
