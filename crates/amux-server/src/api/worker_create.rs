//! The create/attach-a-worker surface — everything the New Worker and Connect
//! modals ask for BESIDE `POST /api/sessions` (AMUX-2871).
//!
//! Ported as one batch because they are one screen, and because each of them
//! failed in a way that read as "the feature is off" rather than "the route is
//! missing":
//!
//! | route | what its 404 looked like in the UI |
//! |---|---|
//! | `GET /api/templates`      | template accordion silently hidden (`_allTemplates=[]`) |
//! | `GET /api/git-check`      | the Worktree checkbox never appears — the feature looks absent |
//! | `GET /api/git-branches`   | existing-branch chips never render |
//! | `POST /api/suggest-branch`| the ✨ button spins and returns nothing |
//! | `GET /api/tmux-sessions`  | "Failed to load workers." |
//! | `GET /api/iterm2/sessions`| "Failed to reach iTerm2. Make sure iTerm2 is running." |
//!
//! The last one is the reason this batch is worth a comment: iTerm2 *was*
//! running. Every one of these client `catch` arms attributes a missing route
//! to the user's machine, so the dashboard was actively pointing whoever hit it
//! at the wrong cause.
//!
//! Python contract (amux-server.py at 792ce1f^):
//!   templates       py:68425   git-branches  py:71834   suggest-branch py:71878
//!   git-check       py:71909   tmux-sessions py:71765 (+ list_tmux_sessions py:26068)
//!   iterm2/sessions py:72028   (+ _iterm2_list_panes py:4547)

use super::AppState;
use crate::api::fs::{parse_qs, qs_get};
use crate::api::session_verbs::{home, templates_dir};
use axum::extract::RawQuery;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::time::Duration;

const OP_TIMEOUT: Duration = Duration::from_secs(5);
/// git-check is on the create-modal's keystroke path (debounced dir input), so
/// it gets a tighter budget than the rest — py:71909 used 3s for the same
/// reason.
const GIT_CHECK_TIMEOUT: Duration = Duration::from_secs(3);
/// A helper-model branch suggestion is user-initiated and interactive; past
/// this the deterministic fallback is a better answer than a spinner.
const SUGGEST_TIMEOUT: Duration = Duration::from_secs(20);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/templates", get(templates))
        .route("/api/git-check", get(git_check))
        .route("/api/git-branches", get(git_branches))
        .route("/api/suggest-branch", post(suggest_branch))
        .route("/api/tmux-sessions", get(tmux_sessions))
        .route("/api/iterm2/sessions", get(iterm2_sessions))
}

async fn run(bin: &str, args: &[&str], timeout: Duration) -> Option<std::process::Output> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => Some(out),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// GET /api/templates
// ---------------------------------------------------------------------------

