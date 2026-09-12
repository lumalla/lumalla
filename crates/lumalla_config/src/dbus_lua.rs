//! Lua bindings that talk to the compositor over D-Bus.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::thread;
use std::time::Duration;

use std::sync::Arc;

use anyhow::Context;
use lumalla_ipc::{
    BUS_NAME, DrmDeviceInfo, KeyBindingInfo, LayoutOutputInfo, LayoutSpacesInfo, ModsInfo,
    OutputConfigInfo, OutputInfo, WindowInfo, WindowManagerProxy, WindowRuleInfo, XkbInfo,
};
use lumalla_shared::{CallbackRef, Mods, Output, geometry_field_to_dbus};
use mlua::{
    Error as LuaError, FromLua, Function as LuaFunction, IntoLua, Lua, Result as LuaResult,
    Table as LuaTable, Value as LuaValue,
};
use zbus::blocking::Connection;
use zbus::blocking::fdo::DBusProxy;
use zbus::names::BusName;

use crate::{args::Args, callback::CallbackState};

const LUA_MODULE_NAME: &str = "lumalla";

fn dbus_result<T>(result: zbus::fdo::Result<T>) -> LuaResult<T> {
    result.map_err(|err| LuaError::external(Arc::new(err)))
}

/// Client handle for external configuration.
#[derive(Clone)]
pub struct DbusConfigClient {
    pub(crate) proxy: Arc<WindowManagerProxy<'static>>,
    connection: &'static Connection,
    /// Unique name of the compositor that owned [`BUS_NAME`] at connect time.
    pub(crate) compositor_unique_name: String,
}

impl DbusConfigClient {
    /// Connect to a running compositor.
    pub fn connect() -> anyhow::Result<Self> {
        let connection = Box::leak(Box::new(
            Connection::session().context("Failed to connect to session bus")?,
        ));
        let bus_name = BusName::try_from(BUS_NAME).context("Invalid compositor bus name")?;
        let dbus_proxy =
            DBusProxy::new(connection).context("Failed to create org.freedesktop.DBus proxy")?;
        let compositor_unique_name = dbus_proxy
            .get_name_owner(bus_name)
            .with_context(|| format!("Compositor bus name `{BUS_NAME}` is not owned"))?
            .to_string();
        let proxy = WindowManagerProxy::new(connection).context("Failed to create D-Bus proxy")?;
        Ok(Self {
            proxy: Arc::new(proxy),
            connection,
            compositor_unique_name,
        })
    }

    pub(crate) fn connection(&self) -> &'static Connection {
        self.connection
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
    init_dbus_input(lua, &module, client.clone())?;
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
    let xkb_client = client.clone();
    module.set(
        "set_xkb",
        lua.create_function(move |_, config: ConfigXkb| {
            dbus_result(xkb_client.proxy.set_xkb(XkbInfo {
                rules: config.rules.unwrap_or_default(),
                model: config.model.unwrap_or_default(),
                layout: config.layout.unwrap_or_default(),
                variant: config.variant.unwrap_or_default(),
                options: config.options.unwrap_or_default(),
            }))?;
            Ok(())
        })?,
    )?;

    module.set(
        "map_key",
        lua.create_function(move |_, keymap: ConfigKeymap| {
            let callback = callback_state.register_keymap_callback(keymap.callback);
            dbus_result(client.proxy.map_key(KeyBindingInfo {
                binding_id: callback.callback_id.to_string(),
                key: keymap.key,
                mods: ModsInfo::from(keymap.mods),
                on_release: keymap.on_release,
                consume: keymap.consume,
            }))?;
            Ok(())
        })?,
    )?;
    Ok(())
}

