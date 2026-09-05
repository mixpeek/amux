//! Org API (SPA long-tail port): `/api/org*` over the LIVE `org` /
//! `org_members` / `org_invites` tables, route- and field-compatible with
//! the Python handlers — the cloud gateway consumes these shapes.
//!
//! Parity decisions, recorded so they are not "fixed" later:
//! - `GET /api/org` LAZILY CREATES the singleton `('default', 'My
//!   Workspace')` row, exactly like Python's `_get_org()` — the org exists
//!   the first time anyone asks about it.
//! - Invite URLs are built from the REQUEST's `Host` header +
//!   `X-Forwarded-Proto` (https only when the gateway says https, http
//!   otherwise, default host `localhost:<canonical port>`) — Python's shape
//!   `f"{scheme}://{host}/invite/{token}"`, with the fallback host derived
//!   from this server's own port instead of Python's 8822 literal.
//! - Tokens are `secrets.token_urlsafe(24)`-shaped (24 CSPRNG bytes,
//!   base64url, no padding — 32 chars); invites expire in 7 days; the
//!   invites list hides used AND expired rows.
//! - DELETE member/invite answer `{"ok": true}` without existence checks
//!   (Python does not 404 there).
//! - NOT ported (named deviation): the public `/invite/{token}` HTML
//!   landing page and its POST mark-used/join flow. Those live outside
//!   `/api/org` and outside auth; this module is the API surface only, so
//!   accepting an invite still needs the Python server (or a follow-up
//!   port of `/invite/*` onto the public router).

use super::calendar::query_rows_json;
use super::AppState;
use crate::db::{PendingEvent, WriteOutcome};
use crate::integrations::email::base64url_nopad;
use amux_core::revision::{EntityType, MutationKind};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use p256::elliptic_curve::rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(get_org).patch(patch_org))
        .route("/members", get(list_members))
        .route("/members/{id}", axum::routing::delete(delete_member))
        .route("/invites", get(list_invites).post(create_invite))
        .route("/invites/{token}", axum::routing::delete(delete_invite))
}

// ---- shared helpers -------------------------------------------------------

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

use super::internal;

fn ev(entity: &str, id: &str, mutation: MutationKind) -> PendingEvent {
    PendingEvent {
        entity_type: EntityType::Other(entity.into()),
        entity_id: id.to_string(),
        mutation,
        payload: None,
    }
}

/// `secrets.token_urlsafe(n)`: n CSPRNG bytes, base64url, no padding.
fn token_urlsafe(nbytes: usize) -> String {
    let mut bytes = vec![0u8; nbytes];
    OsRng.fill_bytes(&mut bytes);
    base64url_nopad(&bytes)
}

/// Python: `scheme = "https" if X-Forwarded-Proto == "https" else "http"`,
/// host from the Host header. The fallback host is this server's OWN port
/// (`config::canonical_port()`), not Python's 8822 literal — an invite link is
/// mailed to a person and outlives the process, so minting it against the
/// retired address hands out a URL with an expiry date on it.
fn base_url(headers: &HeaderMap) -> String {
    let fallback = format!("localhost:{}", crate::config::canonical_port());
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(&fallback);
    let scheme = if headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        == Some("https")
    {
        "https"
    } else {
        "http"
    };
    format!("{scheme}://{host}")
}

/// Python `_get_org()`: SELECT-or-INSERT the singleton row. Returns whether
/// the row was created (so the write's applied/events stay honest).
fn ensure_org(conn: &rusqlite::Connection) -> rusqlite::Result<bool> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM org WHERE id='default'", [], |r| r.get(0))?;
    if n == 0 {
        conn.execute(
            "INSERT INTO org (id, name, created_at) VALUES ('default','My Workspace',?1)",
            [chrono::Utc::now().timestamp()],
        )?;
        return Ok(true);
    }
    Ok(false)
}

// ---- GET /api/org ---------------------------------------------------------

