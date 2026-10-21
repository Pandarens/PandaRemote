//! A streaming session on the host: video goes out as datagrams from the capture thread,
//! input and statistics come in on the control stream.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::sync::oneshot;

use super::{
    audio,
    capture::{self, Control},
    gamepad, input,
};
use crate::proto::{self, ClientMsg, HostMsg, Profile};

pub async fn serve(conn: &Connection, profile: Profile, mut send: SendStream, mut recv: RecvStream) -> Result<()> {
    let (start_bitrate, min_bitrate, max_bitrate) = profile.bitrate();
    let control = Arc::new(Control::default());
    control.bitrate.store(start_bitrate, Ordering::Relaxed);

    let (start_tx, start_rx) = oneshot::channel();
    {
        let (conn, control) = (conn.clone(), control.clone());
        std::thread::Builder::new()
            .name("capture".into())
            .spawn(move || capture::run(conn, profile, control, start_tx))?;
    }
    let start = start_rx.await??;
    tracing::info!(encoder = %start.encoder, profile = profile.name(), "stream started");
    let (rumble_tx, mut rumble_rx) = tokio::sync::mpsc::unbounded_channel();
    let (pad_tx, gamepad_error) = match profile {
        Profile::Game => match gamepad::spawn(rumble_tx) {
            Ok(tx) => (Some(tx), None),
            Err(e) => {
                tracing::warn!("virtual gamepad unavailable: {e:#}");
                (None, Some(format!("{e:#}")))
            }
        },
        Profile::Desktop => (None, None),
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
    };
    proto::write_msg(&mut send, &info).await?;

    let input_tx = input::spawn();
    let mut abr = Abr { current: start_bitrate, min: min_bitrate, max: max_bitrate, clean_seconds: 0 };

    let control_loop = async {
        loop {
            match proto::read_msg::<ClientMsg>(&mut recv).await? {
                ClientMsg::Input(ev) => {
                    let _ = input_tx.send(ev);
                }
                ClientMsg::Gamepad(state) => {
                    if let Some(tx) = &pad_tx {
                        let _ = tx.send(state);
                    }
                }
                ClientMsg::RequestKeyframe => control.keyframe.store(true, Ordering::Relaxed),
                ClientMsg::Stats { frames, lost } => {
                    if let Some(bps) = abr.update(frames, lost) {
                        tracing::debug!(bps, frames, lost, "bitrate changed");
                        control.bitrate.store(bps, Ordering::Relaxed);
                    }
                }
                _ => {}
            }
        }
    };
    // Host -> viewer notifications: vibration, and whether the lock screen / UAC is showing
    // (the viewer then sends physical keys, so a password typed remotely lands in the lock screen's layout).
    let notify = async {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
        let (mut secure, mut rumble_open) = (false, true);
        loop {
            tokio::select! {
                r = rumble_rx.recv(), if rumble_open => match r {
                    Some((large, small)) => proto::write_msg(&mut send, &HostMsg::Rumble { large, small }).await?,
                    None => rumble_open = false,
                },
                _ = tick.tick() => {
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
        _ = conn.closed() => Ok(()),
    };
    control.stop.store(true, Ordering::Relaxed);
    audio_stop.store(true, Ordering::Relaxed);
    // A closed control stream is the normal way a viewer leaves.
    match result {
        Err(e) if is_closed(&e) => Ok(()),
        other => other,
    }
}

fn on_secure_desktop() -> bool {
    #[cfg(windows)]
    return super::desktop_switch::input_desktop_name().is_some_and(|n| !n.eq_ignore_ascii_case("Default"));
    #[cfg(not(windows))]
    false
}

/// Adaptive bitrate: back off on loss, probe upwards after a clean stretch.
struct Abr {
    current: u32,
    min: u32,
    max: u32,
    clean_seconds: u32,
}

impl Abr {
    fn update(&mut self, frames: u32, lost: u32) -> Option<u32> {
        let total = frames + lost;
        if total == 0 {
            return None;
        }
        let before = self.current;
        if lost as f32 / total as f32 > 0.02 {
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
        let mut abr = Abr { current: 20_000_000, min: 5_000_000, max: 50_000_000, clean_seconds: 0 };
        assert_eq!(abr.update(50, 10), Some(16_000_000));
        for _ in 0..4 {
            assert_eq!(abr.update(60, 0), None);
        }
        assert_eq!(abr.update(60, 0), Some(17_600_000));
        for _ in 0..20 {
            abr.update(1, 9);
        }
        assert_eq!(abr.current, 5_000_000);
    }
}
