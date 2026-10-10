//! Example invocations for a skill, shown when it is expanded in the Skills
//! tab ("so I can see how to best use it", Ethan 2026-10-10).
//!
//! Two sources, both derived from the skill's own markdown:
//! - **written**: any line in the body that already is an invocation
//!   (`/name args`, optionally in a list item, quote or backticks). Free and
//!   exact, so it is always included.
//! - **inferred**: realistic arguments need judgment (what does a good
//!   `/pr-merge` argument look like?), so the meta-task model
//!   (`mdai::resolve_model(None)`, i.e. `AMUX_HELPER_MODEL`) is asked once per
//!   VERSION of the file. The answer is cached under
//!   `<amux_home>/skill-examples/<name>.json` keyed by the content's sha256,
//!   so an edit regenerates and an unchanged skill never costs a second call.
//!
//! A failed or unparseable model answer is reported (`measured: false`,
//! `why_unmeasured`, WARN `verdict=skill_examples_unavailable`), never cached,
//! and the written examples still render.

use super::mdai::ModelClient;
use super::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use sha2::Digest;
use std::collections::HashMap;

const MAX_EXAMPLES: usize = 5;

fn sha(content: &str) -> String {
    hex::encode(sha2::Sha256::digest(content.as_bytes()))
}

fn body_of(content: &str) -> &str {
    if let Some(rest) = content.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            return &rest[end + 4..];
        }
    }
    content
}

/// Lines in the skill body that already are `/name ...` invocations.
pub(crate) fn written_examples(name: &str, content: &str) -> Vec<String> {
    let cmd = format!("/{name}");
    let mut out: Vec<String> = Vec::new();
    for line in body_of(content).lines() {
        let t = line
            .trim()
            .trim_start_matches(['-', '*', '>', ' '])
            .trim()
            .trim_matches('`')
            .trim();
        let rest = match t.strip_prefix(&cmd) {
            Some(r) => r,
            None => continue,
        };
        if !(rest.is_empty() || rest.starts_with(' ')) {
            continue; // `/pr-merger` is not `/pr-merge`
        }
        let inv = t.trim_end_matches('`').trim().to_string();
        if !out.contains(&inv) {
            out.push(inv);
        }
    }
    out.truncate(MAX_EXAMPLES);
    out
}

pub(crate) fn prompt(name: &str, content: &str) -> String {
    format!(
        "You write example invocations for a Claude Code slash command named /{name}. \
Its full definition file is between the markers below.\n\n\
Return ONLY a JSON array of 3 to 5 objects, each {{\"invocation\": string, \"purpose\": string}}.\n\
- invocation: exactly what a user would type, starting with \"/{name}\", with realistic, \
concrete arguments that fit the argument-hint and instructions (no placeholders like <x> or [y]). \
If the command takes no arguments, include the bare \"/{name}\" once and vary only where it makes sense.\n\
- purpose: one short sentence saying what that call does or when to use it.\n\
- Cover different uses, from the most common to the most powerful.\n\
- Plain sentences: no em dashes.\n\
No prose outside the JSON.\n\n<<<SKILL\n{content}\nSKILL>>>"
    )
}

/// The model's answer, kept only where each invocation really calls this skill.
pub(crate) fn parse_examples(name: &str, out: &str) -> Option<Vec<Value>> {
    let start = out.find('[')?;
    let end = out.rfind(']')?;
    if end <= start {
        return None;
    }
    let arr: Vec<Value> = serde_json::from_str(&out[start..=end]).ok()?;
    let cmd = format!("/{name}");
    let mut seen = std::collections::HashSet::new();
    let examples: Vec<Value> = arr
        .into_iter()
        .filter_map(|v| {
            let inv = v.get("invocation")?.as_str()?.trim().to_string();
            let rest = inv.strip_prefix(&cmd)?;
            if !(rest.is_empty() || rest.starts_with(' ')) || !seen.insert(inv.clone()) {
                return None;
            }
            // The owner's writing rules ban em dashes; the prompt says so and
            // this holds the line when a model ignores it.
            let purpose = v
                .get("purpose")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .replace(" \u{2014} ", ": ")
                .replace('\u{2014}', ", ");
            Some(json!({ "invocation": inv, "purpose": purpose }))
        })
        .take(MAX_EXAMPLES)
        .collect();
    (!examples.is_empty()).then_some(examples)
}

