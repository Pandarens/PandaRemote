//! Capture thread: screen -> H.264 -> FEC shards -> QUIC datagrams.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use iroh::endpoint::Connection;
use tokio::sync::oneshot;
use xcap::Monitor;

use super::encoder::{self, Picture};
use crate::{media::fec, proto::Profile};

/// Frames wider than this are downscaled by an integer factor.
const MAX_WIDTH: u32 = 2560;
/// Reed-Solomon parity added to every frame.
const PARITY_PERCENT: u32 = 20;

/// Streamed frame size (the chosen monitor, possibly downscaled).
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
}

pub struct StreamStart {
    pub geometry: Geometry,
    pub encoder: String,
    /// Labels of the host's monitors and the index of the streamed one.
    pub monitors: Vec<String>,
    pub monitor: u8,
    /// Where the streamed monitor sits on the virtual desktop (for mouse positions); `None` = primary.
    pub place: Option<Place>,
}

/// A monitor rectangle on the virtual desktop.
#[derive(Clone, Copy, Debug)]
pub struct Place {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

/// Labels for the viewer's monitor menu, and the index `requested` resolves to.
fn monitor_labels(requested: Option<usize>) -> (Vec<String>, u8, Option<Place>) {
    #[cfg(windows)]
    {
        let outputs = super::dxgi::outputs();
        let labels = outputs
            .iter()
            .enumerate()
            .map(|(i, o)| {
                let main = if o.primary() { " (основной)" } else { "" };
                format!("Монитор {} · {}×{}{main}", i + 1, o.width, o.height)
            })
            .collect();
        let chosen =
            requested.filter(|&i| i < outputs.len()).or_else(|| outputs.iter().position(|o| o.primary())).unwrap_or(0);
        let place = outputs.get(chosen).filter(|o| !o.primary()).map(|o| Place {
            left: o.left,
            top: o.top,
            width: o.width,
            height: o.height,
        });
        (labels, chosen as u8, place)
    }
    #[cfg(not(windows))]
    {
        let _ = requested;
        (vec!["Монитор 1".into()], 0, None)
    }
}

/// Knobs the network side turns while the capture thread runs.
#[derive(Default)]
pub struct Control {
    pub keyframe: AtomicBool,
    pub stop: AtomicBool,
    pub bitrate: AtomicU32,
    /// Last frame id sent; kept here so a restarted capture thread continues the sequence.
    frame_id: AtomicU32,
}

/// Why a capture thread ended without an error.
enum Exit {
    Stopped,
    /// The input desktop changed (lock screen, UAC) and this thread could not follow it.
    DesktopChanged,
}

pub fn run(
    conn: Connection,
    profile: Profile,
    monitor: Option<usize>,
    control: Arc<Control>,
    start_tx: oneshot::Sender<Result<StreamStart>>,
) {
    let mut start_tx = Some(start_tx);
    // A thread that owns windows (the encoder may create some) cannot move to another desktop,
    // so each desktop switch gets a fresh capture thread.
    loop {
        let result = std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("capture".into())
                .spawn_scoped(scope, || capture_loop(&conn, profile, monitor, &control, &mut start_tx))
                .map_err(anyhow::Error::from)
                .and_then(|t| t.join().map_err(|_| anyhow::anyhow!("capture thread panicked"))?)
        });
        match result {
            Ok(Exit::Stopped) => return,
            Ok(Exit::DesktopChanged) => {
                tracing::info!("input desktop changed, restarting capture on a fresh thread");
                control.keyframe.store(true, Ordering::Relaxed);
            }
            Err(e) => {
                tracing::warn!("capture stopped: {e:#}");
                if let Some(tx) = start_tx.take() {
                    let _ = tx.send(Err(e));
                }
                return;
            }
        }
    }
}

/// Where frames come from: DXGI duplication when available, GDI (xcap) as a slow fallback.
enum Source {
    #[cfg(windows)]
    Dxgi(super::dxgi::Dxgi),
    Gdi {
        monitor: Monitor,
        rgba: Vec<u8>,
        size: (u32, u32),
    },
}