/// Reads the SAME directory `apply-template` writes from
/// (`session_verbs::templates_dir`). Deliberately not a second resolution
/// order: a list that can offer a template the apply path cannot find is the
/// "view disagreeing with the mechanism" failure, and here it would be
/// invisible — the user picks a card and the scaffold silently does nothing.
async fn templates() -> Response {
    let Some(root) = templates_dir() else {
        // Empty array, not an error: the accordion's contract is a list, and
        // `_loadTemplates` treats any non-array as `[]` anyway. The DIAGNOSIS
        // lives in /api/templates/_diag-shaped fields on the apply side, which
        // is the call that can actually fail a user's action.
        return Json(Value::Array(vec![])).into_response();
    };
    let mut dirs: Vec<_> = match std::fs::read_dir(&root) {
        Ok(rd) => rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect(),
        Err(_) => return Json(Value::Array(vec![])).into_response(),
    };
    dirs.sort();
    let mut out = Vec::new();
    for d in dirs {
        let Ok(text) = std::fs::read_to_string(d.join("template.json")) else {
            continue;
        };
        let Ok(mut meta) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(obj) = meta.as_object_mut() {
            if let Ok(claude) = std::fs::read_to_string(d.join("CLAUDE.md")) {
                obj.insert(
                    "claude_md_preview".into(),
                    json!(claude.chars().take(500).collect::<String>()),
                );
            }
        }
        out.push(meta);
    }
    Json(Value::Array(out)).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/git-check?dir=  ·  GET /api/git-branches?dir=
// ---------------------------------------------------------------------------

fn qs_dir(q: &Option<String>) -> String {
    let params = parse_qs(q.as_deref().unwrap_or(""));
    qs_get(&params, "dir").unwrap_or_default().trim().to_string()
}

async fn git_check(RawQuery(q): RawQuery) -> Response {
    let dir = qs_dir(&q);
    if dir.is_empty() {
        return Json(json!({"is_git": false})).into_response();
    }
    let is_git = match run(
        "git",
        &["-C", &dir, "rev-parse", "--is-inside-work-tree"],
        GIT_CHECK_TIMEOUT,
    )
    .await
    {
        Some(out) => {
            out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true"
        }
        None => false,
    };
    Json(json!({"is_git": is_git})).into_response()
}

async fn git_branches(RawQuery(q): RawQuery) -> Response {
    let dir = qs_dir(&q);
    if dir.is_empty() || !std::path::Path::new(&dir).is_dir() {
        return Json(json!({"branches": []})).into_response();
    }
    let Some(out) = run(
        "git",
        &["-C", &dir, "branch", "--format=%(refname:short)"],
        OP_TIMEOUT,
    )
    .await
    else {
        return Json(json!({"branches": []})).into_response();
    };
    Json(json!({"branches": sort_branches(&String::from_utf8_lossy(&out.stdout))}))
        .into_response()
}

/// py:71834 — drop `main`, then `session/*` first, alphabetical within each
/// group. The create modal filters this list by substring, so ordering is what
/// puts a session branch in the first visible chips.
fn sort_branches(raw: &str) -> Vec<String> {
    let mut branches: Vec<String> = raw
        .lines()
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty() && b != "main")
        .collect();
    branches.sort_by(|a, b| {
        let rank = |s: &String| u8::from(!s.starts_with("session/"));
        rank(a).cmp(&rank(b)).then_with(|| a.cmp(b))
    });
    branches
}

// ---------------------------------------------------------------------------
// POST /api/suggest-branch
// ---------------------------------------------------------------------------

/// py:71878 suggested via a PINNED haiku + a hard `ANTHROPIC_API_KEY`
/// requirement. Both are gone:
///
/// - the model is whatever `AMUX_HELPER_CLI`/`AMUX_HELPER_MODEL` names (D3 —
///   one knob, so the helper tier improves without a code change), and
/// - **no model is called at all unless there is something to judge.** With no
///   goal text the answer is `slug` decorated four ways, which is string
///   manipulation; spending a CLI boot on it is ethos rule 2's exact anti-
///   pattern. With a goal, naming a branch after an intent IS judgment, and the
///   user pressed a button asking for it.
async fn suggest_branch(Json(body): Json<Value>) -> Response {
    let name = body["name"].as_str().unwrap_or("").trim().to_string();
    let dir = body["dir"].as_str().unwrap_or("").trim().to_string();
    let prompt = body["prompt"].as_str().unwrap_or("").trim().to_string();
    let fallback = deterministic_branches(&name);

    if prompt.is_empty() {
        return Json(json!({"suggestions": fallback, "via": "deterministic"})).into_response();
    }

    let mut ask = format!("Suggest 4 git branch names for a coding session.\nSession name: {name:?}");
    if !dir.is_empty() {
        ask.push_str(&format!("\nProject directory: {dir:?}"));
    }
    ask.push_str(&format!("\nGoal: {prompt:?}"));
    ask.push_str(
        "\n\nReply with exactly 4 branch names, one per line, no explanations, \
         no bullets, no numbers. Use kebab-case. Vary the schema: one with \
         'feat/', one with 'session/', one descriptive without prefix, one short.",
    );

    let raw_cli = std::env::var("AMUX_HELPER_CLI").unwrap_or_else(|_| "claude".into());
    let cli = super::lookup::resolve_cli(&raw_cli);
    let model = std::env::var("AMUX_HELPER_MODEL").unwrap_or_default();
    let mut cmd = tokio::process::Command::new(&cli);
    cmd.arg("--print").arg(&ask);
    if !model.trim().is_empty() {
        cmd.arg("--model").arg(model.trim());
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let out = tokio::time::timeout(SUGGEST_TIMEOUT, cmd.output()).await;
    let lines = match out {
        Ok(Ok(o)) if o.status.success() => parse_suggestions(&String::from_utf8_lossy(&o.stdout)),
        _ => vec![],
    };
    if lines.is_empty() {
        // Never a spinner and never an error: the deterministic names are a
        // real answer, and the caller is told which one it got rather than
        // being left to guess whether the model ran.
        return Json(json!({"suggestions": fallback, "via": "deterministic"})).into_response();
    }
    Json(json!({"suggestions": lines, "via": cli})).into_response()
}

fn slugify(name: &str) -> String {
    let s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "session".into() } else { s }
}

