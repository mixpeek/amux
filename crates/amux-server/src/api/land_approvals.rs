//! The owner's approval of one exact change to an owner-gated path, before it
//! lands (scripts/land_gate.py, scripts/review-tiers.txt `[owner]`).
//!
//! The Claude Code team's review write-up (2026-10-26) keeps a human on code
//! that controls money, auth and the gates themselves, sets that tier by the
//! CHANGE rather than its author, and makes an approval good for one diff
//! only. Here the diff is named by its `git patch-id --stable`, restricted to
//! the owner-gated files, so a clean rebase keeps the approval and any edit to
//! those files needs a new one.
//!
//! - `POST /api/land/approvals/request` (anyone): records the ask once per
//!   patch-id and files one needsyou card carrying the question and the
//!   approval link `#land-approve=<patch-id>`. A lane with no board (isolated)
//!   gets the card unattributed, on the owner's board.
//! - `POST /api/land/approvals` (owner only): approves one patch-id and closes
//!   its card. Owner means the owner bearer and no worker or member identity,
//!   the vault's predicate (`vault_secrets::owner_only`): omitting the worker
//!   headers is not proof of ownership.
//! - `GET /api/land/approvals[?patch_id=]` (anyone): one record, or all.
//!
//! Stored in `<AMUX_HOME>/land-approvals.json`. Verdicts:
//! land_approval_requested, land_approval_granted, land_approval_refused.

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::AppState;

const FILE: &str = "land-approvals.json";
const ACTOR: &str = "harness:land-gate";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/land/approvals", axum::routing::get(get_route).post(approve_route))
        .route("/api/land/approvals/request", axum::routing::post(request_route))
}

/// Every load-modify-save of the file holds this, so two asks arriving at
/// once cannot overwrite each other (the land review's F2, 2026-10-27).
static FILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn file_lock() -> std::sync::MutexGuard<'static, ()> {
    FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn path(home: &Path) -> PathBuf {
    home.join(FILE)
}

fn load(home: &Path) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(path(home))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn save(home: &Path, all: &serde_json::Map<String, Value>) -> std::io::Result<()> {
    let p = path(home);
    let tmp = p.with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&Value::Object(all.clone())).unwrap_or_default())?;
    std::fs::rename(tmp, p)
}

