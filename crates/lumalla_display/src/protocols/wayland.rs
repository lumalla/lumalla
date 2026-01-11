use log::debug;
use lumalla_wayland_protocol::{
    Ctx, ObjectId,
    protocols::{WaylandProtocol, WlDisplay, wayland::*},
    registry::{DISPLAY_OBJECT_ID, InterfaceIndex},
};

use crate::{DisplayState, GlobalId};

impl WaylandProtocol for DisplayState {}

impl WlDisplay for DisplayState {
    fn sync(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlDisplaySync<'_>) {
        ctx.writer
            .wl_callback_done(*params.callback())
            .callback_data(0);
        ctx.writer
            .wl_display_delete_id(object_id)
            .id((*params.callback()).get());
    }

    fn get_registry(
        &mut self,
        ctx: &mut Ctx,
        _object_id: ObjectId,
        params: &WlDisplayGetRegistry<'_>,
    ) {
        ctx.registry
            .register_object(params.registry(), InterfaceIndex::WlRegistry);
        for (&name, global) in self.globals.iter() {
            ctx.writer
                .wl_registry_global(*params.registry())
                .name(name)
                .interface(global.interface_index.interface_name())
                .version(global.interface_index.interface_version());
        }
    }
}

impl WlRegistry for DisplayState {
    fn bind(&mut self, ctx: &mut Ctx, _object_id: ObjectId, params: &WlRegistryBind<'_>) {
        let global_id: GlobalId = params.name();
        let Some(global) = self.globals.get(global_id) else {
            debug!("Received bind request for unknown global {}", global_id);
            return;
        };
        // TODO: Do we need to care what version the global is bound to?
        ctx.registry
            .register_object(params.id().0, global.interface_index);

        let interface_name = params.id().1;
        match params.id().1 {
            _ if interface_name == InterfaceIndex::WlShm.interface_name() => {
                // TODO: Get the available formats from the GPU
                ctx.writer
                    .wl_shm_format(*params.id().0)
                    .format(WL_SHM_FORMAT_RGBA8888);
                ctx.writer
                    .wl_shm_format(*params.id().0)
                    .format(WL_SHM_FORMAT_XRGB8888);
            }
            _ if interface_name == InterfaceIndex::WlSeat.interface_name() => {
                ctx.writer
                    .wl_seat_name(*params.id().0)
                    .name(self.seat_manager.get_name(global_id).unwrap_or_default());
                ctx.writer
                    .wl_seat_capabilities(*params.id().0)
                    .capabilities(WL_SEAT_CAPABILITY_POINTER | WL_SEAT_CAPABILITY_KEYBOARD);
            }
            _ => {}
        }
    }
}

impl WlCompositor for DisplayState {
    fn create_surface(
        &mut self,
        ctx: &mut Ctx,
        _object_id: ObjectId,
        params: &WlCompositorCreateSurface<'_>,
    ) {
        ctx.registry
            .register_object(params.id(), InterfaceIndex::WlSurface);
        self.surface_manager
            .create_surface(ctx.client_id, *params.id());
    }

    fn create_region(
        &mut self,
        ctx: &mut Ctx,
        _object_id: ObjectId,
        params: &WlCompositorCreateRegion<'_>,
    ) {
        ctx.registry
            .register_object(params.id(), InterfaceIndex::WlRegion);
    }
}

impl WlShm for DisplayState {
    fn create_pool(&mut self, ctx: &mut Ctx, _object_id: ObjectId, params: &WlShmCreatePool) {
        ctx.registry
            .register_object(params.id(), InterfaceIndex::WlShmPool);
        if self.shm_manager.create_pool(
            ctx.client_id,
            *params.id(),
            params.fd(),
            params.size() as usize,
        ) {
            debug!(
                "Failed to mmap shared memory from client {:?}",
                ctx.client_id
            );
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(*params.id())
                .code(WL_SHM_ERROR_INVALID_FD)
                .message("Failed to mmap shared memory");
        }
    }

    fn release(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlShmRelease) {
        ctx.registry.free_object(object_id, &mut ctx.writer);
    }
}

