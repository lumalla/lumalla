//! The config module is responsible for allowing the user to interact with the rest of the application.

#![warn(missing_docs)]

mod callback;
mod config_watcher;
mod keymap;
mod output;
mod spawn;
mod window;
mod zone;

use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::{Arc, mpsc},
};

use anyhow::Context;
pub use callback::CallbackState;
use config_watcher::ConfigWatcher;
use log::{error, warn};
use lumalla_shared::{
    CallbackRef, Comms, ConfigMessage, DisplayMessage, GlobalArgs, InputMessage,
    MESSAGE_CHANNEL_TOKEN, MainMessage, MessageRunner, Mods, Output,
};
use mio::{Events, Poll};
use mlua::{Function as LuaFunction, Lua, Result as LuaResult, Table as LuaTable};

/// Holds the state of the config module
pub struct ConfigState {
    comms: Comms,
    shutting_down: bool,
    channel: mpsc::Receiver<ConfigMessage>,
    event_loop: Poll,
    lua: Lua,
    callback_state: CallbackState,
    on_startup: Option<CallbackRef>,
    on_connector_change: Option<CallbackRef>,
    outputs: HashMap<String, Output>,
    extra_env: HashMap<String, String>,
    config_watcher: ConfigWatcher,
}

impl MessageRunner for ConfigState {
    type Message = ConfigMessage;

    fn new(
        comms: Comms,
        event_loop: Poll,
        channel: mpsc::Receiver<Self::Message>,
        args: Arc<GlobalArgs>,
    ) -> anyhow::Result<Self> {
        let config_watcher =
            ConfigWatcher::new(comms.config_sender()).context("Failed to create config watcher")?;
        let mut state = Self {
            comms,
            shutting_down: false,
            channel,
            event_loop,
            lua: Lua::new(),
            callback_state: Default::default(),
            on_startup: None,
            on_connector_change: None,
            outputs: HashMap::new(),
            extra_env: HashMap::new(),
            config_watcher,
        };
        state.load_user_config(args, state.callback_state.clone())?;

        Ok(state)
    }

    fn run(&mut self) -> anyhow::Result<()> {
        let mut events = Events::with_capacity(128);
        loop {
            if let Err(err) = self.event_loop.poll(&mut events, None) {
                error!("Unable to poll event loop: {err}");
            }

            for event in events.iter() {
                match event.token() {
                    MESSAGE_CHANNEL_TOKEN => {
                        // Handle messages
                        while let Ok(msg) = self.channel.try_recv() {
                            if let Err(err) = self.handle_message(msg) {
                                error!("Unable to handle message: {err}");
                            }
                        }
                    }
                    _ => unreachable!(),
                }
            }

            // Stop the loop if we're shutting down
            if self.shutting_down {
                break;
            }
        }

        Ok(())
    }
}

const LUA_MODULE_NAME: &str = "lumalla";

