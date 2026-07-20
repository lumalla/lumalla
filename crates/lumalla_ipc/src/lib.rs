//! D-Bus constants, types, and client proxy shared by the compositor and config.

#![warn(missing_docs)]

#[allow(missing_docs)] // zbus-generated proxy trait methods
mod window_manager;
pub mod types;

pub use window_manager::{WindowManager, WindowManagerHandler, WindowManagerProxy, signals};
pub use types::{
    DrmConnectorInfo, DrmDeviceInfo, DrmModeInfo, KeyBindingInfo, LayoutOutputInfo,
    LayoutSpacesInfo, ModsInfo, OutputInfo, WindowRuleInfo, ZoneInfo,
};

/// Well-known session bus name for the compositor.
pub const BUS_NAME: &str = "org.lumalla.wm";

/// Object path exported by the compositor.
pub const OBJECT_PATH: &str = "/org/lumalla/wm";

/// Primary control/query interface.
pub const INTERFACE_NAME: &str = "org.lumalla.WindowManager";
