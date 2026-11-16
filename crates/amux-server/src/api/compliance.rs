//! Compliance reads (governance PR 21): two views the compliance skill
//! (`skills/compliance.md`) needs and no existing route gives in one answer.
//!
//! - `GET /api/compliance/access-review`: who can do what. People (owner,
//!   members, admins, auditors) with role, scope, second factor and when the
//!   ledger last saw them; workers with their effective capabilities, sponsor
//!   and last activity; published apps; SCIM tokens; standing approvals; and the
//!   identity policy (MFA, SSO). `findings` lists what looks stale or
//!   over-broad, each with the rule that flagged it. It recommends; the owner
//!   decides. Nothing here changes anything.
//! - `GET /api/compliance/timeline`: one chronological record for a trace id,
//!   a worker, an actor or a time window, merged from the ledger, the trace
//!   tree and the commits that carry amux's trailers, with the chain's verify
//!   result beside it.
//!
//! The evidence pack is assembled by `amux compliance evidence` from these and
//! the existing routes (ledger verify, checkpoints, audit export, at-rest,
//! kill switch, holds), so nothing is computed twice.
//!
//! Readable by the owner, an auditor or an admin (governance::can_read), and
//! listed in org::auditor_may. Every section says whether it was measured
//! (A5): a section that could not be read reads `measured: false` with the
//! reason, never an empty list.

use super::AppState;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const DAY_MS: i64 = 86_400_000;
const DEFAULT_STALE_DAYS: i64 = 90;
const MAX_TIMELINE: i64 = 5000;
const MAX_REPOS: usize = 40;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/compliance/access-review", get(access_review_route))
        .route("/api/compliance/timeline", get(timeline_route))
}

