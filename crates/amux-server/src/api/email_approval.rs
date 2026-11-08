//! Per-message human approval for worker-originated EXTERNAL email
//! (AMUX-3510 — Ethan's rule, stated twice on 2026-08-22: "we never send an
//! email to a customer before Ethan gives explicit approval" and "workers can
//! never send emails to people without explicit permissions").
//!
//! # The incident this encodes
//!
//! At 13:49:52Z the autodesk lane POSTed /api/email/reply and a 4KB message
//! reached four external Autodesk addresses on a live customer thread. Its
//! authorization was a general "do them all" about a board queue. The lane
//! had read the outbound-comms rules and had itself flagged the send as
//! needing Ethan — and still sent. Their own words in the request that led
//! here: "a rule I can talk myself past is not a control." So the control
//! lives in the API, not in worker prompts.
//!
//! # The mechanism
//!
//! A send or reply whose request carries a worker origin (X-Amux-Worker /
//! X-Amux-Session) and whose RESOLVED recipients include any address outside
//! the internal domains is refused with `code: "approval_required"` and the
//! frozen request is written to `~/.amux/email-approvals/apr_<hex>.json`.
//! A HUMAN (a request with NO worker origin — the dashboard) releases it via
//! POST /api/email/approve/{id}, which executes exactly the frozen payload:
//! one-shot (an atomic rename consumes the file, so a raced double-approve
//! loses cleanly), expiring (1h), and audited (the send-audit row carries
//! approval_id + the originating session; the refusal itself is also a
//! ledger row, so a gated send that nobody approves still left a trace).
//!
//! # What this is, honestly
//!
//! An accident guardrail, not a security boundary. Every worker on this
//! machine can read the auth token and could omit its own origin header; a
//! worker that does that is lying about its identity, which the send-audit
//! ledger already treats as a forensics flag (AMUX-1897: an unattributed
//! send cost real investigation time). The gate stops the incident class
//! that actually happened — an honest lane talking itself into a send.
//!
//! # Knobs (server.env, both optional)
//!
//! - `AMUX_INTERNAL_EMAIL_DOMAINS` — comma list ADDED to the built-in
//!   OUR_DOMAINS set. A recipient at any of these needs no approval.
//! - `AMUX_EMAIL_EXTERNAL_EXEMPT` — comma list of session names whose
//!   external sends skip the gate. DELIBERATELY EMPTY by default: gtm-ticker
//!   and refresh-house sent 26 external emails in the last 14 days and WILL
//!   start queueing approvals — that is the mandate as given, and the
//!   exemption is Ethan's one env line to write, not this code's to assume.

use crate::config::now_f64;
use crate::integrations::email::OUR_DOMAINS;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// An approval not consumed within this window expires (autodesk's sketch:
/// "an hour seems right"). Outbound email is EXEMPT since AMUX-5240: see
/// [`never_expires`].
pub const APPROVAL_TTL_S: f64 = 3600.0;

/// Does this approval stay pending until a human acts on it? (AMUX-5240)
///
/// Outbound email (`send` / `reply`) does. The 1h TTL was sized for a human
/// sitting at the dashboard while an interactive lane waited, and the parked
/// drafts that matter come from SCHEDULED lanes: gtm-ticker's SCHED-498 drafts
/// replies and parks each one, the owner reads the queue hours later, the
/// draft has expired, and the next fire re-requests it under a new id. Four
/// drafts (Rami, Abraham, John, AssemblyAI) expired twice that way in the
/// 2026-09-26 fleet review. Expiry protected nothing: an expired draft was
/// simply asked for again, so the only thing the TTL did was throw away the
/// owner's place in the queue. Now it stays until approved or discarded, and
/// a re-request reuses it ([`request_approval`]).
///
/// Any other kind, if one is ever frozen here, keeps the TTL.
pub fn never_expires(doc: &Value) -> bool {
    matches!(
        doc.get("endpoint").and_then(Value::as_str),
        Some("send") | Some("reply")
    )
}

fn expired(doc: &Value, now: f64) -> bool {
    let created = doc.get("created").and_then(Value::as_f64).unwrap_or(0.0);
    !never_expires(doc) && now - created > APPROVAL_TTL_S
}

/// The identity of a DRAFT for dedupe (AMUX-5240): the requesting lane, the
/// resolved recipients (to + cc, lowercased, sorted) and the subject with
/// reply/forward prefixes stripped, so "Re: Pilot" on the thread and "Pilot"
/// to the same person are one draft. A reply with no subject falls back to
/// its anchor message id. `None` when there are no recipients: nothing to
/// match on, so never dedupe.
///
/// The lane is part of the key on purpose. Two lanes writing to the same
/// person about the same thing are two drafts a human should see; one lane
/// asking again on its next schedule fire is the same draft.
pub fn draft_key(session: &str, preview: &Value, payload: &Value) -> Option<String> {
    let field = |k: &str| {
        preview
            .get(k)
            .and_then(Value::as_str)
            .or_else(|| payload.get(k).and_then(Value::as_str))
            .unwrap_or("")
    };
    let mut rcpts: Vec<String> = format!("{},{}", field("to"), field("cc"))
        .split(',')
        .map(|a| a.trim().to_lowercase())
        .filter(|a| a.contains('@'))
        .collect();
    rcpts.sort();
    rcpts.dedup();
    if rcpts.is_empty() {
        return None;
    }
    let mut subject = field("subject").trim().to_lowercase();
    loop {
        let before = subject.len();
        for pre in ["re:", "fwd:", "fw:"] {
            if let Some(rest) = subject.strip_prefix(pre) {
                subject = rest.trim_start().to_string();
            }
        }
        if subject.len() == before {
            break;
        }
    }
    let thread = if subject.is_empty() {
        format!(
            "msg:{}",
            preview
                .get("in_reply_to")
                .or_else(|| payload.get("message_id"))
                .and_then(Value::as_str)
                .unwrap_or("")
        )
    } else {
        format!("subj:{subject}")
    };
    Some(format!("{}|{}|{thread}", session.trim().to_lowercase(), rcpts.join(",")))
}

