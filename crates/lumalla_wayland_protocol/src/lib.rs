use anyhow::Context;
use log::{debug, error};
use std::{
    fs, io,
    num::NonZeroU32,
    ops::Deref,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::net::UnixListener,
    },
    path::{Path, PathBuf},
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
    socket_path: PathBuf,
}

impl Wayland {
    pub fn new(socket_path: PathBuf) -> anyhow::Result<Self> {
        if socket_path.exists() {
            anyhow::bail!("Wayland socket already exists: {socket_path:?}");
        }
        let listener = UnixListener::bind(&socket_path).context("Failed to bind to socket")?;
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

    pub fn as_raw_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    /// Allocate the next client id (used when accepting via io_uring).
    pub fn allocate_client_id(&mut self) -> Option<ClientId> {
        let client_id = self.next_client_id;
        let next = NonZeroU32::new(self.next_client_id.get() + 1)?;
        self.next_client_id = ClientId::new(next);
        Some(client_id)
    }

    /// Create a client from an accepted connection fd.
    pub fn client_from_accepted_fd(&mut self, fd: RawFd) -> Option<ClientConnection> {
        // Take ownership immediately so the fd is closed on every failure path.
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let Some(client_id) = self.allocate_client_id() else {
            error!("Failed to allocate Wayland client id; dropping accepted connection");
            return None;
        };
        match ClientConnection::from_accepted_fd(owned, client_id) {
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

    pub fn next_client(&mut self) -> Option<ClientConnection> {
        match self.listener.accept() {
            Ok((stream, _addr)) => {
                let client_id = self.allocate_client_id()?;
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
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => None,
            Err(e) => {
                error!("Failed to accept client: {}", e);
                None
            }
        }
    }

    pub fn socket_path(&self) -> &Path {
        self.socket_path.as_path()
    }
}

impl Drop for Wayland {
    fn drop(&mut self) {
        if self.socket_path.exists() {
            if let Err(e) = fs::remove_file(&self.socket_path) {
                error!("Failed to remove socket file: {}", e);
            }
        }
    }
}
