use std::collections::{HashMap, HashSet};

use lumalla_shared::KeymapMemfd;
use lumalla_wayland_protocol::{
    ClientConnection, ClientId, ObjectId,
    buffer::Writer,
    protocols::wayland::{
        WL_KEYBOARD_KEY_STATE_PRESSED, WL_KEYBOARD_KEY_STATE_RELEASED,
        WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1, WL_POINTER_BUTTON_STATE_PRESSED,
        WL_POINTER_BUTTON_STATE_RELEASED,
    },
    registry::InterfaceIndex,
};

use crate::{
    GlobalId, Globals,
    surface::{SurfaceError, SurfaceManager},
};

/// Active client cursor for compositor rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveCursor {
    pub client_id: ClientId,
    pub surface_id: ObjectId,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
}

pub struct SeatManager {
    has_main_seat: bool,
    known_seats: HashSet<String>,
    id_to_name: HashMap<GlobalId, String>,
    /// Sealed memfd of the xkb TEXT_V1 keymap, shared with all clients via SCM_RIGHTS.
    keymap: Option<KeymapMemfd>,
    modifiers: KeyboardModifiers,
    keyboards: Vec<SeatKeyboard>,
    pointers: Vec<SeatPointer>,
    touches: Vec<SeatTouch>,
    /// Output-local pointer position in pixels.
    pointer_x: f64,
    pointer_y: f64,
    output_width: u32,
    output_height: u32,
    /// Active touch points: seat slot -> (client, surface).
    active_touches: HashMap<i32, (ClientId, ObjectId)>,
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

struct SeatPointer {
    client_id: ClientId,
    id: ObjectId,
    version: u32,
    focus: Option<ObjectId>,
    cursor_surface: Option<ObjectId>,
    hotspot: (i32, i32),
    enter_serial: Option<u32>,
}

struct SeatTouch {
    client_id: ClientId,
    id: ObjectId,
    #[allow(dead_code)]
    version: u32,
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
            pointers: Vec::new(),
            touches: Vec::new(),
            pointer_x: 0.0,
            pointer_y: 0.0,
            output_width: 0,
            output_height: 0,
            active_touches: HashMap::new(),
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

    pub fn create_pointer(
        &mut self,
        client_id: ClientId,
        pointer_id: ObjectId,
        version: u32,
        writer: &mut Writer,
        focus_surface: Option<ObjectId>,
        surface_manager: &SurfaceManager,
    ) {
        let mut enter_serial = None;
        if let Some(surface) = focus_surface {
            enter_serial = Some(self.send_pointer_enter(
                writer,
                pointer_id,
                surface,
                surface_manager,
                client_id,
            ));
            if version >= 5 {
                writer.wl_pointer_frame(pointer_id);
            }
        }
        self.pointers.push(SeatPointer {
            client_id,
            id: pointer_id,
            version,
            focus: focus_surface,
            cursor_surface: None,
            hotspot: (0, 0),
            enter_serial,
        });
    }

    pub fn destroy_pointer(
        &mut self,
        client_id: ClientId,
        pointer_id: ObjectId,
        surface_manager: &mut SurfaceManager,
    ) {
        if let Some(pointer) = self
            .pointers
            .iter()
            .find(|p| p.client_id == client_id && p.id == pointer_id)
        {
            if let Some(cursor) = pointer.cursor_surface {
                let _ = surface_manager.clear_cursor_role(client_id, cursor);
            }
        }
        self.pointers
            .retain(|p| !(p.client_id == client_id && p.id == pointer_id));
    }

    pub fn create_touch(&mut self, client_id: ClientId, touch_id: ObjectId, version: u32) {
        self.touches.push(SeatTouch {
            client_id,
            id: touch_id,
            version,
        });
    }

    pub fn destroy_touch(&mut self, client_id: ClientId, touch_id: ObjectId) {
        self.touches
            .retain(|t| !(t.client_id == client_id && t.id == touch_id));
    }

    pub fn remove_client(&mut self, client_id: ClientId) {
        self.keyboards.retain(|kb| kb.client_id != client_id);
        self.pointers.retain(|p| p.client_id != client_id);
        self.touches.retain(|t| t.client_id != client_id);
        self.active_touches
            .retain(|_, (owner, _)| *owner != client_id);
    }

    pub fn next_serial(&mut self) -> u32 {
        self.serial.next_serial()
    }

