//! Safe wrappers around libudev.

use std::{
    ffi::{CStr, CString, OsStr},
    os::unix::ffi::OsStrExt,
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
}

impl Drop for UdevDevice {
    fn drop(&mut self) {
        unsafe {
            bindings::udev_device_unref(self.device.as_ptr());
        }
    }
}
