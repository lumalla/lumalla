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
    next_object_id: ObjectId,
    freed_object_ids: Vec<ObjectId>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            objects: HashMap::new(),
            next_object_id: MIN_SERVER_OBJECT_ID,
            freed_object_ids: Vec::new(),
        }
    }

    pub fn interface_index(&self, object_id: ObjectId) -> Option<InterfaceIndex> {
        self.objects.get(&object_id).copied()
    }

    pub fn register_object(&mut self, object_id: ObjectId, interface_index: InterfaceIndex) {
        self.objects.insert(object_id, interface_index);
    }

    pub fn create_object(&mut self, interface_index: InterfaceIndex) -> ObjectId {
        let object_id = self.next_object_id();
        self.objects.insert(object_id, interface_index);
        object_id
    }

    fn next_object_id(&mut self) -> ObjectId {
        self.freed_object_ids.pop().unwrap_or_else(|| {
            let object_id = self.next_object_id;
            self.next_object_id += 1;
            object_id
        })
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
        fds: &mut VecDeque<RawFd>,
    ) -> anyhow::Result<()> {
        match handler {
            II_WL_DISPLAY => WlDisplay::handle_request(self, ctx, header, data, fds),
            _ => {
                ctx.writer
                    .wl_display_error(header.object_id)
                    .object_id(header.object_id)
                    .code(WL_DISPLAY_ERROR_INVALID_METHOD)
                    .message("Invalid method");
                anyhow::bail!("Invalid method");
            }
        }
    }
}

pub const II_WL_DISPLAY: InterfaceIndex = 0;
