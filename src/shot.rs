//! Screenshots, Snipping-Tool style: fullscreen / window / drag-region.
//!
//! Capture reuses the same Windows Graphics Capture path as the recorder
//! (WGC via `windows-capture`), but grabs a single frame instead of a
//! stream: start the session, take the first frame, stop. No cursor, no
//! border — like the Snipping Tool.
//!
//! After capture the caller decides what to do: [`save_png`] writes a
//! timestamped PNG, [`copy_to_clipboard`] puts RGBA on the clipboard, and
//! [`crop`] backs region-snips (fullscreen capture + crop to the drag rect).
//! [`stroke_line`] backs the basic pen in the GUI editor.

use std::borrow::Cow;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

/// Screenshot file format. PNG is lossless (bigger, slower to encode);
/// WebP is lossless too but much smaller (best all-rounder on modern apps);
/// JPG is the fastest/smallest (lossy, no alpha — best for quick shares);
/// BMP is raw (fastest encode, huge files — rarely what you want).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ShotFormat {
    /// Lossless, default. Best text sharpness.
    #[default]
    Png,
    /// Lossless + small. Best all-rounder (modern apps/Discord support it).
    Webp,
    /// Fastest + smallest. Best for quick shares.
    Jpg,
    /// Uncompressed. Fast encode, huge files.
    Bmp,
}

impl ShotFormat {
    pub fn ext(self) -> &'static str {
        match self {
            ShotFormat::Png => "png",
            ShotFormat::Webp => "webp",
            ShotFormat::Jpg => "jpg",
            ShotFormat::Bmp => "bmp",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            ShotFormat::Png => "PNG (sharp)",
            ShotFormat::Webp => "WebP (small + sharp)",
            ShotFormat::Jpg => "JPG (fastest)",
            ShotFormat::Bmp => "BMP (raw)",
        }
    }
}

/// Which part of the screen to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ShotMode {
    /// Whole monitor (primary, or `--monitor N`).
    Fullscreen,
    /// First window whose title contains `--window <text>`.
    Window,
    /// Crop of the monitor: needs `--shot-region WxH+X+Y` (physical pixels).
    Region,
}

/// Parse `WxH+X+Y` (e.g. `800x600+100+200`) into (x, y, w, h).
pub fn parse_region(s: &str) -> Result<(u32, u32, u32, u32)> {
    let s = s.trim().to_lowercase();
    let (wh, xy) = s
        .split_once('+')
        .context("bad --shot-region: use WxH+X+Y, e.g. 800x600+100+200")?;
    let (w, h) = wh
        .split_once('x')
        .context("bad --shot-region: use WxH+X+Y, e.g. 800x600+100+200")?;
    let mut rest = xy.split('+');
    let (w, h, x, y) = (
        w.trim().parse::<u32>().unwrap_or(0),
        h.trim().parse::<u32>().unwrap_or(0),
        rest.next().unwrap_or("0").trim().parse::<u32>().unwrap_or(0),
        rest.next().unwrap_or("0").trim().parse::<u32>().unwrap_or(0),
    );
    if w < 2 || h < 2 || w > 7680 || h > 7680 {
        anyhow::bail!("bad --shot-region \"{s}\": use WxH+X+Y, e.g. 800x600+100+200");
    }
    Ok((x, y, w, h))
}

/// A finished screenshot: 8-bit RGBA, row-major, top-left origin.
#[derive(Debug, Clone, Default)]
pub struct Shot {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Shot {
    pub fn is_empty(&self) -> bool {
        self.rgba.is_empty() || self.width == 0 || self.height == 0
    }

