//! Wire protocol: messages, length-prefixed framing and the auth MAC.

use anyhow::{Result, bail};
use iroh::{
    Endpoint, EndpointId, SecretKey,
    endpoint::{QuicTransportConfig, RecvStream, SendStream, presets},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Single ALPN; the stream profile is chosen in [`ClientMsg::Hello`].
pub const ALPN: &[u8] = b"panda-rc/2";
pub const PROTOCOL_VERSION: u32 = 4;
/// Presence check: the host completes the handshake and closes, nothing else (no auth, no data).
pub const PING_ALPN: &[u8] = b"panda-rc/ping";
const MAX_FRAME: usize = 32 * 1024 * 1024;

/// Video codec of the stream. Newer codecs need fewer bits for the same picture, but both sides need
/// hardware (or system) support for them, so the client lists what it decodes and the host picks.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
            Codec::Av1 => "AV1",
        }
    }

    /// Best first: the order the host tries them in. AV1 is left out: on the hardware tested, its encoder is
    /// too slow for live video (45 ms per 1080p frame) and its output did not decode with Windows' decoder.
    pub const PREFERENCE: [Codec; 2] = [Codec::Hevc, Codec::H264];

    pub fn parse(s: &str) -> Option<Codec> {
        match s.to_ascii_lowercase().as_str() {
            "h264" | "h.264" | "avc" => Some(Codec::H264),
            "hevc" | "h265" | "h.265" => Some(Codec::Hevc),
            "av1" => Some(Codec::Av1),
            _ => None,
        }
    }
}

/// Streaming profile: picks frame rate, bitrate range and input model.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Remote desktop: up to 30 fps, absolute mouse, text input.
    Desktop,
    /// Games: 60 fps, adaptive high bitrate, relative mouse, scancodes, gamepad.
    Game,
    /// No picture: one-off requests (e.g. Wake-on-LAN) after authentication.
    Control,
}

impl Profile {
    pub fn fps(self) -> u32 {
        match self {
            Profile::Desktop | Profile::Control => 30,
            Profile::Game => 60,
        }
    }

