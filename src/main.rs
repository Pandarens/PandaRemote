// Hide the console window for the GUI build in release.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod client;
mod config;
mod gui;
mod host;
mod media;
mod proto;
#[cfg(windows)]
mod service;
mod ticket;
mod yuv_render;

use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "panda-rc", version, about = "P2P удалённый доступ и игровой стриминг по ключу")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
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
    Ticket,
    /// Открыть окно сеанса (рабочий стол или игра)
    Connect {
        ticket: String,
        /// Игровой профиль: 60 к/с, захват мыши, геймпад
        #[arg(long)]
        game: bool,
    },
    /// Показать журнал подключений к этому ПК
    Log,
    /// Диагностика: скорость H.264-энкодера на синтетических кадрах 1080p
    #[command(hide = true)]
    BenchEncoder {
        #[arg(long)]
        sw: bool,
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
        /// Через 2 с сдвинуть мышь хоста относительным движением и вернуть обратно (проверка ввода)
        #[arg(long, hide = true)]
        nudge: bool,
        /// Через 2 с выполнить действия через запятую: `click` (клик в центре) или сканкод в hex (нажать и отпустить)
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
    /// Показать состояние службы
    Status,
    /// Точка входа службы (запускается Windows)
    #[command(hide = true)]
    Run,
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
        let dir = config::machine_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        // Keep logs bounded: start over once they pass 5 MB.
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 5 << 20) {
            let _ = std::fs::remove_file(&path);
        }
        if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            config::share_read_with_users(&path);
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
        None => gui::run(rt, None),
        Some(Command::Connect { ticket, game }) => {
            gui::run(rt, Some((ticket::Ticket::decode(&ticket)?, profile(game))))
        }
        Some(Command::Ticket) => {
            let cfg = config::HostConfig::load_or_create().map_err(service_files_hint)?;
            let t = ticket::Ticket { host_id: config::host_key()?.public(), secret: cfg.access_secret };
            println!("{}", t.encode());
            Ok(())
        }
        Some(Command::Log) => {
            let entries = config::read_journal(100);
            if entries.is_empty() && config::service_installed() {
                eprintln!("Журнал службы доступен только администраторам: запустите терминал от имени администратора.");
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
            ServiceAction::Status => {
                match service::status() {
                    Some(state) => println!("Служба: {state:?}"),
                    None => println!("Служба не установлена"),
                }
                Ok(())
            }
            ServiceAction::Run => service::run_dispatcher(),
        },
        Some(Command::Host { rotate_secret, service_helper }) => rt.block_on(async {
            // Two hosts with one key would fight over the relay; the service already serves this PC.
            #[cfg(windows)]
            if !service_helper && service::status() == Some(windows_service::service::ServiceState::Running) {
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
        Some(Command::BenchEncoder { sw, fps }) => bench_encoder(!sw, fps),
        Some(Command::Probe { ticket, out, seconds, game, nudge, actions }) => rt.block_on(async {
            let ticket = ticket::Ticket::decode(&ticket)?;
            let started = std::time::Instant::now();
            let session = client::connect(&ticket, profile(game)).await?;
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
                    if !shared.secure_desktop.load(std::sync::atomic::Ordering::Relaxed) {
                        println!("Действия пропущены: хост не на экране блокировки");
                        return;
                    }
                    for action in actions.split(',').map(str::trim) {
                        if action == "click" {
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
            let run = client::viewer::run(session, shared.clone(), tx, rx, repaint, false);
            let _ = tokio::time::timeout(std::time::Duration::from_secs(seconds), run).await;
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

fn bench_encoder(hw: bool, fps: u32) -> Result<()> {
    use host::encoder::{self, Picture};
    let (w, h) = (1920u32, 1080u32);
    let mut enc = encoder::create(w, h, fps, 20_000_000, hw)?;
    println!("Энкодер: {}", enc.name());
    let mut decoders = [client::decoder::create(w, h, true)?, client::decoder::create(w, h, false)?];
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
