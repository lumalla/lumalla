//! Window manager D-Bus interface implementation.

use std::{
    collections::HashMap,
    process::Command,
    sync::{Arc, Mutex},
};

use log::{error, info, warn};
use lumalla_ipc::{
    types::{LayoutSpacesInfo, OutputInfo, WindowRuleInfo, ZoneInfo},
    KeyBindingInfo, INTERFACE_NAME, OBJECT_PATH,
};
use lumalla_shared::{
    CallbackRef, Comms, DisplayMessage, InputMessage, MainMessage, Mods, Output,
};
use zbus::interface;

pub(crate) struct ServiceState {
    pub comms: Comms,
    pub outputs: Arc<Mutex<Vec<OutputInfo>>>,
    pub output_lookup: Arc<Mutex<HashMap<String, Output>>>,
    pub extra_env: Arc<Mutex<HashMap<String, String>>>,
    pub keymaps: Arc<Mutex<Vec<KeyBindingInfo>>>,
}

pub(crate) struct WindowManager {
    pub state: Arc<ServiceState>,
}

#[interface(name = "org.lumalla.WindowManager")]
impl WindowManager {
    fn quit(&mut self) -> zbus::fdo::Result<()> {
        info!("Quit requested over D-Bus");
        self.state.comms.main(MainMessage::Shutdown);
        Ok(())
    }

    fn get_outputs(&self) -> zbus::fdo::Result<Vec<OutputInfo>> {
        Ok(self.state.outputs.lock().unwrap().clone())
    }

    fn set_zones(&mut self, zones: Vec<ZoneInfo>) -> zbus::fdo::Result<()> {
        self.state.comms.display(DisplayMessage::SetZones(
            zones.into_iter().map(Into::into).collect(),
        ));
        Ok(())
    }

    fn set_layout(&mut self, spaces: LayoutSpacesInfo) -> zbus::fdo::Result<()> {
        let outputs = self.state.output_lookup.lock().unwrap();
        self.state.comms.display(DisplayMessage::SetLayout {
            spaces: spaces
                .into_iter()
                .map(|(name, layout_outputs)| {
                    (
                        name,
                        layout_outputs
                            .into_iter()
                            .filter_map(|layout_output| {
                                let Some(output) = outputs.get(&layout_output.name) else {
                                    warn!("Output not found: {}", layout_output.name);
                                    return None;
                                };
                                let mut output = output.clone();
                                output.set_location(layout_output.x, layout_output.y);
                                Some(output)
                            })
                            .collect(),
                    )
                })
                .collect(),
        });
        Ok(())
    }

    fn add_window_rule(&mut self, rule: WindowRuleInfo) -> zbus::fdo::Result<()> {
        self.state
            .comms
            .display(DisplayMessage::AddWindowRule(rule.into()));
        Ok(())
    }

    fn close_current_window(&mut self) -> zbus::fdo::Result<()> {
        self.state.comms.display(DisplayMessage::CloseCurrentWindow);
        Ok(())
    }

    fn move_current_window_to_zone(&mut self, zone: &str) -> zbus::fdo::Result<()> {
        self.state
            .comms
            .display(DisplayMessage::MoveCurrentWindowToZone(zone.to_string()));
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
        self.state.comms.display(DisplayMessage::FocusOrSpawn {
            app_id: app_id.to_string(),
            command: command.to_string(),
            args,
        });
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
        self.state.comms.display(DisplayMessage::ToggleDebugUi);
        Ok(())
    }

    fn start_video_stream(&mut self) -> zbus::fdo::Result<()> {
        self.state.comms.display(DisplayMessage::StartVideoStream);
        Ok(())
    }

    fn vt_switch(&mut self, vt: i32) -> zbus::fdo::Result<()> {
        self.state.comms.display(DisplayMessage::VtSwitch(vt));
        Ok(())
    }

    fn map_key(&mut self, binding: KeyBindingInfo) -> zbus::fdo::Result<()> {
        self.state.keymaps.lock().unwrap().push(binding.clone());
        if let Ok(callback_id) = binding.binding_id.parse::<usize>() {
            self.state.comms.input(InputMessage::Keymap {
                key_name: binding.key,
                mods: Mods::from(binding.mods),
                callback: CallbackRef { callback_id },
            });
        }
        Ok(())
    }

    fn clear_keymaps(&mut self) -> zbus::fdo::Result<()> {
        self.state.keymaps.lock().unwrap().clear();
        self.state.comms.input(InputMessage::ClearKeymaps);
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

pub(crate) fn emit_signal<B>(
    connection: &zbus::blocking::Connection,
    member: &str,
    body: &B,
) -> anyhow::Result<()>
where
    B: serde::ser::Serialize + zbus::zvariant::DynamicType,
{
    connection
        .emit_signal(None::<()>, OBJECT_PATH, INTERFACE_NAME, member, body)
        .map_err(Into::into)
}
