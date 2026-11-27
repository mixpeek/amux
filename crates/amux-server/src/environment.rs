//! The environment primitive: ONE resolver for a configuration key.
//!
//! Before this module there were two (primitive integrity audit 2026-11-26,
//! P1): `api::settings::effective_env` read `server.env` over the process env
//! for global keys, and `api::session_verbs::scoped_setting_in` read the
//! worker > group > global scope files. About 325 call sites read
//! `std::env::var("AMUX_...")` directly and skipped both, so a key the Scope
//! tab offers at worker or group level was silently ignored wherever a reader
//! went straight to the process env.
//!
//! The pieces:
//! - [`REGISTRY`] declares every key this server reads through here and how it
//!   resolves ([`Precedence`]). A key the registry marks scopable may not be
//!   read raw anywhere outside this module and `config.rs`: the test
//!   `no_raw_read_of_a_scoped_key` fails naming the file and line.
//! - [`resolve`] / [`resolve_in`] answer one key for one [`Scope`], with the
//!   [`Layer`] it came from.
//! - [`process`] is the sanctioned process-env read for a process-only key.
//!
//! The layers, outermost first:
//! - `server.env`: the operator file under `~/.amux`, exported into the
//!   process env at boot (config.rs). [`Scope::Global`] reads the FILE first so
//!   a settings PATCH is visible without a restart; see `effective_env`.
//! - process env: what the server was started with, `server.env` included.
//! - scope files: `~/.amux/sessions/<worker>.env`, then each group's
//!   `~/.amux/env/<group>.env` (groups from the worker's CC_TAGS), then the
//!   fleet-wide `~/.amux/amux.env`.
//!
//! Log signals (two-fix rule): a value taken from a worker or group layer
//! emits a DEBUG `verdict = "env_scoped_override"`, and a key read through
//! here that the registry does not know emits one WARN per key per process,
//! `verdict = "env_key_unregistered"`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::api::session_verbs::EnvFile;

/// Who is asking. A lane is a worker name; an empty lane has no scope files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    Global,
    Lane(&'a str),
}

/// How a key resolves for a lane. Every variant is a ladder that existed in
/// the code before this module; none is new.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precedence {
    /// The documented ladder: a non-empty process env value (the operator
    /// switch in `server.env`) wins, then worker > group > global files.
    ProcessFirst,
    /// Worker > group > global files, then the process env. The board
    /// lifecycle switches read this way, so a worker's own Off beats a
    /// fleet-wide process default.
    ScopeFirst,
    /// Worker > group > global files only. The process env is the SERVER's,
    /// and these keys describe one worker (its CPUs, its isolation).
    ScopeOnly,
    /// Not scopable. Server configuration read from the process env.
    ProcessOnly,
}

impl Precedence {
    pub fn scopable(self) -> bool {
        !matches!(self, Precedence::ProcessOnly)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Precedence::ProcessFirst => "process_first",
            Precedence::ScopeFirst => "scope_first",
            Precedence::ScopeOnly => "scope_only",
            Precedence::ProcessOnly => "process_only",
        }
    }
    /// The layers a lane read consults, in order, as the Scope tab shows them.
    pub fn order(self) -> &'static [&'static str] {
        match self {
            Precedence::ProcessFirst => &["process", "worker", "group", "global"],
            Precedence::ScopeFirst => &["worker", "group", "global", "process"],
            Precedence::ScopeOnly => &["worker", "group", "global"],
            Precedence::ProcessOnly => &["process"],
        }
    }
}

/// One registered key.
#[derive(Debug, Clone, Copy)]
pub struct KeySpec {
    pub key: &'static str,
    pub precedence: Precedence,
    pub doc: &'static str,
}

const fn k(key: &'static str, precedence: Precedence, doc: &'static str) -> KeySpec {
    KeySpec { key, precedence, doc }
}

use Precedence::{ProcessFirst as PF, ProcessOnly as PO, ScopeFirst as SF, ScopeOnly as SO};

