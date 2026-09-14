# Crabby — hardware-adaptive screen recorder for Windows

OBS-like capture for Windows, built in Rust. Records **monitor or specific
window** at up to **60 fps** with **hardware encoding when your machine has
it** (Quick Sync, NVENC, AMF) and honest software fallbacks when it doesn't.
Ships with a **native GUI** and a scriptable **CLI** that expose the same
tiers, codecs and containers.

On startup Crabby probes your actual hardware and picks sane defaults for
it — the chosen encoder and the reason are printed at record start (paste
that block into bug reports).

## System requirements

- Windows 10 (1803+) or Windows 11, 64-bit x86_64.
- Any CPU; 4 GB RAM minimum (8+ GB recommended for software encoding).
- No GPU required — software x264/VP8/VP9 works everywhere. A hardware
  encoder just makes it cheaper (see table).
- ~150 MB free for the app + ffmpeg (auto-downloaded on first run).

## Encoder compatibility

| Encoder | Needs | Expected tier |
|---|---|---|
| Quick Sync H.264 | Intel iGPU (2012+) + driver | balanced/high at 1080p60, low CPU |
| NVENC H.264 | NVIDIA Kepler+ GPU + driver | balanced/high, near-zero CPU |
| AMF H.264 | AMD GCN+ GPU + driver | balanced/high, near-zero CPU |
| x264 software | any x86_64 | balanced on 8+ threads, fastest below |
| Quick Sync / NVENC / AMF HEVC | Intel 6th-gen+ / Maxwell+ / Polaris+ | high, low CPU |
| Quick Sync / NVENC / AMF AV1 | recent Intel / NVIDIA / AMD + driver | high, low CPU |
| VP8 / VP9 software | any x86_64 | fastest/balanced; VP9 is heavy |

HEVC and AV1 have **no software fallback** (software AV1/HEVC at 60 fps is
a slideshow) — on machines without the hardware they fail with a message
saying what to use instead. See `TESTING.md` for what was verified on real
hardware vs what follows from API docs.

## Windows 10 vs 11 (unchanged, still true)

- **Border capture:** Windows 10 rejects explicit border toggling, so the
  app always uses the default. Windows 11 supports border on/off.
- **Per-app audio:** needs Windows 11+. On Windows 10, system capture hears
  everything playing — mute other apps for clean recordings.
- **Exclusive fullscreen** can't be captured (same limit as OBS display
  capture) — run games borderless-windowed.

## Quality tiers

Vendor-neutral, describe encode effort vs quality. Bitrates scale with
resolution (shown: 1080p60 reference).

| Tier | 1080p bitrate | Best on |
|---|---|---|
| `fastest` | ~6M | weak hardware, commentary drafts |
| `balanced` (default) | ~10M | everything else |
| `high-quality` | ~20M | machines with encode headroom |
| `lossless` | exact pixels, huge files | x264 or VP9 only (HW encoders + VP8 rejected with guidance) |

`--bitrate` and `--cpu-used` override the tier for power users; they don't
replace it. Run `crabby --benchmark` to see which encoders hold realtime on
your machine, then pick a tier.

## Containers, codecs, crash-safety

| Container | Video | Audio | If killed mid-record |
|---|---|---|---|
| MP4 (default) | H.264, H.265, AV1 | AAC | playable (fragmented MP4) |
| MKV | anything | Opus or AAC | playable up to last 2 s cluster |
| WebM | VP8, VP9, AV1 | Opus | playable up to last 2 s cluster |

Invalid combinations (`--codec vp8` into MP4, Opus into MP4, …) are rejected
at startup with a message telling you the fix — files are never silently
renamed.

## Usage

```powershell
cargo build --release

# GUI (recommended) — also what a plain double-click opens
.\target\release\crabby.exe --gui

# CLI: record primary monitor until Enter/Ctrl+C (Videos folder)
.\target\release\crabby.exe

# window capture, 10-second test
.\target\release\crabby.exe --window "Notepad" --duration 10 --output test

# tiers, containers, overrides
.\target\release\crabby.exe --quality high-quality --container mkv
.\target\release\crabby.exe --codec vp9 --bitrate 12M --output draft
.\target\release\crabby.exe --codec h265 --container mp4

# what can I capture / how fast is my hardware
.\target\release\crabby.exe --list-monitors
.\target\release\crabby.exe --list-windows
.\target\release\crabby.exe --benchmark

# updates without reinstalling
.\target\release\crabby.exe --check-updates
.\target\release\crabby.exe --update
```

Stop with **Enter** or **Ctrl+C**. Finished files never overwrite: existing
names get `_001`, `_002`, … `--border` is accepted but needs Windows 11.

## Config file

Optional TOML so you don't repeat flags. Default location
`%APPDATA%\Crabby\config.toml` (`--config <path>` overrides).
Precedence: **CLI flag > config file > built-in default**.

```toml
dir = "D:/Videos/Crabby"
quality = "balanced"     # fastest | balanced | high-quality | lossless
codec = "h264"           # h264 | h264-qsv | h264-nvenc | h264-amf | x264 | h265 | av1 | vp8 | vp9
container = "mp4"        # mp4 | mkv | webm
audio = "system"         # system | mic | both | off
audio_codec = "aac"      # opus | aac (must fit the container)
threads = 8              # omit = auto (CPU count)
```

## Updates

Same as before: daily background check + header button in the GUI,
`--check-updates` / `--update` in the CLI, releases published by pushing a
`v*` tag. One known limitation: the updater stages the new exe **next to
the running one**, so if you install Crabby under `C:\Program Files` (not
writable without elevation) the update will fail with a clear message —
install it somewhere user-writable instead. No redesign planned; it works
for the portable-zip distribution this project ships.

## How it stays light

- Windows Graphics Capture (GPU-composited, event-driven — idle ≈ 0% CPU)
- Hardware encode when present, fastest-sane software settings otherwise
- Bounded channel + `try_send` (never blocks capture; drops = realtime)
- Buffer reuse, CFR pacer thread, reused scratch sized from your display
- Zero `unsafe` in our code, no unwraps on hot/user/hardware paths

## Branding

`assets/crabby.png` is the master logo. `assets/icon-<N>.png` are
high-quality sizes regenerated from it; `build.rs` packs them into a
multi-image `icon.ico` and embeds version info + a DPI-aware manifest,
while the GUI sets the same art as its window icon at runtime.
