//! `.mdai` computed-file engine (AMUX-3240).
//!
//! A `.mdai` file is markdown-with-metadata: YAML frontmatter declares a list of
//! `sources` (connections to other files, folders, or other `.mdai` files, each
//! carrying a configurable edge `prompt`), plus an optional per-file `model`
//! override; the markdown body is the node's synthesis instruction. Opening a
//! `.mdai` file runs its whole upstream chain: each source is resolved (a `.mdai`
//! source runs first, depth-first, so upstream output is available before this
//! node synthesizes), the resolved sources are assembled into context, and the
//! model is called with the body prompt to populate the output. Sources may be
//! other `.mdai` files, so the graph is an arbitrary DAG.
//!
//! ## Why the single extension `.mdai` and not `.md.ai`
//!
//! macOS reads a trailing `.ai` as an Adobe Illustrator file, so `foo.md.ai`
//! would be misclassified as binary artwork by Finder, Quick Look, and editors.
//! `.mdai` stays plain text everywhere.
//!
//! ## Model selection (ethos D3: do not pin a weak model)
//!
//! The model is resolved once, in [`resolve_model`], from three places in order:
//! a per-file `model:` override, then the `AMUX_HELPER_MODEL` config path (the
//! same knob the rest of the server's helper calls read), then a named
//! fastest-Claude default. The value flows to the call site as a resolved
//! `String`, never a literal spelled at `ModelClient::complete`, so the default
//! moves with config and improves as the fast tier improves, rather than being
//! frozen into the engine.
//!
//! ## Model injection seam
//!
//! Every model call goes through the [`ModelClient`] trait. Production uses
//! [`CliModel`] (the configured helper CLI). Tests inject a deterministic fake,
//! so DAG ordering, cycle detection, and cache short-circuiting are all
//! testable without spending a real model call or a network round trip.
//!
//! ## Observability (ethos: a bug must surface in the logs)
//!
//! Cycles, model failures, and IO failures are logged at WARN with the offending
//! path (and the named cycle chain), and every run endpoint hit rides the
//! structured request log like any other `/api` call, so a cycle that starts
//! firing or a model that starts failing shows up in `/api/logs/analyze` without
//! anyone going and looking for it.

use super::AppState;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::db::{Store, WriteOutcome};

/// The single extension. NOT `.md.ai` (see the module doc).
pub const MDAI_EXT: &str = "mdai";

/// The fastest Claude model today (Haiku 4.5). This is the ONE place a model id
/// literal lives; it is the DEFAULT, overridden by `AMUX_HELPER_MODEL` and by a
/// per-file `model:`. The call site never spells a literal, so the default is
/// config, not a pin (ethos D3).
const DEFAULT_FAST_MODEL: &str = "claude-haiku-4-5";

/// The sensible default edge prompt written into a target's frontmatter when a
/// connection is created without an explicit one.
const DEFAULT_EDGE_PROMPT: &str =
    "Summarize what this source contributes and how it should inform the synthesis.";

/// Per-source read cap. A plain-file or folder source is truncated to this many
/// bytes so one large file cannot blow up a node's context (ethos: caps are
/// policy, kept explicit rather than implicit). `.mdai` sources are model output
/// and are not capped here.
const MAX_SOURCE_BYTES: usize = 256 * 1024;

/// Total cap when a source is a folder that expands to many files.
const MAX_FOLDER_BYTES: usize = 256 * 1024;

/// How long a single node's model call may take before the run fails loudly
/// rather than hanging the request.
const MODEL_TIMEOUT_S: u64 = 150;
/// The CLIENT's whole-RUN abort, in seconds (`app.js`, `_mdaiRun`). Kept here so
/// the relation is checkable from one place — the two numbers live in different
/// languages and drifted into being EQUAL, which is how AF-141 happened.
const CLIENT_RUN_ABORT_S: u64 = 600;

/// AF-141, enforced at COMPILE time rather than in a test: a per-node budget
/// that leaves the client no room to receive the server's answer is not a
/// configuration mistake to discover on a run, it is a build that should not
/// exist. Clippy pointed this out by refusing the same assertion in a test as
/// "constant value", and it was right — a constant relation belongs in a const.
///
/// The cross-file half CANNOT live here (it reads app.js at runtime) and stays
/// in `the_two_budgets_cannot_collide`. That split is the point: the numeric
/// relation is a fact about this file, the agreement with app.js is a claim
/// about another one, and only the second can go stale silently.
const _: () = assert!(
    MODEL_TIMEOUT_S * 2 <= CLIENT_RUN_ABORT_S,
    "per-node budget must leave the client's per-run backstop room to receive the \
     server's answer — equal or near-equal budgets are AF-141"
);

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list))
        .route("/run", post(run))
        .route("/history", get(history))
        .route("/connect", post(connect))
}

// ---------------------------------------------------------------------------
// The files root
// ---------------------------------------------------------------------------

/// The root under which `.mdai` files live: `AMUX_FILES_ROOT` (tests, containers)
/// else `$HOME`. Deliberately the same root as `api/files.rs`, so a file browsed
/// there and a node run here are the same file.
fn mdai_root() -> PathBuf {
    // AMUX_FILES_ROOT is the EXPLICIT override (tests pin a temp root; the cloud
    // container pins its own) and wins first, so a live `mdai_root` pref read
    // from the real DB cannot leak into a test's isolated root (mdai_live_e2e).
    if let Ok(r) = std::env::var("AMUX_FILES_ROOT") {
        if !r.trim().is_empty() {
            return PathBuf::from(r);
        }
    }
    // Then a live `mdai_root` pref (set from the dashboard, no restart), so the
    // MDAI scan can be scoped to the user's notes vault. The $HOME default walks
    // the ENTIRE home tree (find_mdai_files, depth 12) and on a dev machine with
    // a huge ~/Dev that is slow enough to time out the list request, so the MDAI
    // tab shows nothing (AMUX-3310). Finally $HOME.
    if let Some(p) = mdai_root_pref() {
        return p;
    }
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| "/".into())
}

/// The live `mdai_root` pref (a path), read with a short-lived read-only
/// connection so this stays a sync free function; any error (no DB, no row,
/// empty) returns None and the env/$HOME default applies.
fn mdai_root_pref() -> Option<PathBuf> {
    let db = std::env::var("AMUX_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::config::amux_home().join("amux.db"));
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    let v: String = conn
        .query_row("SELECT value FROM prefs WHERE key='mdai_root'", [], |r| {
            r.get(0)
        })
        .ok()?;
    let v = v.trim();
    (!v.is_empty()).then(|| PathBuf::from(v))
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum MdaiError {
    /// A referenced path does not exist under the root.
    NotFound(String),
    /// Frontmatter or YAML could not be parsed.
    Parse(String),
    /// A dependency cycle, carrying the chain that closes it.
    Cycle(Vec<String>),
    /// A path escapes the files root (traversal / symlink out).
    Escape(String),
    /// Filesystem IO failed.
    Io(String),
    /// The model call itself failed.
    Model(String),
    /// A required request field was missing or empty.
    BadRequest(String),
    /// The model did not answer inside MODEL_TIMEOUT_S (AMUX-3765).
    ///
    /// SPLIT OUT OF `Model` BECAUSE THE STATUS DIFFERS, and the difference is
    /// what a reader does next. `Model` means the upstream answered with
    /// something unusable — investigate the model or the parse, and 502 is
    /// right. A TIMEOUT means it never answered — investigate the budget or the
    /// load, and 504 is right by definition.
    ///
    /// `lookup.rs` already made this call for the identical error shape
    /// (`GATEWAY_TIMEOUT` at its own model timeout). Two paths in one server
    /// disagreeing about the same fact is the thing to remove.
    ModelTimeout(String),
}

impl std::fmt::Display for MdaiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MdaiError::NotFound(p) => write!(f, "no such path: {p}"),
            MdaiError::Parse(m) => write!(f, "parse error: {m}"),
            MdaiError::Cycle(chain) => write!(f, "dependency cycle: {}", chain.join(" -> ")),
            MdaiError::Escape(p) => write!(f, "path escapes the files root: {p}"),
            MdaiError::Io(m) => write!(f, "io error: {m}"),
            MdaiError::Model(m) => write!(f, "model error: {m}"),
            MdaiError::ModelTimeout(m) => write!(f, "model timeout: {m}"),
            MdaiError::BadRequest(m) => write!(f, "{m}"),
        }
    }
}

