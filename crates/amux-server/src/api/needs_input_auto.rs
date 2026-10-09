//! Automatic approval of needs-input items as they arrive (AMUX-5301).
//!
//! Ethan, 2026-09-27 15:20: "there should also be some kind of mechanism to
//! automatically approve all needs input as they come in". 15:27: "you should
//! enable 4 for now across all the active workers" and "approve all should be
//! a configuration on the worker level".
//!
//! Three parts, over stores that already exist (no new primitive):
//!
//! - **Policy**: six scoped environment keys ([`FIELDS`]), resolved worker >
//!   group > global (`amux.env`) by the same layering every other scoped
//!   `AMUX_*` gate uses, so a worker or a group can differ from the fleet.
//!   Defaults when nothing is set anywhere: ON, `other` on, `money` on with a
//!   $50 per-item cap, `prod_data` off, `outbound` off (a send cannot be taken
//!   back). `AMUX_NEEDS_INPUT_AUTO=0` in server.env or the process env is the
//!   fleet-wide kill switch and wins over every layer. Credential and access
//!   asks are never approved: "approved" cannot mint a key or sign anybody in.
//! - **Engine**: [`tick_with`], run every 60s by
//!   `runtime_jobs::needs_input_auto`. It builds the queue GET
//!   /api/needs-input serves and evaluates each item ONCE, the first tick it
//!   is seen, against its worker's resolved policy. The very first run records
//!   everything already waiting as `baseline` and approves none of it; POST
//!   /api/needs-input/auto/sweep is the owner's explicit "include what is
//!   already waiting". An approval does what the triage sheet's Approve does:
//!   an owner message "Approved (ID): ... Proceed." to the worker (through the
//!   owner-policy guard, so it reaches isolated lanes too), a note on the card
//!   and the needsyou -> todo move through the ordinary board PATCH, read
//!   back, or the email release through the real /api/email/approve handler.
//! - **Visibility**: every evaluation is a row in the ledger ([`LEDGER_KEY`]).
//!   Approved and refused rows from the last 7 days are the "Auto-approved"
//!   list in the triage sheet and Settings. One FYI web push per batch, and
//!   one verdict line per item: `needs_input_auto_approved`,
//!   `needs_input_auto_refused`, `needs_input_auto_skipped_cap`,
//!   `needs_input_auto_skipped_category`.
//!
//! Dedupe: an item is claimed in the ledger BEFORE anything is sent, keyed on
//! the item key plus a hash of its question, so a crash mid-approve can never
//! send twice, and a card re-asked with a new question is a new ask. A refusal
//! is recorded and not retried: the item stays in the queue for the owner.
//! Snoozed items are never touched: an item first seen snoozed is recorded as
//! `snoozed` and is not auto-approved when the snooze ends.

use super::AppState;
use crate::api::needs_input;
use crate::config::now_f64;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

/// `prefs` key holding the ledger: an array of [`Entry`].
pub const LEDGER_KEY: &str = "needs_input_auto_ledger";
/// Master switch and kill switch.
pub const KILL_SWITCH: &str = "AMUX_NEEDS_INPUT_AUTO";
/// What the triage sheet and Settings show.
pub const SHOW_S: f64 = 7.0 * 86_400.0;
/// How long a ledger row outlives its item leaving the queue. A row whose
/// item is still waiting is never pruned, or a long-waiting baseline item
/// would come back as "new".
pub const KEEP_S: f64 = 30.0 * 86_400.0;
/// Named in the card note, the send-audit ledger and the board log.
pub const APPROVER: &str = "owner policy (needs-input auto-approve)";
pub const DEFAULT_CAP_USD: f64 = 50.0;
/// The source reported for categories the contract keeps with the owner.
pub const CONTRACT_RULE_11: &str = "contract rule 11 (never auto-approved)";
/// A `pending` row older than this was claimed by a tick that never finished
/// (a restart mid-batch): it is re-evaluated under the current policy.
/// Measured 2026-09-27: ten 18:52 claims sat pending for 90 minutes after a
/// deploy restarted the server mid-sweep, and a pending row is `seen`.
pub const PENDING_STALE_S: f64 = 15.0 * 60.0;

/// (field, scoped env key, default). The field names are the API's.
pub const FIELDS: [(&str, &str, &str); 7] = [
    ("enabled", KILL_SWITCH, "1"),
    ("send_back", "AMUX_NEEDS_INPUT_AUTO_SEND_BACK", "1"),
    ("other", "AMUX_NEEDS_INPUT_AUTO_OTHER", "1"),
    ("money", "AMUX_NEEDS_INPUT_AUTO_MONEY", "0"),
    ("money_cap_usd", "AMUX_NEEDS_INPUT_AUTO_MONEY_CAP", "50"),
    ("prod_data", "AMUX_NEEDS_INPUT_AUTO_PROD_DATA", "0"),
    ("outbound", "AMUX_NEEDS_INPUT_AUTO_OUTBOUND", "0"),
];

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/needs-input/auto",
            axum::routing::get(get_policy).put(put_policy),
        )
        .route("/api/needs-input/auto/sweep", axum::routing::post(sweep))
}

// ---------------------------------------------------------------------------
// Policy: scoped resolution
// ---------------------------------------------------------------------------

/// A worker's resolved policy.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub enabled: bool,
    pub other: bool,
    pub money: bool,
    pub money_cap_usd: f64,
    pub prod_data: bool,
    pub outbound: bool,
    /// Return credential and owner-action asks to their worker with the
    /// access ladder instead of leaving them for the owner.
    pub send_back: bool,
    /// field -> where the value came from: `worker`, `group:<name>`,
    /// `global`, `server.env`, or `default`.
    pub sources: BTreeMap<String, String>,
}

impl Default for Policy {
    fn default() -> Self {
        Policy::from_lookup(|_| None)
    }
}

