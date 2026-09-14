//! Shared recording core: WGC capture -> bounded queue -> ffmpeg.
//! Used by both the CLI and the egui GUI.
//!
//! Performance design:
//!
//!   - GPU-composited event-driven capture (idle screen ~= 0% CPU)
//!   - fixed-size pipe, center crop/pad in-Rust (cheap memcpy, no scaler)
//!   - bounded channel + try_send (never blocks capture; drops = realtime)
//!   - buffer reuse (no per-frame alloc), CFR pacer thread, realtime encoder flags
//!
//! Hardware specifics (which encoder, how many threads, what display) come
//! from `crate::caps`, never from hardcoded machine assumptions.

use std::io::Write as _;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use clap::ValueEnum;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};
use serde::Deserialize;
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use crate::caps::{Capabilities, capabilities, resolve_av1, resolve_h264, resolve_hevc};

/// Video codec choice. Hardware variants fall back across vendors via
/// `crate::caps`; forced vendor variants bail with a clear message when
/// their GPU/driver is missing. Values are kebab-case on the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Codec {
    /// Auto H.264: best available — QSV, then NVENC, then AMF, then x264.
    H264,
    /// Forced Intel Quick Sync H.264. Needs an Intel iGPU + driver.
    H264Qsv,
    /// Forced NVIDIA NVENC H.264. Needs an NVIDIA GPU + driver.
    H264Nvenc,
    /// Forced AMD AMF H.264. Needs an AMD GPU + driver.
    H264Amf,
    /// Forced software x264 (also the lossless-tier vehicle). Any x86_64.
    X264,
    /// Auto HEVC/H.265 hardware (QSV → NVENC → AMF). No software fallback:
    /// software HEVC at 60 fps is unusable, so absence is an error.
    H265,
    /// Auto AV1 hardware (QSV → NVENC → AMF). Opt-in. No software fallback.
    Av1,
    /// Software VP8. Lowest CPU, universal playback.
    Vp8,
    /// Software VP9. Better compression, needs more CPU.
    Vp9,
}

impl Codec {
    /// CLI spelling (`--codec h264-qsv`).
    pub fn cli_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H264Qsv => "h264-qsv",
            Codec::H264Nvenc => "h264-nvenc",
            Codec::H264Amf => "h264-amf",
            Codec::X264 => "x264",
            Codec::H265 => "h265",
            Codec::Av1 => "av1",
            Codec::Vp8 => "vp8",
            Codec::Vp9 => "vp9",
        }
    }
}

/// Output container. Each has different crash-safety properties, see
/// `container_mux_args`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Container {
    /// Fragmented MP4: playable even if the process is killed mid-record.
    #[default]
    Mp4,
    /// Matroska: broadly compatible, bounded clusters for kill-safety.
    Mkv,
    /// WebM: VP8/VP9/AV1 + Opus only, bounded clusters for kill-safety.
    Webm,
}

impl Container {
    pub fn ext(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Mkv => "mkv",
            Container::Webm => "webm",
        }
    }
    /// Audio codec used when the user doesn't pick one explicitly.
    pub fn default_audio(self) -> AudioCodec {
        match self {
            Container::Mp4 => AudioCodec::Aac,
            Container::Mkv | Container::Webm => AudioCodec::Opus,
        }
    }
    pub fn allows_audio(self, a: AudioCodec) -> bool {
        match (self, a) {
            (Container::Mp4, AudioCodec::Aac) => true,
            (Container::Mkv, _) => true, // MKV officially supports both
            (Container::Webm, AudioCodec::Opus) => true,
            _ => false,
        }
    }
    pub fn allows_video(self, c: Codec) -> bool {
        match self {
            Container::Mp4 => matches!(
                c,
                Codec::H264
                    | Codec::H264Qsv
                    | Codec::H264Nvenc
                    | Codec::H264Amf
                    | Codec::X264
                    | Codec::H265
                    | Codec::Av1
            ),
            Container::Mkv => true, // MKV holds everything we encode
            Container::Webm => matches!(c, Codec::Vp8 | Codec::Vp9 | Codec::Av1),
        }
    }
}

/// Audio codec. Follows the container by default (`Container::default_audio`);
/// `--audio-codec` overrides within the container's allowed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AudioCodec {
    #[default]
    Opus,
    Aac,
}

impl AudioCodec {
    pub fn cli_name(self) -> &'static str {
        match self {
            AudioCodec::Opus => "opus",
            AudioCodec::Aac => "aac",
        }
    }
    pub(crate) fn ffmpeg_id(self) -> &'static str {
        match self {
            // Native AAC (no libfdk in our ffmpeg build); Opus via libopus.
            AudioCodec::Opus => "libopus",
            AudioCodec::Aac => "aac",
        }
    }
}

/// Reject invalid codec+container+audio combinations at argument-parse time
/// with a clear message, instead of failing (or silently renaming files)
/// deep in the encode pipeline.
pub fn validate_matrix(
    codec: Codec,
    container: Container,
    audio: AudioCodec,
) -> Result<()> {
    if !container.allows_video(codec) {
        bail!(
            "--codec {} can't go in .{} — {} holds {}; try --container mp4, mkv or webm",
            codec.cli_name(),
            container.ext(),
            match container {
                Container::Mp4 => "MP4",
                Container::Mkv => "MKV",
                Container::Webm => "WebM",
            },
            match container {
                Container::Mp4 => "H.264, H.265, AV1",
                Container::Mkv => "anything",
                Container::Webm => "VP8, VP9, AV1",
            },
        );
    }
    if !container.allows_audio(audio) {
        bail!(
            "--audio-codec {} isn't allowed in .{} ({} supports {}) — drop the flag to use {}",
            audio.cli_name(),
            container.ext(),
            container.ext(),
            match container {
                Container::Mp4 => "AAC",
                Container::Mkv => "Opus or AAC",
                Container::Webm => "Opus",
            },
            container.default_audio().cli_name(),
        );
    }
    Ok(())
}

/// Concrete ffmpeg encoder for a codec choice, plus a one-line human reason
/// ("no Quick Sync, NVIDIA NVENC available") for logs and bug reports.
pub fn video_encoder_verbose(codec: Codec) -> Result<(&'static str, String)> {
    let caps = capabilities();
    match codec {
        Codec::H264Qsv if !caps.h264.qsv => {
            bail!("forced h264-qsv unavailable: no Intel Quick Sync (needs an Intel iGPU + driver)")
        }
        Codec::H264Qsv => Ok(("h264_qsv", "forced Intel Quick Sync".to_owned())),
        Codec::H264Nvenc if !caps.h264.nvenc => {
            bail!("forced h264-nvenc unavailable: no NVIDIA NVENC (needs an NVIDIA GPU + driver)")
        }
        Codec::H264Nvenc => Ok(("h264_nvenc", "forced NVIDIA NVENC".to_owned())),
        Codec::H264Amf if !caps.h264.amf => {
            bail!("forced h264-amf unavailable: no AMD AMF (needs an AMD GPU + driver)")
        }
        Codec::H264Amf => Ok(("h264_amf", "forced AMD AMF".to_owned())),
        Codec::H264 => Ok(resolve_h264(&caps.h264)),
        Codec::X264 => Ok(("libx264", "forced software x264".to_owned())),
        Codec::H265 => resolve_hevc(&caps.hevc).map_err(anyhow::Error::msg),
        Codec::Av1 => resolve_av1(&caps.av1).map_err(anyhow::Error::msg),
        Codec::Vp8 => Ok(("libvpx", "software VP8".to_owned())),
        Codec::Vp9 => Ok(("libvpx-vp9", "software VP9".to_owned())),
    }
}

