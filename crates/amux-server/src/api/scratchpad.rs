//! The scratchpad's settings and expiry (Ethan, 2026-10-09: "everything should
//! expire after 1 week, it should be configurable in the scratchpad tab").
//!
//! The scratchpad is a paste-and-forget dumping ground (default folder
//! ~/Vault/Scratchpad). Its config lives in `~/.amux/scratchpad.json`:
//! `retain_days` (0 keeps everything) and `dir`. The storage sweep calls
//! [`sweep`], which MOVES files older than that into
//! `<amux home>/scratchpad-expired/<date>/`; it never deletes one. The tab
//! reads and writes the config through GET/PUT /api/scratchpad/config.
//!
//! No config file means keep everything (2026-11-17). Before that a missing
//! file meant 7 days, and the folder's `~` resolves against the process HOME,
//! so every test that ran the storage sweep with a temporary amux home swept
//! the owner's real folder on a 7-day expiry while his own setting read
//! "Keep forever". Nothing logged it: a test's tracing goes nowhere.
use axum::{extract::State, http::StatusCode, response::{IntoResponse, Response}, Json};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use super::AppState;

pub const DEFAULT_RETAIN_DAYS: u64 = 0;
pub const DEFAULT_DIR: &str = "~/Vault/Scratchpad";

fn config_path(home: &Path) -> PathBuf {
    home.join("scratchpad.json")
}

/// (retain_days, dir as written, dir resolved).
pub fn config(home: &Path) -> (u64, String, PathBuf) {
    let v: Value = std::fs::read_to_string(config_path(home))
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or(Value::Null);
    let days = v.get("retain_days").and_then(Value::as_u64).unwrap_or(DEFAULT_RETAIN_DAYS);
    let dir = v.get("dir").and_then(Value::as_str).filter(|d| !d.trim().is_empty()).unwrap_or(DEFAULT_DIR).to_string();
    let resolved = match dir.strip_prefix("~/") {
        Some(rest) => std::env::var("HOME").map(PathBuf::from).unwrap_or_default().join(rest),
        None => PathBuf::from(&dir),
    };
    (days, dir, resolved)
}

/// Where expired items go, under the amux home.
pub fn expired_root(home: &Path) -> PathBuf {
    home.join("scratchpad-expired")
}

/// The sweep's plan for this home: `None` (keep everything) unless the home
/// has a saved scratchpad.json with a retention above 0.
pub fn plan(home: &Path) -> Option<(u64, PathBuf)> {
    if !config_path(home).is_file() {
        return None;
    }
    let (days, _, dir) = config(home);
    (days > 0).then_some((days, dir))
}

fn move_out(src: &Path, dst: &Path) -> bool {
    if dst.exists() {
        return false;
    }
    if let Some(parent) = dst.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    if std::fs::rename(src, dst).is_ok() {
        return true;
    }
    // Another filesystem: copy, check the length, and only then remove.
    let Ok(len) = std::fs::copy(src, dst) else { return false };
    if std::fs::metadata(src).map(|m| m.len()).ok() != Some(len) {
        if let Err(e) = std::fs::remove_file(dst) {
            tracing::warn!(file = %dst.display(), error = %e, verdict = "scratchpad_partial_copy_left",
                "a short copy of an expired scratchpad item could not be removed; the original stays");
        }
        return false;
    }
    std::fs::remove_file(src).is_ok()
}

/// Move files under `dir` last modified more than `retain_days` ago into
/// `dest`, keeping their relative paths, then remove subfolders that became
/// empty (never `dir` itself). A file that cannot be moved stays where it is.
/// Symlinks are not followed. Returns (files moved, bytes moved).
pub fn sweep_dir(dir: &Path, retain_days: u64, now: std::time::SystemTime, dest: &Path) -> (usize, u64) {
    if retain_days == 0 {
        return (0, 0);
    }
    struct Walk<'a> {
        root: &'a Path,
        dest: &'a Path,
        cutoff: std::time::Duration,
        now: std::time::SystemTime,
        n: usize,
        bytes: u64,
    }
    fn walk(d: &Path, w: &mut Walk) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(e.path()) else { continue };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                walk(&e.path(), w);
                if e.path() != w.root && std::fs::read_dir(e.path()).map(|mut r| r.next().is_none()).unwrap_or(false) {
                    let _ = std::fs::remove_dir(e.path());
                }
            } else if meta.modified().ok().and_then(|m| w.now.duration_since(m).ok()).is_some_and(|age| age > w.cutoff) {
                let Ok(rel) = e.path().strip_prefix(w.root).map(Path::to_path_buf) else { continue };
                if move_out(&e.path(), &w.dest.join(&rel)) {
                    w.n += 1;
                    w.bytes += meta.len();
                } else {
                    tracing::warn!(file = %e.path().display(), verdict = "scratchpad_expire_kept",
                        "an expired scratchpad item could not be moved out, so it stays");
                }
            }
        }
    }
    let mut w = Walk { root: dir, dest, cutoff: std::time::Duration::from_secs(retain_days * 86_400), now, n: 0, bytes: 0 };
    walk(dir, &mut w);
    (w.n, w.bytes)
}

