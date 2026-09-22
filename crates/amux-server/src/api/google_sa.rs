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

/// A lookup into the ambient (process) environment. Injected rather than called
/// directly so that [`sa_config_in`]'s suppression of it is testable without a
/// `std::env::set_var`, which would be order-dependent across a threaded test
/// binary — the exact flake class AF-529 is about.
type AmbientEnv<'a> = &'a dyn Fn(&str) -> Option<String>;
type OwnedAmbientEnv = Box<dyn Fn(&str) -> Option<String>>;

/// `(key_file_path, impersonation_subject)` if both are configured, else None.
/// server.env is read fresh so a just-added key needs no restart.
///
/// Reads the REAL amux home. Any caller that was handed a home must use
/// [`sa_config_in`] instead — see its docstring for what goes wrong otherwise.
pub fn sa_config() -> Option<(String, String)> {
    sa_config_in(&crate::config::amux_home())
}

/// The same resolution, under an EXPLICIT amux home.
///
/// Two things used to leak here and they are independent (AF-529). `sa_config`
/// read `amux_home()` unconditionally, so a caller operating under a different
/// home — a test's tempdir, a second install — silently got the real box's
/// config; and the process-env fallback then answered even when that home's own
/// server.env said nothing.
///
/// The ambient fallback is consulted ONLY when `home` IS the real amux home,
/// because that is the one case where the process env was seeded from this same
/// server.env at startup ("loaded at startup as setdefault"). Under any other
/// home the ambient env belongs to a DIFFERENT installation, and answering from
/// it means the handler's behaviour is decided by the machine it runs on rather
/// than by the home it was handed.
///
/// Measured cost of not doing this: `connectors::mint_connector_token` gates the
/// needs_auth branch on `sa_usable()`. On a CI runner nothing is set, the branch
/// fires, and the response carries a `connect` action. On any amux box
/// GOOGLE_SA_KEY_FILE is exported and the file exists, so the branch does NOT
/// fire and the response has no `connect` — the same commit, green in CI and red
/// locally, with neither result saying which environment it was measuring.
pub fn sa_config_in(home: &std::path::Path) -> Option<(String, String)> {
    let file_env = crate::config::parse_env_file(&home.join("server.env"));
    let ambient: Option<OwnedAmbientEnv> = if home == crate::config::amux_home() {
        Some(Box::new(|k: &str| std::env::var(k).ok()))
    } else {
        None
    };
    resolve_sa(&file_env, ambient.as_deref())
}

/// The resolution itself, with the ambient lookup INJECTED rather than read.
///
/// Split out so the suppression above is testable without touching the process
/// env: a test hands in an ambient closure that always answers, and asserts it
/// is ignored when it should be. `std::env::set_var` would make the same
/// assertion order-dependent across a threaded test binary, which is the class
/// of flake this whole card is about.
fn resolve_sa(
    file_env: &std::collections::BTreeMap<String, String>,
    ambient: Option<AmbientEnv<'_>>,
) -> Option<(String, String)> {
    let get = |k: &str| {
        file_env
            .get(k)
            .cloned()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| ambient.and_then(|f| f(k)).filter(|v| !v.trim().is_empty()))
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
    sa_usable_in(&crate::config::amux_home())
}

