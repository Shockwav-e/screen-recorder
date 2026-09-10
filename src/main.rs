//! lite-rec — lightweight OBS-like recorder for Windows.
//! CLI (default) + modern native GUI (`--gui`).
//! Capture: Windows Graphics Capture API. Encode: FFmpeg → WebM 1080p60.

mod gui;
mod recorder;

use std::io::Read;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use clap::Parser;

use recorder::{
    Codec, Quality, RecordConfig, Source, describe_source, list_monitors, list_windows,
    resolve_output, start_session,
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

    /// Quality preset: youtube (VP9 16M, best for uploads) or balanced (VP8 8M, low CPU)
    #[arg(long, value_enum, default_value_t = Quality::Youtube)]
    quality: Quality,

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

fn main() -> Result<()> {
    let args = Args::parse();

    if args.gui {
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
    let width = args.width.clamp(2, 7680) & !1;
    let height = args.height.clamp(2, 4320) & !1;
    let fps = args.fps.clamp(1, 120);
    let codec = args.codec.unwrap_or_else(|| args.quality.codec());
    let bitrate = args.bitrate.clone().unwrap_or_else(|| args.quality.bitrate().into());
    let cpu_used = args.cpu_used.unwrap_or_else(|| args.quality.cpu_used()).clamp(0, 8);

    let output = resolve_output(&args.output, &args.dir)?;
    let cfg = RecordConfig {
        output,
        fps,
        width,
        height,
        source: match args.window.clone() {
            Some(needle) => Source::Window { needle },
            None => Source::Monitor { index: args.monitor },
        },
        codec,
        bitrate: bitrate.clone(),
        cpu_used,
        threads: args.threads,
        duration: args.duration,
        no_cursor: args.no_cursor,
    };
    if args.border {
        eprintln!("NOTE: --border needs Windows 11 and is ignored on Windows 10.");
    }

    let src_desc = describe_source(&cfg)
        .with_context(|| "source unavailable — try --list-windows / --list-monitors")?;

    println!("shockwave  |  {src_desc}");
    println!(
        "target    |  {width}x{height} @ {fps}fps  WebM({})  {} preset  bitrate {}  cpu-used {}",
        codec.label(),
        match args.quality {
            Quality::Youtube => "youtube",
            Quality::Balanced => "balanced",
        },
        bitrate,
        cpu_used,
    );
    if codec == Codec::Vp9 {
        println!("note      |  VP9 on 4-thread Haswell ~= 35-55% CPU; --quality balanced for ~15-25%.");
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
    let s = session.wait()?;
    println!(
        "done      |  saved (captured={} dropped={} written={}; drops = realtime pacing, normal)",
        s.captured, s.dropped, s.written,
    );
    Ok(())
}
