/// Represents a zone in logical compositor space. A zone is a rectangular area that is used for window placement.
#[derive(Debug)]
pub struct Zone {
    /// The name of the zone
    pub name: String,
    /// The geometry of the zone
    pub geometry: (i32, i32, i32, i32),
    /// Whether the zone is the default zone
    pub default: bool,
}

impl Zone {
    /// Creates a new instance from the given name, offset, size and default flag
    pub fn new(name: String, x: i32, y: i32, width: i32, height: i32, default: bool) -> Self {
        Self {
            name,
            geometry: (x, y, width, height),
            default,
        }
    }
}
