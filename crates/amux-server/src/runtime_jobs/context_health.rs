//! Compaction-generation watch (AMUX-3742).
//!
//! The report was "the raw claude code terminal is performing better than amux
//! claude code, same model same prompt". It was true, and nothing in amux could
//! say why, so it stayed a feeling.
//!
//! Measured 2026-08-26 across every transcript active since 08-05: model and
//! effort were IDENTICAL on both sides (both `claude-opus-5` at `xhigh`, both
//! ~57k first-turn input tokens). What differed was compaction generations —
//! amux lanes median 8, mean 20, max 215; raw terminal sessions median 0, mean
//! 3.1. A lane resumes the same conversation forever, so it answers from a
//! summary of a summary while a fresh terminal reads primary sources.
//!
//! This job is the "and make it surface in the logs" half of that fix. The
//! remedy (`amux fresh <lane>`) is the other half, and it is deliberately never
//! automatic: a conversation is the lane's accumulated work, so deciding it is
//! spent belongs to its owner (ethos rule 8). This job reports; a human acts.

use crate::api::session_verbs::{generation_census, is_degraded, GENERATIONS_WARN_AT};

const JOB: &str = "context-health";
const TICK_SECS: u64 = 3600;

/// One pass. Returns (measured, unmeasurable, over-threshold) so the test can
/// assert on the counts rather than on a log line.
pub fn tick() -> (u32, u32, usize) {
    let census = generation_census();
    let mut measured = 0u32;
    let mut unmeasurable = 0u32;
    let mut over: Vec<(String, u32)> = Vec::new();
    for (lane, gens) in census {
        match gens {
            Some(g) => {
                measured += 1;
                if is_degraded(g) {
                    over.push((lane, g));
                }
            }
            None => unmeasurable += 1,
        }
    }
    over.sort_by_key(|(_, g)| std::cmp::Reverse(*g));

    // ALWAYS logged, including on a clean pass. A tick that only speaks when
    // something is wrong makes "no lanes are degraded" and "the census found
    // nothing to measure" the same silence — the exact shape ethos rule 4 is
    // about, which is why `measured` and `unmeasurable` ride along even when
    // `over` is empty.
    tracing::info!(
        job = JOB,
        measured,
        unmeasurable,
        over = over.len(),
        warn_at = GENERATIONS_WARN_AT,
        "context-health census"
    );
    if !over.is_empty() {
        let worst = over
            .iter()
            .take(8)
            .map(|(n, g)| format!("{n}={g}"))
            .collect::<Vec<_>>()
            .join(" ");
        tracing::warn!(
            job = JOB,
            count = over.len(),
            worst = %worst,
            "context_degraded: lanes answering from deeply-compacted conversations — \
             `amux fresh <lane>` recycles one (keeps env/cards/memory), \
             GET /api/debug/context-health for the full census"
        );
    }
    (measured, unmeasurable, over.len())
}

pub fn spawn() -> super::PeriodicTask {
    super::spawn_periodic(JOB, TICK_SECS, || async {
        let _ = tokio::task::spawn_blocking(|| {
            tick();
        })
        .await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The census must never panic on a real `~/.amux/sessions` tree, and its
    /// three counters must partition the lanes it saw — a census where
    /// measured+unmeasurable disagrees with the lane count is reporting on a
    /// set nobody can name.
    #[test]
    fn tick_counts_partition_the_census() {
        let census = generation_census();
        let (measured, unmeasurable, over) = tick();
        assert_eq!(
            (measured + unmeasurable) as usize,
            census.len(),
            "every lane must land in exactly one of measured/unmeasurable"
        );
        assert!(over <= measured as usize, "an over-threshold lane must have been measured");
    }

    /// The boundary of the degraded predicate, both sides.
    ///
    /// The first version of this asserted on two constants, which clippy
    /// rightly refused: a comparison the compiler folds is the purest form of
    /// a check that cannot fail (ethos rule 7). This exercises the shipped
    /// predicate at runtime instead, and pins that the comparison is `>=` —
    /// a lane sitting exactly ON the threshold is degraded, which is the cell
    /// an off-by-one silently drops.
    #[test]
    fn the_degraded_predicate_includes_its_own_threshold() {
        let at = std::hint::black_box(GENERATIONS_WARN_AT);
        assert!(is_degraded(at), "a lane exactly at the threshold must count");
        assert!(is_degraded(at + 1));
        assert!(!is_degraded(at - 1), "one below must not");
        assert!(!is_degraded(std::hint::black_box(0u32)), "a pristine lane is never degraded");
    }
}
