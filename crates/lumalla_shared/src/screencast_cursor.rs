//! Screencast cursor inclusion mode (Mutter ScreenCast / portal).

/// How the pointer should appear in a PipeWire screencast.
///
/// Matches `org.gnome.Mutter.ScreenCast` `cursor-mode` values:
/// `0` hidden, `1` embedded, `2` metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScreencastCursorMode {
    /// Do not include the cursor in captured frames.
    #[default]
    Hidden,
    /// Composite the cursor into the framebuffer.
    Embedded,
}

impl ScreencastCursorMode {
    /// Parse a Mutter / portal `cursor-mode` property.
    ///
    /// Metadata (`2`) is treated as embedded until SPA cursor metadata is implemented.
    pub fn from_mutter(value: u32) -> Self {
        match value {
            1 | 2 => Self::Embedded,
            _ => Self::Hidden,
        }
    }

    /// Whether the cursor should be drawn into capture buffers.
    pub fn embed(self) -> bool {
        matches!(self, Self::Embedded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mutter_cursor_modes() {
        assert_eq!(ScreencastCursorMode::from_mutter(0), ScreencastCursorMode::Hidden);
        assert_eq!(ScreencastCursorMode::from_mutter(1), ScreencastCursorMode::Embedded);
        assert_eq!(ScreencastCursorMode::from_mutter(2), ScreencastCursorMode::Embedded);
        assert_eq!(ScreencastCursorMode::from_mutter(99), ScreencastCursorMode::Hidden);
    }
}
