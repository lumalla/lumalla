use crate::ObjectId;

// Generated

pub trait WlDisplay {
    fn sync(&mut self, params: &Sync);

    fn handle_request(&mut self, opcode: u16, data: &[u8]) -> bool {
        match opcode {
            1 => self.sync(unsafe { &*(data.as_ptr() as *const Sync) }),
            _ => return false,
        }

        true
    }
}

impl<T> WlDisplay for T {
    fn sync(&mut self, _params: &Sync) {}
}

#[derive(Debug)]
struct Sync {
    callback: ObjectId,
}
