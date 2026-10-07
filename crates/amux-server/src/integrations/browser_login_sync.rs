//! Keep amux browser profiles signed in by mirroring the owner's real Chrome
//! (2026-10-07).
//!
//! # Why
//!
//! Logins were saved by hand (a CDP capture of one tab) and then decayed:
//! session cookies were dropped when the profile's browser closed, so 9 of 25
//! "Logins for X" profiles held nothing, and the rest expired on their own
//! clock. Ethan: "they should always have the logins active if they're saved",
//! and it must be automatic within the harness.
//!
//! # How
//!
//! A registry entry (`playwright-auth/profiles.json`) opts in with
//!
//! ```json
//! "ethan": { "sync": { "from": "chrome", "chrome_identity": "ethan@mixpeek.com",
//!                      "sites": ["github.com", "cloudflare.com", ...] } }
//! ```
//!
//! On every pass, for each such profile:
//!
//! 1. The source is the owner's Chrome profile signed in as `chrome_identity`
//!    (Chrome's `Local State` names the account per profile), or the profile
//!    directory named by `chrome_profile`.
//! 2. Its cookie DB is read from a copy (with the WAL), never in place.
//! 3. For each listed site, the target's rows are REPLACED by the source's: a
//!    login present in Chrome is copied, and one the owner signed out of is
//!    removed, so the profile mirrors Chrome rather than accumulating.
//! 4. SESSION cookies become persistent for `SESSION_TTL_DAYS`. Chrome drops
//!    session cookies when a profile's browser closes, which is what emptied
//!    the hand-saved profiles: GitHub kept 3 of 14, Cloudflare 3 of 43.
//!
//! Encrypted values are copied as-is: every Google Chrome profile on this host
//! (the owner's and amux's, which launches the same binary with no mock
//! keychain) encrypts with the one "Chrome Safe Storage" key, and Chrome binds
//! a value to its host_key, which is copied unchanged. The same fact
//! `/profile/combine` relies on.
//!
//! ONLY LISTED SITES ARE EVER COPIED. The owner's Chrome holds banking,
//! insurance and health logins; nothing reaches an agent-usable profile unless
//! a human named the site.
//!
//! A profile whose browser is running is skipped (its DB is live) and retried
//! on the next pass.

use crate::integrations::browser_logins as bl;
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const SESSION_TTL_DAYS: i64 = 30;
const CHROME_EPOCH_OFFSET_S: i64 = 11_644_473_600;

#[derive(Debug, Clone, PartialEq)]
pub struct SyncSpec {
    pub profile: String,
    /// Registry role; `restricted` and `personal` profiles only receive
    /// logins once access to them is narrowed (see `held_for_scope`).
    pub role: String,
    pub chrome_identity: Option<String>,
    pub chrome_profile: Option<String>,
    pub sites: Vec<String>,
}

/// Every registry entry that opts in.
pub fn specs_from_registry(reg: &serde_json::Map<String, serde_json::Value>) -> Vec<SyncSpec> {
    let mut out: Vec<SyncSpec> = reg
        .iter()
        .filter_map(|(name, e)| {
            let s = e.get("sync")?;
            if s.get("from").and_then(|v| v.as_str()) != Some("chrome") {
                return None;
            }
            let sites: Vec<String> = s
                .get("sites")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str()).filter_map(bl::site_of).collect())
                .unwrap_or_default();
            Some(SyncSpec {
                profile: name.clone(),
                role: e.get("role").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                chrome_identity: s.get("chrome_identity").and_then(|v| v.as_str()).map(str::to_string),
                chrome_profile: s.get("chrome_profile").and_then(|v| v.as_str()).map(str::to_string),
                sites,
            })
        })
        .collect();
    out.sort_by(|a, b| a.profile.cmp(&b.profile));
    out
}

