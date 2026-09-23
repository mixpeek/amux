//! Session bootstrap: the spawn/registration glue between the durable
//! records the API writes and the processes/protocol sessions that make them
//! real.
//!
//! WHY THIS EXISTS (found 2026-08-09, herdr+opencode verification): the
//! worker API writes honest durable records — `start` = worker `Starting` +
//! a live `_amux_sessions` row, `stop` = row ended as `Killed` — and its own
//! docstring says "the actual process spawn is the orchestrator's job
//! (RR-0041)". But no code anywhere called `SessionBackend::spawn`
//! or `StructuredCliProtocol::register` outside tests, so on the live
//! server: no started worker ever got a terminal process, and the command
//! pump's `protocol.state()` returned `NoSession` for every worker — every
//! `AtTurnBoundary` message stayed Queued forever. This module closes that
//! loop. It lives beside the backends (not in `orchestrator/`, which is
//! mid-edit by other lanes); when RR-0041's runtime work lands, fold
//! `pass_once` into the tick and delete this file's loop.
//!
//! One pass does three sweeps, each returning what it did in a
//! [`BootstrapReport`] — a skip that leaves no trace is indistinguishable
//! from a pass that found nothing (ethos rule 4):
//!
//! 1. **Spawn**: `Starting` workers with a live session row get their
//!    terminal process spawned via the session's named backend
//!    (`ProviderAdapter::build_command(Interactive)` -> `SessionSpec`), the
//!    session's `pid` recorded, the worker flipped to `Idle`, and — when the
//!    provider has a structured CLI shape — a protocol session registered.
//!    A spawn that cannot happen (backend not configured, unknown provider,
//!    spawn error) flips the worker to `Error { detail }`: visible and
//!    terminal, not an endless silent retry.
//! 2. **Adopt**: live-session workers past `Starting` that the protocol
//!    does not know (this process restarted; backend processes survive a
//!    server restart) get re-registered, never re-spawned.
//! 3. **Reap**: backend-hosted `amux-*` refs whose LATEST session row in
//!    OUR database is ended get terminated — `stop` becomes real. A hosted
//!    ref with NO row in our database is FOREIGN (another amux instance,
//!    a probe, a human's workspace that happens to match the prefix) and is
//!    never touched: this store only reaps what this store minted (ethos
//!    rule 8 — verified against the live specimen `amux-herdr-probe`).

use std::sync::Arc;

use amux_core::ids::WorkerId;
use amux_core::worker::WorkerState;
use rusqlite::params;

use crate::db::queries::{self};
use crate::db::{PendingEvent, SharedStore, WriteOutcome};
use crate::opencode::structured::{
    CliProvider, StructuredCliProtocol, WorkerConfig as ProtocolWorkerConfig,
};
use crate::provider::{PromptMode, ProviderRegistry};

use super::{ProcessRef, SessionBackend, SessionSpec};

/// The one thing bootstrap needs from the protocol: registration. A trait so
/// tests can record registrations without spawning anything.
pub trait ProtocolRegistrar: Send + Sync {
    fn is_registered(&self, worker: &WorkerId) -> bool;
    fn register_worker(&self, worker: WorkerId, config: ProtocolWorkerConfig);
}

impl ProtocolRegistrar for StructuredCliProtocol {
    fn is_registered(&self, worker: &WorkerId) -> bool {
        StructuredCliProtocol::is_registered(self, worker)
    }
    fn register_worker(&self, worker: WorkerId, config: ProtocolWorkerConfig) {
        self.register(worker, config);
    }
}

/// What one pass did. Serialized into logs; tests assert on it directly.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct BootstrapReport {
    /// backend_refs spawned this pass.
    pub spawned: Vec<String>,
    /// worker ids registered with the structured protocol this pass.
    pub registered: Vec<String>,
    /// backend_refs terminated because their latest session row is ended.
    pub terminated: Vec<String>,
    /// worker ids flipped to Error, with the reason.
    pub errored: Vec<String>,
}

