//! Desktop GUI (eframe/egui): computers list with online status, session window with drawing tools,
//! "this PC" panel (access, ticket, service, settings, journal).

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use eframe::egui::{self, Color32, Pos2, Rect, Sense, TextureHandle, TextureOptions, Vec2};
use tokio::{runtime::Runtime, sync::mpsc as tmpsc, task::JoinHandle};

use crate::{
    autostart,
    client::{self, gamepad, viewer},
    config::{self, HostConfig, JournalEntry, SavedHost},
    host::Host,
    media::annotate::{self, DrawEvent, Ink},
    proto::{ClientMsg, InputEvent, KeyCode, Modifiers, MouseButton, Profile},
    ticket::Ticket,
    update::{self, Version},
    yuv_render::{self, PendingFrame},
};

pub fn run(rt: Runtime, initial: Option<(Ticket, Profile)>, minimized: bool) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Panda Remote")
            .with_inner_size([820.0, 560.0])
            .with_min_inner_size([620.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Panda Remote",
        options,
        Box::new(move |cc| {
            let gpu_video = yuv_render::init(cc.wgpu_render_state.as_ref());
            Ok(Box::new(App::new(cc.egui_ctx.clone(), rt, initial, gpu_video, minimized)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

enum UiEvent {
    HostStarted(Result<Host>),
    HostOnline,
    Connected {
        ticket: String,
    },
    DesktopEnded(Option<String>),
    /// Elevated service install/uninstall finished: (installing, result).
    ServiceDone(bool, Result<(), String>),
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
    /// The "add computer" form is open.
    adding: bool,
    /// Host being renamed: (index, edited name).
    renaming: Option<(usize, String)>,

    this_pc: ThisPc,
    desktop: Option<DesktopView>,
    /// NV12 frames are drawn by a wgpu shader; otherwise converted to RGBA on the CPU.
    gpu_video: bool,
    settings: config::Settings,
    /// Online state of saved hosts, by host id; refreshed in the background.
    presence: Arc<Mutex<HashMap<iroh::EndpointId, Presence>>>,
    /// Hosts the background checker pings (follows `hosts`).
    ping_targets: Arc<Mutex<Vec<iroh::EndpointId>>>,
    update: Arc<Mutex<UpdateState>>,
    /// Wakes the update checker early ("check now").
    update_now: Arc<tokio::sync::Notify>,
    /// Our executable's modification time at start: when the service replaces it, we restart too.
    exe_mtime: Option<std::time::SystemTime>,
    exe_checked: Instant,
}

/// Where the self-update stands (shown on "this PC", acted on when no session is open).
#[derive(Clone)]
enum UpdateState {
    Checking,
    UpToDate,
    /// Downloaded and verified, waiting to be installed.
    Ready(Version, std::path::PathBuf),
    /// Newer release exists, but this copy lives where only the service may replace it.
    ByService(Version),
    Failed(String),
}

#[derive(Clone)]
enum Presence {
    Online(String),
    Offline,
}

/// How often saved hosts are checked for being online.
const PRESENCE_INTERVAL: Duration = Duration::from_secs(20);

struct ThisPc {
    host: Option<Host>,
    starting: bool,
    online: bool,
    show_ticket: bool,
    confirm_rotate: bool,
    journal: Vec<JournalEntry>,
    journal_at: Option<Instant>,
    /// An elevated install/uninstall is running.
    service_busy: bool,
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
    /// Physical-key keyboard used while the host shows its lock screen.
    secure_keys: ScancodeKeyboard,
    tool: Tool,
    /// Pen colour (index into `annotate::COLORS`).
    color: usize,
    /// Local copy of what is drawn on the remote screen.
    ink: Ink,
    stroke_seq: u32,
    /// Stroke being drawn while the button is held.
    drawing: Option<u32>,
}

/// Game-profile input: captured pointer, raw mouse deltas, keys as scancodes, gamepad forwarding.
struct GameInput {
    captured: bool,
    keys: ScancodeKeyboard,
    motion: Vec2,
    pad_stop: Arc<AtomicBool>,
}

/// Keyboard forwarded as physical keys (scancodes): in the game profile, and while the host shows its
/// lock screen, where a typed password must arrive as real key presses in the lock screen's layout.
#[derive(Default)]
struct ScancodeKeyboard {
    mods: egui::Modifiers,
    pressed: HashSet<(u16, bool)>,
}

impl ScancodeKeyboard {
    /// egui reports modifiers as state, not key events: diff them into scancodes.
    fn sync_modifiers(&mut self, now: egui::Modifiers, send: &impl Fn(InputEvent)) {
        for (was, is, code) in [
            (self.mods.ctrl, now.ctrl, SC_LCTRL),
            (self.mods.shift, now.shift, SC_LSHIFT),
            (self.mods.alt, now.alt, SC_LALT),
        ] {
            if was != is {
                self.set((code, false), is, send);
            }
        }
        self.mods = now;
    }

    fn key(&mut self, key: egui::Key, pressed: bool, send: &impl Fn(InputEvent)) {
        if let Some(code) = scancode(key) {
            self.set(code, pressed, send);
        }
    }

    fn set(&mut self, (code, extended): (u16, bool), down: bool, send: &impl Fn(InputEvent)) {
        if down {
            self.pressed.insert((code, extended));
        } else {
            self.pressed.remove(&(code, extended));
        }
        send(InputEvent::Scancode { code, extended, down });
    }

    fn release_all(&mut self, send: &impl Fn(InputEvent)) {
        for (code, extended) in self.pressed.drain() {
            send(InputEvent::Scancode { code, extended, down: false });
        }
        self.mods = egui::Modifiers::default();
    }
}

impl Drop for GameInput {
    fn drop(&mut self) {
        self.pad_stop.store(true, Ordering::Relaxed);
    }
}

impl App {
    fn new(
        ctx: egui::Context,
        rt: Runtime,
        initial: Option<(Ticket, Profile)>,
        gpu_video: bool,
        minimized: bool,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        apply_style(&ctx);
        if minimized {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        let settings = config::load_settings();
        if settings.autostart {
            // Re-register on every start: keeps the entry pointing at the current program location.
            let _ = autostart::set(true);
        }
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
            adding: false,
            renaming: None,
            this_pc: ThisPc {
                host: None,
                starting: false,
                online: false,
                show_ticket: false,
                confirm_rotate: false,
                service_busy: false,
                journal: Vec::new(),
                journal_at: None,
            },
            desktop: None,
            gpu_video,
            settings,
            presence: Arc::default(),
            ping_targets: Arc::default(),
            update: Arc::new(Mutex::new(UpdateState::Checking)),
            update_now: Arc::default(),
            exe_mtime: exe_mtime(),
            exe_checked: Instant::now(),
        };
        app.spawn_update_checker();
        *app.ping_targets.lock().unwrap() = app.host_ids();
        app.spawn_presence_checker();
        if app.settings.host_enabled && !config::service_installed() {
            app.start_host();
        }
        if let Some((t, profile)) = initial {
            app.start_session("Удалённый ПК".into(), t.encode(), profile);
        }
        app
    }

    /// Looks for a signed release at start and every 6 hours (or when asked) and downloads it.
    fn spawn_update_checker(&self) {
        let (state, wake, ctx) = (self.update.clone(), self.update_now.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            loop {
                *state.lock().unwrap() = UpdateState::Checking;
                ctx.request_repaint();
                let next = match update::newer_release().await {
                    Ok(None) => UpdateState::UpToDate,
                    Ok(Some(rel)) if !update::can_replace_self() => UpdateState::ByService(rel.version),
                    Ok(Some(rel)) => match update::download(&rel, &update::staging_dir(false)).await {
                        Ok(path) => UpdateState::Ready(rel.version, path),
                        Err(e) => UpdateState::Failed(format!("{e:#}")),
                    },
                    Err(e) => UpdateState::Failed(format!("{e:#}")),
                };
                *state.lock().unwrap() = next;
                ctx.request_repaint();
                tokio::select! {
                    _ = tokio::time::sleep(update::CHECK_INTERVAL) => {}
                    _ = wake.notified() => {}
                }
            }
        });
    }

    /// Installs a downloaded update and restarts into it — only between sessions, never mid-session.
    fn maybe_apply_update(&mut self, force: bool) {
        if self.desktop.is_some() {
            return;
        }
        let ready = match &*self.update.lock().unwrap() {
            UpdateState::Ready(v, path) => Some((*v, path.clone())),
            _ => None,
        };
        if let Some((version, path)) = ready {
            let result =
                std::env::current_exe().map_err(anyhow::Error::from).and_then(|exe| update::install(&path, &exe));
            match result.and_then(|()| update::relaunch(&[])) {
                Ok(()) => {
                    tracing::info!(%version, "restarting into the update");
                    std::process::exit(0);
                }
                Err(e) => *self.update.lock().unwrap() = UpdateState::Failed(format!("{e:#}")),
            }
        }
        // The service updates the installed copy we may be running from: follow it.
        if (force || self.exe_checked.elapsed() > Duration::from_secs(60)) && self.exe_mtime.is_some() {
            self.exe_checked = Instant::now();
            if exe_mtime() != self.exe_mtime && update::relaunch(&[]).is_ok() {
                std::process::exit(0);
            }
        }
    }

    fn host_ids(&self) -> Vec<iroh::EndpointId> {
        self.hosts.iter().filter_map(|h| Ticket::decode(&h.ticket).ok()).map(|t| t.host_id).collect()
    }

    /// Pings every saved host in the background so the list shows which ones are online.
    fn spawn_presence_checker(&self) {
        let (targets, presence, ctx) = (self.ping_targets.clone(), self.presence.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            loop {
                let ids = targets.lock().unwrap().clone();
                let mut checks = tokio::task::JoinSet::new();
                for id in ids {
                    checks.spawn(async move { (id, client::ping(id).await) });
                }
                while let Some(Ok((id, result))) = checks.join_next().await {
                    let state = match result {
                        Ok(route) => Presence::Online(route),
                        Err(_) => Presence::Offline,
                    };
                    presence.lock().unwrap().insert(id, state);
                    ctx.request_repaint();
                }
                tokio::time::sleep(PRESENCE_INTERVAL).await;
            }
        });
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
                UiEvent::ServiceDone(installing, result) => {
                    self.this_pc.service_busy = false;
                    self.message = Some(match (installing, result) {
                        (true, Ok(())) => (
                            false,
                            "Служба установлена и запущена: ПК доступен на экране блокировки и до входа в систему."
                                .into(),
                        ),
                        (false, Ok(())) => (false, "Служба удалена.".into()),
                        (_, Err(e)) => (true, format!("Служба: {e}")),
                    });
                }
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
            GameInput { captured: false, keys: ScancodeKeyboard::default(), motion: Vec2::ZERO, pad_stop }
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
            secure_keys: ScancodeKeyboard::default(),
            tool: Tool::Control,
            color: 0,
            ink: Ink::default(),
            stroke_seq: 0,
            drawing: None,
        });
    }

    fn start_host(&mut self) {
        self.set_host_enabled(true);
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

    /// Remembers whether the in-app host is on, so the next start (e.g. autostart) restores it.
    fn set_host_enabled(&mut self, on: bool) {
        if self.settings.host_enabled != on {
            self.settings.host_enabled = on;
            let _ = config::save_settings(&self.settings);
        }
    }

    fn stop_host(&mut self) {
        self.set_host_enabled(false);
        if let Some(host) = self.this_pc.host.take() {
            self.rt.spawn(host.shutdown());
        }
        self.this_pc.online = false;
    }

    // ---------- views ----------

    fn computers_tab(&mut self, ui: &mut egui::Ui) {
        *self.ping_targets.lock().unwrap() = self.host_ids();
        let presence = self.presence.lock().unwrap().clone();
        let state = |h: &SavedHost| Ticket::decode(&h.ticket).ok().and_then(|t| presence.get(&t.host_id).cloned());
        let online = self.hosts.iter().filter(|h| matches!(state(h), Some(Presence::Online(_)))).count();

        ui.horizontal(|ui| {
            ui.heading("Компьютеры");
            if !self.hosts.is_empty() {
                ui.label(egui::RichText::new(format!("{online} из {} в сети", self.hosts.len())).weak());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(primary_button("+ Добавить")).clicked() {
                    self.adding ^= true;
                }
            });
        });
        ui.add_space(4.0);

        if self.adding {
            self.add_form(ui);
            ui.add_space(6.0);
        }

        if self.hosts.is_empty() && !self.adding {
            card(ui, |ui| {
                ui.label(egui::RichText::new("Как подключиться к своему компьютеру").strong());
                ui.add_space(4.0);
                ui.label("1. На том компьютере откройте Panda → «Этот ПК» → «Разрешить доступ».");
                ui.label("2. Нажмите «Копировать билет» и перешлите его себе (это как пароль).");
                ui.label("3. Здесь нажмите «+ Добавить» и вставьте билет.");
            });
            return;
        }

        // Online computers first, otherwise in the saved order.
        let mut order: Vec<usize> = (0..self.hosts.len()).collect();
        order.sort_by_key(|&i| !matches!(state(&self.hosts[i]), Some(Presence::Online(_))));
        let mut action: Option<(usize, &'static str)> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for i in order {
                let h = &self.hosts[i];
                let st = state(h);
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (color, line) = match &st {
                            Some(Presence::Online(route)) => (GREEN, format!("в сети · {route}")),
                            Some(Presence::Offline) => (
                                Color32::GRAY,
                                h.last_connected
                                    .as_deref()
                                    .map(|t| format!("не в сети · был {}", t[..16.min(t.len())].replace('T', " ")))
                                    .unwrap_or_else(|| "не в сети".into()),
                            ),
                            None => (YELLOW, "проверка…".into()),
                        };
                        status_dot(ui, color);
                        ui.vertical(|ui| {
                            if let Some((idx, name)) = self.renaming.as_mut().filter(|(idx, _)| *idx == i) {
                                let resp = ui.add(egui::TextEdit::singleline(name).desired_width(200.0));
                                if resp.lost_focus() {
                                    action = Some((*idx, "rename_done"));
                                }
                                resp.request_focus();
                            } else {
                                ui.label(egui::RichText::new(&h.name).strong().size(15.0));
                            }
                            ui.label(egui::RichText::new(line).small().weak());
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.menu_button("⋯", |ui| {
                                if ui.button("✏ Переименовать").clicked() {
                                    action = Some((i, "rename"));
                                    ui.close();
                                }
                                if ui.button("📋 Копировать билет").clicked() {
                                    ui.ctx().copy_text(h.ticket.clone());
                                    ui.close();
                                }
                                if ui.button("🗑 Удалить").clicked() {
                                    action = Some((i, "delete"));
                                    ui.close();
                                }
                            });
                            if ui
                                .button("🎮")
                                .on_hover_text("Игровой режим: 60 к/с, захват мыши, геймпад, звук")
                                .clicked()
                            {
                                action = Some((i, "game"));
                            }
                            if ui.add(primary_button("Подключиться")).clicked() {
                                action = Some((i, "desktop"));
                            }
                        });
                    });
                });
            }
        });
        if let Some((i, what)) = action {
            let h = self.hosts[i].clone();
            match what {
                "desktop" => self.start_session(h.name, h.ticket, Profile::Desktop),
                "game" => self.start_session(h.name, h.ticket, Profile::Game),
                "rename" => self.renaming = Some((i, h.name)),
                "rename_done" => {
                    if let Some((_, name)) = self.renaming.take()
                        && !name.trim().is_empty()
                    {
                        self.hosts[i].name = name.trim().to_string();
                        let _ = config::save_hosts(&self.hosts);
                    }
                }
                _ => {
                    self.hosts.remove(i);
                    let _ = config::save_hosts(&self.hosts);
                }
            }
        }
    }

    fn add_form(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.label(egui::RichText::new("Новый компьютер").strong());
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_name)
                        .hint_text("Название, например «Дом»")
                        .desired_width(170.0),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_ticket)
                        .hint_text("Билет panda1…")
                        .desired_width(ui.available_width() - 170.0)
                        .password(true),
                );
                if ui.add(primary_button("Добавить")).clicked() {
                    match Ticket::decode(&self.new_ticket) {
                        Ok(t) => {
                            let name = if self.new_name.trim().is_empty() {
                                t.host_id.fmt_short().to_string()
                            } else {
                                self.new_name.trim().to_string()
                            };
                            self.hosts
                                .retain(|h| Ticket::decode(&h.ticket).map(|x| x.host_id != t.host_id).unwrap_or(true));
                            self.hosts.push(SavedHost { name, ticket: t.encode(), last_connected: None });
                            let _ = config::save_hosts(&self.hosts);
                            self.new_name.clear();
                            self.new_ticket.clear();
                            self.adding = false;
                        }
                        Err(_) => {
                            self.message = Some((true, "Это не похоже на билет Panda: он начинается с panda1".into()))
                        }
                    }
                }
                if ui.button("Отмена").clicked() {
                    self.adding = false;
                }
            });
        });
    }

    /// The ticket row: masked, show/copy.
    fn ticket_row(&mut self, ui: &mut egui::Ui, ticket: &str) {
        ui.horizontal(|ui| {
            let shown = if self.this_pc.show_ticket {
                ticket.to_string()
            } else {
                "••••••••••••••••".into()
            };
            ui.label(egui::RichText::new(shown).monospace());
            if ui.small_button(if self.this_pc.show_ticket { "Скрыть" } else { "Показать" }).clicked() {
                self.this_pc.show_ticket ^= true;
            }
            if ui.add(primary_button("📋 Копировать билет")).clicked() {
                ui.ctx().copy_text(ticket.to_string());
                self.message = Some((false, "Билет скопирован. Он работает как пароль — не публикуйте его.".into()));
            }
        });
    }

    fn this_pc_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Этот ПК");
        ui.add_space(4.0);
        // With the Windows service installed it is the host; the in-app host would fight it over the key.
        let service_mode = config::service_installed();

        card(ui, |ui| {
            if service_mode {
                self.service_status(ui);
            } else {
                let (text, color) = match (&self.this_pc.host, self.this_pc.starting, self.this_pc.online) {
                    (_, true, _) | (Some(_), _, false) => ("Включается…", YELLOW),
                    (Some(_), _, true) => ("Доступ открыт: к этому ПК можно подключиться откуда угодно", GREEN),
                    (None, _, _) => ("Доступ закрыт", Color32::GRAY),
                };
                ui.horizontal(|ui| {
                    status_dot(ui, color);
                    ui.label(egui::RichText::new(text).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.this_pc.host.is_none() {
                            if ui.add_enabled(!self.this_pc.starting, primary_button("Разрешить доступ")).clicked()
                            {
                                self.start_host();
                            }
                        } else if ui.button("Закрыть доступ").clicked() {
                            self.stop_host();
                        }
                    });
                });
            }
        });

        // Ticket: from the running in-app host, or from the service's files (admin-only).
        let ticket = if service_mode {
            config::host_key()
                .and_then(|k| Ok(Ticket { host_id: k.public(), secret: HostConfig::load_or_create()?.access_secret }))
                .ok()
                .map(|t| t.encode())
        } else {
            self.this_pc.host.as_ref().map(|h| h.ticket.encode())
        };
        if let Some(ticket) = ticket {
            card(ui, |ui| {
                ui.label(egui::RichText::new("Билет для подключения").strong());
                self.ticket_row(ui, &ticket);
                if !service_mode {
                    self.rotate_secret_row(ui);
                }
            });
        } else if service_mode {
            card(ui, |ui| {
                ui.label("Билет службы виден только администраторам: panda-rc ticket в терминале администратора.");
            });
        }

        card(ui, |ui| {
            ui.label(egui::RichText::new("Настройки").strong());
            if ui.checkbox(&mut self.settings.autostart, "Запускать Panda вместе с Windows (свёрнутой)").changed()
            {
                let _ = config::save_settings(&self.settings);
                if let Err(e) = autostart::set(self.settings.autostart) {
                    self.message = Some((true, format!("Автозапуск: {e:#}")));
                }
            }
            #[cfg(windows)]
            if !service_mode {
                ui.horizontal(|ui| {
                    ui.label("🔒 Доступ на экране блокировки и после перезагрузки:");
                    let label = if self.this_pc.service_busy {
                        "Установка…"
                    } else {
                        "Установить службу…"
                    };
                    if ui.add_enabled(!self.this_pc.service_busy, egui::Button::new(label)).clicked() {
                        self.service_action(true);
                    }
                });
            }
        });

        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("Panda {}", Version::current())).strong());
                let state = self.update.lock().unwrap().clone();
                match &state {
                    UpdateState::Checking => {
                        ui.spinner();
                        ui.label(egui::RichText::new("проверка обновлений…").weak());
                    }
                    UpdateState::UpToDate => {
                        ui.label(egui::RichText::new("последняя версия").weak());
                    }
                    UpdateState::Ready(v, _) => {
                        ui.label(egui::RichText::new(format!("загружена {v}")).color(GREEN));
                    }
                    UpdateState::ByService(v) => {
                        ui.label(egui::RichText::new(format!("вышла {v}: служба обновит программу сама")).weak());
                    }
                    UpdateState::Failed(e) => {
                        ui.label(egui::RichText::new("не удалось проверить обновления").color(YELLOW)).on_hover_text(e);
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if matches!(state, UpdateState::Ready(..)) {
                        if ui.add(primary_button("Обновить сейчас")).on_hover_text("Перезапустит Panda").clicked()
                        {
                            self.maybe_apply_update(true);
                        }
                    } else if ui
                        .add_enabled(!matches!(state, UpdateState::Checking), egui::Button::new("Проверить"))
                        .clicked()
                    {
                        self.update_now.notify_one();
                    }
                });
            });
        });

        egui::CollapsingHeader::new("Журнал подключений").default_open(false).show(ui, |ui| {
            if self.this_pc.journal_at.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
                self.this_pc.journal = config::read_journal(50);
                self.this_pc.journal_at = Some(Instant::now());
            }
            if self.this_pc.journal.is_empty() {
                ui.label(egui::RichText::new("Подключений ещё не было").weak());
            }
            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                egui::Grid::new("journal").striped(true).spacing([10.0, 3.0]).show(ui, |ui| {
                    for e in &self.this_pc.journal {
                        ui.label(egui::RichText::new(e.ts.replace('T', " ").trim_end_matches('Z')).small());
                        let ev = match e.event.as_str() {
                            "connected" => "✅ вход",
                            "disconnected" => "⏹ выход",
                            "auth_failed" => "⛔ отказ",
                            other => other,
                        };
                        ui.label(egui::RichText::new(ev).small());
                        ui.label(egui::RichText::new(&e.client_name).small());
                        ui.label(egui::RichText::new(&e.detail).small().weak());
                        ui.end_row();
                    }
                });
            });
        });
        ui.ctx().request_repaint_after(Duration::from_secs(2));
    }

    fn rotate_secret_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if !self.this_pc.confirm_rotate {
                if ui.small_button("Сменить билет…").on_hover_text("Если билет попал не в те руки").clicked()
                {
                    self.this_pc.confirm_rotate = true;
                }
            } else {
                ui.label(egui::RichText::new("Все выданные билеты перестанут работать.").small());
                if ui.small_button("Сменить").clicked() {
                    self.this_pc.confirm_rotate = false;
                    match HostConfig::load_or_create().and_then(|mut c| c.rotate_secret()) {
                        Ok(()) => {
                            self.stop_host();
                            self.start_host();
                            self.message = Some((false, "Билет обновлён — скопируйте новый.".into()));
                        }
                        Err(e) => self.message = Some((true, format!("{e:#}"))),
                    }
                }
                if ui.small_button("Отмена").clicked() {
                    self.this_pc.confirm_rotate = false;
                }
            }
        });
    }

    fn service_status(&mut self, ui: &mut egui::Ui) {
        #[cfg(windows)]
        let running = crate::service::status() == Some(windows_service::service::ServiceState::Running);
        #[cfg(not(windows))]
        let running = false;
        ui.horizontal(|ui| {
            if running {
                status_dot(ui, GREEN);
                ui.label(egui::RichText::new("Доступ открыт всегда (служба Windows)").strong()).on_hover_text(
                    "Работает и на экране блокировки, в окнах UAC и после перезагрузки до входа в систему",
                );
            } else {
                status_dot(ui, Color32::GRAY);
                ui.label(egui::RichText::new("Служба установлена, но не запущена").strong());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let busy = self.this_pc.service_busy;
                ui.menu_button("Служба…", |ui| {
                    // Reinstalling over the running service upgrades it to this version of the program.
                    if ui.add_enabled(!busy, egui::Button::new("Обновить до этой версии…")).clicked()
                    {
                        self.service_action(true);
                        ui.close();
                    }
                    if ui.add_enabled(!busy, egui::Button::new("Удалить службу…")).clicked() {
                        self.service_action(false);
                        ui.close();
                    }
                });
                if busy {
                    ui.spinner();
                }
            });
        });
        ui.ctx().request_repaint_after(Duration::from_secs(2));
    }

    /// Installs or removes the service with a UAC prompt, off the UI thread.
    fn service_action(&mut self, install: bool) {
        #[cfg(windows)]
        {
            if install {
                // The service takes over this PC's key; the in-app host must not compete with it.
                self.stop_host();
            }
            self.this_pc.service_busy = true;
            let send = self.send_event();
            std::thread::spawn(move || {
                let result = crate::service::run_elevated(if install { "install" } else { "uninstall" });
                send(UiEvent::ServiceDone(install, result.map_err(|e| format!("{e:#}"))));
            });
        }
        #[cfg(not(windows))]
        let _ = install;
    }

    fn session_toolbar(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(view) = self.desktop.as_mut() else { return false };
        let mut disconnect = false;
        egui::Panel::top("session_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                let icon = if view.profile == Profile::Game { "🎮" } else { "🖥" };
                let status = view.shared.status.lock().unwrap().clone();
                let connected = view.frame_size.is_some();
                status_dot(ui, if connected { GREEN } else { YELLOW });
                ui.label(egui::RichText::new(format!("{icon} {}", view.name)).strong()).on_hover_text(&status);
                ui.label(egui::RichText::new(short_status(&status)).small().weak()).on_hover_text(&status);

                ui.separator();
                for (tool, label, tip) in [
                    (Tool::Control, "🖱", "Управление удалённым ПК"),
                    (Tool::Pen, "✏", "Рисовать: линии видны на удалённом экране и исчезают через секунду"),
                    (Tool::Laser, "🔦", "Указка: красная точка на удалённом экране"),
                ] {
                    if ui.selectable_label(view.tool == tool, label).on_hover_text(tip).clicked() {
                        view.tool = tool;
                    }
                }
                if view.tool == Tool::Pen {
                    for (i, c) in annotate::COLORS.iter().enumerate() {
                        let color = Color32::from_rgb(c[0], c[1], c[2]);
                        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::click());
                        ui.painter().circle_filled(rect.center(), if view.color == i { 7.0 } else { 5.0 }, color);
                        if resp.clicked() {
                            view.color = i;
                        }
                    }
                }
                if view.tool != Tool::Control && ui.button("🧹").on_hover_text("Стереть всё").clicked() {
                    view.ink.apply(&DrawEvent::Clear, Instant::now());
                    let _ = view.out.send(ClientMsg::Draw(DrawEvent::Clear));
                }

                if view.shared.secure_desktop.load(Ordering::Relaxed) {
                    ui.separator();
                    ui.label(egui::RichText::new("🔒 экран блокировки").color(ACCENT)).on_hover_text(
                        "Клавиши передаются как физические — пароль вводится как на обычной клавиатуре. Сначала кликните в поле пароля.",
                    );
                }
                if let Some(notice) = view.shared.notice.lock().unwrap().as_deref() {
                    ui.label(egui::RichText::new("🎮 нет геймпада").color(YELLOW)).on_hover_text(notice);
                }
                if let Some(g) = &view.game {
                    let hint = if g.captured { "Ctrl+Alt+Shift+Q — отпустить мышь" } else { "клик по картинке — захват мыши" };
                    ui.label(egui::RichText::new(hint).small().weak());
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("✕ Отключиться").clicked() {
                        disconnect = true;
                    }
                    if ui.button("Во весь экран").on_hover_text("F11 — выйти").clicked() {
                        view.fullscreen = true;
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                    }
                });
            });
        });
        disconnect
    }

    fn desktop_view(&mut self, ui: &mut egui::Ui) {
        let gpu_video = self.gpu_video;
        let fullscreen = self.desktop.as_ref().is_some_and(|v| v.fullscreen);
        let disconnect = if fullscreen { false } else { self.session_toolbar(ui) };
        let Some(view) = self.desktop.as_mut() else {
            return;
        };

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
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.spinner();
                        ui.label(view.shared.status.lock().unwrap().clone());
                    })
                });
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
            let (events, mods, focused) = ui.input(|i| (i.events.clone(), i.modifiers, i.focused));

            // Drawing tools: the pointer draws instead of controlling the remote mouse.
            if view.tool != Tool::Control && view.game.as_ref().is_none_or(|g| !g.captured) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
                draw_input(view, &events, rect);
            } else {
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Default);
                }
                if let Some(g) = view.game.as_mut() {
                    if !g.captured && resp.clicked() {
                        g.captured = true;
                        g.keys.mods = egui::Modifiers::default();
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::CursorGrab(egui::CursorGrab::Locked));
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::CursorVisible(false));
                        return;
                    }
                    if g.captured {
                        let release = !focused
                            || game_input(g, &events, mods, &view.out, &mut view.wheel, &mut view.fullscreen, ui.ctx());
                        if release {
                            release_capture(g, &view.out, ui.ctx());
                        }
                        return;
                    }
                }
                remote_input(view, &events, mods, rect, ui.ctx());
            }
            paint_ink(ui.painter(), &mut view.ink, rect);
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
        self.maybe_apply_update(false);

        if self.desktop.is_some() {
            self.desktop_view(ui);
            return;
        }

        egui::Panel::top("tabs").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("🐼 Panda").strong().size(17.0));
                ui.add_space(12.0);
                ui.selectable_value(&mut self.tab, Tab::Hosts, "💻 Компьютеры");
                ui.selectable_value(&mut self.tab, Tab::ThisPc, "🏠 Этот ПК");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Where this PC stands, one glance from any tab.
                    let (text, color) = if config::service_installed() {
                        ("доступ открыт всегда", GREEN)
                    } else if self.this_pc.online {
                        ("доступ открыт", GREEN)
                    } else {
                        ("доступ закрыт", Color32::GRAY)
                    };
                    if ui.add(egui::Button::new(egui::RichText::new(text).small()).frame(false)).clicked() {
                        self.tab = Tab::ThisPc;
                    }
                    status_dot(ui, color);
                    ui.label(egui::RichText::new("Этот ПК:").small().weak());
                });
            });
            ui.add_space(2.0);
        });
        if let Some((is_err, text)) = self.message.clone() {
            egui::Panel::bottom("message").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(if is_err { RED } else { GREEN }, text);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✕").clicked() {
                            self.message = None;
                        }
                    });
                });
            });
        }
        egui::CentralPanel::default_margins().show(ui, |ui| match self.tab {
            Tab::Hosts => self.computers_tab(ui),
            Tab::ThisPc => self.this_pc_tab(ui),
        });
    }
}

