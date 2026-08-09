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
    anchor_x: i32,
    anchor_y: i32,
    anchor_width: i32,
    anchor_height: i32,
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
            anchor_x: 0,
            anchor_y: 0,
            anchor_width: 0,
            anchor_height: 0,
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
    wl_surface: ObjectId,
    role: Option<XdgRole>,
    window_geometry: Option<(i32, i32, i32, i32)>,
    /// Serials that have been sent and not yet consumed by ack.
    pending_configures: Vec<u32>,
    /// Last acked configure serial (committed on next surface commit).
    acked_serial: Option<u32>,
    /// Serial applied on the last successful commit with an ack.
    last_acked_committed: Option<u32>,
    initial_configure_sent: bool,
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
}

#[derive(Debug)]
struct PopupState {
    xdg_surface: ObjectId,
    #[allow(dead_code)]
    parent: ObjectId,
    #[allow(dead_code)]
    positioner: ObjectId,
}

impl XdgManager {
    pub fn create_wm_base(&mut self, client_id: ClientId, id: ObjectId) {
        self.wm_bases.insert((client_id, id), ());
    }

    pub fn destroy_wm_base(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
    ) -> Result<(), XdgError> {
        self.wm_bases
            .remove(&(client_id, id))
            .ok_or(XdgError::UnknownWmBase)?;
        Ok(())
    }

    pub fn create_positioner(&mut self, client_id: ClientId, id: ObjectId) {
        self.positioners
            .insert((client_id, id), PositionerState::default());
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
        self.positioner_mut(client_id, id)?.set_size(width, height);
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
            .set_anchor_rect(x, y, width, height);
        Ok(())
    }

    pub fn positioner_set_anchor(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        anchor: u32,
    ) -> Result<(), XdgError> {
        self.positioner_mut(client_id, id)?.set_anchor(anchor);
        Ok(())
    }

    pub fn positioner_set_gravity(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        gravity: u32,
    ) -> Result<(), XdgError> {
        self.positioner_mut(client_id, id)?.set_gravity(gravity);
        Ok(())
    }

