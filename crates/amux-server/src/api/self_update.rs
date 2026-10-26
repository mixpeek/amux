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
//! `--ff-only` is already the safe mode: it allows both an ordinary remote
//! fast-forward and the harmless "local branch is ahead, remote unchanged"
//! no-op. It refuses only when the histories have diverged. Pre-flight retains
//! the shared-checkout protections that git cannot express: never move files
//! under an uncommitted edit or a half-finished operation.
//!
//! A refused LOCAL pull must not also prevent the isolated activation authority
//! from seeing a remote update. When the shared checkout is dirty or mid-git
//! operation, the button fetches only its upstream remote-tracking ref. Fetching
//! does not move HEAD, the index, or any worktree file; the clean detached
//! activation worktree can then adopt that measured ref on its normal pass.
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
    Router::new().route("/api/pull", post(pull).get(pull_status))
}

// ---- deploy status: what this server runs, against main -----------------
//
// Ethan, 2026-10-24: "i should easily keep the mac server up to date". The
// button used to report the shared checkout's `git pull` ("Already up to
// date") while the server ran a hand build of a fork two weeks behind main,
// and while the CI gate held every deploy. Neither fact reached the Settings
// card. Now GET /api/pull says what runs, what main has, and what the gate is
// doing, and POST adds the same report after it fetches and starts the builder.

/// The ref the activation authority deploys from (its launchd plist sets it).
fn activation_ref() -> String {
    std::env::var("AMUX_RS_ACTIVATION_REF").unwrap_or_else(|_| "origin/main".into())
}

pub(crate) fn gate_state() -> Option<serde_json::Value> {
    let path = std::env::var("AMUX_ACTIVATION_GATE_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::api::session_verbs::home().join(".amux/activation-gate.json"));
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(9)]
}

/// (state, sentence) from the measured facts. Pure, so every branch is tested.
/// `tip` is None when main could not be read; `on_main` is whether the live
/// commit is an ancestor of it; `behind` counts main's commits the server lacks.
pub(crate) fn deploy_summary(
    live: &str,
    tip: Option<&str>,
    on_main: bool,
    behind: u64,
    gate: Option<&serde_json::Value>,
) -> (&'static str, String) {
    let Some(tip) = tip else {
        return ("unmeasured", format!("Server runs {}. main could not be read here, so it cannot say whether that is current.", short(live)));
    };
    let target = gate.and_then(|g| g["target"].as_str()).filter(|t| !t.is_empty());
    let tip_state = gate.and_then(|g| g["tip_state"].as_str()).unwrap_or("unknown");
    if !on_main {
        return ("off_main", match target {
            Some(t) => format!("Server runs {}, which is not on main (a local or forked build). The builder replaces it with {}, the newest main commit that passed CI.", short(live), short(t)),
            None => format!("Server runs {}, which is not on main (a local or forked build). No main commit has passed CI recently, so the builder has nothing to replace it with yet.", short(live)),
        });
    }
    if live == tip || behind == 0 {
        return ("current", format!("Up to date: server runs {}, the latest commit on main.", short(live)));
    }
    if let Some(t) = target.filter(|t| *t != live) {
        return ("deploying", format!("Updating: the builder is deploying {}, the newest main commit that passed CI. Server runs {}, {behind} commit(s) behind main.", short(t), short(live)));
    }
    let why = match tip_state {
        "red" => format!("main is failing CI at {}", short(tip)),
        "pending" => format!("CI is still running on {}", short(tip)),
        _ => format!("main's newer commits have not passed CI ({} is {tip_state})", short(tip)),
    };
    ("held", format!("Server runs {}, the newest main commit that passed CI. main is {behind} commit(s) ahead; they deploy once they pass CI ({why}).", short(live)))
}

