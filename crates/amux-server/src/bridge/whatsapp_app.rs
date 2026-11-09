//! WhatsApp through the owner's own WhatsApp app on this Mac: the owner's
//! "message yourself" chat is a direct line to amux, with no Meta app, no
//! business number and no public URL (Ethan, 2026-11-05: "make it so amux can
//! receive whatsapp messages and execute them via @<worker_name> and receive
//! responses").
//!
//! - Receive: WhatsApp keeps its chats in a local SQLite database, current
//!   while the app runs. Every 3 s this reads, read-only, the owner's own chat
//!   for new messages they wrote. One that starts with `@` or `#` is routed
//!   by `self_chat` (`@worker`, `@person`, `#channel`); anything else is the
//!   owner's note to self and is left alone.
//! - Only answers to what the chat started come back (`self_chat::pending`),
//!   never the channel's chatter.
//! - Send: `~/.amux/whatsapp/send-whatsapp.sh` pastes into the app and
//!   confirms the message landed in its database. It can only type while the
//!   Mac is unlocked and idle, so a locked or busy Mac is a WAIT, not a failed
//!   attempt: the answer goes out when the Mac is free.
//!
//! Off with AMUX_WHATSAPP_APP=0. Needs AMUX_OWNER_PHONE (the self chat's
//! number). Verdicts: bridge_whatsapp_app_filed,
//! bridge_whatsapp_app_read_failed, bridge_whatsapp_app_launched,
//! bridge_outbound_waiting.

use super::{setting, Inbound};
use rusqlite::{params, OpenFlags, OptionalExtension};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const PLATFORM: &str = "whatsapp-app";
/// While a send waited (locked or busy Mac), the next try is this far off.
const WAIT_BACKOFF: Duration = Duration::from_secs(60);

fn db_path() -> PathBuf {
    setting("AMUX_WHATSAPP_APP_DB").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Library/Group Containers/group.net.whatsapp.WhatsApp.shared/ChatStorage.sqlite")
    })
}

fn script() -> PathBuf {
    setting("AMUX_WHATSAPP_APP_SEND").map(PathBuf::from)
        .unwrap_or_else(|| crate::config::amux_home().join("whatsapp/send-whatsapp.sh"))
}

fn owner_digits() -> Option<String> {
    setting("AMUX_OWNER_PHONE").map(|p| p.chars().filter(char::is_ascii_digit).collect::<String>()).filter(|d| !d.is_empty())
}

/// On, with a database to read and the owner's number to answer on.
pub fn enabled() -> bool {
    setting("AMUX_WHATSAPP_APP").as_deref() != Some("0") && owner_digits().is_some() && db_path().exists()
}

/// Why it is not running, for the status route; None when it is.
pub fn why_off() -> Option<String> {
    if setting("AMUX_WHATSAPP_APP").as_deref() == Some("0") {
        return Some("AMUX_WHATSAPP_APP=0".into());
    }
    if owner_digits().is_none() {
        return Some("AMUX_OWNER_PHONE is not set".into());
    }
    if !db_path().exists() {
        return Some(format!("no WhatsApp database at {}", db_path().display()));
    }
    None
}

/// The owner's own chat: AMUX_WHATSAPP_SELF_JID, else the chat WhatsApp names
/// "You" (behind a left-to-right mark), else the phone-number JID.
fn self_chat(c: &rusqlite::Connection, phone: &str) -> rusqlite::Result<Option<(i64, String)>> {
    if let Some(jid) = setting("AMUX_WHATSAPP_SELF_JID") {
        return c.query_row("SELECT Z_PK, ZCONTACTJID FROM ZWACHATSESSION WHERE ZCONTACTJID=?1", [jid], |r| Ok((r.get(0)?, r.get(1)?))).optional();
    }
    c.query_row(
        "SELECT Z_PK, ZCONTACTJID FROM ZWACHATSESSION
         WHERE ZSESSIONTYPE=0 AND (replace(ZPARTNERNAME, char(8206), '')='You' OR ZCONTACTJID=?1)
         ORDER BY ZLASTMESSAGEDATE DESC LIMIT 1",
        [format!("{phone}@s.whatsapp.net")], |r| Ok((r.get(0)?, r.get(1)?))).optional()
}

