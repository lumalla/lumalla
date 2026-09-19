//! Input handling for Lumalla via libinput.

mod libinput;
mod xkb;

use std::{collections::HashSet, os::fd::RawFd, pin::Pin, time::Instant};

use log::{debug, warn};
use lumalla_seat::SeatState;
use lumalla_shared::{Comms, DbusMessage, KeymapMemfd, MainMessage, Mods, XkbConfig};

use crate::libinput::{InputEvent, KEY_STATE_PRESSED, LibInput, is_modifier_key, update_modifier};
use crate::xkb::Xkb;

pub use xkb::XkbModifiers as KeyboardModifiers;

/// Resolve a key name (e.g. `"m"`, `"Return"`, `"F1"`) to a Linux evdev keycode.
///
/// Uses the system default XKB names (typically US). Prefer
/// [`evdev_keycode_from_name_with_xkb`] when a layout has been configured.
pub fn evdev_keycode_from_name(name: &str) -> Option<u32> {
    evdev_keycode_from_name_with_xkb(name, &XkbConfig::default())
}

/// Resolve a key name against a specific XKB RMLVO configuration (layout group 0).
pub fn evdev_keycode_from_name_with_xkb(name: &str, config: &XkbConfig) -> Option<u32> {
    let xkb = Xkb::new(config).ok()?;
    for candidate in candidate_key_names(name) {
        let Ok(keysym) = Xkb::keysym_from_name(&candidate) else {
            continue;
        };
        let Some(press) = xkb.evdev_key_for_keysym(keysym) else {
            continue;
        };
        if press.shift {
            warn!(
                "Key `{name}` requires shift on the configured keymap; map_key bindings do not model shift yet"
            );
        }
        return Some(press.evdev_keycode);
    }
    None
}

fn candidate_key_names(name: &str) -> Vec<String> {
    let mut candidates = vec![name.to_string()];
    if name.len() == 1 {
        candidates.push(name.to_uppercase());
    }
    if let Some(digits) = name.strip_prefix('f').or_else(|| name.strip_prefix('F'))
        && !digits.is_empty()
        && digits.chars().all(|ch| ch.is_ascii_digit())
    {
        candidates.push(format!("F{digits}"));
    }
    if name.eq_ignore_ascii_case("backspace") {
        candidates.push("BackSpace".to_string());
    }
    candidates
}

/// Split an optional Control chord prefix from a key name.
///
/// Accepts `C-c`, `Ctrl+c`, and `Control+c` (any ASCII case on the prefix).
fn split_ctrl_chord(name: &str) -> (bool, &str) {
    for prefix in [
        "C-", "c-", "Ctrl+", "ctrl+", "CTRL+", "Control+", "control+", "CONTROL+",
    ] {
        if let Some(rest) = name.strip_prefix(prefix) {
            if !rest.is_empty() {
                return (true, rest);
            }
        }
    }
    (false, name)
}

struct KeyBinding {
    key: u32,
    mods: Mods,
    binding_id: String,
    on_release: bool,
    consume: bool,
}

/// Keyboard updates for the Wayland seat after libinput dispatch.
#[derive(Debug, Clone, Copy)]
pub enum KeyboardEvent {
    Key {
        time_msec: u32,
        /// Linux/evdev keycode (libinput / `wl_keyboard.key`).
        key: u32,
        pressed: bool,
    },
    Modifiers(KeyboardModifiers),
}

/// Pointer updates for the Wayland seat after libinput dispatch.
#[derive(Debug, Clone, Copy)]
pub enum PointerEvent {
    Motion {
        time_msec: u32,
        dx: f64,
        dy: f64,
        dx_unaccel: f64,
        dy_unaccel: f64,
    },
    Absolute {
        time_msec: u32,
        x: f64,
        y: f64,
    },
    Button {
        time_msec: u32,
        button: u32,
        pressed: bool,
    },
    Axis {
        time_msec: u32,
        axis: u32,
        value: f32,
    },
}

