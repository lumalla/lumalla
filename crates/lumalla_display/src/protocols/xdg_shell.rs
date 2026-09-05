use log::debug;
use lumalla_wayland_protocol::{
    Ctx, NewObjectId, ObjectId,
    buffer::Writer,
    protocols::{XdgShellProtocol, wayland::WL_DISPLAY_ERROR_INVALID_OBJECT, xdg_shell::*},
    registry::{DISPLAY_OBJECT_ID, InterfaceIndex},
};

use crate::{
    DisplayState,
    surface::SurfaceError,
    xdg::{
        ConfigurePayload, ConfigureSnapshot, TOPLEVEL_STATE_ACTIVATED, TOPLEVEL_STATE_FULLSCREEN,
        TOPLEVEL_STATE_MAXIMIZED, XdgError,
    },
};

impl XdgShellProtocol for DisplayState {}

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

fn report_xdg_error(ctx: &mut Ctx, object_id: ObjectId, error: XdgError) {
    let (code, message) = match error {
        XdgError::RoleConflict => (XDG_WM_BASE_ERROR_ROLE, "Surface already has a role"),
        XdgError::AlreadyConstructed => (
            XDG_SURFACE_ERROR_ALREADY_CONSTRUCTED,
            "xdg_surface already has a role object",
        ),
        XdgError::NotConstructed => (
            XDG_SURFACE_ERROR_NOT_CONSTRUCTED,
            "xdg_surface has no role object",
        ),
        XdgError::InvalidSerial => (XDG_SURFACE_ERROR_INVALID_SERIAL, "Invalid configure serial"),
        XdgError::UnconfiguredBuffer => (
            XDG_SURFACE_ERROR_UNCONFIGURED_BUFFER,
            "Buffer attached before first configure",
        ),
        XdgError::DefunctSurfaces => (
            XDG_WM_BASE_ERROR_DEFUNCT_SURFACES,
            "xdg_wm_base destroyed before its children",
        ),
        XdgError::DefunctRoleObject => (
            XDG_SURFACE_ERROR_DEFUNCT_ROLE_OBJECT,
            "xdg_surface destroyed before its role object",
        ),
        XdgError::NotTopmostPopup => (
            XDG_WM_BASE_ERROR_NOT_THE_TOPMOST_POPUP,
            "xdg_popup is not the topmost popup",
        ),
        XdgError::InvalidPopupParent => (
            XDG_WM_BASE_ERROR_INVALID_POPUP_PARENT,
            "Invalid or unmapped popup parent",
        ),
        XdgError::InvalidSurfaceState => (
            XDG_WM_BASE_ERROR_INVALID_SURFACE_STATE,
            "wl_surface already has attached or committed content",
        ),
        XdgError::InvalidParent => (XDG_TOPLEVEL_ERROR_INVALID_PARENT, "Invalid toplevel parent"),
        XdgError::InvalidPositioner => (
            XDG_WM_BASE_ERROR_INVALID_POSITIONER,
            "Incomplete or invalid positioner",
        ),
        XdgError::InvalidPositionerInput => (
            XDG_POSITIONER_ERROR_INVALID_INPUT,
            "Invalid positioner input",
        ),
        XdgError::InvalidWindowGeometry => (
            XDG_SURFACE_ERROR_INVALID_SIZE,
            "Invalid window geometry size",
        ),
        XdgError::InvalidToplevelSize => (XDG_TOPLEVEL_ERROR_INVALID_SIZE, "Invalid toplevel size"),
        XdgError::InvalidGrab => (XDG_POPUP_ERROR_INVALID_GRAB, "Invalid popup grab"),
        XdgError::UnknownPositioner => (
            XDG_WM_BASE_ERROR_INVALID_POSITIONER,
            "Unknown xdg_positioner",
        ),
        XdgError::UnknownWmBase
        | XdgError::UnknownXdgSurface
        | XdgError::UnknownToplevel
        | XdgError::UnknownPopup
        | XdgError::UnknownSurface => (WL_DISPLAY_ERROR_INVALID_OBJECT, "Unknown object"),
    };
    debug!("xdg-shell protocol error: {error:?}");
    ctx.writer
        .wl_display_error(DISPLAY_OBJECT_ID)
        .object_id(object_id)
        .code(code)
        .message(message);
}

