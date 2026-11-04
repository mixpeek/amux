//! Capture-card reconciliation (AMUX-5238).
//!
//! # The incident
//!
//! amux auto-captures each owner prompt as an intake card: `apply` in
//! `api/board_lifecycle.rs` creates it with `source='command'`,
//! `creator='command-lifecycle'` and a log line `command MSG-<n>: ...`, and
//! stores the card id on the message (`cmd_history.card_id`). When the
//! interpreter gives up it files a "Structure request: ..." investigation
//! instead. Nothing ever closed either kind. The fleet stall review of
//! 2026-09-26 counted 11 on mixpeek-homepage-claude (MHC-923..944), six on
//! primis (PRIMI-271..276) and TP-35 on tubescience-parity, all sitting in
//! `todo` after the work they asked for had shipped and been verified. A
//! reader of the board saw a dozen open owner requests that were finished,
//! and auto-pickup kept offering them.
//!
//! # What closes one, and why this differs from `commit_mentions`
//!
//! `commit_mentions` deliberately NEVER closes a card: a lane-owned card named
//! in a commit may be context, partial work or a revert. A capture card is not
//! lane-owned work. It is the harness's own receipt of an owner message, and
//! the only question it asks is "was this message acted on". So it closes, as
//! `done` with the evidence written onto the card, when the lane itself cites
//! the message id together with proof:
//!
//! 1. a landed commit whose message names `MSG-<n>` (scanned by the periodic
//!    job below, over the same bounded history walk `commit_mentions` uses);
//! 2. a board note (`desc_append` / `evidence`) or a turn-end message that
//!    names `MSG-<n>` AND a commit sha that is reachable from a ref in the
//!    lane's checkout (checked with git, so a typed hex string is not enough);
//! 3. the lane moving ANOTHER card of its own to done/verified whose own text
//!    names `MSG-<n>` (the "Structure request" contract says exactly that:
//!    reconcile into canonical task ids and link them).
//!
//! # A split request closes card by card
//!
//! Intake can split one message into several cards (MSG-96581 became
//! MI-5988..5991, four independent outcomes). Every one of them carries the
//! same `command MSG-<n>:` line, so a citation used to close them all: one
//! board note on MI-5988 with its sha closed MI-5989..5991 as done with work
//! that had not shipped, and when mvs-infra reopened them, its own note
//! re-closed them a minute later (2026-10-28). When a message has more than
//! one capture card, a citation closes only the cards it is ON (the note's
//! card) or NAMES by id. Naming none closes none, logged as
//! `capture_reconcile_split_unnamed` with the ids so the lane can see why.
//!
//! A card a lane reopened after this job closed it stays open: its log
//! already holds a `capture-reconcile: MSG-<n>` close, and the lane's reopen
//! is the later judgement (`capture_reconcile_reopen_kept`).
//!
//! Never without evidence: every close writes the sha or the closing card into
//! `evidence` and the card log. Epics are skipped because
//! `board_drive::complete_finished_epics` already owns their completion.
//!
//! # Unreconciled
//!
//! A capture card still open 24h after capture gets the `unreconciled` tag,
//! which `amux board ls` prints as `[unreconciled]` and the Smart Board shows
//! as a derived status. The periodic job logs the count every tick, including
//! zero, under `verdict="capture_unreconciled"`.
//!
//! # Kill switch
//!
//! `AMUX_CAPTURE_RECONCILE=0`, scoped worker > group > global (the same
//! resolver `AMUX_COMMAND_LIFECYCLE` uses), resolved for the lane that owns
//! the capture card. Default ON. The whole job also honours the per-job
//! `AMUX_CAPTURE_RECONCILE_SECS=0` switch every periodic job has.

use crate::api::{session_verbs, AppState};
use crate::db::{board_store as bs, PendingEvent, WriteOutcome};
use amux_core::revision::{EntityType, MutationKind};
use rusqlite::Connection;
use std::collections::BTreeSet;

const JOB: &str = super::registry::ids::CAPTURE_RECONCILE;
pub(crate) const POLICY_KEY: &str = "AMUX_CAPTURE_RECONCILE";
pub(crate) const UNRECONCILED_TAG: &str = "unreconciled";
const ACTOR: &str = "capture-reconcile";
/// Hard ceiling on how many sha-shaped tokens one note may ask git about. A
/// pasted log could otherwise fan out into hundreds of subprocesses.
const MAX_SHA_PROBES: usize = 5;

fn tick_secs() -> u64 {
    crate::config::env_i64("AMUX_CAPTURE_RECONCILE_TICK_S", 3600).max(60) as u64
}

fn unreconciled_after_s() -> i64 {
    crate::config::env_i64("AMUX_CAPTURE_UNRECONCILED_AFTER_S", 24 * 3600).max(60)
}

/// The scoped kill switch, default ON. Same spelling rules as
/// `board_lifecycle::policy_enabled`: only an explicit off value disables.
pub(crate) fn enabled(session: &str) -> bool {
    let v = session_verbs::scoped_setting_in(&session_verbs::home(), session, POLICY_KEY)
        .or_else(|| std::env::var(POLICY_KEY).ok());
    policy_on(v.as_deref())
}

