//! Resume pending board consequences, never the commands that produced them.
use crate::api::{session_verbs, AppState};
const JOB: &str = super::registry::ids::MESSAGE_CAPTURE;
// The registry's health budget must exceed one permitted recovery attempt.
// A30s cadence had a90s budget and mislabeled bounded120s reads as hung.
const TICK_SECONDS: u64 = 90;
const ATTEMPT_SECONDS: u64 = 120;

#[cfg(test)]
pub(crate) async fn tick(state: &AppState) {
    tick_after(state, 0).await;
}

async fn tick_after(state: &AppState, after: i64) -> i64 {
    let pending = (|| -> anyhow::Result<Vec<i64>> {
        let conn = state.store.read()?;
        let mut stmt = conn.prepare(
            "SELECT id FROM cmd_history WHERE capture_pending!=0 AND project_group IS NULL ORDER BY (id>?1) DESC,id LIMIT 16",
        )?;
        let ids = stmt
            .query_map([after], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    })();
    match pending {
        Ok(ids) => {
            let next = ids.last().copied().unwrap_or(after);
            if !ids.is_empty() {
                tracing::warn!(measured=true, n_considered=ids.len(), message_ids=?ids,
                    verdict="capture_recovery_pending", "ledger: resuming unfinished message captures");
            }
            // Serialize within each lane through board_intake::lock; independent
            // lanes can progress even when another lane's classifier is slow.
            let mut jobs = tokio::task::JoinSet::new();
            for id in ids {
                let state = state.clone();
                jobs.spawn(async move {
                    if tokio::time::timeout(
                        std::time::Duration::from_secs(ATTEMPT_SECONDS),
                        session_verbs::capture_recorded_message(&state, id),
                    )
                    .await
                    .is_err()
                    {
                        tracing::warn!(
                            message_id = id,
                            measured = true,
                            n_considered = 1,
                            verdict = "capture_recovery_timeout",
                            "ledger: capture still pending after bounded retry"
                        );
                    }
                });
            }
            while let Some(result) = jobs.join_next().await {
                if let Err(error) = result {
                    tracing::warn!(%error, measured=false, n_considered=0,
                        verdict="capture_recovery_interrupted", "ledger: capture remains pending for retry");
                }
            }
            next
        }
        Err(error) => {
            tracing::warn!(%error, measured=false, n_considered=0,
            verdict="capture_recovery_unmeasured", "ledger: pending capture population unavailable");
            after
        }
    }
}

pub fn spawn(state: AppState) -> super::PeriodicTask {
    // Rotate across retained pending rows, including failed rows. A poisoned
    // first batch must not starve newer messages. Restart resets only the scan
    // cursor; the pending work itself lives in the database.
    tracing::info!(
        job = JOB,
        interval_s = TICK_SECONDS,
        attempt_timeout_s = ATTEMPT_SECONDS,
        health_budget_s = super::registry::stall_after_s(TICK_SECONDS as f64),
        measured = true,
        n_considered = 1,
        verdict = "capture_recovery_configured",
        "capture recovery cadence includes its bounded attempt budget"
    );
    let cursor = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    super::spawn_periodic(JOB, TICK_SECONDS, move || {
        let state = state.clone();
        let cursor = cursor.clone();
        async move {
            let after = cursor.load(std::sync::atomic::Ordering::Relaxed);
            let next = tick_after(&state, after).await;
            cursor.store(next, std::sync::atomic::Ordering::Relaxed);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::session_verbs::{cmd_hist_record_full, DeliveryMeta};
    use crate::db::WriteOutcome;
    use std::sync::{Arc, Mutex};
    use tracing::instrument::WithSubscriber;

    #[test]
    fn bounded_attempt_fits_the_registered_health_budget() {
        assert!(
            super::super::registry::stall_after_s(TICK_SECONDS as f64)
                > (TICK_SECONDS + ATTEMPT_SECONDS) as f64,
            "normal idle interval plus permitted attempt must fit the health budget"
        );
        assert_eq!(
            super::super::registry::doc_for(JOB).unwrap().name,
            "Message capture recovery"
        );
        let facts = super::super::registry::Facts {
            spawned: true,
            interval_s: Some(TICK_SECONDS as f64),
            spawned_at: Some(0.0),
            last_tick_at: Some(0.0),
            in_flight_since: Some(TICK_SECONDS as f64),
            instrumented: true,
            ..Default::default()
        };
        assert_eq!(
            super::super::registry::classify(&facts, (TICK_SECONDS + ATTEMPT_SECONDS) as f64),
            "ok"
        );
        let deadline =
            TICK_SECONDS as f64 + super::super::registry::stall_after_s(TICK_SECONDS as f64);
        assert_eq!(
            super::super::registry::classify(&facts, deadline + 1.0),
            "hung",
            "a real overrun must still be detected"
        );
    }

    fn fixture() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState {
            store: Arc::new(crate::db::Store::open(&dir.path().join("capture.db")).unwrap()),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        (state, dir)
    }

    // Record emitted event fields directly. This subscriber is always
    // interested, including when other tests first register the same callsite
    // with no subscriber installed on their runtime thread.
    struct Events {
        bytes: Arc<Mutex<Vec<u8>>>,
        // tracing-core 0.1.36's single-dispatch fast path registers a new
        // callsite against the CURRENT thread's default, which can be None in
        // a concurrent fixture. Keep a second, uninstalled dispatch alive so
        // registration considers all collectors instead of caching Never.
        _registration_peer: tracing::Dispatch,
    }
    impl tracing::Subscriber for Events {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn register_callsite(
            &self,
            _: &'static tracing::Metadata<'static>,
        ) -> tracing::subscriber::Interest {
            tracing::subscriber::Interest::always()
        }
        fn max_level_hint(&self) -> Option<tracing::metadata::LevelFilter> {
            Some(tracing::metadata::LevelFilter::TRACE)
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Fields(String);
            impl tracing::field::Visit for Fields {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    use std::fmt::Write;
                    write!(&mut self.0, " {}={value:?}", field.name()).unwrap();
                }
            }
            let mut fields = Fields(String::new());
            event.record(&mut fields);
            fields.0.push('\n');
            self.bytes
                .lock()
                .unwrap()
                .extend_from_slice(fields.0.as_bytes());
        }
    }
    fn subscriber(bytes: Arc<Mutex<Vec<u8>>>) -> Events {
        Events {
            bytes,
            _registration_peer: tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()),
        }
    }

    #[test]
    fn capture_collector_observes_callsites_first_used_on_another_thread() {
        fn emit() {
            tracing::warn!(
                verdict = "capture_collector_control",
                measured = true,
                n_considered = 1
            );
        }
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let _scope = tracing::subscriber::set_default(subscriber(bytes.clone()));
        std::thread::spawn(emit).join().unwrap();
        assert!(
            bytes.lock().unwrap().is_empty(),
            "the uninstrumented thread must not send its event to this collector"
        );
        emit();
        let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(
            log.contains("capture_collector_control"),
            "the registered callsite must not stay disabled: {log}"
        );
    }

    #[tokio::test]
    async fn history_retention_preserves_unfinished_capture_input() {
        let (state, _dir) = fixture();
        state.store.write(|conn| {
            session_verbs::ensure_fleet_tables(conn)?;
            conn.execute("INSERT INTO cmd_history(id,text,type,session,ts,capture_pending) VALUES (1,'Implement retained parser validation','user','retention-fixture',1,1)",[])?;
            for id in 2..=501 {
                conn.execute("INSERT INTO cmd_history(id,text,type,session,ts) VALUES (?1,'/compact','user','retention-fixture',?1)",[id])?;
            }
            Ok(WriteOutcome {applied:true,events:vec![]})
        }).unwrap();
        cmd_hist_record_full(
            &state,
            "retention-fixture",
            "/compact",
            "user",
            "",
            false,
            DeliveryMeta::direct(),
        )
        .await;
        {
            let conn = state.store.read().unwrap();
            assert_eq!(
                conn.query_row(
                    "SELECT capture_pending FROM cmd_history WHERE id=1",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                1,
                "retention must not delete the only durable recovery input"
            );
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM cmd_history", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                201,
                "ordinary history must still be pruned"
            );
        }
        tick(&state).await;
        assert!(state
            .store
            .read()
            .unwrap()
            .query_row("SELECT card_id FROM cmd_history WHERE id=1", [], |r| r
                .get::<_, Option<
                String,
            >>(
                0
            ))
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn failed_first_batch_does_not_starve_a_newer_message() {
        let (state, _dir) = fixture();
        state.store.write(|conn| {
            session_verbs::ensure_fleet_tables(conn)?;
            for id in 1..=17 {
                conn.execute("INSERT INTO cmd_history(id,text,type,session,ts,capture_pending) VALUES (?1,'Implement parser validation','user',?2,1,1)", rusqlite::params![id,format!("rotation-{id}")])?;
            }
            conn.execute_batch("CREATE TRIGGER refuse_first_batch BEFORE UPDATE OF card_id ON cmd_history WHEN OLD.id<=16 BEGIN SELECT RAISE(ABORT,'fixture first batch unavailable'); END;")?;
            Ok(WriteOutcome {applied:true,events:vec![]})
        }).unwrap();
        let next = tick_after(&state, 0).await;
        assert_eq!(next, 16);
        assert_eq!(
            state
                .store
                .read()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM issues", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        tick_after(&state, next).await;
        let conn = state.store.read().unwrap();
        assert!(conn
            .query_row("SELECT card_id FROM cmd_history WHERE id=17", [], |r| {
                r.get::<_, Option<String>>(0)
            })
            .unwrap()
            .is_some());
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM cmd_history WHERE capture_pending=1",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            16
        );
    }

    #[tokio::test]
    async fn failed_link_rolls_back_card_and_retries_once_without_sending() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let _log_scope = tracing::subscriber::set_default(subscriber(bytes.clone()));
        assert!(
            tracing::dispatcher::get_default(|d| d.is::<Events>()),
            "subscriber must actually be installed"
        );
        tracing::warn!(
            verdict = "test_subscriber_control",
            "positive subscriber control"
        );
        assert!(
            !bytes.lock().unwrap().is_empty(),
            "subscriber must capture a positive control before the measured operation"
        );
        let (state, _dir) = fixture();
        state.store.write(|conn| {
            conn.execute_batch("CREATE TRIGGER refuse_capture_link BEFORE UPDATE OF card_id ON cmd_history BEGIN SELECT RAISE(ABORT,'fixture capture link unavailable'); END;")?;
            Ok(WriteOutcome {applied:true, events:vec![]})
        }).unwrap();
        cmd_hist_record_full(
            &state,
            "capture-failure-fixture",
            "Implement retryable parser validation",
            "user",
            "",
            false,
            DeliveryMeta::direct(),
        )
        .await;
        assert!(
            tracing::dispatcher::get_default(|d| d.is::<Events>()),
            "subscriber changed during async operation"
        );
        let snapshot = state
            .store
            .read()
            .unwrap()
            .query_row(
                "SELECT COUNT(*),MAX(capture_pending),MAX(card_id) FROM cmd_history",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            snapshot,
            (1, Some(1), None),
            "positive precondition: failed link must leave its committed input pending"
        );
        let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(
            log.contains("capture_retry_pending")
                && log.contains("message_id=1")
                && log.contains("measured=true")
                && log.contains("n_considered=1"),
            "stored={snapshot:?}, warn_enabled={}, log={log}",
            tracing::enabled!(tracing::Level::WARN)
        );
        {
            let conn = state.store.read().unwrap();
            let row: (i64, Option<String>) = conn
                .query_row(
                    "SELECT capture_pending,card_id FROM cmd_history WHERE id=1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(row, (1, None));
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM issues", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                0,
                "card creation must roll back with its link"
            );
        }
        state
            .store
            .write(|conn| {
                conn.execute_batch("DROP TRIGGER refuse_capture_link")?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .unwrap();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        tokio::join!(
            tick(&state).with_subscriber(subscriber(bytes.clone())),
            session_verbs::capture_recorded_message(&state, 1)
        );
        let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(
            log.contains("capture_recovery_pending") && log.contains("n_considered=1"),
            "{log}"
        );
        let conn = state.store.read().unwrap();
        let row: (i64, i64, Option<String>) = conn
            .query_row(
                "SELECT COUNT(*),capture_pending,card_id FROM cmd_history",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row.0, 1);
        assert_eq!(row.1, 0);
        assert!(row.2.is_some());
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM issues", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM steering_queue", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM session_events WHERE type='task.claimed'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn legacy_cardless_and_control_messages_are_not_reinterpreted() {
        let (state, _dir) = fixture();
        state.store.write(|conn| {
            conn.execute("INSERT INTO cmd_history(text,type,session,ts) VALUES ('Implement old parser validation','user','old',1)", [])?;
            Ok(WriteOutcome {applied:true,events:vec![]})
        }).unwrap();
        cmd_hist_record_full(
            &state,
            "control-fixture",
            "/compact",
            "user",
            "",
            false,
            DeliveryMeta::direct(),
        )
        .await;
        cmd_hist_record_full(
            &state,
            "question-fixture",
            "status?",
            "user",
            "",
            false,
            DeliveryMeta::direct(),
        )
        .await;
        cmd_hist_record_full(
            &state,
            "stuck-fixture",
            "Implement stuck parser validation",
            "user",
            "",
            false,
            DeliveryMeta {
                submit_verdict: Some("stuck"),
                client_meta: None,
                ..DeliveryMeta::direct()
            },
        )
        .await;
        tick(&state).await;
        let conn = state.store.read().unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM cmd_history", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            4
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM cmd_history WHERE card_id IS NOT NULL OR capture_pending!=0",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM issues", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn unreadable_pending_population_is_logged_as_unmeasured() {
        let (state, _dir) = fixture();
        state
            .store
            .write(|conn| {
                conn.execute_batch("DROP TABLE cmd_history")?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .unwrap();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        tick(&state)
            .with_subscriber(subscriber(bytes.clone()))
            .await;
        let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(
            log.contains("capture_recovery_unmeasured")
                && log.contains("measured=false")
                && log.contains("n_considered=0")
                && log.contains("no such table"),
            "{log}"
        );
    }
}
