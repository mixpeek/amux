//! Email to yourself as a line into amux, routed by `self_chat`.
//!
//! - Receive: every 60 s, the owner's Gmail account (AMUX_BRIDGE_EMAIL, else
//!   AMUX_OWNER_EMAIL; never a guessed account) is searched for
//!   mail from the owner to the owner. A message whose subject (or, with no
//!   subject, first line) starts with `@` or `#` is a new post: subject and
//!   body together, signature and quoted text cut. A reply in a thread that
//!   started that way is a reply in its Messages thread, so the worker there
//!   gets it without a new @.
//! - Send: answers go out as a reply in the same Gmail thread through the
//!   amux email API (`api::email::reply`), so they are in the send log, and a
//!   recipient other than the owner would be held like any other send.
//! - Every message seen and every reply sent is kept in
//!   ~/.amux/bridge-email.json, so a restart neither repeats nor files the
//!   bridge's own replies as the owner's. The first read only records what is
//!   there.
//!
//! Off with AMUX_BRIDGE_EMAIL=0. Verdicts: bridge_email_filed, bridge_email_state_unsaved,
//! bridge_email_read_failed, bridge_email_off.

use super::{setting, Inbound};
use crate::integrations::email::GmailClient;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const PLATFORM: &str = "email";
const KEEP: usize = 500;

fn state_path() -> PathBuf {
    crate::config::amux_home().join("bridge-email.json")
}

#[derive(Default)]
struct Saved {
    started: bool,
    seen: Vec<String>,
    sent: Vec<String>,
    /// Gmail thread id -> (root post id, the root email's Message-ID).
    threads: Vec<(String, i64, String)>,
}

fn saved() -> &'static Mutex<Option<Saved>> {
    static S: Mutex<Option<Saved>> = Mutex::new(None);
    &S
}

fn with_saved<T>(f: impl FnOnce(&mut Saved) -> T) -> T {
    let mut g = saved().lock().unwrap_or_else(|e| e.into_inner());
    let s = g.get_or_insert_with(|| {
        let v: Value = std::fs::read_to_string(state_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}));
        let strs = |k: &str| v[k].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
        Saved {
            started: v["started"].as_bool().unwrap_or(false),
            seen: strs("seen"),
            sent: strs("sent"),
            threads: v["threads"].as_array().map(|a| a.iter().filter_map(|t| Some((t[0].as_str()?.to_string(),
                t[1].as_i64()?, t[2].as_str()?.to_string()))).collect()).unwrap_or_default(),
        }
    });
    let out = f(s);
    for l in [&mut s.seen, &mut s.sent] {
        if l.len() > KEEP {
            l.drain(..l.len() - KEEP);
        }
    }
    if s.threads.len() > KEEP {
        s.threads.drain(..s.threads.len() - KEEP);
    }
    let v = json!({"started": s.started, "seen": s.seen, "sent": s.sent,
        "threads": s.threads.iter().map(|(t, p, m)| json!([t, p, m])).collect::<Vec<_>>()});
    if let Err(e) = std::fs::write(state_path(), v.to_string()) {
        // Unsaved, a restart could file an email again or read a reply as the owner's.
        tracing::warn!(error = %e, measured = true, n_considered = 1, verdict = "bridge_email_state_unsaved",
            "bridge: what the email bridge has seen and sent could not be saved");
    }
    out
}

fn client() -> GmailClient {
    GmailClient::new_default()
}

/// The owner's mailbox, if it is a connected Gmail account. Only an address
/// the owner named: mail "from me to me" in any other connected account (a
/// shared inbox, a test fixture) is not the owner speaking, and reading it
/// spends that account's token refreshes.
pub fn account() -> Option<String> {
    if setting("AMUX_BRIDGE_EMAIL").as_deref() == Some("0") {
        return None;
    }
    let want = setting("AMUX_BRIDGE_EMAIL").or_else(|| setting("AMUX_OWNER_EMAIL"))?;
    client().connected_accounts().into_iter().find(|a| a.eq_ignore_ascii_case(&want))
}

pub fn why_off() -> Option<String> {
    if setting("AMUX_BRIDGE_EMAIL").as_deref() == Some("0") {
        return Some("AMUX_BRIDGE_EMAIL=0".into());
    }
    if setting("AMUX_BRIDGE_EMAIL").or_else(|| setting("AMUX_OWNER_EMAIL")).is_none() {
        return Some("neither AMUX_BRIDGE_EMAIL nor AMUX_OWNER_EMAIL is set".into());
    }
    account().is_none().then(|| "the owner's address (AMUX_BRIDGE_EMAIL or AMUX_OWNER_EMAIL) is not a connected Gmail account".into())
}

pub fn enabled() -> bool {
    why_off().is_none()
}

fn addr(s: &str) -> String {
    let s = s.trim();
    s.rsplit_once('<').map(|(_, a)| a.trim_end_matches('>')).unwrap_or(s).trim().to_lowercase()
}

