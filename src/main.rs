// Hide the console window for the GUI build in release.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
use panda_rc::service;
use panda_rc::{client, config, gui, host, media, proto, ticket, update};

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "panda-rc", version, about = "P2P удалённый доступ и игровой стриминг по ключу")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Открыть окно свёрнутым (так запускает автозагрузка Windows)
    #[arg(long, global = true)]
    minimized: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Запустить хост без окна (принимать подключения к этому ПК)
    Host {
        /// Сгенерировать новый секрет: все выданные ранее билеты перестанут работать
        #[arg(long)]
        rotate_secret: bool,
        /// Запуск службой в сессии пользователя (не для ручного использования)
        #[arg(long, hide = true)]
        service_helper: bool,
    },
    /// Служба Windows: доступ на экране блокировки, в UAC и до входа в систему
    #[cfg(windows)]
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Показать билет подключения к этому ПК
    Ticket {
        /// Билет только для просмотра: экран виден, управлять нельзя
        #[arg(long)]
        view: bool,
    },
    /// Задать PIN-код, который спрашивается после билета (4–8 цифр), или `off` — убрать
    Pin { pin: String },
    /// Свой relay-сервер вместо серверов n0 (например https://relay.example.com), `off` — обратно
    Relay { url: String },
    /// Открыть окно сеанса (рабочий стол или игра)
    Connect {
        ticket: String,
        /// Игровой профиль: 60 к/с, захват мыши, геймпад
        #[arg(long)]
        game: bool,
    },
    /// Показать журнал подключений к этому ПК
    Log,
    /// Проверить, в сети ли компьютер с этим билетом
    Ping { ticket: String },
    /// Разбудить выключенный ПК (Wake-on-LAN) через другой ПК с Panda в той же сети
    Wake {
        /// Билет ПК, который отправит пакет пробуждения
        via: String,
        /// MAC-адрес сетевой карты спящего ПК, например 70-85-C2-0A-1B-FF
        mac: String,
        #[arg(long)]
        pin: Option<String>,
    },
    /// Проверить обновления на GitHub и установить, если есть
    Update,
    /// Выпуск: создать ключ подписи (приватный — в файл, публичный — на экран)
    #[command(hide = true)]
    ReleaseKeygen { out: std::path::PathBuf },
    /// Выпуск: подписать файл ключом из PANDA_SIGNING_KEY (пишет <файл>.sig)
    #[command(hide = true)]
    ReleaseSign { file: std::path::PathBuf },
    /// Какие кодеки (H.264, HEVC, AV1) этот ПК умеет кодировать и декодировать
    Codecs,
    /// Диагностика: скорость энкодера на синтетических кадрах 1080p
    #[command(hide = true)]
    BenchEncoder {
        #[arg(long)]
        sw: bool,
        /// h264, hevc или av1
        #[arg(long, default_value = "h264")]
        codec: String,
        #[arg(long, default_value_t = 60)]
        fps: u32,
    },
    /// Диагностика: подключиться к рабочему столу, принять видео и сохранить кадр в PNG
    Probe {
        ticket: String,
        #[arg(long, default_value = "probe.png")]
        out: std::path::PathBuf,
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        #[arg(long)]
        game: bool,
        /// PIN-код, если компьютер его спрашивает
        #[arg(long)]
        pin: Option<String>,
        /// Номер монитора (с 1); без него — основной
        #[arg(long)]
        monitor: Option<u8>,
        /// Предложить хосту только этот кодек (h264, hevc, av1); без него — всё, что декодирует этот ПК
        #[arg(long)]
        codec: Option<String>,
        /// Отправить файл на удалённый ПК (в «Загрузки\Panda»)
        #[arg(long, hide = true)]
        send_file: Option<std::path::PathBuf>,
        /// Показать сообщение на экране удалённого ПК
        #[arg(long, hide = true)]
        message: Option<String>,
        /// Положить текст в буфер обмена удалённого ПК
        #[arg(long, hide = true)]
        clipboard: Option<String>,
        /// Записать сеанс в эту папку
        #[arg(long, hide = true)]
        record: Option<std::path::PathBuf>,
        /// Через 2 с сдвинуть мышь хоста относительным движением и вернуть обратно (проверка ввода)
        #[arg(long, hide = true)]
        nudge: bool,
        /// Через 2 с выполнить действия через запятую: `click` (клик в центре), сканкод в hex (нажать и отпустить),
        /// `draw` (линия поверх экрана) или `sas` (Ctrl+Alt+Del через службу)
        #[arg(long, hide = true)]
        actions: Option<String>,
    },
}

