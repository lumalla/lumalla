use lumalla_wayland_protocol::{
    Ctx, ObjectId,
    protocols::{WaylandProtocol, WlDisplay, wayland::*},
};

use crate::DisplayState;

// Implement the protocol supertrait
impl WaylandProtocol for DisplayState {}

impl WlDisplay for DisplayState {
    fn sync(&mut self, ctx: &Ctx, object_id: ObjectId, params: &WlDisplaySync<'_>) {
        todo!()
    }

    fn get_registry(&mut self, ctx: &Ctx, object_id: ObjectId, params: &WlDisplayGetRegistry<'_>) {
        todo!()
    }
}

impl WlRegistry for DisplayState {
    fn bind(&mut self, ctx: &Ctx, object_id: ObjectId, params: &WlRegistryBind<'_>) {
        todo!()
    }
}

impl WlCompositor for DisplayState {
    fn create_surface(
        &mut self,
        ctx: &Ctx,
        object_id: ObjectId,
        params: &WlCompositorCreateSurface<'_>,
    ) {
        todo!()
    }

    fn create_region(
        &mut self,
        ctx: &Ctx,
        object_id: ObjectId,
        params: &WlCompositorCreateRegion<'_>,
    ) {
        todo!()
    }
}

// Add implementations for other common Wayland interfaces
impl WlShm for DisplayState {
    fn create_pool(&mut self, ctx: &Ctx, object_id: ObjectId, params: &WlShmCreatePool) {
        todo!()
    }

    fn release(&mut self, ctx: &Ctx, object_id: ObjectId, params: &WlShmRelease) {
        todo!()
    }
}

impl WlShmPool for DisplayState {
    fn create_buffer(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlShmPoolCreateBuffer<'_>,
    ) {
        todo!()
    }

    fn destroy(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlShmPoolDestroy<'_>) {
        todo!()
    }

    fn resize(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlShmPoolResize<'_>) {
        todo!()
    }
}

impl WlBuffer for DisplayState {
    fn destroy(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlBufferDestroy<'_>) {
        todo!()
    }
}

impl WlDataOffer for DisplayState {
    fn accept(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlDataOfferAccept<'_>) {
        todo!()
    }

    fn receive(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlDataOfferReceive<'_>) {
        todo!()
    }

    fn destroy(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlDataOfferDestroy<'_>) {
        todo!()
    }

    fn finish(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlDataOfferFinish<'_>) {
        todo!()
    }

    fn set_actions(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlDataOfferSetActions<'_>,
    ) {
        todo!()
    }
}

impl WlDataSource for DisplayState {
    fn offer(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlDataSourceOffer<'_>) {
        todo!()
    }

    fn destroy(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlDataSourceDestroy<'_>) {
        todo!()
    }

    fn set_actions(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlDataSourceSetActions<'_>,
    ) {
        todo!()
    }
}

impl WlDataDevice for DisplayState {
    fn start_drag(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlDataDeviceStartDrag<'_>,
    ) {
        todo!()
    }

    fn set_selection(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlDataDeviceSetSelection<'_>,
    ) {
        todo!()
    }

    fn release(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlDataDeviceRelease<'_>) {
        todo!()
    }
}

impl WlDataDeviceManager for DisplayState {
    fn create_data_source(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlDataDeviceManagerCreateDataSource<'_>,
    ) {
        todo!()
    }

    fn get_data_device(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlDataDeviceManagerGetDataDevice<'_>,
    ) {
        todo!()
    }
}

impl WlShell for DisplayState {
    fn get_shell_surface(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlShellGetShellSurface<'_>,
    ) {
        todo!()
    }
}

impl WlShellSurface for DisplayState {
    fn pong(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlShellSurfacePong<'_>) {
        todo!()
    }

    fn move_(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlShellSurfaceMove<'_>) {
        todo!()
    }

    fn resize(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlShellSurfaceResize<'_>) {
        todo!()
    }

    fn set_toplevel(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlShellSurfaceSetToplevel<'_>,
    ) {
        todo!()
    }

    fn set_transient(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlShellSurfaceSetTransient<'_>,
    ) {
        todo!()
    }

    fn set_fullscreen(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlShellSurfaceSetFullscreen<'_>,
    ) {
        todo!()
    }

