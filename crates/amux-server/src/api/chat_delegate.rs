//! Chat delegate: a read-only background job on a worker's own provider, so a
//! worker's Chat tab can answer questions its own narrower tools cannot
//! (AMUX-5432, design in docs/design/chat-delegate.md).
//!
//! Ethan, 2026-10-01: the `social-activities` Chat could not answer "what is
//! the dress code for the brownstone today?" (WebFetch denied) while its
//! terminal worker could have fetched the page in one step. "It needs to be
//! able to use the workers regular terminal coding agent thing to do work in a
//! read only way ... not interfere with the workers work if it's working or not
//! and it needs to be model agnostic."
//!
//! A delegate is a JOB, not a worker: one non-interactive run of the parent
//! worker's provider CLI, read-only, in its own process group, answering one
//! question, gone when it answers. It never touches the worker's pane, its
//! steering queue, the board under the worker's identity, or its checkout.
//!
//! Writes are taken away three ways so no single layer is trusted:
//! 1. the provider CLI's own read-only mode (`provider_command`),
//! 2. an OS sandbox denying writes under the checkout and its git dir
//!    (`sandbox-exec` on macOS),
//! 3. the API guard (`delegate_read_only`): identity `<worker>@delegate` may
//!    only GET.
//!
//! Jobs run detached like Chat turns (61a92bfc), so a server restart re-attaches
//! (`recover_all`) and the answer is still delivered exactly once.

use super::session_verbs::{emit_event, home, meta_str, sh_quote};
use super::AppState;
use axum::extract::{Path as AxumPath, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

/// The identity suffix every delegate runs under. The API guard keys on it.
pub(crate) const DELEGATE_SUFFIX: &str = "@delegate";

/// Tools a claude delegate may use: reading, the web, and the amux CLI's read
/// verbs. Anything else is denied by `--permission-mode dontAsk`.
const CLAUDE_READ_TOOLS: &[&str] = &[
    "Read",
    "Grep",
    "Glob",
    "WebFetch",
    "WebSearch",
    "TodoWrite",
    "Bash(amux peek:*)",
    "Bash(amux info:*)",
    "Bash(amux ls:*)",
    "Bash(amux board ls:*)",
    "Bash(amux board status:*)",
    "Bash(amux get:*)",
    "Bash(amux crm get:*)",
    "Bash(amux crm list:*)",
    "Bash(amux whoami:*)",
    "Bash(git log:*)",
    "Bash(git show:*)",
    "Bash(git diff:*)",
    "Bash(git status:*)",
    "Bash(git blame:*)",
];
/// Never allowed, even if a worker's own settings would allow them.
const CLAUDE_WRITE_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];

fn delegates_dir() -> PathBuf {
    home().join("chat-state").join("delegates")
}
fn job_dir(id: &str) -> PathBuf {
    delegates_dir().join(id)
}
fn job_file(id: &str) -> PathBuf {
    job_dir(id).join("job.json")
}

fn now() -> f64 {
    crate::config::now_f64()
}

fn env_num(key: &str, worker: &str, default: f64) -> f64 {
    let cfg = super::session_verbs::parse_env(worker);
    cfg.get(key)
        .map(str::to_string)
        .or_else(|| std::env::var(key).ok())
        .and_then(|v| v.trim().trim_matches('"').parse().ok())
        .filter(|v: &f64| *v > 0.0)
        .unwrap_or(default)
}