#[cfg(windows)]
#[derive(Subcommand)]
enum ServiceAction {
    /// Установить и запустить службу (от имени администратора)
    Install {
        /// Куда записать результат (используется кнопкой в GUI)
        #[arg(long, hide = true)]
        result_file: Option<std::path::PathBuf>,
    },
    /// Остановить и удалить службу (от имени администратора)
    Uninstall {
        #[arg(long, hide = true)]
        result_file: Option<std::path::PathBuf>,
    },
    /// Скопировать билет службы в папку пользователя (для окна программы)
    #[command(hide = true)]
    SyncTicket {
        /// Папка пользователя, куда положить копию
        #[arg(long)]
        to: std::path::PathBuf,
        #[arg(long, hide = true)]
        result_file: Option<std::path::PathBuf>,
    },
    /// Создать билет для просмотра у службы и скопировать его в папку пользователя
    #[command(hide = true)]
    ViewTicket {
        #[arg(long)]
        to: std::path::PathBuf,
        #[arg(long, hide = true)]
        result_file: Option<std::path::PathBuf>,
    },
    /// Задать PIN-код службы из файла (пустой — убрать)
    #[command(hide = true)]
    SetPin {
        #[arg(long)]
        pin_file: std::path::PathBuf,
        #[arg(long)]
        to: std::path::PathBuf,
        #[arg(long, hide = true)]
        result_file: Option<std::path::PathBuf>,
    },
    /// Показать состояние службы
    Status,
    /// Точка входа службы (запускается Windows)
    #[command(hide = true)]
    Run,
    /// Перезапустить службу (после самообновления)
    #[command(hide = true)]
    Restart,
}

