//! Minimal xdg-shell state: toplevel mapping via configure/ack.

use std::collections::HashMap;

use lumalla_wayland_protocol::{ClientId, ObjectId};

type ResourceKey = (ClientId, ObjectId);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XdgError {
    UnknownWmBase,
    UnknownPositioner,
    UnknownXdgSurface,
    UnknownToplevel,
    UnknownPopup,
    UnknownSurface,
    RoleConflict,
    AlreadyConstructed,
    NotConstructed,
    DefunctSurfaces,
    DefunctRoleObject,
    NotTopmostPopup,
    InvalidPopupParent,
    InvalidSurfaceState,
    InvalidParent,
    InvalidPositioner,
    InvalidPositionerInput,
    InvalidWindowGeometry,
    InvalidToplevelSize,
    InvalidGrab,
    InvalidSerial,
    UnconfiguredBuffer,
}

#[derive(Debug, Default)]
pub struct XdgManager {
    wm_bases: HashMap<ResourceKey, ()>,
    positioners: HashMap<ResourceKey, PositionerState>,
    xdg_surfaces: HashMap<ResourceKey, XdgSurfaceState>,
    toplevels: HashMap<ResourceKey, ToplevelState>,
    popups: HashMap<ResourceKey, PopupState>,
    /// wl_surface → xdg_surface
    surface_to_xdg: HashMap<ResourceKey, ObjectId>,
    next_configure_serial: u32,
}

#[derive(Debug, Clone)]
pub struct PositionerState {
    width: i32,
    height: i32,
    size_set: bool,
    anchor_x: i32,
    anchor_y: i32,
    anchor_width: i32,
    anchor_height: i32,
    anchor_rect_set: bool,
    anchor: u32,
    gravity: u32,
    constraint_adjustment: u32,
    offset_x: i32,
    offset_y: i32,
    reactive: bool,
    parent_size: Option<(i32, i32)>,
    parent_configure: Option<u32>,
}

impl Default for PositionerState {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            size_set: false,
            anchor_x: 0,
            anchor_y: 0,
            anchor_width: 0,
            anchor_height: 0,
            anchor_rect_set: false,
            anchor: 0,
            gravity: 0,
            constraint_adjustment: 0,
            offset_x: 0,
            offset_y: 0,
            reactive: false,
            parent_size: None,
            parent_configure: None,
        }
    }
}

