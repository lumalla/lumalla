use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;

use anyhow::Context;
use log::{debug, warn};
use lumalla_shared::Comms;

use crate::libseat::LibSeat;

mod libseat;

pub use libseat::SeatDevice;

pub struct SeatState {
    main_seat: LibSeat,
    /// Maps fds opened via libseat to their device ids (needed for libinput closes).
    devices_by_fd: RefCell<HashMap<RawFd, i32>>,
}

impl SeatState {
    pub fn new(comms: Comms) -> anyhow::Result<Self> {
        let seat = LibSeat::new(comms).context("Failed to create seat")?;
        Ok(Self {
            main_seat: seat,
            devices_by_fd: RefCell::new(HashMap::new()),
        })
    }

    pub fn fd(&self) -> RawFd {
        self.main_seat.fd()
    }

    pub fn dispatch(&mut self) -> anyhow::Result<()> {
        self.main_seat
            .dispatch()
            .context("Failed to dispatch libseat events")
    }

    pub fn seat_name(&self) -> anyhow::Result<String> {
        self.main_seat.seat_name()
    }

    pub fn is_enabled(&self) -> bool {
        self.main_seat.is_enabled()
    }

    /// Open the device from the given path via libseat.
    pub fn open_device(&self, path: &Path) -> anyhow::Result<SeatDevice> {
        debug!("Opening device in main seat: {}", path.display());
        let path_str = path.to_str().context("Device path is not valid UTF-8")?;
        let c_path = CString::new(path_str).context("Device path contains null byte")?;
        let device = self.main_seat.open_device(&c_path)?;
        self.devices_by_fd
            .borrow_mut()
            .insert(device.fd().as_raw_fd(), device.device_id());
        Ok(device)
    }

    /// Close a device previously opened with [`Self::open_device`].
    pub fn close_device(&self, device: SeatDevice) -> anyhow::Result<()> {
        let fd = device.fd().as_raw_fd();
        let device_id = device.device_id();
        debug!("Closing device in main seat: device_id={device_id}");
        self.devices_by_fd.borrow_mut().remove(&fd);
        self.main_seat.close_device(device)
    }

    /// Release a libseat device by fd (used by libinput `close_restricted`).
    ///
    /// Always closes the local fd. `libseat_close_device` may fail after the seat
    /// has already been disabled; that is logged and ignored.
    pub fn close_device_fd(&self, fd: RawFd) {
        // Drop the RefMut before calling into libseat: ReleaseDevice may re-enter
        // libseat/libinput and try to borrow this map again.
        let device_id = self.devices_by_fd.borrow_mut().remove(&fd);
        if let Some(device_id) = device_id {
            debug!("Closing libseat device via fd: device_id={device_id} fd={fd}");
            if let Err(err) = self.main_seat.close_device_by_id(device_id) {
                warn!("libseat_close_device({device_id}) failed (fd={fd}): {err:#}");
            }
        }
        unsafe {
            libc::close(fd);
        }
    }

    /// Switch to the given VT/session (1-based).
    pub fn switch_session(&self, session: i32) -> anyhow::Result<()> {
        debug!("Switching seat session to {session}");
        self.main_seat.switch_session(session)
    }
}

impl mio::event::Source for SeatState {
    fn register(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> io::Result<()> {
        self.main_seat.register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> io::Result<()> {
        self.main_seat.reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &mio::Registry) -> io::Result<()> {
        self.main_seat.deregister(registry)
    }
}
