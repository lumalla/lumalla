/// Snapshot of a managed window for IPC and configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowState {
    /// Compositor-assigned window id.
    pub id: u32,
    /// Application id reported by the client.
    pub app_id: String,
    /// Window title reported by the client.
    pub title: String,
    /// X position in compositor space.
    pub x: i32,
    /// Y position in compositor space.
    pub y: i32,
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
    /// Whether this window currently has keyboard focus.
    pub focused: bool,
}
