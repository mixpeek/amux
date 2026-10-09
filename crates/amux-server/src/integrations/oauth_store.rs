//! Shared OAuth grant maintenance. Serialize refreshes for an account, retain
//! rotated credentials before returning a bearer, and identify Google/Gmail
//! copies by client and refresh credential rather than merely an email address.
use super::secure_store;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::{fs::File, io};

const MIRROR_HASH: &str = "gmail_mirror_refresh_sha256";

pub(crate) fn valid_account(account: &str) -> bool {
    !account.is_empty()
        && account.len() <= 240
        && account != "."
        && account != ".."
        && !account.contains(['/', '\\', '\0', '\n', '\r'])
}

pub(crate) fn family_path(home: &Path, family: &str, account: &str) -> io::Result<PathBuf> {
    if !valid_account(account)
        || family.is_empty()
        || !family
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid connector account or family",
        ));
    }
    Ok(home
        .join("connectors")
        .join(family)
        .join(format!("{account}.json")))
}

pub(crate) async fn lock(home: &Path, family: &str, account: &str) -> io::Result<File> {
    let path = family_path(home, family, account)?;
    let result = tokio::task::spawn_blocking(move || secure_store::lock(&path))
        .await
        .map_err(io::Error::other)?;
    if let Err(ref error) = result {
        tracing::warn!(family, account, %error, verdict = "connector_refresh_lease_failed", "connector refresh could not acquire its account lease");
    }
    result
}

fn legacy_path(home: &Path, account: &str) -> PathBuf {
    home.join("gmail-tokens").join(format!("{account}.json"))
}
fn field<'a>(body: &'a Value, key: &str) -> &'a str {
    body.get(key).and_then(Value::as_str).unwrap_or("")
}
fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}
fn gmail_scope(tf: &Value) -> bool {
    field(tf, "scopes")
        .split_whitespace()
        .any(|s| s.contains("/auth/gmail.") || s == "https://mail.google.com/")
}

fn linked_mirror(home: &Path, account: &str, canonical: &Value) -> Option<Value> {
    if !gmail_scope(canonical) {
        return None;
    }
    let legacy: Value = secure_store::read_json(&legacy_path(home, account)).ok()?;
    let refresh = field(&legacy, "refresh_token");
    let client = field(&legacy, "client_id");
    if refresh.is_empty() || client.is_empty() || client != field(canonical, "client_id") {
        return None;
    }
    (refresh == field(canonical, "refresh_token") || hash(refresh) == field(canonical, MIRROR_HASH))
        .then_some(legacy)
}

/// Existing unlinked Gmail grants stay independent, including a later explicit
/// Gmail-only reauthorization. A linked compatibility copy may lag a committed
/// rotation after SIGKILL; its fingerprint still points to the canonical grant.
/// Requiring the legacy file preserves the existing explicit disconnect path.
pub(crate) fn gmail_path(home: &Path, account: &str) -> PathBuf {
    let legacy = legacy_path(home, account);
    let Ok(canonical) = family_path(home, "google", account) else {
        return home.join("connectors/invalid-account");
    };
    if !legacy.exists() {
        return legacy;
    }
    match secure_store::read_json::<Value>(&canonical) {
        Ok(tf) if linked_mirror(home, account, &tf).is_some() => canonical,
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            tracing::warn!(%error, verdict = "connector_grant_unreadable", "canonical grant unreadable; refusing to guess from a compatibility copy");
            canonical
        }
        _ => legacy,
    }
}

/// Preserve omitted/blank refresh tokens and arbitrary grant metadata. Reject
/// malformed successful replies before overwriting the only working copy.
pub(crate) fn refreshed(tf: &Value, response: &Value, with_expiry: bool) -> io::Result<Value> {
    let access = field(response, "access_token");
    if access.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "refresh omitted access token",
        ));
    }
    let mut out = tf
        .as_object()
        .cloned()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "grant is not an object"))?;
    out.insert("token".into(), json!(access));
    let rotated = field(response, "refresh_token");
    if !rotated.trim().is_empty() {
        out.insert("refresh_token".into(), json!(rotated));
    }
    if with_expiry {
        let lifetime = response
            .get("expires_in")
            .and_then(Value::as_f64)
            .unwrap_or(3599.0);
        if !lifetime.is_finite() || lifetime <= 0.0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid token lifetime",
            ));
        }
        out.insert(
            "expires_at".into(),
            json!(crate::config::now_f64() + lifetime),
        );
    }
    Ok(Value::Object(out))
}

