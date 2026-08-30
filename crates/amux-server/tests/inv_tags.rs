//! `INV-xxx` tags must name a real invariant (AMUX-3598, direction 2).
//!
//! `docs/rust-rebuild-plan.md` § Doc enforceability claims, in the present
//! tense, that every invariant's semantic ID is "Tagged in code", "Tagged in
//! tests", and "CI-enforced bidirectionally":
//!
//!   1. no invariant in the doc without at least one test tagged with its ID
//!   2. no `INV-xxx` tag in code/tests without a matching invariant in the doc
//!
//! Neither direction existed and there was not one tag anywhere outside the doc.
//! That is ethos rule 6 at its most expensive: an enforcement mechanism that is
//! claimed and not implemented is worse than an absent one, because a reader of
//! that section reasonably concludes the 54 invariants are covered.
//!
//! # What this file does, and what it deliberately does NOT
//!
//! It implements direction 2 only. Direction 1 — "every invariant has a tagged
//! test" — cannot be implemented honestly today, and shipping it would be the
//! exact failure this repo keeps recording: against zero tags it would pass by
//! comparing an empty set with an empty set, and it would go green on a
//! codebase with no coverage at all. The card that filed this said so in
//! advance and it is right.
//!
//! # Why nothing is tagged here either, which is the finding rather than a
//! shortcut
//!
//! `rust-rebuild-plan.md` is a REDESIGN document. Its invariants describe a
//! system that does not exist: invariant 7 ("Done != Verified") is specified as
//! a `Verification { verifier, criteria, evidence, result }` record, and today's
//! board has a status string with gates. Tagging a current test with invariant
//! 7's id would claim that a test proves an invariant of an unbuilt design —
//! manufacturing exactly the false coverage this card is about, while appearing
//! to fix it.
//!
//! # No id is spelled literally in this file, on purpose
//!
//! The walk below scans every source file, and this one is a source file. A real
//! id written here — even in a comment, even as a control fixture — is
//! indistinguishable from a tag, so it would count toward the coverage number
//! this test PRINTS and make the guard's own prose inflate its own report. The
//! first run caught exactly that. Ids are therefore assembled at runtime by
//! `id()`, which costs nothing and removes the need for a self-exclusion — and a
//! self-exclusion would have been a hole: a genuine tag added here later would
//! be silently unchecked.
//!
//! So the doc has been corrected to say what is true, and this guard makes the
//! FIRST real tag meaningful: the moment anyone writes one, a typo or an
//! invented ID fails the build instead of quietly claiming an invariant that
//! does not exist.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Every `INV-...` id the doc's mapping table defines.
fn ids_in(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let b = text.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = text[from..].find("INV-") {
        let start = from + rel;
        let mut i = start + 4;
        while i < b.len() && (b[i].is_ascii_uppercase() || b[i].is_ascii_digit() || b[i] == b'-') {
            i += 1;
        }
        // Trailing '-' is punctuation, not part of the id.
        let end = if i > start + 4 && b[i - 1] == b'-' { i - 1 } else { i };
        if end > start + 4 {
            out.insert(text[start..end].to_string());
        }
        from = start + 4;
    }
    out
}

