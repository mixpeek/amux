//! The daily decision digest (Z15).
//!
//! Owner's ask: "Daily digest: each needsyou card with its recommendation and
//! a date, taking effect unless vetoed. Money, outside sends and customer data
//! never auto-apply."
//!
//! Spec: mixpeek research/fleet-audits/2026-11-01-no-baggage-plan.md, Z15
//! "Daily digest with default-on", and that plan's limits line.
//!
//! Once a local day the owner's inbox and web push get every open needsyou
//! card, each labelled with one of: the date it takes effect, the date it
//! would take effect (opted out), needs a recommendation, or never
//! auto-applies and why. Apply is ON by default. `AMUX_DECISION_DIGEST_APPLY=0`
//! at worker, group or global scope, or in server.env for the whole fleet, is
//! the off switch, and that scope then runs as a dry run.
//!
//! # Classification fails closed
//!
//! [`classify_view`] returns `Safe` only through one path, and every other
//! path keeps the card with the owner. The positive evidence is structured:
//! `ask_type` is exactly `decision`, the card was filed by a worker
//! (`source = agent`), it has a recommendation, and its worker declared the
//! closed-vocabulary scope [`SAFE_SCOPE`]. The text checks can only refuse:
//! the absence of a keyword proves nothing, so a clean text with no
//! declaration is `Unclassified`.
//!
//! # Timeline
//!
//! A date counts only once a delivered digest published it while apply was on
//! for that worker; a date shown while opted out is never a promise. First publication sets
//! the effect date at least two days out, a second delivered digest has to
//! follow, and application needs that day's digest delivered and
//! [`APPLY_GRACE_HOURS`] elapsed since it was actually sent, so a digest that
//! went out late still gets the full window. A date can move later, never
//! earlier.
//!
//! # Stops
//!
//! A veto (`POST /api/decision-digest/veto`, the `#digest-veto=<card>` link in
//! the digest) and any owner note on the card since publication are final for
//! that card. A worker's edit, a status change or a reclassification restarts
//! the clock, so what applies is always what the owner read.
//!
//! # Application
//!
//! The recommendation goes to the card's worker through the steering queue
//! with guard [`ORIGIN`], which is automation, so the chokepoint refuses
//! isolated, board-off and paused workers on its own. The text and the board
//! log both say it was a digest default.
//!
//! # Crash recovery
//!
//! `tracked -> applying` commits before anything is sent. The message id and
//! the card marker are stable, so a resumed `applying` row adopts a send that
//! already happened and finishes its receipt, and a row with no send re-runs
//! every gate first. `applied` is written only after the card reads back.
//!
//! Verdicts: decision_digest_sent, _undelivered, _unmeasured, _disabled,
//! _would_apply, _applying, _applied, _adopted, _apply_failed, _held, _vetoed,
//! _veto_refused, _stopped_owner, _reset, _recommendation_set,
//! _recommendation_refused, _recommendation_asked.

use super::needs_input_auto as nia;
use super::AppState;
use crate::api::alerts::AlertChannels;
use crate::config::now_f64;
use crate::db::board_store::{self as bs, IssueRow};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use chrono::{Datelike, NaiveDate, Timelike};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

pub const KILL_SWITCH: &str = "AMUX_DECISION_DIGEST";
pub const APPLY_KEY: &str = "AMUX_DECISION_DIGEST_APPLY";
/// The only scope a worker can declare that makes a card eligible.
pub const SAFE_SCOPE: &str = "internal_reversible";
/// Steering guard and origin of an application. Deliberately absent from
/// `is_owner_configured_guard`: this is automation, never owner input.
pub const ORIGIN: &str = "decision-digest";
pub const ACTOR: &str = "policy:decision-digest";
pub const APPROVER: &str = "policy:decision-digest (digest default; published to the owner, not vetoed)";
/// Stable closing literal; the dashboard classifies it as harness text.
pub const CLOSING: &str = "(Applied by digest default, not by the owner.)";
const STAMP_KEY: &str = "decision_digest_last_sent";
/// Epoch seconds of that digest: what the grace window is measured from.
const STAMP_AT_KEY: &str = "decision_digest_last_sent_at";
/// Hours between a day's digest being sent and that day's applications.
pub const APPLY_GRACE_HOURS: u32 = 8;
const MIN_WAIT_DAYS: i64 = 2;
const MAX_ATTEMPTS: i64 = 3;
/// Rows per section. Each section states its true count.
const LIST_CAP: usize = 25;
/// Recommendation requests per digest: each one costs a worker a turn.
const ASK_CAP: usize = 10;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/decision-digest", axum::routing::get(get_view))
        .route("/api/decision-digest/veto", axum::routing::post(veto_route))
        .route(
            "/api/decision-digest/recommendation",
            axum::routing::put(recommendation_route),
        )
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Class {
    /// Positively classified: this card can take effect.
    Safe,
    /// Nothing wrong with it, and nothing to apply yet.
    NeedsRecommendation,
    /// Touches the boundary or is reserved for the owner.
    Never(String),
    /// Not positively classified. Treated exactly like `Never`.
    Unclassified(String),
}

/// A hard-stop pattern. One that fails to compile is logged and then matches
/// everything: a stop that cannot run must refuse, never fall silent.
fn compile(kind: &str, pattern: &str) -> Option<regex::Regex> {
    regex::Regex::new(pattern)
        .inspect_err(|e| tracing::warn!(kind, error = %e, verdict = "decision_digest_pattern_invalid",
            "decision digest: a hard-stop pattern did not compile; it now refuses every card"))
        .ok()
}

fn hits(r: &Option<regex::Regex>, text: &str) -> bool {
    r.as_ref().is_none_or(|r| r.is_match(text))
}

