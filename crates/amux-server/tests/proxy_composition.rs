//! FULL-composition routing tests for the python-proxy boundary
//! (AMUX-2594 swallow + AMUX-2597 registry).
//!
//! Why these exist at the integration level and not only as unit tests: the
//! nested routers' unit tests build `Router::new().nest(...)` WITHOUT the
//! static SPA catch-all (`/{*path}`), and in the full `api::router`
//! composition that catch-all out-competes a nested `.fallback()` — so a
//! fallback-based proxy passed its unit test while the live server answered
//! index.html (200 text/html) for /api/groups, /api/browser/state and every
//! other unrouted API path. That 200-HTML swallow is what broke the SPA's
//! group picker ("adding a group didn't work") and what earlier misled an
//! auth probe into reporting an unauthenticated 200 on /api/fs.
//!
//! Post-AMUX-2597 this file also pins the BOUNDARY itself:
//! - py_proxy::PROXIED_FAMILIES is EMPTY and must STAY empty (AMUX-2608:
//!   /api/scope was the last row) — a reappearing row fails here, which is
//!   what makes the empty table the cutover's standing proof rather than a
//!   coincidence of the moment;
//! - the families that went NATIVE (/api/fs, /api/groups, /api/tags,
//!   /api/ls, /api/scope, …) answer WITHOUT the `x-amux-answered-by:
//!   python-proxy` stamp, so a regression back to proxying (or forward to
//!   the SPA shell) fails loudly;
//! - the registry cannot drift from mod.rs: every /api route literal mod.rs
//!   mounts must be claimed by NATIVE_FAMILIES or PROXIED_FAMILIES (a view
//!   must share the predicate of the mechanism it describes — ethos rule 1).

use amux_server::api::{router, py_proxy, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

fn app() -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("amux-test.db")).unwrap();
    let state = AppState {
        secrets: std::sync::Arc::new(amux_server::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
    reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    (router(state), dir)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, String, String, bool) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let ct = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let proxied = res.headers().get("x-amux-answered-by").is_some();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, ct, String::from_utf8_lossy(&bytes).into_owned(), proxied)
}