fn deterministic_branches(name: &str) -> Vec<String> {
    let slug = slugify(name);
    vec![
        format!("session/{slug}"),
        format!("feat/{slug}"),
        format!("wip/{slug}"),
        slug,
    ]
}

/// A helper CLI answers in prose more often than the prompt admits. Keep only
/// lines that could BE a branch name, so a "Here are 4 suggestions:" preamble
/// cannot end up in the chip row.
fn parse_suggestions(raw: &str) -> Vec<String> {
    raw.lines()
        .map(|l| l.trim().trim_start_matches(['-', '*', '•']).trim())
        .filter(|l| {
            !l.is_empty()
                && l.len() <= 80
                && !l.contains(' ')
                && l.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_/.".contains(c))
        })
        .map(|l| l.to_string())
        .take(4)
        .collect()
}

// ---------------------------------------------------------------------------
// GET /api/tmux-sessions — tmux sessions amux does NOT already own
// ---------------------------------------------------------------------------

async fn tmux_sessions() -> Response {
    let registered: BTreeSet<String> = match std::fs::read_dir(home().join("sessions")) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension()? != "env" {
                    return None;
                }
                Some(format!("amux-{}", p.file_stem()?.to_string_lossy()))
            })
            .collect(),
        Err(_) => BTreeSet::new(),
    };
    // ':' separator, never '\t' — the locale-sanitization incident (2026-08-09)
    // is why every `-F` format in this repo agrees on that.
    let Some(out) = run(
        "tmux",
        &["list-sessions", "-F", "#{session_name}:#{pane_current_path}"],
        OP_TIMEOUT,
    )
    .await
    else {
        return Json(Value::Array(vec![])).into_response();
    };
    if !out.status.success() {
        return Json(Value::Array(vec![])).into_response();
    }
    let mut items = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (name, dir) = line.split_once(':').unwrap_or((line, ""));
        if name.is_empty() || registered.contains(name) {
            continue;
        }
        items.push(json!({"tmux_name": name, "dir": dir}));
    }
    Json(Value::Array(items)).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/iterm2/sessions
// ---------------------------------------------------------------------------

const ITERM2_LIST_PANES: &str = r#"
tell application "iTerm2"
    set out to ""
    set wi to 0
    repeat with w in windows
        set ti to 0
        repeat with t in tabs of w
            repeat with s in sessions of t
                set out to out & (id of s) & "|" & (name of s) & "|" & (tty of s) & "|" & wi & "|" & ti & "\n"
            end repeat
            set ti to ti + 1
        end repeat
        set wi to wi + 1
    end repeat
    return out
end tell"#;