/// The marker the timeout path writes, and the ONE place both halves agree on
/// it (AMUX-3765).
///
/// `complete()` returns a bare String, so the caller cannot tell a timeout from
/// any other model failure without reading the text. That is a weak seam and it
/// is named rather than hidden: if the timeout wording changes, this const is
/// what has to change with it, and `a_model_timeout_is_a_504_and_other_model_
/// errors_are_502` fails if the two drift.
pub(crate) const MODEL_TIMEOUT_MARKER: &str = "did not answer within";

/// The timeout message itself, so the PRODUCER and the classifier's test share
/// one definition instead of two copies of a format string.
///
/// The first version of that test rebuilt this string inline and called it a
/// seam check. It was not: mutating the producer's wording left the test green,
/// because the test was asserting against its own copy. A paraphrase cannot
/// detect drift in the thing it paraphrases — the failure this whole session
/// kept finding elsewhere, reproduced here by me.
pub(crate) fn model_timeout_msg(cli: &str) -> String {
    format!("{cli} {MODEL_TIMEOUT_MARKER} {MODEL_TIMEOUT_S}s")
}

/// Route a model failure to the variant whose STATUS is right for it.
pub(crate) fn classify_model_err(m: String) -> MdaiError {
    if m.contains(MODEL_TIMEOUT_MARKER) {
        MdaiError::ModelTimeout(m)
    } else {
        MdaiError::Model(m)
    }
}

impl MdaiError {
    fn status(&self) -> StatusCode {
        match self {
            MdaiError::NotFound(_) => StatusCode::NOT_FOUND,
            MdaiError::Parse(_) => StatusCode::BAD_REQUEST,
            // A cycle is a bad graph the caller must fix, and the body names it.
            MdaiError::Cycle(_) => StatusCode::BAD_REQUEST,
            MdaiError::Escape(_) => StatusCode::BAD_REQUEST,
            MdaiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            // The upstream (the model) failed, not this request: 502.
            MdaiError::Model(_) => StatusCode::BAD_GATEWAY,
            // It did not fail, it did not ANSWER. 504 by definition, and the
            // distinction is load-bearing: measured 2026-08-26, three sibling
            // calls finished at 105-125s against a 150s budget and the fourth
            // timed out. A 502 sent a reader hunting a model bug; the real
            // subject is a budget ~20% above typical runtime, which will fail
            // regularly under any load.
            MdaiError::ModelTimeout(_) => StatusCode::GATEWAY_TIMEOUT,
            MdaiError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn body(&self) -> Value {
        match self {
            MdaiError::Cycle(chain) => json!({"error": self.to_string(), "cycle": chain}),
            _ => json!({"error": self.to_string()}),
        }
    }
}

impl IntoResponse for MdaiError {
    fn into_response(self) -> Response {
        // Surface the classes that mean something broke, where a log sweep looks
        // (ethos: the next occurrence must self-announce).
        match &self {
            MdaiError::Cycle(chain) => {
                tracing::warn!(cycle = %chain.join(" -> "), "mdai: dependency cycle")
            }
            MdaiError::Model(m) => tracing::warn!(error = %m, "mdai: model call failed"),
            MdaiError::ModelTimeout(m) => tracing::warn!(
                error = %m,
                "mdai: model did not answer inside its budget — this is a TIMEOUT (504), not a \
                 model fault (502); look at MODEL_TIMEOUT_S and host load before the model"
            ),
            MdaiError::Io(m) => tracing::warn!(error = %m, "mdai: io error"),
            _ => {}
        }
        (self.status(), Json(self.body())).into_response()
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// One connection (edge) from a source into a node.
#[derive(Debug, Clone)]
pub struct Source {
    /// Path to the source, relative to the containing `.mdai` file's directory
    /// (absolute paths are allowed but still containment-checked).
    pub path: String,
    /// The configurable edge prompt.
    pub prompt: String,
}

/// A parsed `.mdai` document.
#[derive(Debug, Clone)]
pub struct MdaiDoc {
    pub sources: Vec<Source>,
    pub model: Option<String>,
    /// The markdown body: the node synthesis instruction.
    pub body: String,
}

/// A source in frontmatter is either a full `{path, prompt}` mapping or a bare
/// string path (convenience shorthand; the default edge prompt is filled in).
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum SourceRaw {
    Full {
        path: String,
        #[serde(default)]
        prompt: Option<String>,
    },
    Bare(String),
}

#[derive(serde::Deserialize, Default)]
struct FrontMatter {
    #[serde(default)]
    sources: Vec<SourceRaw>,
    #[serde(default)]
    model: Option<String>,
}

/// Split leading YAML frontmatter (delimited by `---` lines) from the markdown
/// body. Returns `(Some(yaml), body)` when a well-formed block is present, else
/// `(None, whole_text)` so a plain markdown file is still a valid node with no
/// sources.
fn split_frontmatter(text: &str) -> (Option<String>, String) {
    let stripped = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut it = stripped.lines();
    if it.next().map(|l| l.trim_end()) != Some("---") {
        return (None, stripped.to_string());
    }
    let mut yaml_lines: Vec<&str> = Vec::new();
    let mut body_lines: Vec<&str> = Vec::new();
    let mut in_body = false;
    for line in it {
        if !in_body && matches!(line.trim_end(), "---" | "...") {
            in_body = true;
            continue;
        }
        if in_body {
            body_lines.push(line);
        } else {
            yaml_lines.push(line);
        }
    }
    if !in_body {
        // Opening delimiter with no close: malformed. Treat the whole thing as
        // body rather than guessing where the frontmatter ends.
        return (None, stripped.to_string());
    }
    (Some(yaml_lines.join("\n")), body_lines.join("\n"))
}

/// Parse a `.mdai` document from its raw text.
pub fn parse_mdai(text: &str) -> Result<MdaiDoc, MdaiError> {
    let (yaml, body) = split_frontmatter(text);
    let fm: FrontMatter = match yaml {
        Some(y) if !y.trim().is_empty() => {
            serde_yaml::from_str(&y).map_err(|e| MdaiError::Parse(e.to_string()))?
        }
        _ => FrontMatter::default(),
    };
    let sources = fm
        .sources
        .into_iter()
        .map(|s| match s {
            SourceRaw::Full { path, prompt } => Source {
                path,
                prompt: prompt
                    .filter(|p| !p.trim().is_empty())
                    .unwrap_or_else(|| DEFAULT_EDGE_PROMPT.to_string()),
            },
            SourceRaw::Bare(path) => Source {
                path,
                prompt: DEFAULT_EDGE_PROMPT.to_string(),
            },
        })
        .collect();
    Ok(MdaiDoc {
        sources,
        model: fm.model.filter(|m| !m.trim().is_empty()),
        body,
    })
}

// ---------------------------------------------------------------------------
// Model injection seam
// ---------------------------------------------------------------------------

/// The one model call the engine makes. Injected so tests use a deterministic
/// fake and production uses the configured CLI.
pub trait ModelClient: Send + Sync {
    /// Run `model` over `prompt`, returning the completion or an error string.
    fn complete(&self, model: &str, prompt: &str) -> Result<String, String>;
}

/// Resolve the model for a node: per-file override, else `AMUX_HELPER_MODEL`,
/// else the fastest-Claude default. The result is a resolved string handed to
/// [`ModelClient::complete`]; no literal is spelled at that call site.
pub fn resolve_model(per_file: Option<&str>) -> String {
    if let Some(m) = per_file {
        let m = m.trim();
        if !m.is_empty() {
            return m.to_string();
        }
    }
    if let Ok(m) = std::env::var("AMUX_HELPER_MODEL") {
        let m = m.trim().to_string();
        if !m.is_empty() {
            return m;
        }
    }
    DEFAULT_FAST_MODEL.to_string()
}

/// Production model client: the configured helper CLI (`AMUX_HELPER_CLI`, default
/// `claude`) invoked with `--print --model <resolved>`. Same mechanism the rest
/// of the server's helper calls use. Bounded by [`MODEL_TIMEOUT_S`] so a wedged
/// CLI fails the run instead of hanging the request.
///
/// AF-141: this budget is PER NODE and the client's abort is PER RUN, and both
/// were 180. Two different quantities wearing the same number, so the client
/// always won the race and this error — which names the CLI and the budget —
/// could never reach a user; they got the client's guess ("a stuck source or a
/// wedged model call") instead. Worse for a chain: two legitimate 100s nodes
/// were killed by a budget that was never meant to bound the run.
///
/// The layering, now explicit: the SERVER bounds each node (it is the only side
/// that knows how many there are), and the CLIENT's abort is a backstop against
/// a hung server, so it must be generous enough that the server's specific
/// answer always arrives first.
pub struct CliModel;

impl ModelClient for CliModel {
    fn complete(&self, model: &str, prompt: &str) -> Result<String, String> {
        let cli = std::env::var("AMUX_HELPER_CLI").unwrap_or_else(|_| "claude".into());
        let mut cmd = std::process::Command::new(&cli);
        cmd.arg("--print").arg(prompt);
        if !model.trim().is_empty() {
            cmd.arg("--model").arg(model.trim());
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("could not run {cli}: {e}"))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(MODEL_TIMEOUT_S);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        return Err(model_timeout_msg(&cli));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => return Err(format!("{cli} failed: {e}")),
            }
        }
        let out = child
            .wait_with_output()
            .map_err(|e| format!("{cli} failed: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !stdout.is_empty() {
            // Non-empty stdout wins even on a non-zero exit: the CLI sometimes
            // prints its answer then exits non-zero.
            return Ok(stdout);
        }
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() {
            format!("{cli} exited without output")
        } else {
            err.chars().take(400).collect()
        })
    }
}

// ---------------------------------------------------------------------------
// Path resolution (containment)
// ---------------------------------------------------------------------------

fn canon_root(root: &Path) -> Result<PathBuf, MdaiError> {
    root.canonicalize()
        .map_err(|e| MdaiError::Io(format!("files root {} unusable: {e}", root.display())))
}

/// Resolve a path that must EXIST, keeping it under the canonicalized root.
/// `base` is the directory the path is relative to (the root for the entry node,
/// the containing file's directory for a source).
fn resolve_existing(root_canon: &Path, base: &Path, rel: &str) -> Result<PathBuf, MdaiError> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Err(MdaiError::BadRequest("empty path".into()));
    }
    let p = Path::new(rel);
    let joined = if p.is_absolute() { p.to_path_buf() } else { base.join(p) };
    let canon = joined
        .canonicalize()
        .map_err(|_| MdaiError::NotFound(rel.to_string()))?;
    if !canon.starts_with(root_canon) {
        return Err(MdaiError::Escape(rel.to_string()));
    }
    Ok(canon)
}

