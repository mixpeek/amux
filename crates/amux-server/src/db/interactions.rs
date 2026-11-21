//! Causality crosses the async-to-writer boundary explicitly. Spawned background
//! work must carry the scope too; a revision window is never evidence of cause.
tokio::task_local! {
    pub static CURRENT: String;
}

pub fn current_id() -> Option<String> {
    CURRENT.try_with(Clone::clone).ok()
}

/// Use when a command hands work to another Tokio task. Task locals otherwise
/// disappear at spawn, which would detach later writes from their cause.
pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let id = current_id();
    tokio::spawn(async move {
        match id {
            Some(id) => CURRENT.scope(id, future).await,
            None => future.await,
        }
    })
}

pub fn spawn_blocking<F, R>(work: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let id = current_id();
    tokio::task::spawn_blocking(move || match id {
        Some(id) => CURRENT.sync_scope(id, work),
        None => work(),
    })
}

/// Run a command's work to completion even if the caller goes away, and
/// return its result to the caller while it is still there.
///
/// Hyper drops a handler's future when the client disconnects: a page reload,
/// a closed tab, the dashboard's own write timeout. A lifecycle verb that types
/// `/exit` into a worker and then cleans up must not stop between the two. A
/// dashboard reload six seconds into a Delete left queue-launch stopped but
/// still registered, its env, meta and log in place and its interaction stuck
/// at `sending` (2026-11-21). The work runs in its own task (carrying the
/// interaction scope, like [`spawn`]), so dropping the caller drops only the
/// wait. Work that outlives its caller logs
/// `verdict="lifecycle_verb_outlived_client"` when it finishes.
pub async fn run_to_completion<F>(verb: &'static str, target: &str, work: F) -> F::Output
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // Set by the waiter's Drop when the caller is dropped before the result.
    struct Waiter {
        gone: Arc<AtomicBool>,
        answered: bool,
    }
    impl Drop for Waiter {
        fn drop(&mut self) {
            if !self.answered {
                self.gone.store(true, Ordering::Release);
            }
        }
    }

    let gone = Arc::new(AtomicBool::new(false));
    let seen = gone.clone();
    let target_owned = target.to_string();
    let handle = spawn(async move {
        let out = work.await;
        if seen.load(Ordering::Acquire) {
            tracing::warn!(
                verb,
                target = %target_owned,
                verdict = "lifecycle_verb_outlived_client",
                measured = true,
                n_considered = 1,
                "the client left before this command finished; it ran to completion anyway"
            );
        }
        out
    });
    let mut waiter = Waiter { gone, answered: false };
    let joined = handle.await;
    waiter.answered = true;
    match joined {
        Ok(out) => out,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        // Only a runtime shutdown cancels a spawned task; nothing is left to answer.
        Err(e) => panic!("{verb} for {target} was cancelled with the runtime: {e}"),
    }
}

pub async fn progress(store: &super::Store, phase: &str) -> anyhow::Result<()> {
    if !matches!(
        phase,
        "running" | "waiting" | "blocked" | "applied" | "failed" | "refused"
    ) {
        anyhow::bail!("Invalid interaction progress phase");
    }
    let Some(id) = current_id() else {
        return Ok(());
    };
    let phase = phase.to_string();
    store
        .write_async(move |conn| {
            conn.execute(
                "UPDATE _amux_interactions SET phase=?2,updated_at=?3 WHERE id=?1",
                rusqlite::params![id, phase, chrono::Utc::now().timestamp_millis()],
            )?;
            Ok(super::WriteOutcome {
                applied: false,
                events: vec![],
            })
        })
        .await?;
    Ok(())
}

pub fn effects(conn: &rusqlite::Connection, id: &str) -> rusqlite::Result<Vec<serde_json::Value>> {
    let mut stmt = conn.prepare(
        "SELECT id,event_id,kind,entity_kind,entity_id,rev
        FROM _amux_interaction_effects WHERE interaction_id=?1 ORDER BY id LIMIT 1001",
    )?;
    let rows = stmt.query_map([id], |row| {
        let stored: String = row.get(2)?;
        let mutation: serde_json::Value = serde_json::from_str(&stored).unwrap_or(serde_json::Value::Null);
        let entity_kind: String = row.get(3)?;
        let kind = format!("{}.{}", entity_kind, mutation["kind"].as_str().unwrap_or("unknown"));
        Ok(serde_json::json!({"id":row.get::<_,i64>(0)?.to_string(), "interaction_id":id,
            "event_id":row.get::<_,i64>(1)?, "kind":kind, "mutation":mutation,
            "from":mutation.get("from"), "to":mutation.get("to"),
            "entity":{"primitive":entity_kind, "id":row.get::<_,String>(4)?}, "rev":row.get::<_,u64>(5)?}))
    })?.collect();
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The caller is dropped mid-operation, the way hyper drops a handler when
    /// the dashboard reloads. The work must still finish.
    #[tokio::test]
    async fn run_to_completion_finishes_after_the_caller_is_dropped() {
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let started_w = started.clone();
        let caller = tokio::spawn(run_to_completion("test-verb", "w", async move {
            started_w.notify_one();
            tokio::time::sleep(Duration::from_millis(150)).await;
            let _ = done_tx.send("finished");
        }));
        started.notified().await;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled(), "the caller was dropped mid-work");
        let done = tokio::time::timeout(Duration::from_secs(5), done_rx)
            .await
            .expect("work abandoned when its caller was dropped")
            .unwrap();
        assert_eq!(done, "finished");
    }

    #[tokio::test]
    async fn run_to_completion_returns_the_result_and_keeps_the_interaction_scope() {
        let id = CURRENT
            .scope("int_test".to_string(), run_to_completion("test-verb", "w", async { current_id() }))
            .await;
        assert_eq!(id.as_deref(), Some("int_test"));
    }

    #[tokio::test]
    #[should_panic(expected = "boom")]
    async fn run_to_completion_propagates_a_panic() {
        run_to_completion("test-verb", "w", async { panic!("boom") }).await;
    }
}
