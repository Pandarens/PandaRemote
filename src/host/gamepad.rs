//! Virtual Xbox 360 controller on the host through the ViGEmBus driver.

use std::sync::mpsc;

use anyhow::Result;

use crate::proto::GamepadState;

/// Rumble motor speeds (large, small) the game requested.
pub type RumbleSender = tokio::sync::mpsc::UnboundedSender<(u8, u8)>;

/// Plugs in a virtual controller for the session; dropping the sender unplugs it.
/// Errors (driver not installed) are returned so the viewer can tell the user.
pub fn spawn(rumble: RumbleSender) -> Result<mpsc::Sender<GamepadState>> {
    let (tx, rx) = mpsc::channel::<GamepadState>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
    std::thread::Builder::new().name("gamepad".into()).spawn(move || run(rx, ready_tx, rumble))?;
    ready_rx.recv()??;
    Ok(tx)
}

#[cfg(windows)]
fn run(rx: mpsc::Receiver<GamepadState>, ready: mpsc::Sender<Result<()>>, rumble: RumbleSender) {
    use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};

    let mut target = match Client::connect()
        .map_err(|e| anyhow::anyhow!("драйвер ViGEmBus не установлен ({e:?})"))
        .and_then(|client| {
            let mut t = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
            t.plugin().map_err(|e| anyhow::anyhow!("ViGEm plugin: {e:?}"))?;
            t.wait_ready().map_err(|e| anyhow::anyhow!("ViGEm wait_ready: {e:?}"))?;
            Ok(t)
        }) {
        Ok(t) => t,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // Games drive vibration through the virtual pad; the driver reports it back to us.
    // The notification thread ends by itself when the target is unplugged.
    match target.request_notification() {
        Ok(notification) => {
            notification.spawn_thread(move |_, n| {
                let _ = rumble.send((n.large_motor, n.small_motor));
            });
        }
        Err(e) => tracing::debug!("rumble notifications unavailable: {e:?}"),
    }
    let _ = ready.send(Ok(()));
    for s in rx {
        let pad = XGamepad {
            buttons: XButtons(s.buttons),
            left_trigger: s.left_trigger,
            right_trigger: s.right_trigger,
            thumb_lx: s.thumb_lx,
            thumb_ly: s.thumb_ly,
            thumb_rx: s.thumb_rx,
            thumb_ry: s.thumb_ry,
        };
        if let Err(e) = target.update(&pad) {
            tracing::debug!("gamepad update failed: {e:?}");
        }
    }
    let _ = target.unplug();
}

#[cfg(not(windows))]
fn run(_rx: mpsc::Receiver<GamepadState>, ready: mpsc::Sender<Result<()>>, _rumble: RumbleSender) {
    let _ = ready.send(Err(anyhow::anyhow!("виртуальный геймпад поддерживается только на Windows")));
}