impl WlShmPool for DisplayState {
    fn create_buffer(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlShmPoolCreateBuffer<'_>,
    ) {
        ctx.registry
            .register_object(params.id(), InterfaceIndex::WlBuffer);
        if self.shm_manager.create_buffer(
            ctx.client_id,
            object_id,
            *params.id(),
            params.offset() as usize,
            params.width() as usize,
            params.height() as usize,
            params.stride() as usize,
            params.format(),
        ) {
            debug!(
                "Failed to create shm_buffer from client {:?}",
                ctx.client_id
            );
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_SHM_ERROR_INVALID_FORMAT)
                .message("Failed to create shm_buffer");
        }
    }

    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlShmPoolDestroy<'_>) {
        ctx.registry.free_object(object_id, &mut ctx.writer);
        self.shm_manager.delete_pool(ctx.client_id, object_id);
    }

    fn resize(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlShmPoolResize<'_>) {
        if !self
            .shm_manager
            .resize_pool(ctx.client_id, object_id, params.size() as usize)
        {
            debug!(
                "Failed to resize shm_pool to {} from client {:?}",
                params.size(),
                ctx.client_id
            );
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_SHM_ERROR_INVALID_FD)
                .message("Failed to resize shm_pool");
        }
    }
}

impl WlBuffer for DisplayState {
    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlBufferDestroy<'_>) {
        ctx.registry.free_object(object_id, &mut ctx.writer);
        self.shm_manager.delete_buffer(ctx.client_id, object_id);
        ctx.writer.wl_buffer_release(object_id);
    }
}

impl WlDataOffer for DisplayState {
    fn accept(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlDataOfferAccept<'_>) {
        todo!()
    }

    fn receive(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlDataOfferReceive<'_>) {
        todo!()
    }

    fn destroy(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlDataOfferDestroy<'_>) {
        todo!()
    }

    fn finish(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlDataOfferFinish<'_>) {
        todo!()
    }

    fn set_actions(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlDataOfferSetActions<'_>,
    ) {
        todo!()
    }
}

impl WlDataSource for DisplayState {
    fn offer(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlDataSourceOffer<'_>) {
        todo!()
    }

    fn destroy(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlDataSourceDestroy<'_>) {
        todo!()
    }

    fn set_actions(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlDataSourceSetActions<'_>,
    ) {
        todo!()
    }
}

impl WlDataDevice for DisplayState {
    fn start_drag(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlDataDeviceStartDrag<'_>,
    ) {
        todo!()
    }

    fn set_selection(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlDataDeviceSetSelection<'_>,
    ) {
        todo!()
    }

    fn release(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlDataDeviceRelease<'_>) {
        todo!()
    }
}

impl WlDataDeviceManager for DisplayState {
    fn create_data_source(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlDataDeviceManagerCreateDataSource<'_>,
    ) {
        todo!()
    }

    fn get_data_device(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlDataDeviceManagerGetDataDevice<'_>,
    ) {
        todo!()
    }
}

impl WlShell for DisplayState {
    fn get_shell_surface(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlShellGetShellSurface<'_>,
    ) {
        todo!()
    }
}

impl WlShellSurface for DisplayState {
    fn pong(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlShellSurfacePong<'_>) {
        todo!()
    }

    fn move_(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlShellSurfaceMove<'_>) {
        todo!()
    }

    fn resize(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlShellSurfaceResize<'_>) {
        todo!()
    }

    fn set_toplevel(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlShellSurfaceSetToplevel<'_>,
    ) {
        todo!()
    }

    fn set_transient(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlShellSurfaceSetTransient<'_>,
    ) {
        todo!()
    }

    fn set_fullscreen(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlShellSurfaceSetFullscreen<'_>,
    ) {
        todo!()
    }

    fn set_popup(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlShellSurfaceSetPopup<'_>,
    ) {
        todo!()
    }

    fn set_maximized(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlShellSurfaceSetMaximized<'_>,
    ) {
        todo!()
    }

    fn set_title(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlShellSurfaceSetTitle<'_>,
    ) {
        todo!()
    }

    fn set_class(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlShellSurfaceSetClass<'_>,
    ) {
        todo!()
    }
}

