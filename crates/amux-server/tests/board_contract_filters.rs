//! `/api/board/contract` must document the list filters that actually exist,
//! and must not document ones that do not (AMUX-2933, reported by ts-gke).
//!
//! The filters worked and were documented NOWHERE — "discoverable only by
//! guessing", in the report's words. The cap was worse than undocumented, it
//! was silent: a lane auditing its own board got the 100 most-recently-updated
//! TERMINAL rows fleet-wide with nothing in the body saying so, which is how
//! `GET /api/board` came to return FEWER of a lane's done cards (4) than
//! `?session=<lane>` did (102). A list that drops rows with no signal reads as
//! data, not as truncation — and ts-gke was one step from reporting "only 4
//! done cards exist".
//!
//! So this test exists to stop the DOC drifting from the CODE, which is the
//! failure that would put the silence back. It compares the documented filter
//! names against `ListParams`' real fields, in BOTH directions.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

async fn contract() -> Value {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("contract-test.db")).unwrap();
    let state = AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
    };
    let app = router(state);
    let res = app
        .oneshot(Request::builder().uri("/api/board/contract").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// The names ListParams actually accepts, read off the struct's source rather
/// than restated here — a hand-copied list is the thing that drifts.
fn list_params_fields() -> Vec<String> {
    let src = include_str!("../src/api/board.rs");
    // Start AFTER the declaration line: `pub struct ListParams {` itself
    // matches the `pub ` prefix below and was scraped as a field named
    // "struct ListParams {" on the first run.
    let decl = "pub struct ListParams {";
    let start = src.find(decl).expect("ListParams exists") + decl.len();
    let end = start + src[start..].find("\n}\n").expect("struct closes");
    src[start..end]
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            t.strip_prefix("pub ")
                .and_then(|r| r.split(':').next())
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
        })
        .collect()
}

#[tokio::test]
async fn the_contract_documents_every_real_list_filter() {
    let c = contract().await;
    let list = c.get("list").expect("contract documents the list endpoint");
    let filters = list["filters"].as_object().expect("filters object");
    let refused = list["not_a_filter"].as_object().expect("refused params named too");

    // Every entry in `filters` must be name -> DESCRIPTION. This is what keeps
    // the two assertions below meaningful: they treat each key as a real query
    // param, so a descriptive key smuggled in here fails them for a reason that
    // reads like a missing field. AF-161's follow-up put `slim_omits` (an array
    // of the fields slim drops) inside `filters` and turned main red with
    // "ListParams has no such field" — which points at adding the field, the one
    // fix that would be wrong. Fail here first, and say where such a key goes.
    for (k, v) in filters.iter() {
        assert!(
            v.is_string(),
            "contract filter `{k}` is not a description string ({v}) — `filters` is \
             strictly query-param -> description. Metadata ABOUT a filter (what it \
             omits, what it defaults to) belongs beside `filters`, not inside it; see \
             `slim_omits`. Leaving it here makes the next assertion demand a \
             `ListParams` field that must not exist."
        );
    }

    let documented: Vec<String> = filters
        .keys()
        .cloned()
        .chain(refused.keys().flat_map(|k| {
            // "q / query / search" documents three names in one key.
            k.split('/').map(|s| s.trim().to_string()).collect::<Vec<_>>()
        }))
        .collect();

    let real = list_params_fields();
    assert!(real.len() >= 7, "the ListParams scraper is broken, found {real:?}");

    for f in &real {
        assert!(
            documented.iter().any(|d| d == f),
            "ListParams accepts `{f}` but /api/board/contract does not document it — \
             that is the AMUX-2933 defect (filters discoverable only by guessing). \
             documented: {documented:?}"
        );
    }
    // The mirror: nothing documented that does not exist, or callers write
    // queries that are silently ignored — which is exactly how `?q=` used to
    // return the whole board.
    for d in &documented {
        assert!(
            real.iter().any(|r| r == d),
            "contract documents `{d}` but ListParams has no such field — a param axum will \
             silently drop. real: {real:?}"
        );
    }
}

