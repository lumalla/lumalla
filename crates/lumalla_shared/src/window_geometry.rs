/// Partial window geometry update. Only specified fields are applied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowGeometryUpdate {
    /// New x position in compositor space.
    pub x: Option<i32>,
    /// New y position in compositor space.
    pub y: Option<i32>,
    /// New client area width in pixels.
    pub width: Option<i32>,
    /// New client area height in pixels.
    pub height: Option<i32>,
}

/// Sentinel value for unset optional geometry fields over D-Bus.
pub const WINDOW_GEOMETRY_UNSET: i32 = i32::MIN;

/// Convert a D-Bus geometry field to an optional value.
pub fn geometry_field_from_dbus(value: i32) -> Option<i32> {
    if value == WINDOW_GEOMETRY_UNSET {
        None
    } else {
        Some(value)
    }
}

/// Convert an optional geometry field to its D-Bus representation.
pub fn geometry_field_to_dbus(value: Option<i32>) -> i32 {
    value.unwrap_or(WINDOW_GEOMETRY_UNSET)
}

impl WindowGeometryUpdate {
    /// Returns true when no geometry field is set.
    pub fn is_empty(&self) -> bool {
        self.x.is_none() && self.y.is_none() && self.width.is_none() && self.height.is_none()
    }
}
