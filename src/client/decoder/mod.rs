//! H.264 decoders: hardware (DXVA) via Media Foundation on Windows, openh264 as fallback.

#[cfg(windows)]
mod mf;
mod sw;

use anyhow::Result;

use super::viewer::VideoFrame;

pub trait VideoDecoder {
    fn name(&self) -> String;
    /// Decodes one access unit; returns a picture when the decoder emits one.
    fn decode(&mut self, au: &[u8]) -> Result<Option<VideoFrame>>;
}

/// Hardware decoder when available, otherwise openh264. `width`/`height` are the stream's picture size.
pub fn create(width: u32, height: u32, allow_hw: bool) -> Result<Box<dyn VideoDecoder>> {
    #[cfg(windows)]
    if allow_hw {
        match mf::MfDecoder::new(width, height) {
            Ok(d) => return Ok(Box::new(d)),
            Err(e) => tracing::warn!("hardware decoder unavailable, using openh264: {e:#}"),
        }
    }
    let _ = (width, height, allow_hw);
    Ok(Box::new(sw::SwDecoder::new()?))
}

/// NV12 (BT.601 limited range, as produced by our encoders) to RGBA.
/// `pitch` is the byte stride of both planes; `uv` starts at the chroma plane.
pub fn nv12_to_rgba(y_plane: &[u8], uv_plane: &[u8], pitch: usize, width: usize, height: usize, out: &mut [u8]) {
    for row in 0..height {
        let y_row = &y_plane[row * pitch..row * pitch + width];
        let uv_row = &uv_plane[(row / 2) * pitch..(row / 2) * pitch + width];
        let dst = &mut out[row * width * 4..(row + 1) * width * 4];
        for x in 0..width {
            let c = 298 * (y_row[x] as i32 - 16);
            let d = uv_row[x & !1] as i32 - 128;
            let e = uv_row[x | 1] as i32 - 128;
            let px = &mut dst[x * 4..x * 4 + 4];
            px[0] = ((c + 409 * e + 128) >> 8).clamp(0, 255) as u8;
            px[1] = ((c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u8;
            px[2] = ((c + 516 * d + 128) >> 8).clamp(0, 255) as u8;
            px[3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::encoder::{Picture, to_nv12};

    #[test]
    fn nv12_roundtrip_close_to_source() {
        let (w, h) = (8usize, 4usize);
        // Colour constant within each 2x2 block, so chroma subsampling loses nothing.
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let block = (i / w / 2) * (w / 2) + (i % w) / 2;
                [(block * 30) as u8, 120, 200 - (block * 20) as u8, 255]
            })
            .collect();
        let mut nv12 = Vec::new();
        to_nv12(&Picture { pixels: &rgba, width: w as u32, height: h as u32, bgra: false }, &mut nv12);
        let mut back = vec![0u8; w * h * 4];
        nv12_to_rgba(&nv12[..w * h], &nv12[w * h..], w, w, h, &mut back);
        for (a, b) in rgba.chunks(4).zip(back.chunks(4)) {
            for c in 0..3 {
                assert!((a[c] as i32 - b[c] as i32).abs() <= 3, "{a:?} vs {b:?}");
            }
        }
    }
}
