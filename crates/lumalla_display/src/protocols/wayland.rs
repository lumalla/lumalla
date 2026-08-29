use log::debug;
use lumalla_wayland_protocol::{
    Ctx, NewObjectId, ObjectId,
    protocols::{WaylandProtocol, WlDisplay, wayland::*},
    registry::{DISPLAY_OBJECT_ID, InterfaceIndex},
};

use crate::{
    CommittedFrame, DisplayState, GlobalId, SurfaceUpdate,
    data_device::DataDeviceError,
    shm::{ShmError, ShmErrorKind},
    surface::{
        Rectangle, ShellMode, SurfaceCommit, SurfaceError, effective_surface_size,
    },
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
        SurfaceError::UnknownSubsurface => (WL_DISPLAY_ERROR_INVALID_OBJECT, "Unknown subsurface"),
        SurfaceError::BadParent => (
            WL_SUBCOMPOSITOR_ERROR_BAD_PARENT,
            "Invalid subsurface parent",
        ),
        SurfaceError::BadSurface => (
            WL_SUBCOMPOSITOR_ERROR_BAD_SURFACE,
            "Invalid subsurface surface",
        ),
        SurfaceError::InvalidScale => (WL_SURFACE_ERROR_INVALID_SCALE, "Buffer scale must be > 0"),
        SurfaceError::InvalidTransform => (
            WL_SURFACE_ERROR_INVALID_TRANSFORM,
            "Invalid buffer transform",
        ),
        SurfaceError::InvalidOffset => (
            WL_SURFACE_ERROR_INVALID_OFFSET,
            "Attach offset must be zero since version 5",
        ),
        SurfaceError::ViewportExists
        | SurfaceError::NoSurface
        | SurfaceError::ViewportBadValue => (WL_DISPLAY_ERROR_INVALID_OBJECT, "Viewport error"),
    };
    ctx.writer
        .wl_display_error(DISPLAY_OBJECT_ID)
        .object_id(object_id)
        .code(code)
        .message(message);
}

fn report_subcompositor_error(ctx: &mut Ctx, object_id: ObjectId, error: SurfaceError) {
    let (code, message) = match error {
        SurfaceError::BadParent => (
            WL_SUBCOMPOSITOR_ERROR_BAD_PARENT,
            "Invalid subsurface parent",
        ),
        SurfaceError::BadSurface | SurfaceError::RoleAlreadyAssigned => (
            WL_SUBCOMPOSITOR_ERROR_BAD_SURFACE,
            "Invalid subsurface surface",
        ),
        other => {
            report_surface_error(ctx, object_id, other);
            return;
        }
    };
    ctx.writer
        .wl_display_error(DISPLAY_OBJECT_ID)
        .object_id(object_id)
        .code(code)
        .message(message);
}

fn report_subsurface_error(ctx: &mut Ctx, object_id: ObjectId, error: SurfaceError) {
    let (code, message) = match error {
        SurfaceError::BadSurface => (WL_SUBSURFACE_ERROR_BAD_SURFACE, "Invalid sibling surface"),
        other => {
            report_surface_error(ctx, object_id, other);
            return;
        }
    };
    ctx.writer
        .wl_display_error(DISPLAY_OBJECT_ID)
        .object_id(object_id)
        .code(code)
        .message(message);
}

fn report_data_device_error(ctx: &mut Ctx, object_id: ObjectId, error: DataDeviceError) {
    let (code, message) = match error {
        DataDeviceError::UsedSource => (WL_DATA_DEVICE_ERROR_USED_SOURCE, "Source already used"),
        DataDeviceError::RoleConflict => (WL_DATA_DEVICE_ERROR_ROLE, "Surface already has a role"),
        DataDeviceError::InvalidActionMask => {
            // Prefer source/offer-specific codes when possible; default to offer.
            (
                WL_DATA_OFFER_ERROR_INVALID_ACTION_MASK,
                "Invalid action mask",
            )
        }
        DataDeviceError::InvalidAction => (WL_DATA_OFFER_ERROR_INVALID_ACTION, "Invalid action"),
        DataDeviceError::InvalidFinish => (WL_DATA_OFFER_ERROR_INVALID_FINISH, "Invalid finish"),
        DataDeviceError::InvalidOffer => {
            (WL_DATA_OFFER_ERROR_INVALID_OFFER, "Invalid offer request")
        }
        DataDeviceError::InvalidSource => (
            WL_DATA_SOURCE_ERROR_INVALID_SOURCE,
            "Invalid source request",
        ),
        DataDeviceError::UnknownSource
        | DataDeviceError::UnknownDevice
        | DataDeviceError::UnknownOffer
        | DataDeviceError::UnknownSeat
        | DataDeviceError::UnknownSurface => (WL_DISPLAY_ERROR_INVALID_OBJECT, "Unknown object"),
    };
    debug!("Data device protocol error: {error}");
    ctx.writer
        .wl_display_error(DISPLAY_OBJECT_ID)
        .object_id(object_id)
        .code(code)
        .message(message);
}

fn div_ceil_i32(value: i32, divisor: i32) -> i32 {
    let divisor = i64::from(divisor.max(1));
    let value = i64::from(value.max(0));
    ((value + divisor - 1) / divisor)
        .try_into()
        .unwrap_or(i32::MAX)
}

/// Maps commit damage hints to output-space and buffer-space rectangles.
fn commit_damage(
    commit: &SurfaceCommit,
    output_x: i32,
    output_y: i32,
    buffer_width: usize,
    buffer_height: usize,
    surface_width: i32,
    surface_height: i32,
) -> (Vec<Rectangle>, Vec<Rectangle>, bool) {
    let full_surface =
        commit.newly_mapped || (commit.damage.is_empty() && commit.buffer_damage.is_empty());
    if full_surface {
        return (Vec::new(), Vec::new(), true);
    }

    let scale = commit.buffer_scale.max(1);
    let mut output_damage = Vec::new();
    let mut buffer_damage = Vec::new();
    if !commit.buffer_damage.is_empty() {
        for rect in &commit.buffer_damage {
            if rect.width <= 0 || rect.height <= 0 {
                continue;
            }
            buffer_damage.push(*rect);
            // Buffer damage → output: approximate via surface size mapping when viewport scales.
            let out_w = if buffer_width == 0 {
                0
            } else {
                div_ceil_i32(
                    rect.width.saturating_mul(surface_width),
                    buffer_width as i32,
                )
            };
            let out_h = if buffer_height == 0 {
                0
            } else {
                div_ceil_i32(
                    rect.height.saturating_mul(surface_height),
                    buffer_height as i32,
                )
            };
            let out_x_off = if buffer_width == 0 {
                0
            } else {
                rect.x.saturating_mul(surface_width) / buffer_width as i32
            };
            let out_y_off = if buffer_height == 0 {
                0
            } else {
                rect.y.saturating_mul(surface_height) / buffer_height as i32
            };
            output_damage.push(Rectangle {
                x: output_x + out_x_off,
                y: output_y + out_y_off,
                width: out_w,
                height: out_h,
            });
        }
    } else {
        for rect in &commit.damage {
            if rect.width <= 0 || rect.height <= 0 {
                continue;
            }
            output_damage.push(Rectangle {
                x: output_x + rect.x,
                y: output_y + rect.y,
                width: rect.width,
                height: rect.height,
            });
            // Surface-local damage → buffer space via viewport source or scale.
            if let Some((sx, sy, sw, sh)) = commit.viewport.source {
                let src_x = (sx * scale as f32) as i32;
                let src_y = (sy * scale as f32) as i32;
                let src_w = (sw * scale as f32).ceil() as i32;
                let src_h = (sh * scale as f32).ceil() as i32;
                let bw = if surface_width <= 0 {
                    0
                } else {
                    rect.width.saturating_mul(src_w) / surface_width
                };
                let bh = if surface_height <= 0 {
                    0
                } else {
                    rect.height.saturating_mul(src_h) / surface_height
                };
                buffer_damage.push(Rectangle {
                    x: src_x + if surface_width <= 0 {
                        0
                    } else {
                        rect.x.saturating_mul(src_w) / surface_width
                    },
                    y: src_y + if surface_height <= 0 {
                        0
                    } else {
                        rect.y.saturating_mul(src_h) / surface_height
                    },
                    width: bw.max(1),
                    height: bh.max(1),
                });
            } else {
                buffer_damage.push(Rectangle {
                    x: rect.x.saturating_mul(scale),
                    y: rect.y.saturating_mul(scale),
                    width: rect.width.saturating_mul(scale),
                    height: rect.height.saturating_mul(scale),
                });
            }
        }
    }

    if output_damage.is_empty() {
        output_damage.push(Rectangle {
            x: output_x,
            y: output_y,
            width: surface_width,
            height: surface_height,
        });
        if let Some((sx, sy, sw, sh)) = commit.viewport.source {
            buffer_damage.push(Rectangle {
                x: (sx * scale as f32) as i32,
                y: (sy * scale as f32) as i32,
                width: (sw * scale as f32).ceil() as i32,
                height: (sh * scale as f32).ceil() as i32,
            });
        } else {
            buffer_damage.push(Rectangle {
                x: 0,
                y: 0,
                width: buffer_width as i32,
                height: buffer_height as i32,
            });
        }
    }

    (output_damage, buffer_damage, false)
}