impl BootstrapReport {
    pub fn is_empty(&self) -> bool {
        self.spawned.is_empty()
            && self.registered.is_empty()
            && self.terminated.is_empty()
            && self.errored.is_empty()
    }
}

pub struct Bootstrap {
    pub store: SharedStore,
    pub backends: Vec<Arc<dyn SessionBackend>>,
    pub registry: Arc<ProviderRegistry>,
    pub registrar: Arc<dyn ProtocolRegistrar>,
}

impl Bootstrap {
    fn backend(&self, name: &str) -> Option<&Arc<dyn SessionBackend>> {
        self.backends.iter().find(|b| b.name() == name)
    }

    /// Flip a worker's state and journal the transition with a post-mutation
    /// snapshot (RR-0111a — a payload-less worker event would leave replay
    /// permanently blind to this worker).
    async fn set_worker_state(
        &self,
        worker_id: &str,
        from_tag: &str,
        state: WorkerState,
    ) -> anyhow::Result<()> {
        let wid = worker_id.to_string();
        let from = from_tag.to_string();
        let to = serde_json::to_value(&state)
            .ok()
            .and_then(|v| v.get("state").and_then(|s| s.as_str()).map(str::to_string))
            .unwrap_or_else(|| "unknown".into());
        self.store
            .write_async(move |conn| {
                let now = chrono::Utc::now().to_rfc3339();
                let n = queries::update_worker_state(conn, &wid, &state, &now)?;
                let payload = if n > 0 {
                    queries::get_worker(conn, &wid)?.map(|r| r.snapshot())
                } else {
                    None
                };
                Ok(WriteOutcome {
                    applied: n > 0,
                    events: if n > 0 {
                        vec![PendingEvent {
                            entity_type: amux_core::revision::EntityType::Worker,
                            entity_id: wid.clone(),
                            mutation: amux_core::revision::MutationKind::StatusChanged {
                                from: from.clone(),
                                to: to.clone(),
                            },
                            payload,
                        }]
                    } else {
                        vec![]
                    },
                })
            })
            .await?;
        Ok(())
    }

    async fn mark_error(
        &self,
        report: &mut BootstrapReport,
        worker_id: &str,
        detail: String,
    ) -> anyhow::Result<()> {
        tracing::warn!(worker = worker_id, detail = %detail, "bootstrap: worker errored");
        report.errored.push(format!("{worker_id}: {detail}"));
        self.set_worker_state(worker_id, "starting", WorkerState::Error { detail })
            .await
    }

    /// Register a protocol session for a worker when its provider has a
    /// structured CLI shape. A provider without one (ollama) is skipped
    /// honestly: the worker stays terminal-hosted and the pump keeps seeing
    /// NoSession — never a fake registration that would accept prompts into
    /// a void.
    ///
    /// Registration HYDRATES the worker's persisted conversation ref
    /// (`_amux_conversations`, migration 0011) so continuity survives a
    /// server restart — without this, every restart silently amnesia'd the
    /// fleet (AMUX-2613 gap 2). A ref stored under a DIFFERENT provider
    /// family is left behind, never replayed into the wrong CLI.
    fn register_protocol(&self, report: &mut BootstrapReport, row: &queries::WorkerRow) {
        let Ok(worker) = WorkerId::parse(&row.id) else {
            return;
        };
        if self.registrar.is_registered(&worker) {
            return;
        }
        let Some(cli) = CliProvider::from_provider_id(&row.provider) else {
            return;
        };
        let conversation = self.persisted_conversation(&row.id, cli);
        self.registrar.register_worker(
            worker,
            ProtocolWorkerConfig {
                provider: cli,
                cwd: std::path::PathBuf::from(&row.cwd),
                binary: None,
                model: row.model.clone(),
                conversation,
            },
        );
        report.registered.push(row.id.clone());
    }

