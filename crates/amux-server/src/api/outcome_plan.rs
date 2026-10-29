//! `POST /api/outcome/plan`: what "Describe what you want" starts.
//!
//! The dialog used to compute everything from the first words of the text:
//! "I need a worker to behave like a CRM that tracks my outreach" became a
//! worker named `need-worker-behave-crm`, started in the remembered default
//! directory (`/home/amux`), as a `/goal` (Ethan, 2026-10-29). Picking a name
//! and the place the work lives is judgment, so the helper model makes it,
//! from the text, any attached audio or files, and the directories and groups
//! the fleet already works in. It can only pick a directory or group that
//! exists; anything else falls back to "", never to an invented path.
//!
//! Audio attachments are transcribed once and the transcript is saved beside
//! the file (`<file>.transcript.txt`), so the worker reads the same words the
//! planner did and the audio itself stays on disk.

use super::AppState;
use crate::config::amux_home;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/outcome/plan", post(plan))
}

const AUDIO_EXT: [&str; 10] = ["m4a", "mp3", "wav", "webm", "ogg", "aac", "caf", "flac", "opus", "mp4"];
const MAX_AUDIO_BYTES: u64 = 25 * 1024 * 1024;
/// How much of each attachment the planner reads. The worker gets the files.
const EXCERPT_CHARS: usize = 6000;

fn audio_mime(ext: &str) -> &'static str {
    match ext {
        "m4a" | "mp4" | "aac" | "caf" => "audio/mp4",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "webm" => "audio/webm",
        "ogg" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        _ => "application/octet-stream",
    }
}

fn user_home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/"))
}

/// Paths written as `@/abs/path` in the description, the way a file is
/// referenced everywhere else in amux.
pub(crate) fn at_paths(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|w| w.strip_prefix('@'))
        .map(|p| p.trim_end_matches([',', '.', ';', ')', '"', '\'']).to_string())
        .filter(|p| p.starts_with('/'))
        .collect()
}

/// An attachment the planner may read: an existing file under the user's
/// home or the amux home, nothing else.
fn readable_attachment(raw: &str) -> Result<PathBuf, String> {
    let p = Path::new(raw).canonicalize().map_err(|e| format!("not found: {e}"))?;
    if !p.is_file() {
        return Err("not a file".into());
    }
    let ok = [user_home(), amux_home()]
        .iter()
        .filter_map(|r| r.canonicalize().ok())
        .any(|r| p.starts_with(r));
    if !ok {
        return Err("outside the home directory".into());
    }
    Ok(p)
}

fn transcript_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".transcript.txt");
    PathBuf::from(s)
}

/// Read or make the transcript of one audio file. A transcript already beside
/// it is reused, so a second plan of the same file costs nothing.
async fn transcript_of(state: &AppState, p: &Path, ext: &str) -> Result<(PathBuf, String, &'static str), String> {
    let tp = transcript_path(p);
    if let Ok(t) = std::fs::read_to_string(&tp) {
        if !t.trim().is_empty() {
            return Ok((tp, t, "saved"));
        }
    }
    let len = std::fs::metadata(p).map(|m| m.len()).map_err(|e| e.to_string())?;
    if len > MAX_AUDIO_BYTES {
        return Err(format!("audio is {} MB; the limit is 25 MB", len / (1024 * 1024)));
    }
    let raw = std::fs::read(p).map_err(|e| e.to_string())?;
    let (text, engine) = super::dictation::transcribe_clip(state, &raw, audio_mime(ext)).await?;
    if text.trim().is_empty() {
        return Err("the transcription engine heard no speech".into());
    }
    if let Err(e) = std::fs::write(&tp, &text) {
        tracing::warn!(path = %tp.display(), error = %e, verdict = "outcome_transcript_not_saved",
            "outcome plan: transcribed an attachment but could not save the transcript beside it");
    }
    Ok((tp, text, engine))
}

/// One directory the fleet already works in, with who works there.
#[derive(Debug, Clone, Default)]
pub(crate) struct Place {
    pub workers: Vec<String>,
    pub groups: BTreeSet<String>,
}

fn env_value(body: &str, key: &str) -> Option<String> {
    body.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))
        .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|v| !v.is_empty())
}

