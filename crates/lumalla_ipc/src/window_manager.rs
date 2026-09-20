//! Window manager D-Bus interface and client proxy.

use zbus::{interface, object_server::SignalEmitter};

use crate::types::{
    DrmDeviceInfo, GuideInfo, KeyBindingInfo, ModsInfo, OutputConfigInfo, OutputInfo, ViewInfo,
    WindowInfo, WindowRuleInfo, XkbInfo, ZoneInfo,
};

/// Server-side handler for [`WindowManager`] D-Bus methods.
///
/// Implement this trait in the compositor; the generated interface delegates to it.
pub trait WindowManagerHandler: Send + Sync {
    /// Request compositor shutdown.
    fn quit(&mut self) -> zbus::fdo::Result<()>;

    /// Return the current output layout.
    fn get_outputs(&self) -> zbus::fdo::Result<Vec<OutputInfo>>;

    /// Return the current DRM primary nodes.
    fn get_drm_devices(&self) -> zbus::fdo::Result<Vec<DrmDeviceInfo>>;

    /// Select the Vulkan render device by DRM primary path (empty = auto).
    fn set_render_device(&mut self, path: &str) -> zbus::fdo::Result<()>;

    /// Merge per-connector output configuration.
    fn set_output_configs(&mut self, configs: Vec<OutputConfigInfo>) -> zbus::fdo::Result<()>;

    /// Add a logical Wayland output.
    fn add_output(&mut self, output: OutputInfo) -> zbus::fdo::Result<()>;

    /// Remove a logical Wayland output by name.
    fn remove_output(&mut self, name: &str) -> zbus::fdo::Result<()>;

    /// Append a view to a named output (replaces an existing view with the same name).
    fn add_view(&mut self, output: &str, view: ViewInfo) -> zbus::fdo::Result<()>;

    /// Remove a view from a named output by view name.
    fn remove_view(&mut self, output: &str, view: &str) -> zbus::fdo::Result<()>;

    /// Add or replace a zone by name.
    fn add_zone(&mut self, zone: ZoneInfo) -> zbus::fdo::Result<()>;

    /// Remove a zone by name.
    fn remove_zone(&mut self, name: &str) -> zbus::fdo::Result<()>;

    /// Add or replace a guide by name.
    fn add_guide(&mut self, guide: GuideInfo) -> zbus::fdo::Result<()>;

    /// Remove a guide by name.
    fn remove_guide(&mut self, name: &str) -> zbus::fdo::Result<()>;

    /// Remove all guides.
    fn clear_guides(&mut self) -> zbus::fdo::Result<()>;

    /// Return current guides.
    fn get_guides(&self) -> zbus::fdo::Result<Vec<GuideInfo>>;

    /// Add a window placement rule.
    fn add_window_rule(&mut self, rule: WindowRuleInfo) -> zbus::fdo::Result<()>;

    /// Remove all window placement rules.
    fn clear_window_rules(&mut self) -> zbus::fdo::Result<()>;

    /// Return managed windows.
    fn get_windows(&self) -> zbus::fdo::Result<Vec<WindowInfo>>;

    /// Return the focused window id, if any.
    fn get_focused_window(&self) -> zbus::fdo::Result<u32>;

