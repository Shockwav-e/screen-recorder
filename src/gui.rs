//! Crabby GUI (egui/eframe — GPU-accelerated, no webview).
//!
//! Normal-recorder layout: live preview + transport on top, source / output /
//! quality sections below, recordings library at the bottom.
//!
//! Performance best practices applied:
//!   - UI thread never blocks: capture + encode run on background threads,
//!     `Session::wait()` finalizes on a helper thread during Stop.
//!   - Repaints throttled to ~10 Hz while recording; fully static when idle.
//!   - Preview is a 480px, 10 fps, latest-only tap (~0.5 MB/frame).

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt as _;

use anyhow::Result;
use eframe::egui;

use crate::caps::capabilities;
use crate::config::{self, default_video_dir};
use crate::recorder::{
    AudioCodec, AudioMode, Codec, Container, PerfLevel, RecordConfig, Snapshot, Source, Session,
    Tier, check_perf, check_tier, default_tier, describe_source, encoder_display, list_monitors,
    list_windows, resolve_output, resolve_size, start_session, validate_matrix,
    MonitorInfo, WindowInfo,
};
use crate::shot::{self, Shot, ShotFormat, ShotMode};
use crate::updater;

/// Index 0 = follow the tier; the rest override it (power users).
const BITRATES: [&str; 6] = ["Auto (tier)", "8M", "10M", "12M", "16M", "20M"];
const RES_OPTIONS: [&str; 3] = ["native", "1080p", "720p"];
const RES_LABELS: [&str; 3] = ["Native (app size, no bars)", "1080p fixed", "720p fixed"];
/// Parallel to `enc_override`: 0 = auto, then one entry per Codec variant.
const ENC_NAMES: [&str; 10] = [
    "Auto (H.264 best available)",
    "H.264 auto",
    "H.264 Intel Quick Sync",
    "H.264 NVIDIA NVENC",
    "H.264 AMD AMF",
    "H.264 software x264",
    "H.265 auto (hardware)",
    "AV1 auto (hardware)",
    "VP8",
    "VP9",
];

#[derive(PartialEq)]
enum Phase {
    Idle,
    Recording,
    Stopping,
}

/// Background self-update state machine (never blocks the UI thread).
#[derive(PartialEq)]
enum UpdatePhase {
    /// Nothing known yet / no update.
    Quiet,
    /// A check is running on a background thread.
    Checking,
    /// A newer release exists; dialog offers it.
    Available,
    /// Download running; `done/total` bytes.
    Downloading,
    /// Staged on disk; one click restarts into it.
    Ready,
    /// Last check said we're current.
    UpToDate,
    /// Last check/download failed (transient — retry anytime).
    Failed,
}

enum UpdateEvent {
    Checked(Result<Option<updater::ReleaseInfo>, String>),
    Progress(u64, u64),
    Downloaded(Result<std::path::PathBuf, String>),
}

struct UpdateUi {
    phase: UpdatePhase,
    info: Option<updater::ReleaseInfo>,
    staged: Option<std::path::PathBuf>,
    done: u64,
    total: u64,
    error: Option<String>,
    show_dialog: bool,
    tx: Sender<UpdateEvent>,
    rx: Receiver<UpdateEvent>,
}

impl UpdateUi {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            phase: UpdatePhase::Quiet,
            info: None,
            staged: None,
            done: 0,
            total: 0,
            error: None,
            show_dialog: false,
            tx,
            rx,
        }
    }

    /// Start a background check (no-op while one is already running).
    fn spawn_check(&mut self) {
        if self.phase == UpdatePhase::Checking || self.phase == UpdatePhase::Downloading {
            return;
        }
        self.phase = UpdatePhase::Checking;
        self.error = None;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let res = updater::check_for_update().map_err(|e| format!("{e:?}"));
            updater::mark_checked();
            let _ = tx.send(UpdateEvent::Checked(res));
        });
    }

    /// Start a background download of the known release.
    fn spawn_download(&mut self) {
        let Some(info) = self.info.clone() else { return };
        self.phase = UpdatePhase::Downloading;
        self.done = 0;
        self.total = info.bytes;
        self.error = None;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let res = updater::download_update(&info, &|done, total| {
                let _ = tx.send(UpdateEvent::Progress(done, total));
            })
            .map_err(|e| format!("{e:?}"));
            let _ = tx.send(UpdateEvent::Downloaded(res));
        });
    }

    fn poll(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                UpdateEvent::Checked(Ok(None)) => {
                    self.phase = UpdatePhase::UpToDate;
                    self.info = None;
                }
                UpdateEvent::Checked(Ok(Some(info))) => {
                    self.phase = UpdatePhase::Available;
                    self.info = Some(info);
                    self.show_dialog = true;
                }
                UpdateEvent::Checked(Err(e)) => {
                    self.phase = UpdatePhase::Failed;
                    self.error = Some(e);
                }
                UpdateEvent::Progress(done, total) => {
                    self.done = done;
                    self.total = total;
                }
                UpdateEvent::Downloaded(Ok(path)) => {
                    self.phase = UpdatePhase::Ready;
                    self.staged = Some(path);
                }
                UpdateEvent::Downloaded(Err(e)) => {
                    self.phase = UpdatePhase::Failed;
                    self.error = Some(e);
                }
            }
        }
    }
}

struct StopOutcome {
    output: String,
    result: std::result::Result<Snapshot, String>,
    /// Last ffmpeg stderr lines (read before `wait()` consumes the session).
    ffmpeg_tail: Vec<String>,
}

/// In-progress screenshot editor (basic pen + crop, Snipping-Tool style).
struct ShotEdit {
    shot: Shot,
    tex: Option<egui::TextureHandle>,
    pen: usize,
    pen_size: f32,
    crop_mode: bool,
    /// Drag rect in image pixels (set while dragging in crop mode).
    crop_rect: Option<egui::Rect>,
    /// Drag anchor in image pixels.
    drag_start: Option<egui::Pos2>,
    /// In-progress pen stroke in image pixels (drawn as overlay, committed
    /// to pixels on release — one texture upload per stroke, not per move).
    cur_stroke: Vec<egui::Pos2>,
}

struct ShotSaveOutcome {
    path: String,
    copied: bool,
    shot: Shot,
    result: std::result::Result<(), String>,
}

/// Global screenshot hotkeys (work from anywhere while Crabby runs):
/// Win+PrtSc fullscreen (like Windows), PrtSc region, Alt+PrtSc window.
/// The manager must stay alive — dropping it unregisters the keys.
struct HotkeyState {
    _manager: Option<global_hotkey::GlobalHotKeyManager>,
    /// (hotkey id, action) for every successfully registered key.
    bindings: Vec<(u32, ShotMode)>,
    /// Human-readable status for the screenshot section.
    status: String,
}

impl HotkeyState {
    fn init(enabled: bool) -> Self {
        if !enabled {
            return Self {
                _manager: None,
                bindings: Vec::new(),
                status: "global keys off (shot_hotkeys = false)".to_owned(),
            };
        }
        let manager = match global_hotkey::GlobalHotKeyManager::new() {
            Ok(m) => m,
            Err(e) => {
                return Self {
                    _manager: None,
                    bindings: Vec::new(),
                    status: format!("global keys unavailable: {e:?}"),
                }
            }
        };
        use global_hotkey::hotkey::{Code, HotKey, Modifiers};
        // (label, modifiers, key, action)
        let wants = [
            ("Win+PrtSc fullscreen", Some(Modifiers::SUPER), Code::PrintScreen, ShotMode::Fullscreen),
            ("PrtSc region", None, Code::PrintScreen, ShotMode::Region),
            ("Alt+PrtSc window", Some(Modifiers::ALT), Code::PrintScreen, ShotMode::Window),
        ];
        let mut bindings = Vec::new();
        let mut failed = Vec::new();
        for (label, mods, code, mode) in wants {
            let hk = HotKey::new(mods, code);
            match manager.register(hk) {
                Ok(()) => bindings.push((hk.id(), mode)),
                Err(_) => failed.push(label),
            }
        }
        let live = wants
            .iter()
            .filter(|(label, _, _, _)| !failed.contains(label))
            .map(|(label, _, _, _)| *label)
            .collect::<Vec<_>>()
            .join(" · ");
        let status = if failed.is_empty() {
            format!("keys live: {live}")
        } else if bindings.is_empty() {
            format!("global keys taken by another app ({})", failed.join(", "))
        } else {
            format!("keys live: {live}  (taken: {})", failed.join(", "))
        };
        Self { _manager: Some(manager), bindings, status }
    }
}

