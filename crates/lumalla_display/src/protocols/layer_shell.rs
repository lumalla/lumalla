use log::debug;
use lumalla_wayland_protocol::{
    Ctx, NewObjectId, ObjectId,
    protocols::{
        WlrLayerShellUnstableV1Protocol,
        wayland::WL_DISPLAY_ERROR_INVALID_OBJECT,
        wlr_layer_shell::*,
    },
    registry::{DISPLAY_OBJECT_ID, InterfaceIndex},
};

use crate::{
    DisplayState,
    layer_shell::LayerShellError,
    surface::SurfaceError,
};

impl WlrLayerShellUnstableV1Protocol for DisplayState {}

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

fn report_layer_error(ctx: &mut Ctx, object_id: ObjectId, error: LayerShellError) {
    let (code, message) = match error {
        LayerShellError::RoleConflict => {
            (ZWLR_LAYER_SHELL_V1_ERROR_ROLE, "wl_surface has another role")
        }
        LayerShellError::InvalidLayer => {
            (ZWLR_LAYER_SHELL_V1_ERROR_INVALID_LAYER, "layer value is invalid")
        }
        LayerShellError::AlreadyConstructed => (
            ZWLR_LAYER_SHELL_V1_ERROR_ALREADY_CONSTRUCTED,
            "wl_surface has a buffer attached or committed",
        ),
        LayerShellError::InvalidSurfaceState | LayerShellError::UnconfiguredBuffer => (
            ZWLR_LAYER_SURFACE_V1_ERROR_INVALID_SURFACE_STATE,
            "provided surface state is invalid",
        ),
        LayerShellError::InvalidSize => {
            (ZWLR_LAYER_SURFACE_V1_ERROR_INVALID_SIZE, "size is invalid")
        }
        LayerShellError::InvalidAnchor => (
            ZWLR_LAYER_SURFACE_V1_ERROR_INVALID_ANCHOR,
            "anchor bitfield is invalid",
        ),
        LayerShellError::InvalidKeyboardInteractivity => (
            ZWLR_LAYER_SURFACE_V1_ERROR_INVALID_KEYBOARD_INTERACTIVITY,
            "keyboard interactivity is invalid",
        ),
        LayerShellError::UnknownShell
        | LayerShellError::UnknownLayerSurface
        | LayerShellError::UnknownSurface
        | LayerShellError::InvalidSerial => {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("Invalid layer-shell object");
            return;
        }
    };
    ctx.writer
        .wl_display_error(DISPLAY_OBJECT_ID)
        .object_id(object_id)
        .code(code)
        .message(message);
}

fn report_surface_role_error(ctx: &mut Ctx, object_id: ObjectId, error: SurfaceError) {
    match error {
        SurfaceError::RoleAlreadyAssigned => {
            report_layer_error(ctx, object_id, LayerShellError::RoleConflict);
        }
        _ => {
            ctx.writer
                .wl_display_error(DISPLAY_OBJECT_ID)
                .object_id(object_id)
                .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                .message("Invalid surface for layer-shell");
        }
    }
}

