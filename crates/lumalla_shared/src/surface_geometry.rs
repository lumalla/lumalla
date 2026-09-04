//! Geometry helpers for Wayland buffer transforms.

/// A `wl_output_transform` value used by `wl_surface.set_buffer_transform`.
#[repr(u32)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BufferTransform {
    #[default]
    Normal = 0,
    Rotate90 = 1,
    Rotate180 = 2,
    Rotate270 = 3,
    Flipped = 4,
    Flipped90 = 5,
    Flipped180 = 6,
    Flipped270 = 7,
}

impl BufferTransform {
    pub const ALL: [Self; 8] = [
        Self::Normal,
        Self::Rotate90,
        Self::Rotate180,
        Self::Rotate270,
        Self::Flipped,
        Self::Flipped90,
        Self::Flipped180,
        Self::Flipped270,
    ];

    pub const fn from_raw(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Normal),
            1 => Some(Self::Rotate90),
            2 => Some(Self::Rotate180),
            3 => Some(Self::Rotate270),
            4 => Some(Self::Flipped),
            5 => Some(Self::Flipped90),
            6 => Some(Self::Flipped180),
            7 => Some(Self::Flipped270),
            _ => None,
        }
    }

    pub const fn swaps_axes(self) -> bool {
        matches!(
            self,
            Self::Rotate90 | Self::Rotate270 | Self::Flipped90 | Self::Flipped270
        )
    }

    /// Size of a buffer after applying this transform.
    pub const fn transformed_size(self, width: usize, height: usize) -> (usize, usize) {
        if self.swaps_axes() {
            (height, width)
        } else {
            (width, height)
        }
    }

    /// Surface-local size after applying the transform and integer buffer scale.
    pub const fn scaled_size(
        self,
        width: usize,
        height: usize,
        buffer_scale: i32,
    ) -> (usize, usize) {
        let (width, height) = self.transformed_size(width, height);
        let scale = if buffer_scale < 1 {
            1
        } else {
            buffer_scale as usize
        };
        (width / scale, height / scale)
    }

    /// Source crop in transformed buffer pixels. Wayland viewport source
    /// coordinates are post-transform, post-scale surface coordinates.
    pub fn transformed_source_rect(
        self,
        buffer_width: usize,
        buffer_height: usize,
        buffer_scale: i32,
        viewport_src: Option<(f32, f32, f32, f32)>,
    ) -> [f64; 4] {
        let scale = buffer_scale.max(1) as f64;
        match viewport_src {
            Some((x, y, width, height)) => [
                x as f64 * scale,
                y as f64 * scale,
                width as f64 * scale,
                height as f64 * scale,
            ],
            None => {
                let (width, height) = self.transformed_size(buffer_width, buffer_height);
                [0.0, 0.0, width as f64, height as f64]
            }
        }
    }

    /// Maps a buffer pixel into transformed (surface-oriented) space.
    pub const fn buffer_to_transformed(
        self,
        x: usize,
        y: usize,
        buffer_width: usize,
        buffer_height: usize,
    ) -> (usize, usize) {
        match self {
            Self::Normal => (x, y),
            Self::Rotate90 => (y, buffer_width - 1 - x),
            Self::Rotate180 => (buffer_width - 1 - x, buffer_height - 1 - y),
            Self::Rotate270 => (buffer_height - 1 - y, x),
            Self::Flipped => (buffer_width - 1 - x, y),
            Self::Flipped90 => (y, x),
            Self::Flipped180 => (x, buffer_height - 1 - y),
            Self::Flipped270 => (buffer_height - 1 - y, buffer_width - 1 - x),
        }
    }

    /// Maps an integer pixel in transformed (surface-oriented) space back to
    /// the corresponding pixel in the original buffer.
    pub const fn transformed_to_buffer(
        self,
        x: usize,
        y: usize,
        buffer_width: usize,
        buffer_height: usize,
    ) -> (usize, usize) {
        match self {
            Self::Normal => (x, y),
            Self::Rotate90 => (buffer_width - 1 - y, x),
            Self::Rotate180 => (buffer_width - 1 - x, buffer_height - 1 - y),
            Self::Rotate270 => (y, buffer_height - 1 - x),
            Self::Flipped => (buffer_width - 1 - x, y),
            Self::Flipped90 => (y, x),
            Self::Flipped180 => (x, buffer_height - 1 - y),
            Self::Flipped270 => (buffer_width - 1 - y, buffer_height - 1 - x),
        }
    }

    /// Maps a half-open rectangle from transformed buffer space back to a
    /// conservative, axis-aligned rectangle in original buffer space.
    pub fn transformed_rect_to_buffer(
        self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        buffer_width: u32,
        buffer_height: u32,
    ) -> (u32, u32, u32, u32) {
        let x1 = x.saturating_add(width);
        let y1 = y.saturating_add(height);
        match self {
            Self::Normal => (x, y, width, height),
            Self::Rotate90 => (buffer_width.saturating_sub(y1), x, height, width),
            Self::Rotate180 => (
                buffer_width.saturating_sub(x1),
                buffer_height.saturating_sub(y1),
                width,
                height,
            ),
            Self::Rotate270 => (y, buffer_height.saturating_sub(x1), height, width),
            Self::Flipped => (buffer_width.saturating_sub(x1), y, width, height),
            Self::Flipped90 => (y, x, height, width),
            Self::Flipped180 => (x, buffer_height.saturating_sub(y1), width, height),
            Self::Flipped270 => (
                buffer_width.saturating_sub(y1),
                buffer_height.saturating_sub(x1),
                height,
                width,
            ),
        }
    }

    /// Maps a half-open rectangle from original buffer space into transformed
    /// (surface-oriented) buffer space.
    pub fn buffer_rect_to_transformed(
        self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        buffer_width: u32,
        buffer_height: u32,
    ) -> (u32, u32, u32, u32) {
        let x1 = x.saturating_add(width);
        let y1 = y.saturating_add(height);
        match self {
            Self::Normal => (x, y, width, height),
            Self::Rotate90 => (y, buffer_width.saturating_sub(x1), height, width),
            Self::Rotate180 => (
                buffer_width.saturating_sub(x1),
                buffer_height.saturating_sub(y1),
                width,
                height,
            ),
            Self::Rotate270 => (buffer_height.saturating_sub(y1), x, height, width),
            Self::Flipped => (buffer_width.saturating_sub(x1), y, width, height),
            Self::Flipped90 => (y, x, height, width),
            Self::Flipped180 => (x, buffer_height.saturating_sub(y1), width, height),
            Self::Flipped270 => (
                buffer_height.saturating_sub(y1),
                buffer_width.saturating_sub(x1),
                height,
                width,
            ),
        }
    }
}