/// A message in the self chat that is a command for amux.
pub fn is_command(text: &str) -> bool {
    super::self_chat::is_command(text)
}

/// What one read found: the self chat's JID and the new messages the owner
/// wrote there after `after` (by WhatsApp's row id).
pub struct Read {
    pub jid: String,
    pub top: i64,
    pub rows: Vec<(i64, String, String, f64)>,
}

pub fn read_new(db: &std::path::Path, phone: &str, after: Option<i64>) -> Result<Option<Read>, String> {
    let c = rusqlite::Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| e.to_string())?;
    c.busy_timeout(Duration::from_secs(2)).ok();
    let Some((chat, jid)) = self_chat(&c, phone).map_err(|e| e.to_string())? else { return Ok(None) };
    let top: i64 = c.query_row("SELECT COALESCE(MAX(Z_PK),0) FROM ZWAMESSAGE WHERE ZCHATSESSION=?1", [chat], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    let Some(after) = after else { return Ok(Some(Read { jid, top, rows: vec![] })) };
    let mut st = c.prepare(
        "SELECT Z_PK, COALESCE(ZSTANZAID, ''), ZTEXT, COALESCE(ZMESSAGEDATE, 0) FROM ZWAMESSAGE
         WHERE ZCHATSESSION=?1 AND Z_PK>?2 AND ZISFROMME=1 AND ZMESSAGETYPE=0 AND ZTEXT IS NOT NULL ORDER BY Z_PK LIMIT 50",
    ).map_err(|e| e.to_string())?;
    let rows = st.query_map(params![chat, after], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .map_err(|e| e.to_string())?.flatten().collect();
    Ok(Some(Read { jid, top, rows }))
}

fn sent_recently() -> &'static Mutex<Vec<String>> {
    static S: std::sync::OnceLock<Mutex<Vec<String>>> = std::sync::OnceLock::new();
    S.get_or_init(|| Mutex::new(Vec::new()))
}

fn state_path() -> PathBuf {
    crate::config::amux_home().join("bridge-whatsapp-app.json")
}

/// One read of the self chat. The last row seen is kept in
/// ~/.amux/bridge-whatsapp-app.json, so a restart neither repeats nor skips;
/// the first read only records where the chat is, so history is not replayed.
pub async fn poll_once(state: &crate::api::AppState) -> Result<usize, String> {
    let Some(phone) = owner_digits() else { return Ok(0) };
    let saved: Option<i64> = std::fs::read_to_string(state_path()).ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()).and_then(|v| v["last_pk"].as_i64());
    let (db, p) = (db_path(), phone.clone());
    let read = tokio::task::spawn_blocking(move || read_new(&db, &p, saved)).await.map_err(|e| e.to_string())??;
    let Some(read) = read else { return Err("no self chat (the chat WhatsApp calls \"You\") in the database".into()) };
    let mut n = 0;
    for (pk, stanza, text, _ts) in &read.rows {
        let echo = sent_recently().lock().map(|s| s.iter().any(|t| t == text.trim())).unwrap_or(false);
        if echo || !is_command(text) {
            continue;
        }
        let m = Inbound { platform: PLATFORM.into(), conversation: read.jid.clone(), is_group: false, sender_id: phone.clone(),
            sender_name: "owner".into(), text: text.clone(), platform_msg_id: if stanza.is_empty() { format!("pk:{pk}") } else { stanza.clone() } };
        let out = super::self_chat::file(state, m, None).await;
        tracing::info!(row = pk, outcome = ?out, measured = true, n_considered = 1, verdict = "bridge_whatsapp_app_filed",
            "bridge: a command from the owner's WhatsApp self chat went through the bridge");
        n += 1;
    }
    let top = read.rows.last().map(|r| r.0).unwrap_or(read.top).max(saved.unwrap_or(0));
    if saved != Some(top) {
        std::fs::write(state_path(), serde_json::json!({"last_pk": top, "jid": read.jid}).to_string()).map_err(|e| e.to_string())?;
    }
    Ok(n)
}