fn policy_on(value: Option<&str>) -> bool {
    !value.is_some_and(|v| {
        matches!(
            v.trim().trim_matches('"').to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

// ---------------------------------------------------------------------------
// Pure text rules
// ---------------------------------------------------------------------------

/// Every `MSG-<digits>` in `text`, word-bounded on both sides so `MSG-12`
/// never matches inside `MSG-123` or `XMSG-12`.
pub(crate) fn msg_ids_in(text: &str) -> BTreeSet<i64> {
    let bytes = text.as_bytes();
    let mut out = BTreeSet::new();
    let mut from = 0;
    while let Some(rel) = text[from..].find("MSG-") {
        let start = from + rel;
        let digits_at = start + 4;
        let mut end = digits_at;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if end > digits_at && before_ok && after_ok {
            if let Ok(n) = text[digits_at..end].parse::<i64>() {
                out.insert(n);
            }
        }
        from = end.max(start + 4);
    }
    out
}

/// The message ids THE INTAKE linked this card to: its own
/// `command MSG-<n>:` log lines. Deliberately not every MSG id in the
/// description, which a lane or a peer may have written for context.
pub(crate) fn intake_msg_ids(log: &str) -> BTreeSet<i64> {
    let mut out = BTreeSet::new();
    for line in log.lines() {
        if let Some(pos) = line.find("command MSG-") {
            let rest = &line[pos + "command ".len()..];
            let head: String = rest.chars().take_while(|c| *c != ':').collect();
            if rest.len() > head.len() {
                out.extend(msg_ids_in(&head));
            }
        }
    }
    out
}

/// Tokens shaped like an abbreviated or full commit sha: 7 to 40 lowercase
/// hex characters with at least one digit AND one letter. The mixed-class rule
/// keeps English words ("defaced") and plain numbers (timestamps, MSG ids)
/// out; every survivor is still checked against git before it counts.
pub(crate) fn sha_tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tok in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        if !(7..=40).contains(&tok.len()) {
            continue;
        }
        let hex = tok
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
        let digit = tok.chars().any(|c| c.is_ascii_digit());
        let letter = tok.chars().any(|c| ('a'..='f').contains(&c));
        if hex && digit && letter && !out.iter().any(|t| t == tok) {
            out.push(tok.to_string());
        }
    }
    out
}

/// Card ids named in `text` (`MI-5988`, `AMUX-6470`): an uppercase prefix,
/// a dash and digits, word-bounded. `MSG-<n>` is a message, never a card.
pub(crate) fn card_ids_in(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for tok in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')) {
        let Some((prefix, num)) = tok.rsplit_once('-') else { continue };
        if prefix == "MSG"
            || prefix.is_empty()
            || num.is_empty()
            || !num.chars().all(|c| c.is_ascii_digit())
            || !prefix.starts_with(|c: char| c.is_ascii_uppercase())
            || !prefix.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-')
        {
            continue;
        }
        out.insert(tok.to_string());
    }
    out
}

/// Which of a message's cards a citation is about: the card a note was
/// written on, and any card ids its text names. Only consulted when the
/// message was split into more than one card.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Target {
    pub on_card: Option<String>,
    pub named: BTreeSet<String>,
    /// Every card of the message: a request closed as a whole closes all of them.
    pub all: bool,
}

impl Target {
    pub(crate) fn of(text: &str, on_card: Option<&str>) -> Self {
        Target {
            on_card: on_card.filter(|c| !c.is_empty()).map(str::to_string),
            named: card_ids_in(text),
            all: false,
        }
    }
    fn covers(&self, card: &str) -> bool {
        self.all || self.on_card.as_deref() == Some(card) || self.named.contains(card)
    }
}

/// Is this an auto-captured intake card? Both shapes the harness has produced:
/// semantic intake (`source='command'`, `creator='command-lifecycle'`) and the
/// older raw prompt capture (`source='capture'`, `creator='amux'`).
pub(crate) fn is_capture_card(row: &bs::IssueRow) -> bool {
    matches!(
        (row.source.as_deref(), row.creator.as_str()),
        (Some("command"), "command-lifecycle") | (Some("capture"), "amux")
    )
}

/// Can reconciliation close this card? Open, live, and not an epic (epic
/// completion is `board_drive::complete_finished_epics`'s job).
pub(crate) fn closable(row: &bs::IssueRow) -> bool {
    is_capture_card(row)
        && row.archived == 0
        && !bs::is_terminal_status(&row.status)
        && row.item_type != "epic"
}

/// Should the periodic job tag this card `unreconciled` now?
pub(crate) fn should_mark_unreconciled(row: &bs::IssueRow, now: i64, after_s: i64) -> bool {
    is_capture_card(row)
        && row.archived == 0
        && !bs::is_terminal_status(&row.status)
        && now - row.created > after_s
        && !row.tags.iter().any(|t| t == UNRECONCILED_TAG)
}

/// What proved the capture was acted on. Rendered verbatim into `evidence`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Proof {
    Commit {
        sha: String,
        subject: String,
        repo: String,
    },
    Note {
        sha: String,
        source: &'static str,
        author: String,
    },
    CardDone {
        card: String,
        status: String,
        evidence: String,
    },
    /// The owner's request is closed in the requests ledger
    /// (`~/.amux/requests-closed.json`, `amux requests close`): landed, answered or
    /// declined, with the reason given there (docs/design/board-cards-stay-current.md).
    RequestClosed {
        reason: String,
        sha: String,
    },
}