impl WlSurface for DisplayState {
    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlSurfaceDestroy<'_>) {
        ctx.registry.free_object(object_id, &mut ctx.writer);
    }

    fn attach(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlSurfaceAttach<'_>) {
        let Some(pending_buffer) = params.buffer() else {
            if !self.surface_manager.set_pending_buffer(
                ctx.client_id,
                object_id,
                None,
                params.x(),
                params.y(),
            ) {
                ctx.writer
                    .wl_display_error(DISPLAY_OBJECT_ID)
                    .object_id(object_id)
                    .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                    .message("Invalid surface");
            }
            return;
        };
        if ctx.registry.interface_index(pending_buffer) != Some(InterfaceIndex::WlBuffer) {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(pending_buffer)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("Invalid buffer");
            return;
        }
        if !self.surface_manager.set_pending_buffer(
            ctx.client_id,
            object_id,
            Some(pending_buffer),
            params.x(),
            params.y(),
        ) {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("Invalid surface");
        }
    }

    fn damage(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlSurfaceDamage<'_>) {
        // TODO: Implement damage tracking
    }

    fn frame(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlSurfaceFrame<'_>) {
        todo!()
    }

    fn set_opaque_region(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlSurfaceSetOpaqueRegion<'_>,
    ) {
        todo!()
    }

    fn set_input_region(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlSurfaceSetInputRegion<'_>,
    ) {
        todo!()
    }

    fn commit(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlSurfaceCommit<'_>) {
        todo!()
    }

    fn set_buffer_transform(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlSurfaceSetBufferTransform<'_>,
    ) {
        todo!()
    }

    fn set_buffer_scale(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlSurfaceSetBufferScale<'_>,
    ) {
        todo!()
    }

    fn damage_buffer(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlSurfaceDamageBuffer<'_>,
    ) {
        todo!()
    }

    fn offset(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlSurfaceOffset) {
        todo!()
    }
}

impl WlSeat for DisplayState {
    fn get_pointer(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlSeatGetPointer<'_>,
    ) {
        todo!()
    }

    fn get_keyboard(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlSeatGetKeyboard<'_>,
    ) {
        todo!()
    }

    fn get_touch(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlSeatGetTouch<'_>) {
        todo!()
    }

    fn release(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlSeatRelease<'_>) {
        todo!()
    }
}

impl WlPointer for DisplayState {
    fn set_cursor(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlPointerSetCursor<'_>,
    ) {
        todo!()
    }

    fn release(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlPointerRelease<'_>) {
        todo!()
    }
}

impl WlKeyboard for DisplayState {
    fn release(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlKeyboardRelease<'_>) {
        todo!()
    }
}

impl WlTouch for DisplayState {
    fn release(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlTouchRelease<'_>) {
        todo!()
    }
}

impl WlOutput for DisplayState {
    fn release(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlOutputRelease<'_>) {
        todo!()
    }
}

impl WlRegion for DisplayState {
    fn destroy(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlRegionDestroy<'_>) {
        todo!()
    }

    fn add(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlRegionAdd<'_>) {
        todo!()
    }

    fn subtract(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlRegionSubtract<'_>) {
        todo!()
    }
}

impl WlSubcompositor for DisplayState {
    fn destroy(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlSubcompositorDestroy<'_>,
    ) {
        todo!()
    }

    fn get_subsurface(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlSubcompositorGetSubsurface<'_>,
    ) {
        todo!()
    }
}

impl WlSubsurface for DisplayState {
    fn destroy(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlSubsurfaceDestroy<'_>) {
        todo!()
    }

    fn set_position(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlSubsurfaceSetPosition<'_>,
    ) {
        todo!()
    }

    fn place_above(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlSubsurfacePlaceAbove<'_>,
    ) {
        todo!()
    }

    fn place_below(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlSubsurfacePlaceBelow<'_>,
    ) {
        todo!()
    }

    fn set_sync(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlSubsurfaceSetSync<'_>,
    ) {
        todo!()
    }

    fn set_desync(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlSubsurfaceSetDesync<'_>,
    ) {
        todo!()
    }
}

impl WlFixes for DisplayState {
    fn destroy(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlFixesDestroy) {
        todo!()
    }

    fn destroy_registry(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &WlFixesDestroyRegistry,
    ) {
        todo!()
    }
}
