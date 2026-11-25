//! Dictation API (SPA long-tail port): `/api/dictation/*` + `POST
//! /api/dictate` over the LIVE `dictation_history` / `dictation_dict`
//! tables (0001_baseline.sql), route- and field-compatible with the Python
//! handlers.
//!
//! Parity decisions, recorded so they are not "fixed" later:
//! - The TRANSCRIPTION ENGINE is NATIVE (AMUX-2598 cutover; this used to be
//!   the last dictation proxy row in py_proxy's PROXIED_FAMILIES). It is a
//!   line-for-line port of the Python engine (amux-server.py ~27217-27650 +
//!   ~72074): a warm openai-whisper WORKER subprocess (same inline worker
//!   script, `<model>.pt` presence detection in `~/.amux/models/whisper`
//!   then `~/.cache/whisper` (AMUX-4627), same
//!   `AMUX_WHISPER_MODEL` / `AMUX_WHISPER_PYTHON` env knobs) preferred, the
//!   Gemini `generateContent` API as fallback and as the AI-edit engine
//!   (same `AMUX_DICTATION_MODEL` default `gemini-2.5-flash`, same BYO-key
//!   pref `dictation_gemini_key` beating env `GOOGLE_API_KEY`). Whisper
//!   output gets the same deterministic session-name recovery pass
//!   (`_dictation_fix_names`), including a byte-exact difflib
//!   `SequenceMatcher.ratio` port — 0/7 -> 7/7 session names in Python's
//!   own benchmark hinged on it. If neither engine is present the answer is
//!   Python's honest 503 naming what to install; a transcription is never
//!   fabricated (ethos rule 3).
//! - The interpreter that hosts the whisper worker is discovered by
//!   ABSOLUTE path (env override, then PATH, then the fixed Homebrew /
//!   system locations) because launchd starts this server with no shell
//!   PATH — `Command::new("python3.11")` alone would report the engine
//!   absent on the exact machine it lives on.
//! - Unlike Python's blocking `readline()`, worker reads carry generous
//!   timeouts (start 180s, transcribe 600s): a wedged worker under the
//!   Python design hangs the request AND the engine mutex forever; here it
//!   is killed and reported, and the next request restarts it.
//! - `GET /history` `count=` mode mirrors Python's `parse_qs` truthiness:
//!   the mode triggers on a NON-EMPTY `count` value (`?count=` is dropped by
//!   parse_qs and does not trigger it).
//! - `total_words` sums the WHOLE table even when `session=` filters items —
//!   that is what Python computes, so the SPA's total badge matches.
//! - Dict rows honor `UNIQUE(word, correct)`: a duplicate POST answers
//!   `{"ok": true, "already": true}` (200), exactly Python's
//!   IntegrityError branch.
//! - `PATCH /dict/{id}` overwrites BOTH columns with `body.get(k) or ""`
//!   defaults — a partial PATCH blanks the missing field, as in Python.
//! - Non-numeric `{id}` path segments fall to Python's trailing
//!   `{"error": "dictation route not found"}` 404, not an axum 400.

use super::calendar::query_rows_json;
use super::AppState;
use crate::db::{PendingEvent, WriteOutcome};
use amux_core::revision::{EntityType, MutationKind};
use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/history", get(history))
        .route("/history/{id}", axum::routing::delete(delete_history))
        .route("/history/{id}/edit", axum::routing::post(edit_history))
        .route("/dict", get(list_dict).post(add_dict))
        // What dictation listens with and knows about, kept on the server so every device
        // (browser, Mac app, phone) dictates the same way (DictSettings).
        .route("/settings", get(get_settings).put(put_settings))
        .route(
            "/dict/{id}",
            axum::routing::patch(patch_dict).delete(delete_dict),
        )
        // Config describes/configures the NATIVE transcription engine
        // (AMUX-2598: `local`/`engine`/`source` report the engine
        // /api/dictate itself uses). `any`: Python answers its dictation
        // 404 for non-GET/POST methods, so the handler dispatches methods.
        .route("/config", axum::routing::any(config))
        // Python's trailing `return self._json({"error": "dictation route
        // not found"}, 404)` for anything else under /api/dictation.
        // EXPLICIT wildcard routes, not `.fallback()`: in the full
        // composition the static SPA catch-all (`/{*path}`) out-competes a
        // nested fallback, so the fallback answered index.html/generic 404
        // instead of this module's Python-shape 404 (caught live on 18940).
        .route("/", axum::routing::any(|| async { route_not_found() }))
        .route(
            "/{*rest}",
            axum::routing::any(|| async { route_not_found() }),
        )
}

// ---- shared helpers -------------------------------------------------------

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

use super::internal;

fn route_not_found() -> Response {
    err(
        StatusCode::NOT_FOUND,
        json!({ "error": "dictation route not found" }),
    )
}

fn ev(entity: &str, id: &str, mutation: MutationKind) -> PendingEvent {
    PendingEvent {
        entity_type: EntityType::Other(entity.into()),
        entity_id: id.to_string(),
        mutation,
        payload: None,
    }
}

/// Python truthiness for `body.get("undo")`.
fn is_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Python `s[:n]` truncates by CHARACTERS, not bytes.
fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python `re.match(r"\d+")` route ids: non-numeric falls to the module 404.
fn parse_id(id: &str) -> Option<i64> {
    id.parse::<i64>().ok()
}

// ---- POST /api/dictate ----------------------------------------------------

/// Query params (py:72085-72097): raw-binary uploads carry
/// `?session=&dur_ms=&mime=`; the JSON shape carries them in the body.
#[derive(serde::Deserialize)]
pub struct DictateQuery {
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    dur_ms: Option<String>,
    #[serde(default)]
    mime: Option<String>,
}

/// Native transcription (audio -> text), ported from py:72079-72145.
/// LOCAL whisper first (~1.1s vs ~12.5s Gemini round trip, works with the
/// uplink dead, needs no key) + the deterministic session-name pass that
/// makes whisper output usable; Gemini is the fallback. Same request
/// shapes: raw binary body (preferred — base64 costs 33% more bytes on a
/// phone) or JSON `{audio: base64}`.
pub async fn dictate(
    State(state): State<AppState>,
    Query(q): Query<DictateQuery>,
    req: Request,
) -> Response {
    let ctype = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let q_session = truncate_chars(q.session.as_deref().unwrap_or("").trim(), 64);
    let q_dur = q
        .dur_ms
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);

    // (base64 for Gemini, decoded bytes for whisper, mime, session, dur_ms)
    let (b64, raw_audio, mime, session, dur_ms): (String, Option<Vec<u8>>, String, String, i64);
    // A take the client already transcribed (the Mac app hears it on device,
    // desktop/amux-dictation-core.swift): its text and the engine that made
    // it. The names pass, history and kept audio still happen here.
    let mut device_text: Option<(String, String)> = None;
    if !ctype.is_empty() && ctype != "application/json" {
        // RAW BINARY upload. Python 413s on the Content-Length header
        // (py:72087-72089); the actual body size is checked too so a
        // missing/lying header cannot smuggle an oversized clip.
        let clen = req
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        if clen > DICTATION_MAX_BYTES {
            return err(
                StatusCode::PAYLOAD_TOO_LARGE,
                json!({ "error": "audio too large (max 25MB)" }),
            );
        }
        let bytes = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
            Ok(b) => b,
            Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e.to_string() })),
        };
        if bytes.len() > DICTATION_MAX_BYTES {
            return err(
                StatusCode::PAYLOAD_TOO_LARGE,
                json!({ "error": "audio too large (max 25MB)" }),
            );
        }
        if bytes.is_empty() {
            return err(
                StatusCode::BAD_REQUEST,
                json!({ "error": "audio required" }),
            );
        }
        b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        raw_audio = Some(bytes.to_vec());
        // `(qs.get("mime", [ctype])[0] or ctype).split(";")[0]`
        let m = q.mime.as_deref().unwrap_or(&ctype);
        let m = if m.is_empty() { ctype.as_str() } else { m };
        mime = m.split(';').next().unwrap_or("").to_string();
        session = q_session;
        dur_ms = q_dur;
    } else {
        let bytes = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
            Ok(b) => b,
            Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e.to_string() })),
        };
        // Python's `_read_body` tolerance: unparseable reads as `{}`.
        let body: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
        let audio = body
            .get("audio")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if let Some(t) = body.get("text").and_then(Value::as_str) {
            let engine: String = body
                .get("engine")
                .and_then(Value::as_str)
                .unwrap_or("device")
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                .take(24)
                .collect();
            let engine = if engine.is_empty() { "device".to_string() } else { engine };
            device_text = Some((t.trim().to_string(), engine));
        }
        if audio.is_empty() && device_text.is_none() {
            return err(
                StatusCode::BAD_REQUEST,
                json!({ "error": "audio required" }),
            );
        }
        if audio.len() > DICTATION_MAX_BYTES * 4 / 3 {
            return err(
                StatusCode::PAYLOAD_TOO_LARGE,
                json!({ "error": "audio too large (max ~25MB)" }),
            );
        }
        b64 = audio;
        raw_audio = None;
        let m = body
            .get("mime")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("audio/webm");
        mime = m.split(';').next().unwrap_or("").to_string();
        let s = body
            .get("session")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&q_session);
        session = truncate_chars(s.trim(), 64);
        // `int(body.get("dur_ms") or 0)`
        dur_ms = body
            .get("dur_ms")
            .map(|v| {
                v.as_i64()
                    .or_else(|| v.as_f64().map(|f| f as i64))
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    .unwrap_or(0)
            })
            .unwrap_or(0);
    }

    // THE AUDIO IS KEPT (Ethan, 2026-10-29: a worker made from dictation
    // could not be retested against what was said, because the clip was gone
    // once transcribed). Saved under dictation-audio/<history id>.<ext> below.
    let keep_audio: Option<Vec<u8>> = raw_audio
        .clone()
        .or_else(|| b64_decode_lenient(&b64).ok())
        .filter(|a| !a.is_empty());
    let t0 = std::time::Instant::now();
    let mut text = String::new();
    let mut werr = String::new();
    let mut engine = "";
    let mut raw_text: Option<String> = None;
    let device_engine: String;
    if let Some((t, e)) = device_text {
        let store = state.store.clone();
        let raw = t.clone();
        let fixed = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let conn = store.read()?;
            Ok(fix_names_with_targets(&raw, &dn_targets(&conn), 0.86))
        })
        .await;
        text = match fixed {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => return internal(e),
            Err(e) => return internal(e),
        };
        raw_text = Some(t);
        device_engine = e;
        engine = device_engine.as_str();
    }
    // The shared transcription setting (Settings > Connect > Transcription)
    // decides which engines may run; Recordings follow the same one.
    let pref = {
        let store = state.store.clone();
        tokio::task::spawn_blocking(move || store.read().map(|c| super::transcription::pref(&c)))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(super::transcription::Pref::Auto)
    };
    let use_local = pref != super::transcription::Pref::Gemini;
    let use_gemini = pref != super::transcription::Pref::Local;
    // LOCAL FIRST (py:72108-72119). Whisper's raw output mangles every
    // session name, so the deterministic pass is not optional — it is what
    // makes this path usable (0/7 -> 7/7 names in the benchmark).
    if engine.is_empty() && use_local && whisper_available().await {
        let raw = match raw_audio {
            Some(r) => Some(r),
            None => match b64_decode_lenient(&b64) {
                Ok(r) => Some(r),
                // Python's b64decode would raise out of the handler here; an
                // honest 500 beats handing Gemini the same broken payload.
                Err(e) => {
                    return err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        json!({ "error": format!("bad base64 audio: {e}") }),
                    )
                }
            },
        };
        if let Some(raw) = raw {
            let store = state.store.clone();
            let prompt = tokio::task::spawn_blocking(move || store.read().map(|c| local_vocab_prompt(&c)).unwrap_or_default())
                .await
                .unwrap_or_default();
            let (t, e, local_engine) = whisper_transcribe(&raw, &mime, &prompt).await;
            werr = e;
            let outcome = read_local_result(&t, &werr);
            let outcome_was_no_speech = outcome == LocalResult::NoSpeech;
            if let LocalResult::Text(t) = outcome {
                let store = state.store.clone();
                let fixed = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
                    let conn = store.read()?;
                    Ok(fix_names_with_targets(&t, &dn_targets(&conn), 0.86))
                })
                .await;
                match fixed {
                    Ok(Ok(t)) => text = t,
                    Ok(Err(e)) => return internal(e),
                    Err(e) => return internal(e),
                }
                engine = local_engine;
            } else if outcome_was_no_speech {
                // THE ENGINE RAN AND HEARD NO SPEECH. That is a RESULT, not a
                // failure, and escalating it is how silence became a sentence
                // (AMUX-4675).
                //
                // Measured on the live server 2026-09-15: a synthesised 1s
                // 440Hz tone came back from the paid engine as 68 words reading
                // fleet lane names; a 0.13s clip came back as "I need to open up
                // a new session for ai-for-smbs". Neither sentence is in the
                // audio. This text is what the dictation UI hands the user to
                // SEND, so a silent clip did not produce silence, it produced a
                // confident fleet-specific instruction someone could send to a
                // worker believing they had said it. The invented content looks
                // like the custom dictionary echoed back, which is exactly what
                // makes it plausible enough to send.
                //
                // The old branch could not tell the two apart: it keyed on
                // `!t.is_empty()` alone, so an empty result with no error read
                // as failure and logged "local transcribe failed ()" — naming
                // no cause, because there was none.
                //
                // The local engine was never broken: its own worker returns
                // {"text": "Testing 123"} on a real speech clip (AMUX-4627).
                engine = local_engine;
                tracing::info!(
                    measured = true,
                    n_considered = 1,
                    verdict = "dictation_local_empty_is_a_result",
                    "[dictation] local engine heard no speech; returning an empty transcription \
                     rather than escalating to the paid engine"
                );
            } else {
                tracing::warn!(
                    measured = true, n_considered = 1,
                    verdict = "dictation_local_failed",
                    error = %werr,
                    "[dictation] local transcribe FAILED — trying Gemini"
                );
            }
        }
    }
    // Gemini is the FALLBACK (py:72120-72135), not the single point of
    // failure it used to be.
    //
    // `engine` is the gate, not `text` (AMUX-4675). An empty string is the
    // right answer for a clip with no speech, and reaching the paid engine on
    // it is what turned silence into invented instructions. Once the local
    // engine has ANSWERED — even with nothing — the question is settled and
    // there is nothing left to fall back from.
    if text.is_empty() && engine.is_empty() && !use_gemini {
        let why = if werr.is_empty() { local_status().await.err().unwrap_or_default() } else { werr };
        let msg = match super::transcription::choose(pref, Err(&why), false) {
            super::transcription::Engine::Unavailable(m) => m,
            _ => why,
        };
        return err(StatusCode::SERVICE_UNAVAILABLE, json!({ "error": msg }));
    }
    if text.is_empty() && engine.is_empty() {
        let store = state.store.clone();
        let sess = session.clone();
        let keyed = tokio::task::spawn_blocking(
            move || -> anyhow::Result<(String, &'static str, String)> {
                let conn = store.read()?;
                let (key, src) = dictation_key(&conn);
                let prompt = dictation_prompt(&conn, &sess);
                Ok((key, src, prompt))
            },
        )
        .await;
        let (key, src, prompt) = match keyed {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return internal(e),
            Err(e) => return internal(e),
        };
        if key.is_empty() {
            let local_why = if werr.is_empty() { local_status().await.err().unwrap_or_default() } else { werr };
            let msg = match super::transcription::choose(pref, Err(&local_why), false) {
                super::transcription::Engine::Unavailable(m) => m,
                _ => local_why,
            };
            return err(StatusCode::SERVICE_UNAVAILABLE, json!({ "error": msg }));
        }
        let parts = json!([
            { "text": format!("{prompt}\n\nTranscribe and clean this dictation:") },
            { "inline_data": { "mime_type": mime, "data": b64 } },
        ]);
        let (t, e) = gemini_generate(&key, parts, 90).await;
        if !e.is_empty() {
            tracing::warn!("[dictation] {src} key failed: {e}");
            // THE AUDIO ITSELF WAS REFUSED (AF-1361). Gemini's 400 "Unable to
            // process input audio" answers the same way for the same bytes, so
            // a 502 made the page retry it: the owner's Mac app sent 6.6 KB
            // clips stamped 24.8 h long 24 times in 4 h (2026-11-23), each
            // refused. 422 with a code tells the page to stop and say so.
            if audio_rejected(&e) {
                tracing::warn!(
                    session = %session,
                    dur_ms,
                    b64_len = b64.len(),
                    measured = true,
                    n_considered = 1,
                    verdict = "dictation_audio_unreadable",
                    "dictation: the engine refused this audio as empty or corrupt; a retry sends the same bytes"
                );
                return err(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    json!({ "error": e, "code": "audio_unreadable" }),
                );
            }
            return err(StatusCode::BAD_GATEWAY, json!({ "error": e }));
        }
        text = t;
        engine = "gemini";
    }

    // Spoken lists come out as lists (owner, 2026-11-17: "the formatting stuff
    // like Wispr Flow does, where I'm doing a list ... format the output
    // accordingly"). Every engine's text passes here, the Mac app's included.
    let listed = format_spoken_list(&text);
    if listed != text {
        tracing::info!(engine, items = listed.lines().filter(|l| l.starts_with(|c: char| c.is_ascii_digit())).count(),
            measured = true, n_considered = 1, verdict = "dictation_list_formatted",
            "[dictation] a spoken list was formatted as a numbered list");
    }
    // Then the meta-task model formats it (owner, 2026-11-25: "it should be run
    // through the lightweight meta model that we've configured"), within a
    // short limit: a slow or wrong answer keeps the rule-based formatting.
    text = match format_with_helper(&text).await {
        Some(formatted) => formatted,
        None => listed,
    };
    let words = text.split_whitespace().count() as i64;
    let ts = chrono::Utc::now().timestamp_millis();
    let slot: Arc<Mutex<Option<i64>>> = Arc::new(Mutex::new(None));
    let (slot_w, text_w, session_w) = (slot.clone(), text.clone(), session.clone());
    let raw_w = raw_text.unwrap_or_else(|| text.clone());
    let write = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO dictation_history (session, ts, text, raw_text, words, dur_ms) \
                 VALUES (?1,?2,?3,?4,?5,?6)",
                rusqlite::params![session_w, ts, text_w, raw_w, words, dur_ms],
            )?;
            let id = conn.last_insert_rowid();
            *slot_w.lock().expect("slot") = Some(id);
            // Also record the transcript in cmd_history so a dictation shows up
            // in the Messages list like a typed message (Ethan, 2026-08-17).
            // ts is milliseconds (the cmd_history convention); origin='dictation'
            // marks how it arrived.
            conn.execute(
                "INSERT INTO cmd_history (text, type, session, ts, origin) \
                 VALUES (?1, 'user', ?2, ?3, 'dictation')",
                rusqlite::params![text_w, session_w, ts],
            )?;
            let mid = conn.last_insert_rowid();
            Ok(WriteOutcome {
                applied: true,
                events: vec![
                    ev("dictation_history", &id.to_string(), MutationKind::Created),
                    ev("cmd_history", &mid.to_string(), MutationKind::Created),
                ],
            })
        })
        .await;
    let id = match write {
        Ok(_) => slot.lock().expect("slot").take().unwrap_or(0),
        Err(e) => return internal(e),
    };
    let audio = keep_audio.and_then(|bytes| save_audio(id, &mime, &bytes));
    let secs = (t0.elapsed().as_secs_f64() * 100.0).round() / 100.0;
    tracing::info!("[dictation] {words}w in {secs:.1}s via {engine}");
    Json(json!({ "id": id, "text": text, "words": words, "engine": engine, "secs": secs, "audio": audio }))
        .into_response()
}