fn process_surface_commit(state: &mut DisplayState, ctx: &mut Ctx, commit: SurfaceCommit) {
    match commit.attached_buffer {
        Some(Some(buffer_id)) => {
            let is_cursor = state
                .surface_manager
                .surface_role_is_cursor(ctx.client_id, commit.surface_id);
            if is_cursor || commit.mapped {
                let (pixels, width, height, stride, format, dmabuf) =
                    if state.dmabuf_manager.has_buffer(ctx.client_id, buffer_id) {
                        match state.dmabuf_manager.export_buffer(ctx.client_id, buffer_id) {
                            Ok(exported) => {
                                let width = exported.width as usize;
                                let height = exported.height as usize;
                                let stride = exported.stride as usize;
                                let format = exported.wl_format;
                                (Vec::new(), width, height, stride, format, Some(exported))
                            }
                            Err(error) => {
                                debug!("dmabuf export failed: {error}");
                                let message = error.to_string();
                                ctx.writer
                                    .wl_display_error(DISPLAY_OBJECT_ID)
                                    .object_id(buffer_id)
                                    .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                                    .message(&message);
                                return;
                            }
                        }
                    } else {
                        match state.shm_manager.snapshot_buffer(ctx.client_id, buffer_id) {
                            Ok(snapshot) => (
                                snapshot.pixels,
                                snapshot.width,
                                snapshot.height,
                                snapshot.stride,
                                snapshot.format,
                                None,
                            ),
                            Err(error) => {
                                report_shm_error(ctx, buffer_id, &error);
                                return;
                            }
                        }
                    };
                if let Err((viewport_id, error)) = state.surface_manager.validate_viewport_commit(
                    ctx.client_id,
                    commit.surface_id,
                    Some(width as i32),
                    Some(height as i32),
                ) {
                    super::viewporter::report_viewport_commit_error(ctx, viewport_id, error);
                    return;
                }
                let _ = state.surface_manager.set_committed_buffer_size(
                    ctx.client_id,
                    commit.surface_id,
                    width as i32,
                    height as i32,
                );
                let (surface_width, surface_height) = effective_surface_size(
                    Some((width as i32, height as i32)),
                    commit.buffer_scale,
                    &commit.viewport,
                )
                .unwrap_or((0, 0));
                let output_x = commit.layout.0 + commit.offset.0;
                let output_y = commit.layout.1 + commit.offset.1;
                let (damage, buffer_damage, full_surface) = commit_damage(
                    &commit,
                    output_x,
                    output_y,
                    width,
                    height,
                    surface_width,
                    surface_height,
                );
                let frame = CommittedFrame {
                    client_id: ctx.client_id,
                    surface_id: commit.surface_id,
                    buffer_id,
                    pixels,
                    width,
                    height,
                    stride,
                    format,
                    buffer_scale: commit.buffer_scale,
                    buffer_transform: commit.buffer_transform,
                    offset_x: commit.offset.0,
                    offset_y: commit.offset.1,
                    x: if is_cursor { 0 } else { output_x },
                    y: if is_cursor { 0 } else { output_y },
                    surface_width,
                    surface_height,
                    viewport_src: commit.viewport.source,
                    dmabuf,
                    damage,
                    buffer_damage: if is_cursor {
                        Vec::new()
                    } else {
                        buffer_damage
                    },
                    full_surface: is_cursor || full_surface,
                };
                if is_cursor {
                    state
                        .surface_updates
                        .push_back(SurfaceUpdate::Cursor(frame));
                } else {
                    state
                        .surface_updates
                        .push_back(SurfaceUpdate::Frame(frame));
                }
                if commit.newly_mapped {
                    if let Some(shell_id) = commit.shell_id {
                        let serial = state.seat_manager.next_serial();
                        if state
                            .surface_manager
                            .set_pending_shell_ping(ctx.client_id, shell_id, serial)
                            .is_ok()
                        {
                            ctx.writer.wl_shell_surface_ping(shell_id).serial(serial);
                        }
                    }
                    state.seat_manager.focus_keyboards_on_surface(
                        ctx.client_id,
                        commit.surface_id,
                        ctx.writer,
                    );
                    state.on_surface_focused(ctx.client_id, commit.surface_id);
                    for output in state.output_manager.bound_outputs_for_client(ctx.client_id) {
                        ctx.writer
                            .wl_surface_enter(commit.surface_id)
                            .output(output);
                    }
                }
            }
            ctx.writer.wl_buffer_release(buffer_id);
        }
        Some(None) => {
            if let Err((viewport_id, error)) = state.surface_manager.validate_viewport_commit(
                ctx.client_id,
                commit.surface_id,
                None,
                None,
            ) {
                super::viewporter::report_viewport_commit_error(ctx, viewport_id, error);
                return;
            }
            let _ = state
                .surface_manager
                .clear_committed_buffer_size(ctx.client_id, commit.surface_id);
            state.seat_manager.leave_keyboards_on_surface(
                ctx.client_id,
                commit.surface_id,
                ctx.writer,
            );
            state.seat_manager.leave_pointers_on_surface(
                ctx.client_id,
                commit.surface_id,
                ctx.writer,
            );
            state.surface_updates.push_back(SurfaceUpdate::Unmapped {
                client_id: ctx.client_id,
                surface_id: commit.surface_id,
            });
            for output in state.output_manager.bound_outputs_for_client(ctx.client_id) {
                ctx.writer
                    .wl_surface_leave(commit.surface_id)
                    .output(output);
            }
            state.discard_presentation_feedbacks_for_surface(
                ctx.client_id,
                commit.surface_id,
                Vec::new(),
                ctx.writer,
                ctx.registry,
            );
        }
        None => {
            let buffer_dims = state
                .surface_manager
                .committed_buffer_size(ctx.client_id, commit.surface_id);
            if let Err((viewport_id, error)) = state.surface_manager.validate_viewport_commit(
                ctx.client_id,
                commit.surface_id,
                buffer_dims.map(|(w, _)| w),
                buffer_dims.map(|(_, h)| h),
            ) {
                super::viewporter::report_viewport_commit_error(ctx, viewport_id, error);
                return;
            }
            if commit.viewport_changed
                && commit.mapped
                && let Some(buffer_id) = commit.buffer
            {
                let is_cursor = state
                    .surface_manager
                    .surface_role_is_cursor(ctx.client_id, commit.surface_id);
                let (pixels, width, height, stride, format, dmabuf) =
                    if state.dmabuf_manager.has_buffer(ctx.client_id, buffer_id) {
                        match state.dmabuf_manager.export_buffer(ctx.client_id, buffer_id) {
                            Ok(exported) => (
                                Vec::new(),
                                exported.width as usize,
                                exported.height as usize,
                                exported.stride as usize,
                                exported.wl_format,
                                Some(exported),
                            ),
                            Err(error) => {
                                debug!("dmabuf export failed on viewport update: {error}");
                                return;
                            }
                        }
                    } else {
                        match state.shm_manager.snapshot_buffer(ctx.client_id, buffer_id) {
                            Ok(snapshot) => (
                                snapshot.pixels,
                                snapshot.width,
                                snapshot.height,
                                snapshot.stride,
                                snapshot.format,
                                None,
                            ),
                            Err(error) => {
                                debug!("shm snapshot failed on viewport update: {error}");
                                return;
                            }
                        }
                    };
                let (surface_width, surface_height) = effective_surface_size(
                    Some((width as i32, height as i32)),
                    commit.buffer_scale,
                    &commit.viewport,
                )
                .unwrap_or((0, 0));
                let output_x = commit.layout.0 + commit.offset.0;
                let output_y = commit.layout.1 + commit.offset.1;
                let frame = CommittedFrame {
                    client_id: ctx.client_id,
                    surface_id: commit.surface_id,
                    buffer_id,
                    pixels,
                    width,
                    height,
                    stride,
                    format,
                    buffer_scale: commit.buffer_scale,
                    buffer_transform: commit.buffer_transform,
                    offset_x: commit.offset.0,
                    offset_y: commit.offset.1,
                    x: if is_cursor { 0 } else { output_x },
                    y: if is_cursor { 0 } else { output_y },
                    surface_width,
                    surface_height,
                    viewport_src: commit.viewport.source,
                    dmabuf,
                    damage: Vec::new(),
                    buffer_damage: Vec::new(),
                    full_surface: true,
                };
                if is_cursor {
                    state
                        .surface_updates
                        .push_back(SurfaceUpdate::Cursor(frame));
                } else {
                    state
                        .surface_updates
                        .push_back(SurfaceUpdate::Frame(frame));
                }
            }
        }
    }

    for callback in commit.frame_callbacks {
        state
            .pending_frame_callbacks
            .push_back((ctx.client_id, callback));
    }

    if !commit.deferred {
        state.queue_presentation_feedbacks(
            ctx.client_id,
            commit.surface_id,
            commit.presentation_feedbacks,
            ctx.writer,
            ctx.registry,
        );
    }
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
                ctx.writer.wl_seat_capabilities(*id).capabilities(
                    WL_SEAT_CAPABILITY_KEYBOARD
                        | WL_SEAT_CAPABILITY_POINTER
                        | WL_SEAT_CAPABILITY_TOUCH,
                );
            }
            _ if interface_name == InterfaceIndex::WlOutput.interface_name() => {
                if !self.output_manager.bind_output(
                    ctx.client_id,
                    global_id,
                    *id,
                    requested_version,
                    ctx.writer,
                ) {
                    debug!("Failed to bind unknown wl_output global {global_id}");
                }
            }
            _ if interface_name == InterfaceIndex::XdgWmBase.interface_name() => {
                self.xdg_manager.create_wm_base(ctx.client_id, *id);
            }
            _ if interface_name == InterfaceIndex::ZwpLinuxDmabufV1.interface_name() => {
                super::linux_dmabuf::send_dmabuf_formats(
                    ctx.writer,
                    *id,
                    self.dmabuf_manager.supported_formats(),
                );
            }
            _ if interface_name == InterfaceIndex::WpPresentation.interface_name() => {
                ctx.writer
                    .wp_presentation_clock_id(*id)
                    .clk_id(libc::CLOCK_MONOTONIC as u32);
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
        let fd = params.fd();
        let size = params.size();
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map_or(1, |object| object.version.min(WL_SHM_POOL_VERSION));
        if !register_object(ctx, params.id(), InterfaceIndex::WlShmPool, version) {
            // Request parser already released OwnedFd into a bare RawFd; close it.
            if fd >= 0 {
                unsafe {
                    libc::close(fd);
                }
            }
            return;
        }
        if let Err(error) = self
            .shm_manager
            .create_pool(ctx.client_id, *params.id(), fd, size)
        {
            // create_pool consumes/closes `fd` on every path; drop the registry object
            // so we do not leave a wl_shm_pool id without a backing pool.
            ctx.registry.free_object(*params.id(), &mut ctx.writer);
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
        self.dmabuf_manager.delete_buffer(ctx.client_id, object_id);
    }
}

