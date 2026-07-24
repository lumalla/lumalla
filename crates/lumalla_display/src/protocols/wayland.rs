use log::debug;
use lumalla_wayland_protocol::{
    Ctx, NewObjectId, ObjectId,
    protocols::{WaylandProtocol, WlDisplay, wayland::*},
    registry::{DISPLAY_OBJECT_ID, InterfaceIndex},
};

use crate::{
    CommittedFrame, DisplayState, GlobalId, SurfaceUpdate,
    shm::{ShmError, ShmErrorKind},
    surface::{Rectangle, ShellMode, SurfaceError},
};

impl WaylandProtocol for DisplayState {}

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

fn report_shm_error(ctx: &mut Ctx, object_id: ObjectId, error: &ShmError) {
    let code = match error.kind() {
        ShmErrorKind::InvalidFormat => WL_SHM_ERROR_INVALID_FORMAT,
        ShmErrorKind::InvalidStride => WL_SHM_ERROR_INVALID_STRIDE,
        ShmErrorKind::InvalidFd => WL_SHM_ERROR_INVALID_FD,
        ShmErrorKind::InvalidObject => WL_DISPLAY_ERROR_INVALID_OBJECT,
    };
    debug!("Shared-memory protocol error: {error}");
    ctx.writer
        .wl_display_error(DISPLAY_OBJECT_ID)
        .object_id(object_id)
        .code(code)
        .message(&error.to_string());
}

fn report_surface_error(ctx: &mut Ctx, object_id: ObjectId, error: SurfaceError) {
    let (code, message) = match error {
        SurfaceError::RoleAlreadyAssigned => (WL_SHELL_ERROR_ROLE, "Surface already has a role"),
        SurfaceError::UnknownSurface => (WL_DISPLAY_ERROR_INVALID_OBJECT, "Unknown surface"),
        SurfaceError::UnknownBuffer => (WL_DISPLAY_ERROR_INVALID_OBJECT, "Unknown buffer"),
        SurfaceError::UnknownShellSurface => {
            (WL_DISPLAY_ERROR_INVALID_OBJECT, "Unknown shell surface")
        }
        SurfaceError::UnknownRegion => (WL_DISPLAY_ERROR_INVALID_OBJECT, "Unknown region"),
    };
    ctx.writer
        .wl_display_error(DISPLAY_OBJECT_ID)
        .object_id(object_id)
        .code(code)
        .message(message);
}

impl WlDisplay for DisplayState {
    fn sync(&mut self, ctx: &mut Ctx, _object_id: ObjectId, params: &WlDisplaySync<'_>) {
        if !register_object(ctx, params.callback(), InterfaceIndex::WlCallback, 1) {
            return;
        }
        ctx.writer
            .wl_callback_done(*params.callback())
            .callback_data(0);
        ctx.registry.free_object(*params.callback(), ctx.writer);
    }

