//! The worker projection (primitive integrity audit P2, step 9).
//!
//! One record per worker, computed once, serialized twice:
//!
//! - `GET /api/sessions` serves the projection rows themselves, the
//!   Python-shaped array the dashboard, the bash CLI and the apps read. The
//!   rows are built by the fleet build in `sessions_legacy` (env files, tmux,
//!   the typed store) and their status comes from the derivation below.
//! - `GET /api/workers` and `GET /api/workers/{id}` serialize the SAME rows into
//!   the modern envelope through [`worker_record`]. The typed store only adds
//!   the fields it alone has (`id`, `version`, the typed `state`, ...).
//!
//! Before this module the two routes computed status twice: `/api/sessions`
//! through `FleetSignals::derive_status` and `python_status`, `/api/workers`
//! through its own `dashboard_status` over the typed state, and they listed
//! different fleets (every env-file worker was missing from `/api/workers`).
//! [`SHARED_FIELDS`] names every field both routes carry; [`diverged_fields`]
//! checks them, and the `/api/workers` handlers log the verdict.

use super::sessions_legacy::{
    api_retry_in, authority_of, env_secs, pane_churn_distinct, report_applies_event, telemetry_health,
    FleetSignals, CHURN_MIN_DISTINCT, CHURN_MIN_SINCE_CLAIM,
};

/// How a shared field is spelled on `/api/workers` relative to `/api/sessions`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoding {
    /// Byte-identical value.
    Same,
    /// `/api/sessions` carries epoch seconds (0 = none); `/api/workers` has
    /// always carried RFC 3339 or null. Same fact, the older spelling kept.
    EpochAsRfc3339,
}

/// Every field both routes expose: (`/api/sessions` key, `/api/workers` key,
/// encoding). `/api/workers` takes each of these from the projection row
/// whenever the row carries it, so a value on one route is the value on the
/// other. A field the row does not carry (a typed worker's row has no
/// `tokens`) keeps the typed store's value and is not compared.
pub(crate) const SHARED_FIELDS: &[(&str, &str, Encoding)] = &[
    ("name", "name", Encoding::Same),
    ("name", "display_name", Encoding::Same),
    ("dir", "dir", Encoding::Same),
    ("dir", "cwd", Encoding::Same),
    ("status", "status", Encoding::Same),
    ("running", "running", Encoding::Same),
    ("lifecycle", "lifecycle", Encoding::Same),
    ("archived", "archived", Encoding::Same),
    ("isolated", "isolated", Encoding::Same),
    ("provider", "provider", Encoding::Same),
    ("model", "model", Encoding::Same),
    ("worker_type", "worker_type", Encoding::Same),
    ("renderer", "renderer", Encoding::Same),
    ("preview", "preview", Encoding::Same),
    ("preview_lines", "preview_lines", Encoding::Same),
    ("task_name", "task_name", Encoding::Same),
    ("last_activity", "last_activity", Encoding::Same),
    ("tokens", "tokens", Encoding::Same),
    ("agent_state", "agent_state", Encoding::Same),
    ("turn_state", "turn_state", Encoding::Same),
    ("activity", "activity", Encoding::Same),
    ("status_authority", "status_authority", Encoding::Same),
    ("rate_limited_until", "rate_limited_until", Encoding::EpochAsRfc3339),
];

fn encode(v: &serde_json::Value, enc: Encoding) -> serde_json::Value {
    match enc {
        Encoding::Same => v.clone(),
        Encoding::EpochAsRfc3339 => v
            .as_i64()
            .filter(|t| *t > 0)
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map_or(serde_json::Value::Null, |d| serde_json::Value::String(d.to_rfc3339())),
    }
}

/// The base of a record for a worker the typed store does not know (every
/// env-file worker). The typed-only fields are present and empty, the shape
/// `/api/workers` has always had; `state` carries the projection's status in
/// the typed tag's place, `stopped` when the row reads blank.
fn untyped_base(row: &serde_json::Value) -> serde_json::Value {
    let status = row.get("status").and_then(serde_json::Value::as_str).unwrap_or("");
    serde_json::json!({
        "id": serde_json::Value::Null,
        "name_aliases": [],
        "backend": row.get("backend").cloned().unwrap_or(serde_json::Value::Null),
        "environment": {},
        "permissions": [],
        "group": serde_json::Value::Null,
        "state": {"state": if status.is_empty() { "stopped" } else { status }},
        "rate_limited_until": serde_json::Value::Null,
        "version": serde_json::Value::Null,
        "created_at": serde_json::Value::Null,
        "updated_at": serde_json::Value::Null,
        "tokens": serde_json::Value::Null,
    })
}

/// The `/api/workers` record for one projection row. `typed` is the typed
/// store's body for the same worker (`workers::worker_body`), or None for a
/// worker only the fleet registry knows. Every [`SHARED_FIELDS`] entry the row
/// carries overwrites whatever the typed body said.
pub(crate) fn worker_record(
    row: &serde_json::Value,
    typed: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut out = typed.unwrap_or_else(|| untyped_base(row));
    if let Some(obj) = out.as_object_mut() {
        for (from, to, enc) in SHARED_FIELDS {
            if let Some(v) = row.get(*from) {
                obj.insert((*to).to_string(), encode(v, *enc));
            }
        }
    }
    out
}

/// The `/api/workers` keys whose value differs from the projection row's.
/// Empty means the two routes agree on this worker.
pub(crate) fn diverged_fields(
    row: &serde_json::Value,
    record: &serde_json::Value,
) -> Vec<&'static str> {
    SHARED_FIELDS
        .iter()
        .filter(|(from, to, enc)| {
            row.get(*from)
                .is_some_and(|v| record.get(*to) != Some(&encode(v, *enc)))
        })
        .map(|(_, to, _)| *to)
        .collect()
}

