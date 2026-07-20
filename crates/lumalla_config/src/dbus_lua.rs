//! Lua bindings that talk to the compositor over D-Bus.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use std::sync::Arc;

use anyhow::Context;
use lumalla_ipc::{
    DrmDeviceInfo, KeyBindingInfo, LayoutOutputInfo, LayoutSpacesInfo, ModsInfo, OutputConfigInfo,
    OutputInfo, WindowManagerProxy, WindowRuleInfo, ZoneInfo,
};
use lumalla_shared::{CallbackRef, GlobalArgs, Mods, Output};
use mlua::{
    Error as LuaError, FromLua, Function as LuaFunction, IntoLua, Lua, Result as LuaResult,
    Table as LuaTable, Value as LuaValue,
};
use zbus::blocking::Connection;

use crate::callback::CallbackState;

const LUA_MODULE_NAME: &str = "lumalla";

fn dbus_result<T>(result: zbus::fdo::Result<T>) -> LuaResult<T> {
    result.map_err(|err| LuaError::external(Arc::new(err)))
}

/// Client handle for external configuration.
#[derive(Clone)]
pub struct DbusConfigClient {
    pub(crate) proxy: Arc<WindowManagerProxy<'static>>,
    _connection: &'static Connection,
}

impl DbusConfigClient {
    /// Connect to a running compositor.
    pub fn connect() -> anyhow::Result<Self> {
        let connection = Box::leak(Box::new(
            Connection::session().context("Failed to connect to session bus")?,
        ));
        let proxy = WindowManagerProxy::new(connection)
            .context("Failed to create D-Bus proxy")?;
        Ok(Self {
            proxy: Arc::new(proxy),
            _connection: connection,
        })
    }
}

pub(crate) fn init_dbus_module(
    lua: &Lua,
    client: DbusConfigClient,
    callback_state: CallbackState,
    on_startup: Rc<RefCell<Option<CallbackRef>>>,
    on_connector_change: Rc<RefCell<Option<CallbackRef>>>,
    on_drm_devices_change: Rc<RefCell<Option<CallbackRef>>>,
) -> LuaResult<LuaTable> {
    let module = lua.create_table()?;

    let cb_state = callback_state.clone();
    let on_startup_cb = on_startup.clone();
    module.set(
        "on_startup",
        lua.create_function(move |_, callback: LuaFunction| {
            let callback = cb_state.register_callback(callback);
            *on_startup_cb.borrow_mut() = Some(callback);
            Ok(())
        })?,
    )?;

    let cb_state = callback_state.clone();
    let on_connector_change_cb = on_connector_change.clone();
    module.set(
        "on_connector_change",
        lua.create_function(move |_, callback: LuaFunction| {
            let callback = cb_state.register_callback(callback);
            *on_connector_change_cb.borrow_mut() = Some(callback);
            Ok(())
        })?,
    )?;

    let cb_state = callback_state.clone();
    let on_drm_devices_change_cb = on_drm_devices_change.clone();
    module.set(
        "on_drm_devices_change",
        lua.create_function(move |_, callback: LuaFunction| {
            let callback = cb_state.register_callback(callback);
            *on_drm_devices_change_cb.borrow_mut() = Some(callback);
            Ok(())
        })?,
    )?;

    module.set("quit", create_quit_callback(lua, client.clone())?)?;
    module.set("shutdown", create_quit_callback(lua, client.clone())?)?;

    let c = client.clone();
    module.set(
        "toggle_debug_ui",
        lua.create_function(move |_, ()| {
            dbus_result(c.proxy.toggle_debug_ui())?;
            Ok(())
        })?,
    )?;

    let c = client.clone();
    module.set(
        "start_video_stream",
        lua.create_function(move |_, ()| {
            dbus_result(c.proxy.start_video_stream())?;
            Ok(())
        })?,
    )?;

    init_dbus_keymap(lua, &module, client.clone(), callback_state)?;
    init_dbus_output(lua, &module, client.clone())?;
    init_dbus_drm(lua, &module, client.clone())?;
    init_dbus_spawn(lua, &module, client.clone())?;
    init_dbus_zone(lua, &module, client.clone())?;
    init_dbus_window(lua, &module, client)?;

    Ok(module)
}

