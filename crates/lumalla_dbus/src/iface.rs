//! Compositor-side implementation of the window manager D-Bus API.

use std::{
    collections::HashMap,
    fs::File,
    io::BufWriter,
    process::Command,
    sync::{Arc, Condvar, Mutex},
};

use log::{error, info, warn};
use lumalla_input::evdev_keycode_from_name_with_xkb;
use lumalla_ipc::{
    INTERFACE_NAME, KeyBindingInfo, OBJECT_PATH, WindowManagerHandler,
    types::{
        DrmDeviceInfo, LayoutSpacesInfo, OutputConfigInfo, OutputInfo, WindowInfo, WindowRuleInfo,
        XkbInfo, ZoneInfo,
    },
};
use lumalla_shared::{
    CapturedImage, Comms, InjectedInput, MainMessage, Mods, Output, WindowGeometryUpdate,
    WindowState, XkbConfig, geometry_field_from_dbus,
};
use std::path::PathBuf;
use zbus::blocking::Connection;

/// In-flight screenshot waiting for main-thread capture + PNG write completion.
pub(crate) struct PendingScreenshot {
    pub path: String,
    /// `None` until the IPC thread finishes encode/write (or records an error).
    pub result: Mutex<Option<Result<(), String>>>,
    pub done: Condvar,
}

pub(crate) struct ServiceState {
    pub comms: Comms,
    pub outputs: Arc<Mutex<Vec<OutputInfo>>>,
    pub output_lookup: Arc<Mutex<HashMap<String, Output>>>,
    pub drm_devices: Arc<Mutex<Vec<DrmDeviceInfo>>>,
    pub wayland_display: Arc<Mutex<Option<String>>>,
    pub extra_env: Arc<Mutex<HashMap<String, String>>>,
    pub keymaps: Arc<Mutex<Vec<KeyBindingInfo>>>,
    pub xkb_config: Arc<Mutex<XkbConfig>>,
    pub windows: Arc<Mutex<Vec<WindowState>>>,
    pub pending_screenshots: Arc<Mutex<HashMap<usize, Arc<PendingScreenshot>>>>,
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
        let config = self.state.xkb_config.lock().unwrap().clone();
        let Some(key) = evdev_keycode_from_name_with_xkb(&binding.key, &config) else {
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

    fn set_xkb(&mut self, xkb: XkbInfo) -> zbus::fdo::Result<()> {
        let config = XkbConfig::from(xkb);
        info!("Set XKB config over D-Bus: {config:?}");
        *self.state.xkb_config.lock().unwrap() = config.clone();
        self.state.comms.main(MainMessage::SetXkb(config));
        Ok(())
    }

    fn inject_key(&mut self, name: &str) -> zbus::fdo::Result<()> {
        self.state
            .comms
            .main(MainMessage::InjectInput(InjectedInput::Key {
                name: name.to_string(),
            }));
        Ok(())
    }

    fn type_text(&mut self, text: &str) -> zbus::fdo::Result<()> {
        self.state
            .comms
            .main(MainMessage::InjectInput(InjectedInput::TypeText {
                text: text.to_string(),
            }));
        Ok(())
    }

    fn inject_pointer_move(&mut self, x: f64, y: f64) -> zbus::fdo::Result<()> {
        self.state
            .comms
            .main(MainMessage::InjectInput(InjectedInput::PointerMove {
                x,
                y,
            }));
        Ok(())
    }

    fn inject_pointer_click(&mut self, x: f64, y: f64, button: u32) -> zbus::fdo::Result<()> {
        self.state
            .comms
            .main(MainMessage::InjectInput(InjectedInput::PointerClick {
                x,
                y,
                button,
            }));
        Ok(())
    }

    fn capture_screenshot(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        path: &str,
    ) -> zbus::fdo::Result<()> {
        if width <= 0 || height <= 0 {
            return Err(zbus::fdo::Error::Failed(
                "screenshot width and height must be positive".into(),
            ));
        }
        if path.is_empty() {
            return Err(zbus::fdo::Error::Failed(
                "screenshot path must not be empty".into(),
            ));
        }

        let path = path.to_string();
        let request_id = path.as_ptr() as usize;
        let pending = Arc::new(PendingScreenshot {
            path,
            result: Mutex::new(None),
            done: Condvar::new(),
        });
        self.state
            .pending_screenshots
            .lock()
            .unwrap()
            .insert(request_id, Arc::clone(&pending));

        self.state.comms.main(MainMessage::CaptureScreenshot {
            request_id,
            x,
            y,
            width,
            height,
        });

        let mut guard = pending.result.lock().unwrap();
        while guard.is_none() {
            guard = pending.done.wait(guard).unwrap();
        }
        match guard.take().unwrap() {
            Ok(()) => Ok(()),
            Err(err) => Err(zbus::fdo::Error::Failed(err)),
        }
    }
}

pub(crate) fn complete_screenshot(
    pending_screenshots: &Mutex<HashMap<usize, Arc<PendingScreenshot>>>,
    request_id: usize,
    result: Result<CapturedImage, String>,
) {
    let Some(pending) = pending_screenshots.lock().unwrap().remove(&request_id) else {
        error!("Screenshot reply for unknown request_id={request_id:#x}");
        return;
    };

    let write_result = match result {
        Ok(image) => write_png(&pending.path, &image),
        Err(err) => Err(err),
    };

    {
        let mut guard = pending.result.lock().unwrap();
        *guard = Some(write_result);
    }
    pending.done.notify_one();
}

fn write_png(path: &str, image: &CapturedImage) -> Result<(), String> {
    let expected = (image.width as usize)
        .checked_mul(image.height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| "screenshot dimensions overflow".to_string())?;
    if image.rgba.len() != expected {
        return Err(format!(
            "screenshot buffer size mismatch: got {} bytes, expected {expected}",
            image.rgba.len()
        ));
    }

    let file = File::create(path).map_err(|err| format!("failed to create {path}: {err}"))?;
    let writer = BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut png_writer = encoder
        .write_header()
        .map_err(|err| format!("failed to write PNG header to {path}: {err}"))?;
    png_writer
        .write_image_data(&image.rgba)
        .map_err(|err| format!("failed to write PNG data to {path}: {err}"))?;
    Ok(())
}

pub(crate) fn spawn_process(
    command: &str,
    args: &[String],
    wayland_display: &Arc<Mutex<Option<String>>>,
    extra_env: &Arc<Mutex<HashMap<String, String>>>,
) -> Option<std::process::Child> {
    info!("Starting program: {command} {args:?}");
    let mut cmd = Command::new(command);
    cmd.args(args).envs(extra_env.lock().unwrap().iter());
    if let Some(wayland_display) = wayland_display.lock().unwrap().as_ref() {
        info!("Spawning `{command}` with WAYLAND_DISPLAY={wayland_display}");
        cmd.env("WAYLAND_DISPLAY", wayland_display);
    } else {
        warn!(
            "Spawning `{command}` without WAYLAND_DISPLAY; client may connect to the wrong compositor"
        );
    }
    match cmd.spawn() {
        Ok(child) => Some(child),
        Err(e) => {
            error!("Failed to start program {command}: {e}");
            None
        }
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
