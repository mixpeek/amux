//! Fleet-level circuit breakers (Invariants 48 + 45 + 10, RR-0028h).
//!
//! Per-task limits ([`crate::limits`], Invariant 47) stop one task from
//! thrashing; this module stops the FLEET: spend rate, progress rate, error
//! rate, and the every-item-blocked terminal event are watched at fleet
//! grain, and tripping any of them halts new assignments.
//!
//! **The critical interaction (Invariant 10 + 48):** the no-stall guarantee
//! calls an idle worker with runnable tasks a system failure — but a tripped
//! breaker DELIBERATELY produces idle workers with runnable tasks; that is
//! its job. Without [`stall_check_enabled`] returning `false` during
//! `CircuitOpen` AND `Reconciling`, the stall-fixer would fight the breaker:
//! the breaker halts, the stall check restarts, forever — a livelock in
//! which the emergency brake is the thing burning tokens. The no-stall
//! invariant is SUSPENDED while the circuit is open.
//!
//! `AllItemsBlocked` is the single owner of the "every remaining item is
//! blocked" terminal event (shared with Invariant 45's agent loop, which
//! enters reconciliation here rather than producing its own competing
//! terminal report — one owner, Invariant 36).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Why the circuit opened. Each variant carries the numbers that tripped it
/// — a reason that cannot say WHICH threshold, by HOW much, is a diagnosis
/// the data cannot express (ethos rule 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CircuitOpenReason {
    /// Token spend in the rolling window reached the budget.
    SpendRateExceeded { window_tokens: u64, budget: u64 },
    /// A full window elapsed with fewer completions than required.
    NoProgress { window_secs: u64 },
    /// Failures in the window reached the cap.
    ErrorRateExceeded { failures: u32, window_secs: u64 },
    /// Every remaining item is blocked — the terminal event this module
    /// owns for Invariant 45 as well.
    AllItemsBlocked,
    /// A human pulled the brake. Never produced by [`FleetCircuitBreaker::
    /// evaluate`] — stopping the fleet by hand is the human's call to make,
    /// not something core infers (ethos rule 8).
    ManualStop,
}

/// The fleet's operating state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FleetState {
    Normal,
    /// Assignments halted; diagnostic report written.
    CircuitOpen {
        reason: CircuitOpenReason,
        opened_at: DateTime<Utc>,
    },
    /// Low-power loop: re-evaluate blocked items, audit progress, close the
    /// circuit if runnable work is found (Invariant 48 steps 3-4).
    Reconciling {
        since: DateTime<Utc>,
    },
}

impl FleetState {
    /// Open the circuit. The timestamp is a parameter — core is pure and
    /// never reads a clock.
    pub fn open(reason: CircuitOpenReason, at: DateTime<Utc>) -> FleetState {
        FleetState::CircuitOpen {
            reason,
            opened_at: at,
        }
    }

    /// `CircuitOpen` or `Reconciling` — the states in which the fleet is
    /// deliberately not assigning work.
    pub fn is_emergency(&self) -> bool {
        matches!(
            self,
            FleetState::CircuitOpen { .. } | FleetState::Reconciling { .. }
        )
    }

    /// Enter the reconciliation loop. Legal only from `CircuitOpen` —
    /// reconciliation is the step AFTER a trip (Invariant 48 step 3), not a
    /// steady-state mode, so `None` from `Normal`/`Reconciling` refuses an
    /// impossible transition instead of silently rewriting history.
    pub fn begin_reconciling(&self, at: DateTime<Utc>) -> Option<FleetState> {
        match self {
            FleetState::CircuitOpen { .. } => Some(FleetState::Reconciling { since: at }),
            _ => None,
        }
    }

    /// Close the circuit: reconciliation found runnable work (Invariant 48
    /// step 4). `None` from `Normal` — closing a closed circuit is a bug in
    /// the caller worth surfacing, not a no-op to swallow.
    pub fn close(&self) -> Option<FleetState> {
        match self {
            FleetState::CircuitOpen { .. } | FleetState::Reconciling { .. } => {
                Some(FleetState::Normal)
            }
            FleetState::Normal => None,
        }
    }
}

