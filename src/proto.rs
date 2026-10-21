//! Wire protocol: messages, length-prefixed framing and the auth MAC.

use anyhow::{Result, bail};
use iroh::{
    Endpoint, EndpointId, SecretKey,
    endpoint::{QuicTransportConfig, RecvStream, SendStream, presets},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Single ALPN; the session kind is chosen in [`ClientMsg::Hello`].
pub const ALPN: &[u8] = b"panda-rc/1";
pub const PROTOCOL_VERSION: u32 = 1;
const MAX_FRAME: usize = 32 * 1024 * 1024;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// Built-in remote desktop (screen + mouse/keyboard).
    Desktop,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ClientMsg {
    Hello {
        version: u32,
        client_name: String,
        kind: SessionKind,
    },
    Auth {
        mac: [u8; 32],
    },
    Input(InputEvent),
    RequestKeyframe,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum HostMsg {
    Challenge { nonce: [u8; 32], host_name: String },
    AuthResult { ok: bool, reason: String },
    ScreenInfo { width: u32, height: u32 },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum InputEvent {
    /// Position normalized to 0..=65535 over the streamed frame.
    MouseMove { x: u16, y: u16 },
    MouseButton { button: MouseButton, down: bool },
    /// Wheel notches; positive `dy` scrolls down, positive `dx` scrolls right.
    Wheel { dx: i32, dy: i32 },
    Key { key: KeyCode, down: bool, mods: Modifiers },
    Text(String),
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub enum KeyCode {
    Enter,
    Tab,
    Backspace,
    Escape,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    Space,
    F(u8),
    /// Printable key pressed together with Ctrl/Alt/Win (shortcuts); plain typing goes via `Text`.
    Char(char),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub keyframe: bool,
    /// H.264 Annex B bitstream.
    pub data: Vec<u8>,
}

/// Endpoint with the given identity, n0 address lookup + relays, and buffers sized for streaming.
pub async fn bind_endpoint(key: SecretKey) -> Result<Endpoint> {
    let transport = QuicTransportConfig::builder()
        .datagram_receive_buffer_size(Some(8 * 1024 * 1024))
        .datagram_send_buffer_size(8 * 1024 * 1024)
        .build();
    Ok(Endpoint::builder(presets::N0)
        .secret_key(key)
        .transport_config(transport)
        .bind()
        .await?)
}

pub async fn write_msg<T: Serialize>(send: &mut SendStream, msg: &T) -> Result<()> {
    let body = postcard::to_stdvec(msg)?;
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    send.write_all(&buf).await?;
    Ok(())
}

pub async fn read_msg<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<T> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        bail!("frame too large: {len}");
    }
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await?;
    Ok(postcard::from_bytes(&body)?)
}

/// MAC proving knowledge of the access secret, bound to both endpoint identities.
pub fn auth_mac(secret: &[u8; 32], nonce: &[u8; 32], client: EndpointId, host: EndpointId) -> blake3::Hash {
    let mut h = blake3::Hasher::new_keyed(secret);
    h.update(b"panda-rc auth v1");
    h.update(nonce);
    h.update(client.as_bytes());
    h.update(host.as_bytes());
    h.finalize()
}
