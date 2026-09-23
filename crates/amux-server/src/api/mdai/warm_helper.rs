//! A helper process started BEFORE the request that needs it (AMUX-4659).
//!
//! Board intake calls the helper CLI once per create, and that call is nearly
//! all of the create's latency. Measured on the amux Mac 2026-09-15, ten
//! alternating runs of the same call: cold median 20.37s (min 7.52, max 93.45)
//! against 2.34s (min 1.71, max 20.61) when the process was already up, with
//! the model's own session time ~1.2-2.1s in both. So the ~18s is startup.
//!
//! One process, one message. `claude --print` reads a prompt and exits, and it
//! cannot be started early in that form: with stdin held open it waits 3s,
//! warns "no stdin data received in 3s", and exits 1. The stream-json input
//! form waits for a message line instead, so a process can be started, sit
//! idle, take ONE message and exit. Each comparison therefore still runs in a
//! fresh process with no shared context, which is what intake needs: two
//! candidate sets must never see each other.
//!
//! Cost: a waiting process holds ~260 MB. At most one is kept, and it is only
//! started AFTER a call, so a server that never files a card holds none.
//!
//! The ready helper lives in a static, so its `Drop` cleanup never runs and a
//! restart cannot stop it that way. It does not need to: the child's stdin is a
//! pipe from this process, so when the server exits the pipe closes and the
//! helper ends its own session. Measured 2026-09-15: a waiting helper exited
//! 2.81s after stdin closed, rc 0. Without that, every deploy on this box (the
//! auto-builder restarts the server on each commit) would strand 260 MB.
use std::{
    process::Command,
    sync::Mutex,
    time::{Duration, Instant},
};

use super::helper_io;

/// Scope key. Default ON (ethos rule 1: opt-out, not opt-in), because every
/// lane pays this latency on every card it files.
pub(super) const WARM_HELPER_KEY: &str = "AMUX_WARM_HELPER";

/// A helper older than this is dropped rather than used. It is not a timeout:
/// a ready process answers fine after minutes (measured: alive and answering in
/// 2.44s after 60s idle). It bounds how stale a process the server can hand a
/// prompt to, and how long it can hold its memory unused.
const MAX_READY_AGE: Duration = Duration::from_secs(30 * 60);

struct Ready {
    cli: String,
    model: String,
    started_at: Instant,
    child: helper_io::Started,
}

static READY: Mutex<Option<Ready>> = Mutex::new(None);

/// Is the pre-start path on for this process?
pub(super) fn enabled() -> bool {
    match std::env::var(WARM_HELPER_KEY) {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => true,
    }
}

/// The helper CLI invoked in stream-json mode: it waits for one message line,
/// answers it, and exits when stdin closes. Same read-only posture as the
/// classification path it serves: no tools, no MCP servers, no hooks, no
/// session persistence.
pub(super) fn stream_command(cli: &str, model: &str) -> Command {
    let mut cmd = Command::new(cli);
    cmd.args([
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        "{\"mcpServers\":{}}",
        "--disable-slash-commands",
        "--no-session-persistence",
        "--settings",
        "{\"disableAllHooks\":true}",
    ]);
    if !model.trim().is_empty() {
        cmd.arg("--model").arg(model.trim());
    }
    super::read_only_helper_options(&mut cmd);
    cmd
}

/// The one user message this process will ever receive.
pub(super) fn message_line(prompt: &str) -> String {
    let msg = serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": [{"type": "text", "text": prompt}]},
    });
    format!("{msg}\n")
}

/// The answer out of a stream-json transcript: the `result` event's text.
///
/// Reads the LAST result event, and refuses one marked `is_error` rather than
/// handing its text back as an answer: the CLI reports a refusal, a quota stop
/// or a hit turn limit in exactly that shape, and those are not model output.
#[cfg(test)]
pub(super) fn parse_result(stdout: &str) -> Result<String, String> {
    parse_completion(stdout).map(|result| result.text)
}

pub(super) fn parse_completion(stdout: &str) -> Result<super::ModelCompletion, String> {
    let mut answer: Option<Result<super::ModelCompletion, String>> = None;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if event.get("type").and_then(serde_json::Value::as_str) != Some("result") {
            continue;
        }
        let text = event
            .get("result")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        answer = Some(
            if event.get("is_error").and_then(serde_json::Value::as_bool) == Some(true) {
                let subtype = event
                    .get("subtype")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("error");
                Err(format!(
                    "helper reported {subtype}: {}",
                    text.chars().take(200).collect::<String>()
                ))
            } else if text.is_empty() {
                Err("helper returned a result event with no text".to_string())
            } else {
                Ok(super::ModelCompletion {
                    text,
                    usage: event.get("usage").filter(|v| v.is_object()).cloned(),
                })
            },
        );
    }
    answer.unwrap_or_else(|| Err("helper produced no result event".to_string()))
}

