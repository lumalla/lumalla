//! Device open requests routed through the main thread (libseat).

use std::ffi::{CStr, CString};
use std::os::fd::{IntoRawFd, OwnedFd, RawFd};
use std::sync::mpsc::{self, SyncSender};

use anyhow::Context;
use lumalla_shared::MessageSender;

/// A request to open an input device through the session seat.
pub struct OpenRequest {
    /// Device path to open.
    pub path: String,
    /// Channel used to return the opened file descriptor.
    pub response: SyncSender<anyhow::Result<OwnedFd>>,
}

/// Opens input devices via the compositor main thread.
#[derive(Clone)]
pub struct RestrictedDeviceOpener {
    request_tx: mpsc::Sender<OpenRequest>,
    notify_tx: MessageSender<()>,
}

impl RestrictedDeviceOpener {
    /// Create a new opener that sends requests on `request_tx`.
    pub fn new(request_tx: mpsc::Sender<OpenRequest>, notify_tx: MessageSender<()>) -> Self {
        Self {
            request_tx,
            notify_tx,
        }
    }

    /// Open a device path, blocking until the main thread responds.
    pub fn open(&self, path: &CStr, flags: i32) -> anyhow::Result<RawFd> {
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        self.request_tx
            .send(OpenRequest {
                path: path.to_string_lossy().into_owned(),
                response: response_tx,
            })
            .context("Failed to send restricted device open request")?;
        let _ = self.notify_tx.send(());

        let fd = response_rx
            .recv()
            .context("Main thread dropped restricted open response channel")??;
        let raw_fd = fd.into_raw_fd();
        if flags & libc::O_CLOEXEC == 0 {
            let current = unsafe { libc::fcntl(raw_fd, libc::F_GETFD) };
            if current >= 0 {
                unsafe { libc::fcntl(raw_fd, libc::F_SETFD, current & !libc::FD_CLOEXEC) };
            }
        }
        Ok(raw_fd)
    }

    /// Close a device file descriptor opened through [`Self::open`].
    pub fn close(fd: RawFd) {
        unsafe {
            libc::close(fd);
        }
    }
}