fn audio_dir() -> PathBuf {
    // Tests share the real AMUX_HOME with the running server, so their clips go elsewhere.
    if cfg!(test) {
        return std::env::temp_dir().join(format!("amux-dictation-audio-test-{}", std::process::id()));
    }
    amux_home().join("dictation-audio")
}

fn audio_ext(mime: &str) -> &'static str {
    match mime {
        "audio/webm" | "video/webm" => "webm",
        "audio/ogg" => "ogg",
        "audio/mp4" | "audio/x-m4a" | "audio/m4a" | "video/mp4" | "audio/aac" => "m4a",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/mpeg" | "audio/mp3" => "mp3",
        _ => "bin",
    }
}

/// Keep one dictation clip beside its history row. A failure is a WARN, not
/// an error for the caller: the transcription it asked for still stands.
fn save_audio(id: i64, mime: &str, bytes: &[u8]) -> Option<String> {
    if id <= 0 || bytes.is_empty() {
        return None;
    }
    let dir = audio_dir();
    let path = dir.join(format!("{id}.{}", audio_ext(mime)));
    let res = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, bytes));
    match res {
        Ok(()) => Some(path.to_string_lossy().into_owned()),
        Err(e) => {
            tracing::warn!(id, error = %e, measured = true, n_considered = 1, verdict = "dictation_audio_not_saved",
                "[dictation] the clip was transcribed but could not be saved; it cannot be replayed");
            None
        }
    }
}

/// Remove a deleted history row's clip, whatever its extension.
fn remove_audio(id: i64) {
    let Ok(rd) = std::fs::read_dir(audio_dir()) else { return };
    let prefix = format!("{id}.");
    for e in rd.flatten() {
        if e.file_name().to_str().is_some_and(|n| n.starts_with(&prefix)) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// One recording to text through the configured engines (local first, then
/// Gemini), for callers other than the dictation box: no history row, and
/// the same rule that silence is an empty answer, not a reason to escalate.
/// Gemini refused the audio itself (a 400 about the input audio), as opposed
/// to a key, quota or network failure that a later retry can get past.
fn audio_rejected(e: &str) -> bool {
    e.starts_with("gemini 400:") && e.contains("input audio")
}

pub(crate) async fn transcribe_clip(state: &AppState, raw: &[u8], mime: &str) -> Result<(String, &'static str), String> {
    let pref = {
        let store = state.store.clone();
        tokio::task::spawn_blocking(move || store.read().map(|c| super::transcription::pref(&c)))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(super::transcription::Pref::Auto)
    };
    let mut why = String::new();
    if pref != super::transcription::Pref::Gemini && whisper_available().await {
        let store = state.store.clone();
        let prompt = tokio::task::spawn_blocking(move || store.read().map(|c| local_vocab_prompt(&c)).unwrap_or_default())
            .await
            .unwrap_or_default();
        let (t, e, local_engine) = whisper_transcribe(raw, mime, &prompt).await;
        match read_local_result(&t, &e) {
            LocalResult::Text(t) => return Ok((t, local_engine)),
            LocalResult::NoSpeech => return Ok((String::new(), local_engine)),
            LocalResult::Failed(e) => why = e,
        }
    }
    if pref == super::transcription::Pref::Local {
        return Err(if why.is_empty() { local_status().await.err().unwrap_or_else(|| "local transcription is unavailable".into()) } else { why });
    }
    let store = state.store.clone();
    let key = tokio::task::spawn_blocking(move || store.read().map(|c| dictation_key(&c).0))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    if key.is_empty() {
        return Err("no transcription engine: local Whisper is unavailable and no Gemini key is set".into());
    }
    let parts = json!([
        { "text": "Transcribe this recording verbatim. Return only the transcript." },
        { "inline_data": { "mime_type": mime, "data": base64::engine::general_purpose::STANDARD.encode(raw) } },
    ]);
    let (t, e) = gemini_generate(&key, parts, 180).await;
    if !e.is_empty() {
        return Err(e);
    }
    Ok((t, "gemini"))
}

// ---- GET /api/dictation/history -------------------------------------------

#[derive(serde::Deserialize)]
pub struct HistoryParams {
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    limit: Option<String>,
    #[serde(default)]
    count: Option<String>,
}

const HISTORY_COLS: &str = "id, session, ts, text, raw_text, prev_text, ai_edited, words, dur_ms";

pub async fn history(State(state): State<AppState>, Query(p): Query<HistoryParams>) -> Response {
    let sess = p.session.unwrap_or_default();
    // Python: min(int(limit or 200), 500) — no lower clamp (kept as-is).
    let limit = p
        .limit
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(200)
        .min(500);
    let count_mode = p.count.as_deref().is_some_and(|c| !c.is_empty());
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let conn = store.read()?;
        if count_mode {
            let n: i64 = if sess.is_empty() {
                conn.query_row("SELECT COUNT(*) FROM dictation_history", [], |r| r.get(0))?
            } else {
                conn.query_row(
                    "SELECT COUNT(*) FROM dictation_history WHERE session=?1",
                    [&sess],
                    |r| r.get(0),
                )?
            };
            return Ok(json!({ "count": n }));
        }
        let items = if sess.is_empty() {
            query_rows_json(
                &conn,
                &format!("SELECT {HISTORY_COLS} FROM dictation_history ORDER BY ts DESC LIMIT ?1"),
                &[&limit],
            )?
        } else {
            query_rows_json(
                &conn,
                &format!(
                    "SELECT {HISTORY_COLS} FROM dictation_history WHERE session=?1 \
                     ORDER BY ts DESC LIMIT ?2"
                ),
                &[&sess, &limit],
            )?
        };
        // Whole-table total even when session-filtered (Python parity).
        let total: i64 = conn.query_row(
            "SELECT COALESCE(SUM(words),0) FROM dictation_history",
            [],
            |r| r.get(0),
        )?;
        Ok(json!({ "items": items, "total_words": total }))
    })
    .await;
    match joined {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---- DELETE /api/dictation/history/{id} -----------------------------------

pub async fn delete_history(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(rid) = parse_id(&id) else {
        return route_not_found();
    };
    let write = state
        .store
        .write_async(move |conn| {
            let n = conn.execute("DELETE FROM dictation_history WHERE id=?1", [rid])?;
            let events = if n > 0 {
                vec![ev(
                    "dictation_history",
                    &rid.to_string(),
                    MutationKind::Deleted,
                )]
            } else {
                vec![]
            };
            Ok(WriteOutcome {
                applied: n > 0,
                events,
            })
        })
        .await;
    match write {
        // Python answers ok whether or not the row existed.
        Ok(_) => {
            remove_audio(rid);
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => internal(e),
    }
}

// ---- POST /api/dictation/history/{id}/edit --------------------------------

pub async fn edit_history(
    State(state): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> Response {
    let Some(rid) = parse_id(&id) else {
        return route_not_found();
    };
    let body_bytes = match axum::body::to_bytes(req.into_body(), 16 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e.to_string() })),
    };
    // Python's `_read_body` tolerance: an unparseable body reads as `{}`.
    let body: Value = serde_json::from_slice(&body_bytes).unwrap_or_else(|_| json!({}));
    let store = state.store.clone();
    let looked = tokio::task::spawn_blocking(
        move || -> anyhow::Result<Option<(String, String, String)>> {
            let conn = store.read()?;
            let row = conn
                .query_row(
                    "SELECT text, raw_text, prev_text FROM dictation_history WHERE id=?1",
                    [rid],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })?;
            Ok(row)
        },
    )
    .await;
    let (old_text, raw_text, prev_text) = match looked {
        Ok(Ok(Some(row))) => row,
        Ok(Ok(None)) => return err(StatusCode::NOT_FOUND, json!({ "error": "not found" })),
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(e),
    };

    if is_truthy(body.get("undo")) {
        // Pure-DB undo: `prev_text or raw_text` (Python falsy-string chain).
        let prev = if prev_text.is_empty() {
            raw_text
        } else {
            prev_text
        };
        let prev_w = prev.clone();
        let write = state
            .store
            .write_async(move |conn| {
                let n = conn.execute(
                    "UPDATE dictation_history SET text=?1, ai_edited=0, prev_text='' WHERE id=?2",
                    rusqlite::params![prev_w, rid],
                )?;
                let events = if n > 0 {
                    vec![ev(
                        "dictation_history",
                        &rid.to_string(),
                        MutationKind::Updated,
                    )]
                } else {
                    vec![]
                };
                Ok(WriteOutcome {
                    applied: n > 0,
                    events,
                })
            })
            .await;
        return match write {
            Ok(_) => Json(json!({ "ok": true, "text": prev, "ai_edited": 0 })).into_response(),
            Err(e) => internal(e),
        };
    }

    // AI-edit path, NATIVE (py:72190-72203): the same Gemini call the
    // Python engine makes — `dictation_gemini_key` pref beats env
    // GOOGLE_API_KEY, model from AMUX_DICTATION_MODEL. Same 503 with no
    // key, same 502 on a Gemini error, same success shape.
    let instruction = truncate_chars(
        body.get("instruction")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("Fix grammar and make it clearer.")
            .trim(),
        500,
    );
    let store = state.store.clone();
    let keyed = tokio::task::spawn_blocking(move || -> anyhow::Result<(String, &'static str)> {
        let conn = store.read()?;
        Ok(dictation_key(&conn))
    })
    .await;
    let key = match keyed {
        Ok(Ok((key, _src))) => key,
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(e),
    };
    if key.is_empty() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "error": "no Gemini key configured" }),
        );
    }
    let parts = json!([{ "text": format!(
        "Edit this dictated text per the instruction. Output ONLY the edited text, \
         no commentary.\n\nInstruction: {instruction}\n\nText: {old_text}"
    ) }]);
    let (new_text, gerr) = gemini_generate(&key, parts, 90).await;
    if !gerr.is_empty() {
        return err(StatusCode::BAD_GATEWAY, json!({ "error": gerr }));
    }
    let (new_w, old_w) = (new_text.clone(), old_text.clone());
    let write = state
        .store
        .write_async(move |conn| {
            let n = conn.execute(
                "UPDATE dictation_history SET text=?1, prev_text=?2, ai_edited=1 WHERE id=?3",
                rusqlite::params![new_w, old_w, rid],
            )?;
            let events = if n > 0 {
                vec![ev(
                    "dictation_history",
                    &rid.to_string(),
                    MutationKind::Updated,
                )]
            } else {
                vec![]
            };
            Ok(WriteOutcome {
                applied: n > 0,
                events,
            })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true, "text": new_text, "ai_edited": 1 })).into_response(),
        Err(e) => internal(e),
    }
}

// ---- GET/POST /api/dictation/dict -----------------------------------------

pub async fn list_dict(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        Ok(query_rows_json(
            &conn,
            "SELECT id, word, correct, created FROM dictation_dict ORDER BY created DESC",
            &[],
        )?)
    })
    .await;
    match joined {
        Ok(Ok(rows)) => Json(Value::Array(rows)).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

/// Insert outcome carried out of the write closure.
enum DictInsert {
    Created(i64),
    Already,
}

pub async fn add_dict(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let word = truncate_chars(
        body.get("word")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim(),
        120,
    );
    let correct = truncate_chars(
        body.get("correct")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim(),
        120,
    );
    if word.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "word required" }));
    }
    let slot: Arc<Mutex<Option<DictInsert>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let created = chrono::Utc::now().timestamp();
            match conn.execute(
                "INSERT INTO dictation_dict (word, correct, created) VALUES (?1,?2,?3)",
                rusqlite::params![word, correct, created],
            ) {
                Ok(_) => {
                    let id = conn.last_insert_rowid();
                    *slot_w.lock().expect("slot") = Some(DictInsert::Created(id));
                    Ok(WriteOutcome {
                        applied: true,
                        events: vec![ev("dictation_dict", &id.to_string(), MutationKind::Created)],
                    })
                }
                // UNIQUE(word, correct) — Python's IntegrityError branch.
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    *slot_w.lock().expect("slot") = Some(DictInsert::Already);
                    Ok(WriteOutcome {
                        applied: false,
                        events: vec![],
                    })
                }
                Err(other) => Err(other),
            }
        })
        .await;
    match write {
        Ok(_) => match slot.lock().expect("slot").take() {
            Some(DictInsert::Created(id)) => {
                (StatusCode::CREATED, Json(json!({ "ok": true, "id": id }))).into_response()
            }
            _ => Json(json!({ "ok": true, "already": true })).into_response(),
        },
        Err(e) => internal(e),
    }
}

// ---- PATCH/DELETE /api/dictation/dict/{id} --------------------------------