/// A RESTRICTED OR PERSONAL PROFILE IS NOT FILLED WHILE EVERY WORKER CAN USE
/// IT. Its logins (Brex, Stripe, personal accounts) would otherwise reach the
/// whole fleet between the profile being declared and the owner narrowing
/// access, which only the owner may do. Returns why it is held, or None.
pub fn held_for_scope(role: &str, usable_by_every_worker: bool) -> Option<String> {
    (matches!(role, "restricted" | "personal") && usable_by_every_worker).then(|| {
        format!(
            "role '{role}' but every worker may use it: its logins are not copied until access is \
             narrowed (set AMUX_BROWSER_PROFILES_DENY to include it globally, and ALLOW it for the \
             lanes that need it, from the dashboard Scope tab)"
        )
    })
}

/// The owner's Chrome profile directory signed in as `identity`, from
/// Chrome's `Local State` (profile.info_cache.<dir>.user_name).
pub fn chrome_profile_for_identity(chrome_dir: &Path, identity: &str) -> Option<String> {
    let raw = std::fs::read_to_string(chrome_dir.join("Local State")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let cache = v.get("profile")?.get("info_cache")?.as_object()?;
    let want = identity.trim().to_ascii_lowercase();
    let mut hits: Vec<(&String, i64)> = cache
        .iter()
        .filter(|(_, e)| e.get("user_name").and_then(|u| u.as_str()).map(|u| u.to_ascii_lowercase()) == Some(want.clone()))
        .map(|(k, e)| (k, e.get("active_time").and_then(|t| t.as_f64()).unwrap_or(0.0) as i64))
        .collect();
    // Two profiles signed in as one account: the most recently used is the
    // one the owner actually works in.
    hits.sort_by_key(|h| std::cmp::Reverse(h.1));
    hits.first().map(|(k, _)| k.to_string())
}

/// Copy a live SQLite DB with its WAL into `dir`, returning the copy's path.
fn snapshot_db(src: &Path, dir: &Path) -> std::io::Result<PathBuf> {
    let dst = dir.join("Cookies");
    std::fs::copy(src, &dst)?;
    for suf in ["-wal", "-shm"] {
        let s = PathBuf::from(format!("{}{suf}", src.display()));
        if s.is_file() {
            std::fs::copy(&s, dir.join(format!("Cookies{suf}")))?;
        }
    }
    Ok(dst)
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ProfileSyncReport {
    pub profile: String,
    pub source: Option<String>,
    pub copied: usize,
    pub removed: usize,
    pub sessions_persisted: usize,
    /// Listed sites with a live login in the source after the copy.
    pub signed_in: Vec<String>,
    /// Listed sites the owner is NOT signed in to in Chrome: the profile will
    /// not be either, and only a sign-in in Chrome fixes it.
    pub not_signed_in: Vec<String>,
    pub skipped: Option<String>,
}

/// Mirror `sites` from the source cookie DB into the target cookie DB.
/// Both are paths to files this function may write (the target is the real
/// profile DB, the source a snapshot). Pure over the two databases, so the
/// tests drive it with fixtures.
pub fn mirror_sites(source_db: &Path, target_db: &Path, sites: &[String], now: i64) -> rusqlite::Result<(usize, usize, usize)> {
    let src = rusqlite::Connection::open(source_db)?;
    let tgt = rusqlite::Connection::open(target_db)?;
    // The target may be a fresh profile with no cookies table yet: create it
    // with the source's own DDL (same Chrome binary, same schema).
    let has_table: bool = tgt
        .query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='cookies'", [], |r| r.get::<_, i64>(0))
        .map(|n| n > 0)?;
    if !has_table {
        let ddl: Vec<String> = src
            .prepare("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name IN ('cookies','meta') OR (type='index' AND tbl_name='cookies' AND sql IS NOT NULL)")?
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(Result::ok)
            .collect();
        for d in &ddl {
            tgt.execute_batch(d)?;
        }
        if ddl.iter().any(|d| d.contains("CREATE TABLE meta")) {
            let meta: Vec<(String, String)> = src
                .prepare("SELECT key, value FROM meta")?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .filter_map(Result::ok)
                .collect();
            for (k, v) in meta {
                tgt.execute("INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)", rusqlite::params![k, v])?;
            }
        }
    }
    let cols: Vec<String> = src
        .prepare("SELECT name FROM pragma_table_info('cookies')")?
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok)
        .collect();
    let tcols: Vec<String> = tgt
        .prepare("SELECT name FROM pragma_table_info('cookies')")?
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok)
        .collect();
    let shared: Vec<String> = cols.iter().filter(|c| tcols.contains(c)).cloned().collect();
    let idx = |c: &str| shared.iter().position(|x| x == c);
    let (Some(i_host), Some(i_pers), Some(i_exp)) = (idx("host_key"), idx("is_persistent"), idx("expires_utc")) else {
        return Ok((0, 0, 0));
    };
    let i_has = idx("has_expires");
    let in_scope = |host: &str| bl::site_of(host).is_some_and(|s| sites.iter().any(|w| w == &s));
    let collist = shared.join(", ");
    let mut stmt = src.prepare(&format!("SELECT {collist} FROM cookies"))?;
    let mut rows: Vec<Vec<rusqlite::types::Value>> = Vec::new();
    let mut q = stmt.query([])?;
    while let Some(r) = q.next()? {
        let mut v = Vec::with_capacity(shared.len());
        for i in 0..shared.len() {
            v.push(r.get::<_, rusqlite::types::Value>(i)?);
        }
        if let rusqlite::types::Value::Text(h) = &v[i_host] {
            if in_scope(h) {
                rows.push(v);
            }
        }
    }
    drop(q);
    let expiry = (now + SESSION_TTL_DAYS * 86_400 + CHROME_EPOCH_OFFSET_S) * 1_000_000;
    let tx = tgt.unchecked_transaction()?;
    // Mirror: drop the target's rows for these sites first, so a logout in
    // Chrome reaches the profile instead of a stale session lingering.
    let existing: Vec<(i64, String, String, String)> = tx
        .prepare("SELECT rowid, host_key, name, path FROM cookies")?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?
        .filter_map(Result::ok)
        .collect();
    let mut before: std::collections::HashSet<(String, String, String)> = std::collections::HashSet::new();
    for (rowid, host, name, path) in existing {
        if in_scope(&host) {
            tx.execute("DELETE FROM cookies WHERE rowid = ?1", [rowid])?;
            before.insert((host, name, path));
        }
    }
    let placeholders = vec!["?"; shared.len()].join(", ");
    let mut persisted = 0;
    let mut copied = 0;
    {
        let mut ins = tx.prepare(&format!("INSERT OR REPLACE INTO cookies ({collist}) VALUES ({placeholders})"))?;
        for mut v in rows {
            let session = matches!(v[i_pers], rusqlite::types::Value::Integer(0));
            if session {
                v[i_pers] = rusqlite::types::Value::Integer(1);
                v[i_exp] = rusqlite::types::Value::Integer(expiry);
                if let Some(h) = i_has {
                    v[h] = rusqlite::types::Value::Integer(1);
                }
                persisted += 1;
            }
            copied += ins.execute(rusqlite::params_from_iter(v.iter()))?;
        }
    }
    tx.commit()?;
    // REMOVED = cookies the profile had for these sites that the owner's
    // Chrome no longer has (a sign-out), by identity, not by count.
    let after: std::collections::HashSet<(String, String, String)> = tgt
        .prepare("SELECT host_key, name, path FROM cookies")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
        .filter_map(Result::ok)
        .collect();
    let removed = before.difference(&after).count();
    Ok((copied, removed, persisted))
}

