//! `amux board --force` must put the reason in the field the LEDGER reads.
//!
//! AMUX-3464, 2026-08-26. The live board had written 41 force audit lines and
//! all 41 read `reason=` with nothing after the `=`. The obvious explanations
//! were both wrong. Operators were not withholding the judgment: `--force`
//! has always refused to run without one ("--force requires a reason"). And
//! the server was not dropping it: `force_bypasses_the_gate_and_leaves_the
//! _audit_line` has always asserted a supplied reason reaches the log.
//!
//! The CLI collected the reason, validated it, and then sent it as
//! `desc_append` — prose on the card — while never populating the `reason`
//! key the server interpolates into the permanent audit line. Nine of those
//! 41 cards carry a perfectly good "[FORCED] <why>" in their desc next to a
//! ledger line that says nothing. Two components disagreeing about where the
//! same fact lives, with each half looking correct in isolation, which is why
//! it survived a test on either side of the seam.
//!
//! So this guard sits ON the seam. It executes the CLI's OWN body builder —
//! the python snippet lifted out of `amux` verbatim, not a paraphrase of what
//! it is believed to do (ethos rule 7: simulating what you think a function
//! does cannot catch it doing something else) — and asserts the JSON that
//! actually goes on the wire carries both halves.

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root above crates/amux-server")
        .to_path_buf()
}

/// Lift the transition body builder out of `amux` by its first and last lines.
///
/// Anchored on code, never on a byte offset: a positional window sized against
/// the prose that happens to precede it is one comment away from reading the
/// wrong lines, and this file's own history has that failure in it.
fn body_builder_snippet(cli: &str) -> String {
    let start = cli
        .find(r#"b = {"status": os.environ["S"]}"#)
        .expect("the transition body builder moved or was renamed — find it and re-anchor this test");
    let rest = &cli[start..];
    let end = rest
        .find("print(json.dumps(b))")
        .expect("body builder has no print(json.dumps(b)) terminator");
    format!("{}print(json.dumps(b))", &rest[..end])
}

fn build_body(force_reason: Option<&str>) -> serde_json::Value {
    let root = repo_root();
    let cli = std::fs::read_to_string(root.join("amux")).expect("read ./amux");
    let snippet = format!("import json, os, sys, time\n{}", body_builder_snippet(&cli));

    let out = Command::new("python3")
        .arg("-c")
        .arg(&snippet)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("S", "discarded")
        .env("F", force_reason.unwrap_or(""))
        .output()
        .expect("run the CLI's own body builder");
    assert!(
        out.status.success(),
        "body builder failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("body builder emitted valid JSON")
}

#[test]
fn force_puts_the_reason_where_the_audit_line_reads_it() {
    let body = build_body(Some("gate does not fit this card"));

    assert_eq!(body["force"], serde_json::json!(true), "{body}");
    assert_eq!(
        body["reason"],
        serde_json::json!("gate does not fit this card"),
        "the CLI must send `reason` — the server interpolates THAT key into the \
         card's permanent `force by <who>: a->b reason=<...>` line, and for 41 \
         forces it received nothing: {body}"
    );
    // Both, deliberately. The desc note is the story a human reads on the card;
    // the ledger line is the audit. Asserting only one lets the other regress,
    // which is the exact shape of the bug: one half present, the other empty,
    // and nothing in either component able to notice.
    assert_eq!(
        body["desc_append"],
        serde_json::json!("[FORCED] gate does not fit this card"),
        "{body}"
    );
}

#[test]
fn no_force_means_no_reason_and_no_forced_note() {
    // NEGATIVE CONTROL. Without it, a builder that unconditionally stamped
    // `reason` onto every transition would satisfy the test above — and that
    // is worse than the bug, because it would put an unforced move's status
    // text into the bypass ledger.
    let body = build_body(None);
    assert!(body.get("force").is_none(), "{body}");
    assert!(body.get("reason").is_none(), "{body}");
    assert!(body.get("desc_append").is_none(), "{body}");
    assert_eq!(body["status"], serde_json::json!("discarded"), "{body}");
}
