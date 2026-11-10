//! iMessage through Messages on this Mac: the owner's thread with their own
//! number (text yourself) is a line into amux, routed by `self_chat`.
//!
//! - Receive: Messages keeps every thread in ~/Library/Messages/chat.db.
//!   Every 3 s this reads, read-only, the owner's own 1:1 thread (its handle
//!   is AMUX_OWNER_PHONE, or one of AMUX_IMESSAGE_SELF_HANDLES, which also
//!   takes an Apple ID email). Newer macOS leaves `text` empty and keeps the
//!   words in `attributedBody`, so both are read. A message starting with
//!   `@` or `#` is filed; the rest (amux's own texts to the owner, notes) is
//!   left alone. A message to yourself can be stored twice (sent and
//!   received), so the same words within two minutes are filed once.
//! - Send: `~/.amux/imessage/send-imessage.sh <handle> <text>`, the script
//!   the /imessage skill uses, into the same thread.
//!
//! Reading chat.db needs Full Disk Access for the server; without it the
//! read fails and says so once. Off with AMUX_IMESSAGE_BRIDGE=0.
//! Verdicts: bridge_imessage_filed, bridge_imessage_read_failed,
//! bridge_imessage_off.

use super::{setting, Inbound};
use rusqlite::OpenFlags;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

pub const PLATFORM: &str = "imessage";
const SAME_WORDS_WINDOW_S: f64 = 120.0;

fn db_path() -> PathBuf {
    setting("AMUX_IMESSAGE_DB").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Library/Messages/chat.db")
    })
}

fn script() -> PathBuf {
    setting("AMUX_IMESSAGE_SEND").map(PathBuf::from)
        .unwrap_or_else(|| crate::config::amux_home().join("imessage/send-imessage.sh"))
}

fn digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// The owner's own handles: AMUX_IMESSAGE_SELF_HANDLES, else AMUX_OWNER_PHONE.
fn self_handles() -> Vec<String> {
    setting("AMUX_IMESSAGE_SELF_HANDLES").or_else(|| setting("AMUX_OWNER_PHONE"))
        .map(|s| s.split(',').map(|h| h.trim().to_lowercase()).filter(|h| !h.is_empty()).collect())
        .unwrap_or_default()
}

/// Is a chat identifier one of the owner's handles? A phone matches on its
/// last ten digits (chat.db writes +14105550100, the setting may not).
pub fn is_self(identifier: &str, handles: &[String]) -> bool {
    let id = identifier.trim().to_lowercase();
    handles.iter().any(|h| {
        if h.contains('@') {
            *h == id
        } else {
            let (a, b) = (digits(h), digits(&id));
            a.len() >= 7 && b.len() >= 7 && a[a.len().saturating_sub(10)..] == b[b.len().saturating_sub(10)..]
        }
    })
}

pub fn enabled() -> bool {
    why_off().is_none()
}

pub fn why_off() -> Option<String> {
    if setting("AMUX_IMESSAGE_BRIDGE").as_deref() == Some("0") {
        return Some("AMUX_IMESSAGE_BRIDGE=0".into());
    }
    if self_handles().is_empty() {
        return Some("neither AMUX_OWNER_PHONE nor AMUX_IMESSAGE_SELF_HANDLES is set".into());
    }
    if !db_path().exists() {
        return Some(format!("no Messages database at {}", db_path().display()));
    }
    None
}

/// The words of an `attributedBody` (an NSAttributedString archive): the
/// bytes after the NSString class name, behind a `+` and a length that is one
/// byte, or 0x81 and two bytes, or 0x82 and three, little-endian.
pub fn attributed_text(blob: &[u8]) -> Option<String> {
    let at = blob.windows(8).position(|w| w == b"NSString")? + 8;
    let s = &blob[at..];
    let plus = s.iter().position(|b| *b == b'+')?;
    let (mut n, mut k) = (*s.get(plus + 1)? as usize, plus + 2);
    if n == 0x81 || n == 0x82 {
        let w = if n == 0x81 { 2 } else { 3 };
        let bytes = s.get(k..k + w)?;
        n = bytes.iter().rev().fold(0usize, |acc, b| (acc << 8) | *b as usize);
        k += w;
    }
    s.get(k..k + n).map(|b| String::from_utf8_lossy(b).into_owned())
}

