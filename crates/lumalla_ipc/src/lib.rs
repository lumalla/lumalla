//! D-Bus constants, types, and client proxy shared by the compositor and config.

#![warn(missing_docs)]

pub mod types;
#[allow(missing_docs)] // zbus-generated proxy trait methods
mod window_manager;

pub use types::{
    DrmConnectorInfo, DrmDeviceInfo, DrmModeInfo, KeyBindingInfo, LayoutOutputInfo,
    LayoutSpacesInfo, ModsInfo, OutputConfigInfo, OutputInfo, WindowInfo, WindowRuleInfo, XkbInfo,
    ZoneInfo,
};
pub use window_manager::{WindowManager, WindowManagerHandler, WindowManagerProxy, signals};

/// Well-known session bus name for the compositor.
///
/// Debug builds use a distinct name so a release compositor and a debug
/// compositor can own names on the same session bus at once.
pub const BUS_NAME: &str = if cfg!(debug_assertions) {
    "org.lumalla.wm.debug"
} else {
    "org.lumalla.wm"
};

/// Object path exported by the compositor.
pub const OBJECT_PATH: &str = "/org/lumalla/wm";

/// Primary control/query interface.
pub const INTERFACE_NAME: &str = "org.lumalla.WindowManager";
