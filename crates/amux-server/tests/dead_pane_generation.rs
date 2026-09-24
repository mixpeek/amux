// Regression originally reproduced independently by amux-research (AF-784).
use amux_core::{
    ids::{SessionId, WorkerId},
    protocol::ExitStatus,
    provider::ProviderId,
    session::{BackendId, ExitReason},
    worker::{WorkerConfig, WorkerState},
};
use amux_server::{
    backend::*,
    db::{
        queries::{self, SessionRow, WorkerRow},
        SharedStore, Store, WriteOutcome,
    },
    orchestrator::scan::ScanLoop,
};
use async_trait::async_trait;
use std::{collections::BTreeMap, sync::Arc};

// A process-wide subscriber captures writer-thread diagnostics as well as scan
// task diagnostics. Assertions select a unique worker, so parallel tests cannot
// satisfy each other's positive controls.
#[derive(Clone)]
struct LogWriter(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn logs() -> Arc<std::sync::Mutex<Vec<u8>>> {
    static LOGS: std::sync::OnceLock<Arc<std::sync::Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
    LOGS.get_or_init(|| {
        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = LogWriter(bytes.clone());
        tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .try_init()
            .unwrap();
        bytes
    })
    .clone()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mutation {
    None,
    Replace,
    End,
}
struct RaceBackend {
    store: SharedStore,
    worker: WorkerId,
    mutation: Mutation,
    barrier: Option<Arc<tokio::sync::Barrier>>,
}

#[async_trait]
impl SessionBackend for RaceBackend {
    fn name(&self) -> &'static str {
        "tmux"
    }
    async fn spawn(&self, _: &SessionSpec) -> Result<ProcessRef> {
        unreachable!()
    }
    async fn terminate(&self, _: &ProcessRef) -> Result<()> {
        unreachable!()
    }
    async fn status(&self, _: &ProcessRef) -> Result<BackendStatus> {
        Ok(BackendStatus::Running)
    }
    async fn attach_info(&self, _: &ProcessRef) -> Result<AttachInfo> {
        unreachable!()
    }
    async fn reconcile(&self) -> Result<Vec<BackendSession>> {
        Ok(vec![])
    }
    async fn capture(&self, _: &ProcessRef, _: u32) -> Result<String> {
        Ok(String::new())
    }
    async fn process_exits(&self) -> Result<BTreeMap<String, ExitStatus>> {
        if let Some(barrier) = &self.barrier {
            barrier.wait().await;
        }
        Ok(BTreeMap::from([(
            backend_ref(&self.worker),
            ExitStatus {
                code: Some(1),
                signal: None,
            },
        )]))
    }
    async fn agent_states(&self) -> Result<BTreeMap<String, String>> {
        // Deterministic restart after census, before the scan applies its exit.
        if self.mutation != Mutation::None {
            let replace = self.mutation == Mutation::Replace;
            let worker = self.worker.clone();
            self.store
                .write_async(move |conn| {
                    let now = chrono::Utc::now();
                    let old = queries::live_session_for(conn, worker.as_str())?.unwrap();
                    queries::end_session(conn, &old.id, &ExitReason::Killed, &now.to_rfc3339())?;
                    if replace {
                        queries::insert_session(
                            conn,
                            &SessionRow {
                                id: SessionId::from_ulid(ulid::Ulid::new()).to_string(),
                                worker_id: worker.to_string(),
                                backend: "tmux".into(),
                                backend_ref: backend_ref(&worker),
                                pid: Some(222),
                                started_at: now.to_rfc3339(),
                                ended_at: None,
                                exit_reason: None,
                            },
                        )?;
                    }
                    // Preserve a distinct authoritative state for ended-without-replacement.
                    let state = if replace {
                        WorkerState::Idle { since: now }
                    } else {
                        WorkerState::Error {
                            detail: "already ended by owner".into(),
                        }
                    };
                    queries::update_worker_state(conn, worker.as_str(), &state, &now.to_rfc3339())?;
                    Ok(WriteOutcome {
                        applied: true,
                        events: vec![],
                    })
                })
                .await
                .unwrap();
        }
        Ok(BTreeMap::new())
    }
}

async fn run(mutation: Mutation, concurrent: bool) {
    let logs = logs();
    let dir = tempfile::tempdir().unwrap();
    let store: SharedStore = Arc::new(Store::open(&dir.path().join("test.db")).unwrap());
    let worker = WorkerId::from_ulid(ulid::Ulid::new());
    let w = worker.clone();
    store
        .write_async(move |conn| {
            let now = chrono::Utc::now();
            let config = WorkerConfig {
                worker_type: Default::default(),
                display_name: w.to_string(),
                name_aliases: vec![],
                cwd: "/tmp".into(),
                provider: ProviderId::new("claude"),
                model: None,
                backend: BackendId::tmux(),
                environment: BTreeMap::new(),
                permissions: vec![],
                group: None,
            };
            queries::insert_worker(conn, &WorkerRow::new(&w, &config, &now.to_rfc3339()))?;
            queries::update_worker_state(
                conn,
                w.as_str(),
                &WorkerState::Idle { since: now },
                &now.to_rfc3339(),
            )?;
            queries::insert_session(
                conn,
                &SessionRow {
                    id: SessionId::from_ulid(ulid::Ulid::new()).to_string(),
                    worker_id: w.to_string(),
                    backend: "tmux".into(),
                    backend_ref: backend_ref(&w),
                    pid: Some(111),
                    started_at: now.to_rfc3339(),
                    ended_at: None,
                    exit_reason: None,
                },
            )?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await
        .unwrap();
    let old = queries::live_session_for(&store.read().unwrap(), worker.as_str())
        .unwrap()
        .unwrap()
        .id;
    let backend = Arc::new(RaceBackend {
        store: store.clone(),
        worker: worker.clone(),
        mutation,
        barrier: concurrent.then(|| Arc::new(tokio::sync::Barrier::new(2))),
    });
    let scan = ScanLoop::new(store.clone(), vec![backend.clone()], None);
    let reports = if concurrent {
        let other = ScanLoop::new(store.clone(), vec![backend], None);
        let (a, b) = tokio::join!(scan.scan_once(), other.scan_once());
        vec![a.unwrap(), b.unwrap()]
    } else {
        vec![scan.scan_once().await.unwrap()]
    };
    let applied: usize = reports.iter().map(|r| r.events_applied).sum();
    let exits: usize = reports.iter().map(|r| r.process_exits.len()).sum();
    let stale: Vec<_> = reports
        .iter()
        .flat_map(|r| r.stale_process_exits.values())
        .collect();
    assert_eq!(
        applied, exits,
        "skipped observations cannot count as applied exits"
    );
    assert!(reports.iter().all(|r| r.process_exit_failures.is_empty()));
    for rejected in &stale {
        assert_eq!(rejected.observed_session, old);
        assert_eq!(rejected.backend_ref, backend_ref(&worker));
    }
    let captured = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    let worker_logs: Vec<_> = captured
        .lines()
        .filter(|line| line.contains(worker.as_str()))
        .collect();
    assert_eq!(
        worker_logs
            .iter()
            .filter(|line| line.contains("terminal_process_exit:"))
            .count(),
        exits
    );
    let stale_logs: Vec<_> = worker_logs
        .iter()
        .filter(|line| line.contains("terminal_process_exit_stale:"))
        .collect();
    assert_eq!(stale_logs.len(), stale.len());
    for line in stale_logs {
        assert!(line.contains(&old));
        assert!(
            line.contains("measured=true")
                && line.contains("n_considered=1")
                && line.contains("applied=false")
        );
    }
    let conn = store.read().unwrap();
    let current = queries::live_session_for(&conn, worker.as_str()).unwrap();
    let state = queries::get_worker(&conn, worker.as_str())
        .unwrap()
        .unwrap()
        .state;
    eprintln!("current={current:?} state={state:?} reports={reports:?}");
    if mutation == Mutation::Replace {
        let replacement_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _amux_sessions WHERE worker_id=?1 AND pid=222",
                [worker.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            replacement_count, 1,
            "restart control must actually create a new session"
        );
        assert!(
            current.is_some(),
            "old census must not end the replacement live session"
        );
        let current = current.unwrap();
        assert_ne!(current.id, old);
        assert_eq!(current.pid, Some(222));
        assert!(matches!(state, WorkerState::Idle { .. }));
        assert_eq!(applied, 0);
        assert_eq!(stale.len(), 1);
        assert_eq!(
            stale[0].current_session.as_deref(),
            Some(current.id.as_str())
        );
    } else if mutation == Mutation::End {
        assert!(current.is_none());
        assert!(
            matches!(state, WorkerState::Error { .. }),
            "a stale exit cannot overwrite later state"
        );
        assert_eq!(applied, 0);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].current_session, None);
    } else {
        assert!(current.is_none());
        assert!(matches!(state, WorkerState::Stopped));
        assert_eq!(applied, 1);
        assert_eq!(stale.len(), usize::from(concurrent));
        if concurrent {
            assert_eq!(stale[0].current_session, None);
        }
    }
}

#[tokio::test]
async fn unchanged_session_still_consumes_exit() {
    run(Mutation::None, false).await;
}

#[tokio::test]
async fn replacement_session_survives_stale_exit() {
    run(Mutation::Replace, false).await;
}

#[tokio::test]
async fn already_ended_session_preserves_later_state() {
    run(Mutation::End, false).await;
}

#[tokio::test]
async fn concurrent_scans_apply_exit_only_once() {
    run(Mutation::None, true).await;
}
