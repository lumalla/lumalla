//! D-Bus serializable types.

use std::collections::HashMap;

use lumalla_shared::{Mods, Output, WindowRule, Zone};
use serde::{Deserialize, Serialize};
use zbus::zvariant::Type;

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