pub async fn patch_dict(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(did) = parse_id(&id) else {
        return route_not_found();
    };
    // Python sets BOTH columns from `(body.get(k) or "")` — a missing field
    // blanks the column. Kept identical.
    let word = truncate_chars(
        body.get("word")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim(),
        120,
    );
    let correct = truncate_chars(
        body.get("correct")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim(),
        120,
    );
    let write = state
        .store
        .write_async(move |conn| {
            let n = conn.execute(
                "UPDATE dictation_dict SET word=?1, correct=?2 WHERE id=?3",
                rusqlite::params![word, correct, did],
            )?;
            let events = if n > 0 {
                vec![ev(
                    "dictation_dict",
                    &did.to_string(),
                    MutationKind::Updated,
                )]
            } else {
                vec![]
            };
            Ok(WriteOutcome {
                applied: n > 0,
                events,
            })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

pub async fn delete_dict(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(did) = parse_id(&id) else {
        return route_not_found();
    };
    let write = state
        .store
        .write_async(move |conn| {
            let n = conn.execute("DELETE FROM dictation_dict WHERE id=?1", [did])?;
            let events = if n > 0 {
                vec![ev(
                    "dictation_dict",
                    &did.to_string(),
                    MutationKind::Deleted,
                )]
            } else {
                vec![]
            };
            Ok(WriteOutcome {
                applied: n > 0,
                events,
            })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------------
// The transcription engine — native port of amux-server.py ~27217-27650.
// ---------------------------------------------------------------------------

/// py:27223 `_DICTATION_MAX_BYTES` — ~25MB of audio per clip.
const DICTATION_MAX_BYTES: usize = 25 * 1024 * 1024;

/// py:27222 `_DICTATION_MODEL`.
pub(crate) fn dictation_model() -> String {
    std::env::var("AMUX_DICTATION_MODEL").unwrap_or_else(|_| "gemini-2.5-flash".into())
}

use crate::config::amux_home;

/// Env half of the key lookup, override-able in tests (process env reads
/// are not hermetic under parallel tests).
fn env_gemini_key() -> String {
    #[cfg(test)]
    if let Some(v) = tests::GEMINI_KEY_OVERRIDE
        .lock()
        .expect("key override")
        .clone()
    {
        return v;
    }
    std::env::var("GOOGLE_API_KEY")
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// py:27226 `_dictation_key` — (key, source). BYO key from prefs wins
/// (WRITE-ONLY: readable by the server, never returned to a client); else
/// the server's env key.
pub(crate) fn dictation_key(conn: &rusqlite::Connection) -> (String, &'static str) {
    let byo: Option<String> = conn
        .query_row(
            "SELECT value FROM prefs WHERE key='dictation_gemini_key'",
            [],
            |r| r.get(0),
        )
        .ok();
    if let Some(k) = byo {
        let k = k.trim().to_string();
        if !k.is_empty() {
            return (k, "byo");
        }
    }
    (env_gemini_key(), "server")
}

// ---- settings: microphone and context sources ------------------------------

/// The `prefs` key holding `DictSettings` as JSON.
pub(crate) const SETTINGS_KEY: &str = "dictation_settings";
const MIC_MAX: usize = 200;
const CONTEXT_MAX: usize = 4000;
const CONTACTS_MAX: usize = 200;

/// Dictation settings shared by every device. `mic` is a microphone's NAME, because a
/// browser's device ids differ per site and per device; each client matches the name
/// against its own inputs and says so when the microphone is not connected there.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub(crate) struct DictSettings {
    /// "" is the system default.
    pub mic: String,
    /// Free text about the speaker and what they talk about, given to the transcriber.
    pub context: String,
    /// Worker names as a source: the prompt lists them and the name-recovery pass uses them.
    pub workers: bool,
    /// CRM contact names as a source, for the people you dictate about.
    pub contacts: bool,
}

impl Default for DictSettings {
    fn default() -> Self {
        Self { mic: String::new(), context: String::new(), workers: true, contacts: false }
    }
}

pub(crate) fn read_settings(conn: &rusqlite::Connection) -> DictSettings {
    conn.query_row("SELECT value FROM prefs WHERE key = ?1", [SETTINGS_KEY], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|v| match serde_json::from_str(&v) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("dictation_settings verdict=unreadable error={e}");
                None
            }
        })
        .unwrap_or_default()
}

fn contact_names(conn: &rusqlite::Connection) -> Vec<String> {
    conn.prepare("SELECT name FROM crm_contacts WHERE trim(name) != '' ORDER BY name LIMIT ?1")
        .and_then(|mut st| {
            st.query_map([CONTACTS_MAX as i64], |r| r.get::<_, String>(0))
                .map(|it| it.flatten().collect::<Vec<_>>())
        })
        .unwrap_or_default()
}

fn dict_count(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM dictation_dict", [], |r| r.get(0)).unwrap_or(0)
}

/// GET /api/dictation/settings — the settings, and how much each source holds, so the page
/// can say "12 workers, 40 words, 0 contacts" beside each switch.
async fn get_settings(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    let read = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let conn = store.read()?;
        let s = read_settings(&conn);
        Ok(json!({
            "settings": s,
            "sources": {
                "workers": session_env_stems().len(),
                "dictionary": dict_count(&conn),
                "contacts": contact_names(&conn).len(),
            },
            "measured": true,
        }))
    })
    .await;
    match read {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

/// PUT /api/dictation/settings — the whole settings object.
async fn put_settings(State(state): State<AppState>, body: axum::body::Bytes) -> Response {
    let mut s: DictSettings = match serde_json::from_slice(&body) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": format!("invalid settings: {e}") })),
    };
    s.mic = s.mic.trim().to_string();
    s.context = s.context.trim().to_string();
    if s.mic.chars().count() > MIC_MAX {
        return err(StatusCode::BAD_REQUEST, json!({ "error": format!("microphone name is over {MIC_MAX} characters") }));
    }
    if s.context.chars().count() > CONTEXT_MAX {
        return err(StatusCode::BAD_REQUEST, json!({ "error": format!("context is over {CONTEXT_MAX} characters") }));
    }
    let value = match serde_json::to_string(&s) {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    let stored = value.clone();
    let res = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO prefs (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
                rusqlite::params![SETTINGS_KEY, stored],
            )?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![PendingEvent {
                    entity_type: EntityType::Other("pref".into()),
                    entity_id: SETTINGS_KEY.into(),
                    mutation: MutationKind::Updated,
                    payload: None,
                }],
            })
        })
        .await;
    match res {
        Ok(_) => {
            tracing::info!(
                "dictation_settings verdict=saved mic_set={} context_chars={} workers={} contacts={}",
                !s.mic.is_empty(), s.context.chars().count(), s.workers, s.contacts
            );
            Json(json!({ "ok": true, "settings": s })).into_response()
        }
        Err(e) => internal(e),
    }
}

/// py:27237 `_dictation_vocab` — personal vocabulary as prompt context, plus the context
/// sources in `DictSettings` (the speaker's note, contact names). Recordings use it too.
pub(crate) fn dictation_vocab(conn: &rusqlite::Connection) -> String {
    let mut terms: Vec<String> = Vec::new();
    let mut fixes: Vec<String> = Vec::new();
    let rows = conn
        .prepare("SELECT word, correct FROM dictation_dict ORDER BY created DESC LIMIT 300")
        .and_then(|mut s| {
            s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map(|it| it.flatten().collect::<Vec<_>>())
        });
    let Ok(rows) = rows else { return String::new() };
    for (word, correct) in rows {
        if correct.trim().is_empty() {
            terms.push(word);
        } else {
            fixes.push(format!("\"{word}\" → \"{correct}\""));
        }
    }
    let mut out: Vec<String> = Vec::new();
    if !terms.is_empty() {
        out.push(format!(
            "Spell these terms EXACTLY as written: {}",
            terms.join(", ")
        ));
    }
    if !fixes.is_empty() {
        out.push(format!(
            "Always apply these corrections: {}",
            fixes.join("; ")
        ));
    }
    let settings = read_settings(conn);
    if settings.contacts {
        let people = contact_names(conn);
        if !people.is_empty() {
            out.push(format!(
                "People the speaker knows; spell their names EXACTLY as written: {}",
                people.join(", ")
            ));
        }
    }
    if !settings.context.is_empty() {
        out.push(format!(
            "About the speaker and what they talk about (use it to resolve unclear words, never copy it into the output):\n{}",
            settings.context
        ));
    }
    out.join("\n")
}

/// Session names from `~/.amux/sessions/*.env` stems. Python globs in
/// filesystem order; sorted here so prompt content and tie-breaks are
/// deterministic (the Python order was arbitrary, not meaningful).
fn session_env_stems() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(amux_home().join("sessions")) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("env") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
    }
    names.sort();
    names
}

/// py:27259 `_dictation_prompt` — transcribe + clean, with amux-specific
/// context (session names are the #1 thing speech-to-text mangles) and the
/// user's vocabulary.
fn dictation_prompt(conn: &rusqlite::Connection, session: &str) -> String {
    let mut parts: Vec<String> = vec![
        "You transcribe voice dictation for amux, a terminal session manager.".into(),
        "Transcribe the audio, then clean it up: remove filler words (um, uh, like),".into(),
        "fix punctuation and capitalization, and correct obvious speech-to-text errors.".into(),
        "Do NOT answer, summarize, or add commentary — output ONLY the cleaned transcription."
            .into(),
        "Preserve the speaker's wording and intent; do not paraphrase.".into(),
    ];
    let names: Vec<String> = if read_settings(conn).workers {
        session_env_stems().into_iter().take(80).collect()
    } else {
        Vec::new()
    };
    if !names.is_empty() {
        parts.push(format!(
            "These are the user's session names — if a spoken phrase sounds like one \
             (letters spelled out, hyphens omitted, words run together, or a near-homophone), \
             replace it with the EXACT name from this list:\n{}",
            names.join(", ")
        ));
    }
    if !session.is_empty() {
        parts.push(format!("The user is dictating into session \"{session}\"."));
    }
    let vocab = dictation_vocab(conn);
    if !vocab.is_empty() {
        parts.push(vocab);
    }
    parts.join("\n")
}

/// Python `base64.b64decode` default mode discards non-alphabet characters
/// instead of rejecting them; mirror that leniency.
fn b64_decode_lenient(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    let filtered: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
        .collect();
    base64::engine::general_purpose::STANDARD.decode(filtered.as_bytes())
}

// ---- Gemini (fallback engine + AI-edit engine) ----------------------------

fn gemini_base() -> String {
    #[cfg(test)]
    if let Some(v) = tests::GEMINI_BASE_OVERRIDE
        .lock()
        .expect("gemini base override")
        .clone()
    {
        return v;
    }
    // AMUX_GEMINI_BASE points every Gemini call at another host: the e2e
    // suite's stub, so no test reaches the real, billed API.
    std::env::var("AMUX_GEMINI_BASE")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://generativelanguage.googleapis.com".into())
}

/// py:27604 `_gemini_generate` — POST generateContent, returns
/// (text, error). Error strings match Python's exactly so the SPA's error
/// surfaces read the same from either origin.
pub(crate) async fn gemini_generate(key: &str, parts: Value, timeout_s: u64) -> (String, String) {
    let url = format!(
        "{}/v1beta/models/{}:generateContent?key={}",
        gemini_base(),
        dictation_model(),
        key
    );
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_s))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                String::new(),
                format!("gemini error: {}", truncate_chars(&e.to_string(), 200)),
            )
        }
    };
    let body = json!({ "contents": [{ "parts": parts }] }).to_string();
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            return (
                String::new(),
                format!("gemini error: {}", truncate_chars(&e.to_string(), 200)),
            )
        }
    };
    let status = resp.status();
    let bytes = resp.bytes().await.unwrap_or_default();
    if !status.is_success() {
        let detail = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|d| {
                d.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .map(|s| truncate_chars(s, 200))
            })
            .unwrap_or_default();
        let detail = if detail.is_empty() {
            "request failed".to_string()
        } else {
            detail
        };
        return (
            String::new(),
            format!("gemini {}: {}", status.as_u16(), detail),
        );
    }
    let d: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return (
                String::new(),
                format!("gemini error: {}", truncate_chars(&e.to_string(), 200)),
            )
        }
    };
    let cands = d.get("candidates").and_then(Value::as_array);
    let Some(first) = cands.and_then(|c| c.first()) else {
        return (
            String::new(),
            "no transcription returned (audio may be silent)".into(),
        );
    };
    let text = first
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .map(|segs| {
            segs.iter()
                .map(|s| s.get("text").and_then(Value::as_str).unwrap_or(""))
                .collect::<String>()
        })
        .unwrap_or_default();
    (text.trim().to_string(), String::new())
}

// ---- Local transcription (Whisper) ----------------------------------------
// Presence-detected, never build-flagged (py:27289): if an interpreter with
// the module and the weights are on this box, dictation runs locally;
// otherwise it falls through to Gemini. Same binary, both deployments.

/// The preferred local engine (owner, 2026-11-18: "the cheapest, fastest, most
/// accurate model local"): Whisper large-v3-turbo on MLX, Apple's GPU framework.
/// Measured on the owner's own 30 saved dictations (487 s of audio, M3 Ultra):
/// 0.89 s per clip against 1.51 s for openai-whisper `base`, which ran under
/// an x86_64 Python (Rosetta, CPU only); 3.8% WER against full large-v3 against
/// 10.3% for `base`; the owner's terms spelled right 16 times and wrong twice
/// against 1 and 9; 2.3 GB resident. Parakeet TDT 0.6B was faster (0.3 s) but
/// weaker on names and content. The worker takes one JSON line per clip,
/// `{"path", "prompt"}`; the prompt is the dictionary as plain terms. A clip
/// whose peak is under -60 dB is no speech, answered empty without the model:
/// on the owner's three silent clips Whisper says "Thank you.".
const MLX_WORKER_PY: &str = r#"
import sys, json, os
try:
    import numpy as np
    import mlx_whisper
    from mlx_whisper.audio import load_audio
    model = os.environ["AMUX_MLX_MODEL_DIR"]
    mlx_whisper.transcribe(np.zeros(16000, dtype=np.float32), path_or_hf_repo=model, language="en")
except Exception as e:
    print(json.dumps({"fatal": str(e)[:200]}), flush=True); sys.exit(1)
print(json.dumps({"ready": True}), flush=True)
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        req = json.loads(line) if line.startswith("{") else {"path": line}
        audio = load_audio(req["path"])
        if float(np.abs(audio).max(initial=0.0)) < 0.001:
            print(json.dumps({"text": "", "silent": True}), flush=True); continue
        kw = {"path_or_hf_repo": model, "language": "en", "condition_on_previous_text": False}
        if req.get("prompt"): kw["initial_prompt"] = req["prompt"]
        r = mlx_whisper.transcribe(audio, **kw)
        print(json.dumps({"text": (r.get("text") or "").strip()}), flush=True)
    except Exception as e:
        print(json.dumps({"error": str(e)[:200]}), flush=True)
"#;

pub(crate) const MLX_DEFAULT_MODEL: &str = "mlx-community/whisper-large-v3-turbo";

/// `AMUX_MLX_MODEL` (a Hugging Face repo id), or the default; `off` disables.
fn mlx_model_name() -> String {
    std::env::var("AMUX_MLX_MODEL")
        .unwrap_or_else(|_| MLX_DEFAULT_MODEL.into())
        .trim()
        .to_string()
}

/// The model's local snapshot directory under `<home>/.amux/models/hf`, or
/// None. Loaded from disk only: a missing model must never start a download
/// on a request (the same rule as the openai-whisper weights, AMUX-4627).
fn mlx_model_dir_in(home: &std::path::Path, repo: &str) -> Option<PathBuf> {
    let snaps = home
        .join(".amux/models/hf/hub")
        .join(format!("models--{}", repo.replace('/', "--")))
        .join("snapshots");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(snaps).ok()?.flatten().map(|e| e.path()).collect();
    dirs.sort();
    dirs.into_iter().rev().find(|d| {
        d.join("config.json").exists()
            && (d.join("weights.safetensors").exists() || d.join("weights.npz").exists())
    })
}

fn mlx_model_dir() -> Option<PathBuf> {
    mlx_model_dir_in(&PathBuf::from(std::env::var("HOME").unwrap_or_default()), &mlx_model_name())
}

static MLX_PY: OnceLock<Option<String>> = OnceLock::new();

/// An arm64 Python that can import mlx_whisper: `AMUX_MLX_PYTHON`, then the
/// venv amux keeps at `~/.amux/models/asr-venv`.
fn mlx_python_blocking() -> Option<String> {
    MLX_PY
        .get_or_init(|| {
            let mut cands: Vec<PathBuf> = Vec::new();
            if let Ok(v) = std::env::var("AMUX_MLX_PYTHON") {
                if !v.trim().is_empty() {
                    cands.push(PathBuf::from(v.trim()));
                }
            }
            cands.push(PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".amux/models/asr-venv/bin/python"));
            cands.into_iter().filter(|c| c.exists()).find_map(|c| {
                let ok = run_with_timeout(
                    &c.to_string_lossy(),
                    &["-c", "import importlib.util as u,sys;sys.exit(0 if u.find_spec('mlx_whisper') else 1)"],
                    Duration::from_secs(20),
                )
                .map(|s| s.success())
                .unwrap_or(false);
                ok.then(|| c.to_string_lossy().into_owned())
            })
        })
        .clone()
}

/// The engine name local dictation runs as: "mlx-whisper" or "whisper".
pub(crate) async fn local_engine_name() -> &'static str {
    if mlx_available().await { "mlx-whisper" } else { "whisper" }
}

