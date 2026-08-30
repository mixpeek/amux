//! A board-drive steering enqueue must label its row from `BOARD_DRIVE_GUARD`,
//! never from a typed literal (AF-269).
//!
//! The `guard` column is the only thing that routes a queued row into the
//! AMUX-3052 stale-pickup check: `pickup_stale_void` returns None on its first
//! line if the guard does not start with "board-drive". So a producer that
//! labels its row with a stale or mistyped copy of that string takes its
//! deliveries out of the guard silently, BEFORE the parser is consulted, which
//! means the parse-failure WARN added for AF-268 cannot fire either. The row
//! never reaches it.
//!
//! AF-268 was that defect one layer down: the prompt template and its parser
//! each held a copy of the same string, a correct reword moved one of them, and
//! the guard voided nothing for 17 hours with every unit test green. Collapsing
//! the guard string to one const makes the same drift impossible for the five
//! call sites that exist today. It cannot stop a SIXTH site being written with a
//! fresh literal, and that is what this walks the source for.
//!
//! # What this does NOT cover, said out loud
//!
//! Only the three files in the seam. A steering row enqueued from somewhere else
//! entirely with a hand-written "board-drive" guard would pass here. That is a
//! deliberate scope: those three are where board-drive deliveries are minted
//! today, and a guard that walked every file would spend its failures on
//! unrelated strings.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

/// The files that mint or read a board-drive steering guard.
const SEAM: [&str; 3] = [
    "crates/amux-server/src/runtime_jobs/board_drive.rs",
    "crates/amux-server/src/api/board.rs",
    "crates/amux-server/src/api/session_verbs.rs",
];

#[test]
fn no_board_drive_guard_is_written_as_a_literal_in_the_steering_seam() {
    let root = workspace_root();
    let mut offenders: Vec<String> = Vec::new();
    let mut const_uses = 0usize;
    let mut declarations = 0usize;

    for rel in SEAM {
        let path = root.join(rel);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        // THE WALK MUST HAVE FOUND A REAL FILE. A rename that empties this
        // string makes every check below vacuously true.
        assert!(
            src.len() > 1000,
            "{rel} read as {} bytes — the guard is walking the wrong path",
            src.len()
        );

        for (i, line) in src.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("///") || trimmed.starts_with("*") {
                continue; // prose about the guard is not a use of it
            }
            if line.contains("BOARD_DRIVE_GUARD") {
                if line.contains("pub(crate) const BOARD_DRIVE_GUARD") {
                    declarations += 1;
                } else {
                    const_uses += 1;
                }
                continue;
            }
            // A literal in the guard role: the string, in a line that is either
            // an enqueue call or a guard comparison. `ids::BOARD_DRIVE` in the
            // registry is the JOB id and a different concept, so this stays
            // scoped to the three seam files above.
            let literal = line.contains("\"board-drive\"") || line.contains("\"board-drive:");
            // The literal alone is not a violation — the string appears as a JOB
            // name and as a log field too. It has to be in the GUARD position.
            // `contains("guard")` was the first draft and it fired on
            // `["commit-nudge", "board-drive", "staged-guard", ...]`, a list of
            // source names: "staged-guard" contains "guard". Hence the shapes.
            let guard_role = line.contains("steer_enqueue")
                || line.contains("starts_with")
                || line.contains("guard:")
                || line.contains("guard =")
                || line.contains("guard,");
            if literal && guard_role {
                offenders.push(format!("{rel}:{}  {}", i + 1, line.trim()));
            }
        }
    }

    // POSITIVE CONTROLS, both directions. Without these the test passes just as
    // happily against a seam where the const was deleted and every use with it.
    assert_eq!(
        declarations, 1,
        "expected exactly one `pub(crate) const BOARD_DRIVE_GUARD` declaration, found {declarations}"
    );
    assert!(
        const_uses >= 4,
        "only {const_uses} uses of BOARD_DRIVE_GUARD across the seam — there were 5 call sites \
         when this guard was written (2 enqueues, 1 reactive dispatch, 2 reads), so either they \
         were removed or this guard has stopped finding them"
    );

    assert!(
        offenders.is_empty(),
        "a board-drive steering guard is written as a literal instead of BOARD_DRIVE_GUARD.\n\
         A copy that drifts takes its rows out of the AMUX-3052 stale-pickup check before the \
         parser sees them, so the AF-268 parse-failure WARN cannot fire either:\n  {}",
        offenders.join("\n  ")
    );
}
