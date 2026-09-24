//! Worker: the durable entity (RR-0003, RR-0018; Invariants 1, 43).
//!
//! A worker survives crashes, context exhaustion, renames, and server
//! restarts. Sessions come and go underneath it (Invariant 1); configuration
//! changes freely on top of it (Invariant 43). The one thing that never
//! changes is `WorkerId` — tasks, messages, memory, turn history, and the
//! backend process name (`session::backend_ref`) all hang off it, so mutating
//! it would orphan everything the worker has ever done.
//!
//! RR-0018 lives here too: classifying a config change into how it is applied
//! (Immediate / NextTurn / SessionRestart) and reporting the outcome, so the
//! API can tell the caller truthfully what happened — including whether a
//! session was replaced (ethos rule 6: report the swap, don't just do it).

use crate::ids::{GroupId, SessionId, TurnId, WorkerId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Everything about a worker that is allowed to change (Invariant 43:
/// configuration is mutable; identity is not). Every field here can be
/// edited without creating a new worker — some edits just cost a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// User-facing name. Mutable — renames are `Immediate` because nothing
    /// durable derives from it (backend refs derive from `WorkerId`).
    pub display_name: String,
    /// Old display names keep resolving for `@worker` addressing
    /// (Invariant 17): renaming must not break mentions written yesterday.
    pub name_aliases: Vec<String>,
    /// Where the session's process starts. Changing it means a new process:
    /// `SessionRestart`.
    pub cwd: String,
    /// Which agent provider runs the sessions. Process-level: `SessionRestart`.
    pub provider: crate::provider::ProviderId,
    /// Model override; `None` = provider default. Whether a change is
    /// `NextTurn` or `SessionRestart` depends on the provider's
    /// `hot_model_switch` capability.
    pub model: Option<String>,
    /// Which terminal backend hosts the process (Invariant 33: switching it
    /// must not alter any durable worker state; Invariant 8: open string,
    /// not a closed enum). Still `SessionRestart` — the process has to move
    /// hosts.
    pub backend: crate::session::BackendId,
    /// Worker-scope env vars (the Worker layer of `scope::LayeredMap`).
    pub environment: BTreeMap<String, String>,
    pub permissions: Vec<String>,
    /// Group membership (Invariant 12). Mutable — moving groups is an
    /// `Immediate` re-scoping, not a new worker.
    pub group: Option<GroupId>,
    /// Execution experience (ACW-1): selects the execution adapter and the
    /// primary output renderer, nothing else. Absent in older payloads and
    /// rows, which read as `coding`. Changing it swaps the adapter, so it is
    /// `SessionRestart`, like `backend`.
    #[serde(default)]
    pub worker_type: crate::worker_type::WorkerTypeId,
}

/// What a worker is able to do — used by capability matching
/// (`WaitingFor::Capability` names what was missing when no worker had it).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerCapabilities {
    pub tools: BTreeSet<String>,
    pub repositories: BTreeSet<String>,
    pub browser: bool,
    pub integrations: BTreeSet<String>,
}

/// Lifecycle state: orthogonal to execution state. A worker can be
/// `Active + Stopped`, `Paused + Idle`, or `Archived + Stopped`.
/// Execution state says what the process is doing NOW; lifecycle says
/// whether the worker SHOULD be doing anything at all.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerLifecycle {
    /// Part of the active fleet: eligible for orchestration, scheduling,
    /// messages, task assignment, and automatic driving.
    #[default]
    Active,
    /// Work is stopped; history and configuration remain available.
    /// Resume before starting or assigning new work.
    Paused,
    /// Deliberately parked/retired. Excluded from fleet discovery,
    /// scheduling, orchestration, automatic driving, and default views.
    /// History, config, and artifacts are preserved. Restorable.
    Archived,
    /// Terminal state. Removed from normal product use. Audit/tombstone
    /// data preserved for referential integrity. Never scheduled,
    /// started, messaged, or automatically restored.
    Deleted,
}

