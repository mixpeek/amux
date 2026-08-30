//! Urgent owner alert + its channel config (Python amux-server.py:65560-65618,
//! `_send_urgent_alert` ~:7872, `urgent_alert_decision` ~:7821).
//!
//! Routes (nested at /api/alert):
//! - GET/PATCH /api/alert/config — channel toggles + phone, stored where
//!   Python stores them: the `AMUX_OWNER_PHONE` / `AMUX_URGENT_PUSH` /
//!   `AMUX_URGENT_SMS` keys in `~/.amux/server.env` (Python `_env_set`).
//!   NOT the prefs table: both servers share `~/.amux` during coexistence,
//!   and the Python sender reads exactly these env keys — a prefs-table copy
//!   would be a second spelling of the same fact that immediately diverges.
//! - POST /api/alert/owner — the fire alarm: push (crate::push, live
//!   `push_subscriptions` table) + iMessage/SMS (Twilio when configured,
//!   else the EXACT osascript argv Python uses). Response contract per
//!   CLAUDE.md: `{"channels": {"push": ..., "sms": ...}}` plus
//!   ok/message/origin/claimed/provenance_mismatch.
//! - GET /api/alert/owner — the first-hand `owner_alerts` ledger
//!   (AMUX-1795): every attempt recorded with server-verified origin AND
//!   claimed session, so provenance mismatches are caught, not believed.
//!
//! Delivery is behind [`AlertChannels`] so tests mock both senders — a test
//! must NEVER page the owner's phone. The dedupe/storm guard state lives in
//! the router instance (fresh per construction), mirroring Python's
//! in-process dicts; the decision function is a pure port of Python's
//! `urgent_alert_decision` so the 38-page storm (MF-427) replays exactly.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::Digest;

use super::settings::{amux_home, effective_env, set_server_env_key, truthy};
use super::AppState;

// ---------------------------------------------------------------------------
// Channel seam — the ONLY place a real page can leave the process.
// ---------------------------------------------------------------------------

#[async_trait]
pub trait AlertChannels: Send + Sync {
    /// In-app/web push. Ok(()) = dispatched (Python's `channels["push"] =
    /// "sent"` means the broadcast was handed off, not per-device receipts).
    async fn push(&self, state: &AppState, session: &str, message: &str) -> Result<(), String>;
    /// SMS/iMessage. Returns Python's `_send_sms` tuple: (ok, detail) where
    /// detail is "twilio"/"imessage" on success or the failure text.
    async fn sms(&self, phone: &str, text: &str) -> (bool, String);
    /// Email, via the connected Gmail account (AMUX-3203). This is the channel
    /// that reaches the owner with NO manual setup: push needs him to subscribe
    /// and sms needs his phone, but a connected Gmail account already exists and
    /// its own inbox is a destination. `(ok, detail)`, detail names the account.
    async fn email(&self, to: &str, subject: &str, body: &str) -> (bool, String);
}

/// Production channels: web push via crate::push, SMS via Twilio-or-osascript.
pub struct RealChannels;

#[async_trait]
impl AlertChannels for RealChannels {
    async fn push(&self, state: &AppState, session: &str, message: &str) -> Result<(), String> {
        // Python `_push_alert("urgent", session or "amux", msg)`: title is
        // the session, first body line is the human label for the type.
        let title = if session.is_empty() { "amux" } else { session };
        let results = crate::push::send_all(
            state,
            title,
            &format!("URGENT\n{message}"),
            session,
            "urgent",
            "/",
        )
        .await;
        push_delivery_verdict(&results)
    }

    async fn sms(&self, phone: &str, text: &str) -> (bool, String) {
        send_sms(phone, text).await
    }

    async fn email(&self, to: &str, subject: &str, body: &str) -> (bool, String) {
        let gc = crate::integrations::email::GmailClient::new_default();
        // Try connected accounts NEWEST-TOKEN-FIRST until one actually sends. A
        // stale account whose refresh_token is dead (invalid_grant) must not
        // silently win: during a live cloud outage the fire alarm reached NOBODY
        // because it picked the first-ALPHABETICAL account (esteininger21@gmail.com,
        // token dead) while ethan@mixpeek.com was fresh and sending fine moments
        // later (amux-cloud, 2026-08-16). Trying each in turn means the alarm
        // delivers if ANY connected account can, not just if the first one can.
        let accounts = crate::integrations::email::connected_accounts_by_freshness_in(gc.home());
        if accounts.is_empty() {
            return (false, "no connected Gmail account to send from".into());
        }
        let mut last_err = String::new();
        for from in &accounts {
            match gc.compose_send(from, to, subject, body, "", "", "", "", true, &[]).await {
                Ok(_) => return (true, format!("email via {from}")),
                Err(e) => last_err = format!("email error via {from}: {}", truncate(&e, 80)),
            }
        }
        (false, if last_err.is_empty() { "email send failed".into() } else { last_err })
    }
}

/// The FIRE-ALARM honesty rule, pure over `send_all`'s per-endpoint results so
/// it can be tested without a network or a DB (mixpeek-finances / amux-cloud
/// AC-347). `Ok(())` — a legitimate "sent" — is returned ONLY when an endpoint
/// actually ACCEPTED the push (2xx). Everything else is a truthful error:
///
///   * broadcast couldn't be attempted (vapid/db) — the synthetic (host="",
///     status 0) row;
///   * ZERO subscriptions (AMUX-2938) — `send_all` returns an empty vec, which
///     the old code let fall through to "sent" while reaching nobody (measured
///     2026-08-11: 0 rows in `push_subscriptions`, alert answered {"push":"sent"});
///   * subscriptions EXIST but every endpoint rejected (410 Gone / expired) —
///     the gap AMUX-2938 left and MF-582 does NOT close: registering a sub does
///     not help once it lapses, and the old rule ("per-endpoint rejections count
///     as dispatched") reported "sent" for a page nobody received.
///
/// "sent" must mean an endpoint took it, never the send call's self-description.
fn push_delivery_verdict(results: &[Value]) -> Result<(), String> {
    if results.len() == 1 {
        let r = &results[0];
        if r.get("host").and_then(Value::as_str) == Some("")
            && r.get("status").and_then(Value::as_u64) == Some(0)
        {
            let detail = r.get("detail").and_then(Value::as_str).unwrap_or("push failed");
            if detail.starts_with("vapid:") || detail.starts_with("db") {
                return Err(detail.to_string());
            }
        }
    }
    if results.is_empty() {
        return Err("no push subscriptions — nobody is registered to receive it".into());
    }
    let delivered = results.iter().any(|r| {
        matches!(r.get("status").and_then(Value::as_u64), Some(s) if (200..300).contains(&s))
    });
    if !delivered {
        return Err(format!(
            "push not delivered — {} subscription(s), 0 accepted (all rejected/expired)",
            results.len()
        ));
    }
    Ok(())
}