async fn deploy_status(dir: Option<&Path>) -> serde_json::Value {
    let full = env!("AMUX_BUILD_COMMIT_FULL");
    let live = full.trim_end_matches("-dirty");
    let gate = gate_state();
    let mut tip: Option<String> = None;
    let (mut on_main, mut behind) = (false, 0u64);
    if let Some(dir) = dir {
        let r = activation_ref();
        tip = git(dir, &["rev-parse", "--verify", "-q", &format!("{r}^{{commit}}")], GIT_TIMEOUT)
            .await
            .filter(|o| o.status.success())
            .map(|o| out_of(&o))
            .filter(|s| !s.is_empty());
        if let Some(t) = tip.as_deref() {
            on_main = git(dir, &["merge-base", "--is-ancestor", live, t], GIT_TIMEOUT)
                .await
                .is_some_and(|o| o.status.success());
            if on_main {
                behind = git(dir, &["rev-list", "--count", &format!("{live}..{t}")], GIT_TIMEOUT)
                    .await
                    .and_then(|o| out_of(&o).parse().ok())
                    .unwrap_or(0);
            }
        }
    }
    let (state, summary) = deploy_summary(live, tip.as_deref(), on_main, behind, gate.as_ref());
    if state == "off_main" {
        tracing::warn!(live = %live, tip = ?tip, measured = true, n_considered = 1, verdict = "server_off_main",
            "the running server is not a build of main");
    }
    json!({
        "measured": tip.is_some(),
        "n_considered": 1,
        "state": state,
        "summary": summary,
        "live": live,
        "live_dirty": full.ends_with("-dirty"),
        "ref": activation_ref(),
        "tip": tip,
        "on_main": on_main,
        "behind": behind,
        "gate": gate,
        // The checkout this server builds from, so /amux-contribute can find it.
        "repo": dir.map(|d| d.display().to_string()),
    })
}

/// Start the builder now instead of on its next 60s tick. Best effort: the
/// answer says whether it ran, and the timer catches up either way.
async fn kick_builder() -> serde_json::Value {
    let (prog, args): (&str, Vec<String>) = if cfg!(target_os = "macos") {
        let uid = unsafe { libc::getuid() };
        ("launchctl", vec!["kickstart".into(), format!("gui/{uid}/com.amux.server-rs-builder")])
    } else {
        ("systemctl", vec!["--user".into(), "start".into(), "amux-builder.service".into()])
    };
    let run = tokio::process::Command::new(prog)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .output();
    match tokio::time::timeout(GIT_TIMEOUT, run).await {
        Ok(Ok(o)) if o.status.success() => json!({"kicked": true}),
        Ok(Ok(o)) => json!({"kicked": false, "why": out_of(&o)}),
        Ok(Err(e)) => json!({"kicked": false, "why": e.to_string()}),
        Err(_) => json!({"kicked": false, "why": "timed out"}),
    }
}

async fn pull_status() -> Response {
    Json(deploy_status(repo_dir().as_deref()).await).into_response()
}

/// The scoped switch that makes "Pull from remote" send local work upstream as
/// a pull request and then replace it with remote main (`amux contribute
/// --then-pull`). Cloud servers turn it on (cloud/desktop bootstrap.sh and
/// update.sh): one owner, one checkout. Off by default, because a shared
/// checkout holds other workers' uncommitted drafts, which are not the owner's
/// to publish or replace.
pub(crate) const PULL_SENDS_LOCAL_KEY: &str = "AMUX_PULL_SENDS_LOCAL_AS_PR";

fn pull_sends_local_as_pr() -> bool {
    super::settings::effective_env(&crate::config::amux_home(), PULL_SENDS_LOCAL_KEY)
        .is_some_and(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
}

/// The amux CLI beside this server's binary (both install to the same bin
/// dir), else whatever `amux` the PATH finds.
fn amux_cli() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("amux")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("amux"))
}

/// The `AMUX_CONTRIBUTE {...}` line `amux contribute --json` ends with.
pub(crate) fn contribute_result(output: &str) -> Option<serde_json::Value> {
    output
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("AMUX_CONTRIBUTE "))
        .and_then(|j| serde_json::from_str(j).ok())
}

