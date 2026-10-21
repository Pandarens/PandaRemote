//! Audio plumbing shared by host and client: packet format and a streaming resampler.
//!
//! Audio datagram: `type u8 | seq u32 | opus packet`. Opus runs at 48 kHz stereo, 20 ms frames.

use bytes::{BufMut, Bytes, BytesMut};

pub const DGRAM_AUDIO: u8 = 0x02;
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// 20 ms at 48 kHz.
pub const FRAME_SAMPLES: usize = 960;

pub fn packet(seq: u32, opus: &[u8]) -> Bytes {
    let mut b = BytesMut::with_capacity(5 + opus.len());
    b.put_u8(DGRAM_AUDIO);
    b.put_u32(seq);
    b.put_slice(opus);
    b.freeze()
}

pub fn parse(dgram: &Bytes) -> Option<(u32, Bytes)> {
    if dgram.len() < 6 || dgram[0] != DGRAM_AUDIO {
        return None;
    }
    let seq = u32::from_be_bytes(dgram[1..5].try_into().ok()?);
    Some((seq, dgram.slice(5..)))
}

/// Linear-interpolating resampler for interleaved stereo, keeping state across chunks.
pub struct Resampler {
    step: f64,
    pos: f64,
    carry: Vec<f32>,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Resampler { step: from as f64 / to as f64, pos: 0.0, carry: Vec::new() }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.step == 1.0 {
            out.extend_from_slice(input);
            return;
        }
        self.carry.extend_from_slice(input);
        let frames = self.carry.len() / CHANNELS;
        while self.pos + 1.0 < frames as f64 {
            let i = self.pos as usize;
            let t = (self.pos - i as f64) as f32;
            for c in 0..CHANNELS {
                let a = self.carry[i * CHANNELS + c];
                let b = self.carry[(i + 1) * CHANNELS + c];
                out.push(a + (b - a) * t);
            }
            self.pos += self.step;
        }
        let consumed = (self.pos as usize).min(frames);
        self.carry.drain(..consumed * CHANNELS);
        self.pos -= consumed as f64;
    }
}

/// Folds `channels`-channel interleaved samples into stereo.
pub fn to_stereo(input: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        1 => input.iter().for_each(|&s| out.extend_from_slice(&[s, s])),
        2 => out.extend_from_slice(input),
        n => input.chunks_exact(n).for_each(|f| out.extend_from_slice(&f[..2])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_ratio() {
        let mut r = Resampler::new(44_100, 48_000);
        let input = vec![0.5f32; 44_100 * CHANNELS];
        let mut out = Vec::new();
        r.process(&input, &mut out);
        let frames = out.len() / CHANNELS;
        assert!((47_990..=48_000).contains(&frames), "{frames}");
        assert!(out.iter().all(|&s| (s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn opus_roundtrip_keeps_signal() {
        use opus_rs::{Application, OpusDecoder, OpusEncoder};
        let mut enc = OpusEncoder::new(SAMPLE_RATE as i32, CHANNELS, Application::RestrictedLowDelay).unwrap();
        enc.bitrate_bps = 128_000;
        let mut dec = OpusDecoder::new(SAMPLE_RATE as i32, CHANNELS).unwrap();
        let (mut packet, mut pcm) = (vec![0u8; 1500], vec![0f32; FRAME_SAMPLES * CHANNELS]);
        let (mut rms_in, mut rms_out) = (0f64, 0f64);
        for frame in 0..50 {
            let input: Vec<f32> = (0..FRAME_SAMPLES)
                .flat_map(|i| {
                    let t = (frame * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
                    let v = 0.3 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
                    [v, v]
                })
                .collect();
            let n = enc.encode(&input, FRAME_SAMPLES, &mut packet).unwrap();
            assert!(n > 2 && n < 1000, "packet size {n}");
            let got = dec.decode(&packet[..n], FRAME_SAMPLES, &mut pcm).unwrap();
            assert_eq!(got, FRAME_SAMPLES);
            if frame >= 5 {
                rms_in += input.iter().map(|&x| (x * x) as f64).sum::<f64>();
                rms_out += pcm.iter().map(|&x| (x * x) as f64).sum::<f64>();
            }
        }
        let ratio = (rms_out / rms_in).sqrt();
        assert!((0.7..1.3).contains(&ratio), "level ratio {ratio}");
    }

    #[test]
    fn packet_roundtrip() {
        let p = packet(42, b"opus");
        let (seq, body) = parse(&p).unwrap();
        assert_eq!(seq, 42);
        assert_eq!(&body[..], b"opus");
    }
}
