//! Safe wrapper around libinput (udev backend).

use std::{
    ffi::{CStr, CString, OsStr, c_char, c_int, c_void},
    marker::PhantomData,
    os::{
        fd::{IntoRawFd, RawFd},
        unix::ffi::OsStrExt,
    },
    path::Path,
    pin::Pin,
    ptr::NonNull,
};

use anyhow::Context;
use log::{debug, info, warn};
use lumalla_seat::SeatState;
use lumalla_shared::Udev;

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code
)]
pub(crate) mod bindings {
    use std::ffi::{c_char, c_int, c_void};

    pub use lumalla_shared::udev::bindings::udev;

    pub const LIBINPUT_EVENT_NONE: u32 = 0;
    pub const LIBINPUT_EVENT_DEVICE_ADDED: u32 = 1;
    pub const LIBINPUT_EVENT_DEVICE_REMOVED: u32 = 2;
    pub const LIBINPUT_EVENT_KEYBOARD_KEY: u32 = 300;
    pub const LIBINPUT_EVENT_POINTER_MOTION: u32 = 400;
    pub const LIBINPUT_EVENT_POINTER_MOTION_ABSOLUTE: u32 = 401;
    pub const LIBINPUT_EVENT_POINTER_BUTTON: u32 = 402;
    pub const LIBINPUT_EVENT_POINTER_AXIS: u32 = 403; // Event is deprecated and should be ignored
    pub const LIBINPUT_EVENT_POINTER_SCROLL_WHEEL: u32 = 404;
    pub const LIBINPUT_EVENT_POINTER_SCROLL_FINGER: u32 = 405;
    pub const LIBINPUT_EVENT_POINTER_SCROLL_CONTINUOUS: u32 = 406;
    pub const LIBINPUT_EVENT_TOUCH_DOWN: u32 = 500;
    pub const LIBINPUT_EVENT_TOUCH_UP: u32 = 501;
    pub const LIBINPUT_EVENT_TOUCH_MOTION: u32 = 502;
    pub const LIBINPUT_EVENT_TOUCH_CANCEL: u32 = 503;
    pub const LIBINPUT_EVENT_TOUCH_FRAME: u32 = 504;

    pub const LIBINPUT_KEY_STATE_RELEASED: u32 = 0;
    pub const LIBINPUT_KEY_STATE_PRESSED: u32 = 1;

    pub const LIBINPUT_BUTTON_STATE_RELEASED: u32 = 0;
    pub const LIBINPUT_BUTTON_STATE_PRESSED: u32 = 1;

    pub const LIBINPUT_POINTER_AXIS_SCROLL_VERTICAL: u32 = 0;
    pub const LIBINPUT_POINTER_AXIS_SCROLL_HORIZONTAL: u32 = 1;

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
    pub struct libinput_event_pointer {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct libinput_event_touch {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct libinput_interface {
        pub open_restricted:
            Option<unsafe extern "C" fn(*const c_char, c_int, *mut c_void) -> c_int>,
        pub close_restricted: Option<unsafe extern "C" fn(c_int, *mut c_void)>,
    }