/// Directories a new worker could start in: every directory a worker already
/// uses, and the projects directly under the home and `~/Dev` (a directory
/// holding `.git`, `CLAUDE.md` or `README.md`). The home itself is never a
/// candidate: it is the default that put the CRM worker in the wrong place.
pub(crate) fn candidate_dirs(amux: &Path, home: &Path) -> BTreeMap<String, Place> {
    let mut out: BTreeMap<String, Place> = BTreeMap::new();
    if let Ok(rd) = std::fs::read_dir(amux.join("sessions")) {
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("env") {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(&path) else { continue };
            let Some(dir) = env_value(&body, "CC_DIR") else { continue };
            if Path::new(&dir) == home || !Path::new(&dir).is_dir() {
                continue;
            }
            let place = out.entry(dir).or_default();
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                place.workers.push(stem.to_string());
            }
            for g in env_value(&body, "CC_TAGS").unwrap_or_default().split(',') {
                if !g.trim().is_empty() {
                    place.groups.insert(g.trim().to_string());
                }
            }
        }
    }
    for root in [home.to_path_buf(), home.join("Dev")] {
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let hidden = p.file_name().and_then(|n| n.to_str()).is_none_or(|n| n.starts_with('.'));
            if hidden || !p.is_dir() {
                continue;
            }
            if ["Dev", "tmp", "snap", "node_modules"].iter().any(|n| p.file_name().and_then(|x| x.to_str()) == Some(n)) {
                continue;
            }
            if [".git", "CLAUDE.md", "README.md"].iter().any(|m| p.join(m).exists()) {
                out.entry(p.to_string_lossy().into_owned()).or_default();
            }
        }
    }
    while out.len() > 80 {
        let Some(k) = out.iter().find(|(_, p)| p.workers.is_empty()).map(|(k, _)| k.clone()) else { break };
        out.remove(&k);
    }
    out
}

pub(crate) fn taken_names(amux: &Path) -> BTreeSet<String> {
    std::fs::read_dir(amux.join("sessions"))
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let p = e.path();
                    (p.extension().and_then(|x| x.to_str()) == Some("env"))
                        .then(|| p.file_stem().and_then(|s| s.to_str()).map(str::to_string))
                        .flatten()
                })
                .collect()
        })
        .unwrap_or_default()
}

const FILLER: &[&str] = &[
    "a", "an", "the", "and", "or", "to", "of", "for", "in", "on", "with", "my", "our", "me", "i", "we", "it", "is",
    "be", "that", "this", "so", "can", "please", "want", "would", "like", "make", "some", "new", "into", "from",
    "at", "by", "need", "needs", "worker", "workers", "agent", "behave", "behaves", "act", "acts", "as", "wanna",
    "gonna", "able", "dedicated", "help", "helps", "create", "build", "set", "up", "you", "your", "something", "lets",
];

/// The name when no model answers: the first content words, minus the
/// request's scaffolding ("I need a worker to behave like ...").
pub(crate) fn fallback_name(text: &str) -> String {
    let words: Vec<String> = text
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty() && !FILLER.contains(w))
        .map(str::to_string)
        .collect();
    let base = words.iter().take(3).cloned().collect::<Vec<_>>().join("-");
    let base: String = base.chars().take(24).collect();
    let base = base.trim_end_matches('-').to_string();
    if base.is_empty() { "outcome".into() } else { base }
}

/// A worker name: lowercase letters, digits and single dashes, 2 to 32 chars.
pub(crate) fn clean_name(raw: &str) -> Option<String> {
    let s: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let parts: Vec<&str> = s.split('-').filter(|p| !p.is_empty()).collect();
    let s = parts.join("-");
    let s: String = s.chars().take(32).collect();
    let s = s.trim_end_matches('-').to_string();
    (s.len() >= 2).then_some(s)
}

pub(crate) fn unique(name: &str, taken: &BTreeSet<String>) -> String {
    if !taken.contains(name) {
        return name.to_string();
    }
    (2..).map(|n| format!("{name}-{n}")).find(|c| !taken.contains(c)).unwrap_or_else(|| name.to_string())
}

