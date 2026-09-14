//! D-Bus serializable types.

use lumalla_shared::{
    CompositionStrategy, DrmConnector, DrmDeviceState, DrmMode, Mods, Output, WindowRule,
    WindowState, Zone,
};
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
            connectors: device
                .connectors
                .iter()
                .map(DrmConnectorInfo::from)
                .collect(),
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

/// A view mapping global compositor space onto an output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ViewInfo {
    /// Unique name within the owning output.
    pub name: String,
    /// Source X in global compositor space.
    pub source_x: i32,
    /// Source Y in global compositor space.
    pub source_y: i32,
    /// Source width in global compositor space.
    pub source_width: i32,
    /// Source height in global compositor space.
    pub source_height: i32,
    /// Destination X in output-local coordinates.
    pub dest_x: i32,
    /// Destination Y in output-local coordinates.
    pub dest_y: i32,
    /// Destination width in output-local coordinates.
    pub dest_width: i32,
    /// Destination height in output-local coordinates.
    pub dest_height: i32,
}

impl From<&lumalla_shared::View> for ViewInfo {
    fn from(view: &lumalla_shared::View) -> Self {
        Self {
            name: view.name.clone(),
            source_x: view.source.0,
            source_y: view.source.1,
            source_width: view.source.2,
            source_height: view.source.3,
            dest_x: view.dest.0,
            dest_y: view.dest.1,
            dest_width: view.dest.2,
            dest_height: view.dest.3,
        }
    }
}