    /// (start, min, max) bitrate in bits per second.
    pub fn bitrate(self) -> (u32, u32, u32) {
        match self {
            Profile::Desktop | Profile::Control => (8_000_000, 2_000_000, 15_000_000),
            Profile::Game => (20_000_000, 5_000_000, 50_000_000),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Profile::Desktop => "desktop",
            Profile::Game => "game",
            Profile::Control => "control",
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ClientMsg {
    Hello {
        version: u32,
        client_name: String,
        profile: Profile,
        /// Which monitor to stream (index into `StreamInfo::monitors`); `None` = the primary one.
        monitor: Option<u8>,
        /// Codecs the viewer can decode; the host streams the best one it can encode.
        codecs: Vec<Codec>,
    },
    Auth {
        mac: [u8; 32],
    },
    Input(InputEvent),
    Gamepad(GamepadState),
    /// Drawing over the host's screen (shown to the remote user, fades by itself).
    Draw(crate::media::annotate::DrawEvent),
    RequestKeyframe,
    /// Sent once a second: frames assembled and frames lost since the previous report.
    Stats {
        frames: u32,
        lost: u32,
    },
    /// Answer to `HostMsg::PinRequired`.
    Pin(String),
    /// Press Ctrl+Alt+Del on the host (works when the Windows service is installed).
    SecureAttention,
    /// Text the viewer copied; the host puts it on its clipboard.
    Clipboard(String),
    /// A short note shown on the host's screen for a few seconds.
    Message(String),
    /// Control profile: send a Wake-on-LAN packet for `mac` on this host's networks.
    Wake {
        mac: [u8; 6],
    },
}

/// First message on a unidirectional stream the viewer opens to send a file; the file's bytes follow.
#[derive(Serialize, Deserialize, Debug)]
pub struct FileHeader {
    pub name: String,
    pub size: u64,
}

/// Largest file accepted, and the largest clipboard text synced.
pub const MAX_FILE: u64 = 4 * 1024 * 1024 * 1024;
pub const MAX_CLIPBOARD: usize = 1024 * 1024;

#[derive(Serialize, Deserialize, Debug)]
pub enum HostMsg {
    Challenge {
        nonce: [u8; 32],
        host_name: String,
    },
    AuthResult {
        ok: bool,
        reason: String,
    },
    /// The ticket was right, but this PC also wants its PIN (answer: `ClientMsg::Pin`).
    PinRequired,
    StreamInfo {
        width: u32,
        height: u32,
        fps: u32,
        encoder: String,
        audio: bool,
        /// `None` when a virtual gamepad is available, otherwise why not.
        gamepad_error: Option<String>,
        /// The host's monitors, as labels for the viewer to pick from.
        monitors: Vec<String>,
        /// Index of the streamed one in `monitors`.
        monitor: u8,
        codec: Codec,
        /// The viewer came with the view-only ticket: input, clipboard and files are ignored.
        view_only: bool,
        /// The host's network card addresses, so it can be woken over the LAN later.
        macs: Vec<[u8; 6]>,
    },
    /// Force feedback the game set on the virtual controller (0 = off).
    Rumble {
        large: u8,
        small: u8,
    },
    /// The host shows the lock screen / a UAC prompt (a secure desktop) or is back on the normal desktop.
    SecureDesktop(bool),
    /// Text copied on the host.
    Clipboard(String),
    /// A file the viewer sent: where it was saved, or why not.
    FileSaved {
        name: String,
        result: Result<String, String>,
    },
    /// Answer to a control-profile request.
    Done(Result<(), String>),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum InputEvent {
    /// Position normalized to 0..=65535 over the streamed frame.
    MouseMove {
        x: u16,
        y: u16,
    },
    /// Raw relative motion (game profile, pointer captured).
    MouseRelative {
        dx: i32,
        dy: i32,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    /// Wheel notches; positive `dy` scrolls down, positive `dx` scrolls right.
    Wheel {
        dx: i32,
        dy: i32,
    },
    Key {
        key: KeyCode,
        down: bool,
        mods: Modifiers,
    },
    /// Physical key by PC/AT set-1 scancode (game profile).
    Scancode {
        code: u16,
        extended: bool,
        down: bool,
    },
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

/// Controller state in XInput layout.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GamepadState {
    /// `XINPUT_GAMEPAD_*` button bits.
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub thumb_lx: i16,
    pub thumb_ly: i16,
    pub thumb_rx: i16,
    pub thumb_ry: i16,
}

pub mod buttons {
    pub const DPAD_UP: u16 = 0x0001;
    pub const DPAD_DOWN: u16 = 0x0002;
    pub const DPAD_LEFT: u16 = 0x0004;
    pub const DPAD_RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LEFT_THUMB: u16 = 0x0040;
    pub const RIGHT_THUMB: u16 = 0x0080;
    pub const LEFT_SHOULDER: u16 = 0x0100;
    pub const RIGHT_SHOULDER: u16 = 0x0200;
    pub const GUIDE: u16 = 0x0400;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
}

/// Endpoint with the given identity, n0 address lookup + relays, and buffers sized for streaming.
pub async fn bind_endpoint(key: SecretKey) -> Result<Endpoint> {
    let transport = QuicTransportConfig::builder()
        .datagram_receive_buffer_size(Some(8 * 1024 * 1024))
        .datagram_send_buffer_size(8 * 1024 * 1024)
        .build();
    Ok(Endpoint::builder(presets::N0).secret_key(key).transport_config(transport).bind().await?)
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
    h.update(b"panda-rc auth v2");
    h.update(nonce);
    h.update(client.as_bytes());
    h.update(host.as_bytes());
    h.finalize()
}
