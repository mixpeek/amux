//! Readable scrollback comes from conversation records. A pipe-pane log is a
//! sequence of cursor updates, not lines of conversation (TubeScience incident).
use super::*;
use std::io::{Read, Seek, SeekFrom};

pub(super) struct Page {
    pub(super) text: String,
    before: u64,
    pub(super) records: usize,
    tool_output_arrays: usize,
}

// Reuse the provider's conversation projection (including wrapper removal and
// mirror dedup). Keep output records independent of their calls: a page can
// start between the two, and pairing only within that page would lose output.
fn readable_records(record: Value) -> Vec<Value> {
    use crate::opencode::events::{codex_rollout_transcript, TranscriptEvent};
    if record["type"] != "response_item" {
        return vec![record];
    }
    let payload = &record["payload"];
    if matches!(payload["type"].as_str(), Some("function_call_output" | "custom_tool_call_output")) {
        return vec![json!({"type":"user", "message":{"role":"user", "content":[
            {"type":"tool_result", "content":crate::opencode::events::output_text(payload)}
        ]}})];
    }
    codex_rollout_transcript(&[record]).into_iter().filter_map(|event| {
        let (role, block) = match event {
            TranscriptEvent::User { text } => ("user", json!({"type":"text", "text":text})),
            TranscriptEvent::Assistant { text } => ("assistant", json!({"type":"text", "text":text})),
            TranscriptEvent::Tool { tool, detail, .. } => ("assistant", json!({
                "type":"tool_use", "name":tool, "input":{"description":detail}
            })),
            TranscriptEvent::Plan { steps } => {
                let text = steps.into_iter().map(|step| format!("[{}] {}", step.status, step.step)).collect::<Vec<_>>().join("\n");
                ("assistant", json!({"type":"text", "text":text}))
            }
            // Like the Claude terminal renderer, display conversation and
            // tool activity, not internal reasoning records.
            TranscriptEvent::Reasoning { .. } => return None,
        };
        Some(json!({"type":role, "message":{"role":role, "content":[block]}}))
    }).collect()
}

fn conversation_path(name: &str, provider: &str) -> Option<PathBuf> {
    match provider {
        "codex" | "ollama" => codex_rollout_path(name),
        "claude" => session_jsonl_path(name),
        _ => None,
    }
}

pub(super) fn snapshot(name: &str, provider: &str, budget: usize) -> Result<Page, &'static str> {
    let result = conversation_path(name, provider).ok_or("conversation_not_resolved")
        .and_then(|path| read_page(&path, None, budget).map_err(|error| {
            tracing::warn!(session = name, provider, measured = false, n_considered = 0,
                verdict = "peek_history_read_failed", %error);
            "conversation_read_failed"
        }));
    match &result {
        Ok(page) => tracing::info!(session = name, provider, measured = true,
            n_considered = page.records, tool_output_arrays = page.tool_output_arrays,
            bytes = page.text.len(), verdict = "peek_history_loaded"),
        Err(why) => tracing::warn!(session = name, provider, measured = false,
            n_considered = 0, why_unmeasured = why, verdict = "peek_history_unavailable"),
    }
    result
}

// Absolute byte cursors remain stable while the worker appends. Include the
// record crossing the read boundary; otherwise a large tool result disappears
// between two pages. Never render a partial JSON record as terminal text.
fn read_page(path: &Path, before: Option<u64>, budget: usize) -> std::io::Result<Page> {
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let end = before.unwrap_or(size);
    if end > size {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "transcript cursor exceeds file size"));
    }
    let mut start = end.saturating_sub(5_000_000);
    while start > 0 {
        let probe = start.saturating_sub(8192);
        file.seek(SeekFrom::Start(probe))?;
        let mut bytes = vec![0; (start - probe) as usize];
        file.read_exact(&mut bytes)?;
        if let Some(nl) = bytes.iter().rposition(|b| *b == b'\n') {
            start = probe + nl as u64 + 1;
            break;
        }
        start = probe;
    }
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; (end - start) as usize];
    file.read_exact(&mut bytes)?;
    let mut offset = start;
    let mut records = Vec::new();
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        records.push((offset, line));
        offset += line.len() as u64;
    }
    let mut parts = Vec::new();
    let mut chars = 0;
    let mut cursor = end;
    let mut count = 0;
    let mut tool_output_arrays = 0;
    for (offset, line) in records.into_iter().rev() {
        cursor = offset;
        let Ok(record) = serde_json::from_slice::<Value>(line) else { continue };
        if record["type"] == "response_item"
            && matches!(record["payload"]["type"].as_str(), Some("function_call_output" | "custom_tool_call_output"))
            && record["payload"]["output"].is_array()
        {
            tool_output_arrays += 1;
        }
        let normalized = readable_records(record);
        let text = render_transcript_records(normalized.clone(), usize::MAX);
        count += 1;
        if !text.is_empty() {
            chars += text.chars().count();
            parts.push(normalized);
        }
        if chars >= budget { break; }
    }
    parts.reverse();
    Ok(Page { text: render_transcript_records(parts.into_iter().flatten().collect(), usize::MAX), before: cursor, records: count, tool_output_arrays })
}

