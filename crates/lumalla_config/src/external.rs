//! External configuration process that controls the compositor over D-Bus.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Context;
use log::{error, info, warn};
use lumalla_ipc::{BUS_NAME, DrmDeviceInfo, OutputInfo, signals};
use lumalla_shared::{CallbackRef, Output};
use mlua::Lua;
use zbus::Message;
use zbus::blocking::fdo::DBusProxy;
use zbus::names::{BusName, OwnedUniqueName};

use crate::args::Args;
use crate::callback::CallbackState;
use crate::config_watcher::ConfigWatcher;
use crate::dbus_lua::{
    ConfigOutput, DbusConfigClient, eval_repl_chunk, load_config_files, outputs_from_infos,
    prepare_repl_env, register_dbus_module, reload_config_file, set_default_keymaps,
    watch_config_files,
};
use crate::repl::{ReplRequest, ReplResponse, start_repl_server};

enum RunEvent {
    Signal(Message),
    /// Compositor bus name was lost or claimed by a different unique name.
    CompositorGone,
}

/// Runs configuration against a compositor exposed on the session D-Bus.
pub struct ExternalConfig {
    client: DbusConfigClient,
    lua: Lua,
    callback_state: CallbackState,
    on_startup: Rc<RefCell<Option<CallbackRef>>>,
    on_connector_change: Rc<RefCell<Option<CallbackRef>>>,
    on_drm_devices_change: Rc<RefCell<Option<CallbackRef>>>,
    outputs: HashMap<String, Output>,
    config_watcher: ConfigWatcher,
    reload_receiver: mpsc::Receiver<PathBuf>,
    repl_socket: Option<PathBuf>,
    shutting_down: bool,
    startup_done: bool,
}

impl ExternalConfig {
    /// Connect to the compositor and load Lua configuration.
    pub fn new(args: &Args) -> anyhow::Result<Self> {
        let client = DbusConfigClient::connect().context("Failed to connect to compositor")?;
        let lua = Lua::new();
        let callback_state = CallbackState::default();
        let on_startup = Rc::new(RefCell::new(None));
        let on_connector_change = Rc::new(RefCell::new(None));
        let on_drm_devices_change = Rc::new(RefCell::new(None));
        let (reload_tx, reload_receiver) = mpsc::channel();
        let config_watcher = ConfigWatcher::new(reload_tx)?;
        let repl_socket = args.repl_socket_path()?;

        register_dbus_module(
            &lua,
            client.clone(),
            callback_state.clone(),
            on_startup.clone(),
            on_connector_change.clone(),
            on_drm_devices_change.clone(),
        )?;

        let mut state = Self {
            client,
            lua,
            callback_state,
            on_startup,
            on_connector_change,
            on_drm_devices_change,
            outputs: HashMap::new(),
            config_watcher,
            reload_receiver,
            repl_socket,
            shutting_down: false,
            startup_done: false,
        };

        if let Err(err) = set_default_keymaps(&state.lua, &state.client, &state.callback_state) {
            error!("Unable to set default keymaps: {err}");
        }

        if let Err(err) = load_config_files(&state.lua, args) {
            warn!("Unable to load user config: {err}");
        }

        if let Err(err) = watch_config_files(&mut state.config_watcher, args) {
            warn!("Unable to watch user config: {err}");
        }

        Ok(state)
    }

