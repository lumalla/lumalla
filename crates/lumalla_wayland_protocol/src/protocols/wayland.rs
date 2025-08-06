use anyhow::Context;

use crate::{
    ObjectId,
    buffer::{MessageHeader, Writer},
    client::Ctx,
};

// Generated
use lumalla_wayland_protocol_macros::wayland_protocol;

wayland_protocol!("src/protocols/wayland.xml");