    fn get_registry(
        &mut self,
        ctx: &mut Ctx,
        _object_id: ObjectId,
        params: &WlDisplayGetRegistry<'_>,
    ) {
        if !register_object(ctx, params.registry(), InterfaceIndex::WlRegistry, 1) {
            return;
        }
        for (&name, global) in self.globals.iter() {
            ctx.writer
                .wl_registry_global(*params.registry())
                .name(name)
                .interface(global.name)
                .version(global.version);
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
        let (id, interface_name, requested_version) = params.id();
        let interface_index = global.interface_index;
        let global_name = global.name;
        let global_version = global.version;
        if interface_name != global_name
            || requested_version == 0
            || requested_version > global_version
        {
            debug!(
                "Invalid bind for global {global_id}: interface={interface_name}, version={requested_version}"
            );
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(*id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("Global interface or version mismatch");
            return;
        }
        if !register_object(ctx, id, interface_index, requested_version) {
            return;
        }

        match interface_name {
            _ if interface_name == InterfaceIndex::WlShm.interface_name() => {
                ctx.writer.wl_shm_format(*id).format(WL_SHM_FORMAT_ARGB8888);
                ctx.writer.wl_shm_format(*id).format(WL_SHM_FORMAT_XRGB8888);
            }
            _ if interface_name == InterfaceIndex::WlSeat.interface_name() => {
                if requested_version >= 2 {
                    ctx.writer
                        .wl_seat_name(*id)
                        .name(self.seat_manager.get_name(global_id).unwrap_or_default());
                }
                ctx.writer
                    .wl_seat_capabilities(*id)
                    .capabilities(WL_SEAT_CAPABILITY_KEYBOARD);
            }
            _ => {}
        }
    }
}

impl WlCompositor for DisplayState {
    fn create_surface(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlCompositorCreateSurface<'_>,
    ) {
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map_or(1, |object| object.version.min(WL_SURFACE_VERSION));
        if !register_object(ctx, params.id(), InterfaceIndex::WlSurface, version) {
            return;
        }
        let surface_id = *params.id();
        self.surface_manager
            .create_surface(ctx.client_id, surface_id);
    }

    fn create_region(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlCompositorCreateRegion<'_>,
    ) {
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map_or(1, |object| object.version.min(WL_REGION_VERSION));
        if register_object(ctx, params.id(), InterfaceIndex::WlRegion, version) {
            self.surface_manager
                .create_region(ctx.client_id, *params.id());
        }
    }
}

impl WlShm for DisplayState {
    fn create_pool(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlShmCreatePool) {
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map_or(1, |object| object.version.min(WL_SHM_POOL_VERSION));
        if !register_object(ctx, params.id(), InterfaceIndex::WlShmPool, version) {
            return;
        }
        if let Err(error) =
            self.shm_manager
                .create_pool(ctx.client_id, *params.id(), params.fd(), params.size())
        {
            report_shm_error(ctx, *params.id(), &error);
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
        if !register_object(ctx, params.id(), InterfaceIndex::WlBuffer, 1) {
            return;
        }
        if let Err(error) = self.shm_manager.create_buffer(
            ctx.client_id,
            object_id,
            *params.id(),
            params.offset(),
            params.width(),
            params.height(),
            params.stride(),
            params.format(),
        ) {
            report_shm_error(ctx, object_id, &error);
        }
    }

    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlShmPoolDestroy<'_>) {
        ctx.registry.free_object(object_id, &mut ctx.writer);
        self.shm_manager.delete_pool(ctx.client_id, object_id);
    }

    fn resize(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlShmPoolResize<'_>) {
        if let Err(error) = self
            .shm_manager
            .resize_pool(ctx.client_id, object_id, params.size())
        {
            report_shm_error(ctx, object_id, &error);
        }
    }
}

impl WlBuffer for DisplayState {
    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlBufferDestroy<'_>) {
        ctx.registry.free_object(object_id, &mut ctx.writer);
        self.shm_manager.delete_buffer(ctx.client_id, object_id);
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
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlShellGetShellSurface<'_>,
    ) {
        if ctx.registry.interface_index(params.surface()) != Some(InterfaceIndex::WlSurface) {
            report_surface_error(ctx, params.surface(), SurfaceError::UnknownSurface);
            return;
        }
        if !register_object(ctx, params.id(), InterfaceIndex::WlShellSurface, 1) {
            return;
        }
        if let Err(error) =
            self.surface_manager
                .create_shell_surface(ctx.client_id, *params.id(), params.surface())
        {
            report_surface_error(ctx, object_id, error);
        }
    }
}

impl WlShellSurface for DisplayState {
    fn pong(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlShellSurfacePong<'_>) {}

    fn move_(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlShellSurfaceMove<'_>) {}

    fn resize(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlShellSurfaceResize<'_>) {
    }

    fn set_toplevel(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &WlShellSurfaceSetToplevel<'_>,
    ) {
        if let Err(error) =
            self.surface_manager
                .set_shell_mode(ctx.client_id, object_id, ShellMode::Toplevel)
        {
            report_surface_error(ctx, object_id, error);
        } else if let Ok(surface_id) = self
            .surface_manager
            .surface_for_shell(ctx.client_id, object_id)
        {
            self.seat_manager
                .focus_keyboards_on_surface(ctx.client_id, surface_id, ctx.writer);
        }
    }

    fn set_transient(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &WlShellSurfaceSetTransient<'_>,
    ) {
        if let Err(error) =
            self.surface_manager
                .set_shell_mode(ctx.client_id, object_id, ShellMode::Transient)
        {
            report_surface_error(ctx, object_id, error);
        }
    }

    fn set_fullscreen(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &WlShellSurfaceSetFullscreen<'_>,
    ) {
        if let Err(error) =
            self.surface_manager
                .set_shell_mode(ctx.client_id, object_id, ShellMode::Fullscreen)
        {
            report_surface_error(ctx, object_id, error);
        }
    }

    fn set_popup(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &WlShellSurfaceSetPopup<'_>,
    ) {
        if let Err(error) =
            self.surface_manager
                .set_shell_mode(ctx.client_id, object_id, ShellMode::Popup)
        {
            report_surface_error(ctx, object_id, error);
        }
    }

    fn set_maximized(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &WlShellSurfaceSetMaximized<'_>,
    ) {
        if let Err(error) =
            self.surface_manager
                .set_shell_mode(ctx.client_id, object_id, ShellMode::Maximized)
        {
            report_surface_error(ctx, object_id, error);
        }
    }

    fn set_title(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlShellSurfaceSetTitle<'_>,
    ) {
        if let Err(error) = self.surface_manager.set_shell_title(
            ctx.client_id,
            object_id,
            params.title().to_owned(),
        ) {
            report_surface_error(ctx, object_id, error);
        }
    }

    fn set_class(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlShellSurfaceSetClass<'_>,
    ) {
        if let Err(error) = self.surface_manager.set_shell_class(
            ctx.client_id,
            object_id,
            params.class_().to_owned(),
        ) {
            report_surface_error(ctx, object_id, error);
        }
    }
}

impl WlSurface for DisplayState {
    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlSurfaceDestroy<'_>) {
        match self
            .surface_manager
            .destroy_surface(ctx.client_id, object_id)
        {
            Ok((shell_id, callbacks, was_mapped)) => {
                for callback in callbacks {
                    ctx.registry.free_object(callback, ctx.writer);
                }
                if let Some(shell_id) = shell_id {
                    ctx.registry.free_object(shell_id, ctx.writer);
                }
                if was_mapped {
                    self.surface_updates.push_back(SurfaceUpdate::Unmapped {
                        client_id: ctx.client_id,
                        surface_id: object_id,
                    });
                }
            }
            Err(error) => {
                report_surface_error(ctx, object_id, error);
                return;
            }
        }
        ctx.registry.free_object(object_id, &mut ctx.writer);
    }

    fn attach(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlSurfaceAttach<'_>) {
        let pending_buffer = params.buffer();
        if pending_buffer.is_some_and(|buffer| {
            ctx.registry.interface_index(buffer) != Some(InterfaceIndex::WlBuffer)
        }) {
            report_surface_error(ctx, pending_buffer.unwrap(), SurfaceError::UnknownBuffer);
            return;
        }
        if let Err(error) = self.surface_manager.attach(
            ctx.client_id,
            object_id,
            pending_buffer,
            params.x(),
            params.y(),
        ) {
            report_surface_error(ctx, object_id, error);
        }
    }

    fn damage(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlSurfaceDamage<'_>) {
        let rectangle = Rectangle {
            x: params.x(),
            y: params.y(),
            width: params.width(),
            height: params.height(),
        };
        if let Err(error) = self
            .surface_manager
            .damage(ctx.client_id, object_id, rectangle)
        {
            report_surface_error(ctx, object_id, error);
        }
    }

    fn frame(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlSurfaceFrame<'_>) {
        if !register_object(ctx, params.callback(), InterfaceIndex::WlCallback, 1) {
            return;
        }
        if let Err(error) =
            self.surface_manager
                .add_frame_callback(ctx.client_id, object_id, *params.callback())
        {
            ctx.registry.free_object(*params.callback(), ctx.writer);
            report_surface_error(ctx, object_id, error);
        }
    }

    fn set_opaque_region(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlSurfaceSetOpaqueRegion<'_>,
    ) {
        let region = params.region();
        if region
            .is_some_and(|id| ctx.registry.interface_index(id) != Some(InterfaceIndex::WlRegion))
        {
            report_surface_error(ctx, region.unwrap(), SurfaceError::UnknownRegion);
            return;
        }
        if let Err(error) = self
            .surface_manager
            .set_opaque_region(ctx.client_id, object_id, region)
        {
            report_surface_error(ctx, object_id, error);
        }
    }

    fn set_input_region(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlSurfaceSetInputRegion<'_>,
    ) {
        let region = params.region();
        if region
            .is_some_and(|id| ctx.registry.interface_index(id) != Some(InterfaceIndex::WlRegion))
        {
            report_surface_error(ctx, region.unwrap(), SurfaceError::UnknownRegion);
            return;
        }
        if let Err(error) = self
            .surface_manager
            .set_input_region(ctx.client_id, object_id, region)
        {
            report_surface_error(ctx, object_id, error);
        }
    }

    fn commit(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlSurfaceCommit<'_>) {
        let commit = match self.surface_manager.commit(ctx.client_id, object_id) {
            Ok(commit) => commit,
            Err(error) => {
                report_surface_error(ctx, object_id, error);
                return;
            }
        };

        if let Some(Some(buffer_id)) = commit.attached_buffer {
            if commit.mapped {
                match self.shm_manager.snapshot_buffer(ctx.client_id, buffer_id) {
                    Ok(snapshot) => {
                        self.surface_updates
                            .push_back(SurfaceUpdate::Frame(CommittedFrame {
                                client_id: ctx.client_id,
                                surface_id: commit.surface_id,
                                buffer_id,
                                pixels: snapshot.pixels,
                                width: snapshot.width,
                                height: snapshot.height,
                                stride: snapshot.stride,
                                format: snapshot.format,
                            }));
                    }
                    Err(error) => {
                        report_shm_error(ctx, buffer_id, &error);
                        return;
                    }
                }
            }
            ctx.writer.wl_buffer_release(buffer_id);
        } else if commit.attached_buffer == Some(None) {
            self.surface_updates.push_back(SurfaceUpdate::Unmapped {
                client_id: ctx.client_id,
                surface_id: commit.surface_id,
            });
        }

        for callback in commit.frame_callbacks {
            ctx.writer.wl_callback_done(callback).callback_data(0);
            ctx.registry.free_object(callback, ctx.writer);
        }
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
    fn get_pointer(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlSeatGetPointer<'_>) {
        ctx.writer
            .wl_display_error(DISPLAY_OBJECT_ID)
            .object_id(object_id)
            .code(WL_SEAT_ERROR_MISSING_CAPABILITY)
            .message("Seat has no pointer capability");
    }

    fn get_keyboard(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlSeatGetKeyboard<'_>) {
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map_or(1, |object| object.version.min(WL_KEYBOARD_VERSION));
        if !register_object(ctx, params.id(), InterfaceIndex::WlKeyboard, version) {
            return;
        }
        let focus = self.surface_manager.first_surface(ctx.client_id);
        if let Err(err) = self.seat_manager.create_keyboard(
            ctx.client_id,
            *params.id(),
            version,
            ctx.writer,
            focus,
        ) {
            log::error!("Failed to create wl_keyboard: {err:#}");
        }
    }

    fn get_touch(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlSeatGetTouch<'_>) {
        ctx.writer
            .wl_display_error(DISPLAY_OBJECT_ID)
            .object_id(object_id)
            .code(WL_SEAT_ERROR_MISSING_CAPABILITY)
            .message("Seat has no touch capability");
    }

    fn release(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlSeatRelease<'_>) {
        ctx.registry.free_object(object_id, ctx.writer);
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
    fn release(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlKeyboardRelease<'_>) {
        self.seat_manager.destroy_keyboard(ctx.client_id, object_id);
        ctx.registry.free_object(object_id, ctx.writer);
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
    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlRegionDestroy<'_>) {
        if let Err(error) = self
            .surface_manager
            .destroy_region(ctx.client_id, object_id)
        {
            report_surface_error(ctx, object_id, error);
            return;
        }
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn add(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlRegionAdd<'_>) {
        let rectangle = Rectangle {
            x: params.x(),
            y: params.y(),
            width: params.width(),
            height: params.height(),
        };
        if let Err(error) = self
            .surface_manager
            .add_region(ctx.client_id, object_id, rectangle)
        {
            report_surface_error(ctx, object_id, error);
        }
    }

    fn subtract(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlRegionSubtract<'_>) {
        let rectangle = Rectangle {
            x: params.x(),
            y: params.y(),
            width: params.width(),
            height: params.height(),
        };
        if let Err(error) =
            self.surface_manager
                .subtract_region(ctx.client_id, object_id, rectangle)
        {
            report_surface_error(ctx, object_id, error);
        }
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

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs::File,
        io::Write,
        num::NonZeroU32,
        os::{
            fd::{AsRawFd, FromRawFd, IntoRawFd},
            unix::net::UnixStream,
        },
        ptr,
        sync::atomic::{AtomicU64, Ordering},
    };

    use lumalla_shared::{DbusMessage, MainMessage, message_loop_with_channel};
    use lumalla_wayland_protocol::{
        ClientId,
        buffer::Writer,
        registry::{InterfaceIndex, Registry},
    };

    use super::*;

    fn object_id(id: u32) -> ObjectId {
        ObjectId::new(NonZeroU32::new(id).unwrap())
    }

    fn bind_data(name: u32, interface: &str, version: u32, id: u32) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&name.to_ne_bytes());
        let string_len = interface.len() + 1;
        data.extend_from_slice(&(string_len as u32).to_ne_bytes());
        data.extend_from_slice(interface.as_bytes());
        data.push(0);
        data.resize((data.len() + 3) & !3, 0);
        data.extend_from_slice(&version.to_ne_bytes());
        data.extend_from_slice(&id.to_ne_bytes());
        data
    }

    fn display_state() -> DisplayState {
        let (_main_poll, _main_rx, to_main) = message_loop_with_channel::<MainMessage>().unwrap();
        let (_dbus_poll, _dbus_rx, to_dbus) = message_loop_with_channel::<DbusMessage>().unwrap();
        DisplayState::new(lumalla_shared::Comms::new(to_main, to_dbus)).unwrap()
    }

    fn memory_file(bytes: &[u8]) -> i32 {
        let fd = unsafe { libc::memfd_create(c"lumalla-surface-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.set_len(bytes.len() as u64).unwrap();
        file.write_all(bytes).unwrap();
        file.into_raw_fd()
    }

    fn wire_message(object_id: u32, opcode: u16, payload: &[u32]) -> Vec<u8> {
        let size = 8 + payload.len() * 4;
        let mut message = Vec::with_capacity(size);
        message.extend_from_slice(&object_id.to_ne_bytes());
        message.extend_from_slice(&opcode.to_ne_bytes());
        message.extend_from_slice(&(size as u16).to_ne_bytes());
        for value in payload {
            message.extend_from_slice(&value.to_ne_bytes());
        }
        message
    }

    fn wire_bind(name: u32, interface: &str, id: u32) -> Vec<u8> {
        let mut data = bind_data(name, interface, 1, id);
        let size = 8 + data.len();
        let mut message = Vec::with_capacity(size);
        message.extend_from_slice(&2u32.to_ne_bytes());
        message.extend_from_slice(&WL_REGISTRY_BIND_OPCODE.to_ne_bytes());
        message.extend_from_slice(&(size as u16).to_ne_bytes());
        message.append(&mut data);
        message
    }

    fn send_wire_with_fd(stream: &UnixStream, bytes: &[u8], fd: i32) {
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let control_len = unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as usize;
        let mut control = vec![0usize; control_len.div_ceil(std::mem::size_of::<usize>())];
        let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
        header.msg_iov = &mut iov;
        header.msg_iovlen = 1;
        header.msg_control = control.as_mut_ptr().cast();
        header.msg_controllen = control_len;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&header);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as usize;
            ptr::write(libc::CMSG_DATA(cmsg).cast::<i32>(), fd);
        }
        let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &header, libc::MSG_NOSIGNAL) };
        assert_eq!(sent as usize, bytes.len());
    }

