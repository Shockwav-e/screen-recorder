//! Crabby — hardware-adaptive screen recorder for Windows.
//! GUI (`--gui`, or plain double-click) + scriptable CLI.
//! Capture: Windows Graphics Capture API. Encode: ffmpeg (H.264/H.265/AV1/VP8/VP9).

// Release builds are windowed apps: no console pops up behind the GUI.
// (Debug builds keep the console for development. When a release build is run
// from a terminal, stdout/stderr still go to that terminal.)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod caps;
mod config;
mod gui;
mod recorder;
mod updater;

use std::io::{IsTerminal as _, Read};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use clap::Parser;

use caps::capabilities;
use config::{FileConfig, default_video_dir};
use recorder::{
    AudioCodec, AudioMode, Codec, Container, RecordConfig, Source, Tier, check_tier, default_tier,
    describe_source, encoder_display, list_monitors, list_windows, resolve_output, resolve_size,
    start_session, validate_matrix,
};

#[derive(Parser, Debug)]
#[command(name = "crabby", version, about = "Crabby — hardware-adaptive screen recorder for Windows (WGC capture, ffmpeg encode)")]
struct Args {
    /// Launch the modern graphical interface instead of the CLI
    #[arg(long, default_value_t = false)]
    gui: bool,

    /// Output file. Bare filename => saved under --dir. An explicit
    /// webm/mp4/mkv extension must match the container; omit it to append.
    #[arg(short, long, default_value = "recording")]
    output: String,

    /// Folder where recordings are stored (created if missing).
    /// Full paths in --output bypass this. Default: your Videos folder.
    #[arg(long)]
    dir: Option<String>,

    /// Quality tier: encode effort vs quality. Bitrates scale with resolution.
    #[arg(long, value_enum)]
    quality: Option<Tier>,

    /// Video codec (default h264 = best available hardware, else x264)
    #[arg(long, value_enum)]
    codec: Option<Codec>,

    /// Output container (default mp4)
    #[arg(long, value_enum)]
    container: Option<Container>,

    /// Audio codec. Default follows the container (AAC for MP4, Opus otherwise).
    #[arg(long, value_enum)]
    audio_codec: Option<AudioCodec>,

    /// Override the tier bitrate, e.g. 8M, 12M, 20M
    #[arg(long)]
    bitrate: Option<String>,

    /// Override the tier libvpx speed 0(best)..8(fastest). VP8/VP9 only.
    #[arg(long)]
    cpu_used: Option<u8>,

    /// Frames per second
    #[arg(long, default_value_t = 60)]
    fps: u32,

    /// Output size: native (match the app/monitor — no black bars),
    /// 1080p, 720p, or explicit WIDTHxHEIGHT (e.g. 1600x900).
    #[arg(long, default_value = "native")]
    size: String,

    /// Audio source: system, mic, both, or off.
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

    /// Encoder threads (default: auto = CPU count)
    #[arg(long)]
    threads: Option<u32>,

    /// Stop automatically after N seconds (omit = record until Enter/Ctrl+C)
    #[arg(long)]
    duration: Option<u64>,

    /// Hide mouse cursor in recording
    #[arg(long, default_value_t = false)]
    no_cursor: bool,

    /// Draw the yellow OBS-style border around captured window (default off)
    #[arg(long, default_value_t = false)]
    border: bool,

    /// TOML config for defaults (default: %APPDATA%/Crabby/config.toml).
    /// CLI flags override config values; config overrides built-ins.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Encode benchmark: short synthetic encode per available encoder path,
    /// reporting achieved fps so you can pick a tier for your hardware.
    #[arg(long, default_value_t = false)]
    benchmark: bool,

    /// Check GitHub for a newer Crabby release and exit (no recording)
    #[arg(long, default_value_t = false)]
    check_updates: bool,

    /// Download + install the newest Crabby release, then restart (no recording)
    #[arg(long, default_value_t = false)]
    update: bool,

    /// Internal: passed by the updater on restart after a successful update
    #[arg(long, hide = true)]
    updated_from: Option<String>,
}

