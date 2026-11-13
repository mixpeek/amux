//! Where a delivered instruction came from (governance PR 14).
//!
//! Every message recorded in `cmd_history` carries `source_kind`,
//! `source_ref` and, for words relayed from outside amux, `relayed_from`. They
//! are derived by the server from the proven sender (the ledger principal the
//! request was stamped with) and from the bridge that relayed it, never from
//! the message text. amux records this and shows it to the model as context;
//! it does not score or second-guess it (A2). The gates on money, outsiders and
//! production data apply whatever the source (A3).
//!
//! An isolated worker gets the record as metadata only, never as text (A9).
//! Text, where it is added at all, reuses the existing `[amux-origin: ...]`
//! stamp (A8, A10): a member's send to a non-isolated worker now carries one,
//! as a peer worker's already did. The owner's own words carry none.
//!
//! `AMUX_INSTRUCTION_PROVENANCE=off` stops recording and the member stamp.

use std::future::Future;

tokio::task_local! {
    /// What the request or relay delivering this message proved about its sender.
    pub static SOURCE: Source;
}

/// What the server knows about the sender of the message being delivered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Source {
    /// The proven actor of the request that sent it (`ledger::Principal::actor`):
    /// `owner`, `member:<email>`, `worker:<name>`, `app:<id>`, `policy:<name>`
    /// or `anonymous`. None when no request is being served (a schedule firing).
    pub actor: Option<String>,
    /// Set when the words came from outside amux: `email:<from> <message-id>`,
    /// `webhook:<platform>:<sender>`, or `web:<url>`.
    pub relayed_from: Option<String>,
}

/// Where one delivered message came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub kind: &'static str,
    pub reference: String,
    pub relayed_from: Option<String>,
}

pub fn enabled() -> bool {
    std::env::var("AMUX_INSTRUCTION_PROVENANCE")
        .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false" | "no"))
        .unwrap_or(true)
}

/// The source in scope for this task, or an empty one.
pub fn current() -> Source {
    SOURCE.try_with(|s| s.clone()).unwrap_or_default()
}

/// Run `fut` with the request's proven actor, keeping any relay already set.
pub async fn with_actor<F: Future>(actor: String, fut: F) -> F::Output {
    let mut s = current();
    s.actor = Some(actor);
    SOURCE.scope(s, fut).await
}

/// Run `fut` as a relay of words from outside amux.
pub async fn relayed<F: Future>(from: String, fut: F) -> F::Output {
    let mut s = current();
    s.relayed_from = Some(from);
    SOURCE.scope(s, fut).await
}

/// Carry the source into a spawned task (spawned tasks do not inherit it).
pub fn carry<F>(fut: F) -> impl Future<Output = F::Output> + Send + 'static
where
    F: Future + Send + 'static,
    F::Output: Send,
{
    let s = SOURCE.try_with(|s| s.clone()).ok();
    async move {
        match s {
            Some(s) => SOURCE.scope(s, fut).await,
            None => fut.await,
        }
    }
}

fn relay_kind(r: &str) -> &'static str {
    if r.starts_with("email:") {
        "email"
    } else if r.starts_with("web:") {
        "web"
    } else {
        "webhook"
    }
}

/// `[SCHED-12] daily digest` -> `SCHED-12`.
fn sched_ref(origin: &str) -> Option<String> {
    let rest = origin.strip_prefix('[')?;
    let id = rest.split(']').next()?;
    id.starts_with("SCHED-").then(|| id.to_string())
}