/// One read: the newest message row in the owner's own threads, and the
/// messages after `after` as (row, guid, chat identifier, text).
pub struct Read {
    pub top: i64,
    pub rows: Vec<(i64, String, String, String)>,
}

pub fn read_new(db: &std::path::Path, handles: &[String], after: Option<i64>) -> Result<Read, String> {
    let c = rusqlite::Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| e.to_string())?;
    c.busy_timeout(Duration::from_secs(2)).ok();
    let chats: Vec<(i64, String)> = {
        let mut st = c.prepare("SELECT ROWID, chat_identifier FROM chat WHERE style=45").map_err(|e| e.to_string())?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get::<_, String>(1)?))).map_err(|e| e.to_string())?;
        rows.flatten().filter(|(_, id)| is_self(id, handles)).collect()
    };
    if chats.is_empty() {
        return Err("no iMessage thread with the owner's own number or Apple ID in Messages".into());
    }
    let ids = chats.iter().map(|(i, _)| i.to_string()).collect::<Vec<_>>().join(",");
    let top: i64 = c.query_row(&format!("SELECT COALESCE(MAX(message_id),0) FROM chat_message_join WHERE chat_id IN ({ids})"), [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    let Some(after) = after else { return Ok(Read { top, rows: vec![] }) };
    let mut st = c.prepare(&format!(
        "SELECT m.ROWID, m.guid, j.chat_id, m.text, m.attributedBody FROM message m
         JOIN chat_message_join j ON j.message_id = m.ROWID
         WHERE j.chat_id IN ({ids}) AND m.ROWID > ?1 AND COALESCE(m.item_type,0)=0 AND COALESCE(m.associated_message_type,0)=0
         ORDER BY m.ROWID LIMIT 50")).map_err(|e| e.to_string())?;
    let rows = st.query_map([after], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?,
            r.get::<_, Option<String>>(3)?, r.get::<_, Option<Vec<u8>>>(4)?)))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (rid, guid, chat, text, blob) in rows.flatten() {
        let words = text.filter(|t| !t.trim().is_empty()).or_else(|| blob.as_deref().and_then(attributed_text)).unwrap_or_default();
        let words = words.replace('\u{fffc}', "").trim().to_string();
        let ident = chats.iter().find(|(i, _)| *i == chat).map(|(_, s)| s.clone()).unwrap_or_default();
        if !words.is_empty() {
            out.push((rid, guid, ident, words));
        }
    }
    Ok(Read { top, rows: out })
}

/// Texts the bridge sent, and texts it filed with when: a sent answer must
/// not read as the owner's command, and one message stored twice is filed once.
#[derive(Default)]
struct Recent {
    sent: Vec<String>,
    filed: Vec<(String, f64)>,
}

fn recent() -> &'static Mutex<Recent> {
    static S: std::sync::OnceLock<Mutex<Recent>> = std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

fn state_path() -> PathBuf {
    crate::config::amux_home().join("bridge-imessage.json")
}

pub async fn poll_once(state: &crate::api::AppState) -> Result<usize, String> {
    let handles = self_handles();
    let saved: Option<i64> = std::fs::read_to_string(state_path()).ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()).and_then(|v| v["last_rowid"].as_i64());
    let (db, h) = (db_path(), handles.clone());
    let read = tokio::task::spawn_blocking(move || read_new(&db, &h, saved)).await.map_err(|e| e.to_string())??;
    let now = crate::config::now_f64();
    let mut n = 0;
    for (rid, guid, ident, text) in &read.rows {
        let t = text.trim().to_string();
        let skip = recent().lock().map(|mut g| {
            g.filed.retain(|(_, at)| now - at < SAME_WORDS_WINDOW_S);
            let s = g.sent.contains(&t) || g.filed.iter().any(|(x, _)| *x == t);
            if !s && is_command(&t) { g.filed.push((t.clone(), now)); }
            s
        }).unwrap_or(false);
        if skip || !is_command(&t) {
            continue;
        }
        let m = Inbound { platform: PLATFORM.into(), conversation: ident.clone(), is_group: false,
            sender_id: ident.clone(), sender_name: "owner".into(), text: t, platform_msg_id: guid.clone() };
        let out = super::self_chat::file(state, m, None).await;
        tracing::info!(row = rid, outcome = ?out, measured = true, n_considered = 1, verdict = "bridge_imessage_filed",
            "bridge: a message from the owner's own iMessage thread went through the bridge");
        n += 1;
    }
    let top = read.rows.last().map(|r| r.0).unwrap_or(read.top).max(read.top).max(saved.unwrap_or(0));
    if saved != Some(top) {
        std::fs::write(state_path(), serde_json::json!({"last_rowid": top}).to_string()).map_err(|e| e.to_string())?;
    }
    Ok(n)
}