/// Touch updates for the Wayland seat after libinput dispatch.
#[derive(Debug, Clone, Copy)]
pub enum TouchEvent {
    Down {
        time_msec: u32,
        id: i32,
        x: f64,
        y: f64,
    },
    Up {
        time_msec: u32,
        id: i32,
    },
    Motion {
        time_msec: u32,
        id: i32,
        x: f64,
        y: f64,
    },
    Frame,
    Cancel,
}

/// Aggregated seat input events forwarded to the compositor.
#[derive(Debug, Clone, Copy)]
pub enum SeatEvent {
    Keyboard(KeyboardEvent),
    Pointer(PointerEvent),
    Touch(TouchEvent),
}

/// Linux input button code for the left mouse button.
pub const BTN_LEFT: u32 = 0x110;

pub struct InputState {
    comms: Comms,
    libinput: LibInput,
    xkb: Xkb,
    xkb_config: XkbConfig,
    mods: Mods,
    keymaps: Vec<KeyBinding>,
    /// Keycodes whose press was consumed; matching releases are also withheld from clients.
    suppressed_keys: HashSet<u32>,
    /// Modifier keycodes currently held and already forwarded to clients.
    forwarded_modifiers: HashSet<u32>,
    start: Instant,
}

impl InputState {
    pub fn new(comms: Comms, seat_state: Pin<&SeatState>) -> anyhow::Result<Self> {
        let xkb_config = XkbConfig::default();
        Ok(Self {
            comms,
            libinput: LibInput::new(seat_state)?,
            xkb: Xkb::new(&xkb_config)?,
            xkb_config,
            mods: Mods::default(),
            keymaps: Vec::new(),
            suppressed_keys: HashSet::new(),
            forwarded_modifiers: HashSet::new(),
            start: Instant::now(),
        })
    }

    /// Replace the XKB keymap from RMLVO names. On failure the previous keymap is kept.
    pub fn set_xkb(&mut self, config: XkbConfig) -> anyhow::Result<()> {
        self.xkb.set_names(&config)?;
        self.xkb_config = config;
        Ok(())
    }

    /// Sealed memfd with null-terminated xkb TEXT_V1 keymap for `wl_keyboard.keymap`.
    pub fn keymap_memfd(&self) -> anyhow::Result<KeymapMemfd> {
        self.xkb.keymap_memfd()
    }

    pub fn modifiers(&self) -> KeyboardModifiers {
        self.xkb.modifiers()
    }

    pub fn enable_seat(&mut self, seat_name: &str) -> anyhow::Result<()> {
        self.libinput.assign_seat(seat_name)?;
        self.libinput.resume()?;
        // Resume queues DEVICE_ADDED while we are inside another poll handler.
        // With oneshot POLL_ADD that edge is consumed; drain now so the fd can
        // go idle and re-arm on real input.
        self.dispatch(|_| {})
    }

    /// Suspend libinput when the session is disabled. Safe if never enabled.
    pub fn disable_seat(&mut self) -> anyhow::Result<()> {
        self.mods = Mods::default();
        self.suppressed_keys.clear();
        self.forwarded_modifiers.clear();
        self.xkb.reset()?;
        self.libinput.suspend()?;
        self.dispatch(|_| {})
    }

    pub fn add_keymap(
        &mut self,
        key: u32,
        mods: Mods,
        binding_id: String,
        on_release: bool,
        consume: bool,
    ) {
        self.keymaps.push(KeyBinding {
            key,
            mods,
            binding_id,
            on_release,
            consume,
        });
    }

    pub fn clear_keymaps(&mut self) {
        self.keymaps.clear();
    }

    /// Remove all bindings with the given id (normally at most one).
    pub fn remove_keymap(&mut self, binding_id: &str) {
        self.keymaps.retain(|b| b.binding_id != binding_id);
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.libinput.as_raw_fd()
    }

    pub fn set_output_geometry(&mut self, width: u32, height: u32) {
        self.libinput.set_coordinate_transform(width, height);
    }

    pub fn dispatch(&mut self, mut on_event: impl FnMut(SeatEvent)) -> anyhow::Result<()> {
        self.libinput.dispatch()?;
        while let Some(event) = self.libinput.next_event() {
            self.handle_input_event(event, false, &mut on_event);
        }
        Ok(())
    }

