//! Shockwave Screen Recorder — lightweight OBS-like recorder for Windows.
//! GUI (`--gui`, or plain double-click) + scriptable CLI.
//! Capture: Windows Graphics Capture API. Encode: FFmpeg → WebM 1080p60.

// Release builds are windowed apps: no console pops up behind the GUI.
// (Debug builds keep the console for development. When a release build is run
// from a terminal, stdout/stderr still go to that terminal.)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod gui;
mod recorder;

use std::io::{IsTerminal as _, Read};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use clap::Parser;

use recorder::{
    AudioMode, Codec, Quality, RecordConfig, Source, auto_quality, describe_source,
    encoder_display, list_monitors, list_windows, resolve_output, resolve_size, start_session,
    video_encoder,
};

#[derive(Parser, Debug)]
#[command(name = "shockwave-rec", version, about = "Shockwave Screen Recorder — lightweight WebM 1080p60 monitor/window recorder (OBS-like, Rust)")]
struct Args {
    /// Launch the modern graphical interface instead of the CLI
    #[arg(long, default_value_t = false)]
    gui: bool,

    /// Output file (must end with .webm). Bare filename => saved under --dir.
    #[arg(short, long, default_value = "gameplay.webm")]
    output: String,

    /// Folder where recordings are stored (created if missing).
    /// Full paths in --output bypass this. Default is on your D disk.
    #[arg(long, default_value = r"D:\Recordings")]
    dir: String,

    /// Quality preset: youtube (VP9 16M) or smooth/balanced (VP8 8M).
    /// Omit it: auto-picks Smooth on ≤4-core PCs (like yours), YouTube HQ above.
    #[arg(long, value_enum)]
    quality: Option<Quality>,

    /// Override the preset codec (vp8 | vp9)
    #[arg(long, value_enum)]
    codec: Option<Codec>,

    /// Override the preset bitrate, e.g. 8M, 12M, 16M, 20M
    #[arg(long)]
    bitrate: Option<String>,

    /// Override the preset libvpx speed 0(best)..8(fastest). Higher = less CPU.
    #[arg(long)]
    cpu_used: Option<u8>,

    /// Frames per second (60 = target, 30 = half CPU fallback)
    #[arg(long, default_value_t = 60)]
    fps: u32,

    /// Output size: native (match the app/monitor — no black bars),
    /// 1080p, 720p, or explicit WIDTHxHEIGHT (e.g. 1600x900).
    #[arg(long, default_value = "native")]
    size: String,

    /// Audio source: system (game sound), mic, both, or off.
    /// On Win10 there is no per-app isolation — mute other apps for clean audio.
    #[arg(long, value_enum, default_value_t = AudioMode::System)]
    audio: AudioMode,

    /// Monitor index (1-based). Default: primary monitor. Ignored when --window is set.
    #[arg(long)]
    monitor: Option<usize>,

    /// Record a specific window whose title CONTAINS this text (OBS-like).
    #[arg(long)]
    window: Option<String>,

    /// List capturable windows and exit
    #[arg(long, default_value_t = false)]
    list_windows: bool,

