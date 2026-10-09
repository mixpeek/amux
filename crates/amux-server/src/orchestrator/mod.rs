//! Orchestrator runtime (Phase 1: RR-0029/0041) and its deterministic
//! simulation seams (Phase 0: RR-0027, Invariant 22).
//!
//! Phase 0 lands the seams the simulation needs — a clock the tests own and
//! a seedable RNG — because retrofitting determinism after the loop exists
//! is how simulations end up testing a paraphrase of the system instead of
//! the system (ethos rule 7).

pub mod compaction;
pub mod context;
pub mod events;
pub mod memory_resupply;
pub mod runtime;
pub mod scan;
pub mod sim;
pub mod verify;

use chrono::{DateTime, Utc};

/// The orchestrator's only source of time. Production uses [`SystemClock`];
/// simulation uses [`sim::FakeClock`] and advances it explicitly. Nothing in
/// the orchestrator may call `Utc::now()` directly — that one rule is what
/// makes "replay the same event sequence, get the same state" possible.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}
