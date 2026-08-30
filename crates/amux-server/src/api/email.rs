//! Email API (RR-0088): `/api/email/send|reply|inbox|search|log`, request
//! and response field names IDENTICAL to the Python handlers (the CLAUDE.md
//! email contract: to/subject/body/from/cc; message_id for replies;
//! X-Amux-Session attribution recorded in the send-audit ledger,
//! AMUX-1897).
//!
//! Scope note (honest deviation, named): the Python handlers fall back to
//! Mail.app/AppleScript for accounts without a Gmail token. That path is
//! deliberately NOT ported — this server answers 501 with the way out
//! (connect the account) instead of silently doing nothing. Everything on
//! the Gmail-API path is ported: validation messages, the new-thread guard
//! (AMUX-1739), signature handling, threading proof fields, the send-audit
//! ledger, and the AMUX-1886 inbox window semantics.
//!
//! Every real Gmail outcome feeds the IntegrationRegistry (RR-0073): a
//! successful call proves `email: available`; an auth-shaped failure
//! (invalid_grant / not_connected) proves `unavailable`; anything else is
//! `degraded`. That is the "state from real outcomes, not token files"
//! contract.

use super::AppState;
use crate::integrations::email::{
    email_log, read_email_log, Attachment, GmailClient, OUR_DOMAINS,
};
use base64::Engine as _;
use crate::integrations::{self, IntegrationRegistry, IntegrationState};
use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// Handler context: the Gmail client (mockable transport) + the registry
/// its outcomes feed.
pub struct EmailCtx {
    pub client: Arc<GmailClient>,
    pub registry: Arc<IntegrationRegistry>,
}

pub fn routes() -> Router<AppState> {
    routes_with(Arc::new(EmailCtx {
        client: Arc::new(GmailClient::new_default()),
        registry: integrations::global_registry().clone(),
    }))
}

pub fn routes_with(ctx: Arc<EmailCtx>) -> Router<AppState> {
    Router::new()
        .route("/send", post(send))
        .route("/reply", post(reply))
        .route("/inbox", get(inbox))
        .route("/message/{id}", get(message))
        .route("/message/{id}/attachments/{attachment_id}", get(message_attachment))
        .route("/search", get(search))
        .route("/log", get(send_log))
        // AMUX-3510: the human approval half of the external-send gate.
        .route("/approve/{id}", post(approve))
        .route("/reject/{id}", post(reject))
        .route("/approvals", get(list_approvals))
        .layer(Extension(ctx))
}

// ---- shared helpers -------------------------------------------------------

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

/// Python `_hdr_worker`: X-Amux-Worker is canonical, X-Amux-Session still
/// works. `None` (not "") when unattributed so the ledger's `session` field
/// round-trips as Python's `null`.
fn hdr_worker(headers: &HeaderMap) -> Option<String> {
    for name in ["x-amux-worker", "x-amux-session"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim) {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Which Gmail accounts this lane is scoped to, or `None` for "unrestricted"
/// (AMUX-3103).
///
/// Resolved through `scoped_setting_in`, so `AMUX_GMAIL_ACCOUNTS` obeys the same
/// global -> group -> worker precedence as every other scoped setting: set it once
/// on the `mixpeek` group, override it on one worker, and the worker wins. That
/// is the whole connector-scoping mechanism — a connector is a composition of
/// env + scope, not a new subsystem (docs/design/connectors.md).
///
/// ABSENT MEANS UNRESTRICTED, deliberately. A connector that denied by default
/// would break every lane that sends mail today the moment this shipped, and a
/// guard that has to be rolled out atomically with its policy is a guard people
/// disable. Presence of configuration is the switch, per the single-codebase
/// rule — no build flag, no IS_SCOPED branch.
fn gmail_scope_allowed(home: &std::path::Path, lane: &str) -> Option<Vec<String>> {
    let raw = crate::api::session_verbs::scoped_setting_in(home, lane, "AMUX_GMAIL_ACCOUNTS")?;
    let list: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    (!list.is_empty()).then_some(list)
}

/// The UNHAPPY PATH Ethan named as the acceptance test: a lane asking to send as
/// an account it is not scoped to must be DENIED, and told what it may use.
///
/// Returns the refusal body, or `None` to allow. Also resolves the empty-`from`
/// case: an unscoped lane keeps the historical default, a scoped lane defaults to
/// its FIRST allowed account rather than to a global default it may not hold —
/// otherwise "scoped to the personal account" would still send as ethan@ whenever
/// `from` was omitted, which is the same bug wearing a default.
fn gmail_scope_check(home: &std::path::Path, lane: &str, from: &str) -> Result<Option<String>, Value> {
    let Some(allowed) = gmail_scope_allowed(home, lane) else {
        return Ok(None); // unrestricted: caller's `from` stands
    };
    if from.is_empty() {
        return Ok(Some(allowed[0].clone()));
    }
    if allowed.iter().any(|a| a.eq_ignore_ascii_case(from)) {
        return Ok(None);
    }
    Err(json!({
        "error": format!(
            "worker '{lane}' is not scoped to send as {from} — allowed: {}",
            allowed.join(", ")
        ),
        "blocked": "connector_scope",
        "connector": "gmail",
        "worker": lane,
        "requested": from,
        "allowed": allowed,
        "how_to_change": "set AMUX_GMAIL_ACCOUNTS at global/group/worker scope (worker wins)",
    }))
}

fn body_str(body: &Value, k: &str) -> String {
    body.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// A MIME content type from a filename extension; octet-stream when unknown.
fn guess_content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("").to_lowercase().as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "csv" => "text/csv",
        "txt" | "log" | "md" => "text/plain",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xls" => "application/vnd.ms-excel",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Parse the request body's optional `attachments` array (AMUX-3357). Each entry
/// is either `{"path": "..."}` (read from disk; filename is the basename,
/// content_type inferred from the extension unless given) or
/// `{"filename": "...", "content_base64": "...", "content_type": "..."}`. Caps
/// the total at Gmail's 25MB. Returns a 400-worthy message on any bad entry.
fn parse_attachments(body: &Value) -> Result<Vec<Attachment>, String> {
    let Some(v) = body.get("attachments") else { return Ok(vec![]) };
    if v.is_null() {
        return Ok(vec![]);
    }
    let arr = v.as_array().ok_or("attachments must be an array")?;
    const MAX_TOTAL: usize = 25 * 1024 * 1024;
    let mut out = Vec::new();
    let mut total = 0usize;
    for (i, a) in arr.iter().enumerate() {
        let (filename, ct_opt, data) = if let Some(path) = a.get("path").and_then(Value::as_str) {
            let p = std::path::Path::new(path);
            let data = std::fs::read(p).map_err(|e| format!("attachment {i}: read {path}: {e}"))?;
            let fname = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "attachment".into());
            (fname, a.get("content_type").and_then(Value::as_str).map(String::from), data)
        } else if let Some(b64) = a.get("content_base64").and_then(Value::as_str) {
            let data = base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .map_err(|e| format!("attachment {i}: invalid base64: {e}"))?;
            let fname =
                a.get("filename").and_then(Value::as_str).unwrap_or("attachment").to_string();
            (fname, a.get("content_type").and_then(Value::as_str).map(String::from), data)
        } else {
            return Err(format!("attachment {i}: needs a 'path' or 'content_base64'"));
        };
        total += data.len();
        if total > MAX_TOTAL {
            return Err(format!("attachments exceed {}MB (Gmail cap)", MAX_TOTAL / 1024 / 1024));
        }
        let content_type =
            ct_opt.unwrap_or_else(|| guess_content_type(&filename).to_string());
        out.push(Attachment { filename, content_type, data });
    }
    Ok(out)
}

/// Feed a real Gmail outcome into the registry (RR-0073). Auth-shaped
/// failures are `Unavailable` (re-auth needed / no token); transient ones
/// `Degraded`; success is the only path to `Available`.
fn report_outcome(reg: &IntegrationRegistry, result: &Result<(), String>) {
    match result {
        Ok(()) => reg.set("email", IntegrationState::Available),
        Err(e) if e.contains("invalid_grant") || e.contains("not_connected") => {
            reg.set("email", IntegrationState::Unavailable { reason: e.clone() })
        }
        Err(e) => reg.set("email", IntegrationState::Degraded { reason: e.clone() }),
    }
}

/// The Mail.app/AppleScript fallback is not ported (module docs): refuse
/// honestly with the way out, never pretend to send.
fn applescript_not_ported(account: &str) -> Response {
    err(
        StatusCode::NOT_IMPLEMENTED,
        json!({
            "error": format!(
                "'{account}' is not a connected Gmail account and the Mail.app/AppleScript \
                 fallback is not ported to this server — connect it \
                 (GET /api/gmail/auth?account={account}) or use a connected account"
            ),
            "connected_hint": "GET /api/gmail/accounts lists connected accounts",
        }),
    )
}

/// Refuse a SEND/REPLY whose from-address is not a connected Gmail account, AND
/// record the attempt where a human already looks (GE-621, prevention-a + its
/// observability half). Refusing was already correct — the Rust server never had
/// the silent Mail.app fallback that sent 3 replies from Ethan's personal account
/// off-ledger. But the refusal wrote NOTHING to /api/email/log or the server log,
/// so an off-account attempt left no trace: the API did the right thing invisibly.
/// Now every refused send lands in the audited log (`via: "refused"`, `refused:
/// true`) and a WARN, so a sweep of /api/email/log catches "someone keeps trying
/// to send from a non-connected account" — the early-warning signal for the whole
/// class (two-fixes rule). Read endpoints (inbox/search) keep the quiet refusal:
/// a read from an unconnected account is benign and frequent, not an incident.
fn refuse_send(ctx: &EmailCtx, headers: &HeaderMap, endpoint: &str, account: &str) -> Response {
    tracing::warn!(
        endpoint, from = %account,
        "email {endpoint} REFUSED: from-address is not a connected Gmail account \
         (no silent Mail.app fallback) — recorded to /api/email/log (GE-621)"
    );
    email_log(
        ctx.client.home(),
        json!({
            "endpoint": endpoint,
            "via": "refused",
            "refused": true,
            "from": account,
            "reason": "from-address is not a connected Gmail account; \
                       Mail.app/AppleScript fallback is not ported",
            "session": hdr_worker(headers),
        }),
    );
    applescript_not_ported(account)
}

const ADDR_RE: &str = r"^[^@\s]+@[^@\s]+\.[^@\s]+$";

fn bad_addrs(list: &str) -> Vec<String> {
    let re = regex::Regex::new(ADDR_RE).expect("static regex");
    list.split(',')
        .map(str::trim)
        .filter(|a| !a.is_empty() && !re.is_match(a))
        .map(String::from)
        .collect()
}

// ---- POST /api/email/send -------------------------------------------------

pub async fn send(
    Extension(ctx): Extension<Arc<EmailCtx>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    // Reject threading headers — Python's exact refusal (threaded replies
    // belong on /reply, where In-Reply-To/References are derived correctly).
    for forbidden_key in ["in_reply_to", "references", "inReplyTo"] {
        if body.get(forbidden_key).map(|v| !v.is_null() && v != &json!("")).unwrap_or(false) {
            return err(
                StatusCode::BAD_REQUEST,
                json!({
                    "error": format!(
                        "'{forbidden_key}' is not supported on /api/email/send — \
                         Mail.app cannot set custom headers on outgoing messages. \
                         Use POST /api/email/reply to reply in-thread."
                    )
                }),
            );
        }
    }
    let to = body_str(&body, "to");
    let subject = body_str(&body, "subject");
    let message = body_str(&body, "body");
    let cc = body_str(&body, "cc");
    let mut from_acct = body_str(&body, "from");
    // CONNECTOR SCOPE (AMUX-3103): enforced before anything is sent, and keyed on
    // the SERVER-VERIFIED caller header rather than anything in the body — a lane
    // must not be able to widen its own scope by claiming a different worker
    // (AMUX-1768 is the same principle for message provenance).
    if let Some(lane) = hdr_worker(&headers) {
        match gmail_scope_check(&crate::api::session_verbs::home(), &lane, &from_acct) {
            Err(denial) => return err(StatusCode::FORBIDDEN, denial),
            Ok(Some(defaulted)) => from_acct = defaulted,
            Ok(None) => {}
        }
    }
    if to.is_empty() || subject.is_empty() || message.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            json!({ "error": "to, subject, and body are required" }),
        );
    }
    // to/cc may be comma-separated lists; validate each part (Python
    // parity, including the error strings).
    let bad_to = bad_addrs(&to);
    if !bad_to.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            json!({ "error": format!("invalid email address: {}", bad_to.join(", ")) }),
        );
    }
    let bad_cc = bad_addrs(&cc);
    if !bad_cc.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            json!({ "error": format!("invalid cc address: {}", bad_cc.join(", ")) }),
        );
    }

    let connected = ctx.client.connected_accounts();

    // EXTERNAL-SEND APPROVAL GATE (AMUX-3510) — before the etiquette guards:
    // a worker-originated send reaching any non-internal recipient is frozen
    // for a human, not sent. See email_approval.rs for the incident and the
    // full contract; the refusal is itself a ledger row so a gated send
    // nobody approves still left a trace.
    if let Some(lane) = hdr_worker(&headers) {
        let home = ctx.client.home().to_path_buf();
        let externals = crate::api::email_approval::external_recipients(
            &format!("{to},{cc}"),
            &crate::api::email_approval::internal_domains(&home),
            &connected,
        );
        if !externals.is_empty()
            && !crate::api::email_approval::exempt_sessions(&home).contains(&lane.to_lowercase())
        {
            let preview = json!({
                "endpoint": "send", "from": from_acct, "to": to, "cc": cc,
                "subject": subject,
                "body": message.chars().take(2000).collect::<String>(),
                "external_recipients": externals,
            });
            let payload = json!({
                "from": from_acct, "to": to, "cc": cc, "subject": subject,
                "body": message,
                "signature": body.get("signature").cloned().unwrap_or(Value::Null),
                "attachments": body.get("attachments").cloned().unwrap_or(Value::Null),
            });
            return match crate::api::email_approval::create_approval(
                &home, &lane, "send", payload, preview.clone(),
            ) {
                Ok(id) => {
                    email_log(
                        ctx.client.home(),
                        json!({
                            "endpoint": "send", "blocked": "approval_required",
                            "approval_id": id, "from": from_acct, "to": to,
                            "subject": subject, "session": lane,
                        }),
                    );
                    tracing::warn!(
                        session = %lane, approval = %id,
                        "[email] EXTERNAL send from a worker held for human approval (AMUX-3510)"
                    );
                    approval_required_response(&id, preview)
                }
                Err(e) => err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({ "error": format!("could not record the approval request: {e} — NOT sent") }),
                ),
            };
        }
    }

    // NEW-THREAD GUARD (AMUX-1739): a /send to an EXTERNAL recipient with an
    // active thread fragments a customer conversation. Block with the
    // candidate so replying stays the DEFAULT; a new thread requires an
    // explicit force_new_thread. Own-domain/connected recipients exempt;
    // fails open on Gmail API errors (latest_matching -> None).
    if !body.get("force_new_thread").map(truthy).unwrap_or(false) {
        let conn_lc: Vec<String> = connected.iter().map(|a| a.to_lowercase()).collect();
        let ext_rcpts: Vec<String> = to
            .split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .filter(|a| {
                let al = a.to_lowercase();
                !conn_lc.contains(&al)
                    && !OUR_DOMAINS.iter().any(|d| al.ends_with(&format!("@{d}")))
            })
            .map(String::from)
            .collect();
        let win = body
            .get("thread_window_days")
            .and_then(Value::as_i64)
            .unwrap_or(14)
            .clamp(1, 90);
        for r in &ext_rcpts {
            let mut cand = None;
            for acct in &connected {
                cand = ctx.client.latest_matching(acct, "", r, "", win).await;
                if cand.is_some() {
                    break;
                }
            }
            if let Some(cand) = cand {
                tracing::info!(recipient = %r, "new-thread guard: blocked /send (active thread)");
                return err(
                    StatusCode::CONFLICT,
                    json!({
                        "error": format!(
                            "{r} has an active thread in the last {win} days — \
                             replying in-thread is the default; opening a new thread \
                             must be explicit"
                        ),
                        "blocked": true,
                        "recipient": r,
                        "candidate_thread": cand,
                        "reply_instead": {
                            "endpoint": "POST /api/email/reply",
                            "body": {
                                "message_id": cand.get("message_id").cloned().unwrap_or(json!("")),
                                "body": "...",
                                "from": if from_acct.is_empty() { "ethan@mixpeek.com".to_string() } else { from_acct.clone() },
                            },
                        },
                        "or_force": {
                            "force_new_thread": true,
                            "note": "resend with this flag to deliberately start a new thread",
                        },
                    }),
                );
            }
        }
    }

    // Python: `body.get("signature", True) is not False` — only a literal
    // false disables the signature.
    let include_sig = body.get("signature") != Some(&Value::Bool(false));
    let attachments = match parse_attachments(&body) {
        Ok(a) => a,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    if !from_acct.is_empty() && connected.contains(&from_acct) {
        let res = ctx
            .client
            .compose_send(&from_acct, &to, &subject, &message, &cc, "", "", "", include_sig, &attachments)
            .await;
        report_outcome(&ctx.registry, &res.as_ref().map(|_| ()).map_err(Clone::clone));
        return match res {
            Err(e) => err(StatusCode::BAD_GATEWAY, json!({ "error": e })),
            Ok(res) => {
                email_log(
                    ctx.client.home(),
                    json!({
                        "endpoint": "send", "via": "gmail", "from": from_acct,
                        "to": to, "cc": if cc.is_empty() { Value::Null } else { json!(cc) },
                        "subject": subject,
                        "body_chars": message.chars().count(),
                        "body_preview": message.chars().take(240).collect::<String>(),
                        "id": res.get("id").cloned().unwrap_or(Value::Null),
                        "thread_id": res.get("thread_id").cloned().unwrap_or(Value::Null),
                        "session": hdr_worker(&headers),
                    }),
                );
                Json(json!({
                    "ok": true, "to": to, "subject": subject, "from": from_acct,
                    "cc": if cc.is_empty() { Value::Null } else { json!(cc) },
                    "via": "gmail",
                    "id": res.get("id").cloned().unwrap_or(Value::Null),
                    "thread_id": res.get("thread_id").cloned().unwrap_or(Value::Null),
                    "signature_included": res.get("signature_included").cloned().unwrap_or(Value::Null),
                }))
                .into_response()
            }
        };
    }
    refuse_send(&ctx, &headers, "send", &from_acct)
}