    /// Inject a named key press and release (e.g. `"Return"`, `"a"`, `"C-c"`).
    ///
    /// Control chords are accepted as `C-<key>`, `Ctrl+<key>`, or `Control+<key>`.
    pub fn inject_key_name(
        &mut self,
        name: &str,
        on_event: &mut impl FnMut(SeatEvent),
    ) -> anyhow::Result<()> {
        let (ctrl, key_name) = split_ctrl_chord(name);
        let keysym = Xkb::keysym_from_name(key_name)?;
        if ctrl {
            self.inject_key(libinput::bindings::KEY_LEFTCTRL, true, on_event);
        }
        let result = self.inject_keysym(keysym, on_event);
        if ctrl {
            self.inject_key(libinput::bindings::KEY_LEFTCTRL, false, on_event);
        }
        result
    }

    /// Inject a UTF-8 string as individual key presses.
    pub fn inject_type_text(
        &mut self,
        text: &str,
        on_event: &mut impl FnMut(SeatEvent),
    ) -> anyhow::Result<()> {
        for ch in text.chars() {
            let keysym = Xkb::keysym_from_char(ch)?;
            self.inject_keysym(keysym, on_event)?;
        }
        Ok(())
    }

    /// Move the pointer to absolute compositor coordinates.
    pub fn inject_pointer_move(&mut self, x: f64, y: f64, on_event: &mut impl FnMut(SeatEvent)) {
        let time_msec = self.now_msec();
        on_event(SeatEvent::Pointer(PointerEvent::Absolute {
            time_msec,
            x,
            y,
        }));
    }

    /// Press and release a pointer button at the given coordinates.
    pub fn inject_pointer_click(
        &mut self,
        x: f64,
        y: f64,
        button: u32,
        on_event: &mut impl FnMut(SeatEvent),
    ) {
        self.inject_pointer_move(x, y, on_event);
        self.inject_pointer_button(button, true, on_event);
        self.inject_pointer_button(button, false, on_event);
    }

    fn inject_keysym(
        &mut self,
        keysym: u32,
        on_event: &mut impl FnMut(SeatEvent),
    ) -> anyhow::Result<()> {
        let Some(press) = self.xkb.evdev_key_for_keysym(keysym) else {
            anyhow::bail!("No key binding for keysym {keysym}");
        };
        if press.shift {
            self.inject_key(libinput::bindings::KEY_LEFTSHIFT, true, on_event);
        }
        self.inject_key(press.evdev_keycode, true, on_event);
        self.inject_key(press.evdev_keycode, false, on_event);
        if press.shift {
            self.inject_key(libinput::bindings::KEY_LEFTSHIFT, false, on_event);
        }
        Ok(())
    }

    fn inject_key(&mut self, key: u32, pressed: bool, on_event: &mut impl FnMut(SeatEvent)) {
        let state = if pressed {
            KEY_STATE_PRESSED
        } else {
            libinput::KEY_STATE_RELEASED
        };
        self.handle_key(key, state, true, on_event);
    }

    fn inject_pointer_button(
        &mut self,
        button: u32,
        pressed: bool,
        on_event: &mut impl FnMut(SeatEvent),
    ) {
        let time_msec = self.now_msec();
        on_event(SeatEvent::Pointer(PointerEvent::Button {
            time_msec,
            button,
            pressed,
        }));
    }

