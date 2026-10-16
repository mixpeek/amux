use std::{env, fs, path::Path};

// The version recipe lives in src/version.rs and is compiled into the library
// too, so the hash of an AMUX_DASHBOARD_DIR override at runtime is computed by
// the same code as the embedded one (see `amux_dashboard::asset_version`).
include!("src/version.rs");

fn watch(path: &Path) {
    // Absolute paths keep a shared target's fingerprint tied to the checkout
    // whose assets were embedded. Track files as well as directory additions.
    println!("cargo:rerun-if-changed={}", path.display());
    if path.is_dir() {
        for entry in fs::read_dir(path)
            .expect("dashboard asset directory")
            .flatten()
        {
            watch(&entry.path());
        }
    }
}

/// The dashboard asset version: a hash of every file under `static/`, keyed by
/// its path relative to `static/` (sorted, `/`-separated) and its bytes.
///
/// app.js and sw.js carry `__AMUX_ASSET_VER__` where a hand-bumped literal used
/// to be, and the server substitutes this value when it serves them (see
/// `amux_dashboard::asset`). Every client-visible byte change therefore moves
/// the version, and the version can never disagree between the two files. The
/// literal used to make every pair of dashboard PRs conflict on the same line.
fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_MANIFEST_DIR");
    let root = env::var("CARGO_MANIFEST_DIR").expect("dashboard manifest directory");
    let static_dir = Path::new(&root).join("static");
    watch(&static_dir);
    println!("cargo:rerun-if-changed={}", Path::new(&root).join("src/version.rs").display());
    let version = dir_asset_version(&static_dir).expect("readable dashboard assets");
    println!("cargo:rustc-env=AMUX_ASSET_VER={version}");
    println!("cargo:rustc-env=AMUX_DASHBOARD_SOURCE_ROOT={root}");
}
