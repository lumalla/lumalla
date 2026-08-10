//! Persistent CPU backing store and incremental compositing.

use anyhow::Context;

use crate::default_cursor::default_cursor_frame;
use crate::{CursorFrame, SurfaceFrame};

const WL_SHM_FORMAT_ARGB8888: u32 = 0;
const WL_SHM_FORMAT_XRGB8888: u32 = 1;
pub const MAX_DAMAGE_RECTS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamageRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompositeMode {
    Full,
    Partial(Vec<UploadRect>),
}

#[derive(Debug, Clone)]
pub struct SceneBacking {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl SceneBacking {
    pub fn new(width: u32, height: u32, clear: [f32; 4]) -> anyhow::Result<Self> {
        let pixels = clear_pixels(width, height, clear)?;
        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32, clear: [f32; 4]) -> anyhow::Result<()> {
        self.width = width;
        self.height = height;
        self.pixels = clear_pixels(width, height, clear)?;
        Ok(())
    }
}

pub fn cursor_damage_rects(
    cursor: &CursorFrame,
    old_pointer: (i32, i32),
    new_pointer: (i32, i32),
) -> Vec<DamageRect> {
    let old = cursor_bounds(cursor, old_pointer.0, old_pointer.1);
    let new = cursor_bounds(cursor, new_pointer.0, new_pointer.1);
    match (old, new) {
        (Some(a), Some(b)) => rect_union(a, b).into_iter().collect(),
        (Some(a), None) => vec![a],
        (None, Some(b)) => vec![b],
        (None, None) => Vec::new(),
    }
}

pub fn cursor_damage_rects_default(
    old_pointer: (i32, i32),
    new_pointer: (i32, i32),
) -> Vec<DamageRect> {
    cursor_damage_rects(default_cursor_frame(), old_pointer, new_pointer)
}

pub fn prepare_composite(
    backing: &mut Option<SceneBacking>,
    output_width: u32,
    output_height: u32,
    clear: [f32; 4],
    pending_damage: &[DamageRect],
    force_full: bool,
    frames: &[&SurfaceFrame],
    cursor: Option<&CursorFrame>,
    pointer_x: i32,
    pointer_y: i32,
) -> anyhow::Result<CompositeMode> {
    anyhow::ensure!(
        output_width > 0 && output_height > 0,
        "Output dimensions must be non-zero"
    );

    let size_changed = backing
        .as_ref()
        .is_none_or(|b| b.width != output_width || b.height != output_height);

    if force_full || size_changed || pending_damage.is_empty() {
        let pixels = composite_scene_full(
            frames,
            cursor,
            pointer_x,
            pointer_y,
            output_width,
            output_height,
            clear,
        )?;
        *backing = Some(SceneBacking {
            width: output_width,
            height: output_height,
            pixels,
        });
        return Ok(CompositeMode::Full);
    }

    let clipped = clip_damage_list(pending_damage, output_width, output_height);
    if clipped.len() > MAX_DAMAGE_RECTS {
        let pixels = composite_scene_full(
            frames,
            cursor,
            pointer_x,
            pointer_y,
            output_width,
            output_height,
            clear,
        )?;
        *backing = Some(SceneBacking {
            width: output_width,
            height: output_height,
            pixels,
        });
        return Ok(CompositeMode::Full);
    }

    let backing = backing
        .as_mut()
        .context("Scene backing missing for partial composite")?;

    for rect in &clipped {
        composite_region(
            &mut backing.pixels,
            output_width,
            output_height,
            *rect,
            clear,
            frames,
            cursor,
            pointer_x,
            pointer_y,
        )?;
    }

    let upload = clipped
        .into_iter()
        .map(|rect| UploadRect {
            x: rect.x.max(0) as u32,
            y: rect.y.max(0) as u32,
            width: rect.width.max(0) as u32,
            height: rect.height.max(0) as u32,
        })
        .filter(|rect| rect.width > 0 && rect.height > 0)
        .collect();

    Ok(CompositeMode::Partial(upload))
}

fn composite_scene_full(
    frames: &[&SurfaceFrame],
    cursor: Option<&CursorFrame>,
    pointer_x: i32,
    pointer_y: i32,
    output_width: u32,
    output_height: u32,
    clear: [f32; 4],
) -> anyhow::Result<Vec<u8>> {
    let mut upload = composite_surface_full(frames, output_width, output_height, clear)?;
    match cursor {
        Some(client) => composite_cursor_into(
            &mut upload,
            output_width as usize,
            output_height as usize,
            client,
            pointer_x,
            pointer_y,
        )?,
        None => composite_cursor_into(
            &mut upload,
            output_width as usize,
            output_height as usize,
            default_cursor_frame(),
            pointer_x,
            pointer_y,
        )?,
    }
    Ok(upload)
}

fn composite_region(
    pixels: &mut [u8],
    output_width: u32,
    output_height: u32,
    damage: DamageRect,
    clear: [f32; 4],
    frames: &[&SurfaceFrame],
    cursor: Option<&CursorFrame>,
    pointer_x: i32,
    pointer_y: i32,
) -> anyhow::Result<()> {
    let width = output_width as usize;
    let height = output_height as usize;
    let row_bytes = width
        .checked_mul(4)
        .context("Output row size overflows")?;
    let clear_b = (clear[2].clamp(0.0, 1.0) * 255.0) as u8;
    let clear_g = (clear[1].clamp(0.0, 1.0) * 255.0) as u8;
    let clear_r = (clear[0].clamp(0.0, 1.0) * 255.0) as u8;

    let x0 = damage.x.max(0) as usize;
    let y0 = damage.y.max(0) as usize;
    let x1 = damage
        .x
        .saturating_add(damage.width)
        .clamp(0, output_width as i32) as usize;
    let y1 = damage
        .y
        .saturating_add(damage.height)
        .clamp(0, output_height as i32) as usize;
    if x0 >= x1 || y0 >= y1 {
        return Ok(());
    }

    for y in y0..y1 {
        let row_start = y * row_bytes + x0 * 4;
        let row_end = y * row_bytes + x1 * 4;
        for chunk in pixels[row_start..row_end].chunks_exact_mut(4) {
            chunk.copy_from_slice(&[clear_b, clear_g, clear_r, 0xff]);
        }
    }

    for frame in frames {
        composite_frame_in_rect(
            pixels, width, height, x0, y0, x1, y1, frame,
        )?;
    }

    let cursor_rect = DamageRect {
        x: damage.x,
        y: damage.y,
        width: damage.width,
        height: damage.height,
    };
    match cursor {
        Some(client) => composite_cursor_in_rect(
            pixels,
            width,
            height,
            client,
            pointer_x,
            pointer_y,
            cursor_rect,
        )?,
        None => composite_cursor_in_rect(
            pixels,
            width,
            height,
            default_cursor_frame(),
            pointer_x,
            pointer_y,
            cursor_rect,
        )?,
    }
    Ok(())
}

fn composite_frame_in_rect(
    pixels: &mut [u8],
    output_width: usize,
    output_height: usize,
    clip_x0: usize,
    clip_y0: usize,
    clip_x1: usize,
    clip_y1: usize,
    frame: &SurfaceFrame,
) -> anyhow::Result<()> {
    frame.validate()?;
    let row_bytes = output_width
        .checked_mul(4)
        .context("Output row size overflows")?;
    let scale = frame.buffer_scale.max(1) as usize;
    let dest_w = frame.width / scale;
    let dest_h = frame.height / scale;
    if dest_w == 0 || dest_h == 0 {
        return Ok(());
    }

    for dy in 0..dest_h {
        let out_y = frame.y + dy as i32;
        if out_y < 0 || out_y as usize >= output_height {
            continue;
        }
        let oy = out_y as usize;
        if oy < clip_y0 || oy >= clip_y1 {
            continue;
        }
        let source_y = ((dy as u128 * frame.height as u128) / dest_h as u128) as usize;
        for dx in 0..dest_w {
            let out_x = frame.x + dx as i32;
            if out_x < 0 || out_x as usize >= output_width {
                continue;
            }
            let ox = out_x as usize;
            if ox < clip_x0 || ox >= clip_x1 {
                continue;
            }
            let source_x = ((dx as u128 * frame.width as u128) / dest_w as u128) as usize;
            let source = source_y * frame.stride + source_x * 4;
            let destination = oy * row_bytes + ox * 4;
            pixels[destination..destination + 4]
                .copy_from_slice(&frame.pixels[source..source + 4]);
            if frame.format == WL_SHM_FORMAT_XRGB8888 {
                pixels[destination + 3] = u8::MAX;
            }
        }
    }
    Ok(())
}

fn composite_cursor_in_rect(
    pixels: &mut [u8],
    output_width: usize,
    output_height: usize,
    cursor: &CursorFrame,
    pointer_x: i32,
    pointer_y: i32,
    clip: DamageRect,
) -> anyhow::Result<()> {
    cursor.validate()?;
    let row_bytes = output_width
        .checked_mul(4)
        .context("Output row size overflows")?;
    let scale = cursor.buffer_scale.max(1) as usize;
    let dest_w = cursor.width / scale;
    let dest_h = cursor.height / scale;
    if dest_w == 0 || dest_h == 0 {
        return Ok(());
    }
    let dest_x = pointer_x - cursor.hotspot_x;
    let dest_y = pointer_y - cursor.hotspot_y;
    let clip_x0 = clip.x.max(0) as usize;
    let clip_y0 = clip.y.max(0) as usize;
    let clip_x1 = clip
        .x
        .saturating_add(clip.width)
        .clamp(0, output_width as i32) as usize;
    let clip_y1 = clip
        .y
        .saturating_add(clip.height)
        .clamp(0, output_height as i32) as usize;

    for dy in 0..dest_h {
        let out_y = dest_y + dy as i32;
        if out_y < 0 || out_y as usize >= output_height {
            continue;
        }
        let oy = out_y as usize;
        if oy < clip_y0 || oy >= clip_y1 {
            continue;
        }
        let source_y = ((dy as u128 * cursor.height as u128) / dest_h as u128) as usize;
        for dx in 0..dest_w {
            let out_x = dest_x + dx as i32;
            if out_x < 0 || out_x as usize >= output_width {
                continue;
            }
            let ox = out_x as usize;
            if ox < clip_x0 || ox >= clip_x1 {
                continue;
            }
            let source_x = ((dx as u128 * cursor.width as u128) / dest_w as u128) as usize;
            let source = source_y * cursor.stride + source_x * 4;
            let destination = oy * row_bytes + ox * 4;
            let src = &cursor.pixels[source..source + 4];
            let alpha = if cursor.format == WL_SHM_FORMAT_ARGB8888 {
                src[3] as u16
            } else {
                255
            };
            if alpha == 0 {
                continue;
            }
            if alpha == 255 {
                pixels[destination..destination + 4].copy_from_slice(src);
                if cursor.format == WL_SHM_FORMAT_XRGB8888 {
                    pixels[destination + 3] = u8::MAX;
                }
                continue;
            }
            let inv = 255 - alpha;
            for channel in 0..3 {
                let dst = pixels[destination + channel] as u16;
                let src_channel = src[channel] as u16;
                pixels[destination + channel] =
                    ((src_channel * alpha + dst * inv) / 255) as u8;
            }
            pixels[destination + 3] = u8::MAX;
        }
    }
    Ok(())
}

pub fn composite_surface_full(
    frames: &[&SurfaceFrame],
    output_width: u32,
    output_height: u32,
    clear: [f32; 4],
) -> anyhow::Result<Vec<u8>> {
    let mut pixels = clear_pixels(output_width, output_height, clear)?;
    let width = output_width as usize;
    let height = output_height as usize;
    let row_bytes = width
        .checked_mul(4)
        .context("Scaled surface row size overflows")?;

    for frame in frames {
        frame.validate()?;
        let scale = frame.buffer_scale.max(1) as usize;
        let dest_w = frame.width / scale;
        let dest_h = frame.height / scale;
        if dest_w == 0 || dest_h == 0 {
            continue;
        }
        for dy in 0..dest_h {
            let out_y = frame.y + dy as i32;
            if out_y < 0 || out_y as usize >= height {
                continue;
            }
            let source_y = ((dy as u128 * frame.height as u128) / dest_h as u128) as usize;
            for dx in 0..dest_w {
                let out_x = frame.x + dx as i32;
                if out_x < 0 || out_x as usize >= width {
                    continue;
                }
                let source_x = ((dx as u128 * frame.width as u128) / dest_w as u128) as usize;
                let source = source_y * frame.stride + source_x * 4;
                let destination = out_y as usize * row_bytes + out_x as usize * 4;
                pixels[destination..destination + 4]
                    .copy_from_slice(&frame.pixels[source..source + 4]);
                if frame.format == WL_SHM_FORMAT_XRGB8888 {
                    pixels[destination + 3] = u8::MAX;
                }
            }
        }
    }

    Ok(pixels)
}

pub fn composite_cursor_into(
    pixels: &mut [u8],
    output_width: usize,
    output_height: usize,
    cursor: &CursorFrame,
    pointer_x: i32,
    pointer_y: i32,
) -> anyhow::Result<()> {
    composite_cursor_in_rect(
        pixels,
        output_width,
        output_height,
        cursor,
        pointer_x,
        pointer_y,
        DamageRect {
            x: 0,
            y: 0,
            width: output_width as i32,
            height: output_height as i32,
        },
    )
}

fn clear_pixels(width: u32, height: u32, clear: [f32; 4]) -> anyhow::Result<Vec<u8>> {
    let width = width as usize;
    let height = height as usize;
    let row_bytes = width
        .checked_mul(4)
        .context("Scaled surface row size overflows")?;
    let capacity = row_bytes
        .checked_mul(height)
        .context("Scaled surface size overflows")?;
    let clear_b = (clear[2].clamp(0.0, 1.0) * 255.0) as u8;
    let clear_g = (clear[1].clamp(0.0, 1.0) * 255.0) as u8;
    let clear_r = (clear[0].clamp(0.0, 1.0) * 255.0) as u8;
    let mut pixels = vec![0u8; capacity];
    for chunk in pixels.chunks_exact_mut(4) {
        chunk.copy_from_slice(&[clear_b, clear_g, clear_r, 0xff]);
    }
    Ok(pixels)
}

fn clip_damage_list(
    damage: &[DamageRect],
    output_width: u32,
    output_height: u32,
) -> Vec<DamageRect> {
    damage
        .iter()
        .filter_map(|rect| clip_damage(*rect, output_width, output_height))
        .collect()
}

fn clip_damage(rect: DamageRect, output_width: u32, output_height: u32) -> Option<DamageRect> {
    if rect.width <= 0 || rect.height <= 0 {
        return None;
    }
    let x0 = rect.x.max(0);
    let y0 = rect.y.max(0);
    let x1 = rect.x.saturating_add(rect.width).min(output_width as i32);
    let y1 = rect
        .y
        .saturating_add(rect.height)
        .min(output_height as i32);
    let width = x1 - x0;
    let height = y1 - y0;
    if width <= 0 || height <= 0 {
        return None;
    }
    Some(DamageRect {
        x: x0,
        y: y0,
        width,
        height,
    })
}

fn cursor_bounds(cursor: &CursorFrame, pointer_x: i32, pointer_y: i32) -> Option<DamageRect> {
    let scale = cursor.buffer_scale.max(1);
    let dest_w = div_ceil_i32(cursor.width as i32, scale);
    let dest_h = div_ceil_i32(cursor.height as i32, scale);
    if dest_w <= 0 || dest_h <= 0 {
        return None;
    }
    Some(DamageRect {
        x: pointer_x - cursor.hotspot_x,
        y: pointer_y - cursor.hotspot_y,
        width: dest_w,
        height: dest_h,
    })
}

fn rect_union(a: DamageRect, b: DamageRect) -> Option<DamageRect> {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = a.x.saturating_add(a.width).max(b.x.saturating_add(b.width));
    let y1 = a.y.saturating_add(a.height).max(b.y.saturating_add(b.height));
    let width = x1 - x0;
    let height = y1 - y0;
    if width <= 0 || height <= 0 {
        return None;
    }
    Some(DamageRect {
        x: x0,
        y: y0,
        width,
        height,
    })
}

fn div_ceil_i32(value: i32, divisor: i32) -> i32 {
    let divisor = divisor.max(1);
    (value + divisor - 1) / divisor
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SurfaceFrame;

    fn frame() -> SurfaceFrame {
        SurfaceFrame {
            owner_id: 1,
            surface_id: 2,
            pixels: vec![0; 16],
            width: 2,
            height: 2,
            stride: 8,
            format: 0,
            x: 0,
            y: 0,
            buffer_scale: 1,
            damage: Vec::new(),
            full_surface: true,
        }
    }

    #[test]
    fn partial_update_changes_only_damaged_pixel() {
        let clear = [0.0, 0.0, 0.0, 1.0];
        let frame = SurfaceFrame {
            pixels: vec![1, 2, 3, 0, 4, 5, 6, 0],
            width: 2,
            height: 1,
            stride: 8,
            format: WL_SHM_FORMAT_XRGB8888,
            x: 0,
            y: 0,
            buffer_scale: 1,
            damage: vec![DamageRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }],
            full_surface: false,
            ..frame()
        };
        let mut backing = Some(SceneBacking::new(3, 1, clear).unwrap());
        let mode = prepare_composite(
            &mut backing,
            3,
            1,
            clear,
            &[DamageRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }],
            false,
            &[&frame],
            None,
            5,
            0,
        )
        .unwrap();
        assert!(matches!(mode, CompositeMode::Partial(_)));
        let backing = backing.unwrap();
        assert_eq!(
            backing.pixels,
            vec![1, 2, 3, 255, 0, 0, 0, 255, 0, 0, 0, 255]
        );
    }

    #[test]
    fn cursor_damage_covers_motion() {
        let cursor = CursorFrame {
            owner_id: 1,
            surface_id: 3,
            pixels: vec![10; 4],
            width: 1,
            height: 1,
            stride: 4,
            format: WL_SHM_FORMAT_ARGB8888,
            hotspot_x: 0,
            hotspot_y: 0,
            buffer_scale: 1,
        };
        let rects = cursor_damage_rects(&cursor, (0, 0), (3, 0));
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].width, 4);
    }

    #[test]
    fn sequential_pointer_moves_cover_intermediate_positions() {
        let cursor = CursorFrame {
            owner_id: 1,
            surface_id: 3,
            pixels: vec![10; 4],
            width: 1,
            height: 1,
            stride: 4,
            format: WL_SHM_FORMAT_ARGB8888,
            hotspot_x: 0,
            hotspot_y: 0,
            buffer_scale: 1,
        };
        let first = cursor_damage_rects(&cursor, (0, 0), (10, 0));
        let second = cursor_damage_rects(&cursor, (10, 0), (20, 0));
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        // Second move must include the intermediate position (10, 0), not just (0,0) and (20,0).
        let mid = DamageRect {
            x: 10,
            y: 0,
            width: 1,
            height: 1,
        };
        let covers_mid = |rect: DamageRect| {
            rect.x <= mid.x
                && rect.y <= mid.y
                && rect.x + rect.width >= mid.x + mid.width
                && rect.y + rect.height >= mid.y + mid.height
        };
        assert!(covers_mid(second[0]));
    }
}
