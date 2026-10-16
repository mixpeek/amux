//! Embeds the dashboard SPA static files into the binary at compile time.
//!
//! The SPA lives in `static/` (`index.html`, `app.js`, `app.css`, `sw.js`),
//! extracted from the deleted Python server's inline copy (792ce1f has the
//! history).
//!
//! # Versioning
//!
//! The client version is DERIVED, never hand-bumped. `build.rs` hashes every
//! file under `static/` into [`ASSET_VER`]. `app.js` declares
//! `const APP_VER = '__AMUX_ASSET_VER__';` and `sw.js` declares
//! `const CACHE = 'amux-v__AMUX_ASSET_VER__';`, and [`asset`] substitutes the
//! placeholder when a file is read for serving. So the service-worker cache
//! name moves on every client-visible change, the SSE ping's `v` (api/sse.rs)
//! equals what every freshly loaded page holds, and the two files cannot drift.
//!
//! Serve and inspect dashboard bytes through [`asset`], not
//! `DashboardAssets::get`: the raw embed still carries the placeholder.
//!
//! # Serving from a directory (`AMUX_DASHBOARD_DIR`)
//!
//! When `AMUX_DASHBOARD_DIR` names a directory, every file is served from it
//! instead of the embed, and [`asset_version`] is that directory's hash,
//! computed at startup by the same recipe build.rs uses (`src/version.rs`,
//! compiled into both). CI uses it so a pull request that changes only
//! `static/` can run a server and test binaries main already compiled against
//! the PR's dashboard files. Production never sets it; the embed is the
//! default. An unreadable directory panics on first use rather than quietly
//! serving the embed, because a fallback there would test the wrong bytes.

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

use rust_embed::RustEmbed;

mod version;
pub use version::dir_asset_version;

#[derive(RustEmbed)]
#[folder = "static/"]
pub struct DashboardAssets;

/// Content hash of the EMBEDDED `static/`, computed by `build.rs`. What a
/// client is served is [`asset_version`], which differs only under an
/// `AMUX_DASHBOARD_DIR` override.
pub const ASSET_VER: &str = env!("AMUX_ASSET_VER");

/// The token in `app.js` / `sw.js` that [`asset`] replaces with [`asset_version`].
pub const ASSET_VER_PLACEHOLDER: &str = "__AMUX_ASSET_VER__";

/// The environment variable that points the server at a dashboard directory.
pub const DASHBOARD_DIR_ENV: &str = "AMUX_DASHBOARD_DIR";

/// An active `AMUX_DASHBOARD_DIR` override: the directory and its hash.
#[derive(Debug)]
pub struct DirOverride {
    pub dir: PathBuf,
    pub version: String,
}

/// The active override, resolved once per process. `None` (the default and
/// the production state) means the embedded copy is served.
pub fn dir_override() -> Option<&'static DirOverride> {
    static OVERRIDE: std::sync::OnceLock<Option<DirOverride>> = std::sync::OnceLock::new();
    OVERRIDE
        .get_or_init(|| {
            let raw = std::env::var_os(DASHBOARD_DIR_ENV).filter(|v| !v.is_empty())?;
            let dir = PathBuf::from(raw);
            let version = dir_asset_version(&dir).unwrap_or_else(|e| {
                panic!(
                    "{DASHBOARD_DIR_ENV}={} cannot be served ({e}); unset it to serve the embedded dashboard",
                    dir.display()
                )
            });
            Some(DirOverride { dir, version })
        })
        .as_ref()
}

/// The version clients are served: the override directory's hash when
/// `AMUX_DASHBOARD_DIR` is set, else [`ASSET_VER`].
pub fn asset_version() -> &'static str {
    dir_override().map_or(ASSET_VER, |o| o.version.as_str())
}

/// Text assets that may carry the placeholder. Binary files (fonts, wasm,
/// images) are passed through untouched without a scan.
fn may_carry_version(path: &str) -> bool {
    matches!(
        path.rsplit('.').next(),
        Some("js" | "mjs" | "html" | "css" | "json")
    )
}

/// Substitute [`ASSET_VER_PLACEHOLDER`] in `bytes`. Borrowed when absent.
pub fn stamp_version<'a>(bytes: Cow<'a, [u8]>) -> Cow<'a, [u8]> {
    let needle = ASSET_VER_PLACEHOLDER.as_bytes();
    if !bytes.windows(needle.len()).any(|w| w == needle) {
        return bytes;
    }
    let text = String::from_utf8_lossy(&bytes).replace(ASSET_VER_PLACEHOLDER, asset_version());
    Cow::Owned(text.into_bytes())
}

