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
}

#[derive(Clone, Debug, PartialEq)]
pub struct Contract {
    pub card: String,
    pub acceptance: String,
    pub command: String,
    pub hash: String,
    pub state: String,
    pub sha: Option<String>,
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

fn hash_of(acceptance: &str, command: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(acceptance.as_bytes());
    h.update([0u8]);
    h.update(command.as_bytes());
    format!("{:x}", h.finalize())[..16].to_string()
}

fn nonempty(v: Option<&str>) -> Option<String> {
    v.map(str::trim).filter(|s| !s.is_empty() && *s != "[]" && *s != "null").map(String::from)
}

pub fn load(conn: &Connection, card: &str) -> rusqlite::Result<Option<Contract>> {
    conn.query_row(
        "SELECT card, acceptance, command, hash, state, sha FROM card_contracts WHERE card = ?1",
        [card],
        |r| {
            Ok(Contract {
                card: r.get(0)?,
                acceptance: r.get(1)?,
                command: r.get(2)?,
                hash: r.get(3)?,
                state: r.get(4)?,
                sha: r.get(5)?,
            })
        },
    )
    .optional()
}

fn save(conn: &Connection, c: &Contract, now: f64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO card_contracts (card, acceptance, command, hash, frozen_at, state) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(card) DO UPDATE SET acceptance = ?2, command = ?3, hash = ?4, frozen_at = ?5, state = ?6",
        rusqlite::params![c.card, c.acceptance, c.command, c.hash, now, c.state],
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
pub fn decide(card: &Card, body: &Value, owner: bool, existing: Option<&Contract>, default_cmd: Option<&str>) -> Action {
    if card.item_type != "code" {
        return Action::Pass;
    }
    let edits_contract = ["acceptance_criteria", "verify_cmd"].iter().any(|k| body.get(*k).is_some());
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
    match body.get("status").and_then(Value::as_str).unwrap_or("") {
        "cannot_satisfy" => {
            let reason = nonempty(body.get("reason").and_then(Value::as_str))
                .or_else(|| nonempty(body.get("desc_append").and_then(Value::as_str)))
                .unwrap_or_else(|| "no reason given".into());
            tracing::info!(card = %card.id, lane = %card.lane, measured = true, n_considered = 1,
                verdict = "contract_cannot_satisfy", "a worker declared its contract unsatisfiable");
            Action::Rewrite(json!({
                "status": "needsyou",
                "ask_actor": "owner",
                "ask_type": "decision",
                "ask_question": format!("{} cannot satisfy its frozen contract as written ({reason}): change the contract, re-scope the card, or close it?", card.id),
                "ask_unblocks": "The owner's change to the contract or the card, after which the lane resumes or the card closes.",
                "desc_append": format!("\ncannot_satisfy (contract rule 2): {reason}"),
            }))
        }
        "doing" if card.status != "doing" => {
            let acceptance = nonempty(body.get("acceptance_criteria").map(|v| v.to_string()).as_deref().map(|s| s.trim_matches('"')))
                .or_else(|| nonempty(card.acceptance.as_deref()));
            let command = nonempty(body.get("verify_cmd").and_then(Value::as_str))
                .or_else(|| existing.map(|c| c.command.clone()))
                .or_else(|| nonempty(default_cmd));
            match (acceptance, command) {
                (Some(a), Some(c)) => Action::PassThenFreeze(Contract {
                    card: card.id.clone(),
                    hash: hash_of(&a, &c),
                    acceptance: a,
                    command: c,
                    state: "frozen".into(),
                    sha: None,
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
        "done" if !owner && card.status == "doing" => match existing {
            Some(c) if c.state == "verifying" => Action::Respond(
                (StatusCode::ACCEPTED, Json(json!({"ok": true, "verification": "already_running", "card": card.id}))).into_response(),
            ),
            Some(_) => Action::Pass, // the route starts the verification
            None => Action::Respond(refuse(StatusCode::CONFLICT, "contract_missing",
                format!("{} has no frozen contract to verify against", card.id),
                json!({"worker": "add acceptance_criteria and verify_cmd, then request done again (they freeze now)"}))),
        },
        _ => Action::Pass,
    }
}

/// Freeze a contract after a successful entry to doing.
pub async fn freeze(state: &AppState, c: Contract) {
    let now = crate::config::now_f64();
    let c2 = c.clone();
    let r = state.store.write_async(move |conn| {
        save(conn, &c2, now)?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await;
    tracing::info!(card = %c.card, hash = %c.hash, ok = r.is_ok(), measured = true, n_considered = 1,
        verdict = "contract_frozen", "froze a card's acceptance criteria and verify command");
}

async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = tokio::process::Command::new("git").arg("-C").arg(dir).args(args).output().await.map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
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
    let (st, c, l) = (state.clone(), card.to_string(), lane.to_string());
    tokio::spawn(async move { run_verification(&st, &c, &l).await });
    (StatusCode::ACCEPTED, Json(json!({
        "ok": true, "verification": "started", "card": card,
        "note": "The server runs the frozen verify command at your committed HEAD and grants done if it passes. A failure comes back to you with the output.",
    }))).into_response()
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
    let _ = git(&tree, &["worktree", "remove", "--force", &tmp.to_string_lossy()]).await;
    git(&tree, &["worktree", "add", "--detach", &tmp.to_string_lossy(), &sha]).await.map_err(|e| (Some(sha.clone()), format!("could not check out {sha}: {e}")))?;
    let ws = crate::fanout_workspace::Workspace {
        repo: tree.to_string_lossy().into_owned(),
        path: tree.to_string_lossy().into_owned(),
        branch: lane.to_string(),
        base: String::new(),
    };
    let r = crate::fanout_workspace::verify_commands(&ws, &tmp.to_string_lossy(), &[contract.command.as_str()], timeout, &|| Ok(())).await;
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
        }
        Err((sha, why)) => {
            let short = tail(&why, 1500);
            let (c, s2, log) = (card.to_string(), sha.clone(), short.clone());
            let _ = state.store.write_async(move |conn| {
                set_state(conn, &c, "failed", s2.as_deref(), &log, now)?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            }).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn card(status: &str, ty: &str, acceptance: Option<&str>) -> Card {
        Card { id: "T-1".into(), lane: "lane".into(), status: status.into(), item_type: ty.into(), acceptance: acceptance.map(String::from) }
    }
    fn frozen() -> Contract {
        Contract { card: "T-1".into(), acceptance: "it works".into(), command: "make test".into(), hash: "h".into(), state: "frozen".into(), sha: None }
    }
    fn code(a: &Action) -> String {
        match a {
            Action::Respond(r) => r.status().as_u16().to_string(),
            Action::Pass => "pass".into(),
            Action::Rewrite(_) => "rewrite".into(),
            Action::PassThenFreeze(_) => "freeze".into(),
        }
    }

    #[test]
    fn a_code_card_needs_acceptance_and_a_command_to_enter_doing() {
        let b = json!({"status": "doing"});
        assert_eq!(code(&decide(&card("todo", "code", None), &b, false, None, Some("make test"))), "409", "no acceptance");
        assert_eq!(code(&decide(&card("todo", "code", Some("it works")), &b, false, None, None)), "409", "no command");
        assert_eq!(code(&decide(&card("todo", "code", Some("[]")), &b, false, None, Some("x"))), "409", "an empty list is not acceptance");
        match decide(&card("todo", "code", Some("it works")), &b, false, None, Some("make test")) {
            Action::PassThenFreeze(c) => assert_eq!((c.command.as_str(), c.acceptance.as_str()), ("make test", "it works")),
            _ => panic!("a complete contract freezes"),
        }
        let inline = json!({"status": "doing", "acceptance_criteria": ["it works"], "verify_cmd": "cargo test"});
        assert_eq!(code(&decide(&card("todo", "code", None), &inline, false, None, None)), "freeze", "fields in the PATCH count");
        assert_eq!(code(&decide(&card("todo", "chore", None), &b, false, None, None)), "pass", "only code cards");
    }

    #[test]
    fn a_frozen_contract_and_force_belong_to_the_owner() {
        let edit = json!({"verify_cmd": "true"});
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &edit, false, Some(&frozen()), None)), "409");
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &edit, true, Some(&frozen()), None)), "pass", "the owner may edit");
        let force = json!({"status": "done", "force": true, "reason": "x"});
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &force, false, Some(&frozen()), None)), "403");
    }

    #[test]
    fn done_is_verified_by_the_server_and_cannot_satisfy_goes_to_the_owner() {
        let done = json!({"status": "done", "evidence": "trust me", "gate_checked": ["x"]});
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &done, false, Some(&frozen()), None)), "pass", "the route starts verification");
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &done, false, None, None)), "409", "nothing frozen to verify");
        let running = Contract { state: "verifying".into(), ..frozen() };
        assert_eq!(code(&decide(&card("doing", "code", Some("a")), &done, false, Some(&running), None)), "202");
        match decide(&card("doing", "code", Some("a")), &json!({"status": "cannot_satisfy", "reason": "the fixture is gone"}), false, Some(&frozen()), None) {
            Action::Rewrite(v) => {
                assert_eq!(v["status"], "needsyou");
                assert!(v["ask_question"].as_str().unwrap().contains("the fixture is gone"));
            }
            _ => panic!("cannot_satisfy becomes an owner ask"),
        }
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
    }
}