fn truthy(v: &str) -> bool {
    matches!(
        v.trim().trim_matches('"').to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn falsy(v: &str) -> bool {
    matches!(
        v.trim().trim_matches('"').to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

impl Policy {
    /// Build from a lookup `key -> (value, source)`; unset keys take the
    /// default. A cap that does not parse falls back to the default rather
    /// than to "no cap".
    pub fn from_lookup(get: impl Fn(&str) -> Option<(String, String)>) -> Policy {
        let mut sources = BTreeMap::new();
        let mut val = |field: &str, key: &str, default: &str| -> String {
            match get(key) {
                Some((v, src)) => {
                    sources.insert(field.to_string(), src);
                    v
                }
                None => {
                    sources.insert(field.to_string(), "default".into());
                    default.to_string()
                }
            }
        };
        let mut v: HashMap<&str, String> = HashMap::new();
        for (field, key, default) in FIELDS {
            v.insert(field, val(field, key, default));
        }
        let b = |f: &str| {
            let s = &v[f];
            if f == "enabled" {
                !falsy(s)
            } else {
                truthy(s)
            }
        };
        let cap = v["money_cap_usd"]
            .trim()
            .trim_start_matches('$')
            .parse::<f64>()
            .ok()
            .filter(|c| c.is_finite() && *c >= 0.0)
            .unwrap_or(DEFAULT_CAP_USD);
        // Orchestration contract rule 11 (AH-386): auto-approve never covers
        // money, production data or outside parties, whatever a scope sets.
        // Forced here, where the policy resolves, so the decision, the summary
        // and the dashboard all report the same thing.
        for f in ["money", "prod_data", "outbound"] {
            sources.insert(f.to_string(), CONTRACT_RULE_11.to_string());
        }
        Policy {
            enabled: b("enabled"),
            other: b("other"),
            money: false,
            money_cap_usd: cap,
            prod_data: false,
            outbound: false,
            send_back: b("send_back"),
            sources,
        }
    }

    pub fn to_json(&self) -> Value {
        let src = |f: &str| self.sources.get(f).cloned().unwrap_or_else(|| "default".into());
        json!({
            "enabled": {"value": self.enabled, "source": src("enabled")},
            "other": {"value": self.other, "source": src("other")},
            "money": {"value": self.money, "source": src("money")},
            "money_cap_usd": {"value": self.money_cap_usd, "source": src("money_cap_usd")},
            "prod_data": {"value": self.prod_data, "source": src("prod_data")},
            "outbound": {"value": self.outbound, "source": src("outbound")},
            "send_back": {"value": self.send_back, "source": src("send_back")},
        })
    }

    /// One plain sentence: what this policy approves.
    pub fn summary(&self, who: &str) -> String {
        if !self.enabled {
            return format!("Auto-approve is OFF for {who}. Every ask waits for you.");
        }
        let mut parts = Vec::new();
        if self.other {
            parts.push("judgment asks".to_string());
        }
        if self.money {
            parts.push(format!("spend up to ${}", fmt_usd(self.money_cap_usd)));
        }
        if self.prod_data {
            parts.push("production data changes".to_string());
        }
        if self.outbound {
            parts.push("outbound emails and posts".to_string());
        }
        if parts.is_empty() {
            return format!("Auto-approve is ON for {who}, but every category is off, so nothing is approved.");
        }
        parts.push("never money, production data, outside parties, scope, priorities or reversals of owner actions".to_string());
        let list = match parts.len() {
            1 => parts[0].clone(),
            n => format!("{} and {}", parts[..n - 1].join(", "), parts[n - 1]),
        };
        let back = if self.send_back {
            " Key, sign-in and grant asks go back to the worker to do itself."
        } else {
            ""
        };
        format!("Auto-approve is ON for {who}: {list}.{back}")
    }
}

fn fmt_usd(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v:.2}")
    }
}

/// `key` at worker > group > global, with the layer that supplied it.
/// Mirrors `session_verbs::scoped_setting_in`, plus the source.
pub fn scoped_with_source(home: &Path, worker: &str, key: &str) -> Option<(String, String)> {
    use crate::api::session_verbs::EnvFile;
    let nonempty = |v: Option<&str>| v.map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
    let mut groups: Vec<String> = Vec::new();
    if !worker.is_empty() {
        let wf = EnvFile::load(&home.join("sessions").join(format!("{worker}.env")));
        if let Some(v) = nonempty(wf.get(key)) {
            return Some((v, "worker".into()));
        }
        groups = wf
            .get("CC_TAGS")
            .map(|v| {
                v.split(',')
                    .map(|t| t.trim().trim_matches('"').to_lowercase())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default();
    }
    for g in groups {
        let gf = EnvFile::load(&home.join("env").join(format!("{g}.env")));
        if let Some(v) = nonempty(gf.get(key)) {
            return Some((v, format!("group:{g}")));
        }
    }
    nonempty(EnvFile::load(&home.join("amux.env")).get(key)).map(|v| (v, "global".into()))
}

/// The fleet-wide kill switch: `AMUX_NEEDS_INPUT_AUTO=0` in server.env or the
/// process env. Wins over every scoped layer.
pub fn server_kill(home: &Path) -> bool {
    crate::api::settings::effective_env(home, KILL_SWITCH).is_some_and(|v| falsy(&v))
}

/// The policy for one worker (`""` = the fleet default, global layer only).
pub fn resolve(home: &Path, worker: &str) -> Policy {
    let mut p = Policy::from_lookup(|k| scoped_with_source(home, worker, k));
    if server_kill(home) {
        p.enabled = false;
        p.sources.insert("enabled".into(), "server.env".into());
    }
    p
}

// ---------------------------------------------------------------------------
// Pure decision
// ---------------------------------------------------------------------------

fn hash(s: &str) -> String {
    use sha2::Digest;
    let d = sha2::Sha256::digest(s.as_bytes());
    d.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// One ask, once: the item key plus its question. A card re-asked with a new
/// question is a new ask; the same ask seen on every tick is not.
pub fn dedupe_key(item: &Value) -> String {
    let key = item["key"].as_str().unwrap_or("");
    let q = item["question"].as_str().unwrap_or("");
    format!("{key}#{}", hash(q.trim()))
}

/// The ask text the category and the cap read.
fn ask_text(item: &Value) -> String {
    ["question", "title", "unblocks"]
        .iter()
        .filter_map(|k| item[*k].as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// An ask's explicit reservation outranks the default judgment policy. Include
/// its unblock and context, where GS-199 put the reservation; do not infer a
/// credential step from the recorder's generic unblock boilerplate.
fn requires_explicit_approval(item: &Value) -> bool {
    let text = format!("{} {}", ask_text(item), item["context"].as_str().unwrap_or(""))
        .to_ascii_lowercase();
    static EXCLUSION: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"\b(?:auto(?:matic|mated)?[- ](?:needs[- ]input[- ])?approv(?:al|e) (?:does not|doesn't|cannot|can't|must not) (?:cover|approve)",
            r"|not covered by auto(?:matic|mated)?[- ](?:needs[- ]input[- ])?approv(?:al|e)",
            r"|(?:do not|don't|must not) auto[- ]approve|(?:requires|needs|must receive) explicit (?:human|owner) approval)\b"
        )).expect("constant approval exclusion regex")
    });
    EXCLUSION.is_match(&text)
}

/// Why an ask can never be auto-approved, if it cannot. `None` = the policy
/// decides. Measured 2026-09-27 (Ethan: "auto is on but its not continuing
/// each"): the sweep approved 0 of 74, skipping most as credential_or_access on
/// loose words: "Sign off (or amend) the MP-106 PITR API design" read as a
/// sign-in, "reachable without an API key" and "a sandbox or demo API key" as
/// credential asks. And the opposite: AMUX-5276, "Will you run `! ~/.amux/
/// seed-standing-approvals.sh`", was auto-approved although only the owner can
/// run it.
pub fn never_reason(item: &Value) -> Option<&'static str> {
    let at = item["ask_type"].as_str().unwrap_or("");
    // The QUESTION only. `unblocks` on a card the turn-end recorder filed is
    // boilerplate ("completes the sign-in, grant or credential step named in
    // the question"), so reading it made every such card look like a
    // credential ask (AMH-16, 2026-09-27: "Want me to do that pass, or would
    // you rather look at the categories yourself first?").
    let text = item["question"].as_str().filter(|q| !q.trim().is_empty())
        .or_else(|| item["title"].as_str()).unwrap_or("").to_string();
    let t = text.to_ascii_lowercase();
    // A declared credential/access ask that reads like one.
    if matches!(at, "credential" | "access") && crate::api::needs_input::reads_like_credential(&t) {
        return Some("credential_or_access");
    }
    // FIRST, so an owner-action phrasing cannot send it back to the worker
    // (BACKE-3504 "a new POST /v1/ops/events endpoint", 2026-09-27 20:40).
    // Repo rule (Mixpeek CLAUDE.md): new endpoints and new primitives need
    // explicit human approval, so a public-surface decision stays with him.
    let surface = [
        "/v1/", "new endpoint", "new primitive", "public surface", "api design", "pricing page",
        "new sourcetype", "new source type",
    ];
    // "a new collection SourceType" (BACKE-3829), "a new GET" (MF-1963).
    let new_surface = r"\bnew\b[^.?!]{0,40}\b(sourcetype|source type|primitive|endpoint|route|get|post|put|patch)\b";
    if surface.iter().any(|w| t.contains(w))
        || regex::Regex::new(new_surface).map(|r| r.is_match(&t)).unwrap_or(false)
    {
        return Some("public_surface");
    }
    // AN APPROVAL MUST BIND TO A NAMED THING (contract rule 11). 2026-10-06:
    // AH-391, "Shall I remove it?", was approved automatically; the "it" was a
    // launchd agent on the host, which only the owner may unload. A short
    // question whose object is a bare pronoun names nothing to approve.
    let words: Vec<&str> = t.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    if words.len() <= 6 && words.iter().any(|w| matches!(*w, "it" | "this" | "that" | "them" | "these" | "those")) {
        return Some("no_artifact_named");
    }
    // AN ASK TO SPEAK FOR THE OWNER is his alone (AH-394, 2026-10-07: "tell
    // the orchestrator, in your name, ..." was approved automatically, and the
    // owner-policy nudge answered the same question twice). Approval by policy
    // would put words in his mouth.
    if regex::Regex::new(r"\b(in your name|on your behalf|as your (instruction|decision|words)|in ethan'?s name|on ethan'?s behalf)\b")
        .map(|r| r.is_match(&t)).unwrap_or(false)
    {
        return Some("owner_voice");
    }
    // The host's own services (~/Dev/CLAUDE.md: no restart, reboot or launchd
    // change without the owner's explicit approval).
    if regex::Regex::new(r"\b(launchd|launchctl|launch agent|launchagent|reboot|restart the (mac|host|machine))\b")
        .map(|r| r.is_match(&t)).unwrap_or(false)
    {
        return Some("host_service");
    }
    // The ask is for the OWNER to do something: approval cannot complete it.
    let owner_act = [
        r"(^|[.?!]\s*)(can|could|will|would) you\b",
        r"\bsign(ing)?[ -]?in\b",
        r"\blog(ging)?[ -]?in\b",
        r"\bre-?auth",
        r"\bmint\b",
        r"\brotate (the|a|an|these|those|this|\d)",
        r"`!\s",
        r"\brun `",
        r"\btop (it )?up\b",
        r"\badd me\b",
        r"\bgive me\b",
        r"\binvite (me|us|amux)\b",
        r"\bpaste\b",
        r"\bshare (the|a|your)\b.*\b(key|token|password|secret)",
    ];
    if owner_act.iter().any(|re| regex::Regex::new(re).map(|r| r.is_match(&t)).unwrap_or(false)) {
        return Some("owner_must_act");
    }
    // A CALL THE TEXT RESERVES FOR THE OWNER ("your call", "Ethan's call").
    // 2026-10-01: AH-296, a goal-spec scope choice for Ethan, was approved
    // automatically minutes after he had questioned that very scope cut. An
    // either/or alone is NOT enough: the policy deliberately approves "now, or
    // wait for gs-4?" as the worker's call; the lane has to SAY the choice is
    // the owner's, and then approval cannot answer it.
    let reserved = r"\b(your call|your decision|your choice|yours to (decide|make|call)|up to you|you decide|you choose|(ethan|the owner)'?s (call|decision|choice))\b";
    if regex::Regex::new(reserved).map(|r| r.is_match(&t)).unwrap_or(false) {
        return Some("reserved_for_owner");
    }
    // THE OWNER'S OWN SHARE. A setting that keeps something back FOR the owner
    // (the plan-window reserve "keeps 30 percent for you") protects the owner
    // from automation, so automation lowering it on the owner's behalf is the
    // one answer it can never give. 2026-10-02 06:48 ET: MO-4161 ("do you want
    // the reserve lowered") was approved automatically and a lane was asked to
    // apply it (amux-helper declined).
    let own_share = r"\b(reserves?|reserved|keeps?|kept|set aside|held back|protects?)\b[^.?!]{0,80}\bfor (you|the human|the owner|ethan)\b";
    if regex::Regex::new(own_share).map(|r| r.is_match(&t)).unwrap_or(false) {
        return Some("owners_own_share");
    }
    None
}

/// Parse one number starting at byte `i` of `b` (digits, `,` thousands groups,
/// one `.`, optional `k` suffix). Returns (value, end).
fn number_at(b: &[u8], mut i: usize) -> Option<(f64, usize)> {
    let start = i;
    let mut s = String::new();
    let mut dot = false;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() {
            s.push(c as char);
            i += 1;
        } else if c == b','
            && !s.is_empty()
            && b.len() >= i + 4
            && b[i + 1..i + 4].iter().all(u8::is_ascii_digit)
            && b.get(i + 4).is_none_or(|x| !x.is_ascii_digit())
        {
            i += 1;
        } else if c == b'.' && !dot && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            dot = true;
            s.push('.');
            i += 1;
        } else {
            break;
        }
    }
    if i == start || s.is_empty() {
        return None;
    }
    let mut v: f64 = s.parse().ok()?;
    if matches!(b.get(i), Some(b'k' | b'K'))
        && b.get(i + 1).is_none_or(|x| !x.is_ascii_alphabetic())
    {
        v *= 1000.0;
        i += 1;
    }
    Some((v, i))
}

