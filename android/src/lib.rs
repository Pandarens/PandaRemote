//! Panda Remote for Android: connects to your computers by ticket. The phone is only a viewer; the shared
//! library does the networking, decoding and protocol, this crate is the touch UI.

#![cfg(target_os = "android")]

mod session;
mod ui;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use eframe::egui::{self, Color32};
use eframe::winit::platform::android::activity::AndroidApp;
use panda_rc::{
    client::{self, viewer},
    config::{self, SavedHost},
    icons::Icon,
    proto::Profile,
    ticket::Ticket,
};
use tokio::runtime::Runtime;

use session::Session;
use ui::*;

#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("panda"));
    // Saved computers and this phone's identity live in the app's private storage.
    if let Some(dir) = app.internal_data_path() {
        // SAFETY: set once at startup, before any other thread reads the environment.
        unsafe { std::env::set_var("PANDA_HOME", dir) };
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    let options = eframe::NativeOptions { android_app: Some(app.clone()), ..Default::default() };
    let result = eframe::run_native(
        "Panda Remote",
        options,
        Box::new(move |cc| {
            ui::apply_style(&cc.egui_ctx);
            Ok(Box::new(App::new(cc.egui_ctx.clone(), rt, app)))
        }),
    );
    if let Err(e) = result {
        log::error!("eframe: {e}");
    }
}

#[derive(Clone)]
enum Presence {
    Online(String),
    Offline,
}

enum Event {
    Ended(Option<String>),
    PinNeeded(Option<String>),
}

struct PinPrompt {
    name: String,
    ticket: String,
    profile: Profile,
    pin: String,
    error: Option<String>,
}

pub struct App {
    rt: Runtime,
    ctx: egui::Context,
    android: AndroidApp,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    hosts: Vec<SavedHost>,
    presence: Arc<Mutex<HashMap<iroh::EndpointId, Presence>>>,
    targets: Arc<Mutex<Vec<iroh::EndpointId>>>,
    adding: Option<(String, String)>,
    message: Option<(bool, String)>,
    session: Option<Session>,
    pin: Option<PinPrompt>,
}

impl App {
    fn new(ctx: egui::Context, rt: Runtime, android: AndroidApp) -> Self {
        let (tx, rx) = mpsc::channel();
        let app = App {
            rt,
            ctx,
            android,
            tx,
            rx,
            hosts: config::load_hosts(),
            presence: Arc::default(),
            targets: Arc::default(),
            adding: None,
            message: None,
            session: None,
            pin: None,
        };
        app.spawn_presence();
        app
    }

    fn host_ids(&self) -> Vec<iroh::EndpointId> {
        self.hosts.iter().filter_map(|h| Ticket::decode(&h.ticket).ok()).map(|t| t.host_id).collect()
    }

    /// Pings saved computers every 20 seconds, like the desktop app.
    fn spawn_presence(&self) {
        let (targets, presence, ctx) = (self.targets.clone(), self.presence.clone(), self.ctx.clone());
        *targets.lock().unwrap() = self.host_ids();
        self.rt.spawn(async move {
            loop {
                let ids = targets.lock().unwrap().clone();
                let mut checks = tokio::task::JoinSet::new();
                for id in ids {
                    checks.spawn(async move { (id, client::ping(id).await) });
                }
                while let Some(Ok((id, r))) = checks.join_next().await {
                    let p = match r {
                        Ok(route) => Presence::Online(route),
                        Err(_) => Presence::Offline,
                    };
                    presence.lock().unwrap().insert(id, p);
                    ctx.request_repaint();
                }
                tokio::time::sleep(Duration::from_secs(20)).await;
            }
        });
    }

    fn connect(&mut self, name: String, ticket_str: String, profile: Profile, pin: Option<String>) {
        let ticket = match Ticket::decode(&ticket_str) {
            Ok(t) => t,
            Err(e) => {
                self.message = Some((true, format!("{e:#}")));
                return;
            }
        };
        let shared = Arc::new(viewer::Shared::default());
        *shared.status.lock().unwrap() = "Подключение…".into();
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_files_tx, files_rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = self.ctx.clone();
        let repaint: viewer::Repaint = Arc::new(move || ctx.request_repaint());
        let (tx, ctx2) = (self.tx.clone(), self.ctx.clone());
        let (shared2, out2) = (shared.clone(), out_tx.clone());
        let task = self.rt.spawn(async move {
            let res = async {
                let codecs = client::decoder::supported();
                let session = client::connect(&ticket, profile, pin.as_deref(), None, &codecs).await?;
                viewer::run(session, shared2, out2, out_rx, files_rx, repaint, true).await
            }
            .await;
            let event = match res.as_ref().err().and_then(|e| e.downcast_ref::<client::PinError>()) {
                Some(p) => Event::PinNeeded(p.0.clone()),
                None => Event::Ended(res.err().map(|e| format!("{e:#}"))),
            };
            let _ = tx.send(event);
            ctx2.request_repaint();
        });
        self.message = None;
        self.session = Some(Session::new(name, ticket_str, profile, shared, out_tx, task));
    }

