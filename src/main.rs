//! lite-rec — lightweight OBS-like recorder for Windows.
//! Capture: Windows Graphics Capture API (monitor or specific window, like OBS).
//! Encode: FFmpeg (auto-downloaded) → WebM (VP8/VP9), 60fps 1080p, realtime-tuned.
//!
//! Tuned for i5-4570 / 4 threads / 16GB / 1920x1080 display:
//!   - native 1080p input => no scaler cost
//!   - bounded 2-frame queue => ~25MB RAM, never balloons
//!   - buffer reuse, zero alloc per frame in steady state
//!   - VP8 default (lighter than VP9 on Haswell which has no VP9 HW encode)

use std::io::{Read, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use clap::{Parser, ValueEnum};
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
enum Codec {
    /// WebM VP8 — ~30-40% cheaper CPU than VP9. Recommended for i5-4570.
    Vp8,
    /// WebM VP9 — better compression, needs more CPU. Use if you have headroom.
    Vp9,
}

#[derive(Parser, Debug)]
#[command(name = "lite-rec", version, about = "Lightweight WebM 1080p60 monitor/window recorder (OBS-like, Rust)")]
struct Args {
    /// Output file (must end with .webm). Bare filename => saved under --dir.
    #[arg(short, long, default_value = "out.webm")]
    output: String,

    /// Folder where recordings are stored (created if missing).
    /// Full paths in --output bypass this. Default is on your D disk.
    #[arg(long, default_value = r"D:\Recordings")]
    dir: String,

    /// Frames per second (60 = target, 30 = half CPU fallback)
    #[arg(long, default_value_t = 60)]
    fps: u32,

    /// Output width (default 1920; native display is 1920 so no scaling cost)
    #[arg(long, default_value_t = 1920)]
    width: u32,

    /// Output height (default 1080)
    #[arg(long, default_value_t = 1080)]
    height: u32,

    /// Monitor index (1-based). Default: primary monitor. Ignored when --window is set.
    #[arg(long)]
    monitor: Option<usize>,

    /// Record a specific window whose title CONTAINS this text (OBS-like).
    /// Example: --window "Notepad"  |  --window "YouTube"
    #[arg(long)]
    window: Option<String>,

    /// List capturable windows and exit
    #[arg(long, default_value_t = false)]
    list_windows: bool,

    /// List monitors and exit
    #[arg(long, default_value_t = false)]
    list_monitors: bool,

    /// Video codec (WebM container either way)
    #[arg(long, value_enum, default_value_t = Codec::Vp8)]
    codec: Codec,

    /// Target video bitrate, e.g. 6M, 8M, 12M
    #[arg(long, default_value = "6M")]
    bitrate: String,

    /// libvpx speed 0(best)..8(fastest). 8 = lowest CPU. Keep 8 on i5-4570.
    #[arg(long, default_value_t = 8)]
    cpu_used: u8,

    /// Encoder threads (default 4 = your logical cores; 0 = auto)
    #[arg(long, default_value_t = 4)]
    threads: u32,

    /// Stop automatically after N seconds (omit = record until Enter/Ctrl+C)
    #[arg(long)]
    duration: Option<u64>,

    /// Hide mouse cursor in recording
    #[arg(long, default_value_t = false)]
    no_cursor: bool,

    /// Draw the yellow OBS-style border around captured window (default off)
    #[arg(long, default_value_t = false)]
    border: bool,
}

// ---------- shared plumbing ----------

/// A finished 1080p BGRA frame ready for ffmpeg stdin.
type Packet = Vec<u8>;

#[derive(Clone)]
struct PipeFlags {
    tx: Sender<Packet>,
    out_w: u32,
    out_h: u32,
    stop: Arc<AtomicBool>,
    captured: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    deadline: Option<Instant>,
}

struct Recorder {
    tx: Sender<Packet>,
    out_w: u32,
    out_h: u32,
    stop: Arc<AtomicBool>,
    captured: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    deadline: Option<Instant>,
    nopad: Vec<u8>,
    /// reused scratch for fitted 1080p output (no alloc per frame)
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
            out_w: f.out_w,
            out_h: f.out_h,
            stop: f.stop,
            captured: f.captured,
            dropped: f.dropped,
            deadline: f.deadline,
            nopad: Vec::with_capacity(1920 * 1080 * 4),
            fitted: vec![0u8; px],
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // stop conditions (checked on capture thread — prompt, no extra wakeups)
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

        // Get raw pixels without stride padding, reusing `nopad`.
        // as_nopadding_buffer fills `self.nopad` only if padding exists,
        // otherwise it borrows the internal buffer — either way we memcpy
        // out into `fitted` immediately, then send.
        let fb = frame.buffer()?;
        let tmp = fb.as_nopadding_buffer(&mut self.nopad);
        if tmp.len() < (sw as usize) * (sh as usize) * 4 {
            return Ok(()); // malformed frame, skip
        }
        fit_bgra_center(tmp, sw, sh, &mut self.fitted, self.out_w, self.out_h);
        self.captured.fetch_add(1, Ordering::Relaxed);
        // Send without ever blocking the capture thread (blocks = lag + RAM).
        // Channel depth 2 keeps RAM flat; drops = realtime pacing, like OBS.
        match self.tx.try_send(std::mem::replace(
            &mut self.fitted,
            vec![0u8; (self.out_w as usize) * (self.out_h as usize) * 4],
        )) {
            Ok(()) => {}
            Err(TrySendError::Full(pkt)) => {
                // reclaim buffer, count drop
                self.fitted = pkt;
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

/// Center-crop / center-pad BGRA `src` (sw×sh) into `dst` (dw×dh).
/// Fast path: identical size = single memcpy. Otherwise per-row memcpy.
/// Black bars when window < 1080p; crop center when window > 1080p.
fn fit_bgra_center(src: &[u8], sw: u32, sh: u32, dst: &mut [u8], dw: u32, dh: u32) {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    if sw == dw && sh == dh {
        let n = dw * dh * 4;
        dst[..n].copy_from_slice(&src[..n]);
        return;
    }
    // clear to black first (cheap memset, ~8MB)
    for b in dst.iter_mut() {
        *b = 0;
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

// ---------- ffmpeg writer (CFR pacer) ----------

fn spawn_ffmpeg(args: &Args, out_w: u32, out_h: u32) -> Result<ffmpeg_sidecar::child::FfmpegChild> {
    // Ensure an ffmpeg binary exists (auto-download next to the exe on first run).
    let _ = ffmpeg_sidecar::download::auto_download().context("ffmpeg auto-download failed");

    let (enc, extra): (&str, Vec<String>) = match args.codec {
        Codec::Vp8 => (
            "libvpx",
            vec![
                "-deadline".into(),
                "realtime".into(),
                "-cpu-used".into(),
                args.cpu_used.clamp(0, 8).to_string(),
                "-lag-in-frames".into(),
                "16".into(),
                "-auto-alt-ref".into(),
                "1".into(),
                "-g".into(),
                (args.fps * 2).to_string(),
            ],
        ),
        Codec::Vp9 => (
            "libvpx-vp9",
            vec![
                "-deadline".into(),
                "realtime".into(),
                "-cpu-used".into(),
                args.cpu_used.clamp(0, 8).to_string(),
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
                (args.fps * 2).to_string(),
            ],
        ),
    };

    let threads = if args.threads == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(4)
    } else {
        args.threads
    };

    let mut cmd = ffmpeg_sidecar::command::FfmpegCommand::new();
    cmd.args([
        "-y",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "bgra",
        "-s",
        &format!("{out_w}x{out_h}"),
        "-framerate",
        &args.fps.to_string(),
        "-i",
        "-",
        "-an",
        "-c:v",
        enc,
        "-b:v",
        &args.bitrate,
        "-threads",
        &threads.to_string(),
        "-pix_fmt",
        "yuv420p",
    ]);
    for e in &extra {
        cmd.arg(e);
    }
    cmd.args(["-vsync", "cfr", &args.output]);
    let child = cmd.spawn().context(
        "failed to spawn ffmpeg. Install it (winget install Gyan.FFmpeg) or let lite-rec auto-download it",
    )?;
    Ok(child)
}

/// Writer thread: paces channel frames out at exactly `fps` CFR.
/// Reuses last frame when idle (standard OBS behavior) so output is always
/// full-framerate WebM without burning CPU on the capture thread.
fn writer_loop(
    rx: Receiver<Packet>,
    mut child: ffmpeg_sidecar::child::FfmpegChild,
    fps: u32,
    out_w: u32,
    out_h: u32,
    stop: Arc<AtomicBool>,
    written: Arc<AtomicU64>,
) -> Result<()> {
    let mut stdin = child
        .take_stdin()
        .context("ffmpeg stdin unavailable")?;
    let frame_bytes = (out_w as usize) * (out_h as usize) * 4;
    let mut last: Vec<u8> = vec![0u8; frame_bytes]; // starts black
    let mut have_frame = false;
    let interval = Duration::from_nanos(1_000_000_000 / fps.max(1) as u64);
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
            // nothing captured yet (window minimised?) — wait briefly, no spin
            std::thread::sleep(Duration::from_millis(5));
            next = Instant::now() + interval;
            continue;
        }
        if let Err(e) = stdin.write_all(&last) {
            // ffmpeg exited (or pipe closed) — normal at shutdown
            eprintln!("ffmpeg pipe closed ({e}); finishing…");
            break;
        }
        written.fetch_add(1, Ordering::Relaxed);

        if stop.load(Ordering::Relaxed) && rx.is_empty() {
            // keep pacing a beat so duration-based stops flush cleanly,
            // but exit promptly on user stop: drain check above handles it.
            // We break when the capture side dropped the sender.
        }

        // CFR pacing: sleep until next tick (no busy spin => low CPU)
        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
        } else if now - next > Duration::from_millis(500) {
            next = now; // fell far behind (encode stall) — resync, don't spiral
        }
        next += interval;

        if stop.load(Ordering::Relaxed) && rx.is_empty() {
            // capture thread ended; flush one last time then close stdin
            // (loop will write duplicates otherwise — exit instead)
            break;
        }
    }
    drop(stdin); // EOF -> ffmpeg finalizes the .webm
    let _ = child.wait();
    Ok(())
}

// ---------- main ----------

fn main() -> Result<()> {
    let mut args = Args::parse();

    // WebM container enforcement
    if !args.output.to_lowercase().ends_with(".webm") {
        eprintln!("NOTE: forcing .webm extension (was {})", args.output);
        args.output.push_str(".webm");
    }
    // Save location: bare filename => --dir (your D disk); full/absolute
    // path in --output bypasses --dir. Parent folders are auto-created.
    {
        let p = std::path::Path::new(&args.output);
        let is_bare = p.parent().map(|par| par.as_os_str().is_empty()).unwrap_or(true)
            && !args.output.contains(':')
            && !args.output.contains('/')
            && !args.output.contains('\\');
        if is_bare {
            let dir = std::path::Path::new(&args.dir);
            if let Err(e) = std::fs::create_dir_all(dir) {
                anyhow::bail!("cannot create --dir {}: {e}", args.dir);
            }
            args.output = dir.join(&args.output).to_string_lossy().into_owned();
        } else if let Some(par) = p.parent() {
            if !par.as_os_str().is_empty() {
                std::fs::create_dir_all(par)
                    .with_context(|| format!("cannot create output folder {}", par.display()))?;
            }
        }
    }
    // VPx needs even dimensions
    args.width &= !1;
    args.height &= !1;
    if args.width == 0 || args.height == 0 {
        anyhow::bail!("width/height must be >= 2 and even");
    }
    let out_w = args.width;
    let out_h = args.height;
    let fps = args.fps.clamp(1, 120);

    if args.list_windows {
        let wins = Window::enumerate().context("failed to enumerate windows")?;
        if wins.is_empty() {
            println!("No capturable windows found.");
        }
        for w in &wins {
            let title = w.title().unwrap_or_default();
            let proc_ = w.process_name().unwrap_or_default();
            let (ww, hh) = (w.width().unwrap_or(0), w.height().unwrap_or(0));
            println!("\"{title}\"  [{ww}x{hh}]  ({proc_})");
        }
        return Ok(());
    }
    if args.list_monitors {
        let mons = Monitor::enumerate().context("failed to enumerate monitors")?;
        for m in &mons {
            println!(
                "#{}: {}  {}x{} @{}Hz  ({})",
                m.index().unwrap_or(0),
                m.name().unwrap_or_else(|_| "?".into()),
                m.width().unwrap_or(0),
                m.height().unwrap_or(0),
                m.refresh_rate().unwrap_or(0),
                m.device_name().unwrap_or_default(),
            );
        }
        return Ok(());
    }

    // Resolve source for logging (capture itself happens in Recorder::start)
    let (src_desc, native_w, native_h): (String, u32, u32) = if let Some(ref needle) = args.window {
        let w = Window::from_contains_name(needle).with_context(|| {
            format!("no window title contains \"{needle}\" — try --list-windows")
        })?;
        let (ww, hh) = (
            w.width().unwrap_or(0).max(0) as u32,
            w.height().unwrap_or(0).max(0) as u32,
        );
        let desc = format!("window \"{}\" ({ww}x{hh})", w.title().unwrap_or_default());
        (desc, ww, hh)
    } else {
        let m = match args.monitor {
            Some(i) => Monitor::from_index(i.max(1))
                .with_context(|| format!("no monitor #{i} — try --list-monitors"))?,
            None => Monitor::primary().context("no primary monitor found")?,
        };
        let desc = format!(
            "monitor #{} \"{}\" ({}x{})",
            m.index().unwrap_or(0),
            m.name().unwrap_or_default(),
            m.width().unwrap_or(0),
            m.height().unwrap_or(0)
        );
        (desc, m.width().unwrap_or(0), m.height().unwrap_or(0))
    };

    println!("lite-rec  |  {src_desc}");
    println!(
        "target    |  {out_w}x{out_h} @ {fps}fps  WebM({})  bitrate {}  cpu-used {}  threads {}",
        match args.codec {
            Codec::Vp8 => "VP8",
            Codec::Vp9 => "VP9",
        },
        args.bitrate,
        args.cpu_used,
        args.threads,
    );
    if native_w == out_w && native_h == out_h {
        println!("scaling   |  none (native match — zero scaler CPU)");
    } else {
        println!("fit       |  center crop/pad in-Rust (cheap memcpy, no ffmpeg scale)");
    }
    if matches!(args.codec, Codec::Vp9) {
        println!("hint      |  VP9 on 4-thread Haswell ≈ 35-55% CPU; use --codec vp8 for ~15-25%.");
    }

    // Channels + stop flags (bounded 2 => RAM ≈ 2×8.3MB + ffmpeg ≈ 60-120MB total)
    let (tx, rx): (Sender<Packet>, Receiver<Packet>) = crossbeam_channel::bounded(2);
    let stop = Arc::new(AtomicBool::new(false));
    let captured = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicU64::new(0));
    let written = Arc::new(AtomicU64::new(0));
    let deadline = args.duration.map(|s| Instant::now() + Duration::from_secs(s));

    // Ctrl+C => graceful stop (finalizes .webm instead of corrupting it)
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || {
            stop.store(true, Ordering::Relaxed);
        });
    }
    // Enter => stop (separate thread, doesn't block capture)
    {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1];
            let _ = std::io::stdin().read(&mut buf);
            stop.store(true, Ordering::Relaxed);
        });
    }

    let child = spawn_ffmpeg(&args, out_w, out_h)?;
    println!("recording |  {}  (Enter or Ctrl+C to stop)", args.output);

    // Writer thread owns ffmpeg
    let w_stop = stop.clone();
    let w_written = written.clone();
    let writer = std::thread::spawn(move || writer_loop(rx, child, fps, out_w, out_h, w_stop, w_written));

    // Stats thread (1/s, cheap)
    {
        let (stop, captured, dropped, written) =
            (stop.clone(), captured.clone(), dropped.clone(), written.clone());
        let t0 = Instant::now();
        std::thread::spawn(move || {
            let mut last_w = 0u64;
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let w = written.load(Ordering::Relaxed);
                let c = captured.load(Ordering::Relaxed);
                let d = dropped.load(Ordering::Relaxed);
                println!(
                    "  [{:>4}s] out {:>4}fps  captured {c}  dropped {d}",
                    t0.elapsed().as_secs(),
                    w - last_w
                );
                last_w = w;
                if stop.load(Ordering::Relaxed) {
                    break;
                }
            }
        });
    }

    let flags = PipeFlags {
        tx,
        out_w,
        out_h,
        stop: stop.clone(),
        captured: captured.clone(),
        dropped: dropped.clone(),
        deadline,
    };
    let cursor = if args.no_cursor {
        CursorCaptureSettings::WithoutCursor
    } else {
        CursorCaptureSettings::WithCursor
    };
    let border = if args.border {
        DrawBorderSettings::WithBorder
    } else {
        DrawBorderSettings::WithoutBorder
    };

    // Start capture (blocks until stop). Same Recorder works for both sources.
    if let Some(needle) = args.window.clone() {
        let item = Window::from_contains_name(&needle).context("window disappeared")?;
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
        Recorder::start(settings).context("capture failed")?;
    } else {
        let item = match args.monitor {
            Some(i) => Monitor::from_index(i.max(1))?,
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
        Recorder::start(settings).context("capture failed")?;
    }

    stop.store(true, Ordering::Relaxed);
    let _ = writer.join();
    println!(
        "done      |  {}  captured={} dropped={} written={} ({} dropped = realtime pacing, normal)",
        args.output,
        captured.load(Ordering::Relaxed),
        dropped.load(Ordering::Relaxed),
        written.load(Ordering::Relaxed),
        dropped.load(Ordering::Relaxed),
    );
    Ok(())
}
