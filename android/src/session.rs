//! A session on the phone: the remote screen full-window, touch as mouse, the soft keyboard as keyboard.
//!
//! Touch: a tap clicks, a long press right-clicks, dragging drags (left button held), two fingers scroll.

use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use eframe::egui::{self, Color32, Pos2, Rect, Sense, TextureHandle, TextureOptions, Vec2};
use android_activity::AndroidApp;
use panda_rc::{
    client::viewer,
    icons::Icon,
    proto::{ClientMsg, InputEvent, KeyCode, Modifiers, MouseButton, Profile},
};
use tokio::{sync::mpsc::UnboundedSender, task::JoinHandle};

use crate::ui::*;

const LONG_PRESS: Duration = Duration::from_millis(550);
/// Finger movement (points) after which a touch is a drag, not a tap.
const DRAG_SLOP: f32 = 14.0;
/// Two-finger movement (points) per wheel notch.
const SCROLL_STEP: f32 = 36.0;

pub struct Session {
    pub name: String,
    pub ticket: String,
    pub profile: Profile,
    pub shared: Arc<viewer::Shared>,
    out: UnboundedSender<ClientMsg>,
    pub task: JoinHandle<()>,
    texture: Option<TextureHandle>,
    size: Option<(usize, usize)>,
    touch: Option<Touch>,
    scroll: f32,
    keyboard: bool,
    toolbar: bool,
}

/// The finger currently down.
struct Touch {
    start: Pos2,
    at: Instant,
    dragging: bool,
    /// The long press already right-clicked.
    done: bool,
}

impl Session {
    pub fn new(
        name: String,
        ticket: String,
        profile: Profile,
        shared: Arc<viewer::Shared>,
        out: UnboundedSender<ClientMsg>,
        task: JoinHandle<()>,
    ) -> Self {
        Session {
            name,
            ticket,
            profile,
            shared,
            out,
            task,
            texture: None,
            size: None,
            touch: None,
            scroll: 0.0,
            keyboard: false,
            toolbar: true,
        }
    }

    fn send(&self, ev: InputEvent) {
        let _ = self.out.send(ClientMsg::Input(ev));
    }

    fn click(&self, button: MouseButton) {
        self.send(InputEvent::MouseButton { button, down: true });
        self.send(InputEvent::MouseButton { button, down: false });
    }

