// Hide the console window for the GUI build in release.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod client;
mod config;
mod gui;
mod host;
mod proto;
mod ticket;

use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "panda-rc", version, about = "P2P удалённый доступ и игровой туннель по ключу")]
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
    },
    /// Показать билет подключения к этому ПК
    Ticket,
    /// Открыть окно удалённого рабочего стола
    Connect { ticket: String },
    /// Показать журнал подключений к этому ПК
    Log,
    /// Диагностика: подключиться к рабочему столу, принять видео и сохранить кадр в PNG
    Probe {
        ticket: String,
        #[arg(long, default_value = "probe.png")]
        out: std::path::PathBuf,
        #[arg(long, default_value_t = 5)]
        seconds: u64,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,iroh=warn,iroh_relay=warn,noq=warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let rt = tokio::runtime::Runtime::new()?;

    match cli.command {
        None => gui::run(rt, None),
        Some(Command::Connect { ticket }) => gui::run(rt, Some(ticket::Ticket::decode(&ticket)?)),
        Some(Command::Ticket) => {
            let cfg = config::HostConfig::load_or_create()?;
            let t = ticket::Ticket { host_id: config::host_key()?.public(), secret: cfg.access_secret };
            println!("{}", t.encode());
            Ok(())
        }
        Some(Command::Log) => {
            for e in config::read_journal(100).into_iter().rev() {
                println!("{}  {:<13} {} ({})  {}", e.ts, e.event, e.client_name, &e.client_id[..10.min(e.client_id.len())], e.detail);
            }
            Ok(())
        }
        Some(Command::Host { rotate_secret }) => rt.block_on(async {
            let mut cfg = config::HostConfig::load_or_create()?;
            if rotate_secret {
                cfg.rotate_secret()?;
                println!("Секрет обновлён, старые билеты больше не действуют.");
            }
            let host = host::Host::start(cfg).await?;
            println!("Хост запущен. ID: {}", host.ticket.host_id);
            println!("Билет подключения (храните как пароль):\n\n{}\n", host.ticket.encode());
            host.online().await;
            println!("В сети. Ctrl+C для остановки.");
            tokio::signal::ctrl_c().await?;
            host.shutdown().await;
            Ok(())
        }),
        Some(Command::Probe { ticket, out, seconds }) => rt.block_on(async {
            let ticket = ticket::Ticket::decode(&ticket)?;
            let started = std::time::Instant::now();
            let session = client::connect(&ticket, proto::SessionKind::Desktop).await?;
            println!("Подключено к «{}» за {:?} ({})", session.host_name, started.elapsed(), client::path_summary(&session.conn));
            let shared = Arc::new(client::viewer::Shared::default());
            let frames = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let counter = frames.clone();
            let repaint: client::viewer::Repaint = Arc::new(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            });
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let run = client::viewer::run(session, shared.clone(), tx, rx, repaint);
            let _ = tokio::time::timeout(std::time::Duration::from_secs(seconds), run).await;
            let frame = shared.frame.lock().unwrap().take();
            match frame {
                Some(f) => {
                    xcap::image::RgbaImage::from_raw(f.width as u32, f.height as u32, f.pixels)
                        .ok_or_else(|| anyhow::anyhow!("bad frame"))?
                        .save(&out)?;
                    println!("Кадр {}x{} сохранён в {}, событий отрисовки: {}", f.width, f.height, out.display(), frames.load(std::sync::atomic::Ordering::Relaxed));
                    Ok(())
                }
                None => anyhow::bail!("кадры не получены"),
            }
        }),
    }
}