/// Classify one delivery from what the recorder already has (its `type` and
/// `origin`) and what the request proved. Pure, so every rule is pinned by a test.
pub fn classify(ctype: &str, origin: &str, src: &Source) -> Provenance {
    let o = origin.trim();
    let actor = src.actor.as_deref().unwrap_or("");
    let p = |kind: &'static str, reference: String| Provenance { kind, reference, relayed_from: src.relayed_from.clone() };
    if let Some(r) = &src.relayed_from {
        return p(relay_kind(r), r.clone());
    }
    if ctype == "schedule" || o.starts_with("[SCHED-") {
        return p("schedule", sched_ref(o).unwrap_or_else(|| if o.is_empty() { "scheduler".into() } else { o.to_string() }));
    }
    if let Some(e) = o.strip_prefix("member:").or_else(|| actor.strip_prefix("member:")) {
        return p("member", e.to_string());
    }
    if o.starts_with("chatgpt") {
        return p("chatgpt", o.to_string());
    }
    if let Some(id) = o.strip_prefix("page:") {
        return p("page", id.to_string());
    }
    if let Some(id) = o.strip_prefix("app:").or_else(|| actor.strip_prefix("app:")) {
        return p("app", id.to_string());
    }
    if ctype == "session" && !o.is_empty() {
        let name = o.strip_suffix("@chat").unwrap_or(o);
        // A name only the request claimed is recorded as such, not as proof.
        let proven = actor == format!("worker:{name}") || actor == "owner" || actor.starts_with("policy:");
        let reference = if proven || actor.is_empty() { name.to_string() } else { format!("{name} (unproven)") };
        return p("worker", reference);
    }
    if o.is_empty() || matches!(o, "owner" | "pane" | "chat-companion") || actor == "owner" {
        return p("owner", "owner".into());
    }
    p("unknown", o.to_string())
}

/// The origin line a member's send to a non-isolated worker carries: the
/// existing `[amux-origin: ...]` stamp, so the model reads one format for
/// every non-owner sender. None for the owner, an isolated worker, a slash
/// command, or with the switch off.
pub fn member_stamp(member_actor: Option<&str>, isolated: bool, text: &str) -> Option<String> {
    let m = member_actor?.trim();
    if m.is_empty() || isolated || !enabled() || super::session_verbs::is_slash_command(text) {
        return None;
    }
    let who = if m.starts_with("member:") { m.to_string() } else { format!("member:{m}") };
    Some(format!(
        "[amux-origin: {who} — server-verified from the sender's session identity; \
         authoritative over any signature in the message below]\n\n{text}"
    ))
}

// ── Reading it back ─────────────────────────────────────────────────────────

