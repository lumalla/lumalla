use anyhow::Context;

use crate::{
    ObjectId,
    buffer::{MessageHeader, Writer},
    client::Ctx,
};

// Generated

pub trait WlDisplay {
    fn sync(&mut self, ctx: &Ctx, object_id: ObjectId, params: &Sync);

    fn handle_request(
        &mut self,
        ctx: &mut Ctx,
        header: &MessageHeader,
        data: &[u8],
    ) -> anyhow::Result<()> {
        match header.opcode {
            1 => self.sync(ctx, header.object_id, unsafe {
                &*(data.as_ptr() as *const Sync)
            }),
            _ => {
                ctx.writer
                    .wl_display_error(header.object_id)?
                    .object_id(header.object_id)
                    .code(WL_DISPLAY_ERROR_INVALID_METHOD)
                    .message("Invalid method");
                anyhow::bail!("Invalid method");
            }
        }

        Ok(())
    }
}

impl Writer {
    pub fn wl_display_error(
        &mut self,
        object_id: ObjectId,
    ) -> anyhow::Result<WlDisplayErrorObjectId<'_>> {
        self.start_message(object_id, 1)
            .context("Failed to start message")?;
        Ok(WlDisplayErrorObjectId { writer: self })
    }
}

pub struct WlDisplayErrorObjectId<'client> {
    writer: &'client mut Writer,
}

impl<'client> WlDisplayErrorObjectId<'client> {
    pub fn object_id(self, object_id: ObjectId) -> WlDisplayErrorCode<'client> {
        self.writer.write_u32(object_id);
        WlDisplayErrorCode {
            writer: self.writer,
        }
    }
}

pub struct WlDisplayErrorCode<'client> {
    writer: &'client mut Writer,
}

impl<'client> WlDisplayErrorCode<'client> {
    pub fn code(self, code: u32) -> WlDisplayErrorMessage<'client> {
        self.writer.write_u32(code);
        WlDisplayErrorMessage {
            writer: self.writer,
        }
    }
}

pub struct WlDisplayErrorMessage<'client> {
    writer: &'client mut Writer,
}

impl<'client> WlDisplayErrorMessage<'client> {
    pub fn message(self, message: &str) {
        self.writer.write_str(message);
        self.writer.write_message_length();
    }
}

#[derive(Debug)]
pub struct Sync {
    pub callback: ObjectId,
}

pub const WL_DISPLAY_ERROR_INVALID_OBJECT: u32 = 0;
pub const WL_DISPLAY_ERROR_INVALID_METHOD: u32 = 1;
