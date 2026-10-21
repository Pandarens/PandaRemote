//! Reads local controllers (gilrs) and forwards their state in XInput layout.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gilrs::{
    Axis, Button, Gamepad, GamepadId, Gilrs,
    ff::{BaseEffect, BaseEffectType, Effect, EffectBuilder, Replay, Ticks},
};
use tokio::sync::mpsc;

use crate::proto::{ClientMsg, GamepadState, buttons};

/// Vibration requests from the host: (large motor, small motor), 0..=255.
pub type RumbleSender = std::sync::mpsc::Sender<(u8, u8)>;

/// Polls controllers until `stop` is set; sends a message whenever the state changes
/// and plays the host's vibration requests on the local controllers.
pub fn spawn(out: mpsc::UnboundedSender<ClientMsg>, stop: Arc<AtomicBool>) -> RumbleSender {
    let (rumble_tx, rumble_rx) = std::sync::mpsc::channel::<(u8, u8)>();
    let _ = std::thread::Builder::new().name("gamepad".into()).spawn(move || {
        let mut gilrs = match Gilrs::new() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("gamepads unavailable: {e}");
                return;
            }
        };
        let mut last = GamepadState::default();
        let mut effect: Option<Effect> = None;
        while !stop.load(Ordering::Relaxed) {
            // Only the newest vibration level matters.
            if let Some((large, small)) = rumble_rx.try_iter().last() {
                effect = set_rumble(&mut gilrs, effect.take(), large, small);
            }
            // The timeout bounds how late a vibration change or `stop` is noticed.
            if gilrs.next_event_blocking(Some(Duration::from_millis(10))).is_none() {
                continue;
            }
            while gilrs.next_event().is_some() {}
            let state = gilrs.gamepads().next().map(|(_, pad)| read(&pad)).unwrap_or_default();
            if state != last {
                last = state;
                if out.send(ClientMsg::Gamepad(state)).is_err() {
                    break;
                }
            }
        }
        if let Some(e) = effect {
            let _ = e.stop();
        }
    });
    rumble_tx
}

/// Replaces the running effect with a constant one at the requested motor speeds (or none for 0/0).
fn set_rumble(gilrs: &mut Gilrs, old: Option<Effect>, large: u8, small: u8) -> Option<Effect> {
    if let Some(e) = old {
        let _ = e.stop();
    }
    if large == 0 && small == 0 {
        return None;
    }
    let pads: Vec<GamepadId> = gilrs.gamepads().filter(|(_, g)| g.is_ff_supported()).map(|(id, _)| id).collect();
    if pads.is_empty() {
        return None;
    }
    // A 1 s slot repeated forever (the builder's default) = constant vibration until the next change.
    let scheduling = Replay { after: Ticks::from_ms(0), play_for: Ticks::from_ms(1000), with_delay: Ticks::from_ms(0) };
    let effect = EffectBuilder::new()
        .add_effect(BaseEffect { kind: BaseEffectType::Strong { magnitude: large as u16 * 257 }, scheduling, envelope: Default::default() })
        .add_effect(BaseEffect { kind: BaseEffectType::Weak { magnitude: small as u16 * 257 }, scheduling, envelope: Default::default() })
        .gamepads(&pads)
        .finish(gilrs)
        .map_err(|e| tracing::debug!("rumble effect: {e}"))
        .ok()?;
    effect.play().map_err(|e| tracing::debug!("rumble play: {e}")).ok()?;
    Some(effect)
}

fn read(pad: &Gamepad) -> GamepadState {
    let map = [
        (Button::South, buttons::A),
        (Button::East, buttons::B),
        (Button::West, buttons::X),
        (Button::North, buttons::Y),
        (Button::LeftTrigger, buttons::LEFT_SHOULDER),
        (Button::RightTrigger, buttons::RIGHT_SHOULDER),
        (Button::Select, buttons::BACK),
        (Button::Start, buttons::START),
        (Button::Mode, buttons::GUIDE),
        (Button::LeftThumb, buttons::LEFT_THUMB),
        (Button::RightThumb, buttons::RIGHT_THUMB),
        (Button::DPadUp, buttons::DPAD_UP),
        (Button::DPadDown, buttons::DPAD_DOWN),
        (Button::DPadLeft, buttons::DPAD_LEFT),
        (Button::DPadRight, buttons::DPAD_RIGHT),
    ];
    let mut bits = 0;
    for (button, bit) in map {
        if pad.is_pressed(button) {
            bits |= bit;
        }
    }
    let trigger = |b: Button| (pad.button_data(b).map_or(0.0, |d| d.value()).clamp(0.0, 1.0) * 255.0) as u8;
    let stick = |a: Axis| (pad.axis_data(a).map_or(0.0, |d| d.value()).clamp(-1.0, 1.0) * 32767.0) as i16;
    GamepadState {
        buttons: bits,
        left_trigger: trigger(Button::LeftTrigger2),
        right_trigger: trigger(Button::RightTrigger2),
        thumb_lx: stick(Axis::LeftStickX),
        thumb_ly: stick(Axis::LeftStickY),
        thumb_rx: stick(Axis::RightStickX),
        thumb_ry: stick(Axis::RightStickY),
    }
}
