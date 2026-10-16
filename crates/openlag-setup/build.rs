//! Packs the payload into the installer:
//! - `core.zip`   from `$OPENLAG_PAYLOAD/core`   (OpenLag.exe and licenses)
//! - `extras.zip` from `$OPENLAG_PAYLOAD/extras` (WinDivert, xray-core, WebView2 setup)
//!
//! and copies the background music to `ui/music.mp3`. Run `scripts/build-installer.ps1`
//! to stage the payload; without it the installer builds with empty archives.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let payload = std::env::var("OPENLAG_PAYLOAD")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest.join("../../target/dist/payload"));
    println!("cargo:rerun-if-env-changed=OPENLAG_PAYLOAD");
    println!("cargo:rerun-if-env-changed=OPENLAG_PAYLOAD_STAMP");
    for part in ["core", "extras"] {
        let dir = payload.join(part);
        println!("cargo:rerun-if-changed={}", dir.display());
        let size = pack(&dir, &out.join(format!("{part}.zip")));
        println!(
            "cargo:rustc-env=OPENLAG_{}_SIZE={size}",
            part.to_uppercase()
        );
        if size == 0 {
            println!(
                "cargo:warning=payload '{}' is empty: run scripts/build-installer.ps1",
                dir.display()
            );
        }
    }

    let music = std::env::var("OPENLAG_SETUP_MUSIC")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest.join("../../vendor/setup/music.mp3"));
    println!("cargo:rerun-if-env-changed=OPENLAG_SETUP_MUSIC");
    println!("cargo:rerun-if-changed={}", music.display());
    // The translator is shared with the app.
    let i18n = manifest.join("../openlag-gui/ui/i18n.js");
    println!("cargo:rerun-if-changed={}", i18n.display());
    std::fs::copy(&i18n, manifest.join("ui/i18n.js")).expect("copy i18n.js");

    let dest = manifest.join("ui/music.mp3");
    // OPENLAG_SETUP_MUSIC=none builds the public installer without a track (the UI
    // then plays music.mp3 found next to the exe, or hides the player).
    if std::env::var("OPENLAG_SETUP_MUSIC").as_deref() != Ok("none") && music.exists() {
        let _ = std::fs::copy(&music, &dest);
    } else {
        let _ = File::create(&dest);
    }

    let windows = tauri_build::WindowsAttributes::new().app_manifest(include_str!("app.manifest"));
    tauri_build::try_build(tauri_build::Attributes::new().windows_attributes(windows))
        .expect("tauri build failed");
}

/// Zips every file under `dir` (relative paths). Returns the uncompressed size.
fn pack(dir: &Path, zip_path: &Path) -> u64 {
    let mut zip = zip::ZipWriter::new(File::create(zip_path).unwrap());
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(9));
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let rel = p
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let mut data = Vec::new();
            File::open(&p).unwrap().read_to_end(&mut data).unwrap();
            total += data.len() as u64;
            zip.start_file(rel, opts).unwrap();
            zip.write_all(&data).unwrap();
        }
    }
    zip.finish().unwrap();
    total
}