/// One test fn on purpose: it mutates process env (AMUX_PY_URL, AMUX_HOME),
/// and tests within a binary share the process.
#[tokio::test]
async fn boundary_routes_proxied_to_python_native_stays_native() {
    // Dead port: bind-then-drop reserves an address nothing serves.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    std::env::set_var("AMUX_PY_URL", &dead);
    // Hermetic fleet home: /api/groups must not read the developer's real
    // ~/.amux in a test.
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("sessions")).unwrap();
    std::fs::write(
        home.path().join("sessions/w1.env"),
        "CC_TAGS=\"alpha, beta\"\n",
    )
    .unwrap();
    std::env::set_var("AMUX_HOME", home.path());

    let (app, _dir) = app();

    // -- The registry is EMPTY (AMUX-2608: /api/scope was the last row) and
    //    the assertion is what keeps it empty — a new proxy row must delete
    //    this check to land, which is the loud conversation it deserves.
    assert!(
        py_proxy::PROXIED_FAMILIES.is_empty(),
        "PROXIED_FAMILIES must stay empty post-cutover; a new row reintroduces \
         the python proxy: {:?}",
        py_proxy::PROXIED_FAMILIES.iter().map(|f| f.family).collect::<Vec<_>>()
    );

    // -- /api/scope answers NATIVELY (was the last proxied family): with
    //    AMUX_PY_URL pointing at a dead port, a proxy regression would be a
    //    502 here; the SPA-shell swallow would be text/html.
    let (status, ct, body, proxied) = get(&app, "/api/scope").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(ct.starts_with("application/json"), "{ct}");
    assert!(!proxied, "/api/scope must be NATIVE");
    let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
    assert_eq!(v["level"], "global");
    // ASSERT THE KEYS, NOT THE COUNT. This was `Some(5)` with the message "all
    // five capabilities reported", and adding the `skin` descriptor turned it
    // red with `left: Some(6) right: Some(5)` — a diff that says a number
    // changed and nothing about WHICH capability appeared or vanished. A count
    // also cannot distinguish "skin was added" from "gates was dropped and
    // something else added", which is the failure worth catching: `/api/scope`
    // is the uniform per-scope contract, so a capability silently disappearing
    // from it is a feature going missing fleet-wide.
    let keys: Vec<&str> = v["capabilities"]
        .as_array()
        .expect("capabilities must be an array")
        .iter()
        .map(|c| c["key"].as_str().unwrap_or("<missing key>"))
        .collect();
    assert_eq!(
        keys,
        // `connectors` is the 7th, added intentionally (df798ca): a connector is
        // a scopable capability, not a new subsystem (docs/design/connectors.md).
        // Publication order is SCOPE_CAPS order, so it follows status_mode.
        vec!["memory", "rules", "env", "gates", "skin", "status_mode", "connectors"],
        "the scope contract's capabilities, in publication order: {body}"
    );
    // The hermetic fleet (w1: alpha, beta) shows through the global read.
    assert_eq!(v["groups"], serde_json::json!(["alpha", "beta"]));
    let (status, _, body, proxied) = get(&app, "/api/scope?level=worker&name=w1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!proxied);
    let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
    assert_eq!(v["groups"], serde_json::json!(["alpha", "beta"]));
    // Sub-paths: python's generic JSON 404, never the shell.
    let (status, ct, body, proxied) = get(&app, "/api/scope/definitely-not").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(ct.starts_with("application/json"), "{ct}");
    assert!(!proxied);
    let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
    assert_eq!(v["error"], "not found", "{body}");

    // -- NATIVE families answer themselves: no proxy stamp, no 502, no
    //    static shell. /api/fs, /api/groups, /api/tags, /api/ls were
    //    proxied before AMUX-2597; a reappearing stamp means the boundary
    //    regressed.
    let (status, _, body, proxied) = get(&app, "/api/groups").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!proxied, "/api/groups must be NATIVE");
    let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
    assert_eq!(v["groups"], serde_json::json!([
        {"name": "alpha", "workers": 1}, {"name": "beta", "workers": 1}
    ]));

    let (status, _, body, proxied) = get(&app, "/api/tags").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!proxied, "/api/tags must be NATIVE");

    // -- Session verbs are NATIVE (AMUX-2598): the per-name family answers
    //    from the fleet substrate (env files in the hermetic AMUX_HOME), no
    //    proxy stamp, and a missing session is Python's exact 404 shape —
    //    never a 502 at a dead python and never the static shell.
    let (status, _, body, proxied) = get(&app, "/api/sessions/w1/meta").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!proxied, "/api/sessions/{{name}}/meta must be NATIVE");
    let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
    assert_eq!(v["name"], serde_json::json!("w1"));
    assert_eq!(v["tags"], serde_json::json!(["alpha", "beta"]));
    let (status, ct, body, proxied) = get(&app, "/api/sessions/definitely-not/peek").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(ct.starts_with("application/json"), "{ct}");
    assert!(!proxied, "missing-session 404 must be NATIVE, not a python 502");
    let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
    assert_eq!(v["error"], serde_json::json!("session 'definitely-not' not found"));
    let (status, _, body, proxied) = get(&app, "/api/sessions/w1/instructions").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!proxied);
    let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
    assert_eq!(v["instructions"], serde_json::json!(""));

    let (status, _, body, proxied) = get(&app, "/api/groups/alpha/config").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!proxied);
    let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"name": "alpha", "department": "", "goal": "", "kpis": [], "human_cost": 0})
    );

    let dirq = home.path().to_str().unwrap().replace('/', "%2F");
    for path in [
        format!("/api/fs/list?path={dirq}"),
        format!("/api/ls?path={dirq}"),
        format!("/api/fs/search?path={dirq}&q=zzz-not-there"),
    ] {
        let (status, _, body, proxied) = get(&app, &path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert!(!proxied, "{path} must be NATIVE");
    }

    // -- File viewer family + /api/library: NATIVE post-AMUX-2598 (was the
    //    registry's last Namespace row). A reappearing proxy stamp — or the
    //    502 these paths answered pre-cutover — means the boundary regressed.
    std::fs::write(home.path().join("viewer.txt"), "hello viewer\n").unwrap();
    let filep = format!("{dirq}%2Fviewer.txt");
    let (status, _, body, proxied) = get(&app, &format!("/api/file?path={filep}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!proxied, "/api/file must be NATIVE");
    let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
    assert_eq!(v["content"], "hello viewer\n", "{body}");

    // Raw streaming honors Range natively (the semantics browsers scrub with).
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/file/raw?path={filep}"))
                .header("range", "bytes=0-4")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert!(res.headers().get("x-amux-answered-by").is_none(), "/api/file/raw must be NATIVE");
    assert_eq!(res.headers()["content-range"], "bytes 0-4/13");
    let raw_body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&raw_body[..], b"hello");

    let (status, _, body, proxied) = get(&app, &format!("/api/library?path={dirq}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!proxied, "/api/library must be NATIVE");
    let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
    assert_eq!(v["is_library"], false, "hermetic home holds no ebooks: {body}");

    // Unknown /api/file subpaths: the module's python-shape 404, no proxy.
    let (status, ct, body, proxied) = get(&app, "/api/file/definitely-not").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(ct.starts_with("application/json"), "{ct}");
    assert!(!proxied);

    // Unknown paths in native namespaces: python's generic JSON 404 shape.
    for path in ["/api/fs", "/api/fs/definitely-not", "/api/tags/mytag", "/api/groups/x/y"] {
        let (status, ct, body, proxied) = get(&app, path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
        assert!(ct.starts_with("application/json"), "{path}: {ct}");
        assert!(!proxied, "{path}");
        let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
        assert_eq!(v["error"], "not found", "{path}: {body}");
    }

    // -- /api/identity: native config-introspection (the SPA's boot call
    //    404'd on this origin before AMUX-2597). AMUX_HOME is the hermetic
    //    tempdir (no server.env), so no key is visible; oauth may be true
    //    on a dev box with a real ~/.claude.json — assert shape + the
    //    hermetic facts only.
    let (status, _, body, proxied) = get(&app, "/api/identity").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!proxied, "/api/identity must be NATIVE");
    let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
    assert_eq!(v["email"], "");
    assert_eq!(v["is_cloud"], false);
    assert_eq!(v["managed_upstream"], false, "no server.env in the hermetic home");
    assert_eq!(v["key_valid"], Value::Null);
    for k in ["has_api_key", "has_oauth", "key_error"] {
        assert!(v.get(k).is_some(), "identity payload missing {k}: {body}");
    }

    // -- Browser driver verbs — NATIVE post-AMUX-2598 (were the Module-mount
    //    proxy row inside api::browser). With no amux-launched Chrome in this
    //    test process they answer an honest 409 POINTING AT /start — never
    //    the python proxy's 502, never the SPA shell. The old world answered
    //    502 here (see the removed loop entries in this file's history), so
    //    a 502 reappearing IS the regression signal.
    for path in ["/api/browser/state?session=x", "/api/browser/screenshot"] {
        let (status, ct, body, proxied) = get(&app, path).await;
        assert_eq!(status, StatusCode::CONFLICT, "{path}: {body}");
        assert!(ct.starts_with("application/json"), "{path}: {ct}");
        assert!(!proxied, "{path} must be NATIVE");
        let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
        assert!(
            v["error"].as_str().unwrap().contains("/api/browser/start"),
            "{path}: the 409 must name the fix: {body}"
        );
    }
    let (status, _, body, proxied) = get(&app, "/api/browser/pw-profiles").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!proxied, "/api/browser/pw-profiles must be NATIVE");
    let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
    assert!(v["profiles"].is_array(), "{body}");

    // Unknown browser routes answer the ported route catalog natively (the
    // "guessed /status for /state" incident), not the shell, not a proxy.
    let (status, ct, body, proxied) = get(&app, "/api/browser/definitely-not").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(ct.starts_with("application/json"), "{ct}");
    assert!(!proxied);
    let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
    assert!(
        v["error"].as_str().unwrap().starts_with("browser route not found"),
        "{body}"
    );
    assert!(v["routes"].is_array() && v["actions"].is_array(), "{body}");

    // -- The registry endpoint itself.
    let (status, _, body, proxied) = get(&app, "/api/debug/boundary").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!proxied);
    let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
    // The registry reached empty (AMUX-2608) and the LIVE VIEW must say so —
    // an empty `proxied` array is the runtime half of the cutover proof.
    assert_eq!(v["proxied"], serde_json::json!([]), "{body}");
    assert!(v["native"].as_array().unwrap().len() > 20, "{body}");
    assert_eq!(v["doc"], "docs/rust-migration/server-boundary.md");

    // Dictation engine config — NATIVE (AMUX-2598; was the last dictation
    // proxy row). Values are environment-dependent (whisper weights / key
    // presence on the host), so pin the Python field SHAPE, not the values.
    let (status, ct, body, proxied) = get(&app, "/api/dictation/config").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(ct.starts_with("application/json"), "{ct}");
    assert!(!proxied, "/api/dictation/config must be NATIVE");
    let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
    for k in ["configured", "source", "model", "local", "local_model", "engine"] {
        assert!(v.get(k).is_some(), "config payload missing {k}: {body}");
    }

    // Unrouted dictation paths answer the module's NATIVE Python-shape 404
    // (never the static shell's generic one, never a proxy attempt).
    let (status, ct, body, _) = get(&app, "/api/dictation/bogus").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(ct.starts_with("application/json"), "{ct}");
    let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
    assert_eq!(v["error"], "dictation route not found", "{body}");

    // Unknown API paths outside every nest: static's JSON 404 (Python's
    // generic shape), NOT the SPA shell.
    let (status, ct, body, _) = get(&app, "/api/definitely-not-a-route").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(ct.starts_with("application/json"), "{ct}");
    let v: Value = serde_json::from_slice(body.as_bytes()).unwrap();
    assert_eq!(v["error"], "not found", "{body}");

    // Non-API unknown paths still serve the SPA shell (client routing).
    let (status, ct, _body, _) = get(&app, "/some/client/route").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.starts_with("text/html"), "{ct}");

    std::env::remove_var("AMUX_PY_URL");
    std::env::remove_var("AMUX_HOME");
}

