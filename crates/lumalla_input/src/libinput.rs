//! Safe wrapper around libinput (udev backend).

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    io,
    os::fd::RawFd,
    ptr::NonNull,
};

use anyhow::Context;
use log::{debug, info, warn};
use mio::{Interest, Registry, Token, event::Source, unix::SourceFd};

use crate::restricted::RestrictedDeviceOpener;

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code
)]
mod bindings {
    use std::ffi::{c_char, c_int, c_void};

    pub const LIBINPUT_EVENT_DEVICE_ADDED: u32 = 0;
    pub const LIBINPUT_EVENT_DEVICE_REMOVED: u32 = 1;
    pub const LIBINPUT_EVENT_KEYBOARD_KEY: u32 = 200;

    pub const LIBINPUT_KEY_STATE_RELEASED: u32 = 0;
    pub const LIBINPUT_KEY_STATE_PRESSED: u32 = 1;

    pub const KEY_LEFTCTRL: u32 = 29;
    pub const KEY_RIGHTCTRL: u32 = 97;
    pub const KEY_LEFTALT: u32 = 56;
    pub const KEY_RIGHTALT: u32 = 100;
    pub const KEY_LEFTSHIFT: u32 = 42;
    pub const KEY_RIGHTSHIFT: u32 = 54;
    pub const KEY_LEFTMETA: u32 = 125;
    pub const KEY_RIGHTMETA: u32 = 126;
    pub const KEY_BACKSPACE: u32 = 14;
    pub const KEY_F1: u32 = 59;
    pub const KEY_F12: u32 = 70;

    #[repr(C)]
    pub struct libinput {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct libinput_event {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct libinput_event_keyboard {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct udev {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct libinput_interface {
        pub open_restricted:
            Option<unsafe extern "C" fn(*const c_char, c_int, *mut c_void) -> c_int>,
        pub close_restricted: Option<unsafe extern "C" fn(c_int, *mut c_void)>,
    }

    unsafe extern "C" {
        pub fn udev_new() -> *mut udev;
        pub fn udev_unref(udev: *mut udev);

        pub fn libinput_udev_create_context(
            interface: *const libinput_interface,
            user_data: *mut c_void,
            udev: *mut udev,
        ) -> *mut libinput;
        pub fn libinput_udev_assign_seat(libinput: *mut libinput, seat_id: *const c_char) -> c_int;
        pub fn libinput_unref(libinput: *mut libinput);
        pub fn libinput_get_fd(libinput: *mut libinput) -> c_int;
        pub fn libinput_dispatch(libinput: *mut libinput) -> c_int;
        pub fn libinput_get_event(libinput: *mut libinput) -> *mut libinput_event;
        pub fn libinput_event_destroy(event: *mut libinput_event);
        pub fn libinput_event_get_type(event: *mut libinput_event) -> u32;
        pub fn libinput_event_get_keyboard_event(
            event: *mut libinput_event,
        ) -> *mut libinput_event_keyboard;
        pub fn libinput_event_keyboard_get_key(event: *mut libinput_event_keyboard) -> u32;
        pub fn libinput_event_keyboard_get_key_state(
            event: *mut libinput_event_keyboard,
        ) -> u32;
    }
}

/// Wrapper around a libinput udev context.
pub struct LibInput {
    libinput: NonNull<bindings::libinput>,
    udev: NonNull<bindings::udev>,
    fd: RawFd,
    _opener: std::sync::Arc<RestrictedDeviceOpener>,
}

impl LibInput {
    /// Create a libinput context backed by udev.
    pub fn new(opener: std::sync::Arc<RestrictedDeviceOpener>) -> anyhow::Result<Self> {
        let opener_ptr: &'static RestrictedDeviceOpener = Box::leak(Box::new((*opener).clone()));

        unsafe extern "C" fn open_restricted(
            path: *const c_char,
            flags: c_int,
            userdata: *mut c_void,
        ) -> c_int {
            if path.is_null() || userdata.is_null() {
                return -1;
            }
            let opener = &*(userdata as *const RestrictedDeviceOpener);
            let path = unsafe { CStr::from_ptr(path) };
            match opener.open(path, flags) {
                Ok(fd) => fd,
                Err(err) => {
                    warn!("Failed to open restricted device {}: {err:#}", path.to_string_lossy());
                    -1
                }
            }
        }

        unsafe extern "C" fn close_restricted(fd: c_int, _userdata: *mut c_void) {
            RestrictedDeviceOpener::close(fd);
        }

        static INTERFACE: bindings::libinput_interface = bindings::libinput_interface {
            open_restricted: Some(open_restricted),
            close_restricted: Some(close_restricted),
        };

        let udev = unsafe { bindings::udev_new() };
        let Some(udev) = NonNull::new(udev) else {
            anyhow::bail!("Failed to create udev context");
        };

        let libinput = unsafe {
            bindings::libinput_udev_create_context(
                &INTERFACE,
                opener_ptr as *const _ as *mut c_void,
                udev.as_ptr(),
            )
        };
        let Some(libinput) = NonNull::new(libinput) else {
            unsafe { bindings::udev_unref(udev.as_ptr()) };
            anyhow::bail!("Failed to create libinput context");
        };

        let fd = unsafe { bindings::libinput_get_fd(libinput.as_ptr()) };
        if fd < 0 {
            unsafe {
                bindings::libinput_unref(libinput.as_ptr());
                bindings::udev_unref(udev.as_ptr());
            }
            anyhow::bail!("Failed to get libinput file descriptor");
        }

        Ok(Self {
            libinput,
            udev,
            fd,
            _opener: opener,
        })
    }

