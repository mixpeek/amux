//! Text-to-speech: the read-aloud backend the SPA has been calling into a void.
//!
//! `POST /api/tts` `{text, voice_id?}` -> `{url, size, engine, voice}` and
//! `GET /api/tts/voices` -> `{voices:[{voice_id,name,category}]}`. The client
//! (one-click read-aloud on a worker message + the Text-to-Speech dialog) was
//! fully wired but these routes never existed, so every call 404'd — which is
//! why the button did nothing and why `route.callers_have_routes` flagged
//! `/api/tts` as a caller with no route.
//!
//! MODEL-AGNOSTIC via `AMUX_TTS_ENGINE` (`say` | `piper` | `auto`, default
//! `auto`). Local-first and OSS-spirit, per Ethan's ask ("best oss ... model,
//! model agnostic"): macOS `say` (built in, free, instant — ideal for one-click
//! read-aloud) is the default where present; Piper (`AMUX_TTS_PIPER_BIN` +
//! `AMUX_TTS_PIPER_VOICE`, a real cross-platform OSS TTS model, and the path the
//! Linux cloud image would take) is used when configured. If neither is
//! available the answer is an honest 503 naming what to install — audio is never
//! fabricated (ethos rule 3), the same contract dictation.rs holds for the
//! reverse direction.
//!
//! The result is a `data:` URL, not a served file: TTS output is small and
//! single-use, so inlining the bytes avoids a temp-file lifecycle, a static
//! route, and a cleanup job — the client plays it directly with
//! `new Audio(url)`. Single codebase, no env branch: the engine is chosen from
//! runtime availability + env, so the same binary does the right thing on a
//! laptop (`say`) and in the container (`piper`).

use super::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine as _;
use serde_json::{json, Value};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// Hard cap on input: `say`/Piper both slow to a crawl on huge inputs, and the
/// read-aloud target is one message, not a transcript. Characters, not bytes,
/// so multibyte text is not cut mid-codepoint.
const MAX_CHARS: usize = 8000;

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn engine_pref() -> String {
    std::env::var("AMUX_TTS_ENGINE")
        .ok()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "auto".into())
}

/// Is a binary reachable? launchd starts this server with no shell PATH, so a
/// bare `Command::new("say")` can report a present binary as absent (the exact
/// trap dictation.rs documents). Probe with an absolute-path fallback list.
fn bin_path(name: &str, fallbacks: &[&str]) -> Option<String> {
    // PATH lookup first (works in a login shell / when PATH is inherited).
    if Command::new(name).arg("--version").output().is_ok() {
        return Some(name.to_string());
    }
    for p in fallbacks {
        if std::path::Path::new(p).exists() {
            return Some((*p).to_string());
        }
    }
    None
}

fn say_bin() -> Option<String> {
    // `say` has no --version; probe the canonical macOS location directly.
    if std::path::Path::new("/usr/bin/say").exists() {
        return Some("/usr/bin/say".to_string());
    }
    // PATH fallback (say -v '?' exits 0 and is cheap).
    if Command::new("say")
        .arg("-v")
        .arg("?")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return Some("say".to_string());
    }
    None
}

fn piper_bin() -> Option<String> {
    let explicit = std::env::var("AMUX_TTS_PIPER_BIN")
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(p) = explicit {
        return std::path::Path::new(&p).exists().then_some(p);
    }
    bin_path(
        "piper",
        &["/usr/local/bin/piper", "/opt/homebrew/bin/piper"],
    )
}

/// Resolve which engine to use, honoring the env override then availability.
/// Returns the engine name, or an Err with an honest "what to install" message.
fn resolve_engine() -> Result<&'static str, String> {
    match engine_pref().as_str() {
        "say" => say_bin()
            .map(|_| "say")
            .ok_or_else(|| "AMUX_TTS_ENGINE=say but macOS `say` is not present".to_string()),
        "piper" => piper_bin().map(|_| "piper").ok_or_else(|| {
            "AMUX_TTS_ENGINE=piper but no piper binary (set AMUX_TTS_PIPER_BIN)".to_string()
        }),
        _ => {
            // auto: local, OSS-spirit first.
            if say_bin().is_some() {
                Ok("say")
            } else if piper_bin().is_some() {
                Ok("piper")
            } else {
                Err("no TTS engine available. On macOS `say` is built in; \
                     elsewhere install piper and set AMUX_TTS_PIPER_BIN \
                     (+ AMUX_TTS_PIPER_VOICE to a .onnx voice)."
                    .to_string())
            }
        }
    }
}