/// Every text check that can only refuse. Finding nothing here is not evidence
/// of safety; [`classify_view`] gets that from the structured fields.
pub fn boundary_in_text(text: &str) -> Option<&'static str> {
    // The plan's limits, ahead of the general read so the digest names them.
    static LIMITS: std::sync::LazyLock<Vec<(&'static str, Option<regex::Regex>)>> = std::sync::LazyLock::new(|| {
        [
            ("tubescience_systems", r"\b(tubescience|ts-api)\b"),
            ("new_spend", r"\b(new|more|extra|additional) (spend|cost|charge)|\bspend\w*\b"),
            ("repo_gate", r"\b(new (endpoints?|primitives?))\b"),
            ("gate_loss", r"\b(remov\w*|disabl\w*|bypass\w*|skip\w*|drop\w*|turn\w* off)\b[^.?!]{0,40}\b(gates?|guards?|checks?)\b"),
        ]
        .into_iter()
        .map(|(k, p)| (k, compile(k, p)))
        .collect()
    });
    let lower = text.to_ascii_lowercase();
    if let Some((k, _)) = LIMITS.iter().find(|(_, r)| hits(r, &lower)) {
        return Some(k);
    }
    match super::needs_input::classify("", text).0 {
        "money" => return Some("money"),
        "outbound" => return Some("outside_send"),
        "prod_data" => return Some("customer_or_production_data"),
        _ => {}
    }
    static RULES: std::sync::LazyLock<Vec<(&'static str, Option<regex::Regex>)>> = std::sync::LazyLock::new(|| {
        [
            ("money", r"\$\s?\d|\b(invoice|billing|budget|price|pricing|quota|paid|cost)\b"),
            ("customer_or_production_data", r"\b(customers?|tenants?|clients?|prod|production|pii|user data)\b"),
            ("outside_send", r"\b(e-?mail|dm|newsletter|tweet|publish\w*|announce\w*|linkedin|press|external|outside the company|third[- ]party)\b"),
            ("destructive", r"\b(delete|deleting|drop|dropping|wipe|purge|truncate|erase|destroy|migrat\w*|overwrite|force[- ]push)\b"),
            ("irreversible", r"\b(irreversibl\w*|cannot be undone|can't be undone|permanent\w*|one[- ]way)\b"),
            ("repo_gate", r"\b(new (endpoints?|primitives?)|push\w* to main|foreign commits?)\b"),
        ]
        .into_iter()
        .map(|(k, p)| (k, compile(k, p)))
        .collect()
    });
    let t = text.to_ascii_lowercase();
    RULES.iter().find(|(_, r)| hits(r, &t)).map(|(k, _)| *k)
}

/// One card as the digest sees it.
#[derive(Debug, Clone)]
pub struct View {
    pub card: String,
    pub worker: String,
    pub item: Value,
    pub title: String,
    pub question: String,
    pub recommendation: String,
    pub scope: String,
    pub source: String,
    pub text: String,
    pub isolated: bool,
    pub snoozed: bool,
    pub fingerprint: String,
    pub log: String,
}

fn strict_policy() -> nia::Policy {
    nia::Policy { money: false, prod_data: false, outbound: false, send_back: false, ..nia::Policy::default() }
}

/// The needs-input hold rules, for one reading of the card. A multiple-choice
/// ask is the one refusal that does not apply here: a recommendation is the
/// choice "approved" could not make.
fn hold_rules(item: &Value, now: f64) -> Option<String> {
    match nia::decide_at(&strict_policy(), item, now) {
        nia::Decision::Approve | nia::Decision::Never("multiple_choice") => None,
        nia::Decision::Never(why) => Some(why.to_string()),
        nia::Decision::SkipCategory(c) => Some(c),
        nia::Decision::SendBack(why) => Some(why.to_string()),
        nia::Decision::Off => Some("policy_off".into()),
    }
}

/// The one function that decides whether a card may apply.
pub fn classify_view(v: &View, now: f64) -> Class {
    let never = |s: &str| Class::Never(s.to_string());
    if v.worker.trim().is_empty() {
        return never("no_worker");
    }
    if v.isolated {
        return never("isolated_worker");
    }
    if v.snoozed {
        return never("snoozed");
    }
    match v.item["ask_type"].as_str().unwrap_or("") {
        "decision" => {}
        "budget" => return never("money"),
        "customer_outbound" => return never("outside_send"),
        "credential" | "access" => return never("credential_or_access"),
        _ => return Class::Unclassified("ask_type_missing".into()),
    }
    if !super::needs_input::actor_is_owner(v.item["ask_actor"].as_str().unwrap_or("")) {
        return Class::Unclassified("not_addressed_to_owner".into());
    }
    // A runtime job's card (a land approval, a schedule failure, a turn-end
    // ask) has no worker judgment behind it; `land_approval` is typed decision.
    if v.source != "agent" {
        return Class::Unclassified("not_filed_by_a_worker".into());
    }
    if let Some(why) = boundary_in_text(&v.text) {
        return never(why);
    }
    if let Some(why) = hold_rules(&v.item, now) {
        return Class::Never(why);
    }
    if v.recommendation.trim().is_empty() {
        return Class::NeedsRecommendation;
    }
    // The recommendation is what actually happens, so it gets the same rules
    // as the question it answers.
    let mut rec = v.item.clone();
    rec["question"] = json!(v.recommendation);
    rec["category"] = json!(super::needs_input::classify("decision", &v.recommendation).0);
    if let Some(why) = hold_rules(&rec, now) {
        return Class::Never(why);
    }
    if v.scope != SAFE_SCOPE {
        return Class::Unclassified("scope_not_declared".into());
    }
    Class::Safe
}

// ---------------------------------------------------------------------------
// Reading the board
// ---------------------------------------------------------------------------

fn sha(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().take(12).map(|b| format!("{b:02x}")).collect()
}

/// Everything the owner read. Hygiene's Auto-aged line is excluded: it rewrites
/// itself daily on an aged card and would hold the clock at zero forever.
fn fingerprint(row: &IssueRow, recommendation: &str, scope: &str) -> String {
    let desc: Vec<&str> = row.desc.lines().filter(|l| !l.contains("Auto-aged:")).collect();
    let parts = [
        row.title.as_str(),
        row.status.as_str(),
        row.session.as_deref().unwrap_or(""),
        row.item_type.as_str(),
        row.ask_type.as_deref().unwrap_or(""),
        row.ask_question.as_deref().unwrap_or(""),
        row.ask_unblocks.as_deref().unwrap_or(""),
        row.ask_actor.as_deref().unwrap_or(""),
        recommendation,
        scope,
    ];
    sha(&format!("{}\n{}\n{}", parts.join("\n"), row.entered_state_at.unwrap_or(0), desc.join("\n")))
}

/// Who wrote to the card since publication, other than its worker and the
/// harness's own housekeeping. Any such line stops the card, and a log that no
/// longer extends the published one counts as one: doubt stops it.
pub fn outside_activity(log: &str, log_len: i64, log_hash: &str, worker: &str) -> Option<String> {
    let Some(base) = log.get(..log_len.max(0) as usize) else {
        return Some("card log rewritten".into());
    };
    if sha(base) != log_hash {
        return Some("card log rewritten".into());
    }
    let own = format!("{worker}:");
    log[base.len()..].lines().find_map(|line| {
        let entry = line.trim().strip_prefix('`')?.split_once("` ")?.1;
        let housekeeping = ["Auto-aged:", "authz:", "revisit ", "policy:", "harness:"];
        if entry.starts_with(&own) || housekeeping.iter().any(|p| entry.starts_with(p)) {
            return None;
        }
        Some(entry.chars().take(120).collect())
    })
}