/// Every key read through this module. Sorted by key; `registry_is_sorted_and_unique`
/// pins that so a duplicate cannot carry two precedences.
pub const REGISTRY: &[KeySpec] = &[
    k("AMUX_ALLOW_AGENT_SESSION_DELETE", PO, "let an agent caller delete a worker"),
    k("AMUX_APPROVAL_TYPES", PF, "approval types a worker may file needs-you asks for"),
    k("AMUX_AUTO_RESUME", PF, "retry a turn that ended on a transient API error"),
    k("AMUX_BACKEND", PO, "terminal backend (tmux or herdr)"),
    k("AMUX_BACKLOG_REVISIT_DAYS", PF, "default revisit days for a backlog card"),
    k("AMUX_BENCH_JSONL", PO, "file to append send benchmarks to"),
    k("AMUX_BG_VIEW_ACTION", PO, "what to do with a background-task view prompt"),
    k("AMUX_BLOCKED_NEEDS_WATCH", PF, "a blocked card must name what it waits on"),
    k("AMUX_BLOCKER_RECOVERY_LANE_INTERVAL_S", PF, "seconds between blocker-recovery reviews per worker"),
    k("AMUX_BOARD_DEFAULT", SF, "a worker nobody configured keeps its board"),
    k("AMUX_BOARD_DELEGATION", PF, "allow cross-board delegation"),
    k("AMUX_BOARD_FORCE_ADHERENCE", SF, "the board steers this worker"),
    k("AMUX_BOARD_GROOM", SF, "offer this worker board-grooming turns"),
    k("AMUX_BOARD_STEERS", SF, "board steering notes"),
    k("AMUX_BROWSER_DEFAULT_PROFILE", PF, "browser profile for a worker that names none"),
    k("AMUX_CAPTURE_DEDUP_WINDOW_S", PO, "window for prompt-capture dedupe"),
    k("AMUX_CAPTURE_RECONCILE", SF, "reconcile captured prompts against the board"),
    k("AMUX_CHAT_MODEL", SO, "model a chat worker runs"),
    k("AMUX_CLAUDE_CMD", PO, "custom Claude Code launch command"),
    k("AMUX_COMMAND_LIFECYCLE", SF, "file owner prompts as board cards"),
    k("AMUX_COMMIT_GUARD", PO, "commit guard mode"),
    k("AMUX_COMPUTER", PF, "computer-use backend for this worker"),
    k("AMUX_COMPUTER_COLIMA_AUTOSTART", PF, "start Colima for computer use on demand"),
    k("AMUX_CONTEXT_WINDOW", PO, "context window size override"),
    k("AMUX_CONTINUATION_REQUIRED", PF, "a done card must name its continuation"),
    k("AMUX_CONV_CLAIM_STALE_S", PO, "seconds before a conversation claim is stale"),
    k("AMUX_DB", PO, "path to amux.db"),
    k("AMUX_DEPLOY_WAKE", PF, "wake a worker when its commit deploys"),
    k("AMUX_DIRECTIVE_SENDERS", SO, "who may send this worker a directive"),
    k("AMUX_DISPATCH_BACKLOG_WHEN_IDLE", PF, "drain backlog when todo is empty"),
    k("AMUX_DISPATCH_DONE_WIP_MAX", PF, "done cards a worker may hold before new work"),
    k("AMUX_DOCKER_VM", SO, "docker VM this worker runs in"),
    k("AMUX_DONE_EVIDENCE_REQUIRED", PF, "done needs evidence"),
    k("AMUX_DONE_LINK_REQUIRED", PF, "done needs a link"),
    k("AMUX_EMAIL_EXTERNAL_ALLOW", SO, "this worker may email outside the company"),
    k("AMUX_GMAIL_ACCOUNTS", SO, "Gmail accounts this worker may use"),
    k("AMUX_GOAL_CHECK", PF, "the worker's goal checker; creates its goal-check schedule"),
    k("AMUX_GOAL_CHECK_INTERVAL_S", PF, "minutes between goal checker runs, in seconds"),
    k("AMUX_GOAL_CHECK_TIMEOUT_S", PF, "how long one goal-check run may take, in seconds (default 600, at most 4h)"),
    k("AMUX_GOAL_CONTINUE", PF, "goal keeper continues an unmet /goal"),
    k("AMUX_GOAL_LOOP_GUARD", PF, "stop a /goal that loops without progress"),
    k("AMUX_GOAL_STALL_H", PF, "hours before a goal counts as stalled"),
    k("AMUX_HOME", PO, "amux state directory"),
    k("AMUX_INTAKE_CALLS_PER_HOUR", SF, "board intake model calls per hour"),
    k("AMUX_INTAKE_CANDIDATES", SF, "cards board intake compares against"),
    k("AMUX_INTAKE_MODEL", SF, "model board intake uses"),
    k("AMUX_KUBE_IMPERSONATE", SO, "give this worker its own cluster identity"),
    k("AMUX_LOG_FLUSH_MS", PO, "pane log flush interval"),
    k("AMUX_LOG_MAX_MB", PO, "pane log size cap"),
    k("AMUX_LOG_RAW", PO, "keep raw pane logs"),
    k("AMUX_MAC_ANY_APP", PF, "Mac computer use may drive any app"),
    k("AMUX_MAC_APPS", PF, "apps Mac computer use may drive"),
    k("AMUX_MAC_FOREGROUND", PF, "Mac computer use may take the foreground"),
    k("AMUX_MAC_OUTSIDE_UNTIL", PF, "Mac computer use may leave its apps until this time"),
    k("AMUX_MAX_DOING_PER_SESSION", PF, "cards a worker may hold in doing"),
    k("AMUX_MEMORY_DREAM", PF, "ask this worker to curate a large memory"),
    k("AMUX_MEMORY_DREAM_KB", PO, "memory size that triggers a curate ask"),
    k("AMUX_MEMORY_INHERIT_FILES", PO, "memory files a new worker inherits"),
    k("AMUX_NATIVE_ARCH", PO, "run the native-arch build"),
    k("AMUX_NEEDSYOU_ASK_REQUIRED", PF, "a needs-you card must carry its question"),
    k("AMUX_NEEDSYOU_REVISIT_DAYS", PF, "default revisit days for a needs-you card"),
    k("AMUX_NEW_CONVERSATION_PER_TASK", SO, "fresh conversation per board task"),
    k("AMUX_OWNER_ASK_STEER", PF, "turn-end reminder for an unanswered owner ask"),
    k("AMUX_PANE_PROMPT_HISTORY", PF, "record pane prompts"),
    k("AMUX_PICKUP_DELEGATED_ONLY", SO, "pick up only delegated cards"),
    k("AMUX_PLAN_STALE_HIDE_HOURS", PO, "hours before a stale plan is hidden"),
    k("AMUX_PROBE_GUARD", PF, "guard sends with a liveness probe"),
    k("AMUX_PROMISE_NUDGE", PF, "nudge a worker that promised and stopped"),
    k("AMUX_PUSH_STRATEGY", SO, "how this worker lands commits (read by the worker shell)"),
    k("AMUX_RATE_LIMIT_ACTION", PO, "answer to the rate-limit prompt"),
    k("AMUX_RATE_LIMIT_SWEEP_S", PO, "rate-limit sweep interval"),
    k("AMUX_REPEAT_ASK_NOTE", PF, "note a repeated owner ask"),
    k("AMUX_RESUME_MODE_ACTION", PO, "answer to the resume-mode prompt"),
    k("AMUX_RS_LEGACY_PORT", PO, "legacy port compatibility bind"),
    k("AMUX_RS_ORCH", PO, "1 runs the retiring orchestrator tick loop (off by default)"),
    k("AMUX_SEND_QUEUE_FOR_PAUSED", PF, "queue sends to a paused worker"),
    k("AMUX_SEND_RESERVATION_EXPIRE_S", PO, "send reservation expiry"),
    k("AMUX_SIGNAL_WAKE", PF, "wake a worker on a signal"),
    k("AMUX_STAGE_OWNER_COMMANDS", SF, "stage owner commands before delivery"),
    k("AMUX_STANDING_APPROVALS", PF, "standing approvals apply"),
    k("AMUX_STEER_DEAD_LETTER_S", PO, "seconds before a steer is dead-lettered"),
    k("AMUX_STEER_MAX_AGE_S", PO, "max age of a queued steer"),
    k("AMUX_STEER_NO_SIGNAL_MAX_AGE_S", PO, "max age of a steer with no signal"),
    k("AMUX_SUBAGENT_SILENT_S", PO, "seconds before a silent subagent is flagged"),
    k("AMUX_TEMPLATES_DIR", PO, "worker templates directory"),
    k("AMUX_TMUX_COLS", PO, "tmux pane columns"),
    k("AMUX_TMUX_ROWS", PO, "tmux pane rows"),
    k("AMUX_TODO_WIP_LIMIT", PF, "todo cards a worker may hold"),
    k("AMUX_TRANSCRIPT_EVIDENCE_TTL_S", PO, "transcript evidence cache TTL"),
    k("AMUX_WORKER_CPUS", SO, "CPUs this worker may use"),
    k("AMUX_WORKER_DISK_GB", SO, "disk this worker may use"),
    k("AMUX_WORKER_GIT_CREDENTIALS", PO, "give workers git credentials"),
    k("AMUX_WORKER_MEM_GB", SO, "memory this worker may use"),
    k("AMUX_WORKSPACE_ISOLATION", SO, "own git worktree and TMPDIR for this worker"),
    k("CC_AUTO_CONTINUE", SO, "legacy board continue switch"),
    k("CC_AUTO_PICKUP", SO, "pick up todo cards"),
    k("CC_BOARD_CONTINUE", SO, "continue to the next board card"),
    k("CC_PROBE_TARGET", SO, "this worker is a send-probe target"),
    k("CC_RECEIVE_DENY", SO, "senders this worker refuses"),
    k("CC_STANDING_ORDERS", SO, "standing orders apply"),
    k("GIT_AUTHOR_NAME", SO, "commit author for this worker"),
];

