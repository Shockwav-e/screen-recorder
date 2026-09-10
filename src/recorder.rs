//! Shared recording core: WGC capture -> bounded queue -> ffmpeg WebM.
//! Used by both the CLI and the egui GUI.
//!
//! Performance design (i5-4570, 4 threads, 16 GB, 1080p display):
//!   - GPU-composited event-driven capture (idle screen ~= 0% CPU)
//!   - fixed-size pipe, center crop/pad in-Rust (cheap memcpy, no scaler)
//!   - bounded 2-frame channel + try_send (never blocks capture; drops = realtime)
//!   - buffer reuse (no per-frame alloc), CFR pacer thread, realtime libvpx flags

use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use clap::ValueEnum;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Codec {
    /// WebM VP8 — ~30-40% cheaper CPU than VP9. Best for weak CPUs / long sessions.
    Vp8,
    /// WebM VP9 — better compression at the same bitrate. Best for YouTube uploads.
    Vp9,
}

impl Codec {
    pub fn label(self) -> &'static str {
        match self {
            Codec::Vp8 => "VP8",
            Codec::Vp9 => "VP9",
        }
    }
}

/// Quality presets. YouTube re-encodes everything you upload, so starting from
/// a high-bitrate master is what keeps the final video sharp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum Quality {
    /// VP9 1080p60 @ 16M, cpu-used 5. For YouTube uploads (recommended: YT
    /// wants ~12 Mbps for 1080p60, 16M master gives it headroom).
    #[default]
    Youtube,
    /// VP8 1080p60 @ 8M, cpu-used 8 (fastest). Lowest CPU, still decent.
    Balanced,
}

impl Quality {
    pub fn codec(self) -> Codec {
        match self {
            Quality::Youtube => Codec::Vp9,
            Quality::Balanced => Codec::Vp8,
        }
    }
    pub fn bitrate(self) -> &'static str {
        match self {
            Quality::Youtube => "16M",
            Quality::Balanced => "8M",
        }
    }
    pub fn cpu_used(self) -> u8 {
        match self {
            Quality::Youtube => 5,
            Quality::Balanced => 8,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Quality::Youtube => "YouTube HQ (VP9 16M)",
            Quality::Balanced => "Balanced (VP8 8M, low CPU)",
        }
    }
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
    pub bitrate: String,
    pub cpu_used: u8,
    pub threads: u32,
    pub duration: Option<u64>,
    pub no_cursor: bool,
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
/// directory component or drive letter is used as-is. Enforces `.webm`,
/// creates parent folders, and auto-increments (`name_001.webm`) so recordings
/// are never silently overwritten.
pub fn resolve_output(output: &str, dir: &str) -> Result<String> {
    let mut out = output.to_owned();
    if !out.to_lowercase().ends_with(".webm") {
        out.push_str(".webm");
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
            nopad: Vec::with_capacity(1920 * 1080 * 4),
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
        // Channel depth 2 keeps RAM flat; drops = realtime pacing, like OBS.
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

// ---------- ffmpeg ----------

fn spawn_ffmpeg(cfg: &RecordConfig) -> Result<ffmpeg_sidecar::child::FfmpegChild> {
    let _ = ffmpeg_sidecar::download::auto_download().context("ffmpeg auto-download failed");

    let enc: &str = match cfg.codec {
        Codec::Vp8 => "libvpx",
        Codec::Vp9 => "libvpx-vp9",
    };
    // Realtime-tuned libvpx args: lowest-latency path that still looks good.
    // cpu-used 8 = fastest (lowest CPU), lower = better compression per bit
    // (YouTube preset uses 5). row-mt + tiles let VP9 use all 4 threads.
    let extra: Vec<String> = match cfg.codec {
        Codec::Vp8 => vec![
            "-deadline".into(),
            "realtime".into(),
            "-cpu-used".into(),
            cfg.cpu_used.clamp(0, 8).to_string(),
            "-lag-in-frames".into(),
            "16".into(),
            "-auto-alt-ref".into(),
            "1".into(),
            "-g".into(),
            (cfg.fps * 2).to_string(),
        ],
        Codec::Vp9 => vec![
            "-deadline".into(),
            "realtime".into(),
            "-cpu-used".into(),
            cfg.cpu_used.clamp(0, 8).to_string(),
            "-row-mt".into(),
            "1".into(),
            "-tile-columns".into(),
            "2".into(),
            "-tile-rows".into(),
            "1".into(),
            "-lag-in-frames".into(),
            "16".into(),
            "-auto-alt-ref".into(),
            "1".into(),
            "-g".into(),
            (cfg.fps * 2).to_string(),
        ],
    };

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
        "-an",
        "-c:v",
        enc,
        "-b:v",
        &cfg.bitrate,
        "-threads",
        &threads.to_string(),
        "-pix_fmt",
        "yuv420p",
    ]);
    for e in &extra {
        cmd.arg(e);
    }
    // NOTE: no -vsync/-fps_mode flag on purpose. Our writer thread already
    // paces stdin at exactly CFR, and the vsync option was removed in recent
    // ffmpeg builds (Unrecognized option 'vsync') — passing it kills ffmpeg.
    cmd.arg(&cfg.output);
    cmd.spawn().context(
        "failed to spawn ffmpeg. Install it (winget install Gyan.FFmpeg) or let lite-rec auto-download it",
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
        drop(stdin); // EOF -> ffmpeg finalizes the .webm
        let _ = child.wait();
        Ok(())
    })();
    if let Err(e) = res {
        *lock_err(&error) = Some(format!("encoder: {e:?}"));
    }
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
    preview_rx: Option<Receiver<PreviewFrame>>,
}

/// Start a recording. `preview` opens a latest-only preview tap for the GUI
/// (the CLI passes false — zero preview cost).
pub fn start_session(cfg: RecordConfig, preview: bool) -> Result<Session> {
    let (tx, rx): (Sender<Packet>, Receiver<Packet>) = crossbeam_channel::bounded(2);
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

    let child = spawn_ffmpeg(&cfg)?;

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
        std::thread::spawn(move || {
            let flags = PipeFlags {
                tx,
                preview: ptx,
                out_w: cfg.width,
                out_h: cfg.height,
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
    /// True once both background threads have exited (joinable without blocking).
    pub fn captures_done(&self) -> bool {
        let cap = self.capture.as_ref().map(|h| h.is_finished()).unwrap_or(true);
        let wr = self.writer.as_ref().map(|h| h.is_finished()).unwrap_or(true);
        cap && wr
    }
    /// Finalize the .webm and return any background error. Call after
    /// `request_stop()` or once `captures_done()` (duration reached / window
    /// closed). Never call from the capture thread itself.
    pub fn wait(mut self) -> Result<Snapshot> {
        self.request_stop();
        if let Some(h) = self.capture.take() {
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
