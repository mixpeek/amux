//! The compaction-generation meter (AMUX-3742).
//!
//! Why this file exists rather than a unit test next to the code: the census
//! helper reads the real `~/.amux/sessions`, so on a machine with no lanes it
//! passes vacuously. These tests build the artifact themselves, so they can
//! fail.
//!
//! The one that matters is `counts_boundaries_not_their_debris`. The first
//! draft of the census that produced this feature counted THREE markers —
//! `subtype:"compact_boundary"`, `isCompactSummary`, and the presence of
//! `compactMetadata` — and reported exactly 2x the truth (median 17 where the
//! answer was 8), because a single compaction writes several records. That
//! wrong number was already written onto a board card before a hand count of
//! one transcript caught it.

use std::io::Write;

/// One real compaction, as Claude Code writes it: a `compact_boundary` system
/// record AND a separate summary record. Verbatim shape from a live transcript
/// (`~/.claude/projects/-Users-ethan-Dev-amux/*.jsonl`, 2026-08-26).
fn one_compaction(i: usize) -> String {
    format!(
        "{}\n{}\n",
        format_args!(
            r#"{{"type":"system","subtype":"compact_boundary","content":"Conversation compacted","compactMetadata":{{"trigger":"auto","preTokens":{}}},"uuid":"u{i}"}}"#,
            160_000 + i
        ),
        r#"{"type":"user","isCompactSummary":true,"message":{"role":"user","content":"summary"}}"#
    )
}

fn ordinary_turn(i: usize) -> String {
    format!(r#"{{"type":"user","message":{{"role":"user","content":"turn {i}"}}}}"#) + "\n"
}

#[test]
fn counts_boundaries_not_their_debris() {
    let dir = std::env::temp_dir().join(format!("amux-gen-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("a.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    for i in 0..3 {
        f.write_all(ordinary_turn(i).as_bytes()).unwrap();
        f.write_all(one_compaction(i).as_bytes()).unwrap();
    }
    f.flush().unwrap();
    drop(f);

    let got = amux_server::api::session_verbs::count_compact_boundaries(&path);
    assert_eq!(
        got,
        Some(3),
        "three compactions wrote six records; counting the debris too reports 6 \
         (the 2x over-count that shipped onto a card before a hand count caught it)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_appended_boundary_is_picked_up_by_the_incremental_pass() {
    // The cache exists because transcripts reach 648MB here. A cache that
    // never re-reads is a meter that freezes at whatever it saw first, which
    // would read as "this lane stopped degrading" — the failure mode this
    // whole feature is about.
    let dir = std::env::temp_dir().join(format!("amux-gen-inc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("b.jsonl");
    std::fs::write(&path, one_compaction(0)).unwrap();
    assert_eq!(amux_server::api::session_verbs::count_compact_boundaries(&path), Some(1));

    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(ordinary_turn(9).as_bytes()).unwrap();
    f.write_all(one_compaction(1).as_bytes()).unwrap();
    f.flush().unwrap();
    drop(f);
    assert_eq!(
        amux_server::api::session_verbs::count_compact_boundaries(&path),
        Some(2),
        "the second pass must scan the appended bytes, not return the cached count"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_partial_trailing_line_is_counted_once_it_completes() {
    // A transcript is being written while this reads it. A record caught
    // mid-write must not be counted twice (once as a fragment, once whole) and
    // must not be lost — the offset only advances to the last newline.
    let dir = std::env::temp_dir().join(format!("amux-gen-part-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("c.jsonl");
    let whole = one_compaction(0);
    let cut = whole.find('\n').unwrap() - 10;
    std::fs::write(&path, &whole[..cut]).unwrap();
    assert_eq!(
        amux_server::api::session_verbs::count_compact_boundaries(&path),
        Some(0),
        "an incomplete record is not a compaction yet"
    );

    std::fs::write(&path, &whole).unwrap();
    assert_eq!(
        amux_server::api::session_verbs::count_compact_boundaries(&path),
        Some(1),
        "and it is counted exactly once when the line completes"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_transcript_is_unmeasurable_not_zero() {
    // The distinction the whole payload is built around: "never compacted" and
    // "nobody could measure this" must not both arrive as a number.
    let missing = std::env::temp_dir().join("amux-gen-does-not-exist.jsonl");
    let _ = std::fs::remove_file(&missing);
    assert_eq!(amux_server::api::session_verbs::count_compact_boundaries(&missing), None);
}

#[test]
fn a_file_larger_than_one_read_chunk_is_scanned_to_the_end() {
    // THE BUG THIS PINS: the first draft did a single bounded read and returned
    // the partial count as the answer. On the 324MB transcript of the lane that
    // wrote this feature it reported 30 against a hand count of 75, and nothing
    // about a low number says "truncated".
    //
    // A 64MB fixture is not worth writing, so this drives the same loop through
    // the chunk seam with a tiny read size. That seam exists precisely so this
    // test can fail: any fixture small enough to write fits inside one default
    // 64MB read, so without it the test passes against the single-read bug it
    // exists to catch.
    let dir = std::env::temp_dir().join(format!("amux-gen-big-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("big.jsonl");
    let mut body = String::new();
    body.push_str(&one_compaction(0));
    for i in 0..400 {
        body.push_str(&ordinary_turn(i));
    }
    body.push_str(&one_compaction(1));
    for i in 0..400 {
        body.push_str(&ordinary_turn(i));
    }
    body.push_str(&one_compaction(2));
    std::fs::write(&path, &body).unwrap();

    // 4096 is the floor the implementation clamps to, and the fixture is many
    // times that — so reaching the last boundary REQUIRES more than one read.
    let chunk = 4096u64;
    assert!(
        body.len() as u64 > chunk * 3,
        "fixture must span several chunks or this test cannot fail (it is {} bytes)",
        body.len()
    );
    assert_eq!(
        amux_server::api::session_verbs::count_compact_boundaries_with_chunk(&path, chunk),
        Some(3),
        "every boundary must be found, including the one at the very end of the file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