/// Whether the MLX engine is installed here: not turned off, model on disk, an
/// interpreter with mlx_whisper. Takes no lock: `whisper_start` calls it with
/// the WHISPER lock held (tokio's Mutex is not reentrant).
async fn mlx_installed() -> bool {
    #[cfg(test)]
    if tests::WHISPER_OVERRIDE.lock().expect("whisper override").is_some() {
        return false;
    }
    let name = mlx_model_name();
    if name.is_empty() || ["off", "none", "0"].contains(&name.to_lowercase().as_str()) {
        return false;
    }
    if mlx_model_dir().is_none() {
        return false;
    }
    tokio::task::spawn_blocking(mlx_python_blocking).await.ok().flatten().is_some()
}

/// Installed, and its worker has not failed to start in this process.
/// Takes the WHISPER lock: never call it while holding that lock.
async fn mlx_available() -> bool {
    // A local, so the guard drops here: as one `&&` expression the temporary
    // guard lived across the install probe's await (land review F3).
    let failed = WHISPER.lock().await.mlx_failed;
    !failed && mlx_installed().await
}

/// The dictionary as plain terms for Whisper's prompt: correct spellings, then
/// contact names when that source is on, then the speaker's note. Whisper
/// copies the spelling of prompt words; an instruction sentence it would echo
/// (measured: a "Vocabulary:" prefix turned silence into invented sentences).
pub(crate) fn local_vocab_prompt(conn: &rusqlite::Connection) -> String {
    let mut terms: Vec<String> = conn
        .prepare("SELECT word, correct FROM dictation_dict ORDER BY created DESC LIMIT 100")
        .and_then(|mut s| {
            s.query_map([], |r| {
                let (w, c): (String, String) = (r.get(0)?, r.get(1)?);
                Ok(if c.trim().is_empty() { w } else { c })
            })
            .map(|it| it.flatten().collect())
        })
        .unwrap_or_default();
    let settings = read_settings(conn);
    if settings.contacts {
        terms.extend(contact_names(conn).into_iter().take(40));
    }
    terms.retain(|t| !t.trim().is_empty());
    terms.dedup();
    let mut out = if terms.is_empty() { String::new() } else { format!("{}.", terms.join(", ")) };
    if !settings.context.is_empty() {
        out = format!("{out} {}", settings.context).trim().to_string();
    }
    // Whisper reads at most about 224 prompt tokens; keep the head.
    truncate_chars(&out, 700)
}

/// py:27302 `_WHISPER_WORKER` — the inline worker script. It matched Python's
/// byte for byte until AMUX-4627 added `download_root`, which makes the model
/// load from the directory `whisper_weights_path` found instead of
/// openai-whisper's default `~/.cache/whisper`.
const WHISPER_WORKER_PY: &str = r#"
import sys, json, os
try:
    import torch; torch.set_num_threads(max(2, min(6, (os.cpu_count() or 4) - 2)))
    import whisper
    m = whisper.load_model(os.environ["AMUX_WHISPER_MODEL"], device="cpu", download_root=os.environ.get("AMUX_WHISPER_DIR") or None)
except Exception as e:
    print(json.dumps({"fatal": str(e)[:200]}), flush=True); sys.exit(1)
print(json.dumps({"ready": True}), flush=True)
for line in sys.stdin:
    path = line.strip()
    if not path: continue
    try:
        r = m.transcribe(path, fp16=False, language="en")
        print(json.dumps({"text": (r.get("text") or "").strip()}), flush=True)
    except Exception as e:
        print(json.dumps({"error": str(e)[:200]}), flush=True)
"#;

fn whisper_model_name() -> String {
    std::env::var("AMUX_WHISPER_MODEL")
        .unwrap_or_else(|_| "base".into())
        .trim()
        .to_string()
}

/// py:27324 `_whisper_weights_path` — local weights file, or None. Checked
/// BEFORE anything loads a model: a missing model makes openai-whisper
/// reach out to download, which hung ~300s on the Python host — exactly
/// wrong for a feature whose point is working with no uplink.
///
/// AMUX-4627: `~/.amux/models/whisper` is checked first. `~/.cache` is a cache,
/// and cleanups treat it as one: the Disk tab's tool-cache roots include it as
/// "Generic tool cache", a past Disk scan listed `~/.cache/whisper` itself as a
/// finding (frustrations-archive.md), and the weights vanished from there on
/// 2026-08-17 and again before 2026-09-14, silently sending every clip to
/// Gemini. The cache stays a fallback so a host that only has the old copy
/// keeps working.
fn whisper_weights_path(name: &str) -> Option<PathBuf> {
    whisper_weights_in(
        &PathBuf::from(std::env::var("HOME").unwrap_or_default()),
        name,
    )
}

fn whisper_weights_in(home: &std::path::Path, name: &str) -> Option<PathBuf> {
    [".amux/models/whisper", ".cache/whisper"]
        .iter()
        .map(|dir| home.join(dir).join(format!("{name}.pt")))
        .find(|p| p.exists())
}

/// The directory the worker should load from, or "" to let openai-whisper use
/// its default. Passed as `AMUX_WHISPER_DIR`.
fn whisper_weights_dir() -> String {
    whisper_weights_path(&whisper_model_name())
        .and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

/// Run `cmd` with args, killed after `timeout` — std::process has no
/// native timeout and the import probe must never wedge startup.
fn run_with_timeout(
    cmd: &str,
    args: &[&str],
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let mut child = std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let t0 = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if t0.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

static WHISPER_PY: OnceLock<Option<String>> = OnceLock::new();

/// py:27334 `_whisper_python` — an interpreter that can `import whisper`,
/// or None. The server's own process certainly isn't it (this one is
/// Rust); find the interpreter that already has it. Candidates are
/// ABSOLUTE: launchd starts this server with no shell PATH, so bare names
/// resolved against PATH alone would report the engine absent on the exact
/// machine it lives on (same class of bug as the restic full-path fix).
fn whisper_python_blocking() -> Option<String> {
    WHISPER_PY
        .get_or_init(|| {
            let mut cands: Vec<PathBuf> = Vec::new();
            if let Ok(v) = std::env::var("AMUX_WHISPER_PYTHON") {
                let v = v.trim();
                if !v.is_empty() {
                    cands.push(PathBuf::from(v));
                }
            }
            let names = ["python3.11", "python3.12", "python3.13", "python3"];
            let mut dirs: Vec<PathBuf> = std::env::var("PATH")
                .unwrap_or_default()
                .split(':')
                .filter(|d| !d.is_empty())
                .map(PathBuf::from)
                .collect();
            for d in [
                "/usr/local/bin",
                "/opt/homebrew/bin",
                "/usr/bin",
                "/usr/local/opt/python@3.11/bin",
                "/usr/local/opt/python@3.12/bin",
                "/usr/local/opt/python@3.13/bin",
                "/opt/homebrew/opt/python@3.11/bin",
                "/opt/homebrew/opt/python@3.12/bin",
                "/opt/homebrew/opt/python@3.13/bin",
            ] {
                dirs.push(PathBuf::from(d));
            }
            for n in names {
                for d in &dirs {
                    let p = d.join(n);
                    if p.exists() && !cands.contains(&p) {
                        cands.push(p);
                    }
                }
            }
            for c in cands {
                if !c.exists() {
                    continue;
                }
                let ok = run_with_timeout(
                    &c.to_string_lossy(),
                    &[
                        "-c",
                        "import importlib.util as u,sys;\
                         sys.exit(0 if u.find_spec('whisper') and u.find_spec('torch') else 1)",
                    ],
                    Duration::from_secs(20),
                )
                .map(|s| s.success())
                .unwrap_or(false);
                if ok {
                    return Some(c.to_string_lossy().into_owned());
                }
            }
            None
        })
        .clone()
}

struct WhisperWorker {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
}

#[derive(Default)]
struct WhisperState {
    worker: Option<WhisperWorker>,
    /// Which engine the live worker runs: "mlx-whisper" or "whisper".
    kind: &'static str,
    /// The MLX worker could not start: use openai-whisper until restart.
    mlx_failed: bool,
    /// When the worker last answered, for the idle unload.
    last_used: Option<std::time::Instant>,
    /// py `_whisper_failed`: a worker that could not START disables the
    /// local path until restart (fall through to Gemini, don't retry-spin).
    failed: bool,
}

static WHISPER: tokio::sync::Mutex<WhisperState> = tokio::sync::Mutex::const_new(WhisperState {
    worker: None,
    kind: "",
    mlx_failed: false,
    last_used: None,
    failed: false,
});

/// py:27362 `_whisper_available` — same checks, same order (weights before
/// interpreter discovery so hosts without models short-circuit cheaply).
async fn whisper_available() -> bool {
    #[cfg(test)]
    if let Some(v) = *tests::WHISPER_OVERRIDE.lock().expect("whisper override") {
        return v;
    }
    if mlx_available().await {
        return true;
    }
    let name = whisper_model_name();
    if name.is_empty() || ["off", "none", "0"].contains(&name.to_lowercase().as_str()) {
        return false;
    }
    if WHISPER.lock().await.failed {
        return false;
    }
    // Two-fixes rule (2026-08-17): whisper silently disabling itself is exactly
    // what made Dictate "not work" — the base weights got cleaned off
    // ~/.cache/whisper by disk pressure, so every clip fell to the gemini
    // fallback, which hallucinates the session-name list on unclear audio.
    // Nothing logged it; the only signal was garbage transcripts. Announce it
    // ONCE per disappearance so a log sweep catches the next one; the flag
    // resets when the weights return, so a re-vanish re-warns.
    use std::sync::atomic::{AtomicBool, Ordering};
    static WEIGHTS_WARNED: AtomicBool = AtomicBool::new(false);
    if whisper_weights_path(&name).is_none() {
        if !WEIGHTS_WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "[dictation] whisper '{name}' weights ABSENT (neither ~/.amux/models/whisper/{name}.pt \
                 nor ~/.cache/whisper/{name}.pt) — falling back to gemini, which hallucinates on \
                 unclear audio. Restore: python3 -c \"import os, whisper; whisper.load_model('{name}', \
                 download_root=os.path.expanduser('~/.amux/models/whisper'))\""
            );
        }
        return false;
    }
    WEIGHTS_WARNED.store(false, Ordering::Relaxed);
    tokio::task::spawn_blocking(whisper_python_blocking)
        .await
        .ok()
        .flatten()
        .is_some()
}

/// Dictate's local engine for the transcription setting: `Ok(model)` when it
/// can run, else why not.
pub(crate) async fn local_status() -> Result<String, String> {
    if mlx_available().await {
        return Ok(format!("{} (MLX)", mlx_model_name()));
    }
    if whisper_available().await {
        return Ok(whisper_model_name());
    }
    let name = whisper_model_name();
    Err(if name.is_empty() || ["off", "none", "0"].contains(&name.to_lowercase().as_str()) {
        "local Whisper is turned off by AMUX_WHISPER_MODEL".to_string()
    } else if whisper_weights_path(&name).is_none() {
        format!("no Whisper '{name}' model in ~/.amux/models/whisper")
    } else {
        "no Python with openai-whisper and torch".to_string()
    })
}

/// py:27373 `_whisper_start` — spawn the WARM worker (model resident;
/// loading costs 0.6-2.6s, paying it per request would erase the latency
/// win the local path exists for). Called with the WHISPER lock held.
async fn whisper_start(st: &mut WhisperState) {
    if !st.mlx_failed && mlx_installed().await {
        if let (Some(py), Some(dir)) = (tokio::task::spawn_blocking(mlx_python_blocking).await.ok().flatten(), mlx_model_dir()) {
            let cmd = tokio::process::Command::new(&py);
            if spawn_worker(st, cmd, MLX_WORKER_PY, &[("AMUX_MLX_MODEL_DIR", dir.to_string_lossy().into_owned()), ("HF_HUB_OFFLINE", "1".into())], "mlx-whisper").await {
                tracing::info!(model = %mlx_model_name(), python = %py, verdict = "dictation_engine_warm", engine = "mlx-whisper",
                    "[dictation] MLX Whisper warm");
                return;
            }
            st.mlx_failed = true;
            tracing::warn!(model = %mlx_model_name(), verdict = "dictation_mlx_failed", engine = "mlx-whisper",
                "[dictation] the MLX worker did not start; using openai-whisper until restart");
        }
    }
    let py = tokio::task::spawn_blocking(whisper_python_blocking)
        .await
        .ok()
        .flatten();
    let Some(py) = py else { return };
    let cmd = tokio::process::Command::new(&py);
    if !spawn_worker(st, cmd, WHISPER_WORKER_PY,
        &[("AMUX_WHISPER_MODEL", whisper_model_name()), ("AMUX_WHISPER_DIR", whisper_weights_dir())], "whisper").await {
        st.failed = true;
        return;
    }
    tracing::info!(
        "[dictation] whisper '{}' warm via {py}, weights from {}",
        whisper_model_name(),
        whisper_weights_dir()
    );
}

/// Spawn a worker script and wait for its ready line. On success the worker
/// is in `st` with `kind`, and an idle watcher is armed.
async fn spawn_worker(
    st: &mut WhisperState,
    mut cmd: tokio::process::Command,
    script: &str,
    envs: &[(&str, String)],
    kind: &'static str,
) -> bool {
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let spawned = cmd
        .args(["-u", "-c", script])
        .env("PYTHONUNBUFFERED", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // KEEP STDERR (AMUX-4675). It was Stdio::null(), so a worker that
        // failed for a real reason — a missing model, a bad weights dir, an
        // import error — could not explain itself, and the caller's only
        // evidence was an empty string. That is what made "local transcribe
        // failed ()" possible: an escalation naming no cause, because the cause
        // had been sent to /dev/null at spawn time. Piped and drained below.
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(engine = kind, "[dictation] local worker spawn failed: {e}");
            return false;
        }
    };
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return false;
    };
    // Drain stderr into the log on its own task. It must be DRAINED whatever
    // happens: a piped stream nobody reads fills its pipe buffer and blocks the
    // worker mid-write, which would turn a diagnostic improvement into a hang.
    if let Some(errs) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(errs).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let l = l.trim();
                if !l.is_empty() {
                    tracing::warn!(
                        verdict = "dictation_whisper_stderr",
                        "[dictation] whisper worker: {}",
                        truncate_chars(l, 300)
                    );
                }
            }
        });
    }
    let mut stdout = tokio::io::BufReader::new(stdout);
    let mut line = String::new();
    // Python blocks on readline forever here; bound it so a wedged model
    // load cannot deadlock the engine mutex for the life of the process.
    let read = tokio::time::timeout(Duration::from_secs(180), stdout.read_line(&mut line)).await;
    let ok = matches!(read, Ok(Ok(n)) if n > 0) && line.contains("\"ready\"");
    if !ok {
        let _ = child.start_kill();
        tracing::warn!(
            engine = kind,
            "[dictation] local worker failed to start: {}",
            truncate_chars(line.trim(), 160)
        );
        return false;
    }
    st.worker = Some(WhisperWorker {
        child,
        stdin,
        stdout,
    });
    st.kind = kind;
    st.last_used = Some(std::time::Instant::now());
    arm_idle_unload();
    true
}

/// `AMUX_DICTATION_IDLE_SECS` (default 900; 0 keeps the model loaded).
fn idle_unload_secs() -> u64 {
    std::env::var("AMUX_DICTATION_IDLE_SECS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(900)
}

/// One watcher per process: a worker idle past `idle_unload_secs` is stopped,
/// so its model (2.3 GB for MLX turbo) leaves memory on a machine that is
/// often under pressure. The next clip starts it again (about 2 s).
fn arm_idle_unload() {
    static ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if idle_unload_secs() == 0 || ARMED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
    rt.spawn(async {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let mut st = WHISPER.lock().await;
            let idle = st.last_used.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            if st.worker.is_some() && idle >= idle_unload_secs() {
                if let Some(mut w) = st.worker.take() {
                    let _ = w.child.start_kill();
                }
                tracing::info!(engine = st.kind, idle_secs = idle, measured = true, n_considered = 1,
                    verdict = "dictation_engine_unloaded", "[dictation] local engine unloaded after idle");
            }
        }
    });
}

/// py:27400 `_whisper_transcribe` — (text, err) from the warm local worker.
/// What the local engine's `(text, error)` pair actually MEANS (AMUX-4675).
///
/// Three outcomes, not two. The handler used to key on `!text.is_empty()`
/// alone, which folded "heard no speech" into "failed" and escalated silence to
/// the paid engine. Measured on the live server: a 1s 440Hz tone came back as
/// 68 words of fleet lane names, and a 0.13s clip came back as "I need to open
/// up a new session for ai-for-smbs". Neither is in the audio, and this text is
/// what the UI hands the user to SEND.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LocalResult {
    /// Speech, transcribed.
    Text(String),
    /// The engine ran and there was nothing to hear. A RESULT: the honest
    /// answer is an empty transcription, and no paid call is owed.
    NoSpeech,
    /// The engine could not answer, and says why. The only case that escalates.
    Failed(String),
}

