//! Route + field aliasing infrastructure (RR-0018a; Invariants 13, 53).
//!
//! The vernacular rename (session -> worker, issue -> task) must not break a
//! single existing consumer: scripts, hooks, CLI muscle memory, CLAUDE.md
//! instructions that say `/api/sessions` — all keep working, forever (the
//! plan sets no removal timeline). Two mechanisms:
//!
//! - **Route aliases**: legacy paths (`/api/sessions/*`, `/api/issues/*`)
//!   rewrite to their canonical twins (`/api/workers/*`, `/api/tasks/*`)
//!   BEFORE routing, and the response gains a `Deprecated: true` header so
//!   the rename is signalled without breaking anyone.
//! - **Field aliases**: [`alias_fields`] maps `worker<->session` /
//!   `task<->issue` key names in response bodies, per [`FieldStyle`]
//!   (default `Both`: both names appear, carrying the same value).
//!   [`canonicalize_request_fields`] is the request-side half: either name
//!   is accepted; both-present-and-different is a conflict the caller must
//!   surface as a 400 — never a silently picked winner (Invariant 37).

use axum::extract::Request;
use axum::http::{HeaderValue, Uri};
use axum::middleware::Next;
use axum::response::Response;
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Route alias registry: (legacy prefix, canonical prefix). Table-driven so
/// the Phase-2 board rename (`/api/issues` -> `/api/tasks`) is one row, not
/// a second middleware.
const ROUTE_ALIASES: &[(&str, &str)] = &[
    ("/api/sessions", "/api/workers"),
    ("/api/issues", "/api/tasks"),
];

/// Field alias registry: (modern, legacy). Exact key names only — the plan's
/// contract is about these specific fields (board cards carry `session`,
/// modern payloads carry `worker`), not about fuzzy substring renames.
pub const FIELD_ALIASES: &[(&str, &str)] = &[
    ("worker", "session"),
    ("worker_id", "session_id"),
    ("task", "issue"),
    ("task_id", "issue_id"),
];

/// Which field names response bodies carry (pref `api_field_style`).
/// `Both` is the default: new consumers see the modern names, old consumers
/// keep finding the legacy ones, and nobody breaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldStyle {
    #[default]
    Both,
    Modern,
    Legacy,
}

/// Wrap the FINISHED app router with legacy-route aliasing.
///
/// Takes/returns `Router` (i.e. `Router<()>`, after `.with_state`), not
/// `Router<AppState>`: a URI rewrite must run BEFORE routing, which in axum
/// means wrapping the whole router as a *service* — and a router still
/// missing its state is not a service yet. The wrapping shape (outer router
/// whose fallback is the real app, with the rewrite layered on top) is the
/// documented axum approach to pre-routing rewrites; auth is unaffected
/// because it lives inside the wrapped router and runs after the rewrite,
/// so legacy paths are exactly as protected as canonical ones.
pub fn alias_layer(router: Router) -> Router {
    Router::new()
        .fallback_service(router)
        .layer(axum::middleware::from_fn(alias_middleware))
}

/// Rewrite a legacy URI to its canonical twin, preserving any sub-path and
/// query. `None` = not a legacy path (also for a prefix-shaped near-miss
/// like `/api/sessionsfoo`, which must NOT rewrite).
fn rewrite_legacy_uri(uri: &Uri) -> Option<Uri> {
    let path = uri.path();
    // /api/sessions/* never rewrites: the bare list has a dedicated
    // Python-SHAPE handler, and per-session VERBS proxy to the Python
    // server that owns the fleet (api::py_proxy) — rewriting them to
    // /api/workers served the wrong contract for both.
    if path == "/api/sessions" || path.starts_with("/api/sessions/") {
        return None;
    }
    for (legacy, canonical) in ROUTE_ALIASES {
        let rest = if path == *legacy {
            Some("")
        } else {
            // Only a real sub-path counts: the char after the prefix must be
            // '/', otherwise "/api/sessionsfoo" would silently alias.
            path.strip_prefix(legacy).filter(|r| r.starts_with('/'))
        };
        let Some(rest) = rest else { continue };
        let new_pq = match uri.query() {
            Some(q) => format!("{canonical}{rest}?{q}"),
            None => format!("{canonical}{rest}"),
        };
        let mut parts = uri.clone().into_parts();
        parts.path_and_query = Some(new_pq.parse().ok()?);
        return Uri::from_parts(parts).ok();
    }
    None
}

