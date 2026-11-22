//! Make the brotli-precompressed assets embeddable.
//!
//! `scripts/build-wasm.sh` writes `<asset>.br` next to each served file, but
//! those files are gitignored (they are build products, and `rds-source.py`
//! tars everything under `rds/`), so a fresh checkout — or a container with no
//! `brotli` CLI — has none. `include_bytes!` cannot point at a file that does
//! not exist, so every `.br` is copied (or created empty) into `OUT_DIR` here
//! and embedded from there: an empty body means "no brotli variant", and the
//! gateway serves identity.

use std::fs;
use std::path::{Path, PathBuf};

/// Files the gateway serves from `assets/`, and therefore compresses.
const ASSETS: [&str; 5] = [
    "app.css",
    "login.js",
    "console.js",
    "uflinux_rds_web.js",
    "uflinux_rds_web_bg.wasm",
];

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let out_dir = std::env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .expect("OUT_DIR");
    for name in ASSETS {
        println!("cargo:rerun-if-changed=assets/{name}");
        println!("cargo:rerun-if-changed=assets/{name}.br");
        let compressed = Path::new("assets").join(format!("{name}.br"));
        let dest = out_dir.join(format!("{name}.br"));
        match fs::read(&compressed) {
            Ok(bytes) => {
                // A real brotli stream always has a payload; a zero-length
                // file is how we spell "not available".
                if bytes.is_empty() {
                    println!("cargo:warning={name}.br is empty, serving identity");
                }
                write(&dest, &bytes);
            }
            Err(_) => write(&dest, b""),
        }
    }
}

fn write(dest: &Path, bytes: &[u8]) {
    // Skip the write when the content is unchanged, so an unchanged tree does
    // not look stale on the next build.
    if fs::read(dest).is_ok_and(|existing| existing == bytes) {
        return;
    }
    if let Err(error) = fs::write(dest, bytes) {
        println!("cargo:warning=cannot write {}: {error}", dest.display());
    }
}
