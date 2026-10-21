//! Captures what the host is playing (WASAPI loopback via cpal) and sends it as Opus datagrams.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use cpal::{
    SampleFormat,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use iroh::endpoint::Connection;
use opus_rs::{Application, OpusEncoder};

use crate::media::audio::{self, CHANNELS, FRAME_SAMPLES, Resampler, SAMPLE_RATE};

const BITRATE: i32 = 128_000;

/// Starts audio streaming; returns once capture is running (or why it can't).
pub fn spawn(conn: Connection, stop: Arc<AtomicBool>) -> Result<()> {
    let (ready_tx, ready_rx) = mpsc::channel();
    std::thread::Builder::new().name("audio".into()).spawn(move || {
        if let Err(e) = run(&conn, &stop, &ready_tx) {
            tracing::warn!("audio stopped: {e:#}");
            let _ = ready_tx.send(Err(e));
        }
    })?;
    ready_rx.recv()?
}

fn run(conn: &Connection, stop: &AtomicBool, ready: &mpsc::Sender<Result<()>>) -> Result<()> {
    let device = cpal::default_host().default_output_device().context("no audio output device")?;
    let supported = device.default_output_config()?;
    let channels = supported.channels() as usize;
    let rate = supported.sample_rate();
    let config = supported.config();

    let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(64);
    let err = |e| tracing::debug!("loopback stream error: {e}");
    // An input stream on an output device is WASAPI loopback.
    let stream = match supported.sample_format() {
        SampleFormat::F32 => {
            device.build_input_stream::<f32, _, _>(config, move |d, _| drop(tx.try_send(d.to_vec())), err, None)?
        }
        SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            config,
            move |d, _| drop(tx.try_send(d.iter().map(|&s| s as f32 / 32768.0).collect())),
            err,
            None,
        )?,
        other => bail!("unsupported sample format {other:?}"),
    };
    stream.play()?;
    let _ = ready.send(Ok(()));
    tracing::info!(rate, channels, "audio loopback started");

    let mut encoder = OpusEncoder::new(SAMPLE_RATE as i32, CHANNELS, Application::RestrictedLowDelay)
        .map_err(|e| anyhow::anyhow!("opus encoder: {e}"))?;
    encoder.bitrate_bps = BITRATE;
    let mut resampler = Resampler::new(rate, SAMPLE_RATE);
    let (mut stereo, mut pending, mut out) = (Vec::new(), Vec::new(), vec![0u8; 1500]);
    let mut seq: u32 = 0;
    while !stop.load(Ordering::Relaxed) {
        let Ok(chunk) = rx.recv_timeout(Duration::from_millis(200)) else {
            continue;
        };
        stereo.clear();
        audio::to_stereo(&chunk, channels, &mut stereo);
        resampler.process(&stereo, &mut pending);
        while pending.len() >= FRAME_SAMPLES * CHANNELS {
            let n = encoder
                .encode(&pending[..FRAME_SAMPLES * CHANNELS], FRAME_SAMPLES, &mut out)
                .map_err(|e| anyhow::anyhow!("opus encode: {e}"))?;
            pending.drain(..FRAME_SAMPLES * CHANNELS);
            seq = seq.wrapping_add(1);
            if conn.send_datagram(audio::packet(seq, &out[..n])).is_err() && conn.close_reason().is_some() {
                return Ok(());
            }
        }
    }
    Ok(())
}
