//! `GET /api/config/export` · `PUT /api/config/apply` — the whole instance as
//! declarative config (AMUX-2679).
//!
//! Ethan: "a way to configure a client/server using yaml entirely (workers,
//! tabs visible, etc.)… preload their shit using the IaC".
//!
//! # Why the server speaks JSON and the CLI speaks YAML
//!
//! Every other amux contract is JSON, and adding a YAML parser to the server
//! would add a dependency to make ONE endpoint different from all the others.
//! `amux config apply file.yaml` converts with python3 (already this CLI's
//! interpreter, and pyyaml is present) and PUTs JSON. YAML stays the human's
//! format, which is where it belongs; the wire stays uniform.
//!
//! # What it covers, and what it deliberately does not
//!
//! Covered: skins (all three scope levels), board columns, and worker
//! DECLARATIONS. Those are the three Ethan named and all three are pure
//! configuration — declaring them twice produces the same instance.
//!
//! NOT covered, on purpose: starting workers. Apply writes a worker's
//! declaration (its dir, tags, flags) and stops. Spawning processes from a
//! config file means a mistyped apply starts 40 agents, and the difference
//! between "declare" and "run" is exactly the difference IaC exists to keep.
//! `amux workers start` is the verb for the second half, and it already exists.
//!
//! # Idempotence is the property, not a nice-to-have
//!
//! Apply reports what it CHANGED versus what already matched, per item. A
//! config tool that cannot tell you "nothing to do" is one nobody dares run
//! twice, and the whole point of preloading a vertical is running it against
//! an instance that may be half-configured already.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde_json::{json, Map, Value};

use super::AppState;

/// Everything an instance's configuration consists of, in one payload.
pub async fn export(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("store unreadable: {e}")})),
            )
        }
    };

    // ---- skins, all three levels ------------------------------------------
    let mut skins = Map::new();
    if let Ok(mut st) = conn.prepare("SELECT key, value FROM prefs WHERE key LIKE 'skin:%'") {
        if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        {
            for (k, v) in rows.flatten() {
                let parsed: Value = serde_json::from_str(&v).unwrap_or(Value::Null);
                // `skin:group:ops` -> skins.group.ops ; `skin:global` -> skins.global
                let rest = k.trim_start_matches("skin:");
                match rest.split_once(':') {
                    Some((lvl, name)) => {
                        skins
                            .entry(lvl.to_string())
                            .or_insert_with(|| json!({}))
                            .as_object_mut()
                            .map(|m| m.insert(name.to_string(), parsed));
                    }
                    None => {
                        skins.insert(rest.to_string(), parsed);
                    }
                }
            }
        }
    }

    // ---- board columns ----------------------------------------------------
    let mut columns: Vec<Value> = Vec::new();
    if let Ok(mut st) = conn.prepare(
        "SELECT id, label, position, COALESCE(is_builtin,0), gate, mode, COALESCE(gate_custom,0) \
         FROM statuses ORDER BY position",
    ) {
        if let Ok(rows) = st.query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "label": r.get::<_, Option<String>>(1)?,
                "position": r.get::<_, Option<i64>>(2)?,
                "builtin": r.get::<_, i64>(3)? == 1,
                // Only export a gate a HUMAN set. A seeded gate is not
                // configuration, and exporting it would make every instance's
                // config file claim an intent nobody expressed (AMUX-2641).
                "gate": if r.get::<_, i64>(6)? == 1 {
                    r.get::<_, Option<String>>(4)?
                        .and_then(|g| serde_json::from_str::<Value>(&g).ok())
                } else { None },
                "mode": r.get::<_, Option<String>>(5)?,
            }))
        }) {
            columns.extend(rows.flatten());
        }
    }

    // ---- worker declarations ---------------------------------------------
    let home = super::groups::amux_home();
    let mut workers: Vec<Value> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(home.join("sessions")) {
        let mut names: Vec<String> = rd
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                (p.extension().and_then(|x| x.to_str()) == Some("env"))
                    .then(|| p.file_stem()?.to_str().map(String::from))
                    .flatten()
            })
            .collect();
        names.sort();
        for name in names {
            let env =
                crate::config::parse_env_file(&home.join("sessions").join(format!("{name}.env")));
            if env.get("CC_ARCHIVED").map(|v| v == "1").unwrap_or(false) {
                continue;
            }
            workers.push(json!({
                "name": name,
                "dir": env.get("CC_DIR").cloned().unwrap_or_default(),
                // A LIST, not the raw CC_TAGS string: the config file is the
                // human-facing artifact and "tags: [ops, gtm]" is what a person
                // writes. (An earlier version passed CC_TAGS as the worker NAME
                // to session_tags_of — clippy flagged the awkward chain, and the
                // real defect underneath was the wrong argument.)
                "tags": env.get("CC_TAGS")
                    .map(|raw| raw.split([',', ' '])
                        .map(str::trim).filter(|t| !t.is_empty())
                        .map(String::from).collect::<Vec<_>>())
                    .unwrap_or_default(),
                "flags": env.get("CC_FLAGS").cloned().unwrap_or_default(),
            }));
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "version": 1,
            "skins": Value::Object(skins),
            "columns": columns,
            "workers": workers,
            "note": "apply is idempotent; workers are DECLARED here, not started \
                     (`amux workers start <name>` is the separate verb, on purpose)",
        })),
    )
}

