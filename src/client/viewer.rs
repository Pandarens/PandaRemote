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
    proto::{self, ClientMsg, Codec, GamepadState, HostMsg, InputEvent},
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
    pub rumble: Mutex<Option<std::sync::mpsc::Sender<(u8, u8)>>>,
    /// View only: input and gamepad are not sent (releases still are, so nothing stays held down).
    pub view_only: AtomicBool,
    /// The host's monitors and the index of the streamed one.
    pub monitors: Mutex<(Vec<String>, u8)>,
    /// Codec and picture size of the stream (known once the host answered).
    pub stream: Mutex<Option<(Codec, u32, u32)>>,
    /// The host let us in with the view-only ticket.
    pub host_view_only: AtomicBool,
    /// The host's network card addresses (for Wake-on-LAN later).
    pub macs: Mutex<Vec<[u8; 6]>>,
    /// Text the host copied, not yet put on this PC's clipboard.
    pub remote_clipboard: Mutex<Option<String>>,
    /// Short notes for the toolbar (file saved, recording finished), newest last.
    pub notes: Mutex<Vec<(Instant, bool, String)>>,
    /// File being sent: name, bytes sent, size.
    pub transfer: Mutex<Option<(String, u64, u64)>>,
    /// Active recording.
    pub recorder: Mutex<Option<super::record::Recorder>>,
    pub decoded: AtomicU32,
    assembled: AtomicU32,
    lost: AtomicU32,
    audio_packets: AtomicU32,
}

pub type Repaint = Arc<dyn Fn() + Send + Sync>;

impl Shared {
    /// Adds a toolbar note (`error` shows it in red).
    pub fn note(&self, error: bool, text: impl Into<String>) {
        let mut notes = self.notes.lock().unwrap();
        notes.push((Instant::now(), error, text.into()));
        let excess = notes.len().saturating_sub(4);
        notes.drain(..excess);
    }
}

/// In view-only mode: only releases (so no key, button or stick stays held) and non-input messages go out.
fn allowed_when_viewing(msg: &ClientMsg) -> bool {
    match msg {
        ClientMsg::Input(
            InputEvent::MouseButton { down: false, .. }
            | InputEvent::Key { down: false, .. }
            | InputEvent::Scancode { down: false, .. },
        ) => true,
        ClientMsg::Input(_) | ClientMsg::SecureAttention | ClientMsg::Clipboard(_) => false,
        ClientMsg::Gamepad(state) => *state == GamepadState::default(),
        _ => true,
    }
}

pub async fn run(
    session: Session,
    shared: Arc<Shared>,
    out_tx: mpsc::UnboundedSender<ClientMsg>,
    mut out_rx: mpsc::UnboundedReceiver<ClientMsg>,
    mut files_rx: mpsc::UnboundedReceiver<std::path::PathBuf>,
    repaint: Repaint,
    play_audio: bool,
) -> Result<()> {
    let Session { conn, mut send, mut recv, .. } = session;
    let HostMsg::StreamInfo {
        width,
        height,
        fps,
        encoder,
        audio: has_audio,
        gamepad_error,
        monitors,
        monitor,
        codec,
        view_only,
        macs,
    } = proto::read_msg(&mut recv).await?
    else {
        anyhow::bail!("expected StreamInfo");
    };
    *shared.notice.lock().unwrap() = gamepad_error;
    *shared.monitors.lock().unwrap() = (monitors, monitor);
    *shared.stream.lock().unwrap() = Some((codec, width, height));
    *shared.macs.lock().unwrap() = macs;
    if view_only {
        shared.host_view_only.store(true, Ordering::Relaxed);
        shared.view_only.store(true, Ordering::Relaxed);
    }
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
            .spawn(move || decode_loop(dec_rx, shared, out_tx, resync, repaint, codec, (width, height)))?;
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
                if let Some(rec) = shared.recorder.lock().unwrap().as_mut()
                    && let Err(e) = rec.push(&frame.data, frame.keyframe)
                {
                    tracing::warn!("recording: {e:#}");
                }
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
                HostMsg::Clipboard(text) => {
                    *shared.remote_clipboard.lock().unwrap() = Some(text);
                    repaint();
                }
                HostMsg::FileSaved { name, result } => {
                    match result {
                        Ok(path) => shared.note(false, format!("«{name}» сохранён: {path}")),
                        Err(e) => shared.note(true, format!("«{name}» не передан: {e}")),
                    }
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
    // Files go one after another, each on its own stream.
    let send_files = async {
        while let Some(path) = files_rx.recv().await {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if shared.host_view_only.load(Ordering::Relaxed) {
                shared.note(true, format!("«{name}» не передан: билет для просмотра не даёт передавать файлы"));
                repaint();
                continue;
            }
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            *shared.transfer.lock().unwrap() = Some((name.clone(), 0, size));
            repaint();
            let result = super::send_file(&conn, &path, |sent| {
                if let Some(t) = shared.transfer.lock().unwrap().as_mut() {
                    t.1 = sent;
                }
                repaint();
            })
            .await;
            *shared.transfer.lock().unwrap() = None;
            if let Err(e) = result {
                shared.note(true, format!("«{name}» не передан: {e:#}"));
            }
            repaint();
        }
        // No more files can come (the sender is gone): that is not the end of the session.
        std::future::pending::<()>().await;
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
            let audio = if shared.audio_packets.swap(0, Ordering::Relaxed) > 0 { " · звук" } else { "" };
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
        _ = send_files => Ok(()),
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
    codec: Codec,
    (width, height): (u32, u32),
) {
    // PANDA_DECODER=sw forces the software decoder (diagnostics).
    let allow_hw = std::env::var("PANDA_DECODER").map_or(true, |v| v != "sw");
    let mut decoder = match decoder::create(codec, width, height, allow_hw) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(codec = codec.name(), "decoder init failed: {e:#}");
            // The status line is rebuilt every second from this field, so the reason stays visible.
            *shared.decoder.lock().unwrap() = format!("{} не запустился: {e:#}", codec.name());
            return;
        }
    };
    *shared.decoder.lock().unwrap() = decoder.name();
    // A hardware decoder that takes input but never returns a picture (seen on phones) is replaced by
    // the software one, so the viewer gets a picture either way.
    let (mut fed_blind, mut seen_picture, mut on_hw) = (0u32, false, allow_hw && codec == Codec::H264);
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
            // An empty picture (a decoder quirk) counts as none.
            Ok(Some(picture)) if picture.width > 0 && picture.height > 0 => {
                seen_picture = true;
                *shared.frame.lock().unwrap() = Some(picture);
                shared.decoded.fetch_add(1, Ordering::Relaxed);
                repaint();
            }
            Ok(_) => fed_blind += 1,
            Err(e) => {
                fed_blind += 1;
                tracing::debug!("decode error, requesting keyframe: {e:#}");
                awaiting_key = true;
                request_key(&mut last_request);
            }
        }
        if on_hw && !seen_picture && fed_blind >= 60 {
            on_hw = false;
            tracing::warn!(decoder = %decoder.name(), "hardware decoder gave no picture, switching to software");
            match decoder::create(codec, width, height, false) {
                Ok(d) => {
                    decoder = d;
                    *shared.decoder.lock().unwrap() = format!("{} (аппаратный не выдал кадров)", decoder.name());
                    awaiting_key = true;
                    request_key(&mut last_request);
                }
                Err(e) => tracing::error!("software decoder failed too: {e:#}"),
            }
        }
    }
}
