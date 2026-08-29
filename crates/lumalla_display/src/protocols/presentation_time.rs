use log::debug;
use lumalla_wayland_protocol::{
    Ctx, NewObjectId, ObjectId,
    protocols::{
        PresentationTimeProtocol,
        presentation_time::*,
        wayland::WL_DISPLAY_ERROR_INVALID_OBJECT,
    },
    registry::{DISPLAY_OBJECT_ID, InterfaceIndex},
};

use crate::{DisplayState, surface::SurfaceError};

impl PresentationTimeProtocol for DisplayState {}

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

impl WpPresentation for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &WpPresentationDestroy<'_>,
    ) {
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn feedback(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WpPresentationFeedback<'_>,
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
                .message("feedback surface is not a wl_surface");
            return;
        }
        if !register_object(
            ctx,
            params.callback(),
            InterfaceIndex::WpPresentationFeedback,
            version,
        ) {
            return;
        }
        if let Err(error) = self.surface_manager.add_presentation_feedback(
            ctx.client_id,
            surface,
            *params.callback(),
        ) {
            let message = match error {
                SurfaceError::UnknownSurface => "Unknown surface",
                _ => "Invalid presentation feedback request",
            };
            debug!("presentation feedback error: {message}");
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message(message);
            ctx.registry.free_object(*params.callback(), ctx.writer);
        }
    }
}