fn cache_path(home: &std::path::Path, name: &str) -> std::path::PathBuf {
    home.join("skill-examples").join(format!("{name}.json"))
}

/// Cached inferred examples for this exact content, or a fresh model call.
/// Returns (examples, from_cache) or the reason none could be produced.
pub(crate) fn inferred_examples(
    home: &std::path::Path,
    client: &dyn ModelClient,
    model: &str,
    name: &str,
    content: &str,
) -> Result<(Vec<Value>, bool), String> {
    let digest = sha(content);
    let path = cache_path(home, name);
    if let Ok(raw) = std::fs::read_to_string(&path) {
        if let Ok(c) = serde_json::from_str::<Value>(&raw) {
            if c["sha256"] == digest.as_str() {
                if let Some(ex) = c["examples"].as_array() {
                    return Ok((ex.clone(), true));
                }
            }
        }
    }
    let out = client
        .complete(model, &prompt(name, content))
        .map_err(|e| format!("meta-task model call failed: {e}"))?;
    let examples = parse_examples(name, &out).ok_or_else(|| {
        format!(
            "the model's answer held no usable /{name} examples ({} bytes)",
            out.len()
        )
    })?;
    let record = json!({ "sha256": digest, "model": model, "generated_at": crate::config::now_f64(), "examples": examples });
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, record.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
    Ok((examples, false))
}

/// The skill's markdown: the Skills-tab table first, then a command file.
fn skill_content(state: &AppState, name: &str, source: Option<&str>) -> Option<String> {
    if source != Some("file") {
        if let Ok(conn) = state.store.read() {
            let row: Option<String> = conn
                .query_row("SELECT content FROM skills WHERE name=?1", [name], |r| r.get(0))
                .ok();
            if row.is_some() {
                return row;
            }
        }
    }
    super::skills::command_dirs()
        .into_iter()
        .find_map(|d| std::fs::read_to_string(d.join(format!("{name}.md"))).ok())
}

/// Warm the cache after a save so the first expand does not wait on the model.
pub(crate) fn prewarm(name: String, content: String) {
    tokio::spawn(async move {
        let _guard = GENERATING.lock().await;
        let n2 = name.clone();
        let res = tokio::task::spawn_blocking(move || {
            let model = super::mdai::resolve_model(None);
            let client = super::mdai::best_model();
            inferred_examples(&crate::config::amux_home(), client.as_ref(), &model, &n2, &content)
        })
        .await;
        if let Ok(Err(why)) = res {
            tracing::warn!(skill = %name, error = %why, verdict = "skill_examples_unavailable",
                measured = false, "could not infer example invocations after a skill save");
        }
    });
}