fn exe_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(std::env::current_exe().ok()?).ok()?.modified().ok()
}

// ---------- look ----------

const GREEN: Color32 = Color32::from_rgb(52, 199, 89);
const YELLOW: Color32 = Color32::from_rgb(255, 204, 0);
const RED: Color32 = Color32::from_rgb(255, 99, 88);
const ACCENT: Color32 = Color32::from_rgb(10, 132, 255);

fn apply_style(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(10.0, 4.0);
    });
}

fn primary_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(text).color(Color32::WHITE)).fill(ACCENT)
}

fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).fill(ui.visuals().faint_bg_color).corner_radius(8.0).inner_margin(10.0).show(
        ui,
        |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        },
    );
    ui.add_space(4.0);
}

fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, color);
}

/// "P2P напрямую, 2 мс · 58 к/с · потери 0% · …" → "P2P · 2 мс · 58 к/с".
fn short_status(status: &str) -> String {
    let parts: Vec<&str> = status.split(" · ").collect();
    if parts.len() < 3 {
        return status.to_string();
    }
    let route = parts[0].replace("P2P напрямую", "P2P").replace("через relay", "relay").replace(", ", " · ");
    format!("{route} · {} · {}", parts[1], parts[2])
}

// ---------- drawing ----------

#[derive(PartialEq, Clone, Copy)]
enum Tool {
    Control,
    Pen,
    Laser,
}

