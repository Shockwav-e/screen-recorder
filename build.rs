//! Build script: pack the PNG logo sizes into a multi-image .ico and embed it
//! (plus version metadata) into the Windows exe. Pure std — no extra deps.

use std::{env, fs, path::PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Vista-style ICO: PNG payloads at 16/32/48/256 px (0 byte = 256).
    let sizes: [u32; 4] = [16, 32, 48, 256];
    let mut pngs: Vec<Vec<u8>> = Vec::new();
    for s in sizes {
        let p = manifest.join(format!("assets/icon-{s}.png"));
        println!("cargo:rerun-if-changed={}", p.display());
        pngs.push(fs::read(&p).expect("assets/icon-N.png missing — logo not converted"));
    }

    let mut ico: Vec<u8> = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]); // reserved, type = icon
    ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes()); // count
    let mut offset = 6 + 16 * sizes.len();
    for (png, s) in pngs.iter().zip(sizes) {
        let dim = if s >= 256 { 0u8 } else { s as u8 };
        ico.push(dim); // width
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

    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(out.to_str().unwrap());
        res.set("ProductName", "Shockwave Screen Recorder");
        res.set(
            "FileDescription",
            "Shockwave Screen Recorder - lightweight WebM 1080p60 game recorder",
        );
        res.set("CompanyName", "Shockwave");
        res.set("OriginalFilename", "shockwave-rec.exe");
        if let Err(e) = res.compile() {
            eprintln!("winresource failed (exe will lack embedded icon): {e}");
        }
    }
}
