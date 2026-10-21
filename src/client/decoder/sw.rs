//! Software H.264 decoding (openh264).

use anyhow::Result;
use openh264::{decoder::Decoder, formats::YUVSource};

use super::VideoDecoder;
use crate::client::viewer::VideoFrame;

pub struct SwDecoder {
    decoder: Decoder,
}

impl SwDecoder {
    pub fn new() -> Result<Self> {
        Ok(SwDecoder { decoder: Decoder::new()? })
    }
}

impl VideoDecoder for SwDecoder {
    fn name(&self) -> String {
        "openh264 (программный)".into()
    }

    fn decode(&mut self, au: &[u8]) -> Result<Option<VideoFrame>> {
        let Some(yuv) = self.decoder.decode(au)? else { return Ok(None) };
        let (width, height) = yuv.dimensions();
        let (ys, us, vs) = yuv.strides();
        let mut y = Vec::with_capacity(width * height);
        for row in 0..height {
            y.extend_from_slice(&yuv.y()[row * ys..row * ys + width]);
        }
        // I420 -> NV12: interleave the chroma planes.
        let mut uv = Vec::with_capacity(width * height / 2);
        let (u_plane, v_plane) = (yuv.u(), yuv.v());
        for row in 0..height / 2 {
            let (u, v) = (&u_plane[row * us..row * us + width / 2], &v_plane[row * vs..row * vs + width / 2]);
            for (a, b) in u.iter().zip(v) {
                uv.push(*a);
                uv.push(*b);
            }
        }
        Ok(Some(VideoFrame { width, height, y, uv }))
    }
}
