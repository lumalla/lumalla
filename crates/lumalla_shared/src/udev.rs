//! Safe wrappers around libudev.

use std::{
    ffi::{CStr, CString, OsStr},
    os::{
        fd::{AsRawFd, RawFd},
        unix::ffi::OsStrExt,
    },
    path::{Path, PathBuf},
    ptr::NonNull,
};

use anyhow::Context;

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code
)]
pub mod bindings {
    use std::ffi::{c_char, c_int};

    #[repr(C)]
    pub struct udev {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct udev_enumerate {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct udev_list_entry {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct udev_device {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct udev_monitor {
        _private: [u8; 0],
    }

    unsafe extern "C" {
        pub fn udev_new() -> *mut udev;
        pub fn udev_ref(udev: *mut udev) -> *mut udev;
        pub fn udev_unref(udev: *mut udev) -> *mut udev;

        pub fn udev_enumerate_new(udev: *mut udev) -> *mut udev_enumerate;
        pub fn udev_enumerate_unref(udev_enumerate: *mut udev_enumerate) -> *mut udev_enumerate;
        pub fn udev_enumerate_add_match_subsystem(
            udev_enumerate: *mut udev_enumerate,
            subsystem: *const c_char,
        ) -> c_int;
        pub fn udev_enumerate_add_match_sysname(
            udev_enumerate: *mut udev_enumerate,
            sysname: *const c_char,
        ) -> c_int;
        pub fn udev_enumerate_scan_devices(udev_enumerate: *mut udev_enumerate) -> c_int;
        pub fn udev_enumerate_get_list_entry(
            udev_enumerate: *mut udev_enumerate,
        ) -> *mut udev_list_entry;

        pub fn udev_list_entry_get_next(list_entry: *mut udev_list_entry) -> *mut udev_list_entry;
        pub fn udev_list_entry_get_name(list_entry: *mut udev_list_entry) -> *const c_char;

        pub fn udev_device_new_from_syspath(
            udev: *mut udev,
            syspath: *const c_char,
        ) -> *mut udev_device;
        pub fn udev_device_unref(udev_device: *mut udev_device) -> *mut udev_device;
        pub fn udev_device_get_devnode(udev_device: *mut udev_device) -> *const c_char;
        pub fn udev_device_get_sysname(udev_device: *mut udev_device) -> *const c_char;
        pub fn udev_device_get_action(udev_device: *mut udev_device) -> *const c_char;
        pub fn udev_device_get_subsystem(udev_device: *mut udev_device) -> *const c_char;

        pub fn udev_monitor_new_from_netlink(
            udev: *mut udev,
            name: *const c_char,
        ) -> *mut udev_monitor;
        pub fn udev_monitor_unref(udev_monitor: *mut udev_monitor) -> *mut udev_monitor;
        pub fn udev_monitor_filter_add_match_subsystem_devtype(
            udev_monitor: *mut udev_monitor,
            subsystem: *const c_char,
            devtype: *const c_char,
        ) -> c_int;
        pub fn udev_monitor_enable_receiving(udev_monitor: *mut udev_monitor) -> c_int;
        pub fn udev_monitor_get_fd(udev_monitor: *mut udev_monitor) -> c_int;
        pub fn udev_monitor_receive_device(udev_monitor: *mut udev_monitor) -> *mut udev_device;
    }
}

/// libudev context.
pub struct Udev {
    ptr: NonNull<bindings::udev>,
}

impl Udev {
    /// Create a new udev context.
    pub fn new() -> anyhow::Result<Self> {
        let ptr = unsafe { bindings::udev_new() };
        let Some(ptr) = NonNull::new(ptr) else {
            anyhow::bail!("Failed to create udev context");
        };
        Ok(Self { ptr })
    }

    /// Raw pointer for FFI consumers such as libinput.
    pub fn as_ptr(&self) -> *mut bindings::udev {
        self.ptr.as_ptr()
    }

    /// Create a device enumeration object for this context.
    pub fn enumerate(&self) -> anyhow::Result<UdevEnumerate<'_>> {
        let enumerate = unsafe { bindings::udev_enumerate_new(self.as_ptr()) };
        let Some(enumerate) = NonNull::new(enumerate) else {
            anyhow::bail!("Failed to create udev enumerate");
        };
        Ok(UdevEnumerate {
            enumerate,
            udev: self,
        })
    }

    /// Create a netlink monitor for receiving device events.
    pub fn monitor(&self) -> anyhow::Result<UdevMonitor> {
        let name = CString::new("udev").context("Invalid udev monitor name")?;
        let monitor =
            unsafe { bindings::udev_monitor_new_from_netlink(self.as_ptr(), name.as_ptr()) };
        let Some(monitor) = NonNull::new(monitor) else {
            anyhow::bail!("Failed to create udev monitor");
        };
        Ok(UdevMonitor { monitor })
    }
}

impl Drop for Udev {
    fn drop(&mut self) {
        unsafe {
            bindings::udev_unref(self.as_ptr());
        }
    }
}

/// Device enumeration builder/scanner.
pub struct UdevEnumerate<'a> {
    enumerate: NonNull<bindings::udev_enumerate>,
    udev: &'a Udev,
}

impl UdevEnumerate<'_> {
    /// Only include devices belonging to `subsystem` (e.g. `"drm"`).
    pub fn match_subsystem(&mut self, subsystem: &str) -> anyhow::Result<()> {
        let subsystem = CString::new(subsystem).context("Subsystem contains null byte")?;
        let result = unsafe {
            bindings::udev_enumerate_add_match_subsystem(
                self.enumerate.as_ptr(),
                subsystem.as_ptr(),
            )
        };
        if result < 0 {
            anyhow::bail!("Failed to add udev subsystem match");
        }
        Ok(())
    }