fn recommendation_of(conn: &Connection, card: &str) -> (String, String) {
    conn.query_row(
        "SELECT recommendation, scope FROM decision_digest_recommendations WHERE card=?1",
        [card],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap_or_default()
}

fn view_of(conn: &Connection, item: &Value, snoozed: bool) -> rusqlite::Result<Option<View>> {
    if item["kind"].as_str() != Some("card") || item["status"].as_str() != Some("needsyou") {
        return Ok(None);
    }
    let card = item["card"].as_str().unwrap_or("").to_string();
    let Some(row) = bs::get_issue(conn, &card)? else { return Ok(None) };
    let (recommendation, scope) = recommendation_of(conn, &card);
    let worker = row.session.clone().unwrap_or_default();
    let question = item["question"].as_str().unwrap_or("").to_string();
    let text = [
        row.title.as_str(),
        question.as_str(),
        row.ask_unblocks.as_deref().unwrap_or(""),
        recommendation.as_str(),
        row.desc.as_str(),
    ]
    .join("\n");
    Ok(Some(View {
        fingerprint: fingerprint(&row, &recommendation, &scope),
        // Filled by the caller, off the store connection (it reads env files).
        // True until then, so a view nobody checked is the safe reading.
        isolated: true,
        title: row.title.clone(),
        source: row.source.clone().unwrap_or_default(),
        log: row.log.clone().unwrap_or_default(),
        item: item.clone(),
        card,
        worker,
        question,
        recommendation,
        scope,
        text,
        snoozed,
    }))
}

fn load_views(conn: &Connection, home: &Path, now: f64) -> rusqlite::Result<(Vec<View>, usize)> {
    let q = super::needs_input::build(conn, home, now)?;
    let mut out = Vec::new();
    for (items, snoozed) in [(&q.items, false), (&q.snoozed, true)] {
        for it in items {
            if let Some(v) = view_of(conn, it, snoozed)? {
                out.push(v);
            }
        }
    }
    Ok((out, q.n_considered))
}

// ---------------------------------------------------------------------------
// Ledger
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Row {
    pub card: String,
    pub worker: String,
    pub fingerprint: String,
    pub log_len: i64,
    pub log_hash: String,
    /// tracked, applying, applied, failed, vetoed, stopped, reset.
    pub state: String,
    pub reason: String,
    pub actor: String,
    pub live: bool,
    pub digests: i64,
    pub first_digest: String,
    pub last_digest: String,
    pub effect_date: String,
    pub attempts: i64,
    pub receipt: String,
}

fn load_rows(conn: &Connection) -> rusqlite::Result<BTreeMap<String, Row>> {
    let mut s = conn.prepare(
        "SELECT card,worker,fingerprint,log_len,log_hash,state,reason,actor,live,digests,
                first_digest,last_digest,effect_date,attempts,receipt
           FROM decision_digest_ledger",
    )?;
    let rows = s.query_map([], |r| {
        Ok(Row {
            card: r.get(0)?,
            worker: r.get(1)?,
            fingerprint: r.get(2)?,
            log_len: r.get(3)?,
            log_hash: r.get(4)?,
            state: r.get(5)?,
            reason: r.get(6)?,
            actor: r.get(7)?,
            live: r.get::<_, i64>(8)? != 0,
            digests: r.get(9)?,
            first_digest: r.get(10)?,
            last_digest: r.get(11)?,
            effect_date: r.get(12)?,
            attempts: r.get(13)?,
            receipt: r.get(14)?,
        })
    })?;
    rows.map(|r| r.map(|x| (x.card.clone(), x))).collect()
}

fn upsert(conn: &Connection, r: &Row) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO decision_digest_ledger
           (card,worker,fingerprint,log_len,log_hash,state,reason,actor,live,digests,
            first_digest,last_digest,effect_date,attempts,receipt,updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
         ON CONFLICT(card) DO UPDATE SET worker=?2,fingerprint=?3,log_len=?4,log_hash=?5,state=?6,
           reason=?7,actor=?8,live=?9,digests=?10,first_digest=?11,last_digest=?12,effect_date=?13,
           attempts=?14,receipt=?15,updated_at=?16
         WHERE decision_digest_ledger.state NOT IN ('vetoed','stopped','applied','applying')",
        params![
            r.card, r.worker, r.fingerprint, r.log_len, r.log_hash, r.state, r.reason, r.actor,
            r.live as i64, r.digests, r.first_digest, r.last_digest, r.effect_date, r.attempts,
            r.receipt, now_f64()
        ],
    )
}

fn no_events(applied: bool) -> crate::db::WriteOutcome {
    crate::db::WriteOutcome { applied, events: vec![] }
}

