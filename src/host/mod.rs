//! Host side: accepts connections addressed to this machine's key, authenticates them
//! and runs a streaming session.

mod audio;
mod capture;
mod clipboard;
#[cfg(windows)]
pub mod desktop_switch;
#[cfg(windows)]
mod dxgi;
pub mod encoder;
mod files;
mod gamepad;
mod input;
pub mod net;
#[cfg(windows)]
mod overlay;
mod session;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use iroh::{
    Endpoint, EndpointId,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler, Router},
};

use crate::{
    config::{self, HostConfig, PinHash},
    proto::{self, ClientMsg, HostMsg, PROTOCOL_VERSION, Profile},
    ticket::Ticket,
};

pub struct Host {
    router: Router,
    pub ticket: Ticket,
}

impl Host {
    pub async fn start(cfg: HostConfig) -> Result<Self> {
        input::set_dpi_aware();

        let endpoint = proto::bind_endpoint(config::host_key()?, cfg.relay.as_deref()).await?;
        let id = endpoint.id();
        let ticket = Ticket { host_id: id, secret: cfg.access_secret };
        let handler = Handler { cfg: Arc::new(cfg), id, pin_guard: Arc::default() };
        let router = Router::builder(endpoint).accept(proto::ALPN, handler).accept(proto::PING_ALPN, Ping).spawn();
        Ok(Host { router, ticket })
    }

    /// Resolves once the endpoint has a home relay and is reachable from anywhere.
    pub async fn online(&self) {
        self.router.endpoint().online().await;
    }

    /// Endpoint handle for awaiting `online()` without borrowing the host.
    pub fn online_handle(&self) -> Endpoint {
        self.router.endpoint().clone()
    }

    pub async fn shutdown(self) {
        let _ = self.router.shutdown().await;
    }
}

/// One request on a control connection (no picture): Wake-on-LAN for a PC on this host's network.
async fn control(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    view_only: bool,
) -> Result<(), String> {
    let _ = send;
    if view_only {
        return Err("билет для просмотра не даёт управлять этим ПК".into());
    }
    match proto::read_msg::<ClientMsg>(recv).await.map_err(|e| format!("{e:#}"))? {
        ClientMsg::Wake { mac } => net::wake(mac).map_err(|e| format!("{e:#}")),
        other => Err(format!("неожиданный запрос: {other:?}")),
    }
}

/// Answers presence checks: the finished handshake is the answer.
#[derive(Debug, Clone)]
struct Ping;

impl ProtocolHandler for Ping {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        conn.close(0u32.into(), b"pong");
        Ok(())
    }
}

#[derive(Clone)]
struct Handler {
    cfg: Arc<HostConfig>,
    id: EndpointId,
    pin_guard: Arc<PinGuard>,
}

/// Wrong PINs in a row before PIN entry is locked for a while.
const PIN_TRIES: u32 = 5;
const PIN_LOCKOUT: Duration = Duration::from_secs(5 * 60);

/// Stops PIN guessing: a few tries, then a lockout for everyone (failures, lockout end).
#[derive(Default)]
struct PinGuard(Mutex<(u32, Option<Instant>)>);

impl PinGuard {
    fn check(&self, pin: &PinHash, entered: &str) -> Result<(), String> {
        let mut state = self.0.lock().unwrap();
        if let Some(until) = state.1 {
            let left = until.saturating_duration_since(Instant::now());
            if !left.is_zero() {
                return Err(format!(
                    "слишком много неверных PIN-кодов, попробуйте через {} мин",
                    left.as_secs() / 60 + 1
                ));
            }
            *state = (0, None);
        }
        if pin.matches(entered) {
            state.0 = 0;
            return Ok(());
        }
        state.0 += 1;
        if state.0 >= PIN_TRIES {
            state.1 = Some(Instant::now() + PIN_LOCKOUT);
            return Err(format!("неверный PIN-код; вход заблокирован на {} мин", PIN_LOCKOUT.as_secs() / 60));
        }
        Err(format!("неверный PIN-код, осталось попыток: {}", PIN_TRIES - state.0))
    }
}

