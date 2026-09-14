//! Hardware & capability detection.
//!
//! Everything downstream (encoder choice, thread default, scratch sizing,
//! tier defaults) queries [`Capabilities`] instead of assuming a specific
//! CPU/GPU/display. Detection runs once per process: parallel hardware
//! probes plus cheap OS queries, all failure-tolerant — absent hardware or
//! a headless CI box yields `false`/`None`, never a panic.

use std::sync::OnceLock;

/// Which H.264 encoders survived a real init probe.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HwH264 {
    pub qsv: bool,
    pub nvenc: bool,
    pub amf: bool,
}

/// HEVC hardware support. There is deliberately no software fallback:
/// software HEVC at 60 fps is unusable on typical hardware, so absence is
/// reported as an error with guidance instead of a silent slideshow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HwHevc {
    pub qsv: bool,
    pub nvenc: bool,
    pub amf: bool,
}

/// AV1 hardware support (opt-in codec only, same no-software-fallback rule).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HwAv1 {
    pub qsv: bool,
    pub nvenc: bool,
    pub amf: bool,
}

/// Primary display, when one can be enumerated (headless machines: `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayInfo {
    pub w: u32,
    pub h: u32,
    pub hz: u32,
}

/// Machine capabilities. Obtain via [`capabilities()`] (detected once) or
/// build literally in tests.
#[derive(Debug, Clone)]
pub struct Capabilities {
    pub h264: HwH264,
    pub hevc: HwHevc,
    pub av1: HwAv1,
    pub cpu_threads: u32,
    pub total_ram_mb: u64,
    pub primary_display: Option<DisplayInfo>,
}

/// Run one real 5-frame encode: listing an encoder proves nothing (NVENC is
/// listed with no NVIDIA card present) — only a successful init counts.
/// Missing ffmpeg binary, hung driver, anything: `false`, never a panic.
fn probe_encoder(enc: &str) -> bool {
    std::process::Command::new(ffmpeg_sidecar::paths::ffmpeg_path())
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "nullsrc=s=64x64:r=30:d=1",
            "-frames:v",
            "5",
            "-c:v",
            enc,
            "-f",
            "null",
            "-",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn cpu_threads() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
        .max(1)
}

fn total_ram_mb() -> u64 {
    // sysinfo 0.30 reports bytes.
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.total_memory() / 1_048_576
}

fn primary_display() -> Option<DisplayInfo> {
    use windows_capture::monitor::Monitor;
    let m = Monitor::primary().ok()?;
    let d = DisplayInfo {
        w: m.width().unwrap_or(0),
        h: m.height().unwrap_or(0),
        hz: m.refresh_rate().unwrap_or(0),
    };
    (d.w >= 16 && d.h >= 16).then_some(d)
}

impl Capabilities {
    /// Detect everything. The nine hardware probes run in three parallel
    /// groups so startup pays ~the slowest single probe, not the sum.
    /// Pure OS queries (threads/RAM/display) never fail the whole result.
    pub fn detect() -> Self {
        let (h264, hevc, av1) = std::thread::scope(|s| {
            let h264 = s.spawn(|| HwH264 {
                qsv: probe_encoder("h264_qsv"),
                nvenc: probe_encoder("h264_nvenc"),
                amf: probe_encoder("h264_amf"),
            });
            let hevc = s.spawn(|| HwHevc {
                qsv: probe_encoder("hevc_qsv"),
                nvenc: probe_encoder("hevc_nvenc"),
                amf: probe_encoder("hevc_amf"),
            });
            let av1 = s.spawn(|| HwAv1 {
                qsv: probe_encoder("av1_qsv"),
                nvenc: probe_encoder("av1_nvenc"),
                amf: probe_encoder("av1_amf"),
            });
            let h264 = h264.join().unwrap_or_default();
            let hevc = hevc.join().unwrap_or_default();
            let av1 = av1.join().unwrap_or_default();
            (h264, hevc, av1)
        });
        Self {
            h264,
            hevc,
            av1,
            cpu_threads: cpu_threads(),
            total_ram_mb: total_ram_mb(),
            primary_display: primary_display(),
        }
    }

    /// One-line summary for logs and bug reports.
    pub fn describe(&self) -> String {
        let hw = |name: &str, q: bool, n: bool, a: bool| {
            let mut v = Vec::new();
            if q {
                v.push("QSV");
            }
            if n {
                v.push("NVENC");
            }
            if a {
                v.push("AMF");
            }
            if v.is_empty() {
                format!("{name}[none]")
            } else {
                format!("{name}[{}]", v.join("+"))
            }
        };
        let disp = match self.primary_display {
            Some(d) => format!("{}x{}@{}Hz", d.w, d.h, d.hz),
            None => "no display".to_owned(),
        };
        format!(
            "caps: {} threads, {} MB RAM, display {disp}, {}, {}, {}",
            self.cpu_threads,
            self.total_ram_mb,
            hw("H.264", self.h264.qsv, self.h264.nvenc, self.h264.amf),
            hw("HEVC", self.hevc.qsv, self.hevc.nvenc, self.hevc.amf),
            hw("AV1", self.av1.qsv, self.av1.nvenc, self.av1.amf),
        )
    }
}