    /// The worker's persisted conversation ref, if its provider family
    /// still matches. Best-effort: a read failure hydrates nothing (a fresh
    /// conversation is always a safe start), it never blocks registration.
    fn persisted_conversation(&self, worker_id: &str, cli: CliProvider) -> Option<String> {
        let conn = self.store.read().ok()?;
        let (provider, cref): (String, String) = conn
            .query_row(
                "SELECT provider, conversation_ref FROM _amux_conversations WHERE worker_id = ?1",
                params![worker_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()?;
        (CliProvider::from_provider_id(&provider) == Some(cli)).then_some(cref)
    }

    /// One pass over the store + backends. See module docs for the sweeps.
    pub async fn pass_once(&self) -> anyhow::Result<BootstrapReport> {
        let mut report = BootstrapReport::default();

        // ---- sweep 1: spawn Starting workers --------------------------------
        let starting: Vec<(String, String, String, String)> = {
            let conn = self.store.read()?;
            let mut stmt = conn.prepare(
                "SELECT w.id, s.id, s.backend, s.backend_ref
                 FROM _amux_workers w
                 JOIN _amux_sessions s ON s.worker_id = w.id AND s.ended_at IS NULL
                 WHERE json_extract(w.state, '$.state') = 'starting'",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        for (worker_id, session_id, backend_name, backend_ref) in starting {
            let row = {
                let conn = self.store.read()?;
                queries::get_worker(&conn, &worker_id)?
            };
            let Some(row) = row else { continue };
            let lock = crate::api::workers::lifecycle_lock(&row.display_name);
            let _guard = lock.lock().await;
            // Re-read under the lifecycle lock: the Starting scan can predate Pause.
            let row = {
                let conn = self.store.read()?;
                queries::get_worker(&conn, &worker_id)?
            };
            let Some(row) =
                row.filter(|r| r.lifecycle.can_start() && matches!(r.state, WorkerState::Starting))
            else {
                continue;
            };
            let Ok(worker) = WorkerId::parse(&row.id) else {
                continue;
            };

            let Some(backend) = self.backend(&backend_name) else {
                self.mark_error(
                    &mut report,
                    &row.id,
                    format!(
                        "backend '{backend_name}' is not available on this server \
                         (herdr requires AMUX_HERDR_SESSION)"
                    ),
                )
                .await?;
                continue;
            };
            let Some(adapter) = self.registry.resolve(&row.provider) else {
                self.mark_error(
                    &mut report,
                    &row.id,
                    format!("provider '{}' has no registered adapter", row.provider),
                )
                .await?;
                continue;
            };

            let mut command = adapter.build_command(PromptMode::Interactive);
            if row
                .permissions
                .iter()
                .any(|permission| permission == "unsafe" || permission == "claude:skip_permissions")
                && command.first().is_some_and(|binary| binary == "claude")
            {
                tracing::warn!(
                    worker = %row.id,
                    permission = "claude:skip_permissions",
                    "worker explicitly enabled Claude's unsafe permission bypass"
                );
                command.push("--dangerously-skip-permissions".into());
            }
            let spec = SessionSpec {
                worker: worker.clone(),
                command,
                cwd: row.cwd.clone(),
                // Worker-scope env only for now; the four-tier scope
                // assembly (amux-core scope) wires in with RR-0040.
                env: row.environment.clone(),
                // Display metadata for backends that can show it (herdr
                // workspace tokens) — the backend ref stays id-derived.
                human_label: Some(row.display_name.clone()).filter(|n| !n.trim().is_empty()),
            };
            match backend.spawn(&spec).await {
                Ok(proc) => {
                    tracing::info!(
                        worker = %worker, backend = backend_name.as_str(),
                        backend_ref = %proc.backend_ref, pid = ?proc.pid,
                        "bootstrap: spawned terminal session"
                    );
                    let sid = session_id.clone();
                    let pid = proc.pid;
                    self.store
                        .write_async(move |conn| {
                            conn.execute(
                                "UPDATE _amux_sessions SET pid = ?2 WHERE id = ?1",
                                params![sid, pid],
                            )?;
                            // pid is bookkeeping on an already-announced
                            // session row; no event of its own.
                            Ok(WriteOutcome {
                                applied: true,
                                events: vec![],
                            })
                        })
                        .await?;
                    self.set_worker_state(
                        &row.id,
                        "starting",
                        WorkerState::Idle {
                            since: chrono::Utc::now(),
                        },
                    )
                    .await?;
                    self.register_protocol(&mut report, &row);
                    report.spawned.push(backend_ref.clone());
                }
                Err(e) => {
                    // Includes the duplicate-ref case (a previous pass
                    // spawned but crashed before the state write): Error is
                    // the honest terminal — a human restarts the worker,
                    // and reap (sweep 3) clears the orphan once its row ends.
                    self.mark_error(&mut report, &row.id, format!("spawn failed: {e}"))
                        .await?;
                }
            }
        }

        // ---- sweep 2: adopt live workers the protocol lost (restart) --------
        let live_ids: Vec<String> = {
            let conn = self.store.read()?;
            let mut stmt = conn.prepare(
                "SELECT w.id FROM _amux_workers w
                 JOIN _amux_sessions s ON s.worker_id = w.id AND s.ended_at IS NULL
                 WHERE json_extract(w.state, '$.state') IN ('idle', 'active', 'waiting')",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        };
        for id in live_ids {
            let row = {
                let conn = self.store.read()?;
                queries::get_worker(&conn, &id)?
            };
            if let Some(row) = row {
                self.register_protocol(&mut report, &row);
            }
        }

        // ---- sweep 3: reap backend refs whose latest session is ended -------
        for backend in &self.backends {
            let hosted = match backend.reconcile().await {
                Ok(h) => h,
                Err(e) => {
                    // Cannot answer != empty; skip this backend, loudly.
                    tracing::warn!(backend = backend.name(), error = %e,
                        "bootstrap: reconcile failed; skipping reap for this backend");
                    continue;
                }
            };
            for session in hosted {
                let latest_ended: Option<bool> = {
                    let conn = self.store.read()?;
                    conn.query_row(
                        "SELECT ended_at IS NOT NULL FROM _amux_sessions
                         WHERE backend = ?1 AND backend_ref = ?2
                         ORDER BY started_at DESC, id DESC LIMIT 1",
                        params![backend.name(), session.backend_ref],
                        |r| r.get(0),
                    )
                    .ok()
                };
                match latest_ended {
                    // FOREIGN ref (no row in OUR store): never touch it —
                    // this store only reaps what this store minted (ethos
                    // rule 8; tmux reconcile sees the whole machine's
                    // amux-* sessions, most of which are the Python fleet).
                    None => continue,
                    Some(false) => continue, // live, exactly as recorded
                    Some(true) => {
                        let proc = ProcessRef {
                            backend_ref: session.backend_ref.clone(),
                            pid: None,
                        };
                        match backend.terminate(&proc).await {
                            Ok(()) => {
                                tracing::info!(backend = backend.name(),
                                    backend_ref = %session.backend_ref,
                                    "bootstrap: reaped ended session's process");
                                report.terminated.push(session.backend_ref.clone());
                            }
                            Err(e) => tracing::warn!(backend = backend.name(),
                                backend_ref = %session.backend_ref, error = %e,
                                "bootstrap: reap failed"),
                        }
                    }
                }
            }
        }

        Ok(report)
    }

    /// The loop: one pass every `interval_secs`, forever.
    pub async fn run(self: Arc<Self>, interval_secs: u64) {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(interval_secs.max(1)));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            // AMUX-4828: bracket the pass; the one-shot records no duration.
            crate::runtime_jobs::registry::tick_start(
                crate::runtime_jobs::registry::ids::BOOTSTRAP,
            );
            match self.pass_once().await {
                Ok(r) if !r.is_empty() => {
                    tracing::info!(report = %serde_json::to_string(&r).unwrap_or_default(),
                        "bootstrap pass");
                    // BOTH Ok arms stamp. Stamping only the empty-report arm
                    // would leave a pass that actually did work unreported, so
                    // the busiest passes would be the ones reading as stalled.
                    crate::runtime_jobs::registry::tick_end(
                        crate::runtime_jobs::registry::ids::BOOTSTRAP,
                    );
                }
                Ok(_) => {
                    crate::runtime_jobs::registry::tick_end(
                        crate::runtime_jobs::registry::ids::BOOTSTRAP,
                    );
                }
                Err(e) => tracing::warn!(error = %e, "bootstrap pass failed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{backend_ref, AttachInfo, BackendError, BackendSession, BackendStatus};
    use crate::db::queries::SessionRow;
    use amux_core::session::ExitReason;
    use amux_core::worker::WorkerConfig as CoreWorkerConfig;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Backend double that records spawns/terminates and reports a scripted
    /// hosted set.
    #[derive(Default)]
    struct FakeBackend {
        hosted: Mutex<Vec<String>>,
        spawns: Mutex<Vec<SessionSpec>>,
        terminated: Mutex<Vec<String>>,
        fail_spawn: bool,
    }

    #[async_trait]
    impl SessionBackend for FakeBackend {
        fn name(&self) -> &'static str {
            "herdr"
        }
        async fn spawn(&self, s: &SessionSpec) -> crate::backend::Result<ProcessRef> {
            if self.fail_spawn {
                return Err(BackendError::SpawnFailed("scripted failure".into()));
            }
            self.spawns.lock().unwrap().push(s.clone());
            Ok(ProcessRef {
                backend_ref: backend_ref(&s.worker),
                pid: Some(4242),
            })
        }
        async fn terminate(&self, p: &ProcessRef) -> crate::backend::Result<()> {
            self.terminated.lock().unwrap().push(p.backend_ref.clone());
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
            Ok(self
                .hosted
                .lock()
                .unwrap()
                .iter()
                .map(|r| BackendSession {
                    backend_ref: r.clone(),
                    status: BackendStatus::Running,
                })
                .collect())
        }
        async fn capture(&self, _p: &ProcessRef, _l: u32) -> crate::backend::Result<String> {
            Ok(String::new())
        }
    }

    #[derive(Default)]
    struct RecordingRegistrar {
        registered: Mutex<Vec<(WorkerId, ProtocolWorkerConfig)>>,
    }

    impl ProtocolRegistrar for RecordingRegistrar {
        fn is_registered(&self, worker: &WorkerId) -> bool {
            self.registered
                .lock()
                .unwrap()
                .iter()
                .any(|(w, _)| w == worker)
        }
        fn register_worker(&self, worker: WorkerId, config: ProtocolWorkerConfig) {
            self.registered.lock().unwrap().push((worker, config));
        }
    }

    fn store() -> (SharedStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let s = Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        (s, dir)
    }

    /// Seed a worker (+ optionally a live session) in the given state.
    fn seed(
        store: &SharedStore,
        state: WorkerState,
        backend: &str,
        provider: &str,
        with_session: bool,
    ) -> (WorkerId, String) {
        let id = WorkerId::from_ulid(ulid::Ulid::new());
        let bref = backend_ref(&id);
        let config = CoreWorkerConfig {
            display_name: format!("w-{id}"),
            name_aliases: vec![],
            cwd: "/tmp/bootstrap-test-cwd".into(),
            provider: amux_core::provider::ProviderId::new(provider),
            model: Some("haiku".into()),
            backend: amux_core::session::BackendId::new(backend),
            environment: [("FOO".to_string(), "bar".to_string())].into(),
            permissions: vec![],
            group: None,
        };
        let now = chrono::Utc::now().to_rfc3339();
        let row = queries::WorkerRow::new(&id, &config, &now);
        let (idc, brefc, backendc, nowc) = (id.clone(), bref.clone(), backend.to_string(), now);
        store
            .write(move |conn| {
                queries::insert_worker(conn, &row)?;
                queries::update_worker_state(conn, row.id.as_str(), &state, &nowc)?;
                if with_session {
                    queries::insert_session(
                        conn,
                        &SessionRow {
                            id: format!("ses_{idc}"),
                            worker_id: idc.to_string(),
                            backend: backendc,
                            backend_ref: brefc,
                            pid: None,
                            started_at: nowc.clone(),
                            ended_at: None,
                            exit_reason: None,
                        },
                    )?;
                }
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .unwrap();
        (id, bref)
    }

    fn end_session(store: &SharedStore, worker: &WorkerId) {
        let sid = format!("ses_{worker}");
        store
            .write(move |conn| {
                queries::end_session(
                    conn,
                    &sid,
                    &ExitReason::Killed,
                    &chrono::Utc::now().to_rfc3339(),
                )?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .unwrap();
    }

    fn worker_state(store: &SharedStore, worker: &WorkerId) -> WorkerState {
        let conn = store.read().unwrap();
        queries::get_worker(&conn, worker.as_str())
            .unwrap()
            .unwrap()
            .state
    }

    fn bootstrap(
        store: SharedStore,
        backend: Arc<FakeBackend>,
        registrar: Arc<RecordingRegistrar>,
    ) -> Bootstrap {
        Bootstrap {
            store,
            backends: vec![backend],
            registry: Arc::new(crate::provider::default_registry()),
            registrar,
        }
    }

    #[tokio::test]
    async fn starting_worker_spawns_registers_and_goes_idle() {
        let (store, _dir) = store();
        // Provider "claude" — the SCHEMA DEFAULT spelling, deliberately:
        // this test fails if the registry alias or the CliProvider mapping
        // loses it.
        let (id, bref) = seed(&store, WorkerState::Starting, "herdr", "claude", true);
        let backend = Arc::new(FakeBackend::default());
        let registrar = Arc::new(RecordingRegistrar::default());
        let boot = bootstrap(store.clone(), backend.clone(), registrar.clone());

        let report = boot.pass_once().await.unwrap();
        assert_eq!(report.spawned, vec![bref]);
        assert_eq!(report.errored, Vec::<String>::new());

        // The spawn carried the provider's interactive argv, the worker's
        // cwd and env.
        let spawns = backend.spawns.lock().unwrap();
        assert_eq!(spawns.len(), 1);
        assert_eq!(spawns[0].command, vec!["claude"]);
        assert_eq!(spawns[0].cwd, "/tmp/bootstrap-test-cwd");
        assert_eq!(spawns[0].env.get("FOO").map(String::as_str), Some("bar"));
        // Display name rides along for backend metadata (herdr workspace
        // tokens, AMUX-2613 gap 5) — display-only, never the ref.
        assert_eq!(
            spawns[0].human_label.as_deref(),
            Some(format!("w-{id}").as_str())
        );

        // pid recorded on the session row.
        let pid: Option<i64> = {
            let conn = store.read().unwrap();
            conn.query_row(
                "SELECT pid FROM _amux_sessions WHERE worker_id = ?1",
                params![id.as_str()],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(pid, Some(4242));

        // Worker Idle; protocol registered with the mapped CLI + model.
        assert!(matches!(
            worker_state(&store, &id),
            WorkerState::Idle { .. }
        ));
        let regs = registrar.registered.lock().unwrap();
        assert_eq!(regs.len(), 1);
        assert_eq!(regs[0].0, id);
        assert_eq!(regs[0].1.provider, CliProvider::ClaudeCode);
        assert_eq!(regs[0].1.model.as_deref(), Some("haiku"));
    }

    #[tokio::test]
    async fn claude_permission_bypass_requires_an_explicit_worker_permission() {
        let (store, _dir) = store();
        let (id, _) = seed(&store, WorkerState::Starting, "herdr", "claude", true);
        let worker_id = id.to_string();
        store
            .write(move |conn| {
                conn.execute(
                    "UPDATE _amux_workers SET permissions='[\"claude:skip_permissions\"]' WHERE id=?1",
                    params![worker_id],
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let backend = Arc::new(FakeBackend::default());
        let registrar = Arc::new(RecordingRegistrar::default());
        let boot = bootstrap(store, backend.clone(), registrar);

        boot.pass_once().await.unwrap();

        let spawns = backend.spawns.lock().unwrap();
        assert_eq!(
            spawns[0].command,
            vec!["claude", "--dangerously-skip-permissions"]
        );
    }

    #[tokio::test]
    async fn missing_backend_marks_error_not_silent_retry() {
        let (store, _dir) = store();
        // Session says tmux; only the herdr fake is configured.
        let (id, _) = seed(&store, WorkerState::Starting, "tmux", "claude", true);
        let backend = Arc::new(FakeBackend::default());
        let registrar = Arc::new(RecordingRegistrar::default());
        let boot = bootstrap(store.clone(), backend.clone(), registrar.clone());

        let report = boot.pass_once().await.unwrap();
        assert_eq!(report.spawned, Vec::<String>::new());
        assert_eq!(report.errored.len(), 1);
        assert!(report.errored[0].contains("tmux"), "{:?}", report.errored);
        match worker_state(&store, &id) {
            WorkerState::Error { detail } => assert!(detail.contains("tmux")),
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(registrar.registered.lock().unwrap().is_empty());
        // A second pass does nothing more (Error is terminal for bootstrap).
        let again = boot.pass_once().await.unwrap();
        assert!(again.is_empty(), "{again:?}");
    }

    #[tokio::test]
    async fn spawn_failure_marks_error() {
        let (store, _dir) = store();
        let (id, _) = seed(&store, WorkerState::Starting, "herdr", "claude", true);
        let backend = Arc::new(FakeBackend {
            fail_spawn: true,
            ..Default::default()
        });
        let registrar = Arc::new(RecordingRegistrar::default());
        let boot = bootstrap(store.clone(), backend, registrar);
        let report = boot.pass_once().await.unwrap();
        assert_eq!(report.errored.len(), 1);
        assert!(report.errored[0].contains("scripted failure"));
        assert!(matches!(
            worker_state(&store, &id),
            WorkerState::Error { .. }
        ));
    }

    #[tokio::test]
    async fn reap_terminates_only_refs_this_store_minted() {
        let (store, _dir) = store();
        // Mine: ended session whose process the backend still hosts.
        let (mine, my_ref) = seed(&store, WorkerState::Stopped, "herdr", "claude", true);
        end_session(&store, &mine);
        // Live sibling: must NOT be reaped.
        let (_live, live_ref) = seed(
            &store,
            WorkerState::Idle {
                since: chrono::Utc::now(),
            },
            "herdr",
            "claude",
            true,
        );

        let backend = Arc::new(FakeBackend::default());
        // The backend hosts: my ended ref, the live ref, and a FOREIGN
        // amux-prefixed ref with no row in this store — the exact specimen
        // observed live in the fleet's herdr session ("amux-herdr-probe").
        *backend.hosted.lock().unwrap() = vec![
            my_ref.clone(),
            live_ref.clone(),
            "amux-herdr-probe".to_string(),
        ];
        let registrar = Arc::new(RecordingRegistrar::default());
        let boot = bootstrap(store.clone(), backend.clone(), registrar);

        let report = boot.pass_once().await.unwrap();
        assert_eq!(report.terminated, vec![my_ref.clone()]);
        assert_eq!(
            *backend.terminated.lock().unwrap(),
            vec![my_ref],
            "reap must touch ONLY the ref whose ended row this store holds"
        );
    }

    #[tokio::test]
    async fn restart_adopts_live_worker_without_respawning() {
        let (store, _dir) = store();
        let (id, _) = seed(
            &store,
            WorkerState::Idle {
                since: chrono::Utc::now(),
            },
            "herdr",
            "claude",
            true,
        );
        let backend = Arc::new(FakeBackend::default());
        let registrar = Arc::new(RecordingRegistrar::default());
        let boot = bootstrap(store.clone(), backend.clone(), registrar.clone());

        let report = boot.pass_once().await.unwrap();
        assert_eq!(report.registered, vec![id.to_string()]);
        assert!(
            backend.spawns.lock().unwrap().is_empty(),
            "adopt must not respawn"
        );
        // Idempotent: the second pass re-registers nothing.
        let again = boot.pass_once().await.unwrap();
        assert!(again.registered.is_empty(), "{again:?}");
    }

    fn seed_conversation(store: &SharedStore, worker: &WorkerId, provider: &str, cref: &str) {
        let (w, p, c) = (worker.to_string(), provider.to_string(), cref.to_string());
        store
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO _amux_conversations (worker_id, provider, conversation_ref, updated_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![w, p, c, chrono::Utc::now().to_rfc3339()],
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
    }

    /// AMUX-2613 gap 2, restart half: adoption must hydrate the persisted
    /// conversation ref into the protocol registration — and must NOT
    /// replay a ref stored under a different provider family.
    #[tokio::test]
    async fn adoption_hydrates_the_persisted_conversation_ref() {
        let (store, _dir) = store();
        let (id, _) = seed(
            &store,
            WorkerState::Idle {
                since: chrono::Utc::now(),
            },
            "herdr",
            "claude",
            true,
        );
        // Stored under the canonical family (what the sink writes) while the
        // row says "claude" — the family mapping must bridge the spellings.
        seed_conversation(&store, &id, "claude-code", "conv-abc");
        // A sibling whose stored family no longer matches its row provider.
        let (mismatched, _) = seed(
            &store,
            WorkerState::Idle {
                since: chrono::Utc::now(),
            },
            "herdr",
            "gemini",
            true,
        );
        seed_conversation(&store, &mismatched, "codex", "wrong-family-ref");

        let backend = Arc::new(FakeBackend::default());
        let registrar = Arc::new(RecordingRegistrar::default());
        let boot = bootstrap(store.clone(), backend, registrar.clone());
        boot.pass_once().await.unwrap();

        let regs = registrar.registered.lock().unwrap();
        let of = |w: &WorkerId| {
            regs.iter()
                .find(|(rw, _)| rw == w)
                .map(|(_, c)| c.conversation.clone())
                .expect("worker registered")
        };
        assert_eq!(of(&id).as_deref(), Some("conv-abc"));
        assert_eq!(
            of(&mismatched),
            None,
            "a ref from another provider family must not be replayed"
        );
    }

    #[tokio::test]
    async fn provider_without_structured_cli_is_hosted_but_not_registered() {
        // Build a registry that includes a non-structured adapter ("bare-repl")
        // to exercise the "terminal-hosted but not protocol-registered" path.
        // ("ollama" was the representative before 2026-08-15; it now uses
        // `codex --oss --local-provider ollama` and IS a structured CLI.)
        #[derive(Debug)]
        struct BareReplAdapter;
        #[async_trait::async_trait]
        impl crate::provider::ProviderAdapter for BareReplAdapter {
            fn id(&self) -> amux_core::provider::ProviderId {
                amux_core::provider::ProviderId::new("bare-repl")
            }
            fn capabilities(&self) -> amux_core::provider::ProviderCapabilities {
                amux_core::provider::ProviderCapabilities::default() // all false
            }
            async fn usage(&self) -> amux_core::provider::ProviderUsage {
                amux_core::provider::ProviderUsage::unknown(self.id())
            }
            async fn models(&self) -> Vec<String> {
                vec![]
            }
            fn build_command(&self, _m: crate::provider::PromptMode) -> Vec<String> {
                vec!["bare-repl".into()]
            }
        }
        let mut reg = crate::provider::default_registry();
        reg.register(std::sync::Arc::new(BareReplAdapter));

        let (store, _dir) = store();
        let (id, bref) = seed(&store, WorkerState::Starting, "herdr", "bare-repl", true);
        let backend = Arc::new(FakeBackend::default());
        let registrar = Arc::new(RecordingRegistrar::default());
        let boot = Bootstrap {
            store: store.clone(),
            backends: vec![backend],
            registry: std::sync::Arc::new(reg),
            registrar: registrar.clone(),
        };

        let report = boot.pass_once().await.unwrap();
        // Terminal hosting is real...
        assert_eq!(report.spawned, vec![bref]);
        assert!(matches!(
            worker_state(&store, &id),
            WorkerState::Idle { .. }
        ));
        // ...but no protocol session is faked for a provider the protocol
        // cannot drive.
        assert!(registrar.registered.lock().unwrap().is_empty());
        assert!(report.registered.is_empty());
    }
}