/// A subject without its Re:/Fwd: prefixes, and whether it had any.
pub fn base_subject(s: &str) -> (String, bool) {
    let mut t = s.trim();
    let mut was_reply = false;
    loop {
        let low = t.to_lowercase();
        let cut = ["re:", "fw:", "fwd:"].iter().find(|p| low.starts_with(**p)).map(|p| p.len());
        match cut {
            Some(n) => { t = t[n..].trim_start(); was_reply = true; }
            None => return (t.to_string(), was_reply),
        }
    }
}

/// What the owner wrote: the body up to their signature or the quoted mail.
pub fn own_words(body: &str) -> String {
    let mut out = Vec::new();
    let body = body.replace("\r\n", "\n");
    for line in body.lines() {
        let t = line.trim_end();
        if t == "--" || t.starts_with('>') || (t.starts_with("On ") && t.ends_with("wrote:")) {
            break;
        }
        out.push(t);
    }
    out.join("\n").trim().to_string()
}

/// A new post from an email: the subject is the command when it starts with
/// @ or #, and the body follows it; with no such subject, the body's first
/// line must be.
pub fn command_text(subject: &str, body: &str) -> Option<String> {
    let (subj, _) = base_subject(subject);
    let words = own_words(body);
    if super::self_chat::is_command(&subj) {
        return Some(if words.is_empty() { subj } else { format!("{subj}\n\n{words}") });
    }
    super::self_chat::is_command(&words).then_some(words)
}

pub async fn poll_once(state: &crate::api::AppState) -> Result<usize, String> {
    let acct = account().ok_or("no account")?;
    let c = client();
    let list = c.inbox_messages(&acct, 25, "from:me to:me newer_than:2d", 0.0, None).await?;
    let msgs: Vec<Value> = list["messages"].as_array().cloned().unwrap_or_default();
    let mine = |m: &Value| addr(m["from"].as_str().unwrap_or("")) == acct.to_lowercase()
        && m["to"].as_str().unwrap_or("").to_lowercase().contains(&acct.to_lowercase());
    let started = with_saved(|s| s.started);
    if !started {
        with_saved(|s| {
            s.seen.extend(msgs.iter().filter_map(|m| m["gmail_id"].as_str().map(str::to_string)));
            s.started = true;
        });
        return Ok(0);
    }
    let mut n = 0;
    // Oldest first, so a follow-up is filed after the message it answers.
    for m in msgs.iter().rev() {
        let gid = m["gmail_id"].as_str().unwrap_or("").to_string();
        if gid.is_empty() || with_saved(|s| s.seen.contains(&gid) || s.sent.contains(&gid)) {
            continue;
        }
        // Marked seen only once it is settled: a Gmail error before then
        // leaves it for the next read, so a command is never dropped.
        let settle = |gid: &str| { let g = gid.to_string(); with_saved(|s| s.seen.push(g)) };
        if !mine(m) {
            settle(&gid);
            continue;
        }
        let thread = m["thread_id"].as_str().unwrap_or("").to_string();
        let (_, is_reply) = base_subject(m["subject"].as_str().unwrap_or(""));
        let root = with_saved(|s| s.threads.iter().find(|t| t.0 == thread).cloned());
        if root.is_none() && is_reply && !super::self_chat::is_command(&base_subject(m["subject"].as_str().unwrap_or("")).0) {
            settle(&gid);
            continue;
        }
        let full = c.get_message_full(&acct, &gid).await?;
        let body = full["body"].as_str().unwrap_or("");
        let msgid = full["message_id"].as_str().unwrap_or("").to_string();
        let (text, parent, reply_to) = match &root {
            Some((_, post, root_msgid)) if is_reply => {
                let w = own_words(body);
                if w.is_empty() || with_saved(|s| s.sent.contains(&w)) {
                    settle(&gid);
                    continue;
                }
                (w, Some(*post), root_msgid.clone())
            }
            _ => match command_text(m["subject"].as_str().unwrap_or(""), body) {
                Some(t) => (t, None, msgid.clone()),
                None => {
                    settle(&gid);
                    continue;
                }
            },
        };
        let inbound = Inbound { platform: PLATFORM.into(), conversation: reply_to.clone(), is_group: false,
            sender_id: acct.clone(), sender_name: "owner".into(), text, platform_msg_id: gid.clone() };
        let out = super::self_chat::file(state, inbound, parent).await;
        if settles(&out) {
            settle(&gid);
        }
        if let (super::Outcome::Posted { post_id, .. }, None) = (&out, parent) {
            let (t, p, r) = (thread.clone(), *post_id, reply_to.clone());
            with_saved(|s| s.threads.push((t, p, r)));
        }
        tracing::info!(gmail_id = %gid, outcome = ?out, reply = parent.is_some(), measured = true, n_considered = 1,
            verdict = "bridge_email_filed", "bridge: an email the owner sent themselves went through the bridge");
        n += 1;
    }
    Ok(n)
}