    /// Draws the session; returns true when the user leaves it.
    pub fn show(&mut self, ui: &mut egui::Ui, android: &AndroidApp) -> bool {
        let mut leave = false;
        if let Some(frame) = self.shared.frame.lock().unwrap().take() {
            self.size = Some((frame.width, frame.height));
            let image = egui::ColorImage::from_rgba_unmultiplied([frame.width, frame.height], &frame.to_rgba());
            match &mut self.texture {
                Some(t) => t.set(image, TextureOptions::LINEAR),
                None => self.texture = Some(ui.ctx().load_texture("remote", image, TextureOptions::LINEAR)),
            }
        }
        if self.toolbar {
            leave = self.toolbar_panel(ui, android);
        }
        if self.keyboard {
            self.keys_panel(ui);
        }
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let full = ui.max_rect();
            ui.painter().rect_filled(full, 0.0, Color32::BLACK);
            let Some((fw, fh)) = self.size else {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.spinner();
                        ui.label(self.shared.status.lock().unwrap().clone());
                    })
                });
                return;
            };
            let size = Vec2::new(fw as f32, fh as f32);
            let scale = (full.width() / size.x).min(full.height() / size.y);
            let rect = Rect::from_center_size(full.center(), size * scale);
            if let Some(tex) = &self.texture {
                ui.painter().image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            }
            let resp = ui.allocate_rect(full, Sense::click_and_drag());
            self.touch_input(ui, rect, &resp);
            self.key_input(ui);
        });
        if !self.toolbar {
            // A small handle brings the toolbar back.
            egui::Area::new(egui::Id::unique("handle")).anchor(egui::Align2::RIGHT_TOP, Vec2::new(-12.0, 34.0)).show(ui.ctx(), |ui| {
                if button_sized(ui, Some(Icon::More), "", Kind::Secondary, 40.0).clicked() {
                    self.toolbar = true;
                }
            });
        }
        leave
    }

    fn toolbar_panel(&mut self, ui: &mut egui::Ui, android: &AndroidApp) -> bool {
        let mut leave = false;
        let frame = egui::Frame::new().fill(TOOLBAR).inner_margin(egui::Margin { left: 10, right: 10, top: 30, bottom: 8 });
        egui::Panel::top("bar").frame(frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                if button_sized(ui, Some(Icon::Close), "", Kind::Danger, 40.0).clicked() {
                    leave = true;
                }
                let status = self.shared.status.lock().unwrap().clone();
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(semibold(&self.name, 15.0));
                    let short = status.split(" · ").take(3).collect::<Vec<_>>().join(" · ");
                    ui.add(egui::Label::new(muted(short).small()).truncate());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if button_sized(ui, Some(Icon::ChevronDown), "", Kind::Ghost, 40.0).clicked() {
                        self.toolbar = false;
                    }
                    let kind = if self.keyboard { Kind::Active } else { Kind::Secondary };
                    if button_sized(ui, Some(Icon::Key), "", kind, 40.0).clicked() {
                        self.keyboard ^= true;
                        if self.keyboard {
                            android.show_soft_input(true);
                        } else {
                            android.hide_soft_input(true);
                        }
                    }
                    if self.shared.secure_desktop.load(Ordering::Relaxed) {
                        icon(ui, Icon::Lock, YELLOW, 20.0);
                    }
                });
            });
        });
        leave
    }

    /// Keys a phone keyboard doesn't have.
    fn keys_panel(&mut self, ui: &mut egui::Ui) {
        let frame = egui::Frame::new().fill(TOOLBAR).inner_margin(8.0);
        egui::Panel::bottom("keys").frame(frame).show(ui, |ui| {
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.horizontal(|ui| {
                    let none = Modifiers::default();
                    let ctrl = Modifiers { ctrl: true, ..Default::default() };
                    for (label, key, mods) in [
                        ("Esc", KeyCode::Escape, none),
                        ("Tab", KeyCode::Tab, none),
                        ("←", KeyCode::Left, none),
                        ("↑", KeyCode::Up, none),
                        ("↓", KeyCode::Down, none),
                        ("→", KeyCode::Right, none),
                        ("Del", KeyCode::Delete, none),
                        ("Ctrl+C", KeyCode::Char('c'), ctrl),
                        ("Ctrl+V", KeyCode::Char('v'), ctrl),
                        ("Ctrl+Z", KeyCode::Char('z'), ctrl),
                    ] {
                        if button_sized(ui, None, label, Kind::Secondary, 40.0).clicked() {
                            self.send(InputEvent::Key { key, down: true, mods });
                            self.send(InputEvent::Key { key, down: false, mods });
                        }
                    }
                    if button_sized(ui, None, "Ctrl+Alt+Del", Kind::Secondary, 40.0).clicked() {
                        let _ = self.out.send(ClientMsg::SecureAttention);
                    }
                });
            });
        });
    }

    fn touch_input(&mut self, ui: &egui::Ui, rect: Rect, resp: &egui::Response) {
        let norm = |p: Pos2| {
            let x = ((p.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
            let y = ((p.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
            ((x * 65535.0) as u16, (y * 65535.0) as u16)
        };
        // Two fingers: scroll, and forget the single-finger gesture.
        if let Some(mt) = ui.input(|i| i.multi_touch())
            && mt.num_touches >= 2
        {
            if self.touch.as_ref().is_some_and(|t| t.dragging) {
                self.send(InputEvent::MouseButton { button: MouseButton::Left, down: false });
            }
            self.touch = None;
            self.scroll += mt.translation_delta.y;
            while self.scroll.abs() >= SCROLL_STEP {
                let dir = self.scroll.signum();
                // Fingers moving up scroll the page down, as on the phone.
                self.send(InputEvent::Wheel { dx: 0, dy: -dir as i32 });
                self.scroll -= dir * SCROLL_STEP;
            }
            return;
        }
        let pos = ui.input(|i| i.pointer.interact_pos());
        let down = ui.input(|i| i.pointer.primary_down());
        match (&mut self.touch, down, pos) {
            (None, true, Some(p)) if resp.hovered() || resp.is_pointer_button_down_on() => {
                let (x, y) = norm(p);
                self.send(InputEvent::MouseMove { x, y });
                self.touch = Some(Touch { start: p, at: Instant::now(), dragging: false, done: false });
            }
            (Some(t), true, Some(p)) => {
                if !t.dragging && !t.done && (p - t.start).length() > DRAG_SLOP {
                    t.dragging = true;
                    let _ = self.out.send(ClientMsg::Input(InputEvent::MouseButton { button: MouseButton::Left, down: true }));
                }
                if t.dragging {
                    let (x, y) = norm(p);
                    let _ = self.out.send(ClientMsg::Input(InputEvent::MouseMove { x, y }));
                } else if !t.done && t.at.elapsed() >= LONG_PRESS {
                    t.done = true;
                    let _ = self.out.send(ClientMsg::Input(InputEvent::MouseButton { button: MouseButton::Right, down: true }));
                    let _ = self.out.send(ClientMsg::Input(InputEvent::MouseButton { button: MouseButton::Right, down: false }));
                }
                ui.ctx().request_repaint_after(Duration::from_millis(50));
            }
            (Some(_), false, _) => {
                let t = self.touch.take().unwrap();
                if t.dragging {
                    self.send(InputEvent::MouseButton { button: MouseButton::Left, down: false });
                } else if !t.done {
                    self.click(MouseButton::Left);
                }
            }
            _ => {}
        }
    }

    /// Soft keyboard: typed text goes as text, editing keys as keys.
    fn key_input(&self, ui: &egui::Ui) {
        let events = ui.input(|i| i.events.clone());
        for ev in events {
            match ev {
                egui::Event::Text(t) => self.send(InputEvent::Text(t)),
                egui::Event::Key { key, pressed, modifiers, .. } => {
                    let code = match key {
                        egui::Key::Enter => KeyCode::Enter,
                        egui::Key::Backspace => KeyCode::Backspace,
                        egui::Key::Delete => KeyCode::Delete,
                        egui::Key::Tab => KeyCode::Tab,
                        egui::Key::Escape => KeyCode::Escape,
                        egui::Key::ArrowLeft => KeyCode::Left,
                        egui::Key::ArrowRight => KeyCode::Right,
                        egui::Key::ArrowUp => KeyCode::Up,
                        egui::Key::ArrowDown => KeyCode::Down,
                        _ => continue,
                    };
                    let mods = Modifiers { ctrl: modifiers.ctrl, shift: modifiers.shift, alt: modifiers.alt, meta: false };
                    self.send(InputEvent::Key { key: code, down: pressed, mods });
                }
                _ => {}
            }
        }
    }
}