    fn set_popup(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlShellSurfaceSetPopup<'_>,
    ) {
        todo!()
    }

    fn set_maximized(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlShellSurfaceSetMaximized<'_>,
    ) {
        todo!()
    }

    fn set_title(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlShellSurfaceSetTitle<'_>,
    ) {
        todo!()
    }

    fn set_class(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlShellSurfaceSetClass<'_>,
    ) {
        todo!()
    }
}

impl WlSurface for DisplayState {
    fn destroy(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlSurfaceDestroy<'_>) {
        todo!()
    }

    fn attach(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlSurfaceAttach<'_>) {
        todo!()
    }

    fn damage(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlSurfaceDamage<'_>) {
        todo!()
    }

    fn frame(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlSurfaceFrame<'_>) {
        todo!()
    }

    fn set_opaque_region(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlSurfaceSetOpaqueRegion<'_>,
    ) {
        todo!()
    }

    fn set_input_region(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlSurfaceSetInputRegion<'_>,
    ) {
        todo!()
    }

    fn commit(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlSurfaceCommit<'_>) {
        todo!()
    }

    fn set_buffer_transform(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlSurfaceSetBufferTransform<'_>,
    ) {
        todo!()
    }

    fn set_buffer_scale(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlSurfaceSetBufferScale<'_>,
    ) {
        todo!()
    }

    fn damage_buffer(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlSurfaceDamageBuffer<'_>,
    ) {
        todo!()
    }

    fn offset(&mut self, ctx: &Ctx, object_id: ObjectId, params: &WlSurfaceOffset) {
        todo!()
    }
}

impl WlSeat for DisplayState {
    fn get_pointer(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlSeatGetPointer<'_>) {
        todo!()
    }

    fn get_keyboard(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlSeatGetKeyboard<'_>) {
        todo!()
    }

    fn get_touch(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlSeatGetTouch<'_>) {
        todo!()
    }

    fn release(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlSeatRelease<'_>) {
        todo!()
    }
}

impl WlPointer for DisplayState {
    fn set_cursor(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlPointerSetCursor<'_>) {
        todo!()
    }

    fn release(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlPointerRelease<'_>) {
        todo!()
    }
}

impl WlKeyboard for DisplayState {
    fn release(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlKeyboardRelease<'_>) {
        todo!()
    }
}

impl WlTouch for DisplayState {
    fn release(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlTouchRelease<'_>) {
        todo!()
    }
}

impl WlOutput for DisplayState {
    fn release(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlOutputRelease<'_>) {
        todo!()
    }
}

impl WlRegion for DisplayState {
    fn destroy(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlRegionDestroy<'_>) {
        todo!()
    }

    fn add(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlRegionAdd<'_>) {
        todo!()
    }

    fn subtract(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlRegionSubtract<'_>) {
        todo!()
    }
}

impl WlSubcompositor for DisplayState {
    fn destroy(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlSubcompositorDestroy<'_>) {
        todo!()
    }

    fn get_subsurface(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlSubcompositorGetSubsurface<'_>,
    ) {
        todo!()
    }
}

impl WlSubsurface for DisplayState {
    fn destroy(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlSubsurfaceDestroy<'_>) {
        todo!()
    }

    fn set_position(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlSubsurfaceSetPosition<'_>,
    ) {
        todo!()
    }

    fn place_above(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlSubsurfacePlaceAbove<'_>,
    ) {
        todo!()
    }

    fn place_below(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlSubsurfacePlaceBelow<'_>,
    ) {
        todo!()
    }

    fn set_sync(&mut self, _ctx: &Ctx, _object_id: ObjectId, _params: &WlSubsurfaceSetSync<'_>) {
        todo!()
    }

    fn set_desync(
        &mut self,
        _ctx: &Ctx,
        _object_id: ObjectId,
        _params: &WlSubsurfaceSetDesync<'_>,
    ) {
        todo!()
    }
}

impl WlFixes for DisplayState {
    fn destroy(&mut self, ctx: &Ctx, object_id: ObjectId, params: &WlFixesDestroy) {
        todo!()
    }

    fn destroy_registry(
        &mut self,
        ctx: &Ctx,
        object_id: ObjectId,
        params: &WlFixesDestroyRegistry,
    ) {
        todo!()
    }
}