/// Result of [`request_approval`].
#[derive(Debug, Clone, PartialEq)]
pub struct Requested {
    pub id: String,
    /// True when an existing pending approval for the same draft was returned.
    pub reused: bool,
    /// How many times this draft has now been requested (1 on creation).
    pub requests: u64,
    /// The re-request's body differs from the frozen one. The FROZEN draft is
    /// what a human approves; the caller is told so rather than having the
    /// draft swapped under a reviewer who may already have it open.
    pub draft_changed: bool,
}

fn requests_sidecar(home: &Path, id: &str) -> PathBuf {
    approvals_dir(home).join(format!("{id}.requests.json"))
}

fn read_requests(home: &Path, id: &str) -> (u64, f64) {
    std::fs::read_to_string(requests_sidecar(home, id))
        .ok()
        .and_then(|r| serde_json::from_str::<Value>(&r).ok())
        .map(|v| {
            (
                v.get("requests").and_then(Value::as_u64).unwrap_or(1),
                v.get("last_requested").and_then(Value::as_f64).unwrap_or(0.0),
            )
        })
        .unwrap_or((1, 0.0))
}

/// Freeze a refused request, or return the pending approval that already
/// holds the same draft (AMUX-5240). Handlers call this; `create_approval`
/// stays the raw primitive.
///
/// The re-request count lives in a SIDECAR (`<id>.requests.json`), never in
/// the approval file itself. Rewriting `<id>.json` could race `consume`'s
/// rename and resurrect a draft a human just released, which would send it
/// twice. The sidecar is not an id-shaped `.json` name, so `list_pending`
/// never mistakes it for an approval.
pub fn request_approval(
    home: &Path,
    session: &str,
    endpoint: &str,
    payload: Value,
    preview: Value,
) -> std::io::Result<Requested> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(key) = draft_key(session, &preview, &payload) {
        if let Some(existing) = find_pending_draft(home, &key) {
            let id = existing
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let (prior, _) = read_requests(home, &id);
            let requests = prior + 1;
            let frozen_body = existing
                .pointer("/payload/body")
                .and_then(Value::as_str)
                .unwrap_or("");
            let new_body = payload.get("body").and_then(Value::as_str).unwrap_or("");
            let draft_changed = frozen_body.trim() != new_body.trim();
            let _ = std::fs::write(
                requests_sidecar(home, &id),
                json!({"requests": requests, "last_requested": now_f64(),
                       "last_draft_changed": draft_changed})
                .to_string(),
            );
            return Ok(Requested {
                id,
                reused: true,
                requests,
                draft_changed,
            });
        }
    }
    let id = create_approval(home, session, endpoint, payload, preview)?;
    Ok(Requested {
        id,
        reused: false,
        requests: 1,
        draft_changed: false,
    })
}

