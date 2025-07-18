use log::warn;
use lumalla_shared::{Comms, InputMessage, Mods};
use mlua::{
    FromLua, Function as LuaFunction, Lua, Result as LuaResult, Table as LuaTable,
    Value as LuaValue,
};

use crate::CallbackState;

pub(crate) fn init(
    lua: &Lua,
    module: &LuaTable,
    comms: Comms,
    callback_state: CallbackState,
) -> LuaResult<()> {
    module.set(
        "map_key",
        lua.create_function(move |_, spawn: ConfigKeymap| {
            comms.input(InputMessage::Keymap {
                key_name: spawn.key,
                mods: spawn.mods,
                callback: callback_state.register_callback(spawn.callback),
            });
            Ok(())
        })?,
    )?;

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
                _ => warn!("Unhandled mod key: {mod_key}"),
            }
        }

        let key = table.get::<String>("key")?;
        let callback = table.get::<LuaFunction>("callback")?;

        Ok(ConfigKeymap {
            key,
            mods,
            callback,
        })
    }
}
