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

## Dropping frames?

`Saved clip.mp4 (26 frames, 951 dropped)` means capture ran full speed but
the encoder only finished a few frames per second — the file is valid but
nearly empty. The app now warns **before** a doomed recording (GUI blocks
the first press; press Record again to override, or hit **Use safe 720p30
Fastest**). Rules of thumb for software encode (no QSV/NVENC/AMF):

| Machine | Holds | Avoid |
|---|---|---|
| 8+ threads | 1080p60 Balanced | 4K60, Lossless VP9 |
| 4 threads | 720p30 Fastest | 1080p60, High/Lossless |
| VP9, any tier | 720p30 | 1080p60 (needs ~2× the CPU of x264) |

Fastest relief in order: **720p → 30 fps → Fastest tier → Auto H.264**
(hardware when present). `crabby --benchmark` measures your encoders
directly. Native size on a 1440p/4K display is the usual culprit — the pixel
count, not the window content, is what the encoder chokes on.

## Screenshots (Snipping-Tool style)

Region (drag a rectangle, Esc cancels), Window (§1 pick) and Fullscreen
(§1 monitor) — every capture auto-saves **and** copies to the clipboard,
then pops a Windows-style notification at the screen's bottom-right:
thumbnail (grows on hover), Open/Copy buttons, gone after 3 s unless
hovered. The built-in editor does pen markup (6 colors, 2–24px) and crop.

Keys (Crabby must be running for the global ones):

| Keys | Action |
|---|---|
| `Win+PrtSc` | fullscreen screenshot, anywhere |
| `PrtSc` | region snip, anywhere |
| `Alt+PrtSc` | window snip, anywhere |
| `Alt+N` / `Alt+W` / `Alt+F` | region / window / fullscreen (app focused) |
| `Ctrl+S` / `Ctrl+C` / `Esc` | save / copy / close editor |

`Win+PrtSc` needs no setup; if another tool owns a key the app says which
ones are live in §5. Formats: PNG (default), WebP, JPG (fastest), BMP —
pick in §5 or set `shot_format` / `shot_dir` in the config file
(`shot_hotkeys = false` disables the global keys).

```powershell
.\target\release\crabby.exe --screenshot                                    # fullscreen
.\target\release\crabby.exe --screenshot --shot-mode window --window Notepad # window
.\target\release\crabby.exe --screenshot --shot-mode region --shot-region 800x600+100+200 --shot-format webp
```

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
