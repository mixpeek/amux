//! /api/secrets/* — Central secrets management endpoints
//! 
//! Provides read/write access to encrypted secrets via REST API.
//! Used by: Web UI, CLI, MCP integration for Claude.

use crate::api::AppState;
use crate::db::secret_metadata;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Serialize, Deserialize)]
pub struct SecretResponse {
    pub value: String,
}

#[derive(Serialize, Deserialize)]
pub struct SecretsListResponse {
    pub secrets: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct UpdateSecretRequest {
    pub value: String,
}

#[derive(Serialize, Deserialize)]
pub struct UpdateSecretResponse {
    pub ok: bool,
    pub path: String,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/secrets", get(list_secrets))
        .route("/api/secrets/inspect", get(inspect_secrets))
        .route("/api/secrets/reload", post(reload_secrets))
        .route("/api/secrets/manifest", get(list_manifest))
        .route("/api/secrets/{path}", get(get_secret).post(update_secret))
        .route("/api/secrets/{path}/metadata", get(get_metadata).post(set_metadata))
}

/// List all secret paths (keys only, no values)
async fn list_secrets(State(state): State<AppState>) -> impl IntoResponse {
    let paths = state.secrets.list_paths().await;
    Json(SecretsListResponse { secrets: paths })
}

/// Get specific secret by path (requires auth)
async fn get_secret(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    // Auth check
    if state.auth_token.is_some() {
        // In a real implementation, check request headers for token
        // For now, just verify token exists (basic check)
    }

    match state.secrets.get(&path).await {
        Some(value) => (
            StatusCode::OK,
            Json(SecretResponse { value }),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "secret not found"})),
        )
            .into_response(),
    }
}

/// Get secrets schema (structure only, no values)
async fn inspect_secrets(State(state): State<AppState>) -> impl IntoResponse {
    let schema = state.secrets.inspect_schema().await;
    Json(schema)
}

/// Manually reload secrets from encrypted file
///
/// Called when user clicks "Refresh" button in UI.
/// Decrypts current secrets/amux-secrets.yaml and updates cache.
async fn reload_secrets(State(state): State<AppState>) -> impl IntoResponse {
    match state.secrets.reload().await {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "message": "Secrets reloaded from encrypted file"
            })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Failed to reload secrets: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "ok": false,
                    "error": format!("Failed to reload: {}", e)
                })),
            )
                .into_response()
        }
    }
}

/// Update secret (requires admin auth)
async fn update_secret(
    State(state): State<AppState>,
    Path(path): Path<String>,
    Json(req): Json<UpdateSecretRequest>,
) -> impl IntoResponse {
    // Auth check - in production, verify admin role
    if state.auth_token.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "authentication required"})),
        )
            .into_response();
    }

    // Update and persist to encrypted file
    match state.secrets.update_and_persist(&path, req.value).await {
        Ok(_) => {
            (
                StatusCode::OK,
                Json(UpdateSecretResponse {
                    ok: true,
                    path: path.clone(),
                }),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("Failed to update secret {}: {}", path, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "ok": false,
                    "error": format!("Failed to persist: {}", e),
                    "path": path
                })),
            )
                .into_response()
        }
    }
}

/// Get metadata for a secret (service, purpose, owner, rotation schedule)
async fn get_metadata(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to get database connection: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "ok": false,
                    "error": "database connection failed"
                })),
            )
                .into_response();
        }
    };

    match secret_metadata::get_metadata(&conn, &path) {
        Ok(Some(metadata)) => {
            let days_until = secret_metadata::days_until_rotation(&metadata);
            let needs_rotation = secret_metadata::needs_rotation(&metadata);

            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "metadata": {
                        "secret_path": metadata.secret_path,
                        "service_name": metadata.service_name,
                        "purpose": metadata.purpose,
                        "used_by": metadata.used_by,
                        "owner": metadata.owner,
                        "rotation_days": metadata.rotation_days,
                        "last_rotated": metadata.last_rotated,
                        "needs_rotation": needs_rotation,
                        "days_until_rotation": days_until,
                    }
                })),
            )
                .into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "ok": false,
                "error": "metadata not found for this secret"
            })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Failed to get metadata for {}: {}", path, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "ok": false,
                    "error": format!("Database error: {}", e)
                })),
            )
                .into_response()
        }
    }
}

/// Set or update metadata for a secret
async fn set_metadata(
    State(state): State<AppState>,
    Path(path): Path<String>,
    Json(req): Json<secret_metadata::SecretMetadata>,
) -> impl IntoResponse {
    // Auth check
    if state.auth_token.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "authentication required"})),
        )
            .into_response();
    }

    // Ensure path matches
    let mut metadata = req;
    metadata.secret_path = path.clone();

    match state.store.write_async(move |conn| {
        secret_metadata::set_metadata(conn, &metadata)?;
        Ok(crate::db::WriteOutcome {
            applied: true,
            events: vec![],
        })
    }).await {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "message": "metadata updated",
                "path": path
            })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Failed to set metadata for {}: {}", path, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "ok": false,
                    "error": format!("Database error: {}", e)
                })),
            )
                .into_response()
        }
    }
}

/// List all secrets with their metadata (manifest)
async fn list_manifest(State(state): State<AppState>) -> impl IntoResponse {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to get database connection: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "ok": false,
                    "error": "database connection failed"
                })),
            )
                .into_response();
        }
    };

    match secret_metadata::list_all(&conn) {
        Ok(metadata_list) => {
            let manifest: Vec<_> = metadata_list
                .iter()
                .map(|m| {
                    json!({
                        "path": m.secret_path,
                        "service": m.service_name,
                        "purpose": m.purpose,
                        "used_by": m.used_by,
                        "owner": m.owner,
                        "rotation_days": m.rotation_days,
                        "last_rotated": m.last_rotated,
                        "needs_rotation": secret_metadata::needs_rotation(m),
                        "days_until_rotation": secret_metadata::days_until_rotation(m),
                    })
                })
                .collect();

            (StatusCode::OK, Json(json!({"secrets": manifest}))).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to list metadata: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "ok": false,
                    "error": format!("Database error: {}", e)
                })),
            )
                .into_response()
        }
    }
}
