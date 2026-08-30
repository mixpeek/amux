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
/// "an hour seems right").
pub const APPROVAL_TTL_S: f64 = 3600.0;

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
        && id[4..].chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
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
    std::fs::write(approvals_dir(home).join(format!("{id}.json")), doc.to_string())?;
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
    let Ok(raw) = std::fs::read_to_string(&live) else { return Consume::Gone };
    let Ok(doc) = serde_json::from_str::<Value>(&raw) else { return Consume::Gone };
    let created = doc.get("created").and_then(Value::as_f64).unwrap_or(0.0);
    if now_f64() - created > APPROVAL_TTL_S {
        let _ = std::fs::rename(&live, dir.join(format!("{id}.expired.json")));
        return Consume::Expired;
    }
    if std::fs::rename(&live, dir.join(format!("{id}.approved.json"))).is_err() {
        return Consume::Gone; // raced: the other approver's rename won
    }
    Consume::Ready(doc)
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
            .and_then(|d| d.get("rejected_by").and_then(Value::as_str).map(str::to_string))
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
        return "this approval EXPIRED unreleased (1h TTL) — the worker must request the send again"
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

/// Pending approvals, oldest first, for the dashboard / a human's curl.
pub fn list_pending(home: &Path) -> Vec<Value> {
    let dir = approvals_dir(home);
    let mut out: Vec<Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".json") else { continue };
            if !valid_id(id) {
                continue; // .approved.json / .expired.json land here too
            }
            let Ok(raw) = std::fs::read_to_string(e.path()) else { continue };
            let Ok(mut doc) = serde_json::from_str::<Value>(&raw) else { continue };
            let created = doc.get("created").and_then(Value::as_f64).unwrap_or(0.0);
            let age = (now_f64() - created).max(0.0);
            if age > APPROVAL_TTL_S {
                let _ = std::fs::rename(e.path(), dir.join(format!("{id}.expired.json")));
                continue;
            }
            if let Some(obj) = doc.as_object_mut() {
                obj.insert("age_s".into(), json!(age as i64));
                obj.insert("expires_in_s".into(), json!((APPROVAL_TTL_S - age) as i64));
                // The frozen payload stays server-side until approval; the
                // list serves the preview, which already carries to/cc/
                // subject/body for the human's decision.
                obj.remove("payload");
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

    #[test]
    fn classifier_external_is_external_and_every_internal_shape_is_not() {
        let internal: Vec<String> = internal_domains(Path::new("/nonexistent-home"));
        let connected = vec!["ethan@mixpeek.com".to_string(), "esteininger21@gmail.com".to_string()];
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
        let mixed = external_recipients(
            "ethan@mixpeek.com, ceo@customer.com",
            &internal,
            &connected,
        );
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
        assert!(home.join("email-approvals").join(format!("{id}.approved.json")).exists());
        assert!(list_pending(home).is_empty());
        // Expiry: backdate a fresh approval past the TTL.
        let id2 = create_approval(home, "s", "send", json!({}), json!({})).unwrap();
        let p = home.join("email-approvals").join(format!("{id2}.json"));
        let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        doc["created"] = json!(now_f64() - APPROVAL_TTL_S - 5.0);
        std::fs::write(&p, doc.to_string()).unwrap();
        assert!(matches!(consume(home, &id2), Consume::Expired));
        // Traversal shapes never reach the filesystem.
        for bad in ["../../etc/passwd", "apr_..", "apr_ZZZZZZZZZZZZZZZZ", "", "apr_short"] {
            assert!(!valid_id(bad), "{bad}");
            assert!(matches!(consume(home, bad), Consume::Gone));
        }
    }
}