fn init_logging(cli: &Cli) {
    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info,iroh=warn,iroh_relay=warn,noq=warn".into())
    };
    // The service and its helper have no console: log to a file next to their config.
    let file_name = match &cli.command {
        #[cfg(windows)]
        Some(Command::Service { action: ServiceAction::Run }) => Some("service.log"),
        Some(Command::Host { service_helper: true, .. }) => Some("helper.log"),
        _ => None,
    };
    if let Some(name) = file_name {
        let dir = config::machine_logs_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        // Keep logs bounded: start over once they pass 5 MB.
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 5 << 20) {
            let _ = std::fs::remove_file(&path);
        }
        if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            tracing_subscriber::fmt()
                .with_env_filter(filter())
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .init();
            return;
        }
    }
    tracing_subscriber::fmt().with_env_filter(filter()).init();
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(&cli);
    let rt = tokio::runtime::Runtime::new()?;

    match cli.command {
        None => {
            if update::apply_staged_on_start() {
                return Ok(());
            }
            gui::run(rt, None, cli.minimized)
        }
        Some(Command::Connect { ticket, game }) => {
            gui::run(rt, Some((ticket::Ticket::decode(&ticket)?, profile(game))), cli.minimized)
        }
        Some(Command::Ticket { view }) => {
            let mut cfg = config::HostConfig::load_or_create().map_err(service_files_hint)?;
            let secret = if view { cfg.ensure_view_secret()? } else { cfg.access_secret };
            #[cfg(windows)]
            if view && config::host_dir() == config::machine_dir() {
                service::sync_user_copy(&config::user_dir())?;
            }
            let t = ticket::Ticket { host_id: config::host_key()?.public(), secret };
            println!("{}", t.encode());
            Ok(())
        }
        Some(Command::Wake { via, mac, pin }) => rt.block_on(async {
            let via = ticket::Ticket::decode(&via)?;
            let mac = host::net::parse_mac(&mac).context("MAC вида 70-85-C2-0A-1B-FF")?;
            let result = client::wake(&via, pin.as_deref(), &[mac]).await;
            client::shutdown().await;
            result?;
            println!("Пакет пробуждения отправлен через {}", via.host_id.fmt_short());
            Ok(())
        }),
        Some(Command::Codecs) => {
            println!("Кодирование (аппаратное, для хоста):");
            for (codec, name) in host::encoder::hardware_codecs() {
                println!("  {:6} {name}", codec.name());
            }
            println!("Декодирование (для клиента):");
            for (codec, name) in client::decoder::decoder_names() {
                println!("  {:6} {name}", codec.name());
            }
            let ok: Vec<_> = client::decoder::supported().into_iter().map(|c| c.name()).collect();
            println!("Клиент попросит: {}", ok.join(", "));
            Ok(())
        }
        Some(Command::Relay { url }) => {
            let relay = (url != "off").then_some(url);
            if let Some(u) = &relay {
                u.parse::<iroh::RelayUrl>().with_context(|| format!("адрес relay-сервера: {u}"))?;
            }
            let mut cfg = config::HostConfig::load_or_create().map_err(service_files_hint)?;
            cfg.relay = relay.clone();
            cfg.save()?;
            #[cfg(windows)]
            if config::host_dir() == config::machine_dir() {
                service::sync_user_copy(&config::user_dir())?;
                service::restart()?;
                println!("Служба перезапущена с новым relay.");
            }
            match relay {
                Some(u) => println!("Relay: {u}. Хост в окне программы применит его после перезапуска Panda."),
                None => println!("Relay: серверы n0 (по умолчанию)."),
            }
            Ok(())
        }
        Some(Command::Pin { pin }) => {
            let pin = (pin != "off").then_some(pin);
            let mut cfg = config::HostConfig::load_or_create().map_err(service_files_hint)?;
            cfg.set_pin(pin.as_deref())?;
            #[cfg(windows)]
            if config::host_dir() == config::machine_dir() {
                // The service's files changed: refresh this user's copy the app reads.
                service::sync_user_copy(&config::user_dir())?;
            }
            println!("{}", if pin.is_some() { "PIN-код задан." } else { "PIN-код убран." });
            Ok(())
        }
        Some(Command::ReleaseKeygen { out }) => {
            let key = iroh::SecretKey::generate();
            std::fs::write(&out, data_encoding::HEXLOWER.encode(&key.to_bytes()))?;
            println!("{}", data_encoding::HEXLOWER.encode(key.public().as_bytes()));
            Ok(())
        }
        Some(Command::ReleaseSign { file }) => {
            let secret =
                std::env::var("PANDA_SIGNING_KEY").map_err(|_| anyhow::anyhow!("PANDA_SIGNING_KEY is not set"))?;
            let sig = update::sign(&std::fs::read(&file)?, update::Version::current(), &secret)?;
            let mut sig_path = file.into_os_string();
            sig_path.push(".sig");
            std::fs::write(&sig_path, sig)?;
            Ok(())
        }
        Some(Command::Update) => rt.block_on(async {
            println!("Текущая версия: {}", update::Version::current());
            match update::newer_release().await? {
                None => println!("Установлена последняя версия."),
                Some(rel) => {
                    println!("Доступна {}: скачиваю и проверяю подпись…", rel.version);
                    let staged = update::download(&rel, &update::staging_dir(false)).await?;
                    update::install(&staged, &std::env::current_exe()?)?;
                    println!("Обновлено до {}. Перезапустите Panda.", rel.version);
                }
            }
            Ok(())
        }),
        Some(Command::Ping { ticket }) => rt.block_on(async {
            let ticket = ticket::Ticket::decode(&ticket)?;
            match client::ping(ticket.host_id).await {
                Ok(route) => println!("В сети: {route}"),
                Err(e) => println!("Не в сети: {e:#}"),
            }
            client::shutdown().await;
            Ok(())
        }),
        Some(Command::Log) => {
            let entries = config::read_journal(100);
            if entries.is_empty() && config::service_installed() {
                eprintln!("Журнал службы пока пуст: {}", config::machine_logs_dir().join("connections.log").display());
            }
            for e in entries.into_iter().rev() {
                println!(
                    "{}  {:<13} {} ({})  {}",
                    e.ts,
                    e.event,
                    e.client_name,
                    &e.client_id[..10.min(e.client_id.len())],
                    e.detail
                );
            }
            Ok(())
        }
        #[cfg(windows)]
        Some(Command::Service { action }) => match action {
            ServiceAction::Install { result_file } => service::report(result_file, service::install(), || {
                println!("Служба установлена и запущена. Билет прежний: panda-rc ticket")
            }),
            ServiceAction::Uninstall { result_file } => {
                service::report(result_file, service::uninstall(), || println!("Служба удалена."))
            }
            ServiceAction::SyncTicket { to, result_file } => {
                service::report(result_file, service::sync_user_copy(&to), || {})
            }
            ServiceAction::ViewTicket { to, result_file } => {
                service::report(result_file, service::create_view_ticket(&to), || {})
            }
            ServiceAction::SetPin { pin_file, to, result_file } => {
                service::report(result_file, service::set_pin(&pin_file, &to), || println!("PIN-код службы обновлён."))
            }
            ServiceAction::Status => {
                match service::status() {
                    Some(state) => println!("Служба: {state:?}"),
                    None => println!("Служба не установлена"),
                }
                Ok(())
            }
            ServiceAction::Run => service::run_dispatcher(),
            ServiceAction::Restart => service::restart(),
        },
        Some(Command::Host { rotate_secret, service_helper }) => rt.block_on(async {
            // Two hosts with one key would fight over the relay; the service already serves this PC.
            #[cfg(windows)]
            // A separate PANDA_HOME means a separate identity (tests), which doesn't compete with the service.
            if !service_helper
                && std::env::var_os("PANDA_HOME").is_none()
                && service::status() == Some(windows_service::service::ServiceState::Running)
            {
                anyhow::bail!("на этом ПК уже работает служба Panda Remote; она принимает подключения");
            }
            let mut cfg = config::HostConfig::load_or_create()?;
            if rotate_secret {
                cfg.rotate_secret()?;
                println!("Секрет обновлён, старые билеты больше не действуют.");
            }
            let host = host::Host::start(cfg).await?;
            println!("Хост запущен. ID: {}", host.ticket.host_id);
            println!("Билет подключения (храните как пароль):\n\n{}\n", host.ticket.encode());
            host.online().await;
            tracing::info!(host_id = %host.ticket.host_id, "host online");
            if service_helper {
                // Runs until the service terminates this process.
                std::future::pending::<()>().await;
            }
            println!("В сети. Ctrl+C для остановки.");
            tokio::signal::ctrl_c().await?;
            host.shutdown().await;
            Ok(())
        }),
        Some(Command::BenchEncoder { sw, fps, codec }) => {
            bench_encoder(!sw, fps, proto::Codec::parse(&codec).context("кодек: h264, hevc или av1")?)
        }
        Some(Command::Probe {
            ticket,
            out,
            seconds,
            game,
            pin,
            monitor,
            codec,
            send_file,
            message,
            clipboard,
            record,
            nudge,
            actions,
        }) => rt.block_on(async {
            let ticket = ticket::Ticket::decode(&ticket)?;
            let started = std::time::Instant::now();
            let codecs = match codec {
                Some(c) => vec![proto::Codec::parse(&c).context("кодек: h264, hevc или av1")?],
                None => client::decoder::supported(),
            };
            let monitor = monitor.map(|m| m.saturating_sub(1));
            let session = client::connect(&ticket, profile(game), pin.as_deref(), monitor, &codecs).await?;
            println!(
                "Подключено к «{}» за {:?} ({})",
                session.host_name,
                started.elapsed(),
                client::path_summary(&session.conn)
            );
            let shared = Arc::new(client::viewer::Shared::default());
            let frames = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let counter = frames.clone();
            let repaint: client::viewer::Repaint = Arc::new(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            });
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            if nudge {
                let tx = tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    for (dx, dy) in [(200, 100), (-200, -100)] {
                        let _ = tx.send(proto::ClientMsg::Input(proto::InputEvent::MouseRelative { dx, dy }));
                        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
                    }
                });
            }
            // No playback here: on a same-machine test the host's loopback would capture it (feedback).
            if let Some(actions) = actions {
                let (tx, shared) = (tx.clone(), shared.clone());
                tokio::spawn(async move {
                    use proto::{ClientMsg::Input, InputEvent::*, MouseButton};
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    // Test input only ever goes to the lock screen, never into someone's open windows.
                    // Drawing is not input (it only shows an overlay), and Ctrl+Alt+Del only opens the security
                    // screen, so those are always allowed.
                    let draw_only = actions.split(',').all(|a| matches!(a.trim(), "draw" | "sas"));
                    if !draw_only && !shared.secure_desktop.load(std::sync::atomic::Ordering::Relaxed) {
                        println!("Действия пропущены: хост не на экране блокировки");
                        return;
                    }
                    for action in actions.split(',').map(str::trim) {
                        if action == "draw" {
                            // A diagonal stroke across the middle, left unfinished so it stays visible.
                            for i in 0..=20u32 {
                                let v = (20000 + i * 1250) as u16;
                                let _ = tx.send(proto::ClientMsg::Draw(media::annotate::DrawEvent::Point {
                                    stroke: 1,
                                    x: v,
                                    y: v,
                                    color: media::annotate::COLORS[0],
                                }));
                            }
                        } else if action == "sas" {
                            let _ = tx.send(proto::ClientMsg::SecureAttention);
                        } else if action == "click" {
                            let _ = tx.send(Input(MouseMove { x: 32768, y: 32768 }));
                            let _ = tx
                                .send(Input(proto::InputEvent::MouseButton { button: MouseButton::Left, down: true }));
                            let _ = tx
                                .send(Input(proto::InputEvent::MouseButton { button: MouseButton::Left, down: false }));
                        } else if let Ok(code) = u16::from_str_radix(action, 16) {
                            let _ = tx.send(Input(Scancode { code, extended: false, down: true }));
                            let _ = tx.send(Input(Scancode { code, extended: false, down: false }));
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
                    }
                });
            }
            let (files_tx, files_rx) = tokio::sync::mpsc::unbounded_channel();
            if let Some(path) = send_file {
                let _ = files_tx.send(path);
            }
            if let Some(text) = message {
                let _ = tx.send(proto::ClientMsg::Message(text));
            }
            if let Some(text) = clipboard {
                let _ = tx.send(proto::ClientMsg::Clipboard(text));
            }
            if let Some(dir) = record.clone() {
                let (shared, tx) = (shared.clone(), tx.clone());
                tokio::spawn(async move {
                    // Wait for the stream's codec and size, then record from the next keyframe.
                    for _ in 0..50 {
                        if let Some((codec, w, h)) = *shared.stream.lock().unwrap() {
                            match client::record::Recorder::start(&dir, "probe", codec, w, h) {
                                Ok(r) => *shared.recorder.lock().unwrap() = Some(r),
                                Err(e) => println!("Запись: {e:#}"),
                            }
                            let _ = tx.send(proto::ClientMsg::RequestKeyframe);
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                });
            }
            let run = client::viewer::run(session, shared.clone(), tx, rx, files_rx, repaint, false);
            let _ = tokio::time::timeout(std::time::Duration::from_secs(seconds), run).await;
            client::shutdown().await;
            drop(files_tx);
            if let Some((codec, w, h)) = *shared.stream.lock().unwrap() {
                println!("Кодек: {} {w}x{h}", codec.name());
            }
            if let Some(rec) = shared.recorder.lock().unwrap().take() {
                match rec.finish() {
                    Ok(path) => println!("Запись: {}", path.display()),
                    Err(e) => println!("Запись не сохранена: {e:#}"),
                }
            }
            for (_, error, text) in shared.notes.lock().unwrap().iter() {
                println!("{}{text}", if *error { "Ошибка: " } else { "" });
            }
            if let Some(text) = shared.remote_clipboard.lock().unwrap().as_deref() {
                println!("Буфер обмена хоста: {} символов", text.chars().count());
            }
            let macs: Vec<_> = shared.macs.lock().unwrap().iter().map(host::net::format_mac).collect();
            println!("MAC хоста: {}", macs.join(", "));
            let frame = shared.frame.lock().unwrap().take();
            if frame.is_none() {
                println!("Статус без кадров: {}", shared.status.lock().unwrap());
                println!(
                    "Экран блокировки (защищённый рабочий стол): {}",
                    shared.secure_desktop.load(std::sync::atomic::Ordering::Relaxed)
                );
            }
            match frame {
                Some(f) => {
                    xcap::image::RgbaImage::from_raw(f.width as u32, f.height as u32, f.to_rgba())
                        .ok_or_else(|| anyhow::anyhow!("bad frame"))?
                        .save(&out)?;
                    println!("Последний статус: {}", shared.status.lock().unwrap());
                    println!(
                        "Экран блокировки (защищённый рабочий стол): {}",
                        shared.secure_desktop.load(std::sync::atomic::Ordering::Relaxed)
                    );
                    println!(
                        "Кадр {}x{} сохранён в {}, событий отрисовки: {}",
                        f.width,
                        f.height,
                        out.display(),
                        frames.load(std::sync::atomic::Ordering::Relaxed)
                    );
                    Ok(())
                }
                None => anyhow::bail!("кадры не получены"),
            }
        }),
    }
}