/// The parent worker of a name a caller passes: `w`, `w@chat` or `w@delegate`.
pub(crate) fn parent_worker(name: &str) -> &str {
    name.trim_end_matches(DELEGATE_SUFFIX).trim_end_matches(super::chat_worker::COMPANION_SUFFIX)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Mode {
    /// A fresh run, seeded with what the worker has already produced.
    Fresh,
    /// A fork of the worker's own conversation (claude only), never written back.
    Fork,
}

/// The provider's non-interactive, read-only argv (after the binary). Pure, so
/// each provider's read-only flags are unit tested. The prompt goes on stdin.
pub(crate) fn provider_command(
    provider: &str,
    model: Option<&str>,
    mode: Mode,
    fork_from: Option<&str>,
    max_usd: f64,
    extra_tools: &[String],
) -> Result<Vec<String>, String> {
    match provider {
        "claude" | "" => {
            let mut tools: Vec<String> = CLAUDE_READ_TOOLS.iter().map(|s| s.to_string()).collect();
            tools.extend(extra_tools.iter().filter(|t| !CLAUDE_WRITE_TOOLS.contains(&t.as_str())).cloned());
            let mut a: Vec<String> = vec![
                "-p".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--verbose".into(),
                // dontAsk: anything not on the allowlist is denied, never asked.
                "--permission-mode".into(),
                "dontAsk".into(),
                "--allowedTools".into(),
                tools.join(","),
                "--disallowedTools".into(),
                CLAUDE_WRITE_TOOLS.join(","),
                // Nothing this job does is saved as a conversation.
                "--no-session-persistence".into(),
                // The harness hooks in ~/.claude/settings.json report a
                // WORKER's status and guard a worker's commits; a read-only
                // job has neither, and their POSTs were refused by the API
                // guard as delegate writes (seen live 2026-10-01).
                "--settings".into(),
                "{\"disableAllHooks\":true}".into(),
            ];
            if mode == Mode::Fork {
                let id = fork_from.filter(|s| !s.is_empty()).ok_or("fork mode needs the worker's conversation id")?;
                a.extend(["--resume".into(), id.to_string(), "--fork-session".into()]);
                // NO --max-budget-usd on a fork: the CLI counts the forked
                // conversation's past spend against it, so a fork of a long
                // conversation was refused at once (live 2026-10-01: a 15 MB
                // social-activities conversation reported $101.62 and ended
                // `budget` with no answer). A fork is bounded by the size cap
                // in start_job and by the timeout instead.
            } else {
                a.extend(["--max-budget-usd".into(), format!("{max_usd:.2}")]);
            }
            if let Some(m) = model.filter(|m| !m.is_empty()) {
                a.extend(["--model".into(), m.to_string()]);
            }
            Ok(a)
        }
        "codex" => {
            if mode == Mode::Fork {
                return Err("codex has no fork of a conversation; use fresh".into());
            }
            let mut a: Vec<String> = vec![
                "exec".into(),
                "--json".into(),
                "--skip-git-repo-check".into(),
                "--sandbox".into(),
                "read-only".into(),
                "-c".into(),
                "approval_policy=\"never\"".into(),
                "-c".into(),
                "tools.web_search=true".into(),
            ];
            if let Some(m) = model.filter(|m| !m.is_empty()) {
                a.extend(["--model".into(), m.to_string()]);
            }
            a.push("-".into());
            Ok(a)
        }
        other => Err(format!(
            "provider_unsupported: {other} has no headless read-only mode amux can enforce, so it cannot run a delegate"
        )),
    }
}

/// The worker's model: `--model` in CC_FLAGS, else CC_MODEL.
fn worker_model(worker: &str) -> Option<String> {
    let cfg = super::session_verbs::parse_env(worker);
    let flags = cfg.get_or("CC_FLAGS", "").to_string();
    let toks: Vec<&str> = flags.split_whitespace().collect();
    toks.iter()
        .enumerate()
        .find_map(|(i, t)| {
            if *t == "--model" {
                toks.get(i + 1).map(|m| m.trim_matches(['"', '\'']).to_string())
            } else {
                t.strip_prefix("--model=").map(str::to_string)
            }
        })
        .or_else(|| Some(cfg.get_or("CC_MODEL", "").trim().to_string()).filter(|m| !m.is_empty()))
}

fn provider_bin(provider: &str) -> String {
    let key = format!("AMUX_DELEGATE_{}_BIN", provider.to_uppercase());
    std::env::var(&key).unwrap_or_else(|_| provider.to_string())
}

/// The claude conversation file for `id`, if one exists under any project.
fn claude_session_file(id: &str) -> Option<PathBuf> {
    if id.is_empty() {
        return None;
    }
    let root = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude").join("projects");
    std::fs::read_dir(root).ok()?.flatten().map(|d| d.path().join(format!("{id}.jsonl"))).find(|p| p.is_file())
}

/// Paths the OS sandbox protects from writes: the checkout and its git dir.
fn protected_paths(cwd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let canon = |p: &str| std::fs::canonicalize(p).ok().map(|p| p.to_string_lossy().into_owned());
    if let Some(c) = canon(cwd) {
        out.push(c);
    }
    let common = std::process::Command::new("git")
        .args(["-C", cwd, "rev-parse", "--path-format=absolute", "--git-common-dir"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    if let Some(c) = common.as_deref().and_then(canon) {
        if !out.iter().any(|p| c.starts_with(p.as_str())) {
            out.push(c);
        }
    }
    out
}

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The macOS sandbox profile denying writes under `paths`, or None where no OS
/// sandbox exists.
pub(crate) fn sandbox_profile(paths: &[String]) -> Option<String> {
    if paths.is_empty() || !Path::new(SANDBOX_EXEC).is_file() || std::env::var("AMUX_DELEGATE_NO_SANDBOX").is_ok_and(|v| v == "1") {
        return None;
    }
    let deny: String = paths
        .iter()
        .map(|p| format!("(subpath \"{}\")", p.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(" ");
    Some(format!("(version 1)(allow default)(deny file-write* {deny})"))
}

/// Exact-match pane target (`=amux-<w>:`) from the L2 helper, so a missing
/// session never falls through to a sibling's pane (tests/tmux_target_audit.rs).
fn pane_target_of(worker: &str) -> String {
    crate::backend::tmux::pane_target(&format!("amux-{worker}"))
}

/// What the worker has already produced, for a fresh (non-fork) delegate:
/// its last reply and the tail of its terminal. Read-only: `capture-pane`
/// prints, it never sends input.
fn seed_context(worker: &str) -> String {
    let mut s = String::new();
    let last = super::session_verbs::last_assistant_message(worker, 4000);
    if !last.trim().is_empty() {
        s.push_str("The worker's most recent reply:\n");
        s.push_str(last.trim());
        s.push_str("\n\n");
    }
    let pt = pane_target_of(worker);
    if let Ok(o) = std::process::Command::new("tmux")
        .args(["capture-pane", "-p", "-J", "-S", "-300", "-t", &pt])
        .output()
    {
        let pane = String::from_utf8_lossy(&o.stdout);
        let pane = pane.trim();
        if o.status.success() && !pane.is_empty() {
            let tail: String = pane.chars().rev().take(12000).collect::<Vec<_>>().into_iter().rev().collect();
            s.push_str("The end of the worker's terminal:\n");
            s.push_str(&tail);
            s.push('\n');
        }
    }
    s
}

fn preamble(worker: &str, mode: Mode) -> String {
    let view = match mode {
        Mode::Fork => "You are a READ-ONLY copy of this conversation, forked so the owner's Chat can ask a question. You are not the live worker: the live worker keeps running separately and will never see what you do here.",
        Mode::Fresh => "You are a READ-ONLY background job answering a question for the owner's Chat about the amux worker below. You are not the live worker and cannot reach it.",
    };
    format!(
        "{view}\n\nRules: answer the question, then stop. You cannot change anything: do not edit or write \
         files, do not commit, do not send messages, do not use `amux send`, and do not change the board. \
         Read whatever helps: the worker's files (including uncommitted edits), its git history, `amux peek {worker}`, \
         `amux board ls`, `amux get <api path>` for messages, calendar or email, and the web. Say plainly what \
         you checked. If the answer exists only in the live worker's head, say so.\n\n"
    )
}

/// Ids this process is following, so recovery never starts a second follower.
static FOLLOWING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

fn load_job(id: &str) -> Option<Value> {
    std::fs::read_to_string(job_file(id)).ok().and_then(|t| serde_json::from_str(&t).ok())
}
fn save_job(id: &str, job: &Value) {
    let p = job_file(id);
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(job).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}
fn patch_job(id: &str, updates: &[(&str, Value)]) -> Option<Value> {
    let mut j = load_job(id)?;
    for (k, v) in updates {
        j[*k] = v.clone();
    }
    save_job(id, &j);
    Some(j)
}

fn refuse(worker: &str, reason: &str, why: String) -> Response {
    tracing::warn!(worker, reason, verdict = "chat_delegate_refused", why = %why, "a chat delegate was refused");
    (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": why, "reason": reason}))).into_response()
}

/// POST /api/chat-delegate {worker, prompt, mode?: auto|fork|fresh, wait_s?}
async fn start_route(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let caller = headers.get("x-amux-session").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let named = body["worker"].as_str().map(str::to_string).unwrap_or_else(|| caller.clone());
    let worker = parent_worker(named.trim()).to_string();
    let prompt = body["prompt"].as_str().unwrap_or("").trim().to_string();
    if worker.is_empty() || !super::session_verbs::env_path(&worker).is_file() {
        return refuse(&worker, "no_worker", format!("no worker named '{worker}'"));
    }
    if prompt.is_empty() {
        return refuse(&worker, "empty_prompt", "the question is empty".into());
    }
    let wait_s = body["wait_s"].as_f64().unwrap_or(0.0).clamp(0.0, 600.0);
    match start_job(&state, &worker, &prompt, body["mode"].as_str().unwrap_or("auto"), wait_s, &caller).await {
        Ok(job) => (StatusCode::OK, Json(json!({"ok": true, "job": job}))).into_response(),
        Err((reason, why)) => refuse(&worker, &reason, why),
    }
}

/// Start one delegate job. Returns the job record.
pub(crate) async fn start_job(
    state: &AppState,
    worker: &str,
    prompt: &str,
    mode_req: &str,
    wait_s: f64,
    caller: &str,
) -> Result<Value, (String, String)> {
    let cfg = super::session_verbs::parse_env(worker);
    let provider = cfg.get_or("CC_PROVIDER", "claude").trim().to_lowercase();
    let cwd = {
        let d = cfg.get_or("CC_DIR", "").trim().to_string();
        if d.is_empty() || !Path::new(&d).is_dir() { super::chat_worker::default_chat_dir(worker) } else { d }
    };
    let _ = std::fs::create_dir_all(&cwd);
    let meta = super::session_verbs::load_meta(worker);
    let conv = meta_str(&meta, "cc_conversation_id");
    // A fork re-reads the worker's whole conversation, so its cost grows with
    // that conversation. Fork only a conversation under the size cap; a larger
    // one is answered fresh, seeded with the worker's recent context, and the
    // record says why.
    let fork_max_mb = env_num("AMUX_CHAT_DELEGATE_FORK_MAX_MB", worker, 2.0);
    let session_mb = if provider == "claude" {
        claude_session_file(&conv).and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len() as f64 / 1_048_576.0)
    } else {
        None
    };
    let too_large = session_mb.is_some_and(|mb| mb > fork_max_mb);
    let can_fork = session_mb.is_some() && !too_large;
    let fork_skipped = too_large.then(|| {
        format!("the worker's conversation is {:.1} MB, over the {fork_max_mb} MB fork cap", session_mb.unwrap_or(0.0))
    });
    let mode = match mode_req {
        "fork" if can_fork => Mode::Fork,
        "fork" => {
            return Err((
                "no_fork".into(),
                fork_skipped.clone().map(|why| format!("{why}; ask again with mode fresh")).unwrap_or_else(|| {
                    format!("{worker} has no {provider} conversation amux can fork; ask again with mode fresh")
                }),
            ))
        }
        "fresh" => Mode::Fresh,
        _ => if can_fork { Mode::Fork } else { Mode::Fresh },
    };
    let max_usd = env_num("AMUX_CHAT_DELEGATE_MAX_USD", worker, 1.0);
    let timeout_s = env_num("AMUX_CHAT_DELEGATE_TIMEOUT_S", worker, 300.0);
    let extra: Vec<String> = cfg
        .get("AMUX_CHAT_DELEGATE_TOOLS")
        .map(str::to_string)
        .or_else(|| std::env::var("AMUX_CHAT_DELEGATE_TOOLS").ok())
        .unwrap_or_default()
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    let args = provider_command(&provider, worker_model(worker).as_deref(), mode, Some(&conv), max_usd, &extra)
        .map_err(|e| ("provider_unsupported".to_string(), e))?;
    let profile = sandbox_profile(&protected_paths(&cwd));
    if profile.is_none() && !matches!(provider.as_str(), "claude" | "codex") {
        return Err(("no_read_only_enforcement".into(), format!("{provider} cannot be held read-only on this host")));
    }

    let id = format!("dg-{}", ulid::Ulid::new());
    let dir = job_dir(&id);
    std::fs::create_dir_all(&dir).map_err(|e| ("io".to_string(), e.to_string()))?;
    let mut text = preamble(worker, mode);
    if mode == Mode::Fresh {
        let seed = seed_context(worker);
        if !seed.is_empty() {
            text.push_str("Context from the worker (read-only, may be cut):\n\n");
            text.push_str(&seed);
            text.push('\n');
        }
    }
    text.push_str("Question from the owner:\n");
    text.push_str(prompt);
    std::fs::write(dir.join("in"), &text).map_err(|e| ("io".to_string(), e.to_string()))?;

    let me = format!("{worker}{DELEGATE_SUFFIX}");
    // Vault secrets reach the job the way they reach a worker launch: never
    // argv, never a file, and moved to their real names AFTER the prelude
    // sources the scope env files, so the vault wins as it does at launch.
    let vault = super::vault_secrets::deliver(&home(), worker, super::vault_secrets::DeliveryPath::Delegate)
        .unwrap_or_else(|error| {
            tracing::warn!(session = worker, %error, measured = false, n_considered = 0,
                verdict = "vault_delivery_failed", "vault: a delegate runs without its worker's secrets (AMUX-5375)");
            super::vault_secrets::Delivery { values: Vec::new(), manifest: Default::default() }
        });
    let (vault_env, vault_line) = super::vault_secrets::staged_env(&vault.values);
    let prelude = format!(
        "{}{vault_line}export AMUX_SESSION={q}; export AMUX_WORKER={q}; export GIT_OPTIONAL_LOCKS=0; ",
        super::session_verbs::headless_turn_prelude(worker, &provider, &cwd),
        q = sh_quote(&me)
    );
    let f = |n: &str| sh_quote(&dir.join(n).to_string_lossy());
    let run = if profile.is_some() {
        format!("{SANDBOX_EXEC} -p \"$AMUX_DELEGATE_SANDBOX\" {} \"$@\"", sh_quote(&provider_bin(&provider)))
    } else {
        format!("{} \"$@\"", sh_quote(&provider_bin(&provider)))
    };
    let inner = format!(
        "{prelude}{run}; ec=$?; printf '%s' \"$ec\" > {tmp} && mv {tmp} {exit}",
        tmp = f("exit.tmp"),
        exit = f("exit")
    );
    let launcher = format!("bash -c {} amux-delegate \"$@\" < {} > {} 2> {} & echo $!", sh_quote(&inner), f("in"), f("out"), f("err"));
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(&launcher)
        .arg("amux-delegate-launch")
        .args(&args)
        .current_dir(&cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        // The LAUNCHER is owned and short-lived; the job it starts is not.
        .kill_on_drop(true);
    if let Some(p) = &profile {
        cmd.env("AMUX_DELEGATE_SANDBOX", p);
    }
    for (k, v) in vault_env {
        cmd.env(k, v);
    }
    let child = cmd.spawn().map_err(|e| ("spawn".to_string(), format!("could not start {provider}: {e}")))?;
    let pgid = child.id().unwrap_or(0);
    let done = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
        .await
        .map_err(|_| ("spawn".to_string(), "the launcher did not return".to_string()))?
        .map_err(|e| ("spawn".to_string(), e.to_string()))?;
    if !done.status.success() || pgid == 0 {
        return Err(("spawn".into(), format!("could not start {provider}: {}", String::from_utf8_lossy(&done.stderr).trim())));
    }
    let started = now();
    let job = json!({
        "id": id, "worker": worker, "chat": super::chat_worker::companion_key(worker),
        "caller": caller, "provider": provider, "mode": if mode == Mode::Fork { "fork" } else { "fresh" },
        "prompt": prompt, "cwd": cwd, "read_only": true,
        "enforcement": {
            "cli": true, "os_sandbox": profile.is_some(), "api_guard": true,
        },
        "timeout_s": timeout_s, "max_usd": max_usd,
        "started": started, "wait_until": started + wait_s,
        "pgid": pgid, "status": "running",
        "fork_skipped": fork_skipped,
    });
    save_job(&id, &job);
    tracing::info!(worker, job = %id, provider = %provider, mode = job["mode"].as_str().unwrap_or(""),
        os_sandbox = profile.is_some(), verdict = "chat_delegate_started", "a read-only chat delegate started");
    spawn_follower(state.clone(), id.clone());
    Ok(job)
}

fn spawn_follower(state: AppState, id: String) {
    if !FOLLOWING.lock().unwrap().insert(id.clone()) {
        return;
    }
    tokio::spawn(async move {
        follow(&state, &id).await;
        FOLLOWING.lock().unwrap().remove(&id);
    });
}

/// Follow a job to its end, record the outcome, then deliver it.
async fn follow(state: &AppState, id: &str) {
    let Some(job) = load_job(id) else { return };
    let pgid = job["pgid"].as_u64().unwrap_or(0) as u32;
    let started = job["started"].as_f64().unwrap_or_else(now);
    let timeout_s = job["timeout_s"].as_f64().unwrap_or(300.0);
    let dir = job_dir(id);
    let exit_path = dir.join("exit");
    let mut outcome: Option<&str> = None;
    loop {
        if exit_path.is_file() {
            break;
        }
        if !super::chat_worker::group_alive(pgid) {
            // The exit file is written by the job's own shell; give a racing
            // write a moment before calling it lost.
            tokio::time::sleep(Duration::from_millis(300)).await;
            if !exit_path.is_file() {
                outcome = Some("lost");
            }
            break;
        }
        if now() - started > timeout_s {
            super::chat_worker::signal_turn(pgid);
            tokio::time::sleep(Duration::from_secs(3)).await;
            if super::chat_worker::group_alive(pgid) {
                // SAFETY: the group was created by this server for one job.
                unsafe { libc::kill(-(pgid as i32), libc::SIGKILL) };
            }
            outcome = Some("timeout");
            tracing::warn!(job = id, timeout_s, verdict = "chat_delegate_timeout", "a chat delegate ran past its timeout and was stopped");
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let out = std::fs::read_to_string(dir.join("out")).unwrap_or_default();
    let err = std::fs::read_to_string(dir.join("err")).unwrap_or_default();
    let code: Option<i32> = std::fs::read_to_string(&exit_path).ok().and_then(|t| t.trim().parse().ok());
    let provider = job["provider"].as_str().unwrap_or("claude").to_string();
    let parsed = parse_output(&provider, &out);
    let outcome = outcome.unwrap_or(if parsed.budget_exhausted {
        "budget"
    } else if parsed.text.trim().is_empty() || (code.unwrap_or(1) != 0 && !parsed.ok) {
        "failed"
    } else {
        "ok"
    });
    let text = if parsed.text.trim().is_empty() {
        let tail: String = err.chars().rev().take(800).collect::<Vec<_>>().into_iter().rev().collect();
        format!("(no answer; {outcome}{})", if tail.trim().is_empty() { String::new() } else { format!(": {}", tail.trim()) })
    } else {
        parsed.text.clone()
    };
    let duration_s = now() - started;
    // A fork's reported total includes the forked conversation's past spend,
    // so it is not this job's cost: say so rather than print the wrong number.
    let is_fork = job["mode"] == "fork";
    let cost = if is_fork { None } else { parsed.cost_usd };
    let cost_note = if is_fork { json!("not separable: the CLI reports the forked conversation's total") } else { Value::Null };
    let job = patch_job(
        id,
        &[
            ("status", json!("done")),
            ("outcome", json!(outcome)),
            ("result", json!(text)),
            ("cost_usd", cost.map(|c| json!(c)).unwrap_or(Value::Null)),
            ("cost_note", cost_note),
            ("exit_code", code.map(|c| json!(c)).unwrap_or(Value::Null)),
            ("duration_s", json!((duration_s * 10.0).round() / 10.0)),
            ("finished", json!(now())),
        ],
    )
    .unwrap_or(job);
    tracing::info!(job = id, worker = job["worker"].as_str().unwrap_or(""), outcome, provider = %provider,
        cost_usd = cost.unwrap_or(0.0), cost_measured = cost.is_some(), duration_s, verdict = if outcome == "ok" { "chat_delegate_finished" } else { "chat_delegate_killed" },
        "a chat delegate ended");
    // Inputs and outputs can hold the worker's data: keep only the record.
    for f in ["in", "out", "err", "exit", "exit.tmp"] {
        let _ = std::fs::remove_file(dir.join(f));
    }
    deliver(state, id).await;
}

/// The one-line record a Chat shows for a job.
pub(crate) fn record_line(job: &Value) -> String {
    let mode = if job["mode"] == "fork" {
        "fork of the worker's conversation, not the live agent".to_string()
    } else if let Some(why) = job["fork_skipped"].as_str() {
        format!("fresh read-only job (no fork: {why})")
    } else {
        "fresh read-only job".to_string()
    };
    let cost = job["cost_usd"].as_f64().map(|c| format!(" · ${c:.3}")).unwrap_or_default();
    format!(
        "[background job {} · {} · {} · {} · {:.0}s{cost}]",
        job["id"].as_str().unwrap_or(""),
        job["provider"].as_str().unwrap_or(""),
        mode,
        job["outcome"].as_str().unwrap_or(""),
        job["duration_s"].as_f64().unwrap_or(0.0),
    )
}

/// Deliver a finished job to its Chat, unless a waiting caller already read it.
async fn deliver(state: &AppState, id: &str) {
    let Some(job) = load_job(id) else { return };
    if job["delivered"].as_bool().unwrap_or(false) {
        return;
    }
    // A caller waiting on this job gets it from GET ?consume=1; only deliver
    // what nobody read by the end of their wait (plus a short grace).
    let wait_until = job["wait_until"].as_f64().unwrap_or(0.0);
    let left = wait_until + 3.0 - now();
    if left > 0.0 {
        tokio::time::sleep(Duration::from_secs_f64(left)).await;
    }
    let Some(job) = load_job(id) else { return };
    if job["consumed"].as_bool().unwrap_or(false) || job["delivered"].as_bool().unwrap_or(false) {
        return;
    }
    patch_job(id, &[("delivered", json!(true))]);
    let chat = job["chat"].as_str().unwrap_or("").to_string();
    let msg = format!(
        "{}\nThe background job you started has finished. Relay its answer to the owner.\n\nQuestion: {}\n\nAnswer:\n{}",
        record_line(&job),
        job["prompt"].as_str().unwrap_or(""),
        job["result"].as_str().unwrap_or("")
    );
    let delivered = super::chat_worker::deliver_delegate_result(state, &chat, &msg).await;
    if !delivered {
        // The Chat is not running: keep the answer visible in its transcript.
        emit_event(
            state,
            &chat,
            super::chat_worker::CHAT_EVENT,
            Some(json!({"role": "user", "origin": "delegate", "text": msg, "turn_id": format!("delegate-{id}")})),
            Some(format!("chat:delegate-{id}")),
            "amux-delegate",
        )
        .await;
    }
    tracing::info!(job = id, chat = %chat, queued_turn = delivered, verdict = "chat_delegate_delivered", "a chat delegate's answer was delivered to its Chat");
}

#[derive(Default)]
pub(crate) struct Parsed {
    pub text: String,
    pub cost_usd: Option<f64>,
    pub ok: bool,
    pub budget_exhausted: bool,
}

/// The answer, cost and status from a provider's JSON output.
pub(crate) fn parse_output(provider: &str, out: &str) -> Parsed {
    let mut p = Parsed::default();
    let mut assistant = String::new();
    for line in out.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
        if provider == "codex" {
            let item = &v["item"];
            if v["type"] == "item.completed" && item["type"] == "agent_message" {
                p.text = item["text"].as_str().unwrap_or("").to_string();
                p.ok = true;
            } else if v["msg"]["type"] == "agent_message" {
                p.text = v["msg"]["message"].as_str().unwrap_or("").to_string();
                p.ok = true;
            }
            continue;
        }
        match v["type"].as_str() {
            Some("result") => {
                if let Some(t) = v["result"].as_str() {
                    p.text = t.to_string();
                }
                p.cost_usd = v["total_cost_usd"].as_f64();
                p.ok = !v["is_error"].as_bool().unwrap_or(false);
                p.budget_exhausted = v["subtype"].as_str().is_some_and(|s| s.contains("budget"));
            }
            Some("assistant") => {
                if let Some(blocks) = v["message"]["content"].as_array() {
                    for b in blocks {
                        if b["type"] == "text" {
                            assistant = b["text"].as_str().unwrap_or("").to_string();
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if p.text.trim().is_empty() {
        p.text = assistant;
    }
    p
}

/// GET /api/chat-delegate/{id}[?consume=1]
async fn get_route(AxumPath(id): AxumPath<String>, RawQuery(q): RawQuery) -> Response {
    if !id.starts_with("dg-") || id.contains('/') || id.contains("..") {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad job id"}))).into_response();
    }
    let Some(job) = load_job(&id) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "no such job"}))).into_response();
    };
    let consume = q.as_deref().unwrap_or("").split('&').any(|kv| kv == "consume=1");
    let job = if consume && job["status"] == "done" && !job["delivered"].as_bool().unwrap_or(false) {
        patch_job(&id, &[("consumed", json!(true))]).unwrap_or(job)
    } else {
        job
    };
    let mut out = job.clone();
    if job["status"] == "done" {
        out["record"] = json!(record_line(&job));
    }
    Json(out).into_response()
}

/// GET /api/chat-delegate?worker=<w>: the most recent jobs, newest first.
async fn list_route(RawQuery(q): RawQuery) -> Response {
    let worker = q
        .as_deref()
        .unwrap_or("")
        .split('&')
        .find_map(|kv| kv.strip_prefix("worker="))
        .map(|w| parent_worker(&w.replace("%40", "@")).to_string());
    let mut jobs: Vec<Value> = std::fs::read_dir(delegates_dir())
        .map(|d| d.flatten().filter_map(|e| load_job(&e.file_name().to_string_lossy())).collect())
        .unwrap_or_default();
    if let Some(w) = &worker {
        jobs.retain(|j| j["worker"].as_str() == Some(w.as_str()));
    }
    jobs.sort_by(|a, b| b["started"].as_f64().unwrap_or(0.0).total_cmp(&a["started"].as_f64().unwrap_or(0.0)));
    jobs.truncate(20);
    Json(json!({"jobs": jobs, "measured": true, "n_considered": jobs.len()})).into_response()
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/chat-delegate", get(list_route).post(start_route))
        .route("/api/chat-delegate/{id}", get(get_route))
}

/// Boot: re-attach to jobs a previous server process started.
pub async fn recover_all(state: &AppState) -> usize {
    let Ok(d) = std::fs::read_dir(delegates_dir()) else { return 0 };
    let mut n = 0;
    for e in d.flatten() {
        let id = e.file_name().to_string_lossy().into_owned();
        let Some(job) = load_job(&id) else { continue };
        if job["status"] == "running" {
            n += 1;
            tracing::info!(job = %id, alive = super::chat_worker::group_alive(job["pgid"].as_u64().unwrap_or(0) as u32),
                verdict = "chat_delegate_reattached", "a chat delegate outlived a server restart; following it again");
            spawn_follower(state.clone(), id);
        } else if job["status"] == "done" && !job["delivered"].as_bool().unwrap_or(false) && !job["consumed"].as_bool().unwrap_or(false) {
            let st = state.clone();
            tokio::spawn(async move { deliver(&st, &id).await });
        }
    }
    n
}

/// The API guard: a delegate's identity may only read. Layered on the whole
/// router so it covers routes nobody has written yet.
pub async fn delegate_read_only(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let who = ["x-amux-session", "x-amux-worker"]
        .iter()
        .filter_map(|h| req.headers().get(*h).and_then(|v| v.to_str().ok()))
        .find(|v| v.trim().ends_with(DELEGATE_SUFFIX))
        .map(str::to_string);
    if let Some(who) = who {
        if !matches!(*req.method(), axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS) {
            tracing::warn!(who = %who, method = %req.method(), path = %req.uri().path(),
                verdict = "chat_delegate_write_refused", "a read-only chat delegate tried to change something");
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "delegate_read_only", "detail": "a chat delegate is read-only; it may not change anything"})),
            )
                .into_response();
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_delegate_is_read_only_and_never_skips_permissions() {
        let a = provider_command("claude", Some("sonnet"), Mode::Fresh, None, 0.5, &["Edit".into(), "mcp__x__search".into()]).unwrap();
        let s = a.join(" ");
        assert!(s.contains("--permission-mode dontAsk"), "{s}");
        let allow = &a[a.iter().position(|x| x == "--allowedTools").unwrap() + 1];
        let deny = &a[a.iter().position(|x| x == "--disallowedTools").unwrap() + 1];
        for w in CLAUDE_WRITE_TOOLS {
            assert!(deny.split(',').any(|d| d == *w), "{w} not denied: {deny}");
            assert!(!allow.split(',').any(|t| t == *w), "{w} allowed: {allow}");
        }
        assert!(allow.contains("WebFetch") && allow.contains("mcp__x__search"), "{allow}");
        assert!(!allow.contains("Bash(amux send") && !allow.split(',').any(|t| t == "Bash" || t == "Bash(amux:*)"), "{allow}");
        assert!(!s.contains("dangerously"), "{s}");
        assert!(s.contains("--max-budget-usd 0.50") && s.contains("--no-session-persistence"), "{s}");
        assert!(s.contains("--settings {\"disableAllHooks\":true}"), "worker hooks do not run in a delegate: {s}");
        assert!(!s.contains("--resume"), "{s}");
    }

    #[test]
    fn claude_fork_resumes_a_copy_and_never_saves_it() {
        let a = provider_command("claude", None, Mode::Fork, Some("conv-1"), 1.0, &[]).unwrap();
        let s = a.join(" ");
        assert!(s.contains("--resume conv-1 --fork-session") && s.contains("--no-session-persistence"), "{s}");
        // The CLI counts a forked conversation's past spend against the cap.
        assert!(!s.contains("--max-budget-usd"), "a fork must not carry the budget flag: {s}");
        assert!(provider_command("claude", None, Mode::Fork, Some(""), 1.0, &[]).is_err());
    }

    #[test]
    fn codex_delegate_runs_in_the_read_only_sandbox_and_cannot_fork() {
        let a = provider_command("codex", Some("gpt-5"), Mode::Fresh, None, 1.0, &[]).unwrap();
        let s = a.join(" ");
        assert!(s.contains("--sandbox read-only") && s.contains("approval_policy=\"never\""), "{s}");
        assert!(!s.contains("dangerously") && !s.contains("--yolo") && !s.contains("resume"), "{s}");
        assert_eq!(a.last().map(String::as_str), Some("-"));
        assert!(provider_command("codex", None, Mode::Fork, Some("t"), 1.0, &[]).is_err());
    }

    #[test]
    fn a_provider_without_an_enforceable_read_only_mode_is_refused() {
        let e = provider_command("gemini", None, Mode::Fresh, None, 1.0, &[]).unwrap_err();
        assert!(e.starts_with("provider_unsupported"), "{e}");
    }

    #[test]
    fn parent_worker_strips_chat_and_delegate_suffixes() {
        assert_eq!(parent_worker("social-activities@chat"), "social-activities");
        assert_eq!(parent_worker("w@delegate"), "w");
        assert_eq!(parent_worker("w"), "w");
    }

    #[test]
    fn output_parsing_reads_the_answer_and_cost() {
        let claude = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"draft\"}]}}\n{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"Cocktail attire.\",\"total_cost_usd\":0.12}\n";
        let p = parse_output("claude", claude);
        assert_eq!(p.text, "Cocktail attire.");
        assert_eq!(p.cost_usd, Some(0.12));
        assert!(p.ok && !p.budget_exhausted);
        let b = parse_output("claude", "{\"type\":\"result\",\"subtype\":\"error_max_budget_usd\",\"is_error\":true}\n");
        assert!(b.budget_exhausted);
        let codex = "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"Black tie.\"}}\n";
        assert_eq!(parse_output("codex", codex).text, "Black tie.");
    }
}