fn report_surface_role_error(ctx: &mut Ctx, object_id: ObjectId, error: SurfaceError) {
    let (code, message) = match error {
        SurfaceError::RoleAlreadyAssigned => (XDG_WM_BASE_ERROR_ROLE, "Surface already has a role"),
        SurfaceError::UnknownSurface => (WL_DISPLAY_ERROR_INVALID_OBJECT, "Unknown surface"),
        other => {
            debug!("Unexpected surface error in xdg path: {other:?}");
            (WL_DISPLAY_ERROR_INVALID_OBJECT, "Surface role error")
        }
    };
    ctx.writer
        .wl_display_error(DISPLAY_OBJECT_ID)
        .object_id(object_id)
        .code(code)
        .message(message);
}

pub(crate) fn emit_configure_snapshot(
    ctx: &mut Ctx,
    xdg_surface_id: ObjectId,
    snapshot: ConfigureSnapshot,
) {
    write_configure_snapshot(ctx.writer, xdg_surface_id, snapshot);
}

pub(crate) fn write_configure_snapshot(
    writer: &mut Writer,
    xdg_surface_id: ObjectId,
    snapshot: ConfigureSnapshot,
) {
    match snapshot.payload {
        ConfigurePayload::Toplevel {
            width,
            height,
            states,
        } => {
            let mut state_bytes = Vec::with_capacity(8);
            if states & TOPLEVEL_STATE_MAXIMIZED != 0 {
                state_bytes.extend_from_slice(&XDG_TOPLEVEL_STATE_MAXIMIZED.to_ne_bytes());
            }
            if states & TOPLEVEL_STATE_FULLSCREEN != 0 {
                state_bytes.extend_from_slice(&XDG_TOPLEVEL_STATE_FULLSCREEN.to_ne_bytes());
            }
            if states & TOPLEVEL_STATE_ACTIVATED != 0 {
                state_bytes.extend_from_slice(&XDG_TOPLEVEL_STATE_ACTIVATED.to_ne_bytes());
            }
            writer
                .xdg_toplevel_configure(snapshot.role_id)
                .width(width)
                .height(height)
                .states(&state_bytes);
        }
        ConfigurePayload::Popup { geometry, .. } => {
            writer
                .xdg_popup_configure(snapshot.role_id)
                .x(geometry.x)
                .y(geometry.y)
                .width(geometry.width)
                .height(geometry.height);
        }
    }
    writer
        .xdg_surface_configure(xdg_surface_id)
        .serial(snapshot.serial);
}

fn popup_constraint_bounds(
    state: &DisplayState,
    client_id: lumalla_wayland_protocol::ClientId,
    parent_xdg: ObjectId,
) -> Option<(i32, i32, i32, i32)> {
    let parent_wl = state.xdg_manager.xdg_surface_wl(client_id, parent_xdg)?;
    let (parent_x, parent_y) = state
        .surface_manager
        .surface_window_origin(client_id, parent_wl)
        .unwrap_or((0, 0));
    let (work_x, work_y, work_width, work_height) = state
        .output_manager
        .work_area_for_point(parent_x, parent_y)?;
    Some((
        work_x - parent_x,
        work_y - parent_y,
        work_width,
        work_height,
    ))
}