/// The projection row for `name` (case-insensitive, the fleet's own rule).
pub(crate) fn row_for<'a>(
    rows: &'a [serde_json::Value],
    name: &str,
) -> Option<&'a serde_json::Value> {
    rows.iter().find(|r| {
        r.get("name")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

/// Rebuild the fleet snapshot when a typed row was written after it began, so
/// a typed worker's record is never older than the row itself. Reads the
/// newest `updated_at` of the rows the caller just loaded.
pub(crate) fn refresh_for_typed<'a>(updated_at: impl Iterator<Item = &'a str>) -> bool {
    let newest = updated_at
        .filter_map(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|d| d.timestamp_millis() as f64 / 1000.0)
        .fold(f64::NEG_INFINITY, f64::max);
    if !newest.is_finite() {
        return false;
    }
    let rebuilt = super::sessions_legacy::invalidate_if_built_before(newest);
    if rebuilt {
        tracing::debug!(target: "amux::workers", verdict = "worker_projection_refreshed",
            measured = true, n_considered = 1,
            "a typed worker row is newer than the fleet snapshot; rebuilding before serving it");
    }
    rebuilt
}

/// Whether the two routes agreed, over the records one `/api/workers` read
/// produced. Debug on every read (the route is rarely polled); a divergence is
/// a WARN at most once per ten minutes, naming the first worker and fields.
/// `unprojected` counts typed rows the fleet snapshot did not carry (served
/// from the typed body alone, so nothing to compare).
pub(crate) fn note_verdict(
    route: &'static str,
    pairs: &[(&serde_json::Value, &serde_json::Value)],
    unprojected: usize,
) -> usize {
    use std::sync::atomic::{AtomicI64, Ordering};
    static LAST_WARN_S: AtomicI64 = AtomicI64::new(0);
    let mut diverged = 0usize;
    let mut first: Option<(String, Vec<&'static str>)> = None;
    for (row, record) in pairs {
        let fields = diverged_fields(row, record);
        if !fields.is_empty() {
            diverged += 1;
            if first.is_none() {
                let name = row.get("name").and_then(serde_json::Value::as_str).unwrap_or("");
                first = Some((name.to_string(), fields));
            }
        }
    }
    let verdict = if diverged == 0 { "worker_projection_agrees" } else { "worker_projection_diverged" };
    tracing::debug!(target: "amux::workers", route, verdict, measured = true,
        n_considered = pairs.len(), diverged, unprojected,
        "shared worker fields checked against the /api/sessions projection");
    if let Some((name, fields)) = first {
        let now = chrono::Utc::now().timestamp();
        let last = LAST_WARN_S.load(Ordering::Relaxed);
        if now - last >= 600
            && LAST_WARN_S
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            tracing::warn!(target: "amux::workers", route, verdict, measured = true,
                n_considered = pairs.len(), diverged, worker = %name, fields = ?fields,
                "/api/workers and /api/sessions disagree about a worker; the record builder \
                 is not copying the projection (sampled: one line per 10 minutes)");
        }
    }
    diverged
}

/// WorkerState -> the Python status vocabulary the SPA's badges render.
pub(crate) fn python_status(state_json: &str) -> &'static str {
    let value: serde_json::Value = serde_json::from_str(state_json).unwrap_or_default();
    match value.get("state").and_then(serde_json::Value::as_str) {
        Some("active") => "active",
        Some("idle") => "idle",
        Some("waiting") => "waiting",
        Some("rate_limited") => "rate_limited",
        Some("error") => "error",
        Some("starting") => "starting",
        _ => "", // stopped renders as blank in the legacy wire format
    }
}

// ---- status derivation (AMUX-2589) ---------------------------------------
//
// Python's `status` is its scanner's judgment (pane regex) overridden by a
// fresh self-report (amux-server.py:20201-20263). The Rust server runs no
// scanner (D1: scrapers are the deviation, not the goal), so the honest
// equivalents are, in Python-precedence order:
//   base:  the Python scanner's own LAST PERSISTED judgment — the
//          session.working/idle/waiting transition it writes to
//          `session_events` (py:20268-20270, the D1 report-endpoint shape:
//          a durable store the producer already writes) — guarded against
//          staleness (pre-restart events discarded; an `active` with no
//          pane output for AMUX_ACTIVE_HEARTBEAT_S is not active);
//          falling back to tmux activity (<60s = active, else idle).
//   over:  self_report when fresh, with Python's ASYMMETRIC freshness
//          (py:20233-20263): `idle` does not decay (the only exit is a
//          prompt, which fires UserPromptSubmit -> a new report; window
//          AMUX_HOOKS_LIVE_IDLE_S=86400), `active`/`waiting` do
//          (AMUX_HOOKS_LIVE_S=1800), and a stale `active` report (older
//          than the heartbeat, AMUX_ACTIVE_HEARTBEAT_S=120) never
//          overrides — a long turn is byte-identical to a wedged one.
//   last:  CONTRADICTION — physical evidence overrides a stale `idle`
//          (AMUX-2646, below).
//   "" :   not running.
//
// AMUX-2646 — "it is running but says idle". The asymmetric window above
// says an `idle` report never decays, on the reasoning that "the only exit
// from idle is a prompt, and every prompt fires UserPromptSubmit". That
// premise is false in at least four reachable ways, and each of them leaves
// a lane permanently mislabelled because nothing else in the derivation
// could ever disagree:
//
//   1. The UserPromptSubmit POST is best-effort (`curl -m 2`, no retry). The
//      server re-execs on every save of its own source on a shared checkout,
//      so a dropped report is routine, not exotic.
//   2. `report_post` then REFUSES every `tool-hook` heartbeat for the rest of
//      the turn ("a heartbeat must not resurrect a finished turn",
//      AMUX-2538) — the one signal that could self-heal is suppressed by
//      design, correctly, for a different reason.
//   3. Anything can write any state for any session over `/report`; a hand
//      -run hook test wrote `{"state":"idle","source":"stop-hook-test"}` onto
//      a LIVE working lane and it stuck for 1076s until a human noticed.
//   4. Work resumes without a prompt: a backgrounded command re-invoking the
//      agent, a resumed session, a hookless provider (gemini/codex) whose
//      stale claude-era report outlives its hooks.
//
// A claim that no evidence can contradict is not a status, it is an axiom.
// So `idle` still survives SILENCE for the full 24h — a parked lane must not
// be re-scraped forever, which is the asymmetry's real purpose — but it does
// not survive CONTRADICTION: a pane that is unambiguously mid-turn AND has
// painted within AMUX_IDLE_CONTRADICTION_S overrides an idle report older
// than that same window. Both halves are required, and the "has painted"
// half is what keeps this from re-reading a parked lane's scrollback: a lane
// that quotes "esc to interrupt" in a transcript it wrote hours ago (a real
// self-block here, AMUX-2642) emits no output, so it is never probed.
//
// KNOWN residual, measured 2026-08-09 against the live fleet (114/116
// exact): Python emits "" for a RUNNING session whose pane shows no
// recognizable agent UI (claude exited to a shell). That cell exists only
// in the pane regex; this derivation reads idle for it. Re-implementing
// the regex would deepen D1, so the residual is documented, not coded away.

impl FleetSignals {
    /// Python's status value for one session (see the derivation note above).
    pub fn derive_status(&self, name: &str, running: bool) -> String {
        self.derive_status_explain(name, running).0
    }

