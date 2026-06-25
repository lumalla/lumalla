//! D-Bus serializable types for Lumalla.

#![warn(missing_docs)]

use lumalla_shared::Output;
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
