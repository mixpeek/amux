//! Shared worker messaging authority. This check has no permission exceptions:
//! shared membership, self input, and direct owner input are the only controls.
//! A worker's own Chat tab (`<worker>@chat`) sending to that worker is self
//! input: it is the owner talking to the worker through another surface. It
//! reaches no other worker this way (Ethan, 2026-10-10: the chat said it was
//! blocked from sending to its own isolated worker).

#[derive(Clone, Debug)]
pub(crate) struct PeerInputRefusal {
    pub code: &'static str,
    pub reason: String,
}

pub(crate) fn worker_group_refusal(
    origin: &str, target: &str,
    origin_groups: &std::collections::BTreeSet<String>,
    target_groups: &std::collections::BTreeSet<String>,
) -> Option<PeerInputRefusal> {
    if origin.is_empty() || origin == target || !origin_groups.is_disjoint(target_groups) { return None; }
    if super::chat_worker::companion_parent(origin) == Some(target) {
        tracing::info!(origin, target, verdict = "chat_tab_self_input", measured = true, n_considered = 1,
            "a worker's own Chat tab sent to it: self input, not a cross-group message");
        return None;
    }
    tracing::warn!(origin, target, ?origin_groups, ?target_groups,
        verdict = "worker_group_boundary", measured = true, n_considered = 1,
        "worker input refused outside a shared group");
    Some(PeerInputRefusal { code: "worker_group_boundary", reason: format!(
        "worker group boundary refused: {origin} [{origin_groups:?}] -> {target} [{target_groups:?}]. \
         Worker messages must stay within a shared group. Do not retry or reroute to another \
         outside-group worker. File harness bugs in the Amux repository's frustrations.md \
         with a linked amux-frustrations card and evidence. Owner input remains permitted.") })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_membership_is_required_and_multi_group_hubs_remain_reachable() {
        let own = ["gs12-platform".to_string()].into_iter().collect();
        let hub = ["ops".to_string(), "gs12-platform".to_string()].into_iter().collect();
        let other = ["amux".to_string()].into_iter().collect();
        assert!(worker_group_refusal("source", "hub", &own, &hub).is_none());
        assert_eq!(worker_group_refusal("source", "outside", &own, &other).unwrap().code,"worker_group_boundary");
        assert!(worker_group_refusal("", "outside", &own, &other).is_none());
        assert!(worker_group_refusal("source", "source", &own, &other).is_none());
        assert!(worker_group_refusal("retired", "outside", &Default::default(), &other).is_some());
    }

    /// The live refusal: mxp-gs12@chat (no groups) -> mxp-gs12 (gs12) was
    /// worker_group_boundary, so the Chat tab could not reach its own worker.
    #[test]
    fn a_workers_own_chat_tab_reaches_it_and_no_one_else() {
        let none = Default::default();
        let gs12 = ["gs12".to_string()].into_iter().collect();
        assert!(worker_group_refusal("mxp-gs12@chat", "mxp-gs12", &none, &gs12).is_none());
        assert!(worker_group_refusal("mxp-gs12@chat", "other-lane", &none, &gs12).is_some(),
            "the chat tab must not become a route to any other worker");
        assert!(worker_group_refusal("other@chat", "mxp-gs12", &none, &gs12).is_some(),
            "another worker's chat tab is not self input");
    }
}