use super::py_truthy as truthy;

// ---- POST /api/email/reply ------------------------------------------------

pub async fn reply(
    Extension(ctx): Extension<Arc<EmailCtx>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let mut message_id = body_str(&body, "message_id");
    let reply_body = body_str(&body, "body");
    let reply_all = body.get("reply_all").map(truthy).unwrap_or(false);
    let from_acct = body_str(&body, "from");
    let dry_run = body.get("dry_run").map(truthy).unwrap_or(false);
    let connected = ctx.client.connected_accounts();

    // REPLY-BY-SELECTOR (AMUX-1739): resolve "the latest message from X"
    // server-side; dry_run returns the target WITHOUT sending.
    let mut resolved: Option<Value> = None;
    if message_id.is_empty() {
        let sel_from = body_str(&body, "reply_to_latest_from");
        if !sel_from.is_empty() {
            let subj_c = body_str(&body, "subject_contains");
            let pref = body_str(&body, "from");
            let mut accts: Vec<String> = Vec::new();
            if connected.contains(&pref) {
                accts.push(pref.clone());
            }
            accts.extend(connected.iter().filter(|a| **a != pref).cloned());
            for a in &accts {
                resolved = ctx.client.latest_matching(a, &sel_from, "", &subj_c, 0).await;
                if resolved.is_some() {
                    break;
                }
            }
            match &resolved {
                Some(r) => {
                    message_id =
                        r.get("message_id").and_then(Value::as_str).unwrap_or("").to_string()
                }
                None => {
                    let mut msg = format!("no message from {sel_from}");
                    if !subj_c.is_empty() {
                        msg.push_str(&format!(" with subject containing {subj_c:?}"));
                    }
                    msg.push_str(" found in any connected account");
                    return err(StatusCode::NOT_FOUND, json!({ "error": msg }));
                }
            }
        }
    }
    if message_id.is_empty() || (reply_body.is_empty() && !dry_run) {
        return err(
            StatusCode::BAD_REQUEST,
            json!({ "error": "message_id (or reply_to_latest_from) and body are required" }),
        );
    }
    if dry_run {
        return Json(json!({
            "ok": true, "dry_run": true, "would_reply_to": message_id,
            "resolved": resolved,
            "note": "no email sent — repeat without dry_run to send",
        }))
        .into_response();
    }
    let gmail_from =
        if from_acct.is_empty() { body_str(&body, "account") } else { from_acct.clone() };
    let include_sig = body.get("signature") != Some(&Value::Bool(false));
    let attachments = match parse_attachments(&body) {
        Ok(a) => a,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    if !gmail_from.is_empty() && connected.contains(&gmail_from) {
        let allow_self = body.get("allow_self").map(truthy).unwrap_or(false);
        // EXTERNAL-SEND APPROVAL GATE (AMUX-3510), the /reply half — STRICTER
        // by construction, because a reply's recipients are the THREAD's, not
        // the caller's: the incident's one call reached four people it never
        // named. Resolve the fan-out first (the same lookup + plan the send
        // runs), classify, and freeze for a human if anyone external is on it.
        if let Some(lane) = hdr_worker(&headers) {
            let home = ctx.client.home().to_path_buf();
            if !crate::api::email_approval::exempt_sessions(&home).contains(&lane.to_lowercase())
            {
                let (r_to, r_cc, r_subject) = match ctx
                    .client
                    .resolve_reply_recipients(&gmail_from, &message_id, reply_all, allow_self)
                    .await
                {
                    Ok(v) => v,
                    Err(e) => return err(StatusCode::BAD_GATEWAY, json!({ "error": e })),
                };
                let externals = crate::api::email_approval::external_recipients(
                    &format!("{r_to},{r_cc}"),
                    &crate::api::email_approval::internal_domains(&home),
                    &connected,
                );
                if !externals.is_empty() {
                    let preview = json!({
                        "endpoint": "reply", "from": gmail_from,
                        "in_reply_to": message_id, "reply_all": reply_all,
                        "to": r_to, "cc": r_cc, "subject": r_subject,
                        "body": reply_body.chars().take(2000).collect::<String>(),
                        "external_recipients": externals,
                    });
                    let payload = json!({
                        "from": gmail_from, "message_id": message_id,
                        "body": reply_body, "reply_all": reply_all,
                        "allow_self": allow_self,
                        "signature": body.get("signature").cloned().unwrap_or(Value::Null),
                        "attachments": body.get("attachments").cloned().unwrap_or(Value::Null),
                    });
                    return match crate::api::email_approval::create_approval(
                        &home, &lane, "reply", payload, preview.clone(),
                    ) {
                        Ok(id) => {
                            email_log(
                                ctx.client.home(),
                                json!({
                                    "endpoint": "reply", "blocked": "approval_required",
                                    "approval_id": id, "from": gmail_from,
                                    "to": r_to, "cc": r_cc, "in_reply_to": message_id,
                                    "subject": r_subject, "session": lane,
                                }),
                            );
                            tracing::warn!(
                                session = %lane, approval = %id,
                                "[email] EXTERNAL reply from a worker held for human approval (AMUX-3510)"
                            );
                            approval_required_response(&id, preview)
                        }
                        Err(e) => err(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            json!({ "error": format!("could not record the approval request: {e} — NOT sent") }),
                        ),
                    };
                }
            }
        }
        let res = ctx
            .client
            .reply_send(&gmail_from, &message_id, &reply_body, include_sig, reply_all, allow_self, &attachments)
            .await;
        report_outcome(&ctx.registry, &res.as_ref().map(|_| ()).map_err(Clone::clone));
        return match res {
            Err(e) => err(StatusCode::BAD_GATEWAY, json!({ "error": e })),
            Ok(res) => {
                email_log(
                    ctx.client.home(),
                    json!({
                        "endpoint": "reply", "via": "gmail", "from": gmail_from,
                        // Resolved by reply_send from the anchor message (GT-58):
                        // without these, no ledger consumer could attribute a
                        // reply to a recipient (AMUX-1897), and gtm's corrective
                        // sender had to confirm sends via Sent-mailbox search.
                        "to": res.get("to").cloned().unwrap_or(Value::Null),
                        "cc": res.get("cc").cloned().unwrap_or(Value::Null),
                        "subject": res.get("subject").cloned().unwrap_or(Value::Null),
                        "in_reply_to": message_id, "reply_all": reply_all,
                        "body_chars": reply_body.chars().count(),
                        "body_preview": reply_body.chars().take(240).collect::<String>(),
                        "id": res.get("id").cloned().unwrap_or(Value::Null),
                        "thread_id": res.get("thread_id").cloned().unwrap_or(Value::Null),
                        "session": hdr_worker(&headers),
                    }),
                );
                Json(json!({
                    "ok": true, "message_id": message_id, "reply_all": reply_all,
                    "to": res.get("to").cloned().unwrap_or(Value::Null),
                    "subject": res.get("subject").cloned().unwrap_or(Value::Null),
                    "from": gmail_from, "via": "gmail",
                    "id": res.get("id").cloned().unwrap_or(Value::Null),
                    "thread_id": res.get("thread_id").cloned().unwrap_or(Value::Null),
                    "orig_thread_id": res.get("orig_thread_id").cloned().unwrap_or(Value::Null),
                    "threaded": res.get("threaded").cloned().unwrap_or(Value::Null),
                    "signature_included": res.get("signature_included").cloned().unwrap_or(Value::Null),
                }))
                .into_response()
            }
        };
    }
    refuse_send(&ctx, &headers, "reply", &gmail_from)
}

/// The refusal every gated send/reply returns (AMUX-3510): the draft comes
/// BACK with an approval id instead of going out. The worker's next honest
/// move is spelled out in the body, because the AMUX-2325 lesson is that a
/// refusal whose escape is unstated gets walked around off-trail.
fn approval_required_response(id: &str, preview: Value) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "ok": false,
            "code": "approval_required",
            "approval_id": id,
            "preview": preview,
            "why": "external recipients + worker origin: Ethan's rule (2026-08-22) — no email \
                    to anyone outside the internal domains without explicit per-message human \
                    approval",
            "what_to_do": "surface the preview to Ethan (board card or your turn output) and \
                           STOP — do not resend, do not rephrase, do not approve it yourself. \
                           A human approves from the dashboard origin: \
                           POST /api/email/approve/<approval_id> (no X-Amux-Session header). \
                           The approval expires in 1h and releases exactly this draft.",
            "expires_in_s": crate::api::email_approval::APPROVAL_TTL_S as i64,
        })),
    )
        .into_response()
}