/// Send the checkout's local work as a pull request, then match remote main.
/// Ok carries the CLI's result; Err is the whole reply for a pull that must
/// not go on, because the local work is not safely on GitHub yet.
async fn contribute_then_override(dir: &Path) -> Result<serde_json::Value, serde_json::Value> {
    let cli = amux_cli();
    let run = tokio::process::Command::new(&cli)
        .args(["contribute", "--then-pull", "--json", "--repo"])
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(Duration::from_secs(300), run).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, cli = %cli.display(), measured = false, n_considered = 1,
                verdict = "pull_contribute_unavailable", "Pull from remote could not run amux contribute; nothing was replaced");
            return Err(json!({"ok": false, "output": format!("Could not run amux contribute ({e}). Nothing was replaced.")}));
        }
        Err(_) => {
            tracing::warn!(measured = false, n_considered = 1, verdict = "pull_contribute_timed_out",
                "amux contribute did not finish within 300s; nothing was replaced");
            return Err(json!({"ok": false, "output": "Sending your local changes as a pull request took over 5 minutes. Nothing was replaced."}));
        }
    };
    let text = out_of(&out);
    let result = contribute_result(&text).unwrap_or_else(|| json!({"verdict": "no_result"}));
    let verdict = result["verdict"].as_str().unwrap_or("no_result").to_string();
    let shown: String = text.lines().filter(|l| !l.starts_with("AMUX_CONTRIBUTE ")).collect::<Vec<_>>().join("\n");
    match out.status.code() {
        Some(0) => {
            tracing::info!(verdict = %verdict, pr_url = result["pr_url"].as_str().unwrap_or(""), measured = true, n_considered = 1,
                "Pull from remote: local work handled by amux contribute");
            Ok(json!({"verdict": verdict, "pr_url": result["pr_url"], "output": shown}))
        }
        code => {
            tracing::warn!(verdict = %verdict, exit = ?code, measured = true, n_considered = 1,
                "Pull from remote stopped: the local work could not be sent as a pull request, so nothing was replaced");
            Err(json!({"ok": false, "needs_github": code == Some(4) && verdict == "github_not_connected",
                "contribute": {"verdict": verdict}, "output": shown}))
        }
    }
}

async fn pull() -> Response {
    let mut contribute = serde_json::Value::Null;
    if let (Some(dir), true) = (repo_dir(), pull_sends_local_as_pr()) {
        match contribute_then_override(&dir).await {
            Ok(v) => contribute = v,
            Err(v) => return Json(v).into_response(),
        }
    }
    let resp = pull_checkout().await;
    let (parts, body) = resp.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 1 << 20).await else {
        return Json(json!({"ok": false, "output": "could not read the pull result"})).into_response();
    };
    let mut v: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({"ok": false}));
    let dir = repo_dir();
    if dir.is_some() {
        v["builder"] = kick_builder().await;
    }
    v["deploy"] = deploy_status(dir.as_deref()).await;
    if !contribute.is_null() {
        v["contribute"] = contribute;
    }
    (parts.status, Json(v)).into_response()
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