static CAPS: OnceLock<Capabilities> = OnceLock::new();

/// Process-wide capabilities, detected once on first use. Callers should
/// trigger this at startup (before recording) so the probe cost never lands
/// on a hot path.
pub fn capabilities() -> &'static Capabilities {
    CAPS.get_or_init(Capabilities::detect)
}

// ---------- fallback chain (pure: unit-testable without hardware) ----------

/// Pick an H.264 encoder: QSV → NVENC → AMF → libx264 software.
/// Infallible by construction — software is always the floor.
pub fn resolve_h264(hw: &HwH264) -> (&'static str, String) {
    if hw.qsv {
        ("h264_qsv", "Intel Quick Sync available".to_owned())
    } else if hw.nvenc {
        ("h264_nvenc", "no Quick Sync, NVIDIA NVENC available".to_owned())
    } else if hw.amf {
        ("h264_amf", "no Quick Sync/NVENC, AMD AMF available".to_owned())
    } else {
        ("libx264", "no hardware H.264, libx264 software fallback".to_owned())
    }
}

/// Pick an HEVC encoder. `Err` when no HEVC hardware exists — callers turn
/// this into guidance, not a panic. No software fallback on purpose.
pub fn resolve_hevc(hw: &HwHevc) -> Result<(&'static str, String), String> {
    if hw.qsv {
        Ok(("hevc_qsv", "Intel Quick Sync HEVC available".to_owned()))
    } else if hw.nvenc {
        Ok(("hevc_nvenc", "NVIDIA NVENC HEVC available".to_owned()))
    } else if hw.amf {
        Ok(("hevc_amf", "AMD AMF HEVC available".to_owned()))
    } else {
        Err("no HEVC hardware found (needs Intel 6th-gen+, NVIDIA Maxwell+, \
             or AMD Polaris+ with a current driver); use --codec h264 or vp9 instead"
            .to_owned())
    }
}

/// Pick an AV1 hardware encoder. Same no-software-fallback rule as HEVC:
/// software AV1 at 60 fps is a slideshow, so absence is an error.
pub fn resolve_av1(hw: &HwAv1) -> Result<(&'static str, String), String> {
    if hw.qsv {
        Ok(("av1_qsv", "Intel Quick Sync AV1 available".to_owned()))
    } else if hw.nvenc {
        Ok(("av1_nvenc", "NVIDIA NVENC AV1 available".to_owned()))
    } else if hw.amf {
        Ok(("av1_amf", "AMD AMF AV1 available".to_owned()))
    } else {
        Err("no AV1 hardware found (needs recent Intel/NVIDIA/AMD hardware \
             with a current driver); use --codec h264 or vp9 instead"
            .to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_prefers_qsv_then_nvenc_then_amf_then_software() {
        assert_eq!(resolve_h264(&HwH264 { qsv: true, nvenc: true, amf: true }).0, "h264_qsv");
        assert_eq!(resolve_h264(&HwH264 { qsv: false, nvenc: true, amf: true }).0, "h264_nvenc");
        assert_eq!(resolve_h264(&HwH264 { qsv: false, nvenc: false, amf: true }).0, "h264_amf");
        assert_eq!(
            resolve_h264(&HwH264 { qsv: false, nvenc: false, amf: false }).0,
            "libx264"
        );
    }

    #[test]
    fn h264_reason_always_non_empty() {
        for hw in [
            HwH264 { qsv: true, nvenc: false, amf: false },
            HwH264 { qsv: false, nvenc: true, amf: false },
            HwH264 { qsv: false, nvenc: false, amf: true },
            HwH264 { qsv: false, nvenc: false, amf: false },
        ] {
            assert!(!resolve_h264(&hw).1.is_empty());
        }
    }

    #[test]
    fn hevc_and_av1_fail_cleanly_without_hardware() {
        let hw = HwHevc::default();
        let err = resolve_hevc(&hw).unwrap_err();
        assert!(err.contains("h264"), "should suggest an alternative: {err}");
        let err = resolve_av1(&HwAv1::default()).unwrap_err();
        assert!(err.contains("h264"), "should suggest an alternative: {err}");
    }

    #[test]
    fn hevc_and_av1_prefer_qsv() {
        assert_eq!(
            resolve_hevc(&HwHevc { qsv: true, nvenc: true, amf: true }).unwrap().0,
            "hevc_qsv"
        );
        assert_eq!(
            resolve_av1(&HwAv1 { qsv: false, nvenc: true, amf: true }).unwrap().0,
            "av1_nvenc"
        );
    }

    #[test]
    fn detect_never_panics_and_reports_sane_basics() {
        // Must hold on any machine, including headless CI without ffmpeg.
        let c = Capabilities::detect();
        assert!(c.cpu_threads >= 1);
        // RAM may legitimately read 0 in a weird container; just don't panic.
        let _ = c.total_ram_mb;
        let _ = c.describe();
    }
}
