//! D-Bus service thread for the Lumalla compositor.

#![warn(missing_docs)]

mod iface;
mod mutter;

use std::{
    collections::HashMap,
    io,
    process::{Child, Command},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, Ordering},
        mpsc,
    },
    thread::{self},
};

use anyhow::Context;
use iface::{
    CompositorHandler, ServiceState, complete_pipewire_stream, complete_screenshot, emit_signal,
};
use log::{error, info, warn};
use lumalla_ipc::{
    BUS_NAME, OBJECT_PATH, WindowManager, signals,
    types::{DrmDeviceInfo, OutputInfo},
};
use lumalla_shared::{
    Comms, Completion, DbusMessage, DrmDeviceState, EventLoop, MainMessage, OpKind, Output,
};
use mutter::{DisplayConfig, ScreenCast, complete_mutter_stream};
use zbus::{Error as ZbusError, blocking::connection};

use crate::iface::spawn_process_with_options;
use crate::mutter::screen_cast::MutterStreamRegistry;

/// A registered D-Bus service that must be kept alive for the lifetime of the compositor.
pub struct DbusService {
    connection: zbus::blocking::Connection,
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
    output_lookup: Arc<Mutex<HashMap<String, Output>>>,
    drm_devices: Arc<Mutex<Vec<DrmDeviceInfo>>>,
    wayland_display: Arc<Mutex<Option<String>>>,
    windows: Arc<Mutex<Vec<lumalla_shared::WindowState>>>,
    pending_screenshots: Arc<Mutex<HashMap<usize, Arc<iface::PendingScreenshot>>>>,
    pending_pipewire_streams: Arc<Mutex<HashMap<usize, Arc<iface::PendingPipewireStream>>>>,
    mutter_streams: MutterStreamRegistry,
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
        let pending_pipewire_streams = Arc::new(Mutex::new(HashMap::new()));
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
            guides: Arc::new(Mutex::new(Vec::new())),
            pending_screenshots: Arc::clone(&pending_screenshots),
            pending_pipewire_streams: Arc::clone(&pending_pipewire_streams),
            next_pipewire_request_id: Arc::new(Mutex::new(1)),
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

        let mut claimed_screen_cast = false;
        let (screen_cast, mutter_streams) =
            ScreenCast::new(Arc::clone(&outputs), comms, connection.clone());
        connection
            .object_server()
            .at("/org/gnome/Mutter/ScreenCast", screen_cast)
            .context("Failed to register Mutter ScreenCast object")?;
        match connection.request_name("org.gnome.Mutter.ScreenCast") {
            Ok(()) => {
                info!("D-Bus service listening on org.gnome.Mutter.ScreenCast");
                claimed_screen_cast = true;
            }
            Err(err) => warn!(
                "Could not claim org.gnome.Mutter.ScreenCast (portal-gnome screencast may be unavailable): {err}"
            ),
        }

        connection
            .object_server()
            .at(
                "/org/gnome/Mutter/DisplayConfig",
                DisplayConfig::new(Arc::clone(&outputs)),
            )
            .context("Failed to register Mutter DisplayConfig object")?;
        match connection.request_name("org.gnome.Mutter.DisplayConfig") {
            Ok(()) => info!("D-Bus service listening on org.gnome.Mutter.DisplayConfig"),
            Err(err) => warn!(
                "Could not claim org.gnome.Mutter.DisplayConfig (portal monitor list may be empty): {err}"
            ),
        }

        if claimed_screen_cast {
            // portal-gnome only exports impl.portal.ScreenCast after it sees
            // org.gnome.Mutter.ScreenCast. If it started earlier it stays in
            // "settings only" mode until restarted — Chromium then offers tabs only
            // and getDisplayMedia fails with NotAllowedError for monitors.
            //
            // Must not block compositor startup: a synchronous systemctl restart
            // can deadlock waiting on the still-starting session.
            #[cfg(not(test))]
            nudge_screencast_portals_later();
        }

        Ok(Self {
            connection,
            outputs,
            output_lookup,
            drm_devices,
            wayland_display,
            windows: state.windows.clone(),
            pending_screenshots,
            pending_pipewire_streams,
            mutter_streams,
            ready,
        })
    }

    /// Notify config clients that the compositor is ready.
    pub fn emit_ready(&self) -> anyhow::Result<()> {
        emit_signal(&self.connection, signals::READY, &())
    }

    /// Set `WAYLAND_DISPLAY` used for processes spawned over D-Bus.
    ///
    /// Applied immediately (not via the D-Bus thread channel) so config spawns
    /// cannot race an unset value.
    pub fn set_wayland_display(&self, wayland_display: String) {
        info!("Setting WAYLAND_DISPLAY for D-Bus spawns to {wayland_display}");
        *self.wayland_display.lock().unwrap() = Some(wayland_display);
    }
}

