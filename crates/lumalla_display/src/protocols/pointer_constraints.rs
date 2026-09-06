use log::debug;
use lumalla_wayland_protocol::{
    Ctx, NewObjectId, ObjectId,
    buffer::fixed_to_f32,
    protocols::{
        PointerConstraintsUnstableV1Protocol,
        pointer_constraints::*,
        wayland::{WL_DISPLAY_ERROR_INVALID_OBJECT, WL_DISPLAY_ERROR_INVALID_METHOD},
    },
    registry::{DISPLAY_OBJECT_ID, InterfaceIndex},
};

use crate::{
    DisplayState,
    pointer_constraints::{
        ConstraintError, ConstraintKind, ConstraintLifetime,
    },
};

impl PointerConstraintsUnstableV1Protocol for DisplayState {}

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

fn report_constraint_error(ctx: &mut Ctx, object_id: ObjectId, error: ConstraintError) {
    let (code, message) = match error {
        ConstraintError::AlreadyConstrained => (
            ZWP_POINTER_CONSTRAINTS_V1_ERROR_ALREADY_CONSTRAINED,
            "pointer constraint already requested on that surface",
        ),
        ConstraintError::UnknownRegion => (WL_DISPLAY_ERROR_INVALID_OBJECT, "Unknown region"),
        ConstraintError::InvalidLifetime => (WL_DISPLAY_ERROR_INVALID_METHOD, "Invalid lifetime"),
        ConstraintError::UnknownConstraint => {
            (WL_DISPLAY_ERROR_INVALID_OBJECT, "Unknown constraint")
        }
        ConstraintError::Defunct => (WL_DISPLAY_ERROR_INVALID_OBJECT, "Constraint is defunct"),
    };
    ctx.writer
        .wl_display_error(DISPLAY_OBJECT_ID)
        .object_id(object_id)
        .code(code)
        .message(message);
}

impl ZwpPointerConstraintsV1 for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ZwpPointerConstraintsV1Destroy<'_>,
    ) {
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn lock_pointer(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwpPointerConstraintsV1LockPointer<'_>,
    ) {
        create_constraint(
            self,
            ctx,
            object_id,
            ConstraintKind::Locked,
            params.id(),
            params.surface(),
            params.pointer(),
            params.region(),
            params.lifetime(),
        );
    }

    fn confine_pointer(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwpPointerConstraintsV1ConfinePointer<'_>,
    ) {
        create_constraint(
            self,
            ctx,
            object_id,
            ConstraintKind::Confined,
            params.id(),
            params.surface(),
            params.pointer(),
            params.region(),
            params.lifetime(),
        );
    }
}

fn create_constraint(
    state: &mut DisplayState,
    ctx: &mut Ctx,
    manager_id: ObjectId,
    kind: ConstraintKind,
    id: NewObjectId,
    surface: ObjectId,
    pointer: ObjectId,
    region: Option<ObjectId>,
    lifetime: u32,
) {
    let version = ctx
        .registry
        .object_metadata(manager_id)
        .map(|m| m.version)
        .unwrap_or(1);

    if ctx.registry.interface_index(surface) != Some(InterfaceIndex::WlSurface) {
        ctx.writer
            .wl_display_error(DISPLAY_OBJECT_ID)
            .object_id(manager_id)
            .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
            .message("surface is not a wl_surface");
        return;
    }
    if ctx.registry.interface_index(pointer) != Some(InterfaceIndex::WlPointer) {
        ctx.writer
            .wl_display_error(DISPLAY_OBJECT_ID)
            .object_id(manager_id)
            .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
            .message("pointer is not a wl_pointer");
        return;
    }
    if let Some(region_id) = region {
        if ctx.registry.interface_index(region_id) != Some(InterfaceIndex::WlRegion) {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(manager_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("region is not a wl_region");
            return;
        }
    }

    let Some(lifetime) = ConstraintLifetime::from_protocol(lifetime) else {
        report_constraint_error(ctx, manager_id, ConstraintError::InvalidLifetime);
        return;
    };

    let interface = match kind {
        ConstraintKind::Locked => InterfaceIndex::ZwpLockedPointerV1,
        ConstraintKind::Confined => InterfaceIndex::ZwpConfinedPointerV1,
    };
    if !register_object(ctx, id, interface, version) {
        return;
    }

    let region = match state
        .surface_manager
        .snapshot_region(ctx.client_id, region)
    {
        Ok(region) => region,
        Err(_) => {
            report_constraint_error(ctx, manager_id, ConstraintError::UnknownRegion);
            ctx.registry.free_object(*id, ctx.writer);
            return;
        }
    };

    if let Err(error) = state.pointer_constraints_manager.create_constraint(
        ctx.client_id,
        *id,
        kind,
        surface,
        pointer,
        region,
        lifetime,
    ) {
        report_constraint_error(ctx, manager_id, error);
        ctx.registry.free_object(*id, ctx.writer);
        return;
    }

    // Activate immediately if the pointer is already focused inside the region.
    let (px, py) = state.seat_manager.pointer_position();
    if state.seat_manager.pointer_focus(ctx.client_id, pointer) == Some(surface) {
        state.pointer_constraints_manager.activate_if_ready(
            ctx.client_id,
            surface,
            pointer,
            px,
            py,
            &state.surface_manager,
            ctx.writer,
        );
    }
}

impl ZwpLockedPointerV1 for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ZwpLockedPointerV1Destroy<'_>,
    ) {
        if let Some((surface, hint)) = self
            .pointer_constraints_manager
            .destroy(ctx.client_id, object_id)
        {
            if let Some((sx, sy)) = hint {
                if let Some((gx, gy)) = self.surface_manager.global_from_surface_local(
                    ctx.client_id,
                    surface,
                    sx,
                    sy,
                ) {
                    self.seat_manager.set_pointer_position(gx, gy);
                }
            }
        }
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn set_cursor_position_hint(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwpLockedPointerV1SetCursorPositionHint<'_>,
    ) {
        if let Err(error) = self.pointer_constraints_manager.set_pending_cursor_hint(
            ctx.client_id,
            object_id,
            fixed_to_f32(params.surface_x()) as f64,
            fixed_to_f32(params.surface_y()) as f64,
        ) {
            report_constraint_error(ctx, object_id, error);
        }
    }

    fn set_region(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwpLockedPointerV1SetRegion<'_>,
    ) {
        set_constraint_region(self, ctx, object_id, params.region());
    }
}

impl ZwpConfinedPointerV1 for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ZwpConfinedPointerV1Destroy<'_>,
    ) {
        let _ = self
            .pointer_constraints_manager
            .destroy(ctx.client_id, object_id);
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn set_region(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwpConfinedPointerV1SetRegion<'_>,
    ) {
        set_constraint_region(self, ctx, object_id, params.region());
    }
}

fn set_constraint_region(
    state: &mut DisplayState,
    ctx: &mut Ctx,
    object_id: ObjectId,
    region: Option<ObjectId>,
) {
    if let Some(region_id) = region {
        if ctx.registry.interface_index(region_id) != Some(InterfaceIndex::WlRegion) {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("region is not a wl_region");
            return;
        }
    }
    let region = match state
        .surface_manager
        .snapshot_region(ctx.client_id, region)
    {
        Ok(region) => region,
        Err(_) => {
            report_constraint_error(ctx, object_id, ConstraintError::UnknownRegion);
            return;
        }
    };
    if let Err(error) = state
        .pointer_constraints_manager
        .set_pending_region(ctx.client_id, object_id, region)
    {
        report_constraint_error(ctx, object_id, error);
    }
}
