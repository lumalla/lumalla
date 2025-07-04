use crate::{ObjectId, Opcode};

#[repr(C)]
pub struct MessageHeader {
    pub object_id: ObjectId,
    pub size: u16,
    pub opcode: Opcode,
}

// TODO: make this fn unsafe, since it relies on the caller provides the correct available bytes
pub fn read_header(buffer: &[u8], available_bytes: usize) -> Option<&MessageHeader> {
    if available_bytes < size_of::<MessageHeader>() {
        return None;
    }

    Some(unsafe { &*(buffer.as_ptr() as *const MessageHeader) })
}