    unsafe extern "C" {
        pub fn libinput_udev_create_context(
            interface: *const libinput_interface,
            user_data: *const c_void,
            udev: *mut udev,
        ) -> *mut libinput;
        pub fn libinput_udev_assign_seat(libinput: *mut libinput, seat_id: *const c_char) -> c_int;
        pub fn libinput_unref(libinput: *mut libinput);
        pub fn libinput_get_fd(libinput: *mut libinput) -> c_int;
        pub fn libinput_suspend(libinput: *mut libinput) -> c_int;
        pub fn libinput_resume(libinput: *mut libinput) -> c_int;
        pub fn libinput_dispatch(libinput: *mut libinput) -> c_int;
        pub fn libinput_get_event(libinput: *mut libinput) -> *mut libinput_event;
        pub fn libinput_event_destroy(event: *mut libinput_event);
        pub fn libinput_event_get_type(event: *mut libinput_event) -> u32;
        pub fn libinput_event_get_keyboard_event(
            event: *mut libinput_event,
        ) -> *mut libinput_event_keyboard;
        pub fn libinput_event_keyboard_get_key(event: *mut libinput_event_keyboard) -> u32;
        pub fn libinput_event_keyboard_get_key_state(event: *mut libinput_event_keyboard) -> u32;
        pub fn libinput_event_get_pointer_event(
            event: *mut libinput_event,
        ) -> *mut libinput_event_pointer;
        pub fn libinput_event_pointer_get_dx(event: *mut libinput_event_pointer) -> f64;
        pub fn libinput_event_pointer_get_dy(event: *mut libinput_event_pointer) -> f64;
        pub fn libinput_event_pointer_get_absolute_x_transformed(
            event: *mut libinput_event_pointer,
            width: u32,
        ) -> f64;
        pub fn libinput_event_pointer_get_absolute_y_transformed(
            event: *mut libinput_event_pointer,
            height: u32,
        ) -> f64;
        pub fn libinput_event_pointer_get_button(event: *mut libinput_event_pointer) -> u32;
        pub fn libinput_event_pointer_get_button_state(event: *mut libinput_event_pointer) -> u32;
        pub fn libinput_event_pointer_has_axis(
            event: *mut libinput_event_pointer,
            axis: u32,
        ) -> c_int;
        pub fn libinput_event_pointer_get_scroll_value(
            event: *mut libinput_event_pointer,
            axis: u32,
        ) -> f64;
        pub fn libinput_event_get_touch_event(
            event: *mut libinput_event,
        ) -> *mut libinput_event_touch;
        pub fn libinput_event_touch_get_seat_slot(event: *mut libinput_event_touch) -> i32;
        pub fn libinput_event_touch_get_x_transformed(
            event: *mut libinput_event_touch,
            width: u32,
        ) -> f64;
        pub fn libinput_event_touch_get_y_transformed(
            event: *mut libinput_event_touch,
            height: u32,
        ) -> f64;
    }
}

/// Wrapper around a libinput udev context.
///
/// The `seat_state` reference passed to [`Self::new`] is stored as libinput userdata and
/// must remain valid until this value is dropped.
pub struct LibInput {
    libinput: NonNull<bindings::libinput>,
    _udev: Udev,
    fd: RawFd,
    _seat_state_lifetime: PhantomData<*const SeatState>,
    seat_assigned: bool,
    suspended: bool,
    transform_width: u32,
    transform_height: u32,
}

impl LibInput {
    /// Create a libinput context backed by udev.
    pub fn new(seat_state: Pin<&SeatState>) -> anyhow::Result<Self> {
        let seat_ptr = (seat_state.get_ref() as *const SeatState).cast::<c_void>();

        unsafe extern "C" fn open_restricted(
            path: *const c_char,
            flags: c_int,
            userdata: *mut c_void,
        ) -> c_int {
            if path.is_null() || userdata.is_null() {
                return -1;
            }
            let seat_state = unsafe { NonNull::new_unchecked(userdata.cast::<SeatState>()) };
            let path = unsafe { CStr::from_ptr(path) };
            let path = Path::new(OsStr::from_bytes(path.to_bytes()));
            match unsafe { seat_state.as_ref() }.open_device(path) {
                Ok(device) => {
                    let raw_fd = device.into_fd().into_raw_fd();
                    if flags & libc::O_CLOEXEC == 0 {
                        clear_close_on_exec(raw_fd);
                    }
                    raw_fd
                }
                Err(err) => {
                    warn!(
                        "Failed to open restricted device {}: {err:#}",
                        path.display()
                    );
                    -1
                }
            }
        }

        unsafe extern "C" fn close_restricted(fd: c_int, userdata: *mut c_void) {
            if userdata.is_null() {
                unsafe {
                    libc::close(fd);
                }
                return;
            }
            let seat_state = unsafe { NonNull::new_unchecked(userdata.cast::<SeatState>()) };
            // Must call libseat_close_device, not only close(2), or reopen after
            // VT resume fails with EINVAL (device still claimed by the seat).
            unsafe { seat_state.as_ref() }.close_device_fd(fd);
        }

        static INTERFACE: bindings::libinput_interface = bindings::libinput_interface {
            open_restricted: Some(open_restricted),
            close_restricted: Some(close_restricted),
        };

        let udev = Udev::new()?;

        let libinput = unsafe {
            bindings::libinput_udev_create_context(&INTERFACE, seat_ptr, udev.as_ptr())
        };
        let Some(libinput) = NonNull::new(libinput) else {
            anyhow::bail!("Failed to create libinput context");
        };

        let fd = unsafe { bindings::libinput_get_fd(libinput.as_ptr()) };
        if fd < 0 {
            unsafe {
                bindings::libinput_unref(libinput.as_ptr());
            }
            anyhow::bail!("Failed to get libinput file descriptor");
        }

        // Stay suspended until the session is enabled and a seat is assigned.
        let result = unsafe { bindings::libinput_suspend(libinput.as_ptr()) };
        if result < 0 {
            unsafe {
                bindings::libinput_unref(libinput.as_ptr());
            }
            anyhow::bail!("Failed to suspend libinput after creation");
        }

        Ok(Self {
            libinput,
            _udev: udev,
            fd,
            _seat_state_lifetime: PhantomData,
            seat_assigned: false,
            suspended: true,
            transform_width: TRANSFORM_WIDTH,
            transform_height: TRANSFORM_HEIGHT,
        })
    }