/// Root-relative key for a canonical path, used as the `mdai_runs.path` column
/// and in reporting. Forward slashes, no leading slash.
fn rel_key(root_canon: &Path, canon: &Path) -> String {
    canon
        .strip_prefix(root_canon)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| canon.to_string_lossy().to_string())
}

fn is_mdai(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case(MDAI_EXT))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Source resolution
// ---------------------------------------------------------------------------

/// Read a plain file, capped to [`MAX_SOURCE_BYTES`] on a char boundary.
fn read_capped(path: &Path) -> Result<String, MdaiError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| MdaiError::Io(format!("{}: {e}", path.display())))?;
    Ok(cap_str(&text, MAX_SOURCE_BYTES))
}

fn cap_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n...[truncated]", &s[..end])
}

/// Expand a folder source into concatenated file contents (top-level regular
/// files, sorted by name for determinism), size-capped in total.
fn expand_folder(dir: &Path) -> Result<String, MdaiError> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| MdaiError::Io(format!("{}: {e}", dir.display())))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    entries.sort();
    let mut out = String::new();
    for f in entries {
        if out.len() >= MAX_FOLDER_BYTES {
            out.push_str("\n...[folder truncated]");
            break;
        }
        let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let remaining = MAX_FOLDER_BYTES.saturating_sub(out.len());
        let content = std::fs::read_to_string(&f).unwrap_or_default();
        out.push_str(&format!("### {name}\n{}\n\n", cap_str(&content, remaining)));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Hashing and prompt assembly
// ---------------------------------------------------------------------------

fn sha256_hex(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]); // domain separator so a||b != a'||b'
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn source_block(display_path: &str, edge_prompt: &str, resolved: &str) -> String {
    format!("[source: {display_path}]\n{edge_prompt}\n\n{resolved}")
}

fn build_prompt(body: &str, assembled: &str) -> String {
    if assembled.trim().is_empty() {
        body.to_string()
    } else {
        format!(
            "{body}\n\nContext from connected sources follows. Use it to produce the output.\n\n{assembled}"
        )
    }
}

// ---------------------------------------------------------------------------
// The resolver
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug)]
pub struct NodeRun {
    pub path: String,
    pub model: String,
    pub cached: bool,
    pub inputs_hash: String,
}

#[derive(Serialize, Debug)]
pub struct RunResult {
    /// Root-relative path of the entry node.
    pub path: String,
    /// The entry node's output (the "latest output" the run produced).
    pub output: String,
    pub model: String,
    pub cached: bool,
    pub inputs_hash: String,
    pub ts: i64,
    /// Every node executed, in upstream-first order (the entry node is last).
    pub nodes: Vec<NodeRun>,
}

