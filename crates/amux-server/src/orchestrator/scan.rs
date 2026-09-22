//! Terminal scan loop with demotion (RR-0067, D1).
//!
//! The scraper is the FALLBACK voice, never the control plane: a worker
//! whose structured protocol session is live is not scanned at all — its
//! own reports outrank any scrape (the D1 exit, applied from day one
//! instead of retrofitted like the Python's AMUX_SCAN_DEMOTE_S). Only
//! workers with a live TERMINAL session and no protocol session get their
//! panes captured and run through the TerminalAdapter.
//!
//! The same exit, applied to a whole backend (#84): a backend that can
//! REPORT its hosted agents' lifecycle state (herdr `agent_status`) has
//! that answer outrank the pane scrape too — one batched
//! [`SessionBackend::agent_states`] read per pass, no per-lane probe.
//! Precedence: the worker's own protocol voice (above) > the backend's
//! native report > the scrape.
//!
//! Consecutive identical scan results are deduped by content hash — the
//! Python system's two-scan persistence gate, done once here so every
//! event consumer inherits it (a rate-limit banner sitting in scrollback
//! must not re-fire state changes every cycle).

use crate::backend::{ProcessRef, SessionBackend};
use crate::db::SharedStore;
use crate::opencode::AgentProtocol;
use amux_core::ids::{TurnId, WorkerId};
use amux_core::protocol::{ExitStatus, WaitReason, WorkerEvent};
use amux_core::provider::ProviderId;
use amux_core::worker::WorkerState;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::Digest;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::{OnceLock, RwLock};

pub struct ScanLoop {
    pub store: SharedStore,
    pub backends: Vec<Arc<dyn SessionBackend>>,
    pub protocol: Option<Arc<dyn AgentProtocol>>,
    /// worker -> hash of the last scan's emitted events (dedupe).
    last_scan: Mutex<BTreeMap<WorkerId, String>>,
}

/// What one scan pass did — returned so callers (and tests) see the
/// demotion decisions instead of inferring them from silence (the
/// /api/debug/scan lesson: a skip that leaves no trace is indistinguishable
/// from a scan that found nothing).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ScanReport {
    pub scanned: Vec<String>,
    pub demoted_structured: Vec<String>,
    /// Workers whose capture was skipped because the BACKEND reported their
    /// agent state natively (#84: herdr `agent_status` via one batched
    /// `workspace list`). Same trace discipline as `demoted_structured`.
    pub demoted_native: Vec<String>,
    /// Backends whose native-status read failed this pass; their lanes fell
    /// back to the scrape, and the failure is named rather than silent.
    pub native_status_failures: Vec<String>,
    /// Backend-confirmed exits, distinct from text inferred by the adapter.
    pub process_exits: BTreeMap<String, ExitStatus>,
    pub process_exit_failures: Vec<String>,
    /// Exit observations rejected atomically because their session ended or changed.
    pub stale_process_exits: BTreeMap<String, StaleProcessExit>,
    pub events_applied: usize,
    pub capture_failures: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StaleProcessExit {
    pub observed_session: String,
    pub current_session: Option<String>,
    pub backend_ref: String,
}

/// The last completed scan pass, published for `GET /api/debug/scan` (AF-80):
/// its demotion decisions plus the per-worker dedupe state, so "which lanes
/// were skipped/demoted and why" is answerable without inferring it from
/// silence (ethos rule 4, the D1 "scan" deviation: a skip that leaves no trace
/// is indistinguishable from a scan that found nothing). Same `last_report()`
/// shape as `board_drive`/`autofix`/`storage`: one publish per completed pass,
/// read in-process so there is no second copy of the fact to drift.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ScanState {
    /// Unix seconds when the last pass finished. `None` until the loop has run
    /// once, which is itself the answer to "has the terminal-scan loop ever
    /// run?".
    pub last_pass_at: Option<f64>,
    /// The last pass's decisions: scanned / demoted_structured / demoted_native
    /// / native_status_failures / capture_failures / events_applied.
    pub report: ScanReport,
    /// worker id -> content hash of the events the scrape last emitted for it.
    /// An identical hash on the next pass means the banner is still on screen,
    /// not a fresh occurrence, so a lane sitting here explains why its state did
    /// not re-fire.
    pub deduped: BTreeMap<String, String>,
}

static LAST_SCAN_STATE: OnceLock<RwLock<Option<ScanState>>> = OnceLock::new();

fn scan_state_slot() -> &'static RwLock<Option<ScanState>> {
    LAST_SCAN_STATE.get_or_init(|| RwLock::new(None))
}

fn publish_scan_state(state: ScanState) {
    if let Ok(mut slot) = scan_state_slot().write() {
        *slot = Some(state);
    }
}

/// The last completed scan pass, or `None` if the loop has never run, which is
/// itself the answer to "is the terminal-scan loop alive?". Read by `GET
/// /api/debug/scan` and the `terminal-scan` outcome in `/api/system-jobs`.
pub fn last_scan_state() -> Option<ScanState> {
    scan_state_slot().read().ok().and_then(|s| s.clone())
}

impl ScanLoop {
    pub fn new(
        store: SharedStore,
        backends: Vec<Arc<dyn SessionBackend>>,
        protocol: Option<Arc<dyn AgentProtocol>>,
    ) -> Self {
        ScanLoop {
            store,
            backends,
            protocol,
            last_scan: Mutex::new(BTreeMap::new()),
        }
    }

