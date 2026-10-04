//! Phase-local collaboration for seat input (libinput → Wayland / renderer / config).

use crate::Mods;

/// Linux input button code for the left mouse button.
pub const BTN_LEFT: u32 = 0x110;

/// XKB modifier mask state for `wl_keyboard.modifiers`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyboardModifiers {
    pub depressed: u32,
    pub latched: u32,
    pub locked: u32,
    pub group: u32,
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

/// Which cursor-listen channel a policy gate applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorListenKind {
    Move,
    Click,
    Scroll,
}

/// Config-facing cursor listener side effects during the input phase.
pub trait CursorListenSink {
    fn cursor_moved(&mut self, x: f64, y: f64, dx: f64, dy: f64);
    fn cursor_clicked(&mut self, x: f64, y: f64, button: u32, pressed: bool);
    fn cursor_scrolled(&mut self, x: f64, y: f64, axis: u32, value: f64);
}

/// Cursor-listen / consume policy owned by the event loop and passed into seat handling.
#[derive(Debug, Default, Clone)]
pub struct CursorListenPolicy {
    pub listen_move: bool,
    pub listen_click: bool,
    pub listen_scroll: bool,
    pub consume_move: bool,
    pub consume_click: bool,
    pub consume_scroll: bool,
    pub move_mods: Mods,
    pub click_mods: Mods,
    pub scroll_mods: Mods,
    pub pending_dx: f64,
    pub pending_dy: f64,
}

impl CursorListenPolicy {
    pub fn listener_active(&self, kind: CursorListenKind, pressed_mods: Mods) -> bool {
        let (listening, required) = match kind {
            CursorListenKind::Move => (self.listen_move, self.move_mods),
            CursorListenKind::Click => (self.listen_click, self.click_mods),
            CursorListenKind::Scroll => (self.listen_scroll, self.scroll_mods),
        };
        listening && mods_is_subset(required, pressed_mods)
    }

    pub fn consume_active(&self, kind: CursorListenKind, pressed_mods: Mods) -> bool {
        let consume = match kind {
            CursorListenKind::Move => self.consume_move,
            CursorListenKind::Click => self.consume_click,
            CursorListenKind::Scroll => self.consume_scroll,
        };
        consume && self.listener_active(kind, pressed_mods)
    }

    pub fn accumulate_move(&mut self, dx: f64, dy: f64) {
        self.pending_dx += dx;
        self.pending_dy += dy;
    }

    /// Drain pending relative motion and notify the sink when non-zero.
    pub fn flush_moved(&mut self, sink: &mut dyn CursorListenSink, x: f64, y: f64) {
        if self.pending_dx == 0.0 && self.pending_dy == 0.0 {
            return;
        }
        let dx = self.pending_dx;
        let dy = self.pending_dy;
        self.pending_dx = 0.0;
        self.pending_dy = 0.0;
        sink.cursor_moved(x, y, dx, dy);
    }

    pub fn set_listening(
        &mut self,
        listen_move: bool,
        listen_click: bool,
        listen_scroll: bool,
        consume_move: bool,
        consume_click: bool,
        consume_scroll: bool,
        move_mods: Mods,
        click_mods: Mods,
        scroll_mods: Mods,
    ) {
        self.listen_move = listen_move;
        self.listen_click = listen_click;
        self.listen_scroll = listen_scroll;
        self.consume_move = consume_move;
        self.consume_click = consume_click;
        self.consume_scroll = consume_scroll;
        self.move_mods = move_mods;
        self.click_mods = click_mods;
        self.scroll_mods = scroll_mods;
    }
}

/// True when every requested modifier in `required` is present in `pressed`.
pub fn mods_is_subset(required: Mods, pressed: Mods) -> bool {
    (!required.ctrl || pressed.ctrl)
        && (!required.alt || pressed.alt)
        && (!required.shift || pressed.shift)
        && (!required.logo || pressed.logo)
}