    /// Update window geometry. Pass `id = 0` to target the focused window.
    /// Use `WINDOW_GEOMETRY_UNSET` for fields that should not be changed.
    fn set_window(
        &mut self,
        id: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> zbus::fdo::Result<()>;

    /// Assign a window to a zone. Pass `id = 0` to target the focused window.
    fn add_window_to_zone(&mut self, id: u32, zone: &str) -> zbus::fdo::Result<()>;

    /// Clear a window's zone membership. Pass `id = 0` to target the focused window.
    fn remove_window_from_zone(&mut self, id: u32) -> zbus::fdo::Result<()>;

    /// Focus a window. Pass `id = 0` to target the focused window.
    /// When `raise` is true, also raise the window after focusing.
    fn focus_window(&mut self, id: u32, raise: bool) -> zbus::fdo::Result<()>;

    /// Raise a window in stacking order without changing focus.
    /// Pass `id = 0` to target the focused window.
    fn raise_window(&mut self, id: u32) -> zbus::fdo::Result<()>;

    /// Ask a client to close a window (`xdg_toplevel.close`).
    /// Pass `id = 0` to target the focused window.
    fn close_window(&mut self, id: u32) -> zbus::fdo::Result<()>;

    /// Spawn a child process.
    fn spawn(&mut self, command: &str, args: Vec<String>) -> zbus::fdo::Result<()>;

    /// Set an environment variable for future spawns.
    fn set_extra_env(&mut self, name: &str, value: &str) -> zbus::fdo::Result<()>;

    /// Toggle the debug overlay.
    fn toggle_debug_ui(&mut self) -> zbus::fdo::Result<()>;

    /// Start a PipeWire video stream of a compositor region.
    ///
    /// Blocks until the PipeWire node is connected. Returns `(stream_id, node_id)`.
    /// Empty `name` uses the default `"Lumalla"`. `max_fps == 0` defaults to 30.
    fn start_pipewire_stream(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        name: &str,
        max_fps: u32,
    ) -> zbus::fdo::Result<(u32, u32)>;

    /// Stop a PipeWire video stream. Idempotent if the stream is already gone.
    fn stop_pipewire_stream(&mut self, stream_id: u32) -> zbus::fdo::Result<()>;

    /// Switch virtual terminal.
    fn vt_switch(&mut self, vt: i32) -> zbus::fdo::Result<()>;

    /// Register a key binding.
    fn map_key(&mut self, binding: KeyBindingInfo) -> zbus::fdo::Result<()>;

    /// Remove a key binding previously registered with [`Self::map_key`].
    fn unmap_key(&mut self, binding_id: &str) -> zbus::fdo::Result<()>;

    /// Clear all key bindings.
    fn clear_keymaps(&mut self) -> zbus::fdo::Result<()>;

    /// Set the XKB keyboard layout (RMLVO). Empty strings use defaults.
    fn set_xkb(&mut self, xkb: XkbInfo) -> zbus::fdo::Result<()>;

    /// Press and release a named key.
    fn inject_key(&mut self, name: &str) -> zbus::fdo::Result<()>;

    /// Type a UTF-8 string as key presses.
    fn type_text(&mut self, text: &str) -> zbus::fdo::Result<()>;

    /// Move the pointer to absolute compositor coordinates.
    fn inject_pointer_move(&mut self, x: f64, y: f64) -> zbus::fdo::Result<()>;

    /// Click a pointer button at absolute compositor coordinates.
    fn inject_pointer_click(&mut self, x: f64, y: f64, button: u32) -> zbus::fdo::Result<()>;

    /// Enable or disable cursor move / click / scroll signals to config clients.
    ///
    /// When disabled (default), the compositor does not emit
    /// [`signals::CURSOR_MOVED`] / [`signals::CURSOR_CLICKED`] /
    /// [`signals::CURSOR_SCROLLED`].
    ///
    /// When a `consume_*` flag is true **and** the listener's `mods_*` match the
    /// currently pressed modifiers, matching pointer events are delivered to the
    /// config listener but not forwarded to Wayland clients.
    fn set_cursor_listening(
        &mut self,
        listen_move: bool,
        listen_click: bool,
        listen_scroll: bool,
        consume_move: bool,
        consume_click: bool,
        consume_scroll: bool,
        mods_move: ModsInfo,
        mods_click: ModsInfo,
        mods_scroll: ModsInfo,
    ) -> zbus::fdo::Result<()>;

    /// Capture a compositor region to a PNG file at `path`.
    ///
    /// Blocks until the file is written or an error occurs.
    fn capture_screenshot(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        path: &str,
    ) -> zbus::fdo::Result<()>;

    /// Whether the compositor has already emitted the Ready signal.
    ///
    /// Config clients should check this after subscribing to Ready so a race
    /// where Ready fires before the subscription is not missed.
    fn is_ready(&self) -> zbus::fdo::Result<bool>;
}

/// D-Bus object exported at [`crate::OBJECT_PATH`].
pub struct WindowManager {
    handler: Box<dyn WindowManagerHandler>,
}

impl WindowManager {
    /// Wrap a [`WindowManagerHandler`] for export on the session bus.
    pub fn new(handler: impl WindowManagerHandler + 'static) -> Self {
        Self {
            handler: Box::new(handler),
        }
    }
}

/// Signal member names for emission outside the object server.
pub mod signals {
    /// Compositor finished startup and accepts configuration.
    pub const READY: &str = "Ready";
    /// Output layout changed.
    pub const OUTPUT_CHANGED: &str = "OutputChanged";
    /// DRM primary node list changed.
    pub const DRM_DEVICES_CHANGED: &str = "DrmDevicesChanged";
    /// A configured key binding was activated.
    pub const BINDING_ACTIVATED: &str = "BindingActivated";
    /// Pointer moved (only while cursor move listening is enabled).
    pub const CURSOR_MOVED: &str = "CursorMoved";
    /// Pointer button pressed or released (only while cursor click listening is enabled).
    pub const CURSOR_CLICKED: &str = "CursorClicked";
    /// Pointer scroll axis event (only while cursor scroll listening is enabled).
    pub const CURSOR_SCROLLED: &str = "CursorScrolled";
}

#[cfg_attr(
    debug_assertions,
    interface(
        name = "org.lumalla.WindowManager",
        proxy(
            default_service = "org.lumalla.wm.debug",
            default_path = "/org/lumalla/wm",
            gen_blocking = true,
            gen_async = false,
        )
    )
)]
#[cfg_attr(
    not(debug_assertions),
    interface(
        name = "org.lumalla.WindowManager",
        proxy(
            default_service = "org.lumalla.wm",
            default_path = "/org/lumalla/wm",
            gen_blocking = true,
            gen_async = false,
        )
    )
)]
impl WindowManager {
    fn quit(&mut self) -> zbus::fdo::Result<()> {
        self.handler.quit()
    }