pub(crate) mod counters {
    use super::*;
    pub static ACCESS_REVIEWS: AtomicU64 = AtomicU64::new(0);
    pub static TIMELINES: AtomicU64 = AtomicU64::new(0);
    pub static REFUSED: AtomicU64 = AtomicU64::new(0);
    pub fn bump(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

fn refuse() -> Response {
    counters::bump(&counters::REFUSED);
    (
        StatusCode::FORBIDDEN,
        Json(json!({"ok": false, "code": "compliance_read_refused",
            "error": "the owner, an auditor or an admin reads compliance views"})),
    )
        .into_response()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1", [name], |_| Ok(()))
        .optional()
        .ok()
        .flatten()
        .is_some()
}

/// The newest ledger row this actor wrote or was claimed by, epoch ms.
fn last_seen(conn: &Connection, actor: &str) -> Option<i64> {
    conn.query_row(
        "SELECT MAX(ts) FROM _amux_ledger WHERE actor = ?1 OR claimed = ?1",
        [actor],
        |r| r.get::<_, Option<i64>>(0),
    )
    .ok()
    .flatten()
}

/// What a "never seen" can mean: the ledger only remembers from its oldest
/// row, so an actor quiet for longer than that reads as never seen.
fn ledger_basis(conn: Option<&Connection>) -> Value {
    let Some(c) = conn.filter(|c| table_exists(c, "_amux_ledger")) else {
        return json!({"measured": false, "why_unmeasured": "the ledger is not readable here"});
    };
    let (oldest, rows): (Option<i64>, i64) = c
        .query_row("SELECT MIN(ts), COUNT(*) FROM _amux_ledger", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap_or((None, 0));
    json!({
        "measured": true,
        "rows": rows,
        "oldest_ts": oldest,
        "oldest_at": oldest.and_then(chrono::DateTime::from_timestamp_millis).map(|t| t.to_rfc3339()),
        "note": "last_seen comes from the ledger; an actor quiet since before oldest_at reads as never seen",
    })
}

fn days_since(now: i64, ts: Option<i64>) -> Option<i64> {
    ts.map(|t| (now - t).max(0) / DAY_MS)
}

/// Stale: not seen within the window. `None` (never seen) counts as stale only
/// when the subject is older than the window, so a person added yesterday is
/// not flagged for not having acted yet.
fn stale(now: i64, seen: Option<i64>, since: Option<i64>, window_days: i64) -> bool {
    let window = window_days * DAY_MS;
    match seen {
        Some(t) => now - t > window,
        None => since.map(|s| now - s > window).unwrap_or(true),
    }
}

fn finding(subject: &str, rule: &str, detail: String, recommend: &str) -> Value {
    json!({"subject": subject, "rule": rule, "detail": detail, "recommend": recommend})
}

fn worker_names(home: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(home.join("sessions")) else { return vec![] };
    let mut v: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            (p.extension().and_then(|x| x.to_str()) == Some("env"))
                .then(|| p.file_stem().and_then(|s| s.to_str()).map(String::from))
                .flatten()
        })
        .collect();
    v.sort();
    v
}

fn env_flag(home: &Path, worker: &str, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(home.join("sessions").join(format!("{worker}.env"))).ok()?;
    text.lines().rev().find_map(|l| {
        let l = l.trim().strip_prefix("export ").unwrap_or(l.trim());
        l.strip_prefix(&format!("{key}=")).map(|v| v.trim_matches('"').trim_matches('\'').to_string())
    })
}

fn mfa_enrolled(conn: &Connection, subject: &str) -> Option<bool> {
    if !table_exists(conn, "mfa_factors") {
        return None;
    }
    super::mfa::enrolled(conn, subject).ok()
}

fn people(conn: Option<&Connection>, now: i64, window: i64, findings: &mut Vec<Value>) -> Value {
    let Some(c) = conn else {
        return json!({"measured": false, "why_unmeasured": "the database is not readable", "items": []});
    };
    let ledger = table_exists(c, "_amux_ledger");
    let mfa_on = super::mfa::policy() != super::mfa::Policy::Off;
    let mut items = vec![];
    let owner_seen = ledger.then(|| last_seen(c, "owner")).flatten();
    let owner_mfa = mfa_enrolled(c, "owner");
    items.push(json!({"subject": "owner", "role": "owner", "scope": "everything",
        "mfa_enrolled": owner_mfa, "last_seen": owner_seen, "days_since_seen": days_since(now, owner_seen)}));
    if mfa_on && owner_mfa == Some(false) {
        findings.push(finding("owner", "owner_without_second_factor",
            "the owner holds every power and has no second factor enrolled".into(),
            "enroll one: amux mfa enroll (or /mfa)"));
    }
    let has_offboard = c.prepare("SELECT offboarded_at FROM org_members LIMIT 0").is_ok();
    let sql = format!(
        "SELECT m.id, m.email, COALESCE(m.name,''), m.role, m.joined_at,
                COALESCE(t.scope_level, m.scope_level), COALESCE(t.scope_name, m.scope_name),
                COALESCE(t.name, ''), {}
         FROM org_members m LEFT JOIN org_teams t ON t.id = m.team_id ORDER BY m.joined_at",
        if has_offboard { "m.offboarded_at" } else { "NULL" }
    );
    // id, email, name, role, joined_at, scope level, scope name, team, offboarded_at
    type MemberRow = (String, String, String, String, i64, String, String, String, Option<i64>);
    let rows: rusqlite::Result<Vec<MemberRow>> =
        c.prepare(&sql).and_then(|mut st| {
            st.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))
            })?
            .collect()
        });
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            return json!({"measured": false, "why_unmeasured": format!("org_members unreadable: {e}"), "items": items});
        }
    };
    for (id, email, name, role, joined, level, scope_name, team, offboarded) in rows {
        let subject = format!("member:{email}");
        // joined_at is epoch seconds in org_members; the ledger is epoch ms.
        let joined_ms = joined.saturating_mul(1000);
        let seen = ledger.then(|| last_seen(c, &subject)).flatten();
        let enrolled = mfa_enrolled(c, &subject);
        items.push(json!({"subject": subject, "id": id, "name": name, "role": role,
            "scope": if level == "global" { "global".to_string() } else { format!("{level}:{scope_name}") },
            "team": team, "joined_at": joined, "offboarded_at": offboarded,
            "mfa_enrolled": enrolled, "last_seen": seen, "days_since_seen": days_since(now, seen)}));
        if offboarded.is_some() {
            continue;
        }
        if ledger && stale(now, seen, Some(joined_ms), window) {
            findings.push(finding(&subject, "person_inactive",
                match seen {
                    Some(_) => format!("{role} last seen {} days ago (window {window} days)", days_since(now, seen).unwrap_or(0)),
                    None => format!("{role} never seen in the ledger (window {window} days)"),
                },
                "confirm they still need access; if not: amux org offboard (POST /api/org/members/{email}/offboard)"));
        }
        if role == "admin" && mfa_on && enrolled == Some(false) {
            findings.push(finding(&subject, "admin_without_second_factor",
                "an admin can change members and has no second factor enrolled".into(),
                "ask them to enroll at /mfa, or set AMUX_MFA_POLICY=required"));
        }
        if level == "global" && role != "auditor" {
            findings.push(finding(&subject, "member_unscoped",
                format!("{role} reaches every group and worker"),
                "scope them to the groups they work in (PATCH /api/org/members/{id} scope_level/scope_name)"));
        }
    }
    json!({"measured": true, "n_considered": items.len(), "items": items, "last_seen_measured": ledger})
}