/// Move one row between states, only from `from`. True when it moved: this is
/// the atomic claim, and the reason a veto and an application cannot both win.
async fn transition(state: &AppState, card: &str, from: &[&str], to: &str, reason: &str, actor: &str, receipt: Option<&str>) -> bool {
    let (card_s, to_s, reason_s, actor_s) = (card.to_string(), to.to_string(), reason.to_string(), actor.to_string());
    let from_s = from.join(",");
    let receipt_s = receipt.map(str::to_string);
    let moved = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let m = moved.clone();
    let res = state
        .store
        .write_async(move |c| {
            let n = c.execute(
                "UPDATE decision_digest_ledger
                    SET state=?2, reason=?3, actor=?4, receipt=COALESCE(?5,receipt), updated_at=?6,
                        attempts=attempts+(?2='applying')
                  WHERE card=?1 AND instr(','||?7||',', ','||state||',')>0",
                params![card_s, to_s, reason_s, actor_s, receipt_s, now_f64(), from_s],
            )?;
            m.store(n > 0, std::sync::atomic::Ordering::SeqCst);
            Ok(no_events(n > 0))
        })
        .await;
    if let Err(e) = res {
        tracing::warn!(card, to, error = %e, verdict = "decision_digest_unmeasured", "decision digest: ledger write failed");
        return false;
    }
    moved.load(std::sync::atomic::Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// Settings and dates
// ---------------------------------------------------------------------------

fn falsy(v: &str) -> bool {
    matches!(v.trim().trim_matches('"').to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off")
}

fn truthy(v: &str) -> bool {
    matches!(v.trim().trim_matches('"').to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

fn enabled(home: &Path) -> bool {
    !crate::api::settings::effective_env(home, KILL_SWITCH).is_some_and(|v| falsy(&v))
}

/// Default on (plan Z15); the setting is the off switch. server.env is the
/// fleet-wide stop and wins over every scope. Once the key is set, only an
/// explicit yes is on, so a mistyped value stops rather than applies.
pub fn apply_on(home: &Path, worker: &str) -> bool {
    if crate::api::settings::effective_env(home, APPLY_KEY).is_some_and(|v| !truthy(&v)) {
        return false;
    }
    nia::scoped_with_source(home, worker, APPLY_KEY).is_none_or(|(v, _)| truthy(&v))
}

fn digest_hour(home: &Path) -> u32 {
    crate::api::settings::effective_env(home, "AMUX_DECISION_DIGEST_HOUR")
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|h| *h < 24)
        .unwrap_or(9)
        // A later digest could not fit its grace window into its own day.
        .min(23 - APPLY_GRACE_HOURS)
}

fn wait_days(home: &Path) -> i64 {
    crate::api::settings::effective_env(home, "AMUX_DECISION_DIGEST_WAIT_DAYS")
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(MIN_WAIT_DAYS)
        .max(MIN_WAIT_DAYS)
}

fn add_days(date: &str, days: i64) -> String {
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| (d + chrono::Duration::days(days)).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|_| date.to_string())
}

/// `row` after a delivered digest publishes `v` today. Pure: the render shows
/// exactly the dates this commits. A row the owner closed stays closed, and a
/// row whose mode flipped starts over, because a dry-run date promised nothing.
pub fn publish(row: Option<&Row>, v: &View, today: &str, live: bool, wait: i64) -> Row {
    if let Some(r) = row {
        if matches!(r.state.as_str(), "vetoed" | "stopped" | "applied" | "applying" | "failed") {
            return r.clone();
        }
        if r.state == "tracked" && r.live == live && r.fingerprint == v.fingerprint {
            let mut n = r.clone();
            if r.last_digest != today {
                n.digests += 1;
                n.last_digest = today.to_string();
                if n.digests == 2 {
                    n.effect_date = n.effect_date.clone().max(add_days(today, 1));
                }
            }
            return n;
        }
    }
    Row {
        card: v.card.clone(),
        worker: v.worker.clone(),
        fingerprint: v.fingerprint.clone(),
        log_len: v.log.len() as i64,
        log_hash: sha(&v.log),
        state: "tracked".into(),
        live,
        digests: 1,
        first_digest: today.to_string(),
        last_digest: today.to_string(),
        effect_date: add_days(today, wait.max(MIN_WAIT_DAYS)),
        ..Row::default()
    }
}

/// Has the published promise been kept? Live, seen in two delivered digests,
/// and at or past the date. The rule that separates publication from firing.
pub fn date_reached(r: &Row, today: &str) -> bool {
    r.state == "tracked" && r.live && r.digests >= 2 && !r.effect_date.is_empty() && r.effect_date.as_str() <= today
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

fn clip(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= n { s } else { s.chars().take(n).collect::<String>() + "..." }
}

fn long_date(d: &str) -> String {
    NaiveDate::parse_from_str(d, "%Y-%m-%d")
        .map(|x| format!("{} {} {}", x.format("%A"), x.format("%B"), x.day()))
        .unwrap_or_else(|_| d.to_string())
}

fn reason_words(why: &str) -> String {
    match why {
        "money" => "money".into(),
        "new_spend" => "new spend, which needs a yes and a USD figure".into(),
        "tubescience_systems" => "TubeScience's systems".into(),
        "gate_loss" => "removes or weakens a gate".into(),
        "outside_send" | "outbound" => "goes outside the company".into(),
        "customer_or_production_data" | "prod_data" => "customer or production data".into(),
        "credential_or_access" => "a credential or access only you hold".into(),
        "destructive" => "destroys or migrates data".into(),
        "irreversible" => "irreversible".into(),
        "repo_gate" => "a new endpoint or primitive, or a repo-gated push".into(),
        "public_surface" => "a public surface the repo rule keeps with you".into(),
        "isolated_worker" => "an isolated worker, which has no board".into(),
        "no_worker" => "no worker owns it".into(),
        "snoozed" => "you snoozed it".into(),
        "ask_type_missing" => "unclassified: the card has no typed ask".into(),
        "not_addressed_to_owner" => "unclassified: addressed to someone else".into(),
        "not_filed_by_a_worker" => "unclassified: filed by a harness job".into(),
        "scope_not_declared" => "unclassified: its worker has not declared it internal and reversible".into(),
        other => other.replace('_', " "),
    }
}

fn veto_line(base: Option<&str>, card: &str) -> String {
    match base {
        Some(b) => format!("Veto: {}/#digest-veto={card}", b.trim_end_matches('/')),
        None => format!("Veto: add any note to {card} in the dashboard."),
    }
}

fn section(body: &mut String, title: &str, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    body.push_str(&format!("\n\n{title} ({})", lines.len()));
    for l in lines.iter().take(LIST_CAP) {
        body.push_str("\n\n");
        body.push_str(l);
    }
    if lines.len() > LIST_CAP {
        body.push_str(&format!("\n\nand {} more, listed at /api/decision-digest.", lines.len() - LIST_CAP));
    }
}

/// One planned digest entry.
pub struct Entry<'a> {
    pub view: &'a View,
    pub class: Class,
    pub row: Option<Row>,
    pub apply: bool,
}

/// Subject and body. Blank lines between blocks and a marker on every item:
/// mail clients reflow single newlines (CLAUDE.md, the RB2B digest).
pub fn render(entries: &[Entry<'_>], today: &str, base: Option<&str>) -> (String, String) {
    let (mut live, mut dry, mut need, mut never, mut closed) = (vec![], vec![], vec![], vec![], vec![]);
    for e in entries {
        let v = e.view;
        let head = format!("- {} ({}): {}", v.card, v.worker, clip(&v.question, 200));
        let rec = if v.recommendation.is_empty() {
            String::new()
        } else {
            format!("\n\n  Recommendation: {}", clip(&v.recommendation, 300))
        };
        match (&e.row, &e.class) {
            (Some(r), _) if r.state == "vetoed" => closed.push(format!("{head}{rec}\n\n  Vetoed by {}. It stays with you.", r.actor)),
            (Some(r), _) if r.state == "stopped" => closed.push(format!("{head}{rec}\n\n  Stopped: {}. It stays with you.", clip(&r.reason, 160))),
            (Some(r), _) if matches!(r.state.as_str(), "applied" | "applying") => closed.push(format!("{head}{rec}\n\n  Applied by digest default: {}.", clip(&r.receipt, 160))),
            (Some(r), _) if r.state == "failed" => closed.push(format!("{head}{rec}\n\n  Application failed: {}. It stays with you.", clip(&r.reason, 160))),
            (Some(r), Class::Safe) if e.apply => {
                let when = if r.effect_date.as_str() <= today {
                    format!("today, {APPLY_GRACE_HOURS} hours after this digest,")
                } else {
                    long_date(&r.effect_date)
                };
                live.push(format!("{head}{rec}\n\n  Takes effect {when} ({}) unless vetoed.\n\n  {}", r.effect_date, veto_line(base, &v.card)));
            }
            (Some(r), Class::Safe) => dry.push(format!("{head}{rec}\n\n  Would take effect {} ({}). This worker is opted out.", long_date(&r.effect_date), r.effect_date)),
            (_, Class::NeedsRecommendation) => need.push(format!("{head}\n\n  Needs a recommendation before it can take effect.")),
            (_, Class::Never(why)) | (_, Class::Unclassified(why)) => never.push(format!("{head}{rec}\n\n  Never auto-applies: {}.", reason_words(why))),
            (None, Class::Safe) => {}
        }
    }
    let n = entries.len();
    let noun = if n == 1 { "card" } else { "cards" };
    let subject = format!("amux: {n} needsyou {noun}, {} taking effect unless vetoed", live.len());
    let mut body = format!(
        "{n} needsyou {noun} on {today}. {} take effect on their date unless you veto. {} would take effect if not opted out. \
         Any note you add to a card also stops it.",
        live.len(),
        dry.len()
    );
    section(&mut body, "Taking effect unless vetoed", &live);
    section(&mut body, "Opted out, dry run", &dry);
    section(&mut body, "Needs a recommendation", &need);
    section(&mut body, "Stopped", &closed);
    section(&mut body, "Never auto-applies", &never);
    (subject, body)
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// What leaving the ledger touches. A trait so the refusal and crash tests run
/// against a real store with no real worker.
#[async_trait::async_trait]
pub trait Actions: Send + Sync {
    fn isolated(&self, worker: &str) -> bool;
    /// Has this stable message id already been queued or delivered?
    async fn already_sent(&self, state: &AppState, msg_id: &str) -> Result<bool, String>;
    async fn send(&self, state: &AppState, worker: &str, text: &str, msg_id: &str) -> Result<String, String>;
    async fn card(&self, state: &AppState, card: &str, note: &str, marker: &str) -> Result<String, String>;
}

pub struct RealActions;

#[async_trait::async_trait]
impl Actions for RealActions {
    fn isolated(&self, worker: &str) -> bool {
        crate::api::session_verbs::session_is_isolated(worker)
    }

    async fn already_sent(&self, state: &AppState, msg_id: &str) -> Result<bool, String> {
        let id = msg_id.to_string();
        state
            .store
            .read_async(move |c| {
                let found = c
                    .query_row(
                        "SELECT 1 FROM steering_queue WHERE id=?1 UNION ALL SELECT 1 FROM steering_history WHERE id=?1 LIMIT 1",
                        [&id],
                        |_| Ok(()),
                    )
                    .optional()?;
                Ok(found.is_some())
            })
            .await
            .map_err(|e| e.to_string())
    }

    async fn send(&self, state: &AppState, worker: &str, text: &str, msg_id: &str) -> Result<String, String> {
        let r = crate::api::session_verbs::steer_enqueue_idempotent_report(state, worker, text, ORIGIN, "", msg_id)
            .await
            .map_err(|e| format!("send to {worker}: {e}"))?;
        crate::api::session_verbs::steer_deliver_for_session(state, worker).await;
        Ok(format!("message {} queued for {worker}", r.id))
    }

    async fn card(&self, state: &AppState, card: &str, note: &str, marker: &str) -> Result<String, String> {
        nia::card_via_board_as(state, card, note, marker, APPROVER, ACTOR).await
    }
}

fn msg_id(r: &Row) -> String {
    format!("decision-digest-{}", sha(&format!("{}:{}", r.card, r.fingerprint)))
}

pub fn apply_text(v: &View, r: &Row) -> String {
    format!(
        "[amux digest default, not the owner] Decision on {card}: {q} Take your recommendation: {rec}. \
         Published in the owner's digest from {first} with effect date {date}, and not vetoed. It covers this \
         recommendation only, and anything at the owner's boundary stays a needsyou card. {CLOSING}",
        card = v.card,
        q = clip(&v.question, 300),
        rec = clip(&v.recommendation, 400),
        first = r.first_digest,
        date = r.effect_date,
    )
}

/// What a card needs. Sent only to workers with apply on: an opted-out scope
/// is a dry run, where the digest reaches the owner and nobody else.
pub fn ask_text(card: &str) -> String {
    format!(
        "{card} is waiting on the owner with no recommendation, so the daily digest cannot put a date on it. If you \
         have one, record it: PUT /api/decision-digest/recommendation with {{\"card\":\"{card}\",\"recommendation\":\"<what you \
         would do>\",\"scope\":\"{SAFE_SCOPE}\"}}. Declare that scope only if it spends nothing, reaches nobody outside the \
         company, touches no customer or production data, and can be undone; otherwise leave scope empty and it stays \
         with the owner."
    )
}

// ---------------------------------------------------------------------------
// One tick
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub ran: bool,
    pub digest_sent: bool,
    pub applied: Vec<String>,
    pub would_apply: Vec<String>,
}

/// Why a tracked row can no longer stand as published, if it cannot.
fn gate(row: &Row, v: Option<&View>, now: f64) -> Option<(&'static str, String)> {
    let Some(v) = v else {
        return Some(("reset", "left the needsyou queue".into()));
    };
    if let Some(line) = outside_activity(&v.log, row.log_len, &row.log_hash, &row.worker) {
        return Some(("stopped", format!("card changed by someone other than its worker: {line}")));
    }
    if v.fingerprint != row.fingerprint {
        return Some(("reset", "card changed after the digest".into()));
    }
    match classify_view(v, now) {
        Class::Safe => None,
        other => Some(("reset", format!("no longer safe: {other:?}"))),
    }
}

async fn close_row(state: &AppState, row: &Row, from: &[&str], to: &str, why: &str) {
    if transition(state, &row.card, from, to, why, ACTOR, None).await {
        let verdict = if to == "stopped" { "decision_digest_stopped_owner" } else { "decision_digest_reset" };
        tracing::info!(card = %row.card, worker = %row.worker, reason = why, measured = true, n_considered = 1, verdict,
            "decision digest: the published default no longer stands");
    }
}

async fn settle_error(state: &AppState, row: &Row, e: &str) {
    if e.contains("target is paused") {
        transition(state, &row.card, &["applying"], "tracked", e, ACTOR, None).await;
        tracing::info!(card = %row.card, worker = %row.worker, measured = true, n_considered = 1,
            verdict = "decision_digest_held", "decision digest: worker is paused; held until it resumes");
        return;
    }
    if row.attempts >= MAX_ATTEMPTS {
        transition(state, &row.card, &["applying"], "failed", e, ACTOR, None).await;
    }
    tracing::warn!(card = %row.card, worker = %row.worker, error = e, attempts = row.attempts, measured = true, n_considered = 1,
        verdict = "decision_digest_apply_failed", "decision digest: application did not finish; no receipt written");
}

/// Send (unless a prior process already did), note the card, write the receipt.
/// `row.state` is `applying` here.
async fn finish_apply(acts: &dyn Actions, state: &AppState, row: &Row, v: &View, sent: bool, rep: &mut Report) {
    let id = msg_id(row);
    if !sent {
        if let Err(e) = acts.send(state, &row.worker, &apply_text(v, row), &id).await {
            settle_error(state, row, &e).await;
            return;
        }
    }
    let stamp = chrono::Local::now().format("%b %d %H:%M");
    let note = format!(
        "[{stamp}] Applied by digest default ({ACTOR}): {}. Published in the owner's digest from {} with effect date {}, not vetoed. This is not an owner decision.",
        clip(&v.recommendation, 300), row.first_digest, row.effect_date
    );
    let marker = format!("#{id}");
    match acts.card(state, &row.card, &note, &marker).await {
        Ok(detail) => {
            let receipt = format!("{id}; {detail}");
            if transition(state, &row.card, &["applying"], "applied", "applied by digest default", ACTOR, Some(&receipt)).await {
                tracing::info!(card = %row.card, worker = %row.worker, effect_date = %row.effect_date, origin = ORIGIN, receipt = %receipt,
                    adopted = sent, measured = true, n_considered = 1,
                    verdict = if sent { "decision_digest_adopted" } else { "decision_digest_applied" },
                    "decision digest: recommendation applied by digest default");
                rep.applied.push(row.card.clone());
            }
        }
        Err(e) => settle_error(state, row, &e).await,
    }
}

/// A row a dead process left in `applying`. A send that happened is adopted
/// whatever has changed since, because the worker already has it. With no send,
/// the claim bought nothing, so every gate runs again.
async fn resume(acts: &dyn Actions, state: &AppState, home: &Path, row: &Row, v: Option<&View>, now: f64, rep: &mut Report) {
    let sent = match acts.already_sent(state, &msg_id(row)).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(card = %row.card, error = %e, verdict = "decision_digest_unmeasured",
                "decision digest: cannot tell whether a claimed send happened; holding it");
            return;
        }
    };
    if !sent {
        if let Some((to, why)) = gate(row, v, now) {
            close_row(state, row, &["applying"], to, &why).await;
            return;
        }
        if !apply_on(home, &row.worker) {
            close_row(state, row, &["applying"], "reset", "apply turned off").await;
            return;
        }
    }
    match v {
        Some(v) => {
            let mut r = row.clone();
            if r.attempts >= MAX_ATTEMPTS {
                transition(state, &r.card, &["applying"], "failed", "gave up after repeated restarts", ACTOR, None).await;
                return;
            }
            transition(state, &r.card, &["applying"], "applying", "resumed", ACTOR, None).await;
            r.attempts += 1;
            finish_apply(acts, state, &r, v, sent, rep).await;
        }
        // Sent, and the card has already left needsyou: the worker moved it.
        None => {
            let receipt = format!("{}; card left the queue before the receipt", msg_id(row));
            if transition(state, &row.card, &["applying"], "applied", "adopted after a restart", ACTOR, Some(&receipt)).await {
                tracing::info!(card = %row.card, measured = true, n_considered = 1, verdict = "decision_digest_adopted",
                    "decision digest: adopted a completed send after a restart");
                rep.applied.push(row.card.clone());
            }
        }
    }
}

async fn deliver(channels: &dyn AlertChannels, state: &AppState, home: &Path, subject: &str, body: &str) -> (Value, bool) {
    let (email_ok, email) = match crate::api::alerts::owner_email_destination(home) {
        Some((to, _)) => {
            let (ok, detail) = channels.email(&to, subject, body).await;
            (ok, format!("{}{detail} -> {to}", if ok { "" } else { "failed: " }))
        }
        None => (false, "no AMUX_OWNER_EMAIL and no connected Gmail account".to_string()),
    };
    // Publication claims the owner can read the dates, and web push carries
    // only the subject, so the email alone counts. Push follows a sent email:
    // a retry every tick must not buzz the phone.
    let push = if email_ok {
        let results = crate::push::send_all(state, "amux", subject, "", "decision-digest", "/").await;
        crate::api::alerts::push_delivery_verdict(&results).is_ok()
    } else {
        false
    };
    (json!({"push": push, "email": email}), email_ok)
}

fn plan<'a>(views: &'a [View], rows: &BTreeMap<String, Row>, home: &Path, today: &str, now: f64) -> Vec<Entry<'a>> {
    let wait = wait_days(home);
    views
        .iter()
        .map(|v| {
            let class = classify_view(v, now);
            let existing = rows.get(&v.card);
            let apply = apply_on(home, &v.worker);
            let row = match class {
                Class::Safe => Some(publish(existing, v, today, apply, wait)),
                _ => existing.filter(|r| matches!(r.state.as_str(), "vetoed" | "stopped" | "failed")).cloned(),
            };
            Entry { view: v, class, row, apply }
        })
        .collect()
}