    /// Wait for compositor events and dispatch Lua callbacks.
    pub fn run(&mut self) -> anyhow::Result<()> {
        let (event_tx, event_rx) = mpsc::channel();

        // Multiplex all interface signals on one stream. Polling separate blocking
        // SignalIterators in sequence hangs after the first idle stream (e.g. Ready),
        // so key bindings would only fire once.
        let proxy = self.client.proxy.clone();
        let signal_tx = event_tx.clone();
        std::thread::Builder::new()
            .name(String::from("lumalla-config-signals"))
            .spawn(move || {
                let mut signals = match proxy.inner().receive_all_signals() {
                    Ok(signals) => signals,
                    Err(err) => {
                        error!("Unable to subscribe to compositor signals: {err}");
                        return;
                    }
                };
                while let Some(message) = signals.next() {
                    if signal_tx.send(RunEvent::Signal(message)).is_err() {
                        break;
                    }
                }
            })
            .context("Unable to spawn config signal thread")?;

        let expected_owner = self.client.compositor_unique_name.clone();
        let connection = self.client.connection();
        std::thread::Builder::new()
            .name(String::from("lumalla-config-nameowner"))
            .spawn(move || {
                watch_compositor_name_owner(connection, &expected_owner, event_tx);
            })
            .context("Unable to spawn compositor name-owner watch thread")?;

        info!(
            "External config connected to compositor owner {}",
            self.client.compositor_unique_name
        );

        let repl_receiver = if let Some(socket_path) = self.repl_socket.clone() {
            prepare_repl_env(&self.lua)?;
            let (repl_tx, repl_rx) = mpsc::channel();
            start_repl_server(socket_path, repl_tx)?;
            Some(repl_rx)
        } else {
            None
        };

        // Ready may have been emitted before we subscribed; pick it up via is_ready.
        if self.client.proxy.is_ready().unwrap_or(false) {
            self.handle_ready()?;
        }

        loop {
            if self.shutting_down {
                break;
            }

            while let Ok(path) = self.reload_receiver.try_recv() {
                if let Err(err) =
                    reload_config_file(&self.lua, &self.client, &self.callback_state, &path)
                {
                    warn!("Unable to reload config from {}: {err}", path.display());
                }
            }

            if let Some(repl_rx) = &repl_receiver {
                while let Ok(request) = repl_rx.try_recv() {
                    self.handle_repl_request(request);
                }
            }

            match event_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(RunEvent::Signal(message)) => {
                    if let Err(err) = self.handle_signal(message) {
                        warn!("Error while handling compositor signal: {err:#}");
                    }
                }
                Ok(RunEvent::CompositorGone) => {
                    info!("Compositor D-Bus name `{BUS_NAME}` lost or replaced; exiting");
                    self.shutting_down = true;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !compositor_owner_matches(
                        self.client.connection(),
                        &self.client.compositor_unique_name,
                    ) {
                        info!(
                            "Compositor D-Bus name `{BUS_NAME}` no longer owned by {}; exiting",
                            self.client.compositor_unique_name
                        );
                        self.shutting_down = true;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("Compositor event threads disconnected");
                }
            }
        }

        Ok(())
    }

    fn handle_repl_request(&self, request: ReplRequest) {
        let response = match eval_repl_chunk(&self.lua, &request.chunk) {
            Ok(text) => ReplResponse { ok: true, text },
            Err(text) => ReplResponse { ok: false, text },
        };
        if request.reply.send(response).is_err() {
            warn!("REPL client disconnected before receiving response");
        }
    }

    fn handle_signal(&mut self, message: Message) -> anyhow::Result<()> {
        let member = message
            .header()
            .member()
            .map(|member| member.as_str().to_owned());
        match member.as_deref() {
            Some(signals::READY) => self.handle_ready()?,
            Some(signals::OUTPUT_CHANGED) => {
                let outputs: Vec<OutputInfo> = message.body().deserialize()?;
                self.handle_output_changed(outputs)?;
            }
            Some(signals::DRM_DEVICES_CHANGED) => {
                let devices: Vec<DrmDeviceInfo> = message.body().deserialize()?;
                self.handle_drm_devices_changed(devices)?;
            }
            Some(signals::BINDING_ACTIVATED) => {
                let binding_id: String = message.body().deserialize()?;
                self.handle_binding_activated(&binding_id)?;
            }
            Some(other) => {
                warn!("Ignoring unknown compositor signal: {other}");
            }
            None => {
                warn!("Ignoring compositor signal without a member name");
            }
        }
        Ok(())
    }

