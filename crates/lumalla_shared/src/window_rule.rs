/// Default placement for windows matching an application id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowRule {
    /// Application id to match (`xdg_toplevel.app_id`).
    pub app_id: String,
    /// Default x position.
    pub x: Option<i32>,
    /// Default y position.
    pub y: Option<i32>,
    /// Default width.
    pub width: Option<i32>,
    /// Default height.
    pub height: Option<i32>,
}

impl WindowRule {
    /// Geometry fields carried by this rule.
    pub fn geometry(&self) -> super::WindowGeometryUpdate {
        super::WindowGeometryUpdate {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }
}