impl WlDataOffer for DisplayState {
    fn accept(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlDataOfferAccept<'_>) {
        if let Err(error) = self.data_device_manager.accept(
            ctx.client_id,
            object_id,
            params.serial(),
            params.mime_type(),
            ctx.writer,
        ) {
            report_data_device_error(ctx, object_id, error);
        }
    }

    fn receive(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlDataOfferReceive<'_>) {
        if let Err(error) = self.data_device_manager.receive(
            ctx.client_id,
            object_id,
            params.mime_type(),
            params.fd(),
            ctx.writer,
        ) {
            report_data_device_error(ctx, object_id, error);
        }
    }

    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlDataOfferDestroy<'_>) {
        if let Err(error) = self
            .data_device_manager
            .destroy_offer(ctx.client_id, object_id)
        {
            report_data_device_error(ctx, object_id, error);
            return;
        }
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn finish(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlDataOfferFinish<'_>) {
        if let Err(error) = self
            .data_device_manager
            .finish(ctx.client_id, object_id, ctx.writer)
        {
            report_data_device_error(ctx, object_id, error);
        }
    }

    fn set_actions(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlDataOfferSetActions<'_>,
    ) {
        if let Err(error) = self.data_device_manager.set_offer_actions(
            ctx.client_id,
            object_id,
            params.dnd_actions(),
            params.preferred_action(),
            ctx.writer,
        ) {
            // Prefer offer-specific error codes for action issues.
            let code = match error {
                DataDeviceError::InvalidActionMask => WL_DATA_OFFER_ERROR_INVALID_ACTION_MASK,
                DataDeviceError::InvalidAction => WL_DATA_OFFER_ERROR_INVALID_ACTION,
                DataDeviceError::InvalidOffer => WL_DATA_OFFER_ERROR_INVALID_OFFER,
                other => {
                    report_data_device_error(ctx, object_id, other);
                    return;
                }
            };
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(code)
                .message(&error.to_string());
        }
    }
}

impl WlDataSource for DisplayState {
    fn offer(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlDataSourceOffer<'_>) {
        if let Err(error) =
            self.data_device_manager
                .offer(ctx.client_id, object_id, params.mime_type())
        {
            report_data_device_error(ctx, object_id, error);
        }
    }

    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlDataSourceDestroy<'_>) {
        if let Err(error) =
            self.data_device_manager
                .destroy_source(ctx.client_id, object_id, ctx.writer)
        {
            report_data_device_error(ctx, object_id, error);
            return;
        }
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn set_actions(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlDataSourceSetActions<'_>,
    ) {
        if let Err(error) = self.data_device_manager.set_source_actions(
            ctx.client_id,
            object_id,
            params.dnd_actions(),
        ) {
            let code = match error {
                DataDeviceError::InvalidActionMask => WL_DATA_SOURCE_ERROR_INVALID_ACTION_MASK,
                DataDeviceError::InvalidSource => WL_DATA_SOURCE_ERROR_INVALID_SOURCE,
                other => {
                    report_data_device_error(ctx, object_id, other);
                    return;
                }
            };
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(code)
                .message(&error.to_string());
        }
    }
}