    #[test]
    fn advertises_only_the_minimal_implemented_globals() {
        let state = display_state();
        let globals: Vec<_> = state
            .globals
            .iter()
            .map(|(_, global)| (global.name, global.version))
            .collect();

        assert!(globals.contains(&(WL_COMPOSITOR_NAME, 1)));
        assert!(globals.contains(&(WL_SHM_NAME, 1)));
        assert!(globals.contains(&(WL_SHELL_NAME, 1)));
    }

    #[test]
    fn registry_bind_records_requested_version() {
        let (_receiver, sender) = UnixStream::pair().unwrap();
        let mut state = display_state();
        let mut registry = Registry::new();
        let mut writer = Writer::new(sender.as_raw_fd());
        let mut ctx = Ctx {
            registry: &mut registry,
            writer: &mut writer,
            client_id: ClientId::new(NonZeroU32::new(1).unwrap()),
        };
        let mut fds = VecDeque::new();
        let data = bind_data(1, "wl_compositor", 1, 2);
        let params = WlRegistryBind::new(&data, &mut fds);

        WlRegistry::bind(&mut state, &mut ctx, object_id(10), &params);

        let metadata = ctx.registry.object_metadata(object_id(2)).unwrap();
        assert_eq!(metadata.interface_index, InterfaceIndex::WlCompositor);
        assert_eq!(metadata.version, 1);
    }