    /// The derivation WITH its why (AMUX-3434). This IS the implementation —
    /// `derive_status` discards the explanation — so the explain can never
    /// drift from the verdict it describes (a view must share the predicate of
    /// the mechanism, ethos rule 1). Built because AMUX-3426 cost a screenshot
    /// investigation: nothing could answer "which rule decided, over what
    /// evidence, inside which trust window". Served by
    /// GET /api/sessions/{name}/status-explain.
    /// Process, turn and activity as separate facts, plus the authority of the
    /// status and the health of the channel that produced it (MSG-69352 items
    /// 1-5). `agent_state` is `status` unless the only evidence is an
    /// `inferred` guess while telemetry is not healthy: then it is `unknown`,
    /// and nothing automated acts on it (`turn_boundary_status`).
    pub(super) fn status_projection(
        &self,
        name: &str,
        status: &str,
        decided: &str,
        ex: &serde_json::Map<String, serde_json::Value>,
    ) -> serde_json::Value {
        use serde_json::json;
        let cfg = crate::api::session_verbs::parse_env(name);
        let provider = crate::api::session_verbs::provider_of(&cfg);
        // A SessionEnd cannot have come from the process running now: it is the
        // previous run's last word (gtm-ticker 2026-09-26 read "healthy, quiet"
        // on a 28h-old SessionEnd when its start time was unrecorded).
        let report = ex
            .get("report")
            .filter(|r| r.is_object() && r["event"].as_str() != Some("SessionEnd"));
        let rep_tuple = report.map(|r| {
            (
                r["state"].as_str().unwrap_or(""),
                r["from_this_life"].as_bool().unwrap_or(false),
                r["applied"].as_bool().unwrap_or(false),
                r["age_s"].as_f64().unwrap_or(f64::MAX),
            )
        });
        let codex_live = ex.get("codex_rollout").is_some_and(|c| c["applied"] == true);
        let started = self.started.get(name).copied().unwrap_or(0.0);
        let started_age = (started > 0.0).then_some(self.now - started);
        let (health, reason) = telemetry_health(&provider, rep_tuple, codex_live, started_age);
        // A pane read backed by the provider's own explicit chrome is more
        // than a guess: a RECOGNISED empty composer for idle, and Claude's
        // "esc to interrupt" footer (drawn only while a turn is in flight) for
        // active. Generic spinner shapes and redraws stay inferred.
        let decided_eff = if decided == "pane"
            && status == "idle"
            && self
                .pane_of(name)
                .is_some_and(crate::api::session_verbs::pane_is_at_boundary)
        {
            "pane_boundary"
        } else if decided == "pane"
            && status == "active"
            && self.pane_of(name).is_some_and(|raw| {
                let clean = crate::backend::adapter::strip_ansi(raw);
                let lines: Vec<&str> = clean.lines().filter(|l| !l.trim().is_empty()).collect();
                lines[lines.len().saturating_sub(3)..]
                    .iter()
                    .any(|l| l.to_lowercase().contains("esc to interrupt"))
            })
        {
            "pane_interruptible"
        } else {
            decided
        };
        let authority = authority_of(decided_eff).unwrap_or("inferred");
        // TURN: from authoritative or corroborated evidence only.
        let map_state = |st: &str| match st {
            "active" => "running",
            "idle" => "completed",
            "waiting" | "blocked" => "waiting",
            "error" => "failed",
            _ => "unknown",
        };
        let rep_applied = rep_tuple.is_some_and(|t| t.2);
        let turn_state = if decided == "claude_transcript_interrupt" {
            "interrupted"
        } else if codex_live {
            map_state(ex["codex_rollout"]["state"].as_str().unwrap_or(""))
        } else if rep_applied {
            map_state(rep_tuple.map(|t| t.0).unwrap_or(""))
        } else if authority != "inferred" {
            map_state(status)
        } else {
            "unknown"
        };
        let background = ex.get("provider_background_working").is_some_and(|v| v == true);
        let subagents = ex.get("subagents_working").is_some_and(|v| v == true);
        let event = report.and_then(|r| r["event"].as_str()).unwrap_or("");
        let activity = match (status, turn_state) {
            ("rate_limited", _) => "rate_limited",
            ("api_error", _) => "error",
            ("blocked", _) => "waiting_for_approval",
            ("waiting", _) => "waiting_for_input",
            _ if subagents => "subagents",
            (_, "completed") if background => "background_shell",
            (_, "running") if event == "PreToolUse" => "tool",
            (_, "running") => "working",
            (_, "completed") | (_, "interrupted") => "none",
            ("active", "unknown") if background => "background_shell",
            _ => "unknown",
        };
        let agent_state = if authority == "inferred"
            && health != "healthy"
            && matches!(status, "active" | "idle")
        {
            "unknown"
        } else {
            status
        };
        // PROGRESS: the newest real event from this run. Pane redraws are not
        // progress and never count here.
        let mut progress: Option<f64> = report
            .filter(|r| r["from_this_life"] == true)
            .and_then(|r| r["observed_at"].as_f64());
        if codex_live {
            let ts = self.now - ex["codex_rollout"]["age_s"].as_f64().unwrap_or(0.0);
            progress = Some(progress.map_or(ts, |p| p.max(ts)));
        }
        // Between turns, say WHY the lane is quiet: its own job is running, it
        // only has watch loops, or nothing at all (MSG-99658).
        let jobs = self.background_jobs.get(name).cloned().unwrap_or_default();
        let idle_reason = (turn_state == "completed").then(|| {
            if jobs.iter().any(|j| !j.waiting) { "own_job_running" } else if !jobs.is_empty() { "watch_only" } else { "idle" }
        });
        json!({
            "process_state": "running",
            "turn_state": turn_state,
            "activity": activity,
            "background_jobs": jobs,
            "idle_reason": idle_reason,
            "authority": authority,
            "agent_state": agent_state,
            "telemetry": {
                "health": health,
                "reason": reason,
                "provider": provider,
                "last_report_age_s": rep_tuple.map(|t| t.3).filter(|a| a.is_finite()),
            },
            "last_progress_at": progress,
        })
    }

