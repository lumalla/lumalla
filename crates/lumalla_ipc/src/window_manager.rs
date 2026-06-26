//! Window manager D-Bus interface and client proxy.

use zbus::{interface, object_server::SignalEmitter};

use crate::types::{
    KeyBindingInfo, LayoutSpacesInfo, OutputInfo, WindowRuleInfo, ZoneInfo,
};

/// Server-side handler for [`WindowManager`] D-Bus methods.
///
/// Implement this trait in the compositor; the generated interface delegates to it.
pub trait WindowManagerHandler: Send + Sync {
    /// Request compositor shutdown.
    fn quit(&mut self) -> zbus::fdo::Result<()>;

    /// Return the current output layout.
    fn get_outputs(&self) -> zbus::fdo::Result<Vec<OutputInfo>>;

    /// Replace zone definitions.
    fn set_zones(&mut self, zones: Vec<ZoneInfo>) -> zbus::fdo::Result<()>;

    /// Replace workspace layout.
    fn set_layout(&mut self, spaces: LayoutSpacesInfo) -> zbus::fdo::Result<()>;

    /// Add a window placement rule.
    fn add_window_rule(&mut self, rule: WindowRuleInfo) -> zbus::fdo::Result<()>;

    /// Close the focused window.
    fn close_current_window(&mut self) -> zbus::fdo::Result<()>;

    /// Move the focused window to a zone.
    fn move_current_window_to_zone(&mut self, zone: &str) -> zbus::fdo::Result<()>;

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

    fn set_zones(&mut self, zones: Vec<ZoneInfo>) -> zbus::fdo::Result<()> {
        self.handler.set_zones(zones)
    }

    fn set_layout(&mut self, spaces: LayoutSpacesInfo) -> zbus::fdo::Result<()> {
        self.handler.set_layout(spaces)
    }

    fn add_window_rule(&mut self, rule: WindowRuleInfo) -> zbus::fdo::Result<()> {
        self.handler.add_window_rule(rule)
    }

    fn close_current_window(&mut self) -> zbus::fdo::Result<()> {
        self.handler.close_current_window()
    }

    fn move_current_window_to_zone(&mut self, zone: &str) -> zbus::fdo::Result<()> {
        self.handler.move_current_window_to_zone(zone)
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

    #[zbus(signal)]
    async fn ready(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn output_changed(
        emitter: &SignalEmitter<'_>,
        outputs: Vec<OutputInfo>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn binding_activated(
        emitter: &SignalEmitter<'_>,
        binding_id: &str,
    ) -> zbus::Result<()>;
}