    fn handle_input_event(
        &mut self,
        event: InputEvent,
        synthetic: bool,
        on_event: &mut impl FnMut(SeatEvent),
    ) {
        match event {
            InputEvent::KeyboardKey { key, state } => {
                self.handle_key(key, state, synthetic, on_event);
            }
            InputEvent::PointerMotion {
                dx,
                dy,
                dx_unaccel,
                dy_unaccel,
            } => {
                let time_msec = self.now_msec();
                on_event(SeatEvent::Pointer(PointerEvent::Motion {
                    time_msec,
                    dx,
                    dy,
                    dx_unaccel,
                    dy_unaccel,
                }));
            }
            InputEvent::PointerAbsolute { x, y } => {
                let time_msec = self.now_msec();
                on_event(SeatEvent::Pointer(PointerEvent::Absolute {
                    time_msec,
                    x,
                    y,
                }));
            }
            InputEvent::PointerButton { button, pressed } => {
                let time_msec = self.now_msec();
                on_event(SeatEvent::Pointer(PointerEvent::Button {
                    time_msec,
                    button,
                    pressed,
                }));
            }
            InputEvent::PointerAxis { axis, value } => {
                let time_msec = self.now_msec();
                on_event(SeatEvent::Pointer(PointerEvent::Axis {
                    time_msec,
                    axis,
                    value: value as f32,
                }));
            }
            InputEvent::TouchDown { id, x, y } => {
                let time_msec = self.now_msec();
                on_event(SeatEvent::Touch(TouchEvent::Down {
                    time_msec,
                    id,
                    x,
                    y,
                }));
            }
            InputEvent::TouchUp { id } => {
                let time_msec = self.now_msec();
                on_event(SeatEvent::Touch(TouchEvent::Up { time_msec, id }));
            }
            InputEvent::TouchMotion { id, x, y } => {
                let time_msec = self.now_msec();
                on_event(SeatEvent::Touch(TouchEvent::Motion {
                    time_msec,
                    id,
                    x,
                    y,
                }));
            }
            InputEvent::TouchCancel => {
                on_event(SeatEvent::Touch(TouchEvent::Cancel));
            }
            InputEvent::TouchFrame => {
                on_event(SeatEvent::Touch(TouchEvent::Frame));
            }
        }
    }

