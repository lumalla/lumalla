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
        self.libinput.resume()
    }

    pub fn disable_seat(&mut self) -> anyhow::Result<()> {
        self.mods = Mods::default();
        self.xkb.reset()?;
        self.libinput.suspend()
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

    pub fn dispatch(
        &mut self,
        mut on_keyboard_event: impl FnMut(KeyboardEvent),
    ) -> anyhow::Result<()> {
        self.libinput.dispatch()?;
        while let Some(event) = self.libinput.next_event() {
            match event {
                InputEvent::KeyboardKey { key, state } => {
                    self.handle_key(key, state, &mut on_keyboard_event);
                }
            }
        }
        Ok(())
    }

    fn handle_key(
        &mut self,
        key: u32,
        state: u32,
        on_keyboard_event: &mut impl FnMut(KeyboardEvent),
    ) {
        if key == libinput::bindings::KEY_F1 {
            self.comms.main(MainMessage::Shutdown);
            return;
        }
        let pressed = state == KEY_STATE_PRESSED;
        let mods_changed = self.xkb.update_key(key, pressed);
        if pressed {
            let keysym = self.xkb.key_get_one_sym(key);
            match Xkb::keysym_get_name(keysym) {
                Ok(name) => debug!("xkb keysym: {name} (evdev key={key})"),
                Err(err) => debug!("xkb keysym lookup failed for key={key}: {err:#}"),
            }
        }

        let time_msec = self.start.elapsed().as_millis() as u32;
        on_keyboard_event(KeyboardEvent::Key {
            time_msec,
            key,
            pressed,
        });
        if mods_changed {
            let modifiers = self.xkb.modifiers();
            debug!("xkb modifiers: {modifiers:?}");
            on_keyboard_event(KeyboardEvent::Modifiers(modifiers));
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
