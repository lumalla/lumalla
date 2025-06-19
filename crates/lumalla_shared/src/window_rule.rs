/// Represents a window rule
#[derive(Debug)]
pub struct WindowRule {
    /// The app_id of the window to which this rule applies
    pub app_id: String,
    /// The zone to which the window should be moved to
    pub zone: String,
}