    #[test]
    fn registry_bind_rejects_interface_and_version_mismatches() {
        for data in [
            bind_data(1, "wl_shm", 1, 2),
            bind_data(1, "wl_compositor", WL_COMPOSITOR_VERSION + 1, 2),
            bind_data(1, "wl_compositor", 0, 2),
        ] {
            let (_receiver, sender) = UnixStream::pair().unwrap();
            let mut state = display_state();
            let mut registry = Registry::new();
            let mut writer = Writer::new(sender.as_raw_fd());
            let mut ctx = Ctx {
                registry: &mut registry,
                writer: &mut writer,
                client_id: ClientId::new(NonZeroU32::new(1).unwrap()),
            };
            let mut fds = VecDeque::new();
            let params = WlRegistryBind::new(&data, &mut fds);

            WlRegistry::bind(&mut state, &mut ctx, object_id(10), &params);

            assert!(ctx.registry.object_metadata(object_id(2)).is_none());
        }
    }

    #[test]
    fn mapped_surface_commit_snapshots_and_releases_buffer() {
        let (_receiver, sender) = UnixStream::pair().unwrap();
        let mut state = display_state();
        let client_id = ClientId::new(NonZeroU32::new(1).unwrap());
        let surface_id = object_id(2);
        let shell_id = object_id(3);
        let pool_id = object_id(4);
        let buffer_id = object_id(5);
        let callback_id = object_id(6);
        state.surface_manager.create_surface(client_id, surface_id);
        state
            .surface_manager
            .create_shell_surface(client_id, shell_id, surface_id)
            .unwrap();
        state
            .surface_manager
            .set_shell_mode(client_id, shell_id, ShellMode::Toplevel)
            .unwrap();
        state
            .shm_manager
            .create_pool(client_id, pool_id, memory_file(&[1, 2, 3, 4]), 4)
            .unwrap();
        state
            .shm_manager
            .create_buffer(
                client_id,
                pool_id,
                buffer_id,
                0,
                1,
                1,
                4,
                WL_SHM_FORMAT_ARGB8888,
            )
            .unwrap();
        state
            .surface_manager
            .attach(client_id, surface_id, Some(buffer_id), 0, 0)
            .unwrap();
        state
            .surface_manager
            .add_frame_callback(client_id, surface_id, callback_id)
            .unwrap();

        let mut registry = Registry::new();
        registry
            .register_client_object_with_version(
                NewObjectId::new(surface_id),
                InterfaceIndex::WlSurface,
                1,
            )
            .unwrap();
        registry
            .register_client_object_with_version(
                NewObjectId::new(buffer_id),
                InterfaceIndex::WlBuffer,
                1,
            )
            .unwrap();
        registry
            .register_client_object_with_version(
                NewObjectId::new(callback_id),
                InterfaceIndex::WlCallback,
                1,
            )
            .unwrap();
        let mut writer = Writer::new(sender.as_raw_fd());
        let mut ctx = Ctx {
            registry: &mut registry,
            writer: &mut writer,
            client_id,
        };
        let mut fds = VecDeque::new();
        let params = WlSurfaceCommit::new(&[], &mut fds);

        WlSurface::commit(&mut state, &mut ctx, surface_id, &params);

        assert!(ctx.registry.object_metadata(callback_id).is_none());
        let updates: Vec<_> = state.take_surface_updates().collect();
        assert_eq!(updates.len(), 1);
        let SurfaceUpdate::Frame(frame) = &updates[0] else {
            panic!("expected a committed frame");
        };
        assert_eq!(frame.surface_id, surface_id);
        assert_eq!(frame.buffer_id, buffer_id);
        assert_eq!(frame.pixels, [1, 2, 3, 4]);

        state
            .surface_manager
            .attach(client_id, surface_id, None, 0, 0)
            .unwrap();
        WlSurface::commit(&mut state, &mut ctx, surface_id, &params);
        let updates: Vec<_> = state.take_surface_updates().collect();
        assert!(matches!(
            updates.as_slice(),
            [SurfaceUpdate::Unmapped {
                client_id: owner,
                surface_id: unmapped,
            }] if *owner == client_id && *unmapped == surface_id
        ));
    }