/// Python `_send_sms`: Twilio when TWILIO_* is configured, else macOS
/// Messages via osascript — argv ported EXACTLY (`buddy ph of s` is the form
/// that delivers; `participant ph of svc` compiles and hangs), guarded by
/// the same 12s timeout so a TCC permission wall cannot hang the server.
async fn send_sms(phone: &str, text: &str) -> (bool, String) {
    if phone.is_empty() {
        return (false, "no phone configured".into());
    }
    let home = amux_home();
    let sid = effective_env(&home, "TWILIO_ACCOUNT_SID").unwrap_or_default();
    let tok = effective_env(&home, "TWILIO_AUTH_TOKEN").unwrap_or_default();
    let frm = effective_env(&home, "TWILIO_FROM").unwrap_or_default();
    if !sid.is_empty() && !tok.is_empty() && !frm.is_empty() {
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
        {
            Ok(c) => c,
            Err(e) => return (false, format!("twilio error: {}", truncate(&e.to_string(), 120))),
        };
        let url = format!("https://api.twilio.com/2010-04-01/Accounts/{sid}/Messages.json");
        let res = client
            .post(&url)
            .basic_auth(&sid, Some(&tok))
            .form(&[("From", frm.as_str()), ("To", phone), ("Body", text)])
            .send()
            .await;
        return match res {
            Ok(r) if r.status().is_success() => (true, "twilio".into()),
            Ok(r) => (false, format!("twilio error: {}", truncate(&format!("HTTP {}", r.status()), 120))),
            Err(e) => (false, format!("twilio error: {}", truncate(&e.to_string(), 120))),
        };
    }
    // Circuit breaker on the TCC wall (AMUX-3492). With the Automation
    // permission missing, the osascript below hangs to the full 12s timeout —
    // and it did so INLINE on every owner alert, three pages in three days at
    // a near-identical 12.6-12.7s. A wall that just ate 12s will eat the next
    // 12s too, so after a timeout the channel is skipped (saying so in the
    // channels map, never silently — AMUX-2938) until the cooldown passes,
    // then probed again so a granted permission resumes within one window.
    // Fast errors are NOT breakered: they cost nothing and carry their own
    // diagnosis.
    //
    // The wall stamp PERSISTS across restarts (AMUX-3500 corrected AMUX-3492's
    // own judgment here). The first cut kept it in-process, reasoning "a
    // restart re-probing once is the behavior you want" — which assumed
    // restarts are rare. On this machine the auto-builder restarts the server
    // on every landed commit (~6x on the day this shipped), so with sparse
    // alerts virtually EVERY page was a fresh process paying the 12s probe:
    // the breaker was measurably inert (12,674ms at 11:28 pre-fix, 12,526ms
    // at 14:11 post-fix, each the first alert on its process). The wall is
    // per-MACHINE state — a TCC permission — so the stamp is a file under
    // AMUX_HOME (mtime = wall time), machine-scoped like the thing it
    // records. The atomic stays as the in-process fast path.
    let wall_ts = IMSG_WALL_TS
        .load(std::sync::atomic::Ordering::Relaxed)
        .max(imessage_wall_stamp_ts(&home));
    if let Some(skip) = imessage_breaker_verdict(wall_ts, now_f64() as u64, imessage_retry_s(&home))
    {
        return (false, skip);
    }
    let run = tokio::process::Command::new("osascript")
        .args([
            "-e", "on run {msg, ph}",
            "-e", "tell application \"Messages\"",
            "-e", "set s to first service whose service type = iMessage",
            "-e", "set b to buddy ph of s",
            "-e", "send msg to b",
            "-e", "end tell",
            "-e", "end run",
            "--",
        ])
        .arg(text)
        .arg(phone)
        .output();
    match tokio::time::timeout(std::time::Duration::from_secs(12), run).await {
        Err(_) => {
            IMSG_WALL_TS.store(now_f64() as u64, std::sync::atomic::Ordering::Relaxed);
            stamp_imessage_wall(&home);
            // TWO-FIXES: the 12s burn used to leave no log line of its own —
            // only the channel string in the ledger, invisible unless every
            // channel failed. The wall is now greppable where sweeps look.
            tracing::warn!(
                "[urgent-alert] imessage hit the 12s TCC wall — Automation permission for \
                 Messages is missing; channel breakered for {}s (AMUX_IMESSAGE_RETRY_S)",
                imessage_retry_s(&home)
            );
            (
                false,
                "imessage timed out — grant Automation permission for Messages, or set TWILIO_* creds"
                    .into(),
            )
        }
        Ok(Err(e)) => (false, format!("imessage error: {}", truncate(&e.to_string(), 100))),
        Ok(Ok(out)) if out.status.success() => {
            IMSG_WALL_TS.store(0, std::sync::atomic::Ordering::Relaxed);
            clear_imessage_wall(&home);
            (true, "imessage".into())
        }
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            (false, format!("imessage error: {}", truncate(stderr.trim(), 100)))
        }
    }
}

/// The wall stamp's file: mtime = when the 12s TCC timeout last fired.
/// Machine-scoped on purpose (see the breaker comment above).
fn imessage_wall_stamp_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join("imessage-wall.stamp")
}

/// Epoch seconds of the persisted wall, 0 when none (or unreadable — an
/// absent stamp means probe, which fails toward one 12s wait, never toward
/// a silently skipped channel).
fn imessage_wall_stamp_ts(home: &std::path::Path) -> u64 {
    std::fs::metadata(imessage_wall_stamp_path(home))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn stamp_imessage_wall(home: &std::path::Path) {
    let _ = std::fs::write(
        imessage_wall_stamp_path(home),
        b"mtime of this file = last 12s iMessage TCC-wall timeout (AMUX-3500)\n",
    );
}

fn clear_imessage_wall(home: &std::path::Path) {
    let _ = std::fs::remove_file(imessage_wall_stamp_path(home));
}

/// Epoch seconds of the last 12s-timeout failure; 0 = closed (send normally).
static IMSG_WALL_TS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn imessage_retry_s(home: &std::path::Path) -> u64 {
    effective_env(home, "AMUX_IMESSAGE_RETRY_S")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(900)
}

/// Pure breaker decision, split out so the skip/probe boundary is testable
/// without a Messages.app or a 12s wait. Some(reason) = skip the send.
fn imessage_breaker_verdict(last_wall_ts: u64, now_s: u64, retry_s: u64) -> Option<String> {
    if last_wall_ts == 0 {
        return None;
    }
    let elapsed = now_s.saturating_sub(last_wall_ts);
    if elapsed >= retry_s {
        return None; // cooldown over — probe the channel again
    }
    Some(format!(
        "imessage skipped — hit the 12s Automation-permission wall {elapsed}s ago \
         (AMUX-3492); next probe in {}s. Grant Automation permission for Messages \
         or set TWILIO_* creds",
        retry_s - elapsed
    ))
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ---------------------------------------------------------------------------
// Dedupe + storm guard (pure port of Python's urgent_alert_decision)
// ---------------------------------------------------------------------------

/// Python `URGENT_STORM_THRESHOLD` / `_WINDOW` / `_MUTE`.
const STORM_THRESHOLD: usize = 2;
const STORM_WINDOW: f64 = 1800.0;
const STORM_MUTE: f64 = 1800.0;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum AlertAction {
    Send,
    Dedupe,
    StormNotice,
    Muted,
}

/// Pure decision for one alert attempt — no I/O, no clock, so the 38-row
/// storm (MF-427) can be replayed exactly. Returns
/// (action, new_hist, new_mute_until). `dedupe_last` of None/0.0 means NEVER
/// SENT (Python's explicit fix for the "sent at epoch 0" conflation).
pub(crate) fn urgent_alert_decision(
    now: f64,
    hist: &[f64],
    mute_until: f64,
    dedupe_last: Option<f64>,
) -> (AlertAction, Vec<f64>, f64) {
    if let Some(last) = dedupe_last {
        if last != 0.0 && now - last < 60.0 {
            return (AlertAction::Dedupe, hist.to_vec(), mute_until);
        }
    }
    if mute_until != 0.0 && now < mute_until {
        // Still storming: extend rather than count down toward a resume.
        return (AlertAction::Muted, hist.to_vec(), now + STORM_MUTE);
    }
    let mut recent: Vec<f64> = hist.iter().copied().filter(|t| now - t < STORM_WINDOW).collect();
    recent.push(now);
    if recent.len() >= STORM_THRESHOLD {
        return (AlertAction::StormNotice, recent, now + STORM_MUTE);
    }
    (AlertAction::Send, recent, 0.0)
}

/// Rebuild guard state for one key from the `owner_alerts` LEDGER.
///
/// The in-memory maps below are the Python port's shape, and on this server
/// they are fiction: `AlertGuard::default()` is constructed in `routes()`, and
/// the process re-execs every time the auto-builder adopts a commit — many
/// times an hour on a shared checkout. So the storm guard forgot everything
/// constantly and every alert arrived as a first alert.
///
/// MEASURED (MF-427, 2026-08-11): 81 rows in owner_alerts, **0 with deduped=1**
/// — the guard had never suppressed anything, ever. The reported storm was 38
/// identical `--help` alerts over 186 minutes at a ~302s cadence;
/// STORM_THRESHOLD=2 over an 1800s window would have muted it after the second,
/// had the state survived.
///
/// The durable history was already written on every attempt — including
/// suppressed ones, deliberately, so suppression never hides evidence. Nothing
/// read it back. This reads it back.
fn guard_state_from_ledger(
    conn: &rusqlite::Connection,
    session: &str,
    msg: &str,
) -> (Vec<f64>, Option<f64>) {
    let Ok(mut stmt) = conn.prepare(
        "SELECT ts FROM owner_alerts WHERE claimed=?1 AND message=?2 ORDER BY ts DESC LIMIT 64",
    ) else {
        return (vec![], None);
    };
    let ts: Vec<f64> = stmt
        .query_map(rusqlite::params![session, msg], |r| r.get::<_, f64>(0))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();
    let last = ts.first().copied();
    let mut hist = ts;
    hist.reverse(); // oldest-first, for the storm window filter
    (hist, last)
}

/// Per-router guard state (Python's `_urgent_alert_last` / `_hist` / `_mute`
/// module dicts). Keyed by sha256(claimed_session + "|" + msg)[..16].
#[derive(Default)]
pub struct AlertGuard {
    last: HashMap<String, f64>,
    hist: HashMap<String, Vec<f64>>,
    mute: HashMap<String, f64>,
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

pub fn routes() -> Router<AppState> {
    routes_with(Arc::new(RealChannels))
}

pub fn routes_with(channels: Arc<dyn AlertChannels>) -> Router<AppState> {
    Router::new()
        .route("/config", axum::routing::get(get_config).patch(patch_config))
        .route("/owner", axum::routing::post(post_owner).get(get_owner_ledger))
        .layer(Extension(channels))
        .layer(Extension(Arc::new(Mutex::new(AlertGuard::default()))))
}

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

/// Python `_hdr_worker`: X-Amux-Worker canonical, X-Amux-Session accepted.
/// The urgent-alert handler truncates it to 64 chars.
pub(crate) fn hdr_worker(headers: &HeaderMap) -> String {
    for name in ["x-amux-worker", "x-amux-session"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim) {
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    String::new()
}

// ---- /api/alert/config -----------------------------------------------------

/// The send path's own SMS gate, shared with `/api/alert/config` so the view
/// and the send cannot disagree. Found by amux-frustrations verifying AC-362:
/// config reported `"sms": true` in exactly the state where a real send
/// reports "no phone configured (AMUX_OWNER_PHONE is empty)" — the flag is a
/// real fact (the toggle), but a reader checking it believed a send would
/// text. Returns (deliverable, blocked_reason); the reason strings are the
/// send path's own, verbatim, so the two surfaces stay in string parity.
fn sms_gate(home: &std::path::Path) -> (bool, Option<&'static str>) {
    if effective_env(home, "AMUX_URGENT_SMS").unwrap_or_else(|| "1".into()) == "0" {
        return (false, Some("disabled (AMUX_URGENT_SMS=0)"));
    }
    if effective_env(home, "AMUX_OWNER_PHONE").unwrap_or_default().is_empty() {
        return (false, Some("no phone configured (AMUX_OWNER_PHONE is empty)"));
    }
    (true, None)
}

async fn get_config() -> Response {
    let home = amux_home();
    let (sms_deliverable, sms_blocked) = sms_gate(&home);
    Json(json!({
        "phone": effective_env(&home, "AMUX_OWNER_PHONE").unwrap_or_default(),
        "push": effective_env(&home, "AMUX_URGENT_PUSH").unwrap_or_else(|| "1".into()) != "0",
        // `sms` stays the raw TOGGLE (existing consumers render it as the
        // setting); deliverability is its own field because they genuinely
        // differ — a toggle can be on with no phone to text.
        "sms": effective_env(&home, "AMUX_URGENT_SMS").unwrap_or_else(|| "1".into()) != "0",
        "sms_deliverable": sms_deliverable,
        "sms_blocked_reason": sms_blocked,
        "sms_provider": if effective_env(&home, "TWILIO_ACCOUNT_SID").is_some() { "twilio" } else { "imessage" },
    }))
    .into_response()
}

async fn patch_config(body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let home = amux_home();
    // Python: each key applied only when PRESENT in the body (`"phone" in body`).
    if let Some(v) = body.get("phone") {
        let phone = v.as_str().unwrap_or("").trim().to_string();
        if let Err(e) = set_server_env_key(&home, "AMUX_OWNER_PHONE", &phone) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() }));
        }
    }
    for (key, env_key) in [("push", "AMUX_URGENT_PUSH"), ("sms", "AMUX_URGENT_SMS")] {
        if let Some(v) = body.get(key) {
            let val = if truthy(v) { "1" } else { "0" };
            if let Err(e) = set_server_env_key(&home, env_key, val) {
                return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() }));
            }
        }
    }
    Json(json!({ "ok": true })).into_response()
}