/// One pass: reconcile, publish, apply. `now_local` is injected so the
/// timeline is testable.
pub async fn tick_with(
    channels: &dyn AlertChannels,
    acts: &dyn Actions,
    state: &AppState,
    home: &Path,
    now_local: chrono::DateTime<chrono::Local>,
) -> Report {
    let mut rep = Report::default();
    if !enabled(home) {
        if crate::log_dedupe::first_this_bucket("decision-digest-disabled", crate::log_dedupe::hour_bucket(now_f64()) / 24) {
            tracing::info!(switch = KILL_SWITCH, verdict = "decision_digest_disabled", "decision digest: kill switch is off");
        }
        return rep;
    }
    let now = now_local.timestamp() as f64;
    let today = now_local.format("%Y-%m-%d").to_string();
    let hour = now_local.hour();
    let dh = digest_hour(home);
    let h = home.to_path_buf();
    let read = state
        .store
        .read_async(move |c| {
            let (views, n) = load_views(c, &h, now)?;
            let pref = |k: &str| c.query_row("SELECT value FROM prefs WHERE key=?1", [k], |r| r.get::<_, String>(0)).optional();
            let stamp = pref(STAMP_KEY)?;
            let sent_at = pref(STAMP_AT_KEY)?.and_then(|v| v.parse::<f64>().ok());
            Ok((views, n, load_rows(c)?, stamp, sent_at))
        })
        .await;
    let (mut views, n_considered, rows, stamp, mut sent_at) = match read {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, verdict = "decision_digest_unmeasured", "decision digest: could not read the queue; nothing evaluated");
            return rep;
        }
    };
    for v in &mut views {
        v.isolated = acts.isolated(&v.worker);
    }
    rep.ran = true;
    let by_card: HashMap<&str, &View> = views.iter().map(|v| (v.card.as_str(), v)).collect();

    // Crash recovery first, then every published row against the card now.
    for row in rows.values().filter(|r| r.state == "applying") {
        resume(acts, state, home, row, by_card.get(row.card.as_str()).copied(), now, &mut rep).await;
    }
    for row in rows.values().filter(|r| r.state == "tracked") {
        if let Some((to, why)) = gate(row, by_card.get(row.card.as_str()).copied(), now) {
            close_row(state, row, &["tracked"], to, &why).await;
        }
    }

    // Publish. Reload so this tick's stops show in the digest it sends.
    let due = !views.is_empty() && hour >= dh && stamp.as_deref() != Some(today.as_str());
    let mut digest_today = stamp.as_deref() == Some(today.as_str());
    if due {
        let rows = state.store.read_async(|c| Ok(load_rows(c)?)).await.unwrap_or_default();
        let entries = plan(&views, &rows, home, &today, now);
        let base = crate::api::static_files::public_links::public_url(home);
        let (subject, body) = render(&entries, &today, base.as_deref());
        let (out, delivered) = deliver(channels, state, home, &subject, &body).await;
        if delivered {
            let writes: Vec<Row> = entries.iter().filter(|e| e.class == Class::Safe).filter_map(|e| e.row.clone()).collect();
            let (t, n) = (today.clone(), entries.len());
            let wrote = state
                .store
                .write_async(move |c| {
                    // One writer closure is one transaction: the dates and the
                    // stamp commit together or not at all.
                    for r in &writes {
                        upsert(c, r)?;
                    }
                    for (k, v) in [(STAMP_KEY, t.clone()), (STAMP_AT_KEY, now.to_string())] {
                        c.execute("INSERT INTO prefs(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2", params![k, v])?;
                    }
                    Ok(no_events(true))
                })
                .await;
            match wrote {
                Ok(_) => {
                    digest_today = true;
                    sent_at = Some(now);
                    rep.digest_sent = true;
                    tracing::info!(measured = true, n_considered, cards = n, channels = %out, verdict = "decision_digest_sent", "decision digest delivered");
                    ask_for_recommendations(acts, state, home, &entries).await;
                }
                // Sent but unrecorded: the next tick sends it again, and no
                // date moved, so the failure costs a duplicate email.
                Err(e) => tracing::warn!(error = %e, verdict = "decision_digest_unmeasured", "decision digest: sent, but the ledger write failed"),
            }
        } else if crate::log_dedupe::first_this_bucket("decision-digest-undelivered", crate::log_dedupe::hour_bucket(now_f64())) {
            tracing::warn!(measured = true, n_considered, channels = %out, verdict = "decision_digest_undelivered",
                "decision digest reached no inbox; dates not published, will retry next tick");
        }
    }

    // Apply. An unread digest applies nothing: today's has to be out first.
    let grace = f64::from(APPLY_GRACE_HOURS) * 3600.0;
    if !digest_today || sent_at.is_none_or(|t| now - t < grace) {
        return rep;
    }
    let rows = match state.store.read_async(|c| Ok(load_rows(c)?)).await {
        Ok(r) => r,
        Err(_) => return rep,
    };
    for row in rows.values().filter(|r| r.state == "tracked" && date_reached(r, &today)) {
        let Some(v) = by_card.get(row.card.as_str()).copied() else { continue };
        if gate(row, Some(v), now).is_some() {
            continue;
        }
        if !apply_on(home, &row.worker) {
            continue;
        }
        if !transition(state, &row.card, &["tracked"], "applying", "claimed", ACTOR, None).await {
            continue;
        }
        tracing::info!(card = %row.card, worker = %row.worker, measured = true, n_considered = 1, verdict = "decision_digest_applying", "decision digest: claimed");
        let mut r = row.clone();
        r.state = "applying".into();
        r.attempts += 1;
        finish_apply(acts, state, &r, v, false, &mut rep).await;
    }
    // The dry run's half of the same rule: what the switch would have done.
    for v in &views {
        if classify_view(v, now) == Class::Safe && !apply_on(home, &v.worker) {
            let past = rows.get(&v.card).is_some_and(|r| !r.live && r.effect_date.as_str() <= today.as_str());
            if past && crate::log_dedupe::first_this_bucket(&format!("decision-digest-would-{}", v.card), crate::log_dedupe::hour_bucket(now) / 24) {
                tracing::info!(card = %v.card, worker = %v.worker, measured = true, n_considered = 1,
                    verdict = "decision_digest_would_apply", "decision digest: dry run, this card would apply today");
            }
            if past {
                rep.would_apply.push(v.card.clone());
            }
        }
    }
    rep
}