impl From<ViewInfo> for lumalla_shared::View {
    fn from(info: ViewInfo) -> Self {
        Self {
            name: info.name,
            source: (
                info.source_x,
                info.source_y,
                info.source_width,
                info.source_height,
            ),
            dest: (info.dest_x, info.dest_y, info.dest_width, info.dest_height),
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
    /// X position in the global layout (from the first view's source origin).
    pub x: i32,
    /// Y position in the global layout (from the first view's source origin).
    pub y: i32,
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
    /// Buffer scale factor advertised to clients.
    pub scale: i32,
    /// Refresh rate in mHz (Wayland `wl_output.mode` units).
    pub refresh_mhz: i32,
    /// Physical width in millimeters.
    pub physical_width_mm: i32,
    /// Physical height in millimeters.
    pub physical_height_mm: i32,
    /// Whether this is a config-created virtual output (no DRM connector).
    pub is_virtual: bool,
    /// Views composited onto this output (paint order: later draws on top).
    pub views: Vec<ViewInfo>,
}

impl From<&Output> for OutputInfo {
    fn from(output: &Output) -> Self {
        let (x, y) = output.location();
        Self {
            name: output.name.clone(),
            description: output.description.clone(),
            x,
            y,
            width: output.size.0,
            height: output.size.1,
            scale: output.scale,
            refresh_mhz: output.refresh_mhz,
            physical_width_mm: output.physical_width_mm,
            physical_height_mm: output.physical_height_mm,
            is_virtual: output.is_virtual,
            views: output.views.iter().map(ViewInfo::from).collect(),
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
            views: info.views.iter().cloned().map(Into::into).collect(),
            size: (info.width, info.height),
            scale: info.scale,
            refresh_mhz: info.refresh_mhz,
            physical_width_mm: info.physical_width_mm,
            physical_height_mm: info.physical_height_mm,
            is_virtual: info.is_virtual,
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

/// Zone definition exposed over D-Bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ZoneInfo {
    /// Zone name.
    pub name: String,
    /// Anchor X position.
    pub x: i32,
    /// Anchor Y position.
    pub y: i32,
    /// Whether this is the default zone.
    pub default: bool,
    /// Composition strategy name (`"free"`).
    pub composition: String,
    /// Default window width for the `free` strategy.
    pub default_width: i32,
    /// Default window height for the `free` strategy.
    pub default_height: i32,
}

impl From<Zone> for ZoneInfo {
    fn from(zone: Zone) -> Self {
        let (composition, default_width, default_height) = match zone.composition {
            CompositionStrategy::Free {
                default_width,
                default_height,
            } => (String::from("free"), default_width, default_height),
        };
        Self {
            name: zone.name,
            x: zone.anchor.0,
            y: zone.anchor.1,
            default: zone.default,
            composition,
            default_width,
            default_height,
        }
    }
}

impl From<ZoneInfo> for Zone {
    fn from(zone: ZoneInfo) -> Self {
        let composition = match zone.composition.as_str() {
            "free" | "" => CompositionStrategy::Free {
                default_width: zone.default_width,
                default_height: zone.default_height,
            },
            _ => CompositionStrategy::Free {
                default_width: zone.default_width,
                default_height: zone.default_height,
            },
        };
        Zone::new(zone.name, zone.x, zone.y, zone.default, composition)
    }
}

/// Window placement rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct WindowRuleInfo {
    /// Application id to match.
    pub app_id: String,
    /// Zone name to join (`""` = unset).
    pub zone: String,
    /// Default x position (`WINDOW_GEOMETRY_UNSET` = unset).
    pub x: i32,
    /// Default y position (`WINDOW_GEOMETRY_UNSET` = unset).
    pub y: i32,
    /// Default width (`WINDOW_GEOMETRY_UNSET` = unset).
    pub width: i32,
    /// Default height (`WINDOW_GEOMETRY_UNSET` = unset).
    pub height: i32,
}

impl From<WindowRule> for WindowRuleInfo {
    fn from(rule: WindowRule) -> Self {
        use lumalla_shared::geometry_field_to_dbus;
        Self {
            app_id: rule.app_id,
            zone: rule.zone.unwrap_or_default(),
            x: geometry_field_to_dbus(rule.x),
            y: geometry_field_to_dbus(rule.y),
            width: geometry_field_to_dbus(rule.width),
            height: geometry_field_to_dbus(rule.height),
        }
    }
}

impl From<WindowRuleInfo> for WindowRule {
    fn from(rule: WindowRuleInfo) -> Self {
        use lumalla_shared::geometry_field_from_dbus;
        Self {
            app_id: rule.app_id,
            zone: if rule.zone.is_empty() {
                None
            } else {
                Some(rule.zone)
            },
            x: geometry_field_from_dbus(rule.x),
            y: geometry_field_from_dbus(rule.y),
            width: geometry_field_from_dbus(rule.width),
            height: geometry_field_from_dbus(rule.height),
        }
    }
}

/// Window state exposed over D-Bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct WindowInfo {
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

impl From<&WindowState> for WindowInfo {
    fn from(window: &WindowState) -> Self {
        Self {
            id: window.id,
            app_id: window.app_id.clone(),
            title: window.title.clone(),
            x: window.x,
            y: window.y,
            width: window.width,
            height: window.height,
            focused: window.focused,
        }
    }
}

impl From<WindowState> for WindowInfo {
    fn from(window: WindowState) -> Self {
        Self::from(&window)
    }
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
    /// When true, fire on key release instead of press.
    pub on_release: bool,
    /// When true, stop further binding matches and do not forward the key to clients.
    pub consume: bool,
}

/// XKB RMLVO configuration. Empty strings select libxkbcommon defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct XkbInfo {
    /// XKB rules file name.
    pub rules: String,
    /// Keyboard model (e.g. `pc105`).
    pub model: String,
    /// Layout(s), e.g. `us` or `us,de`.
    pub layout: String,
    /// Variant(s) matching `layout`.
    pub variant: String,
    /// Options, e.g. `grp:alt_shift_toggle`.
    pub options: String,
}

impl From<&lumalla_shared::XkbConfig> for XkbInfo {
    fn from(config: &lumalla_shared::XkbConfig) -> Self {
        Self {
            rules: config.rules.clone().unwrap_or_default(),
            model: config.model.clone().unwrap_or_default(),
            layout: config.layout.clone().unwrap_or_default(),
            variant: config.variant.clone().unwrap_or_default(),
            options: config.options.clone().unwrap_or_default(),
        }
    }
}

impl From<XkbInfo> for lumalla_shared::XkbConfig {
    fn from(info: XkbInfo) -> Self {
        fn nonempty(s: String) -> Option<String> {
            if s.is_empty() { None } else { Some(s) }
        }
        Self {
            rules: nonempty(info.rules),
            model: nonempty(info.model),
            layout: nonempty(info.layout),
            variant: nonempty(info.variant),
            options: nonempty(info.options),
        }
    }
}
