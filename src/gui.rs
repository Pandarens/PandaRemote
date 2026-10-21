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
    icons::{self, Icon},
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
    /// An elevated service operation finished.
    ServiceDone(ServiceOp, Result<(), String>),
    /// The host wants a PIN: none was given, or it refused the one given (the reason).
    PinNeeded(Option<String>),
}

/// What runs with administrator rights (UAC prompt) through `panda-rc service …`.
#[derive(Clone, Copy)]
enum ServiceOp {
    Install,
    Uninstall,
    /// Copy the service's ticket into this user's folder.
    SyncTicket,
    /// Set (true) or remove (false) the service's PIN.
    SetPin(bool),
}

/// The PIN dialog shown when a host asks for one; connecting again sends it.
struct PinPrompt {
    name: String,
    ticket: String,
    profile: Profile,
    monitor: Option<u8>,
    pin: String,
    error: Option<String>,
}

#[derive(PartialEq, Clone, Copy)]
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
    pin_prompt: Option<PinPrompt>,
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
    journal_open: bool,
    /// An elevated service operation is running.
    service_busy: bool,
    /// The PIN being typed in settings.
    pin_edit: Option<String>,
}

struct DesktopView {
    name: String,
    ticket: String,
    /// What this session connected with, to connect again on another monitor.
    pin: Option<String>,
    monitor: Option<u8>,
    /// Set by the toolbar: reconnect to this monitor.
    switch_monitor: Option<u8>,
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
                journal_open: false,
                pin_edit: None,
            },
            desktop: None,
            pin_prompt: None,
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
            app.start_session("Удалённый ПК".into(), t.encode(), profile, None, None);
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
                UiEvent::ServiceDone(op, result) => {
                    self.this_pc.service_busy = false;
                    self.message = Some(match (op, result) {
                        (ServiceOp::Install, Ok(())) => (
                            false,
                            "Служба установлена и запущена: ПК доступен на экране блокировки и до входа в систему."
                                .into(),
                        ),
                        (ServiceOp::Uninstall, Ok(())) => (false, "Служба удалена.".into()),
                        (ServiceOp::SyncTicket, Ok(())) => (false, "Билет службы теперь виден здесь.".into()),
                        (ServiceOp::SetPin(true), Ok(())) => {
                            (false, "PIN-код задан: без него к этому ПК не войти.".into())
                        }
                        (ServiceOp::SetPin(false), Ok(())) => (false, "PIN-код убран.".into()),
                        (_, Err(e)) => (true, format!("Служба: {e}")),
                    });
                }
                UiEvent::PinNeeded(error) => {
                    if let Some(d) = self.desktop.take() {
                        if d.fullscreen {
                            self.ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                        }
                        let (name, ticket, profile, monitor) = (d.name, d.ticket, d.profile, d.monitor);
                        self.pin_prompt = Some(PinPrompt { name, ticket, profile, monitor, pin: String::new(), error });
                    }
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

    fn start_session(
        &mut self,
        name: String,
        ticket_str: String,
        profile: Profile,
        pin: Option<String>,
        monitor: Option<u8>,
    ) {
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
        let (shared2, out_tx2, ticket_str2, pin2) = (shared.clone(), out_tx.clone(), ticket_str.clone(), pin.clone());
        let task = self.rt.spawn(async move {
            let res = async {
                let session = client::connect(&ticket, profile, pin2.as_deref(), monitor).await?;
                send(UiEvent::Connected { ticket: ticket_str2 });
                viewer::run(session, shared2, out_tx2, out_rx, repaint, true).await
            }
            .await;
            match res.as_ref().err().and_then(|e| e.downcast_ref::<client::PinError>()) {
                Some(pin) => send(UiEvent::PinNeeded(pin.0.clone())),
                None => send(UiEvent::DesktopEnded(res.err().map(|e| format!("{e:#}")))),
            }
        });
        let game = (profile == Profile::Game).then(|| {
            let pad_stop = Arc::new(AtomicBool::new(false));
            *shared.rumble.lock().unwrap() = Some(gamepad::spawn(out_tx.clone(), pad_stop.clone()));
            GameInput { captured: false, keys: ScancodeKeyboard::default(), motion: Vec2::ZERO, pad_stop }
        });
        self.message = None;
        self.desktop = Some(DesktopView {
            name,
            ticket: ticket_str,
            pin,
            monitor,
            switch_monitor: None,
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
            ui.label(semibold("Компьютеры", 18.0));
            if !self.hosts.is_empty() {
                ui.label(muted(format!("{online} из {} в сети", self.hosts.len())));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let kind = if self.adding { Kind::Secondary } else { Kind::Primary };
                if button(ui, Some(Icon::Plus), "Добавить", kind).clicked() {
                    self.adding ^= true;
                }
            });
        });
        ui.add_space(6.0);

        if self.adding {
            self.add_form(ui);
        }

        if self.hosts.is_empty() && !self.adding {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    icon_tile(ui, Icon::Devices, ACCENT_SOFT, ACCENT_TILE);
                    ui.vertical(|ui| {
                        ui.label(semibold("Как подключиться к своему компьютеру", 14.0));
                        ui.label(muted("Три шага, без регистрации и IP-адресов").small());
                    });
                });
                ui.add_space(6.0);
                for step in [
                    "1.  На том компьютере откройте Panda → «Этот ПК» → «Разрешить доступ».",
                    "2.  Скопируйте билет и перешлите его себе — он работает как пароль.",
                    "3.  Здесь нажмите «Добавить» и вставьте билет.",
                ] {
                    ui.label(egui::RichText::new(step).color(SOFT));
                }
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
                let is_online = matches!(st, Some(Presence::Online(_)));
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (color, line) = match &st {
                            Some(Presence::Online(route)) => (GREEN, format!("В сети · {route}")),
                            Some(Presence::Offline) => (
                                OFFLINE,
                                h.last_connected
                                    .as_deref()
                                    .map(|t| format!("Не в сети · был {}", t[..16.min(t.len())].replace('T', " ")))
                                    .unwrap_or_else(|| "Не в сети".into()),
                            ),
                            None => (YELLOW, "Проверка…".into()),
                        };
                        status_dot(ui, color);
                        ui.add_space(2.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            if let Some((idx, name)) = self.renaming.as_mut().filter(|(idx, _)| *idx == i) {
                                let resp = ui.add(
                                    egui::TextEdit::singleline(name)
                                        .desired_width(200.0)
                                        .margin(egui::Margin::symmetric(8, 4)),
                                );
                                if resp.lost_focus() {
                                    action = Some((*idx, "rename_done"));
                                }
                                resp.request_focus();
                            } else {
                                let name = semibold(&h.name, 14.0);
                                ui.label(if is_online { name } else { name.color(SOFT) });
                            }
                            ui.label(muted(line).small());
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let more = icon_button(ui, Icon::More, "Ещё", Kind::Secondary);
                            egui::Popup::menu(&more).show(|ui| {
                                ui.set_min_width(190.0);
                                if menu_item(ui, Icon::Rename, "Переименовать", false).clicked() {
                                    action = Some((i, "rename"));
                                    ui.close();
                                }
                                if menu_item(ui, Icon::Copy, "Копировать билет", false).clicked() {
                                    ui.ctx().copy_text(h.ticket.clone());
                                    ui.close();
                                }
                                if menu_item(ui, Icon::Trash, "Удалить", true).clicked() {
                                    action = Some((i, "delete"));
                                    ui.close();
                                }
                            });
                            if icon_button(
                                ui,
                                Icon::Gamepad,
                                "Игровой режим: 60 к/с, захват мыши, геймпад, звук",
                                Kind::Secondary,
                            )
                            .clicked()
                            {
                                action = Some((i, "game"));
                            }
                            let kind = if is_online { Kind::Primary } else { Kind::Secondary };
                            if button(ui, Some(Icon::Monitor), "Подключиться", kind).clicked() {
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
                "desktop" => self.start_session(h.name, h.ticket, Profile::Desktop, None, None),
                "game" => self.start_session(h.name, h.ticket, Profile::Game, None, None),
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
            ui.label(semibold("Новый компьютер", 14.0));
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_name)
                        .hint_text("Название, например «Дом»")
                        .desired_width(170.0)
                        .margin(egui::Margin::symmetric(10, 8)),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_ticket)
                        .hint_text("Билет panda1…")
                        .desired_width((ui.available_width() - 200.0).max(120.0))
                        .margin(egui::Margin::symmetric(10, 8))
                        .password(true),
                );
                if button(ui, None, "Добавить", Kind::Primary).clicked() {
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
                if button(ui, None, "Отмена", Kind::Ghost).clicked() {
                    self.adding = false;
                }
            });
        });
    }

    /// The ticket field: masked, show/copy.
    fn ticket_row(&mut self, ui: &mut egui::Ui, ticket: &str) {
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if button(ui, Some(Icon::Copy), "Копировать", Kind::Primary).clicked() {
                    ui.ctx().copy_text(ticket.to_string());
                    self.message =
                        Some((false, "Билет скопирован. Он работает как пароль — не публикуйте его.".into()));
                }
                let (eye, tip) = if self.this_pc.show_ticket {
                    (Icon::EyeOff, "Скрыть")
                } else {
                    (Icon::Eye, "Показать")
                };
                if icon_button(ui, eye, tip, Kind::Secondary).clicked() {
                    self.this_pc.show_ticket ^= true;
                }
                let (shown, color) = if self.this_pc.show_ticket {
                    (ticket.to_string(), TEXT)
                } else {
                    ("panda1 ••••••••••••••••••••••••".to_string(), MUTED)
                };
                egui::Frame::new()
                    .fill(BG)
                    .stroke(egui::Stroke::new(1.0, RAISED_BORDER))
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::symmetric(12, 7))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add(egui::Label::new(egui::RichText::new(shown).monospace().color(color)).truncate());
                        });
                    });
            });
        });
    }

    fn this_pc_tab(&mut self, ui: &mut egui::Ui) {
        // With the Windows service installed it is the host; the in-app host would fight it over the key.
        let service_mode = config::service_installed();

        card(ui, |ui| {
            if service_mode {
                self.service_status(ui);
            } else {
                let host_on = self.this_pc.host.is_some();
                let (fg, bg, title, sub) = match (host_on, self.this_pc.starting, self.this_pc.online) {
                    (_, true, _) | (true, _, false) => (YELLOW, YELLOW_TILE, "Включается…", "Подключение к сети Panda"),
                    (true, _, true) => (
                        GREEN,
                        GREEN_TILE,
                        "Доступ открыт",
                        "К этому ПК можно подключиться откуда угодно, пока Panda запущена",
                    ),
                    (false, _, _) => (MUTED, RAISED, "Доступ закрыт", "Откройте доступ, чтобы подключаться к этому ПК"),
                };
                ui.horizontal(|ui| {
                    icon_tile(ui, Icon::Power, fg, bg);
                    title_block(ui, title, sub);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if !host_on {
                            let enabled = !self.this_pc.starting;
                            if ui
                                .add_enabled_ui(enabled, |ui| button(ui, None, "Разрешить доступ", Kind::Primary))
                                .inner
                                .clicked()
                            {
                                self.start_host();
                            }
                        } else if button(ui, None, "Закрыть доступ", Kind::Secondary).clicked() {
                            self.stop_host();
                        }
                    });
                });
            }
        });

        // Ticket: from the running in-app host, else from this user's host files (the service runs on a copy).
        let user_host = config::user_host();
        let ticket = self.this_pc.host.as_ref().map(|h| h.ticket.encode()).or_else(|| {
            user_host.as_ref().map(|(id, cfg)| Ticket { host_id: *id, secret: cfg.access_secret }.encode())
        });
        if let Some(ticket) = ticket {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    icon(ui, Icon::Key, MUTED, 16.0);
                    ui.label(semibold("Билет для подключения", 13.5));
                    ui.label(muted("— работает как пароль").small());
                    if !service_mode {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            self.rotate_secret_row(ui);
                        });
                    }
                });
                ui.add_space(2.0);
                self.ticket_row(ui, &ticket);
            });
        } else if service_mode {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    icon(ui, Icon::Key, MUTED, 16.0);
                    ui.label(muted(
                        "Билет хранится у службы: покажите его здесь один раз (нужны права администратора).",
                    ));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let busy = self.this_pc.service_busy;
                        if ui
                            .add_enabled_ui(!busy, |ui| {
                                button(ui, Some(Icon::Shield), "Показать билет…", Kind::Secondary)
                            })
                            .inner
                            .clicked()
                        {
                            self.service_op(ServiceOp::SyncTicket);
                        }
                    });
                });
            });
        }

        card(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.horizontal(|ui| {
                ui.set_min_height(32.0);
                icon(ui, Icon::Power, MUTED, 16.0);
                ui.label("Запускать вместе с Windows");
                ui.label(muted("свёрнутой").small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if toggle(ui, &mut self.settings.autostart).changed() {
                        let _ = config::save_settings(&self.settings);
                        if let Err(e) = autostart::set(self.settings.autostart) {
                            self.message = Some((true, format!("Автозапуск: {e:#}")));
                        }
                    }
                });
            });
            ui.separator();
            let pin_on = user_host.as_ref().is_some_and(|(_, c)| c.pin.is_some());
            ui.horizontal(|ui| {
                ui.set_min_height(32.0);
                icon(ui, Icon::Pin, MUTED, 16.0);
                ui.label("PIN-код при подключении");
                ui.label(muted(if pin_on { "включён" } else { "выключен" }).small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let busy = self.this_pc.service_busy;
                    if let Some(pin) = self.this_pc.pin_edit.as_mut() {
                        // Not while another elevated step (e.g. a service update) is rewriting the service's files.
                        let ok = config::valid_pin(pin) && !busy;
                        let save =
                            ui.add_enabled_ui(ok, |ui| button(ui, None, "Сохранить", Kind::Primary)).inner.clicked();
                        let cancel = button(ui, None, "Отмена", Kind::Ghost).clicked();
                        let resp = ui.add(
                            egui::TextEdit::singleline(pin)
                                .password(true)
                                .hint_text("4–8 цифр")
                                .desired_width(110.0)
                                .margin(egui::Margin::symmetric(10, 7)),
                        );
                        pin.retain(|c| c.is_ascii_digit());
                        pin.truncate(8);
                        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if (save || enter) && ok {
                            let pin = self.this_pc.pin_edit.take();
                            self.save_pin(pin, service_mode);
                        } else if cancel {
                            self.this_pc.pin_edit = None;
                        }
                    } else if ui
                        .add_enabled_ui(!busy, |ui| {
                            button(ui, None, if pin_on { "Изменить…" } else { "Задать…" }, Kind::Secondary)
                        })
                        .inner
                        .clicked()
                    {
                        self.this_pc.pin_edit = Some(String::new());
                    } else if pin_on
                        && ui.add_enabled_ui(!busy, |ui| button(ui, None, "Выключить", Kind::Ghost)).inner.clicked()
                    {
                        self.save_pin(None, service_mode);
                    }
                });
            });
            #[cfg(windows)]
            if !service_mode {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.set_min_height(32.0);
                    icon(ui, Icon::Lock, MUTED, 16.0);
                    ui.label("Доступ на экране блокировки и после перезагрузки");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let busy = self.this_pc.service_busy;
                        let label = if busy { "Установка…" } else { "Установить службу…" };
                        if ui
                            .add_enabled_ui(!busy, |ui| button(ui, Some(Icon::Shield), label, Kind::Secondary))
                            .inner
                            .clicked()
                        {
                            self.service_op(ServiceOp::Install);
                        }
                        if busy {
                            ui.spinner();
                        }
                    });
                });
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.set_min_height(32.0);
                icon(ui, Icon::Refresh, MUTED, 16.0);
                ui.label(format!("Panda {}", Version::current()));
                let state = self.update.lock().unwrap().clone();
                match &state {
                    UpdateState::Checking => {
                        ui.spinner();
                        ui.label(muted("проверка обновлений…"));
                    }
                    UpdateState::UpToDate => {
                        ui.label(muted("· последняя версия"));
                    }
                    UpdateState::Ready(v, _) => {
                        ui.label(egui::RichText::new(format!("· загружена {v}")).color(GREEN));
                    }
                    UpdateState::ByService(v) => {
                        ui.label(muted(format!("· вышла {v}: служба обновит программу сама")));
                    }
                    UpdateState::Failed(e) => {
                        ui.label(egui::RichText::new("· не удалось проверить обновления").color(YELLOW))
                            .on_hover_text(e);
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if matches!(state, UpdateState::Ready(..)) {
                        if button(ui, None, "Обновить сейчас", Kind::Primary)
                            .on_hover_text("Перезапустит Panda")
                            .clicked()
                        {
                            self.maybe_apply_update(true);
                        }
                    } else if ui
                        .add_enabled_ui(!matches!(state, UpdateState::Checking), |ui| {
                            button(ui, None, "Проверить", Kind::Secondary)
                        })
                        .inner
                        .clicked()
                    {
                        self.update_now.notify_one();
                    }
                });
            });
        });

        let chevron = if self.this_pc.journal_open { Icon::ChevronDown } else { Icon::Chevron };
        if button(ui, Some(chevron), "Журнал подключений", Kind::Ghost).clicked() {
            self.this_pc.journal_open ^= true;
        }
        if self.this_pc.journal_open {
            if self.this_pc.journal_at.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
                self.this_pc.journal = config::read_journal(50);
                self.this_pc.journal_at = Some(Instant::now());
            }
            card(ui, |ui| {
                if self.this_pc.journal.is_empty() {
                    ui.label(muted("Подключений ещё не было"));
                }
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    egui::Grid::new("journal").spacing([14.0, 5.0]).show(ui, |ui| {
                        for e in &self.this_pc.journal {
                            ui.label(muted(e.ts.replace('T', " ").trim_end_matches('Z').to_string()).small());
                            let (ev, color) = match e.event.as_str() {
                                "connected" => ("вход", GREEN),
                                "disconnected" => ("выход", MUTED),
                                "auth_failed" => ("отказ", RED),
                                other => (other, MUTED),
                            };
                            ui.label(egui::RichText::new(ev).small().color(color));
                            ui.label(egui::RichText::new(&e.client_name).small());
                            ui.label(muted(&e.detail).small());
                            ui.end_row();
                        }
                    });
                });
            });
        }
        ui.ctx().request_repaint_after(Duration::from_secs(2));
    }

    /// "Change ticket" with an inline confirmation (laid out right to left).
    fn rotate_secret_row(&mut self, ui: &mut egui::Ui) {
        if !self.this_pc.confirm_rotate {
            if small_button(ui, None, "Сменить билет…", Kind::Ghost)
                .on_hover_text("Если билет попал не в те руки")
                .clicked()
            {
                self.this_pc.confirm_rotate = true;
            }
            return;
        }
        if small_button(ui, None, "Отмена", Kind::Ghost).clicked() {
            self.this_pc.confirm_rotate = false;
        }
        if small_button(ui, None, "Сменить", Kind::Danger).clicked() {
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
        ui.label(muted("Старые билеты перестанут работать").small());
    }

    fn service_status(&mut self, ui: &mut egui::Ui) {
        #[cfg(windows)]
        let running = crate::service::status() == Some(windows_service::service::ServiceState::Running);
        #[cfg(not(windows))]
        let running = false;
        ui.horizontal(|ui| {
            if running {
                icon_tile(ui, Icon::Shield, GREEN, GREEN_TILE);
                title_block(
                    ui,
                    "Доступ открыт всегда",
                    "Служба Windows: работает на экране блокировки, в окнах UAC и после перезагрузки",
                );
            } else {
                icon_tile(ui, Icon::Shield, MUTED, RAISED);
                title_block(ui, "Служба установлена, но не запущена", "Обновите её до этой версии или переустановите");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let busy = self.this_pc.service_busy;
                let menu = button(ui, None, "Служба…", Kind::Secondary);
                egui::Popup::menu(&menu).show(|ui| {
                    ui.set_min_width(220.0);
                    // Reinstalling over the running service upgrades it to this version of the program.
                    if ui
                        .add_enabled_ui(!busy, |ui| menu_item(ui, Icon::Refresh, "Обновить до этой версии…", false))
                        .inner
                        .clicked()
                    {
                        self.service_op(ServiceOp::Install);
                        ui.close();
                    }
                    if ui
                        .add_enabled_ui(!busy, |ui| menu_item(ui, Icon::Trash, "Удалить службу…", true))
                        .inner
                        .clicked()
                    {
                        self.service_op(ServiceOp::Uninstall);
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

    /// Runs a service operation with a UAC prompt, off the UI thread.
    fn service_op(&mut self, op: ServiceOp) {
        #[cfg(windows)]
        {
            if let ServiceOp::Install = op {
                // The service takes over this PC's key; the in-app host must not compete with it.
                self.stop_host();
            }
            let action = match op {
                ServiceOp::Install => "install".to_string(),
                ServiceOp::Uninstall => "uninstall".to_string(),
                ServiceOp::SyncTicket => format!("sync-ticket --to \"{}\"", config::user_dir().display()),
                ServiceOp::SetPin(_) => {
                    format!("set-pin --pin-file \"{}\" --to \"{}\"", pin_file().display(), config::user_dir().display())
                }
            };
            self.this_pc.service_busy = true;
            let send = self.send_event();
            std::thread::spawn(move || {
                let result = crate::service::run_elevated(&action);
                send(UiEvent::ServiceDone(op, result.map_err(|e| format!("{e:#}"))));
            });
        }
        #[cfg(not(windows))]
        let _ = op;
    }

    /// Sets or removes the PIN: directly for the in-app host, through an elevated step for the service.
    fn save_pin(&mut self, pin: Option<String>, service_mode: bool) {
        if service_mode {
            // Handed over in a file in this user's private folder, not on a command line.
            match std::fs::write(pin_file(), pin.as_deref().unwrap_or("")) {
                Ok(()) => self.service_op(ServiceOp::SetPin(pin.is_some())),
                Err(e) => self.message = Some((true, format!("PIN-код: {e}"))),
            }
            return;
        }
        let result = HostConfig::load_or_create().and_then(|mut c| c.set_pin(pin.as_deref()));
        self.message = Some(match result {
            Ok(()) if pin.is_some() => (false, "PIN-код задан: без него к этому ПК не войти.".into()),
            Ok(()) => (false, "PIN-код убран.".into()),
            Err(e) => (true, format!("{e:#}")),
        });
    }

    /// Asks for the PIN of a host that wants one, then connects again with it.
    fn pin_dialog(&mut self, ctx: &egui::Context) {
        let Some(p) = self.pin_prompt.as_mut() else { return };
        let (mut submit, mut cancel) = (false, false);
        let frame = egui::Frame::new()
            .fill(SURFACE)
            .stroke(egui::Stroke::new(1.0, RAISED_BORDER))
            .corner_radius(12.0)
            .inner_margin(20.0);
        let modal = egui::Modal::new(egui::Id::unique("pin_prompt")).frame(frame).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.horizontal(|ui| {
                icon_tile(ui, Icon::Lock, ACCENT_SOFT, ACCENT_TILE);
                title_block(ui, "Введите PIN-код", &format!("«{}» защищён PIN-кодом", p.name));
            });
            ui.add_space(10.0);
            let edit = egui::TextEdit::singleline(&mut p.pin)
                .password(true)
                .hint_text("PIN-код")
                .desired_width(f32::INFINITY)
                .margin(egui::Margin::symmetric(12, 9))
                .font(egui::TextStyle::Heading);
            ui.add(edit).request_focus();
            p.pin.retain(|c| c.is_ascii_digit());
            p.pin.truncate(8);
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submit = true;
            }
            if let Some(err) = &p.error {
                ui.add_space(2.0);
                ui.label(egui::RichText::new(err).small().color(RED));
            }
            ui.add_space(10.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if button(ui, None, "Войти", Kind::Primary).clicked() {
                    submit = true;
                }
                if button(ui, None, "Отмена", Kind::Ghost).clicked() {
                    cancel = true;
                }
            });
        });
        if cancel || modal.should_close() {
            self.pin_prompt = None;
        } else if submit && !p.pin.is_empty() {
            let p = self.pin_prompt.take().unwrap();
            self.start_session(p.name, p.ticket, p.profile, Some(p.pin), p.monitor);
        }
    }

    fn session_toolbar(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(view) = self.desktop.as_mut() else { return false };
        let mut disconnect = false;
        let bar = egui::Frame::new().fill(TOOLBAR).inner_margin(egui::Margin::symmetric(10, 6));
        egui::Panel::top("session_bar").frame(bar).show(ui, |ui| {
            ui.horizontal(|ui| {
                let status = view.shared.status.lock().unwrap().clone();
                let connected = view.frame_size.is_some();
                status_dot(ui, if connected { GREEN } else { YELLOW });
                ui.label(semibold(&view.name, 13.5)).on_hover_text(&status);
                if view.profile == Profile::Game {
                    icon(ui, Icon::Gamepad, MUTED, 16.0);
                }
                ui.label(muted(short_status(&status)).small()).on_hover_text(&status);
                ui.add_space(6.0);

                egui::Frame::new()
                    .fill(BG)
                    .stroke(egui::Stroke::new(1.0, BORDER))
                    .corner_radius(8.0)
                    .inner_margin(2.0)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        for (tool, ic, tip) in [
                            (Tool::Control, Icon::Cursor, "Управление удалённым ПК"),
                            (Tool::Pen, Icon::Pen, "Перо: линии видны на удалённом экране и исчезают через секунду"),
                            (Tool::Laser, Icon::Laser, "Указка: красная точка на удалённом экране"),
                            (Tool::View, Icon::Eye, "Только просмотр: экран виден, мышь, клавиатура и геймпад не передаются"),
                        ] {
                            let kind = if view.tool == tool { Kind::Active } else { Kind::Ghost };
                            if button_sized(ui, Some(ic), "", kind, 26.0).on_hover_text(tip).clicked() {
                                set_tool(view, tool, ui.ctx());
                            }
                        }
                    });
                if view.tool == Tool::Pen {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for (i, c) in annotate::COLORS.iter().enumerate() {
                        let color = Color32::from_rgb(c[0], c[1], c[2]);
                        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::click());
                        let r = if resp.hovered() { 7.5 } else { 7.0 };
                        ui.painter().circle_filled(rect.center(), r, color);
                        if view.color == i {
                            ui.painter().circle_stroke(rect.center(), 9.5, egui::Stroke::new(1.5, TEXT));
                        }
                        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            view.color = i;
                        }
                    }
                    ui.spacing_mut().item_spacing.x = 8.0;
                }
                if matches!(view.tool, Tool::Pen | Tool::Laser)
                    && button_sized(ui, Some(Icon::Eraser), "", Kind::Ghost, 28.0).on_hover_text("Стереть всё").clicked()
                {
                    view.ink.apply(&DrawEvent::Clear, Instant::now());
                    let _ = view.out.send(ClientMsg::Draw(DrawEvent::Clear));
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if button_sized(ui, Some(Icon::Close), "Отключиться", Kind::Danger, 28.0).clicked() {
                        disconnect = true;
                    }
                    if button_sized(ui, Some(Icon::Expand), "", Kind::Ghost, 28.0)
                        .on_hover_text("Во весь экран (F11 — выйти)")
                        .clicked()
                    {
                        view.fullscreen = true;
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                    }
                    let (monitors, current) = view.shared.monitors.lock().unwrap().clone();
                    if monitors.len() > 1 {
                        let label = format!("{}", current + 1);
                        let menu = button_sized(ui, Some(Icon::Monitor), &label, Kind::Ghost, 28.0)
                            .on_hover_text("Какой монитор показывать");
                        egui::Popup::menu(&menu).show(|ui| {
                            ui.set_min_width(220.0);
                            for (i, name) in monitors.iter().enumerate() {
                                let icon = if i == current as usize { Icon::Check } else { Icon::Monitor };
                                if menu_item(ui, icon, name, false).clicked() {
                                    if i != current as usize {
                                        view.switch_monitor = Some(i as u8);
                                    }
                                    ui.close();
                                }
                            }
                        });
                    }
                    if view.tool != Tool::View
                        && button_sized(ui, None, "Ctrl+Alt+Del", Kind::Ghost, 28.0)
                            .on_hover_text("Нажать Ctrl+Alt+Del на удалённом ПК (нужна служба Windows на нём)")
                            .clicked()
                    {
                        let _ = view.out.send(ClientMsg::SecureAttention);
                    }
                    if view.shared.secure_desktop.load(Ordering::Relaxed) {
                        chip(ui, Icon::Lock, "Экран блокировки", YELLOW, YELLOW_TILE).on_hover_text(
                            "Клавиши передаются как физические — пароль вводится как на обычной клавиатуре. Сначала кликните в поле пароля.",
                        );
                    }
                    if let Some(notice) = view.shared.notice.lock().unwrap().as_deref() {
                        chip(ui, Icon::Gamepad, "Нет геймпада", YELLOW, YELLOW_TILE).on_hover_text(notice);
                    }
                    if let Some(g) = &view.game {
                        let hint = if g.captured { "Ctrl+Alt+Shift+Q — отпустить мышь" } else { "Клик по картинке — захват мыши" };
                        ui.label(muted(hint).small());
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

            if view.tool == Tool::View {
                // View only: the pointer and keys stay here.
            } else if view.tool != Tool::Control && view.game.as_ref().is_none_or(|g| !g.captured) {
                // Drawing tools: the pointer draws instead of controlling the remote mouse.
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
        } else if let Some(monitor) = self.desktop.as_ref().and_then(|d| d.switch_monitor)
            && let Some(mut d) = self.desktop.take()
        {
            // Another monitor means another resolution: connect again (with the same PIN, if any).
            if let Some(g) = d.game.as_mut() {
                release_capture(g, &d.out, ui.ctx());
            }
            d.task.abort();
            let fullscreen = d.fullscreen;
            self.start_session(d.name, d.ticket, d.profile, d.pin, Some(monitor));
            if let Some(v) = self.desktop.as_mut() {
                v.fullscreen = fullscreen;
            }
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

        let header = egui::Frame::new().fill(BG).inner_margin(egui::Margin::symmetric(16, 8));
        egui::Panel::top("tabs").frame(header).show(ui, |ui| {
            ui.horizontal(|ui| {
                icon(ui, Icon::Panda, TEXT, 22.0);
                ui.label(semibold("Panda", 15.0));
                ui.add_space(14.0);
                segmented(
                    ui,
                    &mut self.tab,
                    &[(Tab::Hosts, Icon::Devices, "Компьютеры"), (Tab::ThisPc, Icon::Laptop, "Этот ПК")],
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Where this PC stands, one glance from any tab.
                    let (text, color) = if config::service_installed() {
                        ("Доступ открыт всегда", GREEN)
                    } else if self.this_pc.online {
                        ("Доступ открыт", GREEN)
                    } else {
                        ("Доступ закрыт", OFFLINE)
                    };
                    let resp = ui.add(egui::Label::new(muted(text).small()).sense(Sense::click()));
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        self.tab = Tab::ThisPc;
                    }
                    status_dot(ui, color);
                });
            });
        });
        if let Some((is_err, text)) = self.message.clone() {
            let bar = egui::Frame::new().fill(SURFACE).inner_margin(egui::Margin::symmetric(16, 6));
            egui::Panel::bottom("message").frame(bar).show(ui, |ui| {
                ui.horizontal(|ui| {
                    status_dot(ui, if is_err { RED } else { GREEN });
                    ui.label(egui::RichText::new(text).color(if is_err { RED } else { TEXT }));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if button_sized(ui, Some(Icon::Close), "", Kind::Ghost, 24.0).clicked() {
                            self.message = None;
                        }
                    });
                });
            });
        }
        let body = egui::Frame::new().fill(BG).inner_margin(egui::Margin { left: 16, right: 16, top: 16, bottom: 10 });
        egui::CentralPanel::no_frame().frame(body).show(ui, |ui| match self.tab {
            Tab::Hosts => self.computers_tab(ui),
            Tab::ThisPc => egui::ScrollArea::vertical().show(ui, |ui| self.this_pc_tab(ui)).inner,
        });
        self.pin_dialog(&ui.ctx().clone());
    }
}

/// Where a new service PIN waits for the elevated step that applies it (and deletes the file).
fn pin_file() -> std::path::PathBuf {
    config::user_dir().join("pin-change.tmp")
}

fn exe_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(std::env::current_exe().ok()?).ok()?.modified().ok()
}

// ---------- look ----------

const BG: Color32 = Color32::from_rgb(0x0E, 0x10, 0x14);
const SURFACE: Color32 = Color32::from_rgb(0x16, 0x19, 0x20);
const TOOLBAR: Color32 = Color32::from_rgb(0x12, 0x15, 0x1B);
const BORDER: Color32 = Color32::from_rgb(0x22, 0x27, 0x31);
const RAISED: Color32 = Color32::from_rgb(0x1D, 0x21, 0x2A);
const RAISED_BORDER: Color32 = Color32::from_rgb(0x2A, 0x30, 0x3B);
const HOVER: Color32 = Color32::from_rgb(0x26, 0x2B, 0x36);
const TEXT: Color32 = Color32::from_rgb(0xE9, 0xEC, 0xF1);
const SOFT: Color32 = Color32::from_rgb(0xC9, 0xCE, 0xD6);
const MUTED: Color32 = Color32::from_rgb(0x9B, 0xA3, 0xAF);
const OFFLINE: Color32 = Color32::from_rgb(0x5C, 0x63, 0x70);
const ACCENT: Color32 = Color32::from_rgb(0x33, 0x66, 0xE6);
const ACCENT_HOVER: Color32 = Color32::from_rgb(0x44, 0x76, 0xF0);
const ACCENT_SOFT: Color32 = Color32::from_rgb(0x8F, 0xB0, 0xFF);
const ACCENT_TILE: Color32 = Color32::from_rgb(0x1A, 0x25, 0x45);
const GREEN: Color32 = Color32::from_rgb(0x3D, 0xD6, 0x8C);
const GREEN_TILE: Color32 = Color32::from_rgb(0x14, 0x30, 0x1F);
const YELLOW: Color32 = Color32::from_rgb(0xFF, 0xD4, 0x3B);
const YELLOW_TILE: Color32 = Color32::from_rgb(0x2E, 0x26, 0x14);
const RED: Color32 = Color32::from_rgb(0xFF, 0x6B, 0x6B);
const DANGER_FILL: Color32 = Color32::from_rgb(0x3A, 0x1D, 0x22);
const DANGER_HOVER: Color32 = Color32::from_rgb(0x4A, 0x23, 0x29);
const DANGER_TEXT: Color32 = Color32::from_rgb(0xFF, 0x8A, 0x8A);

const SEMIBOLD: &str = "semibold";

fn apply_style(ctx: &egui::Context) {
    install_fonts(ctx);
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.extreme_bg_color = BG;
    v.faint_bg_color = SURFACE;
    v.window_stroke = egui::Stroke::new(1.0, RAISED_BORDER);
    v.window_corner_radius = 10.into();
    v.menu_corner_radius = 10.into();
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = egui::Stroke::new(1.0, Color32::WHITE);
    v.hyperlink_color = ACCENT_SOFT;
    v.weak_text_color = Some(MUTED);
    let w = &mut v.widgets;
    for (state, fill, stroke, fg) in [
        (&mut w.noninteractive, SURFACE, BORDER, TEXT),
        (&mut w.inactive, RAISED, RAISED_BORDER, SOFT),
        (&mut w.hovered, HOVER, Color32::from_rgb(0x36, 0x3D, 0x4A), TEXT),
        (&mut w.active, Color32::from_rgb(0x2E, 0x34, 0x40), Color32::from_rgb(0x3E, 0x46, 0x55), TEXT),
        (&mut w.open, HOVER, RAISED_BORDER, TEXT),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.bg_stroke = egui::Stroke::new(1.0, stroke);
        state.fg_stroke = egui::Stroke::new(1.0, fg);
        state.corner_radius = 8.into();
        state.expansion = 0.0;
    }
    ctx.set_visuals(v);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(12.0, 6.0);
        style.spacing.interact_size.y = 30.0;
        style.spacing.menu_margin = egui::Margin::same(6);
        use egui::{FontFamily, FontId, TextStyle};
        style.text_styles = [
            (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(13.5, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(18.0, FontFamily::Name(SEMIBOLD.into()))),
            (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
        ]
        .into();
    });
}

/// Segoe UI (regular and semibold) and Cascadia Mono from the system, egui's own fonts as fallback.
fn install_fonts(ctx: &egui::Context) {
    use egui::{FontData, FontDefinitions, FontFamily};
    let mut fonts = FontDefinitions::default();
    let dir =
        std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into())).join("Fonts");
    let mut load = |name: &str, file: &str| {
        let data = std::fs::read(dir.join(file)).ok()?;
        fonts.font_data.insert(name.into(), Arc::new(FontData::from_owned(data)));
        Some(name.to_string())
    };
    let regular = load("segoe", "segoeui.ttf");
    let semibold = load("segoe-semibold", "seguisb.ttf");
    let mono = load("cascadia", "CascadiaMono.ttf").or_else(|| load("consolas", "consola.ttf"));
    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    if let Some(r) = &regular {
        proportional.insert(0, r.clone());
    }
    let mut strong: Vec<String> = semibold.into_iter().chain(proportional.iter().cloned()).collect();
    strong.dedup();
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), strong);
    if let Some(m) = mono {
        fonts.families.entry(FontFamily::Monospace).or_default().insert(0, m);
    }
    ctx.set_fonts(fonts);
}