/// Take the ready helper if it matches this call and is still usable.
///
/// A helper for a DIFFERENT model is dropped rather than reused: the model is
/// fixed at spawn, so handing this one a prompt would answer with a model the
/// caller did not ask for, which no caller could detect from the answer.
pub(super) fn take(cli: &str, model: &str) -> Option<helper_io::Started> {
    let mut slot = READY.lock().ok()?;
    let ready = slot.take()?;
    let mut ready = ready;
    let reason = if ready.cli != cli || ready.model != model {
        "different_model"
    } else if ready.started_at.elapsed() > MAX_READY_AGE {
        "too_old"
    } else if ready.child.exited() {
        "exited_while_idle"
    } else {
        return Some(ready.child);
    };
    tracing::info!(target: "amux::model_helper", verdict = "warm_helper_discarded", reason,
        measured = true, n_considered = 1, age_s = ready.started_at.elapsed().as_secs(),
        "dropped the ready helper instead of using it");
    None
}

/// Start a helper for the NEXT call, in the background, if none is waiting.
///
/// Deliberately called after a call rather than at boot: a server that files no
/// cards should hold no helper, and starting one costs the memory whether or
/// not it is ever used.
pub(super) fn prepare(cli: String, model: String) {
    if !enabled() {
        return;
    }
    match READY.lock() {
        Ok(slot) if slot.is_some() => return,
        Ok(_) => {}
        Err(_) => return,
    }
    std::thread::spawn(move || {
        let started_at = Instant::now();
        let child = match helper_io::start(stream_command(&cli, &model)) {
            Ok(child) => child,
            Err(e) => {
                tracing::warn!(target: "amux::model_helper", helper = %cli, error = ?e,
                    verdict = "warm_helper_start_failed", measured = true, n_considered = 1,
                    "could not start a helper ahead of the next call; it will run cold");
                return;
            }
        };
        let Ok(mut slot) = READY.lock() else { return };
        if slot.is_some() {
            return;
        }
        *slot = Some(Ready {
            cli,
            model,
            started_at,
            child,
        });
        tracing::info!(target: "amux::model_helper", verdict = "warm_helper_ready",
            measured = true, n_considered = 1, "a helper is up and waiting for the next call");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// READY is one slot shared by the whole process, and cargo runs these in
    /// parallel threads. Without this, one test's helper answers another's
    /// `take` (or makes its `prepare` skip), which reads as a broken feature.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    #[test]
    fn provider_usage_is_preserved_and_missing_usage_is_not_zero() {
        let result=parse_completion(r#"{"type":"result","result":"{}","usage":{"input_tokens":11,"output_tokens":7,"cache_read_input_tokens":80}}"#).unwrap();
        assert_eq!(result.usage.unwrap()["cache_read_input_tokens"], 80);
        assert!(parse_completion(r#"{"type":"result","result":"{}"}"#)
            .unwrap()
            .usage
            .is_none());
    }

    #[test]
    fn the_answer_is_the_last_result_event_and_an_error_result_is_not_an_answer() {
        let ok = "{\"type\":\"system\",\"subtype\":\"init\"}\n\
                  {\"type\":\"assistant\",\"message\":{}}\n\
                  {\"type\":\"result\",\"is_error\":false,\"result\":\"{\\\"action\\\":\\\"create\\\"}\"}";
        assert_eq!(parse_result(ok).unwrap(), "{\"action\":\"create\"}");

        let err = "{\"type\":\"result\",\"is_error\":true,\"subtype\":\"error_max_turns\",\"result\":\"limit\"}";
        assert!(parse_result(err).unwrap_err().contains("error_max_turns"));

        // A transcript that never reached a result is not an empty answer.
        assert!(parse_result("{\"type\":\"system\"}")
            .unwrap_err()
            .contains("no result event"));
        assert!(parse_result("").unwrap_err().contains("no result event"));
        // Nor is a result event carrying no text.
        assert!(parse_result("{\"type\":\"result\",\"result\":\"\"}")
            .unwrap_err()
            .contains("no text"));
    }

    #[test]
    fn the_message_line_is_one_user_message_ending_in_a_newline() {
        let line = message_line("compare these");
        assert!(line.ends_with('\n'), "{line}");
        assert_eq!(line.lines().count(), 1, "{line}");
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["content"][0]["text"], "compare these");
    }

    /// The prompt is DATA and must not become argv: a candidate description
    /// containing a newline would otherwise split into a second message.
    #[test]
    fn a_prompt_with_newlines_stays_one_message() {
        let line = message_line("first\nsecond\n{\"injected\":true}");
        assert_eq!(line.lines().count(), 1, "{line}");
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(
            v["message"]["content"][0]["text"],
            "first\nsecond\n{\"injected\":true}"
        );
    }

    #[test]
    fn the_command_carries_the_read_only_posture_and_the_model() {
        let cmd = stream_command("claude", "claude-haiku-4-5");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        for expected in [
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "--verbose",
            "--strict-mcp-config",
            "--disable-slash-commands",
            "--no-session-persistence",
        ] {
            assert!(
                args.iter().any(|a| a == expected),
                "{expected} missing from {args:?}"
            );
        }
        assert_eq!(
            args.iter().filter(|a| *a == "stream-json").count(),
            2,
            "{args:?}"
        );
        let model_at = args
            .iter()
            .position(|a| a == "--model")
            .expect("model flag");
        assert_eq!(args[model_at + 1], "claude-haiku-4-5");
    }

    /// The whole path, with a stand-in CLI: prepare starts a process, take
    /// hands it over, and the message it answers is the one this call sent.
    ///
    /// The stand-in records the time it STARTED into a file. The assertion is
    /// that this time precedes the moment the prompt was sent, which is the
    /// property the feature exists for and the one a faster answer alone would
    /// not prove (a fast cold call looks identical).
    #[test]
    fn a_prepared_helper_starts_before_the_call_it_answers() {
        let _slot = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let started_file = dir.path().join("started-at");
        let script = dir.path().join("stand-in-cli");
        std::fs::write(
            &script,
            format!(
                // Counts stdin with `wc -c` rather than `$(cat)`: command
                // substitution strips the trailing newline, and the newline is
                // exactly what ends a stream-json message.
                "#!/bin/sh\n\
                 date +%s%N > {marker}.tmp && mv {marker}.tmp {marker}\n\
                 bytes=$(wc -c | tr -d ' ')\n\
                 printf '{{\"type\":\"result\",\"is_error\":false,\"result\":\"saw %s bytes\"}}\\n' \"$bytes\"\n",
                marker = started_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let cli = script.to_string_lossy().to_string();

        *READY.lock().unwrap() = None;
        prepare(cli.clone(), "stand-in-model".into());
        let deadline = Instant::now() + Duration::from_secs(10);
        while READY.lock().unwrap().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let child = take(&cli, "stand-in-model").expect("prepare must leave a helper waiting");

        // `prepare` stores the child as soon as it is SPAWNED, so the process
        // may not have reached its first line yet. The real CLI is the same:
        // starting early wins because its boot overlaps idle time, not because
        // it is ready the instant prepare returns. Wait for it to be up, which
        // is the state the assertion below is about.
        let up_by = Instant::now() + Duration::from_secs(10);
        while !started_file.exists() && Instant::now() < up_by {
            std::thread::sleep(Duration::from_millis(20));
        }
        let started_ns: u128 = std::fs::read_to_string(&started_file)
            .expect("the prepared helper must have started")
            .trim()
            .parse()
            .expect("the stand-in records nanoseconds");
        let sent_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        assert!(
            started_ns < sent_ns,
            "the helper must already be running when the prompt is sent"
        );

        let message = message_line("compare these two cards");
        let out = helper_io::exchange(
            child,
            message.as_bytes(),
            Duration::from_secs(20),
            1024 * 1024,
        )
        .unwrap();
        assert!(out.status.success(), "{out:?}");
        let answer = parse_result(&String::from_utf8_lossy(&out.stdout)).unwrap();
        assert_eq!(
            answer,
            format!("saw {} bytes", message.len()),
            "the helper answered a different message"
        );
        *READY.lock().unwrap() = None;
    }

    /// A ready helper that belongs to another model is never handed a prompt.
    #[test]
    fn a_ready_helper_for_another_model_is_not_reused() {
        let _slot = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "cat >/dev/null; printf answer"]);
        let child = helper_io::start(cmd).unwrap();
        *READY.lock().unwrap() = Some(Ready {
            cli: "claude".into(),
            model: "claude-haiku-4-5".into(),
            started_at: Instant::now(),
            child,
        });
        assert!(
            take("claude", "some-other-model").is_none(),
            "a different model must not be reused"
        );
        assert!(
            READY.lock().unwrap().is_none(),
            "the mismatched helper is dropped, not left behind"
        );
    }
}