    #[test]
    fn wire_client_can_commit_a_wl_shell_shm_surface() {
        static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);
        let socket_path = std::env::temp_dir().join(format!(
            "lumalla-wayland-test-{}-{}",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
        ));
        let mut wayland =
            lumalla_wayland_protocol::Wayland::new(socket_path.to_string_lossy().into_owned())
                .unwrap();
        let client_stream = UnixStream::connect(&socket_path).unwrap();
        let mut client = wayland.next_client().unwrap();
        let mut state = display_state();

        let mut wire = Vec::new();
        wire.extend(wire_message(1, WL_DISPLAY_GET_REGISTRY_OPCODE, &[2]));
        wire.extend(wire_bind(1, "wl_compositor", 3));
        wire.extend(wire_bind(2, "wl_shm", 4));
        wire.extend(wire_bind(3, "wl_shell", 5));
        wire.extend(wire_message(3, WL_COMPOSITOR_CREATE_SURFACE_OPCODE, &[6]));
        wire.extend(wire_message(4, WL_SHM_CREATE_POOL_OPCODE, &[7, 4]));
        wire.extend(wire_message(
            7,
            WL_SHM_POOL_CREATE_BUFFER_OPCODE,
            &[8, 0, 1, 1, 4, WL_SHM_FORMAT_XRGB8888],
        ));
        wire.extend(wire_message(5, WL_SHELL_GET_SHELL_SURFACE_OPCODE, &[9, 6]));
        wire.extend(wire_message(9, WL_SHELL_SURFACE_SET_TOPLEVEL_OPCODE, &[]));
        wire.extend(wire_message(6, WL_SURFACE_ATTACH_OPCODE, &[8, 0, 0]));
        wire.extend(wire_message(6, WL_SURFACE_DAMAGE_OPCODE, &[0, 0, 1, 1]));
        wire.extend(wire_message(6, WL_SURFACE_FRAME_OPCODE, &[10]));
        wire.extend(wire_message(6, WL_SURFACE_COMMIT_OPCODE, &[]));

        let fd = memory_file(&[1, 2, 3, 0xff]);
        send_wire_with_fd(&client_stream, &wire, fd);
        unsafe {
            libc::close(fd);
        }
        client.handle_messages(&mut state).unwrap();

        let updates: Vec<_> = state.take_surface_updates().collect();
        let [SurfaceUpdate::Frame(frame)] = updates.as_slice() else {
            panic!("expected one committed frame");
        };
        assert_eq!(frame.client_id, client.client_id());
        assert_eq!(frame.surface_id, object_id(6));
        assert_eq!(frame.buffer_id, object_id(8));
        assert_eq!(frame.pixels, [1, 2, 3, 0xff]);
        assert_eq!((frame.width, frame.height, frame.stride), (1, 1, 4));
        assert_eq!(frame.format, WL_SHM_FORMAT_XRGB8888);
    }
}
