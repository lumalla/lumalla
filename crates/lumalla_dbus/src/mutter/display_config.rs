//! Minimal `org.gnome.Mutter.DisplayConfig` for the GNOME portal monitor list.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use lumalla_ipc::types::OutputInfo;
use serde::Serialize;
use zbus::{
    fdo, interface,
    object_server::SignalEmitter,
    zvariant::{OwnedValue, Type},
};

/// Read-only display topology for portal-gnome's screencast picker.
pub(crate) struct DisplayConfig {
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
}

impl DisplayConfig {
    pub(crate) fn new(outputs: Arc<Mutex<Vec<OutputInfo>>>) -> Self {
        Self { outputs }
    }
}

#[derive(Serialize, Type)]
struct Monitor {
    names: (String, String, String, String),
    modes: Vec<Mode>,
    properties: HashMap<String, OwnedValue>,
}

#[derive(Serialize, Type)]
struct Mode {
    id: String,
    width: i32,
    height: i32,
    refresh_rate: f64,
    preferred_scale: f64,
    supported_scales: Vec<f64>,
    properties: HashMap<String, OwnedValue>,
}

#[derive(Serialize, Type)]
struct LogicalMonitor {
    x: i32,
    y: i32,
    scale: f64,
    transform: u32,
    is_primary: bool,
    monitors: Vec<(String, String, String, String)>,
    properties: HashMap<String, OwnedValue>,
}

#[interface(name = "org.gnome.Mutter.DisplayConfig")]
impl DisplayConfig {
    fn get_current_state(
        &self,
    ) -> fdo::Result<(
        u32,
        Vec<Monitor>,
        Vec<LogicalMonitor>,
        HashMap<String, OwnedValue>,
    )> {
        let mut monitors = Vec::new();
        let mut logical_monitors = Vec::new();

        let outputs = self.outputs.lock().unwrap();
        for (index, output) in outputs.iter().enumerate() {
            let connector = output.name.clone();
            let is_builtin = is_laptop_panel(&connector);
            let vendor = String::from("Lumalla");
            let product = if output.description.is_empty() {
                connector.clone()
            } else {
                output.description.clone()
            };
            let serial = connector.clone();
            let names = (connector.clone(), vendor, product.clone(), serial);

            let refresh_rate = if output.refresh_mhz > 0 {
                f64::from(output.refresh_mhz) / 1000.0
            } else {
                60.0
            };
            let scale = f64::from(output.scale.max(1));

            let mut mode_props = HashMap::new();
            mode_props.insert(String::from("is-current"), OwnedValue::from(true));
            mode_props.insert(String::from("is-preferred"), OwnedValue::from(true));

            let mode = Mode {
                id: format!("{}x{}@{refresh_rate:.3}", output.width, output.height),
                width: output.width,
                height: output.height,
                refresh_rate,
                preferred_scale: scale,
                supported_scales: if (scale - 1.0).abs() < f64::EPSILON {
                    vec![1.0]
                } else {
                    vec![1.0, scale]
                },
                properties: mode_props,
            };

            let mut properties = HashMap::new();
            properties.insert(
                String::from("display-name"),
                OwnedValue::from(zbus::zvariant::Str::from(if is_builtin {
                    String::from("Built-in display")
                } else if !output.description.is_empty() {
                    output.description.clone()
                } else {
                    product
                })),
            );
            properties.insert(String::from("is-builtin"), OwnedValue::from(is_builtin));
            if output.physical_width_mm > 0 {
                properties.insert(
                    String::from("width-mm"),
                    OwnedValue::from(output.physical_width_mm),
                );
            }
            if output.physical_height_mm > 0 {
                properties.insert(
                    String::from("height-mm"),
                    OwnedValue::from(output.physical_height_mm),
                );
            }

            if output.width > 0 && output.height > 0 {
                logical_monitors.push(LogicalMonitor {
                    x: output.x,
                    y: output.y,
                    scale,
                    transform: 0,
                    is_primary: index == 0,
                    monitors: vec![names.clone()],
                    properties: HashMap::new(),
                });
            }

            monitors.push(Monitor {
                names,
                modes: vec![mode],
                properties,
            });
        }

        monitors.sort_unstable_by(|a, b| a.names.0.cmp(&b.names.0));
        logical_monitors.sort_unstable_by(|a, b| a.monitors[0].0.cmp(&b.monitors[0].0));

        let properties = HashMap::from([(String::from("layout-mode"), OwnedValue::from(1u32))]);
        Ok((0, monitors, logical_monitors, properties))
    }

    fn apply_monitors_config(
        &self,
        _serial: u32,
        _method: u32,
        _logical_monitors: Vec<(i32, i32, f64, u32, bool, Vec<(String, String, HashMap<String, OwnedValue>)>, HashMap<String, OwnedValue>)>,
        _properties: HashMap<String, OwnedValue>,
    ) -> fdo::Result<()> {
        Err(fdo::Error::Failed(
            "ApplyMonitorsConfig is not supported".into(),
        ))
    }

    #[zbus(signal)]
    async fn monitors_changed(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(property)]
    fn power_save_mode(&self) -> i32 {
        -1
    }

    #[zbus(property)]
    fn set_power_save_mode(&self, _mode: i32) -> zbus::Result<()> {
        Err(zbus::Error::Unsupported)
    }

    #[zbus(property)]
    fn panel_orientation_managed(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn apply_monitors_config_allowed(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn night_light_supported(&self) -> bool {
        false
    }
}

fn is_laptop_panel(connector: &str) -> bool {
    let lower = connector.to_ascii_lowercase();
    lower.starts_with("edp") || lower.starts_with("lvds") || lower.starts_with("dsi")
}
