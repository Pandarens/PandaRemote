//! Reads local controllers (gilrs) and forwards their state in XInput layout.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gilrs::{Axis, Button, Gamepad, Gilrs};
use tokio::sync::mpsc;

use crate::proto::{ClientMsg, GamepadState, buttons};

/// Polls controllers until `stop` is set; sends a message whenever the state changes.
pub fn spawn(out: mpsc::UnboundedSender<ClientMsg>, stop: Arc<AtomicBool>) {
    let _ = std::thread::Builder::new().name("gamepad".into()).spawn(move || {
        let mut gilrs = match Gilrs::new() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("gamepads unavailable: {e}");
                return;
            }
        };
        let mut last = GamepadState::default();
        while !stop.load(Ordering::Relaxed) {
            // Wakes on any controller event; the timeout only bounds how long `stop` goes unnoticed.
            if gilrs.next_event_blocking(Some(Duration::from_millis(100))).is_none() {
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
    });
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
