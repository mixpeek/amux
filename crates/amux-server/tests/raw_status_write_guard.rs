//! Every card status change goes through `db::advance`.
//!
//! `db/advance.rs` says "No direct SQL UPDATE on `issues.status` is permitted
//! outside this module", and nothing held it. The implicit state machines
//! inventory (2026-10-31, docs/design/implicit-state-machines.md #6) found five
//! raw `UPDATE issues SET status=...` writes in background jobs: they skipped
//! the lease release, the attempt record and the status-change event, so a
//! `doing` card closed that way kept its lease. They now call
//! `db::advance::harness_close`. This fails the build on a new one.

use std::path::Path;

/// Files allowed to write `issues.status` directly, each with its reason.
const ALLOW: &[(&str, usize, &str)] = &[
    ("db/advance.rs", usize::MAX, "the transition engine itself"),
    ("db/board_store.rs", 1, "save_patched, the whole-row storage write the transition engine calls"),
    (
        "api/session_verbs.rs",
        2,
        "lane archive / unarchive: a set-based move of every card of the lane into and out of \
         pre_archive_status, recorded on the lane, not a per-card transition",
    ),
];

/// Source with every `#[cfg(test)]` item removed (brace-matched).
fn without_test_items(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(at) = rest.find("#[cfg(test)]") {
        out.push_str(&rest[..at]);
        let after = &rest[at..];
        let Some(open) = after.find('{') else {
            rest = "";
            break;
        };
        // An item without a body (`#[cfg(test)] use x;`) ends at `;` first.
        if let Some(semi) = after.find(';') {
            if semi < open {
                rest = &after[semi + 1..];
                continue;
            }
        }
        let end = open + matching_close(&after[open..]).map(|i| i + 1).unwrap_or(after.len() - open);
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// Index of the `}` closing the `{` at `body[0]`, skipping braces inside
/// string, raw string and char literals and comments (a test module full of
/// JSON and format strings otherwise ends early and leaks into the scan).
fn matching_close(body: &str) -> Option<usize> {
    let b = body.as_bytes();
    let (mut i, mut depth) = (0usize, 0i64);
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            b'r' if matches!(b.get(i + 1), Some(b'"') | Some(b'#')) && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) => {
                let mut j = i + 1;
                let mut hashes = 0;
                while b.get(j) == Some(&b'#') {
                    hashes += 1;
                    j += 1;
                }
                if b.get(j) == Some(&b'"') {
                    let close: Vec<u8> = std::iter::once(b'"').chain(std::iter::repeat_n(b'#', hashes)).collect();
                    j += 1;
                    while j + close.len() <= b.len() && b[j..j + close.len()] != close[..] {
                        j += 1;
                    }
                    i = j + close.len() - 1;
                }
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'\'' => {
                // A char literal ('x', '\n', '\''); a lifetime ('a) is left alone.
                if b.get(i + 1) == Some(&b'\\') {
                    i += 2;
                    while i < b.len() && b[i] != b'\'' {
                        i += 1;
                    }
                } else if b.get(i + 2) == Some(&b'\'') {
                    i += 2;
                }
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Does an `UPDATE issues SET <assignments> WHERE` assign `status`?
fn raw_status_writes(src: &str) -> usize {
    let lower = src.to_ascii_lowercase();
    let mut n = 0;
    let mut from = 0;
    while let Some(i) = lower[from..].find("update issues set") {
        let start = from + i + "update issues set".len();
        let tail = &lower[start..];
        let stop = tail.find("where").unwrap_or(tail.len()).min(600);
        let sets = &tail[..stop];
        let assigns_status = sets.match_indices("status").any(|(j, _)| {
            let before = sets[..j].chars().next_back();
            let after = sets[j + "status".len()..].trim_start();
            !matches!(before, Some(c) if c.is_ascii_alphanumeric() || c == '_') && after.starts_with('=')
        });
        if assigns_status {
            n += 1;
        }
        from = start;
    }
    n
}

#[test]
fn card_status_is_written_only_by_the_transition_engine() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    let mut files = 0usize;
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if p.file_name().and_then(|n| n.to_str()) != Some("tests") {
                    stack.push(p);
                }
                continue;
            }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.ends_with(".rs") || name == "tests.rs" || name.ends_with("_tests.rs") {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&p) else { continue };
            files += 1;
            let rel = p.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
            let n = raw_status_writes(&without_test_items(&src));
            let allowed = ALLOW.iter().find(|(f, _, _)| *f == rel).map(|(_, n, _)| *n).unwrap_or(0);
            if n > allowed {
                offenders.push(format!("{rel}: {n} raw issues.status write(s), {allowed} allowed"));
            }
        }
    }
    assert!(files > 100, "the walk read only {files} files; it is not looking at the server source");
    assert!(
        offenders.is_empty(),
        "write card status through db::advance::advance (or harness_close for a harness-owned card), \
         not raw SQL:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_scanner_sees_a_raw_write_and_ignores_test_items_and_lookalikes() {
    assert_eq!(raw_status_writes(r#""UPDATE issues SET status='done' WHERE id=?1""#), 1);
    assert_eq!(raw_status_writes(r#""UPDATE issues SET \"desc\" = ?1, status = 'done', updated=?2 WHERE id=?3""#), 1);
    assert_eq!(raw_status_writes(r#""UPDATE issues SET pre_archive_status = NULL WHERE id=?1""#), 0);
    assert_eq!(raw_status_writes(r#""UPDATE issues SET lease_expires_at=?1 WHERE status='doing'""#), 0);
    let src = "fn a() {}\n#[cfg(test)]\nmod tests { fn t() { let _ = \"UPDATE issues SET status='x' WHERE id='T'\"; } }\nfn b() {}";
    assert_eq!(raw_status_writes(&without_test_items(src)), 0);
    assert!(without_test_items(src).contains("fn b()"));
    // Braces inside literals and comments do not end the test module early.
    let tricky = "#[cfg(test)]\nmod t { const A: &str = \"}\"; const C: char = '}'; // }\n fn x() { let _ = r#\"}\"#; \"UPDATE issues SET status='x' WHERE id='T'\"; } }\nfn after() {}";
    let kept = without_test_items(tricky);
    assert_eq!(raw_status_writes(&kept), 0, "{kept}");
    assert!(kept.contains("fn after()"), "{kept}");
}