/// Caller holds the account lease across the HTTP exchange and this commit.
/// The canonical commit retains a fingerprint of the old compatibility copy
/// before rotating: a killed mirror write cannot strand Gmail on the old token.
pub(crate) fn persist_refresh(
    home: &Path,
    family: &str,
    account: &str,
    path: &Path,
    old: &Value,
    updated: Value,
) -> io::Result<()> {
    persist_before_mirror(home, family, account, path, old, updated, || {})
}

fn persist_before_mirror(
    home: &Path,
    family: &str,
    account: &str,
    path: &Path,
    old: &Value,
    mut updated: Value,
    before_mirror: impl FnOnce(),
) -> io::Result<()> {
    let linked = if family == "google" && path == family_path(home, family, account)? {
        linked_mirror(home, account, old)
    } else {
        None
    };
    if let Some(ref legacy) = linked {
        updated[MIRROR_HASH] = json!(hash(field(legacy, "refresh_token")));
    }
    secure_store::write(path, updated.to_string().as_bytes())?;
    if field(old, "refresh_token") != field(&updated, "refresh_token") {
        tracing::info!(
            family,
            account,
            verdict = "connector_refresh_rotated",
            "rotated refresh credential durably committed"
        );
    }
    before_mirror();
    if let Some(mut mirror) = linked {
        // Keep legacy identity warnings and metadata while updating the
        // compatibility fields. Canonical identity warnings remain visible to
        // the Gmail accounts UI, which reads the compatibility file.
        for key in [
            "token",
            "refresh_token",
            "token_uri",
            "client_id",
            "client_secret",
            "identity_unverified",
            "identity_unverified_why",
        ] {
            if let Some(value) = updated.get(key) {
                mirror[key] = value.clone();
            }
        }
        if let Err(error) =
            secure_store::write(&legacy_path(home, account), mirror.to_string().as_bytes())
        {
            tracing::warn!(account, %error, verdict = "connector_gmail_mirror_deferred", "canonical rotation committed; linked Gmail consumers read it until the copy can be updated");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mirror_crash_child() {
        let Ok(root) = std::env::var("AMUX_OAUTH_MIRROR_CRASH_FIXTURE") else {
            return;
        };
        let home = PathBuf::from(root);
        let account = "fixture@example.com";
        let path = family_path(&home, "google", account).unwrap();
        let _lease = secure_store::lock(&path).unwrap();
        let old: Value = secure_store::read_json(&path).unwrap();
        let updated = refreshed(
            &old,
            &json!({"access_token":"committed-access","refresh_token":"rotated-refresh"}),
            true,
        )
        .unwrap();
        persist_before_mirror(&home, "google", account, &path, &old, updated, || {
            secure_store::write(&home.join("mirror-boundary-ready"), b"canonical committed")
                .unwrap();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        })
        .unwrap();
    }

    #[test]
    fn sigkill_between_canonical_commit_and_mirror_preserves_the_rotated_grant() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let home = tempfile::tempdir().unwrap();
        let account = "fixture@example.com";
        let old = json!({"token":"old-access","refresh_token":"old-refresh","client_id":"fixture-client","client_secret":"fixture-secret","token_uri":"fixture","scopes":"https://www.googleapis.com/auth/gmail.modify"});
        let canonical = family_path(home.path(), "google", account).unwrap();
        secure_store::write(&canonical, old.to_string().as_bytes()).unwrap();
        secure_store::write(
            &legacy_path(home.path(), account),
            old.to_string().as_bytes(),
        )
        .unwrap();
        let output = File::create(home.path().join("child-output.log")).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "integrations::oauth_store::tests::mirror_crash_child",
                "--nocapture",
            ])
            .env("AMUX_OAUTH_MIRROR_CRASH_FIXTURE", home.path())
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !home.path().join("mirror-boundary-ready").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let ready = home.path().join("mirror-boundary-ready").exists();
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            ready,
            "child never reached the committed canonical / stale mirror boundary: {}",
            std::fs::read_to_string(home.path().join("child-output.log")).unwrap_or_default()
        );
        let _lease = secure_store::lock(&canonical).expect("SIGKILL releases the account lease");
        let recovered = gmail_path(home.path(), account);
        assert_eq!(recovered, canonical);
        let grant: Value = secure_store::read_json(&recovered).unwrap();
        assert_eq!(grant["refresh_token"], "rotated-refresh");
        let copy: Value = secure_store::read_json(&legacy_path(home.path(), account)).unwrap();
        assert_eq!(
            copy["refresh_token"], "old-refresh",
            "the compatibility write was actually interrupted"
        );
        let next = refreshed(
            &grant,
            &json!({"access_token":"next-access","refresh_token":"next-refresh"}),
            true,
        )
        .unwrap();
        persist_refresh(home.path(), "google", account, &canonical, &grant, next).unwrap();
        let repaired: Value = secure_store::read_json(&legacy_path(home.path(), account)).unwrap();
        assert_eq!(repaired["refresh_token"], "next-refresh");
    }
    #[test]
    fn a_killed_mirror_update_still_resolves_the_committed_rotation() {
        let home = tempfile::tempdir().unwrap();
        let account = "fixture@example.com";
        let old = json!({"token":"old-access","refresh_token":"old-refresh","client_id":"client","client_secret":"fixture","token_uri":"fixture","scopes":"https://www.googleapis.com/auth/gmail.modify"});
        secure_store::write(
            &legacy_path(home.path(), account),
            old.to_string().as_bytes(),
        )
        .unwrap();
        let mut committed = refreshed(
            &old,
            &json!({"access_token":"new-access","refresh_token":"new-refresh"}),
            true,
        )
        .unwrap();
        committed[MIRROR_HASH] = json!(hash("old-refresh"));
        let canonical = family_path(home.path(), "google", account).unwrap();
        secure_store::write(&canonical, committed.to_string().as_bytes()).unwrap();
        assert_eq!(
            gmail_path(home.path(), account),
            canonical,
            "old mirror must not be used after the rotation commit"
        );
        let mut independent = old.clone();
        independent["refresh_token"] = json!("explicit-later-gmail-grant");
        secure_store::write(
            &legacy_path(home.path(), account),
            independent.to_string().as_bytes(),
        )
        .unwrap();
        assert_eq!(
            gmail_path(home.path(), account),
            legacy_path(home.path(), account),
            "an independent later Gmail grant must not be overwritten or shadowed"
        );
        std::fs::remove_file(legacy_path(home.path(), account)).unwrap();
        assert_eq!(
            gmail_path(home.path(), account),
            legacy_path(home.path(), account),
            "disconnect does not recreate a grant"
        );
    }
    #[test]
    fn omitted_refresh_retains_credentials_and_identity_metadata() {
        let tf = json!({"refresh_token":"keep","identity_unverified":true,"scopes":"keep-scope"});
        for rt in [Value::Null, json!(""), json!(" ")] {
            let out = refreshed(
                &tf,
                &json!({"access_token":"fresh","refresh_token":rt}),
                false,
            )
            .unwrap();
            assert_eq!(out["refresh_token"], "keep");
            assert_eq!(out["identity_unverified"], true);
            assert_eq!(out["scopes"], "keep-scope");
        }
    }
    #[test]
    fn grant_paths_reject_traversal_and_unsafe_account_names() {
        let home = tempfile::tempdir().unwrap();
        for account in [
            "..",
            "../outside",
            "one/two",
            "one\\two",
            "bad\0name",
            "bad\nname",
        ] {
            assert!(family_path(home.path(), "google", account).is_err());
        }
        assert!(family_path(home.path(), "../outside", "a@b").is_err());
        assert!(family_path(home.path(), "slack", "Fixture Workspace").is_ok());
    }
}
