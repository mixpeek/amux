//! POST /api/pull — the dashboard's "⬇ Pull from remote" button (AMUX-2891).
//!
//! Python contract: py:67857, with `_install_channel` py:6093 and
//! `_git_pull_with_fallback` py:6064.
//!
//! ONE DELIBERATE DEPARTURE, and it is the whole reason this card said "port or
//! delete" rather than "port": Python pulled whenever the button was pressed.
//! On a SHARED checkout that is the incident CLAUDE.md already records — a
//! peer's pull replayed another session's unpushed commit onto origin
//! (2026-08-03), and the repo's standing rule is "staleness announces itself;
//! nothing auto-pulls... the hook reports, the human decides."
//!
//! `--ff-only` is already the safe mode: with local commits ahead it REFUSES
//! rather than rewriting anything. But it refuses with git's own wording
//! ("Not possible to fast-forward, aborting"), which says nothing about the 97
//! unpushed commits or the peer editing a file right now. So the pre-flight
//! checks here are not new safety — they are the SAME safety, made legible:
//! refuse first, and say which condition and whose work.
//!
//! What this endpoint must never become: a scheduled or background pull. It is
//! a button a human presses, and every refusal below names what the human has
//! to decide.

use super::AppState;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

const GIT_TIMEOUT: Duration = Duration::from_secs(30);

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/pull", post(pull))
}

async fn git(dir: &Path, args: &[&str], timeout: Duration) -> Option<std::process::Output> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(o)) => Some(o),
        _ => None,
    }
}

fn out_of(o: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
    .trim()
    .to_string()
}

/// py:6093 — how this copy was installed. Package-managed copies must never
/// git-pull themselves. Python walked up from `__file__`; the Rust binary's
/// equivalent anchor is `current_exe`.
fn install_channel() -> (&'static str, Option<PathBuf>) {
    let exe = std::env::current_exe().unwrap_or_default();
    let mut d = exe.parent().map(Path::to_path_buf);
    while let Some(dir) = d {
        if dir.join(".git").exists() {
            return ("source", Some(dir));
        }
        d = dir.parent().map(Path::to_path_buf);
    }
    let p = exe.to_string_lossy().to_string();
    if p.contains("/Cellar/") || p.contains("/homebrew/") || p.contains("/Homebrew/") {
        return ("brew", None);
    }
    if p.contains("site-packages") || p.contains("dist-packages") {
        return ("pip", None);
    }
    ("standalone", None)
}

/// The installed binary usually lives OUTSIDE its source tree
/// (`~/.local/bin/amux-server-rs`), so `install_channel` finds no `.git` and
/// reports `standalone` on a perfectly normal source install. `AMUX_REPO_DIR`
/// names the checkout explicitly; otherwise the server's own CWD is tried.
pub(crate) fn repo_dir() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("AMUX_REPO_DIR") {
        let p = PathBuf::from(v);
        if p.join(".git").exists() {
            return Some(p);
        }
    }
    if let ("source", Some(d)) = install_channel() {
        return Some(d);
    }
    let mut d = std::env::current_dir().ok();
    while let Some(dir) = d {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        d = dir.parent().map(Path::to_path_buf);
    }
    None
}