/// The largest dollar figure an ask names, or None when it names none.
///
/// A figure is a number after `$`, or before `usd`/`dollars`. The upper end
/// of a range counts ("$90-130/mo" is 130), because the cap is a promise
/// about the most that can be spent. A monthly price counts as its figure.
/// Live specimens (2026-09-27) are pinned in the tests.
pub fn max_dollar_figure(text: &str) -> Option<f64> {
    let lower = text.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut best: Option<f64> = None;
    let mut take = |v: f64| best = Some(best.map_or(v, |x: f64| x.max(v)));
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() || (i > 0 && (b[i - 1].is_ascii_digit() || b[i - 1] == b'.')) {
            i += 1;
            continue;
        }
        let Some((v, end)) = number_at(b, i) else {
            i += 1;
            continue;
        };
        let before = lower[..i].trim_end();
        let after = lower[end..].trim_start();
        if before.ends_with('$') || after.starts_with("usd") || after.starts_with("dollar") {
            take(v);
            // A range: "$90-130", "$90 - $130", "$90 to 130".
            let trimmed = lower[end..].trim_start();
            let tail = ["-", "\u{2013}", "to "]
                .iter()
                .find_map(|c| trimmed.strip_prefix(c));
            if let Some(tail) = tail {
                let tail2 = tail.trim_start().trim_start_matches('$');
                let off = lower.len() - tail2.len();
                if let Some((v2, end2)) = number_at(b, off) {
                    take(v2);
                    i = end2;
                    continue;
                }
            }
        }
        i = end;
    }
    best
}