    pub fn pixel_len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    /// Validate the buffer matches the dimensions (guards against a
    /// transient resize frame sneaking through).
    pub fn validate(&self) -> Result<()> {
        if self.width < 2 || self.height < 2 {
            bail!("capture delivered an empty frame (window minimized?)");
        }
        if self.rgba.len() != self.pixel_len() {
            bail!("capture frame size mismatch (transient resize?) — try again");
        }
        Ok(())
    }
}

// ---------- single-frame WGC plumbing ----------

struct ShotFlags {
    tx: crossbeam_channel::Sender<Shot>,
}

struct ShotHandler {
    tx: crossbeam_channel::Sender<Shot>,
    scratch: Vec<u8>,
    done: bool,
}

impl GraphicsCaptureApiHandler for ShotHandler {
    type Flags = ShotFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { tx: ctx.flags.tx.clone(), scratch: Vec::new(), done: false })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.done {
            control.stop();
            return Ok(());
        }
        let w = frame.width();
        let h = frame.height();
        if w == 0 || h == 0 {
            return Ok(()); // transient during resize — wait for the next one
        }
        let fb = frame.buffer()?;
        let tmp = fb.as_nopadding_buffer(&mut self.scratch);
        let expect = w as usize * h as usize * 4;
        if tmp.len() < expect {
            return Ok(()); // malformed frame, wait for the next one
        }
        let shot = Shot { width: w, height: h, rgba: tmp[..expect].to_vec() };
        let _ = self.tx.try_send(shot);
        self.done = true;
        control.stop();
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Run one WGC session on a helper thread and wait for its first frame.
/// WGC always delivers the current composited frame on start (even for a
/// static screen), so a timeout here means the source is gone.
fn grab_first_frame(
    build: impl FnOnce(ShotFlags) -> Result<()> + Send + 'static,
) -> Result<Shot> {
    let (tx, rx) = crossbeam_channel::bounded::<Shot>(1);
    // Detached by design: the capture call blocks until our handler stops it
    // on the first frame — the thread then exits on its own. On timeout we
    // bail and let the thread finish/clean up.
    std::thread::spawn(move || {
        let res = build(ShotFlags { tx });
        if let Err(e) = res {
            eprintln!("screenshot capture failed: {e:?}");
        }
    });
    match rx.recv_timeout(Duration::from_secs(8)) {
        Ok(shot) => {
            shot.validate()?;
            Ok(shot)
        }
        Err(_) => bail!("no frame arrived (window closed or minimized?)"),
    }
}

/// Fullscreen capture of one monitor (1-based `index`, `None` = primary).
pub fn capture_monitor(index: Option<usize>) -> Result<Shot> {
    grab_first_frame(move |flags| {
        let item = match index {
            Some(i) => Monitor::from_index(i.max(1))?,
            None => Monitor::primary()?,
        };
        let settings = Settings::new(
            item,
            CursorCaptureSettings::WithoutCursor,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Rgba8,
            flags,
        );
        ShotHandler::start(settings).context("monitor capture failed")?;
        Ok(())
    })
}

/// Capture the first window whose title contains `needle` (case-sensitive,
/// same matching as `--window` recording).
pub fn capture_window(needle: &str) -> Result<Shot> {
    let needle = needle.to_owned();
    grab_first_frame(move |flags| {
        let item = Window::from_contains_name(&needle)
            .with_context(|| format!("no window title contains \"{needle}\""))?;
        let settings = Settings::new(
            item,
            CursorCaptureSettings::WithoutCursor,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Rgba8,
            flags,
        );
        ShotHandler::start(settings).context("window capture failed")?;
        Ok(())
    })
}

// ---------- region / crop ----------

/// Crop `shot` to the physical-pixel rect (x, y, w, h). Out-of-bounds edges
/// are clamped; a fully-out-of-bounds or degenerate rect is an error.
pub fn crop(shot: &Shot, x: u32, y: u32, w: u32, h: u32) -> Result<Shot> {
    shot.validate()?;
    let x = x.min(shot.width.saturating_sub(1));
    let y = y.min(shot.height.saturating_sub(1));
    let w = w.max(1).min(shot.width - x);
    let h = h.max(1).min(shot.height - y);
    if w < 2 || h < 2 {
        bail!("selection too small — drag a larger region");
    }
    let sw = shot.width as usize;
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for row in 0..h as usize {
        let s = ((y as usize + row) * sw + x as usize) * 4;
        rgba.extend_from_slice(&shot.rgba[s..s + w as usize * 4]);
    }
    Ok(Shot { width: w, height: h, rgba })
}

// ---------- save / clipboard ----------

/// Expand `%VAR%` (Windows) / `$VAR` (other) in a folder path so values
/// like `%TEMP%/shots` or `$HOME/shots` work from CLI and config files.
/// Unknown variables are left as-is.
pub fn expand_dir(dir: &str) -> String {
    let mut out = dir.to_owned();
    // Windows %VAR% style.
    while let Some(a) = out.find('%') {
        let Some(b) = out[a + 1..].find('%').map(|i| a + 1 + i) else { break };
        let key = out[a + 1..b].to_owned();
        let val = std::env::var(&key).unwrap_or_else(|_| format!("%{key}%"));
        out.replace_range(a..=b, &val);
        if val.starts_with('%') {
            break; // unknown var — stop instead of looping forever
        }
    }
    // $VAR style (and ${VAR}).
    let mut res = String::with_capacity(out.len());
    let mut it = out.chars().peekable();
    while let Some(c) = it.next() {
        if c == '$' {
            if it.peek() == Some(&'{') {
                it.next();
                let mut name = String::new();
                for ch in it.by_ref() {
                    if ch == '}' {
                        break;
                    }
                    name.push(ch);
                }
                res.push_str(&std::env::var(&name).unwrap_or_else(|_| format!("${{{name}}}")));
            } else {
                // Collect without eating the terminator (`take_while` would
                // swallow it — hence the manual peek loop).
                let mut name = String::new();
                while matches!(it.peek(), Some(ch) if ch.is_alphanumeric() || *ch == '_') {
                    name.push(it.next().unwrap_or_default());
                }
                if name.is_empty() {
                    res.push('$');
                } else {
                    res.push_str(&std::env::var(&name).unwrap_or_else(|_| format!("${name}")));
                }
            }
        } else {
            res.push(c);
        }
    }
    res
}

/// Where screenshots land: `Pictures/Crabby`, falling back to the Videos
/// folder and then the current directory. Never a fixed drive letter.
pub fn screenshot_dir() -> String {
    if let Some(p) = dirs::picture_dir() {
        return p.join("Crabby").to_string_lossy().into_owned();
    }
    crate::config::default_video_dir()
}

fn unique_path(dir: &std::path::Path, stem: &str, ext: &str) -> PathBuf {
    let cand = dir.join(format!("{stem}.{ext}"));
    if !cand.exists() {
        return cand;
    }
    for i in 1..10000 {
        let cand = dir.join(format!("{stem}_{i:03}.{ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    dir.join(format!(
        "{stem}_{}.{ext}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    ))
}

/// Encode and save under `dir` (created if missing) with a timestamped
/// stem like `shot-1234567890-10-30-00`. The container extension comes from
/// `format`; explicit save work runs on a background thread at call sites so
/// the UI never blocks on the (slow, for PNG) encode. Returns the full path.
pub fn save_shot(
    shot: &Shot,
    dir: &str,
    stem: Option<&str>,
    format: ShotFormat,
) -> Result<String> {
    shot.validate()?;
    let d = PathBuf::from(expand_dir(dir));
    std::fs::create_dir_all(&d).with_context(|| format!("cannot create folder {dir}"))?;
    let stem = stem.map(|s| s.to_owned()).unwrap_or_else(|| format!("shot-{}", chrono_stamp()));
    // Sanitize the stem: bare file name only, no folders or extensions.
    let stem: String = stem
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let stem = stem.trim_matches('_');
    let stem = if stem.is_empty() { "shot".to_owned() } else { stem.to_owned() };
    let ext = format.ext();
    let path = unique_path(&d, &stem, ext);
    // JPEG has no alpha: screenshots are opaque, so JPG drops A (fast single
    // pass inside `encode_to`, no extra full-size RGBA copy).
    encode_to(shot, &path, format)?;
    Ok(path.to_string_lossy().into_owned())
}

/// Save to an explicit file path (parents created). The extension must match
/// `format` or be absent (appended). Returns the full path.
pub fn save_shot_to(shot: &Shot, path: &str, format: ShotFormat) -> Result<String> {
    shot.validate()?;
    let mut out = path.to_owned();
    let want = format!(".{}", format.ext());
    if !out.to_lowercase().ends_with(&want) {
        // Reject a conflicting known image extension instead of mislabeling.
        let lower = out.to_lowercase();
        for known in [".png", ".webp", ".jpg", ".jpeg", ".bmp"] {
            if lower.ends_with(known) && known != want {
                anyhow::bail!(
                    "output ends with {known} but {want} was requested — rename or change format"
                );
            }
        }
        out.push_str(&want);
    }
    let out = expand_dir(&out);
    let p = PathBuf::from(&out);
    if let Some(par) = p.parent() {
        if !par.as_os_str().is_empty() {
            std::fs::create_dir_all(par)
                .with_context(|| format!("cannot create folder {}", par.display()))?;
        }
    }
    encode_to(shot, &p, format)?;
    Ok(p.to_string_lossy().into_owned())
}

fn encode_to(shot: &Shot, path: &PathBuf, format: ShotFormat) -> Result<()> {
    shot.validate()?;
    match format {
        ShotFormat::Png | ShotFormat::Bmp | ShotFormat::Webp => {
            let img = image::RgbaImage::from_raw(shot.width, shot.height, shot.rgba.clone())
                .context("bad pixel buffer")?;
            img.save(path).with_context(|| format!("couldn't save {}", path.display()))?;
        }
        ShotFormat::Jpg => {
            let mut rgb = Vec::with_capacity(shot.width as usize * shot.height as usize * 3);
            for px in shot.rgba.chunks_exact(4) {
                rgb.extend_from_slice(&px[..3]);
            }
            let img = image::RgbImage::from_raw(shot.width, shot.height, rgb)
                .context("bad pixel buffer")?;
            img.save(path).with_context(|| format!("couldn't save {}", path.display()))?;
        }
    }
    Ok(())
}

fn chrono_stamp() -> String {
    // No chrono dep — build the stamp from system time. Calendar-accurate
    // date math without a date lib is overkill; epoch-based uniqueness plus
    // readable time-of-day is enough for file names.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{secs}-{h:02}-{m:02}-{s:02}")
}

/// Copy RGBA to the system clipboard (paste into Discord, Paint, …).
pub fn copy_to_clipboard(shot: &Shot) -> Result<()> {
    shot.validate()?;
    let mut clip = arboard::Clipboard::new().context("couldn't open clipboard")?;
    clip.set_image(arboard::ImageData {
        width: shot.width as usize,
        height: shot.height as usize,
        bytes: Cow::Borrowed(&shot.rgba),
    })
    .context("couldn't copy image to clipboard")?;
    Ok(())
}

// ---------- basic pen ----------

/// Draw a brush segment from (x0,y0) to (y1) in image pixels. `rgba_color`
/// is [r,g,b,a]; thickness is the brush diameter in pixels. Alpha blends
/// over the existing pixels; out-of-bounds segments are clipped.
pub fn stroke_line(
    shot: &mut Shot,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    rgba_color: [u8; 4],
    thickness: f32,
) {
    if shot.is_empty() {
        return;
    }
    let radius = (thickness.max(1.0) / 2.0).ceil() as i32;
    let dx = x1 - x0;
    let dy = y1 - y0;
    let steps = (dx.abs().max(dy.abs()).ceil() as i32).max(1);
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let cx = (x0 + dx * t).round() as i32;
        let cy = (y0 + dy * t).round() as i32;
        stamp(shot, cx, cy, radius, rgba_color);
    }
}

fn stamp(shot: &mut Shot, cx: i32, cy: i32, radius: i32, color: [u8; 4]) {
    let (w, h) = (shot.width as i32, shot.height as i32);
    let alpha = color[3] as f32 / 255.0;
    if alpha <= 0.0 {
        return;
    }
    for oy in -radius..=radius {
        for ox in -radius..=radius {
            if ox * ox + oy * oy > radius * radius {
                continue;
            }
            let (x, y) = (cx + ox, cy + oy);
            if x < 0 || y < 0 || x >= w || y >= h {
                continue;
            }
            let o = (y as usize * w as usize + x as usize) * 4;
            let dst = &mut shot.rgba[o..o + 4];
            // "over" blend for rgb; keep alpha opaque (screenshots are opaque).
            for c in 0..3 {
                let s = color[c] as f32;
                let d = dst[c] as f32;
                dst[c] = (s * alpha + d * (1.0 - alpha)).round().clamp(0.0, 255.0) as u8;
            }
            dst[3] = 255;
        }
    }
}

// ---------- region selector overlay (Snipping-Tool style drag) ----------

/// Fullscreen drag-to-select overlay: dims the screen, you drag a rectangle,
/// release to confirm, Esc/right-click to cancel.
///
/// Returns the rect in physical pixels (x, y, w, h), or `None` on cancel.
/// Blocks until done — call from a background thread, never the UI thread.
/// Covers the primary monitor (multi-monitor span is a future improvement).
/// Channel the region overlay uses to report its rect (or `None` on cancel).
type RegionTx = std::sync::mpsc::Sender<Option<(u32, u32, u32, u32)>>;

/// Open a file with its default app, without flashing a console window.
pub fn open_file(path: &str) -> Result<()> {
    let mut cmd = std::process::Command::new("cmd");
    cmd.args(["/C", "start", "", path]);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    cmd.spawn().map(|_| ()).with_context(|| format!("couldn't open {path}"))?;
    Ok(())
}

// ---------- desktop popup (Windows-toast style) ----------

/// How long the popup stays. Hovering pauses the countdown (and grows the
/// thumbnail), like the Snipping Tool notification.
const POPUP_SECS: u64 = 3;
const POPUP_W: f32 = 344.0;
const POPUP_H: f32 = 136.0;

static POPUP_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Spawn a detached bottom-right desktop popup for a saved screenshot:
/// thumbnail (grows on hover), file name, Open/Copy/dismiss, gone after
/// [`POPUP_SECS`] unless hovered. A separate OS window — visible even when
/// the main window is minimized. Call from anywhere; never blocks.
pub fn show_popup(shot: Shot, path: String) {
    if shot.validate().is_err() {
        return;
    }
    std::thread::spawn(move || {
        let viewport = eframe::egui::ViewportBuilder::default()
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_resizable(false)
            .with_inner_size([POPUP_W, POPUP_H])
            .with_title("Crabby screenshot");
        let opts = eframe::NativeOptions { viewport, ..Default::default() };
        let _ = eframe::run_native(
            "crabby-shot-popup",
            opts,
            Box::new(move |cc| {
                let tex = cc.egui_ctx.load_texture(
                    "shot-popup",
                    eframe::egui::ColorImage::from_rgba_unmultiplied(
                        [shot.width as usize, shot.height as usize],
                        &shot.rgba,
                    ),
                    eframe::egui::TextureOptions::LINEAR,
                );
                Ok(Box::new(Popup {
                    tex,
                    shot,
                    path,
                    created: Instant::now(),
                    placed: false,
                    close: false,
                    hover_t: 0.0,
                    copied_at: None,
                    cascade: POPUP_COUNT
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                }) as Box<dyn eframe::App>)
            }),
        );
    });
}

struct Popup {
    tex: eframe::egui::TextureHandle,
    shot: Shot,
    path: String,
    created: Instant,
    placed: bool,
    close: bool,
    /// Smoothed hover 0..1 for the grow animation.
    hover_t: f32,
    copied_at: Option<Instant>,
    cascade: usize,
}

impl eframe::App for Popup {
    fn clear_color(&self, _visuals: &eframe::egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        use eframe::egui::*;
        // Pin to the screen's bottom-right on the first frame, when the real
        // pixels-per-point is known (DPI-correct on any scaling).
        if !self.placed {
            self.placed = true;
            let ppp = ctx.pixels_per_point().max(0.5);
            let (mw, mh) = Monitor::primary()
                .ok()
                .map(|m| (m.width().unwrap_or(1920) as f32, m.height().unwrap_or(1080) as f32))
                .unwrap_or((1920.0, 1080.0));
            let x = (mw / ppp - POPUP_W - 12.0).max(0.0);
            let stack = (self.cascade % 3) as f32 * (POPUP_H + 12.0);
            let y = (mh / ppp - POPUP_H - 64.0 - stack).max(0.0);
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(pos2(x, y)));
        }

        let thumb_w = 132.0 + 28.0 * self.hover_t;
        let mut open = false;
        let mut copy = false;
        CentralPanel::default().frame(Frame::NONE).show(ctx, |ui| {
            let card = Frame {
                fill: Color32::from_rgb(32, 32, 36),
                corner_radius: CornerRadius::same(10),
                stroke: Stroke::new(1.0_f32, Color32::from_white_alpha(24)),
                inner_margin: Margin::same(10),
                outer_margin: Margin::same(6),
                ..Default::default()
            };
            let ir = card.show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(Image::new(&self.tex).max_width(thumb_w).corner_radius(6.0));
                    ui.vertical(|ui| {
                        ui.strong("Screenshot saved");
                        let mut nm = self
                            .path
                            .rsplit(['/', '\\'])
                            .next()
                            .unwrap_or(&self.path)
                            .to_owned();
                        if nm.len() > 26 {
                            nm.truncate(25);
                            nm.push('…');
                        }
                        ui.monospace(nm);
                        if self
                            .copied_at
                            .map(|t| t.elapsed() < Duration::from_millis(1500))
                            .unwrap_or(false)
                        {
                            ui.colored_label(Color32::GREEN, "Copied ✓");
                        } else {
                            let left = POPUP_SECS
                                .saturating_sub(self.created.elapsed().as_secs());
                            ui.weak(format!("closing in {left}s · hover to keep"));
                        }
                        ui.horizontal(|ui| {
                            if ui.small_button("Open").clicked() {
                                open = true;
                            }
                            if ui.small_button("Copy").clicked() {
                                copy = true;
                            }
                            if ui.small_button("✕").clicked() {
                                self.close = true;
                            }
                        });
                    });
                });
            });
            self.hover_t =
                ctx.animate_bool_with_time(ir.response.id, ir.response.hovered(), 0.15);
        });

        if open {
            let _ = open_file(&self.path);
            self.close = true;
        }
        if copy && copy_to_clipboard(&self.shot).is_ok() {
            self.copied_at = Some(Instant::now());
        }
        let hovered = self.hover_t > 0.02;
        if self.close || (self.created.elapsed() > Duration::from_secs(POPUP_SECS) && !hovered) {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        } else {
            // Countdown + grow animation without busy-looping.
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }
}

