//! Native Chrome profile management + launch (RR-0092; plan lesson L7).
//!
//! Profile locations are the PYTHON server's, not a new scheme — L7's whole
//! lesson is that create-path and use-path must be the same bytes, and the
//! existing logged-in profiles live where `_bu_profile_dir` in amux-server.py
//! resolves them (NOT under `~/.amux/browser-profiles/`, which nothing ever
//! used — verified by grep before writing this):
//! - `default` (or empty)  -> `<amux_home>/playwright-auth/profile`
//! - named, legacy         -> `<amux_home>/playwright-auth/profiles/<name>`
//!   when that directory exists (the 35+ real logged-in profiles are here)
//! - named, otherwise      -> imported on first use from
//!   `<chrome-user-data-dir>/<name>` into the amux-owned location above
//!
//! NEW profiles are created under `playwright-auth/profiles/<name>` — the
//! amux-owned location — because this launcher passes `--user-data-dir`
//! straight at the profile dir. (Python's create targets the shared Chrome
//! dir for browser-use interop; the native launcher IS the consumer here, so
//! amux-owned is the location whose create-path == use-path.) Existing
//! Chrome-dir profiles are never launched in place. Chrome locks its ENTIRE
//! user-data-dir even when a different `--profile-directory` is selected, so
//! doing that made automation require quitting every ordinary Chrome window.
//! First use now takes an atomic, cache-free snapshot into an isolated amux
//! user-data-dir and launches the snapshot. The user's browser can stay open,
//! and its profile is never driven or mutated by amux.
//!
//! Lock-file hygiene: Chrome drops `SingletonLock`/`SingletonCookie`/
//! `SingletonSocket` on a clean exit and leaves them when killed; a stale set
//! blocks the next launch (AMUX-2070). We clean them on demand and at startup
//! reconcile — but ONLY inside amux-owned dirs (`playwright-auth/...`). The
//! real Chrome user-data-dir is the human's live browser; deleting its locks
//! while it runs would corrupt their session, so it is never touched.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

/// Chrome removes these on clean exit; their presence after exit means the
/// profile was never flushed (Python `_CHROME_SINGLETONS`).
pub const CHROME_SINGLETONS: [&str; 3] = ["SingletonLock", "SingletonCookie", "SingletonSocket"];

pub use crate::config::amux_home;

/// The user's real Chrome user-data-dir (Python `_chrome_user_data_dir`).
pub fn chrome_user_data_dir() -> PathBuf {
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/"));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Google/Chrome")
    } else if cfg!(windows) {
        std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or(home)
            .join("Google/Chrome/User Data")
    } else {
        home.join(".config/google-chrome")
    }
}

/// Chrome user profiles from `Local State` → `profile.info_cache`.
/// Returns directory names ("Default", "Profile 11", …) sorted.
pub fn list_chrome_profiles() -> Vec<String> {
    let local_state = chrome_user_data_dir().join("Local State");
    let data = match std::fs::read_to_string(&local_state) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let parsed: serde_json::Value = match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let Some(cache) = parsed
        .get("profile")
        .and_then(|p| p.get("info_cache"))
        .and_then(|c| c.as_object())
    else {
        return vec![];
    };
    let mut names: Vec<String> = cache.keys().cloned().collect();
    names.sort();
    names
}

/// Python `_bu_profile_dir`: where a profile name lives on disk. Pure in its
/// inputs so tests can drive it hermetically.
pub fn resolve_profile_dir(home: &Path, chrome_dir: &Path, name: &str) -> PathBuf {
    let n = name.trim();
    if n.is_empty() || n == "default" {
        return home.join("playwright-auth").join("profile");
    }
    let legacy = home.join("playwright-auth").join("profiles").join(n);
    if legacy.is_dir() {
        return legacy;
    }
    chrome_dir.join(n)
}

/// How to hand a resolved profile dir to Chrome (Python
/// `_bu_profile_launch_target`): a dir nested in the Chrome user-data-dir is
/// a `--profile-directory`, an amux-owned dir IS the `--user-data-dir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchTarget {
    pub user_data_dir: PathBuf,
    pub profile_directory: Option<String>,
}

pub fn launch_target(home: &Path, chrome_dir: &Path, name: &str) -> LaunchTarget {
    let d = resolve_profile_dir(home, chrome_dir, name);
    if d.parent() == Some(chrome_dir) {
        LaunchTarget {
            user_data_dir: chrome_dir.to_path_buf(),
            profile_directory: d.file_name().map(|s| s.to_string_lossy().into_owned()),
        }
    } else {
        LaunchTarget { user_data_dir: d, profile_directory: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedChromeProfile {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub files: u64,
    pub bytes: u64,
}

/// Turn an io error from the Chrome-profile import into something a person can
/// act on (AF-502).
///
/// Every failure in this path used to surface as a bare `std::io::Error`: no
/// paths, no statement of what was being attempted, no remedy. Measured in a
/// live onboarding session (2026-09-04) — a user clicked "auto profile / import
/// on first use", got a one-line error, and neither they NOR the person walking
/// them through it could read it. The session's own words were "Error Chrome
/// for Vanos rename" and then "What just happened?"
///
/// The errno is the discriminator and it is the part nobody looks up.
/// PermissionDenied here is almost always macOS TCC rather than file modes:
/// Chrome's user-data-dir lives under ~/Library/Application Support, which is
/// protected, so the server needs Full Disk Access to read it. That is a
/// two-click fix and it is invisible from the error alone.
///
/// Pure so the wording is testable without a filesystem.
pub fn import_failure_message(
    step: &str,
    from: &Path,
    to: &Path,
    kind: std::io::ErrorKind,
    raw: &str,
) -> String {
    let cause = match kind {
        std::io::ErrorKind::PermissionDenied => {
            "macOS is almost certainly refusing the read rather than the file modes being \
             wrong: Chrome's profile lives under ~/Library/Application Support, which is \
             TCC-protected. Give the amux server Full Disk Access — System Settings > \
             Privacy & Security > Full Disk Access — then retry. Nothing needs to be \
             deleted or recreated."
        }
        std::io::ErrorKind::NotFound => {
            "the source profile is not where Chrome usually keeps it. Check the profile \
             name against the directories in your Chrome user-data-dir; a display name in \
             Chrome's profile menu is NOT the directory name (`Profile 3`, `Default`)."
        }
        std::io::ErrorKind::AlreadyExists => {
            "something already occupies the destination. amux treats a complete \
             destination as a concurrent importer winning the race, so reaching here means \
             the destination exists but is not a usable directory. Move it aside and retry."
        }
        std::io::ErrorKind::StorageFull => {
            "the disk is full. A profile import copies the whole profile, which is \
             routinely hundreds of megabytes."
        }
        _ if raw.contains("Cross-device") || raw.contains("os error 18") => {
            "the staging directory and the destination are on different filesystems, so \
             the atomic rename cannot work. This happens when ~/.amux is a symlink onto \
             another volume."
        }
        _ => {
            "no known-cause mapping for this errno — the raw error is the whole of what \
             amux knows, and that is worth reporting."
        }
    };
    format!(
        "Chrome profile import failed during {step}.\n  from: {}\n  to:   {}\n  error: {raw}\n\n\
         Most likely: {cause}",
        from.display(),
        to.display()
    )
}

/// Import a human Chrome profile into an isolated amux user-data-dir.
///
/// Chrome's profile lock belongs to the whole user-data-dir, not the selected
/// profile directory. Launching `--user-data-dir=<the human Chrome dir>` thus
/// delegates to an already-running Chrome and exits before CDP binds. More
/// importantly, deleting that lock to force the launch would risk the user's
/// browser data. A point-in-time copy is the safe boundary: workers get the
/// login state they selected while ordinary Chrome remains independent.
///
/// The copy is assembled under a unique sibling and renamed into place, so a
/// crash never exposes a half-profile as runnable. Volatile caches and lock
/// files are omitted; Chrome recreates them. Symlinks are skipped so a profile
/// cannot make the importer escape either tree.
pub fn import_chrome_profile(
    home: &Path,
    chrome_dir: &Path,
    name: &str,
) -> anyhow::Result<Option<ImportedChromeProfile>> {
    let name = name.trim();
    if name.is_empty() || name == "default" {
        return Ok(None);
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
        anyhow::bail!("Chrome profile name must be [A-Za-z0-9._-]+");
    }
    let destination = home.join("playwright-auth").join("profiles").join(name);
    if destination.is_dir() {
        return Ok(None);
    }
    let source = chrome_dir.join(name);
    if !source.is_dir() {
        anyhow::bail!(
            "Chrome profile {name:?} does not exist at {}; create an amux profile with POST \
             /api/browser/profile/create instead",
            source.display()
        );
    }

    let parent = destination
        .parent()
        .ok_or_else(|| anyhow::anyhow!("browser profile destination has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp = parent.join(format!(".import-{name}-{}-{stamp}", std::process::id()));
    let copied = (|| -> anyhow::Result<(u64, u64)> {
        std::fs::create_dir(&temp)?;
        let profile_root = temp.join("Default");
        let mut stats = (0u64, 0u64);
        copy_profile_tree(&source, &profile_root, &mut stats)?;
        copy_import_local_state(chrome_dir, name, &temp)?;
        Ok(stats)
    })();
    let (files, bytes) = match copied {
        Ok(v) => v,
        Err(e) => {
            // AF-502: the COPY is where a fresh Mac actually fails, and it
            // failed with a bare io error. Chrome's user-data-dir is under
            // ~/Library/Application Support, which is TCC-protected, so reading
            // it without Full Disk Access is a PermissionDenied that says
            // nothing about permissions in the sense a person can act on.
            let io_kind = e.downcast_ref::<std::io::Error>().map(std::io::Error::kind);
            let raw = format!("{e:#}");
            let _ = std::fs::remove_dir_all(&temp);
            tracing::warn!(
                profile = %name, step = "copy", kind = ?io_kind, error = %raw,
                source = %source.display(), temp = %temp.display(),
                "browser: Chrome profile import failed (AF-502)"
            );
            let Some(kind) = io_kind else {
                // Not an io error at all (a bad Local State JSON, say). Keep it
                // as it is rather than dressing it in a filesystem explanation
                // that would send the reader at the wrong thing.
                return Err(e.context(format!(
                    "Chrome profile import failed while copying {} into place",
                    source.display()
                )));
            };
            return Err(anyhow::anyhow!(import_failure_message(
                "the copy of the source profile", &source, &temp, kind, &raw
            )));
        }
    };
    match std::fs::rename(&temp, &destination) {
        Ok(()) => {}
        // A concurrent importer won the race. Its atomic destination is valid;
        // discard our complete duplicate and let this start use the winner.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && destination.is_dir() => {
            let _ = std::fs::remove_dir_all(&temp);
            return Ok(None);
        }
        Err(e) => {
            let kind = e.kind();
            let raw = e.to_string();
            let _ = std::fs::remove_dir_all(&temp);
            tracing::warn!(
                profile = %name, step = "rename", kind = ?kind, error = %raw,
                temp = %temp.display(), destination = %destination.display(),
                "browser: Chrome profile import failed (AF-502)"
            );
            return Err(anyhow::anyhow!(import_failure_message(
                "the final rename into place", &temp, &destination, kind, &raw
            )));
        }
    }
    // This is selected login state, not disposable scratch. Registering it
    // exempts the isolated copy from the profile TTL reaper. A failed registry
    // write rolls back only the destination we just created; the human source
    // remains untouched throughout.
    if let Err(e) = registry_register(home, name, "", "") {
        let _ = std::fs::remove_dir_all(&destination);
        return Err(e.context("register imported Chrome profile"));
    }
    Ok(Some(ImportedChromeProfile { source, destination, files, bytes }))
}

fn copy_profile_tree(src: &Path, dst: &Path, stats: &mut (u64, u64)) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = match entry {
            Ok(v) => v,
            // A live Chrome can rotate a transient entry between read_dir and
            // inspection. A vanished cache is not an import failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let name = entry.file_name();
        let label = name.to_string_lossy();
        if import_entry_is_volatile(&label) {
            continue;
        }
        let ty = match entry.file_type() {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let to = dst.join(&name);
        if ty.is_symlink() {
            continue;
        }
        if ty.is_dir() {
            copy_profile_tree(&entry.path(), &to, stats)?;
        } else if ty.is_file() {
            match std::fs::copy(entry.path(), &to) {
                Ok(n) => {
                    stats.0 += 1;
                    stats.1 += n;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

fn import_entry_is_volatile(name: &str) -> bool {
    name == "LOCK"
        || name.starts_with("Singleton")
        || matches!(
            name,
            "Cache"
                | "Code Cache"
                | "GPUCache"
                | "DawnCache"
                | "GrShaderCache"
                | "GraphiteDawnCache"
                | "ShaderCache"
                | "Crashpad"
        )
}

fn copy_import_local_state(chrome_dir: &Path, source_name: &str, dst: &Path) -> anyhow::Result<()> {
    let path = chrome_dir.join("Local State");
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    let mut value: serde_json::Value = serde_json::from_str(&raw)?;
    if let Some(profile) = value.get_mut("profile").and_then(serde_json::Value::as_object_mut) {
        let source_info = profile
            .get("info_cache")
            .and_then(serde_json::Value::as_object)
            .and_then(|m| m.get(source_name))
            .cloned()
            .unwrap_or_else(|| json!({ "name": source_name }));
        profile.insert("last_used".into(), json!("Default"));
        profile.insert("last_active_profiles".into(), json!(["Default"]));
        profile.insert("info_cache".into(), json!({ "Default": source_info }));
    }
    std::fs::write(dst.join("Local State"), serde_json::to_vec(&value)?)?;
    Ok(())
}

/// Is this dir one amux owns outright (safe to create, clean locks in,
/// delete)? Anything else — notably the real Chrome user-data-dir — is the
/// human's and is never mutated beyond what Chrome itself does.
pub fn is_amux_owned(home: &Path, dir: &Path) -> bool {
    dir.starts_with(home.join("playwright-auth"))
}

// ---- inventory ------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct BrowserProfile {
    pub name: String,
    pub path: String,
    /// Unix seconds from the profile dir's mtime — Chrome touches the dir on
    /// use, so this is "when was this profile last opened", cheaply.
    pub last_used: Option<i64>,
    /// Only when the caller asked (`?sizes=1`): walking a 565MB profile took
    /// 2.9s in the Python listing and starved the picker, so sizes stay
    /// opt-in here too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_mb: Option<f64>,
    /// Registry metadata (`playwright-auth/profiles.json`, shared with the
    /// Python server): which domains this profile is signed into.
    pub domains: Vec<String>,
    pub label: String,
    pub registered: bool,
}

fn dir_mtime_unix(p: &Path) -> Option<i64> {
    let modified = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    )
}

/// Recursive on-disk size without a walkdir dep (profiles are a few hundred
/// files; the Python listing does the same walk).
pub fn dir_size_bytes(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

/// Registry metadata (Python `_bu_registry_load`): name -> {domains, label}.
fn registry_load(home: &Path) -> serde_json::Map<String, serde_json::Value> {
    let path = home.join("playwright-auth").join("profiles.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// Inventory of amux-owned profiles: a directory that exists IS a profile
/// (the Python listing's hard-won rule — the registry adds metadata, it is
/// not the source of truth for existence).
pub fn list_profiles(home: &Path, with_sizes: bool) -> Vec<BrowserProfile> {
    let registry = registry_load(home);
    let mut names: std::collections::BTreeSet<String> =
        registry.keys().cloned().collect();
    let profiles_dir = home.join("playwright-auth").join("profiles");
    if let Ok(entries) = std::fs::read_dir(&profiles_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.path().is_dir() && !name.starts_with('.') {
                names.insert(name);
            }
        }
    }
    if home.join("playwright-auth").join("profile").is_dir() {
        names.insert("default".into());
    }

    let chrome_dir = chrome_user_data_dir();
    names
        .into_iter()
        .map(|name| {
            let dir = resolve_profile_dir(home, &chrome_dir, &name);
            let meta = registry.get(&name).and_then(|v| v.as_object());
            BrowserProfile {
                path: dir.display().to_string(),
                last_used: dir_mtime_unix(&dir),
                // 3-decimal precision: a 2KB profile must not round to 0.0
                // (a nonzero directory reading as empty is a lie; display
                // rounding is the UI's call).
                size_mb: with_sizes
                    .then(|| (dir_size_bytes(&dir) as f64 / (1024.0 * 1024.0) * 1000.0).round() / 1000.0),
                domains: meta
                    .and_then(|m| m.get("domains"))
                    .and_then(|d| d.as_array())
                    .map(|a| {
                        a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()
                    })
                    .unwrap_or_default(),
                label: meta
                    .and_then(|m| m.get("label"))
                    .and_then(|l| l.as_str())
                    .unwrap_or("")
                    .to_string(),
                registered: meta.is_some(),
                name,
            }
        })
        .collect()
}

// ---- lock files -----------------------------------------------------------

/// Which Singleton* entries are present. `symlink_metadata` (lstat), not
/// `exists()`: the entries are symlinks pointing at `<host>-<pid>`, and
/// `exists()` follows the link — a live lock with an unresolvable target
/// would read as absent exactly when the check matters (Python
/// `_chrome_locks_present` carries the same comment).
pub fn locks_present(dir: &Path) -> Vec<String> {
    CHROME_SINGLETONS
        .iter()
        .filter(|n| dir.join(n).symlink_metadata().is_ok())
        .map(|n| n.to_string())
        .collect()
}

/// Remove Singleton* entries from ONE dir. Callers are responsible for only
/// pointing this at amux-owned dirs whose Chrome is known dead.
pub fn clean_locks(dir: &Path) -> Vec<String> {
    let mut removed = Vec::new();
    for n in CHROME_SINGLETONS {
        let p = dir.join(n);
        if p.symlink_metadata().is_ok() && std::fs::remove_file(&p).is_ok() {
            removed.push(n.to_string());
        }
    }
    removed
}

/// PID Chrome records in the `SingletonLock` symlink (`<host>-<pid>`).
///
/// Chrome places the lock at the USER-DATA-DIR level, not inside the selected
/// `--profile-directory`. That distinction matters for profiles from the
/// human's normal Chrome: launching `Profile 7` while `Default` is open still
/// delegates the URL to the already-running Chrome process. Parsing from the
/// right keeps hostnames containing dashes valid.
fn singleton_lock_owner_pid(dir: &Path) -> Option<u32> {
    let target = std::fs::read_link(dir.join("SingletonLock")).ok()?;
    let target = target.to_string_lossy();
    let (_, pid) = target.rsplit_once('-')?;
    let pid = pid.parse::<u32>().ok()?;
    (pid != 0).then_some(pid)
}

/// A live Chrome outside amux already owns this human user-data-dir.
///
/// Never infer this from lock PRESENCE alone: Chrome can leave stale locks
/// after a crash. The PID recorded in `SingletonLock` must still be alive, and
/// it must not be one of amux's registered browsers (a same-profile restart is
/// allowed to reach the normal stop-and-replace path below).
fn unmanaged_live_lock_owner(home: &Path, dir: &Path) -> Option<u32> {
    if is_amux_owned(home, dir) {
        return None;
    }
    let pid = singleton_lock_owner_pid(dir)?;
    if !pid_alive(pid) {
        return None;
    }
    let managed = RUNNING
        .lock()
        .expect("browser registry poisoned")
        .values()
        .any(|r| r.pid == pid);
    (!managed).then_some(pid)
}

/// Startup reconcile: at boot no amux-launched Chrome exists, so any lock in
/// an amux-owned profile dir is stale by definition and blocks the next
/// launch. Returns (dir, removed-locks) pairs so the caller can log WHAT was
/// cleaned, not just that cleaning happened (ethos rule 4).
pub fn reconcile_locks_at_startup(home: &Path) -> Vec<(PathBuf, Vec<String>)> {
    let mut dirs = vec![home.join("playwright-auth").join("profile")];
    if let Ok(entries) = std::fs::read_dir(home.join("playwright-auth").join("profiles")) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                dirs.push(e.path());
            }
        }
    }
    let mut cleaned = Vec::new();
    for dir in dirs {
        debug_assert!(is_amux_owned(home, &dir));
        let removed = clean_locks(&dir);
        if !removed.is_empty() {
            cleaned.push((dir, removed));
        }
    }
    cleaned
}

// ---- Chrome launch + CDP over HTTP ----------------------------------------

/// The one browser this server has running (the dashboard's start/stop model
/// is a single session). Starting while one runs stops the old one first.
pub struct RunningBrowser {
    pub profile: String,
    pub user_data_dir: PathBuf,
    pub cdp_port: u16,
    pub started_at: i64,
    /// Unix timestamp of the last navigate/screenshot/action verb. Stamped by
    /// `touch_verb` on every driver call so the reaper can detect stale browsers
    /// that have pages open but nobody driving them (2026-08-30).
    pub last_verb_at: i64,
    /// Always known. `child` is None for a browser ADOPTED after a server
    /// restart — we did not spawn it, so we have no handle, but we can still
    /// speak CDP to it and still kill it by pid.
    pub pid: u32,
    /// The session that started this browser (AMUX-3063). "" for a browser
    /// adopted from a pre-fix running-file. The one browser is a machine
    /// singleton, so an anonymous `start` STOMPS whoever staged it — measured
    /// 2026-08-20 09:24:57: an unattributed default-profile start killed
    /// amux-gtm's staged NetSuite login mid-handoff. The API layer refuses a
    /// cross-session start without an explicit takeover flag using this field.
    pub started_by: String,
    child: Option<tokio::process::Child>,
}

/// The one running browser's ownership snapshot: (profile, started_by,
/// started_at, pid, cdp_port). What the start handler's takeover guard reads.
///
/// `cdp_port` joined the tuple for AMUX-3610. The takeover refusal has to be
/// able to say "0 tabs open for 18.5 hours", and it cannot ask without the port.
/// A caller told only "someone else holds this" must choose between blocking
/// forever and destroying possibly-live state, which is a shared single-instance
/// resource whose only escape hatch is destructive.
/// One profile's browser, if it is running.
pub fn running_snapshot_for(profile: &str) -> Option<(String, String, i64, u32, u16)> {
    RUNNING
        .lock()
        .expect("browser registry poisoned")
        .get(profile)
        .map(|r| (r.profile.clone(), r.started_by.clone(), r.started_at, r.pid, r.cdp_port))
}

/// Every running browser, newest first. The status verb renders all of them;
/// nothing may assume a single one (AMUX-3828).
/// Tuple: (profile, started_by, started_at, pid, cdp_port, last_verb_at)
pub fn running_all() -> Vec<(String, String, i64, u32, u16, i64)> {
    let mut v: Vec<_> = RUNNING
        .lock()
        .expect("browser registry poisoned")
        .values()
        .map(|r| (r.profile.clone(), r.started_by.clone(), r.started_at, r.pid, r.cdp_port, r.last_verb_at))
        .collect();
    v.sort_by_key(|r| std::cmp::Reverse(r.2));
    v
}

/// Stamp the current time as the last verb on a profile. Called by every
/// driver verb (navigate, screenshot, action, state) so the reaper can tell
/// "browser with open page but nobody driving it" from "browser in active use".
pub fn touch_verb(profile: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    if let Ok(mut g) = RUNNING.lock() {
        if let Some(b) = g.get_mut(profile) {
            b.last_verb_at = now;
        }
    }
}

/// Like `touch_verb` but looks up the browser by `started_by` == session.
/// Mirrors the `port_for_session` fallback: if no owner matches but exactly
/// one browser is running, that browser gets the stamp (it is the one being
/// driven). Called from API verb handlers where we have the session name.
pub fn touch_verb_for_session(session: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    if let Ok(mut g) = RUNNING.lock() {
        // Prefer the browser owned by this session.
        if !session.is_empty() {
            if let Some(b) = g.values_mut().find(|b| b.started_by == session) {
                b.last_verb_at = now;
                return;
            }
        }
        // Single-browser fallback: mirrors port_for_session.
        if g.len() == 1 {
            if let Some(b) = g.values_mut().next() {
                b.last_verb_at = now;
            }
        }
    }
}


/// Why the last browser is gone (AMUX-3414: a silent Chrome exit left NOTHING
/// on record — no API call in the window, no exit reason anywhere, and two
/// sessions spent a morning on hypotheses the log could have settled).
/// `reason` is "stopped" (an explicit /stop, with `by` naming the actor) or
/// "exited" (Chrome went away on its own; code/signal when we spawned it and
/// could reap, null for an adopted browser we could only poll). In-memory
/// only: a server restart clears it, which GET /status says out loud.
pub static LAST_EXIT: LazyLock<Mutex<Option<serde_json::Value>>> =
    LazyLock::new(|| Mutex::new(None));

/// Watch the registered browser and RECORD its death instead of letting the
/// registry silently disagree with the process table. One global loop, spawned
/// lazily by start()/adopt: every 5s, a spawned child gets try_wait() (real
/// exit code/signal), an adopted one gets a kill -0 liveness poll. On death:
/// WARN with everything known, set LAST_EXIT, clear the registry and the
/// persisted running-file so no verb adopts a corpse.
fn spawn_exit_monitor() {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            // (profile, pid, started_by, exit code, signal)
            type DeadBrowser = (String, u32, String, Option<i32>, Option<i32>);
            let dead: Option<DeadBrowser> = {
                let mut g = RUNNING.lock().expect("browser registry poisoned");
                // FIRST dead browser this tick, if any. One per tick is fine at
                // a 5s cadence and keeps the borrow simple; the next tick takes
                // the next corpse (AMUX-3828).
                let mut dead_key: Option<String> = None;
                for (k, rb) in g.iter_mut() {
                    if rb.child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(Some(_)))) {
                        dead_key = Some(k.clone());
                        break;
                    }
                }
                match dead_key.and_then(|k| g.get_mut(&k)) {
                    None => None,
                    Some(rb) => match rb.child.as_mut() {
                        Some(child) => match child.try_wait() {
                            Ok(Some(status)) => {
                                #[cfg(unix)]
                                let signal = std::os::unix::process::ExitStatusExt::signal(&status);
                                #[cfg(not(unix))]
                                let signal = None;
                                Some((rb.profile.clone(), rb.pid, rb.started_by.clone(), status.code(), signal))
                            }
                            _ => None,
                        },
                        None => None, // adopted: polled below without the lock held
                    },
                }
            };
            // Adopted browser (no child handle): poll liveness outside the lock.
            let adopted_dead = if dead.is_none() {
                // The first ADOPTED browser (no child handle) whose pid is
                // gone. Checked one per tick like the spawned case above; with
                // several browsers the next tick takes the next (AMUX-3828).
                let snap = running_all().into_iter().find(|(p, _, _, _, _, _)| {
                    RUNNING
                        .lock()
                        .expect("browser registry poisoned")
                        .get(p)
                        .is_some_and(|r| r.child.is_none())
                });
                match snap {
                    Some((profile, owner, _, pid, _, _)) => {
                        let alive = pid_alive(pid);
                        // Only report dead if the registry STILL names this pid
                        // (a stop/start between poll and here changes the pid).
                        // Still the SAME pid under THIS profile (a stop/start
                        // between poll and here changes it).
                        if !alive
                            && running_snapshot_for(&profile).map(|(_, _, _, p, _)| p) == Some(pid)
                        {
                            Some((profile, pid, owner, None, None))
                        } else {
                            None
                        }
                    }
                    None => None,
                }
            } else {
                None
            };
            if let Some((profile, pid, owner, code, signal)) = dead.or(adopted_dead) {
                tracing::warn!(
                    profile, pid, started_by = %owner, code, signal,
                    "browser: Chrome EXITED outside any /stop — recording last_exit and clearing \
                     the registry (AMUX-3414: a silent exit used to leave nothing on record)"
                );
                *LAST_EXIT.lock().expect("last-exit poisoned") = Some(serde_json::json!({
                    "ts": chrono::Utc::now().timestamp(),
                    "reason": "exited",
                    "profile": profile,
                    "pid": pid,
                    "started_by": owner,
                    "code": code,
                    "signal": signal,
                }));
                RUNNING.lock().expect("browser registry poisoned").remove(&profile);
                clear_running_for(&amux_home(), &profile);
            }
        }
    });
}

/// Test seam: registry seeding for the takeover-guard tests, which must
/// exercise the refusal WITHOUT launching a Chrome. `child` stays None.
#[cfg(test)]
pub fn test_seed_running(profile: &str, started_by: &str, pid: u32) {
    test_seed_running_port(profile, started_by, pid, 1)
}

/// Seed with an explicit CDP port. Two seeded browsers must be able to DIFFER
/// in port, or a test asserting that two workers get different browsers passes
/// vacuously against a registry that hands both the same one (AMUX-3828).
#[cfg(test)]
pub fn test_seed_running_port(profile: &str, started_by: &str, pid: u32, port: u16) {
    RUNNING.lock().expect("browser registry poisoned").insert(profile.to_string(), RunningBrowser {
        profile: profile.into(),
        user_data_dir: PathBuf::new(),
        cdp_port: port,
        started_at: 0,
        last_verb_at: 0,
        pid,
        started_by: started_by.into(),
        child: None,
    });
}
#[cfg(test)]
pub fn test_clear_running() {
    RUNNING.lock().expect("browser registry poisoned").clear();
}

/// Where the handle survives a restart (AC-325).
///
/// The registry was in-process ONLY, so every server rebuild turned a live
/// browser into "no amux-launched browser is running" while Chrome kept
/// running — orphaned and unreachable at the same time. On a shared checkout
/// where peers' compile loops restart this server constantly, that killed every
/// browser sequence longer than a few seconds, which is every UI verification
/// (start -> auth -> switch org -> navigate -> click -> read). Two sessions
/// spent two days concluding the rig was flaky; it was being killed on a
/// schedule set by other lanes.
///
/// This is AC-296 in rust: python's driver registry had the same in-memory
/// assumption and was fixed by persisting the marker.
fn running_state_path(home: &Path) -> PathBuf {
    home.join("browser-running.json")
}

fn last_exit_path(home: &Path) -> PathBuf {
    home.join("browser-last-exit.json")
}

/// Record why the browser is gone, in memory AND on disk (AF-378).
///
/// `LAST_EXIT` alone is in-memory, and its own note says a server restart clears
/// it. That is a worse limitation than it reads, because of WHICH restart clears
/// it. The commonest death is "killed alongside a server restart", and the adopt
/// path below records that correctly in the NEW process, so the record does
/// survive the restart that caused the death. What kills it is the NEXT restart,
/// unrelated and usually minutes later: this box rebuilds and swaps the server on
/// every commit, so the explanation evaporates long before anyone asks.
///
/// Reported by tubescience 2026-08-31: two browsers on two ports, each dying
/// within minutes, and all they saw was ConnectionRefusedError on the CDP socket
/// mid-suite. `status()` would have explained it; a process watching a socket does
/// not call `status()`, and by the time it did the record was gone.
///
/// Best-effort on purpose. A failure to WRITE the explanation must never fail the
/// path that is already handling a death.
pub(crate) fn record_last_exit(home: &Path, v: serde_json::Value) {
    *LAST_EXIT.lock().expect("last-exit poisoned") = Some(v.clone());
    let _ = std::fs::write(last_exit_path(home), v.to_string());
}

/// The persisted record, for when this process has none of its own.
///
/// Tagged `from_disk` so a reader can tell a record THIS server observed from one
/// it inherited. Without that the two read identically, which is the distinction
/// the in-memory note was careful about in the first place.
pub(crate) fn persisted_last_exit(home: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(last_exit_path(home)).ok()?;
    let mut v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if let Some(o) = v.as_object_mut() {
        o.insert("from_disk".into(), serde_json::json!(true));
    }
    Some(v)
}

/// The running-file, as a MAP keyed by profile (AMUX-3828).
///
/// One file holding every browser, rather than one file per profile: profile
/// names are user input and would have to be sanitised into filenames, and a
/// traversal bug in a path built from a profile name is a worse failure than
/// anything this file is protecting. Rewritten whole on every change, always
/// under the RUNNING lock, so two concurrent starts cannot interleave a
/// read-modify-write.
///
/// READS TOLERATE THE LEGACY SHAPE. Before this change the file was a single
/// bare object; a server that upgrades mid-flight must still adopt the browser
/// it left behind rather than orphaning a live Chrome nobody can stop.
fn read_running_file(home: &Path) -> std::collections::HashMap<String, serde_json::Value> {
    let Ok(raw) = std::fs::read_to_string(running_state_path(home)) else {
        return Default::default();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Default::default();
    };
    // Legacy: a bare record with a `pid` at the top level.
    if v.get("pid").is_some() {
        let profile =
            v.get("profile").and_then(serde_json::Value::as_str).unwrap_or("default").to_string();
        return std::collections::HashMap::from([(profile, v)]);
    }
    v.as_object()
        .map(|o| o.iter().map(|(k, x)| (k.clone(), x.clone())).collect())
        .unwrap_or_default()
}

fn write_running_file(home: &Path, map: &std::collections::HashMap<String, serde_json::Value>) {
    let obj: serde_json::Map<String, serde_json::Value> =
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let _ = std::fs::write(running_state_path(home), serde_json::Value::Object(obj).to_string());
}

fn persist_running(home: &Path, profile: &str, dir: &Path, port: u16, pid: u32, started: i64, started_by: &str) {
    let mut map = read_running_file(home);
    map.insert(
        profile.to_string(),
        serde_json::json!({
            "profile": profile,
            "user_data_dir": dir.to_string_lossy(),
            "cdp_port": port,
            "pid": pid,
            "started_at": started,
            "started_by": started_by,
        }),
    );
    write_running_file(home, &map);
}

/// Drop ONE profile's record. The others must survive — clearing the whole file
/// on a single browser's death is how the remaining ones become unadoptable
/// orphans after a restart.
fn clear_running_for(home: &Path, profile: &str) {
    let mut map = read_running_file(home);
    map.remove(profile);
    if map.is_empty() {
        let _ = std::fs::remove_file(running_state_path(home));
    } else {
        write_running_file(home, &map);
    }
}

#[allow(dead_code)]
fn clear_running(home: &Path) {
    let _ = std::fs::remove_file(running_state_path(home));
}

/// Re-adopt a browser this server did not spawn, if one is still there.
///
/// Verified against the LIVE process before adopting: the pid must exist AND
/// its CDP port must answer. A stale file must never resurrect a dead browser —
/// that would replace an honest "not running" with a confident wrong handle,
/// which is worse than the bug being fixed.
pub async fn adopt_if_orphaned(home: &Path) -> bool {
    // EVERY persisted record, not the first (AMUX-3828). With N browsers a
    // restart leaves N orphans, and adopting only one leaves the rest live but
    // unmanaged — running Chromes nobody can stop, holding profiles nobody can
    // reuse. Each is judged on its own liveness.
    let mut adopted_any = false;
    for (prof_key, v) in read_running_file(home) {
        if RUNNING.lock().expect("browser registry poisoned").contains_key(&prof_key) {
            continue;
        }
        if adopt_one(home, &v).await {
            adopted_any = true;
        }
    }
    adopted_any
}

/// Adopt ONE persisted record if its browser is genuinely alive.
async fn adopt_one(home: &Path, v: &serde_json::Value) -> bool {
    let port = v.get("cdp_port").and_then(serde_json::Value::as_u64).unwrap_or(0) as u16;
    let raw_pid = v.get("pid").and_then(serde_json::Value::as_u64).unwrap_or(0);
    let pid = persisted_process_id(v);
    let Some(pid) = pid else {
        if raw_pid != 0 {
            tracing::warn!(
                raw_pid,
                verdict = "invalid_process_id_refused",
                "browser: refused an unsafe process id from the persisted running-file; \
                 it cannot be adopted or signalled"
            );
        }
        return false;
    };
    if port == 0 {
        return false;
    }
    // Does CDP still answer? That is the only proof that matters — a live pid
    // whose Chrome has wedged is not an adoptable browser.
    if cdp_list(port).await.is_err() {
        // RECORD THE CORPSE, do not just sweep it (AMUX-3414). The exit
        // monitor dies WITH the server, so a Chrome killed during a server
        // restart — the builder adopts a new binary several times an hour on
        // this machine, and the deaths correlate — leaves no in-process
        // record. The restarted server finding a running-file whose browser
        // is dead IS the observation; clearing it silently was why two lanes
        // spent a morning on hypotheses for a death nothing logged.
        let started_by =
            v.get("started_by").and_then(serde_json::Value::as_str).unwrap_or("");
        let profile =
            v.get("profile").and_then(serde_json::Value::as_str).unwrap_or("default");
        tracing::warn!(
            pid, port, profile, started_by,
            "browser: running-file names a browser that is GONE (CDP dead) — it died while \
             no server was watching, most likely killed alongside a server restart; \
             recording last_exit and clearing the file"
        );
        record_last_exit(home, serde_json::json!({
            "ts": chrono::Utc::now().timestamp(),
            "reason": "found dead on adopt (died while the server was down or restarting)",
            "profile": profile,
            "pid": pid,
            "started_by": started_by,
            "code": serde_json::Value::Null,
            "signal": serde_json::Value::Null,
        }));
        clear_running_for(home, profile);
        return false;
    }
    let profile = v.get("profile").and_then(serde_json::Value::as_str).unwrap_or("default").to_string();
    let dir = v
        .get("user_data_dir")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_default();
    let started = v.get("started_at").and_then(serde_json::Value::as_i64).unwrap_or(0);
    let started_by =
        v.get("started_by").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
    RUNNING.lock().expect("browser registry poisoned").insert(profile.clone(), RunningBrowser {
        profile,
        user_data_dir: dir,
        cdp_port: port,
        started_at: started,
        last_verb_at: started,
        pid,
        started_by,
        child: None,
    });
    tracing::info!(pid, port, "browser: adopted an orphan left by a previous server process");
    spawn_exit_monitor();
    true
}

/// Every LIVE Chrome holding this exact `--user-data-dir`, from the OS.
///
/// `browser-running.json` records ONE pid, so every reconcile built on it can
/// clear at most one orphan — and since AMUX-3184 detached Chrome into its own
/// process group (so it survives the builder's re-exec, which is the whole
/// point), each start/restart cycle can strand another. They accumulate
/// silently, and Chrome's user-data-dir is single-instance: the second and
/// later ones show the user "Something went wrong when opening your profile.
/// Some features may be unavailable."
///
/// Measured 2026-08-24 from Ethan's report: FIVE live Chromes on
/// `playwright-auth/profile`, ports 58242, 63713, 53913, 53402 and 60005, plus
/// a sixth on `profiles/netsuite`. The state file named one of them.
///
/// So ask the OS, which knows all of them, instead of a handle we remembered.
/// Scoped hard: an EXACT match on the flag value, and only inside amux-owned
/// dirs — the user's own Chrome must never be a candidate.
fn live_chromes_on_dir(home: &Path, target_dir: &Path) -> Vec<u32> {
    if !is_amux_owned(home, target_dir) {
        return Vec::new();
    }
    let flag = format!("--user-data-dir={}", target_dir.display());
    let out = match std::process::Command::new("ps").args(["-ax", "-o", "pid=,command="]).output() {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(?e, "browser: could not list processes to find profile co-tenants");
            return Vec::new();
        }
    };
    pids_on_dir(&String::from_utf8_lossy(&out.stdout), &flag)
}

