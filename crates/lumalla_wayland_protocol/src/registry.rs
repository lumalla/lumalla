use std::collections::HashMap;

use crate::{ObjectId, Opcode, protocols::WlDisplay};

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
}

trait RegistryAccess {
    fn registry(&mut self) -> &mut Registry;
}

trait RequestHandler {
    fn handle_request(&mut self, handler: InterfaceIndex, opcode: Opcode, data: &[u8]) -> bool;
}

impl<T> RequestHandler for T
where
    T: RegistryAccess + WlDisplay,
{
    fn handle_request(&mut self, handler: InterfaceIndex, opcode: Opcode, data: &[u8]) -> bool {
        match handler {
            WL_DISPLAY => WlDisplay::handle_request(self, opcode, data),
            _ => false,
        }
    }
}

/// Forward the request to the appropriate handler
/// Returns true if the request was handled, false otherwise
#[must_use]
pub fn handle_request<T: RequestHandler + RegistryAccess>(
    state: &mut T,
    object_id: ObjectId,
    opcode: Opcode,
    data: &[u8],
) -> bool {
    if let Some(interface_index) = state.registry().objects.get(&object_id).copied() {
        return state.handle_request(interface_index, opcode, data);
    }

    false
}

const WL_DISPLAY: InterfaceIndex = 0;
