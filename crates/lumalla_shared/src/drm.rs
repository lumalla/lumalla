use std::path::PathBuf;

/// A display mode advertised on a DRM connector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrmMode {
    /// Horizontal active pixels.
    pub width: u32,
    /// Vertical active pixels.
    pub height: u32,
    /// Vertical refresh rate in Hz (as reported by the kernel).
    pub refresh_hz: u32,
    /// Kernel mode name (e.g. `1920x1080`).
    pub name: String,
    /// Whether this is the connector's preferred mode.
    pub preferred: bool,
}

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
    /// Modes advertised by the kernel for this connector (usually empty if disconnected).
    pub modes: Vec<DrmMode>,
}

/// Snapshot of a DRM primary node and its connectors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrmDeviceState {
    /// Primary node path (e.g. `/dev/dri/card0`).
    pub path: PathBuf,
    /// Connectors on this device (empty until probed with an open fd).
    pub connectors: Vec<DrmConnector>,
}
