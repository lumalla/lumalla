use std::collections::HashMap;

use lumalla_wayland_protocol::{ClientId, ObjectId};

#[derive(Default)]
pub struct SurfaceManager {
    surfaces: HashMap<(ClientId, ObjectId), Surface>,
}

impl SurfaceManager {
    pub fn create_surface(&mut self, client_id: ClientId, id: ObjectId) {
        self.surfaces
            .insert((client_id, id), Surface::new(id, client_id));
    }

    #[must_use]
    pub fn set_pending_buffer(
        &mut self,
        client_id: ClientId,
        id: ObjectId,
        buffer: Option<ObjectId>,
        x: i32,
        y: i32,
    ) -> bool {
        let Some(surface) = self.surfaces.get_mut(&(client_id, id)) else {
            return false;
        };
        surface.pending.buffer = Some(buffer);
        surface.pending.x = Some(x);
        surface.pending.y = Some(y);
        true
    }
}

struct Surface {
    id: ObjectId,
    client_id: ClientId,
    role: Option<Role>,
    buffer: Option<ObjectId>,
    pending: Pending,
}

impl Surface {
    fn new(id: ObjectId, client_id: ClientId) -> Self {
        Self {
            id,
            client_id,
            role: None,
            buffer: None,
            pending: Pending::default(),
        }
    }
}

enum Role {}

#[derive(Default)]
struct Pending {
    buffer: Option<Option<ObjectId>>,
    x: Option<i32>,
    y: Option<i32>,
}