/// The registry must share the predicate of the mechanism it describes:
/// every /api route literal mounted in mod.rs maps to a family claimed by
/// NATIVE_FAMILIES or PROXIED_FAMILIES. Add a mount without a registry row
/// and this fails; retire a family without pruning the registry and the
/// stale row is at least visible in the diff of this list.
#[test]
fn every_mounted_api_family_is_claimed_by_the_registry() {
    let src = include_str!("../src/api/mod.rs");
    let mut mounted: std::collections::BTreeSet<String> = Default::default();
    for line in src.lines() {
        let t = line.trim();
        if !(t.starts_with(".nest(") || t.starts_with(".route(") || t.starts_with(".merge(")) {
            continue;
        }
        if let Some(start) = t.find("\"/api/") {
            let rest = &t[start + 1..];
            let path = &rest[..rest.find('"').unwrap_or(rest.len())];
            // Family root: first two segments ("/api/xxx").
            let family: String =
                path.split('/').take(3).collect::<Vec<_>>().join("/");
            mounted.insert(family);
        }
    }
    assert!(
        mounted.len() > 20,
        "source parse broke — found only {mounted:?}"
    );
    let native: std::collections::BTreeSet<&str> =
        py_proxy::NATIVE_FAMILIES.iter().map(|(f, _)| *f).collect();
    // Proxied families that appear as literals in mod.rs do so only through
    // family_routes(); mod.rs itself should carry no proxy path literals.
    for fam in &mounted {
        let claimed = native.contains(fam.as_str())
            || native.iter().any(|n| fam.starts_with(*n) && fam[n.len()..].starts_with('.'))
            || py_proxy::PROXIED_FAMILIES.iter().any(|p| p.family.contains(fam.as_str()));
        assert!(
            claimed,
            "mod.rs mounts {fam} but the boundary registry (py_proxy.rs \
             NATIVE_FAMILIES / PROXIED_FAMILIES) does not claim it — add the row"
        );
    }
    // And the inverse spot check: no native family claims a proxied prefix.
    for p in py_proxy::PROXIED_FAMILIES {
        for (n, _) in py_proxy::NATIVE_FAMILIES {
            assert!(
                !p.family.starts_with(*n) || p.family.contains("sessions") || p.family.contains("browser"),
                "family {n} is claimed native AND proxied ({})",
                p.family
            );
        }
    }
}
