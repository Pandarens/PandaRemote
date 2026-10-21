//! Video encoders: hardware H.264/HEVC/AV1 via Media Foundation (Windows), software openh264 (H.264) as fallback.

#[cfg(windows)]
mod mf;
mod sw;

use anyhow::Result;

use crate::proto::Codec;

pub struct EncodedFrame {
    /// One access unit (Annex B for H.264/HEVC, OBUs for AV1).
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// Input picture: tightly packed 32-bit pixels.
pub struct Picture<'a> {
    pub pixels: &'a [u8],
    pub width: u32,
    pub height: u32,
    /// BGRA byte order (DXGI); otherwise RGBA.
    pub bgra: bool,
}

pub trait VideoEncoder {
    fn name(&self) -> String;
    /// Encodes one picture; may return nothing while the encoder is filling its pipeline.
    fn encode(&mut self, pic: &Picture, force_keyframe: bool) -> Result<Vec<EncodedFrame>>;
    fn set_bitrate(&mut self, bps: u32);
}

/// The best codec in `accepted` this machine can encode in hardware, with its encoder; H.264 in software
/// (openh264) when nothing else works.
pub fn create(
    accepted: &[Codec],
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    allow_hw: bool,
) -> Result<(Codec, Box<dyn VideoEncoder>)> {
    #[cfg(windows)]
    if allow_hw {
        for codec in Codec::PREFERENCE.into_iter().filter(|c| accepted.contains(c)) {
            match mf::MfEncoder::new(codec, width, height, fps, bitrate) {
                Ok(e) => return Ok((codec, Box::new(e))),
                Err(e) => tracing::info!(codec = codec.name(), "hardware encoder unavailable: {e:#}"),
            }
        }
        tracing::warn!("no hardware encoder for the viewer's codecs, using openh264");
    }
    let _ = (accepted, allow_hw);
    Ok((Codec::H264, Box::new(sw::SwEncoder::new(width, height, fps, bitrate)?)))
}

/// Codecs this machine has hardware encoders for (diagnostics).
pub fn hardware_codecs() -> Vec<(Codec, String)> {
    #[cfg(windows)]
    return mf::available();
    #[cfg(not(windows))]
    Vec::new()
}

/// Converts 32-bit RGB(A) to NV12 (BT.601, limited range), matching the client's decoder conversion.
pub fn to_nv12(pic: &Picture, out: &mut Vec<u8>) {
    let (w, h) = (pic.width as usize, pic.height as usize);
    out.resize(w * h * 3 / 2, 0);
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    let (ri, bi) = if pic.bgra { (2, 0) } else { (0, 2) };
    let px = pic.pixels;
    for y in (0..h).step_by(2) {
        let row0 = &px[y * w * 4..(y + 1) * w * 4];
        let row1 = &px[(y + 1) * w * 4..(y + 2) * w * 4];
        let (y0, rest) = y_plane[y * w..].split_at_mut(w);
        let y1 = &mut rest[..w];
        let uv = &mut uv_plane[(y / 2) * w..(y / 2) * w + w];
        for x in (0..w).step_by(2) {
            let mut rs = 0i32;
            let mut gs = 0i32;
            let mut bs = 0i32;
            for (row, yrow) in [(row0, &mut *y0), (row1, &mut *y1)] {
                for dx in 0..2 {
                    let p = &row[(x + dx) * 4..(x + dx) * 4 + 4];
                    let (r, g, b) = (p[ri] as i32, p[1] as i32, p[bi] as i32);
                    yrow[x + dx] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                    rs += r;
                    gs += g;
                    bs += b;
                }
            }
            let (r, g, b) = (rs >> 2, gs >> 2, bs >> 2);
            uv[x] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
            uv[x + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_white_and_black() {
        let white = vec![255u8; 4 * 4 * 4];
        let mut out = Vec::new();
        to_nv12(&Picture { pixels: &white, width: 4, height: 4, bgra: true }, &mut out);
        assert_eq!(out.len(), 24);
        assert!(out[..16].iter().all(|&y| y == 235));
        assert!(out[16..].iter().all(|&c| c == 128));
        let black = vec![0u8; 64];
        to_nv12(&Picture { pixels: &black, width: 4, height: 4, bgra: false }, &mut out);
        assert!(out[..16].iter().all(|&y| y == 16));
    }
}