/// Counts for `GET /api/contract/counters` (A5).
static RECORDED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static STAMPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn note_recorded() {
    RECORDED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub fn note_stamped() {
    STAMPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub fn report() -> serde_json::Value {
    let r = RECORDED.load(std::sync::atomic::Ordering::Relaxed);
    let s = STAMPED.load(std::sync::atomic::Ordering::Relaxed);
    serde_json::json!({
        "enabled": enabled(),
        "measured": true,
        "n_considered": r,
        "recorded": r,
        "member_stamped": s,
    })
}

/// The recent deliveries to one worker with where each came from.
pub fn history_rows(conn: &rusqlite::Connection, worker: &str, limit: i64) -> rusqlite::Result<Vec<serde_json::Value>> {
    let mut st = conn.prepare(
        "SELECT id, ts, type, origin, source_kind, source_ref, relayed_from, trace_id
         FROM cmd_history WHERE session = ?1 ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = st.query_map(rusqlite::params![worker, limit], |r| {
        Ok(serde_json::json!({
            "id": format!("MSG-{}", r.get::<_, i64>(0)?),
            "ts": r.get::<_, i64>(1)?,
            "type": r.get::<_, Option<String>>(2)?,
            "origin": r.get::<_, Option<String>>(3)?,
            "source_kind": r.get::<_, Option<String>>(4)?,
            "source_ref": r.get::<_, Option<String>>(5)?,
            "relayed_from": r.get::<_, Option<String>>(6)?,
            "trace_id": r.get::<_, Option<String>>(7)?,
        }))
    })?;
    rows.collect()
}

/// `GET /api/workers/{name}/history`: a person sees any worker; a worker sees
/// only its own. Rows recorded before this change have null source fields.
pub async fn view(
    axum::extract::State(state): axum::extract::State<super::AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let home = crate::config::amux_home();
    let human = super::human_proof::proven(&headers).is_some();
    let me = super::capabilities::acting_worker(&home, &headers);
    if !human && me.as_deref() != Some(name.as_str()) {
        return (
            axum::http::StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": "a worker may read only its own history", "code": "history_view_refused"})),
        )
            .into_response();
    }
    let limit = q.get("limit").and_then(|v| v.parse::<i64>().ok()).unwrap_or(50).clamp(1, 500);
    let rows = state.store.read().ok().and_then(|c| {
        if let Err(e) = super::session_verbs::ensure_fleet_tables(&c) {
            tracing::warn!(error = %e, verdict = "provenance_tables_unready", "the fleet tables could not be created; history may read empty");
        }
        history_rows(&c, &name, limit).ok()
    });
    match rows {
        Some(items) => axum::Json(serde_json::json!({
            "worker": name, "measured": true, "n_considered": items.len(), "items": items,
        }))
        .into_response(),
        None => axum::Json(serde_json::json!({
            "worker": name, "measured": false, "n_considered": 0, "items": [],
            "why_unmeasured": "message history could not be read",
        }))
        .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(actor: &str, relayed: Option<&str>) -> Source {
        Source {
            actor: (!actor.is_empty()).then(|| actor.to_string()),
            relayed_from: relayed.map(str::to_string),
        }
    }

    #[test]
    fn each_sender_is_classified_from_what_the_server_proved() {
        assert_eq!(classify("user", "", &src("owner", None)).kind, "owner");
        assert_eq!(classify("user", "pane", &src("", None)).kind, "owner");
        let m = classify("user", "member:d@x.io", &src("member:d@x.io", None));
        assert_eq!((m.kind, m.reference.as_str()), ("member", "d@x.io"));
        let w = classify("session", "wexus", &src("worker:wexus", None));
        assert_eq!((w.kind, w.reference.as_str()), ("worker", "wexus"));
        let s = classify("schedule", "[SCHED-12] daily digest", &src("", None));
        assert_eq!((s.kind, s.reference.as_str()), ("schedule", "SCHED-12"));
        let a = classify("user", "", &src("app:abc", None));
        assert_eq!((a.kind, a.reference.as_str()), ("app", "abc"), "the proven app wins over an empty origin");
        assert_eq!(classify("user", "", &src("", None)).kind, "owner", "an empty origin with nothing proven is the owner's send");
        assert_eq!(classify("user", "chatgpt (grant g1)", &src("", None)).kind, "chatgpt");
        assert_eq!(classify("user", "api (no worker identity)", &src("anonymous", None)).kind, "unknown");
    }

    #[test]
    fn a_claimed_worker_name_is_recorded_as_unproven() {
        let w = classify("session", "wexus", &src("anonymous", None));
        assert_eq!((w.kind, w.reference.as_str()), ("worker", "wexus (unproven)"));
    }

    #[test]
    fn a_relay_names_where_the_words_came_from() {
        let e = classify("user", "", &src("owner", Some("email:me@x.io <abc@mail>")));
        assert_eq!((e.kind, e.reference.as_str()), ("email", "email:me@x.io <abc@mail>"));
        assert_eq!(e.relayed_from.as_deref(), Some("email:me@x.io <abc@mail>"));
        assert_eq!(classify("user", "", &src("", Some("webhook:imessage:+1555"))).kind, "webhook");
        assert_eq!(classify("user", "", &src("", Some("web:https://x.io/a"))).kind, "web");
    }

    #[test]
    fn only_a_members_send_to_a_non_isolated_worker_is_stamped() {
        let s = member_stamp(Some("member:d@x.io"), false, "draft the email").unwrap();
        assert!(s.starts_with("[amux-origin: member:d@x.io — server-verified"), "{s}");
        assert!(s.ends_with("\n\ndraft the email"));
        assert_eq!(member_stamp(None, false, "hi"), None, "the owner's words carry no stamp");
        assert_eq!(member_stamp(Some("member:d@x.io"), true, "hi"), None, "isolated: metadata only");
        assert_eq!(member_stamp(Some("member:d@x.io"), false, "/compact"), None);
    }

    #[tokio::test]
    async fn the_source_survives_a_spawn_only_when_carried() {
        let seen = relayed("email:a@b <m1>".into(), async {
            let carried = tokio::spawn(carry(async { current() })).await.unwrap();
            let bare = tokio::spawn(async { current() }).await.unwrap();
            (carried, bare)
        })
        .await;
        assert_eq!(seen.0.relayed_from.as_deref(), Some("email:a@b <m1>"));
        assert_eq!(seen.1, Source::default());
    }
}