impl XdgWmBase for DisplayState {
    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &XdgWmBaseDestroy<'_>) {
        if let Err(error) = self.xdg_manager.destroy_wm_base(ctx.client_id, object_id) {
            report_xdg_error(ctx, object_id, error);
            return;
        }
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn create_positioner(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &XdgWmBaseCreatePositioner<'_>,
    ) {
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map_or(1, |object| object.version.min(XDG_POSITIONER_VERSION));
        if !register_object(ctx, params.id(), InterfaceIndex::XdgPositioner, version) {
            return;
        }
        if let Err(error) =
            self.xdg_manager
                .create_positioner_for_wm_base(ctx.client_id, object_id, *params.id())
        {
            ctx.registry.free_object(*params.id(), ctx.writer);
            report_xdg_error(ctx, object_id, error);
        }
    }

    fn get_xdg_surface(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &XdgWmBaseGetXdgSurface<'_>,
    ) {
        if ctx.registry.interface_index(params.surface()) != Some(InterfaceIndex::WlSurface) {
            report_xdg_error(ctx, params.surface(), XdgError::UnknownSurface);
            return;
        }
        if self
            .surface_manager
            .has_attached_or_committed_buffer(ctx.client_id, params.surface())
            .unwrap_or(false)
        {
            report_xdg_error(ctx, object_id, XdgError::InvalidSurfaceState);
            return;
        }
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map_or(1, |object| object.version.min(XDG_SURFACE_VERSION));
        if !register_object(ctx, params.id(), InterfaceIndex::XdgSurface, version) {
            return;
        }
        if let Err(error) =
            self.surface_manager
                .assign_xdg_role(ctx.client_id, params.surface(), *params.id())
        {
            ctx.registry.free_object(*params.id(), ctx.writer);
            report_surface_role_error(ctx, object_id, error);
            return;
        }
        if let Err(error) = self.xdg_manager.create_xdg_surface_owned(
            ctx.client_id,
            object_id,
            *params.id(),
            params.surface(),
        ) {
            let _ = self
                .surface_manager
                .clear_xdg_role(ctx.client_id, params.surface());
            ctx.registry.free_object(*params.id(), ctx.writer);
            report_xdg_error(ctx, object_id, error);
        }
    }

    fn pong(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &XdgWmBasePong<'_>) {
        // Liveness check acknowledged; no-op for MVP.
    }
}

impl XdgPositioner for DisplayState {
    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &XdgPositionerDestroy<'_>) {
        if let Err(error) = self
            .xdg_manager
            .destroy_positioner(ctx.client_id, object_id)
        {
            report_xdg_error(ctx, object_id, error);
            return;
        }
        ctx.registry.free_object(object_id, ctx.writer);
    }

    fn set_size(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &XdgPositionerSetSize<'_>) {
        if let Err(error) = self.xdg_manager.positioner_set_size(
            ctx.client_id,
            object_id,
            params.width(),
            params.height(),
        ) {
            report_xdg_error(ctx, object_id, error);
        }
    }

    fn set_anchor_rect(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &XdgPositionerSetAnchorRect<'_>,
    ) {
        if let Err(error) = self.xdg_manager.positioner_set_anchor_rect(
            ctx.client_id,
            object_id,
            params.x(),
            params.y(),
            params.width(),
            params.height(),
        ) {
            report_xdg_error(ctx, object_id, error);
        }
    }

    fn set_anchor(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &XdgPositionerSetAnchor<'_>,
    ) {
        if let Err(error) =
            self.xdg_manager
                .positioner_set_anchor(ctx.client_id, object_id, params.anchor())
        {
            report_xdg_error(ctx, object_id, error);
        }
    }

    fn set_gravity(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &XdgPositionerSetGravity<'_>,
    ) {
        if let Err(error) =
            self.xdg_manager
                .positioner_set_gravity(ctx.client_id, object_id, params.gravity())
        {
            report_xdg_error(ctx, object_id, error);
        }
    }

    fn set_constraint_adjustment(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &XdgPositionerSetConstraintAdjustment<'_>,
    ) {
        if let Err(error) = self.xdg_manager.positioner_set_constraint_adjustment(
            ctx.client_id,
            object_id,
            params.constraint_adjustment(),
        ) {
            report_xdg_error(ctx, object_id, error);
        }
    }

    fn set_offset(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &XdgPositionerSetOffset<'_>,
    ) {
        if let Err(error) =
            self.xdg_manager
                .positioner_set_offset(ctx.client_id, object_id, params.x(), params.y())
        {
            report_xdg_error(ctx, object_id, error);
        }
    }

    fn set_reactive(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &XdgPositionerSetReactive<'_>,
    ) {
        if let Err(error) = self
            .xdg_manager
            .positioner_set_reactive(ctx.client_id, object_id, true)
        {
            report_xdg_error(ctx, object_id, error);
        }
    }

    fn set_parent_size(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &XdgPositionerSetParentSize<'_>,
    ) {
        if let Err(error) = self.xdg_manager.positioner_set_parent_size(
            ctx.client_id,
            object_id,
            params.parent_width(),
            params.parent_height(),
        ) {
            report_xdg_error(ctx, object_id, error);
        }
    }

    fn set_parent_configure(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &XdgPositionerSetParentConfigure<'_>,
    ) {
        if let Err(error) = self.xdg_manager.positioner_set_parent_configure(
            ctx.client_id,
            object_id,
            params.serial(),
        ) {
            report_xdg_error(ctx, object_id, error);
        }
    }
}

