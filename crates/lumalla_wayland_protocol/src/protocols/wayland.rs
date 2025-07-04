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

#[derive(Debug)]
pub struct Sync {
    pub callback: ObjectId,
}