/// The refusals, extracted so they can be TESTED. Inline in the handler they
/// were unreachable from a test: this server resolves to `standalone` on its own
/// machine (launchd gives it no useful cwd and the binary lives outside the
/// checkout), so the live endpoint never reaches them — the most important logic
/// in this file had no way to fail.
///
/// Returns `Some(refusal_body)` when the pull must not proceed.
pub(crate) async fn preflight(dir: &Path) -> Option<serde_json::Value> {
    // A rebase/merge/cherry-pick left half-done. Pulling into one produces git
    // errors that read like a network problem.
    for (marker, what) in [
        ("rebase-merge", "a rebase"),
        ("rebase-apply", "a rebase"),
        ("MERGE_HEAD", "a merge"),
        ("CHERRY_PICK_HEAD", "a cherry-pick"),
    ] {
        if dir.join(".git").join(marker).exists() {
            return Some(json!({"ok": false, "blocked": "operation_in_progress",
                "output": format!("Refusing to pull: {what} is in progress in {}.\n\
                                   Finish or abort it first — pulling now would compound it.",
                                  dir.display())}));
        }
    }

    // A dirty tree. `--ff-only` may still succeed here and silently move files
    // under a session that is mid-edit — the failure this repo has already paid
    // for. Name the FILES rather than the count: "3 files" is not actionable.
    if let Some(o) = git(dir, &["status", "--porcelain"], GIT_TIMEOUT).await {
        let dirty: Vec<String> = out_of(&o)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        if !dirty.is_empty() {
            let shown: Vec<String> = dirty.iter().take(20).cloned().collect();
            return Some(json!({"ok": false, "blocked": "dirty_tree", "files": shown.clone(),
                "output": format!(
                    "Refusing to pull: {} uncommitted change(s) in {}.\n{}\n\n\
                     This is a shared checkout — those edits may belong to another session \
                     that is not even running. Commit or stash them first.",
                    dirty.len(), dir.display(), shown.join("\n"))}));
        }
    }

    // Local commits not on the remote. `--ff-only` refuses these anyway, but
    // with git's own wording, which never mentions that the commits exist or
    // how many — and on this checkout the answer has been 100.
    if let Some(o) = git(dir, &["rev-list", "--count", "@{u}..HEAD"], GIT_TIMEOUT).await {
        if o.status.success() {
            let ahead: i64 = out_of(&o).trim().parse().unwrap_or(0);
            if ahead > 0 {
                return Some(json!({"ok": false, "blocked": "unpushed_commits", "ahead": ahead,
                    "output": format!(
                        "Refusing to pull: {ahead} local commit(s) are not on the remote.\n\
                         --ff-only would abort anyway; refusing here so the reason is legible.\n\
                         On a shared checkout these commits may not all be yours — check the \
                         Amux-Session trailers on `git log @{{u}}..HEAD` before pushing them.")}));
            }
        }
    }
    None
}

async fn pull() -> Response {
    match install_channel().0 {
        "brew" => {
            return Json(json!({"ok": false, "channel": "brew",
                "output": "Installed via Homebrew — update with: brew upgrade amux"}))
            .into_response()
        }
        "pip" => {
            return Json(json!({"ok": false, "channel": "pip",
                "output": "Installed via pip/pipx — update with: pipx upgrade amux (or pip install -U amux)"}))
            .into_response()
        }
        _ => {}
    }

    let Some(dir) = repo_dir() else {
        return Json(json!({"ok": false, "channel": "standalone",
            "output": "No git repo here — updates deploy automatically.\n\
                       (If this IS a source install, set AMUX_REPO_DIR to the checkout.)"}))
        .into_response();
    };

    if let Some(refusal) = preflight(&dir).await {
        return Json(refusal).into_response();
    }

    // ---- the pull itself ---------------------------------------------------
    let Some(o) = git(&dir, &["pull", "--ff-only"], GIT_TIMEOUT).await else {
        return Json(json!({"ok": false, "output": "git pull timed out after 30s"})).into_response();
    };
    let output = out_of(&o);
    if o.status.success() {
        tracing::info!(repo = %dir.display(), "pull ok");
        return Json(json!({"ok": true, "output": output})).into_response();
    }

    // py:6064 — amux is a PUBLIC repo, so updating must never depend on the
    // user's git credentials. A cron/launchd env with no SSH agent was silently
    // failing hourly (2026-07-17); retry anonymously over HTTPS.
    let lower = output.to_lowercase();
    let auth_failed = ["permission denied", "authentication failed", "could not read from remote",
                       "publickey", "terminal prompts disabled"]
        .iter()
        .any(|k| lower.contains(k));
    if auth_failed {
        let url = git(&dir, &["remote", "get-url", "origin"], GIT_TIMEOUT)
            .await
            .map(|o| out_of(&o))
            .unwrap_or_default();
        if let Some(https) = https_equiv(&url) {
            let branch = git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"], GIT_TIMEOUT)
                .await
                .map(|o| out_of(&o))
                .filter(|b| !b.is_empty())
                .unwrap_or_else(|| "main".into());
            if let Some(o2) = git(
                &dir,
                &["-c", "credential.helper=", "pull", "--ff-only", &https, &branch],
                GIT_TIMEOUT,
            )
            .await
            {
                if o2.status.success() {
                    return Json(json!({"ok": true,
                        "output": format!("origin auth failed — pulled anonymously via {https}\n{}", out_of(&o2))}))
                    .into_response();
                }
                return Json(json!({"ok": false,
                    "output": format!("{output}\n[https fallback also failed] {}", out_of(&o2))}))
                .into_response();
            }
        }
    }
    tracing::warn!(repo = %dir.display(), %output, "pull failed");
    Json(json!({"ok": false, "output": output})).into_response()
}