    /// Assign the udev seat used for device discovery.
    pub fn assign_seat(&self, seat_name: &str) -> anyhow::Result<()> {
        let seat_name_c = CString::new(seat_name).context("Seat name contains null byte")?;
        let result = unsafe {
            bindings::libinput_udev_assign_seat(self.libinput.as_ptr(), seat_name_c.as_ptr())
        };
        if result != 0 {
            anyhow::bail!("Failed to assign libinput seat `{seat_name}`");
        }
        info!("Assigned libinput seat `{seat_name}`");
        Ok(())
    }

    pub(crate) fn fd(&self) -> RawFd {
        self.fd
    }

    pub(crate) fn dispatch(&self) -> anyhow::Result<()> {
        let result = unsafe { bindings::libinput_dispatch(self.libinput.as_ptr()) };
        if result != 0 {
            anyhow::bail!("Failed to dispatch libinput events");
        }
        Ok(())
    }

    pub(crate) fn drain_events(
        &self,
        mut handler: impl FnMut(u32, u32),
    ) -> anyhow::Result<()> {
        loop {
            let event = unsafe { bindings::libinput_get_event(self.libinput.as_ptr()) };
            if event.is_null() {
                break;
            }

            let event_type = unsafe { bindings::libinput_event_get_type(event) };
            match event_type {
                bindings::LIBINPUT_EVENT_DEVICE_ADDED => {
                    debug!("libinput device added");
                }
                bindings::LIBINPUT_EVENT_DEVICE_REMOVED => {
                    debug!("libinput device removed");
                }
                bindings::LIBINPUT_EVENT_KEYBOARD_KEY => {
                    let keyboard =
                        unsafe { bindings::libinput_event_get_keyboard_event(event) };
                    if !keyboard.is_null() {
                        let key = unsafe { bindings::libinput_event_keyboard_get_key(keyboard) };
                        let state =
                            unsafe { bindings::libinput_event_keyboard_get_key_state(keyboard) };
                        handler(key, state);
                    }
                }
                _ => {}
            }

            unsafe { bindings::libinput_event_destroy(event) };
        }

        Ok(())
    }
}

impl Drop for LibInput {
    fn drop(&mut self) {
        unsafe {
            bindings::libinput_unref(self.libinput.as_ptr());
            bindings::udev_unref(self.udev.as_ptr());
        }
    }
}

impl Source for LibInput {
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

pub(crate) const KEY_STATE_PRESSED: u32 = bindings::LIBINPUT_KEY_STATE_PRESSED;
pub(crate) const KEY_STATE_RELEASED: u32 = bindings::LIBINPUT_KEY_STATE_RELEASED;

pub(crate) fn key_to_name(key: u32) -> Option<String> {
    match key {
        bindings::KEY_BACKSPACE => Some(String::from("backspace")),
        bindings::KEY_F1..=bindings::KEY_F12 => Some(format!("f{}", key - bindings::KEY_F1 + 1)),
        _ => None,
    }
}

pub(crate) fn is_modifier_key(key: u32) -> bool {
    matches!(
        key,
        bindings::KEY_LEFTCTRL
            | bindings::KEY_RIGHTCTRL
            | bindings::KEY_LEFTALT
            | bindings::KEY_RIGHTALT
            | bindings::KEY_LEFTSHIFT
            | bindings::KEY_RIGHTSHIFT
            | bindings::KEY_LEFTMETA
            | bindings::KEY_RIGHTMETA
    )
}

pub(crate) fn update_modifier(key: u32, pressed: bool, mods: &mut lumalla_shared::Mods) {
    use bindings::{
        KEY_LEFTALT, KEY_LEFTCTRL, KEY_LEFTMETA, KEY_LEFTSHIFT, KEY_RIGHTALT, KEY_RIGHTCTRL,
        KEY_RIGHTMETA, KEY_RIGHTSHIFT,
    };

    match key {
        KEY_LEFTCTRL | KEY_RIGHTCTRL => mods.ctrl = pressed,
        KEY_LEFTALT | KEY_RIGHTALT => mods.alt = pressed,
        KEY_LEFTSHIFT | KEY_RIGHTSHIFT => mods.shift = pressed,
        KEY_LEFTMETA | KEY_RIGHTMETA => mods.logo = pressed,
        _ => {}
    }
}