/// POST /api/email/reject/{id} — the human DISCARD (AMUX-3698).
///
/// Reported by autodesk, and Ethan hit the consequence directly: his approvals
/// banner showed two of autodesk's `example.invalid` gate probes, and the only
/// button on it was "Approve & send". A human looking at a draft they do not
/// want has had exactly one move — wait an hour for the TTL — while the copy
/// underneath told them so.
///
/// That is ethos rule 3 on a HUMAN-facing surface: a constraint whose only exit
/// is the passage of time. It also means the queue cannot distinguish "pending a
/// decision" from "decided against, waiting to rot", so a glance at the banner
/// overstates what is actually awaiting judgment.
///
/// Attributed like approve, and for the same reason: a discard is a decision
/// about someone else's queued work, and "why was this not sent" is a question
/// the ledger could not answer for ANY unsent draft. It can now.
pub async fn reject(
    Extension(ctx): Extension<Arc<EmailCtx>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let by = headers
        .get("x-amux-approver")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| hdr_worker(&headers))
        .unwrap_or_else(|| "unidentified".to_string());
    let reason = body
        .as_ref()
        .and_then(|b| b.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let home = ctx.client.home().to_path_buf();
    // The creating lane may discard its OWN draft — unlike approve, where
    // self-release is the whole loop the gate exists to break. Withdrawing a
    // send you queued is not a bypass; refusing it would leave a worker that
    // notices its own mistake with no way to clean up after itself, which is
    // what left two probe drafts in Ethan's banner.
    match crate::api::email_approval::discard(&home, &id, &by) {
        Some(doc) => {
            let session = doc.get("session").and_then(Value::as_str).unwrap_or("").to_string();
            tracing::info!(
                approval = %id, session = %session, by = %by, reason = %reason,
                "[email] approval DISCARDED — the draft was never sent (AMUX-3698)"
            );
            email_log(
                ctx.client.home(),
                json!({
                    "endpoint": "reject", "approval_id": id, "session": session,
                    "rejected": true, "rejected_by": by, "reason": reason,
                    "approver_verified": false,
                }),
            );
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "rejected": true, "approval_id": id,
                             "was_for_session": session })),
            )
                .into_response()
        }
        None => err(
            StatusCode::NOT_FOUND,
            json!({ "error": crate::api::email_approval::fate(&home, &id) }),
        ),
    }
}

/// POST /api/email/approve/{id} — the human release (AMUX-3510). Refuses any
/// request carrying a worker origin: a worker approving its own held send is
/// the exact loop the gate exists to break.
pub async fn approve(
    Extension(ctx): Extension<Arc<EmailCtx>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(lane) = hdr_worker(&headers) {
        tracing::warn!(
            session = %lane, approval = %id,
            "[email] a WORKER tried to approve a held external send — refused (AMUX-3510)"
        );
        return err(
            StatusCode::FORBIDDEN,
            json!({
                "error": "approval must come from a human context — this request carries a \
                          worker origin. Surface the preview to Ethan instead.",
                "code": "worker_cannot_approve",
            }),
        );
    }
    // AUTHORIZE ON A POSITIVE, NEVER ON AN ABSENCE (autodesk, AUTOD-48).
    //
    // THE BYPASS, measured against the live server, two calls and 743ms:
    //
    //   12:51:06  POST /api/email/send      X-Amux-Session: autodesk  -> 403, apr_5da6...
    //   12:51:07  POST /api/email/approve/apr_5da6...   no headers    -> 200 approved:true
    //
    // Same shell, same lane, no human. The check above only ever stopped a lane
    // that VOLUNTEERED its identity while trying to bypass a control, which is
    // not a behaviour worth designing against. Absence of `X-Amux-Session` is
    // the one condition a worker can produce at will.
    //
    // AND THE LEDGER LIED ABOUT IT, which is worse than the bypass. The release
    // row read `approved_by: "dashboard (no worker origin)"` — an affirmative
    // false exculpation, inferred from an absence. Anyone auditing the 22 Aug
    // incident class would read that row and clear the lane. The send-path hole
    // autodesk probed earlier degrades to `session='unattributed'`, a flag a
    // reader can act on; this one handed out a clean bill of health.
    //
    // WHAT THIS IS AND IS NOT. On a localhost box a worker can send any header,
    // so `X-Amux-Approver` is NOT a cryptographic boundary and nothing here
    // pretends otherwise. What it changes is the shape of the act: a bypass now
    // requires the lane to ASSERT a human identity under a name, instead of
    // obtaining one by omitting a field. The ledger then records the claim as a
    // claim (`approver_verified: false`) rather than manufacturing a fact.
    //
    // A real boundary needs an auth distinction the localhost bypass currently
    // erases — see AMUX-3699. Fixing that is not a reason to leave this.
    let approver = headers
        .get("x-amux-approver")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    let Some(approver) = approver else {
        tracing::warn!(
            approval = %id,
            "[email] approve refused: no X-Amux-Approver. Absence used to mean 'human' and \
             that is the AUTOD-48 bypass — a worker produces an absence at will"
        );
        return err(
            StatusCode::FORBIDDEN,
            json!({
                "error": "approval must NAME its approver: send X-Amux-Approver: <who>. \
                          Absence used to be read as 'a human at the dashboard', which a \
                          worker can produce by sending no headers at all (AUTOD-48).",
                "code": "approver_unidentified",
                "how_to_fix": {
                    "dashboard": "the Approvals panel sends this automatically — reload if it does not",
                    "curl": "curl -X POST -H 'X-Amux-Approver: <your name>' $AMUX_URL/api/email/approve/<id>",
                    "note": "this names you in the send-audit ledger; it is not a password",
                },
            }),
        );
    };
    let home = ctx.client.home().to_path_buf();
    // CREATOR CHECK BEFORE CONSUME (AUTOD-48 follow-up). This used to run
    // AFTER, and my own comment argued that was the safe direction. It was not:
    // `consume` is the one-shot rename, so reading the doc to learn the creator
    // DESTROYED the draft the check refused.
    //
    // What that cost, from autodesk's bisect: any lane could delete any pending
    // draft, because `GET /api/email/approvals` hands every caller the id AND
    // the creating session — name that session back at the server and the draft
    // is gone. And the legitimate failure was worse than the abuse: a lane that
    // self-approved ONCE, the exact mistake this gate exists to catch,
    // permanently destroyed the message a human was supposed to review.
    //
    // The retry worry that motivated the old order does not survive contact
    // with it. The approver is a NAME, not a secret, so an attacker never
    // needed to retry against one pending row; they could always create their
    // own. I traded a real destructive defect for a guess about an attack that
    // the design already conceded.
    if let Some(pending) = crate::api::email_approval::peek(&home, &id) {
        let creator = pending.get("session").and_then(Value::as_str).unwrap_or("");
        if !creator.is_empty() && approver.eq_ignore_ascii_case(creator) {
            tracing::warn!(
                session = %creator, approver = %approver, approval = %id,
                "[email] approve refused: the approver names the REQUESTING lane (AUTOD-48 v5) \
                 — the draft is left PENDING, a refusal must not destroy what it refuses"
            );
            // AUDIT THE REFUSAL. It previously wrote nothing, so the only trace
            // of a destroyed draft was its original freeze — a draft that
            // existed and died left no row at all.
            email_log(
                ctx.client.home(),
                json!({
                    "endpoint": "approve", "blocked": "creator_cannot_approve",
                    "approval_id": id, "session": creator, "approver_claimed": approver,
                    "approver_verified": false,
                }),
            );
            return err(
                StatusCode::FORBIDDEN,
                json!({
                    "error": format!(
                        "'{approver}' requested this send, so it cannot also approve it. The \
                         draft is STILL PENDING — surface the preview to a human."
                    ),
                    "code": "creator_cannot_approve",
                }),
            );
        }
    }
    let doc = match crate::api::email_approval::consume(&home, &id) {
        crate::api::email_approval::Consume::Ready(d) => d,
        crate::api::email_approval::Consume::Expired => {
            return err(
                StatusCode::GONE,
                json!({ "error": "approval expired (1h) — the worker must request the send again" }),
            )
        }
        crate::api::email_approval::Consume::Gone => {
            // DO NOT ENUMERATE OUTCOMES THE SERVER CANNOT DISTINGUISH
            // (AUTOD-48 follow-up). This said "already approved, expired, or
            // never existed", and the first branch EXCULPATES: an auditor reads
            // it and concludes a human released the draft. That is the same
            // family as the `approved_by: "dashboard"` inference this endpoint
            // was just fixed for — a message asserting something the server
            // does not know.
            //
            // The directory DOES know, because consumed and expired files are
            // renamed rather than deleted, so say which when it can be read and
            // say nothing when it cannot.
            let fate = crate::api::email_approval::fate(&home, &id);
            return err(StatusCode::NOT_FOUND, json!({ "error": fate }));
        }
    };
    let payload = doc.get("payload").cloned().unwrap_or_else(|| json!({}));
    let session = doc.get("session").and_then(Value::as_str).unwrap_or("").to_string();
    let endpoint = doc.get("endpoint").and_then(Value::as_str).unwrap_or("").to_string();
    let p = |k: &str| payload.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let include_sig = payload.get("signature") != Some(&Value::Bool(false));
    let attachments = match parse_attachments(&payload) {
        Ok(a) => a,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    let res = match endpoint.as_str() {
        "send" => {
            ctx.client
                .compose_send(
                    &p("from"), &p("to"), &p("subject"), &p("body"), &p("cc"),
                    "", "", "", include_sig, &attachments,
                )
                .await
        }
        "reply" => {
            let reply_all = payload.get("reply_all").map(truthy).unwrap_or(false);
            let allow_self = payload.get("allow_self").map(truthy).unwrap_or(false);
            ctx.client
                .reply_send(
                    &p("from"), &p("message_id"), &p("body"), include_sig,
                    reply_all, allow_self, &attachments,
                )
                .await
        }
        other => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": format!("frozen approval has unknown endpoint {other:?}") }),
            )
        }
    };
    report_outcome(&ctx.registry, &res.as_ref().map(|_| ()).map_err(Clone::clone));
    match res {
        Err(e) => err(StatusCode::BAD_GATEWAY, json!({ "error": e, "approval_id": id })),
        Ok(res) => {
            // The audit answers "who approved this", not only "which worker
            // sent it" (autodesk's point 5). The approver is the dashboard
            // origin; the SESSION on the row stays the worker that authored
            // the draft, so both halves of the story are on one row.
            email_log(
                ctx.client.home(),
                json!({
                    "endpoint": endpoint, "via": "gmail", "approved": true,
                    "approval_id": id,
                    // The CLAIM, verbatim. Never "dashboard (no worker origin)",
                    // which was an inference from an absence and was false in
                    // AUTOD-48's specimen. `approver_verified` says plainly that
                    // nothing here proved it, so a forensic reader weighs it as
                    // a claim rather than a finding.
                    "approved_by": approver,
                    "approver_verified": false,
                    "from": p("from"), "to": p("to"),
                    "subject": p("subject"),
                    "body_chars": p("body").chars().count(),
                    "body_preview": p("body").chars().take(240).collect::<String>(),
                    "id": res.get("id").cloned().unwrap_or(Value::Null),
                    "thread_id": res.get("thread_id").cloned().unwrap_or(Value::Null),
                    "session": session,
                }),
            );
            Json(json!({
                "ok": true, "approved": true, "approval_id": id,
                "sent_for_session": session,
                "id": res.get("id").cloned().unwrap_or(Value::Null),
                "thread_id": res.get("thread_id").cloned().unwrap_or(Value::Null),
                "to": res.get("to").cloned().unwrap_or(Value::Null),
            }))
            .into_response()
        }
    }
}