const PEN_COLORS: [([u8; 4], &str); 6] = [
    ([255, 60, 60, 255], "Red"),
    ([255, 210, 60, 255], "Yellow"),
    ([80, 220, 100, 255], "Green"),
    ([90, 160, 255, 255], "Blue"),
    ([255, 255, 255, 255], "White"),
    ([20, 20, 20, 255], "Black"),
];

fn shot_texture(ctx: &egui::Context, name: &str, shot: &Shot) -> Option<egui::TextureHandle> {
    if shot.is_empty() || (shot.width as usize) * (shot.height as usize) * 4 != shot.rgba.len() {
        return None;
    }
    Some(ctx.load_texture(
        name,
        egui::ColorImage::from_rgba_unmultiplied(
            [shot.width as usize, shot.height as usize],
            &shot.rgba,
        ),
        egui::TextureOptions::LINEAR,
    ))
}

struct LibFile {
    name: String,
    path: String,
    bytes: u64,
    modified: std::time::SystemTime,
}

fn fmt_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.0} MB", bytes as f64 / 1_048_576.0)
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
}

fn fmt_age(modified: std::time::SystemTime) -> String {
    let s = modified.elapsed().map(|d| d.as_secs()).unwrap_or(0);
    if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86400)
    }
}

pub struct GuiApp {
    monitors: Vec<MonitorInfo>,
    windows: Vec<WindowInfo>,
    lists_error: Option<String>,

    use_window: bool,
    monitor_sel: usize,
    win_filter: String,
    win_sel: Option<usize>,

    dir: String,
    filename: String,
    tier: Tier,
    bitrate_sel: usize,
    res_sel: usize,
    enc_sel: usize,
    container_sel: usize,
    /// None = follow the container (AAC for MP4, Opus otherwise).
    audio_codec_sel: Option<AudioCodec>,
    audio: AudioMode,
    fps60: bool,
    no_cursor: bool,
    auto_stop: bool,
    stop_secs: u32,
    /// 0 = auto (CPU count); exposed for CLI/GUI parity.
    threads: u32,

    phase: Phase,
    session: Option<Session>,
    pending: Option<Receiver<StopOutcome>>,
    preview_tex: Option<egui::TextureHandle>,
    show_preview: bool,
    lib_files: Vec<LibFile>,
    start_t: Instant,
    snap: Snapshot,
    out_fps: f32,
    last_written: u64,
    last_t: Instant,

    done_msg: Option<String>,
    error_msg: Option<String>,
    /// Why the current recording uses its encoder (set at Start).
    enc_note: Option<String>,
    update: UpdateUi,

    // --- screenshots (Snipping-Tool style) ---
    shot_format: ShotFormat,
    shot_dir: String,
    shot_edit: Option<ShotEdit>,
    shot_cap_rx: Option<Receiver<std::result::Result<Shot, String>>>,
    shot_save_rx: Option<Receiver<ShotSaveOutcome>>,
    shot_busy: bool,
    hotkeys: HotkeyState,
    /// Settings key the perf warning was last acknowledged for — pressing
    /// Record again with unchanged settings records anyway; changing any
    /// perf-relevant setting re-arms the guard.
    perf_ack_key: String,
    /// Show the one-click "safe 720p30" button under the transport.
    perf_offer_safe: bool,
}

impl GuiApp {
    fn new(cc: &eframe::CreationContext<'_>, updated_from: Option<String>) -> Self {
        // Modern dark theme, slightly rounded, comfortable spacing.
        let mut style = (*cc.egui_ctx.style()).clone();
        style.visuals = egui::Visuals::dark();
        style.visuals.selection.bg_fill = egui::Color32::from_rgb(88, 101, 242); // blurple
        for w in [
            &mut style.visuals.widgets.hovered,
            &mut style.visuals.widgets.inactive,
            &mut style.visuals.widgets.active,
        ] {
            w.corner_radius = egui::CornerRadius::same(6);
        }
        style.spacing.button_padding = egui::vec2(12.0, 7.0);
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        cc.egui_ctx.set_style(style);

        // Initial values: config file wins over built-ins (CLI flags would
        // win over both, but the GUI *is* the flag surface — it edits live).
        let file_cfg = config::load(None).unwrap_or_default();
        let caps = capabilities();
        let tier = file_cfg.quality.unwrap_or_else(|| default_tier(caps));
        let container = file_cfg.container.unwrap_or_default();
        let enc_sel = file_cfg
            .codec
            .map(|c| match c {
                Codec::H264 => 1,
                Codec::H264Qsv => 2,
                Codec::H264Nvenc => 3,
                Codec::H264Amf => 4,
                Codec::X264 => 5,
                Codec::H265 => 6,
                Codec::Av1 => 7,
                Codec::Vp8 => 8,
                Codec::Vp9 => 9,
            })
            .unwrap_or(0);
        let mut app = Self {
            monitors: Vec::new(),
            windows: Vec::new(),
            lists_error: None,
            use_window: false,
            monitor_sel: 0,
            win_filter: String::new(),
            win_sel: None,
            dir: file_cfg.dir.unwrap_or_else(default_video_dir),
            filename: "recording".to_owned(),
            tier,
            bitrate_sel: 0, // Auto (tier)
            res_sel: 0, // native app size
            enc_sel,
            container_sel: match container {
                Container::Mp4 => 0,
                Container::Mkv => 1,
                Container::Webm => 2,
            },
            audio_codec_sel: file_cfg.audio_codec,
            audio: file_cfg.audio.unwrap_or(AudioMode::System),
            fps60: true,
            no_cursor: false,
            auto_stop: false,
            stop_secs: 60,
            threads: file_cfg.threads.unwrap_or(0),
            phase: Phase::Idle,
            session: None,
            pending: None,
            preview_tex: None,
            show_preview: true,
            lib_files: Vec::new(),
            start_t: Instant::now(),
            snap: Snapshot::default(),
            out_fps: 0.0,
            last_written: 0,
            last_t: Instant::now(),
            done_msg: None,
            error_msg: None,
            enc_note: None,
            update: UpdateUi::new(),
            shot_format: file_cfg.shot_format.unwrap_or_default(),
            shot_dir: file_cfg.shot_dir.unwrap_or_else(shot::screenshot_dir),
            shot_edit: None,
            shot_cap_rx: None,
            shot_save_rx: None,
            shot_busy: false,
            hotkeys: HotkeyState::init(file_cfg.shot_hotkeys.unwrap_or(true)),
            perf_ack_key: String::new(),
            perf_offer_safe: false,
        };
        if let Some(from) = updated_from {
            app.done_msg = Some(format!(
                "Updated to v{} (was v{from}) — you're on the latest!",
                updater::current_version()
            ));
        }
        // One silent background check per day; manual checks anytime via header.
        if updater::should_auto_check() {
            app.update.spawn_check();
        }
        app.refresh_sources();
        app.refresh_library();
        app
    }

