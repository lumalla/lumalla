use log::debug;
use lumalla_wayland_protocol::{
    Ctx, NewObjectId, ObjectId,
    protocols::{
        ExtImageCopyCaptureV1Protocol,
        image_copy_capture::*,
        wayland::WL_DISPLAY_ERROR_INVALID_OBJECT,
    },
    registry::{DISPLAY_OBJECT_ID, InterfaceIndex},
};

use crate::{
    DisplayState,
    image_copy_capture::{CaptureBufferSize, CaptureDamageRect, CaptureSourceKind},
};

impl ExtImageCopyCaptureV1Protocol for DisplayState {}

fn register_object(
    ctx: &mut Ctx,
    id: NewObjectId,
    interface: InterfaceIndex,
    version: u32,
) -> bool {
    if let Err(err) = ctx
        .registry
        .register_client_object_with_version(id, interface, version)
    {
        debug!("Failed to register {}: {err}", interface.interface_name());
        ctx.writer
            .wl_display_error(DISPLAY_OBJECT_ID)
            .object_id(*id)
            .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
            .message("Invalid or duplicate object ID");
        return false;
    }
    true
}

fn protocol_error(ctx: &mut Ctx, object_id: ObjectId, code: u32, message: &str) {
    ctx.writer
        .wl_display_error(DISPLAY_OBJECT_ID)
        .object_id(object_id)
        .code(code)
        .message(message);
}

impl ExtImageCopyCaptureManagerV1 for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ExtImageCopyCaptureManagerV1Destroy<'_>,
    ) {
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn create_session(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ExtImageCopyCaptureManagerV1CreateSession<'_>,
    ) {
        let options = params.options();
        let known = EXT_IMAGE_COPY_CAPTURE_MANAGER_V1_OPTIONS_PAINT_CURSORS;
        if options & !known != 0 {
            protocol_error(
                ctx,
                object_id,
                EXT_IMAGE_COPY_CAPTURE_MANAGER_V1_ERROR_INVALID_OPTION,
                "invalid option flag",
            );
            return;
        }
        let source = params.source();
        if ctx.registry.interface_index(source) != Some(InterfaceIndex::ExtImageCaptureSourceV1)
        {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("source is not an ext_image_capture_source_v1");
            return;
        }
        let Some(kind) = self.image_copy_capture.source_kind(ctx.client_id, source) else {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("unknown capture source");
            return;
        };
        let Some(size) = self.capture_source_size(ctx.client_id, kind) else {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("capture source is not available");
            return;
        };
        if !register_object(
            ctx,
            params.session(),
            InterfaceIndex::ExtImageCopyCaptureSessionV1,
            1,
        ) {
            return;
        }
        let paint_cursors = options & EXT_IMAGE_COPY_CAPTURE_MANAGER_V1_OPTIONS_PAINT_CURSORS != 0;
        let _ = self.image_copy_capture.create_session(
            ctx.client_id,
            *params.session(),
            source,
            paint_cursors,
            size,
            ctx.writer,
        );
    }

    fn create_pointer_cursor_session(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ExtImageCopyCaptureManagerV1CreatePointerCursorSession<'_>,
    ) {
        let source = params.source();
        let pointer = params.pointer();
        if ctx.registry.interface_index(source) != Some(InterfaceIndex::ExtImageCaptureSourceV1)
        {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("source is not an ext_image_capture_source_v1");
            return;
        }
        if ctx.registry.interface_index(pointer) != Some(InterfaceIndex::WlPointer) {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("pointer is not a wl_pointer");
            return;
        }
        if self
            .image_copy_capture
            .source_kind(ctx.client_id, source)
            .is_none()
        {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("unknown capture source");
            return;
        }
        if !register_object(
            ctx,
            params.session(),
            InterfaceIndex::ExtImageCopyCaptureCursorSessionV1,
            1,
        ) {
            return;
        }
        let _ = self.image_copy_capture.create_cursor_session(
            ctx.client_id,
            *params.session(),
            source,
            pointer,
        );
    }
}

impl ExtImageCopyCaptureSessionV1 for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ExtImageCopyCaptureSessionV1Destroy<'_>,
    ) {
        self.image_copy_capture
            .destroy_session(ctx.client_id, object_id);
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn create_frame(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ExtImageCopyCaptureSessionV1CreateFrame<'_>,
    ) {
        match self
            .image_copy_capture
            .create_frame(ctx.client_id, object_id, *params.frame())
        {
            Ok(()) => {}
            Err(0) => {
                ctx.writer
                    .wl_display_error(DISPLAY_OBJECT_ID)
                    .object_id(object_id)
                    .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                    .message("unknown capture session");
                return;
            }
            Err(code) => {
                protocol_error(ctx, object_id, code, "duplicate frame");
                return;
            }
        }
        if !register_object(
            ctx,
            params.frame(),
            InterfaceIndex::ExtImageCopyCaptureFrameV1,
            1,
        ) {
            self.image_copy_capture
                .destroy_frame(ctx.client_id, *params.frame());
        }
    }
}

