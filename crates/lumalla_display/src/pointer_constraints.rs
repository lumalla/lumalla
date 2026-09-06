use std::collections::HashMap;

use lumalla_wayland_protocol::{
    ClientConnection, ClientId, ObjectId,
    protocols::pointer_constraints::{
        ZWP_POINTER_CONSTRAINTS_V1_LIFETIME_ONESHOT, ZWP_POINTER_CONSTRAINTS_V1_LIFETIME_PERSISTENT,
    },
};

use crate::surface::{Region, SurfaceManager};

type ResourceKey = (ClientId, ObjectId);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConstraintLifetime {
    Oneshot,
    Persistent,
}

impl ConstraintLifetime {
    pub(crate) fn from_protocol(value: u32) -> Option<Self> {
        match value {
            ZWP_POINTER_CONSTRAINTS_V1_LIFETIME_ONESHOT => Some(Self::Oneshot),
            ZWP_POINTER_CONSTRAINTS_V1_LIFETIME_PERSISTENT => Some(Self::Persistent),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConstraintKind {
    Locked,
    Confined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConstraintPhase {
    /// Waiting for focus + pointer inside region.
    Pending,
    Active,
    /// Oneshot after deactivate, or surface destroyed before activate.
    Defunct,
}

#[derive(Debug)]
struct Constraint {
    kind: ConstraintKind,
    surface: ObjectId,
    pointer: ObjectId,
    lifetime: ConstraintLifetime,
    phase: ConstraintPhase,
    region: Option<Region>,
    /// Outer `Some` means a pending `set_region` exists; inner is the region (null → full).
    pending_region: Option<Option<Region>>,
    cursor_hint: Option<(f64, f64)>,
    pending_cursor_hint: Option<(f64, f64)>,
}

#[derive(Debug, Default)]
pub struct PointerConstraintsManager {
    constraints: HashMap<ResourceKey, Constraint>,
    /// At most one constraint object per surface for this seat.
    by_surface: HashMap<ResourceKey, ObjectId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ActiveConstraint {
    pub client_id: ClientId,
    pub object_id: ObjectId,
    pub surface: ObjectId,
    pub pointer: ObjectId,
    pub kind: ConstraintKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintError {
    AlreadyConstrained,
    UnknownRegion,
    InvalidLifetime,
    UnknownConstraint,
    Defunct,
}

impl PointerConstraintsManager {
    pub fn create_constraint(
        &mut self,
        client_id: ClientId,
        object_id: ObjectId,
        kind: ConstraintKind,
        surface: ObjectId,
        pointer: ObjectId,
        region: Option<Region>,
        lifetime: ConstraintLifetime,
    ) -> Result<(), ConstraintError> {
        let surface_key = (client_id, surface);
        if self.by_surface.contains_key(&surface_key) {
            return Err(ConstraintError::AlreadyConstrained);
        }
        self.by_surface.insert(surface_key, object_id);
        self.constraints.insert(
            (client_id, object_id),
            Constraint {
                kind,
                surface,
                pointer,
                lifetime,
                phase: ConstraintPhase::Pending,
                region,
                pending_region: None,
                cursor_hint: None,
                pending_cursor_hint: None,
            },
        );
        Ok(())
    }

    pub fn set_pending_region(
        &mut self,
        client_id: ClientId,
        object_id: ObjectId,
        region: Option<Region>,
    ) -> Result<(), ConstraintError> {
        let constraint = self
            .constraints
            .get_mut(&(client_id, object_id))
            .ok_or(ConstraintError::UnknownConstraint)?;
        if constraint.phase == ConstraintPhase::Defunct {
            return Err(ConstraintError::Defunct);
        }
        constraint.pending_region = Some(region);
        Ok(())
    }

    pub fn set_pending_cursor_hint(
        &mut self,
        client_id: ClientId,
        object_id: ObjectId,
        surface_x: f64,
        surface_y: f64,
    ) -> Result<(), ConstraintError> {
        let constraint = self
            .constraints
            .get_mut(&(client_id, object_id))
            .ok_or(ConstraintError::UnknownConstraint)?;
        if constraint.phase == ConstraintPhase::Defunct {
            return Err(ConstraintError::Defunct);
        }
        if constraint.kind != ConstraintKind::Locked {
            return Err(ConstraintError::UnknownConstraint);
        }
        constraint.pending_cursor_hint = Some((surface_x, surface_y));
        Ok(())
    }

    /// Apply double-buffered region / cursor hint for constraints on `surface`.
    ///
    /// Returns confined constraints that became active with a region that may require
    /// warping the pointer (caller checks position).
    pub fn apply_surface_commit(
        &mut self,
        client_id: ClientId,
        surface: ObjectId,
    ) -> Vec<ActiveConstraint> {
        let Some(&object_id) = self.by_surface.get(&(client_id, surface)) else {
            return Vec::new();
        };
        let Some(constraint) = self.constraints.get_mut(&(client_id, object_id)) else {
            return Vec::new();
        };
        if let Some(region) = constraint.pending_region.take() {
            constraint.region = region;
        }
        if let Some(hint) = constraint.pending_cursor_hint.take() {
            constraint.cursor_hint = Some(hint);
        }
        if constraint.phase == ConstraintPhase::Active
            && constraint.kind == ConstraintKind::Confined
        {
            return vec![ActiveConstraint {
                client_id,
                object_id,
                surface: constraint.surface,
                pointer: constraint.pointer,
                kind: constraint.kind,
            }];
        }
        Vec::new()
    }

    pub fn active_for_seat(&self) -> Option<ActiveConstraint> {
        self.constraints.iter().find_map(|(&(client_id, object_id), c)| {
            (c.phase == ConstraintPhase::Active).then_some(ActiveConstraint {
                client_id,
                object_id,
                surface: c.surface,
                pointer: c.pointer,
                kind: c.kind,
            })
        })
    }

    pub fn region_for(&self, client_id: ClientId, object_id: ObjectId) -> Option<&Region> {
        self.constraints
            .get(&(client_id, object_id))
            .and_then(|c| c.region.as_ref())
    }

    /// Try to activate pending constraints when pointer focus/position allows it.
    pub fn try_activate(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        surface_manager: &SurfaceManager,
        pointer_focus: Option<(ClientId, ObjectId, ObjectId)>,
        pointer_x: f64,
        pointer_y: f64,
    ) {
        let Some((focus_client, focus_surface, focus_pointer)) = pointer_focus else {
            return;
        };
        let Some(client) = clients.get_mut(&focus_client) else {
            return;
        };
        self.activate_if_ready(
            focus_client,
            focus_surface,
            focus_pointer,
            pointer_x,
            pointer_y,
            surface_manager,
            client.writer_mut(),
        );
    }

    /// Activate a pending constraint for the given focus if the pointer is inside the region.
    pub fn activate_if_ready(
        &mut self,
        client_id: ClientId,
        focus_surface: ObjectId,
        focus_pointer: ObjectId,
        pointer_x: f64,
        pointer_y: f64,
        surface_manager: &SurfaceManager,
        writer: &mut lumalla_wayland_protocol::buffer::Writer,
    ) {
        let pending: Vec<ObjectId> = self
            .constraints
            .iter()
            .filter(|((cid, _), c)| {
                c.phase == ConstraintPhase::Pending
                    && *cid == client_id
                    && c.surface == focus_surface
                    && c.pointer == focus_pointer
            })
            .map(|((_, oid), _)| *oid)
            .collect();

        for object_id in pending {
            let Some(constraint) = self.constraints.get(&(client_id, object_id)) else {
                continue;
            };
            let Some((sx, sy)) = surface_manager.surface_local_coords(
                client_id,
                constraint.surface,
                pointer_x,
                pointer_y,
            ) else {
                continue;
            };
            if !surface_manager.constraint_region_contains(
                client_id,
                constraint.surface,
                constraint.region.as_ref(),
                sx as f64,
                sy as f64,
            ) {
                continue;
            }
            let kind = constraint.kind;
            let Some(constraint) = self.constraints.get_mut(&(client_id, object_id)) else {
                continue;
            };
            constraint.phase = ConstraintPhase::Active;
            match kind {
                ConstraintKind::Locked => {
                    writer.zwp_locked_pointer_v1_locked(object_id);
                }
                ConstraintKind::Confined => {
                    writer.zwp_confined_pointer_v1_confined(object_id);
                }
            }
        }
    }

    /// Deactivate an active constraint (compositor-initiated unlock/unconfine).
    #[allow(dead_code)]
    pub fn deactivate(
        &mut self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        client_id: ClientId,
        object_id: ObjectId,
        send_event: bool,
    ) -> Option<(f64, f64)> {
        let constraint = self.constraints.get_mut(&(client_id, object_id))?;
        if constraint.phase != ConstraintPhase::Active {
            return None;
        }
        let kind = constraint.kind;
        let lifetime = constraint.lifetime;
        let hint = if kind == ConstraintKind::Locked {
            constraint.cursor_hint.take()
        } else {
            None
        };
        if send_event {
            if let Some(client) = clients.get_mut(&client_id) {
                match kind {
                    ConstraintKind::Locked => {
                        client.writer_mut().zwp_locked_pointer_v1_unlocked(object_id);
                    }
                    ConstraintKind::Confined => {
                        client
                            .writer_mut()
                            .zwp_confined_pointer_v1_unconfined(object_id);
                    }
                }
            }
        }
        let constraint = self.constraints.get_mut(&(client_id, object_id))?;
        match lifetime {
            ConstraintLifetime::Oneshot => {
                constraint.phase = ConstraintPhase::Defunct;
            }
            ConstraintLifetime::Persistent => {
                constraint.phase = ConstraintPhase::Pending;
            }
        }
        hint
    }

    /// Destroy constraint object (client destroy request or teardown).
    ///
    /// Returns `(surface, cursor_hint)` if an active lock was destroyed with a hint.
    pub fn destroy(
        &mut self,
        client_id: ClientId,
        object_id: ObjectId,
    ) -> Option<(ObjectId, Option<(f64, f64)>)> {
        let Some(constraint) = self.constraints.remove(&(client_id, object_id)) else {
            return None;
        };
        self.by_surface.remove(&(client_id, constraint.surface));
        let hint = if constraint.phase == ConstraintPhase::Active
            && constraint.kind == ConstraintKind::Locked
        {
            constraint.cursor_hint
        } else {
            None
        };
        Some((constraint.surface, hint))
    }

    pub fn mark_surface_destroyed(
        &mut self,
        writer: &mut lumalla_wayland_protocol::buffer::Writer,
        client_id: ClientId,
        surface: ObjectId,
    ) {
        let Some(object_id) = self.by_surface.get(&(client_id, surface)).copied() else {
            return;
        };
        let Some(constraint) = self.constraints.get_mut(&(client_id, object_id)) else {
            return;
        };
        if constraint.phase == ConstraintPhase::Active {
            match constraint.kind {
                ConstraintKind::Locked => {
                    writer.zwp_locked_pointer_v1_unlocked(object_id);
                }
                ConstraintKind::Confined => {
                    writer.zwp_confined_pointer_v1_unconfined(object_id);
                }
            }
        }
        if let Some(constraint) = self.constraints.get_mut(&(client_id, object_id)) {
            constraint.phase = ConstraintPhase::Defunct;
        }
    }

    pub fn delete_client(&mut self, client_id: ClientId) {
        self.constraints.retain(|(cid, _), _| *cid != client_id);
        self.by_surface.retain(|(cid, _), _| *cid != client_id);
    }

    pub fn remove_pointer(&mut self, client_id: ClientId, pointer: ObjectId) {
        let to_remove: Vec<ObjectId> = self
            .constraints
            .iter()
            .filter(|((cid, _), c)| *cid == client_id && c.pointer == pointer)
            .map(|((_, oid), _)| *oid)
            .collect();
        for object_id in to_remove {
            if let Some(constraint) = self.constraints.remove(&(client_id, object_id)) {
                self.by_surface.remove(&(client_id, constraint.surface));
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn force_active_for_test(&mut self, client_id: ClientId, object_id: ObjectId) {
        if let Some(constraint) = self.constraints.get_mut(&(client_id, object_id)) {
            constraint.phase = ConstraintPhase::Active;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use lumalla_wayland_protocol::{ClientId, ObjectId};

    use super::*;

    fn client(id: u32) -> ClientId {
        ClientId::new(NonZeroU32::new(id).unwrap())
    }

    fn object(id: u32) -> ObjectId {
        ObjectId::new(NonZeroU32::new(id).unwrap())
    }

    #[test]
    fn rejects_second_constraint_on_same_surface() {
        let mut manager = PointerConstraintsManager::default();
        manager
            .create_constraint(
                client(1),
                object(10),
                ConstraintKind::Locked,
                object(2),
                object(3),
                None,
                ConstraintLifetime::Oneshot,
            )
            .unwrap();
        assert!(matches!(
            manager.create_constraint(
                client(1),
                object(11),
                ConstraintKind::Confined,
                object(2),
                object(3),
                None,
                ConstraintLifetime::Persistent,
            ),
            Err(ConstraintError::AlreadyConstrained)
        ));
    }

    #[test]
    fn destroy_clears_surface_slot() {
        let mut manager = PointerConstraintsManager::default();
        manager
            .create_constraint(
                client(1),
                object(10),
                ConstraintKind::Locked,
                object(2),
                object(3),
                None,
                ConstraintLifetime::Persistent,
            )
            .unwrap();
        let _ = manager.destroy(client(1), object(10));
        manager
            .create_constraint(
                client(1),
                object(11),
                ConstraintKind::Confined,
                object(2),
                object(3),
                None,
                ConstraintLifetime::Oneshot,
            )
            .unwrap();
    }

    #[test]
    fn apply_commit_updates_region() {
        let mut manager = PointerConstraintsManager::default();
        manager
            .create_constraint(
                client(1),
                object(10),
                ConstraintKind::Confined,
                object(2),
                object(3),
                None,
                ConstraintLifetime::Persistent,
            )
            .unwrap();
        // Force active so apply_surface_commit returns the confine.
        manager.force_active_for_test(client(1), object(10));
        manager
            .set_pending_region(client(1), object(10), Some(Region::default()))
            .unwrap();
        let confined = manager.apply_surface_commit(client(1), object(2));
        assert_eq!(confined.len(), 1);
        assert!(manager.region_for(client(1), object(10)).is_some());
    }
}