/// Concrete ffmpeg encoder for a codec choice.
pub fn video_encoder(codec: Codec) -> Result<&'static str> {
    Ok(video_encoder_verbose(codec)?.0)
}
/// Short display name for a concrete encoder id.
pub fn encoder_display(enc: &'static str) -> &'static str {
    match enc {
        "h264_qsv" => "H.264-QuickSync",
        "h264_nvenc" => "H.264-NVENC",
        "h264_amf" => "H.264-AMF",
        "libx264" => "H.264-x264",
        "hevc_qsv" => "H.265-QSV",
        "hevc_nvenc" => "H.265-NVENC",
        "hevc_amf" => "H.265-AMF",
        "av1_qsv" => "AV1-QSV",
        "av1_nvenc" => "AV1-NVENC",
        "av1_amf" => "AV1-AMF",
        "libvpx" => "VP8",
        "libvpx-vp9" => "VP9",
        _ => enc,
    }
}

/// Quality tiers: vendor-neutral names describing encode effort vs quality.
/// Bitrates scale with resolution (`bitrate_for`) instead of fixed strings,
/// and presets are per-encoder-family. Values are kebab-case on the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    /// Lowest CPU/GPU load, lower quality. Weak hardware, commentary drafts.
    Fastest,
    /// The default. Good quality per bit on any machine.
    #[default]
    Balanced,
    /// Higher bitrate + slower preset. Needs encode headroom.
    /// Spelled `high-quality` on the CLI and in config files (not `high`).
    #[serde(rename = "high-quality")]
    #[value(name = "high-quality")]
    High,
    /// Mathematically lossless where the codec supports it (x264, VP9).
    /// Hardware encoders and VP8 are rejected with guidance, not silence.
    Lossless,
}

impl Tier {
    /// CLI spelling (`--quality high-quality`).
    pub fn cli_name(self) -> &'static str {
        match self {
            Tier::Fastest => "fastest",
            Tier::Balanced => "balanced",
            Tier::High => "high-quality",
            Tier::Lossless => "lossless",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Tier::Fastest => "Fastest (lowest load)",
            Tier::Balanced => "Balanced (default)",
            Tier::High => "High quality (needs headroom)",
            Tier::Lossless => "Lossless (huge files, x264/VP9 only)",
        }
    }
    /// Reference bitrate at 1080p60, Mbps. Scales with pixel count.
    fn base_mbps(self) -> f64 {
        match self {
            Tier::Fastest => 6.0,
            Tier::Balanced => 10.0,
            Tier::High => 20.0,
            Tier::Lossless => 0.0, // unused: lossless modes ignore bitrate
        }
    }
    /// Bitrate target like `"12M"`, scaled linearly by pixel count vs 1080p
    /// and clamped to a sane range. A 720p balanced record asks ~4M; a 4K
    /// high-quality one asks ~80M.
    pub fn bitrate_for(self, w: u32, h: u32) -> String {
        let mp = (w.max(64) as f64 * h.max(64) as f64) / (1920.0 * 1080.0);
        let m = (self.base_mbps() * mp).clamp(1.0, 80.0);
        format!("{m:.0}M")
    }
    /// libvpx speed 0(best)..8(fastest). Lossless is exact at any speed;
    /// 2 keeps 1080p60 near-realtime on decent CPUs.
    pub fn vpx_cpu_used(self) -> u8 {
        match self {
            Tier::Fastest => 8,
            Tier::Balanced => 7,
            Tier::High => 5,
            Tier::Lossless => 2,
        }
    }
    pub fn x264_preset(self) -> &'static str {
        match self {
            Tier::Fastest => "ultrafast",
            Tier::Balanced => "veryfast",
            Tier::High => "medium",
            Tier::Lossless => "ultrafast",
        }
    }
    pub fn qsv_preset(self) -> &'static str {
        match self {
            Tier::Fastest => "veryfast",
            Tier::Balanced => "fast",
            Tier::High => "medium",
            Tier::Lossless => "veryslow", // unreachable via validate_tier; defensive
        }
    }
    pub fn nvenc_preset(self) -> &'static str {
        match self {
            Tier::Fastest => "p1",
            Tier::Balanced => "p4",
            Tier::High => "p7",
            Tier::Lossless => "p7", // unreachable via validate_tier; defensive
        }
    }
    pub fn amf_quality(self) -> &'static str {
        match self {
            Tier::Fastest => "speed",
            Tier::Balanced => "balanced",
            Tier::High => "quality",
            Tier::Lossless => "quality", // unreachable via validate_tier; defensive
        }
    }
}

/// Default tier from capabilities: hardware H.264 (or plenty of threads for
/// software x264) gets Balanced; weak software-only machines get Fastest.
pub fn default_tier(caps: &Capabilities) -> Tier {
    let hw = caps.h264.qsv || caps.h264.nvenc || caps.h264.amf;
    if hw || caps.cpu_threads >= 8 {
        Tier::Balanced
    } else {
        Tier::Fastest
    }
}

/// Reject tier+codec combinations with no lossless mode (lossless on a
/// hardware encoder or VP8) at parse time. Returns the resolved ffmpeg
/// encoder id plus the human reason, so callers don't probe twice.
pub fn check_tier(codec: Codec, tier: Tier) -> Result<(&'static str, String)> {
    let (enc, why) = video_encoder_verbose(codec)?;
    if tier == Tier::Lossless && enc != "libx264" && enc != "libvpx-vp9" {
        bail!(
            "lossless tier needs a codec with a lossless mode ({enc} has none) — \
             use --codec x264 or --codec vp9"
        );
    }
    Ok((enc, why))
}

#[derive(Debug, Clone)]
pub enum Source {
    Monitor { index: Option<usize> },
    Window { needle: String },
}

#[derive(Debug, Clone)]
pub struct RecordConfig {
    /// Fully resolved output path (use `resolve_output` to build it).
    pub output: String,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    pub source: Source,
    pub codec: Codec,
    /// Resolved bitrate like `"12M"` (tier-derived unless overridden).
    /// Ignored by lossless modes.
    pub bitrate: String,
    /// libvpx speed 0(best)..8(fastest); tier-derived unless overridden.
    /// Only affects VP8/VP9.
    pub cpu_used: u8,
    /// Encoder tier (drives presets). Kept so the writer path and
    /// `--benchmark` report the same settings the user picked.
    pub tier: Tier,
    pub container: Container,
    pub audio_codec: AudioCodec,
    pub threads: u32,
    pub duration: Option<u64>,
    pub no_cursor: bool,
    pub audio: AudioMode,
}

/// Audio source. Note: per-app isolation needs Windows 11+; on Windows 10
/// "System" hears everything playing — mute other apps for clean game audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AudioMode {
    /// Everything you hear (game + all apps). Best for game videos.
    #[default]
    System,
    /// Microphone only (commentary).
    Mic,
    /// Game sound + microphone mixed together.
    Both,
    /// Silent video.
    Off,
}

impl AudioMode {
    pub fn label(self) -> &'static str {
        match self {
            AudioMode::System => "System (game sound)",
            AudioMode::Mic => "Microphone",
            AudioMode::Both => "System + mic",
            AudioMode::Off => "Off",
        }
    }
    fn system(self) -> bool {
        matches!(self, AudioMode::System | AudioMode::Both)
    }
    fn mic(self) -> bool {
        matches!(self, AudioMode::Mic | AudioMode::Both)
    }
}

#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub title: String,
    pub process: String,
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub index: usize,
    pub name: String,
    pub w: u32,
    pub h: u32,
    pub hz: u32,
}