impl Proof {
    fn render(&self, msg: i64) -> String {
        match self {
            Proof::Commit { sha, subject, repo } => format!(
                "capture-reconcile: MSG-{msg} cited by landed commit {sha} \"{subject}\" in {repo}"
            ),
            Proof::Note {
                sha,
                source,
                author,
            } => format!(
                "capture-reconcile: MSG-{msg} cited in a {source} by {author} with landed commit {sha}"
            ),
            Proof::RequestClosed { reason, sha } => {
                let r: String = reason.chars().take(300).collect();
                format!(
                    "capture-reconcile: MSG-{msg} closed in the requests ledger{}: {}",
                    if sha.is_empty() { String::new() } else { format!(" with {sha}") },
                    if r.trim().is_empty() { "no reason recorded" } else { r.trim() }
                )
            }
            Proof::CardDone {
                card,
                status,
                evidence,
            } => {
                let e: String = evidence.chars().take(300).collect();
                format!(
                    "capture-reconcile: MSG-{msg} cited by {card}, moved to {status} by its lane (its evidence: {})",
                    if e.trim().is_empty() { "none recorded" } else { e.trim() }
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Store half
// ---------------------------------------------------------------------------

/// Open capture cards linked to `msg`: through the intake's own log line, and
/// through `cmd_history.card_id` for the legacy capture shape.
pub(crate) fn capture_cards_for_msg(
    conn: &Connection,
    msg: i64,
) -> rusqlite::Result<Vec<bs::IssueRow>> {
    let mut ids: BTreeSet<String> = BTreeSet::new();
    let pattern = format!("%command MSG-{msg}:%");
    let mut st = conn.prepare(
        "SELECT id FROM issues WHERE deleted IS NULL AND COALESCE(archived,0)=0 \
         AND status NOT IN ('done','verified','discarded') AND log LIKE ?1",
    )?;
    for id in st
        .query_map([&pattern], |r| r.get::<_, String>(0))?
        .flatten()
    {
        ids.insert(id);
    }
    let linked: Option<String> = conn
        .query_row("SELECT card_id FROM cmd_history WHERE id=?1", [msg], |r| {
            r.get(0)
        })
        .unwrap_or(None);
    if let Some(id) = linked {
        ids.insert(id);
    }
    let mut out = Vec::new();
    for id in ids {
        if let Some(row) = bs::get_issue(conn, &id)? {
            // The LIKE above is a prefilter. `command MSG-12:` is also a
            // substring of nothing else, but the legacy link is not checked by
            // it, so re-derive openness on the row for both.
            if closable(&row) {
                out.push(row);
            }
        }
    }
    Ok(out)
}

/// How many capture cards, in any status, intake linked to `msg`. More than
/// one means the request was split, and a citation must name its card.
pub(crate) fn capture_cards_total(conn: &Connection, msg: i64) -> rusqlite::Result<usize> {
    let pattern = format!("%command MSG-{msg}:%");
    let mut ids: BTreeSet<String> = conn
        .prepare("SELECT id FROM issues WHERE deleted IS NULL AND log LIKE ?1")?
        .query_map([&pattern], |r| r.get::<_, String>(0))?
        .flatten()
        .collect();
    if let Ok(Some(id)) = conn.query_row("SELECT card_id FROM cmd_history WHERE id=?1", [msg], |r| {
        r.get::<_, Option<String>>(0)
    }) {
        ids.insert(id);
    }
    Ok(ids.len())
}

/// Did this job close `row` for `msg` before? Then it is open now only
/// because its lane reopened it.
pub(crate) fn reopened_after_reconcile(row: &bs::IssueRow, msg: i64) -> bool {
    row.log
        .as_deref()
        .is_some_and(|l| l.contains(&format!("capture-reconcile: MSG-{msg} ")))
}

/// Close one capture card as done with `proof`. Runs inside a write
/// transaction; re-reads the card so a racing close is a no-op.
fn close_in_txn(
    conn: &Connection,
    card: &str,
    msg: i64,
    proof: &Proof,
) -> rusqlite::Result<Result<Vec<PendingEvent>, String>> {
    let Some(mut row) = bs::get_issue(conn, card)? else {
        return Ok(Err("card vanished".into()));
    };
    if !closable(&row) {
        return Ok(Err(format!("no longer open ({})", row.status)));
    }
    let text = proof.render(msg);
    let now = chrono::Utc::now().timestamp();
    let stamp = chrono::Local::now().format("%H:%M").to_string();
    row.evidence = Some(
        match row.evidence.as_deref().filter(|e| !e.trim().is_empty()) {
            Some(prior) => format!("{prior}\n{text}"),
            None => text.clone(),
        },
    );
    row.log = Some(bs::append_log(row.log.as_deref(), &stamp, &text));
    row.updated = now;
    row.rev += 1;
    row.version += 1;
    let from = row.status.clone();
    bs::save_patched(conn, &mut row)?;
    // gate_ack, like epic completion: the proof IS what the done gate asks
    // for ("implemented and merged"), and it is recorded above in the same
    // transaction. Without it every typed card's default done gate would hold
    // the receipt open forever, which is the state this module exists to end.
    let opts = crate::db::advance::AdvanceOpts {
        expected_from: Some(from),
        gate_ack: true,
        skip_continuation: true,
        reason: Some(text.clone()),
        ..Default::default()
    };
    match crate::db::advance::advance(conn, card, "done", ACTOR, &opts)? {
        Ok(out) => {
            conn.execute(
                "DELETE FROM issue_tags WHERE issue_id=?1 AND tag=?2",
                rusqlite::params![card, UNRECONCILED_TAG],
            )?;
            Ok(Ok(out.events))
        }
        // Rolling back is the caller's job: returning Err from the write
        // closure is what undoes the evidence write above.
        Err(why) => Ok(Err(format!("{why:?}"))),
    }
}

/// Close the open capture cards for `msg` owned by a lane in `lanes` (or any
/// lane when `lanes` is None) with `proof`. When intake split `msg` into
/// several cards, only the ones `target` covers. Returns the ids closed.
pub(crate) async fn reconcile(
    state: &AppState,
    msg: i64,
    lanes: Option<&BTreeSet<String>>,
    exclude: Option<&str>,
    proof: Proof,
    target: &Target,
) -> Vec<String> {
    let (candidates, total) = match state
        .store
        .read_async(move |conn| Ok((capture_cards_for_msg(conn, msg)?, capture_cards_total(conn, msg)?)))
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(msg, error = %e, measured = false, n_considered = 0,
                verdict = "capture_reconcile_unmeasured", "capture reconcile: store unreadable");
            return vec![];
        }
    };
    let (candidates, kept): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|r| !reopened_after_reconcile(r, msg));
    for row in &kept {
        if crate::log_dedupe::first_this_bucket(
            &format!("capture-reconcile-reopen:{}:{msg}", row.id),
            crate::log_dedupe::hour_bucket(crate::config::now_f64()),
        ) {
            tracing::info!(card = %row.id, msg, session = ?row.session, measured = true, n_considered = 1,
                verdict = "capture_reconcile_reopen_kept",
                "capture card left open: its lane reopened it after this job closed it");
        }
    }
    let candidates: Vec<_> = if total > 1 {
        let (hit, missed): (Vec<_>, Vec<_>) = candidates.into_iter().partition(|r| target.covers(&r.id));
        if hit.is_empty() && !missed.is_empty() {
            let ids: Vec<&str> = missed.iter().map(|r| r.id.as_str()).collect();
            tracing::info!(msg, cards = ?ids, split_into = total, proof = %proof.render(msg),
                measured = true, n_considered = missed.len(), verdict = "capture_reconcile_split_unnamed",
                "capture reconcile: MSG was split into several cards and the citation names none of them; nothing closed");
        }
        hit
    } else {
        candidates
    };
    let mut closed = Vec::new();
    for row in candidates {
        let owner = row.session.clone().unwrap_or_default();
        if exclude == Some(row.id.as_str()) {
            continue;
        }
        if let Some(lanes) = lanes {
            if !lanes.contains(&owner) {
                continue;
            }
        }
        if !enabled(&owner) {
            tracing::info!(card = %row.id, msg, session = %owner, switch = POLICY_KEY,
                verdict = "capture_reconcile_disabled", "capture reconcile: kill switch is off for this lane");
            continue;
        }
        let (card, p) = (row.id.clone(), proof.clone());
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let slot_w = slot.clone();
        let res = state
            .store
            .write_async(move |conn| match close_in_txn(conn, &card, msg, &p)? {
                Ok(events) => Ok(WriteOutcome {
                    applied: true,
                    events,
                }),
                Err(why) => {
                    if let Ok(mut g) = slot_w.lock() {
                        *g = Some(why.clone());
                    }
                    // Undo the evidence/log write: a refused close must not
                    // leave a card claiming a reconciliation that did not land.
                    Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        std::io::Error::other(why),
                    )))
                }
            })
            .await;
        match res {
            Ok(r) if r.applied => {
                tracing::info!(card = %row.id, msg, session = %owner, proof = %proof.render(msg),
                    measured = true, n_considered = 1, verdict = "capture_reconciled",
                    "capture card closed on cited evidence (AMUX-5238)");
                closed.push(row.id.clone());
            }
            _ => {
                let why = slot.lock().ok().and_then(|g| g.clone()).unwrap_or_default();
                if crate::log_dedupe::first_this_bucket(
                    &format!("capture-reconcile-held:{}:{msg}", row.id),
                    crate::log_dedupe::hour_bucket(crate::config::now_f64()),
                ) {
                    tracing::warn!(card = %row.id, msg, session = %owner, reason = %why,
                        measured = true, n_considered = 1, verdict = "capture_reconcile_held",
                        "capture card NOT closed: the transition was refused");
                }
            }
        }
    }
    closed
}