    pub fn set_output_geometry(&mut self, width: u32, height: u32) {
        self.output_width = width;
        self.output_height = height;
        self.clamp_pointer();
    }

    pub fn pointer_position(&self) -> (f64, f64) {
        (self.pointer_x, self.pointer_y)
    }

    pub fn active_cursor(&self) -> Option<ActiveCursor> {
        let pointer = self
            .pointers
            .iter()
            .find(|p| p.focus.is_some() && p.cursor_surface.is_some())?;
        Some(ActiveCursor {
            client_id: pointer.client_id,
            surface_id: pointer.cursor_surface?,
            hotspot_x: pointer.hotspot.0,
            hotspot_y: pointer.hotspot.1,
        })
    }

    pub fn pointer_focus_for_client(&self, client_id: ClientId) -> Option<ObjectId> {
        self.pointers
            .iter()
            .find(|p| p.client_id == client_id && p.focus.is_some())
            .and_then(|p| p.focus)
    }

    pub fn set_cursor(
        &mut self,
        client_id: ClientId,
        pointer_id: ObjectId,
        serial: u32,
        surface: Option<ObjectId>,
        hotspot_x: i32,
        hotspot_y: i32,
        surface_manager: &mut SurfaceManager,
    ) -> Result<(), SurfaceError> {
        let pointer_index = self
            .pointers
            .iter()
            .position(|p| p.client_id == client_id && p.id == pointer_id);
        let Some(pointer_index) = pointer_index else {
            return Ok(());
        };
        if self.pointers[pointer_index].enter_serial != Some(serial) {
            // Protocol: ignore set_cursor when serial does not match latest enter.
            return Ok(());
        }

        let previous_cursor = self.pointers[pointer_index].cursor_surface;
        if previous_cursor == surface {
            self.pointers[pointer_index].hotspot = (hotspot_x, hotspot_y);
            return Ok(());
        }

        if let Some(new_surface) = surface {
            // Reject if another pointer already owns this cursor surface, or if the
            // surface has a non-cursor role.
            let owned_by_other = self.pointers.iter().any(|p| {
                p.cursor_surface == Some(new_surface)
                    && !(p.client_id == client_id && p.id == pointer_id)
            });
            if owned_by_other {
                return Err(SurfaceError::RoleAlreadyAssigned);
            }
            if surface_manager.surface_role_is_cursor(client_id, new_surface) {
                // Already a cursor; allow reclaiming if no other pointer owns it.
            } else {
                surface_manager.assign_cursor_role(client_id, new_surface)?;
            }
        }

        if let Some(old) = previous_cursor {
            let still_used = self.pointers.iter().enumerate().any(|(idx, p)| {
                idx != pointer_index && p.client_id == client_id && p.cursor_surface == Some(old)
            });
            if !still_used {
                let _ = surface_manager.clear_cursor_role(client_id, old);
            }
        }

        self.pointers[pointer_index].cursor_surface = surface;
        self.pointers[pointer_index].hotspot = (hotspot_x, hotspot_y);
        Ok(())
    }

    pub fn leave_keyboards_on_surface(
        &mut self,
        client_id: ClientId,
        surface: ObjectId,
        writer: &mut Writer,
    ) {
        let keyboards: Vec<ObjectId> = self
            .keyboards
            .iter()
            .filter(|kb| kb.client_id == client_id && kb.focus == Some(surface))
            .map(|kb| kb.id)
            .collect();
        for keyboard_id in keyboards {
            let serial = self.serial.next_serial();
            writer
                .wl_keyboard_leave(keyboard_id)
                .serial(serial)
                .surface(surface);
            if let Some(keyboard) = self
                .keyboards
                .iter_mut()
                .find(|kb| kb.client_id == client_id && kb.id == keyboard_id)
            {
                keyboard.focus = None;
            }
        }
    }

    pub fn leave_pointers_on_surface(
        &mut self,
        client_id: ClientId,
        surface: ObjectId,
        writer: &mut Writer,
    ) {
        let pointers: Vec<(ObjectId, u32)> = self
            .pointers
            .iter()
            .filter(|p| p.client_id == client_id && p.focus == Some(surface))
            .map(|p| (p.id, p.version))
            .collect();
        for (pointer_id, version) in pointers {
            let serial = self.serial.next_serial();
            writer
                .wl_pointer_leave(pointer_id)
                .serial(serial)
                .surface(surface);
            if version >= 5 {
                writer.wl_pointer_frame(pointer_id);
            }
            if let Some(pointer) = self
                .pointers
                .iter_mut()
                .find(|p| p.client_id == client_id && p.id == pointer_id)
            {
                pointer.focus = None;
                pointer.enter_serial = None;
            }
        }
    }