/// `git@host:owner/repo(.git)` / `ssh://git@host/owner/repo` -> https form.
/// Returns None for anything already https or unrecognised — a malformed URL
/// must not be handed to `git pull` as if it were a remote.
fn https_equiv(url: &str) -> Option<String> {
    let u = url.trim();
    if u.starts_with("https://") {
        return Some(u.to_string());
    }
    if let Some(rest) = u.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        return Some(format!("https://{host}/{}", path.trim_end_matches(".git")));
    }
    if let Some(rest) = u.strip_prefix("ssh://git@") {
        let (host, path) = rest.split_once('/')?;
        return Some(format!("https://{host}/{}", path.trim_end_matches(".git")));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway repo with one commit and an upstream, so `@{u}..HEAD` is a
    /// real question rather than an error.
    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str], cwd: &Path| {
            std::process::Command::new("git")
                .args(args).current_dir(cwd)
                .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
                .output().unwrap()
        };
        // A bare "remote" so the branch can have a real upstream.
        let remote = d.path().join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        run(&["init", "--bare", "-b", "main", remote.to_str().unwrap()], d.path());
        let work = d.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        run(&["init", "-b", "main"], &work);
        std::fs::write(work.join("a.txt"), "one\n").unwrap();
        run(&["add", "."], &work);
        run(&["commit", "-m", "first"], &work);
        run(&["remote", "add", "origin", remote.to_str().unwrap()], &work);
        run(&["push", "-u", "origin", "main"], &work);
        d
    }

    /// The refusal that matters most: a dirty tree on a SHARED checkout, where
    /// the uncommitted edit may belong to a session that is not even running.
    #[tokio::test]
    async fn a_dirty_tree_is_refused_and_the_files_are_named() {
        let d = repo();
        let work = d.path().join("work");
        // CONTROL: clean and level with the remote — nothing to refuse.
        assert!(preflight(&work).await.is_none(), "a clean, up-to-date repo must pull");

        std::fs::write(work.join("a.txt"), "edited by a peer\n").unwrap();
        let r = preflight(&work).await.expect("a dirty tree must be refused");
        assert_eq!(r["blocked"], "dirty_tree");
        // The FILE, not just a count — "1 file" tells nobody what to look at.
        assert_eq!(r["files"][0].as_str().unwrap().trim(), "M a.txt");
    }

    #[tokio::test]
    async fn unpushed_commits_are_refused_with_their_count() {
        let d = repo();
        let work = d.path().join("work");
        std::fs::write(work.join("b.txt"), "two\n").unwrap();
        for args in [vec!["add", "."], vec!["commit", "-m", "local only"]] {
            std::process::Command::new("git").args(&args).current_dir(&work)
                .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
                .output().unwrap();
        }
        let r = preflight(&work).await.expect("unpushed commits must be refused");
        assert_eq!(r["blocked"], "unpushed_commits");
        assert_eq!(r["ahead"], 1);
    }

    #[tokio::test]
    async fn a_half_finished_merge_is_refused_before_anything_else() {
        let d = repo();
        let work = d.path().join("work");
        std::fs::write(work.join(".git/MERGE_HEAD"), "deadbeef\n").unwrap();
        let r = preflight(&work).await.expect("an in-progress merge must be refused");
        assert_eq!(r["blocked"], "operation_in_progress");
        assert!(r["output"].as_str().unwrap().contains("a merge"));
    }

    #[test]
    fn ssh_remotes_convert_and_junk_does_not() {
        assert_eq!(
            https_equiv("git@github.com:ethan/amux.git").as_deref(),
            Some("https://github.com/ethan/amux")
        );
        assert_eq!(
            https_equiv("ssh://git@github.com/ethan/amux").as_deref(),
            Some("https://github.com/ethan/amux")
        );
        assert_eq!(
            https_equiv("https://github.com/ethan/amux").as_deref(),
            Some("https://github.com/ethan/amux")
        );
        // The controls that matter: an unrecognised remote must yield None, not
        // a half-built string. Handing `git pull` a malformed URL is how an
        // "update" turns into a confusing network error.
        assert_eq!(https_equiv("git@github.com"), None, "no colon, no path");
        assert_eq!(https_equiv(""), None);
        assert_eq!(https_equiv("/local/path/repo.git"), None);
    }
}