    /// One pass over live terminal sessions.
    pub async fn scan_once(&self) -> anyhow::Result<ScanReport> {
        let mut report = ScanReport::default();
        // Capture session identity BEFORE asynchronous backend reads. Backend refs
        // survive restarts and cannot identify the generation an exit belongs to.
        let targets: Vec<(String, String, String, String, String)> = {
            let conn = self.store.read()?;
            let mut stmt = conn.prepare(
                "SELECT s.worker_id, s.backend, s.backend_ref,
                        COALESCE(w.provider, 'claude'), s.id
                 FROM _amux_sessions s
                 LEFT JOIN _amux_workers w ON w.id = s.worker_id
                 WHERE s.ended_at IS NULL AND s.backend IN ('tmux', 'herdr')",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
            rows.collect::<Result<_, _>>()?
        };

        // #84: ONE batched native-status read per backend for the whole
        // pass. An empty result (tmux, or a herdr session with no
        // recognized agents) contributes nothing; a FAILED read is named in
        // the report and that backend's lanes keep the scrape this tick —
        // "cannot answer" must not read as "stopped".
        let mut native: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let mut exits = BTreeMap::new();
        for backend in &self.backends {
            match backend.process_exits().await {
                Ok(found) => {
                    exits.insert(backend.name().to_string(), found);
                }
                Err(e) => report
                    .process_exit_failures
                    .push(format!("{}: {e}", backend.name())),
            }
            match backend.agent_states().await {
                Ok(states) if !states.is_empty() => {
                    native.insert(backend.name().to_string(), states);
                }
                Ok(_) => {}
                Err(e) => report
                    .native_status_failures
                    .push(format!("{}: {e}", backend.name())),
            }
        }

        for (wid_str, backend_name, backend_ref, provider, session_id) in targets {
            let Ok(worker) = WorkerId::parse(&wid_str) else {
                continue;
            };

            // DEMOTION: a live structured session means the worker speaks
            // for itself. Skip — and SAY so.
            if let Some(p) = &self.protocol {
                if p.state(&worker).await.is_ok() {
                    report.demoted_structured.push(wid_str.clone());
                    continue;
                }
            }

            // NATIVE (#84): the backend reported this worker's agent state
            // — capture is demoted and the state is applied as the event it
            // already is in WorkerEvent terms. Stays BELOW the protocol
            // demotion above: the worker's own voice outranks its host's
            // observation of it.
            if let Some(status) = native
                .get(&backend_name)
                .and_then(|m| m.get(&backend_ref))
                .filter(|status| matches!(status.as_str(), "working" | "blocked" | "idle" | "done"))
            {
                report.demoted_native.push(wid_str.clone());
                let event = {
                    let conn = self.store.read()?;
                    let prior = crate::db::queries::get_worker(&conn, &wid_str)
                        .ok()
                        .flatten()
                        .map(|r| r.state);
                    let open_turn = open_turn_id(&conn, &wid_str);
                    native_status_event(prior.as_ref(), status, open_turn)
                };
                if let Some(ev) = event {
                    let w = worker.clone();
                    let applied = self
                        .store
                        .write_async(move |conn| {
                            crate::orchestrator::events::apply_event(
                                conn,
                                &w,
                                &ev,
                                chrono::Utc::now(),
                            )
                        })
                        .await;
                    match applied {
                        Ok(_) => report.events_applied += 1,
                        Err(e) => {
                            tracing::warn!(worker = %worker, error = %e, "native status event apply failed")
                        }
                    }
                }
                continue;
            }

            let Some(backend) = self.backends.iter().find(|b| b.name() == backend_name) else {
                continue;
            };
            let proc = ProcessRef {
                backend_ref: backend_ref.clone(),
                pid: None,
            };
            if let Some(status) = exits.get(&backend_name).and_then(|m| m.get(&backend_ref)) {
                let event = WorkerEvent::Exited(status.clone());
                let w = worker.clone();
                let observed = session_id.clone();
                let expected_backend = backend_name.clone();
                let expected_ref = backend_ref.clone();
                let stale = Arc::new(Mutex::new(None));
                let stale_write = stale.clone();
                let applied = self
                    .store
                    .write_async(move |conn| {
                        // Store executes this closure inside its authoritative writer
                        // transaction. A pre-write read would leave the same race open.
                        let current = crate::db::queries::live_session_for(conn, w.as_str())?;
                        if !current.as_ref().is_some_and(|s| s.id == observed
                            && s.backend == expected_backend && s.backend_ref == expected_ref)
                        {
                            let rejected = StaleProcessExit {
                                observed_session: observed,
                                current_session: current.map(|s| s.id),
                                backend_ref: expected_ref,
                            };
                            tracing::warn!(worker = %w,
                                observed_session = %rejected.observed_session,
                                current_session = ?rejected.current_session,
                                backend_ref = %rejected.backend_ref,
                                measured = true, n_considered = 1, applied = false,
                                "terminal_process_exit_stale: exit observation belongs to an ended or replaced session");
                            *stale_write.lock().expect("exit result mutex") = Some(rejected);
                            return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
                        }
                        crate::orchestrator::events::apply_event(
                            conn, &w, &event, chrono::Utc::now(),
                        )
                    })
                    .await;
                match applied {
                    Ok(reply) if reply.applied => {
                        report.events_applied += 1;
                        report.process_exits.insert(wid_str.clone(), status.clone());
                        tracing::warn!(worker = %worker, backend_ref = %backend_ref,
                            session = %session_id,
                            exit_code = ?status.code, signal = ?status.signal,
                            measured = true, n_considered = 1,
                            "terminal_process_exit: retained session no longer hosts a live process");
                    }
                    Ok(_) => {
                        if let Some(rejected) = stale.lock().expect("exit result mutex").take() {
                            report.stale_process_exits.insert(wid_str.clone(), rejected);
                        }
                    }
                    Err(e) => report
                        .process_exit_failures
                        .push(format!("{wid_str}: apply exit: {e}")),
                }
                continue;
            }
            let captured = match backend.capture(&proc, 60).await {
                Ok(c) => c,
                Err(e) => {
                    report.capture_failures.push(format!("{wid_str}: {e}"));
                    continue;
                }
            };
            report.scanned.push(wid_str.clone());

            let adapter =
                crate::backend::adapter::TerminalAdapter::new(ProviderId(provider.clone()));
            let events = adapter.scan(&captured);

            // ACTIVE via scrape (AMUX-3165). A hookless codex/ollama pane's
            // "• Working (…esc to interrupt)" is the ONLY signal that reaches
            // the store that the worker is generating — it has no
            // Stop/UserPromptSubmit hooks (claude) and no structured session
            // (opencode), so unlike every other lane its active state has
            // nowhere else to come from. `scan` cannot carry it (Active needs a
            // turn id and `scan` is pure), so it is reported as a boolean and
            // minted here, treated EXACTLY like a backend's native `working`
            // report: `native_status_event` fires TurnStarted on the
            // idle->active EDGE only and returns None once Active, so a pane
            // that stays "• Working" across scans does not mint a turn row +
            // StatusChanged every pass (Invariant 37, ethos rule 5). Runs
            // before the empty-events guard because an active pane's scan is
            // empty by design.
            if adapter.generating(&captured) {
                let event = {
                    let conn = self.store.read()?;
                    let prior = crate::db::queries::get_worker(&conn, &wid_str)
                        .ok()
                        .flatten()
                        .map(|r| r.state);
                    let open_turn = open_turn_id(&conn, &wid_str);
                    native_status_event(prior.as_ref(), "working", open_turn)
                };
                if let Some(ev) = event {
                    let w = worker.clone();
                    match self
                        .store
                        .write_async(move |conn| {
                            crate::orchestrator::events::apply_event(
                                conn,
                                &w,
                                &ev,
                                chrono::Utc::now(),
                            )
                        })
                        .await
                    {
                        Ok(_) => report.events_applied += 1,
                        Err(e) => {
                            tracing::warn!(worker = %worker, error = %e, "codex active event apply failed")
                        }
                    }
                }
            }

            if events.is_empty() {
                continue;
            }
            // Dedupe: same events from the same worker as last pass = the
            // banner is still on screen, not a new occurrence.
            let hash = {
                let mut h = sha2::Sha256::new();
                h.update(serde_json::to_string(&events).unwrap_or_default());
                hex::encode(h.finalize())
            };
            {
                let mut last = self.last_scan.lock().unwrap();
                if last.get(&worker).map(|s| s.as_str()) == Some(hash.as_str()) {
                    continue;
                }
                last.insert(worker.clone(), hash);
            }
            for ev in events {
                let w = worker.clone();
                let applied = self
                    .store
                    .write_async(move |conn| {
                        crate::orchestrator::events::apply_event(conn, &w, &ev, chrono::Utc::now())
                    })
                    .await;
                match applied {
                    Ok(_) => report.events_applied += 1,
                    Err(e) => {
                        tracing::warn!(worker = %worker, error = %e, "scan event apply failed")
                    }
                }
            }
        }
        // Publish this pass for GET /api/debug/scan (AF-80): the demotion
        // decisions and per-worker dedupe state, so a skip leaves a trace
        // instead of reading as a scan that found nothing (ethos rule 4, the
        // D1 "scan" deviation). Every pass publishes, including an empty one,
        // so a fresh last_pass_at also proves the loop is ticking.
        let deduped = self
            .last_scan
            .lock()
            .map(|m| m.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
            .unwrap_or_default();
        publish_scan_state(ScanState {
            last_pass_at: Some(crate::runtime_jobs::registry::unix_now()),
            report: report.clone(),
            deduped,
        });
        Ok(report)
    }

    /// The loop: scan every `interval_secs`, forever.
    pub async fn run(self: Arc<Self>, interval_secs: u64) {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(interval_secs.max(5)));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            // AMUX-4828: bracket the pass; the one-shot records no duration.
            crate::runtime_jobs::registry::tick_start(crate::runtime_jobs::registry::ids::SCAN);
            match self.scan_once().await {
                Ok(r)
                    if !r.scanned.is_empty()
                        || !r.capture_failures.is_empty()
                        || !r.demoted_native.is_empty()
                        || !r.process_exits.is_empty()
                        || !r.process_exit_failures.is_empty()
                        || !r.stale_process_exits.is_empty()
                        || !r.native_status_failures.is_empty() =>
                {
                    tracing::debug!(
                        scanned = r.scanned.len(),
                        demoted = r.demoted_structured.len(),
                        demoted_native = r.demoted_native.len(),
                        native_failures = r.native_status_failures.len(),
                        process_exits = r.process_exits.len(),
                        process_exit_failures = r.process_exit_failures.len(),
                        stale_process_exits = r.stale_process_exits.len(),
                        events = r.events_applied,
                        failures = r.capture_failures.len(),
                        "terminal scan pass"
                    );
                    // BOTH Ok arms stamp: this is the BUSY one, and stamping
                    // only the quiet arm would make an actively-working
                    // scanner the one that reads as stalled.
                    crate::runtime_jobs::registry::tick_end(
                        crate::runtime_jobs::registry::ids::SCAN,
                    );
                }
                Ok(_) => {
                    crate::runtime_jobs::registry::tick_end(
                        crate::runtime_jobs::registry::ids::SCAN,
                    );
                }
                Err(e) => tracing::warn!(error = %e, "terminal scan pass failed"),
            }
        }
    }
}

