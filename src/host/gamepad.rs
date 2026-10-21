//! Virtual Xbox 360 controller on the host through the ViGEmBus driver.

use std::sync::mpsc;

use anyhow::Result;

use crate::proto::GamepadState;

/// Plugs in a virtual controller for the session; dropping the sender unplugs it.
/// Errors (driver not installed) are returned so the viewer can tell the user.
pub fn spawn() -> Result<mpsc::Sender<GamepadState>> {
    let (tx, rx) = mpsc::channel::<GamepadState>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
    std::thread::Builder::new().name("gamepad".into()).spawn(move || run(rx, ready_tx))?;
    ready_rx.recv()??;
    Ok(tx)
}

#[cfg(windows)]
fn run(rx: mpsc::Receiver<GamepadState>, ready: mpsc::Sender<Result<()>>) {
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
fn run(_rx: mpsc::Receiver<GamepadState>, ready: mpsc::Sender<Result<()>>) {
    let _ = ready.send(Err(anyhow::anyhow!("виртуальный геймпад поддерживается только на Windows")));
}