    pub fn set_coordinate_transform(&mut self, width: u32, height: u32) {
        self.transform_width = width.max(1);
        self.transform_height = height.max(1);
    }

    /// Assign the udev seat used for device discovery.
    /// Only does something if the seat has not been assigned yet.
    pub fn assign_seat(&mut self, seat_name: &str) -> anyhow::Result<()> {
        if self.seat_assigned {
            return Ok(());
        }
        let seat_name_c = CString::new(seat_name).context("Seat name contains null byte")?;
        let result = unsafe {
            bindings::libinput_udev_assign_seat(self.libinput.as_ptr(), seat_name_c.as_ptr())
        };
        if result != 0 {
            anyhow::bail!("Failed to assign libinput seat `{seat_name}`");
        }
        self.seat_assigned = true;
        info!("Assigned libinput seat `{seat_name}`");
        Ok(())
    }

    pub(crate) fn dispatch(&self) -> anyhow::Result<()> {
        let result = unsafe { bindings::libinput_dispatch(self.libinput.as_ptr()) };
        if result != 0 {
            anyhow::bail!("Failed to dispatch libinput events");
        }
        Ok(())
    }

    pub(crate) fn suspend(&mut self) -> anyhow::Result<()> {
        if self.suspended {
            return Ok(());
        }
        let result = unsafe { bindings::libinput_suspend(self.libinput.as_ptr()) };
        if result < 0 {
            anyhow::bail!("Failed to suspend libinput");
        }
        self.suspended = true;
        Ok(())
    }

