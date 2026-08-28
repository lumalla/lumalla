//! Safe wrapper around libxkbcommon.

use std::{
    ffi::{CStr, c_char, c_int, c_void},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    ptr::{self, NonNull},
};

use anyhow::Context;
use log::debug;
use lumalla_shared::KeymapMemfd;

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code
)]
mod bindings {
    use std::ffi::{c_char, c_int};

    pub const XKB_CONTEXT_NO_FLAGS: c_int = 0;
    pub const XKB_KEYMAP_COMPILE_NO_FLAGS: c_int = 0;
    pub const XKB_KEYMAP_FORMAT_TEXT_V1: c_int = 1;
    pub const XKB_KEY_UP: c_int = 0;
    pub const XKB_KEY_DOWN: c_int = 1;

    pub const XKB_STATE_MODS_DEPRESSED: c_int = 1 << 0;
    pub const XKB_STATE_MODS_LATCHED: c_int = 1 << 1;
    pub const XKB_STATE_MODS_LOCKED: c_int = 1 << 2;
    pub const XKB_STATE_LAYOUT_EFFECTIVE: c_int = 1 << 7;
    pub const XKB_KEYSYM_NO_FLAGS: c_int = 0;

    /// Offset from Linux/evdev KEY_* codes to xkb keycodes.
    pub const EVDEV_OFFSET: u32 = 8;

    #[repr(C)]
    pub struct xkb_context {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct xkb_keymap {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct xkb_state {
        _private: [u8; 0],
    }

    pub type xkb_keycode_t = u32;
    pub type xkb_keysym_t = u32;
    pub type xkb_mod_mask_t = u32;
    pub type xkb_layout_index_t = u32;

    #[repr(C)]
    pub struct xkb_rule_names {
        pub rules: *const c_char,
        pub model: *const c_char,
        pub layout: *const c_char,
        pub variant: *const c_char,
        pub options: *const c_char,
    }

    unsafe extern "C" {
        pub fn xkb_context_new(flags: c_int) -> *mut xkb_context;
        pub fn xkb_context_unref(context: *mut xkb_context);

        pub fn xkb_keymap_new_from_names(
            context: *mut xkb_context,
            names: *const xkb_rule_names,
            flags: c_int,
        ) -> *mut xkb_keymap;
        pub fn xkb_keymap_unref(keymap: *mut xkb_keymap);
        pub fn xkb_keymap_get_as_string(keymap: *mut xkb_keymap, format: c_int) -> *mut c_char;

        pub fn xkb_state_new(keymap: *mut xkb_keymap) -> *mut xkb_state;
        pub fn xkb_state_unref(state: *mut xkb_state);
        pub fn xkb_state_update_key(
            state: *mut xkb_state,
            key: xkb_keycode_t,
            direction: c_int,
        ) -> c_int;
        pub fn xkb_state_key_get_one_sym(state: *mut xkb_state, key: xkb_keycode_t)
        -> xkb_keysym_t;
        pub fn xkb_state_serialize_mods(state: *mut xkb_state, components: c_int)
        -> xkb_mod_mask_t;
        pub fn xkb_state_serialize_layout(
            state: *mut xkb_state,
            components: c_int,
        ) -> xkb_layout_index_t;

        pub fn xkb_keysym_get_name(keysym: xkb_keysym_t, buffer: *mut c_char, size: usize)
        -> c_int;
        pub fn xkb_keysym_from_name(name: *const c_char, flags: c_int) -> xkb_keysym_t;
        pub fn xkb_utf32_to_keysym(ucs4: u32) -> xkb_keysym_t;
        pub fn xkb_keymap_min_keycode(keymap: *mut xkb_keymap) -> xkb_keycode_t;
        pub fn xkb_keymap_max_keycode(keymap: *mut xkb_keymap) -> xkb_keycode_t;
        pub fn xkb_keymap_num_levels_for_key(
            keymap: *mut xkb_keymap,
            key: xkb_keycode_t,
            layout: xkb_layout_index_t,
        ) -> u32;
        pub fn xkb_keymap_key_get_syms_by_level(
            keymap: *mut xkb_keymap,
            key: xkb_keycode_t,
            layout: xkb_layout_index_t,
            level: u32,
            syms_out: *mut *const xkb_keysym_t,
        ) -> c_int;
    }
}

/// How to press a keysym on the current keymap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeysymPress {
    /// Linux/evdev keycode.
    pub evdev_keycode: u32,
    /// Whether left shift should be held for this press.
    pub shift: bool,
}

/// Modifier and layout state from xkb.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct XkbModifiers {
    pub depressed: u32,
    pub latched: u32,
    pub locked: u32,
    pub group: u32,
}