fn workers(conn: Option<&Connection>, home: &Path, now: i64, window: i64, findings: &mut Vec<Value>) -> Value {
    let names = worker_names(home);
    let ledger = conn.is_some_and(|c| table_exists(c, "_amux_ledger"));
    let sponsors = conn.is_some_and(|c| table_exists(c, "worker_sponsors"));
    let mut items = vec![];
    for name in &names {
        let caps = super::capabilities::for_worker(home, conn, name);
        let actor = format!("worker:{name}");
        let seen = conn.filter(|_| ledger).and_then(|c| last_seen(c, &actor));
        let sponsor: Option<String> = conn.filter(|_| sponsors).and_then(|c| {
            c.query_row("SELECT sponsor FROM worker_sponsors WHERE worker=?1", [name], |r| r.get(0)).optional().ok().flatten()
        });
        let archived = env_flag(home, name, "CC_ARCHIVED").is_some_and(|v| v == "1" || v == "true");
        items.push(json!({"worker": name, "archived": archived, "sponsor": sponsor,
            "last_seen": seen, "days_since_seen": days_since(now, seen), "capabilities": caps}));
        if archived {
            continue;
        }
        if caps["outbound"]["value"] == "allow" {
            findings.push(finding(&actor, "outbound_without_approval",
                format!("sends external email without a person's approval ({})", caps["outbound"]["source"].as_str().unwrap_or("")),
                "keep only if this worker's email is routine; otherwise remove the bypass rule"));
        }
        if caps["publish"]["value"] == "public" {
            findings.push(finding(&actor, "publishes_publicly",
                format!("may publish to the public internet ({})", caps["publish"]["source"].as_str().unwrap_or("")),
                "narrow AMUX_CAP_PUBLISH unless this worker ships public pages"));
        }
        if ledger && stale(now, seen, None, window) {
            findings.push(finding(&actor, "worker_inactive",
                match seen {
                    Some(_) => format!("last active {} days ago (window {window} days)", days_since(now, seen).unwrap_or(0)),
                    None => format!("never seen in the ledger (window {window} days)"),
                },
                "archive it if the work is over: amux archive"));
        }
        if sponsors && sponsor.is_none() {
            findings.push(finding(&actor, "worker_without_sponsor",
                "no person answers for this worker's schedules and tokens".into(),
                "set a sponsor: POST /api/schedules/{id}/reown or the worker's settings"));
        }
    }
    json!({"measured": true, "n_considered": names.len(), "items": items, "last_seen_measured": ledger,
        "capabilities_mode": super::capabilities::mode().as_str()})
}

fn apps(conn: Option<&Connection>, home: &Path, now: i64, window: i64, findings: &mut Vec<Value>) -> Value {
    let links = super::static_files::public_links::load(home);
    let ledger = conn.is_some_and(|c| table_exists(c, "_amux_ledger"));
    let mut items = vec![];
    for l in &links {
        let actor = format!("app:{}", l.id);
        let seen = conn.filter(|_| ledger).and_then(|c| last_seen(c, &actor));
        let approved = !l.approved_by.is_empty();
        // created_at in public-links.json is epoch seconds.
        let created_ms = l.created_at.saturating_mul(1000);
        items.push(json!({"id": l.id, "kind": l.kind, "title": l.title, "created_by": l.created_by,
            "created_at": l.created_at, "approved": approved, "approved_by": l.approved_by,
            "api_allow": l.api_allow, "api_requested": l.api_requested,
            "last_api_call": seen, "days_since_api_call": days_since(now, seen)}));
        if !approved && now - created_ms > window * DAY_MS {
            findings.push(finding(&actor, "link_pending_long",
                format!("\"{}\" has waited for approval longer than {window} days", l.title),
                "approve it or turn it off (amux publish --revoke)"));
        }
        if approved && !l.api_allow.is_empty() && ledger && stale(now, seen, Some(created_ms), window) {
            findings.push(finding(&actor, "app_api_unused",
                format!("\"{}\" may call {} amux route(s) and has not in {window} days", l.title, l.api_allow.len()),
                "remove its API access, or turn the app off"));
        }
    }
    json!({"measured": true, "n_considered": links.len(), "items": items, "last_seen_measured": ledger})
}

fn scim_tokens(conn: Option<&Connection>, now: i64, window: i64, findings: &mut Vec<Value>) -> Value {
    let Some(c) = conn.filter(|c| table_exists(c, "scim_tokens")) else {
        return json!({"measured": false, "why_unmeasured": "no scim_tokens table here", "items": []});
    };
    // id, label, created_at, last_used_at, revoked_at
    type TokenRow = (String, String, i64, Option<i64>, Option<i64>);
    let rows: rusqlite::Result<Vec<TokenRow>> = c
        .prepare("SELECT id, label, created_at, last_used_at, revoked_at FROM scim_tokens ORDER BY created_at")
        .and_then(|mut st| st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect());
    let rows = match rows {
        Ok(r) => r,
        Err(e) => return json!({"measured": false, "why_unmeasured": e.to_string(), "items": []}),
    };
    let mut items = vec![];
    for (id, label, created, used, revoked) in rows {
        // scim_tokens times are epoch seconds.
        let used_ms = used.map(|u| u.saturating_mul(1000));
        items.push(json!({"id": id, "label": label, "created_at": created, "last_used_at": used,
            "revoked_at": revoked, "days_since_used": days_since(now, used_ms)}));
        if revoked.is_none() && stale(now, used_ms, Some(created.saturating_mul(1000)), window) {
            findings.push(finding(&format!("scim_token:{id}"), "scim_token_unused",
                format!("\"{label}\" can add and remove people and has not been used in {window} days"),
                "revoke it: DELETE /api/scim/tokens/{id}"));
        }
    }
    json!({"measured": true, "n_considered": items.len(), "items": items})
}