impl WorkerLifecycle {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Archived => "archived",
            Self::Deleted => "deleted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            "archived" => Some(Self::Archived),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }

    pub fn is_drivable(self) -> bool {
        matches!(self, Self::Active)
    }

    pub fn can_start(self) -> bool {
        matches!(self, Self::Active)
    }

    pub fn is_visible_default(self) -> bool {
        matches!(self, Self::Active | Self::Paused)
    }
}

impl std::fmt::Display for WorkerLifecycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Execution state (Invariant 11: always current). Distinct from task state
/// (Invariant 19) — a worker is Active/Idle, a task is doing/done.
///
/// `Idle { since }` carries its timestamp because the no-stall check
/// (Invariant 10) reports how long a stalled worker has been sitting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WorkerState {
    /// No session exists.
    Stopped,
    /// A session is spawning (also the state during a `SessionRestart` swap).
    Starting,
    /// Mid-turn (Invariant 6); `turn` is None briefly between session-up and
    /// first turn start.
    Active { turn: Option<TurnId> },
    /// Session up, nothing running. Idle + a non-terminal task = stall
    /// (Invariant 10).
    Idle { since: DateTime<Utc> },
    /// Blocked on something structured — the free-form `reason` is display
    /// text; the structured wait lives on the task (`stall::WaitingFor`).
    Waiting { reason: String },
    /// Provider throttled us. NOT idle — a rate-limited worker with pending
    /// tasks is waiting, not stalled (Invariant 10, resolution rule 1).
    RateLimited { reset_at: Option<DateTime<Utc>> },
    /// Session broken in a way that needs intervention.
    Error { detail: String },
}

/// The durable entity (Invariant 1).
///
/// `id` is PRIVATE and has no setter — this is Invariant 43 enforced by the
/// type system rather than by review. Everything durable (tasks, messages,
/// memory, turn history, backend process names, audit history) keys off
/// `WorkerId`; an API that could mutate it would let one call orphan all of
/// it. Construction (`new`) and persistence hydration (serde) are the only
/// ways an id enters a `Worker`, and no method writes it after that.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Worker {
    id: WorkerId,
    pub config: WorkerConfig,
    pub capabilities: WorkerCapabilities,
    pub state: WorkerState,
    #[serde(default)]
    pub lifecycle: WorkerLifecycle,
    /// Optimistic-concurrency version (Invariant 35): increments on every
    /// config mutation, so two concurrent editors cannot silently clobber
    /// each other.
    pub version: u64,
}

impl Worker {
    /// Mint a worker around a caller-supplied id (core is pure; the server
    /// mints ULIDs, tests pass fixed ones). Starts `Stopped` at version 0.
    pub fn new(id: WorkerId, config: WorkerConfig, capabilities: WorkerCapabilities) -> Worker {
        Worker {
            id,
            config,
            capabilities,
            state: WorkerState::Stopped,
            lifecycle: WorkerLifecycle::Active,
            version: 0,
        }
    }

    /// Read-only access to the immutable identity. There is deliberately no
    /// `set_id` / `id_mut` counterpart (Invariant 43).
    pub fn id(&self) -> &WorkerId {
        &self.id
    }
}

/// How a config change reaches the running session (RR-0018, Invariant 43).
///
/// Declaration order IS escalation order — `Ord` derives from it, and
/// `classify_config_change` combines per-field modes with `max`, so the
/// strongest requirement wins when several fields change at once.
/// Do not reorder variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigApplyMode {
    /// Applied to the running session now (rename, aliases, group,
    /// environment, permissions).
    Immediate,
    /// Applied when the current turn ends (model hot-switch on a capable
    /// provider — turn boundaries are where changes land, Invariant 6).
    NextTurn,
    /// Requires terminating and replacing the session (cwd, provider,
    /// backend, model without hot-switch). The WORKER survives; only the
    /// session is swapped (Invariant 43).
    SessionRestart,
}

