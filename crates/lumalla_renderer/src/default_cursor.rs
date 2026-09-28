use std::cell::RefCell;
use std::rc::Rc;

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
            let i = (y * SIZE + x) * 4;
            match MASK[y][x] {
                1 => {
                    pixels[i] = 0;
                    pixels[i + 1] = 0;
                    pixels[i + 2] = 0;
                    pixels[i + 3] = 255;
                }
                2 => {
                    pixels[i] = 255;
                    pixels[i + 1] = 255;
                    pixels[i + 2] = 255;
                    pixels[i + 3] = 255;
                }
                _ => {}
            }
        }
    }
    pixels
}

fn build_default_cursor_frame() -> CursorFrame {
    CursorFrame {
        owner_id: 0,
        surface_id: 0,
        buffer_id: 0,
        pixels: Rc::new(build_default_cursor_pixels()),
        width: SIZE,
        height: SIZE,
        stride: SIZE * 4,
        format: WL_SHM_FORMAT_ARGB8888,
        hotspot_x: 0,
        hotspot_y: 0,
        buffer_scale: 1,
        buffer_transform: 0,
        dmabuf: None,
    }
}

thread_local! {
    static CURSOR: RefCell<Option<CursorFrame>> = const { RefCell::new(None) };
}

/// Default compositor cursor. Cheap to call: clones an `Rc` to the shared pixels.
pub fn default_cursor_frame() -> CursorFrame {
    CURSOR.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(build_default_cursor_frame());
        }
        let cursor = slot.as_ref().expect("default cursor initialized");
        CursorFrame {
            owner_id: cursor.owner_id,
            surface_id: cursor.surface_id,
            buffer_id: cursor.buffer_id,
            pixels: Rc::clone(&cursor.pixels),
            width: cursor.width,
            height: cursor.height,
            stride: cursor.stride,
            format: cursor.format,
            hotspot_x: cursor.hotspot_x,
            hotspot_y: cursor.hotspot_y,
            buffer_scale: cursor.buffer_scale,
            buffer_transform: cursor.buffer_transform,
            dmabuf: None,
        }
    })
}
