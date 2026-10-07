//! Orchestration contract rules 1 and 2 for ordinary code cards (AH-375, AH-376).
//!
//! docs/orchestration-contract.md. Rule 1: a code card cannot enter `doing`
//! without acceptance criteria and a verify command, and both are frozen there.
//! Rule 2: `done` is granted by the server, which measures the lane's committed
//! HEAD and runs the frozen command in a clean detached checkout of it; the
//! worker's pasted evidence and its own gate list no longer decide. A typed
//! `cannot_satisfy` exit replaces worker `force`.
//!
//! Scoped switch `AMUX_CONTRACT_DONE` (server.env, then worker, group, global),
//! off unless set, so a rule rolls out amux lanes first, then gs12-platform,
//! then the fleet. Acceptance is measured on GS-12, not on merge.
//!
//! Log verdicts (rule 14 counters): contract_doing_refused, contract_frozen,
//! contract_frozen_edit_refused, contract_force_refused, contract_verify_started,
//! contract_verify_passed, contract_verify_failed, contract_cannot_satisfy.
//!
//! Rule 2b (AH-377): every contract has a verification kind. `code`, `proof`
//! and `artifact` are all checked the same way at done (the frozen command
//! names the test, the proof run's result or the artifact), so they share one
//! path. `deploy` adds a frozen post-deploy check: after done, the server
//! watches production's deployed sha (the lane's `AMUX_DEPLOY_SHA_CMD`), and
//! once production contains the verified commit it runs the check and records
//! the result on the card. Three failures reopen the card to doing. Verdicts:
//! contract_deploy_passed, contract_deploy_retry, contract_deploy_failed,
//! contract_deploy_stale, contract_deploy_unmeasured.
//!
//! Rule 3 (AH-378): `verified` is granted only by a reviewer the harness
//! spawns: fresh (a one-shot `--print` run, no session), read-only (read and
//! git-read tools only, in a detached checkout of the verified commit), on a
//! different model from the lane where one is available, never the author. A
//! card becomes eligible when its check passed (and, for a deploy card, its
//! post-deploy check). Pass moves it to verified; fail sends the findings back
//! to the lane and reopens it; three failed rounds go to the owner. Workers
//! cannot set verified on a contract card. Verdicts: contract_review_started,
//! contract_review_passed, contract_review_failed, contract_review_escalated,
//! contract_review_unmeasured, contract_verified_refused.
//!
//! Rule 9 (AH-390): hub and spoke. On a contract lane's board only the lane,
//! the owner and the lane's hub (`AMUX_CONTRACT_HUB`, usually set at group
//! scope to the orchestrator) may clear an ask (needsyou, blocked) or assign
//! work. A peer's `authorized_by` or the legacy `AMUX_BOARD_DELEGATION`
//! opt-in no longer counts; a peer's message is data, and `amux signal raise`
//! is how a peer clears a wait. Verdicts: rule9_peer_approval_refused,
//! rule9_peer_delegation_refused.
use crate::api::AppState;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const SWITCH: &str = "AMUX_CONTRACT_DONE";
/// A lane's default verify command when a card names none.
pub const DEFAULT_VERIFY: &str = "CC_VERIFY";
pub const ACTOR: &str = "harness:contract";
const DEFAULT_TIMEOUT_S: u64 = 1800;
/// Verification kinds (rule 1). Only `deploy` has a path of its own.
pub const KINDS: &[&str] = &["code", "deploy", "proof", "artifact"];
/// A lane's command that prints production's deployed sha, e.g.
/// `curl -s https://api.mixpeek.com/version | jq -r .deploy_sha`.
pub const DEPLOY_SHA_CMD: &str = "AMUX_DEPLOY_SHA_CMD";
/// A lane's default post-deploy check when a deploy card names none.
pub const DEFAULT_DEPLOY_CHECK: &str = "CC_DEPLOY_CHECK";
const DEPLOY_TRIES: i64 = 3;
const DEPLOY_CHECK_TIMEOUT_S: u64 = 600;
const DEPLOY_STALE_S: f64 = 24.0 * 3600.0;

/// A lane's contract defaults, read from its scoped settings.
#[derive(Default)]
pub struct Defaults {
    pub verify: Option<String>,
    pub deploy_check: Option<String>,
    /// Whether the lane can observe production's sha at all.
    pub deploy_probe: bool,
}

impl Defaults {
    pub fn for_lane(home: &Path, lane: &str) -> Self {
        Defaults {
            verify: lane_setting(home, lane, DEFAULT_VERIFY),
            deploy_check: lane_setting(home, lane, DEFAULT_DEPLOY_CHECK),
            deploy_probe: lane_setting(home, lane, DEPLOY_SHA_CMD).is_some_and(|v| !v.trim().trim_matches('"').is_empty()),
        }
    }
}

