//! The per-platform half of the chat bridge: send a text into a
//! conversation, and turn a platform's inbound payload into an [`Inbound`].
//! Each one is small on purpose; everything shared lives in `bridge/mod.rs`.
//!
//! | platform | inbound | needs a public URL | credentials (server.env, via Connectors) |
//! |---|---|---|---|
//! | WhatsApp | webhook `/api/bridge/hook/whatsapp`, X-Hub-Signature-256 | yes | WHATSAPP_TOKEN, WHATSAPP_PHONE_NUMBER_ID, WHATSAPP_APP_SECRET, WHATSAPP_VERIFY_TOKEN |
//! | Slack | Events API `/api/bridge/hook/slack`, X-Slack-Signature | yes | SLACK_BOT_TOKEN, SLACK_SIGNING_SECRET |
//! | Telegram | long poll (runtime_jobs::telegram_poll) | no | TELEGRAM_BOT_TOKEN |
//! | Discord | poll of DISCORD_CHANNELS (this module) | no | DISCORD_BOT_TOKEN, DISCORD_CHANNELS |
//!
//! Each platform's API base can be pointed elsewhere (AMUX_BRIDGE_<P>_BASE),
//! which is how the e2e runs every adapter against a local stand-in that
//! speaks the platform's wire format.

use super::{setting, Inbound};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::time::Duration;

fn base(platform: &str, default: &str) -> String {
    setting(&format!("AMUX_BRIDGE_{}_BASE", platform.to_uppercase())).unwrap_or_else(|| default.to_string()).trim_end_matches('/').to_string()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().timeout(Duration::from_secs(20)).build().unwrap_or_default()
}

fn need(key: &str) -> Result<String, String> {
    setting(key).ok_or_else(|| format!("{key} is not set (Connectors)"))
}

/// Which platforms have the credentials to send, for the status route.
pub fn configured() -> Vec<(&'static str, bool, Vec<&'static str>)> {
    let req: [(&str, &[&str]); 4] = [
        ("whatsapp", &["WHATSAPP_TOKEN", "WHATSAPP_PHONE_NUMBER_ID", "WHATSAPP_APP_SECRET", "WHATSAPP_VERIFY_TOKEN"]),
        ("slack", &["SLACK_BOT_TOKEN", "SLACK_SIGNING_SECRET"]),
        ("telegram", &["TELEGRAM_BOT_TOKEN"]),
        ("discord", &["DISCORD_BOT_TOKEN", "DISCORD_CHANNELS"]),
    ];
    req.iter().map(|(p, keys)| {
        let missing: Vec<&'static str> = keys.iter().copied().filter(|k| setting(k).is_none()).collect();
        (*p, missing.is_empty(), missing)
    }).collect()
}

