//! Self-update from GitHub releases.
//!
//! A release is `panda-rc.exe` plus `panda-rc.exe.sig`: an ed25519 signature over
//! `"panda-rc release <version>\n" || exe bytes`, made in CI with a key whose public half is built in
//! below. An update is installed only if the signature checks out for exactly that version, so neither
//! a tampered file nor an older signed build can be passed off as an update (the service applies
//! updates as SYSTEM).

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};

pub use crate::release::Version;
use crate::release::http;

/// Public half of the release signing key (the private half is the `PANDA_SIGNING_KEY` CI secret).
pub const RELEASE_PUBLIC_KEY: &str = "c58f7d00f79c68e436cfff2bea7890057cb6c26042e0f52e8a094f5f6215286b";
/// How often running copies look for a new release.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const EXE_ASSET: &str = "panda-rc.exe";
const SIG_ASSET: &str = "panda-rc.exe.sig";

pub struct Release {
    pub version: Version,
    exe_url: String,
    sig_url: String,
}

fn signed_message(version: Version, exe: &[u8]) -> Vec<u8> {
    let mut msg = format!("panda-rc release {version}\n").into_bytes();
    msg.extend_from_slice(exe);
    msg
}

/// Signs a release file (CI). The key is 64 hex chars (32-byte ed25519 secret).
pub fn sign(exe: &[u8], version: Version, secret_hex: &str) -> Result<String> {
    let bytes: [u8; 32] = data_encoding::HEXLOWER_PERMISSIVE
        .decode(secret_hex.trim().as_bytes())
        .ok()
        .and_then(|b| b.try_into().ok())
        .context("signing key must be 64 hex characters")?;
    let key = iroh::SecretKey::from_bytes(&bytes);
    Ok(data_encoding::HEXLOWER.encode(&key.sign(&signed_message(version, exe)).to_bytes()))
}

pub fn verify(exe: &[u8], version: Version, sig_hex: &str) -> Result<()> {
    let key: [u8; 32] = data_encoding::HEXLOWER_PERMISSIVE
        .decode(RELEASE_PUBLIC_KEY.as_bytes())
        .ok()
        .and_then(|b| b.try_into().ok())
        .context("no release key built in")?;
    let sig: [u8; 64] = data_encoding::HEXLOWER_PERMISSIVE
        .decode(sig_hex.trim().as_bytes())
        .ok()
        .and_then(|b| b.try_into().ok())
        .context("malformed signature")?;
    iroh::PublicKey::from_bytes(&key)?
        .verify(&signed_message(version, exe), &iroh::Signature::from_bytes(&sig))
        .map_err(|_| anyhow::anyhow!("подпись обновления не совпадает — файл отклонён"))
}

/// The latest GitHub release, if it is newer than this build.
pub async fn newer_release() -> Result<Option<Release>> {
    let Some(rel) = crate::release::latest().await? else { return Ok(None) };
    let version = rel.version;
    if version <= Version::current() {
        return Ok(None);
    }
    let asset = |name: &str| rel.asset(name).map(str::to_string);
    let (Some(exe_url), Some(sig_url)) = (asset(EXE_ASSET), asset(SIG_ASSET)) else {
        bail!("release {version} has no signed {EXE_ASSET}");
    };
    Ok(Some(Release { version, exe_url, sig_url }))
}

/// Where downloaded updates wait to be installed.
pub fn staging_dir(machine: bool) -> PathBuf {
    if machine {
        crate::config::machine_dir().parent().map(|p| p.join("update")).unwrap_or_else(|| PathBuf::from("update"))
    } else {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("PandaRC")
            .join("update")
    }
}

/// Downloads a release into `dir` and keeps it only if its signature is valid.
pub async fn download(rel: &Release, dir: &Path) -> Result<PathBuf> {
    let client = http()?;
    let exe = client.get(&rel.exe_url).send().await?.error_for_status()?.bytes().await?;
    let sig = client.get(&rel.sig_url).send().await?.error_for_status()?.text().await?;
    verify(&exe, rel.version, &sig)?;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("panda-rc-{}.exe", rel.version));
    std::fs::write(&path, &exe)?;
    std::fs::write(path.with_extension("exe.sig"), sig)?;
    Ok(path)
}

