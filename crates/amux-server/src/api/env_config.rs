//! /api/env — declarative environment config (AMUX-2977, Ethan's centerpiece).
//!
//! One YAML that CREATES an amux environment by configuring the existing
//! primitives — this is a loader OVER the primitives (groups, workers,
//! schedulers, board, files), NOT a new primitive, which is the whole ethos:
//! "model your organization as configuration of the eight primitives."
//!
//!   POST /api/env/apply            apply the YAML (idempotent)
//!   POST /api/env/apply?dry_run=1  report what WOULD change, write nothing
//!   GET  /api/env/schema           the accepted shape, as docs
//!
//! Body is YAML (Content-Type text/yaml or application/x-yaml) OR JSON — both
//! parse to the same shape. Idempotent by IDENTITY: a group is its name, a
//! worker is its env file, so applying twice converges instead of duplicating.
//!
//! PHASE 1 (this) covers the org STRUCTURE — `groups` and `workers` — the two
//! primitives everything else hangs off. `schedules`, board `columns` + `gates`,
//! seed `files`, and `global` env are PHASE 2, each an additive stanza the
//! report already accounts for as "not-yet-applied" so nothing is silently
//! dropped. See AMUX-2977.

use super::AppState;
use crate::db::WriteOutcome;
use rusqlite::OptionalExtension;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/apply", post(apply))
        .route("/schema", get(schema))
}

// ---- the accepted shape ----------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct EnvSpec {
    #[serde(default)]
    groups: Vec<GroupSpec>,
    #[serde(default)]
    workers: Vec<WorkerSpec>,
    // `files` (seeded docs) is APPLIED (AMUX-2977 phase 2): the seeded docs ARE
    // a vertical's content, so an env without them has workers and nothing to
    // work on (amux-cloud's highest-value gap).
    #[serde(default)]
    files: Vec<FileSpec>,
    // `schedules` is APPLIED (AMUX-2977 phase 2): one per (worker,title),
    // enabled:false honored HARD (never auto-run — Ethan's rule), created via
    // the SAME path a hand-made schedule uses so `expr`/next_run match exactly.
    #[serde(default)]
    schedules: Vec<ScheduleSpec>,
    // `cards` (initial board issues — the demo's visible content) is APPLIED
    // (AMUX-2977, amux-cloud co-design): a vertical's starting work belongs in
    // the YAML so the whole env round-trips. Idempotent by (worker,title).
    #[serde(default)]
    cards: Vec<CardSpec>,
    // `messages` (seeded inbox — a vertical's kickoff/coordination messages) is
    // APPLIED (AC-352, Ethan's "entirely via YAML"): resolved + delivered through
    // the SAME create-message path POST /api/messages uses (ethos D6), so group
    // fan-out and AtTurnBoundary delivery match exactly. Was silently dropped.
    #[serde(default)]
    messages: Vec<MessageSpec>,
    // Still phase-2, parsed-and-reported so a full spec is not rejected.
    #[serde(default)]
    columns: Vec<Value>,
    #[serde(default)]
    global: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct CardSpec {
    /// Owning session (the worker this card is on).
    worker: String,
    title: String,
    #[serde(default)]
    desc: String,
    /// Board status; defaults to `backlog` (a seeded card is not auto-dispatched
    /// until a human/worker moves it).
    #[serde(default)]
    status: String,
    /// Item type; defaults to `code`.
    #[serde(rename = "type", default)]
    item_type: String,
    /// Optional epic (semantic id of a type=epic card) to roll this card under.
    #[serde(default)]
    epic: String,
}

#[derive(Debug, Deserialize)]
struct MessageSpec {
    /// Recipient: a worker name/id, a group name/id, or the literal "human".
    to: String,
    /// Sender: a worker name/id, or "human"/"owner". Defaults to the owner — an
    /// env is applied by the owner, so an unspecified sender is honestly them.
    #[serde(default)]
    from: String,
    text: String,
}

#[derive(Debug, Deserialize)]
struct ScheduleSpec {
    /// The session the schedule fires a command into.
    worker: String,
    title: String,
    /// Human schedule string (e.g. "every weekday at 07:00"), re-parsed by the
    /// real scheduler — NOT a cron literal (matches the exporter's `expr`).
    #[serde(default)]
    expr: String,
    /// Default FALSE: an applied env never auto-runs; a human arms it.
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    command: String,
}

