//! The once-a-local-day clock the two owner digests share: the email approval
//! digest (`email_approval_digest.rs`) and the decision digest
//! (`api/decision_digest.rs`). Each used to carry its own `digest_hour` and its
//! own "at or after the hour, not sent today" rule (primitive integrity audit
//! 2026-11-26, "Duplicated digest timer"); this is the one copy.
//!
//! Why the digests are not `schedules` rows (step 5 of that audit): the
//! decision digest's tick also resumes an interrupted apply and applies a
//! published default only after its grace window, hours after the digest
//! goes out, so it needs a sub-daily loop whatever sends the digest. The
//! approval digest retries an undelivered notice on its next tick within the
//! hour, and a `daily at H` schedule would wait a day. Both stay
//! `PeriodicTask`s that ask this clock whether today's digest is due.

use std::path::Path;

/// The hour used when the key is unset or not an hour.
pub const DEFAULT_HOUR: u32 = 9;

/// The local hour a digest goes out, from a global key (`server.env`, then
/// the process env; the digests are addressed to the owner, not to a worker).
pub fn hour(home: &Path, key: &str) -> u32 {
    crate::api::settings::effective_env(home, key)
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|h| *h < 24)
        .unwrap_or(DEFAULT_HOUR)
}

/// Is today's digest due? Pure: something to say, at or after the hour, and
/// not already sent today (`last_sent` is the local date it last went out).
pub fn due(today: &str, hour_now: u32, last_sent: Option<&str>, hour: u32, pending: usize) -> bool {
    pending > 0 && hour_now >= hour && last_sent != Some(today)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_once_a_day_after_the_hour_and_only_with_something_pending() {
        assert!(due("2026-09-26", 9, None, 9, 4));
        assert!(!due("2026-09-26", 8, None, 9, 4), "before the hour");
        assert!(!due("2026-09-26", 15, Some("2026-09-26"), 9, 4), "already sent today");
        assert!(due("2026-09-27", 9, Some("2026-09-26"), 9, 4), "next day");
        assert!(!due("2026-09-26", 9, None, 9, 0), "nothing pending, nothing sent");
    }

    #[test]
    fn the_hour_comes_from_the_key_and_falls_back_on_garbage() {
        let d = tempfile::tempdir().unwrap();
        let key = "AMUX_DIGEST_CLOCK_TEST_HOUR";
        assert_eq!(hour(d.path(), key), DEFAULT_HOUR);
        std::fs::write(d.path().join("server.env"), format!("{key}=14\n")).unwrap();
        assert_eq!(hour(d.path(), key), 14);
        std::fs::write(d.path().join("server.env"), format!("{key}=25\n")).unwrap();
        assert_eq!(hour(d.path(), key), DEFAULT_HOUR, "not an hour");
    }
}