/// Is `sha` a commit reachable from some ref in `dir`'s repository? "Landed"
/// here means committed onto a branch or remote-tracking ref, which covers the
/// graft-push lanes whose local HEAD lags origin.
async fn sha_landed(dir: &str, sha: &str) -> bool {
    if dir.trim().is_empty() {
        return false;
    }
    let commit = format!("{sha}^{{commit}}");
    let ok = crate::api::commit_mentions::git_output(dir, &["cat-file", "-e", &commit])
        .await
        .is_some_and(|o| o.status.success());
    if !ok {
        return false;
    }
    crate::api::commit_mentions::git_output(
        dir,
        &[
            "for-each-ref",
            "--count=1",
            "--format=%(refname)",
            "--contains",
            sha,
        ],
    )
    .await
    .is_some_and(|o| o.status.success() && !o.stdout.is_empty())
}

/// Does `lane` own any open capture card at all? The cheap gate in front of
/// every text path, so a lane with nothing to reconcile never pays for a git
/// probe or a transcript read.
fn lane_has_open_captures(conn: &Connection, lane: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM issues WHERE session=?1 AND deleted IS NULL \
         AND COALESCE(archived,0)=0 AND status NOT IN ('done','verified','discarded') \
         AND ((source='command' AND creator='command-lifecycle') OR (source='capture' AND creator='amux')))",
        [lane],
        |r| r.get::<_, i64>(0),
    )
    .map(|v| v != 0)
    .unwrap_or(false)
}

/// Path 2: a board note or a turn-end message from `author` naming MSG ids and
/// a sha. The sha must be landed in the author's checkout.
pub(crate) async fn on_text(
    state: &AppState,
    author: &str,
    text: &str,
    source: &'static str,
    on_card: Option<&str>,
) {
    let msgs = msg_ids_in(text);
    if msgs.is_empty() || author.is_empty() {
        return;
    }
    let a = author.to_string();
    let has = state
        .store
        .read_async(move |conn| Ok(lane_has_open_captures(conn, &a)))
        .await
        .unwrap_or(false);
    if !has {
        return;
    }
    let shas = sha_tokens(text);
    if shas.is_empty() {
        tracing::info!(session = %author, source, msgs = ?msgs, verdict = "capture_reconcile_no_sha",
            "capture reconcile: MSG id cited without a commit sha, nothing closed");
        return;
    }
    let dir = session_verbs::parse_env(author)
        .get_or("CC_DIR", "")
        .trim()
        .to_string();
    let mut landed = None;
    for sha in shas.iter().take(MAX_SHA_PROBES) {
        if sha_landed(&dir, sha).await {
            landed = Some(sha.clone());
            break;
        }
    }
    let Some(sha) = landed else {
        tracing::info!(session = %author, source, msgs = ?msgs, shas = ?shas,
            verdict = "capture_reconcile_sha_not_landed",
            "capture reconcile: cited sha is not a landed commit in the lane's checkout, nothing closed");
        return;
    };
    let lanes: BTreeSet<String> = [author.to_string()].into_iter().collect();
    let target = Target::of(text, on_card);
    for msg in msgs {
        reconcile(
            state,
            msg,
            Some(&lanes),
            None,
            Proof::Note {
                sha: sha.clone(),
                source,
                author: author.to_string(),
            },
            &target,
        )
        .await;
    }
}

/// Path 2, turn-end half. `last_text` reads the lane's final assistant message
/// and only runs when the lane has something to reconcile.
pub(crate) async fn on_turn_end<F>(state: &AppState, lane: &str, last_text: F)
where
    F: FnOnce() -> String + Send + 'static,
{
    if !enabled(lane) {
        return;
    }
    let l = lane.to_string();
    let has = state
        .store
        .read_async(move |conn| Ok(lane_has_open_captures(conn, &l)))
        .await
        .unwrap_or(false);
    if !has {
        return;
    }
    let text = crate::db::interactions::spawn_blocking(last_text)
        .await
        .unwrap_or_default();
    if text.contains("MSG-") {
        on_text(state, lane, &text, "turn-end message", None).await;
    }
}

/// The MSG ids a card that just reached a terminal status CITES in its own
/// text, minus the ones the intake itself linked it to. A sibling created by
/// the same intake "cites" its MSG only through that log line, and closing a
/// capture card because its sibling finished would be the harness vouching for
/// itself.
pub(crate) fn cited_by_closed_card(row: &bs::IssueRow) -> BTreeSet<i64> {
    let own = intake_msg_ids(row.log.as_deref().unwrap_or(""));
    msg_ids_in(&closed_card_text(row)).difference(&own).copied().collect()
}

/// The text a finished card cites through: title, desc, evidence and
/// last_result. Its MSG ids and the card ids it names come from the same
/// text, so a card named only in `last_result` still counts.
fn closed_card_text(row: &bs::IssueRow) -> String {
    let mut text = String::new();
    text.push_str(&row.title);
    text.push('\n');
    text.push_str(&row.desc);
    if let Some(e) = row.evidence.as_deref() {
        text.push('\n');
        text.push_str(e);
    }
    if let Some(r) = row.last_result.as_deref() {
        text.push('\n');
        text.push_str(r);
    }
    text
}

