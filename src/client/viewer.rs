//! Remote-desktop viewer networking: receives and decodes video, sends input.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use openh264::{decoder::Decoder, formats::YUVSource};
use tokio::sync::mpsc;

use super::Session;
use crate::proto::{self, ClientMsg, HostMsg, VideoFrame};

pub struct RgbaFrame {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

/// State shared between the network tasks and the GUI.
#[derive(Default)]
pub struct Shared {
    pub frame: Mutex<Option<RgbaFrame>>,
    pub status: Mutex<String>,
    frames: AtomicU32,
}

pub type Repaint = Arc<dyn Fn() + Send + Sync>;

pub async fn run(
    session: Session,
    shared: Arc<Shared>,
    out_tx: mpsc::UnboundedSender<ClientMsg>,
    mut out_rx: mpsc::UnboundedReceiver<ClientMsg>,
    repaint: Repaint,
) -> Result<()> {
    let Session { conn, mut send, mut recv, .. } = session;
    let HostMsg::ScreenInfo { width, height } = proto::read_msg(&mut recv).await? else {
        anyhow::bail!("expected ScreenInfo");
    };
    tracing::info!(width, height, "remote screen");
    let mut video = conn.accept_uni().await?;

    let (dec_tx, dec_rx) = mpsc::channel::<VideoFrame>(8);
    {
        let (shared, repaint, out_tx) = (shared.clone(), repaint.clone(), out_tx.clone());
        std::thread::Builder::new()
            .name("decoder".into())
            .spawn(move || decode_loop(dec_rx, shared, out_tx, repaint))?;
    }

    let receive = async {
        loop {
            let frame: VideoFrame = proto::read_msg(&mut video).await?;
            if dec_tx.send(frame).await.is_err() {
                anyhow::bail!("decoder stopped");
            }
        }
    };
    let transmit = async {
        while let Some(msg) = out_rx.recv().await {
            proto::write_msg(&mut send, &msg).await?;
        }
        anyhow::Ok(())
    };
    let status = async {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let fps = shared.frames.swap(0, Ordering::Relaxed);
            *shared.status.lock().unwrap() = format!("{} · {fps} к/с", super::path_summary(&conn));
            repaint();
        }
    };

    tokio::select! {
        r = receive => r,
        r = transmit => r,
        _ = status => Ok(()),
        e = conn.closed() => Err(anyhow::anyhow!("соединение закрыто: {e}")),
    }
}

fn decode_loop(
    mut rx: mpsc::Receiver<VideoFrame>,
    shared: Arc<Shared>,
    out_tx: mpsc::UnboundedSender<ClientMsg>,
    repaint: Repaint,
) {
    let mut decoder = match Decoder::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("H.264 decoder init failed: {e}");
            return;
        }
    };
    while let Some(frame) = rx.blocking_recv() {
        match decoder.decode(&frame.data) {
            Ok(Some(yuv)) => {
                let (w, h) = yuv.dimensions();
                let mut pixels = vec![0u8; w * h * 4];
                yuv.write_rgba8(&mut pixels);
                *shared.frame.lock().unwrap() = Some(RgbaFrame { width: w, height: h, pixels });
                shared.frames.fetch_add(1, Ordering::Relaxed);
                repaint();
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!("decode error, requesting keyframe: {e}");
                let _ = out_tx.send(ClientMsg::RequestKeyframe);
            }
        }
    }
}