/// The registry entry for `key`.
pub fn spec(key: &str) -> Option<&'static KeySpec> {
    REGISTRY
        .binary_search_by(|s| s.key.cmp(key))
        .ok()
        .map(|i| &REGISTRY[i])
}

/// The registry as JSON rows, for the Scope tab (GET /api/scope, env capability).
pub fn registry_json() -> serde_json::Value {
    serde_json::Value::Array(
        REGISTRY
            .iter()
            .map(|s| {
                serde_json::json!({
                    "key": s.key,
                    "scopable": s.precedence.scopable(),
                    "precedence": s.precedence.as_str(),
                    "order": s.precedence.order(),
                    "doc": s.doc,
                })
            })
            .collect(),
    )
}

/// One WARN per unregistered key per process. A miss on a hot path would
/// otherwise log every tick and bury the signal.
fn note_unregistered(key: &str, via: &'static str) {
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let first = SEEN
        .get_or_init(|| Mutex::new(BTreeSet::new()))
        .lock()
        .map(|mut s| s.insert(key.to_string()))
        .unwrap_or(false);
    if first {
        tracing::warn!(
            key,
            via,
            verdict = "env_key_unregistered",
            measured = true,
            n_considered = REGISTRY.len(),
            "environment: key read through the resolver is not in environment::REGISTRY; \
             add it with its precedence so the Scope tab can show it"
        );
    }
}

