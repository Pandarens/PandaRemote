//! Desktop GUI (eframe/egui): saved hosts, remote-desktop viewer, "this PC" host panel.

use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use eframe::egui::{self, Color32, Pos2, Rect, Sense, TextureHandle, TextureOptions, Vec2};
use tokio::{runtime::Runtime, sync::mpsc as tmpsc, task::JoinHandle};

use crate::{
    client::{self, gamepad, viewer},
    config::{self, HostConfig, JournalEntry, SavedHost},
    host::Host,
    proto::{ClientMsg, InputEvent, KeyCode, Modifiers, MouseButton, Profile},
    ticket::Ticket,
    yuv_render::{self, PendingFrame},
};

pub fn run(rt: Runtime, initial: Option<(Ticket, Profile)>) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Panda Remote")
            .with_inner_size([1100.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Panda Remote",
        options,
        Box::new(move |cc| {
            let gpu_video = yuv_render::init(cc.wgpu_render_state.as_ref());
            Ok(Box::new(App::new(cc.egui_ctx.clone(), rt, initial, gpu_video)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

enum UiEvent {
    HostStarted(Result<Host>),
    HostOnline,
    Connected { ticket: String },
    DesktopEnded(Option<String>),
}

#[derive(PartialEq)]
enum Tab {
    Hosts,
    ThisPc,
}

struct App {
    rt: Runtime,
    ctx: egui::Context,
    tx: mpsc::Sender<UiEvent>,
    rx: mpsc::Receiver<UiEvent>,
    tab: Tab,
    message: Option<(bool, String)>,

    hosts: Vec<SavedHost>,
    new_name: String,
    new_ticket: String,

    this_pc: ThisPc,
    desktop: Option<DesktopView>,
    /// NV12 frames are drawn by a wgpu shader; otherwise converted to RGBA on the CPU.
    gpu_video: bool,
}

struct ThisPc {
    host: Option<Host>,
    starting: bool,
    online: bool,
    show_ticket: bool,
    confirm_rotate: bool,
    journal: Vec<JournalEntry>,
    journal_at: Option<Instant>,
}

struct DesktopView {
    name: String,
    profile: Profile,
    shared: Arc<viewer::Shared>,
    out: tmpsc::UnboundedSender<ClientMsg>,
    task: JoinHandle<()>,
    texture: Option<TextureHandle>,
    pending: PendingFrame,
    frame_size: Option<(usize, usize)>,
    last_pos: Option<(u16, u16)>,
    wheel: Vec2,
    fullscreen: bool,
    /// Present in the game profile.
    game: Option<GameInput>,
}

/// Game-profile input: captured pointer, raw mouse deltas, keys as scancodes, gamepad forwarding.
struct GameInput {
    captured: bool,
    mods: egui::Modifiers,
    pressed: HashSet<(u16, bool)>,
    motion: Vec2,
    pad_stop: Arc<AtomicBool>,
}

impl Drop for GameInput {
    fn drop(&mut self) {
        self.pad_stop.store(true, Ordering::Relaxed);
    }
}

impl App {
    fn new(ctx: egui::Context, rt: Runtime, initial: Option<(Ticket, Profile)>, gpu_video: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut app = App {
            rt,
            ctx,
            tx,
            rx,
            tab: Tab::Hosts,
            message: None,
            hosts: config::load_hosts(),
            new_name: String::new(),
            new_ticket: String::new(),
            this_pc: ThisPc {
                host: None,
                starting: false,
                online: false,
                show_ticket: false,
                confirm_rotate: false,
                journal: Vec::new(),
                journal_at: None,
            },
            desktop: None,
            gpu_video,
        };
        if let Some((t, profile)) = initial {
            app.start_session("Удалённый ПК".into(), t.encode(), profile);
        }
        app
    }

    fn send_event(&self) -> impl Fn(UiEvent) + Send + Clone + 'static {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        move |ev| {
            let _ = tx.send(ev);
            ctx.request_repaint();
        }
    }

    fn handle_events(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                UiEvent::HostStarted(Ok(host)) => {
                    self.this_pc.starting = false;
                    self.this_pc.host = Some(host);
                }
                UiEvent::HostStarted(Err(e)) => {
                    self.this_pc.starting = false;
                    self.message = Some((true, format!("Не удалось запустить хост: {e:#}")));
                }
                UiEvent::HostOnline => self.this_pc.online = true,
                UiEvent::Connected { ticket } => {
                    if let Some(h) = self.hosts.iter_mut().find(|h| h.ticket == ticket) {
                        h.last_connected = Some(config::now_rfc3339());
                        let _ = config::save_hosts(&self.hosts);
                    }
                }
                UiEvent::DesktopEnded(err) => {
                    if let Some(d) = self.desktop.take() {
                        if d.fullscreen {
                            self.ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                        }
                    }
                    self.message = Some(match err {
                        Some(e) => (true, format!("Сеанс завершён: {e}")),
                        None => (false, "Сеанс завершён".into()),
                    });
                }
            }
        }
    }

    // ---------- actions ----------

    fn start_session(&mut self, name: String, ticket_str: String, profile: Profile) {
        let ticket = match Ticket::decode(&ticket_str) {
            Ok(t) => t,
            Err(e) => {
                self.message = Some((true, format!("{e:#}")));
                return;
            }
        };
        let shared = Arc::new(viewer::Shared::default());
        *shared.status.lock().unwrap() = "Подключение…".into();
        let (out_tx, out_rx) = tmpsc::unbounded_channel();
        let send = self.send_event();
        let ctx = self.ctx.clone();
        let repaint: viewer::Repaint = Arc::new(move || ctx.request_repaint());
        let (shared2, out_tx2) = (shared.clone(), out_tx.clone());
        let task = self.rt.spawn(async move {
            let res = async {
                let session = client::connect(&ticket, profile).await?;
                send(UiEvent::Connected { ticket: ticket_str });
                viewer::run(session, shared2, out_tx2, out_rx, repaint, true).await
            }
            .await;
            send(UiEvent::DesktopEnded(res.err().map(|e| format!("{e:#}"))));
        });
        let game = (profile == Profile::Game).then(|| {
            let pad_stop = Arc::new(AtomicBool::new(false));
            *shared.rumble.lock().unwrap() = Some(gamepad::spawn(out_tx.clone(), pad_stop.clone()));
            GameInput { captured: false, mods: egui::Modifiers::default(), pressed: HashSet::new(), motion: Vec2::ZERO, pad_stop }
        });
        self.message = None;
        self.desktop = Some(DesktopView {
            name,
            profile,
            shared,
            out: out_tx,
            task,
            texture: None,
            pending: PendingFrame::default(),
            frame_size: None,
            last_pos: None,
            wheel: Vec2::ZERO,
            fullscreen: false,
            game,
        });
    }

    fn start_host(&mut self) {
        self.this_pc.starting = true;
        self.this_pc.online = false;
        let send = self.send_event();
        self.rt.spawn(async move {
            let res = async { Host::start(HostConfig::load_or_create()?).await }.await;
            match res {
                Ok(host) => {
                    // `online()` borrows the host; hand it to the UI first and poll readiness separately.
                    let ep_ready = host.online_handle();
                    send(UiEvent::HostStarted(Ok(host)));
                    ep_ready.online().await;
                    send(UiEvent::HostOnline);
                }
                Err(e) => send(UiEvent::HostStarted(Err(e))),
            }
        });
    }

    fn stop_host(&mut self) {
        if let Some(host) = self.this_pc.host.take() {
            self.rt.spawn(host.shutdown());
        }
        self.this_pc.online = false;
    }

    // ---------- views ----------

    fn hosts_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Мои компьютеры");
        ui.add_space(6.0);

        let mut action: Option<(usize, &'static str)> = None;
        if self.hosts.is_empty() {
            ui.label("Пока пусто. Запустите хост на удалённом ПК (вкладка «Этот компьютер»), скопируйте билет и добавьте его ниже.");
        }
        egui::Grid::new("hosts").striped(true).spacing([16.0, 8.0]).show(ui, |ui| {
            for (i, h) in self.hosts.iter().enumerate() {
                ui.label(egui::RichText::new(&h.name).strong());
                let id = Ticket::decode(&h.ticket).map(|t| t.host_id.fmt_short().to_string()).unwrap_or_default();
                ui.label(egui::RichText::new(id).monospace().weak());
                ui.label(h.last_connected.as_deref().map(|t| format!("был: {}", &t[..16.min(t.len())])).unwrap_or_default());
                if ui.button("🖥 Рабочий стол").clicked() {
                    action = Some((i, "desktop"));
                }
                if ui.button("🎮 Игра").clicked() {
                    action = Some((i, "game"));
                }
                if ui.button("🗑").on_hover_text("Удалить").clicked() {
                    action = Some((i, "delete"));
                }
                ui.end_row();
            }
        });
        if let Some((i, what)) = action {
            let h = self.hosts[i].clone();
            match what {
                "desktop" => self.start_session(h.name, h.ticket, Profile::Desktop),
                "game" => self.start_session(h.name, h.ticket, Profile::Game),
                _ => {
                    self.hosts.remove(i);
                    let _ = config::save_hosts(&self.hosts);
                }
            }
        }

        ui.add_space(16.0);
        ui.separator();
        ui.label(egui::RichText::new("Добавить компьютер").strong());
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.new_name).hint_text("Название").desired_width(160.0));
            ui.add(egui::TextEdit::singleline(&mut self.new_ticket).hint_text("Билет panda1…").desired_width(480.0).password(true));
            if ui.button("Добавить").clicked() {
                match Ticket::decode(&self.new_ticket) {
                    Ok(t) => {
                        let name = if self.new_name.trim().is_empty() { t.host_id.fmt_short().to_string() } else { self.new_name.trim().to_string() };
                        self.hosts.retain(|h| Ticket::decode(&h.ticket).map(|x| x.host_id != t.host_id).unwrap_or(true));
                        self.hosts.push(SavedHost { name, ticket: t.encode(), last_connected: None });
                        let _ = config::save_hosts(&self.hosts);
                        self.new_name.clear();
                        self.new_ticket.clear();
                    }
                    Err(e) => self.message = Some((true, format!("{e:#}"))),
                }
            }
        });
    }

    fn this_pc_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Этот компьютер");
        ui.add_space(6.0);

        let (status, color) = match (&self.this_pc.host, self.this_pc.starting, self.this_pc.online) {
            (_, true, _) => ("Запуск…", Color32::YELLOW),
            (Some(_), _, true) => ("В сети — к этому ПК можно подключиться откуда угодно", Color32::from_rgb(80, 200, 120)),
            (Some(_), _, false) => ("Подключение к сети…", Color32::YELLOW),
            (None, _, _) => ("Хост выключен", Color32::GRAY),
        };
        ui.horizontal(|ui| {
            ui.colored_label(color, "●");
            ui.label(status);
        });
        ui.horizontal(|ui| {
            if self.this_pc.host.is_none() {
                if ui.add_enabled(!self.this_pc.starting, egui::Button::new("▶ Разрешить подключения")).clicked() {
                    self.start_host();
                }
            } else if ui.button("■ Остановить").clicked() {
                self.stop_host();
            }
        });

        if let Some(host) = &self.this_pc.host {
            ui.add_space(8.0);
            ui.label(format!("ID компьютера (постоянный адрес): {}", host.ticket.host_id));
            ui.horizontal(|ui| {
                ui.label("Билет:");
                let ticket = host.ticket.encode();
                if self.this_pc.show_ticket {
                    ui.label(egui::RichText::new(&ticket).monospace());
                } else {
                    ui.label("••••••••••••");
                }
                if ui.button(if self.this_pc.show_ticket { "Скрыть" } else { "Показать" }).clicked() {
                    self.this_pc.show_ticket ^= true;
                }
                if ui.button("📋 Копировать").clicked() {
                    ui.ctx().copy_text(ticket);
                    self.message = Some((false, "Билет скопирован. Храните его как пароль.".into()));
                }
            });
            ui.horizontal(|ui| {
                if !self.this_pc.confirm_rotate {
                    if ui.button("Сменить секрет…").clicked() {
                        self.this_pc.confirm_rotate = true;
                    }
                } else {
                    ui.label("Все выданные билеты перестанут работать. Продолжить?");
                    if ui.button("Да, сменить").clicked() {
                        self.this_pc.confirm_rotate = false;
                        let res = HostConfig::load_or_create().and_then(|mut c| c.rotate_secret());
                        match res {
                            Ok(()) => {
                                self.stop_host();
                                self.start_host();
                                self.message = Some((false, "Секрет обновлён, скопируйте новый билет.".into()));
                            }
                            Err(e) => self.message = Some((true, format!("{e:#}"))),
                        }
                    }
                    if ui.button("Отмена").clicked() {
                        self.this_pc.confirm_rotate = false;
                    }
                }
            });
        }

        ui.add_space(12.0);
        ui.separator();
        ui.label(egui::RichText::new("Журнал подключений").strong());
        if self.this_pc.journal_at.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            self.this_pc.journal = config::read_journal(50);
            self.this_pc.journal_at = Some(Instant::now());
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("journal").striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
                for e in &self.this_pc.journal {
                    ui.label(e.ts.replace('T', " ").trim_end_matches('Z'));
                    let ev = match e.event.as_str() {
                        "connected" => "✅ подключение",
                        "disconnected" => "⏹ отключение",
                        "auth_failed" => "⛔ отказ",
                        other => other,
                    };
                    ui.label(ev);
                    ui.label(&e.client_name);
                    ui.label(egui::RichText::new(&e.client_id[..10.min(e.client_id.len())]).monospace().weak());
                    ui.label(&e.detail);
                    ui.end_row();
                }
            });
        });
        ui.ctx().request_repaint_after(Duration::from_secs(2));
    }

    fn desktop_view(&mut self, ui: &mut egui::Ui) {
        let gpu_video = self.gpu_video;
        let Some(view) = self.desktop.as_mut() else { return };
        let mut disconnect = false;

        if !view.fullscreen {
            egui::Panel::top("session_bar").show(ui, |ui| {
                ui.horizontal(|ui| {
                    let icon = if view.profile == Profile::Game { "🎮" } else { "🖥" };
                    ui.label(egui::RichText::new(format!("{icon} {}", view.name)).strong());
                    ui.label(view.shared.status.lock().unwrap().clone());
                    if let Some(g) = &view.game {
                        let hint = if g.captured {
                            "мышь захвачена · Ctrl+Alt+Shift+Q — отпустить"
                        } else {
                            "кликните по картинке, чтобы захватить мышь"
                        };
                        ui.label(egui::RichText::new(hint).weak());
                    }
                    if let Some(notice) = view.shared.notice.lock().unwrap().as_deref() {
                        ui.colored_label(Color32::from_rgb(230, 170, 60), format!("🎮 геймпад: {notice}"));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Отключиться").clicked() {
                            disconnect = true;
                        }
                        if ui.button("Полный экран (F11)").clicked() {
                            view.fullscreen = true;
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                        }
                    });
                });
            });
        }

        egui::CentralPanel::no_frame().show(ui, |ui| {
            ui.painter().rect_filled(ui.max_rect(), 0.0, Color32::BLACK);
            if let Some(frame) = view.shared.frame.lock().unwrap().take() {
                view.frame_size = Some((frame.width, frame.height));
                if gpu_video {
                    *view.pending.lock().unwrap() = Some(frame);
                } else {
                    let image = egui::ColorImage::from_rgba_unmultiplied([frame.width, frame.height], &frame.to_rgba());
                    match &mut view.texture {
                        Some(t) => t.set(image, TextureOptions::LINEAR),
                        None => view.texture = Some(ui.ctx().load_texture("remote", image, TextureOptions::LINEAR)),
                    }
                }
            }
            let Some((fw, fh)) = view.frame_size else {
                ui.centered_and_justified(|ui| ui.label(view.shared.status.lock().unwrap().clone()));
                return;
            };
            let avail = ui.max_rect();
            let size = Vec2::new(fw as f32, fh as f32);
            let scale = (avail.width() / size.x).min(avail.height() / size.y);
            let rect = Rect::from_center_size(avail.center(), size * scale);
            if gpu_video {
                yuv_render::paint(ui.painter(), rect, view.pending.clone());
            } else if let Some(tex) = &view.texture {
                ui.painter().image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            }
            let resp = ui.allocate_rect(rect, Sense::click_and_drag());
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Default);
            }
            let (events, mods, focused) = ui.input(|i| (i.events.clone(), i.modifiers, i.focused));
            if let Some(g) = view.game.as_mut() {
                if !g.captured && resp.clicked() {
                    g.captured = true;
                    g.mods = egui::Modifiers::default();
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::CursorGrab(egui::CursorGrab::Locked));
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::CursorVisible(false));
                    return;
                }
                if g.captured {
                    let release = !focused || game_input(g, &events, mods, &view.out, &mut view.wheel, &mut view.fullscreen, ui.ctx());
                    if release {
                        release_capture(g, &view.out, ui.ctx());
                    }
                    return;
                }
            }
            for ev in events {
                if let egui::Event::Key { key: egui::Key::F11, pressed: true, .. } = ev {
                    view.fullscreen ^= true;
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(view.fullscreen));
                    continue;
                }
                for msg in translate(&ev, rect, &mut view.last_pos, &mut view.wheel) {
                    let _ = view.out.send(ClientMsg::Input(msg));
                }
            }
        });

        if disconnect && let Some(mut d) = self.desktop.take() {
            if let Some(g) = d.game.as_mut() {
                release_capture(g, &d.out, ui.ctx());
            }
            if d.fullscreen {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            }
            d.task.abort();
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_events();

        if self.desktop.is_some() {
            self.desktop_view(ui);
            return;
        }

        egui::Panel::top("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("🐼 Panda Remote");
                ui.add_space(16.0);
                ui.selectable_value(&mut self.tab, Tab::Hosts, "Подключения");
                ui.selectable_value(&mut self.tab, Tab::ThisPc, "Этот компьютер");
            });
        });
        if let Some((is_err, text)) = self.message.clone() {
            egui::Panel::bottom("message").show(ui, |ui| {
                ui.horizontal(|ui| {
                    let color = if is_err { Color32::from_rgb(230, 90, 90) } else { Color32::from_rgb(80, 200, 120) };
                    ui.colored_label(color, text);
                    if ui.small_button("✕").clicked() {
                        self.message = None;
                    }
                });
            });
        }
        egui::CentralPanel::default_margins().show(ui, |ui| match self.tab {
            Tab::Hosts => self.hosts_tab(ui),
            Tab::ThisPc => self.this_pc_tab(ui),
        });
    }
}

