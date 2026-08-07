#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// The name of the output
    pub name: String,
    /// The description of the output
    pub description: String,
    /// The location of the output
    pub location: (i32, i32),
    /// The size of the output
    pub size: (i32, i32),
    /// Buffer scale factor advertised to clients.
    pub scale: i32,
    /// Refresh rate in mHz (Wayland `wl_output.mode` units).
    pub refresh_mhz: i32,
    /// Physical width in millimeters.
    pub physical_width_mm: i32,
    /// Physical height in millimeters.
    pub physical_height_mm: i32,
    /// Whether this is a config-created virtual output (no DRM connector).
    pub is_virtual: bool,
}

impl Output {
    /// Sets the location of the output
    pub fn set_location(&mut self, x: i32, y: i32) {
        self.location = (x, y);
    }
}

impl Default for Output {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            location: (0, 0),
            size: (0, 0),
            scale: 1,
            refresh_mhz: 60_000,
            physical_width_mm: 0,
            physical_height_mm: 0,
            is_virtual: false,
        }
    }
}
