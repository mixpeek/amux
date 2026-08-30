//! The Execution Checklist in `docs/rust-rebuild-plan.md` must not contradict itself
//! (AMUX-2583).
//!
//! That doc calls itself "the authoritative system of record for the Rust rebuild"
//! and declares in its own Notation section which fields carry state: `Status:` and
//! `Evidence:`. On 2026-08-24 fifty-seven items disagreed with it. Each had a TICKED
//! BOX and an HTML comment on the title line reading `<!-- verified 2026-08-09: ... -->`
//! naming real files and real tests, while the two declared fields still read
//! `Status: TODO` with no Evidence at all.
//!
//! Every one of those items was genuinely implemented. The evidence was real, it was
//! specific, and 56 of 57 file references still resolved fifteen days later. It was
//! simply written where the doc does not look — and an HTML comment renders as
//! NOTHING, so a reader of the rendered checklist saw 163 TODOs, of which 57 were
//! finished work. A plan doc that misreports its own state is worse than an absent
//! one, because it is trusted.
//!
//! WHY A COMMENT AND NOT THE FIELD. Ticking a box and appending a comment is a
//! one-line edit at the top of the block; setting Status and adding Evidence is a
//! two-line edit inside it. The cheap path and the declared path were different
//! paths, so the cheap one won fifty-seven times. That is ethos rule 6's shape (the
//! sanctioned route being the awkward one), and the fix is not a rule asking people
//! to remember — it is making the cheap path fail.
//!
//! So `evidence_is_not_hidden_in_an_html_comment` is the load-bearing cell here. The
//! other two clean up after a drift; that one stops it being written.
//!
//! CONTROL: run against the pre-fix doc, all three cells fail — 57 contradictions,
//! 57 missing-evidence, 57 comments. They are not vacuous.

use std::collections::BTreeMap;

fn doc() -> String {
    // AMUX_PLAN_DOC exists so the control can run these same cells against the
    // PRE-FIX doc out of git, rather than reimplementing the parser in a throwaway
    // script — a copy of a check is not the check, and it would pass forever while
    // the real one rotted. Unset in every normal run.
    let p = std::env::var("AMUX_PLAN_DOC").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/rust-rebuild-plan.md").to_string()
    });
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {p}: {e}"))
}

/// One checklist item, as the doc's own Notation section defines it.
struct Item {
    id: String,
    line: usize,
    checked: bool,
    status: Option<String>,
    evidence: Option<String>,
    /// Text of an HTML comment on the header line, if any.
    header_comment: Option<String>,
}

/// Parse `- [x] RR-0029 — title <!-- ... -->` blocks and the indented fields under
/// each. A field belongs to the item whose header precedes it, which is why the
/// parser resets on every header rather than scanning the file for `Status:`.
fn items(src: &str) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::new();
    for (n, raw) in src.lines().enumerate() {
        let line = n + 1;
        if let Some(rest) = raw.strip_prefix("- [") {
            let (checked, rest) = match rest.split_once("] ") {
                Some(("x", r)) => (true, r),
                Some((" ", r)) => (false, r),
                _ => continue,
            };
            let Some(id) = rest.split_whitespace().next() else { continue };
            if !id.starts_with("RR-") {
                continue;
            }
            let header_comment = rest
                .split_once("<!--")
                .and_then(|(_, c)| c.split_once("-->"))
                .map(|(c, _)| c.trim().to_string());
            out.push(Item {
                id: id.to_string(),
                line,
                checked,
                status: None,
                evidence: None,
                header_comment,
            });
            continue;
        }
        let Some(cur) = out.last_mut() else { continue };
        let t = raw.trim_start();
        if raw.starts_with(char::is_whitespace) {
            if let Some(v) = t.strip_prefix("Status:") {
                if cur.status.is_none() {
                    cur.status = Some(v.trim().to_string());
                }
            } else if let Some(v) = t.strip_prefix("Evidence:") {
                if cur.evidence.is_none() && !v.trim().is_empty() {
                    cur.evidence = Some(v.trim().to_string());
                }
            }
        }
    }
    out
}