async fn ask_for_recommendations(acts: &dyn Actions, state: &AppState, home: &Path, entries: &[Entry<'_>]) {
    let need = entries.iter().filter(|e| e.class == Class::NeedsRecommendation && apply_on(home, &e.view.worker));
    for e in need.take(ASK_CAP) {
        let id = format!("decision-digest-ask-{}", sha(&format!("{}:{}", e.view.card, e.view.fingerprint)));
        let sent = acts.send(state, &e.view.worker, &ask_text(&e.view.card), &id).await;
        tracing::info!(card = %e.view.card, worker = %e.view.worker, ok = sent.is_ok(), measured = true, n_considered = 1,
            verdict = "decision_digest_recommendation_asked", "decision digest: asked the worker for a recommendation");
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

fn fail(code: StatusCode, error: &str, name: &str) -> Response {
    (code, Json(json!({"ok": false, "error": error, "code": name}))).into_response()
}

/// Record the owner's veto. Final: there is no route that lifts one.
pub async fn veto(state: &AppState, card: &str, who: &str, reason: &str) -> Result<String, (StatusCode, String)> {
    let c = card.to_string();
    let found = state
        .store
        .read_async(move |conn| {
            let row = bs::get_issue(conn, &c)?;
            let led = load_rows(conn)?.remove(&c);
            Ok((row, led))
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let (Some(issue), led) = found else {
        return Err((StatusCode::NOT_FOUND, format!("no card {card}")));
    };
    if let Some(l) = &led {
        match l.state.as_str() {
            "applied" => return Err((StatusCode::CONFLICT, format!("{card} already took effect: {}", l.receipt))),
            "applying" => return Err((StatusCode::CONFLICT, format!("{card} is being applied now"))),
            "vetoed" => return Ok(format!("{card} was already vetoed by {}", l.actor)),
            _ => {}
        }
    }
    let why = if reason.trim().is_empty() { "owner veto".to_string() } else { clip(reason, 300) };
    let row = Row {
        card: card.to_string(),
        worker: issue.session.clone().unwrap_or_default(),
        state: "vetoed".into(),
        reason: why.clone(),
        actor: who.to_string(),
        ..led.unwrap_or_default()
    };
    let (w, wa, wr) = (row.clone(), who.to_string(), why.clone());
    let wrote = state
        .store
        .write_async(move |c| {
            // The claim's `applying` is the only state a veto loses to.
            let n = c.execute(
                "INSERT INTO decision_digest_ledger(card,worker,state,reason,actor,updated_at) VALUES(?1,?2,'vetoed',?3,?4,?5)
                 ON CONFLICT(card) DO UPDATE SET state='vetoed',reason=?3,actor=?4,updated_at=?5
                 WHERE decision_digest_ledger.state NOT IN ('applied','applying')",
                params![w.card, w.worker, wr, wa, now_f64()],
            )?;
            Ok(no_events(n > 0))
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !wrote.applied {
        return Err((StatusCode::CONFLICT, format!("{card} is being applied now")));
    }
    let note = format!("[{}] {who} vetoed the digest default. This card stays in needsyou.", chrono::Local::now().format("%b %d %H:%M"));
    let noted = crate::api::board::patch_item(
        State(state.clone()),
        axum::extract::Path(card.to_string()),
        crate::api::org::policy_headers(ACTOR),
        Json(json!({"desc_append": note})),
    )
    .await
    .status()
    .is_success();
    tracing::info!(card, who, reason = %why, noted, measured = true, n_considered = 1, verdict = "decision_digest_vetoed",
        "decision digest: owner veto recorded, final");
    Ok(format!("{card} vetoed"))
}

async fn veto_route(State(state): State<AppState>, headers: HeaderMap, uri: Uri, Json(body): Json<Value>) -> Response {
    if let Err(r) = super::land_approvals::owner_only(&state, &headers, &uri, "decision_digest_veto_refused", "veto a digest default") {
        return *r;
    }
    let card = body["card"].as_str().unwrap_or("").trim().to_string();
    if card.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "card is required", "card_required");
    }
    let who = crate::api::turn_end::owner_name();
    match veto(&state, &card, &who, body["reason"].as_str().unwrap_or("")).await {
        Ok(msg) => Json(json!({"ok": true, "card": card, "detail": msg, "by": who})).into_response(),
        Err((code, msg)) => {
            tracing::warn!(card, error = %msg, verdict = "decision_digest_veto_refused", measured = true, n_considered = 1, "decision digest: veto could not be recorded");
            fail(code, &msg, "veto_refused")
        }
    }
}

async fn recommendation_route(State(state): State<AppState>, headers: HeaderMap, uri: Uri, Json(body): Json<Value>) -> Response {
    let card = body["card"].as_str().unwrap_or("").trim().to_string();
    let rec = body["recommendation"].as_str().unwrap_or("").trim().to_string();
    let scope = body["scope"].as_str().unwrap_or("").trim().to_string();
    let refuse = |code: StatusCode, why: &str, name: &str| {
        tracing::warn!(card = %card, reason = why, measured = true, n_considered = 1, verdict = "decision_digest_recommendation_refused",
            "decision digest: recommendation refused");
        fail(code, why, name)
    };
    if card.is_empty() || rec.split_whitespace().count() < 3 {
        return refuse(StatusCode::BAD_REQUEST, "card and a recommendation of at least three words are required", "recommendation_invalid");
    }
    if !(scope.is_empty() || scope == SAFE_SCOPE) {
        return refuse(StatusCode::BAD_REQUEST, "scope is empty or internal_reversible", "scope_invalid");
    }
    let lane = super::alerts::hdr_worker(&headers);
    let c = card.clone();
    let issue = match state.store.read_async(move |conn| Ok(bs::get_issue(conn, &c)?)).await {
        Ok(Some(i)) => i,
        Ok(None) => return refuse(StatusCode::NOT_FOUND, "no such card", "card_not_found"),
        Err(e) => return refuse(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), "read_failed"),
    };
    // A worker speaks for its own card, and only the owner speaks without a
    // worker identity: dropping the header is not proof of being the owner.
    let by = if lane.is_empty() {
        if let Err(r) = super::land_approvals::owner_only(&state, &headers, &uri, "decision_digest_recommendation_refused", "set a recommendation without a worker identity") {
            return *r;
        }
        crate::api::turn_end::owner_name()
    } else if issue.session.as_deref() == Some(lane.as_str()) {
        lane
    } else {
        return refuse(StatusCode::FORBIDDEN, "only the card's worker or the owner records its recommendation", "not_card_worker");
    };
    let (c, r, s, b) = (card.clone(), rec, scope, by.clone());
    let wrote = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO decision_digest_recommendations(card,recommendation,scope,set_by,set_at) VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(card) DO UPDATE SET recommendation=?2,scope=?3,set_by=?4,set_at=?5",
                params![c, r, s, b, now_f64()],
            )?;
            Ok(no_events(true))
        })
        .await;
    match wrote {
        Ok(_) => {
            tracing::info!(card = %card, by = %by, measured = true, n_considered = 1, verdict = "decision_digest_recommendation_set", "decision digest: recommendation recorded");
            Json(json!({"ok": true, "card": card, "by": by})).into_response()
        }
        Err(e) => refuse(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), "write_failed"),
    }
}