impl WlDataDevice for DisplayState {
    fn start_drag(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlDataDeviceStartDrag<'_>,
    ) {
        if let Some(source) = params.source() {
            if ctx.registry.interface_index(source) != Some(InterfaceIndex::WlDataSource) {
                report_data_device_error(ctx, object_id, DataDeviceError::UnknownSource);
                return;
            }
        }
        if ctx.registry.interface_index(params.origin()) != Some(InterfaceIndex::WlSurface) {
            report_data_device_error(ctx, object_id, DataDeviceError::UnknownSurface);
            return;
        }
        if let Some(icon) = params.icon() {
            if ctx.registry.interface_index(icon) != Some(InterfaceIndex::WlSurface) {
                report_data_device_error(ctx, object_id, DataDeviceError::UnknownSurface);
                return;
            }
            if let Err(error) = self
                .surface_manager
                .assign_dnd_icon_role(ctx.client_id, icon)
            {
                let mapped = match error {
                    SurfaceError::RoleAlreadyAssigned => DataDeviceError::RoleConflict,
                    SurfaceError::UnknownSurface => DataDeviceError::UnknownSurface,
                    _ => DataDeviceError::RoleConflict,
                };
                report_data_device_error(ctx, object_id, mapped);
                return;
            }
        }

        let target = self
            .seat_manager
            .pointer_focus_for_client(ctx.client_id)
            .or_else(|| self.surface_manager.pointer_target(ctx.client_id, 0.0, 0.0));
        let (px, py) = self.seat_manager.pointer_position();

        if let Err(error) = self.data_device_manager.start_drag(
            ctx.client_id,
            object_id,
            params.source(),
            params.origin(),
            params.icon(),
            params.serial(),
            target,
            px as f32,
            py as f32,
            ctx.registry,
            ctx.writer,
        ) {
            if let Some(icon) = params.icon() {
                let _ = self
                    .surface_manager
                    .clear_dnd_icon_role(ctx.client_id, icon);
            }
            report_data_device_error(ctx, object_id, error);
        }
    }

    fn set_selection(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlDataDeviceSetSelection<'_>,
    ) {
        if let Some(source) = params.source() {
            if ctx.registry.interface_index(source) != Some(InterfaceIndex::WlDataSource) {
                report_data_device_error(ctx, object_id, DataDeviceError::UnknownSource);
                return;
            }
        }
        if let Err(error) = self.data_device_manager.set_selection(
            ctx.client_id,
            object_id,
            params.source(),
            params.serial(),
            ctx.registry,
            ctx.writer,
        ) {
            report_data_device_error(ctx, object_id, error);
        }
    }

    fn release(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlDataDeviceRelease<'_>) {
        if let Some((client_id, icon)) = self.data_device_manager.active_drag_icon()
            && client_id == ctx.client_id
        {
            let _ = self.surface_manager.clear_dnd_icon_role(client_id, icon);
        }
        if let Err(error) = self
            .data_device_manager
            .release_device(ctx.client_id, object_id)
        {
            report_data_device_error(ctx, object_id, error);
            return;
        }
        ctx.registry.free_object(object_id, ctx.writer);
    }
}

impl WlDataDeviceManager for DisplayState {
    fn create_data_source(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlDataDeviceManagerCreateDataSource<'_>,
    ) {
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map_or(1, |object| object.version.min(WL_DATA_SOURCE_VERSION));
        if !register_object(ctx, params.id(), InterfaceIndex::WlDataSource, version) {
            return;
        }
        self.data_device_manager
            .create_data_source(ctx.client_id, *params.id(), version);
    }

