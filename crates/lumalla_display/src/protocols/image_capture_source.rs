use log::debug;
use lumalla_wayland_protocol::{
    Ctx, NewObjectId, ObjectId,
    protocols::{
        ExtImageCaptureSourceV1Protocol,
        image_capture_source::*,
        wayland::WL_DISPLAY_ERROR_INVALID_OBJECT,
    },
    registry::{DISPLAY_OBJECT_ID, InterfaceIndex},
};

use crate::{
    DisplayState,
    image_copy_capture::CaptureSourceKind,
};

impl ExtImageCaptureSourceV1Protocol for DisplayState {}

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

impl ExtImageCaptureSourceV1 for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ExtImageCaptureSourceV1Destroy<'_>,
    ) {
        self.image_copy_capture
            .destroy_source(ctx.client_id, object_id);
        ctx.registry.free_object(object_id, ctx.writer);
    }
}

impl ExtOutputImageCaptureSourceManagerV1 for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ExtOutputImageCaptureSourceManagerV1Destroy<'_>,
    ) {
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn create_source(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ExtOutputImageCaptureSourceManagerV1CreateSource<'_>,
    ) {
        let output = params.output();
        if ctx.registry.interface_index(output) != Some(InterfaceIndex::WlOutput) {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("output is not a wl_output");
            return;
        }
        if self
            .output_manager
            .global_for_binding(ctx.client_id, output)
            .is_none()
        {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("wl_output is not bound");
            return;
        }
        if !register_object(
            ctx,
            params.source(),
            InterfaceIndex::ExtImageCaptureSourceV1,
            1,
        ) {
            return;
        }
        self.image_copy_capture.create_source(
            ctx.client_id,
            *params.source(),
            CaptureSourceKind::Output {
                wl_output: output,
            },
        );
    }
}

impl ExtForeignToplevelImageCaptureSourceManagerV1 for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ExtForeignToplevelImageCaptureSourceManagerV1Destroy<'_>,
    ) {
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn create_source(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ExtForeignToplevelImageCaptureSourceManagerV1CreateSource<'_>,
    ) {
        let handle = params.toplevel_handle();
        if ctx.registry.interface_index(handle)
            != Some(InterfaceIndex::ExtForeignToplevelHandleV1)
        {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("toplevel_handle is not an ext_foreign_toplevel_handle_v1");
            return;
        }
        let Some(window_id) = self
            .foreign_toplevel_manager
            .resolve_handle(ctx.client_id, handle)
        else {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("unknown foreign toplevel handle");
            return;
        };
        if !register_object(
            ctx,
            params.source(),
            InterfaceIndex::ExtImageCaptureSourceV1,
            1,
        ) {
            return;
        }
        self.image_copy_capture.create_source(
            ctx.client_id,
            *params.source(),
            CaptureSourceKind::Toplevel { window_id },
        );
    }
}