struct RunCtx<'a> {
    store: &'a Store,
    root_canon: PathBuf,
    model: &'a dyn ModelClient,
    session: Option<String>,
    /// Per-run memo: a node resolved once (a diamond does not re-run a shared
    /// upstream twice).
    memo: HashMap<PathBuf, NodeRun>,
    /// Cycle-detection stack: the path from the entry node to the node being
    /// resolved. Contains only ANCESTORS, so a repeat means a real cycle.
    visiting: Vec<PathBuf>,
    /// Execution order, upstream-first, for reporting.
    order: Vec<NodeRun>,
    /// Model calls actually spent this run (a cache hit does not increment).
    calls: usize,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The last recorded run for a path, if any: `(inputs_hash, output)`.
fn latest_run(store: &Store, path: &str) -> Option<(String, String)> {
    let conn = store.read().ok()?;
    conn.query_row(
        "SELECT inputs_hash, output FROM mdai_runs WHERE path=?1 ORDER BY id DESC LIMIT 1",
        rusqlite::params![path],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )
    .ok()
}

/// One row of run history to append. Owns its strings so it can move into the
/// writer-thread closure.
struct RunRow {
    path: String,
    ts: i64,
    inputs_hash: String,
    output: String,
    model: String,
    cached: bool,
    session: Option<String>,
    /// Model-call wall time; None on cached rows (no call happened).
    dur_ms: Option<i64>,
}

