//! The retired Files spellings answer exactly what their canonical twins do
//! (primitive integrity audit 2026-11-26, P1 / step 7).
//!
//! One browse API (`/api/fs`, plus `/api/file/raw` for bytes) and one upload
//! writer (the chunked `/api/upload`'s `publish`). The old spellings stay for
//! outside callers, so this file pins that each one is the canonical handler
//! wearing an old name: same status, same body, through the real router.
//!
//! One test function on purpose: it sets process env (`AMUX_HOME`,
//! `AMUX_FILES_ROOT`) for a hermetic root, and nextest gives it its own
//! process.

use amux_server::api::{router, AppState};
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;

async fn call(app: &axum::Router, method: &str, uri: &str, ctype: Option<&str>, body: Vec<u8>) -> (u16, Vec<u8>, axum::http::HeaderMap) {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(c) = ctype {
        b = b.header("content-type", c);
    }
    let res = app.clone().oneshot(b.body(Body::from(body)).unwrap()).await.unwrap();
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), 64 << 20).await.unwrap().to_vec();
    (status, bytes, headers)
}

fn json(b: &[u8]) -> Value {
    serde_json::from_slice(b).unwrap_or_else(|e| panic!("not json ({e}): {}", String::from_utf8_lossy(b)))
}

fn enc(s: &str) -> String {
    let mut u = reqwest::Url::parse("http://x/").unwrap();
    u.query_pairs_mut().append_pair("p", s);
    u.query().unwrap()[2..].to_string()
}

fn multipart(dir: &str, name: &str, data: &[u8]) -> (String, Vec<u8>) {
    let b = "amuxboundary7";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{b}\r\nContent-Disposition: form-data; name=\"dir\"\r\n\r\n{dir}\r\n").as_bytes());
    body.extend_from_slice(
        format!("--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={b}"), body)
}

