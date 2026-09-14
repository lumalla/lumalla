/// A view maps a rectangle of global compositor space onto an output-local dest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// Unique name within the owning output.
    pub name: String,
    /// Source rectangle in global compositor space: `(x, y, width, height)`.
    pub source: (i32, i32, i32, i32),
    /// Destination rectangle in output-local coordinates: `(x, y, width, height)`.
    pub dest: (i32, i32, i32, i32),
}

impl View {
    /// Update source/dest sizes that matched the previous output mode.
    ///
    /// Full-output destinations (`0,0,old_w,old_h`) become the new mode size.
    /// Sources whose size matched the old mode keep their origin and adopt the new size.
    pub fn resize_for_output_mode(&mut self, old_size: (i32, i32), new_size: (i32, i32)) {
        let (old_w, old_h) = old_size;
        let (new_w, new_h) = new_size;
        if old_w <= 0 || old_h <= 0 || old_size == new_size {
            return;
        }
        let (dx, dy, dw, dh) = self.dest;
        if dx == 0 && dy == 0 && dw == old_w && dh == old_h {
            self.dest = (0, 0, new_w, new_h);
        }
        let (sx, sy, sw, sh) = self.source;
        if sw == old_w && sh == old_h {
            self.source = (sx, sy, new_w, new_h);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// The name of the output
    pub name: String,
    /// The description of the output
    pub description: String,
    /// Views composited onto this output (paint order: later draws on top).
    ///
    /// [`Self::location`] is derived from the first view's source origin.
    pub views: Vec<View>,
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
    /// Logical location in global compositor space from the first view's source origin.
    ///
    /// Returns `(0, 0)` when the output has no views.
    pub fn location(&self) -> (i32, i32) {
        self.views
            .first()
            .map(|view| (view.source.0, view.source.1))
            .unwrap_or((0, 0))
    }

    /// Resize views that were sized to the previous output mode.
    pub fn resize_views_for_mode(&mut self, old_size: (i32, i32), new_size: (i32, i32)) {
        for view in &mut self.views {
            view.resize_for_output_mode(old_size, new_size);
        }
    }
}

impl Default for Output {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            views: Vec::new(),
            size: (0, 0),
            scale: 1,
            refresh_mhz: 60_000,
            physical_width_mm: 0,
            physical_height_mm: 0,
            is_virtual: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_updates_full_output_view() {
        let mut view = View {
            name: "main".into(),
            source: (1920, 0, 1920, 1080),
            dest: (0, 0, 1920, 1080),
        };
        view.resize_for_output_mode((1920, 1080), (2560, 1440));
        assert_eq!(view.source, (1920, 0, 2560, 1440));
        assert_eq!(view.dest, (0, 0, 2560, 1440));
    }

    #[test]
    fn resize_leaves_pip_dest_alone() {
        let mut view = View {
            name: "pip".into(),
            source: (0, 0, 640, 480),
            dest: (100, 100, 320, 240),
        };
        view.resize_for_output_mode((1920, 1080), (2560, 1440));
        assert_eq!(view.source, (0, 0, 640, 480));
        assert_eq!(view.dest, (100, 100, 320, 240));
    }
}
