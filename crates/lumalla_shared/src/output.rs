#[derive(Debug, Clone)]
pub struct Output {
    /// The name of the output
    pub name: String,
    /// The description of the output
    pub description: String,
    /// The location of the output
    pub location: (i32, i32),
    /// The size of the output
    pub size: (i32, i32),
}

impl Output {
    /// Sets the location of the output
    pub fn set_location(&mut self, x: i32, y: i32) {
        self.location = (x, y);
    }
}