/// The pending, unexpired approval whose draft key is `key`, oldest first.
fn find_pending_draft(home: &Path, key: &str) -> Option<Value> {
    let dir = approvals_dir(home);
    let now = now_f64();
    let mut hits: Vec<Value> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(".json")?;
            if !valid_id(id) {
                return None;
            }
            let doc: Value = serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()?;
            let session = doc.get("session").and_then(Value::as_str).unwrap_or("");
            let preview = doc.get("preview").cloned().unwrap_or(Value::Null);
            let payload = doc.get("payload").cloned().unwrap_or(Value::Null);
            (!expired(&doc, now) && draft_key(session, &preview, &payload).as_deref() == Some(key))
                .then_some(doc)
        })
        .collect();
    hits.sort_by(|a, b| {
        let ka = a.get("created").and_then(Value::as_f64).unwrap_or(0.0);
        let kb = b.get("created").and_then(Value::as_f64).unwrap_or(0.0);
        ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.into_iter().next()
}

fn env_list(home: &Path, key: &str) -> Vec<String> {
    crate::api::settings::effective_env(home, key)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The domains that count as "us": the built-in set plus server.env additions.
pub fn internal_domains(home: &Path) -> Vec<String> {
    let mut out: Vec<String> = OUR_DOMAINS.iter().map(|d| d.to_string()).collect();
    out.extend(env_list(home, "AMUX_INTERNAL_EMAIL_DOMAINS"));
    out
}

/// Sessions whose external sends skip the gate. Empty unless Ethan sets it.
pub fn exempt_sessions(home: &Path) -> Vec<String> {
    env_list(home, "AMUX_EMAIL_EXTERNAL_EXEMPT")
}

/// Whether this worker has a standing authorization to send external email
/// without creating a one-shot human approval.
///
/// The first-class setting is the ordinary scoped environment key
/// `AMUX_EMAIL_EXTERNAL_ALLOW`, resolved worker > group > global by the same
/// resolver that supplies every other worker policy. The older global
/// `AMUX_EMAIL_EXTERNAL_EXEMPT` comma-list remains accepted for compatibility,
/// but new configuration should use the boolean: it can be understood and
/// changed for one worker from the Configurations UI and can be explicitly
/// denied at a more-specific layer.
pub fn external_email_allowed(home: &Path, session: &str) -> bool {
    let scoped =
        crate::api::session_verbs::scoped_setting_in(home, session, "AMUX_EMAIL_EXTERNAL_ALLOW");
    if let Some(value) = scoped {
        return matches!(
            value.trim().trim_matches('"').to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        );
    }
    exempt_sessions(home).contains(&session.trim().to_lowercase())
}

/// The classifier, pure so every cell is testable: which of these addresses
/// are EXTERNAL? `addrs` is one or more comma-separated lists concatenated
/// with commas; a connected account's own address (a self-send) is internal.
pub fn external_recipients(
    addrs: &str,
    internal_domains: &[String],
    connected: &[String],
) -> Vec<String> {
    let conn_lc: Vec<String> = connected.iter().map(|a| a.to_lowercase()).collect();
    addrs
        .split(',')
        .map(str::trim)
        .filter(|a| !a.is_empty() && a.contains('@'))
        .filter(|a| {
            let al = a.to_lowercase();
            let dom = al.rsplit('@').next().unwrap_or("");
            !conn_lc.contains(&al) && !internal_domains.iter().any(|d| dom == d.to_lowercase())
        })
        .map(String::from)
        .collect()
}

pub fn approvals_dir(home: &Path) -> PathBuf {
    let d = home.join("email-approvals");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Ids are `apr_<16 hex>` and NOTHING else reaches the filesystem — the id
/// arrives in a URL path, so the shape check is the traversal guard.
pub fn valid_id(id: &str) -> bool {
    id.len() == 20
        && id.starts_with("apr_")
        && id[4..]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Freeze a refused request to disk; returns the approval id.
pub fn create_approval(
    home: &Path,
    session: &str,
    endpoint: &str,
    payload: Value,
    preview: Value,
) -> std::io::Result<String> {
    use sha2::Digest;
    let now = now_f64();
    let mut h = sha2::Sha256::new();
    h.update(format!("{now}|{session}|{endpoint}|{payload}"));
    let id = format!("apr_{}", &hex::encode(h.finalize())[..16]);
    let doc = json!({
        "id": id,
        "created": now,
        "session": session,
        "endpoint": endpoint,
        "payload": payload,
        "preview": preview,
    });
    std::fs::write(
        approvals_dir(home).join(format!("{id}.json")),
        doc.to_string(),
    )?;
    Ok(id)
}

pub enum Consume {
    Ready(Value),
    Expired,
    /// Already consumed, or never existed — indistinguishable on purpose
    /// (probing which ids ever existed reveals nothing).
    Gone,
}

/// Read a pending approval WITHOUT consuming it (AUTOD-48 follow-up).
///
/// `consume` is a one-shot atomic rename, so any check that has to read the
/// frozen doc first DESTROYS the draft it is checking. The creator-cannot-approve
/// gate did exactly that: it consumed to learn the creator, then refused, and the
/// human's draft was gone.
///
/// autodesk bisected it, one probe per fresh approval, checking the pending list
/// before and after:
///
/// ```text
/// no-headers    -> 403 approver_unidentified   pending before=True  after=True
/// self-named    -> 403 creator_cannot_approve  pending before=True  after=FALSE
/// worker-header -> 403 worker_cannot_approve   pending before=True  after=True
/// ```
///
/// Only the creator check consumed. And because `GET /api/email/approvals`
/// returns each pending id AND its creating session to any caller, ANY lane
/// could destroy ANY pending draft by naming that creator back at the server.
///
/// The legitimate failure mode is worse than the abuse: a lane that self-approves
/// ONCE — the precise mistake the gate exists to catch — permanently destroyed
/// the message a human was supposed to review.
///
/// So: peek for the checks, consume only on the path that actually sends.
pub fn peek(home: &Path, id: &str) -> Option<Value> {
    if !valid_id(id) {
        return None;
    }
    let raw = std::fs::read_to_string(approvals_dir(home).join(format!("{id}.json"))).ok()?;
    serde_json::from_str::<Value>(&raw).ok()
}

/// One-shot consume: the rename is the atomicity — of two racing approvals,
/// exactly one rename succeeds and the loser sees Gone. Consumed and expired
/// files are KEPT (renamed, not deleted) so "what happened to apr_X" stays
/// answerable from the directory alone.
pub fn consume(home: &Path, id: &str) -> Consume {
    if !valid_id(id) {
        return Consume::Gone;
    }
    let dir = approvals_dir(home);
    let live = dir.join(format!("{id}.json"));
    let Ok(raw) = std::fs::read_to_string(&live) else {
        return Consume::Gone;
    };
    let Ok(doc) = serde_json::from_str::<Value>(&raw) else {
        return Consume::Gone;
    };
    if expired(&doc, now_f64()) {
        let _ = std::fs::rename(&live, dir.join(format!("{id}.expired.json")));
        return Consume::Expired;
    }
    if std::fs::rename(&live, dir.join(format!("{id}.approved.json"))).is_err() {
        return Consume::Gone; // raced: the other approver's rename won
    }
    Consume::Ready(doc)
}

/// A consumed approval whose send did not happen goes back to pending, with
/// the failure recorded on it, so the approver can release it again.
///
/// `consume` renames to `.approved.json` BEFORE the send is tried, and the
/// handler can still fail after it (an attachment that will not parse, an
/// unknown endpoint, Gmail refusing). Left `.approved.json`, `fate` called the
/// draft RELEASED and a retry with the `approval_id` the error handed back got
/// Gone: an unsent email read as sent (implicit state machines, 2026-11-06).
/// Returns false when there was no consumed file to reopen.
pub fn reopen_after_failed_send(home: &Path, id: &str, error: &str) -> bool {
    if !valid_id(id) {
        return false;
    }
    let dir = approvals_dir(home);
    let consumed = dir.join(format!("{id}.approved.json"));
    let Ok(raw) = std::fs::read_to_string(&consumed) else {
        return false;
    };
    let Ok(mut doc) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    if let Some(obj) = doc.as_object_mut() {
        // A scheduled release that failed waits for a human again: left
        // scheduled, the job would retry it every tick.
        // The approver's edit becomes the draft, so Send now sends their words
        // rather than the original (land review, 2026-11-08).
        if let Some(mut sched) = obj.remove("scheduled") {
            if let Some(Value::String(b)) = sched.as_object_mut().and_then(|m| m.remove("body")) {
                if let Some(payload) = obj.get_mut("payload").and_then(Value::as_object_mut) {
                    payload.insert("body".into(), json!(b));
                }
                sched["edit_kept"] = json!(true);
            }
            obj.insert("schedule_failed".into(), sched);
        }
        let failures = obj.entry("failed_releases").or_insert_with(|| json!([]));
        if let Some(list) = failures.as_array_mut() {
            list.push(json!({ "at": now_f64(), "error": error.chars().take(400).collect::<String>() }));
        }
    }
    // Write the pending file whole, then drop the consumed one: a crash between
    // the two leaves both, and `consume` reads the pending one first.
    let tmp = dir.join(format!(".{id}.reopen.tmp"));
    if std::fs::write(&tmp, doc.to_string()).is_err()
        || std::fs::rename(&tmp, dir.join(format!("{id}.json"))).is_err()
    {
        tracing::warn!(approval = %id, measured = false, verdict = "email_approval_reopen_failed",
            "[email] a failed release could not be put back to pending; it reads as released");
        return false;
    }
    if let Err(e) = std::fs::remove_file(&consumed) {
        tracing::warn!(approval = %id, error = %e, "[email] reopened approval kept its consumed file");
    }
    tracing::warn!(approval = %id, measured = true, n_considered = 1,
        verdict = "email_approval_reopened_after_failed_send",
        "[email] the release failed before anything was sent; the approval is pending again");
    true
}

/// What actually became of an approval id, read off the directory.
///
/// `consume` renames rather than deletes precisely so "what happened to apr_X"
/// stays answerable, and nothing was reading it — the 404 enumerated three
/// possibilities instead, one of which (`already approved`) asserts a human
/// released the draft. An auditor reading that on a draft destroyed by a refused
/// self-approval would have drawn exactly the wrong conclusion.
///
/// Says only what the filesystem shows. An id with no file of any kind gets the
/// non-committal answer, because that genuinely is indistinguishable from one
/// that never existed.
pub fn fate(home: &Path, id: &str) -> String {
    if !valid_id(id) {
        return "not a valid approval id".into();
    }
    let dir = approvals_dir(home);
    if dir.join(format!("{id}.approved.json")).exists() {
        return "this approval was already RELEASED — it is not pending. \
                GET /api/email/log shows who released it and when."
            .into();
    }
    // NAME THE REJECTER, NEVER INFER ONE (autodesk, third instance of this
    // defect in one day). This said "a human rejected it" — while the ledger row
    // for a headerless discard correctly reads `rejected_by: "unidentified"`.
    // So the MESSAGE asserted what the ROW refuses to assert, and pointed the
    // reader at that row as proof. An operator reads the message, not the row.
    //
    // Same family as `approved_by: "dashboard (no worker origin)"` and the
    // three-way 404, both fixed hours earlier. I wrote this one after fixing
    // those, which is the part worth remembering: knowing the rule did not stop
    // me applying its inverse in the next sentence I typed.
    let rejected = dir.join(format!("{id}.rejected.json"));
    if rejected.exists() {
        let by = std::fs::read_to_string(&rejected)
            .ok()
            .and_then(|r| serde_json::from_str::<Value>(&r).ok())
            .and_then(|d| {
                d.get("rejected_by")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default();
        return match by.as_str() {
            "" | "unidentified" => "this approval was DISCARDED and never sent. The caller did \
                                    not identify itself, so who discarded it is not known — \
                                    GET /api/email/log has the row."
                .into(),
            who => format!(
                "this approval was DISCARDED by '{who}' and never sent (the identity is \
                 CLAIMED, not verified). GET /api/email/log has the row."
            ),
        };
    }
    if dir.join(format!("{id}.expired.json")).exists() {
        return "this approval EXPIRED unreleased under the old 1h TTL (email approvals no longer \
                expire, AMUX-5240); the worker must request the send again"
            .into();
    }
    "no approval with that id is pending, and the directory holds no record of one".into()
}

/// Discard a pending approval without sending it (AMUX-3698).
///
/// The same one-shot rename `consume` uses, to `.rejected.json`. Renamed rather
/// than deleted, like every other terminal state here, so "what happened to
/// apr_X" stays answerable from the directory alone — and so `fate()` can tell a
/// later caller it was discarded rather than guessing.
pub fn discard(home: &Path, id: &str, by: &str) -> Option<Value> {
    if !valid_id(id) {
        return None;
    }
    let dir = approvals_dir(home);
    let live = dir.join(format!("{id}.json"));
    let mut doc = serde_json::from_str::<Value>(&std::fs::read_to_string(&live).ok()?).ok()?;
    // WHO, RECORDED IN THE DOC ITSELF (autodesk, third instance). `fate()` reads
    // the directory and nothing else, so without this it could only GUESS who
    // discarded a draft — and it guessed "a human", which is exactly the
    // assertion this endpoint spent the morning learning not to make.
    if let Some(o) = doc.as_object_mut() {
        o.insert("rejected_by".into(), Value::String(by.to_string()));
        o.insert("rejected_at".into(), json!(now_f64()));
    }
    let dest = dir.join(format!("{id}.rejected.json"));
    // Write the enriched doc to the destination, then remove the live file. Not
    // a rename, because the rename would carry the ORIGINAL bytes and lose the
    // attribution just added. Ordering matters: the destination exists before
    // the source goes, so a crash between them leaves a discarded record rather
    // than a vanished draft.
    std::fs::write(&dest, serde_json::to_string(&doc).ok()?).ok()?;
    std::fs::remove_file(&live).ok()?;
    Some(doc)
}

/// The latest send time a schedule may name: a year out. Further than that is
/// a typo, not a plan.
pub const SCHEDULE_MAX_AHEAD_S: f64 = 366.0 * 86400.0;

/// Schedule a pending approval to send at `send_at` (unix seconds) instead of
/// now (Ethan, 2026-11-08: "when i approve outbound email there should be
/// another button to schedule for a date/time").
///
/// The human's decision is recorded on the pending doc as `scheduled`
/// (`send_at`, the approver, when, and the edited body if any) and the draft
/// STAYS pending: the release itself is the same one-shot `consume` that
/// Approve uses, run by the scheduled-send job when the time comes, so a
/// scheduled email can never go out twice. Approve still sends it at once,
/// Discard still drops it, and a second schedule replaces the first.
/// The doc is rewritten whole through a temp file and a rename, so a crash
/// leaves either the old doc or the new one.
pub fn schedule(
    home: &Path,
    id: &str,
    send_at: f64,
    approver: &str,
    body: Option<&str>,
) -> Result<Value, String> {
    let now = now_f64();
    if !send_at.is_finite() || send_at <= now {
        return Err("send_at must be in the future".into());
    }
    if send_at > now + SCHEDULE_MAX_AHEAD_S {
        return Err("send_at is more than a year away".into());
    }
    if !valid_id(id) {
        return Err(fate(home, id));
    }
    // CLAIM THE DRAFT BY RENAMING IT, the same one-shot primitive `consume`
    // uses. A read-then-replace let a release that consumed the file in
    // between be followed by this rename, which put the sent draft back as
    // pending and the job sent it a second time (land review, 2026-11-08).
    // While the claim is held, a racing Approve or Discard finds no live file
    // and loses cleanly; a claim left by a crash is put back by
    // `recover_schedule_claims`.
    let dir = approvals_dir(home);
    let live = dir.join(format!("{id}.json"));
    let claim = dir.join(format!("{id}.scheduling.json"));
    if std::fs::rename(&live, &claim).is_err() {
        return Err(fate(home, id));
    }
    let restore = |e: String| {
        fs_logged("restore_schedule_claim", id, std::fs::rename(&claim, &live));
        e
    };
    let raw = std::fs::read_to_string(&claim).map_err(|e| restore(e.to_string()))?;
    let mut doc: Value = serde_json::from_str(&raw).map_err(|e| restore(e.to_string()))?;
    let mut sched = json!({ "send_at": send_at, "approver": approver, "at": now });
    if let Some(b) = body {
        sched["body"] = json!(b);
    }
    if let Some(o) = doc.as_object_mut() {
        o.insert("scheduled".into(), sched);
    }
    let tmp = dir.join(format!(".{id}.schedule.tmp"));
    if let Err(e) = std::fs::write(&tmp, doc.to_string()).and_then(|_| std::fs::rename(&tmp, &live)) {
        fs_logged("remove_schedule_tmp", id, std::fs::remove_file(&tmp));
        return Err(restore(e.to_string()));
    }
    fs_logged("remove_schedule_claim", id, std::fs::remove_file(&claim));
    tracing::info!(approval = %id, send_at, measured = true, n_considered = 1,
        verdict = "email_approval_scheduled", "[email] a held send was scheduled by its approver");
    Ok(doc)
}

/// A file step of scheduling that failed, logged rather than dropped: the
/// caller can carry on, but a stuck claim or temp file must be findable.
fn fs_logged(op: &'static str, id: &str, r: std::io::Result<()>) {
    if let Err(e) = r {
        tracing::warn!(approval = %id, op, error = %e, measured = true, n_considered = 1,
            verdict = "email_schedule_file_step_failed", "[email] a scheduling file step failed");
    }
}

/// Put back a draft whose schedule claim a crash left behind: the claim holds
/// the pending doc, and with no live file the draft would vanish from the
/// queue. A claim beside a live file is the leftover of a finished schedule.
pub fn recover_schedule_claims(home: &Path) {
    let dir = approvals_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_suffix(".scheduling.json") else { continue };
        if !valid_id(id) {
            continue;
        }
        let live = dir.join(format!("{id}.json"));
        if live.exists() {
            fs_logged("remove_stale_schedule_claim", id, std::fs::remove_file(e.path()));
        } else if std::fs::rename(e.path(), &live).is_ok() {
            tracing::warn!(approval = %id, measured = true, n_considered = 1,
                verdict = "email_schedule_claim_recovered",
                "[email] a schedule interrupted mid-write was put back to pending");
        }
    }
}

/// Pending approvals whose scheduled time has come, with the approver and the
/// edited body the human scheduled them with.
pub fn due_scheduled(home: &Path, now: f64) -> Vec<(String, String, Option<String>)> {
    let mut out = Vec::new();
    recover_schedule_claims(home);
    let Ok(entries) = std::fs::read_dir(approvals_dir(home)) else { return out };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_suffix(".json") else { continue };
        if !valid_id(id) {
            continue;
        }
        let Some(doc) = peek(home, id) else { continue };
        let Some(s) = doc.get("scheduled") else { continue };
        if s.get("send_at").and_then(Value::as_f64).is_some_and(|t| t <= now) {
            out.push((
                id.to_string(),
                s.get("approver").and_then(Value::as_str).unwrap_or("").to_string(),
                s.get("body").and_then(Value::as_str).map(str::to_string),
            ));
        }
    }
    out.sort();
    out
}

/// Pending approvals, oldest first, for the dashboard / a human's curl.
pub fn list_pending(home: &Path) -> Vec<Value> {
    recover_schedule_claims(home);
    let dir = approvals_dir(home);
    let mut out: Vec<Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            if !valid_id(id) {
                continue; // .approved.json / .expired.json land here too
            }
            let Ok(raw) = std::fs::read_to_string(e.path()) else {
                continue;
            };
            let Ok(mut doc) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            let created = doc.get("created").and_then(Value::as_f64).unwrap_or(0.0);
            let age = (now_f64() - created).max(0.0);
            if expired(&doc, now_f64()) {
                let _ = std::fs::rename(e.path(), dir.join(format!("{id}.expired.json")));
                continue;
            }
            let keeps = never_expires(&doc);
            let (requests, last_requested) = read_requests(home, id);
            if let Some(obj) = doc.as_object_mut() {
                obj.insert("age_s".into(), json!(age as i64));
                // null, not a number, for a kind that never expires: a
                // countdown that never reaches zero is a lie on the banner.
                obj.insert(
                    "expires_in_s".into(),
                    if keeps { Value::Null } else { json!((APPROVAL_TTL_S - age) as i64) },
                );
                obj.insert("expires".into(), json!(!keeps));
                obj.insert("requests".into(), json!(requests));
                if last_requested > 0.0 {
                    obj.insert("last_requested".into(), json!(last_requested));
                }
                // Business review must distinguish a truncated summary from the
                // frozen message. Never expose raw attachment data or file paths.
                // Existing dashboard previews retain their compatible shape.
                if let Some(payload) = obj.get("payload").cloned() {
                    let body = payload.get("body").and_then(Value::as_str).unwrap_or("");
                    let attachment_count = payload
                        .get("attachments")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    let complete =
                        !body.is_empty() && body.len() <= 100_000 && attachment_count == 0;
                    obj.insert(
                        "review".into(),
                        json!({
                            "body": if body.len() <= 100_000 { Some(body) } else { None },
                            "body_complete": !body.is_empty() && body.len() <= 100_000,
                            "attachment_count": attachment_count,
                            "includes_signature": payload.get("signature")
                                .and_then(Value::as_bool).unwrap_or(true),
                            "complete": complete,
                        }),
                    );
                    if !complete {
                        tracing::debug!(approval_id = %id, attachment_count,
                            body_bytes = body.len(), "email_approval_review_incomplete");
                    }
                }
                obj.remove("payload");
                // A scheduled draft shows the words that will go out: the
                // approver's edit when they made one. The edit itself stays
                // out of `scheduled`, which says only whether there was one.
                for key in ["scheduled", "schedule_failed"] {
                    let Some(sched) = obj.get_mut(key).and_then(Value::as_object_mut) else { continue };
                    let edit = sched.remove("body");
                    sched.insert("edited".into(), json!(edit.is_some()));
                    if let (Some(Value::String(b)), true) = (edit, key == "scheduled") {
                        if let Some(review) = obj.get_mut("review").and_then(Value::as_object_mut) {
                            review.insert("body".into(), json!(b));
                        }
                    }
                }
            }
            out.push(doc);
        }
    }
    out.sort_by(|a, b| {
        let ka = a.get("created").and_then(Value::as_f64).unwrap_or(0.0);
        let kb = b.get("created").and_then(Value::as_f64).unwrap_or(0.0);
        ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A schedule never resurrects a released draft, and a schedule a crash
    /// interrupted is put back to pending (land review, 2026-11-08).
    #[test]
    fn schedule_claims_the_draft_and_a_crashed_claim_is_recovered() {
        let home = tempfile::tempdir().unwrap();
        let later = now_f64() + 600.0;
        let id = create_approval(home.path(), "lane", "send", json!({ "body": "b" }), json!({})).unwrap();
        assert!(matches!(consume(home.path(), &id), Consume::Ready(_)));
        assert!(schedule(home.path(), &id, later, "dashboard", None).is_err());
        assert!(!approvals_dir(home.path()).join(format!("{id}.json")).exists(), "the released draft stays released");

        let id2 = create_approval(home.path(), "lane", "send", json!({ "body": "b2" }), json!({})).unwrap();
        let dir = approvals_dir(home.path());
        std::fs::rename(dir.join(format!("{id2}.json")), dir.join(format!("{id2}.scheduling.json"))).unwrap();
        assert!(list_pending(home.path()).iter().any(|d| d["id"] == json!(id2)), "a crashed claim is put back");
        assert!(schedule(home.path(), &id2, later, "dashboard", None).is_ok());
        assert!(!dir.join(format!("{id2}.scheduling.json")).exists());
        assert_eq!(due_scheduled(home.path(), later + 1.0).len(), 1);
    }

    #[test]
    fn pending_business_review_uses_full_body_and_announces_unreviewed_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let body = "A".repeat(3000);
        let id = create_approval(
            dir.path(),
            "business-test",
            "send",
            json!({"body": body, "attachments": [], "signature": false}),
            json!({"body": "short preview", "to": "recipient@example.test"}),
        )
        .unwrap();
        let rows = list_pending(dir.path());
        let row = rows.iter().find(|r| r["id"] == id).unwrap();
        assert_eq!(row["review"]["body"], body);
        assert_eq!(row["review"]["complete"], true);
        assert_eq!(row["review"]["includes_signature"], false);
        assert!(row.get("payload").is_none());
        create_approval(dir.path(), "business-test", "send",
            json!({"body": "Attached", "attachments": [{"filename":"evidence.pdf", "data":"private"}]}),
            json!({"body": "Attached"})).unwrap();
        let rows = list_pending(dir.path());
        let attached = rows.iter().find(|r| r["id"] != id).unwrap();
        assert_eq!(attached["review"]["complete"], false);
        assert_eq!(attached["review"]["attachment_count"], 1);
        assert!(!attached.to_string().contains("private"));
    }

    #[test]
    fn classifier_external_is_external_and_every_internal_shape_is_not() {
        let internal: Vec<String> = internal_domains(Path::new("/nonexistent-home"));
        let connected = vec![
            "ethan@mixpeek.com".to_string(),
            "esteininger21@gmail.com".to_string(),
        ];
        // The incident's own recipients: all external.
        let ext = external_recipients(
            "hilmar.koch@autodesk.com, jonathan.brooks@autodesk.com",
            &internal,
            &connected,
        );
        assert_eq!(ext.len(), 2, "{ext:?}");
        // Internal domain, any case; a connected account's own address
        // (self-send, gmail.com!); empty and junk entries.
        assert!(external_recipients("Ops@Mixpeek.com", &internal, &connected).is_empty());
        assert!(external_recipients("esteininger21@gmail.com", &internal, &connected).is_empty());
        assert!(external_recipients(" , not-an-address ,", &internal, &connected).is_empty());
        // Mixed list: exactly the external one survives.
        let mixed =
            external_recipients("ethan@mixpeek.com, ceo@customer.com", &internal, &connected);
        assert_eq!(mixed, vec!["ceo@customer.com".to_string()]);
        // A DIFFERENT gmail address is NOT internal merely because a
        // connected account is on gmail — the exemption is the exact
        // address, never its domain.
        assert_eq!(
            external_recipients("someone-else@gmail.com", &internal, &connected).len(),
            1
        );
    }

    #[test]
    fn external_email_authorization_is_scoped_and_defaults_closed() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        std::fs::create_dir_all(home.join("env")).unwrap();
        std::fs::write(home.join("sessions/probe.env"), "CC_TAGS=gtm\n").unwrap();

        assert!(!external_email_allowed(home, "probe"), "closed by default");

        // Global allow is visible until a more-specific worker denial wins.
        std::fs::write(home.join("amux.env"), "AMUX_EMAIL_EXTERNAL_ALLOW=1\n").unwrap();
        assert!(external_email_allowed(home, "probe"));
        std::fs::write(
            home.join("sessions/probe.env"),
            "CC_TAGS=gtm\nAMUX_EMAIL_EXTERNAL_ALLOW=0\n",
        )
        .unwrap();
        assert!(!external_email_allowed(home, "probe"));

        // Removing the worker override exposes the group layer.
        std::fs::write(home.join("amux.env"), "AMUX_EMAIL_EXTERNAL_ALLOW=0\n").unwrap();
        std::fs::write(home.join("env/gtm.env"), "AMUX_EMAIL_EXTERNAL_ALLOW=yes\n").unwrap();
        std::fs::write(home.join("sessions/probe.env"), "CC_TAGS=gtm\n").unwrap();
        assert!(external_email_allowed(home, "probe"));

        // Compatibility for the old global comma-list remains, but the new
        // scoped key is authoritative when present.
        std::fs::remove_file(home.join("env/gtm.env")).unwrap();
        std::fs::write(home.join("amux.env"), "").unwrap();
        std::fs::write(
            home.join("server.env"),
            "AMUX_EMAIL_EXTERNAL_EXEMPT=Probe,other\n",
        )
        .unwrap();
        assert!(external_email_allowed(home, "probe"));
        std::fs::write(
            home.join("sessions/probe.env"),
            "CC_TAGS=gtm\nAMUX_EMAIL_EXTERNAL_ALLOW=0\n",
        )
        .unwrap();
        assert!(!external_email_allowed(home, "probe"));
    }

    #[test]
    fn consume_is_one_shot_expiring_and_traversal_safe() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let id = create_approval(
            home,
            "autodesk",
            "reply",
            json!({"message_id": "<x@y>", "body": "hi"}),
            json!({"to": "a@ext.com"}),
        )
        .unwrap();
        assert!(valid_id(&id));
        assert_eq!(list_pending(home).len(), 1);
        // First consume wins…
        assert!(matches!(consume(home, &id), Consume::Ready(_)));
        // …the second sees Gone (the file was renamed, not deleted).
        assert!(matches!(consume(home, &id), Consume::Gone));
        assert!(home
            .join("email-approvals")
            .join(format!("{id}.approved.json"))
            .exists());
        // A release whose send failed goes back to pending with the failure on
        // it, and the next approval consumes it again (2026-11-06).
        assert!(reopen_after_failed_send(home, &id, "gmail said 400"));
        assert!(!fate(home, &id).contains("RELEASED"), "{}", fate(home, &id));
        match consume(home, &id) {
            Consume::Ready(doc) => assert_eq!(doc["failed_releases"][0]["error"], "gmail said 400"),
            _ => panic!("a reopened approval is consumable again"),
        }
        assert!(!reopen_after_failed_send(home, "apr_..", "x"));
        assert!(list_pending(home).is_empty());
        // Expiry still applies to a NON-email kind: backdate past the TTL.
        let id2 = create_approval(home, "s", "grant", json!({}), json!({})).unwrap();
        let p = home.join("email-approvals").join(format!("{id2}.json"));
        let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        doc["created"] = json!(now_f64() - APPROVAL_TTL_S - 5.0);
        std::fs::write(&p, doc.to_string()).unwrap();
        assert!(matches!(consume(home, &id2), Consume::Expired));
        // AMUX-5240: an outbound email approval aged far past the old TTL is
        // still releasable. This is the SCHED-498 shape: parked at one fire,
        // read by the owner hours later.
        let id3 = create_approval(home, "gtm-ticker", "send", json!({"body": "x"}), json!({})).unwrap();
        let p = home.join("email-approvals").join(format!("{id3}.json"));
        let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        doc["created"] = json!(now_f64() - 30.0 * 3600.0);
        std::fs::write(&p, doc.to_string()).unwrap();
        let pending = list_pending(home);
        assert_eq!(pending.len(), 1, "a day-old email draft stays pending");
        assert_eq!(pending[0]["expires_in_s"], Value::Null);
        assert_eq!(pending[0]["expires"], json!(false));
        assert!(matches!(consume(home, &id3), Consume::Ready(_)));
        // Traversal shapes never reach the filesystem.
        for bad in [
            "../../etc/passwd",
            "apr_..",
            "apr_ZZZZZZZZZZZZZZZZ",
            "",
            "apr_short",
        ] {
            assert!(!valid_id(bad), "{bad}");
            assert!(matches!(consume(home, bad), Consume::Gone));
        }
    }

    /// AMUX-5240: SCHED-498 re-requests the same reply on every fire. The
    /// second request must return the FIRST id, count it, and keep the frozen
    /// draft; a different recipient, subject or lane is a different draft.
    #[test]
    fn a_re_request_of_the_same_draft_reuses_the_pending_approval() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let preview = |to: &str, subject: &str| json!({"endpoint": "reply", "to": to, "cc": "", "subject": subject, "in_reply_to": "<a@b>"});
        let payload = |body: &str| json!({"message_id": "<a@b>", "body": body});
        let first = request_approval(home, "gtm-ticker", "reply", payload("hi Rami"), preview("Rami@Ext.com", "Pilot")).unwrap();
        assert!(!first.reused);
        let again = request_approval(home, "gtm-ticker", "reply", payload("hi Rami"), preview("rami@ext.com", "Re: RE: pilot")).unwrap();
        assert_eq!(again.id, first.id, "same lane + recipient + subject is one draft");
        assert!(again.reused && !again.draft_changed);
        assert_eq!(again.requests, 2);
        let edited = request_approval(home, "gtm-ticker", "reply", payload("hi Rami, v2"), preview("rami@ext.com", "Pilot")).unwrap();
        assert_eq!(edited.id, first.id);
        assert!(edited.draft_changed, "the caller is told its new body is not what is frozen");
        let pending = list_pending(home);
        assert_eq!(pending.len(), 1, "no duplicate in the owner's queue");
        assert_eq!(pending[0]["requests"], json!(3));
        assert_eq!(pending[0].pointer("/review/body"), Some(&json!("hi Rami")), "frozen draft unchanged");
        // Different recipient, different subject, different lane: new drafts.
        for (lane, to, subj) in [("gtm-ticker", "abraham@ext.com", "Pilot"), ("gtm-ticker", "rami@ext.com", "Invoice"), ("refresh-house", "rami@ext.com", "Pilot")] {
            let r = request_approval(home, lane, "reply", payload("x"), preview(to, subj)).unwrap();
            assert!(!r.reused, "{lane} {to} {subj}");
        }
        assert_eq!(list_pending(home).len(), 4);
        // Once released, a new request is a new approval (the old one is gone).
        assert!(matches!(consume(home, &first.id), Consume::Ready(_)));
        let after = request_approval(home, "gtm-ticker", "reply", payload("hi Rami"), preview("rami@ext.com", "Pilot")).unwrap();
        assert!(!after.reused && after.id != first.id);
        // No recipients: never deduped.
        assert_eq!(draft_key("s", &json!({}), &json!({})), None);
    }
}
