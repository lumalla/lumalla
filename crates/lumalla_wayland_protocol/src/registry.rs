use std::{
    collections::{HashMap, VecDeque},
    os::fd::RawFd,
};

use crate::{
    ObjectId,
    buffer::MessageHeader,
    client::Ctx,
    protocols::{WaylandProtocol, WlDisplay, wayland::WL_DISPLAY_ERROR_INVALID_METHOD},
};

type InterfaceIndex = usize;

const MIN_SERVER_OBJECT_ID: ObjectId = 0xFF000000;

#[derive(Debug)]
pub struct Registry {
    objects: HashMap<ObjectId, InterfaceIndex>,
    _next_object_id: ObjectId,
    _freed_object_ids: Vec<ObjectId>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            objects: HashMap::new(),
            _next_object_id: MIN_SERVER_OBJECT_ID,
            _freed_object_ids: Vec::new(),
        }
    }

    pub fn interface_index(&self, object_id: ObjectId) -> Option<InterfaceIndex> {
        self.objects.get(&object_id).copied()
    }
}

pub trait RequestHandler {
    fn handle_request(
        &mut self,
        handler: InterfaceIndex,
        ctx: &mut Ctx,
        header: &MessageHeader,
        data: &[u8],
        fds: &mut VecDeque<RawFd>,
    ) -> anyhow::Result<()>;
}

impl<T> RequestHandler for T
where
    T: WaylandProtocol,
{
    fn handle_request(
        &mut self,
        handler: InterfaceIndex,
        ctx: &mut Ctx,
        header: &MessageHeader,
        data: &[u8],
        _fds: &mut VecDeque<RawFd>,
    ) -> anyhow::Result<()> {
        match handler {
            WL_DISPLAY => WlDisplay::handle_request(self, ctx, header, data),
            _ => {
                ctx.writer
                    .wl_display_error(header.object_id)?
                    .object_id(header.object_id)
                    .code(WL_DISPLAY_ERROR_INVALID_METHOD)
                    .message("Invalid method");
                anyhow::bail!("Invalid method");
            }
        }
    }
}

const WL_DISPLAY: InterfaceIndex = 0;
