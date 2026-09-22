//! `GET /api/skin` — the resolved skin for a worker (or the global default).
//!
//! Ethan: "a way to add skins (powered by the amux api) so it should be trivial
//! to build a new ux… then we create a skin for every vertical (terms, ux,
//! coloring, etc.)".
//!
//! # Why this is a scope capability and not a `/api/skins` subsystem
//!
//! A skin is terminology, colours and which tabs are visible. Every one of
//! those is per-scope configuration, and amux already has exactly one uniform
//! per-scope contract (`/api/scope`, `SCOPE_CAPS`). Adding `skin` as a
//! descriptor row means it inherits the global -> group -> worker precedence,
//! the write authorization (a session may write only its own worker layer),
//! and the `interaction_log` audit row — none of which a new subsystem would
//! have had, and all of which it would eventually have needed.
//!
//! This endpoint is the RESOLVER: `/api/scope` answers "what is set AT this
//! level", which is what a configuration screen needs; a client rendering a
//! skin needs "what do I actually get", which is a different question and the
//! reason both exist.
//!
//! # The merge
//!
//! Per-key, deep for objects: a group skin that renames one noun does not have
//! to restate the global palette, and a worker overriding one colour does not
//! drop the group's terminology. `replace` would force every layer to be a
//! complete skin, which is what makes theme systems unusable.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::AppState;

#[derive(Deserialize)]
pub struct Params {
    /// Resolve as this worker sees it. Absent = the global layer only.
    pub worker: Option<String>,
}

/// Deep per-key merge, `over` winning. Objects recurse; everything else
/// replaces wholesale.
///
/// Arrays REPLACE rather than concatenate: `tabs` is an ordered whitelist, and
/// concatenating would make a narrower layer unable to remove a tab — a skin
/// that can only ever add is not a skin.
pub fn merge_skin(base: &Value, over: &Value) -> Value {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            let mut out: Map<String, Value> = b.clone();
            for (k, v) in o {
                let merged = match b.get(k) {
                    Some(existing) => merge_skin(existing, v),
                    None => v.clone(),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        (_, o) => o.clone(),
    }
}

fn read_layer(conn: &rusqlite::Connection, key: &str) -> Option<Value> {
    let raw: String = conn
        .query_row(
            "SELECT value FROM prefs WHERE key=?1",
            rusqlite::params![key],
            |r| r.get(0),
        )
        .ok()?;
    serde_json::from_str(&raw).ok()
}

/// A worker's groups are its tags. Deliberately the SAME reader `/api/scope`
/// uses rather than a second one: two answers to "which groups is this worker
/// in" would eventually disagree, and then the configuration screen and the
/// resolved skin would disagree about which layers applied.
fn groups_of(worker: &str) -> Vec<String> {
    super::scope::session_tags_of(&super::groups::amux_home(), worker)
}

pub async fn get_skin(
    State(state): State<AppState>,
    Query(p): Query<Params>,
) -> (StatusCode, Json<Value>) {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("store unreadable: {e}")})),
            )
        }
    };

    // Layers in precedence order, least specific first — the SAME order the
    // `skin` descriptor publishes, so the resolver cannot drift from what the
    // configuration screen says will happen.
    let mut applied: Vec<String> = Vec::new();
    let mut resolved = json!({});

    if let Some(v) = read_layer(&conn, "skin:global") {
        resolved = merge_skin(&resolved, &v);
        applied.push("global".into());
    }
    let worker = p.worker.unwrap_or_default();
    if !worker.is_empty() {
        for g in groups_of(&worker) {
            if let Some(v) = read_layer(&conn, &format!("skin:group:{g}")) {
                resolved = merge_skin(&resolved, &v);
                applied.push(format!("group:{g}"));
            }
        }
        if let Some(v) = read_layer(&conn, &format!("skin:worker:{worker}")) {
            resolved = merge_skin(&resolved, &v);
            applied.push(format!("worker:{worker}"));
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "skin": resolved,
            // WHICH layers contributed, not just the result. "why is this tab
            // hidden" is the question a skin generates, and answering it from
            // the result alone is impossible (ethos rule 4).
            "layers": applied,
            "worker": worker,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_narrower_layer_overrides_one_key_without_restating_the_rest() {
        let global = json!({
            "terms": {"worker": "Worker", "board": "Board"},
            "colors": {"accent": "#58a6ff", "bg": "#0d1117"}
        });
        let group = json!({ "terms": {"worker": "Technician"} });
        let out = merge_skin(&global, &group);
        assert_eq!(out["terms"]["worker"], "Technician");
        // The whole point of merge-by-key: renaming one noun must not drop the
        // palette or the other terms.
        assert_eq!(out["terms"]["board"], "Board");
        assert_eq!(out["colors"]["accent"], "#58a6ff");
    }

    /// Arrays replace. `tabs` is an ordered whitelist, so concatenating would
    /// make a narrower layer able to ADD a tab but never remove one — which
    /// would defeat the main thing a vertical skin needs to do.
    #[test]
    fn a_tab_whitelist_can_be_narrowed_not_only_extended() {
        let global = json!({ "tabs": ["board", "workers", "calendar", "logs"] });
        let vertical = json!({ "tabs": ["board", "workers"] });
        let out = merge_skin(&global, &vertical);
        assert_eq!(out["tabs"], json!(["board", "workers"]));
    }

    #[test]
    fn deeper_nesting_still_merges_per_key() {
        let a = json!({"ui": {"peek": {"font": "mono", "size": 12}}});
        let b = json!({"ui": {"peek": {"size": 14}}});
        let out = merge_skin(&a, &b);
        assert_eq!(out["ui"]["peek"]["font"], "mono");
        assert_eq!(out["ui"]["peek"]["size"], 14);
    }

    #[test]
    fn an_empty_override_changes_nothing() {
        let a = json!({"terms": {"worker": "Agent"}});
        assert_eq!(merge_skin(&a, &json!({})), a);
    }
}