    pub fn focus_keyboards_on_surface(
        &mut self,
        client_id: ClientId,
        surface: ObjectId,
        writer: &mut Writer,
    ) {
        let modifiers = self.modifiers;
        let keyboards: Vec<(ObjectId, Option<ObjectId>)> = self
            .keyboards
            .iter()
            .filter(|kb| kb.client_id == client_id)
            .map(|kb| (kb.id, kb.focus))
            .collect();
        for (keyboard_id, previous_focus) in keyboards {
            if previous_focus == Some(surface) {
                continue;
            }
            if let Some(old_surface) = previous_focus {
                let serial = self.serial.next_serial();
                writer
                    .wl_keyboard_leave(keyboard_id)
                    .serial(serial)
                    .surface(old_surface);
            }
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

    pub fn handle_pointer_motion(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        surface_manager: &SurfaceManager,
        time_msec: u32,
        dx: f64,
        dy: f64,
    ) {
        self.pointer_x += dx;
        self.pointer_y += dy;
        self.clamp_pointer();
        self.update_pointer_focus_and_motion(clients, surface_manager, time_msec, true);
    }

    pub fn handle_pointer_absolute(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        surface_manager: &SurfaceManager,
        time_msec: u32,
        x: f64,
        y: f64,
    ) {
        self.pointer_x = x;
        self.pointer_y = y;
        self.clamp_pointer();
        self.update_pointer_focus_and_motion(clients, surface_manager, time_msec, true);
    }

    pub fn handle_pointer_button(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        _surface_manager: &SurfaceManager,
        time_msec: u32,
        button: u32,
        pressed: bool,
    ) {
        if pressed {
            if let Some((client_id, surface)) = self
                .pointers
                .iter()
                .find_map(|pointer| pointer.focus.map(|surface| (pointer.client_id, surface)))
            {
                if let Some(client) = clients.get_mut(&client_id) {
                    self.focus_keyboards_on_surface(client_id, surface, client.writer_mut());
                }
            }
        }
        let state = if pressed {
            WL_POINTER_BUTTON_STATE_PRESSED
        } else {
            WL_POINTER_BUTTON_STATE_RELEASED
        };
        let focused: Vec<(ClientId, ObjectId, u32)> = self
            .pointers
            .iter()
            .filter(|p| p.focus.is_some())
            .map(|p| (p.client_id, p.id, p.version))
            .collect();
        for (client_id, pointer_id, version) in focused {
            let Some(client) = clients.get_mut(&client_id) else {
                continue;
            };
            let serial = self.serial.next_serial();
            client
                .writer_mut()
                .wl_pointer_button(pointer_id)
                .serial(serial)
                .time(time_msec)
                .button(button)
                .state(state);
            if version >= 5 {
                client.writer_mut().wl_pointer_frame(pointer_id);
            }
        }
    }

    pub fn handle_pointer_axis(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        time_msec: u32,
        axis: u32,
        value: f32,
    ) {
        let focused: Vec<(ClientId, ObjectId, u32)> = self
            .pointers
            .iter()
            .filter(|p| p.focus.is_some())
            .map(|p| (p.client_id, p.id, p.version))
            .collect();
        for (client_id, pointer_id, version) in focused {
            let Some(client) = clients.get_mut(&client_id) else {
                continue;
            };
            client
                .writer_mut()
                .wl_pointer_axis(pointer_id)
                .time(time_msec)
                .axis(axis)
                .value(value);
            if version >= 5 {
                client.writer_mut().wl_pointer_frame(pointer_id);
            }
        }
    }

    pub fn handle_touch_down(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        surface_manager: &SurfaceManager,
        time_msec: u32,
        touch_id: i32,
        x: f64,
        y: f64,
    ) {
        let Some((client_id, surface)) =
            surface_manager.global_pointer_target(None, x, y)
        else {
            return;
        };
        self.active_touches.insert(touch_id, (client_id, surface));
        let touches: Vec<ObjectId> = self
            .touches
            .iter()
            .filter(|t| t.client_id == client_id)
            .map(|t| t.id)
            .collect();
        for object_id in touches {
            let Some(client) = clients.get_mut(&client_id) else {
                continue;
            };
            let (local_x, local_y) = surface_manager
                .surface_local_coords(client_id, surface, x, y)
                .unwrap_or((x as f32, y as f32));
            let serial = self.serial.next_serial();
            client
                .writer_mut()
                .wl_touch_down(object_id)
                .serial(serial)
                .time(time_msec)
                .surface(surface)
                .id(touch_id)
                .x(local_x)
                .y(local_y);
        }
    }

    pub fn handle_touch_up(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        time_msec: u32,
        touch_id: i32,
    ) {
        let Some((client_id, _)) = self.active_touches.remove(&touch_id) else {
            return;
        };
        let touches: Vec<ObjectId> = self
            .touches
            .iter()
            .filter(|t| t.client_id == client_id)
            .map(|t| t.id)
            .collect();
        for object_id in touches {
            let Some(client) = clients.get_mut(&client_id) else {
                continue;
            };
            let serial = self.serial.next_serial();
            client
                .writer_mut()
                .wl_touch_up(object_id)
                .serial(serial)
                .time(time_msec)
                .id(touch_id);
        }
    }

    pub fn handle_touch_motion(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        surface_manager: &SurfaceManager,
        time_msec: u32,
        touch_id: i32,
        x: f64,
        y: f64,
    ) {
        let Some((client_id, surface)) = self.active_touches.get(&touch_id).copied() else {
            return;
        };
        let touches: Vec<ObjectId> = self
            .touches
            .iter()
            .filter(|t| t.client_id == client_id)
            .map(|t| t.id)
            .collect();
        let (local_x, local_y) = surface_manager
            .surface_local_coords(client_id, surface, x, y)
            .unwrap_or((x as f32, y as f32));
        for object_id in touches {
            let Some(client) = clients.get_mut(&client_id) else {
                continue;
            };
            client
                .writer_mut()
                .wl_touch_motion(object_id)
                .time(time_msec)
                .id(touch_id)
                .x(local_x)
                .y(local_y);
        }
    }

    pub fn handle_touch_frame(&mut self, clients: &mut HashMap<ClientId, ClientConnection>) {
        let client_ids: HashSet<ClientId> = self.touches.iter().map(|t| t.client_id).collect();
        for client_id in client_ids {
            let touches: Vec<ObjectId> = self
                .touches
                .iter()
                .filter(|t| t.client_id == client_id)
                .map(|t| t.id)
                .collect();
            let Some(client) = clients.get_mut(&client_id) else {
                continue;
            };
            for object_id in touches {
                client.writer_mut().wl_touch_frame(object_id);
            }
        }
    }

    pub fn handle_touch_cancel(&mut self, clients: &mut HashMap<ClientId, ClientConnection>) {
        let client_ids: HashSet<ClientId> = self
            .active_touches
            .values()
            .map(|(client_id, _)| *client_id)
            .collect();
        self.active_touches.clear();
        for client_id in client_ids {
            let touches: Vec<ObjectId> = self
                .touches
                .iter()
                .filter(|t| t.client_id == client_id)
                .map(|t| t.id)
                .collect();
            let Some(client) = clients.get_mut(&client_id) else {
                continue;
            };
            for object_id in touches {
                client.writer_mut().wl_touch_cancel(object_id);
            }
        }
    }

    fn update_pointer_focus_and_motion(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        surface_manager: &SurfaceManager,
        time_msec: u32,
        send_motion: bool,
    ) {
        let preferred = self
            .pointers
            .iter()
            .find(|p| p.focus.is_some())
            .map(|p| p.client_id);
        let target =
            surface_manager.global_pointer_target(preferred, self.pointer_x, self.pointer_y);

        // Leave pointers whose focus no longer matches the target.
        let leave_list: Vec<(ClientId, ObjectId, ObjectId, u32)> = self
            .pointers
            .iter()
            .filter_map(|p| {
                let focus = p.focus?;
                let should_leave = match target {
                    Some((client_id, surface)) => p.client_id != client_id || focus != surface,
                    None => true,
                };
                should_leave.then_some((p.client_id, p.id, focus, p.version))
            })
            .collect();
        for (client_id, pointer_id, surface, version) in leave_list {
            if let Some(client) = clients.get_mut(&client_id) {
                let serial = self.serial.next_serial();
                client
                    .writer_mut()
                    .wl_pointer_leave(pointer_id)
                    .serial(serial)
                    .surface(surface);
                if version >= 5 {
                    client.writer_mut().wl_pointer_frame(pointer_id);
                }
            }
            if let Some(pointer) = self
                .pointers
                .iter_mut()
                .find(|p| p.client_id == client_id && p.id == pointer_id)
            {
                pointer.focus = None;
                pointer.enter_serial = None;
            }
        }

        let Some((target_client, target_surface)) = target else {
            return;
        };

        let (sx, sy) = surface_manager
            .surface_local_coords(target_client, target_surface, self.pointer_x, self.pointer_y)
            .unwrap_or((self.pointer_x as f32, self.pointer_y as f32));

        let enter_list: Vec<(ObjectId, u32, bool)> = self
            .pointers
            .iter()
            .filter(|p| p.client_id == target_client)
            .map(|p| (p.id, p.version, p.focus == Some(target_surface)))
            .collect();

        for (pointer_id, version, already_focused) in enter_list {
            let Some(client) = clients.get_mut(&target_client) else {
                continue;
            };
            if !already_focused {
                let serial = self.serial.next_serial();
                client
                    .writer_mut()
                    .wl_pointer_enter(pointer_id)
                    .serial(serial)
                    .surface(target_surface)
                    .surface_x(sx)
                    .surface_y(sy);
                if version >= 5 {
                    client.writer_mut().wl_pointer_frame(pointer_id);
                }
                if let Some(pointer) = self
                    .pointers
                    .iter_mut()
                    .find(|p| p.client_id == target_client && p.id == pointer_id)
                {
                    pointer.focus = Some(target_surface);
                    pointer.enter_serial = Some(serial);
                }
            } else if send_motion {
                client
                    .writer_mut()
                    .wl_pointer_motion(pointer_id)
                    .time(time_msec)
                    .surface_x(sx)
                    .surface_y(sy);
                if version >= 5 {
                    client.writer_mut().wl_pointer_frame(pointer_id);
                }
            }
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

    fn send_pointer_enter(
        &mut self,
        writer: &mut Writer,
        pointer_id: ObjectId,
        surface: ObjectId,
        surface_manager: &SurfaceManager,
        client_id: ClientId,
    ) -> u32 {
        let serial = self.serial.next_serial();
        let (surface_x, surface_y) = surface_manager
            .surface_local_coords(client_id, surface, self.pointer_x, self.pointer_y)
            .unwrap_or((self.pointer_x as f32, self.pointer_y as f32));
        writer
            .wl_pointer_enter(pointer_id)
            .serial(serial)
            .surface(surface)
            .surface_x(surface_x)
            .surface_y(surface_y);
        serial
    }

    fn clamp_pointer(&mut self) {
        if self.output_width == 0 || self.output_height == 0 {
            return;
        }
        let max_x = (self.output_width.saturating_sub(1)) as f64;
        let max_y = (self.output_height.saturating_sub(1)) as f64;
        self.pointer_x = self.pointer_x.clamp(0.0, max_x);
        self.pointer_y = self.pointer_y.clamp(0.0, max_y);
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

#[cfg(test)]
mod tests {
    use std::{
        num::NonZeroU32,
        os::{fd::AsRawFd, unix::net::UnixStream},
    };

    use lumalla_wayland_protocol::{ClientId, ObjectId, buffer::Writer};

    use super::*;
    use crate::surface::{ShellMode, SurfaceManager};

    fn client(id: u32) -> ClientId {
        ClientId::new(NonZeroU32::new(id).unwrap())
    }

    fn object(id: u32) -> ObjectId {
        ObjectId::new(NonZeroU32::new(id).unwrap())
    }

    fn writer() -> (UnixStream, Writer) {
        let (_receiver, sender) = UnixStream::pair().unwrap();
        let writer = Writer::new(sender.as_raw_fd());
        (sender, writer)
    }

    #[test]
    fn create_pointer_tracks_object() {
        let mut seat = SeatManager::default();
        let (_keep, mut writer) = writer();
        seat.create_pointer(client(1), object(10), 5, &mut writer, None, &SurfaceManager::default());
        assert_eq!(seat.pointers.len(), 1);
        assert_eq!(seat.pointers[0].id, object(10));
        seat.destroy_pointer(client(1), object(10), &mut SurfaceManager::default());
        assert!(seat.pointers.is_empty());
    }

    #[test]
    fn set_cursor_assigns_cursor_role() {
        let mut seat = SeatManager::default();
        let mut surfaces = SurfaceManager::default();
        let (_keep, mut writer) = writer();
        let client_id = client(1);
        let pointer = object(10);
        let surface = object(20);
        let cursor = object(21);

        surfaces.create_surface(client_id, surface);
        surfaces.create_surface(client_id, cursor);
        surfaces
            .create_shell_surface(client_id, object(30), surface)
            .unwrap();
        surfaces
            .set_shell_mode(client_id, object(30), ShellMode::Toplevel)
            .unwrap();
        surfaces.attach(client_id, surface, Some(object(40)), 0, 0, 1).unwrap();
        let _ = surfaces.commit(client_id, surface).unwrap();

        seat.create_pointer(client_id, pointer, 5, &mut writer, Some(surface), &surfaces);
        let enter_serial = seat.pointers[0].enter_serial.unwrap();

        seat.set_cursor(
            client_id,
            pointer,
            enter_serial,
            Some(cursor),
            1,
            2,
            &mut surfaces,
        )
        .unwrap();
        assert!(surfaces.surface_role_is_cursor(client_id, cursor));
        assert_eq!(seat.pointers[0].cursor_surface, Some(cursor));
        assert_eq!(seat.pointers[0].hotspot, (1, 2));
    }

    #[test]
    fn set_cursor_rejects_surface_with_other_role() {
        let mut seat = SeatManager::default();
        let mut surfaces = SurfaceManager::default();
        let (_keep, mut writer) = writer();
        let client_id = client(1);
        let pointer = object(10);
        let surface = object(20);

        surfaces.create_surface(client_id, surface);
        surfaces
            .create_shell_surface(client_id, object(30), surface)
            .unwrap();
        seat.create_pointer(client_id, pointer, 5, &mut writer, Some(surface), &surfaces);
        let enter_serial = seat.pointers[0].enter_serial.unwrap();

        let err = seat
            .set_cursor(
                client_id,
                pointer,
                enter_serial,
                Some(surface),
                0,
                0,
                &mut surfaces,
            )
            .unwrap_err();
        assert_eq!(err, SurfaceError::RoleAlreadyAssigned);
    }

    #[test]
    fn leave_pointers_on_surface_clears_focus() {
        let mut seat = SeatManager::default();
        let (_keep, mut writer) = writer();
        let client_id = client(1);
        let pointer = object(10);
        let surface = object(20);
        seat.create_pointer(
            client_id,
            pointer,
            5,
            &mut writer,
            Some(surface),
            &SurfaceManager::default(),
        );
        assert_eq!(seat.pointers[0].focus, Some(surface));
        seat.leave_pointers_on_surface(client_id, surface, &mut writer);
        assert!(seat.pointers[0].focus.is_none());
        assert!(seat.pointers[0].enter_serial.is_none());
    }

    #[test]
    fn touch_down_up_tracks_active_points() {
        let mut seat = SeatManager::default();
        let mut surfaces = SurfaceManager::default();
        let mut clients = HashMap::new();
        let (receiver, sender) = UnixStream::pair().unwrap();
        let client_id = client(1);
        let surface = object(20);
        surfaces.create_surface(client_id, surface);
        surfaces
            .create_shell_surface(client_id, object(30), surface)
            .unwrap();
        surfaces
            .set_shell_mode(client_id, object(30), ShellMode::Toplevel)
            .unwrap();
        surfaces.attach(client_id, surface, Some(object(40)), 0, 0, 1).unwrap();
        let _ = surfaces.commit(client_id, surface).unwrap();
        surfaces
            .set_committed_buffer_size(client_id, surface, 200, 200)
            .unwrap();

        seat.create_touch(client_id, object(50), 5);
        // ClientConnection is heavy; exercise SeatManager state without a full client map.
        let _ = (receiver, sender);
        assert!(seat.active_touches.is_empty());
        seat.handle_touch_down(&mut clients, &surfaces, 1, 7, 10.0, 20.0);
        // No client connection means events are skipped after tracking insert... actually
        // insert happens before client lookup, so active_touches should be set.
        assert_eq!(seat.active_touches.get(&7), Some(&(client_id, surface)));
        seat.handle_touch_up(&mut clients, 2, 7);
        assert!(seat.active_touches.is_empty());
    }
}
