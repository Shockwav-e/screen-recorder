//! Modern native GUI for lite-rec (egui/eframe — GPU-accelerated, no webview).
//!
//! Performance best practices applied:
//!   - UI thread never blocks: capture + encode run on background threads,
//!     `Session::wait()` finalizes on a helper thread during Stop.
//!   - Repaints throttled to 5 Hz while recording (stats only); fully static
//!     when idle — the UI costs ~0% CPU otherwise.
//!   - No live preview: on an HD 4600 a preview texture upload + composite per
//!     frame would steal GPU/CPU from the game being recorded. Stats instead.

use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui;

use crate::recorder::{
    Codec, Quality, RecordConfig, Snapshot, Source, Session, describe_source, list_monitors,
    list_windows, resolve_output, start_session, MonitorInfo, WindowInfo,
};

const BITRATES: [&str; 5] = ["8M", "10M", "12M", "16M", "20M"];

#[derive(PartialEq)]
enum Phase {
    Idle,
    Recording,
    Stopping,
}

struct StopOutcome {
    output: String,
    result: std::result::Result<Snapshot, String>,
}

pub struct GuiApp {
    monitors: Vec<MonitorInfo>,
    windows: Vec<WindowInfo>,
    lists_error: Option<String>,

    use_window: bool,
    monitor_sel: usize,
    win_filter: String,
    win_sel: Option<usize>,

    dir: String,
    filename: String,
    quality: Quality,
    bitrate_sel: usize,
    fps60: bool,
    no_cursor: bool,
    border: bool,
    auto_stop: bool,
    stop_secs: u32,

    phase: Phase,
    session: Option<Session>,
    pending: Option<Receiver<StopOutcome>>,
    start_t: Instant,
    snap: Snapshot,
    out_fps: f32,
    last_written: u64,
    last_t: Instant,

    done_msg: Option<String>,
    error_msg: Option<String>,
}