fn semibold(text: impl Into<String>, size: f32) -> egui::RichText {
    egui::RichText::new(text).family(egui::FontFamily::Name(SEMIBOLD.into())).size(size).color(TEXT)
}

fn muted(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).color(MUTED)
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// The main action: accent fill.
    Primary,
    /// Raised, bordered.
    Secondary,
    /// No background until hovered.
    Ghost,
    /// Selected tool.
    Active,
    /// Selected tab of a segmented control.
    Tab,
    Danger,
}

fn button(ui: &mut egui::Ui, icon: Option<Icon>, text: &str, kind: Kind) -> egui::Response {
    button_sized(ui, icon, text, kind, 32.0)
}

fn small_button(ui: &mut egui::Ui, icon: Option<Icon>, text: &str, kind: Kind) -> egui::Response {
    button_sized(ui, icon, text, kind, 26.0)
}

fn icon_button(ui: &mut egui::Ui, icon: Icon, tip: &str, kind: Kind) -> egui::Response {
    button_sized(ui, Some(icon), "", kind, 32.0).on_hover_text(tip)
}

/// The app's button: optional leading icon, text, painted in one of the `Kind` styles.
fn button_sized(ui: &mut egui::Ui, icon: Option<Icon>, text: &str, kind: Kind, height: f32) -> egui::Response {
    let enabled = ui.is_enabled();
    let size = if height < 28.0 { 12.5 } else { 13.5 };
    let galley = ui.painter().layout_no_wrap(text.to_owned(), egui::FontId::proportional(size), Color32::PLACEHOLDER);
    let icon_size = if height < 28.0 { 14.0 } else { 16.0 };
    let icon_w = if icon.is_some() { icon_size } else { 0.0 };
    let gap = if icon.is_some() && !text.is_empty() { 7.0 } else { 0.0 };
    let pad = if text.is_empty() {
        (height - icon_size) / 2.0
    } else if height < 28.0 {
        9.0
    } else {
        12.0
    };
    let width = pad * 2.0 + icon_w + gap + galley.size().x;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    if ui.is_rect_visible(rect) {
        let hot = enabled && (resp.hovered() || resp.has_focus());
        let none = egui::Stroke::NONE;
        let (fill, stroke, fg) = match kind {
            Kind::Primary => (if hot { ACCENT_HOVER } else { ACCENT }, none, Color32::WHITE),
            Kind::Secondary => {
                (if hot { HOVER } else { RAISED }, egui::Stroke::new(1.0, RAISED_BORDER), if hot { TEXT } else { SOFT })
            }
            Kind::Ghost => (if hot { HOVER } else { Color32::TRANSPARENT }, none, if hot { TEXT } else { MUTED }),
            Kind::Active => (ACCENT, none, Color32::WHITE),
            Kind::Tab => (HOVER, none, TEXT),
            Kind::Danger => (if hot { DANGER_HOVER } else { DANGER_FILL }, none, DANGER_TEXT),
        };
        let (fill, fg) = if enabled { (fill, fg) } else { (fill.gamma_multiply(0.5), fg.gamma_multiply(0.45)) };
        let radius = if height < 30.0 { 6.0 } else { 8.0 };
        ui.painter().rect(rect, radius, fill, stroke, egui::StrokeKind::Inside);
        let mut x = rect.min.x + pad;
        if let Some(icon) = icon {
            let r = Rect::from_min_size(Pos2::new(x, rect.center().y - icon_size / 2.0), Vec2::splat(icon_size));
            icons::paint(ui, icon, r, icon_size, fg);
            x += icon_size + gap;
        }
        ui.painter().galley(Pos2::new(x, rect.center().y - galley.size().y / 2.0), galley, fg);
    }
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// A full-width row of a popup menu.
fn menu_item(ui: &mut egui::Ui, icon: Icon, text: &str, danger: bool) -> egui::Response {
    let enabled = ui.is_enabled();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::click());
    let hot = enabled && resp.hovered();
    if hot {
        ui.painter().rect_filled(rect, 6.0, HOVER);
    }
    let color = match (enabled, danger) {
        (false, _) => OFFLINE,
        (true, true) => DANGER_TEXT,
        (true, false) => {
            if hot {
                TEXT
            } else {
                SOFT
            }
        }
    };
    let icon_rect = Rect::from_min_size(Pos2::new(rect.min.x + 8.0, rect.center().y - 8.0), Vec2::splat(16.0));
    icons::paint(ui, icon, icon_rect, 16.0, color);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), egui::FontId::proportional(13.5), color);
    ui.painter().galley(Pos2::new(rect.min.x + 34.0, rect.center().y - galley.size().y / 2.0), galley, color);
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// Tabs as a segmented control.
fn segmented<T: PartialEq + Copy>(ui: &mut egui::Ui, value: &mut T, items: &[(T, Icon, &str)]) {
    egui::Frame::new().fill(SURFACE).stroke(egui::Stroke::new(1.0, BORDER)).corner_radius(9.0).inner_margin(3.0).show(
        ui,
        |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for &(v, icon, label) in items {
                let kind = if *value == v { Kind::Tab } else { Kind::Ghost };
                if button_sized(ui, Some(icon), label, kind, 28.0).clicked() {
                    *value = v;
                }
            }
        },
    );
}

fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(36.0, 20.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let track = if *on { ACCENT } else { Color32::from_rgb(0x3A, 0x41, 0x4E) };
    ui.painter().rect_filled(rect, 10.0, track);
    let x = egui::lerp((rect.left() + 10.0)..=(rect.right() - 10.0), t);
    ui.painter().circle_filled(Pos2::new(x, rect.center().y), 7.0, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn icon(ui: &mut egui::Ui, icon: Icon, color: Color32, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    icons::paint(ui, icon, rect, size, color);
}

/// A 40-point rounded square with a big icon: the state of a card at a glance.
fn icon_tile(ui: &mut egui::Ui, icon: Icon, fg: Color32, bg: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
    ui.painter().rect_filled(rect, 10.0, bg);
    icons::paint(ui, icon, rect, 22.0, fg);
}

fn title_block(ui: &mut egui::Ui, title: &str, sub: &str) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(semibold(title, 14.0));
        ui.label(muted(sub).small());
    });
}

/// A small coloured badge with an icon (lock screen, no gamepad).
fn chip(ui: &mut egui::Ui, icon: Icon, text: &str, fg: Color32, bg: Color32) -> egui::Response {
    egui::Frame::new()
        .fill(bg)
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                self::icon(ui, icon, fg, 14.0);
                ui.label(egui::RichText::new(text).small().color(fg));
            });
        })
        .response
}

fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(egui::Stroke::new(1.0, BORDER))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(4.0);
}

fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
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
    /// Watch only: nothing is sent to the remote PC.
    View,
}

/// Switches the session tool; entering view-only first lets go of everything held on the remote PC.
fn set_tool(view: &mut DesktopView, tool: Tool, ctx: &egui::Context) {
    if tool == Tool::View && view.tool != Tool::View {
        let send = |ev: InputEvent| {
            let _ = view.out.send(ClientMsg::Input(ev));
        };
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            send(InputEvent::MouseButton { button, down: false });
        }
        view.secure_keys.release_all(&send);
        if let Some(g) = view.game.as_mut() {
            release_capture(g, &view.out, ctx);
        }
        let _ = view.out.send(ClientMsg::Gamepad(Default::default()));
    }
    view.tool = tool;
    view.shared.view_only.store(tool == Tool::View, Ordering::Relaxed);
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
