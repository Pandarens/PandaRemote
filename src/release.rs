//! GitHub releases, shared by the desktop self-update and the Android app's update check.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use serde::Deserialize;

pub const REPO: &str = "Pandarens/PandaRemote";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u32, u32, u32);

impl Version {
    /// This build of the shared library (the desktop program and the Android app share the version).
    pub fn current() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).expect("crate version is semver")
    }

    pub fn parse(s: &str) -> Option<Version> {
        let mut parts = s.trim().trim_start_matches('v').split('.').map(|p| p.parse::<u32>().ok());
        Some(Version(parts.next()??, parts.next()??, parts.next()??))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The latest release: its version and download links by file name.
pub struct Latest {
    pub version: Version,
    pub assets: Vec<(String, String)>,
}

impl Latest {
    pub fn asset(&self, name: &str) -> Option<&str> {
        self.assets.iter().find(|(n, _)| n == name).map(|(_, url)| url.as_str())
    }
}

/// HTTPS client with built-in root certificates (no system store needed, works on Android too).
pub fn http() -> Result<reqwest::Client> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .user_agent(concat!("panda-rc/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(300))
        .build()?)
}

/// The latest GitHub release, or `None` when there is none yet.
pub async fn latest() -> Result<Option<Latest>> {
    #[derive(Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
    }
    #[derive(Deserialize)]
    struct GhRelease {
        tag_name: String,
        assets: Vec<Asset>,
    }
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = http()?.get(url).send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let rel: GhRelease = resp.error_for_status()?.json().await?;
    let version = Version::parse(&rel.tag_name).with_context(|| format!("bad release tag {}", rel.tag_name))?;
    let assets = rel.assets.into_iter().map(|a| (a.name, a.browser_download_url)).collect();
    Ok(Some(Latest { version, assets }))
}
