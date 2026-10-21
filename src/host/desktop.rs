//! Remote-desktop session on the host: video out on a uni stream, input in on the control stream.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::sync::{mpsc, oneshot};

use super::{capture, input};
use crate::proto::{self, ClientMsg, HostMsg, VideoFrame};

pub async fn serve(conn: &Connection, mut send: SendStream, mut recv: RecvStream) -> Result<()> {
    let (frame_tx, mut frame_rx) = mpsc::channel::<VideoFrame>(1);
    let (geo_tx, geo_rx) = oneshot::channel();
    let keyframe = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let (keyframe, stop) = (keyframe.clone(), stop.clone());
        std::thread::Builder::new()
            .name("capture".into())
            .spawn(move || capture::run(frame_tx, geo_tx, keyframe, stop))?;
    }
    let geo = geo_rx.await??;
    proto::write_msg(&mut send, &HostMsg::ScreenInfo { width: geo.width, height: geo.height }).await?;

    let input_tx = input::spawn(geo);
    let mut video = conn.open_uni().await?;
    let mut video_task = tokio::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            proto::write_msg(&mut video, &frame).await?;
        }
        anyhow::Ok(())
    });

    let control = async {
        loop {
            match proto::read_msg::<ClientMsg>(&mut recv).await? {
                ClientMsg::Input(ev) => {
                    let _ = input_tx.send(ev);
                }
                ClientMsg::RequestKeyframe => keyframe.store(true, Ordering::Relaxed),
                _ => {}
            }
        }
    };

    let result: Result<()> = tokio::select! {
        r = &mut video_task => r.map_err(Into::into).and_then(|r| r),
        r = control => r,
    };
    stop.store(true, Ordering::Relaxed);
    video_task.abort();
    // A closed control stream is the normal way a viewer leaves.
    match result {
        Err(e) if is_closed(&e) => Ok(()),
        other => other,
    }
}

fn is_closed(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}").to_lowercase();
    s.contains("closed") || s.contains("finished") || s.contains("reset") || s.contains("lost")
}
