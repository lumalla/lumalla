use log::debug;
use lumalla_wayland_protocol::{
    Ctx, NewObjectId, ObjectId,
    protocols::{
        XdgOutputUnstableV1Protocol,
        wayland::WL_DISPLAY_ERROR_INVALID_OBJECT,
        xdg_output::*,
    },
    registry::{DISPLAY_OBJECT_ID, InterfaceIndex},
};

use crate::DisplayState;

impl XdgOutputUnstableV1Protocol for DisplayState {}

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

impl ZxdgOutputManagerV1 for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ZxdgOutputManagerV1Destroy<'_>,
    ) {
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn get_xdg_output(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZxdgOutputManagerV1GetXdgOutput<'_>,
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

        let version = ctx
            .registry
            .object_metadata(object_id)
            .map(|m| m.version)
            .unwrap_or(1);
        if !register_object(
            ctx,
            params.id(),
            InterfaceIndex::ZxdgOutputV1,
            version,
        ) {
            return;
        }

        if !self.output_manager.bind_xdg_output(
            ctx.client_id,
            *params.id(),
            output,
            version,
            ctx.writer,
        ) {
            debug!(
                "get_xdg_output: unknown or unbound wl_output {} for client {}",
                output.get(),
                ctx.client_id.get()
            );
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("unknown wl_output");
            ctx.registry.free_object(*params.id(), ctx.writer);
        }
    }
}

impl ZxdgOutputV1 for DisplayState {
    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &ZxdgOutputV1Destroy<'_>) {
        self.output_manager
            .destroy_xdg_output(ctx.client_id, object_id);
        ctx.registry.free_object(object_id, ctx.writer);
    }
}