impl XdgSurface for DisplayState {
    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &XdgSurfaceDestroy<'_>) {
        match self
            .xdg_manager
            .destroy_xdg_surface(ctx.client_id, object_id)
        {
            Ok(_wl_surface) => {
                ctx.registry.free_object(object_id, ctx.writer);
            }
            Err(error) => report_xdg_error(ctx, object_id, error),
        }
    }

    fn get_toplevel(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &XdgSurfaceGetToplevel<'_>,
    ) {
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map_or(1, |object| object.version.min(XDG_TOPLEVEL_VERSION));
        if !register_object(ctx, params.id(), InterfaceIndex::XdgToplevel, version) {
            return;
        }
        let Ok(wl_surface) = self
            .xdg_manager
            .wl_surface_for_xdg(ctx.client_id, object_id)
        else {
            ctx.registry.free_object(*params.id(), ctx.writer);
            report_xdg_error(ctx, object_id, XdgError::UnknownXdgSurface);
            return;
        };
        let (width, height) =
            self.register_toplevel(ctx.client_id, *params.id(), object_id, wl_surface);
        match self.xdg_manager.create_toplevel(
            ctx.client_id,
            *params.id(),
            object_id,
            width,
            height,
        ) {
            Ok(()) => {}
            Err(error) => {
                self.unregister_toplevel(ctx.client_id, *params.id());
                ctx.registry.free_object(*params.id(), ctx.writer);
                report_xdg_error(ctx, object_id, error);
            }
        }
    }

    fn get_popup(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &XdgSurfaceGetPopup<'_>) {
        if ctx.registry.interface_index(params.positioner()) != Some(InterfaceIndex::XdgPositioner)
        {
            report_xdg_error(ctx, object_id, XdgError::UnknownPositioner);
            return;
        }
        let parent = match params.parent() {
            Some(parent)
                if ctx.registry.interface_index(parent) == Some(InterfaceIndex::XdgSurface) =>
            {
                parent
            }
            Some(parent) => {
                report_xdg_error(ctx, parent, XdgError::UnknownXdgSurface);
                return;
            }
            None => {
                report_xdg_error(ctx, object_id, XdgError::UnknownXdgSurface);
                return;
            }
        };
        let version = ctx
            .registry
            .object_metadata(object_id)
            .map_or(1, |object| object.version.min(XDG_POPUP_VERSION));
        if !register_object(ctx, params.id(), InterfaceIndex::XdgPopup, version) {
            return;
        }
        let constraint_bounds = popup_constraint_bounds(self, ctx.client_id, parent);
        match self.xdg_manager.create_popup_with_bounds(
            ctx.client_id,
            *params.id(),
            object_id,
            parent,
            params.positioner(),
            constraint_bounds,
        ) {
            Ok(_geometry) => {
                // Initial popup configure is emitted after the required
                // bufferless wl_surface commit.
            }
            Err(error) => {
                ctx.registry.free_object(*params.id(), ctx.writer);
                report_xdg_error(ctx, object_id, error);
            }
        }
    }

    fn set_window_geometry(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &XdgSurfaceSetWindowGeometry<'_>,
    ) {
        if let Err(error) = self.xdg_manager.set_window_geometry(
            ctx.client_id,
            object_id,
            params.x(),
            params.y(),
            params.width(),
            params.height(),
        ) {
            report_xdg_error(ctx, object_id, error);
        }
    }

    fn ack_configure(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &XdgSurfaceAckConfigure<'_>,
    ) {
        if let Err(error) =
            self.xdg_manager
                .ack_configure(ctx.client_id, object_id, params.serial())
        {
            report_xdg_error(ctx, object_id, error);
        }
    }
}