    pub fn positioner_set_constraint_adjustment(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        adjustment: u32,
    ) -> Result<(), XdgError> {
        self.positioner_mut(client_id, id)?
            .set_constraint_adjustment(adjustment);
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
            .set_parent_size(width, height);
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
        if self.surface_to_xdg.contains_key(&(client_id, wl_surface)) {
            return Err(XdgError::RoleConflict);
        }
        self.xdg_surfaces.insert(
            (client_id, xdg_surface_id),
            XdgSurfaceState {
                wl_surface,
                role: None,
                window_geometry: None,
                pending_configures: Vec::new(),
                acked_serial: None,
                last_acked_committed: None,
                initial_configure_sent: false,
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
            .remove(&(client_id, xdg_surface_id))
            .ok_or(XdgError::UnknownXdgSurface)?;
        self.surface_to_xdg.remove(&(client_id, state.wl_surface));
        if let Some(XdgRole::Toplevel(toplevel)) = state.role {
            self.toplevels.remove(&(client_id, toplevel));
        }
        if let Some(XdgRole::Popup(popup)) = state.role {
            self.popups.remove(&(client_id, popup));
        }
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

    pub fn create_toplevel(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
        xdg_surface_id: ObjectId,
        configure_width: i32,
        configure_height: i32,
    ) -> Result<u32, XdgError> {
        let surface = self
            .xdg_surfaces
            .get_mut(&(client_id, xdg_surface_id))
            .ok_or(XdgError::UnknownXdgSurface)?;
        if surface.role.is_some() {
            return Err(XdgError::AlreadyConstructed);
        }
        surface.role = Some(XdgRole::Toplevel(toplevel_id));
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
            },
        );
        self.send_configure_serial(client_id, xdg_surface_id)
    }

    /// Allocates a configure serial and records it as pending. Caller must emit events.
    pub fn send_configure_serial(
        &mut self,
        client_id: ClientId,
        xdg_surface_id: ObjectId,
    ) -> Result<u32, XdgError> {
        let serial = {
            self.next_configure_serial = self.next_configure_serial.wrapping_add(1).max(1);
            self.next_configure_serial
        };
        let surface = self
            .xdg_surfaces
            .get_mut(&(client_id, xdg_surface_id))
            .ok_or(XdgError::UnknownXdgSurface)?;
        surface.pending_configures.push(serial);
        surface.initial_configure_sent = true;
        Ok(serial)
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
        if let Some(surface) = self.xdg_surfaces.get_mut(&(client_id, toplevel.xdg_surface)) {
            if surface.role == Some(XdgRole::Toplevel(toplevel_id)) {
                surface.role = None;
            }
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
        self.toplevels
            .get_mut(&(client_id, toplevel_id))
            .ok_or(XdgError::UnknownToplevel)?
            .parent = parent;
        Ok(())
    }

    pub fn set_toplevel_min_size(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
        width: i32,
        height: i32,
    ) -> Result<(), XdgError> {
        self.toplevels
            .get_mut(&(client_id, toplevel_id))
            .ok_or(XdgError::UnknownToplevel)?
            .min_size = (width, height);
        Ok(())
    }

    pub fn set_toplevel_max_size(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
        width: i32,
        height: i32,
    ) -> Result<(), XdgError> {
        self.toplevels
            .get_mut(&(client_id, toplevel_id))
            .ok_or(XdgError::UnknownToplevel)?
            .max_size = (width, height);
        Ok(())
    }

    pub fn create_popup(
        &mut self,
        client_id: ClientId,
        popup_id: ObjectId,
        xdg_surface_id: ObjectId,
        parent: ObjectId,
        positioner: ObjectId,
    ) -> Result<u32, XdgError> {
        if !self.positioners.contains_key(&(client_id, positioner)) {
            return Err(XdgError::UnknownPositioner);
        }
        let surface = self
            .xdg_surfaces
            .get_mut(&(client_id, xdg_surface_id))
            .ok_or(XdgError::UnknownXdgSurface)?;
        if surface.role.is_some() {
            return Err(XdgError::AlreadyConstructed);
        }
        surface.role = Some(XdgRole::Popup(popup_id));
        self.popups.insert(
            (client_id, popup_id),
            PopupState {
                xdg_surface: xdg_surface_id,
                parent,
                positioner,
            },
        );
        // Consume positioner copy semantics: leave positioner object alive but ok.
        let _ = parent;
        self.send_configure_serial(client_id, xdg_surface_id)
    }

    pub fn destroy_popup(
        &mut self,
        client_id: ClientId,
        popup_id: ObjectId,
    ) -> Result<ObjectId, XdgError> {
        let popup = self
            .popups
            .remove(&(client_id, popup_id))
            .ok_or(XdgError::UnknownPopup)?;
        if let Some(surface) = self.xdg_surfaces.get_mut(&(client_id, popup.xdg_surface)) {
            if surface.role == Some(XdgRole::Popup(popup_id)) {
                surface.role = None;
            }
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
        self.xdg_surfaces
            .get_mut(&(client_id, xdg_surface_id))
            .ok_or(XdgError::UnknownXdgSurface)?
            .window_geometry = Some((x, y, width, height));
        Ok(())
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
        if !surface.pending_configures.contains(&serial) {
            return Err(XdgError::InvalidSerial);
        }
        // Consume this serial and all older ones.
        surface
            .pending_configures
            .retain(|pending| *pending > serial);
        surface.acked_serial = Some(serial);
        Ok(())
    }

    /// Returns whether the wl_surface may map (has role + acked configure).
    pub fn can_map_wl_surface(&self, client_id: ClientId, wl_surface: ObjectId) -> bool {
        let Some(xdg_id) = self.surface_to_xdg.get(&(client_id, wl_surface)) else {
            return false;
        };
        let Some(surface) = self.xdg_surfaces.get(&(client_id, *xdg_id)) else {
            return false;
        };
        surface.role.is_some()
            && (surface.acked_serial.is_some() || surface.last_acked_committed.is_some())
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
        if surface.role.is_none() {
            return Err(XdgError::NotConstructed);
        }
        if attaching_buffer
            && surface.last_acked_committed.is_none()
            && surface.acked_serial.is_none()
        {
            return Err(XdgError::UnconfiguredBuffer);
        }
        Ok(())
    }

    /// Apply pending ack on commit.
    pub fn on_wl_surface_commit(&mut self, client_id: ClientId, wl_surface: ObjectId) {
        let Some(xdg_id) = self.surface_to_xdg.get(&(client_id, wl_surface)).copied() else {
            return;
        };
        if let Some(surface) = self.xdg_surfaces.get_mut(&(client_id, xdg_id))
            && let Some(serial) = surface.acked_serial.take()
        {
            surface.last_acked_committed = Some(serial);
        }
    }

    pub fn is_xdg_toplevel_mapped_role(
        &self,
        client_id: ClientId,
        wl_surface: ObjectId,
    ) -> bool {
        self.can_map_wl_surface(client_id, wl_surface)
    }

    pub fn delete_client(&mut self, client_id: ClientId) {
        self.wm_bases.retain(|(owner, _), _| *owner != client_id);
        self.positioners.retain(|(owner, _), _| *owner != client_id);
        self.xdg_surfaces.retain(|(owner, _), _| *owner != client_id);
        self.toplevels.retain(|(owner, _), _| *owner != client_id);
        self.popups.retain(|(owner, _), _| *owner != client_id);
        self.surface_to_xdg
            .retain(|(owner, _), _| *owner != client_id);
    }
}

impl PositionerState {
    pub fn set_size(&mut self, width: i32, height: i32) {
        self.width = width;
        self.height = height;
    }

    pub fn set_anchor_rect(&mut self, x: i32, y: i32, width: i32, height: i32) {
        self.anchor_x = x;
        self.anchor_y = y;
        self.anchor_width = width;
        self.anchor_height = height;
    }

    pub fn set_anchor(&mut self, anchor: u32) {
        self.anchor = anchor;
    }

    pub fn set_gravity(&mut self, gravity: u32) {
        self.gravity = gravity;
    }

    pub fn set_constraint_adjustment(&mut self, adjustment: u32) {
        self.constraint_adjustment = adjustment;
    }

    pub fn set_offset(&mut self, x: i32, y: i32) {
        self.offset_x = x;
        self.offset_y = y;
    }

    pub fn set_reactive(&mut self, reactive: bool) {
        self.reactive = reactive;
    }

    pub fn set_parent_size(&mut self, width: i32, height: i32) {
        self.parent_size = Some((width, height));
    }

    pub fn set_parent_configure(&mut self, serial: u32) {
        self.parent_configure = Some(serial);
    }
}