/// Fetch the current branch's upstream without touching HEAD, the index, or a
/// worktree file. This is the safe meaning of "Pull from remote" while another
/// worker owns uncommitted bytes in the shared checkout.
async fn fetch_upstream_for_activation(dir: &Path) -> Result<String, String> {
    let upstream = git(
        dir,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
        GIT_TIMEOUT,
    )
    .await
    .map(|o| out_of(&o))
    .filter(|s| !s.is_empty())
    .ok_or_else(|| "cannot resolve this branch's upstream".to_string())?;
    let (remote, branch) = upstream
        .split_once('/')
        .filter(|(remote, branch)| !remote.is_empty() && !branch.is_empty())
        .ok_or_else(|| format!("upstream {upstream:?} is not <remote>/<branch>"))?;
    let refspec = format!("refs/heads/{branch}:refs/remotes/{remote}/{branch}");

    let Some(first) = git(dir, &["fetch", "--", remote, &refspec], GIT_TIMEOUT).await else {
        return Err(format!("git fetch {upstream} timed out after 30s"));
    };
    let first_output = out_of(&first);
    if first.status.success() {
        let target = git(dir, &["rev-parse", &upstream], GIT_TIMEOUT)
            .await
            .map(|o| out_of(&o))
            .unwrap_or_else(|| "unknown revision".to_string());
        return Ok(format!(
            "Fetched {upstream} at {target} for the isolated activation authority.\n\
             The shared checkout was left unchanged."
        ));
    }

    let lower = first_output.to_lowercase();
    let auth_failed = [
        "permission denied",
        "authentication failed",
        "could not read from remote",
        "publickey",
        "terminal prompts disabled",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    if auth_failed {
        let url = git(dir, &["remote", "get-url", remote], GIT_TIMEOUT)
            .await
            .map(|o| out_of(&o))
            .unwrap_or_default();
        if let Some(https) = https_equiv(&url) {
            if let Some(second) = git(
                dir,
                &["-c", "credential.helper=", "fetch", "--", &https, &refspec],
                GIT_TIMEOUT,
            )
            .await
            {
                if second.status.success() {
                    let target = git(dir, &["rev-parse", &upstream], GIT_TIMEOUT)
                        .await
                        .map(|o| out_of(&o))
                        .unwrap_or_else(|| "unknown revision".to_string());
                    return Ok(format!(
                        "Fetched {upstream} anonymously at {target} for the isolated activation authority.\n\
                         The shared checkout was left unchanged."
                    ));
                }
                return Err(format!(
                    "{first_output}\n[https fallback also failed] {}",
                    out_of(&second)
                ));
            }
        }
    }
    Err(first_output)
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
            return Some(
                json!({"ok": false, "blocked": "dirty_tree", "files": shown.clone(),
                "output": format!(
                    "Refusing to pull: {} uncommitted change(s) in {}.\n{}\n\n\
                     This is a shared checkout — those edits may belong to another session \
                     that is not even running. Commit or stash them first.",
                    dirty.len(), dir.display(), shown.join("\n"))}),
            );
        }
    }

    // Do NOT reject an ahead-only branch here. `git pull --ff-only` correctly
    // treats it as "Already up to date"; the old preflight turned the most
    // common shared-checkout state into a permanently broken Pull button.
    // If the remote is also ahead, the pull below fetches it and refuses the
    // divergence without rebasing, merging, or rewriting either side.
    None
}

