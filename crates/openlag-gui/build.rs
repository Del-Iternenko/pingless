use std::path::PathBuf;

fn main() {
    copy_runtime_files();
    tauri_build::build()
}

/// Puts xray and WinDivert next to the built exe so `cargo run` works like an install.
fn copy_runtime_files() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest.join("../../vendor");
    // OUT_DIR is target/<profile>/build/<pkg>/out
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let Some(profile_dir) = out.ancestors().nth(3) else {
        return;
    };
    for (src, name) in [
        ("xray/xray.exe", "xray.exe"),
        ("WinDivert-2.2.2-A/x64/WinDivert.dll", "WinDivert.dll"),
        ("WinDivert-2.2.2-A/x64/WinDivert64.sys", "WinDivert64.sys"),
    ] {
        let from = vendor.join(src);
        let to = profile_dir.join(name);
        if from.exists() && !to.exists() {
            let _ = std::fs::copy(&from, &to);
        }
    }
}
