//! Shared screencast output size caps.
//!
//! Portal `Stream.parameters.size`, DMA-BUF pool allocation, and PipeWire enum
//! formats must agree on the DMA path size. Keep these helpers as the single
//! source of truth so Mutter ScreenCast and the compositor cannot drift.

/// Longest edge for DMA-BUF PipeWire buffers (native 5K fits).
pub const SCREENCAST_DMA_MAX_EDGE: u32 = 7680;

/// Longest edge for MemFd frames — full-res CPU readback freezes the session.
pub const SCREENCAST_MEMFD_MAX_EDGE: u32 = 1920;

fn fit_edge(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    let width = width.max(1);
    let height = height.max(1);
    let longest = width.max(height);
    if longest <= max_edge {
        return (width, height);
    }
    let w = (u64::from(width) * u64::from(max_edge) / u64::from(longest)).max(1) as u32;
    let h = (u64::from(height) * u64::from(max_edge) / u64::from(longest)).max(1) as u32;
    (w, h)
}

/// Shrink for DMA-BUF / portal size (high cap — typically native).
pub fn fit_screencast_dma_size(width: u32, height: u32) -> (u32, u32) {
    fit_edge(width, height, SCREENCAST_DMA_MAX_EDGE)
}

/// Shrink for MemFd RGBA frames (low cap — avoids 5K CPU readback).
pub fn fit_screencast_memfd_size(width: u32, height: u32) -> (u32, u32) {
    fit_edge(width, height, SCREENCAST_MEMFD_MAX_EDGE)
}

/// Signed variant for Mutter ScreenCast `parameters.size`.
pub fn fit_screencast_portal_size(width: i32, height: i32) -> (i32, i32) {
    let (w, h) = fit_screencast_dma_size(width.max(1) as u32, height.max(1) as u32);
    (w as i32, h as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_size_matches_dma_size() {
        let cases = [(3840i32, 2160), (5120, 2880), (8000, 4500), (640, 480)];
        for (w, h) in cases {
            let (pw, ph) = fit_screencast_portal_size(w, h);
            let (dw, dh) = fit_screencast_dma_size(w as u32, h as u32);
            assert_eq!((pw as u32, ph as u32), (dw, dh), "{w}x{h}");
        }
    }

    #[test]
    fn memfd_cap_is_stricter_than_dma() {
        let (dw, dh) = fit_screencast_dma_size(3840, 2160);
        let (mw, mh) = fit_screencast_memfd_size(3840, 2160);
        assert_eq!((dw, dh), (3840, 2160));
        assert!(mw.max(mh) <= SCREENCAST_MEMFD_MAX_EDGE);
        assert!(mw < dw || mh < dh);
    }
}
