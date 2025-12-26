use std::collections::HashMap;

use lumalla_wayland_protocol::{ClientId, ObjectId};

#[derive(Default)]
pub struct SurfaceManager {
    surfaces: HashMap<(ClientId, ObjectId), Surface>,
}

impl SurfaceManager {
    pub fn create_surface(&mut self, client_id: ClientId, id: ObjectId) {
        self.surfaces
            .insert((client_id, id), Surface { id, client_id });
    }
}

struct Surface {
    id: ObjectId,
    client_id: ClientId,
}
