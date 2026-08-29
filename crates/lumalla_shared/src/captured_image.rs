/// Pixel buffer returned from a compositor region capture.
#[derive(Debug, Clone)]
pub struct CapturedImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// 8-bit RGBA, row-major, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}