/// Read the local engine's answer. Pure, so the rule can be tested without a
/// model, a worker process, or a network call — the defect was entirely in this
/// reading, and a test that needed Whisper installed is a test nobody runs.
pub(crate) fn read_local_result(text: &str, err: &str) -> LocalResult {
    if !text.trim().is_empty() {
        return LocalResult::Text(text.to_string());
    }
    if err.trim().is_empty() {
        return LocalResult::NoSpeech;
    }
    LocalResult::Failed(err.trim().to_string())
}

async fn whisper_transcribe(raw: &[u8], mime: &str, prompt: &str) -> (String, String, &'static str) {
    let ext = match mime.to_lowercase().as_str() {
        "audio/ogg" => ".ogg",
        "audio/mp4" => ".mp4",
        "audio/aac" => ".aac",
        "audio/wav" | "audio/x-wav" => ".wav",
        "audio/mpeg" => ".mp3",
        _ => ".webm", // audio/webm and anything unknown, as in Python
    };
    let tmp = tokio::task::spawn_blocking({
        let raw = raw.to_vec();
        let ext = ext.to_string();
        move || -> std::io::Result<tempfile::NamedTempFile> {
            let mut f = tempfile::Builder::new().suffix(&ext).tempfile()?;
            std::io::Write::write_all(&mut f, &raw)?;
            Ok(f)
        }
    })
    .await;
    let tmp = match tmp {
        Ok(Ok(f)) => f,
        Ok(Err(e)) => return (String::new(), truncate_chars(&e.to_string(), 200), "whisper"),
        Err(e) => return (String::new(), truncate_chars(&e.to_string(), 200), "whisper"),
    };
    let path = tmp.path().to_string_lossy().into_owned();

    let mut st = WHISPER.lock().await;
    let dead = st
        .worker
        .as_mut()
        .map(|w| w.child.try_wait().map(|s| s.is_some()).unwrap_or(true))
        .unwrap_or(true);
    if dead {
        st.worker = None;
        whisper_start(&mut st).await;
    }
    if st.worker.is_none() {
        return (String::new(), "local transcription unavailable".into(), "whisper");
    }
    let kind = st.kind;
    let req = if kind == "mlx-whisper" {
        json!({ "path": path, "prompt": prompt }).to_string()
    } else {
        path.clone()
    };
    let t0 = std::time::Instant::now();
    let wrote = {
        let w = st.worker.as_mut().expect("worker present");
        w.stdin
            .write_all(format!("{req}\n").as_bytes())
            .await
            .is_ok()
            && w.stdin.flush().await.is_ok()
    };
    if !wrote {
        st.worker = None;
        return (String::new(), "local worker died".into(), kind);
    }
    let mut line = String::new();
    // Python's readline here has NO timeout — a wedged worker hangs the
    // request and the mutex forever. 600s is far past any legitimate
    // 25MB-clip transcription on CPU; past it the worker is killed and the
    // next request restarts it.
    let read = {
        let w = st.worker.as_mut().expect("worker present");
        tokio::time::timeout(Duration::from_secs(600), w.stdout.read_line(&mut line)).await
    };
    drop(tmp); // worker has read the file by the time it answers
    match read {
        Ok(Ok(n)) if n > 0 => {
            let d: Value = serde_json::from_str(&line).unwrap_or_else(|_| json!({}));
            let text = d
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let err = d
                .get("error")
                .and_then(Value::as_str)
                .or_else(|| d.get("fatal").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            st.last_used = Some(std::time::Instant::now());
            let silent = d.get("silent").and_then(Value::as_bool).unwrap_or(false);
            tracing::info!(engine = kind, secs = t0.elapsed().as_secs_f64(), words = text.split_whitespace().count(),
                silent, error = %err,
                measured = true, n_considered = 1, verdict = "dictation_engine_ran",
                "[dictation] local engine answered");
            (text, err, kind)
        }
        Ok(_) => {
            st.worker = None;
            (String::new(), "local worker died".into(), kind)
        }
        Err(_) => {
            if let Some(mut w) = st.worker.take() {
                let _ = w.child.start_kill();
            }
            (String::new(), "local worker timed out".into(), kind)
        }
    }
}

// ---- Session-name recovery for locally-transcribed text -------------------
// py:27512-27602. Speech-to-text mangles session names constantly and they
// are the one token you actually paste. A deterministic pass fixes them
// with no network, which is what makes the offline path worth having.

/// py `_DN_STOP`.
const DN_STOP: &[&str] = &[
    "a", "an", "and", "are", "as", "ask", "at", "be", "but", "by", "can", "do", "for", "from",
    "get", "go", "had", "has", "have", "he", "her", "him", "his", "how", "i", "if", "in", "is",
    "it", "its", "me", "my", "no", "not", "of", "on", "or", "our", "out", "ping", "put", "say",
    "see", "she", "so", "tell", "than", "that", "the", "then", "there", "they", "this", "to",
    "try", "up", "us", "was", "we", "what", "when", "where", "which", "who", "why", "will", "with",
    "you", "your",
];

/// py `_DN_SUB` — order matters; the replacements are sequential.
const DN_SUB: &[(&str, &str)] = &[
    ("ph", "f"),
    ("ck", "k"),
    ("qu", "k"),
    ("x", "ks"),
    ("z", "s"),
    ("v", "f"),
    ("b", "p"),
    ("d", "t"),
    ("g", "k"),
    ("ee", "i"),
    ("ea", "i"),
    ("ai", "a"),
    ("y", "i"),
    ("c", "k"),
];

/// py `_dn_norm` — lowercase, keep [a-z0-9] only.
fn dn_norm(x: &str) -> String {
    x.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect()
}

/// py `_dn_phon` — normalize, apply the sub table, collapse runs, strip
/// vowels.
fn dn_phon(x: &str) -> String {
    let mut x = dn_norm(x);
    for (a, b) in DN_SUB {
        x = x.replace(a, b);
    }
    let mut collapsed = String::with_capacity(x.len());
    let mut prev: Option<char> = None;
    for c in x.chars() {
        if Some(c) != prev {
            collapsed.push(c);
        }
        prev = Some(c);
    }
    collapsed
        .chars()
        .filter(|c| !matches!(c, 'a' | 'e' | 'i' | 'o' | 'u'))
        .collect()
}

/// difflib `SequenceMatcher` total matching characters — the exact CPython
/// algorithm (queue over find_longest_match). No junk/autojunk handling:
/// inputs here are normalized name-length strings, far below the 200-char
/// popularity threshold, so CPython's junk machinery is inert for them.
fn sm_matches(a: &[u8], b: &[u8]) -> usize {
    let mut b2j: HashMap<u8, Vec<usize>> = HashMap::new();
    for (j, &c) in b.iter().enumerate() {
        b2j.entry(c).or_default().push(j);
    }
    let mut queue: Vec<(usize, usize, usize, usize)> = vec![(0, a.len(), 0, b.len())];
    let mut matched = 0usize;
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        // find_longest_match(alo, ahi, blo, bhi): first-longest wins, as in
        // CPython (strict `>` over ascending i, then ascending j).
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for (i, &ca) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut new_j2len: HashMap<usize, usize> = HashMap::new();
            if let Some(js) = b2j.get(&ca) {
                for &j in js {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break; // js is ascending
                    }
                    let k = if j > 0 {
                        j2len.get(&(j - 1)).copied().unwrap_or(0) + 1
                    } else {
                        1
                    };
                    new_j2len.insert(j, k);
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            j2len = new_j2len;
        }
        if bestsize > 0 {
            matched += bestsize;
            if alo < besti && blo < bestj {
                queue.push((alo, besti, blo, bestj));
            }
            if besti + bestsize < ahi && bestj + bestsize < bhi {
                queue.push((besti + bestsize, ahi, bestj + bestsize, bhi));
            }
        }
    }
    matched
}

/// difflib `SequenceMatcher(None, a, b).ratio()` for the ASCII strings
/// dn_norm/dn_phon emit.
fn sm_ratio(a: &str, b: &str) -> f64 {
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let total = ab.len() + bb.len();
    if total == 0 {
        return 1.0;
    }
    2.0 * sm_matches(ab, bb) as f64 / total as f64
}

/// py `_dn_score` — literal ratio with a phonetic alternative. The phonetic
/// score is ALWAYS considered, not only when the literal one is weak
/// (scoring one span with the boost and another without made them
/// incomparable — see the Python comment).
fn dn_score(cn: &str, cp: &str, nn: &str, np: &str) -> f64 {
    let r = sm_ratio(cn, nn);
    let rp = sm_ratio(cp, np);
    if rp >= 0.9 {
        r.max(rp * 0.97)
    } else {
        r
    }
}

/// py `_dn_targets` filtering applied to a name list: dedupe by lowercase,
/// keep names whose normalized form is >= 4 chars.
fn targets_from_names<I: IntoIterator<Item = String>>(names: I) -> Vec<(String, String, String)> {
    let mut seen: std::collections::HashSet<String> = Default::default();
    let mut out: Vec<(String, String, String)> = Vec::new();
    for n in names {
        if n.is_empty() || seen.contains(&n.to_lowercase()) || dn_norm(&n).len() < 4 {
            continue;
        }
        seen.insert(n.to_lowercase());
        let (nn, np) = (dn_norm(&n), dn_phon(&n));
        out.push((n, nn, np));
    }
    out
}

/// The dictation_dict half of `_dn_targets`: `(r["correct"] or r["word"] or
/// "").strip()` per row — the user's CORRECTED spelling is the recovery
/// target, falling back to the term itself.
fn dict_target_names(conn: &rusqlite::Connection) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(mut stmt) = conn.prepare("SELECT word, correct FROM dictation_dict LIMIT 300") {
        if let Ok(rows) =
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        {
            for (word, correct) in rows.flatten() {
                let w = if correct.trim().is_empty() {
                    word
                } else {
                    correct
                };
                let w = w.trim().to_string();
                if !w.is_empty() {
                    names.push(w);
                }
            }
        }
    }
    names
}

/// py `_dn_targets` — session env stems + dictation_dict words.
fn dn_targets(conn: &rusqlite::Connection) -> Vec<(String, String, String)> {
    let settings = read_settings(conn);
    let mut names = if settings.workers { session_env_stems() } else { Vec::new() };
    names.extend(dict_target_names(conn));
    if settings.contacts {
        names.extend(contact_names(conn));
    }
    targets_from_names(names)
}

/// `core = re.sub(r"^[^\w]+|[^\w.,!?]+$", "", chunk); trail = chunk[len(core):]`
/// — including the quirk that a stripped LEADING run shifts `trail` into
/// the middle of the chunk (Python slices by the core's LENGTH, not its
/// position). Kept bug-for-bug: the replacement output depends on it.
fn strip_core(chunk: &str) -> (String, String) {
    let chars: Vec<char> = chunk.chars().collect();
    let is_w = |c: char| c.is_alphanumeric() || c == '_';
    let mut start = 0usize;
    while start < chars.len() && !is_w(chars[start]) {
        start += 1;
    }
    let keep_trailing = |c: char| is_w(c) || matches!(c, '.' | ',' | '!' | '?');
    let mut end = chars.len();
    while end > start && !keep_trailing(chars[end - 1]) {
        end -= 1;
    }
    let core: String = chars[start..end].iter().collect();
    let core_len = end - start;
    let trail: String = chars[core_len.min(chars.len())..].iter().collect();
    (core, trail)
}

/// py:27558 `_dictation_fix_names` — map spoken session names back onto
/// their exact spelling. Pure function over an explicit target list so
/// tests can drive it hermetically; `dn_targets` supplies the live list.
/// Seconds the meta-task model may take to format a dictation before the
/// rule-based formatting is used (`AMUX_DICTATION_FORMAT_TIMEOUT_S`, 0 = off).
fn format_timeout_s() -> u64 {
    std::env::var("AMUX_DICTATION_FORMAT_TIMEOUT_S").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(8)
}

/// Dictations shorter than this go as they are: nothing to lay out, and the
/// model's latency would be all cost.
const FORMAT_MIN_WORDS: usize = 15;

fn format_prompt(text: &str) -> String {
    format!(
        "You format dictated text. Return the SAME words, laid out: a spoken enumeration (\"number one is X, number two is Y\", \
         \"first ... second ...\", \"one is ... two is ...\") becomes a numbered list with one item per line and the spoken markers \
         dropped; a remark after the list is its own paragraph; separate paragraphs with a blank line. Fix only punctuation and \
         capitalisation. Do not add, answer, summarise or rephrase anything, and do not wrap it in quotes or code fences. If it \
         needs no layout, return it unchanged. Return only the text.\n\nText:\n{text}"
    )
}

/// Words of `s`, lowercased, without punctuation or list numbers.
fn bare_words(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|w| !w.is_empty() && !w.chars().all(|c| c.is_ascii_digit()))
        .collect()
}

/// Did the model lay the words out, or write something else? Kept only when
/// nearly every word it returned was spoken and it dropped at most the
/// markers it was asked to drop.
pub(crate) fn formatting_kept_words(said: &str, got: &str) -> bool {
    // The spoken scaffolding a list drops: "so number one is", "and second,".
    const MARKERS: [&str; 33] = ["so", "and", "then", "okay", "ok", "alright", "now", "number", "is", "that", "one", "two", "three",
        "four", "five", "six", "seven", "eight", "nine", "ten", "first", "second", "third", "fourth", "fifth", "firstly", "secondly",
        "thirdly", "lastly", "finally", "next", "also", "the"];
    let (a, b) = (bare_words(said), bare_words(got));
    if b.is_empty() || b.len() > a.len() + 3 {
        return false;
    }
    let mut left: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
    for w in &a {
        *left.entry(w.as_str()).or_default() += 1;
    }
    let mut foreign = 0usize;
    for w in &b {
        match left.get_mut(w.as_str()) {
            Some(n) if *n > 0 => *n -= 1,
            _ => foreign += 1,
        }
    }
    // Words spoken and not returned: only markers may go, plus a slip or two.
    let dropped: usize = left.iter().filter(|(w, _)| !MARKERS.contains(w)).map(|(_, n)| (*n).max(0) as usize).sum();
    foreign * 20 <= b.len() && dropped * 20 <= a.len()
}

/// The dictation formatted by the meta-task model (`lookup::helper_answer`,
/// the `helper_model` setting or AMUX_HELPER_MODEL), or None to keep the
/// rule-based formatting: too short, turned off, too slow, failed, or an answer
/// that changed the words.
async fn format_with_helper(text: &str) -> Option<String> {
    let secs = format_timeout_s();
    if secs == 0 || text.split_whitespace().count() < FORMAT_MIN_WORDS {
        return None;
    }
    let started = std::time::Instant::now();
    let got = tokio::time::timeout(std::time::Duration::from_secs(secs), super::lookup::helper_answer(&format_prompt(text))).await;
    let ms = started.elapsed().as_millis() as u64;
    let (model, answer) = match got {
        Ok(Ok(v)) => v,
        Ok(Err((_, why))) => {
            tracing::warn!(ms, error = %truncate_chars(&why, 200), measured = true, n_considered = 1, verdict = "dictation_format_helper_failed",
                "[dictation] the meta-task model did not format this dictation; the rule-based formatting is used");
            return None;
        }
        Err(_) => {
            tracing::warn!(ms, limit_s = secs, measured = true, n_considered = 1, verdict = "dictation_format_helper_slow",
                "[dictation] the meta-task model took longer than AMUX_DICTATION_FORMAT_TIMEOUT_S; the rule-based formatting is used");
            return None;
        }
    };
    let answer = answer.trim().trim_matches('`').trim().to_string();
    if !formatting_kept_words(text, &answer) {
        tracing::warn!(ms, model = %model, measured = true, n_considered = 1, verdict = "dictation_format_rejected",
            "[dictation] the meta-task model's answer changed the words; the rule-based formatting is used");
        return None;
    }
    tracing::info!(ms, model = %model, changed = answer != text, measured = true, n_considered = 1, verdict = "dictation_formatted_by_helper",
        "[dictation] the meta-task model formatted this dictation");
    Some(answer)
}