impl ExtImageCopyCaptureFrameV1 for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ExtImageCopyCaptureFrameV1Destroy<'_>,
    ) {
        self.image_copy_capture
            .destroy_frame(ctx.client_id, object_id);
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn attach_buffer(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ExtImageCopyCaptureFrameV1AttachBuffer<'_>,
    ) {
        let buffer = params.buffer();
        if ctx.registry.interface_index(buffer) != Some(InterfaceIndex::WlBuffer) {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("buffer is not a wl_buffer");
            return;
        }
        match self
            .image_copy_capture
            .attach_buffer(ctx.client_id, object_id, buffer)
        {
            Ok(()) => {}
            Err(0) => {
                ctx.writer
                    .wl_display_error(DISPLAY_OBJECT_ID)
                    .object_id(object_id)
                    .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                    .message("unknown capture frame");
            }
            Err(code) => protocol_error(ctx, object_id, code, "attach_buffer after capture"),
        }
    }

    fn damage_buffer(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ExtImageCopyCaptureFrameV1DamageBuffer<'_>,
    ) {
        let rect = CaptureDamageRect {
            x: params.x(),
            y: params.y(),
            width: params.width(),
            height: params.height(),
        };
        match self
            .image_copy_capture
            .damage_buffer(ctx.client_id, object_id, rect)
        {
            Ok(()) => {}
            Err(0) => {
                ctx.writer
                    .wl_display_error(DISPLAY_OBJECT_ID)
                    .object_id(object_id)
                    .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                    .message("unknown capture frame");
            }
            Err(code) => protocol_error(ctx, object_id, code, "invalid damage_buffer"),
        }
    }

    fn capture(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ExtImageCopyCaptureFrameV1Capture<'_>,
    ) {
        match self.image_copy_capture.request_capture(ctx.client_id, object_id) {
            Ok(()) => {
                self.render_mut().request_present();
            }
            Err(0) => {
                ctx.writer
                    .wl_display_error(DISPLAY_OBJECT_ID)
                    .object_id(object_id)
                    .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                    .message("unknown capture frame");
            }
            Err(code) => protocol_error(ctx, object_id, code, "invalid capture"),
        }
    }
}

impl ExtImageCopyCaptureCursorSessionV1 for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ExtImageCopyCaptureCursorSessionV1Destroy<'_>,
    ) {
        self.image_copy_capture
            .destroy_cursor_session(ctx.client_id, object_id);
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn get_capture_session(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ExtImageCopyCaptureCursorSessionV1GetCaptureSession<'_>,
    ) {
        // Cursor image size: advertise a small default; updated when cursor changes.
        let size = CaptureBufferSize {
            width: 64,
            height: 64,
        };
        if !register_object(
            ctx,
            params.session(),
            InterfaceIndex::ExtImageCopyCaptureSessionV1,
            1,
        ) {
            return;
        }
        match self.image_copy_capture.get_cursor_capture_session(
            ctx.client_id,
            object_id,
            *params.session(),
            size,
            ctx.writer,
        ) {
            Ok(()) => {}
            Err(0) => {
                ctx.writer
                    .wl_display_error(DISPLAY_OBJECT_ID)
                    .object_id(object_id)
                    .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                    .message("unknown cursor session");
            }
            Err(code) => protocol_error(ctx, object_id, code, "duplicate capture session"),
        }
    }
}

impl DisplayState {
    pub(crate) fn capture_source_size(
        &self,
        client_id: lumalla_wayland_protocol::ClientId,
        kind: CaptureSourceKind,
    ) -> Option<CaptureBufferSize> {
        match kind {
            CaptureSourceKind::Output { wl_output } => {
                let (w, h) = self
                    .output_manager
                    .logical_size_for_client_output(client_id, Some(wl_output))?;
                if w <= 0 || h <= 0 {
                    return None;
                }
                Some(CaptureBufferSize {
                    width: w as u32,
                    height: h as u32,
                })
            }
            CaptureSourceKind::Toplevel { window_id } => {
                let (_, _, _, w, h, _) = self.window_capture_layers(Some(window_id))?;
                Some(CaptureBufferSize {
                    width: w as u32,
                    height: h as u32,
                })
            }
        }
    }
}
