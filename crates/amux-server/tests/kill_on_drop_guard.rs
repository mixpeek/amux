//! Every timeout-wrapped subprocess must set `kill_on_drop(true)`.
//!
//! # Why this guard exists
//!
//! `tokio::time::timeout(t, cmd.output())` looks safe and leaks a process when
//! it fires. The timeout DROPS the future; without `kill_on_drop` the child is
//! neither killed nor reaped, so it becomes a zombie the moment it exits.
//!
//! Measured on 2026-08-29: **97 zombies parented to amux-server-rs**,
//! accumulated in bursts across 15 hours. Four call sites were missing the flag
//! (`git_guard`, `lookup`, `autofix`'s `gh`, `scheduler`'s shell runner) while
//! four others already had it — the drift was invisible because every site
//! compiles and behaves identically until its timeout actually fires.
//!
//! A comment asking people to remember is the kind of rule this repo's ethos
//! file warns about. This fails the build instead.

use std::path::Path;

/// Sites that legitimately have no `kill_on_drop` because they do not wrap the
/// child in a timeout at all. Add with a reason, never to silence a real hit.
const ALLOW: &[&str] = &[];

#[test]
fn every_timeout_wrapped_command_sets_kill_on_drop() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    let mut checked = 0usize;

    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|x| x.to_str()) != Some("rs") {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&p) else { continue };
            let lines: Vec<&str> = src.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if !line.contains("timeout(") {
                    continue;
                }
                // Match on the timeout's INNER FUTURE, not on nearby text. The
                // first version keyed off "a Command appears within 12 lines"
                // and produced two false positives immediately: an HTTP call
                // (`client.inbox_messages`) whose neighbour was `let futs`, and
                // `ch.wait()` on an already-owned child sitting below an
                // unrelated `Command::new("kill")`. Proximity cannot tell those
                // from a real one; the inner expression can.
                let direct = line.contains(".output()") || line.contains(".status()");

                // The other real shape: `let fut = Command::new(..)..output();`
                // then `timeout(d, fut)`. Only counts if that identifier was
                // actually bound from a Command just above.
                let indirect = line.contains(", fut)")
                    && lines[i.saturating_sub(10)..i]
                        .iter()
                        .any(|l| l.contains("let fut") && l.contains("Command::new"))
                    || (line.contains(", fut)")
                        && lines[i.saturating_sub(10)..i].iter().any(|l| l.contains("let fut"))
                        && lines[i.saturating_sub(10)..i]
                            .iter()
                            .any(|l| l.contains("Command::new")));

                if !(direct || indirect) {
                    continue;
                }
                checked += 1;
                let lo = i.saturating_sub(14);
                let hi = (i + 3).min(lines.len());
                let window = lines[lo..hi].join("\n");
                if window.contains("kill_on_drop") {
                    continue;
                }
                let rel = p
                    .strip_prefix(Path::new(env!("CARGO_MANIFEST_DIR")))
                    .unwrap_or(&p)
                    .display()
                    .to_string();
                let loc = format!("{rel}:{}", i + 1);
                if ALLOW.iter().any(|a| loc.starts_with(a)) {
                    continue;
                }
                offenders.push(loc);
            }
        }
    }

    // The guard must actually be looking at something. If a refactor changes the
    // idiom, `checked == 0` would make this pass forever while checking nothing —
    // green, and blind (ethos rule 7).
    assert!(
        checked >= 6,
        "the guard matched only {checked} timeout-wrapped commands; the idiom probably \
         changed and this check has stopped discriminating"
    );

    assert!(
        offenders.is_empty(),
        "timeout-wrapped subprocess without `kill_on_drop(true)` — a fired timeout drops \
         the future and leaks a zombie (97 of them on 2026-08-29):\n  - {}\n\nAdd \
         `.kill_on_drop(true)` to the Command builder.",
        offenders.join("\n  - ")
    );
}
