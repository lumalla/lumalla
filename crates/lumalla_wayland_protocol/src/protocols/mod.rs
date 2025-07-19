pub mod wayland;

pub use wayland::WlDisplay;

use crate::registry::Registry;

pub struct Ctx<'registry> {
    pub registry: &'registry mut Registry,
    pub responder: (),
}