/// The storage sweep's entry point.
pub fn sweep(home: &Path) -> (usize, u64) {
    let Some((days, dir)) = plan(home) else { return (0, 0) };
    let dest = expired_root(home).join(chrono::Local::now().format("%Y-%m-%d").to_string());
    let (n, b) = sweep_dir(&dir, days, std::time::SystemTime::now(), &dest);
    if n > 0 {
        tracing::info!(dir = %dir.display(), moved_to = %dest.display(), retain_days = days, files = n, bytes = b,
            measured = true, n_considered = n, verdict = "scratchpad_expired",
            "moved scratchpad items older than the configured retention out of the folder");
    }
    (n, b)
}

pub async fn get_config(State(_s): State<AppState>) -> Response {
    let (days, dir, _) = config(&crate::config::amux_home());
    Json(json!({"retain_days": days, "dir": dir, "default_retain_days": DEFAULT_RETAIN_DAYS})).into_response()
}

pub async fn put_config(State(_s): State<AppState>, Json(body): Json<Value>) -> Response {
    let home = crate::config::amux_home();
    let (cur_days, cur_dir, _) = config(&home);
    let days = match body.get("retain_days") {
        None => cur_days,
        Some(v) => match v.as_u64() {
            Some(d) if d <= 3650 => d,
            _ => return (StatusCode::BAD_REQUEST, Json(json!({"error": "retain_days must be a whole number of days, 0 to keep everything"}))).into_response(),
        },
    };
    let dir = body.get("dir").and_then(Value::as_str).map(str::trim).filter(|d| !d.is_empty()).map(str::to_string).unwrap_or(cur_dir);
    let out = json!({"retain_days": days, "dir": dir});
    let path = config_path(&home);
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, out.to_string()).and_then(|_| std::fs::rename(&tmp, &path)).is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "could not save the scratchpad settings"}))).into_response();
    }
    tracing::info!(retain_days = days, dir = %dir, verdict = "scratchpad_config_saved", "scratchpad settings saved");
    Json(json!({"ok": true, "retain_days": days, "dir": dir})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_items_older_than_the_retention_are_removed() {
        let d = tempfile::tempdir().unwrap();
        let old = d.path().join("old.md");
        let fresh = d.path().join("fresh.md");
        let sub = d.path().join("folder");
        std::fs::create_dir(&sub).unwrap();
        let old_in_sub = sub.join("old.png");
        for p in [&old, &fresh, &old_in_sub] {
            std::fs::write(p, "x").unwrap();
        }
        let week_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 86_400);
        for p in [&old, &old_in_sub] {
            std::fs::File::open(p).unwrap().set_modified(week_ago).unwrap();
        }
        let out = tempfile::tempdir().unwrap();
        assert_eq!(sweep_dir(d.path(), 0, std::time::SystemTime::now(), out.path()).0, 0, "0 keeps everything");
        let (n, _) = sweep_dir(d.path(), 7, std::time::SystemTime::now(), out.path());
        assert_eq!(n, 2);
        assert!(!old.exists() && !old_in_sub.exists() && fresh.exists());
        assert!(
            out.path().join("old.md").exists() && out.path().join("folder/old.png").exists(),
            "expired items are moved, never deleted"
        );
        assert!(!sub.exists(), "a folder emptied by expiry goes too");
        assert!(d.path().exists(), "the scratchpad folder itself stays");
    }

    #[test]
    fn a_home_without_saved_settings_expires_nothing() {
        // A test's temporary amux home has no scratchpad.json. It must not
        // fall back to a retention, because `~` in the default folder is the
        // real HOME's: that is how test runs swept the owner's notes.
        let home = tempfile::tempdir().unwrap();
        assert!(plan(home.path()).is_none());
        assert_eq!(config(home.path()).0, 0, "no saved setting reads as keep forever");
        std::fs::write(home.path().join("scratchpad.json"), r#"{"retain_days":0}"#).unwrap();
        assert!(plan(home.path()).is_none(), "keep forever is saved as 0");
        std::fs::write(home.path().join("scratchpad.json"), r#"{"retain_days":7,"dir":"/nowhere"}"#).unwrap();
        assert_eq!(plan(home.path()).map(|p| p.0), Some(7));
    }
}
