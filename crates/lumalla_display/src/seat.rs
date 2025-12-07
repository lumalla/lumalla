use std::collections::{HashMap, HashSet};

use lumalla_wayland_protocol::registry::InterfaceIndex;

use crate::Globals;

pub struct SeatManager {
    known_seats: HashSet<String>,
    id_to_name: HashMap<u32, String>,
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
    pub fn add_seat(&mut self, seat_name: String, globals: &mut Globals) {
        let is_new_seat = self.known_seats.insert(seat_name.clone());
        if is_new_seat {
            let id = globals.register(InterfaceIndex::WlSeat);
            self.id_to_name.insert(id, seat_name);
        }
    }
}
