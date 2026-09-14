/// How a zone places windows when they join it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompositionStrategy {
    /// Place at the zone anchor with a fixed default size; the window may move/resize freely afterward.
    Free {
        /// Initial configure width.
        default_width: i32,
        /// Initial configure height.
        default_height: i32,
    },
}

/// A named placement anchor with a composition strategy.
///
/// Zones do not define a bounding rectangle — only an anchor point. Size and
/// layout policy come from [`CompositionStrategy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    /// The name of the zone.
    pub name: String,
    /// Placement origin `(x, y)` in compositor space.
    pub anchor: (i32, i32),
    /// Whether this is the default zone for newly spawned top-level windows.
    pub default: bool,
    /// How windows are placed when they join this zone.
    pub composition: CompositionStrategy,
}

impl Zone {
    /// Creates a zone with the given name, anchor, default flag, and strategy.
    pub fn new(
        name: String,
        x: i32,
        y: i32,
        default: bool,
        composition: CompositionStrategy,
    ) -> Self {
        Self {
            name,
            anchor: (x, y),
            default,
            composition,
        }
    }

    /// Initial `(x, y, width, height)` for a window joining this zone.
    pub fn place(&self) -> (i32, i32, i32, i32) {
        match self.composition {
            CompositionStrategy::Free {
                default_width,
                default_height,
            } => {
                let (x, y) = self.anchor;
                (x, y, default_width, default_height)
            }
        }
    }
}
