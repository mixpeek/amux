//! Standing orders must be switchable at GLOBAL, GROUP and WORKER scope
//! (AMUX-2930).
//!
//! Ethan, 2026-08-11: "I should be able to shut off standing orders like 'Hey
//! you have stuff in your to-do. Keep going.' … on the group, global, or
//! individual worker level … configurable but also obviously have defaults."
//!
//! Before this, only the WORKER level worked. `/api/scope` has advertised `env`
//! at `["global","group","worker"]` since the cutover and the scope UI writes
//! all three files, but every consumer called `parse_env(lane)`, which loads
//! the worker file and nothing else. Setting the switch globally or on a group
//! wrote a file that no code read: it saved, reported success, and changed
//! nothing.
//!
//! Each test drives the real files through its OWN temp home, passed in
//! explicitly. Nothing here touches process env, so nothing races.

use amux_server::api::session_verbs::standing_orders_on_in;
use std::fs;
use std::path::Path;

/// A home PER TEST. The first version shared one via OnceLock + AMUX_HOME and
/// three tests failed immediately: cargo runs a binary's tests in parallel, so
/// siblings rewrote each other's single `amux.env` mid-assertion. Serialising
/// with a mutex would have gone green while leaving the tests order-dependent —
/// the resolution is parameterised on the home instead, so there is nothing
/// global left to race.
struct Home(tempfile::TempDir);

impl Home {
    fn new() -> Self {
        let d = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(d.path().join("sessions")).unwrap();
        fs::create_dir_all(d.path().join("env")).unwrap();
        Self(d)
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn worker(&self, lane: &str, body: &str) -> &Self {
        fs::write(
            self.path().join("sessions").join(format!("{lane}.env")),
            body,
        )
        .unwrap();
        self
    }
    fn group(&self, group: &str, body: &str) -> &Self {
        fs::write(self.path().join("env").join(format!("{group}.env")), body).unwrap();
        self
    }
    fn global(&self, body: &str) -> &Self {
        fs::write(self.path().join("amux.env"), body).unwrap();
        self
    }
    fn on(&self, lane: &str, key: &str) -> bool {
        standing_orders_on_in(self.path(), lane, key)
    }
}

#[test]
fn default_is_on_at_every_level() {
    let h = Home::new();
    h.worker("so-default", "CC_DIR=/tmp\n");
    assert!(
        h.on("so-default", "CC_AUTO_PICKUP"),
        "default must be ON — the opt-IN version of this reached 2 lanes of ~50"
    );
    assert!(h.on("so-default", "CC_AUTO_CONTINUE"));
    // A lane with no env file at all is still ON.
    assert!(h.on("so-nonexistent-lane", "CC_AUTO_PICKUP"));
}

#[test]
fn worker_level_off_silences_that_lane_only() {
    let h = Home::new();
    h.worker("so-off", "CC_AUTO_PICKUP=0\n")
        .worker("so-on", "CC_DIR=/tmp\n");
    assert!(!h.on("so-off", "CC_AUTO_PICKUP"));
    assert!(
        h.on("so-on", "CC_AUTO_PICKUP"),
        "the neighbour is unaffected"
    );
}

/// THE REGRESSION, half one. Pre-fix this wrote `~/.amux/env/<group>.env` and
/// nothing read it, so this failed while the UI reported "saved".
#[test]
fn group_level_off_silences_every_member() {
    let h = Home::new();
    h.group("quiet-crew", "CC_STANDING_ORDERS=0\n")
        .worker("so-member-a", "CC_TAGS=quiet-crew\n")
        .worker("so-member-b", "CC_TAGS=quiet-crew,other\n")
        .worker("so-outsider", "CC_TAGS=other\n");
    assert!(
        !h.on("so-member-a", "CC_AUTO_PICKUP"),
        "group off silences a member"
    );
    assert!(
        !h.on("so-member-b", "CC_AUTO_CONTINUE"),
        "multi-group member too"
    );
    assert!(
        h.on("so-outsider", "CC_AUTO_PICKUP"),
        "a lane outside the group keeps its default"
    );
}

/// THE REGRESSION, half two. Same story for `~/.amux/amux.env`.
#[test]
fn global_level_off_silences_the_whole_fleet() {
    let h = Home::new();
    h.global("CC_STANDING_ORDERS=0\n")
        .worker("so-fleet-a", "CC_DIR=/tmp\n")
        .worker("so-fleet-b", "CC_TAGS=some-group\n");
    assert!(!h.on("so-fleet-a", "CC_AUTO_PICKUP"));
    assert!(!h.on("so-fleet-b", "CC_AUTO_CONTINUE"));
}

/// Precedence must run this direction or a per-worker exception is impossible:
/// turn the fleet off, then turn ONE lane back on.
#[test]
fn worker_overrides_group_overrides_global() {
    let h = Home::new();
    h.global("CC_STANDING_ORDERS=0\n")
        .group("loud-crew", "CC_STANDING_ORDERS=1\n")
        .worker("so-prec-global", "CC_DIR=/tmp\n")
        .worker("so-prec-group", "CC_TAGS=loud-crew\n")
        .worker(
            "so-prec-worker",
            "CC_TAGS=loud-crew\nCC_STANDING_ORDERS=0\n",
        );
    assert!(
        !h.on("so-prec-global", "CC_AUTO_PICKUP"),
        "global off applies"
    );
    assert!(
        h.on("so-prec-group", "CC_AUTO_PICKUP"),
        "the group's ON beats the global OFF"
    );
    assert!(
        !h.on("so-prec-worker", "CC_AUTO_PICKUP"),
        "the worker's OFF beats both"
    );
}

/// The master switch silences classes it does not name; the per-class key still
/// cuts finer when the master is on.
#[test]
fn the_master_switch_covers_every_class_and_the_per_class_keys_still_work() {
    let h = Home::new();
    h.worker("so-master", "CC_STANDING_ORDERS=0\nCC_AUTO_PICKUP=1\n");
    assert!(
        !h.on("so-master", "CC_AUTO_PICKUP"),
        "master OFF wins even against an explicit per-class ON — one knob to reach for"
    );
    h.worker("so-fine", "CC_AUTO_PICKUP=0\n");
    assert!(!h.on("so-fine", "CC_AUTO_PICKUP"), "per-class off");
    assert!(
        h.on("so-fine", "CC_AUTO_CONTINUE"),
        "…and it does NOT silence the other class"
    );
}

/// Every falsey spelling the rest of the codebase accepts, plus a control set
/// that must NOT read as off — without it, a predicate returning false for
/// everything would pass all of the above.
#[test]
fn the_falsey_spellings_are_the_usual_ones_and_nothing_else() {
    let h = Home::new();
    for v in ["0", "false", "no", "off", "OFF", " 0 "] {
        h.worker("so-spell", &format!("CC_AUTO_PICKUP={v}\n"));
        assert!(!h.on("so-spell", "CC_AUTO_PICKUP"), "{v:?} means off");
    }
    for v in ["1", "true", "yes", "on", "", "maybe"] {
        h.worker("so-spell", &format!("CC_AUTO_PICKUP={v}\n"));
        assert!(
            h.on("so-spell", "CC_AUTO_PICKUP"),
            "{v:?} must NOT be read as off — default-on is the point"
        );
    }
}