// Manual impl so the access secret never ends up in logs.
impl std::fmt::Debug for Handler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handler").field("id", &self.id).finish_non_exhaustive()
    }
}

impl ProtocolHandler for Handler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let client_id = conn.remote_id().to_string();
        if let Err(e) = self.handle(conn).await {
            tracing::warn!(%client_id, "session ended with error: {e:#}");
        }
        Ok(())
    }
}

impl Handler {
    async fn handle(&self, conn: Connection) -> Result<()> {
        let client_id = conn.remote_id();
        let cid = client_id.to_string();
        let (mut send, mut recv) = conn.accept_bi().await?;

        let ClientMsg::Hello { version, client_name, profile, monitor, codecs } = proto::read_msg(&mut recv).await?
        else {
            bail!("expected Hello");
        };
        let nonce = config::random_bytes();
        proto::write_msg(&mut send, &HostMsg::Challenge { nonce, host_name: self.cfg.name.clone() }).await?;
        let ClientMsg::Auth { mac } = proto::read_msg(&mut recv).await? else {
            bail!("expected Auth");
        };

        let mac = blake3::Hash::from_bytes(mac);
        // blake3::Hash equality is constant-time. Secrets are re-read, so a view-only ticket made after
        // the host started works at once.
        let current = HostConfig::load().ok().flatten();
        let full = current.as_ref().map_or(self.cfg.access_secret, |c| c.access_secret);
        let view = current.as_ref().and_then(|c| c.view_secret);
        let view_only = view.is_some_and(|v| proto::auth_mac(&v, &nonce, client_id, self.id) == mac);
        let full_ok = proto::auth_mac(&full, &nonce, client_id, self.id) == mac;
        let mut reject = if !full_ok && !view_only {
            Some("неверный ключ доступа".to_string())
        } else if version != PROTOCOL_VERSION {
            Some("несовместимая версия протокола".to_string())
        } else {
            None
        };
        // Re-read on every connection, so a new PIN applies without restarting the host.
        let pin = current.and_then(|c| c.pin);
        if reject.is_none()
            && let Some(pin) = pin
        {
            proto::write_msg(&mut send, &HostMsg::PinRequired).await?;
            let ClientMsg::Pin(entered) = proto::read_msg(&mut recv).await? else {
                bail!("expected Pin");
            };
            reject = self.pin_guard.check(&pin, &entered).err();
        }
        if let Some(reason) = reject {
            config::journal(&cid, &client_name, "auth_failed", &reason);
            tokio::time::sleep(Duration::from_secs(2)).await;
            proto::write_msg(&mut send, &HostMsg::AuthResult { ok: false, reason }).await?;
            send.finish()?;
            let _ = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
            return Ok(());
        }
        proto::write_msg(&mut send, &HostMsg::AuthResult { ok: true, reason: String::new() }).await?;

        if profile == Profile::Control {
            let result = control(&mut send, &mut recv, view_only).await;
            config::journal(
                &cid,
                &client_name,
                "control",
                &result.as_ref().err().cloned().unwrap_or_else(|| "ok".into()),
            );
            proto::write_msg(&mut send, &HostMsg::Done(result)).await?;
            send.finish()?;
            let _ = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
            return Ok(());
        }
        let kind_name = if view_only { "view" } else { profile.name() };
        config::journal(&cid, &client_name, "connected", kind_name);
        let started = Instant::now();
        let result = session::serve(&conn, profile, monitor, codecs, view_only, send, recv).await;
        let detail = match &result {
            Ok(()) => format!("{kind_name}, {} с", started.elapsed().as_secs()),
            Err(e) => format!("{kind_name}, {} с, {e:#}", started.elapsed().as_secs()),
        };
        config::journal(&cid, &client_name, "disconnected", &detail);
        conn.close(0u32.into(), b"bye");
        Ok(())
    }
}