impl XdgToplevel for DisplayState {
    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &XdgToplevelDestroy<'_>) {
        match self.xdg_manager.destroy_toplevel(ctx.client_id, object_id) {
            Ok(xdg_surface) => {
                if let Some(wl_surface) =
                    self.xdg_manager.xdg_surface_wl(ctx.client_id, xdg_surface)
                {
                    let _ =
                        self.surface_manager
                            .set_xdg_map_ready(ctx.client_id, wl_surface, false);
                    self.surface_updates
                        .push_back(crate::SurfaceUpdate::Unmapped {
                            client_id: ctx.client_id,
                            surface_id: wl_surface,
                        });
                }
                self.unregister_toplevel(ctx.client_id, object_id);
                ctx.registry.free_object(object_id, ctx.writer);
            }
            Err(error) => report_xdg_error(ctx, object_id, error),
        }
    }

    fn set_parent(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &XdgToplevelSetParent<'_>,
    ) {
        if let Err(error) =
            self.xdg_manager
                .set_toplevel_parent(ctx.client_id, object_id, params.parent())
        {
            report_xdg_error(ctx, object_id, error);
        }
    }

    fn set_title(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &XdgToplevelSetTitle<'_>) {
        let title = params.title().to_owned();
        if let Err(error) =
            self.xdg_manager
                .set_toplevel_title(ctx.client_id, object_id, title.clone())
        {
            report_xdg_error(ctx, object_id, error);
            return;
        }
        self.on_toplevel_title_set(ctx.client_id, object_id, title);
    }

    fn set_app_id(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &XdgToplevelSetAppId<'_>) {
        let app_id = params.app_id().to_owned();
        if let Err(error) =
            self.xdg_manager
                .set_toplevel_app_id(ctx.client_id, object_id, app_id.clone())
        {
            report_xdg_error(ctx, object_id, error);
            return;
        }
        self.queue_rule_geometry_for_toplevel(ctx.client_id, object_id, app_id);
    }

    fn show_window_menu(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &XdgToplevelShowWindowMenu<'_>,
    ) {
    }

    fn move_(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &XdgToplevelMove<'_>) {}

    fn resize(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &XdgToplevelResize<'_>) {}

    fn set_max_size(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &XdgToplevelSetMaxSize<'_>,
    ) {
        if let Err(error) = self.xdg_manager.set_toplevel_max_size(
            ctx.client_id,
            object_id,
            params.width(),
            params.height(),
        ) {
            report_xdg_error(ctx, object_id, error);
        }
    }

    fn set_min_size(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &XdgToplevelSetMinSize<'_>,
    ) {
        if let Err(error) = self.xdg_manager.set_toplevel_min_size(
            ctx.client_id,
            object_id,
            params.width(),
            params.height(),
        ) {
            report_xdg_error(ctx, object_id, error);
        }
    }

    fn set_maximized(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &XdgToplevelSetMaximized<'_>,
    ) {
        let size = self
            .output_manager
            .logical_size_for_client_output(ctx.client_id, None);
        match self
            .xdg_manager
            .set_toplevel_maximized_size(ctx.client_id, object_id, true, size)
        {
            Ok(Some(snapshot)) => {
                if let Ok(xdg_surface) = self
                    .xdg_manager
                    .xdg_surface_for_toplevel(ctx.client_id, object_id)
                {
                    emit_configure_snapshot(ctx, xdg_surface, snapshot);
                }
            }
            Ok(None) => {}
            Err(error) => report_xdg_error(ctx, object_id, error),
        }
    }

    fn unset_maximized(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &XdgToplevelUnsetMaximized<'_>,
    ) {
        match self
            .xdg_manager
            .set_toplevel_maximized(ctx.client_id, object_id, false)
        {
            Ok(Some(snapshot)) => {
                if let Ok(xdg_surface) = self
                    .xdg_manager
                    .xdg_surface_for_toplevel(ctx.client_id, object_id)
                {
                    emit_configure_snapshot(ctx, xdg_surface, snapshot);
                }
            }
            Ok(None) => {}
            Err(error) => report_xdg_error(ctx, object_id, error),
        }
    }

    fn set_fullscreen(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        params: &XdgToplevelSetFullscreen<'_>,
    ) {
        let size = self
            .output_manager
            .logical_size_for_client_output(ctx.client_id, params.output());
        match self
            .xdg_manager
            .set_toplevel_fullscreen_size(ctx.client_id, object_id, true, size)
        {
            Ok(Some(snapshot)) => {
                if let Ok(xdg_surface) = self
                    .xdg_manager
                    .xdg_surface_for_toplevel(ctx.client_id, object_id)
                {
                    emit_configure_snapshot(ctx, xdg_surface, snapshot);
                }
            }
            Ok(None) => {}
            Err(error) => report_xdg_error(ctx, object_id, error),
        }
    }

    fn unset_fullscreen(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &XdgToplevelUnsetFullscreen<'_>,
    ) {
        match self
            .xdg_manager
            .set_toplevel_fullscreen(ctx.client_id, object_id, false)
        {
            Ok(Some(snapshot)) => {
                if let Ok(xdg_surface) = self
                    .xdg_manager
                    .xdg_surface_for_toplevel(ctx.client_id, object_id)
                {
                    emit_configure_snapshot(ctx, xdg_surface, snapshot);
                }
            }
            Ok(None) => {}
            Err(error) => report_xdg_error(ctx, object_id, error),
        }
    }

    fn set_minimized(
        &mut self,
        _ctx: &mut Ctx,
        _object_id: ObjectId,
        _params: &XdgToplevelSetMinimized<'_>,
    ) {
    }
}

impl XdgPopup for DisplayState {
    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &XdgPopupDestroy<'_>) {
        match self.xdg_manager.destroy_popup(ctx.client_id, object_id) {
            Ok(xdg_surface) => {
                if let Some(wl_surface) =
                    self.xdg_manager.xdg_surface_wl(ctx.client_id, xdg_surface)
                {
                    let _ =
                        self.surface_manager
                            .set_xdg_map_ready(ctx.client_id, wl_surface, false);
                    self.surface_manager
                        .clear_role_parent(ctx.client_id, wl_surface);
                    self.surface_updates
                        .push_back(crate::SurfaceUpdate::Unmapped {
                            client_id: ctx.client_id,
                            surface_id: wl_surface,
                        });
                }
                ctx.registry.free_object(object_id, ctx.writer);
            }
            Err(error) => report_xdg_error(ctx, object_id, error),
        }
    }

    fn grab(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &XdgPopupGrab<'_>) {
        if ctx.registry.interface_index(params.seat()) != Some(InterfaceIndex::WlSeat) {
            report_xdg_error(ctx, object_id, XdgError::InvalidGrab);
            return;
        }
        if !self.seat_manager.is_valid_serial(params.serial()) {
            // Deny without a protocol error: dismiss the popup immediately.
            ctx.writer.xdg_popup_popup_done(object_id);
            return;
        }
        if let Err(error) = self.xdg_manager.grab_popup(ctx.client_id, object_id) {
            report_xdg_error(ctx, object_id, error);
        }
    }

    fn reposition(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &XdgPopupReposition<'_>) {
        if ctx.registry.interface_index(params.positioner()) != Some(InterfaceIndex::XdgPositioner)
        {
            report_xdg_error(ctx, object_id, XdgError::UnknownPositioner);
            return;
        }
        let constraint_bounds = self
            .xdg_manager
            .popup_parent_xdg(ctx.client_id, object_id)
            .and_then(|parent| popup_constraint_bounds(self, ctx.client_id, parent));
        match self.xdg_manager.reposition_popup_with_bounds(
            ctx.client_id,
            object_id,
            params.positioner(),
            params.token(),
            constraint_bounds,
        ) {
            Ok((serial, geometry, xdg_surface)) => {
                ctx.writer
                    .xdg_popup_repositioned(object_id)
                    .token(params.token());
                ctx.writer
                    .xdg_popup_configure(object_id)
                    .x(geometry.x)
                    .y(geometry.y)
                    .width(geometry.width)
                    .height(geometry.height);
                ctx.writer.xdg_surface_configure(xdg_surface).serial(serial);
            }
            Err(error) => report_xdg_error(ctx, object_id, error),
        }
    }
}

