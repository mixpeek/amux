//! Encryption at rest, as the owner sees it (governance PR 18). The mechanism
//! is `integrations::at_rest`; this is its report and the owner's way back.
use axum::{http::HeaderMap, http::StatusCode, response::IntoResponse, response::Response, Json};
use serde_json::json;

/// GET /api/security/at-rest: per store, encrypted yes/no/partial, the key
/// provider, and whether the key sits beside the data. People only.
pub async fn view(headers: HeaderMap) -> Response {
    if super::human_proof::proven(&headers).is_none() {
        return (StatusCode::FORBIDDEN,
            Json(json!({"error": "people only", "code": "at_rest_view_refused"}))).into_response();
    }
    let home = crate::config::amux_home();
    let v = tokio::task::spawn_blocking(move || crate::integrations::at_rest::status(&home))
        .await
        .unwrap_or_else(|e| json!({"measured": false, "why_unmeasured": e.to_string(), "n_considered": 0}));
    Json(v).into_response()
}

/// POST /api/security/at-rest/decrypt: rewrite sealed grants as plain files.
/// The owner only, and only with sealing off (`AMUX_ENCRYPT_AT_REST=off`).
pub async fn decrypt(headers: HeaderMap) -> Response {
    if !matches!(super::human_proof::proven(&headers), Some(super::human_proof::Proven::Owner)) {
        return (StatusCode::FORBIDDEN,
            Json(json!({"error": "the owner only", "code": "at_rest_decrypt_refused"}))).into_response();
    }
    let home = crate::config::amux_home();
    match tokio::task::spawn_blocking(move || crate::integrations::at_rest::decrypt_all(&home)).await {
        Ok(Ok((n, done, locked))) => Json(json!({"ok": true, "measured": true, "n_considered": n,
            "decrypted": done, "locked": locked})).into_response(),
        Ok(Err(e)) => (StatusCode::CONFLICT, Json(json!({"ok": false, "error": e}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": e.to_string()}))).into_response(),
    }
}