    /// List monitors and exit
    #[arg(long, default_value_t = false)]
    list_monitors: bool,

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

impl Args {
    /// True when the user passed no recording options at all — i.e. a plain
    /// double-click on the exe rather than a scripted invocation.
    fn is_default_invocation(&self) -> bool {
        !self.gui
            && self.output == "gameplay.webm"
            && self.dir == r"D:\Recordings"
            && self.quality.is_none()
            && self.codec.is_none()
            && self.bitrate.is_none()
            && self.cpu_used.is_none()
            && self.fps == 60
            && self.size == "native"
            && self.audio == AudioMode::System
            && self.monitor.is_none()
            && self.window.is_none()
            && !self.list_windows
            && !self.list_monitors
            && self.threads == 4
            && self.duration.is_none()
            && !self.no_cursor
            && !self.border
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Plain double-click (no console, no flags) would otherwise start a blind
    // recording with nowhere to show status — open the GUI instead. Explicit
    // flags always honor the CLI (scripts/automation unaffected).
    if args.gui || (args.is_default_invocation() && !std::io::stdin().is_terminal()) {
        return gui::run();
    }

    if args.list_windows {
        let wins = list_windows()?;
        if wins.is_empty() {
            println!("No capturable windows found.");
        }
        for w in &wins {
            println!("\"{}\"  [{}x{}]  ({})", w.title, w.w, w.h, w.process);
        }
        return Ok(());
    }
    if args.list_monitors {
        let mons = list_monitors()?;
        for m in &mons {
            println!("#{}: {}  {}x{} @{}Hz", m.index, m.name, m.w, m.h, m.hz);
        }
        return Ok(());
    }

    // Harden numeric inputs (garbage in => clamped, never panic/overflow).
    let fps = args.fps.clamp(1, 120);
    let quality = args.quality.unwrap_or_else(auto_quality);
    let codec = args.codec.unwrap_or_else(|| quality.codec());
    let bitrate = args.bitrate.clone().unwrap_or_else(|| quality.bitrate().into());
    let cpu_used = args.cpu_used.unwrap_or_else(|| quality.cpu_used()).clamp(0, 8);

    let source = match args.window.clone() {
        Some(needle) => Source::Window { needle },
        None => Source::Monitor { index: args.monitor },
    };
    // Native size = the app's own size: no black bars, fastest path.
    let (width, height) = resolve_size(&args.size, &source)
        .with_context(|| "bad --size / source — try --list-windows")?;
    let output = resolve_output(&args.output, &args.dir, codec.container_ext())?;
    let cfg = RecordConfig {
        output,
        fps,
        width,
        height,
        source,
        codec,
        bitrate: bitrate.clone(),
        cpu_used,
        threads: args.threads,
        duration: args.duration,
        no_cursor: args.no_cursor,
        audio: args.audio,
    };
    if args.border {
        eprintln!("NOTE: --border needs Windows 11 and is ignored on Windows 10.");
    }

    let src_desc = describe_source(&cfg)
        .with_context(|| "source unavailable — try --list-windows / --list-monitors")?;

    let enc_name: &str = match video_encoder(codec) {
        Ok(enc) => encoder_display(enc),
        Err(e) => {
            eprintln!("Error: {e:?}");
            eprintln!("hint: use --codec h264 (auto) or vp8 — run --help for all encoders.");
            std::process::exit(2);
        }
    };
    println!("shockwave  |  {src_desc}");
    println!(
        "target    |  {width}x{height}{} @ {fps}fps  {}({})  {} preset  bitrate {}  cpu-used {}  audio {}",
        if args.size.trim().eq_ignore_ascii_case("native") { " (native)" } else { "" },
        codec.container_ext(),
        enc_name,
        match quality {
            Quality::Youtube => "youtube",
            Quality::Balanced => "smooth",
        },
        bitrate,
        cpu_used,
        args.audio.label(),
    );
    if codec == Codec::Vp9 {
        println!("note      |  VP9 software encode is heavy; prefer youtube (Quick Sync) or smooth preset.");
    }

    let session = start_session(cfg, false)?;
    println!("recording |  {}  (Enter or Ctrl+C to stop)", session.output());

    // Graceful stop: Ctrl+C or Enter finalizes the .webm instead of corrupting it.
    {
        let stop = session.stop_flag();
        ctrlc::set_handler(move || {
            stop.store(true, Ordering::Relaxed);
        });
    }
    {
        let stop = session.stop_flag();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1];
            let _ = std::io::stdin().read(&mut buf);
            stop.store(true, Ordering::Relaxed);
        });
    }

    // Console stats (1/s, cheap) until both background threads exit.
    let t0 = Instant::now();
    let mut last_w = 0u64;
    while !session.captures_done() {
        std::thread::sleep(Duration::from_secs(1));
        let s = session.snapshot();
        println!(
            "  [{:>4}s] out {:>4}fps  captured {}  dropped {}",
            t0.elapsed().as_secs(),
            s.written - last_w,
            s.captured,
            s.dropped,
        );
        last_w = s.written;
    }
    let out_path = session.output().to_owned();
    match session.wait() {
        Ok(s) => println!(
            "done      |  saved (captured={} dropped={} written={}; drops = realtime pacing, normal)",
            s.captured, s.dropped, s.written,
        ),
        Err(e) => {
            // Don't leave a useless 0-byte file behind on encoder failure.
            if std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(u64::MAX) < 4096 {
                let _ = std::fs::remove_file(&out_path);
            }
            return Err(e);
        }
    }
    Ok(())
}