impl ZwlrLayerShellV1 for DisplayState {
    fn get_layer_surface(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwlrLayerShellV1GetLayerSurface<'_>,
    ) {
        if ctx.registry.interface_index(params.surface()) != Some(InterfaceIndex::WlSurface) {
            report_layer_error(ctx, params.surface(), LayerShellError::UnknownSurface);
            return;
        }
        if self
            .surface_manager
            .has_attached_or_committed_buffer(ctx.client_id, params.surface())
            .unwrap_or(false)
        {
            report_layer_error(ctx, object_id, LayerShellError::AlreadyConstructed);
            return;
        }

        let output_global = match params.output() {
            Some(output) => {
                if ctx.registry.interface_index(output) != Some(InterfaceIndex::WlOutput) {
                    ctx.writer
                        .wl_display_error(DISPLAY_OBJECT_ID)
                        .object_id(object_id)
                        .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                        .message("output is not a wl_output");
                    return;
                }
                match self
                    .output_manager
                    .global_for_binding(ctx.client_id, output)
                {
                    Some(global) => global,
                    None => {
                        ctx.writer
                            .wl_display_error(DISPLAY_OBJECT_ID)
                            .object_id(object_id)
                            .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                            .message("unknown wl_output");
                        return;
                    }
                }
            }
            None => match self.output_manager.primary_global_id() {
                Some(global) => global,
                None => {
                    ctx.writer
                        .wl_display_error(DISPLAY_OBJECT_ID)
                        .object_id(object_id)
                        .code(WL_DISPLAY_ERROR_INVALID_OBJECT)
                        .message("no output available for layer surface");
                    return;
                }
            },
        };

        let version = ctx
            .registry
            .object_metadata(object_id)
            .map_or(1, |object| object.version.min(ZWLR_LAYER_SURFACE_V1_VERSION));
        if !register_object(
            ctx,
            params.id(),
            InterfaceIndex::ZwlrLayerSurfaceV1,
            version,
        ) {
            return;
        }

        if let Err(error) =
            self.surface_manager
                .assign_layer_role(ctx.client_id, params.surface(), *params.id())
        {
            ctx.registry.free_object(*params.id(), ctx.writer);
            report_surface_role_error(ctx, object_id, error);
            return;
        }

        if let Err(error) = self.layer_shell_manager.create_layer_surface(
            ctx.client_id,
            object_id,
            *params.id(),
            params.surface(),
            output_global,
            params.layer(),
            params.namespace().to_owned(),
        ) {
            let _ = self
                .surface_manager
                .clear_layer_role(ctx.client_id, params.surface());
            ctx.registry.free_object(*params.id(), ctx.writer);
            report_layer_error(ctx, object_id, error);
        }
    }

    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ZwlrLayerShellV1Destroy<'_>,
    ) {
        if let Err(error) = self
            .layer_shell_manager
            .destroy_shell(ctx.client_id, object_id)
        {
            report_layer_error(ctx, object_id, error);
            return;
        }
        ctx.registry.free_object(object_id, ctx.writer);
    }
}

impl ZwlrLayerSurfaceV1 for DisplayState {
    fn set_size(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwlrLayerSurfaceV1SetSize<'_>,
    ) {
        if let Err(error) = self.layer_shell_manager.set_size(
            ctx.client_id,
            object_id,
            params.width(),
            params.height(),
        ) {
            report_layer_error(ctx, object_id, error);
        }
    }

    fn set_anchor(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwlrLayerSurfaceV1SetAnchor<'_>,
    ) {
        if let Err(error) =
            self.layer_shell_manager
                .set_anchor(ctx.client_id, object_id, params.anchor())
        {
            report_layer_error(ctx, object_id, error);
        }
    }

    fn set_exclusive_zone(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwlrLayerSurfaceV1SetExclusiveZone<'_>,
    ) {
        if let Err(error) = self.layer_shell_manager.set_exclusive_zone(
            ctx.client_id,
            object_id,
            params.zone(),
        ) {
            report_layer_error(ctx, object_id, error);
        }
    }

    fn set_margin(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwlrLayerSurfaceV1SetMargin<'_>,
    ) {
        if let Err(error) = self.layer_shell_manager.set_margin(
            ctx.client_id,
            object_id,
            params.top(),
            params.right(),
            params.bottom(),
            params.left(),
        ) {
            report_layer_error(ctx, object_id, error);
        }
    }

    fn set_keyboard_interactivity(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwlrLayerSurfaceV1SetKeyboardInteractivity<'_>,
    ) {
        if let Err(error) = self.layer_shell_manager.set_keyboard_interactivity(
            ctx.client_id,
            object_id,
            params.keyboard_interactivity(),
        ) {
            report_layer_error(ctx, object_id, error);
        }
    }

    fn get_popup(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwlrLayerSurfaceV1GetPopup<'_>,
    ) {
        if ctx.registry.interface_index(params.popup()) != Some(InterfaceIndex::XdgPopup) {
            report_layer_error(ctx, object_id, LayerShellError::UnknownSurface);
            return;
        }
        let Some(layer_wl) = self
            .layer_shell_manager
            .wl_surface(ctx.client_id, object_id)
        else {
            report_layer_error(ctx, object_id, LayerShellError::UnknownLayerSurface);
            return;
        };
        if let Err(error) = self.xdg_manager.assign_popup_layer_parent(
            ctx.client_id,
            params.popup(),
            layer_wl,
        ) {
            crate::protocols::xdg_shell::report_xdg_error(ctx, object_id, error);
            return;
        }
        if let Some(popup_wl) = self.xdg_manager.popup_wl(ctx.client_id, params.popup())
            && let Err(error) =
                self.surface_manager
                    .set_role_parent(ctx.client_id, popup_wl, layer_wl)
        {
            report_surface_role_error(ctx, object_id, error);
        }
    }

