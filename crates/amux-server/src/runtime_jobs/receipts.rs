//! Receipts: outcomes a worker should know about that are not worth a turn.
//!
//! # Why this exists
//!
//! In the 20-lane GS-12 phase (2026-10-01 to 10-08) 1,115 of the 3,934
//! messages into lanes were the lane's own `amux land (detached) finished with
//! rc 0` receipts, about 28% of lane input. Each one woke an idle lane, so a
//! successful land cost a full turn that re-read the whole context to learn
//! that nothing needed doing.
//!
//! # What this does
//!
//! A receipt is one JSON line in `~/.amux/receipts/<worker>.jsonl`
//! (`{"ts":..,"from":"land","text":".."}`), written by whoever has the news
//! (the detached `amux land` runner on success). Nothing is sent. The next
//! time amux wakes the worker anyway (a schedule fire, a goal-keeper
//! continue), the pending receipts ride along at the top of that message and
//! are consumed. The worker can read them any time with `amux receipts`.
//! Failures still wake: only good news waits.

use std::path::{Path, PathBuf};

/// At most this many receipts ride on one wake; the rest wait for the next.
const MAX_ATTACHED: usize = 20;

pub fn path(home: &Path, name: &str) -> PathBuf {
    home.join("receipts").join(format!("{name}.jsonl"))
}

/// Pending receipt texts, oldest first, with how many lines they came from.
pub fn pending(home: &Path, name: &str) -> Vec<String> {
    let Ok(body) = std::fs::read_to_string(path(home, name)) else {
        return Vec::new();
    };
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| match serde_json::from_str::<serde_json::Value>(l) {
            Ok(v) => {
                let text = v["text"].as_str().unwrap_or("").trim();
                let from = v["from"].as_str().unwrap_or("");
                let when = v["ts"]
                    .as_f64()
                    .and_then(|t| chrono::DateTime::from_timestamp(t as i64, 0))
                    .map(|t| t.format("%H:%MZ").to_string())
                    .unwrap_or_default();
                let one: String = text.chars().take(400).collect();
                format!("{when} {from}: {one}").trim().to_string()
            }
            Err(_) => l.chars().take(400).collect(),
        })
        .collect()
}

/// Prefix `text` with up to [`MAX_ATTACHED`] pending receipts. Returns the
/// text to deliver and how many receipts it carries; pass that count to
/// [`consume`] once the delivery has landed or been queued.
pub fn attach(home: &Path, name: &str, text: &str) -> (String, usize) {
    let all = pending(home, name);
    if all.is_empty() {
        return (text.to_string(), 0);
    }
    let n = all.len().min(MAX_ATTACHED);
    let more = all.len() - n;
    let mut head = String::from("[amux receipts] Held so they would not wake you:\n");
    for r in &all[..n] {
        head.push_str("- ");
        head.push_str(r);
        head.push('\n');
    }
    if more > 0 {
        head.push_str(&format!("- and {more} more: run `amux receipts`\n"));
    }
    (format!("{head}\n{text}"), n)
}

/// Drop the first `n` receipts. The file is renamed away first, so a receipt
/// appended while this runs lands in a fresh file and is never lost; the
/// unconsumed remainder is appended back.
pub fn consume(home: &Path, name: &str, n: usize) {
    if n == 0 {
        return;
    }
    let p = path(home, name);
    let side = p.with_extension(format!("consuming.{}.{}", std::process::id(), crate::config::now_f64() as u64));
    if std::fs::rename(&p, &side).is_err() {
        return;
    }
    let body = std::fs::read_to_string(&side).unwrap_or_default();
    let rest: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).skip(n).collect();
    if !rest.is_empty() {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
            let _ = f.write_all(format!("{}\n", rest.join("\n")).as_bytes());
        }
    }
    let _ = std::fs::remove_file(&side);
    tracing::info!(target: "amux::receipts", session = %name, verdict = "receipts_delivered_on_wake",
        measured = true, n_considered = n + rest.len(), delivered = n, remaining = rest.len(),
        "receipts rode along on a wake instead of waking the worker themselves");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(home: &Path, name: &str, text: &str) {
        let p = path(home, name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p).unwrap();
        writeln!(f, "{}", serde_json::json!({"ts": 1792277685.0, "from": "land", "text": text})).unwrap();
    }

    #[test]
    fn receipts_ride_on_the_next_wake_and_are_consumed_once() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        assert_eq!(attach(h, "w", "keep going"), ("keep going".to_string(), 0));
        add(h, "w", "amux land (detached) finished with rc 0: landed abc123");
        add(h, "w", "amux land (detached) finished with rc 0: landed def456");
        let (text, n) = attach(h, "w", "keep going");
        assert_eq!(n, 2);
        assert!(text.starts_with("[amux receipts]") && text.contains("abc123") && text.contains("def456"), "{text}");
        assert!(text.ends_with("keep going"), "{text}");
        // A receipt that arrives between attach and consume is kept.
        add(h, "w", "landed ghi789");
        consume(h, "w", n);
        let left = pending(h, "w");
        assert_eq!(left.len(), 1, "{left:?}");
        assert!(left[0].contains("ghi789"));
        consume(h, "w", 1);
        assert!(pending(h, "w").is_empty());
    }

    #[test]
    fn a_long_backlog_is_capped_and_says_how_many_wait() {
        let home = tempfile::tempdir().unwrap();
        for i in 0..25 {
            add(home.path(), "w", &format!("landed {i}"));
        }
        let (text, n) = attach(home.path(), "w", "x");
        assert_eq!(n, MAX_ATTACHED);
        assert!(text.contains("and 5 more"), "{text}");
        consume(home.path(), "w", n);
        assert_eq!(pending(home.path(), "w").len(), 5);
    }
}