pub async fn get_org(State(state): State<AppState>) -> Response {
    let slot: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let created = ensure_org(conn)?;
            let mut org = query_rows_json(conn, "SELECT * FROM org WHERE id='default'", &[])?
                .pop()
                .unwrap_or_else(|| json!({}));
            let members: i64 =
                conn.query_row("SELECT COUNT(*) FROM org_members", [], |r| r.get(0))?;
            let now = chrono::Utc::now().timestamp();
            let invites: i64 = conn.query_row(
                "SELECT COUNT(*) FROM org_invites WHERE used_at IS NULL AND expires_at > ?1",
                [now],
                |r| r.get(0),
            )?;
            org["member_count"] = json!(members);
            org["invite_count"] = json!(invites);
            *slot_w.lock().expect("slot") = Some(org);
            let events =
                if created { vec![ev("org", "default", MutationKind::Created)] } else { vec![] };
            Ok(WriteOutcome { applied: created, events })
        })
        .await;
    match write {
        Ok(_) => {
            let org = slot.lock().expect("slot").take().unwrap_or_else(|| json!({}));
            Json(org).into_response()
        }
        Err(e) => internal(e),
    }
}

// ---- PATCH /api/org -------------------------------------------------------

pub async fn patch_org(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    // Python: body.get("name", "").strip()[:80] — char truncation.
    let name: String = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .take(80)
        .collect();
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "name required" }));
    }
    let name_w = name.clone();
    let write = state
        .store
        .write_async(move |conn| {
            ensure_org(conn)?;
            conn.execute("UPDATE org SET name=?1 WHERE id='default'", [&name_w])?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![ev("org", "default", MutationKind::Updated)],
            })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true, "name": name })).into_response(),
        Err(e) => internal(e),
    }
}

// ---- GET /api/org/members -------------------------------------------------

pub async fn list_members(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        Ok(query_rows_json(
            &conn,
            "SELECT id, email, name, role, joined_at FROM org_members ORDER BY joined_at",
            &[],
        )?)
    })
    .await;
    match joined {
        Ok(Ok(rows)) => Json(Value::Array(rows)).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---- DELETE /api/org/members/{id} -----------------------------------------

pub async fn delete_member(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let id_w = id.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let n = conn.execute("DELETE FROM org_members WHERE id=?1", [&id_w])?;
            let events = if n > 0 {
                vec![ev("org_member", &id_w, MutationKind::Deleted)]
            } else {
                vec![]
            };
            Ok(WriteOutcome { applied: n > 0, events })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

// ---- GET /api/org/invites -------------------------------------------------

pub async fn list_invites(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let base = base_url(&headers);
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        let now = chrono::Utc::now().timestamp();
        let mut rows = query_rows_json(
            &conn,
            "SELECT token, email, created_at, expires_at, used_at, used_by \
             FROM org_invites WHERE used_at IS NULL AND expires_at > ?1 \
             ORDER BY created_at DESC",
            &[&now],
        )?;
        for r in &mut rows {
            let tok = r.get("token").and_then(Value::as_str).unwrap_or("").to_string();
            r["url"] = json!(format!("{base}/invite/{tok}"));
        }
        Ok(rows)
    })
    .await;
    match joined {
        Ok(Ok(rows)) => Json(Value::Array(rows)).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---- POST /api/org/invites ------------------------------------------------

pub async fn create_invite(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    // Python: `body.get("email", "").strip().lower() or None`.
    let email: Option<String> = body
        .get("email")
        .and_then(Value::as_str)
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty());
    let token = token_urlsafe(24);
    let now = chrono::Utc::now().timestamp();
    let expires = now + 7 * 86400;
    let token_w = token.clone();
    let write = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO org_invites (token, email, created_at, expires_at) VALUES (?1,?2,?3,?4)",
                rusqlite::params![token_w, email, now, expires],
            )?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![ev("org_invite", &token_w, MutationKind::Created)],
            })
        })
        .await;
    match write {
        Ok(_) => {
            let url = format!("{}/invite/{token}", base_url(&headers));
            (
                StatusCode::CREATED,
                Json(json!({ "token": token, "url": url, "expires_at": expires })),
            )
                .into_response()
        }
        Err(e) => internal(e),
    }
}

// ---- DELETE /api/org/invites/{token} --------------------------------------

