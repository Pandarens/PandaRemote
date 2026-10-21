//! Client side: dial a host by its key, authenticate, then run a streaming session.

pub mod audio;
pub mod decoder;
#[cfg(not(target_os = "android"))]
pub mod gamepad;
pub mod record;
pub mod viewer;

use std::time::Duration;

use anyhow::{Context, Result, bail};
use iroh::{
    Endpoint,
    endpoint::{Connection, RecvStream, SendStream},
};
use tokio::sync::OnceCell;

use crate::{
    config,
    proto::{self, ClientMsg, Codec, HostMsg, PROTOCOL_VERSION, Profile},
    ticket::Ticket,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// One endpoint per process: several sessions share the client identity without fighting over the relay.
static ENDPOINT: OnceCell<Endpoint> = OnceCell::const_new();

async fn endpoint() -> Result<&'static Endpoint> {
    ENDPOINT.get_or_try_init(|| async { proto::bind_endpoint(config::client_key()?, None).await }).await
}

pub struct Session {
    pub conn: Connection,
    pub send: SendStream,
    pub recv: RecvStream,
    pub host_name: String,
}

/// The host asks for a PIN: none was given (`None`), or it refused the one given (the reason).
#[derive(Debug)]
pub struct PinError(pub Option<String>);

impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.as_deref().unwrap_or("нужен PIN-код"))
    }
}

impl std::error::Error for PinError {}

/// Opens a session. `codecs`: what this viewer decodes (see [`decoder::supported`]).
pub async fn connect(
    ticket: &Ticket,
    profile: Profile,
    pin: Option<&str>,
    monitor: Option<u8>,
    codecs: &[Codec],
) -> Result<Session> {
    let ep = endpoint().await?;
    let conn = tokio::time::timeout(CONNECT_TIMEOUT, ep.connect(ticket.host_id, proto::ALPN))
        .await
        .context("хост не отвечает (выключен или нет интернета)")?
        .context("не удалось соединиться с хостом")?;

    let (mut send, mut recv) = conn.open_bi().await?;
    let hello = ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        client_name: config::machine_name(),
        profile,
        monitor,
        codecs: codecs.to_vec(),
    };
    proto::write_msg(&mut send, &hello).await?;
    let HostMsg::Challenge { nonce, host_name } = proto::read_msg(&mut recv).await? else {
        bail!("unexpected message from host");
    };
    let mac = proto::auth_mac(&ticket.secret, &nonce, ep.id(), ticket.host_id);
    proto::write_msg(&mut send, &ClientMsg::Auth { mac: *mac.as_bytes() }).await?;
    let mut reply = proto::read_msg(&mut recv).await?;
    if let HostMsg::PinRequired = reply {
        let Some(pin) = pin else {
            conn.close(0u32.into(), b"pin");
            return Err(PinError(None).into());
        };
        proto::write_msg(&mut send, &ClientMsg::Pin(pin.to_string())).await?;
        reply = proto::read_msg(&mut recv).await?;
        if let HostMsg::AuthResult { ok: false, reason } = reply {
            return Err(PinError(Some(reason)).into());
        }
    }
    match reply {
        HostMsg::AuthResult { ok: true, .. } => {}
        HostMsg::AuthResult { ok: false, reason } => bail!("хост отказал: {reason}"),
        _ => bail!("unexpected message from host"),
    }
    Ok(Session { conn, send, recv, host_name })
}

/// Asks the PC behind `via` to send a Wake-on-LAN packet for each of `macs` on its network.
pub async fn wake(via: &Ticket, pin: Option<&str>, macs: &[[u8; 6]]) -> Result<()> {
    anyhow::ensure!(
        !macs.is_empty(),
        "адрес сетевой карты этого ПК неизвестен: подключитесь к нему один раз, когда он включён"
    );
    for mac in macs {
        let Session { conn, mut send, mut recv, .. } = connect(via, Profile::Control, pin, None, &[]).await?;
        proto::write_msg(&mut send, &ClientMsg::Wake { mac: *mac }).await?;
        let reply = proto::read_msg::<HostMsg>(&mut recv).await?;
        conn.close(0u32.into(), b"bye");
        match reply {
            HostMsg::Done(Ok(())) => {}
            HostMsg::Done(Err(e)) => bail!("{e}"),
            _ => bail!("unexpected answer"),
        }
    }
    Ok(())
}

/// Sends a file on its own stream; `progress(sent)` is called as it goes.
pub async fn send_file(conn: &Connection, path: &std::path::Path, progress: impl Fn(u64)) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await.with_context(|| format!("open {}", path.display()))?;
    let size = file.metadata().await?.len();
    anyhow::ensure!(size <= proto::MAX_FILE, "файл больше {} ГБ", proto::MAX_FILE >> 30);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "файл".into());
    let mut send = conn.open_uni().await?;
    proto::write_msg(&mut send, &proto::FileHeader { name, size }).await?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut sent = 0u64;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        send.write_all(&buf[..n]).await?;
        sent += n as u64;
        progress(sent);
    }
    send.finish()?;
    Ok(())
}

/// Closes the process-wide endpoint gracefully, so hosts see the disconnect at once instead of after
/// an idle timeout. Call before a short-lived command exits.
pub async fn shutdown() {
    if let Some(ep) = ENDPOINT.get() {
        ep.close().await;
    }
}

/// Checks whether a host is online; returns its route (P2P/relay, RTT) when it answered.
pub async fn ping(host_id: iroh::EndpointId) -> Result<String> {
    let ep = endpoint().await?;
    match tokio::time::timeout(Duration::from_secs(8), ep.connect(host_id, proto::PING_ALPN)).await {
        Err(_) => bail!("не отвечает"),
        Ok(Ok(conn)) => {
            let route = path_summary(&conn);
            conn.close(0u32.into(), b"ping");
            Ok(route)
        }
        Ok(Err(e)) => {
            // Hosts from before the ping protocol refuse it during the handshake, which still proves they are up.
            let text = format!("{e:#}").to_lowercase();
            if text.contains("alpn") || text.contains("application protocol") || text.contains("known protocol") {
                Ok("старая версия Panda, обновите её".into())
            } else {
                Err(e.into())
            }
        }
    }
}

/// Human-readable route: direct P2P or via relay, plus round-trip time.
pub fn path_summary(conn: &Connection) -> String {
    let paths = conn.paths();
    let direct = paths.iter().any(|p| p.is_ip());
    let rtt = paths.iter().map(|p| p.rtt()).min();
    let route = if direct { "P2P напрямую" } else { "через relay" };
    match rtt {
        Some(rtt) => format!("{route}, {} мс", rtt.as_millis()),
        None => route.to_string(),
    }
}
