//! Intent's records live in Messages (docs/design/intent-first.md, "Built from
//! primitives"). No table of its own (A1):
//!
//! - An outcome is the thread `o:<id>`. Its routing (which worker, the
//!   router's decision, what it came from) is a post in that thread by
//!   `amux:record`; a re-route or a "Change" is another one. The worker an
//!   outcome belongs to is the newest record's.
//!
//! Ideas, goals and to-dos are board cards (`intent_items.rs`), reminders are
//! scheduler entries, drafts are held email-API sends.
//!
//! A record post's body is the plain line a person reads in Messages ("Sent to
//! Family helper", "Moved to School helper"); its structured part is an
//! attachment of kind `intent`, which Messages does not draw. `amux:record`
//! posts notify nobody and are left out of the thread Home shows.

use super::AppState;
use serde_json::{json, Value};

pub(crate) const AUTHOR: &str = "amux:record";

/// One record post: its id, thread, time and structured part.
#[derive(Debug, Clone)]
pub(crate) struct Rec {
    pub post_id: i64,
    pub conv: String,
    pub ts: i64,
    pub data: Value,
}

/// The structured part of a post's attachments, if it is a record.
pub(crate) fn data_of(attachments: &Value) -> Option<Value> {
    attachments.as_array()?.iter().find(|a| a["kind"] == "intent").map(|a| a["data"].clone())
}

/// Write a record post. Returns its post id.
pub(crate) async fn write(state: &AppState, conv: &str, body: &str, data: Value) -> Result<i64, String> {
    let v = super::conversations::record_post(state, conv, body, json!([{"kind": "intent", "data": data}])).await?;
    v["id"].as_i64().ok_or_else(|| format!("the record post into {conv} has no id"))
}

fn rows(state: &AppState, sql: &str, arg: &str) -> Result<Vec<Rec>, String> {
    let c = state.store.read().map_err(|e| e.to_string())?;
    let mut st = c.prepare(sql).map_err(|e| e.to_string())?;
    let out = st
        .query_map([AUTHOR, arg], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .filter_map(|(post_id, conv, ts, att)| {
            let data = data_of(&serde_json::from_str(&att).unwrap_or(json!([])))?;
            Some(Rec { post_id, conv, ts, data })
        })
        .collect();
    Ok(out)
}

/// The records of one thread, oldest first.
pub(crate) fn of_conv(state: &AppState, conv: &str) -> Result<Vec<Rec>, String> {
    rows(state,
        "SELECT id, conv, ts, attachments FROM conversation_posts \
         WHERE author_key=?1 AND conv=?2 AND deleted_at IS NULL ORDER BY id ASC",
        conv)
}

/// The records of every outcome thread, oldest first.
pub(crate) fn of_outcomes(state: &AppState) -> Result<Vec<Rec>, String> {
    rows(state,
        "SELECT id, conv, ts, attachments FROM conversation_posts \
         WHERE author_key=?1 AND conv LIKE 'o:%' AND conv <> ?2 \
         AND deleted_at IS NULL ORDER BY id ASC LIMIT 20000",
        "")
}

/// The newest post time in each of these threads, so an outcome's "updated"
/// is its thread's last activity.
pub(crate) fn last_activity(state: &AppState) -> std::collections::HashMap<String, i64> {
    let Ok(c) = state.store.read() else { return Default::default() };
    let Ok(mut st) = c.prepare(
        "SELECT conv, MAX(ts) FROM conversation_posts WHERE conv LIKE 'o:%' AND deleted_at IS NULL GROUP BY conv") else {
        return Default::default();
    };
    st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map(|r| r.flatten().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_is_the_intent_attachment_only() {
        assert_eq!(data_of(&json!([{"kind": "audio", "path": "x"}, {"kind": "intent", "data": {"a": 1}}])), Some(json!({"a": 1})));
        assert_eq!(data_of(&json!([{"kind": "audio", "path": "x"}])), None);
        assert_eq!(data_of(&json!([])), None);
    }
}