/// What the engine does with one item.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Approve,
    /// The worker's resolved policy is off.
    Off,
    /// Only the owner can complete it (credential, an action of his, or a
    /// public-surface decision the repo rule reserves).
    Never(&'static str),
    /// The category's switch is off.
    SkipCategory(String),
    /// Money: over the cap, or no figure to compare (None).
    /// Return it to the worker: the work is the worker's to do through the
    /// access ladder (Ethan, 2026-09-27 20:23: "63 is unacceptable it needs
    /// to push the workers to do them on their own").
    SendBack(&'static str),
}

/// Can an owner-action ask go back to its worker? Not when it spends,
/// reaches outside, or touches production data (those are the owner's by
/// the standing boundary), and not when it asks the owner to record his own
/// approval (AMUX-5276: a worker must never write an owner-only approval).
pub fn can_send_back(item: &Value) -> bool {
    if matches!(category_of(item).as_str(), "money" | "outbound" | "prod_data") {
        return false;
    }
    let t = ask_text(item).to_ascii_lowercase();
    !["standing approval", "standing-approval", "seed-standing"].iter().any(|w| t.contains(w))
}

/// The category an item is approved under. An email approval is outbound.
pub fn category_of(item: &Value) -> String {
    if item["kind"].as_str() == Some("email") {
        return "outbound".into();
    }
    item["category"].as_str().unwrap_or("other").to_string()
}

/// The category the POLICY judges an ask by. An ask filed as "other" that
/// reads like a production or customer change is scored as `prod_data`, so the
/// owner's own `prod_data` switch decides it (off by default). 2026-10-02: the
/// policy approved MO-4149 (disable a webhook in the owner's org), MO-4150
/// (TubeScience ts-api replicas) and MO-4160 (promote through the TubeScience
/// production ring), all filed as "other".
fn effective_category(item: &Value) -> String {
    // The structured ask_type outranks keyword scoring (contract rule 11).
    match item["ask_type"].as_str().unwrap_or("") {
        "budget" => return "money".to_string(),
        "customer_outbound" => return "outbound".to_string(),
        _ => {}
    }
    let cat = category_of(item);
    if matches!(cat.as_str(), "money" | "prod_data" | "outbound") {
        return cat;
    }
    let t = ask_text(item).to_ascii_lowercase();
    let hit = |re: &str| regex::Regex::new(re).map(|r| r.is_match(&t)).unwrap_or(false);
    let action = r"\b(disabl|delet|drop|purg|remov|migrat|mov|retir|backfill|overwrit|restor|promot|roll ?out|rollout|scal|replica|deploy|cut ?over|rotat)\w*";
    let target = r"\b(production|prod\b|prod plane|customer|tenant|tubescience|primis|live (data|plane|cluster)|your own org)";
    if hit(action) && hit(target) {
        tracing::info!(target: "amux::needs_input_auto", verdict = "scored_as_prod_data",
            "an ask filed as other reads like a production or customer change");
        return "prod_data".to_string();
    }
    cat
}

/// A decision about a project's scope or deadline: deferring, cutting or
/// moving work. Contract rule 11 keeps these with the owner (2026-10-05: an
/// auto-approval that named no list deferred seven GS-12 plan items).
pub fn is_scope_decision(item: &Value) -> bool {
    let t = ask_text(item).to_ascii_lowercase();
    let re = r"\b(defer\w*|descope\w*|de-scope\w*|out of (the )?scope|scope (cut|change|reduction)|(?:cut|change|reduce|expand|narrow) (?:the |project |goal )?scope|cut list|done[- ]by|deadline|postpone\w*|(drop|remove) (it |them |these |those )?from (the )?(plan|scope|done))\b";
    regex::Regex::new(re).map(|r| r.is_match(&t)).unwrap_or(false)
}

/// Fleet control and changes to owner-set priorities or disabled automation
/// require a new owner decision, not an implementation-choice auto-approval.
pub(crate) fn is_owner_control_decision(item: &Value) -> bool {
    let t = ask_text(item).to_ascii_lowercase();
    static CONTROL: std::sync::LazyLock<[regex::Regex; 6]> = std::sync::LazyLock::new(|| [
        r"\b(stop|pause|halt|archive|shut ?down)\b",
        r"\b(fleet|all (the )?(workers|lanes)|every (worker|lane))\b",
        r"\b(change|shift|reorder|override|reprioriti[sz]e)\b",
        r"\bpriorit\w*\b",
        r"\b(re[- ]?enable|enable|turn (it |them )?back on|resume|restart|undo|override|overrule)\b",
        r"\b(disabled|turned off|owner[- ]stop|you (stopped|paused|held)|owner'?s (stop|hold|instruction))\b",
    ].map(|s| regex::Regex::new(s).expect("constant owner control pattern")));
    let hit = |i: usize| CONTROL[i].is_match(&t);
    (hit(0) && hit(1)) || (hit(2) && hit(3)) || (hit(4) && hit(5))
}

pub fn decide(policy: &Policy, item: &Value) -> Decision {
    if !policy.enabled {
        return Decision::Off;
    }
    if requires_explicit_approval(item) {
        return Decision::Never("explicit_approval_required");
    }
    let boundary = effective_category(item);
    if matches!(boundary.as_str(), "money" | "prod_data" | "outbound") {
        return Decision::SkipCategory(boundary);
    }
    if is_scope_decision(item) {
        return Decision::Never("scope_decision");
    }
    if is_owner_control_decision(item) {
        return Decision::Never("owner_control_decision");
    }
    if let Some(why) = never_reason(item) {
        // A worker can complete a credential step down the access ladder; it
        // cannot grant itself a host change, an approval for an unnamed thing,
        // or a public surface (AH-391, 2026-10-06: a launchd removal was sent
        // back as "yours to do").
        // Only an actual credential/access step belongs on the access ladder.
        // MO-4465: "Can you change the group setting?" is an owner action,
        // not a credential ask the worker can perform by dropping its header.
        let access_step = matches!(item["ask_type"].as_str(), Some("credential" | "access"))
            && matches!(why, "credential_or_access" | "owner_must_act");
        if policy.send_back && access_step && can_send_back(item) {
            return Decision::SendBack(why);
        }
        return Decision::Never(why);
    }
    let cat = effective_category(item);
    let on = match cat.as_str() {
        "money" => policy.money,
        "prod_data" => policy.prod_data,
        "outbound" => policy.outbound,
        _ => policy.other,
    };
    if !on {
        return Decision::SkipCategory(cat);
    }
    Decision::Approve
}

// ---------------------------------------------------------------------------
// Ledger (one prefs row)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Entry {
    pub dk: String,
    pub key: String,
    pub kind: String,
    pub card: String,
    pub worker: String,
    pub category: String,
    pub question: String,
    /// `pending` (claimed, in flight), `approved`, `refused`, or a skip:
    /// `baseline`, `snoozed`, `off`, `never`, `skipped_category`,
    /// `skipped_cap`.
    pub outcome: String,
    pub detail: String,
    pub at: f64,
}

/// Outcomes the owner's "include what is already waiting" re-evaluates.
const REEVALUABLE: [&str; 6] = [
    "baseline",
    "snoozed",
    "off",
    "never",
    "skipped_category",
    "skipped_cap",
];
/// Outcomes shown in the Auto-approved list.
const SHOWN: [&str; 4] = ["approved", "sent_back", "refused", "pending"];

fn read_pref(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM prefs WHERE key = ?1", [key], |r| r.get(0))
        .ok()
}

fn write_ledger(conn: &Connection, led: &[Entry]) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO prefs (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
        rusqlite::params![LEDGER_KEY, serde_json::to_string(led).unwrap_or_else(|_| "[]".into())],
    )
    .map(|_| ())
}

/// None when the row has never been written: the first run.
pub fn load_ledger(conn: &Connection) -> Option<Vec<Entry>> {
    read_pref(conn, LEDGER_KEY).map(|r| serde_json::from_str(&r).unwrap_or_default())
}

