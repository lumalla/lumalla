use lumalla_shared::{Comms, DisplayMessage, WindowRule};
use mlua::{
    Error as LuaError, FromLua, Lua, Result as LuaResult, Table as LuaTable, Value as LuaValue,
};

pub(crate) fn init(lua: &Lua, module: &LuaTable, comms: Comms) -> LuaResult<()> {
    init_add_window_rule(lua, module, comms.clone())?;
    init_close_current_window(lua, module, comms)?;

    Ok(())
}

fn init_close_current_window(lua: &Lua, module: &LuaTable, comms: Comms) -> LuaResult<()> {
    module.set(
        "close_current_window",
        lua.create_function(move |_, ()| {
            comms.display(DisplayMessage::CloseCurrentWindow);
            Ok(())
        })?,
    )?;

    Ok(())
}

fn init_add_window_rule(lua: &Lua, module: &LuaTable, comms: Comms) -> LuaResult<()> {
    module.set(
        "add_window_rule",
        lua.create_function(move |_, window_rule: ConfigWindowRule| {
            comms.display(DisplayMessage::AddWindowRule(window_rule.into()));
            Ok(())
        })?,
    )?;

    Ok(())
}

struct ConfigWindowRule {
    app_id: String,
    zone: String,
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

        Ok(ConfigWindowRule {
            app_id: table.get("app_id")?,
            zone: table.get("zone")?,
        })
    }
}

impl From<ConfigWindowRule> for WindowRule {
    fn from(value: ConfigWindowRule) -> Self {
        Self {
            app_id: value.app_id,
            zone: value.zone,
        }
    }
}
