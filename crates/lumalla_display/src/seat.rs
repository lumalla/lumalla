use std::collections::{HashMap, HashSet};

use lumalla_shared::KeymapMemfd;
use lumalla_wayland_protocol::{
    ClientConnection, ClientId, ObjectId,
    buffer::Writer,
    protocols::wayland::{
        WL_KEYBOARD_KEY_STATE_PRESSED, WL_KEYBOARD_KEY_STATE_RELEASED,
        WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1,
    },
    registry::InterfaceIndex,
};

use crate::{GlobalId, Globals};

pub struct SeatManager {
    has_main_seat: bool,
    known_seats: HashSet<String>,
    id_to_name: HashMap<GlobalId, String>,
    /// Sealed memfd of the xkb TEXT_V1 keymap, shared with all clients via SCM_RIGHTS.
    keymap: Option<KeymapMemfd>,
    modifiers: KeyboardModifiers,
    keyboards: Vec<SeatKeyboard>,
    serial: Serial,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyboardModifiers {
    pub depressed: u32,
    pub latched: u32,
    pub locked: u32,
    pub group: u32,
}

struct SeatKeyboard {
    client_id: ClientId,
    id: ObjectId,
    focus: Option<ObjectId>,
}

impl Default for SeatManager {
    fn default() -> Self {
        Self {
            has_main_seat: false,
            known_seats: HashSet::new(),
            id_to_name: HashMap::new(),
            keymap: None,
            modifiers: KeyboardModifiers::default(),
            keyboards: Vec::new(),
            serial: Serial::new(),
        }
    }
}

impl SeatManager {
    pub fn set_keymap(&mut self, keymap: KeymapMemfd) {
        self.keymap = Some(keymap);
    }

    pub fn set_modifiers(&mut self, modifiers: KeyboardModifiers) {
        self.modifiers = modifiers;
    }

    /// Adds a seat with the given name to the seat manager.
    pub fn add_seat<'connection>(
        &mut self,
        seat_name: String,
        globals: &mut Globals,
        client_connections: impl Iterator<Item = &'connection mut ClientConnection>,
    ) {
        let is_new_seat = self.known_seats.insert(seat_name.clone());
        if is_new_seat {
            let id = globals.register(InterfaceIndex::WlSeat, client_connections);
            self.id_to_name.insert(id, seat_name);
        }
    }