impl Args {
    /// True when the user passed no recording options at all — i.e. a plain
    /// double-click on the exe rather than a scripted invocation.
    fn is_default_invocation(&self) -> bool {
        !self.gui
            && self.output == "recording"
            && self.dir.is_none()
            && self.quality.is_none()
            && self.codec.is_none()
            && self.container.is_none()
            && self.audio_codec.is_none()
            && self.bitrate.is_none()
            && self.cpu_used.is_none()
            && self.fps == 60
            && self.size == "native"
            && self.audio == AudioMode::System
            && self.monitor.is_none()
            && self.window.is_none()
            && !self.list_windows
            && !self.list_monitors
            && self.threads.is_none()
            && self.duration.is_none()
            && !self.no_cursor
            && !self.border
            && self.config.is_none()
            && !self.benchmark
            && !self.check_updates
            && !self.update
            && self.updated_from.is_none()
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Plain double-click (no console, no flags) would otherwise start a blind
    // recording with nowhere to show status — open the GUI instead. Explicit
    // flags always honor the CLI (scripts/automation unaffected).
    if args.gui || args.updated_from.is_some() || (args.is_default_invocation() && !std::io::stdin().is_terminal()) {
        return gui::run(args.updated_from);
    }

    if args.check_updates {
        return check_updates_cli();
    }
    if args.update {
        return update_cli();
    }
    if args.benchmark {
        return run_benchmark();
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

    // Resolve effective settings: CLI flag > config file > built-in default.
    // All validation happens here, before any capture starts.
    let caps = capabilities();
    let file_cfg: FileConfig = config::load(args.config.as_deref())?;
    let dir = args
        .dir
        .or(file_cfg.dir)
        .unwrap_or_else(default_video_dir);
    let tier = args.quality.or(file_cfg.quality).unwrap_or_else(|| default_tier(caps));
    let codec = args.codec.or(file_cfg.codec).unwrap_or(Codec::H264);
    let container = args.container.or(file_cfg.container).unwrap_or_default();
    let audio_codec = args
        .audio_codec
        .or(file_cfg.audio_codec)
        .unwrap_or_else(|| container.default_audio());
    let threads = args.threads.or(file_cfg.threads).unwrap_or(caps.cpu_threads).max(1);
    let audio = args.audio; // audio source is CLI/GUI-only (device-dependent)

    // Harden numeric inputs (garbage in => clamped, never panic/overflow).
    let fps = args.fps.clamp(1, 120);

    validate_matrix(codec, container, audio_codec)?;
    let (enc_id, enc_why) = check_tier(codec, tier)?;

    let source = match args.window.clone() {
        Some(needle) => Source::Window { needle },
        None => Source::Monitor { index: args.monitor },
    };
    // Native size = the app's own size: no black bars, fastest path.
    let (width, height) = resolve_size(&args.size, &source)
        .with_context(|| "bad --size / source — try --list-windows")?;
    // Tier bitrate scales with the resolved resolution; explicit --bitrate wins.
    let bitrate = args
        .bitrate
        .clone()
        .unwrap_or_else(|| tier.bitrate_for(width, height));
    let cpu_used = args.cpu_used.unwrap_or_else(|| tier.vpx_cpu_used()).clamp(0, 8);
    let output = resolve_output(&args.output, &dir, container)?;
    let cfg = RecordConfig {
        output,
        fps,
        width,
        height,
        source,
        codec,
        bitrate: bitrate.clone(),
        cpu_used,
        tier,
        container,
        audio_codec,
        threads,
        duration: args.duration,
        no_cursor: args.no_cursor,
        audio,
    };
    if args.border {
        eprintln!("NOTE: --border needs Windows 11 and is ignored on Windows 10.");
    }

    let src_desc = describe_source(&cfg)
        .with_context(|| "source unavailable — try --list-windows / --list-monitors")?;

    let enc_name: &str = encoder_display(enc_id);
    println!("crabby     |  {src_desc}");
    println!("caps       |  {}", caps.describe());
    println!(
        "target    |  {width}x{height}{} @ {fps}fps  .{}({})  {} tier  bitrate {}  audio {}/{}  threads {}",
        if args.size.trim().eq_ignore_ascii_case("native") { " (native)" } else { "" },
        container.ext(),
        enc_name,
        tier.cli_name(),
        bitrate,
        args.audio.label(),
        audio_codec.cli_name(),
        threads,
    );
    println!("encoder    |  {enc_name} — {enc_why}");
    if enc_id.starts_with("lib") {
        println!("note       |  software encode; if frames drop, step down a tier or use --fps 30.");
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

/// `--check-updates`: print whether a newer release exists.
fn check_updates_cli() -> Result<()> {
    println!("crabby v{} — checking for updates…", updater::current_version());
    match updater::check_for_update()? {
        None => println!("up to date ✓"),
        Some(rel) => {
            println!("update available: {} (you have v{})", rel.tag, updater::current_version());
            println!("run `crabby --update` to install, or update from the GUI.");
        }
    }
    Ok(())
}

/// `--update`: download + install the newest release, then restart.
fn update_cli() -> Result<()> {    println!("crabby v{} — checking for updates…", updater::current_version());
    let Some(rel) = updater::check_for_update()? else {
        println!("up to date ✓");
        return Ok(());
    };
    println!("downloading {} ({} MB)…", rel.tag, rel.bytes / 1_048_576);
    let last_pct = std::cell::Cell::new(0u64);
    let staged = updater::download_update(&rel, &|done, total| {
        let pct = done.saturating_mul(100).checked_div(total).unwrap_or(0);
        if total > 0 && pct != last_pct.get() && pct % 10 == 0 {
            last_pct.set(pct);
            println!("  {pct}% ({done}/{total} bytes)");
        }
    })?;
    println!("installing + restarting…");
    updater::install_and_restart(&staged)
}

/// `--benchmark`: synthetic 5 s encode per available encoder path at the
/// display's native size, reporting achieved fps. Encoder-only (no screen
/// capture involved): it answers "can this encoder hold 60 fps here", which
/// is what tier choice depends on. Dropped-frame behavior is only observable
/// in real recordings (see TESTING.md).
fn run_benchmark() -> Result<()> {
    use recorder::encoder_args;

    let _ = ffmpeg_sidecar::download::auto_download()
        .context("ffmpeg auto-download failed (benchmark needs it)");
    let caps = capabilities();
    println!("crabby benchmark | {}", caps.describe());
    let (mut w, mut h) = caps
        .primary_display
        .map(|d| (d.w, d.h))
        .unwrap_or((1920, 1080));
    w = w.clamp(64, 7680) & !1;
    h = h.clamp(64, 7680) & !1;
    let tier = Tier::Balanced;
    let bitrate = tier.bitrate_for(w, h);
    println!("synthetic  |  {w}x{h} @ 60fps, 5 s, {}/{} tier", tier.cli_name(), bitrate);

    // Candidate encoder ids in preference order; hardware only when probed.
    let mut cands: Vec<&'static str> = Vec::new();
    if caps.h264.qsv {
        cands.push("h264_qsv");
    }
    if caps.h264.nvenc {
        cands.push("h264_nvenc");
    }
    if caps.h264.amf {
        cands.push("h264_amf");
    }
    cands.push("libx264");
    if caps.hevc.qsv {
        cands.push("hevc_qsv");
    }
    if caps.hevc.nvenc {
        cands.push("hevc_nvenc");
    }
    if caps.hevc.amf {
        cands.push("hevc_amf");
    }
    if caps.av1.qsv {
        cands.push("av1_qsv");
    }
    if caps.av1.nvenc {
        cands.push("av1_nvenc");
    }
    if caps.av1.amf {
        cands.push("av1_amf");
    }
    cands.push("libvpx");
    cands.push("libvpx-vp9");

    let ff = ffmpeg_sidecar::paths::ffmpeg_path();
    for id in cands {
        let mut cmd = std::process::Command::new(&ff);
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=s={w}x{h}:r=60:d=5"),
            "-c:v",
            id,
        ]);
        for a in encoder_args(id, tier, &bitrate, tier.vpx_cpu_used(), 60) {
            cmd.arg(a);
        }
        cmd.args(["-f", "null", "-"]);
        let t0 = Instant::now();
        let ok = cmd.status().map(|s| s.success()).unwrap_or(false);
        let el = t0.elapsed().as_secs_f64().max(0.01);
        if ok {
            let fps = 300.0 / el;
            println!(
                "  {id:12} {fps:7.1} fps  {}",
                if fps >= 60.0 { "(realtime)" } else { "(SLOWER than realtime)" }
            );
        } else {
            println!("  {id:12} FAILED to encode");
        }
    }
    println!("pick a tier your encoder holds at realtime; verify with a real recording.");
    Ok(())
}