/// Apply a config document. Reports per-item `changed` vs `unchanged` so a
/// second run visibly does nothing.
pub async fn apply(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(doc): Json<Value>,
) -> (StatusCode, Json<Value>) {
    // Writing global/group configuration is the human's, matching
    // /api/scope's own policy: a session may not reshape the whole instance.
    let actor = headers
        .get("x-amux-session")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !actor.is_empty() && std::env::var("AMUX_SCOPE_WRITE_AGENTS").ok().as_deref() != Some("1") {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": format!(
                    "session '{actor}' may not apply instance config — global and group \
                     layers belong to the human (same policy as /api/scope). Set \
                     AMUX_SCOPE_WRITE_AGENTS=1 to allow it."
                ),
            })),
        );
    }

    let mut changed: Vec<String> = Vec::new();
    let mut unchanged: Vec<String> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();

    // ---- skins -----------------------------------------------------------
    if let Some(skins) = doc.get("skins").and_then(Value::as_object) {
        for (level, v) in skins {
            let pairs: Vec<(String, Value)> = if level == "global" {
                vec![("skin:global".to_string(), v.clone())]
            } else {
                v.as_object()
                    .map(|m| {
                        m.iter()
                            .map(|(n, sv)| (format!("skin:{level}:{n}"), sv.clone()))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            for (key, val) in pairs {
                let want = val.to_string();
                let have: Option<String> = state.store.read().ok().and_then(|c| {
                    c.query_row(
                        "SELECT value FROM prefs WHERE key=?1",
                        rusqlite::params![key],
                        |r| r.get::<_, String>(0),
                    )
                    .ok()
                });
                if have.as_deref() == Some(want.as_str()) {
                    unchanged.push(key);
                    continue;
                }
                let k2 = key.clone();
                let res = state
                    .store
                    .write_async(move |conn| {
                        conn.execute(
                            "INSERT INTO prefs (key, value) VALUES (?1, ?2) \
                             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                            rusqlite::params![k2, want],
                        )?;
                        Ok(crate::db::WriteOutcome {
                            applied: true,
                            events: vec![],
                        })
                    })
                    .await;
                match res {
                    Ok(_) => changed.push(key),
                    Err(e) => errors.push(json!({"item": key, "error": e.to_string()})),
                }
            }
        }
    }

    // ---- board columns ---------------------------------------------------
    if let Some(cols) = doc.get("columns").and_then(Value::as_array) {
        for c in cols {
            let Some(id) = c.get("id").and_then(Value::as_str) else {
                errors.push(json!({"item": "column", "error": "column needs an id"}));
                continue;
            };
            let label = c.get("label").and_then(Value::as_str).map(String::from);
            let position = c.get("position").and_then(Value::as_i64);
            let gate = c
                .get("gate")
                .and_then(|g| g.as_array())
                .map(|_| c["gate"].to_string());
            let id2 = id.to_string();
            let tag = format!("column:{id}");
            // COMPARE FIRST. Writing unconditionally made a re-apply report
            // "2 changed" when nothing had, which destroys the one property
            // this endpoint promises — a config tool that claims to have
            // changed things it did not is one you cannot trust to tell you
            // when it DID.
            let current: Option<(Option<String>, Option<i64>, Option<String>)> =
                state.store.read().ok().and_then(|c| {
                    c.query_row(
                        "SELECT label, position, gate FROM statuses WHERE id=?1",
                        rusqlite::params![id2],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .ok()
                });
            if let Some((cur_label, cur_pos, cur_gate)) = &current {
                let label_ok = label.is_none() || label.as_deref() == cur_label.as_deref();
                let pos_ok = position.is_none() || position == *cur_pos;
                let gate_ok = gate.is_none() || gate.as_deref() == cur_gate.as_deref();
                if label_ok && pos_ok && gate_ok {
                    unchanged.push(tag);
                    continue;
                }
            }
            let res = state
                .store
                .write_async(move |conn| {
                    conn.execute(
                        "INSERT INTO statuses (id, label, position, is_builtin) VALUES (?1, ?2, ?3, 0) \
                         ON CONFLICT(id) DO NOTHING",
                        rusqlite::params![id2, label, position],
                    )?;
                    if let Some(l) = &label {
                        conn.execute("UPDATE statuses SET label=?1 WHERE id=?2", rusqlite::params![l, id2])?;
                    }
                    if let Some(p) = position {
                        conn.execute("UPDATE statuses SET position=?1 WHERE id=?2", rusqlite::params![p, id2])?;
                    }
                    if let Some(g) = &gate {
                        // gate_custom=1: a config file IS a human expressing
                        // intent, so the gate must be enforced (AMUX-2641).
                        conn.execute(
                            "UPDATE statuses SET gate=?1, gate_custom=1 WHERE id=?2",
                            rusqlite::params![g, id2],
                        )?;
                    }
                    Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                })
                .await;
            match res {
                Ok(_) => changed.push(tag),
                Err(e) => errors.push(json!({"item": tag, "error": e.to_string()})),
            }
        }
    }

    // ---- worker declarations ---------------------------------------------
    if let Some(ws) = doc.get("workers").and_then(Value::as_array) {
        let home = super::groups::amux_home();
        for w in ws {
            let Some(name) = w.get("name").and_then(Value::as_str) else {
                errors.push(json!({"item": "worker", "error": "worker needs a name"}));
                continue;
            };
            let f = home.join("sessions").join(format!("{name}.env"));
            let mut env = crate::config::parse_env_file(&f);
            let before = env.clone();
            for (yaml_key, env_key) in [
                ("dir", "CC_DIR"),
                ("tags", "CC_TAGS"),
                ("flags", "CC_FLAGS"),
                ("creator", "CC_CREATOR"),
            ] {
                if let Some(v) = w.get(yaml_key) {
                    let s = match v {
                        Value::String(s) => s.clone(),
                        Value::Array(a) => a
                            .iter()
                            .filter_map(|x| x.as_str())
                            .collect::<Vec<_>>()
                            .join(","),
                        other => other.to_string(),
                    };
                    env.insert(env_key.to_string(), s);
                }
            }
            let tag = format!("worker:{name}");
            if env == before && f.exists() {
                unchanged.push(tag);
                continue;
            }
            let body: String = env.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
            if let Some(p) = f.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            match std::fs::write(&f, body) {
                Ok(_) => changed.push(tag),
                Err(e) => errors.push(json!({"item": tag, "error": e.to_string()})),
            }
        }
    }

    let status = if errors.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::MULTI_STATUS
    };
    (
        status,
        Json(json!({
            "ok": errors.is_empty(),
            "changed": changed,
            "unchanged": unchanged,
            "errors": errors,
            // The number people actually want on a second run.
            "summary": format!("{} changed, {} already matched", changed.len(), unchanged.len()),
        })),
    )
}