pub fn list_windows() -> Result<Vec<WindowInfo>> {
    // Never offer our own window (recording it = feedback loop), and skip
    // untitled helper windows — normal recorders hide both.
    let own_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    let wins = Window::enumerate().context("failed to enumerate windows")?;
    Ok(wins
        .iter()
        .filter_map(|w| {
            let title = w.title().unwrap_or_default();
            if title.trim().is_empty() {
                return None;
            }
            let proc_ = w.process_name().unwrap_or_default();
            if !own_exe.is_empty() && proc_.to_lowercase() == own_exe {
                return None;
            }
            Some(WindowInfo {
                title,
                process: proc_,
                w: w.width().unwrap_or(0),
                h: w.height().unwrap_or(0),
            })
        })
        .collect())
}

pub fn list_monitors() -> Result<Vec<MonitorInfo>> {
    let mons = Monitor::enumerate().context("failed to enumerate monitors")?;
    Ok(mons
        .iter()
        .map(|m| MonitorInfo {
            index: m.index().unwrap_or(0),
            name: m.name().unwrap_or_else(|_| "?".into()),
            w: m.width().unwrap_or(0),
            h: m.height().unwrap_or(0),
            hz: m.refresh_rate().unwrap_or(0),
        })
        .collect())
}

fn source_native_size(source: &Source) -> Result<(u32, u32)> {
    match source {
        Source::Window { needle } => {
            let w = Window::from_contains_name(needle)
                .with_context(|| format!("no window title contains \"{needle}\""))?;
            Ok((
                w.width().unwrap_or(0).max(0) as u32,
                w.height().unwrap_or(0).max(0) as u32,
            ))
        }
        Source::Monitor { index } => {
            let m = match index {
                Some(i) => Monitor::from_index((*i).max(1))?,
                None => Monitor::primary()?,
            };
            Ok((m.width().unwrap_or(0), m.height().unwrap_or(0)))
        }
    }
}

/// Output frame size.
/// Native (default): match the app/monitor at record start — no black bars,
/// and the identical-size fast path (single memcpy). Fixed modes pad/crop.
pub fn resolve_size(size: &str, source: &Source) -> Result<(u32, u32)> {
    fn even(v: u32) -> u32 {
        v.clamp(64, 7680) & !1 // VPx needs even dimensions
    }
    match size.trim().to_lowercase().as_str() {
        "native" | "auto" | "" => {
            let (w, h) = source_native_size(source)?;
            if w < 16 || h < 16 {
                anyhow::bail!("source has no size (window minimized?)");
            }
            Ok((even(w), even(h)))
        }
        "1080p" => Ok((1920, 1080)),
        "720p" => Ok((1280, 720)),
        custom => {
            if let Some((a, b)) = custom.split_once('x') {
                if let (Ok(w), Ok(h)) =
                    (a.trim().parse::<u32>(), b.trim().parse::<u32>())
                {
                    if w >= 64 && h >= 64 {
                        return Ok((even(w), even(h)));
                    }
                }
            }
            anyhow::bail!("bad size \"{size}\": use native, 1080p, 720p, or WIDTHxHEIGHT")
        }
    }
}

pub fn describe_source(cfg: &RecordConfig) -> Result<String> {
    match &cfg.source {
        Source::Window { needle } => {
            let w = Window::from_contains_name(needle)
                .with_context(|| format!("no window title contains \"{needle}\""))?;
            Ok(format!(
                "window \"{}\" ({}x{})",
                w.title().unwrap_or_default(),
                w.width().unwrap_or(0).max(0),
                w.height().unwrap_or(0).max(0),
            ))
        }
        Source::Monitor { index } => {
            let m = match index {
                Some(i) => Monitor::from_index((*i).max(1))?,
                None => Monitor::primary()?,
            };
            Ok(format!(
                "monitor #{} \"{}\" ({}x{})",
                m.index().unwrap_or(0),
                m.name().unwrap_or_default(),
                m.width().unwrap_or(0),
                m.height().unwrap_or(0),
            ))
        }
    }
}

/// Resolve the output path: bare filename => joined under `dir`; a path with a
/// directory component or drive letter is used as-is. The extension must
/// agree with `container` (or be absent, in which case it is appended):
/// a conflicting extension is a parse-time error, never a silent rename.
/// Parent folders are created, and collisions auto-increment (`name_001`)
/// so recordings are never silently overwritten.
pub fn resolve_output(output: &str, dir: &str, container: Container) -> Result<String> {
    let want_ext = container.ext();
    let mut out = output.to_owned();
    let lower = out.to_lowercase();
    const KNOWN: [&str; 3] = ["webm", "mp4", "mkv"];
    if let Some(got) = KNOWN.iter().find(|e| lower.ends_with(&format!(".{e}"))) {
        if *got != want_ext {
            bail!(
                "output ends with .{got} but the {want_ext} container needs .{want_ext} — \
                 rename the file or pass --container {got}"
            );
        }
    } else {
        out.push('.');
        out.push_str(want_ext);
    }
    let p = Path::new(&out);
    let is_bare = p
        .parent()
        .map(|par| par.as_os_str().is_empty())
        .unwrap_or(true)
        && !out.contains(':')
        && !out.contains('/')
        && !out.contains('\\');
    let joined: PathBuf = if is_bare {
        let d = Path::new(dir);
        std::fs::create_dir_all(d)
            .with_context(|| format!("cannot create folder {dir}"))?;
        d.join(&out)
    } else {
        if let Some(par) = p.parent() {
            if !par.as_os_str().is_empty() {
                std::fs::create_dir_all(par)
                    .with_context(|| format!("cannot create folder {}", par.display()))?;
            }
        }
        PathBuf::from(&out)
    };
    Ok(unique_path(&joined).to_string_lossy().into_owned())
}

fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_owned();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".into());
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let parent = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    for i in 1..10000 {
        let cand = parent.join(format!("{stem}_{i:03}{ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    parent.join(format!(
        "{stem}_{}{ext}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    ))
}

// ---------- capture plumbing ----------

/// A finished RGBA frame ready for ffmpeg stdin.
type Packet = Vec<u8>;

/// A small RGBA frame for the GUI live preview (480px wide, ~10 fps).
/// Kept tiny on purpose: ~500 KB per frame, latest-only channel.
#[derive(Debug, Clone, Default)]
pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Downscale RGBA `src` (sw×sh) to ~480px-wide RGBA with nearest-neighbor.
/// Cheap (~130k pixel copies for 1080p) — runs at most 10×/s.
fn downscale_rgba_preview(src: &[u8], sw: u32, sh: u32) -> PreviewFrame {
    let step = (sw / 480).max(1);
    let pw = sw / step;
    let ph = sh / step;
    let mut rgba = Vec::with_capacity((pw as usize) * (ph as usize) * 4);
    for y in 0..ph {
        for x in 0..pw {
            let s = (((y * step) as usize) * (sw as usize) + ((x * step) as usize)) * 4;
            rgba.extend_from_slice(&[src[s], src[s + 1], src[s + 2], 255]);
        }
    }
    PreviewFrame { width: pw, height: ph, rgba }
}

/// Lock the shared error slot, tolerating a poisoned mutex (a crashed thread
/// must never take the whole app down with it).
fn lock_err(m: &Mutex<Option<String>>) -> std::sync::MutexGuard<'_, Option<String>> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone)]
struct PipeFlags {
    tx: Sender<Packet>,
    preview: Option<Sender<PreviewFrame>>,
    out_w: u32,
    out_h: u32,
    /// Initial scratch capacity (bytes) for the de-padding buffer, derived
    /// from the detected display / target resolution — not a fixed 1080p.
    scratch_hint: usize,
    stop: Arc<AtomicBool>,
    captured: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    deadline: Option<Instant>,
}

struct Recorder {
    tx: Sender<Packet>,
    preview: Option<Sender<PreviewFrame>>,
    out_w: u32,
    out_h: u32,
    stop: Arc<AtomicBool>,
    captured: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    deadline: Option<Instant>,
    last_preview: Instant,
    /// reused scratch for de-padding (no alloc per frame)
    nopad: Vec<u8>,
    /// reused scratch for fitted output (no alloc per frame)
    fitted: Vec<u8>,
}

impl GraphicsCaptureApiHandler for Recorder {
    type Flags = PipeFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let f = ctx.flags.clone();
        let px = (f.out_w as usize) * (f.out_h as usize) * 4;
        Ok(Self {
            tx: f.tx,
            preview: f.preview,
            out_w: f.out_w,
            out_h: f.out_h,
            stop: f.stop,
            captured: f.captured,
            dropped: f.dropped,
            deadline: f.deadline,
            last_preview: Instant::now() - Duration::from_secs(1),
            nopad: Vec::with_capacity(f.scratch_hint),
            fitted: vec![0u8; px],
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.stop.load(Ordering::Relaxed) {
            control.stop();
            return Ok(());
        }
        if let Some(d) = self.deadline {
            if Instant::now() >= d {
                self.stop.store(true, Ordering::Relaxed);
                control.stop();
                return Ok(());
            }
        }

        let sw = frame.width();
        let sh = frame.height();
        if sw == 0 || sh == 0 {
            return Ok(()); // transient during window resize
        }

        let fb = frame.buffer()?;
        let tmp = fb.as_nopadding_buffer(&mut self.nopad);
        if tmp.len() < (sw as usize) * (sh as usize) * 4 {
            return Ok(()); // malformed frame, skip
        }
        fit_frame_center(tmp, sw, sh, &mut self.fitted, self.out_w, self.out_h);
        self.captured.fetch_add(1, Ordering::Relaxed);
        // Live preview for the GUI: downscaled copy at most every 100 ms,
        // latest-only channel. Must run BEFORE the try_send below moves
        // `fitted` away. Zero cost when no preview consumer exists.
        if let Some(ptx) = &self.preview {
            let now = Instant::now();
            if now.duration_since(self.last_preview) >= Duration::from_millis(100) {
                self.last_preview = now;
                let small = downscale_rgba_preview(&self.fitted, self.out_w, self.out_h);
                // depth-1 channel: if the GUI hasn't drained yet, drop this
                // frame (latest wins next tick) — never block capture.
                let _ = ptx.try_send(small);
            }
        }
        // Never block the capture thread (blocks = lag + RAM growth).
        // Bounded channel keeps RAM flat; drops = realtime pacing, like OBS.
        // (Depth is 3: absorbs the ~1 s encoder spin-up burst at record
        // start; steady state still can't balloon.)
        match self.tx.try_send(std::mem::replace(
            &mut self.fitted,
            vec![0u8; (self.out_w as usize) * (self.out_h as usize) * 4],
        )) {
            Ok(()) => {}
            Err(TrySendError::Full(pkt)) => {
                self.fitted = pkt; // reclaim buffer
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(pkt)) => {
                self.fitted = pkt;
                control.stop();
            }
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.stop.store(true, Ordering::Relaxed);
        Ok(())
    }
}

/// Center-crop / center-pad `src` (sw×sh) into `dst` (dw×dh).
/// Pixel-order agnostic (pure memcpy). Fast path: identical size = single memcpy.
fn fit_frame_center(src: &[u8], sw: u32, sh: u32, dst: &mut [u8], dw: u32, dh: u32) {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    if sw == dw && sh == dh {
        let n = dw * dh * 4;
        dst[..n].copy_from_slice(&src[..n]);
        return;
    }
    for b in dst.iter_mut() {
        *b = 0; // black bars
    }
    let cw = sw.min(dw);
    let ch = sh.min(dh);
    let sx0 = (sw - cw) / 2;
    let sy0 = (sh - ch) / 2;
    let dx0 = (dw - cw) / 2;
    let dy0 = (dh - ch) / 2;
    for y in 0..ch {
        let s_off = ((sy0 + y) * sw + sx0) * 4;
        let d_off = ((dy0 + y) * dw + dx0) * 4;
        let n = cw * 4;
        dst[d_off..d_off + n].copy_from_slice(&src[s_off..s_off + n]);
    }
}

/// "12M" / "8000k" / "8000000" -> kilobits per second (for maxrate/bufsize).
fn bitrate_kbps(s: &str) -> u64 {
    let t = s.trim().to_lowercase();
    let kbps = if let Some(n) = t.strip_suffix('m') {
        n.trim().parse::<f64>().unwrap_or(12.0) * 1000.0
    } else if let Some(n) = t.strip_suffix('k') {
        n.trim().parse::<f64>().unwrap_or(8000.0)
    } else {
        t.parse::<f64>().unwrap_or(12_000_000.0) / 1000.0
    };
    kbps.max(500.0) as u64
}

/// Realtime-tuned encoder args for one concrete ffmpeg encoder id.
/// Shared by the recorder and `--benchmark` so both use identical settings.
/// Tier drives presets/speed; `bitrate` is the resolved `"12M"`-style target
/// (ignored by lossless modes); `cpu_used` only affects VP8/VP9.
pub(crate) fn encoder_args(
    enc: &str,
    tier: Tier,
    bitrate: &str,
    cpu_used: u8,
    fps: u32,
) -> Vec<String> {
    let gop = (fps.max(1) * 2).to_string();
    let cpu = cpu_used.clamp(0, 8).to_string();
    // CBR-ish bitrate trio shared by all lossy paths.
    let trio = vec![
        "-b:v".to_owned(),
        format!("{}k", bitrate_kbps(bitrate)),
        "-maxrate".to_owned(),
        format!("{}k", bitrate_kbps(bitrate) * 3 / 2),
        "-bufsize".to_owned(),
        format!("{}k", bitrate_kbps(bitrate) * 2),
    ];
    let mut v = Vec::new();
    match enc {
        "libvpx" => {
            v.extend(trio);
            v.extend([
                "-deadline".into(),
                "realtime".into(),
                "-cpu-used".into(),
                cpu,
                "-lag-in-frames".into(),
                "16".into(),
                "-auto-alt-ref".into(),
                "1".into(),
                "-g".into(),
                gop,
            ]);
        }
        "libvpx-vp9" if tier == Tier::Lossless => {
            // True lossless VP9. Exact at any speed; cpu speed only sets pace.
            v.extend([
                "-lossless".into(),
                "1".into(),
                "-b:v".into(),
                "0".into(),
                "-cpu-used".into(),
                cpu,
                "-row-mt".into(),
                "1".into(),
                "-tile-columns".into(),
                "2".into(),
                "-tile-rows".into(),
                "1".into(),
                "-g".into(),
                gop,
            ]);
        }
        "libvpx-vp9" => {
            v.extend(trio);
            v.extend([
                "-deadline".into(),
                "realtime".into(),
                "-cpu-used".into(),
                cpu,
                "-row-mt".into(),
                "1".into(),
                "-tile-columns".into(),
                "2".into(),
                "-tile-rows".into(),
                "1".into(),
                // zero lookahead: lowest CPU + lowest latency; the bitrate
                // carries the quality instead — right trade for live capture.
                "-lag-in-frames".into(),
                "0".into(),
                "-auto-alt-ref".into(),
                "1".into(),
                "-g".into(),
                gop,
            ]);
        }
        "libx264" if tier == Tier::Lossless => {
            v.extend([
                "-crf".into(),
                "0".into(),
                "-preset".into(),
                "ultrafast".into(),
                "-tune".into(),
                "zerolatency".into(),
                "-g".into(),
                gop,
            ]);
        }
        "libx264" => {
            v.extend(trio);
            v.extend([
                "-preset".into(),
                tier.x264_preset().into(),
                "-tune".into(),
                "zerolatency".into(),
                "-g".into(),
                gop,
            ]);
        }
        // H.264 Quick Sync: the long-verified path, incl. no-lookahead.
        "h264_qsv" => {
            v.extend(trio);
            v.extend([
                "-preset".into(),
                tier.qsv_preset().into(),
                "-look_ahead".into(),
                "0".into(),
                "-g".into(),
                gop,
            ]);
        }
        // HEVC/AV1 hardware: minimal verified-shape flags only. Extra
        // per-encoder options are deliberately NOT passed here — an
        // unsupported option kills ffmpeg, and these paths can't all be
        // tested on one machine (see TESTING.md).
        other if other.ends_with("_qsv") => {
            v.extend(trio);
            v.extend(["-preset".into(), tier.qsv_preset().into(), "-g".into(), gop]);
        }
        other if other.ends_with("_nvenc") => {
            v.extend(trio);
            v.extend([
                "-preset".into(),
                tier.nvenc_preset().into(),
                "-tune".into(),
                "ll".into(),
                "-g".into(),
                gop,
            ]);
        }
        other if other.ends_with("_amf") => {
            v.extend(trio);
            v.extend(["-quality".into(), tier.amf_quality().into(), "-g".into(), gop]);
        }
        _ => {
            // Defensive: an unknown future encoder still gets paced output.
            v.extend(trio);
            v.extend(["-g".into(), gop]);
        }
    }
    v
}

// ---------- ffmpeg ----------

/// Spawn ffmpeg. `audio` is Some((sample_rate, tcp_port)) unless Off —
/// video comes over stdin, audio over loopback TCP, muxed to one file.
fn spawn_ffmpeg(
    cfg: &RecordConfig,
    audio: Option<(u32, u16)>,
) -> Result<ffmpeg_sidecar::child::FfmpegChild> {
    let _ = ffmpeg_sidecar::download::auto_download().context("ffmpeg auto-download failed");

    let enc: &str = video_encoder(cfg.codec)?;
    let extra: Vec<String> =
        encoder_args(enc, cfg.tier, &cfg.bitrate, cfg.cpu_used, cfg.fps);

    let threads = if cfg.threads == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(4)
    } else {
        cfg.threads
    };

    let mut cmd = ffmpeg_sidecar::command::FfmpegCommand::new();
    // NOTE: capture delivers RGBA (ColorFormat::Rgba8) — declaring anything
    // else (e.g. bgra) swaps red/blue and tints everything yellow.
    // `-use_wallclock_as_timestamps`: frame timestamps come from the wall
    // clock, so if the encoder stalls under load the video keeps real speed
    // (slight stepping) instead of fast-forwarding.
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-use_wallclock_as_timestamps",
        "1",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgba",
        "-s",
        &format!("{}x{}", cfg.width, cfg.height),
        "-framerate",
        &cfg.fps.to_string(),
        "-i",
        "-",
    ]);
    if let Some((rate, port)) = audio {
        // Game/mic sound: raw PCM over loopback TCP -> encoded audio
        // (Opus or AAC depending on container) muxed into the same file.
        cmd.args([
            "-f",
            "s16le",
            "-ar",
            &rate.to_string(),
            "-ac",
            "2",
            "-i",
            &format!("tcp://127.0.0.1:{port}"),
        ]);
    } else {
        cmd.arg("-an");
    }
    cmd.args([
        "-c:v",
        enc,
        "-threads",
        &threads.to_string(),
        "-pix_fmt",
        "yuv420p",
    ]);
    if audio.is_some() {
        cmd.args([
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-c:a",
            cfg.audio_codec.ffmpeg_id(),
            "-b:a",
            "128k",
            "-ar",
            "48000",
        ]);
    }
    for e in &extra {
        cmd.arg(e);
    }
    // Crash-safety (verified with kill tests, see TESTING.md):
    // - MP4 is fragmented (empty moov + fragments at keyframes), so a file
    //   killed mid-record stays playable up to the last keyframe instead of
    //   losing everything to a missing moov atom.
    // - MKV/WebM get 2 s bounded clusters for the same reason: a killed file
    //   plays up to the last complete cluster. (This ffmpeg build has no
    //   matroska/webm `live` muxer option; bounded clusters are the
    //   equivalent mechanism here.)
    match cfg.container {
        Container::Mp4 => {
            cmd.args(["-movflags", "frag_keyframe+empty_moov"]);
        }
        Container::Mkv | Container::Webm => {
            cmd.args(["-cluster_time_limit", "2000"]);
        }
    }
    // NOTE: no -vsync/-fps_mode flag on purpose. Our writer thread already
    // paces stdin at exactly CFR, and the vsync option was removed in recent
    // ffmpeg builds (Unrecognized option 'vsync') — passing it kills ffmpeg.
    cmd.arg(&cfg.output);
    cmd.spawn().context(
        "failed to spawn ffmpeg. Install it (winget install Gyan.FFmpeg) or let crabby auto-download it",
    )
}

/// Writer thread: paces channel frames out at exactly `fps` CFR, reusing the
/// last frame while idle (standard OBS behavior).
fn writer_loop(
    rx: Receiver<Packet>,
    mut child: ffmpeg_sidecar::child::FfmpegChild,
    cfg: &RecordConfig,
    stop: Arc<AtomicBool>,
    written: Arc<AtomicU64>,
    error: Arc<Mutex<Option<String>>>,
) {
    let res: Result<()> = (|| {
        use std::io::Write as _;
        let mut stdin = child
            .take_stdin()
            .context("ffmpeg stdin unavailable")?;
        let frame_bytes = (cfg.width as usize) * (cfg.height as usize) * 4;
        let mut last: Vec<u8> = vec![0u8; frame_bytes]; // starts black
        let mut have_frame = false;
        let interval = Duration::from_nanos(1_000_000_000 / cfg.fps.max(1) as u64);
        let mut next = Instant::now() + interval;

        loop {
            // drain channel, keep newest (drop stale => realtime + flat RAM)
            while let Ok(pkt) = rx.try_recv() {
                if pkt.len() == frame_bytes {
                    last = pkt;
                    have_frame = true;
                }
            }
            if !have_frame {
                if stop.load(Ordering::Relaxed) && rx.is_empty() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5)); // no spin
                next = Instant::now() + interval;
                continue;
            }
            if stdin.write_all(&last).is_err() {
                break; // ffmpeg exited — normal at shutdown
            }
            written.fetch_add(1, Ordering::Relaxed);

            let now = Instant::now();
            if now < next {
                std::thread::sleep(next - now);
            } else if now - next > Duration::from_millis(500) {
                next = now; // encode stall — resync instead of spiralling
            }
            next += interval;

            if stop.load(Ordering::Relaxed) && rx.is_empty() {
                break;
            }
        }
        drop(stdin); // EOF -> ffmpeg finalizes the .webm/.mp4
        // Loud failure beats a silent empty file: a bad encoder (or dead
        // ffmpeg) must surface, not produce 0-byte recordings.
        let status = child.wait().context("ffmpeg wait failed")?;
        if !status.success() {
            anyhow::bail!("ffmpeg exited with {status} — encoder failed to start?");
        }
        Ok(())
    })();
    if let Err(e) = res {
        *lock_err(&error) = Some(format!("encoder: {e:?}"));
    }
}

