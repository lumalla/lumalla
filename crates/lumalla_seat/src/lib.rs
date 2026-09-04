use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;

use anyhow::Context;
use log::{debug, info, warn};
use lumalla_shared::{Comms, MainMessage};

use crate::libseat::LibSeat;

mod libseat;

pub use libseat::SeatDevice;

/// Session device-open backend. Libseat requires an active TTY/session; without one
/// the compositor runs in headless mode (no privileged device opens).
enum SeatBackend {
    Libseat(LibSeat),
    /// No libseat session: Wayland and injected input still work; DRM/libinput device
    /// opens via the seat are unavailable.
    Headless { seat_name: String },
}

pub struct SeatState {
    backend: SeatBackend,
    /// Maps fds opened via libseat to their device ids (needed for libinput closes).
    devices_by_fd: RefCell<HashMap<RawFd, i32>>,
}

impl SeatState {
    /// Open a libseat session, or fall back to a headless backend if that fails
    /// (e.g. no TTY / no logind seat).
    ///
    /// When `force_headless` is true, skip libseat entirely (useful when nested under
    /// another compositor where the seat opens but never enables).
    ///
    /// Headless mode posts [`MainMessage::MainSeatEnabled`] immediately so the app
    /// can activate the Wayland seat and become Ready without DRM/libinput devices.
    pub fn new(comms: Comms, force_headless: bool) -> anyhow::Result<Self> {
        if force_headless {
            warn!("Running without a session backend (--headless)");
            return Ok(Self::new_headless(comms));
        }

        match LibSeat::new(comms.clone()) {
            Ok(seat) => {
                info!("Opened libseat session");
                Ok(Self {
                    backend: SeatBackend::Libseat(seat),
                    devices_by_fd: RefCell::new(HashMap::new()),
                })
            }
            Err(err) => {
                warn!(
                    "libseat unavailable ({err:#}); continuing without a session backend \
                     (no DRM/libinput device opens)"
                );
                Ok(Self::new_headless(comms))
            }
        }
    }

    fn new_headless(comms: Comms) -> Self {
        comms.main(MainMessage::MainSeatEnabled);
        Self {
            backend: SeatBackend::Headless {
                seat_name: "seat0".to_string(),
            },
            devices_by_fd: RefCell::new(HashMap::new()),
        }
    }

    /// Whether this backend can open DRM/input devices (libseat only).
    pub fn can_open_devices(&self) -> bool {
        matches!(self.backend, SeatBackend::Libseat(_))
    }

    /// Whether the compositor is running without a libseat session.
    pub fn is_headless(&self) -> bool {
        matches!(self.backend, SeatBackend::Headless { .. })
    }

    /// Libseat event fd to poll, if any.
    pub fn poll_fd(&self) -> Option<RawFd> {
        match &self.backend {
            SeatBackend::Libseat(seat) => Some(seat.fd()),
            SeatBackend::Headless { .. } => None,
        }
    }

    pub fn fd(&self) -> Option<RawFd> {
        self.poll_fd()
    }

    pub fn as_raw_fd(&self) -> Option<RawFd> {
        self.poll_fd()
    }

    pub fn dispatch(&mut self) -> anyhow::Result<()> {
        match &self.backend {
            SeatBackend::Libseat(seat) => seat
                .dispatch()
                .context("Failed to dispatch libseat events"),
            SeatBackend::Headless { .. } => Ok(()),
        }
    }

    pub fn seat_name(&self) -> anyhow::Result<String> {
        match &self.backend {
            SeatBackend::Libseat(seat) => seat.seat_name(),
            SeatBackend::Headless { seat_name } => Ok(seat_name.clone()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        match &self.backend {
            SeatBackend::Libseat(seat) => seat.is_enabled(),
            SeatBackend::Headless { .. } => true,
        }
    }

    /// Open the device from the given path via libseat.
    ///
    /// Fails in headless mode — callers must check [`Self::can_open_devices`] first
    /// when a no-session path is expected.
    pub fn open_device(&self, path: &Path) -> anyhow::Result<SeatDevice> {
        let SeatBackend::Libseat(seat) = &self.backend else {
            anyhow::bail!(
                "Cannot open device {}: no libseat session",
                path.display()
            );
        };
        debug!("Opening device in main seat: {}", path.display());
        let path_str = path.to_str().context("Device path is not valid UTF-8")?;
        let c_path = CString::new(path_str).context("Device path contains null byte")?;
        let device = seat.open_device(&c_path)?;
        self.devices_by_fd
            .borrow_mut()
            .insert(device.fd().as_raw_fd(), device.device_id());
        Ok(device)
    }

    /// Close a device previously opened with [`Self::open_device`].
    pub fn close_device(&self, device: SeatDevice) -> anyhow::Result<()> {
        let SeatBackend::Libseat(seat) = &self.backend else {
            anyhow::bail!("Cannot close seat device: no libseat session");
        };
        let fd = device.fd().as_raw_fd();
        let device_id = device.device_id();
        debug!("Closing device in main seat: device_id={device_id}");
        self.devices_by_fd.borrow_mut().remove(&fd);
        seat.close_device(device)
    }

    /// Release a libseat device by fd (used by libinput `close_restricted`).
    ///
    /// Only acts when this fd is still tracked in [`Self::devices_by_fd`]. A second
    /// close for the same number (or a close after [`Self::close_device`]) must not
    /// call `close(2)`: that fd slot may already belong to an unrelated file such as
    /// a Wayland SHM pool, and double-closing aborts debug builds on `OwnedFd` drop.
    ///
    /// When tracked, closes the local fd even if `libseat_close_device` fails (e.g.
    /// after the seat has already been disabled).
    pub fn close_device_fd(&self, fd: RawFd) {
        // Drop the RefMut before calling into libseat: ReleaseDevice may re-enter
        // libseat/libinput and try to borrow this map again.
        let Some(device_id) = self.devices_by_fd.borrow_mut().remove(&fd) else {
            debug!("Ignoring close for untracked libseat fd={fd}");
            return;
        };
        debug!("Closing libseat device via fd: device_id={device_id} fd={fd}");
        match &self.backend {
            SeatBackend::Libseat(seat) => {
                if let Err(err) = seat.close_device_by_id(device_id) {
                    warn!("libseat_close_device({device_id}) failed (fd={fd}): {err:#}");
                }
            }
            SeatBackend::Headless { .. } => {
                warn!("close_device_fd({fd}) with tracked id={device_id} but no libseat session");
            }
        }
        // libseat releases the device claim but does not close the local fd.
        unsafe {
            libc::close(fd);
        }
    }

    /// Switch to the given VT/session (1-based).
    pub fn switch_session(&self, session: i32) -> anyhow::Result<()> {
        let SeatBackend::Libseat(seat) = &self.backend else {
            anyhow::bail!("Cannot switch session: no libseat session");
        };
        debug!("Switching seat session to {session}");
        seat.switch_session(session)
    }
}