    fn ack_configure(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwlrLayerSurfaceV1AckConfigure<'_>,
    ) {
        if let Err(error) =
            self.layer_shell_manager
                .ack_configure(ctx.client_id, object_id, params.serial())
        {
            report_layer_error(ctx, object_id, error);
        }
    }

    fn destroy(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &ZwlrLayerSurfaceV1Destroy<'_>,
    ) {
        match self
            .layer_shell_manager
            .destroy_layer_surface(ctx.client_id, object_id)
        {
            Ok(wl_surface) => {
                let _ = self
                    .surface_manager
                    .set_xdg_map_ready(ctx.client_id, wl_surface, false);
                let _ = self
                    .surface_manager
                    .clear_layer_role(ctx.client_id, wl_surface);
                self.shm_manager
                    .clear_surface_backing(ctx.client_id, wl_surface);
                self.emit_surface_unmapped(ctx.client_id, wl_surface);
                ctx.registry.free_object(object_id, ctx.writer);
            }
            Err(error) => report_layer_error(ctx, object_id, error),
        }
    }

    fn set_layer(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &ZwlrLayerSurfaceV1SetLayer<'_>,
    ) {
        if let Err(error) =
            self.layer_shell_manager
                .set_layer(ctx.client_id, object_id, params.layer())
        {
            report_layer_error(ctx, object_id, error);
        }
    }

    fn set_exclusive_edge(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &ZwlrLayerSurfaceV1SetExclusiveEdge<'_>,
    ) {
        // Intentionally ignored: exclusive edge is deduced from anchors only.
    }
}

pub(crate) fn apply_layer_surface_commit(
    state: &mut DisplayState,
    client_id: lumalla_wayland_protocol::ClientId,
    surface_id: ObjectId,
    buffer: Option<bool>,
) -> Result<crate::layer_shell::LayerCommitOutcome, (ObjectId, LayerShellError)> {
    let Some(layer_id) = state
        .layer_shell_manager
        .layer_surface_for_wl(client_id, surface_id)
    else {
        return Ok(crate::layer_shell::LayerCommitOutcome::default());
    };
    let output = state
        .layer_shell_manager
        .output_global(client_id, layer_id)
        .ok_or((layer_id, LayerShellError::UnknownLayerSurface))?;
    let output_size = state
        .output_manager
        .get(output)
        .map(|info| (info.width, info.height))
        .unwrap_or((0, 0));
    let exclusive = state.layer_shell_manager.exclusive_insets_for_output(output);

    let outcome = state
        .layer_shell_manager
        .on_wl_surface_commit(client_id, surface_id, buffer, output_size, exclusive)
        .map_err(|error| (layer_id, error))?;

    if let Some(geo) = outcome.geometry {
        let _ = state
            .surface_manager
            .set_surface_layout(client_id, surface_id, geo.x, geo.y);
    }
    let map_ready = state
        .layer_shell_manager
        .is_map_ready(client_id, layer_id);
    let _ = state
        .surface_manager
        .set_xdg_map_ready(client_id, surface_id, map_ready);
    if outcome.unmapped {
        let _ = state
            .surface_manager
            .set_xdg_map_ready(client_id, surface_id, false);
    }
    Ok(outcome)
}

pub(crate) fn emit_layer_commit_events(
    ctx: &mut Ctx,
    layer_surface: ObjectId,
    outcome: crate::layer_shell::LayerCommitOutcome,
) {
    if let Some(configure) = outcome.initial_configure {
        ctx.writer
            .zwlr_layer_surface_v1_configure(layer_surface)
            .serial(configure.serial)
            .width(configure.width)
            .height(configure.height);
    }
}

pub(crate) fn report_layer_commit_error(
    ctx: &mut Ctx,
    object_id: ObjectId,
    error: LayerShellError,
) {
    report_layer_error(ctx, object_id, error);
}
