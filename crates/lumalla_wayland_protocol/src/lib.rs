use anyhow::Context;
use log::{debug, error};
use mio::{event::Source, unix::SourceFd};
use std::{
    fs, io,
    os::{fd::AsRawFd, unix::net::UnixListener},
    path::Path,
};

pub mod buffer;
mod client;
pub mod protocols;
pub mod registry;
pub use client::{ClientConnection, ClientId, Ctx};

// TODO: Make the object ID NonZeroU32
pub type ObjectId = u32;
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
            next_client_id: 1,
            socket_path,
        })
    }

    pub fn next_client(&mut self) -> Option<ClientConnection> {
        match self.listener.accept() {
            Ok((stream, _addr)) => {
                let client_id = self.next_client_id;
                self.next_client_id += 1;

                match ClientConnection::new(stream, client_id) {
                    Ok(client) => {
                        debug!("New client connected with ID: {}", client_id);
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
