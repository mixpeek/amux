//! LIVE e2e for the `.mdai` computed-file engine (AMUX-3240), using REAL files
//! and the REAL fast model.
//!
//! This test creates real text/markdown files and a folder in a temp dir, builds
//! a small multi-node DAG with various edge prompts, and runs it through the real
//! model. It asserts STRUCTURAL eval criteria rather than exact output:
//!
//! - the output is non-empty and references source content,
//! - execution is upstream-first (the entry node runs last),
//! - a history entry is recorded and returned newest-first,
//! - a cyclic graph errors, naming the cycle,
//! - changing a source changes the output.
//!
//! It SKIPS cleanly (loud, printed reason) when no model is configured, exactly
//! like the live-Chrome tests skip without a browser, so CI without model
//! credentials stays green. The gate is a real probe: build the real client and
//! ask it a trivial question; if that fails (no CLI, no auth), skip. A probe
//! beats guessing which of several credential sources is present.

use amux_server::api::mdai::{connect_edge, run_dag, CliModel, ModelClient, MdaiError};
use amux_server::db::Store;
use std::path::Path;

/// True when the real fast model can actually answer. A probe, not a guess.
///
/// THE PROBE MUST READ THE ANSWER, NOT JUST ITS LENGTH (2026-09-02). It asked
/// for exactly the word "ok" and then accepted ANY non-empty string, so a CLI
/// that answers without answering counted as available. Measured: with the
/// account session-limited, `complete` returns `Ok("You've hit your session
/// limit · resets 7pm (America/New_York)")` — non-empty, so the probe said
/// available, both tests ran, and both failed comparing that sentence against
/// expected content:
///
///   the connected source should reach the output; got: You've hit your session
///   limit · resets 7pm (America/New_York)
///
/// That is a red suite reporting a quota state as a product defect, and the
/// whole point of this gate is that CI without model credentials stays green.
/// A refusal is exactly the case it exists to skip; it just arrives as `Ok`.
///
/// So: check for the token that was requested. Any other reply — a quota
/// message, a policy refusal, a login prompt, an outage banner — is a model
/// that cannot answer, which is the condition this function is named for. The
/// reason is printed either way, so a skip is never silent.
fn model_available() -> bool {
    let model = amux_server::api::mdai::resolve_model(None);
    match CliModel.complete(&model, "Reply with exactly the word: ok") {
        Ok(s) => {
            let ok = probe_answered(&s);
            if !ok {
                eprintln!(
                    "SKIP mdai live e2e: the model replied but did not answer the probe \
                     (asked for \"ok\", got: {}). Treating as unavailable — a quota message, \
                     a refusal or a login prompt is not a working model.",
                    s.trim().chars().take(120).collect::<String>()
                );
            }
            ok
        }
        Err(e) => {
            eprintln!("SKIP mdai live e2e: model not available ({e})");
            false
        }
    }
}

/// Did the reply actually answer the probe? Pure, so the SKIP path can be
/// tested without a session limit to reproduce.
///
/// Substring, not equality: a live model may add punctuation or a trailing
/// newline, and pinning exact bytes would skip on a model that answered
/// correctly — the opposite failure and the more expensive one, since it would
/// silently stop exercising the feature.
fn probe_answered(reply: &str) -> bool {
    reply.to_lowercase().contains("ok")
}

#[test]
fn the_probe_reads_the_answer_rather_than_its_length() {
    // THE SPECIMEN, verbatim from the failing run on 2026-09-02. Non-empty, so
    // the old `!s.trim().is_empty()` called it available and ran both tests,
    // which then failed comparing this sentence against expected content.
    assert!(!probe_answered("You've hit your session limit · resets 7pm (America/New_York)"));

    // Other ways a CLI answers without answering.
    assert!(!probe_answered("Please run `claude login` to continue."));
    assert!(!probe_answered("I can't help with that request."));
    assert!(!probe_answered(""));
    assert!(!probe_answered("   \n  "));

    // CONTROLS — a real answer must still count, including the shapes a live
    // model actually produces. Without these, `false` would pass every cell
    // above and the gate would skip forever, silently retiring the feature.
    assert!(probe_answered("ok"));
    assert!(probe_answered("ok\n"));
    assert!(probe_answered("OK."));
    assert!(probe_answered("  Ok  "));
}

fn write(dir: &Path, name: &str, content: &str) {
    std::fs::write(dir.join(name), content).unwrap();
}

fn lower(s: &str) -> String {
    s.to_lowercase()
}

