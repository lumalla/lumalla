use std::collections::HashMap;

use lumalla_wayland_protocol::{ClientId, ObjectId};

type ResourceKey = (ClientId, ObjectId);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceError {
    UnknownSurface,
    UnknownBuffer,
    UnknownShellSurface,
    UnknownRegion,
    RoleAlreadyAssigned,
    InvalidScale,
    InvalidTransform,
    InvalidOffset,
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
    pub buffer_scale: i32,
    pub buffer_transform: u32,
    pub offset: (i32, i32),
    #[allow(dead_code)]
    pub damage: Vec<Rectangle>,
    #[allow(dead_code)]
    pub buffer_damage: Vec<Rectangle>,
}

#[derive(Debug, Default)]
pub struct SurfaceManager {
    surfaces: HashMap<ResourceKey, Surface>,
    shell_surfaces: HashMap<ResourceKey, ObjectId>,
    regions: HashMap<ResourceKey, Region>,
}

impl SurfaceManager {
    pub fn create_surface(&mut self, client_id: ClientId, id: ObjectId) {
        self.surfaces.insert((client_id, id), Surface::default());
    }

    pub fn destroy_surface(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
    ) -> Result<(Option<ObjectId>, Vec<ObjectId>, bool), SurfaceError> {
        let surface = self
            .surfaces
            .remove(&(client_id, id))
            .ok_or(SurfaceError::UnknownSurface)?;
        let was_mapped = surface.is_mapped();
        let shell_id = match surface.role {
            Some(Role::Shell(shell_id)) => {
                self.shell_surfaces.remove(&(client_id, shell_id));
                Some(shell_id)
            }
            None => None,
        };
        Ok((shell_id, surface.pending.frame_callbacks, was_mapped))
    }

    pub fn first_surface(&self, client_id: ClientId) -> Option<ObjectId> {
        self.surfaces
            .iter()
            .find(|((owner, _), surface)| *owner == client_id && surface.is_mapped())
            .map(|((_, id), _)| *id)
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
    ) -> Result<SurfaceCommit, SurfaceError> {
        let surface = self
            .surfaces
            .get_mut(&(client_id, id))
            .ok_or(SurfaceError::UnknownSurface)?;
        let was_mapped = surface.is_mapped();
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
        let frame_callbacks = std::mem::take(&mut surface.pending.frame_callbacks);
        let mapped = surface.is_mapped();
        let shell_id = match surface.role {
            Some(Role::Shell(shell_id)) => Some(shell_id),
            None => None,
        };
        Ok(SurfaceCommit {
            surface_id: id,
            buffer: surface.current.buffer,
            attached_buffer,
            mapped,
            newly_mapped: mapped && !was_mapped,
            shell_id,
            frame_callbacks,
            buffer_scale: surface.current.buffer_scale,
            buffer_transform: surface.current.buffer_transform,
            offset: surface.current.offset,
            damage: surface.current.damage.clone(),
            buffer_damage: surface.current.buffer_damage.clone(),
        })
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
        self.regions.retain(|(owner, _), _| *owner != client_id);
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

#[derive(Debug, Default)]
struct Surface {
    role: Option<Role>,
    shell: ShellState,
    current: SurfaceState,
    pending: PendingState,
}

impl Surface {
    fn is_mapped(&self) -> bool {
        self.current.buffer.is_some()
            && matches!(
                self.shell.mode,
                ShellMode::Toplevel
                    | ShellMode::Transient
                    | ShellMode::Fullscreen
                    | ShellMode::Popup
                    | ShellMode::Maximized
            )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Shell(ObjectId),
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
    opaque_region: Option<Option<Region>>,
    input_region: Option<Option<Region>>,
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
        manager
            .set_buffer_scale(client(1), object(2), 2)
            .unwrap();
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

        let commit = manager.commit(client(1), object(2)).unwrap();

        assert_eq!(commit.buffer, Some(object(4)));
        assert_eq!(commit.attached_buffer, Some(Some(object(4))));
        assert!(commit.mapped);
        assert!(commit.newly_mapped);
        assert_eq!(commit.shell_id, Some(object(3)));
        assert_eq!(commit.frame_callbacks, [object(5)]);
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
        let second = manager.commit(client(1), object(2)).unwrap();
        assert_eq!(second.buffer, Some(object(4)));
        assert_eq!(second.attached_buffer, None);
        assert!(!second.newly_mapped);
        assert!(second.frame_callbacks.is_empty());
        assert_eq!(second.buffer_scale, 2);
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
        let commit = manager.commit(client(1), object(2)).unwrap();
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
        let commit = manager.commit(client(1), object(2)).unwrap();
        assert!(commit.mapped);
        assert!(commit.newly_mapped);
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
        assert!(!manager
            .acknowledge_shell_ping(client(1), object(3), 7)
            .unwrap());
        assert!(manager
            .acknowledge_shell_ping(client(1), object(3), 42)
            .unwrap());
        assert!(!manager
            .acknowledge_shell_ping(client(1), object(3), 42)
            .unwrap());
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
        assert!(manager.commit(client(1), object(2)).unwrap().mapped);

        manager.attach(client(1), object(2), None, 0, 0, 1).unwrap();
        let commit = manager.commit(client(1), object(2)).unwrap();
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
}
