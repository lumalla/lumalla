//! Compositor-side implementation of the window manager D-Bus API.

use std::{
    collections::HashMap,
    process::Command,
    sync::{Arc, Mutex},
};

use log::{error, info, warn};
use lumalla_input::evdev_keycode_from_name;
use lumalla_ipc::{
    INTERFACE_NAME, KeyBindingInfo, OBJECT_PATH, WindowManagerHandler,
    types::{
        DrmDeviceInfo, LayoutSpacesInfo, OutputConfigInfo, OutputInfo, WindowInfo, WindowRuleInfo,
        ZoneInfo,
    },
};
use lumalla_shared::{
    Comms, InjectedInput, MainMessage, Mods, Output, WindowGeometryUpdate, WindowState,
    geometry_field_from_dbus,
};
use std::path::PathBuf;
use zbus::blocking::Connection;

pub(crate) struct ServiceState {
    pub comms: Comms,
    pub outputs: Arc<Mutex<Vec<OutputInfo>>>,
    pub output_lookup: Arc<Mutex<HashMap<String, Output>>>,
    pub drm_devices: Arc<Mutex<Vec<DrmDeviceInfo>>>,
    pub wayland_display: Arc<Mutex<Option<String>>>,
    pub extra_env: Arc<Mutex<HashMap<String, String>>>,
    pub keymaps: Arc<Mutex<Vec<KeyBindingInfo>>>,
    pub windows: Arc<Mutex<Vec<WindowState>>>,
}

pub(crate) struct CompositorHandler {
    pub state: Arc<ServiceState>,
}

impl WindowManagerHandler for CompositorHandler {
    fn quit(&mut self) -> zbus::fdo::Result<()> {
        info!("Quit requested over D-Bus");
        self.state.comms.main(MainMessage::Shutdown);
        Ok(())
    }

    fn get_outputs(&self) -> zbus::fdo::Result<Vec<OutputInfo>> {
        Ok(self.state.outputs.lock().unwrap().clone())
    }

    fn get_drm_devices(&self) -> zbus::fdo::Result<Vec<DrmDeviceInfo>> {
        Ok(self.state.drm_devices.lock().unwrap().clone())
    }

    fn set_render_device(&mut self, path: &str) -> zbus::fdo::Result<()> {
        let device = if path.is_empty() {
            None
        } else {
            Some(PathBuf::from(path))
        };
        info!("Set render device over D-Bus: {path:?}");
        self.state.comms.main(MainMessage::SetRenderDevice(device));
        Ok(())
    }

    fn set_output_configs(&mut self, configs: Vec<OutputConfigInfo>) -> zbus::fdo::Result<()> {
        info!("Set output configs over D-Bus: {} entries", configs.len());
        self.state.comms.main(MainMessage::SetOutputConfigs(
            configs.into_iter().map(Into::into).collect(),
        ));
        Ok(())
    }

    fn add_output(&mut self, info: OutputInfo) -> zbus::fdo::Result<()> {
        let output = Output::from(&info);
        {
            let mut lookup = self.state.output_lookup.lock().unwrap();
            if lookup.contains_key(&output.name) {
                return Err(zbus::fdo::Error::Failed(format!(
                    "Output already exists: {}",
                    output.name
                )));
            }
            lookup.insert(output.name.clone(), output.clone());
            self.state.outputs.lock().unwrap().push(info);
        }
        info!("Add output over D-Bus: {}", output.name);
        self.state.comms.main(MainMessage::AddOutput(output));
        Ok(())
    }

    fn remove_output(&mut self, name: &str) -> zbus::fdo::Result<()> {
        {
            let mut lookup = self.state.output_lookup.lock().unwrap();
            if lookup.remove(name).is_none() {
                return Err(zbus::fdo::Error::Failed(format!("Unknown output: {name}")));
            }
            self.state
                .outputs
                .lock()
                .unwrap()
                .retain(|output| output.name != name);
        }
        info!("Remove output over D-Bus: {name}");
        self.state.comms.main(MainMessage::RemoveOutput {
            name: name.to_owned(),
        });
        Ok(())
    }

    fn set_zones(&mut self, zones: Vec<ZoneInfo>) -> zbus::fdo::Result<()> {
        let _ = zones;
        // self.state.comms.display(DisplayMessage::SetZones(
        //     zones.into_iter().map(Into::into).collect(),
        // ));
        Ok(())
    }

    fn set_layout(&mut self, spaces: LayoutSpacesInfo) -> zbus::fdo::Result<()> {
        let _outputs = self.state.output_lookup.lock().unwrap();
        let _ = spaces;
        // self.state.comms.display(DisplayMessage::SetLayout {
        //     spaces: spaces
        //         .into_iter()
        //         .map(|(name, layout_outputs)| {
        //             (
        //                 name,
        //                 layout_outputs
        //                     .into_iter()
        //                     .filter_map(|layout_output| {
        //                         let Some(output) = outputs.get(&layout_output.name) else {
        //                             warn!("Output not found: {}", layout_output.name);
        //                             return None;
        //                         };
        //                         let mut output = output.clone();
        //                         output.set_location(layout_output.x, layout_output.y);
        //                         Some(output)
        //                     })
        //                     .collect(),
        //             )
        //         })
        //         .collect(),
        // });
        Ok(())
    }

    fn add_window_rule(&mut self, rule: WindowRuleInfo) -> zbus::fdo::Result<()> {
        self.state
            .comms
            .main(MainMessage::AddWindowRule(rule.into()));
        Ok(())
    }

