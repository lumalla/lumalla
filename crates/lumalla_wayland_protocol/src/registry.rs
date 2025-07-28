use std::{
    collections::{HashMap, VecDeque},
    os::fd::RawFd,
};

use crate::{ObjectId, Opcode, client::Ctx, protocols::WlDisplay};

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
        opcode: Opcode,
        ctx: Ctx,
        data: &[u8],
        fds: &mut VecDeque<RawFd>,
    ) -> bool;
}

impl<T> RequestHandler for T
where
    T: WlDisplay,
{
    fn handle_request(
        &mut self,
        handler: InterfaceIndex,
        opcode: Opcode,
        ctx: Ctx,
        data: &[u8],
        _fds: &mut VecDeque<RawFd>,
    ) -> bool {
        match handler {
            WL_DISPLAY => WlDisplay::handle_request(self, opcode, ctx, data),
            _ => false,
        }
    }
}

const WL_DISPLAY: InterfaceIndex = 0;