/// GET /api/email/approvals — pending approvals for the dashboard / a
/// human's curl. Previews only; the frozen payload stays server-side.
pub async fn list_approvals(Extension(ctx): Extension<Arc<EmailCtx>>) -> Response {
    let pending = crate::api::email_approval::list_pending(ctx.client.home());
    Json(json!({
        "pending": pending,
        "approve_with": "POST /api/email/approve/<id> from a human context (no worker origin header)",
    }))
    .into_response()
}

// ---- GET /api/email/inbox -------------------------------------------------

pub async fn inbox(
    Extension(ctx): Extension<Arc<EmailCtx>>,
    Query(qs): Query<HashMap<String, String>>,
) -> Response {
    let account_filter = qs.get("account").cloned().unwrap_or_default();
    let count: usize =
        qs.get("count").and_then(|v| v.parse().ok()).unwrap_or(20).min(500);
    let lookback_days: f64 = qs.get("days").and_then(|v| v.parse().ok()).unwrap_or(7.0);
    let envelope = matches!(
        qs.get("envelope").map(String::as_str),
        Some("1") | Some("true") | Some("yes")
    );
    let reply_shape = |msgs: Vec<Value>, truncated: bool| -> Response {
        if envelope {
            Json(json!({
                "messages": msgs, "returned": msgs.len(),
                "truncated": truncated, "window_days": lookback_days,
            }))
            .into_response()
        } else {
            Json(Value::Array(msgs)).into_response()
        }
    };
    let connected = ctx.client.connected_accounts();
    if !account_filter.is_empty() && connected.contains(&account_filter) {
        let res = ctx.client.inbox_messages(&account_filter, count, "", lookback_days).await;
        report_outcome(&ctx.registry, &res.as_ref().map(|_| ()).map_err(Clone::clone));
        return match res {
            Err(e) => err(StatusCode::BAD_GATEWAY, json!({ "error": e })),
            Ok(v) => reply_shape(
                v.get("messages").and_then(Value::as_array).cloned().unwrap_or_default(),
                v.get("truncated").and_then(Value::as_bool).unwrap_or(false),
            ),
        };
    }
    if account_filter.is_empty() && !connected.is_empty() {
        // Parallel fan-out over connected accounts (the Python 504 fix):
        // each bounded so one wedged token can't hang the unified inbox.
        let futs = connected.iter().map(|a| {
            let client = ctx.client.clone();
            let a = a.clone();
            async move {
                tokio::time::timeout(
                    std::time::Duration::from_secs(20),
                    client.inbox_messages(&a, count, "", lookback_days),
                )
                .await
                .ok()
                .and_then(Result::ok)
            }
        });
        let results = futures::future::join_all(futs).await;
        let mut msgs: Vec<Value> = Vec::new();
        let mut any_trunc = false;
        let mut any_ok = false;
        for r in results.into_iter().flatten() {
            any_ok = true;
            if let Some(m) = r.get("messages").and_then(Value::as_array) {
                msgs.extend(m.iter().cloned());
            }
            if r.get("truncated").and_then(Value::as_bool).unwrap_or(false) {
                any_trunc = true;
            }
        }
        if any_ok {
            ctx.registry.set("email", IntegrationState::Available);
        }
        // Newest first; an unparseable date sinks, never errors.
        msgs.sort_by(|a, b| recv_ts(b).partial_cmp(&recv_ts(a)).unwrap_or(std::cmp::Ordering::Equal));
        if msgs.len() > count {
            any_trunc = true;
        }
        msgs.truncate(count);
        return reply_shape(msgs, any_trunc);
    }
    applescript_not_ported(&account_filter)
}

fn recv_ts(m: &Value) -> f64 {
    m.get("date")
        .and_then(Value::as_str)
        .and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok())
        .map(|dt| dt.timestamp() as f64)
        .unwrap_or(0.0)
}

// ---- GET /api/email/search ------------------------------------------------

/// GET /api/email/message/{id} — read ONE message by its RFC822 Message-ID (the
/// `message_id` returned by /inbox and /search). The global CLAUDE.md documented
/// this endpoint, but the cutover never ported it, so a worker following the docs
/// got a bare `{"error":"not found"}` (autodesk, 2026-08-18). Resolves the id via
/// a Gmail `rfc822msgid:` search across the connected accounts (or `?account=` to
/// target one) and returns the full message in the same shape /inbox yields.
pub async fn message(
    Extension(ctx): Extension<Arc<EmailCtx>>,
    Path(id): Path<String>,
    Query(qs): Query<HashMap<String, String>>,
) -> Response {
    let id = id.trim().trim_start_matches('<').trim_end_matches('>').trim().to_string();
    if id.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "message id required" }));
    }
    let account = qs.get("account").map(|s| s.trim().to_string()).unwrap_or_default();
    let explicit_account = !account.is_empty();
    let connected = ctx.client.connected_accounts();
    let accounts: Vec<String> = if !account.is_empty() {
        if !connected.contains(&account) {
            return err(
                StatusCode::BAD_REQUEST,
                json!({ "error": format!("account {account} is not connected"), "connected": connected }),
            );
        }
        vec![account]
    } else {
        connected.clone()
    };
    if accounts.is_empty() {
        return err(StatusCode::SERVICE_UNAVAILABLE, json!({ "error": "no connected Gmail accounts" }));
    }
    // Resolve the RFC822 id to a gmail message id, then fetch format=full so the
    // FULL body + attachments come back — not the list-path snippet (AMUX-3354,
    // autodesk: the snippet path truncated bodies and hid attachments).
    //
    // SERVE THE SENT COPY WHEN WE SENT IT (GT-59). A cross-account send
    // (info@ -> ethan@) stores TWO objects with one Message-ID, and Gmail's
    // DELIVERY pipeline rewrites the recipient copy's text/plain part —
    // measured on the 2026-08-20 RB2B digest and reproduced with a controlled
    // probe: the sent copy read 120 lines / 44 blanks, the delivered copy 87
    // lines / 0 blanks, newlines collapsed and rewrapped. The account loop
    // used to return whichever copy matched first, which for our own outbound
    // mail was the MANGLED delivered copy — so "read back what was actually
    // sent" audited Gmail's rewrite and filed a bug against our composer
    // (GT-59 did exactly that; the composer was verbatim all along). When the
    // found copy's From is one of OUR connected accounts and a copy exists in
    // that account, serve THAT one — the bytes we actually handed Gmail. The
    // response's `copy` field names which one was served, so forensics can
    // see the discriminator instead of inferring it (rule 4). An explicit
    // ?account= always wins and is labelled honestly.
    for acct in &accounts {
        if let Some(meta) = ctx.client.find_message_by_rfc822(acct, &id).await {
            if let Some(gmail_id) = meta.get("id").and_then(Value::as_str) {
                return match ctx.client.get_message_full(acct, gmail_id).await {
                    Ok(mut full) => {
                        let from = full.get("from").and_then(Value::as_str).unwrap_or("");
                        let from_acct = connected
                            .iter()
                            .find(|a| from.to_lowercase().contains(&a.to_lowercase()))
                            .cloned();
                        let explicit = explicit_account;
                        match from_acct {
                            Some(fa) if fa != *acct && !explicit => {
                                // Our own outbound mail, found via the recipient
                                // copy: re-resolve in the sending account.
                                if let Some(smeta) =
                                    ctx.client.find_message_by_rfc822(&fa, &id).await
                                {
                                    if let Some(sid) = smeta.get("id").and_then(Value::as_str) {
                                        if let Ok(mut sent) =
                                            ctx.client.get_message_full(&fa, sid).await
                                        {
                                            sent["copy"] = json!("sent");
                                            sent["delivered_copy_account"] = json!(acct);
                                            return Json(sent).into_response();
                                        }
                                    }
                                }
                                full["copy"] = json!("delivered");
                                Json(full).into_response()
                            }
                            Some(fa) if fa == *acct => {
                                full["copy"] = json!("sent");
                                Json(full).into_response()
                            }
                            _ => {
                                full["copy"] = json!("delivered");
                                Json(full).into_response()
                            }
                        }
                    }
                    Err(e) => err(StatusCode::BAD_GATEWAY, json!({ "error": e })),
                };
            }
        }
    }
    err(
        StatusCode::NOT_FOUND,
        json!({
            "error": "message not found",
            "id": id,
            "searched_accounts": accounts,
            "hint": "the id is the RFC822 Message-ID from /inbox or /search (e.g. <...@host>), not the gmail short id; pass ?account=<email> to target one mailbox",
        }),
    )
}

/// GET /api/email/message/{id}/attachments/{attachment_id} — download one
/// attachment's raw bytes (AMUX-3354/autodesk). `{id}` is the RFC822 Message-ID,
/// `{attachment_id}` the Gmail attachmentId from the message's attachments list.
/// `?filename=` and `?content_type=` set the download headers.
pub async fn message_attachment(
    Extension(ctx): Extension<Arc<EmailCtx>>,
    Path((id, attachment_id)): Path<(String, String)>,
    Query(qs): Query<HashMap<String, String>>,
) -> Response {
    let id = id.trim().trim_start_matches('<').trim_end_matches('>').trim().to_string();
    let account = qs.get("account").map(|s| s.trim().to_string()).unwrap_or_default();
    let connected = ctx.client.connected_accounts();
    let accounts: Vec<String> = if !account.is_empty() {
        if !connected.contains(&account) {
            return err(StatusCode::BAD_REQUEST, json!({ "error": format!("account {account} is not connected") }));
        }
        vec![account]
    } else {
        connected.clone()
    };
    for acct in &accounts {
        if let Some(meta) = ctx.client.find_message_by_rfc822(acct, &id).await {
            if let Some(gid) = meta.get("id").and_then(Value::as_str) {
                return match ctx.client.get_attachment(acct, gid, &attachment_id).await {
                    Ok(bytes) => {
                        let fname = qs.get("filename").map(String::as_str).unwrap_or("attachment").replace(['"', '\r', '\n'], "");
                        let ct = qs.get("content_type").map(String::as_str).unwrap_or("application/octet-stream").to_string();
                        (
                            [
                                (axum::http::header::CONTENT_TYPE, ct),
                                (axum::http::header::CONTENT_DISPOSITION, format!("attachment; filename=\"{fname}\"")),
                            ],
                            bytes,
                        )
                            .into_response()
                    }
                    Err(e) => err(StatusCode::BAD_GATEWAY, json!({ "error": e })),
                };
            }
        }
    }
    err(StatusCode::NOT_FOUND, json!({ "error": "message not found", "id": id }))
}