/// Restart portal backends so they pick up Lumalla's Mutter ScreenCast name.
///
/// Runs after a short delay on a detached thread, and uses `--no-block`, so the
/// compositor can finish starting without waiting on systemd.
fn nudge_screencast_portals_later() {
    thread::spawn(|| {
        thread::sleep(std::time::Duration::from_secs(2));
        // Restart gnome first so it re-exports ScreenCast, then the front-end portal
        // so it rediscovers the implementation.
        for unit in [
            "xdg-desktop-portal-gnome.service",
            "xdg-desktop-portal.service",
        ] {
            match Command::new("systemctl")
                .args(["--user", "--no-block", "try-restart", unit])
                .status()
            {
                Ok(status) if status.success() => {
                    info!("Requested restart of {unit} so portal ScreenCast can bind to Mutter");
                }
                Ok(status) => warn!("systemctl try-restart {unit} exited with {status}"),
                Err(err) => warn!("Failed to restart {unit}: {err}"),
            }
        }
    });
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
    pending_pipewire_streams: Arc<Mutex<HashMap<usize, Arc<iface::PendingPipewireStream>>>>,
    mutter_streams: MutterStreamRegistry,
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
            pending_pipewire_streams: service.pending_pipewire_streams,
            mutter_streams: service.mutter_streams,
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
            DbusMessage::EmitCursorMoved { x, y, dx, dy } => {
                emit_signal(&self.connection, signals::CURSOR_MOVED, &(x, y, dx, dy))?;
            }
            DbusMessage::EmitCursorClicked {
                x,
                y,
                button,
                pressed,
            } => {
                emit_signal(
                    &self.connection,
                    signals::CURSOR_CLICKED,
                    &(x, y, button, pressed),
                )?;
            }
            DbusMessage::EmitCursorScrolled {
                x,
                y,
                axis,
                value,
            } => {
                emit_signal(
                    &self.connection,
                    signals::CURSOR_SCROLLED,
                    &(x, y, axis, value),
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
            DbusMessage::PipewireStreamStarted { request_id, result } => {
                complete_pipewire_stream(&self.pending_pipewire_streams, request_id, result);
            }
            DbusMessage::MutterScreenCastStarted {
                mutter_stream_id,
                result,
            } => {
                complete_mutter_stream(&self.mutter_streams, mutter_stream_id, result);
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
/// Returns the tread id of the newly created thread.
pub fn run_thread(
    comms: Comms,
    event_loop: EventLoop,
    channel: mpsc::Receiver<DbusMessage>,
    service: DbusService,
) -> io::Result<i32> {
    let thread_id = Arc::new(AtomicI32::new(0));
    let thread_id_for_thread = thread_id.clone();
    thread::Builder::new()
        .name(String::from("dbus"))
        .spawn(move || {
            let tid = unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t };
            thread_id_for_thread.store(tid, Ordering::Release);
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
        })?;
    loop {
        let tid = thread_id.load(Ordering::Acquire);
        if tid != 0 {
            return Ok(tid);
        }
        std::hint::spin_loop();
    }
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

        let first = match DbusService::register(comms()) {
            Ok(service) => service,
            Err(err) => {
                eprintln!("skip dbus_name_registration: {err:#}");
                return;
            }
        };
        drop(first);
        // Name release can lag a moment on the session bus.
        std::thread::sleep(std::time::Duration::from_millis(50));

        let holder = match DbusService::register(comms()) {
            Ok(service) => service,
            Err(err) => {
                eprintln!("skip dbus_name_registration after release: {err:#}");
                return;
            }
        };
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

    #[test]
    fn mutter_bus_names_claimed() {
        if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_err() {
            return;
        }

        let service = match DbusService::register(comms()) {
            Ok(service) => service,
            Err(err) => {
                eprintln!("skip mutter_bus_names_claimed: {err:#}");
                return;
            }
        };

        let conn = zbus::blocking::Connection::session().expect("session bus");
        let dbus = zbus::blocking::fdo::DBusProxy::new(&conn).expect("DBus proxy");
        for name in [
            "org.gnome.Mutter.ScreenCast",
            "org.gnome.Mutter.DisplayConfig",
        ] {
            let owner = dbus.get_name_owner(name.try_into().unwrap());
            assert!(
                owner.is_ok(),
                "expected {name} to be owned while DbusService is alive: {owner:?}"
            );
        }

        let reply = conn.call_method(
            Some("org.gnome.Mutter.ScreenCast"),
            "/org/gnome/Mutter/ScreenCast",
            Some("org.gnome.Mutter.ScreenCast"),
            "CreateSession",
            &std::collections::HashMap::<&str, zbus::zvariant::Value<'_>>::new(),
        );
        assert!(reply.is_ok(), "CreateSession should succeed: {reply:?}");

        let reply = conn.call_method(
            Some("org.gnome.Mutter.DisplayConfig"),
            "/org/gnome/Mutter/DisplayConfig",
            Some("org.gnome.Mutter.DisplayConfig"),
            "GetCurrentState",
            &(),
        );
        assert!(reply.is_ok(), "GetCurrentState should succeed: {reply:?}");

        drop(service);
    }
}