    pub(crate) fn resume(&mut self) -> anyhow::Result<()> {
        if !self.suspended {
            return Ok(());
        }
        let result = unsafe { bindings::libinput_resume(self.libinput.as_ptr()) };
        if result < 0 {
            anyhow::bail!("Failed to resume libinput");
        }
        self.suspended = false;
        Ok(())
    }
}

/// Clears the close-on-exec flag on `fd`, leaving other descriptor flags unchanged.
fn clear_close_on_exec(fd: RawFd) {
    let current = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if current >= 0 && current & libc::FD_CLOEXEC != 0 {
        unsafe { libc::fcntl(fd, libc::F_SETFD, current & !libc::FD_CLOEXEC) };
    }
}

impl Drop for LibInput {
    fn drop(&mut self) {
        unsafe {
            bindings::libinput_unref(self.libinput.as_ptr());
        }
    }
}

impl LibInput {
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

pub(crate) const KEY_STATE_PRESSED: u32 = bindings::LIBINPUT_KEY_STATE_PRESSED;
pub(crate) const KEY_STATE_RELEASED: u32 = bindings::LIBINPUT_KEY_STATE_RELEASED;
pub(crate) const BUTTON_STATE_PRESSED: u32 = bindings::LIBINPUT_BUTTON_STATE_PRESSED;

/// Default transform size for absolute pointer/touch coordinates (matches virtual output).
const TRANSFORM_WIDTH: u32 = 800;
const TRANSFORM_HEIGHT: u32 = 600;

pub(crate) enum InputEvent {
    KeyboardKey { key: u32, state: u32 },
    PointerMotion { dx: f64, dy: f64 },
    PointerAbsolute { x: f64, y: f64 },
    PointerButton { button: u32, pressed: bool },
    PointerAxis { axis: u32, value: f64 },
    TouchDown { id: i32, x: f64, y: f64 },
    TouchUp { id: i32 },
    TouchMotion { id: i32, x: f64, y: f64 },
    TouchCancel,
    TouchFrame,
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

impl LibInput {
    pub(crate) fn next_event(&self) -> Option<InputEvent> {
        loop {
            let event = unsafe { bindings::libinput_get_event(self.libinput.as_ptr()) };
            if event.is_null() {
                return None;
            }
            let event_type = unsafe { bindings::libinput_event_get_type(event) };
            let input_event = match event_type {
                bindings::LIBINPUT_EVENT_NONE => {
                    unsafe { bindings::libinput_event_destroy(event) };
                    return None;
                }
                bindings::LIBINPUT_EVENT_DEVICE_ADDED => {
                    debug!("libinput device added");
                    None
                }
                bindings::LIBINPUT_EVENT_DEVICE_REMOVED => {
                    debug!("libinput device removed");
                    None
                }
                bindings::LIBINPUT_EVENT_KEYBOARD_KEY => {
                    let keyboard_event =
                        unsafe { bindings::libinput_event_get_keyboard_event(event) };
                    if keyboard_event.is_null() {
                        None
                    } else {
                        let key =
                            unsafe { bindings::libinput_event_keyboard_get_key(keyboard_event) };
                        let state = unsafe {
                            bindings::libinput_event_keyboard_get_key_state(keyboard_event)
                        };
                        Some(InputEvent::KeyboardKey { key, state })
                    }
                }
                bindings::LIBINPUT_EVENT_POINTER_MOTION => {
                    let pointer = unsafe { bindings::libinput_event_get_pointer_event(event) };
                    if pointer.is_null() {
                        None
                    } else {
                        let dx = unsafe { bindings::libinput_event_pointer_get_dx(pointer) };
                        let dy = unsafe { bindings::libinput_event_pointer_get_dy(pointer) };
                        Some(InputEvent::PointerMotion { dx, dy })
                    }
                }
                bindings::LIBINPUT_EVENT_POINTER_MOTION_ABSOLUTE => {
                    let pointer = unsafe { bindings::libinput_event_get_pointer_event(event) };
                    if pointer.is_null() {
                        None
                    } else {
                        let x = unsafe {
                            bindings::libinput_event_pointer_get_absolute_x_transformed(
                                pointer,
                                self.transform_width,
                            )
                        };
                        let y = unsafe {
                            bindings::libinput_event_pointer_get_absolute_y_transformed(
                                pointer,
                                self.transform_height,
                            )
                        };
                        Some(InputEvent::PointerAbsolute { x, y })
                    }
                }
                bindings::LIBINPUT_EVENT_POINTER_BUTTON => {
                    let pointer = unsafe { bindings::libinput_event_get_pointer_event(event) };
                    if pointer.is_null() {
                        None
                    } else {
                        let button =
                            unsafe { bindings::libinput_event_pointer_get_button(pointer) };
                        let state = unsafe {
                            bindings::libinput_event_pointer_get_button_state(pointer)
                        };
                        Some(InputEvent::PointerButton {
                            button,
                            pressed: state == BUTTON_STATE_PRESSED,
                        })
                    }
                }
                bindings::LIBINPUT_EVENT_POINTER_SCROLL_WHEEL
                | bindings::LIBINPUT_EVENT_POINTER_SCROLL_FINGER
                | bindings::LIBINPUT_EVENT_POINTER_SCROLL_CONTINUOUS => {
                    let pointer = unsafe { bindings::libinput_event_get_pointer_event(event) };
                    if pointer.is_null() {
                        None
                    } else {
                        parse_scroll_axes(pointer)
                    }
                }
                bindings::LIBINPUT_EVENT_POINTER_AXIS => None,
                bindings::LIBINPUT_EVENT_TOUCH_DOWN => {
                    let touch = unsafe { bindings::libinput_event_get_touch_event(event) };
                    if touch.is_null() {
                        None
                    } else {
                        let id = unsafe { bindings::libinput_event_touch_get_seat_slot(touch) };
                        let x = unsafe {
                            bindings::libinput_event_touch_get_x_transformed(
                                touch,
                                self.transform_width,
                            )
                        };
                        let y = unsafe {
                            bindings::libinput_event_touch_get_y_transformed(
                                touch,
                                self.transform_height,
                            )
                        };
                        Some(InputEvent::TouchDown { id, x, y })
                    }
                }
                bindings::LIBINPUT_EVENT_TOUCH_UP => {
                    let touch = unsafe { bindings::libinput_event_get_touch_event(event) };
                    if touch.is_null() {
                        None
                    } else {
                        let id = unsafe { bindings::libinput_event_touch_get_seat_slot(touch) };
                        Some(InputEvent::TouchUp { id })
                    }
                }
                bindings::LIBINPUT_EVENT_TOUCH_MOTION => {
                    let touch = unsafe { bindings::libinput_event_get_touch_event(event) };
                    if touch.is_null() {
                        None
                    } else {
                        let id = unsafe { bindings::libinput_event_touch_get_seat_slot(touch) };
                        let x = unsafe {
                            bindings::libinput_event_touch_get_x_transformed(
                                touch,
                                self.transform_width,
                            )
                        };
                        let y = unsafe {
                            bindings::libinput_event_touch_get_y_transformed(
                                touch,
                                self.transform_height,
                            )
                        };
                        Some(InputEvent::TouchMotion { id, x, y })
                    }
                }
                bindings::LIBINPUT_EVENT_TOUCH_CANCEL => Some(InputEvent::TouchCancel),
                bindings::LIBINPUT_EVENT_TOUCH_FRAME => Some(InputEvent::TouchFrame),
                event_type => {
                    debug!("Unhandled libinput event type: {event_type}");
                    None
                }
            };
            unsafe { bindings::libinput_event_destroy(event) };
            if input_event.is_some() {
                return input_event;
            }
        }
    }
}

fn parse_scroll_axes(pointer: *mut bindings::libinput_event_pointer) -> Option<InputEvent> {
    // Prefer vertical, then horizontal; emit one axis event per libinput event.
    for axis in [
        bindings::LIBINPUT_POINTER_AXIS_SCROLL_VERTICAL,
        bindings::LIBINPUT_POINTER_AXIS_SCROLL_HORIZONTAL,
    ] {
        let has = unsafe { bindings::libinput_event_pointer_has_axis(pointer, axis) };
        if has != 0 {
            let value = unsafe { bindings::libinput_event_pointer_get_scroll_value(pointer, axis) };
            return Some(InputEvent::PointerAxis { axis, value });
        }
    }
    None
}