fn build_prompt(text: &str, excerpts: &[(String, String)], places: &BTreeMap<String, Place>, groups: &BTreeSet<String>) -> String {
    let mut p = String::from(
        "You are setting up a new AI worker from its owner's request. Choose:\n\
         - name: 1 to 3 lowercase words joined by '-', at most 24 characters, naming the job \
         (for example gtm-crm or ar-reconcile). Never use filler such as need, worker, want, behave, like.\n\
         - dir: the directory from the list below where this work's files already live. \
         Copy one path exactly, or \"\" if none fits.\n\
         - groups: zero to two existing group names from the list below that this worker belongs with.\n\
         - desc: one plain sentence, at most 120 characters, saying what the worker does.\n\
         Reply with only a JSON object: {\"name\":\"...\",\"dir\":\"...\",\"groups\":[...],\"desc\":\"...\"}\n\n",
    );
    p.push_str(&format!("REQUEST:\n{}\n\n", text.trim()));
    for (path, ex) in excerpts {
        p.push_str(&format!("ATTACHED {path} (excerpt):\n{ex}\n\n"));
    }
    p.push_str("DIRECTORIES:\n");
    for (dir, place) in places {
        p.push_str(&format!("- {dir}"));
        if !place.workers.is_empty() {
            p.push_str(&format!(" (workers: {}", place.workers.join(", ")));
            if !place.groups.is_empty() {
                p.push_str(&format!("; groups: {}", place.groups.iter().cloned().collect::<Vec<_>>().join(", ")));
            }
            p.push(')');
        }
        p.push('\n');
    }
    p.push_str(&format!("\nGROUPS: {}\n", groups.iter().cloned().collect::<Vec<_>>().join(", ")));
    p
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Choice {
    pub name: Option<String>,
    pub dir: String,
    pub groups: Vec<String>,
    pub desc: String,
}

/// Read the model's answer. Only an existing candidate directory and existing
/// groups survive; a name is cleaned to a valid worker name.
pub(crate) fn parse_choice(raw: &str, places: &BTreeMap<String, Place>, groups: &BTreeSet<String>) -> Option<Choice> {
    let (a, b) = (raw.find('{')?, raw.rfind('}')?);
    let v: Value = serde_json::from_str(raw.get(a..=b)?).ok()?;
    let dir = v["dir"].as_str().unwrap_or("").trim().trim_end_matches('/').to_string();
    let desc: String = v["desc"].as_str().unwrap_or("").trim().chars().take(140).collect();
    Some(Choice {
        name: v["name"].as_str().and_then(clean_name),
        dir: if places.contains_key(&dir) { dir } else { String::new() },
        groups: v["groups"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).filter(|g| groups.contains(*g)).take(2).map(str::to_string).collect())
            .unwrap_or_default(),
        desc,
    })
}