fn pref_event() -> crate::db::WriteOutcome {
    crate::db::WriteOutcome {
        applied: true,
        events: vec![crate::db::PendingEvent {
            entity_type: amux_core::revision::EntityType::Other("pref".into()),
            entity_id: LEDGER_KEY.into(),
            mutation: amux_core::revision::MutationKind::Updated,
            payload: None,
        }],
    }
}

/// Append rows whose `dk` is not already present. Returns the dks that were
/// newly written, which is the whole dedupe: only the tick that wrote a
/// `pending` row may act on it.
async fn append(state: &AppState, rows: Vec<Entry>, live: HashSet<String>, now: f64) -> HashSet<String> {
    let written = std::sync::Arc::new(std::sync::Mutex::new(HashSet::new()));
    let w2 = written.clone();
    let _ = state
        .store
        .write_async(move |conn| {
            let mut led = load_ledger(conn).unwrap_or_default();
            led.retain(|e| !stale_pending(e, now));
            let have: HashSet<String> = led.iter().map(|e| e.dk.clone()).collect();
            let mut added = HashSet::new();
            for r in rows {
                if !have.contains(&r.dk) && added.insert(r.dk.clone()) {
                    led.push(r);
                }
            }
            // Prune rows whose item left the queue long ago. A row whose item
            // is still waiting is kept however old it is.
            led.retain(|e| now - e.at < KEEP_S || live.contains(&e.dk));
            write_ledger(conn, &led)?;
            *w2.lock().unwrap() = added;
            Ok(pref_event())
        })
        .await;
    let out = written.lock().unwrap().clone();
    out
}

fn stale_pending(e: &Entry, now: f64) -> bool {
    e.outcome == "pending" && now - e.at > PENDING_STALE_S
}

async fn settle(state: &AppState, dk: String, outcome: &'static str, detail: String) {
    let _ = state
        .store
        .write_async(move |conn| {
            let mut led = load_ledger(conn).unwrap_or_default();
            if let Some(e) = led.iter_mut().find(|e| e.dk == dk) {
                e.outcome = outcome.into();
                e.detail = detail;
            }
            write_ledger(conn, &led)?;
            Ok(pref_event())
        })
        .await;
}

// ---------------------------------------------------------------------------
// Actions: what the Approve button does, server-side
// ---------------------------------------------------------------------------

/// The writes an approval makes, plus the batch FYI. A trait so the
/// integration test can run the job against a hermetic store without sending
/// a real email or pushing to a real phone.
#[async_trait::async_trait]
pub trait Actions: Send + Sync {
    /// Owner message to the worker. `msg_id` is stable per item, so a retry
    /// is deduped by the steering queue.
    async fn send(&self, state: &AppState, worker: &str, text: &str, msg_id: &str) -> Result<String, String>;
    /// Note on the card and needsyou -> todo, read back.
    async fn card(&self, state: &AppState, card: &str, note: &str, marker: &str) -> Result<String, String>;
    /// Release a held outbound email.
    async fn email(&self, state: &AppState, approval_id: &str) -> Result<String, String>;
    /// One FYI per batch.
    async fn fyi(&self, state: &AppState, text: &str);
}

pub struct RealActions;

/// The board half of Approve, through the ordinary PATCH handler so every
/// board gate applies, then read back: a 2xx is not proof the board stored
/// what was asked for.
pub async fn card_via_board(state: &AppState, card: &str, note: &str, marker: &str) -> Result<String, String> {
    use crate::db::board_store as bs;
    let read = |id: &str| {
        state
            .store
            .read()
            .ok()
            .and_then(|c| bs::get_issue(&c, id).ok().flatten())
    };
    let Some(before) = read(card) else {
        return Err(format!("could not read {card}"));
    };
    if before.archived != 0 {
        return Err(format!("{card} is archived"));
    }
    let already = before.desc.contains(marker);
    let moving = before.status == "needsyou";
    let mut patch = serde_json::Map::new();
    if !already {
        patch.insert("desc_append".into(), json!(format!("{note} {marker}")));
    }
    if moving {
        patch.insert("status".into(), json!("todo"));
        patch.insert("authorized_by".into(), json!(APPROVER));
    }
    if patch.is_empty() {
        return Ok(format!("{card} already recorded"));
    }
    let send = |patch: serde_json::Map<String, Value>| async move {
        let resp = crate::api::board::patch_item(
            State(state.clone()),
            axum::extract::Path(card.to_string()),
            HeaderMap::new(),
            Json(Value::Object(patch)),
        )
        .await;
        let code = resp.status();
        if code.is_success() {
            return Ok(());
        }
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap_or_default();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Err((code, v))
    };
    // A full todo queue is not a refusal of the answer: the worker already has
    // it. Retry into backlog so the card stops flagging the worker (2026-09-27:
    // mvs-infra 20/20 and amux 78/20 turned approved cards away).
    let mut landed = "todo";
    if let Err((code, v)) = send(patch.clone()).await {
        if moving && v["code"].as_str() == Some("todo_wip_limit_reached") {
            patch.insert("status".into(), json!("backlog"));
            landed = "backlog";
            if let Err((code2, v2)) = send(patch).await {
                return Err(format!("{card}: board refused {code2}: {}", v2["error"].as_str().unwrap_or("")));
            }
        } else {
            return Err(format!("{card}: board refused {code}: {}", v["error"].as_str().unwrap_or("")));
        }
    }
    let Some(back) = read(card) else {
        return Err(format!("{card}: could not read it back"));
    };
    if moving && back.status == "needsyou" {
        return Err(format!("{card} is still needsyou after the PATCH (board kept it)"));
    }
    if !already && !back.desc.contains(marker) {
        return Err(format!("{card}: the note did not land on the card"));
    }
    Ok(format!("{card} {}", if moving { format!("moved to {landed}") } else { "noted".to_string() }))
}

#[async_trait::async_trait]
impl Actions for RealActions {
    async fn send(&self, state: &AppState, worker: &str, text: &str, msg_id: &str) -> Result<String, String> {
        // The owner-policy guard: standing owner configuration, so it reaches
        // isolated lanes the way a schedule does (turn_end.rs).
        crate::api::session_verbs::steer_enqueue_idempotent_report(
            state,
            worker,
            text,
            "needs-input:auto-approve",
            "",
            msg_id,
        )
        .await
        .map_err(|e| format!("send to {worker}: {e}"))?;
        crate::api::session_verbs::steer_deliver_for_session(state, worker).await;
        Ok(format!("message queued for {worker}"))
    }

    async fn card(&self, state: &AppState, card: &str, note: &str, marker: &str) -> Result<String, String> {
        card_via_board(state, card, note, marker).await
    }

