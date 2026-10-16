// The dashboard asset version recipe, shared by build.rs (`include!`) and the
// library (`mod version`), so the compile-time hash and the runtime hash of an
// `AMUX_DASHBOARD_DIR` override cannot drift apart. std only: build.rs has no
// dependencies. No inner doc comments here, because `include!` rejects them.

/// Every shipped file under `root`, recursively. Dotfiles (.DS_Store and
/// friends) are local noise, never shipped content, so they must not move the
/// version between machines.
pub fn asset_files(root: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out)?;
            } else if !entry.file_name().to_string_lossy().starts_with('.') {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, &mut out)?;
    Ok(out)
}

/// FNV-1a, 64 bit. Dependency-free and stable across Rust releases, which
/// `std::hash` is not. Collision resistance against an adversary is not a
/// requirement: the value only has to change when the shipped bytes do.
fn fnv1a(hash: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *hash ^= u64::from(*b);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

/// The dashboard asset version: a hash of every file in `files`, keyed by its
/// path relative to `root` (sorted, `/`-separated) and its bytes.
pub fn asset_version(root: &std::path::Path, files: &[std::path::PathBuf]) -> std::io::Result<String> {
    let mut rel: Vec<(String, &std::path::PathBuf)> = Vec::with_capacity(files.len());
    for p in files {
        let r = p
            .strip_prefix(root)
            .map_err(|_| std::io::Error::other(format!("{} is not under {}", p.display(), root.display())))?
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        rel.push((r, p));
    }
    rel.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (name, path) in rel {
        fnv1a(&mut hash, name.as_bytes());
        fnv1a(&mut hash, &[0]);
        let bytes = std::fs::read(path)?;
        fnv1a(&mut hash, &(bytes.len() as u64).to_le_bytes());
        fnv1a(&mut hash, &bytes);
    }
    Ok(format!("{hash:016x}")[..12].to_string())
}

/// [`asset_version`] over every shipped file under `root`.
pub fn dir_asset_version(root: &std::path::Path) -> std::io::Result<String> {
    asset_version(root, &asset_files(root)?)
}
