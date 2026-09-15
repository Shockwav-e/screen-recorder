# Changelog

## Unreleased

- Perf pre-flight guard (`check_perf`): software-encoder throughput is
  estimated from resolution × fps × threads before recording. The GUI warns
  and asks for a second Record press on doomed settings, with a one-click
  **Use safe 720p30 Fastest** escape hatch; the CLI prints an advisory
  `perf` line. Fixes silent 97%-drop recordings (e.g. software x264 at
  native 60 fps on 4-thread machines).
- Screenshots, Snipping-Tool style: Region/Window/Fullscreen capture
  (§5 panel + `--screenshot` CLI), auto-save + clipboard, basic pen + crop
  editor, and a desktop popup at the screen's bottom-right (thumbnail with
  hover-grow, Open/Copy, 3 s unless hovered).
- Screenshot hotkeys: global Win+PrtSc / PrtSc / Alt+PrtSc (config
  `shot_hotkeys = false` opts out) plus in-app Alt+N/W/F, Ctrl+S/C, Esc.
- Screenshot formats: PNG (default), WebP, JPG, BMP — GUI dropdown plus
  `shot_format` config key and `--shot-format` CLI flag.
- Screenshot settings: `shot_dir` config key, `%VAR%`/`$VAR` expansion in
  screenshot paths.
- Fixed `cmd.exe` console windows flashing on updater restart and on
  Play/open-file (CREATE_NO_WINDOW + `start /B`).
- ffmpeg stderr is now drained into a bounded ring buffer (an unread pipe
  could deadlock ffmpeg mid-record, surfacing as mass drops) and surfaced:
  last lines on failure, live line under the dropping-frames warning, tail
  on the saved message. `-loglevel` raised to `warning` to catch stall
  signatures like buffer-queue overflows.

## 2.0.0 — hardware-adaptive public release

**Breaking CLI changes** (no deprecation shims — 2.0.0 is the cleanup release):

- `--quality youtube|balanced` removed. New vendor-neutral tiers:
  `--quality fastest|balanced|high-quality|lossless`. The old `youtube`
  preset roughly maps to `balanced` (H.264, resolution-scaled bitrate) and
  the old `smooth` preset to `fastest`.
- Default output file renamed `gameplay.webm` → `recording` (the container
  extension is appended automatically).
- Default save folder changed from hardcoded `D:\Recordings` to your Windows
  Videos folder (override with `--dir` or config file).
- Mismatched output extensions are now an error, not a silent rename:
  `--output clip.webm` with an MP4 container fails with guidance instead of
  quietly writing `clip.mp4`.
- Default audio codec for MP4 is now AAC (was Opus-in-MP4, which most players
  handle poorly). WebM/MKV stay on Opus.
- `--threads` default is now auto (CPU count) instead of hardcoded 4.

Added:

- Hardware capability detection (`src/caps.rs`): QSV/NVENC/AMF probes for
  H.264, HEVC and AV1, plus CPU threads, RAM and display resolution. Every
  default derives from it; the chosen encoder and reason are logged.
- New codecs: H.265/HEVC and AV1 via detected hardware (no software
  fallbacks — absence is a clear error), forced software `x264`.
- New containers: MKV alongside MP4/WebM, with crash-safe muxing
  (fragmented MP4, bounded 2 s clusters for MKV/WebM).
- AAC audio alongside Opus, with container-aware defaults and validation.
- TOML config file (`%APPDATA%/Crabby/config.toml`, `--config` override).
  Precedence: CLI flag > config file > built-in default.
- `--benchmark` mode: per-encoder synthetic encode test with fps report.
- GUI parity: tier radios, container + audio-codec dropdowns, threads
  control, MP4/MKV in the recordings library.
- Unit tests for the codec/container/audio validation and the hardware
  fallback chain; `TESTING.md` documents real vs theoretical coverage.
- Release workflow now runs clippy (`-D warnings`) and `cargo test` before
  attaching the binary.

## 1.0.0

- Initial Crabby release: WGC monitor/window capture, H.264 (QSV/NVENC/AMF/
  x264) + VP8/VP9 via ffmpeg, egui GUI with live preview, WASAPI
  loopback/mic mixing to Opus, GitHub self-updater, embedded icon + version
  metadata.
