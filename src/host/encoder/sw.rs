//! Software H.264 (openh264).

use anyhow::Result;
use openh264::{
    OpenH264API,
    encoder::{BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, UsageType},
    formats::{BgraSliceU8, RgbaSliceU8, YUVBuffer},
};

use super::{EncodedFrame, Picture, VideoEncoder};

pub struct SwEncoder {
    encoder: Encoder,
    fps: u32,
    pending_bitrate: Option<u32>,
}

fn config(fps: u32, bitrate: u32) -> EncoderConfig {
    EncoderConfig::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(bitrate))
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        .intra_frame_period(IntraFramePeriod::from_num_frames(fps * 5))
        .skip_frames(true)
}

impl SwEncoder {
    pub fn new(_width: u32, _height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config(fps, bitrate))?;
        Ok(SwEncoder { encoder, fps, pending_bitrate: None })
    }
}

impl VideoEncoder for SwEncoder {
    fn name(&self) -> String {
        "openh264 (программный)".into()
    }

    fn encode(&mut self, pic: &Picture, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        // openh264 has no runtime bitrate API in this binding: rebuild the encoder (next frame is IDR anyway).
        if let Some(bps) = self.pending_bitrate.take() {
            self.encoder = Encoder::with_api_config(OpenH264API::from_source(), config(self.fps, bps))?;
        }
        let dims = (pic.width as usize, pic.height as usize);
        let yuv = if pic.bgra {
            YUVBuffer::from_rgb_source(BgraSliceU8::new(pic.pixels, dims))
        } else {
            YUVBuffer::from_rgb_source(RgbaSliceU8::new(pic.pixels, dims))
        };
        if force_keyframe {
            self.encoder.force_intra_frame();
        }
        let bitstream = self.encoder.encode(&yuv)?;
        let keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        let data = bitstream.to_vec();
        Ok(if data.is_empty() { Vec::new() } else { vec![EncodedFrame { data, keyframe }] })
    }

    fn set_bitrate(&mut self, bps: u32) {
        self.pending_bitrate = Some(bps);
    }
}