/// Pure half of [`live_chromes_on_dir`], so the matching rule is testable
/// without a live `ps` and without live Chromes to kill.
fn pids_on_dir(ps_output: &str, flag: &str) -> Vec<u32> {
    ps_output
        .lines()
        // EXACT ARG MATCH, NOT `contains`. `playwright-auth/profile` is a
        // PREFIX of `playwright-auth/profiles/netsuite`, so a substring test on
        // the flag makes the DEFAULT profile match every named one — measured
        // before shipping, on the live machine: `contains` found 9 processes on
        // the default dir INCLUDING the `netsuite` browser holding a staged
        // login, while the exact rule finds 7 and leaves netsuite alone. A
        // launch on `default` would have killed every other profile's browser,
        // which is a far worse bug than the one being fixed.
        //
        // Tokenising on whitespace is safe HERE and only here: the
        // `is_amux_owned` gate in the caller means the dir is under `~/.amux`,
        // which has no spaces. The user's own Chrome dirs do ("Application
        // Support") and are excluded by that same gate.
        .filter(|l| l.split_whitespace().any(|tok| tok == flag))
        // Helper processes carry `--type=renderer` and friends and die with
        // their parent; killing them individually is noise at best.
        .filter(|l| !l.split_whitespace().any(|t| t.starts_with("--type=")))
        .filter_map(|l| l.split_whitespace().next()?.parse::<u32>().ok())
        .collect()
}

/// Before launching, make sure no Chrome from a PRIOR server process still holds
/// `target_dir`. The in-memory RUNNING registry does not survive the builder's
/// re-exec, so a live orphan is invisible to `start()`, and a second Chrome on
/// the same `--user-data-dir` delegates to that orphan and exits 0 before binding
/// any debug port ("Chrome exited 0 before CDP came up", AMUX-3207). Three cases,
/// all closed here so the launch never delegates:
///   - orphan with LIVE CDP: adopt it into RUNNING, so start()'s replace-check
///     stops it by pid (the reported incident: pid 54237, CDP 53470, still alive).
///   - orphan whose CDP has WEDGED: adopt cannot speak to it and clears the file,
///     but the process still owns the dir. Kill it by the pid the running-file
///     recorded, or the next launch delegates to a zombie forever.
///   - CO-TENANTS THE STATE FILE NEVER NAMED (AMUX-3674): the file records one
///     pid, and detached Chrome survives a re-exec, so earlier launches strand
///     browsers nothing remembers. [`live_chromes_on_dir`] asks the OS instead.
async fn reconcile_orphan_before_launch(home: &Path, target_dir: &Path) {
    // Snapshot the persisted pid/dir BEFORE adopt, which clears the file on dead CDP.
    let persisted = std::fs::read_to_string(running_state_path(home))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
    // Live-CDP orphan: adopt so start()'s replace path stops it.
    if adopt_if_orphaned(home).await {
        return;
    }
    // Dead-CDP orphan still holding the dir: kill by pid so the launch does not
    // delegate to it. Only when the recorded dir matches what we are about to
    // launch on, so we never kill a browser on an unrelated profile.
    if let Some(v) = persisted {
        let pid = persisted_process_id(&v).unwrap_or(0);
        let dir = v.get("user_data_dir").and_then(serde_json::Value::as_str).unwrap_or_default();
        if pid != 0 && Path::new(dir) == target_dir {
            let alive = pid_alive(pid);
            if alive {
                tracing::warn!(
                    pid, dir,
                    "browser: killing an orphan Chrome that held the profile but whose CDP was \
                     unreachable — a launch on this dir would delegate to it and exit 0 (AMUX-3207)"
                );
                let _ = run_kill(pid, "-KILL", "orphan reconciliation").await;
                clear_running(home);
            }
        }
    }

    // THE ONES THE STATE FILE NEVER KNEW ABOUT (AMUX-3674).
    //
    // Everything above reconciles the ONE pid we persisted. Detached Chrome
    // survives a server re-exec, so earlier launches strand co-tenants the file
    // no longer names, and a launch onto a dir another Chrome already holds
    // gets Chrome's profile-lock modal instead of a working browser.
    //
    // Runs LAST and unconditionally, including after `adopt_if_orphaned`
    // returned early — the adopted one is a legitimate tenant and is excluded
    // by pid, but its co-tenants are not.
    // EVERY registered pid is a legitimate tenant, not just one (AMUX-3828).
    // With N browsers, filtering on a single pid would have this loop kill its
    // own siblings — the exact "two Chromes on one dir" harm it exists to stop,
    // caused by the fix for it.
    let adopted: std::collections::HashSet<u32> =
        running_all().into_iter().map(|(_, _, _, pid, _, _)| pid).collect();
    let strays: Vec<u32> = live_chromes_on_dir(home, target_dir)
        .into_iter()
        .filter(|p| !adopted.contains(p))
        .collect();
    for pid in &strays {
        tracing::warn!(
            pid, dir = %target_dir.display(),
            "browser: killing a stray Chrome co-tenant on this profile — two Chromes on one \
             user-data-dir is what shows the user 'Something went wrong when opening your \
             profile' (AMUX-3674)"
        );
        let _ = run_kill(*pid, "-KILL", "stray browser reconciliation").await;
    }
    if !strays.is_empty() {
        // Counted, not just logged: a silent cleanup cannot tell anyone the
        // leak is still happening, and the leak is the actual defect. This is
        // the number to watch — it should trend to zero once nothing strands.
        tracing::warn!(
            count = strays.len(), dir = %target_dir.display(),
            "browser: profile had {} stray co-tenant(s) before launch — each one was a \
             corrupted-profile modal waiting to happen",
            strays.len()
        );
    }
}

/// EVERY RUNNING BROWSER, KEYED BY PROFILE (AMUX-3828; Ethan: "multiple
/// browsers shouldnt be an issue").
///
/// One slot per PROFILE, not per machine and not per session, because the
/// invariant this registry exists to protect is a filesystem one: two Chrome
/// processes on one `user_data_dir` corrupt it. Profiles are separate
/// directories under `playwright-auth/profiles/`, so N browsers on N profiles
/// is safe by construction — and keying by SESSION would have permitted the one
/// arrangement that is not.
///
/// The map is the guard: an occupied key IS the refusal, so the safety property
/// cannot drift from the data structure the way a separate check would.
pub static RUNNING: LazyLock<Mutex<std::collections::HashMap<String, RunningBrowser>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Locate a Chrome/Chromium binary. None is an honest answer the API
/// surfaces as 501 — not a fallback to some other browser.
pub fn chrome_binary() -> Option<PathBuf> {
    let fixed = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ];
    for c in fixed {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Some(p);
        }
    }
    let names = ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"];
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        for n in names {
            let p = dir.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn ephemeral_port() -> anyhow::Result<u16> {
    // Bind :0, read the port, release. A race with another process is
    // possible but Chrome fails loudly if it loses, and CDP wait catches it.
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

/// base64(sha256(DER SubjectPublicKeyInfo)) of amux's own TLS cert — the exact
/// form Chrome's `--ignore-certificate-errors-spki-list` expects.
///
/// Read from `~/.amux/tls/cert.pem` at LAUNCH time rather than baked in as a
/// constant: tls.rs regenerates that material when it expires or is deleted, and
/// a hardcoded pin would then silently stop matching — the browser would go back
/// to failing on amux's own URL with nothing pointing at why. Deriving it from
/// the same file the server presents means the pin cannot drift from the cert.
///
/// Returns None if the cert is unreadable or malformed, in which case the flag is
/// simply not passed: no pin is strictly worse for reaching amux, but inventing
/// or guessing a pin would be worse still, and a blanket cert bypass is never the
/// fallback (see the call site's note about the auth profile).
fn amux_cert_spki_b64(home: &Path) -> Option<String> {
    use base64::Engine as _;
    use sha2::Digest as _;
    // Derived from the KEY via rcgen — which already generated this material in
    // tls.rs and hands back the DER SubjectPublicKeyInfo directly. That avoids
    // both a new x509 dependency and hand-slicing ASN.1 out of the certificate,
    // which is the step this kind of helper usually gets subtly wrong.
    let key_pem = std::fs::read_to_string(home.join("tls").join("key.pem")).ok()?;
    let key_pair = rcgen::KeyPair::from_pem(&key_pem).ok()?;
    let digest = sha2::Sha256::digest(key_pair.public_key_der());
    Some(base64::engine::general_purpose::STANDARD.encode(digest))
}

#[derive(Debug, Clone, Serialize)]
pub struct StartedBrowser {
    pub profile: String,
    /// Present only on the first start of a profile selected from ordinary
    /// Chrome. The browser runs from `user_data_dir`, never from this source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_imported_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_imported_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_imported_bytes: Option<u64>,
    pub pid: Option<u32>,
    pub cdp_port: u16,
    pub cdp_http: String,
    pub user_data_dir: String,
    pub started_at: i64,
    /// The URL of the page tab this launch actually produced.
    ///
    /// `start()` took a `url` and returned NOTHING about where the browser
    /// ended up, so `/api/browser/start`'s response had no url of any kind.
    /// The dashboard read it as `d.data && d.data.url`, which is undefined for
    /// this endpoint, fell back to the url it had ASKED for, and printed
    /// "Navigated" unconditionally. So the one word the user gets meant both
    /// "you are on the page you typed" and "Chrome started and went somewhere
    /// else entirely" — reported 2026-08-25 as "it says navigated but i dont
    /// see anything", with google.com requested and a session-restored
    /// app.hubspot.com/login on screen.
    ///
    /// HONEST ABOUT ITS OWN LIMITS: this is read immediately after launch, so a
    /// slow page can still be `about:blank` here and a redirect may not have
    /// resolved. It is evidence of WHERE THE TAB WENT, not a load-complete
    /// signal — the caller must not treat a mismatch as failure, only as a fact
    /// worth showing. `None` when CDP listed no page tab at all.
    pub launch_url: Option<String>,
}

/// The tail of Chrome's launch stderr, formatted for an error message, or "" if
/// the file is missing or empty. Turns "CDP never answered" from an opaque
/// timeout into an actionable reason (a bad flag, a locked profile, no display).
fn chrome_stderr_tail(path: &Path) -> String {
    let Ok(s) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let s = s.trim();
    if s.is_empty() {
        return String::new();
    }
    let n = s.chars().count();
    let tail: String = s.chars().skip(n.saturating_sub(600)).collect();
    format!(" (chrome stderr: {tail})")
}

/// What one CDP readiness poll actually got (AMUX-3689).
///
/// Pure and separate from the wait loop so the distinction it exists to draw can
/// be tested without spawning a Chrome. The distinction: **Chrome answering with
/// a status we reject is not Chrome being silent**, and the loop reported both
/// as "never answered within 30s" while the stderr quoted in the same message
/// said `DevTools listening on ws://127.0.0.1:<the very port>`. An investigator
/// reading "never answered" goes looking at Chrome's startup; an investigator
/// reading "HTTP 403" goes looking at what amux asked for.
fn describe_cdp_probe(
    outcome: &Result<reqwest::Response, reqwest::Error>,
    http: &str,
) -> String {
    match outcome {
        Ok(r) => format!("HTTP {} from {http}/json/version", r.status()),
        Err(e) if e.is_connect() => "connection refused (nothing listening)".into(),
        Err(e) if e.is_timeout() => "no response within the 1s poll timeout".into(),
        Err(e) => format!("{e}"),
    }
}

/// Keep a FAILED launch's stderr where the retry cannot destroy it (AMUX-3689).
///
/// `start()` opens the stderr path with `File::create`, which truncates. So the
/// diagnostic for a failure is deleted by the next attempt on the same profile,
/// and a caller that fails always retries. `primer` hit the CDP timeout six
/// times on 2026-08-24 and only the last one's file survived, leaving the 600
/// char tail already pasted into the error as the entire record of the incident.
/// That is the artifact you need MOST of when a failure repeats, destroyed by
/// the fact that it repeated.
///
/// Copies rather than renames, because Chrome is still running and still holds
/// the descriptor. Best-effort throughout: a browser launch must not fail
/// because a diagnostic copy failed, so every error here returns "" and the
/// caller's message is merely shorter.
fn preserve_failed_stderr(path: &Path) -> String {
    let Some(dir) = path.parent() else { return String::new() };
    // A WALL CLOCK IS NOT A UNIQUENESS SOURCE, and this function learned it
    // twice from its own test. Draft one stamped SECONDS: two retries inside one
    // second minted the same name, the copy overwrote the previous failure, and
    // the function silently did the exact thing it exists to prevent (the exit-0
    // delegation path fails in 1.5 to 3s, so that is routine). Draft two moved to
    // MILLISECONDS and the same assertion failed again, because two calls in a
    // unit test land in one millisecond just as happily.
    //
    // Any finer clock is the same bet at a smaller scale. So the stamp is for
    // humans and for sorting, and the SEQUENCE is what guarantees uniqueness:
    // probe until the name is free. Zero-padded so lexicographic order still
    // matches chronological order within a millisecond, which the prune below
    // relies on.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let base = format!("amux-chrome-launch.failed-{stamp}");
    let mut kept = dir.join(format!("{base}-000.stderr"));
    let mut seq = 0u32;
    // Bounded: 1000 failures inside one millisecond is not a state worth looping
    // for, and overwriting the last name beats spinning in a launch path.
    while kept.exists() && seq < 999 {
        seq += 1;
        kept = dir.join(format!("{base}-{seq:03}.stderr"));
    }
    if std::fs::copy(path, &kept).is_err() {
        return String::new();
    }
    // Bounded, or a profile that fails all day fills the disk with its own
    // evidence. Newest 5 by name, which sorts by the millisecond stamp above.
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut old: Vec<_> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("amux-chrome-launch.failed-"))
            })
            .collect();
        old.sort();
        while old.len() > 5 {
            let _ = std::fs::remove_file(old.remove(0));
        }
    }
    format!(" Full stderr kept at {}.", kept.display())
}

/// Chrome's `kBadFlags` (chrome/browser/ui/startup/bad_flags_prompt.cc) that
/// this launcher could ever pass. Any of them puts the yellow "You are using an
/// unsupported command-line flag ... Stability and security will suffer."
/// infobar across the top of every window UNLESS the launch also declares
/// itself a test harness — see `chrome_launch_args`. Kept as data so the test
/// pinning that invariant cannot drift from the flags actually passed.
#[cfg(test)]
const CHROME_BAD_FLAGS: [&str; 2] =
    ["--ignore-certificate-errors", "--ignore-certificate-errors-spki-list"];