impl ConfigState {
    fn handle_message(&mut self, message: ConfigMessage) -> anyhow::Result<()> {
        match message {
            ConfigMessage::Shutdown => {
                self.shutting_down = true;
            }
            ConfigMessage::RunCallback(callback_ref) => {
                self.callback_state
                    .run_callback::<(), ()>(callback_ref, ())?;
            }
            ConfigMessage::ForgetCallback(callback_ref) => {
                self.callback_state.forget_callback(callback_ref)
            }
            ConfigMessage::Startup => {
                if let Some(on_startup) = self.on_startup {
                    self.callback_state.run_callback::<(), ()>(on_startup, ())?;
                }
            }
            ConfigMessage::ConnectorChange(outputs) => {
                self.outputs.clear();
                for output in outputs {
                    self.outputs.insert(output.name.clone(), output);
                }
                self.on_connector_change()?;
            }
            ConfigMessage::ExtraEnv { name, value } => {
                self.extra_env.insert(name, value);
            }
            ConfigMessage::Spawn(command, args) => {
                self.spawn(&command, &args);
            }
            ConfigMessage::SetOnStartup(callback) => {
                if let Some(on_startup) = self.on_startup {
                    self.callback_state.forget_callback(on_startup);
                }
                self.on_startup = Some(callback);
            }
            ConfigMessage::SetOnConnectorChange(callback) => {
                if let Some(on_connector_change) = self.on_connector_change {
                    self.callback_state.forget_callback(on_connector_change);
                }
                self.on_connector_change = Some(callback);
            }
            ConfigMessage::SetLayout { spaces } => {
                self.comms.display(DisplayMessage::SetLayout {
                    spaces: spaces
                        .into_iter()
                        .map(|(name, outputs)| {
                            (
                                name,
                                outputs
                                    .into_iter()
                                    .filter_map(|config_output| {
                                        let Some(output) = self.outputs.get(&config_output.0)
                                        else {
                                            warn!("Output not found: {}", config_output.0);
                                            return None;
                                        };
                                        let mut output = output.clone();
                                        output.set_location(config_output.1, config_output.2);

                                        Some(output)
                                    })
                                    .collect(),
                            )
                        })
                        .collect(),
                });
            }
            ConfigMessage::LoadConfig(path) => {
                self.load_config(&path)?;
            }
        }

        Ok(())
    }

    /// Reload config from a file path
    fn load_config(&mut self, path: &Path) -> anyhow::Result<()> {
        // TODO: do this read async
        let user_config = fs::read(path)?;
        let config = self.lua.load(&user_config);
        config
            .exec()
            .map_err(|err| anyhow::anyhow!("Unable to run config: {err}"))?;
        self.config_watcher.watch(path.as_ref())?;
        Ok(())
    }

    /// Initialize the lua state and starts requires some lua modules
    fn load_user_config(
        &mut self,
        args: Arc<GlobalArgs>,
        callback_state: CallbackState,
    ) -> anyhow::Result<()> {
        let comms = self.comms.clone();
        let cb_state = callback_state.clone();
        let _: LuaTable = self
            .lua
            .load_from_function(
                LUA_MODULE_NAME,
                self.lua
                    .create_function(move |lua: &Lua, _modname: String| {
                        init_base_module(lua, comms.clone(), cb_state.clone())
                    })
                    .map_err(|err| anyhow::anyhow!("Unable to initialize base module: {err}"))?,
            )
            .map_err(|err| anyhow::anyhow!("Unable to initialize base module: {err}"))?;

        if let Err(err) = self.set_default_keymaps() {
            error!("Unable to set default keymaps: {err}");
        }

        if let Err(err) = self.run_and_watch_user_config(args) {
            warn!("Unable to run and watch user config: {err}");
        }

        Ok(())
    }

    fn run_and_watch_user_config(&mut self, args: Arc<GlobalArgs>) -> anyhow::Result<()> {
        if let Some(config_path) = &args.config {
            self.load_config(config_path.as_ref())?;
        } else {
            let xdg_dirs = xdg::BaseDirectories::with_prefix("lumalla").unwrap();
            for path in xdg_dirs.list_config_files("") {
                self.load_config(path.as_ref())?;
            }
        }

        Ok(())
    }