/// A message is done with unless its post failed to store; then the next
/// read files it again (`self_chat::file` drops a repeat that did land).
fn settles(out: &super::Outcome) -> bool {
    !matches!(out, super::Outcome::Ignored { why: "post_failed" })
}

/// Reply in the thread of the owner's email `message_id`.
pub async fn send(message_id: &str, text: &str) -> Result<String, String> {
    let acct = account().ok_or("final: the owner's Gmail account is not connected")?;
    let ctx = Arc::new(crate::api::email::EmailCtx {
        client: Arc::new(client()),
        registry: crate::integrations::global_registry().clone(),
    });
    // Known before it is sent: the poll may list the reply before this returns.
    let t = text.trim().to_string();
    with_saved(|s| s.sent.push(t));
    let mut h = axum::http::HeaderMap::new();
    h.insert("x-amux-session", axum::http::HeaderValue::from_static("bridge"));
    let body = json!({"message_id": message_id, "body": text, "from": acct, "allow_self": true, "signature": false});
    let resp = crate::api::email::reply(axum::Extension(ctx), h, axum::Json(body)).await;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap_or_default();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    if !(status.is_success() && v["delivered"].as_bool() == Some(true)) {
        // Held for approval, or refused: either way not to be typed again.
        let why = v["error"].as_str().unwrap_or("not delivered");
        return Err(if status.as_u16() == 403 { format!("final: HTTP 403 {why}") } else { format!("HTTP {}: {why}", status.as_u16()) });
    }
    let id = v["id"].as_str().unwrap_or("").to_string();
    let i = id.clone();
    with_saved(|s| if !i.is_empty() { s.sent.push(i) });
    Ok(if id.is_empty() { format!("email:{}", super::now_ms()) } else { id })
}

pub async fn run(state: crate::api::AppState) {
    let (mut last_err, mut last_off) = (String::new(), String::new());
    loop {
        let off = why_off();
        if let Some(why) = &off {
            if *why != last_off {
                tracing::info!(reason = %why, measured = true, n_considered = 1, verdict = "bridge_email_off",
                    "bridge: email to yourself is not being read");
                last_off = why.clone();
            }
        } else {
            last_off.clear();
            match poll_once(&state).await {
                Ok(_) => last_err.clear(),
                Err(e) => {
                    if e != last_err {
                        tracing::warn!(error = %e, measured = true, n_considered = 1, verdict = "bridge_email_read_failed",
                            "bridge: reading email the owner sent themselves failed");
                        last_err = e;
                    }
                }
            }
        }
        let wait = wait_for(off.is_none());
        crate::runtime_jobs::registry::tick_every(crate::runtime_jobs::registry::ids::BRIDGE_EMAIL, wait);
        tokio::time::sleep(wait).await;
    }
}

fn wait_for(on: bool) -> Duration {
    Duration::from_secs(if on { 60 } else { 300 })
}

pub fn spawn(state: crate::api::AppState) -> tokio::task::JoinHandle<()> {
    crate::runtime_jobs::registry::spawn_loop(crate::runtime_jobs::registry::ids::BRIDGE_EMAIL,
        Some(Duration::from_secs(300)), move || run(state.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_subject_or_first_line_is_the_command() {
        assert_eq!(command_text("@amux deploy status", "Is it out?\n\n--\nEthan").as_deref(), Some("@amux deploy status\n\nIs it out?"));
        assert_eq!(command_text("", "#ops the build is red\n> quoted").as_deref(), Some("#ops the build is red"));
        assert_eq!(command_text("Fwd: @amux look at this", "").as_deref(), Some("@amux look at this"));
        assert_eq!(command_text("amux.io funnel, week of 2026-11-09", "Weekly funnel..."), None, "amux's own reports are left alone");
        assert_eq!(command_text("Note to self", "@ 5pm call mom"), None);
    }

    #[test]
    fn a_reply_keeps_only_what_the_owner_wrote() {
        assert_eq!(base_subject("Re: RE: @amux x"), ("@amux x".to_string(), true));
        assert_eq!(base_subject("@amux x"), ("@amux x".to_string(), false));
        assert_eq!(own_words("ship it\r\n\r\nOn Mon, Nov 9, 2026 at 1:00 PM Ethan <e@x.com> wrote:\r\n> amux: done"), "ship it");
        assert_eq!(addr("Ethan Steininger <Ethan@Mixpeek.com>"), "ethan@mixpeek.com");
    }

    #[test]
    fn only_a_post_that_failed_to_store_is_read_again() {
        assert!(!settles(&super::super::Outcome::Ignored { why: "post_failed" }));
        assert!(settles(&super::super::Outcome::Ignored { why: "unroutable" }), "a bad name is answered once, not every minute");
        assert!(settles(&super::super::Outcome::Duplicate));
        assert!(settles(&super::super::Outcome::Posted { post_id: 1, delivering_to: 1 }));
    }

    #[test]
    fn the_loop_declares_the_wait_it_keeps() {
        assert_eq!(wait_for(true), Duration::from_secs(60));
        assert_eq!(wait_for(false), Duration::from_secs(300));
    }
}
