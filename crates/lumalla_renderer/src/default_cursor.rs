use std::sync::OnceLock;

use super::{CursorFrame, WL_SHM_FORMAT_ARGB8888};

const SIZE: usize = 16;

/// Classic white arrow with black outline, hotspot at the tip (0, 0).
fn build_default_cursor_pixels() -> Vec<u8> {
    // 0 = transparent, 1 = black outline, 2 = white fill
    const MASK: [[u8; SIZE]; SIZE] = [
        [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        [1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        [1, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        [1, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        [1, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        [1, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        [1, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        [1, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0],
        [1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0],
        [1, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0],
        [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0],
        [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0],
        [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0],
        [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 2, 0, 0],
        [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 2, 0, 0, 0],
    ];

    let mut pixels = vec![0u8; SIZE * SIZE * 4];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let idx = (y * SIZE + x) * 4;
            match MASK[y][x] {
                1 => pixels[idx..idx + 4].copy_from_slice(&[0, 0, 0, 255]),
                2 => pixels[idx..idx + 4].copy_from_slice(&[255, 255, 255, 255]),
                _ => {}
            }
        }
    }
    pixels
}

pub fn default_cursor_frame() -> &'static CursorFrame {
    static CURSOR: OnceLock<CursorFrame> = OnceLock::new();
    CURSOR.get_or_init(|| CursorFrame {
        owner_id: 0,
        surface_id: 0,
        pixels: build_default_cursor_pixels(),
        width: SIZE,
        height: SIZE,
        stride: SIZE * 4,
        format: WL_SHM_FORMAT_ARGB8888,
        hotspot_x: 0,
        hotspot_y: 0,
        buffer_scale: 1,
    })
}