/// What the PATCH route should do with a request.
pub enum Action {
    /// Not a contract case: let the ordinary path handle it.
    Pass,
    /// Answer with this response and do nothing else.
    Respond(Response),
    /// Replace the request body (cannot_satisfy becomes a needsyou ask).
    Rewrite(Value),
    /// Let it through, then freeze this contract if the transition succeeds.
    PassThenFreeze(Contract),
    /// Replace the frozen verify command once (new command, reason).
    Amend(String, String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Contract {
    pub card: String,
    pub acceptance: String,
    pub command: String,
    pub hash: String,
    pub state: String,
    pub sha: Option<String>,
    pub amended: bool,
    pub kind: String,
    pub deploy_check: Option<String>,
}

/// The command recorded for a card the harness reviews without a contract
/// (it reached done before the contract reached its lane). It is never run.
pub const UNCONTRACTED_CMD: &str = "(none: reviewed from evidence)";

impl Contract {
    /// A review record for a card that has no frozen contract. Rules 1 and 2
    /// treat the card as uncontracted; only the reviewer reads this row.
    pub fn is_uncontracted(&self) -> bool {
        self.command == UNCONTRACTED_CMD
    }
}

fn truthy(v: &str) -> bool {
    matches!(v.trim().trim_matches('"').to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// A lane's effective value for `key`: server.env wins, then worker, group,
/// global, the precedence every scoped switch uses.
pub fn lane_setting(home: &Path, lane: &str, key: &str) -> Option<String> {
    if let Some(v) = crate::config::parse_env_file(&home.join("server.env")).get(key) {
        return Some(v.clone());
    }
    for layer in crate::api::session_verbs::scope_env_layers(home, lane).iter().rev() {
        if let Some(v) = crate::config::parse_env_file(layer).get(key) {
            return Some(v.clone());
        }
    }
    None
}

pub fn enabled_for(home: &Path, lane: &str) -> bool {
    !lane.is_empty() && lane_setting(home, lane, SWITCH).is_some_and(|v| truthy(&v))
}

/// Rules held back while the contract is on for a lane, by number, from the
/// scoped `AMUX_CONTRACT_RULES_OFF` (e.g. "6,7"). Lets a rollout widen one
/// rule at a time: 2026-10-06 gs12-platform took rules 1, 2, 3 and 8 without
/// recycling twenty working sessions (7) or budget-parking long cards (6).
pub fn rule_on(home: &Path, lane: &str, rule: &str) -> bool {
    enabled_for(home, lane)
        && !lane_setting(home, lane, "AMUX_CONTRACT_RULES_OFF")
            .is_some_and(|v| v.trim().trim_matches('"').split(',').any(|r| r.trim().eq_ignore_ascii_case(rule)))
}

/// The hub that may act on `lane`'s board under rule 9.
pub const HUB: &str = "AMUX_CONTRACT_HUB";

/// Rule 9: whether `caller` (a worker lane) is a PEER of contract lane `lane`,
/// so it may not clear `lane`'s asks or assign it work. None when rule 9 does
/// not apply (switch off, same lane, no worker caller).
pub fn rule9_peer(home: &Path, lane: &str, caller: &str) -> Option<bool> {
    let caller = caller.trim();
    if caller.is_empty() || caller == lane || !enabled_for(home, lane) {
        return None;
    }
    let hub = lane_setting(home, lane, HUB).map(|v| v.trim().trim_matches('"').to_string());
    Some(hub.as_deref() != Some(caller))
}

/// Rules 2 and 3 inside `db::advance`, the engine every status change uses
/// (AH-374). On a lane where rule 2 is on, a code card reaches `done` only by
/// the server's own check and `verified` only by the harness reviewer; the
/// owner (owner token, or `force`, both logged by their callers) is exempt.
/// Returns the refusal reason, or None to proceed.
pub fn advance_gate(conn: &Connection, row: &crate::db::board_store::IssueRow, destination: &str, actor: &str, force: bool) -> Option<String> {
    if force || !matches!(destination, "done" | "verified") || row.item_type != "code" {
        return None;
    }
    if actor == ACTOR || actor == crate::api::auth::OWNER_TOKEN_ACTOR || actor == crate::api::turn_end::owner_name() {
        return None;
    }
    let lane = row.session.as_deref().unwrap_or("");
    if !rule_on(&crate::config::amux_home(), lane, "2") {
        return None;
    }
    // A card finished before the contract reached its lane has no contract
    // and no reviewer will run for it, so its verified path stays as it was.
    if destination == "verified" && load(conn, &row.id).ok().flatten().is_none() {
        return None;
    }
    tracing::info!(card = %row.id, lane, actor, destination, measured = true, n_considered = 1,
        verdict = "contract_advance_refused", "a non-PATCH path tried to finish a contract code card");
    Some(format!(
        "contract rule {}: on {lane}, {} for a code card is granted by the server (PATCH status done starts its check; the harness reviewer grants verified), not by {actor}",
        if destination == "done" { "2" } else { "3" }, destination
    ))
}

fn hash_of(acceptance: &str, command: &str, deploy_check: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(acceptance.as_bytes());
    h.update([0u8]);
    h.update(command.as_bytes());
    if let Some(d) = deploy_check {
        h.update([0u8]);
        h.update(d.as_bytes());
    }
    format!("{:x}", h.finalize())[..16].to_string()
}

fn nonempty(v: Option<&str>) -> Option<String> {
    v.map(str::trim).filter(|s| !s.is_empty() && *s != "[]" && *s != "null").map(String::from)
}

pub fn load(conn: &Connection, card: &str) -> rusqlite::Result<Option<Contract>> {
    conn.query_row(
        "SELECT card, acceptance, command, hash, state, sha, amended, kind, deploy_check FROM card_contracts WHERE card = ?1",
        [card],
        |r| {
            Ok(Contract {
                card: r.get(0)?,
                acceptance: r.get(1)?,
                command: r.get(2)?,
                hash: r.get(3)?,
                state: r.get(4)?,
                sha: r.get(5)?,
                amended: r.get::<_, i64>(6)? != 0,
                kind: r.get(7)?,
                deploy_check: r.get(8)?,
            })
        },
    )
    .optional()
}

fn save(conn: &Connection, c: &Contract, now: f64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO card_contracts (card, acceptance, command, hash, frozen_at, state, kind, deploy_check) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(card) DO UPDATE SET acceptance = ?2, command = ?3, hash = ?4, frozen_at = ?5, state = ?6, kind = ?7, deploy_check = ?8",
        rusqlite::params![c.card, c.acceptance, c.command, c.hash, now, c.state, c.kind, c.deploy_check],
    )
    .map(|_| ())
}

fn set_state(conn: &Connection, card: &str, state: &str, sha: Option<&str>, log: &str, now: f64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE card_contracts SET state = ?2, sha = COALESCE(?3, sha), log = ?4, at = ?5 WHERE card = ?1",
        rusqlite::params![card, state, sha, log, now],
    )
    .map(|_| ())
}

fn refuse(status: StatusCode, code: &str, error: String, fix: Value) -> Response {
    (status, Json(json!({"ok": false, "code": code, "error": error, "how_to_fix": fix, "contract": "docs/orchestration-contract.md"}))).into_response()
}

/// The card facts a decision needs, read once by the route.
pub struct Card {
    pub id: String,
    pub lane: String,
    pub status: String,
    pub item_type: String,
    pub acceptance: Option<String>,
}

/// Rules 1 and 2 as a pure decision, so each branch is testable without a
/// server. `existing` is the card's frozen contract, if any.
pub fn decide(card: &Card, body: &Value, owner: bool, existing: Option<&Contract>, defaults: &Defaults) -> Action {
    if card.item_type != "code" {
        return Action::Pass;
    }
    // The verify command (HOW it is checked) may be amended once by the lane
    // or its orchestrator, with a reason, logged; the acceptance criteria
    // (WHAT must hold) stay owner-only. GE2-8, 2026-10-05: a command that
    // could not run in the clean checkout had no truthful way to change.
    if let (Some(c), Some(new_cmd)) = (existing, nonempty(body.get("verify_cmd").and_then(Value::as_str))) {
        if !owner && body.get("acceptance_criteria").is_none() {
            let reason = nonempty(body.get("reason").and_then(Value::as_str));
            return match (c.amended, reason) {
                (true, _) => Action::Respond(refuse(StatusCode::CONFLICT, "contract_amended_once",
                    format!("{}'s verify command was already amended once", card.id),
                    json!({"next": "the owner can change it again, or PATCH {\"status\":\"cannot_satisfy\",\"reason\":\"...\"}"}))),
                (_, None) => Action::Respond(refuse(StatusCode::CONFLICT, "contract_amend_needs_reason",
                    "amending a frozen verify command needs a reason".into(),
                    json!({"patch": {"verify_cmd": "...", "reason": "why the frozen command cannot run as written"}}))),
                (_, Some(r)) => Action::Amend(new_cmd, r),
            };
        }
    }
    let edits_contract = ["acceptance_criteria", "verify_cmd", "verify_kind", "deploy_check"].iter().any(|k| body.get(*k).is_some());
    if edits_contract && existing.is_some() && !owner {
        tracing::info!(card = %card.id, lane = %card.lane, measured = true, n_considered = 1,
            verdict = "contract_frozen_edit_refused", "a frozen contract was edited by a worker");
        return Action::Respond(refuse(StatusCode::CONFLICT, "contract_frozen",
            format!("{}'s acceptance criteria and verify command were frozen when it entered doing", card.id),
            json!({"owner": "the owner can change a frozen contract", "worker": "if the contract cannot be met as written, PATCH {\"status\":\"cannot_satisfy\",\"reason\":\"...\"}"})));
    }
    if body.get("force").and_then(Value::as_bool) == Some(true) && !owner {
        tracing::info!(card = %card.id, lane = %card.lane, measured = true, n_considered = 1,
            verdict = "contract_force_refused", "a worker tried to force a contract card");
        return Action::Respond(refuse(StatusCode::FORBIDDEN, "contract_force_owner_only",
            "force on a contract card is the owner's; a worker's exit is cannot_satisfy".into(),
            json!({"worker": "PATCH {\"status\":\"cannot_satisfy\",\"reason\":\"...\"}"})));
    }
    // A card already in doing with no frozen contract (it entered doing before
    // contracts, or by a route that skipped the freeze) gets one from a PATCH
    // that carries the fields, so "add acceptance_criteria and verify_cmd,
    // then request done again" is a path that exists. Before this the fields
    // passed through untouched and were dropped: verify_cmd is not a card
    // column (gs12-spend, GS-215, 2026-10-06).
    let status = body.get("status").and_then(Value::as_str).unwrap_or("");
    if card.status == "doing" && existing.is_none() && (status.is_empty() || status == "doing")
        && ["acceptance_criteria", "verify_cmd", "verify_kind", "deploy_check"].iter().any(|k| body.get(*k).is_some())
    {
        return freeze_from(card, body, existing, defaults);
    }
    match body.get("status").and_then(Value::as_str).unwrap_or("") {
        "cannot_satisfy" => {
            if let Err(why) = left_undone_items(body) {
                return Action::Respond(left_undone_refusal(card, &why));
            }
            let reason = nonempty(body.get("reason").and_then(Value::as_str))
                .or_else(|| nonempty(body.get("desc_append").and_then(Value::as_str)))
                .unwrap_or_else(|| "no reason given".into());
            tracing::info!(card = %card.id, lane = %card.lane, measured = true, n_considered = 1,
                verdict = "contract_cannot_satisfy", "a worker declared its contract unsatisfiable");
            Action::Rewrite(json!({
                "status": "needsyou",
                "ask_actor": crate::api::turn_end::owner_name(),
                "ask_type": "decision",
                "ask_question": format!("{} cannot satisfy its frozen contract as written ({reason}): change the contract, re-scope the card, or close it?", card.id),
                "ask_unblocks": "The owner's change to the contract or the card, after which the lane resumes or the card closes.",
                "desc_append": format!("\ncannot_satisfy (contract rule 2): {reason}"),
                // A standing approval cannot answer this ask: only the owner
                // changes a frozen contract. Without this a keyword match
                // (SA-9 on "check") refused GG-31's cannot_satisfy as "already
                // approved" and the card could not leave doing (gs12-gates,
                // 2026-10-07). Uses the gate's own logged decline.
                "standing_approval_decline": "contract cannot_satisfy: changing a frozen contract is the owner's decision, which no standing approval covers",
            }))
        }
        "doing" if card.status != "doing" => freeze_from(card, body, existing, defaults),
        "verified" if !owner && existing.is_some() => {
            tracing::info!(card = %card.id, lane = %card.lane, measured = true, n_considered = 1,
                verdict = "contract_verified_refused", "a worker tried to set verified on a contract card");
            Action::Respond(refuse(StatusCode::CONFLICT, "contract_verified_by_reviewer",
                format!("{} is verified by the harness reviewer, not by its lane (contract rule 3)", card.id),
                json!({"worker": "request done; after the server's check passes, a fresh reviewer runs and grants verified or sends findings back"})))
        }
        // Done is only reachable from doing, where the contract is checked.
        // gs12-mvs, GM-149, 2026-10-06: `board todo` then `board done` moved a
        // code card to done with no contract check at all.
        "done" if !owner && card.status != "doing" && card.status != "done" => {
            tracing::info!(card = %card.id, lane = %card.lane, from = %card.status, measured = true, n_considered = 1,
                verdict = "contract_done_from_non_doing_refused", "a worker requested done on a contract card outside doing");
            Action::Respond(refuse(StatusCode::CONFLICT, "contract_done_requires_doing",
                format!("{} is {}; on a contract lane done is requested from doing, where the frozen contract is verified", card.id, card.status),
                json!({"worker": "PATCH {\"status\":\"doing\",\"acceptance_criteria\":[...],\"verify_cmd\":\"...\"}, then request done with left_undone"})))
        }
        "done" if !owner && card.status == "doing" => match existing {
            Some(c) if c.state == "verifying" => Action::Respond(
                (StatusCode::ACCEPTED, Json(json!({"ok": true, "verification": "already_running", "card": card.id}))).into_response(),
            ),
            // Rule 8: the close names what it leaves undone; the route records it.
            Some(_) => match left_undone_items(body) {
                Ok(_) => Action::Pass, // the route starts the verification
                Err(why) => Action::Respond(left_undone_refusal(card, &why)),
            },
            None => Action::Respond(refuse(StatusCode::CONFLICT, "contract_missing",
                format!("{} has no frozen contract to verify against", card.id),
                json!({"worker": "PATCH {\"acceptance_criteria\":[...],\"verify_cmd\":\"...\"} on this card (they freeze), then request done again"}))),
        },
        _ => Action::Pass,
    }
}

/// Build and freeze a contract from the PATCH, the card and the lane's
/// defaults, or refuse saying which half is missing. Shared by entering doing
/// and by adding the contract to a card already in doing.
fn freeze_from(card: &Card, body: &Value, existing: Option<&Contract>, defaults: &Defaults) -> Action {
    let acceptance = nonempty(body.get("acceptance_criteria").map(|v| v.to_string()).as_deref().map(|s| s.trim_matches('"')))
        .or_else(|| nonempty(card.acceptance.as_deref()));
    let command = nonempty(body.get("verify_cmd").and_then(Value::as_str))
        .or_else(|| existing.map(|c| c.command.clone()))
        .or_else(|| nonempty(defaults.verify.as_deref()));
    let kind = nonempty(body.get("verify_kind").and_then(Value::as_str))
        .or_else(|| existing.map(|c| c.kind.clone()))
        .unwrap_or_else(|| "code".into());
    if !KINDS.contains(&kind.as_str()) {
        return Action::Respond(refuse(StatusCode::CONFLICT, "contract_unknown_kind",
            format!("verify_kind {kind:?} is not one of {KINDS:?}"), json!({"verify_kind": KINDS})));
    }
    let deploy_check = if kind == "deploy" {
        let d = nonempty(body.get("deploy_check").and_then(Value::as_str))
            .or_else(|| existing.and_then(|c| c.deploy_check.clone()))
            .or_else(|| nonempty(defaults.deploy_check.as_deref()));
        if d.is_none() || !defaults.deploy_probe {
            tracing::info!(card = %card.id, lane = %card.lane, missing_check = d.is_none(), missing_probe = !defaults.deploy_probe,
                measured = true, n_considered = 1, verdict = "contract_doing_refused", "a deploy card had no way to be checked in production");
            return Action::Respond(refuse(StatusCode::CONFLICT, "contract_deploy_unobservable",
                format!("{} is a deploy card, so it needs a post-deploy check and a lane that can read production's sha (contract rule 2b)", card.id),
                json!({
                    "deploy_check": format!("a command that exits 0 when the change works in production, in this PATCH or the lane's {DEFAULT_DEPLOY_CHECK} setting"),
                    DEPLOY_SHA_CMD: if defaults.deploy_probe { json!("set") } else { json!("unset: the lane's scope needs a command that prints production's deployed sha") },
                })));
        }
        d
    } else {
        None
    };
    match (acceptance, command) {
        (Some(a), Some(c)) => Action::PassThenFreeze(Contract {
            card: card.id.clone(),
            hash: hash_of(&a, &c, deploy_check.as_deref()),
            acceptance: a,
            command: c,
            state: "frozen".into(),
            sha: None,
            amended: false,
            kind,
            deploy_check,
        }),
        (a, c) => {
            tracing::info!(card = %card.id, lane = %card.lane, missing_acceptance = a.is_none(),
                missing_command = c.is_none(), measured = true, n_considered = 1,
                verdict = "contract_doing_refused", "a code card tried to enter doing without a contract");
            Action::Respond(refuse(StatusCode::CONFLICT, "contract_required",
                format!("{} needs acceptance criteria and a verify command before doing (contract rule 1)", card.id),
                json!({
                    "acceptance_criteria": "a list of testable statements, in this PATCH or already on the card",
                    "verify_cmd": format!("a command run from the repo root of the lane's committed HEAD, in this PATCH, or the lane's {DEFAULT_VERIFY} setting"),
                })))
        }
    }
}

/// A store write that returns a value (the store's own API returns only its
/// outcome).
pub(crate) async fn write_value<T, F>(state: &AppState, f: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
{
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let s2 = slot.clone();
    state
        .store
        .write_async(move |c| {
            let v = f(c)?;
            *s2.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await?;
    let v = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    v.ok_or_else(|| anyhow::anyhow!("write produced no value"))
}

/// Rule 8 (AH-384): the `left_undone` list a closing request must carry.
/// An empty list is an explicit "nothing"; a missing field is not an answer.
/// Each item names the work and either the card that now holds it or the
/// reason it is dismissed.
pub fn left_undone_items(body: &Value) -> Result<Vec<Value>, String> {
    let Some(v) = body.get("left_undone") else {
        return Err("the request has no left_undone field".into());
    };
    let Some(list) = v.as_array() else {
        return Err("left_undone must be a list".into());
    };
    for (i, it) in list.iter().enumerate() {
        let s = |k: &str| it.get(k).and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty());
        if s("item").is_none() {
            return Err(format!("left_undone[{i}] has no item"));
        }
        if s("card").is_some() == s("dismissed").is_some() {
            return Err(format!("left_undone[{i}] needs exactly one of card or dismissed"));
        }
    }
    Ok(list.clone())
}

fn left_undone_refusal(card: &Card, why: &str) -> Response {
    tracing::info!(card = %card.id, lane = %card.lane, why, measured = true, n_considered = 1,
        verdict = "contract_left_undone_refused", "a closing request did not say what it left undone");
    refuse(StatusCode::CONFLICT, "contract_left_undone_required",
        format!("closing {} must say what it leaves undone (contract rule 8): {why}", card.id),
        json!({
            "left_undone": [
                {"item": "what was not finished", "card": "<the card id that now holds it>"},
                {"item": "what was not finished", "dismissed": "<why it does not need doing>"},
            ],
            "nothing_left": "send \"left_undone\": [] when the card leaves nothing undone",
        }))
}

/// Record a validated `left_undone` list on the contract and the card. Every
/// named card must exist, so an item cannot point at work nobody filed.
pub async fn record_left_undone(state: &AppState, card: &str, items: Vec<Value>) -> Result<(), Response> {
    let (c, now) = (card.to_string(), crate::config::now_f64());
    let n = items.len();
    let r = write_value(state, move |conn| {
        for it in &items {
            if let Some(id) = it.get("card").and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()) {
                if crate::db::board_store::get_issue(conn, id)?.is_none() {
                    return Ok(Some(id.to_string()));
                }
            }
        }
        conn.execute("UPDATE card_contracts SET left_undone = ?2, at = ?3 WHERE card = ?1",
            rusqlite::params![c, Value::Array(items.clone()).to_string(), now])?;
        if let Some(mut row) = crate::db::board_store::get_issue(conn, &c)? {
            row.desc.push_str(&format!("\nLeft undone at close (contract rule 8): {}", left_undone_summary(&items)));
            crate::db::board_store::save_patched(conn, &mut row)?;
        }
        Ok(None)
    }).await;
    match r {
        Ok(None) => {
            tracing::info!(card, items = n, measured = true, n_considered = n, verdict = "contract_left_undone_recorded",
                "recorded what a closing card left undone");
            Ok(())
        }
        Ok(Some(missing)) => {
            tracing::info!(card, missing = %missing, measured = true, n_considered = n, verdict = "contract_left_undone_refused",
                "a left_undone item named a card that does not exist");
            Err(refuse(StatusCode::CONFLICT, "contract_left_undone_unknown_card",
                format!("left_undone names {missing}, which is not a card"),
                json!({"fix": "file the card first and name its id, or dismiss the item with a reason"})))
        }
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": e.to_string()}))).into_response()),
    }
}

fn left_undone_summary(items: &[Value]) -> String {
    if items.is_empty() {
        return "nothing".into();
    }
    items.iter().map(|it| {
        let s = |k: &str| it.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
        match it.get("card").and_then(Value::as_str) {
            Some(_) => format!("{} -> {}", s("item"), s("card")),
            None => format!("{} (dismissed: {})", s("item"), s("dismissed")),
        }
    }).collect::<Vec<_>>().join("; ")
}

/// Apply a one-time amendment of the frozen verify command.
pub async fn amend(state: &AppState, card: &str, actor: &str, command: String, reason: String) -> Response {
    let now = crate::config::now_f64();
    let (c, cmd, why, who) = (card.to_string(), command.clone(), reason.clone(), actor.to_string());
    let r = state.store.write_async(move |conn| {
        let Some(mut k) = load(conn, &c)? else {
            return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
        };
        let old = k.command.clone();
        k.hash = hash_of(&k.acceptance, &cmd, k.deploy_check.as_deref());
        k.command = cmd.clone();
        k.state = "frozen".into();
        save(conn, &k, now)?;
        conn.execute("UPDATE card_contracts SET amended = 1 WHERE card = ?1", [&c])?;
        if let Some(mut row) = crate::db::board_store::get_issue(conn, &c)? {
            row.desc.push_str(&format!("\nContract amended once by {who} (contract rule 1): verify command `{old}` -> `{cmd}`. Reason: {why}"));
            crate::db::board_store::save_patched(conn, &mut row)?;
        }
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await;
    let ok = matches!(r, Ok(ref o) if o.applied);
    tracing::info!(card, actor, ok, measured = true, n_considered = 1, verdict = "contract_amended",
        "a frozen verify command was amended once with a reason");
    let code = if ok { StatusCode::OK } else { StatusCode::CONFLICT };
    (code, Json(json!({"ok": ok, "card": card, "verify_cmd": command, "amended": ok}))).into_response()
}

/// Freeze a contract after a successful entry to doing.
/// Returns whether the contract was stored, so the PATCH can say so.
pub async fn freeze(state: &AppState, c: Contract) -> bool {
    let now = crate::config::now_f64();
    let c2 = c.clone();
    let r = state.store.write_async(move |conn| {
        save(conn, &c2, now)?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await;
    tracing::info!(card = %c.card, hash = %c.hash, ok = r.is_ok(), measured = true, n_considered = 1,
        verdict = "contract_frozen", "froze a card's acceptance criteria and verify command");
    r.is_ok()
}

async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = tokio::process::Command::new("git").arg("-C").arg(dir).args(args).output().await.map_err(|e| e.to_string())?;
    if !out.status.success() {
        // The exit status rides along: git's progress lines go to stderr too,
        // so "Preparing worktree" alone read as the whole failure (GO-79).
        return Err(format!("git exit {:?}: {}", out.status.code(), tail(String::from_utf8_lossy(&out.stderr).trim(), 600)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A clean detached checkout of `sha` at `tmp`, for a server check or a
/// review. 2026-10-06: a review killed by a server restart left its directory
/// behind unregistered, so every later `worktree remove` failed quietly and
/// `worktree add` refused the existing path, pass after pass (GO-79, 8 times
/// in an hour). The directory lives under our own ~/.amux/tmp/contract, so a
/// leftover is ours to clear. The repo's own hooks do not run here: they are
/// for lanes' checkouts, and Mixpeek's cost time on 39k files.
async fn fresh_checkout(tree: &Path, tmp: &Path, sha: &str) -> Result<(), String> {
    let t = tmp.to_string_lossy().into_owned();
    // A detached reviewer still running here, or a result nobody has read yet,
    // is not a leftover (rule 3 restart safety).
    if !matches!(review_job(tmp), ReviewJob::None) {
        return Err(format!("{t} holds a reviewer run that is still live or unread"));
    }
    let _ = git(tree, &["worktree", "remove", "--force", &t]).await;
    let _ = git(tree, &["worktree", "prune"]).await;
    if tmp.exists() && tmp.starts_with(crate::config::amux_home().join("tmp").join("contract")) {
        let _ = std::fs::remove_dir_all(tmp);
        tracing::info!(path = %t, measured = true, n_considered = 1, verdict = "contract_checkout_leftover_cleared",
            "cleared a checkout directory a killed check or review left behind");
    }
    git(tree, &["-c", "core.hooksPath=/dev/null", "worktree", "add", "--detach", &t, sha]).await
        .map(|_| ())
        .map_err(|e| format!("could not check out {sha}: {e}"))
}

fn tail(s: &str, n: usize) -> String {
    let t: Vec<char> = s.chars().collect();
    t[t.len().saturating_sub(n)..].iter().collect()
}

/// Start server verification for a `done` request. Returns 202 at once; the
/// transition happens when the check passes.
pub async fn start_verification(state: &AppState, card: &str, lane: &str) -> Response {
    let (card_s, now) = (card.to_string(), crate::config::now_f64());
    let claimed = state.store.write_async(move |conn| {
        let n = conn.execute("UPDATE card_contracts SET state = 'verifying', at = ?2 WHERE card = ?1 AND state <> 'verifying'",
            rusqlite::params![card_s, now])?;
        Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
    }).await;
    if !matches!(claimed, Ok(ref o) if o.applied) {
        return (StatusCode::ACCEPTED, Json(json!({"ok": true, "verification": "already_running", "card": card}))).into_response();
    }
    tracing::info!(card, lane, measured = true, n_considered = 1, verdict = "contract_verify_started",
        "server verification started for a done request");
    spawn_verification(state, card, lane);
    (StatusCode::ACCEPTED, Json(json!({
        "ok": true, "verification": "started", "card": card,
        "note": "The server runs the frozen verify command at your committed HEAD and grants done if it passes. A failure comes back to you with the output.",
    }))).into_response()
}

/// Interpreter directories for the verify run, ahead of the login PATH: the
/// lane tree's and its repo's venvs (a clean checkout has no venv of its own;
/// Mixpeek worktrees symlink server/.venv to the main checkout's), then the
/// lane's CC_VERIFY_PATH. GE2-8, 2026-10-05: bare `python` could not import
/// croniter in the clean checkout, so done could never be granted.
pub fn verify_path_prefix(tree: &Path, lane: &str) -> String {
    let home = crate::config::amux_home();
    let repo = PathBuf::from(crate::api::session_verbs::session_work_dir(lane));
    let mut dirs: Vec<String> = Vec::new();
    if let Some(extra) = lane_setting(&home, lane, "CC_VERIFY_PATH") {
        dirs.extend(extra.trim().trim_matches('"').split(':').filter(|d| !d.is_empty()).map(String::from));
    }
    for base in [tree, repo.as_path()] {
        for rel in ["server/.venv/bin", ".venv/bin", "venv/bin"] {
            let d = base.join(rel);
            if d.is_dir() {
                let s = d.to_string_lossy().into_owned();
                if !dirs.contains(&s) {
                    dirs.push(s);
                }
            }
        }
    }
    dirs.join(":")
}

/// Verifications running in THIS process. A row in `verifying` that is not
/// here lost its task: a deploy swap execs a new image and every spawned task
/// ends with the old one. 2026-10-06 12:57Z: GC-142, GC-44, GC-39, GC-58 and
/// GS-233 had their checks pass, then sat in `verifying` with nothing left to
/// write the result, and every done request answered already_running.
static LIVE_VERIFY: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

/// Load-adaptive caps for server checks and harness reviews (AH-397,
/// 2026-10-07). A fixed cap of 3 checks and 2 reviews ran on a host whose load
/// sat at 15 to 36 on a few cores; the right number depends on how busy the
/// machine already is. Per logical CPU, a 1-minute load at or above
/// LOAD_HIGH_PER_CPU allows one at a time, at or below LOAD_LOW_PER_CPU the
/// configured maximum, and the default in between. Maxima come from server.env
/// (AMUX_CONTRACT_VERIFY_CONCURRENCY, AMUX_CONTRACT_REVIEW_CONCURRENCY).
pub const LOAD_HIGH_PER_CPU: f64 = 1.5;
pub const LOAD_LOW_PER_CPU: f64 = 0.7;
const CHECKS_MAX_DEFAULT: usize = 4;
const CHECKS_MID: usize = 3;
/// Reviews are a model call and a checkout, not a build: they barely load the
/// host, so they are not held to the CPU bands. Ethan, 2026-10-07: raise review
/// concurrency to clear the 82-card review backlog (the bottleneck detector's
/// top constraint). Only an extreme load (REVIEW_HOLD_PER_CPU) halves them.
const REVIEWS_MAX_DEFAULT: usize = 5;
const REVIEW_HOLD_PER_CPU: f64 = 3.0;

/// The cap for one kind of work at a given load per CPU. Pure, for tests.
/// An unreadable load (None) gets the middle band, never the maximum.
pub fn adaptive_cap(max: usize, mid: usize, load_per_cpu: Option<f64>) -> usize {
    let max = max.max(1);
    match load_per_cpu {
        Some(l) if l >= LOAD_HIGH_PER_CPU => 1,
        Some(l) if l <= LOAD_LOW_PER_CPU => max,
        _ => mid.clamp(1, max),
    }
}

/// The review cap at a given load per CPU. Pure, for tests.
pub fn review_cap(max: usize, load_per_cpu: Option<f64>) -> usize {
    let max = max.max(1);
    match load_per_cpu {
        Some(l) if l >= REVIEW_HOLD_PER_CPU => max.div_ceil(2),
        _ => max,
    }
}

fn configured_max(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.trim().parse::<usize>().ok())
        .or_else(|| crate::config::parse_env_file(&crate::config::amux_home().join("server.env"))
            .get(key).and_then(|v| v.trim().trim_matches('"').parse::<usize>().ok()))
        .unwrap_or(default)
        .clamp(1, 32)
}

fn load_per_cpu() -> Option<f64> {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
    crate::api::request_log::host_load1().map(|l| l / cpus)
}

#[derive(Clone, Copy)]
enum Work {
    Check,
    Review,
}

/// The effective cap now, logging contract_capacity_adjusted when it moves.
fn current_cap(work: Work) -> usize {
    if cfg!(test) {
        // Tests run many cases in parallel in one process; the bands are
        // tested through adaptive_cap directly.
        return 32;
    }
    static LAST: [std::sync::atomic::AtomicUsize; 2] =
        [std::sync::atomic::AtomicUsize::new(0), std::sync::atomic::AtomicUsize::new(0)];
    let (max, mid, slot, name) = match work {
        Work::Check => (configured_max("AMUX_CONTRACT_VERIFY_CONCURRENCY", CHECKS_MAX_DEFAULT), CHECKS_MID, 0, "check"),
        Work::Review => (configured_max("AMUX_CONTRACT_REVIEW_CONCURRENCY", REVIEWS_MAX_DEFAULT), 0, 1, "review"),
    };
    let load = load_per_cpu();
    let cap = match work {
        Work::Check => adaptive_cap(max, mid, load),
        Work::Review => review_cap(max, load),
    };
    let old = LAST[slot].swap(cap, std::sync::atomic::Ordering::Relaxed);
    if old != cap {
        tracing::info!(work = name, old, new = cap, load_per_cpu = ?load.map(|l| (l * 100.0).round() / 100.0),
            measured = load.is_some(), n_considered = 1, verdict = "contract_capacity_adjusted",
            "the contract's concurrent {name} cap follows host load");
    }
    cap
}

/// Checks running now in this process; a check waits for a free place under
/// the current cap.
static CHECKS_RUNNING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

async fn take_check_place() {
    use std::sync::atomic::Ordering;
    loop {
        let cap = current_cap(Work::Check);
        let n = CHECKS_RUNNING.load(Ordering::Acquire);
        if n < cap && CHECKS_RUNNING.compare_exchange(n, n + 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Reviews running in this process, and the most that run at once.
static LIVE_REVIEW: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);
// Tests share the process-wide LIVE_REVIEW set across parallel cases.

fn spawn_verification(state: &AppState, card: &str, lane: &str) {
    if let Ok(mut live) = LIVE_VERIFY.lock() {
        live.insert(card.to_string());
    }
    let (st, c, l) = (state.clone(), card.to_string(), lane.to_string());
    tokio::spawn(async move {
        take_check_place().await;
        run_verification(&st, &c, &l).await;
        CHECKS_RUNNING.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        if let Ok(mut live) = LIVE_VERIFY.lock() {
            live.remove(&c);
        }
    });
}

/// Restart every verification whose task this process does not hold. Runs on
/// the contract clock, so a restart strands a verification for at most one tick.
pub async fn resume_orphaned_verifications(state: &AppState) -> usize {
    let rows: Vec<(String, String)> = state.store.read_async(|conn| {
        let mut st = conn.prepare(
            "SELECT c.card, COALESCE(i.session, '') FROM card_contracts c JOIN issues i ON i.id = c.card \
             WHERE c.state = 'verifying' AND i.status = 'doing'",
        )?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }).await.unwrap_or_default();
    // Only a card still in doing whose lane still runs contracts: a card that
    // closed another way, or a lane whose contract was rolled back, is never
    // re-judged (gs12-platform rollback, 2026-10-06 13:15Z: 37 done cards
    // still read `verifying`).
    let home = crate::config::amux_home();
    let orphans: Vec<(String, String)> = {
        let live = LIVE_VERIFY.lock().map(|l| l.clone()).unwrap_or_default();
        rows.iter().filter(|(c, lane)| !live.contains(c) && enabled_for(&home, lane)).cloned().collect()
    };
    for (card, lane) in &orphans {
        tracing::warn!(card = %card, lane = %lane, measured = true, n_considered = rows.len(),
            verdict = "contract_verify_resumed",
            "a verification lost its task (server restarted mid-run); running it again");
        spawn_verification(state, card, lane);
    }
    orphans.len()
}

async fn run_verification(state: &AppState, card: &str, lane: &str) {
    let home = crate::config::amux_home();
    let timeout = lane_setting(&home, lane, "AMUX_CONTRACT_VERIFY_TIMEOUT_S")
        .and_then(|v| v.trim().trim_matches('"').parse::<u64>().ok())
        .unwrap_or(DEFAULT_TIMEOUT_S)
        .clamp(1, amux_core::project::MAX_VERIFICATION_TIMEOUT_SECS);
    let result = verify(state, card, lane, Duration::from_secs(timeout)).await;
    finish(state, card, lane, result).await;
}

async fn verify(state: &AppState, card: &str, lane: &str, timeout: Duration) -> Result<(String, String, String), (Option<String>, String)> {
    let card_s = card.to_string();
    let contract = state.store.read_async(move |c| Ok(load(c, &card_s)?)).await.ok().flatten()
        .ok_or((None, "the card has no frozen contract".to_string()))?;
    let tree: PathBuf = crate::api::session_verbs::worker_worktree(lane)
        .or_else(|| Some(PathBuf::from(crate::api::session_verbs::session_work_dir(lane))).filter(|p| p.join(".git").exists()))
        .ok_or((None, format!("lane {lane} has no git checkout to measure")))?;
    let sha = git(&tree, &["rev-parse", "HEAD"]).await.map_err(|e| (None, format!("could not read HEAD: {e}")))?;
    let tmp = crate::config::amux_home().join("tmp").join("contract").join(format!("{card}-{}", &sha[..12.min(sha.len())]));
    let _ = std::fs::create_dir_all(tmp.parent().unwrap_or(&tmp));
    fresh_checkout(&tree, &tmp, &sha).await.map_err(|e| (Some(sha.clone()), e))?;
    let ws = crate::fanout_workspace::Workspace {
        repo: tree.to_string_lossy().into_owned(),
        path: tree.to_string_lossy().into_owned(),
        branch: lane.to_string(),
        base: String::new(),
    };
    let prefix = verify_path_prefix(&tree, lane);
    let r = crate::fanout_workspace::verify_commands_prefixed(&ws, &tmp.to_string_lossy(), &[contract.command.as_str()], timeout, &|| Ok(()), &prefix).await;
    let _ = git(&tree, &["worktree", "remove", "--force", &tmp.to_string_lossy()]).await;
    match r {
        Ok(()) => Ok((sha, contract.command, contract.acceptance)),
        Err(e) => Err((Some(sha), e)),
    }
}

async fn finish(state: &AppState, card: &str, lane: &str, result: Result<(String, String, String), (Option<String>, String)>) {
    let now = crate::config::now_f64();
    match result {
        Ok((sha, command, acceptance)) => {
            let evidence = format!(
                "Server-verified (contract rule 2) at {sha}: `{command}` exited 0 in a clean detached checkout of the lane's committed HEAD. Acceptance criteria (frozen): {acceptance}"
            );
            let (c, ev, sha2) = (card.to_string(), evidence.clone(), sha.clone());
            let r = state.store.write_async(move |conn| {
                set_state(conn, &c, "passed", Some(&sha2), "exit 0", now)?;
                let left: Option<String> = conn.query_row("SELECT left_undone FROM card_contracts WHERE card = ?1", [&c], |r| r.get(0)).optional()?.flatten();
                let ev = match left.and_then(|l| serde_json::from_str::<Vec<Value>>(&l).ok()) {
                    Some(items) => format!("{ev} Left undone (contract rule 8): {}", left_undone_summary(&items)),
                    None => ev,
                };
                // A deploy card is watched from here until production holds sha2.
                conn.execute("UPDATE card_contracts SET deploy_state = 'waiting', deploy_tries = 0, deploy_log = NULL, deploy_at = ?2
                              WHERE card = ?1 AND kind = 'deploy'", rusqlite::params![c, now])?;
                // Every other kind is ready for its reviewer now (rule 3).
                conn.execute("UPDATE card_contracts SET review_state = 'pending', review_at = ?2 WHERE card = ?1 AND kind <> 'deploy'",
                    rusqlite::params![c, now])?;
                let Some(mut row) = crate::db::board_store::get_issue(conn, &c)? else {
                    return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
                };
                row.evidence = Some(ev.clone());
                let from = row.status.clone();
                crate::db::board_store::save_patched(conn, &mut row)?;
                let opts = crate::db::advance::AdvanceOpts {
                    expected_from: Some(from),
                    gate_ack: true,
                    skip_continuation: true,
                    reason: Some(ev.clone()),
                    ..Default::default()
                };
                match crate::db::advance::advance(conn, &c, "done", ACTOR, &opts)? {
                    Ok(out) => Ok(crate::db::WriteOutcome { applied: true, events: out.events }),
                    Err(why) => Err(rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(format!("{why:?}"))))),
                }
            }).await;
            tracing::info!(card, lane, sha = %sha, ok = r.is_ok(), measured = true, n_considered = 1,
                verdict = "contract_verify_passed", "server verification passed; done granted");
            if r.is_ok() {
                crate::api::contract_fresh::mark_pending(lane, card); // rule 7
            }
        }
        Err((sha, why)) => {
            let short = tail(&why, 1500);
            let (c, s2, log) = (card.to_string(), sha.clone(), short.clone());
            // A1: a prod-change card's failed positive control is a hold for
            // the owner and a rollback for the lane (`applied` reports it).
            let held = state.store.write_async(move |conn| {
                set_state(conn, &c, "failed", s2.as_deref(), &log, now)?;
                let held = crate::api::prod_change::hold_on_failed_control(conn, &c, &log)?;
                Ok(crate::db::WriteOutcome { applied: held, events: vec![] })
            }).await.is_ok_and(|o| o.applied);
            if held {
                tracing::warn!(card, lane, sha = ?sha, measured = true, n_considered = 1, verdict = "a1_positive_control_failed",
                    reason = %tail(&why, 300), "a planned production change failed its positive control; held for the owner");
                let text = format!(
                    "[amux contract A1] {card}'s positive control failed after the production change. Roll the change back now, then record what you did on the card; the owner decides what happens next.\n\n{}",
                    tail(&why, 800)
                );
                let _ = crate::api::session_verbs::steer_enqueue(state, lane, &text, "contract-a1", ACTOR).await;
                return;
            }
            tracing::warn!(card, lane, sha = ?sha, measured = true, n_considered = 1,
                verdict = "contract_verify_failed", reason = %tail(&why, 300), "server verification failed; the card stays in doing");
            let text = format!(
                "[amux contract] {card} is not done: server verification failed at {}.\n\n{}\n\nFix and request done again, or PATCH {{\"status\":\"cannot_satisfy\",\"reason\":\"...\"}} if the frozen contract cannot be met as written.",
                sha.as_deref().unwrap_or("an unreadable HEAD"),
                tail(&why, 800)
            );
            let _ = crate::api::session_verbs::steer_enqueue(state, lane, &text, "contract-verify", ACTOR).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Rule 2b: the post-deploy check (AH-377)
// ---------------------------------------------------------------------------

/// The first full 40-hex sha in a probe's output, lowercased. A probe that
/// prints no full sha is UNMEASURED, never "not deployed yet".
pub fn parse_sha(out: &str) -> Option<String> {
    out.split(|c: char| !c.is_ascii_hexdigit())
        .find(|t| t.len() == 40)
        .map(str::to_ascii_lowercase)
}

/// The lane's checkout: its worktree, else its work dir if that is a repo.
pub(crate) fn lane_tree(lane: &str) -> Option<PathBuf> {
    crate::api::session_verbs::worker_worktree(lane)
        .or_else(|| Some(PathBuf::from(crate::api::session_verbs::session_work_dir(lane))).filter(|p| p.join(".git").exists()))
}

/// Run `cmd` with `sh -c` in `dir`, the lane's interpreters first on PATH.
/// Returns (exit 0, combined output tail).
pub(crate) async fn sh(dir: &Path, cmd: &str, prefix: &str, timeout: Duration) -> (bool, String) {
    let path = std::env::var("PATH").unwrap_or_default();
    let path = if prefix.is_empty() { path } else { format!("{prefix}:{path}") };
    let fut = tokio::process::Command::new("sh").arg("-c").arg(cmd).current_dir(dir).env("PATH", path)
        .kill_on_drop(true).output();
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(o)) => {
            let mut t = String::from_utf8_lossy(&o.stdout).into_owned();
            t.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), tail(t.trim(), 1500))
        }
        Ok(Err(e)) => (false, format!("could not run: {e}")),
        Err(_) => (false, format!("timed out after {}s", timeout.as_secs())),
    }
}

/// Some(true) when `deployed` contains `sha`, Some(false) when it does not,
/// None when git cannot tell (an object it does not have, even after a fetch).
async fn contains(tree: &Path, sha: &str, deployed: &str) -> Option<bool> {
    for attempt in 0..2 {
        let st = tokio::process::Command::new("git").arg("-C").arg(tree)
            .args(["merge-base", "--is-ancestor", sha, deployed]).output().await.ok()?;
        match st.status.code() {
            Some(0) => return Some(true),
            Some(1) => return Some(false),
            _ if attempt == 0 => {
                let _ = git(tree, &["fetch", "-q", "origin"]).await;
            }
            _ => {}
        }
    }
    None
}

async fn note_on_card(state: &AppState, card: &str, note: String, deploy_state: &str, tries: i64, log: &str, reopen: bool) -> bool {
    let (c, st, lg, now) = (card.to_string(), deploy_state.to_string(), log.to_string(), crate::config::now_f64());
    let r = state.store.write_async(move |conn| {
        conn.execute("UPDATE card_contracts SET deploy_state = ?2, deploy_tries = ?3, deploy_log = ?4 WHERE card = ?1",
            rusqlite::params![c, st, tries, lg])?;
        if st == "passed" {
            conn.execute("UPDATE card_contracts SET review_state = 'pending', review_at = ?2 WHERE card = ?1", rusqlite::params![c, now])?;
        }
        let Some(mut row) = crate::db::board_store::get_issue(conn, &c)? else {
            return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
        };
        row.desc.push_str(&note);
        let from = row.status.clone();
        crate::db::board_store::save_patched(conn, &mut row)?;
        if reopen && from == "done" {
            conn.execute("UPDATE card_contracts SET state = 'frozen', deploy_state = NULL, at = ?2 WHERE card = ?1", rusqlite::params![c, now])?;
            let opts = crate::db::advance::AdvanceOpts {
                expected_from: Some(from),
                gate_ack: true,
                skip_continuation: true,
                reason: Some(note.trim().to_string()),
                ..Default::default()
            };
            if let Ok(Ok(out)) = crate::db::advance::advance(conn, &c, "doing", ACTOR, &opts) {
                return Ok(crate::db::WriteOutcome { applied: true, events: out.events });
            }
        }
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await;
    r.is_ok()
}

/// One pass of the deploy watch. Returns (cards considered, lanes whose
/// production sha could not be read).
pub async fn watch_deploys(state: &AppState) -> (usize, usize) {
    type Row = (String, Option<String>, Option<String>, String, i64, f64);
    let rows: Vec<Row> = state.store.read_async(|c| {
        let mut st = c.prepare("SELECT card, sha, deploy_check, deploy_state, deploy_tries, COALESCE(deploy_at, at, frozen_at)
                                FROM card_contracts WHERE kind = 'deploy' AND deploy_state IN ('waiting', 'retry', 'stale')")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<rusqlite::Result<Vec<Row>>>()?;
        Ok(v)
    }).await.unwrap_or_default();
    let home = crate::config::amux_home();
    let now = crate::config::now_f64();
    let mut deployed: std::collections::HashMap<String, Option<String>> = Default::default();
    let mut unmeasured_lanes = 0usize;
    for (card, sha, check, dstate, tries, since) in &rows {
        let card_s = card.clone();
        let lane = state.store.read_async(move |c| Ok(crate::db::board_store::get_issue(c, &card_s)?)).await
            .ok().flatten().and_then(|r| r.session).unwrap_or_default();
        let (Some(sha), Some(check), Some(tree)) = (sha, check, lane_tree(&lane)) else {
            tracing::warn!(card, lane, measured = false, n_considered = 1, verdict = "contract_deploy_unmeasured",
                why_unmeasured = "no verified sha, post-deploy check or lane checkout", "a deploy card cannot be watched");
            continue;
        };
        if !deployed.contains_key(&lane) {
            let prod = match lane_setting(&home, &lane, DEPLOY_SHA_CMD).map(|v| v.trim().trim_matches('"').to_string()).filter(|v| !v.is_empty()) {
                Some(cmd) => {
                    let (ok, out) = sh(&tree, &cmd, "", Duration::from_secs(60)).await;
                    if ok { parse_sha(&out) } else { None }
                }
                None => None,
            };
            if prod.is_none() {
                unmeasured_lanes += 1;
                tracing::warn!(lane, measured = false, n_considered = 1, verdict = "contract_deploy_unmeasured",
                    why_unmeasured = "the lane's AMUX_DEPLOY_SHA_CMD printed no full sha", "could not read production's deployed sha");
            }
            deployed.insert(lane.clone(), prod);
        }
        let Some(prod) = deployed.get(&lane).cloned().flatten() else { continue };
        match contains(&tree, sha, &prod).await {
            Some(true) => {}
            Some(false) => {
                if dstate != "stale" && now - since > DEPLOY_STALE_S {
                    let note = format!("\nContract rule 2b: verified commit {sha} has not reached production (at {prod}) {:.0} h after done. The post-deploy check runs when it does.", (now - since) / 3600.0);
                    note_on_card(state, card, note, "stale", *tries, &format!("production at {prod}"), false).await;
                    tracing::warn!(card, lane, sha, prod, measured = true, n_considered = 1, verdict = "contract_deploy_stale",
                        "a deploy card's verified commit has waited over 24 h for production");
                }
                continue;
            }
            None => {
                tracing::warn!(card, lane, sha, prod, measured = false, n_considered = 1, verdict = "contract_deploy_unmeasured",
                    why_unmeasured = "git could not compare the verified and deployed shas", "deploy ancestry unknown");
                continue;
            }
        }
        let timeout = lane_setting(&home, &lane, "AMUX_CONTRACT_DEPLOY_TIMEOUT_S")
            .and_then(|v| v.trim().trim_matches('"').parse::<u64>().ok()).unwrap_or(DEPLOY_CHECK_TIMEOUT_S).clamp(1, 3600);
        let (ok, out) = sh(&tree, check, &verify_path_prefix(&tree, &lane), Duration::from_secs(timeout)).await;
        if ok {
            let note = format!("\nDeploy-verified (contract rule 2b): production at {prod} contains {sha}; post-deploy check `{check}` exited 0.");
            note_on_card(state, card, note, "passed", tries + 1, &tail(&out, 500), false).await;
            tracing::info!(card, lane, sha, prod, measured = true, n_considered = 1, verdict = "contract_deploy_passed",
                "the post-deploy check passed in production");
            continue;
        }
        let tries = tries + 1;
        if tries < DEPLOY_TRIES {
            note_on_card(state, card, String::new(), "retry", tries, &out, false).await;
            tracing::info!(card, lane, sha, prod, tries, measured = true, n_considered = 1, verdict = "contract_deploy_retry",
                reason = %tail(&out, 300), "the post-deploy check failed; retrying next pass");
            continue;
        }
        let note = format!("\nContract rule 2b: post-deploy check `{check}` failed {tries} times in production at {prod}; reopened to doing.");
        note_on_card(state, card, note, "failed", tries, &out, true).await;
        tracing::warn!(card, lane, sha, prod, tries, measured = true, n_considered = 1, verdict = "contract_deploy_failed",
            reason = %tail(&out, 300), "the post-deploy check failed in production; the card is back in doing");
        let text = format!(
            "[amux contract] {card} is back in doing: its post-deploy check failed {tries} times in production at {prod}.\n\n{}\n\nFix and request done again, or PATCH {{\"status\":\"cannot_satisfy\",\"reason\":\"...\"}}.",
            tail(&out, 800)
        );
        let _ = crate::api::session_verbs::steer_enqueue(state, &lane, &text, "contract-deploy", ACTOR).await;
    }
    tracing::info!(measured = true, n_considered = rows.len(), unmeasured_lanes, "contract deploy watch pass");
    (rows.len(), unmeasured_lanes)
}

// ---------------------------------------------------------------------------
// Rule 3: the harness-spawned reviewer (AH-378)
// ---------------------------------------------------------------------------

const REVIEW_ROUNDS: i64 = 3;
const REVIEW_TIMEOUT_S: u64 = 1200;
pub(crate) const REVIEWS_PER_PASS: usize = 5;
/// A review still `running` this long after it started died with the server.
const REVIEW_STALE_S: f64 = 2.0 * 3600.0;

/// The reviewer's model: the lane's `AMUX_CONTRACT_REVIEW_MODEL`, else a
/// different family from the lane's own model.
pub fn reviewer_model(home: &Path, lane: &str) -> String {
    if let Some(m) = lane_setting(home, lane, "AMUX_CONTRACT_REVIEW_MODEL").map(|v| v.trim().trim_matches('"').to_string()).filter(|v| !v.is_empty()) {
        return m;
    }
    let own = lane_setting(home, lane, "CC_MODEL").unwrap_or_default().to_ascii_lowercase();
    if own.contains("opus") { "claude-sonnet-5-5".into() } else { "claude-opus-5-5".into() }
}

/// The reviewer's verdict: the last line that parses as `{"verdict": ...}`.
/// Anything else is unmeasured, never a pass.
pub fn parse_review(out: &str) -> Option<(bool, Vec<String>)> {
    out.lines().rev().map(str::trim).filter(|l| l.starts_with('{')).find_map(|l| {
        let v: Value = serde_json::from_str(l).ok()?;
        let pass = match v.get("verdict")?.as_str()? {
            "pass" => true,
            "fail" => false,
            _ => return None,
        };
        let findings = v.get("findings").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|f| f.as_str().map(String::from)).collect()).unwrap_or_default();
        Some((pass, findings))
    })
}

/// Extra instructions for a completion-proof or requirement card (Ethan,
/// 2026-10-07: "make sure its all measured"). The reviewer otherwise sees only
/// the acceptance and evidence, not the card's description, so measurement
/// requirements written there (per surface, per extractor) went unread.
pub fn proof_rules(title: &str, desc: &str) -> String {
    let t = title.trim_start();
    if !(t.starts_with("GS12 proof") || t.starts_with("GS12 requirement")) {
        return String::new();
    }
    let tail: String = desc.chars().rev().take(4000).collect::<Vec<_>>().into_iter().rev().collect();
    format!(
        "\n\nTHIS IS A COMPLETION PROOF. Fail it unless every criterion, and every measurement the card's description \
         requires (per surface, per extractor, per template), has a recorded measured number from a run on origin/main: \
         the command, its output, and the value. A statement that something works, or a measurement of some surfaces \
         standing in for all of them, is a failure; name each missing measurement as a finding.\n\
         Card description (latest part, including any owner measurement requirements):\n{tail}"
    )
}

fn review_prompt(card: &str, title: &str, c: &Contract, evidence: &str, round: i64) -> String {
    if c.is_uncontracted() {
        return format!(
"You are the independent reviewer for board card {card} (round {round} of {REVIEW_ROUNDS}). You did not write this work. \
The working directory is a clean checkout of {sha}.

Card: {title}
Acceptance criteria: {acc}
There is no frozen verify command: this card reached done before the contract reached its lane, so no server check ran. \
Judge each acceptance criterion against the evidence the lane recorded and the commits themselves.
Evidence recorded on the card: {evidence}

Find the card's change: `git log --oneline --grep={card} -20`, then `git show` those commits, and read any test or run note the \
evidence names. A criterion is met only if the evidence or the code shows it, with a measured result where the criterion asks \
for one. Look for tests that were weakened, skipped or made to assert nothing, criteria satisfied in name only, and obvious \
regressions. Do not modify files.

Finish with exactly one line of JSON and nothing after it:
{{\"verdict\": \"pass\" or \"fail\", \"findings\": [\"one sentence per problem, with file:line\"]}}",
            sha = c.sha.as_deref().unwrap_or("HEAD"),
            acc = c.acceptance,
        );
    }
    format!(
"You are the independent reviewer for board card {card} (round {round} of {REVIEW_ROUNDS}). You did not write this work. \
The working directory is a clean checkout of the commit the server verified ({sha}).

Card: {title}
Frozen acceptance criteria: {acc}
Frozen verify command (it exited 0 at this commit): {cmd}
Server evidence: {evidence}

Find the card's change: `git log --oneline --grep={card} -20`, then `git show` those commits. Judge whether each acceptance \
criterion is actually met by the change, not only whether the command passed: look for tests that were weakened, skipped or \
made to assert nothing, criteria satisfied in name only, and obvious regressions in code the change touched. Do not modify files.

Finish with exactly one line of JSON and nothing after it:
{{\"verdict\": \"pass\" or \"fail\", \"findings\": [\"one sentence per problem, with file:line\"]}}",
        sha = c.sha.as_deref().unwrap_or("HEAD"),
        acc = c.acceptance,
        cmd = c.command,
    )
}

/// The reviewer runs DETACHED (its own process group, not killed with the
/// server) and writes into its own checkout directory, so a server restart
/// no longer throws a review away. 2026-10-06: the builder restarts the server
/// on many commits and contract_review_recovered fired 10 to 13 times an
/// hour, each one a review started over from scratch (up to its $2 budget).
const REVIEW_OUT: &str = ".amux-review.out";
const REVIEW_ERR: &str = ".amux-review.err";
const REVIEW_PID: &str = ".amux-review.pid";
const REVIEW_EXIT: &str = ".amux-review.exit";
const REVIEW_PROMPT: &str = ".amux-review.prompt";

#[derive(Debug, PartialEq)]
enum ReviewJob {
    /// No reviewer run in this directory.
    None,
    /// The reviewer's process is alive (pid, seconds since it started).
    Running(u32, f64),
    /// The reviewer exited; its output is waiting to be read.
    Finished,
}

fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill").args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .status().is_ok_and(|s| s.success())
}

fn review_job(dir: &Path) -> ReviewJob {
    if dir.join(REVIEW_EXIT).exists() {
        return ReviewJob::Finished;
    }
    let Some(pid) = std::fs::read_to_string(dir.join(REVIEW_PID)).ok().and_then(|s| s.trim().parse::<u32>().ok()) else {
        return ReviewJob::None;
    };
    if !pid_alive(pid) {
        return ReviewJob::None;
    }
    let age = std::fs::metadata(dir.join(REVIEW_PID)).and_then(|m| m.modified()).ok()
        .and_then(|t| t.elapsed().ok()).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    ReviewJob::Running(pid, age)
}

/// Start the reviewer detached: `sh` runs the CLI with the prompt on stdin
/// and writes stdout, stderr and finally the exit code into `dir`.
fn launch_review(dir: &Path, cli: &str, args: &[String], prompt: &str) -> Result<u32, String> {
    use std::os::unix::process::CommandExt;
    std::fs::write(dir.join(REVIEW_PROMPT), prompt).map_err(|e| e.to_string())?;
    let script = format!(
        "\"$0\" \"$@\" < {REVIEW_PROMPT} > {REVIEW_OUT} 2> {REVIEW_ERR}; echo $? > {REVIEW_EXIT}.tmp && mv {REVIEW_EXIT}.tmp {REVIEW_EXIT}"
    );
    let mut child = std::process::Command::new("sh")
        .arg("-c").arg(script).arg(cli).args(args)
        .current_dir(dir)
        .env_remove("CLAUDECODE").env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env_remove("AMUX_SESSION").env_remove("AMUX_WORKER")
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn().map_err(|e| format!("could not run {cli}: {e}"))?;
    let pid = child.id();
    std::fs::write(dir.join(REVIEW_PID), pid.to_string()).map_err(|e| e.to_string())?;
    // Reap it if this process is still here when it exits; after a restart
    // the reviewer is reparented and the next pass reads its files instead.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(pid)
}

/// Wait for a reviewer run in `dir` to finish, up to REVIEW_TIMEOUT_S from its
/// start, then read its verdict.
async fn collect_review(dir: &Path, model: &str) -> Result<(bool, Vec<String>, String), String> {
    loop {
        match review_job(dir) {
            ReviewJob::Finished => break,
            ReviewJob::Running(pid, age) if age >= REVIEW_TIMEOUT_S as f64 => {
                let _ = std::process::Command::new("kill").args(["-TERM", &format!("-{pid}")]).status();
                let _ = std::fs::write(dir.join(REVIEW_EXIT), "timeout");
                return Err(format!("reviewer timed out after {REVIEW_TIMEOUT_S}s"));
            }
            ReviewJob::Running(..) => tokio::time::sleep(Duration::from_millis(500)).await,
            ReviewJob::None => return Err("the reviewer exited without writing its result".into()),
        }
    }
    let text = std::fs::read_to_string(dir.join(REVIEW_OUT)).unwrap_or_default();
    let code = std::fs::read_to_string(dir.join(REVIEW_EXIT)).unwrap_or_default();
    match parse_review(&text) {
        Some((pass, findings)) => Ok((pass, findings, model.to_string())),
        None => Err(format!("reviewer ({model}, exit {}) gave no verdict line: {}", code.trim(),
            tail(&format!("{text}{}", std::fs::read_to_string(dir.join(REVIEW_ERR)).unwrap_or_default()), 400))),
    }
}

/// The directory a card's review runs in, for its verified sha.
fn review_dir(card: &str, sha: &str) -> PathBuf {
    crate::config::amux_home().join("tmp").join("contract").join(format!("{card}-review-{}", &sha[..12.min(sha.len())]))
}

/// Run one reviewer, or adopt the one a restarted server left behind.
/// Ok((pass, findings, model)) or Err(why unmeasured).
async fn review(card: &str, lane: &str, title: &str, c: &Contract, evidence: &str, round: i64) -> Result<(bool, Vec<String>, String), String> {
    let home = crate::config::amux_home();
    let tree = lane_tree(lane).ok_or_else(|| format!("lane {lane} has no checkout"))?;
    let sha = c.sha.clone().ok_or("the contract has no verified sha")?;
    let tmp = review_dir(card, &sha);
    let _ = std::fs::create_dir_all(tmp.parent().unwrap_or(&tmp));
    let model = reviewer_model(&home, lane);
    match review_job(&tmp) {
        ReviewJob::Finished => {
            tracing::info!(card, lane, measured = true, n_considered = 1, verdict = "contract_review_resumed_result",
                "a reviewer finished while the server was down; its result is read, not rerun");
        }
        ReviewJob::Running(pid, age) => {
            tracing::info!(card, lane, pid, age_s = age as i64, measured = true, n_considered = 1, verdict = "contract_review_still_running",
                "a reviewer outlived a server restart; waiting for it instead of starting another");
        }
        ReviewJob::None => {
            fresh_checkout(&tree, &tmp, &sha).await?;
            let cli = lane_setting(&home, lane, "AMUX_CONTRACT_REVIEW_CLI").map(|v| v.trim().trim_matches('"').to_string())
                .filter(|v| !v.is_empty()).unwrap_or_else(|| "claude".into());
            let budget = lane_setting(&home, lane, "AMUX_CONTRACT_REVIEW_BUDGET_USD").and_then(|v| v.trim().trim_matches('"').parse::<f64>().ok())
                .filter(|b| b.is_finite() && *b > 0.0).unwrap_or(2.0);
            let args: Vec<String> = ["--print", "--model", model.as_str(), "--no-session-persistence",
                "--allowedTools", "Read Grep Glob Bash(git log:*) Bash(git show:*) Bash(git diff:*)",
                "--disallowedTools", "Edit Write NotebookEdit",
                "--strict-mcp-config", "--mcp-config", "{\"mcpServers\":{}}",
                "--settings", "{\"disableAllHooks\":true}",
                "--max-budget-usd", &budget.to_string()].iter().map(|s| s.to_string()).collect();
            launch_review(&tmp, &cli, &args, &review_prompt(card, title, c, evidence, round))?;
        }
    }
    let result = collect_review(&tmp, &model).await;
    let _ = git(&tree, &["worktree", "remove", "--force", &tmp.to_string_lossy()]).await;
    if tmp.exists() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    result
}

async fn review_one(state: &AppState, card: String) {
    let c2 = card.clone();
    let facts = state.store.read_async(move |conn| {
        let k = load(conn, &c2)?;
        let row = crate::db::board_store::get_issue(conn, &c2)?;
        let rounds: i64 = conn.query_row("SELECT review_rounds FROM card_contracts WHERE card = ?1", [&c2], |r| r.get(0)).unwrap_or(0);
        Ok((k, row, rounds))
    }).await.ok();
    let Some((Some(k), Some(row), rounds)) = facts else { return };
    let lane = row.session.clone().unwrap_or_default();
    // A card that left done while its review waited (verified another way,
    // reopened, discarded) has nothing to review. 2026-10-06: GS-230 was
    // already verified, its review's move to verified was refused, the
    // refusal rolled back the review's own state, and it was reviewed again
    // every pass: 12 paid reviews of one card in an hour.
    if row.status != "done" {
        let (c, st) = (card.clone(), row.status.clone());
        let _ = state.store.write_async(move |conn| {
            conn.execute("UPDATE card_contracts SET review_state = 'superseded', review_log = ?2 WHERE card = ?1",
                rusqlite::params![c, format!("card left done (now {st}) before its review ran")])?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).await;
        tracing::info!(card, lane, status = %row.status, measured = true, n_considered = 1, verdict = "contract_review_superseded",
            "the card left done before its review ran; no review is spent on it");
        return;
    }
    let round = rounds + 1;
    tracing::info!(card, lane, round, measured = true, n_considered = 1, verdict = "contract_review_started",
        "a fresh reviewer started for a verified-eligible card");
    let evidence = format!("{}{}", row.evidence.as_deref().unwrap_or(""), proof_rules(&row.title, &row.desc));
    let result = review(&card, &lane, &row.title, &k, &evidence, round).await;
    let now = crate::config::now_f64();
    let (pass, findings, model) = match result {
        Ok(r) => r,
        Err(why) => {
            // Unmeasured: back to pending for the next pass, no round spent.
            let (c, w) = (card.clone(), why.clone());
            let _ = state.store.write_async(move |conn| {
                conn.execute("UPDATE card_contracts SET review_state = 'pending', review_log = ?2, review_at = ?3 WHERE card = ?1",
                    rusqlite::params![c, w, now])?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            }).await;
            tracing::warn!(card, lane, measured = false, n_considered = 1, verdict = "contract_review_unmeasured",
                why_unmeasured = %tail(&why, 300), "the reviewer produced no verdict; it reruns next pass");
            return;
        }
    };
    let reviewer = format!("harness:reviewer:{model}");
    let list = if findings.is_empty() { "none".to_string() } else { findings.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n") };
    let (target, rstate, note) = if pass {
        ("verified", "passed", format!("\nReviewed (contract rule 3, round {round}) by a fresh read-only {model} reviewer at {}: pass. Findings: {list}", k.sha.as_deref().unwrap_or("?")))
    } else if round >= REVIEW_ROUNDS {
        ("needsyou", "escalated", format!("\nReview round {round} of {REVIEW_ROUNDS} failed (contract rule 3, {model}); the owner decides. Findings:\n{list}"))
    } else {
        ("doing", "failed", format!("\nReview round {round} of {REVIEW_ROUNDS} failed (contract rule 3, {model}); reopened. Findings:\n{list}"))
    };
    let (c, n, rv, ts) = (card.clone(), note.clone(), reviewer.clone(), target.to_string());
    let owner = crate::api::turn_end::owner_name();
    let refused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let refused2 = refused.clone();
    let r = state.store.write_async(move |conn| {
        conn.execute("UPDATE card_contracts SET review_state = ?2, review_rounds = ?3, review_log = ?4, review_at = ?5 WHERE card = ?1",
            rusqlite::params![c, rstate, round, n, now])?;
        if ts == "doing" {
            conn.execute("UPDATE card_contracts SET state = 'frozen', deploy_state = NULL WHERE card = ?1", [&c])?;
        }
        let Some(mut row) = crate::db::board_store::get_issue(conn, &c)? else {
            return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
        };
        row.desc.push_str(&n);
        if ts == "verified" {
            row.reviewer = Some(rv.clone());
        }
        if ts == "needsyou" {
            row.ask_actor = Some(owner.clone());
            row.ask_type = Some("decision".into());
            row.ask_question = Some(format!("{c} failed its independent review {REVIEW_ROUNDS} times (contract rule 3): accept it as is, change its contract, or reopen it with direction?"));
            row.ask_unblocks = Some("Your decision on the card; the findings are in its description.".into());
        }
        let from = row.status.clone();
        crate::db::board_store::save_patched(conn, &mut row)?;
        let opts = crate::db::advance::AdvanceOpts {
            expected_from: Some(from),
            gate_ack: true,
            skip_continuation: true,
            reason: Some(n.trim().to_string()),
            ..Default::default()
        };
        match crate::db::advance::advance(conn, &c, &ts, ACTOR, &opts)? {
            Ok(out) => Ok(crate::db::WriteOutcome { applied: true, events: out.events }),
            // Keep the review's record even when the card's move is refused;
            // rolling it back left the review "running" and it ran again.
            Err(why) => {
                tracing::warn!(card = %c, to = %ts, reason = ?why, measured = true, n_considered = 1,
                    verdict = "contract_review_transition_refused", "the review is recorded but the card's move was refused");
                refused2.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            }
        }
    }).await;
    let ok = r.is_ok() && !refused.load(std::sync::atomic::Ordering::Relaxed);
    match target {
        "verified" => tracing::info!(card, lane, round, model, ok, measured = true, n_considered = 1, verdict = "contract_review_passed",
            "the harness reviewer passed the card; verified granted"),
        "needsyou" => tracing::warn!(card, lane, round, model, ok, measured = true, n_considered = 1, verdict = "contract_review_escalated",
            "three review rounds failed; the owner decides"),
        _ => tracing::info!(card, lane, round, model, ok, findings = findings.len(), measured = true, n_considered = 1,
            verdict = "contract_review_failed", "the harness reviewer failed the card; reopened with findings"),
    }
    if target == "doing" {
        let text = format!("[amux contract] {card} is back in doing: the independent reviewer failed round {round} of {REVIEW_ROUNDS}.\n\n{list}\n\nAddress the findings and request done again, or PATCH {{\"status\":\"cannot_satisfy\",\"reason\":\"...\"}}.");
        let _ = crate::api::session_verbs::steer_enqueue(state, &lane, &text, "contract-review", ACTOR).await;
    }
}

/// Rule 3 for cards that reached done with no contract (they predate the
/// contract on their lane), on lanes whose scoped AMUX_REVIEW_UNCONTRACTED is
/// on. 2026-10-07: 12 GS-12 proof cards sat at done for 4+ hours with no
/// reviewer, because the harness reviewer only knew contract cards. A review
/// record is created (UNCONTRACTED_CMD, never run) and the existing reviewer,
/// cap and restart safety take it from there. A card whose uncontracted review
/// failed comes back when it is done again, with its rounds kept. Proof and
/// requirement cards count whatever their type: 10 of the 12 waiting GS-12
/// proof cards were typed `ops`, and a code-only filter skipped every one.
pub async fn enqueue_uncontracted(state: &AppState) -> usize {
    type Cand = (String, String, String, Option<String>, String, Option<String>);
    let cands: Vec<Cand> = state.store.read_async(|conn| {
        let mut st = conn.prepare(
            "SELECT i.id, COALESCE(i.session, ''), i.title, i.acceptance_criteria, substr(COALESCE(i.desc, ''), 1, 400), i.evidence \
             FROM issues i LEFT JOIN card_contracts c ON c.card = i.id \
             WHERE i.status = 'done' AND COALESCE(i.archived, 0) = 0 AND COALESCE(i.deleted, 0) = 0 \
             AND (i.type = 'code' OR i.title LIKE 'GS12 proof%' OR i.title LIKE 'GS12 requirement%') \
             AND (c.card IS NULL OR (c.command = ?1 AND c.review_state = 'failed'))")?;
        let v = st.query_map([UNCONTRACTED_CMD], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<rusqlite::Result<Vec<Cand>>>()?;
        Ok(v)
    }).await.unwrap_or_default();
    let home = crate::config::amux_home();
    let mut opted: std::collections::HashMap<String, bool> = Default::default();
    let mut n = 0usize;
    for (card, lane, title, acc, desc, evidence) in cands {
        let on = *opted.entry(lane.clone()).or_insert_with(|| {
            !lane.is_empty() && lane_setting(&home, &lane, "AMUX_REVIEW_UNCONTRACTED").is_some_and(|v| truthy(&v))
        });
        if !on {
            continue;
        }
        let Some(tree) = lane_tree(&lane) else { continue };
        // The commit to review: a sha the evidence names, if this checkout
        // has it; else the lane's view of origin/main.
        let mut sha = None;
        if let Some(s) = evidence.as_deref().and_then(parse_sha) {
            if git(&tree, &["cat-file", "-e", &format!("{s}^{{commit}}")]).await.is_ok() {
                sha = Some(s);
            }
        }
        if sha.is_none() {
            sha = git(&tree, &["rev-parse", "origin/main"]).await.ok().filter(|s| !s.is_empty());
        }
        let Some(sha) = sha else { continue };
        let acceptance = nonempty(acc.as_deref())
            .unwrap_or_else(|| format!("{title}. {}", desc.trim()));
        let now = crate::config::now_f64();
        let (c, a, sh) = (card.clone(), acceptance, sha.clone());
        let r = state.store.write_async(move |conn| {
            let k = conn.execute(
                "INSERT INTO card_contracts (card, acceptance, command, hash, frozen_at, state, sha, kind, review_state, review_at) \
                 VALUES (?1, ?2, ?3, '', ?4, 'passed', ?5, 'code', 'pending', ?4) \
                 ON CONFLICT(card) DO UPDATE SET review_state = 'pending', review_at = ?4, sha = ?5 \
                 WHERE card_contracts.command = ?3 AND card_contracts.review_state = 'failed'",
                rusqlite::params![c, a, UNCONTRACTED_CMD, now, sh])?;
            Ok(crate::db::WriteOutcome { applied: k > 0, events: vec![] })
        }).await;
        if matches!(r, Ok(ref o) if o.applied) {
            n += 1;
            tracing::info!(card = %card, lane = %lane, sha = %sha, measured = true, n_considered = 1,
                verdict = "contract_review_uncontracted_enqueued",
                "a done card with no contract was queued for the harness reviewer");
        }
    }
    n
}

/// One pass: claim up to REVIEWS_PER_PASS pending reviews and run them in the
/// background. Returns (pending before the pass, claimed).
pub async fn run_reviews(state: &AppState) -> (usize, usize) {
    let now = crate::config::now_f64();
    let live: Vec<String> = LIVE_REVIEW.lock().map(|l| l.iter().cloned().collect()).unwrap_or_default();
    let _ = state.store.write_async(move |conn| {
        let mut n = conn.execute("UPDATE card_contracts SET review_state = 'pending' WHERE review_state = 'running' AND review_at < ?1",
            [now - REVIEW_STALE_S])?;
        // A review whose task a restart ended goes back to pending now, not
        // after REVIEW_STALE_S.
        let running: Vec<String> = conn.prepare("SELECT card FROM card_contracts WHERE review_state = 'running'")?
            .query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        for c in running.iter().filter(|c| !live.contains(*c)) {
            n += conn.execute("UPDATE card_contracts SET review_state = 'pending' WHERE card = ?1 AND review_state = 'running'", [c])?;
            tracing::warn!(card = %c, measured = true, n_considered = 1, verdict = "contract_review_recovered",
                "a review lost its task (server restarted mid-run); it goes back to pending");
        }
        Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
    }).await;
    enqueue_uncontracted(state).await;
    // Proof cards first: they decide a project's finish (GS-12, 2026-10-07).
    let pending: Vec<String> = state.store.read_async(|conn| {
        let mut st = conn.prepare(
            "SELECT c.card FROM card_contracts c LEFT JOIN issues i ON i.id = c.card WHERE c.review_state = 'pending' \
             ORDER BY CASE WHEN i.title LIKE 'GS12 proof%' OR i.title LIKE 'GS12 requirement%' THEN 0 ELSE 1 END, c.review_at")?;
        let v = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(v)
    }).await.unwrap_or_default();
    let mut claimed = 0;
    let free = current_cap(Work::Review).saturating_sub(LIVE_REVIEW.lock().map(|l| l.len()).unwrap_or(0)).min(REVIEWS_PER_PASS);
    let n_pending = pending.len();
    for card in pending {
        if claimed >= free {
            break;
        }
        let c = card.clone();
        let won = state.store.write_async(move |conn| {
            let n = conn.execute("UPDATE card_contracts SET review_state = 'running', review_at = ?2 WHERE card = ?1 AND review_state = 'pending'",
                rusqlite::params![c, now])?;
            Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
        }).await;
        if matches!(won, Ok(ref o) if o.applied) {
            claimed += 1;
            let st = state.clone();
            if let Ok(mut live) = LIVE_REVIEW.lock() {
                live.insert(card.clone());
            }
            tokio::spawn(async move {
                review_one(&st, card.clone()).await;
                if let Ok(mut live) = LIVE_REVIEW.lock() {
                    live.remove(&card);
                }
            });
        }
    }
    tracing::info!(measured = true, n_considered = n_pending, claimed, "contract review pass");
    (n_pending, claimed)
}

// ---------------------------------------------------------------------------
// Rule 14: per-rule counters (AH-379)
// ---------------------------------------------------------------------------

/// Each live rule and the verdicts it already logs. A rule is counted by the
/// lines it writes, so a counter cannot claim activity the rule did not log.
pub const RULE_VERDICTS: &[(&str, &[&str])] = &[
    ("1", &["contract_doing_refused", "contract_frozen", "contract_frozen_edit_refused", "contract_amended"]),
    ("2", &["contract_advance_refused", "contract_verify_started", "contract_verify_passed", "contract_verify_failed", "contract_force_refused", "contract_cannot_satisfy"]),
    ("2b", &["contract_deploy_passed", "contract_deploy_retry", "contract_deploy_failed", "contract_deploy_stale", "contract_deploy_unmeasured"]),
    ("3", &["contract_review_uncontracted_enqueued", "contract_review_superseded", "contract_review_transition_refused", "contract_review_resumed_result", "contract_review_still_running", "contract_review_recovered", "contract_review_started", "contract_review_passed", "contract_review_failed", "contract_review_escalated", "contract_review_unmeasured", "contract_verified_refused"]),
    ("6", &["contract_budget_exhausted"]),
    ("7", &["contract_fresh_session", "contract_fresh_skipped"]),
    ("9", &["rule9_peer_approval_refused", "rule9_peer_delegation_refused"]),
    ("A5", &["contract_dispatch_held", "a2_pool_held"]),
    ("A2", &["a2_pool_assigned", "a2_pool_empty", "a2_pool_skipped_parked"]),
    ("8", &["contract_left_undone_recorded", "contract_left_undone_refused"]),
    ("A3", &["done_line_frozen", "done_line_revised", "done_line_change_refused", "done_line_revision_refused"]),
    ("4", &["worktree_launch_isolated", "worktree_launch_refused", "shared_guard_skipped_isolated"]),
    ("A1", &["a1_notice_raised", "a1_doing_refused_notice", "a1_held_by_owner", "a1_positive_control_failed"]),
    ("5", &["land_queued", "land_merged", "land_refused", "land_batch_bisected", "worker_push_refused", "land_lock_acquired", "land_lock_waited", "land_lock_timeout_requeued"]),
    ("10", &["worker_token_minted", "worker_identity_refused", "owner_by_absence_refused", "credentials_in_worker_env", "bypass_without_sandbox"]),
    ("11", &["needs_input_auto_approved", "needs_input_auto_skipped_category", "needs_input_auto_refused", "needs_input_auto_sent_back"]),
    ("13", &["memory_recomposed_at_boot", "memory_over_budget", "memory_within_budget", "memory_pointers_archived", "rules_delivered", "rules_not_delivered"]),
];

/// Count each rule's verdicts in `log` text with a timestamp at or after
/// `since` (epoch seconds). Pure, for tests; returns (counts, lines scanned).
pub fn count_verdicts(log: &str, since: f64) -> (std::collections::BTreeMap<String, u64>, u64) {
    let mut counts = std::collections::BTreeMap::new();
    let mut scanned = 0u64;
    for line in log.lines() {
        let Some(ts) = line.split_whitespace().next().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) else {
            continue;
        };
        if (ts.timestamp() as f64) < since {
            continue;
        }
        scanned += 1;
        let Some(i) = line.find("verdict=") else { continue };
        let v = line[i + 8..].trim_start_matches('"');
        let v: String = v.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if RULE_VERDICTS.iter().any(|(_, vs)| vs.contains(&v.as_str())) {
            *counts.entry(v).or_insert(0) += 1;
        }
    }
    (counts, scanned)
}

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/api/contract/counters", axum::routing::get(counters_route))
        .merge(crate::api::done_line::routes())
}

async fn counters_route(axum::extract::State(state): axum::extract::State<AppState>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>) -> Response {
    let since_h = q.get("since_h").and_then(|v| v.parse::<f64>().ok()).filter(|v| *v > 0.0 && *v <= 168.0).unwrap_or(24.0);
    let since = crate::config::now_f64() - since_h * 3600.0;
    let path = crate::config::amux_home().join("logs").join("server-rs.log");
    // The tail only: the log rotates and can be large; 256 MB covers days.
    let text = match std::fs::File::open(&path) {
        Ok(mut f) => {
            use std::io::{Read, Seek, SeekFrom};
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            let _ = f.seek(SeekFrom::Start(len.saturating_sub(256 << 20)));
            let mut buf = Vec::new();
            let _ = f.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        }
        Err(e) => {
            return Json(json!({"measured": false, "n_considered": 0, "why_unmeasured": format!("cannot read {}: {e}", path.display())})).into_response();
        }
    };
    let (counts, scanned) = count_verdicts(&text, since);
    let rules: Vec<Value> = RULE_VERDICTS
        .iter()
        .map(|(rule, vs)| {
            let per: serde_json::Map<String, Value> = vs.iter().map(|v| (v.to_string(), json!(counts.get(*v).copied().unwrap_or(0)))).collect();
            json!({"rule": rule, "total": per.values().filter_map(Value::as_u64).sum::<u64>(), "verdicts": per})
        })
        .collect();
    let home = crate::config::amux_home();
    let lanes: Vec<String> = std::fs::read_dir(home.join("sessions")).map(|d| {
        d.flatten().filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".env")).map(String::from)).collect()
    }).unwrap_or_default();
    // A2: each hub's pool (cards tagged `pool`) and how many are ready now.
    let pool = match state.store.read_async(|c| Ok(super::runner::pool_census(c)?)).await {
        Ok(rows) => json!({"measured": true, "n_considered": rows.len(),
            "hubs": rows.into_iter().map(|(hub, size, ready)| json!({"hub": hub, "pool": size, "ready": ready})).collect::<Vec<_>>()}),
        Err(e) => json!({"measured": false, "n_considered": 0, "why_unmeasured": e.to_string()}),
    };
    Json(json!({"since_h": since_h, "measured": true, "n_considered": scanned, "rules": rules,
        "rule10": super::worker_identity::report(&home, &lanes),
        "a2_pool": pool,
        "contract": "docs/orchestration-contract.md (rule 14)"})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(status: &str, ty: &str, acceptance: Option<&str>) -> Card {
        Card { id: "T-1".into(), lane: "lane".into(), status: status.into(), item_type: ty.into(), acceptance: acceptance.map(String::from) }
    }
    fn frozen() -> Contract {
        Contract { card: "T-1".into(), acceptance: "it works".into(), command: "make test".into(), hash: "h".into(), state: "frozen".into(), sha: None, amended: false, kind: "code".into(), deploy_check: None }
    }
    fn dflt(verify: Option<&str>) -> Defaults {
        Defaults { verify: verify.map(String::from), ..Default::default() }
    }
    fn code(a: &Action) -> String {
        match a {
            Action::Respond(r) => r.status().as_u16().to_string(),
            Action::Pass => "pass".into(),
            Action::Rewrite(_) => "rewrite".into(),
            Action::PassThenFreeze(_) => "freeze".into(),
            Action::Amend(..) => "amend".into(),
        }
    }

    #[test]
    fn a_code_card_needs_acceptance_and_a_command_to_enter_doing() {
        let b = json!({"status": "doing"});
        assert_eq!(code(&decide(&card("todo", "code", None), &b, false, None, &dflt(Some("make test")))), "409", "no acceptance");
        assert_eq!(code(&decide(&card("todo", "code", Some("it works")), &b, false, None, &dflt(None))), "409", "no command");
        assert_eq!(code(&decide(&card("todo", "code", Some("[]")), &b, false, None, &dflt(Some("x")))), "409", "an empty list is not acceptance");
        match decide(&card("todo", "code", Some("it works")), &b, false, None, &dflt(Some("make test"))) {
            Action::PassThenFreeze(c) => assert_eq!((c.command.as_str(), c.acceptance.as_str()), ("make test", "it works")),
            _ => panic!("a complete contract freezes"),
        }
        let inline = json!({"status": "doing", "acceptance_criteria": ["it works"], "verify_cmd": "cargo test"});
        assert_eq!(code(&decide(&card("todo", "code", None), &inline, false, None, &dflt(None))), "freeze", "fields in the PATCH count");
        assert_eq!(code(&decide(&card("todo", "chore", None), &b, false, None, &dflt(None))), "pass", "only code cards");
    }

    #[test]
    fn a_doing_card_without_a_contract_freezes_one_from_a_patch() {
        // GS-215: in doing before contracts; verify_cmd was dropped on PATCH.
        let add = json!({"acceptance_criteria": ["the reaper runs"], "verify_cmd": "kubectl get cronjob x"});
        match decide(&card("doing", "code", None), &add, false, None, &dflt(None)) {
            Action::PassThenFreeze(c) => assert_eq!(c.command, "kubectl get cronjob x"),
            _ => panic!("a doing card with no contract freezes the one in the PATCH"),
        }
        let only_cmd = json!({"verify_cmd": "true"});
        assert_eq!(code(&decide(&card("doing", "code", None), &only_cmd, false, None, &dflt(None))), "409",
            "no acceptance anywhere: refused, saying what is missing");
        assert_eq!(code(&decide(&card("doing", "code", Some("it works")), &only_cmd, false, None, &dflt(None))), "freeze",
            "acceptance already on the card counts");
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &only_cmd, false, Some(&frozen()), &dflt(None))), "409",
            "a frozen contract is still the owner's to edit");
    }

    #[test]
    fn done_from_outside_doing_is_refused_for_a_worker() {
        let done = json!({"status": "done", "evidence": "x", "left_undone": []});
        for from in ["todo", "backlog", "review", "blocked"] {
            assert_eq!(code(&decide(&card(from, "code", Some("a")), &done, false, None, &dflt(None))), "409", "{from} -> done");
            assert_eq!(code(&decide(&card(from, "code", Some("a")), &done, true, None, &dflt(None))), "pass", "the owner may, from {from}");
        }
        assert_eq!(code(&decide(&card("todo", "chore", None), &done, false, None, &dflt(None))), "pass", "only code cards");
    }

    #[test]
    fn a_frozen_contract_and_force_belong_to_the_owner() {
        let edit = json!({"verify_cmd": "true"});
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &edit, false, Some(&frozen()), &dflt(None))), "409");
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &edit, true, Some(&frozen()), &dflt(None))), "pass", "the owner may edit");
        let force = json!({"status": "done", "force": true, "reason": "x"});
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &force, false, Some(&frozen()), &dflt(None))), "403");
    }

