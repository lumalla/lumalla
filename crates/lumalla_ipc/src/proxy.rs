//! D-Bus client proxy for the compositor.

use zbus::proxy;

use super::types::{
    KeyBindingInfo, LayoutSpacesInfo, OutputInfo, WindowRuleInfo, ZoneInfo,
};

#[proxy(
    interface = "org.lumalla.WindowManager",
    default_service = "org.lumalla.wm",
    default_path = "/org/lumalla/wm",
    gen_blocking = true,
    gen_async = false
)]
pub trait WindowManager {
    fn quit(&self) -> zbus::fdo::Result<()>;

    fn get_outputs(&self) -> zbus::fdo::Result<Vec<OutputInfo>>;

    fn set_zones(&self, zones: Vec<ZoneInfo>) -> zbus::fdo::Result<()>;

    fn set_layout(&self, spaces: LayoutSpacesInfo) -> zbus::fdo::Result<()>;

    fn add_window_rule(&self, rule: WindowRuleInfo) -> zbus::fdo::Result<()>;

    fn close_current_window(&self) -> zbus::fdo::Result<()>;

    fn move_current_window_to_zone(&self, zone: &str) -> zbus::fdo::Result<()>;

    fn spawn(&self, command: &str, args: Vec<String>) -> zbus::fdo::Result<()>;

    fn focus_or_spawn(
        &self,
        app_id: &str,
        command: &str,
        args: Vec<String>,
    ) -> zbus::fdo::Result<()>;

    fn set_extra_env(&self, name: &str, value: &str) -> zbus::fdo::Result<()>;

    fn toggle_debug_ui(&self) -> zbus::fdo::Result<()>;

    fn start_video_stream(&self) -> zbus::fdo::Result<()>;

    fn vt_switch(&self, vt: i32) -> zbus::fdo::Result<()>;

    fn map_key(&self, binding: KeyBindingInfo) -> zbus::fdo::Result<()>;

    fn clear_keymaps(&self) -> zbus::fdo::Result<()>;

    #[zbus(signal)]
    fn ready(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn output_changed(&self, outputs: Vec<OutputInfo>) -> zbus::Result<()>;

    #[zbus(signal)]
    fn binding_activated(&self, binding_id: &str) -> zbus::Result<()>;
}
