//! Ad-hoc, owner-approved permission grants (AMUX-3997).
//!
//! # Why this exists
//!
//! Ethan, 2026-09-01: "some workers that are isolated, even if I explicitly say
//! to override the group boundary, are not able to do so. We should leverage the
//! existing approval process that we have."
//!
//! The trigger was a real one. A peer session relayed "Ethan authorizes crossing
//! group boundaries" as BODY TEXT. That is not authorization and must never be
//! treated as such — provenance comes from the server stamp, never the message —
//! so the correct answer was to refuse. But refusing was ALSO the wrong end
//! state, because the owner really had said it and there was no mechanism that
//! could carry his intent to the machine. A rule with no truthful path for a
//! legitimate case is ethos rule 3, and it pushes people toward laundering the
//! permission through prose.
//!
//! So: the refusal stops being terminal. It mints a GRANT the owner can approve
//! from the dashboard, and the worker's retry then succeeds.
//!
//! # The mechanism, deliberately the email gate's
//!
//! `email_approval` already solved this shape for outbound mail and has been
//! load-bearing since AMUX-3510. This module reuses its properties rather than
//! inventing a second discipline:
//!
//!   * the request is FROZEN to disk (`~/.amux/grants/grn_<hex>.json`)
//!   * only a HUMAN releases it — a request carrying no worker origin, which is
//!     the dashboard. A worker cannot approve its own grant, which is the whole
//!     point
//!   * ONE-SHOT via atomic rename, so a raced double-approve loses cleanly
//!   * it EXPIRES (1h), so an unapproved ask does not become a standing power
//!   * every state change is greppable, and `fate()` answers "what became of
//!     grn_X" from the directory rather than guessing
//!
//! # Where it differs from email, and why
//!
//! The email gate replays a frozen PAYLOAD on approval: the draft is executed
//! verbatim, which is right for a message whose exact bytes the human read.
//!
//! A boundary override is not a message, it is a PERMISSION. Approving it mints
//! a single-use ALLOWANCE for that (origin -> target) pair, and the worker's own
//! retry consumes it. That keeps the human approving the thing they were
//! actually asked about — "may this lane reach that lane" — instead of a payload
//! they would have to re-read, and it avoids replaying a send through a handler
//! that was never built to be re-entered.
//!
//! Single-use is the important half. A grant that stayed open for its whole TTL
//! would be a standing cross-group channel opened by one yes.
//!
//! # What this is, honestly
//!
//! The same thing `email_approval` says it is: an accident guardrail, not a
//! security boundary. Any worker on this machine can read the auth token and
//! omit its own origin header, and a worker that does so is lying about its
//! identity — which the audit trail treats as a forensics flag. This closes the
//! honest case: a lane that hits a wall, asks, and waits.

use crate::config::{amux_home, now_f64};
use axum::extract::Path as AxPath;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Same window the email gate uses. An ask nobody answers in an hour has been
/// declined by silence, and a grant that outlived the conversation that
/// produced it is a permission nobody remembers giving.
pub const GRANT_TTL_S: f64 = 3600.0;

pub fn grants_dir(home: &Path) -> PathBuf {
    home.join("grants")
}

/// `grn_` + hex. Rejected rather than sanitised: the id is a path component.
pub fn valid_id(id: &str) -> bool {
    id.len() > 4
        && id.len() <= 48
        && id.starts_with("grn_")
        && id[4..].chars().all(|c| c.is_ascii_hexdigit())
}

fn new_id() -> String {
    let mut b = [0u8; 12];
    getrandom_bytes(&mut b);
    format!("grn_{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>())
}

fn getrandom_bytes(buf: &mut [u8]) {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Not a secret — an id only has to be unguessable enough not to collide and
    // not to be trivially enumerated in a directory listing.
    let mut seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
        as u64
        ^ (std::process::id() as u64) << 17;
    for b in buf.iter_mut() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *b = (seed >> 33) as u8;
    }
}

