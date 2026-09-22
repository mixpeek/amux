//! Provider capacity routing (RR-0044, Invariant 20). PURE logic: no
//! adapters, no I/O, no clock — a decision function over usage snapshots the
//! caller already holds, so every branch is unit-testable and the scheduler
//! that calls it stays deterministic.
//!
//! Two rules the plan states outright, encoded here so they cannot be
//! re-litigated at a call site:
//!
//! - **Never silently swap the configured provider.** When policy forbids
//!   failover, an exhausted primary yields [`RouteDecision::Blocked`] with the
//!   exhausted window NAMED — the user configured that provider (ethos rule
//!   8: their decision), and a swap they did not sanction is worse than a
//!   visible stop.
//! - **Never route on invented data.** Exhaustion requires REAL numbers:
//!   both `used` and `limit` present, confidence Exact or Approximate.
//!   Unknown carries no numbers at all (Invariant 20) and Stale numbers have
//!   drifted — neither may take a provider out of rotation. An unknown
//!   provider therefore routes as Primary, and a wrong guess would have been
//!   undetectable from the data we keep (ethos rule 4).

use std::collections::BTreeMap;

use amux_core::provider::{ProviderId, ProviderUsage, UsageConfidence, UsageWindow};

/// The user's failover policy. `fallback_chain` is ORDERED preference; it is
/// only consulted when `allow_failover` is true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingPolicy {
    pub allow_failover: bool,
    pub fallback_chain: Vec<ProviderId>,
}

/// Where the work goes. `Failover`/`Blocked` carry prose because the decision
/// must be explainable wherever it surfaces (board card, log line, API) —
/// a silent reroute is the failure mode this module exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteDecision {
    /// Use the configured provider.
    Primary,
    /// Primary is exhausted and policy allows failover: use `to`.
    Failover { to: ProviderId, because: String },
    /// Nothing routable — the reason names the exhausted window(s).
    Blocked { reason: String },
}

/// The first window that PROVES exhaustion: `used >= limit` with both sides
/// reported and trustworthy confidence. This is deliberately the only way a
/// provider counts as exhausted — absence of data is absence of evidence.
fn exhausted_window(usage: &ProviderUsage) -> Option<&UsageWindow> {
    usage.windows.iter().find(|w| {
        matches!(
            w.confidence,
            UsageConfidence::Exact | UsageConfidence::Approximate
        ) && matches!((w.used, w.limit), (Some(used), Some(limit)) if used >= limit)
    })
}

/// True when we have positive evidence the provider is out of capacity.
/// Missing usage, zero windows, Unknown/Stale confidence: NOT exhausted.
fn is_exhausted(usages: &BTreeMap<ProviderId, ProviderUsage>, id: &ProviderId) -> bool {
    usages.get(id).and_then(exhausted_window).is_some()
}

/// Human-readable description of an exhausted window, for `because`/`reason`.
fn describe_window(id: &ProviderId, w: &UsageWindow) -> String {
    let used = w.used.unwrap_or(0);
    let limit = w.limit.unwrap_or(0);
    match w.resets_at {
        Some(reset) => format!(
            "{id} {:?} window exhausted ({used}/{limit}, resets {})",
            w.window_kind,
            reset.to_rfc3339()
        ),
        None => format!("{id} {:?} window exhausted ({used}/{limit})", w.window_kind),
    }
}

