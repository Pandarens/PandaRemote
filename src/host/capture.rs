//! Screen capture + H.264 encoding, running on a dedicated thread.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use openh264::{
    OpenH264API,
    encoder::{BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, UsageType},
    formats::{BgraSliceU8, RgbaSliceU8, YUVBuffer},
};
use tokio::sync::{mpsc, oneshot};
use xcap::Monitor;

use crate::proto::VideoFrame;

const FPS: f32 = 30.0;
const BITRATE: u32 = 8_000_000;
/// Frames wider than this are downscaled by an integer factor to keep software encoding real-time.
const MAX_WIDTH: u32 = 2560;

/// Streamed frame size; also the primary monitor's coordinate space divided by `scale`.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    pub scale: u32,
}

pub fn run(
    tx: mpsc::Sender<VideoFrame>,
    geo_tx: oneshot::Sender<Result<Geometry>>,
    keyframe: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    let mut geo_tx = Some(geo_tx);
    if let Err(e) = capture_loop(&tx, &mut geo_tx, &keyframe, &stop) {
        tracing::warn!("capture stopped: {e:#}");
        if let Some(g) = geo_tx.take() {
            let _ = g.send(Err(e));
        }
    }
}

/// Where frames come from: DXGI duplication when available, GDI (xcap) as a slow fallback.
enum Source {
    #[cfg(windows)]
    Dxgi(super::dxgi::Dxgi),
    Gdi { monitor: Monitor, rgba: Vec<u8>, size: (u32, u32) },
}

impl Source {
    fn open() -> Result<Self> {
        #[cfg(windows)]
        match super::dxgi::Dxgi::new() {
            Ok(d) => return Ok(Source::Dxgi(d)),
            Err(e) => tracing::warn!("DXGI capture unavailable, falling back to GDI: {e:#}"),
        }
        let monitors = Monitor::all().context("list monitors")?;
        let primary = monitors.iter().position(|m| m.is_primary().unwrap_or(false)).unwrap_or(0);
        let monitor = monitors.into_iter().nth(primary).context("no monitors found")?;
        let img = monitor.capture_image().context("capture screen")?;
        Ok(Source::Gdi { size: img.dimensions(), rgba: img.into_raw(), monitor })
    }

    fn size(&self) -> (u32, u32) {
        match self {
            #[cfg(windows)]
            Source::Dxgi(d) => (d.width, d.height),
            Source::Gdi { size, .. } => *size,
        }
    }

    /// Returns whether the picture changed.
    fn grab(&mut self, timeout: Duration) -> Result<bool> {
        match self {
            #[cfg(windows)]
            Source::Dxgi(d) => d.next_frame(timeout.as_millis() as u32),
            Source::Gdi { monitor, rgba, size } => {
                let img = monitor.capture_image().context("capture screen")?;
                if img.dimensions() != *size {
                    bail!("display mode changed");
                }
                *rgba = img.into_raw();
                Ok(true)
            }
        }
    }

    /// Pixels of the last frame and whether they are BGRA (otherwise RGBA).
    fn pixels(&self) -> (&[u8], bool) {
        match self {
            #[cfg(windows)]
            Source::Dxgi(d) => (&d.bgra, true),
            Source::Gdi { rgba, .. } => (rgba, false),
        }
    }
}

fn capture_loop(
    tx: &mpsc::Sender<VideoFrame>,
    geo_tx: &mut Option<oneshot::Sender<Result<Geometry>>>,
    keyframe: &AtomicBool,
    stop: &AtomicBool,
) -> Result<()> {
    let mut source = Source::open()?;
    let (sw, sh) = source.size();
    let scale = sw.div_ceil(MAX_WIDTH).max(1);
    // H.264 needs even dimensions.
    let (w, h) = ((sw / scale) & !1, (sh / scale) & !1);
    let geo = Geometry { width: w, height: h, scale };
    if let Some(g) = geo_tx.take() {
        let _ = g.send(Ok(geo));
    }

    let config = EncoderConfig::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(BITRATE))
        .max_frame_rate(FrameRate::from_hz(FPS))
        .intra_frame_period(IntraFramePeriod::from_num_frames((FPS * 5.0) as u32))
        .skip_frames(true);
    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config)?;

    let interval = Duration::from_secs_f32(1.0 / FPS);
    let mut packed = vec![0u8; (w * h * 4) as usize];
    let mut have_frame = false;
    let mut last_sent: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        let changed = match source.grab(interval) {
            Ok(c) => c,
            Err(e) => {
                // Lost duplication: lock screen, UAC prompt, display mode change. Keep retrying.
                tracing::debug!("capture lost: {e:#}");
                std::thread::sleep(Duration::from_millis(500));
                match Source::open() {
                    Ok(s) if s.size() == (sw, sh) => {
                        source = s;
                        keyframe.store(true, Ordering::Relaxed);
                    }
                    Ok(_) => bail!("разрешение экрана изменилось, переподключитесь"),
                    Err(_) => {}
                }
                continue;
            }
        };
        have_frame |= changed;
        let force_key = keyframe.swap(false, Ordering::Relaxed);
        // Static screen: refresh once a second so a late viewer/decoder recovers.
        let refresh = last_sent.is_none_or(|t| t.elapsed() > Duration::from_secs(1));
        if !have_frame || !(changed || force_key || refresh) {
            continue;
        }

        let (raw, is_bgra) = source.pixels();
        let px: &[u8] = if scale == 1 && (w, h) == (sw, sh) {
            raw
        } else {
            pack(raw, sw, scale, w, h, &mut packed);
            &packed
        };
        let dims = (w as usize, h as usize);
        let yuv = if is_bgra {
            YUVBuffer::from_rgb_source(BgraSliceU8::new(px, dims))
        } else {
            YUVBuffer::from_rgb_source(RgbaSliceU8::new(px, dims))
        };
        if force_key {
            encoder.force_intra_frame();
        }
        let bitstream = encoder.encode(&yuv)?;
        let is_key = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        let data = bitstream.to_vec();
        if !data.is_empty() {
            let frame = VideoFrame { width: w, height: h, keyframe: is_key, data };
            // Capacity 1: blocks while the network is behind, which lowers the frame rate instead of queueing.
            if tx.blocking_send(frame).is_err() {
                break;
            }
            last_sent = Some(Instant::now());
        }
        if let Some(rest) = interval.checked_sub(started.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    Ok(())
}

/// Crops to `w`×`h` and downscales by `scale` (nearest neighbour).
fn pack(src: &[u8], src_w: u32, scale: u32, w: u32, h: u32, dst: &mut [u8]) {
    let (src_w, scale, w) = (src_w as usize, scale as usize, w as usize);
    for y in 0..h as usize {
        let src_row = &src[(y * scale * src_w) * 4..];
        let dst_row = &mut dst[y * w * 4..(y + 1) * w * 4];
        if scale == 1 {
            dst_row.copy_from_slice(&src_row[..w * 4]);
        } else {
            for x in 0..w {
                let s = x * scale * 4;
                dst_row[x * 4..x * 4 + 4].copy_from_slice(&src_row[s..s + 4]);
            }
        }
    }
}