/// Every flag Chrome is launched with, in order, as ONE list. Pure in its
/// inputs so the invariants between flags can be tested without spawning a
/// Chrome (`launch_args_tests`); `start()` feeds it straight to the Command.
fn chrome_launch_args(
    port: u16,
    target: &LaunchTarget,
    headless: bool,
    spki: Option<&str>,
    url: &str,
) -> Vec<String> {
    let mut args = vec![
        format!("--remote-debugging-port={port}"),
        format!("--user-data-dir={}", target.user_data_dir.display()),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        // --test-type: the SPKI pin below is on Chrome's kBadFlags list, so every
        // launch opened with the "unsupported command-line flag" infobar across
        // the window (Ethan's screenshot, 2026-08-24, MR-38). Chrome skips ALL of
        // its startup infobars for a launch that declares itself a test harness:
        // chrome/browser/ui/startup/infobar_utils.cc returns before
        // ShowBadFlagsPrompt when `kTestType || IsAutomationEnabled()`.
        // --test-type is the right one of the two: --enable-automation also
        // skips the bad-flags bar but adds its own "Chrome is being controlled by
        // automated test software" bar, and this is a browser a human logs into.
        // ChromeDriver passes `--test-type=webdriver` on every session for the
        // same reason. It changes nothing about what the pin permits — the pin
        // is still the only certificate exemption. Passed unconditionally rather
        // than only alongside the pin, so a future bad flag added here inherits
        // the suppression instead of re-introducing the banner.
        "--test-type".to_string(),
    ];
    // AMUX-3508: same profile dirs, no window — the logins made headfully in
    // the amux Browser UI ride along, which is the whole point of profiles
    // being durable. `new` headless shares the profile format with headful
    // Chrome (the old --headless did not persist everything).
    if headless {
        args.push("--headless=new".to_string());
    }
    // amux serves its own dashboard over a SELF-SIGNED cert, so without this the
    // one site this browser most needs to reach — amux itself — lands on
    // chrome-error://chromewebdata/ and every subsequent verb runs against the
    // error page. Dogfooding: the browser primitive could not browse the product
    // that ships it.
    //
    // Deliberately SPKI-PINNED to amux's own key rather than the blanket
    // --ignore-certificate-errors. This profile (~/.amux/playwright-auth/profile)
    // holds real logged-in sessions for third-party sites; globally disabling
    // certificate validation there would expose those cookies to any MITM on the
    // network. The pin excuses exactly one public key and leaves validation fully
    // intact for everything else — including, importantly, still rejecting a
    // DIFFERENT bad cert on localhost.
    if let Some(spki) = spki {
        args.push(format!("--ignore-certificate-errors-spki-list={spki}"));
    }
    if let Some(pd) = &target.profile_directory {
        args.push(format!("--profile-directory={pd}"));
    }
    if !url.trim().is_empty() {
        args.push(url.trim().to_string());
    }
    args
}

/// Launch Chrome on a profile. Cleans stale locks first (amux-owned dirs
/// only — see module docs), waits for the CDP HTTP endpoint to answer so a
/// returned port is a WORKING port, and records the child for stop().
/// `session` is the lane this launch belongs to. It exists because of AC-336:
/// the tab Chrome opens for `url` must be CLAIMED by the caller, or the next
/// lane to run a driver verb adopts it as an unowned page.
pub async fn start(
    home: &Path,
    profile: &str,
    url: &str,
    session: &str,
    started_by: &str,
    headless: bool,
) -> anyhow::Result<StartedBrowser> {
    // A Chrome from a PREVIOUS server process may still hold this profile. The
    // RUNNING registry is in-memory and does not survive the builder's constant
    // re-exec, so start() used to be blind to that orphan: it checked only
    // RUNNING, found nothing, and launched a SECOND Chrome on the same
    // --user-data-dir. Chrome delegates to the instance already holding the
    // profile and exits 0, so the new debug port never binds ("Chrome exited 0
    // before CDP came up", AMUX-3207). The driver-verb path already reconciles
    // via adopt_if_orphaned (connect_session); start() did not. Reconcile here,
    // before the replace-check, so the orphan is STOPPED rather than delegated to.
    let chrome_dir = chrome_user_data_dir();
    let unresolved = launch_target(home, &chrome_dir, profile);
    let imported = if is_amux_owned(home, &unresolved.user_data_dir) {
        None
    } else {
        // Never launch automation against the human browser's user-data-dir.
        // Besides risking profile corruption, Chrome delegates the launch to
        // the already-running app and exits before CDP exists. Copying can be
        // substantial, so keep it off the async runtime's worker threads.
        let import_home = home.to_path_buf();
        let import_chrome = chrome_dir.clone();
        let import_name = profile.to_string();
        let result = tokio::task::spawn_blocking(move || {
            import_chrome_profile(&import_home, &import_chrome, &import_name)
        })
        .await
        .map_err(|e| anyhow::anyhow!("Chrome profile import task failed: {e}"))??;
        if let Some(i) = &result {
            tracing::info!(
                profile,
                source = %i.source.display(),
                destination = %i.destination.display(),
                files = i.files,
                bytes = i.bytes,
                "browser: imported a human Chrome profile into an isolated amux profile"
            );
        }
        result
    };
    let target = launch_target(home, &chrome_dir, profile);
    if !is_amux_owned(home, &target.user_data_dir) {
        anyhow::bail!(
            "refusing to launch automation against the human Chrome user-data-dir {}; the \
             isolated profile import did not produce a runnable destination",
            target.user_data_dir.display()
        );
    }
    reconcile_orphan_before_launch(home, &target.user_data_dir).await;

    // A profile inside the HUMAN'S Chrome directory shares one SingletonLock
    // across every profile in that Chrome process. If the human Chrome is
    // running, spawning another `Google Chrome` does not fail harmlessly: it
    // hands the URL to the existing, non-debuggable process and exits 0. A
    // retry loop therefore opens one real tab per retry (MO-3146: tubescience
    // opened dozens before the user noticed).
    //
    // Fail BEFORE spawn. We neither stop the human's process nor remove its
    // lock; both would mutate state amux does not own. The typed 409 tells an
    // agent not to retry and gives both honest exits: fully quit Chrome once,
    // or drive the live browser through /chrome-cdp after enabling remote
    // debugging. The WARN is deliberately stable and carries a verdict so a
    // log sweep sees prevented incidents rather than only the user's tab bar.
    if let Some(owner_pid) = unmanaged_live_lock_owner(home, &target.user_data_dir) {
        tracing::warn!(
            profile,
            owner_pid,
            dir = %target.user_data_dir.display(),
            verdict = "prevented_before_spawn",
            retryable = false,
            "browser: prevented launch into a live human Chrome — retrying would open another tab (MO-3146)"
        );
        return Err(anyhow::Error::new(ExternalProfileInUse {
            profile: profile.to_string(),
            user_data_dir: target.user_data_dir,
            owner_pid,
        }));
    }

    let binary = chrome_binary().ok_or_else(|| {
        anyhow::anyhow!("no Chrome/Chromium binary found (looked in /Applications and PATH)")
    })?;

    // ONE BROWSER PER PROFILE, and only this profile (AMUX-3828). This used to
    // stop whatever was running, which is what made the browser a machine
    // singleton. The invariant was never "one Chrome" — it is "one Chrome per
    // user_data_dir", because that is what corrupts. A browser on a DIFFERENT
    // profile is a legitimate neighbour and is left alone.
    if RUNNING.lock().expect("browser registry poisoned").contains_key(profile) {
        // NAME THE DISPLACER AND SAY IT WAS A DISPLACEMENT (AF-378 follow-up).
        // `started_by` is the resolved attribution of whoever asked for THIS
        // start; it was already in hand here and was being dropped, so every
        // browser replaced by a peer's start recorded `by: null`. The reason is
        // "displaced by a new start" rather than "stopped" because nobody asked
        // to stop it: naming the starter as the stopper would be a wrong
        // attribution, which is the failure this record exists to prevent.
        let _ = stop_profile_as_reason(home, profile, started_by, "displaced by a new start").await;
    }

    if is_amux_owned(home, &target.user_data_dir) {
        std::fs::create_dir_all(&target.user_data_dir)?;
        // The child registry is empty (stopped above), so any lock here is a
        // leftover from a dead Chrome and would block this launch.
        let removed = clean_locks(&target.user_data_dir);
        if !removed.is_empty() {
            tracing::info!(dir = %target.user_data_dir.display(), ?removed, "cleaned stale Chrome locks before launch");
        }
    }

    let port = ephemeral_port()?;
    let mut cmd = tokio::process::Command::new(&binary);
    // OWN PROCESS GROUP (AMUX-3414 root cause). Chrome inherited this server's
    // group, and the builder's self-adoption relaunch (`launchctl kickstart -k`)
    // kills the whole group — so every build adoption, several per hour on this
    // machine, silently killed the running browser. That contradicted AC-325's
    // entire adopt design, which assumes the browser SURVIVES a server restart;
    // it never did, and the deaths recorded no reason (three staged-login kills
    // in one morning were root-caused by starting a browser, committing, and
    // watching the adoption take it down). Detached, the group kill misses
    // Chrome and the next server process adopts it, as designed.
    #[cfg(unix)]
    cmd.process_group(0);
    let spki = amux_cert_spki_b64(home);
    cmd.args(chrome_launch_args(port, &target, headless, spki.as_deref(), url));
    // Capture Chrome's stderr so a launch failure is DIAGNOSABLE. It used to go
    // to /dev/null, so "CDP never answered" could not distinguish a crash (a bad
    // flag, a locked profile, no display) from a slow start — the exact failure
    // this wait exists to report. A file is non-blocking; its tail is read back
    // into the error below.
    let stderr_path = target.user_data_dir.join("amux-chrome-launch.stderr");
    cmd.stdout(std::process::Stdio::null());
    match std::fs::File::create(&stderr_path) {
        Ok(f) => {
            cmd.stderr(std::process::Stdio::from(f));
        }
        Err(_) => {
            cmd.stderr(std::process::Stdio::null());
        }
    }
    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("spawn {}: {e}", binary.display()))?;
    let pid = child.id();

    // A port Chrome never bound is not a started browser; wait for CDP HTTP.
    // Two failure modes, told apart because they need opposite responses:
    //  - Chrome EXITED before CDP came up (crash): report the exit status and
    //    the stderr tail IMMEDIATELY — waiting the full window for a dead
    //    process is pointless and the exit reason is what the caller needs.
    //  - Chrome is ALIVE but slow: a real logged-in profile on a busy machine
    //    (a 50-lane fleet) routinely takes longer than the old 12s to open the
    //    debugging port, so give a live Chrome up to 30s before giving up.
    let http = format!("http://127.0.0.1:{port}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    // ONE CLIENT, not one per poll. This loop runs ~120 times in a 30s wait and
    // built a fresh `reqwest::Client` on every pass, each of which allocates a
    // connection pool, a TLS config, and on macOS reads the system proxy
    // settings through SystemConfiguration. Nothing here needs a new client.
    let probe = reqwest::Client::new();
    // WHAT THE LAST POLL ACTUALLY GOT (AMUX-3689). The loop discarded every
    // outcome, so connection-refused, a timeout, a 403 and a 500 all produced
    // the identical "CDP never answered within 30s".
    //
    // That is not merely vague, it can be FALSE, and the specimen proves it: on
    // 2026-08-24 `primer` hit this six times, and the stderr embedded in the very
    // same message reads `DevTools listening on ws://127.0.0.1:60005/...` for the
    // port the message says never answered. CDP came up ~3s in and amux polled
    // it for another 27s. "Never answered" and "answered with a status I did not
    // accept" send an investigator to opposite places, and this could not say
    // which had happened.
    let mut attempts: u32 = 0;
    // Deliberately uninitialised: every path that READS it assigns it first, in
    // the same iteration. A placeholder here would be a value that can never be
    // observed, i.e. a state the message could claim and never mean.
    let mut last_probe: String;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            // A CLEAN exit (status 0) before CDP is the delegation signature: the
            // new Chrome handed its URL to an instance already holding this
            // profile and quit. reconcile_orphan_before_launch should now prevent
            // it, so reaching here means an orphan slipped past reconciliation
            // (e.g. an untracked Chrome outside the running-file). Name the cause
            // and WARN so a log sweep catches the class, not just this instance.
            let delegated = is_delegation_exit(&status);
            let hint = if delegated {
                " — exit 0 is the delegation signature: another Chrome already holds this \
                 --user-data-dir. amux reconciles known orphans before launch (AMUX-3207); an \
                 untracked Chrome on this profile can still cause it. Try again, or GET \
                 /api/browser/status and stop it first."
            } else {
                ""
            };
            if delegated {
                tracing::warn!(
                    ?pid, port, dir = %target.user_data_dir.display(),
                    "browser: launch delegated to an existing Chrome and exited 0 before CDP bound (AMUX-3207)"
                );
            }
            let msg = format!(
                "Chrome (pid {pid:?}) exited {status} before CDP on port {port} came up{}{hint}",
                chrome_stderr_tail(&stderr_path)
            );
            // Same string either way; only the TYPE differs, and only when the
            // caller can act (AF-381).
            if delegated {
                return Err(anyhow::Error::new(ProfileDelegated(msg)));
            }
            anyhow::bail!(msg);
        }
        attempts += 1;
        let outcome = probe
            .get(format!("{http}/json/version"))
            .timeout(std::time::Duration::from_secs(1))
            .send()
            .await;
        match outcome {
            Ok(r) if r.status().is_success() => break,
            other => {
                last_probe = describe_cdp_probe(&other, &http);
                if std::time::Instant::now() > deadline {
                    // WARN so a log sweep sees the CLASS, not just this instance
                    // (/api/logs/analyze groups the 502 but cannot read a
                    // free-text error body for the discriminator).
                    tracing::warn!(
                        ?pid, port, attempts, last_probe = %last_probe,
                        stderr = %stderr_path.display(),
                        "browser: CDP wait timed out (AMUX-3689)"
                    );
                    let kept = preserve_failed_stderr(&stderr_path);
                    anyhow::bail!(
                        "Chrome (pid {pid:?}) is running but CDP on port {port} did not become \
                         usable within 30s. Last of {attempts} polls: {last_probe}.{}{}",
                        kept,
                        chrome_stderr_tail(&stderr_path)
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
    }

    // AC-336: CLAIM THE TAB WE JUST OPENED, FOR THE LANE THAT ASKED FOR IT.
    //
    // `resolve_page` treats a page tab as available unless it appears in
    // NATIVE_TARGETS — i.e. unless some lane has already DRIVEN it through this
    // API. The tab Chrome opens for this `url` has never been driven, so it was
    // indistinguishable from an unowned tab, and the next lane to call a driver
    // verb with no binding of its own adopted it. That is precisely the hijack
    // `resolve_page`'s own comment promises not to do ("open a fresh one rather
    // than hijacking a peer's tab"), reached through the one tab kind that was
    // never registered.
    //
    // How it presented: I called /start with url=cloud.amux.io/sign-in, then
    // /action, and eval ran against localhost:4177/solutions/creative-dna — a
    // PEER lane's dev server, opened by their own /start. Every response said
    // ok:true, because from the driver's side adopting an unowned tab is the
    // documented behaviour. I typed a god-mode credential into it.
    //
    // Stale bindings are dropped first: `start` replaces any running browser
    // (see the stop above), so every target id recorded against the previous
    // Chrome now names a tab that does not exist. Leaving them is not merely
    // untidy — they are counted as `claimed_by_others`, so dead ids would make
    // live tabs look owned.
    //
    // A freshly launched Chrome has exactly one page tab, so taking the first
    // one is right regardless of what `url` redirected to — which is why this
    // matches on tab TYPE and not on the URL string.
    // The CDP call happens BEFORE the lock is taken: NATIVE_TARGETS is a
    // std::sync::Mutex, and holding one across an await deadlocks the runtime.
    // The same listing already resolves the page tab; its URL was being thrown
    // away, which is why the response could not say where the browser landed.
    let launch_page = cdp_list(port).await.ok().and_then(|tabs| {
        tabs.as_array()
            .and_then(|ts| ts.iter().find(|t| t.get("type").and_then(Value::as_str) == Some("page")))
            .map(|t| {
                (
                    t.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                    t.get("url").and_then(Value::as_str).unwrap_or("").to_string(),
                )
            })
    });
    let launch_url = launch_page
        .as_ref()
        .map(|(_, u)| u.clone())
        .filter(|u| !u.is_empty());
    let launch_tab = launch_page
        .as_ref()
        .map(|(id, _)| id.clone())
        .filter(|id| !id.is_empty());
    {
        let mut map = NATIVE_TARGETS.lock().expect("native targets poisoned");
        map.clear();
        if let Some(id) = &launch_tab {
            map.insert(session.to_string(), id.clone());
        }
    }
    match &launch_tab {
        Some(id) => {
            tracing::info!(session, target = %id, "claimed the launch tab for the starting lane (AC-336)")
        }
        // Not fatal: the lane simply has no binding yet and resolve_page will
        // open it a fresh tab. Logged because a silent miss here is what the
        // whole card is about.
        None => tracing::warn!(session, port, "launch tab not claimed — CDP listed no page tab"),
    }

    let started_at = chrono::Utc::now().timestamp();
    let info = StartedBrowser {
        profile: profile.to_string(),
        profile_imported_from: imported.as_ref().map(|i| i.source.display().to_string()),
        profile_imported_files: imported.as_ref().map(|i| i.files),
        profile_imported_bytes: imported.as_ref().map(|i| i.bytes),
        pid,
        cdp_port: port,
        cdp_http: http,
        user_data_dir: target.user_data_dir.display().to_string(),
        started_at,
        launch_url,
    };
    // `child.id()` is None only once the child has been reaped; a browser we
    // just spawned always has one. Refusing here beats persisting pid 0, which
    // `kill` would interpret as the whole process group.
    let pid_num = pid.ok_or_else(|| anyhow::anyhow!("chrome spawned without a pid"))?;
    let udd_for_state = target.user_data_dir.clone();
    RUNNING.lock().expect("browser registry poisoned").insert(profile.to_string(), RunningBrowser {
        profile: profile.to_string(),
        user_data_dir: target.user_data_dir,
        cdp_port: port,
        started_at,
        last_verb_at: started_at,
        pid: pid_num,
        // Ownership is the EXPLICIT attribution, never the tab-binding
        // default ("amux") — an anonymous start records "" so the guard can
        // refuse the next anonymous caller instead of treating them as the
        // same session (amux-cloud's validation catch on AMUX-3063).
        started_by: started_by.to_string(),
        child: Some(child),
    });
    // Survive a server restart (AC-325). Written AFTER the handle is live so a
    // file never claims a browser that failed to start.
    persist_running(home, profile, &udd_for_state, port, pid_num, started_at, started_by);
    spawn_exit_monitor();
    Ok(info)
}

#[derive(Debug, Clone, Serialize)]
pub struct StopReport {
    pub stopped: bool,
    pub profile: Option<String>,
    /// True when Chrome exited and dropped its own Singleton locks — the
    /// signal that Cookies/Local Storage were flushed (AMUX-2070).
    pub clean_exit: Option<bool>,
    pub locks_cleaned: Vec<String>,
}

/// Stop the tracked Chrome. SIGTERM first — TERM is the signal Chrome
/// flushes Cookies/Local Storage on; SIGKILL is precisely what loses them
/// (Python `_chrome_terminate_automation`'s lesson) — escalating to SIGKILL
/// only if it ignores TERM for 8s.
pub async fn stop(home: &Path) -> StopReport {
    stop_as(home, "").await
}

/// Stop ONE profile's browser, leaving every other running (AMUX-3828).
pub async fn stop_profile(home: &Path, profile: &str) -> StopReport {
    stop_profile_as(home, profile, "").await
}

/// Stop, recording WHO in last_exit (AMUX-3063: an unattributed stop is half
/// of how a staged login died anonymously; the request log had the row but
/// nothing browser-side named the actor or the owner it took the browser from).
pub async fn stop_as(home: &Path, stopped_by: &str) -> StopReport {
    // No profile named: stop the OLDEST, which is the closest honest reading of
    // a bare `stop` now that there can be several (AMUX-3828). The API layer
    // always names one; this arm exists for internal callers and tests.
    let oldest = {
        let g = RUNNING.lock().expect("browser registry poisoned");
        g.values().min_by_key(|r| r.started_at).map(|r| r.profile.clone())
    };
    let Some(profile) = oldest else {
        return StopReport { stopped: false, profile: None, clean_exit: None, locks_cleaned: vec![] };
    };
    stop_profile_as(home, &profile, stopped_by).await
}

/// [`stop_as`] for one named profile.
/// The `last_exit` record, as a pure value so it can be asserted on.
///
/// Extracted because the behaviour worth pinning lives in two fields and the
/// function that writes them early-returns when nothing is running, so there is
/// no seam to test through. `by` is NULL rather than "" on purpose: an empty
/// string and an unnamed actor read identically to a consumer, and telling them
/// apart is the whole point of the record (AMUX-3063).
fn exit_record(
    reason: &str,
    stopped_by: &str,
    profile: &str,
    pid: u32,
    started_by: &str,
    ts: i64,
) -> serde_json::Value {
    serde_json::json!({
        "ts": ts,
        "reason": reason,
        "by": if stopped_by.is_empty() { serde_json::Value::Null } else { serde_json::json!(stopped_by) },
        "profile": profile,
        "pid": pid,
        "started_by": started_by,
    })
}

pub async fn stop_profile_as(home: &Path, profile: &str, stopped_by: &str) -> StopReport {
    stop_profile_as_reason(home, profile, stopped_by, "stopped").await
}

/// Stop, naming BOTH the actor and WHY (AF-378 follow-up).
///
/// `stop_profile_as` hardcoded `reason: "stopped"`, which is right for a `/stop`
/// and wrong for the internal stop inside `start()`: there nobody asked for a
/// stop, a peer asked for a START and the running browser was displaced to make
/// room. Recording that as "stopped by <the new starter>" would name them as the
/// stopper, which is a subtly wrong attribution of exactly the kind this record
/// exists to prevent.
///
/// Found by reading a VALUE rather than a field's presence. amux-homepage
/// verified AF-378 by checking `last_exit` was populated, and the value they
/// pasted back was `{by: None, reason: stopped, started_by: mixpeek-cicd}` — the
/// record survived, and the one field the card is about was null. The path that
/// produced it was `start()` calling the actor-less `stop_profile`, while the
/// actor sat in its own `started_by` parameter the whole time.
pub async fn stop_profile_as_reason(
    home: &Path,
    profile: &str,
    stopped_by: &str,
    reason: &str,
) -> StopReport {
    let running = RUNNING.lock().expect("browser registry poisoned").remove(profile);
    let Some(mut running) = running else {
        return StopReport { stopped: false, profile: None, clean_exit: None, locks_cleaned: vec![] };
    };
    *LAST_EXIT.lock().expect("last-exit poisoned") = Some(exit_record(
        reason,
        stopped_by,
        &running.profile,
        running.pid,
        &running.started_by,
        chrono::Utc::now().timestamp(),
    ));

    // Validate before crossing the signed OS pid boundary. procps `kill` on
    // Linux accepts u32::MAX and turns it into the process-group sentinel -1;
    // a test fixture using that supposedly-impossible pid terminated the CI
    // runner and every sibling process (ATE-44). A Chrome child can never be
    // pid 0/1, so refusing those reserved values is also the safe direction.
    let signalable = checked_process_id(running.pid).is_some();
    // std/tokio only offer SIGKILL; /bin/kill sends the TERM Chrome needs to
    // flush Cookies and Local Storage. `run_kill` validates before spawning.
    let _ = run_kill(running.pid, "-TERM", "browser stop").await;
    match running.child.as_mut() {
        Some(ch) => {
            let graceful =
                tokio::time::timeout(std::time::Duration::from_secs(8), ch.wait()).await;
            if graceful.is_err() {
                let _ = ch.kill().await; // last resort; may lose storage flush
                let _ = ch.wait().await;
            }
        }
        None => {
            // ADOPTED: not our child, so we cannot wait() on it. Poll for exit
            // by pid instead, keeping the same 8s TERM budget before SIGKILL —
            // the flush window is the point, not the handle.
            if signalable {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
                while std::time::Instant::now() < deadline {
                    if !pid_alive(running.pid) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                let _ = run_kill(running.pid, "-KILL", "adopted browser stop").await;
            }
        }
    }
    clear_running(home);

    let clean_exit = locks_present(&running.user_data_dir).is_empty();
    let locks_cleaned = if !clean_exit && is_amux_owned(home, &running.user_data_dir) {
        // The child is reaped; remaining locks are stale by definition.
        clean_locks(&running.user_data_dir)
    } else {
        vec![]
    };
    StopReport {
        stopped: true,
        profile: Some(running.profile),
        clean_exit: Some(clean_exit),
        locks_cleaned,
    }
}

/// CDP over plain HTTP: the tab list. (Command traffic — screenshot, eval,
/// input — is the WebSocket client below, [`CdpClient`].)
pub async fn cdp_list(port: u16) -> anyhow::Result<serde_json::Value> {
    let r = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/json/list"))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await?;
    Ok(r.json().await?)
}

/// CDP over plain HTTP: open a new tab. Chrome 111+ requires PUT on
/// /json/new; older builds only accept GET — try PUT, fall back.
pub async fn cdp_new_tab(port: u16, url: &str) -> anyhow::Result<serde_json::Value> {
    let client = reqwest::Client::new();
    let endpoint = format!(
        "http://127.0.0.1:{port}/json/new?{}",
        serde_urlencoded_url(url)
    );
    let put = client
        .put(&endpoint)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;
    let resp = match put {
        Ok(r) if r.status().is_success() => r,
        _ => client
            .get(&endpoint)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await?,
    };
    Ok(resp.json().await?)
}

/// Minimal query-encoding for the one place we build a query string by hand
/// (reqwest's `query()` would double-encode an already-composed URL value).
fn serde_urlencoded_url(url: &str) -> String {
    let mut out = String::from("url=");
    for b in url.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---- CDP WebSocket client (AMUX-2598 cutover) -----------------------------
//
// The driver verbs (/navigate /screenshot /state /action /inspect /search)
// used to proxy to the Python server's browser engine; they now speak the
// DevTools protocol directly to the Chrome THIS process launched via
// [`start`]: JSON-RPC over the page target's local ws:// endpoint. One
// connection per API call — the protocol is stateless at our grain — and the
// only cross-call state lives either in this process (the session→tab map)
// or in the PAGE itself as an injected shim (console/network capture,
// element index list), exactly like the Python implementation
// (`_BROWSER_CAPTURE_JS` / `window.__amux`), so a dropped connection loses
// nothing a re-run cannot rebuild.

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};

/// A WebSocket connection to one CDP target.
pub struct CdpClient {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next_id: u64,
}

impl CdpClient {
    /// HARD INVARIANT (owner directive, AMUX-2598): browser automation
    /// executes on the SERVER machine, never in a dashboard-viewing client's
    /// browser. This client therefore connects exclusively to LOOPBACK CDP
    /// endpoints — the Chrome this process spawned advertises exactly those —
    /// and refuses anything else, so no code path can quietly grow a remote
    /// browser dependency.
    pub async fn connect(ws_url: &str) -> anyhow::Result<Self> {
        let host_ok = ws_url
            .strip_prefix("ws://")
            .and_then(|rest| rest.split('/').next())
            .map(|hp| {
                let host = hp.rsplit_once(':').map(|(h, _)| h).unwrap_or(hp);
                matches!(host, "127.0.0.1" | "localhost" | "[::1]")
            })
            .unwrap_or(false);
        if !host_ok {
            anyhow::bail!(
                "refusing non-loopback CDP endpoint {ws_url}: browser automation runs only \
                 against the server-machine Chrome this process launched"
            );
        }
        let (ws, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| anyhow::anyhow!("CDP websocket connect {ws_url}: {e}"))?;
        Ok(Self { ws, next_id: 0 })
    }

    /// One CDP command. Chrome interleaves EVENT messages on the same
    /// socket; anything without our id is skipped, and the deadline caps the
    /// whole exchange so a wedged page degrades to an error, not a hung
    /// handler. A CDP-level error surfaces with its message — never as an
    /// empty success.
    pub async fn call(
        &mut self,
        method: &str,
        params: Value,
        timeout: std::time::Duration,
    ) -> anyhow::Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let payload = json!({ "id": id, "method": method, "params": params }).to_string();
        let fut = async {
            use tokio_tungstenite::tungstenite::Message;
            self.ws.send(Message::Text(payload)).await?;
            loop {
                let Some(frame) = self.ws.next().await else {
                    anyhow::bail!("CDP websocket closed during {method}");
                };
                match frame? {
                    Message::Text(t) => {
                        let v: Value = serde_json::from_str(&t).map_err(|e| {
                            anyhow::anyhow!("CDP sent non-JSON during {method}: {e}")
                        })?;
                        if v.get("id").and_then(Value::as_u64) == Some(id) {
                            if let Some(err) = v.get("error") {
                                anyhow::bail!(
                                    "CDP {method}: {}",
                                    err.get("message").and_then(Value::as_str).unwrap_or("error")
                                );
                            }
                            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                        }
                        // No id match: a protocol event — not ours, keep reading.
                    }
                    Message::Close(_) => anyhow::bail!("CDP websocket closed during {method}"),
                    _ => {} // Ping/Pong/Binary: tungstenite answers pings itself.
                }
            }
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(r) => r,
            // TYPED, not just a message (AMUX-3672). The API boundary maps a
            // timeout to 504 and a protocol error to 502, and it must decide
            // that from a TYPE rather than by matching this string — the wording
            // here would then be load-bearing for a status code, which is the
            // fragile coupling ethos rule 7 keeps producing.
            Err(_) => Err(anyhow::Error::new(CdpTimeout {
                method: method.to_string(),
                secs: timeout.as_secs(),
            })),
        }
    }

    /// Runtime.evaluate with by-value results. A page exception is an ERROR
    /// carrying the page's own description, never a silent null — an eval
    /// that cannot fail reads as "the page had no answer" (ethos rule 7).
    pub async fn eval(&mut self, expr: &str, timeout_s: u64) -> anyhow::Result<Value> {
        let r = self
            .call(
                "Runtime.evaluate",
                json!({ "expression": expr, "returnByValue": true, "awaitPromise": true }),
                std::time::Duration::from_secs(timeout_s),
            )
            .await?;
        if let Some(ex) = r.get("exceptionDetails") {
            let desc = ex
                .pointer("/exception/description")
                .and_then(Value::as_str)
                .or_else(|| ex.get("text").and_then(Value::as_str))
                .unwrap_or("page threw");
            // Carried as a TYPE (AMUX-98), same technique as CdpTimeout right
            // below: a caller-supplied selector that is not valid CSS (e.g.
            // Playwright-style `text=...` syntax reaching a native
            // `document.querySelector`) throws HERE, inside the page — the
            // caller's mistake, not amux's. Every OTHER caller of `eval()`
            // (click_selector, the "wait" selector probe, ...) used to
            // propagate this as a bare anyhow error indistinguishable from a
            // real transport failure, so the API boundary answered 502 for a
            // malformed selector — "amux is broken" for what was actually a
            // well-formed refusal. Message text is UNCHANGED ("eval: {desc}"),
            // only the type changed, so this is not a wire-format break.
            return Err(anyhow::Error::new(PageException(desc.to_string())));
        }
        Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }
}

// ---- driver session → tab resolution --------------------------------------

/// Which CDP page tab each amux browser-session name operates on, inside the
/// one Chrome this server launched. AC-293's resolution order (explicit →
/// X-Amux-Session → "amux") happens in the API layer; this map is keyed by
/// the RESOLVED name. Two sessions never silently share a tab: an unbound
/// session binds only to a tab no OTHER session claims (the AC-293 incident
/// was two lanes driving one browser and diagnosing each other's pages).
pub static NATIVE_TARGETS: LazyLock<Mutex<std::collections::HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Why a driver verb cannot run. `NotRunning` is the caller's fixable state
/// (the API answers 409 + a pointer at /start); `Cdp` is the browser
/// misbehaving (502).
#[derive(Debug)]
pub enum DriverError {
    NotRunning,
    Cdp(anyhow::Error),
}

/// A CDP call that never answered (AMUX-3672).
///
/// Carried as a TYPE so the API boundary can answer 504 Gateway Timeout instead
/// of 502 Bad Gateway. Both used to be `Cdp`, so both became 502 — and
/// `/api/logs/analyze` groups by (status, method, target), which folded "the
/// browser is WEDGED" and "the browser REJECTED our call" into one group. Two
/// different faults, one row, and only the error body told them apart.
///
/// That is what made the 2026-08-24 15:57 report need its card body to diagnose:
/// `Emulation.setDeviceMetricsOverride timed out after 10s` was a symptom of
/// five Chromes contending one profile (AMUX-3674), and nothing in the log
/// grouping could have said "this one is a hang".
#[derive(Debug)]
pub struct CdpTimeout {
    pub method: String,
    pub secs: u64,
}

impl std::fmt::Display for CdpTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Unchanged wording: it is what the existing card bodies and the
        // AMUX-3207 notes quote, and a status code no longer depends on it.
        write!(f, "CDP {} timed out after {}s", self.method, self.secs)
    }
}