pub(crate) fn register_dbus_module(
    lua: &Lua,
    client: DbusConfigClient,
    callback_state: CallbackState,
    on_startup: Rc<RefCell<Option<CallbackRef>>>,
    on_connector_change: Rc<RefCell<Option<CallbackRef>>>,
    on_drm_devices_change: Rc<RefCell<Option<CallbackRef>>>,
) -> anyhow::Result<()> {
    lua.register_module(
        LUA_MODULE_NAME,
        init_dbus_module(
            lua,
            client,
            callback_state,
            on_startup,
            on_connector_change,
            on_drm_devices_change,
        )
        .map_err(|err| anyhow::anyhow!("Unable to create D-Bus config module: {err}"))?,
    )
    .map_err(|err| anyhow::anyhow!("Unable to register D-Bus config module: {err}"))?;
    Ok(())
}

fn create_quit_callback(lua: &Lua, client: DbusConfigClient) -> LuaResult<LuaFunction> {
    lua.create_function(move |_, ()| {
        dbus_result(client.proxy.quit())?;
        Ok(())
    })
}

fn create_vt_callback(lua: &Lua, client: DbusConfigClient, vt: i32) -> LuaResult<LuaFunction> {
    lua.create_function(move |_, ()| {
        dbus_result(client.proxy.vt_switch(vt))?;
        Ok(())
    })
}

fn init_dbus_keymap(
    lua: &Lua,
    module: &LuaTable,
    client: DbusConfigClient,
    callback_state: CallbackState,
) -> LuaResult<()> {
    module.set(
        "map_key",
        lua.create_function(move |_, keymap: ConfigKeymap| {
            let callback = callback_state.register_callback(keymap.callback);
            dbus_result(client.proxy.map_key(KeyBindingInfo {
                binding_id: callback.callback_id.to_string(),
                key: keymap.key,
                mods: ModsInfo::from(keymap.mods),
            }))?;
            Ok(())
        })?,
    )?;
    Ok(())
}

fn init_dbus_output(lua: &Lua, module: &LuaTable, client: DbusConfigClient) -> LuaResult<()> {
    module.set(
        "set_layout",
        lua.create_function(move |_, layout: ConfigLayout| {
            let spaces: LayoutSpacesInfo = layout
                .spaces
                .into_iter()
                .map(|(name, outputs)| {
                    (
                        name,
                        outputs
                            .into_iter()
                            .map(|output| LayoutOutputInfo {
                                name: output.name,
                                x: output.x,
                                y: output.y,
                            })
                            .collect(),
                    )
                })
                .collect();
            dbus_result(client.proxy.set_layout(spaces))?;
            Ok(())
        })?,
    )?;
    Ok(())
}

fn init_dbus_drm(lua: &Lua, module: &LuaTable, client: DbusConfigClient) -> LuaResult<()> {
    let get_client = client.clone();
    module.set(
        "get_drm_devices",
        lua.create_function(move |lua, ()| {
            let devices = dbus_result(get_client.proxy.get_drm_devices())?;
            drm_devices_to_lua(lua, devices)
        })?,
    )?;

    let render_client = client.clone();
    module.set(
        "set_render_device",
        lua.create_function(move |_, path: Option<String>| {
            let path = path.unwrap_or_default();
            dbus_result(render_client.proxy.set_render_device(&path))?;
            Ok(())
        })?,
    )?;

    let configs_client = client;
    module.set(
        "set_output_configs",
        lua.create_function(move |_, configs: Vec<ConfigOutputSetting>| {
            let infos: Vec<OutputConfigInfo> = configs
                .into_iter()
                .map(|c| OutputConfigInfo {
                    name: c.name,
                    enabled: c.enabled,
                    mode_name: c.mode.unwrap_or_default(),
                })
                .collect();
            dbus_result(configs_client.proxy.set_output_configs(infos))?;
            Ok(())
        })?,
    )?;
    Ok(())
}