/// A patch-id is 40 lowercase hex characters; anything else is refused so the
/// id is safe inside a URL hash and a card title.
fn valid_patch_id(p: &str) -> bool {
    p.len() == 40 && p.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn worker_origin(headers: &HeaderMap) -> Option<String> {
    ["x-amux-session", "x-amux-worker"].iter().find_map(|h| {
        headers.get(*h).and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
    })
}

/// The owner, and only the owner: the owner bearer and no worker or member
/// identity (the vault's predicate). Shared with the chat bridge's approvals.
pub(crate) fn owner_only(state: &AppState, headers: &HeaderMap, uri: &Uri, verdict: &'static str, what: &str) -> Result<(), Box<Response>> {
    let refuse = |who: &str, why: &str| {
        tracing::warn!(who, what, measured = true, n_considered = 1, verdict, "a request that is not the owner was refused");
        Err(Box::new(bad(StatusCode::FORBIDDEN, &format!("only the owner can {what}"), why)))
    };
    if let Some(w) = worker_origin(headers) {
        return refuse(&w, "a worker cannot do this, for itself or another lane");
    }
    if super::org::is_verified_local_member(headers) {
        return refuse("member", "an invited member cannot do this");
    }
    if state.auth_token.is_some() && !super::auth::has_owner_token(state, headers, uri) {
        return refuse("no-owner-credential", "send the owner bearer (the dashboard does)");
    }
    Ok(())
}

fn bad(code: StatusCode, error: &str, why: &str) -> Response {
    (code, Json(json!({"ok": false, "error": error, "why": why}))).into_response()
}

async fn get_route(Query(q): Query<HashMap<String, String>>) -> Response {
    let home = crate::config::amux_home();
    let all = load(&home);
    match q.get("patch_id").map(|s| s.trim().to_string()) {
        Some(p) => {
            let rec = all.get(&p).cloned().unwrap_or_else(|| json!({}));
            let approved = rec.get("approved_at").is_some_and(|v| !v.is_null());
            Json(json!({
                "patch_id": p, "known": all.contains_key(&p), "approved": approved,
                "approved_at": rec.get("approved_at"), "by": rec.get("approved_by"),
                "request": rec,
            }))
            .into_response()
        }
        None => Json(json!({"approvals": all, "n": all.len()})).into_response(),
    }
}

fn approval_link(patch_id: &str) -> String {
    format!("/#land-approve={patch_id}")
}

async fn request_route(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let patch_id = body.get("patch_id").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if !valid_patch_id(&patch_id) {
        return bad(StatusCode::BAD_REQUEST, "patch_id must be a 40-character `git patch-id --stable`", "");
    }
    let lane = worker_origin(&headers)
        .or_else(|| body.get("lane").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))
        .unwrap_or_default();
    let files: Vec<String> = body
        .get("files")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).take(50).map(str::to_string).collect())
        .unwrap_or_default();
    let diffstat: String = body.get("diffstat").and_then(Value::as_str).unwrap_or("").chars().take(3000).collect();
    let subjects: Vec<String> = body
        .get("subjects")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).take(20).map(|s| s.chars().take(200).collect()).collect())
        .unwrap_or_default();
    let home = crate::config::amux_home();
    {
        let _g = file_lock();
        let mut all = load(&home);
        if let Some(rec) = all.get(&patch_id) {
            return Json(json!({"ok": true, "patch_id": patch_id, "card": rec.get("card"), "link": approval_link(&patch_id),
                "approved": rec.get("approved_at").is_some_and(|v| !v.is_null()), "already_requested": true}))
            .into_response();
        }
        // Claim the patch-id before the card write, so a second ask for the
        // same diff in the meantime files no second card.
        all.insert(patch_id.clone(), json!({"patch_id": patch_id, "lane": lane, "requested_at": crate::config::now_f64(),
            "approved_at": null, "card": null}));
        if let Err(e) = save(&home, &all) {
            return bad(StatusCode::INTERNAL_SERVER_ERROR, "could not record the request", &e.to_string());
        }
    }
    let short = &patch_id[..12];
    let who = if lane.is_empty() { "a person" } else { lane.as_str() };
    let has_board = !lane.is_empty() && crate::api::board_lifecycle::lane_has_board(&lane);
    let desc = format!(
        "{who} wants to land a change to owner-gated files (scripts/review-tiers.txt [owner]):\n\n{}\n\n```\n{}\n```\n\n\
         Commits:\n{}\n\nApprove this exact diff (patch-id {patch_id}) at {}. \
         Any further edit to these files needs a new approval. If you do not want it, move this card to discarded.",
        files.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n"),
        diffstat.trim(),
        subjects.iter().map(|s| format!("- {s}")).collect::<Vec<_>>().join("\n"),
        approval_link(&patch_id),
    );
    let new = crate::db::board_store::NewIssue {
        title: format!("Approve owner-gated change {short} by {who}: {}", files.join(", ").chars().take(120).collect::<String>()),
        desc,
        status: "needsyou".into(),
        session: has_board.then(|| lane.clone()),
        item_type: "decision".into(),
        creator: ACTOR.into(),
        owner_type: "human".into(),
        due: None,
        due_time: None,
        reviewer: None,
        shepherd: None,
        gate: vec![],
        depends_on: vec![],
        tags: vec!["land-approval".into()],
        ask_type: Some("decision".into()),
        next_action: None,
        acceptance_criteria: None,
        ask_question: Some(format!("Approve {who}'s change {short} to {}?", files.join(", ").chars().take(160).collect::<String>())),
        ask_unblocks: Some(format!("Approving at {} lets that exact diff land.", approval_link(&patch_id))),
        ask_actor: Some(crate::api::turn_end::owner_name()),
        source: Some("land_approval".into()),
        requested_by: None,
        callback_session: None,
        callback_prompt: None,
    };
    let now = crate::config::now_f64();
    let made = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let made_w = made.clone();
    let wrote = state
        .store
        .write_async(move |conn| {
            let row = crate::db::board_store::create_issue(conn, &new, now as i64)?;
            if let Ok(mut m) = made_w.lock() {
                *m = Some(row.id.clone());
            }
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Task,
                    entity_id: row.id.clone(),
                    mutation: amux_core::revision::MutationKind::Created,
                    payload: Some(json!({"id": row.id, "source": "land_approval"})),
                }],
            })
        })
        .await;
    let card = made.lock().ok().and_then(|m| m.clone());
    let card_err = wrote.err().map(|e| e.to_string());
    let saved = {
        let _g = file_lock();
        let mut all = load(&home);
        all.insert(patch_id.clone(), json!({
            "patch_id": patch_id, "lane": lane, "files": files, "diffstat": diffstat, "subjects": subjects,
            "head": body.get("head"), "requested_at": now, "card": card, "approved_at": null,
        }));
        save(&home, &all)
    };
    tracing::info!(patch_id = %patch_id, lane = %lane, card = ?card, card_error = ?card_err, saved = saved.is_ok(),
        measured = true, n_considered = 1, verdict = "land_approval_requested",
        "land gate: asked the owner to approve an owner-gated change");
    if let Err(e) = saved {
        return bad(StatusCode::INTERNAL_SERVER_ERROR, "could not record the request", &e.to_string());
    }
    Json(json!({"ok": true, "patch_id": patch_id, "card": card, "card_error": card_err, "link": approval_link(&patch_id)}))
        .into_response()
}

