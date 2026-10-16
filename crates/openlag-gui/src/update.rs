//! Updates from GitHub Releases.
//!
//! The latest release is checked on start and every 6 hours. Updating downloads the
//! release's `Pingless-Setup-*.exe`, verifies its size and SHA-256 (GitHub publishes a
//! digest per asset), and runs it with `--update`: the installer reinstalls into the
//! same folder with the same options and starts Pingless again.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter};

pub const REPO: &str = "Del-Iternenko/pingless";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub available: bool,
    pub notes: String,
    pub url: Option<String>,
    pub size: u64,
    pub sha256: Option<String>,
    pub page: String,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    body: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(format!("Pingless/{CURRENT}"))
        .timeout(std::time::Duration::from_secs(30))
        .build()?)
}

/// `v1.2.3` / `1.2.3` -> (1, 2, 3). Anything after a `-` is ignored.
pub fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let core = v.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u32>().ok());
    Some((
        it.next()??,
        it.next().flatten().unwrap_or(0),
        it.next().flatten().unwrap_or(0),
    ))
}

pub async fn check() -> Result<UpdateInfo> {
    let rel: Release = client()?
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("GitHub is unreachable")?
        .error_for_status()
        .context("no published release yet")?
        .json()
        .await?;
    let latest = rel.tag_name.trim_start_matches(['v', 'V']).to_string();
    let newer = match (parse_version(&latest), parse_version(CURRENT)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    };
    let asset = rel.assets.iter().find(|a| {
        a.name.to_lowercase().starts_with("pingless-setup")
            && a.name.to_lowercase().ends_with(".exe")
    });
    Ok(UpdateInfo {
        current: CURRENT.into(),
        available: newer && asset.is_some() && !rel.draft && !rel.prerelease,
        latest,
        notes: rel.body,
        url: asset.map(|a| a.browser_download_url.clone()),
        size: asset.map(|a| a.size).unwrap_or(0),
        sha256: asset
            .and_then(|a| a.digest.as_deref())
            .and_then(|d| d.strip_prefix("sha256:"))
            .map(str::to_lowercase),
        page: rel.html_url,
    })
}

/// Downloads and verifies the installer, emitting `update-progress` (0..100).
pub async fn download(app: &AppHandle, info: &UpdateInfo) -> Result<PathBuf> {
    let url = info
        .url
        .as_deref()
        .context("the release has no installer")?;
    anyhow::ensure!(
        url.starts_with("https://github.com/"),
        "unexpected download host"
    );
    let resp = client()?.get(url).send().await?.error_for_status()?;
    let total = resp.content_length().unwrap_or(info.size).max(1);
    let path = std::env::temp_dir().join(format!("Pingless-Setup-{}.exe", info.latest));
    let mut file = tokio::fs::File::create(&path).await?;
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let mut stream = resp.bytes_stream();
    let mut last_pct = -1i64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
        done += chunk.len() as u64;
        let pct = (done * 100 / total) as i64;
        if pct != last_pct {
            last_pct = pct;
            let _ = app.emit("update-progress", pct);
        }
    }
    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    drop(file);
    if info.size > 0 && done != info.size {
        bail!("download is incomplete ({done} of {} bytes)", info.size);
    }
    if let Some(want) = &info.sha256 {
        let got = format!("{:x}", hasher.finalize());
        if &got != want {
            let _ = std::fs::remove_file(&path);
            bail!("installer checksum mismatch, update cancelled");
        }
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.0"), Some((1, 0, 0)));
        assert_eq!(parse_version("2.0.0-beta.1"), Some((2, 0, 0)));
        assert!(parse_version("v1.0.10") > parse_version("v1.0.9"));
        assert_eq!(parse_version("latest"), None);
    }
}
