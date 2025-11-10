use std::{ffi::CStr, os::fd::RawFd, ptr::NonNull};

use log::warn;
use lumalla_shared::{Comms, SeatMessage};

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code,
    clippy::all
)]
mod bindings {
    include!(concat!(env!("OUT_DIR"), "/libseat_bindings.rs"));
}

/// Safe wrapper around libseat
pub struct LibSeat {
    seat: NonNull<bindings::libseat>,
    // Need to keep comms alive, since libseat's userdata is a pointer to it
    _comms: Box<Comms>,
}

impl LibSeat {
    /// Open a new seat
    pub fn new(comms: Comms) -> anyhow::Result<Self> {
        let mut comms = Box::new(comms);
        let comms_ptr = comms.as_mut() as *mut Comms as *mut std::ffi::c_void;

        unsafe extern "C" fn enable_seat_callback(
            _seat: *mut bindings::libseat,
            userdata: *mut std::ffi::c_void,
        ) {
            unsafe {
                let comms = &mut *(userdata as *mut Comms);
                comms.seat(SeatMessage::SeatEnabled);
            }
        }

        unsafe extern "C" fn disable_seat_callback(
            _seat: *mut bindings::libseat,
            userdata: *mut std::ffi::c_void,
        ) {
            unsafe {
                let comms = &mut *(userdata as *mut Comms);
                comms.seat(SeatMessage::SeatDisabled);
            }
        }

        let listener = bindings::libseat_seat_listener {
            enable_seat: Some(enable_seat_callback),
            disable_seat: Some(disable_seat_callback),
        };

        let seat = unsafe { bindings::libseat_open_seat(&listener, comms_ptr) };

        if seat.is_null() {
            anyhow::bail!("Failed to open seat");
        }

        Ok(Self {
            seat: unsafe { NonNull::new_unchecked(seat) },
            _comms: comms,
        })
    }

    /// Get the file descriptor for the seat
    pub fn fd(&self) -> anyhow::Result<RawFd> {
        let fd = unsafe { bindings::libseat_get_fd(self.seat.as_ptr()) };
        if fd < 0 {
            anyhow::bail!("Failed to get seat file descriptor");
        }
        Ok(fd)
    }

    /// Dispatch all available seat events
    pub fn dispatch(&self) -> anyhow::Result<()> {
        let result = unsafe { bindings::libseat_dispatch(self.seat.as_ptr(), 0) };
        if result < 0 {
            anyhow::bail!("Failed to dispatch seat events");
        }
        if result == 0 {
            warn!("No seat events to dispatch, but requested to dispatch anyway");
        }
        Ok(())
    }

    /// Get the seat name
    pub fn seat_name(&self) -> anyhow::Result<String> {
        let name_ptr = unsafe { bindings::libseat_seat_name(self.seat.as_ptr()) };
        if name_ptr.is_null() {
            anyhow::bail!("Failed to get seat name");
        }
        let c_str = unsafe { CStr::from_ptr(name_ptr) };
        Ok(c_str.to_string_lossy().into_owned())
    }

    /// Disable the seat
    pub fn disable_seat(&self) -> anyhow::Result<()> {
        let result = unsafe { bindings::libseat_disable_seat(self.seat.as_ptr()) };
        if result < 0 {
            anyhow::bail!("Failed to disable seat");
        }
        Ok(())
    }

    /// Open a device
    pub fn open_device(&self, path: &CStr) -> anyhow::Result<RawFd> {
        let mut fd: RawFd = 0;
        let result = unsafe {
            bindings::libseat_open_device(self.seat.as_ptr(), path.as_ptr(), &mut fd as *mut RawFd)
        };
        if result < 0 {
            anyhow::bail!("Failed to open device: {}", path.to_string_lossy());
        }
        Ok(fd)
    }

    /// Close a device
    pub fn close_device(&self, fd: RawFd) -> anyhow::Result<()> {
        let result = unsafe { bindings::libseat_close_device(self.seat.as_ptr(), fd) };
        if result < 0 {
            anyhow::bail!("Failed to close device");
        }
        Ok(())
    }

    /// Switch session
    pub fn switch_session(&self, session: i32) -> anyhow::Result<()> {
        let result = unsafe { bindings::libseat_switch_session(self.seat.as_ptr(), session) };
        if result < 0 {
            anyhow::bail!("Failed to switch session");
        }
        Ok(())
    }
}

impl Drop for LibSeat {
    fn drop(&mut self) {
        unsafe {
            bindings::libseat_close_seat(self.seat.as_ptr());
        }
    }
}
