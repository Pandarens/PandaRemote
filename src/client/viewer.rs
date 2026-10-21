//! Viewer networking: datagrams -> FEC reassembly -> H.264 decode; input and stats go back on the control stream.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use tokio::sync::mpsc;

use super::{Session, audio, decoder};
use crate::{
    media::{
        audio as audio_pkt,
        fec::{self, Frame, Reassembler},
    },
    proto::{self, ClientMsg, GamepadState, HostMsg, InputEvent},
};

/// Decoded picture in NV12: full-resolution luma plus interleaved half-resolution chroma, tightly packed.
/// The GUI converts it to RGB on the GPU.
pub struct VideoFrame {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub uv: Vec<u8>,
}

impl VideoFrame {
    pub fn to_rgba(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.width * self.height * 4];
        super::decoder::nv12_to_rgba(&self.y, &self.uv, self.width, self.width, self.height, &mut out);
        out
    }
}

/// State shared between the network tasks and the GUI.
#[derive(Default)]
pub struct Shared {
    pub frame: Mutex<Option<VideoFrame>>,
    pub status: Mutex<String>,
    /// Problem worth showing next to the status (e.g. no virtual gamepad on the host).
    pub notice: Mutex<Option<String>>,
    pub decoder: Mutex<String>,
    /// The host is on its lock screen / a UAC prompt.
    pub secure_desktop: AtomicBool,
    /// Where host vibration requests go (set by the GUI in the game profile).
    pub rumble: Mutex<Option<super::gamepad::RumbleSender>>,
    /// View only: input and gamepad are not sent (releases still are, so nothing stays held down).
    pub view_only: AtomicBool,
    /// The host's monitors and the index of the streamed one.
    pub monitors: Mutex<(Vec<String>, u8)>,
    pub decoded: AtomicU32,
    assembled: AtomicU32,
    lost: AtomicU32,
    audio_packets: AtomicU32,
}

pub type Repaint = Arc<dyn Fn() + Send + Sync>;

/// In view-only mode: only releases (so no key, button or stick stays held) and non-input messages go out.
fn allowed_when_viewing(msg: &ClientMsg) -> bool {
    match msg {
        ClientMsg::Input(
            InputEvent::MouseButton { down: false, .. }
            | InputEvent::Key { down: false, .. }
            | InputEvent::Scancode { down: false, .. },
        ) => true,
        ClientMsg::Input(_) | ClientMsg::SecureAttention => false,
        ClientMsg::Gamepad(state) => *state == GamepadState::default(),
        _ => true,
    }
}