/// Record an ask. Returns the grant id to hand back to the refused caller.
pub fn create_grant(
    home: &Path,
    kind: &str,
    requested_by: &str,
    summary: &str,
    payload: Value,
) -> Option<String> {
    let dir = grants_dir(home);
    std::fs::create_dir_all(&dir).ok()?;
    let id = new_id();
    let doc = json!({
        "id": id,
        "kind": kind,
        "requested_by": requested_by,
        "summary": summary,
        "payload": payload,
        "created": now_f64(),
    });
    std::fs::write(dir.join(format!("{id}.json")), serde_json::to_vec_pretty(&doc).ok()?).ok()?;
    tracing::warn!(
        grant = %id, kind = %kind, requested_by = %requested_by,
        "[grant] a worker hit a permission wall and asked — pending owner approval"
    );
    Some(id)
}

pub enum Consume {
    Ready(Value),
    Expired,
    Gone,
}

/// One-shot: the rename is the lock. A raced second approver loses.
pub fn consume(home: &Path, id: &str) -> Consume {
    if !valid_id(id) {
        return Consume::Gone;
    }
    let dir = grants_dir(home);
    let live = dir.join(format!("{id}.json"));
    let Ok(raw) = std::fs::read_to_string(&live) else { return Consume::Gone };
    let Ok(doc) = serde_json::from_str::<Value>(&raw) else { return Consume::Gone };
    let created = doc.get("created").and_then(Value::as_f64).unwrap_or(0.0);
    if now_f64() - created > GRANT_TTL_S {
        let _ = std::fs::rename(&live, dir.join(format!("{id}.expired.json")));
        return Consume::Expired;
    }
    if std::fs::rename(&live, dir.join(format!("{id}.approved.json"))).is_err() {
        return Consume::Gone;
    }
    Consume::Ready(doc)
}

/// What became of a grant id, read off the directory rather than guessed.
pub fn fate(home: &Path, id: &str) -> String {
    if !valid_id(id) {
        return "not a grant id".into();
    }
    let dir = grants_dir(home);
    for (suffix, verdict) in [
        ("json", "pending"),
        ("approved.json", "approved"),
        ("rejected.json", "rejected"),
        ("expired.json", "expired"),
        ("used.json", "approved and used"),
    ] {
        if dir.join(format!("{id}.{suffix}")).exists() {
            return verdict.into();
        }
    }
    "unknown (no record of this id)".into()
}