/// Send `text` into a conversation. Returns the platform's message id.
pub async fn send(platform: &str, conversation: &str, is_group: bool, text: &str) -> Result<String, String> {
    let text: String = text.chars().take(3800).collect();
    let c = client();
    let (status, body): (u16, Value) = match platform {
        "whatsapp" => {
            let url = format!("{}/{}/messages", base("whatsapp", "https://graph.facebook.com/v21.0"), need("WHATSAPP_PHONE_NUMBER_ID")?);
            let payload = json!({"messaging_product": "whatsapp", "recipient_type": if is_group { "group" } else { "individual" },
                                 "to": conversation, "type": "text", "text": {"preview_url": false, "body": text}});
            let r = c.post(url).bearer_auth(need("WHATSAPP_TOKEN")?).json(&payload).send().await.map_err(|e| e.to_string())?;
            (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
        }
        "slack" => {
            let r = c.post(format!("{}/chat.postMessage", base("slack", "https://slack.com/api"))).bearer_auth(need("SLACK_BOT_TOKEN")?)
                .json(&json!({"channel": conversation, "text": text})).send().await.map_err(|e| e.to_string())?;
            (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
        }
        "telegram" => {
            let url = format!("{}/bot{}/sendMessage", base("telegram", "https://api.telegram.org"), need("TELEGRAM_BOT_TOKEN")?);
            let r = c.post(url).json(&json!({"chat_id": conversation, "text": text})).send().await.map_err(|e| e.to_string())?;
            (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
        }
        "discord" => {
            let url = format!("{}/channels/{}/messages", base("discord", "https://discord.com/api/v10"), conversation);
            let r = c.post(url).header("Authorization", format!("Bot {}", need("DISCORD_BOT_TOKEN")?))
                .json(&json!({"content": text.chars().take(1990).collect::<String>()})).send().await.map_err(|e| e.to_string())?;
            (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
        }
        other => return Err(format!("no adapter for {other}")),
    };
    let id = match platform {
        "whatsapp" => body["messages"][0]["id"].as_str().map(str::to_string),
        "slack" => (body["ok"] == true).then(|| body["ts"].as_str().unwrap_or("").to_string()),
        "telegram" => (body["ok"] == true).then(|| body["result"]["message_id"].to_string()),
        "discord" => body["id"].as_str().map(str::to_string),
        _ => None,
    };
    match id {
        Some(id) if (200..300).contains(&status) => Ok(id),
        _ => Err(format!("{platform} answered {status}: {}", body.to_string().chars().take(300).collect::<String>())),
    }
}

fn hmac_hex(secret: &str, data: &[u8]) -> String {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()).expect("hmac takes any key length");
    m.update(data);
    hex::encode(m.finalize().into_bytes())
}

fn eq_ct(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---- WhatsApp ----------------------------------------------------------------

/// Meta signs every webhook body with the app secret.
pub fn whatsapp_signature_ok(header: Option<&str>, body: &[u8]) -> bool {
    let (Some(h), Some(secret)) = (header, setting("WHATSAPP_APP_SECRET")) else { return false };
    let Some(sig) = h.strip_prefix("sha256=") else { return false };
    eq_ct(&sig.to_lowercase(), &hmac_hex(&secret, body))
}

/// The subscription handshake: echo the challenge when the token matches.
pub fn whatsapp_verify(mode: Option<&str>, token: Option<&str>, challenge: Option<&str>) -> Option<String> {
    let want = setting("WHATSAPP_VERIFY_TOKEN")?;
    (mode == Some("subscribe") && token.is_some_and(|t| eq_ct(t, &want))).then(|| challenge.unwrap_or("").to_string())
}

/// Text messages in a Cloud API webhook. A message in a group (Groups API)
/// carries `group_id`; the conversation is then the group, else the sender.
pub fn whatsapp_parse(v: &Value) -> Vec<Inbound> {
    let mut out = Vec::new();
    for entry in v["entry"].as_array().into_iter().flatten() {
        for change in entry["changes"].as_array().into_iter().flatten() {
            let val = &change["value"];
            let names: Vec<(String, String)> = val["contacts"].as_array().into_iter().flatten()
                .map(|c| (c["wa_id"].as_str().unwrap_or("").to_string(), c["profile"]["name"].as_str().unwrap_or("").to_string()))
                .collect();
            for m in val["messages"].as_array().into_iter().flatten() {
                let text = match m["type"].as_str() {
                    Some("text") => m["text"]["body"].as_str().unwrap_or("").to_string(),
                    Some("button") => m["button"]["text"].as_str().unwrap_or("").to_string(),
                    _ => String::new(),
                };
                let from = m["from"].as_str().unwrap_or("").to_string();
                let group = m["group_id"].as_str().map(str::to_string);
                let name = names.iter().find(|(w, _)| w == &from).map(|(_, n)| n.clone()).unwrap_or_default();
                out.push(Inbound {
                    platform: "whatsapp".into(),
                    conversation: group.clone().unwrap_or_else(|| from.clone()),
                    is_group: group.is_some(),
                    sender_id: from,
                    sender_name: name,
                    text,
                    platform_msg_id: m["id"].as_str().unwrap_or("").to_string(),
                });
            }
        }
    }
    out
}

// ---- Slack -------------------------------------------------------------------

/// Slack signs `v0:<timestamp>:<body>`; a timestamp more than five minutes
/// off is refused (replay).
pub fn slack_signature_ok(ts: Option<&str>, sig: Option<&str>, body: &[u8], now_s: i64) -> bool {
    let (Some(ts), Some(sig), Some(secret)) = (ts, sig, setting("SLACK_SIGNING_SECRET")) else { return false };
    let Ok(t) = ts.parse::<i64>() else { return false };
    if (now_s - t).abs() > 300 {
        return false;
    }
    let mut base = format!("v0:{ts}:").into_bytes();
    base.extend_from_slice(body);
    eq_ct(sig, &format!("v0={}", hmac_hex(&secret, &base)))
}

pub fn slack_parse(v: &Value) -> Option<Inbound> {
    let e = &v["event"];
    if v["type"] != "event_callback" || e["type"] != "message" || e.get("subtype").is_some_and(|s| !s.is_null()) || e.get("bot_id").is_some_and(|b| !b.is_null()) {
        return None;
    }
    Some(Inbound {
        platform: "slack".into(),
        conversation: e["channel"].as_str()?.to_string(),
        is_group: e["channel_type"].as_str() != Some("im"),
        sender_id: e["user"].as_str().unwrap_or("").to_string(),
        sender_name: String::new(),
        text: e["text"].as_str().unwrap_or("").to_string(),
        platform_msg_id: format!("{}:{}", e["channel"].as_str().unwrap_or(""), e["ts"].as_str().unwrap_or("")),
    })
}

// ---- Telegram ----------------------------------------------------------------

pub fn telegram_parse(update: &Value) -> Option<Inbound> {
    let msg = update.get("message")?;
    let chat = &msg["chat"];
    let chat_type = chat["type"].as_str().unwrap_or("private");
    let from = &msg["from"];
    Some(Inbound {
        platform: "telegram".into(),
        conversation: chat["id"].as_i64()?.to_string(),
        is_group: chat_type == "group" || chat_type == "supergroup",
        sender_id: from["id"].as_i64().map(|i| i.to_string()).unwrap_or_default(),
        sender_name: from["username"].as_str().or(from["first_name"].as_str()).unwrap_or("").to_string(),
        text: msg["text"].as_str().unwrap_or("").to_string(),
        platform_msg_id: format!("{}:{}", chat["id"].as_i64().unwrap_or(0), msg["message_id"].as_i64().unwrap_or(0)),
    })
}

// ---- Discord -----------------------------------------------------------------

pub fn discord_parse(channel: &str, is_group: bool, m: &Value) -> Option<Inbound> {
    if m["author"]["bot"] == true {
        return None;
    }
    Some(Inbound {
        platform: "discord".into(),
        conversation: channel.to_string(),
        is_group,
        sender_id: m["author"]["id"].as_str().unwrap_or("").to_string(),
        sender_name: m["author"]["username"].as_str().unwrap_or("").to_string(),
        text: m["content"].as_str().unwrap_or("").to_string(),
        platform_msg_id: m["id"].as_str()?.to_string(),
    })
}

/// Discord delivers messages to bots only over its gateway WebSocket or by
/// reading channels; reading the configured channels every few seconds needs
/// no public URL, like Telegram's poll. The newest id seen per channel is
/// kept in ~/.amux/bridge-discord.json, so a restart neither repeats nor
/// skips. A channel listed with a `dm:` prefix is a direct chat.
pub async fn discord_poll_once(state: &crate::api::AppState) -> Result<usize, String> {
    let (Some(token), Some(chans)) = (setting("DISCORD_BOT_TOKEN"), setting("DISCORD_CHANNELS")) else { return Ok(0) };
    let path = crate::config::amux_home().join("bridge-discord.json");
    let mut seen: serde_json::Map<String, Value> = std::fs::read_to_string(&path).ok()
        .and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let c = client();
    let mut n = 0;
    for raw in chans.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (chan, dm) = raw.strip_prefix("dm:").map(|c| (c, true)).unwrap_or((raw, false));
        let after = seen.get(chan).and_then(Value::as_str).map(str::to_string);
        let mut url = format!("{}/channels/{chan}/messages?limit=50", base("discord", "https://discord.com/api/v10"));
        if let Some(a) = &after { url.push_str(&format!("&after={a}")); }
        let r = c.get(url).header("Authorization", format!("Bot {token}")).send().await.map_err(|e| e.to_string())?;
        let status = r.status().as_u16();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        let Some(list) = v.as_array() else { return Err(format!("discord channel {chan} answered {status}")) };
        // Newest first from the API; file oldest first. The first read of a
        // channel only records where it is, so history is not replayed.
        let mut msgs: Vec<&Value> = list.iter().collect();
        msgs.sort_by_key(|m| m["id"].as_str().and_then(|s| s.parse::<u128>().ok()).unwrap_or(0));
        if let Some(last) = msgs.last().and_then(|m| m["id"].as_str()) {
            seen.insert(chan.to_string(), json!(last));
        }
        if after.is_none() {
            continue;
        }
        for m in msgs {
            if let Some(i) = discord_parse(chan, !dm, m) {
                super::inbound(state, i).await;
                n += 1;
            }
        }
    }
    let _ = std::fs::write(&path, serde_json::to_vec(&seen).unwrap_or_default());
    Ok(n)
}

pub async fn run_discord(state: crate::api::AppState) {
    loop {
        crate::runtime_jobs::registry::tick_every(crate::runtime_jobs::registry::ids::BRIDGE_DISCORD, Duration::from_secs(5));
        if let Err(e) = discord_poll_once(&state).await {
            tracing::warn!(error = %e, verdict = "bridge_discord_poll_failed", "bridge: reading Discord channels failed");
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        tokio::time::sleep(Duration::from_secs(if setting("DISCORD_BOT_TOKEN").is_some() { 4 } else { 30 })).await;
    }
}

pub fn spawn_discord(state: crate::api::AppState) -> tokio::task::JoinHandle<()> {
    crate::runtime_jobs::registry::spawn_loop(crate::runtime_jobs::registry::ids::BRIDGE_DISCORD,
        Some(Duration::from_secs(30)), move || run_discord(state.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whatsapp_payloads_parse_to_direct_and_group_messages() {
        let v = json!({"entry": [{"changes": [{"value": {
            "contacts": [{"wa_id": "15550102030", "profile": {"name": "Ethan"}}],
            "messages": [
                {"from": "15550102030", "id": "wamid.1", "type": "text", "text": {"body": "hi amux"}},
                {"from": "15550102030", "id": "wamid.2", "type": "text", "group_id": "120363@g.us", "text": {"body": "@amux status"}},
                {"from": "15550102030", "id": "wamid.3", "type": "image"}
            ]}}]}]});
        let got = whatsapp_parse(&v);
        assert_eq!(got.len(), 3);
        assert_eq!((got[0].conversation.as_str(), got[0].is_group, got[0].sender_name.as_str()), ("15550102030", false, "Ethan"));
        assert_eq!((got[1].conversation.as_str(), got[1].is_group), ("120363@g.us", true));
        assert_eq!(got[2].text, "", "a non-text message carries no text (the bridge ignores it)");
    }

    #[test]
    fn signatures_are_checked_against_the_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(dir.path());
        std::fs::write(dir.path().join("server.env"), "WHATSAPP_APP_SECRET=s3cret\nSLACK_SIGNING_SECRET=sl4ck\nWHATSAPP_VERIFY_TOKEN=vt\n").unwrap();
        let body = br#"{"x":1}"#;
        let good = format!("sha256={}", hmac_hex("s3cret", body));
        assert!(whatsapp_signature_ok(Some(&good), body));
        assert!(!whatsapp_signature_ok(Some(&good), br#"{"x":2}"#), "a changed body fails");
        assert!(!whatsapp_signature_ok(None, body));
        let ts = "1793000000";
        let sig = format!("v0={}", hmac_hex("sl4ck", format!("v0:{ts}:{{\"x\":1}}").as_bytes()));
        assert!(slack_signature_ok(Some(ts), Some(&sig), body, 1_793_000_100));
        assert!(!slack_signature_ok(Some(ts), Some(&sig), body, 1_793_000_400), "older than five minutes is a replay");
        assert_eq!(whatsapp_verify(Some("subscribe"), Some("vt"), Some("123")), Some("123".into()));
        assert_eq!(whatsapp_verify(Some("subscribe"), Some("no"), Some("123")), None);
    }

    #[test]
    fn slack_and_telegram_skip_what_is_not_a_person_writing() {
        let bot = json!({"type": "event_callback", "event": {"type": "message", "bot_id": "B1", "channel": "C1", "text": "x", "ts": "1"}});
        assert!(slack_parse(&bot).is_none(), "the bot's own messages never loop back");
        let edit = json!({"type": "event_callback", "event": {"type": "message", "subtype": "message_changed", "channel": "C1"}});
        assert!(slack_parse(&edit).is_none());
        let dm = json!({"type": "event_callback", "event": {"type": "message", "channel": "D1", "channel_type": "im", "user": "U1", "text": "hi", "ts": "5.1"}});
        let i = slack_parse(&dm).unwrap();
        assert_eq!((i.is_group, i.platform_msg_id.as_str()), (false, "D1:5.1"));
        let tg = json!({"update_id": 9, "message": {"message_id": 3, "chat": {"id": -100, "type": "supergroup"}, "from": {"id": 7, "username": "e"}, "text": "@amux hi"}});
        let t = telegram_parse(&tg).unwrap();
        assert_eq!((t.conversation.as_str(), t.is_group, t.sender_id.as_str()), ("-100", true, "7"));
        assert!(discord_parse("c", true, &json!({"id": "1", "author": {"bot": true}, "content": "x"})).is_none());
    }
}
