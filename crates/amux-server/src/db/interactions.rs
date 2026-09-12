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
