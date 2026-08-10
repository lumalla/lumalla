//! Input handling for Lumalla via libinput.

mod libinput;
mod xkb;

use std::{io, pin::Pin, time::Instant};

use log::debug;
use lumalla_seat::SeatState;
use lumalla_shared::{Comms, DbusMessage, KeymapMemfd, MainMessage, Mods};
use mio::{Interest, Registry, Token, event::Source};

use crate::libinput::{InputEvent, KEY_STATE_PRESSED, LibInput, is_modifier_key, update_modifier};
use crate::xkb::Xkb;

pub use xkb::XkbModifiers as KeyboardModifiers;

struct KeyBinding {
    key: u32,
    mods: Mods,
    binding_id: String,
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

pub struct InputState {
    comms: Comms,
    libinput: LibInput,
    xkb: Xkb,
    mods: Mods,
    keymaps: Vec<KeyBinding>,
    start: Instant,
}

impl InputState {
    pub fn new(comms: Comms, seat_state: Pin<&SeatState>) -> anyhow::Result<Self> {
        Ok(Self {
            comms,
            libinput: LibInput::new(seat_state)?,
            xkb: Xkb::new()?,
            mods: Mods::default(),
            keymaps: Vec::new(),
            start: Instant::now(),
        })
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
        // mio uses edge-triggered epoll, so that readability edge is missed and
        // the fd stays readable forever — no further LIBINPUT_TOKEN wakes.
        // Drain now so the fd can go idle and re-arm on real input.
        self.dispatch(|_| {})
    }

    pub fn disable_seat(&mut self) -> anyhow::Result<()> {
        self.mods = Mods::default();
        self.xkb.reset()?;
        self.libinput.suspend()?;
        self.dispatch(|_| {})
    }

    pub fn add_keymap(&mut self, key: u32, mods: Mods, binding_id: String) {
        self.keymaps.push(KeyBinding {
            key,
            mods,
            binding_id,
        });
    }

    pub fn clear_keymaps(&mut self) {
        self.keymaps.clear();
    }

    pub fn set_output_geometry(&mut self, width: u32, height: u32) {
        self.libinput.set_coordinate_transform(width, height);
    }

    pub fn dispatch(&mut self, mut on_event: impl FnMut(SeatEvent)) -> anyhow::Result<()> {
        self.libinput.dispatch()?;
        while let Some(event) = self.libinput.next_event() {
            match event {
                InputEvent::KeyboardKey { key, state } => {
                    self.handle_key(key, state, &mut on_event);
                }
                InputEvent::PointerMotion { dx, dy } => {
                    let time_msec = self.now_msec();
                    on_event(SeatEvent::Pointer(PointerEvent::Motion {
                        time_msec,
                        dx,
                        dy,
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
        Ok(())
    }

    fn now_msec(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    fn handle_key(
        &mut self,
        key: u32,
        state: u32,
        on_event: &mut impl FnMut(SeatEvent),
    ) {
        let pressed = state == KEY_STATE_PRESSED;
        if pressed {
            // Hardcoded: Ctrl+Alt+F1..F12 switches VT; bare F1 exits.
            if self.mods.ctrl && self.mods.alt {
                if let Some(vt) = fn_key_to_vt(key) {
                    self.comms.main(MainMessage::SwitchVt(vt));
                    return;
                }
            }
            if key == libinput::bindings::KEY_F1 {
                self.comms.main(MainMessage::Shutdown);
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

        let time_msec = self.now_msec();
        on_event(SeatEvent::Keyboard(KeyboardEvent::Key {
            time_msec,
            key,
            pressed,
        }));
        if mods_changed {
            let modifiers = self.xkb.modifiers();
            debug!("xkb modifiers: {modifiers:?}");
            on_event(SeatEvent::Keyboard(KeyboardEvent::Modifiers(modifiers)));
        }

        if is_modifier_key(key) {
            update_modifier(key, pressed, &mut self.mods);
            return;
        }
        if !pressed {
            return;
        }
        let binding_id = self
            .keymaps
            .iter()
            .find(|binding| binding.key == key && binding.mods == self.mods)
            .map(|binding| binding.binding_id.clone());
        if let Some(binding_id) = binding_id {
            debug!(
                "Key binding activated: key={key} mods={:?} id={binding_id}",
                self.mods
            );
            self.comms
                .dbus(DbusMessage::EmitBindingActivated(binding_id));
        }
    }
}

/// Map Linux evdev `KEY_F1`..`KEY_F12` to VT numbers 1..12.
fn fn_key_to_vt(key: u32) -> Option<i32> {
    if (libinput::bindings::KEY_F1..=libinput::bindings::KEY_F12).contains(&key) {
        Some((key - libinput::bindings::KEY_F1 + 1) as i32)
    } else {
        None
    }
}

impl Source for InputState {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        self.libinput.register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        self.libinput.reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        self.libinput.deregister(registry)
    }
}