    /// Recordings library: video files in the save folder, newest first.
    fn refresh_library(&mut self) {
        self.lib_files.clear();
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return; // folder may not exist yet — created on first record
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let is_video = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("webm") || e.eq_ignore_ascii_case("mp4") || e.eq_ignore_ascii_case("mkv"))
                .unwrap_or(false);
            if is_video {
                if let Ok(md) = entry.metadata() {
                    self.lib_files.push(LibFile {
                        name: p
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        path: p.to_string_lossy().into_owned(),
                        bytes: md.len(),
                        modified: md.modified().unwrap_or(std::time::UNIX_EPOCH),
                    });
                }
            }
        }
        self.lib_files.sort_by_key(|f| std::cmp::Reverse(f.modified));
    }

    fn refresh_sources(&mut self) {
        match list_monitors() {
            Ok(m) => {
                self.monitors = m;
                if self.monitor_sel >= self.monitors.len() {
                    self.monitor_sel = 0;
                }
            }
            Err(e) => self.lists_error = Some(format!("monitors: {e:?}")),
        }
        match list_windows() {
            Ok(w) => {
                self.windows = w;
                self.win_sel = None;
            }
            Err(e) => self.lists_error = Some(format!("windows: {e:?}")),
        }
    }

    /// Bitrate override, if any. Index 0 follows the tier (scaled to the
    /// recording size); the rest are fixed power-user values.
    fn selected_bitrate(&self) -> Option<String> {
        if self.bitrate_sel == 0 {
            None
        } else {
            BITRATES
                .get(self.bitrate_sel)
                .map(|s| (*s).to_owned())
        }
    }

    /// Manual encoder override (None = H.264 auto). Parallel to ENC_NAMES.
    fn enc_override(&self) -> Option<Codec> {
        match self.enc_sel {
            1 => Some(Codec::H264),
            2 => Some(Codec::H264Qsv),
            3 => Some(Codec::H264Nvenc),
            4 => Some(Codec::H264Amf),
            5 => Some(Codec::X264),
            6 => Some(Codec::H265),
            7 => Some(Codec::Av1),
            8 => Some(Codec::Vp8),
            9 => Some(Codec::Vp9),
            _ => None,
        }
    }

    fn selected_container(&self) -> Container {
        match self.container_sel {
            1 => Container::Mkv,
            2 => Container::Webm,
            _ => Container::Mp4,
        }
    }

    fn start(&mut self) {
        self.done_msg = None;
        self.error_msg = None;
        self.enc_note = None;

        let source = if self.use_window {
            let Some(i) = self.win_sel else {
                self.error_msg = Some("Pick a window from the list first.".to_owned());
                return;
            };
            let Some(w) = self.windows.get(i) else {
                self.error_msg = Some("Selected window is gone — hit Refresh.".to_owned());
                return;
            };
            Source::Window { needle: w.title.clone() }
        } else {
            let index = self.monitors.get(self.monitor_sel).map(|m| m.index);
            Source::Monitor { index }
        };

        // Sanitize: filename field must stay a bare file name (no folders or
        // drive letters — those belong in the Folder field above).
        let fname = std::path::Path::new(&self.filename)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "recording".to_owned());
        let codec: Codec = self.enc_override().unwrap_or(Codec::H264);
        let container = self.selected_container();
        let audio_codec = self.audio_codec_sel.unwrap_or_else(|| container.default_audio());
        // Parse-time validation first: never start a capture we can't encode.
        if let Err(e) = validate_matrix(codec, container, audio_codec)
            .and_then(|()| check_tier(codec, self.tier).map(|_| ()))
        {
            self.error_msg = Some(format!("{e:?}"));
            return;
        }
        let output = match resolve_output(&fname, &self.dir, container) {
            Ok(p) => p,
            Err(e) => {
                self.error_msg = Some(format!("bad output: {e:?}"));
                return;
            }
        };

        let size_mode = RES_OPTIONS[self.res_sel.min(RES_OPTIONS.len() - 1)];
        let (width, height) = match resolve_size(size_mode, &source) {
            Ok(wh) => wh,
            Err(e) => {
                self.error_msg = Some(format!("bad size/source: {e:?}"));
                return;
            }
        };
        let cfg = RecordConfig {
            output,
            fps: if self.fps60 { 60 } else { 30 },
            width,
            height,
            source,
            codec,
            bitrate: self
                .selected_bitrate()
                .unwrap_or_else(|| self.tier.bitrate_for(width, height)),
            cpu_used: self.tier.vpx_cpu_used(),
            tier: self.tier,
            container,
            audio_codec,
            threads: self.threads,
            duration: self.auto_stop.then_some(self.stop_secs.max(5) as u64),
            no_cursor: self.no_cursor,
            audio: self.audio,
        };
        // Validate the source exists before claiming "recording".
        if let Err(e) = describe_source(&cfg) {
            self.error_msg = Some(format!("source unavailable: {e:?}"));
            return;
        }
        // Perf pre-flight: a doomed recording (e.g. software x264 at
        // 1440p60 on 4 threads) warns first instead of silently dropping
        // 97% of frames. Pressing Record again with unchanged settings
        // records anyway; changing settings re-arms the guard.
        {
            let fps = if self.fps60 { 60 } else { 30 };
            let threads_eff = if self.threads == 0 {
                capabilities().cpu_threads.max(1)
            } else {
                self.threads
            };
            let enc_id = check_tier(codec, self.tier).map(|(id, _)| id).unwrap_or("libx264");
            let cpu_used = self.tier.vpx_cpu_used();
            let verdict = check_perf(enc_id, self.tier, cpu_used, width, height, fps, threads_eff);
            let key = format!("{enc_id}|{}|{width}x{height}@{}", self.tier.cli_name(), fps);
            if verdict.level != PerfLevel::Ok && self.perf_ack_key != key {
                self.perf_ack_key = key.clone();
                self.perf_offer_safe = verdict.level == PerfLevel::TooHeavy;
                self.error_msg = Some(format!(
                    "likely to drop frames: {}. Fix the settings — or press Record again to record anyway.",
                    verdict.message
                ));
                return;
            }
            self.perf_offer_safe = false;
        }
        match start_session(cfg, true) {
            Ok(sess) => {
                self.start_t = Instant::now();
                self.last_t = Instant::now();
                self.last_written = 0;
                self.out_fps = 0.0;
                self.snap = Snapshot::default();
                self.preview_tex = None;
                self.session = Some(sess);
                self.phase = Phase::Recording;
                // Already validated above; recompute for the note (cached probe).
                self.enc_note = check_tier(codec, self.tier)
                    .map(|(id, why)| format!("{} — {why}", encoder_display(id)))
                    .ok();
            }
            Err(e) => self.error_msg = Some(format!("couldn't start: {e:?}")),
        }
    }

    /// Graceful stop: signal threads, finalize the .webm on a helper thread
    /// (joining can take ~1s — never on the UI thread).
    fn begin_stop(&mut self) {
        let Some(sess) = self.session.take() else {
            self.phase = Phase::Idle;
            return;
        };
        sess.request_stop();
        let output = sess.output().to_owned();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let tail = sess.ffmpeg_tail(3);
            let result = sess.wait().map_err(|e| format!("{e:?}"));
            let _ = tx.send(StopOutcome { output, result, ffmpeg_tail: tail });
        });
        self.pending = Some(rx);
        self.phase = Phase::Stopping;
    }

    fn poll(&mut self) {
        if self.phase == Phase::Recording {
            if let Some(sess) = &self.session {
                self.snap = sess.snapshot();
                // output fps from written-counter delta
                let now = Instant::now();
                let dt = now.duration_since(self.last_t).as_secs_f32();
                if dt > 0.05 {
                    self.out_fps =
                        (self.snap.written - self.last_written) as f32 / dt;
                    self.last_written = self.snap.written;
                    self.last_t = now;
                }
                // auto-finish: duration reached, window closed, or capture error
                if sess.captures_done() {
                    self.begin_stop();
                }
            }
        }
        if self.phase == Phase::Stopping {
            if let Some(rx) = &self.pending {
                if let Ok(done) = rx.try_recv() {
                    self.pending = None;
                    self.phase = Phase::Idle;
                    match done.result {
                        Ok(s) => {
                            let mut msg = format!(
                                "Saved {}  ({} frames, {} dropped)",
                                done.output, s.written, s.dropped
                            );
                            if s.dropped > 0 && s.dropped * 20 > s.captured.max(1) {
                                if let Some(last) = done.ffmpeg_tail.last() {
                                    msg.push_str(&format!("  ffmpeg: {last}"));
                                }
                            }
                            self.done_msg = Some(msg);
                        }
                        Err(e) => {
                            if std::fs::metadata(&done.output)
                                .map(|m| m.len())
                                .unwrap_or(u64::MAX)
                                < 4096
                            {
                                let _ = std::fs::remove_file(&done.output);
                            }
                            self.error_msg = Some(format!("recording failed: {e}"));
                        }
                    }
                    self.refresh_library();
                }
            }
        }
    }

    /// Pull the latest preview frame (if any) and upload it as a texture.
    /// Called on the UI thread at repaint rate; old texture is freed on replace.
    fn poll_preview(&mut self, ctx: &egui::Context) {
        if !self.show_preview {
            return;
        }
        let Some(sess) = &self.session else { return };
        let Some(rx) = sess.preview_rx() else { return };
        let mut latest = None;
        while let Ok(f) = rx.try_recv() {
            latest = Some(f);
        }
        let Some(f) = latest else { return };
        if (f.width as usize) * (f.height as usize) * 4 != f.rgba.len() || f.rgba.is_empty() {
            return; // transient resize frame — skip, keep last good texture
        }
        let img = egui::ColorImage::from_rgba_unmultiplied(
            [f.width as usize, f.height as usize],
            &f.rgba,
        );
        self.preview_tex =
            Some(ctx.load_texture("preview", img, egui::TextureOptions::LINEAR));
    }

    // ---------- screenshots ----------

    /// Start a capture on a background thread (WGC setup + fullscreen
    /// overlay both block — never on the UI thread). Result lands in
    /// `shot_cap_rx`, polled in `poll_shots`.
    fn start_shot_capture(&mut self, mode: ShotMode) {
        if self.shot_busy {
            return;
        }
        self.error_msg = None;
        let monitor = self.monitors.get(self.monitor_sel).map(|m| m.index);
        let needle = if mode == ShotMode::Window {
            match self.win_sel.and_then(|i| self.windows.get(i)) {
                Some(w) => w.title.clone(),
                None => {
                    self.error_msg =
                        Some("Pick a window in §1 first (or use Fullscreen).".to_owned());
                    return;
                }
            }
        } else {
            String::new()
        };
        let (tx, rx) = mpsc::channel();
        self.shot_cap_rx = Some(rx);
        self.shot_busy = true;
        std::thread::spawn(move || {
            let res: anyhow::Result<Shot> = (|| {
                match mode {
                    ShotMode::Fullscreen => shot::capture_monitor(monitor),
                    ShotMode::Window => shot::capture_window(&needle),
                    ShotMode::Region => {
                        // Overlay first (blocks till drag/Esc), then one
                        // fullscreen grab + in-memory crop — no temp files.
                        let Some((x, y, w, h)) = shot::select_region()? else {
                            anyhow::bail!("snip cancelled");
                        };
                        let full = shot::capture_monitor(monitor)?;
                        shot::crop(&full, x, y, w, h)
                    }
                }
            })();
            let _ = tx.send(res.map_err(|e| format!("{e:?}")));
        });
    }

    /// Save + clipboard on a background thread (PNG/WebP encode of a 1080p
    /// frame is ~100 ms — never on the UI thread). Result lands in
    /// `shot_save_rx` and becomes a toast.
    fn save_shot_async(&mut self, img: Shot) {
        let (tx, rx) = mpsc::channel();
        self.shot_save_rx = Some(rx);
        let dir = self.shot_dir.clone();
        let format = self.shot_format;
        std::thread::spawn(move || {
            let saved = shot::save_shot(&img, &dir, None, format);
            let (result, copied) = match saved {
                Ok(path) => {
                    let copied = shot::copy_to_clipboard(&img).is_ok();
                    (Ok(path.clone()), copied)
                }
                Err(e) => (Err(format!("{e:?}")), false),
            };
            let _ = tx.send(ShotSaveOutcome {
                path: result.clone().unwrap_or_default(),
                copied,
                shot: img,
                result: result.map(|_| ()),
            });
        });
    }

    /// Drain screenshot channels: open the editor + autosave on capture,
    /// raise a toast on save. Textures upload here (needs `ctx`).
    fn poll_shots(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.shot_cap_rx {
            if let Ok(res) = rx.try_recv() {
                self.shot_cap_rx = None;
                self.shot_busy = false;
                match res {
                    Ok(img) => {
                        let tex = shot_texture(ctx, "shot-edit", &img);
                        self.shot_edit = Some(ShotEdit {
                            shot: img.clone(),
                            tex,
                            pen: 0,
                            pen_size: 6.0,
                            crop_mode: false,
                            crop_rect: None,
                            drag_start: None,
                            cur_stroke: Vec::new(),
                        });
                        // Autosave + clipboard now; the editor re-saves on
                        // demand after pen/crop edits.
                        self.save_shot_async(img);
                    }
                    Err(e) => {
                        if e != "snip cancelled" {
                            self.error_msg = Some(format!("screenshot: {e}"));
                        }
                    }
                }
            }
        }
        if let Some(rx) = &self.shot_save_rx {
            if let Ok(done) = rx.try_recv() {
                self.shot_save_rx = None;
                match done.result {
                    Ok(()) => {
                        // Desktop popup (bottom-right of the screen, 3 s,
                        // hover to keep) + a quiet line in the app.
                        shot::show_popup(done.shot, done.path.clone());
                        self.done_msg = Some(format!(
                            "Screenshot saved {}  ({})",
                            done.path,
                            if done.copied { "copied to clipboard" } else { "clipboard copy failed" }
                        ));
                    }
                    Err(e) => self.error_msg = Some(format!("screenshot save: {e}")),
                }
            }
        }
    }

    /// Global hotkeys (Win+PrtSc etc. — work even when Crabby isn't
    /// focused) plus in-app Snipping-Tool shortcuts (Alt+N/W/F new snip,
    /// Ctrl+S save, Ctrl+C copy, Esc close editor).
    fn poll_hotkeys(&mut self, ctx: &egui::Context) {
        // Global keys first: collect actions, then fire (busy guard dedups).
        let mut modes: Vec<ShotMode> = Vec::new();
        while let Ok(ev) = global_hotkey::GlobalHotKeyEvent::receiver().try_recv() {
            if let Some((_, m)) = self.hotkeys.bindings.iter().find(|(id, _)| *id == ev.id) {
                modes.push(*m);
            }
        }
        for m in modes {
            self.start_shot_capture(m);
        }

        // In-app shortcuts — never while typing in a text field.
        if ctx.wants_keyboard_input() {
            return;
        }
        let mut save_edit = false;
        let mut copy_edit = false;
        ctx.input_mut(|i| {
            if i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::ALT,
                egui::Key::N,
            )) {
                self.start_shot_capture(ShotMode::Region);
            }
            if i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::ALT,
                egui::Key::W,
            )) {
                self.start_shot_capture(ShotMode::Window);
            }
            if i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::ALT,
                egui::Key::F,
            )) {
                self.start_shot_capture(ShotMode::Fullscreen);
            }
            if self.shot_edit.is_some() {
                if i.consume_shortcut(&egui::KeyboardShortcut::new(
                    egui::Modifiers::CTRL,
                    egui::Key::S,
                )) {
                    save_edit = true;
                }
                if i.consume_shortcut(&egui::KeyboardShortcut::new(
                    egui::Modifiers::CTRL,
                    egui::Key::C,
                )) {
                    copy_edit = true;
                }
            }
        });
        if save_edit {
            if let Some(ed) = &self.shot_edit {
                self.save_shot_async(ed.shot.clone());
            }
        }
        if copy_edit {
            if let Some(ed) = &self.shot_edit {
                match shot::copy_to_clipboard(&ed.shot) {
                    Ok(()) => {
                        self.done_msg = Some("Screenshot copied to clipboard.".to_owned())
                    }
                    Err(e) => self.error_msg = Some(format!("copy failed: {e:?}")),
                }
            }
        }
        // Esc closes the editor (the region overlay handles its own Esc).
        if self.shot_edit.is_some() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.shot_edit = None;
        }
    }

}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        self.update.poll();
        self.poll_shots(ctx);
        self.poll_hotkeys(ctx);
        // Throttled repaints: ~10 Hz while recording (preview + stats),
        // slower while finalizing, fully static when idle (0% UI cost).
        if self.phase == Phase::Recording {
            self.poll_preview(ctx);
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if self.phase != Phase::Idle {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        // Keep the update badge / progress bar alive without busy-looping.
        if self.update.phase == UpdatePhase::Checking
            || self.update.phase == UpdatePhase::Downloading
        {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        let recording = self.phase != Phase::Idle;

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Crabby");
                ui.label("Screen Recorder · 1080p60");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (dot, txt) = match self.phase {
                        Phase::Idle => (egui::Color32::GRAY, "idle"),
                        Phase::Recording => (egui::Color32::RED, "● REC"),
                        Phase::Stopping => (egui::Color32::YELLOW, "stopping…"),
                    };
                    ui.colored_label(dot, txt);
                    ui.separator();
                    // Self-update controls (right side, left of the status).
                    match self.update.phase {
                        UpdatePhase::Available => {
                            if ui
                                .button(egui::RichText::new("Update available").strong())
                                .on_hover_text("A newer Crabby is ready — see what's new")
                                .clicked()
                            {
                                self.update.show_dialog = true;
                            }
                        }
                        UpdatePhase::Downloading => {
                            let pct = if self.update.total > 0 {
                                self.update.done as f32 / self.update.total as f32
                            } else {
                                0.0
                            };
                            ui.add(
                                egui::ProgressBar::new(pct)
                                    .desired_width(90.0)
                                    .show_percentage(),
                            );
                        }
                        UpdatePhase::Ready => {
                            if ui
                                .button(egui::RichText::new("Restart to update").strong())
                                .clicked()
                            {
                                self.update.show_dialog = true;
                            }
                        }
                        UpdatePhase::Checking => {
                            ui.spinner();
                        }
                        _ => {
                            if ui
                                .small_button("Check for updates")
                                .on_hover_text("Ask GitHub for a newer Crabby")
                                .clicked()
                            {
                                self.update.spawn_check();
                            }
                        }
                    }
                    ui.weak(format!("v{}", updater::current_version()));
                });
            });
        });

        // Self-update dialog (modal-ish, closable, never blocks recording).
        if self.update.show_dialog {
            let mut open = true;
            egui::Window::new("Crabby update")
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .default_size([420.0, 300.0])
                .show(ctx, |ui| match self.update.phase {
                    UpdatePhase::Available => {
                        let tag = self
                            .update
                            .info
                            .as_ref()
                            .map(|i| i.tag.clone())
                            .unwrap_or_default();
                        ui.heading(format!("{tag} is ready"));
                        ui.label(format!(
                            "You're on v{} — no reinstall needed, one click swaps the exe.",
                            updater::current_version()
                        ));
                        ui.separator();
                        ui.strong("What's new:");
                        egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                            ui.label(
                                self.update
                                    .info
                                    .as_ref()
                                    .map(|i| i.notes.as_str())
                                    .unwrap_or(""),
                            );
                        });
                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui
                                .button(egui::RichText::new("Download + restart").strong())
                                .clicked()
                            {
                                self.update.spawn_download();
                            }
                            if ui.button("Later").clicked() {
                                self.update.show_dialog = false;
                            }
                        });
                    }
                    UpdatePhase::Downloading => {
                        let (done, total) = (self.update.done, self.update.total);
                        ui.label("Downloading update…");
                        ui.add(
                            egui::ProgressBar::new(if total > 0 {
                                done as f32 / total as f32
                            } else {
                                0.0
                            })
                            .show_percentage(),
                        );
                        ui.weak(format!(
                            "{} / {} MB",
                            done / 1_048_576,
                            total.max(1) / 1_048_576
                        ));
                    }
                    UpdatePhase::Ready => {
                        ui.heading("Ready to restart");
                        ui.label("The new version is downloaded. Restart swaps it in.");
                        ui.horizontal(|ui| {
                            if ui
                                .button(egui::RichText::new("Restart now").strong())
                                .clicked()
                            {
                                if let Some(staged) = self.update.staged.clone() {
                                    if let Err(e) =
                                        updater::install_and_restart(&staged)
                                    {
                                        self.error_msg =
                                            Some(format!("couldn't install update: {e:?}"));
                                        self.update.show_dialog = false;
                                    }
                                    // On success this process exits via the updater.
                                }
                            }
                            if ui.button("Later").clicked() {
                                self.update.show_dialog = false;
                            }
                        });
                    }
                    UpdatePhase::Failed => {
                        ui.colored_label(
                            egui::Color32::from_rgb(255, 120, 120),
                            self.update.error.clone().unwrap_or_else(|| "update failed".into()),
                        );
                        ui.horizontal(|ui| {
                            if ui.button("Retry").clicked() {
                                self.update.spawn_check();
                            }
                            if ui.button("Close").clicked() {
                                self.update.show_dialog = false;
                            }
                        });
                    }
                    _ => {
                        ui.label("You're on the latest version.");
                        if ui.button("Close").clicked() {
                            self.update.show_dialog = false;
                        }
                    }
                });
            if !open {
                self.update.show_dialog = false;
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                // Live preview — what you see here is what gets saved.
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.strong("Preview");
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.checkbox(&mut self.show_preview, "Live");
                            },
                        );
                    });
                    let avail = ui.available_width();
                    let h = (avail * 9.0 / 16.0).clamp(90.0, 240.0);
                    if self.show_preview {
                        if let Some(tex) = &self.preview_tex {
                            ui.add(
                                egui::Image::new(tex)
                                    .fit_to_exact_size(egui::vec2(avail, h)),
                            );
                        } else {
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(avail, h),
                                egui::Sense::hover(),
                            );
                            ui.painter().rect_filled(
                                rect,
                                8.0,
                                egui::Color32::BLACK,
                            );
                            let msg = if recording {
                                "Waiting for first frame…"
                            } else {
                                "Hit Record — this is exactly what gets saved"
                            };
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                msg,
                                egui::FontId::proportional(14.0),
                                egui::Color32::GRAY,
                            );
                        }
                    } else {
                        ui.weak("Preview off (saves a little CPU). Recording still runs full quality.");
                    }
                });

                ui.add_space(4.0);
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.strong("1 · What to record");
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui
                                    .add_enabled(!recording, egui::Button::new("Refresh"))
                                    .clicked()
                                {
                                    self.refresh_sources();
                                }
                            },
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(!recording, |ui| {
                            ui.radio_value(&mut self.use_window, false, "Monitor");
                            ui.radio_value(&mut self.use_window, true, "Window (game)");
                        });
                    });
                    if !self.use_window {
                        let label = self
                            .monitors
                            .get(self.monitor_sel)
                            .map(|m| format!("#{} {} ({}x{})", m.index, m.name, m.w, m.h))
                            .unwrap_or_else(|| "no monitors found".to_owned());
                        egui::ComboBox::from_id_salt("mon")
                            .selected_text(label)
                            .show_ui(ui, |ui| {
                                for (i, m) in self.monitors.iter().enumerate() {
                                    ui.selectable_value(
                                        &mut self.monitor_sel,
                                        i,
                                        format!("#{} {} ({}x{} @{}Hz)", m.index, m.name, m.w, m.h, m.hz),
                                    );
                                }
                            });
                    } else {
                        ui.horizontal(|ui| {
                            ui.label("App:");
                            let filt = self.win_filter.to_lowercase();
                            let items: Vec<(usize, String, String)> = self
                                .windows
                                .iter()
                                .enumerate()
                                .filter(|(_, w)| {
                                    filt.is_empty()
                                        || w.title.to_lowercase().contains(&filt)
                                        || w.process.to_lowercase().contains(&filt)
                                })
                                .map(|(i, w)| {
                                    let t: String =
                                        w.title.chars().take(38).collect();
                                    (i, t, w.process.clone())
                                })
                                .collect();
                            let label = self
                                .win_sel
                                .and_then(|i| self.windows.get(i))
                                .map(|w| {
                                    w.title.chars().take(38).collect::<String>()
                                })
                                .unwrap_or_else(|| "pick an app…".to_owned());
                            let mut picked = self.win_sel;
                            egui::ComboBox::from_id_salt("win")
                                .selected_text(label)
                                .width(260.0)
                                .show_ui(ui, |ui| {
                                    for (i, t, p) in &items {
                                        ui.selectable_value(
                                            &mut picked,
                                            Some(*i),
                                            format!("{t}  ({p})"),
                                        );
                                    }
                                });
                            self.win_sel = picked;
                            ui.add_enabled(
                                !recording,
                                egui::TextEdit::singleline(&mut self.win_filter)
                                    .hint_text("filter")
                                    .desired_width(80.0),
                            );
                        });
                    }
                    if self.use_window {
                        match self.win_sel.and_then(|i| self.windows.get(i)) {
                            Some(w) => ui.colored_label(
                                egui::Color32::GREEN,
                                format!("Will record: {}", w.title),
                            ),
                            None => ui.weak(
                                "Select an app above — or use Monitor for fullscreen games.",
                            ),
                        };
                    }
                    if let Some(e) = &self.lists_error {
                        ui.colored_label(egui::Color32::YELLOW, e);
                    }
                });

                ui.group(|ui| {
                    ui.strong("2 · Where to save");
                    ui.horizontal(|ui| {
                        ui.label("Folder:");
                        ui.add_enabled(
                            !recording,
                            egui::TextEdit::singleline(&mut self.dir).desired_width(300.0),
                        );
                        if ui.add_enabled(!recording, egui::Button::new("Browse…")).clicked() {
                            if let Some(p) = rfd::FileDialog::new()
                                .set_directory(&self.dir)
                                .pick_folder()
                            {
                                self.dir = p.to_string_lossy().into_owned();
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("File:");
                        ui.add_enabled(
                            !recording,
                            egui::TextEdit::singleline(&mut self.filename)
                                .hint_text("recording")
                                .desired_width(300.0),
                        );
                    });
                    ui.weak("Container extension (.mp4/.mkv/.webm) is added automatically.");
                    ui.weak("Never overwritten — existing names get _001, _002…");
                });

                ui.group(|ui| {
                    ui.strong("3 · Quality");
                    ui.add_enabled_ui(!recording, |ui| {
                        ui.radio_value(&mut self.tier, Tier::Fastest, Tier::Fastest.label());
                        ui.weak("Lowest load. Weak hardware or quick drafts.");
                        ui.radio_value(&mut self.tier, Tier::Balanced, Tier::Balanced.label());
                        ui.weak("Good quality per bit on any machine.");
                        ui.radio_value(&mut self.tier, Tier::High, Tier::High.label());
                        ui.weak("Higher bitrate, slower preset. Needs headroom.");
                        ui.radio_value(&mut self.tier, Tier::Lossless, Tier::Lossless.label());
                        ui.weak("Exact pixels, huge files. x264/VP9 only.");
                    });
                    ui.horizontal(|ui| {
                        ui.label("Bitrate:");
                        let sel_txt = self
                            .selected_bitrate()
                            .unwrap_or_else(|| "Auto (tier)".to_owned());
                        egui::ComboBox::from_id_salt("br")
                            .selected_text(sel_txt)
                            .show_ui(ui, |ui| {
                                for (i, b) in BITRATES.iter().enumerate() {
                                    ui.selectable_value(&mut self.bitrate_sel, i, *b);
                                }
                            });
                        ui.label("FPS:");
                        ui.add_enabled_ui(!recording, |ui| {
                            ui.radio_value(&mut self.fps60, true, "60");
                            ui.radio_value(&mut self.fps60, false, "30");
                        });
                    });
                    ui.horizontal(|ui| {
                        ui.label("Audio:");
                        egui::ComboBox::from_id_salt("aud")
                            .selected_text(self.audio.label())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.audio,
                                    AudioMode::System,
                                    AudioMode::System.label(),
                                );
                                ui.selectable_value(
                                    &mut self.audio,
                                    AudioMode::Mic,
                                    AudioMode::Mic.label(),
                                );
                                ui.selectable_value(
                                    &mut self.audio,
                                    AudioMode::Both,
                                    AudioMode::Both.label(),
                                );
                                ui.selectable_value(
                                    &mut self.audio,
                                    AudioMode::Off,
                                    AudioMode::Off.label(),
                                );
                            });
                        ui.label("Size:");
                        egui::ComboBox::from_id_salt("res")
                            .selected_text(
                                RES_LABELS[self.res_sel.min(RES_LABELS.len() - 1)],
                            )
                            .show_ui(ui, |ui| {
                                for (i, l) in RES_LABELS.iter().enumerate() {
                                    ui.selectable_value(&mut self.res_sel, i, *l);
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label("Encoder:");
                        egui::ComboBox::from_id_salt("enc")
                            .selected_text(ENC_NAMES[self.enc_sel.min(ENC_NAMES.len() - 1)])
                            .show_ui(ui, |ui| {
                                for (i, n) in ENC_NAMES.iter().enumerate() {
                                    ui.selectable_value(&mut self.enc_sel, i, *n);
                                }
                            });
                        ui.label("File:");
                        let cont_names = ["MP4", "MKV", "WebM"];
                        egui::ComboBox::from_id_salt("cont")
                            .selected_text(cont_names[self.container_sel.min(2)])
                            .show_ui(ui, |ui| {
                                for (i, n) in cont_names.iter().enumerate() {
                                    ui.selectable_value(&mut self.container_sel, i, *n);
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label("Sound:");
                        let ac_names = ["Auto", "Opus", "AAC"];
                        let ac_sel = match self.audio_codec_sel {
                            None => 0,
                            Some(AudioCodec::Opus) => 1,
                            Some(AudioCodec::Aac) => 2,
                        };
                        egui::ComboBox::from_id_salt("acodec")
                            .selected_text(ac_names[ac_sel])
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(ac_sel == 0, ac_names[0]).clicked() {
                                    self.audio_codec_sel = None;
                                }
                                if ui.selectable_label(ac_sel == 1, ac_names[1]).clicked() {
                                    self.audio_codec_sel = Some(AudioCodec::Opus);
                                }
                                if ui.selectable_label(ac_sel == 2, ac_names[2]).clicked() {
                                    self.audio_codec_sel = Some(AudioCodec::Aac);
                                }
                            });
                        ui.weak("Auto: AAC for MP4, Opus otherwise.");
                    });
                    ui.weak("Win10 has no per-app audio — mute other apps for clean game sound.");
                    ui.horizontal(|ui| {
                        ui.label("Threads:");
                        let max_t = capabilities().cpu_threads.max(1);
                        ui.add_enabled(
                            !recording,
                            egui::Slider::new(&mut self.threads, 0..=max_t),
                        );
                        ui.weak(if self.threads == 0 {
                            format!("Auto ({max_t})")
                        } else {
                            String::new()
                        });
                    });
                    ui.horizontal(|ui| {
                        ui.add_enabled(
                            !recording,
                            egui::Checkbox::new(&mut self.no_cursor, "Hide cursor"),
                        );
                        ui.add_enabled(
                            !recording,
                            egui::Checkbox::new(&mut self.auto_stop, "Auto-stop"),
                        );
                        if self.auto_stop {
                            ui.add_enabled(
                                !recording,
                                egui::Slider::new(&mut self.stop_secs, 5..=600).suffix("s"),
                            );
                        }
                    });
                });

                ui.add_space(6.0);
                ui.vertical_centered(|ui| {
                    match self.phase {
                        Phase::Idle => {
                            let btn = egui::Button::new(
                                egui::RichText::new("●  Record").size(17.0).strong(),
                            )
                            .fill(egui::Color32::from_rgb(200, 40, 40))
                            .min_size(egui::vec2(180.0, 38.0));
                            if ui.add(btn).clicked() {
                                self.start();
                            }
                        }
                        Phase::Recording => {
                            let btn = egui::Button::new(
                                egui::RichText::new("■  Stop").size(17.0).strong(),
                            )
                            .min_size(egui::vec2(180.0, 38.0));
                            if ui.add(btn).clicked() {
                                self.begin_stop();
                            }
                            let el = self
                                .session
                                .as_ref()
                                .map(|s| s.elapsed().as_secs())
                                .unwrap_or(0);
                            ui.monospace(format!(
                                "{:02}:{:02}   {:.0} fps   captured {}   dropped {}",
                                el / 60,
                                el % 60,
                                self.out_fps,
                                self.snap.captured,
                                self.snap.dropped
                            ));
                            if self.snap.dropped > 0
                                && self.snap.dropped * 20 > self.snap.captured.max(1)
                            {
                                ui.colored_label(
                                    egui::Color32::YELLOW,
                                    "Dropping frames — step down a tier or try 30 fps.",
                                );
                                // ffmpeg's own complaint (queue overflow,
                                // timestamp gaps, …) when it has one.
                                if let Some(sess) = self.session.as_ref() {
                                    if let Some(line) =
                                        sess.ffmpeg_tail(1).into_iter().next()
                                    {
                                        ui.weak(format!("ffmpeg: {line}"));
                                    }
                                }
                            }
                            if let Some(n) = &self.enc_note {
                                ui.weak(n);
                            }
                        }
                        Phase::Stopping => {
                            ui.spinner();
                            ui.label("Finalizing video…");
                        }
                    }
                    // One-click escape hatch after a perf warning: settings
                    // a software encoder can actually hold on this machine.
                    if self.perf_offer_safe && self.phase == Phase::Idle {
                        ui.add_space(4.0);
                        if ui
                            .button("Use safe 720p30 Fastest")
                            .on_hover_text("720p + 30 fps + Fastest tier + Auto H.264, then press Record")
                            .clicked()
                        {
                            self.tier = Tier::Fastest;
                            self.fps60 = false;
                            self.res_sel = 2; // 720p fixed
                            self.enc_sel = 0; // Auto (H.264 best available)
                            self.bitrate_sel = 0; // Auto (tier)
                            self.perf_offer_safe = false;
                            self.error_msg = None;
                            self.done_msg = Some(
                                "Safe settings applied (720p30 Fastest) — press Record.".to_owned(),
                            );
                        }
                    }
                });

                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.strong(format!("4 · Recordings ({})", self.lib_files.len()));
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.button("Open folder").clicked()
                                    && std::process::Command::new("explorer")
                                        .arg(&self.dir)
                                        .spawn()
                                        .is_err()
                                {
                                    self.error_msg =
                                        Some("couldn't open the save folder".to_owned());
                                }
                                if ui.button("Refresh").clicked() {
                                    self.refresh_library();
                                }
                            },
                        );
                    });
                    if self.lib_files.is_empty() {
                        ui.weak("No recordings yet — finished videos land here.");
                    } else {
                        let mut play_path: Option<String> = None;
                        let mut del_idx: Option<usize> = None;
                        egui::ScrollArea::vertical().max_height(96.0).show(ui, |ui| {
                            for (i, f) in self.lib_files.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    let mut nm = f.name.clone();
                                    if nm.len() > 32 {
                                        nm.truncate(29);
                                        nm.push('…');
                                    }
                                    ui.monospace(nm);
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui.small_button("Del").clicked() {
                                                del_idx = Some(i);
                                            }
                                            if ui.small_button("Play").clicked() {
                                                play_path = Some(f.path.clone());
                                            }
                                            ui.weak(format!(
                                                "{} · {}",
                                                fmt_size(f.bytes),
                                                fmt_age(f.modified)
                                            ));
                                        },
                                    );
                                });
                            }
                        });
                        if let Some(p) = play_path {
                            let mut cmd = std::process::Command::new("cmd");
                            cmd.args(["/C", "start", "", &p]);
                            #[cfg(target_os = "windows")]
                            {
                                cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
                            }
                            if cmd.spawn().is_err() {
                                self.error_msg = Some(format!("couldn't play {p}"));
                            }
                        }
                        if let Some(i) = del_idx {
                            if let Some(f) = self.lib_files.get(i) {
                                let _ = std::fs::remove_file(&f.path);
                            }
                            self.refresh_library();
                        }
                    }
                });

                ui.group(|ui| {
                    ui.strong("5 · Screenshot (Snipping-Tool style)");
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(!self.shot_busy && !recording, |ui| {
                            if ui
                                .button("▢ Region")
                                .on_hover_text("Drag a rectangle (Esc cancels)")
                                .clicked()
                            {
                                self.start_shot_capture(ShotMode::Region);
                            }
                            if ui
                                .button("▣ Window")
                                .on_hover_text("Capture the window picked in §1")
                                .clicked()
                            {
                                self.start_shot_capture(ShotMode::Window);
                            }
                            if ui
                                .button("⛶ Fullscreen")
                                .on_hover_text("Capture the monitor picked in §1")
                                .clicked()
                            {
                                self.start_shot_capture(ShotMode::Fullscreen);
                            }
                        });
                        if self.shot_busy {
                            ui.spinner();
                            ui.weak("snipping…");
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Format:");
                        egui::ComboBox::from_id_salt("shotfmt")
                            .selected_text(self.shot_format.label())
                            .show_ui(ui, |ui| {
                                for f in [
                                    ShotFormat::Png,
                                    ShotFormat::Webp,
                                    ShotFormat::Jpg,
                                    ShotFormat::Bmp,
                                ] {
                                    ui.selectable_value(&mut self.shot_format, f, f.label());
                                }
                            });
                        ui.label("Folder:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.shot_dir)
                                .desired_width(220.0),
                        );
                        if ui.button("Browse…").clicked() {
                            if let Some(p) = rfd::FileDialog::new()
                                .set_directory(&self.shot_dir)
                                .pick_folder()
                            {
                                self.shot_dir = p.to_string_lossy().into_owned();
                            }
                        }
                    });
                    ui.weak("Auto-saves + copies to clipboard. Region snips the primary monitor; Window/Fullscreen follow §1. JPG = fastest, WebP = small + sharp.");
                    ui.weak(&self.hotkeys.status);
                    ui.weak("In-app: Alt+N region · Alt+W window · Alt+F fullscreen · Ctrl+S save · Ctrl+C copy · Esc close.");

                    if self.shot_edit.is_some() {
                        ui.separator();
                        let mut do_save = false;
                        let mut do_copy = false;
                        let mut do_close = false;
                        let mut apply_crop = false;
                        ui.horizontal(|ui| {
                            ui.strong("Edit");
                            let ed = self.shot_edit.as_mut().expect("checked above");
                            egui::ComboBox::from_id_salt("shotpen")
                                .selected_text(PEN_COLORS[ed.pen].1)
                                .show_ui(ui, |ui| {
                                    for (i, (_, n)) in PEN_COLORS.iter().enumerate() {
                                        ui.selectable_value(&mut ed.pen, i, *n);
                                    }
                                });
                            ui.add(
                                egui::Slider::new(&mut ed.pen_size, 2.0..=24.0).suffix("px"),
                            );
                            ui.checkbox(&mut ed.crop_mode, "Crop");
                            if ed.crop_mode && ui.button("Apply crop").clicked() {
                                apply_crop = true;
                            }
                            if ui.button("Save").clicked() {
                                do_save = true;
                            }
                            if ui.button("Copy").clicked() {
                                do_copy = true;
                            }
                            if ui.button("Close").clicked() {
                                do_close = true;
                            }
                        });
                        if apply_crop {
                            let mut err = None;
                            if let Some(ed) = self.shot_edit.as_mut() {
                                match ed.crop_rect {
                                    Some(r) => {
                                        let x = r.min.x.round().max(0.0) as u32;
                                        let y = r.min.y.round().max(0.0) as u32;
                                        let w = r.width().round() as u32;
                                        let h = r.height().round() as u32;
                                        match shot::crop(&ed.shot, x, y, w, h) {
                                            Ok(c) => {
                                                ed.shot = c;
                                                ed.tex =
                                                    shot_texture(ctx, "shot-edit", &ed.shot);
                                                ed.crop_rect = None;
                                            }
                                            Err(e) => err = Some(format!("{e:?}")),
                                        }
                                    }
                                    None => {
                                        err = Some(
                                            "drag on the image to pick a crop first".to_owned(),
                                        )
                                    }
                                }
                            }
                            if let Some(e) = err {
                                self.error_msg = Some(e);
                            }
                        }
                        if do_save {
                            if let Some(ed) = &self.shot_edit {
                                self.save_shot_async(ed.shot.clone());
                            }
                        }
                        if do_copy {
                            if let Some(ed) = &self.shot_edit {
                                match shot::copy_to_clipboard(&ed.shot) {
                                    Ok(()) => {
                                        self.done_msg = Some(
                                            "Screenshot copied to clipboard.".to_owned(),
                                        )
                                    }
                                    Err(e) => {
                                        self.error_msg =
                                            Some(format!("copy failed: {e:?}"))
                                    }
                                }
                            }
                        }
                        if do_close {
                            self.shot_edit = None;
                        }
                        // Canvas: drag to draw (pen) or to mark the crop rect.
                        // Strokes commit to pixels on release — one texture
                        // upload per stroke keeps 4K edits smooth.
                        if let Some(ed) = self.shot_edit.as_mut() {
                            let (w, h) = (ed.shot.width as f32, ed.shot.height as f32);
                            let avail = ui.available_width();
                            let scale = (avail / w).min(420.0 / h).clamp(0.05, 1.0);
                            let size = egui::vec2(w * scale, h * scale);
                            let tex_id = ed.tex.as_ref().map(|t| t.id());
                            let (rect, resp) =
                                ui.allocate_exact_size(size, egui::Sense::drag());
                            if let Some(tid) = tex_id {
                                ui.painter().image(
                                    tid,
                                    rect,
                                    egui::Rect::from_min_max(
                                        egui::pos2(0.0, 0.0),
                                        egui::pos2(1.0, 1.0),
                                    ),
                                    egui::Color32::WHITE,
                                );
                            }
                            let to_img = |p: egui::Pos2| {
                                egui::pos2(
                                    (p.x - rect.min.x) / scale,
                                    (p.y - rect.min.y) / scale,
                                )
                            };
                            let to_screen = |p: egui::Pos2| {
                                egui::pos2(
                                    rect.min.x + p.x * scale,
                                    rect.min.y + p.y * scale,
                                )
                            };
                            if resp.drag_started() {
                                if let Some(p) = resp.interact_pointer_pos() {
                                    let q = to_img(p);
                                    ed.drag_start = Some(q);
                                    if !ed.crop_mode {
                                        ed.cur_stroke = vec![q];
                                    }
                                }
                            }
                            if resp.dragged() {
                                if let (Some(s), Some(p)) =
                                    (ed.drag_start, resp.interact_pointer_pos())
                                {
                                    let cur = to_img(p);
                                    if ed.crop_mode {
                                        ed.crop_rect =
                                            Some(egui::Rect::from_two_pos(s, cur));
                                    } else {
                                        ed.cur_stroke.push(cur);
                                    }
                                }
                            }
                            if resp.drag_stopped() {
                                if !ed.crop_mode {
                                    let (color, _) = PEN_COLORS[ed.pen];
                                    let px = ed.pen_size;
                                    let pts = std::mem::take(&mut ed.cur_stroke);
                                    if pts.len() == 1 {
                                        let p = pts[0];
                                        shot::stroke_line(
                                            &mut ed.shot, p.x, p.y, p.x, p.y, color, px,
                                        );
                                    } else {
                                        for pair in pts.windows(2) {
                                            shot::stroke_line(
                                                &mut ed.shot,
                                                pair[0].x,
                                                pair[0].y,
                                                pair[1].x,
                                                pair[1].y,
                                                color,
                                                px,
                                            );
                                        }
                                    }
                                    ed.tex = shot_texture(ctx, "shot-edit", &ed.shot);
                                }
                                ed.drag_start = None;
                            }
                            if ed.crop_mode {
                                if let Some(r) = ed.crop_rect {
                                    let rs = egui::Rect::from_two_pos(
                                        to_screen(r.min),
                                        to_screen(r.max),
                                    );
                                    ui.painter().rect_filled(
                                        rs,
                                        0.0,
                                        egui::Color32::from_white_alpha(20),
                                    );
                                    ui.painter().rect_stroke(
                                        rs,
                                        0.0,
                                        egui::Stroke::new(
                                            2.0_f32,
                                            egui::Color32::from_rgb(88, 101, 242),
                                        ),
                                        egui::StrokeKind::Outside,
                                    );
                                }
                                ui.weak("Drag on the image to pick the crop, then Apply crop.");
                            } else {
                                if ed.cur_stroke.len() >= 2 {
                                    let (color, _) = PEN_COLORS[ed.pen];
                                    let pts: Vec<egui::Pos2> = ed
                                        .cur_stroke
                                        .iter()
                                        .map(|p| to_screen(*p))
                                        .collect();
                                    ui.painter().add(egui::Shape::line(
                                        pts,
                                        egui::Stroke::new(
                                            ed.pen_size * scale,
                                            egui::Color32::from_rgba_unmultiplied(
                                                color[0], color[1], color[2], color[3],
                                            ),
                                        ),
                                    ));
                                }
                                ui.weak("Drag on the image to draw. Save re-saves the edited shot.");
                            }
                        }
                    }
                });

                if let Some(m) = &self.done_msg {
                    ui.add_space(4.0);
                    ui.colored_label(egui::Color32::GREEN, m);
                }
                if let Some(e) = &self.error_msg {
                    ui.add_space(4.0);
                    ui.colored_label(egui::Color32::from_rgb(255, 120, 120), e);
                }

                ui.add_space(4.0);
                ui.separator();
                ui.weak("Tip: run games borderless-windowed — exclusive fullscreen can't be captured (same in OBS).");
            });
        });
    }
}

pub fn run(updated_from: Option<String>) -> Result<()> {
    let icon =
        eframe::icon_data::from_png_bytes(&include_bytes!("../assets/icon-256.png")[..])
            .expect("assets/icon-256.png is corrupt");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([680.0, 800.0])
            .with_min_inner_size([560.0, 620.0])
            .with_icon(icon),
        ..Default::default()
    };
    eframe::run_native(
        "Crabby Screen Recorder",
        options,
        Box::new(move |cc| Ok(Box::new(GuiApp::new(cc, updated_from)) as Box<dyn eframe::App>)),
    )
    .map_err(|e| anyhow::anyhow!("GUI failed: {e}"))
}