impl GuiApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Modern dark theme, slightly rounded, comfortable spacing.
        let mut style = (*cc.egui_ctx.style()).clone();
        style.visuals = egui::Visuals::dark();
        style.visuals.selection.bg_fill = egui::Color32::from_rgb(88, 101, 242); // blurple
        for w in [
            &mut style.visuals.widgets.hovered,
            &mut style.visuals.widgets.inactive,
            &mut style.visuals.widgets.active,
        ] {
            w.corner_radius = egui::CornerRadius::same(6);
        }
        style.spacing.button_padding = egui::vec2(12.0, 7.0);
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        cc.egui_ctx.set_style(style);

        let mut app = Self {
            monitors: Vec::new(),
            windows: Vec::new(),
            lists_error: None,
            use_window: false,
            monitor_sel: 0,
            win_filter: String::new(),
            win_sel: None,
            dir: r"D:\Recordings".to_owned(),
            filename: "gameplay.webm".to_owned(),
            quality: Quality::Youtube,
            bitrate_sel: 3, // 16M
            fps60: true,
            no_cursor: false,
            border: false,
            auto_stop: false,
            stop_secs: 60,
            phase: Phase::Idle,
            session: None,
            pending: None,
            start_t: Instant::now(),
            snap: Snapshot::default(),
            out_fps: 0.0,
            last_written: 0,
            last_t: Instant::now(),
            done_msg: None,
            error_msg: None,
        };
        app.refresh_sources();
        app
    }

    fn refresh_sources(&mut self) {
        match list_monitors() {
            Ok(m) => {
                self.monitors = m;
                if self.monitor_sel >= self.monitors.len() {
                    self.monitor_sel = 0;
                }
            }
            Err(e) => self.lists_error = Some(format!("monitors: {e:?}")),
        }
        match list_windows() {
            Ok(w) => {
                self.windows = w;
                self.win_sel = None;
            }
            Err(e) => self.lists_error = Some(format!("windows: {e:?}")),
        }
    }

    fn selected_bitrate(&self) -> String {
        BITRATES[self.bitrate_sel.min(BITRATES.len() - 1)].to_owned()
    }

    fn start(&mut self) {
        self.done_msg = None;
        self.error_msg = None;

        let source = if self.use_window {
            let Some(i) = self.win_sel else {
                self.error_msg = Some("Pick a window from the list first.".to_owned());
                return;
            };
            let Some(w) = self.windows.get(i) else {
                self.error_msg = Some("Selected window is gone — hit Refresh.".to_owned());
                return;
            };
            Source::Window { needle: w.title.clone() }
        } else {
            let index = self.monitors.get(self.monitor_sel).map(|m| m.index);
            Source::Monitor { index }
        };

        // Sanitize: filename field must stay a bare file name (no folders or
        // drive letters — those belong in the Folder field above).
        let fname = std::path::Path::new(&self.filename)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "gameplay.webm".to_owned());
        let output = match resolve_output(&fname, &self.dir) {
            Ok(p) => p,
            Err(e) => {
                self.error_msg = Some(format!("bad output: {e:?}"));
                return;
            }
        };

        let codec: Codec = self.quality.codec();
        let cfg = RecordConfig {
            output,
            fps: if self.fps60 { 60 } else { 30 },
            width: 1920,
            height: 1080,
            source,
            codec,
            bitrate: self.selected_bitrate(),
            cpu_used: self.quality.cpu_used(),
            threads: 4,
            duration: self.auto_stop.then_some(self.stop_secs.max(5) as u64),
            no_cursor: self.no_cursor,
            border: self.border,
        };
        // Validate the source exists before claiming "recording".
        if let Err(e) = describe_source(&cfg) {
            self.error_msg = Some(format!("source unavailable: {e:?}"));
            return;
        }
        match start_session(cfg) {
            Ok(sess) => {
                self.start_t = Instant::now();
                self.last_t = Instant::now();
                self.last_written = 0;
                self.out_fps = 0.0;
                self.snap = Snapshot::default();
                self.session = Some(sess);
                self.phase = Phase::Recording;
            }
            Err(e) => self.error_msg = Some(format!("couldn't start: {e:?}")),
        }
    }

    /// Graceful stop: signal threads, finalize the .webm on a helper thread
    /// (joining can take ~1s — never on the UI thread).
    fn begin_stop(&mut self) {
        let Some(sess) = self.session.take() else {
            self.phase = Phase::Idle;
            return;
        };
        sess.request_stop();
        let output = sess.output().to_owned();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = sess.wait().map_err(|e| format!("{e:?}"));
            let _ = tx.send(StopOutcome { output, result });
        });
        self.pending = Some(rx);
        self.phase = Phase::Stopping;
    }

    fn poll(&mut self) {
        if self.phase == Phase::Recording {
            if let Some(sess) = &self.session {
                self.snap = sess.snapshot();
                // output fps from written-counter delta
                let now = Instant::now();
                let dt = now.duration_since(self.last_t).as_secs_f32();
                if dt > 0.05 {
                    self.out_fps =
                        (self.snap.written - self.last_written) as f32 / dt;
                    self.last_written = self.snap.written;
                    self.last_t = now;
                }
                // auto-finish: duration reached, window closed, or capture error
                if sess.captures_done() {
                    self.begin_stop();
                }
            }
        }
        if self.phase == Phase::Stopping {
            if let Some(rx) = &self.pending {
                if let Ok(done) = rx.try_recv() {
                    self.pending = None;
                    self.phase = Phase::Idle;
                    match done.result {
                        Ok(s) => {
                            self.done_msg = Some(format!(
                                "Saved {}  ({} frames, {} dropped)",
                                done.output, s.written, s.dropped
                            ));
                        }
                        Err(e) => self.error_msg = Some(format!("recording failed: {e}")),
                    }
                }
            }
        }
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        // Throttled repaints while busy; static otherwise (0% idle UI cost).
        if self.phase != Phase::Idle {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        let recording = self.phase != Phase::Idle;

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Shockwave");
                ui.label("Screen Recorder · WebM 1080p60");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (dot, txt) = match self.phase {
                        Phase::Idle => (egui::Color32::GRAY, "idle"),
                        Phase::Recording => (egui::Color32::RED, "● REC"),
                        Phase::Stopping => (egui::Color32::YELLOW, "stopping…"),
                    };
                    ui.colored_label(dot, txt);
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(4.0);
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.strong("1 · Source");
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui
                                    .add_enabled(!recording, egui::Button::new("Refresh"))
                                    .clicked()
                                {
                                    self.refresh_sources();
                                }
                            },
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(!recording, |ui| {
                            ui.radio_value(&mut self.use_window, false, "Monitor");
                            ui.radio_value(&mut self.use_window, true, "Window (game)");
                        });
                    });
                    if !self.use_window {
                        let label = self
                            .monitors
                            .get(self.monitor_sel)
                            .map(|m| format!("#{} {} ({}x{})", m.index, m.name, m.w, m.h))
                            .unwrap_or_else(|| "no monitors found".to_owned());
                        egui::ComboBox::from_id_salt("mon")
                            .selected_text(label)
                            .show_ui(ui, |ui| {
                                for (i, m) in self.monitors.iter().enumerate() {
                                    ui.selectable_value(
                                        &mut self.monitor_sel,
                                        i,
                                        format!("#{} {} ({}x{} @{}Hz)", m.index, m.name, m.w, m.h, m.hz),
                                    );
                                }
                            });
                    } else {
                        ui.horizontal(|ui| {
                            ui.label("Filter:");
                            ui.add_enabled(
                                !recording,
                                egui::TextEdit::singleline(&mut self.win_filter)
                                    .hint_text("e.g. Warships")
                                    .desired_width(220.0),
                            );
                        });
                        let filt = self.win_filter.to_lowercase();
                        egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                            let mut picked = None;
                            for (i, w) in self.windows.iter().enumerate() {
                                if !filt.is_empty()
                                    && !w.title.to_lowercase().contains(&filt)
                                    && !w.process.to_lowercase().contains(&filt)
                                {
                                    continue;
                                }
                                let label = format!("{}  [{}x{}] ({})", w.title, w.w, w.h, w.process);
                                if ui
                                    .add_enabled(
                                        !recording,
                                        egui::Button::selectable(
                                            self.win_sel == Some(i),
                                            label,
                                        ),
                                    )
                                    .clicked()
                                {
                                    picked = Some(i);
                                }
                            }
                            if let Some(i) = picked {
                                self.win_sel = Some(i);
                            }
                        });
                    }
                    if let Some(e) = &self.lists_error {
                        ui.colored_label(egui::Color32::YELLOW, e);
                    }
                });

                ui.group(|ui| {
                    ui.strong("2 · Where to save (D disk)");
                    ui.horizontal(|ui| {
                        ui.label("Folder:");
                        ui.add_enabled(
                            !recording,
                            egui::TextEdit::singleline(&mut self.dir).desired_width(300.0),
                        );
                        if ui.add_enabled(!recording, egui::Button::new("Browse…")).clicked() {
                            if let Some(p) = rfd::FileDialog::new()
                                .set_directory(&self.dir)
                                .pick_folder()
                            {
                                self.dir = p.to_string_lossy().into_owned();
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("File:");
                        ui.add_enabled(
                            !recording,
                            egui::TextEdit::singleline(&mut self.filename)
                                .hint_text("gameplay.webm")
                                .desired_width(300.0),
                        );
                    });
                    ui.weak("Never overwritten — existing names get _001, _002…");
                });

                ui.group(|ui| {
                    ui.strong("3 · Quality (for YouTube)");
                    ui.add_enabled_ui(!recording, |ui| {
                        if ui
                            .radio_value(&mut self.quality, Quality::Youtube, Quality::Youtube.label())
                            .clicked()
                        {
                            self.bitrate_sel = 3;
                        }
                        ui.weak("Sharp 1080p60 masters; YouTube's re-encode stays clean. ~35-55% CPU on your i5.");
                        if ui
                            .radio_value(&mut self.quality, Quality::Balanced, Quality::Balanced.label())
                            .clicked()
                        {
                            self.bitrate_sel = 0;
                        }
                        ui.weak("Lowest CPU (~15-25%). Fine for drafts.");
                    });
                    ui.horizontal(|ui| {
                        ui.label("Bitrate:");
                        egui::ComboBox::from_id_salt("br")
                            .selected_text(self.selected_bitrate())
                            .show_ui(ui, |ui| {
                                for (i, b) in BITRATES.iter().enumerate() {
                                    ui.selectable_value(&mut self.bitrate_sel, i, *b);
                                }
                            });
                        ui.label("FPS:");
                        ui.add_enabled_ui(!recording, |ui| {
                            ui.radio_value(&mut self.fps60, true, "60");
                            ui.radio_value(&mut self.fps60, false, "30");
                        });
                    });
                    ui.horizontal(|ui| {
                        ui.add_enabled(
                            !recording,
                            egui::Checkbox::new(&mut self.no_cursor, "Hide cursor"),
                        );
                        ui.add_enabled(
                            !recording,
                            egui::Checkbox::new(&mut self.border, "Capture border"),
                        );
                        ui.add_enabled(
                            !recording,
                            egui::Checkbox::new(&mut self.auto_stop, "Auto-stop"),
                        );
                        if self.auto_stop {
                            ui.add_enabled(
                                !recording,
                                egui::Slider::new(&mut self.stop_secs, 5..=600).suffix("s"),
                            );
                        }
                    });
                });

                ui.add_space(6.0);
                ui.vertical_centered(|ui| {
                    match self.phase {
                        Phase::Idle => {
                            let btn = egui::Button::new(
                                egui::RichText::new("●  Record").size(20.0).strong(),
                            )
                            .fill(egui::Color32::from_rgb(200, 40, 40))
                            .min_size(egui::vec2(220.0, 44.0));
                            if ui.add(btn).clicked() {
                                self.start();
                            }
                        }
                        Phase::Recording => {
                            let btn = egui::Button::new(
                                egui::RichText::new("■  Stop").size(20.0).strong(),
                            )
                            .min_size(egui::vec2(220.0, 44.0));
                            if ui.add(btn).clicked() {
                                self.begin_stop();
                            }
                            let el = self
                                .session
                                .as_ref()
                                .map(|s| s.elapsed().as_secs())
                                .unwrap_or(0);
                            ui.monospace(format!(
                                "{:02}:{:02}   {:.0} fps   captured {}   dropped {}",
                                el / 60,
                                el % 60,
                                self.out_fps,
                                self.snap.captured,
                                self.snap.dropped
                            ));
                            if self.snap.dropped > 0
                                && self.snap.dropped * 20 > self.snap.captured.max(1)
                            {
                                ui.colored_label(
                                    egui::Color32::YELLOW,
                                    "Dropping frames — switch to Balanced or 30 fps.",
                                );
                            }
                        }
                        Phase::Stopping => {
                            ui.spinner();
                            ui.label("Finalizing video…");
                        }
                    }
                });

                if let Some(m) = &self.done_msg {
                    ui.add_space(4.0);
                    ui.colored_label(egui::Color32::GREEN, m);
                }
                if let Some(e) = &self.error_msg {
                    ui.add_space(4.0);
                    ui.colored_label(egui::Color32::from_rgb(255, 120, 120), e);
                }

                ui.add_space(4.0);
                ui.separator();
                ui.weak("Tip: run games borderless-windowed — exclusive fullscreen can't be captured (same in OBS). No live preview by design: it would steal GPU from your game.");
            });
        });
    }
}

pub fn run() -> Result<()> {
    let icon =
        eframe::icon_data::from_png_bytes(&include_bytes!("../assets/icon-256.png")[..])
            .expect("assets/icon-256.png is corrupt");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([640.0, 760.0])
            .with_min_inner_size([560.0, 640.0])
            .with_icon(icon),
        ..Default::default()
    };
    eframe::run_native(
        "Shockwave Screen Recorder",
        options,
        Box::new(|cc| Ok(Box::new(GuiApp::new(cc)) as Box<dyn eframe::App>)),
    )
    .map_err(|e| anyhow::anyhow!("GUI failed: {e}"))
}