pub(super) fn response(name: &str, qs: &[(String, String)]) -> Response {
    let provider = provider_of(&parse_env(name));
    if !matches!(provider.as_str(), "claude" | "codex" | "ollama") {
        if !qs_first(qs, "conversation", "").is_empty() {
            return jresp(StatusCode::CONFLICT, json!({"error": "worker provider changed; reopen its history"}));
        }
        let legacy: Vec<_> = qs.iter().filter(|(key, _)| key != "source").cloned().collect();
        return log_get(name, "", &legacy);
    }
    let Some(path) = conversation_path(name, &provider) else {
        tracing::warn!(session = name, provider, measured = false, n_considered = 0,
            verdict = "conversation_history_unavailable",
            "readable history unavailable; refusing to display terminal redraw fragments as conversation");
        return jresp(StatusCode::NOT_FOUND, json!({"error": "no saved conversation"}));
    };
    let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let requested_id = qs_first(qs, "conversation", "");
    if !requested_id.is_empty() && requested_id != id {
        return jresp(StatusCode::CONFLICT, json!({"error": "conversation changed; reopen the worker to load its history"}));
    }
    let cursor = qs_first(qs, "before", "");
    let before = if cursor.is_empty() { None } else {
        match cursor.parse::<u64>() {
            Ok(n) => Some(n),
            Err(_) => return jresp(StatusCode::BAD_REQUEST, json!({"error": "invalid history cursor"})),
        }
    };
    match read_page(&path, before, 192_000) {
        Ok(page) => {
            tracing::info!(session = name, provider, measured = true, n_considered = page.records,
                verdict = "conversation_history_page", source = "transcript",
                records = page.records, tool_output_arrays = page.tool_output_arrays,
                bytes = page.text.len(), remaining = page.before,
                "served readable conversation history without terminal redraw fragments");
            (StatusCode::OK, [
                ("content-type", "text/plain; charset=utf-8".to_string()),
                ("x-amux-session", name.to_string()),
                ("x-log-source", "conversation".to_string()),
                ("x-log-conversation", id.to_string()),
                ("x-log-remaining", page.before.to_string()),
            ], page.text).into_response()
        }
        Err(error) => {
            tracing::warn!(session = name, provider, measured = false, n_considered = 0,
                verdict = "conversation_history_read_failed", %error);
            jresp(StatusCode::CONFLICT, json!({"error": "could not read this conversation history page; reopen the worker"}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn record(text: &str) -> String {
        format!("{}\n", json!({"type":"assistant", "message":{"role":"assistant", "content":[{"type":"text", "text":text}]}}))
    }

    fn codex_record(kind: &str, payload: Value) -> String {
        format!("{}\n", json!({"type":kind,"payload":payload}))
    }

    #[test]
    fn codex_history_is_readable_without_the_tui_frame() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "{}{}{}{}{}", codex_record("response_item", json!({"type":"message","role":"user","content":[{"type":"input_text","text":"Find the missing logs"}]})),
            codex_record("response_item", json!({"type":"function_call","name":"exec_command","call_id":"call-one","arguments":"{\"cmd\":\"printf fixture\"}"})),
            codex_record("response_item", json!({"type":"function_call_output","call_id":"call-one","output":"fixture completed"})),
            codex_record("response_item", json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Earlier work remains visible."}]})),
            codex_record("event_msg", json!({"type":"agent_message","message":"Earlier work remains visible."}))).unwrap();
        let page = read_page(file.path(), None, 192_000).unwrap();
        assert!(page.text.contains("Find the missing logs"), "{}", page.text);
        assert!(page.text.contains("exec_command"));
        assert!(page.text.contains("fixture completed"));
        assert_eq!(page.text.matches("Earlier work remains visible.").count(), 1);
    }

    #[test]
    fn codex_output_at_page_boundary_is_not_dropped_or_duplicated() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "{}{}", codex_record("response_item", json!({"type":"function_call","name":"exec_command","call_id":"call-two","arguments":"{}"})),
            codex_record("response_item", json!({"type":"function_call_output","call_id":"call-two","output":"A complete tool result at the boundary"}))).unwrap();
        let last = read_page(file.path(), None, 1).unwrap();
        assert!(last.text.contains("A complete tool result at the boundary"));
        let before = read_page(file.path(), Some(last.before), 1).unwrap();
        assert!(before.text.contains("exec_command"));
        assert!(!before.text.contains("A complete tool result at the boundary"));
    }

    #[test]
    fn codex_history_removes_harness_metadata_and_internal_records() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for payload in [
            json!({"type":"reasoning", "summary":[{"text":"internal reasoning specimen"}]}),
            json!({"type":"message", "role":"developer", "content":[{"type":"input_text", "text":"harness instructions specimen"}]}),
            json!({"type":"custom_tool_call_output", "call_id":"custom-one", "output":{"output":"Chunk ID: abc\nWall time: 0.1\nProcess exited with code 0\nOutput:\nActual tool output"}}),
        ] {
            write!(file, "{}", codex_record("response_item", payload)).unwrap();
        }
        let page = read_page(file.path(), None, 192_000).unwrap();
        assert!(page.text.contains("Actual tool output"));
        for hidden in ["internal reasoning specimen", "harness instructions specimen", "Chunk ID:"] {
            assert!(!page.text.contains(hidden), "{hidden}");
        }
        assert_eq!(page.records, 3);
    }

    #[test]
    fn codex_native_array_tool_output_is_visible_without_image_payloads() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "{}", codex_record("response_item", json!({
            "type":"custom_tool_call_output", "call_id":"native-output", "output":[
                {"type":"input_text", "text":"The native tool completed successfully"},
                {"type":"input_image", "image_url":"data:image/png;base64,private-image-payload"},
                {"type":"text", "text":"A second result block"}
            ]
        }))).unwrap();
        let page = read_page(file.path(), None, 192_000).unwrap();
        assert!(page.text.contains("The native tool completed successfully"));
        assert!(page.text.contains("A second result block"));
        assert!(!page.text.contains("private-image-payload"));
        assert_eq!(page.tool_output_arrays, 1);
    }

    #[test]
    fn pages_preserve_complete_messages_and_cursor_survives_appends() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for s in ["First complete message", "Second complete message", "Third complete message"] {
            write!(file, "{}", record(s)).unwrap();
        }
        let last = read_page(file.path(), None, 1).unwrap();
        assert!(last.text.contains("Third complete message"));
        write!(file, "{}", record("New output while reading")).unwrap();
        let middle = read_page(file.path(), Some(last.before), 1).unwrap();
        assert!(middle.text.contains("Second complete message"));
        assert!(!middle.text.contains("Third") && !middle.text.contains("New output"));
        let first = read_page(file.path(), Some(middle.before), 1).unwrap();
        assert!(first.text.contains("First complete message"));
        assert_eq!(first.before, 0);
        assert!(read_page(file.path(), Some(0), 1).unwrap().text.is_empty());
    }

    #[test]
    fn large_records_at_page_boundary_are_not_lost() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "{}{}", record("Earlier message"), record(&format!("Large {} intact", "x".repeat(5_010_000)))).unwrap();
        let page = read_page(file.path(), None, 192_000).unwrap();
        assert!(page.text.contains("Large ") && page.text.contains(" intact"));
        let previous = read_page(file.path(), Some(page.before), 192_000).unwrap();
        assert!(previous.text.contains("Earlier message"));
        assert_eq!(previous.before, 0);
    }

    #[test]
    fn consumed_mid_turn_messages_render_once_without_queue_bookkeeping() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let prompt = "[amux-origin: mvs-infra]\n\nHousekeeping: the roll is complete. Continue validation.";
        for operation in ["enqueue", "dequeue"] {
            writeln!(file, "{}", json!({"type":"queue-operation", "operation":operation, "content":prompt})).unwrap();
        }
        writeln!(file, "{}", json!({"type":"attachment", "attachment":{
            "type":"queued_command", "prompt":prompt, "commandMode":"prompt"
        }, "rendered":[{"content":"<system-reminder>duplicate wrapper</system-reminder>"}]})).unwrap();
        write!(file, "{}", record("Validation continued.")).unwrap();
        let page = read_page(file.path(), None, 192_000).unwrap();
        assert_eq!(page.text.matches("Housekeeping: the roll is complete.").count(), 1);
        assert!(page.text.contains("[amux-origin: mvs-infra]"));
        assert!(page.text.contains("Validation continued."));
        assert!(!page.text.contains("duplicate wrapper"));
        assert_eq!(page.before, 0);
    }

    #[test]
    fn malformed_tail_and_spinner_metadata_are_not_displayed() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "{}{}\n{{\"partial\":", record("Readable TubeScience response ✓"),
            json!({"type":"progress", "data":{"text":"* d i\n+ e n 4"}})).unwrap();
        let page = read_page(file.path(), None, 192_000).unwrap();
        assert!(page.text.contains("Readable TubeScience response ✓"));
        assert!(!page.text.contains("* d i") && !page.text.contains("partial"));
        assert_eq!(page.before, 0);
        assert!(read_page(file.path(), Some(u64::MAX), 1).is_err());
    }
}