/// The newest verified update waiting in `dir`, if newer than this build.
pub fn staged(dir: &Path) -> Option<(Version, PathBuf)> {
    let mut best: Option<(Version, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(v) = name.strip_prefix("panda-rc-").and_then(|n| n.strip_suffix(".exe")).and_then(Version::parse)
        else {
            continue;
        };
        if v > Version::current() && best.as_ref().is_none_or(|(b, _)| v > *b) {
            best = Some((v, entry.path()));
        }
    }
    // Re-check right before use: the file sat on disk since it was downloaded.
    let (v, path) = best?;
    let exe = std::fs::read(&path).ok()?;
    let sig = std::fs::read_to_string(path.with_extension("exe.sig")).ok()?;
    verify(&exe, v, &sig).ok()?;
    Some((v, path))
}

/// Puts `new_exe` in place of `target`. A running program can't be overwritten but can be moved
/// aside, so the old file is renamed first; leftovers are cleaned up on the next update.
pub fn install(new_exe: &Path, target: &Path) -> Result<()> {
    let dir = target.parent().context("no parent dir")?;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with("panda-rc.old-") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    if target.exists() && std::fs::remove_file(target).is_err() {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        std::fs::rename(target, dir.join(format!("panda-rc.old-{stamp}.exe")))
            .with_context(|| format!("move {} aside", target.display()))?;
    }
    std::fs::copy(new_exe, target).with_context(|| format!("copy update to {}", target.display()))?;
    Ok(())
}

/// Starts the (possibly just replaced) executable again with the same arguments.
pub fn relaunch(extra: &[&str]) -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    for a in extra {
        if !args.iter().any(|x| x == a) {
            args.push(a.to_string());
        }
    }
    std::process::Command::new(std::env::current_exe()?).args(args).spawn()?;
    Ok(())
}

/// At program start: install a verified update downloaded earlier and restart into it.
/// Returns true when the caller should exit (the new version is starting).
pub fn apply_staged_on_start() -> bool {
    if !can_replace_self() {
        return false;
    }
    let Some((version, path)) = staged(&staging_dir(false)) else { return false };
    let applied = std::env::current_exe().map_err(anyhow::Error::from).and_then(|exe| install(&path, &exe));
    match applied {
        Ok(()) => {
            tracing::info!(%version, "update installed, restarting");
            relaunch(&[]).is_ok()
        }
        Err(e) => {
            tracing::warn!("could not install update {version}: {e:#}");
            false
        }
    }
}

/// Whether this process may replace its own executable (not the case in Program Files for a user).
pub fn can_replace_self() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let probe = exe.with_extension("write-test");
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(probe);
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        assert!(Version::parse("v0.10.0").unwrap() > Version::parse("0.9.3").unwrap());
        assert_eq!(Version::parse("1.2"), None);
    }

    #[test]
    fn signature_binds_version_and_bytes() {
        let secret = iroh::SecretKey::generate();
        let hex = data_encoding::HEXLOWER.encode(&secret.to_bytes());
        let v = Version::parse("1.2.3").unwrap();
        let sig = sign(b"exe", v, &hex).unwrap();
        let check = |exe: &[u8], v: Version| {
            let s: [u8; 64] = data_encoding::HEXLOWER.decode(sig.as_bytes()).unwrap().try_into().unwrap();
            secret.public().verify(&signed_message(v, exe), &iroh::Signature::from_bytes(&s)).is_ok()
        };
        assert!(check(b"exe", v));
        assert!(!check(b"exE", v), "tampered file");
        assert!(!check(b"exe", Version::parse("1.2.4").unwrap()), "replayed as another version");
    }
}
