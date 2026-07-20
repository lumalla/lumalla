use std::path::PathBuf;

/// A DRM connector discovered on a primary node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrmConnector {
    /// Connector name (e.g. `HDMI-A-1`).
    pub name: String,
    /// DRM connector object id.
    pub connector_id: u32,
    /// Connector type name (e.g. `HDMI-A`, `eDP`).
    pub connector_type: String,
    /// Whether a sink is currently connected.
    pub connected: bool,
    /// Physical width in millimeters.
    pub mm_width: u32,
    /// Physical height in millimeters.
    pub mm_height: u32,
}

/// Snapshot of a DRM primary node and its connectors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrmDeviceState {
    /// Primary node path (e.g. `/dev/dri/card0`).
    pub path: PathBuf,
    /// Connectors on this device (empty until probed with an open fd).
    pub connectors: Vec<DrmConnector>,
}