fn norm_in(p: Pos2, rect: Rect) -> Option<(u16, u16)> {
    rect.contains(p).then(|| {
        let x = ((p.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
        let y = ((p.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
        ((x * 65535.0) as u16, (y * 65535.0) as u16)
    })
}

/// Turns pointer events into drawing events: applied locally at once and sent to the host overlay.
fn draw_input(view: &mut DesktopView, events: &[egui::Event], rect: Rect) {
    let now = Instant::now();
    let emit = |view: &mut DesktopView, ev: DrawEvent| {
        view.ink.apply(&ev, now);
        let _ = view.out.send(ClientMsg::Draw(ev));
    };
    for ev in events {
        match ev {
            egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, .. } => {
                if *pressed && view.tool == Tool::Pen && rect.contains(*pos) {
                    view.stroke_seq += 1;
                    view.drawing = Some(view.stroke_seq);
                    if let Some((x, y)) = norm_in(*pos, rect) {
                        let (stroke, color) = (view.stroke_seq, annotate::COLORS[view.color]);
                        emit(view, DrawEvent::Point { stroke, x, y, color });
                    }
                } else if !*pressed && let Some(stroke) = view.drawing.take() {
                    emit(view, DrawEvent::End { stroke });
                }
            }
            egui::Event::PointerMoved(pos) => match (view.tool, view.drawing) {
                (Tool::Pen, Some(stroke)) => {
                    if let Some((x, y)) = norm_in(*pos, rect) {
                        let color = annotate::COLORS[view.color];
                        emit(view, DrawEvent::Point { stroke, x, y, color });
                    }
                }
                (Tool::Laser, _) => {
                    if let Some((x, y)) = norm_in(*pos, rect) {
                        emit(view, DrawEvent::Laser { x, y });
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

/// Draws the local copy of the ink over the video and keeps animating while it fades.
fn paint_ink(painter: &egui::Painter, ink: &mut Ink, rect: Rect) {
    let now = Instant::now();
    if !ink.prune(now) {
        return;
    }
    let to_screen = |(x, y): (f32, f32)| Pos2::new(rect.min.x + x * rect.width(), rect.min.y + y * rect.height());
    let width = (annotate::STROKE_WIDTH * rect.height()).max(2.0);
    for s in &ink.strokes {
        let a = Ink::stroke_alpha(s, now);
        let color = Color32::from_rgba_unmultiplied(s.color[0], s.color[1], s.color[2], (a * 255.0) as u8);
        let points: Vec<Pos2> = s.points.iter().map(|&p| to_screen(p)).collect();
        if points.len() == 1 {
            painter.circle_filled(points[0], width / 2.0, color);
        } else {
            painter.add(egui::Shape::line(points, egui::Stroke::new(width, color)));
        }
    }
    for (a, b, alpha) in ink.laser_trail(now) {
        let color = Color32::from_rgba_unmultiplied(255, 40, 40, (alpha * 160.0) as u8);
        painter.line_segment([to_screen(a), to_screen(b)], egui::Stroke::new(width * 1.6, color));
    }
    if let Some((p, alpha)) = ink.laser_dot(now) {
        painter.circle_filled(
            to_screen(p),
            width * 4.0,
            Color32::from_rgba_unmultiplied(255, 40, 40, (alpha * 64.0) as u8),
        );
        painter.circle_filled(
            to_screen(p),
            width * 2.0,
            Color32::from_rgba_unmultiplied(255, 40, 40, (alpha * 255.0) as u8),
        );
    }
    painter.ctx().request_repaint();
}

/// Desktop-profile input: absolute mouse, text, keys (physical keys on the remote lock screen).
fn remote_input(
    view: &mut DesktopView,
    events: &[egui::Event],
    mods: egui::Modifiers,
    rect: Rect,
    ctx: &egui::Context,
) {
    let secure = view.shared.secure_desktop.load(Ordering::Relaxed);
    let out = &view.out;
    let send = |ev: InputEvent| {
        let _ = out.send(ClientMsg::Input(ev));
    };
    if secure {
        view.secure_keys.sync_modifiers(mods, &send);
    } else if !view.secure_keys.pressed.is_empty() {
        view.secure_keys.release_all(&send);
    }
    for ev in events {
        if let egui::Event::Key { key: egui::Key::F11, pressed: true, .. } = ev {
            view.fullscreen ^= true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(view.fullscreen));
            continue;
        }
        // Lock screen: keys go as physical presses; text and clipboard events would not register there.
        if secure {
            match ev {
                egui::Event::Key { key, physical_key, pressed, .. } => {
                    view.secure_keys.key(physical_key.unwrap_or(*key), *pressed, &send);
                    continue;
                }
                egui::Event::Text(_) | egui::Event::Paste(_) | egui::Event::Copy | egui::Event::Cut => continue,
                _ => {}
            }
        }
        for msg in translate(ev, rect, &mut view.last_pos, &mut view.wheel) {
            send(msg);
        }
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
    g.keys.sync_modifiers(mods, &send);

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
                g.keys.key(key, *pressed, &send);
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
    g.keys.release_all(&|ev| {
        let _ = out.send(ClientMsg::Input(ev));
    });
    g.captured = false;
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
            let mods = Modifiers { ctrl: modifiers.ctrl, shift: modifiers.shift, alt: modifiers.alt, meta: false };
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
