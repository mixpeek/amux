//! Browser profile import: discover installed browsers on the local machine
//! and import their cookies into amux's Playwright-based browser profiles.
//!
//! Two endpoints, nested under `/api/browser/import`:
//!
//! - `GET  /discover` — scan for installed browsers and their profiles
//! - `POST /`         — import cookies from a source browser profile into
//!   an amux browser profile
//!
//! Chrome/Chromium cookie decryption on macOS uses the keychain password
//! (via `security find-generic-password`), PBKDF2-SHA1 key derivation, and
//! AES-128-CBC. Firefox cookies are plaintext in `cookies.sqlite`.

use super::AppState;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tracing::warn;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/discover", get(discover))
        .route("/", post(import))
}

// ── Data types ──

#[derive(Serialize, Clone)]
struct BrowserSource {
    id: &'static str,
    name: &'static str,
    family: &'static str,
    profiles: Vec<SourceProfile>,
    cookie_support: &'static str,
}

#[derive(Serialize, Clone)]
struct SourceProfile {
    id: String,
    name: String,
    display_name: String,
    default: bool,
}

struct BrowserDescriptor {
    id: &'static str,
    name: &'static str,
    family: &'static str,
    root: &'static str,
    keychain_services: &'static [&'static str],
}

const BROWSERS: &[BrowserDescriptor] = &[
    BrowserDescriptor {
        id: "chrome",
        name: "Google Chrome",
        family: "chromium",
        root: "Google/Chrome",
        keychain_services: &["Chrome Safe Storage", "Google Chrome Safe Storage"],
    },
    BrowserDescriptor {
        id: "edge",
        name: "Microsoft Edge",
        family: "chromium",
        root: "Microsoft Edge",
        keychain_services: &["Microsoft Edge Safe Storage"],
    },
    BrowserDescriptor {
        id: "brave",
        name: "Brave",
        family: "chromium",
        root: "BraveSoftware/Brave-Browser",
        keychain_services: &["Brave Safe Storage", "Brave Browser Safe Storage"],
    },
    BrowserDescriptor {
        id: "chromium",
        name: "Chromium",
        family: "chromium",
        root: "Chromium",
        keychain_services: &["Chromium Safe Storage"],
    },
    BrowserDescriptor {
        id: "arc",
        name: "Arc",
        family: "chromium",
        root: "Arc/User Data",
        keychain_services: &["Arc Safe Storage"],
    },
    BrowserDescriptor {
        id: "firefox",
        name: "Firefox",
        family: "firefox",
        root: "Firefox",
        keychain_services: &[],
    },
];

fn browser_root(desc: &BrowserDescriptor) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(&home).join("Library/Application Support")
    } else {
        PathBuf::from(&home).join(".config")
    };
    let root = base.join(desc.root);
    if root.is_dir() {
        Some(root)
    } else {
        None
    }
}

fn discover_chromium_profiles(root: &Path) -> Vec<SourceProfile> {
    let mut profiles = Vec::new();

    // Read Local State for profile names
    let local_state_path = root.join("Local State");
    let display_names: std::collections::HashMap<String, String> = (|| {
        let data = std::fs::read_to_string(&local_state_path).ok()?;
        let parsed: Value = serde_json::from_str(&data).ok()?;
        let cache = parsed.get("profile")?.get("info_cache")?.as_object()?;
        Some(
            cache
                .iter()
                .filter_map(|(k, v)| {
                    let name = v.get("name")?.as_str()?;
                    Some((k.clone(), name.to_string()))
                })
                .collect(),
        )
    })()
    .unwrap_or_default();

    // Scan for profile directories
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return profiles,
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "Default" || name.starts_with("Profile ") {
            let dir = entry.path();
            // Must have a Cookies database
            let has_cookies =
                dir.join("Cookies").is_file() || dir.join("Network").join("Cookies").is_file();
            if !has_cookies {
                continue;
            }
            let display = display_names
                .get(&name)
                .cloned()
                .unwrap_or_else(|| name.clone());
            let is_default = name == "Default";
            profiles.push(SourceProfile {
                id: format!("{:x}", md5_hash(dir.to_string_lossy().as_bytes())),
                name: name.clone(),
                display_name: display,
                default: is_default,
            });
        }
    }

    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    profiles
}

