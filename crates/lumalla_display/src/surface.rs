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
    pub frame_callbacks: Vec<ObjectId>,
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
    ) -> Result<(Option<ObjectId>, Vec<ObjectId>), SurfaceError> {
        let surface = self
            .surfaces
            .remove(&(client_id, id))
            .ok_or(SurfaceError::UnknownSurface)?;
        let shell_id = match surface.role {
            Some(Role::Shell(shell_id)) => {
                self.shell_surfaces.remove(&(client_id, shell_id));
                Some(shell_id)
            }
            None => None,
        };
        Ok((shell_id, surface.pending.frame_callbacks))
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
    ) -> Result<(), SurfaceError> {
        let surface = self
            .surfaces
            .get_mut(&(client_id, id))
            .ok_or(SurfaceError::UnknownSurface)?;
        surface.pending.buffer = Some(buffer);
        surface.pending.offset = Some((x, y));
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
        let frame_callbacks = std::mem::take(&mut surface.pending.frame_callbacks);
        Ok(SurfaceCommit {
            surface_id: id,
            buffer: surface.current.buffer,
            attached_buffer,
            mapped: surface.is_mapped(),
            frame_callbacks,
        })
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
}

impl Default for ShellState {
    fn default() -> Self {
        Self {
            mode: ShellMode::None,
            title: String::new(),
            class: String::new(),
        }
    }
}

#[derive(Debug, Default)]
struct SurfaceState {
    buffer: Option<ObjectId>,
    offset: (i32, i32),
    damage: Vec<Rectangle>,
    opaque_region: Option<Region>,
    input_region: Option<Region>,
}

#[derive(Debug, Default)]
struct PendingState {
    buffer: Option<Option<ObjectId>>,
    offset: Option<(i32, i32)>,
    damage: Vec<Rectangle>,
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
            .attach(client(1), object(2), Some(object(4)), 5, 6)
            .unwrap();
        manager
            .add_frame_callback(client(1), object(2), object(5))
            .unwrap();

        let commit = manager.commit(client(1), object(2)).unwrap();

        assert_eq!(commit.buffer, Some(object(4)));
        assert_eq!(commit.attached_buffer, Some(Some(object(4))));
        assert!(commit.mapped);
        assert_eq!(commit.frame_callbacks, [object(5)]);
        let second = manager.commit(client(1), object(2)).unwrap();
        assert_eq!(second.buffer, Some(object(4)));
        assert_eq!(second.attached_buffer, None);
        assert!(second.frame_callbacks.is_empty());
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
            .attach(client(1), object(2), Some(object(4)), 0, 0)
            .unwrap();
        assert!(manager.commit(client(1), object(2)).unwrap().mapped);

        manager.attach(client(1), object(2), None, 0, 0).unwrap();
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