impl std::error::Error for CdpTimeout {}

/// A page-side exception from `Runtime.evaluate` (AMUX-98) — the SCRIPT
/// threw, most commonly because a caller-supplied selector interpolated
/// into a `document.querySelector(...)` call is not valid CSS (e.g. a
/// Playwright-style `text=...` selector, which native `querySelector` does
/// not understand and rejects with a `SyntaxError`). This is the caller's
/// mistake, same class as the NOELEMENT/NOTVISIBLE/STALE sentinels
/// `click_outcome` already maps to 400 — it just arrives as an exception
/// instead of a benign string return, because the malformed selector fails
/// before the click JS's own logic ever runs.
///
/// Carried as a TYPE, same technique as [`CdpTimeout`] right above (and for
/// the identical reason, AMUX-3672): the API boundary decides the status
/// code from the error's TYPE, never by matching its message text, so a
/// future rewording can't silently flip a 400 into a 502.
#[derive(Debug)]
pub struct PageException(pub String);

impl std::fmt::Display for PageException {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Unchanged wording ("eval: {desc}") from the old bail!() this
        // replaced — callers and tests that match on the message see no
        // difference; only the type is now inspectable.
        write!(f, "eval: {}", self.0)
    }
}

impl std::error::Error for PageException {}

/// The launch was DELEGATED, not broken: a Chrome already holding this
/// `--user-data-dir` accepted the URL and the new process exited 0 before CDP
/// bound. The caller can resolve it (stop the holder, or just retry), which is
/// the same class `DriverError::NotRunning` already answers 409 for.
///
/// TYPED FOR THE SAME REASON `PageException` IS (AF-381). `api/browser.rs`'s
/// `start` maps every `Err` to 502, so this arrived as "the upstream is broken,
/// your request was fine" while its own hint told the caller to go and fix it —
/// and a client that retries on 5xx retries a conflict that will not clear
/// itself. `cdp_status`'s rule applies verbatim: decide on the TYPE, never by
/// matching the message, or the status breaks the first time someone rephrases
/// the hint.
///
/// Display reproduces the `bail!()` string it replaces EXACTLY, so nothing that
/// quotes the message sees a difference; only the type is now inspectable.
#[derive(Debug)]
pub struct ProfileDelegated(pub String);

impl std::fmt::Display for ProfileDelegated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ProfileDelegated {}

/// A live, non-amux Chrome owns the user-data-dir needed for this profile.
///
/// Typed so the API can return a structured, non-retryable 409 without
/// message matching. Unlike [`ProfileDelegated`], this is detected BEFORE a
/// Chrome process is spawned, so `spawned:false` is a fact the caller can use.
#[derive(Debug)]
pub struct ExternalProfileInUse {
    pub profile: String,
    pub user_data_dir: PathBuf,
    pub owner_pid: u32,
}

impl std::fmt::Display for ExternalProfileInUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Chrome profile {:?} uses the human Chrome data directory {}, which is already \
             locked by live process {}. amux did not launch Chrome: Chrome would hand the URL \
             to that existing window, and every retry would open another tab. Do not retry \
             while this 409 persists. Fully quit Chrome (all windows) and retry once, or \
             enable remote debugging at chrome://inspect/#remote-debugging and use \
             /chrome-cdp to drive the live browser.",
            self.profile,
            self.user_data_dir.display(),
            self.owner_pid
        )
    }
}

impl std::error::Error for ExternalProfileInUse {}

/// Is a Chrome that exited BEFORE CDP bound the delegation signature?
///
/// Exit 0 means it handed its URL to an instance already holding the profile
/// and quit cleanly; any other code means the launch genuinely failed. One line,
/// extracted only so it can be PINNED: mutating the construction site to never
/// build `ProfileDelegated` survived the whole suite, because the classifier had
/// a test and the decision feeding it did not (AF-381, and the same seam class
/// as AF-438). The launch loop needs a real Chrome to exercise; this does not.
pub fn is_delegation_exit(status: &std::process::ExitStatus) -> bool {
    status.success()
}

impl From<anyhow::Error> for DriverError {
    fn from(e: anyhow::Error) -> Self {
        DriverError::Cdp(e)
    }
}

/// The resolved page a driver verb runs against.
pub struct DriverPage {
    pub cdp_port: u16,
    pub target_id: String,
    pub ws_url: String,
    pub url: String,
    pub title: String,
}

/// Resolve the page for a session: its bound tab if still alive, else the
/// first live page tab no other session claims (bound as a side effect),
/// else a fresh tab on `create_url` (default about:blank). The port comes
/// from [`RUNNING`] only — verbs NEVER attach to a browser this process did
/// not launch (a human's Chrome is not ours to drive).
pub async fn resolve_page(session: &str, create_url: Option<&str>) -> Result<DriverPage, DriverError> {
    resolve_page_avoiding(session, create_url, None).await
}

/// How many times a bound target was abandoned because its socket would not
/// open (AMUX-3708). Read by `GET /api/browser/status`.
pub static STALE_BINDING_RECOVERIES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The registry row for a CDP port: (profile, started_by, pid).
///
/// READS THE STRUCT BY FIELD NAME, deliberately, rather than going through
/// `running_all()`. That helper returns a positional tuple which peers widen as
/// the reaper grows — it gained a sixth element on 2026-08-30, mid-edit, and
/// every destructuring call site broke at once for a field none of them wanted.
/// Naming the three fields this path actually needs makes the next widening a
/// non-event here.
fn registry_row_for_port(port: u16) -> Option<(String, String, u32)> {
    RUNNING
        .lock()
        .expect("browser registry poisoned")
        .values()
        .find(|r| r.cdp_port == port)
        .map(|r| (r.profile.clone(), r.started_by.clone(), r.pid))
}

/// How many times a verb found the registry naming a browser whose PROCESS is
/// gone (AMUX-3886). Read by `GET /api/browser/status`.
pub static DEAD_BROWSER_RECOVERIES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Is `pid` a currently-live process? `kill(pid, 0)` sends no signal but
/// performs the existence+permission check — Ok/EPERM means alive, ESRCH means
/// gone.
fn checked_process_id(pid: u32) -> Option<i32> {
    let pid = i32::try_from(pid).ok()?;
    (pid > 1).then_some(pid)
}

fn persisted_process_id(v: &serde_json::Value) -> Option<u32> {
    let raw = v.get("pid")?.as_u64()?;
    let pid = u32::try_from(raw).ok()?;
    checked_process_id(pid)?;
    Some(pid)
}

/// Construct arguments that can signal one process, never a POSIX process-group
/// sentinel. `None` means the caller MUST return before spawning `/bin/kill`.
///
/// The WARN is the two-fix signal for the CI-killing ATE-44 failure: the next
/// malformed persisted pid or unsafe test sentinel is visible in the normal
/// server log instead of being parsed by an external utility and broadcast.
fn checked_kill_args(
    pid: u32,
    signal: &'static str,
    operation: &'static str,
) -> Option<[String; 2]> {
    let Some(pid) = checked_process_id(pid) else {
        tracing::warn!(
            pid,
            signal,
            operation,
            verdict = "invalid_process_id_refused",
            "browser: refused to signal an unsafe process id; no OS signal was sent"
        );
        return None;
    };
    Some([signal.to_string(), pid.to_string()])
}

#[cfg(test)]
tokio::task_local! {
    static TEST_KILL_COMMANDS: std::cell::RefCell<Vec<[String; 2]>>;
}

/// The only external-signal launch point. Invalid PIDs return before both the
/// test observer and `Command::new`, so the reaper regression can prove that
/// its unsigned sentinel never invoked kill at all.
async fn run_kill(
    pid: u32,
    signal: &'static str,
    operation: &'static str,
) -> Option<std::io::Result<std::process::ExitStatus>> {
    let args = checked_kill_args(pid, signal, operation)?;
    #[cfg(test)]
    let _ = TEST_KILL_COMMANDS.try_with(|commands| commands.borrow_mut().push(args.clone()));
    Some(tokio::process::Command::new("kill").args(args).status().await)
}

#[cfg(test)]
pub async fn test_with_kill_capture<F>(future: F) -> F::Output
where
    F: std::future::Future,
{
    TEST_KILL_COMMANDS.scope(std::cell::RefCell::new(Vec::new()), future).await
}

#[cfg(test)]
pub fn test_kill_commands() -> Vec<[String; 2]> {
    TEST_KILL_COMMANDS.try_with(|commands| commands.borrow().clone()).unwrap_or_default()
}

#[cfg(test)]
pub async fn test_normal_pid_reaches_kill(pid: u32) -> bool {
    matches!(run_kill(pid, "-0", "normal pid control").await, Some(Ok(status)) if status.success())
}

fn pid_alive(pid: u32) -> bool {
    let Some(pid) = checked_process_id(pid) else {
        tracing::warn!(
            pid,
            signal = 0,
            operation = "browser liveness probe",
            verdict = "invalid_process_id_refused",
            "browser: refused to probe an unsafe process id; no OS signal was sent"
        );
        return false;
    };
    // Signal 0 performs the existence+permission check without affecting the
    // target. EPERM still proves the process exists.
    // SAFETY: `checked_process_id` admits only a positive non-special signed
    // pid, never 0 or -1, so this cannot target a process group.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// What a `/json/list` failure on a registered port MEANS. Pure on purpose.
///
/// PURE FOR THE SAME REASON AS `choose_target`. The effectful version has to
/// clear the registry, rewrite the running-file under `~/.amux` and record a
/// corpse; a test that drove it would reach the REAL `~/.amux` and could clear
/// a live lane's browser (AF-111 is exactly this hazard). Splitting the
/// decision out is what makes the check runnable in CI at all, rather than an
/// `#[ignore]`d live test that never fails because it never executes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CdpListVerdict {
    /// The registry no longer names this port — a stop raced us. Nothing to
    /// clear, no pid to judge; report the CDP fault with its cause.
    Unregistered,
    /// The process is ALIVE and its CDP port did not answer: wedged or
    /// refusing, not gone. A real 502, and the pid is what makes it diagnosable.
    Wedged { pid: u32, profile: String },
    /// The process is GONE while the registry still named it. The caller's
    /// fixable state, not amux failing.
    Gone { pid: u32, profile: String, started_by: String },
}

/// The rule, separated from its side effects. `entry` is the registry row for
/// the port (profile, started_by, pid), `alive` its `kill -0` result.
pub(crate) fn classify_cdp_list_failure(
    entry: Option<(String, String, u32)>,
    alive: bool,
) -> CdpListVerdict {
    match entry {
        None => CdpListVerdict::Unregistered,
        Some((profile, _, pid)) if alive => CdpListVerdict::Wedged { pid, profile },
        Some((profile, started_by, pid)) => CdpListVerdict::Gone { pid, profile, started_by },
    }
}

/// `/json/list` failed on a port the registry says is ours. SAY WHICH FAULT
/// THIS IS before handing it back.
///
/// AMUX-3886. Two 502s from `general-canvas-apps` on 2026-08-29 carried
/// `error sending request for url (http://127.0.0.1:49731/json/list)` and
/// nothing else, and the server log had NO line for either — the whole
/// `cdp_list` failure path was silent. Three different faults reach this point
/// and the old code could not tell them apart, because the one string it
/// produced is identical for a refused connect, a DNS failure and a timeout.
///
/// A GONE process must not wear a 5xx: that is the caller's fixable state
/// (start a browser), which is the card's own instruction — "if this is really
/// a refusal, the fix is the STATUS CODE, not the caller". The corpse is
/// recorded exactly as `adopt_one` records it, and the registry cleared, so the
/// next verb says `NotRunning` instead of retrying a dead port.
///
/// The exit monitor polls every 5s and would eventually notice a dead pid; this
/// runs on the REQUEST, so the caller that hits the gap gets the answer rather
/// than a 502 that reads as a mystery.
async fn cdp_list_failure(session: &str, port: u16, e: anyhow::Error) -> DriverError {
    let entry = registry_row_for_port(port);
    let alive = entry.as_ref().is_some_and(|(_, _, pid)| pid_alive(*pid));
    match classify_cdp_list_failure(entry, alive) {
        CdpListVerdict::Unregistered => {
            tracing::warn!(
                session, port, cause = %format!("{e:#}"),
                "browser: /json/list failed on a port no longer in the registry (AMUX-3886)"
            );
            DriverError::Cdp(e)
        }
        CdpListVerdict::Wedged { pid, profile } => {
            tracing::warn!(
                session, port, pid, profile, cause = %format!("{e:#}"),
                "browser: Chrome is ALIVE but its CDP port did not answer — wedged or refusing, \
                 not gone; the registry is left intact (AMUX-3886)"
            );
            DriverError::Cdp(e.context(format!(
                "Chrome pid {pid} (profile {profile:?}) is ALIVE but its CDP port {port} did not \
                 answer /json/list, so the browser is wedged or refusing rather than gone. \
                 POST /api/browser/stop then /api/browser/start to replace it"
            )))
        }
        CdpListVerdict::Gone { pid, profile, started_by } => {
            tracing::warn!(
                session, port, pid, profile, started_by = %started_by, cause = %format!("{e:#}"),
                "browser: the registry named a browser whose PROCESS IS GONE — recording \
                 last_exit, clearing the registry, and answering the caller with the fixable \
                 state instead of a 502 (AMUX-3886)"
            );
            *LAST_EXIT.lock().expect("last-exit poisoned") = Some(serde_json::json!({
                "ts": chrono::Utc::now().timestamp(),
                "reason": "found dead on a verb (the registry still named it; CDP and the pid are both gone)",
                "profile": profile,
                "pid": pid,
                "started_by": started_by,
                "code": serde_json::Value::Null,
                "signal": serde_json::Value::Null,
            }));
            RUNNING.lock().expect("browser registry poisoned").remove(&profile);
            NATIVE_TARGETS.lock().expect("native targets poisoned").remove(session);
            clear_running_for(&amux_home(), &profile);
            DEAD_BROWSER_RECOVERIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            DriverError::NotRunning
        }
    }
}

#[cfg(test)]
mod cdp_list_failure_tests {
    use super::*;

    /// THE DEFECT AMUX-3886 IS ABOUT, pinned against a real socket.
    ///
    /// `to_string()` on the error `cdp_list` returns prints the outermost frame
    /// only. That frame names the URL and NOTHING about the fault, so a refused
    /// connect, a DNS failure and a timeout all render as the same bytes — which
    /// is why two 502s on 2026-08-29 were undiagnosable four hours later.
    ///
    /// Asserts BOTH directions on purpose. Without the negative arm this test
    /// would still pass if `{:#}` silently started behaving like `{}`, and the
    /// whole fix would be gone with the check still green.
    #[tokio::test]
    async fn cdp_list_error_keeps_its_cause_only_in_the_alternate_form() {
        // Bind then drop, so the port is certainly closed and the connect is
        // REFUSED rather than timing out (the 2026-08-29 samples answered in
        // ~1ms, which is a refusal, not the 3s timeout).
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = l.local_addr().expect("addr").port();
        drop(l);

        let e = cdp_list(port).await.expect_err("a closed port must fail");
        let plain = e.to_string();
        let with_cause = format!("{e:#}");

        assert!(
            !plain.to_lowercase().contains("refused"),
            "the CONTROL has stopped holding: `to_string()` now carries the cause, so this test \
             no longer proves anything. Got: {plain}"
        );
        assert!(
            with_cause.to_lowercase().contains("refused"),
            "the alternate form must carry the cause that separates a refusal from a timeout. \
             Got: {with_cause}"
        );
        assert!(
            with_cause.len() > plain.len(),
            "cause chain must ADD to the message: plain={plain:?} alt={with_cause:?}"
        );
    }

    /// A GONE process is the caller's fixable state, not amux failing.
    #[test]
    fn a_gone_process_is_not_running_and_a_live_one_is_wedged() {
        let entry = || Some(("default".to_string(), "some-lane".to_string(), 4242u32));

        assert_eq!(
            classify_cdp_list_failure(entry(), false),
            CdpListVerdict::Gone {
                pid: 4242,
                profile: "default".into(),
                started_by: "some-lane".into()
            },
            "pid gone => the registry is stale; the caller can fix it by starting a browser"
        );
        // CONTROL: same registry row, live pid. A change that answered `Gone`
        // for everything would pass the assertion above and clear a HEALTHY
        // browser out from under its owner.
        assert_eq!(
            classify_cdp_list_failure(entry(), true),
            CdpListVerdict::Wedged { pid: 4242, profile: "default".into() },
            "a live pid must never be swept as a corpse"
        );
        // A stop that raced us leaves no row to judge.
        assert_eq!(classify_cdp_list_failure(None, false), CdpListVerdict::Unregistered);
        assert_eq!(classify_cdp_list_failure(None, true), CdpListVerdict::Unregistered);
    }

    /// `pid_alive` must actually discriminate, or the classifier above is fed a
    /// constant and every arm of it is decoration.
    #[test]
    fn pid_alive_separates_this_process_from_a_reaped_one() {
        assert!(pid_alive(std::process::id()), "this very process must read as alive");
        // A child we spawn and reap: its pid is genuinely gone, and unlike a
        // made-up number it cannot accidentally belong to something else.
        let mut c = std::process::Command::new("true").spawn().expect("spawn");
        let pid = c.id();
        c.wait().expect("wait");
        assert!(!pid_alive(pid), "a reaped child (pid {pid}) must read as gone");
    }

    /// The exact Linux runner-kill boundary from ATE-44. procps accepted
    /// 4294967295 and passed it to the signed pid API as -1, which means every
    /// signalable process except the caller. Keep both group sentinels out while
    /// proving an ordinary positive process id still passes.
    #[test]
    fn process_id_validation_rejects_group_sentinels_and_unsigned_wrap() {
        for invalid in [0, 1, (i32::MAX as u32) + 1, u32::MAX] {
            assert_eq!(checked_process_id(invalid), None, "accepted invalid pid {invalid}");
            assert_eq!(
                checked_kill_args(invalid, "-TERM", "test"),
                None,
                "pid {invalid} reached /bin/kill argument construction"
            );
        }
        assert_eq!(checked_process_id(2), Some(2), "ordinary positive pid must remain valid");
        assert_eq!(
            checked_kill_args(42, "-TERM", "test"),
            Some(["-TERM".to_string(), "42".to_string()]),
            "a normal pid must still reach the one-process signal path"
        );
        assert_eq!(
            checked_process_id(i32::MAX as u32),
            Some(i32::MAX),
            "the top of the signed pid range is still one process, not a group"
        );
    }

    #[test]
    fn persisted_state_cannot_create_a_running_browser_with_an_unsafe_pid() {
        for invalid in [
            serde_json::json!({"pid": 0}),
            serde_json::json!({"pid": 1}),
            serde_json::json!({"pid": (i32::MAX as u64) + 1}),
            serde_json::json!({"pid": u32::MAX}),
            serde_json::json!({"pid": u64::MAX}),
            serde_json::json!({"pid": "42"}),
        ] {
            assert_eq!(persisted_process_id(&invalid), None, "accepted {invalid}");
        }
        assert_eq!(persisted_process_id(&serde_json::json!({"pid": 42})), Some(42));
    }
}

/// `resolve_page`, but refusing to hand back `avoid`.
///
/// AMUX-3708. The bound-tab branch below already falls through when the tab is
/// GONE from `/json/list`, which covers a closed or crashed tab. It does not
/// cover the state this card is about: a tab Chrome still LISTS whose renderer
/// is wedged, so the target resolves fine and the WebSocket connect is reset
/// without a closing handshake. Two 502s 40s apart on 2026-08-25 were exactly
/// that pair — `Page.captureScreenshot timed out after 30s` at 11:12:41 leaving
/// the tab wedged, then `Connection reset without closing handshake` in 24ms at
/// 11:13:21 when the next request resolved to the same still-listed target.
/// Recovery arrived at 11:13:25 only because Chrome had by then removed the
/// target, so the existing fall-through could finally see it. Every request in
/// that window 502s, and nothing shortens it.
///
/// `avoid` closes the window: the caller that just failed to open a socket
/// knows which target is bad, which is knowledge `/json/list` does not have.
/// The binding is dropped too, or the next call would resolve straight back to
/// it.
pub async fn resolve_page_avoiding(
    session: &str,
    create_url: Option<&str>,
    avoid: Option<&str>,
) -> Result<DriverPage, DriverError> {
    let Some(port) = port_for_session(session) else {
        return Err(DriverError::NotRunning);
    };
    let tabs_v = match cdp_list(port).await {
        Ok(v) => v,
        Err(e) => return Err(cdp_list_failure(session, port, e).await),
    };
    let empty = vec![];
    let tabs = tabs_v.as_array().unwrap_or(&empty);
    let page_of = |t: &Value| -> Option<DriverPage> {
        Some(DriverPage {
            cdp_port: port,
            target_id: t.get("id")?.as_str()?.to_string(),
            ws_url: t.get("webSocketDebuggerUrl")?.as_str()?.to_string(),
            url: t.get("url").and_then(Value::as_str).unwrap_or("").to_string(),
            title: t.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
        })
    };
    let tab_id = |t: &Value| t.get("id").and_then(Value::as_str).map(str::to_string);

    let (bound, claimed_by_others): (Option<String>, std::collections::HashSet<String>) = {
        let map = NATIVE_TARGETS.lock().expect("native targets poisoned");
        (
            map.get(session).cloned(),
            map.iter().filter(|(k, _)| k.as_str() != session).map(|(_, v)| v.clone()).collect(),
        )
    };
    let (choice, binding_was_stale) =
        choose_target(tabs, bound.as_deref(), &claimed_by_others, avoid);
    if binding_was_stale {
        // The caller just failed to open a socket on this exact target, so the
        // binding is stale however healthy `/json/list` still claims it is. Drop
        // it here rather than at the call site: leaving it would make the very
        // next request resolve straight back to the tab we are abandoning.
        NATIVE_TARGETS.lock().expect("native targets poisoned").remove(session);
        STALE_BINDING_RECOVERIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            "[browser] session {session:?} unbound from wedged target {}: Chrome still lists it \
             but its CDP socket would not open (AMUX-3708); rebinding",
            bound.as_deref().unwrap_or("?")
        );
    }
    match choice {
        TargetChoice::KeepBound(tid) | TargetChoice::Rebind(tid) => {
            if let Some(p) = tabs.iter().find(|t| tab_id(t).as_deref() == Some(tid.as_str())).and_then(page_of)
            {
                NATIVE_TARGETS
                    .lock()
                    .expect("native targets poisoned")
                    .insert(session.to_string(), p.target_id.clone());
                return Ok(p);
            }
            // Listed but missing a webSocketDebuggerUrl: fall through and open one.
        }
        TargetChoice::NewTab => {}
    }
    // Every page tab is claimed by another session (or none exist): open a
    // fresh one rather than hijacking a peer's tab.
    let t = cdp_new_tab(port, create_url.unwrap_or("about:blank")).await.map_err(DriverError::Cdp)?;
    let p = page_of(&t).ok_or_else(|| {
        DriverError::Cdp(anyhow::anyhow!("CDP /json/new returned no webSocketDebuggerUrl: {t}"))
    })?;
    NATIVE_TARGETS
        .lock()
        .expect("native targets poisoned")
        .insert(session.to_string(), p.target_id.clone());
    Ok(p)
}