/// Pending asks, freshest first, with the time left on each.
pub fn list_pending(home: &Path) -> Vec<Value> {
    let dir = grants_dir(home);
    let Ok(rd) = std::fs::read_dir(&dir) else { return vec![] };
    let mut out: Vec<Value> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        // Only a LIVE GRANT. Two things are excluded and the dot count could only
        // ever catch one of them:
        //   * `grn_X.approved.json` etc — the audit trail, not the queue
        //   * `allow_<origin>__<target>.json` — the ALLOWANCE an approval writes
        //
        // The first version filtered on `matches('.').count() != 1`, which an
        // allowance passes: it ends in .json and has exactly one dot. So an
        // approved allowance was listed as a pending grant and rendered in the
        // owner's banner as "? wants to message ? across a group boundary" with
        // no preview — asking a human to approve something unidentifiable, which
        // is worse than not asking. Seen live on allow_mvs-infra__ts-gke.json.
        //
        // The real discriminator is the `grn_` PREFIX, so use it.
        if !name.starts_with("grn_") || !name.ends_with(".json") || name.matches('.').count() != 1 {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(e.path()) else { continue };
        let Ok(mut doc) = serde_json::from_str::<Value>(&raw) else { continue };
        let created = doc.get("created").and_then(Value::as_f64).unwrap_or(0.0);
        let age = now_f64() - created;
        if age > GRANT_TTL_S {
            continue; // expired; swept on next consume
        }
        if let Some(obj) = doc.as_object_mut() {
            obj.insert("expires_in_s".into(), json!((GRANT_TTL_S - age) as i64));
        }
        out.push(doc);
    }
    out.sort_by(|a, b| {
        b.get("created")
            .and_then(Value::as_f64)
            .partial_cmp(&a.get("created").and_then(Value::as_f64))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

pub fn discard(home: &Path, id: &str, by: &str) -> Option<Value> {
    if !valid_id(id) {
        return None;
    }
    let dir = grants_dir(home);
    let live = dir.join(format!("{id}.json"));
    let raw = std::fs::read_to_string(&live).ok()?;
    let mut doc: Value = serde_json::from_str(&raw).ok()?;
    if let Some(o) = doc.as_object_mut() {
        o.insert("rejected_by".into(), json!(by));
        o.insert("rejected_at".into(), json!(now_f64()));
    }
    std::fs::write(dir.join(format!("{id}.rejected.json")), serde_json::to_vec_pretty(&doc).ok()?)
        .ok()?;
    std::fs::remove_file(&live).ok()?;
    Some(doc)
}

// ---------------------------------------------------------------------------
// Allowances: what an APPROVED cross-group grant becomes
// ---------------------------------------------------------------------------

fn allowance_path(home: &Path, origin: &str, target: &str) -> PathBuf {
    let safe = |s: &str| -> String {
        s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
    };
    grants_dir(home).join(format!("allow_{}__{}.json", safe(origin), safe(target)))
}

/// Approving a cross-group grant writes this. The refused worker's RETRY
/// consumes it.
pub fn write_allowance(home: &Path, origin: &str, target: &str, grant_id: &str) {
    let _ = std::fs::create_dir_all(grants_dir(home));
    let _ = std::fs::write(
        allowance_path(home, origin, target),
        serde_json::to_vec_pretty(&json!({
            "origin": origin, "target": target, "grant": grant_id, "created": now_f64(),
        }))
        .unwrap_or_default(),
    );
}

/// Take the allowance for this pair, if one is live. SINGLE USE — the file is
/// removed as it is read, so one yes opens one send and not a channel.
pub fn take_allowance(home: &Path, origin: &str, target: &str) -> Option<String> {
    let p = allowance_path(home, origin, target);
    let raw = std::fs::read_to_string(&p).ok()?;
    let doc: Value = serde_json::from_str(&raw).ok()?;
    let created = doc.get("created").and_then(Value::as_f64).unwrap_or(0.0);
    if now_f64() - created > GRANT_TTL_S {
        let _ = std::fs::remove_file(&p);
        return None;
    }
    // Remove FIRST: if the send then fails, the allowance is still spent. That is
    // the safe direction — a lost allowance costs one more ask, while a retained
    // one is a permission nobody granted twice.
    let _ = std::fs::remove_file(&p);
    let gid = doc.get("grant").and_then(Value::as_str).unwrap_or_default().to_string();
    tracing::warn!(
        origin = %origin, target = %target, grant = %gid,
        "[grant] cross-group send proceeding on a single-use owner approval"
    );
    Some(gid)
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// A request with NO worker origin is the human at the dashboard. Identical to
/// the email gate's discriminator, and it is the reason a worker cannot approve
/// its own ask.
fn is_worker_origin(headers: &HeaderMap) -> bool {
    ["x-amux-session", "x-amux-worker"]
        .iter()
        .any(|h| headers.get(*h).and_then(|v| v.to_str().ok()).is_some_and(|s| !s.trim().is_empty()))
}

async fn list() -> Response {
    Json(json!({ "pending": list_pending(&amux_home()), "ttl_s": GRANT_TTL_S as i64 }))
        .into_response()
}

async fn approve(headers: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    let home = amux_home();
    if is_worker_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "error": "a worker may not approve a grant",
                "why": "the whole point of the grant is that a human decided. Approval is a \
                        request with no X-Amux-Session / X-Amux-Worker header — the dashboard.",
            })),
        )
            .into_response();
    }
    match consume(&home, &id) {
        Consume::Ready(doc) => {
            let kind = doc.get("kind").and_then(Value::as_str).unwrap_or_default();
            let p = doc.get("payload").cloned().unwrap_or_else(|| json!({}));
            match kind {
                "cross_group_send" => {
                    let origin = p.get("origin").and_then(Value::as_str).unwrap_or_default();
                    let target = p.get("target").and_then(Value::as_str).unwrap_or_default();
                    write_allowance(&home, origin, target, &id);
                    Json(json!({
                        "ok": true,
                        "granted": kind,
                        "origin": origin,
                        "target": target,
                        "single_use": true,
                        "note": format!(
                            "{origin} may now send to {target} ONCE. It takes effect on their \
                             next attempt; nothing is sent by this approval itself."
                        ),
                    }))
                    .into_response()
                }
                other => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "ok": false,
                        "error": format!("grant kind '{other}' has no approval action"),
                        "why": "the grant was consumed and recorded, but this server does not \
                                know how to act on that kind — say so rather than report success",
                    })),
                )
                    .into_response(),
            }
        }
        Consume::Expired => (
            StatusCode::GONE,
            Json(json!({"ok": false, "error": "that grant expired", "fate": fate(&home, &id)})),
        )
            .into_response(),
        Consume::Gone => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": "no such pending grant", "fate": fate(&home, &id)})),
        )
            .into_response(),
    }
}

