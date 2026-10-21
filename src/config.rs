//! Local storage: keys, host settings, saved hosts, connection journal.
//!
//! Client files live in the user's directory (`%APPDATA%\PandaRC\config`). Host files (identity key,
//! access secret, journal) live there too, unless the Windows service is installed: then they move to
//! the machine-wide `%ProgramData%\PandaRC\config` that the service uses. `$PANDA_HOME` overrides both.

use std::{fs, io::Write, path::PathBuf, time::SystemTime};

use anyhow::{Context, Result};
use iroh::SecretKey;
use serde::{Deserialize, Serialize};

fn override_dir() -> Option<PathBuf> {
    std::env::var_os("PANDA_HOME").map(PathBuf::from)
}

/// Per-user directory (client identity, saved hosts).
pub fn user_dir() -> PathBuf {
    override_dir().unwrap_or_else(|| {
        directories::ProjectDirs::from("", "", "PandaRC")
            .map(|d| d.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".panda"))
    })
}

/// Machine-wide directory used by the Windows service.
pub fn machine_dir() -> PathBuf {
    let base = std::env::var_os("PROGRAMDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("PandaRC").join("config")
}

/// True when the Panda Remote Windows service is registered (readable by every user, unlike its files).
pub fn service_installed() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::{
            Win32::System::Registry::{HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegOpenKeyExW},
            core::w,
        };
        let mut key = HKEY::default();
        let found = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            w!("SYSTEM\\CurrentControlSet\\Services\\PandaRemote"),
            None,
            KEY_READ,
            &mut key,
        )
        .is_ok();
        if found {
            let _ = RegCloseKey(key);
        }
        found
    }
    #[cfg(not(windows))]
    false
}

/// Machine-wide logs: service and helper logs plus the service's connection journal. Readable by
/// every local user, unlike `machine_dir()` with the key and secret.
pub fn machine_logs_dir() -> PathBuf {
    machine_dir().parent().map(|p| p.join("logs")).unwrap_or_else(|| PathBuf::from("logs"))
}

/// Where the host's identity, secret and journal live.
pub fn host_dir() -> PathBuf {
    override_dir().unwrap_or_else(|| if service_installed() { machine_dir() } else { user_dir() })
}

fn in_dir(dir: PathBuf, name: &str) -> Result<PathBuf> {
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(dir.join(name))
}

fn path(name: &str) -> Result<PathBuf> {
    in_dir(user_dir(), name)
}

fn host_path(name: &str) -> Result<PathBuf> {
    in_dir(host_dir(), name)
}

/// The service keeps its journal with its logs, where ordinary users (the GUI) can read it.
fn journal_path() -> Result<PathBuf> {
    let dir = if host_dir() == machine_dir() { machine_logs_dir() } else { host_dir() };
    in_dir(dir, "connections.log")
}

/// Loads a 32-byte secret key from `p`, generating it on first use.
fn load_or_create_key(p: PathBuf) -> Result<SecretKey> {
    if let Ok(bytes) = fs::read(&p) {
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| anyhow::anyhow!("{} is corrupted", p.display()))?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    let key = SecretKey::generate();
    fs::write(&p, key.to_bytes())?;
    Ok(key)
}

pub fn host_key() -> Result<SecretKey> {
    load_or_create_key(host_path("host.key")?)
}

pub fn client_key() -> Result<SecretKey> {
    load_or_create_key(path("client.key")?)
}

/// Host files copied to the machine directory when the service is installed, so the ticket stays valid.
pub const HOST_FILES: [&str; 2] = ["host.key", "host.json"];

pub fn random_bytes() -> [u8; 32] {
    // A fresh ed25519 secret is 32 bytes from the OS CSPRNG; reuse it to avoid another RNG dependency.
    SecretKey::generate().to_bytes()
}

pub fn machine_name() -> String {
    std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "panda".into())
}

pub fn now_rfc3339() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

// ---------- host settings ----------

#[derive(Serialize, Deserialize, Clone)]
pub struct HostConfig {
    pub name: String,
    #[serde(with = "hex32")]
    pub access_secret: [u8; 32],
    /// Asked after the ticket when set: a ticket alone is then not enough to get in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<PinHash>,
    /// Secret of the view-only ticket (see the picture, no control), created when first asked for.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "hex32_opt")]
    pub view_secret: Option<[u8; 32]>,
}

impl HostConfig {
    pub fn load_or_create() -> Result<Self> {
        if let Some(cfg) = Self::load()? {
            return Ok(cfg);
        }
        let cfg = HostConfig { name: machine_name(), access_secret: random_bytes(), pin: None, view_secret: None };
        cfg.save()?;
        Ok(cfg)
    }

    /// The saved settings, without creating them.
    pub fn load() -> Result<Option<Self>> {
        let p = host_path("host.json")?;
        match fs::read_to_string(&p) {
            Ok(text) => Ok(Some(serde_json::from_str(&text).with_context(|| format!("parse {}", p.display()))?)),
            Err(_) => Ok(None),
        }
    }