/// What `resolve_page_avoiding` should do, given only what Chrome listed and
/// what this process has bound.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TargetChoice {
    /// Reuse the session's existing binding.
    KeepBound(String),
    /// Bind the session to this listed tab.
    Rebind(String),
    /// Nothing usable is listed; open a fresh tab.
    NewTab,
}

/// The target-selection rule, separated from its side effects.
///
/// PURE ON PURPOSE (AMUX-3708). Every existing test of this area launches a
/// real Chrome and is `#[ignore]`d, so it never runs in CI — a live-only test
/// for this fix would be a check that cannot fail because it never executes,
/// which is the exact shape ethos rule 7 is about. Extracting the decision is
/// what makes the fix testable at all; unbinding, counting and opening tabs
/// stay in the caller.
///
/// Returns the choice and whether the session's binding was found STALE, i.e.
/// it names the target the caller has just proven unreachable.
pub(crate) fn choose_target(
    tabs: &[Value],
    bound: Option<&str>,
    claimed_by_others: &std::collections::HashSet<String>,
    avoid: Option<&str>,
) -> (TargetChoice, bool) {
    let id_of = |t: &Value| t.get("id").and_then(Value::as_str).map(str::to_string);
    let listed = |tid: &str| {
        tabs.iter().any(|t| {
            id_of(t).as_deref() == Some(tid) && t.get("webSocketDebuggerUrl").is_some()
        })
    };
    let stale = bound.is_some() && bound == avoid;
    let blank_url = |u: &str| u.is_empty() || u == "about:blank" || u.starts_with("chrome://");
    let is_blank_tab = |tid: &str| {
        tabs.iter().any(|t| {
            id_of(t).as_deref() == Some(tid)
                && blank_url(t.get("url").and_then(Value::as_str).unwrap_or(""))
        })
    };
    let real_page_available = tabs.iter().any(|t| {
        t.get("type").and_then(Value::as_str) == Some("page")
            && t.get("webSocketDebuggerUrl").is_some()
            && !blank_url(t.get("url").and_then(Value::as_str).unwrap_or(""))
            && id_of(t).is_some_and(|id| !claimed_by_others.contains(&id))
            && (avoid.is_none() || id_of(t).as_deref() != avoid)
    });
    if let Some(tid) = bound {
        // A bound tab that is GONE from /json/list was already handled before
        // this card: it falls through and rebinds. The case this adds is a tab
        // still LISTED whose socket will not open, which /json/list reports
        // identically to a healthy one — only the failing caller knows.
        //
        // A BLANK BINDING DOES NOT SURVIVE A REAL PAGE (AMUX-3834, second pass).
        // The first fix taught the REBIND to prefer a real page, which stops the
        // drift happening — and does nothing for a session ALREADY stuck on a
        // blank tab, because a blank tab is listed and this arm kept it forever.
        // tubescience was right to walk back the fix: their binding was already
        // stuck, so preventing future drift could not help them, and I confirmed
        // it here — screenshot and eval BOTH returned about:blank on this
        // machine, agreeing with each other and both wrong.
        //
        // So: keep a blank binding only while there is nothing better. That
        // makes a stuck session self-heal on its next verb rather than needing a
        // navigate to break it out.
        if !stale && listed(tid) && !(is_blank_tab(tid) && real_page_available) {
            return (TargetChoice::KeepBound(tid.to_string()), false);
        }
    }
    // PREFER A REAL PAGE OVER A BLANK ONE (AMUX-3834, tubescience).
    //
    // This used to take the FIRST listed page, and `/json/list` order is
    // Chrome's, not ours. So when a bound tab died while a fresh `about:blank`
    // was open — a new-tab page, a popup, an opener Chrome spawned — the rebind
    // took the blank one, and because a blank tab IS listed the `KeepBound` arm
    // above then held the session there permanently. Screenshot and eval both
    // read that binding and both showed a blank page; only `navigate` escaped,
    // because it passes a URL and re-targets. That asymmetry is what made this
    // look like "screenshot resolves differently from action" when in fact every
    // verb shared one drifted binding.
    //
    // The cost of the old behaviour was not a blank screenshot. It was a
    // screenshot that LOOKS like evidence: tubescience cross-checked with eval
    // and caught it, and someone who did not would have concluded their UI
    // change did not render.
    //
    // A PREFERENCE, NOT A FILTER. If the only tab is blank, bind to it — the
    // alternative is opening tabs forever when blank is genuinely all there is.
    let is_blank = |t: &Value| {
        let u = t.get("url").and_then(Value::as_str).unwrap_or("");
        u.is_empty() || u == "about:blank" || u.starts_with("chrome://")
    };
    let usable = |t: &&Value| {
        t.get("type").and_then(Value::as_str) == Some("page")
            && id_of(t).is_some_and(|id| !claimed_by_others.contains(&id))
            // Never rebind to the target the caller just told us is
            // unreachable. Dropping the binding makes it "unclaimed", so
            // without this the rebind picks straight back up the tab we are
            // abandoning.
            && (avoid.is_none() || id_of(t).as_deref() != avoid)
            && t.get("webSocketDebuggerUrl").is_some()
    };
    let pick = tabs
        .iter()
        .find(|t| usable(t) && !is_blank(t))
        .or_else(|| tabs.iter().find(usable))
        .and_then(id_of);
    match pick {
        Some(tid) => (TargetChoice::Rebind(tid), stale),
        None => (TargetChoice::NewTab, stale),
    }
}

/// WHICH BROWSER A SESSION MEANS (AMUX-3828).
///
/// 1. The one it STARTED. That is what a lane means by "my browser", and it is
///    what makes two workers on two profiles independent without either naming
///    a profile on every verb.
/// 2. Otherwise, if exactly one is running, that one — an unambiguous situation
///    should not require a parameter.
/// 3. Otherwise None. With several browsers and no claim on any of them, a
///    guess would drive somebody else's page; the caller must name a profile.
///
/// Ethan's requirement is BOTH shapes: workers on DIFFERENT browsers are served
/// by (1), and workers SHARING one are served by (2) — the second worker did
/// not start it, exactly one is up, so it drives the same browser rather than
/// being refused.
pub fn port_for_session(session: &str) -> Option<u16> {
    let g = RUNNING.lock().expect("browser registry poisoned");
    if !session.is_empty() {
        if let Some(r) = g.values().find(|r| r.started_by == session) {
            return Some(r.cdp_port);
        }
    }
    if g.len() == 1 {
        return g.values().next().map(|r| r.cdp_port);
    }
    None
}

/// Every running browser's CDP port.
pub fn all_ports() -> Vec<u16> {
    RUNNING.lock().expect("browser registry poisoned").values().map(|r| r.cdp_port).collect()
}

#[cfg(test)]
mod multi_browser_tests {
    use super::*;

    /// ETHAN'S ACCEPTANCE CRITERION, 2026-08-28 12:50: "make sure multiple
    /// workers can use same and different browsers in amux that should be a
    /// test verified criteria."
    ///
    /// BOTH shapes, because they are different mechanisms:
    ///   DIFFERENT browsers — two profiles coexist in the registry, and each
    ///     worker's verbs resolve to its OWN browser. This is what the
    ///     single-slot registry made impossible.
    ///   SAME browser — a second worker that started nothing still resolves to
    ///     the running one, so two workers share it rather than one being
    ///     refused. That is the case the takeover refusal used to eat.
    ///
    /// Uses the registry seam rather than launching Chrome: the property under
    /// test is the SELECTION rule, and a test that needed two real Chromes
    /// would not run in CI.
    /// ONE TEST, NOT TWO, DELIBERATELY. `RUNNING` is process-global, so two
    /// #[test] fns exercising it run concurrently under the default harness and
    /// clobber each other's seeds — which is exactly how the first draft of
    /// this failed: "two profiles must coexist" saw three, and "a profile is a
    /// slot" saw a neighbour's entry. Serial by construction beats a flake.
    #[test]
    fn multiple_workers_can_use_same_and_different_browsers() {
        test_clear_running();
        test_seed_running_port("alpha", "worker-a", 111, 9001);
        test_seed_running_port("beta", "worker-b", 222, 9002);

        // DIFFERENT BROWSERS: both coexist, and each worker gets its OWN.
        assert_eq!(running_all().len(), 2, "two profiles must coexist");
        let a = port_for_session("worker-a").expect("worker-a has a browser");
        let b = port_for_session("worker-b").expect("worker-b has a browser");
        assert_ne!(a, b, "two workers on two profiles must not share a port");
        assert_eq!(running_snapshot_for("alpha").map(|x| x.1), Some("worker-a".into()));
        assert_eq!(running_snapshot_for("beta").map(|x| x.1), Some("worker-b".into()));

        // A third worker owning nothing, with TWO running: ambiguous, so it
        // must name a profile rather than be pointed at someone else's page.
        assert_eq!(port_for_session("worker-c"), None, "ambiguous must not guess");

        // SAME BROWSER: with exactly one running, a worker that started nothing
        // still resolves to it — sharing, not refusal.
        test_clear_running();
        test_seed_running_port("shared", "worker-a", 111, 9003);
        let owner = port_for_session("worker-a").expect("the owner resolves");
        let guest = port_for_session("worker-b").expect("a guest resolves to the same one");
        assert_eq!(owner, guest, "two workers must be able to drive ONE browser");

        // THE INVARIANT THE PROFILE KEY EXISTS FOR: never two entries for one
        // profile, because two Chromes on one user_data_dir corrupt it.
        test_clear_running();
        test_seed_running_port("same", "worker-a", 111, 9004);
        test_seed_running_port("same", "worker-b", 222, 9005);
        assert_eq!(running_all().len(), 1, "a profile is a slot, not a list");
        assert_eq!(running_snapshot_for("same").map(|x| x.3), Some(222), "last write replaces");

        // CONTROL: with nothing running nobody resolves — the rule must not
        // manufacture a browser, or "not running" becomes an invalid port.
        test_clear_running();
        assert_eq!(port_for_session("worker-a"), None);
        assert!(running_all().is_empty());
    }
}

/// Session bindings for GET /api/browser/sessions: (name, target, alive?).
pub async fn session_bindings() -> Vec<(String, String, bool)> {
    // UNION ACROSS EVERY BROWSER (AMUX-3828). A binding is live if its target
    // exists in ANY running browser; scoping to one would report a worker's
    // perfectly good tab as dead because a different worker's browser was
    // consulted.
    let mut live: std::collections::HashSet<String> = Default::default();
    for p in all_ports() {
        if let Ok(v) = cdp_list(p).await {
            if let Some(a) = v.as_array() {
                live.extend(
                    a.iter().filter_map(|t| t.get("id").and_then(Value::as_str).map(str::to_string)),
                );
            }
        }
    }
    let map = NATIVE_TARGETS.lock().expect("native targets poisoned").clone();
    let mut out: Vec<(String, String, bool)> =
        map.into_iter().map(|(name, tid)| (name, tid.clone(), live.contains(&tid))).collect();
    out.sort();
    out
}

// ---- observation caps (Python `_obs_cap`, D4: policy lives in env) --------

pub fn obs_eval_cap() -> usize {
    std::env::var("AMUX_OBS_EVAL_CAP").ok().and_then(|v| v.parse().ok()).unwrap_or(8000)
}

pub fn obs_state_cap() -> usize {
    std::env::var("AMUX_OBS_STATE_CAP").ok().and_then(|v| v.parse().ok()).unwrap_or(24000)
}

/// Python `_obs_cap`: truncate AND say so — silent truncation reads as the
/// page ending there. Char-boundary safe (a byte slice through a multibyte
/// char panics where Python's slice would not).
pub fn obs_cap(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let cut: String = text.chars().take(limit).collect();
    format!(
        "{cut}\n…[truncated at {limit} chars — page stays live; re-query narrower or reopen via the url pointer]"
    )
}

/// AMUX-3062: how much a cap will cut, for a STRUCTURED envelope signal. Returns
/// `Some(full_char_len)` when `text` exceeds `limit`, `None` when it fits. The
/// same predicate `obs_cap` truncates on (`chars > limit`), factored out so a
/// caller can set `truncated: true` + the original length on the response.
/// `obs_cap` appends a human notice INTO the string; that is easy to miss when
/// the envelope still reads `{ok:true}`, which is exactly how a truncated eval
/// read as success (a 10589-char page returned 23 of 34 records and would have
/// passed review). This is the half a run or reviewer checks programmatically.
pub fn obs_truncation(text: &str, limit: usize) -> Option<usize> {
    let full = text.chars().count();
    (full > limit).then_some(full)
}

// ---- driver JS blobs (ported from amux-server.py) -------------------------

/// Python `_BROWSER_CAPTURE_JS` verbatim: idempotent page-side capture shim
/// mirroring console.*, fetch, XHR and window errors into ring buffers on
/// `window.__amux`. Injected after every native /navigate and on first
/// /inspect, so capture state survives our stateless per-call connections.
pub const CAPTURE_JS: &str = r#"
(function(){
  if (window.__amux && window.__amux.__installed) return 'already';
  var CAP = 500;
  var S = window.__amux = window.__amux || {};
  S.console = S.console || []; S.net = S.net || []; S.errors = S.errors || [];
  S.__installed = true;
  var now = function(){ return Date.now(); };
  function push(a, x){ a.push(x); if (a.length > CAP) a.shift(); }
  function fmt(a){
    try {
      if (a instanceof Error) return a.stack || (a.name + ': ' + a.message);
      if (typeof a === 'object' && a !== null) { try { return JSON.stringify(a); } catch(e){ return String(a); } }
      return String(a);
    } catch(e){ return '[unserializable]'; }
  }
  ['log','info','warn','error','debug'].forEach(function(level){
    var orig = console[level]; if (!orig) return;
    console[level] = function(){
      try { push(S.console, { level: level, text: [].map.call(arguments, fmt).join(' '), ts: now() }); } catch(e){}
      return orig.apply(console, arguments);
    };
  });
  window.addEventListener('error', function(e){
    push(S.errors, { text: (e.message || 'error') + (e.filename ? (' @ ' + e.filename + ':' + e.lineno + ':' + e.colno) : ''),
                     stack: (e.error && e.error.stack) || '', ts: now() });
  });
  window.addEventListener('unhandledrejection', function(e){
    var r = e.reason; push(S.errors, { text: 'unhandledrejection: ' + ((r && r.message) || fmt(r)), stack: (r && r.stack) || '', ts: now() });
  });
  if (window.fetch) {
    var of = window.fetch;
    window.fetch = function(input, init){
      var url = (typeof input === 'string') ? input : (input && input.url) || '';
      var method = (init && init.method) || (input && input.method) || 'GET';
      var t0 = now();
      return of.apply(this, arguments).then(function(res){
        push(S.net, { type:'fetch', method:method, url:url, status:res.status, ok:res.ok, ms: now()-t0, ts: t0 }); return res;
      }, function(err){
        push(S.net, { type:'fetch', method:method, url:url, status:0, ok:false, error:String(err), ms: now()-t0, ts: t0 }); throw err;
      });
    };
  }
  var OX = window.XMLHttpRequest;
  if (OX) {
    var NX = function(){
      var xhr = new OX();
      var rec = { type:'xhr', method:'GET', url:'', status:0, ok:false, ms:0, ts: now() };
      var open = xhr.open;
      xhr.open = function(m, u){ rec.method = m; rec.url = u; return open.apply(xhr, arguments); };
      var send = xhr.send;
      xhr.send = function(){ var t0 = now();
        xhr.addEventListener('loadend', function(){ rec.status = xhr.status; rec.ok = (xhr.status >= 200 && xhr.status < 400); rec.ms = now()-t0; push(S.net, rec); });
        return send.apply(xhr, arguments); };
      return xhr;
    };
    NX.prototype = OX.prototype; window.XMLHttpRequest = NX;
  }
  return 'installed';
})()
"#;

/// Python `_BROWSER_INSPECT_JS` verbatim (placeholders `__LIMIT__` /
/// `__CLEAR__` substituted by [`inspect_js`]): read the capture buffers plus
/// the Resource Timing back-fill, which lists requests that fired before the
/// shim was installed.
const INSPECT_JS: &str = r#"
(function(){
  var S = window.__amux || {};
  var L = __LIMIT__;
  function tail(a){ a = a || []; return a.slice(Math.max(0, a.length - L)); }
  var res = [];
  try {
    res = performance.getEntriesByType('resource').slice(-L).map(function(e){
      return { url:e.name, type:e.initiatorType, ms:Math.round(e.duration), size:e.transferSize||0, start:Math.round(e.startTime) };
    });
  } catch(e){}
  var out = { url: location.href, title: document.title, installed: !!S.__installed,
    console: tail(S.console), network: tail(S.net), errors: tail(S.errors), resources: res,
    counts: { console:(S.console||[]).length, network:(S.net||[]).length, errors:(S.errors||[]).length, resources:res.length } };
  if (__CLEAR__) { if (S.console) S.console.length = 0; if (S.net) S.net.length = 0; if (S.errors) S.errors.length = 0; }
  return out;
})()
"#;

pub fn inspect_js(limit: usize, clear: bool) -> String {
    INSPECT_JS
        .replace("__LIMIT__", &limit.to_string())
        .replace("__CLEAR__", if clear { "true" } else { "false" })
}

/// Structured perception (the Python state surface's `elements`/`viewport`):
/// enumerate visible interactive elements, keep the live element list on
/// `window.__amux_els` so click-by-index addresses EXACTLY what /state
/// showed, and return `{url,title,viewport,text,elements}`. `__TEXT_CAP__`
/// slices one past the cap so [`obs_cap`] can append its truncation notice
/// only when text was actually cut.
const STATE_JS: &str = r#"
(function(){
  var SEL = 'a[href], button, input, select, textarea, summary, [role="button"], [role="link"], [role="tab"], [role="menuitem"], [role="checkbox"], [onclick], [contenteditable="true"]';
  var seen = [];
  document.querySelectorAll(SEL).forEach(function(e){
    var r = e.getBoundingClientRect();
    if (!r.width && !r.height) return;
    var st = getComputedStyle(e);
    if (st.visibility === 'hidden' || st.display === 'none') return;
    seen.push(e);
  });
  window.__amux_els = seen;
  var els = seen.slice(0, __EL_LIMIT__).map(function(e, i){
    var label = e.getAttribute('aria-label') || e.innerText || e.value || e.placeholder || e.name || e.id || '';
    label = String(label).replace(/\s+/g, ' ').trim().slice(0, 80);
    return { index: i, tag: (e.tagName || '').toLowerCase(), label: label };
  });
  return { url: location.href, title: document.title,
           viewport: { w: window.innerWidth, h: window.innerHeight },
           text: ((document.body && document.body.innerText) || '').slice(0, __TEXT_CAP__),
           elements: els,
           // NO SILENT CAPS (AMUX-3721). `seen` holds every visible match and is
           // what click-by-index addresses; `els` is only what we render. When
           // those differ the caller MUST be told, because the failure is
           // invisible otherwise: you get a full-looking list, the element you
           // want is not in it, and the only available conclusion is "it is not
           // on the page" — which is wrong, and which is exactly what happened.
           elements_total: seen.length,
           elements_shown: els.length,
           elements_truncated: seen.length > els.length,
           elements_note: seen.length > els.length
             ? ('showing ' + els.length + ' of ' + seen.length + ' visible elements. '
                + 'The rest ARE addressable: click-by-index resolves against the full list, '
                + 'so indices ' + els.length + '..' + (seen.length - 1) + ' work even though '
                + 'they are not listed here. To find one, eval over window.__amux_els, or '
                + 'click by CSS selector instead of index.')
             : null };
})()
"#;

/// Elements shown per state call — matches Python `_bu_parse_elements`'s cap.
pub const STATE_EL_LIMIT: usize = 120;

pub fn state_js() -> String {
    STATE_JS
        .replace("__EL_LIMIT__", &STATE_EL_LIMIT.to_string())
        .replace("__TEXT_CAP__", &(obs_state_cap() + 1).to_string())
}

/// Python `_bu_click_selector`'s resolve-then-click, verbatim in behavior:
/// distinguishes no-match from hidden from clicked, because a click that
/// silently hits nothing is indistinguishable from one that worked.
fn selector_click_js(selector: &str) -> String {
    format!(
        "(function(){{var e=document.querySelector({sel});\
         if(!e)return 'NOMATCH';\
         e.scrollIntoView({{block:'center'}});\
         var r=e.getBoundingClientRect();\
         if(r.width===0&&r.height===0)return 'NOTVISIBLE';\
         e.click();\
         return 'OK|'+(e.tagName||'')+'|'+((e.textContent||'').trim().slice(0,60));}})()",
        sel = json!(selector)
    )
}

/// Click element N of the `window.__amux_els` list /state built — same
/// discrimination ladder as the selector click, plus STALE (the list
/// outlived a navigation) and NOELEMENT (index out of range / no list yet).
fn index_click_js(index: usize) -> String {
    format!(
        "(function(){{var els=window.__amux_els||[];var e=els[{index}];\
         if(!e)return 'NOELEMENT';\
         if(!e.isConnected)return 'STALE';\
         e.scrollIntoView({{block:'center'}});\
         var r=e.getBoundingClientRect();\
         if(!r.width&&!r.height)return 'NOTVISIBLE';\
         e.click();\
         return 'OK|'+(e.tagName||'')+'|'+((e.textContent||e.value||'').trim().slice(0,60));}})()"
    )
}

// ---- driver verb mechanics -------------------------------------------------

/// Python `_CDP_KEYS`: key name → (windowsVirtualKeyCode, char text). The
/// API layer validates against this table so an unsupported key is a 400
/// naming the supported set, not a CDP failure.
pub const CDP_KEYS: &[(&str, i64, &str)] = &[
    ("Enter", 13, "\r"),
    ("Tab", 9, "\t"),
    ("Escape", 27, ""),
    ("Backspace", 8, ""),
    ("ArrowDown", 40, ""),
    ("ArrowUp", 38, ""),
    ("ArrowLeft", 37, ""),
    ("ArrowRight", 39, ""),
    ("PageDown", 34, ""),
    ("PageUp", 33, ""),
];