async fn reject(headers: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    let home = amux_home();
    if is_worker_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"ok": false, "error": "a worker may not reject a grant either"})),
        )
            .into_response();
    }
    match discard(&home, &id, "owner") {
        Some(_) => Json(json!({"ok": true, "rejected": id})).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": "no such pending grant", "fate": fate(&home, &id)})),
        )
            .into_response(),
    }
}

pub fn routes() -> Router<super::AppState> {
    Router::new()
        .route("/api/grants", get(list))
        .route("/api/grants/{id}/approve", post(approve))
        .route("/api/grants/{id}/reject", post(reject))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn mint(h: &Path) -> String {
        create_grant(
            h,
            "cross_group_send",
            "ts-gke",
            "ts-gke -> autodesk",
            json!({"origin": "ts-gke", "target": "autodesk", "preview": "hello"}),
        )
        .expect("minted")
    }

    /// THE PROPERTY THE WHOLE MODULE EXISTS FOR: one yes opens ONE send, not a
    /// channel. A grant that stayed usable for its TTL would be a standing
    /// cross-group route opened by a single approval.
    #[test]
    fn an_allowance_is_single_use() {
        let h = home();
        let id = mint(h.path());
        write_allowance(h.path(), "ts-gke", "autodesk", &id);
        assert_eq!(take_allowance(h.path(), "ts-gke", "autodesk").as_deref(), Some(id.as_str()));
        assert!(
            take_allowance(h.path(), "ts-gke", "autodesk").is_none(),
            "a second send must be refused again — one approval is one send"
        );
    }

    /// An allowance is scoped to the PAIR. Approving ts-gke -> autodesk must not
    /// let ts-gke reach anyone else, or the grant would be a general permission
    /// wearing a specific label.
    #[test]
    fn an_allowance_does_not_leak_to_another_target_or_origin() {
        let h = home();
        let id = mint(h.path());
        write_allowance(h.path(), "ts-gke", "autodesk", &id);
        assert!(take_allowance(h.path(), "ts-gke", "backend").is_none(), "other target");
        assert!(take_allowance(h.path(), "backend", "autodesk").is_none(), "other origin");
        assert!(take_allowance(h.path(), "ts-gke", "autodesk").is_some(), "the granted pair still works");
    }

    /// Approval is ONE-SHOT. A raced second approver must lose rather than mint
    /// a second allowance.
    #[test]
    fn consume_is_one_shot_and_the_loser_gets_gone() {
        let h = home();
        let id = mint(h.path());
        assert!(matches!(consume(h.path(), &id), Consume::Ready(_)));
        assert!(matches!(consume(h.path(), &id), Consume::Gone));
        assert_eq!(fate(h.path(), &id), "approved");
    }

    /// An unanswered ask does not become a standing power.
    #[test]
    fn a_grant_expires_and_says_so() {
        let h = home();
        let id = mint(h.path());
        // Age it past the window by rewriting `created`.
        let p = grants_dir(h.path()).join(format!("{id}.json"));
        let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        doc["created"] = json!(now_f64() - GRANT_TTL_S - 5.0);
        std::fs::write(&p, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
        assert!(matches!(consume(h.path(), &id), Consume::Expired));
        assert_eq!(fate(h.path(), &id), "expired");
        assert!(list_pending(h.path()).is_empty(), "an expired ask is not a pending ask");
    }

    /// An expired ALLOWANCE is not usable either. Without this the grant's TTL
    /// would bound only the ask, and an approval nobody used would sit open.
    #[test]
    fn an_expired_allowance_is_not_usable() {
        let h = home();
        write_allowance(h.path(), "a", "b", "grn_dead");
        let p = grants_dir(h.path()).join("allow_a__b.json");
        let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        doc["created"] = json!(now_f64() - GRANT_TTL_S - 5.0);
        std::fs::write(&p, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
        assert!(take_allowance(h.path(), "a", "b").is_none());
        // CONTROL: a FRESH one is usable, or the cell above passes for a
        // take_allowance that never returns anything.
        write_allowance(h.path(), "a", "b", "grn_live");
        assert_eq!(take_allowance(h.path(), "a", "b").as_deref(), Some("grn_live"));
    }

    /// The worker-origin discriminator is the reason a lane cannot approve its
    /// own ask. Asserted on the header predicate directly.
    #[test]
    fn a_request_carrying_a_worker_origin_is_not_the_human() {
        let mut h = HeaderMap::new();
        assert!(!is_worker_origin(&h), "no headers = the dashboard");
        h.insert("x-amux-session", "ts-gke".parse().unwrap());
        assert!(is_worker_origin(&h));
        let mut h2 = HeaderMap::new();
        h2.insert("x-amux-worker", "ts-gke".parse().unwrap());
        assert!(is_worker_origin(&h2));
        // An EMPTY header is not an origin — otherwise a worker could send the
        // header blank and be read as a human by a naive presence check.
        let mut h3 = HeaderMap::new();
        h3.insert("x-amux-session", "   ".parse().unwrap());
        assert!(!is_worker_origin(&h3), "blank is absent");
    }

    /// Ids are path components. Rejected, never sanitised.
    #[test]
    fn ids_are_validated_not_cleaned() {
        assert!(valid_id("grn_00ff11"));
        assert!(!valid_id("grn_../../etc/passwd"));
        assert!(!valid_id("apr_00ff"), "an email approval id is not a grant id");
        assert!(!valid_id("grn_"), "no body");
        assert!(!valid_id("grn_zz"), "not hex");
    }

    /// Rejecting removes it from the queue and leaves the audit trail.
    #[test]
    fn reject_clears_the_queue_and_is_recorded() {
        let h = home();
        let id = mint(h.path());
        assert_eq!(list_pending(h.path()).len(), 1);
        assert!(discard(h.path(), &id, "owner").is_some());
        assert!(list_pending(h.path()).is_empty());
        assert_eq!(fate(h.path(), &id), "rejected");
        assert!(matches!(consume(h.path(), &id), Consume::Gone), "a rejected ask cannot be approved");
    }

    /// AN ALLOWANCE IS NOT A PENDING GRANT. Both live in the same directory, both
    /// end in `.json`, and both have exactly one dot — so the original dot-count
    /// filter listed an approved allowance as an ask, and the owner's banner
    /// rendered "? wants to message ? across a group boundary" with no preview.
    #[test]
    fn an_allowance_file_is_not_listed_as_a_pending_grant() {
        let h = home();
        write_allowance(h.path(), "mvs-infra", "ts-gke", "grn_abc");
        assert!(
            list_pending(h.path()).is_empty(),
            "an allowance must never appear in the approval queue"
        );
        // CONTROL: a real grant beside it IS listed, or the assertion above
        // passes for a filter that excludes everything.
        mint(h.path());
        let p = list_pending(h.path());
        assert_eq!(p.len(), 1, "the real ask must still be listed");
        assert_eq!(p[0]["requested_by"], json!("ts-gke"));
    }

    /// The pending list carries what a human needs to decide: who asked, what
    /// for, and how long is left.
    #[test]
    fn the_pending_list_says_who_asked_and_for_what() {
        let h = home();
        mint(h.path());
        let p = list_pending(h.path());
        assert_eq!(p.len(), 1);
        assert_eq!(p[0]["requested_by"], json!("ts-gke"));
        assert_eq!(p[0]["kind"], json!("cross_group_send"));
        assert_eq!(p[0]["payload"]["target"], json!("autodesk"));
        assert!(p[0]["expires_in_s"].as_i64().unwrap_or(0) > 0, "a human needs the clock");
        // The audit siblings must not appear as pending.
        assert!(matches!(consume(h.path(), p[0]["id"].as_str().unwrap()), Consume::Ready(_)));
        assert!(list_pending(h.path()).is_empty(), ".approved.json is history, not queue");
    }
}
