use std::{
    ffi::CStr,
    io,
    os::fd::{FromRawFd, OwnedFd, RawFd},
    ptr::NonNull,
};

use log::error;
use lumalla_shared::{Comms, MainMessage};
use mio::{Interest, Registry, Token, event::Source, unix::SourceFd};

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code
)]
mod bindings {
    use std::ffi::{c_char, c_int, c_void};

    #[repr(C)]
    pub struct libseat {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct libseat_seat_listener {
        pub enable_seat: Option<unsafe extern "C" fn(*mut libseat, *mut c_void)>,
        pub disable_seat: Option<unsafe extern "C" fn(*mut libseat, *mut c_void)>,
    }

    unsafe extern "C" {
        pub fn libseat_open_seat(
            listener: *const libseat_seat_listener,
            userdata: *mut c_void,
        ) -> *mut libseat;
        pub fn libseat_close_seat(seat: *mut libseat) -> c_int;
        pub fn libseat_get_fd(seat: *mut libseat) -> c_int;
        pub fn libseat_dispatch(seat: *mut libseat, timeout: c_int) -> c_int;
        pub fn libseat_seat_name(seat: *mut libseat) -> *const c_char;
        pub fn libseat_disable_seat(seat: *mut libseat) -> c_int;
        pub fn libseat_open_device(
            seat: *mut libseat,
            path: *const c_char,
            fd: *mut c_int,
        ) -> c_int;
        pub fn libseat_close_device(seat: *mut libseat, device_id: c_int) -> c_int;
        pub fn libseat_switch_session(seat: *mut libseat, session: c_int) -> c_int;
    }
}

pub struct SeatDevice {
    device_id: i32,
    fd: OwnedFd,
}

/// Safe wrapper around libseat
pub struct LibSeat {
    seat: NonNull<bindings::libseat>,
    fd: RawFd,
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
                comms.main(MainMessage::SeatEnabled);
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
                comms.main(MainMessage::SeatDisabled);
            }
        }

        static LISTENER: bindings::libseat_seat_listener = bindings::libseat_seat_listener {
            enable_seat: Some(enable_seat_callback),
            disable_seat: Some(disable_seat_callback),
        };

        let seat = unsafe { bindings::libseat_open_seat(&LISTENER, comms_ptr) };
        let Some(seat) = NonNull::new(seat) else {
            let err = std::io::Error::last_os_error();
            anyhow::bail!("Failed to open seat: {}", err);
        };
        let fd = unsafe { bindings::libseat_get_fd(seat.as_ptr()) };
        if fd < 0 {
            anyhow::bail!("Failed to get seat file descriptor");
        }
        Ok(Self {
            seat,
            fd,
            _comms: comms,
        })
    }

    /// Get the file descriptor for the seat
    pub fn fd(&self) -> RawFd {
        self.fd
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

    /// Open a device
    pub fn open_device(&self, path: &CStr) -> anyhow::Result<SeatDevice> {
        let mut fd: RawFd = 0;
        let device_id = unsafe {
            bindings::libseat_open_device(self.seat.as_ptr(), path.as_ptr(), &mut fd as *mut RawFd)
        };

        if device_id >= 0 && fd > 0 {
            return Ok(SeatDevice {
                device_id,
                fd: unsafe { OwnedFd::from_raw_fd(fd) },
            });
        }

        let err = std::io::Error::last_os_error();
        anyhow::bail!("Unable to open device {}: {}", path.to_string_lossy(), err)
    }

    /// Close a device by its device_id (returned from open_device)
    pub fn close_device(&self, device: SeatDevice) -> anyhow::Result<()> {
        let result =
            unsafe { bindings::libseat_close_device(self.seat.as_ptr(), device.device_id) };
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

impl Source for LibSeat {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        SourceFd(&self.fd).register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        SourceFd(&self.fd).reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        SourceFd(&self.fd).deregister(registry)
    }
}