/// Python `_live_key`: rawKeyDown → optional char → keyUp.
pub async fn dispatch_key(c: &mut CdpClient, key: &str) -> anyhow::Result<()> {
    let (name, vk, text) = CDP_KEYS
        .iter()
        .find(|(k, ..)| *k == key)
        .map(|(k, v, t)| (*k, *v, *t))
        .ok_or_else(|| anyhow::anyhow!("unsupported key {key:?}"))?;
    let t = std::time::Duration::from_secs(10);
    c.call(
        "Input.dispatchKeyEvent",
        json!({ "type": "rawKeyDown", "key": name, "code": name,
                "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk }),
        t,
    )
    .await?;
    if !text.is_empty() {
        c.call("Input.dispatchKeyEvent", json!({ "type": "char", "text": text, "key": name }), t)
            .await?;
    }
    c.call(
        "Input.dispatchKeyEvent",
        json!({ "type": "keyUp", "key": name, "code": name,
                "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk }),
        t,
    )
    .await?;
    Ok(())
}

/// Coordinate click: move → press → release, like a pointer would.
pub async fn click_xy(c: &mut CdpClient, x: f64, y: f64) -> anyhow::Result<()> {
    let t = std::time::Duration::from_secs(10);
    c.call(
        "Input.dispatchMouseEvent",
        json!({ "type": "mouseMoved", "x": x, "y": y, "button": "none" }),
        t,
    )
    .await?;
    for ev in ["mousePressed", "mouseReleased"] {
        c.call(
            "Input.dispatchMouseEvent",
            json!({ "type": ev, "x": x, "y": y, "button": "left", "clickCount": 1 }),
            t,
        )
        .await?;
    }
    Ok(())
}

/// Interpret the OK|tag|text / NOMATCH / NOTVISIBLE / STALE / NOELEMENT
/// protocol the click JS speaks. `Ok(json)` may still carry an `error` key —
/// the API layer maps that to a 400, matching Python's selector-click.
pub fn click_outcome(raw: &Value, what: &str, hint: &str) -> Value {
    let out = raw.as_str().unwrap_or("");
    if out == "NOMATCH" || out == "NOELEMENT" {
        return json!({ "error": format!("no element matches {what}"), "hint": hint });
    }
    if out == "NOTVISIBLE" {
        return json!({ "error": format!("element for {what} has zero size (hidden or not laid out)") });
    }
    if out == "STALE" {
        return json!({ "error": format!("element list is stale for {what} — the page navigated; re-fetch GET /api/browser/state") });
    }
    if let Some(rest) = out.strip_prefix("OK|") {
        let mut parts = rest.splitn(2, '|');
        let tag = parts.next().unwrap_or("");
        let txt = parts.next().unwrap_or("");
        return json!({ "ok": true, "clicked": { "tag": tag, "text": txt } });
    }
    json!({ "error": "click produced no result — the page may have navigated mid-click", "raw": raw })
}

/// Click by CSS selector (Python `_bu_click_selector`).
/// Attach local files to an `<input type=file>` (TUBES-2343).
///
/// # Why this is not an `eval`
///
/// Every other verb in this module drives the page with `Runtime.evaluate`.
/// This one cannot: `input.files` is not assignable from page script, and that
/// is the browser's file-upload security model rather than a gap to route
/// around. `DOM.setFileInputFiles` is the seam the debugger protocol reserves
/// for a driver, and it is what Playwright's `page.setInputFiles` is built on.
///
/// # The element is checked BEFORE the protocol call
///
/// `DOM.setFileInputFiles` against a non-file element fails with a protocol
/// error that names neither the selector nor what was wrong with it, and
/// against a MISSING path it reports SUCCESS — the page then holds an input
/// whose file has no bytes, which reads as a broken upload rather than a bad
/// request. The caller's paths are validated by the handler; the element is
/// validated here, in one round trip, so both failures name themselves.
///
/// Returns `{"error": ...}` for a caller mistake (no such element, wrong
/// element type) and `Err` only for a transport fault, matching
/// [`click_selector`]'s split.
pub async fn set_input_files(
    c: &mut CdpClient,
    selector: &str,
    files: &[String],
) -> anyhow::Result<Value> {
    let ten = std::time::Duration::from_secs(10);
    // One probe, returning a verdict rather than a bare boolean, so a wrong
    // element type can say WHAT it found instead of "not found".
    let probe = format!(
        "(()=>{{const el=document.querySelector({sel});if(!el)return 'NOELEMENT';\
         if(el.tagName!=='INPUT'||(el.type||'').toLowerCase()!=='file')\
         return 'NOTFILE:'+el.tagName+'/'+(el.type||'');\
         return el.multiple?'OK:multiple':'OK:single';}})()",
        sel = json!(selector)
    );
    let verdict = c.eval(&probe, 20).await?;
    let verdict = verdict.as_str().unwrap_or("");
    if verdict == "NOELEMENT" {
        return Ok(json!({
            "error": format!("no element matches {selector:?}"),
            "fix": "check the selector against GET /api/browser/state",
        }));
    }
    if let Some(found) = verdict.strip_prefix("NOTFILE:") {
        return Ok(json!({
            "error": format!(
                "{selector:?} matched a <{found}>, which is not an <input type=file>"
            ),
            "fix": "point the selector at the file input itself — a label or a styled \
                    wrapper is a common miss, and the real input is often visually hidden",
        }));
    }
    // MULTIPLE FILES NEED `multiple`. Chrome silently keeps only the first
    // otherwise, so the test would attach one file and report N.
    if files.len() > 1 && verdict == "OK:single" {
        return Ok(json!({
            "error": format!(
                "{} files given but {selector:?} is a single-file input (no `multiple` \
                 attribute) — Chrome would keep only the first and report success",
                files.len()
            ),
            "fix": "send one file, or point at an input that has `multiple`",
        }));
    }
    // Now the objectId, which is what setFileInputFiles needs and what an eval
    // result cannot give us.
    let resolved = c
        .call(
            "Runtime.evaluate",
            json!({
                "expression": format!("document.querySelector({})", json!(selector)),
                "returnByValue": false,
            }),
            ten,
        )
        .await?;
    let Some(object_id) = resolved
        .get("result")
        .and_then(|r| r.get("objectId"))
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        // The probe just saw the element, so losing it here is a page that
        // navigated or re-rendered underneath us, not a caller mistake.
        return Ok(json!({
            "error": format!("{selector:?} resolved to no remote object — the page changed between the probe and the attach"),
            "fix": "retry; if it repeats, the element is being re-rendered and needs a `wait` first",
        }));
    };
    // DOM domain must be enabled before setFileInputFiles; idempotent.
    let _ = c.call("DOM.enable", json!({}), ten).await;
    c.call(
        "DOM.setFileInputFiles",
        json!({ "files": files, "objectId": object_id }),
        ten,
    )
    .await?;
    // READ THE INPUT BACK. setFileInputFiles returns an empty result and
    // reports success for a path the browser could not use, so the only honest
    // confirmation is what the page now holds — and that is also what the
    // upload under test will read.
    let attached = c
        .eval(
            &format!(
                "(()=>{{const el=document.querySelector({sel});\
                 return el&&el.files?Array.from(el.files).map(f=>f.name+':'+f.size):[];}})()",
                sel = json!(selector)
            ),
            20,
        )
        .await?;
    Ok(json!({
        "ok": true,
        "selector": selector,
        "requested": files,
        // name:size per file, straight off the input. A zero size is the shape
        // a missing or unreadable path leaves behind.
        "attached": attached,
    }))
}

pub async fn click_selector(c: &mut CdpClient, selector: &str) -> anyhow::Result<Value> {
    let raw = c.eval(&selector_click_js(selector), 20).await?;
    let mut v = click_outcome(
        &raw,
        &format!("selector {selector:?}"),
        "check the selector against GET /api/browser/state",
    );
    if let Some(o) = v.as_object_mut() {
        o.insert("selector".into(), json!(selector));
    }
    Ok(v)
}

/// Click by /state element index. NOELEMENT (no list yet — e.g. the caller
/// never fetched /state on this connection's page) re-enumerates once and
/// retries, so index clicks work on the list /state WOULD show.
pub async fn click_index(c: &mut CdpClient, index: usize) -> anyhow::Result<Value> {
    let mut raw = c.eval(&index_click_js(index), 20).await?;
    if raw.as_str() == Some("NOELEMENT") {
        let _ = c.eval(&state_js(), 20).await?;
        raw = c.eval(&index_click_js(index), 20).await?;
    }
    let mut v = click_outcome(
        &raw,
        &format!("element index {index}"),
        "indexes come from GET /api/browser/state — re-fetch it",
    );
    if let Some(o) = v.as_object_mut() {
        o.insert("index".into(), json!(index));
    }
    Ok(v)
}

/// Navigate the page and wait (bounded) for `document.readyState` to reach
/// `complete`; a slow page degrades to "still loading", never a hang. Then
/// re-inject the capture shim (parity with Python `_bu_open` re-injecting
/// per navigation) and run the landing check.
pub async fn navigate_and_settle(c: &mut CdpClient, url: &str) -> anyhow::Result<Value> {
    c.call("Page.navigate", json!({ "url": url }), std::time::Duration::from_secs(20)).await?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut ready = String::new();
    loop {
        if let Ok(v) = c.eval("document.readyState", 5).await {
            ready = v.as_str().unwrap_or("").to_string();
            if ready == "complete" {
                break;
            }
        }
        if std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let _ = c.eval(CAPTURE_JS, 10).await;
    // WHERE WE LANDED IS A TARGET FACT, NOT A PAGE FACT (AC-324).
    //
    // A cross-origin navigation SWAPS the renderer process, which invalidates
    // the execution context this CdpClient is bound to. `location.href` then
    // evaluates in the dead pre-swap context and returns the OLD url — for a
    // navigation that had briefly shown Chrome's error page, that is
    // `chrome-error://chromewebdata/`, so a page that loaded perfectly reports
    // nav_failed.
    //
    // Measured on cloud.amux.io: amux said `nav_failed: true, landed:
    // chrome-error://` while the tab read `https://cloud.amux.io/` at +0s, +3s
    // and +8s. That false verdict blocked all god-mode UI verification of
    // prospect envs and sent two sessions chasing DNS, certificates and a
    // "poisoned" browser profile — none of which were involved.
    //
    // `Page.getNavigationHistory` is answered by the BROWSER about the target,
    // not by a script inside a renderer that may no longer exist, so it
    // survives the swap. The eval stays as a fallback for the case where the
    // history call is unavailable.
    let landed = match c
        .call("Page.getNavigationHistory", json!({}), std::time::Duration::from_secs(10))
        .await
        .ok()
        .and_then(|v| {
            let idx = v.get("currentIndex")?.as_i64()?;
            let entries = v.get("entries")?.as_array()?;
            entries
                .get(idx as usize)?
                .get("url")?
                .as_str()
                .map(str::to_string)
        }) {
        Some(u) if !u.is_empty() => u,
        _ => c
            .eval("location.href", 10)
            .await
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
    };
    let mut out = json!({ "ok": true, "url": landed, "ready_state": ready });
    // Landing check (Python `_bu_landing_check`): an open that cannot fail
    // sends the caller to debug the wrong layer — every later verb would run
    // against an error page or about:blank, not their page.
    if landed.starts_with("chrome-error://") {
        out["nav_failed"] = json!(true);
        out["landed"] = json!(landed);
        out["why"] = json!(format!(
            "Chromium refused to load {url}. For amux's own https://localhost this is the \
             self-signed certificate — the browser has no exception for it."
        ));
        out["hint"] = json!("every subsequent verb runs against the error page, not your page");
    } else if landed == "about:blank" && !url.is_empty() && url != "about:blank" {
        out["nav_failed"] = json!(true);
        out["landed"] = json!(landed);
        out["why"] = json!(format!("navigation to {url} left the page on about:blank"));
        out["hint"] = json!("every subsequent verb runs against a blank page, not your page");
    }
    Ok(out)
}

/// Page.captureScreenshot → PNG on disk, same output shape as Python's
/// driver screenshot (`~/.amux/browser-screenshots/<backend>-<session>.png`,
/// response carries `path`). Zero decoded bytes is an ERROR — a 0-byte file
/// reading as success is the lie ethos rule 7 exists for.
pub async fn screenshot_to_file(c: &mut CdpClient, home: &Path, session: &str) -> anyhow::Result<(PathBuf, usize)> {
    // ACTIVATE BEFORE CAPTURING (AMUX-3712).
    //
    // `Page.captureScreenshot` waits for the renderer to produce a frame, and a
    // tab that is not the active surface does not composite. A backgrounded or
    // occluded tab therefore does not FAIL, it BLOCKS, until the 30s deadline
    // below turns into a 502. That is this card's entire signature: 30,003ms and
    // 30,006ms, the deadline to the millisecond, twice, against a browser whose
    // active surface was an omnibox popup rather than the page being captured.
    //
    // BEST EFFORT AND NON-FATAL, with a deadline of its own. If the renderer is
    // genuinely wedged, bringToFront cannot fix it and must not become a second
    // way for the same fault to be reported; if the tab was merely
    // backgrounded, this is the whole fix. Proceeding on failure means it can
    // only help.
    //
    // The WARN is the discriminator, and it is the thing that did not exist:
    // "capture timed out" alone cannot separate a backgrounded tab from a dead
    // one, and those want opposite responses. A timeout AFTER a successful
    // bringToFront is a wedged renderer.
    if let Err(e) =
        c.call("Page.bringToFront", json!({}), std::time::Duration::from_secs(5)).await
    {
        tracing::warn!(
            "[browser] Page.bringToFront failed for session {session:?} before capture: {e} — \
             capturing anyway. If the capture now times out, the renderer is wedged rather than \
             merely backgrounded (AMUX-3712)"
        );
    }
    let r = c
        .call(
            "Page.captureScreenshot",
            json!({ "format": "png" }),
            std::time::Duration::from_secs(30),
        )
        .await?;
    let b64 = r
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Page.captureScreenshot returned no data"))?;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| anyhow::anyhow!("screenshot base64 decode: {e}"))?;
    if bytes.is_empty() {
        anyhow::bail!("Page.captureScreenshot returned zero bytes");
    }
    let dir = home.join("browser-screenshots");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("native-{}.png", safe_file_component(session)));
    std::fs::write(&file, &bytes)?;
    Ok((file, bytes.len()))
}

/// Session names come from callers; a name is a FILE component here, so
/// anything path-shaped is flattened (Python interpolates the raw name —
/// deliberately not ported).
pub fn safe_file_component(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect();
    if cleaned.trim_matches(['.', '_']).is_empty() {
        "amux".into()
    } else {
        cleaned
    }
}

// ---- profile registry writes (Python `_bu_registry_register` etc.) --------

fn registry_path(home: &Path) -> PathBuf {
    home.join("playwright-auth").join("profiles.json")
}

pub fn registry_save(home: &Path, reg: &serde_json::Map<String, Value>) -> anyhow::Result<()> {
    let path = registry_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write-then-rename, like Python: a torn write must not eat the registry.
    let tmp = path.with_file_name("profiles.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&Value::Object(reg.clone()))?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Ensure a profile is registered; add `host` to its domains, set `label`
/// when given, bump `updated`. Returns the entry (Python
/// `_bu_registry_register`).
pub fn registry_register(home: &Path, name: &str, host: &str, label: &str) -> anyhow::Result<Value> {
    let mut reg = registry_load(home);
    let mut entry = match reg.get(name) {
        Some(Value::Object(m)) => m.clone(),
        _ => serde_json::Map::new(),
    };
    entry.entry("domains").or_insert_with(|| json!([]));
    entry.entry("label").or_insert_with(|| json!(""));
    if !host.is_empty() {
        if let Some(doms) = entry.get_mut("domains").and_then(Value::as_array_mut) {
            if !doms.iter().any(|d| d.as_str() == Some(host)) {
                doms.push(json!(host));
            }
        }
    }
    if !label.is_empty() {
        entry.insert("label".into(), json!(label));
    }
    entry.insert("updated".into(), json!(chrono::Utc::now().timestamp()));
    let entry_v = Value::Object(entry);
    reg.insert(name.to_string(), entry_v.clone());
    registry_save(home, &reg)?;
    Ok(entry_v)
}

/// Python's GET /api/browser/pw-profiles listing: profile DIRS under
/// `playwright-auth/profiles` plus `default` when the bare profile exists.
pub fn pw_profiles(home: &Path) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(home.join("playwright-auth").join("profiles")) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.path().is_dir() && !name.starts_with('.') {
                out.insert(name);
            }
        }
    }
    if home.join("playwright-auth").join("profile").is_dir() {
        out.insert("default".into());
    }
    out.into_iter().collect()
}

/// DELETE /api/browser/profile/{name}. DELIBERATE deviation from Python:
/// Python resolves an unknown name into the REAL Chrome user-data-dir and
/// `rmtree`s it — deleting a human's actual Chrome profile directory. The
/// native port refuses anything outside amux-owned dirs, the rule every
/// other mutation in this module already follows (see module docs).
pub fn delete_profile(home: &Path, name: &str) -> Result<Value, (u16, Value)> {
    if name == "default" {
        return Err((400, json!({ "error": "refusing to delete the default profile" })));
    }
    let dir = resolve_profile_dir(home, &chrome_user_data_dir(), name);
    if !dir.is_dir() {
        return Err((404, json!({ "error": "no such profile" })));
    }
    if !is_amux_owned(home, &dir) {
        return Err((
            400,
            json!({
                "error": format!(
                    "profile {name:?} resolves into the real Chrome user-data-dir — refusing to \
                     delete a human's browser profile (native-only guard; amux-owned profiles \
                     live under playwright-auth/)"
                ),
                "path": dir.display().to_string(),
            }),
        ));
    }
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        return Err((500, json!({ "error": e.to_string() })));
    }
    let mut reg = registry_load(home);
    if reg.remove(name).is_some() {
        let _ = registry_save(home, &reg);
    }
    Ok(json!({ "ok": true, "deleted": name }))
}


// ---- identify: which of these windows is the one amux drives? -------------
//
// AF-496. amux launches Chrome into its OWN --user-data-dir, so the window it
// opens is a second, visually identical Chrome beside the human's. Nothing in
// it says so. Measured in a live onboarding session (Doron + Ethan, 2026-09-04):
// the user signed into HubSpot in the WRONG window, was corrected three times
// ("that browser was already open", "go into the browser window that was
// CREATED when it's trying to configure the profile"), and asked the question
// the product could not answer — "should I trust it, it does look like two
// different browsers?"
//
// amux HOLDS the answer the whole time: pid, CDP port, profile, and which lane
// started it. It just never puts it anywhere the human is looking, which is the
// window itself. `identify` does two independent things so a failure of either
// still leaves a signal:
//
//   1. draws a banner IN the page naming the profile and the lane, which is
//      visible without touching anything and labels EVERY amux window at once
//      when no profile is named;
//   2. raises that window, which answers "show me" when there is one to show.
//
// The unlabelled window is then the human's own, by elimination — the property
// that makes the no-profile form worth having.

/// How long the identify banner stays on screen.
pub fn identify_banner_ms() -> u64 {
    std::env::var("AMUX_BROWSER_IDENTIFY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(12_000)
}

/// The JS an identify injects into one page.
///
/// Pure, and every interpolated value goes through `serde_json::to_string` — a
/// profile name is caller-supplied and would otherwise be JS injected into the
/// page this banner is meant to make trustworthy.
///
/// Returns the number of banners it placed (1), so the caller can tell a page
/// that RAN the script from one that merely did not error.
pub fn identify_js(profile: &str, started_by: &str, ms: u64) -> String {
    let p = serde_json::to_string(profile).unwrap_or_else(|_| "\"?\"".into());
    let b = serde_json::to_string(started_by).unwrap_or_else(|_| "\"?\"".into());
    format!(
        r#"(() => {{
  const ID = 'amux-identify-banner';
  document.getElementById(ID)?.remove();
  const el = document.createElement('div');
  el.id = ID;
  el.setAttribute('data-amux', 'identify');
  el.style.cssText = [
    'position:fixed','top:0','left:0','right:0','z-index:2147483647',
    'background:#1f6feb','color:#fff','padding:10px 14px',
    'font:600 14px/1.4 -apple-system,BlinkMacSystemFont,Segoe UI,sans-serif',
    'box-shadow:0 2px 8px rgba(0,0,0,.35)','text-align:center',
    'pointer-events:none','white-space:normal'
  ].join(';');
  const started = {b} ? (' · started by ' + {b}) : '';
  el.textContent = 'This window is the amux browser · profile ' + {p} + started
    + ' — sign in HERE and the worker inherits the login. Any Chrome window without this bar is your own.';
  (document.body || document.documentElement).appendChild(el);
  setTimeout(() => document.getElementById(ID)?.remove(), {ms});
  return 1;
}})()"#
    )
}

/// What one browser's identify actually managed to do.
///
/// Every field is an OUTCOME, not an intention: `labeled` counts pages the
/// banner script returned from, `raise_supported` says whether raising is even
/// implemented on this platform, and both error strings are kept. A caller
/// reading `labeled: 0, raised: false` with no errors would be reading a lie,
/// so there is no path that produces it silently (ethos rule 4).
#[derive(Debug, Clone, serde::Serialize)]
pub struct IdentifiedBrowser {
    pub profile: String,
    pub started_by: String,
    pub pid: u32,
    pub cdp_port: u16,
    /// Page targets the banner was injected into.
    pub labeled: usize,
    /// Page targets CDP listed at all. `labeled < pages` means some tab refused.
    pub pages: usize,
    pub raised: bool,
    pub raise_supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raise_error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// Bring a running app to the front by pid.
///
/// macOS only, deliberately: `Page.bringToFront` raises the TAB within its
/// Chrome window but does not activate the application, and with two Chrome
/// processes (the human's and amux's) the one the user is staring at stays on
/// top. System Events activates by unix id, which is exactly the identity amux
/// already records. On any other platform this returns the reason rather than
/// a bare failure, so `raise_supported: false` and "it did not work" stay
/// distinguishable.
pub async fn raise_app_by_pid(pid: u32) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("raising a window by pid is implemented on macOS only".into());
    }
    let script = format!(
        "tell application \"System Events\" to set frontmost of (first process whose unix id is {pid}) to true"
    );
    // BOUNDED, because this can BLOCK INDEFINITELY and did (measured on the very
    // first live call of this endpoint, 2026-09-04: 2 minutes, then the client
    // gave up). macOS gates Automation between processes: the first System
    // Events call from a process that has not been granted it puts up a consent
    // dialog and `osascript` waits on the human. A "which window is it" button
    // that hangs the request is worse than one that says it could not tell you,
    // so the timeout is short and its message NAMES the likely cause rather
    // than reporting a generic timeout nobody can act on.
    //
    // kill_on_drop, per tests/kill_on_drop_guard.rs: a dropped timeout future
    // otherwise leaves the osascript child unreaped.
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        tokio::process::Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| {
        "osascript did not answer within 8s — macOS is most likely showing an Automation \
         consent dialog for System Events. Approve it once (System Settings > Privacy & \
         Security > Automation) and the raise works from then on."
            .to_string()
    })?
    .map_err(|e| format!("osascript: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if stderr.is_empty() { format!("osascript exited {}", out.status) } else { stderr })
}

/// Label every page of ONE running browser, and optionally raise its window.
pub async fn identify_one(
    profile: &str,
    started_by: &str,
    pid: u32,
    port: u16,
    raise: bool,
) -> IdentifiedBrowser {
    let mut out = IdentifiedBrowser {
        profile: profile.to_string(),
        started_by: started_by.to_string(),
        pid,
        cdp_port: port,
        labeled: 0,
        pages: 0,
        raised: false,
        raise_supported: cfg!(target_os = "macos"),
        raise_error: None,
        errors: Vec::new(),
    };
    let js = identify_js(profile, started_by, identify_banner_ms());
    match cdp_list(port).await {
        Ok(tabs) => {
            let pages: Vec<Value> = tabs
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            out.pages = pages.len();
            for t in &pages {
                let ws = t.get("webSocketDebuggerUrl").and_then(Value::as_str).unwrap_or("");
                if ws.is_empty() {
                    out.errors.push("a page target had no webSocketDebuggerUrl".into());
                    continue;
                }
                match CdpClient::connect(ws).await {
                    Ok(mut c) => match c.eval(&js, 10).await {
                        Ok(_) => out.labeled += 1,
                        Err(e) => out.errors.push(format!("banner: {e}")),
                    },
                    Err(e) => out.errors.push(format!("connect: {e}")),
                }
            }
        }
        Err(e) => out.errors.push(format!("cdp_list: {e}")),
    }
    if raise {
        match raise_app_by_pid(pid).await {
            Ok(()) => out.raised = true,
            Err(e) => out.raise_error = Some(e),
        }
    }
    out
}


#[cfg(test)]
mod identify_tests {
    use super::*;

    /// The banner has to say the thing that DISCRIMINATES, not just "amux".
    /// A bar reading "amux browser" on one window still leaves the human
    /// wondering what the other window is; the sentence about an unlabelled
    /// window is what closes it, and it is the sentence Ethan had to say out
    /// loud three times.
    #[test]
    fn the_banner_names_the_profile_and_says_what_an_unlabelled_window_means() {
        let js = identify_js("hubspot", "gtm-engine", 12_000);
        assert!(js.contains("\"hubspot\""), "the profile is not in the banner: {js}");
        assert!(js.contains("\"gtm-engine\""), "the starting lane is not in the banner: {js}");
        assert!(
            js.contains("without this bar is your own"),
            "the banner never says what an UNLABELLED window is, which is the whole answer"
        );
        assert!(js.contains("12000"), "the self-removal timeout is not the one asked for");
    }

    /// A profile name is caller-supplied (`POST /profile/create {name}`), and
    /// this script is injected into a page. Interpolating the name raw would
    /// let a profile named `"; fetch(...)` run arbitrary JS in whatever site
    /// the human is signed into — in the window whose entire purpose is to be
    /// the trustworthy one.
    #[test]
    fn a_profile_name_cannot_break_out_of_the_banner_string() {
        let hostile = "a\"; document.title='pwned'; //";
        let js = identify_js(hostile, "amux", 5);
        assert!(
            !js.contains(hostile),
            "the raw profile name reached the script unescaped: {js}"
        );
        assert!(
            js.contains(&serde_json::to_string(hostile).unwrap()),
            "the name is not present as a JSON-escaped literal either: {js}"
        );
    }

    /// The banner removes itself. A permanent overlay on a page the worker then
    /// drives would sit on top of the buttons it needs to click.
    #[test]
    fn the_banner_removes_itself() {
        let js = identify_js("default", "amux", 900);
        assert!(js.contains("setTimeout"), "nothing schedules the removal");
        assert!(js.contains("remove()"), "nothing removes it");
        assert!(js.contains("pointer-events:none"), "the banner can swallow clicks while it is up");
    }

    #[test]
    fn the_banner_duration_is_configurable_and_a_zero_falls_back() {
        let key = "AMUX_BROWSER_IDENTIFY_MS";
        let prior = std::env::var(key).ok();
        // SAFETY: single-threaded test; restored below.
        unsafe { std::env::set_var(key, "3000") };
        assert_eq!(identify_banner_ms(), 3000);
        unsafe { std::env::set_var(key, "0") };
        assert_eq!(identify_banner_ms(), 12_000, "0 would mean a banner nobody can read");
        unsafe { std::env::set_var(key, "not-a-number") };
        assert_eq!(identify_banner_ms(), 12_000);
        match prior {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    /// `raise_supported` is a claim about the PLATFORM, and it has to match
    /// what `raise_app_by_pid` will actually attempt — otherwise a Linux caller
    /// reads `raise_supported: true, raised: false` and goes looking for a
    /// window manager problem that does not exist.
    #[tokio::test]
    async fn a_platform_that_cannot_raise_says_so_rather_than_failing_silently() {
        if cfg!(target_os = "macos") {
            // pid 1 is launchd: System Events refuses it, which is the point —
            // the call is ATTEMPTED here, so the error is a real one.
            let r = raise_app_by_pid(1).await;
            assert!(r.is_err(), "raising launchd should not report success");
        } else {
            let r = raise_app_by_pid(1).await;
            assert_eq!(
                r.unwrap_err(),
                "raising a window by pid is implemented on macOS only"
            );
        }
    }
}

#[cfg(test)]
mod import_error_tests {
    use super::*;

    /// THE CELL THIS EXISTS FOR. A user clicked "import on first use", got one
    /// line, and neither they nor the person walking them through it could read
    /// it. Every message from this path must now name what was attempted, both
    /// paths, and a remedy.
    #[test]
    fn every_import_failure_names_the_step_both_paths_and_a_remedy() {
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::AlreadyExists,
            std::io::ErrorKind::StorageFull,
            std::io::ErrorKind::Other,
        ] {
            let m = import_failure_message(
                "the final rename into place",
                Path::new("/from/here"),
                Path::new("/to/there"),
                kind,
                "some raw error",
            );
            assert!(m.contains("the final rename into place"), "{kind:?}: no step: {m}");
            assert!(m.contains("/from/here"), "{kind:?}: no source path: {m}");
            assert!(m.contains("/to/there"), "{kind:?}: no destination path: {m}");
            assert!(m.contains("some raw error"), "{kind:?}: the raw error was swallowed: {m}");
            assert!(m.contains("Most likely:"), "{kind:?}: no cause offered: {m}");
        }
    }

    /// The remedy that actually resolves the common case on a fresh Mac, and the
    /// one nobody guesses from "Permission denied": Chrome's profile lives under
    /// ~/Library/Application Support, which is TCC-protected, so the server needs
    /// Full Disk Access. Two clicks, invisible from the errno.
    #[test]
    fn a_permission_error_points_at_full_disk_access_not_at_file_modes() {
        let m = import_failure_message(
            "the copy of the source profile",
            Path::new("/src"),
            Path::new("/dst"),
            std::io::ErrorKind::PermissionDenied,
            "Permission denied (os error 13)",
        );
        assert!(m.contains("Full Disk Access"), "{m}");
        assert!(m.contains("Privacy & Security"), "{m}");
        assert!(
            m.contains("Nothing needs to be deleted or recreated"),
            "the reader is left thinking they must start over: {m}"
        );
    }

    /// Each errno gets its OWN cause. A single generic sentence would be exactly
    /// as green against this suite's first cell while telling every reader the
    /// same useless thing.
    #[test]
    fn different_errnos_produce_different_causes() {
        let mk = |k, raw| {
            import_failure_message("s", Path::new("/a"), Path::new("/b"), k, raw)
        };
        let perm = mk(std::io::ErrorKind::PermissionDenied, "x");
        let nf = mk(std::io::ErrorKind::NotFound, "x");
        let ae = mk(std::io::ErrorKind::AlreadyExists, "x");
        let full = mk(std::io::ErrorKind::StorageFull, "x");
        let xdev = mk(std::io::ErrorKind::Other, "Cross-device link (os error 18)");
        let unknown = mk(std::io::ErrorKind::Other, "something nobody mapped");
        let all = [&perm, &nf, &ae, &full, &xdev, &unknown];
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert_ne!(a, b, "two errnos produced the same message");
            }
        }
        assert!(nf.contains("is NOT the directory name"), "{nf}");
        assert!(xdev.contains("different filesystems"), "{xdev}");
    }

    /// An unmapped errno says it is unmapped. Presenting a guess as the likely
    /// cause is how someone spends an hour on the wrong thing; saying "this is
    /// all amux knows, and that is worth reporting" costs nothing and is true.
    #[test]
    fn an_unmapped_errno_admits_it_rather_than_guessing() {
        let m = import_failure_message(
            "s",
            Path::new("/a"),
            Path::new("/b"),
            std::io::ErrorKind::Other,
            "errno 12345",
        );
        assert!(m.contains("no known-cause mapping"), "{m}");
        assert!(m.contains("errno 12345"), "{m}");
    }
}