fn lookup(key: &str, via: &'static str) -> Option<&'static KeySpec> {
    let s = spec(key);
    if s.is_none() {
        note_unregistered(key, via);
    }
    s
}

/// Where a resolved value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layer {
    /// `~/.amux/server.env`, read from the file ([`Scope::Global`] only).
    ServerEnv,
    /// The server's process env.
    Process,
    /// `~/.amux/sessions/<worker>.env`.
    Worker,
    /// `~/.amux/env/<group>.env`.
    Group(String),
    /// `~/.amux/amux.env`.
    Global,
}

impl Layer {
    pub fn is_lane_layer(&self) -> bool {
        matches!(self, Layer::Worker | Layer::Group(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub value: String,
    pub layer: Layer,
}

/// The process env value of `key`, as `std::env::var(key).ok()` returns it
/// (empty strings included). The one sanctioned raw read: callers that need
/// the process value as an input (the `_in` test seams) take it from here.
pub fn process(key: &str) -> Option<String> {
    lookup(key, "process");
    std::env::var(key).ok()
}

fn nonempty_trimmed(v: &str) -> Option<String> {
    let t = v.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// The groups a worker belongs to, from CC_TAGS in its own env file.
fn lane_groups(worker: &EnvFile) -> BTreeSet<String> {
    worker
        .get("CC_TAGS")
        .map(|v| {
            v.split(',')
                .map(|t| t.trim().trim_matches('"').to_lowercase())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Worker, then group layers. Values are trimmed; empty means unset.
pub fn lane_layers_in(home: &Path, lane: &str, key: &str) -> Option<Resolved> {
    lookup(key, "lane_layers");
    let r = lane_layers(home, lane, key);
    if let Some(r) = &r {
        note_layer(key, lane, r);
    }
    r
}

fn lane_layers(home: &Path, lane: &str, key: &str) -> Option<Resolved> {
    let worker = EnvFile::load(&home.join("sessions").join(format!("{lane}.env")));
    if let Some(v) = worker.get(key).and_then(nonempty_trimmed) {
        return Some(Resolved { value: v, layer: Layer::Worker });
    }
    for g in lane_groups(&worker) {
        let path = home.join("env").join(format!("{g}.env"));
        if let Some(v) = EnvFile::load(&path).get(key).and_then(nonempty_trimmed) {
            return Some(Resolved { value: v, layer: Layer::Group(g) });
        }
    }
    None
}

/// Worker > group > global (`amux.env`) scope files. The body of the old
/// `session_verbs::scoped_setting_in`, unchanged.
pub fn scope_files_in(home: &Path, lane: &str, key: &str) -> Option<Resolved> {
    lookup(key, "scope_files");
    let r = lane_layers(home, lane, key).or_else(|| {
        EnvFile::load(&home.join("amux.env"))
            .get(key)
            .and_then(nonempty_trimmed)
            .map(|value| Resolved { value, layer: Layer::Global })
    });
    if let Some(r) = &r {
        note_layer(key, lane, r);
    }
    r
}

fn note_layer(key: &str, lane: &str, r: &Resolved) {
    if r.layer.is_lane_layer() {
        let layer = match &r.layer {
            Layer::Group(g) => format!("group:{g}"),
            _ => "worker".to_string(),
        };
        tracing::debug!(
            key,
            lane,
            layer = %layer,
            verdict = "env_scoped_override",
            measured = true,
            n_considered = 1,
            "environment: scoped key resolved from a worker or group layer"
        );
    }
}

/// The env files that make up a lane's scope, IN SOURCE ORDER: global, then
/// each group, then the worker. Only files that exist. Source order is the
/// precedence (the last assignment wins), so this delivers into a lane's shell
/// exactly what [`scope_files_in`] answers for one key.
pub fn scope_env_layers(home: &Path, lane: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let global = home.join("amux.env");
    if global.exists() {
        out.push(global);
    }
    let worker_file = home.join("sessions").join(format!("{lane}.env"));
    for g in lane_groups(&EnvFile::load(&worker_file)) {
        let gp = home.join("env").join(format!("{g}.env"));
        if gp.exists() {
            out.push(gp);
        }
    }
    if worker_file.exists() {
        out.push(worker_file);
    }
    out
}

/// Resolve `key` for `scope` over an explicit home and process value.
///
/// - [`Scope::Global`]: a `server.env` entry first (present but empty means
///   CLEARED and answers None; see `effective_env`), then a non-empty process
///   value. Any key; the registry is not consulted, because the settings API
///   serves arbitrary server.env keys through this.
/// - [`Scope::Lane`]: the key's registered [`Precedence`]. An unregistered key
///   resolves [`Precedence::ProcessFirst`], the documented ladder, and logs
///   `env_key_unregistered` once. An empty lane has no scope files.
pub fn resolve_in(home: &Path, scope: Scope<'_>, key: &str, process_value: Option<&str>) -> Option<Resolved> {
    let from_process = || {
        process_value
            .filter(|v| !v.trim().is_empty())
            .map(|v| Resolved { value: v.to_string(), layer: Layer::Process })
    };
    let lane = match scope {
        Scope::Global => {
            let file_env = crate::config::parse_env_file(&home.join("server.env"));
            if let Some(v) = file_env.get(key) {
                return (!v.is_empty()).then(|| Resolved { value: v.clone(), layer: Layer::ServerEnv });
            }
            return from_process();
        }
        Scope::Lane(l) => l,
    };
    let precedence = lookup(key, "resolve").map(|s| s.precedence).unwrap_or(Precedence::ProcessFirst);
    let files = || (!lane.is_empty()).then(|| scope_files_in(home, lane, key)).flatten();
    match precedence {
        Precedence::ProcessFirst => from_process().or_else(files),
        Precedence::ScopeFirst => files().or_else(from_process),
        Precedence::ScopeOnly => files(),
        Precedence::ProcessOnly => from_process(),
    }
}

/// [`resolve_in`] over the live home and process env.
pub fn resolve(scope: Scope<'_>, key: &str) -> Option<String> {
    let home = match scope {
        Scope::Global => crate::config::amux_home(),
        Scope::Lane(_) => crate::api::session_verbs::home(),
    };
    resolve_in(&home, scope, key, std::env::var(key).ok().as_deref()).map(|r| r.value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn write(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn fleet() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        write(&h.join("amux.env"), "AMUX_DISPATCH_BACKLOG_WHEN_IDLE=g\nAMUX_BOARD_GROOM=g\nAMUX_WORKER_CPUS=g\n");
        write(&h.join("env/alpha.env"), "AMUX_DISPATCH_BACKLOG_WHEN_IDLE=grp\n");
        write(&h.join("sessions/w.env"), "CC_TAGS=Alpha\nAMUX_BOARD_GROOM=w\n");
        home
    }

    #[test]
    fn registry_is_sorted_and_unique() {
        for pair in REGISTRY.windows(2) {
            assert!(pair[0].key < pair[1].key, "REGISTRY out of order or duplicated at {}", pair[1].key);
        }
    }

    #[test]
    fn lane_precedence_follows_the_registry() {
        let home = fleet();
        let h = home.path();
        // ProcessFirst: a process value wins over every file; without it the group layer answers.
        let r = resolve_in(h, Scope::Lane("w"), "AMUX_DISPATCH_BACKLOG_WHEN_IDLE", Some("p")).unwrap();
        assert_eq!((r.value.as_str(), r.layer), ("p", Layer::Process));
        let r = resolve_in(h, Scope::Lane("w"), "AMUX_DISPATCH_BACKLOG_WHEN_IDLE", Some("  ")).unwrap();
        assert_eq!((r.value.as_str(), r.layer), ("grp", Layer::Group("alpha".into())));
        // ScopeFirst: the worker file beats the process value.
        let r = resolve_in(h, Scope::Lane("w"), "AMUX_BOARD_GROOM", Some("p")).unwrap();
        assert_eq!((r.value.as_str(), r.layer), ("w", Layer::Worker));
        // ScopeOnly: the process value is never consulted.
        let r = resolve_in(h, Scope::Lane("w"), "AMUX_WORKER_CPUS", Some("p")).unwrap();
        assert_eq!((r.value.as_str(), r.layer), ("g", Layer::Global));
        assert_eq!(resolve_in(h, Scope::Lane("nobody"), "AMUX_WORKSPACE_ISOLATION", Some("1")), None);
        // ProcessOnly ignores scope files.
        assert_eq!(resolve_in(h, Scope::Lane("w"), "AMUX_TMUX_COLS", None), None);
        // An empty lane has no scope files.
        assert_eq!(resolve_in(h, Scope::Lane(""), "AMUX_BOARD_GROOM", None), None);
    }

    #[test]
    fn global_reads_server_env_first_and_empty_means_cleared() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        assert_eq!(resolve_in(h, Scope::Global, "X_KEY", Some("proc")).unwrap().layer, Layer::Process);
        write(&h.join("server.env"), "X_KEY=file\nY_KEY=\n");
        let r = resolve_in(h, Scope::Global, "X_KEY", Some("proc")).unwrap();
        assert_eq!((r.value.as_str(), r.layer), ("file", Layer::ServerEnv));
        assert_eq!(resolve_in(h, Scope::Global, "Y_KEY", Some("proc")), None);
    }

    #[test]
    fn scope_env_layers_are_in_source_order() {
        let home = fleet();
        let h = home.path();
        let got = scope_env_layers(h, "w");
        assert_eq!(got, vec![h.join("amux.env"), h.join("env/alpha.env"), h.join("sessions/w.env")]);
    }

    /// Files allowed to read the process env directly: this module, and
    /// boot-time process config in config.rs (audit P1 exception).
    const RAW_READ_ALLOWED: &[&str] = &["environment.rs", "config.rs"];
    /// A line carrying this marker is exempt, for test code that saves and
    /// restores a process value around `set_var`.
    const RAW_READ_MARKER: &str = "env-raw-ok:";

    /// Every `env::var("KEY")` / `env::var(CONST)` read in the server's source
    /// whose key the registry marks scopable, as `file:line key`.
    fn raw_scoped_reads(src: &Path) -> Vec<String> {
        let read = regex::Regex::new(r#"env::var(?:_os)?\(\s*(?:"([A-Z][A-Z0-9_]*)"|(?:[a-z_]+::)*([A-Z][A-Z0-9_]*)\s*\))"#).unwrap();
        let konst = regex::Regex::new(r#"const\s+([A-Z][A-Z0-9_]*)\s*:\s*&(?:'static\s+)?str\s*=\s*"([A-Z][A-Z0-9_]*)""#).unwrap();
        let mut files = vec![];
        let mut stack = vec![src.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    files.push(p);
                }
            }
        }
        files.sort();
        // Constants resolve by name across the crate: a reader may import one
        // from another module (`board_drive::DISPATCH_BACKLOG_KEY`).
        let mut consts: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let texts: Vec<(PathBuf, String)> = files
            .into_iter()
            .map(|p| {
                let t = std::fs::read_to_string(&p).unwrap();
                (p, t)
            })
            .collect();
        for (_, t) in &texts {
            for c in konst.captures_iter(t) {
                consts.entry(c[1].to_string()).or_default().insert(c[2].to_string());
            }
        }
        let mut out = vec![];
        for (p, t) in &texts {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            if RAW_READ_ALLOWED.contains(&name.as_str()) {
                continue;
            }
            for (i, line) in t.lines().enumerate() {
                if line.contains(RAW_READ_MARKER) || line.trim_start().starts_with("//") {
                    continue;
                }
                for c in read.captures_iter(line) {
                    let keys: Vec<String> = match (c.get(1), c.get(2)) {
                        (Some(lit), _) => vec![lit.as_str().to_string()],
                        (None, Some(cn)) => consts.get(cn.as_str()).map(|s| s.iter().cloned().collect()).unwrap_or_default(),
                        _ => vec![],
                    };
                    for key in keys {
                        if spec(&key).is_some_and(|s| s.precedence.scopable()) {
                            let rel = p.strip_prefix(src).unwrap_or(p);
                            out.push(format!("{}:{} {key}", rel.display(), i + 1));
                        }
                    }
                }
            }
        }
        out
    }

    #[test]
    fn no_raw_read_of_a_scoped_key() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let found = raw_scoped_reads(&src);
        assert!(
            found.is_empty(),
            "{} raw std::env::var read(s) of a key environment::REGISTRY marks scopable. \
             A raw read ignores the worker/group/global scope files the Scope tab writes. \
             Use environment::resolve(Scope::Lane(..), KEY), or environment::process(KEY) \
             for a process value an `_in` seam takes:\n{}",
            found.len(),
            found.join("\n")
        );
    }

    /// The scanner must be able to see a read, or the test above is green by blindness.
    #[test]
    fn raw_read_scanner_sees_literal_and_const_reads() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("a.rs"),
            "const K: &str = \"AMUX_WORKSPACE_ISOLATION\";\nfn f() { let _ = std::env::var(K); }\n\
             fn g() { let _ = std::env::var(\"AMUX_BOARD_GROOM\"); }\n\
             fn h() { let _ = std::env::var(\"AMUX_TMUX_COLS\"); }\n\
             fn i() { let _ = std::env::var(\"AMUX_BOARD_GROOM\"); } // env-raw-ok: test restore\n",
        );
        let got = raw_scoped_reads(dir.path());
        assert_eq!(got, vec!["a.rs:2 AMUX_WORKSPACE_ISOLATION".to_string(), "a.rs:3 AMUX_BOARD_GROOM".to_string()]);
    }
}
