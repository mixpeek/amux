//! A gate script piped into another command cannot fail the build.
//!
//! GitHub's default shell for a `run:` step is `bash -e {0}`, WITHOUT
//! `pipefail`. A pipeline then exits with the status of its LAST command, so
//! `bash scripts/soak-probe.sh | tee soak.log` reports whatever `tee` reports,
//! which is always success. The script's `exit 1` is discarded.
//!
//! Measured 2026-09-18 (AMUX-4742): the weekly `rust-soak` RSS leak gate had
//! been in exactly that shape. Five consecutive runs (08-16, 08-23, 08-30,
//! 09-06, 09-13) each printed `FAIL: RSS grew ...` with growth 0.76-1.47
//! against a 0.20 threshold, and each reported `success`. The regression the
//! gate exists to catch was visible in its own output for four weeks while the
//! workflow stayed green.
//!
//! This is ethos rule 7 ("can your check actually fail?") applied to the CI
//! definition rather than to the code. A check that cannot go red is
//! indistinguishable from a check that passes, and it is worse than no check,
//! because somebody is relying on it.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/amux-server has two ancestors")
        .to_path_buf()
}

/// `||` is not a pipe. Strip it before looking for one, or every
/// `cmd || true` in the repo reads as an unguarded pipeline.
fn has_real_pipe(line: &str) -> bool {
    line.replace("||", "").contains('|')
}

/// The LAST command of a run body decides the step's exit status. That is the
/// shape that shipped, and restricting to it is what keeps this guard from
/// being noise.
///
/// WHAT THIS DELIBERATELY DOES NOT CATCH: a piped gate EARLIER in a body.
/// Under `bash -e` a mid-body pipeline that "fails" would abort the step, but
/// without pipefail it never reports failure, so that shape is a real defect
/// too. It is excluded because the honest example in this repo is diagnostic:
/// rust.yml's frustrations-audit step captures `rc=$?` from an UNPIPED run and
/// pipes into `head` only to PRINT, then `exit "$rc"`. Flagging it would be
/// wrong, and a guard that cries wolf gets deleted rather than satisfied.
/// Catching the mid-body case needs dataflow this test does not do.
fn last_command(body: &str) -> Option<&str> {
    body.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty() && !l.starts_with('#'))
}

/// Only OUR OWN scripts are in scope. A pipeline over stock tools
/// (`grep ... | head`) is ordinary shell; the defect is discarding the exit
/// status of a repo script whose whole job is to return one.
fn pipes_a_repo_script(body: &str) -> bool {
    last_command(body).is_some_and(|l| has_real_pipe(l) && l.contains("scripts/"))
}

/// Either form is a real remedy: `pipefail` inside the block, or `shell: bash`,
/// which GitHub expands to `bash --noprofile --norc -eo pipefail {0}`.
fn exit_status_survives(body: &str, shell: Option<&str>) -> bool {
    body.contains("pipefail") || matches!(shell, Some(s) if s.trim() == "bash")
}

/// Every (file, step-name, run-body) in .github/workflows.
fn steps() -> Vec<(String, String, String, Option<String>)> {
    let dir = workspace_root().join(".github/workflows");
    let mut out = Vec::new();
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yml") {
            continue;
        }
        let file = path.file_name().unwrap().to_string_lossy().to_string();
        let text = std::fs::read_to_string(&path).expect("workflow is readable");
        let doc: serde_yaml::Value = match serde_yaml::from_str(&text) {
            Ok(v) => v,
            Err(e) => panic!("{file} is not valid YAML: {e}"),
        };
        let Some(jobs) = doc.get("jobs").and_then(|j| j.as_mapping()) else {
            continue;
        };
        for (_, job) in jobs {
            let Some(list) = job.get("steps").and_then(|s| s.as_sequence()) else {
                continue;
            };
            for step in list {
                let Some(run) = step.get("run").and_then(|r| r.as_str()) else {
                    continue;
                };
                let name = step
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("<unnamed>")
                    .to_string();
                let shell = step
                    .get("shell")
                    .and_then(|s| s.as_str())
                    .map(str::to_string);
                out.push((file.clone(), name, run.to_string(), shell));
            }
        }
    }
    assert!(
        !out.is_empty(),
        "found no run: steps at all — the scan is broken"
    );
    out
}

#[test]
fn a_piped_gate_script_keeps_its_exit_status() {
    let mut offenders = Vec::new();
    for (file, name, body, shell) in steps() {
        if pipes_a_repo_script(&body) && !exit_status_survives(&body, shell.as_deref()) {
            offenders.push(format!("{file} / step {name:?}: {}", body.trim()));
        }
    }
    assert!(
        offenders.is_empty(),
        "these steps pipe a repo script and discard its exit status, so the gate \
         cannot fail the build. Add `set -o pipefail` to the run block (or \
         `shell: bash`):\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn the_detector_can_actually_fail() {
    // The EXACT pre-fix body from rust-soak.yml, which shipped a gate that
    // could not go red for four weeks. If this stops being flagged, the
    // detector above is decoration.
    let pre_fix = "bash scripts/soak-probe.sh | tee soak.log";
    assert!(pipes_a_repo_script(pre_fix));
    assert!(
        !exit_status_survives(pre_fix, None),
        "the pre-fix body must be reported as unguarded"
    );

    // And both remedies must clear it, or the guard would refuse a correct fix.
    assert!(exit_status_survives(
        "set -o pipefail\nbash scripts/soak-probe.sh | tee soak.log",
        None
    ));
    assert!(exit_status_survives(pre_fix, Some("bash")));

    // NEGATIVE CONTROLS. These must NOT be flagged, or the guard is noise that
    // someone will delete rather than satisfy.
    assert!(
        !pipes_a_repo_script("brew install xcodegen || true"),
        "`||` is not a pipe"
    );
    assert!(
        !pipes_a_repo_script("grep -c foo bar.txt | head -1"),
        "a pipeline over stock tools is ordinary shell, not a discarded gate"
    );
    assert!(
        !pipes_a_repo_script("bash scripts/soak-probe.sh"),
        "an unpiped script already keeps its exit status"
    );

    // The real body from rust.yml's frustrations-audit step. Its pipe is
    // DIAGNOSTIC: the status comes from `rc=$?` on an unpiped run and is
    // re-raised with `exit "$rc"`, and the step ends on an unpiped script. The
    // first version of this guard flagged it, which is what narrowed the
    // detector to the last command. Kept verbatim so a future widening has to
    // decide about this case on purpose.
    let diagnostic_pipe = "set +e\n\
                           python3 scripts/frustrations_audit.py > /dev/null\n\
                           rc=$?\n\
                           set -e\n\
                           if [ \"$rc\" != \"0\" ] && [ \"$rc\" != \"2\" ]; then\n\
                             python3 scripts/frustrations_audit.py | head -20\n\
                             exit \"$rc\"\n\
                           fi\n\
                           bash scripts/test-frustrations-audit.sh";
    assert!(
        !pipes_a_repo_script(diagnostic_pipe),
        "a mid-body diagnostic pipe whose step ends on an unpiped script is not the defect"
    );
}