// ---- tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // All tests run against TEMP dirs only — never the live Chrome profiles
    // (repo rule: tests must not touch real profile contents), and never
    // launch Chrome except the #[ignore]d gated tests at the bottom.

    /// AMUX-3708: a tab Chrome still LISTS can be unreachable, and only the
    /// caller that failed to open its socket knows.
    ///
    /// THE SPECIMEN is a pair of 502s 40 seconds apart on 2026-08-25, filed as
    /// one card because they share (status, method, family, target):
    ///   11:12:41  30,006ms  "CDP Page.captureScreenshot timed out after 30s"
    ///   11:13:21      24ms  "WebSocket protocol error: Connection reset
    ///                        without closing handshake"
    /// The first wedged the renderer. The second is its aftermath: the next
    /// request resolved to the SAME target, because `/json/list` still reported
    /// it and the pre-existing fall-through only triggers when a tab is GONE.
    /// Recovery came at 11:13:25 solely because Chrome had by then removed the
    /// target. Every request in that window 502s and nothing shortens it.
    ///
    /// FOUR CELLS, and the last two are the ones that keep this honest. Without
    /// the no-avoid cell, dropping every binding unconditionally would pass. Without
    /// the peer cell, avoiding a target by hijacking a peer's tab would pass.
    /// AMUX-3834 (tubescience): a rebind must not land on a blank tab while a
    /// real page is available, because a blank binding is PERMANENT.
    ///
    /// THE TRAP IS THE `KeepBound` ARM. Once the session is bound to an
    /// about:blank, that tab IS listed, so every later resolve keeps it — the
    /// binding never recovers on its own. Closing the blank tabs did not help
    /// the reporter, because Chrome respawns one and the next rebind takes it
    /// again.
    ///
    /// AND WHY IT LOOKED LIKE A SCREENSHOT BUG: screenshot, eval and action all
    /// resolve through this one rule, so all three showed the blank page. Only
    /// `navigate` escaped, because it passes a URL and re-targets. The reporter
    /// read that asymmetry as "screenshot resolves differently from action";
    /// their own follow-up disproved it when eval hit the blank origin too.
    #[test]
    fn a_rebind_prefers_a_real_page_over_a_blank_one() {
        let t = |id: &str, url: &str| {
            json!({"id": id, "type": "page",
                   "webSocketDebuggerUrl": format!("ws://127.0.0.1:9/devtools/page/{id}"),
                   "url": url, "title": ""})
        };
        let none = std::collections::HashSet::new();

        // The reporter's shape: a blank tab listed FIRST, the real page after.
        // Chrome's /json/list order is Chrome's, not ours.
        let tabs = vec![t("BLANK", "about:blank"), t("STUDIO", "https://studio.mixpeek.com/x")];
        let (choice, _) = choose_target(&tabs, None, &none, None);
        assert_eq!(
            choice,
            TargetChoice::Rebind("STUDIO".into()),
            "a real page must win over a blank one regardless of list order"
        );

        // chrome:// internals are blank-equivalent: a new-tab page is not what
        // a caller means either.
        let tabs = vec![t("NEWTAB", "chrome://newtab/"), t("STUDIO", "https://studio.mixpeek.com/x")];
        assert_eq!(
            choose_target(&tabs, None, &none, None).0,
            TargetChoice::Rebind("STUDIO".into()),
            "chrome:// internals lose to a real page too"
        );

        // A PREFERENCE, NOT A FILTER. If blank is all there is, bind to it —
        // the alternative is opening tabs forever when the browser genuinely
        // has nothing else.
        let only_blank = vec![t("BLANK", "about:blank")];
        assert_eq!(
            choose_target(&only_blank, None, &none, None).0,
            TargetChoice::Rebind("BLANK".into()),
            "with nothing else, a blank tab is still usable"
        );

        // THE SECOND PASS (AMUX-3834). Preventing bad rebinds does nothing for
        // a session ALREADY bound to a blank tab: that tab is listed, so the
        // KeepBound arm held it forever and only an explicit navigate escaped.
        // tubescience walked back the first fix on exactly this, correctly — I
        // then reproduced it here, with screenshot and eval BOTH returning
        // about:blank, agreeing with each other and both wrong.
        let stuck = vec![t("BLANK", "about:blank"), t("STUDIO", "https://studio.mixpeek.com/x")];
        let (choice, stale) = choose_target(&stuck, Some("BLANK"), &none, None);
        assert_eq!(
            choice,
            TargetChoice::Rebind("STUDIO".into()),
            "a session stuck on blank must self-heal when a real page exists"
        );
        assert!(!stale, "self-healing off a blank tab is not the wedged-target case");

        // AND IT MUST NOT CHURN. With nothing better available the blank
        // binding is KEPT, or every verb rebinds on every call and the session
        // never holds still.
        let only_blank = vec![t("BLANK", "about:blank"), t("OTHER", "about:blank")];
        assert_eq!(
            choose_target(&only_blank, Some("BLANK"), &none, None).0,
            TargetChoice::KeepBound("BLANK".into()),
            "with nothing better, a blank binding is kept rather than churned"
        );

        // A REAL binding is never disturbed. This rule only ever demotes BLANK.
        let two_real = vec![t("A", "https://a.com/"), t("B", "https://b.com/")];
        assert_eq!(
            choose_target(&two_real, Some("A"), &none, None).0,
            TargetChoice::KeepBound("A".into()),
            "a good binding must survive the presence of other pages"
        );

        // THE CONTROLS, so the preference cannot quietly override the rules it
        // sits inside. `avoid` still wins: a real page the caller just proved
        // unreachable must NOT be picked over a blank one.
        let tabs = vec![t("BLANK", "about:blank"), t("DEAD", "https://studio.mixpeek.com/x")];
        assert_eq!(
            choose_target(&tabs, Some("DEAD"), &none, Some("DEAD")).0,
            TargetChoice::Rebind("BLANK".into()),
            "an unreachable real page loses to a usable blank one"
        );
        // And a peer's claim still wins over the preference.
        let claimed: std::collections::HashSet<String> = ["STUDIO".to_string()].into();
        let tabs = vec![t("BLANK", "about:blank"), t("STUDIO", "https://studio.mixpeek.com/x")];
        assert_eq!(
            choose_target(&tabs, None, &claimed, None).0,
            TargetChoice::Rebind("BLANK".into()),
            "a real page claimed by a peer must not be hijacked for being real"
        );
    }

    #[test]
    fn a_listed_but_unreachable_target_is_abandoned_and_not_picked_up_again() {
        let tab = |id: &str| {
            json!({"id": id, "type": "page", "webSocketDebuggerUrl": format!("ws://127.0.0.1:9/devtools/page/{id}"), "url": "about:blank", "title": ""})
        };
        let none = std::collections::HashSet::new();

        // 1. The bound target is the one that just refused a socket. It must be
        //    abandoned, reported stale, and NOT handed back.
        let tabs = vec![tab("WEDGED"), tab("SPARE")];
        let (choice, stale) = choose_target(&tabs, Some("WEDGED"), &none, Some("WEDGED"));
        assert!(stale, "the binding names the unreachable target, so it is stale");
        assert_eq!(
            choice,
            TargetChoice::Rebind("SPARE".into()),
            "must rebind away from the wedged tab, not hand it back"
        );

        // 2. ...and the rebind must not pick it up again. Dropping the binding
        //    makes WEDGED unclaimed, so a rebind that ignores `avoid` selects it
        //    right back: this is the cell that fails if the second filter is lost.
        let only_wedged = vec![tab("WEDGED")];
        let (choice, stale) = choose_target(&only_wedged, Some("WEDGED"), &none, Some("WEDGED"));
        assert!(stale);
        assert_eq!(
            choice,
            TargetChoice::NewTab,
            "with no other tab, open a fresh one rather than re-selecting the wedged target"
        );

        // 3. NO avoid: behaviour is unchanged, and the binding is not stale. A
        //    fix that unbound on every call would satisfy 1 and 2 and break every
        //    ordinary request.
        let (choice, stale) = choose_target(&tabs, Some("WEDGED"), &none, None);
        assert!(!stale, "an ordinary resolve must never report a stale binding");
        assert_eq!(
            choice,
            TargetChoice::KeepBound("WEDGED".into()),
            "without a failed socket there is nothing to avoid: keep the binding"
        );

        // 4. Avoiding a target must not reach into a PEER's tab. AC-336 is the
        //    card that says one lane never adopts another's; a recovery path that
        //    forgets it would be a regression wearing a fix's clothes.
        let claimed: std::collections::HashSet<String> = ["SPARE".to_string()].into_iter().collect();
        let (choice, _) = choose_target(&tabs, Some("WEDGED"), &claimed, Some("WEDGED"));
        assert_eq!(
            choice,
            TargetChoice::NewTab,
            "SPARE belongs to another lane — open a new tab instead of hijacking it"
        );
    }

    /// AMUX-3689: an ANSWERED poll and a SILENT one must not read the same.
    ///
    /// THE SPECIMEN, and it is why this is a test rather than a comment: on
    /// 2026-08-24 `primer` got six `POST /api/browser/start` 502s at ~30.3s each
    /// saying "CDP on port 60005 never answered within 30s", with
    /// `DevTools listening on ws://127.0.0.1:60005/devtools/browser/e9edcb66-...`
    /// in the stderr embedded in the same message. Chrome had answered on that
    /// port about three seconds in. The message was not vague, it was wrong, and
    /// it sends an investigator to Chrome's startup instead of to the poll.
    ///
    /// The connect case is a REAL error object, produced by dialling a port
    /// nothing listens on, rather than a hand-built stand-in: `reqwest::Error`
    /// has no public constructor, and a stand-in would be testing my model of
    /// `is_connect()` rather than `is_connect()`.
    #[tokio::test]
    async fn an_answered_cdp_poll_never_reads_as_silence() {
        let answered: Result<reqwest::Response, reqwest::Error> =
            Ok(axum::http::Response::builder().status(403).body("").unwrap().into());
        let d = describe_cdp_probe(&answered, "http://127.0.0.1:60005");
        assert!(d.contains("403"), "the STATUS is the whole discriminator: {d}");
        assert!(
            !d.to_lowercase().contains("never answered")
                && !d.to_lowercase().contains("refused"),
            "a 403 is Chrome answering — calling it silence is the AMUX-3689 defect: {d}"
        );

        // Port 1 on loopback: privileged, unbound, refuses immediately.
        let refused = reqwest::Client::new()
            .get("http://127.0.0.1:1/json/version")
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await;
        assert!(refused.is_err(), "premise: nothing may be listening on 127.0.0.1:1");
        let d = describe_cdp_probe(&refused, "http://127.0.0.1:1");
        assert!(
            d.contains("refused") || d.contains("connection"),
            "a genuinely silent port must say so, or the two cells above collapse: {d}"
        );
    }

    /// AMUX-3689: a retry must not destroy the previous failure's stderr.
    ///
    /// `start()` opens the stderr path with `File::create`, which truncates, and
    /// a failing caller always retries. `primer` failed six times and five of the
    /// six stderr files were overwritten before anyone looked, leaving a 600-char
    /// tail as the whole record. The artifact you need most when a failure
    /// REPEATS was being deleted by the fact that it repeated.
    #[test]
    fn a_failed_launch_keeps_its_stderr_and_the_pile_stays_bounded() {
        let dir = std::env::temp_dir().join(format!("amux-stderr-keep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join("amux-chrome-launch.stderr");

        std::fs::write(&live, "first failure\nDevTools listening on ws://127.0.0.1:1/x\n").unwrap();
        let note = preserve_failed_stderr(&live);
        assert!(note.contains("failed-"), "the message must NAME the kept file: {note:?}");
        let kept: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("amux-chrome-launch.failed-"))
            .collect();
        assert_eq!(kept.len(), 1, "{kept:?}");
        let body = std::fs::read_to_string(dir.join(&kept[0])).unwrap();
        assert!(body.contains("DevTools listening"), "the COPY must hold the evidence: {body:?}");

        // Simulate the retry that used to erase it: truncate the live file and
        // preserve again. The first failure's copy must survive.
        std::fs::write(&live, "second failure\n").unwrap();
        preserve_failed_stderr(&live);
        assert!(
            std::fs::read_to_string(dir.join(&kept[0])).unwrap().contains("DevTools listening"),
            "the retry erased the first failure's evidence, which is the whole bug"
        );

        // BOUNDED: a profile that fails all day must not fill the disk with its
        // own evidence. Written directly rather than by repeated calls so the
        // prune is tested on a known set rather than on however many distinct
        // millisecond stamps the loop happened to produce.
        for i in 0..9 {
            std::fs::write(dir.join(format!("amux-chrome-launch.failed-{:010}.stderr", i)), "x")
                .unwrap();
        }
        std::fs::write(&live, "another\n").unwrap();
        preserve_failed_stderr(&live);
        let n = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name().to_string_lossy().starts_with("amux-chrome-launch.failed-")
            })
            .count();
        assert_eq!(n, 5, "the pile must be pruned to the newest 5, found {n}");

        // A MISSING file must be silent, not a panic: a launch may fail before
        // the stderr file is ever created, and a browser start must never fail
        // because its diagnostic copy failed.
        assert_eq!(preserve_failed_stderr(&dir.join("nope.stderr")), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AMUX-3674, from Ethan's report and the live `ps` output that explained
    /// it. Five Chromes were sharing one `--user-data-dir`, which is
    /// single-instance in Chrome, so all but the lock holder showed
    /// "Something went wrong when opening your profile."
    ///
    /// The dangerous half is the CONTROL, and it caught a bug in the first
    /// draft of this function before it shipped: `playwright-auth/profile` is a
    /// PREFIX of `playwright-auth/profiles/netsuite`, so a `contains` test on
    /// the flag matched 9 processes on the live machine including the netsuite
    /// browser holding a staged login. Cleaning the DEFAULT profile would have
    /// killed every NAMED profile's browser.
    #[test]
    fn stray_cotenants_match_the_dir_exactly_and_never_a_longer_one() {
        // Verbatim shape of `ps -ax -o pid=,command=` on this machine.
        let ps = "\
29637 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome --remote-debugging-port=58242 --user-data-dir=/h/.amux/playwright-auth/profile --no-first-run
35866 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome --remote-debugging-port=63713 --user-data-dir=/h/.amux/playwright-auth/profile --no-first-run
35459 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome --remote-debugging-port=59100 --user-data-dir=/h/.amux/playwright-auth/profiles/netsuite --no-first-run
40001 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome Helper --type=renderer --user-data-dir=/h/.amux/playwright-auth/profile
  871 /Library/PrivilegedHelperTools/ChromeRemoteDesktopHost.app/Contents/MacOS/remoting_agent_process_broker";

        let default_dir = pids_on_dir(ps, "--user-data-dir=/h/.amux/playwright-auth/profile");
        assert_eq!(
            default_dir,
            vec![29637, 35866],
            "the default profile's co-tenants, and NOT profiles/netsuite: {default_dir:?}"
        );
        assert!(
            !default_dir.contains(&35459),
            "a longer path that merely STARTS with this one is a different profile — killing \
             it would take out another session's staged login"
        );
        assert!(!default_dir.contains(&40001), "helper processes die with their parent");
        assert!(!default_dir.contains(&871), "unrelated processes are not candidates");

        // The named profile finds only its own, which is the same rule read
        // from the other side.
        assert_eq!(
            pids_on_dir(ps, "--user-data-dir=/h/.amux/playwright-auth/profiles/netsuite"),
            vec![35459]
        );

        // A dir nobody is on is empty, not everything.
        assert!(pids_on_dir(ps, "--user-data-dir=/h/.amux/playwright-auth/profiles/nope").is_empty());
    }

    /// EVERY live-Chrome test must hold this for its whole body.
    ///
    /// `RUNNING` and `NATIVE_TARGETS` are process-global — one browser, one
    /// binding table — so two live tests running concurrently are two lanes
    /// fighting over one browser, which is the very bug this file's newest test
    /// is about. Concretely: `start` stops whatever is running and clears the
    /// binding table, so a peer test's launch wipes this test's claim mid-body
    /// and the assertion fails against CORRECT code. That is the worst kind of
    /// red — it accuses the fix.
    ///
    /// It is a tokio Mutex, not a std one, because these bodies await while
    /// holding it. `--test-threads=1` would also work but cannot be enforced
    /// from here, and a test that only passes under a flag someone has to
    /// remember is a test that will go red for the wrong reason.
    static LIVE_BROWSER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn fake_home() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn profile_resolution_matches_python_precedence() {
        let home = fake_home();
        let chrome = home.path().join("fake-chrome-udd");
        std::fs::create_dir_all(&chrome).unwrap();

        // default -> the bare playwright-auth/profile dir
        assert_eq!(
            resolve_profile_dir(home.path(), &chrome, "default"),
            home.path().join("playwright-auth/profile")
        );
        assert_eq!(
            resolve_profile_dir(home.path(), &chrome, "  "),
            home.path().join("playwright-auth/profile")
        );

        // a legacy dir that exists wins over the chrome location
        let legacy = home.path().join("playwright-auth/profiles/gh");
        std::fs::create_dir_all(&legacy).unwrap();
        assert_eq!(resolve_profile_dir(home.path(), &chrome, "gh"), legacy);

        // unknown names resolve into the chrome user-data-dir
        assert_eq!(resolve_profile_dir(home.path(), &chrome, "brandnew"), chrome.join("brandnew"));
    }

    #[test]
    fn launch_target_splits_chrome_dir_profiles() {
        let home = fake_home();
        let chrome = home.path().join("fake-chrome-udd");
        std::fs::create_dir_all(&chrome).unwrap();

        // amux-owned dir IS the user-data-dir
        let legacy = home.path().join("playwright-auth/profiles/gh");
        std::fs::create_dir_all(&legacy).unwrap();
        let t = launch_target(home.path(), &chrome, "gh");
        assert_eq!(t.user_data_dir, legacy);
        assert_eq!(t.profile_directory, None);

        // chrome-dir profile becomes --profile-directory under the parent
        let t = launch_target(home.path(), &chrome, "Work");
        assert_eq!(t.user_data_dir, chrome);
        assert_eq!(t.profile_directory.as_deref(), Some("Work"));
    }

    #[test]
    fn chrome_profile_import_is_atomic_isolated_and_cache_free() {
        let home = fake_home();
        let chrome = home.path().join("fake-chrome-udd");
        let source = chrome.join("ethan-tubescience");
        std::fs::create_dir_all(source.join("Network")).unwrap();
        std::fs::create_dir_all(source.join("Cache")).unwrap();
        std::fs::write(source.join("Network/Cookies"), b"logged-in").unwrap();
        std::fs::write(source.join("Preferences"), b"prefs").unwrap();
        std::fs::write(source.join("Cache/disposable"), b"cache").unwrap();
        std::fs::write(source.join("LOCK"), b"held").unwrap();
        std::fs::write(
            chrome.join("Local State"),
            r#"{"os_crypt":{"encrypted_key":"keep-me"},"profile":{"last_used":"ethan-tubescience","last_active_profiles":["ethan-tubescience"],"info_cache":{"ethan-tubescience":{"name":"TubeScience"},"Other":{"name":"Other"}}}}"#,
        )
        .unwrap();

        let imported = import_chrome_profile(home.path(), &chrome, "ethan-tubescience")
            .unwrap()
            .expect("first use imports the Chrome profile");
        let dest = home.path().join("playwright-auth/profiles/ethan-tubescience");
        assert_eq!(imported.source, source);
        assert_eq!(imported.destination, dest);
        assert_eq!(std::fs::read(dest.join("Default/Network/Cookies")).unwrap(), b"logged-in");
        assert_eq!(std::fs::read(dest.join("Default/Preferences")).unwrap(), b"prefs");
        assert!(!dest.join("Default/Cache").exists(), "cache is recreated, not imported");
        assert!(!dest.join("Default/LOCK").exists(), "a live profile's lock must not cross over");

        let state: Value =
            serde_json::from_slice(&std::fs::read(dest.join("Local State")).unwrap()).unwrap();
        assert_eq!(state["os_crypt"]["encrypted_key"], "keep-me");
        assert_eq!(state["profile"]["last_used"], "Default");
        assert_eq!(state["profile"]["last_active_profiles"], json!(["Default"]));
        assert_eq!(state["profile"]["info_cache"]["Default"]["name"], "TubeScience");
        assert!(state["profile"]["info_cache"].get("Other").is_none());

        let target = launch_target(home.path(), &chrome, "ethan-tubescience");
        assert_eq!(target.user_data_dir, dest);
        assert!(is_amux_owned(home.path(), &target.user_data_dir));
        assert_eq!(target.profile_directory, None);
        let inventory = list_profiles(home.path(), false);
        assert!(
            inventory.iter().any(|p| p.name == "ethan-tubescience" && p.registered),
            "imported login state must be exempt from scratch-profile TTL cleanup"
        );
        assert!(
            import_chrome_profile(home.path(), &chrome, "ethan-tubescience")
                .unwrap()
                .is_none(),
            "later starts reuse the durable isolated copy"
        );
    }

    #[test]
    fn chrome_profile_import_rejects_escape_and_missing_sources() {
        let home = fake_home();
        let chrome = home.path().join("fake-chrome-udd");
        std::fs::create_dir_all(&chrome).unwrap();
        let escape = import_chrome_profile(home.path(), &chrome, "../outside").unwrap_err();
        assert!(escape.to_string().contains("[A-Za-z0-9._-]+"));
        let missing = import_chrome_profile(home.path(), &chrome, "missing").unwrap_err();
        assert!(missing.to_string().contains("does not exist"));
        assert!(!home.path().join("playwright-auth/profiles/missing").exists());
    }

    #[test]
    fn inventory_lists_dirs_and_registry_metadata() {
        let home = fake_home();
        let profiles = home.path().join("playwright-auth/profiles");
        std::fs::create_dir_all(profiles.join("github")).unwrap();
        std::fs::write(profiles.join("github/Cookies"), vec![0u8; 2048]).unwrap();
        std::fs::create_dir_all(home.path().join("playwright-auth/profile")).unwrap();
        std::fs::write(
            home.path().join("playwright-auth/profiles.json"),
            r#"{"github":{"domains":["github.com"],"label":"GH"}}"#,
        )
        .unwrap();

        let list = list_profiles(home.path(), true);
        let names: Vec<&str> = list.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"github"), "dir-backed profile listed: {names:?}");
        assert!(names.contains(&"default"), "default profile listed: {names:?}");

        let gh = list.iter().find(|p| p.name == "github").unwrap();
        assert!(gh.registered);
        assert_eq!(gh.domains, vec!["github.com"]);
        assert_eq!(gh.label, "GH");
        assert!(gh.last_used.is_some(), "mtime-derived last_used");
        assert!(gh.size_mb.unwrap() > 0.0, "walked size includes the 2KB cookie file");

        // sizes are opt-in
        let slim = list_profiles(home.path(), false);
        assert!(slim.iter().all(|p| p.size_mb.is_none()));
    }

    #[test]
    fn lock_cleanup_removes_singletons_including_dangling_symlinks() {
        let home = fake_home();
        let prof = home.path().join("playwright-auth/profiles/x");
        std::fs::create_dir_all(&prof).unwrap();
        // Real Chrome locks are symlinks at "<host>-<pid>" whose target does
        // not exist — the exact case exists() gets wrong and lstat gets right.
        #[cfg(unix)]
        std::os::unix::fs::symlink("nohost-99999", prof.join("SingletonLock")).unwrap();
        #[cfg(not(unix))]
        std::fs::write(prof.join("SingletonLock"), "x").unwrap();
        std::fs::write(prof.join("SingletonCookie"), "c").unwrap();
        std::fs::write(prof.join("Preferences"), "{}").unwrap();

        let present = locks_present(&prof);
        assert!(present.contains(&"SingletonLock".to_string()), "lstat sees the dangling symlink");
        assert!(present.contains(&"SingletonCookie".to_string()));

        let cleaned = reconcile_locks_at_startup(home.path());
        assert_eq!(cleaned.len(), 1);
        let (dir, removed) = &cleaned[0];
        assert_eq!(dir, &prof);
        assert!(removed.contains(&"SingletonLock".to_string()));
        assert!(removed.contains(&"SingletonCookie".to_string()));
        assert!(locks_present(&prof).is_empty(), "locks actually gone");
        assert!(prof.join("Preferences").exists(), "profile CONTENTS untouched");
    }

    /// MO-3146: a live lock on the HUMAN Chrome dir must be detected before
    /// spawn. Chrome's exit-0 delegation is not harmless — every attempted
    /// launch opens one more tab in the existing browser.
    #[cfg(unix)]
    #[test]
    fn live_human_chrome_lock_is_a_pre_spawn_conflict_but_a_stale_lock_is_not() {
        let home = fake_home();
        let human_chrome = home.path().join("human-chrome-data");
        std::fs::create_dir_all(&human_chrome).unwrap();
        let lock = human_chrome.join("SingletonLock");

        // SPECIMEN: the real lock on the incident machine was
        // `ethan-m1-4.local-97230`; dashes in the hostname must not confuse
        // the pid parser.
        let me = std::process::id();
        std::os::unix::fs::symlink(format!("ethan-m1-4.local-{me}"), &lock).unwrap();
        assert_eq!(singleton_lock_owner_pid(&human_chrome), Some(me));
        assert_eq!(
            unmanaged_live_lock_owner(home.path(), &human_chrome),
            Some(me),
            "a live non-amux owner must stop the launch before Chrome sees its URL"
        );

        // CONTROL 1: lock presence is not liveness. A crashed Chrome can leave
        // this symlink behind; treating every leftover as live would wedge the
        // human profile forever.
        std::fs::remove_file(&lock).unwrap();
        let mut exited = std::process::Command::new("true").spawn().unwrap();
        let stale_pid = exited.id();
        exited.wait().unwrap();
        std::os::unix::fs::symlink(format!("host-{stale_pid}"), &lock).unwrap();
        assert_eq!(unmanaged_live_lock_owner(home.path(), &human_chrome), None);

        // CONTROL 2: amux-owned dirs use the existing stale-lock cleanup and
        // tracked-browser replacement path. This guard must never turn that
        // ordinary path into a human-Chrome refusal.
        std::fs::remove_file(&lock).unwrap();
        let owned = home.path().join("playwright-auth/profiles/x");
        std::fs::create_dir_all(&owned).unwrap();
        std::os::unix::fs::symlink(format!("host-{me}"), owned.join("SingletonLock")).unwrap();
        assert_eq!(unmanaged_live_lock_owner(home.path(), &owned), None);
    }

    #[test]
    fn amux_owned_boundary() {
        let home = fake_home();
        assert!(is_amux_owned(home.path(), &home.path().join("playwright-auth/profiles/x")));
        assert!(is_amux_owned(home.path(), &home.path().join("playwright-auth/profile")));
        assert!(!is_amux_owned(home.path(), &chrome_user_data_dir()));
        assert!(!is_amux_owned(home.path(), Path::new("/tmp/elsewhere")));
    }

    // ---- CDP client + driver mechanics (hermetic — fake WS, temp dirs) ----

    /// The client's one job: match responses by id THROUGH interleaved
    /// events, and surface CDP errors as errors. A fake Chrome answers every
    /// call with an event first — a client that read frames positionally
    /// would return the event as the result.
    #[tokio::test]
    async fn cdp_client_skips_events_matches_ids_and_surfaces_errors() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            use tokio_tungstenite::tungstenite::Message;
            while let Some(Ok(msg)) = ws.next().await {
                if let Message::Text(t) = msg {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    let id = v["id"].as_u64().unwrap();
                    let method = v["method"].as_str().unwrap_or("");
                    // Interleaved EVENT before every response.
                    ws.send(Message::Text(
                        json!({ "method": "Page.frameNavigated", "params": {} }).to_string(),
                    ))
                    .await
                    .unwrap();
                    let resp = if method == "Deliberate.fail" {
                        json!({ "id": id, "error": { "message": "boom" } })
                    } else {
                        json!({ "id": id, "result": { "echo": method } })
                    };
                    ws.send(Message::Text(resp.to_string())).await.unwrap();
                }
            }
        });
        let mut c = CdpClient::connect(&format!("ws://{addr}")).await.unwrap();
        let five = std::time::Duration::from_secs(5);
        let r = c.call("Page.navigate", json!({}), five).await.unwrap();
        assert_eq!(r["echo"], "Page.navigate");
        let r = c.call("Runtime.evaluate", json!({}), five).await.unwrap();
        assert_eq!(r["echo"], "Runtime.evaluate", "second call matches its own id");
        let err = c.call("Deliberate.fail", json!({}), five).await.unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
    }

    /// AMUX-3712: the capture activates the tab first, and a failure to activate
    /// does not abort the capture.
    ///
    /// THE SPECIMEN: `GET /api/browser/screenshot` returned 502 with "CDP
    /// Page.captureScreenshot timed out after 30s" at 11:12:41 and again at
    /// 12:06:17 on 2026-08-25, at 30,006ms and 30,003ms. The deadline to the
    /// millisecond, twice, is not a slow capture — it is a capture that never
    /// returns. `Page.captureScreenshot` waits on a compositor frame, and the
    /// browser in question had its active surface on an omnibox popup, so the
    /// page being captured was not compositing at all.
    ///
    /// BOTH CELLS MATTER. The order cell is the fix. The tolerate-failure cell
    /// is what stops the fix becoming a second way for a wedged renderer to
    /// fail: bringToFront cannot revive a dead renderer, so if it errors the
    /// capture must still be attempted rather than short-circuiting into a
    /// different error message for the same fault.
    #[tokio::test]
    async fn a_capture_activates_the_tab_first_and_survives_a_failed_activation() {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        // 1x1 transparent PNG, so the capture path writes real bytes.
        const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

        // `fail_front` drives the second cell: the fake refuses bringToFront.
        let serve = |fail_front: bool, seen: Arc<Mutex<Vec<String>>>| async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                use tokio_tungstenite::tungstenite::Message;
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(t) = msg {
                        let v: Value = serde_json::from_str(&t).unwrap();
                        let id = v["id"].as_u64().unwrap();
                        let method = v["method"].as_str().unwrap_or("").to_string();
                        seen.lock().unwrap().push(method.clone());
                        let resp = match method.as_str() {
                            "Page.bringToFront" if fail_front => {
                                json!({"id": id, "error": {"message": "not attached to an active page"}})
                            }
                            "Page.captureScreenshot" => json!({"id": id, "result": {"data": PNG_B64}}),
                            _ => json!({"id": id, "result": {}}),
                        };
                        ws.send(Message::Text(resp.to_string())).await.unwrap();
                    }
                }
            });
            addr
        };

        let home = fake_home();

        // CELL 1 — the order. bringToFront must precede the capture, or a
        // backgrounded tab blocks for 30s instead of being made visible.
        let addr = serve(false, seen.clone()).await;
        let mut c = CdpClient::connect(&format!("ws://{addr}")).await.unwrap();
        let (path, size) = screenshot_to_file(&mut c, home.path(), "cell1").await.expect("capture");
        assert!(size > 0 && path.exists(), "the capture wrote real bytes");
        let calls = seen.lock().unwrap().clone();
        let front = calls.iter().position(|m| m == "Page.bringToFront");
        let shot = calls.iter().position(|m| m == "Page.captureScreenshot");
        assert!(front.is_some(), "the tab must be activated before capturing: {calls:?}");
        assert!(front < shot, "activation must come FIRST, not after: {calls:?}");

        // CELL 2 — a refused activation must not abort the capture. Without
        // this, a wedged renderer would report "not attached to an active page"
        // instead of the timeout that actually describes it, and the fix would
        // have added a failure mode rather than removed one.
        let seen2: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let addr2 = serve(true, seen2.clone()).await;
        let mut c2 = CdpClient::connect(&format!("ws://{addr2}")).await.unwrap();
        let (_, size2) = screenshot_to_file(&mut c2, home.path(), "cell2")
            .await
            .expect("a refused bringToFront must not fail the capture");
        assert!(size2 > 0);
        assert!(
            seen2.lock().unwrap().iter().any(|m| m == "Page.captureScreenshot"),
            "the capture must still be attempted after a failed activation: {:?}",
            seen2.lock().unwrap()
        );
    }

    /// The server-machine invariant, hermetically: a CDP endpoint that is
    /// not loopback is refused BEFORE any connection attempt — automation
    /// can only ever reach the Chrome this server launched.
    #[tokio::test]
    async fn cdp_client_refuses_non_loopback_endpoints() {
        for url in [
            "ws://192.168.1.50:9222/devtools/page/X",
            "ws://client.example.com:9222/devtools/page/X",
            "wss://example.com/devtools/page/X",
        ] {
            let err = match CdpClient::connect(url).await {
                Err(e) => e,
                Ok(_) => panic!("{url}: non-loopback endpoint must be refused"),
            };
            assert!(err.to_string().contains("refusing non-loopback"), "{url}: {err}");
        }
    }

    #[test]
    fn obs_cap_truncates_and_says_so() {
        assert_eq!(obs_cap("short", 100), "short");
        let capped = obs_cap(&"x".repeat(50), 10);
        assert!(capped.starts_with("xxxxxxxxxx\n…[truncated at 10 chars"), "{capped}");
        // Multibyte safety: a byte-index slice would panic here.
        let capped = obs_cap(&"é".repeat(50), 10);
        assert!(capped.contains("truncated at 10"), "{capped}");
    }

    /// AMUX-3062: the structured half. obs_truncation reports Some(full_len) on
    /// the same predicate obs_cap truncates on, and None otherwise, so the eval
    /// envelope can carry truncated=true + the original length instead of the
    /// caller substring-matching the notice out of an {ok:true} result.
    #[test]
    fn obs_truncation_reports_the_original_length_only_when_it_cut() {
        assert_eq!(obs_truncation("short", 100), None, "fits => no signal");
        assert_eq!(obs_truncation(&"x".repeat(10), 10), None, "exactly at the cap is not truncated");
        assert_eq!(obs_truncation(&"x".repeat(50), 10), Some(50), "over the cap => full length");
        // Counts CHARS, matching obs_cap, so a multibyte page is not mis-measured.
        assert_eq!(obs_truncation(&"é".repeat(50), 10), Some(50), "multibyte counted by char");
    }

    #[test]
    fn click_outcome_discriminates() {
        let ok = click_outcome(&json!("OK|A|More information..."), "selector \"a\"", "h");
        assert_eq!(ok["ok"], json!(true));
        assert_eq!(ok["clicked"]["tag"], "A");
        for (raw, needle) in [
            ("NOMATCH", "no element matches"),
            ("NOELEMENT", "no element matches"),
            ("NOTVISIBLE", "zero size"),
            ("STALE", "stale"),
        ] {
            let v = click_outcome(&json!(raw), "x", "h");
            assert!(
                v["error"].as_str().unwrap().contains(needle),
                "{raw}: {v}"
            );
        }
        // Garbage (page navigated mid-click) is an error, not a silent ok.
        assert!(click_outcome(&json!(null), "x", "h")["error"].is_string());
    }

    #[test]
    fn js_blob_substitution() {
        let js = inspect_js(300, true);
        assert!(js.contains("var L = 300;"));
        assert!(js.contains("if (true) {"));
        assert!(!js.contains("__LIMIT__") && !js.contains("__CLEAR__"));
        let js = state_js();
        assert!(!js.contains("__EL_LIMIT__") && !js.contains("__TEXT_CAP__"));
        assert!(js.contains("window.__amux_els"));
        // THE CAP MUST DISCLOSE ITSELF (AMUX-3721). The state surface renders
        // STATE_EL_LIMIT elements out of however many are visible, and it used
        // to say nothing about the difference — so a caller got a full-looking
        // list of exactly 120, could not find the element it wanted, and had
        // only one available conclusion: "it is not on the page". Measured live
        // on the amux dashboard: 158 visible, 120 returned, and the two elements
        // being looked for sat at indices 155 and 156 — addressable the whole
        // time, since click-by-index resolves against `seen`, not `els`.
        //
        // SCOPE, honestly: this asserts the disclosure is EMITTED, not that its
        // arithmetic is right — the arithmetic runs in a browser, and no
        // assertion in this process can execute it. It is a
        // deletion/regression guard. The arithmetic was verified against the
        // live dashboard instead, which is the only place it can be.
        for f in [
            "elements_total",
            "elements_shown",
            "elements_truncated",
            "elements_note",
            "seen.length > els.length",
        ] {
            assert!(js.contains(f), "state JS must disclose its cap, missing {f}: {js}");
        }
        // Selector JSON-encodes through format!: quotes must survive.
        let js = selector_click_js("a[href=\"x\"]");
        assert!(js.contains("querySelector(\"a[href=\\\"x\\\"]\")"), "{js}");
    }

    #[test]
    fn registry_register_appends_domains_and_survives_reload() {
        let home = fake_home();
        let e = registry_register(home.path(), "gh", "github.com", "GH").unwrap();
        assert_eq!(e["domains"], json!(["github.com"]));
        assert_eq!(e["label"], "GH");
        // Second host appends; duplicate host does not; label sticks.
        registry_register(home.path(), "gh", "gist.github.com", "").unwrap();
        let e = registry_register(home.path(), "gh", "github.com", "").unwrap();
        assert_eq!(e["domains"], json!(["github.com", "gist.github.com"]));
        assert_eq!(e["label"], "GH");
        assert!(e["updated"].as_i64().unwrap() > 0);
        // And the listing path (registry_load) sees the same bytes.
        let reg = registry_load(home.path());
        assert_eq!(reg["gh"]["domains"], json!(["github.com", "gist.github.com"]));
    }

    #[test]
    fn pw_profiles_lists_dirs_plus_default() {
        let home = fake_home();
        std::fs::create_dir_all(home.path().join("playwright-auth/profiles/github")).unwrap();
        std::fs::create_dir_all(home.path().join("playwright-auth/profiles/.hidden")).unwrap();
        std::fs::create_dir_all(home.path().join("playwright-auth/profile")).unwrap();
        assert_eq!(pw_profiles(home.path()), vec!["default".to_string(), "github".to_string()]);
    }

    #[test]
    fn delete_profile_guards() {
        let home = fake_home();
        let dir = home.path().join("playwright-auth/profiles/x");
        std::fs::create_dir_all(&dir).unwrap();
        registry_register(home.path(), "x", "x.com", "").unwrap();

        assert_eq!(delete_profile(home.path(), "default").unwrap_err().0, 400);
        assert_eq!(
            delete_profile(home.path(), "amux-native-test-nonexistent-9f3a").unwrap_err().0,
            404
        );
        let ok = delete_profile(home.path(), "x").unwrap();
        assert_eq!(ok["deleted"], "x");
        assert!(!dir.exists());
        assert!(registry_load(home.path()).get("x").is_none(), "registry entry pruned");
    }

    #[test]
    fn safe_file_component_flattens_path_shapes() {
        assert_eq!(safe_file_component("amux"), "amux");
        assert_eq!(safe_file_component("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(safe_file_component(""), "amux");
        assert_eq!(safe_file_component(".."), "amux");
    }

    /// Live-Chrome smoke test: launches a real Chrome on a THROWAWAY temp
    /// user-data-dir (never a saved profile), checks CDP answers, stops it.
    /// #[ignore]d: `cargo test -- --ignored` runs it only where a human asked.
    #[tokio::test]
    #[ignore = "launches a real Chrome; run explicitly with -- --ignored"]
    async fn live_chrome_start_stop() {
        let _serial = LIVE_BROWSER.lock().await;
        if chrome_binary().is_none() {
            eprintln!("no Chrome binary on this machine; skipping");
            return;
        }
        let home = fake_home();
        // Use a scratch profile name so the target dir is amux-owned + temp.
        let info = start(home.path(), "default", "about:blank", "live-smoke", "live-smoke", false).await.expect("start");
        assert!(info.cdp_port > 0);
        let tabs = cdp_list(info.cdp_port).await.expect("cdp /json/list");
        assert!(tabs.is_array());

        // Server-machine pin (owner directive): the page a driver verb
        // resolves is served by the LOOPBACK CDP port of the child THIS
        // process spawned — same port, same registry pid — and a round-trip
        // eval proves the automation executed there.
        let page = resolve_page("live-smoke", None).await.map_err(|e| match e {
            DriverError::NotRunning => anyhow::anyhow!("not running"),
            DriverError::Cdp(e) => e,
        }).expect("resolve_page");
        assert_eq!(page.cdp_port, info.cdp_port, "verb port IS the spawned child's port");
        assert!(
            page.ws_url.starts_with(&format!("ws://127.0.0.1:{}/", info.cdp_port))
                || page.ws_url.starts_with(&format!("ws://localhost:{}/", info.cdp_port)),
            "loopback CDP only: {}",
            page.ws_url
        );
        {
            let guard = RUNNING.lock().expect("browser registry poisoned");
            // `pid` rather than `child.id()`: an ADOPTED browser has no Child
            // (AC-325) and the registry must still name the process it is
            // acting on. Asserting through the Option would pass vacuously the
            // day adoption is exercised here.
            let reg_pid = guard.values().next().map(|r| r.pid);
            assert_eq!(reg_pid, info.pid, "acting Chrome is the registry's process");
            assert!(reg_pid.is_some());
            assert!(
                guard.values().next().and_then(|r| r.child.as_ref()).is_some(),
                "a browser this test SPAWNED must carry its Child handle"
            );
        }
        let mut c = CdpClient::connect(&page.ws_url).await.expect("cdp ws connect");
        let v = c.eval("1+1", 10).await.expect("eval");
        assert_eq!(v, json!(2));

        NATIVE_TARGETS.lock().unwrap().remove("live-smoke");
        let report = stop(home.path()).await;
        assert!(report.stopped);
    }

    /// AC-336: one lane must not be handed the tab another lane launched.
    ///
    /// This is the incident rebuilt from its own artifact rather than a
    /// convenient case. The reported specimen was a peer's `/start {url}` tab
    /// being adopted by the next lane to call a driver verb — and it was
    /// adoptable for exactly one reason: the launch tab was never recorded in
    /// NATIVE_TARGETS, so `resolve_page` could not tell it from an unowned page.
    ///
    /// THE ORDERING IS THE WHOLE TEST, and getting it wrong made an earlier
    /// draft of this test non-discriminating. The hijack needs the PEER to
    /// resolve BEFORE the owner has ever driven its own tab — which is exactly
    /// what happened: a peer lane called `/start {url}` and never drove it, then
    /// I called a driver verb and was handed their page. If the owner resolves
    /// first it claims the launch tab under the old code too, so an
    /// owner-then-peer test passes against the bug and proves nothing.
    ///
    /// The launch tab is read from CDP, not from NATIVE_TARGETS, for the same
    /// reason: asserting on our own bookkeeping would make the test fail at the
    /// bookkeeping instead of at the behaviour, and the behaviour is the claim.
    ///
    /// Falsifiability CHECKED, not assumed: with `map.insert` in `start`
    /// removed, `peer` is handed the launch tab and the assert_ne below fires.
    #[tokio::test]
    #[ignore = "launches a real Chrome; run explicitly with -- --ignored"]
    async fn a_peers_launch_tab_is_not_adopted_by_another_lane() {
        let _serial = LIVE_BROWSER.lock().await;
        if chrome_binary().is_none() {
            eprintln!("no Chrome binary on this machine; skipping");
            return;
        }
        let home = fake_home();
        let info = start(home.path(), "default", "about:blank", "owner", "owner", false).await.expect("start");

        // The launch tab as CHROME reports it — the incident's artifact.
        let launch_tab = cdp_list(info.cdp_port)
            .await
            .expect("cdp /json/list")
            .as_array()
            .and_then(|ts| {
                ts.iter()
                    .find(|t| t.get("type").and_then(Value::as_str) == Some("page"))
                    .and_then(|t| t.get("id").and_then(Value::as_str))
                    .map(str::to_string)
            })
            .expect("a freshly launched Chrome has a page tab");

        // PEER RESOLVES FIRST — the owner has driven nothing yet.
        let peer_page = resolve_page("peer", None).await.expect("peer resolves");
        assert_ne!(
            peer_page.target_id, launch_tab,
            "peer was handed the tab the owner launched — this is AC-336"
        );
        assert_eq!(peer_page.cdp_port, info.cdp_port, "still one shared browser, per-lane tabs");

        // The owner still gets the tab it launched, not the peer's new one.
        let owner_page = resolve_page("owner", None).await.expect("owner resolves");
        assert_eq!(owner_page.target_id, launch_tab, "owner keeps the tab it launched");
        assert_ne!(owner_page.target_id, peer_page.target_id, "two lanes, two tabs");

        for s in ["owner", "peer"] {
            NATIVE_TARGETS.lock().unwrap().remove(s);
        }
        let report = stop(home.path()).await;
        assert!(report.stopped);
    }
}

#[cfg(test)]
mod launch_args_tests {
    use super::*;

    fn owned_target() -> LaunchTarget {
        LaunchTarget {
            user_data_dir: PathBuf::from("/tmp/amux-launch-args-test/profile"),
            profile_directory: None,
        }
    }

    fn is_bad(arg: &str) -> bool {
        CHROME_BAD_FLAGS.iter().any(|b| arg == *b || arg.starts_with(&format!("{b}=")))
    }

    /// MR-38: a kBadFlags entry without --test-type is the "unsupported
    /// command-line flag" infobar on every window. The CONTROL assertion comes
    /// first: if the pin is not actually in the list, the invariant is vacuous
    /// and this test pins nothing (ethos rule 7 — say what a positive looks
    /// like before believing the negative).
    #[test]
    fn bad_flags_never_launch_without_test_type() {
        let args = chrome_launch_args(1234, &owned_target(), false, Some("PIN="), "");
        let bad: Vec<&String> = args.iter().filter(|a| is_bad(a)).collect();
        assert_eq!(bad.len(), 1, "control: the SPKI pin must be in the list: {args:?}");
        assert!(
            args.iter().any(|a| a == "--test-type"),
            "a kBadFlags entry is passed without --test-type — Chrome will show the \
             unsupported-flag infobar (chrome/browser/ui/startup/infobar_utils.cc): {args:?}"
        );
        // --enable-automation would suppress the same bar but add its own
        // "controlled by automated test software" bar; it must not creep in.
        assert!(!args.iter().any(|a| a == "--enable-automation"), "{args:?}");
    }

    /// An unreadable cert means NO pin — never a blanket bypass in its place.
    #[test]
    fn no_pin_means_no_bad_flag_at_all() {
        let args = chrome_launch_args(1234, &owned_target(), false, None, "");
        assert!(!args.iter().any(|a| is_bad(a)), "{args:?}");
    }

    /// Shape Chrome depends on: the URL is the LAST positional, headless is
    /// `new`, a Chrome-dir profile gets --profile-directory, and the debug port
    /// and user-data-dir are the exact values the caller resolved.
    #[test]
    fn shape_is_what_chrome_expects() {
        let t = LaunchTarget {
            user_data_dir: PathBuf::from("/Users/x/Library/Application Support/Google/Chrome"),
            profile_directory: Some("Profile 11".into()),
        };
        let args = chrome_launch_args(4321, &t, true, Some("PIN="), "  https://example.com/  ");
        assert_eq!(args[0], "--remote-debugging-port=4321");
        assert_eq!(args[1], "--user-data-dir=/Users/x/Library/Application Support/Google/Chrome");
        assert!(args.contains(&"--headless=new".to_string()), "{args:?}");
        assert!(args.contains(&"--profile-directory=Profile 11".to_string()), "{args:?}");
        assert_eq!(args.last().map(String::as_str), Some("https://example.com/"), "url is trimmed and last");
        // An empty url adds no positional at all (Chrome would open a blank
        // window for "" — and the last arg must then be a flag).
        let none = chrome_launch_args(4321, &t, false, None, "   ");
        assert!(none.last().unwrap().starts_with("--"), "{none:?}");
    }
}

#[cfg(test)]
mod spki_pin_tests {
    use super::*;

    /// The pin must equal what OpenSSL computes for the same cert, because that
    /// is the value Chrome compares against. Generating a real keypair here and
    /// checking only "returns Some" would pass for any digest of any bytes — so
    /// this recomputes the expected answer by the independent path (public key
    /// DER -> sha256 -> base64) and, crucially, also asserts the helper does NOT
    /// simply hash the file contents, which is the plausible wrong implementation.
    #[test]
    fn spki_pin_matches_openssl_definition_and_is_not_a_file_hash() {
        use base64::Engine as _;
        use sha2::Digest as _;

        let dir = std::env::temp_dir().join(format!("amux-spki-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let tls = dir.join("tls");
        std::fs::create_dir_all(&tls).unwrap();

        let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "amux");
        let key_pair = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        std::fs::write(tls.join("cert.pem"), cert.pem()).unwrap();
        std::fs::write(tls.join("key.pem"), key_pair.serialize_pem()).unwrap();

        let got = amux_cert_spki_b64(&dir).expect("pin computed");

        // Independent recomputation of Chrome's documented input:
        // base64(sha256(DER SubjectPublicKeyInfo)).
        let want = base64::engine::general_purpose::STANDARD
            .encode(sha2::Sha256::digest(key_pair.public_key_der()));
        assert_eq!(got, want, "pin must be base64(sha256(SPKI DER))");

        // Counter-case: a digest of the PEM FILE would also be Some(...) and
        // would look correct in every "did it return a value" check, while
        // matching nothing Chrome ever computes.
        let file_hash = base64::engine::general_purpose::STANDARD
            .encode(sha2::Sha256::digest(std::fs::read(tls.join("cert.pem")).unwrap()));
        assert_ne!(got, file_hash, "pin must not be a hash of the cert file");

        // Missing material must degrade to None (no flag), never to a guess.
        std::fs::remove_file(tls.join("key.pem")).unwrap();
        assert!(amux_cert_spki_b64(&dir).is_none(), "absent key -> no pin");

        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod last_exit_persistence_tests {
    use super::*;

    /// AF-378: the exit record survives the restart that erases the in-memory one.
    ///
    /// WHY THIS IS NOT PARANOIA. The commonest browser death is "killed alongside
    /// a server restart". The adopt path records that correctly in the NEW
    /// process, so the record does survive the restart that caused the death.
    /// What erases it is the NEXT restart, unrelated and usually minutes later,
    /// because this box swaps the server binary on every commit. Reported live by
    /// tubescience: two browsers dying minutes apart, and all they saw was
    /// ConnectionRefusedError on the CDP socket, with `last_exit: null` by the
    /// time anyone looked.
    /// The CALL SITE, not just the record shape (AF-161's rule).
    ///
    /// The test below pins what `exit_record` produces. It passed unchanged when
    /// the displacement call site was mutated back to the actor-less
    /// `stop_profile`, which is a check pinning the wrong layer: exactly as green
    /// as one pinning the right layer, and worth nothing. This reads the source
    /// and fails if `start()` goes back to dropping the actor.
    ///
    /// A source scan is the weaker kind of probe, so it is bounded to the one
    /// window that matters and asserts the positive rather than the absence.
    #[test]
    fn starts_displacement_stop_names_the_actor_in_the_source() {
        let src = include_str!("browser.rs");
        let at = src
            .find("// ONE BROWSER PER PROFILE, and only this profile (AMUX-3828)")
            .expect("the displacement block's anchor comment must exist");
        let window = &src[at..src.len().min(at + 1400)];
        assert!(
            window.contains("stop_profile_as_reason(home, profile, started_by,"),
            "start()'s displacement stop must pass the actor; window was:\n{window}"
        );
        assert!(
            !window.contains("stop_profile(home, profile)"),
            "start() must not use the actor-less stop, which records by: null"
        );
    }

    /// A DISPLACED browser names who displaced it, and does not call them the
    /// stopper (AF-378 follow-up).
    ///
    /// Found by reading a value rather than a field's presence: amux-homepage
    /// verified AF-378 by checking last_exit was populated, and the value they
    /// pasted back was `{by: None, reason: stopped, started_by: mixpeek-cicd}`.
    /// The record survived a restart, which is what AF-378 shipped, and the one
    /// field the card is about was null. The path was `start()` calling the
    /// actor-less `stop_profile` while the actor sat in its own `started_by`
    /// parameter.
    ///
    /// Both halves are asserted because passing the actor alone would be a WRONG
    /// attribution: nobody asked to stop that browser, somebody asked to start a
    /// new one, and "stopped by X" reads as X having stopped it.
    #[test]
    fn a_displaced_browser_names_the_displacer_and_says_it_was_displaced() {
        let v = super::exit_record(
            "displaced by a new start", "lane-b", "default", 4242, "lane-a", 1000,
        );
        assert_eq!(v["by"], serde_json::json!("lane-b"), "the displacer must be named: {v}");
        assert_eq!(v["started_by"], serde_json::json!("lane-a"), "the displaced owner must survive: {v}");
        assert_eq!(
            v["reason"], serde_json::json!("displaced by a new start"),
            "a displacement must not be recorded as a plain stop: {v}"
        );

        // A REAL stop still reads as one, so this did not just rename everything.
        let stopped = super::exit_record("stopped", "activity-reaper", "default", 1, "lane-a", 1000);
        assert_eq!(stopped["reason"], serde_json::json!("stopped"));
        assert_eq!(stopped["by"], serde_json::json!("activity-reaper"));

        // AND AN UNNAMED ACTOR IS STILL NULL, NOT "". The two read identically to
        // a consumer and telling them apart is what the record is for (AMUX-3063).
        let anon = super::exit_record("stopped", "", "default", 1, "lane-a", 1000);
        assert!(anon["by"].is_null(), "an unnamed actor must be null, not empty: {anon}");
    }

    #[test]
    fn a_recorded_exit_outlives_the_process_that_recorded_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path();

        // CONTROL FIRST, and it is load-bearing: with nothing recorded the reader
        // must answer None. A function that returned Some(_) unconditionally
        // would pass the assertion below and mean nothing.
        assert!(
            persisted_last_exit(home).is_none(),
            "an empty home must report no persisted exit, not an invented one"
        );

        record_last_exit(
            home,
            serde_json::json!({ "reason": "found dead on adopt", "pid": 4242, "profile": "default" }),
        );

        // Reading it back is the whole point: this stands in for a LATER server
        // process, which has an empty LAST_EXIT and must still be able to say why.
        let got = persisted_last_exit(home).expect("the record must survive on disk");
        assert_eq!(got["reason"], serde_json::json!("found dead on adopt"));
        assert_eq!(got["pid"], serde_json::json!(4242));

        // TAGGED, so a record this server OBSERVED stays distinguishable from one
        // it INHERITED. Without the tag the two read identically, which is the
        // exact distinction the in-memory note was careful about.
        assert_eq!(
            got["from_disk"],
            serde_json::json!(true),
            "an inherited record must say so: {got}"
        );

        // And the in-memory copy is still set, so the fast path is unchanged.
        let mem = LAST_EXIT.lock().expect("last-exit poisoned").clone();
        assert!(mem.is_some(), "record_last_exit must still set the in-memory copy");
        assert!(
            mem.expect("checked")["from_disk"].is_null(),
            "the in-memory copy must NOT claim it came from disk"
        );
    }
}