pub fn select_region() -> Result<Option<(u32, u32, u32, u32)>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let tx_fallback = tx.clone();
    std::thread::spawn(move || {
        let viewport = eframe::egui::ViewportBuilder::default()
            .with_fullscreen(true)
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_resizable(false)
            .with_title("Crabby snip — drag to select, Esc to cancel");
        let opts = eframe::NativeOptions { viewport, ..Default::default() };
        let res = eframe::run_native(
            "crabby-snip",
            opts,
            Box::new(move |_cc| {
                Ok(Box::new(Selector {
                    tx: Some(tx),
                    start: None,
                    cur: eframe::egui::Pos2::ZERO,
                }) as Box<dyn eframe::App>)
            }),
        );
        if res.is_err() {
            let _ = tx_fallback.send(None);
        }
    });
    rx.recv().context("region selector failed")
}

struct Selector {
    tx: Option<RegionTx>,
    start: Option<eframe::egui::Pos2>,
    cur: eframe::egui::Pos2,
}

impl Selector {
    fn finish(&mut self, ctx: &eframe::egui::Context, rect: Option<(u32, u32, u32, u32)>) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(rect);
        }
        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Close);
    }
}

impl eframe::App for Selector {
    fn clear_color(&self, _visuals: &eframe::egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0] // transparent so the screen shows through
    }

    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        use eframe::egui::*;
        ctx.request_repaint(); // track the mouse smoothly (short-lived window)

        // Cancel paths.
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.finish(ctx, None);
            return;
        }
        if ctx.input(|i| i.pointer.secondary_clicked()) {
            self.finish(ctx, None);
            return;
        }

        if let Some(p) = ctx.input(|i| i.pointer.hover_pos()) {
            self.cur = p;
        }
        if ctx.input(|i| i.pointer.primary_pressed()) {
            if let Some(p) = ctx.input(|i| i.pointer.hover_pos()) {
                self.start = Some(p);
                self.cur = p;
            }
        }
        if ctx.input(|i| i.pointer.primary_released()) {
            match self.start.take() {
                Some(s) => {
                    let ppp = ctx.pixels_per_point();
                    let x = (s.x.min(self.cur.x) * ppp).round().max(0.0) as u32;
                    let y = (s.y.min(self.cur.y) * ppp).round().max(0.0) as u32;
                    let w = ((s.x - self.cur.x).abs() * ppp).round() as u32;
                    let h = ((s.y - self.cur.y).abs() * ppp).round() as u32;
                    if w >= 2 && h >= 2 {
                        self.finish(ctx, Some((x, y, w, h)));
                    } else {
                        self.finish(ctx, None); // click without drag = cancel
                    }
                }
                None => self.finish(ctx, None),
            }
            return;
        }

        CentralPanel::default()
            .frame(Frame::NONE.fill(Color32::from_black_alpha(90)))
            .show(ctx, |ui| {
                let painter = ui.painter();
                let full = ui.max_rect();
                // Crosshair through the cursor.
                painter.hline(full.x_range(), self.cur.y, (1.0, Color32::WHITE));
                painter.vline(self.cur.x, full.y_range(), (1.0, Color32::WHITE));
                match self.start {
                    Some(s) => {
                        let r = Rect::from_two_pos(s, self.cur);
                        painter.rect_filled(r, 0.0, Color32::from_white_alpha(28));
                        painter.rect_stroke(
                            r,
                            0.0,
                            Stroke::new(2.0_f32, Color32::from_rgb(88, 101, 242)),
                            StrokeKind::Outside,
                        );
                        let ppp = ctx.pixels_per_point();
                        let w = ((s.x - self.cur.x).abs() * ppp).round() as u32;
                        let h = ((s.y - self.cur.y).abs() * ppp).round() as u32;
                        painter.text(
                            r.left_top() + vec2(4.0, -20.0),
                            Align2::LEFT_TOP,
                            format!("{w} × {h}"),
                            FontId::proportional(14.0),
                            Color32::WHITE,
                        );
                    }
                    None => {
                        painter.text(
                            full.center(),
                            Align2::CENTER_CENTER,
                            "Drag to select  ·  Esc to cancel",
                            FontId::proportional(18.0),
                            Color32::WHITE,
                        );
                    }
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_shot(w: u32, h: u32) -> Shot {
        Shot { width: w, height: h, rgba: vec![128u8; w as usize * h as usize * 4] }
    }

    #[test]
    fn crop_clamps_and_copies() {
        let s = test_shot(100, 80);
        let c = crop(&s, 10, 10, 20, 20).unwrap();
        assert_eq!((c.width, c.height), (20, 20));
        assert_eq!(c.rgba.len(), 20 * 20 * 4);
        // Partially out of bounds clamps instead of failing.
        let c = crop(&s, 90, 70, 50, 50).unwrap();
        assert_eq!((c.width, c.height), (10, 10));
    }

    #[test]
    fn crop_rejects_degenerate() {
        let s = test_shot(100, 80);
        assert!(crop(&s, 0, 0, 1, 1).is_err());
        let empty = Shot::default();
        assert!(crop(&empty, 0, 0, 10, 10).is_err());
    }

    #[test]
    fn pen_paints_within_bounds() {
        let mut s = test_shot(32, 32);
        stroke_line(&mut s, -50.0, -50.0, 16.0, 16.0, [255, 0, 0, 255], 5.0);
        // Center pixel took the red paint.
        let o = (16 * 32 + 16) * 4;
        assert_eq!(&s.rgba[o..o + 3], &[255, 0, 0]);
        // Top-right corner is far from the diagonal — stayed gray.
        let o = 31 * 4;
        assert_eq!(&s.rgba[o..o + 3], &[128, 128, 128]);
        assert_eq!(s.rgba.len(), 32 * 32 * 4);
    }

    #[test]
    fn save_roundtrip_png() {
        let s = test_shot(16, 16);
        let dir = std::env::temp_dir().join("crabby-shot-test");
        let _ = std::fs::remove_dir_all(&dir);
        let p = save_shot(&s, dir.to_str().unwrap(), Some("unit"), ShotFormat::Png).unwrap();
        assert!(p.ends_with(".png"));
        let back = image::open(&p).unwrap().to_rgba8();
        assert_eq!((back.width(), back.height()), (16, 16));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_roundtrip_webp_and_jpg() {
        let s = test_shot(16, 16);
        let dir = std::env::temp_dir().join("crabby-shot-test-fmt");
        let _ = std::fs::remove_dir_all(&dir);
        let p = save_shot(&s, dir.to_str().unwrap(), Some("unit"), ShotFormat::Webp).unwrap();
        assert!(p.ends_with(".webp"));
        let back = image::open(&p).unwrap().to_rgba8();
        assert_eq!((back.width(), back.height()), (16, 16));
        let p = save_shot(&s, dir.to_str().unwrap(), Some("unit"), ShotFormat::Jpg).unwrap();
        assert!(p.ends_with(".jpg"));
        let back = image::open(&p).unwrap().to_rgb8();
        assert_eq!((back.width(), back.height()), (16, 16));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_dir_resolves_vars() {
        std::env::set_var("CRABBY_SHOT_TEST_VAR", "hello-dir");
        assert_eq!(expand_dir("%CRABBY_SHOT_TEST_VAR%/x"), "hello-dir/x");
        assert_eq!(expand_dir("$CRABBY_SHOT_TEST_VAR/x"), "hello-dir/x");
        assert_eq!(expand_dir("%NOPE_NOT_SET_123%/x"), "%NOPE_NOT_SET_123%/x");
        assert_eq!(expand_dir("plain/path"), "plain/path");
    }

    #[test]
    fn region_parses_and_rejects() {
        assert_eq!(parse_region("800x600+100+200").unwrap(), (100, 200, 800, 600));
        assert_eq!(parse_region(" 1920X1080+0+0 ").unwrap(), (0, 0, 1920, 1080));
        assert!(parse_region("nope").is_err());
        assert!(parse_region("10x10+0+0").is_ok());
        assert!(parse_region("1x1+0+0").is_err());
    }
}