impl Source {
    /// Opens monitor `monitor` (index into the DXGI outputs) or the primary one. The GDI fallback
    /// always captures the primary monitor.
    fn open(monitor: Option<usize>) -> Result<Self> {
        // Lock screen / UAC live on another desktop; follow it when we are allowed to (service helper).
        #[cfg(windows)]
        super::desktop_switch::follow_input_desktop();
        #[cfg(windows)]
        match super::dxgi::Dxgi::new(monitor) {
            Ok((d, _)) => return Ok(Source::Dxgi(d)),
            Err(e) => tracing::warn!("DXGI capture unavailable, falling back to GDI: {e:#}"),
        }
        let _ = monitor;
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

    fn kind(&self) -> &'static str {
        match self {
            #[cfg(windows)]
            Source::Dxgi(_) => "dxgi",
            Source::Gdi { .. } => "gdi",
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
    conn: &Connection,
    profile: Profile,
    monitor: Option<usize>,
    control: &Control,
    start_tx: &mut Option<oneshot::Sender<Result<StreamStart>>>,
) -> Result<Exit> {
    let mut source = Source::open(monitor)?;
    let (sw, sh) = source.size();
    tracing::info!(source = source.kind(), width = sw, height = sh, desktop = %current_desktop(), "capture source opened");
    let scale = sw.div_ceil(MAX_WIDTH).max(1);
    // H.264 needs even dimensions.
    let (w, h) = ((sw / scale) & !1, (sh / scale) & !1);
    let geometry = Geometry { width: w, height: h };

    let fps = profile.fps();
    let mut bitrate = control.bitrate.load(Ordering::Relaxed);
    let mut encoder = encoder::create(w, h, fps, bitrate, true)?;
    tracing::info!(encoder = %encoder.name(), "encoder ready");
    if let Some(tx) = start_tx.take() {
        let (monitors, chosen, place) = monitor_labels(monitor);
        let place = if source.kind() == "dxgi" { place } else { None };
        let _ = tx.send(Ok(StreamStart { geometry, encoder: encoder.name(), monitors, monitor: chosen, place }));
    }

    let interval = Duration::from_secs_f64(1.0 / fps as f64);
    let mut packed = vec![0u8; (w * h * 4) as usize];
    let mut have_frame = false;
    let mut dirty = false;
    let mut last_sent: Option<Instant> = None;
    let (mut grabbed, mut encoded, mut encode_time, mut stats_at) = (0u32, 0u32, Duration::ZERO, Instant::now());
    while !control.stop.load(Ordering::Relaxed) {
        if stats_at.elapsed() >= Duration::from_secs(5) {
            let secs = stats_at.elapsed().as_secs_f32();
            tracing::debug!(
                grabbed_fps = grabbed as f32 / secs,
                encoded_fps = encoded as f32 / secs,
                encode_ms = encode_time.as_secs_f32() * 1000.0 / encoded.max(1) as f32,
                "capture stats"
            );
            (grabbed, encoded, encode_time, stats_at) = (0, 0, Duration::ZERO, Instant::now());
        }
        // Wait for a screen update, but never longer than until the next frame slot is due.
        let since = last_sent.map_or(interval, |t| t.elapsed());
        let wait = interval.saturating_sub(since).max(Duration::from_millis(1));
        match source.grab(wait) {
            Ok(changed) => {
                grabbed += changed as u32;
                have_frame |= changed;
                dirty |= changed;
            }
            Err(e) => {
                // Lost duplication: lock screen, UAC prompt, display mode change. Keep retrying.
                tracing::debug!("capture lost: {e:#}");
                std::thread::sleep(Duration::from_millis(500));
                if !on_input_desktop() {
                    return Ok(Exit::DesktopChanged);
                }
                match Source::open(monitor) {
                    Ok(s) if s.size() == (sw, sh) => {
                        source = s;
                        control.keyframe.store(true, Ordering::Relaxed);
                    }
                    Ok(_) => bail!("разрешение экрана изменилось, переподключитесь"),
                    Err(_) => {}
                }
                continue;
            }
        }
        let since = last_sent.map_or(interval, |t| t.elapsed());
        let force_key = control.keyframe.load(Ordering::Relaxed);
        // A static screen is refreshed once a second so a recovering decoder gets a picture.
        let refresh = since > Duration::from_secs(1);
        if !have_frame || since < interval.mul_f32(0.9) || !(dirty || force_key || refresh) {
            continue;
        }

        let target = control.bitrate.load(Ordering::Relaxed);
        if target != bitrate {
            bitrate = target;
            encoder.set_bitrate(bitrate);
        }
        let (raw, bgra) = source.pixels();
        let pixels: &[u8] = if scale == 1 && (w, h) == (sw, sh) {
            raw
        } else {
            pack(raw, sw, scale, w, h, &mut packed);
            &packed
        };
        control.keyframe.store(false, Ordering::Relaxed);
        let encode_started = Instant::now();
        let frames = encoder.encode(&Picture { pixels, width: w, height: h, bgra }, force_key)?;
        encode_time += encode_started.elapsed();
        encoded += 1;
        dirty = false;
        // Frame slots are measured from when encoding started, so encode time doesn't eat into the next slot.
        last_sent = Some(encode_started);

        let max_dgram = conn.max_datagram_size().context("datagrams are not supported on this path")?;
        for f in frames {
            let frame_id = control.frame_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            for shard in fec::packetize(frame_id, &f.data, f.keyframe, max_dgram, PARITY_PERCENT)? {
                // A full send buffer drops the oldest datagrams; FEC and keyframe requests cover that.
                if let Err(e) = conn.send_datagram(shard) {
                    if conn.close_reason().is_some() {
                        return Ok(Exit::Stopped);
                    }
                    tracing::debug!("datagram dropped: {e}");
                }
            }
        }
    }
    Ok(Exit::Stopped)
}

fn current_desktop() -> String {
    #[cfg(windows)]
    return super::desktop_switch::thread_desktop_name().unwrap_or_else(|| "?".into());
    #[cfg(not(windows))]
    "default".into()
}

/// False when the input desktop (lock screen, UAC) differs from the one this thread is on.
fn on_input_desktop() -> bool {
    #[cfg(windows)]
    return super::desktop_switch::input_desktop_name()
        .is_none_or(|input| Some(input) == super::desktop_switch::thread_desktop_name());
    #[cfg(not(windows))]
    true
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