// ---- POST /api/alert/owner --------------------------------------------------

/// Python repr() of a short message string, for the junk-refusal error text.
fn py_repr(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

use crate::config::now_f64;

/// Best-effort ledger append (Python `_record_owner_alert`): never let a
/// ledger-write failure block the alarm itself.
async fn record_owner_alert(
    state: &AppState,
    origin: &str,
    claimed: &str,
    message: &str,
    reason: &str,
    channels: &Map<String, Value>,
    deduped: bool,
) {
    let (origin, claimed, message, reason) =
        (origin.to_string(), claimed.to_string(), message.to_string(), reason.to_string());
    let channels_json = serde_json::to_string(channels).unwrap_or_else(|_| "{}".into());
    let ts = now_f64() as i64;
    let res = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO owner_alerts (ts, origin, claimed, message, reason, channels, deduped)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![ts, origin, claimed, message, reason, channels_json, deduped as i64],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    if let Err(e) = res {
        tracing::warn!("[urgent-alert] LEDGER WRITE FAILED: {e}");
    }
}

async fn post_owner(
    State(state): State<AppState>,
    Extension(channels): Extension<Arc<dyn AlertChannels>>,
    Extension(guard): Extension<Arc<Mutex<AlertGuard>>>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let message = body.get("message").and_then(Value::as_str).unwrap_or("");
    // Claimed origin is the body's self-report; the verified origin is the
    // header identity — the ledger records BOTH (AMUX-1795).
    let session = body.get("session").and_then(Value::as_str).unwrap_or("").to_string();
    let reason = body.get("reason").and_then(Value::as_str).unwrap_or("").to_string();
    let origin = truncate(&hdr_worker(&headers), 64);

    // Junk-message rejection (the 38-SMS night): flags and empty strings are
    // never legitimate pages; refuse loudly instead of texting the owner.
    let m = message.trim();
    if m.is_empty() || m.starts_with('-') || ["help", "usage", "test"].contains(&m.to_lowercase().as_str()) {
        let who = if !origin.is_empty() {
            origin.as_str()
        } else if !session.is_empty() {
            session.as_str()
        } else {
            "unknown"
        };
        tracing::warn!(
            "[alert] REFUSED junk owner-alert message {} from {who} — flags/empty are never pages",
            py_repr(m),
        );
        return Json(json!({
            "error": format!("refused: {} is not an alert message", py_repr(m)),
            "sent": false,
        }))
        .into_response();
    }

    let dry_run = body.get("dry_run").and_then(Value::as_bool).unwrap_or(false);

    let mut msg = m.to_string();
    if !reason.is_empty() {
        msg = format!("{msg}\n({reason})");
    }
    let mismatch = !origin.is_empty() && !session.is_empty() && origin != session;
    if mismatch {
        tracing::warn!(
            "[urgent-alert] PROVENANCE MISMATCH: verified origin={origin:?} but claimed session={session:?} — recording both"
        );
    }

    let key = {
        let mut h = sha2::Sha256::new();
        h.update(format!("{session}|{msg}"));
        hex::encode(h.finalize())[..16].to_string()
    };
    let now = now_f64();
    // Seed from the LEDGER when the in-memory guard has nothing for this key —
    // i.e. after every restart, which is most of the time on this server. The
    // in-memory maps stay the hot path; the ledger is what lets the guard
    // survive the auto-builder swapping the binary underneath it.
    let (ledger_hist, ledger_last) = match state.store.read() {
        Ok(conn) => guard_state_from_ledger(&conn, &session, &msg),
        Err(_) => (vec![], None),
    };
    // `simulate_history: N` (dry-run only) — synthesise N prior in-window
    // attempts so the send->dedupe->mute TRANSITION can be exercised without
    // writing history or paging anyone.
    //
    // mixpeek-frustrations could verify "a first alert still sends" and "dry-run
    // does not mutate", but NOT the transition, and for a structural reason: a
    // dry run cannot write history, so repeating it can never cross
    // threshold=2. Their words, and they are right — without this the claim
    // "the guard suppresses" stays reasoned-not-measured, which is precisely
    // the state my own 81-rows-zero-deduped finding shows is dangerous.
    let simulate: Option<i64> = body.get("simulate_history").and_then(Value::as_i64);
    let ledger_hist: Vec<f64> = match simulate {
        // Spread them inside the storm window, oldest first, all strictly
        // before `now` — the same shape a real burst leaves behind.
        Some(n) if dry_run && n > 0 => (1..=n.min(64))
            .map(|i| now - (STORM_WINDOW / (n.min(64) as f64 + 1.0)) * (i as f64))
            .rev()
            .collect(),
        _ => ledger_hist,
    };
    let (action, hist_len) = {
        let mut g = guard.lock().unwrap_or_else(|e| e.into_inner());
        let mem_hist: Vec<f64> = g.hist.get(&key).cloned().unwrap_or_default();
        let hist_in: Vec<f64> = if mem_hist.is_empty() { ledger_hist } else { mem_hist };
        let last_in = g.last.get(&key).copied().or(ledger_last);
        let (action, new_hist, new_mute) = urgent_alert_decision(
            now,
            &hist_in,
            g.mute.get(&key).copied().unwrap_or(0.0),
            last_in,
        );
        let hist_len = new_hist.len();
        // A DRY RUN MUST NOT MUTATE THE GUARD. Caught on the first live probe of
        // this very feature: four dry-runs of one message returned send, then
        // dedupe, dedupe, dedupe — because the first had written `last`. That
        // means a dry run could suppress a subsequent REAL alert for 60s, and a
        // rehearsal that quiets the fire alarm is worse than no rehearsal.
        if !dry_run {
            g.hist.insert(key.clone(), new_hist);
            g.mute.insert(key.clone(), new_mute);
            if matches!(action, AlertAction::Send | AlertAction::StormNotice) {
                g.last.insert(key.clone(), now);
            }
        }
        (action, hist_len)
    };

    // DRY RUN (MF-427). Verifying a storm guard used to require FIRING the
    // channel, which reaches Ethan's phone by push AND iMessage — and because a
    // storm needs a sustained burst to reproduce, an honest test meant paging
    // him repeatedly. mixpeek-frustrations correctly refused to make that trade
    // and stopped. This runs the identical decision against the identical state
    // and reports the verdict, sending nothing and writing nothing.
    if dry_run {
        return Json(json!({
            "ok": true, "dry_run": true,
            "would": match action {
                AlertAction::Send => "send",
                AlertAction::Dedupe => "suppress (dedupe: same message within 60s)",
                AlertAction::StormNotice => "send ONE storm notice, then mute",
                AlertAction::Muted => "suppress (storm mute active)",
            },
            // NAMED FOR WHAT IT IS. `hist_len` was ambiguous exactly at the
            // boundary the guard turns on: it counts in-window history
            // INCLUDING this hypothetical alert, so a novel message reports 1
            // and a key with one prior row reports 2 — which is threshold, so
            // the SECOND alert is the one that mutes. mixpeek-frustrations
            // caught the ambiguity and inferred the right reading; a field that
            // needs inferring on the deciding boundary is a bad field.
            "would_be_attempt_number": hist_len,
            "prior_attempts_in_window": hist_len.saturating_sub(1),
            "simulated_history": simulate.filter(|_| dry_run),
            "storm_threshold": STORM_THRESHOLD,
            "storm_window_s": STORM_WINDOW,
            "storm_mute_s": STORM_MUTE,
            "key": key,
            "message": msg, "origin": origin, "claimed": session,
            "note": "no channel was contacted and no ledger row was written",
        }))
        .into_response();
    }

    match action {
        AlertAction::Dedupe => {
            record_owner_alert(&state, &origin, &session, &msg, &reason, &Map::new(), true).await;
            return Json(json!({
                "ok": true, "deduped": true, "channels": {}, "message": msg,
                "origin": origin, "claimed": session, "provenance_mismatch": mismatch,
            }))
            .into_response();
        }
        AlertAction::Muted => {
            // Recorded, deliberately not delivered — the ledger still shows
            // every attempt, so suppression never hides the evidence.
            record_owner_alert(&state, &origin, &session, &msg, &reason, &Map::new(), true).await;
            tracing::warn!("[urgent-alert] STORM-MUTED key={key} origin={origin:?} msg={:?}", truncate(&msg, 80));
            return Json(json!({
                "ok": true, "deduped": true, "storm_muted": true, "channels": {},
                "message": msg, "origin": origin, "claimed": session,
                "provenance_mismatch": mismatch,
            }))
            .into_response();
        }
        AlertAction::StormNotice => {
            // One page saying it is storming, then silence; it carries where
            // to look rather than just repeating the alert.
            msg = format!(
                "STORM: this alert has fired {hist_len}x and is now MUTED for {}m. Original: {msg}\nFull history: GET /api/alert/owner",
                (STORM_MUTE as i64) / 60
            );
        }
        AlertAction::Send => {}
    }

    let home = amux_home();
    let mut out_channels = Map::new();
    // Track ACTUAL delivery, not the per-channel string. The CLI used to infer
    // "sent" from the presence of a channels map and printed "alert sent" + exit
    // 0 even when push reached zero subscriptions AND sms had no phone — a lying
    // fire alarm that swallowed two prod-security escalations (AMUX-3151/GCA-96).
    // The server already knows the truth (push_delivery_verdict Err, sms ok=false);
    // this makes it EXPLICIT in the response so no caller has to guess.
    let mut push_delivered = false;
    let mut sms_delivered = false;
    let mut email_delivered = false;
    if effective_env(&home, "AMUX_URGENT_PUSH").unwrap_or_else(|| "1".into()) != "0" {
        match channels.push(&state, &session, &msg).await {
            Ok(()) => {
                out_channels.insert("push".into(), json!("sent"));
                push_delivered = true;
            }
            Err(e) => {
                out_channels.insert("push".into(), json!(format!("error: {}", truncate(&e, 80))));
            }
        };
    } else {
        // Symmetric with the sms branch below: a channel that was switched off
        // must SAY it was switched off. Omitting the key made "disabled" and
        // "never attempted" identical on the wire (AMUX-2938).
        out_channels.insert("push".into(), json!("disabled (AMUX_URGENT_PUSH=0)"));
    }
    let phone = effective_env(&home, "AMUX_OWNER_PHONE").unwrap_or_default();
    let sms_enabled = effective_env(&home, "AMUX_URGENT_SMS").unwrap_or_else(|| "1".into()) != "0";
    // SAY WHY IT DID NOT TEXT, rather than omitting the key (AMUX-2938). The
    // response used to carry no `sms` field at all when either condition was
    // false, so "disabled on purpose", "no phone configured" and "the send was
    // never attempted" were the same bytes on the wire — and CLAUDE.md
    // documents the contract as {"channels":{"push":...,"sms":...}}, so a
    // caller checking `channels.sms` got None and could not tell.
    //
    // Both are OFF on this machine right now (AMUX_URGENT_SMS=0, and
    // AMUX_OWNER_PHONE present but empty), which is very likely a deliberate
    // response to the 38-SMS night of 2026-08-03 — see cmd_alert in the CLI.
    // Deliberate or not, the alarm must say so out loud.
    // The reason comes from the SAME gate /api/alert/config now reports, so
    // the config view and a real send can never name different truths.
    if let (false, Some(reason)) = sms_gate(&home) {
        out_channels.insert("sms".into(), json!(reason));
    }
    if sms_enabled && !phone.is_empty() {
        // Stamp the originating session so the owner sees WHICH session
        // raised the alarm (the push title already carries it).
        let sms_prefix = if !session.is_empty() {
            format!("amux URGENT [{session}]: ")
        } else {
            "amux URGENT: ".to_string()
        };
        let (ok, detail) = channels.sms(&phone, &format!("{sms_prefix}{}", msg.replace('\n', " — "))).await;
        sms_delivered = ok;
        out_channels.insert("sms".into(), json!(if ok { detail } else { format!("failed: {detail}") }));
    }
    // EMAIL (AMUX-3203): the channel that reaches the owner with no manual setup.
    // Destination = AMUX_OWNER_EMAIL if set, else the connected account's own
    // inbox (self-send). This is why the fire alarm no longer depends on a push
    // subscription he never made or a phone he cleared after the 38-SMS night.
    let email_enabled = effective_env(&home, "AMUX_URGENT_EMAIL").unwrap_or_else(|| "1".into()) != "0";
    let pinned_email = effective_env(&home, "AMUX_OWNER_EMAIL").filter(|e| !e.trim().is_empty());
    let owner_email = pinned_email.clone().or_else(|| {
        crate::integrations::email::connected_accounts_by_freshness_in(&home).into_iter().next()
    });
    // SAY WHICH INBOX, AND WHY (AMUX-3524). With AMUX_OWNER_EMAIL unset the
    // destination is "whichever connected account was refreshed most
    // recently" — so the SAME alert class lands in different inboxes over
    // time, and nothing anywhere said so. Measured over 7 days: 9 pages to
    // info@, 5 to a PERSONAL gmail, 1 to ethan@, which is most of why the
    // fire alarm felt like noise. The fallback stays (an alarm with no
    // configured address must still reach someone) but it is no longer
    // silent: the channels map carries the destination and the reason, and
    // an unpinned send WARNs where sweeps look.
    let owner_email_pinned = pinned_email.is_some();
    if !email_enabled {
        out_channels.insert("email".into(), json!("disabled (AMUX_URGENT_EMAIL=0)"));
    } else if owner_email.is_none() {
        out_channels.insert("email".into(), json!("no AMUX_OWNER_EMAIL and no connected Gmail account"));
    }
    if email_enabled {
        if let Some(to) = owner_email.as_deref() {
            let subject = if session.is_empty() {
                "amux URGENT".to_string()
            } else {
                format!("amux URGENT [{session}]")
            };
            let (ok, detail) = channels.email(to, &subject, &msg).await;
            email_delivered = ok;
            if !owner_email_pinned {
                tracing::warn!(
                    destination = %to,
                    "[urgent-alert] AMUX_OWNER_EMAIL is unset — paged the most recently \
                     refreshed connected account, so this alert class moves between inboxes. \
                     Pin one address in server.env (AMUX-3524)."
                );
            }
            let how = if owner_email_pinned { "AMUX_OWNER_EMAIL" } else { "UNPINNED: freshest connected account (set AMUX_OWNER_EMAIL)" };
            out_channels.insert(
                "email".into(),
                json!(if ok { format!("{detail} -> {to} [{how}]") } else { format!("failed: {detail}") }),
            );
        }
    }
    record_owner_alert(&state, &origin, &session, &msg, &reason, &out_channels, false).await;
    let delivered = (push_delivered as u8 + sms_delivered as u8 + email_delivered as u8) as i64;
    let delivered_any = delivered > 0;
    if delivered_any {
        tracing::info!(
            "[urgent-alert] DELIVERED ({delivered} channel(s)) origin={origin:?} claimed={session:?} reason={reason:?} channels={:?} msg={:?}",
            out_channels,
            truncate(message, 120)
        );
    } else {
        // TWO-FIXES (AMUX-3151/GCA-96): the fire alarm reached NOBODY. This was an
        // info! line — byte-identical to a delivered page in a log sweep — so an
        // escalation nobody received left no distinct trace. A WARN makes
        // "the owner was NOT paged" greppable, which is the entire point of an
        // alarm that must not fail silently. grep "reached ZERO channels".
        tracing::warn!(
            "[urgent-alert] reached ZERO channels — the owner was NOT paged. origin={origin:?} claimed={session:?} reason={reason:?} channels={:?} msg={:?}",
            out_channels,
            truncate(message, 120)
        );
    }
    let mut resp = json!({
        // `ok` = the request was processed; `delivered_any` = a human was actually
        // paged. They differ exactly in the case this fixes, so the CLI keys off
        // delivered_any, never `ok`.
        "ok": true, "channels": out_channels, "message": msg,
        "origin": origin, "claimed": session, "provenance_mismatch": mismatch,
        "delivered": delivered, "delivered_any": delivered_any,
    });
    if !delivered_any {
        resp["fallback"] = json!(
            "no channel delivered — the owner was NOT paged; post to the board and say so in your turn output"
        );
    }
    Json(resp).into_response()
}

// ---- GET /api/alert/owner ---------------------------------------------------

#[derive(Deserialize)]
pub struct LedgerQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    limit: String,
}

