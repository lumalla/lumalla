use std::collections::HashMap;

use lumalla_wayland_protocol::{ClientId, ObjectId};

type ResourceKey = (ClientId, ObjectId);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceError {
    UnknownSurface,
    UnknownBuffer,
    UnknownShellSurface,
    UnknownRegion,
    UnknownSubsurface,
    RoleAlreadyAssigned,
    BadParent,
    BadSurface,
    InvalidScale,
    InvalidTransform,
    InvalidOffset,
    ViewportExists,
    NoSurface,
    ViewportBadValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewportCommitError {
    BadSize,
    OutOfBuffer,
}

/// Crop and scale state from wp_viewport (committed).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ViewportState {
    /// Source rectangle in post-scale surface coordinates, or unset.
    pub source: Option<(f32, f32, f32, f32)>,
    /// Destination surface size, or unset.
    pub destination: Option<(i32, i32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellMode {
    None,
    Toplevel,
    Transient,
    Fullscreen,
    Popup,
    Maximized,
}

#[derive(Debug)]
pub struct SurfaceCommit {
    pub surface_id: ObjectId,
    #[allow(dead_code)]
    pub buffer: Option<ObjectId>,
    pub attached_buffer: Option<Option<ObjectId>>,
    pub mapped: bool,
    /// True when this commit transitioned the surface from unmapped to mapped.
    pub newly_mapped: bool,
    pub shell_id: Option<ObjectId>,
    pub frame_callbacks: Vec<ObjectId>,
    pub presentation_feedbacks: Vec<ObjectId>,
    /// True when this commit was cached for a synchronized subsurface (not applied yet).
    pub deferred: bool,
    pub buffer_scale: i32,
    pub buffer_transform: u32,
    pub offset: (i32, i32),
    pub layout: (i32, i32),
    #[allow(dead_code)]
    pub damage: Vec<Rectangle>,
    #[allow(dead_code)]
    pub buffer_damage: Vec<Rectangle>,
    pub viewport: ViewportState,
    #[allow(dead_code)]
    pub viewport_id: Option<ObjectId>,
    /// True when viewport source/destination pending was applied this commit.
    pub viewport_changed: bool,
}

#[derive(Debug)]
pub struct CommitResult {
    pub primary: SurfaceCommit,
    pub synchronized_children: Vec<SurfaceCommit>,
}

#[derive(Debug)]
pub struct DestroyedSurface {
    pub shell_id: Option<ObjectId>,
    pub subsurface_id: Option<ObjectId>,
    pub xdg_surface_id: Option<ObjectId>,
    pub orphaned_subsurface_ids: Vec<ObjectId>,
    pub callbacks: Vec<ObjectId>,
    pub presentation_feedbacks: Vec<ObjectId>,
    pub was_mapped: bool,
}

#[derive(Debug, Default)]
pub struct SurfaceManager {
    surfaces: HashMap<ResourceKey, Surface>,
    shell_surfaces: HashMap<ResourceKey, ObjectId>,
    subsurfaces: HashMap<ResourceKey, SubsurfaceState>,
    regions: HashMap<ResourceKey, Region>,
    /// wp_viewport object → surface binding (surface may be destroyed).
    viewports: HashMap<ResourceKey, ViewportBinding>,
    next_cascade: i32,
    /// Mapped surfaces in paint order (back to front).
    paint_order: Vec<(ClientId, ObjectId)>,
}

#[derive(Debug, Clone, Copy)]
struct ViewportBinding {
    surface_id: ObjectId,
    surface_alive: bool,
}

impl SurfaceManager {
    pub fn create_surface(&mut self, client_id: ClientId, id: ObjectId) {
        self.surfaces.insert((client_id, id), Surface::default());
    }

    pub fn destroy_surface(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
    ) -> Result<DestroyedSurface, SurfaceError> {
        let was_mapped = self.is_mapped(client_id, id)?;
        let surface = self
            .surfaces
            .remove(&(client_id, id))
            .ok_or(SurfaceError::UnknownSurface)?;

        let mut orphaned_subsurface_ids = Vec::new();
        let children: Vec<ObjectId> = surface
            .pending_children
            .as_ref()
            .unwrap_or(&surface.current_children)
            .clone();
        for child_surface_id in children {
            if let Some(child) = self.surfaces.get_mut(&(client_id, child_surface_id)) {
                if let Some(Role::Subsurface(sub_id)) = child.role.take() {
                    self.subsurfaces.remove(&(client_id, sub_id));
                    orphaned_subsurface_ids.push(sub_id);
                }
            }
        }

        let (shell_id, subsurface_id, xdg_surface_id) = match surface.role {
            Some(Role::Shell(shell_id)) => {
                self.shell_surfaces.remove(&(client_id, shell_id));
                (Some(shell_id), None, None)
            }
            Some(Role::Subsurface(subsurface_id)) => {
                if let Some(sub) = self.subsurfaces.remove(&(client_id, subsurface_id)) {
                    self.remove_child_from_parent(client_id, sub.parent, id);
                }
                (None, Some(subsurface_id), None)
            }
            Some(Role::Xdg(xdg_surface_id)) => (None, None, Some(xdg_surface_id)),
            Some(Role::Cursor) | Some(Role::DndIcon) | None => (None, None, None),
        };

        let mut callbacks = surface.pending.frame_callbacks;
        let mut presentation_feedbacks = surface.pending.presentation_feedbacks;
        if let Some(cache) = surface.cache {
            callbacks.extend(cache.frame_callbacks);
            presentation_feedbacks.extend(cache.presentation_feedbacks);
        }

        if let Some(viewport_id) = surface.viewport_id
            && let Some(binding) = self.viewports.get_mut(&(client_id, viewport_id))
        {
            binding.surface_alive = false;
        }

        self.remove_painted_surface(client_id, id);

        Ok(DestroyedSurface {
            shell_id,
            subsurface_id,
            xdg_surface_id,
            orphaned_subsurface_ids,
            callbacks,
            presentation_feedbacks,
            was_mapped,
        })
    }

    pub fn first_surface(&self, client_id: ClientId) -> Option<ObjectId> {
        let ids: Vec<ObjectId> = self
            .surfaces
            .keys()
            .filter_map(|(owner, id)| (*owner == client_id).then_some(*id))
            .collect();
        ids.into_iter()
            .find(|id| self.is_mapped(client_id, *id).unwrap_or(false))
    }

    /// Global coordinates → surface-local coordinates for a mapped surface.
    pub fn surface_local_coords(
        &self,
        client_id: ClientId,
        surface_id: ObjectId,
        global_x: f64,
        global_y: f64,
    ) -> Option<(f32, f32)> {
        let (origin_x, origin_y) = self.surface_origin(client_id, surface_id)?;
        Some((
            (global_x - origin_x as f64) as f32,
            (global_y - origin_y as f64) as f32,
        ))
    }

    pub fn record_painted_surface(&mut self, client_id: ClientId, surface_id: ObjectId) {
        self.paint_order
            .retain(|entry| *entry != (client_id, surface_id));
        self.paint_order.push((client_id, surface_id));
    }

    pub fn remove_painted_surface(&mut self, client_id: ClientId, surface_id: ObjectId) {
        self.paint_order
            .retain(|entry| *entry != (client_id, surface_id));
    }

    /// Geometry hit-test for a client: top-most mapped surface containing (x, y).
    pub fn pointer_target(&self, client_id: ClientId, x: f64, y: f64) -> Option<ObjectId> {
        self.hit_test(Some(client_id), x, y)
            .filter(|(owner, _)| *owner == client_id)
            .map(|(_, surface)| surface)
    }

    /// Geometry hit-test across clients (top-most first).
    ///
    /// When `preferred_client` is set, only that client's surfaces are considered.
    pub fn global_pointer_target(
        &self,
        preferred_client: Option<ClientId>,
        x: f64,
        y: f64,
    ) -> Option<(ClientId, ObjectId)> {
        self.hit_test(preferred_client, x, y)
    }

    fn hit_test(
        &self,
        preferred_client: Option<ClientId>,
        x: f64,
        y: f64,
    ) -> Option<(ClientId, ObjectId)> {
        for &(client_id, surface_id) in self.paint_order.iter().rev() {
            if let Some(preferred) = preferred_client
                && client_id != preferred
            {
                continue;
            }
            if !self.is_mapped(client_id, surface_id).unwrap_or(false) {
                continue;
            }
            let Some(surface) = self.surfaces.get(&(client_id, surface_id)) else {
                continue;
            };
            // Include shell/xdg tops and their subsurfaces; skip cursor/dnd icons.
            if !matches!(
                surface.role,
                Some(Role::Shell(_)) | Some(Role::Xdg(_)) | Some(Role::Subsurface(_))
            ) {
                continue;
            }
            let Some((bw, bh)) = surface.buffer_size else {
                continue;
            };
            let scale = surface.current.buffer_scale.max(1);
            let Some((width, height)) =
                effective_surface_size(Some((bw, bh)), scale, &surface.current.viewport)
            else {
                continue;
            };
            let Some((origin_x, origin_y)) = self.surface_origin(client_id, surface_id) else {
                continue;
            };
            if x < origin_x as f64
                || y < origin_y as f64
                || x >= (origin_x + width) as f64
                || y >= (origin_y + height) as f64
            {
                continue;
            }
            let local_x = (x - origin_x as f64) as i32;
            let local_y = (y - origin_y as f64) as i32;
            if !self.surface_accepts_input_at(surface, local_x, local_y) {
                continue;
            }
            return Some((client_id, surface_id));
        }
        None
    }

    fn surface_origin(&self, client_id: ClientId, surface_id: ObjectId) -> Option<(i32, i32)> {
        let surface = self.surfaces.get(&(client_id, surface_id))?;
        let (layout_x, layout_y) = surface.layout;
        let (offset_x, offset_y) = match surface.role {
            Some(Role::Subsurface(sub_id)) => self
                .subsurfaces
                .get(&(client_id, sub_id))
                .map(|sub| sub.current_position)
                .unwrap_or(surface.current.offset),
            _ => surface.current.offset,
        };
        Some((layout_x + offset_x, layout_y + offset_y))
    }

    fn surface_accepts_input_at(&self, surface: &Surface, local_x: i32, local_y: i32) -> bool {
        match &surface.current.input_region {
            None => true,
            Some(region) => region.contains(local_x, local_y),
        }
    }

    pub fn assign_xdg_role(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
        xdg_surface_id: ObjectId,
    ) -> Result<(), SurfaceError> {
        let surface = self
            .surfaces
            .get_mut(&(client_id, surface_id))
            .ok_or(SurfaceError::UnknownSurface)?;
        if surface.role.is_some() {
            return Err(SurfaceError::RoleAlreadyAssigned);
        }
        surface.role = Some(Role::Xdg(xdg_surface_id));
        surface.xdg_map_ready = false;
        Ok(())
    }

    pub fn clear_xdg_role(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
    ) -> Result<(), SurfaceError> {
        let surface = self
            .surfaces
            .get_mut(&(client_id, surface_id))
            .ok_or(SurfaceError::UnknownSurface)?;
        if matches!(surface.role, Some(Role::Xdg(_))) {
            surface.role = None;
            surface.xdg_map_ready = false;
        }
        Ok(())
    }

    pub fn set_xdg_map_ready(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
        ready: bool,
    ) -> Result<(), SurfaceError> {
        let surface = self
            .surfaces
            .get_mut(&(client_id, surface_id))
            .ok_or(SurfaceError::UnknownSurface)?;
        surface.xdg_map_ready = ready;
        Ok(())
    }

    pub fn set_committed_buffer_size(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
        width: i32,
        height: i32,
    ) -> Result<(), SurfaceError> {
        let surface = self
            .surfaces
            .get_mut(&(client_id, surface_id))
            .ok_or(SurfaceError::UnknownSurface)?;
        surface.buffer_size = Some((width, height));
        Ok(())
    }

    pub fn clear_committed_buffer_size(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
    ) -> Result<(), SurfaceError> {
        let surface = self
            .surfaces
            .get_mut(&(client_id, surface_id))
            .ok_or(SurfaceError::UnknownSurface)?;
        surface.buffer_size = None;
        Ok(())
    }

    pub fn committed_buffer_size(
        &self,
        client_id: ClientId,
        surface_id: ObjectId,
    ) -> Option<(i32, i32)> {
        self.surfaces
            .get(&(client_id, surface_id))
            .and_then(|surface| surface.buffer_size)
    }

    pub fn set_surface_layout(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
        x: i32,
        y: i32,
    ) -> Result<(), SurfaceError> {
        let surface = self
            .surfaces
            .get_mut(&(client_id, surface_id))
            .ok_or(SurfaceError::UnknownSurface)?;
        surface.layout = (x, y);
        Ok(())
    }

    pub fn surface_layout(&self, client_id: ClientId, surface_id: ObjectId) -> Option<(i32, i32)> {
        self.surfaces
            .get(&(client_id, surface_id))
            .map(|surface| surface.layout)
    }

    pub fn assign_cursor_role(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
    ) -> Result<(), SurfaceError> {
        let surface = self
            .surfaces
            .get_mut(&(client_id, surface_id))
            .ok_or(SurfaceError::UnknownSurface)?;
        match surface.role {
            None => {
                surface.role = Some(Role::Cursor);
                Ok(())
            }
            Some(Role::Cursor) => Ok(()),
            Some(_) => Err(SurfaceError::RoleAlreadyAssigned),
        }
    }

    pub fn clear_cursor_role(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
    ) -> Result<(), SurfaceError> {
        let surface = self
            .surfaces
            .get_mut(&(client_id, surface_id))
            .ok_or(SurfaceError::UnknownSurface)?;
        if surface.role == Some(Role::Cursor) {
            surface.role = None;
        }
        Ok(())
    }

    pub fn surface_role_is_cursor(&self, client_id: ClientId, surface_id: ObjectId) -> bool {
        self.surfaces
            .get(&(client_id, surface_id))
            .is_some_and(|surface| surface.role == Some(Role::Cursor))
    }

    pub fn assign_dnd_icon_role(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
    ) -> Result<(), SurfaceError> {
        let surface = self
            .surfaces
            .get_mut(&(client_id, surface_id))
            .ok_or(SurfaceError::UnknownSurface)?;
        match surface.role {
            None => {
                surface.role = Some(Role::DndIcon);
                Ok(())
            }
            Some(Role::DndIcon) => Ok(()),
            Some(_) => Err(SurfaceError::RoleAlreadyAssigned),
        }
    }

    pub fn clear_dnd_icon_role(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
    ) -> Result<(), SurfaceError> {
        let surface = self
            .surfaces
            .get_mut(&(client_id, surface_id))
            .ok_or(SurfaceError::UnknownSurface)?;
        if surface.role == Some(Role::DndIcon) {
            surface.role = None;
        }
        Ok(())
    }

    pub fn attach(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        buffer: Option<ObjectId>,
        x: i32,
        y: i32,
        surface_version: u32,
    ) -> Result<(), SurfaceError> {
        if surface_version >= 5 && (x != 0 || y != 0) {
            return Err(SurfaceError::InvalidOffset);
        }
        let surface = self
            .surfaces
            .get_mut(&(client_id, id))
            .ok_or(SurfaceError::UnknownSurface)?;
        surface.pending.buffer = Some(buffer);
        if surface_version < 5 {
            surface.pending.offset = Some((x, y));
        }
        Ok(())
    }

    pub fn set_buffer_transform(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        transform: i32,
    ) -> Result<(), SurfaceError> {
        if !(0..=7).contains(&transform) {
            return Err(SurfaceError::InvalidTransform);
        }
        self.surfaces
            .get_mut(&(client_id, id))
            .ok_or(SurfaceError::UnknownSurface)?
            .pending
            .buffer_transform = Some(transform as u32);
        Ok(())
    }

    pub fn set_buffer_scale(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        scale: i32,
    ) -> Result<(), SurfaceError> {
        if scale <= 0 {
            return Err(SurfaceError::InvalidScale);
        }
        self.surfaces
            .get_mut(&(client_id, id))
            .ok_or(SurfaceError::UnknownSurface)?
            .pending
            .buffer_scale = Some(scale);
        Ok(())
    }

    pub fn create_viewport(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
        viewport_id: ObjectId,
    ) -> Result<(), SurfaceError> {
        let surface = self
            .surfaces
            .get_mut(&(client_id, surface_id))
            .ok_or(SurfaceError::UnknownSurface)?;
        if surface.viewport_id.is_some() {
            return Err(SurfaceError::ViewportExists);
        }
        surface.viewport_id = Some(viewport_id);
        self.viewports.insert(
            (client_id, viewport_id),
            ViewportBinding {
                surface_id,
                surface_alive: true,
            },
        );
        Ok(())
    }

    pub fn destroy_viewport(
        &mut self,
        client_id: ClientId,
        viewport_id: ObjectId,
    ) -> Result<(), SurfaceError> {
        let Some(binding) = self.viewports.remove(&(client_id, viewport_id)) else {
            return Ok(());
        };
        if binding.surface_alive
            && let Some(surface) = self.surfaces.get_mut(&(client_id, binding.surface_id))
        {
            if surface.viewport_id == Some(viewport_id) {
                surface.viewport_id = None;
            }
            // Crop/scale state is removed on the next commit.
            surface.pending.viewport_source = Some(None);
            surface.pending.viewport_destination = Some(None);
        }
        Ok(())
    }

    pub fn viewport_surface(
        &self,
        client_id: ClientId,
        viewport_id: ObjectId,
    ) -> Result<ObjectId, SurfaceError> {
        let binding = self
            .viewports
            .get(&(client_id, viewport_id))
            .ok_or(SurfaceError::UnknownSurface)?;
        if !binding.surface_alive {
            return Err(SurfaceError::NoSurface);
        }
        Ok(binding.surface_id)
    }

    pub fn set_viewport_source(
        &mut self,
        client_id: ClientId,
        viewport_id: ObjectId,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    ) -> Result<(), SurfaceError> {
        let surface_id = self.viewport_surface(client_id, viewport_id)?;
        let unset = x == -1.0 && y == -1.0 && width == -1.0 && height == -1.0;
        if !unset && (x < 0.0 || y < 0.0 || width <= 0.0 || height <= 0.0) {
            return Err(SurfaceError::ViewportBadValue);
        }
        let surface = self
            .surfaces
            .get_mut(&(client_id, surface_id))
            .ok_or(SurfaceError::NoSurface)?;
        surface.pending.viewport_source = Some(if unset {
            None
        } else {
            Some((x, y, width, height))
        });
        Ok(())
    }

    pub fn set_viewport_destination(
        &mut self,
        client_id: ClientId,
        viewport_id: ObjectId,
        width: i32,
        height: i32,
    ) -> Result<(), SurfaceError> {
        let surface_id = self.viewport_surface(client_id, viewport_id)?;
        let unset = width == -1 && height == -1;
        if !unset && (width <= 0 || height <= 0) {
            return Err(SurfaceError::ViewportBadValue);
        }
        let surface = self
            .surfaces
            .get_mut(&(client_id, surface_id))
            .ok_or(SurfaceError::NoSurface)?;
        surface.pending.viewport_destination =
            Some(if unset { None } else { Some((width, height)) });
        Ok(())
    }

    /// Validates committed viewport state against buffer content size.
    pub fn validate_viewport_commit(
        &self,
        client_id: ClientId,
        surface_id: ObjectId,
        buffer_width: Option<i32>,
        buffer_height: Option<i32>,
    ) -> Result<(), (ObjectId, ViewportCommitError)> {
        let Some(surface) = self.surfaces.get(&(client_id, surface_id)) else {
            return Ok(());
        };
        let Some(viewport_id) = surface.viewport_id else {
            return Ok(());
        };
        let viewport = &surface.current.viewport;
        let scale = surface.current.buffer_scale.max(1);
        if let Some((sx, sy, sw, sh)) = viewport.source {
            if viewport.destination.is_none() && (!is_whole_number(sw) || !is_whole_number(sh)) {
                return Err((viewport_id, ViewportCommitError::BadSize));
            }
            if let (Some(bw), Some(bh)) = (buffer_width, buffer_height) {
                let (cw, ch) = content_size(bw, bh, scale);
                if sx < 0.0 || sy < 0.0 || sx + sw > cw as f32 || sy + sh > ch as f32 {
                    return Err((viewport_id, ViewportCommitError::OutOfBuffer));
                }
            }
        }
        Ok(())
    }

    pub fn committed_viewport(&self, client_id: ClientId, surface_id: ObjectId) -> ViewportState {
        self.surfaces
            .get(&(client_id, surface_id))
            .map(|s| s.current.viewport)
            .unwrap_or_default()
    }

    pub fn damage_buffer(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        rectangle: Rectangle,
    ) -> Result<(), SurfaceError> {
        let surface = self
            .surfaces
            .get_mut(&(client_id, id))
            .ok_or(SurfaceError::UnknownSurface)?;
        if rectangle.width > 0 && rectangle.height > 0 {
            surface.pending.buffer_damage.push(rectangle);
        }
        Ok(())
    }

    pub fn offset(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        x: i32,
        y: i32,
    ) -> Result<(), SurfaceError> {
        self.surfaces
            .get_mut(&(client_id, id))
            .ok_or(SurfaceError::UnknownSurface)?
            .pending
            .offset = Some((x, y));
        Ok(())
    }

    pub fn damage(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        rectangle: Rectangle,
    ) -> Result<(), SurfaceError> {
        let surface = self
            .surfaces
            .get_mut(&(client_id, id))
            .ok_or(SurfaceError::UnknownSurface)?;
        if rectangle.width > 0 && rectangle.height > 0 {
            surface.pending.damage.push(rectangle);
        }
        Ok(())
    }

    pub fn add_frame_callback(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        callback: ObjectId,
    ) -> Result<(), SurfaceError> {
        self.surfaces
            .get_mut(&(client_id, id))
            .ok_or(SurfaceError::UnknownSurface)?
            .pending
            .frame_callbacks
            .push(callback);
        Ok(())
    }

    pub fn add_presentation_feedback(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        feedback: ObjectId,
    ) -> Result<(), SurfaceError> {
        self.surfaces
            .get_mut(&(client_id, id))
            .ok_or(SurfaceError::UnknownSurface)?
            .pending
            .presentation_feedbacks
            .push(feedback);
        Ok(())
    }

    pub fn set_opaque_region(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
        region_id: Option<ObjectId>,
    ) -> Result<(), SurfaceError> {
        let region = self.copy_region(client_id, region_id)?;
        self.surfaces
            .get_mut(&(client_id, surface_id))
            .ok_or(SurfaceError::UnknownSurface)?
            .pending
            .opaque_region = Some(region);
        Ok(())
    }

    pub fn set_input_region(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
        region_id: Option<ObjectId>,
    ) -> Result<(), SurfaceError> {
        let region = self.copy_region(client_id, region_id)?;
        self.surfaces
            .get_mut(&(client_id, surface_id))
            .ok_or(SurfaceError::UnknownSurface)?
            .pending
            .input_region = Some(region);
        Ok(())
    }

    pub fn commit(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
    ) -> Result<CommitResult, SurfaceError> {
        if !self.surfaces.contains_key(&(client_id, id)) {
            return Err(SurfaceError::UnknownSurface);
        }

        if self.is_effectively_synchronized(client_id, id) {
            self.cache_pending_commit(client_id, id)?;
            let primary = self.deferred_commit_result(client_id, id)?;
            return Ok(CommitResult {
                primary,
                synchronized_children: Vec::new(),
            });
        }

        let primary = self.apply_commit(client_id, id)?;
        let synchronized_children = self.apply_synchronized_children(client_id, id)?;
        Ok(CommitResult {
            primary,
            synchronized_children,
        })
    }

    pub fn create_subsurface(
        &mut self,
        client_id: ClientId,
        subsurface_id: ObjectId,
        surface: ObjectId,
        parent: ObjectId,
    ) -> Result<(), SurfaceError> {
        if surface == parent {
            return Err(SurfaceError::BadParent);
        }
        if !self.surfaces.contains_key(&(client_id, surface)) {
            return Err(SurfaceError::BadSurface);
        }
        if !self.surfaces.contains_key(&(client_id, parent)) {
            return Err(SurfaceError::BadParent);
        }
        if self.would_create_cycle(client_id, surface, parent) {
            return Err(SurfaceError::BadParent);
        }

        let child = self
            .surfaces
            .get_mut(&(client_id, surface))
            .ok_or(SurfaceError::BadSurface)?;
        if child.role.is_some() {
            return Err(SurfaceError::BadSurface);
        }
        child.role = Some(Role::Subsurface(subsurface_id));

        self.subsurfaces.insert(
            (client_id, subsurface_id),
            SubsurfaceState {
                parent,
                surface,
                sync: true,
                current_position: (0, 0),
                pending_position: None,
            },
        );

        let parent_surface = self
            .surfaces
            .get_mut(&(client_id, parent))
            .ok_or(SurfaceError::BadParent)?;
        let pending = parent_surface
            .pending_children
            .get_or_insert_with(|| parent_surface.current_children.clone());
        pending.push(surface);
        Ok(())
    }

    pub fn destroy_subsurface(
        &mut self,
        client_id: ClientId,
        subsurface_id: ObjectId,
    ) -> Result<(ObjectId, bool), SurfaceError> {
        let sub = self
            .subsurfaces
            .remove(&(client_id, subsurface_id))
            .ok_or(SurfaceError::UnknownSubsurface)?;
        let surface_id = sub.surface;
        self.remove_child_from_parent(client_id, sub.parent, surface_id);
        let was_mapped = self.is_mapped(client_id, surface_id).unwrap_or(false);
        if let Some(surface) = self.surfaces.get_mut(&(client_id, surface_id)) {
            if surface.role == Some(Role::Subsurface(subsurface_id)) {
                surface.role = None;
            }
            // Unmap immediately: clear current buffer.
            surface.current.buffer = None;
        }
        Ok((surface_id, was_mapped))
    }

    pub fn set_position(
        &mut self,
        client_id: ClientId,
        subsurface_id: ObjectId,
        x: i32,
        y: i32,
    ) -> Result<(), SurfaceError> {
        self.subsurfaces
            .get_mut(&(client_id, subsurface_id))
            .ok_or(SurfaceError::UnknownSubsurface)?
            .pending_position = Some((x, y));
        Ok(())
    }

    pub fn place_above(
        &mut self,
        client_id: ClientId,
        subsurface_id: ObjectId,
        sibling: ObjectId,
    ) -> Result<(), SurfaceError> {
        self.restack(client_id, subsurface_id, sibling, Restack::Above)
    }

    pub fn place_below(
        &mut self,
        client_id: ClientId,
        subsurface_id: ObjectId,
        sibling: ObjectId,
    ) -> Result<(), SurfaceError> {
        self.restack(client_id, subsurface_id, sibling, Restack::Below)
    }

    pub fn set_sync(
        &mut self,
        client_id: ClientId,
        subsurface_id: ObjectId,
    ) -> Result<(), SurfaceError> {
        self.subsurfaces
            .get_mut(&(client_id, subsurface_id))
            .ok_or(SurfaceError::UnknownSubsurface)?
            .sync = true;
        Ok(())
    }

    pub fn set_desync(
        &mut self,
        client_id: ClientId,
        subsurface_id: ObjectId,
    ) -> Result<(), SurfaceError> {
        self.subsurfaces
            .get_mut(&(client_id, subsurface_id))
            .ok_or(SurfaceError::UnknownSubsurface)?
            .sync = false;
        Ok(())
    }

    pub fn acknowledge_shell_ping(
        &mut self,
        client_id: ClientId,
        shell_id: ObjectId,
        serial: u32,
    ) -> Result<bool, SurfaceError> {
        let shell = self.shell_state_mut(client_id, shell_id)?;
        if shell.pending_ping == Some(serial) {
            shell.pending_ping = None;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn set_pending_shell_ping(
        &mut self,
        client_id: ClientId,
        shell_id: ObjectId,
        serial: u32,
    ) -> Result<(), SurfaceError> {
        self.shell_state_mut(client_id, shell_id)?.pending_ping = Some(serial);
        Ok(())
    }

    pub fn record_shell_move(
        &mut self,
        client_id: ClientId,
        shell_id: ObjectId,
        seat: ObjectId,
        serial: u32,
    ) -> Result<(), SurfaceError> {
        let shell = self.shell_state_mut(client_id, shell_id)?;
        shell.last_move = Some((seat, serial));
        Ok(())
    }

    pub fn record_shell_resize(
        &mut self,
        client_id: ClientId,
        shell_id: ObjectId,
        seat: ObjectId,
        serial: u32,
        edges: u32,
    ) -> Result<(), SurfaceError> {
        let shell = self.shell_state_mut(client_id, shell_id)?;
        shell.last_resize = Some((seat, serial, edges));
        Ok(())
    }

    pub fn create_shell_surface(
        &mut self,
        client_id: ClientId,
        shell_id: ObjectId,
        surface_id: ObjectId,
    ) -> Result<(), SurfaceError> {
        let surface = self
            .surfaces
            .get_mut(&(client_id, surface_id))
            .ok_or(SurfaceError::UnknownSurface)?;
        if surface.role.is_some() {
            return Err(SurfaceError::RoleAlreadyAssigned);
        }
        surface.role = Some(Role::Shell(shell_id));
        self.shell_surfaces
            .insert((client_id, shell_id), surface_id);
        Ok(())
    }

    pub fn set_shell_mode(
        &mut self,
        client_id: ClientId,
        shell_id: ObjectId,
        mode: ShellMode,
    ) -> Result<(), SurfaceError> {
        self.shell_state_mut(client_id, shell_id)?.mode = mode;
        Ok(())
    }

    pub fn surface_for_shell(
        &self,
        client_id: ClientId,
        shell_id: ObjectId,
    ) -> Result<ObjectId, SurfaceError> {
        self.shell_surfaces
            .get(&(client_id, shell_id))
            .copied()
            .ok_or(SurfaceError::UnknownShellSurface)
    }

    pub fn set_shell_title(
        &mut self,
        client_id: ClientId,
        shell_id: ObjectId,
        title: String,
    ) -> Result<(), SurfaceError> {
        self.shell_state_mut(client_id, shell_id)?.title = title;
        Ok(())
    }

    pub fn set_shell_class(
        &mut self,
        client_id: ClientId,
        shell_id: ObjectId,
        class: String,
    ) -> Result<(), SurfaceError> {
        self.shell_state_mut(client_id, shell_id)?.class = class;
        Ok(())
    }

    pub fn create_region(&mut self, client_id: ClientId, id: ObjectId) {
        self.regions.insert((client_id, id), Region::default());
    }

    pub fn destroy_region(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
    ) -> Result<(), SurfaceError> {
        self.regions
            .remove(&(client_id, id))
            .map(|_| ())
            .ok_or(SurfaceError::UnknownRegion)
    }

    pub fn add_region(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        rectangle: Rectangle,
    ) -> Result<(), SurfaceError> {
        let region = self
            .regions
            .get_mut(&(client_id, id))
            .ok_or(SurfaceError::UnknownRegion)?;
        if rectangle.width > 0 && rectangle.height > 0 {
            region.operations.push(RegionOperation::Add(rectangle));
        }
        Ok(())
    }

    pub fn subtract_region(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        rectangle: Rectangle,
    ) -> Result<(), SurfaceError> {
        let region = self
            .regions
            .get_mut(&(client_id, id))
            .ok_or(SurfaceError::UnknownRegion)?;
        if rectangle.width > 0 && rectangle.height > 0 {
            region.operations.push(RegionOperation::Subtract(rectangle));
        }
        Ok(())
    }

    pub fn delete_client(&mut self, client_id: ClientId) {
        self.surfaces.retain(|(owner, _), _| *owner != client_id);
        self.shell_surfaces
            .retain(|(owner, _), _| *owner != client_id);
        self.subsurfaces.retain(|(owner, _), _| *owner != client_id);
        self.regions.retain(|(owner, _), _| *owner != client_id);
        self.viewports.retain(|(owner, _), _| *owner != client_id);
        self.paint_order.retain(|(owner, _)| *owner != client_id);
    }

    fn cache_pending_commit(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
    ) -> Result<(), SurfaceError> {
        let surface = self
            .surfaces
            .get_mut(&(client_id, id))
            .ok_or(SurfaceError::UnknownSurface)?;
        let pending = std::mem::take(&mut surface.pending);
        match &mut surface.cache {
            Some(cache) => merge_pending(cache, pending),
            None => surface.cache = Some(pending),
        }
        Ok(())
    }

    fn deferred_commit_result(
        &self,
        client_id: ClientId,
        id: ObjectId,
    ) -> Result<SurfaceCommit, SurfaceError> {
        let surface = self
            .surfaces
            .get(&(client_id, id))
            .ok_or(SurfaceError::UnknownSurface)?;
        let mapped = self.is_mapped(client_id, id)?;
        let offset = self.presentation_offset(client_id, surface);
        Ok(SurfaceCommit {
            surface_id: id,
            buffer: surface.current.buffer,
            attached_buffer: None,
            mapped,
            newly_mapped: false,
            shell_id: None,
            frame_callbacks: Vec::new(),
            presentation_feedbacks: Vec::new(),
            deferred: true,
            buffer_scale: surface.current.buffer_scale,
            buffer_transform: surface.current.buffer_transform,
            offset,
            layout: surface.layout,
            damage: surface.current.damage.clone(),
            buffer_damage: surface.current.buffer_damage.clone(),
            viewport: surface.current.viewport,
            viewport_id: surface.viewport_id,
            viewport_changed: false,
        })
    }

    fn apply_commit(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
    ) -> Result<SurfaceCommit, SurfaceError> {
        let was_mapped = self.is_mapped(client_id, id)?;
        let surface = self
            .surfaces
            .get_mut(&(client_id, id))
            .ok_or(SurfaceError::UnknownSurface)?;

        if surface.cache.is_some() {
            let pending = std::mem::take(&mut surface.pending);
            if let Some(cache) = surface.cache.as_mut() {
                merge_pending(cache, pending);
            }
            let cache = surface.cache.take().unwrap();
            surface.pending = cache;
        }

        let viewport_changed = surface.pending.viewport_source.is_some()
            || surface.pending.viewport_destination.is_some();
        let attached_buffer = surface.pending.buffer.take();
        if let Some(buffer) = attached_buffer {
            surface.current.buffer = buffer;
        }
        if let Some(offset) = surface.pending.offset.take() {
            surface.current.offset = offset;
        }
        if let Some(region) = surface.pending.opaque_region.take() {
            surface.current.opaque_region = region;
        }
        if let Some(region) = surface.pending.input_region.take() {
            surface.current.input_region = region;
        }
        surface.current.damage = std::mem::take(&mut surface.pending.damage);
        surface.current.buffer_damage = std::mem::take(&mut surface.pending.buffer_damage);
        if let Some(scale) = surface.pending.buffer_scale.take() {
            surface.current.buffer_scale = scale;
        }
        if let Some(transform) = surface.pending.buffer_transform.take() {
            surface.current.buffer_transform = transform;
        }
        if let Some(source) = surface.pending.viewport_source.take() {
            surface.current.viewport.source = source;
        }
        if let Some(destination) = surface.pending.viewport_destination.take() {
            surface.current.viewport.destination = destination;
        }
        let frame_callbacks = std::mem::take(&mut surface.pending.frame_callbacks);
        let presentation_feedbacks = std::mem::take(&mut surface.pending.presentation_feedbacks);
        let shell_id = match surface.role {
            Some(Role::Shell(shell_id)) => Some(shell_id),
            _ => None,
        };
        let buffer = surface.current.buffer;
        let buffer_scale = surface.current.buffer_scale;
        let buffer_transform = surface.current.buffer_transform;
        let viewport = surface.current.viewport;
        let viewport_id = surface.viewport_id;
        let damage = surface.current.damage.clone();
        let buffer_damage = surface.current.buffer_damage.clone();
        let surface_offset = surface.current.offset;
        let subsurface_role = surface.role;

        let offset = match subsurface_role {
            Some(Role::Subsurface(sub_id)) => self
                .subsurfaces
                .get(&(client_id, sub_id))
                .map(|sub| sub.current_position)
                .unwrap_or(surface_offset),
            _ => surface_offset,
        };

        let mapped = self.is_mapped(client_id, id)?;
        let newly_mapped = mapped && !was_mapped;
        if newly_mapped {
            let surface = self
                .surfaces
                .get_mut(&(client_id, id))
                .ok_or(SurfaceError::UnknownSurface)?;
            if matches!(surface.role, Some(Role::Shell(_))) {
                let pos = self.next_cascade;
                self.next_cascade = self.next_cascade.wrapping_add(32);
                surface.layout = (pos, pos);
            }
        }
        let layout = self
            .surfaces
            .get(&(client_id, id))
            .map(|s| s.layout)
            .unwrap_or((0, 0));
        // Only restack on map/unmap. Re-raising on every commit desyncs hit-testing
        // from draw order (renderer keeps insertion order) and steals pointer focus.
        if newly_mapped {
            self.record_painted_surface(client_id, id);
        } else if was_mapped && !mapped {
            self.remove_painted_surface(client_id, id);
        }
        Ok(SurfaceCommit {
            surface_id: id,
            buffer,
            attached_buffer,
            mapped,
            newly_mapped,
            shell_id,
            frame_callbacks,
            presentation_feedbacks,
            deferred: false,
            buffer_scale,
            buffer_transform,
            offset,
            layout,
            damage,
            buffer_damage,
            viewport,
            viewport_id,
            viewport_changed,
        })
    }

    fn apply_synchronized_children(
        &mut self,
        client_id: ClientId,
        parent_id: ObjectId,
    ) -> Result<Vec<SurfaceCommit>, SurfaceError> {
        self.apply_pending_subsurface_state(client_id, parent_id)?;
        let children = self
            .surfaces
            .get(&(client_id, parent_id))
            .ok_or(SurfaceError::UnknownSurface)?
            .current_children
            .clone();

        let mut commits = Vec::new();
        for child_id in children {
            let has_cache = self
                .surfaces
                .get(&(client_id, child_id))
                .is_some_and(|surface| surface.cache.is_some());
            if has_cache {
                commits.push(self.apply_commit(client_id, child_id)?);
            }
            commits.extend(self.apply_synchronized_children(client_id, child_id)?);
        }
        Ok(commits)
    }

    fn apply_pending_subsurface_state(
        &mut self,
        client_id: ClientId,
        parent_id: ObjectId,
    ) -> Result<(), SurfaceError> {
        let children = {
            let parent = self
                .surfaces
                .get_mut(&(client_id, parent_id))
                .ok_or(SurfaceError::UnknownSurface)?;
            if let Some(pending) = parent.pending_children.take() {
                parent.current_children = pending;
            }
            parent.current_children.clone()
        };
        for child_id in children {
            let Some(Role::Subsurface(sub_id)) = self
                .surfaces
                .get(&(client_id, child_id))
                .and_then(|surface| surface.role)
            else {
                continue;
            };
            if let Some(sub) = self.subsurfaces.get_mut(&(client_id, sub_id))
                && let Some(position) = sub.pending_position.take()
            {
                sub.current_position = position;
            }
        }
        Ok(())
    }

    fn presentation_offset(&self, client_id: ClientId, surface: &Surface) -> (i32, i32) {
        if let Some(Role::Subsurface(sub_id)) = surface.role
            && let Some(sub) = self.subsurfaces.get(&(client_id, sub_id))
        {
            return sub.current_position;
        }
        surface.current.offset
    }

    fn is_mapped(&self, client_id: ClientId, id: ObjectId) -> Result<bool, SurfaceError> {
        let surface = self
            .surfaces
            .get(&(client_id, id))
            .ok_or(SurfaceError::UnknownSurface)?;
        if surface.current.buffer.is_none() {
            return Ok(false);
        }
        match surface.role {
            Some(Role::Shell(_)) => Ok(matches!(
                surface.shell.mode,
                ShellMode::Toplevel
                    | ShellMode::Transient
                    | ShellMode::Fullscreen
                    | ShellMode::Popup
                    | ShellMode::Maximized
            )),
            Some(Role::Xdg(_)) => Ok(surface.xdg_map_ready),
            Some(Role::Subsurface(sub_id)) => {
                let parent = self
                    .subsurfaces
                    .get(&(client_id, sub_id))
                    .ok_or(SurfaceError::UnknownSubsurface)?
                    .parent;
                self.is_mapped(client_id, parent)
            }
            Some(Role::Cursor) | Some(Role::DndIcon) | None => Ok(false),
        }
    }

    fn is_effectively_synchronized(&self, client_id: ClientId, id: ObjectId) -> bool {
        let Some(surface) = self.surfaces.get(&(client_id, id)) else {
            return false;
        };
        let Some(Role::Subsurface(sub_id)) = surface.role else {
            return false;
        };
        let Some(sub) = self.subsurfaces.get(&(client_id, sub_id)) else {
            return false;
        };
        if sub.sync {
            return true;
        }
        self.is_effectively_synchronized(client_id, sub.parent)
    }

    fn would_create_cycle(&self, client_id: ClientId, surface: ObjectId, parent: ObjectId) -> bool {
        let mut current = parent;
        loop {
            if current == surface {
                return true;
            }
            let Some(surf) = self.surfaces.get(&(client_id, current)) else {
                return false;
            };
            let Some(Role::Subsurface(sub_id)) = surf.role else {
                return false;
            };
            let Some(sub) = self.subsurfaces.get(&(client_id, sub_id)) else {
                return false;
            };
            current = sub.parent;
        }
    }

    fn remove_child_from_parent(&mut self, client_id: ClientId, parent: ObjectId, child: ObjectId) {
        if let Some(parent_surface) = self.surfaces.get_mut(&(client_id, parent)) {
            parent_surface.current_children.retain(|id| *id != child);
            if let Some(pending) = parent_surface.pending_children.as_mut() {
                pending.retain(|id| *id != child);
            }
        }
    }

    fn restack(
        &mut self,
        client_id: ClientId,
        subsurface_id: ObjectId,
        sibling: ObjectId,
        mode: Restack,
    ) -> Result<(), SurfaceError> {
        let sub = self
            .subsurfaces
            .get(&(client_id, subsurface_id))
            .ok_or(SurfaceError::UnknownSubsurface)?;
        let parent = sub.parent;
        let surface = sub.surface;
        if sibling != parent {
            let sibling_is_valid =
                self.surfaces
                    .get(&(client_id, parent))
                    .is_some_and(|parent_surface| {
                        let stack = parent_surface
                            .pending_children
                            .as_ref()
                            .unwrap_or(&parent_surface.current_children);
                        stack.contains(&sibling)
                    });
            if !sibling_is_valid {
                return Err(SurfaceError::BadSurface);
            }
        }
        if sibling == surface {
            return Err(SurfaceError::BadSurface);
        }

        let parent_surface = self
            .surfaces
            .get_mut(&(client_id, parent))
            .ok_or(SurfaceError::UnknownSurface)?;
        let pending = parent_surface
            .pending_children
            .get_or_insert_with(|| parent_surface.current_children.clone());
        pending.retain(|id| *id != surface);
        let insert_at = if sibling == parent {
            match mode {
                Restack::Above => 0,
                Restack::Below => 0,
            }
        } else {
            let sibling_index = pending
                .iter()
                .position(|id| *id == sibling)
                .ok_or(SurfaceError::BadSurface)?;
            match mode {
                Restack::Above => sibling_index + 1,
                Restack::Below => sibling_index,
            }
        };
        pending.insert(insert_at, surface);
        Ok(())
    }

    fn copy_region(
        &self,
        client_id: ClientId,
        region_id: Option<ObjectId>,
    ) -> Result<Option<Region>, SurfaceError> {
        region_id
            .map(|id| {
                self.regions
                    .get(&(client_id, id))
                    .cloned()
                    .ok_or(SurfaceError::UnknownRegion)
            })
            .transpose()
    }

    fn shell_state_mut(
        &mut self,
        client_id: ClientId,
        shell_id: ObjectId,
    ) -> Result<&mut ShellState, SurfaceError> {
        let surface_id = *self
            .shell_surfaces
            .get(&(client_id, shell_id))
            .ok_or(SurfaceError::UnknownShellSurface)?;
        let surface = self
            .surfaces
            .get_mut(&(client_id, surface_id))
            .ok_or(SurfaceError::UnknownSurface)?;
        Ok(&mut surface.shell)
    }
}

fn merge_pending(cache: &mut PendingState, mut pending: PendingState) {
    if let Some(buffer) = pending.buffer.take() {
        cache.buffer = Some(buffer);
    }
    if let Some(offset) = pending.offset.take() {
        cache.offset = Some(offset);
    }
    if let Some(region) = pending.opaque_region.take() {
        cache.opaque_region = Some(region);
    }
    if let Some(region) = pending.input_region.take() {
        cache.input_region = Some(region);
    }
    if let Some(scale) = pending.buffer_scale.take() {
        cache.buffer_scale = Some(scale);
    }
    if let Some(transform) = pending.buffer_transform.take() {
        cache.buffer_transform = Some(transform);
    }
    if let Some(source) = pending.viewport_source.take() {
        cache.viewport_source = Some(source);
    }
    if let Some(destination) = pending.viewport_destination.take() {
        cache.viewport_destination = Some(destination);
    }
    cache.damage.append(&mut pending.damage);
    cache.buffer_damage.append(&mut pending.buffer_damage);
    cache.frame_callbacks.append(&mut pending.frame_callbacks);
    cache
        .presentation_feedbacks
        .append(&mut pending.presentation_feedbacks);
}

/// Buffer size after buffer_scale (identity transform).
pub fn content_size(buffer_width: i32, buffer_height: i32, buffer_scale: i32) -> (i32, i32) {
    let scale = buffer_scale.max(1);
    (buffer_width / scale, buffer_height / scale)
}

/// Effective surface size from buffer + viewport destination/source.
pub fn effective_surface_size(
    buffer_size: Option<(i32, i32)>,
    buffer_scale: i32,
    viewport: &ViewportState,
) -> Option<(i32, i32)> {
    let (bw, bh) = buffer_size?;
    if let Some((dw, dh)) = viewport.destination {
        return Some((dw.max(1), dh.max(1)));
    }
    if let Some((_, _, sw, sh)) = viewport.source {
        return Some((sw as i32, sh as i32));
    }
    let (cw, ch) = content_size(bw, bh, buffer_scale);
    Some((cw.max(0), ch.max(0)))
}

fn is_whole_number(value: f32) -> bool {
    value == value.trunc()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Shell(ObjectId),
    Xdg(ObjectId),
    Subsurface(ObjectId),
    Cursor,
    DndIcon,
}

#[derive(Debug, Default)]
struct Surface {
    role: Option<Role>,
    shell: ShellState,
    /// Layout position for mapped shell/xdg surfaces (compositor space).
    layout: (i32, i32),
    /// Last committed buffer size in surface-local pixels (before scale).
    buffer_size: Option<(i32, i32)>,
    /// For Role::Xdg: true after an ack_configure has been applied (ready to map with buffer).
    xdg_map_ready: bool,
    /// Associated wp_viewport object, if any.
    viewport_id: Option<ObjectId>,
    current: SurfaceState,
    pending: PendingState,
    cache: Option<PendingState>,
    current_children: Vec<ObjectId>,
    pending_children: Option<Vec<ObjectId>>,
}

#[derive(Debug)]
struct ShellState {
    mode: ShellMode,
    title: String,
    class: String,
    pending_ping: Option<u32>,
    last_move: Option<(ObjectId, u32)>,
    last_resize: Option<(ObjectId, u32, u32)>,
}

impl Default for ShellState {
    fn default() -> Self {
        Self {
            mode: ShellMode::None,
            title: String::new(),
            class: String::new(),
            pending_ping: None,
            last_move: None,
            last_resize: None,
        }
    }
}

#[derive(Debug)]
struct SurfaceState {
    buffer: Option<ObjectId>,
    offset: (i32, i32),
    damage: Vec<Rectangle>,
    buffer_damage: Vec<Rectangle>,
    buffer_scale: i32,
    buffer_transform: u32,
    opaque_region: Option<Region>,
    input_region: Option<Region>,
    viewport: ViewportState,
}

impl Default for SurfaceState {
    fn default() -> Self {
        Self {
            buffer: None,
            offset: (0, 0),
            damage: Vec::new(),
            buffer_damage: Vec::new(),
            buffer_scale: 1,
            buffer_transform: 0, // WL_OUTPUT_TRANSFORM_NORMAL
            opaque_region: None,
            input_region: None,
            viewport: ViewportState::default(),
        }
    }
}

#[derive(Debug, Default)]
struct PendingState {
    buffer: Option<Option<ObjectId>>,
    offset: Option<(i32, i32)>,
    damage: Vec<Rectangle>,
    buffer_damage: Vec<Rectangle>,
    buffer_scale: Option<i32>,
    buffer_transform: Option<u32>,
    frame_callbacks: Vec<ObjectId>,
    presentation_feedbacks: Vec<ObjectId>,
    opaque_region: Option<Option<Region>>,
    input_region: Option<Option<Region>>,
    viewport_source: Option<Option<(f32, f32, f32, f32)>>,
    viewport_destination: Option<Option<(i32, i32)>>,
}

#[derive(Debug)]
struct SubsurfaceState {
    parent: ObjectId,
    surface: ObjectId,
    sync: bool,
    current_position: (i32, i32),
    pending_position: Option<(i32, i32)>,
}

#[derive(Debug, Clone, Copy)]
enum Restack {
    Above,
    Below,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rectangle {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Region {
    operations: Vec<RegionOperation>,
}

impl Region {
    fn contains(&self, x: i32, y: i32) -> bool {
        if self.operations.is_empty() {
            return false;
        }
        let mut inside = false;
        for operation in &self.operations {
            match operation {
                RegionOperation::Add(rect) => {
                    if rect_contains(rect, x, y) {
                        inside = true;
                    }
                }
                RegionOperation::Subtract(rect) => {
                    if rect_contains(rect, x, y) {
                        inside = false;
                    }
                }
            }
        }
        inside
    }
}

fn rect_contains(rect: &Rectangle, x: i32, y: i32) -> bool {
    x >= rect.x && y >= rect.y && x < rect.x + rect.width && y < rect.y + rect.height
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionOperation {
    Add(Rectangle),
    Subtract(Rectangle),
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;

    fn client(id: u32) -> ClientId {
        ClientId::new(NonZeroU32::new(id).unwrap())
    }

    fn object(id: u32) -> ObjectId {
        ObjectId::new(NonZeroU32::new(id).unwrap())
    }

    #[test]
    fn commit_applies_pending_state_atomically() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager
            .create_shell_surface(client(1), object(3), object(2))
            .unwrap();
        manager
            .set_shell_mode(client(1), object(3), ShellMode::Toplevel)
            .unwrap();
        manager
            .attach(client(1), object(2), Some(object(4)), 5, 6, 1)
            .unwrap();
        manager.set_buffer_scale(client(1), object(2), 2).unwrap();
        manager
            .set_buffer_transform(client(1), object(2), 1)
            .unwrap();
        manager
            .damage_buffer(
                client(1),
                object(2),
                Rectangle {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
            )
            .unwrap();
        manager
            .add_frame_callback(client(1), object(2), object(5))
            .unwrap();

        let commit = manager.commit(client(1), object(2)).unwrap().primary;

        assert_eq!(commit.buffer, Some(object(4)));
        assert_eq!(commit.attached_buffer, Some(Some(object(4))));
        assert!(commit.mapped);
        assert!(commit.newly_mapped);
        assert_eq!(commit.shell_id, Some(object(3)));
        assert_eq!(commit.frame_callbacks, [object(5)]);
        assert!(commit.presentation_feedbacks.is_empty());
        assert!(!commit.deferred);
        assert_eq!(commit.buffer_scale, 2);
        assert_eq!(commit.buffer_transform, 1);
        assert_eq!(commit.offset, (5, 6));
        assert_eq!(
            commit.buffer_damage,
            [Rectangle {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }]
        );
        let second = manager.commit(client(1), object(2)).unwrap().primary;
        assert_eq!(second.buffer, Some(object(4)));
        assert_eq!(second.attached_buffer, None);
        assert!(!second.newly_mapped);
        assert!(second.frame_callbacks.is_empty());
        assert_eq!(second.buffer_scale, 2);
    }

    #[test]
    fn presentation_feedback_is_taken_on_commit() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager
            .add_presentation_feedback(client(1), object(2), object(5))
            .unwrap();
        manager
            .add_presentation_feedback(client(1), object(2), object(6))
            .unwrap();

        let commit = manager.commit(client(1), object(2)).unwrap().primary;
        assert_eq!(commit.presentation_feedbacks, [object(5), object(6)]);
        assert!(!commit.deferred);

        let second = manager.commit(client(1), object(2)).unwrap().primary;
        assert!(second.presentation_feedbacks.is_empty());
    }

    #[test]
    fn attach_offset_rejected_on_surface_version_5() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        assert_eq!(
            manager
                .attach(client(1), object(2), Some(object(4)), 1, 0, 5)
                .unwrap_err(),
            SurfaceError::InvalidOffset
        );
        manager.offset(client(1), object(2), 1, 2).unwrap();
        manager
            .attach(client(1), object(2), Some(object(4)), 0, 0, 5)
            .unwrap();
        let commit = manager.commit(client(1), object(2)).unwrap().primary;
        assert_eq!(commit.offset, (1, 2));
    }

    #[test]
    fn popup_shell_mode_maps_with_buffer() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager
            .create_shell_surface(client(1), object(3), object(2))
            .unwrap();
        manager
            .set_shell_mode(client(1), object(3), ShellMode::Popup)
            .unwrap();
        manager
            .attach(client(1), object(2), Some(object(4)), 0, 0, 1)
            .unwrap();
        let commit = manager.commit(client(1), object(2)).unwrap().primary;
        assert!(commit.mapped);
        assert!(commit.newly_mapped);
    }

    #[test]
    fn pointer_target_uses_buffer_geometry() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager.create_surface(client(1), object(5));
        manager
            .create_shell_surface(client(1), object(3), object(2))
            .unwrap();
        manager
            .set_shell_mode(client(1), object(3), ShellMode::Toplevel)
            .unwrap();
        manager
            .create_shell_surface(client(1), object(6), object(5))
            .unwrap();
        manager
            .set_shell_mode(client(1), object(6), ShellMode::Toplevel)
            .unwrap();

        manager
            .attach(client(1), object(2), Some(object(4)), 0, 0, 1)
            .unwrap();
        assert!(manager.commit(client(1), object(2)).unwrap().primary.mapped);
        manager
            .set_committed_buffer_size(client(1), object(2), 100, 100)
            .unwrap();

        manager
            .attach(client(1), object(5), Some(object(7)), 0, 0, 1)
            .unwrap();
        assert!(manager.commit(client(1), object(5)).unwrap().primary.mapped);
        manager
            .set_committed_buffer_size(client(1), object(5), 100, 100)
            .unwrap();

        assert_eq!(
            manager.pointer_target(client(1), 10.0, 10.0),
            Some(object(2))
        );
        // Overlap region prefers the later-cascaded surface.
        assert_eq!(
            manager.pointer_target(client(1), 40.0, 40.0),
            Some(object(5))
        );
        assert!(manager.pointer_target(client(1), 200.0, 200.0).is_none());
    }

    #[test]
    fn global_pointer_target_uses_topmost_across_clients() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager.create_surface(client(2), object(5));
        manager
            .create_shell_surface(client(1), object(3), object(2))
            .unwrap();
        manager
            .set_shell_mode(client(1), object(3), ShellMode::Toplevel)
            .unwrap();
        manager
            .create_shell_surface(client(2), object(6), object(5))
            .unwrap();
        manager
            .set_shell_mode(client(2), object(6), ShellMode::Toplevel)
            .unwrap();

        manager
            .attach(client(1), object(2), Some(object(4)), 0, 0, 1)
            .unwrap();
        assert!(manager.commit(client(1), object(2)).unwrap().primary.mapped);
        manager
            .set_committed_buffer_size(client(1), object(2), 100, 100)
            .unwrap();

        manager
            .attach(client(2), object(5), Some(object(7)), 0, 0, 1)
            .unwrap();
        assert!(manager.commit(client(2), object(5)).unwrap().primary.mapped);
        manager
            .set_committed_buffer_size(client(2), object(5), 100, 100)
            .unwrap();

        // Overlap: later-mapped client is on top.
        assert_eq!(
            manager.global_pointer_target(None, 40.0, 40.0),
            Some((client(2), object(5)))
        );
        // Region only covered by the bottom window.
        assert_eq!(
            manager.global_pointer_target(None, 10.0, 10.0),
            Some((client(1), object(2)))
        );
    }

    #[test]
    fn recommit_does_not_restack_paint_order() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager.create_surface(client(1), object(5));
        manager
            .create_shell_surface(client(1), object(3), object(2))
            .unwrap();
        manager
            .set_shell_mode(client(1), object(3), ShellMode::Toplevel)
            .unwrap();
        manager
            .create_shell_surface(client(1), object(6), object(5))
            .unwrap();
        manager
            .set_shell_mode(client(1), object(6), ShellMode::Toplevel)
            .unwrap();

        manager
            .attach(client(1), object(2), Some(object(4)), 0, 0, 1)
            .unwrap();
        assert!(manager.commit(client(1), object(2)).unwrap().primary.mapped);
        manager
            .set_committed_buffer_size(client(1), object(2), 100, 100)
            .unwrap();

        manager
            .attach(client(1), object(5), Some(object(7)), 0, 0, 1)
            .unwrap();
        assert!(manager.commit(client(1), object(5)).unwrap().primary.mapped);
        manager
            .set_committed_buffer_size(client(1), object(5), 100, 100)
            .unwrap();

        assert_eq!(
            manager.pointer_target(client(1), 40.0, 40.0),
            Some(object(5))
        );

        // Damage/commit on the back window must not steal stacking.
        manager
            .attach(client(1), object(2), Some(object(8)), 0, 0, 1)
            .unwrap();
        assert!(manager.commit(client(1), object(2)).unwrap().primary.mapped);
        assert_eq!(
            manager.pointer_target(client(1), 40.0, 40.0),
            Some(object(5))
        );
    }

    #[test]
    fn surface_local_coords_subtracts_layout() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager
            .create_shell_surface(client(1), object(3), object(2))
            .unwrap();
        manager
            .set_shell_mode(client(1), object(3), ShellMode::Toplevel)
            .unwrap();
        manager
            .attach(client(1), object(2), Some(object(4)), 0, 0, 1)
            .unwrap();
        assert!(manager.commit(client(1), object(2)).unwrap().primary.mapped);
        manager
            .set_committed_buffer_size(client(1), object(2), 100, 100)
            .unwrap();
        let layout = manager
            .surfaces
            .get(&(client(1), object(2)))
            .unwrap()
            .layout;
        let (local_x, local_y) = manager
            .surface_local_coords(
                client(1),
                object(2),
                layout.0 as f64 + 20.0,
                layout.1 as f64 + 30.0,
            )
            .unwrap();
        assert!((local_x - 20.0).abs() < f32::EPSILON);
        assert!((local_y - 30.0).abs() < f32::EPSILON);
    }

    #[test]
    fn assign_cursor_role_rejects_shell_surface() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager
            .create_shell_surface(client(1), object(3), object(2))
            .unwrap();
        assert_eq!(
            manager
                .assign_cursor_role(client(1), object(2))
                .unwrap_err(),
            SurfaceError::RoleAlreadyAssigned
        );
        manager.create_surface(client(1), object(4));
        manager.assign_cursor_role(client(1), object(4)).unwrap();
        assert!(manager.surface_role_is_cursor(client(1), object(4)));
    }

    #[test]
    fn shell_ping_serial_is_acknowledged_by_matching_pong() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager
            .create_shell_surface(client(1), object(3), object(2))
            .unwrap();
        manager
            .set_pending_shell_ping(client(1), object(3), 42)
            .unwrap();
        assert!(
            !manager
                .acknowledge_shell_ping(client(1), object(3), 7)
                .unwrap()
        );
        assert!(
            manager
                .acknowledge_shell_ping(client(1), object(3), 42)
                .unwrap()
        );
        assert!(
            !manager
                .acknowledge_shell_ping(client(1), object(3), 42)
                .unwrap()
        );
    }

    #[test]
    fn null_buffer_unmaps_surface() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager
            .create_shell_surface(client(1), object(3), object(2))
            .unwrap();
        manager
            .set_shell_mode(client(1), object(3), ShellMode::Toplevel)
            .unwrap();
        manager
            .attach(client(1), object(2), Some(object(4)), 0, 0, 1)
            .unwrap();
        assert!(manager.commit(client(1), object(2)).unwrap().primary.mapped);

        manager.attach(client(1), object(2), None, 0, 0, 1).unwrap();
        let commit = manager.commit(client(1), object(2)).unwrap().primary;
        assert_eq!(commit.buffer, None);
        assert!(!commit.mapped);
    }

    #[test]
    fn surface_roles_are_permanent() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager
            .create_shell_surface(client(1), object(3), object(2))
            .unwrap();
        assert_eq!(
            manager
                .create_shell_surface(client(1), object(4), object(2))
                .unwrap_err(),
            SurfaceError::RoleAlreadyAssigned
        );
    }

    #[test]
    fn subsurface_role_conflicts_with_shell() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager.create_surface(client(1), object(3));
        manager
            .create_shell_surface(client(1), object(4), object(2))
            .unwrap();
        assert_eq!(
            manager
                .create_subsurface(client(1), object(5), object(2), object(3))
                .unwrap_err(),
            SurfaceError::BadSurface
        );
        manager
            .create_subsurface(client(1), object(5), object(3), object(2))
            .unwrap();
        assert_eq!(
            manager
                .create_shell_surface(client(1), object(6), object(3))
                .unwrap_err(),
            SurfaceError::RoleAlreadyAssigned
        );
    }

    #[test]
    fn sync_subsurface_commit_is_deferred_until_parent_commit() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager.create_surface(client(1), object(3));
        manager
            .create_shell_surface(client(1), object(4), object(2))
            .unwrap();
        manager
            .set_shell_mode(client(1), object(4), ShellMode::Toplevel)
            .unwrap();
        manager
            .create_subsurface(client(1), object(5), object(3), object(2))
            .unwrap();

        manager
            .attach(client(1), object(3), Some(object(6)), 0, 0, 1)
            .unwrap();
        manager
            .add_frame_callback(client(1), object(3), object(7))
            .unwrap();
        let child = manager.commit(client(1), object(3)).unwrap();
        assert_eq!(child.primary.attached_buffer, None);
        assert!(child.primary.frame_callbacks.is_empty());
        assert!(child.synchronized_children.is_empty());
        assert!(!child.primary.mapped);

        manager
            .attach(client(1), object(2), Some(object(8)), 0, 0, 1)
            .unwrap();
        let parent = manager.commit(client(1), object(2)).unwrap();
        assert!(parent.primary.mapped);
        assert_eq!(parent.synchronized_children.len(), 1);
        let child_commit = &parent.synchronized_children[0];
        assert_eq!(child_commit.surface_id, object(3));
        assert_eq!(child_commit.attached_buffer, Some(Some(object(6))));
        assert_eq!(child_commit.frame_callbacks, [object(7)]);
        assert!(child_commit.mapped);
        assert!(child_commit.newly_mapped);
    }

    #[test]
    fn desync_subsurface_commits_independently() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager.create_surface(client(1), object(3));
        manager
            .create_shell_surface(client(1), object(4), object(2))
            .unwrap();
        manager
            .set_shell_mode(client(1), object(4), ShellMode::Toplevel)
            .unwrap();
        manager
            .create_subsurface(client(1), object(5), object(3), object(2))
            .unwrap();
        manager.set_desync(client(1), object(5)).unwrap();

        manager
            .attach(client(1), object(2), Some(object(8)), 0, 0, 1)
            .unwrap();
        assert!(manager.commit(client(1), object(2)).unwrap().primary.mapped);

        manager
            .attach(client(1), object(3), Some(object(6)), 0, 0, 1)
            .unwrap();
        manager
            .add_frame_callback(client(1), object(3), object(7))
            .unwrap();
        let child = manager.commit(client(1), object(3)).unwrap();
        assert_eq!(child.primary.attached_buffer, Some(Some(object(6))));
        assert_eq!(child.primary.frame_callbacks, [object(7)]);
        assert!(child.primary.mapped);
        assert!(child.synchronized_children.is_empty());
    }

    #[test]
    fn set_position_is_applied_on_parent_commit() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager.create_surface(client(1), object(3));
        manager
            .create_shell_surface(client(1), object(4), object(2))
            .unwrap();
        manager
            .set_shell_mode(client(1), object(4), ShellMode::Toplevel)
            .unwrap();
        manager
            .create_subsurface(client(1), object(5), object(3), object(2))
            .unwrap();
        manager.set_position(client(1), object(5), 12, 34).unwrap();

        manager
            .attach(client(1), object(3), Some(object(6)), 0, 0, 1)
            .unwrap();
        manager.commit(client(1), object(3)).unwrap();

        manager
            .attach(client(1), object(2), Some(object(8)), 0, 0, 1)
            .unwrap();
        let parent = manager.commit(client(1), object(2)).unwrap();
        assert_eq!(parent.synchronized_children[0].offset, (12, 34));
        assert_eq!(
            manager
                .subsurfaces
                .get(&(client(1), object(5)))
                .unwrap()
                .current_position,
            (12, 34)
        );
    }

    #[test]
    fn region_state_is_copied_into_pending_surface_state() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager.create_region(client(1), object(3));
        let first = Rectangle {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        manager.add_region(client(1), object(3), first).unwrap();
        manager
            .set_opaque_region(client(1), object(2), Some(object(3)))
            .unwrap();
        manager
            .subtract_region(
                client(1),
                object(3),
                Rectangle {
                    x: 1,
                    y: 1,
                    width: 2,
                    height: 2,
                },
            )
            .unwrap();
        manager.commit(client(1), object(2)).unwrap();

        let surface = manager.surfaces.get(&(client(1), object(2))).unwrap();
        assert_eq!(
            surface.current.opaque_region.as_ref().unwrap().operations,
            [RegionOperation::Add(first)]
        );
    }

    #[test]
    fn viewport_destination_sets_surface_size() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager
            .create_viewport(client(1), object(2), object(3))
            .unwrap();
        manager
            .set_viewport_destination(client(1), object(3), 40, 30)
            .unwrap();
        manager.commit(client(1), object(2)).unwrap();
        manager
            .set_committed_buffer_size(client(1), object(2), 200, 100)
            .unwrap();
        let viewport = manager.committed_viewport(client(1), object(2));
        assert_eq!(viewport.destination, Some((40, 30)));
        assert_eq!(
            effective_surface_size(Some((200, 100)), 1, &viewport),
            Some((40, 30))
        );
    }

    #[test]
    fn viewport_source_only_requires_integer_size() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager
            .create_viewport(client(1), object(2), object(3))
            .unwrap();
        manager
            .set_viewport_source(client(1), object(3), 0.0, 0.0, 10.5, 10.0)
            .unwrap();
        manager.commit(client(1), object(2)).unwrap();
        manager
            .set_committed_buffer_size(client(1), object(2), 100, 100)
            .unwrap();
        assert_eq!(
            manager.validate_viewport_commit(client(1), object(2), Some(100), Some(100)),
            Err((object(3), ViewportCommitError::BadSize))
        );
    }

    #[test]
    fn viewport_out_of_buffer_is_detected() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager
            .create_viewport(client(1), object(2), object(3))
            .unwrap();
        manager
            .set_viewport_source(client(1), object(3), 0.0, 0.0, 80.0, 80.0)
            .unwrap();
        manager
            .set_viewport_destination(client(1), object(3), 40, 40)
            .unwrap();
        manager.commit(client(1), object(2)).unwrap();
        // Content size with scale 2 is 50x50; source 80x80 is out of buffer.
        manager.set_buffer_scale(client(1), object(2), 2).unwrap();
        manager.commit(client(1), object(2)).unwrap();
        assert_eq!(
            manager.validate_viewport_commit(client(1), object(2), Some(100), Some(100)),
            Err((object(3), ViewportCommitError::OutOfBuffer))
        );
    }

    #[test]
    fn viewport_exists_rejects_second_viewport() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager
            .create_viewport(client(1), object(2), object(3))
            .unwrap();
        assert_eq!(
            manager
                .create_viewport(client(1), object(2), object(4))
                .unwrap_err(),
            SurfaceError::ViewportExists
        );
    }

    #[test]
    fn destroy_viewport_clears_state_on_commit() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager
            .create_viewport(client(1), object(2), object(3))
            .unwrap();
        manager
            .set_viewport_destination(client(1), object(3), 40, 30)
            .unwrap();
        manager.commit(client(1), object(2)).unwrap();
        manager.destroy_viewport(client(1), object(3)).unwrap();
        manager.commit(client(1), object(2)).unwrap();
        let viewport = manager.committed_viewport(client(1), object(2));
        assert_eq!(viewport.destination, None);
        assert_eq!(viewport.source, None);
    }

    #[test]
    fn surface_destroy_makes_viewport_no_surface() {
        let mut manager = SurfaceManager::default();
        manager.create_surface(client(1), object(2));
        manager
            .create_viewport(client(1), object(2), object(3))
            .unwrap();
        manager.destroy_surface(client(1), object(2)).unwrap();
        assert_eq!(
            manager
                .set_viewport_destination(client(1), object(3), 10, 10)
                .unwrap_err(),
            SurfaceError::NoSurface
        );
    }
}