// One generation at a time: two expands of the same skill must not pay twice.
static GENERATING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(super) async fn get_examples(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let name = name.trim_start_matches('/').to_string();
    if super::skills::bad_name(&name) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid name" }))).into_response();
    }
    let Some(content) = skill_content(&state, &name, q.get("source").map(String::as_str)) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response();
    };
    let written = written_examples(&name, &content);
    let _guard = GENERATING.lock().await;
    let (n2, c2) = (name.clone(), content.clone());
    let inferred = tokio::task::spawn_blocking(move || {
        let model = super::mdai::resolve_model(None);
        let client = super::mdai::best_model();
        inferred_examples(&crate::config::amux_home(), client.as_ref(), &model, &n2, &c2)
    })
    .await
    .unwrap_or_else(|e| Err(format!("example generation panicked: {e}")));
    drop(_guard);
    match inferred {
        Ok((examples, cached)) => Json(json!({
            "name": name, "written": written, "inferred": examples, "cached": cached,
            "measured": true, "n_considered": 1,
        }))
        .into_response(),
        Err(why) => {
            tracing::warn!(skill = %name, error = %why, verdict = "skill_examples_unavailable",
                measured = false, "could not infer example invocations for a skill");
            Json(json!({
                "name": name, "written": written, "inferred": [], "cached": false,
                "measured": false, "n_considered": 1, "why_unmeasured": why,
            }))
            .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(std::sync::Mutex<usize>, String);
    impl ModelClient for Fake {
        fn complete(&self, _model: &str, _prompt: &str) -> Result<String, String> {
            *self.0.lock().unwrap() += 1;
            Ok(self.1.clone())
        }
    }

    #[test]
    fn written_examples_are_lines_that_already_invoke_the_skill() {
        let md = "---\ndescription: x\n---\nUse it like:\n- `/pr-merge 264`\n> /pr-merge all PRs\n/pr-merger 1\nsee /pr-merge in prose\n/pr-merge 264\n";
        assert_eq!(written_examples("pr-merge", md), vec!["/pr-merge 264", "/pr-merge all PRs"]);
    }

    #[test]
    fn parsed_examples_keep_only_real_invocations_of_this_skill() {
        let out = "Here you go:\n[{\"invocation\":\"/pr-merge 264\",\"purpose\":\"Merge one PR\"},\
{\"invocation\":\"/other 1\",\"purpose\":\"wrong skill\"},{\"invocation\":\"/pr-merger 2\"},\
{\"invocation\":\"/pr-merge 264\",\"purpose\":\"dupe\"},{\"invocation\":\"/pr-merge\",\"purpose\":\"bare\"}]";
        let ex = parse_examples("pr-merge", out).unwrap();
        let invs: Vec<&str> = ex.iter().map(|e| e["invocation"].as_str().unwrap()).collect();
        assert_eq!(invs, vec!["/pr-merge 264", "/pr-merge"]);
        assert_eq!(ex[0]["purpose"], "Merge one PR");
        let dashed = parse_examples("s", "[{\"invocation\":\"/s 4\",\"purpose\":\"Quick check \u{2014} last 4 hours\"}]").unwrap();
        assert_eq!(dashed[0]["purpose"], "Quick check: last 4 hours");
        assert!(parse_examples("pr-merge", "no json here").is_none());
        assert!(parse_examples("pr-merge", "[{\"invocation\":\"/other\"}]").is_none());
    }

    /// One model call per content version: a repeat is served from the cache,
    /// an edit regenerates, and a useless answer is not cached.
    #[test]
    fn examples_are_generated_once_per_version_of_the_skill() {
        let home = tempfile::tempdir().unwrap();
        let good = Fake(Default::default(), "[{\"invocation\":\"/s go\",\"purpose\":\"p\"}]".into());
        let (ex, cached) = inferred_examples(home.path(), &good, "m", "s", "v1").unwrap();
        assert_eq!((ex.len(), cached), (1, false));
        let (_, cached) = inferred_examples(home.path(), &good, "m", "s", "v1").unwrap();
        assert!(cached);
        assert_eq!(*good.0.lock().unwrap(), 1, "a repeat must not call the model");
        let (_, cached) = inferred_examples(home.path(), &good, "m", "s", "v2").unwrap();
        assert!(!cached, "an edited skill regenerates");
        assert_eq!(*good.0.lock().unwrap(), 2);

        let bad = Fake(Default::default(), "sorry".into());
        assert!(inferred_examples(home.path(), &bad, "m", "t", "v1").is_err());
        assert!(!cache_path(home.path(), "t").exists(), "a failed answer is never cached");
    }
}
