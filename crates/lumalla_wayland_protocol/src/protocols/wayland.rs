use std::{
    io::{BufWriter, Write},
    os::unix::net::UnixStream,
};

use crate::{ObjectId, client::Ctx};

// Generated

pub trait WlDisplay {
    fn sync(&mut self, ctx: Ctx, params: &Sync);

    fn handle_request(&mut self, opcode: u16, ctx: Ctx, data: &[u8]) -> bool {
        match opcode {
            1 => self.sync(ctx, unsafe { &*(data.as_ptr() as *const Sync) }),
            _ => return false,
        }

        true
    }
}

pub struct WlDisplayErrorObjectId<'client> {
    buffer: &'client mut BufWriter<UnixStream>,
}

impl<'client> WlDisplayErrorObjectId<'client> {
    pub fn object_id(self, object_id: ObjectId) -> std::io::Result<WlDisplayErrorCode<'client>> {
        self.buffer.write(&object_id.to_ne_bytes())?;
        Ok(WlDisplayErrorCode {
            buffer: self.buffer,
        })
    }
}

pub struct WlDisplayErrorCode<'client> {
    buffer: &'client mut BufWriter<UnixStream>,
}

impl<'client> WlDisplayErrorCode<'client> {
    pub fn code(self, code: u32) -> std::io::Result<WlDisplayErrorMessage<'client>> {
        self.buffer.write(&code.to_ne_bytes())?;
        Ok(WlDisplayErrorMessage {
            buffer: self.buffer,
        })
    }
}

pub struct WlDisplayErrorMessage<'client> {
    buffer: &'client mut BufWriter<UnixStream>,
}

impl<'client> WlDisplayErrorMessage<'client> {
    pub fn message(self, message: &str) -> std::io::Result<()> {
        let len = message.len() as u32;
        self.buffer.write(&len.to_ne_bytes())?;
        self.buffer.write(message.as_bytes())?;
        self.buffer.write(&[0u8; 1])?;
        match len % 4 {
            0 => self.buffer.write(&[0u8; 3]),
            1 => self.buffer.write(&[0u8; 2]),
            2 => self.buffer.write(&[0u8; 1]),
            _ => Ok(0),
        }
        .map(|_| ())
    }
}

fn wl_display_error<'client>(
    object_id: ObjectId,
    buffer: &'client mut BufWriter<UnixStream>,
) -> WlDisplayErrorObjectId<'client> {
    WlDisplayErrorObjectId { buffer }
}

#[derive(Debug)]
pub struct Sync {
    pub callback: ObjectId,
}