pub fn is_command(text: &str) -> bool {
    super::self_chat::is_command(text)
}

pub async fn run(state: crate::api::AppState) {
    let (mut last_err, mut last_off) = (String::new(), String::new());
    loop {
        let off = why_off();
        if let Some(why) = &off {
            if *why != last_off {
                tracing::info!(reason = %why, measured = true, n_considered = 1, verdict = "bridge_imessage_off",
                    "bridge: the owner's own iMessage thread is not being read");
                last_off = why.clone();
            }
        } else {
            last_off.clear();
            match poll_once(&state).await {
                Ok(_) => last_err.clear(),
                Err(e) => {
                    if e != last_err {
                        let hint = if e.contains("authorization denied") || e.contains("unable to open") {
                            " (give amux-server-rs Full Disk Access in System Settings > Privacy & Security)"
                        } else { "" };
                        tracing::warn!(error = %e, hint, measured = true, n_considered = 1, verdict = "bridge_imessage_read_failed",
                            "bridge: reading the owner's own iMessage thread failed");
                        last_err = e;
                    }
                }
            }
        }
        let wait = wait_for(off.is_none());
        crate::runtime_jobs::registry::tick_every(crate::runtime_jobs::registry::ids::BRIDGE_IMESSAGE, wait);
        tokio::time::sleep(wait).await;
    }
}

fn wait_for(on: bool) -> Duration {
    Duration::from_secs(if on { 3 } else { 30 })
}

pub fn spawn(state: crate::api::AppState) -> tokio::task::JoinHandle<()> {
    crate::runtime_jobs::registry::spawn_loop(crate::runtime_jobs::registry::ids::BRIDGE_IMESSAGE,
        Some(Duration::from_secs(30)), move || run(state.clone()))
}

