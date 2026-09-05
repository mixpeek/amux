//! GitHub OAuth connector — integrates with encrypted secrets for credentials
//! 
//! Phase 7: Uses secrets infrastructure (Phase 1-6) for OAuth credential storage

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::api::AppState;

#[derive(Serialize, Deserialize, Debug)]
pub struct GitHubConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
}

#[derive(Serialize, Deserialize)]
pub struct GitHubAuthRequest {
    pub code: String,
    pub state: String,
}

#[derive(Serialize, Deserialize)]
pub struct GitHubAuthResponse {
    pub access_token: String,
    pub token_type: String,
    pub scope: String,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/github/auth/start", get(start_auth))
        .route("/api/github/auth/callback", get(handle_callback))
        .route("/api/github/status", get(github_status))
}

/// Start GitHub OAuth flow
async fn start_auth(State(state): State<AppState>) -> impl IntoResponse {
    match load_github_config(&state).await {
        Ok(config) => {
            // Simple percent-encoding for URL parameter
            let encoded_redirect = config.redirect_uri
                .replace(' ', "%20")
                .replace('?', "%3F")
                .replace('&', "%26")
                .replace('#', "%23");

            let auth_url = format!(
                "https://github.com/login/oauth/authorize?\
                client_id={}&redirect_uri={}&scope=repo,user",
                config.client_id,
                encoded_redirect
            );
            
            Json(serde_json::json!({
                "ok": true,
                "auth_url": auth_url
            })).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "ok": false,
                "error": format!("GitHub not configured: {}", e)
            }))
        ).into_response()
    }
}

/// Handle GitHub OAuth callback
async fn handle_callback(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let code = match params.get("code") {
        Some(c) => c,
        None => return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing code"}))
        ).into_response()
    };

    match exchange_code_for_token(&state, code).await {
        Ok(token) => Json(serde_json::json!({
            "ok": true,
            "access_token": token
        })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e}))
        ).into_response()
    }
}

/// Check GitHub connection status
async fn github_status(State(state): State<AppState>) -> impl IntoResponse {
    match load_github_config(&state).await {
        Ok(_) => Json(serde_json::json!({
            "connected": true,
            "message": "GitHub connector configured"
        })).into_response(),
        Err(e) => Json(serde_json::json!({
            "connected": false,
            "message": format!("GitHub not configured: {}", e)
        })).into_response()
    }
}

/// Load GitHub config from secrets
async fn load_github_config(state: &AppState) -> Result<GitHubConfig, String> {
    let client_id = state.secrets.get("oauth.github.client_id").await
        .ok_or("GitHub client ID not configured")?;
    let client_secret = state.secrets.get("oauth.github.client_secret").await
        .ok_or("GitHub client secret not configured")?;
    let redirect_uri = state.secrets.get("oauth.github.redirect_uri").await
        .unwrap_or_else(|| "http://localhost:8824/api/github/callback".to_string());

    Ok(GitHubConfig {
        client_id,
        client_secret,
        redirect_uri,
    })
}

/// Exchange authorization code for access token (mock)
async fn exchange_code_for_token(_state: &AppState, code: &str) -> Result<String, String> {
    // In production, this would call GitHub's token endpoint
    // For now, return a mock token
    Ok(format!("gho_mock_{}", code))
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_github_config_load() {
        // TODO: Mock secrets store and test config loading
    }
}
