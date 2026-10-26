//! One transcription setting for every place amux turns speech into text:
//! Dictate (`/api/dictate`) and Recordings (the `recordings-transcribe` job).
//!
//! Ethan, 2026-10-25, looking at a sandbox recording that read "No local
//! transcriber: whisper-cli is not installed (brew install whisper-cpp)" while
//! Dictate on the same server transcribed fine through Gemini: "this should be
//! the same as dictate whatever is used there. we should have a centralized
//! transcription setting/config".
//!
//! The setting is the `transcription_engine` pref, changed in Settings >
//! Connect > Transcription:
//!
//! - `auto` (default): a local engine when this server has one, else Gemini.
//! - `local`: only a local engine; nothing leaves the server.
//! - `gemini`: only Gemini, with the same key Dictate uses (your own key from
//!   Settings, else the server's GOOGLE_API_KEY).
//!
//! The local engines differ by caller (Dictate keeps a warm openai-whisper
//! worker for short clips; Recordings run whisper.cpp over long files), so
//! each caller reports whether its own local engine is usable and [`choose`]
//! decides. That function is the one rule both follow.

use super::AppState;
use crate::db::{PendingEvent, WriteOutcome};
use amux_core::revision::{EntityType, MutationKind};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::{json, Value};

pub(crate) const PREF: &str = "transcription_engine";
/// Where a person changes it. Every "cannot transcribe" message names it.
pub(crate) const SETTING: &str = "Settings > Connect > Transcription";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pref {
    Auto,
    Local,
    Gemini,
}

impl Pref {
    pub(crate) fn parse(s: &str) -> Option<Pref> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Pref::Auto),
            "local" => Some(Pref::Local),
            "gemini" => Some(Pref::Gemini),
            _ => None,
        }
    }
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Pref::Auto => "auto",
            Pref::Local => "local",
            Pref::Gemini => "gemini",
        }
    }
}

/// The saved setting. Missing or unreadable reads as `auto`.
pub(crate) fn pref(conn: &rusqlite::Connection) -> Pref {
    conn.query_row("SELECT value FROM prefs WHERE key=?1", [PREF], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|v| Pref::parse(&v))
        .unwrap_or(Pref::Auto)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Engine {
    Local,
    Gemini,
    /// Nothing can transcribe, in words that name the setting and the fix.
    Unavailable(String),
}

/// THE rule: which engine a caller uses, from the setting, whether the
/// caller's local engine is usable (and why not), and whether a Gemini key is
/// configured.
pub(crate) fn choose(pref: Pref, local: Result<(), &str>, gemini: bool) -> Engine {
    match (pref, local, gemini) {
        (Pref::Gemini, _, true) | (Pref::Auto, Err(_), true) => Engine::Gemini,
        (Pref::Local | Pref::Auto, Ok(()), _) => Engine::Local,
        (Pref::Gemini, _, false) => Engine::Unavailable(format!(
            "{SETTING} is set to Gemini only, and no Gemini key is set. Add one there, or set GOOGLE_API_KEY in server.env."
        )),
        (Pref::Local, Err(why), _) => Engine::Unavailable(format!(
            "{SETTING} is set to Local only, and this server has no local engine ({why}). Choose Automatic or Gemini there, or install one."
        )),
        (Pref::Auto, Err(why), false) => Engine::Unavailable(format!(
            "No transcriber: this server has no local engine ({why}) and no Gemini key. Add a Gemini key in {SETTING} (or GOOGLE_API_KEY in server.env), or install a local engine."
        )),
    }
}

/// The Gemini key Dictate and Recordings share: the owner's own key saved in
/// Settings (write-only: never returned to a client), else the server's
/// GOOGLE_API_KEY. `(key, "byo" | "server")`; an empty key means none.
pub(crate) fn gemini_key(conn: &rusqlite::Connection) -> (String, &'static str) {
    super::dictation::dictation_key(conn)
}

/// How to install a missing tool on THIS host: the Homebrew formula on a Mac,
/// `elsewhere` (a full instruction) on any other system. A Linux box was told
/// to run `brew install whisper-cpp` (sandbox, 2026-10-25).
pub(crate) fn install_hint(brew: &str, elsewhere: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("brew install {brew}")
    } else {
        elsewhere.to_string()
    }
}

/// Transcribe audio with Gemini. `Ok("")` is an empty transcript.
pub(crate) async fn gemini_transcribe(key: &str, mime: &str, audio: &[u8], prompt: &str, timeout_s: u64) -> Result<String, String> {
    use base64::Engine as _;
    let parts = json!([
        { "text": prompt },
        { "inline_data": { "mime_type": mime, "data": base64::engine::general_purpose::STANDARD.encode(audio) } },
    ]);
    let (text, e) = super::dictation::gemini_generate(key, parts, timeout_s).await;
    if e.is_empty() {
        Ok(text)
    } else {
        Err(e)
    }
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/transcription", axum::routing::get(get_config).post(post_config))
}

