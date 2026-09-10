use std::{env, fs, path::Path};

fn watch(path: &Path) {
    // Absolute paths keep a shared target's fingerprint tied to the checkout
    // whose assets were embedded. Track files as well as directory additions.
    println!("cargo:rerun-if-changed={}", path.display());
    if path.is_dir() {
        for entry in fs::read_dir(path).expect("dashboard asset directory").flatten() {
            watch(&entry.path());
        }
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_MANIFEST_DIR");
    let root = env::var("CARGO_MANIFEST_DIR").expect("dashboard manifest directory");
    watch(&Path::new(&root).join("static"));
    println!("cargo:rustc-env=AMUX_DASHBOARD_SOURCE_ROOT={root}");
}