/// Path 3: `card` just moved to done/verified. Close the same lane's capture
/// cards for every MSG id it cites.
pub(crate) async fn on_card_done(state: &AppState, card: &str) {
    let c = card.to_string();
    let row = match state
        .store
        .read_async(move |conn| Ok(bs::get_issue(conn, &c)?))
        .await
    {
        Ok(Some(r)) => r,
        _ => return,
    };
    if !matches!(row.status.as_str(), "done" | "verified") {
        return;
    }
    let Some(lane) = row.session.clone().filter(|s| !s.is_empty()) else {
        return;
    };
    let msgs = cited_by_closed_card(&row);
    if msgs.is_empty() {
        return;
    }
    let lanes: BTreeSet<String> = [lane].into_iter().collect();
    let target = Target::of(&closed_card_text(&row), None);
    for msg in msgs {
        reconcile(
            state,
            msg,
            Some(&lanes),
            Some(&row.id),
            Proof::CardDone {
                card: row.id.clone(),
                status: row.status.clone(),
                evidence: row.evidence.clone().unwrap_or_default(),
            },
            &target,
        )
        .await;
    }
}

/// Every open capture card, for the periodic job.
fn open_capture_cards(conn: &Connection) -> rusqlite::Result<Vec<bs::IssueRow>> {
    let mut st = conn.prepare(
        "SELECT id FROM issues WHERE deleted IS NULL AND COALESCE(archived,0)=0 \
         AND status NOT IN ('done','verified','discarded') \
         AND ((source='command' AND creator='command-lifecycle') OR (source='capture' AND creator='amux'))",
    )?;
    let ids: Vec<String> = st
        .query_map([], |r| r.get::<_, String>(0))?
        .flatten()
        .collect();
    let mut out = Vec::new();
    for id in ids {
        if let Some(r) = bs::get_issue(conn, &id)? {
            out.push(r);
        }
    }
    Ok(out)
}

/// The linked MSG ids of a capture card: its intake log lines plus the
/// `cmd_history.card_id` back-link.
fn linked_msgs(conn: &Connection, row: &bs::IssueRow) -> BTreeSet<i64> {
    let mut out = intake_msg_ids(row.log.as_deref().unwrap_or(""));
    if let Ok(mut st) = conn.prepare("SELECT id FROM cmd_history WHERE card_id=?1") {
        if let Ok(rows) = st.query_map([&row.id], |r| r.get::<_, i64>(0)) {
            out.extend(rows.flatten());
        }
    }
    out
}

/// Tag every capture card open past the threshold. Returns (newly marked,
/// total open-and-unreconciled after the pass, open capture cards considered).
pub(crate) fn mark_unreconciled(
    conn: &Connection,
    now: i64,
    after_s: i64,
    lane_enabled: &dyn Fn(&str) -> bool,
) -> rusqlite::Result<(Vec<String>, usize, usize, Vec<PendingEvent>)> {
    let open = open_capture_cards(conn)?;
    let mut marked = Vec::new();
    let mut events = Vec::new();
    let mut total = 0usize;
    for row in &open {
        if row.tags.iter().any(|t| t == UNRECONCILED_TAG) {
            total += 1;
            continue;
        }
        if !should_mark_unreconciled(row, now, after_s) {
            continue;
        }
        if !lane_enabled(row.session.as_deref().unwrap_or("")) {
            continue;
        }
        conn.execute(
            "INSERT OR IGNORE INTO issue_tags (issue_id, tag, added_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![row.id, UNRECONCILED_TAG, now],
        )?;
        let msgs = linked_msgs(conn, row)
            .iter()
            .map(|m| format!("MSG-{m}"))
            .collect::<Vec<_>>()
            .join(", ");
        let hours = (now - row.created) / 3600;
        let stamp = chrono::Local::now().format("%H:%M").to_string();
        let line =
            format!(
            "unreconciled: this auto-captured request ({}) has been open {hours}h. If it shipped, \
             cite the MSG id with the landed sha (commit message, `amux board progress`, or a card \
             you mark done) and it closes itself; if it is not work, discard it with the reason.",
            if msgs.is_empty() { "no MSG link".to_string() } else { msgs }
        );
        let log = bs::append_log(row.log.as_deref(), &stamp, &line);
        conn.execute(
            "UPDATE issues SET log=?1 WHERE id=?2",
            rusqlite::params![log, row.id],
        )?;
        if let Some(fresh) = bs::get_issue(conn, &row.id)? {
            events.push(PendingEvent {
                entity_type: EntityType::Task,
                entity_id: fresh.id.clone(),
                mutation: MutationKind::Updated,
                payload: Some(fresh.snapshot()),
            });
        }
        marked.push(row.id.clone());
        total += 1;
    }
    Ok((marked, total, open.len(), events))
}

/// Path 1: landed commits naming a capture card's MSG id.
async fn reconcile_from_commits(state: &AppState) -> (usize, usize, bool) {
    let open = match state
        .store
        .read_async(|conn| {
            let rows = open_capture_cards(conn)?;
            Ok(rows
                .iter()
                .filter(|r| closable(r))
                .map(|r| (r.session.clone().unwrap_or_default(), linked_msgs(conn, r)))
                .collect::<Vec<_>>())
        })
        .await
    {
        Ok(v) => v,
        Err(_) => return (0, 0, false),
    };
    let owners: BTreeSet<String> = open
        .iter()
        .filter(|(s, m)| !s.is_empty() && !m.is_empty() && enabled(s))
        .map(|(s, _)| s.clone())
        .collect();
    let tokens: BTreeSet<String> = open
        .iter()
        .filter(|(s, _)| owners.contains(s))
        .flat_map(|(_, m)| m.iter().map(|n| format!("MSG-{n}")))
        .collect();
    if tokens.is_empty() {
        return (0, 0, true);
    }
    let Some((hits, truncated)) =
        crate::api::commit_mentions::scan_tokens(state, &owners, &tokens).await
    else {
        return (tokens.len(), 0, false);
    };
    let mut closed = 0usize;
    for (token, sha, subject, repo) in hits {
        let Some(msg) = msg_ids_in(&token).into_iter().next() else {
            continue;
        };
        let target = Target::of(&subject, None);
        closed += reconcile(state, msg, None, None, Proof::Commit { sha, subject, repo }, &target)
            .await
            .len();
    }
    (tokens.len(), closed, !truncated)
}

/// The requests ledger: message id -> (reason, sha) for every request closed by hand.
pub(crate) fn requests_ledger() -> std::collections::BTreeMap<i64, (String, String)> {
    let path = crate::api::session_verbs::home().join("requests-closed.json");
    let v: serde_json::Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    v.as_object()
        .into_iter()
        .flatten()
        .filter_map(|(id, c)| {
            let mid: i64 = id.trim_start_matches("MSG-").parse().ok()?;
            Some((
                mid,
                (
                    c["reason"].as_str().unwrap_or("").to_string(),
                    c["sha"].as_str().unwrap_or("").to_string(),
                ),
            ))
        })
        .collect()
}