/// Drop a trailing slash when, and only when, the bare path IS a declared route
/// (AMUX-4753).
///
/// # The fault
///
/// Every nested API root answers 200 bare and 404 on the slash, because they are
/// mounted as `.nest("/api/x", Router::new().route("/", ..))` and axum 0.6
/// dropped automatic trailing-slash redirects. Measured on build b971a3e9, 11 of
/// 12 nested roots: `/api/board`, `/api/workers`, `/api/schedules`,
/// `/api/memories`, `/api/groups`, `/api/board-lifecycle`, `/api/messages`,
/// `/api/search`, `/api/why`, `/api/policy`, `/api/prefs`.
///
/// It is not cosmetic. Real clients send the slash form (12 GETs to
/// `/api/board-lifecycle/` from Python-urllib/3.11 on 2026-09-15, every one a
/// 404), and amux DISAGREED WITH ITSELF about it: the invariant matcher
/// `match_route_full` trims slashes, so `route.mounted_routes_answer` judged
/// `/api/board-lifecycle/` as a mounted route and failed it, while axum refused
/// to serve it. Two components, one fact, opposite answers.
///
/// # Why the predicate is "is the bare path routed" rather than "ends in /"
///
/// A blanket strip is what `NormalizePathLayer` does, and it would reach
/// surfaces this card is not about: the SPA shell fallback, the `{*path}` file
/// servers, `/proxy/`. Asking [`super::request_log::routed_methods_at`] instead
/// confines the rewrite to requests that would have matched a DECLARED route but
/// for the slash, which is exactly the population the invariant matcher already
/// counts as mounted. One fact, one definition, and the two consumers can no
/// longer drift — the alternative fix (teach the matcher that a slash is not a
/// mounted path) would have made them agree by leaving the real clients broken.
///
/// # Why a rewrite and not a 308
///
/// A redirect only helps a client that follows redirects, and Python's urllib
/// only learned 308 in 3.11. A rewrite serves the request. The response carries
/// `x-amux-canonical-path` so a client can still discover the canonical spelling
/// without having to follow anything; that value is the resolved path, never the
/// raw one the client sent.
///
/// Returns the rewritten URI, or `None` when nothing should change. `/` itself
/// is never rewritten (it would become the empty path).
fn trim_trailing_slash(uri: &Uri) -> Option<Uri> {
    let path = uri.path();
    let bare = path.strip_suffix('/').filter(|b| !b.is_empty())?;
    // Only a path a declared route claims. An unrouted `/api/nope/` keeps its
    // 404, with the catalog and nearest_routes it already gets.
    if super::request_log::routed_methods_at(bare).is_empty() {
        return None;
    }
    let new_pq = match uri.query() {
        Some(q) => format!("{bare}?{q}"),
        None => bare.to_string(),
    };
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(new_pq.parse().ok()?);
    Uri::from_parts(parts).ok()
}

async fn alias_middleware(mut req: Request, next: Next) -> Response {
    let rewritten = rewrite_legacy_uri(req.uri());
    let was_legacy = rewritten.is_some();
    if let Some(uri) = rewritten {
        *req.uri_mut() = uri;
    }
    // AFTER the legacy rewrite, not before: `/api/issues/5/` has to become
    // `/api/tasks/5` and only the canonical spelling is in ROUTE_TABLE, so a
    // strip that ran first would find nothing routed and leave the 404 in place.
    let canonical = trim_trailing_slash(req.uri());
    if let Some(uri) = canonical.clone() {
        let served = uri.path().to_string();
        // THE TWO-FIX SIGNAL. The population is small by construction (a client
        // has to send the slash form at all), and until now the only trace was a
        // 404 in the request log that looked like a client guessing a URL. A
        // sweep can now count who is on the slash form and which roots they want.
        tracing::info!(
            target: "amux::routing",
            verdict = "trailing_slash_normalized",
            sent = %req.uri().path(),
            served = %served,
            "a declared route was addressed with a trailing slash and was served rather than 404'd"
        );
        *req.uri_mut() = uri;
    }
    let mut res = next.run(req).await;
    if was_legacy {
        // The rename announces itself instead of breaking callers.
        res.headers_mut()
            .insert("deprecated", HeaderValue::from_static("true"));
    }
    if let Some(uri) = canonical {
        // Server-resolved, never the raw path the client sent.
        if let Ok(v) = HeaderValue::from_str(uri.path()) {
            res.headers_mut().insert("x-amux-canonical-path", v);
        }
    }
    res
}

