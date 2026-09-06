use log::debug;
use lumalla_wayland_protocol::{
    Ctx, NewObjectId, ObjectId,
    protocols::{
        RelativePointerUnstableV1Protocol,
        relative_pointer::*,
        wayland::WL_DISPLAY_ERROR_INVALID_OBJECT,
    },
    registry::{DISPLAY_OBJECT_ID, InterfaceIndex},
};

use crate::DisplayState;

impl RelativePointerUnstableV1Protocol for DisplayState {}

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

impl ZwpRelativePointerManagerV1 for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ZwpRelativePointerManagerV1Destroy<'_>,
    ) {
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn get_relative_pointer(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwpRelativePointerManagerV1GetRelativePointer<'_>,
    ) {
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map(|m| m.version)
            .unwrap_or(1);
        let pointer = params.pointer();
        if ctx.registry.interface_index(pointer) != Some(InterfaceIndex::WlPointer) {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("pointer is not a wl_pointer");
            return;
        }
        if !register_object(
            ctx,
            params.id(),
            InterfaceIndex::ZwpRelativePointerV1,
            version,
        ) {
            return;
        }
        self.relative_pointer_manager
            .create(ctx.client_id, *params.id(), pointer);
    }
}

impl ZwpRelativePointerV1 for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ZwpRelativePointerV1Destroy<'_>,
    ) {
        self.relative_pointer_manager
            .destroy(ctx.client_id, object_id);
        ctx.registry.free_object(object_id, ctx.writer);
    }
}