pub(crate) fn drm_devices_to_lua(lua: &Lua, devices: Vec<DrmDeviceInfo>) -> LuaResult<LuaValue> {
    let table = lua.create_table()?;
    for (index, device) in devices.into_iter().enumerate() {
        let device_table = lua.create_table()?;
        device_table.set("path", device.path)?;
        device_table.set("selected_render_device", device.selected_render_device)?;
        let connectors = lua.create_table()?;
        for (c_index, connector) in device.connectors.into_iter().enumerate() {
            let connector_table = lua.create_table()?;
            connector_table.set("name", connector.name)?;
            connector_table.set("connector_id", connector.connector_id)?;
            connector_table.set("connector_type", connector.connector_type)?;
            connector_table.set("connected", connector.connected)?;
            connector_table.set("mm_width", connector.mm_width)?;
            connector_table.set("mm_height", connector.mm_height)?;
            let modes = lua.create_table()?;
            for (m_index, mode) in connector.modes.into_iter().enumerate() {
                let mode_table = lua.create_table()?;
                mode_table.set("name", mode.name)?;
                mode_table.set("width", mode.width)?;
                mode_table.set("height", mode.height)?;
                mode_table.set("refresh_hz", mode.refresh_hz)?;
                mode_table.set("preferred", mode.preferred)?;
                modes.set(m_index + 1, mode_table)?;
            }
            connector_table.set("modes", modes)?;
            connectors.set(c_index + 1, connector_table)?;
        }
        device_table.set("connectors", connectors)?;
        table.set(index + 1, device_table)?;
    }
    table.into_lua(lua)
}

struct ConfigOutputSetting {
    name: String,
    enabled: bool,
    mode: Option<String>,
}

impl FromLua for ConfigOutputSetting {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        let table = value.as_table().ok_or_else(|| LuaError::FromLuaConversionError {
            from: "LuaOutputConfig",
            to: String::from("ConfigOutputSetting"),
            message: Some(String::from("Expected a Lua table for output config")),
        })?;
        Ok(Self {
            name: table.get("name")?,
            enabled: table.get("enabled").unwrap_or(true),
            mode: table.get::<Option<String>>("mode").unwrap_or(None),
        })
    }
}

fn init_dbus_spawn(lua: &Lua, module: &LuaTable, client: DbusConfigClient) -> LuaResult<()> {
    let spawn_client = client.clone();
    module.set(
        "spawn",
        lua.create_function(move |_, spawn: ConfigSpawn| {
            dbus_result(spawn_client.proxy.spawn(&spawn.command, spawn.args))?;
            Ok(())
        })?,
    )?;

    let focus_client = client;
    module.set(
        "focus_or_spawn",
        lua.create_function(move |_, (app_id, command): (String, ConfigSpawn)| {
            dbus_result(
                focus_client
                    .proxy
                    .focus_or_spawn(&app_id, &command.command, command.args),
            )?;
            Ok(())
        })?,
    )?;
    Ok(())
}

fn init_dbus_zone(lua: &Lua, module: &LuaTable, client: DbusConfigClient) -> LuaResult<()> {
    let zones_client = client.clone();
    module.set(
        "set_zones",
        lua.create_function(move |_, zones: Vec<ConfigZone>| {
            dbus_result(
                zones_client
                    .proxy
                    .set_zones(zones.into_iter().map(Into::into).collect()),
            )?;
            Ok(())
        })?,
    )?;

    let move_client = client;
    module.set(
        "move_current_window_to_zone",
        lua.create_function(move |_, zone_name: String| {
            dbus_result(move_client.proxy.move_current_window_to_zone(&zone_name))?;
            Ok(())
        })?,
    )?;
    Ok(())
}