/// Close the receipts whose request the ledger says is closed, all of the
/// message's cards at once (a split request is resolved as a whole there).
/// Returns (messages considered, cards closed).
pub(crate) async fn reconcile_from_ledger(
    state: &AppState,
    ledger: &std::collections::BTreeMap<i64, (String, String)>,
) -> (usize, usize) {
    let open = match state
        .store
        .read_async(|conn| {
            let rows = open_capture_cards(conn)?;
            Ok(rows
                .iter()
                .filter(|r| closable(r) && enabled(r.session.as_deref().unwrap_or("")))
                .flat_map(|r| linked_msgs(conn, r))
                .collect::<BTreeSet<i64>>())
        })
        .await
    {
        Ok(v) => v,
        Err(_) => return (0, 0),
    };
    let mut closed = 0usize;
    let target = Target { all: true, ..Target::default() };
    for msg in open.iter().filter(|m| ledger.contains_key(m)) {
        let (reason, sha) = ledger[msg].clone();
        closed += reconcile(state, *msg, None, None, Proof::RequestClosed { reason, sha }, &target)
            .await
            .len();
    }
    (open.len(), closed)
}

pub async fn tick(state: &AppState) {
    let (msgs_considered, closed, complete) = reconcile_from_commits(state).await;
    let (ledger_considered, ledger_closed) = reconcile_from_ledger(state, &requests_ledger()).await;
    let now = chrono::Utc::now().timestamp();
    let after = unreconciled_after_s();
    let slot = std::sync::Arc::new(std::sync::Mutex::new((0usize, 0usize, 0usize)));
    let slot_w = slot.clone();
    let marked = state
        .store
        .write_async(move |conn| {
            let (marked, total, considered, events) =
                mark_unreconciled(conn, now, after, &|lane| enabled(lane))?;
            if let Ok(mut g) = slot_w.lock() {
                *g = (marked.len(), total, considered);
            }
            Ok(WriteOutcome {
                applied: !events.is_empty(),
                events,
            })
        })
        .await;
    let (newly_marked, open_unreconciled, considered) = slot.lock().map(|g| *g).unwrap_or_default();
    // UNCONDITIONAL, INCLUDING ZERO (AF-178): a job that only speaks when it
    // has news cannot tell you it ran.
    match marked {
        Ok(_) => tracing::info!(
            measured = true,
            n_considered = considered,
            open_unreconciled,
            newly_marked,
            commit_msgs_considered = msgs_considered,
            commit_closed = closed,
            commit_scan_complete = complete,
            ledger_msgs_considered = ledger_considered,
            ledger_closed,
            verdict = "capture_unreconciled",
            "capture reconcile tick (AMUX-5238)"
        ),
        Err(e) => tracing::warn!(error = %e, measured = false, n_considered = 0,
            verdict = "capture_unreconciled_unmeasured", "capture reconcile: marking pass failed"),
    }
}

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, tick_secs(), move || {
        let st = state.clone();
        async move { tick(&st).await }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        (
            AppState {
                store,
                started: std::time::Instant::now(),
                build_hash: "test".into(),
                auth_token: None,
                reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            },
            dir,
        )
    }

    /// Seed a card in the exact live shape of MHC-923 (read off the board on
    /// 2026-09-26): source=command, creator=command-lifecycle, type
    /// investigation, status todo, the intake's own log line.
    async fn seed(st: &AppState, id: &str, session: &str, msg: i64, created: i64, kind: &str) {
        let (id, session, kind) = (id.to_string(), session.to_string(), kind.to_string());
        st.store
            .write_async(move |conn| {
                let log = format!(
                    "`2026-09-24`\n`10:46` command MSG-{msg}: create (Automatic interpretation exhausted its two attempts; the owning worker must reconcile the request before implementation)"
                );
                conn.execute(
                    "INSERT INTO issues (id,title,\"desc\",status,session,created,updated,archived,source,creator,type,log) \
                     VALUES (?1,'Structure request: Fix this and make sure it autoheals',?2,'todo',?3,?4,?4,0,'command','command-lifecycle',?5,?6)",
                    rusqlite::params![id, format!("Reconcile owner request MSG-{msg}. Read its original text."), session, created, kind, log],
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
    }

    fn row(st: &AppState, id: &str) -> bs::IssueRow {
        let conn = st.store.read().unwrap();
        bs::get_issue(&conn, id).unwrap().unwrap()
    }

    /// docs/design/board-cards-stay-current.md step 1: a request closed in the
    /// requests ledger closes its receipt, and all of a split request's cards;
    /// a request not in the ledger closes nothing.
    #[tokio::test]
    async fn a_request_closed_in_the_ledger_closes_its_receipts() {
        let (st, _d) = state();
        let now = 1_790_500_000;
        seed(&st, "RC-1", "lane", 501, now, "investigation").await;
        seed(&st, "RC-2", "lane", 502, now, "investigation").await;
        seed(&st, "RC-3", "lane", 502, now, "investigation").await;
        seed(&st, "RC-4", "lane", 503, now, "investigation").await;
        let mut ledger = std::collections::BTreeMap::new();
        ledger.insert(501, ("answered in chat, nothing to build".to_string(), String::new()));
        ledger.insert(502, ("landed".to_string(), "abc1234".to_string()));
        let (considered, closed) = reconcile_from_ledger(&st, &ledger).await;
        assert_eq!((considered, closed), (3, 3));
        for id in ["RC-1", "RC-2", "RC-3"] {
            let r = row(&st, id);
            assert!(matches!(r.status.as_str(), "done" | "verified" | "discarded"), "{id} is {}", r.status);
            assert!(r.log.as_deref().unwrap_or("").contains("closed in the requests ledger"), "{id} log: {:?}", r.log);
        }
        assert_eq!(row(&st, "RC-4").status, "todo", "a request not in the ledger stays open");
    }

    #[test]
    fn msg_ids_are_word_bounded() {
        let got = msg_ids_in("shipped MSG-68756 and (MSG-69134); not XMSG-1, not MSG-12a, MSG-");
        assert_eq!(got, [68756, 69134].into_iter().collect());
        assert!(msg_ids_in("MSG-12").contains(&12));
        assert!(!msg_ids_in("MSG-123").contains(&12));
    }

    #[test]
    fn intake_ids_come_only_from_the_intake_log_line() {
        let log =
            "`10:46` command MSG-68756: create (x)\n`11:00` peer note mentions MSG-1 for context";
        assert_eq!(intake_msg_ids(log), [68756].into_iter().collect());
    }

    #[test]
    fn sha_tokens_need_mixed_hex_and_skip_words_and_numbers() {
        let t =
            sha_tokens("landed c3ffbffe, defaced 1790261170 deadbeef 21800c39a MSG-68756 c3ffbffe");
        assert_eq!(t, vec!["c3ffbffe".to_string(), "21800c39a".to_string()]);
    }

    #[test]
    fn kill_switch_only_off_values_disable() {
        assert!(policy_on(None));
        assert!(policy_on(Some("1")));
        assert!(!policy_on(Some("0")));
        assert!(!policy_on(Some("\"off\"")));
    }

    #[tokio::test]
    async fn marks_only_old_open_capture_cards_once() {
        let (st, _d) = state();
        let now = 1_790_500_000;
        seed(
            &st,
            "MHC-923",
            "mhc",
            68756,
            now - 48 * 3600,
            "investigation",
        )
        .await;
        seed(&st, "MHC-944", "mhc", 69134, now - 3600, "investigation").await;
        // A non-capture card of the same age is never touched.
        st.store
            .write_async(move |conn| {
                conn.execute(
                    "INSERT INTO issues (id,title,status,session,created,updated,archived,source,creator) \
                     VALUES ('MHC-1','ordinary','todo','mhc',?1,?1,0,NULL,'mhc')",
                    [now - 90 * 3600],
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
        st.store
            .write_async(move |conn| {
                let (m, total, considered, _) = mark_unreconciled(conn, now, 24 * 3600, &|_| true)?;
                assert_eq!(m, vec!["MHC-923".to_string()]);
                assert_eq!((total, considered), (1, 2));
                let (m2, total2, _, _) = mark_unreconciled(conn, now, 24 * 3600, &|_| true)?;
                assert!(m2.is_empty(), "second pass must not re-mark or re-log");
                assert_eq!(total2, 1);
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .await
            .unwrap();
        let r = row(&st, "MHC-923");
        assert!(r.tags.contains(&UNRECONCILED_TAG.to_string()));
        assert_eq!(
            r.log.unwrap_or_default().matches("unreconciled:").count(),
            1
        );
        assert!(row(&st, "MHC-944").tags.is_empty());
        assert!(row(&st, "MHC-1").tags.is_empty());
        // Kill switch off for the lane: nothing marked.
        let (st2, _d2) = state();
        seed(&st2, "P-1", "primis", 1, now - 48 * 3600, "investigation").await;
        st2.store
            .write_async(move |conn| {
                let (m, _, _, _) = mark_unreconciled(conn, now, 24 * 3600, &|_| false)?;
                assert!(m.is_empty());
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn commit_proof_closes_with_evidence_and_clears_the_tag() {
        let (st, _d) = state();
        seed(&st, "TP-35", "tp", 69187, 0, "investigation").await;
        st.store
            .write_async(|conn| {
                conn.execute(
                    "INSERT INTO issue_tags (issue_id, tag, added_at) VALUES ('TP-35','unreconciled',1)",
                    [],
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
        let closed = reconcile(
            &st,
            69187,
            None,
            None,
            Proof::Commit {
                sha: "c3ffbffe1234".into(),
                subject: "feat: request table (MSG-69187)".into(),
                repo: "/repo".into(),
            },
            &Target::default(),
        )
        .await;
        assert_eq!(closed, vec!["TP-35".to_string()]);
        let r = row(&st, "TP-35");
        assert_eq!(r.status, "done");
        let ev = r.evidence.unwrap_or_default();
        assert!(
            ev.contains("c3ffbffe1234") && ev.contains("MSG-69187"),
            "{ev}"
        );
        assert!(!r.tags.contains(&UNRECONCILED_TAG.to_string()));
        // Idempotent: a second proof finds nothing open.
        let again = reconcile(
            &st,
            69187,
            None,
            None,
            Proof::Commit {
                sha: "abc1234".into(),
                subject: "x".into(),
                repo: "/r".into(),
            },
            &Target::default(),
        )
        .await;
        assert!(again.is_empty());
    }

    #[tokio::test]
    async fn epics_other_lanes_and_non_capture_cards_are_never_closed() {
        let (st, _d) = state();
        seed(&st, "E-1", "lane", 7, 0, "epic").await;
        seed(&st, "C-1", "other", 7, 0, "investigation").await;
        let lanes: BTreeSet<String> = ["lane".to_string()].into_iter().collect();
        let closed = reconcile(
            &st,
            7,
            Some(&lanes),
            None,
            Proof::Note {
                sha: "abc1234".into(),
                source: "board note",
                author: "lane".into(),
            },
            &Target::default(),
        )
        .await;
        assert!(
            closed.is_empty(),
            "epic skipped, other lane's card skipped: {closed:?}"
        );
        assert_eq!(row(&st, "E-1").status, "todo");
        assert_eq!(row(&st, "C-1").status, "todo");
    }

    #[tokio::test]
    async fn a_done_card_citing_the_msg_closes_its_lanes_capture_but_a_sibling_does_not() {
        let (st, _d) = state();
        seed(&st, "PRIMI-271", "primis", 68754, 0, "investigation").await;
        // Intake sibling for the SAME message: cites it only via the intake log.
        seed(&st, "PRIMI-272", "primis", 68754, 0, "code").await;
        st.store
            .write_async(|conn| {
                conn.execute(
                    "UPDATE issues SET status='done', evidence='sent' WHERE id='PRIMI-272'",
                    [],
                )?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .await
            .unwrap();
        on_card_done(&st, "PRIMI-272").await;
        assert_eq!(
            row(&st, "PRIMI-271").status,
            "todo",
            "sibling must not vouch"
        );

        // A lane-authored card whose own text cites the message.
        st.store
            .write_async(|conn| {
                conn.execute(
                    "INSERT INTO issues (id,title,\"desc\",status,session,created,updated,archived,source,creator,type,evidence) \
                     VALUES ('PRIMI-280','Paid pilot proposal','Structured from MSG-68754','done','primis',0,0,0,NULL,'primis','code','draft delivered')",
                    [],
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
        on_card_done(&st, "PRIMI-280").await;
        // MSG-68754 was split into two cards, so citing the message alone
        // proves neither of them (2026-10-28, MI-5988..5991).
        assert_eq!(row(&st, "PRIMI-271").status, "todo", "a split request needs the card named");
        st.store
            .write_async(|conn| {
                conn.execute(
                    "INSERT INTO issues (id,title,\"desc\",status,session,created,updated,archived,source,creator,type,evidence) \
                     VALUES ('PRIMI-281','Pilot follow-up','Structured from MSG-68754','done','primis',0,0,0,NULL,'primis','code','sent')",
                    [],
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
        // Named only in last_result, which is also where a lane's MSG citation
        // can live (land review F1, 2026-10-28).
        st.store
            .write_async(|conn| {
                conn.execute("UPDATE issues SET last_result='delivered as PRIMI-271' WHERE id='PRIMI-281'", [])?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
        on_card_done(&st, "PRIMI-281").await;
        let r = row(&st, "PRIMI-271");
        assert_eq!(r.status, "done");
        assert!(r.evidence.unwrap_or_default().contains("PRIMI-281"));
    }

    #[test]
    fn card_ids_are_read_and_a_msg_id_is_not_one() {
        let ids = card_ids_in("profile in MI-5988 (MSG-96581), see AMUX-6470; not mi-1, X-, MSG-2");
        assert_eq!(ids.into_iter().collect::<Vec<_>>(), vec!["AMUX-6470".to_string(), "MI-5988".to_string()]);
    }

    /// The 2026-10-28 shape: MSG-96581 split into four cards, a citation on
    /// one. Only that card closes; naming none closes none; a card the lane
    /// reopened after a close stays open.
    #[tokio::test]
    async fn a_split_request_closes_only_the_card_the_citation_is_on_or_names() {
        let (st, _d) = state();
        for id in ["MI-5988", "MI-5989", "MI-5990", "MI-5991"] {
            seed(&st, id, "mvs-infra", 96581, 0, "code").await;
        }
        let note = |sha: &str| Proof::Note { sha: sha.into(), source: "board note", author: "mvs-infra".into() };
        let unnamed = reconcile(&st, 96581, None, None, note("a423420b1bc"), &Target::of("MSG-96581 a423420b1bc", None)).await;
        assert!(unnamed.is_empty(), "a citation naming no card closed {unnamed:?}");
        let on = reconcile(&st, 96581, None, None, note("a423420b1bc"),
            &Target::of("profile landed, MSG-96581 a423420b1bc", Some("MI-5988"))).await;
        assert_eq!(on, vec!["MI-5988".to_string()]);
        for id in ["MI-5989", "MI-5990", "MI-5991"] {
            assert_eq!(row(&st, id).status, "todo", "{id} closed by a citation on MI-5988");
        }
        let named = reconcile(&st, 96581, None, None, note("b1c2d3e4f"), &Target::of("MI-5990 done, MSG-96581", None)).await;
        assert_eq!(named, vec!["MI-5990".to_string()]);
        // MI-5988 reopened by its lane: a fresh citation on it does not re-close it.
        st.store
            .write_async(|conn| {
                conn.execute("UPDATE issues SET status='todo' WHERE id='MI-5988'", [])?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
        let again = reconcile(&st, 96581, None, None, note("a423420b1bc"),
            &Target::of("reopened: MSG-96581 a423420b1bc", Some("MI-5988"))).await;
        assert!(again.is_empty(), "a reopened card was closed again: {again:?}");
        assert_eq!(row(&st, "MI-5988").status, "todo");
    }

    /// Pre-fix shape: a note naming the MSG id but no sha closes nothing.
    #[tokio::test]
    async fn a_note_without_a_landed_sha_closes_nothing() {
        let (st, _d) = state();
        seed(&st, "MHC-930", "mhc-test-lane", 5, 0, "investigation").await;
        on_text(
            &st,
            "mhc-test-lane",
            "done with MSG-5, shipped",
            "board note",
            None,
        )
        .await;
        assert_eq!(row(&st, "MHC-930").status, "todo");
        // A sha-shaped token that is not a commit in any checkout.
        on_text(
            &st,
            "mhc-test-lane",
            "MSG-5 landed in abc1234ff",
            "board note",
            None,
        )
        .await;
        assert_eq!(row(&st, "MHC-930").status, "todo");
    }

    #[tokio::test]
    async fn sha_landed_requires_a_commit_on_a_ref() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().to_str().unwrap().to_string();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "fix: x (MSG-5)",
        ]);
        let sha = String::from_utf8(git(&["rev-parse", "--short=10", "HEAD"]).stdout).unwrap();
        let sha = sha.trim();
        assert!(
            sha_landed(&dir, sha).await,
            "a committed sha on a branch is landed"
        );
        assert!(
            !sha_landed(&dir, "abc1234ff0").await,
            "an invented sha is not"
        );
        assert!(!sha_landed("", sha).await, "no checkout, no proof");
    }

    /// The derived Smart Board states this change adds, driven through the
    /// real `derive_display_status`.
    #[tokio::test]
    async fn derived_states_name_unreconciled_captures_and_finished_epics() {
        let (st, _d) = state();
        seed(&st, "MHC-923", "mhc", 1, 0, "investigation").await;
        st.store
            .write_async(|conn| {
                conn.execute("INSERT INTO issue_tags (issue_id, tag, added_at) VALUES ('MHC-923','unreconciled',1)", [])?;
                conn.execute("INSERT INTO issues (id,title,status,session,created,updated,archived,type) VALUES ('LV-109','epic','backlog','lv',0,0,0,'epic')", [])?;
                conn.execute("INSERT INTO issues (id,title,status,session,created,updated,archived,type,epic) VALUES ('LV-110','a','done','lv',0,0,0,'code','LV-109')", [])?;
                conn.execute("INSERT INTO issues (id,title,status,session,created,updated,archived,type,epic) VALUES ('LV-111','b','todo','lv',0,0,0,'code','LV-109')", [])?;
                conn.execute("INSERT INTO issues (id,title,status,session,created,updated,archived,type) VALUES ('E-0','empty epic','backlog','lv',0,0,0,'epic')", [])?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
        let working = BTreeSet::new();
        let derive = |st: &AppState, id: &str| {
            let conn = st.store.read().unwrap();
            let r = bs::get_issue(&conn, id).unwrap().unwrap();
            crate::api::board::derive_display_status(&r, 10, &working, &conn)
        };
        assert_eq!(derive(&st, "MHC-923"), "unreconciled");
        assert_eq!(derive(&st, "LV-109"), "backlog", "one child still open");
        assert_eq!(
            derive(&st, "E-0"),
            "backlog",
            "an epic with no children is not finished"
        );
        st.store
            .write_async(|conn| {
                conn.execute("UPDATE issues SET status='verified' WHERE id='LV-111'", [])?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .await
            .unwrap();
        assert_eq!(derive(&st, "LV-109"), "awaiting-verify");
    }
}