    async fn email(&self, _state: &AppState, approval_id: &str) -> Result<String, String> {
        use crate::api::email::EmailCtx;
        let ctx = std::sync::Arc::new(EmailCtx {
            client: std::sync::Arc::new(crate::integrations::email::GmailClient::new_default()),
            registry: crate::integrations::global_registry().clone(),
        });
        let mut h = HeaderMap::new();
        h.insert("x-amux-approver", axum::http::HeaderValue::from_static(APPROVER));
        let resp = crate::api::email::approve(
            axum::Extension(ctx),
            h,
            axum::extract::Path(approval_id.to_string()),
            None,
        )
        .await;
        let code = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 256 * 1024)
            .await
            .unwrap_or_default();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if code.is_success() {
            Ok(format!(
                "sent for {}",
                v["sent_for_session"].as_str().unwrap_or("worker")
            ))
        } else {
            Err(format!(
                "email approve {code}: {}",
                v["error"].as_str().unwrap_or("")
            ))
        }
    }

    async fn fyi(&self, state: &AppState, text: &str) {
        let r = crate::push::send_all(state, "amux FYI", text, "", "needs-input-auto", "/").await;
        let ok = crate::api::alerts::push_delivery_verdict(&r).is_ok();
        tracing::info!(verdict = "needs_input_auto_fyi", delivered = ok, "needs-input auto-approve FYI pushed");
    }
}