/// The cap is legitimate; the SILENCE was the bug. The contract has to state
/// the default, that scoping lifts it, and how to detect a truncated read —
/// otherwise the next lane repeats ts-gke's measurement from scratch.
#[tokio::test]
async fn the_contract_explains_the_terminal_cap_and_how_to_see_it() {
    let c = contract().await;
    let cap = &c["list"]["terminal_cap"];
    assert_eq!(cap["default_unscoped"], 100, "the unscoped default cap must be stated");
    assert_eq!(cap["default_scoped"], 0, "a scoped query is uncapped — that is the fix ts-gke got");

    for key in ["why", "detect_truncation", "to_get_everything", "auditing_your_own_cards"] {
        let v = cap[key].as_str().unwrap_or("");
        assert!(!v.is_empty(), "terminal_cap.{key} must be documented, got {v:?}");
    }
    // The headers named in the contract must be the ones list_board actually
    // sets — naming a header that does not exist is the same class of lie.
    let detect = cap["detect_truncation"].as_str().unwrap();
    let src = include_str!("../src/api/board.rs");
    for h in ["x-amux-truncated", "x-amux-terminal-total", "x-amux-terminal-returned"] {
        assert!(detect.contains(h), "contract must name {h}");
        assert!(src.contains(h), "list_board must actually set {h}");
    }
}

/// AF-112: the contract advertised ONLY the type-default gates (tier 5 of 5)
/// while enforcement resolved card override → worker → group → global →
/// defaults — so a custom gate was advertised nowhere and the 409's own
/// "learn the gate at /contract" pointer named a LOWER bar than the one
/// refusing the caller, drifting in the dangerous direction (an agent reading
/// the contract concludes self-verification is enough). The repair is one
/// resolver, two doors: ?card=<id> serves what effective_gate_configured will
/// actually enforce, and the global custom tier is served whenever it exists.
#[tokio::test]
async fn contract_serves_the_enforced_gate_not_just_type_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("contract-gate-test.db")).unwrap();
    let store = std::sync::Arc::new(store);
    let state = AppState {
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
    };
    let app = router(state);

    // A custom GLOBAL verified gate, exactly the shape the SPA's gate editor
    // writes — stricter than any type default.
    let custom = serde_json::json!([
        "Peer-reviewed by a DIFFERENT worker (name them)",
        "That peer verified it themselves rather than taking the author's word"
    ]);
    let custom_str = custom.to_string();
    store
        .write(move |conn| {
            conn.execute(
                "UPDATE statuses SET gate=?1, gate_custom=1 WHERE id='verified'",
                rusqlite::params![custom_str],
            )?;
            Ok(amux_server::db::WriteOutcome { applied: true, events: vec![] })
        })
        .unwrap();
    // A card to resolve against.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/board")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"title":"gate drift specimen","type":"investigation"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let created: Value = serde_json::from_slice(&bytes).unwrap();
    let id = created["id"].as_str().or_else(|| created["item"]["id"].as_str()).unwrap().to_string();

    let res = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/board/contract?card={id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let c: Value = serde_json::from_slice(&bytes).unwrap();

    // The card-resolved gate must be the ENFORCED one, not the type default.
    assert_eq!(
        c["card_effective_gates"]["gates"]["verified"], custom,
        "?card= must serve the gate enforcement will use: {c}"
    );
    // The global tier is visible card-agnostically too.
    assert_eq!(c["global_custom_gates"]["verified"], custom, "{c}");
    // And the bare table now says what it is, so nobody reads tier 5 as the law.
    assert!(
        c["gates_are"].as_str().unwrap_or("").contains("TYPE DEFAULTS"),
        "the defaults table must name itself as defaults: {c}"
    );
}

/// The shipped `amux` CLI reads this list to print `amux board add --help`.
///
/// It read `["fields"]["valid_types"]` while the server published `types` at top
/// level, so `--help` against a perfectly HEALTHY server printed "server
/// unreachable" and routed the reader at the AMUX-3046 stranded-port hunt
/// instead of at a key-path bug. Nothing caught it for the same reason the cap
/// above went unnoticed: the bash suite's stub served the CLI's wrong shape, so
/// fixture and code agreed with each other and both disagreed with production. A
/// fixture built to match the code under test cannot fail on the defect.
///
/// Asserted in BOTH directions, like the filter test above — the contract
/// publishes a non-empty `types`, and the shipped CLI still reads that key.
/// Either side renaming turns this red rather than silently blanking the help.
#[tokio::test]
async fn the_contract_publishes_the_type_list_the_cli_reads() {
    let c = contract().await;
    let types = c["types"]
        .as_array()
        .unwrap_or_else(|| panic!("contract must publish `types` at top level: {c}"));
    assert!(!types.is_empty(), "contract published an empty type list: {c}");
    for want in ["code", "investigation", "escalation"] {
        assert!(types.iter().any(|t| t == want), "contract types missing {want}: {types:?}");
    }

    // The consumer half: the bash CLI is not compiled, so nothing else can catch
    // a rename on this side.
    let cli = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../amux"),
    )
    .expect("shipped `amux` CLI not found at the repo root");
    assert!(
        cli.contains(r#"d.get("types")"#),
        "the shipped `amux` CLI no longer reads the `types` key this contract publishes — \
         `amux board add --help` will fall back instead of listing the types"
    );
}
