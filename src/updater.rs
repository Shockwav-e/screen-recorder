//! Crabby self-updater — GitHub Releases, no reinstall needed.
//!
//! How it works:
//!   1. `check_for_update()` asks the GitHub API for the latest release
//!      (`v1.2.3` tag) and compares it to this build's `CARGO_PKG_VERSION`.
//!   2. `download_update()` streams the release asset
//!      (`crabby-windows-x86_64.exe`, published by `.github/workflows/release.yml`)
//!      next to the running exe with byte-progress callbacks.
//!   3. `install_and_restart()` drops a tiny `.bat` in %TEMP% that waits for
//!      this process to exit (Windows can't overwrite a running exe), swaps
//!      starts the app with `--updated-from <old>` → deletes itself.
//!      The user just sees the app restart on the new version.
//!
//! Publishing a new version (maintainer): bump `version` in Cargo.toml,
//! commit, then `git tag v1.2.3` + `git push --tags`. The release workflow
//! builds and attaches the exe; every installed copy updates itself.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};

/// GitHub repo that hosts the releases.
pub const OWNER: &str = "Shockwav-e";
pub const REPO: &str = "screen-recorder";
/// Exact asset file attached to each release by the release workflow.
pub const ASSET_NAME: &str = "crabby-windows-x86_64.exe";
/// How often the GUI auto-checks in the background.
pub const AUTO_CHECK_EVERY: Duration = Duration::from_secs(24 * 3600);

fn api_url() -> String {
    format!("https://api.github.com/repos/{OWNER}/{REPO}/releases/latest")
}

fn user_agent() -> String {
    format!("crabby/{}", env!("CARGO_PKG_VERSION"))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .into()
}

/// This build's version.
pub fn current_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(semver::Version::new(0, 0, 0))
}

/// A newer release found on GitHub.
#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    /// Tag as published, e.g. `v1.2.3`.
    pub tag: String,
    /// Version without the leading `v`.
    pub version: semver::Version,
    /// Release notes (markdown) for the "What's new" dialog.
    pub notes: String,
    /// Direct download URL of [`ASSET_NAME`].
    pub download_url: String,
    /// Total bytes (0 when the API didn't report a size).
    pub bytes: u64,
}

fn strip_v(tag: &str) -> &str {
    tag.strip_prefix(['v', 'V']).unwrap_or(tag)
}

/// Ask GitHub for the latest release. Returns `Ok(None)` when we're already
/// on the newest version (or newer, e.g. a dev build).
pub fn check_for_update() -> Result<Option<ReleaseInfo>> {
    let mut resp = agent()
        .get(&api_url())
        .header("User-Agent", &user_agent())
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| match &e {
            ureq::Error::StatusCode(404) => anyhow::anyhow!(
                "no releases published yet — ask the maintainer to push a v* tag"
            ),
            ureq::Error::StatusCode(403) => {
                anyhow::anyhow!("GitHub rate limit hit (60/hr anonymous) — try again later")
            }
            _ => anyhow::anyhow!("update check failed: {e}"),
        })?;

    let v: serde_json::Value = resp
        .body_mut()
        .read_json()
        .context("couldn't parse GitHub release info")?;
    let tag = v
        .get("tag_name")
        .and_then(|t| t.as_str())
        .context("release has no tag_name")?
        .to_owned();
    let notes = v
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or("(no release notes)")
        .to_owned();
    let latest = semver::Version::parse(strip_v(&tag))
        .with_context(|| format!("bad release tag {tag:?}"))?;

    if latest <= current_version() {
        return Ok(None);
    }

    let assets = v
        .get("assets")
        .and_then(|a| a.as_array())
        .context("release has no assets list")?;
    let mut download_url = None;
    let mut bytes = 0u64;
    for a in assets {
        if a.get("name").and_then(|n| n.as_str()) == Some(ASSET_NAME) {
            download_url = a
                .get("browser_download_url")
                .and_then(|u| u.as_str())
                .map(|s| s.to_owned());
            bytes = a.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
            break;
        }
    }
    let Some(download_url) = download_url else {
        bail!("release {tag} has no {ASSET_NAME} asset yet (build still running?)")
    };

    Ok(Some(ReleaseInfo { tag, version: latest, notes, download_url, bytes }))
}

