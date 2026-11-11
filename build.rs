use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// The repository stores the icon assets as Base64 text (`assets/*.base64`)
/// so the whole tree stays text-only and cloneable anywhere. Locally the raw
/// files may also exist; either way this materializes real files into
/// OUT_DIR for `include_bytes!` (icon.png) and winres (icon.ico).
fn materialize(out_dir: &Path, name: &str) -> PathBuf {
    let dest = out_dir.join(name);
    let raw = Path::new("assets").join(name);
    if raw.exists() {
        fs::copy(&raw, &dest).expect("copy icon asset");
        return dest;
    }
    let b64_path = Path::new("assets").join(format!("{name}.base64"));
    let b64 = fs::read_to_string(&b64_path).expect("icon asset (.base64) missing");
    let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .expect("invalid base64 in icon asset");
    fs::write(&dest, bytes).expect("write decoded icon asset");
    dest
}

fn main() {
    println!("cargo:rerun-if-changed=assets");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let _icon_png = materialize(&out_dir, "icon.png");
    let icon_ico = materialize(&out_dir, "icon.ico");

    // NOTE: build scripts run on the HOST, so do not gate this on
    // cfg(target_os) — winres itself checks the TARGET being built.
    // When cross-building without a resource compiler it returns Err,
    // which we deliberately downgrade to a warning.
    if env::var("TARGET").map(|t| t.contains("windows")).unwrap_or(false) {
        let mut res = winres::WindowsResource::new();
        res.set_icon(icon_ico.to_str().expect("icon path utf8"));
        res.set("ProductName", "Muse");
        res.set("FileDescription", "Muse for Windows (Rust + WebView2) - by Anko");
        res.set("LegalCopyright", "Unofficial wrapper. Muse is a service of Meta.");
        if let Err(e) = res.compile() {
            println!("cargo:warning=winres failed (icon will be default): {e}");
        }
    }
}