/// With the service installed, host files are admin-only; say so instead of a bare access error.
fn service_files_hint(e: anyhow::Error) -> anyhow::Error {
    if config::service_installed() {
        e.context("настройки службы доступны только администраторам: запустите терминал от имени администратора")
    } else {
        e
    }
}

fn profile(game: bool) -> proto::Profile {
    if game { proto::Profile::Game } else { proto::Profile::Desktop }
}

fn bench_encoder(hw: bool, fps: u32, codec: proto::Codec) -> Result<()> {
    use host::encoder::{self, Picture};
    let (w, h) = (1920u32, 1080u32);
    let (codec, mut enc) = encoder::create(&[codec], w, h, fps, 20_000_000, hw)?;
    println!("Энкодер: {} ({})", enc.name(), codec.name());
    // No decoder on this PC (e.g. HEVC without Windows' extension): measure the encoder alone.
    let mut decoders: Vec<_> = client::decoder::create(codec, w, h, true)
        .map_err(|e| println!("Декодер {}: нет ({e:#})", codec.name()))
        .into_iter()
        .collect();
    if codec == proto::Codec::H264 {
        decoders.push(client::decoder::create(codec, w, h, false)?);
    }
    let mut decode_times = [std::time::Duration::ZERO; 2];
    let mut decoded_counts = [0u32; 2];
    let mut decode_cpu = [std::time::Duration::ZERO; 2];
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    let (mut total, mut bytes, mut frames, mut keys) = (std::time::Duration::ZERO, 0usize, 0, 0);
    for i in 0..180u32 {
        // Moving gradient + a sliding white bar: realistic motion for the rate control.
        for y in 0..h {
            for x in 0..w {
                let o = ((y * w + x) * 4) as usize;
                let bar = (x + i * 12) % w < 80;
                pixels[o] = if bar { 255 } else { (x / 8 + i) as u8 };
                pixels[o + 1] = if bar { 255 } else { (y / 4) as u8 };
                pixels[o + 2] = if bar { 255 } else { (i * 3) as u8 };
                pixels[o + 3] = 255;
            }
        }
        let t = std::time::Instant::now();
        let out = enc.encode(&Picture { pixels: &pixels, width: w, height: h, bgra: true }, i == 90)?;
        total += t.elapsed();
        for f in out {
            frames += 1;
            bytes += f.data.len();
            keys += f.keyframe as u32;
            for (k, dec) in decoders.iter_mut().enumerate() {
                let (t, cpu) = (std::time::Instant::now(), thread_cpu_time());
                if dec.decode(&f.data)?.is_some() {
                    decoded_counts[k] += 1;
                }
                decode_times[k] += t.elapsed();
                decode_cpu[k] += thread_cpu_time().saturating_sub(cpu);
            }
        }
    }
    println!(
        "180 кадров: {:.2} мс/кадр (с конвертацией), выход {frames} кадров, IDR {keys}, средний размер {} КБ",
        total.as_secs_f64() * 1000.0 / 180.0,
        bytes / frames.max(1) / 1024,
    );
    for (k, dec) in decoders.iter().enumerate() {
        println!(
            "Декодер {}: {} кадров, {:.2} мс/кадр, из них CPU этого потока {:.2} мс (до NV12 в памяти)",
            dec.name(),
            decoded_counts[k],
            decode_times[k].as_secs_f64() * 1000.0 / decoded_counts[k].max(1) as f64,
            decode_cpu[k].as_secs_f64() * 1000.0 / decoded_counts[k].max(1) as f64
        );
    }
    Ok(())
}

/// CPU time consumed by the calling thread (Windows); zero elsewhere.
fn thread_cpu_time() -> std::time::Duration {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::{
            Foundation::FILETIME,
            System::Threading::{GetCurrentThread, GetThreadTimes},
        };
        let (mut c, mut e, mut k, mut u) =
            (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        if GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u).is_ok() {
            let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) * 100;
            return std::time::Duration::from_nanos(t(k) + t(u));
        }
    }
    std::time::Duration::ZERO
}