    fn get_outputs(&self) -> zbus::fdo::Result<Vec<OutputInfo>> {
        self.handler.get_outputs()
    }

    fn get_drm_devices(&self) -> zbus::fdo::Result<Vec<DrmDeviceInfo>> {
        self.handler.get_drm_devices()
    }

    fn set_render_device(&mut self, path: &str) -> zbus::fdo::Result<()> {
        self.handler.set_render_device(path)
    }

    fn set_output_configs(&mut self, configs: Vec<OutputConfigInfo>) -> zbus::fdo::Result<()> {
        self.handler.set_output_configs(configs)
    }

    fn add_output(&mut self, output: OutputInfo) -> zbus::fdo::Result<()> {
        self.handler.add_output(output)
    }

    fn remove_output(&mut self, name: &str) -> zbus::fdo::Result<()> {
        self.handler.remove_output(name)
    }

    fn add_view(&mut self, output: &str, view: ViewInfo) -> zbus::fdo::Result<()> {
        self.handler.add_view(output, view)
    }

    fn remove_view(&mut self, output: &str, view: &str) -> zbus::fdo::Result<()> {
        self.handler.remove_view(output, view)
    }

    fn add_zone(&mut self, zone: ZoneInfo) -> zbus::fdo::Result<()> {
        self.handler.add_zone(zone)
    }

    fn remove_zone(&mut self, name: &str) -> zbus::fdo::Result<()> {
        self.handler.remove_zone(name)
    }

    fn add_guide(&mut self, guide: GuideInfo) -> zbus::fdo::Result<()> {
        self.handler.add_guide(guide)
    }

    fn remove_guide(&mut self, name: &str) -> zbus::fdo::Result<()> {
        self.handler.remove_guide(name)
    }

    fn clear_guides(&mut self) -> zbus::fdo::Result<()> {
        self.handler.clear_guides()
    }

    fn get_guides(&self) -> zbus::fdo::Result<Vec<GuideInfo>> {
        self.handler.get_guides()
    }

    fn add_window_rule(&mut self, rule: WindowRuleInfo) -> zbus::fdo::Result<()> {
        self.handler.add_window_rule(rule)
    }

    fn clear_window_rules(&mut self) -> zbus::fdo::Result<()> {
        self.handler.clear_window_rules()
    }

    fn get_windows(&self) -> zbus::fdo::Result<Vec<WindowInfo>> {
        self.handler.get_windows()
    }

    fn get_focused_window(&self) -> zbus::fdo::Result<u32> {
        self.handler.get_focused_window()
    }

