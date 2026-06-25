//! D-Bus constants shared by the compositor and clients.

#![warn(missing_docs)]

/// Well-known session bus name for the compositor.
pub const BUS_NAME: &str = "org.lumalla.wm";

/// Object path exported by the compositor.
pub const OBJECT_PATH: &str = "/org/lumalla/wm";

/// Primary control/query interface.
///
/// Interface names conventionally use PascalCase even when the bus name is lowercase
/// (compare `org.freedesktop.DBus.Introspectable` on bus name `org.freedesktop.DBus`).
pub const INTERFACE_NAME: &str = "org.lumalla.WindowManager";