fn record_run(store: &Store, row: RunRow) -> Result<(), MdaiError> {
    store
        .write(move |conn| {
            conn.execute(
                "INSERT INTO mdai_runs (path, ts, inputs_hash, output, model, cached, session, dur_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    row.path,
                    row.ts,
                    row.inputs_hash,
                    row.output,
                    row.model,
                    row.cached as i64,
                    row.session,
                    row.dur_ms
                ],
            )?;
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .map(|_| ())
        .map_err(|e| MdaiError::Io(e.to_string()))
}

/// Format a millisecond timestamp as `MM-DD HH:MM` in local time.
fn fmt_day_min(ts_ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ts_ms)
        .single()
        .map(|dt| dt.format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// Per-message cap. The total cap below is checked after a push, so one message
/// larger than the whole budget blows straight through it — and there is such a
/// message in the live window right now (5,974 KB, sent today).
const MSG_CAP: usize = 4_000;
/// Total budget for the assembled message block.
const MSGS_TOTAL_CAP: usize = 120_000;

/// Assemble the `amux:messages` block: SELECT newest-first (the caller orders
/// `ts DESC`), RENDER oldest-first. Returns the block and how many messages
/// reached it.
///
/// AF-142. This used to take `ts ASC` and break at the byte cap, so the cap kept
/// the OLDEST messages and dropped the newest — then said "(older messages
/// truncated)", asserting the opposite of what it had done. Measured on the live
/// DB: 512 user messages in the 7-day window, cap reached at message 240, so the
/// most recent 272 never reached the prompt while the label claimed they were
/// the ones kept. For a node asking "what am I trying to accomplish", that is
/// the wrong half of the window by a wide margin.
///
/// Same defect the logs endpoint carried until this morning (AF-131): a cap
/// whose DIRECTION is part of its correctness, quietly taking the end nobody
/// wants. Pure so both properties are testable without a database.
pub(crate) fn assemble_messages<I: IntoIterator<Item = (i64, String, String)>>(
    newest_first: I,
) -> (String, usize) {
    let mut lines: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;
    for (ts_ms, session, text) in newest_first {
        let line = format!(
            "[{}] {session}: {}\n",
            fmt_day_min(ts_ms),
            cap_str(text.replace('\n', " ").trim(), MSG_CAP)
        );
        if used + line.len() > MSGS_TOTAL_CAP {
            truncated = true;
            break;
        }
        used += line.len();
        lines.push(line);
    }
    let n = lines.len();
    let mut out = String::with_capacity(used + 64);
    if truncated {
        out.push_str("… (older messages beyond this point were truncated)\n\n");
    }
    for line in lines.iter().rev() {
        out.push_str(line);
    }
    (out, n)
}

/// Resolve an `amux:` data source (AMUX-3294). Grammar: `messages[?days=N]`
/// (default 14, capped at 90). Reads `cmd_history` directly from the run's DB
/// store and returns the recent user directives as text for the synthesis
/// prompt. `cmd_history.ts` is in MILLISECONDS.
fn resolve_amux_source(ctx: &RunCtx, spec: &str) -> Result<String, MdaiError> {
    let (kind, query) = spec.split_once('?').unwrap_or((spec, ""));
    let kind = kind.trim();
    let days: i64 = query
        .split('&')
        .find_map(|kv| kv.trim().strip_prefix("days="))
        .and_then(|v| v.parse().ok())
        .filter(|d| *d > 0 && *d <= 90)
        .unwrap_or(14);
    match kind {
        "messages" => {
            let conn = ctx
                .store
                .read()
                .map_err(|e| MdaiError::Io(format!("db read: {e}")))?;
            let cutoff_ms = (now_secs() * 1000) - days * 86_400_000;
            let mut stmt = conn
                .prepare(
                    "SELECT ts, COALESCE(session,''), COALESCE(text,'') \
                     FROM cmd_history \
                     WHERE type='user' AND ts >= ?1 AND COALESCE(text,'') <> '' \
                     ORDER BY ts DESC LIMIT 4000",
                )
                .map_err(|e| MdaiError::Io(format!("db prepare: {e}")))?;
            let rows = stmt
                .query_map(rusqlite::params![cutoff_ms], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })
                .map_err(|e| MdaiError::Io(format!("db query: {e}")))?;
            let mut out = String::new();
            let (body, n) = assemble_messages(rows.flatten());
            out.push_str(&body);
            if n == 0 {
                return Ok(format!("(no user messages in the last {days} days)"));
            }
            Ok(format!(
                "The last {days} days of amux user directives ({n} messages), oldest first:\n\n{out}"
            ))
        }
        other => Err(MdaiError::Io(format!(
            "unknown amux source '{other}' (supported: messages)"
        ))),
    }
}

/// Resolve and run a single node, recursing into upstream `.mdai` sources first.
/// Returns the node's output.
fn run_node(ctx: &mut RunCtx, abs_path: &Path) -> Result<String, MdaiError> {
    let canon = abs_path
        .canonicalize()
        .map_err(|_| MdaiError::NotFound(abs_path.to_string_lossy().to_string()))?;
    if !canon.starts_with(&ctx.root_canon) {
        return Err(MdaiError::Escape(canon.to_string_lossy().to_string()));
    }

    // Cycle: this node is already an ancestor of itself.
    if let Some(pos) = ctx.visiting.iter().position(|p| p == &canon) {
        let mut chain: Vec<String> = ctx.visiting[pos..]
            .iter()
            .map(|p| rel_key(&ctx.root_canon, p))
            .collect();
        chain.push(rel_key(&ctx.root_canon, &canon)); // close the loop, naming it
        return Err(MdaiError::Cycle(chain));
    }

    // Already computed this run (diamond): reuse.
    if let Some(node) = ctx.memo.get(&canon) {
        return latest_output_for(ctx.store, &node.path)
            .ok_or_else(|| MdaiError::Io("memo output vanished".into()));
    }

    let text = std::fs::read_to_string(&canon)
        .map_err(|e| MdaiError::Io(format!("{}: {e}", canon.display())))?;
    let doc = parse_mdai(&text)?;
    let base = canon.parent().unwrap_or(&ctx.root_canon).to_path_buf();

    ctx.visiting.push(canon.clone());
    let mut blocks: Vec<String> = Vec::new();
    for src in &doc.sources {
        // AMUX-3294: an `amux:` source pulls LIVE amux data (currently messages)
        // from the DB at run time, so a prompt-only node like Priorities fetches
        // its own context instead of a static file. The run is inside the server,
        // so this reads the DB directly — which is why the model kept answering
        // "I can't reach amux" (a one-shot LLM has no env, no tools; the DATA has
        // to be injected here, not fetched by the model). Handled BEFORE the
        // filesystem resolver, which would reject the pseudo-path.
        let resolved = if let Some(spec) = src.path.strip_prefix("amux:") {
            resolve_amux_source(ctx, spec)?
        } else {
            let src_abs = resolve_existing(&ctx.root_canon, &base, &src.path)?;
            if is_mdai(&src_abs) {
                run_node(ctx, &src_abs)?
            } else if src_abs.is_dir() {
                expand_folder(&src_abs)?
            } else {
                read_capped(&src_abs)?
            }
        };
        blocks.push(source_block(&src.path, &src.prompt, &resolved));
    }
    ctx.visiting.pop();

    let assembled = blocks.join("\n\n");
    let model_name = resolve_model(doc.model.as_deref());
    let inputs_hash = sha256_hex(&[&model_name, &doc.body, &assembled]);
    let rel = rel_key(&ctx.root_canon, &canon);

    // Cache short-circuit: an unchanged node reuses its last output rather than
    // spending a model call to reproduce an identical result.
    let (output, cached, dur_ms) = match latest_run(ctx.store, &rel) {
        Some((prev_hash, prev_out)) if prev_hash == inputs_hash => (prev_out, true, None),
        _ => {
            let prompt = build_prompt(&doc.body, &assembled);
            ctx.calls += 1;
            // The chain's latency used to be INVISIBLE (AMUX-3410): zero log
            // lines and nothing in mdai_runs said where the time went, so a
            // "why is this slow" was answered by reconstructing timings from
            // adjacent rows' timestamps. Measured while fixing it: a bare CLI
            // boot is ~10.6s and a 256KB-prompt haiku synthesis ~80s, so the
            // prompt (source size), not the CLI spawn, is the cost to watch —
            // which is exactly what prompt_bytes in this line lets a reader
            // see without re-measuring.
            let t0 = std::time::Instant::now();
            let out = ctx
                .model
                .complete(&model_name, &prompt)
                .map_err(classify_model_err)?;
            let dur = t0.elapsed().as_millis() as i64;
            tracing::info!(
                path = %rel, model = %model_name,
                prompt_bytes = prompt.len(), output_bytes = out.len(), dur_ms = dur,
                "mdai node model call completed"
            );
            (out, false, Some(dur))
        }
    };

    let ts = now_secs();
    record_run(
        ctx.store,
        RunRow {
            path: rel.clone(),
            ts,
            inputs_hash: inputs_hash.clone(),
            output: output.clone(),
            model: model_name.clone(),
            cached,
            session: ctx.session.clone(),
            dur_ms,
        },
    )?;

    let node = NodeRun {
        path: rel,
        model: model_name,
        cached,
        inputs_hash,
    };
    ctx.memo.insert(canon, node.clone());
    ctx.order.push(node);
    Ok(output)
}

/// The most recent output recorded for a path (used to service a diamond memo
/// hit without re-reading the node).
fn latest_output_for(store: &Store, path: &str) -> Option<String> {
    latest_run(store, path).map(|(_, out)| out)
}

/// Run the DAG rooted at `path` (relative to `root`), recording history and
/// returning the entry node's latest output.
pub fn run_dag(
    store: &Store,
    root: &Path,
    path: &str,
    model: &dyn ModelClient,
    session: Option<&str>,
) -> Result<RunResult, MdaiError> {
    let root_canon = canon_root(root)?;
    let entry_abs = resolve_existing(&root_canon, &root_canon, path)?;
    if !is_mdai(&entry_abs) {
        return Err(MdaiError::BadRequest(format!(
            "not a .{MDAI_EXT} file: {path}"
        )));
    }
    let mut ctx = RunCtx {
        store,
        root_canon: root_canon.clone(),
        model,
        session: session.map(|s| s.to_string()),
        memo: HashMap::new(),
        visiting: Vec::new(),
        order: Vec::new(),
        calls: 0,
    };
    let output = run_node(&mut ctx, &entry_abs)?;
    let entry = ctx
        .order
        .last()
        .cloned()
        .ok_or_else(|| MdaiError::Io("no node executed".into()))?;
    Ok(RunResult {
        path: entry.path.clone(),
        output,
        model: entry.model.clone(),
        cached: entry.cached,
        inputs_hash: entry.inputs_hash.clone(),
        ts: now_secs(),
        nodes: ctx.order,
    })
}

// ---------------------------------------------------------------------------
// Listing and history
// ---------------------------------------------------------------------------

/// Recursively find every `.mdai` file under the root. Bounded traversal that
/// skips dotdirs and common heavy directories so listing does not walk a whole
/// home.
/// Wall-clock budget for the scan. AF-129: the $HOME default root makes this
/// walk's size unknowable (the comment on `mdai_root` already records it
/// timing out the live list request, AMUX-3310), and with NO time bound one
/// slow or kernel-blocking directory turned `GET /api/files/mdai` into the
/// route that silently wedged the whole pre-push gate — three orphaned test
/// processes, the oldest 23h. A budget makes the walk answer with what it
/// found; the `truncated` flag and the WARN below make the cut visible
/// instead of reading as "that's all the files there are" (rule 4).
const SCAN_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Returns the files found plus whether the scan was CUT (budget or caps).
fn find_mdai_files(root: &Path) -> (Vec<PathBuf>, bool) {
    fn walk(
        dir: &Path,
        out: &mut Vec<PathBuf>,
        depth: usize,
        deadline: std::time::Instant,
        cut: &mut bool,
    ) {
        if depth > 12 || out.len() > 5000 || std::time::Instant::now() > deadline {
            *cut = true;
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if p.is_dir() {
                if matches!(name.as_str(), "node_modules" | "target" | ".git" | "Library") {
                    continue;
                }
                walk(&p, out, depth + 1, deadline, cut);
            } else if is_mdai(&p) {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    let mut cut = false;
    walk(root, &mut out, 0, std::time::Instant::now() + SCAN_BUDGET, &mut cut);
    out.sort();
    if cut {
        tracing::warn!(
            target: "amux::mdai", root = %root.display(), found = out.len(),
            "mdai scan CUT (budget/depth/cap) — the list is partial; scope mdai_root \
             (pref or AMUX_FILES_ROOT) to a real notes tree"
        );
    }
    (out, cut)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn header_session(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-amux-session")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `GET /api/files/mdai` - every `.mdai` file under the root with metadata.
async fn list(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    let res = tokio::task::spawn_blocking(move || {
        let root = mdai_root();
        let root_canon = match canon_root(&root) {
            Ok(r) => r,
            Err(e) => return Err(e),
        };
        let mut items: Vec<Value> = Vec::new();
        let (found, truncated) = find_mdai_files(&root_canon);
        for path in found {
            let rel = rel_key(&root_canon, &path);
            let (sources, model, title) = match std::fs::read_to_string(&path) {
                Ok(text) => match parse_mdai(&text) {
                    Ok(doc) => (
                        doc.sources.len(),
                        doc.model,
                        doc.body
                            .lines()
                            .map(|l| l.trim_start_matches('#').trim())
                            .find(|l| !l.is_empty())
                            .unwrap_or("")
                            .chars()
                            .take(120)
                            .collect::<String>(),
                    ),
                    Err(_) => (0, None, String::new()),
                },
                Err(_) => (0, None, String::new()),
            };
            let mtime = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64);
            let last_run: Option<i64> = store.read().ok().and_then(|c| {
                c.query_row(
                    "SELECT ts FROM mdai_runs WHERE path=?1 ORDER BY id DESC LIMIT 1",
                    rusqlite::params![rel],
                    |r| r.get::<_, i64>(0),
                )
                .ok()
            });
            items.push(json!({
                "path": rel,
                "sources": sources,
                "model": model,
                "title": title,
                "mtime": mtime,
                "last_run": last_run,
            }));
        }
        Ok((items, truncated))
    })
    .await;
    match res {
        // `truncated` rides in the response so a partial scan cannot read as
        // "that is every file" (AF-129 / rule 4).
        Ok(Ok((items, truncated))) => {
            Json(json!({ "files": items, "truncated": truncated })).into_response()
        }
        Ok(Err(e)) => e.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})))
            .into_response(),
    }
}

/// `POST /api/files/mdai/run {path}` - run the DAG, record history, return the
/// entry node's latest output.
async fn run(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let path = body["path"].as_str().unwrap_or("").trim().to_string();
    if path.is_empty() {
        return MdaiError::BadRequest("path required".into()).into_response();
    }
    let session = header_session(&headers);
    let store = state.store.clone();
    let res = tokio::task::spawn_blocking(move || {
        let root = mdai_root();
        let model = CliModel;
        run_dag(&store, &root, &path, &model, session.as_deref())
    })
    .await;
    match res {
        Ok(Ok(r)) => Json(r).into_response(),
        Ok(Err(e)) => e.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})))
            .into_response(),
    }
}

