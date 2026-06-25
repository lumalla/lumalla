//! External configuration process that controls the compositor over D-Bus.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Context;
use log::{error, info, warn};
use lumalla_ipc::OutputInfo;
use lumalla_shared::{CallbackRef, GlobalArgs, Output};
use mlua::Lua;

use crate::callback::CallbackState;
use crate::config_watcher::ConfigWatcher;
use crate::dbus_lua::{
    ConfigOutput, DbusConfigClient, load_config_files, outputs_from_infos, register_dbus_module,
    reload_config_file, set_default_keymaps, watch_config_files,
};

/// Runs configuration against a compositor exposed on the session D-Bus.
pub struct ExternalConfig {
    client: DbusConfigClient,
    lua: Lua,
    callback_state: CallbackState,
    on_startup: Rc<RefCell<Option<CallbackRef>>>,
    on_connector_change: Rc<RefCell<Option<CallbackRef>>>,
    outputs: HashMap<String, Output>,
    config_watcher: ConfigWatcher,
    reload_receiver: mpsc::Receiver<PathBuf>,
    shutting_down: bool,
}

impl ExternalConfig {
    /// Connect to the compositor and load Lua configuration.
    pub fn new(args: &GlobalArgs) -> anyhow::Result<Self> {
        let client = DbusConfigClient::connect().context("Failed to connect to compositor")?;
        let lua = Lua::new();
        let callback_state = CallbackState::default();
        let on_startup = Rc::new(RefCell::new(None));
        let on_connector_change = Rc::new(RefCell::new(None));
        let (reload_tx, reload_receiver) = mpsc::channel();
        let mut config_watcher = ConfigWatcher::new(reload_tx)?;

        register_dbus_module(
            &lua,
            client.clone(),
            callback_state.clone(),
            on_startup.clone(),
            on_connector_change.clone(),
        )?;

        let mut state = Self {
            client,
            lua,
            callback_state,
            on_startup,
            on_connector_change,
            outputs: HashMap::new(),
            config_watcher,
            reload_receiver,
            shutting_down: false,
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
        let proxy = self.client.proxy.clone();
        let mut ready = proxy.receive_ready()?;
        let mut output_changed = proxy.receive_output_changed()?;
        let mut binding_activated = proxy.receive_binding_activated()?;

        info!("External config connected to compositor");

        loop {
            if self.shutting_down {
                break;
            }

            while let Ok(path) = self.reload_receiver.try_recv() {
                if let Err(err) = reload_config_file(&self.lua, &path) {
                    warn!("Unable to reload config from {}: {err}", path.display());
                }
            }

            if ready.next().is_some() {
                self.handle_ready()?;
            }

            if let Some(signal) = output_changed.next() {
                let args = signal.args()?;
                self.handle_output_changed(args.outputs)?;
            }

            if let Some(signal) = binding_activated.next() {
                let args = signal.args()?;
                self.handle_binding_activated(&args.binding_id)?;
            }

            std::thread::sleep(Duration::from_millis(50));
        }

        Ok(())
    }

    fn handle_ready(&mut self) -> anyhow::Result<()> {
        if let Some(on_startup) = *self.on_startup.borrow() {
            self.callback_state
                .run_callback::<(), ()>(on_startup, ())?;
        }
        Ok(())
    }

    fn handle_output_changed(&mut self, outputs: Vec<OutputInfo>) -> anyhow::Result<()> {
        self.outputs = outputs_from_infos(outputs);
        self.on_connector_change()?;
        Ok(())
    }

    fn handle_binding_activated(&mut self, binding_id: &str) -> anyhow::Result<()> {
        let Ok(callback_id) = binding_id.parse::<usize>() else {
            warn!("Ignoring binding activation with invalid id: {binding_id}");
            return Ok(());
        };
        self.callback_state.run_callback::<(), ()>(
            CallbackRef { callback_id },
            (),
        )?;
        Ok(())
    }

    fn on_connector_change(&mut self) -> anyhow::Result<()> {
        if let Some(on_connector_change) = *self.on_connector_change.borrow() {
            let outputs: Vec<ConfigOutput> = self
                .outputs
                .values()
                .map(ConfigOutput::from)
                .collect();
            self.callback_state
                .run_callback::<Vec<ConfigOutput>, ()>(on_connector_change, outputs)?;
        }
        Ok(())
    }
}
