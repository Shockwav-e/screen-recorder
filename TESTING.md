# Testing

## Unit tests (`cargo test`)

Pure logic, runs anywhere including CI without a display, audio devices,
GPU, or ffmpeg binary:

- `caps::tests` — H.264 fallback order (QSV → NVENC → AMF → libx264),
  non-empty reason strings, HEVC/AV1 clean errors without hardware,
  `detect()` never panics with sane basics.
- `recorder::tests` — codec/container/audio matrix accept + reject cases
  (each rejection asserts the guidance text), tier bitrate scaling vs
  resolution incl. clamps, lossless-tier gating (VP9/x264 OK, VP8 rejected
  on any machine), encoder-arg shapes per family, output-extension
  validation (asserts nothing is created on disk on failure), container
  audio-default consistency.

## Real-hardware verification (filled 2.0.0)

Machine: i5-4570 (4C/4T), Intel HD 4600, 16 GB RAM, 1920x1080@100Hz
(`caps: 4 threads, 16301 MB RAM, display 1920x1080@100Hz, H.264[QSV],
HEVC[none], AV1[none]`).
Probes: H.264 QSV present; NVENC/AMF/HEVC-hw/AV1-hw absent (no such
hardware); libx264/VP8/VP9 present.

> IMPORTANT ENVIRONMENT CAVEAT: this session cannot deliver WGC frames —
> the 1.0 binary from a clean worktree behaves identically (0 frames,
> instant clean exit), while `gdigrab` captures the live desktop fine.
> So live-capture end-to-end runs are impossible *in this session*; the
> capture path itself is byte-identical to 1.0 and untouched by 2.0.
> Everything below was verified for real except live frame delivery.

### Encoder probes (real, this machine)

- [x] `h264_qsv` 5-frame probe: WORKS
- [x] `h264_nvenc` / `h264_amf`: fail as expected (no hardware)
- [x] `hevc_*` / `av1_*`: fail as expected (no hardware)
- [x] `libx264` / `libvpx` / `libvpx-vp9`: WORKS

### Matrix validation via CLI (real, each exits fast with the message)

- [x] `--codec vp8 --container mp4` → "`--codec vp8 can't go in .mp4`"
- [x] `--audio-codec opus` with MP4 → AAC guidance
- [x] `--output x.webm` with MP4 container → rename-or-`--container` guidance
- [x] `--quality lossless` on QSV → x264/VP9 guidance
- [x] `--codec h265` / `av1` / `h264-nvenc` → no-hardware guidance
- [x] `--quality high-quality` accepted; `--quality high` rejected with
  possible-values list (clap/serde spelling bug caught + fixed)
- [x] Missing/malformed `--config` → clear error; config precedence
  (CLI > file) confirmed via threads/dir/container in target lines
- [x] Default output lands in `%USERPROFILE%\Videos\recording.mp4`
  (Videos known-folder, no more `D:\` assumption)

### Standalone encode arg shapes (real, lavfi input, same args as the app)

- [x] x264 lossless (`-crf 0 -preset ultrafast -tune zerolatency`): OK, 6.1 MB
- [x] VP9 lossless (`-lossless 1 -b:v 0 -cpu-used 2` + tiles): OK, 4.9 MB
- [x] AAC (`-c:a aac -b:a 128k -ar 48000`): OK, proper AAC stream in MP4
- [x] QSV balanced (`-preset fast -look_ahead 0`): OK (via `--benchmark`)
- [x] x264/VP8/VP9 balanced: OK (via `--benchmark`)

### Crash-safety kill tests (real)

Procedure per container: encode with app-identical output-side args,
`taskkill /F` the ffmpeg PID at ~5 s wall time, then probe + decode.
(Input side is lavfi instead of rawvideo stdin — the mux flags under test
are identical.)

- [x] MP4 (fragmented): killed mid-encode → Duration readable, decode exit 0
- [x] MKV (2 s clusters): Duration N/A (no cues, expected) but stream
  detected, decode exit 0
- [x] WebM (2 s clusters): same as MKV, decode exit 0

### Benchmark (real)

- [x] `crabby --benchmark` on this machine:

```
crabby benchmark | caps: 4 threads, 16301 MB RAM, display 1920x1080@100Hz, H.264[QSV], HEVC[none], AV1[none]
synthetic  |  1920x1080 @ 60fps, 5 s, balanced/10M tier
  h264_qsv       128.3 fps  (realtime)
  libx264         85.8 fps  (realtime)
  libvpx          85.8 fps  (realtime)
  libvpx-vp9      88.1 fps  (realtime)
```

### GUI smoke (real)

- [x] `--gui` launches, renders all new controls (tiers, container,
  audio-codec, threads slider, update UI), alive 8 s, no panic

> Maintainer: re-run the kill tests + benchmark above on every release and
> refresh this section. Live-capture end-to-end runs need a session where
> WGC delivers frames (any interactive desktop — this headless-ish session
> was the only reason they couldn't run here).

## Known coverage gaps (honest list)

- No NVIDIA hardware: NVENC H.264/HEVC/AV1 success paths unverified; only
  the fail-fast messages are tested.
- No AMD hardware: same for AMF paths.
- No HEVC/AV1-capable Intel hardware: those success paths unverified.
- No multi-monitor / HiDPI / >1080p display tested (scratch sizing follows
  detected resolution by construction, unverified above 1080p).
- AAC quality only spot-checked by ear; bit-exact audio validation not done.
- Release CI runs clippy + unit tests on `windows-latest`, not the
  hardware matrix above.
