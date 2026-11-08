//! Shared worker messaging authority. This check has no permission exceptions:
//! shared membership, self input, and direct owner input are the only controls.
//! A worker's own Chat tab (`<worker>@chat`) sending to that worker is self
//! input: it is the owner talking to the worker through another surface. It
//! reaches no other worker this way (Ethan, 2026-10-10: the chat said it was
//! blocked from sending to its own isolated worker).
//!
//! OPEN GROUPS (Ethan, 2026-10-13: "special groups that all workers can send
//! messages to (but not vice versa)"). A group whose own scope sets
//! `AMUX_GROUP_OPEN_INBOUND=1` takes input from any worker. It is read from the
//! group's env file only (`~/.amux/env/<group>.env`), so a worker env or the
//! global layer cannot open every lane. The opening is one-way: a member of an
//! open group still messages only workers it shares a group with.

#[derive(Clone, Debug)]
pub(crate) struct PeerInputRefusal {
    pub code: &'static str,
    pub reason: String,
}

pub(crate) const OPEN_INBOUND_KEY: &str = "AMUX_GROUP_OPEN_INBOUND";

/// Which of `groups` are open to input from any worker, read from each group's
/// own env file and nowhere else.
pub(crate) fn open_inbound_groups(
    home: &std::path::Path, groups: &std::collections::BTreeSet<String>,
) -> std::collections::BTreeSet<String> {
    groups.iter().filter(|g| group_is_open_inbound(home, g)).cloned().collect()
}

pub(crate) fn group_is_open_inbound(home: &std::path::Path, group: &str) -> bool {
    if group.is_empty() || group.contains('/') || group.contains("..") { return false; }
    super::session_verbs::EnvFile::load(&home.join("env").join(format!("{group}.env")))
        .get(OPEN_INBOUND_KEY)
        .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// Every group configured open, for the roster and the policy endpoint.
pub(crate) fn all_open_inbound_groups(home: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(home.join("env")).into_iter().flatten().flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".env")).map(str::to_string))
        .filter(|g| group_is_open_inbound(home, g))
        .collect();
    out.sort();
    out
}

thread_local! {
    static QUIET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` with the group check's log lines off. A roster asks the send check
/// about every worker to decide whom to list, and logging each "no" as a
/// refused message wrote 4,027 WARN lines in one boot's first minute, for
/// messages nobody sent (AF-1161).
pub(crate) fn without_refusal_logs<T>(f: impl FnOnce() -> T) -> T {
    let was = QUIET.with(|q| q.replace(true));
    let out = f();
    QUIET.with(|q| q.set(was));
    out
}

fn logging() -> bool {
    !QUIET.with(|q| q.get())
}

pub(crate) fn worker_group_refusal(
    origin: &str, target: &str,
    origin_groups: &std::collections::BTreeSet<String>,
    target_groups: &std::collections::BTreeSet<String>,
    target_open_groups: &std::collections::BTreeSet<String>,
) -> Option<PeerInputRefusal> {
    if origin.is_empty() || origin == target || !origin_groups.is_disjoint(target_groups) { return None; }
    if !target_open_groups.is_empty() {
        if logging() { tracing::info!(origin, target, ?target_open_groups, verdict = "open_group_inbound",
            measured = true, n_considered = 1,
            "worker input allowed: the target is in a group open to every worker"); }
        return None;
    }
    if super::chat_worker::companion_parent(origin) == Some(target) {
        if logging() { tracing::info!(origin, target, verdict = "chat_tab_self_input", measured = true, n_considered = 1,
            "a worker's own Chat tab sent to it: self input, not a cross-group message"); }
        return None;
    }
    if logging() {
        tracing::warn!(origin, target, ?origin_groups, ?target_groups,
            verdict = "worker_group_boundary", measured = true, n_considered = 1,
            "worker input refused outside a shared group");
    }
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
    fn a_quiet_check_gives_the_same_answer_and_restores_logging() {
        let own = ["a".to_string()].into_iter().collect();
        let other = ["b".to_string()].into_iter().collect();
        let shut = Default::default();
        let loud = worker_group_refusal("x", "y", &own, &other, &shut).map(|r| r.code);
        let quiet = without_refusal_logs(|| {
            assert!(!logging());
            worker_group_refusal("x", "y", &own, &other, &shut).map(|r| r.code)
        });
        assert_eq!(loud, quiet, "silencing the log must not change the verdict");
        assert!(logging(), "logging is back on after the quiet call");
    }

    #[test]
    fn shared_membership_is_required_and_multi_group_hubs_remain_reachable() {
        let own = ["gs12-platform".to_string()].into_iter().collect();
        let hub = ["ops".to_string(), "gs12-platform".to_string()].into_iter().collect();
        let other = ["amux".to_string()].into_iter().collect();
        let shut = Default::default();
        assert!(worker_group_refusal("source", "hub", &own, &hub, &shut).is_none());
        assert_eq!(worker_group_refusal("source", "outside", &own, &other, &shut).unwrap().code,"worker_group_boundary");
        assert!(worker_group_refusal("", "outside", &own, &other, &shut).is_none());
        assert!(worker_group_refusal("source", "source", &own, &other, &shut).is_none());
        assert!(worker_group_refusal("retired", "outside", &Default::default(), &other, &shut).is_some());
    }

    /// The live refusal: mxp-gs12@chat (no groups) -> mxp-gs12 (gs12) was
    /// worker_group_boundary, so the Chat tab could not reach its own worker.
    #[test]
    fn a_workers_own_chat_tab_reaches_it_and_no_one_else() {
        let none = Default::default();
        let gs12 = ["gs12".to_string()].into_iter().collect();
        let shut = Default::default();
        assert!(worker_group_refusal("mxp-gs12@chat", "mxp-gs12", &none, &gs12, &shut).is_none());
        assert!(worker_group_refusal("mxp-gs12@chat", "other-lane", &none, &gs12, &shut).is_some(),
            "the chat tab must not become a route to any other worker");
        assert!(worker_group_refusal("other@chat", "mxp-gs12", &none, &gs12, &shut).is_some(),
            "another worker's chat tab is not self input");
    }

    /// An open group takes input from any worker; its members still reach only
    /// workers they share a group with.
    #[test]
    fn an_open_group_takes_input_from_anyone_and_stays_one_way() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("env")).unwrap();
        std::fs::write(home.path().join("env/amux.env"), format!("{OPEN_INBOUND_KEY}=1\n")).unwrap();
        std::fs::write(home.path().join("env/gtm.env"), "OTHER=1\n").unwrap();
        let amux: std::collections::BTreeSet<String> = ["amux".to_string()].into_iter().collect();
        let gtm: std::collections::BTreeSet<String> = ["gtm".to_string()].into_iter().collect();
        let amux_open = open_inbound_groups(home.path(), &amux);
        let gtm_open = open_inbound_groups(home.path(), &gtm);
        assert_eq!(amux_open, amux);
        assert!(gtm_open.is_empty());
        assert!(worker_group_refusal("gtm-engine", "amux", &gtm, &amux, &amux_open).is_none(),
            "any worker reaches a member of an open group");
        assert_eq!(worker_group_refusal("amux", "gtm-engine", &amux, &gtm, &gtm_open).unwrap().code,
            "worker_group_boundary", "the opening is one-way");
        assert_eq!(all_open_inbound_groups(home.path()), vec!["amux".to_string()]);
        assert!(!group_is_open_inbound(home.path(), "../amux"));
    }
}
