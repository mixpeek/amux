//! Readable scrollback comes from conversation records. A pipe-pane log is a
//! sequence of cursor updates, not lines of conversation (TubeScience incident).
use super::*;
use std::io::{Read, Seek, SeekFrom};

struct Page {
    text: String,
    before: u64,
    records: usize,
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
    for (offset, line) in records.into_iter().rev() {
        cursor = offset;
        let Ok(record) = serde_json::from_slice::<Value>(line) else { continue };
        let text = render_transcript_records(vec![record.clone()], usize::MAX);
        count += 1;
        if !text.is_empty() {
            chars += text.chars().count();
            parts.push(record);
        }
        if chars >= budget { break; }
    }
    parts.reverse();
    Ok(Page { text: render_transcript_records(parts, usize::MAX), before: cursor, records: count })
}

pub(super) fn response(name: &str, qs: &[(String, String)]) -> Response {
    if provider_of(&parse_env(name)) != "claude" {
        if !qs_first(qs, "conversation", "").is_empty() {
            return jresp(StatusCode::CONFLICT, json!({"error": "worker provider changed; reopen its history"}));
        }
        let legacy: Vec<_> = qs.iter().filter(|(key, _)| key != "source").cloned().collect();
        return log_get(name, "", &legacy);
    }
    let Some(path) = session_jsonl_path(name) else {
        tracing::warn!(session = name, verdict = "conversation_history_unavailable",
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
            tracing::info!(session = name, verdict = "conversation_history_page", source = "transcript",
                records = page.records, bytes = page.text.len(), remaining = page.before,
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
            tracing::warn!(session = name, verdict = "conversation_history_read_failed", %error);
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