/// One rolling window's observations, measured by the store — core only
/// judges them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowStats {
    pub tokens_spent: u64,
    pub tasks_completed: u32,
    pub failures: u32,
    pub all_items_blocked: bool,
    /// At least one non-terminal task exists. An empty queue is healthy, not
    /// evidence that a progress-producing fleet is stalled.
    pub has_live_work: bool,
    /// The store contains observations spanning a complete window. Without
    /// this guard, a newly started server trips the no-progress breaker before
    /// it has had a fair chance to complete anything.
    pub window_elapsed: bool,
}

/// Fleet-level thresholds. All windows share `window_secs`; the breaker is
/// deliberately simple — the structurally-absent signal (no completions, all
/// blocked) over tuned parameters wherever possible (ethos rule 7: a
/// threshold below the baseline is not a detector).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetCircuitBreaker {
    /// Tokens allowed per rolling window.
    pub window_budget_tokens: u64,
    /// Window length in seconds.
    pub window_secs: u64,
    /// Minimum completions per window; `0` disables the no-progress trip.
    pub min_progress_per_window: u32,
    /// Failure cap per window; reaching it trips the circuit.
    pub max_failures_per_window: u32,
}

impl FleetCircuitBreaker {
    /// Judge one window. `Some(reason)` means the circuit must open. Pure —
    /// no clocks, no counters; the store measures, this decides.
    ///
    /// Checked in diagnostic-specificity order, first match wins:
    /// spend (hard budget) -> all-blocked (structural) -> error rate ->
    /// no-progress (the least specific: everything else also shows up as no
    /// progress, so it goes last to avoid masking the real discriminator).
    pub fn evaluate(&self, window: &WindowStats) -> Option<CircuitOpenReason> {
        if self.window_budget_tokens > 0 && window.tokens_spent >= self.window_budget_tokens {
            return Some(CircuitOpenReason::SpendRateExceeded {
                window_tokens: window.tokens_spent,
                budget: self.window_budget_tokens,
            });
        }
        if window.all_items_blocked {
            return Some(CircuitOpenReason::AllItemsBlocked);
        }
        if self.max_failures_per_window > 0 && window.failures >= self.max_failures_per_window {
            return Some(CircuitOpenReason::ErrorRateExceeded {
                failures: window.failures,
                window_secs: self.window_secs,
            });
        }
        if window.has_live_work
            && window.window_elapsed
            && window.tasks_completed < self.min_progress_per_window
        {
            return Some(CircuitOpenReason::NoProgress {
                window_secs: self.window_secs,
            });
        }
        None
    }

    /// [`evaluate`](Self::evaluate) + transition: the state to move to, if
    /// the window trips. `at` stamps `opened_at` — this is where the
    /// evaluation instant is actually consumed, which is why `evaluate`
    /// itself does not take a clock.
    pub fn trip(&self, window: &WindowStats, at: DateTime<Utc>) -> Option<FleetState> {
        self.evaluate(window)
            .map(|reason| FleetState::open(reason, at))
    }

    /// Whether an emergency state may close on this observation.
    ///
    /// A no-progress or all-blocked trip cannot require a completion before
    /// recovery: while open, the planner deliberately makes no assignments,
    /// so that requirement would deadlock the fleet. Runnable work returning
    /// is enough to permit one probe assignment. Spend/error trips still wait
    /// for their measured window to clear, and a manual stop never auto-closes.
    pub fn can_recover(&self, state: &FleetState, window: &WindowStats) -> bool {
        match state {
            FleetState::Normal => false,
            FleetState::CircuitOpen {
                reason: CircuitOpenReason::ManualStop,
                ..
            } => false,
            FleetState::CircuitOpen {
                reason: CircuitOpenReason::AllItemsBlocked | CircuitOpenReason::NoProgress { .. },
                ..
            } if window.has_live_work && !window.all_items_blocked => matches!(
                self.evaluate(window),
                None | Some(CircuitOpenReason::NoProgress { .. })
            ),
            FleetState::CircuitOpen { .. } | FleetState::Reconciling { .. } => {
                self.evaluate(window).is_none()
            }
        }
    }
}

/// Whether the no-stall check (Invariant 10) may run. `false` during
/// `CircuitOpen` and `Reconciling`: the breaker deliberately idles workers
/// with runnable tasks, and a stall-fixer that cannot see the emergency
/// would fight the brake — restart what the breaker halted, which the
/// breaker re-halts, forever (Invariant 10 + 48 interaction; see module
/// docs). One predicate, consulted by the stall detector, so the suspension
/// cannot drift out of step with the state enum.
pub fn stall_check_enabled(state: &FleetState) -> bool {
    !state.is_emergency()
}