/// The database is current only while the app runs: open it, hidden, at most
/// every ten minutes.
async fn ensure_running() {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let running = tokio::process::Command::new("/usr/bin/pgrep").args(["-xq", "WhatsApp"]).status().await.map(|s| s.success()).unwrap_or(true);
    if running {
        return;
    }
    {
        let Ok(mut g) = LAST.lock() else { return };
        if g.is_some_and(|t| t.elapsed() < Duration::from_secs(600)) {
            return;
        }
        *g = Some(Instant::now());
    }
    let ok = tokio::process::Command::new("/usr/bin/open").args(["-gja", "WhatsApp"]).status().await.map(|s| s.success()).unwrap_or(false);
    tracing::info!(ok, measured = true, n_considered = 1, verdict = "bridge_whatsapp_app_launched",
        "bridge: WhatsApp was not running, so its database was stale; it was opened in the background");
}

pub async fn run(state: crate::api::AppState) {
    let mut last_err = String::new();
    let mut last_off = String::new();
    loop {
        let on = enabled();
        if !on {
            // Said once per distinct reason, so a sweep can tell "off on purpose" from "never ran".
            let why = why_off().unwrap_or_default();
            if why != last_off {
                tracing::info!(reason = %why, measured = true, n_considered = 1, verdict = "bridge_whatsapp_app_off",
                    "bridge: the WhatsApp self chat is not being read");
                last_off = why;
            }
        }
        if on {
            last_off.clear();
            ensure_running().await;
            match poll_once(&state).await {
                Ok(_) => last_err.clear(),
                Err(e) => {
                    // Said once per distinct reason, not every 3 s.
                    if e != last_err {
                        tracing::warn!(error = %e, measured = true, n_considered = 1, verdict = "bridge_whatsapp_app_read_failed",
                            "bridge: reading the WhatsApp self chat failed");
                        last_err = e;
                    }
                }
            }
        }
        let wait = wait_for(on);
        crate::runtime_jobs::registry::tick_every(crate::runtime_jobs::registry::ids::BRIDGE_WHATSAPP_APP, wait);
        tokio::time::sleep(wait).await;
    }
}

/// The wait between reads, also the cadence declared to the job registry.
fn wait_for(on: bool) -> Duration {
    Duration::from_secs(if on { 3 } else { 30 })
}

pub fn spawn(state: crate::api::AppState) -> tokio::task::JoinHandle<()> {
    crate::runtime_jobs::registry::spawn_loop(crate::runtime_jobs::registry::ids::BRIDGE_WHATSAPP_APP,
        Some(Duration::from_secs(30)), move || run(state.clone()))
}

/// The send script's exit status, as the bridge reads it: 0 sent, 4 and 6
/// (busy or locked Mac, nothing typed) a wait, anything else a failure.
pub fn classify(rc: Option<i32>, out: &str) -> Result<(), String> {
    match rc {
        Some(0) => Ok(()),
        Some(6) => Err(format!("wait: {}", out.trim())),
        Some(4) if !out.contains("error 1002") => Err(format!("wait: {}", out.trim())),
        // Return was pressed and the message is not in the database: it may
        // be in the chat, so it is never typed again.
        Some(5) => Err(format!("final: {}", out.trim())),
        _ => Err(format!("send-whatsapp.sh rc={}: {}", rc.map_or("killed".into(), |r| r.to_string()), out.trim())),
    }
}