    #[test]
    fn done_is_verified_by_the_server_and_cannot_satisfy_goes_to_the_owner() {
        let done = json!({"status": "done", "evidence": "trust me", "gate_checked": ["x"], "left_undone": []});
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &done, false, Some(&frozen()), &dflt(None))), "pass", "the route starts verification");
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &done, false, None, &dflt(None))), "409", "nothing frozen to verify");
        let running = Contract { state: "verifying".into(), ..frozen() };
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &done, false, Some(&running), &dflt(None))), "202");
        match decide(&card("doing", "code", Some("a")), &json!({"status": "cannot_satisfy", "reason": "the fixture is gone", "left_undone": []}), false, Some(&frozen()), &dflt(None)) {
            Action::Rewrite(v) => {
                assert_eq!(v["status"], "needsyou");
                assert!(v["standing_approval_decline"].as_str().is_some_and(|s| !s.is_empty()),
                    "a cannot_satisfy is never answered by a standing approval");
                assert!(v["ask_question"].as_str().unwrap().contains("the fixture is gone"));
            }
            _ => panic!("cannot_satisfy becomes an owner ask"),
        }
    }

    #[test]
    fn a_close_must_say_what_it_leaves_undone() {
        let c = card("doing", "code", Some("a"));
        for (body, ok, why) in [
            (json!({"status": "done"}), false, "missing"),
            (json!({"status": "done", "left_undone": "nothing"}), false, "not a list"),
            (json!({"status": "done", "left_undone": [{"item": "x"}]}), false, "no card or dismissal"),
            (json!({"status": "done", "left_undone": [{"item": "x", "card": "A-1", "dismissed": "y"}]}), false, "both"),
            (json!({"status": "done", "left_undone": [{"card": "A-1"}]}), false, "no item"),
            (json!({"status": "done", "left_undone": []}), true, "an explicit nothing"),
            (json!({"status": "done", "left_undone": [{"item": "x", "card": "A-1"}, {"item": "y", "dismissed": "out of scope"}]}), true, "both shapes"),
        ] {
            assert_eq!(code(&decide(&c, &body, false, Some(&frozen()), &dflt(None))) == "pass", ok, "{why}");
        }
        assert_eq!(code(&decide(&c, &json!({"status": "cannot_satisfy", "reason": "r"}), false, Some(&frozen()), &dflt(None))), "409",
            "cannot_satisfy is a close too");
    }

    #[test]
    fn a_frozen_verify_command_can_be_amended_once_with_a_reason() {
        let amend = json!({"verify_cmd": "cd server && .venv/bin/python -m pytest -q", "reason": "bare python lacks deps"});
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &amend, false, Some(&frozen()), &dflt(None))), "amend");
        let no_reason = json!({"verify_cmd": "x"});
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &no_reason, false, Some(&frozen()), &dflt(None))), "409");
        let twice = Contract { amended: true, state: "failed".into(), ..frozen() };
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &amend, false, Some(&twice), &dflt(None))), "409", "only once");
        let acceptance = json!({"acceptance_criteria": ["weaker"], "verify_cmd": "x", "reason": "r"});
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &acceptance, false, Some(&frozen()), &dflt(None))), "409", "acceptance stays owner-only");
    }

    #[test]
    fn a_deploy_card_freezes_a_post_deploy_check_only_when_production_is_observable() {
        let b = json!({"status": "doing", "verify_kind": "deploy", "deploy_check": "curl -fsS https://x/health"});
        let probe = Defaults { verify: Some("make test".into()), deploy_probe: true, ..Default::default() };
        match decide(&card("todo", "code", Some("a")), &b, false, None, &probe) {
            Action::PassThenFreeze(c) => {
                assert_eq!((c.kind.as_str(), c.deploy_check.as_deref()), ("deploy", Some("curl -fsS https://x/health")));
                assert_ne!(c.hash, hash_of(&c.acceptance, &c.command, None), "the post-deploy check is part of the frozen hash");
            }
            _ => panic!("a deploy card with a check and a probe freezes"),
        }
        assert_eq!(code(&decide(&card("todo", "code", Some("a")), &b, false, None, &dflt(Some("make test")))), "409", "no production probe on the lane");
        let no_check = json!({"status": "doing", "verify_kind": "deploy"});
        assert_eq!(code(&decide(&card("todo", "code", Some("a")), &no_check, false, None, &probe)), "409", "no check");
        let lane_check = Defaults { deploy_check: Some("make smoke".into()), ..probe };
        assert_eq!(code(&decide(&card("todo", "code", Some("a")), &no_check, false, None, &lane_check)), "freeze", "the lane's CC_DEPLOY_CHECK counts");
        let bogus = json!({"status": "doing", "verify_kind": "vibes"});
        assert_eq!(code(&decide(&card("todo", "code", Some("a")), &bogus, false, None, &lane_check)), "409", "unknown kind");
        let edit = json!({"deploy_check": "true"});
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &edit, false, Some(&frozen()), &lane_check)), "409", "the check is frozen too");
    }

    #[test]
    fn the_deployed_sha_is_the_first_full_hex_token() {
        assert_eq!(parse_sha("eaa27b3cddd44cc1205b59a7985b6f13935cb814\n").as_deref(), Some("eaa27b3cddd44cc1205b59a7985b6f13935cb814"));
        assert_eq!(parse_sha("deploy_sha: \"EAA27B3CDDD44CC1205B59A7985B6F13935CB814\"").as_deref(), Some("eaa27b3cddd44cc1205b59a7985b6f13935cb814"));
        assert_eq!(parse_sha("unknown"), None, "a probe that names no sha is unmeasured, not 'not deployed'");
        assert_eq!(parse_sha("abc1234"), None, "short shas are ambiguous");
    }

    #[test]
    fn the_review_verdict_is_the_last_json_line_and_anything_else_is_unmeasured() {
        let out = "I read the diff.\n{\"note\": 1}\n{\"verdict\": \"fail\", \"findings\": [\"a.rs:3 test asserts nothing\"]}\n";
        assert_eq!(parse_review(out), Some((false, vec!["a.rs:3 test asserts nothing".to_string()])));
        assert_eq!(parse_review("{\"verdict\": \"pass\", \"findings\": []}"), Some((true, vec![])));
        assert_eq!(parse_review("looks good to me"), None, "prose is not a pass");
        assert_eq!(parse_review("{\"verdict\": \"maybe\"}"), None);
    }

    #[test]
    fn a_card_already_in_doing_can_freeze_a_contract_in_place() {
        let b = json!({"acceptance_criteria": ["it works"], "verify_cmd": "make test"});
        assert_eq!(code(&decide(&card("doing", "code", None), &b, false, None, &dflt(None))), "freeze");
        assert_eq!(code(&decide(&card("doing", "code", None), &json!({"verify_cmd": "make test"}), false, None, &dflt(None))), "409", "acceptance too");
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &b, false, Some(&frozen()), &dflt(None))), "409", "a frozen one stays frozen");
    }

    #[test]
    fn only_the_owner_sets_verified_on_a_contract_card() {
        let v = json!({"status": "verified", "reviewer": "gs12-extra-2"});
        assert_eq!(code(&decide(&card("done", "code", Some("a")), &v, false, Some(&frozen()), &dflt(None))), "409");
        assert_eq!(code(&decide(&card("done", "code", Some("a")), &v, true, Some(&frozen()), &dflt(None))), "pass");
        assert_eq!(code(&decide(&card("done", "code", Some("a")), &v, false, None, &dflt(None))), "pass",
            "a card finished before the contract keeps its verified path");
    }

    #[test]
    fn the_reviewer_runs_on_a_different_model_from_the_lane() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path();
        std::fs::create_dir_all(h.join("sessions")).unwrap();
        std::fs::write(h.join("sessions/o.env"), "CC_MODEL=\"claude-opus-5-5\"\n").unwrap();
        std::fs::write(h.join("sessions/s.env"), "CC_MODEL=\"sonnet\"\n").unwrap();
        assert!(!reviewer_model(h, "o").contains("opus"));
        assert!(reviewer_model(h, "s").contains("opus"));
        std::fs::write(h.join("sessions/o.env"), "CC_MODEL=\"claude-opus-5-5\"\nAMUX_CONTRACT_REVIEW_MODEL=\"x\"\n").unwrap();
        assert_eq!(reviewer_model(h, "o"), "x");
    }

    #[tokio::test]
    async fn a_checkout_clears_a_leftover_directory_a_killed_run_left_behind() {
        let home = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(home.path());
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [&["init", "-q"][..], &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "base"][..]] {
            assert!(std::process::Command::new("git").arg("-C").arg(&repo).args(args).status().unwrap().success());
        }
        let sha = git(&repo, &["rev-parse", "HEAD"]).await.unwrap();
        let tmp = home.path().join("tmp/contract/X-1-review-abc");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join(".git"), "gitdir: /nowhere\n").unwrap();
        fresh_checkout(&repo, &tmp, &sha).await.expect("a leftover unregistered directory must not block the checkout");
        assert!(tmp.join(".git").exists());
        let outside = home.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), "x").unwrap();
        assert!(fresh_checkout(&repo, &outside, &sha).await.is_err(), "a directory outside tmp/contract is never cleared");
        assert!(outside.join("keep").exists());
    }

    #[test]
    fn checks_and_reviews_follow_host_load_in_three_bands() {
        assert_eq!(adaptive_cap(4, 3, Some(2.0)), 1, "an overloaded host runs one at a time");
        assert_eq!(adaptive_cap(4, 3, Some(LOAD_HIGH_PER_CPU)), 1, "the high mark itself is overloaded");
        assert_eq!(adaptive_cap(4, 3, Some(1.0)), 3, "between the marks: the default");
        assert_eq!(adaptive_cap(4, 3, Some(0.3)), 4, "a quiet host runs the maximum");
        assert_eq!(adaptive_cap(4, 3, Some(LOAD_LOW_PER_CPU)), 4, "the low mark itself is quiet");
        assert_eq!(adaptive_cap(4, 3, None), 3, "an unreadable load is never read as quiet");
        assert_eq!(adaptive_cap(2, 3, Some(1.0)), 2, "the default never exceeds the configured maximum");
        assert_eq!(adaptive_cap(0, 3, Some(0.1)), 1, "never zero");
        assert_eq!(review_cap(5, Some(1.2)), 5, "reviews are not held to the CPU bands");
        assert_eq!(review_cap(5, None), 5);
        assert_eq!(review_cap(5, Some(REVIEW_HOLD_PER_CPU)), 3, "only an extreme load halves them");
        let rules = proof_rules("GS12 proof 6: Scale to zero", "... Ethan: make sure its all measured. each Ray Serve app at min_replicas 0 ...");
        assert!(rules.contains("COMPLETION PROOF") && rules.contains("Ray Serve app"), "{rules}");
        assert_eq!(proof_rules("GS12 6.9 Dependency resilience", "x"), "", "plan items keep the ordinary prompt");
    }

    #[test]
    fn the_verify_path_puts_the_lane_venv_first() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("server/.venv/bin")).unwrap();
        let p = verify_path_prefix(d.path(), "no-such-lane");
        assert!(p.starts_with(&d.path().join("server/.venv/bin").to_string_lossy().into_owned()), "{p}");
    }

    #[test]
    fn rule_counters_count_only_mapped_verdicts_inside_the_window() {
        let log = "2026-10-05T10:00:00.0Z  INFO x: contract_frozen verdict=\"contract_frozen\" n=1\n\
2026-10-05T10:00:01.0Z  INFO x: verdict=\"contract_verify_failed\"\n\
2026-10-05T10:00:02.0Z  INFO x: verdict=\"something_else\"\n\
2026-10-05T09:00:00.0Z  INFO x: verdict=\"contract_frozen\"\n\
not a log line\n";
        let since = chrono::DateTime::parse_from_rfc3339("2026-10-05T09:30:00Z").unwrap().timestamp() as f64;
        let (c, scanned) = count_verdicts(log, since);
        assert_eq!(c.get("contract_frozen"), Some(&1), "the 09:00 line is outside the window");
        assert_eq!(c.get("contract_verify_failed"), Some(&1));
        assert!(!c.contains_key("something_else"), "unmapped verdicts are not counted");
        assert_eq!(scanned, 3, "lines inside the window are what was considered");
    }

    #[test]
    fn rule9_only_the_lane_the_owner_and_the_hub_act_on_a_contract_board() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path();
        std::fs::create_dir_all(h.join("sessions")).unwrap();
        std::fs::create_dir_all(h.join("env")).unwrap();
        std::fs::write(h.join("sessions/spoke.env"), "CC_TAGS=\"g\"\n").unwrap();
        assert_eq!(rule9_peer(h, "spoke", "peer"), None, "switch off: rule 9 does not apply");
        std::fs::write(h.join("env/g.env"), "AMUX_CONTRACT_DONE=1\nAMUX_CONTRACT_HUB=hub\n").unwrap();
        assert_eq!(rule9_peer(h, "spoke", "peer"), Some(true), "a peer");
        assert_eq!(rule9_peer(h, "spoke", "hub"), Some(false), "the hub");
        assert_eq!(rule9_peer(h, "spoke", "spoke"), None, "the lane itself");
        assert_eq!(rule9_peer(h, "spoke", ""), None, "no worker header is the owner's path");
    }

    #[test]
    fn the_switch_resolves_server_env_then_worker_then_group_then_global() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path();
        std::fs::create_dir_all(h.join("sessions")).unwrap();
        std::fs::create_dir_all(h.join("env")).unwrap();
        std::fs::write(h.join("sessions/a.env"), "CC_TAGS=\"g\"\n").unwrap();
        assert!(!enabled_for(h, "a"), "off unless set");
        std::fs::write(h.join("env/g.env"), "AMUX_CONTRACT_DONE=1\n").unwrap();
        assert!(enabled_for(h, "a"), "group on");
        std::fs::write(h.join("sessions/a.env"), "CC_TAGS=\"g\"\nAMUX_CONTRACT_DONE=0\n").unwrap();
        assert!(!enabled_for(h, "a"), "worker beats group");
        std::fs::write(h.join("server.env"), "AMUX_CONTRACT_DONE=1\n").unwrap();
        assert!(enabled_for(h, "a"), "server.env beats every layer");
        std::fs::write(h.join("env/g.env"), "AMUX_CONTRACT_DONE=1\nAMUX_CONTRACT_RULES_OFF=\"6, 7\"\n").unwrap();
        assert!(rule_on(h, "a", "2") && !rule_on(h, "a", "7") && !rule_on(h, "a", "6"), "a rule can be held back");
    }
}
