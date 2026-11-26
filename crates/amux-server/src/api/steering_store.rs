//! The one place a row is inserted into `steering_queue`.
//!
//! Every queued delivery to a worker is a `steering_queue` row, and the gates
//! that decide whether it may be queued at all (an isolated lane, an archived
//! one, the peer and group boundary, the per-card precondition, dedupe by
//! guard or stable id) run in `session_verbs::steer_enqueue_precond_with_id`,
//! which the code calls the chokepoint. A raw `INSERT` anywhere else would
//! queue input none of those gates saw (primitive integrity audit,
//! 2026-11-26, P0 "two delivery queues"). So the statement lives here, the
//! chokepoint is its only caller, and `tests::the_chokepoint_is_the_only_steering_queue_writer`
//! fails, naming the file and line, on an insert anywhere else in this
//! crate's non-test code.
//!
//! Test fixtures that seed a queue state (a claimed row, a delivered one, a
//! stale `delivering_since`) still write rows directly: they set columns no
//! real enqueue sets, and they run against throwaway databases.

/// One new queued delivery, as the chokepoint built it.
pub(super) struct NewSteer<'a> {
    pub id: &'a str,
    pub session: &'a str,
    pub text: &'a str,
    pub queued_at: f64,
    /// Empty means unguarded (stored NULL).
    pub guard: &'a str,
    pub sender: &'a str,
    /// (card id, card revision) the delivery is conditional on.
    pub precond: Option<(&'a str, i64)>,
}

/// Insert one queued delivery. Only `steer_enqueue_precond_with_id` calls this.
pub(super) fn insert(conn: &rusqlite::Connection, row: &NewSteer<'_>) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO steering_queue\
         (id, session, text, queued_at, guard, sender, precond_card, precond_rev) \
         VALUES(?,?,?,?,?,?,?,?)",
        rusqlite::params![
            row.id,
            row.session,
            row.text,
            row.queued_at,
            if row.guard.is_empty() { None } else { Some(row.guard) },
            row.sender,
            row.precond.map(|(c, _)| c),
            row.precond.map(|(_, r)| r),
        ],
    )
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    const OWNER: &str = "src/api/steering_store.rs";
    const CHOKEPOINT_FILE: &str = "src/api/session_verbs.rs";

    /// Lines of `src` that are test-only: an item under `#[cfg(test)]` (a
    /// module or a function), from its attribute to the closing brace at the
    /// item's own indentation.
    fn test_only_lines(src: &str) -> Vec<bool> {
        let lines: Vec<&str> = src.lines().collect();
        let mut skip = vec![false; lines.len()];
        let mut i = 0;
        while i < lines.len() {
            if lines[i].trim() == "#[cfg(test)]" {
                let start = i;
                let mut j = i + 1;
                while j < lines.len() && lines[j].trim_start().starts_with("#[") {
                    j += 1;
                }
                if j >= lines.len() {
                    break;
                }
                let indent = lines[j].len() - lines[j].trim_start().len();
                let mut end = j;
                if lines[j].trim_end().ends_with('{') {
                    let close = format!("{}}}", " ".repeat(indent));
                    end = (j + 1..lines.len()).find(|&k| lines[k].trim_end() == close).unwrap_or(lines.len() - 1);
                }
                for s in skip.iter_mut().take(end + 1).skip(start) {
                    *s = true;
                }
                i = end + 1;
                continue;
            }
            i += 1;
        }
        skip
    }

    /// `INSERT [OR x] INTO steering_queue`, also split across a `\`
    /// continuation, in the non-test lines of one file, as 1-based line numbers.
    fn inserts(src: &str) -> Vec<usize> {
        let skip = test_only_lines(src);
        let lines: Vec<&str> = src.lines().collect();
        let mut out = Vec::new();
        for (n, line) in lines.iter().enumerate() {
            // Test code, and comments: a comment naming the statement writes nothing.
            if skip[n] || line.trim_start().starts_with("//") {
                continue;
            }
            // Join with the next line so `"INSERT INTO\` + `steering_queue"` is seen.
            let next = lines.get(n + 1).filter(|_| !skip.get(n + 1).copied().unwrap_or(true)).copied().unwrap_or("");
            let joined = format!("{} {}", line.trim_end().trim_end_matches('\\'), next.trim_start()).to_ascii_uppercase();
            let here = line.to_ascii_uppercase();
            let starts_here = here.contains("INSERT") || here.contains("REPLACE");
            let hit = |s: &str| {
                let mut rest = s;
                while let Some(i) = rest.find("INTO") {
                    let before = rest[..i].trim_end();
                    let after = rest[i + 4..].trim_start().trim_start_matches(['"', '`']);
                    let verb = before.ends_with("INSERT") || before.contains("INSERT OR") || before.ends_with("REPLACE");
                    if verb && after.starts_with("STEERING_QUEUE") {
                        return true;
                    }
                    rest = &rest[i + 4..];
                }
                false
            };
            if starts_here && hit(&joined) && !out.contains(&(n + 1)) {
                out.push(n + 1);
            }
        }
        out
    }

    fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                rs_files(&p, out);
            } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
                out.push(p);
            }
        }
    }

    #[test]
    fn the_scanner_sees_inserts_and_skips_test_code() {
        let src = "fn a() {\n    c.execute(\"INSERT OR IGNORE INTO steering_queue(id) VALUES(1)\");\n}\n\
                   fn b() {\n    c.execute(\"INSERT INTO\\\n     steering_queue(id) VALUES(1)\");\n}\n\
                   fn c() { q(\"SELECT * FROM steering_queue\"); q(\"INSERT INTO steering_history\"); }\n\
                   #[cfg(test)]\nmod tests {\n    fn seed() { c.execute(\"INSERT INTO steering_queue VALUES(1)\"); }\n}\n\
                   fn d() { c.execute(\"REPLACE INTO steering_queue(id) VALUES(1)\"); }\n\
                   // an INSERT INTO steering_queue named in a comment writes nothing\n";
        assert_eq!(inserts(src), vec![2, 5, 13]);
    }

    /// The audit's remedy, as a test: no code outside this module inserts into
    /// steering_queue, this module has exactly one insert, and its only caller
    /// is the chokepoint.
    #[test]
    fn the_chokepoint_is_the_only_steering_queue_writer() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        rs_files(&root.join("src"), &mut files);
        files.sort();
        assert!(files.len() > 100, "premise: the crate's sources were found ({} files)", files.len());
        let mut outside = Vec::new();
        let mut owned = 0usize;
        let mut callers = Vec::new();
        for f in &files {
            let rel = f.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            let src = std::fs::read_to_string(f).unwrap();
            let hits = inserts(&src);
            if rel == OWNER {
                owned = hits.len();
                continue;
            }
            outside.extend(hits.iter().map(|n| format!("{rel}:{n}")));
            let skip = test_only_lines(&src);
            for (n, line) in src.lines().enumerate() {
                if !skip[n] && line.contains("steering_store::insert(") {
                    callers.push(format!("{rel}:{}", n + 1));
                }
            }
        }
        assert!(outside.is_empty(),
            "steering_queue rows are inserted outside {OWNER}: {outside:?}. Queue through \
             session_verbs::steer_enqueue* (the chokepoint) so its gates run");
        assert_eq!(owned, 1, "{OWNER} holds the one insert statement");
        assert_eq!(callers.len(), 1, "one caller of steering_store::insert: {callers:?}");
        assert!(callers[0].starts_with(CHOKEPOINT_FILE), "the caller is the chokepoint: {callers:?}");
    }
}
