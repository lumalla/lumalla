//! `org.gnome.Mutter.ServiceChannel` — private Wayland connections for portal backends.

use std::{
    collections::HashMap,
    os::fd::{FromRawFd, OwnedFd},
};

use log::debug;
use lumalla_shared::{Comms, MainMessage};
use zbus::{
    fdo, interface,
    zvariant::{OwnedFd as ZbusOwnedFd, OwnedValue},
};

/// Mutter `ServiceClientType` values used by portal backends.
const SERVICE_CLIENT_PORTAL_BACKEND: u32 = 1;
const SERVICE_CLIENT_FILECHOOSER_PORTAL_BACKEND: u32 = 2;
const SERVICE_CLIENT_GLOBAL_SHORTCUTS_PORTAL_BACKEND: u32 = 3;

/// Opens socketpair-backed Wayland connections for GNOME portal service clients.
#[derive(Clone)]
pub(crate) struct ServiceChannel {
    comms: Comms,
}

impl ServiceChannel {
    pub(crate) fn new(comms: Comms) -> Self {
        Self { comms }
    }

    fn open_connection(&self) -> fdo::Result<ZbusOwnedFd> {
        let mut fds = [0; 2];
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        if rc < 0 {
            return Err(fdo::Error::Failed(format!(
                "socketpair failed: {}",
                std::io::Error::last_os_error()
            )));
        }

        let compositor_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let client_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

        match self
            .comms
            .main_sender()
            .send(MainMessage::InjectWaylandClient {
                fd: compositor_fd,
            }) {
            Ok(()) => {
                debug!("Injected Wayland service client via ServiceChannel");
                Ok(ZbusOwnedFd::from(client_fd))
            }
            Err(_) => Err(fdo::Error::Failed(String::from(
                "failed to deliver Wayland client fd to compositor",
            ))),
        }
    }
}

#[interface(name = "org.gnome.Mutter.ServiceChannel")]
impl ServiceChannel {
    /// Open a Wayland connection for a known portal service client type.
    fn open_wayland_service_connection(
        &self,
        service_client_type: u32,
    ) -> fdo::Result<ZbusOwnedFd> {
        match service_client_type {
            SERVICE_CLIENT_PORTAL_BACKEND
            | SERVICE_CLIENT_FILECHOOSER_PORTAL_BACKEND
            | SERVICE_CLIENT_GLOBAL_SHORTCUTS_PORTAL_BACKEND => self.open_connection(),
            other => Err(fdo::Error::InvalidArgs(format!(
                "unknown service_client_type: {other}"
            ))),
        }
    }

    /// Open a Wayland connection with optional metadata (e.g. `window-tag`).
    ///
    /// Options are accepted for API compatibility; Lumalla currently ignores them.
    fn open_wayland_connection(
        &self,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<ZbusOwnedFd> {
        self.open_connection()
    }
}