impl TryFrom<u32> for BufferTransform {
    type Error = u32;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::from_raw(value).ok_or(value)
    }
}

#[cfg(test)]
mod tests {
    use super::BufferTransform;

    #[test]
    fn all_eight_transforms_have_deterministic_pixel_maps() {
        // A 3x2 buffer containing pixels laid out as:
        //   0 1 2
        //   3 4 5
        // Expected rows after each buffer-to-surface transform.
        let expected: [&[&[usize]]; 8] = [
            &[&[0, 1, 2], &[3, 4, 5]],
            &[&[2, 5], &[1, 4], &[0, 3]],
            &[&[5, 4, 3], &[2, 1, 0]],
            &[&[3, 0], &[4, 1], &[5, 2]],
            &[&[2, 1, 0], &[5, 4, 3]],
            &[&[0, 3], &[1, 4], &[2, 5]],
            &[&[3, 4, 5], &[0, 1, 2]],
            &[&[5, 2], &[4, 1], &[3, 0]],
        ];

        for (transform, rows) in BufferTransform::ALL.into_iter().zip(expected) {
            let (width, height) = transform.transformed_size(3, 2);
            assert_eq!(height, rows.len(), "{transform:?}");
            assert_eq!(width, rows[0].len(), "{transform:?}");
            for (y, row) in rows.iter().enumerate() {
                for (x, expected_index) in row.iter().enumerate() {
                    let (bx, by) = transform.transformed_to_buffer(x, y, 3, 2);
                    assert_eq!(by * 3 + bx, *expected_index, "{transform:?} ({x}, {y})");
                }
            }
        }
    }

    #[test]
    fn transformed_rect_mapping_matches_each_pixel_extent() {
        for transform in BufferTransform::ALL {
            let (tw, th) = transform.transformed_size(4, 3);
            for y in 0..th {
                for x in 0..tw {
                    let expected = transform.transformed_to_buffer(x, y, 4, 3);
                    let actual =
                        transform.transformed_rect_to_buffer(x as u32, y as u32, 1, 1, 4, 3);
                    assert_eq!(actual, (expected.0 as u32, expected.1 as u32, 1, 1));
                }
            }
        }
    }

    #[test]
    fn buffer_rect_mapping_matches_each_pixel_extent() {
        for transform in BufferTransform::ALL {
            for y in 0..3 {
                for x in 0..4 {
                    let expected = transform.buffer_to_transformed(x, y, 4, 3);
                    let actual =
                        transform.buffer_rect_to_transformed(x as u32, y as u32, 1, 1, 4, 3);
                    assert_eq!(actual, (expected.0 as u32, expected.1 as u32, 1, 1));
                }
            }
        }
    }

    #[test]
    fn point_mappings_round_trip_for_all_transforms() {
        for transform in BufferTransform::ALL {
            for y in 0..3 {
                for x in 0..4 {
                    let transformed = transform.buffer_to_transformed(x, y, 4, 3);
                    assert_eq!(
                        transform.transformed_to_buffer(transformed.0, transformed.1, 4, 3),
                        (x, y),
                        "{transform:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn scale_and_viewport_are_applied_in_transformed_space() {
        assert_eq!(BufferTransform::Rotate90.scaled_size(6, 4, 2), (2, 3));
        assert_eq!(
            BufferTransform::Rotate90.transformed_source_rect(6, 4, 2, Some((0.5, 1.0, 1.5, 2.0))),
            [1.0, 2.0, 3.0, 4.0]
        );
        assert_eq!(
            BufferTransform::Rotate90.transformed_source_rect(6, 4, 2, None),
            [0.0, 0.0, 4.0, 6.0]
        );
    }
}