async fn get_owner_ledger(State(state): State<AppState>, Query(qp): Query<LedgerQuery>) -> Response {
    let q = qp.q.trim().to_lowercase();
    // Python: int() failure falls back to 50, then clamps to 1..=500.
    let limit = qp.limit.parse::<i64>().unwrap_or(50).clamp(1, 500) as usize;
    let fetch = if q.is_empty() { limit } else { limit * 4 };
    let store = state.store.clone();
    #[allow(clippy::type_complexity)] // one owner_alerts row, tuple-shaped
    let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<(i64, i64, String, String, String, String, String, i64)>> {
        let conn = store.read()?;
        let mut stmt = conn.prepare(
            "SELECT id, ts, origin, claimed, message, reason, channels, deduped
             FROM owner_alerts ORDER BY ts DESC LIMIT ?1",
        )?;
        let out = stmt
            .query_map([fetch as i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(out)
    })
    .await;
    let rows = match rows {
        Ok(Ok(rows)) => rows,
        Ok(Err(e)) => return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })),
    };
    let mut out = Vec::new();
    for (id, ts, origin, claimed, message, reason, channels, deduped) in rows {
        if !q.is_empty() && !message.to_lowercase().contains(&q) && !reason.to_lowercase().contains(&q) {
            continue;
        }
        let ch: Value = serde_json::from_str(&channels).unwrap_or_else(|_| json!({}));
        out.push(json!({
            "id": id, "ts": ts, "origin": origin, "claimed": claimed,
            "message": message, "reason": reason, "channels": ch,
            "deduped": deduped != 0,
            "provenance_mismatch": !origin.is_empty() && !claimed.is_empty() && origin != claimed,
        }));
        if out.len() >= limit {
            break;
        }
    }
    Json(json!({
        "alerts": out, "count": out.len(),
        "query": if q.is_empty() { Value::Null } else { Value::String(q) },
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Tests — channels are ALWAYS mocked; no push, no osascript, no network.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// AMUX-3492 — the breaker's whole value is at its three boundaries: a
    /// closed breaker must send (or every alert silently loses a channel), a
    /// fresh wall must skip WITH the reason and retry time (a bare skip is
    /// AMUX-2938's indistinguishable-absence bug), and an expired cooldown
    /// must probe again (or a granted permission never resumes).
    #[test]
    fn imessage_breaker_skips_only_inside_the_cooldown_and_says_why() {
        // Closed (never walled): send.
        assert_eq!(imessage_breaker_verdict(0, 1_000_000, 900), None);
        // Walled 10s ago, 900s cooldown: skip, naming elapsed and remaining.
        let skip = imessage_breaker_verdict(1_000_000, 1_000_010, 900).expect("must skip");
        assert!(skip.contains("10s ago"), "elapsed missing: {skip}");
        assert!(skip.contains("890s"), "retry-remaining missing: {skip}");
        assert!(skip.contains("Automation permission"), "remedy missing: {skip}");
        // Cooldown exactly over, and past it: probe again.
        assert_eq!(imessage_breaker_verdict(1_000_000, 1_000_900, 900), None);
        assert_eq!(imessage_breaker_verdict(1_000_000, 1_001_000, 900), None);
        // Clock skew (wall ts in the future): treat as fresh, skip — never
        // underflow into a probe storm.
        assert!(imessage_breaker_verdict(1_000_100, 1_000_000, 900).is_some());
    }

    /// AMUX-3500 — the wall stamp must survive a process restart, because the
    /// builder restarts this server on every landed commit and an in-process
    /// breaker was measurably inert (every alert was a fresh process paying
    /// the 12s probe). File semantics: absent = 0 (probe), stamped = now,
    /// cleared = 0 again — and a stamp in home A is invisible from home B
    /// (perf harnesses boot temp homes; their probes must not inherit the
    /// real machine's wall).
    #[test]
    fn imessage_wall_stamp_survives_what_a_process_restart_destroys() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(imessage_wall_stamp_ts(dir.path()), 0, "absent stamp = probe");
        stamp_imessage_wall(dir.path());
        let now = crate::config::now_f64() as u64;
        let ts = imessage_wall_stamp_ts(dir.path());
        assert!(ts > 0 && now.abs_diff(ts) <= 5, "stamp must read ≈now, got {ts} vs {now}");
        let other = tempfile::tempdir().unwrap();
        assert_eq!(imessage_wall_stamp_ts(other.path()), 0, "stamps are per-home");
        clear_imessage_wall(dir.path());
        assert_eq!(imessage_wall_stamp_ts(dir.path()), 0, "cleared stamp = probe again");
    }

    /// The config view and the send path share one SMS gate (found verifying
    /// AC-362: config said "sms": true in exactly the state where a real send
    /// reports "no phone configured"). Every case writes BOTH keys into the
    /// file because effective_env is file-first and this machine's process env
    /// carries its own values — an absent key would make the test read the
    /// host, not the fixture.
    #[test]
    fn sms_gate_matches_the_send_paths_own_predicate() {
        let dir = tempfile::tempdir().unwrap();
        let env = |sms: &str, phone: &str| {
            std::fs::write(
                dir.path().join("server.env"),
                format!("AMUX_URGENT_SMS={sms}\nAMUX_OWNER_PHONE={phone}\n"),
            )
            .unwrap();
        };
        env("0", "+15551234567");
        assert_eq!(sms_gate(dir.path()), (false, Some("disabled (AMUX_URGENT_SMS=0)")));
        env("1", "");
        assert_eq!(
            sms_gate(dir.path()),
            (false, Some("no phone configured (AMUX_OWNER_PHONE is empty)"))
        );
        env("1", "+15551234567");
        assert_eq!(sms_gate(dir.path()), (true, None));
    }

    /// The fire-alarm honesty rule (AC-347 / mixpeek-finances). "sent" is
    /// legitimate ONLY when an endpoint accepted the push (2xx). The whole
    /// incident was "sent" reported for a page nobody received — this pins that
    /// it can't happen again for either shape: zero subs, or subs-all-rejected.
    #[test]
    fn push_is_delivered_only_when_an_endpoint_accepts_never_on_zero_or_all_rejected() {
        let row = |status: u64| json!({"host": "push.example", "status": status, "detail": "x"});

        // ≥1 endpoint accepted (2xx) -> the ONLY legitimate "sent".
        assert!(push_delivery_verdict(&[row(201), row(410)]).is_ok(), "one 2xx is a real delivery");
        assert!(push_delivery_verdict(&[row(200)]).is_ok());

        // ZERO subscriptions -> not a send (AMUX-2938).
        let e = push_delivery_verdict(&[]).unwrap_err();
        assert!(e.contains("no push subscriptions"), "{e}");

        // Subscriptions EXIST but every endpoint rejected (410 Gone / expired)
        // -> the gap MF-582 doesn't close. Must be an error, not "sent".
        let e = push_delivery_verdict(&[row(410), row(404), row(500)]).unwrap_err();
        assert!(e.contains("not delivered") && e.contains("0 accepted"), "all-rejected must not be sent: {e}");

        // Broadcast couldn't be attempted (vapid/db) -> the synthetic error row.
        let e = push_delivery_verdict(&[json!({"host":"","status":0,"detail":"vapid: bad key"})]).unwrap_err();
        assert!(e.starts_with("vapid:"), "{e}");

        // The negative control that proves the check bites: a non-2xx-only set
        // must NEVER read as delivered, however many rows it has.
        assert!(push_delivery_verdict(&[row(429), row(429)]).is_err(), "'we called send' is not delivery");
    }

    /// MF-427. The guard was correct and its STATE was fiction: the in-memory
    /// maps are rebuilt by `routes()` and this process re-execs on every commit
    /// the auto-builder adopts. Measured on the live ledger: 81 alerts, 0 ever
    /// deduped, including a 38x `--help` storm at a ~302s cadence that
    /// STORM_THRESHOLD=2 over 1800s should have muted after the second.
    ///
    /// This replays that storm through the pure decision to show the guard
    /// stops it WHEN IT REMEMBERS — so the defect is provably the persistence,
    /// not the policy.
    #[test]
    fn the_302s_storm_is_muted_once_history_survives_a_restart() {
        let mut hist: Vec<f64> = vec![];
        let mut mute = 0.0;
        let mut last: Option<f64> = None;
        let mut sent = 0;
        let mut suppressed = 0;
        let t0 = 1_000_000.0;

        for i in 0..38 {
            let now = t0 + (i as f64) * 302.0;
            let (action, h, m) = urgent_alert_decision(now, &hist, mute, last);
            hist = h;
            mute = m;
            match action {
                AlertAction::Send | AlertAction::StormNotice => { sent += 1; last = Some(now); }
                _ => suppressed += 1,
            }
        }
        // 38 attempts, and the owner is paged a handful of times, not 38.
        assert!(sent < 10, "38 identical alerts paged the owner {sent} times — the guard did not hold");
        assert_eq!(sent + suppressed, 38);

        // THE CONTROL, and the actual bug: reset the state between every alert,
        // which is exactly what a restart does. Every one gets through.
        let mut sent_amnesiac = 0;
        for i in 0..38 {
            let now = t0 + (i as f64) * 302.0;
            let (action, _, _) = urgent_alert_decision(now, &[], 0.0, None);
            if matches!(action, AlertAction::Send | AlertAction::StormNotice) { sent_amnesiac += 1; }
        }
        assert_eq!(
            sent_amnesiac, 38,
            "with state reset each time every alert is delivered — this is what the ledger fix prevents"
        );
    }

    use crate::api::settings::test_env;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    /// Recording mock. `push_result`/`sms_result` steer failure-path tests.
    struct MockChannels {
        pushes: Mutex<Vec<(String, String)>>,
        smses: Mutex<Vec<(String, String)>>,
        emails: Mutex<Vec<(String, String, String)>>,
        push_result: Result<(), String>,
        sms_result: (bool, String),
        email_result: (bool, String),
    }

    impl MockChannels {
        fn ok() -> Arc<Self> {
            Arc::new(Self {
                pushes: Mutex::new(vec![]),
                smses: Mutex::new(vec![]),
                emails: Mutex::new(vec![]),
                push_result: Ok(()),
                sms_result: (true, "imessage".into()),
                email_result: (true, "email via ethan@example.com".into()),
            })
        }
    }

    #[async_trait]
    impl AlertChannels for MockChannels {
        async fn push(&self, _state: &AppState, session: &str, message: &str) -> Result<(), String> {
            self.pushes.lock().unwrap().push((session.to_string(), message.to_string()));
            self.push_result.clone()
        }
        async fn sms(&self, phone: &str, text: &str) -> (bool, String) {
            self.smses.lock().unwrap().push((phone.to_string(), text.to_string()));
            self.sms_result.clone()
        }
        async fn email(&self, to: &str, subject: &str, body: &str) -> (bool, String) {
            self.emails.lock().unwrap().push((to.to_string(), subject.to_string(), body.to_string()));
            self.email_result.clone()
        }
    }

    fn app(channels: Arc<MockChannels>) -> axum::Router {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("alerts-test.db")).unwrap();
        std::mem::forget(dir);
        let state = AppState {
            store: Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
        Router::new().nest("/api/alert", routes_with(channels)).with_state(state)
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let req = match body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, v)
    }

    // ---- config ----------------------------------------------------------

    #[tokio::test]
    async fn config_defaults_patch_and_provider_detection() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        let app = app(MockChannels::ok());

        // Python defaults: no phone, both channels on, imessage provider —
        // and the toggle-on/no-phone state must SAY sms is not deliverable
        // (the AC-362 residual: "sms": true beside phone "" read as "a send
        // will text" when a real send reports "no phone configured").
        let (st, v) = send(&app, "GET", "/api/alert/config", &[], None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            v,
            json!({
                "phone": "", "push": true, "sms": true,
                "sms_deliverable": false,
                "sms_blocked_reason": "no phone configured (AMUX_OWNER_PHONE is empty)",
                "sms_provider": "imessage"
            })
        );

        // PATCH applies only the keys present in the body.
        let (st, v) = send(
            &app,
            "PATCH",
            "/api/alert/config",
            &[],
            Some(json!({ "phone": " +15551234567 ", "push": false })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v, json!({ "ok": true }));
        let env = std::fs::read_to_string(dir.path().join("server.env")).unwrap();
        assert!(env.contains("AMUX_OWNER_PHONE=+15551234567"), "{env}");
        assert!(env.contains("AMUX_URGENT_PUSH=0"), "{env}");
        assert!(!env.contains("AMUX_URGENT_SMS"), "absent key untouched: {env}");

        let (_, v) = send(&app, "GET", "/api/alert/config", &[], None).await;
        assert_eq!(v["phone"], json!("+15551234567"));
        assert_eq!(v["push"], json!(false));
        assert_eq!(v["sms"], json!(true));

        // Twilio creds flip the reported provider.
        set_server_env_key(dir.path(), "TWILIO_ACCOUNT_SID", "AC123").unwrap();
        let (_, v) = send(&app, "GET", "/api/alert/config", &[], None).await;
        assert_eq!(v["sms_provider"], json!("twilio"));
    }

    // ---- owner alert ------------------------------------------------------

    #[tokio::test]
    async fn owner_alert_full_send_shape_channels_and_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        set_server_env_key(dir.path(), "AMUX_OWNER_PHONE", "+15550000001").unwrap();
        let mock = MockChannels::ok();
        let app = app(mock.clone());

        let (st, v) = send(
            &app,
            "POST",
            "/api/alert/owner",
            &[("x-amux-session", "sender-a")],
            Some(json!({ "message": "prod is down", "session": "sender-a", "reason": "deploy failed" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        // The CLAUDE.md contract shape.
        assert_eq!(v["ok"], json!(true));
        // Email is a channel now (AMUX-3203), but the temp home has no connected
        // Gmail account and AMUX_OWNER_EMAIL is unset, so it reports why rather
        // than sending — push + sms are unchanged.
        // THE FAILURE MESSAGE MUST NAME THE HOME IT ACTUALLY READ (AMUX-3719).
        //
        // Observed failing once in 4 full-suite runs on 2026-08-25 with
        // `email: "email via ethan@example.com -> pinned@example.com
        // [AMUX_OWNER_EMAIL]"` — a value that can only come from ANOTHER test's
        // fixture home (a_pinned_owner_email_is_used_and_reported_as_pinned
        // writes that pin into its own tempdir). Not reproducible: passes alone,
        // passes as a module, passed three consecutive full runs afterwards.
        //
        // The mechanism could not be determined from the failure output, because
        // the assertion printed the rendered channels and nothing about WHICH
        // home produced them — so the one datum that separates "the guard leaked"
        // from "this test resolved a different home" was absent from the only
        // artifact the flake leaves behind. That missing datum is the bug worth
        // fixing here (ethos rule 4); the cause is still open on AMUX-3719.
        assert_eq!(
            v["channels"],
            json!({
                "push": "sent",
                "sms": "imessage",
                "email": "no AMUX_OWNER_EMAIL and no connected Gmail account",
            }),
            "fixture home = {:?}\nAMUX_HOME  = {:?}\nresolved   = {:?}\n\
             If `resolved` is not `fixture home`, this is the AMUX-3719 cross-test \
             home leak and NOT a defect in the alert path — report the three paths \
             above on that card rather than re-deriving them.",
            dir.path(),
            std::env::var("AMUX_HOME").ok(),
            crate::config::amux_home(),
        );
        assert_eq!(v["message"], json!("prod is down\n(deploy failed)"));
        assert_eq!(v["origin"], json!("sender-a"));
        assert_eq!(v["claimed"], json!("sender-a"));
        assert_eq!(v["provenance_mismatch"], json!(false));

        // Channel senders received Python's exact payloads.
        let pushes = mock.pushes.lock().unwrap().clone();
        assert_eq!(pushes, vec![("sender-a".into(), "prod is down\n(deploy failed)".into())]);
        let smses = mock.smses.lock().unwrap().clone();
        assert_eq!(
            smses,
            vec![("+15550000001".into(), "amux URGENT [sender-a]: prod is down — (deploy failed)".into())]
        );

        // The ledger recorded the attempt with parsed channels.
        let (_, l) = send(&app, "GET", "/api/alert/owner", &[], None).await;
        assert_eq!(l["count"], json!(1));
        assert_eq!(l["query"], Value::Null);
        let row = &l["alerts"][0];
        assert_eq!(row["origin"], json!("sender-a"));
        assert_eq!(row["claimed"], json!("sender-a"));
        assert_eq!(row["channels"]["sms"], json!("imessage"));
        assert_eq!(row["deduped"], json!(false));
        assert_eq!(row["provenance_mismatch"], json!(false));
    }

    #[tokio::test]
    async fn owner_alert_60s_dedupe_and_ledger_visibility() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        let mock = MockChannels::ok();
        let app = app(mock.clone());

        let body = json!({ "message": "db replica lagging", "session": "s1" });
        let (_, first) = send(&app, "POST", "/api/alert/owner", &[], Some(body.clone())).await;
        assert_eq!(first["ok"], json!(true));
        assert!(first.get("deduped").is_none());

        let (st, second) = send(&app, "POST", "/api/alert/owner", &[], Some(body)).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(second["deduped"], json!(true));
        assert_eq!(second["channels"], json!({}));
        // No second delivery attempt reached any channel.
        assert_eq!(mock.pushes.lock().unwrap().len(), 1);
        // But the ledger shows BOTH attempts (suppression never hides evidence).
        let (_, l) = send(&app, "GET", "/api/alert/owner", &[], None).await;
        assert_eq!(l["count"], json!(2));
        let dedup_rows = l["alerts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["deduped"] == json!(true))
            .count();
        assert_eq!(dedup_rows, 1);
    }

    #[tokio::test]
    async fn owner_alert_provenance_mismatch_is_flagged_not_believed() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        let app = app(MockChannels::ok());

        let (_, v) = send(
            &app,
            "POST",
            "/api/alert/owner",
            &[("x-amux-session", "actual-sender")],
            Some(json!({ "message": "disk filling on shared host", "session": "claimed-other" })),
        )
        .await;
        assert_eq!(v["origin"], json!("actual-sender"));
        assert_eq!(v["claimed"], json!("claimed-other"));
        assert_eq!(v["provenance_mismatch"], json!(true));
        let (_, l) = send(&app, "GET", "/api/alert/owner", &[], None).await;
        assert_eq!(l["alerts"][0]["provenance_mismatch"], json!(true));
    }

    #[tokio::test]
    async fn owner_alert_refuses_junk_messages_without_delivering() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        let mock = MockChannels::ok();
        let app = app(mock.clone());

        for junk in ["--help", "", "  ", "TEST", "help"] {
            let (st, v) =
                send(&app, "POST", "/api/alert/owner", &[], Some(json!({ "message": junk }))).await;
            assert_eq!(st, StatusCode::OK);
            assert_eq!(v["sent"], json!(false), "{junk:?} must be refused");
            assert!(v["error"].as_str().unwrap().starts_with("refused: "), "{v}");
        }
        assert_eq!(v_len(&mock), 0, "no junk message may reach a channel");
        // Python returns before recording: the refusals leave no ledger rows.
        let (_, l) = send(&app, "GET", "/api/alert/owner", &[], None).await;
        assert_eq!(l["count"], json!(0));
    }

    fn v_len(mock: &MockChannels) -> usize {
        mock.pushes.lock().unwrap().len()
            + mock.smses.lock().unwrap().len()
            + mock.emails.lock().unwrap().len()
    }

    #[tokio::test]
    async fn owner_alert_respects_channel_config() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        // Push disabled, no phone: nothing is attempted, and the response SAYS
        // so per channel. This assertion used to be `json!({})` — an empty
        // object — directly under a comment claiming "response says so", which
        // it plainly did not (AMUX-2938). The comment described the intent; the
        // assertion locked in the opposite, and a caller could not distinguish
        // "disabled on purpose" from "the alarm silently did nothing".
        set_server_env_key(dir.path(), "AMUX_URGENT_PUSH", "0").unwrap();
        let mock = MockChannels::ok();
        let app = app(mock.clone());
        let (_, v) =
            send(&app, "POST", "/api/alert/owner", &[], Some(json!({ "message": "no channels case" }))).await;
        assert_eq!(v["ok"], json!(true));
        assert_eq!(
            v["channels"],
            json!({
                "push": "disabled (AMUX_URGENT_PUSH=0)",
                "sms": "no phone configured (AMUX_OWNER_PHONE is empty)",
                "email": "no AMUX_OWNER_EMAIL and no connected Gmail account",
            })
        );
        assert_eq!(v_len(&mock), 0);
    }

    /// AMUX-3203: the email channel is what makes the alarm reach the owner with
    /// NO manual setup. In the incident's exact config (no push subscription, no
    /// phone) the alert still DELIVERS via email and counts toward delivered_any,
    /// as long as a connected Gmail account exists — its own inbox is the default
    /// destination. This is the gap that dropped five real pages.
    #[tokio::test]
    async fn owner_alert_delivers_via_email_when_only_a_gmail_account_exists() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        // No push subs, no phone — the incident. But a connected Gmail account:
        std::fs::create_dir_all(dir.path().join("gmail-tokens")).unwrap();
        std::fs::write(dir.path().join("gmail-tokens/ethan@example.com.json"), "{}").unwrap();
        set_server_env_key(dir.path(), "AMUX_URGENT_PUSH", "0").unwrap();
        let mock = MockChannels::ok();
        let app = app(mock.clone());
        let (_, v) = send(
            &app,
            "POST",
            "/api/alert/owner",
            &[],
            Some(json!({ "message": "prod down, only email left" })),
        )
        .await;
        assert_eq!(v["delivered_any"], json!(true), "email must deliver the page: {v}");
        assert!(v.get("fallback").is_none(), "a delivered page must not carry the miss fallback");
        // AMUX-3524: an UNPINNED destination must SAY it is unpinned and name
        // the inbox it chose. Silence here is what let one alert class scatter
        // across three of Ethan's inboxes for a week unnoticed.
        let ch = v["channels"]["email"].as_str().unwrap_or_default();
        assert!(ch.contains("ethan@example.com"), "the channel must name the destination: {ch}");
        assert!(ch.contains("UNPINNED"), "an unset AMUX_OWNER_EMAIL must be stated: {ch}");
        // The mock received the self-send to the connected account.
        let emails = mock.emails.lock().unwrap().clone();
        assert_eq!(emails.len(), 1, "exactly one email attempt");
        assert_eq!(emails[0].0, "ethan@example.com", "owner email defaults to the connected account");
    }

    /// AMUX-3524 CONTROL — with AMUX_OWNER_EMAIL set, the destination is that
    /// address and the channel says it was PINNED. Without this cell the
    /// assertion above passes for a server that shouts UNPINNED unconditionally.
    #[tokio::test]
    async fn a_pinned_owner_email_is_used_and_reported_as_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        std::fs::create_dir_all(dir.path().join("gmail-tokens")).unwrap();
        // A connected account that is NOT the pinned address: if the pin were
        // ignored, the send would go here and the test would catch it.
        std::fs::write(dir.path().join("gmail-tokens/info@example.com.json"), "{}").unwrap();
        set_server_env_key(dir.path(), "AMUX_URGENT_PUSH", "0").unwrap();
        set_server_env_key(dir.path(), "AMUX_OWNER_EMAIL", "pinned@example.com").unwrap();
        let mock = MockChannels::ok();
        let app = app(mock.clone());
        let (_, v) = send(
            &app,
            "POST",
            "/api/alert/owner",
            &[],
            Some(json!({ "message": "pinned destination page" })),
        )
        .await;
        let ch = v["channels"]["email"].as_str().unwrap_or_default();
        assert!(ch.contains("pinned@example.com"), "must page the PINNED address: {ch}");
        assert!(ch.contains("AMUX_OWNER_EMAIL"), "must report the pin as the source: {ch}");
        assert!(!ch.contains("UNPINNED"), "a pinned send must not warn unpinned: {ch}");
        let emails = mock.emails.lock().unwrap().clone();
        assert_eq!(emails[0].0, "pinned@example.com", "the pin, not the connected account");
    }

    /// A FIRE ALARM MUST NOT REPORT SUCCESS HAVING REACHED NOBODY (AMUX-2938).
    ///
    /// Measured on the live machine 2026-08-11, while Ethan was asking whether
    /// the urgent path works: `push_subscriptions` held 0 rows and a real
    /// POST /api/alert/owner answered `{"channels":{"push":"sent"}}`. `send_all`
    /// returns an empty vec with no subscribers, which fell through every
    /// failure branch and landed on Ok(()).
    ///
    /// This is the only endpoint whose entire job is reaching a human who is
    /// not looking at the screen, and it said "sent" to nobody — while
    /// CLAUDE.md tells every session both channels are "wired and confirmed
    /// working".
    #[tokio::test]
    async fn push_with_no_subscribers_is_not_a_send() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        // RealChannels, not the mock: the defect lived in RealChannels::push's
        // reading of send_all, so a mocked push would prove nothing.
        let store = crate::db::Store::open(&dir.path().join("push-test.db")).unwrap();
        let state = AppState {
            store: Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
        let out = RealChannels.push(&state, "amux", "drill").await;
        assert!(
            out.is_err(),
            "0 subscriptions must NOT report a send — got {out:?}"
        );
        let e = out.unwrap_err();
        assert!(
            e.contains("no push subscriptions"),
            "the reason must name the cause, got {e:?}"
        );
    }

    #[tokio::test]
    async fn owner_alert_reports_channel_failures_per_contract() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        set_server_env_key(dir.path(), "AMUX_OWNER_PHONE", "+15550000002").unwrap();
        let mock = Arc::new(MockChannels {
            pushes: Mutex::new(vec![]),
            smses: Mutex::new(vec![]),
            emails: Mutex::new(vec![]),
            push_result: Err("vapid: unreadable key".into()),
            sms_result: (false, "imessage error: -1743".into()),
            email_result: (false, "email error: not_connected".into()),
        });
        let app = app(mock);
        let (_, v) =
            send(&app, "POST", "/api/alert/owner", &[], Some(json!({ "message": "both channels fail" }))).await;
        // Failed channels are REPORTED, not hidden — Python's exact spellings.
        assert_eq!(v["channels"]["push"], json!("error: vapid: unreadable key"));
        assert_eq!(v["channels"]["sms"], json!("failed: imessage error: -1743"));
        assert_eq!(v["ok"], json!(true));
        // AMUX-3151/GCA-96: `ok:true` is "request processed", NOT "owner paged".
        // With BOTH channels failed the response must say so explicitly, or the
        // CLI reports a swallowed escalation as a delivered page.
        assert_eq!(v["delivered_any"], json!(false), "both channels failed → delivered_any must be false");
        assert_eq!(v["delivered"], json!(0));
        assert!(v["fallback"].is_string(), "a zero-delivery response must carry the board fallback");
    }

    /// The positive half: when a channel ACTUALLY delivers, delivered_any is true
    /// and no fallback is emitted — so the CLI's non-zero exit fires ONLY on a
    /// real miss, never on a working page.
    #[tokio::test]
    async fn owner_alert_reports_delivered_when_a_channel_lands() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        set_server_env_key(dir.path(), "AMUX_OWNER_PHONE", "+15550000003").unwrap();
        let mock = Arc::new(MockChannels {
            pushes: Mutex::new(vec![]),
            smses: Mutex::new(vec![]),
            emails: Mutex::new(vec![]),
            push_result: Ok(()),
            sms_result: (true, "imessage".into()),
            email_result: (true, "email via a@b.com".into()),
        });
        let app = app(mock);
        let (_, v) =
            send(&app, "POST", "/api/alert/owner", &[], Some(json!({ "message": "a real page" }))).await;
        assert_eq!(v["delivered_any"], json!(true));
        // push + sms landed; email is not attempted here (no connected account in
        // the temp home and no AMUX_OWNER_EMAIL), so the tally is 2, not 3.
        assert_eq!(v["delivered"], json!(2), "push + sms both landed");
        assert!(v.get("fallback").is_none(), "a delivered page must NOT carry the failure fallback");
    }

    #[tokio::test]
    async fn ledger_query_and_limit_filter() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        let app = app(MockChannels::ok());
        for (i, msg) in ["alpha incident", "beta incident", "gamma routine"].iter().enumerate() {
            let (_, v) = send(
                &app,
                "POST",
                "/api/alert/owner",
                &[],
                Some(json!({ "message": msg, "session": format!("s{i}") })),
            )
            .await;
            assert_eq!(v["ok"], json!(true), "{v}");
        }
        // Substring filter over message/reason, lowercased.
        let (_, l) = send(&app, "GET", "/api/alert/owner?q=INCIDENT", &[], None).await;
        assert_eq!(l["count"], json!(2));
        assert_eq!(l["query"], json!("incident"));
        // The filter EXCLUDED something (ethos rule 7 negative control).
        let (_, all) = send(&app, "GET", "/api/alert/owner", &[], None).await;
        assert_eq!(all["count"], json!(3));
        // Limit clamps.
        let (_, one) = send(&app, "GET", "/api/alert/owner?limit=1", &[], None).await;
        assert_eq!(one["count"], json!(1));
    }

    // ---- pure decision function (the storm replay) ------------------------

    #[test]
    fn storm_decision_replays_the_302s_cadence_incident() {
        // The MF-427 storm: identical pages every ~302s. The 60s dedupe
        // steps cleanly between windows; the storm guard must not.
        let (a1, h1, m1) = urgent_alert_decision(0.0, &[], 0.0, None);
        assert_eq!(a1, AlertAction::Send);
        assert_eq!(m1, 0.0);
        // Second identical send at t=302: threshold (2) crossed -> ONE storm
        // notice, then mute.
        let (a2, h2, m2) = urgent_alert_decision(302.0, &h1, m1, Some(0.0));
        assert_eq!(a2, AlertAction::StormNotice);
        assert_eq!(h2.len(), 2);
        assert_eq!(m2, 302.0 + STORM_MUTE);
        // Third at t=604: muted, and the mute SLIDES outward.
        let (a3, _h3, m3) = urgent_alert_decision(604.0, &h2, m2, Some(302.0));
        assert_eq!(a3, AlertAction::Muted);
        assert_eq!(m3, 604.0 + STORM_MUTE);
    }

    #[test]
    fn decision_dedupe_window_and_epoch_zero_semantics() {
        // Inside 60s of a real send: dedupe.
        let (a, _, _) = urgent_alert_decision(100.0, &[70.0], 0.0, Some(70.0));
        assert_eq!(a, AlertAction::Dedupe);
        // dedupe_last of 0.0 means NEVER SENT, not "sent at epoch" (the
        // Python fix this signature exists for).
        let (a, _, _) = urgent_alert_decision(30.0, &[], 0.0, Some(0.0));
        assert_eq!(a, AlertAction::Send);
        // Distinct messages have distinct keys and fresh histories: a fresh
        // history never storms (the negative control is part of the design).
        let (a, h, m) = urgent_alert_decision(1000.0, &[], 0.0, None);
        assert_eq!((a, h.len(), m), (AlertAction::Send, 1, 0.0));
        // Old history outside the 30m window ages out instead of storming.
        let (a, h, _) = urgent_alert_decision(5000.0, &[100.0, 200.0], 0.0, Some(200.0));
        assert_eq!(a, AlertAction::Send);
        assert_eq!(h, vec![5000.0]);
    }

    #[test]
    fn py_repr_matches_python_for_the_refusal_strings() {
        assert_eq!(py_repr("--help"), "'--help'");
        assert_eq!(py_repr(""), "''");
        assert_eq!(py_repr("it's"), r"'it\'s'");
    }
}