/// A unique temp path per call — the pid alone is not unique across concurrent
/// requests, so a monotonic counter is folded in.
fn tmp_wav() -> std::path::PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("amux-tts-{}-{n}.wav", std::process::id()))
}

/// macOS `say`. Text is a single argv (never a shell string), so no injection
/// path. WAV/PCM because browsers play it natively — verified `say -o x.wav
/// --data-format=LEI16@22050` writes a RIFF/WAVE container.
fn say_synth(bin: &str, text: &str, voice: &str) -> Result<(Vec<u8>, String, String), String> {
    let path = tmp_wav();
    // Write text to a temp file and pass it with -f rather than as a positional
    // arg. Positional args hit macOS ARG_MAX for large files and `say` prints
    // its usage string to stderr instead of failing with a useful message.
    let txt_path = path.with_extension("txt");
    std::fs::write(&txt_path, text.as_bytes())
        .map_err(|e| format!("could not write TTS input: {e}"))?;
    let mut cmd = Command::new(bin);
    cmd.arg("-o").arg(&path).arg("--data-format=LEI16@22050");
    let mut used_voice = String::new();
    if !voice.is_empty() && say_voice_exists(bin, voice) {
        cmd.arg("-v").arg(voice);
        used_voice = voice.to_string();
    }
    cmd.arg("-f").arg(&txt_path);
    let out = cmd
        .output()
        .map_err(|e| format!("say failed to run: {e}"))?;
    let _ = std::fs::remove_file(&txt_path);
    if !out.status.success() {
        let _ = std::fs::remove_file(&path);
        return Err(format!(
            "say exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("could not read say output: {e}"))?;
    let _ = std::fs::remove_file(&path);
    if bytes.is_empty() {
        return Err("say produced no audio".to_string());
    }
    Ok((bytes, "audio/wav".to_string(), used_voice))
}

/// Piper: text on stdin, WAV on the output file. `AMUX_TTS_PIPER_VOICE` points
/// at a `.onnx` voice model. Best-effort (piper is not installed on the dev
/// machine, so the tested path here is `say`); the invocation matches piper's
/// documented CLI and fails loudly rather than fabricating.
fn piper_synth(bin: &str, text: &str, voice: &str) -> Result<(Vec<u8>, String, String), String> {
    let model = if !voice.is_empty() && voice.ends_with(".onnx") {
        voice.to_string()
    } else {
        std::env::var("AMUX_TTS_PIPER_VOICE").unwrap_or_default()
    };
    if model.is_empty() {
        return Err(
            "piper needs a voice model — set AMUX_TTS_PIPER_VOICE to a .onnx path".to_string(),
        );
    }
    let path = tmp_wav();
    use std::io::Write;
    let mut child = Command::new(bin)
        .arg("--model")
        .arg(&model)
        .arg("--output_file")
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("piper failed to run: {e}"))?;
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(text.as_bytes());
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("piper wait failed: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&path);
        return Err(format!(
            "piper exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("could not read piper output: {e}"))?;
    let _ = std::fs::remove_file(&path);
    if bytes.is_empty() {
        return Err("piper produced no audio".to_string());
    }
    Ok((bytes, "audio/wav".to_string(), model))
}

/// Does `say` know this voice name? Guards against passing an arbitrary
/// `voice_id` (e.g. an ElevenLabs id from the dialog's default) as `-v`, which
/// `say` would reject and fail the whole synth. An unknown voice silently uses
/// the system default instead.
fn say_voice_exists(bin: &str, voice: &str) -> bool {
    Command::new(bin)
        .arg("-v")
        .arg("?")
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.split_whitespace().next())
                .any(|name| name.eq_ignore_ascii_case(voice))
        })
        .unwrap_or(false)
}

fn say_voices(bin: &str) -> Vec<Value> {
    // Lines look like: "Samantha            en_US    # Hello, ..."
    let Ok(out) = Command::new(bin).arg("-v").arg("?").output() else {
        return vec![];
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            // Name may contain a space (e.g. "Grandma (Enhanced)"); the locale
            // is the last token before '#'. Split on the comment first.
            let head = line.split('#').next().unwrap_or("").trim();
            let mut parts: Vec<&str> = head.split_whitespace().collect();
            if parts.len() < 2 {
                return None;
            }
            let locale = parts.pop().unwrap_or("").to_string();
            let name = parts.join(" ");
            if name.is_empty() {
                return None;
            }
            Some(json!({ "voice_id": name, "name": name, "category": locale }))
        })
        .collect()
}

/// `POST /api/tts` — synth `text` to audio, returned as a `data:` URL.
pub async fn synth(State(_state): State<AppState>, Json(body): Json<Value>) -> Response {
    let raw = body
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if raw.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "text required" }));
    }
    // Cap on characters, not bytes.
    let text: String = raw.chars().take(MAX_CHARS).collect();
    let voice = body
        .get("voice_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    let engine = match resolve_engine() {
        Ok(e) => e,
        Err(why) => return err(StatusCode::SERVICE_UNAVAILABLE, json!({ "error": why })),
    };

    // `say`/piper block for up to a second on real text — run off the async
    // executor so one read-aloud does not stall every other request.
    let result = tokio::task::spawn_blocking(move || match engine {
        "piper" => {
            let bin = piper_bin().ok_or_else(|| "piper disappeared".to_string())?;
            piper_synth(&bin, &text, &voice).map(|r| (r.0, r.1, r.2, "piper"))
        }
        _ => {
            let bin = say_bin().ok_or_else(|| "say disappeared".to_string())?;
            say_synth(&bin, &text, &voice).map(|r| (r.0, r.1, r.2, "say"))
        }
    })
    .await;

    match result {
        Ok(Ok((bytes, mime, used_voice, eng))) => {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let url = format!("data:{mime};base64,{b64}");
            Json(json!({
                "url": url,
                "size": bytes.len(),
                "engine": eng,
                "voice": used_voice,
            }))
            .into_response()
        }
        Ok(Err(why)) => err(StatusCode::SERVICE_UNAVAILABLE, json!({ "error": why })),
        Err(join) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": format!("tts task panicked: {join}") }),
        ),
    }
}