fn clip(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= n {
        s
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

/// Perform one approval, exactly as the triage sheet's Approve does.
async fn approve_item(acts: &dyn Actions, state: &AppState, item: &Value, dk: &str) -> Result<String, String> {
    if item["kind"].as_str() == Some("email") {
        let id = item["approval_id"].as_str().unwrap_or("");
        return acts.email(state, id).await;
    }
    let card = item["card"].as_str().unwrap_or("");
    let worker = item["worker"].as_str().unwrap_or("");
    let q = clip(
        item["question"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .or(item["title"].as_str())
            .unwrap_or(""),
        300,
    );
    let r = if card.is_empty() { String::new() } else { format!(" ({card})") };
    let mut done = Vec::new();
    if !worker.is_empty() {
        let text = format!(
            "Approved{r}: {q}. Proceed. (Approved automatically under the owner's needs-input policy.)"
        );
        // A refused send leaves the card alone: moving it with nobody told
        // would clear the owner's queue while the worker still waits. A
        // PAUSED lane is the exception: it is not waiting, and the answer on
        // its card is what it reads when resumed (RH-131, BR-78, MS-1234).
        done.push(send_or_paused(acts, state, worker, &text, &format!("ni-auto-{}", hash(dk))).await?);
    }
    if !card.is_empty() {
        let stamp = chrono::Local::now().format("%b %d %H:%M");
        let note = format!(
            "[{stamp}] Approved automatically by {APPROVER}, category {}.",
            category_of(item)
        );
        let marker = format!("#ni-auto-{}", hash(dk));
        done.push(acts.card(state, card, &note, &marker).await?);
    }
    if done.is_empty() {
        return Err("nothing to approve: no worker and no card".into());
    }
    Ok(done.join("; "))
}

async fn send_or_paused(acts: &dyn Actions, state: &AppState, worker: &str, text: &str, id: &str) -> Result<String, String> {
    match acts.send(state, worker, text, id).await {
        Err(e) if e.contains("target is paused") => Ok(format!("{worker} is paused; answer recorded on the card for when it resumes")),
        other => other,
    }
}

/// What a sent-back worker is told. The ladder is the standing rule in
/// ~/.claude/CLAUDE.md; the rotation and data clauses keep a send-back from
/// becoming an unreviewed production change.
pub fn send_back_text(card: &str, q: &str) -> String {
    let r = if card.is_empty() { String::new() } else { format!(" ({card})") };
    format!(
        "Sent back{r}: \"{q}\". This is yours to do; Ethan's needs-input policy returns key, sign-in, grant \
         and rotation asks to the worker. Reach it yourself down the ladder: 1) amux browser with a saved \
         profile, 2) CDP into Ethan's Chrome (/chrome-cdp, his sign-ins are live), 3) CUA (`amux computer`). \
         Mint keys in the UI you reach and store them where the ask says; never print a secret. Rotate by \
         replacement: create the new one, move every consumer, verify, then revoke the old. Stop and re-file \
         needsyou only if all three rungs fail, or the step spends money, reaches someone outside, or \
         re-encrypts or migrates production data; say which rung failed and why. It will not be sent back twice."
    )
}

/// Return one ask to its worker: tell it, then move the card to its queue.
async fn send_back_item(acts: &dyn Actions, state: &AppState, item: &Value, dk: &str) -> Result<String, String> {
    let card = item["card"].as_str().unwrap_or("");
    let worker = item["worker"].as_str().unwrap_or("");
    if worker.is_empty() || card.is_empty() {
        return Err("no worker or card to send it back to".into());
    }
    let q = clip(item["question"].as_str().filter(|s| !s.trim().is_empty()).or(item["title"].as_str()).unwrap_or(""), 300);
    let mut done = vec![send_or_paused(acts, state, worker, &send_back_text(card, &q), &format!("ni-back-{}", hash(dk))).await?];
    let stamp = chrono::Local::now().format("%b %d %H:%M");
    let note = format!(
        "[{stamp}] Sent back to {worker} by owner policy (needs-input send-back): do it through the access \
         ladder (amux browser, CDP, CUA); re-file needsyou only with the rung that failed."
    );
    done.push(acts.card(state, card, &note, &format!("#ni-back-{}", hash(dk))).await?);
    Ok(done.join("; "))
}

// ---------------------------------------------------------------------------
// The tick
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct TickReport {
    pub ran: bool,
    pub considered: usize,
    pub baseline: usize,
    pub approved: Vec<String>,
    pub sent_back: Vec<String>,
    pub refused: Vec<String>,
    pub skipped_cap: Vec<String>,
    pub skipped_category: Vec<String>,
    pub off: Vec<String>,
}

fn entry_for(item: &Value, dk: &str, outcome: &str, detail: String, now: f64) -> Entry {
    Entry {
        dk: dk.to_string(),
        key: item["key"].as_str().unwrap_or("").into(),
        kind: item["kind"].as_str().unwrap_or("").into(),
        card: item["card"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or(item["approval_id"].as_str())
            .unwrap_or("")
            .into(),
        worker: item["worker"].as_str().unwrap_or("").into(),
        category: category_of(item),
        question: clip(item["question"].as_str().unwrap_or(""), 300),
        outcome: outcome.into(),
        detail,
        at: now,
    }
}

pub async fn tick_with(acts: &dyn Actions, state: &AppState, home: &Path, now: f64) -> TickReport {
    let mut rep = TickReport::default();
    if server_kill(home) {
        return rep;
    }
    let h2 = home.to_path_buf();
    let read = state
        .store
        .read_async(move |conn| Ok((load_ledger(conn), needs_input::build(conn, &h2, now)?)))
        .await;
    let (ledger, queue) = match read {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(verdict = "needs_input_auto_unmeasured", error = %e,
                "needs-input auto-approve could not read the queue; nothing evaluated");
            return rep;
        }
    };
    rep.ran = true;
    rep.considered = queue.items.len();
    let live: HashSet<String> = queue
        .items
        .iter()
        .chain(queue.snoozed.iter())
        .map(dedupe_key)
        .collect();

    // FIRST RUN: everything already waiting is the baseline, approved by
    // nobody. "Include what is already waiting" is the owner's explicit sweep.
    let Some(ledger) = ledger else {
        let rows: Vec<Entry> = queue
            .items
            .iter()
            .chain(queue.snoozed.iter())
            .map(|it| entry_for(it, &dedupe_key(it), "baseline", String::new(), now))
            .collect();
        rep.baseline = rows.len();
        append(state, rows, live, now).await;
        tracing::info!(verdict = "needs_input_auto_baseline", measured = true, n_considered = rep.baseline,
            "needs-input auto-approve: first run recorded what is already waiting; none of it is approved");
        return rep;
    };
    let seen: HashSet<String> = ledger.iter().filter(|e| !stale_pending(e, now)).map(|e| e.dk.clone()).collect();
    // Sent back once already: a re-ask (any question) on that card is the
    // worker saying the ladder failed, so it stays with the owner.
    let sent_back_keys: HashSet<String> =
        ledger.iter().filter(|e| e.outcome == "sent_back").map(|e| e.key.clone()).collect();

    let mut skips = Vec::new();
    // Snoozed items are never touched: first seen snoozed means never
    // auto-approved.
    for it in &queue.snoozed {
        let dk = dedupe_key(it);
        if !seen.contains(&dk) {
            skips.push(entry_for(it, &dk, "snoozed", String::new(), now));
        }
    }
    let mut policies: HashMap<String, Policy> = HashMap::new();
    let mut to_approve: Vec<(&Value, String)> = Vec::new();
    let mut to_send_back: Vec<(&Value, String)> = Vec::new();
    for item in &queue.items {
        let dk = dedupe_key(item);
        if seen.contains(&dk) {
            continue;
        }
        let key = item["key"].as_str().unwrap_or("").to_string();
        let worker = item["worker"].as_str().unwrap_or("").to_string();
        let policy = policies
            .entry(worker.clone())
            .or_insert_with(|| resolve(home, &worker))
            .clone();
        let label = entry_for(item, &dk, "", String::new(), now).card;
        let label = if label.is_empty() { key.clone() } else { label };
        let decision = match decide(&policy, item) {
            Decision::SendBack(why) if sent_back_keys.contains(&key) => {
                tracing::info!(verdict = "needs_input_auto_skipped_category", key = %key, worker = %worker,
                    reason = "sent_back_once", ask = why,
                    "needs-input auto: already sent back once; the re-ask stays with the owner");
                Decision::Never("sent_back_once")
            }
            d => d,
        };
        match decision {
            Decision::Approve => to_approve.push((item, dk)),
            Decision::SendBack(_) => to_send_back.push((item, dk)),
            Decision::Off => {
                tracing::info!(verdict = "needs_input_auto_skipped_category", key = %key, worker = %worker,
                    reason = "policy_off", source = %policy.sources.get("enabled").cloned().unwrap_or_default(),
                    "needs-input auto-approve is off for this worker");
                skips.push(entry_for(item, &dk, "off", String::new(), now));
                rep.off.push(label);
            }
            Decision::Never(why) => {
                tracing::info!(verdict = "needs_input_auto_skipped_category", key = %key, worker = %worker,
                    reason = why,
                    "needs-input auto-approve: only the owner can complete this ask");
                let label_why = match why {
                    "owner_must_act" => "only you can do this (it asks you to act)",
                    "public_surface" => "a new endpoint or public surface: yours by the repo rule",
                    "owner_control_decision" => "fleet control, priorities and reversals of owner actions require an owner decision",
                    "sent_back_once" => "sent back once already; the worker re-asked, so it is yours",
                    "scope_decision" => "a scope or deadline decision stays with you (contract rule 11)",
                    "explicit_approval_required" => "this ask requires explicit approval; automatic approval does not cover it",
                    _ => "credential or access: only you can do it",
                };
                skips.push(entry_for(item, &dk, "never", label_why.into(), now));
                rep.skipped_category.push(label);
            }
            Decision::SkipCategory(c) => {
                tracing::info!(verdict = "needs_input_auto_skipped_category", key = %key, worker = %worker,
                    category = %c, reason = "category_off",
                    "needs-input auto-approve: this category's switch is off");
                skips.push(entry_for(item, &dk, "skipped_category", format!("{c} stays with you (contract rule 11)"), now));
                rep.skipped_category.push(label);
            }
        }
    }
    // Claim every approval BEFORE sending anything: only rows this tick wrote
    // are acted on, so two ticks (or a restart mid-batch) cannot double-send.
    let mut rows = skips;
    rows.extend(to_approve.iter().chain(to_send_back.iter()).map(|(it, dk)| entry_for(it, dk, "pending", String::new(), now)));
    let written = if rows.is_empty() {
        HashSet::new()
    } else {
        append(state, rows, live, now).await
    };
    let mut fyi_lines = Vec::new();
    for (item, dk) in to_approve {
        if !written.contains(&dk) {
            continue;
        }
        let e = entry_for(item, &dk, "", String::new(), now);
        let label = if e.card.is_empty() { e.key.clone() } else { e.card.clone() };
        match approve_item(acts, state, item, &dk).await {
            Ok(detail) => {
                tracing::info!(verdict = "needs_input_auto_approved", key = %e.key, card = %e.card,
                    worker = %e.worker, category = %e.category, detail = %detail,
                    "needs-input item approved automatically under owner policy");
                settle(state, dk, "approved", detail).await;
                fyi_lines.push(format!(
                    "{label}{}: {}",
                    if e.worker.is_empty() { String::new() } else { format!(" ({})", e.worker) },
                    clip(&e.question, 80)
                ));
                rep.approved.push(label);
            }
            Err(err) => {
                tracing::warn!(verdict = "needs_input_auto_refused", key = %e.key, card = %e.card,
                    worker = %e.worker, category = %e.category, error = %err,
                    "needs-input auto-approve refused; the item stays in the queue for the owner");
                settle(state, dk, "refused", err).await;
                rep.refused.push(label);
            }
        }
    }
    for (item, dk) in to_send_back {
        if !written.contains(&dk) {
            continue;
        }
        let e = entry_for(item, &dk, "", String::new(), now);
        match send_back_item(acts, state, item, &dk).await {
            Ok(detail) => {
                tracing::info!(verdict = "needs_input_auto_sent_back", key = %e.key, card = %e.card,
                    worker = %e.worker, detail = %detail,
                    "needs-input ask returned to its worker with the access ladder");
                settle(state, dk, "sent_back", detail).await;
                rep.sent_back.push(e.card.clone());
            }
            Err(err) => {
                tracing::warn!(verdict = "needs_input_auto_refused", key = %e.key, card = %e.card,
                    worker = %e.worker, error = %err,
                    "needs-input send-back refused; the item stays in the queue for the owner");
                settle(state, dk, "refused", err).await;
                rep.refused.push(e.card.clone());
            }
        }
    }
    let mut parts = Vec::new();
    if !fyi_lines.is_empty() {
        let n = fyi_lines.len();
        let mut t = format!(
            "Auto-approved {n} needs-input item{}: {}",
            if n == 1 { "" } else { "s" },
            fyi_lines.iter().take(3).cloned().collect::<Vec<_>>().join("; ")
        );
        if n > 3 {
            t.push_str(&format!("; and {} more", n - 3));
        }
        parts.push(t);
    }
    if !rep.sent_back.is_empty() {
        let n = rep.sent_back.len();
        let mut t = format!(
            "Sent {n} back to their workers to do themselves: {}",
            rep.sent_back.iter().take(6).cloned().collect::<Vec<_>>().join(", ")
        );
        if n > 6 {
            t.push_str(&format!(" and {} more", n - 6));
        }
        parts.push(t);
    }
    if !parts.is_empty() {
        let text = parts.join(". ");
        acts.fyi(state, &text).await;
    }
    rep
}

/// The owner's explicit "include what is already waiting": forget the skip
/// rows of items still in the queue (never approved or refused ones), then
/// tick, so each is evaluated once more against its worker's policy now.
pub async fn sweep_with(acts: &dyn Actions, state: &AppState, home: &Path, now: f64) -> (usize, TickReport) {
    let h2 = home.to_path_buf();
    let cleared = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c2 = cleared.clone();
    let _ = state
        .store
        .write_async(move |conn| {
            let q = needs_input::build(conn, &h2, now)?;
            let waiting: HashSet<String> = q.items.iter().map(dedupe_key).collect();
            let mut led = load_ledger(conn).unwrap_or_default();
            let before = led.len();
            led.retain(|e| !(waiting.contains(&e.dk) && REEVALUABLE.contains(&e.outcome.as_str())));
            c2.store(before - led.len(), std::sync::atomic::Ordering::SeqCst);
            write_ledger(conn, &led)?;
            Ok(pref_event())
        })
        .await;
    let n = cleared.load(std::sync::atomic::Ordering::SeqCst);
    tracing::info!(verdict = "needs_input_auto_sweep", measured = true, n_considered = n,
        "needs-input auto-approve: owner asked to include what is already waiting");
    (n, tick_with(acts, state, home, now).await)
}

// ---------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------

fn view(home: &Path, worker: &str, ledger: &[Entry], waiting: &[Value], now: f64) -> Value {
    let p = resolve(home, worker);
    let recent: Vec<&Entry> = ledger
        .iter()
        .rev()
        .filter(|e| now - e.at < SHOW_S && SHOWN.contains(&e.outcome.as_str()))
        .collect();
    let by_dk: HashMap<&str, &str> = ledger.iter().map(|e| (e.dk.as_str(), e.outcome.as_str())).collect();
    let already_waiting = waiting
        .iter()
        .filter(|it| by_dk.get(dedupe_key(it).as_str()).is_some_and(|o| REEVALUABLE.contains(o)))
        .count();
    let who = if worker.is_empty() { "all workers".to_string() } else { worker.to_string() };
    let defaults = Policy::default();
    json!({
        "worker": worker,
        "resolved": p.to_json(),
        "summary": p.summary(&who),
        "keys": FIELDS.iter().map(|(f, k, _)| (f.to_string(), json!(k))).collect::<serde_json::Map<_, _>>(),
        "defaults": {"enabled": defaults.enabled, "send_back": defaults.send_back, "other": defaults.other, "money": defaults.money,
                     "money_cap_usd": defaults.money_cap_usd, "prod_data": defaults.prod_data, "outbound": defaults.outbound},
        "kill_switch": {"var": KILL_SWITCH, "server_env_off": server_kill(home)},
        "precedence": "worker > group > global (amux.env) > default; AMUX_NEEDS_INPUT_AUTO=0 in server.env stops it everywhere",
        "never": "asks explicitly requiring human or owner approval stay with the owner; credential and access asks (keys, sign-ins, grants) are never approved automatically; with send_back on they go back to the worker once, with the access ladder",
        "waiting_now": waiting.len(),
        "already_waiting_unapproved": already_waiting,
        "recent": recent,
        "recent_count": recent.len(),
    })
}

async fn read_view(state: &AppState, worker: String) -> Response {
    let home = crate::config::amux_home();
    let now = now_f64();
    let h2 = home.clone();
    let res = state
        .store
        .read_async(move |conn| {
            let items = needs_input::build(conn, &h2, now).map(|q| q.items).unwrap_or_default();
            Ok((load_ledger(conn).unwrap_or_default(), items))
        })
        .await;
    match res {
        Ok((led, items)) => Json(crate::api::measured::measured(
            view(&home, &worker, &led, &items, now),
            led.len(),
        ))
        .into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(crate::api::measured::unmeasured(
                json!({"error": e.to_string()}),
                "the prefs store could not be read",
            )),
        )
            .into_response(),
    }
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 120
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !s.starts_with('.')
}

/// GET /api/needs-input/auto[?worker=<name>]: the resolved policy (with the
/// layer each value comes from), the last 7 days of auto-approvals, and how
/// many waiting items the explicit sweep would re-evaluate.
async fn get_policy(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let worker = q.get("worker").map(|s| s.trim().to_string()).unwrap_or_default();
    if !worker.is_empty() && !valid_name(&worker) {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": "bad worker name"}))).into_response();
    }
    read_view(&state, worker).await
}

/// PUT /api/needs-input/auto `{level, name, values}`: owner-only. `level` is
/// `global`, `group` or `worker`; `values` maps any of `enabled`, `other`,
/// `money`, `money_cap_usd`, `prod_data`, `outbound` to a value, or to null
/// to remove it at that level (inherit). Writes the same scope env files the
/// Scope tab edits.
async fn put_policy(State(state): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    if let Some(r) = needs_input::refuse_worker(&headers, "auto_policy") {
        return r;
    }
    let bad = |m: &str| (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": m}))).into_response();
    let b = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let level = b["level"].as_str().unwrap_or("global").trim().to_string();
    let name = b["name"].as_str().unwrap_or("").trim().to_string();
    let Some(values) = b["values"].as_object() else {
        return bad("send {level, name, values: {field: value | null}}");
    };
    let home = crate::config::amux_home();
    let path = match level.as_str() {
        "global" => home.join("amux.env"),
        "group" if valid_name(&name) => home.join("env").join(format!("{}.env", name.to_lowercase())),
        "worker" if valid_name(&name) => {
            let p = home.join("sessions").join(format!("{name}.env"));
            if !p.exists() {
                return (StatusCode::NOT_FOUND, Json(json!({"ok": false, "error": format!("no worker named {name}")}))).into_response();
            }
            p
        }
        "group" | "worker" => return bad("name must be a worker or group name"),
        _ => return bad("level must be global, group or worker"),
    };
    let mut updates: Vec<(String, Option<String>)> = Vec::new();
    for (field, v) in values {
        let Some((_, key, _)) = FIELDS.iter().find(|(f, _, _)| f == field) else {
            return bad(&format!("unknown field {field}"));
        };
        let val = match (field.as_str(), v) {
            (_, Value::Null) => None,
            ("money_cap_usd", v) => match v.as_f64() {
                Some(c) if c.is_finite() && (0.0..=100_000.0).contains(&c) => Some(fmt_usd(c)),
                _ => return bad("money_cap_usd must be a number from 0 to 100000"),
            },
            (_, Value::Bool(on)) => Some(if *on { "1".into() } else { "0".into() }),
            _ => return bad(&format!("{field} must be true, false or null")),
        };
        updates.push((key.to_string(), val));
    }
    if let Err(e) = crate::api::session_verbs::EnvFile::merge_plain(&path, &updates) {
        tracing::warn!(verdict = "needs_input_auto_policy", outcome = "write_error", error = %e,
            path = %path.display(), "needs-input auto-approve policy could not be written");
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": e.to_string()}))).into_response();
    }
    tracing::info!(verdict = "needs_input_auto_policy", outcome = "ok", level = %level, name = %name,
        updates = ?updates, "needs-input auto-approve policy changed by the owner");
    read_view(&state, if level == "worker" { name } else { String::new() }).await
}

/// POST /api/needs-input/auto/sweep: owner-only. Apply the policy to what is
/// already waiting.
async fn sweep(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(r) = needs_input::refuse_worker(&headers, "auto_sweep") {
        return r;
    }
    let home = crate::config::amux_home();
    let (n, rep) = sweep_with(&RealActions, &state, &home, now_f64()).await;
    Json(json!({"ok": true, "reevaluated": n, "report": rep})).into_response()
}

#[cfg(test)]
mod tests;