    pub fn save(&self) -> Result<()> {
        fs::write(host_path("host.json")?, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// New secrets for both tickets: every ticket handed out stops working.
    pub fn rotate_secret(&mut self) -> Result<()> {
        self.access_secret = random_bytes();
        if self.view_secret.is_some() {
            self.view_secret = Some(random_bytes());
        }
        self.save()
    }

    /// The view-only secret, created (and saved) on first use.
    pub fn ensure_view_secret(&mut self) -> Result<[u8; 32]> {
        if let Some(s) = self.view_secret {
            return Ok(s);
        }
        let s = random_bytes();
        self.view_secret = Some(s);
        self.save()?;
        Ok(s)
    }

    /// Sets (or with `None` removes) the PIN.
    pub fn set_pin(&mut self, pin: Option<&str>) -> Result<()> {
        if let Some(pin) = pin {
            anyhow::ensure!(valid_pin(pin), "PIN-код — от 4 до 8 цифр");
        }
        self.pin = pin.map(PinHash::new);
        self.save()
    }
}

pub fn valid_pin(pin: &str) -> bool {
    (4..=8).contains(&pin.len()) && pin.bytes().all(|b| b.is_ascii_digit())
}

/// A PIN kept as a salted BLAKE3 hash.
#[derive(Serialize, Deserialize, Clone)]
pub struct PinHash {
    #[serde(with = "hex32")]
    salt: [u8; 32],
    #[serde(with = "hex32")]
    hash: [u8; 32],
}

impl PinHash {
    fn new(pin: &str) -> Self {
        let salt = random_bytes();
        PinHash { salt, hash: *blake3::keyed_hash(&salt, pin.as_bytes()).as_bytes() }
    }

    pub fn matches(&self, pin: &str) -> bool {
        // blake3::Hash equality is constant-time.
        blake3::keyed_hash(&self.salt, pin.as_bytes()) == blake3::Hash::from_bytes(self.hash)
    }
}

/// This user's own copy of the host files. The service runs on a copy of them made at install time, so
/// they give the ticket and the PIN state without administrator rights.
pub fn user_host() -> Option<(iroh::EndpointId, HostConfig)> {
    let dir = user_dir();
    let key: [u8; 32] = fs::read(dir.join("host.key")).ok()?.try_into().ok()?;
    let cfg = serde_json::from_str(&fs::read_to_string(dir.join("host.json")).ok()?).ok()?;
    Some((SecretKey::from_bytes(&key).public(), cfg))
}

mod hex32_opt {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<[u8; 32]>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(b) => s.serialize_str(&data_encoding::HEXLOWER.encode(b)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<[u8; 32]>, D::Error> {
        let Some(text) = Option::<String>::deserialize(d)? else { return Ok(None) };
        let bytes = data_encoding::HEXLOWER.decode(text.as_bytes()).map_err(serde::de::Error::custom)?;
        Ok(Some(bytes.try_into().map_err(|_| serde::de::Error::custom("expected 32 bytes"))?))
    }
}

mod hex32 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&data_encoding::HEXLOWER.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        data_encoding::HEXLOWER
            .decode(s.as_bytes())
            .ok()
            .and_then(|v| v.try_into().ok())
            .ok_or_else(|| serde::de::Error::custom("expected 64 hex chars"))
    }
}

// ---------- user settings ----------

#[derive(Serialize, Deserialize, Clone)]
pub struct Settings {
    /// Start Panda (minimized) when the user logs in to Windows.
    #[serde(default = "yes")]
    pub autostart: bool,
    /// The in-app host was on when the app last closed: turn it back on at start.
    #[serde(default)]
    pub host_enabled: bool,
}

fn yes() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings { autostart: true, host_enabled: false }
    }
}

pub fn load_settings() -> Settings {
    path("settings.json")
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_settings(settings: &Settings) -> Result<()> {
    fs::write(path("settings.json")?, serde_json::to_string_pretty(settings)?)?;
    Ok(())
}

// ---------- saved hosts (client side) ----------

#[derive(Serialize, Deserialize, Clone)]
pub struct SavedHost {
    pub name: String,
    pub ticket: String,
    #[serde(default)]
    pub last_connected: Option<String>,
    /// Network card addresses the host reported, for Wake-on-LAN ("70-85-C2-0A-1B-FF").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macs: Vec<String>,
}

pub fn load_hosts() -> Vec<SavedHost> {
    path("hosts.json")
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_hosts(hosts: &[SavedHost]) -> Result<()> {
    fs::write(path("hosts.json")?, serde_json::to_string_pretty(hosts)?)?;
    Ok(())
}

// ---------- connection journal (host side) ----------

#[derive(Serialize, Deserialize, Clone)]
pub struct JournalEntry {
    pub ts: String,
    pub client_id: String,
    #[serde(default)]
    pub client_name: String,
    pub event: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

pub fn journal(client_id: &str, client_name: &str, event: &str, detail: &str) {
    let entry = JournalEntry {
        ts: now_rfc3339(),
        client_id: client_id.into(),
        client_name: client_name.into(),
        event: event.into(),
        detail: detail.into(),
    };
    tracing::info!(client_id, client_name, event, detail, "journal");
    let write = || -> Result<()> {
        let mut f = fs::OpenOptions::new().create(true).append(true).open(journal_path()?)?;
        writeln!(f, "{}", serde_json::to_string(&entry)?)?;
        Ok(())
    };
    if let Err(e) = write() {
        tracing::warn!("journal write failed: {e:#}");
    }
}

pub fn read_journal(last: usize) -> Vec<JournalEntry> {
    let Ok(text) = journal_path().and_then(|p| Ok(fs::read_to_string(p)?)) else {
        return Vec::new();
    };
    let mut entries: Vec<JournalEntry> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let skip = entries.len().saturating_sub(last);
    entries.drain(..skip);
    entries.reverse();
    entries
}