    fn now_msec(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    fn handle_key(
        &mut self,
        key: u32,
        state: u32,
        synthetic: bool,
        on_event: &mut impl FnMut(SeatEvent),
    ) {
        let pressed = state == KEY_STATE_PRESSED;
        if !synthetic && pressed {
            // Hardcoded: Ctrl+Alt+F1..F12 switches VT; bare F1 exits.
            if self.mods.ctrl && self.mods.alt {
                if let Some(vt) = fn_key_to_vt(key) {
                    self.comms.main(MainMessage::SwitchVt(vt));
                    self.suppressed_keys.insert(key);
                    return;
                }
            }
            if key == libinput::bindings::KEY_F1 {
                self.comms.main(MainMessage::Shutdown);
                self.suppressed_keys.insert(key);
                return;
            }
        }
        let mods_changed = self.xkb.update_key(key, pressed);
        if pressed {
            let keysym = self.xkb.key_get_one_sym(key);
            match Xkb::keysym_get_name(keysym) {
                Ok(name) => debug!("xkb keysym: {name} (evdev key={key})"),
                Err(err) => debug!("xkb keysym lookup failed for key={key}: {err:#}"),
            }
        }

        if is_modifier_key(key) {
            update_modifier(key, pressed, &mut self.mods);
        }

        let mut consumed = false;
        if !synthetic {
            let match_mods = mods_for_binding_match(key, self.mods);
            let on_release = !pressed;
            let activations =
                select_binding_activations(&self.keymaps, key, match_mods, on_release);
            for (binding_id, consume) in activations {
                debug!(
                    "Key binding activated: key={key} mods={match_mods:?} on_release={on_release} consume={consume} id={binding_id}"
                );
                self.comms
                    .dbus(DbusMessage::EmitBindingActivated(binding_id));
                if consume {
                    consumed = true;
                }
            }
        }

        if pressed {
            if consumed {
                self.suppressed_keys.insert(key);
            }
        } else if self.suppressed_keys.remove(&key) {
            // Press was consumed: never deliver the matching release to clients.
            consumed = true;
        }

        let time_msec = self.now_msec();
        if !consumed {
            on_event(SeatEvent::Keyboard(KeyboardEvent::Key {
                time_msec,
                key,
                pressed,
            }));
            if is_modifier_key(key) {
                if pressed {
                    self.forwarded_modifiers.insert(key);
                } else {
                    self.forwarded_modifiers.remove(&key);
                }
            }
        }

        // Consumed chords already delivered modifier presses to the client. Release
        // those modifiers client-side so they cannot combine into app shortcuts
        // (e.g. wezterm Alt+1) around the focus change.
        if consumed && pressed && !self.forwarded_modifiers.is_empty() {
            let forwarded: Vec<u32> = self.forwarded_modifiers.drain().collect();
            for &mod_key in &forwarded {
                self.suppressed_keys.insert(mod_key);
                on_event(SeatEvent::Keyboard(KeyboardEvent::Key {
                    time_msec,
                    key: mod_key,
                    pressed: false,
                }));
                self.xkb.update_key(mod_key, false);
            }
            let client_modifiers = self.xkb.modifiers();
            for &mod_key in &forwarded {
                // Restore physical held state for further binding matches.
                self.xkb.update_key(mod_key, true);
            }
            debug!("xkb modifiers (client after consume): {client_modifiers:?}");
            on_event(SeatEvent::Keyboard(KeyboardEvent::Modifiers(
                client_modifiers,
            )));
        } else if mods_changed {
            let modifiers = self.xkb.modifiers();
            debug!("xkb modifiers: {modifiers:?}");
            on_event(SeatEvent::Keyboard(KeyboardEvent::Modifiers(modifiers)));
        }
    }
}

/// Mods used when matching bindings for `key`.
///
/// For modifier keys, that key's own modifier bit is cleared so bindings like
/// `Alt_L` with empty mods match on Alt press/release.
fn mods_for_binding_match(key: u32, mods: Mods) -> Mods {
    let mut match_mods = mods;
    if is_modifier_key(key) {
        update_modifier(key, false, &mut match_mods);
    }
    match_mods
}

/// True when every modifier required by `binding` is present in `pressed`.
///
/// Extra pressed modifiers are allowed (i3/sway-style subset match).
fn mods_is_subset(binding: Mods, pressed: Mods) -> bool {
    (!binding.ctrl || pressed.ctrl)
        && (!binding.alt || pressed.alt)
        && (!binding.shift || pressed.shift)
        && (!binding.logo || pressed.logo)
}

fn mod_count(mods: Mods) -> u32 {
    u32::from(mods.ctrl) + u32::from(mods.alt) + u32::from(mods.shift) + u32::from(mods.logo)
}

/// Select bindings for this key event.
///
/// Matching is subset-based (`binding.mods ⊆ pressed mods`). When several
/// bindings match, more-specific ones (higher modifier count) run first.
/// Registration order breaks ties. The first `consume: true` binding stops
/// further activations.
fn select_binding_activations(
    keymaps: &[KeyBinding],
    key: u32,
    match_mods: Mods,
    on_release: bool,
) -> Vec<(String, bool)> {
    let mut matched: Vec<(usize, &KeyBinding)> = keymaps
        .iter()
        .enumerate()
        .filter(|(_, binding)| {
            binding.key == key
                && binding.on_release == on_release
                && mods_is_subset(binding.mods, match_mods)
        })
        .collect();
    matched.sort_by(|(index_a, a), (index_b, b)| {
        mod_count(b.mods)
            .cmp(&mod_count(a.mods))
            .then_with(|| index_a.cmp(index_b))
    });

    let mut activations = Vec::new();
    for (_, binding) in matched {
        activations.push((binding.binding_id.clone(), binding.consume));
        if binding.consume {
            break;
        }
    }
    activations
}

/// Map Linux evdev `KEY_F1`..`KEY_F12` to VT numbers 1..12.
fn fn_key_to_vt(key: u32) -> Option<i32> {
    if (libinput::bindings::KEY_F1..=libinput::bindings::KEY_F12).contains(&key) {
        Some((key - libinput::bindings::KEY_F1 + 1) as i32)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_common_key_names_to_evdev_keycodes() {
        assert_eq!(evdev_keycode_from_name("m"), Some(50));
        assert_eq!(evdev_keycode_from_name("f1"), Some(59));
        assert_eq!(evdev_keycode_from_name("backspace"), Some(14));
        assert_eq!(
            evdev_keycode_from_name("Alt_L"),
            Some(libinput::bindings::KEY_LEFTALT)
        );
        assert_eq!(
            evdev_keycode_from_name("Tab"),
            Some(15) // KEY_TAB
        );
    }

    #[test]
    fn resolves_key_names_against_configured_layout() {
        let de = XkbConfig {
            layout: Some("de".into()),
            ..Default::default()
        };
        let us = XkbConfig {
            layout: Some("us".into()),
            ..Default::default()
        };
        // On German QWERTZ, keysym `z` is produced by the physical Y key (evdev 21).
        assert_eq!(evdev_keycode_from_name_with_xkb("z", &de), Some(21));
        assert_eq!(evdev_keycode_from_name_with_xkb("z", &us), Some(44));
    }

    #[test]
    fn splits_control_chord_prefixes() {
        assert_eq!(split_ctrl_chord("C-c"), (true, "c"));
        assert_eq!(split_ctrl_chord("Ctrl+c"), (true, "c"));
        assert_eq!(split_ctrl_chord("Control+Return"), (true, "Return"));
        assert_eq!(split_ctrl_chord("Return"), (false, "Return"));
        assert_eq!(split_ctrl_chord("c"), (false, "c"));
    }

    #[test]
    fn mods_for_binding_match_clears_own_modifier_bit() {
        let mods = Mods {
            alt: true,
            ctrl: true,
            ..Default::default()
        };
        let matched = mods_for_binding_match(libinput::bindings::KEY_LEFTALT, mods);
        assert!(!matched.alt);
        assert!(matched.ctrl);

        let tab_mods = mods_for_binding_match(15, mods); // KEY_TAB
        assert_eq!(tab_mods, mods);
    }

    #[test]
    fn select_binding_activations_stops_at_consume() {
        let bindings = [
            KeyBinding {
                key: 15,
                mods: Mods {
                    alt: true,
                    ..Default::default()
                },
                binding_id: "first".into(),
                on_release: false,
                consume: false,
            },
            KeyBinding {
                key: 15,
                mods: Mods {
                    alt: true,
                    ..Default::default()
                },
                binding_id: "second".into(),
                on_release: false,
                consume: true,
            },
            KeyBinding {
                key: 15,
                mods: Mods {
                    alt: true,
                    ..Default::default()
                },
                binding_id: "third".into(),
                on_release: false,
                consume: true,
            },
        ];
        let activations = select_binding_activations(
            &bindings,
            15,
            Mods {
                alt: true,
                ..Default::default()
            },
            false,
        );
        assert_eq!(
            activations,
            vec![("first".into(), false), ("second".into(), true),]
        );
    }

    #[test]
    fn select_binding_activations_prefers_more_modifiers() {
        let bindings = [
            KeyBinding {
                key: 30, // KEY_A
                mods: Mods {
                    shift: true,
                    ..Default::default()
                },
                binding_id: "shift-a".into(),
                on_release: false,
                consume: true,
            },
            KeyBinding {
                key: 30,
                mods: Mods {
                    shift: true,
                    alt: true,
                    ..Default::default()
                },
                binding_id: "shift-alt-a".into(),
                on_release: false,
                consume: true,
            },
        ];
        let activations = select_binding_activations(
            &bindings,
            30,
            Mods {
                shift: true,
                alt: true,
                ..Default::default()
            },
            false,
        );
        assert_eq!(activations, vec![("shift-alt-a".into(), true)]);
    }

    #[test]
    fn select_binding_activations_subset_match_without_more_specific() {
        let bindings = [KeyBinding {
            key: 30,
            mods: Mods {
                shift: true,
                ..Default::default()
            },
            binding_id: "shift-a".into(),
            on_release: false,
            consume: true,
        }];
        let activations = select_binding_activations(
            &bindings,
            30,
            Mods {
                shift: true,
                alt: true,
                ..Default::default()
            },
            false,
        );
        assert_eq!(activations, vec![("shift-a".into(), true)]);
    }

    #[test]
    fn select_binding_activations_filters_edge() {
        let bindings = [KeyBinding {
            key: 56,
            mods: Mods::default(),
            binding_id: "alt-up".into(),
            on_release: true,
            consume: false,
        }];
        assert!(select_binding_activations(&bindings, 56, Mods::default(), false).is_empty());
        assert_eq!(
            select_binding_activations(&bindings, 56, Mods::default(), true),
            vec![("alt-up".into(), false)]
        );
    }
}