/// Decide where work configured for `configured` should run, given the usage
/// snapshots in hand and the user's policy.
pub fn route(
    configured: &ProviderId,
    usages: &BTreeMap<ProviderId, ProviderUsage>,
    policy: &RoutingPolicy,
) -> RouteDecision {
    // No positive evidence of exhaustion -> the configured provider runs.
    // This branch is also the Unknown-usage path: never route on invented
    // data, so unknown passes through to Primary.
    let Some(window) = usages.get(configured).and_then(exhausted_window) else {
        return RouteDecision::Primary;
    };
    let exhausted_desc = describe_window(configured, window);

    if !policy.allow_failover {
        // The plan is explicit: never silently change the configured
        // provider. Blocked, naming the window, so the stop is visible and
        // attributable to real reported numbers.
        return RouteDecision::Blocked {
            reason: format!("{exhausted_desc}; failover disabled by policy"),
        };
    }

    // First non-exhausted provider in the ordered chain. The configured
    // provider may legally appear in the chain; it is exhausted, so it simply
    // never matches. A chain entry with unknown usage IS eligible — unknown
    // is not evidence of exhaustion (the failover itself will surface any
    // real limit, which is a detectable outcome rather than a guess).
    for candidate in &policy.fallback_chain {
        if !is_exhausted(usages, candidate) {
            return RouteDecision::Failover {
                to: candidate.clone(),
                because: exhausted_desc,
            };
        }
    }

    RouteDecision::Blocked {
        reason: format!(
            "{exhausted_desc}; no non-exhausted provider in fallback chain ({})",
            if policy.fallback_chain.is_empty() {
                "chain is empty".to_string()
            } else {
                policy
                    .fallback_chain
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amux_core::provider::{UsageProvenance, UsageWindow, UsageWindowKind};

    fn pid(s: &str) -> ProviderId {
        ProviderId::new(s)
    }

    fn window(used: u64, limit: u64, confidence: UsageConfidence) -> UsageWindow {
        UsageWindow {
            window_kind: UsageWindowKind::Weekly,
            used: Some(used),
            limit: Some(limit),
            resets_at: None,
            confidence,
            provenance: UsageProvenance::Api,
        }
    }

    fn usage(provider: &str, windows: Vec<UsageWindow>) -> (ProviderId, ProviderUsage) {
        (pid(provider), ProviderUsage::new(pid(provider), windows))
    }

    fn policy(allow_failover: bool, chain: &[&str]) -> RoutingPolicy {
        RoutingPolicy {
            allow_failover,
            fallback_chain: chain.iter().map(|s| pid(s)).collect(),
        }
    }

    // -- capacity has to be real to count -----------------------------------

    #[test]
    fn healthy_primary_routes_primary() {
        let usages = BTreeMap::from([usage(
            "claude-code",
            vec![window(40, 100, UsageConfidence::Exact)],
        )]);
        let d = route(&pid("claude-code"), &usages, &policy(true, &["codex"]));
        assert_eq!(d, RouteDecision::Primary);
    }

    #[test]
    fn unknown_usage_passes_through_to_primary() {
        // Zero-window unknown usage AND a completely absent snapshot both
        // route Primary: no invented exhaustion (Invariant 20).
        let usages = BTreeMap::from([(
            pid("claude-code"),
            ProviderUsage::unknown(pid("claude-code")),
        )]);
        assert_eq!(
            route(&pid("claude-code"), &usages, &policy(true, &["codex"])),
            RouteDecision::Primary
        );
        assert_eq!(
            route(
                &pid("never-probed"),
                &BTreeMap::new(),
                &policy(true, &["codex"])
            ),
            RouteDecision::Primary
        );
    }

    #[test]
    fn exhausted_numbers_with_unknown_or_stale_confidence_do_not_count() {
        // A window CLAIMING 100/100 but with confidence too weak to act on:
        // still Primary. (Unknown windows carry no numbers by contract, but
        // routing must not trust them even if one shows up malformed; Stale
        // numbers have drifted.)
        for confidence in [UsageConfidence::Unknown, UsageConfidence::Stale] {
            let usages = BTreeMap::from([usage("claude-code", vec![window(100, 100, confidence)])]);
            assert_eq!(
                route(&pid("claude-code"), &usages, &policy(true, &["codex"])),
                RouteDecision::Primary,
                "confidence {confidence:?} must never prove exhaustion"
            );
        }
    }

    #[test]
    fn approximate_confidence_does_count() {
        let usages = BTreeMap::from([
            usage(
                "claude-code",
                vec![window(100, 100, UsageConfidence::Approximate)],
            ),
            usage("codex", vec![window(10, 100, UsageConfidence::Exact)]),
        ]);
        let d = route(&pid("claude-code"), &usages, &policy(true, &["codex"]));
        assert!(matches!(d, RouteDecision::Failover { ref to, .. } if *to == pid("codex")));
    }

    #[test]
    fn over_limit_counts_as_exhausted() {
        // used > limit is a legal, real state — and it is exhausted.
        let usages = BTreeMap::from([
            usage(
                "claude-code",
                vec![window(130, 100, UsageConfidence::Exact)],
            ),
            usage("codex", vec![]),
        ]);
        let d = route(&pid("claude-code"), &usages, &policy(true, &["codex"]));
        assert!(matches!(d, RouteDecision::Failover { .. }));
    }

    #[test]
    fn one_exhausted_window_among_healthy_ones_exhausts_the_provider() {
        // 5h window fine, weekly window done -> the provider is done: any
        // real cap that is hit stops work regardless of the others.
        let five_hour_ok = UsageWindow {
            window_kind: UsageWindowKind::Rolling,
            ..window(12, 100, UsageConfidence::Exact)
        };
        let usages = BTreeMap::from([
            usage(
                "claude-code",
                vec![five_hour_ok, window(100, 100, UsageConfidence::Exact)],
            ),
            usage("codex", vec![window(1, 100, UsageConfidence::Exact)]),
        ]);
        let d = route(&pid("claude-code"), &usages, &policy(true, &["codex"]));
        assert!(matches!(d, RouteDecision::Failover { .. }));
    }

    // -- failover simulation -------------------------------------------------

    #[test]
    fn failover_picks_first_non_exhausted_in_chain_order() {
        let usages = BTreeMap::from([
            usage(
                "claude-code",
                vec![window(100, 100, UsageConfidence::Exact)],
            ),
            usage("gemini", vec![window(100, 100, UsageConfidence::Exact)]),
            usage("codex", vec![window(50, 100, UsageConfidence::Exact)]),
            usage("ollama", vec![]),
        ]);
        // gemini is first in the chain but exhausted -> codex.
        let d = route(
            &pid("claude-code"),
            &usages,
            &policy(true, &["gemini", "codex", "ollama"]),
        );
        match d {
            RouteDecision::Failover { to, because } => {
                assert_eq!(to, pid("codex"));
                // The reason names the primary's exhausted window: the swap
                // must be explainable wherever it surfaces.
                assert!(because.contains("claude-code"), "because = {because}");
                assert!(because.contains("100/100"), "because = {because}");
            }
            other => panic!("expected Failover, got {other:?}"),
        }
    }

    #[test]
    fn unknown_usage_fallback_is_eligible() {
        // A fallback we know nothing about may be tried: unknown is not
        // exhausted, and trying it produces evidence where guessing cannot.
        let usages = BTreeMap::from([usage(
            "claude-code",
            vec![window(100, 100, UsageConfidence::Exact)],
        )]);
        let d = route(&pid("claude-code"), &usages, &policy(true, &["ollama"]));
        assert!(matches!(d, RouteDecision::Failover { ref to, .. } if *to == pid("ollama")));
    }

    #[test]
    fn configured_provider_in_its_own_chain_is_not_retried() {
        // Common config shape: chain lists every provider incl. the primary.
        // The exhausted primary never matches itself back in.
        let usages = BTreeMap::from([
            usage(
                "claude-code",
                vec![window(100, 100, UsageConfidence::Exact)],
            ),
            usage("codex", vec![window(0, 100, UsageConfidence::Exact)]),
        ]);
        let d = route(
            &pid("claude-code"),
            &usages,
            &policy(true, &["claude-code", "codex"]),
        );
        assert!(matches!(d, RouteDecision::Failover { ref to, .. } if *to == pid("codex")));
    }

    #[test]
    fn everything_exhausted_blocks_with_the_full_story() {
        let usages = BTreeMap::from([
            usage(
                "claude-code",
                vec![window(100, 100, UsageConfidence::Exact)],
            ),
            usage("codex", vec![window(200, 100, UsageConfidence::Exact)]),
        ]);
        let d = route(&pid("claude-code"), &usages, &policy(true, &["codex"]));
        match d {
            RouteDecision::Blocked { reason } => {
                assert!(reason.contains("claude-code"), "reason = {reason}");
                assert!(reason.contains("codex"), "reason = {reason}");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn empty_chain_with_failover_allowed_blocks() {
        let usages = BTreeMap::from([usage(
            "claude-code",
            vec![window(100, 100, UsageConfidence::Exact)],
        )]);
        let d = route(&pid("claude-code"), &usages, &policy(true, &[]));
        assert!(matches!(d, RouteDecision::Blocked { .. }));
    }

    // -- policy enforcement --------------------------------------------------

    #[test]
    fn failover_forbidden_blocks_and_names_the_window() {
        // THE plan-mandated behavior: never silently swap the configured
        // provider. Healthy codex sits right there in the chain; policy says
        // no; the answer is a visible Blocked naming the exhausted window.
        let reset = chrono::DateTime::parse_from_rfc3339("2026-08-13T07:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let mut w = window(100, 100, UsageConfidence::Exact);
        w.resets_at = Some(reset);
        let usages = BTreeMap::from([
            usage("claude-code", vec![w]),
            usage("codex", vec![window(0, 100, UsageConfidence::Exact)]),
        ]);
        let d = route(&pid("claude-code"), &usages, &policy(false, &["codex"]));
        match d {
            RouteDecision::Blocked { reason } => {
                assert!(reason.contains("claude-code"), "reason = {reason}");
                assert!(reason.contains("Weekly"), "reason = {reason}");
                assert!(reason.contains("100/100"), "reason = {reason}");
                assert!(reason.contains("2026-08-13"), "reason = {reason}");
                assert!(reason.contains("failover disabled"), "reason = {reason}");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn failover_forbidden_with_healthy_primary_is_still_primary() {
        // Policy only bites when the primary is actually exhausted.
        let usages = BTreeMap::from([usage(
            "claude-code",
            vec![window(99, 100, UsageConfidence::Exact)],
        )]);
        assert_eq!(
            route(&pid("claude-code"), &usages, &policy(false, &[])),
            RouteDecision::Primary
        );
    }
}