/// Apply xdg double-buffered state before the matching wl_surface state becomes
/// visible, so mapping and placement are atomic with the commit.
pub(crate) fn apply_xdg_surface_commit_with_buffer(
    state: &mut DisplayState,
    client_id: lumalla_wayland_protocol::ClientId,
    surface_id: ObjectId,
    buffer: Option<bool>,
) -> crate::xdg::CommitOutcome {
    let outcome = state
        .xdg_manager
        .on_wl_surface_commit_with_buffer(client_id, surface_id, buffer);

    if let Some(geometry) = outcome.window_geometry {
        let _ = state
            .surface_manager
            .set_window_geometry_offset(client_id, surface_id, geometry.x, geometry.y);
    }
    if let Some(snapshot) = outcome.applied_configure
        && let ConfigurePayload::Popup { geometry, .. } = snapshot.payload
    {
        let _ = state
            .surface_manager
            .set_surface_layout(client_id, surface_id, geometry.x, geometry.y);
        if let Some(parent_xdg) = state
            .xdg_manager
            .popup_parent_xdg(client_id, snapshot.role_id)
            && let Some(parent_wl) = state.xdg_manager.xdg_surface_wl(client_id, parent_xdg)
        {
            let _ = state
                .surface_manager
                .set_role_parent(client_id, surface_id, parent_wl);
        }
    }
    let ready = state.xdg_manager.can_map_wl_surface(client_id, surface_id);
    let _ = state
        .surface_manager
        .set_xdg_map_ready(client_id, surface_id, ready);
    outcome
}

pub(crate) fn emit_xdg_commit_events(
    state: &DisplayState,
    ctx: &mut Ctx,
    surface_id: ObjectId,
    outcome: crate::xdg::CommitOutcome,
) {
    if let Some(snapshot) = outcome.initial_configure
        && let Some(xdg_surface) = state
            .xdg_manager
            .xdg_surface_for_wl(ctx.client_id, surface_id)
    {
        emit_configure_snapshot(ctx, xdg_surface, snapshot);
    }
}

/// Report an xdg error from the wl_surface.commit path.
pub(crate) fn report_commit_error(ctx: &mut Ctx, object_id: ObjectId, error: XdgError) {
    report_xdg_error(ctx, object_id, error);
}