    fn handle_ready(&mut self) -> anyhow::Result<()> {
        if self.startup_done {
            return Ok(());
        }
        self.startup_done = true;
        if let Some(on_startup) = *self.on_startup.borrow() {
            self.callback_state.run_callback::<(), ()>(on_startup, ())?;
        }
        Ok(())
    }

    fn handle_output_changed(&mut self, outputs: Vec<OutputInfo>) -> anyhow::Result<()> {
        self.outputs = outputs_from_infos(outputs);
        self.on_connector_change()?;
        Ok(())
    }

    fn handle_drm_devices_changed(
        &mut self,
        devices: Vec<lumalla_ipc::DrmDeviceInfo>,
    ) -> anyhow::Result<()> {
        if let Some(on_drm_devices_change) = *self.on_drm_devices_change.borrow() {
            let devices_lua = crate::dbus_lua::drm_devices_to_lua(&self.lua, devices)
                .map_err(|err| anyhow::anyhow!("Unable to convert DRM devices for Lua: {err}"))?;
            self.callback_state
                .run_callback::<mlua::Value, ()>(on_drm_devices_change, devices_lua)?;
        }
        Ok(())
    }

    fn handle_binding_activated(&mut self, binding_id: &str) -> anyhow::Result<()> {
        let Ok(callback_id) = binding_id.parse::<usize>() else {
            warn!("Ignoring binding activation with invalid id: {binding_id}");
            return Ok(());
        };
        if let Err(err) = self
            .callback_state
            .run_callback::<(), ()>(CallbackRef { callback_id }, ())
        {
            // Keep the config process alive so later key bindings still work.
            warn!("Key binding callback {binding_id} failed: {err:#}");
        }
        Ok(())
    }

    fn on_connector_change(&mut self) -> anyhow::Result<()> {
        if let Some(on_connector_change) = *self.on_connector_change.borrow() {
            let outputs: Vec<ConfigOutput> =
                self.outputs.values().map(ConfigOutput::from).collect();
            self.callback_state
                .run_callback::<Vec<ConfigOutput>, ()>(on_connector_change, outputs)?;
        }
        Ok(())
    }
}

fn compositor_owner_matches(connection: &zbus::blocking::Connection, expected_owner: &str) -> bool {
    let Ok(dbus_proxy) = DBusProxy::new(connection) else {
        return false;
    };
    let Ok(bus_name) = BusName::try_from(BUS_NAME) else {
        return false;
    };
    match dbus_proxy.get_name_owner(bus_name) {
        Ok(owner) => owner.as_str() == expected_owner,
        Err(_) => false,
    }
}

fn watch_compositor_name_owner(
    connection: &'static zbus::blocking::Connection,
    expected_owner: &str,
    event_tx: mpsc::Sender<RunEvent>,
) {
    let Ok(dbus_proxy) = DBusProxy::new(connection) else {
        let _ = event_tx.send(RunEvent::CompositorGone);
        return;
    };
    let Ok(mut changes) = dbus_proxy.receive_name_owner_changed() else {
        let _ = event_tx.send(RunEvent::CompositorGone);
        return;
    };

    let expected_owner = match OwnedUniqueName::try_from(expected_owner) {
        Ok(name) => name,
        Err(_) => {
            let _ = event_tx.send(RunEvent::CompositorGone);
            return;
        }
    };

    while let Some(signal) = changes.next() {
        let Ok(args) = signal.args() else {
            continue;
        };
        if args.name().as_str() != BUS_NAME {
            continue;
        }
        let new_owner = args.new_owner().as_ref().map(|name| name.as_str());
        let still_ours = new_owner == Some(expected_owner.as_str());
        if !still_ours {
            let _ = event_tx.send(RunEvent::CompositorGone);
            break;
        }
    }
}