#[derive(Debug, Deserialize)]
struct FileSpec {
    /// Absolute destination path (a container path on the cloud, a real path
    /// locally). Relative paths are refused — the destination must be explicit.
    path: String,
    #[serde(default)]
    content: String,
}

#[derive(Debug, Deserialize)]
struct GroupSpec {
    name: String,
    #[serde(default)]
    department: String,
    #[serde(default)]
    goal: String,
}

#[derive(Debug, Deserialize, Default)]
struct WorkerSpec {
    name: String,
    #[serde(default)]
    dir: String,
    /// Group names -> CC_TAGS (comma-joined). amux's group membership.
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    desc: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    provider: String,
    /// First-run task steered to the worker ONLY when it is newly created — a
    /// re-apply of an existing worker never re-steers (AMUX-2977 co-design).
    #[serde(default)]
    prompt: String,
}

// ---- GET /api/env/schema ---------------------------------------------------

async fn schema() -> Response {
    Json(json!({
        "applied_now": ["groups", "workers", "files", "schedules", "cards", "messages", "worker.prompt"],
        "phase_2": ["columns", "gates", "global"],
        "files_shape": [{"path": "/abs/path/doc.md", "content": "literal file content"}],
        "schedules_shape": [{"worker": "backend-dev", "title": "nightly", "expr": "daily at 02:00", "enabled": false, "command": "the prompt"}],
        "cards_shape": [{"worker": "backend-dev", "title": "First issue", "desc": "...", "status": "backlog", "type": "code", "epic": ""}],
        "messages_shape": [{"to": "backend-dev (worker name/id, group, or \"human\")", "from": "(worker or \"human\"/\"owner\", default owner)", "text": "the message body"}],
        "worker_prompt": "a `prompt` string on each worker — steered as the first task, on create only",
        "example": {
            "groups": [{"name": "engineering", "department": "Engineering", "goal": "Ship the platform"}],
            "workers": [{
                "name": "backend-dev", "dir": "/path/to/repo", "groups": ["engineering"],
                "desc": "Backend API work", "model": "sonnet", "provider": "claude"
            }]
        },
        "idempotent": "a group is its name, a worker is its env file — re-applying converges, never duplicates",
        "content_type": "text/yaml | application/x-yaml | application/json",
    }))
    .into_response()
}

// ---- POST /api/env/apply ---------------------------------------------------

#[derive(Deserialize)]
struct ApplyQ {
    #[serde(default)]
    dry_run: u8,
}