pub async fn run(
    session: Session,
    shared: Arc<Shared>,
    out_tx: mpsc::UnboundedSender<ClientMsg>,
    mut out_rx: mpsc::UnboundedReceiver<ClientMsg>,
    repaint: Repaint,
    play_audio: bool,
) -> Result<()> {
    let Session { conn, mut send, mut recv, .. } = session;
    let HostMsg::StreamInfo { width, height, fps, encoder, audio: has_audio, gamepad_error, monitors, monitor } =
        proto::read_msg(&mut recv).await?
    else {
        anyhow::bail!("expected StreamInfo");
    };
    *shared.notice.lock().unwrap() = gamepad_error;
    *shared.monitors.lock().unwrap() = (monitors, monitor);
    let audio_tx = (has_audio && play_audio).then(audio::spawn);
    tracing::info!(width, height, fps, %encoder, "stream info");

    // Decoding runs on its own thread; a full queue means the decoder fell behind,
    // so the frame is dropped and the decoder resyncs on the next keyframe.
    let (dec_tx, dec_rx) = mpsc::channel::<Frame>(4);
    let resync = Arc::new(AtomicBool::new(false));
    {
        let (shared, repaint, out_tx, resync) = (shared.clone(), repaint.clone(), out_tx.clone(), resync.clone());
        std::thread::Builder::new()
            .name("decoder".into())
            .spawn(move || decode_loop(dec_rx, shared, out_tx, resync, repaint, (width, height)))?;
    }

    let receive = async {
        let mut reasm = Reassembler::default();
        let mut lost_reported = 0;
        loop {
            let dgram = conn.read_datagram().await?;
            match dgram.first() {
                Some(&fec::DGRAM_VIDEO) => {}
                Some(&audio_pkt::DGRAM_AUDIO) => {
                    shared.audio_packets.fetch_add(1, Ordering::Relaxed);
                    if let Some(tx) = &audio_tx {
                        let _ = tx.send(dgram);
                    }
                    continue;
                }
                _ => continue,
            }
            if let Some(frame) = reasm.push(dgram) {
                shared.assembled.fetch_add(1, Ordering::Relaxed);
                if dec_tx.try_send(frame).is_err() {
                    resync.store(true, Ordering::Relaxed);
                }
            }
            if reasm.lost != lost_reported {
                shared.lost.fetch_add(reasm.lost - lost_reported, Ordering::Relaxed);
                lost_reported = reasm.lost;
            }
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    let host_messages = async {
        loop {
            match proto::read_msg::<HostMsg>(&mut recv).await? {
                HostMsg::Rumble { large, small } => {
                    if let Some(tx) = shared.rumble.lock().unwrap().as_ref() {
                        let _ = tx.send((large, small));
                    }
                }
                HostMsg::SecureDesktop(on) => {
                    shared.secure_desktop.store(on, Ordering::Relaxed);
                    repaint();
                }
                _ => {}
            }
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    let transmit = async {
        while let Some(msg) = out_rx.recv().await {
            if shared.view_only.load(Ordering::Relaxed) && !allowed_when_viewing(&msg) {
                continue;
            }
            proto::write_msg(&mut send, &msg).await?;
        }
        anyhow::Ok(())
    };
    let stats = async {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.tick().await;
        loop {
            tick.tick().await;
            let frames = shared.assembled.swap(0, Ordering::Relaxed);
            let lost = shared.lost.swap(0, Ordering::Relaxed);
            let decoded = shared.decoded.swap(0, Ordering::Relaxed);
            let audio = if shared.audio_packets.swap(0, Ordering::Relaxed) > 0 { " · 🔊" } else { "" };
            let _ = out_tx.send(ClientMsg::Stats { frames, lost });
            let loss = if frames + lost > 0 { lost * 100 / (frames + lost) } else { 0 };
            *shared.status.lock().unwrap() = format!(
                "{} · {decoded} к/с · потери {loss}%{audio} · кодер: {encoder} · декодер: {}",
                super::path_summary(&conn),
                shared.decoder.lock().unwrap()
            );
            repaint();
        }
    };

    tokio::select! {
        r = receive => r,
        r = transmit => r,
        r = host_messages => r,
        _ = stats => Ok(()),
        e = conn.closed() => Err(anyhow::anyhow!("соединение закрыто: {e}")),
    }
}

fn decode_loop(
    mut rx: mpsc::Receiver<Frame>,
    shared: Arc<Shared>,
    out_tx: mpsc::UnboundedSender<ClientMsg>,
    resync: Arc<AtomicBool>,
    repaint: Repaint,
    (width, height): (u32, u32),
) {
    // PANDA_DECODER=sw forces the software decoder (diagnostics).
    let allow_hw = std::env::var("PANDA_DECODER").map_or(true, |v| v != "sw");
    let mut decoder = match decoder::create(width, height, allow_hw) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("H.264 decoder init failed: {e:#}");
            return;
        }
    };
    *shared.decoder.lock().unwrap() = decoder.name();
    // Until the first keyframe (and after any loss) P-frames would decode into garbage.
    let mut awaiting_key = true;
    let mut last_request: Option<Instant> = None;
    let request_key = |last: &mut Option<Instant>| {
        if last.is_none_or(|t| t.elapsed() > Duration::from_millis(200)) {
            *last = Some(Instant::now());
            let _ = out_tx.send(ClientMsg::RequestKeyframe);
        }
    };
    while let Some(frame) = rx.blocking_recv() {
        if frame.after_gap || resync.swap(false, Ordering::Relaxed) {
            awaiting_key = true;
        }
        if awaiting_key && !frame.keyframe {
            request_key(&mut last_request);
            continue;
        }
        awaiting_key = false;
        match decoder.decode(&frame.data) {
            Ok(Some(picture)) => {
                *shared.frame.lock().unwrap() = Some(picture);
                shared.decoded.fetch_add(1, Ordering::Relaxed);
                repaint();
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!("decode error, requesting keyframe: {e:#}");
                awaiting_key = true;
                request_key(&mut last_request);
            }
        }
    }
}
