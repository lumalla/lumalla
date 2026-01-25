use std::{ffi::CStr, os::fd::RawFd, ptr::NonNull};

use log::error;
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
            log::debug!("libseat: enable_seat_callback fired");
            if userdata.is_null() {
                error!("enable_seat_callback called with null userdata");
                return;
            }
            unsafe {
                let comms = &mut *(userdata as *mut Comms);
                comms.seat(SeatMessage::SeatEnabled);
            }
        }

        unsafe extern "C" fn disable_seat_callback(
            _seat: *mut bindings::libseat,
            userdata: *mut std::ffi::c_void,
        ) {
            log::debug!("libseat: disable_seat_callback fired");
            if userdata.is_null() {
                error!("disable_seat_callback called with null userdata");
                return;
            }
            unsafe {
                let comms = &mut *(userdata as *mut Comms);
                comms.seat(SeatMessage::SeatDisabled);
            }
        }

        static LISTENER: bindings::libseat_seat_listener = bindings::libseat_seat_listener {
            enable_seat: Some(enable_seat_callback),
            disable_seat: Some(disable_seat_callback),
        };

        let seat = unsafe { bindings::libseat_open_seat(&LISTENER, comms_ptr) };

        if seat.is_null() {
            let err = std::io::Error::last_os_error();
            anyhow::bail!("Failed to open seat: {}", err);
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

    /// Dispatch all available seat events (non-blocking)
    pub fn dispatch(&self) -> anyhow::Result<()> {
        let result = unsafe { bindings::libseat_dispatch(self.seat.as_ptr(), 0) };
        if result < 0 {
            anyhow::bail!("Failed to dispatch seat events");
        }
        Ok(())
    }

    /// Dispatch seat events with a timeout in milliseconds
    pub fn dispatch_timeout(&self, timeout_ms: i32) -> anyhow::Result<i32> {
        let result = unsafe { bindings::libseat_dispatch(self.seat.as_ptr(), timeout_ms) };
        if result < 0 {
            anyhow::bail!("Failed to dispatch seat events");
        }
        Ok(result)
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

    /// Open a device. Returns (device_id, fd).
    /// The device_id is needed to close the device later.
    pub fn open_device(&self, path: &CStr) -> anyhow::Result<(i32, RawFd)> {
        let mut fd: RawFd = 0;
        let device_id = unsafe {
            bindings::libseat_open_device(self.seat.as_ptr(), path.as_ptr(), &mut fd as *mut RawFd)
        };

        if device_id >= 0 {
            return Ok((device_id, fd));
        }

        let err = std::io::Error::last_os_error();
        anyhow::bail!("Unable to open device {}: {}", path.to_string_lossy(), err)
    }

    /// Close a device by its device_id (returned from open_device)
    pub fn close_device(&self, device_id: i32) -> anyhow::Result<()> {
        let result = unsafe { bindings::libseat_close_device(self.seat.as_ptr(), device_id) };
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
