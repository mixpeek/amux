//! Hourly, per-key de-duplication for log lines that restate an unchanging fact.
//!
//! Extracted from `api::session_verbs` (AEAB-13) so a second surface could use
//! it WITHOUT a second spelling of it (AEAB-45). Two spellings of "the same
//! condition, again" drift, and the whole defect both cards describe is one
//! surface deduping while another does not.
//!
//! ## Why this exists at all
//!
//! AEAB-13: `warn_on_stalled_lanes` emitted its `tracing::warn!` on every tick
//! with no memory, while the `emit_event` calls immediately below it were
//! already deduped hourly, per lane, per condition — with a comment naming the
//! exact hazard: "no idem at all fires every tick, which is the nag AC-310 was
//! filed about." The event path was fixed; the log line was left behind. One
//! undeliverable message to a lane that had been dead six days produced 921 of
//! the 1004 log lines since the last restart — 92% of the log — and buried a
//! first-ever `database is locked` line during a log review that existed to
//! find exactly that.
//!
//! AEAB-45 is the same shape from the other side. The disk-ranking warning was
//! moved from once-per-ATTEMPT to once-per-RUN and its own comment says the
//! per-attempt spelling "drowned the log it shares with real faults" — but the
//! run is on a two-minute timer, so it still emitted 1,336 identical lines in
//! 24h naming `~/.Trash`, a condition that cannot change. 77% of the window.
//! **A per-run dedupe is not a dedupe when the run is on a timer.**
//!
//! ## The cadence, and why it is not "log once"
//!
//! First occurrence in a bucket logs; repeats in the same bucket do not; the
//! next bucket logs again. The re-statement is deliberate. A permanent idem
//! would fire once and then stay silent forever, so an ONGOING problem would
//! vanish from the log entirely and the fix would be worse than the bug.
//!
//! In-process and not durable, which is the right trade: re-stating an ongoing
//! problem once per server start is useful rather than noisy, whereas a missed
//! NOTIFICATION is lost, which is why the event-side idem is DB-backed.
//!
//! ## Choosing a key
//!
//! Key on everything a reader would act on differently. `session_verbs` keys on
//! `{lane}:{condition}` because those two are separately actionable. `autofix`
//! keys on the joined PATH LIST, so a skip set that CHANGES reports immediately
//! inside the same hour rather than hiding behind the previous hour's entry —
//! the set is the whole content of the message. Namespace your keys; the map is
//! shared across callers.

/// True the first time this key is seen in this bucket, false afterwards.
///
/// Entries from previous buckets are dropped when the bucket rolls, so the set
/// cannot grow without bound. On a poisoned lock this fails OPEN — it returns
/// true and logs, because swallowing a line to protect a de-duplicator inverts
/// the priority.
pub(crate) fn first_this_bucket(key: &str, bucket: i64) -> bool {
    static M: std::sync::OnceLock<std::sync::Mutex<(i64, std::collections::BTreeSet<String>)>> =
        std::sync::OnceLock::new();
    let m = M.get_or_init(|| std::sync::Mutex::new((i64::MIN, std::collections::BTreeSet::new())));
    let Ok(mut g) = m.lock() else { return true };
    if g.0 != bucket {
        g.0 = bucket;
        g.1.clear();
    }
    g.1.insert(key.to_string())
}

/// The hour containing `now` (unix seconds). Callers share one spelling so two
/// surfaces cannot disagree about when an hour starts.
pub(crate) fn hour_bucket(now: f64) -> i64 {
    (now / 3600.0) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Moved verbatim in intent from `session_verbs` (AEAB-13), because the
    /// properties belong to the helper rather than to its first caller.
    ///
    /// Keys are namespaced per case so this test cannot interfere with the
    /// AEAB-45 case below — the map is process-global and cargo runs tests in
    /// parallel, which has already bitten this repo once (the du budget env
    /// vars, AEAB-33).
    #[test]
    fn a_key_logs_once_per_bucket_and_again_in_the_next() {
        let b = 1_000_000i64;

        // 1. First occurrence logs; the identical repeat does not. This is the
        //    921-lines-per-restart case, and the 1,336-lines-per-day one.
        assert!(first_this_bucket("t1:Amux-gtm:not-running", b));
        assert!(!first_this_bucket("t1:Amux-gtm:not-running", b));
        assert!(!first_this_bucket("t1:Amux-gtm:not-running", b));

        // 2. A DIFFERENT CONDITION on the same lane still logs. `not-running`
        //    and `no-env-file` are actionable and completely different.
        assert!(first_this_bucket("t1:Amux-gtm:no-env-file", b));

        // 3. A different LANE still logs — dedupe is per key, not global. A
        //    global gate would silence a real second outage.
        assert!(first_this_bucket("t1:other-lane:not-running", b));

        // 4. THE NEXT HOUR logs again. This is the load-bearing one: a
        //    permanent idem would fire once and then stay silent forever, so an
        //    ongoing stall would vanish from the log entirely.
        assert!(first_this_bucket("t1:Amux-gtm:not-running", b + 1));
        assert!(!first_this_bucket("t1:Amux-gtm:not-running", b + 1));

        // 5. Rolling the bucket must not leak memory: entries from the previous
        //    hour are dropped, which is observable as the old key logging again
        //    rather than being remembered.
        assert!(first_this_bucket("t1:other-lane:not-running", b + 1));
    }

    /// AEAB-45's specific property, and the one nothing tested before: when the
    /// CONTENT of the message changes, it must be reported in the SAME hour and
    /// not hidden behind the previous content's entry.
    ///
    /// This is the case that distinguishes keying on content from keying on a
    /// lane name, and it is why the disk warnings key on the joined path list.
    /// A skip set growing from `.Trash` to `.Trash, Library/Caches` is new
    /// information about the disk; silencing it for up to an hour would
    /// reproduce the incomplete-ranking blindness AEAB-33 was filed about.
    #[test]
    fn changed_content_logs_immediately_within_the_same_bucket() {
        let b = 2_000_000i64;
        let one = "t2:disk-unreadable:/Users/x/.Trash";
        let two = "t2:disk-unreadable:/Users/x/.Trash, /Users/x/Library/Caches";

        assert!(first_this_bucket(one, b), "first sighting always logs");
        assert!(!first_this_bucket(one, b), "unchanged set stays quiet");
        assert!(
            first_this_bucket(two, b),
            "a CHANGED skip set must log at once, not wait for the next hour"
        );
        assert!(!first_this_bucket(two, b), "…and then it too goes quiet");
        assert!(
            !first_this_bucket(one, b),
            "reverting to the earlier set inside the same hour is not news"
        );
    }

    /// The bucket is a pure function of the clock, and both callers must agree
    /// on it — a helper that computed its own hour differently per call site
    /// would silently give one surface a shorter cadence than the other.
    #[test]
    fn hour_bucket_is_whole_hours_and_shared() {
        assert_eq!(hour_bucket(0.0), 0);
        assert_eq!(hour_bucket(3599.9), 0);
        assert_eq!(hour_bucket(3600.0), 1);
        assert_eq!(hour_bucket(7200.5), 2);
    }
}