    fn handle_events(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::Ended(err) => {
                    self.session = None;
                    self.android.hide_soft_input(true);
                    self.message = Some(match err {
                        Some(e) => (true, format!("Сеанс завершён: {e}")),
                        None => (false, "Сеанс завершён".into()),
                    });
                }
                Event::PinNeeded(error) => {
                    if let Some(s) = self.session.take() {
                        self.pin = Some(PinPrompt { name: s.name, ticket: s.ticket, profile: s.profile, pin: String::new(), error });
                    }
                }
            }
        }
    }

    fn hosts_screen(&mut self, ui: &mut egui::Ui) {
        *self.targets.lock().unwrap() = self.host_ids();
        let presence = self.presence.lock().unwrap().clone();
        let state = |h: &SavedHost| Ticket::decode(&h.ticket).ok().and_then(|t| presence.get(&t.host_id).cloned());

        ui.horizontal(|ui| {
            icon(ui, Icon::Panda, TEXT, 26.0);
            ui.label(semibold("Panda", 20.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if button(ui, Some(Icon::Plus), "Добавить", Kind::Primary).clicked() {
                    self.adding = Some((String::new(), String::new()));
                }
            });
        });
        ui.add_space(10.0);

        if let Some((name, ticket)) = self.adding.as_mut() {
            let mut done = None;
            card(ui, |ui| {
                ui.label(semibold("Новый компьютер", 16.0));
                ui.add_space(6.0);
                ui.add(egui::TextEdit::singleline(name).hint_text("Название, например «Дом»").desired_width(f32::INFINITY).margin(egui::Margin::symmetric(12, 10)));
                ui.add_space(6.0);
                ui.add(egui::TextEdit::singleline(ticket).hint_text("Билет panda1…").password(true).desired_width(f32::INFINITY).margin(egui::Margin::symmetric(12, 10)));
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    if button(ui, Some(Icon::Copy), "Вставить из буфера", Kind::Secondary).clicked() {
                        match clipboard_text(&self.android) {
                            Some(t) => *ticket = t.trim().to_string(),
                            None => self.message = Some((true, "В буфере обмена нет текста".into())),
                        }
                    }
                    if button(ui, None, "Добавить", Kind::Primary).clicked() {
                        done = Some(true);
                    }
                    if button(ui, None, "Отмена", Kind::Ghost).clicked() {
                        done = Some(false);
                    }
                });
            });
            match done {
                Some(true) => match Ticket::decode(ticket.trim()) {
                    Ok(t) => {
                        let name = if name.trim().is_empty() { t.host_id.fmt_short().to_string() } else { name.trim().to_string() };
                        self.hosts.retain(|h| Ticket::decode(&h.ticket).map(|x| x.host_id != t.host_id).unwrap_or(true));
                        self.hosts.push(SavedHost { name, ticket: t.encode(), last_connected: None, macs: Vec::new() });
                        let _ = config::save_hosts(&self.hosts);
                        self.adding = None;
                    }
                    Err(_) => self.message = Some((true, "Это не похоже на билет Panda: он начинается с panda1".into())),
                },
                Some(false) => self.adding = None,
                None => {}
            }
        }

        if self.hosts.is_empty() && self.adding.is_none() {
            card(ui, |ui| {
                ui.label(semibold("Как подключиться к своему компьютеру", 16.0));
                ui.add_space(6.0);
                for step in [
                    "1. На компьютере откройте Panda → «Этот ПК» → «Копировать» билет и перешлите его на телефон.",
                    "2. Скопируйте билет на телефоне.",
                    "3. Здесь нажмите «Добавить» → «Вставить из буфера».",
                ] {
                    ui.label(egui::RichText::new(step).color(SOFT));
                }
            });
        }

        let mut order: Vec<usize> = (0..self.hosts.len()).collect();
        order.sort_by_key(|&i| !matches!(state(&self.hosts[i]), Some(Presence::Online(_))));
        let mut action: Option<(usize, &str)> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for i in order {
                let h = &self.hosts[i];
                let st = state(h);
                let online = matches!(st, Some(Presence::Online(_)));
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (color, line) = match &st {
                            Some(Presence::Online(r)) => (GREEN, format!("В сети · {r}")),
                            Some(Presence::Offline) => (OFFLINE, "Не в сети".into()),
                            None => (YELLOW, "Проверка…".into()),
                        };
                        status_dot(ui, color);
                        ui.vertical(|ui| {
                            ui.label(semibold(&h.name, 16.0));
                            ui.label(muted(line).small());
                        });
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let kind = if online { Kind::Primary } else { Kind::Secondary };
                        if button(ui, Some(Icon::Monitor), "Подключиться", kind).clicked() {
                            action = Some((i, "desktop"));
                        }
                        if button(ui, Some(Icon::Gamepad), "Игра", Kind::Secondary).clicked() {
                            action = Some((i, "game"));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if button(ui, Some(Icon::Trash), "", Kind::Ghost).clicked() {
                                action = Some((i, "delete"));
                            }
                        });
                    });
                });
            }
        });
        if let Some((i, what)) = action {
            let h = self.hosts[i].clone();
            match what {
                "desktop" => self.connect(h.name, h.ticket, Profile::Desktop, None),
                "game" => self.connect(h.name, h.ticket, Profile::Game, None),
                _ => {
                    self.hosts.remove(i);
                    let _ = config::save_hosts(&self.hosts);
                }
            }
        }
        ui.ctx().request_repaint_after(Duration::from_secs(2));
    }

    fn pin_dialog(&mut self, ctx: &egui::Context) {
        let Some(p) = self.pin.as_mut() else { return };
        let (mut submit, mut cancel) = (false, false);
        let frame = egui::Frame::new().fill(SURFACE).stroke(egui::Stroke::new(1.0, RAISED_BORDER)).corner_radius(14.0).inner_margin(20.0);
        egui::Modal::new(egui::Id::new("pin")).frame(frame).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 64.0).clamp(240.0, 420.0));
            ui.label(semibold("Введите PIN-код", 18.0));
            ui.label(muted(format!("«{}» защищён PIN-кодом", p.name)));
            ui.add_space(10.0);
            ui.add(egui::TextEdit::singleline(&mut p.pin).password(true).hint_text("PIN-код").desired_width(f32::INFINITY).margin(egui::Margin::symmetric(12, 10)).font(egui::TextStyle::Heading));
            p.pin.retain(|c| c.is_ascii_digit());
            p.pin.truncate(8);
            if let Some(e) = &p.error {
                ui.label(egui::RichText::new(e).small().color(RED));
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if button(ui, None, "Войти", Kind::Primary).clicked() {
                    submit = true;
                }
                if button(ui, None, "Отмена", Kind::Ghost).clicked() {
                    cancel = true;
                }
            });
        });
        if cancel {
            self.pin = None;
        } else if submit && !p.pin.is_empty() {
            let p = self.pin.take().unwrap();
            self.connect(p.name, p.ticket, p.profile, Some(p.pin));
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_events();
        if let Some(s) = self.session.as_mut() {
            if s.show(ui, &self.android) {
                let s = self.session.take().unwrap();
                s.task.abort();
                self.android.hide_soft_input(true);
            }
            return;
        }
        // The top margin keeps content clear of the status bar.
        let body = egui::Frame::new().fill(BG).inner_margin(egui::Margin { left: 16, right: 16, top: 40, bottom: 16 });
        if let Some((is_err, text)) = self.message.clone() {
            egui::Panel::bottom("msg").frame(egui::Frame::new().fill(SURFACE).inner_margin(12.0)).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(text).color(if is_err { RED } else { TEXT }));
                    if button(ui, Some(Icon::Close), "", Kind::Ghost).clicked() {
                        self.message = None;
                    }
                });
            });
        }
        egui::CentralPanel::no_frame().frame(body).show(ui, |ui| self.hosts_screen(ui));
        self.pin_dialog(&ui.ctx().clone());
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        Color32::BLACK.to_normalized_gamma_f32()
    }
}