    pub fn derive_status_explain(&self, name: &str, running: bool) -> (String, serde_json::Value) {
        use serde_json::json;
        let mut ex = serde_json::Map::new();
        if !running {
            ex.insert("decided_by".into(), json!("not_running"));
            ex.insert("projection".into(), json!({
                "process_state": "absent", "turn_state": "none", "activity": "none",
                "authority": "authoritative", "agent_state": "stopped",
                "telemetry": {"health": "stopped", "reason": "the worker is not running"},
                "last_progress_at": null,
            }));
            return (String::new(), serde_json::Value::Object(ex));
        }
        let mut decided = "activity_fallback";
        let heartbeat = env_secs("AMUX_ACTIVE_HEARTBEAT_S", 120.0);
        let act = self
            .activity
            .get(&format!("amux-{name}"))
            .copied()
            .unwrap_or(0) as f64;
        ex.insert(
            "activity".into(),
            json!({"age_s": (self.now - act).max(0.0), "heartbeat_s": heartbeat}),
        );
        let mut status: Option<String> = None;
        if let Some((st, ts)) = self.transitions.get(name) {
            // A transition from before the session's last (re)start describes
            // a previous life — Python never emits a transition out of the ""
            // state, so a restart leaves the old row behind (verified: the
            // guard flipped 1 live mismatch on 2026-08-09).
            let from_this_life = self.started.get(name).copied().unwrap_or(0.0) <= *ts;
            let demoted = st == "active" && self.now - act > heartbeat;
            ex.insert(
                "transition".into(),
                json!({
                    "state": st,
                    "age_s": (self.now - ts).max(0.0),
                    "from_this_life": from_this_life,
                    "stale_active_demoted_to_idle": from_this_life && demoted,
                }),
            );
            if from_this_life {
                decided = "transition";
                if demoted {
                    // An active session paints its pane continuously; silence
                    // past the heartbeat means the transition went stale.
                    status = Some("idle".into());
                } else {
                    status = Some(st.clone());
                }
            }
        }
        // The pane evidence, recorded regardless of which rule ends up
        // deciding — when the verdict is wrong, this is what the reader needs.
        ex.insert(
            "pane".into(),
            json!({
                "admissible": self.pane_of(name).is_some(),
                "detect": self
                    .pane_of(name)
                    .map(crate::api::session_verbs::detect_claude_status)
                    .unwrap_or_default(),
                "says_working": self.pane_says_working(name),
                // AMUX-3433: the model-agnostic leg, visible where people
                // look — distinct content-frames in the window vs the bar.
                "churn_distinct_frames": pane_churn_distinct(
                    name,
                    self.now,
                    self.contradiction_window()
                ),
                "churn_threshold": CHURN_MIN_DISTINCT,
                "contradiction_window_s": self.contradiction_window(),
            }),
        );
        // THE SUBAGENT COUNT, WHERE THE READERS ALREADY LOOK (AMUX-4024).
        // `status_history.rs` has recorded `explain.subagents_live` in every row
        // since it shipped, on the same "carry the EVIDENCE, not only the
        // verdict" argument as the report and pane blocks beside it — and this
        // key was never inserted here, so every stored row carried null. The
        // field is now the deciding evidence for
        // `contradiction_subagents_reported_live`, and it is the one place a
        // LEAKED count (a lost SubagentStop pinning a lane WORKING) would be
        // visible after the fact. `null` is a real answer and a different one
        // from `0`: it means this lane reports no lifecycle events at all and is
        // being judged on the mtime window instead.
        ex.insert(
            "subagents_live".into(),
            self.reported_subagent_count(name)
                .map(|c| json!(c))
                .unwrap_or(serde_json::Value::Null),
        );
        ex.insert(
            "subagent_live_ids".into(),
            self.reported_subagent_ids(name)
                .map(|ids| json!(ids))
                .unwrap_or(serde_json::Value::Null),
        );
        ex.insert(
            "subagents_working".into(),
            json!(self.subagents_working(name)),
        );
        // No transition: prefer the PANE over the activity timestamp when the
        // pane is admissible. A timestamp says something painted; the pane
        // says what. `detect_claude_status` returning "" is the documented
        // Python residual (an agentless shell) and reads idle here, as it did
        // before — the fallback below stays for a lane with no readable pane,
        // which after `capture_panes` means a silent one (idle) or a herdr
        // lane mid-turn (empty capture, and `act` is fresh, so: active).
        let mut status = status.unwrap_or_else(|| {
            match self
                .pane_of(name)
                .map(crate::api::session_verbs::detect_claude_status)
            {
                Some(v) if v == "active" || v == "waiting" => {
                    decided = "pane";
                    v
                }
                Some(_) => {
                    decided = "pane";
                    "idle".into()
                }
                None if self.now - act < 60.0 => "active".into(),
                None => "idle".into(),
            }
        });
        // self_report override — Python's exact gate (py:20248-20263).
        let mut idle_report_age: Option<f64> = None;
        if let Some(rep) = self.reports.get(name) {
            let st = rep["state"].as_str().unwrap_or("");
            // ts is time.time() — a FLOAT. as_i64() on it is None, which
            // silently read every report as epoch-0 (the age_s bug).
            let ts = rep["ts"].as_f64().unwrap_or(0.0);
            // A report from BEFORE the session's last (re)start describes a
            // PREVIOUS LIFE — the same guard the transition block above has
            // had all along, missing here. Found live 2026-08-11: board-exp-1
            // switched claude -> codex, its hours-old claude `idle` report
            // (24h trust window) outranked the codex trust picker the pane
            // was showing, and a lane blocked on input read idle. A restarted
            // claude lane loses nothing: its hooks re-report on the first
            // turn, and until then the pane and activity decide — which is
            // exactly right for the boot window.
            let started = self.started.get(name).copied().unwrap_or(0.0);
            let from_this_life = started <= ts;
            let age = self.now - ts;
            let ev = rep["event"].as_str().unwrap_or("");
            let stale_active = st == "active"
                && age > if ev == "PreToolUse" { env_secs("AMUX_OPEN_TOOL_S", 1800.0) } else { heartbeat };
            let trust_window = if st == "idle" {
                env_secs("AMUX_HOOKS_LIVE_IDLE_S", 86400.0)
            } else {
                env_secs("AMUX_HOOKS_LIVE_S", 1800.0)
            };
            // THE VERDICT COMES FROM THE SHARED PREDICATE, never from the
            // locals above — those exist only to publish the evidence. The
            // steering/pickup gate calls the same function, so the display and
            // the mechanism cannot drift (AMUX-3756, ethos rule 1).
            let applied = report_applies_event(st, ev, ts, started, self.now);
            ex.insert(
                "report".into(),
                json!({
                    "state": st,
                    "source": rep.get("source").and_then(|v| v.as_str()).unwrap_or(""),
                    "native": rep["native_status"].as_bool().unwrap_or(false),
                    "event": rep["event"], "run_id": rep["run_id"],
                    "sequence": rep["sequence"], "observed_at": ts,
                    "received_at": rep["received_at"],
                    "age_s": age.max(0.0),
                    "trust_window_s": trust_window,
                    "from_this_life": from_this_life,
                    "stale_active": stale_active,
                    "applied": applied,
                }),
            );
            if applied {
                decided = "report";
                status = st.to_string();
                if st == "idle" {
                    idle_report_age = Some(age);
                }
            }
        }
        // A PERMISSION PROMPT DRAWN NOW OUTRANKS AN `active` REPORT (gs12-mvs,
        // 2026-10-07). A subagent's prompt can fire no hook, so the last edge
        // stays `active` while the lane sits on "Do you want to proceed?" for an
        // hour. The prompt on screen, with its options and no status bar below,
        // is current evidence; the report is an older edge. amux never answers
        // it (D2); it only stops calling the lane busy.
        // The raw capture, not pane_of: a frozen prompt does not repaint, and the
        // detector only accepts a prompt drawn at the bottom with no status bar
        // below it. No capture this pass leaves the record as it was.
        if let Some(raw) = self.panes.get(name).map(String::as_str).filter(|r| !r.trim().is_empty()) {
            if let Some((line, since)) = crate::api::prompt_block::observe(name, Some(raw), self.now) {
                ex.insert("permission_prompt".into(), json!({"line": line, "since": since}));
                // `idle` too: once the edge ages past the heartbeat it is
                // demoted to idle, which would invite dispatch into a lane that
                // cannot take a prompt.
                if matches!(status.as_str(), "active" | "idle") {
                    decided = "pane_permission_prompt";
                    status = "waiting".into();
                }
            }
        }
        // CONTRADICTION (AMUX-2646). `idle` survives silence, never
        // contradiction. Fires only when BOTH halves hold: the claim is older
        // than the window (a fresh report is still the authority — D1), and
        // the pane both painted inside the window and shows the main turn
        // generating. It can only ever flip idle -> active, so a missed frame
        // costs a late correction, never a false "busy".
        let idle_gate_open = idle_report_age
            .map(|a| a > self.contradiction_window())
            .unwrap_or(true);
        // ...AND A FRESH ONE IS FALSIFIABLE TOO, BY EVIDENCE THE RACE CANNOT
        // MANUFACTURE (AMUX-3896). The window above is one number doing two
        // jobs, and its own doc says so: "one number for both halves because it
        // is one question". They are two questions. How recent must a frame be
        // to be admissible wants 60s. How long does a lane's own word survive
        // contrary evidence wants ~1s — the repaint lag after a Stop hook is
        // the entire race the window was written for.
        //
        // Conflating them costs the normal amux flow: a lane ends a turn
        // (stop-hook -> idle) and immediately starts another (auto-continue,
        // standing orders, input queued at the boundary), then reads IDLE until
        // 60s pass or its first tool call fires a tool-hook. Ethan, 22:54
        // 2026-08-29: "tubescience worker says idle but its not". The specimen
        // is in that lane's own status-explain history — status=idle,
        // decided_by=report, report age 8.8s, over pane detect=active with 20
        // distinct content frames. It sat that way for 49s. Sampling every
        // running lane found the same shape on 8 of 57, all stop-hook, ages
        // 0.6s to 34s.
        //
        // So: keep the window for weak evidence (a bar phrase, a single
        // spinner-shaped frame — a stale frame can show either), and add one
        // narrow gate for evidence a frozen frame cannot produce. The pane must
        // show a spinner AND have REDRAWN since the claim: `CHURN_MIN_DISTINCT`
        // distinct content-frames inside a window that starts at the report's
        // own timestamp. A stale frame is by definition one frame and stops
        // there; a lane really working repaints ~6x/s. That flips the 8.8s,
        // 16.3s and 34.0s specimens and leaves the 0.6s and 0.7s ones — which
        // ARE the repaint race — exactly as they are.
        //
        // Measured in the same window as the claim, never a fixed one: at age
        // 3s only the last 3s of frames may vote, so the evidence can never
        // include the turn the lane just finished.
        let churn_since_claim = idle_report_age
            .map(|a| self.pane_churn_since(name, a))
            .unwrap_or(0);
        let fresh_idle_contradicted = !idle_gate_open
            && churn_since_claim >= CHURN_MIN_SINCE_CLAIM
            && self
                .pane_of(name)
                .map(crate::api::session_verbs::detect_claude_status)
                .as_deref()
                == Some("active");
        ex.insert(
            "idle_report_age_s".into(),
            idle_report_age
                .map(|a| json!(a))
                .unwrap_or(serde_json::Value::Null),
        );
        ex.insert("idle_contradiction_gate_open".into(), json!(idle_gate_open));
        ex.insert("churn_since_claim".into(), json!(churn_since_claim));
        ex.insert(
            "churn_since_claim_threshold".into(),
            json!(CHURN_MIN_SINCE_CLAIM),
        );
        ex.insert(
            "fresh_idle_contradicted".into(),
            json!(fresh_idle_contradicted),
        );
        // Provider-owned background work is not the repaint-race evidence the
        // fresh-idle gate protects against. Claude explicitly says it is
        // waiting for live agents; Codex explicitly says its background
        // terminal is running. Both describe the lane AFTER the parent prompt
        // became idle, so a fresh parent report cannot contradict them.
        // The footer marker is part of the frame just captured, so it is
        // current by construction: an idle-reported lane is read here even when
        // it has not repainted, and only for this provider-chrome marker.
        let provider_background_working = self
            .pane_of(name)
            .or_else(|| {
                self.idle_report_current(name)
                    .then(|| self.panes.get(name).map(String::as_str))
                    .flatten()
                    .filter(|raw| !raw.trim().is_empty())
            })
            .is_some_and(crate::api::session_verbs::provider_background_working);
        // Shells that only wait are not work. Background AGENTS still count:
        // only the footer's shell count is discounted, and only when the
        // process tree was measured and every shell process is a waiter.
        let raw_now = self.pane_of(name).or_else(|| self.panes.get(name).map(String::as_str)).unwrap_or("");
        let shells_only_waiting = provider_background_working
            && self.claude_waiting_shells.contains(name)
            && !crate::backend::adapter::codex_pane_background_working(raw_now)
            && !crate::backend::adapter::claude_background_wait_verdict(&crate::backend::adapter::strip_ansi(raw_now)).0;
        if shells_only_waiting {
            ex.insert("background_shells_waiting".into(), json!(true));
            if crate::log_dedupe::first_this_bucket(
                &format!("waiting-shells:{name}"),
                crate::log_dedupe::hour_bucket(crate::config::now_f64()),
            ) {
                tracing::info!(session = %name, measured = true, n_considered = 1,
                    verdict = "background_shells_only_waiting",
                    "footer counts background shells but every process in them is waiting; not counted as work");
            }
        }
        let provider_background_working = provider_background_working && !shells_only_waiting;
        ex.insert(
            "provider_background_working".into(),
            json!(provider_background_working),
        );
        if status == "idle" && provider_background_working {
            status = "active".into();
            decided = "contradiction_provider_background_working";
        }
        if status == "idle"
            && (idle_gate_open || fresh_idle_contradicted)
            && self.pane_says_working(name)
        {
            status = "active".into();
            // NAMED APART from the aged path, because the two rest on different
            // evidence and a sweep that cannot tell them apart cannot tell
            // whether this gate is earning its keep or firing on the race.
            decided = if idle_gate_open {
                "contradiction_pane_generating"
            } else {
                "contradiction_pane_redrew_since_claim"
            };
        }
        // A PICKER CONTRADICTS IDLE TOO (AMUX-2952's status half). The rule
        // above only ever flips idle -> active on a GENERATING pane, so a lane
        // sitting at an input-required selector kept reading `idle` — measured
        // live on tubescience 2026-08-11: the pane showed AskUserQuestion's
        // "Ready to submit your answers?" while the header said IDLE, and
        // Ethan pressed Enter into a lane nothing had flagged as waiting.
        // `waiting` is the one state whose whole purpose is to summon a human;
        // mislabelling it idle is strictly worse than mislabelling work,
        // because nothing and nobody is coming.
        //
        // Same shape as the rule above on purpose: one-way (idle -> waiting),
        // gated on the same report-age window, evidence from the same
        // admissible pane. A missed frame costs a late correction, never a
        // false "waiting".
        if status == "idle" && idle_gate_open {
            if let Some(raw) = self.pane_of(name) {
                if crate::api::session_verbs::detect_claude_status(raw) == "waiting" {
                    status = "waiting".into();
                    decided = "contradiction_picker_waiting";
                }
            }
        }
        // SUBAGENTS ARE THE LANE WORKING TOO (AMUX-2904) — but a FRESH idle
        // self-report outranks the subagent-mtime window, exactly as it does
        // over a working pane. This is now LITERALLY "the same one-way rule as
        // the pane contradiction above": same `idle_report_age >
        // contradiction_window` gate, same admissible evidence, still only ever
        // idle -> active. The gate was MISSING here while the comment above
        // claimed sameness (Ethan, 2026-08-13: "says working but it appears
        // done", over a "✻ Crunched for 1m 7s" idle prompt with a ~30s-old
        // stop-hook idle report and 2 background agents). A stopped main turn is
        // a stop-hook idle report, and a stopped main turn means its FOREGROUND
        // subagents have necessarily finished — so a fresh idle report is the
        // stronger signal and must win for the window, instead of the 240s
        // `AMUX_SUBAGENT_WORKING_S` mtime window pinning the header WORKING for
        // up to four minutes after the turn was done. AMUX-2904 is unchanged: a
        // main turn ACTIVE with foreground subagents has NOT stopped, so there
        // is no fresh idle report (`idle_report_age` is None -> `unwrap_or(true)`
        // -> the flip still fires), and once a real idle report ages past the
        // window a still-writing subagent flips it active as the bounded late
        // correction the window was always documented to cost.
        //
        // AMUX-4024: A REPORTED LIVE COUNT DOES NOT WAIT FOR THAT GATE. Read the
        // justification above one more time — "a stopped main turn means its
        // FOREGROUND subagents have necessarily finished". True, and it is the
        // whole argument, and BACKGROUND agents are the case it does not cover:
        // they outlive the main turn by construction (AMUX-2904), so the stop
        // hook fires, the report is fresh, and the gate holds the correction
        // shut for a full minute while the lane sits there working.
        //
        // Ethan, 2026-08-30, tubescience: header IDLE over a pane reading
        // "Waiting for 1 background agent to finish". Its status-explain history
        // flapped idle -> active -> idle on a ~30s cycle, every idle row
        // `decided_by: report`, because each new report restarted the window.
        //
        // The gate exists because an MTIME can be stale — it cannot tell
        // "finished 30s ago" from "thinking, will write in 90s", so a fresh
        // self-report is the better evidence and should win. An event-driven
        // count has no such defect: it moves only when a subagent actually
        // starts or stops, so a positive count is a statement about NOW, and
        // there is nothing for the window to protect against. Gate the mtime,
        // trust the count.
        //
        // Deliberately NOT the pane-churn leg, which stays gated: content churn
        // is also what a human typing at the composer produces
        // (`churn_without_a_spinner_does_not_falsify_a_fresh_idle_claim`), and
        // flipping a lane to WORKING because someone is typing into it is the
        // error the other half of this card is about. A subagent count cannot be
        // typed.
        // Believed only while its report is current, the same rule
        // subagents_working applies: a lost SubagentStop leaves a positive
        // count in every later report, and the structured-children rule below
        // read it raw, so mixpeek-general stayed WORKING after the fix to
        // subagents_working went live (2026-11-08).
        let subagents_reported_live = self.reported_subagent_count(name).is_some_and(|c| c > 0)
            && self.subagent_count_current(name);
        if status == "idle"
            && (idle_gate_open || subagents_reported_live)
            && self.subagents_working(name)
        {
            status = "active".into();
            // Named apart from the aged path, same reason the pane rules are:
            // one rests on a timestamp inside a window, the other on an event.
            decided = if idle_gate_open {
                "contradiction_subagents_working"
            } else {
                "contradiction_subagents_reported_live"
            };
        }
        // CODEX'S STRUCTURED TURN BOUNDARY IS ITS HOOK (AMUX-4051 E2E).
        // tmux's activity clock stayed 41 minutes old while a Codex worker's
        // terminal visibly showed `Working (5m … esc to interrupt)`, so the
        // pane was rejected before the existing Codex-aware scraper could read
        // it. The rollout is append-only and emits task_started/task_complete;
        // honour that direct provider signal after scrape-based contradictions.
        //
        // A signal from before the latest worker restart is a previous life and
        // cannot vote. A visible selector still wins as `waiting`: an open turn
        // says work is unfinished, not that it is safe to type into the picker.
        // Native lifecycle edges lead while current and fresh. Physical
        // contradictions still rescue a missed hook; older rollout boundaries
        // cannot undo a newer permission, interrupt, or completion hook.
        let native = self.reports.get(name).filter(|rep| {
            rep["native_status"].as_bool() == Some(true)
                && report_applies_event(
                    rep["state"].as_str().unwrap_or(""),
                    rep["event"].as_str().unwrap_or(""),
                    rep["ts"].as_f64().unwrap_or(0.0),
                    self.started.get(name).copied().unwrap_or(0.0),
                    self.now,
                )
                && self.now - rep["ts"].as_f64().unwrap_or(0.0) <= 120.0
                && !fresh_idle_contradicted
        });
        if let Some(rep) = native {
            status = rep["state"].as_str().unwrap_or("idle").into();
            decided = "native_hook";
        }
        if let Some(signal) = self.codex_turns.get(name) {
            let started = self.started.get(name).copied().unwrap_or(0.0);
            let from_this_life = started > 0.0 && signal.ts >= started;
            let heartbeat_age = (self.now - signal.heartbeat_ts).max(0.0);
            let heartbeat_window = env_secs("AMUX_CODEX_TURN_HEARTBEAT_S", 300.0);
            let heartbeat_fresh = heartbeat_age <= heartbeat_window;
            let tool_child_running = self.provider_child_activity.contains(name);
            let active_is_live = signal.state != "active"
                || heartbeat_fresh
                || tool_child_running
                || self.subagents_working(name);
            let newer_than_hook =
                native.is_none_or(|rep| signal.ts > rep["ts"].as_f64().unwrap_or(0.0));
            let applied = from_this_life && active_is_live && newer_than_hook;
            let pane_waiting = self
                .pane_of(name)
                .map(crate::api::session_verbs::detect_claude_status)
                .as_deref()
                == Some("waiting");
            ex.insert(
                "codex_rollout".into(),
                json!({
                    "state": signal.state,
                    "boundary": signal.boundary,
                    "rollout_file": signal.rollout_file,
                    "age_s": (self.now - signal.ts).max(0.0),
                    "heartbeat_age_s": heartbeat_age,
                    "heartbeat_window_s": heartbeat_window,
                    "heartbeat_fresh": heartbeat_fresh,
                    "tool_child_running": tool_child_running,
                    "tool_children_measured": self.provider_children_measured,
                    "from_this_life": from_this_life,
                    "applied": applied,
                }),
            );
            if applied {
                if signal.state == "active" && pane_waiting {
                    status = "waiting".into();
                    decided = "codex_rollout_with_picker";
                } else {
                    status = signal.state.clone();
                    decided = "codex_rollout";
                }
            } else if from_this_life && signal.state == "active" && newer_than_hook {
                // A `task_started` edge can survive a provider crash or an
                // interrupted generation indefinitely. Codex also keeps its
                // Working timer/footer repainting, so pane mtime/churn are not
                // independent evidence. With neither a bounded structured
                // heartbeat nor a live tool descendant, force the fossil idle
                // and name the rejected evidence in status-explain.
                if self.provider_children_measured {
                    status = "idle".into();
                    decided = "codex_stale_active_refused";
                } else {
                    status = "active".into();
                    decided = "codex_child_probe_unmeasured";
                }
            }
        }
        if let Some(ts) = self
            .reports
            .get(name)
            .and_then(|r| r["transcript_interrupt_ts"].as_f64())
        {
            if ts >= self.started.get(name).copied().unwrap_or(0.0) {
                status = "idle".into();
                decided = "claude_transcript_interrupt";
                ex.insert("transcript_interrupt_ts".into(), json!(ts));
            }
        }
        // Main-turn completion does not complete its live tool/subagents.
        if status == "idle"
            && (self.provider_child_activity.contains(name)
                || subagents_reported_live
                || provider_background_working)
        {
            status = "active".into();
            decided = "structured_live_children";
        }
        // CLI-owned menus (/hooks, /model, approval) can open without a model
        // turn and therefore without UserPromptSubmit. A current structural
        // picker is newer evidence than an earlier idle/active hook.
        if status != "blocked"
            && self.pane_of(name).is_some_and(|pane| {
                crate::backend::adapter::provider_picker_reason(
                    &crate::backend::adapter::strip_ansi(pane),
                    "codex",
                )
                .is_some()
            })
        {
            status = "waiting".into();
            decided = "provider_picker";
        }
        // API-ERROR (5xx / Overloaded) is its own status (Ethan 2026-08-18).
        // Claude Code ENDS the turn on a 529 and returns to the prompt, so its
        // Stop hook reports `idle` and the scrape reads `idle` too — which is
        // exactly what @backend showed while its pane sat on
        // "API Error: 529 Overloaded". A stuck-on-error lane is not idle in the
        // sense a human cares about: it wants a retry/continue, so a sweep has
        // to be able to FIND it — surfacing it as `idle` hides it in the same
        // bucket as every parked lane. Overrides idle/waiting but never
        // `active`: an actively-retrying lane (spinner) is genuinely working,
        // and `has_current_api_error` only honours the banner when it sits in
        // the TAIL, so a lane that recovered and produced newer output does not
        // read as errored (the bounded false-positive here costs a glance, not
        // a wrong action — unlike ghost-rescue, nothing force-acts on this).
        if (status == "idle" || status == "waiting")
            && self
                .pane_of(name)
                .map(crate::api::session_verbs::has_current_api_error)
                .unwrap_or(false)
        {
            status = "api_error".into();
            decided = "api_error_banner";
        }
        if self
            .panes
            .get(name)
            .and_then(|raw| crate::backend::adapter::claude_auto_resume_banner(raw))
            .is_some()
        {
            status = "rate_limited".into();
            decided = "provider_auto_resume_quota";
        }
        let mut projection = self.status_projection(name, &status, decided, &ex);
        // WHY, IN THE PROVIDER'S OWN WORDS (Ethan 2026-09-27: "this should be
        // something I can actually use to figure out why is it working, idle,
        // rate limited"). Claude's footer says when a request failed and it is
        // waiting to retry; that worker read "working" while it was stuck.
        let retry_in = self.pane_of(name).and_then(api_retry_in);
        if retry_in.is_some() && status == "active" {
            projection["activity"] = serde_json::json!("api_retry");
        }
        ex.insert("pane_cues".into(), serde_json::json!({ "api_retry_in": retry_in }));
        ex.insert("projection".into(), projection);
        ex.insert("decided_by".into(), json!(decided));
        (status, serde_json::Value::Object(ex))
    }

}