fn init_dbus_output(lua: &Lua, module: &LuaTable, client: DbusConfigClient) -> LuaResult<()> {
    let layout_client = client.clone();
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
            dbus_result(layout_client.proxy.set_layout(spaces))?;
            Ok(())
        })?,
    )?;

    let get_client = client.clone();
    module.set(
        "get_outputs",
        lua.create_function(move |lua, ()| {
            let outputs = dbus_result(get_client.proxy.get_outputs())?;
            let table = lua.create_table()?;
            for (index, output) in outputs.into_iter().enumerate() {
                table.set(index + 1, ConfigOutput::from(&Output::from(&output)))?;
            }
            Ok(table)
        })?,
    )?;

    let add_client = client.clone();
    module.set(
        "add_output",
        lua.create_function(move |_, output: ConfigOutput| {
            dbus_result(add_client.proxy.add_output(OutputInfo {
                name: output.name,
                description: output.description,
                x: output.x,
                y: output.y,
                width: output.width,
                height: output.height,
                scale: output.scale,
                refresh_mhz: output.refresh_mhz,
                physical_width_mm: output.physical_width_mm,
                physical_height_mm: output.physical_height_mm,
                is_virtual: output.is_virtual,
            }))?;
            Ok(())
        })?,
    )?;

    let remove_client = client;
    module.set(
        "remove_output",
        lua.create_function(move |_, name: String| {
            dbus_result(remove_client.proxy.remove_output(&name))?;
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
        let table = value
            .as_table()
            .ok_or_else(|| LuaError::FromLuaConversionError {
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
    module.set(
        "spawn",
        lua.create_function(move |_, spawn: ConfigSpawn| {
            dbus_result(client.proxy.spawn(&spawn.command, spawn.args))?;
            Ok(())
        })?,
    )?;
    Ok(())
}

fn init_dbus_input(lua: &Lua, module: &LuaTable, client: DbusConfigClient) -> LuaResult<()> {
    module.set(
        "sleep",
        lua.create_function(|_, seconds: f64| {
            if seconds > 0.0 {
                thread::sleep(Duration::from_secs_f64(seconds));
            }
            Ok(())
        })?,
    )?;

    let key_client = client.clone();
    module.set(
        "key",
        lua.create_function(move |_, name: String| {
            dbus_result(key_client.proxy.inject_key(&name))?;
            Ok(())
        })?,
    )?;

    let type_client = client.clone();
    module.set(
        "type",
        lua.create_function(move |_, text: String| {
            dbus_result(type_client.proxy.type_text(&text))?;
            Ok(())
        })?,
    )?;

    let move_client = client.clone();
    module.set(
        "pointer_move",
        lua.create_function(move |_, (x, y): (f64, f64)| {
            dbus_result(move_client.proxy.inject_pointer_move(x, y))?;
            Ok(())
        })?,
    )?;

    let click_client = client.clone();
    module.set(
        "click",
        lua.create_function(move |_, (x, y, button): (f64, f64, Option<u32>)| {
            dbus_result(
                click_client
                    .proxy
                    .inject_pointer_click(x, y, button.unwrap_or(0)),
            )?;
            Ok(())
        })?,
    )?;

    let screenshot_client = client;
    module.set(
        "screenshot",
        lua.create_function(
            move |_, (x, y, width, height, path): (i32, i32, i32, i32, String)| {
                dbus_result(
                    screenshot_client
                        .proxy
                        .capture_screenshot(x, y, width, height, &path),
                )?;
                Ok(())
            },
        )?,
    )?;

    Ok(())
}

fn init_dbus_window(lua: &Lua, module: &LuaTable, client: DbusConfigClient) -> LuaResult<()> {
    let set_client = client.clone();
    module.set(
        "set_window",
        lua.create_function(move |_, window: ConfigWindowUpdate| {
            dbus_result(set_client.proxy.set_window(
                window.id.unwrap_or(0),
                geometry_field_to_dbus(window.x),
                geometry_field_to_dbus(window.y),
                geometry_field_to_dbus(window.width),
                geometry_field_to_dbus(window.height),
            ))?;
            Ok(())
        })?,
    )?;

    let focus_win_client = client.clone();
    module.set(
        "focus_window",
        lua.create_function(move |_, window: ConfigFocusWindow| {
            dbus_result(
                focus_win_client
                    .proxy
                    .focus_window(window.id.unwrap_or(0), window.raise),
            )?;
            Ok(())
        })?,
    )?;

    let raise_client = client.clone();
    module.set(
        "raise_window",
        lua.create_function(move |_, window: ConfigRaiseWindow| {
            dbus_result(raise_client.proxy.raise_window(window.id.unwrap_or(0)))?;
            Ok(())
        })?,
    )?;

    let get_client = client.clone();
    module.set(
        "get_windows",
        lua.create_function(move |lua, ()| {
            let windows = dbus_result(get_client.proxy.get_windows())?;
            let table = lua.create_table()?;
            for (index, window) in windows.into_iter().enumerate() {
                table.set(index + 1, ConfigWindow::from(window))?;
            }
            Ok(table)
        })?,
    )?;

    let focus_client = client.clone();
    module.set(
        "get_focused_window",
        lua.create_function(move |_, ()| {
            let id = dbus_result(focus_client.proxy.get_focused_window())?;
            Ok(if id == 0 { None } else { Some(id) })
        })?,
    )?;

    let rules_client = client.clone();
    module.set(
        "add_window_rule",
        lua.create_function(move |_, window_rule: ConfigWindowRule| {
            dbus_result(rules_client.proxy.add_window_rule(WindowRuleInfo {
                app_id: window_rule.app_id,
                x: geometry_field_to_dbus(window_rule.x),
                y: geometry_field_to_dbus(window_rule.y),
                width: geometry_field_to_dbus(window_rule.width),
                height: geometry_field_to_dbus(window_rule.height),
            }))?;
            Ok(())
        })?,
    )?;

    module.set(
        "clear_window_rules",
        lua.create_function(move |_, ()| {
            dbus_result(client.proxy.clear_window_rules())?;
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
            create_quit_callback(lua, client.clone()).map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f1",
            create_vt_callback(lua, client.clone(), 1).map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f2",
            create_vt_callback(lua, client.clone(), 2).map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f3",
            create_vt_callback(lua, client.clone(), 3).map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f4",
            create_vt_callback(lua, client.clone(), 4).map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f5",
            create_vt_callback(lua, client.clone(), 5).map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f6",
            create_vt_callback(lua, client.clone(), 6).map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f7",
            create_vt_callback(lua, client.clone(), 7).map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f8",
            create_vt_callback(lua, client.clone(), 8).map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f9",
            create_vt_callback(lua, client.clone(), 9).map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f10",
            create_vt_callback(lua, client.clone(), 10).map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f11",
            create_vt_callback(lua, client.clone(), 11).map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
        (
            Mods {
                ctrl: true,
                alt: true,
                ..Default::default()
            },
            "f12",
            create_vt_callback(lua, client.clone(), 12).map_err(|err| anyhow::anyhow!("{err}"))?,
        ),
    ];

    for (mods, key_name, callback) in default_keymaps {
        let callback_ref = callback_state.register_keymap_callback(callback);
        client
            .proxy
            .map_key(KeyBindingInfo {
                binding_id: callback_ref.callback_id.to_string(),
                key: key_name.to_string(),
                mods: ModsInfo::from(mods),
                on_release: false,
                consume: true,
            })
            .context("Failed to register default keymap")?;
    }

    Ok(())
}

struct ConfigKeymap {
    key: String,
    mods: Mods,
    on_release: bool,
    consume: bool,
    callback: LuaFunction,
}

struct ConfigXkb {
    rules: Option<String>,
    model: Option<String>,
    layout: Option<String>,
    variant: Option<String>,
    options: Option<String>,
}

impl FromLua for ConfigXkb {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        let table = value
            .as_table()
            .ok_or_else(|| LuaError::FromLuaConversionError {
                from: "LuaXkb",
                to: String::from("ConfigXkb"),
                message: Some(String::from("Expected a Lua table for XKB config")),
            })?;
        Ok(Self {
            rules: table.get("rules").unwrap_or(None),
            model: table.get("model").unwrap_or(None),
            layout: table.get("layout").unwrap_or(None),
            variant: table.get("variant").unwrap_or(None),
            options: table.get("options").unwrap_or(None),
        })
    }
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
        let on = table
            .get::<Option<String>>("on")
            .unwrap_or(None)
            .unwrap_or_else(|| String::from("down"));
        let on_release = match on.as_str() {
            "down" | "press" => false,
            "up" | "release" => true,
            other => {
                return Err(LuaError::FromLuaConversionError {
                    from: "LuaKeymap",
                    to: String::from("ConfigKeymap"),
                    message: Some(format!(
                        "Invalid map_key on value `{other}` (expected \"down\" or \"up\")"
                    )),
                });
            }
        };
        Ok(Self {
            key: table.get("key")?,
            mods,
            on_release,
            consume: table.get("consume").unwrap_or(true),
            callback: table.get("callback")?,
        })
    }
}

struct ConfigLayout {
    spaces: HashMap<String, Vec<ConfigOutput>>,
}

impl FromLua for ConfigLayout {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        let table = value
            .as_table()
            .ok_or_else(|| LuaError::FromLuaConversionError {
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
    description: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    scale: i32,
    refresh_mhz: i32,
    physical_width_mm: i32,
    physical_height_mm: i32,
    is_virtual: bool,
}

impl From<&Output> for ConfigOutput {
    fn from(value: &Output) -> Self {
        Self {
            name: value.name.clone(),
            description: value.description.clone(),
            x: value.location.0,
            y: value.location.1,
            width: value.size.0,
            height: value.size.1,
            scale: value.scale,
            refresh_mhz: value.refresh_mhz,
            physical_width_mm: value.physical_width_mm,
            physical_height_mm: value.physical_height_mm,
            is_virtual: value.is_virtual,
        }
    }
}

impl FromLua for ConfigOutput {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        let table = value
            .as_table()
            .ok_or_else(|| LuaError::FromLuaConversionError {
                from: "LuaOutput",
                to: String::from("ConfigOutput"),
                message: Some(String::from("Expected a Lua table for the ConfigOutput")),
            })?;
        Ok(Self {
            name: table.get("name")?,
            description: table
                .get::<Option<String>>("description")
                .unwrap_or(None)
                .unwrap_or_default(),
            x: table.get("x")?,
            y: table.get("y")?,
            width: table.get("width")?,
            height: table.get("height")?,
            scale: table.get("scale").unwrap_or(1),
            refresh_mhz: table.get("refresh_mhz").unwrap_or(60_000),
            physical_width_mm: table
                .get("mm_width")
                .or_else(|_| table.get("physical_width_mm"))
                .unwrap_or(0),
            physical_height_mm: table
                .get("mm_height")
                .or_else(|_| table.get("physical_height_mm"))
                .unwrap_or(0),
            is_virtual: table
                .get("virtual")
                .or_else(|_| table.get("is_virtual"))
                .unwrap_or(false),
        })
    }
}

impl IntoLua for ConfigOutput {
    fn into_lua(self, lua: &Lua) -> LuaResult<LuaValue> {
        let lua_output = lua.create_table()?;
        lua_output.set("name", self.name)?;
        lua_output.set("description", self.description)?;
        lua_output.set("x", self.x)?;
        lua_output.set("y", self.y)?;
        lua_output.set("width", self.width)?;
        lua_output.set("height", self.height)?;
        lua_output.set("scale", self.scale)?;
        lua_output.set("refresh_mhz", self.refresh_mhz)?;
        lua_output.set("mm_width", self.physical_width_mm)?;
        lua_output.set("mm_height", self.physical_height_mm)?;
        lua_output.set("virtual", self.is_virtual)?;
        lua_output.into_lua(lua)
    }
}

struct ConfigSpawn {
    command: String,
    args: Vec<String>,
}

impl FromLua for ConfigSpawn {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        let table = value
            .as_table()
            .ok_or_else(|| LuaError::FromLuaConversionError {
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

struct ConfigWindowUpdate {
    id: Option<u32>,
    x: Option<i32>,
    y: Option<i32>,
    width: Option<i32>,
    height: Option<i32>,
}

impl FromLua for ConfigWindowUpdate {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        let table = value
            .as_table()
            .ok_or_else(|| LuaError::FromLuaConversionError {
                from: "LuaWindowUpdate",
                to: String::from("ConfigWindowUpdate"),
                message: Some(String::from("Expected a Lua table for the window update")),
            })?;
        Ok(Self {
            id: table.get("id").unwrap_or(None),
            x: table.get("x").unwrap_or(None),
            y: table.get("y").unwrap_or(None),
            width: table.get("width").unwrap_or(None),
            height: table.get("height").unwrap_or(None),
        })
    }
}

struct ConfigFocusWindow {
    id: Option<u32>,
    raise: bool,
}

impl FromLua for ConfigFocusWindow {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        let table = value
            .as_table()
            .ok_or_else(|| LuaError::FromLuaConversionError {
                from: "LuaFocusWindow",
                to: String::from("ConfigFocusWindow"),
                message: Some(String::from("Expected a Lua table for focus_window")),
            })?;
        Ok(Self {
            id: table.get("id").unwrap_or(None),
            raise: table.get("raise").unwrap_or(false),
        })
    }
}

struct ConfigRaiseWindow {
    id: Option<u32>,
}

impl FromLua for ConfigRaiseWindow {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        let table = value
            .as_table()
            .ok_or_else(|| LuaError::FromLuaConversionError {
                from: "LuaRaiseWindow",
                to: String::from("ConfigRaiseWindow"),
                message: Some(String::from("Expected a Lua table for raise_window")),
            })?;
        Ok(Self {
            id: table.get("id").unwrap_or(None),
        })
    }
}

struct ConfigWindow {
    id: u32,
    app_id: String,
    title: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    focused: bool,
}

impl From<WindowInfo> for ConfigWindow {
    fn from(window: WindowInfo) -> Self {
        Self {
            id: window.id,
            app_id: window.app_id,
            title: window.title,
            x: window.x,
            y: window.y,
            width: window.width,
            height: window.height,
            focused: window.focused,
        }
    }
}

impl IntoLua for ConfigWindow {
    fn into_lua(self, lua: &Lua) -> LuaResult<LuaValue> {
        let table = lua.create_table()?;
        table.set("id", self.id)?;
        table.set("app_id", self.app_id)?;
        table.set("title", self.title)?;
        table.set("x", self.x)?;
        table.set("y", self.y)?;
        table.set("width", self.width)?;
        table.set("height", self.height)?;
        table.set("focused", self.focused)?;
        table.into_lua(lua)
    }
}

struct ConfigWindowRule {
    app_id: String,
    x: Option<i32>,
    y: Option<i32>,
    width: Option<i32>,
    height: Option<i32>,
}

impl FromLua for ConfigWindowRule {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        let table = value
            .as_table()
            .ok_or_else(|| LuaError::FromLuaConversionError {
                from: "LuaWindowRule",
                to: String::from("ConfigWindowRule"),
                message: Some(String::from(
                    "Expected a Lua table for the ConfigWindowRule",
                )),
            })?;
        Ok(Self {
            app_id: table.get("app_id")?,
            x: table.get("x").unwrap_or(None),
            y: table.get("y").unwrap_or(None),
            width: table.get("width").unwrap_or(None),
            height: table.get("height").unwrap_or(None),
        })
    }
}

pub(crate) fn load_config_files(lua: &Lua, args: &Args) -> anyhow::Result<()> {
    if let Some(config_path) = &args.config_path {
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
    args: &Args,
) -> anyhow::Result<()> {
    if let Some(config_path) = &args.config_path {
        watcher.watch(config_path.as_ref())?;
    } else {
        let xdg_dirs = xdg::BaseDirectories::with_prefix("lumalla").unwrap();
        for path in xdg_dirs.list_config_files("") {
            watcher.watch(path.as_ref())?;
        }
    }
    Ok(())
}

pub(crate) fn reload_config_file(
    lua: &Lua,
    client: &DbusConfigClient,
    callback_state: &CallbackState,
    path: &std::path::Path,
) -> anyhow::Result<()> {
    client
        .proxy
        .clear_keymaps()
        .context("Failed to clear keymaps before config reload")?;
    callback_state.forget_keymap_callbacks();
    set_default_keymaps(lua, client, callback_state)?;
    exec_config_file(lua, path)
}

fn exec_config_file(lua: &Lua, path: &std::path::Path) -> anyhow::Result<()> {
    let user_config = std::fs::read(path)?;
    lua.load(&user_config)
        .exec()
        .map_err(|err| anyhow::anyhow!("Unable to run config: {err}"))?;
    Ok(())
}

/// Expose `require("lumalla")` as a global for REPL convenience.
pub(crate) fn prepare_repl_env(lua: &Lua) -> anyhow::Result<()> {
    let lumalla: LuaTable = lua
        .load("return require('lumalla')")
        .eval()
        .map_err(|err| anyhow::anyhow!("Unable to preload lumalla for REPL: {err}"))?;
    lua.globals()
        .set("lumalla", lumalla)
        .map_err(|err| anyhow::anyhow!("Unable to set lumalla global: {err}"))?;
    Ok(())
}

/// Evaluate a REPL line in the config Lua VM.
///
/// Tries the chunk as an expression first (so `1+2` prints a value), then as a
/// statement chunk (same as config files). Only prints when the chunk returns
/// values — bare assignments print nothing.
pub(crate) fn eval_repl_chunk(lua: &Lua, chunk: &str) -> Result<String, String> {
    let trimmed = chunk.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }

    let function = match lua.load(format!("return {trimmed}")).into_function() {
        Ok(function) => function,
        Err(_) => lua
            .load(trimmed)
            .into_function()
            .map_err(|err| err.to_string())?,
    };

    let values: mlua::MultiValue = function.call(()).map_err(|err| err.to_string())?;
    if values.is_empty() {
        return Ok(String::new());
    }

    Ok(values
        .iter()
        .map(|value| format_lua_value(value, 0, &mut Vec::new()))
        .collect::<Vec<_>>()
        .join("\t"))
}

const REPL_MAX_DEPTH: usize = 8;
const REPL_MAX_ENTRIES: usize = 64;

fn format_lua_value(
    value: &LuaValue,
    depth: usize,
    visited: &mut Vec<*const std::ffi::c_void>,
) -> String {
    match value {
        LuaValue::Nil => String::from("nil"),
        LuaValue::Boolean(b) => b.to_string(),
        LuaValue::Integer(i) => i.to_string(),
        LuaValue::Number(n) => format_lua_number(*n),
        LuaValue::String(s) => format_lua_string(s),
        LuaValue::Table(table) => format_lua_table(table, depth, visited),
        LuaValue::Function(_) => String::from("<function>"),
        LuaValue::Thread(_) => String::from("<thread>"),
        LuaValue::UserData(_) => String::from("<userdata>"),
        LuaValue::LightUserData(_) => String::from("<lightuserdata>"),
        LuaValue::Error(err) => format!("<error: {err}>"),
        _ => format!("{value:?}"),
    }
}

fn format_lua_number(n: f64) -> String {
    if n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

fn format_lua_string(s: &mlua::String) -> String {
    match s.to_str() {
        Ok(text) => format!("\"{}\"", escape_lua_string(&text)),
        Err(_) => format!("{:?}", s.as_bytes()),
    }
}

fn escape_lua_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", u32::from(c))),
            c => out.push(c),
        }
    }
    out
}

fn format_lua_table(
    table: &LuaTable,
    depth: usize,
    visited: &mut Vec<*const std::ffi::c_void>,
) -> String {
    let ptr = LuaValue::Table(table.clone()).to_pointer();
    if visited.contains(&ptr) {
        return String::from("{...}");
    }
    if depth >= REPL_MAX_DEPTH {
        return String::from("{...}");
    }

    visited.push(ptr);
    let mut entries = Vec::new();
    let mut truncated = false;

    let pairs = table.pairs::<LuaValue, LuaValue>();
    for (index, pair) in pairs.enumerate() {
        if index >= REPL_MAX_ENTRIES {
            truncated = true;
            break;
        }
        let Ok((key, value)) = pair else {
            truncated = true;
            break;
        };
        let key_text = format_table_key(&key, depth + 1, visited);
        let value_text = format_lua_value(&value, depth + 1, visited);
        entries.push(format!("{key_text} = {value_text}"));
    }

    visited.pop();

    if entries.is_empty() {
        return String::from("{}");
    }

    let indent = "  ".repeat(depth + 1);
    let closing = "  ".repeat(depth);
    let mut out = String::from("{\n");
    for entry in &entries {
        out.push_str(&indent);
        out.push_str(entry);
        out.push(',');
        out.push('\n');
    }
    if truncated {
        out.push_str(&indent);
        out.push_str("...");
        out.push('\n');
    }
    out.push_str(&closing);
    out.push('}');
    out
}

fn format_table_key(
    key: &LuaValue,
    depth: usize,
    visited: &mut Vec<*const std::ffi::c_void>,
) -> String {
    match key {
        LuaValue::String(s) => match s.to_str() {
            Ok(text) if is_lua_identifier(&text) => text.to_owned(),
            Ok(text) => format!("[\"{}\"]", escape_lua_string(&text)),
            Err(_) => format!("[{:?}]", s.as_bytes()),
        },
        LuaValue::Integer(i) => format!("[{i}]"),
        LuaValue::Number(n) => format!("[{}]", format_lua_number(*n)),
        LuaValue::Boolean(b) => format!("[{b}]"),
        other => format!("[{}]", format_lua_value(other, depth, visited)),
    }
}

fn is_lua_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod repl_eval_tests {
    use super::{eval_repl_chunk, format_lua_value};
    use mlua::{Lua, Value as LuaValue};

    #[test]
    fn evaluates_expressions() {
        let lua = Lua::new();
        assert_eq!(eval_repl_chunk(&lua, "1 + 2").unwrap(), "3");
        assert_eq!(eval_repl_chunk(&lua, "\"hi\"").unwrap(), "\"hi\"");
        assert_eq!(eval_repl_chunk(&lua, "true").unwrap(), "true");
    }

    #[test]
    fn evaluates_statements_then_reads_globals() {
        let lua = Lua::new();
        assert_eq!(eval_repl_chunk(&lua, "x = 40 + 2").unwrap(), "");
        assert_eq!(eval_repl_chunk(&lua, "x").unwrap(), "42");
    }

    #[test]
    fn evaluates_return_chunks() {
        let lua = Lua::new();
        assert_eq!(eval_repl_chunk(&lua, "return 7").unwrap(), "7");
    }

    #[test]
    fn surfaces_lua_errors() {
        let lua = Lua::new();
        let err = eval_repl_chunk(&lua, "error('boom')").unwrap_err();
        assert!(err.contains("boom"), "{err}");
    }

    #[test]
    fn formats_nil() {
        assert_eq!(format_lua_value(&LuaValue::Nil, 0, &mut Vec::new()), "nil");
    }

    #[test]
    fn pretty_prints_tables() {
        let lua = Lua::new();
        let out = eval_repl_chunk(&lua, "{ name = \"a\", n = 1, nested = { ok = true } }").unwrap();
        assert!(out.contains("name = \"a\""), "{out}");
        assert!(out.contains("n = 1"), "{out}");
        assert!(out.contains("ok = true"), "{out}");
    }

    #[test]
    fn pretty_prints_array_keys() {
        let lua = Lua::new();
        let out = eval_repl_chunk(&lua, "{ \"x\", \"y\" }").unwrap();
        assert!(out.contains("[1] = \"x\""), "{out}");
        assert!(out.contains("[2] = \"y\""), "{out}");
    }

    #[test]
    fn pretty_prints_cycles_without_looping() {
        let lua = Lua::new();
        assert_eq!(eval_repl_chunk(&lua, "t = {}; t.self = t; return t").unwrap().contains("{...}"), true);
    }
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
