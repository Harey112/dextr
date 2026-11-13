//! Checking GitHub Releases for a newer dextr, and installing it where the
//! app can update itself (macOS). Elsewhere it links to the release.

use std::{io::Write as _, path::PathBuf, time::Duration};

use anyhow::{Context as _, bail};
use serde::Deserialize;

const LATEST: &str = "https://api.github.com/repos/Harey112/dextr/releases/latest";
/// Larger downloads are refused.
const MAX_DOWNLOAD: u64 = 300 * 1024 * 1024;

pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A newer release.
#[derive(Debug, Clone)]
pub struct Release {
    pub version: String,
    pub page: String,
    /// The file this OS installs from, if the release has one.
    pub package: Option<String>,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .user_agent(format!("dextr/{}", current()))
        .http_status_as_error(false)
        .build()
        .into()
}

/// `1.10.0` > `1.9.3`; a leading `v` and suffixes like `-beta` are ignored.
fn parse(version: &str) -> Vec<u64> {
    version
        .trim_start_matches('v')
        .split(['.', '-', '+'])
        .take(3)
        .map(|p| p.chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0))
        .collect()
}

fn newer(candidate: &str, than: &str) -> bool {
    parse(candidate) > parse(than)
}

/// The latest release if it's newer than this copy.
fn latest() -> anyhow::Result<Option<Release>> {
    latest_from(LATEST, current())
}

fn latest_from(url: &str, current: &str) -> anyhow::Result<Option<Release>> {
    let mut response = agent().get(url).header("Accept", "application/vnd.github+json").call()?;
    match response.status().as_u16() {
        200 => {}
        404 => return Ok(None), // no releases yet
        403 | 429 => bail!("GitHub is limiting requests; try again later"),
        code => bail!("GitHub answered {code}"),
    }
    let release: GhRelease = response.body_mut().read_json()?;
    let version = release.tag_name.trim_start_matches('v').to_owned();
    if !newer(&version, current) {
        return Ok(None);
    }
    let suffix = if cfg!(target_os = "macos") { Some("-macos.dmg") } else { None };
    let package = suffix.and_then(|s| release.assets.iter().find(|a| a.name.ends_with(s))).map(|a| a.browser_download_url.clone());
    Ok(Some(Release { version, page: release.html_url, package }))
}

pub async fn check() -> Result<Option<Release>, String> {
    tokio::task::spawn_blocking(latest).await.map_err(|e| e.to_string())?.map_err(|e| format!("{e:#}"))
}

fn download(url: &str) -> anyhow::Result<PathBuf> {
    let dir = dextr_platform::update::download_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("dextr-update.dmg");
    let mut response = agent().get(url).call()?;
    if !response.status().is_success() {
        bail!("download failed: {}", response.status());
    }
    let mut reader = response.body_mut().with_config().limit(MAX_DOWNLOAD).reader();
    let mut file = std::fs::File::create(&path)?;
    std::io::copy(&mut reader, &mut file).context("downloading")?;
    file.flush()?;
    Ok(path)
}

/// Downloads and installs `release`; the app should quit afterwards so the
/// new version starts.
pub async fn install(release: Release) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let url = release.package.as_deref().context("this release has nothing to install here")?;
        let path = download(url)?;
        dextr_platform::update::install(&path)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))
}

/// Whether "Install" is offered, rather than a link to the release.
pub fn can_install(release: &Release) -> bool {
    release.package.is_some() && dextr_platform::update::can_install()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(newer("v0.10.0", "0.9.9"));
        assert!(newer("1.0.0", "0.99.0"));
        assert!(!newer("v0.1.0", "0.1.0"));
        assert!(!newer("0.1.0-beta", "0.1.0"));
        assert!(newer("0.1.1", "0.1.0"));
    }
}

#[cfg(test)]
mod live {
    #[test]
    #[ignore = "network"]
    fn asks_github() {
        println!("{:?}", super::latest());
        // A repository with releases: an old version sees the update.
        let other = super::latest_from("https://api.github.com/repos/BurntSushi/ripgrep/releases/latest", "0.0.1");
        println!("{other:?}");
        assert!(other.unwrap().is_some());
    }
}