// ---------- game input ----------

const SC_LCTRL: u16 = 0x1D;
const SC_LSHIFT: u16 = 0x2A;
const SC_LALT: u16 = 0x38;

/// Forwards one frame of captured input. Returns true when the user asked to release the pointer.
fn game_input(
    g: &mut GameInput,
    events: &[egui::Event],
    mods: egui::Modifiers,
    out: &tmpsc::UnboundedSender<ClientMsg>,
    wheel: &mut Vec2,
    fullscreen: &mut bool,
    ctx: &egui::Context,
) -> bool {
    let send = |ev: InputEvent| {
        let _ = out.send(ClientMsg::Input(ev));
    };
    // egui reports modifiers as state, not key events: diff them into scancodes.
    for (was, now, code) in [(g.mods.ctrl, mods.ctrl, SC_LCTRL), (g.mods.shift, mods.shift, SC_LSHIFT), (g.mods.alt, mods.alt, SC_LALT)] {
        if was != now {
            if now {
                g.pressed.insert((code, false));
            } else {
                g.pressed.remove(&(code, false));
            }
            send(InputEvent::Scancode { code, extended: false, down: now });
        }
    }
    g.mods = mods;

    for ev in events {
        match ev {
            egui::Event::Key { key, physical_key, pressed, modifiers, .. } => {
                let key = physical_key.unwrap_or(*key);
                if key == egui::Key::Q && modifiers.ctrl && modifiers.alt && modifiers.shift {
                    return true;
                }
                if key == egui::Key::F11 {
                    if *pressed {
                        *fullscreen ^= true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(*fullscreen));
                    }
                    continue;
                }
                if let Some((code, extended)) = scancode(key) {
                    if *pressed {
                        g.pressed.insert((code, extended));
                    } else {
                        g.pressed.remove(&(code, extended));
                    }
                    send(InputEvent::Scancode { code, extended, down: *pressed });
                }
            }
            // egui turns Ctrl+C/X/V into clipboard events; Ctrl is already down on the host.
            egui::Event::Copy | egui::Event::Cut | egui::Event::Paste(_) => {
                let code = match ev {
                    egui::Event::Copy => 0x2E,
                    egui::Event::Cut => 0x2D,
                    _ => 0x2F,
                };
                send(InputEvent::Scancode { code, extended: false, down: true });
                send(InputEvent::Scancode { code, extended: false, down: false });
            }
            egui::Event::MouseMoved(delta) => {
                g.motion += *delta;
                let (dx, dy) = (g.motion.x.trunc(), g.motion.y.trunc());
                g.motion -= Vec2::new(dx, dy);
                if dx != 0.0 || dy != 0.0 {
                    send(InputEvent::MouseRelative { dx: dx as i32, dy: dy as i32 });
                }
            }
            egui::Event::PointerButton { button, pressed, .. } => {
                let button = match button {
                    egui::PointerButton::Primary => MouseButton::Left,
                    egui::PointerButton::Secondary => MouseButton::Right,
                    egui::PointerButton::Middle => MouseButton::Middle,
                    _ => continue,
                };
                send(InputEvent::MouseButton { button, down: *pressed });
            }
            egui::Event::MouseWheel { .. } => {
                for msg in translate(ev, Rect::EVERYTHING, &mut None, wheel) {
                    send(msg);
                }
            }
            _ => {}
        }
    }
    false
}

