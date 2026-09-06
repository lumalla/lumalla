use std::collections::HashMap;

use lumalla_wayland_protocol::{
    ClientConnection, ClientId, ObjectId,
    buffer::Writer,
};

type ResourceKey = (ClientId, ObjectId);

#[derive(Debug, Clone, Copy)]
struct RelativePointer {
    pointer: ObjectId,
}

#[derive(Debug, Default)]
pub struct RelativePointerManager {
    /// `(client, relative_pointer) -> wl_pointer`
    relatives: HashMap<ResourceKey, RelativePointer>,
}

impl RelativePointerManager {
    pub fn create(
        &mut self,
        client_id: ClientId,
        relative_id: ObjectId,
        pointer: ObjectId,
    ) {
        self.relatives.insert(
            (client_id, relative_id),
            RelativePointer { pointer },
        );
    }

    pub fn destroy(&mut self, client_id: ClientId, relative_id: ObjectId) {
        self.relatives.remove(&(client_id, relative_id));
    }

    pub fn remove_pointer(&mut self, client_id: ClientId, pointer: ObjectId) {
        self.relatives
            .retain(|(cid, _), rel| !(*cid == client_id && rel.pointer == pointer));
    }

    pub fn delete_client(&mut self, client_id: ClientId) {
        self.relatives.retain(|(cid, _), _| *cid != client_id);
    }

    /// Emit `relative_motion` for every relative pointer whose `wl_pointer` currently has focus.
    pub fn emit_relative_motion(
        &self,
        clients: &mut HashMap<ClientId, ClientConnection>,
        focused_pointers: &[(ClientId, ObjectId)],
        time_msec: u32,
        dx: f64,
        dy: f64,
        dx_unaccel: f64,
        dy_unaccel: f64,
    ) {
        if focused_pointers.is_empty() {
            return;
        }
        let utime = (time_msec as u64).saturating_mul(1000);
        let utime_hi = (utime >> 32) as u32;
        let utime_lo = utime as u32;

        for (&(client_id, relative_id), rel) in &self.relatives {
            let focused = focused_pointers
                .iter()
                .any(|(cid, pid)| *cid == client_id && *pid == rel.pointer);
            if !focused {
                continue;
            }
            let Some(client) = clients.get_mut(&client_id) else {
                continue;
            };
            write_relative_motion(
                client.writer_mut(),
                relative_id,
                utime_hi,
                utime_lo,
                dx,
                dy,
                dx_unaccel,
                dy_unaccel,
            );
        }
    }
}

fn write_relative_motion(
    writer: &mut Writer,
    relative_id: ObjectId,
    utime_hi: u32,
    utime_lo: u32,
    dx: f64,
    dy: f64,
    dx_unaccel: f64,
    dy_unaccel: f64,
) {
    writer
        .zwp_relative_pointer_v1_relative_motion(relative_id)
        .utime_hi(utime_hi)
        .utime_lo(utime_lo)
        .dx(dx as f32)
        .dy(dy as f32)
        .dx_unaccel(dx_unaccel as f32)
        .dy_unaccel(dy_unaccel as f32);
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
    fn destroy_and_pointer_removal_clear_entries() {
        let mut manager = RelativePointerManager::default();
        manager.create(client(1), object(10), object(5));
        manager.create(client(1), object(11), object(6));
        assert_eq!(manager.relatives.len(), 2);
        manager.destroy(client(1), object(10));
        assert_eq!(manager.relatives.len(), 1);
        manager.remove_pointer(client(1), object(6));
        assert!(manager.relatives.is_empty());
    }
}