/// `path` resolved inside `dir`, refusing anything that could leave it
/// (`..`, an absolute path, a drive prefix). The embed cannot be escaped, so
/// neither may the directory that stands in for it.
fn file_in(dir: &Path, path: &str) -> Option<PathBuf> {
    let rel = Path::new(path);
    if path.is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    let full = dir.join(rel);
    full.is_file().then_some(full)
}

/// The unstamped bytes for `path` from the active source (the override
/// directory, else the embed). `None` when the file does not exist there.
pub fn raw_asset(path: &str) -> Option<Cow<'static, [u8]>> {
    match dir_override() {
        Some(o) => std::fs::read(file_in(&o.dir, path)?).ok().map(Cow::Owned),
        None => DashboardAssets::get(path).map(|f| f.data),
    }
}

/// The bytes a client receives for `path`: the file from the active source
/// with the asset version stamped in. `None` when the file does not exist.
///
/// The scan and substitution run once per path per process. Stamped copies
/// are leaked into a `'static` cache, which is bounded by the handful of files
/// that carry the placeholder (app.js and sw.js today). Under an override,
/// every file read is cached the same way (bounded by the directory's files;
/// misses are never cached), so the bytes cannot drift from the version
/// computed at startup.
pub fn asset(path: &str) -> Option<Cow<'static, [u8]>> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    /// path -> stamped bytes, or `None` when the file has no placeholder.
    type Stamped = Mutex<HashMap<String, Option<&'static [u8]>>>;
    static STAMPED: OnceLock<Stamped> = OnceLock::new();
    static FROM_DIR: OnceLock<Mutex<HashMap<String, &'static [u8]>>> = OnceLock::new();

    if let Some(o) = dir_override() {
        let cache = FROM_DIR.get_or_init(|| Mutex::new(HashMap::new()));
        let mut cache = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(bytes) = cache.get(path) {
            return Some(Cow::Borrowed(*bytes));
        }
        let bytes = std::fs::read(file_in(&o.dir, path)?).ok()?;
        let bytes = if may_carry_version(path) {
            stamp_version(Cow::Owned(bytes)).into_owned()
        } else {
            bytes
        };
        let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        cache.insert(path.to_string(), leaked);
        return Some(Cow::Borrowed(leaked));
    }

    let file = DashboardAssets::get(path)?;
    if !may_carry_version(path) {
        return Some(file.data);
    }
    let cache = STAMPED.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let entry = cache.entry(path.to_string()).or_insert_with(|| {
        match stamp_version(Cow::Borrowed(&file.data[..])) {
            Cow::Borrowed(_) => None,
            Cow::Owned(stamped) => Some(&*Box::leak(stamped.into_boxed_slice())),
        }
    });
    Some(match entry {
        Some(stamped) => Cow::Borrowed(*stamped),
        None => file.data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_a_hash_and_reaches_both_versioned_files() {
        // asset_version(), not ASSET_VER: under AMUX_DASHBOARD_DIR (CI runs
        // main's compiled tests against a PR's static/) the served version is
        // the directory's, and the two must still agree with each other.
        let ver = asset_version();
        assert_eq!(ver.len(), 12);
        assert!(ver.chars().all(|c| c.is_ascii_hexdigit()));
        let app = String::from_utf8(asset("app.js").unwrap().into_owned()).unwrap();
        let sw = String::from_utf8(asset("sw.js").unwrap().into_owned()).unwrap();
        assert!(!app.contains(ASSET_VER_PLACEHOLDER));
        assert!(!sw.contains(ASSET_VER_PLACEHOLDER));
        assert!(app.contains(&format!("const APP_VER = '{ver}';")));
        assert!(sw.contains(&format!("const CACHE = 'amux-v{ver}';")));
    }

    #[test]
    fn stamping_leaves_unversioned_bytes_borrowed() {
        let raw: Cow<'static, [u8]> = Cow::Borrowed(b"no token here");
        assert!(matches!(stamp_version(raw), Cow::Borrowed(_)));
        let owned = stamp_version(Cow::Borrowed(b"v=__AMUX_ASSET_VER__;"));
        assert_eq!(&*owned, format!("v={};", asset_version()).as_bytes());
    }

    #[test]
    fn a_dir_path_cannot_escape_the_directory() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("static");
        assert!(file_in(&dir, "app.js").is_some());
        assert!(file_in(&dir, "../Cargo.toml").is_none());
        assert!(file_in(&dir, "/etc/hosts").is_none());
        assert!(file_in(&dir, "").is_none());
        assert!(file_in(&dir, "state").is_none(), "a directory is not an asset");
    }

    #[test]
    fn the_embedded_version_is_the_hash_of_static_by_the_shared_recipe() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("static");
        if dir_override().is_none() {
            assert_eq!(dir_asset_version(&dir).unwrap(), ASSET_VER);
        }
    }
}
