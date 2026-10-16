//! Keep every running worker's card title current (Ethan, 2026-10-15: "we
//! still don't have the LLM's native task naming on the card, it still shows
//! the last message ... make sure it's always up to date and test against each
//! active worker").
//!
//! Claude Code titles only unnamed conversations, and amux names every worker,
//! so the card shows a generated title (`session_verbs::generate_task_title`).
//! That title was made only when the owner sent a prompt, so three kinds of
//! worker never had a current one, measured across the 35 running workers:
//! isolated workers (titles were skipped, the card showed the last message),
//! workers driven by the board or schedules (no owner prompt, the card showed
//! the role description), and workers whose work moved on after the prompt
//! (titles 8-9 hours old).
//!
//! Each tick looks at running Claude workers whose transcript changed since
//! the title was stamped and whose title is older than the refresh interval
//! (or absent), oldest first, and titles a few of them from their recent
//! conversation. KEEP re-stamps the title. Every tick logs one
//! `task_title_refresh_tick` line with its counts.

use serde_json::Value;

use super::registry::ids;
use crate::api::session_verbs::{self as sv, TitleInput, TitleOutcome};

const JOB: &str = ids::TASK_TITLES;

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// Text from one transcript record that a human or the model wrote, if any.
/// Tool calls, tool results, thinking and system records carry no title.
fn record_text(r: &Value) -> Option<(&'static str, String)> {
    let who = match r["type"].as_str()? {
        "user" => "user",
        "assistant" => "assistant",
        _ => return None,
    };
    if r["isMeta"] == true {
        return None;
    }
    let content = &r["message"]["content"];
    let text = if let Some(s) = content.as_str() {
        s.to_string()
    } else {
        content
            .as_array()?
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let text = text.trim();
    // Hook and harness wrappers are not the conversation.
    if text.is_empty() || text.starts_with('<') {
        return None;
    }
    Some((who, text.to_string()))
}

/// The newest human and model text in a transcript, oldest first, capped.
pub(crate) fn recent_activity(records: &[Value], max_chars: usize) -> String {
    let mut picked: Vec<String> = Vec::new();
    let mut used = 0usize;
    for r in records.iter().rev() {
        let Some((who, text)) = record_text(r) else { continue };
        let text: String = text.chars().take(600).collect();
        if used + text.len() > max_chars && !picked.is_empty() {
            break;
        }
        used += text.len();
        picked.push(format!("{who}: {text}"));
        if picked.len() >= 8 {
            break;
        }
    }
    picked.reverse();
    picked.join("\n")
}

/// Whether a worker's title is due: its transcript moved after the title was
/// stamped and the title is older than the interval, or there is no title.
pub(crate) fn title_due(now: i64, title_ts: i64, transcript_mtime: i64, refresh_s: i64) -> bool {
    if title_ts <= 0 {
        return true;
    }
    transcript_mtime > title_ts && now - title_ts >= refresh_s
}

/// When the card's title was last set, kept or attempted; 0 when never.
/// An attempt that produced nothing counts, so one worker is not retried
/// every tick.
pub(crate) fn title_stamp(meta: &serde_json::Map<String, Value>) -> i64 {
    let has_title = !meta.get("task_summary").and_then(Value::as_str).unwrap_or("").trim().is_empty();
    let set = if has_title { meta.get("task_summary_ts").and_then(Value::as_i64).unwrap_or(0) } else { 0 };
    set.max(meta.get("task_title_attempt_ts").and_then(Value::as_i64).unwrap_or(0))
}

pub fn spawn(app: crate::api::AppState) -> super::PeriodicTask {
    let secs = env_u64("AMUX_TASK_TITLE_SECS", 60).max(1);
    super::spawn_periodic(JOB, secs, move || {
        let app = app.clone();
        async move { tick(app).await }
    })
}

async fn tick(app: crate::api::AppState) {
    if env_u64("AMUX_TASK_TITLE_SECS", 60) == 0 {
        return;
    }
    let Ok(sessions) = crate::api::sessions_legacy::legacy_sessions_values(app.store.clone()).await else {
        return;
    };
    let now = crate::config::now_f64() as i64;
    let refresh_s = env_u64("AMUX_TASK_TITLE_REFRESH_S", 900) as i64;
    let per_tick = env_u64("AMUX_TASK_TITLE_PER_TICK", 3) as usize;
    let mut due: Vec<(i64, String, std::path::PathBuf)> = Vec::new();
    let mut considered = 0usize;
    let mut no_transcript = 0usize;
    for s in &sessions {
        let name = s["name"].as_str().unwrap_or("").to_string();
        if name.is_empty() || s["running"] != true || s["archived"] == true || s["provider"] != "claude" {
            continue;
        }
        // A project worker's card shows its project task, which is current.
        if s["task_source"] == "project" {
            continue;
        }
        considered += 1;
        let Some(path) = sv::session_jsonl_path(&name) else {
            no_transcript += 1;
            continue;
        };
        let Some(mtime) = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
        else {
            continue;
        };
        // The stamp of whatever title the card has, generated or hand-set:
        // a KEEP re-stamps it either way. Reading only generated stamps made
        // a hand-set title permanently due, so the same workers were kept
        // every tick and the rest of the fleet starved (21 due, 0 progress).
        let ts = title_stamp(&sv::load_meta(&name));
        if title_due(now, ts, mtime, refresh_s) {
            due.push((ts, name, path));
        }
    }
    due.sort();
    let n_due = due.len();
    let (mut generated, mut kept, mut failed, mut empty) = (0, 0, 0, 0);
    for (_, name, path) in due.into_iter().take(per_tick) {
        let activity = tokio::task::spawn_blocking(move || {
            recent_activity(&sv::iter_jsonl_tail(&path, 400_000), 3000)
        })
        .await
        .unwrap_or_default();
        if activity.trim().is_empty() {
            empty += 1;
            continue;
        }
        let activity = sv::redact_prompt_secrets(&activity);
        match sv::generate_task_title(&name, TitleInput::RecentActivity(&activity)).await {
            TitleOutcome::Generated => generated += 1,
            TitleOutcome::Kept => kept += 1,
            TitleOutcome::Failed => failed += 1,
            TitleOutcome::NoModel => {
                tracing::warn!(measured = false, n_considered = considered, verdict = "task_title_refresh_no_model",
                    "card titles cannot refresh: no helper model is configured");
                return;
            }
        }
    }
    if n_due > 0 || no_transcript > 0 {
        tracing::info!(measured = true, n_considered = considered, due = n_due, generated, kept, failed,
            no_activity = empty, no_transcript, verdict = "task_title_refresh_tick", "card task titles refreshed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_title_is_due_when_the_work_moved_on_and_the_interval_passed() {
        assert!(title_due(1000, 0, 0, 900), "no title yet");
        assert!(title_due(2000, 1000, 1500, 900), "transcript moved, title 1000s old");
        assert!(!title_due(1500, 1000, 1400, 900), "too soon");
        assert!(!title_due(5000, 1000, 900, 900), "nothing new since the title");
    }

    /// A hand-set title that the model kept is not due again next tick; the
    /// first version starved the fleet on exactly this (21 due, no progress).
    #[test]
    fn a_kept_hand_set_title_counts_as_titled() {
        let m = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(title_stamp(&m(json!({"task_summary":"Hand label","task_summary_ts":500}))), 500);
        assert_eq!(title_stamp(&m(json!({"task_summary":"Gen","task_summary_ts":700,"task_summary_source":"generated"}))), 700);
        assert_eq!(title_stamp(&m(json!({"task_summary":"","task_summary_ts":900}))), 0);
        assert!(!title_due(1000, title_stamp(&m(json!({"task_summary":"Hand label","task_summary_ts":990}))), 995, 900));
        // An attempt that produced no title still spaces the retries.
        assert_eq!(title_stamp(&m(json!({"task_summary":"Old","task_summary_ts":100,"task_title_attempt_ts":800}))), 800);
    }

    #[test]
    fn recent_activity_keeps_human_and_model_text_newest_last() {
        let recs = vec![
            json!({"type":"user","message":{"content":"first ask about the disk"}}),
            json!({"type":"assistant","message":{"content":[{"type":"thinking","thinking":"x"},{"type":"tool_use","name":"Bash"}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}),
            json!({"type":"user","isMeta":true,"message":{"content":"meta"}}),
            json!({"type":"user","message":{"content":"<system-reminder>hook</system-reminder>"}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"Fixing the card titles now."}]}}),
            json!({"type":"custom-title","customTitle":"amux"}),
        ];
        let a = recent_activity(&recs, 3000);
        assert_eq!(a, "user: first ask about the disk\nassistant: Fixing the card titles now.");
    }
}
