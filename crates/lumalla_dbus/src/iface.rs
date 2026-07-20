//! Compositor-side implementation of the window manager D-Bus API.

use std::{
    collections::HashMap,
    process::Command,
    sync::{Arc, Mutex},
};

use log::{error, info};
use lumalla_ipc::{
    INTERFACE_NAME, KeyBindingInfo, OBJECT_PATH, WindowManagerHandler,
    types::{DrmDeviceInfo, LayoutSpacesInfo, OutputInfo, WindowRuleInfo, ZoneInfo},
};
use lumalla_shared::{Comms, MainMessage, Mods, Output};
use zbus::blocking::Connection;

fn key_name_to_keycode(key_name: &str) -> Option<u32> {
    match key_name {
        "backspace" => Some(14),
        "f1" => Some(59),
        "f2" => Some(60),
        "f3" => Some(61),
        "f4" => Some(62),
        "f5" => Some(63),
        "f6" => Some(64),
        "f7" => Some(65),
        "f8" => Some(66),
        "f9" => Some(67),
        "f10" => Some(68),
        "f11" => Some(69),
        "f12" => Some(70),
        _ => None,
    }
}

pub(crate) struct ServiceState {
    pub comms: Comms,
    pub outputs: Arc<Mutex<Vec<OutputInfo>>>,
    pub output_lookup: Arc<Mutex<HashMap<String, Output>>>,
    pub drm_devices: Arc<Mutex<Vec<DrmDeviceInfo>>>,
    pub extra_env: Arc<Mutex<HashMap<String, String>>>,
    pub keymaps: Arc<Mutex<Vec<KeyBindingInfo>>>,
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
        let _ = rule;
        // self.state
        //     .comms
        //     .display(DisplayMessage::AddWindowRule(rule.into()));
        Ok(())
    }

    fn close_current_window(&mut self) -> zbus::fdo::Result<()> {
        // self.state.comms.display(DisplayMessage::CloseCurrentWindow);
        Ok(())
    }

    fn move_current_window_to_zone(&mut self, zone: &str) -> zbus::fdo::Result<()> {
        let _ = zone;
        // self.state
        //     .comms
        //     .display(DisplayMessage::MoveCurrentWindowToZone(zone.to_string()));
        Ok(())
    }

    fn spawn(&mut self, command: &str, args: Vec<String>) -> zbus::fdo::Result<()> {
        spawn_process(command, &args, &self.state.extra_env);
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
        let Some(key) = key_name_to_keycode(&binding.key) else {
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
}

fn spawn_process(command: &str, args: &[String], extra_env: &Arc<Mutex<HashMap<String, String>>>) {
    info!("Starting program: {command} {args:?}");
    if let Err(e) = Command::new(command)
        .args(args)
        .envs(extra_env.lock().unwrap().iter())
        .spawn()
    {
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
