use std::process::Command;

use log::{error, info};
use lumalla_shared::{Comms, ConfigMessage, DisplayMessage};
use mlua::{
    Error as LuaError, FromLua, Lua, Result as LuaResult, Table as LuaTable, Value as LuaValue,
};

use crate::ConfigState;

pub(crate) fn init(lua: &Lua, module: &LuaTable, comms: Comms) -> LuaResult<()> {
    init_spawn(lua, module, comms.clone())?;
    init_focus_or_spawn(lua, module, comms)?;

    Ok(())
}

fn init_spawn(lua: &Lua, module: &LuaTable, comms: Comms) -> LuaResult<()> {
    module.set(
        "spawn",
        lua.create_function(move |_, spawn: ConfigSpawn| {
            comms.config(ConfigMessage::Spawn(spawn.command, spawn.args));
            Ok(())
        })?,
    )?;

    Ok(())
}

fn init_focus_or_spawn(lua: &Lua, module: &LuaTable, comms: Comms) -> LuaResult<()> {
    module.set(
        "focus_or_spawn",
        lua.create_function(move |_, (app_id, command): (String, ConfigSpawn)| {
            comms.display(DisplayMessage::FocusOrSpawn {
                app_id,
                command: command.command,
                args: command.args,
            });
            Ok(())
        })?,
    )?;

    Ok(())
}

impl ConfigState {
    pub(crate) fn spawn(&self, command: &str, args: &[String]) {
        info!("Starting program: {command} {args:?}");

        if let Err(e) = Command::new(command)
            .args(args)
            .envs(self.extra_env.iter())
            .spawn()
        {
            error!("Failed to start program {command}: {e}");
        }
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