fn init_dbus_window(lua: &Lua, module: &LuaTable, client: DbusConfigClient) -> LuaResult<()> {
    let close_client = client.clone();
    module.set(
        "close_current_window",
        lua.create_function(move |_, ()| {
            dbus_result(close_client.proxy.close_current_window())?;
            Ok(())
        })?,
    )?;

    let rules_client = client;
    module.set(
        "add_window_rule",
        lua.create_function(move |_, window_rule: ConfigWindowRule| {
            dbus_result(rules_client.proxy.add_window_rule(WindowRuleInfo {
                app_id: window_rule.app_id,
                zone: window_rule.zone,
            }))?;
            Ok(())
        })?,
    )?;
    Ok(())
}

pub(crate) fn set_default_keymaps(
    lua: &Lua,
    client: &DbusConfigClient,
    callback_state: &CallbackState,
) -> anyhow::Result<()> {
    let default_keymaps = [
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "backspace",
            create_quit_callback(lua, client.clone())
                .map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f1",
            create_vt_callback(lua, client.clone(), 1)
                .map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f2",
            create_vt_callback(lua, client.clone(), 2)
                .map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f3",
            create_vt_callback(lua, client.clone(), 3)
                .map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f4",
            create_vt_callback(lua, client.clone(), 4)
                .map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f5",
            create_vt_callback(lua, client.clone(), 5)
                .map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f6",
            create_vt_callback(lua, client.clone(), 6)
                .map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f7",
            create_vt_callback(lua, client.clone(), 7)
                .map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f8",
            create_vt_callback(lua, client.clone(), 8)
                .map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f9",
            create_vt_callback(lua, client.clone(), 9)
                .map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f10",
            create_vt_callback(lua, client.clone(), 10)
                .map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f11",
            create_vt_callback(lua, client.clone(), 11)
                .map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f12",
            create_vt_callback(lua, client.clone(), 12)
                .map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
    ];

    for (mods, key_name, callback) in default_keymaps {
        let callback_ref = callback_state.register_callback(callback);
        client
            .proxy
            .map_key(KeyBindingInfo {
                binding_id: callback_ref.callback_id.to_string(),
                key: key_name.to_string(),
                mods: ModsInfo::from(mods),
            })
            .context("Failed to register default keymap")?;
    }

    Ok(())
}

struct ConfigKeymap {
    key: String,
    mods: Mods,
    callback: LuaFunction,
}

impl FromLua for ConfigKeymap {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        let table = value.as_table().unwrap();
        let mut mods = Mods::default();
        for mod_key in table.get::<String>("mods").unwrap_or_default().split('|') {
            match mod_key {
                "shift" => mods.shift = true,
                "logo" | "super" => mods.logo = true,
                "ctrl" => mods.ctrl = true,
                "alt" => mods.alt = true,
                "" => {}
                _ => log::warn!("Unhandled mod key: {mod_key}"),
            }
        }
        Ok(Self {
            key: table.get("key")?,
            mods,
            callback: table.get("callback")?,
        })
    }
}

struct ConfigLayout {
    spaces: HashMap<String, Vec<ConfigOutput>>,
}

impl FromLua for ConfigLayout {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        let table = value.as_table().ok_or_else(|| LuaError::FromLuaConversionError {
            from: "LuaConfigLayout",
            to: String::from("ConfigLayout"),
            message: Some(String::from("Expected a Lua table for the ConfigLayout")),
        })?;
        let mut spaces = HashMap::new();
        for pair in table.pairs() {
            let (space_name, config_outputs) = pair?;
            spaces.insert(space_name, config_outputs);
        }
        Ok(Self { spaces })
    }
}

pub(crate) struct ConfigOutput {
    name: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl From<&Output> for ConfigOutput {
    fn from(value: &Output) -> Self {
        Self {
            name: value.name.clone(),
            x: value.location.0,
            y: value.location.1,
            width: value.size.0,
            height: value.size.1,
        }
    }
}

impl FromLua for ConfigOutput {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        let table = value.as_table().ok_or_else(|| LuaError::FromLuaConversionError {
            from: "LuaOutput",
            to: String::from("ConfigOutput"),
            message: Some(String::from("Expected a Lua table for the ConfigOutput")),
        })?;
        Ok(Self {
            name: table.get("name")?,
            x: table.get("x")?,
            y: table.get("y")?,
            width: table.get("width")?,
            height: table.get("height")?,
        })
    }
}

