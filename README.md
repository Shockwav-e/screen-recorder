# Crabby — lightweight Rust game recorder (1080p60)

OBS-like capture for Windows, built in Rust. Records **monitor or specific window**
to **WebM (VP8/VP9)** at **60fps 1080p**. Ships with a **modern native GUI**
and a scriptable **CLI**, tuned for low CPU/RAM and YouTube-ready quality.

## GUI or CLI

```powershell
cargo build --release

# modern interface (recommended)
.\target\release\crabby.exe --gui

# headless / scripted
.\target\release\crabby.exe --quality youtube --output gameplay.webm
```

The GUI has a **live preview** (480p, 10 fps tap — what you see is what's
saved), an **app picker dropdown** (monitor, or filterable window list with a
green "Will record" confirmation — the recorder hides itself), audio + size
dropdowns, save-folder browser, quality presets, and live stats (fps,
captured, dropped). Finished videos appear in a **recordings library** with
Play / Delete / Open-folder actions.
It repaints at ~10 Hz while recording and idles at ~0% CPU otherwise.

## Updates — no reinstall needed

Crabby checks GitHub Releases for a newer version once a day in the
background (plus a "Check for updates" button in the header showing the
current version). When one drops you get an **Update available** badge with
the release notes — one click downloads it and restarts straight into the
new version. CLI works too:

```powershell
crabby --check-updates
crabby --update
```

Maintainer (publishing): bump `version` in `Cargo.toml`, commit, then
`git tag v1.1.0` + `git push --tags`. The release workflow builds
`crabby.exe` and attaches it to the GitHub Release — installed copies pick
it up on their next check.

## Size: native app resolution, no black bars

`--size native` (default) records at the source's own size — a 1440×759
Notepad window saves as 1440×758, not stretched/padded to 1080p. Use
`--size 1080p`, `720p`, or `WIDTHxHEIGHT` to force a frame (center crop/pad).

## Audio: game sound + mic (Opus in the same WebM)

`--audio system` (default) captures everything you hear via WASAPI loopback,
`mic` captures the microphone, `both` mixes them, `off` is silent.
Note: per-app-only audio needs Windows 11+ — on Windows 10, mute other apps
(browser, music) while recording a game window for clean audio.

## Your machine vs requirements

| Part | You | Verdict |
|---|---|---|
| CPU i5-4570 (4C/4T Haswell) | Quick Sync H.264 in hardware | ~low single-digit % CPU recording |
| RAM 16 GB | plenty | recorder uses ~60–120 MB (3-frame queue) |
| Display 1920×1080 @100Hz | exact match | **zero scaler cost** |
| Disk 37 GB free | fine | ~120 MB/min at 16M bitrate |
| GPU HD 4600 | WGC capture is GPU-composited | near-zero capture overhead |

## Quality: hardware H.264 for YouTube, VP8/VP9 WebM when you want it

- `--quality youtube` (default on Quick Sync PCs): **H.264 MP4 @ 12M** —
  Intel Quick Sync encodes in hardware (single-digit CPU), and H.264 MP4 is
  YouTube's preferred upload format. Falls back to libx264 if no QSV driver.
- `--quality smooth`: **VP8 WebM @ 8M**, pure software, lowest CPU.
- `--codec h264` (auto: QSV → NVENC → AMF → x264), `h264-qsv`, `h264-nvenc`,
  `h264-amf`, `vp8`, `vp9` — forced vendor options fail fast with a clear
  message when their GPU is missing, and never leave empty files behind.
- `--bitrate`, `--cpu-used` override the preset.
- Fast-motion games need bits: 12M H.264 or 10–12M VP8.
- If drops climb: close background apps or `--fps 30`.

## Local games (e.g. Modern Warships)

- Run the game in **borderless windowed** mode if fullscreen capture is black
  (WGC can't see exclusive fullscreen — same limit as OBS display capture).
- Window-only (clean, like OBS Game Capture):
  `crabby --window "Modern Warships" --quality youtube`
- Whole screen: `crabby --monitor 1 --quality youtube`
- 10-second test: `--window "Modern Warships" --duration 10 --output test.webm`

## Usage

```powershell
# recordings land in D:\Recordings by default (auto-created, never overwritten —
# existing names get _001, _002…)
crabby --output gameplay.webm             # => D:\Recordings\gameplay.webm
crabby --dir "D:\Videos" --output game.webm
crabby --output "D:\Clips\warships.webm"  # full path bypasses --dir

# list targets
crabby --list-monitors
crabby --list-windows

# overrides / fallbacks
crabby --codec vp8 --bitrate 10M --output light.webm
crabby --fps 30 --output light30.webm
crabby --no-cursor --output clean.webm
```

Stop with **Enter** or **Ctrl+C** — the `.webm` is finalized cleanly.
`--border` is accepted but needs Windows 11; on Windows 10 it's ignored.

## How it stays light

- Windows Graphics Capture API (GPU-composited, event-driven — idle screen ≈ 0% CPU)
- Intel Quick Sync H.264 hardware encode (or fastest-possible software VP8/VP9)
- Fixed-size pipe, center crop/pad in-Rust (cheap memcpy, no scaler)
- Bounded 3-frame channel + `try_send` (never blocks capture; drops = realtime, like OBS)
- Buffer reuse (no per-frame alloc), CFR pacer thread, realtime libvpx flags
- FFmpeg auto-downloaded on first run (no manual install needed)
- Zero `unsafe`, no unwraps on hot paths, all numeric inputs clamped

## Branding

`assets/crabby.png` is the master logo. `assets/icon-<N>.png`
are high-quality sizes regenerated from it; `build.rs` packs them into a
multi-image `icon.ico` at build time and embeds it in the exe (taskbar,
Alt-Tab, shortcuts) with version info + a DPI-aware manifest, while the
GUI sets the same art as its window icon at runtime.