#[tokio::test]
async fn legacy_files_spellings_answer_what_the_canonical_routes_answer() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("sessions")).unwrap();
    std::env::set_var("AMUX_HOME", home.path());
    std::env::remove_var("TMUX");
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    std::env::set_var("AMUX_FILES_ROOT", &root_path);
    std::fs::create_dir_all(root_path.join("docs/sub")).unwrap();
    std::fs::write(root_path.join("docs/a.txt"), b"alpha").unwrap();
    std::fs::write(root_path.join("docs/b.bin"), [0u8, 159, 146, 150]).unwrap();
    std::fs::write(root_path.join("docs/Node.mdai"), "---\nmodel: none\n---\nhello\n").unwrap();

    let db = tempfile::tempdir().unwrap();
    let store = amux_server::db::Store::open(&db.path().join("t.db")).unwrap();
    let app = router(AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    });
    let docs_abs = root_path.join("docs").to_string_lossy().into_owned();

    // 1. Listing: /api/files?path=<rel> == /api/fs/list?path=<abs>.
    let (s_old, b_old, _) = call(&app, "GET", "/api/files?path=docs", None, vec![]).await;
    let (s_new, b_new, _) = call(&app, "GET", &format!("/api/fs/list?path={}", enc(&docs_abs)), None, vec![]).await;
    assert_eq!((s_old, s_new), (200, 200), "{}", String::from_utf8_lossy(&b_old));
    assert_eq!(json(&b_old), json(&b_new), "the legacy listing must be the fs listing");
    let names: Vec<String> = json(&b_old)["entries"].as_array().unwrap().iter()
        .map(|e| e["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, ["sub", "a.txt", "b.bin", "Node.mdai"], "dirs first, then case-folded names");
    // Containment still holds on the legacy spelling.
    let (s_esc, _, _) = call(&app, "GET", "/api/files?path=../../../../etc", None, vec![]).await;
    assert_eq!(s_esc, 400, "a ../ escape of the files root is refused");

    // 2. Download: /api/files/download?path=<rel> == /api/file/raw?path=<abs>&download=1.
    let bin_abs = root_path.join("docs/b.bin").to_string_lossy().into_owned();
    let (s_old, b_old, h_old) = call(&app, "GET", "/api/files/download?path=docs/b.bin", None, vec![]).await;
    let (s_new, b_new, h_new) = call(&app, "GET", &format!("/api/file/raw?path={}&download=1", enc(&bin_abs)), None, vec![]).await;
    assert_eq!((s_old, s_new), (200, 200));
    assert_eq!(b_old, b_new);
    assert_eq!(b_old, [0u8, 159, 146, 150]);
    for h in ["content-type", "content-disposition", "content-length", "etag"] {
        assert_eq!(h_old.get(h), h_new.get(h), "header {h} differs between the spellings");
    }
    assert!(h_old.get("content-disposition").unwrap().to_str().unwrap().starts_with("attachment"));

    // 3. mdai: /api/files/mdai/* rewrites to /api/mdai/* (aliases.rs).
    let (s_old, b_old, h_old) = call(&app, "GET", "/api/files/mdai", None, vec![]).await;
    let (s_new, b_new, h_new) = call(&app, "GET", "/api/mdai", None, vec![]).await;
    assert_eq!((s_old, s_new), (200, 200), "{}", String::from_utf8_lossy(&b_old));
    assert_eq!(json(&b_old), json(&b_new), "the legacy mdai list must be the /api/mdai list");
    assert!(String::from_utf8_lossy(&b_new).contains("Node.mdai"), "the probe must see the seeded node: {}", String::from_utf8_lossy(&b_new));
    assert_eq!(h_old.get("deprecated").map(|v| v.to_str().unwrap()), Some("true"));
    assert!(h_new.get("deprecated").is_none(), "the canonical spelling is not deprecated");
    let doc_q = format!("path={}", enc(&root_path.join("docs/Node.mdai").to_string_lossy()));
    let (s_old, b_old, _) = call(&app, "GET", &format!("/api/files/mdai/doc?{doc_q}"), None, vec![]).await;
    let (s_new, b_new, _) = call(&app, "GET", &format!("/api/mdai/doc?{doc_q}"), None, vec![]).await;
    assert_eq!((s_old, s_new), (200, 200), "{}", String::from_utf8_lossy(&b_old));
    assert_eq!(json(&b_old), json(&b_new));

    // 4. Raw-body upload writes through the chunked path's writer and keeps
    //    its contract: replace in place, answer {ok, path, size}.
    let (s, b, _) = call(&app, "POST", "/api/files/upload?path=docs/up.txt", None, b"one".to_vec()).await;
    assert_eq!(s, 200, "{}", String::from_utf8_lossy(&b));
    assert_eq!(json(&b), serde_json::json!({"ok": true, "path": "docs/up.txt", "size": 3}));
    let (s, _, _) = call(&app, "POST", "/api/files/upload?path=docs/up.txt", None, b"second".to_vec()).await;
    assert_eq!(s, 200);
    assert_eq!(std::fs::read(root_path.join("docs/up.txt")).unwrap(), b"second", "the legacy raw upload replaces");
    let (s, _, _) = call(&app, "POST", "/api/files/upload?path=../x.txt", None, b"x".to_vec()).await;
    assert_eq!(s, 400, "a ../ upload target is refused");

    // 5. Multipart /api/fs/upload: the same writer, never clobbering by default,
    //    and the same collision name the chunked finish picks.
    let (ct, body) = multipart(&docs_abs, "a.txt", b"beta");
    let (s, b, _) = call(&app, "POST", "/api/fs/upload", Some(&ct), body).await;
    assert_eq!(s, 200, "{}", String::from_utf8_lossy(&b));
    assert_eq!(json(&b)["saved"][0]["name"], "a_1.txt");
    assert_eq!(std::fs::read(root_path.join("docs/a.txt")).unwrap(), b"alpha", "the original survives");
    assert_eq!(std::fs::read(root_path.join("docs/a_1.txt")).unwrap(), b"beta");
    let (ct, body) = multipart(&docs_abs, "evil.plist", b"x");
    let (_, b, _) = call(&app, "POST", "/api/fs/upload", Some(&ct), body).await;
    assert_eq!(json(&b)["saved"][0]["error"], "refused: could execute code");

    // 6. The chunked protocol into the same directory picks the next free name,
    //    so the two paths agree about collisions.
    let (s, b, _) = call(&app, "POST", "/api/upload/start", Some("application/json"), br#"{"name":"a.txt","size":5,"chunks":1}"#.to_vec()).await;
    assert_eq!(s, 200);
    let id = json(&b)["id"].as_str().unwrap().to_string();
    let (s, _, _) = call(&app, "PUT", &format!("/api/upload/{id}/chunk/0"), None, b"gamma".to_vec()).await;
    assert_eq!(s, 200);
    let (s, b, _) = call(&app, "POST", &format!("/api/upload/{id}/finish?dir={}&name=a.txt", enc(&docs_abs)), None, vec![]).await;
    assert_eq!(s, 200, "{}", String::from_utf8_lossy(&b));
    assert!(json(&b)["path"].as_str().unwrap().ends_with("/docs/a_2.txt"), "{}", String::from_utf8_lossy(&b));

    // 7. The instrument: every legacy spelling is listed with a verdict and the
    //    population it was drawn from. (The request log is written by the
    //    outer layer in production; this rig counts zero, so it pins shape.)
    let (s, b, _) = call(&app, "GET", "/api/debug/legacy-routes?since_h=1", None, vec![]).await;
    assert_eq!(s, 200);
    let v = json(&b);
    assert_eq!(v["measured"], true, "{v}");
    let legacy: Vec<&str> = v["routes"].as_array().unwrap().iter().map(|r| r["legacy"].as_str().unwrap()).collect();
    for p in ["/api/files", "/api/files/download", "/api/files/upload", "/api/fs/upload", "/api/files/mdai"] {
        assert!(legacy.contains(&p), "{p} is not counted: {legacy:?}");
    }
    assert!(v["routes"].as_array().unwrap().iter().all(|r| r["verdict"].is_string()));
}