impl IntoLua for ConfigOutput {
    fn into_lua(self, lua: &Lua) -> LuaResult<LuaValue> {
        let lua_output = lua.create_table()?;
        lua_output.set("name", self.name)?;
        lua_output.set("x", self.x)?;
        lua_output.set("y", self.y)?;
        lua_output.set("width", self.width)?;
        lua_output.set("height", self.height)?;
        lua_output.into_lua(lua)
    }
}

struct ConfigSpawn {
    command: String,
    args: Vec<String>,
}

impl FromLua for ConfigSpawn {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        let table = value.as_table().ok_or_else(|| LuaError::FromLuaConversionError {
            from: "LuaSpawn",
            to: String::from("ConfigSpawn"),
            message: Some(String::from("Expected a Lua table for the ConfigSpawn")),
        })?;
        Ok(Self {
            command: table.get("command")?,
            args: table.get("args").unwrap_or_default(),
        })
    }
}

struct ConfigZone {
    name: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    default: bool,
}

impl FromLua for ConfigZone {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        let table = value.as_table().ok_or_else(|| LuaError::FromLuaConversionError {
            from: "LuaZone",
            to: String::from("ConfigZone"),
            message: Some(String::from("Expected a Lua table for the ConfigZone")),
        })?;
        Ok(Self {
            name: table.get("name")?,
            x: table.get("x")?,
            y: table.get("y")?,
            width: table.get("width")?,
            height: table.get("height")?,
            default: table.get("default").unwrap_or(false),
        })
    }
}

impl From<ConfigZone> for ZoneInfo {
    fn from(value: ConfigZone) -> Self {
        Self {
            name: value.name,
            x: value.x,
            y: value.y,
            width: value.width,
            height: value.height,
            default: value.default,
        }
    }
}

struct ConfigWindowRule {
    app_id: String,
    zone: String,
}

impl FromLua for ConfigWindowRule {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        let table = value.as_table().ok_or_else(|| LuaError::FromLuaConversionError {
            from: "LuaWindowRule",
            to: String::from("ConfigWindowRule"),
            message: Some(String::from("Expected a Lua table for the ConfigWindowRule")),
        })?;
        Ok(Self {
            app_id: table.get("app_id")?,
            zone: table.get("zone")?,
        })
    }
}

pub(crate) fn load_config_files(lua: &Lua, args: &GlobalArgs) -> anyhow::Result<()> {
    if let Some(config_path) = &args.config {
        exec_config_file(lua, config_path.as_ref())?;
    } else {
        let xdg_dirs = xdg::BaseDirectories::with_prefix("lumalla").unwrap();
        for path in xdg_dirs.list_config_files("") {
            exec_config_file(lua, path.as_ref())?;
        }
    }
    Ok(())
}

pub(crate) fn watch_config_files(
    watcher: &mut crate::config_watcher::ConfigWatcher,
    args: &GlobalArgs,
) -> anyhow::Result<()> {
    if let Some(config_path) = &args.config {
        watcher.watch(config_path.as_ref())?;
    } else {
        let xdg_dirs = xdg::BaseDirectories::with_prefix("lumalla").unwrap();
        for path in xdg_dirs.list_config_files("") {
            watcher.watch(path.as_ref())?;
        }
    }
    Ok(())
}

pub(crate) fn reload_config_file(lua: &Lua, path: &std::path::Path) -> anyhow::Result<()> {
    exec_config_file(lua, path)
}

fn exec_config_file(lua: &Lua, path: &std::path::Path) -> anyhow::Result<()> {
    let user_config = std::fs::read(path)?;
    lua.load(&user_config)
        .exec()
        .map_err(|err| anyhow::anyhow!("Unable to run config: {err}"))?;
    Ok(())
}

pub(crate) fn outputs_from_infos(outputs: Vec<OutputInfo>) -> HashMap<String, Output> {
    outputs
        .into_iter()
        .map(|info| {
            let output = Output::from(&info);
            (output.name.clone(), output)
        })
        .collect()
}
