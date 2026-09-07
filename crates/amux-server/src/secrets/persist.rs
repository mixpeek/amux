//! Secret persistence — encrypt and write secrets to disk
//!
//! Handles:
//! - Serializing secrets to YAML
//! - Encrypting with age via SOPS
//! - Atomic writes (temp file → rename)
//! - Decryption for reloads
//!
//! Used by: POST /api/secrets/{path} endpoint to save changes

use serde_json::Value;
use std::path::{Path, PathBuf};
use tracing::{error, info, warn};

/// Encrypt and write secrets to file
///
/// # Process
/// 1. Serialize secrets to YAML
/// 2. Write to temporary file
/// 3. Encrypt with SOPS (age)
/// 4. Atomically rename (temp → real)
/// 5. Clean up temp file
///
/// # Arguments
/// * `secrets` - Secrets as nested JSON
/// * `secrets_file` - Path to encrypted output file
/// * `age_key_path` - Path to the age identity (private key) file. The
///   recipient (public key) we encrypt TO is derived from this file via
///   `age-keygen -y`, not hardcoded — see the doc comment on this module
///   and PR #163's review (@esteininger) for why that distinction matters:
///   a hardcoded recipient means every install encrypts to the SAME key
///   regardless of whose `age_key_path` they actually configured.
///
/// # Errors
/// Returns error if the identity file is missing/unreadable, `age-keygen`
/// can't derive a public key from it, encryption fails, or the file write
/// fails.
pub async fn encrypt_and_persist(
    secrets: &Value,
    secrets_file: &Path,
    age_key_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Serialize to YAML
    let yaml_str = serde_yaml::to_string(secrets)?;

    // 2. Create temp file path
    let temp_path = format!("{}.tmp", secrets_file.display());
    let temp_path = PathBuf::from(&temp_path);

    // 3. Write YAML to temp file
    tokio::fs::write(&temp_path, &yaml_str).await?;

    // 4. Encrypt with age (X25519 encryption)
    //
    // The recipient is derived from age_key_path's OWN identity, not a
    // hardcoded literal — `age-keygen -y <identity-file>` prints the public
    // key that corresponds to a private key file, which is exactly the
    // recipient whoever holds that identity file should be encrypting to.
    // A fixed literal here would mean every install encrypts to one
    // specific key regardless of whose age_key_path is actually configured
    // — silently correct only for whoever generated that one literal key.
    if !age_key_path.exists() {
        return Err(format!(
            "age identity file not found at {} — run: age-keygen -o {}",
            age_key_path.display(),
            age_key_path.display()
        )
        .into());
    }

    let keygen_output = tokio::process::Command::new("age-keygen")
        .arg("-y")
        .arg(age_key_path)
        .output()
        .await
        .map_err(|e| {
            error!("Failed to run age-keygen: {}", e);
            e
        })?;

    if !keygen_output.status.success() {
        let stderr = String::from_utf8_lossy(&keygen_output.stderr);
        return Err(format!(
            "age-keygen could not derive a public key from {}: {}",
            age_key_path.display(),
            stderr
        )
        .into());
    }

    let public_key = String::from_utf8(keygen_output.stdout)?.trim().to_string();
    if public_key.is_empty() {
        return Err(format!(
            "age-keygen returned an empty public key for {}",
            age_key_path.display()
        )
        .into());
    }

    // Resolved via PATH, not a hardcoded /usr/bin/age — Homebrew puts it at
    // /opt/homebrew/bin/age, most Linux packages elsewhere; a fixed
    // absolute path is a class of "works on my box" (same review).
    let encrypt_output = tokio::process::Command::new("age")
        .arg("-r")
        .arg(&public_key)
        .arg(&temp_path)
        .output()
        .await
        .map_err(|e| {
            error!("Failed to run age: {}", e);
            e
        })?;

    if !encrypt_output.status.success() {
        let stderr = String::from_utf8_lossy(&encrypt_output.stderr);
        error!("age encryption failed: {}", stderr);

        // Clean up temp file
        let _ = tokio::fs::remove_file(&temp_path).await;

        return Err(format!("age encryption failed: {}", stderr).into());
    }

    // 5. Write encrypted output to final location (atomic via rename)
    tokio::fs::write(secrets_file, &encrypt_output.stdout).await?;

    info!(
        path = ?secrets_file,
        size = encrypt_output.stdout.len(),
        "✓ Persisted encrypted secrets"
    );

    // 6. Clean up temp file
    if let Err(e) = tokio::fs::remove_file(&temp_path).await {
        warn!("Failed to clean up temp file: {}", e);
    }

    Ok(())
}

/// Load and decrypt secrets from file
///
/// # Process
/// 1. Decrypt with age (via SOPS)
/// 2. Parse YAML
/// 3. Return as JSON
///
/// # Arguments
/// * `secrets_file` - Path to encrypted secrets file
/// * `age_key_path` - Path to age private key
///
/// # Errors
/// Returns error if file doesn't exist, decryption fails, or YAML is invalid
pub async fn load_and_decrypt(
    secrets_file: &Path,
    age_key_path: &Path,
) -> Result<Value, Box<dyn std::error::Error>> {
    // Check if file exists
    if !secrets_file.exists() {
        warn!(
            path = ?secrets_file,
            "Secrets file not found, using empty store"
        );
        return Ok(Value::Object(Default::default()));
    }

    // Decrypt using age — resolved via PATH (same reasoning as
    // encrypt_and_persist above: a hardcoded /usr/bin/age doesn't exist on
    // every install, e.g. Homebrew's /opt/homebrew/bin/age).
    let decrypt_output = tokio::process::Command::new("age")
        .arg("-d")
        .arg("-i")
        .arg(age_key_path)
        .arg(secrets_file)
        .output()
        .await
        .map_err(|e| {
            error!("Failed to run age: {}", e);
            e
        })?;

    if !decrypt_output.status.success() {
        let stderr = String::from_utf8_lossy(&decrypt_output.stderr);
        error!("age decryption failed: {}", stderr);
        return Err(format!("age decryption failed: {}", stderr).into());
    }

    // Parse decrypted YAML
    let decrypted_str = String::from_utf8(decrypt_output.stdout)?;
    let secrets_value: Value = serde_yaml::from_str(&decrypted_str)?;

    info!(path = ?secrets_file, "✓ Loaded and decrypted secrets");

    Ok(secrets_value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::NamedTempFile;

    #[tokio::test]
    #[ignore] // Requires age key to be configured
    async fn test_encrypt_and_persist() {
        let test_secrets = json!({
            "test": {
                "key": "value"
            }
        });

        let temp_file = NamedTempFile::new().unwrap();
        let secrets_path = temp_file.path();
        let age_key_path = std::env::var("AMUX_AGE_KEY_PATH")
            .ok()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| "~/.config/sops/age/keys.txt".to_string());
        let age_key_path = PathBuf::from(shellexpand::tilde(&age_key_path).as_ref());

        // Try to encrypt
        match encrypt_and_persist(&test_secrets, secrets_path, &age_key_path).await {
            Ok(_) => {
                // Try to decrypt
                match load_and_decrypt(secrets_path, &age_key_path).await {
                    Ok(loaded) => {
                        assert_eq!(loaded["test"]["key"], "value");
                    }
                    Err(e) => {
                        eprintln!("Decryption failed: {}", e);
                    }
                }
            }
            Err(e) => {
                eprintln!("Encryption failed: {}", e);
            }
        }
    }
}