/// A ticked box and `Status: TODO` are a direct contradiction: the box says done,
/// the field the doc declares authoritative says not started.
#[test]
fn a_ticked_box_does_not_claim_status_todo() {
    let src = doc();
    let bad: Vec<_> = items(&src)
        .into_iter()
        .filter(|i| i.checked && i.status.as_deref() == Some("TODO"))
        .map(|i| format!("  {}:{} — [x] but Status: TODO", i.line, i.id))
        .collect();
    assert!(
        bad.is_empty(),
        "{} checklist item(s) contradict themselves — the box is ticked but the field the \
         doc declares authoritative reads TODO. Set Status to what is true \
         (IMPLEMENTED = code exists but NOT verified; VERIFIED = all applicable layers pass \
         with evidence), or untick the box:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// The Notation section defines Evidence as "what was produced to justify VERIFIED".
/// A completed status with nothing behind it is the claim without the receipt.
#[test]
fn a_completed_item_carries_its_evidence() {
    let src = doc();
    let done = ["IMPLEMENTED", "VERIFYING", "VERIFIED"];
    let bad: Vec<_> = items(&src)
        .into_iter()
        .filter(|i| {
            i.status.as_deref().is_some_and(|s| done.contains(&s)) && i.evidence.is_none()
        })
        .map(|i| {
            format!("  {}:{} — Status: {} with no Evidence", i.line, i.id, i.status.unwrap())
        })
        .collect();
    assert!(
        bad.is_empty(),
        "{} item(s) claim a completed status with no Evidence line. Name what was produced \
         (files, tests, commits) so the claim can be checked:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// THE ONE THAT STOPS THE DRIFT RECURRING.
///
/// An HTML comment renders as nothing. Evidence written there is invisible in the
/// rendered doc, invisible to the two cells above, and invisible to anyone reading
/// the checklist to decide what is left — which is how 57 finished items read as
/// TODO for fifteen days. The content was never the problem; the location was.
#[test]
fn evidence_is_not_hidden_in_an_html_comment() {
    let src = doc();
    let bad: Vec<_> = items(&src)
        .into_iter()
        .filter_map(|i| {
            let c = i.header_comment?;
            // Only flag comments that are carrying STATE. A comment that annotates
            // the title for some other reason is not what bit us, and flagging every
            // comment would make this cell fail for reasons it cannot explain.
            let carries_state = ["verified", "implemented", "done", "evidence", "tests"]
                .iter()
                .any(|k| c.to_ascii_lowercase().contains(k));
            carries_state.then(|| {
                format!("  {}:{} — <!-- {} -->", i.line, i.id, c.chars().take(70).collect::<String>())
            })
        })
        .collect();
    assert!(
        bad.is_empty(),
        "{} item(s) record verification state in an HTML COMMENT on the header line. A \
         comment renders as nothing, so this evidence is invisible to every reader of the \
         checklist and to the consistency checks above — the exact failure of AMUX-2583, \
         where 57 finished items read as TODO for 15 days. Move it into the `Evidence:` \
         field and set `Status:` accordingly:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// The parser has to actually find items, or all three cells above pass vacuously
/// against an empty list. This is the control: if the doc's format changes and the
/// parser stops matching, THIS fails loudly instead of the suite going quietly green.
#[test]
fn the_parser_still_understands_the_checklist_format() {
    let src = doc();
    let all = items(&src);
    assert!(
        all.len() > 150,
        "parsed only {} checklist items — the doc's format changed and the checks above are \
         now blind. Fix the parser before trusting a green run.",
        all.len()
    );
    let with_status = all.iter().filter(|i| i.status.is_some()).count();
    assert!(
        with_status > 150,
        "only {} of {} items had a Status field — the field parser is not matching, so \
         a_ticked_box_does_not_claim_status_todo cannot fail",
        with_status,
        all.len()
    );
    let mut by_status: BTreeMap<&str, usize> = BTreeMap::new();
    for i in &all {
        *by_status.entry(i.status.as_deref().unwrap_or("(none)")).or_default() += 1;
    }
    // Both cells above need at least one item in a completed state to be meaningful.
    assert!(
        by_status.keys().any(|s| ["IMPLEMENTED", "VERIFYING", "VERIFIED"].contains(s)),
        "no item is in a completed state, so a_completed_item_carries_its_evidence cannot \
         fail: {by_status:?}"
    );
}