fn discover_firefox_profiles(root: &Path) -> Vec<SourceProfile> {
    let mut profiles = Vec::new();
    let profiles_dir = root.join("Profiles");
    let scan_dir = if profiles_dir.is_dir() {
        &profiles_dir
    } else {
        root
    };

    let entries = match std::fs::read_dir(scan_dir) {
        Ok(e) => e,
        Err(_) => return profiles,
    };

    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        if !dir.join("cookies.sqlite").is_file() {
            continue;
        }
        let dirname = entry.file_name().to_string_lossy().to_string();
        // Firefox profile dirs are like "abc123.default-release"
        let display = dirname
            .find('.')
            .map(|i| dirname[i + 1..].to_string())
            .unwrap_or_else(|| dirname.clone());
        let is_default = display.contains("default") || display.contains("release");
        profiles.push(SourceProfile {
            id: format!("{:x}", md5_hash(dir.to_string_lossy().as_bytes())),
            name: dirname.clone(),
            display_name: display,
            default: is_default,
        });
    }

    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    profiles
}

fn md5_hash(data: &[u8]) -> u128 {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(data);
    let bytes: [u8; 16] = hash[..16].try_into().unwrap_or([0; 16]);
    u128::from_be_bytes(bytes)
}

// ── Discover endpoint ──

async fn discover() -> Response {
    let result = tokio::task::spawn_blocking(|| {
        let mut sources = Vec::new();
        for desc in BROWSERS {
            let root = match browser_root(desc) {
                Some(r) => r,
                None => continue,
            };
            let profiles = if desc.family == "chromium" {
                discover_chromium_profiles(&root)
            } else {
                discover_firefox_profiles(&root)
            };
            if profiles.is_empty() {
                continue;
            }
            let cookie_support = if desc.family == "chromium" && !cfg!(target_os = "macos") {
                "partial"
            } else {
                "supported"
            };
            sources.push(BrowserSource {
                id: desc.id,
                name: desc.name,
                family: desc.family,
                profiles,
                cookie_support,
            });
        }
        sources
    })
    .await;

    match result {
        Ok(sources) => Json(json!({ "sources": sources })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        )
            .into_response(),
    }
}

// ── Import endpoint ──

#[derive(Deserialize)]
struct ImportRequest {
    browser_id: String,
    profile_name: String,
    destination: String,
}

#[derive(Serialize)]
struct ImportResult {
    profile: String,
    imported_cookies: usize,
    skipped_cookies: usize,
    warnings: Vec<String>,
}

async fn import(
    State(_state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ImportRequest>,
) -> Response {
    let session = headers
        .get("x-amux-session")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let browser_id = body.browser_id.clone();
    let profile_name = body.profile_name.clone();
    let destination = body.destination.trim().to_string();

    if destination.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "destination profile name is required" })),
        )
            .into_response();
    }

    // Reject path traversal in destination name
    if destination.contains('/') || destination.contains('\\') || destination.contains("..") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid destination name" })),
        )
            .into_response();
    }

    let result = tokio::task::spawn_blocking(move || {
        do_import(&browser_id, &profile_name, &destination, session.as_deref())
    })
    .await;

    match result {
        Ok(Ok(r)) => Json(json!(r)).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        )
            .into_response(),
    }
}

