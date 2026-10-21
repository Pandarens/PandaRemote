//! Host side: accepts connections addressed to this machine's key, authenticates them
//! and runs a streaming session.

mod audio;
mod capture;
#[cfg(windows)]
pub mod desktop_switch;
#[cfg(windows)]
mod dxgi;
pub mod encoder;
mod gamepad;
mod input;
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
    proto::{self, ClientMsg, HostMsg, PROTOCOL_VERSION},
    ticket::Ticket,
};

pub struct Host {
    router: Router,
    pub ticket: Ticket,
}

impl Host {
    pub async fn start(cfg: HostConfig) -> Result<Self> {
        input::set_dpi_aware();

        let endpoint = proto::bind_endpoint(config::host_key()?).await?;
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

        let ClientMsg::Hello { version, client_name, profile } = proto::read_msg(&mut recv).await? else {
            bail!("expected Hello");
        };
        let nonce = config::random_bytes();
        proto::write_msg(&mut send, &HostMsg::Challenge { nonce, host_name: self.cfg.name.clone() }).await?;
        let ClientMsg::Auth { mac } = proto::read_msg(&mut recv).await? else {
            bail!("expected Auth");
        };

        let expected = proto::auth_mac(&self.cfg.access_secret, &nonce, client_id, self.id);
        // blake3::Hash equality is constant-time.
        let mut reject = if expected != blake3::Hash::from_bytes(mac) {
            Some("неверный ключ доступа".to_string())
        } else if version != PROTOCOL_VERSION {
            Some("несовместимая версия протокола".to_string())
        } else {
            None
        };
        // Re-read on every connection, so a new PIN applies without restarting the host.
        let pin = HostConfig::load().ok().flatten().and_then(|c| c.pin);
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

        let kind_name = profile.name();
        config::journal(&cid, &client_name, "connected", kind_name);
        let started = Instant::now();
        let result = session::serve(&conn, profile, send, recv).await;
        let detail = match &result {
            Ok(()) => format!("{kind_name}, {} с", started.elapsed().as_secs()),
            Err(e) => format!("{kind_name}, {} с, {e:#}", started.elapsed().as_secs()),
        };
        config::journal(&cid, &client_name, "disconnected", &detail);
        conn.close(0u32.into(), b"bye");
        Ok(())
    }
}