#[test]
fn mdai_live_e2e_real_files_real_model() {
    if !model_available() {
        return; // skipped cleanly; reason printed by model_available()
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let store = Store::open(&root.join("mdai-live.db")).unwrap();
    let model = CliModel;

    // Real source files: a markdown note, a plain text fact, and a folder.
    write(
        root,
        "facts.md",
        "# Project Zephyr\nZephyr is a solar-powered irrigation controller. \
         Its codename mascot is a blue heron named Marlow.",
    );
    std::fs::create_dir_all(root.join("specs")).unwrap();
    write(
        &root.join("specs"),
        "battery.txt",
        "The battery reserve for Project Zephyr must sustain 72 hours with no sun.",
    );

    // A leaf node that summarizes the folder of specs.
    write(
        root,
        "specs.mdai",
        "---\nsources:\n  - path: specs\n    prompt: List the hard requirements found in these spec files.\n---\n\
         # Requirements\nExtract every hard requirement, one per line, from the connected spec files.",
    );

    // The entry node depends on the plain markdown fact AND the leaf spec node.
    write(
        root,
        "brief.mdai",
        "---\nsources:\n  - path: facts.md\n    prompt: Use this as the project background.\n  \
         - path: specs.mdai\n    prompt: These are the extracted requirements.\n---\n\
         # Brief\nWrite a two-sentence brief that names the project codename and \
         states its battery reserve requirement, using ONLY the connected sources.",
    );

    // --- Run the DAG ---
    let r = run_dag(&store, root, "brief.mdai", &model, Some("test-e2e"))
        .expect("the DAG should run against the real model");

    // Structural: non-empty output that references source content.
    assert!(!r.output.trim().is_empty(), "output must be non-empty");
    let out = lower(&r.output);
    assert!(
        out.contains("zephyr") || out.contains("72") || out.contains("marlow"),
        "output should reference connected source content; got: {}",
        r.output
    );

    // Upstream-first: the entry node (brief) runs LAST, after specs.
    assert_eq!(r.path, "brief.mdai");
    let entry_pos = r.nodes.iter().position(|n| n.path == "brief.mdai").unwrap();
    let specs_pos = r.nodes.iter().position(|n| n.path == "specs.mdai").unwrap();
    assert!(
        specs_pos < entry_pos,
        "specs.mdai must execute before brief.mdai (upstream-first); nodes: {:?}",
        r.nodes.iter().map(|n| n.path.clone()).collect::<Vec<_>>()
    );

    // History recorded and returned newest-first.
    let conn = store.read().unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM mdai_runs WHERE path='brief.mdai'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(count >= 1, "a history entry must be recorded for brief.mdai");
    drop(conn);

    // Changing a source changes the output (and history grows).
    write(
        root,
        "facts.md",
        "# Project Zephyr\nZephyr is a solar-powered irrigation controller. \
         Its codename mascot is a red kestrel named Pippin.",
    );
    let r2 = run_dag(&store, root, "brief.mdai", &model, Some("test-e2e"))
        .expect("re-run after a source change");
    // The entry node recomputed (its context changed), so it was not a cache hit.
    assert!(!r2.cached, "changing a source must recompute the entry node");

    // Newest-first history: the two entry-node runs come back with the newest id
    // first.
    let conn = store.read().unwrap();
    let mut stmt = conn
        .prepare("SELECT id FROM mdai_runs WHERE path='brief.mdai' ORDER BY id DESC")
        .unwrap();
    let ids: Vec<i64> = stmt
        .query_map([], |r| r.get::<_, i64>(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    assert!(ids.len() >= 2, "history should have grown after the second run");
    assert!(ids[0] > ids[1], "history must be newest-first");
    drop(stmt);
    drop(conn);

    // A cyclic graph errors, naming the cycle.
    write(root, "loopA.mdai", "---\nsources:\n  - path: loopB.mdai\n---\nA");
    write(root, "loopB.mdai", "---\nsources:\n  - path: loopA.mdai\n---\nB");
    let err = run_dag(&store, root, "loopA.mdai", &model, None).unwrap_err();
    match err {
        MdaiError::Cycle(chain) => {
            let j = chain.join(" -> ");
            assert!(j.contains("loopA.mdai") && j.contains("loopB.mdai"), "cycle names both: {j}");
        }
        other => panic!("expected a cycle error, got {other}"),
    }
}

/// connect_edge is exercised against a real file here too: it must produce a
/// frontmatter a subsequent run can resolve. Gated the same way (writing a file
/// is free, but running the resulting graph needs the model).
#[test]
fn mdai_live_connect_then_run() {
    if !model_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // connect_edge reads the files root from AMUX_FILES_ROOT.
    std::env::set_var("AMUX_FILES_ROOT", root);
    let store = Store::open(&root.join("mdai-connect.db")).unwrap();
    let model = CliModel;

    write(root, "ingredient.md", "The secret ingredient is smoked paprika.");
    write(root, "recipe.mdai", "---\nsources: []\n---\n# Recipe\nName the secret ingredient from the connected source.");

    connect_edge("ingredient.md", "recipe.mdai", "This names the secret ingredient.")
        .expect("connect should write a valid edge");

    let r = run_dag(&store, root, "recipe.mdai", &model, None)
        .expect("the connected graph should run");
    assert!(
        lower(&r.output).contains("paprika"),
        "the connected source should reach the output; got: {}",
        r.output
    );

    std::env::remove_var("AMUX_FILES_ROOT");
}