/// Recursively apply [`FieldStyle`] to a response body.
///
/// - `Both`: wherever exactly one of an alias pair exists, the other is
///   added with the same value. If BOTH already exist the object is left
///   untouched — aliasing must never clobber data, even inconsistent data
///   (an inconsistency should be visible, not papered over).
/// - `Modern`/`Legacy`: the other spelling is removed; if only the removed
///   spelling existed, it is renamed rather than dropped, so no value is
///   ever lost to a style preference.
pub fn alias_fields(v: Value, style: FieldStyle) -> Value {
    match v {
        Value::Object(map) => {
            let mut out: Map<String, Value> = map
                .into_iter()
                .map(|(k, val)| (k, alias_fields(val, style)))
                .collect();
            for (modern, legacy) in FIELD_ALIASES {
                apply_style(&mut out, modern, legacy, style);
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|x| alias_fields(x, style)).collect())
        }
        scalar => scalar,
    }
}

fn apply_style(obj: &mut Map<String, Value>, modern: &str, legacy: &str, style: FieldStyle) {
    match style {
        FieldStyle::Both => match (obj.contains_key(modern), obj.contains_key(legacy)) {
            (true, false) => {
                let v = obj[modern].clone();
                obj.insert(legacy.to_string(), v);
            }
            (false, true) => {
                let v = obj[legacy].clone();
                obj.insert(modern.to_string(), v);
            }
            _ => {}
        },
        FieldStyle::Modern => {
            if let Some(v) = obj.remove(legacy) {
                obj.entry(modern.to_string()).or_insert(v);
            }
        }
        FieldStyle::Legacy => {
            if let Some(v) = obj.remove(modern) {
                obj.entry(legacy.to_string()).or_insert(v);
            }
        }
    }
}

/// A request body carried BOTH spellings of an aliased field with different
/// values. Handlers must answer 400 with both values in the error body —
/// silently picking a winner is the dropped-field behavior Invariant 37
/// forbids.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldConflict {
    pub modern_field: String,
    pub legacy_field: String,
    pub modern_value: Value,
    pub legacy_value: Value,
}