fn do_import(
    browser_id: &str,
    profile_name: &str,
    destination: &str,
    _session: Option<&str>,
) -> Result<ImportResult, String> {
    let desc = BROWSERS
        .iter()
        .find(|b| b.id == browser_id)
        .ok_or_else(|| format!("unknown browser: {browser_id}"))?;

    let root =
        browser_root(desc).ok_or_else(|| format!("{} not found on this machine", desc.name))?;

    let profile_dir = if desc.family == "chromium" {
        let dir = root.join(profile_name);
        if !dir.is_dir() {
            return Err(format!("profile directory not found: {profile_name}"));
        }
        dir
    } else {
        let profiles_dir = root.join("Profiles");
        let dir = if profiles_dir.is_dir() {
            profiles_dir.join(profile_name)
        } else {
            root.join(profile_name)
        };
        if !dir.is_dir() {
            return Err(format!("profile directory not found: {profile_name}"));
        }
        dir
    };

    // Containment check: profile must be under the browser root
    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("cannot canonicalize root: {e}"))?;
    let canonical_profile = profile_dir
        .canonicalize()
        .map_err(|e| format!("cannot canonicalize profile: {e}"))?;
    if !canonical_profile.starts_with(&canonical_root) {
        return Err("profile path escapes browser root".to_string());
    }

    let cookies = if desc.family == "chromium" {
        read_chromium_cookies(&profile_dir, desc)?
    } else {
        read_firefox_cookies(&profile_dir)?
    };

    let imported_count = cookies.len();
    let mut warnings = Vec::new();

    // Write cookies into the amux Playwright profile
    let home = crate::config::amux_home();
    let dest_dir = home
        .join("playwright-auth")
        .join("profiles")
        .join(destination);
    std::fs::create_dir_all(&dest_dir)
        .map_err(|e| format!("cannot create destination profile: {e}"))?;

    // Write cookies as a Playwright-compatible cookies.json
    let cookies_file = dest_dir.join("cookies.json");
    let json_cookies: Vec<Value> = cookies
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "value": c.value,
                "domain": c.domain,
                "path": c.path,
                "expires": c.expires,
                "httpOnly": c.http_only,
                "secure": c.secure,
                "sameSite": c.same_site,
            })
        })
        .collect();

    let skipped = 0usize;

    std::fs::write(
        &cookies_file,
        serde_json::to_string_pretty(&json_cookies)
            .map_err(|e| format!("JSON serialization failed: {e}"))?,
    )
    .map_err(|e| format!("cannot write cookies.json: {e}"))?;

    // Also write a Playwright state.json that Playwright's storageState expects
    let state_file = dest_dir.join("state.json");
    let state = json!({
        "cookies": json_cookies,
        "origins": [],
    });
    std::fs::write(
        &state_file,
        serde_json::to_string_pretty(&state).map_err(|e| format!("state JSON failed: {e}"))?,
    )
    .map_err(|e| format!("cannot write state.json: {e}"))?;

    if imported_count == 0 {
        warnings.push("no cookies found in the source profile".to_string());
    }

    tracing::info!(
        browser = browser_id,
        profile = profile_name,
        destination,
        imported_count,
        "browser profile import complete"
    );

    Ok(ImportResult {
        profile: destination.to_string(),
        imported_cookies: imported_count,
        skipped_cookies: skipped,
        warnings,
    })
}

// ── Cookie structures ──

struct ImportedCookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    expires: f64,
    http_only: bool,
    secure: bool,
    same_site: String,
}

// ── Chromium cookie reading ──

