/// Premultiplied-friendly RGBA color (channels 0–255, straight alpha).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorRgba {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
    /// Alpha channel.
    pub a: u8,
}

impl ColorRgba {
    /// Fully opaque white.
    pub const WHITE: Self = Self {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    };

    /// Default guide stroke color (amber).
    pub const DEFAULT_STROKE: Self = Self {
        r: 255,
        g: 180,
        b: 0,
        a: 200,
    };

    /// Convert to premultiplied `[r, g, b, a]` floats in 0..=1.
    pub fn premultiplied_f32(self) -> [f32; 4] {
        let a = self.a as f32 / 255.0;
        [
            (self.r as f32 / 255.0) * a,
            (self.g as f32 / 255.0) * a,
            (self.b as f32 / 255.0) * a,
            a,
        ]
    }
}

/// Whether a guide is drawn under or over the window stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuideLayer {
    /// Drawn after clear, before windows.
    Below,
    /// Drawn after windows, before the cursor.
    Above,
}

impl GuideLayer {
    /// Parse `"above"` / `"below"` (empty defaults to below).
    pub fn parse(s: &str) -> Self {
        match s {
            "above" => Self::Above,
            _ => Self::Below,
        }
    }

    /// Wire / Lua string form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Below => "below",
            Self::Above => "above",
        }
    }
}

/// Guide geometry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuideKind {
    /// Axis-aligned rectangle in compositor (scene) space.
    Box {
        /// Left edge.
        x: i32,
        /// Top edge.
        y: i32,
        /// Width in pixels.
        width: i32,
        /// Height in pixels.
        height: i32,
    },
    /// Line segment in compositor (scene) space.
    Line {
        /// Start X.
        x1: i32,
        /// Start Y.
        y1: i32,
        /// End X.
        x2: i32,
        /// End Y.
        y2: i32,
    },
}

impl GuideKind {
    /// Parse `"box"` / `"line"` (empty defaults to box).
    pub fn parse_tag(s: &str) -> &'static str {
        match s {
            "line" => "line",
            _ => "box",
        }
    }
}

/// A named compositor-drawn helper (line or box), optionally labeled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guide {
    /// Stable config id; `add_guide` replaces by name.
    pub name: String,
    /// Geometry.
    pub kind: GuideKind,
    /// Stacking relative to windows.
    pub layer: GuideLayer,
    /// Stroke / line color.
    pub color: ColorRgba,
    /// Stroke width in scene pixels (also used for line thickness).
    pub stroke: i32,
    /// Optional box fill; ignored for lines.
    pub fill: Option<ColorRgba>,
    /// Optional user-visible label (`A–Za–z0–9` glyphs; others blank).
    pub label: String,
    /// Label color; when `None`, uses [`Self::color`].
    pub label_color: Option<ColorRgba>,
}

impl Guide {
    /// Effective label draw color.
    pub fn effective_label_color(&self) -> ColorRgba {
        self.label_color.unwrap_or(self.color)
    }
}