    fn set_default_keymaps(&mut self) -> LuaResult<()> {
        let default_keymaps = [
            (
                Mods {
                    ctrl: true,
                    alt: true,
                    ..Default::default()
                },
                "backspace",
                create_shutdown_callback(&self.lua, self.comms.clone())?,
            ),
            (
                Mods {
                    ctrl: true,
                    alt: true,
                    ..Default::default()
                },
                "f1",
                create_vt_callback(&self.lua, self.comms.clone(), 1)?,
            ),
            (
                Mods {
                    ctrl: true,
                    alt: true,
                    ..Default::default()
                },
                "f2",
                create_vt_callback(&self.lua, self.comms.clone(), 2)?,
            ),
            (
                Mods {
                    ctrl: true,
                    alt: true,
                    ..Default::default()
                },
                "f3",
                create_vt_callback(&self.lua, self.comms.clone(), 3)?,
            ),
            (
                Mods {
                    ctrl: true,
                    alt: true,
                    ..Default::default()
                },
                "f4",
                create_vt_callback(&self.lua, self.comms.clone(), 4)?,
            ),
            (
                Mods {
                    ctrl: true,
                    alt: true,
                    ..Default::default()
                },
                "f5",
                create_vt_callback(&self.lua, self.comms.clone(), 5)?,
            ),
            (
                Mods {
                    ctrl: true,
                    alt: true,
                    ..Default::default()
                },
                "f6",
                create_vt_callback(&self.lua, self.comms.clone(), 6)?,
            ),
            (
                Mods {
                    ctrl: true,
                    alt: true,
                    ..Default::default()
                },
                "f7",
                create_vt_callback(&self.lua, self.comms.clone(), 7)?,
            ),
            (
                Mods {
                    ctrl: true,
                    alt: true,
                    ..Default::default()
                },
                "f8",
                create_vt_callback(&self.lua, self.comms.clone(), 8)?,
            ),
            (
                Mods {
                    ctrl: true,
                    alt: true,
                    ..Default::default()
                },
                "f9",
                create_vt_callback(&self.lua, self.comms.clone(), 9)?,
            ),
            (
                Mods {
                    ctrl: true,
                    alt: true,
                    ..Default::default()
                },
                "f10",
                create_vt_callback(&self.lua, self.comms.clone(), 10)?,
            ),
            (
                Mods {
                    ctrl: true,
                    alt: true,
                    ..Default::default()
                },
                "f11",
                create_vt_callback(&self.lua, self.comms.clone(), 11)?,
            ),
            (
                Mods {
                    ctrl: true,
                    alt: true,
                    ..Default::default()
                },
                "f12",
                create_vt_callback(&self.lua, self.comms.clone(), 12)?,
            ),
        ];

        for (mods, key_name, callback) in default_keymaps {
            self.comms.input(InputMessage::Keymap {
                key_name: key_name.to_string(),
                mods,
                callback: self.callback_state.register_callback(callback),
            });
        }

        Ok(())
    }
}

/// Initialize the base lua module which is used by the user config to interact with the
/// window manager in a script-able and convenient way.
fn init_base_module(lua: &Lua, comms: Comms, callback_state: CallbackState) -> LuaResult<LuaTable> {
    let module = lua.create_table()?;

    let c = comms.clone();
    let cb_state = callback_state.clone();
    module.set(
        "on_startup",
        lua.create_function(move |_, callback: LuaFunction| {
            let callback = cb_state.register_callback(callback);
            c.config(ConfigMessage::SetOnStartup(callback));
            Ok(())
        })?,
    )?;

    let c = comms.clone();
    module.set(
        "toggle_debug_ui",
        lua.create_function(move |_, ()| {
            c.display(DisplayMessage::ToggleDebugUi);
            Ok(())
        })?,
    )?;

    module.set("quit", create_shutdown_callback(lua, comms.clone())?)?;
    module.set("shutdown", create_shutdown_callback(lua, comms.clone())?)?;

    let c = comms.clone();
    module.set(
        "start_video_stream",
        lua.create_function(move |_, ()| {
            c.display(DisplayMessage::StartVideoStream);
            Ok(())
        })?,
    )?;

    keymap::init(lua, &module, comms.clone(), callback_state.clone())?;
    output::init(lua, &module, comms.clone(), callback_state.clone())?;
    spawn::init(lua, &module, comms.clone())?;
    zone::init(lua, &module, comms.clone())?;
    window::init(lua, &module, comms)?;

    Ok(module)
}

fn create_shutdown_callback(lua: &Lua, comms: Comms) -> LuaResult<LuaFunction> {
    lua.create_function(move |_, ()| {
        comms.main(MainMessage::Shutdown);
        Ok(())
    })
}

fn create_vt_callback(lua: &Lua, comms: Comms, vt: i32) -> LuaResult<LuaFunction> {
    lua.create_function(move |_, ()| {
        comms.display(DisplayMessage::VtSwitch(vt));
        Ok(())
    })
}