/// [`sa_usable`] under an explicit home. Same home-scoping rule as
/// [`sa_config_in`]; every caller holding a `ConnectorsCtx` must use this one.
pub fn sa_usable_in(home: &std::path::Path) -> bool {
    sa_config_in(home)
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
    let form =
        format!("grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&assertion={assertion}");
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
        let err = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("error");
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
    use std::collections::BTreeMap;

    fn env_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// AF-529. The ambient (process-env) lookup is a PARAMETER, so this asserts
    /// the suppression itself rather than asserting what some machine happens to
    /// export. Hand in an ambient that always answers: consulted when supplied,
    /// ignored when not. Delete the `ambient.and_then(..)` arm and the first half
    /// fails; make it unconditional and the second half fails.
    #[test]
    fn the_ambient_env_is_consulted_only_when_it_is_handed_in() {
        let always = |_: &str| Some("from-ambient".to_string());
        let empty = env_of(&[]);

        let with = resolve_sa(&empty, Some(&always));
        assert_eq!(
            with,
            Some(("from-ambient".into(), "from-ambient".into())),
            "an ambient lookup that was handed in must be used when the file says nothing"
        );

        let without = resolve_sa(&empty, None);
        assert_eq!(
            without, None,
            "with no ambient handed in, an empty server.env must resolve to None — \
             this is the arm that kept a handler's behaviour tied to the box it ran on"
        );
    }

    /// The file always wins over the ambient, in both directions, so scoping the
    /// ambient cannot be mistaken for scoping the whole resolution.
    #[test]
    fn the_files_value_wins_over_the_ambient_one() {
        let always = |_: &str| Some("from-ambient".to_string());
        let file = env_of(&[
            ("GOOGLE_SA_KEY_FILE", "/from/file.json"),
            ("GOOGLE_SA_SUBJECT", "file@example.com"),
        ]);
        assert_eq!(
            resolve_sa(&file, Some(&always)),
            Some(("/from/file.json".into(), "file@example.com".into()))
        );
        // A blank in the file is not a value; it falls through, same as before.
        let blank = env_of(&[
            ("GOOGLE_SA_KEY_FILE", "   "),
            ("GOOGLE_SA_SUBJECT", "s@e.com"),
        ]);
        assert_eq!(
            resolve_sa(&blank, Some(&always)),
            Some(("from-ambient".into(), "s@e.com".into()))
        );
    }

    /// AF-529, the end-to-end half: `sa_config_in` reads the home it is HANDED,
    /// not `amux_home()`. Both arms run on every machine — the first proves it
    /// reads the temp home at all (so the second cannot pass by reading nothing),
    /// the second proves an unconfigured temp home answers None even on a box
    /// with GOOGLE_SA_KEY_FILE exported, which is exactly where this was red.
    #[test]
    fn sa_config_in_reads_the_home_it_was_handed_and_no_other() {
        let configured = tempfile::tempdir().unwrap();
        std::fs::write(
            configured.path().join("server.env"),
            "GOOGLE_SA_KEY_FILE=/tmp/af529-key.json\nGOOGLE_SA_SUBJECT=svc@example.com\n",
        )
        .unwrap();
        assert_eq!(
            sa_config_in(configured.path()),
            Some(("/tmp/af529-key.json".into(), "svc@example.com".into())),
            "must read the server.env of the home it was given"
        );

        // A DIFFERENT home must not see the first one's values. This is the
        // "and no other" in this test's name, and it holds on every machine.
        //
        // THE TEMPTING ASSERTION HERE IS `sa_config_in(bare) == None`, AND IT IS
        // NOT SAFE (AF-532). It is only meaningful when the ambient env actually
        // carries GOOGLE_SA_*, and whether it does is decided by a SIBLING test:
        // `settings::test_env::set_home` strips every key found in the real
        // ~/.amux/server.env for as long as its guard is held, and cargo runs
        // these threads in parallel. So the assertion passed in a group run and
        // failed alone, on identical bytes — I spent hours blaming
        // scripts/mutate.sh for that, which was innocent. It is also machine
        // dependent: on CI there is no ~/.amux/server.env, so nothing is
        // stripped and it would have meant something different again.
        //
        // The ambient-suppression rule is asserted where it CAN be deterministic:
        // `the_ambient_env_is_consulted_only_when_it_is_handed_in`, which injects
        // the lookup instead of reading the machine's.
        let bare = tempfile::tempdir().unwrap();
        assert_ne!(
            sa_config_in(bare.path()),
            Some(("/tmp/af529-key.json".into(), "svc@example.com".into())),
            "a home with no server.env must not answer with a DIFFERENT home's values"
        );
    }

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
