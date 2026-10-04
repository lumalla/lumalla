//! Phase-local seat input handler: libinput/`SeatEvent` → Wayland + RenderSink + cursor listen.

use lumalla_shared::{
    BTN_LEFT, CursorListenKind, CursorListenPolicy, CursorListenSink, KeyboardEvent, Mods,
    PointerEvent, RenderSink, SeatEvent, TouchEvent,
};
use stumpalo::Arena;

use crate::{ConnectedClients, DisplayState};

/// Applies one seat-input batch with display, renderer, and cursor-listen peers in hand.
pub struct SeatInputHandler<'a> {
    pub state: &'a mut DisplayState,
    pub clients: &'a mut ConnectedClients,
    pub render: &'a mut dyn RenderSink,
    pub listen: &'a mut CursorListenPolicy,
    pub cursor_notify: &'a mut dyn CursorListenSink,
    pub pressed_mods: Mods,
}

impl SeatInputHandler<'_> {
    /// Apply one seat event. Returns whether compositor pointer coords changed.
    pub fn handle(&mut self, event: SeatEvent, arena: &Arena) -> bool {
        let mut pointer_changed = false;
        match event {
            SeatEvent::Keyboard(KeyboardEvent::Key {
                time_msec,
                key,
                pressed,
            }) => {
                self.state.handle_keyboard_key(
                    self.clients,
                    time_msec,
                    key,
                    pressed,
                    arena,
                );
            }
            SeatEvent::Keyboard(KeyboardEvent::Modifiers(modifiers)) => {
                self.state
                    .handle_keyboard_modifiers(self.clients, modifiers);
            }
            SeatEvent::Pointer(PointerEvent::Motion {
                time_msec,
                dx,
                dy,
                dx_unaccel,
                dy_unaccel,
            }) => {
                pointer_changed = true;
                let active = self
                    .listen
                    .listener_active(CursorListenKind::Move, self.pressed_mods);
                if active {
                    self.listen.accumulate_move(dx, dy);
                }
                if self
                    .listen
                    .consume_active(CursorListenKind::Move, self.pressed_mods)
                {
                    self.state.nudge_pointer(dx, dy);
                } else {
                    self.state.handle_pointer_motion(
                        self.clients,
                        time_msec,
                        dx,
                        dy,
                        dx_unaccel,
                        dy_unaccel,
                        arena,
                    );
                }
            }
            SeatEvent::Pointer(PointerEvent::Absolute { time_msec, x, y }) => {
                pointer_changed = true;
                let active = self
                    .listen
                    .listener_active(CursorListenKind::Move, self.pressed_mods);
                let origin = active.then(|| self.state.pointer_position());
                if self
                    .listen
                    .consume_active(CursorListenKind::Move, self.pressed_mods)
                {
                    self.state.set_pointer_position(x, y);
                } else {
                    self.state.handle_pointer_absolute(
                        self.clients,
                        time_msec,
                        x,
                        y,
                        arena,
                    );
                }
                if let Some((ox, oy)) = origin {
                    let (nx, ny) = self.state.pointer_position();
                    self.listen.accumulate_move(nx - ox, ny - oy);
                }
            }
            SeatEvent::Pointer(PointerEvent::Button {
                time_msec,
                button,
                pressed,
            }) => {
                let active = self
                    .listen
                    .listener_active(CursorListenKind::Click, self.pressed_mods);
                if !self
                    .listen
                    .consume_active(CursorListenKind::Click, self.pressed_mods)
                {
                    self.state.handle_pointer_button(
                        self.clients,
                        time_msec,
                        button,
                        pressed,
                        arena,
                    );
                }
                if active {
                    let (x, y) = self.state.pointer_position();
                    let lua_button = if button == BTN_LEFT { 0 } else { button };
                    self.cursor_notify
                        .cursor_clicked(x, y, lua_button, pressed);
                }
            }
            SeatEvent::Pointer(PointerEvent::Axis {
                time_msec,
                axis,
                value,
            }) => {
                let active = self
                    .listen
                    .listener_active(CursorListenKind::Scroll, self.pressed_mods);
                if !self
                    .listen
                    .consume_active(CursorListenKind::Scroll, self.pressed_mods)
                {
                    self.state.handle_pointer_axis(
                        self.clients,
                        time_msec,
                        axis,
                        value,
                        arena,
                    );
                }
                if active {
                    let (x, y) = self.state.pointer_position();
                    self.cursor_notify
                        .cursor_scrolled(x, y, axis, f64::from(value));
                }
            }
            SeatEvent::Touch(TouchEvent::Down {
                time_msec,
                id,
                x,
                y,
            }) => {
                self.state
                    .handle_touch_down(self.clients, time_msec, id, x, y, arena);
            }
            SeatEvent::Touch(TouchEvent::Up { time_msec, id }) => {
                self.state
                    .handle_touch_up(self.clients, time_msec, id, arena);
            }
            SeatEvent::Touch(TouchEvent::Motion {
                time_msec,
                id,
                x,
                y,
            }) => {
                self.state
                    .handle_touch_motion(self.clients, time_msec, id, x, y, arena);
            }
            SeatEvent::Touch(TouchEvent::Frame) => {
                self.state.handle_touch_frame(self.clients, arena);
            }
            SeatEvent::Touch(TouchEvent::Cancel) => {
                self.state.handle_touch_cancel(self.clients, arena);
            }
        }
        pointer_changed
    }

    /// Drain pending cursor-moved notifications after a batch that changed the pointer.
    pub fn flush_cursor_moved(&mut self) {
        let (x, y) = self.state.pointer_position();
        self.listen.flush_moved(self.cursor_notify, x, y);
    }

    /// Push rounded display pointer into [`RenderSink`]. Returns whether the sink may need present.
    pub fn sync_pointer_to_render(&mut self) -> bool {
        let (x, y) = self.state.pointer_position();
        if let Err(err) = self
            .render
            .update_pointer_position(x.round() as i32, y.round() as i32)
        {
            log::error!("Unable to update renderer pointer position: {err:#}");
            return false;
        }
        true
    }
}