fn standing(conn: Option<&Connection>, now: i64, window: i64, findings: &mut Vec<Value>) -> Value {
    let Some(c) = conn.filter(|c| table_exists(c, "standing_approvals")) else {
        return json!({"measured": false, "why_unmeasured": "no standing_approvals table here", "items": []});
    };
    let uses = table_exists(c, "standing_approval_uses");
    type Row = (i64, String, String, String, String, i64, Option<i64>);
    let rows: rusqlite::Result<Vec<Row>> = c
        .prepare("SELECT id, title, category, scope, granted_by, granted_at, expires_at FROM standing_approvals
                  WHERE revoked = 0 ORDER BY granted_at")
        .and_then(|mut st| {
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?
                .collect()
        });
    let rows = match rows {
        Ok(r) => r,
        Err(e) => return json!({"measured": false, "why_unmeasured": e.to_string(), "items": []}),
    };
    let mut items = vec![];
    for (id, title, category, scope, by, granted, expires) in rows {
        // standing_approvals times are epoch seconds.
        if expires.is_some_and(|e| e * 1000 < now) {
            continue;
        }
        let used: Option<i64> = uses
            .then(|| {
                c.query_row("SELECT MAX(ts) FROM standing_approval_uses WHERE approval_id=?1 AND verdict='applied'", [id],
                    |r| r.get::<_, Option<i64>>(0)).ok().flatten()
            })
            .flatten();
        let used_ms = used.map(|u| u.saturating_mul(1000));
        items.push(json!({"id": id, "title": title, "category": category, "scope": scope, "granted_by": by,
            "granted_at": granted, "expires_at": expires, "last_used": used, "days_since_used": days_since(now, used_ms)}));
        let subject = format!("standing_approval:{id}");
        if expires.is_none() {
            findings.push(finding(&subject, "standing_approval_no_expiry",
                format!("\"{title}\" ({category}, {scope}) never expires"), "give it an expiry, or revoke it if it is no longer needed"));
        }
        if uses && stale(now, used_ms, Some(granted.saturating_mul(1000)), window) {
            findings.push(finding(&subject, "standing_approval_unused",
                format!("\"{title}\" has not answered an ask in {window} days"), "revoke it"));
        }
    }
    json!({"measured": true, "n_considered": items.len(), "items": items, "uses_measured": uses})
}

/// The whole review. Pure over a connection and a home, for tests.
pub fn access_review(conn: Option<&Connection>, home: &Path, now: i64, window_days: i64) -> Value {
    let window = window_days.clamp(1, 3650);
    let mut findings = vec![];
    let people = people(conn, now, window, &mut findings);
    let workers = workers(conn, home, now, window, &mut findings);
    let apps = apps(conn, home, now, window, &mut findings);
    let scim = scim_tokens(conn, now, window, &mut findings);
    let standing = standing(conn, now, window, &mut findings);
    let mut rules: std::collections::BTreeMap<String, u64> = Default::default();
    for f in &findings {
        *rules.entry(f["rule"].as_str().unwrap_or("").to_string()).or_default() += 1;
    }
    json!({
        "generated_at": chrono::DateTime::from_timestamp_millis(now).map(|t| t.to_rfc3339()),
        "stale_days": window,
        "ledger": ledger_basis(conn),
        "identity": {
            "mfa": {"policy": super::mfa::policy().as_str(), "enforce": super::mfa::enforce_mode().as_str(),
                    "step_up": super::mfa::step_up_mode().as_str()},
            "sso": super::sso::report(),
            "kill_switch": {"enabled": super::governance::kill_enabled(),
                            "engaged": super::governance::kill_state(home).is_some()},
            "capabilities_mode": super::capabilities::mode().as_str(),
        },
        "people": people,
        "workers": workers,
        "apps": apps,
        "scim_tokens": scim,
        "standing_approvals": standing,
        "findings": findings,
        "findings_by_rule": rules,
        "measured": true,
        "n_considered": findings.len(),
        "decides": "nothing: every finding is a recommendation for the owner",
    })
}

#[derive(Deserialize, Default)]
struct ReviewQuery {
    stale_days: Option<i64>,
}

async fn access_review_route(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<ReviewQuery>) -> Response {
    if !super::governance::can_read(&headers) {
        return refuse();
    }
    counters::bump(&counters::ACCESS_REVIEWS);
    let home = crate::config::amux_home();
    let days = q.stale_days.unwrap_or(DEFAULT_STALE_DAYS);
    let v = match state.store.read_async(move |c| Ok(access_review(Some(c), &home, now_ms(), days))).await {
        Ok(v) => v,
        Err(_) => access_review(None, &crate::config::amux_home(), now_ms(), days),
    };
    tracing::info!(verdict = "compliance_access_review", measured = true,
        n_considered = v["n_considered"].as_u64().unwrap_or(0), "access review served");
    Json(v).into_response()
}

// ── Timeline ────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default, Clone)]
pub struct TimelineQuery {
    pub trace: Option<String>,
    pub worker: Option<String>,
    pub actor: Option<String>,
    /// Epoch ms.
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub limit: Option<i64>,
    /// `false` leaves commits out (no git calls).
    pub commits: Option<bool>,
}

impl TimelineQuery {
    fn trace_id(&self) -> Option<String> {
        self.trace.as_deref().map(str::trim).map(str::to_ascii_lowercase).filter(|t| !t.is_empty())
    }
    fn worker(&self) -> Option<&str> {
        self.worker.as_deref().map(str::trim).filter(|w| !w.is_empty())
    }
    fn actor(&self) -> Option<&str> {
        self.actor.as_deref().map(str::trim).filter(|a| !a.is_empty())
    }
}

fn valid_trace(t: &str) -> bool {
    t.len() == 32 && t.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Ledger rows for the query, oldest first, plus the chain's verify result.
pub fn ledger_rows(conn: &Connection, q: &TimelineQuery) -> Value {
    if !table_exists(conn, "_amux_ledger") {
        return json!({"measured": false, "why_unmeasured": "no ledger here", "events": []});
    }
    let limit = q.limit.unwrap_or(1000).clamp(1, MAX_TIMELINE);
    let trace = q.trace_id().unwrap_or_default();
    let worker = q.worker().map(|w| format!("worker:{w}")).unwrap_or_default();
    let actor = q.actor().unwrap_or("").to_string();
    let sql = "SELECT seq, event_id, ts, kind, trace_id, actor, claimed, on_behalf_of, authority, action, resource,
                      decision, outcome, hash IS NOT NULL
               FROM _amux_ledger
               WHERE ts >= ?1 AND ts <= ?2
                 AND (?3 = '' OR trace_id = ?3)
                 AND (?4 = '' OR actor = ?4 OR claimed = ?4 OR resource = ?4 OR resource LIKE ?4 || '/%')
                 AND (?5 = '' OR actor = ?5 OR claimed = ?5 OR on_behalf_of = ?5)
               ORDER BY ts, seq LIMIT ?6";
    let p = rusqlite::params![q.since.unwrap_or(0), q.until.unwrap_or(i64::MAX), trace, worker, actor, limit + 1];
    let rows: rusqlite::Result<Vec<Value>> = conn.prepare(sql).and_then(|mut st| {
        st.query_map(p, |r| {
            let ts: i64 = r.get(2)?;
            Ok(json!({"source": "ledger", "ts": ts,
                "at": chrono::DateTime::from_timestamp_millis(ts).map(|t| t.to_rfc3339()),
                "seq": r.get::<_, i64>(0)?, "event_id": r.get::<_, String>(1)?, "kind": r.get::<_, String>(3)?,
                "trace_id": r.get::<_, Option<String>>(4)?, "actor": r.get::<_, String>(5)?,
                "claimed": r.get::<_, Option<String>>(6)?, "on_behalf_of": r.get::<_, Option<String>>(7)?,
                "authority": r.get::<_, Option<String>>(8)?, "action": r.get::<_, Option<String>>(9)?,
                "resource": r.get::<_, Option<String>>(10)?, "decision": r.get::<_, Option<String>>(11)?,
                "outcome": r.get::<_, Option<String>>(12)?, "sealed": r.get::<_, bool>(13)?}))
        })?
        .collect()
    });
    match rows {
        Ok(mut v) => {
            let truncated = v.len() as i64 > limit;
            v.truncate(limit as usize);
            json!({"measured": true, "n_considered": v.len(), "truncated": truncated, "events": v})
        }
        Err(e) => json!({"measured": false, "why_unmeasured": e.to_string(), "events": []}),
    }
}

/// The git repositories behind the workers this query is about.
fn repos_for(home: &Path, q: &TimelineQuery) -> Vec<(String, PathBuf)> {
    let folders = super::capabilities::worker_folders(home);
    let mut seen = std::collections::BTreeSet::new();
    let mut out = vec![];
    for (name, dir) in folders {
        if q.worker().is_some_and(|w| w != name) {
            continue;
        }
        let mut root = dir.clone();
        let top = loop {
            if root.join(".git").exists() {
                break Some(root.clone());
            }
            if !root.pop() {
                break None;
            }
        };
        if let Some(top) = top {
            if seen.insert(top.clone()) {
                out.push((name, top));
            }
        }
        if out.len() >= MAX_REPOS {
            break;
        }
    }
    out
}

/// `git log` arguments for the query: the commit filter and the window.
pub fn git_log_args(q: &TimelineQuery) -> Vec<String> {
    let mut a = vec![
        "log".to_string(),
        "--all".into(),
        "-n".into(),
        "500".into(),
        // hash, author time, author name, author email, subject, then the trailers amux writes.
        "--format=%H%x1f%at%x1f%an%x1f%ae%x1f%s%x1f%(trailers:key=Amux-Trace,valueonly,separator=%x2c)%x1f%(trailers:key=On-behalf-of,valueonly,separator=%x2c)%x1f%(trailers:key=Amux-Host,valueonly,separator=%x2c)%x1e".into(),
    ];
    if let Some(s) = q.since {
        a.push(format!("--since=@{}", s / 1000));
    }
    if let Some(u) = q.until {
        a.push(format!("--until=@{}", u / 1000));
    }
    if let Some(t) = q.trace_id() {
        a.push("-F".into());
        a.push(format!("--grep=Amux-Trace: {t}"));
    } else if let Some(actor) = q.actor() {
        a.push("-F".into());
        a.push(format!("--grep=On-behalf-of: {actor}"));
    }
    if let Some(w) = q.worker() {
        // Agent commits are authored as `<worker> (amux) <worker+host@agents.noreply.amux.io>` (PR 15).
        a.push(format!("--author={w}+"));
    }
    a
}

pub fn parse_git_log(repo: &str, out: &str) -> Vec<Value> {
    out.split('\x1e')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .filter_map(|r| {
            let f: Vec<&str> = r.split('\x1f').collect();
            if f.len() < 5 {
                return None;
            }
            let secs: i64 = f[1].parse().ok()?;
            let ts = secs * 1000;
            Some(json!({"source": "commit", "ts": ts,
                "at": chrono::DateTime::from_timestamp_millis(ts).map(|t| t.to_rfc3339()),
                "repo": repo, "commit": f[0], "author": f[2], "author_email": f[3], "subject": f[4],
                "trace_id": f.get(5).map(|s| s.trim()).filter(|s| !s.is_empty()),
                "on_behalf_of": f.get(6).map(|s| s.trim()).filter(|s| !s.is_empty()),
                "host": f.get(7).map(|s| s.trim()).filter(|s| !s.is_empty())}))
        })
        .collect()
}

async fn commits(home: &Path, q: &TimelineQuery) -> Value {
    let repos = repos_for(home, q);
    let args = git_log_args(q);
    let mut events = vec![];
    let mut repo_status = vec![];
    for (worker, top) in &repos {
        let top_s = top.to_string_lossy().into_owned();
        let mut full: Vec<&str> = vec!["-C", &top_s];
        full.extend(args.iter().map(String::as_str));
        match super::session_verbs::run_cmd("git", &full, std::time::Duration::from_secs(10)).await {
            Some(o) if o.status.success() => {
                let found = parse_git_log(&top_s, &String::from_utf8_lossy(&o.stdout));
                repo_status.push(json!({"repo": top_s, "via_worker": worker, "measured": true, "n": found.len()}));
                events.extend(found);
            }
            Some(o) => repo_status.push(json!({"repo": top_s, "via_worker": worker, "measured": false,
                "why_unmeasured": String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or("git failed").to_string()})),
            None => repo_status.push(json!({"repo": top_s, "via_worker": worker, "measured": false,
                "why_unmeasured": "git log timed out after 10s"})),
        }
    }
    let all_measured = repo_status.iter().all(|r| r["measured"] == true);
    json!({"measured": all_measured, "n_repos": repos.len(), "repos": repo_status, "n_considered": events.len(),
        "events": events, "note": "commits are matched by the trailers amux writes (Amux-Trace, On-behalf-of) and the agent author identity; commits made before those existed are not found"})
}

async fn timeline_route(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<TimelineQuery>) -> Response {
    if !super::governance::can_read(&headers) {
        return refuse();
    }
    if q.trace_id().is_none() && q.worker().is_none() && q.actor().is_none() && q.since.is_none() {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "code": "timeline_needs_a_filter",
            "error": "name a trace, worker, actor or since (epoch ms)"}))).into_response();
    }
    if let Some(t) = q.trace_id() {
        if !valid_trace(&t) {
            return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "code": "bad_trace_id",
                "error": "a trace id is 32 lowercase hex characters"}))).into_response();
        }
    }
    counters::bump(&counters::TIMELINES);
    let q2 = q.clone();
    let read = state
        .store
        .read_async(move |c| {
            let ledger = ledger_rows(c, &q2);
            let trace = match q2.trace_id() {
                Some(t) => super::trace::tree(c, &t).unwrap_or_else(|e| json!({"measured": false, "why_unmeasured": e.to_string()})),
                None => Value::Null,
            };
            let chain = if table_exists(c, "_amux_ledger") { super::ledger::verify(c) } else { json!({"measured": false}) };
            Ok((ledger, trace, chain))
        })
        .await;
    let (ledger, trace, chain) = match read {
        Ok(t) => t,
        Err(e) => {
            return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"measured": false, "n_considered": 0,
                "why_unmeasured": e.to_string()}))).into_response()
        }
    };
    let home = crate::config::amux_home();
    let commits = if q.commits == Some(false) {
        json!({"measured": false, "why_unmeasured": "commits=false", "events": []})
    } else {
        commits(&home, &q).await
    };
    let mut merged: Vec<Value> = vec![];
    merged.extend(ledger["events"].as_array().cloned().unwrap_or_default());
    // Trace nodes the ledger already holds are the same events; keep the others
    // (schedule runs, deliveries, CLI telemetry) the trace tree joins in.
    if let Some(nodes) = trace.get("nodes").and_then(Value::as_array) {
        merged.extend(nodes.iter().filter(|n| n["source"] != "ledger").cloned());
    }
    merged.extend(commits["events"].as_array().cloned().unwrap_or_default());
    merged.sort_by_key(|e| e["ts"].as_i64().unwrap_or(0));
    let n = merged.len();
    tracing::info!(verdict = "compliance_timeline", measured = true, n_considered = n, "timeline served");
    Json(json!({
        "query": {"trace": q.trace_id(), "worker": q.worker(), "actor": q.actor(), "since": q.since, "until": q.until},
        "chain": chain,
        "sources": {
            "ledger": {"measured": ledger["measured"], "n": ledger["n_considered"], "truncated": ledger["truncated"]},
            "trace": if trace.is_null() { json!({"measured": false, "why_unmeasured": "no trace id given"}) }
                     else { json!({"measured": trace.get("measured").cloned().unwrap_or(json!(true)), "sources": trace.get("sources")}) },
            "commits": {"measured": commits["measured"], "n": commits["n_considered"], "repos": commits["repos"], "note": commits["note"]},
        },
        "events": merged,
        "measured": true,
        "n_considered": n,
    }))
    .into_response()
}

