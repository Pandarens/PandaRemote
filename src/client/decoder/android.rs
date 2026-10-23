//! Hardware decoding on Android: MediaCodec renders into an ImageReader in YUV_420_888, the one layout
//! every device supports for reading frames back (raw output buffers use vendor-specific layouts).
//! The three planes are packed into NV12 for the rest of the viewer.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ndk::media::{
    image_reader::{AcquireResult, Image, ImageFormat, ImageReader},
    media_codec::{DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection},
    media_format::MediaFormat,
};

use super::VideoDecoder;
use crate::{client::viewer::VideoFrame, proto::Codec};

fn mime(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "video/avc",
        Codec::Hevc => "video/hevc",
        Codec::Av1 => "video/av01",
    }
}

/// Whether this phone has a decoder for `codec` (for offering HEVC once the MediaCodec path is confirmed).
#[allow(dead_code)]
pub fn has_decoder(codec: Codec) -> bool {
    MediaCodec::from_decoder_type(mime(codec)).is_some()
}

pub struct MediaDecoder {
    codec: MediaCodec,
    reader: ImageReader,
    name: String,
    started: Instant,
}

impl MediaDecoder {
    pub fn new(codec: Codec, width: u32, height: u32) -> Result<Self> {
        let mc = MediaCodec::from_decoder_type(mime(codec)).with_context(|| format!("no {} decoder", codec.name()))?;
        let reader =
            ImageReader::new(width as i32, height as i32, ImageFormat::YUV_420_888, 4).context("ImageReader")?;
        let window = reader.window().context("ImageReader window")?;
        let mut format = MediaFormat::new();
        format.set_str("mime", mime(codec));
        format.set_i32("width", width as i32);
        format.set_i32("height", height as i32);
        // Android 11+: decode each frame as soon as it arrives instead of buffering a few.
        format.set_i32("low-latency", 1);
        // Real-time priority.
        format.set_i32("priority", 0);
        mc.configure(&format, Some(&window), MediaCodecDirection::Decoder).context("configure")?;
        mc.start().context("start")?;
        // `AMediaCodec_getName` needs Android 9; the app supports 8, so name it by type.
        let name = format!("MediaCodec {}", codec.name());
        log::info!("MediaCodec decoder {name} for {} {width}x{height}", codec.name());
        Ok(MediaDecoder { codec: mc, reader, name, started: Instant::now() })
    }

    /// Feeds one access unit; false when the codec had no free input buffer in time.
    fn feed(&self, au: &[u8]) -> Result<bool> {
        match self.codec.dequeue_input_buffer(Duration::from_millis(20))? {
            DequeuedInputBufferResult::Buffer(mut buf) => {
                let dst = buf.buffer_mut();
                if dst.len() < au.len() {
                    bail!("access unit of {} bytes does not fit the {}-byte input buffer", au.len(), dst.len());
                }
                // SAFETY: `dst` has room for `au`; MaybeUninit<u8> has the layout of u8.
                unsafe { std::ptr::copy_nonoverlapping(au.as_ptr(), dst.as_mut_ptr().cast::<u8>(), au.len()) };
                let pts = self.started.elapsed().as_micros() as u64;
                self.codec.queue_input_buffer(buf, 0, au.len(), pts, 0)?;
                Ok(true)
            }
            DequeuedInputBufferResult::TryAgainLater => Ok(false),
        }
    }

    /// Renders decoded outputs into the ImageReader and returns the newest picture.
    fn drain(&self) -> Result<Option<VideoFrame>> {
        let mut rendered = false;
        loop {
            let wait = if rendered { Duration::ZERO } else { Duration::from_millis(8) };
            match self.codec.dequeue_output_buffer(wait)? {
                DequeuedOutputBufferInfoResult::Buffer(out) => {
                    self.codec.release_output_buffer(out, true)?;
                    rendered = true;
                }
                DequeuedOutputBufferInfoResult::TryAgainLater => break,
                DequeuedOutputBufferInfoResult::OutputFormatChanged
                | DequeuedOutputBufferInfoResult::OutputBuffersChanged => {}
            }
        }
        if !rendered {
            return Ok(None);
        }
        // The rendered frame reaches the reader's queue asynchronously; it is normally there within a few ms.
        let deadline = Instant::now() + Duration::from_millis(30);
        loop {
            match self.reader.acquire_latest_image()? {
                AcquireResult::Image(image) => return Ok(Some(to_nv12(&image)?)),
                _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(1)),
                _ => return Ok(None),
            }
        }
    }
}

impl VideoDecoder for MediaDecoder {
    fn name(&self) -> String {
        format!("{} (аппаратный)", self.name)
    }

    fn decode(&mut self, au: &[u8]) -> Result<Option<VideoFrame>> {
        if !self.feed(au)? {
            // Input is full: let the decoder catch up first, then try once more.
            let frame = self.drain()?;
            self.feed(au)?;
            return Ok(frame);
        }
        self.drain()
    }
}

impl Drop for MediaDecoder {
    fn drop(&mut self) {
        let _ = self.codec.stop();
    }
}

/// YUV_420_888 (any plane strides) -> tightly packed NV12 of the visible area.
fn to_nv12(image: &Image) -> Result<VideoFrame> {
    // Some decoders leave the crop rectangle empty: then the whole image is the picture.
    let crop = image.crop_rect()?;
    let (iw, ih) = (image.width()? as usize, image.height()? as usize);
    let (mut x0, mut y0, mut w, mut h) =
        (crop.left as usize, crop.top as usize, (crop.right - crop.left) as usize, (crop.bottom - crop.top) as usize);
    if w == 0 || h == 0 || x0 + w > iw || y0 + h > ih {
        (x0, y0, w, h) = (0, 0, iw, ih);
    }
    let (w, h) = (w & !1, h & !1);
    anyhow::ensure!(w > 0 && h > 0, "empty image");
    let (yp, ys) = (image.plane_data(0)?, image.plane_row_stride(0)? as usize);
    let (up, us, upx) =
        (image.plane_data(1)?, image.plane_row_stride(1)? as usize, image.plane_pixel_stride(1)? as usize);
    let (vp, vs, vpx) =
        (image.plane_data(2)?, image.plane_row_stride(2)? as usize, image.plane_pixel_stride(2)? as usize);
    let mut y = vec![0u8; w * h];
    for row in 0..h {
        let src = (y0 + row) * ys + x0;
        y[row * w..(row + 1) * w].copy_from_slice(&yp[src..src + w]);
    }
    let mut uv = vec![0u8; w * h / 2];
    for row in 0..h / 2 {
        let (cu, cv) = ((y0 / 2 + row) * us, (y0 / 2 + row) * vs);
        let dst = &mut uv[row * w..(row + 1) * w];
        for col in 0..w / 2 {
            let cx = x0 / 2 + col;
            dst[col * 2] = up[cu + cx * upx];
            dst[col * 2 + 1] = vp[cv + cx * vpx];
        }
    }
    Ok(VideoFrame { width: w, height: h, y, uv })
}