    fn clear_window_rules(&mut self) -> zbus::fdo::Result<()> {
        self.state.comms.main(MainMessage::ClearWindowRules);
        Ok(())
    }

    fn get_windows(&self) -> zbus::fdo::Result<Vec<WindowInfo>> {
        Ok(self
            .state
            .windows
            .lock()
            .unwrap()
            .iter()
            .map(WindowInfo::from)
            .collect())
    }

    fn get_focused_window(&self) -> zbus::fdo::Result<u32> {
        Ok(self
            .state
            .windows
            .lock()
            .unwrap()
            .iter()
            .find(|window| window.focused)
            .map(|window| window.id)
            .unwrap_or(0))
    }

    fn set_window(
        &mut self,
        id: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> zbus::fdo::Result<()> {
        self.state.comms.main(MainMessage::SetWindow {
            id: if id == 0 { None } else { Some(id) },
            geometry: WindowGeometryUpdate {
                x: geometry_field_from_dbus(x),
                y: geometry_field_from_dbus(y),
                width: geometry_field_from_dbus(width),
                height: geometry_field_from_dbus(height),
            },
            user_initiated: true,
        });
        Ok(())
    }

    fn spawn(&mut self, command: &str, args: Vec<String>) -> zbus::fdo::Result<()> {
        spawn_process(
            command,
            &args,
            &self.state.wayland_display,
            &self.state.extra_env,
        );
        Ok(())
    }

    fn focus_or_spawn(
        &mut self,
        app_id: &str,
        command: &str,
        args: Vec<String>,
    ) -> zbus::fdo::Result<()> {
        let _ = (app_id, command, args);
        // self.state.comms.display(DisplayMessage::FocusOrSpawn {
        //     app_id: app_id.to_string(),
        //     command: command.to_string(),
        //     args,
        // });
        Ok(())
    }

    fn set_extra_env(&mut self, name: &str, value: &str) -> zbus::fdo::Result<()> {
        self.state
            .extra_env
            .lock()
            .unwrap()
            .insert(name.to_string(), value.to_string());
        Ok(())
    }

    fn toggle_debug_ui(&mut self) -> zbus::fdo::Result<()> {
        // self.state.comms.display(DisplayMessage::ToggleDebugUi);
        Ok(())
    }

    fn start_video_stream(&mut self) -> zbus::fdo::Result<()> {
        // self.state.comms.display(DisplayMessage::StartVideoStream);
        Ok(())
    }

    fn vt_switch(&mut self, vt: i32) -> zbus::fdo::Result<()> {
        info!("VT switch to {vt} requested over D-Bus");
        self.state.comms.main(MainMessage::SwitchVt(vt));
        Ok(())
    }

    fn map_key(&mut self, binding: KeyBindingInfo) -> zbus::fdo::Result<()> {
        self.state.keymaps.lock().unwrap().push(binding.clone());
        let Some(key) = evdev_keycode_from_name(&binding.key) else {
            warn!(
                "Ignoring keymap binding {:?}+{}: unknown key name",
                binding.mods, binding.key
            );
            return Ok(());
        };
        self.state.comms.main(MainMessage::AddKeymap {
            key,
            mods: Mods::from(binding.mods),
            binding_id: binding.binding_id,
        });
        Ok(())
    }

    fn clear_keymaps(&mut self) -> zbus::fdo::Result<()> {
        self.state.keymaps.lock().unwrap().clear();
        self.state.comms.main(MainMessage::ClearKeymaps);
        Ok(())
    }

    fn inject_key(&mut self, name: &str) -> zbus::fdo::Result<()> {
        self.state.comms.main(MainMessage::InjectInput(InjectedInput::Key {
            name: name.to_string(),
        }));
        Ok(())
    }

    fn type_text(&mut self, text: &str) -> zbus::fdo::Result<()> {
        self.state.comms.main(MainMessage::InjectInput(InjectedInput::TypeText {
            text: text.to_string(),
        }));
        Ok(())
    }

    fn inject_pointer_move(&mut self, x: f64, y: f64) -> zbus::fdo::Result<()> {
        self.state
            .comms
            .main(MainMessage::InjectInput(InjectedInput::PointerMove { x, y }));
        Ok(())
    }

    fn inject_pointer_click(&mut self, x: f64, y: f64, button: u32) -> zbus::fdo::Result<()> {
        self.state.comms.main(MainMessage::InjectInput(
            InjectedInput::PointerClick { x, y, button },
        ));
        Ok(())
    }
}

fn spawn_process(
    command: &str,
    args: &[String],
    wayland_display: &Arc<Mutex<Option<String>>>,
    extra_env: &Arc<Mutex<HashMap<String, String>>>,
) {
    info!("Starting program: {command} {args:?}");
    let mut cmd = Command::new(command);
    cmd.args(args).envs(extra_env.lock().unwrap().iter());
    if let Some(wayland_display) = wayland_display.lock().unwrap().as_ref() {
        info!("Spawning `{command}` with WAYLAND_DISPLAY={wayland_display}");
        cmd.env("WAYLAND_DISPLAY", wayland_display);
    } else {
        warn!("Spawning `{command}` without WAYLAND_DISPLAY; client may connect to the wrong compositor");
    }
    if let Err(e) = cmd.spawn() {
        error!("Failed to start program {command}: {e}");
    }
}

pub(crate) fn emit_signal<B>(connection: &Connection, member: &str, body: &B) -> anyhow::Result<()>
where
    B: serde::ser::Serialize + zbus::zvariant::DynamicType,
{
    connection
        .emit_signal(None::<()>, OBJECT_PATH, INTERFACE_NAME, member, body)
        .map_err(Into::into)
}