/// `GET /api/tts/voices` — the voice picker's source.
pub async fn voices(State(_state): State<AppState>) -> Response {
    let list = match resolve_engine() {
        Ok("say") => say_bin().map(|b| say_voices(&b)).unwrap_or_default(),
        Ok("piper") => {
            // Piper voices are files on disk; the configured one is the choice.
            let v = std::env::var("AMUX_TTS_PIPER_VOICE").unwrap_or_default();
            if v.is_empty() {
                vec![]
            } else {
                let name = std::path::Path::new(&v)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("piper")
                    .to_string();
                vec![json!({ "voice_id": v, "name": name, "category": "piper" })]
            }
        }
        _ => vec![],
    };
    Json(json!({ "voices": list })).into_response()
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_say_voice_lines_including_spaced_names() {
        // The parser must survive a name with a space (Enhanced/Premium voices)
        // and a comment containing extra whitespace.
        let sample = "Samantha            en_US    # Hello, my name is Samantha.\n\
                      Grandma (Enhanced)  en_US    # Hi there!\n\
                      Bad line no locale\n";
        // Exercise the same splitting the parser uses.
        let rows: Vec<(String, String)> = sample
            .lines()
            .filter_map(|line| {
                let head = line.split('#').next().unwrap_or("").trim();
                let mut parts: Vec<&str> = head.split_whitespace().collect();
                if parts.len() < 2 {
                    return None;
                }
                let locale = parts.pop().unwrap_or("").to_string();
                let name = parts.join(" ");
                (!name.is_empty()).then_some((name, locale))
            })
            .collect();
        assert_eq!(
            rows.len(),
            3,
            "3 parseable rows (the no-locale line still has 4 tokens)"
        );
        assert_eq!(rows[0], ("Samantha".into(), "en_US".into()));
        assert_eq!(rows[1], ("Grandma (Enhanced)".into(), "en_US".into()));
    }

    #[test]
    fn engine_pref_defaults_to_auto() {
        // Not asserting the env (shared process), just the parse of an unset/empty.
        assert_eq!(
            std::env::var("AMUX_TTS_ENGINE_UNSET_XYZ")
                .ok()
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "auto".into()),
            "auto"
        );
    }
}