/// `GET /api/files/mdai/history?path=...` - newest-first run history for a node.
async fn history(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let path = q.get("path").map(|s| s.trim()).unwrap_or("").to_string();
    if path.is_empty() {
        return MdaiError::BadRequest("path query param required".into()).into_response();
    }
    // Normalize to the same root-relative key the runs are stored under, so a
    // caller passing an absolute or relative path both find their history.
    let store = state.store.clone();
    let res = tokio::task::spawn_blocking(move || {
        let root = mdai_root();
        let key = match canon_root(&root) {
            Ok(root_canon) => match resolve_existing(&root_canon, &root_canon, &path) {
                Ok(canon) => rel_key(&root_canon, &canon),
                // The file may have been deleted but still have history: fall
                // back to the path as given.
                Err(_) => path.clone(),
            },
            Err(_) => path.clone(),
        };
        let conn = store.read().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT id, path, ts, inputs_hash, output, model, cached, session, dur_ms
                 FROM mdai_runs WHERE path=?1 ORDER BY id DESC",
            )
            .map_err(|e| e.to_string())?;
        let rows: Vec<Value> = stmt
            .query_map(rusqlite::params![key], |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "path": r.get::<_, String>(1)?,
                    "ts": r.get::<_, i64>(2)?,
                    "inputs_hash": r.get::<_, String>(3)?,
                    "output": r.get::<_, String>(4)?,
                    "model": r.get::<_, String>(5)?,
                    "cached": r.get::<_, i64>(6)? != 0,
                    "dur_ms": r.get::<_, Option<i64>>(8)?,
                    "session": r.get::<_, Option<String>>(7)?,
                }))
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok::<_, String>(json!({ "path": key, "runs": rows }))
    })
    .await;
    match res {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})))
            .into_response(),
    }
}

/// `POST /api/files/mdai/connect {source, target, prompt?}` - add an edge from
/// `source` into `target`, writing the (default or given) edge prompt into the
/// target's frontmatter.
async fn connect(State(_state): State<AppState>, Json(body): Json<Value>) -> Response {
    let source = body["source"].as_str().unwrap_or("").trim().to_string();
    let target = body["target"].as_str().unwrap_or("").trim().to_string();
    let prompt = body["prompt"]
        .as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_EDGE_PROMPT.to_string());
    if source.is_empty() || target.is_empty() {
        return MdaiError::BadRequest("source and target required".into()).into_response();
    }
    let res = tokio::task::spawn_blocking(move || connect_edge(&source, &target, &prompt)).await;
    match res {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => e.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})))
            .into_response(),
    }
}

/// Add `source` as a connection to `target`, writing back the target's file.
/// Shared by the handler and the tests so the frontmatter round-trip is exercised
/// directly.
pub fn connect_edge(source: &str, target: &str, prompt: &str) -> Result<Value, MdaiError> {
    let root = mdai_root();
    let root_canon = canon_root(&root)?;
    // The target must exist and be a .mdai file (we are editing its frontmatter).
    let target_abs = resolve_existing(&root_canon, &root_canon, target)?;
    if !is_mdai(&target_abs) {
        return Err(MdaiError::BadRequest(format!("target is not a .{MDAI_EXT} file: {target}")));
    }
    // The source need not exist yet at connect time, but if a value is given it
    // must not escape the root. Resolve leniently: reject only a clear escape.
    if Path::new(source).is_absolute() {
        if let Ok(c) = Path::new(source).canonicalize() {
            if !c.starts_with(&root_canon) {
                return Err(MdaiError::Escape(source.to_string()));
            }
        }
    }

    let text = std::fs::read_to_string(&target_abs)
        .map_err(|e| MdaiError::Io(format!("{}: {e}", target_abs.display())))?;
    let (yaml, bodytext) = split_frontmatter(&text);
    let mut map: serde_yaml::Mapping = match yaml {
        Some(y) if !y.trim().is_empty() => {
            serde_yaml::from_str(&y).map_err(|e| MdaiError::Parse(e.to_string()))?
        }
        _ => serde_yaml::Mapping::new(),
    };
    let key = serde_yaml::Value::String("sources".into());
    let seq = match map.get_mut(&key) {
        Some(serde_yaml::Value::Sequence(s)) => s,
        _ => {
            map.insert(key.clone(), serde_yaml::Value::Sequence(Vec::new()));
            match map.get_mut(&key) {
                Some(serde_yaml::Value::Sequence(s)) => s,
                _ => unreachable!("just inserted a sequence"),
            }
        }
    };
    let mut edge = serde_yaml::Mapping::new();
    edge.insert(
        serde_yaml::Value::String("path".into()),
        serde_yaml::Value::String(source.to_string()),
    );
    edge.insert(
        serde_yaml::Value::String("prompt".into()),
        serde_yaml::Value::String(prompt.to_string()),
    );
    seq.push(serde_yaml::Value::Mapping(edge));

    let front = serde_yaml::to_string(&serde_yaml::Value::Mapping(map))
        .map_err(|e| MdaiError::Io(e.to_string()))?;
    let mut rebuilt = String::from("---\n");
    rebuilt.push_str(&front);
    if !front.ends_with('\n') {
        rebuilt.push('\n');
    }
    rebuilt.push_str("---\n");
    rebuilt.push_str(&bodytext);
    if !bodytext.ends_with('\n') {
        rebuilt.push('\n');
    }
    std::fs::write(&target_abs, &rebuilt)
        .map_err(|e| MdaiError::Io(format!("{}: {e}", target_abs.display())))?;

    Ok(json!({
        "ok": true,
        "target": rel_key(&root_canon, &target_abs),
        "source": source,
        "prompt": prompt,
    }))
}

