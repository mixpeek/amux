//! Retired: versioned goal contracts, planning nodes and per-task budgets.
//!
//! These routes served the dormant typed model's planning plane
//! (`_amux_goal_contracts`, `_amux_planning_nodes`, `_amux_current_plans`,
//! `_amux_task_budgets`). No client called them (17.1M logged requests since
//! 2026-11-13, none here) and every table was empty, so migration 0180 drops
//! the tables (primitive integrity audit 2026-11-26, step 2). The paths stay
//! mounted so a caller holding an old instruction gets a pointer, not a 404.
//!
//! Log signal: every call logs `verdict = "planning_route_retired"`.

use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

/// What a caller of a retired route is told to use instead.
pub const RETIRED_POINTER: &str = "goals, planning nodes and task budgets were retired on 2026-11-26 (they had no client). \
     Plan with board cards instead: POST /api/board (or `amux board add`) for each unit of work.";

/// The retired paths, mounted under `/api/harness` with the methods they had.
pub fn routes() -> Router<super::AppState> {
    Router::new()
        .route("/goals", post(retired))
        .route("/goals/{id}", get(retired).put(retired))
        .route("/goals/{id}/nodes", post(retired).get(retired))
        .route("/planning-nodes/{id}", get(retired))
        .route("/planning-nodes/{id}/plan", get(retired).put(retired))
        .route("/budgets/{id}", get(retired).put(retired))
}

pub(crate) async fn retired(uri: Uri) -> Response {
    tracing::warn!(path = %uri.path(), measured = true, n_considered = 1, verdict = "planning_route_retired",
        "a client called a retired planning or budget route");
    (StatusCode::GONE, Json(json!({"ok": false, "code": "route_retired", "error": RETIRED_POINTER}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_retired_route_answers_410_with_the_pointer() {
        use tower::ServiceExt;
        let app: Router = Router::new()
            .nest("/api/harness", routes())
            .with_state(crate::api::standing_approvals::tests::test_state());
        for (method, path) in [
            ("POST", "/api/harness/goals"),
            ("GET", "/api/harness/goals/g1"),
            ("PUT", "/api/harness/goals/g1"),
            ("POST", "/api/harness/goals/g1/nodes"),
            ("GET", "/api/harness/goals/g1/nodes"),
            ("GET", "/api/harness/planning-nodes/n1"),
            ("GET", "/api/harness/planning-nodes/n1/plan"),
            ("PUT", "/api/harness/planning-nodes/n1/plan"),
            ("GET", "/api/harness/budgets/AMUX-1"),
            ("PUT", "/api/harness/budgets/AMUX-1"),
        ] {
            let req = axum::http::Request::builder().method(method).uri(path).body(axum::body::Body::empty()).unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::GONE, "{method} {path}");
            let body = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(v["code"], "route_retired", "{method} {path}");
            assert!(v["error"].as_str().unwrap_or("").contains("/api/board"), "{v}");
        }
    }
}