// ---------- audio (system loopback + mic -> Opus/AAC) ----------

/// Native sample rate for the audio feed. Loopback rate wins (Both/System),
/// otherwise the mic rate — ffmpeg resamples to 48 kHz for Opus.
fn probe_audio_rate(mode: AudioMode) -> Result<u32> {
    let host = cpal::default_host();
    if mode.system() {
        let dev = host
            .default_output_device()
            .context("no system audio device (speakers/headphones?)")?;
        let c = dev.default_output_config().context("no system audio format")?;
        match c.sample_format() {
            cpal::SampleFormat::F32
            | cpal::SampleFormat::I16
            | cpal::SampleFormat::U16 => Ok(c.sample_rate().0),
            f => bail!("unsupported system audio format ({f:?})"),
        }
    } else {
        let dev = host.default_input_device().context("no microphone found")?;
        let c = dev.default_input_config().context("no microphone format")?;
        match c.sample_format() {
            cpal::SampleFormat::F32
            | cpal::SampleFormat::I16
            | cpal::SampleFormat::U16 => Ok(c.sample_rate().0),
            f => bail!("unsupported microphone format ({f:?})"),
        }
    }
}

fn to_stereo(samples: &[f32], channels: usize) -> Vec<f32> {
    let ch = channels.max(1);
    let n = samples.len() / ch;
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n {
        let l = samples[i * ch];
        out.push(l);
        out.push(if ch > 1 { samples[i * ch + 1] } else { l });
    }
    out
}