/// What the next digest would say. Reads only; `measured` and `n_considered`
/// keep an empty list from reading as a quiet queue when the probe failed.
async fn get_view(State(state): State<AppState>, Query(_): Query<HashMap<String, String>>) -> Response {
    let home = crate::config::amux_home();
    let now = now_f64();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let h = home.clone();
    let read = state
        .store
        .read_async(move |c| {
            let (views, n) = load_views(c, &h, now)?;
            Ok((views, n, load_rows(c)?))
        })
        .await;
    let (mut views, n, rows) = match read {
        Ok(v) => v,
        Err(e) => {
            return Json(json!({"measured": false, "n_considered": 0, "why_unmeasured": e.to_string(), "cards": []})).into_response();
        }
    };
    for v in &mut views {
        v.isolated = crate::api::session_verbs::session_is_isolated(&v.worker);
    }
    let cards: Vec<Value> = plan(&views, &rows, &home, &today, now)
        .iter()
        .map(|e| {
            let (class, why) = match &e.class {
                Class::Safe => ("safe", String::new()),
                Class::NeedsRecommendation => ("needs_recommendation", String::new()),
                Class::Never(w) => ("never", w.clone()),
                Class::Unclassified(w) => ("unclassified", w.clone()),
            };
            json!({
                "card": e.view.card, "worker": e.view.worker, "question": e.view.question,
                "recommendation": e.view.recommendation, "scope": e.view.scope,
                "class": class, "reason": why, "apply": e.apply,
                "state": e.row.as_ref().map(|r| r.state.clone()),
                "effect_date": e.row.as_ref().map(|r| r.effect_date.clone()),
                "digests": e.row.as_ref().map(|r| r.digests),
                "actor": e.row.as_ref().map(|r| r.actor.clone()),
                "receipt": e.row.as_ref().map(|r| r.receipt.clone()),
            })
        })
        .collect();
    Json(json!({
        "measured": true, "n_considered": n, "n_cards": cards.len(), "enabled": enabled(&home),
        "digest_hour": digest_hour(&home), "cards": cards,
    }))
    .into_response()
}

#[cfg(test)]
#[path = "decision_digest_tests.rs"]
mod tests;