#[derive(Debug)]
struct XdgSurfaceState {
    wm_base: Option<ObjectId>,
    wl_surface: ObjectId,
    role: Option<XdgRole>,
    role_alive: bool,
    pending_window_geometry: Option<WindowGeometry>,
    current_window_geometry: Option<WindowGeometry>,
    /// Configure snapshots not yet consumed by ack.
    pending_configures: Vec<ConfigureSnapshot>,
    /// Last acked snapshot, applied atomically by the next wl_surface commit.
    pending_ack: Option<ConfigureSnapshot>,
    current_configure: Option<ConfigureSnapshot>,
    initial_configure_sent: bool,
    mapped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XdgRole {
    Toplevel(ObjectId),
    Popup(ObjectId),
}

#[derive(Debug)]
struct ToplevelState {
    xdg_surface: ObjectId,
    title: String,
    app_id: String,
    parent: Option<ObjectId>,
    min_size: (i32, i32),
    max_size: (i32, i32),
    configure_width: i32,
    configure_height: i32,
    states: u32,
    maximized_requested: bool,
    restore_size: Option<(i32, i32)>,
}

#[derive(Debug)]
struct PopupState {
    xdg_surface: ObjectId,
    parent: ObjectId,
    positioner: PositionerState,
    current_geometry: Option<PopupGeometry>,
    pending_reposition: Option<(u32, PopupGeometry)>,
    grabbed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigurePayload {
    Toplevel {
        width: i32,
        height: i32,
        states: u32,
    },
    Popup {
        geometry: PopupGeometry,
        reposition_token: Option<u32>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigureSnapshot {
    pub serial: u32,
    pub role_id: ObjectId,
    pub payload: ConfigurePayload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CommitOutcome {
    pub initial_configure: Option<ConfigureSnapshot>,
    pub applied_configure: Option<ConfigureSnapshot>,
    pub window_geometry: Option<WindowGeometry>,
}

/// Result of resolving an xdg_positioner into a popup configure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PopupGeometry {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl XdgManager {
    pub fn create_wm_base(&mut self, client_id: ClientId, id: ObjectId) {
        self.wm_bases.insert((client_id, id), ());
    }

    pub fn destroy_wm_base(&mut self, client_id: ClientId, id: ObjectId) -> Result<(), XdgError> {
        if self
            .xdg_surfaces
            .iter()
            .any(|((owner, _), state)| *owner == client_id && state.wm_base == Some(id))
        {
            return Err(XdgError::DefunctSurfaces);
        }
        self.wm_bases
            .remove(&(client_id, id))
            .ok_or(XdgError::UnknownWmBase)?;
        Ok(())
    }

    pub fn create_positioner(&mut self, client_id: ClientId, id: ObjectId) {
        self.positioners
            .insert((client_id, id), PositionerState::default());
    }

    pub fn create_positioner_for_wm_base(
        &mut self,
        client_id: ClientId,
        wm_base: ObjectId,
        id: ObjectId,
    ) -> Result<(), XdgError> {
        if !self.wm_bases.contains_key(&(client_id, wm_base)) {
            return Err(XdgError::UnknownWmBase);
        }
        self.positioners
            .insert((client_id, id), PositionerState::default());
        Ok(())
    }

    pub fn destroy_positioner(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
    ) -> Result<(), XdgError> {
        self.positioners
            .remove(&(client_id, id))
            .ok_or(XdgError::UnknownPositioner)?;
        Ok(())
    }

    pub fn positioner_set_size(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        width: i32,
        height: i32,
    ) -> Result<(), XdgError> {
        self.positioner_mut(client_id, id)?
            .set_size(width, height)?;
        Ok(())
    }

    pub fn positioner_set_anchor_rect(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), XdgError> {
        self.positioner_mut(client_id, id)?
            .set_anchor_rect(x, y, width, height)?;
        Ok(())
    }

    pub fn positioner_set_anchor(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        anchor: u32,
    ) -> Result<(), XdgError> {
        self.positioner_mut(client_id, id)?.set_anchor(anchor)?;
        Ok(())
    }

    pub fn positioner_set_gravity(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        gravity: u32,
    ) -> Result<(), XdgError> {
        self.positioner_mut(client_id, id)?.set_gravity(gravity)?;
        Ok(())
    }

    pub fn positioner_set_constraint_adjustment(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        adjustment: u32,
    ) -> Result<(), XdgError> {
        self.positioner_mut(client_id, id)?
            .set_constraint_adjustment(adjustment)?;
        Ok(())
    }

    pub fn positioner_set_offset(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        x: i32,
        y: i32,
    ) -> Result<(), XdgError> {
        self.positioner_mut(client_id, id)?.set_offset(x, y);
        Ok(())
    }

    pub fn positioner_set_reactive(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        reactive: bool,
    ) -> Result<(), XdgError> {
        self.positioner_mut(client_id, id)?.set_reactive(reactive);
        Ok(())
    }

    pub fn positioner_set_parent_size(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        width: i32,
        height: i32,
    ) -> Result<(), XdgError> {
        self.positioner_mut(client_id, id)?
            .set_parent_size(width, height)?;
        Ok(())
    }

    pub fn positioner_set_parent_configure(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        serial: u32,
    ) -> Result<(), XdgError> {
        self.positioner_mut(client_id, id)?
            .set_parent_configure(serial);
        Ok(())
    }

    fn positioner_mut(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
    ) -> Result<&mut PositionerState, XdgError> {
        self.positioners
            .get_mut(&(client_id, id))
            .ok_or(XdgError::UnknownPositioner)
    }

    pub fn create_xdg_surface(
        &mut self,
        client_id: ClientId,
        xdg_surface_id: ObjectId,
        wl_surface: ObjectId,
    ) -> Result<(), XdgError> {
        self.create_xdg_surface_for_wm_base(client_id, None, xdg_surface_id, wl_surface)
    }

    pub fn create_xdg_surface_owned(
        &mut self,
        client_id: ClientId,
        wm_base: ObjectId,
        xdg_surface_id: ObjectId,
        wl_surface: ObjectId,
    ) -> Result<(), XdgError> {
        if !self.wm_bases.contains_key(&(client_id, wm_base)) {
            return Err(XdgError::UnknownWmBase);
        }
        self.create_xdg_surface_for_wm_base(client_id, Some(wm_base), xdg_surface_id, wl_surface)
    }

    fn create_xdg_surface_for_wm_base(
        &mut self,
        client_id: ClientId,
        wm_base: Option<ObjectId>,
        xdg_surface_id: ObjectId,
        wl_surface: ObjectId,
    ) -> Result<(), XdgError> {
        if self.surface_to_xdg.contains_key(&(client_id, wl_surface)) {
            return Err(XdgError::RoleConflict);
        }
        self.xdg_surfaces.insert(
            (client_id, xdg_surface_id),
            XdgSurfaceState {
                wm_base,
                wl_surface,
                role: None,
                role_alive: false,
                pending_window_geometry: None,
                current_window_geometry: None,
                pending_configures: Vec::new(),
                pending_ack: None,
                current_configure: None,
                initial_configure_sent: false,
                mapped: false,
            },
        );
        self.surface_to_xdg
            .insert((client_id, wl_surface), xdg_surface_id);
        Ok(())
    }

    pub fn destroy_xdg_surface(
        &mut self,
        client_id: ClientId,
        xdg_surface_id: ObjectId,
    ) -> Result<ObjectId, XdgError> {
        let state = self
            .xdg_surfaces
            .get(&(client_id, xdg_surface_id))
            .ok_or(XdgError::UnknownXdgSurface)?;
        if state.role_alive {
            return Err(XdgError::DefunctRoleObject);
        }
        let state = self
            .xdg_surfaces
            .remove(&(client_id, xdg_surface_id))
            .expect("xdg_surface checked above");
        self.surface_to_xdg.remove(&(client_id, state.wl_surface));
        Ok(state.wl_surface)
    }

    pub fn wl_surface_for_xdg(
        &self,
        client_id: ClientId,
        xdg_surface_id: ObjectId,
    ) -> Result<ObjectId, XdgError> {
        self.xdg_surfaces
            .get(&(client_id, xdg_surface_id))
            .map(|s| s.wl_surface)
            .ok_or(XdgError::UnknownXdgSurface)
    }

    #[allow(dead_code)]
    pub fn xdg_surface_for_wl(
        &self,
        client_id: ClientId,
        wl_surface: ObjectId,
    ) -> Option<ObjectId> {
        self.surface_to_xdg.get(&(client_id, wl_surface)).copied()
    }

    pub fn validate_wl_surface_destroy(
        &self,
        client_id: ClientId,
        wl_surface: ObjectId,
    ) -> Result<(), XdgError> {
        let Some(xdg_surface) = self.surface_to_xdg.get(&(client_id, wl_surface)) else {
            return Ok(());
        };
        let state = self
            .xdg_surfaces
            .get(&(client_id, *xdg_surface))
            .ok_or(XdgError::UnknownXdgSurface)?;
        if state.role_alive {
            return Err(XdgError::DefunctRoleObject);
        }
        Ok(())
    }

    pub fn create_toplevel(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
        xdg_surface_id: ObjectId,
        configure_width: i32,
        configure_height: i32,
    ) -> Result<(), XdgError> {
        let surface = self
            .xdg_surfaces
            .get_mut(&(client_id, xdg_surface_id))
            .ok_or(XdgError::UnknownXdgSurface)?;
        if surface.role.is_some() {
            return Err(XdgError::AlreadyConstructed);
        }
        surface.role = Some(XdgRole::Toplevel(toplevel_id));
        surface.role_alive = true;
        self.toplevels.insert(
            (client_id, toplevel_id),
            ToplevelState {
                xdg_surface: xdg_surface_id,
                title: String::new(),
                app_id: String::new(),
                parent: None,
                min_size: (0, 0),
                max_size: (0, 0),
                configure_width,
                configure_height,
                states: 0,
                maximized_requested: false,
                restore_size: None,
            },
        );
        Ok(())
    }

    /// Allocates a configure serial and records it as pending. Caller must emit events.
    pub fn send_configure_serial(
        &mut self,
        client_id: ClientId,
        xdg_surface_id: ObjectId,
    ) -> Result<u32, XdgError> {
        self.configure_snapshot(client_id, xdg_surface_id)
            .map(|snapshot| snapshot.serial)
    }

    pub fn configure_snapshot(
        &mut self,
        client_id: ClientId,
        xdg_surface_id: ObjectId,
    ) -> Result<ConfigureSnapshot, XdgError> {
        let surface = self
            .xdg_surfaces
            .get(&(client_id, xdg_surface_id))
            .ok_or(XdgError::UnknownXdgSurface)?;
        let (role_id, payload) = match surface.role {
            Some(XdgRole::Toplevel(id)) => {
                let state = self
                    .toplevels
                    .get(&(client_id, id))
                    .ok_or(XdgError::UnknownToplevel)?;
                (
                    id,
                    ConfigurePayload::Toplevel {
                        width: state.configure_width,
                        height: state.configure_height,
                        states: state.states,
                    },
                )
            }
            Some(XdgRole::Popup(id)) => {
                let state = self
                    .popups
                    .get(&(client_id, id))
                    .ok_or(XdgError::UnknownPopup)?;
                let (geometry, token) = state.pending_reposition.map_or(
                    (
                        state
                            .current_geometry
                            .unwrap_or_else(|| state.positioner.compute_geometry()),
                        None,
                    ),
                    |(token, geometry)| (geometry, Some(token)),
                );
                (
                    id,
                    ConfigurePayload::Popup {
                        geometry,
                        reposition_token: token,
                    },
                )
            }
            None => return Err(XdgError::NotConstructed),
        };
        self.next_configure_serial = self.next_configure_serial.wrapping_add(1).max(1);
        let snapshot = ConfigureSnapshot {
            serial: self.next_configure_serial,
            role_id,
            payload,
        };
        let surface = self
            .xdg_surfaces
            .get_mut(&(client_id, xdg_surface_id))
            .expect("xdg_surface checked above");
        surface.pending_configures.push(snapshot);
        surface.initial_configure_sent = true;
        Ok(snapshot)
    }

    pub fn toplevel_configure(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
    ) -> Result<ConfigureSnapshot, XdgError> {
        let xdg_surface = self.xdg_surface_for_toplevel(client_id, toplevel_id)?;
        self.configure_snapshot(client_id, xdg_surface)
    }

    pub fn toplevel_configure_size(
        &self,
        client_id: ClientId,
        toplevel_id: ObjectId,
    ) -> Result<(i32, i32), XdgError> {
        let toplevel = self
            .toplevels
            .get(&(client_id, toplevel_id))
            .ok_or(XdgError::UnknownToplevel)?;
        Ok((toplevel.configure_width, toplevel.configure_height))
    }

    pub fn set_toplevel_configure_size(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
        width: i32,
        height: i32,
    ) -> Result<(), XdgError> {
        let toplevel = self
            .toplevels
            .get_mut(&(client_id, toplevel_id))
            .ok_or(XdgError::UnknownToplevel)?;
        toplevel.configure_width = width;
        toplevel.configure_height = height;
        Ok(())
    }

    pub fn set_toplevel_maximized(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
        enabled: bool,
    ) -> Result<Option<ConfigureSnapshot>, XdgError> {
        self.set_toplevel_state(
            client_id,
            toplevel_id,
            TOPLEVEL_STATE_MAXIMIZED,
            enabled,
            None,
        )
    }

    pub fn set_toplevel_maximized_size(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
        enabled: bool,
        size: Option<(i32, i32)>,
    ) -> Result<Option<ConfigureSnapshot>, XdgError> {
        self.set_toplevel_state(
            client_id,
            toplevel_id,
            TOPLEVEL_STATE_MAXIMIZED,
            enabled,
            size,
        )
    }

    pub fn set_toplevel_fullscreen(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
        enabled: bool,
    ) -> Result<Option<ConfigureSnapshot>, XdgError> {
        self.set_toplevel_state(
            client_id,
            toplevel_id,
            TOPLEVEL_STATE_FULLSCREEN,
            enabled,
            None,
        )
    }

    pub fn set_toplevel_fullscreen_size(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
        enabled: bool,
        size: Option<(i32, i32)>,
    ) -> Result<Option<ConfigureSnapshot>, XdgError> {
        self.set_toplevel_state(
            client_id,
            toplevel_id,
            TOPLEVEL_STATE_FULLSCREEN,
            enabled,
            size,
        )
    }

    fn set_toplevel_state(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
        bit: u32,
        enabled: bool,
        target_size: Option<(i32, i32)>,
    ) -> Result<Option<ConfigureSnapshot>, XdgError> {
        let state = self
            .toplevels
            .get_mut(&(client_id, toplevel_id))
            .ok_or(XdgError::UnknownToplevel)?;
        if enabled {
            if state.states & (TOPLEVEL_STATE_MAXIMIZED | TOPLEVEL_STATE_FULLSCREEN) == 0 {
                state.restore_size = Some((state.configure_width, state.configure_height));
            }
            if bit == TOPLEVEL_STATE_MAXIMIZED {
                state.maximized_requested = true;
                if state.states & TOPLEVEL_STATE_FULLSCREEN == 0 {
                    state.states |= TOPLEVEL_STATE_MAXIMIZED;
                }
            } else {
                state.states |= TOPLEVEL_STATE_FULLSCREEN;
                state.states &= !TOPLEVEL_STATE_MAXIMIZED;
            }
            if let Some((width, height)) = target_size {
                state.configure_width = width;
                state.configure_height = height;
            }
        } else {
            if bit == TOPLEVEL_STATE_MAXIMIZED {
                state.maximized_requested = false;
                state.states &= !TOPLEVEL_STATE_MAXIMIZED;
            } else {
                state.states &= !TOPLEVEL_STATE_FULLSCREEN;
                if state.maximized_requested {
                    state.states |= TOPLEVEL_STATE_MAXIMIZED;
                }
            }
            if state.states & (TOPLEVEL_STATE_MAXIMIZED | TOPLEVEL_STATE_FULLSCREEN) == 0
                && let Some((width, height)) = state.restore_size.take()
            {
                state.configure_width = width;
                state.configure_height = height;
            }
        }
        let xdg_surface = state.xdg_surface;
        let configure_started = self
            .xdg_surfaces
            .get(&(client_id, xdg_surface))
            .is_some_and(|surface| surface.initial_configure_sent);
        if configure_started {
            self.toplevel_configure(client_id, toplevel_id).map(Some)
        } else {
            Ok(None)
        }
    }

    pub fn is_mapped_xdg_surface(&self, client_id: ClientId, id: ObjectId) -> bool {
        self.xdg_surfaces
            .get(&(client_id, id))
            .is_some_and(|surface| surface.mapped)
    }

    pub fn xdg_surface_for_toplevel(
        &self,
        client_id: ClientId,
        toplevel_id: ObjectId,
    ) -> Result<ObjectId, XdgError> {
        self.toplevels
            .get(&(client_id, toplevel_id))
            .map(|t| t.xdg_surface)
            .ok_or(XdgError::UnknownToplevel)
    }

    pub fn destroy_toplevel(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
    ) -> Result<ObjectId, XdgError> {
        let toplevel = self
            .toplevels
            .remove(&(client_id, toplevel_id))
            .ok_or(XdgError::UnknownToplevel)?;
        if let Some(surface) = self
            .xdg_surfaces
            .get_mut(&(client_id, toplevel.xdg_surface))
        {
            surface.role_alive = false;
            surface.mapped = false;
        }
        Ok(toplevel.xdg_surface)
    }

    pub fn set_toplevel_title(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
        title: String,
    ) -> Result<(), XdgError> {
        self.toplevels
            .get_mut(&(client_id, toplevel_id))
            .ok_or(XdgError::UnknownToplevel)?
            .title = title;
        Ok(())
    }

    pub fn set_toplevel_app_id(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
        app_id: String,
    ) -> Result<(), XdgError> {
        self.toplevels
            .get_mut(&(client_id, toplevel_id))
            .ok_or(XdgError::UnknownToplevel)?
            .app_id = app_id;
        Ok(())
    }

    pub fn set_toplevel_parent(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
        parent: Option<ObjectId>,
    ) -> Result<(), XdgError> {
        if let Some(parent_id) = parent {
            if parent_id == toplevel_id {
                return Err(XdgError::InvalidParent);
            }
            let parent_state = self
                .toplevels
                .get(&(client_id, parent_id))
                .ok_or(XdgError::InvalidParent)?;
            let parent_mapped = self
                .xdg_surfaces
                .get(&(client_id, parent_state.xdg_surface))
                .is_some_and(|surface| surface.mapped && surface.role_alive);
            let parent = if parent_mapped { Some(parent_id) } else { None };
            let mut ancestor = parent;
            while let Some(id) = ancestor {
                if id == toplevel_id {
                    return Err(XdgError::InvalidParent);
                }
                ancestor = self
                    .toplevels
                    .get(&(client_id, id))
                    .and_then(|state| state.parent);
            }
            self.toplevels
                .get_mut(&(client_id, toplevel_id))
                .ok_or(XdgError::UnknownToplevel)?
                .parent = parent;
        } else {
            self.toplevels
                .get_mut(&(client_id, toplevel_id))
                .ok_or(XdgError::UnknownToplevel)?
                .parent = None;
        }
        Ok(())
    }

    pub fn set_toplevel_min_size(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
        width: i32,
        height: i32,
    ) -> Result<(), XdgError> {
        if width < 0 || height < 0 {
            return Err(XdgError::InvalidToplevelSize);
        }
        let state = self
            .toplevels
            .get_mut(&(client_id, toplevel_id))
            .ok_or(XdgError::UnknownToplevel)?;
        if (state.max_size.0 > 0 && width > state.max_size.0)
            || (state.max_size.1 > 0 && height > state.max_size.1)
        {
            return Err(XdgError::InvalidToplevelSize);
        }
        state.min_size = (width, height);
        Ok(())
    }

    pub fn set_toplevel_max_size(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
        width: i32,
        height: i32,
    ) -> Result<(), XdgError> {
        if width < 0 || height < 0 {
            return Err(XdgError::InvalidToplevelSize);
        }
        let state = self
            .toplevels
            .get_mut(&(client_id, toplevel_id))
            .ok_or(XdgError::UnknownToplevel)?;
        if (width > 0 && width < state.min_size.0) || (height > 0 && height < state.min_size.1) {
            return Err(XdgError::InvalidToplevelSize);
        }
        state.max_size = (width, height);
        Ok(())
    }

    pub fn create_popup(
        &mut self,
        client_id: ClientId,
        popup_id: ObjectId,
        xdg_surface_id: ObjectId,
        parent: ObjectId,
        positioner: ObjectId,
    ) -> Result<PopupGeometry, XdgError> {
        self.create_popup_with_bounds(
            client_id,
            popup_id,
            xdg_surface_id,
            parent,
            positioner,
            None,
        )
    }

    pub fn create_popup_with_bounds(
        &mut self,
        client_id: ClientId,
        popup_id: ObjectId,
        xdg_surface_id: ObjectId,
        parent: ObjectId,
        positioner: ObjectId,
        constraint_bounds: Option<(i32, i32, i32, i32)>,
    ) -> Result<PopupGeometry, XdgError> {
        let positioner_state = self
            .positioners
            .get(&(client_id, positioner))
            .ok_or(XdgError::UnknownPositioner)?
            .clone();
        positioner_state.validate_complete()?;
        positioner_state.validate_anchor_bounds()?;
        let parent_surface = self
            .xdg_surfaces
            .get(&(client_id, parent))
            .ok_or(XdgError::InvalidPopupParent)?;
        if !parent_surface.role_alive || !parent_surface.mapped {
            return Err(XdgError::InvalidPopupParent);
        }
        let parent_size = parent_surface.current_window_geometry.map_or_else(
            || match parent_surface
                .current_configure
                .map(|snapshot| snapshot.payload)
            {
                Some(ConfigurePayload::Toplevel { width, height, .. })
                    if width > 0 && height > 0 =>
                {
                    Some((width, height))
                }
                Some(ConfigurePayload::Popup { geometry, .. }) => {
                    Some((geometry.width, geometry.height))
                }
                _ => None,
            },
            |geometry| Some((geometry.width, geometry.height)),
        );
        if let Some((width, height)) = parent_size {
            positioner_state.validate_anchor_with_size(width, height)?;
        }
        if let Some(serial) = positioner_state.parent_configure {
            let valid = parent_surface
                .current_configure
                .is_some_and(|snapshot| snapshot.serial == serial)
                || parent_surface
                    .pending_configures
                    .iter()
                    .any(|snapshot| snapshot.serial == serial);
            if !valid {
                return Err(XdgError::InvalidPositioner);
            }
        }
        let geometry = positioner_state.compute_geometry_with_bounds(constraint_bounds);
        let surface = self
            .xdg_surfaces
            .get_mut(&(client_id, xdg_surface_id))
            .ok_or(XdgError::UnknownXdgSurface)?;
        if surface.role.is_some() {
            return Err(XdgError::AlreadyConstructed);
        }
        surface.role = Some(XdgRole::Popup(popup_id));
        surface.role_alive = true;
        self.popups.insert(
            (client_id, popup_id),
            PopupState {
                xdg_surface: xdg_surface_id,
                parent,
                positioner: positioner_state,
                current_geometry: None,
                pending_reposition: None,
                grabbed: false,
            },
        );
        Ok(geometry)
    }

    /// wl_surface backing an xdg_surface, if known.
    pub fn xdg_surface_wl(&self, client_id: ClientId, xdg_surface: ObjectId) -> Option<ObjectId> {
        self.xdg_surfaces
            .get(&(client_id, xdg_surface))
            .map(|s| s.wl_surface)
    }

    /// Apply a new positioner to an existing popup and allocate a configure serial.
    pub fn reposition_popup(
        &mut self,
        client_id: ClientId,
        popup_id: ObjectId,
        positioner: ObjectId,
        token: u32,
    ) -> Result<(u32, PopupGeometry, ObjectId), XdgError> {
        self.reposition_popup_with_bounds(client_id, popup_id, positioner, token, None)
    }

    pub fn reposition_popup_with_bounds(
        &mut self,
        client_id: ClientId,
        popup_id: ObjectId,
        positioner: ObjectId,
        token: u32,
        constraint_bounds: Option<(i32, i32, i32, i32)>,
    ) -> Result<(u32, PopupGeometry, ObjectId), XdgError> {
        let positioner_state = self
            .positioners
            .get(&(client_id, positioner))
            .ok_or(XdgError::UnknownPositioner)?
            .clone();
        positioner_state.validate_complete()?;
        positioner_state.validate_anchor_bounds()?;
        let parent = self
            .popups
            .get(&(client_id, popup_id))
            .ok_or(XdgError::UnknownPopup)?
            .parent;
        if let Some(parent_surface) = self.xdg_surfaces.get(&(client_id, parent))
            && let Some(geometry) = parent_surface.current_window_geometry
        {
            positioner_state.validate_anchor_with_size(geometry.width, geometry.height)?;
        }
        let geometry = positioner_state.compute_geometry_with_bounds(constraint_bounds);
        let popup = self
            .popups
            .get_mut(&(client_id, popup_id))
            .ok_or(XdgError::UnknownPopup)?;
        popup.positioner = positioner_state;
        popup.pending_reposition = Some((token, geometry));
        let xdg_surface = popup.xdg_surface;
        let serial = self.send_configure_serial(client_id, xdg_surface)?;
        Ok((serial, geometry, xdg_surface))
    }

    /// Parent xdg_surface for a popup, if known.
    pub fn popup_parent_xdg(&self, client_id: ClientId, popup_id: ObjectId) -> Option<ObjectId> {
        self.popups.get(&(client_id, popup_id)).map(|p| p.parent)
    }

    pub fn grab_popup(&mut self, client_id: ClientId, popup_id: ObjectId) -> Result<(), XdgError> {
        let popup = self
            .popups
            .get(&(client_id, popup_id))
            .ok_or(XdgError::UnknownPopup)?;
        let surface = self
            .xdg_surfaces
            .get(&(client_id, popup.xdg_surface))
            .ok_or(XdgError::UnknownXdgSurface)?;
        if surface.mapped {
            return Err(XdgError::InvalidGrab);
        }
        if let Some(parent_popup) = self
            .popups
            .values()
            .find(|candidate| candidate.xdg_surface == popup.parent)
            && !parent_popup.grabbed
        {
            return Err(XdgError::InvalidGrab);
        }
        self.popups
            .get_mut(&(client_id, popup_id))
            .expect("popup checked above")
            .grabbed = true;
        Ok(())
    }

    pub fn destroy_popup(
        &mut self,
        client_id: ClientId,
        popup_id: ObjectId,
    ) -> Result<ObjectId, XdgError> {
        let popup = self
            .popups
            .get(&(client_id, popup_id))
            .ok_or(XdgError::UnknownPopup)?;
        if self
            .popups
            .iter()
            .any(|((owner, _), child)| *owner == client_id && child.parent == popup.xdg_surface)
        {
            return Err(XdgError::NotTopmostPopup);
        }
        let popup = self
            .popups
            .remove(&(client_id, popup_id))
            .expect("popup checked above");
        if let Some(surface) = self.xdg_surfaces.get_mut(&(client_id, popup.xdg_surface)) {
            surface.role_alive = false;
            surface.mapped = false;
        }
        Ok(popup.xdg_surface)
    }

    pub fn set_window_geometry(
        &mut self,
        client_id: ClientId,
        xdg_surface_id: ObjectId,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), XdgError> {
        if width <= 0 || height <= 0 {
            return Err(XdgError::InvalidWindowGeometry);
        }
        let surface = self
            .xdg_surfaces
            .get_mut(&(client_id, xdg_surface_id))
            .ok_or(XdgError::UnknownXdgSurface)?;
        if !surface.role_alive {
            return Err(XdgError::NotConstructed);
        }
        surface.pending_window_geometry = Some(WindowGeometry {
            x,
            y,
            width,
            height,
        });
        Ok(())
    }

    pub fn current_window_geometry(
        &self,
        client_id: ClientId,
        xdg_surface_id: ObjectId,
    ) -> Result<Option<WindowGeometry>, XdgError> {
        self.xdg_surfaces
            .get(&(client_id, xdg_surface_id))
            .map(|surface| surface.current_window_geometry)
            .ok_or(XdgError::UnknownXdgSurface)
    }

    pub fn ack_configure(
        &mut self,
        client_id: ClientId,
        xdg_surface_id: ObjectId,
        serial: u32,
    ) -> Result<(), XdgError> {
        let surface = self
            .xdg_surfaces
            .get_mut(&(client_id, xdg_surface_id))
            .ok_or(XdgError::UnknownXdgSurface)?;
        let Some(index) = surface
            .pending_configures
            .iter()
            .position(|snapshot| snapshot.serial == serial)
        else {
            return Err(XdgError::InvalidSerial);
        };
        let snapshot = surface.pending_configures[index];
        // Consume this serial and all older ones.
        surface.pending_configures.drain(..=index);
        surface.pending_ack = Some(snapshot);
        Ok(())
    }

    /// Returns whether the wl_surface may map (has role + acked configure).
    #[allow(dead_code)]
    pub fn can_map_wl_surface(&self, client_id: ClientId, wl_surface: ObjectId) -> bool {
        let Some(xdg_id) = self.surface_to_xdg.get(&(client_id, wl_surface)) else {
            return false;
        };
        let Some(surface) = self.xdg_surfaces.get(&(client_id, *xdg_id)) else {
            return false;
        };
        surface.role_alive && surface.current_configure.is_some()
    }

    /// Called when a buffer is attached/committed: enforce configure-before-buffer for xdg.
    pub fn check_buffer_commit(
        &self,
        client_id: ClientId,
        wl_surface: ObjectId,
        attaching_buffer: bool,
    ) -> Result<(), XdgError> {
        let Some(xdg_id) = self.surface_to_xdg.get(&(client_id, wl_surface)) else {
            return Ok(());
        };
        let surface = self
            .xdg_surfaces
            .get(&(client_id, *xdg_id))
            .ok_or(XdgError::UnknownXdgSurface)?;
        if surface.role.is_none() || !surface.role_alive {
            return Err(XdgError::NotConstructed);
        }
        if attaching_buffer {
            let no_usable_configure =
                surface.current_configure.is_none() && surface.pending_ack.is_none();
            let newer_configure_unacked =
                !surface.pending_configures.is_empty() && surface.pending_ack.is_none();
            if no_usable_configure || newer_configure_unacked {
                return Err(XdgError::UnconfiguredBuffer);
            }
        }
        Ok(())
    }

    /// Apply pending ack on commit.
    pub fn on_wl_surface_commit(
        &mut self,
        client_id: ClientId,
        wl_surface: ObjectId,
    ) -> CommitOutcome {
        self.on_wl_surface_commit_with_buffer(client_id, wl_surface, None)
    }

    /// Apply xdg double-buffered state. `buffer` is `Some(true)` for a non-null
    /// attachment, `Some(false)` for a null attachment, and `None` when the
    /// caller cannot distinguish the attachment state.
    pub fn on_wl_surface_commit_with_buffer(
        &mut self,
        client_id: ClientId,
        wl_surface: ObjectId,
        buffer: Option<bool>,
    ) -> CommitOutcome {
        let Some(xdg_id) = self.surface_to_xdg.get(&(client_id, wl_surface)).copied() else {
            return CommitOutcome::default();
        };
        let mut outcome = CommitOutcome::default();
        let Some(surface) = self.xdg_surfaces.get_mut(&(client_id, xdg_id)) else {
            return outcome;
        };
        if let Some(geometry) = surface.pending_window_geometry.take() {
            surface.current_window_geometry = Some(geometry);
            outcome.window_geometry = Some(geometry);
        }
        if let Some(snapshot) = surface.pending_ack.take() {
            surface.current_configure = Some(snapshot);
            outcome.applied_configure = Some(snapshot);
            if let ConfigurePayload::Popup {
                geometry,
                reposition_token,
            } = snapshot.payload
                && let Some(XdgRole::Popup(popup_id)) = surface.role
                && let Some(popup) = self.popups.get_mut(&(client_id, popup_id))
            {
                popup.current_geometry = Some(geometry);
                if reposition_token.is_some() {
                    popup.pending_reposition = None;
                }
            }
        }
        match buffer {
            Some(false) => {
                surface.mapped = false;
                surface.current_configure = None;
                surface.initial_configure_sent = false;
                surface.pending_ack = None;
                surface.pending_configures.clear();
            }
            Some(true) => surface.mapped = surface.current_configure.is_some(),
            None => {}
        }
        if buffer != Some(true) && !surface.initial_configure_sent && surface.role_alive {
            // The caller must emit this role payload followed by xdg_surface.configure.
            outcome.initial_configure = self.configure_snapshot(client_id, xdg_id).ok();
        }
        outcome
    }

    #[allow(dead_code)]
    pub fn is_xdg_toplevel_mapped_role(&self, client_id: ClientId, wl_surface: ObjectId) -> bool {
        self.can_map_wl_surface(client_id, wl_surface)
    }

    pub fn delete_client(&mut self, client_id: ClientId) {
        self.wm_bases.retain(|(owner, _), _| *owner != client_id);
        self.positioners.retain(|(owner, _), _| *owner != client_id);
        self.xdg_surfaces
            .retain(|(owner, _), _| *owner != client_id);
        self.toplevels.retain(|(owner, _), _| *owner != client_id);
        self.popups.retain(|(owner, _), _| *owner != client_id);
        self.surface_to_xdg
            .retain(|(owner, _), _| *owner != client_id);
    }
}

impl PositionerState {
    pub fn set_size(&mut self, width: i32, height: i32) -> Result<(), XdgError> {
        if width <= 0 || height <= 0 {
            return Err(XdgError::InvalidPositionerInput);
        }
        self.width = width;
        self.height = height;
        self.size_set = true;
        Ok(())
    }

    pub fn set_anchor_rect(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), XdgError> {
        if width < 0 || height < 0 {
            return Err(XdgError::InvalidPositionerInput);
        }
        self.anchor_x = x;
        self.anchor_y = y;
        self.anchor_width = width;
        self.anchor_height = height;
        self.anchor_rect_set = true;
        Ok(())
    }

    pub fn set_anchor(&mut self, anchor: u32) -> Result<(), XdgError> {
        if anchor > ANCHOR_BOTTOM_RIGHT {
            return Err(XdgError::InvalidPositionerInput);
        }
        self.anchor = anchor;
        Ok(())
    }

    pub fn set_gravity(&mut self, gravity: u32) -> Result<(), XdgError> {
        if gravity > ANCHOR_BOTTOM_RIGHT {
            return Err(XdgError::InvalidPositionerInput);
        }
        self.gravity = gravity;
        Ok(())
    }

    pub fn set_constraint_adjustment(&mut self, adjustment: u32) -> Result<(), XdgError> {
        if adjustment & !CONSTRAINT_ALL != 0 {
            return Err(XdgError::InvalidPositionerInput);
        }
        self.constraint_adjustment = adjustment;
        Ok(())
    }

    pub fn set_offset(&mut self, x: i32, y: i32) {
        self.offset_x = x;
        self.offset_y = y;
    }

    pub fn set_reactive(&mut self, reactive: bool) {
        self.reactive = reactive;
    }

    pub fn set_parent_size(&mut self, width: i32, height: i32) -> Result<(), XdgError> {
        if width <= 0 || height <= 0 {
            return Err(XdgError::InvalidPositionerInput);
        }
        self.parent_size = Some((width, height));
        Ok(())
    }

    pub fn set_parent_configure(&mut self, serial: u32) {
        self.parent_configure = Some(serial);
    }

    pub fn validate_complete(&self) -> Result<(), XdgError> {
        if !self.size_set
            || !self.anchor_rect_set
            || self.width <= 0
            || self.height <= 0
            || self.anchor_width <= 0
            || self.anchor_height <= 0
        {
            return Err(XdgError::InvalidPositioner);
        }
        Ok(())
    }

    fn validate_anchor_bounds(&self) -> Result<(), XdgError> {
        let Some((width, height)) = self.parent_size else {
            return Ok(());
        };
        self.validate_anchor_with_size(width, height)
    }

    fn validate_anchor_with_size(&self, width: i32, height: i32) -> Result<(), XdgError> {
        let right = self.anchor_x.checked_add(self.anchor_width);
        let bottom = self.anchor_y.checked_add(self.anchor_height);
        if self.anchor_x < 0
            || self.anchor_y < 0
            || right.is_none_or(|right| right > width)
            || bottom.is_none_or(|bottom| bottom > height)
        {
            return Err(XdgError::InvalidPositioner);
        }
        Ok(())
    }

    /// Resolve copied positioner rules into constrained parent-relative geometry.
    pub fn compute_geometry(&self) -> PopupGeometry {
        self.compute_geometry_with_bounds(
            self.parent_size
                .map(|(width, height)| (0, 0, width, height)),
        )
    }

    pub fn compute_geometry_with_bounds(
        &self,
        bounds: Option<(i32, i32, i32, i32)>,
    ) -> PopupGeometry {
        let mut geometry = self.geometry_for(self.anchor, self.gravity);
        if let Some((bounds_x, bounds_y, bounds_width, bounds_height)) = bounds {
            let bounds_right = bounds_x.saturating_add(bounds_width);
            let bounds_bottom = bounds_y.saturating_add(bounds_height);
            if self.constraint_adjustment & CONSTRAINT_FLIP_X != 0
                && overflow_x(geometry, bounds_x, bounds_right) > 0
            {
                let flipped = self.geometry_for(flip_x(self.anchor), flip_x(self.gravity));
                if overflow_x(flipped, bounds_x, bounds_right)
                    < overflow_x(geometry, bounds_x, bounds_right)
                {
                    geometry.x = flipped.x;
                }
            }
            if self.constraint_adjustment & CONSTRAINT_FLIP_Y != 0
                && overflow_y(geometry, bounds_y, bounds_bottom) > 0
            {
                let flipped = self.geometry_for(flip_y(self.anchor), flip_y(self.gravity));
                if overflow_y(flipped, bounds_y, bounds_bottom)
                    < overflow_y(geometry, bounds_y, bounds_bottom)
                {
                    geometry.y = flipped.y;
                }
            }
            if self.constraint_adjustment & CONSTRAINT_SLIDE_X != 0 {
                geometry.x = geometry
                    .x
                    .clamp(bounds_x, (bounds_right - geometry.width).max(bounds_x));
            }
            if self.constraint_adjustment & CONSTRAINT_SLIDE_Y != 0 {
                geometry.y = geometry
                    .y
                    .clamp(bounds_y, (bounds_bottom - geometry.height).max(bounds_y));
            }
            if self.constraint_adjustment & CONSTRAINT_RESIZE_X != 0 {
                let left = geometry.x.max(bounds_x);
                let right = (geometry.x + geometry.width).min(bounds_right);
                geometry.x = left;
                geometry.width = (right - left).max(1);
            }
            if self.constraint_adjustment & CONSTRAINT_RESIZE_Y != 0 {
                let top = geometry.y.max(bounds_y);
                let bottom = (geometry.y + geometry.height).min(bounds_bottom);
                geometry.y = top;
                geometry.height = (bottom - top).max(1);
            }
        }
        let _ = self.parent_configure;
        geometry
    }

    fn geometry_for(&self, anchor: u32, gravity: u32) -> PopupGeometry {
        let (anchor_x, anchor_y) = anchor_point(
            anchor,
            self.anchor_x,
            self.anchor_y,
            self.anchor_width,
            self.anchor_height,
        );
        let (gravity_x, gravity_y) = gravity_offset(gravity, self.width, self.height);
        PopupGeometry {
            x: anchor_x + gravity_x + self.offset_x,
            y: anchor_y + gravity_y + self.offset_y,
            width: self.width,
            height: self.height,
        }
    }
}

/// xdg_positioner.anchor values from the protocol.
const ANCHOR_NONE: u32 = 0;
const ANCHOR_TOP: u32 = 1;
const ANCHOR_BOTTOM: u32 = 2;
const ANCHOR_LEFT: u32 = 3;
const ANCHOR_RIGHT: u32 = 4;
const ANCHOR_TOP_LEFT: u32 = 5;
const ANCHOR_BOTTOM_LEFT: u32 = 6;
const ANCHOR_TOP_RIGHT: u32 = 7;
const ANCHOR_BOTTOM_RIGHT: u32 = 8;
const CONSTRAINT_SLIDE_X: u32 = 1;
const CONSTRAINT_SLIDE_Y: u32 = 2;
const CONSTRAINT_FLIP_X: u32 = 4;
const CONSTRAINT_FLIP_Y: u32 = 8;
const CONSTRAINT_RESIZE_X: u32 = 16;
const CONSTRAINT_RESIZE_Y: u32 = 32;
const CONSTRAINT_ALL: u32 = 63;
pub const TOPLEVEL_STATE_MAXIMIZED: u32 = 1 << 0;
pub const TOPLEVEL_STATE_FULLSCREEN: u32 = 1 << 1;

fn anchor_point(anchor: u32, x: i32, y: i32, width: i32, height: i32) -> (i32, i32) {
    let mid_x = x + width / 2;
    let mid_y = y + height / 2;
    let right = x + width;
    let bottom = y + height;
    match anchor {
        ANCHOR_TOP => (mid_x, y),
        ANCHOR_BOTTOM => (mid_x, bottom),
        ANCHOR_LEFT => (x, mid_y),
        ANCHOR_RIGHT => (right, mid_y),
        ANCHOR_TOP_LEFT => (x, y),
        ANCHOR_BOTTOM_LEFT => (x, bottom),
        ANCHOR_TOP_RIGHT => (right, y),
        ANCHOR_BOTTOM_RIGHT => (right, bottom),
        ANCHOR_NONE | _ => (mid_x, mid_y),
    }
}

fn gravity_offset(gravity: u32, width: i32, height: i32) -> (i32, i32) {
    // Offset from the anchor point to the popup's top-left corner.
    match gravity {
        ANCHOR_TOP => (-width / 2, -height),
        ANCHOR_BOTTOM => (-width / 2, 0),
        ANCHOR_LEFT => (-width, -height / 2),
        ANCHOR_RIGHT => (0, -height / 2),
        ANCHOR_TOP_LEFT => (-width, -height),
        ANCHOR_BOTTOM_LEFT => (-width, 0),
        ANCHOR_TOP_RIGHT => (0, -height),
        ANCHOR_BOTTOM_RIGHT => (0, 0),
        ANCHOR_NONE | _ => (-width / 2, -height / 2),
    }
}

fn flip_x(value: u32) -> u32 {
    match value {
        ANCHOR_LEFT => ANCHOR_RIGHT,
        ANCHOR_RIGHT => ANCHOR_LEFT,
        ANCHOR_TOP_LEFT => ANCHOR_TOP_RIGHT,
        ANCHOR_BOTTOM_LEFT => ANCHOR_BOTTOM_RIGHT,
        ANCHOR_TOP_RIGHT => ANCHOR_TOP_LEFT,
        ANCHOR_BOTTOM_RIGHT => ANCHOR_BOTTOM_LEFT,
        other => other,
    }
}

fn flip_y(value: u32) -> u32 {
    match value {
        ANCHOR_TOP => ANCHOR_BOTTOM,
        ANCHOR_BOTTOM => ANCHOR_TOP,
        ANCHOR_TOP_LEFT => ANCHOR_BOTTOM_LEFT,
        ANCHOR_TOP_RIGHT => ANCHOR_BOTTOM_RIGHT,
        ANCHOR_BOTTOM_LEFT => ANCHOR_TOP_LEFT,
        ANCHOR_BOTTOM_RIGHT => ANCHOR_TOP_RIGHT,
        other => other,
    }
}

fn overflow_x(geometry: PopupGeometry, left: i32, right: i32) -> i32 {
    (left - geometry.x).max(0) + (geometry.x + geometry.width - right).max(0)
}

fn overflow_y(geometry: PopupGeometry, top: i32, bottom: i32) -> i32 {
    (top - geometry.y).max(0) + (geometry.y + geometry.height - bottom).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    fn client_id(id: u32) -> ClientId {
        ClientId::new(NonZeroU32::new(id).unwrap())
    }

    fn object_id(id: u32) -> ObjectId {
        ObjectId::new(NonZeroU32::new(id).unwrap())
    }

    fn complete_positioner() -> PositionerState {
        let mut positioner = PositionerState::default();
        positioner.set_size(20, 10).unwrap();
        positioner.set_anchor_rect(10, 10, 10, 10).unwrap();
        positioner
    }

    fn mapped_toplevel(
        manager: &mut XdgManager,
        client: ClientId,
        xdg: ObjectId,
        wl: ObjectId,
        toplevel: ObjectId,
    ) {
        manager.create_xdg_surface(client, xdg, wl).unwrap();
        manager
            .create_toplevel(client, toplevel, xdg, 800, 600)
            .unwrap();
        let initial = manager
            .on_wl_surface_commit_with_buffer(client, wl, Some(false))
            .initial_configure
            .unwrap();
        manager.ack_configure(client, xdg, initial.serial).unwrap();
        manager.on_wl_surface_commit_with_buffer(client, wl, Some(true));
        assert!(manager.is_mapped_xdg_surface(client, xdg));
    }

    #[test]
    fn rejected_buffer_before_initial_ack_does_not_advance_state() {
        let mut manager = XdgManager::default();
        let client = client_id(1);
        let xdg = object_id(2);
        let wl = object_id(3);
        manager.create_xdg_surface(client, xdg, wl).unwrap();
        manager
            .create_toplevel(client, object_id(4), xdg, 800, 600)
            .unwrap();

        assert_eq!(
            manager.check_buffer_commit(client, wl, true),
            Err(XdgError::UnconfiguredBuffer)
        );
        assert!(!manager.can_map_wl_surface(client, wl));
        let outcome = manager.on_wl_surface_commit_with_buffer(client, wl, None);
        assert!(outcome.initial_configure.is_some());
        assert!(!manager.can_map_wl_surface(client, wl));
    }

    #[test]
    fn new_buffer_waits_for_latest_configure_ack() {
        let mut manager = XdgManager::default();
        let client = client_id(1);
        let xdg = object_id(2);
        let wl = object_id(3);
        let toplevel = object_id(4);
        mapped_toplevel(&mut manager, client, xdg, wl, toplevel);
        let configure = manager.toplevel_configure(client, toplevel).unwrap();
        assert_eq!(
            manager.check_buffer_commit(client, wl, true),
            Err(XdgError::UnconfiguredBuffer)
        );
        manager
            .ack_configure(client, xdg, configure.serial)
            .unwrap();
        assert_eq!(manager.check_buffer_commit(client, wl, true), Ok(()));
    }

    #[test]
    fn pre_initial_toplevel_requests_are_folded_into_initial_configure() {
        let mut manager = XdgManager::default();
        let client = client_id(1);
        let xdg = object_id(2);
        let wl = object_id(3);
        let toplevel = object_id(4);
        manager.create_xdg_surface(client, xdg, wl).unwrap();
        manager
            .create_toplevel(client, toplevel, xdg, 800, 600)
            .unwrap();
        assert_eq!(
            manager
                .set_toplevel_maximized(client, toplevel, true)
                .unwrap(),
            None
        );
        let initial = manager
            .on_wl_surface_commit_with_buffer(client, wl, None)
            .initial_configure
            .unwrap();
        assert!(matches!(
            initial.payload,
            ConfigurePayload::Toplevel {
                states: TOPLEVEL_STATE_MAXIMIZED,
                ..
            }
        ));
    }

    #[test]
    fn positioner_bottom_gravity_places_popup_below_anchor() {
        let mut positioner = PositionerState::default();
        positioner.set_size(200, 100).unwrap();
        positioner.set_anchor_rect(10, 20, 40, 16).unwrap();
        positioner.set_anchor(ANCHOR_BOTTOM).unwrap();
        positioner.set_gravity(ANCHOR_BOTTOM).unwrap();
        positioner.set_offset(0, 0);

        let geo = positioner.compute_geometry();
        // Anchor at bottom-center of rect: (10+20, 20+16) = (30, 36)
        // Bottom gravity: top-left at (30 - 100, 36) = (-70, 36)
        assert_eq!(
            geo,
            PopupGeometry {
                x: -70,
                y: 36,
                width: 200,
                height: 100,
            }
        );
    }

    #[test]
    fn positioner_configure_uses_requested_size() {
        let mut positioner = PositionerState::default();
        positioner.set_size(205, 399).unwrap();
        positioner.set_anchor_rect(0, 0, 10, 10).unwrap();
        let geo = positioner.compute_geometry();
        assert_eq!(geo.width, 205);
        assert_eq!(geo.height, 399);
    }

    #[test]
    fn positioner_uses_offset() {
        let mut positioner = PositionerState::default();
        positioner.set_size(10, 10).unwrap();
        positioner.set_anchor_rect(0, 0, 1, 1).unwrap();
        positioner.set_anchor(ANCHOR_TOP_LEFT).unwrap();
        positioner.set_gravity(ANCHOR_BOTTOM_RIGHT).unwrap();
        positioner.set_offset(5, 7);
        assert_eq!(
            positioner.compute_geometry(),
            PopupGeometry {
                x: 5,
                y: 7,
                width: 10,
                height: 10,
            }
        );
    }

    #[test]
    fn positioner_rejects_invalid_input_and_incomplete_state() {
        let mut positioner = PositionerState::default();
        assert_eq!(
            positioner.set_size(0, 1),
            Err(XdgError::InvalidPositionerInput)
        );
        assert_eq!(
            positioner.set_anchor_rect(0, 0, -1, 1),
            Err(XdgError::InvalidPositionerInput)
        );
        assert_eq!(
            positioner.set_anchor(9),
            Err(XdgError::InvalidPositionerInput)
        );
        assert_eq!(
            positioner.set_gravity(9),
            Err(XdgError::InvalidPositionerInput)
        );
        assert_eq!(
            positioner.set_constraint_adjustment(64),
            Err(XdgError::InvalidPositionerInput)
        );
        assert_eq!(
            positioner.validate_complete(),
            Err(XdgError::InvalidPositioner)
        );
        let mut out_of_bounds = complete_positioner();
        out_of_bounds.set_parent_size(15, 15).unwrap();
        assert_eq!(
            out_of_bounds.validate_anchor_bounds(),
            Err(XdgError::InvalidPositioner)
        );
    }

    #[test]
    fn constraints_apply_flip_then_slide_then_resize() {
        let mut flip = complete_positioner();
        flip.set_anchor_rect(95, 40, 5, 5).unwrap();
        flip.set_anchor(ANCHOR_RIGHT).unwrap();
        flip.set_gravity(ANCHOR_RIGHT).unwrap();
        flip.set_parent_size(100, 100).unwrap();
        flip.set_constraint_adjustment(CONSTRAINT_FLIP_X).unwrap();
        assert_eq!(flip.compute_geometry().x, 75);

        let mut slide = complete_positioner();
        slide.set_offset(-30, 100);
        slide.set_parent_size(50, 40).unwrap();
        slide
            .set_constraint_adjustment(CONSTRAINT_SLIDE_X | CONSTRAINT_SLIDE_Y)
            .unwrap();
        let geometry = slide.compute_geometry();
        assert_eq!((geometry.x, geometry.y), (0, 30));

        let mut resize = complete_positioner();
        resize.set_size(80, 70).unwrap();
        resize.set_offset(-20, -20);
        resize.set_parent_size(50, 40).unwrap();
        resize
            .set_constraint_adjustment(CONSTRAINT_RESIZE_X | CONSTRAINT_RESIZE_Y)
            .unwrap();
        let geometry = resize.compute_geometry();
        assert_eq!(
            geometry,
            PopupGeometry {
                x: 0,
                y: 0,
                width: 35,
                height: 30
            }
        );
    }

    #[test]
    fn window_geometry_and_ack_are_applied_only_on_commit() {
        let client = client_id(1);
        let (xdg, wl, top) = (object_id(2), object_id(3), object_id(4));
        let mut manager = XdgManager::default();
        manager.create_xdg_surface(client, xdg, wl).unwrap();
        manager.create_toplevel(client, top, xdg, 640, 480).unwrap();
        manager
            .set_window_geometry(client, xdg, 5, 7, 100, 80)
            .unwrap();
        assert_eq!(manager.current_window_geometry(client, xdg).unwrap(), None);

        let initial = manager
            .on_wl_surface_commit_with_buffer(client, wl, Some(false))
            .initial_configure
            .unwrap();
        assert_eq!(
            manager.current_window_geometry(client, xdg).unwrap(),
            Some(WindowGeometry {
                x: 5,
                y: 7,
                width: 100,
                height: 80
            })
        );
        manager.ack_configure(client, xdg, initial.serial).unwrap();
        assert!(!manager.can_map_wl_surface(client, wl));
        let outcome = manager.on_wl_surface_commit_with_buffer(client, wl, Some(true));
        assert_eq!(outcome.applied_configure, Some(initial));
        assert!(manager.can_map_wl_surface(client, wl));
    }

    #[test]
    fn configure_snapshots_preserve_state_and_ack_consumes_older_serials() {
        let client = client_id(1);
        let (xdg, wl, top) = (object_id(2), object_id(3), object_id(4));
        let mut manager = XdgManager::default();
        manager.create_xdg_surface(client, xdg, wl).unwrap();
        manager.create_toplevel(client, top, xdg, 640, 480).unwrap();
        let first = manager.toplevel_configure(client, top).unwrap();
        let maximized = manager
            .set_toplevel_maximized(client, top, true)
            .unwrap()
            .unwrap();
        assert_eq!(
            first.payload,
            ConfigurePayload::Toplevel {
                width: 640,
                height: 480,
                states: 0
            }
        );
        assert_eq!(
            maximized.payload,
            ConfigurePayload::Toplevel {
                width: 640,
                height: 480,
                states: TOPLEVEL_STATE_MAXIMIZED,
            }
        );
        let fullscreen = manager
            .set_toplevel_fullscreen(client, top, true)
            .unwrap()
            .unwrap();
        assert_eq!(
            fullscreen.payload,
            ConfigurePayload::Toplevel {
                width: 640,
                height: 480,
                states: TOPLEVEL_STATE_FULLSCREEN,
            }
        );
        manager
            .ack_configure(client, xdg, fullscreen.serial)
            .unwrap();
        assert_eq!(
            manager.ack_configure(client, xdg, first.serial),
            Err(XdgError::InvalidSerial)
        );
    }

    #[test]
    fn lifecycle_and_parent_errors_are_enforced() {
        let client = client_id(1);
        let (wm, xdg, wl, top) = (object_id(2), object_id(3), object_id(4), object_id(5));
        let mut manager = XdgManager::default();
        manager.create_wm_base(client, wm);
        manager
            .create_xdg_surface_owned(client, wm, xdg, wl)
            .unwrap();
        assert_eq!(
            manager.destroy_wm_base(client, wm),
            Err(XdgError::DefunctSurfaces)
        );
        manager.create_toplevel(client, top, xdg, 10, 10).unwrap();
        assert_eq!(
            manager.destroy_xdg_surface(client, xdg),
            Err(XdgError::DefunctRoleObject)
        );
        assert_eq!(
            manager.set_toplevel_parent(client, top, Some(top)),
            Err(XdgError::InvalidParent)
        );
        manager.destroy_toplevel(client, top).unwrap();
        manager.destroy_xdg_surface(client, xdg).unwrap();
        manager.destroy_wm_base(client, wm).unwrap();
    }

    #[test]
    fn popup_copies_positioner_and_reposition_applies_after_ack_commit() {
        let client = client_id(1);
        let mut manager = XdgManager::default();
        mapped_toplevel(
            &mut manager,
            client,
            object_id(2),
            object_id(3),
            object_id(4),
        );
        let (popup_xdg, popup_wl, popup, positioner) =
            (object_id(5), object_id(6), object_id(7), object_id(8));
        manager
            .create_xdg_surface(client, popup_xdg, popup_wl)
            .unwrap();
        manager.create_positioner(client, positioner);
        manager
            .positioner_set_size(client, positioner, 20, 10)
            .unwrap();
        manager
            .positioner_set_anchor_rect(client, positioner, 10, 10, 10, 10)
            .unwrap();
        let copied = manager
            .create_popup(client, popup, popup_xdg, object_id(2), positioner)
            .unwrap();
        manager
            .positioner_set_offset(client, positioner, 100, 100)
            .unwrap();
        let initial = manager
            .on_wl_surface_commit_with_buffer(client, popup_wl, Some(false))
            .initial_configure
            .unwrap();
        assert_eq!(
            initial.payload,
            ConfigurePayload::Popup {
                geometry: copied,
                reposition_token: None
            }
        );
        manager
            .ack_configure(client, popup_xdg, initial.serial)
            .unwrap();
        manager.on_wl_surface_commit_with_buffer(client, popup_wl, Some(true));

        let (serial, repositioned, _) = manager
            .reposition_popup(client, popup, positioner, 42)
            .unwrap();
        assert_eq!(
            manager
                .popups
                .get(&(client, popup))
                .unwrap()
                .current_geometry,
            Some(copied)
        );
        manager.ack_configure(client, popup_xdg, serial).unwrap();
        manager.on_wl_surface_commit_with_buffer(client, popup_wl, Some(true));
        assert_eq!(
            manager
                .popups
                .get(&(client, popup))
                .unwrap()
                .current_geometry,
            Some(repositioned)
        );
    }

    #[test]
    fn popup_requires_mapped_parent_and_topmost_destruction_order() {
        let client = client_id(1);
        let mut manager = XdgManager::default();
        manager
            .create_xdg_surface(client, object_id(2), object_id(3))
            .unwrap();
        manager
            .create_toplevel(client, object_id(4), object_id(2), 100, 100)
            .unwrap();
        manager.create_positioner(client, object_id(8));
        manager
            .positioner_set_size(client, object_id(8), 20, 10)
            .unwrap();
        manager
            .positioner_set_anchor_rect(client, object_id(8), 1, 1, 2, 2)
            .unwrap();
        manager
            .create_xdg_surface(client, object_id(5), object_id(6))
            .unwrap();
        assert_eq!(
            manager.create_popup(
                client,
                object_id(7),
                object_id(5),
                object_id(2),
                object_id(8)
            ),
            Err(XdgError::InvalidPopupParent)
        );

        let initial = manager
            .on_wl_surface_commit_with_buffer(client, object_id(3), Some(false))
            .initial_configure
            .unwrap();
        manager
            .ack_configure(client, object_id(2), initial.serial)
            .unwrap();
        manager.on_wl_surface_commit_with_buffer(client, object_id(3), Some(true));
        manager
            .create_popup(
                client,
                object_id(7),
                object_id(5),
                object_id(2),
                object_id(8),
            )
            .unwrap();
        let popup_initial = manager
            .on_wl_surface_commit_with_buffer(client, object_id(6), Some(false))
            .initial_configure
            .unwrap();
        manager
            .ack_configure(client, object_id(5), popup_initial.serial)
            .unwrap();
        manager.on_wl_surface_commit_with_buffer(client, object_id(6), Some(true));

        manager
            .create_xdg_surface(client, object_id(9), object_id(10))
            .unwrap();
        manager
            .create_popup(
                client,
                object_id(11),
                object_id(9),
                object_id(5),
                object_id(8),
            )
            .unwrap();
        assert_eq!(
            manager.destroy_popup(client, object_id(7)),
            Err(XdgError::NotTopmostPopup)
        );
        manager.destroy_popup(client, object_id(11)).unwrap();
        manager.destroy_popup(client, object_id(7)).unwrap();
    }
}