fn read_chromium_cookies(
    profile_dir: &Path,
    desc: &BrowserDescriptor,
) -> Result<Vec<ImportedCookie>, String> {
    // Find the Cookies database (moved to Network/ subdirectory in newer Chrome)
    let cookies_db = {
        let network = profile_dir.join("Network").join("Cookies");
        if network.is_file() {
            network
        } else {
            let direct = profile_dir.join("Cookies");
            if direct.is_file() {
                direct
            } else {
                return Err("no Cookies database found in profile".to_string());
            }
        }
    };

    // Copy to a temp file to avoid locking the live database
    let tmp =
        tempfile::NamedTempFile::new().map_err(|e| format!("cannot create temp file: {e}"))?;
    std::fs::copy(&cookies_db, tmp.path())
        .map_err(|e| format!("cannot copy Cookies database: {e}"))?;
    // Also copy WAL/SHM if present (for consistency)
    for ext in &["-wal", "-shm"] {
        let src = cookies_db.with_extension(
            cookies_db
                .extension()
                .map(|e| format!("{}{ext}", e.to_string_lossy()))
                .unwrap_or_else(|| ext[1..].to_string()),
        );
        if src.is_file() {
            let dst_name = format!("{}{}", tmp.path().to_string_lossy(), ext);
            let _ = std::fs::copy(&src, &dst_name);
        }
    }

    // Get decryption key on macOS
    let decrypt_key = if cfg!(target_os = "macos") {
        get_chromium_key_macos(desc).ok()
    } else {
        None
    };

    let conn = rusqlite::Connection::open_with_flags(
        tmp.path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("cannot open Cookies DB: {e}"))?;

    let _ = conn.execute_batch("PRAGMA query_only = ON;");

    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as f64;

    let mut stmt = conn
        .prepare(
            "SELECT host_key, name, value, encrypted_value, path, \
             expires_utc, is_secure, is_httponly, samesite, \
             COALESCE(top_frame_site_key, '') as top_frame \
             FROM cookies \
             ORDER BY last_access_utc DESC \
             LIMIT 50000",
        )
        .map_err(|e| format!("SQL prepare failed: {e}"))?;

    let mut cookies = Vec::new();
    let mut skipped_encrypted = 0usize;
    let mut skipped_partitioned = 0usize;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,  // host_key
                row.get::<_, String>(1)?,  // name
                row.get::<_, String>(2)?,  // value (plaintext, often empty in newer Chrome)
                row.get::<_, Vec<u8>>(3)?, // encrypted_value
                row.get::<_, String>(4)?,  // path
                row.get::<_, i64>(5)?,     // expires_utc (Chromium epoch)
                row.get::<_, bool>(6)?,    // is_secure
                row.get::<_, bool>(7)?,    // is_httponly
                row.get::<_, i32>(8)?,     // samesite
                row.get::<_, String>(9)?,  // top_frame_site_key
            ))
        })
        .map_err(|e| format!("SQL query failed: {e}"))?;

    for row in rows {
        let (
            host,
            name,
            plaintext_value,
            encrypted_value,
            path,
            expires_utc,
            secure,
            http_only,
            samesite,
            top_frame,
        ) = match row {
            Ok(r) => r,
            Err(_) => continue,
        };

        // Skip partitioned cookies
        if !top_frame.is_empty() {
            skipped_partitioned += 1;
            continue;
        }

        // Chromium timestamps: microseconds since 1601-01-01
        let expires_unix = if expires_utc == 0 {
            0.0
        } else {
            (expires_utc as f64 / 1_000_000.0) - 11_644_473_600.0
        };

        // Skip expired cookies
        if expires_unix > 0.0 && expires_unix < now_unix {
            continue;
        }

        // Decrypt value
        let value = if !plaintext_value.is_empty() {
            plaintext_value
        } else if !encrypted_value.is_empty() {
            match decrypt_chromium_cookie(&encrypted_value, decrypt_key.as_deref(), &host) {
                Some(v) => v,
                None => {
                    skipped_encrypted += 1;
                    continue;
                }
            }
        } else {
            String::new()
        };

        let same_site = match samesite {
            0 => "None",
            1 => "Lax",
            2 => "Strict",
            _ => "Lax",
        };

        cookies.push(ImportedCookie {
            name,
            value,
            domain: host,
            path,
            expires: expires_unix,
            http_only,
            secure,
            same_site: same_site.to_string(),
        });
    }

    if skipped_encrypted > 0 {
        warn!(
            count = skipped_encrypted,
            "skipped cookies that could not be decrypted"
        );
    }
    if skipped_partitioned > 0 {
        tracing::debug!(count = skipped_partitioned, "skipped partitioned cookies");
    }

    Ok(cookies)
}

/// Retrieve the Chromium Safe Storage password from the macOS Keychain.
fn get_chromium_key_macos(desc: &BrowserDescriptor) -> Result<Vec<u8>, String> {
    for service in desc.keychain_services {
        let output = std::process::Command::new("security")
            .args(["find-generic-password", "-w", "-s", service])
            .output();

        match output {
            Ok(o) if o.status.success() => {
                let password = String::from_utf8_lossy(&o.stdout).trim().to_string();
                return derive_chromium_key(password.as_bytes());
            }
            _ => continue,
        }
    }
    Err("could not retrieve keychain password for any known service name".to_string())
}

/// PBKDF2-SHA1 key derivation for Chrome cookie decryption on macOS.
/// Salt: "saltysalt", iterations: 1003, key length: 16 bytes.
fn derive_chromium_key(password: &[u8]) -> Result<Vec<u8>, String> {
    use hmac::Hmac;
    use pbkdf2::pbkdf2;
    use sha1::Sha1;

    let salt = b"saltysalt";
    let mut key = [0u8; 16];
    pbkdf2::<Hmac<Sha1>>(password, salt, 1003, &mut key)
        .map_err(|e| format!("PBKDF2 failed: {e}"))?;
    Ok(key.to_vec())
}

