//! D-Bus service thread for the Lumalla compositor.

#![warn(missing_docs)]

mod iface;

use std::{
    collections::HashMap,
    process::Child,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};

use anyhow::Context;
use iface::{CompositorHandler, ServiceState, complete_screenshot, emit_signal};
use log::{error, info, warn};
use lumalla_ipc::{
    BUS_NAME, OBJECT_PATH, WindowManager, signals,
    types::{DrmDeviceInfo, OutputInfo},
};
use lumalla_shared::{
    Comms, Completion, DbusMessage, DrmDeviceState, EventLoop, MainMessage, OpKind, Output,
};
use zbus::{Error as ZbusError, blocking::connection};

use crate::iface::spawn_process_with_options;

/// A registered D-Bus service that must be kept alive for the lifetime of the compositor.
pub struct DbusService {
    connection: zbus::blocking::Connection,
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
    output_lookup: Arc<Mutex<HashMap<String, Output>>>,
    drm_devices: Arc<Mutex<Vec<DrmDeviceInfo>>>,
    wayland_display: Arc<Mutex<Option<String>>>,
    windows: Arc<Mutex<Vec<lumalla_shared::WindowState>>>,
    pending_screenshots: Arc<Mutex<HashMap<usize, Arc<iface::PendingScreenshot>>>>,
    ready: Arc<AtomicBool>,
}

impl DbusService {
    /// Connect to the session bus and acquire [`BUS_NAME`].
    pub fn register(comms: Comms) -> anyhow::Result<Self> {
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let output_lookup = Arc::new(Mutex::new(HashMap::new()));
        let drm_devices = Arc::new(Mutex::new(Vec::new()));
        let wayland_display = Arc::new(Mutex::new(None));
        let pending_screenshots = Arc::new(Mutex::new(HashMap::new()));
        let ready = Arc::new(AtomicBool::new(false));
        let state = Arc::new(ServiceState {
            comms: comms.clone(),
            outputs: Arc::clone(&outputs),
            output_lookup: Arc::clone(&output_lookup),
            drm_devices: Arc::clone(&drm_devices),
            wayland_display: Arc::clone(&wayland_display),
            extra_env: Arc::new(Mutex::new(HashMap::new())),
            keymaps: Arc::new(Mutex::new(Vec::new())),
            xkb_config: Arc::new(Mutex::new(lumalla_shared::XkbConfig::default())),
            windows: Arc::new(Mutex::new(Vec::new())),
            pending_screenshots: Arc::clone(&pending_screenshots),
            ready: Arc::clone(&ready),
        });
        let connection = connection::Builder::session()
            .context("Failed to connect to session bus")?
            .name(BUS_NAME)
            .context("Invalid D-Bus name")?
            .allow_name_replacements(false)
            .replace_existing_names(false)
            .serve_at(
                OBJECT_PATH,
                WindowManager::new(CompositorHandler {
                    state: Arc::clone(&state),
                }),
            )
            .context("Failed to register D-Bus object")?
            .build()
            .map_err(|err| -> anyhow::Error {
                if err == ZbusError::NameTaken {
                    anyhow::anyhow!("another process already owns the D-Bus name `{BUS_NAME}`")
                } else {
                    err.into()
                }
            })?;
        info!("D-Bus service listening on {BUS_NAME}{OBJECT_PATH}");

        Ok(Self {
            connection,
            outputs,
            output_lookup,
            drm_devices,
            wayland_display,
            windows: state.windows.clone(),
            pending_screenshots,
            ready,
        })
    }

    /// Notify config clients that the compositor is ready.
    pub fn emit_ready(&self) -> anyhow::Result<()> {
        emit_signal(&self.connection, signals::READY, &())
    }
}

/// `user_data` id used for the config-child `WaitId` SQE.
const CONFIG_CHILD_WAITID_ID: u64 = 1;

struct DbusState {
    channel: mpsc::Receiver<DbusMessage>,
    event_loop: EventLoop,
    shutting_down: bool,
    connection: zbus::blocking::Connection,
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
    output_lookup: Arc<Mutex<HashMap<String, Output>>>,
    drm_devices: Arc<Mutex<Vec<DrmDeviceInfo>>>,
    wayland_display: Arc<Mutex<Option<String>>>,
    windows: Arc<Mutex<Vec<lumalla_shared::WindowState>>>,
    pending_screenshots: Arc<Mutex<HashMap<usize, Arc<iface::PendingScreenshot>>>>,
    ready: Arc<AtomicBool>,
    /// Config child process; kept alive so we can reap it via `WaitId` SQE.
    config_child: Option<Child>,
    /// Pid of the child we submitted waitid for; ignore stale waitid completions.
    config_child_pid: Option<u32>,
}

