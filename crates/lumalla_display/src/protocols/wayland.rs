use log::debug;
use lumalla_wayland_protocol::{
    Ctx, NewObjectId, ObjectId,
    protocols::{WaylandProtocol, WlDisplay, wayland::*},
    registry::{DISPLAY_OBJECT_ID, InterfaceIndex},
};

use crate::{
    DisplayState, GlobalId,
    shm::{ShmError, ShmErrorKind},
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

impl WlDisplay for DisplayState {
    fn sync(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlDisplaySync<'_>) {
        if !register_object(ctx, params.callback(), InterfaceIndex::WlCallback, 1) {
            return;
        }
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
        self.seat_manager
            .focus_keyboards_on_surface(ctx.client_id, surface_id, ctx.writer);
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
        register_object(ctx, params.id(), InterfaceIndex::WlRegion, version);
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

    fn get_keyboard(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlSeatGetKeyboard<'_>) {
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map_or(1, |object| object.version.min(WL_KEYBOARD_VERSION));
        if !register_object(ctx, params.id(), InterfaceIndex::WlKeyboard, version) {
            return;
        }
        let focus = self.surface_manager.first_surface(ctx.client_id);
        if let Err(err) =
            self.seat_manager
                .create_keyboard(ctx.client_id, *params.id(), ctx.writer, focus)
        {
            log::error!("Failed to create wl_keyboard: {err:#}");
        }
    }

    fn get_touch(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &WlSeatGetTouch<'_>) {
        todo!()
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

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        num::NonZeroU32,
        os::{fd::AsRawFd, unix::net::UnixStream},
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
        let data = bind_data(1, "wl_compositor", 3, 2);
        let params = WlRegistryBind::new(&data, &mut fds);

        WlRegistry::bind(&mut state, &mut ctx, object_id(10), &params);

        let metadata = ctx.registry.object_metadata(object_id(2)).unwrap();
        assert_eq!(metadata.interface_index, InterfaceIndex::WlCompositor);
        assert_eq!(metadata.version, 3);
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
}
