//! Optional TOML config for defaults, so everyday recording doesn't need
//! ten CLI flags. Precedence: CLI flag > config file > built-in default.
//!
//! Default location: `%APPDATA%/Crabby/config.toml` (same folder as the
//! updater state). `--config <path>` points elsewhere. A missing default
//! file is fine (built-ins apply); a present-but-unparseable file is an
//! error, not a silent ignore.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::recorder::{AudioCodec, AudioMode, Codec, Container, Tier};
use crate::shot::ShotFormat;

/// Flat TOML keys, all optional. Unknown keys are rejected to catch typos.
///
/// ```toml
/// dir = "D:/Videos/Crabby"
/// quality = "balanced"      # fastest | balanced | high-quality | lossless
/// codec = "h264"            # h264 | h264-qsv | ... | h265 | av1 | x264 | vp8 | vp9
/// container = "mp4"         # mp4 | mkv | webm
/// audio = "system"          # system | mic | both | off
/// audio_codec = "aac"       # opus | aac (must fit the container)
/// threads = 8               # omit = auto (CPU count)
/// shot_format = "png"       # png | webp | jpg | bmp (screenshot files)
/// shot_dir = "D:/Pics/Shots" # omit = Pictures/Crabby
/// shot_hotkeys = true       # global keys (Win+PrtSc etc.); omit = on
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub dir: Option<String>,
    pub quality: Option<Tier>,
    pub codec: Option<Codec>,
    pub container: Option<Container>,
    pub audio: Option<AudioMode>,
    pub audio_codec: Option<AudioCodec>,
    pub threads: Option<u32>,
    pub shot_format: Option<ShotFormat>,
    pub shot_dir: Option<String>,
    /// Global screenshot hotkeys (Win+PrtSc fullscreen, PrtSc region,
    /// Alt+PrtSc window). Default on; set false if another tool owns the keys.
    pub shot_hotkeys: Option<bool>,
}

/// Where the default config lives.
pub fn default_config_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(|a| PathBuf::from(a).join("Crabby").join("config.toml"))
}

/// Load config. `explicit`: `--config <path>` (missing file = error).
/// Otherwise the default path (missing file = empty config, not an error).
pub fn load(explicit: Option<&Path>) -> Result<FileConfig> {
    if let Some(p) = explicit {
        let txt = std::fs::read_to_string(p)
            .with_context(|| format!("couldn't read --config {}", p.display()))?;
        return toml::from_str(&txt)
            .with_context(|| format!("bad TOML in --config {}", p.display()));
    }
    let Some(p) = default_config_path() else {
        return Ok(FileConfig::default());
    };
    let Ok(txt) = std::fs::read_to_string(&p) else {
        return Ok(FileConfig::default()); // first run: no config yet
    };
    toml::from_str(&txt).with_context(|| format!("bad TOML in {}", p.display()))
}

/// Default save folder: the user's Videos known-folder, else the home
/// folder's Videos, else the current directory. Never a fixed drive letter.
pub fn default_video_dir() -> String {
    if let Some(v) = dirs::video_dir() {
        return v.to_string_lossy().into_owned();
    }
    if let Some(h) = dirs::home_dir() {
        return h.join("Videos").to_string_lossy().into_owned();
    }
    ".".to_owned()
}
