//! Local storage: keys, host settings, saved hosts, connection journal.
//! Everything lives in one directory (`%APPDATA%\PandaRC\config` or `$PANDA_HOME`).

use std::{
    fs,
    io::Write,
    path::PathBuf,
    time::SystemTime,
};

use anyhow::{Context, Result};
use iroh::SecretKey;
use serde::{Deserialize, Serialize};

pub fn home_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PANDA_HOME") {
        return PathBuf::from(dir);
    }
    directories::ProjectDirs::from("", "", "PandaRC")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".panda"))
}

fn path(name: &str) -> Result<PathBuf> {
    let dir = home_dir();
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(dir.join(name))
}

/// Loads a 32-byte secret key from `name`, generating it on first use.
pub fn load_or_create_key(name: &str) -> Result<SecretKey> {
    let p = path(name)?;
    if let Ok(bytes) = fs::read(&p) {
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("{} is corrupted", p.display()))?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    let key = SecretKey::generate();
    fs::write(&p, key.to_bytes())?;
    Ok(key)
}

pub fn host_key() -> Result<SecretKey> {
    load_or_create_key("host.key")
}

pub fn client_key() -> Result<SecretKey> {
    load_or_create_key("client.key")
}

pub fn random_bytes() -> [u8; 32] {
    // A fresh ed25519 secret is 32 bytes from the OS CSPRNG; reuse it to avoid another RNG dependency.
    SecretKey::generate().to_bytes()
}

pub fn machine_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "panda".into())
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
}

impl HostConfig {
    pub fn load_or_create() -> Result<Self> {
        let p = path("host.json")?;
        if let Ok(text) = fs::read_to_string(&p) {
            return serde_json::from_str(&text).with_context(|| format!("parse {}", p.display()));
        }
        let cfg = HostConfig {
            name: machine_name(),
            access_secret: random_bytes(),
        };
        cfg.save()?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        fs::write(path("host.json")?, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn rotate_secret(&mut self) -> Result<()> {
        self.access_secret = random_bytes();
        self.save()
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

// ---------- saved hosts (client side) ----------

#[derive(Serialize, Deserialize, Clone)]
pub struct SavedHost {
    pub name: String,
    pub ticket: String,
    #[serde(default)]
    pub last_connected: Option<String>,
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
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path("connections.log")?)?;
        writeln!(f, "{}", serde_json::to_string(&entry)?)?;
        Ok(())
    };
    if let Err(e) = write() {
        tracing::warn!("journal write failed: {e:#}");
    }
}

pub fn read_journal(last: usize) -> Vec<JournalEntry> {
    let Ok(text) = path("connections.log").and_then(|p| Ok(fs::read_to_string(p)?)) else {
        return Vec::new();
    };
    let mut entries: Vec<JournalEntry> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let skip = entries.len().saturating_sub(last);
    entries.drain(..skip);
    entries.reverse();
    entries
}