async fn plan(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let text = body["text"].as_str().unwrap_or("").trim().to_string();
    if text.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "describe what you want first"}))).into_response();
    }
    let mut files: Vec<String> = body["files"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    for p in at_paths(&text) {
        if !files.contains(&p) {
            files.push(p);
        }
    }

    // Attachments: audio is transcribed (and the transcript kept); a text
    // file is read; anything else is passed to the worker by path only.
    let mut attachments = Vec::new();
    let mut excerpts: Vec<(String, String)> = Vec::new();
    for f in files.iter().take(8) {
        let p = match readable_attachment(f) {
            Ok(p) => p,
            Err(e) => {
                attachments.push(json!({"path": f, "kind": "unread", "error": e}));
                continue;
            }
        };
        let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
        let shown = p.to_string_lossy().into_owned();
        if AUDIO_EXT.contains(&ext.as_str()) {
            match transcript_of(&state, &p, &ext).await {
                Ok((tp, t, engine)) => {
                    excerpts.push((shown.clone(), t.chars().take(EXCERPT_CHARS).collect()));
                    attachments.push(json!({"path": shown, "kind": "audio", "transcript": tp.to_string_lossy(),
                        "words": t.split_whitespace().count(), "engine": engine}));
                }
                Err(e) => {
                    tracing::warn!(path = %shown, error = %e, measured = false, n_considered = 1,
                        verdict = "outcome_audio_not_transcribed",
                        "outcome plan: an attached recording could not be transcribed; the worker gets its path only");
                    attachments.push(json!({"path": shown, "kind": "audio", "error": e}));
                }
            }
        } else if let Ok(t) = std::fs::read_to_string(&p) {
            excerpts.push((shown.clone(), t.chars().take(EXCERPT_CHARS).collect()));
            attachments.push(json!({"path": shown, "kind": "text", "words": t.split_whitespace().count()}));
        } else {
            attachments.push(json!({"path": shown, "kind": "file"}));
        }
    }

    let amux = amux_home();
    let places = candidate_dirs(&amux, &user_home());
    let groups: BTreeSet<String> = places.values().flat_map(|p| p.groups.iter().cloned()).collect();
    let taken = taken_names(&amux);
    let prompt = build_prompt(&text, &excerpts, &places, &groups);

    let (via, choice, fallback_reason) = match super::lookup::helper_answer(&prompt).await {
        Ok((via, answer)) => match parse_choice(&answer, &places, &groups) {
            Some(c) => (via, c, None),
            None => (via, Choice::default(), Some("the helper model's answer was not the JSON asked for".to_string())),
        },
        Err((_, e)) => ("none".to_string(), Choice::default(), Some(e)),
    };
    let name_source = if choice.name.is_some() { "model" } else { "fallback" };
    let name = unique(&choice.name.clone().unwrap_or_else(|| fallback_name(&text)), &taken);
    if let Some(why) = &fallback_reason {
        tracing::warn!(error = %why, measured = true, n_considered = places.len(), verdict = "outcome_plan_fallback",
            "outcome plan: the helper model did not choose; the name is computed and the directory is left empty");
    }
    tracing::info!(name = %name, name_source, dir = %choice.dir, attachments = attachments.len(),
        transcribed = excerpts.len(), via = %via, measured = true, n_considered = places.len(),
        verdict = "outcome_planned", "outcome plan: chose a worker name and directory for a described outcome");
    Json(json!({
        "measured": true,
        "n_considered": places.len(),
        "via": via,
        "name": name,
        "name_source": name_source,
        "dir": choice.dir,
        "groups": choice.groups,
        "desc": choice.desc,
        "attachments": attachments,
        "fallback_reason": fallback_reason,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fallback_name_drops_the_request_scaffolding() {
        let t = "I need a worker to behave like a CRM that tracks my outreach and profess towards goal";
        assert_eq!(fallback_name(t), "crm-tracks-outreach");
        assert_eq!(fallback_name("I want a worker dedicated to AMUX GTM"), "amux-gtm");
        assert_eq!(fallback_name("please"), "outcome");
    }

    #[test]
    fn names_are_cleaned_and_made_unique() {
        assert_eq!(clean_name("  GTM CRM!! ").as_deref(), Some("gtm-crm"));
        assert_eq!(clean_name("x"), None);
        let taken: BTreeSet<String> = ["gtm-crm".to_string(), "gtm-crm-2".to_string()].into();
        assert_eq!(unique("gtm-crm", &taken), "gtm-crm-3");
        assert_eq!(unique("ar", &taken), "ar");
    }

    #[test]
    fn at_paths_are_read_from_the_description() {
        let t = "Use @/home/x/a.m4a, and @/home/x/notes.md. Not @someone";
        assert_eq!(at_paths(t), vec!["/home/x/a.m4a".to_string(), "/home/x/notes.md".to_string()]);
    }

    #[test]
    fn only_existing_directories_and_groups_survive_the_model() {
        let mut places = BTreeMap::new();
        places.insert("/home/u/amux-GTM/gtm".to_string(), Place::default());
        let groups: BTreeSet<String> = ["gtm".to_string(), "sales".to_string()].into();
        let ok = parse_choice(
            "Sure: {\"name\":\"GTM CRM\",\"dir\":\"/home/u/amux-GTM/gtm/\",\"groups\":[\"gtm\",\"made-up\"],\"desc\":\"Tracks outreach.\"}",
            &places,
            &groups,
        )
        .unwrap();
        assert_eq!(ok.name.as_deref(), Some("gtm-crm"));
        assert_eq!(ok.dir, "/home/u/amux-GTM/gtm");
        assert_eq!(ok.groups, vec!["gtm".to_string()]);
        let invented = parse_choice("{\"name\":\"crm\",\"dir\":\"/home/u/crm\"}", &places, &groups).unwrap();
        assert_eq!(invented.dir, "", "a directory the model made up is never used");
        assert!(parse_choice("no json here", &places, &groups).is_none());
    }

    #[test]
    fn candidates_are_worker_dirs_and_home_projects_but_never_home_itself() {
        let amux = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let gtm = home.path().join("amux-GTM/gtm");
        std::fs::create_dir_all(&gtm).unwrap();
        std::fs::create_dir_all(home.path().join("proj/.git")).unwrap();
        std::fs::create_dir_all(home.path().join("loose")).unwrap();
        std::fs::create_dir_all(amux.path().join("sessions")).unwrap();
        std::fs::write(amux.path().join("sessions/gtm-lead.env"),
            format!("CC_DIR=\"{}\"\nCC_TAGS=\"gtm,sales\"\n", gtm.display())).unwrap();
        std::fs::write(amux.path().join("sessions/stray.env"), format!("CC_DIR=\"{}\"\n", home.path().display())).unwrap();
        let places = candidate_dirs(amux.path(), home.path());
        let g = places.get(&gtm.to_string_lossy().into_owned()).expect("a worker's directory is a candidate");
        assert_eq!(g.workers, vec!["gtm-lead".to_string()]);
        assert!(g.groups.contains("sales"));
        assert!(places.contains_key(&home.path().join("proj").to_string_lossy().into_owned()));
        assert!(!places.contains_key(&home.path().join("loose").to_string_lossy().into_owned()), "a bare folder is not a project");
        assert!(!places.contains_key(&home.path().to_string_lossy().into_owned()), "home is never a candidate");
    }
}