/// Send into the owner's self chat. An `Err` starting with `wait:` means the
/// Mac could not take keystrokes and nothing was typed; `final:` means it may
/// have been sent and must not be retried.
pub async fn send(text: &str) -> Result<String, String> {
    static WAITED: Mutex<Option<Instant>> = Mutex::new(None);
    if WAITED.lock().ok().and_then(|g| *g).is_some_and(|t| t.elapsed() < WAIT_BACKOFF) {
        return Err("wait: the last try found the Mac locked or in use".into());
    }
    let phone = owner_digits().ok_or("AMUX_OWNER_PHONE is not set")?;
    let run = tokio::process::Command::new(script()).arg(&phone).arg(text).kill_on_drop(true).output();
    let out = tokio::time::timeout(Duration::from_secs(240), run).await
        .map_err(|_| "send-whatsapp.sh took over 240 s".to_string())?.map_err(|e| e.to_string())?;
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    let r = classify(out.status.code(), &said);
    if let Ok(mut g) = WAITED.lock() {
        *g = matches!(&r, Err(e) if e.starts_with("wait:")).then(Instant::now);
    }
    r?;
    if let Ok(mut s) = sent_recently().lock() {
        s.push(text.trim().to_string());
        let len = s.len();
        if len > 50 {
            s.drain(..len - 50);
        }
    }
    Ok(format!("app:{}", super::now_ms()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_db(dir: &std::path::Path) -> PathBuf {
        let p = dir.join("ChatStorage.sqlite");
        let c = rusqlite::Connection::open(&p).unwrap();
        c.execute_batch(
            "CREATE TABLE ZWACHATSESSION (Z_PK INTEGER PRIMARY KEY, ZPARTNERNAME TEXT, ZCONTACTJID TEXT, ZSESSIONTYPE INTEGER, ZLASTMESSAGEDATE REAL);
             CREATE TABLE ZWAMESSAGE (Z_PK INTEGER PRIMARY KEY, ZCHATSESSION INTEGER, ZISFROMME INTEGER, ZMESSAGETYPE INTEGER,
               ZTEXT TEXT, ZSTANZAID TEXT, ZMESSAGEDATE REAL);
             INSERT INTO ZWACHATSESSION VALUES (1, 'Doron', '1555@s.whatsapp.net', 0, 10), (2, char(8206)||'You', '999@lid', 0, 5);
             INSERT INTO ZWAMESSAGE VALUES (10, 2, 1, 0, 'old note', 'A', 1), (11, 1, 1, 0, '@amux not the self chat', 'B', 2);",
        ).unwrap();
        p
    }

    /// The self chat is found by WhatsApp's "You" name; the first read only
    /// records where it is; later reads return the owner's new text messages
    /// there and nothing from another chat, from someone else, or not text.
    #[test]
    fn reads_only_new_owner_text_in_the_self_chat() {
        let dir = tempfile::tempdir().unwrap();
        let db = fake_db(dir.path());
        let first = read_new(&db, "1410", None).unwrap().unwrap();
        assert_eq!((first.jid.as_str(), first.top, first.rows.len()), ("999@lid", 10, 0), "history is not replayed");
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch("INSERT INTO ZWAMESSAGE VALUES (12, 2, 1, 0, '@amux \"status?\"', 'C', 3), (13, 2, 0, 0, '@amux from someone', 'D', 3),
                         (14, 2, 1, 8, NULL, 'E', 3), (15, 1, 1, 0, '@amux elsewhere', 'F', 3), (16, 2, 1, 0, 'buy milk', 'G', 4);").unwrap();
        let next = read_new(&db, "1410", Some(first.top)).unwrap().unwrap();
        let texts: Vec<&str> = next.rows.iter().map(|r| r.2.as_str()).collect();
        assert_eq!(texts, vec!["@amux \"status?\"", "buy milk"]);
        assert!(is_command(texts[0]) && !is_command(texts[1]), "a note to self is not a command");
        assert!(is_command("#ops ship it") && !is_command("hello @amux"));
    }

    /// A locked or busy Mac (nothing typed) is a wait; a missing Accessibility
    /// grant or an unconfirmed press is a failure someone has to look at.
    #[test]
    fn a_locked_mac_waits_and_a_refused_press_fails() {
        assert!(classify(Some(0), "rc=0 confirmed").is_ok());
        assert!(classify(Some(6), "rc=6 this Mac is locked").unwrap_err().starts_with("wait:"));
        assert!(classify(Some(4), "rc=4 this Mac stayed in use for 120s").unwrap_err().starts_with("wait:"));
        assert!(!classify(Some(4), "rc=4 error 1002: not allowed to send keystrokes").unwrap_err().starts_with("wait:"));
        assert!(classify(Some(5), "rc=5 Return pressed, not in the database").unwrap_err().starts_with("final:"), "possibly sent: never retyped");
        assert!(!classify(None, "").unwrap_err().starts_with("wait:"));
    }

    #[test]
    fn the_loop_declares_the_wait_it_keeps() {
        assert_eq!(wait_for(true), Duration::from_secs(3));
        assert_eq!(wait_for(false), Duration::from_secs(30));
    }
}
