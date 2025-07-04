use std::collections::HashMap;

use lumalla_shared::{Comms, ConfigMessage, Output};
use mlua::{
    Error as LuaError, FromLua, Function as LuaFunction, IntoLua, Lua, Result as LuaResult,
    Table as LuaTable, Value as LuaValue,
};

use crate::{CallbackState, ConfigState};

/// Set the output functions on the base module
pub(crate) fn init(
    lua: &Lua,
    module: &LuaTable,
    comms: Comms,
    callback_state: CallbackState,
) -> LuaResult<()> {
    init_on_connector_change(lua, module, comms.clone(), callback_state)?;
    init_set_layout(lua, module, comms)?;

    Ok(())
}

fn init_on_connector_change(
    lua: &Lua,
    module: &LuaTable,
    comms: Comms,
    callback_state: CallbackState,
) -> LuaResult<()> {
    module.set(
        "on_connector_change",
        lua.create_function(move |_, callback: LuaFunction| {
            let callback = callback_state.register_callback(callback);
            comms.config(ConfigMessage::SetOnConnectorChange(callback));
            Ok(())
        })?,
    )?;

    Ok(())
}

fn init_set_layout(lua: &Lua, module: &LuaTable, comms: Comms) -> LuaResult<()> {
    module.set(
        "set_layout",
        lua.create_function(move |_, layout: ConfigLayout| {
            comms.config(ConfigMessage::SetLayout {
                spaces: layout
                    .spaces
                    .into_iter()
                    .map(|(name, outputs)| {
                        (
                            name,
                            outputs
                                .into_iter()
                                .map(|config_output| {
                                    (config_output.name, config_output.x, config_output.y)
                                })
                                .collect(),
                        )
                    })
                    .collect(),
            });
            Ok(())
        })?,
    )?;

    Ok(())
}

impl ConfigState {
    pub(crate) fn on_connector_change(&mut self) -> anyhow::Result<()> {
        if let Some(on_connector_change) = self.on_connector_change {
            return self.callback_state.run_callback(
                on_connector_change,
                self.outputs
                    .values()
                    .map(Into::<ConfigOutput>::into)
                    .collect::<Vec<_>>(),
            );
        }

        Ok(())
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

        Ok(ConfigLayout { spaces })
    }
}

struct ConfigOutput {
    name: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl From<&Output> for ConfigOutput {
    fn from(value: &Output) -> Self {
        let location = value.location;
        let size = value.size;
        ConfigOutput {
            name: value.name.clone(),
            x: location.0,
            y: location.1,
            width: size.0,
            height: size.1,
        }
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

impl FromLua for ConfigOutput {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        let table = value
            .as_table()
            .ok_or_else(|| LuaError::FromLuaConversionError {
                from: "LuaOutput",
                to: String::from("ConfigOutput"),
                message: Some(String::from("Expected a Lua table for the ConfigOutput")),
            })?;

        Ok(ConfigOutput {
            name: table.get("name")?,
            x: table.get("x")?,
            y: table.get("y")?,
            width: table.get("width")?,
            height: table.get("height")?,
        })
    }
}