/// Text on the Android clipboard (ClipboardManager over JNI).
fn clipboard_text(app: &AndroidApp) -> Option<String> {
    use jni::objects::{JObject, JString};
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr() as *mut _) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr() as jni::sys::jobject) };
    let name = env.new_string("clipboard").ok()?;
    let manager = env
        .call_method(&activity, "getSystemService", "(Ljava/lang/String;)Ljava/lang/Object;", &[(&name).into()])
        .ok()?
        .l()
        .ok()?;
    let clip = env.call_method(&manager, "getPrimaryClip", "()Landroid/content/ClipData;", &[]).ok()?.l().ok()?;
    if clip.is_null() {
        return None;
    }
    let item = env.call_method(&clip, "getItemAt", "(I)Landroid/content/ClipData$Item;", &[0i32.into()]).ok()?.l().ok()?;
    let text = env
        .call_method(&item, "coerceToText", "(Landroid/content/Context;)Ljava/lang/CharSequence;", &[(&activity).into()])
        .ok()?
        .l()
        .ok()?;
    let s = env.call_method(&text, "toString", "()Ljava/lang/String;", &[]).ok()?.l().ok()?;
    let s = JString::from(s);
    let out: String = env.get_string(&s).ok()?.into();
    // The activity reference belongs to the Android runtime.
    std::mem::forget(activity);
    Some(out)
}
