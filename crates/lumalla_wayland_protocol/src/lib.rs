use anyhow::Context;
use log::{debug, error};
use mio::{event::Source, unix::SourceFd};
use std::{
    fs, io,
    num::NonZeroU32,
    ops::Deref,
    os::{fd::AsRawFd, unix::net::UnixListener},
    path::Path,
};

pub mod buffer;
mod client;
pub mod protocols;
pub mod registry;
pub use client::{ClientConnection, ClientId, Ctx};

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd)]
pub struct ObjectId(NonZeroU32);

impl ObjectId {
    pub const fn new(id: NonZeroU32) -> Self {
        Self(id)
    }

    pub fn get(self) -> u32 {
        self.0.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NewObjectId(ObjectId);

impl NewObjectId {
    pub const fn new(id: ObjectId) -> Self {
        Self(id)
    }
}

impl Deref for NewObjectId {
    type Target = ObjectId;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

type Opcode = u16;

pub struct Wayland {
    listener: UnixListener,
    next_client_id: ClientId,
    socket_path: String,
}

impl Wayland {
    pub fn new(socket_path: String) -> anyhow::Result<Self> {
        // Remove existing socket if it exists
        if Path::new(&socket_path).exists() {
            fs::remove_file(&socket_path).context("Failed to remove existing socket")?;
        }

        // Create Unix domain socket
        let listener = UnixListener::bind(&socket_path).context("Failed to bind to socket")?;

        // Set socket to non-blocking mode
        listener
            .set_nonblocking(true)
            .context("Failed to set socket to non-blocking mode")?;

        Ok(Self {
            listener,
            next_client_id: ClientId::new(
                NonZeroU32::new(1).ok_or(anyhow::anyhow!("Somehow got zero client id"))?,
            ),
            socket_path,
        })
    }

    pub fn next_client(&mut self) -> Option<ClientConnection> {
        match self.listener.accept() {
            Ok((stream, _addr)) => {
                let client_id = self.next_client_id;
                let Some(next_client_id) = NonZeroU32::new(self.next_client_id.get() + 1) else {
                    error!("Failed to increment client ID");
                    return None;
                };
                self.next_client_id = ClientId::new(next_client_id);

                match ClientConnection::new(stream, client_id) {
                    Ok(client) => {
                        debug!("New client connected with ID: {:?}", client_id);
                        Some(client)
                    }
                    Err(e) => {
                        error!("Failed to create client connection: {}", e);
                        None
                    }
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                // No more clients to accept
                None
            }
            Err(e) => {
                error!("Failed to accept client: {}", e);
                None
            }
        }
    }

    pub fn socket_path(&self) -> &str {
        &self.socket_path
    }
}

impl Source for Wayland {
    fn register(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> io::Result<()> {
        SourceFd(&self.listener.as_raw_fd()).register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> io::Result<()> {
        SourceFd(&self.listener.as_raw_fd()).reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &mio::Registry) -> io::Result<()> {
        SourceFd(&self.listener.as_raw_fd()).deregister(registry)
    }
}

impl Drop for Wayland {
    fn drop(&mut self) {
        // Clean up socket file when dropping
        if Path::new(&self.socket_path).exists() {
            if let Err(e) = fs::remove_file(&self.socket_path) {
                error!("Failed to remove socket file: {}", e);
            }
        }
    }
}
