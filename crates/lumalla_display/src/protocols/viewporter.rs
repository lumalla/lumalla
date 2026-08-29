use log::debug;
use lumalla_wayland_protocol::{
    Ctx, NewObjectId, ObjectId,
    buffer::fixed_to_f32,
    protocols::{
        ViewporterProtocol,
        viewporter::*,
        wayland::WL_DISPLAY_ERROR_INVALID_OBJECT,
    },
    registry::{DISPLAY_OBJECT_ID, InterfaceIndex},
};

use crate::{DisplayState, surface::SurfaceError};

impl ViewporterProtocol for DisplayState {}

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

fn report_viewporter_error(ctx: &mut Ctx, object_id: ObjectId, error: SurfaceError) {
    let (code, message) = match error {
        SurfaceError::ViewportExists => (
            WP_VIEWPORTER_ERROR_VIEWPORT_EXISTS,
            "Surface already has a viewport",
        ),
        SurfaceError::UnknownSurface => (WL_DISPLAY_ERROR_INVALID_OBJECT, "Unknown surface"),
        other => {
            debug!("Unexpected viewporter error: {other:?}");
            (WL_DISPLAY_ERROR_INVALID_OBJECT, "Viewporter error")
        }
    };
    ctx.writer
        .wl_display_error(DISPLAY_OBJECT_ID)
        .object_id(object_id)
        .code(code)
        .message(message);
}

fn report_viewport_error(ctx: &mut Ctx, object_id: ObjectId, error: SurfaceError) {
    let (code, message) = match error {
        SurfaceError::NoSurface => (WP_VIEWPORT_ERROR_NO_SURFACE, "wl_surface was destroyed"),
        SurfaceError::ViewportBadValue => (
            WP_VIEWPORT_ERROR_BAD_VALUE,
            "Negative or zero values in width or height",
        ),
        other => {
            debug!("Unexpected viewport error: {other:?}");
            (WL_DISPLAY_ERROR_INVALID_OBJECT, "Viewport error")
        }
    };
    ctx.writer
        .wl_display_error(DISPLAY_OBJECT_ID)
        .object_id(object_id)
        .code(code)
        .message(message);
}

pub(crate) fn report_viewport_commit_error(
    ctx: &mut Ctx,
    viewport_id: ObjectId,
    error: crate::surface::ViewportCommitError,
) {
    let (code, message) = match error {
        crate::surface::ViewportCommitError::BadSize => (
            WP_VIEWPORT_ERROR_BAD_SIZE,
            "Source size must be integer when destination is unset",
        ),
        crate::surface::ViewportCommitError::OutOfBuffer => (
            WP_VIEWPORT_ERROR_OUT_OF_BUFFER,
            "Source rectangle extends outside of the content area",
        ),
    };
    ctx.writer
        .wl_display_error(DISPLAY_OBJECT_ID)
        .object_id(viewport_id)
        .code(code)
        .message(message);
}

impl WpViewporter for DisplayState {
    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WpViewporterDestroy<'_>) {
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn get_viewport(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WpViewporterGetViewport<'_>,
    ) {
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map(|m| m.version)
            .unwrap_or(1);
        let surface = params.surface();
        if ctx.registry.interface_index(surface) != Some(InterfaceIndex::WlSurface) {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("get_viewport surface is not a wl_surface");
            return;
        }
        if !register_object(
            ctx,
            params.id(),
            InterfaceIndex::WpViewport,
            version,
        ) {
            return;
        }
        if let Err(error) =
            self.surface_manager
                .create_viewport(ctx.client_id, surface, *params.id())
        {
            report_viewporter_error(ctx, object_id, error);
            ctx.registry.free_object(*params.id(), ctx.writer);
        }
    }
}

impl WpViewport for DisplayState {
    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WpViewportDestroy<'_>) {
        let _ = self
            .surface_manager
            .destroy_viewport(ctx.client_id, object_id);
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn set_source(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WpViewportSetSource<'_>,
    ) {
        if let Err(error) = self.surface_manager.set_viewport_source(
            ctx.client_id,
            object_id,
            fixed_to_f32(params.x()),
            fixed_to_f32(params.y()),
            fixed_to_f32(params.width()),
            fixed_to_f32(params.height()),
        ) {
            report_viewport_error(ctx, object_id, error);
        }
    }

    fn set_destination(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WpViewportSetDestination<'_>,
    ) {
        if let Err(error) = self.surface_manager.set_viewport_destination(
            ctx.client_id,
            object_id,
            params.width(),
            params.height(),
        ) {
            report_viewport_error(ctx, object_id, error);
        }
    }
}
