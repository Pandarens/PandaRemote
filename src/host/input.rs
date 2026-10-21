//! Injects the viewer's mouse/keyboard events on the host (enigo) from a dedicated thread.

use std::sync::mpsc;

use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};

use super::capture::Geometry;
use crate::proto::{InputEvent, KeyCode, Modifiers, MouseButton};

pub fn spawn(geo: Geometry) -> mpsc::Sender<InputEvent> {
    let (tx, rx) = mpsc::channel::<InputEvent>();
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || {
            let mut enigo = match Enigo::new(&Settings::default()) {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("input injection unavailable: {e}");
                    return;
                }
            };
            for ev in rx {
                if let Err(e) = apply(&mut enigo, geo, ev) {
                    tracing::debug!("input event failed: {e}");
                }
            }
        })
        .expect("spawn input thread");
    tx
}

fn apply(enigo: &mut Enigo, geo: Geometry, ev: InputEvent) -> enigo::InputResult<()> {
    match ev {
        InputEvent::MouseMove { x, y } => {
            // Normalized frame coordinates -> primary monitor pixels (enigo's Abs space).
            let sx = (x as u64 * (geo.width * geo.scale - 1) as u64 / 65535) as i32;
            let sy = (y as u64 * (geo.height * geo.scale - 1) as u64 / 65535) as i32;
            enigo.move_mouse(sx, sy, Coordinate::Abs)
        }
        InputEvent::MouseButton { button, down } => {
            let b = match button {
                MouseButton::Left => Button::Left,
                MouseButton::Right => Button::Right,
                MouseButton::Middle => Button::Middle,
            };
            enigo.button(b, if down { Direction::Press } else { Direction::Release })
        }
        InputEvent::Wheel { dx, dy } => {
            if dy != 0 {
                enigo.scroll(dy, Axis::Vertical)?;
            }
            if dx != 0 {
                enigo.scroll(dx, Axis::Horizontal)?;
            }
            Ok(())
        }
        InputEvent::Text(text) => enigo.text(&text),
        InputEvent::Key { key, down, mods } => {
            let key = map_key(key);
            if down {
                set_mods(enigo, mods, Direction::Press)?;
                enigo.key(key, Direction::Press)
            } else {
                enigo.key(key, Direction::Release)?;
                set_mods(enigo, mods, Direction::Release)
            }
        }
    }
}

fn set_mods(enigo: &mut Enigo, mods: Modifiers, dir: Direction) -> enigo::InputResult<()> {
    for (on, key) in [
        (mods.ctrl, Key::Control),
        (mods.shift, Key::Shift),
        (mods.alt, Key::Alt),
        (mods.meta, Key::Meta),
    ] {
        if on {
            enigo.key(key, dir)?;
        }
    }
    Ok(())
}

fn map_key(key: KeyCode) -> Key {
    match key {
        KeyCode::Enter => Key::Return,
        KeyCode::Tab => Key::Tab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Escape => Key::Escape,
        KeyCode::Delete => Key::Delete,
        KeyCode::Insert => Key::Insert,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Up => Key::UpArrow,
        KeyCode::Down => Key::DownArrow,
        KeyCode::Left => Key::LeftArrow,
        KeyCode::Right => Key::RightArrow,
        KeyCode::Space => Key::Space,
        KeyCode::F(n) => match n {
            1 => Key::F1,
            2 => Key::F2,
            3 => Key::F3,
            4 => Key::F4,
            5 => Key::F5,
            6 => Key::F6,
            7 => Key::F7,
            8 => Key::F8,
            9 => Key::F9,
            10 => Key::F10,
            11 => Key::F11,
            _ => Key::F12,
        },
        KeyCode::Char(c) => Key::Unicode(c.to_ascii_lowercase()),
    }
}