async fn iterm2_sessions() -> Response {
    // iTerm2 and osascript are macOS-only. Without this the Linux spawn
    // failure was reported as "osascript did not answer within 5s", which
    // reads as a hung Mac rather than a platform that has no iTerm2.
    if !cfg!(target_os = "macos") {
        return Json(json!({
            "panes": [],
            "error": "iTerm2 panes are macOS-only; this server runs on another platform"
        }))
        .into_response();
    }
    let out = run("osascript", &["-e", ITERM2_LIST_PANES], OP_TIMEOUT).await;
    // `panes: []` with a REASON. The client's own catch arm blames the user's
    // machine ("Make sure iTerm2 is running"), which was wrong for the whole
    // time this route 404'd — so when it legitimately cannot list panes, say
    // which of the two it was rather than leaving the UI to guess again.
    let (panes, err) = match out {
        None => (vec![], Some("osascript did not answer within 5s".to_string())),
        Some(o) if !o.status.success() => (
            vec![],
            Some(
                String::from_utf8_lossy(&o.stderr)
                    .trim()
                    .chars()
                    .take(300)
                    .collect::<String>(),
            ),
        ),
        Some(o) => (parse_iterm2_panes(&String::from_utf8_lossy(&o.stdout)), None),
    };
    let mut body = json!({"panes": panes});
    if let Some(e) = err {
        body["error"] = json!(if e.is_empty() { "iTerm2 is not running".into() } else { e });
    }
    Json(body).into_response()
}

fn parse_iterm2_panes(raw: &str) -> Vec<Value> {
    raw.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.trim().split('|').collect();
            if parts.len() < 5 {
                return None;
            }
            Some(json!({
                "id": parts[0],
                "name": parts[1],
                "tty": parts[2],
                "window": parts[3].trim().parse::<i64>().unwrap_or(0),
                "tab": parts[4].trim().parse::<i64>().unwrap_or(0),
            }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branches_put_session_prefixed_first_and_drop_main() {
        let got = sort_branches("main\nzebra\nsession/beta\napple\nsession/alpha\n");
        assert_eq!(
            got,
            vec!["session/alpha", "session/beta", "apple", "zebra"],
            "session/* must sort ahead so the chip row shows them first"
        );
    }

    #[test]
    fn branches_tolerate_blank_lines_and_whitespace() {
        assert_eq!(sort_branches("\n  feat/x  \n\n"), vec!["feat/x"]);
        assert!(sort_branches("").is_empty());
        // `main` is the ONLY name dropped — `maintenance` must survive, which a
        // starts_with filter would have eaten.
        assert_eq!(sort_branches("maintenance\nmain\n"), vec!["maintenance"]);
    }

    #[test]
    fn deterministic_names_are_valid_branch_names() {
        let got = deterministic_branches("My Worker!! 2");
        assert_eq!(
            got,
            vec!["session/my-worker---2", "feat/my-worker---2", "wip/my-worker---2", "my-worker---2"]
        );
        // An empty/garbage name still yields usable names rather than "session/".
        assert_eq!(deterministic_branches("***")[0], "session/session");
    }

    #[test]
    fn prose_around_the_answer_is_discarded() {
        let raw = "Here are 4 branch names:\n\
                   - feat/offline-queue\n\
                   session/offline-queue\n\
                   offline-upload-queue\n\
                   * oq\n\
                   Let me know if you want more!";
        assert_eq!(
            parse_suggestions(raw),
            vec!["feat/offline-queue", "session/offline-queue", "offline-upload-queue", "oq"],
            "only bare branch-shaped tokens survive"
        );
    }

    #[test]
    fn a_model_that_answers_only_prose_yields_nothing_so_the_caller_falls_back() {
        // The check that must be able to FAIL: if this returned the prose, the
        // chip row would offer un-checkoutable names and look like a model bug.
        assert!(parse_suggestions("I cannot suggest branch names for that.").is_empty());
        assert!(parse_suggestions("").is_empty());
    }

    #[test]
    fn iterm2_panes_parse_and_short_lines_are_skipped() {
        let raw = "w0|zsh|/dev/ttys001|0|0\ngarbage\nx|vim|/dev/ttys002|1|2\n";
        let got = parse_iterm2_panes(raw);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["id"], "w0");
        assert_eq!(got[1]["window"], 1);
        assert_eq!(got[1]["tab"], 2);
    }
}
