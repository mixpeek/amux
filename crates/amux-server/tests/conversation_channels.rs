//! Channel settings in Messages (AA-52): the owner and admin members create,
//! rename, archive and make channels private, and add or remove people.
//! Everyone else, workers included, gets 403 `channel_admin_only`.
//!
//! Through the real router, real invite cookies, a temp store and a temp amux
//! home holding one worker (`ed`, group `wexus`). Never ~/.amux.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

fn pin_home() {
    static HOME: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    let home = HOME.get_or_init(|| {
        let h = tempfile::tempdir().unwrap().keep();
        std::fs::create_dir_all(h.join("sessions")).unwrap();
        std::fs::write(h.join("sessions/ed.env"), "CC_TAGS=\"wexus\"\n").unwrap();
        h
    });
    std::env::set_var("AMUX_HOME", home);
}

fn server() -> (axum::Router, tempfile::TempDir) {
    pin_home();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("channels-test.db")).unwrap();
    let state = AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    (router(state), dir)
}

async fn send(app: &axum::Router, method: &str, path: &str, body: Option<Value>, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let req = match body {
        Some(v) => b.header(header::CONTENT_TYPE, "application/json").body(Body::from(v.to_string())).unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Invite a global member, accept the invite, and return their cookie.
async fn member(app: &axum::Router, email: &str) -> String {
    let (st, invite) = send(app, "POST", "/api/org/invites",
        Some(json!({"email": email, "scope_level": "global", "grant_host_access": true})),
        &[("host", "test-host:8824"), ("x-forwarded-proto", "https")]).await;
    assert_eq!(st, StatusCode::CREATED, "{invite}");
    let token = invite["token"].as_str().unwrap();
    let req = Request::builder()
        .method("POST")
        .uri(format!("/invite/{token}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("email={}&name=", email.replace('@', "%40"))))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    res.headers()[header::SET_COOKIE].to_str().unwrap().split(';').next().unwrap().to_string()
}

async fn make_admin(app: &axum::Router, email: &str) {
    let (_, rows) = send(app, "GET", "/api/org/members", None, &[]).await;
    let id = rows.as_array().unwrap().iter().find(|r| r["email"] == email).unwrap()["id"].as_str().unwrap().to_string();
    let (st, v) = send(app, "PATCH", &format!("/api/org/members/{id}"), Some(json!({"role": "admin"})), &[]).await;
    assert_eq!(st, StatusCode::OK, "{v}");
}

fn convs(list: &Value) -> Vec<String> {
    list["channels"].as_array().unwrap().iter().map(|c| c["conv"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn a_member_and_a_worker_are_refused_every_channel_setting() {
    let (app, _d) = server();
    let mate = member(&app, "mate@example.com").await;
    let as_mate = [("cookie", mate.as_str())];
    let as_worker = [("x-amux-session", "ed")];
    for who in [&as_mate[..], &as_worker[..]] {
        for (method, path, body) in [
            ("GET", "/api/conversations/channels", None),
            ("POST", "/api/conversations/channels", Some(json!({"name": "sneaky"}))),
            ("PATCH", "/api/conversations/channels/wexus", Some(json!({"archived": true}))),
            ("GET", "/api/conversations/channels/wexus/members", None),
            ("POST", "/api/conversations/channels/wexus/members", Some(json!({"email": "mate@example.com"}))),
            ("DELETE", "/api/conversations/channels/wexus/members/mate@example.com", None),
        ] {
            let (st, v) = send(&app, method, path, body, who).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{who:?} {method} {path}: {v}");
            assert_eq!(v["code"], "channel_admin_only", "{method} {path}: {v}");
        }
    }
    let (_, list) = send(&app, "GET", "/api/conversations", None, &as_mate).await;
    assert_eq!(list["me"]["can_admin_channels"], false, "{list}");
    // Nothing a refused call asked for happened.
    let (_, owner_view) = send(&app, "GET", "/api/conversations/channels", None, &[]).await;
    assert!(!convs(&owner_view).contains(&"g:sneaky".to_string()), "{owner_view}");
    assert!(convs(&list).contains(&"g:wexus".to_string()), "{list}");
}

#[tokio::test]
async fn the_owner_and_an_admin_create_and_manage_a_channel() {
    let (app, _d) = server();
    let mate = member(&app, "mate@example.com").await;
    let boss = member(&app, "boss@example.com").await;
    make_admin(&app, "boss@example.com").await;
    let as_mate = [("cookie", mate.as_str())];
    let as_boss = [("cookie", boss.as_str())];

    let (_, me) = send(&app, "GET", "/api/conversations", None, &as_boss).await;
    assert_eq!(me["me"]["can_admin_channels"], true, "{me}");
    let (_, me) = send(&app, "GET", "/api/conversations", None, &[]).await;
    assert_eq!(me["me"]["can_admin_channels"], true, "{me}");

    // Validation.
    let (st, v) = send(&app, "POST", "/api/conversations/channels", Some(json!({"name": "Bad Name!"})), &[]).await;
    assert_eq!((st, v["code"].as_str()), (StatusCode::BAD_REQUEST, Some("channel_name_invalid")), "{v}");
    let (st, v) = send(&app, "POST", "/api/conversations/channels", Some(json!({"name": "wexus"})), &[]).await;
    assert_eq!((st, v["code"].as_str()), (StatusCode::CONFLICT, Some("channel_exists")), "{v}");
    let (st, v) = send(&app, "PATCH", "/api/conversations/channels/general", Some(json!({"archived": true})), &[]).await;
    assert_eq!((st, v["code"].as_str()), (StatusCode::BAD_REQUEST, Some("general_is_permanent")), "{v}");

    // An admin creates a private channel: a global member does not see it.
    let (st, v) = send(&app, "POST", "/api/conversations/channels", Some(json!({"name": "launch", "private": true})), &as_boss).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let (_, all) = send(&app, "GET", "/api/conversations/channels", None, &as_boss).await;
    assert_eq!(all["measured"], true, "{all}");
    assert!(all["n_considered"].as_u64().unwrap() >= 3, "{all}");
    assert!(convs(&all).contains(&"g:launch".to_string()), "{all}");
    let (_, list) = send(&app, "GET", "/api/conversations", None, &as_mate).await;
    assert!(!convs(&list).contains(&"g:launch".to_string()), "private channel leaked: {list}");
    let (st, _) = send(&app, "POST", "/api/conversations/g:launch/posts", Some(json!({"body": "hi"})), &as_mate).await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // Added by hand: now they read and post there.
    let (st, v) = send(&app, "POST", "/api/conversations/channels/launch/members", Some(json!({"email": "mate@example.com"})), &as_boss).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!((v["member"].as_bool(), v["via"].as_str()), (Some(true), Some("added")), "{v}");
    let (_, list) = send(&app, "GET", "/api/conversations", None, &as_mate).await;
    assert!(convs(&list).contains(&"g:launch".to_string()), "{list}");
    let (st, v) = send(&app, "POST", "/api/conversations/g:launch/posts", Some(json!({"body": "hi"})), &as_mate).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let (_, m) = send(&app, "GET", "/api/conversations/channels/launch/members", None, &[]).await;
    assert_eq!(m["measured"], true, "{m}");
    let row = m["people"].as_array().unwrap().iter().find(|p| p["email"] == "mate@example.com").unwrap().clone();
    assert_eq!((row["member"].as_bool(), row["via"].as_str()), (Some(true), Some("added")), "{m}");
    let owner = m["people"].as_array().unwrap().iter().find(|p| p["email"] == "owner").unwrap().clone();
    assert_eq!(owner["via"], "owner", "{m}");

    // Removed from a channel their team covers.
    let (st, v) = send(&app, "DELETE", "/api/conversations/channels/wexus/members/mate@example.com", None, &[]).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!((v["member"].as_bool(), v["via"].as_str()), (Some(false), Some("removed")), "{v}");
    let (_, list) = send(&app, "GET", "/api/conversations", None, &as_mate).await;
    assert!(!convs(&list).contains(&"g:wexus".to_string()), "{list}");
    let (st, _) = send(&app, "GET", "/api/conversations/g:wexus/posts", None, &as_mate).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // Re-adding returns them to the team rule: no row left behind.
    let (_, v) = send(&app, "POST", "/api/conversations/channels/wexus/members", Some(json!({"email": "mate@example.com"})), &[]).await;
    assert_eq!(v["via"], "team", "{v}");
    let (st, v) = send(&app, "POST", "/api/conversations/channels/wexus/members", Some(json!({"email": "nobody@example.com"})), &[]).await;
    assert_eq!((st, v["code"].as_str()), (StatusCode::NOT_FOUND, Some("unknown_person")), "{v}");
    let (st, v) = send(&app, "DELETE", "/api/conversations/channels/wexus/members/owner", None, &[]).await;
    assert_eq!((st, v["code"].as_str()), (StatusCode::BAD_REQUEST, Some("owner_always_member")), "{v}");

    // Rename and archive. Archived: gone for everyone, and closed to posts.
    let (st, v) = send(&app, "PATCH", "/api/conversations/channels/launch", Some(json!({"title": "Launch room", "archived": true})), &[]).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!((v["channel"]["title"].as_str(), v["channel"]["archived"].as_bool()), (Some("Launch room"), Some(true)), "{v}");
    let (_, list) = send(&app, "GET", "/api/conversations", None, &as_mate).await;
    assert!(!convs(&list).contains(&"g:launch".to_string()), "{list}");
    let (st, _) = send(&app, "POST", "/api/conversations/g:launch/posts", Some(json!({"body": "late"})), &[]).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "an archived channel took a post");
    let (st, v) = send(&app, "POST", "/api/conversations/channels", Some(json!({"name": "launch"})), &[]).await;
    assert_eq!((st, v["code"].as_str()), (StatusCode::CONFLICT, Some("channel_exists")), "{v}");
    // Restored, with its people and its title.
    let (_, _) = send(&app, "PATCH", "/api/conversations/channels/launch", Some(json!({"archived": false})), &as_boss).await;
    let (_, list) = send(&app, "GET", "/api/conversations", None, &as_mate).await;
    let launch = list["channels"].as_array().unwrap().iter().find(|c| c["conv"] == "g:launch").cloned();
    assert_eq!(launch.as_ref().map(|c| c["title"].clone()), Some(json!("Launch room")), "{list}");
}

fn pin_ids(page: &Value) -> Vec<i64> {
    page["pins"].as_array().map(|a| a.iter().filter_map(|p| p["id"].as_i64()).collect()).unwrap_or_default()
}

/// Pinning a message inside a channel (Ethan, 2026-10-29): anyone who can read
/// the channel pins or unpins a post, a reply included; every page of posts
/// carries the pins, newest pin first, with who pinned; someone who cannot
/// read the channel can neither pin nor see its pins, and a deleted post drops
/// out of the list.
#[tokio::test]
async fn a_reader_pins_a_message_in_a_channel_and_everyone_there_sees_it() {
    let (app, _d) = server();
    let mate = member(&app, "pinmate@example.com").await;
    let out = member(&app, "pinout@example.com").await;
    let as_mate = [("cookie", mate.as_str())];
    let as_out = [("cookie", out.as_str())];
    let (st, v) = send(&app, "POST", "/api/conversations/channels", Some(json!({"name": "pins", "private": true})), &[]).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let (st, v) = send(&app, "POST", "/api/conversations/channels/pins/members", Some(json!({"email": "pinmate@example.com"})), &[]).await;
    assert_eq!(st, StatusCode::OK, "{v}");

    let (_, a) = send(&app, "POST", "/api/conversations/g:pins/posts", Some(json!({"body": "the launch date is Friday"})), &[]).await;
    let (_, b) = send(&app, "POST", "/api/conversations/g:pins/posts", Some(json!({"body": "unrelated chatter"})), &as_mate).await;
    let (a, b) = (a["id"].as_i64().unwrap(), b["id"].as_i64().unwrap());
    let (_, r) = send(&app, "POST", "/api/conversations/g:pins/posts", Some(json!({"body": "the venue is booked", "parent_id": a})), &as_mate).await;
    let r = r["id"].as_i64().unwrap();

    // Nothing pinned yet: the page says so.
    let (_, page) = send(&app, "GET", "/api/conversations/g:pins/posts", None, &as_mate).await;
    assert_eq!(page["pins"], json!([]), "{page}");

    let (st, v) = send(&app, "POST", &format!("/api/conversations/posts/{a}/pin"), Some(json!({"pinned": true})), &[]).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(pin_ids(&v), vec![a]);
    // A member pins a reply; it lists first, with who pinned it.
    let (st, v) = send(&app, "POST", &format!("/api/conversations/posts/{r}/pin"), Some(json!({})), &as_mate).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, page) = send(&app, "GET", &format!("/api/conversations/g:pins/posts?after={b}"), None, &[]).await;
    assert_eq!(pin_ids(&page), vec![r, a], "an incremental page still carries the pins: {page}");
    let reply = &page["pins"][0];
    assert_eq!((reply["parent_id"].as_i64(), reply["pinned_by"].as_str()), (Some(a), Some("person:pinmate@example.com")), "{page}");
    assert_eq!(reply["body"], "the venue is booked");
    // Pinning twice keeps the first pin.
    let (st, _) = send(&app, "POST", &format!("/api/conversations/posts/{a}/pin"), Some(json!({"pinned": true})), &as_mate).await;
    assert_eq!(st, StatusCode::OK);
    let (_, page) = send(&app, "GET", "/api/conversations/g:pins/posts", None, &[]).await;
    assert_eq!(page["pins"][1]["pinned_by"], "person:owner", "{page}");

    // Someone outside the private channel: no pin, and no pins to read.
    let (st, _) = send(&app, "POST", &format!("/api/conversations/posts/{b}/pin"), Some(json!({"pinned": true})), &as_out).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = send(&app, "GET", "/api/conversations/g:pins/posts", None, &as_out).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = send(&app, "POST", "/api/conversations/posts/999999/pin", Some(json!({})), &[]).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Unpinned, and a deleted pinned post drops out.
    let (st, v) = send(&app, "POST", &format!("/api/conversations/posts/{a}/pin"), Some(json!({"pinned": false})), &as_mate).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(pin_ids(&v), vec![r]);
    let (st, _) = send(&app, "DELETE", &format!("/api/conversations/posts/{r}"), None, &as_mate).await;
    assert_eq!(st, StatusCode::OK);
    let (_, page) = send(&app, "GET", "/api/conversations/g:pins/posts", None, &[]).await;
    assert_eq!(page["pins"], json!([]), "a deleted post stayed pinned: {page}");
}
