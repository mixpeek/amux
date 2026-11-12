//! A read-only mailbox kept as a file: `<AMUX_HOME>/mailboxes/<account>.json`.
//!
//! For an account with no Gmail connection: a demo or test household, an
//! export someone dropped in, or a mailbox amux should read and never send
//! from. The file is `{"account": "...", "messages": [{message_id, thread_id,
//! from, to, cc, subject, date (RFC 2822), body, labels}]}`. `/api/email/inbox`,
//! `/search` and `/message` read it beside the Gmail accounts with the same
//! response shapes. Nothing sends from it: `/send` and `/reply` never look
//! here, so an outside send from a file account is impossible by construction.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn dir(home: &Path) -> PathBuf {
    home.join("mailboxes")
}

/// Every file mailbox: (account, messages).
pub fn load_all(home: &Path) -> Vec<(String, Vec<Value>)> {
    let Ok(rd) = std::fs::read_dir(dir(home)) else { return vec![] };
    let mut out = vec![];
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&p) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&raw) else {
            tracing::warn!(path = %p.display(), verdict = "local_mailbox_unreadable", measured = true, n_considered = 1,
                "local mailbox: the file is not JSON and is skipped");
            continue;
        };
        let account = v["account"].as_str().map(str::to_string)
            .or_else(|| p.file_stem().and_then(|s| s.to_str()).map(str::to_string))
            .unwrap_or_default();
        let msgs = v["messages"].as_array().cloned().unwrap_or_default();
        out.push((account, msgs));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub fn accounts(home: &Path) -> Vec<String> {
    load_all(home).into_iter().map(|(a, _)| a).collect()
}

fn ts(m: &Value) -> i64 {
    m["date"].as_str().and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok()).map(|d| d.timestamp()).unwrap_or(0)
}

/// The list-row shape the Gmail path returns (no body; a snippet).
fn summary(account: &str, m: &Value) -> Value {
    let body = m["body"].as_str().unwrap_or("");
    json!({
        "message_id": m["message_id"], "id": m["message_id"], "thread_id": m["thread_id"],
        "from": m["from"], "to": m["to"], "cc": m["cc"], "subject": m["subject"], "date": m["date"],
        "snippet": body.chars().take(200).collect::<String>(), "account": account,
        "labels": m["labels"], "source": "file",
    })
}

fn within(m: &Value, days: f64, now: i64) -> bool {
    days <= 0.0 || ts(m) >= now - (days * 86400.0) as i64
}

/// Newest first, optionally one account.
pub fn inbox(home: &Path, account: &str, count: usize, days: f64) -> Vec<Value> {
    let now = chrono::Utc::now().timestamp();
    let mut rows: Vec<(i64, Value)> = load_all(home)
        .into_iter()
        .filter(|(a, _)| account.is_empty() || a == account)
        .flat_map(|(a, msgs)| msgs.into_iter().filter(|m| within(m, days, now)).map(move |m| (ts(&m), summary(&a, &m))).collect::<Vec<_>>())
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    rows.into_iter().take(count).map(|(_, v)| v).collect()
}

/// Gmail-style words: every term must appear in the subject, sender,
/// recipients or body (case-insensitive). `from:x` and `subject:x` narrow to
/// that field; Gmail operators this file cannot answer (`newer_than:`, `in:`,
/// `label:`) are ignored rather than matched as words.
pub fn matches(m: &Value, q: &str) -> bool {
    let field = |k: &str| m[k].as_str().unwrap_or("").to_lowercase();
    let all = format!("{} {} {} {} {}", field("subject"), field("from"), field("to"), field("cc"), field("body"));
    q.split_whitespace().all(|t| {
        let t = t.to_lowercase();
        let t = t.trim_matches('"');
        if let Some(v) = t.strip_prefix("from:") {
            return field("from").contains(v);
        }
        if let Some(v) = t.strip_prefix("subject:") {
            return field("subject").contains(v);
        }
        if ["newer_than:", "older_than:", "in:", "label:", "is:"].iter().any(|p| t.starts_with(p)) {
            return true;
        }
        if t == "or" || t.is_empty() {
            return true;
        }
        all.contains(t)
    })
}

pub fn search(home: &Path, q: &str, account: &str, limit: usize, days: i64) -> Vec<Value> {
    let now = chrono::Utc::now().timestamp();
    let mut rows: Vec<(i64, Value)> = load_all(home)
        .into_iter()
        .filter(|(a, _)| account.is_empty() || a == account)
        .flat_map(|(a, msgs)| {
            msgs.into_iter()
                .filter(|m| within(m, days as f64, now) && matches(m, q))
                .map(move |m| (ts(&m), summary(&a, &m)))
                .collect::<Vec<_>>()
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    rows.into_iter().take(limit).map(|(_, v)| v).collect()
}

/// One message with its body, by Message-ID.
pub fn message(home: &Path, id: &str) -> Option<Value> {
    let id = id.trim().trim_start_matches('<').trim_end_matches('>');
    for (a, msgs) in load_all(home) {
        for m in msgs {
            let mid = m["message_id"].as_str().unwrap_or("").trim_start_matches('<').trim_end_matches('>').to_string();
            if mid == id {
                let mut v = summary(&a, &m);
                v["body"] = m["body"].clone();
                v["attachments"] = json!([]);
                return Some(v);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(msgs: Value) -> tempfile::TempDir {
        let h = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(h.path().join("mailboxes")).unwrap();
        std::fs::write(h.path().join("mailboxes/family.json"),
            json!({"account": "dana@family.test", "messages": msgs}).to_string()).unwrap();
        h
    }

    #[test]
    fn search_needs_every_word_and_ignores_gmail_operators() {
        let now = chrono::Utc::now().to_rfc2822();
        let h = home_with(json!([
            {"message_id": "<a@x>", "from": "Camp Kaylie <office@campkaylie.test>", "subject": "Waitlist update for Rafi", "date": now, "body": "Rafi is #3 on the waitlist"},
            {"message_id": "<b@x>", "from": "School <office@school.test>", "subject": "Application received", "date": now, "body": "We received Ezra's application"},
        ]));
        let r = search(h.path(), "rafi waitlist newer_than:30d", "", 10, 30);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["subject"], "Waitlist update for Rafi");
        assert_eq!(r[0]["account"], "dana@family.test");
        assert!(r[0].get("body").is_none(), "a list row carries a snippet, not the body");
        assert_eq!(search(h.path(), "from:school", "", 10, 30).len(), 1);
        assert!(search(h.path(), "rafi", "someone@else", 10, 30).is_empty());
        assert_eq!(message(h.path(), "a@x").unwrap()["body"], "Rafi is #3 on the waitlist");
    }

    #[test]
    fn the_window_and_order_hold() {
        let old = (chrono::Utc::now() - chrono::Duration::days(40)).to_rfc2822();
        let new = chrono::Utc::now().to_rfc2822();
        let h = home_with(json!([
            {"message_id": "<old@x>", "subject": "old", "date": old, "body": ""},
            {"message_id": "<new@x>", "subject": "new", "date": new, "body": ""},
        ]));
        let r = inbox(h.path(), "", 10, 7.0);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["subject"], "new");
        assert_eq!(inbox(h.path(), "", 10, 0.0)[0]["subject"], "new", "newest first");
        assert_eq!(accounts(h.path()), vec!["dana@family.test".to_string()]);
    }
}