/// The worker's open turn, if any — the id a native `idle`/`done` report
/// must close to keep the turn ledger truthful. `None` means the native
/// report saw the agent finish with no TurnStarted on record (e.g. the
/// working phase happened while the server was down); the caller
/// synthesizes one and `apply_event` names the miss in the log.
fn open_turn_id(conn: &Connection, wid: &str) -> Option<TurnId> {
    conn.query_row(
        "SELECT id FROM _amux_turns WHERE worker_id = ?1 AND ended_at IS NULL
         ORDER BY started_at DESC LIMIT 1",
        params![wid],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .and_then(|t| TurnId::parse(&t).ok())
}

/// Map a backend-reported agent status (#84: herdr `working|blocked|idle|
/// done`) onto the WorkerEvent that moves the store toward it, or `None`
/// when it is already there. Pure so the mapping table is testable
/// without a DB.
///
/// Equilibrium returns `None` on purpose: `write_state` writes
/// unconditionally, so re-applying every tick would push a StatusChanged
/// event per tick and bump the revision for no change (Invariant 37).
/// `done ≡ idle`: herdr 0.8.0 surfaces `done` and amux has no state for
/// "the agent process finished but the pane lives" — idle is the honest
/// reading either way (#84 evidence).
fn native_status_event(
    prior: Option<&WorkerState>,
    status: &str,
    open_turn: Option<TurnId>,
) -> Option<WorkerEvent> {
    match status {
        "working" => match prior {
            Some(WorkerState::Active { .. }) => None,
            _ => Some(WorkerEvent::TurnStarted {
                turn_id: TurnId::from_ulid(ulid::Ulid::new()),
            }),
        },
        "blocked" => match prior {
            // WorkerState::Waiting carries the reason as a flat String
            // (the WaitReason shape belongs to the event, not the state).
            Some(WorkerState::Waiting { reason }) if reason == "herdr_agent_blocked" => None,
            _ => Some(WorkerEvent::Waiting(WaitReason {
                reason: "herdr_agent_blocked".into(),
                detail: None,
            })),
        },
        "idle" | "done" => match prior {
            Some(WorkerState::Idle { .. }) => None,
            _ => Some(WorkerEvent::TurnCompleted(
                amux_core::protocol::TurnResult {
                    turn_id: open_turn.unwrap_or_else(|| TurnId::from_ulid(ulid::Ulid::new())),
                    outcome: "herdr agent idle".into(),
                },
            )),
        },
        // `parse_workspace_statuses` never routes anything else here; a
        // future herdr status word falls through to the scrape rather than
        // being guessed at.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{AttachInfo, BackendError, BackendSession, BackendStatus, SessionSpec};
    use crate::db::WriteOutcome;
    use crate::opencode::mock::MockProtocol;
    use crate::opencode::AgentState;
    use async_trait::async_trait;
    use rusqlite::params;

    /// Backend whose capture returns a scripted frame.
    #[derive(Default)]
    struct ScriptedBackend {
        name: &'static str,
        frame: String,
        /// Native agent states the backend reports (backend_ref -> status).
        /// Empty = the tmux default (no native voice).
        native: BTreeMap<String, String>,
        exits: BTreeMap<String, ExitStatus>,
        exit_probe_fails: bool,
    }

    #[async_trait]
    impl SessionBackend for ScriptedBackend {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn spawn(&self, _s: &SessionSpec) -> crate::backend::Result<ProcessRef> {
            Err(BackendError::SpawnFailed("scripted".into()))
        }
        async fn terminate(&self, _p: &ProcessRef) -> crate::backend::Result<()> {
            Ok(())
        }
        async fn status(&self, _p: &ProcessRef) -> crate::backend::Result<BackendStatus> {
            Ok(BackendStatus::Running)
        }
        async fn attach_info(&self, _p: &ProcessRef) -> crate::backend::Result<AttachInfo> {
            Ok(AttachInfo {
                command: "true".into(),
            })
        }
        async fn reconcile(&self) -> crate::backend::Result<Vec<BackendSession>> {
            Ok(vec![])
        }
        async fn capture(&self, _p: &ProcessRef, _l: u32) -> crate::backend::Result<String> {
            Ok(self.frame.clone())
        }
        async fn agent_states(&self) -> crate::backend::Result<BTreeMap<String, String>> {
            Ok(self.native.clone())
        }
        async fn process_exits(&self) -> crate::backend::Result<BTreeMap<String, ExitStatus>> {
            if self.exit_probe_fails {
                Err(BackendError::CommandFailed(
                    "controlled unreadable exit census".into(),
                ))
            } else {
                Ok(self.exits.clone())
            }
        }
    }

    fn store() -> SharedStore {
        let dir = tempfile::tempdir().unwrap();
        let s = Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        std::mem::forget(dir);
        s
    }

    fn wid(n: u128) -> WorkerId {
        WorkerId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, 800 + n))
    }

    fn seed_terminal_worker(store: &SharedStore, w: &WorkerId) {
        seed_worker(store, w, "tmux", "amux-x");
    }

    fn seed_herdr_worker(store: &SharedStore, w: &WorkerId, backend_ref: &str) {
        seed_worker(store, w, "herdr", backend_ref);
    }

    /// A tmux worker whose provider is codex (hookless: the scrape is its only
    /// voice), for the AMUX-3165 active-via-scrape path.
    fn seed_codex_worker(store: &SharedStore, w: &WorkerId) {
        let (id, sid) = (w.to_string(), format!("ses_{w}"));
        store
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO _amux_workers (id, display_name, provider, created_at, updated_at)
                     VALUES (?1, 'term', 'codex', 'now', 'now')",
                    params![id],
                )?;
                conn.execute(
                    "INSERT INTO _amux_sessions (id, worker_id, backend, backend_ref, started_at)
                     VALUES (?1, ?2, 'tmux', 'amux-cx', 'now')",
                    params![sid, id],
                )?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .unwrap();
    }

    fn seed_worker(store: &SharedStore, w: &WorkerId, backend: &str, backend_ref: &str) {
        let (id, sid) = (w.to_string(), format!("ses_{w}"));
        let (backend, backend_ref) = (backend.to_string(), backend_ref.to_string());
        store
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO _amux_workers (id, display_name, provider, created_at, updated_at)
                     VALUES (?1, 'term', 'claude-code', 'now', 'now')",
                    params![id],
                )?;
                conn.execute(
                    "INSERT INTO _amux_sessions (id, worker_id, backend, backend_ref, started_at)
                     VALUES (?1, ?2, ?3, ?4, 'now')",
                    params![sid, id, backend, backend_ref],
                )?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .unwrap();
    }

    fn worker_state(store: &SharedStore, w: &WorkerId) -> WorkerState {
        let conn = store.read().unwrap();
        crate::db::queries::get_worker(&conn, w.as_str())
            .unwrap()
            .expect("worker row")
            .state
    }

    // A frame the Claude adapter flags: the weekly limit banner (fixture
    // shape from adapter.rs's corpus).
    const LIMIT_FRAME: &str =
        "\n\u{23fa} did things\nYou've reached your weekly limit \u{00b7} resets 3pm\n\u{276f} \n";

    #[tokio::test]
    async fn exit_probe_absence_failure_or_another_worker_cannot_stop_a_live_worker() {
        for (n, fails, foreign) in [(1, false, false), (2, true, false), (3, false, true)] {
            let store = store();
            let w = wid(100 + n);
            seed_terminal_worker(&store, &w);
            let id = w.to_string();
            store
                .write(move |conn| {
                    let now = chrono::Utc::now();
                    crate::db::queries::update_worker_state(
                        conn,
                        &id,
                        &WorkerState::Idle { since: now },
                        &now.to_rfc3339(),
                    )?;
                    Ok(WriteOutcome {
                        applied: true,
                        events: vec![],
                    })
                })
                .unwrap();
            let scan = ScanLoop::new(
                store.clone(),
                vec![Arc::new(ScriptedBackend {
                    name: "tmux",
                    frame: String::new(),
                    exit_probe_fails: fails,
                    exits: if foreign {
                        BTreeMap::from([(
                            "amux-other".into(),
                            ExitStatus {
                                code: Some(1),
                                signal: None,
                            },
                        )])
                    } else {
                        BTreeMap::new()
                    },
                    ..Default::default()
                })],
                None,
            );
            let report = scan.scan_once().await.unwrap();
            assert!(matches!(worker_state(&store, &w), WorkerState::Idle { .. }));
            assert_eq!(report.events_applied, 0);
            assert!(report.process_exits.is_empty());
            assert_eq!(report.process_exit_failures.len(), usize::from(fails));
            let conn = store.read().unwrap();
            assert!(crate::db::queries::live_session_for(&conn, w.as_str())
                .unwrap()
                .is_some());
        }
    }

    #[tokio::test]
    async fn structured_session_is_demoted_not_scanned() {
        let store = store();
        let w = wid(1);
        seed_terminal_worker(&store, &w);
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(w.clone(), AgentState::Idle); // structured = live
        let scan = ScanLoop::new(
            store,
            vec![Arc::new(ScriptedBackend {
                name: "tmux",
                frame: LIMIT_FRAME.into(),
                native: BTreeMap::new(),
                exits: BTreeMap::from([(
                    "amux-x".into(),
                    ExitStatus {
                        code: Some(1),
                        signal: None,
                    },
                )]),
                ..Default::default()
            })],
            Some(protocol),
        );
        let r = scan.scan_once().await.unwrap();
        assert_eq!(r.demoted_structured, vec![w.to_string()]);
        assert!(r.scanned.is_empty(), "demoted worker must not be captured");
    }

    #[tokio::test]
    async fn hookless_worker_is_scanned_and_events_dedupe() {
        let store = store();
        let w = wid(2);
        seed_terminal_worker(&store, &w);
        // No protocol session: the scraper is this worker's only voice.
        let scan = ScanLoop::new(
            store.clone(),
            vec![Arc::new(ScriptedBackend {
                name: "tmux",
                frame: LIMIT_FRAME.into(),
                native: BTreeMap::new(),
                ..Default::default()
            })],
            Some(Arc::new(MockProtocol::new())),
        );
        let r1 = scan.scan_once().await.unwrap();
        assert_eq!(r1.scanned, vec![w.to_string()]);
        // Whether the fixture fires depends on the adapter's exact anchors;
        // what MUST hold: a second identical pass applies nothing new.
        let r2 = scan.scan_once().await.unwrap();
        assert_eq!(r2.events_applied, 0, "identical frame must dedupe: {r2:?}");
    }

    #[tokio::test]
    async fn capture_failure_is_reported_not_silent() {
        struct FailingBackend;
        #[async_trait]
        impl SessionBackend for FailingBackend {
            fn name(&self) -> &'static str {
                "tmux"
            }
            async fn spawn(&self, _s: &SessionSpec) -> crate::backend::Result<ProcessRef> {
                Err(BackendError::SpawnFailed("x".into()))
            }
            async fn terminate(&self, _p: &ProcessRef) -> crate::backend::Result<()> {
                Ok(())
            }
            async fn status(&self, _p: &ProcessRef) -> crate::backend::Result<BackendStatus> {
                Ok(BackendStatus::NotFound)
            }
            async fn attach_info(&self, _p: &ProcessRef) -> crate::backend::Result<AttachInfo> {
                Ok(AttachInfo {
                    command: "true".into(),
                })
            }
            async fn reconcile(&self) -> crate::backend::Result<Vec<BackendSession>> {
                Ok(vec![])
            }
            async fn capture(&self, _p: &ProcessRef, _l: u32) -> crate::backend::Result<String> {
                Err(BackendError::CommandFailed("pane gone".into()))
            }
        }
        let store = store();
        let w = wid(3);
        seed_terminal_worker(&store, &w);
        let scan = ScanLoop::new(store, vec![Arc::new(FailingBackend)], None);
        let r = scan.scan_once().await.unwrap();
        assert_eq!(r.capture_failures.len(), 1);
        assert!(r.capture_failures[0].contains("pane gone"));
    }

    // ---- native backend status (#84) ---------------------------------------

    fn native(ref_: &str, status: &str) -> BTreeMap<String, String> {
        BTreeMap::from([(ref_.to_string(), status.to_string())])
    }

    #[tokio::test]
    async fn native_working_starts_turn_and_demotes_capture() {
        let store = store();
        let w = wid(10);
        seed_herdr_worker(&store, &w, "amux-herdr-1");
        let scan = ScanLoop::new(
            store.clone(),
            vec![Arc::new(ScriptedBackend {
                name: "herdr",
                frame: LIMIT_FRAME.into(),
                native: native("amux-herdr-1", "working"),
                ..Default::default()
            })],
            Some(Arc::new(MockProtocol::new())),
        );
        let r = scan.scan_once().await.unwrap();
        assert_eq!(r.demoted_native, vec![w.to_string()]);
        assert!(r.scanned.is_empty(), "native answer must demote the scrape");
        assert_eq!(r.events_applied, 1, "TurnStarted: {r:?}");
        assert!(matches!(
            worker_state(&store, &w),
            WorkerState::Active { .. }
        ));
    }

    #[tokio::test]
    async fn native_blocked_marks_waiting() {
        let store = store();
        let w = wid(11);
        seed_herdr_worker(&store, &w, "amux-herdr-1");
        let scan = ScanLoop::new(
            store.clone(),
            vec![Arc::new(ScriptedBackend {
                name: "herdr",
                frame: LIMIT_FRAME.into(),
                native: native("amux-herdr-1", "blocked"),
                ..Default::default()
            })],
            Some(Arc::new(MockProtocol::new())),
        );
        let r = scan.scan_once().await.unwrap();
        assert_eq!(r.demoted_native.len(), 1);
        assert!(matches!(
            worker_state(&store, &w),
            WorkerState::Waiting { ref reason } if reason == "herdr_agent_blocked"
        ));
    }

    #[tokio::test]
    async fn native_idle_completes_turn_and_equilibrium_applies_nothing() {
        let store = store();
        let w = wid(12);
        seed_herdr_worker(&store, &w, "amux-herdr-1");
        let scan = ScanLoop::new(
            store.clone(),
            vec![Arc::new(ScriptedBackend {
                name: "herdr",
                frame: LIMIT_FRAME.into(),
                native: native("amux-herdr-1", "working"),
                ..Default::default()
            })],
            Some(Arc::new(MockProtocol::new())),
        );
        let r1 = scan.scan_once().await.unwrap();
        assert_eq!(r1.events_applied, 1, "working -> TurnStarted");
        // Same working report again: equilibrium, no event, no revision churn.
        let r2 = scan.scan_once().await.unwrap();
        assert_eq!(
            r2.events_applied, 0,
            "equilibrium must apply nothing: {r2:?}"
        );
        // The backend now reports idle (a fresh backend answers the same
        // lane — the store, not the loop, carries the prior state).
        let scan = ScanLoop::new(
            store.clone(),
            vec![Arc::new(ScriptedBackend {
                name: "herdr",
                frame: LIMIT_FRAME.into(),
                native: native("amux-herdr-1", "idle"),
                ..Default::default()
            })],
            Some(Arc::new(MockProtocol::new())),
        );
        let r3 = scan.scan_once().await.unwrap();
        assert_eq!(r3.events_applied, 1, "idle -> TurnCompleted");
        assert!(matches!(worker_state(&store, &w), WorkerState::Idle { .. }));
        // Idle again: nothing.
        let r4 = scan.scan_once().await.unwrap();
        assert_eq!(r4.events_applied, 0, "idle equilibrium must apply nothing");
    }

    #[tokio::test]
    async fn native_unknown_or_absent_falls_back_to_scrape() {
        let store = store();
        let w = wid(13);
        // `parse_workspace_statuses` omits `unknown` refs, so the map simply
        // does not carry this lane — the scrape stays its only voice.
        seed_herdr_worker(&store, &w, "amux-herdr-other");
        let scan = ScanLoop::new(
            store,
            vec![Arc::new(ScriptedBackend {
                name: "herdr",
                frame: LIMIT_FRAME.into(),
                native: native("amux-herdr-1", "unknown"),
                ..Default::default()
            })],
            Some(Arc::new(MockProtocol::new())),
        );
        let r = scan.scan_once().await.unwrap();
        assert_eq!(r.scanned, vec![w.to_string()]);
        assert!(r.demoted_native.is_empty());
    }

    #[tokio::test]
    async fn native_read_failure_falls_back_and_is_named() {
        struct FlakyNativeBackend;
        #[async_trait]
        impl SessionBackend for FlakyNativeBackend {
            fn name(&self) -> &'static str {
                "herdr"
            }
            async fn spawn(&self, _s: &SessionSpec) -> crate::backend::Result<ProcessRef> {
                Err(BackendError::SpawnFailed("x".into()))
            }
            async fn terminate(&self, _p: &ProcessRef) -> crate::backend::Result<()> {
                Ok(())
            }
            async fn status(&self, _p: &ProcessRef) -> crate::backend::Result<BackendStatus> {
                Ok(BackendStatus::Running)
            }
            async fn attach_info(&self, _p: &ProcessRef) -> crate::backend::Result<AttachInfo> {
                Ok(AttachInfo {
                    command: "true".into(),
                })
            }
            async fn reconcile(&self) -> crate::backend::Result<Vec<BackendSession>> {
                Ok(vec![])
            }
            async fn capture(&self, _p: &ProcessRef, _l: u32) -> crate::backend::Result<String> {
                Ok(LIMIT_FRAME.to_string())
            }
            async fn agent_states(&self) -> crate::backend::Result<BTreeMap<String, String>> {
                Err(BackendError::CommandFailed(
                    "server_not_running: herdr is down".into(),
                ))
            }
        }
        let store = store();
        let w = wid(14);
        seed_herdr_worker(&store, &w, "amux-herdr-1");
        let scan = ScanLoop::new(store, vec![Arc::new(FlakyNativeBackend)], None);
        let r = scan.scan_once().await.unwrap();
        assert_eq!(r.scanned, vec![w.to_string()], "scrape is the fallback");
        assert_eq!(r.native_status_failures.len(), 1);
        assert!(r.native_status_failures[0].contains("server_not_running"));
    }

    #[tokio::test]
    async fn protocol_voice_outranks_native_report() {
        let store = store();
        let w = wid(15);
        seed_herdr_worker(&store, &w, "amux-herdr-1");
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(w.clone(), AgentState::Idle);
        let scan = ScanLoop::new(
            store.clone(),
            vec![Arc::new(ScriptedBackend {
                name: "herdr",
                frame: LIMIT_FRAME.into(),
                native: native("amux-herdr-1", "working"),
                ..Default::default()
            })],
            Some(protocol),
        );
        let r = scan.scan_once().await.unwrap();
        assert_eq!(r.demoted_structured, vec![w.to_string()]);
        assert!(r.demoted_native.is_empty());
        assert_eq!(r.events_applied, 0, "the worker speaks for itself");
        assert_eq!(worker_state(&store, &w), WorkerState::Stopped);
    }

    #[test]
    fn native_status_event_mapping_table() {
        let turn = || Some(TurnId::from_ulid(ulid::Ulid::from_parts(1, 1)));
        let active = WorkerState::Active { turn: None };
        let idle = WorkerState::Idle {
            since: chrono::Utc::now(),
        };
        let waiting = WorkerState::Waiting {
            reason: "herdr_agent_blocked".into(),
        };

        // Unknown status words never guess.
        assert!(native_status_event(Some(&idle), "unknown", turn()).is_none());
        assert!(native_status_event(Some(&idle), "somewhere-else", turn()).is_none());

        // working: only from not-Active.
        assert!(native_status_event(Some(&active), "working", turn()).is_none());
        assert!(matches!(
            native_status_event(Some(&idle), "working", turn()),
            Some(WorkerEvent::TurnStarted { .. })
        ));
        assert!(matches!(
            native_status_event(None, "working", turn()),
            Some(WorkerEvent::TurnStarted { .. })
        ));

        // blocked: only from not-that-Waiting.
        assert!(native_status_event(Some(&waiting), "blocked", turn()).is_none());
        assert!(matches!(
            native_status_event(Some(&active), "blocked", turn()),
            Some(WorkerEvent::Waiting(_))
        ));

        // idle/done close an open turn; equilibrium applies nothing; the
        // done≡idle mapping is the same branch.
        assert!(native_status_event(Some(&idle), "idle", turn()).is_none());
        assert!(native_status_event(Some(&idle), "done", turn()).is_none());
        for st in ["idle", "done"] {
            match native_status_event(Some(&active), st, turn()) {
                Some(WorkerEvent::TurnCompleted(res)) => {
                    assert_eq!(res.turn_id, turn().unwrap());
                }
                other => panic!("{st} from Active must complete the open turn, got {other:?}"),
            }
        }
        // No open turn on record: a synthesized id still completes —
        // apply_event names the missing TurnStarted in the log.
        assert!(matches!(
            native_status_event(Some(&active), "idle", None),
            Some(WorkerEvent::TurnCompleted(_))
        ));
    }

    // ---- codex/ollama active via scrape (AMUX-3165) ------------------------

    // The pane the live incident showed: a working ollama lane that read
    // running=false the whole time because nothing emitted its active state.
    const CODEX_WORKING: &str =
        "\u{203a} do the thing\n\n\u{2022} Working (12s \u{2022} esc to interrupt)";
    // The idle model bar — must NOT mark the lane active.
    const CODEX_IDLE: &str = "\u{203a} implement {feature}\n\n  gpt-5.5 xhigh \u{b7} ~/Dev/amux";

    #[tokio::test]
    async fn codex_working_pane_marks_active_and_does_not_churn() {
        let store = store();
        let w = wid(20);
        seed_codex_worker(&store, &w);
        // Hookless: no protocol session, so the scrape is this lane's only voice.
        let scan = ScanLoop::new(
            store.clone(),
            vec![Arc::new(ScriptedBackend {
                name: "tmux",
                frame: CODEX_WORKING.into(),
                native: BTreeMap::new(),
                ..Default::default()
            })],
            Some(Arc::new(MockProtocol::new())),
        );
        let r1 = scan.scan_once().await.unwrap();
        assert_eq!(r1.scanned, vec![w.to_string()]);
        assert_eq!(r1.events_applied, 1, "working pane -> TurnStarted: {r1:?}");
        assert!(
            matches!(worker_state(&store, &w), WorkerState::Active { .. }),
            "a '• Working' codex pane must read Active, not running=false (AMUX-3165)"
        );
        // Still working next pass: EDGE-gated, so no new turn / StatusChanged
        // churn (Invariant 37) even though the fresh scan is not deduped.
        let r2 = scan.scan_once().await.unwrap();
        assert_eq!(
            r2.events_applied, 0,
            "already-Active must not re-fire: {r2:?}"
        );
        assert!(matches!(
            worker_state(&store, &w),
            WorkerState::Active { .. }
        ));
    }

    #[tokio::test]
    async fn codex_idle_pane_is_not_marked_active() {
        let store = store();
        let w = wid(21);
        seed_codex_worker(&store, &w);
        let scan = ScanLoop::new(
            store.clone(),
            vec![Arc::new(ScriptedBackend {
                name: "tmux",
                frame: CODEX_IDLE.into(),
                native: BTreeMap::new(),
                ..Default::default()
            })],
            Some(Arc::new(MockProtocol::new())),
        );
        let r = scan.scan_once().await.unwrap();
        assert_eq!(r.scanned, vec![w.to_string()]);
        // The idle model bar emits idle_prompt (Waiting), never Active.
        assert!(
            !matches!(worker_state(&store, &w), WorkerState::Active { .. }),
            "an idle codex model bar must not read Active"
        );
    }

    // ---- /api/debug/scan publish (AF-80) -----------------------------------

    #[tokio::test]
    async fn scan_pass_publishes_state_for_the_debug_endpoint() {
        // A completed pass must leave a trace in last_scan_state(), the state
        // GET /api/debug/scan reads, or a demotion is indistinguishable from a
        // scan that found nothing (ethos rule 4, the D1 "scan" deviation). This
        // fails if the publish is removed from scan_once: with no test publishing,
        // the global stays None across the binary and the expect() below fires.
        let store = store();
        let w = wid(30);
        seed_terminal_worker(&store, &w);
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(w.clone(), AgentState::Idle); // structured = demoted
        let scan = ScanLoop::new(
            store,
            vec![Arc::new(ScriptedBackend {
                name: "tmux",
                frame: LIMIT_FRAME.into(),
                native: BTreeMap::new(),
                ..Default::default()
            })],
            Some(protocol),
        );
        let _ = scan.scan_once().await.unwrap();
        let published = last_scan_state().expect("scan_once must publish the pass");
        assert!(
            published.last_pass_at.is_some(),
            "a completed pass must stamp last_pass_at, got {published:?}"
        );
        // Content (which lane was demoted) is asserted on the RETURNED report by
        // the tests above; the global is asserted only for its wiring, because a
        // peer test's pass can overwrite it between this publish and read.
    }
}
