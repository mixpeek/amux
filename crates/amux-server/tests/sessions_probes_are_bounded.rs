//! Every subprocess the fleet list spawns must be bounded (AF-301).
//!
//! `GET /api/sessions` shells out to enumerate the fleet. Five of those calls
//! were bare `.output()`, which blocks until the child exits with no timeout —
//! and one of them, a `pgrep`, ran once per shell pane, so a 50-lane fleet did
//! ~50 unbounded spawns per cache miss (TTL 2s).
//!
//! MEASURED: /api/sessions max latency was 697,890ms in the 24h window of
//! 2026-08-28, across eight concurrent requests that all ended within one
//! second of each other — the shape of several callers blocked on the same
//! wedged external process. That starved the runtime and 500'd the dashboard
//! (AF-300).
//!
//! The file has carried `run_bounded` the whole time, whose own WARN says
//! "capture blocked GET /api/sessions for as long as tmux took". The fix
//! existed; these call sites bypassed it. This test is why they cannot again.

const SRC: &str = include_str!("../src/api/sessions_legacy.rs");

/// Lines that actually RUN a command, ignoring comments — a `.output()` named
/// in a comment explaining why it is gone must not fail this.
fn code_lines_with_bare_output() -> Vec<(usize, String)> {
    SRC.lines()
        .enumerate()
        .filter(|(_, l)| {
            let t = l.trim_start();
            // Prose, not code: this file's comments and WARN strings QUOTE
            // `.output()` when explaining why a call is gone, and a string
            // continuation is not a comment so a prefix test alone misses it.
            // Backticks mark prose in this codebase; a real call site has none.
            !t.starts_with("//")
                && !t.starts_with("///")
                && !t.contains('`')
                && t.contains(".output()")
        })
        .map(|(i, l)| (i + 1, l.trim().to_string()))
        .collect()
}

#[test]
fn the_fleet_list_spawns_nothing_unbounded() {
    // CONTROL: the helper the assertion depends on must still be here, or this
    // would pass against a file that had lost its bounding entirely.
    assert!(
        SRC.contains("fn run_bounded("),
        "premise gone: run_bounded is not in this file, so `.output()` being absent proves nothing"
    );
    assert!(
        SRC.contains("fn run_bounded_output("),
        "premise gone: run_bounded_output is not in this file"
    );
    assert!(
        SRC.contains("fn probe_budget()"),
        "premise gone: the probe budget is not in this file"
    );

    let offenders = code_lines_with_bare_output();
    assert!(
        offenders.is_empty(),
        "unbounded subprocess call(s) in the fleet-list path — each blocks until the child \
         exits, which is how one wedged tmux held GET /api/sessions for 697s and took the \
         dashboard down (AF-300/AF-301). Use run_bounded / run_bounded_output / \
         capture_pane_bounded: {offenders:?}"
    );
}

/// The child-liveness probe must cost ONE subprocess, whatever the fleet size.
///
/// # This assertion replaces a total-budget grep, and the swap is the point
///
/// It used to require `let probe_start = std::time::Instant::now();` in the
/// source, guarding the property "fifty `pgrep -P` calls each just under budget
/// is still minutes". 99cee1c8 (AMUX-3894) removed the loop entirely — one
/// `ps -eo ppid=` answers "does this pid have a child" for every pid at once —
/// so the clock it grepped for is legitimately gone and the test went red on a
/// fix, which is the worst kind of red: it accuses the change that removed the
/// hazard.
///
/// The property is not merely retired, because the hazard is not gone; it moved.
/// The fix's own comment names the correct invariant: "N sequential subprocesses
/// cannot be made safe by timing them, only by not being N." A total deadline
/// was the weaker statement of that, so the guard now asserts the strong one
/// directly. Anyone reintroducing a per-lane spawn fails here whether or not
/// they clock it.
///
/// Structural rather than a string match, so it cannot go stale the same way: it
/// reads the per-lane loop's own body and requires that nothing in it spawns.
#[test]
fn the_child_liveness_probe_is_one_subprocess_not_one_per_lane() {
    const LOOP_HEAD: &str = "for (sess, pid) in shell_panes {";
    let at = SRC.find(LOOP_HEAD).expect(
        "premise gone: the per-lane child-liveness loop is not in this file under the name \
         this guard reads, so finding no spawns inside it proves nothing",
    );
    // Body by brace-count from the loop's opening brace.
    let from = at + LOOP_HEAD.len() - 1;
    let mut depth = 0usize;
    let mut end = from;
    for (i, c) in SRC[from..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = from + i;
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(end > from, "could not find the end of the loop body");
    let body = &SRC[from..end];

    for spawner in ["Command::new", "run_bounded", "run_bounded_output", "capture_pane_bounded"] {
        assert!(
            !body.contains(spawner),
            "`{spawner}` is back INSIDE the per-lane loop. That is one subprocess per lane, \
             which is the AMUX-3894 shape: ~50 spawns per cache miss, a fleet-size-dependent \
             cost that no per-call timeout can bound. Answer the question once outside the \
             loop (`ps -eo ppid=`) and look the answer up in here. Body was:\n{body}"
        );
    }

    // CONTROL: the single probe must exist OUTSIDE the loop, or the assertion
    // above passes against a file that simply stopped probing at all — which
    // would read every shell-foreground lane as not-running.
    let outside = SRC[..at].to_string() + &SRC[end..];
    assert!(
        outside.contains(r#"c.args(["-eo", "ppid="])"#),
        "premise gone: the one-shot `ps -eo ppid=` probe is not outside the loop, so \
         'no spawns inside the loop' means 'no liveness probe at all'"
    );

    assert!(
        SRC.contains("pgrep_skipped"),
        "the loop must COUNT what it could not prove: a lane dropped there reads as shell-only, \
         so silent truncation makes the fleet look idler than it is (ethos rule 4)"
    );
}