/// A spoken enumeration as a numbered list: "Two things. One is X. Two is
/// that Y." becomes "Two things.\n1. X.\n2. Y.". A list needs at least two
/// markers in order from one (one/first, two/second, ...), each opening a
/// sentence, so "one of the" or a lone "first" in prose is left alone. Text
/// before the first marker stays as the intro line; "is", "is that" and a
/// comma or colon after a marker are dropped. Text with no such run is
/// returned unchanged, so a transcript already formatted passes through.
///
/// Spoken lists also run inside one sentence (owner, 2026-11-25: "So number
/// one is example one, number two is example two, and number three is example
/// three." came out unformatted). A filler ("so", "and", "okay") may open the
/// sentence before a marker, and a "number N" marker may follow a comma (", "
/// or ", and "): "number" is what makes that a list rather than prose like
/// "two cats, one is black". An item drops the comma and "and" that joined it
/// to the next, and a closing remark after the last item ("Having said this,
/// ...") becomes its own paragraph.
pub(crate) fn format_spoken_list(text: &str) -> String {
    const WORDS: [(&str, usize); 30] = [
        ("one", 1), ("two", 2), ("three", 3), ("four", 4), ("five", 5), ("six", 6), ("seven", 7), ("eight", 8), ("nine", 9), ("ten", 10),
        ("first", 1), ("second", 2), ("third", 3), ("fourth", 4), ("fifth", 5), ("sixth", 6), ("seventh", 7), ("eighth", 8), ("ninth", 9), ("tenth", 10),
        ("firstly", 1), ("secondly", 2), ("thirdly", 3), ("fourthly", 4), ("fifthly", 5), ("sixthly", 6), ("seventhly", 7), ("eighthly", 8), ("ninthly", 9), ("tenthly", 10),
    ];
    static MARK: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = MARK.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(?:^|(?P<stop>[.!?])\s+|(?P<comma>,)\s+(?:and\s+)?)(?:(?:so|and|then|okay|ok|alright|now),?\s+)?(?P<num>number\s+)?(?P<w>[a-z]+)(?:\s+is\s+that|\s+is|\s*[,:])?\s+",
        )
        .expect("list marker regex")
    });
    // (number, where the sentence starts, where the item text starts)
    let mut marks: Vec<(usize, usize, usize)> = Vec::new();
    // Overlapping candidates: a comma marker can sit inside a match that
    // started earlier, so scan from every boundary rather than once.
    let mut caps = Vec::new();
    let mut at = 0usize;
    while let Some(c) = re.captures_at(text, at) {
        let Some(all) = c.get(0) else { break };
        at = (all.start() + 1).max(c.name("w").map(|w| w.start()).unwrap_or(all.end()));
        caps.push(c);
        if at >= text.len() {
            break;
        }
    }
    for c in caps {
        let (Some(all), Some(w)) = (c.get(0), c.name("w")) else { continue };
        let Some(&(_, n)) = WORDS.iter().find(|(k, _)| k.eq_ignore_ascii_case(w.as_str())) else { continue };
        // Mid-sentence, only "number N is" opens an item.
        if c.name("comma").is_some() && (c.name("num").is_none() || !text[w.end()..all.end()].to_lowercase().contains("is")) {
            continue;
        }
        // Bare "Two things" opens no item: a marker needs "is", a comma or a
        // colon after it, or to be one of the -ly / "number" forms.
        let tail = &text[w.end()..all.end()];
        let bare = !tail.contains([',', ':']) && !tail.to_lowercase().contains("is");
        if bare && !w.as_str().to_lowercase().ends_with("ly") && !all.as_str().to_lowercase().contains("number") {
            continue;
        }
        // The previous text ends at the sentence's stop (kept) or before the comma.
        let start = if all.start() == 0 { 0 } else if c.name("stop").is_some() { all.start() + 1 } else { all.start() };
        if marks.last().is_some_and(|m: &(usize, usize, usize)| m.1 == start) {
            continue;
        }
        marks.push((n, start, all.end()));
    }
    // The run 1, 2, 3 ... in order, from the first "one".
    let mut run: Vec<(usize, usize, usize)> = Vec::new();
    for m in marks {
        if m.0 == run.len() + 1 {
            run.push(m);
        }
    }
    if run.len() < 2 {
        return text.to_string();
    }
    let mut out = text[..run[0].1].trim().to_string();
    let mut closing = String::new();
    for (i, &(n, _, body)) in run.iter().enumerate() {
        let end = run.get(i + 1).map(|r| r.1).unwrap_or(text.len());
        let mut item = text[body..end].trim();
        if i + 1 == run.len() {
            let (last, rest) = split_closing(item);
            item = last;
            closing = rest.to_string();
        }
        let item = item.trim_end_matches(|c: char| c == ',' || c.is_whitespace());
        let item = item.strip_suffix(" and").or_else(|| item.strip_suffix(", and")).unwrap_or(item).trim_end_matches(',');
        let mut cs = item.chars();
        let item = cs.next().map(|f| f.to_uppercase().collect::<String>() + cs.as_str()).unwrap_or_default();
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("{n}. {item}"));
    }
    if !closing.is_empty() {
        out.push_str("\n\n");
        out.push_str(&closing);
    }
    out
}

/// The last item and a closing remark after it: the first sentence after the
/// item that opens like a wrap-up. Anything else stays in the item, since an
/// item can run to several sentences.
fn split_closing(item: &str) -> (&str, &str) {
    const CLOSERS: [&str; 10] = ["having said", "that said", "with that", "anyway", "in summary", "to summarize", "overall",
        "those are", "that's the list", "this is the list"];
    let mut from = 0usize;
    while let Some(off) = item[from..].find(['.', '!', '?']) {
        let cut = from + off + 1;
        let rest = item[cut..].trim_start();
        if rest.is_empty() {
            break;
        }
        let low = rest.to_lowercase();
        if CLOSERS.iter().any(|k| low.starts_with(k)) {
            return (item[..cut].trim(), rest);
        }
        from = cut;
    }
    (item, "")
}

pub(crate) fn fix_names_with_targets(
    text: &str,
    targets: &[(String, String, String)],
    thresh: f64,
) -> String {
    if text.is_empty() || targets.is_empty() {
        return text.to_string();
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < words.len() {
        // (score, name, span, trail)
        let mut best: Option<(f64, &str, usize, String)> = None;
        for span in [4usize, 3, 2, 1] {
            if i + span > words.len() {
                continue;
            }
            let chunk = words[i..i + span].join(" ");
            let (core, trail) = strip_core(&chunk);
            let cn = dn_norm(&core);
            let cp = dn_phon(&core);
            if cn.len() < 4 {
                continue;
            }
            // One ordinary word inside the span means we are about to
            // delete it. Dropping a real word is worse than leaving a name
            // unresolved, because it is invisible in the result.
            let lower = core.to_lowercase();
            let stop_hit = lower
                .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
                .filter(|t| !t.is_empty())
                .any(|t| DN_STOP.contains(&t));
            if stop_hit {
                continue;
            }
            for (name, nn, np) in targets {
                if ((cn.len() as i64 - nn.len() as i64).abs() as f64)
                    > 4.0f64.max(nn.len() as f64 * 0.35)
                {
                    continue;
                }
                let r = dn_score(&cn, &cp, nn, np);
                if r < thresh {
                    continue;
                }
                if span > 1 {
                    // The leading word must EARN its place: if dropping it
                    // matches this name as well or better, it was never
                    // part of the name.
                    let inner = words[i + 1..i + span].join(" ");
                    if dn_score(&dn_norm(&inner), &dn_phon(&inner), nn, np) >= r {
                        continue;
                    }
                }
                let better = match &best {
                    None => true,
                    Some((br, _, bs, _)) => r > br + 1e-9 || ((r - br).abs() < 1e-9 && span > *bs),
                };
                if better {
                    best = Some((r, name, span, trail.clone()));
                }
            }
        }
        if let Some((_, name, span, trail)) = best {
            out.push(format!("{name}{trail}"));
            i += span;
        } else {
            out.push(words[i].to_string());
            i += 1;
        }
    }
    out.join(" ")
}

// ---- GET/POST /api/dictation/config ---------------------------------------

/// Python `json.dumps` formatting (`", "` / `": "` separators) for a FLAT
/// ordered object — the /config GET body is byte-compatible with the
/// Python origin so nothing diffing the two servers sees a phantom change.
fn py_dumps(fields: &[(&str, Value)]) -> String {
    let mut s = String::from("{");
    for (i, (k, v)) in fields.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&serde_json::to_string(k).unwrap_or_default());
        s.push_str(": ");
        s.push_str(&serde_json::to_string(v).unwrap_or_default());
    }
    s.push('}');
    s
}

fn py_json_response(status: StatusCode, fields: &[(&str, Value)]) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        py_dumps(fields),
    )
        .into_response()
}

/// py:72240-72259 — GET reports the engine /api/dictate actually uses;
/// POST stores/clears the BYO key (WRITE-ONLY: the key itself is never
/// returned to any client). Other methods fall to Python's trailing
/// dictation 404.
pub async fn config(State(state): State<AppState>, req: Request) -> Response {
    match *req.method() {
        axum::http::Method::GET => {
            let store = state.store.clone();
            let keyed =
                tokio::task::spawn_blocking(move || -> anyhow::Result<(String, &'static str)> {
                    let conn = store.read()?;
                    Ok(dictation_key(&conn))
                })
                .await;
            let (key, src) = match keyed {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => return internal(e),
                Err(e) => return internal(e),
            };
            let local = whisper_available().await;
            let mlx = local && mlx_available().await;
            py_json_response(
                StatusCode::OK,
                &[
                    ("configured", json!(!key.is_empty())),
                    ("source", json!(if key.is_empty() { "none" } else { src })),
                    ("model", json!(dictation_model())),
                    ("local", json!(local)),
                    (
                        "local_model",
                        json!(if mlx {
                            mlx_model_name()
                        } else if local {
                            whisper_model_name()
                        } else {
                            String::new()
                        }),
                    ),
                    ("engine", json!(if mlx { "mlx-whisper" } else if local { "whisper" } else { "gemini" })),
                ],
            )
        }
        axum::http::Method::POST => {
            let bytes = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
                Ok(b) => b,
                Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e.to_string() })),
            };
            let body: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
            let k = body
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let k_w = k.clone();
            let write = state
                .store
                .write_async(move |conn| {
                    if k_w.is_empty() {
                        conn.execute("DELETE FROM prefs WHERE key='dictation_gemini_key'", [])?;
                    } else {
                        conn.execute(
                            "INSERT INTO prefs (key, value) VALUES ('dictation_gemini_key', ?1) \
                             ON CONFLICT(key) DO UPDATE SET value=?1",
                            rusqlite::params![k_w],
                        )?;
                    }
                    Ok(WriteOutcome {
                        applied: true,
                        events: vec![PendingEvent {
                            entity_type: EntityType::Other("pref".into()),
                            entity_id: "dictation_gemini_key".into(),
                            mutation: MutationKind::Updated,
                            payload: None,
                        }],
                    })
                })
                .await;
            if let Err(e) = write {
                return internal(e);
            }
            let store = state.store.clone();
            let keyed =
                tokio::task::spawn_blocking(move || -> anyhow::Result<(String, &'static str)> {
                    let conn = store.read()?;
                    Ok(dictation_key(&conn))
                })
                .await;
            match keyed {
                Ok(Ok((k2, src2))) => py_json_response(
                    StatusCode::OK,
                    &[
                        ("ok", json!(true)),
                        ("configured", json!(!k2.is_empty())),
                        ("source", json!(if k2.is_empty() { "none" } else { src2 })),
                    ],
                ),
                Ok(Err(e)) => internal(e),
                Err(e) => internal(e),
            }
        }
        _ => route_not_found(),
    }
}