async fn apply(
    State(state): State<AppState>,
    Query(q): Query<ApplyQ>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let dry = q.dry_run != 0;
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let raw = String::from_utf8_lossy(&body);
    // JSON is a subset of YAML for serde_yaml, but honor an explicit JSON type.
    let spec: EnvSpec = if ct.contains("json") {
        match serde_json::from_str(&raw) {
            Ok(s) => s,
            Err(e) => return bad(format!("invalid JSON: {e}")),
        }
    } else {
        match serde_yaml::from_str(&raw) {
            Ok(s) => s,
            Err(e) => return bad(format!("invalid YAML: {e}")),
        }
    };

    let home = crate::config::amux_home();
    let sessions_dir = home.join("sessions");

    let mut report = Vec::<Value>::new();

    // ---- workers: an env file per worker (idempotent write) ----------------
    // Validate first so a dry-run reports the same refusals an apply would hit.
    let mut worker_writes: Vec<(std::path::PathBuf, String, String, &str, String)> = vec![];
    // (name, prompt) for workers created THIS apply — steered once, on create.
    let mut worker_prompts: Vec<(String, String)> = vec![];
    for w in &spec.workers {
        let name = sanitize(&w.name);
        if name.is_empty() {
            report.push(json!({"kind": "worker", "name": w.name, "action": "error", "detail": "invalid name"}));
            continue;
        }
        // NOTE: the worker's `dir` is NOT required to pre-exist. Applying an env
        // is a BOOTSTRAP — "redeploy this vertical from a YAML" — so the workdir
        // is CREATED on apply (below), not demanded. The old is_dir() error ran
        // BEFORE the files loop that seeds docs UNDER that dir, so a single apply
        // skipped every worker and a second apply created them (amux-cloud, cloud
        // round-trip). Only a create FAILURE at apply time is an error now.
        let path = sessions_dir.join(format!("{name}.env"));
        let existed = path.exists();
        let action = if existed { "update" } else { "create" };
        let content = render_worker_env(w);
        // "unchanged" if the file already holds this exact config (minus the
        // volatile `# updated:` header line) — so a re-apply reports honestly.
        let action = if existed && same_env_body(&path, &content) { "unchanged" } else { action };
        report.push(json!({"kind": "worker", "name": name, "action": action, "groups": w.groups,
            "prompt": !w.prompt.trim().is_empty(), "dir": w.dir}));
        if !dry && action != "unchanged" {
            worker_writes.push((path, content, name.clone(), action, w.dir.clone()));
        }
        // Steer the first-run prompt ONLY when the worker is newly created —
        // never on update/unchanged, or a re-apply would re-interrupt a running
        // lane. Reported (bool) in dry-run so the plan is visible.
        if !dry && action == "create" && !w.prompt.trim().is_empty() {
            worker_prompts.push((name.clone(), w.prompt.clone()));
        }
    }

    // ---- groups: group_config upsert ---------------------------------------
    let groups_for_write: Vec<(String, String, String)> = spec
        .groups
        .iter()
        .map(|g| (g.name.trim().to_string(), g.department.clone(), g.goal.clone()))
        .filter(|(n, _, _)| !n.is_empty())
        .collect();
    // For the report, read current group_config so we can say create/update/unchanged.
    let existing_groups: std::collections::HashMap<String, (String, String)> = state
        .store
        .read()
        .ok()
        .map(|conn| {
            let mut m = std::collections::HashMap::new();
            if let Ok(mut st) = conn.prepare("SELECT name, department, goal FROM group_config") {
                if let Ok(rows) = st.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, (r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
                }) {
                    for row in rows.flatten() {
                        m.insert(row.0, row.1);
                    }
                }
            }
            m
        })
        .unwrap_or_default();
    for (name, dept, goal) in &groups_for_write {
        let action = match existing_groups.get(name) {
            Some((d, g)) if d == dept && g == goal => "unchanged",
            Some(_) => "update",
            None => "create",
        };
        report.push(json!({"kind": "group", "name": name, "action": action}));
    }

    // ---- files: seed docs (idempotent write to an absolute path) -----------
    // Validated up front so a dry-run reports the same create/update/unchanged
    // an apply would produce (the shape amux-cloud's accounting.yaml round-trips).
    let mut file_writes: Vec<(std::path::PathBuf, String)> = vec![];
    for f in &spec.files {
        let p = f.path.trim();
        if p.is_empty() || !std::path::Path::new(p).is_absolute() {
            report.push(json!({"kind": "file", "path": f.path, "action": "error",
                "detail": "path must be absolute"}));
            continue;
        }
        let path = std::path::PathBuf::from(p);
        let action = match std::fs::read_to_string(&path) {
            Ok(existing) if existing == f.content => "unchanged",
            Ok(_) => "update",
            Err(_) => "create",
        };
        report.push(json!({"kind": "file", "path": p, "action": action, "bytes": f.content.len()}));
        if !dry && action != "unchanged" {
            file_writes.push((path, f.content.clone()));
        }
    }

    // ---- schedules: one per (worker,title), idempotent by skip-if-exists ---
    // Existing (session,title) pairs, read once for the report. Re-apply of the
    // same spec converges (no duplicate schedules); editing an existing one's
    // expr is a follow-up (would need update-in-place, not v1).
    let existing_scheds: std::collections::HashSet<(String, String)> = state
        .store
        .read()
        .ok()
        .map(|conn| {
            crate::runtime_jobs::scheduler::list_schedules(&conn, None)
                .unwrap_or_default()
                .iter()
                .map(|s| (s.str_field("session").to_string(), s.str_field("title").to_string()))
                .collect()
        })
        .unwrap_or_default();
    let mut sched_writes: Vec<(String, String, String, bool, String)> = vec![];
    for s in &spec.schedules {
        let (worker, title) = (s.worker.trim(), s.title.trim());
        if worker.is_empty() || title.is_empty() {
            report.push(json!({"kind": "schedule", "action": "error", "detail": "worker and title are required"}));
            continue;
        }
        // Validate the expr up front so a dry-run refuses a bad one (the same
        // grammar the /api/schedules create path enforces).
        let expr = s.expr.trim();
        if !expr.is_empty() {
            if let Err(e) = crate::runtime_jobs::scheduler::ScheduleExpr::parse(expr) {
                report.push(json!({"kind": "schedule", "title": title, "action": "error",
                    "detail": format!("unparseable expr: {e}")}));
                continue;
            }
        }
        let exists = existing_scheds.contains(&(worker.to_string(), title.to_string()));
        let action = if exists { "exists" } else { "create" };
        report.push(json!({"kind": "schedule", "worker": worker, "title": title,
            "action": action, "enabled": s.enabled}));
        if !dry && !exists {
            sched_writes.push((worker.to_string(), title.to_string(), expr.to_string(), s.enabled, s.command.clone()));
        }
    }

    // ---- cards: initial board issues, idempotent by (worker,title) ---------
    let existing_cards: std::collections::HashSet<(String, String)> = state
        .store
        .read()
        .ok()
        .map(|conn| {
            let mut set = std::collections::HashSet::new();
            if let Ok(mut st) = conn.prepare(
                "SELECT session, title FROM issues WHERE deleted IS NULL AND COALESCE(archived,0)=0",
            ) {
                if let Ok(rows) = st.query_map([], |r| {
                    Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get::<_, String>(1)?))
                }) {
                    for row in rows.flatten() {
                        set.insert(row);
                    }
                }
            }
            set
        })
        .unwrap_or_default();
    let mut card_writes: Vec<(String, String, String, String, String, String)> = vec![];
    for c in &spec.cards {
        let (worker, title) = (c.worker.trim(), c.title.trim());
        if worker.is_empty() || title.is_empty() {
            report.push(json!({"kind": "card", "action": "error", "detail": "worker and title are required"}));
            continue;
        }
        let exists = existing_cards.contains(&(worker.to_string(), title.to_string()));
        let action = if exists { "exists" } else { "create" };
        report.push(json!({"kind": "card", "worker": worker, "title": title, "action": action}));
        if !dry && !exists {
            let status = if c.status.trim().is_empty() { "backlog".into() } else { c.status.trim().to_string() };
            let itype = if c.item_type.trim().is_empty() { "code".into() } else { c.item_type.trim().to_string() };
            card_writes.push((worker.to_string(), title.to_string(), c.desc.clone(), status, itype, c.epic.trim().to_string()));
        }
    }

    // ---- messages: seeded inbox — reported here, delivered in the write phase
    // through the SAME create-message path (ethos D6). Idempotent by
    // (recipient, text), so a re-applied env does not re-send.
    //
    // Recipients are RESOLVED up-front (read pass) so the report is HONEST — a
    // message the durable worker registry (`_amux_workers`) can't resolve is
    // reported "skipped", never a phantom "create" (ethos rule 4: a dropped
    // stanza must not look like it worked). This is the worker-model duality
    // (D6): env-apply writes `.env` workers, and a `.env` worker becomes
    // message-addressable only once STARTED and registered in `_amux_workers`.
    // So a kickoff to a not-yet-started worker is surfaced here — the write
    // phase's `continue` on the same None is now visible, not silent. "human",
    // a group, or an already-running worker resolve immediately.
    let msg_resolves: Vec<bool> = state
        .store
        .read()
        .ok()
        .map(|conn| {
            spec.messages
                .iter()
                .map(|m| {
                    let to = m.to.trim();
                    !to.is_empty()
                        && matches!(super::messages::resolve_recipient(&conn, to), Ok(Some(_)))
                })
                .collect()
        })
        .unwrap_or_else(|| vec![false; spec.messages.len()]);
    // Workers THIS apply is creating (sanitized names). A message skipped
    // because its recipient is one of these is not a mystery — it is the
    // worker-model duality (D6, AC-353): the worker exists as a `.env` but is
    // not started, so it is not in `_amux_workers` yet. The actionable answer is
    // NOT to change resolution, it is a DIFFERENT primitive: a new worker's
    // kickoff task is its `prompt:` field (steered on create, delivered when the
    // lane starts — see worker_prompts above), while the `messages` stanza seeds
    // an inbox for recipients that exist NOW ("human", a group, a running lane).
    // So the skip redirects the author to the tool that actually does what they
    // meant, rather than reporting a bare "not found".
    let spec_worker_names: std::collections::HashSet<String> = spec
        .workers
        .iter()
        .map(|w| sanitize(&w.name))
        .filter(|n| !n.is_empty())
        .collect();
    for (m, resolves) in spec.messages.iter().zip(&msg_resolves) {
        let (to, text) = (m.to.trim(), m.text.trim());
        if to.is_empty() || text.is_empty() {
            report.push(json!({"kind": "message", "action": "error", "detail": "to and text are required"}));
            continue;
        }
        if *resolves {
            report.push(json!({"kind": "message", "to": to, "action": "create"}));
        } else {
            let own_new_worker = spec_worker_names.contains(&sanitize(to));
            let detail = if own_new_worker {
                "recipient is a worker THIS env creates but has not started, so it is not message-addressable yet (worker-model duality D6). For a new worker's kickoff task, set its `prompt:` field — it is steered on create and delivered when the lane starts. The `messages` stanza seeds inboxes for recipients that exist now (\"human\", a group, or a running worker)."
            } else {
                "recipient not found in worker registry — a .env worker is message-addressable only once started; \"human\", a group, or a running worker resolve"
            };
            report.push(json!({"kind": "message", "to": to, "action": "skipped", "detail": detail,
                "fix": if own_new_worker { "use worker.prompt" } else { "check recipient" }}));
            // Log signal (every-fix-needs-a-log-signal): the NEXT dropped
            // env-apply message self-announces in server-rs.log, so a sweep
            // catches the worker-model gap without a human noticing first. The
            // own-new-worker case is named distinctly so a sweep can tell a
            // "should have used prompt:" author error from a genuine typo.
            if !dry {
                if own_new_worker {
                    tracing::warn!(target: "env_apply", to = %to,
                        "env-apply message skipped: recipient is a worker this env creates but has not started — use its `prompt:` field for the kickoff (worker-model duality D6, AC-353)");
                } else {
                    tracing::warn!(target: "env_apply", to = %to,
                        "env-apply message skipped: recipient not in _amux_workers (worker-model duality D6)");
                }
            }
        }
    }

    // Phase-2 stanzas still parsed-and-reported (not silently dropped).
    if !spec.columns.is_empty() {
        report.push(json!({"kind": "columns", "action": "not-yet-applied", "count": spec.columns.len(),
            "detail": "phase 2 (AMUX-2977) — parsed and reported, not written"}));
    }
    if spec.global.is_some() {
        report.push(json!({"kind": "global", "action": "not-yet-applied",
            "detail": "phase 2 — server.env writes need a restart, deliberately not automatic"}));
    }

    if dry {
        return Json(json!({"dry_run": true, "report": report})).into_response();
    }

    // ---- APPLY (writes) ----------------------------------------------------
    let mut errors = vec![];
    for (path, content, name, _action, workdir) in worker_writes {
        // Create the worker's WORKDIR (the bootstrap — CC_DIR must exist for the
        // pane to boot into it). A failure here is the real, reportable error the
        // old pre-existence check was reaching for, but now it fires only when
        // creation genuinely can't happen (perms/invalid path), not on absence.
        if !workdir.trim().is_empty() {
            if let Err(e) = std::fs::create_dir_all(&workdir) {
                errors.push(json!({"kind": "worker", "name": name, "error": format!("could not create dir {workdir}: {e}")}));
            }
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = write_env_atomic(&path, &content) {
            errors.push(json!({"kind": "worker", "name": name, "error": e.to_string()}));
        }
    }
    // A worker create/delete changes the fleet registry — invalidate the cache
    // (the AMUX-2960 discipline) so the new workers show up immediately.
    super::sessions_legacy::invalidate_sessions_cache();

    // ---- worker prompts: steer the first-run task to NEW workers only ------
    // After the env files exist. Delivered at the worker's next turn boundary;
    // if its session is not up yet (provisioned separately) it queues and lands
    // when the worker starts. Only newly-created workers are here (see the loop).
    for (name, prompt) in &worker_prompts {
        let _ = crate::api::session_verbs::steer_enqueue(&state, name, prompt, "env-apply-prompt", "").await;
    }

    // ---- files: write each seed doc to its absolute path -------------------
    for (path, content) in file_writes {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, content) {
            errors.push(json!({"kind": "file", "path": path.to_string_lossy(), "error": e.to_string()}));
        }
    }

    if !groups_for_write.is_empty() {
        let gw = groups_for_write.clone();
        let _ = state
            .store
            .write_async(move |conn| {
                let now = chrono::Utc::now().timestamp();
                for (name, dept, goal) in &gw {
                    conn.execute(
                        "INSERT INTO group_config (name, department, goal, updated) VALUES (?1,?2,?3,?4) \
                         ON CONFLICT(name) DO UPDATE SET department=?2, goal=?3, updated=?4",
                        rusqlite::params![name, dept, goal, now],
                    )?;
                }
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await;
    }

    // ---- schedules: create the absent ones via the real scheduler path -----
    if !sched_writes.is_empty() {
        let writes = sched_writes;
        // Capture the result, don't swallow it: a swallowed insert error is
        // exactly how this shipped creating 0 schedules with a 200 (a NOT NULL
        // column left unset). A failure now lands in `errors` in the response.
        let res = state
            .store
            .write_async(move |conn| {
                use crate::runtime_jobs::scheduler as sch;
                let now = chrono::Local::now();
                let now_ts = chrono::Utc::now().timestamp();
                for (worker, title, expr, enabled, command) in &writes {
                    // Re-check inside the write so two concurrent applies cannot
                    // both create the same (worker,title).
                    let already = sch::list_schedules(conn, Some(worker))
                        .unwrap_or_default()
                        .iter()
                        .any(|s| s.str_field("title") == title);
                    if already {
                        continue;
                    }
                    let (sched_type, next_run) = if expr.is_empty() {
                        ("once".to_string(), sch::fmt_minute(now))
                    } else {
                        match sch::ScheduleExpr::parse(expr) {
                            Ok(p) => (
                                "recurring".to_string(),
                                p.next_run_after(now).map(sch::fmt_minute).unwrap_or_else(|| sch::fmt_minute(now)),
                            ),
                            Err(_) => continue, // already reported as error in the dry pass
                        }
                    };
                    let mut m = serde_json::Map::new();
                    m.insert("title".into(), json!(title));
                    m.insert("session".into(), json!(worker));
                    m.insert("command".into(), json!(command));
                    m.insert("kind".into(), json!("tmux"));
                    m.insert("sched_type".into(), json!(sched_type));
                    m.insert("run_at".into(), json!(sch::fmt_minute(now)));
                    m.insert("next_run".into(), json!(next_run));
                    m.insert("last_run".into(), Value::Null);
                    m.insert("enabled".into(), json!(*enabled as i64));
                    m.insert("run_count".into(), json!(0));
                    m.insert("schedule_expr".into(), if expr.is_empty() { Value::Null } else { json!(expr) });
                    m.insert("watch".into(), json!(0));
                    // These columns are NOT NULL DEFAULT <x> — but insert_schedule
                    // lists every column explicitly, so an omitted key inserts NULL
                    // (violating the constraint) rather than falling to the default.
                    // Set them to the create handler's defaults.
                    m.insert("watch_timeout".into(), json!(120));
                    m.insert("done_action".into(), json!("disable"));
                    m.insert("trigger_cooldown".into(), json!(120));
                    m.insert("created".into(), json!(now_ts));
                    m.insert("updated".into(), json!(now_ts));
                    m.insert("deleted".into(), Value::Null);
                    let id = sch::mint_schedule_id(conn)?;
                    m.insert("id".into(), json!(id));
                    sch::insert_schedule(conn, &sch::DurableSchedule::from_map(m))?;
                }
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await;
        if let Err(e) = res {
            errors.push(json!({"kind": "schedule", "error": e.to_string()}));
        }
    }

    // ---- cards: create the absent initial board issues ---------------------
    if !card_writes.is_empty() {
        let cards = card_writes;
        let res = state
            .store
            .write_async(move |conn| {
                let now = chrono::Utc::now().timestamp();
                for (worker, title, desc, status, itype, epic) in &cards {
                    // Re-check inside the write so a concurrent apply cannot
                    // both create the same (worker,title).
                    let exists: bool = conn
                        .query_row(
                            "SELECT 1 FROM issues WHERE session=?1 AND title=?2 \
                             AND deleted IS NULL AND COALESCE(archived,0)=0 LIMIT 1",
                            rusqlite::params![worker, title],
                            |_| Ok(true),
                        )
                        .optional()?
                        .unwrap_or(false);
                    if exists {
                        continue;
                    }
                    let new = crate::db::board_store::NewIssue {
                        title: title.clone(),
                        desc: desc.clone(),
                        status: status.clone(),
                        session: Some(worker.clone()),
                        item_type: itype.clone(),
                        creator: "env-apply".into(),
                        owner_type: "agent".into(),
                        due: None,
                        due_time: None,
                        reviewer: None,
                        shepherd: None,
                        gate: vec![],
                        depends_on: vec![],
                        tags: vec![],
                    };
                    let row = crate::db::board_store::create_issue(conn, &new, now)?;
                    if !epic.is_empty() {
                        conn.execute(
                            "UPDATE issues SET epic=?1 WHERE id=?2",
                            rusqlite::params![epic, row.id],
                        )?;
                    }
                }
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await;
        if let Err(e) = res {
            errors.push(json!({"kind": "card", "error": e.to_string()}));
        }
    }

    // ---- messages: resolve recipients + deliver, idempotent by (target, body)
    // Reuses messages::resolve_recipient + insert_message_and_deliver — the ONE
    // create-message path, so group fan-out and AtTurnBoundary delivery match
    // POST /api/messages exactly (ethos D6, Invariant 9 no-double-queue).
    if !spec.messages.is_empty() {
        let msgs: Vec<(String, String, String)> = spec
            .messages
            .iter()
            .filter(|m| !m.to.trim().is_empty() && !m.text.trim().is_empty())
            .map(|m| (m.to.trim().to_string(), m.from.trim().to_string(), m.text.trim().to_string()))
            .collect();
        let res = state
            .store
            .write_async(move |conn| {
                let now = chrono::Utc::now();
                let mut events = vec![];
                for (to, from, text) in &msgs {
                    // Unknown worker name -> skip (a message to nobody is a no-op).
                    let target = match super::messages::resolve_recipient(conn, to)? {
                        Some(t) => t,
                        None => continue,
                    };
                    // Sender: a worker name/id -> that worker; else the owner (an
                    // env is applied by the owner, so an empty from is honestly them).
                    let is_owner = from.is_empty()
                        || from.eq_ignore_ascii_case("human")
                        || from.eq_ignore_ascii_case("owner");
                    let from_actor = if is_owner {
                        amux_core::events::Actor::Human {
                            name: if from.is_empty() { "owner".into() } else { from.clone() },
                        }
                    } else {
                        match super::messages::resolve_recipient(conn, from)? {
                            Some(amux_core::message::MessageTarget::Worker(w)) => {
                                amux_core::events::Actor::Worker { id: w }
                            }
                            _ => amux_core::events::Actor::Human { name: "owner".into() },
                        }
                    };
                    // Idempotency: skip an identical (recipient, body) already sent.
                    let target_json = serde_json::to_string(&target).unwrap_or_default();
                    let exists: bool = conn
                        .query_row(
                            "SELECT 1 FROM _amux_messages WHERE target = ?1 AND body = ?2 LIMIT 1",
                            rusqlite::params![target_json, text],
                            |_| Ok(true),
                        )
                        .optional()?
                        .unwrap_or(false);
                    if exists {
                        continue;
                    }
                    let (_p, _c, _n, evs) = super::messages::insert_message_and_deliver(
                        conn,
                        from_actor,
                        target,
                        text.clone(),
                        None,
                        now,
                    )?;
                    events.extend(evs);
                }
                Ok(WriteOutcome { applied: true, events })
            })
            .await;
        if let Err(e) = res {
            errors.push(json!({"kind": "message", "error": e.to_string()}));
        }
    }

    Json(json!({
        "applied": true,
        "report": report,
        "errors": errors,
    }))
    .into_response()
}

// ---- helpers ---------------------------------------------------------------

fn bad(msg: String) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response()
}

fn sanitize(raw: &str) -> String {
    raw.trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' })
        .collect()
}

/// The env-file body a worker spec produces (K="V" lines, no volatile header).
fn render_worker_env(w: &WorkerSpec) -> String {
    let mut pairs: Vec<(&str, String)> = vec![];
    if !w.dir.is_empty() {
        pairs.push(("CC_DIR", w.dir.clone()));
    }
    if !w.groups.is_empty() {
        pairs.push(("CC_TAGS", w.groups.join(",")));
    }
    if !w.desc.is_empty() {
        pairs.push(("CC_DESC", w.desc.clone()));
    }
    let provider = if w.provider.is_empty() { "claude".to_string() } else { w.provider.clone() };
    if provider != "claude" {
        pairs.push(("CC_PROVIDER", provider.clone()));
    }
    if !w.model.is_empty() {
        // Model wiring is provider-shaped. Agent CLIs (claude/codex/gemini) take
        // `--model X` as a flag so it rides in CC_FLAGS. Ollama is launched via
        // `codex --oss --local-provider ollama --model <model>`; the model name
        // lives in CC_MODEL and session_verbs.rs builds the `--model` flag from
        // it, so we keep writing CC_MODEL here rather than injecting into CC_FLAGS.
        if provider == "ollama" {
            pairs.push(("CC_MODEL", w.model.clone()));
        } else {
            pairs.push(("CC_FLAGS", format!("--model {}", w.model)));
        }
    }
    pairs.iter().map(|(k, v)| format!("{k}=\"{v}\"")).collect::<Vec<_>>().join("\n")
}

/// True if the existing env file's body (ignoring the `# updated:` header)
/// already equals `content` — so a re-apply reports "unchanged", not "update".
fn same_env_body(path: &std::path::Path, content: &str) -> bool {
    let Ok(existing) = std::fs::read_to_string(path) else { return false };
    let strip = |s: &str| -> String {
        s.lines()
            .filter(|l| !l.trim_start().starts_with("# updated:"))
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    };
    strip(&existing) == strip(content)
}

/// Write the env file the same way create_session_legacy does: `# updated:`
/// header, 0600, atomic rename.
fn write_env_atomic(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let out = format!(
        "# updated: {}\n{}\n",
        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.6f"),
        body
    );
    let tmp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("env"),
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(out.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::sessions_legacy::worker_model_env;

    // Pull the value of a CC_ key out of render_worker_env's `K="V"` body.
    fn env_val(body: &str, key: &str) -> String {
        for line in body.lines() {
            if let Some(rest) = line.strip_prefix(&format!("{key}=\"")) {
                return rest.trim_end_matches('"').to_string();
            }
        }
        String::new()
    }

    /// AF-58 (AMUX-3182 review, amux-frustrations): 0d809ff's commit message and
    /// worker_model_env's own doc-comment claim it "cannot drift from
    /// env_config::render", but nothing enforced that: both functions were
    /// private and the property was two comments agreeing (ethos rule 6: grep
    /// for the thing the docstring promises). This makes the promise a CHECK:
    /// the create path (worker_model_env) and the env-apply path
    /// (render_worker_env) must produce the SAME provider-shaped model wiring.
    ///
    /// Scoped to the inputs both routes accept: no explicit flags and no
    /// create-only default model, because render_worker_env has neither concept.
    /// That single remaining divergence (ollama + explicit CC_FLAGS carrying
    /// --model) is deliberately NOT asserted here; it is caught at launch by
    /// the ollama start-arm WARN (session_verbs.rs), which the review confirmed
    /// is load-bearing for exactly that shape.
    #[test]
    fn create_path_and_render_agree_on_provider_model_wiring() {
        for (provider, model) in [
            ("ollama", "qwen3.8:27b"),
            ("ollama", "qwen2.5vl:7b"),
            ("codex", "gpt-5.5"),
            ("claude", "opus"),
            ("gemini", "gemini-2.5-pro"),
            ("ollama", ""),
            ("claude", ""),
        ] {
            let spec = WorkerSpec {
                provider: provider.to_string(),
                model: model.to_string(),
                ..Default::default()
            };
            let rendered = render_worker_env(&spec);
            let render_cc_model = env_val(&rendered, "CC_MODEL");
            let render_cc_flags = env_val(&rendered, "CC_FLAGS");

            // Same inputs, no explicit flags, no default, the subset
            // render_worker_env is limited to.
            let (create_cc_flags, create_cc_model, _) = worker_model_env(provider, model, "", "");

            assert_eq!(
                render_cc_model, create_cc_model,
                "CC_MODEL disagrees for {provider}/{model:?}: render={render_cc_model:?} create={create_cc_model:?}"
            );
            assert_eq!(
                render_cc_flags, create_cc_flags,
                "CC_FLAGS disagrees for {provider}/{model:?}: render={render_cc_flags:?} create={create_cc_flags:?}"
            );
        }
        // Positive control: the routes MUST diverge if worker_model_env's ollama
        // branch is lost. Simulate that drift by asking the create helper for the
        // claude-shaped wiring of an ollama model, which pins --model in CC_FLAGS,
        // which is exactly what render does NOT do for ollama. If this ever
        // matched, the agreement assertions above would be vacuous.
        let (drift_flags, drift_model, _) = worker_model_env("claude", "qwen3.8:27b", "", "");
        let ollama_render = render_worker_env(&WorkerSpec {
            provider: "ollama".into(),
            model: "qwen3.8:27b".into(),
            ..Default::default()
        });
        assert_ne!(
            drift_flags, env_val(&ollama_render, "CC_FLAGS"),
            "control: claude-shaped wiring must differ from ollama render CC_FLAGS"
        );
        assert!(drift_model.is_empty(), "control: claude path yields no CC_MODEL");
    }
}