/// Send into the owner's own thread (`handle` is its chat identifier). The
/// bridge never texts anyone else from the owner's number.
pub async fn send(handle: &str, text: &str) -> Result<String, String> {
    if !is_self(handle, &self_handles()) {
        return Err(format!("final: {handle} is not the owner's own iMessage thread"));
    }
    let run = tokio::process::Command::new(script()).arg(handle).arg(text).kill_on_drop(true).output();
    let out = tokio::time::timeout(Duration::from_secs(60), run).await
        .map_err(|_| "send-imessage.sh took over 60 s".to_string())?.map_err(|e| e.to_string())?;
    if !out.status.success() {
        let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        return Err(format!("send-imessage.sh rc={}: {}", out.status.code().map_or("killed".into(), |r| r.to_string()), said.trim()));
    }
    if let Ok(mut g) = recent().lock() {
        g.sent.push(text.trim().to_string());
        let len = g.sent.len();
        if len > 50 {
            g.sent.drain(..len - 50);
        }
    }
    Ok(format!("imessage:{}", super::now_ms()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(words: &str) -> Vec<u8> {
        let mut b = b"\x04\x0bstreamtyped\x81\xe8\x03\x84\x01@\x84\x84\x84\x12NSAttributedString\x00\x84\x84\x08NSObject\x00\x85\x92\x84\x84\x84\x08NSString\x01\x94\x84\x01+".to_vec();
        let n = words.len();
        if n < 0x80 {
            b.push(n as u8);
        } else {
            b.push(0x81);
            b.extend_from_slice(&(n as u16).to_le_bytes());
        }
        b.extend_from_slice(words.as_bytes());
        b.extend_from_slice(b"\x86\x84\x02iI\x01");
        b
    }

    #[test]
    fn the_words_come_out_of_an_attributed_body() {
        assert_eq!(attributed_text(&blob("@amux status?")).as_deref(), Some("@amux status?"));
        let long = "x".repeat(1880);
        assert_eq!(attributed_text(&blob(&long)).map(|s| s.len()), Some(1880), "a two-byte length");
        assert_eq!(attributed_text(b"no archive here"), None);
    }

    #[test]
    fn the_owners_handles_match_however_the_number_is_written() {
        let h = vec!["+1 (410) 555-0100".to_string(), "me@icloud.com".to_string()];
        assert!(is_self("+14105550100", &h));
        assert!(is_self("4105550100", &h));
        assert!(is_self("ME@icloud.com", &h));
        assert!(!is_self("+14105550101", &h));
        assert!(!is_self("other@icloud.com", &h));
        assert!(!is_self("chat123456789", &[]), "no handles, no self");
    }

    /// Only the owner's own 1:1 thread is read; the first read only records
    /// where it is; text comes from `text` or `attributedBody`; reactions and
    /// other threads are left out.
    #[test]
    fn reads_only_new_messages_in_the_owners_own_thread() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("chat.db");
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, chat_identifier TEXT, style INTEGER);
             CREATE TABLE message (ROWID INTEGER PRIMARY KEY, guid TEXT, text TEXT, attributedBody BLOB, item_type INTEGER, associated_message_type INTEGER);
             CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER);
             INSERT INTO chat VALUES (1, '+14105550100', 45), (2, '+15550001111', 45), (3, 'chat99', 43);
             INSERT INTO message VALUES (10, 'g10', 'old', NULL, 0, 0);
             INSERT INTO chat_message_join VALUES (1, 10);").unwrap();
        let h = vec!["4105550100".to_string()];
        let first = read_new(&db, &h, None).unwrap();
        assert_eq!((first.top, first.rows.len()), (10, 0), "history is not replayed");
        c.execute("INSERT INTO message VALUES (11, 'g11', NULL, ?1, 0, 0)", [blob("@amux ping")]).unwrap();
        c.execute_batch(
            "INSERT INTO message VALUES (12, 'g12', '#ops hi', NULL, 0, 0), (13, 'g13', 'Loved “x”', NULL, 0, 2000),
                                        (14, 'g14', '@amux elsewhere', NULL, 0, 0), (15, 'g15', '@amux group', NULL, 0, 0);
             INSERT INTO chat_message_join VALUES (1, 11), (1, 12), (1, 13), (2, 14), (3, 15);").unwrap();
        let next = read_new(&db, &h, Some(first.top)).unwrap();
        let got: Vec<(&str, &str)> = next.rows.iter().map(|r| (r.2.as_str(), r.3.as_str())).collect();
        assert_eq!(got, vec![("+14105550100", "@amux ping"), ("+14105550100", "#ops hi")]);
        assert_eq!(next.top, 13);
    }
}