// ---------------------------------------------------------------------------
// Tests — temp-DB stores, Python-shaped rows; no network, no live files.
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::Store;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Arc as StdArc;
    use tower::ServiceExt;

    /// Serializes tests that set the engine-knob overrides below.
    ///
    /// Lived in `py_proxy::tests` as `PY_LOCK` until AMUX-2906 deleted that
    /// module's forwarder. It never had anything to do with the python proxy —
    /// dictation was borrowing a lock from an unrelated module, so a reader of
    /// either file had to visit the other to see why.
    pub(crate) static ENGINE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// One captured request, as the fake upstream saw it.
    #[derive(Clone, Debug)]
    pub(crate) struct Seen {
        pub method: String,
        pub path_and_query: String,
        #[allow(dead_code)] // asserted by some engine tests, not all
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    /// Spin a plain-HTTP fake upstream answering `status`/`body` to every
    /// request and recording what arrived. Returns (base_url, log).
    ///
    /// Formerly `py_proxy::tests::fake_python` — a misleading name, since the
    /// only thing it has ever faked here is the **Gemini API** that the
    /// dictation fallback calls (pointed at via `GEMINI_BASE_OVERRIDE`).
    pub(crate) async fn fake_upstream(
        status: StatusCode,
        body: &'static str,
    ) -> (String, StdArc<Mutex<Vec<Seen>>>) {
        let log: StdArc<Mutex<Vec<Seen>>> = StdArc::new(Mutex::new(Vec::new()));
        let log_c = log.clone();
        let app = Router::new().fallback(move |req: axum::extract::Request| {
            let log = log_c.clone();
            async move {
                let method = req.method().to_string();
                let path_and_query = req
                    .uri()
                    .path_and_query()
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default();
                let headers = req
                    .headers()
                    .iter()
                    .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect();
                let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default();
                log.lock().expect("seen log").push(Seen {
                    method,
                    path_and_query,
                    headers,
                    body: bytes.to_vec(),
                });
                (status, [("content-type", "application/json")], body)
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://{addr}"), log)
    }

    /// Engine-knob overrides: process env / filesystem probes are not
    /// hermetic under parallel tests. Tests that set these hold ENGINE_LOCK.
    pub(crate) static GEMINI_KEY_OVERRIDE: Mutex<Option<String>> = Mutex::new(None);
    pub(crate) static GEMINI_BASE_OVERRIDE: Mutex<Option<String>> = Mutex::new(None);
    pub(crate) static WHISPER_OVERRIDE: Mutex<Option<bool>> = Mutex::new(None);

    fn set_overrides(whisper: Option<bool>, key: Option<&str>, base: Option<String>) {
        *WHISPER_OVERRIDE.lock().unwrap() = whisper;
        *GEMINI_KEY_OVERRIDE.lock().unwrap() = key.map(String::from);
        *GEMINI_BASE_OVERRIDE.lock().unwrap() = base;
    }

    /// AMUX-4627: the lookup prefers ~/.amux/models/whisper, still finds the
    /// old cache copy, and never counts another model's file.
    #[test]
    fn a_dictated_clip_is_kept_by_history_id_and_goes_with_its_row() {
        let path = save_audio(990_001, "audio/mp4", b"clip").expect("the clip is saved");
        assert!(path.ends_with("990001.m4a"), "{path}");
        assert_eq!(std::fs::read(&path).unwrap(), b"clip");
        assert!(save_audio(0, "audio/webm", b"x").is_none(), "no row, no clip");
        remove_audio(990_001);
        assert!(!std::path::Path::new(&path).exists());
    }

    #[test]
    fn a_spoken_list_comes_out_as_a_numbered_list() {
        let said = "Two things. One is we want the formatting like Wispr Flow does. So that's important. \
                    Two is that every input should be supported. Three is when I press the button, it should close.";
        assert_eq!(
            super::format_spoken_list(said),
            "Two things.\n1. We want the formatting like Wispr Flow does. So that's important.\n\
             2. Every input should be supported.\n3. When I press the button, it should close."
        );
        assert_eq!(super::format_spoken_list("First, buy milk. Second, call mom."), "1. Buy milk.\n2. Call mom.");
        assert_eq!(super::format_spoken_list("Firstly we ship. Secondly we test."), "1. We ship.\n2. We test.");
        // Owner, 2026-11-25: one sentence, commas and "and", a filler first,
        // and a closing remark after the list.
        assert_eq!(
            super::format_spoken_list("I'm doing a dictation test that has a list in formatting. Here's the list. So number one is example one, \
                number two is example two, and number three is example three. Having said this, this is the list of elements that I want to test in dictation."),
            "I'm doing a dictation test that has a list in formatting. Here's the list.\n1. Example one\n2. Example two\n3. Example three.\n\n\
             Having said this, this is the list of elements that I want to test in dictation."
        );
        assert_eq!(super::format_spoken_list("So one is milk. And two is bread."), "1. Milk.\n2. Bread.");
        for prose in [
            "One of the workers is slow.",
            "First, check the log. Then restart it.",
            "Two things matter here. Three people agreed.",
            "I have two cats, one is black, two is a lie.",
            "Number one priority, number two pencil.",
            "1. Already a list.\n2. Second item.",
            "",
        ] {
            assert_eq!(super::format_spoken_list(prose), prose, "left alone: {prose}");
        }
    }

    /// The meta-task model's layout is kept only when it kept the words.
    #[test]
    fn a_model_answer_is_kept_only_when_it_lays_out_the_same_words() {
        let said = "Here's the list. So number one is example one, number two is example two, and number three is example three. Having said this, that is all.";
        let laid = "Here's the list.\n1. Example one\n2. Example two\n3. Example three\n\nHaving said this, that is all.";
        assert!(super::formatting_kept_words(said, laid));
        assert!(!super::formatting_kept_words(said, "Sure! Here is your formatted list of three examples, ready to paste anywhere you like today."));
        assert!(!super::formatting_kept_words(said, "1. Example one"), "most of it dropped");
        assert!(!super::formatting_kept_words(said, ""));
    }

    #[tokio::test]
    async fn the_mlx_install_check_takes_no_lock() {
        // whisper_start runs with the WHISPER lock held and asks this; a check
        // that took the lock again deadlocked the first clip (land review F1).
        let _held = super::WHISPER.lock().await;
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), super::mlx_installed()).await;
        assert!(r.is_ok(), "mlx_installed waited on the WHISPER lock");
    }

    #[test]
    fn the_mlx_model_is_found_only_as_a_complete_local_snapshot() {
        let home = tempfile::tempdir().unwrap();
        let repo = "mlx-community/whisper-large-v3-turbo";
        assert_eq!(super::mlx_model_dir_in(home.path(), repo), None, "nothing on disk");
        let snap = home.path().join(".amux/models/hf/hub/models--mlx-community--whisper-large-v3-turbo/snapshots/abc");
        std::fs::create_dir_all(&snap).unwrap();
        std::fs::write(snap.join("config.json"), b"{}").unwrap();
        assert_eq!(super::mlx_model_dir_in(home.path(), repo), None, "a config without weights is a partial download");
        std::fs::write(snap.join("weights.safetensors"), b"w").unwrap();
        assert_eq!(super::mlx_model_dir_in(home.path(), repo), Some(snap));
        assert_eq!(super::mlx_model_dir_in(home.path(), "mlx-community/other"), None);
    }

    #[tokio::test]
    async fn the_local_prompt_is_the_dictionary_as_plain_terms() {
        let (app, dir) = app();
        let conn = rusqlite::Connection::open(dir.path().join("dictation-api-test.db")).unwrap();
        assert_eq!(super::local_vocab_prompt(&conn), "", "an empty dictionary prompts nothing");
        conn.execute("INSERT INTO dictation_dict (word, correct, created) VALUES ('TubeScience', '', 1), ('mixed peak', 'Mixpeek', 2)", []).unwrap();
        let body = json!({"mic": "", "context": "I run amux.", "workers": true, "contacts": false});
        send(&app, "PUT", "/api/dictation/settings", Some(body)).await;
        let p = super::local_vocab_prompt(&conn);
        assert!(p.contains("Mixpeek") && p.contains("TubeScience"), "{p}");
        assert!(!p.contains("mixed peak"), "a correction prompts its right spelling only: {p}");
        assert!(p.ends_with("I run amux."), "{p}");
        assert!(!p.to_lowercase().contains("vocabulary") && !p.contains("EXACTLY"),
            "no instruction words for Whisper to echo: {p}");
    }

    #[test]
    fn whisper_weights_prefer_amux_models_and_fall_back_to_the_cache() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(super::whisper_weights_in(home.path(), "base"), None);
        let cache = home.path().join(".cache/whisper");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("base.pt"), b"w").unwrap();
        assert_eq!(
            super::whisper_weights_in(home.path(), "base"),
            Some(cache.join("base.pt"))
        );
        let models = home.path().join(".amux/models/whisper");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(models.join("base.pt"), b"w").unwrap();
        assert_eq!(
            super::whisper_weights_in(home.path(), "base"),
            Some(models.join("base.pt"))
        );
        assert_eq!(super::whisper_weights_in(home.path(), "small"), None);
    }

    fn app() -> (axum::Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("dictation-api-test.db")).unwrap();
        let state = AppState {
            store: Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let router = Router::new()
            .nest("/api/dictation", routes())
            .route("/api/dictate", axum::routing::post(dictate))
            .with_state(state);
        (router, dir)
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let b = Request::builder().method(method).uri(path);
        let req = match body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, v)
    }

    /// Rows exactly as the Python server INSERTs them.
    fn seed_python_rows(db: &std::path::Path) {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.execute(
            "INSERT INTO dictation_history (session, ts, text, raw_text, words, dur_ms) \
             VALUES ('orch', 1754700000000, 'send the board update', 'send the board update', 4, 2100)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO dictation_history (session, ts, text, raw_text, prev_text, ai_edited, words, dur_ms) \
             VALUES ('', 1754700001000, 'Cleaner text.', 'cleaner text raw', 'cleaner text before', 1, 2, 900)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO dictation_dict (word, correct, created) VALUES ('a mux', 'amux', 1754000000)",
            [],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn python_shaped_history_round_trip() {
        let (app, dir) = app();
        seed_python_rows(&dir.path().join("dictation-api-test.db"));

        let (st, v) = send(&app, "GET", "/api/dictation/history", None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let items = v["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        // ts DESC: the ai-edited row first; every Python column present.
        assert_eq!(items[0]["ts"], json!(1754700001000i64));
        assert_eq!(items[0]["text"], json!("Cleaner text."));
        assert_eq!(items[0]["raw_text"], json!("cleaner text raw"));
        assert_eq!(items[0]["prev_text"], json!("cleaner text before"));
        assert_eq!(items[0]["ai_edited"], json!(1));
        assert_eq!(items[0]["words"], json!(2));
        assert_eq!(items[0]["dur_ms"], json!(900));
        assert_eq!(items[1]["session"], json!("orch"));
        assert_eq!(v["total_words"], json!(6));

        // session filter: items narrow, total stays whole-table (parity).
        let (_, f) = send(&app, "GET", "/api/dictation/history?session=orch", None).await;
        assert_eq!(f["items"].as_array().unwrap().len(), 1);
        assert_eq!(f["total_words"], json!(6));

        // count mode: one integer, no transcript payload.
        let (_, c) = send(&app, "GET", "/api/dictation/history?count=1", None).await;
        assert_eq!(c, json!({ "count": 2 }));
        let (_, c2) = send(
            &app,
            "GET",
            "/api/dictation/history?count=1&session=orch",
            None,
        )
        .await;
        assert_eq!(c2, json!({ "count": 1 }));

        // limit applies.
        let (_, l) = send(&app, "GET", "/api/dictation/history?limit=1", None).await;
        assert_eq!(l["items"].as_array().unwrap().len(), 1);

        // DELETE — ok, row gone.
        let id = items[1]["id"].as_i64().unwrap();
        let (st, r) = send(
            &app,
            "DELETE",
            &format!("/api/dictation/history/{id}"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r["ok"], json!(true));
        let (_, after) = send(&app, "GET", "/api/dictation/history?count=1", None).await;
        assert_eq!(after["count"], json!(1));
    }

    #[tokio::test]
    async fn undo_reverts_to_prev_text_then_raw_text() {
        let (app, dir) = app();
        seed_python_rows(&dir.path().join("dictation-api-test.db"));
        let (_, v) = send(&app, "GET", "/api/dictation/history", None).await;
        let edited_id = v["items"][0]["id"].as_i64().unwrap();

        let (st, r) = send(
            &app,
            "POST",
            &format!("/api/dictation/history/{edited_id}/edit"),
            Some(json!({ "undo": true })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(
            r,
            json!({ "ok": true, "text": "cleaner text before", "ai_edited": 0 })
        );
        let (_, v2) = send(&app, "GET", "/api/dictation/history", None).await;
        assert_eq!(v2["items"][0]["text"], json!("cleaner text before"));
        assert_eq!(v2["items"][0]["ai_edited"], json!(0));
        assert_eq!(v2["items"][0]["prev_text"], json!(""));

        // Second undo: prev_text now '' -> falls back to raw_text (Python's
        // `or` chain).
        let (_, r2) = send(
            &app,
            "POST",
            &format!("/api/dictation/history/{edited_id}/edit"),
            Some(json!({ "undo": 1 })),
        )
        .await;
        assert_eq!(r2["text"], json!("cleaner text raw"));

        // Missing row -> Python's 404.
        let (st, e) = send(
            &app,
            "POST",
            "/api/dictation/history/999999/edit",
            Some(json!({ "undo": true })),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(e["error"], json!("not found"));
    }

    #[tokio::test]
    async fn ai_edit_calls_gemini_natively_and_round_trips_undo() {
        let _guard = ENGINE_LOCK.lock().await;
        // A Gemini-shaped answer; gemini_generate must join + trim parts.
        let (base, log) = fake_upstream(
            StatusCode::OK,
            r#"{"candidates": [{"content": {"parts": [{"text": " Edited."}]}}]}"#,
        )
        .await;
        set_overrides(Some(false), Some("test-key"), Some(base));
        let (app, dir) = app();
        seed_python_rows(&dir.path().join("dictation-api-test.db"));
        let (_, v) = send(&app, "GET", "/api/dictation/history", None).await;
        let id = v["items"][0]["id"].as_i64().unwrap();

        let (st, e) = send(
            &app,
            "POST",
            &format!("/api/dictation/history/{id}/edit"),
            Some(json!({ "instruction": "tighten" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{e}");
        assert_eq!(e, json!({ "ok": true, "text": "Edited.", "ai_edited": 1 }));

        // The Gemini call carries the model route, the key, and Python's
        // exact edit prompt wrapping the instruction and the OLD text.
        let seen = log
            .lock()
            .unwrap()
            .first()
            .cloned()
            .expect("edit reached fake gemini");
        assert_eq!(seen.method, "POST");
        assert!(
            seen.path_and_query
                .contains(":generateContent?key=test-key"),
            "{}",
            seen.path_and_query
        );
        let sent: Value = serde_json::from_slice(&seen.body).unwrap();
        let prompt = sent["contents"][0]["parts"][0]["text"].as_str().unwrap();
        assert!(prompt.starts_with(
            "Edit this dictated text per the instruction. Output ONLY the edited text, no commentary."
        ), "{prompt}");
        assert!(prompt.contains("Instruction: tighten"), "{prompt}");
        assert!(prompt.contains("Text: Cleaner text."), "{prompt}");

        // prev_text now holds the pre-edit text; undo restores it.
        let (_, v2) = send(&app, "GET", "/api/dictation/history", None).await;
        assert_eq!(v2["items"][0]["text"], json!("Edited."));
        assert_eq!(v2["items"][0]["prev_text"], json!("Cleaner text."));
        assert_eq!(v2["items"][0]["ai_edited"], json!(1));
        let (st, r) = send(
            &app,
            "POST",
            &format!("/api/dictation/history/{id}/edit"),
            Some(json!({ "undo": true })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["text"], json!("Cleaner text."));

        // Missing row -> Python-shape 404, no Gemini call.
        let (st, e) = send(
            &app,
            "POST",
            "/api/dictation/history/999999/edit",
            Some(json!({ "instruction": "tighten" })),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(e["error"], json!("not found"));
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "404 + undo made no Gemini calls"
        );
        set_overrides(None, None, None);
    }

    #[tokio::test]
    async fn ai_edit_without_any_key_is_pythons_503() {
        let _guard = ENGINE_LOCK.lock().await;
        set_overrides(Some(false), Some(""), None); // no BYO row, no env key
        let (app, dir) = app();
        seed_python_rows(&dir.path().join("dictation-api-test.db"));
        let (_, v) = send(&app, "GET", "/api/dictation/history", None).await;
        let id = v["items"][0]["id"].as_i64().unwrap();
        let (st, e) = send(
            &app,
            "POST",
            &format!("/api/dictation/history/{id}/edit"),
            Some(json!({ "instruction": "tighten" })),
        )
        .await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(e["error"], json!("no Gemini key configured"));
        set_overrides(None, None, None);
    }

    #[tokio::test]
    async fn dict_crud_matches_python_including_unique_conflict() {
        let (app, dir) = app();
        seed_python_rows(&dir.path().join("dictation-api-test.db"));

        // Python-shaped row reads back column-exact.
        let (st, list) = send(&app, "GET", "/api/dictation/dict", None).await;
        assert_eq!(st, StatusCode::OK);
        let arr = list.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["word"], json!("a mux"));
        assert_eq!(arr[0]["correct"], json!("amux"));
        assert_eq!(arr[0]["created"], json!(1754000000));

        // Create -> 201 {ok, id}.
        let (st, r) = send(
            &app,
            "POST",
            "/api/dictation/dict",
            Some(json!({ "word": "  cloud f l a r e ", "correct": "Cloudflare" })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{r}");
        assert_eq!(r["ok"], json!(true));
        let new_id = r["id"].as_i64().unwrap();

        // Duplicate (word, correct) -> Python's IntegrityError answer, 200.
        let (st, dup) = send(
            &app,
            "POST",
            "/api/dictation/dict",
            Some(json!({ "word": "cloud f l a r e", "correct": "Cloudflare" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(dup, json!({ "ok": true, "already": true }));

        // word required.
        let (st, e) = send(
            &app,
            "POST",
            "/api/dictation/dict",
            Some(json!({ "correct": "x" })),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(e["error"], json!("word required"));

        // PATCH overwrites both columns (missing field blanks — parity).
        let (st, r) = send(
            &app,
            "PATCH",
            &format!("/api/dictation/dict/{new_id}"),
            Some(json!({ "word": "cf" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r["ok"], json!(true));
        let (_, list2) = send(&app, "GET", "/api/dictation/dict", None).await;
        let patched = list2
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"].as_i64() == Some(new_id))
            .unwrap()
            .clone();
        assert_eq!(patched["word"], json!("cf"));
        assert_eq!(patched["correct"], json!(""));

        // DELETE.
        let (st, r) = send(
            &app,
            "DELETE",
            &format!("/api/dictation/dict/{new_id}"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r["ok"], json!(true));
        let (_, list3) = send(&app, "GET", "/api/dictation/dict", None).await;
        assert_eq!(list3.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn config_native_byte_compatible_and_key_write_only() {
        let _guard = ENGINE_LOCK.lock().await;
        set_overrides(Some(false), Some(""), None);
        let (app, _dir) = app();

        // GET: byte-compatible with Python's `json.dumps` output (verified
        // against the live 8822 answer, which reads
        // `{"configured": true, "source": "server", ...}`).
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/dictation/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers().get("x-amux-answered-by").is_none(),
            "must answer natively"
        );
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let expected = format!(
            "{{\"configured\": false, \"source\": \"none\", \"model\": {m}, \
             \"local\": false, \"local_model\": \"\", \"engine\": \"gemini\"}}",
            m = serde_json::to_string(&dictation_model()).unwrap()
        );
        assert_eq!(String::from_utf8_lossy(&bytes), expected);

        // POST a BYO key: stored write-only (never echoed), source flips.
        let (st, r) = send(
            &app,
            "POST",
            "/api/dictation/config",
            Some(json!({ "key": "  byo-secret-1  " })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(
            r,
            json!({ "ok": true, "configured": true, "source": "byo" })
        );
        let (_, c) = send(&app, "GET", "/api/dictation/config", None).await;
        assert_eq!(c["configured"], json!(true));
        assert_eq!(c["source"], json!("byo"));
        assert!(
            !c.to_string().contains("byo-secret-1"),
            "key must never be returned"
        );

        // Clearing the key falls back to the (absent) env key.
        let (_, r) = send(
            &app,
            "POST",
            "/api/dictation/config",
            Some(json!({ "key": "" })),
        )
        .await;
        assert_eq!(
            r,
            json!({ "ok": true, "configured": false, "source": "none" })
        );

        // Local engine present -> whisper reported, with the model name.
        set_overrides(Some(true), Some(""), None);
        let (_, c) = send(&app, "GET", "/api/dictation/config", None).await;
        assert_eq!(c["local"], json!(true));
        assert_eq!(c["engine"], json!("whisper"));
        assert_eq!(c["local_model"], json!(whisper_model_name()));

        // Non-GET/POST methods fall to Python's trailing dictation 404.
        let (st, e) = send(&app, "DELETE", "/api/dictation/config", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(e["error"], json!("dictation route not found"));
        set_overrides(None, None, None);
    }

    /// AMUX-4675: silence must not become a sentence.
    ///
    /// The live specimens this pins, measured 2026-09-15 before the fix: a 1s
    /// 440Hz tone escalated to the paid engine and came back as 68 words of
    /// fleet lane names; a 0.13s clip came back as "I need to open up a new
    /// session for ai-for-smbs". Neither is in the audio, and this text is what
    /// the dictation UI hands the user to SEND — so the failure mode was a
    /// person sending a worker an instruction they never spoke.
    #[test]
    fn an_empty_local_result_with_no_error_is_silence_not_failure() {
        // THE BUG. No error, no text: the engine ran and heard nothing.
        assert_eq!(read_local_result("", ""), LocalResult::NoSpeech);
        // Whitespace is not speech either; the old check was `!t.is_empty()`,
        // which would have called a single newline a successful transcription.
        assert_eq!(read_local_result("   \n ", ""), LocalResult::NoSpeech);
        assert_eq!(
            read_local_result("", "   "),
            LocalResult::NoSpeech,
            "a blank error is no error; it must not read as a failure to escalate"
        );

        // A REAL failure still escalates, and carries its cause. "local
        // transcribe failed ()" naming nothing is what this replaces.
        assert_eq!(
            read_local_result("", "model weights missing"),
            LocalResult::Failed("model weights missing".into())
        );

        // CONTROL: real speech is unaffected. Without this, a "fix" that
        // returned NoSpeech for everything would pass every assertion above and
        // silently disable dictation.
        assert_eq!(
            read_local_result("Testing 123", ""),
            LocalResult::Text("Testing 123".into()),
            "the local engine is not broken: its worker returns this on a real clip (AMUX-4627)"
        );
        // Text WITH a stderr warning is still text. Whisper chatters on stderr
        // routinely, and treating that as failure would escalate good results.
        assert_eq!(
            read_local_result("Testing 123", "some warning"),
            LocalResult::Text("Testing 123".into())
        );
    }

    #[tokio::test]
    async fn dictate_validates_natively_and_503s_honestly_without_engines() {
        let _guard = ENGINE_LOCK.lock().await;
        set_overrides(Some(false), Some(""), None); // no whisper, no key
        let (app, _dir) = app();

        // Python's exact empty-body answer.
        let (st, e) = send(&app, "POST", "/api/dictate?session=t1", Some(json!({}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(e, json!({ "error": "audio required" }));

        // Audio present but no engine anywhere: the honest 503 naming what
        // to install (ethos rule 3 — never fake a transcription).
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"not really audio");
        let (st, e) = send(&app, "POST", "/api/dictate", Some(json!({ "audio": b64 }))).await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{e}");
        let msg = e["error"].as_str().unwrap();
        // The shared transcription setting's words: where to fix it, and how.
        assert!(msg.contains(crate::api::transcription::SETTING), "{msg}");
        assert!(msg.contains("install a local engine"), "{msg}");
        assert!(msg.contains("GOOGLE_API_KEY"), "{msg}");

        // Python's 413s, both shapes.
        let big = "A".repeat(DICTATION_MAX_BYTES * 4 / 3 + 1);
        let (st, e) = send(&app, "POST", "/api/dictate", Some(json!({ "audio": big }))).await;
        assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(e["error"], json!("audio too large (max ~25MB)"));
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/dictate")
                    .header("content-type", "audio/webm")
                    .header("content-length", (DICTATION_MAX_BYTES + 1).to_string())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);

        // Unknown dictation routes stay NATIVE Python-shape 404s.
        let (st, e) = send(&app, "GET", "/api/dictation/bogus", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(e["error"], json!("dictation route not found"));

        // Non-numeric id: Python's regex miss -> module 404, not an axum 400.
        let (st, e) = send(&app, "DELETE", "/api/dictation/history/abc", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(e["error"], json!("dictation route not found"));
        set_overrides(None, None, None);
    }

    #[tokio::test]
    async fn dictate_takes_text_the_device_heard_without_running_an_engine() {
        let _guard = ENGINE_LOCK.lock().await;
        set_overrides(Some(false), Some(""), None); // no engine: none may be needed
        let (app, _dir) = app();
        let (st, _) = send(&app, "POST", "/api/dictation/dict", Some(json!({ "word": "Cloudflare", "correct": "" }))).await;
        assert_eq!(st, StatusCode::CREATED);

        let audio = base64::engine::general_purpose::STANDARD.encode(b"m4a-bytes");
        let (st, r) = send(
            &app,
            "POST",
            "/api/dictate",
            Some(json!({ "text": "deployed to cloud flare today", "engine": "apple", "audio": audio,
                         "mime": "audio/mp4", "session": "orch", "dur_ms": 2100 })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["engine"], json!("apple"));
        assert_eq!(r["text"], json!("deployed to Cloudflare today"), "the names pass still runs");
        assert!(r["audio"].is_string(), "the audio is kept: {r}");

        let (_, h) = send(&app, "GET", "/api/dictation/history?session=orch", None).await;
        let row = &h["items"][0];
        assert_eq!(row["text"], json!("deployed to Cloudflare today"));
        assert_eq!(row["raw_text"], json!("deployed to cloud flare today"), "what the device heard is kept as heard");

        // Text with no audio is enough; an engine name is cleaned, never trusted.
        let (st, r) = send(&app, "POST", "/api/dictate", Some(json!({ "text": "hello", "engine": "x;drop" }))).await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["engine"], json!("xdrop"));
        assert!(r["audio"].is_null(), "{r}");
        set_overrides(None, None, None);
    }

    #[tokio::test]
    async fn dictate_gemini_fallback_both_upload_shapes() {
        let _guard = ENGINE_LOCK.lock().await;
        let (base, log) = fake_upstream(
            StatusCode::OK,
            r#"{"candidates": [{"content": {"parts": [{"text": " testing one two three "}]}}]}"#,
        )
        .await;
        set_overrides(Some(false), Some("srv-key"), Some(base));
        let (app, _dir) = app();

        // JSON shape: body session beats the query param; dur_ms recorded.
        let audio = base64::engine::general_purpose::STANDARD.encode(b"RIFFfakewav");
        let (st, r) = send(
            &app,
            "POST",
            "/api/dictate?session=queryname",
            Some(json!({ "audio": audio, "mime": "audio/wav", "session": "orch", "dur_ms": 1200 })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["text"], json!("testing one two three"));
        assert_eq!(r["words"], json!(4));
        assert_eq!(r["engine"], json!("gemini"));
        assert!(r["id"].as_i64().unwrap() > 0);
        assert!(r["secs"].is_number());
        let seen = log
            .lock()
            .unwrap()
            .first()
            .cloned()
            .expect("reached fake gemini");
        let sent: Value = serde_json::from_slice(&seen.body).unwrap();
        let prompt = sent["contents"][0]["parts"][0]["text"].as_str().unwrap();
        assert!(
            prompt.ends_with("Transcribe and clean this dictation:"),
            "{prompt}"
        );
        assert_eq!(
            sent["contents"][0]["parts"][1]["inline_data"]["mime_type"],
            json!("audio/wav")
        );
        assert_eq!(
            sent["contents"][0]["parts"][1]["inline_data"]["data"],
            json!(audio)
        );

        // RAW binary shape (preferred by the SPA): query params carry the
        // metadata; the body is the audio itself.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/dictate?session=t2&dur_ms=900&mime=audio/webm")
                    .header("content-type", "audio/webm")
                    .body(Body::from(&b"webm-bytes"[..]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let r2: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(r2["engine"], json!("gemini"));

        // Both rows landed in history with Python's column semantics
        // (raw_text mirrors text at insert time).
        let (_, h) = send(&app, "GET", "/api/dictation/history", None).await;
        let items = h["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        let by_sess = |s: &str| {
            items
                .iter()
                .find(|i| i["session"] == json!(s))
                .cloned()
                .unwrap()
        };
        let orch = by_sess("orch");
        assert_eq!(orch["dur_ms"], json!(1200));
        assert_eq!(orch["raw_text"], json!("testing one two three"));
        let t2 = by_sess("t2");
        assert_eq!(t2["dur_ms"], json!(900));
        set_overrides(None, None, None);
    }

    #[tokio::test]
    async fn dictate_gemini_error_is_a_502_with_pythons_message() {
        let _guard = ENGINE_LOCK.lock().await;
        let (base, _log) = fake_upstream(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"error": {"message": "quota exceeded"}}"#,
        )
        .await;
        set_overrides(Some(false), Some("srv-key"), Some(base));
        let (app, _dir) = app();
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"x");
        let (st, e) = send(&app, "POST", "/api/dictate", Some(json!({ "audio": b64 }))).await;
        assert_eq!(st, StatusCode::BAD_GATEWAY);
        assert_eq!(e["error"], json!("gemini 429: quota exceeded"));
        assert!(e.get("code").is_none(), "a quota error can pass on retry: {e}");
        set_overrides(None, None, None);
    }

    // AF-1361: audio the engine refuses is a 422 the page does not retry.
    #[tokio::test]
    async fn dictate_refused_audio_is_a_422_the_page_does_not_retry() {
        let _guard = ENGINE_LOCK.lock().await;
        let (base, _log) = fake_upstream(
            StatusCode::BAD_REQUEST,
            r#"{"error": {"message": "Unable to process input audio: audio data is empty or corrupted."}}"#,
        )
        .await;
        set_overrides(Some(false), Some("srv-key"), Some(base));
        let (app, _dir) = app();
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"x");
        let (st, e) = send(&app, "POST", "/api/dictate", Some(json!({ "audio": b64 }))).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(e["code"], json!("audio_unreadable"));
        assert!(e["error"].as_str().unwrap().starts_with("gemini 400: Unable to process input audio"));
        set_overrides(None, None, None);
    }

    // ---- engine parity: reference values generated from the Python
    // implementation itself (amux-server.py _dn_* / difflib), 2026-08-09.

    fn ref_targets() -> Vec<(String, String, String)> {
        targets_from_names(
            ["ts-gke", "mvs-infra", "mixpeek", "amux-cloud", "orch"].map(String::from),
        )
    }

    #[test]
    fn sequence_matcher_ratio_matches_difflib() {
        for (a, b, want) in [
            ("mbsinfra", "mvsinfra", 0.875),
            ("tsgke", "tsgke", 1.0),
            ("amuxcloud", "muxcloud", 0.9411764705882353),
            ("abcd", "", 0.0),
            ("", "", 1.0),
            ("mixbeak", "mixpeek", 0.7142857142857143),
            ("kbd", "abcdefg", 0.4),
        ] {
            assert!(
                (sm_ratio(a, b) - want).abs() < 1e-12,
                "ratio({a},{b}) = {} != {want}",
                sm_ratio(a, b)
            );
        }
    }

    #[test]
    fn phonetic_normalization_matches_python() {
        for (input, norm, phon) in [
            ("mixpeek", "mixpeek", "mkspk"),
            ("mvs-infra", "mvsinfra", "mfsnfr"),
            ("ts-gke", "tsgke", "tsk"),
            ("amux-cloud", "amuxcloud", "mksklt"),
            ("Mixbeak", "mixbeak", "mkspk"),
            ("quick brown", "quickbrown", "kkprwn"),
        ] {
            assert_eq!(dn_norm(input), norm);
            assert_eq!(dn_phon(input), phon);
        }
    }

    #[test]
    fn fix_names_matches_python_reference_cases() {
        let targets = ref_targets();
        for (input, want) in [
            // Whisper's classic manglings, from the Python benchmark.
            (
                "send this to T-S-G-K-E please",
                "send this to ts-gke please",
            ),
            ("restart MBS Infra now", "restart mvs-infra now"),
            // "tell" is a stop word: the span guard protects it while the
            // single-word span still fixes the name (phonetic path).
            ("tell Mixbeak to deploy", "tell mixpeek to deploy"),
            // Multi-word span + Python's punctuation quirk (the comma sits
            // inside the replaced core and is dropped) — kept bug-for-bug.
            (
                "check amux cloud, then report",
                "check amux-cloud then report",
            ),
            ("Mix peek is down!", "mixpeek is down!"),
            // Ordinary prose is left alone.
            ("the board is fine", "the board is fine"),
        ] {
            assert_eq!(
                fix_names_with_targets(input, &targets, 0.86),
                want,
                "input: {input}"
            );
        }
        // Live-wire specimen (2026-08-09 e2e): whisper's raw "Testing 123."
        // for a `say` clip; BOTH origins rewrote it to the fleet's
        // `load-testing` session, byte-identically, once the target existed.
        let t = targets_from_names(["load-testing".to_string()]);
        assert_eq!(
            fix_names_with_targets("Testing 123.", &t, 0.86),
            "load-testing 123."
        );
    }

    #[tokio::test]
    async fn dict_rows_feed_name_recovery_exactly_as_python() {
        // `_dn_targets`' DB half: correct beats word, blank correct falls
        // back to the term, whitespace trimmed — then the shared filter
        // (dedupe by lowercase, normalized length >= 4) applies.
        let (_app, dir) = app();
        seed_python_rows(&dir.path().join("dictation-api-test.db")); // ('a mux' -> 'amux')
        let conn = rusqlite::Connection::open(dir.path().join("dictation-api-test.db")).unwrap();
        conn.execute(
            "INSERT INTO dictation_dict (word, correct, created) VALUES ('Cloudflare', '', 2)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO dictation_dict (word, correct, created) VALUES ('cf', '  ', 3)",
            [],
        )
        .unwrap();
        let names = dict_target_names(&conn);
        // Order is the UNIQUE(word, correct) index scan (BINARY collation,
        // uppercase first) — the query has no ORDER BY, on either origin,
        // and sqlite serves it from the covering index the same way for
        // Python's sqlite3. Order only breaks dedupe ties.
        assert_eq!(
            names,
            vec!["Cloudflare".to_string(), "amux".into(), "cf".into()]
        );
        let targets = targets_from_names(names);
        // "cf" is dropped (normalized length < 4); the rest carry norm+phon.
        assert_eq!(
            targets
                .iter()
                .map(|(n, _, _)| n.as_str())
                .collect::<Vec<_>>(),
            vec!["Cloudflare", "amux"]
        );
        // And the dict-fed target drives recovery, exactly as in Python.
        assert_eq!(
            fix_names_with_targets("deployed to cloud flare today", &targets, 0.86),
            "deployed to Cloudflare today"
        );
    }

    #[test]
    fn py_dumps_uses_pythons_separators() {
        assert_eq!(
            py_dumps(&[("a", json!(true)), ("b", json!("x")), ("n", json!(0))]),
            r#"{"a": true, "b": "x", "n": 0}"#
        );
    }

    /// Settings live on the server: saved once, read back by every device, and the
    /// sources they switch on reach the prompt (and off, leave it).
    #[tokio::test]
    async fn settings_are_saved_on_the_server_and_shape_the_prompt() {
        let (app, dir) = app();
        let (st, v) = send(&app, "GET", "/api/dictation/settings", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["settings"]["workers"], json!(true), "worker names are on by default");
        assert_eq!(v["settings"]["contacts"], json!(false));
        assert_eq!(v["measured"], json!(true));

        let db = dir.path().join("dictation-api-test.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO crm_contacts (id, name, created, updated) VALUES ('c1', 'Doron Ashkenazi', 1, 1)",
            [],
        )
        .unwrap();

        let body = json!({"mic": " Shure MV7 ", "context": "I build amux and Mixpeek.", "workers": false, "contacts": true});
        let (st, v) = send(&app, "PUT", "/api/dictation/settings", Some(body)).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["settings"]["mic"], json!("Shure MV7"), "the name is trimmed");

        let (_, v) = send(&app, "GET", "/api/dictation/settings", None).await;
        assert_eq!(v["settings"]["mic"], json!("Shure MV7"));
        assert_eq!(v["sources"]["contacts"], json!(1));

        let vocab = dictation_vocab(&conn);
        assert!(vocab.contains("Doron Ashkenazi"), "{vocab}");
        assert!(vocab.contains("I build amux and Mixpeek."), "{vocab}");
        let prompt = dictation_prompt(&conn, "");
        assert!(!prompt.contains("session names"), "worker names off leaves them out: {prompt}");
        assert!(dn_targets(&conn).iter().any(|t| format!("{t:?}").to_lowercase().contains("doron")), "contacts feed the name pass");

        let off = json!({"mic": "", "context": "", "workers": true, "contacts": false});
        send(&app, "PUT", "/api/dictation/settings", Some(off)).await;
        let vocab = dictation_vocab(&conn);
        assert!(!vocab.contains("Doron") && !vocab.contains("About the speaker"), "{vocab}");
    }

    #[tokio::test]
    async fn settings_refuse_what_they_cannot_keep() {
        let (app, _dir) = app();
        let long = "x".repeat(CONTEXT_MAX + 1);
        let (st, v) = send(&app, "PUT", "/api/dictation/settings", Some(json!({"context": long}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        let (st, _) = send(&app, "PUT", "/api/dictation/settings", Some(json!({"workers": "yes"}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (_, v) = send(&app, "GET", "/api/dictation/settings", None).await;
        assert_eq!(v["settings"]["context"], json!(""), "a refused write changes nothing");
    }
}