/// The durable record of a policy-level judgment call: what was decided,
/// what was chosen over what, why, and whether it can be undone. Written on
/// events like the reviewer round cap (RR-0028i,
/// `review_rounds_exhausted: true`) so the judgment is auditable — an
/// unaudited escape hatch that claims to be audited is the exact defect of
/// ethos rule 6.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecision {
    /// What was decided (machine-readable slug, e.g.
    /// `criteria_review_rounds_exhausted`).
    pub decision: String,
    /// The option taken.
    pub chosen: String,
    /// Why — including what was rejected. Dead hypotheses are evidence
    /// (ethos rule 7).
    pub rationale: String,
    pub reversible: bool,
    pub at: DateTime<Utc>,
    /// Set by the RR-0028i reviewer cap: the criteria were accepted because
    /// review rounds ran out, not because the reviewer was satisfied.
    /// Defaults to `false` so ordinary decisions do not carry the flag.
    #[serde(default)]
    pub review_rounds_exhausted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> DateTime<Utc> {
        "2026-08-01T00:00:00Z".parse().unwrap()
    }

    fn breaker() -> FleetCircuitBreaker {
        FleetCircuitBreaker {
            window_budget_tokens: 1_000,
            window_secs: 14_400,
            min_progress_per_window: 1,
            max_failures_per_window: 5,
        }
    }

    fn healthy() -> WindowStats {
        WindowStats {
            tokens_spent: 500,
            tasks_completed: 3,
            failures: 1,
            all_items_blocked: false,
            has_live_work: true,
            window_elapsed: true,
        }
    }

    #[test]
    fn healthy_window_does_not_trip() {
        assert_eq!(breaker().evaluate(&healthy()), None);
    }

    #[test]
    fn spend_trips_at_budget_not_below() {
        let mut w = healthy();
        w.tokens_spent = 999;
        assert_eq!(breaker().evaluate(&w), None);
        w.tokens_spent = 1_000;
        assert_eq!(
            breaker().evaluate(&w),
            Some(CircuitOpenReason::SpendRateExceeded {
                window_tokens: 1_000,
                budget: 1_000
            })
        );
        let mut disabled = breaker();
        disabled.window_budget_tokens = 0;
        assert_eq!(disabled.evaluate(&w), None);
    }

    #[test]
    fn no_progress_trips_below_minimum_not_at_it() {
        let mut w = healthy();
        w.tasks_completed = 1;
        assert_eq!(breaker().evaluate(&w), None);
        w.tasks_completed = 0;
        assert_eq!(
            breaker().evaluate(&w),
            Some(CircuitOpenReason::NoProgress {
                window_secs: 14_400
            })
        );

        // min 0 disables the no-progress trip entirely.
        let mut b = breaker();
        b.min_progress_per_window = 0;
        assert_eq!(b.evaluate(&w), None);

        let mut warming = healthy();
        warming.tasks_completed = 0;
        warming.window_elapsed = false;
        assert_eq!(breaker().evaluate(&warming), None);

        let mut empty = healthy();
        empty.tasks_completed = 0;
        empty.has_live_work = false;
        assert_eq!(breaker().evaluate(&empty), None);
    }

    #[test]
    fn error_rate_trips_at_cap_not_below() {
        let mut w = healthy();
        w.failures = 4;
        assert_eq!(breaker().evaluate(&w), None);
        w.failures = 5;
        assert_eq!(
            breaker().evaluate(&w),
            Some(CircuitOpenReason::ErrorRateExceeded {
                failures: 5,
                window_secs: 14_400
            })
        );
        let mut disabled = breaker();
        disabled.max_failures_per_window = 0;
        assert_eq!(disabled.evaluate(&w), None);
    }

    #[test]
    fn all_items_blocked_trips() {
        let mut w = healthy();
        w.all_items_blocked = true;
        assert_eq!(
            breaker().evaluate(&w),
            Some(CircuitOpenReason::AllItemsBlocked)
        );
    }

    #[test]
    fn trip_stamps_opened_at_and_transitions() {
        let mut w = healthy();
        w.all_items_blocked = true;
        let state = breaker().trip(&w, t0()).unwrap();
        assert_eq!(
            state,
            FleetState::CircuitOpen {
                reason: CircuitOpenReason::AllItemsBlocked,
                opened_at: t0()
            }
        );
        assert_eq!(breaker().trip(&healthy(), t0()), None);
    }

    #[test]
    fn state_transition_helpers() {
        let open = FleetState::open(CircuitOpenReason::ManualStop, t0());

        // Normal cannot begin reconciling or close.
        assert_eq!(FleetState::Normal.begin_reconciling(t0()), None);
        assert_eq!(FleetState::Normal.close(), None);

        // Open -> Reconciling -> Normal.
        let rec = open.begin_reconciling(t0()).unwrap();
        assert_eq!(rec, FleetState::Reconciling { since: t0() });
        assert_eq!(rec.close(), Some(FleetState::Normal));
        // Open can also close directly (manual re-close).
        assert_eq!(open.close(), Some(FleetState::Normal));
        // Reconciling cannot re-enter reconciling.
        assert_eq!(rec.begin_reconciling(t0()), None);
    }

    #[test]
    fn structural_and_progress_breakers_allow_a_probe_when_work_is_runnable() {
        let mut stalled = healthy();
        stalled.tasks_completed = 0;
        let progress_open = FleetState::open(
            CircuitOpenReason::NoProgress {
                window_secs: breaker().window_secs,
            },
            t0(),
        );
        assert!(breaker().can_recover(&progress_open, &stalled));

        let blocked_open = FleetState::open(CircuitOpenReason::AllItemsBlocked, t0());
        assert!(breaker().can_recover(&blocked_open, &stalled));

        stalled.tokens_spent = breaker().window_budget_tokens;
        assert!(
            !breaker().can_recover(&progress_open, &stalled),
            "a structural probe must not bypass a live spend breaker"
        );
        stalled.tokens_spent = 500;
        stalled.all_items_blocked = true;
        assert!(!breaker().can_recover(&blocked_open, &stalled));
    }

    #[test]
    fn measured_and_manual_breakers_do_not_close_early() {
        let mut costly = healthy();
        costly.tokens_spent = breaker().window_budget_tokens;
        let spend_open = FleetState::open(
            CircuitOpenReason::SpendRateExceeded {
                window_tokens: costly.tokens_spent,
                budget: breaker().window_budget_tokens,
            },
            t0(),
        );
        assert!(!breaker().can_recover(&spend_open, &costly));
        costly.tokens_spent = 0;
        assert!(breaker().can_recover(&spend_open, &costly));

        let manual = FleetState::open(CircuitOpenReason::ManualStop, t0());
        assert!(!breaker().can_recover(&manual, &healthy()));
        assert!(!breaker().can_recover(&FleetState::Normal, &healthy()));
    }

    #[test]
    fn stall_check_suspended_during_emergency_states() {
        // The 10+48 interaction: the stall check must not fight the breaker.
        assert!(stall_check_enabled(&FleetState::Normal));
        assert!(!stall_check_enabled(&FleetState::CircuitOpen {
            reason: CircuitOpenReason::AllItemsBlocked,
            opened_at: t0(),
        }));
        assert!(!stall_check_enabled(&FleetState::Reconciling {
            since: t0()
        }));
    }

    #[test]
    fn policy_decision_serde_and_default_flag() {
        let d = PolicyDecision {
            decision: "criteria_review_rounds_exhausted".into(),
            chosen: "accept_criteria".into(),
            rationale: "reviewer objected 3x; cap reached (Invariant 50 rule 5)".into(),
            reversible: true,
            at: t0(),
            review_rounds_exhausted: true,
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: PolicyDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);

        // Absent flag deserializes to false — ordinary decisions never
        // accidentally claim exhaustion.
        let json = format!(
            r#"{{"decision":"d","chosen":"c","rationale":"r","reversible":false,"at":"{}"}}"#,
            t0().to_rfc3339()
        );
        let back: PolicyDecision = serde_json::from_str(&json).unwrap();
        assert!(!back.review_rounds_exhausted);
    }

    #[test]
    fn fleet_state_serde_round_trip() {
        let s = FleetState::CircuitOpen {
            reason: CircuitOpenReason::SpendRateExceeded {
                window_tokens: 2_000,
                budget: 1_000,
            },
            opened_at: t0(),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: FleetState = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }
}