pub async fn search(
    Extension(ctx): Extension<Arc<EmailCtx>>,
    Query(qs): Query<HashMap<String, String>>,
) -> Response {
    let q = qs.get("q").map(|s| s.trim().to_string()).unwrap_or_default();
    if q.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "q parameter is required" }));
    }
    let account = qs.get("account").map(|s| s.trim().to_string()).unwrap_or_default();
    let limit: usize = qs.get("limit").and_then(|v| v.parse().ok()).unwrap_or(20).min(100);
    let days: i64 = qs.get("days").and_then(|v| v.parse().ok()).unwrap_or(30);
    let mailbox = qs.get("mailbox").map(|s| s.trim().to_string()).unwrap_or_default();

    // Python `_gmail_query`: mailbox maps to a Gmail operator; `days` is a
    // real filter (it was silently ignored once — kept fixed).
    let mut gq = q.clone();
    let mb = mailbox.to_lowercase();
    if mb == "sent" || mb == "sent mail" {
        gq.push_str(" in:sent");
    } else if mb == "inbox" {
        gq.push_str(" in:inbox");
    } else if !mb.is_empty() && mb != "all" {
        gq.push_str(&format!(" label:{mailbox}"));
    }
    if days > 0 {
        gq.push_str(&format!(" newer_than:{days}d"));
    }
    let gq = gq.trim().to_string();

    let connected = ctx.client.connected_accounts();
    if !account.is_empty() && connected.contains(&account) {
        let res = ctx.client.inbox_messages(&account, limit, &gq, 0.0).await;
        report_outcome(&ctx.registry, &res.as_ref().map(|_| ()).map_err(Clone::clone));
        return match res {
            Err(e) => err(StatusCode::BAD_GATEWAY, json!({ "error": e })),
            Ok(v) => Json(v.get("messages").cloned().unwrap_or(json!([]))).into_response(),
        };
    }
    if account.is_empty() && !connected.is_empty() {
        let futs = connected.iter().map(|a| {
            let client = ctx.client.clone();
            let (a, gq) = (a.clone(), gq.clone());
            async move { client.inbox_messages(&a, limit, &gq, 0.0).await.ok() }
        });
        let results = futures::future::join_all(futs).await;
        let mut merged: Vec<Value> = Vec::new();
        for r in results.into_iter().flatten() {
            if let Some(m) = r.get("messages").and_then(Value::as_array) {
                merged.extend(m.iter().cloned());
            }
        }
        merged.sort_by(|a, b| {
            recv_ts(b).partial_cmp(&recv_ts(a)).unwrap_or(std::cmp::Ordering::Equal)
        });
        merged.truncate(limit);
        return Json(Value::Array(merged)).into_response();
    }
    applescript_not_ported(&account)
}

// ---- GET /api/email/log ---------------------------------------------------

/// The send-audit ledger (AMUX-1897): one call answers "who sent X and
/// when". `session=unattributed` returns records sent without the header.
pub async fn send_log(
    Extension(ctx): Extension<Arc<EmailCtx>>,
    Query(qs): Query<HashMap<String, String>>,
) -> Response {
    let days: i64 = qs.get("days").and_then(|v| v.parse().ok()).unwrap_or(7);
    let limit: usize = qs.get("limit").and_then(|v| v.parse().ok()).unwrap_or(50).min(500);
    let session = qs.get("session").map(|s| s.trim().to_string()).unwrap_or_default();
    Json(read_email_log(ctx.client.home(), days, limit, &session)).into_response()
}

