//! Build script: pack crisp multi-size icons into the exe + version metadata.
//!
//! Single source of truth: `assets/crabby.png` (master crab logo).
//! High-quality `assets/icon-<N>.png` sizes are committed for the runtime
//! window icon; this script packs them all into one Vista-style PNG .ico so
//! Explorer (list/details/tiles), taskbar, Alt-Tab and shortcuts each get a
//! pixel-perfect size instead of a blurry stretch of one image.
//! Also embeds FileVersion/ProductVersion (from Cargo.toml) and an app
//! manifest (PerMonitorV2 DPI-awareness, UTF-8, Win10/11 compatibility) so
//! the exe looks right in file Properties and on HiDPI displays.

use std::{env, fs, path::PathBuf};

/// Full Windows icon set: small (list/details/taskbar), medium (tiles),
/// large (Alt-Tab, HiDPI). 256 is stored with byte 0 per ICO spec.
const SIZES: [u32; 9] = [16, 20, 24, 32, 40, 48, 64, 128, 256];

/// PerMonitorV2 DPI-aware, UTF-8, Win10/11-compatible manifest.
/// asInvoker: no UAC prompt (updater replaces the exe in user space).
/// NOTE: dpiAware/dpiAwareness values are case-sensitive — lowercase only,
/// or Windows refuses to start the exe (side-by-side error 14001).
const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity version="1.0.0.0" processorArchitecture="amd64"
      name="Crabby.ScreenRecorder" type="win32"/>
  <description>Crabby Screen Recorder</description>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">permonitorv2, system</dpiAwareness>
      <activeCodePage xmlns="http://schemas.microsoft.com/SMI/2019/WindowsSettings">UTF-8</activeCodePage>
    </windowsSettings>
  </application>
</assembly>
"#;

/// "1.2.3" -> 0x0001000200030000 (FILEVERSION/PRODUCTVERSION u64 layout).
fn version_u64(v: &str) -> u64 {
    let mut p = v.split('.').map(|s| {
        s.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u64>()
            .unwrap_or(0)
            .min(0xFFFF)
    });
    let (a, b, c, d) = (
        p.next().unwrap_or(0),
        p.next().unwrap_or(0),
        p.next().unwrap_or(0),
        p.next().unwrap_or(0),
    );
    (a << 48) | (b << 32) | (c << 16) | d
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Pack every committed size; fail loudly if the logo pipeline wasn't run.
    let mut pngs: Vec<Vec<u8>> = Vec::with_capacity(SIZES.len());
    for s in SIZES {
        let p = manifest.join(format!("assets/icon-{s}.png"));
        println!("cargo:rerun-if-changed={}", p.display());
        pngs.push(fs::read(&p).unwrap_or_else(|_| {
            panic!("assets/icon-{s}.png missing — regenerate from assets/crabby.png")
        }));
    }

    let mut ico: Vec<u8> = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]); // reserved, type = icon
    ico.extend_from_slice(&(SIZES.len() as u16).to_le_bytes()); // count
    let mut offset = 6 + 16 * SIZES.len();
    for (png, s) in pngs.iter().zip(SIZES) {
        let dim = if s >= 256 { 0u8 } else { s as u8 };
        ico.push(dim); // width (0 = 256)
        ico.push(dim); // height
        ico.push(0); // palette entries
        ico.push(0); // reserved
        ico.extend_from_slice(&1u16.to_le_bytes()); // color planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += png.len();
    }
    for png in &pngs {
        ico.extend_from_slice(png);
    }

    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("icon.ico");
    fs::write(&out, ico).expect("write icon.ico");

    println!(
        "cargo:rerun-if-changed={}",
        manifest.join("assets/crabby.png").display()
    );
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(target_os = "windows")]
    {
        let version =
            env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "1.0.0".to_owned());
        let vnum = version_u64(&version);
        let mut res = winresource::WindowsResource::new();
        res.set_icon(out.to_str().unwrap());
        res.set_manifest(MANIFEST);
        res.set_version_info(
            winresource::VersionInfo::FILEVERSION,
            vnum,
        );
        res.set_version_info(
            winresource::VersionInfo::PRODUCTVERSION,
            vnum,
        );
        res.set("CompanyName", "Crabby");
        res.set("FileDescription", "Crabby Screen Recorder - lightweight 1080p60 game recorder");
        res.set("ProductName", "Crabby Screen Recorder");
        res.set("InternalName", "crabby");
        res.set("OriginalFilename", "crabby.exe");
        res.set("LegalCopyright", "Crabby");
        res.set("ProductVersion", &version);
        res.set("FileVersion", &version);
        if let Err(e) = res.compile() {
            eprintln!("winresource failed (exe will lack icon/version): {e}");
        }
    }
}