/// Request-side aliasing: rewrite legacy field names to modern ones so
/// handler structs only ever declare the modern spelling.
///
/// - only legacy present: renamed to modern
/// - both present, equal: legacy dropped (either spelling satisfied it)
/// - both present, different: [`FieldConflict`] (caller returns 400)
pub fn canonicalize_request_fields(v: Value) -> Result<Value, FieldConflict> {
    match v {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, val) in map {
                out.insert(k, canonicalize_request_fields(val)?);
            }
            for (modern, legacy) in FIELD_ALIASES {
                if let Some(lv) = out.get(*legacy).cloned() {
                    match out.get(*modern) {
                        None => {
                            out.remove(*legacy);
                            out.insert(modern.to_string(), lv);
                        }
                        Some(mv) if *mv == lv => {
                            out.remove(*legacy);
                        }
                        Some(mv) => {
                            return Err(FieldConflict {
                                modern_field: modern.to_string(),
                                legacy_field: legacy.to_string(),
                                modern_value: mv.clone(),
                                legacy_value: lv,
                            })
                        }
                    }
                }
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => Ok(Value::Array(
            items
                .into_iter()
                .map(canonicalize_request_fields)
                .collect::<Result<_, _>>()?,
        )),
        scalar => Ok(scalar),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::{Path, RawQuery};
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::routing::get;
    use serde_json::json;
    use tower::ServiceExt;

    // ---- field aliasing -------------------------------------------------

    #[test]
    fn both_adds_counterpart_recursively() {
        let v = json!({
            "items": [
                {"worker": "w1", "title": "a"},
                {"session": "s1", "task": "t1"}
            ],
            "nested": {"task_id": "tsk_x"}
        });
        let out = alias_fields(v, FieldStyle::Both);
        assert_eq!(out["items"][0]["worker"], json!("w1"));
        assert_eq!(out["items"][0]["session"], json!("w1"));
        assert_eq!(out["items"][0]["title"], json!("a")); // unrelated keys untouched
        assert_eq!(out["items"][1]["worker"], json!("s1"));
        assert_eq!(out["items"][1]["session"], json!("s1"));
        assert_eq!(out["items"][1]["task"], json!("t1"));
        assert_eq!(out["items"][1]["issue"], json!("t1"));
        assert_eq!(out["nested"]["task_id"], json!("tsk_x"));
        assert_eq!(out["nested"]["issue_id"], json!("tsk_x"));
    }

    #[test]
    fn modern_renames_or_drops_legacy() {
        // Only legacy -> renamed, value preserved.
        let out = alias_fields(json!({"session": "x"}), FieldStyle::Modern);
        assert_eq!(out, json!({"worker": "x"}));
        // Both -> legacy dropped, modern kept.
        let out = alias_fields(json!({"worker": "a", "session": "a"}), FieldStyle::Modern);
        assert_eq!(out, json!({"worker": "a"}));
    }

    #[test]
    fn legacy_renames_or_drops_modern() {
        let out = alias_fields(json!({"worker": "x"}), FieldStyle::Legacy);
        assert_eq!(out, json!({"session": "x"}));
        let out = alias_fields(json!({"worker": "a", "session": "a"}), FieldStyle::Legacy);
        assert_eq!(out, json!({"session": "a"}));
    }

    #[test]
    fn both_never_clobbers_an_inconsistent_pair() {
        // Both present with DIFFERENT values: aliasing leaves the object
        // alone — an inconsistency should stay visible, not be overwritten.
        let v = json!({"worker": "a", "session": "b"});
        assert_eq!(alias_fields(v.clone(), FieldStyle::Both), v);
    }

    #[test]
    fn default_style_is_both() {
        assert_eq!(FieldStyle::default(), FieldStyle::Both);
    }

    #[test]
    fn request_canonicalization_accepts_either_and_rejects_conflicts() {
        // Legacy spelling renamed to modern.
        let out = canonicalize_request_fields(json!({"session": "w1", "x": 1})).unwrap();
        assert_eq!(out, json!({"worker": "w1", "x": 1}));
        // Both equal: collapses to modern.
        let out = canonicalize_request_fields(json!({"worker": "w1", "session": "w1"})).unwrap();
        assert_eq!(out, json!({"worker": "w1"}));
        // Both different: conflict names both fields and both values, so the
        // 400 body can show exactly what disagreed (Invariant 37).
        let err = canonicalize_request_fields(json!({"worker": "a", "session": "b"})).unwrap_err();
        assert_eq!(err.modern_field, "worker");
        assert_eq!(err.legacy_field, "session");
        assert_eq!(err.modern_value, json!("a"));
        assert_eq!(err.legacy_value, json!("b"));
    }

    // ---- route aliasing -------------------------------------------------

    #[test]
    fn rewrite_maps_prefix_subpath_and_query_but_not_near_misses() {
        let u = |s: &str| s.parse::<Uri>().unwrap();
        // Bare /api/sessions is exempt: it has a dedicated shape handler.
        assert!(rewrite_legacy_uri(&u("/api/sessions")).is_none());
        // Session SUBPATHS proxy to the Python fleet owner, never rewrite.
        assert!(rewrite_legacy_uri(&u("/api/sessions/abc/peek?lines=600")).is_none());
        assert_eq!(
            rewrite_legacy_uri(&u("/api/issues/5")).unwrap().path(),
            "/api/tasks/5"
        );
        // Not legacy: canonical paths and prefix near-misses pass through.
        assert!(rewrite_legacy_uri(&u("/api/workers")).is_none());
        assert!(rewrite_legacy_uri(&u("/api/sessionsfoo")).is_none());
        assert!(rewrite_legacy_uri(&u("/health")).is_none());
    }

    fn demo_app() -> Router {
        alias_layer(
            Router::new()
                .route("/api/workers", get(|| async { "list" }))
                .route(
                    "/api/workers/{id}",
                    get(|Path(id): Path<String>| async move { id }),
                )
                .route(
                    "/api/workers/{id}/echo",
                    get(|RawQuery(q): RawQuery| async move { q.unwrap_or_default() }),
                )
                .route(
                    "/api/tasks/{id}/echo",
                    get(|RawQuery(q): RawQuery| async move { q.unwrap_or_default() }),
                ),
        )
    }

    async fn fetch(app: &Router, path: &str) -> (StatusCode, Option<String>, String) {
        let res = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let deprecated = res
            .headers()
            .get("deprecated")
            .map(|v| v.to_str().unwrap().to_string());
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            deprecated,
            String::from_utf8_lossy(&body).into_owned(),
        )
    }

    #[tokio::test]
    async fn legacy_route_resolves_to_same_handler_with_deprecated_header() {
        let app = demo_app();
        // Bare /api/sessions is EXEMPT (dedicated shape handler wins) — the
        // demo app has no such route, so it 404s instead of rewriting.
        let (st, _dep, _body) = fetch(&app, "/api/sessions").await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        // /api/sessions subpaths no longer rewrite (they proxy to the
        // Python fleet owner) — the demo app has no proxy, so 404.
        let (st, _dep, _body) = fetch(&app, "/api/sessions/abc").await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        // Query strings survive the rewrite on a still-rewriting alias.
        let (st, _, body) = fetch(&app, "/api/issues/5/echo?lines=600&x=1").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body, "lines=600&x=1");
    }

    // ---- trailing slash (AMUX-4753) -------------------------------------

    /// The predicate is "is the bare path a DECLARED route", not "ends with a
    /// slash". A blanket strip would reach the SPA shell, the `{*path}` file
    /// servers and `/proxy/`, none of which this card is about.
    #[test]
    fn only_a_routed_bare_path_loses_its_slash() {
        let u = |s: &str| s.parse::<Uri>().unwrap();
        // A real nested root: the whole subject of the card.
        assert_eq!(
            trim_trailing_slash(&u("/api/board/")).unwrap().path(),
            "/api/board"
        );
        assert_eq!(
            trim_trailing_slash(&u("/api/board-lifecycle/"))
                .unwrap()
                .path(),
            "/api/board-lifecycle"
        );
        // Query survives.
        let rewritten = trim_trailing_slash(&u("/api/board/?all=1&slim=0")).unwrap();
        assert_eq!(rewritten.path(), "/api/board");
        assert_eq!(rewritten.query(), Some("all=1&slim=0"));

        // NOT ROUTED: the 404 stays, with the catalog and nearest_routes it
        // already gets. This is the leg that fails on a blanket strip.
        assert!(trim_trailing_slash(&u("/api/definitely-not-a-route/")).is_none());
        // THE ROOT. Stripping it would leave the empty path.
        assert!(trim_trailing_slash(&u("/")).is_none());
        // Nothing to strip.
        assert!(trim_trailing_slash(&u("/api/board")).is_none());
    }

    #[tokio::test]
    async fn a_declared_route_answers_the_same_with_or_without_the_slash() {
        let app = demo_app();
        let (bare, _, bare_body) = fetch(&app, "/api/workers").await;
        let (slash, _, slash_body) = fetch(&app, "/api/workers/").await;
        assert_eq!(bare, StatusCode::OK);
        assert_eq!(
            slash, bare,
            "the slash form 404'd while the bare path answered"
        );
        assert_eq!(
            slash_body, bare_body,
            "the two spellings must reach one handler"
        );

        // An unrouted path keeps its 404: the fix is not "never 404 on a slash".
        let (st, _, _) = fetch(&app, "/api/nope/").await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    /// The legacy rewrite runs FIRST, and this pins why.
    ///
    /// The combination has no live specimen today and the test says so rather
    /// than manufacturing one: `/api/issues` aliases to `/api/tasks`, which is
    /// mounted nowhere (both spellings 404 on the running server) and is absent
    /// from ROUTE_TABLE. So the two halves that ARE true now are pinned instead,
    /// and together they are the argument for the order.
    #[test]
    fn the_slash_is_trimmed_after_the_legacy_rewrite_not_before() {
        let u = |s: &str| s.parse::<Uri>().unwrap();
        // The alias carries a trailing slash through rather than eating it, so
        // there is still something for the strip to do afterwards.
        assert_eq!(
            rewrite_legacy_uri(&u("/api/issues/5/")).unwrap().path(),
            "/api/tasks/5/"
        );
        // And the strip refuses a path ROUTE_TABLE does not declare. Running it
        // first would therefore find nothing routed at the LEGACY spelling and
        // leave a legacy-plus-slash request 404ing, whichever way round the
        // alias target is eventually mounted.
        assert!(trim_trailing_slash(&u("/api/issues/5/")).is_none());
    }

    async fn canonical_header(app: &Router, path: &str) -> Option<String> {
        let res = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        res.headers()
            .get("x-amux-canonical-path")
            .map(|v| v.to_str().unwrap().to_string())
    }

    #[tokio::test]
    async fn the_canonical_path_is_named_on_the_response_and_only_when_rewritten() {
        let app = demo_app();
        assert_eq!(
            canonical_header(&app, "/api/workers/").await.as_deref(),
            Some("/api/workers"),
            "a rewritten request must be able to tell the client the canonical spelling"
        );
        assert_eq!(
            canonical_header(&app, "/api/workers").await,
            None,
            "a request that was already canonical must not be annotated"
        );
    }

    #[tokio::test]
    async fn canonical_route_carries_no_deprecated_header() {
        let app = demo_app();
        let (st, dep, body) = fetch(&app, "/api/workers").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(dep, None);
        assert_eq!(body, "list");
        // Unknown paths still 404 through the wrapper.
        let (st, _, _) = fetch(&app, "/api/nope").await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }
}