/// Seat keyboard state backed by libxkbcommon.
pub struct Xkb {
    context: NonNull<bindings::xkb_context>,
    keymap: NonNull<bindings::xkb_keymap>,
    state: NonNull<bindings::xkb_state>,
}

impl Xkb {
    /// Create a context with the system default keymap and a fresh state.
    pub fn new() -> anyhow::Result<Self> {
        let context = unsafe { bindings::xkb_context_new(bindings::XKB_CONTEXT_NO_FLAGS) };
        let Some(context) = NonNull::new(context) else {
            anyhow::bail!("Failed to create xkb context");
        };

        // NULL fields select libxkbcommon defaults (typically evdev/pc105/us).
        let names = bindings::xkb_rule_names {
            rules: ptr::null(),
            model: ptr::null(),
            layout: ptr::null(),
            variant: ptr::null(),
            options: ptr::null(),
        };
        let keymap = unsafe {
            bindings::xkb_keymap_new_from_names(
                context.as_ptr(),
                &names,
                bindings::XKB_KEYMAP_COMPILE_NO_FLAGS,
            )
        };
        let Some(keymap) = NonNull::new(keymap) else {
            unsafe { bindings::xkb_context_unref(context.as_ptr()) };
            anyhow::bail!("Failed to compile default xkb keymap");
        };

        let state = unsafe { bindings::xkb_state_new(keymap.as_ptr()) };
        let Some(state) = NonNull::new(state) else {
            unsafe {
                bindings::xkb_keymap_unref(keymap.as_ptr());
                bindings::xkb_context_unref(context.as_ptr());
            }
            anyhow::bail!("Failed to create xkb state");
        };

        debug!("Created xkb context with default keymap");
        Ok(Self {
            context,
            keymap,
            state,
        })
    }

    /// Reset keyboard state (e.g. after losing the seat).
    pub fn reset(&mut self) -> anyhow::Result<()> {
        let state = unsafe { bindings::xkb_state_new(self.keymap.as_ptr()) };
        let Some(state) = NonNull::new(state) else {
            anyhow::bail!("Failed to recreate xkb state");
        };
        unsafe { bindings::xkb_state_unref(self.state.as_ptr()) };
        self.state = state;
        Ok(())
    }

    /// Update state from a Linux/evdev keycode and press/release.
    ///
    /// Returns `true` if modifier or layout state changed.
    pub fn update_key(&mut self, evdev_keycode: u32, pressed: bool) -> bool {
        let key = evdev_keycode + bindings::EVDEV_OFFSET;
        let direction = if pressed {
            bindings::XKB_KEY_DOWN
        } else {
            bindings::XKB_KEY_UP
        };
        let changed =
            unsafe { bindings::xkb_state_update_key(self.state.as_ptr(), key, direction) };
        const MODS_OR_LAYOUT: c_int = bindings::XKB_STATE_MODS_DEPRESSED
            | bindings::XKB_STATE_MODS_LATCHED
            | bindings::XKB_STATE_MODS_LOCKED
            | bindings::XKB_STATE_LAYOUT_EFFECTIVE;
        changed & MODS_OR_LAYOUT != 0
    }

    /// Current serialized modifier/layout state.
    pub fn modifiers(&self) -> XkbModifiers {
        unsafe {
            XkbModifiers {
                depressed: bindings::xkb_state_serialize_mods(
                    self.state.as_ptr(),
                    bindings::XKB_STATE_MODS_DEPRESSED,
                ),
                latched: bindings::xkb_state_serialize_mods(
                    self.state.as_ptr(),
                    bindings::XKB_STATE_MODS_LATCHED,
                ),
                locked: bindings::xkb_state_serialize_mods(
                    self.state.as_ptr(),
                    bindings::XKB_STATE_MODS_LOCKED,
                ),
                group: bindings::xkb_state_serialize_layout(
                    self.state.as_ptr(),
                    bindings::XKB_STATE_LAYOUT_EFFECTIVE,
                ),
            }
        }
    }