async fn pull_checkout() -> Response {
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
        // Say only what is known. This used to promise "updates deploy
        // automatically", which nothing here can check, and on a hand-installed
        // host (sandbox.amux.io, 2026-10-11) nothing updated it at all.
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "an unknown path".into());
        tracing::warn!(exe = %exe, measured = true, n_considered = 1, verdict = "pull_no_checkout",
            "Pull from remote pressed on a server with no git checkout; nothing to update");
        return Json(json!({"ok": false, "channel": "standalone", "exe": exe,
            "output": format!(
                "This server runs a standalone binary ({exe}) with no git checkout, so this \
                 button has nothing to pull. It updates only when whatever installed that \
                 binary replaces it.\n\
                 To update from here: set AMUX_REPO_DIR to an amux checkout and run the amux \
                 builder (install.sh sets both up); a pull then deploys.")}))
        .into_response();
    };

    if let Some(refusal) = preflight(&dir).await {
        // A local checkout refusal protects peer bytes, but fetch updates only
        // the remote-tracking ref consumed by the clean activation authority.
        // Treat that as the successful, non-destructive form of this button.
        return match fetch_upstream_for_activation(&dir).await {
            Ok(output) => Json(json!({
                "ok": true,
                "deferred": true,
                "mode": "fetch_only",
                "local_blocked": refusal["blocked"],
                "output": output,
            }))
            .into_response(),
            Err(fetch_error) => Json(json!({
                "ok": false,
                "blocked": refusal["blocked"],
                "output": format!(
                    "{}\n\nThe safe fetch-only fallback also failed: {fetch_error}",
                    refusal["output"].as_str().unwrap_or("Local pull was refused.")
                ),
            }))
            .into_response(),
        };
    }

    // ---- the pull itself ---------------------------------------------------
    let Some(o) = git(&dir, &["pull", "--ff-only"], GIT_TIMEOUT).await else {
        return Json(json!({"ok": false, "output": "git pull timed out after 30s"}))
            .into_response();
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
    let auth_failed = [
        "permission denied",
        "authentication failed",
        "could not read from remote",
        "publickey",
        "terminal prompts disabled",
    ]
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
                &[
                    "-c",
                    "credential.helper=",
                    "pull",
                    "--ff-only",
                    &https,
                    &branch,
                ],
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
mod deploy_summary_tests {
    use super::deploy_summary;
    use serde_json::json;

    const LIVE: &str = "08c8c6973e71ba5b391ec2a553c550e1fe350248";
    const TIP: &str = "3e0f0823a30bf324737e596bda94673ef82efea7";

    #[test]
    fn every_state_names_what_runs_and_why() {
        let (s, m) = deploy_summary(LIVE, None, false, 0, None);
        assert_eq!(s, "unmeasured", "{m}");
        let (s, m) = deploy_summary(LIVE, Some(LIVE), true, 0, None);
        assert_eq!(s, "current", "{m}");
        // The 2026-10-24 morning: a fork build live, a green main commit to replace it.
        let g = json!({"target": LIVE, "tip_state": "pending"});
        let (s, m) = deploy_summary("03adc2a58cf4a7f3430f0166268d2487166b1e70", Some(TIP), false, 0, Some(&g));
        assert_eq!(s, "off_main", "{m}");
        assert!(m.contains("not on main") && m.contains("08c8c6973"), "{m}");
        // Live is the newest green; main is ahead and red.
        let g = json!({"target": LIVE, "tip_state": "red"});
        let (s, m) = deploy_summary(LIVE, Some(TIP), true, 62, Some(&g));
        assert_eq!(s, "held", "{m}");
        assert!(m.contains("62 commit(s) ahead") && m.contains("failing CI at 3e0f0823a"), "{m}");
        // A newer green exists than what runs: deploying.
        let g = json!({"target": TIP, "tip_state": "green"});
        let (s, m) = deploy_summary(LIVE, Some(TIP), true, 62, Some(&g));
        assert_eq!(s, "deploying", "{m}");
        // Off main with nothing green: say there is nothing to replace it with.
        let (s, m) = deploy_summary("03adc2a58", Some(TIP), false, 0, Some(&json!({"target": "", "tip_state": "red"})));
        assert_eq!(s, "off_main");
        assert!(m.contains("nothing to replace it with"), "{m}");
    }
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
                .args(args)
                .current_dir(cwd)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap()
        };
        // A bare "remote" so the branch can have a real upstream.
        let remote = d.path().join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        run(
            &["init", "--bare", "-b", "main", remote.to_str().unwrap()],
            d.path(),
        );
        let work = d.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        run(&["init", "-b", "main"], &work);
        std::fs::write(work.join("a.txt"), "one\n").unwrap();
        run(&["add", "."], &work);
        run(&["commit", "-m", "first"], &work);
        run(
            &["remote", "add", "origin", remote.to_str().unwrap()],
            &work,
        );
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
        assert!(
            preflight(&work).await.is_none(),
            "a clean, up-to-date repo must pull"
        );

        std::fs::write(work.join("a.txt"), "edited by a peer\n").unwrap();
        let r = preflight(&work)
            .await
            .expect("a dirty tree must be refused");
        assert_eq!(r["blocked"], "dirty_tree");
        // The FILE, not just a count — "1 file" tells nobody what to look at.
        assert_eq!(r["files"][0].as_str().unwrap().trim(), "M a.txt");
    }

    #[tokio::test]
    async fn a_dirty_shared_checkout_can_fetch_for_activation_without_moving_peer_bytes() {
        let d = repo();
        let work = d.path().join("work");
        let remote = d.path().join("remote.git");
        let peer = d.path().join("peer");
        let run = |args: &[&str], cwd: &Path| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap()
        };

        run(
            &["clone", remote.to_str().unwrap(), peer.to_str().unwrap()],
            d.path(),
        );
        std::fs::write(peer.join("remote.txt"), "new remote bytes\n").unwrap();
        run(&["add", "."], &peer);
        run(&["commit", "-m", "remote advance"], &peer);
        run(&["push", "origin", "main"], &peer);
        let remote_head = String::from_utf8_lossy(&run(&["rev-parse", "HEAD"], &peer).stdout)
            .trim()
            .to_string();

        std::fs::write(work.join("a.txt"), "peer's uncommitted edit\n").unwrap();
        let before_head = String::from_utf8_lossy(&run(&["rev-parse", "HEAD"], &work).stdout)
            .trim()
            .to_string();
        let output = fetch_upstream_for_activation(&work).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(work.join("a.txt")).unwrap(),
            "peer's uncommitted edit\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run(&["rev-parse", "HEAD"], &work).stdout).trim(),
            before_head,
            "fetch-only must not move the shared checkout"
        );
        assert_eq!(
            String::from_utf8_lossy(&run(&["rev-parse", "origin/main"], &work).stdout).trim(),
            remote_head,
            "the activation authority must see the fetched revision"
        );
        assert!(
            output.contains("shared checkout was left unchanged"),
            "{output}"
        );
    }

    #[tokio::test]
    async fn an_ahead_only_branch_does_not_break_the_pull_button() {
        let d = repo();
        let work = d.path().join("work");
        std::fs::write(work.join("b.txt"), "two\n").unwrap();
        for args in [vec!["add", "."], vec!["commit", "-m", "local only"]] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&work)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
        }
        assert!(
            preflight(&work).await.is_none(),
            "local commits are compatible with pull --ff-only when the remote is unchanged"
        );

        let pulled = std::process::Command::new("git")
            .args(["pull", "--ff-only"])
            .current_dir(&work)
            .output()
            .unwrap();
        assert!(
            pulled.status.success(),
            "the exact command behind the button must remain a safe no-op: {}{}",
            String::from_utf8_lossy(&pulled.stdout),
            String::from_utf8_lossy(&pulled.stderr)
        );
    }

    #[tokio::test]
    async fn a_half_finished_merge_is_refused_before_anything_else() {
        let d = repo();
        let work = d.path().join("work");
        std::fs::write(work.join(".git/MERGE_HEAD"), "deadbeef\n").unwrap();
        let r = preflight(&work)
            .await
            .expect("an in-progress merge must be refused");
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

#[cfg(test)]
mod contribute_tests {
    use super::contribute_result;

    #[test]
    fn reads_the_last_contribute_line_and_nothing_else() {
        let out = "  local commits not on origin/main: 1\n  Pull request: https://github.com/mixpeek/amux/pull/7\n\
            AMUX_CONTRIBUTE {\"verdict\": \"pr_opened\", \"exit\": 0, \"pr_url\": \"https://github.com/mixpeek/amux/pull/7\"}\n";
        let v = contribute_result(out).unwrap();
        assert_eq!(v["verdict"], "pr_opened");
        assert_eq!(v["pr_url"], "https://github.com/mixpeek/amux/pull/7");
        assert!(contribute_result("no result line\n").is_none());
        assert!(contribute_result("AMUX_CONTRIBUTE not json\n").is_none());
    }
}
