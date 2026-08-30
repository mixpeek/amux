//! Google service-account domain-wide delegation (AMUX-3347). Mints an
//! impersonated OAuth access token for a scope by signing a JWT assertion
//! (RS256) with the service-account private key and exchanging it at Google's
//! token endpoint. This lets the Mixpeek Google connectors (Gmail send, Drive,
//! Calendar, ...) act as a Workspace user WITHOUT a per-user browser OAuth grant.
//!
//! SECURITY: the SA private key lives ONLY in the key FILE (its path is
//! `GOOGLE_SA_KEY_FILE`; the impersonation subject is `GOOGLE_SA_SUBJECT`). This
//! module reads the file to sign and NEVER logs the key or the minted token. The
//! token-endpoint error IS surfaced — Google's `unauthorized_client` names the
//! missing scope, which is the Workspace-Admin-console fix a human must apply.

use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct SaKey {
    client_email: String,
    private_key: String,
    #[serde(default = "default_token_uri")]
    token_uri: String,
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".into()
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    sub: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: i64,
    exp: i64,
}

/// `(key_file_path, impersonation_subject)` if both are configured, else None.
/// server.env is read fresh so a just-added key needs no restart.
pub fn sa_config() -> Option<(String, String)> {
    let file_env = crate::config::parse_env_file(&crate::config::amux_home().join("server.env"));
    let get = |k: &str| {
        file_env
            .get(k)
            .cloned()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
    };
    Some((get("GOOGLE_SA_KEY_FILE")?, get("GOOGLE_SA_SUBJECT")?))
}

/// True only when the SA is configured AND its key file actually EXISTS — i.e. a
/// mint can be attempted. `sa_config().is_some()` means the config is SET; the
/// file behind it can go missing (it did — GOOGLE_SA_KEY_FILE pointed at an
/// ephemeral `~/.amux/uploads/…` copy that got cleaned up, so the connector read
/// "connected" while every mint 502'd, AMUX-3383). Status/usable checks that gate
/// on "can we mint" must use THIS, not `sa_config().is_some()`, or they report a
/// key that no longer exists as connected (ethos rule 4).
pub fn sa_usable() -> bool {
    sa_config()
        .map(|(path, _)| std::path::Path::new(&path).exists())
        .unwrap_or(false)
}

/// A minted impersonated access token and the seconds until Google expires it.
/// The token is a SECRET — never log it.
pub struct MintedToken {
    pub access_token: String,
    pub expires_in: i64,
}

/// Mint an access token for `scope` (space-delimited Google OAuth scopes),
/// impersonating the CONFIGURED subject (`GOOGLE_SA_SUBJECT`). Thin wrapper over
/// [`mint_token_as`] returning just the bearer, so existing callers (the Test
/// button) are unchanged. NEVER log the returned token.
pub async fn mint_token(scope: &str) -> Result<String, String> {
    let (_, subject) = sa_config().ok_or("GOOGLE_SA_KEY_FILE / GOOGLE_SA_SUBJECT not set")?;
    Ok(mint_token_as(scope, &subject).await?.access_token)
}

/// Mint an access token for `scope`, impersonating an explicit `subject` — any
/// Workspace user the SA is domain-wide-delegated for. The CALLER decides the
/// subject, which is what binds a token to the requesting user rather than
/// letting a caller impersonate the whole domain: in cloud the connector passes
/// the gateway-authenticated user, locally it passes the configured subject.
/// Returns the token plus its lifetime; the error string is safe to surface
/// (carries no secret). NEVER log the returned token.
pub async fn mint_token_as(scope: &str, subject: &str) -> Result<MintedToken, String> {
    let (path, _) = sa_config().ok_or("GOOGLE_SA_KEY_FILE / GOOGLE_SA_SUBJECT not set")?;
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("read SA key file: {e}"))?;
    let key: SaKey = serde_json::from_str(&raw).map_err(|e| format!("parse SA key JSON: {e}"))?;
    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        iss: &key.client_email,
        sub: subject,
        scope,
        aud: &key.token_uri,
        iat: now,
        exp: now + 3600,
    };
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    let enc = jsonwebtoken::EncodingKey::from_rsa_pem(key.private_key.as_bytes())
        .map_err(|e| format!("SA private key is not valid RSA PEM: {e}"))?;
    let assertion =
        jsonwebtoken::encode(&header, &claims, &enc).map_err(|e| format!("sign JWT: {e}"))?;
    // application/x-www-form-urlencoded body, built by hand so no extra reqwest
    // feature is needed. The JWT (base64url + '.') and the grant-type URN carry
    // no characters that require form-encoding.
    let form = format!(
        "grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&assertion={assertion}"
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let resp = client
        .post(&key.token_uri)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form)
        .send()
        .await
        .map_err(|e| format!("token exchange request: {e}"))?;
    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("token exchange body: {e}"))?;
    if !status.is_success() {
        let err = body.get("error").and_then(|v| v.as_str()).unwrap_or("error");
        let desc = body
            .get("error_description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return Err(format!("{err}: {desc}"));
    }
    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "token exchange returned no access_token".to_string())?;
    let expires_in = body
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(3600);
    Ok(MintedToken {
        access_token,
        expires_in,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AMUX-3383: a configured-but-missing SA key file must read as NOT usable.
    /// `sa_config().is_some()` only means the config is set; the file behind
    /// GOOGLE_SA_KEY_FILE can be gone (it pointed at an ephemeral uploads copy
    /// that got cleaned up), and the connector reading "connected" off it while
    /// every mint 502'd is the dishonest status this guards against.
    #[test]
    fn sa_usable_requires_the_key_file_to_exist_not_just_be_configured() {
        let dir = tempfile::tempdir().expect("tmp");
        let _g = crate::api::settings::test_env::set_home(dir.path());
        let key = dir.path().join("dpa-sa.json");
        let env = dir.path().join("server.env");

        // Configured, but the key file is MISSING -> Some config, NOT usable.
        std::fs::write(
            &env,
            format!(
                "GOOGLE_SA_KEY_FILE={}\nGOOGLE_SA_SUBJECT=ethan@mixpeek.com\n",
                key.display()
            ),
        )
        .unwrap();
        assert!(sa_config().is_some(), "config is set");
        assert!(!sa_usable(), "a missing key file is NOT usable");

        // Key file present -> usable.
        std::fs::write(&key, "{}").unwrap();
        assert!(sa_usable(), "an existing key file is usable");
    }
}