    /// Only include devices whose sysname matches `sysname` (shell-style glob).
    pub fn match_sysname(&mut self, sysname: &str) -> anyhow::Result<()> {
        let sysname = CString::new(sysname).context("Sysname contains null byte")?;
        let result = unsafe {
            bindings::udev_enumerate_add_match_sysname(self.enumerate.as_ptr(), sysname.as_ptr())
        };
        if result < 0 {
            anyhow::bail!("Failed to add udev sysname match");
        }
        Ok(())
    }

    /// Scan `/sys` for devices matching the configured filters.
    pub fn scan_devices(&mut self) -> anyhow::Result<()> {
        let result = unsafe { bindings::udev_enumerate_scan_devices(self.enumerate.as_ptr()) };
        if result < 0 {
            anyhow::bail!("Failed to scan udev devices");
        }
        Ok(())
    }

    /// Return devices discovered by the last [`Self::scan_devices`] call.
    pub fn devices(&self) -> anyhow::Result<Vec<UdevDevice>> {
        let mut devices = Vec::new();
        let mut entry = unsafe { bindings::udev_enumerate_get_list_entry(self.enumerate.as_ptr()) };

        while !entry.is_null() {
            let syspath = unsafe { bindings::udev_list_entry_get_name(entry) };
            if !syspath.is_null() {
                let device =
                    unsafe { bindings::udev_device_new_from_syspath(self.udev.as_ptr(), syspath) };
                if let Some(device) = NonNull::new(device) {
                    devices.push(UdevDevice { device });
                }
            }
            entry = unsafe { bindings::udev_list_entry_get_next(entry) };
        }

        Ok(devices)
    }
}

impl Drop for UdevEnumerate<'_> {
    fn drop(&mut self) {
        unsafe {
            bindings::udev_enumerate_unref(self.enumerate.as_ptr());
        }
    }
}

/// Netlink monitor for udev device add/remove/change events.
pub struct UdevMonitor {
    monitor: NonNull<bindings::udev_monitor>,
}

impl UdevMonitor {
    /// Only receive events for devices in `subsystem` (e.g. `"drm"`).
    pub fn match_subsystem(&mut self, subsystem: &str) -> anyhow::Result<()> {
        let subsystem = CString::new(subsystem).context("Subsystem contains null byte")?;
        let result = unsafe {
            bindings::udev_monitor_filter_add_match_subsystem_devtype(
                self.monitor.as_ptr(),
                subsystem.as_ptr(),
                std::ptr::null(),
            )
        };
        if result < 0 {
            anyhow::bail!("Failed to add udev monitor subsystem filter");
        }
        Ok(())
    }

    /// Start receiving events on the monitor socket.
    ///
    /// Must be called after configuring filters and before polling [`Self::fd`].
    pub fn enable_receiving(&mut self) -> anyhow::Result<()> {
        let result = unsafe { bindings::udev_monitor_enable_receiving(self.monitor.as_ptr()) };
        if result < 0 {
            anyhow::bail!("Failed to enable udev monitor receiving");
        }
        Ok(())
    }

    /// File descriptor suitable for `poll`/`epoll`/`mio`.
    pub fn fd(&self) -> RawFd {
        unsafe { bindings::udev_monitor_get_fd(self.monitor.as_ptr()) }
    }

    /// Receive the next pending device event, if any.
    pub fn receive_device(&mut self) -> Option<UdevDevice> {
        let device = unsafe { bindings::udev_monitor_receive_device(self.monitor.as_ptr()) };
        NonNull::new(device).map(|device| UdevDevice { device })
    }
}

impl AsRawFd for UdevMonitor {
    fn as_raw_fd(&self) -> RawFd {
        self.fd()
    }
}

impl Drop for UdevMonitor {
    fn drop(&mut self) {
        unsafe {
            bindings::udev_monitor_unref(self.monitor.as_ptr());
        }
    }
}

/// A single udev device.
pub struct UdevDevice {
    device: NonNull<bindings::udev_device>,
}

impl UdevDevice {
    /// Device node path, if any (e.g. `/dev/dri/card0`).
    pub fn devnode(&self) -> Option<PathBuf> {
        let ptr = unsafe { bindings::udev_device_get_devnode(self.device.as_ptr()) };
        if ptr.is_null() {
            return None;
        }
        let cstr = unsafe { CStr::from_ptr(ptr) };
        Some(Path::new(OsStr::from_bytes(cstr.to_bytes())).to_path_buf())
    }

    /// Sysfs device name (e.g. `card0`).
    pub fn sysname(&self) -> Option<&str> {
        let ptr = unsafe { bindings::udev_device_get_sysname(self.device.as_ptr()) };
        if ptr.is_null() {
            return None;
        }
        unsafe { CStr::from_ptr(ptr) }.to_str().ok()
    }

    /// Event action (`"add"`, `"remove"`, `"change"`, …), if any.
    pub fn action(&self) -> Option<&str> {
        let ptr = unsafe { bindings::udev_device_get_action(self.device.as_ptr()) };
        if ptr.is_null() {
            return None;
        }
        unsafe { CStr::from_ptr(ptr) }.to_str().ok()
    }

    /// Device subsystem (e.g. `"drm"`), if any.
    pub fn subsystem(&self) -> Option<&str> {
        let ptr = unsafe { bindings::udev_device_get_subsystem(self.device.as_ptr()) };
        if ptr.is_null() {
            return None;
        }
        unsafe { CStr::from_ptr(ptr) }.to_str().ok()
    }
}

impl Drop for UdevDevice {
    fn drop(&mut self) {
        unsafe {
            bindings::udev_device_unref(self.device.as_ptr());
        }
    }
}