#[cfg(test)]
mod bench {
    /// Build-time measurement for `/api/sessions` on a fixture fleet. Ignored:
    /// it is a timing, run by hand with `--run-ignored only`.
    #[test]
    #[ignore]
    fn sessions_build_timing_on_a_fixture_fleet() {
        let dir = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(dir.path());
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        for i in 0..60 {
            let extra = match i % 6 {
                0 => "CC_PAUSED=\"1\"\n",
                1 => "CC_ISOLATED=\"1\"\n",
                2 => "CC_ARCHIVED=\"1\"\n",
                _ => "",
            };
            std::fs::write(
                sessions.join(format!("bench-w{i}.env")),
                format!("CC_DIR=\"/tmp\"\nCC_PROVIDER=\"claude\"\nCC_DESC=\"bench {i}\"\n{extra}"),
            )
            .unwrap();
        }
        let store = std::sync::Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        let mut ms: Vec<f64> = Vec::new();
        for _ in 0..7 {
            crate::api::sessions_legacy::invalidate_sessions_cache();
            let t = std::time::Instant::now();
            let json = crate::api::sessions_legacy::legacy_sessions_array(&store).unwrap();
            ms.push(t.elapsed().as_secs_f64() * 1000.0);
            assert!(json.contains("bench-w59"));
        }
        ms.sort_by(|a, b| a.total_cmp(b));
        eprintln!("{}", serde_json::json!({"verdict":"sessions_build_timing","rows":60,
            "median_ms":ms[3],"min_ms":ms[0],"max_ms":ms[6],"measured":true,"n_considered":7}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Wait out a sibling test's fleet suppression (process-wide under
    /// `cargo test`), so a suppressed read cannot pass as an empty fleet.
    fn wait_for_fleet_reads() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while crate::api::sessions_legacy::fleet_suppressed() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// The fixture fleet: one worker per state the cards render. Returns the
    /// signals that make each one read as its name says.
    fn fixture_fleet(home: &std::path::Path) -> FleetSignals {
        let sessions = home.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        for (name, extra) in [
            ("wp-active", ""),
            ("wp-idle", ""),
            ("wp-waiting", ""),
            ("wp-paused", "CC_PAUSED=\"1\"\n"),
            ("wp-stopped", ""),
            ("wp-deadpane", ""),
            ("wp-isolated", "CC_ISOLATED=\"1\"\n"),
        ] {
            std::fs::write(
                sessions.join(format!("{name}.env")),
                format!("CC_DIR=\"/tmp/{name}\"\nCC_PROVIDER=\"claude\"\nCC_MODEL=\"sonnet\"\n{extra}"),
            )
            .unwrap();
        }
        let mut s = crate::api::sessions_legacy::tests::signals();
        for live in ["wp-active", "wp-idle", "wp-waiting", "wp-isolated", "wp-deadpane"] {
            s.running.insert(format!("amux-{live}"));
            s.created.insert(format!("amux-{live}"), 999_000);
        }
        // The dead pane: tmux still lists the session, the pane hosts a bare
        // shell and the process probe found no agent under it.
        s.shell_only.insert("amux-wp-deadpane".into());
        s.activity.insert("amux-wp-active".into(), 999_970);
        for quiet in ["wp-idle", "wp-waiting", "wp-isolated", "wp-deadpane"] {
            s.activity.insert(format!("amux-{quiet}"), 999_000);
        }
        s.transitions.insert("wp-waiting".into(), ("waiting".into(), 999_900.0));
        s
    }

    /// One record per worker: for every state in the fixture fleet, the
    /// `/api/workers` record carries exactly the `/api/sessions` row's value
    /// for every shared field, and those values are the right ones.
    #[test]
    fn both_routes_serialize_one_record_per_worker() {
        let dir = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(dir.path());
        let signals = fixture_fleet(dir.path());
        wait_for_fleet_reads();
        let rows = crate::api::sessions_legacy::python_fleet_sessions(&signals);
        assert_eq!(rows.len(), 7, "the fixture fleet was not read: {rows:?}");
        // (name, status, running, lifecycle, isolated)
        let want = [
            ("wp-active", "active", true, "active", false),
            ("wp-idle", "idle", true, "active", false),
            ("wp-waiting", "waiting", true, "active", false),
            ("wp-paused", "", false, "paused", false),
            ("wp-stopped", "", false, "active", false),
            ("wp-deadpane", "", false, "active", false),
            ("wp-isolated", "idle", true, "active", true),
        ];
        for (name, status, running, lifecycle, isolated) in want {
            let row = row_for(&rows, name).unwrap_or_else(|| panic!("{name} missing"));
            let record = worker_record(row, None);
            assert_eq!(diverged_fields(row, &record), Vec::<&str>::new(), "{name}: {record}");
            assert_eq!(record["status"], json!(status), "{name}: {record}");
            assert_eq!(record["running"], json!(running), "{name}: {record}");
            assert_eq!(record["lifecycle"], json!(lifecycle), "{name}: {record}");
            assert_eq!(record["isolated"], json!(isolated), "{name}: {record}");
            assert_eq!(record["display_name"], json!(name), "{name}: {record}");
            assert_eq!(record["cwd"], json!(format!("/tmp/{name}")), "{name}: {record}");
            assert_eq!(record["activity"], row["activity"], "{name}: {record}");
            assert_eq!(record["agent_state"], row["agent_state"], "{name}: {record}");
            let tag = if status.is_empty() { "stopped" } else { status };
            assert_eq!(record["state"]["state"], json!(tag), "{name}: {record}");
            assert!(record["id"].is_null(), "an env-file worker has no typed id: {record}");
        }
    }

    /// A typed body never outvotes the projection on a shared field, and keeps
    /// the fields only it has.
    #[test]
    fn the_projection_wins_over_a_typed_body_on_shared_fields() {
        let row = json!({"name": "t", "status": "", "running": false, "dir": "/w",
            "provider": "claude", "model": "", "lifecycle": "active", "rate_limited_until": 0});
        let typed = json!({"id": "wrk_1", "version": 3, "status": "stopped", "running": true,
            "model": null, "state": {"state": "stopped"}, "rate_limited_until": "x"});
        let record = worker_record(&row, Some(typed));
        assert_eq!(record["status"], json!(""));
        assert_eq!(record["running"], json!(false));
        assert_eq!(record["model"], json!(""));
        assert_eq!(record["cwd"], json!("/w"));
        assert_eq!(record["rate_limited_until"], serde_json::Value::Null);
        assert_eq!((record["id"].clone(), record["version"].clone()), (json!("wrk_1"), json!(3)));
        assert_eq!(record["state"]["state"], json!("stopped"));
        assert!(diverged_fields(&row, &record).is_empty(), "{record}");
    }

    /// The comparator can fail: a record that drifted on any shared field is
    /// named, and an epoch reset time compares in the older RFC 3339 spelling.
    #[test]
    fn a_drifted_record_is_named_by_the_comparator() {
        let row = json!({"name": "d", "status": "idle", "running": true, "rate_limited_until": 1_800_000_000});
        let mut record = worker_record(&row, None);
        assert_eq!(record["rate_limited_until"], json!("2027-01-15T08:00:00+00:00"));
        assert!(diverged_fields(&row, &record).is_empty(), "{record}");
        record["status"] = json!("stopped");
        record["rate_limited_until"] = serde_json::Value::Null;
        assert_eq!(diverged_fields(&row, &record), vec!["status", "rate_limited_until"]);
        let pair = [(&row, &record)];
        assert_eq!(note_verdict("/api/workers", &pair, 0), 1, "the verdict counts the drift");
    }
}
