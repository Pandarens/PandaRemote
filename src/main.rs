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
        Some(Command::BenchEncoder { sw, fps }) => bench_encoder(!sw, fps),
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

fn bench_encoder(hw: bool, fps: u32) -> Result<()> {
    use host::encoder::{self, Picture};
    let (w, h) = (1920u32, 1080u32);
    let mut enc = encoder::create(w, h, fps, 20_000_000, hw)?;
    println!("Энкодер: {}", enc.name());
    let mut decoder = openh264::decoder::Decoder::new()?;
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    let (mut total, mut bytes, mut frames, mut keys, mut decoded) = (std::time::Duration::ZERO, 0usize, 0, 0, 0);
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
            if decoder.decode(&f.data)?.is_some() {
                decoded += 1;
            }
        }
    }
    println!(
        "180 кадров: {:.2} мс/кадр (с конвертацией), выход {frames} кадров, IDR {keys}, средний размер {} КБ, декодировано {decoded}",
        total.as_secs_f64() * 1000.0 / 180.0,
        bytes / frames.max(1) / 1024
    );
    Ok(())
}