fn release_capture(g: &mut GameInput, out: &tmpsc::UnboundedSender<ClientMsg>, ctx: &egui::Context) {
    for (code, extended) in g.pressed.drain() {
        let _ = out.send(ClientMsg::Input(InputEvent::Scancode { code, extended, down: false }));
    }
    g.captured = false;
    g.mods = egui::Modifiers::default();
    ctx.send_viewport_cmd(egui::ViewportCommand::CursorGrab(egui::CursorGrab::None));
    ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(true));
}

/// PC/AT set-1 scancode for a physical key (and whether it needs the E0 "extended" prefix).
fn scancode(key: egui::Key) -> Option<(u16, bool)> {
    use egui::Key as K;
    let plain = |c: u16| Some((c, false));
    let ext = |c: u16| Some((c, true));
    match key {
        K::Escape => plain(0x01),
        K::Num1 => plain(0x02),
        K::Num2 => plain(0x03),
        K::Num3 => plain(0x04),
        K::Num4 => plain(0x05),
        K::Num5 => plain(0x06),
        K::Num6 => plain(0x07),
        K::Num7 => plain(0x08),
        K::Num8 => plain(0x09),
        K::Num9 => plain(0x0A),
        K::Num0 => plain(0x0B),
        K::Minus => plain(0x0C),
        K::Equals | K::Plus => plain(0x0D),
        K::Backspace => plain(0x0E),
        K::Tab => plain(0x0F),
        K::Q => plain(0x10),
        K::W => plain(0x11),
        K::E => plain(0x12),
        K::R => plain(0x13),
        K::T => plain(0x14),
        K::Y => plain(0x15),
        K::U => plain(0x16),
        K::I => plain(0x17),
        K::O => plain(0x18),
        K::P => plain(0x19),
        K::OpenBracket => plain(0x1A),
        K::CloseBracket => plain(0x1B),
        K::Enter => plain(0x1C),
        K::A => plain(0x1E),
        K::S => plain(0x1F),
        K::D => plain(0x20),
        K::F => plain(0x21),
        K::G => plain(0x22),
        K::H => plain(0x23),
        K::J => plain(0x24),
        K::K => plain(0x25),
        K::L => plain(0x26),
        K::Semicolon => plain(0x27),
        K::Quote => plain(0x28),
        K::Backtick => plain(0x29),
        K::Backslash => plain(0x2B),
        K::Z => plain(0x2C),
        K::X => plain(0x2D),
        K::C => plain(0x2E),
        K::V => plain(0x2F),
        K::B => plain(0x30),
        K::N => plain(0x31),
        K::M => plain(0x32),
        K::Comma => plain(0x33),
        K::Period => plain(0x34),
        K::Slash => plain(0x35),
        K::Space => plain(0x39),
        K::F1 => plain(0x3B),
        K::F2 => plain(0x3C),
        K::F3 => plain(0x3D),
        K::F4 => plain(0x3E),
        K::F5 => plain(0x3F),
        K::F6 => plain(0x40),
        K::F7 => plain(0x41),
        K::F8 => plain(0x42),
        K::F9 => plain(0x43),
        K::F10 => plain(0x44),
        K::F12 => plain(0x58),
        K::Home => ext(0x47),
        K::ArrowUp => ext(0x48),
        K::PageUp => ext(0x49),
        K::ArrowLeft => ext(0x4B),
        K::ArrowRight => ext(0x4D),
        K::End => ext(0x4F),
        K::ArrowDown => ext(0x50),
        K::PageDown => ext(0x51),
        K::Insert => ext(0x52),
        K::Delete => ext(0x53),
        _ => None,
    }
}

