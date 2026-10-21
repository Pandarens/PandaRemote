//! Plays the host's audio: Opus decode, small jitter buffer, loss concealment, cpal output.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, mpsc},
};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use cpal::{
    SampleFormat,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use opus_rs::OpusDecoder;

use crate::media::audio::{self, CHANNELS, FRAME_SAMPLES, Resampler, SAMPLE_RATE};

/// Playback starts once this much audio is buffered, and the buffer is trimmed above `MAX_BUFFER_MS`.
const PREBUFFER_MS: usize = 40;
const MAX_BUFFER_MS: usize = 150;
/// Gaps longer than this are not concealed (stream paused, not lost).
const MAX_CONCEAL_FRAMES: u32 = 5;

/// Starts the player; feed it audio datagrams. Dropping the sender stops playback.
pub fn spawn() -> mpsc::Sender<Bytes> {
    let (tx, rx) = mpsc::channel::<Bytes>();
    let _ = std::thread::Builder::new().name("audio-out".into()).spawn(move || {
        if let Err(e) = run(rx) {
            tracing::warn!("audio playback unavailable: {e:#}");
        }
    });
    tx
}

struct Buffer {
    samples: VecDeque<f32>,
    playing: bool,
}

fn run(rx: mpsc::Receiver<Bytes>) -> Result<()> {
    let device = cpal::default_host().default_output_device().context("no audio output device")?;
    let supported = device.default_output_config()?;
    let out_channels = supported.channels() as usize;
    let rate = supported.sample_rate();
    let per_ms = rate as usize * CHANNELS / 1000;

    let buffer = Arc::new(Mutex::new(Buffer { samples: VecDeque::new(), playing: false }));
    let fill = {
        let buffer = buffer.clone();
        move |out_frames: usize, write: &mut dyn FnMut(usize, usize, f32)| {
            let mut b = buffer.lock().unwrap();
            if !b.playing && b.samples.len() >= PREBUFFER_MS * per_ms {
                b.playing = true;
            }
            for f in 0..out_frames {
                let (l, r) = if b.playing && b.samples.len() >= CHANNELS {
                    (b.samples.pop_front().unwrap(), b.samples.pop_front().unwrap())
                } else {
                    // Underrun: rebuffer instead of crackling.
                    b.playing = false;
                    (0.0, 0.0)
                };
                for c in 0..out_channels {
                    write(
                        f,
                        c,
                        match c {
                            0 => l,
                            1 => r,
                            _ => (l + r) * 0.5,
                        },
                    );
                }
            }
        }
    };
    let err = |e| tracing::debug!("audio output error: {e}");
    let config = supported.config();
    let stream = match supported.sample_format() {
        SampleFormat::F32 => device.build_output_stream::<f32, _, _>(
            config,
            move |data, _| fill(data.len() / out_channels, &mut |f, c, s| data[f * out_channels + c] = s),
            err,
            None,
        )?,
        SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
            config,
            move |data, _| {
                fill(data.len() / out_channels, &mut |f, c, s| {
                    data[f * out_channels + c] = (s.clamp(-1.0, 1.0) * 32767.0) as i16
                })
            },
            err,
            None,
        )?,
        other => bail!("unsupported sample format {other:?}"),
    };
    stream.play()?;

    let mut decoder =
        OpusDecoder::new(SAMPLE_RATE as i32, CHANNELS).map_err(|e| anyhow::anyhow!("opus decoder: {e}"))?;
    let mut resampler = Resampler::new(SAMPLE_RATE, rate);
    let mut pcm = vec![0f32; FRAME_SAMPLES * CHANNELS];
    let mut resampled = Vec::new();
    let mut last_seq: Option<u32> = None;
    for dgram in rx {
        let Some((seq, opus)) = audio::parse(&dgram) else {
            continue;
        };
        if last_seq.is_some_and(|l| seq.wrapping_sub(l) > u32::MAX / 2 || seq == l) {
            continue; // late or duplicate
        }
        let missing = last_seq.map_or(0, |l| seq.wrapping_sub(l) - 1);
        last_seq = Some(seq);
        resampled.clear();
        if (1..=MAX_CONCEAL_FRAMES).contains(&missing) {
            for _ in 0..missing {
                // An empty packet asks the decoder for packet-loss concealment.
                if decoder.decode(&[], FRAME_SAMPLES, &mut pcm).is_ok() {
                    resampler.process(&pcm, &mut resampled);
                }
            }
        }
        match decoder.decode(&opus, FRAME_SAMPLES, &mut pcm) {
            Ok(n) => resampler.process(&pcm[..n * CHANNELS], &mut resampled),
            Err(e) => tracing::debug!("opus decode: {e}"),
        }
        let mut b = buffer.lock().unwrap();
        b.samples.extend(resampled.iter().copied());
        let max = MAX_BUFFER_MS * per_ms;
        if b.samples.len() > max {
            // Drift or a burst after a stall: drop the oldest audio to keep latency bounded.
            let excess = (b.samples.len() - PREBUFFER_MS * per_ms) & !1;
            b.samples.drain(..excess);
        }
    }
    drop(stream);
    Ok(())
}