/// One profile, end to end. `running` says whether its browser is up.
pub fn sync_profile(home: &Path, chrome_dir: &Path, spec: &SyncSpec, running: bool, now: i64) -> ProfileSyncReport {
    let mut rep = ProfileSyncReport { profile: spec.profile.clone(), ..Default::default() };
    if spec.sites.is_empty() {
        rep.skipped = Some("no sites listed: nothing is copied without a named site".into());
        return rep;
    }
    let source = spec
        .chrome_profile
        .clone()
        .or_else(|| spec.chrome_identity.as_deref().and_then(|i| chrome_profile_for_identity(chrome_dir, i)));
    let Some(source) = source else {
        rep.skipped = Some(format!(
            "no Chrome profile is signed in as {}",
            spec.chrome_identity.as_deref().unwrap_or("(no chrome_identity)")
        ));
        return rep;
    };
    rep.source = Some(source.clone());
    if running {
        rep.skipped = Some("its browser is running (live cookie DB); retried next pass".into());
        return rep;
    }
    let src_db = chrome_dir.join(&source).join("Cookies");
    if !src_db.is_file() {
        rep.skipped = Some(format!("Chrome profile {source} has no cookie DB"));
        return rep;
    }
    let target_dir = home.join("playwright-auth").join("profiles").join(&spec.profile).join("Default");
    if let Err(e) = std::fs::create_dir_all(&target_dir) {
        rep.skipped = Some(format!("cannot create the profile: {e}"));
        return rep;
    }
    let tmp = match tempfile::tempdir() {
        Ok(t) => t,
        Err(e) => {
            rep.skipped = Some(format!("no temp dir: {e}"));
            return rep;
        }
    };
    let snap = match snapshot_db(&src_db, tmp.path()) {
        Ok(p) => p,
        Err(e) => {
            rep.skipped = Some(format!("could not read Chrome's cookie DB: {e}"));
            return rep;
        }
    };
    match mirror_sites(&snap, &target_dir.join("Cookies"), &spec.sites, now) {
        Ok((c, r, p)) => {
            rep.copied = c;
            rep.removed = r;
            rep.sessions_persisted = p;
        }
        Err(e) => {
            rep.skipped = Some(format!("cookie copy failed: {e}"));
            return rep;
        }
    }
    let jar = bl::read_rows(&snap).map(|r| bl::summarize(&r, now)).unwrap_or_default();
    for s in &spec.sites {
        if jar.login_for(s).is_some() {
            rep.signed_in.push(s.clone());
        } else {
            rep.not_signed_in.push(s.clone());
        }
    }
    rep
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jar(path: &Path, rows: &[(&str, &str, i64, i64)]) {
        let c = rusqlite::Connection::open(path).unwrap();
        c.execute_batch(
            "CREATE TABLE meta(key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR);
             INSERT INTO meta VALUES ('version','24');
             CREATE TABLE cookies(creation_utc INTEGER NOT NULL, host_key TEXT NOT NULL, top_frame_site_key TEXT NOT NULL DEFAULT '',
               name TEXT NOT NULL, value TEXT NOT NULL, encrypted_value BLOB NOT NULL DEFAULT x'', path TEXT NOT NULL,
               expires_utc INTEGER NOT NULL, is_secure INTEGER NOT NULL, is_httponly INTEGER NOT NULL,
               has_expires INTEGER NOT NULL DEFAULT 1, is_persistent INTEGER NOT NULL DEFAULT 1,
               UNIQUE (host_key, top_frame_site_key, name, path));",
        )
        .unwrap();
        for (i, (host, name, exp, pers)) in rows.iter().enumerate() {
            c.execute(
                "INSERT INTO cookies(creation_utc, host_key, name, value, encrypted_value, path, expires_utc, is_secure, is_httponly, has_expires, is_persistent)
                 VALUES (?1, ?2, ?3, '', x'763130', '/', ?4, 1, 1, ?5, ?5)",
                rusqlite::params![i as i64, host, name, exp, pers],
            )
            .unwrap();
        }
    }

    fn hosts_names(path: &Path) -> Vec<(String, String, i64)> {
        let c = rusqlite::Connection::open(path).unwrap();
        let mut v: Vec<(String, String, i64)> = c
            .prepare("SELECT host_key, name, is_persistent FROM cookies ORDER BY host_key, name").unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap()
            .map(Result::unwrap).collect();
        v.sort();
        v
    }

    #[test]
    fn only_listed_sites_are_copied_sessions_persist_and_logouts_propagate() {
        let t = tempfile::tempdir().unwrap();
        let now = 1_800_000_000i64;
        let future = (now + 86_400 * 14 + CHROME_EPOCH_OFFSET_S) * 1_000_000;
        let src = t.path().join("src");
        jar(&src, &[
            (".github.com", "user_session", future, 1),
            (".github.com", "_gh_sess", 0, 0),               // session cookie
            (".capitalone.com", "session", future, 1),       // never listed: must not move
            ("dash.cloudflare.com", "CF_Authorization", future, 1),
        ]);
        // Target: a fresh profile with no DB at all.
        let tgt = t.path().join("tgt");
        let (copied, removed, persisted) =
            mirror_sites(&src, &tgt, &["github.com".into(), "cloudflare.com".into()], now).unwrap();
        assert_eq!((copied, removed, persisted), (3, 0, 1));
        let got = hosts_names(&tgt);
        assert!(got.iter().all(|(h, _, p)| !h.contains("capitalone") && *p == 1), "{got:?}");
        assert_eq!(got.len(), 3);

        // The owner signs out of GitHub in Chrome: the next pass removes it.
        let src2 = t.path().join("src2");
        jar(&src2, &[("dash.cloudflare.com", "CF_Authorization", future, 1)]);
        let (copied, removed, _) =
            mirror_sites(&src2, &tgt, &["github.com".into(), "cloudflare.com".into()], now).unwrap();
        assert_eq!(copied, 1);
        assert_eq!(removed, 2, "both GitHub cookies are gone from the profile");
        assert_eq!(hosts_names(&tgt), vec![("dash.cloudflare.com".into(), "CF_Authorization".into(), 1)]);
    }

    #[test]
    fn a_restricted_profile_waits_for_its_scope() {
        assert!(held_for_scope("restricted", true).is_some());
        assert!(held_for_scope("personal", true).is_some());
        assert!(held_for_scope("restricted", false).is_none(), "narrowed: filled");
        assert!(held_for_scope("primary", true).is_none(), "the primary identity is for everyone");
    }

    #[test]
    fn specs_come_only_from_entries_that_opt_in() {
        let reg: serde_json::Map<String, serde_json::Value> = serde_json::from_value(serde_json::json!({
            "ethan": {"sync": {"from": "chrome", "chrome_identity": "ethan@mixpeek.com", "sites": ["dash.cloudflare.com", "github.com"]}},
            "old": {"domains": ["x.com"]},
            "weird": {"sync": {"from": "somewhere-else", "sites": ["a.com"]}},
        })).unwrap();
        let s = specs_from_registry(&reg);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].sites, vec!["cloudflare.com", "github.com"], "hosts are normalised to sites");
    }

    #[test]
    fn the_source_profile_is_found_by_the_signed_in_account() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("Local State"), serde_json::json!({"profile": {"info_cache": {
            "Profile 11": {"user_name": "esteininger21@gmail.com", "active_time": 5.0},
            "Profile 14": {"user_name": "ethan@mixpeek.com", "active_time": 9.0},
            "Profile 8": {"user_name": "Ethan@Mixpeek.com", "active_time": 1.0},
        }}}).to_string()).unwrap();
        assert_eq!(chrome_profile_for_identity(t.path(), "ethan@mixpeek.com").as_deref(), Some("Profile 14"),
            "the most recently used of two matching profiles");
        assert_eq!(chrome_profile_for_identity(t.path(), "nobody@x.com"), None);
    }
}
