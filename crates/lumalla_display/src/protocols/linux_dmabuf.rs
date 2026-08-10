use log::debug;
use lumalla_wayland_protocol::{
    Ctx, NewObjectId, ObjectId,
    protocols::{
        LinuxDmabufV1Protocol,
        linux_dmabuf::*,
        wayland::WL_DISPLAY_ERROR_INVALID_OBJECT,
    },
    registry::{DISPLAY_OBJECT_ID, InterfaceIndex},
};

use crate::{
    DisplayState,
    dmabuf::{DmabufError, DmabufErrorKind},
};

impl LinuxDmabufV1Protocol for DisplayState {}

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

fn report_params_error(ctx: &mut Ctx, object_id: ObjectId, error: &DmabufError) {
    let code = match error.kind() {
        DmabufErrorKind::AlreadyUsed => ZWP_LINUX_BUFFER_PARAMS_V1_ERROR_ALREADY_USED,
        DmabufErrorKind::PlaneIdx => ZWP_LINUX_BUFFER_PARAMS_V1_ERROR_PLANE_IDX,
        DmabufErrorKind::PlaneSet => ZWP_LINUX_BUFFER_PARAMS_V1_ERROR_PLANE_SET,
        DmabufErrorKind::Incomplete => ZWP_LINUX_BUFFER_PARAMS_V1_ERROR_INCOMPLETE,
        DmabufErrorKind::InvalidFormat => ZWP_LINUX_BUFFER_PARAMS_V1_ERROR_INVALID_FORMAT,
        DmabufErrorKind::InvalidDimensions => ZWP_LINUX_BUFFER_PARAMS_V1_ERROR_INVALID_DIMENSIONS,
        DmabufErrorKind::OutOfBounds => ZWP_LINUX_BUFFER_PARAMS_V1_ERROR_OUT_OF_BOUNDS,
        DmabufErrorKind::InvalidFd | DmabufErrorKind::InvalidObject => {
            ZWP_LINUX_BUFFER_PARAMS_V1_ERROR_INVALID_WL_BUFFER
        }
    };
    debug!("linux-dmabuf params error: {error}");
    let message = error.to_string();
    ctx.writer
        .wl_display_error(DISPLAY_OBJECT_ID)
        .object_id(object_id)
        .code(code)
        .message(&message);
}

pub(crate) fn send_dmabuf_formats(writer: &mut lumalla_wayland_protocol::buffer::Writer, id: ObjectId, formats: &[(u32, u64)]) {
    for &(format, modifier) in formats {
        writer
            .zwp_linux_dmabuf_v1_format(id)
            .format(format);
        let modifier_hi = (modifier >> 32) as u32;
        let modifier_lo = modifier as u32;
        writer
            .zwp_linux_dmabuf_v1_modifier(id)
            .format(format)
            .modifier_hi(modifier_hi)
            .modifier_lo(modifier_lo);
    }
}

impl ZwpLinuxDmabufV1 for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ZwpLinuxDmabufV1Destroy<'_>,
    ) {
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn create_params(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwpLinuxDmabufV1CreateParams<'_>,
    ) {
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map(|m| m.version)
            .unwrap_or(1);
        if !register_object(
            ctx,
            params.params_id(),
            InterfaceIndex::ZwpLinuxBufferParamsV1,
            version,
        ) {
            return;
        }
        if let Err(error) = self
            .dmabuf_manager
            .create_params(ctx.client_id, *params.params_id())
        {
            report_params_error(ctx, object_id, &error);
        }
    }

    fn get_default_feedback(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwpLinuxDmabufV1GetDefaultFeedback<'_>,
    ) {
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map(|m| m.version)
            .unwrap_or(1);
        if !register_object(
            ctx,
            params.id(),
            InterfaceIndex::ZwpLinuxDmabufFeedbackV1,
            version,
        ) {
            return;
        }
        // Minimal no-op feedback: done without formats (clients should use v3 events).
        ctx.writer
            .zwp_linux_dmabuf_feedback_v1_done(*params.id());
    }

    fn get_surface_feedback(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwpLinuxDmabufV1GetSurfaceFeedback<'_>,
    ) {
        if ctx.registry.interface_index(params.surface()) != Some(InterfaceIndex::WlSurface) {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("Invalid surface for dmabuf feedback");
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
            InterfaceIndex::ZwpLinuxDmabufFeedbackV1,
            version,
        ) {
            return;
        }
        ctx.writer
            .zwp_linux_dmabuf_feedback_v1_done(*params.id());
    }
}

impl ZwpLinuxBufferParamsV1 for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ZwpLinuxBufferParamsV1Destroy<'_>,
    ) {
        self.dmabuf_manager
            .destroy_params(ctx.client_id, object_id);
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn add(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwpLinuxBufferParamsV1Add<'_>,
    ) {
        if let Err(error) = self.dmabuf_manager.add_plane(
            ctx.client_id,
            object_id,
            params.fd(),
            params.plane_idx(),
            params.offset(),
            params.stride(),
            params.modifier_hi(),
            params.modifier_lo(),
        ) {
            report_params_error(ctx, object_id, &error);
        }
    }

    fn create(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwpLinuxBufferParamsV1Create<'_>,
    ) {
        let Ok(buffer_id) = ctx
            .registry
            .create_object(InterfaceIndex::WlBuffer, 1)
        else {
            ctx.writer
                .zwp_linux_buffer_params_v1_failed(object_id);
            return;
        };
        match self.dmabuf_manager.create_from_params(
            ctx.client_id,
            object_id,
            buffer_id,
            params.width(),
            params.height(),
            params.format(),
            params.flags(),
        ) {
            Ok(()) => {
                ctx.writer
                    .zwp_linux_buffer_params_v1_created(object_id)
                    .buffer(buffer_id);
            }
            Err(error) => {
                ctx.registry.free_object(buffer_id, ctx.writer);
                if matches!(
                    error.kind(),
                    DmabufErrorKind::Incomplete
                        | DmabufErrorKind::InvalidFormat
                        | DmabufErrorKind::InvalidDimensions
                        | DmabufErrorKind::OutOfBounds
                        | DmabufErrorKind::AlreadyUsed
                        | DmabufErrorKind::PlaneIdx
                        | DmabufErrorKind::PlaneSet
                ) {
                    report_params_error(ctx, object_id, &error);
                } else {
                    ctx.writer
                        .zwp_linux_buffer_params_v1_failed(object_id);
                }
            }
        }
    }

    fn create_immed(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwpLinuxBufferParamsV1CreateImmed<'_>,
    ) {
        if !register_object(ctx, params.buffer_id(), InterfaceIndex::WlBuffer, 1) {
            return;
        }
        if let Err(error) = self.dmabuf_manager.create_immed(
            ctx.client_id,
            object_id,
            *params.buffer_id(),
            params.width(),
            params.height(),
            params.format(),
            params.flags(),
        ) {
            report_params_error(ctx, object_id, &error);
        }
    }

    fn set_sampling_device(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &ZwpLinuxBufferParamsV1SetSamplingDevice<'_>,
    ) {
        // Optional since v6; ignored for the basic import path.
    }
}

impl ZwpLinuxDmabufFeedbackV1 for DisplayState {
    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ZwpLinuxDmabufFeedbackV1Destroy<'_>,
    ) {
        ctx.registry.free_object(object_id, ctx.writer);
    }
}
