//! A streaming session on the host: video goes out as datagrams from the capture thread,
//! input and statistics come in on the control stream.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use std::time::Duration;

use anyhow::Result;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::sync::oneshot;

use super::{
    audio,
    capture::{self, Control},
    clipboard, files, gamepad, input, net,
};
use crate::proto::{self, ClientMsg, Codec, HostMsg, Profile};

/// `view_only`: the viewer came with the view-only ticket, so its input, clipboard and files are ignored
/// (drawing and notes still show: they only point at things).
pub async fn serve(
    conn: &Connection,
    profile: Profile,
    monitor: Option<u8>,
    codecs: Vec<Codec>,
    view_only: bool,
    mut send: SendStream,
    mut recv: RecvStream,
) -> Result<()> {
    let (start_bitrate, min_bitrate, max_bitrate) = profile.bitrate();
    let control = Arc::new(Control::default());
    control.bitrate.store(start_bitrate, Ordering::Relaxed);

    let (start_tx, start_rx) = oneshot::channel();
    {
        let (conn, control) = (conn.clone(), control.clone());
        std::thread::Builder::new()
            .name("capture".into())
            .spawn(move || capture::run(conn, profile, monitor.map(usize::from), codecs, control, start_tx))?;
    }
    let start = start_rx.await??;
    tracing::info!(encoder = %start.encoder, profile = profile.name(), "stream started");
    let (rumble_tx, mut rumble_rx) = tokio::sync::mpsc::unbounded_channel();
    let (pad_tx, gamepad_error) = match profile {
        Profile::Game if !view_only => match gamepad::spawn(rumble_tx) {
            Ok(tx) => (Some(tx), None),
            Err(e) => {
                tracing::warn!("virtual gamepad unavailable: {e:#}");
                (None, Some(format!("{e:#}")))
            }
        },
        _ => (None, None),
    };
    tracing::info!("starting audio");
    let audio_stop = Arc::new(AtomicBool::new(false));
    let audio = match audio::spawn(conn.clone(), audio_stop.clone()) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("audio unavailable: {e:#}");
            false
        }
    };
    let info = HostMsg::StreamInfo {
        width: start.geometry.width,
        height: start.geometry.height,
        fps: profile.fps(),
        encoder: start.encoder,
        audio,
        gamepad_error,
        monitors: start.monitors,
        monitor: start.monitor,
        codec: start.codec,
        view_only,
        macs: net::macs(),
    };
    proto::write_msg(&mut send, &info).await?;

    let input_tx = input::spawn(start.place);
    let mut abr = Abr { current: start_bitrate, min: min_bitrate, max: max_bitrate, clean_seconds: 0, base_rtt: None };

    // Messages to the viewer from other tasks (clipboard, received files) go out through the notify loop.
    let (host_tx, mut host_rx) = tokio::sync::mpsc::unbounded_channel::<HostMsg>();
    let clipboard = clipboard::Shared::default();
    let side_stop = Arc::new(AtomicBool::new(false));
    if !view_only {
        let tx = host_tx.clone();
        clipboard.watch(side_stop.clone(), move |text| drop(tx.send(HostMsg::Clipboard(text))));
    }
    // Files arrive on their own streams, one per file.
    let receive_files = async {
        loop {
            let recv = conn.accept_uni().await?;
            let tx = host_tx.clone();
            tokio::spawn(async move {
                let (name, result) = files::receive(recv, !view_only).await;
                let _ = tx.send(HostMsg::FileSaved { name, result });
            });
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    // Created on the first drawing event or note, so most sessions don't put a window on screen.
    #[cfg(windows)]
    let mut overlay: Option<std::sync::mpsc::Sender<super::overlay::Overlay>> = None;
    let control_loop = async {
        loop {
            match proto::read_msg::<ClientMsg>(&mut recv).await? {
                ClientMsg::Input(_) | ClientMsg::Gamepad(_) | ClientMsg::SecureAttention | ClientMsg::Clipboard(_)
                    if view_only => {}
                ClientMsg::Input(ev) => {
                    let _ = input_tx.send(ev);
                }
                ClientMsg::Gamepad(state) => {
                    if let Some(tx) = &pad_tx {
                        let _ = tx.send(state);
                    }
                }
                ClientMsg::Clipboard(text) => {
                    let clipboard = clipboard.clone();
                    // The clipboard API may block briefly; keep it off the network task.
                    tokio::task::spawn_blocking(move || clipboard.set(text));
                }
                ClientMsg::Draw(ev) => {
                    #[cfg(windows)]
                    {
                        let _ =
                            overlay.get_or_insert_with(super::overlay::spawn).send(super::overlay::Overlay::Draw(ev));
                    }
                    #[cfg(not(windows))]
                    let _ = ev;
                }
                ClientMsg::Message(text) => {
                    tracing::info!(%text, "note from the viewer");
                    #[cfg(windows)]
                    {
                        let _ =
                            overlay.get_or_insert_with(super::overlay::spawn).send(super::overlay::Overlay::Note(text));
                    }
                }
                ClientMsg::RequestKeyframe => control.keyframe.store(true, Ordering::Relaxed),
                ClientMsg::SecureAttention =>
                {
                    #[cfg(windows)]
                    if let Err(e) = crate::service::request_sas() {
                        tracing::warn!("Ctrl+Alt+Del: {e:#}");
                    }
                }
                ClientMsg::Stats { frames, lost } => {
                    let rtt = conn.paths().iter().map(|p| p.rtt()).min();
                    if let Some(bps) = abr.update(frames, lost, rtt) {
                        tracing::debug!(bps, frames, lost, ?rtt, "bitrate changed");
                        control.bitrate.store(bps, Ordering::Relaxed);
                    }
                }
                _ => {}
            }
        }
    };
    // Host -> viewer notifications: vibration, and whether the lock screen / UAC is showing
    // (the viewer then sends physical keys, so a password typed remotely lands in the lock screen's layout).
    // If the PC was locked at any point of this session and the viewer unlocked it, lock it again when
    // they leave, so nobody walks up to an unlocked PC afterwards.
    let was_locked = AtomicBool::new(session_locked());
    let notify = async {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
        let (mut secure, mut rumble_open) = (false, true);
        loop {
            tokio::select! {
                r = rumble_rx.recv(), if rumble_open => match r {
                    Some((large, small)) => proto::write_msg(&mut send, &HostMsg::Rumble { large, small }).await?,
                    None => rumble_open = false,
                },
                Some(msg) = host_rx.recv() => proto::write_msg(&mut send, &msg).await?,
                _ = tick.tick() => {
                    if session_locked() {
                        was_locked.store(true, Ordering::Relaxed);
                    }
                    let now = on_secure_desktop();
                    if now != secure {
                        secure = now;
                        tracing::info!(secure, "input desktop changed");
                        proto::write_msg(&mut send, &HostMsg::SecureDesktop(secure)).await?;
                    }
                }
            }
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    let result: Result<()> = tokio::select! {
        r = control_loop => r,
        r = notify => r,
        r = receive_files => r,
        _ = conn.closed() => Ok(()),
    };
    control.stop.store(true, Ordering::Relaxed);
    side_stop.store(true, Ordering::Relaxed);
    if was_locked.load(Ordering::Relaxed) && !session_locked() {
        #[cfg(windows)]
        super::desktop_switch::lock_session();
    }
    audio_stop.store(true, Ordering::Relaxed);
    // A closed control stream is the normal way a viewer leaves.
    match result {
        Err(e) if is_closed(&e) => Ok(()),
        other => other,
    }
}

fn session_locked() -> bool {
    #[cfg(windows)]
    return super::desktop_switch::session_locked();
    #[cfg(not(windows))]
    false
}

fn on_secure_desktop() -> bool {
    #[cfg(windows)]
    return session_locked()
        || super::desktop_switch::input_desktop_name().is_some_and(|n| !n.eq_ignore_ascii_case("Default"));
    #[cfg(not(windows))]
    false
}

/// Adaptive bitrate: back off on loss or on a growing round trip (packets queuing somewhere on the path
/// long before any are dropped), probe upwards after a clean stretch.
struct Abr {
    current: u32,
    min: u32,
    max: u32,
    clean_seconds: u32,
    /// Lowest round trip seen: the path's delay without queuing.
    base_rtt: Option<Duration>,
}

/// Round-trip growth over the base that counts as queuing.
const QUEUE_DELAY: Duration = Duration::from_millis(40);

impl Abr {
    fn update(&mut self, frames: u32, lost: u32, rtt: Option<Duration>) -> Option<u32> {
        let total = frames + lost;
        if total == 0 {
            return None;
        }
        let before = self.current;
        let queuing = rtt.is_some_and(|r| self.base_rtt.is_some_and(|base| r > base + QUEUE_DELAY));
        // The base follows the lowest round trip, and drifts up 1 ms a second so a slower route after a
        // network change is not taken for queuing forever.
        if let Some(r) = rtt {
            self.base_rtt = Some(self.base_rtt.map_or(r, |base| (base + Duration::from_millis(1)).min(r)));
        }
        if queuing {
            self.current = ((self.current as f32 * 0.85) as u32).max(self.min);
            self.clean_seconds = 0;
        } else if lost as f32 / total as f32 > 0.02 {
            self.current = ((self.current as f32 * 0.8) as u32).max(self.min);
            self.clean_seconds = 0;
        } else {
            self.clean_seconds += 1;
            if self.clean_seconds >= 5 {
                self.current = ((self.current as f32 * 1.1) as u32).min(self.max);
                self.clean_seconds = 0;
            }
        }
        (self.current != before).then_some(self.current)
    }
}

fn is_closed(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}").to_lowercase();
    s.contains("closed") || s.contains("finished") || s.contains("reset") || s.contains("lost")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abr_backs_off_and_recovers() {
        let mut abr = Abr { current: 20_000_000, min: 5_000_000, max: 50_000_000, clean_seconds: 0, base_rtt: None };
        assert_eq!(abr.update(50, 10, None), Some(16_000_000));
        for _ in 0..4 {
            assert_eq!(abr.update(60, 0, None), None);
        }
        assert_eq!(abr.update(60, 0, None), Some(17_600_000));
        for _ in 0..20 {
            abr.update(1, 9, None);
        }
        assert_eq!(abr.current, 5_000_000);
    }

    #[test]
    fn abr_backs_off_when_the_round_trip_grows() {
        let ms = Duration::from_millis;
        let mut abr = Abr { current: 20_000_000, min: 5_000_000, max: 50_000_000, clean_seconds: 0, base_rtt: None };
        assert_eq!(abr.update(60, 0, Some(ms(20))), None);
        // No loss, but 80 ms more round trip: a queue is building up.
        assert_eq!(abr.update(60, 0, Some(ms(100))), Some(17_000_000));
        // Back to normal: no further cut.
        assert_eq!(abr.update(60, 0, Some(ms(22))), None);
    }
}
