//! Injects the viewer's input with `SendInput` from a dedicated thread.
//! Scancodes and raw relative mouse motion are what games reading DirectInput/Raw Input expect.

use std::{collections::HashSet, sync::mpsc};

use crate::proto::{InputEvent, KeyCode, Modifiers, MouseButton};

/// `place`: the streamed monitor when it isn't the primary one (mouse positions are relative to it).
pub fn spawn(place: Option<super::capture::Place>) -> mpsc::Sender<InputEvent> {
    let (tx, rx) = mpsc::channel::<InputEvent>();
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || {
            let mut injector = Injector { place, ..Default::default() };
            for ev in rx {
                injector.apply(ev);
            }
            // Viewer gone: never leave a key or button stuck down on the host.
            injector.release_all();
        })
        .expect("spawn input thread");
    tx
}

#[derive(Default)]
struct Injector {
    keys: HashSet<(u16, bool)>,
    buttons: HashSet<u8>,
    place: Option<super::capture::Place>,
    #[cfg(windows)]
    desktop: Desktop,
}

/// Input must be injected from a thread on the desktop that currently has input (lock screen, UAC).
#[cfg(windows)]
struct Desktop(super::desktop_switch::DesktopFollower);

#[cfg(windows)]
impl Default for Desktop {
    fn default() -> Self {
        Desktop(super::desktop_switch::DesktopFollower::new())
    }
}

#[cfg(not(windows))]
impl Injector {
    fn apply(&mut self, _ev: InputEvent) {}
    fn release_all(&mut self) {}
}

#[cfg(windows)]
mod win {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;

    pub fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
            },
        };
        send(&[input]);
    }

    pub fn key(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
            },
        }
    }

    pub fn send(inputs: &[INPUT]) {
        unsafe {
            SendInput(inputs, size_of::<INPUT>() as i32);
        }
    }

    pub fn scan_of(vk: u16) -> u16 {
        unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as u16 }
    }

    pub fn vk_of_char(c: char) -> Option<u16> {
        let mut buf = [0u16; 2];
        let units = c.encode_utf16(&mut buf);
        if units.len() != 1 {
            return None;
        }
        let r = unsafe { VkKeyScanW(units[0]) };
        (r != -1).then_some((r & 0xff) as u16)
    }
}

#[cfg(windows)]
impl Injector {
    fn apply(&mut self, ev: InputEvent) {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;
        self.desktop.0.check();
        match ev {
            InputEvent::MouseMove { x, y } => match self.place {
                // 0..65535 over the primary monitor is exactly MOUSEEVENTF_ABSOLUTE's coordinate space.
                None => win::mouse(x as i32, y as i32, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE),
                // Another monitor: map onto it, in 0..65535 over the whole virtual desktop.
                Some(p) => {
                    let (vx, vy) = virtual_desktop_point(p, x, y);
                    win::mouse(vx, vy, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK);
                }
            },
            InputEvent::MouseRelative { dx, dy } => {
                win::mouse(dx, dy, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_MOVE_NOCOALESCE);
            }
            InputEvent::MouseButton { button, down } => {
                let (id, flags) = match (button, down) {
                    (MouseButton::Left, true) => (0, MOUSEEVENTF_LEFTDOWN),
                    (MouseButton::Left, false) => (0, MOUSEEVENTF_LEFTUP),
                    (MouseButton::Right, true) => (1, MOUSEEVENTF_RIGHTDOWN),
                    (MouseButton::Right, false) => (1, MOUSEEVENTF_RIGHTUP),
                    (MouseButton::Middle, true) => (2, MOUSEEVENTF_MIDDLEDOWN),
                    (MouseButton::Middle, false) => (2, MOUSEEVENTF_MIDDLEUP),
                };
                if down {
                    self.buttons.insert(id);
                } else {
                    self.buttons.remove(&id);
                }
                win::mouse(0, 0, 0, flags);
            }
            InputEvent::Wheel { dx, dy } => {
                const WHEEL_DELTA: i32 = 120;
                if dy != 0 {
                    // Windows: positive wheel data scrolls up.
                    win::mouse(0, 0, -dy * WHEEL_DELTA, MOUSEEVENTF_WHEEL);
                }
                if dx != 0 {
                    win::mouse(0, 0, dx * WHEEL_DELTA, MOUSEEVENTF_HWHEEL);
                }
            }
            InputEvent::Scancode { code, extended, down } => {
                let mut flags = KEYEVENTF_SCANCODE;
                if extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                if down {
                    self.keys.insert((code, extended));
                } else {
                    flags |= KEYEVENTF_KEYUP;
                    self.keys.remove(&(code, extended));
                }
                win::send(&[win::key(0, code, flags)]);
            }
            InputEvent::Text(text) => {
                let inputs: Vec<INPUT> = text
                    .encode_utf16()
                    .flat_map(|u| {
                        [win::key(0, u, KEYEVENTF_UNICODE), win::key(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)]
                    })
                    .collect();
                win::send(&inputs);
            }
            InputEvent::Key { key, down, mods } => {
                let Some((vk, extended)) = vk_of(key) else {
                    return;
                };
                let mut seq = Vec::new();
                let vk_event = |vk: u16, extended: bool, up: bool| {
                    let mut flags = KEYBD_EVENT_FLAGS(0);
                    if extended {
                        flags |= KEYEVENTF_EXTENDEDKEY;
                    }
                    if up {
                        flags |= KEYEVENTF_KEYUP;
                    }
                    win::key(vk, win::scan_of(vk), flags)
                };
                let mod_keys = modifier_vks(mods);
                if down {
                    seq.extend(mod_keys.iter().map(|&m| vk_event(m, m == 0x5B, false)));
                    seq.push(vk_event(vk, extended, false));
                } else {
                    seq.push(vk_event(vk, extended, true));
                    seq.extend(mod_keys.iter().rev().map(|&m| vk_event(m, m == 0x5B, true)));
                }
                win::send(&seq);
            }
        }
    }

