use std::collections::{HashMap, HashSet};

use lumalla_wayland_protocol::{ClientConnection, registry::InterfaceIndex};

use crate::{GlobalId, Globals};

pub struct SeatManager {
    known_seats: HashSet<String>,
    id_to_name: HashMap<GlobalId, String>,
}

impl Default for SeatManager {
    fn default() -> Self {
        Self {
            known_seats: HashSet::new(),
            id_to_name: HashMap::new(),
        }
    }
}

impl SeatManager {
    /// Adds a seat with the given name to the seat manager.
    pub fn add_seat<'connection>(
        &mut self,
        seat_name: String,
        globals: &mut Globals,
        client_connections: impl Iterator<Item = &'connection mut ClientConnection>,
    ) {
        let is_new_seat = self.known_seats.insert(seat_name.clone());
        if is_new_seat {
            let id = globals.register(InterfaceIndex::WlSeat, client_connections);
            self.id_to_name.insert(id, seat_name);
        }
    }

    pub fn get_name(&self, id: GlobalId) -> Option<&str> {
        self.id_to_name.get(&id).map(|s| s.as_str())
    }
}