    fn get_data_device(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlDataDeviceManagerGetDataDevice<'_>,
    ) {
        if ctx.registry.interface_index(params.seat()) != Some(InterfaceIndex::WlSeat) {
            report_data_device_error(ctx, object_id, DataDeviceError::UnknownSeat);
            return;
        }
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map_or(1, |object| object.version.min(WL_DATA_DEVICE_VERSION));
        if !register_object(ctx, params.id(), InterfaceIndex::WlDataDevice, version) {
            return;
        }
        self.data_device_manager.create_data_device(
            ctx.client_id,
            *params.id(),
            params.seat(),
            version,
            ctx.registry,
            ctx.writer,
        );
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
    fn pong(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlShellSurfacePong<'_>) {
        match self
            .surface_manager
            .acknowledge_shell_ping(ctx.client_id, object_id, params.serial())
        {
            Ok(true) => {}
            Ok(false) => {
                debug!(
                    "Ignoring wl_shell_surface.pong with unknown serial {}",
                    params.serial()
                );
            }
            Err(error) => report_surface_error(ctx, object_id, error),
        }
    }

    fn move_(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlShellSurfaceMove<'_>) {
        if ctx.registry.interface_index(params.seat()) != Some(InterfaceIndex::WlSeat) {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(params.seat())
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("Seat object is not a wl_seat");
            return;
        }
        if let Err(error) = self.surface_manager.record_shell_move(
            ctx.client_id,
            object_id,
            params.seat(),
            params.serial(),
        ) {
            report_surface_error(ctx, object_id, error);
        }
    }

    fn resize(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlShellSurfaceResize<'_>) {
        if ctx.registry.interface_index(params.seat()) != Some(InterfaceIndex::WlSeat) {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(params.seat())
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("Seat object is not a wl_seat");
            return;
        }
        if let Err(error) = self.surface_manager.record_shell_resize(
            ctx.client_id,
            object_id,
            params.seat(),
            params.serial(),
            params.edges(),
        ) {
            report_surface_error(ctx, object_id, error);
        }
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
            Ok(destroyed) => {
                self.seat_manager
                    .leave_keyboards_on_surface(ctx.client_id, object_id, ctx.writer);
                self.seat_manager
                    .leave_pointers_on_surface(ctx.client_id, object_id, ctx.writer);
                for callback in destroyed.callbacks {
                    ctx.registry.free_object(callback, ctx.writer);
                }
                self.discard_presentation_feedbacks_for_surface(
                    ctx.client_id,
                    object_id,
                    destroyed.presentation_feedbacks,
                    ctx.writer,
                    ctx.registry,
                );
                if let Some(shell_id) = destroyed.shell_id {
                    ctx.registry.free_object(shell_id, ctx.writer);
                }
                if let Some(xdg_surface_id) = destroyed.xdg_surface_id {
                    let _ = self
                        .xdg_manager
                        .destroy_xdg_surface(ctx.client_id, xdg_surface_id);
                    ctx.registry.free_object(xdg_surface_id, ctx.writer);
                }
                if let Some(subsurface_id) = destroyed.subsurface_id {
                    ctx.registry.free_object(subsurface_id, ctx.writer);
                }
                for subsurface_id in destroyed.orphaned_subsurface_ids {
                    ctx.registry.free_object(subsurface_id, ctx.writer);
                }
                if destroyed.was_mapped {
                    for output in self.output_manager.bound_outputs_for_client(ctx.client_id) {
                        ctx.writer.wl_surface_leave(object_id).output(output);
                    }
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
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map_or(1, |object| object.version);
        if let Err(error) = self.surface_manager.attach(
            ctx.client_id,
            object_id,
            pending_buffer,
            params.x(),
            params.y(),
            version,
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
        let result = match self.surface_manager.commit(ctx.client_id, object_id) {
            Ok(result) => result,
            Err(error) => {
                report_surface_error(ctx, object_id, error);
                return;
            }
        };

        let attaching_buffer = matches!(result.primary.attached_buffer, Some(Some(_)));
        if attaching_buffer
            && let Err(error) = self
                .xdg_manager
                .check_buffer_commit(ctx.client_id, object_id, true)
        {
            crate::protocols::xdg_shell::report_commit_error(ctx, object_id, error);
            return;
        }

        crate::protocols::xdg_shell::on_xdg_surface_commit(self, ctx.client_id, object_id);
        process_surface_commit(self, ctx, result.primary);
        for child in result.synchronized_children {
            process_surface_commit(self, ctx, child);
        }
    }

    fn set_buffer_transform(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlSurfaceSetBufferTransform<'_>,
    ) {
        if let Err(error) =
            self.surface_manager
                .set_buffer_transform(ctx.client_id, object_id, params.transform())
        {
            report_surface_error(ctx, object_id, error);
        }
    }

    fn set_buffer_scale(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlSurfaceSetBufferScale<'_>,
    ) {
        if let Err(error) =
            self.surface_manager
                .set_buffer_scale(ctx.client_id, object_id, params.scale())
        {
            report_surface_error(ctx, object_id, error);
        }
    }

    fn damage_buffer(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlSurfaceDamageBuffer<'_>,
    ) {
        let rectangle = Rectangle {
            x: params.x(),
            y: params.y(),
            width: params.width(),
            height: params.height(),
        };
        if let Err(error) = self
            .surface_manager
            .damage_buffer(ctx.client_id, object_id, rectangle)
        {
            report_surface_error(ctx, object_id, error);
        }
    }

    fn offset(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlSurfaceOffset) {
        if let Err(error) =
            self.surface_manager
                .offset(ctx.client_id, object_id, params.x(), params.y())
        {
            report_surface_error(ctx, object_id, error);
        }
    }
}

impl WlSeat for DisplayState {
    fn get_pointer(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlSeatGetPointer<'_>) {
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map_or(1, |object| object.version.min(WL_POINTER_VERSION));
        if !register_object(ctx, params.id(), InterfaceIndex::WlPointer, version) {
            return;
        }
        // Do not send enter here: per-client geometry can match while another
        // client's surface is on top, and we cannot leave that client from this
        // request. App refreshes seat pointer focus after dispatch / map.
        self.seat_manager.create_pointer(
            ctx.client_id,
            *params.id(),
            version,
            ctx.writer,
            None,
            &self.surface_manager,
        );
    }

    fn get_keyboard(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlSeatGetKeyboard<'_>) {
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map_or(1, |object| object.version.min(WL_KEYBOARD_VERSION));
        if !register_object(ctx, params.id(), InterfaceIndex::WlKeyboard, version) {
            return;
        }
        let focus = match self.seat_manager.focused_keyboard_surface() {
            // Inherit existing seat focus for this client (e.g. second wl_keyboard).
            Some((focused_client, surface)) if focused_client == ctx.client_id => Some(surface),
            // Another client already owns keyboard focus — do not steal it.
            Some(_) => None,
            // No seat focus yet: focus this client's first surface if any.
            None => self.surface_manager.first_surface(ctx.client_id),
        };
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

    fn get_touch(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlSeatGetTouch<'_>) {
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map_or(1, |object| object.version.min(WL_TOUCH_VERSION));
        if !register_object(ctx, params.id(), InterfaceIndex::WlTouch, version) {
            return;
        }
        self.seat_manager
            .create_touch(ctx.client_id, *params.id(), version);
    }

    fn release(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlSeatRelease<'_>) {
        ctx.registry.free_object(object_id, ctx.writer);
    }
}

impl WlPointer for DisplayState {
    fn set_cursor(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &WlPointerSetCursor<'_>) {
        if let Some(surface) = params.surface() {
            if ctx.registry.interface_index(surface) != Some(InterfaceIndex::WlSurface) {
                report_surface_error(ctx, surface, SurfaceError::UnknownSurface);
                return;
            }
        }
        if let Err(error) = self.seat_manager.set_cursor(
            ctx.client_id,
            object_id,
            params.serial(),
            params.surface(),
            params.hotspot_x(),
            params.hotspot_y(),
            &mut self.surface_manager,
        ) {
            report_surface_error(ctx, object_id, error);
        }
    }

    fn release(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlPointerRelease<'_>) {
        self.seat_manager
            .destroy_pointer(ctx.client_id, object_id, &mut self.surface_manager);
        ctx.registry.free_object(object_id, ctx.writer);
    }
}

impl WlKeyboard for DisplayState {
    fn release(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlKeyboardRelease<'_>) {
        self.seat_manager.destroy_keyboard(ctx.client_id, object_id);
        ctx.registry.free_object(object_id, ctx.writer);
    }
}

impl WlTouch for DisplayState {
    fn release(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlTouchRelease<'_>) {
        self.seat_manager.destroy_touch(ctx.client_id, object_id);
        ctx.registry.free_object(object_id, ctx.writer);
    }
}

impl WlOutput for DisplayState {
    fn release(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlOutputRelease<'_>) {
        self.output_manager.release(ctx.client_id, object_id);
        ctx.registry.free_object(object_id, ctx.writer);
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
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &WlSubcompositorDestroy<'_>,
    ) {
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn get_subsurface(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlSubcompositorGetSubsurface<'_>,
    ) {
        if ctx.registry.interface_index(params.surface()) != Some(InterfaceIndex::WlSurface) {
            report_subcompositor_error(ctx, object_id, SurfaceError::BadSurface);
            return;
        }
        if ctx.registry.interface_index(params.parent()) != Some(InterfaceIndex::WlSurface) {
            report_subcompositor_error(ctx, object_id, SurfaceError::BadParent);
            return;
        }
        if !register_object(ctx, params.id(), InterfaceIndex::WlSubsurface, 1) {
            return;
        }
        if let Err(error) = self.surface_manager.create_subsurface(
            ctx.client_id,
            *params.id(),
            params.surface(),
            params.parent(),
        ) {
            report_subcompositor_error(ctx, object_id, error);
        }
    }
}

impl WlSubsurface for DisplayState {
    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlSubsurfaceDestroy<'_>) {
        match self
            .surface_manager
            .destroy_subsurface(ctx.client_id, object_id)
        {
            Ok((surface_id, was_mapped)) => {
                if was_mapped {
                    self.seat_manager.leave_keyboards_on_surface(
                        ctx.client_id,
                        surface_id,
                        ctx.writer,
                    );
                    self.seat_manager.leave_pointers_on_surface(
                        ctx.client_id,
                        surface_id,
                        ctx.writer,
                    );
                    self.surface_updates.push_back(SurfaceUpdate::Unmapped {
                        client_id: ctx.client_id,
                        surface_id,
                    });
                }
                ctx.registry.free_object(object_id, ctx.writer);
            }
            Err(error) => report_surface_error(ctx, object_id, error),
        }
    }

    fn set_position(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlSubsurfaceSetPosition<'_>,
    ) {
        if let Err(error) =
            self.surface_manager
                .set_position(ctx.client_id, object_id, params.x(), params.y())
        {
            report_surface_error(ctx, object_id, error);
        }
    }

    fn place_above(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlSubsurfacePlaceAbove<'_>,
    ) {
        if ctx.registry.interface_index(params.sibling()) != Some(InterfaceIndex::WlSurface) {
            report_subsurface_error(ctx, object_id, SurfaceError::BadSurface);
            return;
        }
        if let Err(error) =
            self.surface_manager
                .place_above(ctx.client_id, object_id, params.sibling())
        {
            report_subsurface_error(ctx, object_id, error);
        }
    }

    fn place_below(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlSubsurfacePlaceBelow<'_>,
    ) {
        if ctx.registry.interface_index(params.sibling()) != Some(InterfaceIndex::WlSurface) {
            report_subsurface_error(ctx, object_id, SurfaceError::BadSurface);
            return;
        }
        if let Err(error) =
            self.surface_manager
                .place_below(ctx.client_id, object_id, params.sibling())
        {
            report_subsurface_error(ctx, object_id, error);
        }
    }

    fn set_sync(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlSubsurfaceSetSync<'_>) {
        if let Err(error) = self.surface_manager.set_sync(ctx.client_id, object_id) {
            report_surface_error(ctx, object_id, error);
        }
    }

    fn set_desync(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &WlSubsurfaceSetDesync<'_>,
    ) {
        if let Err(error) = self.surface_manager.set_desync(ctx.client_id, object_id) {
            report_surface_error(ctx, object_id, error);
        }
    }
}

impl WlFixes for DisplayState {
    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &WlFixesDestroy) {
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn destroy_registry(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &WlFixesDestroyRegistry,
    ) {
        let registry_id = params.registry();
        if ctx.registry.interface_index(registry_id) != Some(InterfaceIndex::WlRegistry) {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("destroy_registry target is not a wl_registry");
            return;
        }
        ctx.registry.free_object(registry_id, ctx.writer);
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
    use crate::OutputInfo;

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
    fn wl_fixes_can_destroy_registry() {
        let (_receiver, sender) = UnixStream::pair().unwrap();
        let mut state = display_state();
        let mut registry = Registry::new();
        registry
            .register_client_object_with_version(
                NewObjectId::new(object_id(2)),
                InterfaceIndex::WlRegistry,
                1,
            )
            .unwrap();
        registry
            .register_client_object_with_version(
                NewObjectId::new(object_id(3)),
                InterfaceIndex::WlFixes,
                1,
            )
            .unwrap();
        let mut writer = Writer::new(sender.as_raw_fd());
        let mut ctx = Ctx {
            registry: &mut registry,
            writer: &mut writer,
            client_id: ClientId::new(NonZeroU32::new(1).unwrap()),
        };
        let mut fds = VecDeque::new();
        let registry_arg = 2u32.to_ne_bytes();
        let params = WlFixesDestroyRegistry::new(&registry_arg, &mut fds);
        WlFixes::destroy_registry(&mut state, &mut ctx, object_id(3), &params);
        assert!(ctx.registry.object_metadata(object_id(2)).is_none());

        let params = WlFixesDestroy::new(&[], &mut fds);
        WlFixes::destroy(&mut state, &mut ctx, object_id(3), &params);
        assert!(ctx.registry.object_metadata(object_id(3)).is_none());
    }

    #[test]
    fn advertises_only_the_minimal_implemented_globals() {
        let state = display_state();
        let globals: Vec<_> = state
            .globals
            .iter()
            .map(|(_, global)| (global.name, global.version))
            .collect();

        assert!(globals.contains(&(WL_COMPOSITOR_NAME, 5)));
        assert!(globals.contains(&(WL_SHM_NAME, 2)));
        assert!(globals.contains(&(WL_SHELL_NAME, 1)));
        assert!(globals.contains(&(WL_SUBCOMPOSITOR_NAME, 1)));
        assert!(globals.contains(&(WL_FIXES_NAME, 1)));
        assert!(globals.contains(&(WL_DATA_DEVICE_MANAGER_NAME, 3)));
        assert!(
            !globals.iter().any(|(name, _)| *name == WL_OUTPUT_NAME),
            "wl_output globals are config-owned and must not be advertised by default"
        );
        assert!(globals.contains(&(
            lumalla_wayland_protocol::protocols::xdg_shell::XDG_WM_BASE_NAME,
            1
        )));
        assert!(globals.contains(&(
            lumalla_wayland_protocol::protocols::linux_dmabuf::ZWP_LINUX_DMABUF_V1_NAME,
            4
        )));
        assert!(globals.contains(&(
            lumalla_wayland_protocol::protocols::presentation_time::WP_PRESENTATION_NAME,
            2
        )));
    }

    #[test]
    fn add_output_advertises_wl_output_global() {
        let mut state = display_state();
        state
            .add_output(OutputInfo::default(), [].into_iter())
            .unwrap();
        let globals: Vec<_> = state
            .globals
            .iter()
            .map(|(_, global)| (global.name, global.version))
            .collect();
        assert!(globals.contains(&(WL_OUTPUT_NAME, 4)));
    }

    #[test]
    fn add_and_remove_outputs_update_display_state() {
        let mut state = display_state();
        state
            .add_output(
                OutputInfo {
                    name: "HDMI-A-1".to_owned(),
                    description: "Main".to_owned(),
                    is_virtual: false,
                    width: 1920,
                    height: 1080,
                    ..OutputInfo::default()
                },
                [].into_iter(),
            )
            .unwrap();
        state
            .add_output(
                OutputInfo {
                    name: "VIRTUAL-1".to_owned(),
                    is_virtual: true,
                    ..OutputInfo::default()
                },
                [].into_iter(),
            )
            .unwrap();
        assert_eq!(state.outputs().count(), 2);
        assert!(
            state
                .add_output(
                    OutputInfo {
                        name: "HDMI-A-1".to_owned(),
                        ..OutputInfo::default()
                    },
                    [].into_iter(),
                )
                .is_err()
        );
        state.remove_output("VIRTUAL-1", [].into_iter()).unwrap();
        let names: Vec<_> = state.outputs().map(|output| output.name.as_str()).collect();
        assert_eq!(names, ["HDMI-A-1"]);
        assert!(state.remove_output("VIRTUAL-1", [].into_iter()).is_err());
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
        let data = bind_data(1, "wl_compositor", 5, 2);
        let params = WlRegistryBind::new(&data, &mut fds);

        WlRegistry::bind(&mut state, &mut ctx, object_id(10), &params);

        let metadata = ctx.registry.object_metadata(object_id(2)).unwrap();
        assert_eq!(metadata.interface_index, InterfaceIndex::WlCompositor);
        assert_eq!(metadata.version, 5);
    }

    #[test]
    fn registry_bind_wp_presentation_registers_object() {
        let (_receiver, sender) = UnixStream::pair().unwrap();
        let mut state = display_state();
        let mut registry = Registry::new();
        let mut writer = Writer::new(sender.as_raw_fd());
        let mut ctx = Ctx {
            registry: &mut registry,
            writer: &mut writer,
            client_id: ClientId::new(NonZeroU32::new(1).unwrap()),
        };
        let global_name = state
            .globals
            .iter()
            .find(|(_, global)| global.interface_index == InterfaceIndex::WpPresentation)
            .map(|(id, _)| *id)
            .expect("wp_presentation global");
        let data = bind_data(global_name, "wp_presentation", 2, 20);
        let mut fds = VecDeque::new();
        let params = WlRegistryBind::new(&data, &mut fds);
        WlRegistry::bind(&mut state, &mut ctx, object_id(10), &params);
        assert_eq!(
            ctx.registry.interface_index(object_id(20)),
            Some(InterfaceIndex::WpPresentation)
        );
        assert!(ctx.writer.has_pending_output());
    }

    #[test]
    fn registry_bind_wp_viewporter_registers_object() {
        let (_receiver, sender) = UnixStream::pair().unwrap();
        let mut state = display_state();
        let mut registry = Registry::new();
        let mut writer = Writer::new(sender.as_raw_fd());
        let mut ctx = Ctx {
            registry: &mut registry,
            writer: &mut writer,
            client_id: ClientId::new(NonZeroU32::new(1).unwrap()),
        };
        let global_name = state
            .globals
            .iter()
            .find(|(_, global)| global.interface_index == InterfaceIndex::WpViewporter)
            .map(|(id, _)| *id)
            .expect("wp_viewporter global");
        let data = bind_data(global_name, "wp_viewporter", 1, 20);
        let mut fds = VecDeque::new();
        let params = WlRegistryBind::new(&data, &mut fds);
        WlRegistry::bind(&mut state, &mut ctx, object_id(10), &params);
        assert_eq!(
            ctx.registry.interface_index(object_id(20)),
            Some(InterfaceIndex::WpViewporter)
        );
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
            .attach(client_id, surface_id, Some(buffer_id), 0, 0, 1)
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

        assert!(
            ctx.registry.object_metadata(callback_id).is_some(),
            "frame callback must remain until present"
        );
        assert_eq!(state.pending_frame_callback_count(), 1);
        let updates: Vec<_> = state.take_surface_updates().collect();
        assert_eq!(updates.len(), 1);
        let SurfaceUpdate::Frame(frame) = &updates[0] else {
            panic!("expected a committed frame");
        };
        assert_eq!(frame.surface_id, surface_id);
        assert_eq!(frame.buffer_id, buffer_id);
        assert_eq!(frame.pixels, [1, 2, 3, 4]);
        assert_eq!(frame.buffer_scale, 1);
        assert_eq!(frame.buffer_transform, 0);
        assert_eq!((frame.offset_x, frame.offset_y), (0, 0));
        assert!(
            state
                .surface_manager
                .acknowledge_shell_ping(client_id, shell_id, 1)
                .unwrap(),
            "expected pending ping serial 1 after first map"
        );

        // Present-time completion: drain queued callbacks onto the same writer/registry.
        while let Some((owner, callback)) = state.pending_frame_callbacks.pop_front() {
            assert_eq!(owner, client_id);
            ctx.writer.wl_callback_done(callback).callback_data(16);
            ctx.registry.free_object(callback, ctx.writer);
        }
        assert!(ctx.registry.object_metadata(callback_id).is_none());

        state
            .surface_manager
            .attach(client_id, surface_id, None, 0, 0, 1)
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
    fn presentation_feedback_queues_discards_on_supersede() {
        use lumalla_wayland_protocol::protocols::presentation_time::{
            WP_PRESENTATION_FEEDBACK_KIND_HW_CLOCK, WP_PRESENTATION_FEEDBACK_KIND_HW_COMPLETION,
            WP_PRESENTATION_FEEDBACK_KIND_VSYNC,
        };

        let (_receiver, sender) = UnixStream::pair().unwrap();
        let mut state = display_state();
        let client_id = ClientId::new(NonZeroU32::new(1).unwrap());
        let surface_id = object_id(2);
        let feedback_a = object_id(7);
        let feedback_b = object_id(8);
        state.surface_manager.create_surface(client_id, surface_id);

        let mut registry = Registry::new();
        for (id, interface) in [
            (surface_id, InterfaceIndex::WlSurface),
            (feedback_a, InterfaceIndex::WpPresentationFeedback),
            (feedback_b, InterfaceIndex::WpPresentationFeedback),
        ] {
            registry
                .register_client_object_with_version(NewObjectId::new(id), interface, 2)
                .unwrap();
        }
        let mut writer = Writer::new(sender.as_raw_fd());
        let mut ctx = Ctx {
            registry: &mut registry,
            writer: &mut writer,
            client_id,
        };
        let mut fds = VecDeque::new();
        let params = WlSurfaceCommit::new(&[], &mut fds);

        state
            .surface_manager
            .add_presentation_feedback(client_id, surface_id, feedback_a)
            .unwrap();
        WlSurface::commit(&mut state, &mut ctx, surface_id, &params);
        assert_eq!(state.pending_presentation_feedback_count(), 1);

        state
            .surface_manager
            .add_presentation_feedback(client_id, surface_id, feedback_b)
            .unwrap();
        WlSurface::commit(&mut state, &mut ctx, surface_id, &params);
        assert_eq!(
            state.pending_presentation_feedback_count(),
            1,
            "superseded feedback must be discarded"
        );
        assert!(ctx.registry.object_metadata(feedback_a).is_none());
        assert!(ctx.registry.object_metadata(feedback_b).is_some());

        let flags = WP_PRESENTATION_FEEDBACK_KIND_VSYNC
            | WP_PRESENTATION_FEEDBACK_KIND_HW_CLOCK
            | WP_PRESENTATION_FEEDBACK_KIND_HW_COMPLETION;
        while let Some(pending) = state.pending_presentation_feedbacks.pop_front() {
            assert_eq!(pending.feedback_id, feedback_b);
            ctx.writer
                .wp_presentation_feedback_presented(pending.feedback_id)
                .tv_sec_hi(0)
                .tv_sec_lo(10)
                .tv_nsec(500_000)
                .refresh(16_666_666)
                .seq_hi(0)
                .seq_lo(99)
                .flags(flags);
            ctx.registry.free_object(pending.feedback_id, ctx.writer);
        }
        assert!(ctx.registry.object_metadata(feedback_b).is_none());
        assert_eq!(state.pending_presentation_feedback_count(), 0);
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

    #[test]
    fn wire_client_can_commit_an_xdg_shm_toplevel() {
        use lumalla_wayland_protocol::protocols::xdg_shell::{
            XDG_SURFACE_ACK_CONFIGURE_OPCODE, XDG_SURFACE_GET_TOPLEVEL_OPCODE,
            XDG_WM_BASE_GET_XDG_SURFACE_OPCODE, XDG_WM_BASE_NAME,
        };

        static NEXT_SOCKET: AtomicU64 = AtomicU64::new(100);
        let socket_path = std::env::temp_dir().join(format!(
            "lumalla-xdg-test-{}-{}",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
        ));
        let mut wayland =
            lumalla_wayland_protocol::Wayland::new(socket_path.to_string_lossy().into_owned())
                .unwrap();
        let mut client_stream = UnixStream::connect(&socket_path).unwrap();
        let mut client = wayland.next_client().unwrap();
        let mut state = display_state();

        // Discover xdg_wm_base global name from state.
        let xdg_global = state
            .globals
            .iter()
            .find(|(_, g)| g.name == XDG_WM_BASE_NAME)
            .map(|(id, _)| *id)
            .expect("xdg_wm_base global");

        let mut wire = Vec::new();
        wire.extend(wire_message(1, WL_DISPLAY_GET_REGISTRY_OPCODE, &[2]));
        wire.extend(wire_bind(1, "wl_compositor", 3));
        wire.extend(wire_bind(2, "wl_shm", 4));
        // Bind xdg_wm_base as object 5
        {
            let mut data = bind_data(xdg_global, XDG_WM_BASE_NAME, 1, 5);
            let size = 8 + data.len();
            wire.extend_from_slice(&2u32.to_ne_bytes());
            wire.extend_from_slice(&WL_REGISTRY_BIND_OPCODE.to_ne_bytes());
            wire.extend_from_slice(&(size as u16).to_ne_bytes());
            wire.append(&mut data);
        }
        wire.extend(wire_message(3, WL_COMPOSITOR_CREATE_SURFACE_OPCODE, &[6]));
        wire.extend(wire_message(4, WL_SHM_CREATE_POOL_OPCODE, &[7, 4]));
        wire.extend(wire_message(
            7,
            WL_SHM_POOL_CREATE_BUFFER_OPCODE,
            &[8, 0, 1, 1, 4, WL_SHM_FORMAT_XRGB8888],
        ));
        wire.extend(wire_message(5, XDG_WM_BASE_GET_XDG_SURFACE_OPCODE, &[9, 6]));
        wire.extend(wire_message(9, XDG_SURFACE_GET_TOPLEVEL_OPCODE, &[10]));

        let fd = memory_file(&[1, 2, 3, 0xff]);
        send_wire_with_fd(&client_stream, &wire, fd);
        unsafe {
            libc::close(fd);
        }
        client.handle_messages(&mut state).unwrap();
        assert!(
            state.take_surface_updates().next().is_none(),
            "must not map before ack_configure"
        );

        let mut wire = Vec::new();
        wire.extend(wire_message(9, XDG_SURFACE_ACK_CONFIGURE_OPCODE, &[1]));
        wire.extend(wire_message(6, WL_SURFACE_ATTACH_OPCODE, &[8, 0, 0]));
        wire.extend(wire_message(6, WL_SURFACE_DAMAGE_OPCODE, &[0, 0, 1, 1]));
        wire.extend(wire_message(6, WL_SURFACE_COMMIT_OPCODE, &[]));
        client_stream.write_all(&wire).unwrap();
        client.handle_messages(&mut state).unwrap();

        let updates: Vec<_> = state.take_surface_updates().collect();
        let [SurfaceUpdate::Frame(frame)] = updates.as_slice() else {
            panic!("expected one committed xdg frame, got {updates:?}");
        };
        assert_eq!(frame.surface_id, object_id(6));
        assert_eq!(frame.pixels, [1, 2, 3, 0xff]);
    }

    #[test]
    fn wire_client_can_commit_an_xdg_dmabuf_toplevel() {
        use crate::dmabuf::DRM_FORMAT_XRGB8888;
        use lumalla_wayland_protocol::protocols::{
            linux_dmabuf::{
                ZWP_LINUX_BUFFER_PARAMS_V1_ADD_OPCODE,
                ZWP_LINUX_BUFFER_PARAMS_V1_CREATE_IMMED_OPCODE,
                ZWP_LINUX_DMABUF_V1_CREATE_PARAMS_OPCODE, ZWP_LINUX_DMABUF_V1_NAME,
            },
            xdg_shell::{
                XDG_SURFACE_ACK_CONFIGURE_OPCODE, XDG_SURFACE_GET_TOPLEVEL_OPCODE,
                XDG_WM_BASE_GET_XDG_SURFACE_OPCODE, XDG_WM_BASE_NAME,
            },
        };

        static NEXT_SOCKET: AtomicU64 = AtomicU64::new(200);
        let socket_path = std::env::temp_dir().join(format!(
            "lumalla-dmabuf-test-{}-{}",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
        ));
        let mut wayland =
            lumalla_wayland_protocol::Wayland::new(socket_path.to_string_lossy().into_owned())
                .unwrap();
        let mut client_stream = UnixStream::connect(&socket_path).unwrap();
        let mut client = wayland.next_client().unwrap();
        let mut state = display_state();

        let xdg_global = state
            .globals
            .iter()
            .find(|(_, g)| g.name == XDG_WM_BASE_NAME)
            .map(|(id, _)| *id)
            .expect("xdg_wm_base global");
        let dmabuf_global = state
            .globals
            .iter()
            .find(|(_, g)| g.name == ZWP_LINUX_DMABUF_V1_NAME)
            .map(|(id, _)| *id)
            .expect("zwp_linux_dmabuf_v1 global");

        let mut wire = Vec::new();
        wire.extend(wire_message(1, WL_DISPLAY_GET_REGISTRY_OPCODE, &[2]));
        wire.extend(wire_bind(1, "wl_compositor", 3));
        {
            let mut data = bind_data(xdg_global, XDG_WM_BASE_NAME, 1, 4);
            let size = 8 + data.len();
            wire.extend_from_slice(&2u32.to_ne_bytes());
            wire.extend_from_slice(&WL_REGISTRY_BIND_OPCODE.to_ne_bytes());
            wire.extend_from_slice(&(size as u16).to_ne_bytes());
            wire.append(&mut data);
        }
        {
            let mut data = bind_data(dmabuf_global, ZWP_LINUX_DMABUF_V1_NAME, 3, 5);
            let size = 8 + data.len();
            wire.extend_from_slice(&2u32.to_ne_bytes());
            wire.extend_from_slice(&WL_REGISTRY_BIND_OPCODE.to_ne_bytes());
            wire.extend_from_slice(&(size as u16).to_ne_bytes());
            wire.append(&mut data);
        }
        wire.extend(wire_message(3, WL_COMPOSITOR_CREATE_SURFACE_OPCODE, &[6]));
        wire.extend(wire_message(4, XDG_WM_BASE_GET_XDG_SURFACE_OPCODE, &[7, 6]));
        wire.extend(wire_message(7, XDG_SURFACE_GET_TOPLEVEL_OPCODE, &[8]));
        client_stream.write_all(&wire).unwrap();
        client.handle_messages(&mut state).unwrap();

        let mut wire = Vec::new();
        wire.extend(wire_message(7, XDG_SURFACE_ACK_CONFIGURE_OPCODE, &[1]));
        wire.extend(wire_message(
            5,
            ZWP_LINUX_DMABUF_V1_CREATE_PARAMS_OPCODE,
            &[9],
        ));
        // add: plane_idx, offset, stride, modifier_hi, modifier_lo (+ fd)
        wire.extend(wire_message(
            9,
            ZWP_LINUX_BUFFER_PARAMS_V1_ADD_OPCODE,
            &[0, 0, 4, 0, 0],
        ));
        wire.extend(wire_message(
            9,
            ZWP_LINUX_BUFFER_PARAMS_V1_CREATE_IMMED_OPCODE,
            &[10, 1, 1, DRM_FORMAT_XRGB8888, 0],
        ));
        wire.extend(wire_message(6, WL_SURFACE_ATTACH_OPCODE, &[10, 0, 0]));
        wire.extend(wire_message(6, WL_SURFACE_DAMAGE_OPCODE, &[0, 0, 1, 1]));
        wire.extend(wire_message(6, WL_SURFACE_COMMIT_OPCODE, &[]));

        let fd = memory_file(&[0xaa, 0xbb, 0xcc, 0xff]);
        send_wire_with_fd(&client_stream, &wire, fd);
        unsafe {
            libc::close(fd);
        }
        client.handle_messages(&mut state).unwrap();

        let updates: Vec<_> = state.take_surface_updates().collect();
        let [SurfaceUpdate::Frame(frame)] = updates.as_slice() else {
            panic!("expected one committed dmabuf frame, got {updates:?}");
        };
        assert_eq!(frame.surface_id, object_id(6));
        assert_eq!(frame.buffer_id, object_id(10));
        assert!(frame.pixels.is_empty());
        let exported = frame.dmabuf.as_ref().expect("dmabuf export");
        assert_eq!(exported.width, 1);
        assert_eq!(exported.height, 1);
        assert_eq!(exported.stride, 4);
        assert_eq!(exported.drm_fourcc, DRM_FORMAT_XRGB8888);
        assert_eq!(frame.format, WL_SHM_FORMAT_XRGB8888);
        let snap = state
            .dmabuf_manager
            .snapshot_buffer(frame.client_id, frame.buffer_id)
            .unwrap();
        assert_eq!(snap.pixels, [0xaa, 0xbb, 0xcc, 0xff]);
    }
}