    fn release_all(&mut self) {
        for ev in self
            .keys
            .drain()
            .map(|(code, extended)| InputEvent::Scancode { code, extended, down: false })
            .chain(self.buttons.drain().map(|b| InputEvent::MouseButton {
                button: [MouseButton::Left, MouseButton::Right, MouseButton::Middle][b as usize],
                down: false,
            }))
            .collect::<Vec<_>>()
        {
            self.apply(ev);
        }
    }
}

/// Virtual-key code and the "extended key" flag for desktop-profile keys.
#[cfg(windows)]
fn vk_of(key: KeyCode) -> Option<(u16, bool)> {
    Some(match key {
        KeyCode::Enter => (0x0D, false),
        KeyCode::Tab => (0x09, false),
        KeyCode::Backspace => (0x08, false),
        KeyCode::Escape => (0x1B, false),
        KeyCode::Space => (0x20, false),
        KeyCode::PageUp => (0x21, true),
        KeyCode::PageDown => (0x22, true),
        KeyCode::End => (0x23, true),
        KeyCode::Home => (0x24, true),
        KeyCode::Left => (0x25, true),
        KeyCode::Up => (0x26, true),
        KeyCode::Right => (0x27, true),
        KeyCode::Down => (0x28, true),
        KeyCode::Insert => (0x2D, true),
        KeyCode::Delete => (0x2E, true),
        KeyCode::F(n) if (1..=12).contains(&n) => (0x70 + n as u16 - 1, false),
        KeyCode::F(_) => return None,
        KeyCode::Char(c) => (win::vk_of_char(c.to_ascii_lowercase())?, false),
    })
}

#[cfg(windows)]
fn modifier_vks(mods: Modifiers) -> Vec<u16> {
    [(mods.ctrl, 0x11), (mods.shift, 0x10), (mods.alt, 0x12), (mods.meta, 0x5B)]
        .into_iter()
        .filter_map(|(on, vk)| on.then_some(vk))
        .collect()
}

/// Makes absolute coordinates and capture use physical pixels.
/// Normalized (0..=65535) position on monitor `p` -> MOUSEEVENTF_VIRTUALDESK coordinates.
#[cfg(windows)]
fn virtual_desktop_point(p: super::capture::Place, x: u16, y: u16) -> (i32, i32) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };
    let (vl, vt, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        )
    };
    let px = p.left as f64 + x as f64 / 65535.0 * (p.width.saturating_sub(1)) as f64;
    let py = p.top as f64 + y as f64 / 65535.0 * (p.height.saturating_sub(1)) as f64;
    let nx = (px - vl as f64) * 65535.0 / (vw - 1).max(1) as f64;
    let ny = (py - vt as f64) * 65535.0 / (vh - 1).max(1) as f64;
    (nx.round() as i32, ny.round() as i32)
}

pub fn set_dpi_aware() {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}
