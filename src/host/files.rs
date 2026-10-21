//! Files the viewer sends: each arrives on its own unidirectional stream (header, then bytes) and is
//! saved to the signed-in user's Downloads\Panda folder.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use iroh::endpoint::RecvStream;
use tokio::io::AsyncWriteExt;

use crate::proto::{self, FileHeader, MAX_FILE};

/// Receives one file; returns its name and where it was saved. `allowed`: false for view-only viewers,
/// whose files are refused after reading the header (so the refusal can name the file).
pub async fn receive(mut recv: RecvStream, allowed: bool) -> (String, Result<String, String>) {
    let header: FileHeader = match proto::read_msg(&mut recv).await {
        Ok(h) => h,
        Err(e) => return (String::new(), Err(format!("{e:#}"))),
    };
    let name = header.name.clone();
    if !allowed {
        let _ = recv.stop(0u32.into());
        return (name, Err("билет для просмотра не даёт передавать файлы".into()));
    }
    let result = save(header, recv).await.map(|p| p.display().to_string()).map_err(|e| format!("{e:#}"));
    match &result {
        Ok(path) => tracing::info!(%name, %path, "file received"),
        Err(e) => tracing::warn!(%name, "file not saved: {e}"),
    }
    (name, result)
}

async fn save(header: FileHeader, mut recv: RecvStream) -> Result<PathBuf> {
    if header.size > MAX_FILE {
        bail!("файл больше {} ГБ", MAX_FILE >> 30);
    }
    let dir = downloads_dir()?.join("Panda");
    tokio::fs::create_dir_all(&dir).await.with_context(|| format!("create {}", dir.display()))?;
    let path = free_name(&dir, &safe_name(&header.name));
    let part = path.with_extension("part");
    let mut file = tokio::fs::File::create(&part).await.with_context(|| format!("create {}", part.display()))?;
    let mut left = header.size;
    let mut buf = vec![0u8; 256 * 1024];
    while left > 0 {
        let want = buf.len().min(left as usize);
        let Some(n) = recv.read(&mut buf[..want]).await? else {
            drop(file);
            let _ = tokio::fs::remove_file(&part).await;
            bail!("передача прервана");
        };
        file.write_all(&buf[..n]).await?;
        left -= n as u64;
    }
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&part, &path).await?;
    Ok(path)
}

/// Keeps only the file name part and characters Windows allows.
fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String =
        base.chars().map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c }).collect();
    let clean = clean.trim().trim_end_matches('.').to_string();
    if clean.is_empty() { "файл".into() } else { clean }
}

/// `dir/name`, or `dir/name (2).ext` and so on when it exists.
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    (2..).map(|i| dir.join(format!("{stem} ({i}){ext}"))).find(|p| !p.exists()).unwrap()
}

/// The Downloads folder of the user signed in to this session. The service's helper runs as SYSTEM, so it
/// asks for the folder with that user's token.
pub fn downloads_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::{
            Foundation::{CloseHandle, HANDLE},
            System::{
                Com::CoTaskMemFree,
                RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken},
                Threading::GetCurrentProcessId,
            },
            UI::Shell::{FOLDERID_Downloads, KNOWN_FOLDER_FLAG, SHGetKnownFolderPath},
        };
        let mut session = 0u32;
        let mut token = HANDLE::default();
        let user_token = ProcessIdToSessionId(GetCurrentProcessId(), &mut session).is_ok()
            && WTSQueryUserToken(session, &mut token).is_ok();
        let path = SHGetKnownFolderPath(&FOLDERID_Downloads, KNOWN_FOLDER_FLAG(0), user_token.then_some(token));
        if user_token {
            let _ = CloseHandle(token);
        }
        let path = path.context("папка «Загрузки»")?;
        let s = path.to_string()?;
        CoTaskMemFree(Some(path.0 as *const _));
        Ok(PathBuf::from(s))
    }
    #[cfg(not(windows))]
    Ok(directories::UserDirs::new()
        .and_then(|d| d.download_dir().map(Path::to_path_buf))
        .context("no downloads dir")?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitized() {
        assert_eq!(safe_name(r"C:\x\..\отчёт.pdf"), "отчёт.pdf");
        assert_eq!(safe_name("a:b?.txt"), "a_b_.txt");
        assert_eq!(safe_name("../.."), "файл");
        assert_eq!(safe_name("  "), "файл");
    }
}