/// What each caller uses right now and why, for the Settings section.
async fn status(state: &AppState) -> anyhow::Result<Value> {
    let store = state.store.clone();
    let (p, (key, src)) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = store.read()?;
        Ok((pref(&conn), gemini_key(&conn)))
    })
    .await??;
    let gemini = !key.is_empty();
    let dictate_local = super::dictation::local_status().await;
    let rec_local = tokio::task::spawn_blocking(super::recordings::local_status).await?;
    let engine_name = |e: &Engine, local_name: &str| -> Value {
        match e {
            Engine::Local => json!({"engine": local_name, "available": true}),
            Engine::Gemini => json!({"engine": "gemini", "available": true}),
            Engine::Unavailable(why) => json!({"engine": "none", "available": false, "why_unavailable": why}),
        }
    };
    let d = choose(p, dictate_local.as_ref().map(|_| ()).map_err(String::as_str), gemini);
    let r = choose(p, rec_local.as_ref().map(|_| ()).map_err(String::as_str), gemini);
    Ok(json!({
        "engine": p.as_str(),
        "setting": SETTING,
        "choices": ["auto", "local", "gemini"],
        "gemini": {"configured": gemini, "source": if gemini { src } else { "none" }, "model": super::dictation::dictation_model()},
        "dictate": {"uses": engine_name(&d, "whisper"), "local": match &dictate_local { Ok(m) => json!({"available": true, "model": m}), Err(w) => json!({"available": false, "why": w}) }},
        "recordings": {"uses": engine_name(&r, "whisper.cpp"), "local": match &rec_local { Ok(m) => json!({"available": true, "model": m}), Err(w) => json!({"available": false, "why": w}) }},
    }))
}

async fn get_config(State(state): State<AppState>) -> Response {
    match status(&state).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

async fn post_config(State(state): State<AppState>, body: axum::body::Bytes) -> Response {
    let v: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let Some(p) = v["engine"].as_str().and_then(Pref::parse) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "engine must be auto, local or gemini"}))).into_response();
    };
    let write = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO prefs (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=?2",
                rusqlite::params![PREF, p.as_str()],
            )?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![PendingEvent {
                    entity_type: EntityType::Other("pref".into()),
                    entity_id: PREF.into(),
                    mutation: MutationKind::Updated,
                    payload: None,
                }],
            })
        })
        .await;
    if let Err(e) = write {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response();
    }
    tracing::info!(engine = p.as_str(), measured = true, n_considered = 1, verdict = "transcription_engine_set",
        "transcription engine setting changed; Dictate and Recordings both follow it");
    // A recording that was waiting for an engine may have one now.
    crate::runtime_jobs::registry::trigger(crate::runtime_jobs::registry::ids::RECORDINGS_TRANSCRIBE);
    get_config(State(state)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_rule_for_every_setting_and_engine_state() {
        let none: Result<(), &str> = Err("whisper-cli is not installed");
        // Automatic: local first, Gemini when there is no local engine.
        assert_eq!(choose(Pref::Auto, Ok(()), true), Engine::Local);
        assert_eq!(choose(Pref::Auto, none, true), Engine::Gemini, "the sandbox: no whisper-cli, Dictate's Gemini key");
        assert!(matches!(choose(Pref::Auto, none, false), Engine::Unavailable(w) if w.contains(SETTING) && w.contains("GOOGLE_API_KEY") && w.contains("whisper-cli is not installed")));
        // Local only never reaches Gemini.
        assert_eq!(choose(Pref::Local, Ok(()), true), Engine::Local);
        assert!(matches!(choose(Pref::Local, none, true), Engine::Unavailable(w) if w.contains("Local only") && w.contains(SETTING)));
        // Gemini only never runs the local engine.
        assert_eq!(choose(Pref::Gemini, Ok(()), true), Engine::Gemini);
        assert!(matches!(choose(Pref::Gemini, Ok(()), false), Engine::Unavailable(w) if w.contains("Gemini only") && w.contains("GOOGLE_API_KEY")));
    }

    #[test]
    fn the_setting_parses_and_defaults_to_automatic() {
        assert_eq!(Pref::parse("Gemini"), Some(Pref::Gemini));
        assert_eq!(Pref::parse(" local "), Some(Pref::Local));
        assert_eq!(Pref::parse(""), Some(Pref::Auto));
        assert_eq!(Pref::parse("whisper"), None);
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE prefs (key TEXT PRIMARY KEY, value TEXT)", []).unwrap();
        assert_eq!(pref(&conn), Pref::Auto, "unset");
        conn.execute("INSERT INTO prefs VALUES (?1, 'gemini')", [PREF]).unwrap();
        assert_eq!(pref(&conn), Pref::Gemini);
        conn.execute("UPDATE prefs SET value='nonsense' WHERE key=?1", [PREF]).unwrap();
        assert_eq!(pref(&conn), Pref::Auto, "garbage reads as automatic");
    }

    #[test]
    fn no_brew_command_off_a_mac() {
        let h = install_hint("whisper-cpp", "build it from github.com/ggml-org/whisper.cpp");
        assert_eq!(h.starts_with("brew "), cfg!(target_os = "macos"));
    }
}
