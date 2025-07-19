use crate::{ObjectId, registry::Registry};

// Generated

pub trait WlDisplay {
    fn sync(&mut self, registry: &mut Registry, params: &Sync);

    fn handle_request(&mut self, opcode: u16, registry: &mut Registry, data: &[u8]) -> bool {
        match opcode {
            1 => self.sync(registry, unsafe { &*(data.as_ptr() as *const Sync) }),
            _ => return false,
        }

        true
    }
}

pub struct WlDisplayErrorObjectId {
    buffer: (),
}

impl WlDisplayErrorObjectId {
    pub fn object_id(self) -> WlDisplayErrorCode {
        // buffer write
        WlDisplayErrorCode { buffer: () }
    }
}

pub struct WlDisplayErrorCode {
    buffer: (),
}

impl WlDisplayErrorCode {
    pub fn code(self) -> WlDisplayErrorMessage {
        // buffer write
        WlDisplayErrorMessage { buffer: () }
    }
}

pub struct WlDisplayErrorMessage {
    buffer: (),
}

impl WlDisplayErrorMessage {
    pub fn message(self) {
        // buffer write
    }
}

fn wl_display_error(object_id: ObjectId, buffer: ()) -> WlDisplayErrorObjectId {
    WlDisplayErrorObjectId { buffer }
}

#[derive(Debug)]
pub struct Sync {
    pub callback: ObjectId,
}
