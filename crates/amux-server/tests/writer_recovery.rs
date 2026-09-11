//! A failed mutation must not permanently disable writes or leave health green.
use amux_server::db::{Store, WriteOutcome, PendingEvent};
use amux_core::revision::{EntityType, MutationKind};
use std::sync::Arc;

fn outcome(applied: bool) -> WriteOutcome { WriteOutcome { applied, events: vec![] } }
fn store() -> (tempfile::TempDir, Arc<Store>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("recovery.db")).unwrap());
    store.write(|c| { c.execute_batch("CREATE TABLE recovery_marker (id INTEGER PRIMARY KEY)")?; Ok(outcome(false)) }).unwrap();
    (dir, store)
}
fn count(store: &Store) -> i64 {
    store.read().unwrap().query_row("SELECT COUNT(*) FROM recovery_marker", [], |r| r.get(0)).unwrap()
}

#[test]
fn mutation_panic_rolls_back_and_the_next_write_succeeds() {
    let (_dir, store) = store();
    let error = store.write(|c| {
        c.execute("INSERT INTO recovery_marker VALUES (1)", [])?;
        panic!("injected mutation panic");
    });
    assert!(error.is_err());
    assert_eq!(count(&store), 0, "panic must roll back the partial mutation");
    store.write(|c| { c.execute("INSERT INTO recovery_marker VALUES (2)", [])?; Ok(outcome(true)) }).expect("writer must survive a failed mutation");
    assert_eq!(count(&store), 1);
}

#[test]
fn journal_failure_rolls_back_the_mutation_and_releases_the_transaction() {
    let (_dir, store) = store();
    store.write(|c| {
        c.execute_batch("CREATE TRIGGER reject_recovery_event BEFORE INSERT ON _amux_state_events BEGIN SELECT RAISE(ABORT, 'injected journal failure'); END")?;
        Ok(outcome(false))
    }).unwrap();
    assert!(store.write(|c| {
        c.execute("INSERT INTO recovery_marker VALUES (1)", [])?;
        Ok(WriteOutcome { applied: true, events: vec![PendingEvent {
            entity_type: EntityType::Other("recovery".into()), entity_id: "1".into(),
            mutation: MutationKind::Updated, payload: None,
        }] })
    }).is_err());
    assert_eq!(count(&store), 0);
    store.write(|c| { c.execute("INSERT INTO recovery_marker VALUES (2)", [])?; Ok(outcome(true)) }).expect("journal failure must release the transaction");
    assert_eq!(count(&store), 1);
}

#[test]
fn commit_failure_rolls_back_and_the_next_write_succeeds() {
    let (_dir, store) = store();
    store.write(|c| {
        c.execute_batch("CREATE TABLE recovery_parent(id INTEGER PRIMARY KEY); CREATE TABLE recovery_child(id INTEGER REFERENCES recovery_parent(id) DEFERRABLE INITIALLY DEFERRED)")?;
        Ok(outcome(false))
    }).unwrap();
    assert!(store.write(|c| {
        c.execute("INSERT INTO recovery_marker VALUES (1)", [])?;
        c.execute("INSERT INTO recovery_child VALUES (999)", [])?;
        Ok(outcome(true))
    }).is_err());
    assert_eq!(count(&store), 0);
    store.write(|c| { c.execute("INSERT INTO recovery_marker VALUES (2)", [])?; Ok(outcome(true)) }).expect("failed COMMIT must release the transaction");
    assert_eq!(count(&store), 1);
}

#[tokio::test]
async fn health_refuses_a_store_that_can_read_but_cannot_write() {
    let (_dir, store) = store();
    store.write(|c| { c.pragma_update(None, "query_only", "ON")?; Ok(outcome(false)) }).unwrap();
    assert!(store.read().is_ok(), "positive read control");
    let state = amux_server::api::AppState {
        store, started: std::time::Instant::now(), build_hash: "writer-recovery-test".into(),
        auth_token: None, reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let (status, axum::Json(body)) = amux_server::api::health::health(axum::extract::State(state)).await;
    assert_eq!(status.as_u16(), 503, "readable is not writable");
    assert_ne!(body.store, "ok");
    assert_eq!(body.board.error.as_deref(), Some("writer_probe_failed"));
}

#[tokio::test(flavor = "current_thread")]
async fn stalled_writer_health_is_bounded_and_recovers_without_changing_revision() {
    let (_dir, store) = store();
    let initial = store.current_rev().unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker = store.clone();
    let stalled = std::thread::spawn(move || worker.write(move |_| {
        entered_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        Ok(outcome(false))
    }));
    entered_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    let state = amux_server::api::AppState {
        store: store.clone(), started: std::time::Instant::now(), build_hash: "writer-stall".into(),
        auth_token: None, reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let start = std::time::Instant::now();
    let (status, _) = amux_server::api::health::health(axum::extract::State(state.clone())).await;
    let mut rejected = 0;
    for _ in 0..3 {
        let (code, _) = amux_server::api::health::health(axum::extract::State(state.clone())).await;
        if code.as_u16() == 503 { rejected += 1; }
    }
    release_tx.send(()).unwrap();
    stalled.join().unwrap().unwrap();
    assert_eq!(status.as_u16(), 503);
    assert_eq!(rejected, 3);
    assert!(start.elapsed() < std::time::Duration::from_secs(1), "health must not wait behind a stuck writer");
    for _ in 0..100 {
        let (code, _) = amux_server::api::health::health(axum::extract::State(state.clone())).await;
        if code.as_u16() == 200 {
            assert_eq!(store.current_rev().unwrap(), initial, "health must not generate sync revisions");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("health did not recover after the writer resumed");
}