impl DbusState {
    fn new(
        event_loop: EventLoop,
        channel: mpsc::Receiver<DbusMessage>,
        service: DbusService,
    ) -> Self {
        Self {
            channel,
            event_loop,
            shutting_down: false,
            connection: service.connection,
            outputs: service.outputs,
            output_lookup: service.output_lookup,
            drm_devices: service.drm_devices,
            wayland_display: service.wayland_display,
            windows: service.windows,
            pending_screenshots: service.pending_screenshots,
            ready: service.ready,
            config_child: None,
            config_child_pid: None,
        }
    }

    fn run(&mut self) -> anyhow::Result<()> {
        let mut completions = Vec::with_capacity(16);
        // Block on the channel waker only. A periodic timeout here previously
        // cancel/re-armed every iteration and busy-spun on TimeoutRemove CQEs.
        loop {
            if let Err(err) = self.event_loop.wait(&mut completions) {
                error!("Unable to wait on D-Bus event loop: {err}");
            }

            for completion in completions.drain(..) {
                self.handle_completion(completion);
            }

            if self.shutting_down {
                break;
            }
        }

        self.kill_config_child();
        self.event_loop.shutdown_drain()?;
        Ok(())
    }

    /// Terminate and reap the config child if it is still running.
    fn kill_config_child(&mut self) {
        self.config_child_pid = None;
        let Some(mut child) = self.config_child.take() else {
            return;
        };
        let pid = child.id();
        match child.try_wait() {
            Ok(Some(status)) => {
                info!("Config process already exited with {status}");
                return;
            }
            Ok(None) => {}
            Err(err) => warn!("Failed to poll config process {pid}: {err}"),
        }

        info!("Stopping config process {pid}");
        // Prefer SIGTERM so the child can unwind; escalate if it ignores us.
        let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        for _ in 0..20 {
            match child.try_wait() {
                Ok(Some(status)) => {
                    info!("Config process exited with {status}");
                    return;
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
                Err(err) => {
                    warn!("Failed to wait for config process {pid}: {err}");
                    return;
                }
            }
        }
        warn!("Config process {pid} did not exit after SIGTERM; sending SIGKILL");
        if let Err(err) = child.kill() {
            warn!("Failed to SIGKILL config process {pid}: {err}");
        }
        match child.wait() {
            Ok(status) => info!("Config process exited with {status}"),
            Err(err) => warn!("Failed to reap config process {pid}: {err}"),
        }
    }

    fn handle_completion(&mut self, completion: Completion) {
        match completion.kind {
            OpKind::Wake => {
                while let Ok(message) = self.channel.try_recv() {
                    if let Err(err) = self.handle_message(message) {
                        error!("Unable to handle D-Bus message: {err}");
                    }
                }
                if let Err(err) = self.event_loop.rearm_waker() {
                    error!("Unable to re-arm D-Bus waker: {err}");
                }
            }
            OpKind::Timeout | OpKind::Cancel => {}
            OpKind::Waitid => {
                let Some(expected_pid) = self.config_child_pid else {
                    return;
                };
                let Some(child) = self.config_child.as_mut() else {
                    return;
                };
                if child.id() != expected_pid {
                    return;
                }
                match child.try_wait() {
                    Ok(Some(status)) => {
                        info!("Config process exited with {status}");
                        self.config_child = None;
                        self.config_child_pid = None;
                    }
                    Ok(None) => info!("Config process waitid fired but process still running"),
                    Err(err) => warn!("Failed to reap config process: {err}"),
                }
            }
            other => {
                debug_assert!(
                    false,
                    "unexpected D-Bus completion kind {other:?} id={}",
                    completion.id
                );
            }
        }
    }

    fn handle_message(&mut self, message: DbusMessage) -> anyhow::Result<()> {
        match message {
            DbusMessage::Shutdown => {
                self.kill_config_child();
                self.shutting_down = true;
            }
            DbusMessage::SetOutputs(outputs) => {
                self.update_outputs(outputs);
            }
            DbusMessage::SetDrmDevices(devices) => {
                self.update_drm_devices(devices);
            }
            DbusMessage::EmitReady => {
                self.ready.store(true, Ordering::SeqCst);
                emit_signal(&self.connection, signals::READY, &())?;
            }
            DbusMessage::EmitOutputChanged(outputs) => {
                let infos = self.update_outputs(outputs);
                emit_signal(&self.connection, signals::OUTPUT_CHANGED, &(&infos,))?;
            }
            DbusMessage::EmitDrmDevicesChanged(devices) => {
                let infos = self.update_drm_devices(devices);
                emit_signal(&self.connection, signals::DRM_DEVICES_CHANGED, &(&infos,))?;
            }
            DbusMessage::EmitBindingActivated(binding_id) => {
                emit_signal(
                    &self.connection,
                    signals::BINDING_ACTIVATED,
                    &(&binding_id,),
                )?;
            }
            DbusMessage::SetWaylandDisplay(wayland_display) => {
                info!("Setting WAYLAND_DISPLAY for D-Bus spawns to {wayland_display}");
                *self.wayland_display.lock().unwrap() = Some(wayland_display);
            }
            DbusMessage::Spawn { command, args } => {
                // Only one config process should be live; replace any previous child.
                self.kill_config_child();
                if let Some(child) = spawn_process_with_options(
                    &command,
                    &args,
                    &self.wayland_display,
                    &Default::default(),
                    true,
                ) {
                    let pid = child.id();
                    self.config_child_pid = Some(pid);
                    self.config_child = Some(child);
                    if let Err(err) = self.event_loop.submit_waitid(pid, CONFIG_CHILD_WAITID_ID) {
                        warn!("Failed to submit waitid SQE for config process: {err}");
                    }
                }
            }
            DbusMessage::SetWindows(windows) => {
                *self.windows.lock().unwrap() = windows;
            }
            DbusMessage::ScreenshotCaptured { request_id, result } => {
                complete_screenshot(&self.pending_screenshots, request_id, result);
            }
        }

        Ok(())
    }

    fn update_outputs(&self, outputs: Vec<Output>) -> Vec<OutputInfo> {
        let infos: Vec<OutputInfo> = outputs.iter().map(OutputInfo::from).collect();
        *self.outputs.lock().unwrap() = infos.clone();
        let mut lookup = self.output_lookup.lock().unwrap();
        lookup.clear();
        for output in outputs {
            lookup.insert(output.name.clone(), output);
        }
        infos
    }

    fn update_drm_devices(&self, devices: Vec<DrmDeviceState>) -> Vec<DrmDeviceInfo> {
        let infos: Vec<DrmDeviceInfo> = devices.iter().map(DrmDeviceInfo::from).collect();
        *self.drm_devices.lock().unwrap() = infos.clone();
        infos
    }
}

/// Run the D-Bus message loop on a dedicated thread.
pub fn run_thread(
    comms: Comms,
    event_loop: EventLoop,
    channel: mpsc::Receiver<DbusMessage>,
    service: DbusService,
) -> anyhow::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(String::from("dbus"))
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut state = DbusState::new(event_loop, channel, service);
                state.run().context("D-Bus thread exited with an error")
            }));
            match result {
                Ok(Ok(())) => info!("D-Bus thread exited normally"),
                Ok(Err(ref err)) => error!("D-Bus thread exited with an error: {err}"),
                Err(ref err) => error!("D-Bus thread panicked: {err:?}"),
            }
            comms.main(MainMessage::Shutdown);
        })
        .context("Unable to spawn D-Bus thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumalla_shared::message_loop_with_channel;

    fn comms() -> Comms {
        let (_, _, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
        let (_, _, to_dbus) = message_loop_with_channel::<DbusMessage>().unwrap();
        Comms::new(to_main, to_dbus)
    }

    #[test]
    fn dbus_name_registration() {
        if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_err() {
            return;
        }

        let first = DbusService::register(comms()).expect("registration should succeed");
        drop(first);

        let holder =
            DbusService::register(comms()).expect("registration should succeed after release");
        let second = DbusService::register(comms());
        assert!(
            second.is_err(),
            "second registration should fail while name is held"
        );
        let err = second.err().unwrap();
        assert!(
            format!("{err:#}").contains("already owns"),
            "error should mention name ownership: {err:#}"
        );
        drop(holder);
    }
}