pub async fn delete_invite(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let tok_w = token.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let n = conn.execute("DELETE FROM org_invites WHERE token=?1", [&tok_w])?;
            let events = if n > 0 {
                vec![ev("org_invite", &tok_w, MutationKind::Deleted)]
            } else {
                vec![]
            };
            Ok(WriteOutcome { applied: n > 0, events })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------------
// Tests — temp-DB stores; Python-shaped rows round-trip column by column.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Store;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn app() -> (axum::Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("org-api-test.db")).unwrap();
        let state = AppState {
            store: Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let router = Router::new().nest("/api/org", routes()).with_state(state);
        (router, dir)
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let req = match body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, v)
    }

    #[tokio::test]
    async fn get_lazily_creates_the_default_org() {
        let (app, dir) = app();
        let (st, v) = send(&app, "GET", "/api/org", None, &[]).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["id"], json!("default"));
        assert_eq!(v["name"], json!("My Workspace"));
        assert_eq!(v["member_count"], json!(0));
        assert_eq!(v["invite_count"], json!(0));
        assert!(v["created_at"].as_i64().unwrap() > 0);
        // The row is persisted, not synthesized per-request.
        let conn = rusqlite::Connection::open(dir.path().join("org-api-test.db")).unwrap();
        let n: i64 =
            conn.query_row("SELECT COUNT(*) FROM org WHERE id='default'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn patch_renames_with_python_validation_and_80_char_cap() {
        let (app, _dir) = app();
        let (st, e) = send(&app, "PATCH", "/api/org", Some(json!({ "name": "  " })), &[]).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(e["error"], json!("name required"));

        let long = "x".repeat(100);
        let (st, r) = send(&app, "PATCH", "/api/org", Some(json!({ "name": long })), &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r["ok"], json!(true));
        assert_eq!(r["name"].as_str().unwrap().len(), 80);
        let (_, v) = send(&app, "GET", "/api/org", None, &[]).await;
        assert_eq!(v["name"].as_str().unwrap().len(), 80);
    }

    #[tokio::test]
    async fn python_shaped_member_and_invite_rows_round_trip() {
        let (app, dir) = app();
        {
            // Rows exactly as the Python server writes them.
            let conn = rusqlite::Connection::open(dir.path().join("org-api-test.db")).unwrap();
            conn.execute(
                "INSERT INTO org (id, name, created_at) VALUES ('default','Mixpeek HQ',1753000000)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO org_members (id, email, name, role, joined_at) \
                 VALUES ('tok_member_0001','a@x.co',NULL,'member',1753000100)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO org_members (id, email, name, role, joined_at) \
                 VALUES ('tok_member_0002','b@x.co','Bee','admin',1753000050)",
                [],
            )
            .unwrap();
            let now = chrono::Utc::now().timestamp();
            conn.execute(
                "INSERT INTO org_invites (token, email, created_at, expires_at) \
                 VALUES ('livetokenlivetokenlivetoken00001','c@x.co',?1,?2)",
                rusqlite::params![now, now + 86400],
            )
            .unwrap();
            // Used and expired invites must be hidden from the list.
            conn.execute(
                "INSERT INTO org_invites (token, email, created_at, expires_at, used_at, used_by) \
                 VALUES ('usedtoken0000000000000000000000x',NULL,?1,?2,?1,'d@x.co')",
                rusqlite::params![now, now + 86400],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO org_invites (token, email, created_at, expires_at) \
                 VALUES ('expiredtoken00000000000000000000',NULL,?1,?2)",
                rusqlite::params![now - 86400 * 8, now - 60],
            )
            .unwrap();
        }

        // GET /api/org counts members + only-live invites.
        let (_, org) = send(&app, "GET", "/api/org", None, &[]).await;
        assert_eq!(org["name"], json!("Mixpeek HQ"));
        assert_eq!(org["created_at"], json!(1753000000));
        assert_eq!(org["member_count"], json!(2));
        assert_eq!(org["invite_count"], json!(1));

        // Members: joined_at ASC ordering, exact Python projection.
        let (st, m) = send(&app, "GET", "/api/org/members", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        let arr = m.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], json!("tok_member_0002"));
        assert_eq!(arr[0]["name"], json!("Bee"));
        assert_eq!(arr[0]["role"], json!("admin"));
        assert_eq!(arr[0]["joined_at"], json!(1753000050));
        assert_eq!(arr[1]["id"], json!("tok_member_0001"));
        assert_eq!(arr[1]["name"], Value::Null);

        // Invites: live row only, URL built from Host + X-Forwarded-Proto.
        let (st, inv) = send(
            &app,
            "GET",
            "/api/org/invites",
            None,
            &[("Host", "cloud.amux.io"), ("X-Forwarded-Proto", "https")],
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let arr = inv.as_array().unwrap();
        assert_eq!(arr.len(), 1, "{inv}");
        assert_eq!(arr[0]["token"], json!("livetokenlivetokenlivetoken00001"));
        assert_eq!(arr[0]["email"], json!("c@x.co"));
        assert_eq!(arr[0]["used_at"], Value::Null);
        assert_eq!(
            arr[0]["url"],
            json!("https://cloud.amux.io/invite/livetokenlivetokenlivetoken00001")
        );

        // Default host/scheme when the headers are absent.
        let (_, inv2) = send(&app, "GET", "/api/org/invites", None, &[]).await;
        let url = inv2[0]["url"].as_str().unwrap();
        // Derived, not literal: the fallback follows this server's own port,
        // so hardcoding one here would pin the test to a deployment.
        let want = format!("http://localhost:{}/invite/", crate::config::canonical_port());
        assert!(url.starts_with(&want), "{url} should start with {want}");
    }

    #[tokio::test]
    async fn create_invite_mints_python_shaped_token_and_expiry() {
        let (app, dir) = app();
        let before = chrono::Utc::now().timestamp();
        let (st, r) = send(
            &app,
            "POST",
            "/api/org/invites",
            Some(json!({ "email": "  NewHire@X.Co " })),
            &[("Host", "myhost:9"), ("X-Forwarded-Proto", "https")],
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{r}");
        let token = r["token"].as_str().unwrap();
        // token_urlsafe(24) shape: 32 urlsafe chars, no padding.
        assert_eq!(token.len(), 32, "{token}");
        assert!(token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_eq!(r["url"], json!(format!("https://myhost:9/invite/{token}")));
        let exp = r["expires_at"].as_i64().unwrap();
        assert!(exp >= before + 7 * 86400 && exp <= before + 7 * 86400 + 60, "{exp}");

        // Stored row: email lowercased; empty email stores NULL.
        let conn = rusqlite::Connection::open(dir.path().join("org-api-test.db")).unwrap();
        let email: Option<String> = conn
            .query_row("SELECT email FROM org_invites WHERE token=?1", [token], |r| r.get(0))
            .unwrap();
        assert_eq!(email.as_deref(), Some("newhire@x.co"));
        let (st, r2) = send(&app, "POST", "/api/org/invites", Some(json!({})), &[]).await;
        assert_eq!(st, StatusCode::CREATED);
        let email2: Option<String> = conn
            .query_row(
                "SELECT email FROM org_invites WHERE token=?1",
                [r2["token"].as_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(email2, None);
    }

    #[tokio::test]
    async fn deletes_answer_ok_and_remove_rows() {
        let (app, dir) = app();
        {
            let conn = rusqlite::Connection::open(dir.path().join("org-api-test.db")).unwrap();
            conn.execute(
                "INSERT INTO org_members (id, email, role, joined_at) \
                 VALUES ('mem1','gone@x.co','member',1)",
                [],
            )
            .unwrap();
            let now = chrono::Utc::now().timestamp();
            conn.execute(
                "INSERT INTO org_invites (token, created_at, expires_at) VALUES ('tok1',?1,?2)",
                rusqlite::params![now, now + 100],
            )
            .unwrap();
        }
        let (st, r) = send(&app, "DELETE", "/api/org/members/mem1", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r["ok"], json!(true));
        let (st, r) = send(&app, "DELETE", "/api/org/invites/tok1", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r["ok"], json!(true));
        // Python answers ok for a missing row too.
        let (st, r) = send(&app, "DELETE", "/api/org/members/never-existed", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r["ok"], json!(true));
        let conn = rusqlite::Connection::open(dir.path().join("org-api-test.db")).unwrap();
        let m: i64 = conn.query_row("SELECT COUNT(*) FROM org_members", [], |r| r.get(0)).unwrap();
        let i: i64 = conn.query_row("SELECT COUNT(*) FROM org_invites", [], |r| r.get(0)).unwrap();
        assert_eq!((m, i), (0, 0));
    }
}