/// Stream the update next to the running exe.
/// `on_progress(downloaded, total)` is called per chunk (total may be 0).
/// Returns the staged file path.
pub fn download_update(
    info: &ReleaseInfo,
    on_progress: &dyn Fn(u64, u64),
) -> Result<PathBuf> {
    let dest = staged_path(info)?;
    if let Some(par) = dest.parent() {
        std::fs::create_dir_all(par).context("couldn't create update folder")?;
    }

    let mut resp = agent()
        .get(&info.download_url)
        .header("User-Agent", &user_agent())
        .header("Accept", "application/octet-stream")
        .call()
        .map_err(|e| anyhow::anyhow!("update download failed: {e}"))?;
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(info.bytes);

    let mut out = std::fs::File::create(&dest)
        .with_context(|| format!("couldn't write {}", dest.display()))?;
    let mut reader = resp.body_mut().as_reader();
    let mut buf = [0u8; 64 * 1024];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf).context("update download broke")?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).context("couldn't save update")?;
        done += n as u64;
        on_progress(done, total);
    }
    out.flush().ok();
    drop(out);

    // Sanity: an exe smaller than ~1 MB is not our app (error page etc.).
    let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    if size < 1_000_000 {
        let _ = std::fs::remove_file(&dest);
        bail!("downloaded file is suspiciously small ({size} bytes) — release asset broken?");
    }
    Ok(dest)
}

/// Where the staged download lives: next to the running exe so the swap is
/// a same-folder rename (atomic, no cross-drive moves).
fn staged_path(info: &ReleaseInfo) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("couldn't locate running exe")?;
    let dir = exe.parent().context("exe has no parent folder")?;
    Ok(dir.join(format!("crabby-{}.pending.exe", info.version)))
}

/// Swap the staged file into place and restart.
///
/// Windows locks the running exe, so this writes a small `.bat` that:
/// waits for our PID to vanish → moves the new exe over the old one →
/// starts the app with `--updated-from <old>` → deletes itself.
/// Then this process exits; never returns on success.
pub fn install_and_restart(staged: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("couldn't locate running exe")?;
    let pid = std::process::id();
    let from = current_version().to_string();
    let bat = std::env::temp_dir().join(format!("crabby-update-{pid}.bat"));
    let script = format!(
        "@echo off\r\n\
        :wait\r\n\
        tasklist /FI \"PID eq {pid}\" 2>nul | find \"{pid}\" >nul && (timeout /t 1 /nobreak >nul & goto wait)\r\n\
        move /y \"{staged}\" \"{exe}\" >nul\r\n\
        start \"\" \"{exe}\" --updated-from {from}\r\n\
        (goto) 2>nul & del \"%~f0\"\r\n",
        pid = pid,
        staged = staged.display(),
        exe = exe.display(),
        from = from,
    );
    std::fs::write(&bat, script).context("couldn't write updater script")?;
    std::process::Command::new("cmd")
        .args(["/C", "start", "/min", "", &bat.to_string_lossy()])
        .spawn()
        .context("couldn't launch updater")?;
    std::process::exit(0);
}

// ---------- auto-check state (one background check per day) ----------

fn state_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| {
        PathBuf::from(a)
            .join("Crabby")
            .join("updater.json")
    })
}

/// True when no successful check ran in the last 24 h (or never).
pub fn should_auto_check() -> bool {
    let Some(p) = state_path() else { return true };
    let Ok(txt) = std::fs::read_to_string(&p) else { return true };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else { return true };
    let last = v.get("last_check").and_then(|n| n.as_u64()).unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.saturating_sub(last) >= AUTO_CHECK_EVERY.as_secs()
}

/// Remember a check so we don't phone home on every launch.
pub fn mark_checked() {
    let Some(p) = state_path() else { return };
    if let Some(par) = p.parent() {
        let _ = std::fs::create_dir_all(par);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = std::fs::write(&p, format!("{{\"last_check\":{now}}}"));
}