// ---------------------------------------------------------------------------
// Tests — mocked transport + temp homes only. No network, no live token
// files, no credential values.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // ---- AMUX-3103: the connector-scope eval -----------------------------
    //
    // Ethan's acceptance for the whole connectors block is "verify/validate
    // access in UNHAPPY paths", so these are written as the acceptance test and
    // not as an afterthought. Each one is a path that must DENY, plus the happy
    // path beside it — a deny-everything bug and a deny-nothing bug look
    // identical if you only test one side.
    fn scope_home(layers: &[(&str, &str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("env")).unwrap();
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        for (level, name, body) in layers {
            let p = match *level {
                "global" => dir.path().join("amux.env"),
                "group" => dir.path().join("env").join(format!("{name}.env")),
                _ => dir.path().join("sessions").join(format!("{name}.env")),
            };
            std::fs::write(p, body).unwrap();
        }
        dir
    }

    #[test]
    fn unscoped_lane_is_unrestricted_so_nothing_breaks_on_rollout() {
        let d = scope_home(&[("worker", "w", "CC_TAGS=gtm\n")]);
        // No AMUX_GMAIL_ACCOUNTS anywhere: the caller's `from` stands untouched.
        assert!(matches!(gmail_scope_check(d.path(), "w", "anyone@example.com"), Ok(None)));
    }

    #[test]
    fn wrong_account_is_denied_and_told_what_it_may_use() {
        let d = scope_home(&[("worker", "w", "AMUX_GMAIL_ACCOUNTS=personal@gmail.com\n")]);
        let denial = gmail_scope_check(d.path(), "w", "ethan@mixpeek.com")
            .expect_err("a lane scoped to personal MUST NOT send as ethan@mixpeek.com");
        assert_eq!(denial["blocked"], "connector_scope");
        assert_eq!(denial["requested"], "ethan@mixpeek.com");
        // The refusal must be actionable: naming the allowed set is what stops
        // the caller retrying the same thing (the AMUX-2325 lesson).
        assert!(denial["error"].as_str().unwrap().contains("personal@gmail.com"));
        assert!(denial["how_to_change"].as_str().unwrap().contains("AMUX_GMAIL_ACCOUNTS"));
    }

    #[test]
    fn right_account_is_allowed_case_insensitively() {
        let d = scope_home(&[("worker", "w", "AMUX_GMAIL_ACCOUNTS=info@mixpeek.com,ethan@mixpeek.com\n")]);
        assert!(matches!(gmail_scope_check(d.path(), "w", "ethan@mixpeek.com"), Ok(None)));
        // Addresses are case-insensitive; a scope that rejected Ethan@ would be a
        // deny-the-right-account bug, which is the failure users report as broken.
        assert!(matches!(gmail_scope_check(d.path(), "w", "Ethan@Mixpeek.com"), Ok(None)));
    }

    #[test]
    fn omitted_from_defaults_into_scope_not_to_the_global_default() {
        // The subtle one. Without this, a lane scoped to the personal account
        // still sends as ethan@mixpeek.com whenever `from` is omitted, because
        // the handler's historical default fires — scoped in name only.
        let d = scope_home(&[("worker", "w", "AMUX_GMAIL_ACCOUNTS=personal@gmail.com\n")]);
        match gmail_scope_check(d.path(), "w", "") {
            Ok(Some(defaulted)) => assert_eq!(defaulted, "personal@gmail.com"),
            other => panic!("expected the scoped default, got {other:?}"),
        }
    }

    #[test]
    fn worker_scope_overrides_group_scope() {
        // Ethan's actual layout: the mixpeek GROUP gets the work accounts, one
        // worker is moved to the personal account. This is the end-to-end proof
        // that connector scope rides the global->group->worker layers rather
        // than being a second mechanism.
        let d = scope_home(&[
            ("global", "", "AMUX_GMAIL_ACCOUNTS=info@mixpeek.com\n"),
            ("group", "mixpeek", "AMUX_GMAIL_ACCOUNTS=info@mixpeek.com,ethan@mixpeek.com\n"),
            ("worker", "refresh-house", "CC_TAGS=mixpeek\nAMUX_GMAIL_ACCOUNTS=personal@gmail.com\n"),
            ("worker", "backend", "CC_TAGS=mixpeek\n"),
        ]);
        // worker layer wins for the overridden lane
        assert!(gmail_scope_check(d.path(), "refresh-house", "ethan@mixpeek.com").is_err());
        assert!(matches!(gmail_scope_check(d.path(), "refresh-house", "personal@gmail.com"), Ok(None)));
        // a lane with no worker override inherits the GROUP layer, not global
        assert!(matches!(gmail_scope_check(d.path(), "backend", "ethan@mixpeek.com"), Ok(None)));
        assert!(gmail_scope_check(d.path(), "backend", "personal@gmail.com").is_err());
    }

    use super::*;
    use crate::db::Store;
    use crate::integrations::email::HttpTransport;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Mutex;
    use tower::ServiceExt;

    /// Scripted transport (same shape as the integrations::email mock):
    /// first (method, url-substring) match wins and pops.
    struct MockHttp {
        calls: Mutex<Vec<(String, String, Option<Value>)>>,
        script: Mutex<Vec<(String, String, u16, Value)>>,
    }
    impl MockHttp {
        fn new(script: Vec<(&str, &str, u16, Value)>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                script: Mutex::new(
                    script
                        .into_iter()
                        .map(|(m, u, s, v)| (m.to_string(), u.to_string(), s, v))
                        .collect(),
                ),
            })
        }
        fn answer(&self, method: &str, url: &str, body: Option<&Value>) -> Result<(u16, Value), String> {
            self.calls.lock().unwrap().push((method.into(), url.into(), body.cloned()));
            let mut script = self.script.lock().unwrap();
            if let Some(pos) =
                script.iter().position(|(m, sub, _, _)| m == method && url.contains(sub.as_str()))
            {
                let (_, _, status, v) = script.remove(pos);
                return Ok((status, v));
            }
            Err(format!("mock has no answer for {method} {url}"))
        }
    }
    #[async_trait]
    impl HttpTransport for MockHttp {
        async fn get(&self, url: &str, _b: Option<&str>) -> Result<(u16, Value), String> {
            self.answer("GET", url, None)
        }
        async fn post_json(
            &self,
            url: &str,
            _b: Option<&str>,
            body: &Value,
        ) -> Result<(u16, Value), String> {
            self.answer("POST", url, Some(body))
        }
        async fn post_form(
            &self,
            url: &str,
            form: &[(String, String)],
        ) -> Result<(u16, Value), String> {
            let v = Value::Object(form.iter().map(|(k, val)| (k.clone(), json!(val))).collect());
            self.answer("FORM", url, Some(&v))
        }
        async fn post_raw(
            &self,
            url: &str,
            _b: Option<&str>,
            _ct: &str,
            body: String,
        ) -> Result<(u16, String), String> {
            let (st, v) = self.answer("RAW", url, Some(&Value::String(body)))?;
            Ok((st, v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
        }
    }

    const ACCT: &str = "acct@example.com";

    /// Temp amux home with one connected account (PLACEHOLDER token values
    /// only — never real credentials).
    fn temp_home() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let tokens = dir.path().join("gmail-tokens");
        std::fs::create_dir_all(&tokens).unwrap();
        std::fs::write(
            tokens.join(format!("{ACCT}.json")),
            json!({
                "token": "PLACEHOLDER_ACCESS",
                "refresh_token": "PLACEHOLDER_REFRESH",
                "token_uri": "https://oauth2.googleapis.com/token",
                "client_id": "PLACEHOLDER_ID",
                "client_secret": "PLACEHOLDER_SECRET",
            })
            .to_string(),
        )
        .unwrap();
        dir
    }

    fn app_with(
        http: Arc<MockHttp>,
        home: &std::path::Path,
    ) -> (axum::Router, tempfile::TempDir, Arc<IntegrationRegistry>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("email-api-test.db")).unwrap();
        let state = AppState {
            store: Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
        let registry = Arc::new(IntegrationRegistry::new());
        let ctx = Arc::new(EmailCtx {
            client: Arc::new(GmailClient::new(http, home.to_path_buf())),
            registry: registry.clone(),
        });
        let router = Router::new().nest("/api/email", routes_with(ctx)).with_state(state);
        (router, dir, registry)
    }

    async fn send_req(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
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

    #[tokio::test]
    async fn send_validations_match_python() {
        let home = temp_home();
        let (app, _d, _r) = app_with(MockHttp::new(vec![]), home.path());
        // Threading headers rejected with Python's message.
        let (st, e) = send_req(
            &app,
            "POST",
            "/api/email/send",
            Some(json!({ "to": "x@y.co", "subject": "s", "body": "b", "in_reply_to": "<m@x>" })),
            &[],
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(e["error"].as_str().unwrap().contains("'in_reply_to' is not supported"));
        // Required fields.
        let (st, e) =
            send_req(&app, "POST", "/api/email/send", Some(json!({ "to": "x@y.co" })), &[]).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(e["error"], json!("to, subject, and body are required"));
        // Address validation, comma lists validated per part.
        let (st, e) = send_req(
            &app,
            "POST",
            "/api/email/send",
            Some(json!({ "to": "good@x.co, not-an-addr", "subject": "s", "body": "b" })),
            &[],
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(e["error"], json!("invalid email address: not-an-addr"));
    }

    #[tokio::test]
    async fn send_from_unconnected_account_is_honest_501() {
        let home = temp_home();
        let (app, _d, _r) = app_with(MockHttp::new(vec![]), home.path());
        let (st, e) = send_req(
            &app,
            "POST",
            "/api/email/send",
            // Internal recipient: the AMUX-3510 gate must not preempt the
            // 501 this test pins (external+worker is gated first by design).
            Some(json!({ "to": "ops@mixpeek.com", "subject": "s", "body": "b",
                         "from": "other@nowhere.com", "force_new_thread": true })),
            &[("x-amux-session", "gtm-lane")],
        )
        .await;
        assert_eq!(st, StatusCode::NOT_IMPLEMENTED);
        assert!(e["error"].as_str().unwrap().contains("not ported"), "{e}");

        // GE-621: the refusal must be VISIBLE in the audited log — an off-account
        // send attempt that leaves no trace is the incident's invisibility. The
        // refusal now writes a `via:"refused"` entry a sweep of /api/email/log can
        // find, so "someone keeps trying to send from a non-connected account"
        // self-announces (two-fixes rule).
        let log = crate::integrations::email::read_email_log(home.path(), 1, 50, "");
        let entries = log["log"].as_array().cloned().unwrap_or_default();
        let refused = entries
            .iter()
            .find(|x| x.get("refused") == Some(&json!(true)))
            .expect("the refused send must be recorded in the audited email log");
        assert_eq!(refused["from"], json!("other@nowhere.com"));
        assert_eq!(refused["endpoint"], json!("send"));
        assert_eq!(refused["via"], json!("refused"));
        assert_eq!(refused["session"], json!("gtm-lane"));
        // A refusal must never be recorded as a successful gmail send.
        assert!(
            entries.iter().all(|x| x.get("via") != Some(&json!("gmail"))),
            "a refused send must not appear as via:gmail"
        );
    }

    #[tokio::test]
    async fn send_happy_path_writes_attributed_audit_and_updates_registry() {
        let home = temp_home();
        let http = MockHttp::new(vec![
            // Guard probe finds no active thread.
            ("GET", "/messages?q=", 200, json!({ "messages": [] })),
            ("GET", "/settings/sendAs", 200, json!({ "sendAs": [] })),
            ("POST", "/messages/send", 200, json!({ "id": "m1", "threadId": "t1" })),
        ]);
        let (app, _d, reg) = app_with(http, home.path());
        let (st, res) = send_req(
            &app,
            "POST",
            "/api/email/send",
            // ops@mixpeek.com: INTERNAL, so this stays a plain attributed
            // send — worker sends to external recipients are the approval
            // gate's own cells now (AMUX-3510).
            Some(json!({ "to": "ops@mixpeek.com", "subject": "Hi", "body": "hello",
                         "from": ACCT, "cc": "" })),
            &[("x-amux-session", "tester-session")],
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{res}");
        assert_eq!(res["ok"], json!(true));
        assert_eq!(res["via"], json!("gmail"));
        assert_eq!(res["from"], json!(ACCT));
        assert_eq!(res["cc"], Value::Null); // Python: cc or None
        assert_eq!(res["id"], json!("m1"));
        assert_eq!(res["thread_id"], json!("t1"));
        // The send-audit ledger recorded the attributed send (AMUX-1897)...
        let (st, log) = send_req(&app, "GET", "/api/email/log?days=1", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(log["count"], json!(1));
        assert_eq!(log["log"][0]["session"], json!("tester-session"));
        assert_eq!(log["log"][0]["endpoint"], json!("send"));
        assert_eq!(log["log"][0]["to"], json!("ops@mixpeek.com"));
        // ...and the filter works both ways.
        let (_, none) =
            send_req(&app, "GET", "/api/email/log?session=unattributed", None, &[]).await;
        assert_eq!(none["count"], json!(0));
        // A real successful call is what proves email available (RR-0073).
        assert_eq!(reg.get("email"), Some(IntegrationState::Available));
    }

    #[tokio::test]
    async fn new_thread_guard_blocks_with_candidate_and_force_bypasses() {
        let home = temp_home();
        let meta = json!({
            "id": "g9", "threadId": "T7",
            "payload": { "headers": [
                { "name": "From", "value": "ceo@customer.com" },
                { "name": "Subject", "value": "Pilot" },
                { "name": "Message-ID", "value": "<pilot@x>" },
                { "name": "Date", "value": "Sun, 09 Aug 2026 10:00:00 -0400" },
            ]},
        });
        let http = MockHttp::new(vec![
            ("GET", "/messages?q=", 200, json!({ "messages": [{ "id": "g9" }] })),
            ("GET", "/messages/g9", 200, meta),
        ]);
        let (app, _d, _r) = app_with(http, home.path());
        let (st, e) = send_req(
            &app,
            "POST",
            "/api/email/send",
            Some(json!({ "to": "ceo@customer.com", "subject": "s", "body": "b", "from": ACCT })),
            &[],
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{e}");
        assert_eq!(e["blocked"], json!(true));
        assert_eq!(e["recipient"], json!("ceo@customer.com"));
        assert_eq!(e["candidate_thread"]["message_id"], json!("<pilot@x>"));
        assert_eq!(e["reply_instead"]["endpoint"], json!("POST /api/email/reply"));
        assert_eq!(e["reply_instead"]["body"]["message_id"], json!("<pilot@x>"));
        assert_eq!(e["or_force"]["force_new_thread"], json!(true));

        // force_new_thread skips the guard and sends.
        let http2 = MockHttp::new(vec![
            ("GET", "/settings/sendAs", 200, json!({ "sendAs": [] })),
            ("POST", "/messages/send", 200, json!({ "id": "m2", "threadId": "t2" })),
        ]);
        let (app2, _d2, _r2) = app_with(http2, home.path());
        let (st, res) = send_req(
            &app2,
            "POST",
            "/api/email/send",
            Some(json!({ "to": "ceo@customer.com", "subject": "s", "body": "b",
                         "from": ACCT, "force_new_thread": true })),
            &[],
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{res}");
        assert_eq!(res["id"], json!("m2"));
    }

    #[tokio::test]
    async fn reply_dry_run_and_validation() {
        let home = temp_home();
        let (app, _d, _r) = app_with(MockHttp::new(vec![]), home.path());
        // Missing everything -> Python's message.
        let (st, e) = send_req(&app, "POST", "/api/email/reply", Some(json!({})), &[]).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(
            e["error"],
            json!("message_id (or reply_to_latest_from) and body are required")
        );
        // dry_run answers without sending (no mock entries were consumed).
        let (st, res) = send_req(
            &app,
            "POST",
            "/api/email/reply",
            Some(json!({ "message_id": "<m@x>", "dry_run": true })),
            &[],
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(res["dry_run"], json!(true));
        assert_eq!(res["would_reply_to"], json!("<m@x>"));
        assert_eq!(res["note"], json!("no email sent — repeat without dry_run to send"));
    }

    #[tokio::test]
    async fn reply_happy_path_reports_threading_proof() {
        let home = temp_home();
        // w1 is EXEMPTED below: derive_reply_plan refuses all-internal
        // threads by construction, so every worker reply is external-facing
        // and would hit the AMUX-3510 gate — the exemption keeps this the
        // plain threading-proof case AND exercises the env knob end to end.
        std::fs::write(home.path().join("server.env"), "AMUX_EMAIL_EXTERNAL_EXEMPT=w1\n")
            .unwrap();
        let orig = json!({
            "id": "g1", "threadId": "T1",
            "payload": { "headers": [
                { "name": "Message-ID", "value": "<orig@ext>" },
                { "name": "Subject", "value": "Deal" },
                { "name": "From", "value": "p@customer.com" },
                { "name": "To", "value": ACCT },
            ]},
        });
        let http = MockHttp::new(vec![
            ("GET", "q=rfc822msgid", 200, json!({ "messages": [{ "id": "g1" }] })),
            ("GET", "/messages/g1", 200, orig),
            ("GET", "/settings/sendAs", 200, json!({ "sendAs": [] })),
            ("POST", "/messages/send", 200, json!({ "id": "m3", "threadId": "T1" })),
        ]);
        let (app, _d, _reg) = app_with(http, home.path());
        let (st, res) = send_req(
            &app,
            "POST",
            "/api/email/reply",
            Some(json!({ "message_id": "<orig@ext>", "body": "thanks", "from": ACCT })),
            &[("x-amux-worker", "w1")],
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{res}");
        assert_eq!(res["ok"], json!(true));
        assert_eq!(res["message_id"], json!("<orig@ext>"));
        assert_eq!(res["via"], json!("gmail"));
        assert_eq!(res["threaded"], json!(true));
        assert_eq!(res["orig_thread_id"], json!("T1"));
        // Audit line for the reply, attributed via X-Amux-Worker.
        let (_, log) = send_req(&app, "GET", "/api/email/log", None, &[]).await;
        assert_eq!(log["log"][0]["endpoint"], json!("reply"));
        assert_eq!(log["log"][0]["session"], json!("w1"));
        assert_eq!(log["log"][0]["in_reply_to"], json!("<orig@ext>"));
        // GT-58: reply rows logged to/subject as NULL while send rows carried
        // both, so AMUX-1897 forensics could not attribute any reply to a
        // recipient and ledger consumers got structural false-negatives
        // (gtm's corrective sender fell back to Sent-mailbox search). The
        // resolved envelope must land in the ledger row and the response.
        assert_eq!(log["log"][0]["to"], json!("p@customer.com"));
        assert_eq!(log["log"][0]["subject"], json!("Re: Deal"));
        assert_eq!(res["to"], json!("p@customer.com"));
        assert_eq!(res["subject"], json!("Re: Deal"));
    }

    #[tokio::test]
    async fn search_requires_q_and_inbox_envelope_shape() {
        let home = temp_home();
        let meta = json!({
            "id": "g1", "threadId": "T", "snippet": "snip", "labelIds": ["INBOX"],
            "payload": { "headers": [
                { "name": "From", "value": "a@ext.com" },
                { "name": "To", "value": ACCT },
                { "name": "Subject", "value": "s" },
                { "name": "Date", "value": "Sun, 09 Aug 2026 10:00:00 -0400" },
                { "name": "Message-ID", "value": "<m1@x>" },
            ]},
        });
        let http = MockHttp::new(vec![
            ("GET", "/messages?q=", 200, json!({ "messages": [{ "id": "g1" }] })),
            ("GET", "/messages/g1", 200, meta),
        ]);
        let (app, _d, _r) = app_with(http, home.path());
        let (st, e) = send_req(&app, "GET", "/api/email/search", None, &[]).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(e["error"], json!("q parameter is required"));

        let (st, res) = send_req(
            &app,
            "GET",
            &format!("/api/email/inbox?account={ACCT}&count=5&days=3&envelope=1"),
            None,
            &[],
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{res}");
        assert_eq!(res["returned"], json!(1));
        assert_eq!(res["truncated"], json!(false));
        assert_eq!(res["window_days"], json!(3.0));
        assert_eq!(res["messages"][0]["message_id"], json!("<m1@x>"));
        assert_eq!(res["messages"][0]["account"], json!(ACCT));
    }

    #[tokio::test]
    async fn auth_failure_marks_email_unavailable_in_registry() {
        let home = temp_home();
        let http = MockHttp::new(vec![
            // Guard probe fails open (no answer -> latest_matching None).
            ("GET", "/messages?q=", 500, json!({ "error": "x" })),
            // Signature fetch fails silently, send hits a revoked token.
            ("GET", "/settings/sendAs", 200, json!({ "sendAs": [] })),
            ("POST", "/messages/send", 401, json!({ "error": "auth" })),
            (
                "FORM",
                "oauth2.googleapis.com/token",
                400,
                json!({ "error": "invalid_grant", "error_description": "revoked" }),
            ),
        ]);
        let (app, _d, reg) = app_with(http, home.path());
        let (st, e) = send_req(
            &app,
            "POST",
            "/api/email/send",
            Some(json!({ "to": "x@customer.com", "subject": "s", "body": "b", "from": ACCT })),
            &[],
        )
        .await;
        assert_eq!(st, StatusCode::BAD_GATEWAY, "{e}");
        match reg.get("email") {
            Some(IntegrationState::Unavailable { reason }) => {
                assert!(reason.contains("invalid_grant"), "{reason}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    // ---- AMUX-3510: the external-send approval gate ------------------------
    //
    // Ethan's acceptance, in his words: "workers can never send emails to
    // people without explicit permissions." Each cell is a path that must
    // HOLD the send, next to the paths that must still flow — a
    // gate-everything bug and a gate-nothing bug look identical if only one
    // side is tested.

    #[tokio::test]
    async fn a_worker_external_send_is_frozen_not_sent() {
        let home = temp_home();
        let http = MockHttp::new(vec![
            // Deliberately includes a send answer: if the gate leaks, the mock
            // WOULD answer and the test still fails on the status — the mock
            // never masks a leak as a transport error.
            ("GET", "/messages?q=", 200, json!({ "messages": [] })),
            ("GET", "/settings/sendAs", 200, json!({ "sendAs": [] })),
            ("POST", "/messages/send", 200, json!({ "id": "leak", "threadId": "t" })),
        ]);
        let (app, _d, _r) = app_with(http.clone(), home.path());
        let (st, res) = send_req(
            &app,
            "POST",
            "/api/email/send",
            Some(json!({ "to": "hilmar.koch@autodesk.com", "subject": "Update",
                         "body": "hello", "from": ACCT })),
            &[("x-amux-session", "autodesk")],
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{res}");
        assert_eq!(res["code"], json!("approval_required"));
        let apr = res["approval_id"].as_str().unwrap().to_string();
        assert!(crate::api::email_approval::valid_id(&apr));
        assert_eq!(res["preview"]["external_recipients"][0], json!("hilmar.koch@autodesk.com"));
        // NOTHING reached the transport.
        assert!(
            http.calls.lock().unwrap().iter().all(|(m, u, _)| !(m == "POST" && u.contains("/send"))),
            "the gate must hold the send, not fire it"
        );
        // The hold is visible where a human looks: the approvals list and the
        // audited email log (a gated send nobody approves must leave a trace).
        let (_, lst) = send_req(&app, "GET", "/api/email/approvals", None, &[]).await;
        assert_eq!(lst["pending"][0]["id"], json!(apr));
        assert_eq!(lst["pending"][0]["session"], json!("autodesk"));
        let log = crate::integrations::email::read_email_log(home.path(), 1, 50, "");
        let held = log["log"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x.get("blocked") == Some(&json!("approval_required")))
            .expect("the hold must be a ledger row");
        assert_eq!(held["approval_id"], json!(apr));
        assert_eq!(held["session"], json!("autodesk"));
    }

    #[tokio::test]
    async fn a_human_send_and_an_exempt_lane_still_flow() {
        // Human context (no worker origin): external sends flow — the
        // dashboard composer is Ethan himself.
        let home = temp_home();
        let http = MockHttp::new(vec![
            ("GET", "/messages?q=", 200, json!({ "messages": [] })),
            ("GET", "/settings/sendAs", 200, json!({ "sendAs": [] })),
            ("POST", "/messages/send", 200, json!({ "id": "m9", "threadId": "t9" })),
        ]);
        let (app, _d, _r) = app_with(http, home.path());
        let (st, res) = send_req(
            &app,
            "POST",
            "/api/email/send",
            Some(json!({ "to": "ceo@customer.com", "subject": "Hi", "body": "b",
                         "from": ACCT, "force_new_thread": true })),
            &[],
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{res}");
        assert_eq!(res["id"], json!("m9"));

        // Exempt lane (server.env, Ethan's one line): flows with its origin.
        let home2 = temp_home();
        std::fs::write(home2.path().join("server.env"), "AMUX_EMAIL_EXTERNAL_EXEMPT=gtm-ticker\n")
            .unwrap();
        let http2 = MockHttp::new(vec![
            ("GET", "/messages?q=", 200, json!({ "messages": [] })),
            ("GET", "/settings/sendAs", 200, json!({ "sendAs": [] })),
            ("POST", "/messages/send", 200, json!({ "id": "m10", "threadId": "t10" })),
        ]);
        let (app2, _d2, _r2) = app_with(http2, home2.path());
        let (st, res) = send_req(
            &app2,
            "POST",
            "/api/email/send",
            Some(json!({ "to": "lead@prospect.com", "subject": "Hi", "body": "b",
                         "from": ACCT, "force_new_thread": true })),
            &[("x-amux-session", "gtm-ticker")],
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{res}");
    }

    /// AMUX-3698: a human can DISCARD a held draft, and it is recorded.
    ///
    /// Ethan's screenshot is the specimen: his approvals banner held two of
    /// autodesk's `example.invalid` gate probes and the only button was
    /// "Approve & send". The available moves were approve a worker's test email
    /// or wait an hour — ethos rule 3 on a human-facing surface.
    #[tokio::test]
    async fn a_held_draft_can_be_discarded_and_the_discard_is_audited() {
        let home = temp_home();
        let http = MockHttp::new(vec![
            ("GET", "/settings/sendAs", 200, json!({ "sendAs": [] })),
            ("POST", "/messages/send", 200, json!({ "id": "n", "threadId": "t" })),
        ]);
        let (app, _d, _r) = app_with(http, home.path());
        let (st, res) = send_req(
            &app,
            "POST",
            "/api/email/send",
            Some(json!({ "to": "gate-probe@example.invalid", "subject": "probe", "body": "b",
                         "from": ACCT, "force_new_thread": true })),
            &[("x-amux-session", "autodesk")],
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "premise: held: {res}");
        let id = res["approval_id"].as_str().expect("approval_id").to_string();

        let (st, ok) = send_req(
            &app,
            "POST",
            &format!("/api/email/reject/{id}"),
            Some(json!({ "reason": "worker probe, not a real send" })),
            &[("x-amux-approver", "dashboard")],
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{ok}");
        assert_eq!(ok["rejected"], json!(true), "{ok}");
        assert_eq!(ok["was_for_session"], json!("autodesk"), "{ok}");

        // IT MUST NOT SEND. The whole point of a discard is that the draft dies
        // unsent, and the MockHttp queue above would have served a send.
        let log = crate::integrations::email::read_email_log(home.path(), 1, 50, "");
        let rows = log["log"].as_array().unwrap();
        assert!(
            !rows.iter().any(|r| r.get("approved") == Some(&json!(true))),
            "a discard must not release the draft: {rows:?}"
        );
        let rej = rows
            .iter()
            .find(|r| r.get("rejected") == Some(&json!(true)))
            .expect("the discard must be a ledger row — 'why was this not sent' was \
                     unanswerable for ANY unsent draft before this");
        assert_eq!(rej["rejected_by"], json!("dashboard"));
        assert_eq!(rej["reason"], json!("worker probe, not a real send"));

        // ONE-SHOT, and the follow-up says WHICH terminal state it reached
        // rather than enumerating three (the AUTOD-48 exculpation shape).
        let (st, e) = send_req(
            &app,
            "POST",
            &format!("/api/email/reject/{id}"),
            None,
            &[("x-amux-approver", "dashboard")],
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{e}");
        assert!(
            e["error"].as_str().unwrap().contains("DISCARDED"),
            "the directory knows which end it met — say it: {e}"
        );
        // AND IT NAMES WHO, rather than inventing a human (autodesk, third
        // instance). The first version of this sentence read "a human rejected
        // it" — while the ledger row for a headerless discard correctly says
        // `rejected_by: "unidentified"`. The message asserted what the row
        // refuses to, and pointed at that row as proof.
        assert!(
            e["error"].as_str().unwrap().contains("'dashboard'"),
            "name the rejecter this doc actually recorded: {e}"
        );
        assert!(
            e["error"].as_str().unwrap().contains("CLAIMED, not verified"),
            "and say the identity is unverified, because nothing here checked it: {e}"
        );

        // THE UNIDENTIFIED CASE, which is the one that produced the false
        // sentence: a headerless reject is permitted (it fails safe — refusing
        // to send destroys nothing that was going to be sent) and must NOT be
        // narrated as a human.
        let (st, res2) = send_req(
            &app,
            "POST",
            "/api/email/send",
            Some(json!({ "to": "someone@customer.com", "subject": "s2", "body": "b",
                         "from": ACCT, "force_new_thread": true })),
            &[("x-amux-session", "autodesk")],
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{res2}");
        let id2 = res2["approval_id"].as_str().unwrap().to_string();
        let (st, _) =
            send_req(&app, "POST", &format!("/api/email/reject/{id2}"), None, &[]).await;
        assert_eq!(st, StatusCode::OK, "a headerless discard fails safe and is allowed");
        let (_, e3) =
            send_req(&app, "POST", &format!("/api/email/reject/{id2}"), None, &[]).await;
        let msg = e3["error"].as_str().unwrap();
        assert!(
            !msg.contains("a human"),
            "NOBODY was identified — the sentence must not invent one: {msg}"
        );
        assert!(msg.contains("not known"), "say so plainly instead: {msg}");

        // AND APPROVE MUST REFUSE IT AFTERWARDS. A discarded draft that could
        // still be released would make the button a suggestion.
        let (st, e2) = send_req(
            &app,
            "POST",
            &format!("/api/email/approve/{id}"),
            None,
            &[("x-amux-approver", "dashboard")],
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{e2}");
    }

    /// AUTOD-48: the gate was bypassable in two calls by the lane it stops, and
    /// the bypass logged as human-approved.
    ///
    /// autodesk's measurement against the live server, 743ms apart in one shell:
    ///
    /// ```text
    /// 12:51:06  POST /api/email/send      X-Amux-Session: autodesk  -> 403, apr_5da6...
    /// 12:51:07  POST /api/email/approve/apr_5da6...   no headers    -> 200 approved:true
    /// ```
    ///
    /// The ledger row read `approved_by: "dashboard (no worker origin)"`. That
    /// is the half that matters: the send-path hole degrades to
    /// `session='unattributed'`, a flag a reader can act on, while this one
    /// produced an affirmative false exculpation. Anyone auditing the incident
    /// class would have read that row and cleared the lane.
    ///
    /// Every cell below is one of the seven variants autodesk said they would
    /// probe the moment the binary was live. Cheaper to cover in one pass.
    /// A lane's send, HELD pending approval, with its approval id.
    ///
    /// Was a closure inside `approve_refuses_an_absent_or_self_named_approver`;
    /// lifted to module scope by AMUX-3699 so the contradiction test drives the
    /// same fixture rather than a second copy that could drift from it.
    async fn held(
        name: &'static str,
    ) -> (axum::Router, tempfile::TempDir, tempfile::TempDir, String) {
        let home = temp_home();
        let http = MockHttp::new(vec![
            ("GET", "/settings/sendAs", 200, json!({ "sendAs": [] })),
            ("POST", "/messages/send", 200, json!({ "id": "x", "threadId": "t" })),
        ]);
        let (app, d, _r) = app_with(http, home.path());
        let (st, res) = send_req(
            &app,
            "POST",
            "/api/email/send",
            Some(json!({ "to": "cto@customer.com", "subject": "Q", "body": "draft",
                         "from": ACCT, "force_new_thread": true })),
            &[("x-amux-session", name)],
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "premise: the send must be HELD: {res}");
        let id = res["approval_id"].as_str().expect("approval_id").to_string();
        (app, d, home, id)
    }

    #[tokio::test]
    async fn approve_refuses_an_absent_or_self_named_approver() {
        // THE ORIGINAL BYPASS: no headers at all.
        let (app, _d, _h, id) = held("autodesk").await;
        let (st, e) =
            send_req(&app, "POST", &format!("/api/email/approve/{id}"), None, &[]).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{e}");
        assert_eq!(e["code"], json!("approver_unidentified"), "{e}");

        // VARIANTS 1 and 3: emptiness must fail CLOSED, on every header that
        // participates. An empty `x-amux-session` reads as None to hdr_worker,
        // so before this fix it was a one-character bypass; an empty approver
        // must not satisfy the new check either.
        for hdrs in [
            vec![("x-amux-session", "")],
            vec![("x-amux-session", "   ")],
            vec![("x-amux-worker", "")],
            vec![("x-amux-approver", "")],
            vec![("x-amux-approver", "  ")],
            vec![("x-amux-session", ""), ("x-amux-approver", " ")],
        ] {
            let (app, _d, _h, id) = held("autodesk").await;
            let (st, e) =
                send_req(&app, "POST", &format!("/api/email/approve/{id}"), None, &hdrs).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "empty/blank must fail closed: {hdrs:?} -> {e}");
            assert_eq!(e["code"], json!("approver_unidentified"), "{hdrs:?} -> {e}");
        }

        // VARIANT 5: creator may not be approver, even WITH a marker. The name
        // is not a secret, so this is the independent gate that survives the
        // marker being guessed or copied. Case-insensitive, because a name is
        // not a password and `Autodesk` is the same lane.
        //
        // AND THE REFUSAL MUST NOT DESTROY THE DRAFT. The first version of this
        // check ran AFTER `consume`, so it burned the row it refused: any lane
        // could delete any pending draft by naming its creator back at the
        // server (the pending list hands out both), and a lane that
        // self-approved ONCE permanently destroyed the message a human was
        // meant to review. autodesk bisected it — pending before=True,
        // after=FALSE — and nothing else in this file could have told us,
        // because the refusal wrote no ledger row either.
        for who in ["autodesk", "AutoDesk"] {
            let (app, _d, _h, id) = held("autodesk").await;
            let (st, e) = send_req(
                &app,
                "POST",
                &format!("/api/email/approve/{id}"),
                None,
                &[("x-amux-approver", who)],
            )
            .await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{who} -> {e}");
            assert_eq!(e["code"], json!("creator_cannot_approve"), "{who} -> {e}");

            // THE DRAFT SURVIVES. This is the cell that would have caught the
            // defect, and the one the original test could not: it asserts on
            // the SIDE EFFECT of a refusal, not on its status code.
            let (st2, ok) = send_req(
                &app,
                "POST",
                &format!("/api/email/approve/{id}"),
                None,
                &[("x-amux-approver", "dashboard")],
            )
            .await;
            assert_eq!(
                st2,
                StatusCode::OK,
                "a refused self-approval must leave the draft PENDING so a human can still \
                 release it — {who} -> {ok}"
            );
        }

        // THE CONTROL, and without it every cell above passes against a gate
        // that refuses everything and no mail could ever be released.
        let (app, _d, _h, id) = held("autodesk").await;
        let (st, ok) = send_req(
            &app,
            "POST",
            &format!("/api/email/approve/{id}"),
            None,
            &[("x-amux-approver", "dashboard")],
        )
        .await;
        assert_eq!(st, StatusCode::OK, "a NAMED approver who is not the creator must succeed: {ok}");
        assert_eq!(ok["approved"], json!(true), "{ok}");
    }

    /// AMUX-3699: the worker-origin refusal must cover EVERY header spelling
    /// `hdr_worker` knows, or it narrows silently and the bypass widens.
    ///
    /// WHAT INVESTIGATING THIS CARD ACTUALLY FOUND. The card's residual reads
    /// "the two-call bypass becomes a three-header bypass: a lane sends
    /// `X-Amux-Approver: dashboard` and the creator check passes because
    /// `autodesk != dashboard`". True as far as it goes, and it understates one
    /// guard: AMUX-3510 refuses ANY request carrying a worker origin, at the
    /// top of `approve`, before the approver is even read. So a lane cannot
    /// merely add a third header — it must also SUPPRESS its own origin. Two
    /// independent things, not one.
    ///
    /// That is still not a boundary (a lane omits headers at will, and this is
    /// self-attestation in both directions), which is why the card stays open
    /// against a precondition amux does not have. But the guard that DOES
    /// exist is worth keeping honest, and it was covered for exactly one of the
    /// two header names `hdr_worker` reads.
    ///
    /// The list is asserted against `hdr_worker` itself rather than restated,
    /// so a third origin header added there fails HERE instead of quietly
    /// creating a spelling the refusal does not see.
    #[tokio::test]
    async fn the_worker_origin_refusal_covers_every_header_hdr_worker_reads() {
        // The guard's own source of truth. A restated copy could drift from the
        // function it is supposed to mirror, which is how a view and its
        // mechanism disagree (ethos rule 1).
        for spelling in ["x-amux-worker", "x-amux-session"] {
            let mut h = HeaderMap::new();
            h.insert(spelling, "autodesk".parse().unwrap());
            assert_eq!(
                hdr_worker(&h).as_deref(),
                Some("autodesk"),
                "premise: hdr_worker reads {spelling}"
            );

            let (app, _d, _home, id) = held("autodesk").await;
            let (st, e) = send_req(
                &app,
                "POST",
                &format!("/api/email/approve/{id}"),
                None,
                // The full bypass attempt: assert a human identity AND carry
                // the origin. The origin is what must stop it.
                &[("x-amux-approver", "dashboard"), (spelling, "autodesk")],
            )
            .await;
            assert_eq!(
                st,
                StatusCode::FORBIDDEN,
                "a lane claiming to be the dashboard while stamping {spelling} must be \
                 refused, not merely logged: {e}"
            );
            assert_eq!(e["code"], json!("worker_cannot_approve"), "{spelling}: {e}");
        }

        // THE CONTROL. Without it, a guard that refused every approval would
        // pass both cells above and no mail could ever be released — and the
        // residual this card documents would read as closed when it is not.
        let (app, _d, _home, id) = held("autodesk").await;
        let (st, ok) = send_req(
            &app,
            "POST",
            &format!("/api/email/approve/{id}"),
            None,
            &[("x-amux-approver", "dashboard")], // a browser sends no origin
        )
        .await;
        assert_eq!(
            st,
            StatusCode::OK,
            "an origin-less caller claiming to be the dashboard still succeeds — that IS the \
             open residual, and asserting it stops this test reading as a boundary: {ok}"
        );
    }

    #[tokio::test]
    async fn approve_is_human_only_one_shot_and_audited() {
        let home = temp_home();
        let http = MockHttp::new(vec![
            ("GET", "/settings/sendAs", 200, json!({ "sendAs": [] })),
            ("POST", "/messages/send", 200, json!({ "id": "rel1", "threadId": "tr1" })),
        ]);
        let (app, _d, _r) = app_with(http, home.path());
        let (st, res) = send_req(
            &app,
            "POST",
            "/api/email/send",
            Some(json!({ "to": "cto@customer.com", "subject": "Q", "body": "draft",
                         "from": ACCT, "force_new_thread": true })),
            &[("x-amux-session", "autodesk")],
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let apr = res["approval_id"].as_str().unwrap().to_string();

        // A worker cannot approve — its own or anyone's (the exact loop the
        // gate exists to break).
        let (st, e) = send_req(
            &app,
            "POST",
            &format!("/api/email/approve/{apr}"),
            None,
            &[("x-amux-session", "autodesk")],
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert_eq!(e["code"], json!("worker_cannot_approve"));

        // THE HUMAN RELEASE NAMES ITSELF (AUTOD-48). This call used to send NO
        // headers and expect 200 — so the cell named "approve_is_human_only"
        // was, precisely, the assertion that an unidentified caller is a human.
        // It certified the bypass for as long as it was green.
        //
        // Kept and corrected rather than deleted: the rest of what it pins
        // (frozen draft released, both halves audited, one-shot) is real and
        // still worth having. `approve_refuses_an_absent_or_self_named_approver`
        // covers the refusals.
        let (st, ok) = send_req(
            &app,
            "POST",
            &format!("/api/email/approve/{apr}"),
            None,
            &[("x-amux-approver", "dashboard")],
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{ok}");
        assert_eq!(ok["approved"], json!(true));
        assert_eq!(ok["id"], json!("rel1"));
        assert_eq!(ok["sent_for_session"], json!("autodesk"));
        let log = crate::integrations::email::read_email_log(home.path(), 1, 50, "");
        let sent = log["log"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x.get("approved") == Some(&json!(true)))
            .expect("the approved send must be a ledger row");
        assert_eq!(sent["approval_id"], json!(apr));
        assert_eq!(sent["session"], json!("autodesk"), "the AUTHOR stays on the row");
        // The ledger records the CLAIM and says it is unverified. It used to
        // assert "dashboard (no worker origin)" on the strength of an absence,
        // which was an affirmative false exculpation in AUTOD-48's specimen.
        assert_eq!(sent["approved_by"], json!("dashboard"), "the claim, verbatim");
        assert_eq!(
            sent["approver_verified"],
            json!(false),
            "nothing here proved who released it, and the row must say so rather than \
             manufacturing a fact a forensic reader would clear the lane on"
        );

        // One-shot: the same approval cannot release twice. It carries the
        // marker so the request REACHES the consume step — the approver gate
        // runs first now, and a headerless retry would 403 before one-shot was
        // exercised at all, which would leave this cell green while testing
        // nothing about one-shot.
        let (st, _) = send_req(
            &app,
            "POST",
            &format!("/api/email/approve/{apr}"),
            None,
            &[("x-amux-approver", "dashboard")],
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_worker_reply_is_frozen_with_the_threads_resolved_fanout() {
        // The incident's vector: one reply call, recipients the caller never
        // named. The preview must carry the RESOLVED fan-out.
        let home = temp_home();
        let orig = json!({
            "id": "g1", "threadId": "T1",
            "payload": { "headers": [
                { "name": "Message-ID", "value": "<deal@adsk>" },
                { "name": "Subject", "value": "Rollout" },
                { "name": "From", "value": "hilmar.koch@autodesk.com" },
                { "name": "To", "value": format!("{ACCT}, jonathan.brooks@autodesk.com") },
            ]},
        });
        let http = MockHttp::new(vec![
            ("GET", "q=rfc822msgid", 200, json!({ "messages": [{ "id": "g1" }] })),
            ("GET", "/messages/g1", 200, orig),
            ("POST", "/messages/send", 200, json!({ "id": "leak", "threadId": "T1" })),
        ]);
        let (app, _d, _r) = app_with(http.clone(), home.path());
        let (st, res) = send_req(
            &app,
            "POST",
            "/api/email/reply",
            Some(json!({ "message_id": "<deal@adsk>", "body": "sounds good",
                         "from": ACCT, "reply_all": true })),
            &[("x-amux-worker", "autodesk")],
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{res}");
        assert_eq!(res["code"], json!("approval_required"));
        let ext = res["preview"]["external_recipients"].as_array().unwrap();
        assert_eq!(ext.len(), 2, "both thread participants the caller never named: {ext:?}");
        assert!(
            http.calls.lock().unwrap().iter().all(|(m, u, _)| !(m == "POST" && u.contains("/send"))),
            "the reply must be held, not sent"
        );
    }
}
