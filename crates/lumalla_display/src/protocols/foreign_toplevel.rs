use lumalla_wayland_protocol::{
    Ctx, ObjectId,
    protocols::{ExtForeignToplevelListV1Protocol, foreign_toplevel_list::*},
};

use crate::DisplayState;

impl ExtForeignToplevelListV1Protocol for DisplayState {}

impl ExtForeignToplevelListV1 for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ExtForeignToplevelListV1Destroy<'_>,
    ) {
        self.foreign_toplevel_manager
            .destroy_list(ctx.client_id, object_id);
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn stop(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ExtForeignToplevelListV1Stop<'_>,
    ) {
        self.foreign_toplevel_manager
            .stop(ctx.client_id, object_id, ctx.writer);
    }
}

impl ExtForeignToplevelHandleV1 for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ExtForeignToplevelHandleV1Destroy<'_>,
    ) {
        self.foreign_toplevel_manager
            .destroy_handle(ctx.client_id, object_id);
        ctx.registry.free_object(object_id, ctx.writer);
    }
}
