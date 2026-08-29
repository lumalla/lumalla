use log::debug;
use lumalla_wayland_protocol::{
    Ctx, NewObjectId, ObjectId,
    protocols::{XdgShellProtocol, wayland::WL_DISPLAY_ERROR_INVALID_OBJECT, xdg_shell::*},
    registry::{DISPLAY_OBJECT_ID, InterfaceIndex},
};

use crate::{DisplayState, surface::SurfaceError, xdg::XdgError};

impl XdgShellProtocol for DisplayState {}

const DEFAULT_TOPLEVEL_WIDTH: i32 = 800;
const DEFAULT_TOPLEVEL_HEIGHT: i32 = 600;

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
            "Buffer attached before configure ack",
        ),
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

fn emit_toplevel_configure(
    state: &mut DisplayState,
    ctx: &mut Ctx,
    xdg_surface_id: ObjectId,
    toplevel_id: ObjectId,
) {
    let Ok(serial) = state
        .xdg_manager
        .send_configure_serial(ctx.client_id, xdg_surface_id)
    else {
        return;
    };
    let (width, height) = state
        .xdg_manager
        .toplevel_configure_size(ctx.client_id, toplevel_id)
        .unwrap_or((DEFAULT_TOPLEVEL_WIDTH, DEFAULT_TOPLEVEL_HEIGHT));
    ctx.writer
        .xdg_toplevel_configure(toplevel_id)
        .width(width)
        .height(height)
        .states(&[]);
    ctx.writer
        .xdg_surface_configure(xdg_surface_id)
        .serial(serial);
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
        self.xdg_manager
            .create_positioner(ctx.client_id, *params.id());
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
            report_surface_role_error(ctx, object_id, error);
            return;
        }
        if let Err(error) =
            self.xdg_manager
                .create_xdg_surface(ctx.client_id, *params.id(), params.surface())
        {
            let _ = self
                .surface_manager
                .clear_xdg_role(ctx.client_id, params.surface());
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
            Ok(wl_surface) => {
                let _ = self
                    .surface_manager
                    .clear_xdg_role(ctx.client_id, wl_surface);
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
            Ok(serial) => {
                ctx.writer
                    .xdg_toplevel_configure(*params.id())
                    .width(width)
                    .height(height)
                    .states(&[]);
                ctx.writer.xdg_surface_configure(object_id).serial(serial);
            }
            Err(error) => {
                self.unregister_toplevel(ctx.client_id, *params.id());
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
        match self.xdg_manager.create_popup(
            ctx.client_id,
            *params.id(),
            object_id,
            parent,
            params.positioner(),
        ) {
            Ok((serial, geometry)) => {
                if let (Some(popup_wl), Some(parent_wl)) = (
                    self.xdg_manager.xdg_surface_wl(ctx.client_id, object_id),
                    self.xdg_manager.xdg_surface_wl(ctx.client_id, parent),
                ) {
                    let (parent_x, parent_y) = self
                        .surface_manager
                        .surface_layout(ctx.client_id, parent_wl)
                        .unwrap_or((0, 0));
                    let _ = self.surface_manager.set_surface_layout(
                        ctx.client_id,
                        popup_wl,
                        parent_x + geometry.x,
                        parent_y + geometry.y,
                    );
                }
                ctx.writer
                    .xdg_popup_configure(*params.id())
                    .x(geometry.x)
                    .y(geometry.y)
                    .width(geometry.width)
                    .height(geometry.height);
                ctx.writer.xdg_surface_configure(object_id).serial(serial);
            }
            Err(error) => report_xdg_error(ctx, object_id, error),
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
            return;
        }
        if let Ok(wl_surface) = self
            .xdg_manager
            .wl_surface_for_xdg(ctx.client_id, object_id)
        {
            let _ = self
                .surface_manager
                .set_xdg_map_ready(ctx.client_id, wl_surface, true);
        }
    }
}

impl XdgToplevel for DisplayState {
    fn destroy(&mut self, ctx: &mut Ctx, object_id: ObjectId, _params: &XdgToplevelDestroy<'_>) {
        match self.xdg_manager.destroy_toplevel(ctx.client_id, object_id) {
            Ok(_) => {
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
        if let Ok(xdg_surface) = self
            .xdg_manager
            .xdg_surface_for_toplevel(ctx.client_id, object_id)
        {
            emit_toplevel_configure(self, ctx, xdg_surface, object_id);
        }
    }

    fn unset_maximized(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &XdgToplevelUnsetMaximized<'_>,
    ) {
        if let Ok(xdg_surface) = self
            .xdg_manager
            .xdg_surface_for_toplevel(ctx.client_id, object_id)
        {
            emit_toplevel_configure(self, ctx, xdg_surface, object_id);
        }
    }

    fn set_fullscreen(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &XdgToplevelSetFullscreen<'_>,
    ) {
        if let Ok(xdg_surface) = self
            .xdg_manager
            .xdg_surface_for_toplevel(ctx.client_id, object_id)
        {
            emit_toplevel_configure(self, ctx, xdg_surface, object_id);
        }
    }

    fn unset_fullscreen(
        &mut self,
        ctx: &mut Ctx,
        object_id: ObjectId,
        _params: &XdgToplevelUnsetFullscreen<'_>,
    ) {
        if let Ok(xdg_surface) = self
            .xdg_manager
            .xdg_surface_for_toplevel(ctx.client_id, object_id)
        {
            emit_toplevel_configure(self, ctx, xdg_surface, object_id);
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
            Ok(_) => ctx.registry.free_object(object_id, ctx.writer),
            Err(error) => report_xdg_error(ctx, object_id, error),
        }
    }

    fn grab(&mut self, _ctx: &mut Ctx, _object_id: ObjectId, _params: &XdgPopupGrab<'_>) {}

    fn reposition(&mut self, ctx: &mut Ctx, object_id: ObjectId, params: &XdgPopupReposition<'_>) {
        if ctx.registry.interface_index(params.positioner()) != Some(InterfaceIndex::XdgPositioner)
        {
            report_xdg_error(ctx, object_id, XdgError::UnknownPositioner);
            return;
        }
        match self
            .xdg_manager
            .reposition_popup(ctx.client_id, object_id, params.positioner())
        {
            Ok((serial, geometry, xdg_surface)) => {
                if let (Some(popup_wl), Some(parent_xdg)) = (
                    self.xdg_manager.xdg_surface_wl(ctx.client_id, xdg_surface),
                    self.xdg_manager.popup_parent_xdg(ctx.client_id, object_id),
                ) {
                    if let Some(parent_wl) =
                        self.xdg_manager.xdg_surface_wl(ctx.client_id, parent_xdg)
                    {
                        let (parent_x, parent_y) = self
                            .surface_manager
                            .surface_layout(ctx.client_id, parent_wl)
                            .unwrap_or((0, 0));
                        let _ = self.surface_manager.set_surface_layout(
                            ctx.client_id,
                            popup_wl,
                            parent_x + geometry.x,
                            parent_y + geometry.y,
                        );
                    }
                }
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

/// Hook called from wl_surface.commit path for xdg bookkeeping.
pub(crate) fn on_xdg_surface_commit(
    state: &mut DisplayState,
    client_id: lumalla_wayland_protocol::ClientId,
    surface_id: ObjectId,
) {
    state
        .xdg_manager
        .on_wl_surface_commit(client_id, surface_id);
}

/// Report an xdg error from the wl_surface.commit path.
pub(crate) fn report_commit_error(ctx: &mut Ctx, object_id: ObjectId, error: XdgError) {
    report_xdg_error(ctx, object_id, error);
}
