# lite-rec — lightweight Rust screen/window recorder (WebM 1080p60)

OBS-like capture for Windows, built in Rust. Records **monitor or specific window**
to **WebM (VP8/VP9)** at **60fps 1080p**, tuned for low CPU/RAM.

## Your machine vs requirements

| Part | You | Verdict |
|---|---|---|
| CPU i5-4570 (4C/4T Haswell) | no VP9 HW encode | use **VP8 default** (~15–25% CPU); VP9 ≈ 35–55% |
| RAM 16 GB | plenty | recorder uses ~60–120 MB (2-frame queue) |
| Display 1920×1080 | exact match | **zero scaler cost** |
| Disk 37 GB free | fine | ~45 MB/min at 6M bitrate |
| GPU HD 4600 | WGC capture is GPU-composited | near-zero capture overhead |

## Local games (e.g. Modern Warships)

- Run the game in **borderless windowed** mode if fullscreen capture is black
  (WGC can't see exclusive fullscreen — same limit as OBS display capture). Then:
  - `lite-rec --window "Modern Warships" --codec vp8 --bitrate 10M` (window-only, clean), or
  - `lite-rec --monitor 1 --codec vp8 --bitrate 10M` (whole screen).
- Fast-motion games need more bits: **10–12M** for VP8, **8–10M** for VP9.
  Default 6M is for desktop; games at 6M will block/pixelate on motion.
- If CPU spikes: drop to `--fps 30` (halves encode cost) or keep `--cpu-used 8`.

## Usage

```powershell
cargo build --release

# recordings land in D:\Recordings by default (auto-created)
.\target\release\lite-rec.exe --output out.webm            # => D:\Recordings\out.webm
.\target\release\lite-rec.exe --dir "D:\Videos" --output game.webm   # => D:\Videos\game.webm
.\target\release\lite-rec.exe --output "D:\Clips\warships.webm"      # full path bypasses --dir

# list targets (OBS-like picker, CLI version)
.\target\release\lite-rec.exe --list-monitors
.\target\release\lite-rec.exe --list-windows

# fullscreen 1080p60 WebM (VP8 = lowest CPU on your i5)
.\target\release\lite-rec.exe --output out.webm

# specific window, game-ready bitrate
.\target\release\lite-rec.exe --window "Notepad" --bitrate 10M --output game.webm

# 10-second test clip
.\target\release\lite-rec.exe --window "Modern Warships" --duration 10 --output test.webm

# VP9 (better compression, more CPU) / no cursor / 30fps fallback
.\target\release\lite-rec.exe --codec vp9 --bitrate 8M --output hq.webm
.\target\release\lite-rec.exe --no-cursor --output clean.webm
.\target\release\lite-rec.exe --fps 30 --output light.webm
```

Stop with **Enter** or **Ctrl+C** — the `.webm` is finalized cleanly.

## How it stays light

- Windows Graphics Capture API (GPU-composited, event-driven — idle screen ≈ 0% CPU)
- Fixed 1920×1080 pipe, center crop/pad in-Rust (cheap memcpy, no scaler)
- Bounded 2-frame channel + `try_send` (never blocks capture; drops = realtime, like OBS)
- Buffer reuse (no per-frame alloc), CFR pacer thread, `deadline realtime / cpu-used 8`
- FFmpeg auto-downloaded on first run (no manual install needed)

## Flags

```
--output, --dir (default D:\Recordings), --fps, --width/--height, --monitor N, --window "title..."
--list-windows, --list-monitors, --codec vp8|vp9, --bitrate 6M
--cpu-used 8, --threads 4, --duration N, --no-cursor, --border
```