// ---------------------------------------------------------------------------
// Deterministic unit tests (fake model, no network, no real model call)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    /// AMUX-3765: a model TIMEOUT is a 504, a model FAULT is a 502, and the two
    /// send a reader to different places.
    ///
    /// The filed card was `POST /api/files/mdai/run -> HTTP 502` with the body
    /// "claude did not answer within 150s". 502 says the upstream answered with
    /// something unusable, so it points at the model or the parse. The evidence
    /// said otherwise: three sibling calls the same minute finished at 105-125s
    /// against a 150s budget and the fourth ran out of clock. The subject is the
    /// BUDGET, and 504 is the code that says so.
    ///
    /// `lookup.rs` already returns GATEWAY_TIMEOUT for the identical shape. Two
    /// paths in one server disagreeing about the same fact is what this removes.
    #[test]
    fn a_model_timeout_is_a_504_and_other_model_errors_are_502() {
        let t = classify_model_err(format!("claude {MODEL_TIMEOUT_MARKER} 150s"));
        assert!(matches!(t, MdaiError::ModelTimeout(_)), "the timeout shape must classify as one");
        assert_eq!(t.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(t.to_string().contains("model timeout"), "and read as one: {t}");

        // CONTROL: a genuine model fault keeps 502. Without this the classifier
        // could route everything to 504 and the assertion above would pass,
        // which would hide real model faults behind a timeout label.
        let e = classify_model_err("claude exited 1: bad JSON".to_string());
        assert!(matches!(e, MdaiError::Model(_)));
        assert_eq!(e.status(), StatusCode::BAD_GATEWAY);

        // THE SEAM IS A STRING MATCH, so pin that the PRODUCER still writes what
        // the classifier reads — by calling the producer, not by rebuilding its
        // format string. The first version of this did rebuild it, and mutating
        // the producer's wording left the test green: a paraphrase cannot detect
        // drift in the thing it paraphrases.
        let produced = model_timeout_msg("claude");
        assert!(
            produced.contains(MODEL_TIMEOUT_MARKER),
            "the timeout message and its classifier have drifted: {produced}"
        );
        assert!(matches!(classify_model_err(produced), MdaiError::ModelTimeout(_)));
    }

    use super::*;
    use std::sync::Mutex;

    /// A deterministic model that records every call and returns an output that
    /// embeds a hash of the prompt, so a downstream node's context can be shown
    /// to contain an upstream node's exact output.
    struct FakeModel {
        calls: Mutex<Vec<(String, String, String)>>, // (model, prompt, output)
    }
    impl FakeModel {
        fn new() -> Self {
            FakeModel { calls: Mutex::new(Vec::new()) }
        }
        fn count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
        fn calls(&self) -> Vec<(String, String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }
    impl ModelClient for FakeModel {
        fn complete(&self, model: &str, prompt: &str) -> Result<String, String> {
            let mut c = self.calls.lock().unwrap();
            let out = format!("FAKEOUT#{}:{}", c.len(), &sha256_hex(&[prompt])[..12]);
            c.push((model.to_string(), prompt.to_string(), out.clone()));
            Ok(out)
        }
    }

    fn temp_store(dir: &Path) -> Store {
        Store::open(&dir.join("mdai-test.db")).unwrap()
    }

    fn write(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    // (1) DAG resolves upstream-first: a leaf .mdai over a plain file feeds a mid
    // .mdai; the mid's assembled context must contain the leaf's output, and the
    // leaf must have run first.
    #[test]
    fn dag_resolves_upstream_first_and_feeds_leaf_output_into_mid() {
        let dir = tempfile::tempdir().unwrap();
        let store = temp_store(dir.path());
        write(dir.path(), "plain.txt", "PLAINSOURCE payload");
        write(
            dir.path(),
            "leaf.mdai",
            "---\nsources:\n  - path: plain.txt\n    prompt: use the file\n---\nLEAFBODY synthesize",
        );
        write(
            dir.path(),
            "mid.mdai",
            "---\nsources:\n  - path: leaf.mdai\n    prompt: use the leaf\n---\nMIDBODY synthesize",
        );
        let fake = FakeModel::new();
        let r = run_dag(&store, dir.path(), "mid.mdai", &fake, None).unwrap();

        let calls = fake.calls();
        assert_eq!(calls.len(), 2, "leaf and mid each call the model exactly once");
        // Upstream-first: leaf ran before mid.
        assert!(calls[0].1.contains("LEAFBODY"), "first call is the leaf: {}", calls[0].1);
        assert!(calls[0].1.contains("PLAINSOURCE"), "leaf sees the plain file");
        assert!(calls[1].1.contains("MIDBODY"), "second call is the mid: {}", calls[1].1);
        // The mid's assembled context contains the leaf's exact output.
        let leaf_output = &calls[0].2;
        assert!(
            calls[1].1.contains(leaf_output.as_str()),
            "mid context must contain leaf output {leaf_output:?}; got {}",
            calls[1].1
        );
        // The run result reports upstream-first order (entry node last).
        assert_eq!(r.nodes.len(), 2);
        assert_eq!(r.nodes[0].path, "leaf.mdai");
        assert_eq!(r.nodes[1].path, "mid.mdai");
        assert_eq!(r.path, "mid.mdai");
        assert!(r.output.starts_with("FAKEOUT#1"));
    }

    // (2) A cycle is detected and errors, naming the cycle.
    #[test]
    fn a_cycle_is_detected_and_named() {
        let dir = tempfile::tempdir().unwrap();
        let store = temp_store(dir.path());
        write(dir.path(), "a.mdai", "---\nsources:\n  - path: b.mdai\n---\nA body");
        write(dir.path(), "b.mdai", "---\nsources:\n  - path: a.mdai\n---\nB body");
        let fake = FakeModel::new();
        let err = run_dag(&store, dir.path(), "a.mdai", &fake, None).unwrap_err();
        match err {
            MdaiError::Cycle(chain) => {
                let joined = chain.join(" -> ");
                assert!(joined.contains("a.mdai"), "cycle names a.mdai: {joined}");
                assert!(joined.contains("b.mdai"), "cycle names b.mdai: {joined}");
            }
            other => panic!("expected a cycle error, got {other:?}"),
        }
        assert_eq!(fake.count(), 0, "a cyclic graph must not spend a model call");
    }

    // (3) The cache short-circuits an unchanged re-run: the fake model is invoked
    // once, not twice. Negative control: a changed source recomputes.
    #[test]
    fn cache_short_circuits_unchanged_and_recomputes_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let store = temp_store(dir.path());
        write(dir.path(), "plain.txt", "ORIGINAL content");
        write(
            dir.path(),
            "node.mdai",
            "---\nsources:\n  - path: plain.txt\n    prompt: use it\n---\nBODY one",
        );
        let fake = FakeModel::new();

        let r1 = run_dag(&store, dir.path(), "node.mdai", &fake, None).unwrap();
        assert_eq!(fake.count(), 1, "first run computes");
        assert!(!r1.cached);

        // Unchanged re-run: reuse, no new model call.
        let r2 = run_dag(&store, dir.path(), "node.mdai", &fake, None).unwrap();
        assert_eq!(fake.count(), 1, "unchanged re-run must NOT call the model again");
        assert!(r2.cached, "the re-run is served from cache");
        assert_eq!(r1.output, r2.output, "cached output is identical");

        // Negative control: change the source -> inputs hash changes -> recompute.
        write(dir.path(), "plain.txt", "CHANGED content");
        let r3 = run_dag(&store, dir.path(), "node.mdai", &fake, None).unwrap();
        assert_eq!(fake.count(), 2, "a changed source MUST recompute");
        assert!(!r3.cached);
        assert_ne!(r1.output, r3.output, "changed input produces a new output");
    }

    /// AF-141. The server bounds each NODE; the client bounds the whole RUN.
    /// They were both 180 — the same number for two different quantities — so
    /// the client always won the race and this server's specific error could
    /// never reach a user.
    ///
    /// The second half is the load-bearing one. Asserting only that the two
    /// Rust constants differ would pass forever while `app.js` said something
    /// else entirely, because CLIENT_RUN_ABORT_S is a CLAIM ABOUT ANOTHER FILE.
    /// So read that file and check the claim is still true. Same pattern as
    /// board_contract_filters.rs reading the shipped `amux` CLI.
    #[test]
    fn the_two_budgets_cannot_collide() {
        let app_js = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../amux-dashboard/static/app.js"),
        )
        .expect("dashboard app.js not found");
        let needle = "_ctrl.abort(), ";
        let at = app_js
            .find(needle)
            .unwrap_or_else(|| panic!("no mdai run abort found in app.js — did it move?"));
        let ms: u64 = app_js[at + needle.len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .expect("abort budget in app.js is not a plain integer of milliseconds");
        assert_eq!(
            ms / 1000,
            CLIENT_RUN_ABORT_S,
            "app.js aborts the run at {}s but CLIENT_RUN_ABORT_S here says {CLIENT_RUN_ABORT_S}s \
             — this constant is a claim about that file and it has gone stale",
            ms / 1000
        );
    }

    /// AF-142. Two properties, and the first is the one that was wrong in
    /// production: when the budget forces a choice, keep the NEWEST messages.
    #[test]
    fn the_message_block_keeps_the_newest_and_caps_each_one() {
        // Caller supplies ts DESC. 400 messages of ~500 bytes each overflows the
        // 120,000-byte budget, so the cap must bite and choose a side.
        let rows: Vec<(i64, String, String)> = (0..400)
            .map(|i| {
                let ts = 1_700_000_000_000i64 + (i as i64) * 60_000;
                (ts, "lane".to_string(), format!("m{i:04}-{}", "x".repeat(480)))
            })
            .rev() // newest first, as the SQL now returns
            .collect();
        let (block, n) = assemble_messages(rows);

        assert!(n > 0 && n < 400, "the cap must actually bite: n={n}");
        assert!(block.len() <= MSGS_TOTAL_CAP + 200, "over budget: {}", block.len());

        // THE REGRESSION: the newest message must be present and the oldest gone.
        // Reversed, this test still passes on the broken version — which is why
        // both halves are asserted, not just "some messages survived".
        assert!(block.contains("m0399-"), "the NEWEST message must survive the cap");
        assert!(!block.contains("m0000-"), "the OLDEST must be the one dropped");

        // Rendered oldest-first among those kept, which is what the prompt says.
        let first = block.find("m0").expect("no message rendered");
        let newest = block.find("m0399-").expect("newest missing");
        assert!(first < newest, "kept messages must render oldest-first");
        assert!(block.starts_with("… (older messages"), "truncation must be announced: {}", &block[..40]);

        // PER-MESSAGE CAP: one message larger than the entire budget must not
        // blow through it. The old code pushed first and checked after, so a
        // single 5,974 KB message (there is one in the live window) produced a
        // ~6MB block from a 120KB cap.
        let huge = vec![(1_700_000_000_000i64, "lane".into(), "y".repeat(6_000_000))];
        let (block, n) = assemble_messages(huge);
        assert_eq!(n, 1, "the huge message should still be included, capped");
        assert!(
            block.len() < MSG_CAP + 200,
            "one oversized message must be capped, not passed through: {}",
            block.len()
        );
    }

    // resolve_model: per-file override wins, then AMUX_HELPER_MODEL, then the
    // fastest-Claude default. No literal is spelled at the call site.
    #[test]
    fn resolve_model_prefers_override_then_config_then_fast_default() {
        // Serialize on the process-global env var.
        std::env::remove_var("AMUX_HELPER_MODEL");
        assert_eq!(resolve_model(Some("claude-opus-5")), "claude-opus-5");
        assert_eq!(resolve_model(None), DEFAULT_FAST_MODEL);
        std::env::set_var("AMUX_HELPER_MODEL", "claude-sonnet-5");
        assert_eq!(resolve_model(None), "claude-sonnet-5");
        // A per-file override still wins over config.
        assert_eq!(resolve_model(Some("claude-haiku-4-5")), "claude-haiku-4-5");
        std::env::remove_var("AMUX_HELPER_MODEL");
    }

    // Parsing: frontmatter, bare-string sources, per-file model, and a plain
    // markdown file (no frontmatter) all parse.
    #[test]
    fn parse_handles_full_bare_and_no_frontmatter() {
        let doc = parse_mdai(
            "---\nmodel: claude-haiku-4-5\nsources:\n  - path: a.md\n    prompt: p1\n  - b.txt\n---\nbody here",
        )
        .unwrap();
        assert_eq!(doc.model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(doc.sources.len(), 2);
        assert_eq!(doc.sources[0].path, "a.md");
        assert_eq!(doc.sources[0].prompt, "p1");
        assert_eq!(doc.sources[1].path, "b.txt");
        assert_eq!(doc.sources[1].prompt, DEFAULT_EDGE_PROMPT);
        assert_eq!(doc.body, "body here");

        let plain = parse_mdai("just markdown, no frontmatter").unwrap();
        assert!(plain.sources.is_empty());
        assert!(plain.model.is_none());
        assert_eq!(plain.body, "just markdown, no frontmatter");
    }

    // connect_edge writes a well-formed edge into a target's frontmatter, and the
    // result re-parses with the new source and default prompt.
    #[test]
    fn connect_edge_writes_a_parseable_edge() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("AMUX_FILES_ROOT", dir.path());
        write(dir.path(), "target.mdai", "---\nsources: []\n---\nTARGET body");
        write(dir.path(), "src.txt", "some source");

        let out = connect_edge("src.txt", "target.mdai", "custom edge prompt").unwrap();
        assert_eq!(out["ok"], json!(true));

        let text = std::fs::read_to_string(dir.path().join("target.mdai")).unwrap();
        let doc = parse_mdai(&text).unwrap();
        assert_eq!(doc.sources.len(), 1);
        assert_eq!(doc.sources[0].path, "src.txt");
        assert_eq!(doc.sources[0].prompt, "custom edge prompt");
        assert!(doc.body.contains("TARGET body"));

        // A second connect with no prompt fills the sensible default.
        connect_edge("other.txt", "target.mdai", DEFAULT_EDGE_PROMPT).unwrap();
        let doc2 = parse_mdai(&std::fs::read_to_string(dir.path().join("target.mdai")).unwrap()).unwrap();
        assert_eq!(doc2.sources.len(), 2);
        assert_eq!(doc2.sources[1].prompt, DEFAULT_EDGE_PROMPT);

        std::env::remove_var("AMUX_FILES_ROOT");
    }
}
