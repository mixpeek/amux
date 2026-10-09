//! The scratchpad's settings and expiry (Ethan, 2026-10-09: "everything should
//! expire after 1 week, it should be configurable in the scratchpad tab").
//!
//! The scratchpad is a paste-and-forget dumping ground (default folder
//! ~/Vault/Scratchpad). Its config lives in `~/.amux/scratchpad.json`:
//! `retain_days` (default 7; 0 keeps everything) and `dir`. The storage sweep
//! calls [`sweep`] to delete files older than that, and the tab reads and
//! writes the config through GET/PUT /api/scratchpad/config.
use axum::{extract::State, http::StatusCode, response::{IntoResponse, Response}, Json};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use super::AppState;

pub const DEFAULT_RETAIN_DAYS: u64 = 7;
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

/// Delete files under `dir` last modified more than `retain_days` ago, then
/// remove subfolders that became empty (never `dir` itself). Symlinks are not
/// followed. Returns (files removed, bytes freed).
pub fn sweep_dir(dir: &Path, retain_days: u64, now: std::time::SystemTime) -> (usize, u64) {
    if retain_days == 0 {
        return (0, 0);
    }
    let cutoff = std::time::Duration::from_secs(retain_days * 86_400);
    let (mut n, mut bytes) = (0usize, 0u64);
    fn walk(d: &Path, root: &Path, cutoff: std::time::Duration, now: std::time::SystemTime, n: &mut usize, bytes: &mut u64) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(e.path()) else { continue };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                walk(&e.path(), root, cutoff, now, n, bytes);
                if e.path() != root && std::fs::read_dir(e.path()).map(|mut r| r.next().is_none()).unwrap_or(false) {
                    let _ = std::fs::remove_dir(e.path());
                }
            } else if meta.modified().ok().and_then(|m| now.duration_since(m).ok()).is_some_and(|age| age > cutoff)
                && std::fs::remove_file(e.path()).is_ok()
            {
                *n += 1;
                *bytes += meta.len();
            }
        }
    }
    walk(dir, dir, cutoff, now, &mut n, &mut bytes);
    (n, bytes)
}

/// The storage sweep's entry point.
pub fn sweep(home: &Path) -> (usize, u64) {
    let (days, _, dir) = config(home);
    let (n, b) = sweep_dir(&dir, days, std::time::SystemTime::now());
    if n > 0 {
        tracing::info!(dir = %dir.display(), retain_days = days, files = n, bytes = b, measured = true, n_considered = n,
            verdict = "scratchpad_expired", "expired scratchpad items older than the configured retention");
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
        assert_eq!(sweep_dir(d.path(), 0, std::time::SystemTime::now()).0, 0, "0 keeps everything");
        let (n, _) = sweep_dir(d.path(), 7, std::time::SystemTime::now());
        assert_eq!(n, 2);
        assert!(!old.exists() && !old_in_sub.exists() && fresh.exists());
        assert!(!sub.exists(), "a folder emptied by expiry goes too");
        assert!(d.path().exists(), "the scratchpad folder itself stays");
    }
}