async fn approve_route(State(state): State<AppState>, headers: HeaderMap, uri: Uri, Json(body): Json<Value>) -> Response {
    let patch_id = body.get("patch_id").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if let Err(r) = owner_only(&state, &headers, &uri, "land_approval_refused", "approve an owner-gated change") {
        return *r;
    }
    if !valid_patch_id(&patch_id) {
        return bad(StatusCode::BAD_REQUEST, "patch_id must be a 40-character `git patch-id --stable`", "");
    }
    let home = crate::config::amux_home();
    let guard = file_lock();
    let mut all = load(&home);
    let Some(rec) = all.get_mut(&patch_id).and_then(Value::as_object_mut) else {
        return bad(StatusCode::NOT_FOUND, "no approval was requested for that patch-id", "run scripts/land_gate.py to file it");
    };
    let now = crate::config::now_f64();
    let by = if state.auth_token.is_some() { super::auth::OWNER_TOKEN_ACTOR } else { "owner-by-absence" };
    rec.insert("approved_at".into(), json!(now));
    rec.insert("approved_by".into(), json!(by));
    if let Some(note) = body.get("note").and_then(Value::as_str) {
        rec.insert("note".into(), json!(note.chars().take(500).collect::<String>()));
    }
    let card = rec.get("card").and_then(Value::as_str).map(str::to_string);
    let saved = save(&home, &all);
    drop(guard);
    if let Err(e) = saved {
        return bad(StatusCode::INTERNAL_SERVER_ERROR, "could not record the approval", &e.to_string());
    }
    let mut closed = false;
    if let Some(card) = card.clone() {
        let pid = patch_id.clone();
        closed = state
            .store
            .write_async(move |conn| {
                let opts = crate::db::advance::AdvanceOpts {
                    gate_ack: true,
                    skip_continuation: true,
                    reason: Some(format!("approved by the owner in the dashboard (patch-id {pid})")),
                    log_line: Some(format!("owner approved patch-id {pid}")),
                    ..Default::default()
                };
                let r = crate::db::advance::advance(conn, &card, "done", ACTOR, &opts);
                Ok(crate::db::WriteOutcome { applied: matches!(r, Ok(Ok(_))), events: vec![] })
            })
            .await
            .is_ok_and(|w| w.applied);
    }
    tracing::info!(patch_id = %patch_id, by, card = ?card, card_closed = closed, measured = true, n_considered = 1,
        verdict = "land_approval_granted", "land gate: the owner approved an owner-gated change");
    Json(json!({"ok": true, "patch_id": patch_id, "approved_at": now, "by": by, "card": card, "card_closed": closed}))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::standing_approvals::tests::{call, test_state};

    const PID: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn a_patch_id_is_forty_lowercase_hex() {
        assert!(valid_patch_id(PID));
        assert!(!valid_patch_id(&PID.to_uppercase()), "uppercase is not what git patch-id prints");
        assert!(!valid_patch_id(&PID[..39]));
        assert!(!valid_patch_id("0123456789abcdef0123456789abcdef0123456g"));
        assert!(!valid_patch_id("../../etc/passwd012345678901234567890123"));
    }

    #[tokio::test]
    async fn a_worker_asks_only_the_owner_approves_and_the_card_closes() {
        let dir = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(dir.path());
        let mut state = test_state();
        state.auth_token = Some("owner-secret".into());
        let store = state.store.clone();
        let app = routes().with_state(state);

        let (st, v) = call(&app, "GET", &format!("/api/land/approvals?patch_id={PID}"), &[], None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!((v["known"].clone(), v["approved"].clone()), (json!(false), json!(false)));

        let ask = json!({"patch_id": PID, "files": ["boundaries.toml"], "diffstat": " boundaries.toml | 1 +",
                         "subjects": ["feat: cap payments"], "head": "abc"});
        let (st, v) = call(&app, "POST", "/api/land/approvals/request", &[("x-amux-session", "lane-t")], Some(ask.clone())).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let card = v["card"].as_str().expect("a needsyou card was filed").to_string();
        assert_eq!(v["link"], format!("/#land-approve={PID}"));
        let row = store.read().unwrap();
        let issue = crate::db::board_store::get_issue(&row, &card).unwrap().unwrap();
        assert_eq!(issue.status, "needsyou");
        assert!(issue.desc.contains(&format!("#land-approve={PID}")), "the card carries the approval link");
        drop(row);
        let (_, v) = call(&app, "POST", "/api/land/approvals/request", &[("x-amux-session", "lane-t")], Some(ask)).await;
        assert_eq!((v["card"].as_str(), v["already_requested"].clone()), (Some(card.as_str()), json!(true)),
            "asking twice files one card");

        // Nobody but the owner approves: a worker header, even with the owner
        // bearer, and a request with no bearer at all.
        for (h, why) in [
            (vec![("x-amux-session", "lane-t"), ("authorization", "Bearer owner-secret")], "worker with the bearer"),
            (vec![], "no bearer"),
            (vec![("authorization", "Bearer wrong")], "wrong bearer"),
        ] {
            let (st, _) = call(&app, "POST", "/api/land/approvals", &h, Some(json!({"patch_id": PID}))).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{why} must not approve");
        }
        let (_, v) = call(&app, "GET", &format!("/api/land/approvals?patch_id={PID}"), &[], None).await;
        assert_eq!(v["approved"], false, "refusals record nothing");

        let (st, v) = call(&app, "POST", "/api/land/approvals", &[("authorization", "Bearer owner-secret")],
            Some(json!({"patch_id": PID}))).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["by"], "owner-token");
        let (_, v) = call(&app, "GET", &format!("/api/land/approvals?patch_id={PID}"), &[], None).await;
        assert_eq!((v["approved"].clone(), v["by"].clone()), (json!(true), json!("owner-token")));
        let row = store.read().unwrap();
        let issue = crate::db::board_store::get_issue(&row, &card).unwrap().unwrap();
        assert_ne!(issue.status, "needsyou", "approving closes the ask");

        let (st, _) = call(&app, "POST", "/api/land/approvals", &[("authorization", "Bearer owner-secret")],
            Some(json!({"patch_id": "f".repeat(40)}))).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "an approval needs a request to approve");
    }
}