/// Open an input stream on `dev` (an output device = loopback capture,
/// an input device = mic) and forward stereo-f32 chunks to `tx`.
fn build_in_stream(
    dev: &cpal::Device,
    sc: &cpal::SupportedStreamConfig,
    tx: Sender<Vec<f32>>,
    what: &'static str,
) -> Result<cpal::Stream> {
    let ch = sc.channels() as usize;
    let cfg: cpal::StreamConfig = sc.config();
    let err_fn = move |e| eprintln!("audio {what} stream error: {e}");
    let stream = match sc.sample_format() {
        cpal::SampleFormat::F32 => dev.build_input_stream(
            &cfg,
            move |d: &[f32], _| {
                let _ = tx.try_send(to_stereo(d, ch));
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => dev.build_input_stream(
            &cfg,
            move |d: &[i16], _| {
                let _ = tx.try_send(to_stereo(
                    &d.iter().map(|&s| s as f32 / 32768.0).collect::<Vec<_>>(),
                    ch,
                ));
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => dev.build_input_stream(
            &cfg,
            move |d: &[u16], _| {
                let _ = tx.try_send(to_stereo(
                    &d.iter().map(|&s| s as f32 / 32768.0 - 1.0).collect::<Vec<_>>(),
                    ch,
                ));
            },
            err_fn,
            None,
        ),
        f => bail!("unsupported {what} audio format ({f:?})"),
    }
    .with_context(|| format!("couldn't open {what} audio"))?;
    stream.play().context("couldn't start audio")?;
    Ok(stream)
}

fn write_s16(tcp: &mut TcpStream, stereo_f32: &[f32]) -> Result<()> {
    let mut buf = Vec::with_capacity(stereo_f32.len() * 2);
    for &v in stereo_f32 {
        buf.extend_from_slice(&((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    tcp.write_all(&buf).context("ffmpeg audio link broke")?;
    Ok(())
}

/// Mix one loopback chunk with resampled mic backlog (linear interpolation).
/// Returns stereo f32 at the loopback rate. Underruns pad with the last
/// sample instead of stalling — audio never blocks the recording.
fn mix_with_mic(
    lchunk: &[f32],
    backlog: &mut Vec<f32>,
    pos: &mut f32,
    mic_rate: u32,
    out_rate: u32,
    n: usize,
) -> Vec<f32> {
    fn at(backlog: &[f32], k: usize) -> (f32, f32) {
        if backlog.len() < 2 {
            return (0.0, 0.0);
        }
        let k = k.min(backlog.len() / 2 - 1);
        (backlog[k * 2], backlog[k * 2 + 1])
    }
    let mut out = Vec::with_capacity(n * 2);
    let ratio = mic_rate as f32 / out_rate.max(1) as f32;
    for i in 0..n {
        let p = *pos + i as f32 * ratio;
        let i0 = p.floor() as usize;
        let frac = (p - i0 as f32).clamp(0.0, 1.0);
        let (al, ar) = at(backlog, i0);
        let (bl, br) = at(backlog, i0 + 1);
        out.push(lchunk[i * 2] + al + (bl - al) * frac);
        out.push(lchunk[i * 2 + 1] + ar + (br - ar) * frac);
    }
    *pos += n as f32 * ratio;
    let drop = (*pos).floor().max(0.0) as usize;
    let drop = drop.min(backlog.len() / 2);
    backlog.drain(..drop * 2);
    *pos -= drop as f32;
    // Bound memory if the mic floods (same clock domain — shouldn't happen).
    const MAX_BACKLOG: usize = 48000 * 2 * 5;
    if backlog.len() > MAX_BACKLOG {
        let excess = backlog.len() - MAX_BACKLOG;
        backlog.drain(..excess);
    }
    out
}

/// Audio thread: owns the cpal streams, mixes, and feeds s16le stereo to
/// ffmpeg over loopback TCP. Accepts the connection FIRST so that even a
/// later capture failure closes the link instead of hanging ffmpeg.
fn audio_loop(
    listener: TcpListener,
    rate: u32,
    mode: AudioMode,
    mic_rate_hint: u32,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
) {
    let res: Result<()> = (|| {
        let (mut tcp, _) = listener.accept().context("ffmpeg audio link failed")?;
        tcp.set_nodelay(true).ok();

        let host = cpal::default_host();
        let (loop_tx, loop_rx) = crossbeam_channel::bounded::<Vec<f32>>(32);
        let (mic_tx, mic_rx) = crossbeam_channel::bounded::<Vec<f32>>(32);
        // Streams stay alive exactly as long as this thread (drop = stop).
        let mut _streams: Vec<cpal::Stream> = Vec::new();
        let mut mic_rate = mic_rate_hint;
        if mode.system() {
            let dev = host.default_output_device().context("system audio lost")?;
            let sc = dev.default_output_config()?;
            _streams.push(build_in_stream(&dev, &sc, loop_tx, "system")?);
        }
        if mode.mic() {
            let dev = host.default_input_device().context("microphone lost")?;
            let sc = dev.default_input_config()?;
            mic_rate = sc.sample_rate().0;
            _streams.push(build_in_stream(&dev, &sc, mic_tx, "mic")?);
        }

        let idle = Duration::from_millis(2);
        let drained = || {
            stop.load(Ordering::Relaxed) && loop_rx.is_empty() && mic_rx.is_empty()
        };
        match mode {
            AudioMode::System => loop {
                match loop_rx.try_recv() {
                    Ok(chunk) => write_s16(&mut tcp, &chunk)?,
                    Err(TryRecvError::Empty) => {
                        if drained() {
                            break;
                        }
                        std::thread::sleep(idle);
                    }
                    Err(TryRecvError::Disconnected) => break,
                }
            },
            AudioMode::Mic => loop {
                match mic_rx.try_recv() {
                    Ok(chunk) => write_s16(&mut tcp, &chunk)?,
                    Err(TryRecvError::Empty) => {
                        if drained() {
                            break;
                        }
                        std::thread::sleep(idle);
                    }
                    Err(TryRecvError::Disconnected) => break,
                }
            },
            AudioMode::Both => {
                let mut backlog: Vec<f32> = Vec::new();
                let mut pos = 0f32;
                loop {
                    while let Ok(m) = mic_rx.try_recv() {
                        backlog.extend_from_slice(&m);
                    }
                    match loop_rx.try_recv() {
                        Ok(lchunk) => {
                            let n = lchunk.len() / 2;
                            if n == 0 {
                                continue;
                            }
                            let mixed =
                                mix_with_mic(&lchunk, &mut backlog, &mut pos, mic_rate, rate, n);
                            write_s16(&mut tcp, &mixed)?;
                        }
                        Err(TryRecvError::Empty) => {
                            if drained() {
                                break;
                            }
                            std::thread::sleep(idle);
                        }
                        Err(TryRecvError::Disconnected) => break,
                    }
                }
            }
            AudioMode::Off => {}
        }
        Ok(())
    })();
    if let Err(e) = res {
        *lock_err(&error) = Some(format!("audio: {e:?}"));
    }
    // TCP + streams drop here -> audio EOF for ffmpeg.
}

// ---------- session (shared by CLI + GUI) ----------

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub captured: u64,
    pub dropped: u64,
    pub written: u64,
}

/// A running recording. UI threads stay responsive: poll `snapshot()` /
/// `captures_done()`, stop with `request_stop()`, finalize with `wait()`.
pub struct Session {
    stop: Arc<AtomicBool>,
    captured: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    written: Arc<AtomicU64>,
    error: Arc<Mutex<Option<String>>>,
    output: String,
    started: Instant,
    capture: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    audio: Option<JoinHandle<()>>,
    preview_rx: Option<Receiver<PreviewFrame>>,
}

/// Start a recording. `preview` opens a latest-only preview tap for the GUI
/// (the CLI passes false — zero preview cost).
pub fn start_session(cfg: RecordConfig, preview: bool) -> Result<Session> {
    // Depth 3 (~25 MB worst case): absorbs the ~1 s ffmpeg/QSV spin-up burst
    // so the first seconds don't drop; steady state still can't balloon.
    let (tx, rx): (Sender<Packet>, Receiver<Packet>) = crossbeam_channel::bounded(3);
    let (ptx, prx) = if preview {
        let (t, r) = crossbeam_channel::bounded::<PreviewFrame>(1);
        (Some(t), Some(r))
    } else {
        (None, None)
    };
    let stop = Arc::new(AtomicBool::new(false));
    let captured = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicU64::new(0));
    let written = Arc::new(AtomicU64::new(0));
    let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let deadline = cfg.duration.map(|s| Instant::now() + Duration::from_secs(s));

    // Audio link: probe devices first (fail fast with a clear message),
    // then bind loopback TCP so ffmpeg gets a stable port. Binding before
    // the spawn guarantees no race; the audio thread accepts first thing so
    // even a later capture failure closes the link instead of hanging ffmpeg.
    enum AudioLink {
        Off,
        On { listener: TcpListener, rate: u32, mic_rate: u32, port: u16 },
    }
    let audio_link = if cfg.audio == AudioMode::Off {
        AudioLink::Off
    } else {
        let rate = probe_audio_rate(cfg.audio)?;
        let mic_rate = if cfg.audio.mic() {
            cpal::default_host()
                .default_input_device()
                .and_then(|d| d.default_input_config().ok())
                .map(|c| c.sample_rate().0)
                .unwrap_or(rate)
        } else {
            rate
        };
        let listener = TcpListener::bind("127.0.0.1:0").context("couldn't open audio link")?;
        let port = listener
            .local_addr()
            .context("couldn't read audio link port")?
            .port();
        AudioLink::On { listener, rate, mic_rate, port }
    };

    let child = spawn_ffmpeg(
        &cfg,
        match &audio_link {
            AudioLink::Off => None,
            AudioLink::On { rate, port, .. } => Some((*rate, *port)),
        },
    )?;

    // audio thread owns the cpal streams + TCP socket
    let audio = match audio_link {
        AudioLink::Off => None,
        AudioLink::On { listener, rate, mic_rate, .. } => {
            let (stop, error, mode) = (stop.clone(), error.clone(), cfg.audio);
            Some(std::thread::spawn(move || {
                audio_loop(listener, rate, mode, mic_rate, stop, error)
            }))
        }
    };

    // writer thread owns ffmpeg
    let writer = {
        let (rx, stop, written, error, cfg) =
            (rx, stop.clone(), written.clone(), error.clone(), cfg.clone());
        std::thread::spawn(move || writer_loop(rx, child, &cfg, stop, written, error))
    };

    // capture thread owns the WGC session (Capture::start blocks)
    let capture = {
        let (stop, captured, dropped, error) =
            (stop.clone(), captured.clone(), dropped.clone(), error.clone());
        // Scratch for the de-padding buffer: must fit the largest frame the
        // source can deliver. Sources can't exceed the display, so size from
        // the detected display (falling back to 1080p headless), floored
        // against the target size in case detection failed low.
        let out_px = cfg.width as usize * cfg.height as usize;
        let disp_px = capabilities()
            .primary_display
            .map(|d| d.w as usize * d.h as usize)
            .unwrap_or(1920 * 1080);
        let scratch_hint = out_px.max(disp_px).saturating_mul(4).max(64 * 64 * 4);
        std::thread::spawn(move || {
            let flags = PipeFlags {
                tx,
                preview: ptx,
                out_w: cfg.width,
                out_h: cfg.height,
                scratch_hint,
                stop: stop.clone(),
                captured,
                dropped,
                deadline,
            };
            let cursor = if cfg.no_cursor {
                CursorCaptureSettings::WithoutCursor
            } else {
                CursorCaptureSettings::WithCursor
            };
            // NOTE: Win10 (like this machine) rejects explicit border toggling —
            // only Default is accepted. Win11 supports With/WithoutBorder.
            let border = DrawBorderSettings::Default;
            let res: Result<()> = (|| -> Result<()> {
                match &cfg.source {
                Source::Window { needle } => {
                    let item = Window::from_contains_name(needle)
                        .context("window not found (did it close?)")?;
                    let settings = Settings::new(
                        item,
                        cursor,
                        border,
                        SecondaryWindowSettings::Default,
                        MinimumUpdateIntervalSettings::Default,
                        DirtyRegionSettings::Default,
                        ColorFormat::Rgba8,
                        flags,
                    );
                    Recorder::start(settings).context("capture failed")
                }
                Source::Monitor { index } => {
                    let item = match index {
                        Some(i) => Monitor::from_index((*i).max(1))?,
                        None => Monitor::primary()?,
                    };
                    let settings = Settings::new(
                        item,
                        cursor,
                        border,
                        SecondaryWindowSettings::Default,
                        MinimumUpdateIntervalSettings::Default,
                        DirtyRegionSettings::Default,
                        ColorFormat::Rgba8,
                        flags,
                    );
                    Recorder::start(settings).context("capture failed")
                }
            }})();
            if let Err(e) = res {
                *lock_err(&error) = Some(format!("capture: {e:?}"));
            }
            stop.store(true, Ordering::Relaxed); // let the writer flush + exit
        })
    };

    Ok(Session {
        stop,
        captured,
        dropped,
        written,
        error,
        output: cfg.output,
        started: Instant::now(),
        capture: Some(capture),
        writer: Some(writer),
        audio,
        preview_rx: prx,
    })
}

impl Session {
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }
    pub fn output(&self) -> &str {
        &self.output
    }
    /// Latest-only preview tap (GUI). Drains with try_recv; may be None (CLI).
    pub fn preview_rx(&self) -> Option<&Receiver<PreviewFrame>> {
        self.preview_rx.as_ref()
    }
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            captured: self.captured.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            written: self.written.load(Ordering::Relaxed),
        }
    }
    /// True once all background threads have exited (joinable without blocking).
    pub fn captures_done(&self) -> bool {
        let cap = self.capture.as_ref().map(|h| h.is_finished()).unwrap_or(true);
        let wr = self.writer.as_ref().map(|h| h.is_finished()).unwrap_or(true);
        let au = self.audio.as_ref().map(|h| h.is_finished()).unwrap_or(true);
        cap && wr && au
    }
    /// Finalize the file and return any background error. Call after
    /// `request_stop()` or once `captures_done()` (duration reached / window
    /// closed). Never call from the capture thread itself.
    pub fn wait(mut self) -> Result<Snapshot> {
        self.request_stop();
        if let Some(h) = self.capture.take() {
            let _ = h.join();
        }
        // Audio next: closing the TCP link ends the audio stream, then the
        // writer closes video stdin and ffmpeg finalizes the file.
        if let Some(h) = self.audio.take() {
            let _ = h.join();
        }
        if let Some(h) = self.writer.take() {
            let _ = h.join();
        }
        if let Some(e) = lock_err(&self.error).take() {
            anyhow::bail!("{e}");
        }
        Ok(self.snapshot())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_accepts_sane_combos() {
        assert!(validate_matrix(Codec::H264, Container::Mp4, AudioCodec::Aac).is_ok());
        assert!(validate_matrix(Codec::H265, Container::Mp4, AudioCodec::Aac).is_ok());
        assert!(validate_matrix(Codec::Av1, Container::Mp4, AudioCodec::Aac).is_ok());
        assert!(validate_matrix(Codec::Vp8, Container::Webm, AudioCodec::Opus).is_ok());
        assert!(validate_matrix(Codec::Vp9, Container::Webm, AudioCodec::Opus).is_ok());
        assert!(validate_matrix(Codec::Av1, Container::Webm, AudioCodec::Opus).is_ok());
        assert!(validate_matrix(Codec::H264, Container::Mkv, AudioCodec::Opus).is_ok());
        assert!(validate_matrix(Codec::Vp9, Container::Mkv, AudioCodec::Aac).is_ok());
        assert!(validate_matrix(Codec::X264, Container::Mkv, AudioCodec::Opus).is_ok());
    }

    #[test]
    fn matrix_rejects_bad_combos_with_guidance() {
        // Opus is not allowed in MP4 (AAC it is).
        let e = validate_matrix(Codec::H264, Container::Mp4, AudioCodec::Opus).unwrap_err();
        assert!(e.to_string().contains("AAC"), "unexpected: {e}");
        // H.264 can't go in WebM.
        let e = validate_matrix(Codec::H264, Container::Webm, AudioCodec::Opus).unwrap_err();
        assert!(e.to_string().contains("--container"), "unexpected: {e}");
        // AAC can't go in WebM.
        let e = validate_matrix(Codec::Vp9, Container::Webm, AudioCodec::Aac).unwrap_err();
        assert!(e.to_string().contains("Opus"), "unexpected: {e}");
        // VP8 can't go in MP4.
        assert!(validate_matrix(Codec::Vp8, Container::Mp4, AudioCodec::Aac).is_err());
    }

    #[test]
    fn tier_bitrate_scales_with_resolution() {
        assert_eq!(Tier::Balanced.bitrate_for(1920, 1080), "10M");
        assert_eq!(Tier::Fastest.bitrate_for(1920, 1080), "6M");
        assert_eq!(Tier::High.bitrate_for(1920, 1080), "20M");
        // 720p asks roughly half of 1080p.
        assert_eq!(Tier::Balanced.bitrate_for(1280, 720), "4M");
        // 4K high clamps at the 80M ceiling (20 * 4 = 80).
        assert_eq!(Tier::High.bitrate_for(3840, 2160), "80M");
        // Tiny inputs clamp to the 1M floor, never 0.
        assert_eq!(Tier::Fastest.bitrate_for(64, 64), "1M");
    }

    #[test]
    fn lossless_tier_needs_lossless_codec_hw_independent() {
        // These hold on ANY machine: VP9/x264 resolve to software everywhere.
        assert!(check_tier(Codec::Vp9, Tier::Lossless).is_ok());
        assert!(check_tier(Codec::X264, Tier::Lossless).is_ok());
        // VP8 has no lossless mode anywhere.
        let e = check_tier(Codec::Vp8, Tier::Lossless).unwrap_err();
        assert!(e.to_string().contains("x264"), "unexpected: {e}");
    }

    #[test]
    fn encoder_args_shapes() {
        // Lossless x264: CRF 0, no bitrate flags.
        let a = encoder_args("libx264", Tier::Lossless, "10M", 8, 60);
        assert!(a.contains(&"-crf".to_owned()) && a.contains(&"0".to_owned()));
        assert!(!a.contains(&"-b:v".to_owned()));
        // Lossy x264 balanced: veryfast preset + bitrate trio.
        let a = encoder_args("libx264", Tier::Balanced, "10M", 8, 60);
        assert!(a.contains(&"veryfast".to_owned()) && a.contains(&"-b:v".to_owned()));
        // QSV balanced uses the fast preset with no lookahead.
        let a = encoder_args("h264_qsv", Tier::Balanced, "10M", 8, 60);
        assert!(a.contains(&"fast".to_owned()) && a.contains(&"-look_ahead".to_owned()));
        // VP9 lossless carries the lossless flag.
        let a = encoder_args("libvpx-vp9", Tier::Lossless, "0M", 2, 60);
        assert!(a.contains(&"-lossless".to_owned()));
        // Unknown future encoder still gets paced output, never garbage.
        let a = encoder_args("mystery_hw", Tier::Balanced, "10M", 8, 30);
        assert!(a.contains(&"-b:v".to_owned()) && a.contains(&"-g".to_owned()));
        assert!(a.contains(&"60".to_owned())); // GOP = 2 x 30 fps
    }

    #[test]
    fn output_rejects_conflicting_extension_before_touching_disk() {
        // Conflicting extension bails before any directory is created.
        let e =
            resolve_output("clip.webm", "definitely-not-a-dir-xyz", Container::Mp4).unwrap_err();
        assert!(e.to_string().contains("--container"), "unexpected: {e}");
        assert!(
            !std::path::Path::new("definitely-not-a-dir-xyz").exists(),
            "must not create anything on validation failure"
        );
        // Matching extension passes; tested in a temp dir to avoid litter.
        let tmp = std::env::temp_dir().join("crabby-test-output");
        let _ = std::fs::remove_dir_all(&tmp);
        let p = resolve_output("clip.mp4", tmp.to_str().unwrap(), Container::Mp4).unwrap();
        assert!(p.ends_with("clip.mp4"), "unexpected: {p}");
        // Bare name gains the container extension.
        let p2 = resolve_output("clip", tmp.to_str().unwrap(), Container::Mkv).unwrap();
        assert!(p2.ends_with("clip.mkv"), "unexpected: {p2}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn container_audio_defaults_match_matrix() {
        assert_eq!(Container::Mp4.default_audio(), AudioCodec::Aac);
        assert_eq!(Container::Mkv.default_audio(), AudioCodec::Opus);
        assert_eq!(Container::Webm.default_audio(), AudioCodec::Opus);
        // The default never violates the matrix it came from.
        for c in [Container::Mp4, Container::Mkv, Container::Webm] {
            assert!(c.allows_audio(c.default_audio()));
        }
    }
}