    pub fn add_main_seat<'connection>(
        &mut self,
        seat_name: String,
        globals: &mut Globals,
        client_connections: impl Iterator<Item = &'connection mut ClientConnection>,
    ) -> anyhow::Result<()> {
        if self.has_main_seat {
            return Ok(());
        }
        self.add_seat(seat_name, globals, client_connections);
        self.has_main_seat = true;
        Ok(())
    }

    pub fn get_name(&self, id: GlobalId) -> Option<&str> {
        self.id_to_name.get(&id).map(|s| s.as_str())
    }

    pub fn create_keyboard(
        &mut self,
        client_id: ClientId,
        keyboard_id: ObjectId,
        version: u32,
        writer: &mut Writer,
        focus_surface: Option<ObjectId>,
    ) -> anyhow::Result<()> {
        self.send_keymap(writer, keyboard_id)?;
        if version >= 4 {
            writer
                .wl_keyboard_repeat_info(keyboard_id)
                .rate(25)
                .delay(600);
        }
        self.send_modifiers(writer, keyboard_id);
        if let Some(surface) = focus_surface {
            self.send_enter(writer, keyboard_id, surface);
            self.send_modifiers(writer, keyboard_id);
        }
        self.keyboards.push(SeatKeyboard {
            client_id,
            id: keyboard_id,
            focus: focus_surface,
        });
        Ok(())
    }

    pub fn destroy_keyboard(&mut self, client_id: ClientId, keyboard_id: ObjectId) {
        self.keyboards
            .retain(|kb| !(kb.client_id == client_id && kb.id == keyboard_id));
    }

    pub fn focus_keyboards_on_surface(
        &mut self,
        client_id: ClientId,
        surface: ObjectId,
        writer: &mut Writer,
    ) {
        let modifiers = self.modifiers;
        let keyboards: Vec<ObjectId> = self
            .keyboards
            .iter()
            .filter(|kb| kb.client_id == client_id && kb.focus.is_none())
            .map(|kb| kb.id)
            .collect();
        for keyboard_id in keyboards {
            let serial = self.serial.next_serial();
            writer
                .wl_keyboard_enter(keyboard_id)
                .serial(serial)
                .surface(surface)
                .keys(&[]);
            writer
                .wl_keyboard_modifiers(keyboard_id)
                .serial(serial)
                .mods_depressed(modifiers.depressed)
                .mods_latched(modifiers.latched)
                .mods_locked(modifiers.locked)
                .group(modifiers.group);
            if let Some(keyboard) = self
                .keyboards
                .iter_mut()
                .find(|kb| kb.client_id == client_id && kb.id == keyboard_id)
            {
                keyboard.focus = Some(surface);
            }
        }
    }

    pub fn handle_key(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        time_msec: u32,
        key: u32,
        pressed: bool,
    ) {
        let state = if pressed {
            WL_KEYBOARD_KEY_STATE_PRESSED
        } else {
            WL_KEYBOARD_KEY_STATE_RELEASED
        };
        let focused: Vec<(ClientId, ObjectId)> = self
            .keyboards
            .iter()
            .filter(|kb| kb.focus.is_some())
            .map(|kb| (kb.client_id, kb.id))
            .collect();
        for (client_id, keyboard_id) in focused {
            let Some(client) = clients.get_mut(&client_id) else {
                continue;
            };
            let serial = self.serial.next_serial();
            client
                .writer_mut()
                .wl_keyboard_key(keyboard_id)
                .serial(serial)
                .time(time_msec)
                .key(key)
                .state(state);
        }
    }

    pub fn handle_modifiers(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        modifiers: KeyboardModifiers,
    ) {
        self.modifiers = modifiers;
        for keyboard in &self.keyboards {
            let client_id = keyboard.client_id;
            let keyboard_id = keyboard.id;
            let Some(client) = clients.get_mut(&client_id) else {
                continue;
            };
            let serial = self.serial.next_serial();
            client
                .writer_mut()
                .wl_keyboard_modifiers(keyboard_id)
                .serial(serial)
                .mods_depressed(modifiers.depressed)
                .mods_latched(modifiers.latched)
                .mods_locked(modifiers.locked)
                .group(modifiers.group);
        }
    }

    fn send_keymap(&self, writer: &mut Writer, keyboard_id: ObjectId) -> anyhow::Result<()> {
        let Some(keymap) = self.keymap.as_ref() else {
            anyhow::bail!("Keyboard keymap has not been set");
        };
        if keymap.size() == 0 {
            anyhow::bail!("Keyboard keymap has not been set");
        }
        writer
            .wl_keyboard_keymap(keyboard_id)
            .format(WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1)
            .fd(keymap.as_raw_fd())
            .size(keymap.size());
        Ok(())
    }

    fn send_modifiers(&mut self, writer: &mut Writer, keyboard_id: ObjectId) {
        let serial = self.serial.next_serial();
        writer
            .wl_keyboard_modifiers(keyboard_id)
            .serial(serial)
            .mods_depressed(self.modifiers.depressed)
            .mods_latched(self.modifiers.latched)
            .mods_locked(self.modifiers.locked)
            .group(self.modifiers.group);
    }

    fn send_enter(&mut self, writer: &mut Writer, keyboard_id: ObjectId, surface: ObjectId) {
        let serial = self.serial.next_serial();
        writer
            .wl_keyboard_enter(keyboard_id)
            .serial(serial)
            .surface(surface)
            .keys(&[]);
    }
}

struct Serial {
    next_serial: u32,
}

impl Serial {
    fn new() -> Self {
        Self { next_serial: 1 }
    }

    fn next_serial(&mut self) -> u32 {
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1);
        serial
    }
}