/// Source files a tag could live in. Excludes `docs/`, which is the DEFINITION
/// side: counting the doc's own ids as tags would make direction 2 pass by
/// comparing the doc with itself.
fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.join("crates"), root.join("scripts"), root.join("e2e")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                // Build output and vendored deps are not ours to police, and
                // walking them turns a 0.1s test into a minute.
                if !matches!(name.as_str(), "target" | "node_modules" | ".git" | "__pycache__") {
                    stack.push(p);
                }
            } else if matches!(
                p.extension().and_then(|s| s.to_str()),
                Some("rs" | "ts" | "js" | "mjs" | "py" | "sh" | "sql")
            ) {
                out.push(p);
            }
        }
    }
    out
}

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/amux-server.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Direction 2: every tag in the tree names an invariant the doc defines.
///
/// THE ANTI-VACUITY GUARD IS THE FILE COUNT, and it is the whole reason this
/// test is not theatre. With zero tags today, "no unknown tags" is zero compared
/// with zero and would hold just as well against a walk that visited nothing, a
/// wrong root, or an extension filter that matched no file. Asserting the walk
/// saw a plausible number of files is what separates "checked and clean" from
/// "did not look", which are otherwise byte-identical greens.
#[test]
fn every_inv_tag_names_an_invariant_the_doc_defines() {
    let root = repo_root();
    let doc = std::fs::read_to_string(root.join("docs/rust-rebuild-plan.md"))
        .expect("docs/rust-rebuild-plan.md must be readable — it is the definition side");
    let known = ids_in(&doc);
    assert!(
        known.len() >= 40,
        "the doc defines {} invariant ids; it listed 53 when this guard was written (a shell \
         grep says 54 because it counts the bare `INV-xxx` placeholder in the prose), so a \
         number this low means the TABLE moved or the parser broke, not that invariants were \
         deleted",
        known.len()
    );

    let files = source_files(&root);
    assert!(
        files.len() > 200,
        "walked only {} source files — the walk is broken, and with no tags in the tree a \
         broken walk and a clean tree produce the SAME pass",
        files.len()
    );

    let mut unknown: Vec<String> = Vec::new();
    let mut tagged: BTreeSet<String> = BTreeSet::new();
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else { continue };
        if !text.contains("INV-") {
            continue;
        }
        for id in ids_in(&text) {
            if known.contains(&id) {
                tagged.insert(id);
            } else {
                unknown.push(format!("{}: {id}", f.strip_prefix(&root).unwrap_or(f).display()));
            }
        }
    }
    assert!(
        unknown.is_empty(),
        "INV- tags naming no invariant in docs/rust-rebuild-plan.md. A tag that matches nothing \
         claims coverage of an invariant that does not exist, which is the defect AMUX-3598 was \
         filed about:\n  {}",
        unknown.join("\n  ")
    );

    // DIRECTION 1 IS REPORTED, NOT ASSERTED. Against 0 tags an assertion would
    // pass by comparing empty with empty and would certify a codebase with no
    // coverage. The number is printed so it is visible and so the day it becomes
    // non-zero is visible too; turning it into a ratchet is honest only once
    // something is genuinely tagged.
    println!(
        "[INV] direction 1 (not enforced): {} of {} invariants have at least one tag across {} \
         source files. See docs/rust-rebuild-plan.md § Doc enforceability for why tagging the \
         current tests against a redesign's invariants would manufacture false coverage.",
        tagged.len(),
        known.len(),
        files.len()
    );
}

/// THE CONTROL, and direction 2 needs one badly: with no tags in the tree the
/// real check passes without evaluating a single tag, so nothing above proves
/// the comparison works. This drives the same predicate over a synthetic set.
#[test]
fn an_invented_tag_is_rejected_and_a_real_one_is_not() {
    // Assembled, never spelled — see the module docs. A literal here would be
    // scanned as a real tag by the test above.
    let id = |s: &str| format!("INV{}{s}", "-");
    let real = id("BOARD-SOT");
    let invented = id("TYPO-SOT");
    let known: BTreeSet<String> = [real.clone(), id("DONE-VS-VERIFIED")].into_iter().collect();

    let found = ids_in(&format!("// {real} holds here\n// {invented} does not exist\n"));
    let unknown: Vec<&String> = found.iter().filter(|i| !known.contains(*i)).collect();
    assert_eq!(unknown.len(), 1, "{found:?}");
    assert_eq!(unknown[0], &invented);
    assert!(found.contains(&real), "a REAL tag must be accepted: {found:?}");

    // The parser's own edges, each one a way a tag could be mis-read.
    assert!(ids_in(&id("")).is_empty(), "a bare prefix is not an id");
    assert_eq!(ids_in(&format!("see {real}, then stop")).iter().next().unwrap(), &real,
               "trailing punctuation must not join the id");
    assert_eq!(ids_in(&format!("{}-", id("A-B"))).iter().next().unwrap(), &id("A-B"),
               "a trailing hyphen is punctuation, not part of the id");
    // Lowercase is not an id: `inv_board_sot_...` is the TEST-NAME convention
    // the doc describes, and folding it in here would make a function name look
    // like a tag.
    assert!(ids_in("inv-board-sot").is_empty(), "ids are uppercase");
}
