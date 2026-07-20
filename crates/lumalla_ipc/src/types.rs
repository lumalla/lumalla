//! D-Bus serializable types.

use std::collections::HashMap;

use lumalla_shared::{DrmConnector, DrmDeviceState, DrmMode, Mods, Output, WindowRule, Zone};
use serde::{Deserialize, Serialize};
use zbus::zvariant::Type;

/// Display mode on a DRM connector, exposed over D-Bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DrmModeInfo {
    /// Horizontal active pixels.
    pub width: u32,
    /// Vertical active pixels.
    pub height: u32,
    /// Vertical refresh rate in Hz.
    pub refresh_hz: u32,
    /// Kernel mode name (e.g. `1920x1080`).
    pub name: String,
    /// Whether this is the connector's preferred mode.
    pub preferred: bool,
}

impl From<&DrmMode> for DrmModeInfo {
    fn from(mode: &DrmMode) -> Self {
        Self {
            width: mode.width,
            height: mode.height,
            refresh_hz: mode.refresh_hz,
            name: mode.name.clone(),
            preferred: mode.preferred,
        }
    }
}

/// DRM connector exposed over D-Bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DrmConnectorInfo {
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
    /// Available modes for this connector.
    pub modes: Vec<DrmModeInfo>,
}

impl From<&DrmConnector> for DrmConnectorInfo {
    fn from(connector: &DrmConnector) -> Self {
        Self {
            name: connector.name.clone(),
            connector_id: connector.connector_id,
            connector_type: connector.connector_type.clone(),
            connected: connector.connected,
            mm_width: connector.mm_width,
            mm_height: connector.mm_height,
            modes: connector.modes.iter().map(DrmModeInfo::from).collect(),
        }
    }
}

/// DRM primary node exposed over D-Bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DrmDeviceInfo {
    /// Primary node path (e.g. `/dev/dri/card0`).
    pub path: String,
    /// Connectors on this device.
    pub connectors: Vec<DrmConnectorInfo>,
    /// Whether this device is the currently selected Vulkan render device.
    pub selected_render_device: bool,
}

impl From<&DrmDeviceState> for DrmDeviceInfo {
    fn from(device: &DrmDeviceState) -> Self {
        Self {
            path: device.path.to_string_lossy().into_owned(),
            connectors: device.connectors.iter().map(DrmConnectorInfo::from).collect(),
            selected_render_device: device.selected_render_device,
        }
    }
}

impl From<DrmDeviceState> for DrmDeviceInfo {
    fn from(device: DrmDeviceState) -> Self {
        Self::from(&device)
    }
}

/// Per-connector presentation config exposed over D-Bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct OutputConfigInfo {
    /// Connector name (e.g. `HDMI-A-1`).
    pub name: String,
    /// Whether the connector should be driven when connected.
    pub enabled: bool,
    /// Kernel mode name; empty string means preferred/first mode.
    pub mode_name: String,
}

impl From<&lumalla_shared::OutputConfig> for OutputConfigInfo {
    fn from(config: &lumalla_shared::OutputConfig) -> Self {
        Self {
            name: config.name.clone(),
            enabled: config.enabled,
            mode_name: config.mode_name.clone().unwrap_or_default(),
        }
    }
}

impl From<OutputConfigInfo> for lumalla_shared::OutputConfig {
    fn from(info: OutputConfigInfo) -> Self {
        Self {
            name: info.name,
            enabled: info.enabled,
            mode_name: if info.mode_name.is_empty() {
                None
            } else {
                Some(info.mode_name)
            },
        }
    }
}

/// Output state exposed over D-Bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct OutputInfo {
    /// Connector/output name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// X position in the global layout.
    pub x: i32,
    /// Y position in the global layout.
    pub y: i32,
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
}

impl From<&Output> for OutputInfo {
    fn from(output: &Output) -> Self {
        Self {
            name: output.name.clone(),
            description: output.description.clone(),
            x: output.location.0,
            y: output.location.1,
            width: output.size.0,
            height: output.size.1,
        }
    }
}

impl From<Output> for OutputInfo {
    fn from(output: Output) -> Self {
        Self::from(&output)
    }
}

impl From<&OutputInfo> for Output {
    fn from(info: &OutputInfo) -> Self {
        Self {
            name: info.name.clone(),
            description: info.description.clone(),
            location: (info.x, info.y),
            size: (info.width, info.height),
        }
    }
}

/// Keyboard modifiers for D-Bus.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Type, Default)]
pub struct ModsInfo {
    /// Control modifier.
    pub ctrl: bool,
    /// Alt modifier.
    pub alt: bool,
    /// Shift modifier.
    pub shift: bool,
    /// Logo/super modifier.
    pub logo: bool,
}

impl From<Mods> for ModsInfo {
    fn from(mods: Mods) -> Self {
        Self {
            ctrl: mods.ctrl,
            alt: mods.alt,
            shift: mods.shift,
            logo: mods.logo,
        }
    }
}

impl From<ModsInfo> for Mods {
    fn from(mods: ModsInfo) -> Self {
        Self {
            ctrl: mods.ctrl,
            alt: mods.alt,
            shift: mods.shift,
            logo: mods.logo,
        }
    }
}

/// Zone geometry exposed over D-Bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ZoneInfo {
    /// Zone name.
    pub name: String,
    /// X position.
    pub x: i32,
    /// Y position.
    pub y: i32,
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
    /// Whether this is the default zone.
    pub default: bool,
}

impl From<Zone> for ZoneInfo {
    fn from(zone: Zone) -> Self {
        Self {
            name: zone.name,
            x: zone.geometry.0,
            y: zone.geometry.1,
            width: zone.geometry.2,
            height: zone.geometry.3,
            default: zone.default,
        }
    }
}

impl From<ZoneInfo> for Zone {
    fn from(zone: ZoneInfo) -> Self {
        Self::new(
            zone.name,
            zone.x,
            zone.y,
            zone.width,
            zone.height,
            zone.default,
        )
    }
}

/// Window placement rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct WindowRuleInfo {
    /// Application id to match.
    pub app_id: String,
    /// Target zone name.
    pub zone: String,
}

impl From<WindowRule> for WindowRuleInfo {
    fn from(rule: WindowRule) -> Self {
        Self {
            app_id: rule.app_id,
            zone: rule.zone,
        }
    }
}

impl From<WindowRuleInfo> for WindowRule {
    fn from(rule: WindowRuleInfo) -> Self {
        Self {
            app_id: rule.app_id,
            zone: rule.zone,
        }
    }
}

/// Output placement within a layout space.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct LayoutOutputInfo {
    /// Output name.
    pub name: String,
    /// X position.
    pub x: i32,
    /// Y position.
    pub y: i32,
}

/// A registered key binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct KeyBindingInfo {
    /// Binding identifier used with `BindingActivated` signals.
    pub binding_id: String,
    /// Key name.
    pub key: String,
    /// Required modifiers.
    pub mods: ModsInfo,
}

/// Layout spaces keyed by name.
pub type LayoutSpacesInfo = HashMap<String, Vec<LayoutOutputInfo>>;