    fn set_window(
        &mut self,
        id: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> zbus::fdo::Result<()> {
        self.handler.set_window(id, x, y, width, height)
    }

    fn add_window_to_zone(&mut self, id: u32, zone: &str) -> zbus::fdo::Result<()> {
        self.handler.add_window_to_zone(id, zone)
    }

    fn remove_window_from_zone(&mut self, id: u32) -> zbus::fdo::Result<()> {
        self.handler.remove_window_from_zone(id)
    }

    fn focus_window(&mut self, id: u32, raise: bool) -> zbus::fdo::Result<()> {
        self.handler.focus_window(id, raise)
    }

    fn raise_window(&mut self, id: u32) -> zbus::fdo::Result<()> {
        self.handler.raise_window(id)
    }

    fn close_window(&mut self, id: u32) -> zbus::fdo::Result<()> {
        self.handler.close_window(id)
    }

    fn spawn(&mut self, command: &str, args: Vec<String>) -> zbus::fdo::Result<()> {
        self.handler.spawn(command, args)
    }

    fn set_extra_env(&mut self, name: &str, value: &str) -> zbus::fdo::Result<()> {
        self.handler.set_extra_env(name, value)
    }

    fn toggle_debug_ui(&mut self) -> zbus::fdo::Result<()> {
        self.handler.toggle_debug_ui()
    }

    fn start_pipewire_stream(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        name: &str,
        max_fps: u32,
    ) -> zbus::fdo::Result<(u32, u32)> {
        self.handler
            .start_pipewire_stream(x, y, width, height, name, max_fps)
    }

    fn stop_pipewire_stream(&mut self, stream_id: u32) -> zbus::fdo::Result<()> {
        self.handler.stop_pipewire_stream(stream_id)
    }

    fn vt_switch(&mut self, vt: i32) -> zbus::fdo::Result<()> {
        self.handler.vt_switch(vt)
    }

    fn map_key(&mut self, binding: KeyBindingInfo) -> zbus::fdo::Result<()> {
        self.handler.map_key(binding)
    }

    fn unmap_key(&mut self, binding_id: &str) -> zbus::fdo::Result<()> {
        self.handler.unmap_key(binding_id)
    }

    fn clear_keymaps(&mut self) -> zbus::fdo::Result<()> {
        self.handler.clear_keymaps()
    }

    fn set_xkb(&mut self, xkb: XkbInfo) -> zbus::fdo::Result<()> {
        self.handler.set_xkb(xkb)
    }

    fn inject_key(&mut self, name: &str) -> zbus::fdo::Result<()> {
        self.handler.inject_key(name)
    }

    fn type_text(&mut self, text: &str) -> zbus::fdo::Result<()> {
        self.handler.type_text(text)
    }

    fn inject_pointer_move(&mut self, x: f64, y: f64) -> zbus::fdo::Result<()> {
        self.handler.inject_pointer_move(x, y)
    }

    fn inject_pointer_click(&mut self, x: f64, y: f64, button: u32) -> zbus::fdo::Result<()> {
        self.handler.inject_pointer_click(x, y, button)
    }

    fn set_cursor_listening(
        &mut self,
        listen_move: bool,
        listen_click: bool,
        listen_scroll: bool,
        consume_move: bool,
        consume_click: bool,
        consume_scroll: bool,
        mods_move: ModsInfo,
        mods_click: ModsInfo,
        mods_scroll: ModsInfo,
    ) -> zbus::fdo::Result<()> {
        self.handler.set_cursor_listening(
            listen_move,
            listen_click,
            listen_scroll,
            consume_move,
            consume_click,
            consume_scroll,
            mods_move,
            mods_click,
            mods_scroll,
        )
    }

    fn capture_screenshot(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        path: &str,
    ) -> zbus::fdo::Result<()> {
        self.handler.capture_screenshot(x, y, width, height, path)
    }

    fn is_ready(&self) -> zbus::fdo::Result<bool> {
        self.handler.is_ready()
    }

    #[zbus(signal)]
    async fn ready(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn output_changed(
        emitter: &SignalEmitter<'_>,
        outputs: Vec<OutputInfo>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn drm_devices_changed(
        emitter: &SignalEmitter<'_>,
        devices: Vec<DrmDeviceInfo>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn binding_activated(emitter: &SignalEmitter<'_>, binding_id: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn cursor_moved(
        emitter: &SignalEmitter<'_>,
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn cursor_clicked(
        emitter: &SignalEmitter<'_>,
        x: f64,
        y: f64,
        button: u32,
        pressed: bool,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn cursor_scrolled(
        emitter: &SignalEmitter<'_>,
        x: f64,
        y: f64,
        axis: u32,
        value: f64,
    ) -> zbus::Result<()>;
}
