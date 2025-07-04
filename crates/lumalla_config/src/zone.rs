use lumalla_shared::{Comms, DisplayMessage, Zone};
use mlua::{
    Error as LuaError, FromLua, Lua, Result as LuaResult, Table as LuaTable, Value as LuaValue,
};

pub(crate) fn init(lua: &Lua, module: &LuaTable, comms: Comms) -> LuaResult<()> {
    init_set_zones(lua, module, comms.clone())?;
    init_move_current_window_to_zone(lua, module, comms)?;
    Ok(())
}

fn init_set_zones(lua: &Lua, module: &LuaTable, comms: Comms) -> LuaResult<()> {
    module.set(
        "set_zones",
        lua.create_function(move |_, zones: Vec<ConfigZone>| {
            comms.display(DisplayMessage::SetZones(
                zones.into_iter().map(Into::into).collect(),
            ));
            Ok(())
        })?,
    )?;

    Ok(())
}

fn init_move_current_window_to_zone(lua: &Lua, module: &LuaTable, comms: Comms) -> LuaResult<()> {
    module.set(
        "move_current_window_to_zone",
        lua.create_function(move |_, zone_name: String| {
            comms.display(DisplayMessage::MoveCurrentWindowToZone(zone_name));
            Ok(())
        })?,
    )?;

    Ok(())
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
        let table = value
            .as_table()
            .ok_or_else(|| LuaError::FromLuaConversionError {
                from: "LuaZone",
                to: String::from("ConfigZone"),
                message: Some(String::from("Expected a Lua table for the ConfigZone")),
            })?;

        Ok(ConfigZone {
            name: table.get("name")?,
            x: table.get("x")?,
            y: table.get("y")?,
            width: table.get("width")?,
            height: table.get("height")?,
            default: table.get("default").unwrap_or(false),
        })
    }
}

impl From<ConfigZone> for Zone {
    fn from(value: ConfigZone) -> Self {
        Self::new(
            value.name,
            value.x,
            value.y,
            value.width,
            value.height,
            value.default,
        )
    }
}