/// `compliance` in `/api/contract/counters`.
pub fn report() -> Value {
    use counters::*;
    json!({
        "measured": true,
        "n_considered": get(&ACCESS_REVIEWS) + get(&TIMELINES) + get(&REFUSED),
        "counts": {"access_reviews": get(&ACCESS_REVIEWS), "timelines": get(&TIMELINES), "refused": get(&REFUSED)},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        crate::db::migrate::test_memdb()
    }

    fn ledger_row(c: &Connection, id: &str, ts: i64, actor: &str, trace: Option<&str>) {
        let e = super::super::ledger::Event {
            event_id: id.into(),
            ts_ms: ts,
            kind: "test".into(),
            actor: actor.into(),
            trace_id: trace.map(String::from),
            ..Default::default()
        };
        super::super::ledger::append(c, &e);
    }

    fn home_with(workers: &[(&str, &str)]) -> tempfile::TempDir {
        let h = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(h.path().join("sessions")).unwrap();
        for (name, extra) in workers {
            let dir = h.path().join("w").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(h.path().join("sessions").join(format!("{name}.env")),
                format!("CC_DIR={}\n{extra}", dir.display())).unwrap();
        }
        h
    }

    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn an_inactive_member_is_flagged_and_an_active_one_is_not() {
        let c = db();
        let joined = (NOW - 400 * DAY_MS) / 1000;
        c.execute("INSERT INTO org_members (id,email,name,role,joined_at) VALUES ('m1','old@x.io','Old','member',?1),
                   ('m2','new@x.io','New','member',?1)", [joined]).unwrap();
        ledger_row(&c, "e1", NOW - 200 * DAY_MS, "member:old@x.io", None);
        ledger_row(&c, "e2", NOW - 2 * DAY_MS, "member:new@x.io", None);
        let h = home_with(&[]);
        let v = access_review(Some(&c), h.path(), NOW, 90);
        let inactive: Vec<&str> = v["findings"].as_array().unwrap().iter()
            .filter(|f| f["rule"] == "person_inactive").map(|f| f["subject"].as_str().unwrap()).collect();
        assert_eq!(inactive, vec!["member:old@x.io"]);
        let old = v["people"]["items"].as_array().unwrap().iter().find(|p| p["subject"] == "member:old@x.io").unwrap();
        assert_eq!(old["days_since_seen"], 200);
    }

    #[test]
    fn a_person_who_just_joined_is_not_flagged_for_not_acting_yet() {
        let c = db();
        let joined = (NOW - DAY_MS) / 1000;
        c.execute("INSERT INTO org_members (id,email,name,role,joined_at) VALUES ('m1','fresh@x.io','F','member',?1)", [joined]).unwrap();
        let h = home_with(&[]);
        let v = access_review(Some(&c), h.path(), NOW, 90);
        assert!(!v["findings"].as_array().unwrap().iter().any(|f| f["rule"] == "person_inactive"));
    }

    #[test]
    fn an_offboarded_member_raises_nothing() {
        let c = db();
        let joined = (NOW - 400 * DAY_MS) / 1000;
        c.execute("INSERT INTO org_members (id,email,name,role,joined_at,offboarded_at) VALUES ('m1','gone@x.io','G','admin',?1,?1)", [joined]).unwrap();
        let h = home_with(&[]);
        let v = access_review(Some(&c), h.path(), NOW, 90);
        assert!(!v["findings"].as_array().unwrap().iter().any(|f| f["subject"] == "member:gone@x.io"));
    }

    #[test]
    fn workers_carry_their_capabilities_and_an_idle_one_is_flagged() {
        let c = db();
        ledger_row(&c, "e1", NOW - DAY_MS, "worker:busy", None);
        let h = home_with(&[("busy", ""), ("idle", ""), ("done", "CC_ARCHIVED=1\n")]);
        let v = access_review(Some(&c), h.path(), NOW, 30);
        let items = v["workers"]["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert!(items.iter().all(|w| w["capabilities"]["worker"] == w["worker"]));
        let idle: Vec<&str> = v["findings"].as_array().unwrap().iter()
            .filter(|f| f["rule"] == "worker_inactive").map(|f| f["subject"].as_str().unwrap()).collect();
        assert_eq!(idle, vec!["worker:idle"], "busy is active and done is archived");
    }

    #[test]
    fn an_unused_scim_token_is_flagged_and_a_revoked_one_is_not() {
        let c = db();
        let old = (NOW - 200 * DAY_MS) / 1000;
        c.execute("INSERT INTO scim_tokens (id,label,token_hash,created_by,created_at,last_used_at,revoked_at) VALUES
                   ('t1','okta','h1','owner',?1,NULL,NULL), ('t2','old','h2','owner',?1,NULL,?1)", [old]).unwrap();
        let h = home_with(&[]);
        let v = access_review(Some(&c), h.path(), NOW, 90);
        let subs: Vec<&str> = v["findings"].as_array().unwrap().iter()
            .filter(|f| f["rule"] == "scim_token_unused").map(|f| f["subject"].as_str().unwrap()).collect();
        assert_eq!(subs, vec!["scim_token:t1"]);
    }

    #[test]
    fn a_standing_approval_with_no_expiry_is_flagged() {
        let c = db();
        let granted = (NOW - 10 * DAY_MS) / 1000;
        c.execute("INSERT INTO standing_approvals (title,allowed,category,granted_at) VALUES ('ship','x','decision',?1)", [granted]).unwrap();
        let h = home_with(&[]);
        let v = access_review(Some(&c), h.path(), NOW, 90);
        assert!(v["findings"].as_array().unwrap().iter().any(|f| f["rule"] == "standing_approval_no_expiry"));
        assert_eq!(v["standing_approvals"]["measured"], true);
    }

    #[test]
    fn without_a_database_every_section_says_it_did_not_measure() {
        let h = home_with(&[]);
        let v = access_review(None, h.path(), NOW, 90);
        assert_eq!(v["people"]["measured"], false);
        assert_eq!(v["scim_tokens"]["measured"], false);
        assert_eq!(v["standing_approvals"]["measured"], false);
        assert_eq!(v["ledger"]["measured"], false);
    }

    #[test]
    fn the_timeline_filters_by_trace_and_worker_in_order() {
        let c = db();
        let t = "0123456789abcdef0123456789abcdef";
        ledger_row(&c, "b", 2000, "worker:w", Some(t));
        ledger_row(&c, "a", 1000, "owner", Some(t));
        ledger_row(&c, "x", 1500, "worker:other", None);
        let by_trace = ledger_rows(&c, &TimelineQuery { trace: Some(t.into()), ..Default::default() });
        let ids: Vec<&str> = by_trace["events"].as_array().unwrap().iter().map(|e| e["event_id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["a", "b"]);
        let by_worker = ledger_rows(&c, &TimelineQuery { worker: Some("w".into()), ..Default::default() });
        let ids: Vec<&str> = by_worker["events"].as_array().unwrap().iter().map(|e| e["event_id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["b"]);
        let window = ledger_rows(&c, &TimelineQuery { since: Some(1200), until: Some(1800), ..Default::default() });
        assert_eq!(window["n_considered"], 1);
    }

    #[test]
    fn an_auditor_reads_the_compliance_views_and_cannot_write() {
        use axum::http::Method;
        use super::super::org::auditor_may;
        for p in ["/api/compliance/access-review", "/api/compliance/timeline"] {
            assert!(auditor_may(&Method::GET, p), "{p}");
            assert!(!auditor_may(&Method::POST, p), "POST {p}");
        }
    }

    #[test]
    fn git_log_args_select_by_trailer_and_author() {
        let q = TimelineQuery { trace: Some("AB".repeat(16)), worker: Some("wexus".into()), since: Some(5000), ..Default::default() };
        let a = git_log_args(&q);
        assert!(a.contains(&format!("--grep=Amux-Trace: {}", "ab".repeat(16))));
        assert!(a.contains(&"--author=wexus+".to_string()));
        assert!(a.contains(&"--since=@5".to_string()));
        let parsed = parse_git_log("/r", "abc\x1f1700000000\x1fwexus (amux)\x1fwexus+h@agents.noreply.amux.io\x1ffix\x1ftid\x1fowner\x1fhost1\x1e");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["ts"], 1_700_000_000_000i64);
        assert_eq!(parsed[0]["trace_id"], "tid");
        assert_eq!(parsed[0]["on_behalf_of"], "owner");
    }
}
