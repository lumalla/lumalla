//! Window manager D-Bus interface and client proxy.

use zbus::{interface, object_server::SignalEmitter};

use crate::types::{
    DrmDeviceInfo, KeyBindingInfo, LayoutSpacesInfo, OutputConfigInfo, OutputInfo, WindowInfo,
    WindowRuleInfo, ZoneInfo,
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

    /// Replace zone definitions.
    fn set_zones(&mut self, zones: Vec<ZoneInfo>) -> zbus::fdo::Result<()>;

    /// Replace workspace layout.
    fn set_layout(&mut self, spaces: LayoutSpacesInfo) -> zbus::fdo::Result<()>;

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

    /// Spawn a child process.
    fn spawn(&mut self, command: &str, args: Vec<String>) -> zbus::fdo::Result<()>;

    /// Focus an app or spawn it if missing.
    fn focus_or_spawn(
        &mut self,
        app_id: &str,
        command: &str,
        args: Vec<String>,
    ) -> zbus::fdo::Result<()>;

    /// Set an environment variable for future spawns.
    fn set_extra_env(&mut self, name: &str, value: &str) -> zbus::fdo::Result<()>;

    /// Toggle the debug overlay.
    fn toggle_debug_ui(&mut self) -> zbus::fdo::Result<()>;

    /// Start the video stream.
    fn start_video_stream(&mut self) -> zbus::fdo::Result<()>;

    /// Switch virtual terminal.
    fn vt_switch(&mut self, vt: i32) -> zbus::fdo::Result<()>;

    /// Register a key binding.
    fn map_key(&mut self, binding: KeyBindingInfo) -> zbus::fdo::Result<()>;

    /// Clear all key bindings.
    fn clear_keymaps(&mut self) -> zbus::fdo::Result<()>;

    /// Press and release a named key.
    fn inject_key(&mut self, name: &str) -> zbus::fdo::Result<()>;

    /// Type a UTF-8 string as key presses.
    fn type_text(&mut self, text: &str) -> zbus::fdo::Result<()>;

    /// Move the pointer to absolute compositor coordinates.
    fn inject_pointer_move(&mut self, x: f64, y: f64) -> zbus::fdo::Result<()>;

    /// Click a pointer button at absolute compositor coordinates.
    fn inject_pointer_click(&mut self, x: f64, y: f64, button: u32) -> zbus::fdo::Result<()>;

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
}

#[interface(
    name = "org.lumalla.WindowManager",
    proxy(
        default_service = "org.lumalla.wm",
        default_path = "/org/lumalla/wm",
        gen_blocking = true,
        gen_async = false,
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

    fn set_zones(&mut self, zones: Vec<ZoneInfo>) -> zbus::fdo::Result<()> {
        self.handler.set_zones(zones)
    }

    fn set_layout(&mut self, spaces: LayoutSpacesInfo) -> zbus::fdo::Result<()> {
        self.handler.set_layout(spaces)
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

    fn spawn(&mut self, command: &str, args: Vec<String>) -> zbus::fdo::Result<()> {
        self.handler.spawn(command, args)
    }

    fn focus_or_spawn(
        &mut self,
        app_id: &str,
        command: &str,
        args: Vec<String>,
    ) -> zbus::fdo::Result<()> {
        self.handler.focus_or_spawn(app_id, command, args)
    }

    fn set_extra_env(&mut self, name: &str, value: &str) -> zbus::fdo::Result<()> {
        self.handler.set_extra_env(name, value)
    }

    fn toggle_debug_ui(&mut self) -> zbus::fdo::Result<()> {
        self.handler.toggle_debug_ui()
    }

    fn start_video_stream(&mut self) -> zbus::fdo::Result<()> {
        self.handler.start_video_stream()
    }

    fn vt_switch(&mut self, vt: i32) -> zbus::fdo::Result<()> {
        self.handler.vt_switch(vt)
    }

    fn map_key(&mut self, binding: KeyBindingInfo) -> zbus::fdo::Result<()> {
        self.handler.map_key(binding)
    }

    fn clear_keymaps(&mut self) -> zbus::fdo::Result<()> {
        self.handler.clear_keymaps()
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
}