// ---------- input translation ----------

fn translate(ev: &egui::Event, rect: Rect, last_pos: &mut Option<(u16, u16)>, wheel: &mut Vec2) -> Vec<InputEvent> {
    let norm = |p: Pos2| -> (u16, u16) {
        let x = ((p.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
        let y = ((p.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
        ((x * 65535.0) as u16, (y * 65535.0) as u16)
    };
    let mut out = Vec::new();
    let mut move_to = |p: Pos2, out: &mut Vec<InputEvent>| {
        let n = norm(p);
        if *last_pos != Some(n) {
            *last_pos = Some(n);
            out.push(InputEvent::MouseMove { x: n.0, y: n.1 });
        }
    };
    match ev {
        egui::Event::PointerMoved(p) if rect.contains(*p) => move_to(*p, &mut out),
        egui::Event::PointerButton { pos, button, pressed, .. } if rect.contains(*pos) || !pressed => {
            let button = match button {
                egui::PointerButton::Primary => MouseButton::Left,
                egui::PointerButton::Secondary => MouseButton::Right,
                egui::PointerButton::Middle => MouseButton::Middle,
                _ => return out,
            };
            if rect.contains(*pos) {
                move_to(*pos, &mut out);
            }
            out.push(InputEvent::MouseButton { button, down: *pressed });
        }
        egui::Event::MouseWheel { unit, delta, .. } => {
            let lines = match unit {
                egui::MouseWheelUnit::Line => *delta,
                egui::MouseWheelUnit::Point => *delta / 40.0,
                egui::MouseWheelUnit::Page => *delta * 3.0,
            };
            *wheel += lines;
            let (dx, dy) = (wheel.x.trunc(), wheel.y.trunc());
            *wheel -= Vec2::new(dx, dy);
            // egui: positive = content moves right/down; enigo: positive = scroll right/down.
            if dx != 0.0 || dy != 0.0 {
                out.push(InputEvent::Wheel { dx: -dx as i32, dy: -dy as i32 });
            }
        }
        egui::Event::Text(t) => out.push(InputEvent::Text(t.clone())),
        egui::Event::Paste(t) => out.push(InputEvent::Text(t.clone())),
        egui::Event::Copy | egui::Event::Cut => {
            let c = if matches!(ev, egui::Event::Copy) { 'c' } else { 'x' };
            let mods = Modifiers { ctrl: true, ..Default::default() };
            out.push(InputEvent::Key { key: KeyCode::Char(c), down: true, mods });
            out.push(InputEvent::Key { key: KeyCode::Char(c), down: false, mods });
        }
        egui::Event::Key { key, pressed, modifiers, .. } => {
            let mods = Modifiers {
                ctrl: modifiers.ctrl,
                shift: modifiers.shift,
                alt: modifiers.alt,
                meta: false,
            };
            if let Some(code) = map_key(*key, mods) {
                out.push(InputEvent::Key { key: code, down: *pressed, mods });
            }
        }
        _ => {}
    }
    out
}

/// Special keys always go as key events; printable keys only with Ctrl/Alt (plain typing arrives as `Text`).
fn map_key(key: egui::Key, mods: Modifiers) -> Option<KeyCode> {
    use egui::Key as K;
    Some(match key {
        K::Enter => KeyCode::Enter,
        K::Tab => KeyCode::Tab,
        K::Backspace => KeyCode::Backspace,
        K::Escape => KeyCode::Escape,
        K::Delete => KeyCode::Delete,
        K::Insert => KeyCode::Insert,
        K::Home => KeyCode::Home,
        K::End => KeyCode::End,
        K::PageUp => KeyCode::PageUp,
        K::PageDown => KeyCode::PageDown,
        K::ArrowUp => KeyCode::Up,
        K::ArrowDown => KeyCode::Down,
        K::ArrowLeft => KeyCode::Left,
        K::ArrowRight => KeyCode::Right,
        K::F1 => KeyCode::F(1),
        K::F2 => KeyCode::F(2),
        K::F3 => KeyCode::F(3),
        K::F4 => KeyCode::F(4),
        K::F5 => KeyCode::F(5),
        K::F6 => KeyCode::F(6),
        K::F7 => KeyCode::F(7),
        K::F8 => KeyCode::F(8),
        K::F9 => KeyCode::F(9),
        K::F10 => KeyCode::F(10),
        K::F12 => KeyCode::F(12),
        K::Space if mods.ctrl || mods.alt => KeyCode::Space,
        other if mods.ctrl || mods.alt => {
            let name = other.name();
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => KeyCode::Char(c),
                _ => return None,
            }
        }
        _ => return None,
    })
}