/// Decrypt a single Chromium cookie value using AES-128-CBC.
/// IV: 16 bytes of 0x20 (space). Prefix: "v10" or "v11" (3 bytes, stripped).
fn decrypt_chromium_cookie(encrypted: &[u8], key: Option<&[u8]>, host: &str) -> Option<String> {
    let key = key?;

    // Must start with v10 or v11
    if encrypted.len() < 4 {
        return None;
    }
    let prefix = &encrypted[..3];
    if prefix != b"v10" && prefix != b"v11" {
        return None;
    }
    let ciphertext = &encrypted[3..];
    if ciphertext.is_empty() {
        return None;
    }

    use aes::cipher::{BlockDecryptMut, KeyIvInit};

    type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

    let iv = [0x20u8; 16];
    let mut buf = ciphertext.to_vec();

    let decrypted = Aes128CbcDec::new_from_slices(key, &iv)
        .ok()?
        .decrypt_padded_mut::<aes::cipher::block_padding::Pkcs7>(&mut buf)
        .ok()?;

    let plaintext = decrypted.to_vec();

    // Chromium v24+: plaintext may be prefixed with SHA-256(host_key).
    // If the first 32 bytes match the hash, strip them.
    if plaintext.len() >= 32 {
        use sha2::{Digest, Sha256};
        let host_hash = Sha256::digest(host.as_bytes());
        if plaintext[..32] == host_hash[..] {
            return String::from_utf8(plaintext[32..].to_vec()).ok();
        }
    }

    String::from_utf8(plaintext).ok()
}

// ── Firefox cookie reading ──

fn read_firefox_cookies(profile_dir: &Path) -> Result<Vec<ImportedCookie>, String> {
    let cookies_db = profile_dir.join("cookies.sqlite");
    if !cookies_db.is_file() {
        return Err("cookies.sqlite not found".to_string());
    }

    // Copy to temp to avoid locking the live browser's DB
    let tmp =
        tempfile::NamedTempFile::new().map_err(|e| format!("cannot create temp file: {e}"))?;
    std::fs::copy(&cookies_db, tmp.path())
        .map_err(|e| format!("cannot copy cookies.sqlite: {e}"))?;

    let conn = rusqlite::Connection::open_with_flags(
        tmp.path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("cannot open cookies.sqlite: {e}"))?;

    let _ = conn.execute_batch("PRAGMA query_only = ON;");

    // Check for originAttributes column (container-tab support)
    let has_origin_attrs = conn
        .prepare("SELECT originAttributes FROM moz_cookies LIMIT 0")
        .is_ok();
    // Check for sameSite column
    let has_samesite = conn
        .prepare("SELECT sameSite FROM moz_cookies LIMIT 0")
        .is_ok();

    let origin_filter = if has_origin_attrs {
        "WHERE COALESCE(originAttributes, '') = ''"
    } else {
        ""
    };

    let samesite_col = if has_samesite { "sameSite" } else { "-1" };

    let sql = format!(
        "SELECT host, name, value, path, expiry, isSecure, isHttpOnly, {samesite_col} \
         FROM moz_cookies {origin_filter} \
         ORDER BY rowid DESC \
         LIMIT 50000"
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("SQL prepare failed: {e}"))?;

    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as f64;

    let mut cookies = Vec::new();

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?, // host
                row.get::<_, String>(1)?, // name
                row.get::<_, String>(2)?, // value
                row.get::<_, String>(3)?, // path
                row.get::<_, i64>(4)?,    // expiry (unix seconds)
                row.get::<_, bool>(5)?,   // isSecure
                row.get::<_, bool>(6)?,   // isHttpOnly
                row.get::<_, i32>(7)?,    // sameSite
            ))
        })
        .map_err(|e| format!("SQL query failed: {e}"))?;

    for row in rows {
        let (host, name, value, path, expiry, secure, http_only, samesite) = match row {
            Ok(r) => r,
            Err(_) => continue,
        };

        let expires = expiry as f64;

        // Skip expired
        if expires > 0.0 && expires < now_unix {
            continue;
        }

        let same_site = match samesite {
            0 => "None",
            1 => "Lax",
            2 => "Strict",
            _ => "Lax",
        };

        cookies.push(ImportedCookie {
            name,
            value,
            domain: host,
            path,
            expires,
            http_only,
            secure,
            same_site: same_site.to_string(),
        });
    }

    Ok(cookies)
}