/// What a config change actually did — returned to the API caller so the
/// applied mode and any session swap are reported, not inferred
/// (ethos rule 4: a wrong answer must be detectable from what we keep).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigChangeResult {
    pub mode: ConfigApplyMode,
    /// True iff a live session was atomically replaced. Both ids are then
    /// present in this ONE result — the swap is reported as a single event,
    /// never as an unpaired death and birth.
    pub session_replaced: bool,
    pub old_session: Option<SessionId>,
    pub new_session: Option<SessionId>,
}

/// Classify a config diff into the weakest apply mode that honors every
/// changed field (RR-0018; Invariant 43's classification table).
///
/// - `display_name` / `name_aliases` / `group` / `environment` /
///   `permissions` -> `Immediate`. (The plan flags "environment affecting
///   process startup" as restart-worthy; a `BTreeMap` diff cannot know which
///   vars those are, so core classifies env `Immediate` and the runtime may
///   escalate for vars it knows feed process startup.)
/// - `model` (same provider AND `caps.hot_model_switch`) -> `NextTurn`,
///   else `SessionRestart`.
/// - `cwd` / `provider` / `backend` / `worker_type` -> `SessionRestart`
///   (all process-level).
///
/// When multiple fields change, the STRONGEST mode wins
/// (`SessionRestart > NextTurn > Immediate`).
///
/// `caps` must be the capabilities of `new.provider`'s provider — that is
/// the provider that would perform a hot switch.
pub fn classify_config_change(
    old: &WorkerConfig,
    new: &WorkerConfig,
    caps: &crate::provider::ProviderCapabilities,
) -> ConfigApplyMode {
    // Immediate-class fields (display_name, name_aliases, group, environment,
    // permissions) never escalate, so they need no checks: Immediate is the
    // floor every classification starts from.
    let mut mode = ConfigApplyMode::Immediate;

    if old.model != new.model {
        let model_mode = if old.provider == new.provider && caps.hot_model_switch {
            ConfigApplyMode::NextTurn
        } else {
            ConfigApplyMode::SessionRestart
        };
        mode = mode.max(model_mode);
    }

    if old.cwd != new.cwd
        || old.provider != new.provider
        || old.backend != new.backend
        || old.worker_type != new.worker_type
    {
        mode = mode.max(ConfigApplyMode::SessionRestart);
    }

    mode
}