    /// Sealed memfd containing a null-terminated TEXT_V1 keymap for `wl_keyboard.keymap`.
    pub fn keymap_memfd(&self) -> anyhow::Result<KeymapMemfd> {
        let ptr = unsafe {
            bindings::xkb_keymap_get_as_string(
                self.keymap.as_ptr(),
                bindings::XKB_KEYMAP_FORMAT_TEXT_V1,
            )
        };
        if ptr.is_null() {
            anyhow::bail!("Failed to serialize xkb keymap");
        }

        let result = (|| {
            let len_with_nul = unsafe { libc::strlen(ptr) }
                .checked_add(1)
                .context("Keymap length overflow")?;
            let size = u32::try_from(len_with_nul).context("Keymap is too large")?;

            let fd = unsafe {
                libc::memfd_create(
                    c"lumalla-keymap".as_ptr(),
                    libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error())
                    .context("Failed to create memfd for keymap");
            }
            let owned = unsafe { OwnedFd::from_raw_fd(fd) };

            let mut written = 0usize;
            while written < len_with_nul {
                let result = unsafe {
                    libc::write(
                        owned.as_raw_fd(),
                        ptr.add(written).cast(),
                        len_with_nul - written,
                    )
                };
                if result < 0 {
                    return Err(std::io::Error::last_os_error())
                        .context("Failed to write keymap memfd");
                }
                written += result as usize;
            }

            if unsafe { libc::lseek(owned.as_raw_fd(), 0, libc::SEEK_SET) } < 0 {
                return Err(std::io::Error::last_os_error())
                    .context("Failed to rewind keymap memfd");
            }

            let seals =
                libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL;
            if unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
                return Err(std::io::Error::last_os_error()).context("Failed to seal keymap memfd");
            }

            Ok(KeymapMemfd::new(owned, size))
        })();

        unsafe {
            libc::free(ptr.cast::<c_void>());
        }
        result
    }

    /// Keysym for an evdev keycode under the current state.
    pub fn key_get_one_sym(&self, evdev_keycode: u32) -> u32 {
        let key = evdev_keycode + bindings::EVDEV_OFFSET;
        unsafe { bindings::xkb_state_key_get_one_sym(self.state.as_ptr(), key) }
    }

    /// Resolve a keysym name (e.g. `"Return"`, `"a"`) to a keysym value.
    pub fn keysym_from_name(name: &str) -> anyhow::Result<u32> {
        let name_c = std::ffi::CString::new(name).context("Keysym name contains null byte")?;
        let keysym = unsafe {
            bindings::xkb_keysym_from_name(
                name_c.as_ptr(),
                bindings::XKB_KEYSYM_NO_FLAGS,
            )
        };
        if keysym == 0 {
            anyhow::bail!("Unknown keysym name: {name}");
        }
        Ok(keysym)
    }

    /// Resolve a Unicode scalar to a keysym.
    pub fn keysym_from_char(ch: char) -> anyhow::Result<u32> {
        let keysym = unsafe { bindings::xkb_utf32_to_keysym(ch as u32) };
        if keysym == 0 {
            anyhow::bail!("No keysym for character: {ch:?}");
        }
        Ok(keysym)
    }

    /// Find an evdev keycode (and shift state) that produces `keysym` on layout 0.
    pub fn evdev_key_for_keysym(&self, keysym: u32) -> Option<KeysymPress> {
        let min_key = unsafe { bindings::xkb_keymap_min_keycode(self.keymap.as_ptr()) };
        let max_key = unsafe { bindings::xkb_keymap_max_keycode(self.keymap.as_ptr()) };
        let layout = 0;

        for key in min_key..=max_key {
            let levels = unsafe {
                bindings::xkb_keymap_num_levels_for_key(self.keymap.as_ptr(), key, layout)
            };
            for level in 0..levels {
                let mut syms_ptr: *const bindings::xkb_keysym_t = std::ptr::null();
                let count = unsafe {
                    bindings::xkb_keymap_key_get_syms_by_level(
                        self.keymap.as_ptr(),
                        key,
                        layout,
                        level,
                        &mut syms_ptr,
                    )
                };
                if count <= 0 || syms_ptr.is_null() {
                    continue;
                }
                let sym = unsafe { *syms_ptr };
                if sym == keysym {
                    let evdev_keycode = key.saturating_sub(bindings::EVDEV_OFFSET);
                    return Some(KeysymPress {
                        evdev_keycode,
                        shift: level > 0,
                    });
                }
            }
        }
        None
    }

    /// Human-readable name for a keysym (e.g. `"a"`, `"Return"`).
    pub fn keysym_get_name(keysym: u32) -> anyhow::Result<String> {
        let mut buf = [0u8; 64];
        let len = unsafe {
            bindings::xkb_keysym_get_name(keysym, buf.as_mut_ptr().cast::<c_char>(), buf.len())
        };
        if len < 0 {
            anyhow::bail!("Failed to get xkb keysym name for {keysym}");
        }
        let name =
            CStr::from_bytes_until_nul(&buf).context("xkb keysym name was not null-terminated")?;
        Ok(name.to_string_lossy().into_owned())
    }
}

impl Drop for Xkb {
    fn drop(&mut self) {
        unsafe {
            bindings::xkb_state_unref(self.state.as_ptr());
            bindings::xkb_keymap_unref(self.keymap.as_ptr());
            bindings::xkb_context_unref(self.context.as_ptr());
        }
    }
}