/// Apply a config mutation to a worker (RR-0018).
///
/// Bumps `version` on EVERY call (optimistic concurrency: the entity changed,
/// whether or not the running session noticed yet) and reports what happened
/// as a [`ConfigChangeResult`].
///
/// Session replacement (Invariant 43, atomic): when classification demands
/// `SessionRestart` and a live session exists, the result carries BOTH the
/// old and the freshly minted new session id, and the worker transitions to
/// `Starting`. Core is pure, so it cannot mint IDs itself —
/// `mint_replacement` is called exactly once, only when a replacement
/// actually happens (the server passes a ULID minter; tests pass a fixed id).
/// The caller is responsible for ending the old `Session` with
/// `ExitReason::Replaced` and spawning the new one under the returned id; if
/// the spawn fails, the caller keeps the old session and discards this
/// result (the plan's "old session remains active" rollback).
///
/// With NO live session (`current_session: None`), even a `SessionRestart`
/// change replaces nothing: the new config simply takes effect on the next
/// spawn, and `session_replaced` stays false.
///
/// `_now`: the instant the change is applied. Deliberately unused today —
/// none of the fields this function writes carry a timestamp — but every
/// mutation in core takes time as a parameter (core never reads a clock), so
/// call sites are stable when a timestamped field (audit / `updated_at`,
/// RR-0008) lands here. Named `_now` so the non-use is visible, not implied
/// (ethos rule 6: don't claim what isn't implemented).
pub fn apply_config(
    worker: Worker,
    new_config: WorkerConfig,
    caps: &crate::provider::ProviderCapabilities,
    _now: DateTime<Utc>,
    current_session: Option<SessionId>,
    mint_replacement: impl FnOnce() -> SessionId,
) -> (Worker, ConfigChangeResult) {
    let mode = classify_config_change(&worker.config, &new_config, caps);

    let (session_replaced, old_session, new_session, state) = match (mode, current_session) {
        (ConfigApplyMode::SessionRestart, Some(old)) => {
            let new = mint_replacement();
            // A session that "replaces" itself would be a lie in the audit
            // trail; ULID minting makes collision practically impossible,
            // and this guards the tests' fixed-id world too.
            debug_assert!(
                old != new,
                "replacement session id must differ from the old one"
            );
            (true, Some(old), Some(new), WorkerState::Starting)
        }
        (_, _) => (false, None, None, worker.state.clone()),
    };

    let worker = Worker {
        id: worker.id, // moved, never rewritten — the only field with no path to change
        config: new_config,
        capabilities: worker.capabilities,
        state,
        lifecycle: worker.lifecycle,
        version: worker.version + 1,
    };

    let result = ConfigChangeResult {
        mode,
        session_replaced,
        old_session,
        new_session,
    };

    (worker, result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ProviderCapabilities, ProviderId};
    use crate::session::BackendId;

    fn fixed_ulid(suffix: &str) -> ulid::Ulid {
        // ULID alphabet excludes I, L, O, U; suffixes below stay within it.
        format!("01JGXV0000000000000000{suffix}").parse().unwrap()
    }

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn base_config() -> WorkerConfig {
        WorkerConfig {
            worker_type: Default::default(),
            display_name: "backend".into(),
            name_aliases: vec![],
            cwd: "/Users/ethan/Dev/amux".into(),
            provider: ProviderId("claude".into()),
            model: Some("fable-5".into()),
            backend: BackendId::herdr(),
            environment: BTreeMap::new(),
            permissions: vec!["bash".into()],
            group: None,
        }
    }

    fn caps(hot_model_switch: bool) -> ProviderCapabilities {
        ProviderCapabilities {
            hot_model_switch,
            reports_usage: true,
            structured_events: true,
            hooks: true,
        }
    }

    fn worker() -> Worker {
        Worker::new(
            WorkerId::from_ulid(fixed_ulid("TEST")),
            base_config(),
            WorkerCapabilities::default(),
        )
    }

    // ---- RR-0003: identity vs config -----------------------------------

    #[test]
    fn config_mutation_preserves_id_and_bumps_version() {
        // `Worker.id` is private with no setter, so id immutability is a
        // compile-time property; this test pins the runtime half — a full
        // config replacement flows through and identity + version behave.
        let w = worker();
        let id_before = w.id().clone();

        let mut new_cfg = base_config();
        new_cfg.display_name = "rust-backend".into();
        new_cfg.cwd = "/Users/ethan/Dev/elsewhere".into();
        new_cfg.provider = ProviderId("codex".into());
        new_cfg.model = None;
        new_cfg.backend = BackendId::tmux();
        new_cfg.environment.insert("K".into(), "V".into());
        new_cfg.permissions.push("web".into());

        let (w1, _) = apply_config(
            w,
            new_cfg,
            &caps(true),
            t("2026-08-09T12:00:00Z"),
            None,
            || SessionId::from_ulid(fixed_ulid("AAAA")),
        );
        assert_eq!(w1.id(), &id_before);
        assert_eq!(w1.version, 1);

        // Version bumps on EVERY mutation, including a further one.
        let (w2, _) = apply_config(
            w1,
            base_config(),
            &caps(true),
            t("2026-08-09T12:01:00Z"),
            None,
            || SessionId::from_ulid(fixed_ulid("AAAA")),
        );
        assert_eq!(w2.id(), &id_before);
        assert_eq!(w2.version, 2);
    }

    #[test]
    fn rename_does_not_change_backend_ref() {
        // Invariant 43's rename guarantee, end to end: display_name changes,
        // the backend process name (derived from WorkerId) does not.
        let w = worker();
        let ref_before = crate::session::backend_ref(w.id());

        let mut renamed = base_config();
        renamed.display_name = "rust-backend".into();
        renamed.name_aliases = vec!["backend".into()];

        let (w1, res) = apply_config(
            w,
            renamed,
            &caps(true),
            t("2026-08-09T12:00:00Z"),
            Some(SessionId::from_ulid(fixed_ulid("AAAA"))),
            || SessionId::from_ulid(fixed_ulid("BBBB")),
        );
        assert_eq!(res.mode, ConfigApplyMode::Immediate);
        assert!(!res.session_replaced);
        assert_eq!(crate::session::backend_ref(w1.id()), ref_before);
    }

    /// A boxed config mutation for table-driven classification tests.
    type ConfigEdit = Box<dyn Fn(&mut WorkerConfig)>;

    // ---- RR-0018: classification, one class per field ------------------

    #[test]
    fn immediate_class_fields_classify_immediate() {
        let old = base_config();
        let cases: Vec<ConfigEdit> = vec![
            Box::new(|c| c.display_name = "renamed".into()),
            Box::new(|c| c.name_aliases = vec!["backend".into()]),
            Box::new(|c| c.group = Some(GroupId::from_ulid(fixed_ulid("GGGG")))),
            Box::new(|c| {
                c.environment.insert("API_URL".into(), "https://x".into());
            }),
            Box::new(|c| c.permissions = vec![]),
        ];
        for mutate in cases {
            let mut new = base_config();
            mutate(&mut new);
            assert_ne!(old, new, "test case mutated nothing");
            assert_eq!(
                classify_config_change(&old, &new, &caps(false)),
                ConfigApplyMode::Immediate
            );
        }
    }

    #[test]
    fn model_change_same_provider_hot_switch_is_next_turn() {
        let old = base_config();
        let mut new = base_config();
        new.model = Some("fable-6".into());
        assert_eq!(
            classify_config_change(&old, &new, &caps(true)),
            ConfigApplyMode::NextTurn
        );
    }

    #[test]
    fn model_change_without_hot_switch_is_session_restart() {
        let old = base_config();
        let mut new = base_config();
        new.model = Some("fable-6".into());
        assert_eq!(
            classify_config_change(&old, &new, &caps(false)),
            ConfigApplyMode::SessionRestart
        );
    }

    #[test]
    fn model_change_across_providers_is_session_restart_even_with_hot_switch() {
        // hot_model_switch is a same-provider capability; a provider change
        // is process-level regardless.
        let old = base_config();
        let mut new = base_config();
        new.provider = ProviderId("codex".into());
        new.model = Some("other-model".into());
        assert_eq!(
            classify_config_change(&old, &new, &caps(true)),
            ConfigApplyMode::SessionRestart
        );
    }

    #[test]
    fn process_level_fields_classify_session_restart() {
        let old = base_config();
        let cases: Vec<ConfigEdit> = vec![
            Box::new(|c| c.cwd = "/somewhere/else".into()),
            Box::new(|c| c.provider = ProviderId("codex".into())),
            Box::new(|c| c.backend = BackendId::tmux()),
        ];
        for mutate in cases {
            let mut new = base_config();
            mutate(&mut new);
            assert_eq!(
                classify_config_change(&old, &new, &caps(true)),
                ConfigApplyMode::SessionRestart
            );
        }
    }

    #[test]
    fn no_change_classifies_immediate() {
        // A no-op diff still has a truthful answer: nothing needs a restart.
        let old = base_config();
        let new = base_config();
        assert_eq!(
            classify_config_change(&old, &new, &caps(false)),
            ConfigApplyMode::Immediate
        );
    }

    #[test]
    fn strongest_mode_wins() {
        // Immediate + NextTurn -> NextTurn
        let old = base_config();
        let mut new = base_config();
        new.display_name = "renamed".into();
        new.model = Some("fable-6".into());
        assert_eq!(
            classify_config_change(&old, &new, &caps(true)),
            ConfigApplyMode::NextTurn
        );

        // Immediate + NextTurn + SessionRestart -> SessionRestart
        new.cwd = "/somewhere/else".into();
        assert_eq!(
            classify_config_change(&old, &new, &caps(true)),
            ConfigApplyMode::SessionRestart
        );
    }

    #[test]
    fn apply_mode_ord_is_escalation_order() {
        // classify_config_change folds with `max`; this pins the derive.
        assert!(ConfigApplyMode::Immediate < ConfigApplyMode::NextTurn);
        assert!(ConfigApplyMode::NextTurn < ConfigApplyMode::SessionRestart);
    }

    // ---- RR-0018: session replacement ----------------------------------

    #[test]
    fn session_restart_with_live_session_replaces_it_atomically() {
        let w = worker();
        let old_ses = SessionId::from_ulid(fixed_ulid("AAAA"));
        let new_ses = SessionId::from_ulid(fixed_ulid("BBBB"));

        let mut new_cfg = base_config();
        new_cfg.cwd = "/somewhere/else".into();

        let (w1, res) = apply_config(
            w,
            new_cfg,
            &caps(true),
            t("2026-08-09T12:00:00Z"),
            Some(old_ses.clone()),
            || new_ses.clone(),
        );

        assert_eq!(res.mode, ConfigApplyMode::SessionRestart);
        assert!(res.session_replaced);
        // Atomicity is represented: ONE result carries both ids, and they
        // differ — the swap can never read as a session replacing itself.
        assert_eq!(res.old_session, Some(old_ses.clone()));
        assert_eq!(res.new_session, Some(new_ses.clone()));
        assert_ne!(res.old_session, res.new_session);
        // The worker survived the swap and is spinning the new session up.
        assert_eq!(w1.state, WorkerState::Starting);
        assert_eq!(w1.version, 1);
    }

    #[test]
    fn session_restart_with_no_live_session_replaces_nothing() {
        let w = worker();
        let mut new_cfg = base_config();
        new_cfg.provider = ProviderId("codex".into());

        let (w1, res) = apply_config(
            w,
            new_cfg,
            &caps(true),
            t("2026-08-09T12:00:00Z"),
            None,
            || panic!("mint_replacement must not be called when there is no session to replace"),
        );
        assert_eq!(res.mode, ConfigApplyMode::SessionRestart);
        assert!(!res.session_replaced);
        assert_eq!(res.old_session, None);
        assert_eq!(res.new_session, None);
        // No swap -> no state transition; the config waits for the next spawn.
        assert_eq!(w1.state, WorkerState::Stopped);
    }

    #[test]
    fn immediate_change_with_live_session_does_not_touch_it() {
        let w = worker();
        let mut new_cfg = base_config();
        new_cfg.permissions.push("web".into());

        let (_, res) = apply_config(
            w,
            new_cfg,
            &caps(true),
            t("2026-08-09T12:00:00Z"),
            Some(SessionId::from_ulid(fixed_ulid("AAAA"))),
            || panic!("mint_replacement must not be called for an Immediate change"),
        );
        assert_eq!(res.mode, ConfigApplyMode::Immediate);
        assert!(!res.session_replaced);
        assert_eq!(res.old_session, None);
        assert_eq!(res.new_session, None);
    }

    // ---- serde ----------------------------------------------------------

    #[test]
    fn worker_state_serde_round_trips() {
        for s in [
            WorkerState::Stopped,
            WorkerState::Starting,
            WorkerState::Active {
                turn: Some(TurnId::from_ulid(fixed_ulid("TRNA"))),
            },
            WorkerState::Active { turn: None },
            WorkerState::Idle {
                since: t("2026-08-09T12:00:00Z"),
            },
            WorkerState::Waiting {
                reason: "gate review".into(),
            },
            WorkerState::RateLimited {
                reset_at: Some(t("2026-08-09T13:00:00Z")),
            },
            WorkerState::Error {
                detail: "backend gone".into(),
            },
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let back: WorkerState = serde_json::from_str(&json).unwrap();
            assert_eq!(s, back);
        }
    }

    #[test]
    fn worker_serde_round_trips_with_private_id() {
        let w = worker();
        let json = serde_json::to_string(&w).unwrap();
        let back: Worker = serde_json::from_str(&json).unwrap();
        assert_eq!(w, back);
        assert_eq!(back.id(), w.id());
    }

    // ---- lifecycle ---------------------------------------------------------

    #[test]
    fn lifecycle_serde_round_trips() {
        for lc in [
            WorkerLifecycle::Active,
            WorkerLifecycle::Paused,
            WorkerLifecycle::Archived,
            WorkerLifecycle::Deleted,
        ] {
            let json = serde_json::to_string(&lc).unwrap();
            let back: WorkerLifecycle = serde_json::from_str(&json).unwrap();
            assert_eq!(lc, back);
        }
    }

    #[test]
    fn lifecycle_parse_round_trips() {
        for lc in [
            WorkerLifecycle::Active,
            WorkerLifecycle::Paused,
            WorkerLifecycle::Archived,
            WorkerLifecycle::Deleted,
        ] {
            assert_eq!(WorkerLifecycle::parse(lc.as_str()), Some(lc));
        }
        assert_eq!(WorkerLifecycle::parse("bogus"), None);
    }

    #[test]
    fn lifecycle_drivable_only_active() {
        assert!(WorkerLifecycle::Active.is_drivable());
        assert!(!WorkerLifecycle::Paused.is_drivable());
        assert!(!WorkerLifecycle::Archived.is_drivable());
        assert!(!WorkerLifecycle::Deleted.is_drivable());
    }

    #[test]
    fn lifecycle_can_start_only_active() {
        assert!(WorkerLifecycle::Active.can_start());
        assert!(!WorkerLifecycle::Paused.can_start());
        assert!(!WorkerLifecycle::Archived.can_start());
        assert!(!WorkerLifecycle::Deleted.can_start());
    }

    #[test]
    fn config_change_preserves_lifecycle() {
        let mut w = worker();
        w.lifecycle = WorkerLifecycle::Paused;
        let mut new_cfg = base_config();
        new_cfg.display_name = "renamed".into();
        let (w1, _) = apply_config(
            w,
            new_cfg,
            &caps(true),
            t("2026-08-09T12:00:00Z"),
            None,
            || SessionId::from_ulid(fixed_ulid("AAAA")),
        );
        assert_eq!(w1.lifecycle, WorkerLifecycle::Paused);
    }

    #[test]
    fn default_lifecycle_is_active() {
        assert_eq!(WorkerLifecycle::default(), WorkerLifecycle::Active);
    }

    #[test]
    fn worker_deserialize_without_lifecycle_defaults_active() {
        let json = r#"{"id":"wrk_01JGXV0000000000000000TEST","config":{"display_name":"backend","name_aliases":[],"cwd":"/tmp","provider":"claude","model":"fable-5","backend":"herdr","environment":{},"permissions":["bash"],"group":null},"capabilities":{"tools":[],"repositories":[],"browser":false,"integrations":[]},"state":{"state":"stopped"},"version":0}"#;
        let w: Worker = serde_json::from_str(json).unwrap();
        assert_eq!(w.lifecycle, WorkerLifecycle::Active);
    }
}
