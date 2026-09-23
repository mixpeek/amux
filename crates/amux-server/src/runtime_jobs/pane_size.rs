//! Pane-size restoration (AMUX-2634) — a VIEWER must not be able to degrade a
//! WORKER.
//!
//! # The bug
//!
//! Opening a peek POSTs `/resize` so the tmux window matches the reader's
//! measured character width; Claude Code then reflows its transcript to that
//! width, which is what makes peek natively responsive instead of
//! double-wrapped on a phone. The capture endpoint takes no width parameter —
//! it returns whatever tmux already hard-wrapped — so resizing the pane is the
//! only lever the feature has.
//!
//! The catch is documented in tmux's own manual: `resize-window` "will
//! automatically set `window-size` to `manual` in the window options". That
//! permanently detaches the window from its session `default-size`. Nothing
//! ever set it back, so **the viewer's width became the worker's width,
//! forever**. Measured on the live fleet before this landed: 12 amux panes
//! below the configured 220 columns, one of them pinned at the client's floor
//! of 50x50 by a single phone peek. Every turn that worker took afterwards —
//! its reasoning, its diffs, its tables — was wrapped into a 50-column column
//! because somebody once glanced at it on a phone.
//!
//! # Why a restorer and not restore-on-close
//!
//! Restore-on-close is the obvious fix and it is not sufficient: it only runs
//! when the client is polite. A backgrounded phone, a killed tab, a dropped
//! network or a crashed browser all skip teardown, and those are exactly the
//! circumstances of the reported incident. A fix that depends on the viewer
//! behaving cannot make the guarantee "a viewer can never degrade a worker".
//!
//! So the guarantee lives server-side and the client holds a LEASE. Every
//! `/resize` (including the no-op "already sized" answer) refreshes it; the
//! SPA re-sends one every 30s while a peek is open. When the lease goes stale
//! the sweep restores the pane. A viewer that vanishes therefore costs the
//! worker at most `LEASE_TTL + TICK` of narrow output, bounded and automatic,
//! with no cooperation required.
//!
//! # Why this is a re-port, not a new idea
//!
//! Python had `_tmux_size_watchdog` on a 60s job (py:4443, registered
//! py:78188): restore every DETACHED amux session to the canonical size and
//! reset `window-size latest`. The rust cutover ported `_resize_pane` — the
//! comment on it even cites `py:25832` — and dropped the restorer, which is
//! precisely what turned a transient resize into a permanent one. This module
//! restores that half and adds the lease, which python lacked: python's timer
//! fought an OPEN peek every 60 seconds, re-widening the pane under a reader
//! who was still looking at it.
//!
//! # The three tmux calls, in order, and why all three
//!
//! 1. `set-option -t <s> default-size <C>x<R>` — some sessions were created
//!    without `-x/-y` and inherit the global `default-size 80x24`; without
//!    this, step 3 would snap them straight back to 80x24.
//! 2. `resize-window -t <s> -x <C> -y <R>` — the actual widening.
//! 3. `set-option -w -t <s> window-size latest` — undo the `manual` pin step 2
//!    just set. Without it the window is the right width but still detached
//!    from `default-size`, and a real human attaching later no longer wins.
//!
//! A width restored while the window stays pinned is a half-fix that looks
//! complete from `list-panes`, which is why the pin is asserted separately.

use super::PeriodicTask;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How often the sweep runs.
const TICK_SECS: u64 = 20;
/// How long a `/resize` keeps the pane at the viewer's width with no further
/// contact. The SPA refreshes at 30s, so this tolerates one missed heartbeat
/// (a backgrounded tab, a slow network) without snapping the pane out from
/// under a live reader.
const LEASE_TTL: Duration = Duration::from_secs(75);

/// Only sessions this server manages. A stray developer tmux session is not
/// ours to resize (ethos rule 8 — whose data is this).
const MANAGED_PREFIX: &str = "amux-";

/// Canonical pane geometry.
///
/// NOTE: two other copies of this default exist — `session_verbs::tmux_cols`/
/// `tmux_rows` (env-overridable, used on session create) and a HARD-CODED
/// `"220"`/`"50"` in `backend/tmux.rs`. They read the same env var here so the
/// restorer cannot converge on a different width than the creator, but three
/// spellings of one constant is a seam worth collapsing when someone next
/// touches session creation.
pub fn configured_cols() -> u32 {
    std::env::var("AMUX_TMUX_COLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|c| *c > 0)
        .unwrap_or(220)
}

pub fn configured_rows() -> u32 {
    std::env::var("AMUX_TMUX_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|r| *r > 0)
        .unwrap_or(50)
}

static LEASES: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

/// Refresh the viewer lease for `session`.
///
/// Called from `session_verbs::resize_pane` on EVERY `/resize`, including the
/// early returns — a peek that is steady at one width still sends heartbeats,
/// and if only the size-CHANGING path refreshed the lease, a stationary reader
/// would have the pane yanked back to 220 mid-read. The lease answers "is
/// someone still looking", not "did the width change".
pub fn note_resize(session: &str) {
    if let Ok(mut g) = LEASES.lock() {
        g.get_or_insert_with(HashMap::new)
            .insert(session.to_string(), Instant::now());
    }
}

fn lease_is_fresh(session: &str) -> bool {
    LEASES
        .lock()
        .ok()
        .and_then(|g| {
            g.as_ref()
                .and_then(|m| m.get(session))
                .map(|t| t.elapsed() < LEASE_TTL)
        })
        .unwrap_or(false)
}

async fn tmux(args: &[&str]) -> Option<std::process::Output> {
    tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("tmux").args(args).output(),
    )
    .await
    .ok()?
    .ok()
}

/// One managed window as tmux reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneState {
    pub session: String,
    pub cols: u32,
    pub rows: u32,
    pub attached: bool,
    pub pinned: bool,
}

/// Parse `list-windows -a` output. Split out from the I/O so the decision
/// logic below has a negative control that does not need a live tmux.
pub fn parse_windows(out: &str) -> Vec<PaneState> {
    out.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 5 {
                return None;
            }
            Some(PaneState {
                session: f[0].to_string(),
                cols: f[1].parse().ok()?,
                rows: f[2].parse().ok()?,
                attached: f[3] != "0",
                pinned: f[4] == "manual",
            })
        })
        .collect()
}

/// Should this window be restored? Split from I/O so it can be tested against
/// every corner without tmux.
///
/// `lease_fresh` is passed in rather than read, so a test can express "a live
/// viewer is holding it" — the case that must NOT be touched.
pub fn needs_restore(p: &PaneState, cols: u32, rows: u32, lease_fresh: bool) -> bool {
    if !p.session.starts_with(MANAGED_PREFIX) {
        return false; // not ours
    }
    if p.attached {
        return false; // a real terminal owns the size while attached
    }
    if lease_fresh {
        return false; // someone is reading it right now
    }
    // Off-size OR merely pinned: a window at the right width but still
    // `manual` is detached from default-size and will not follow a human
    // attach. Restoring only the width leaves that half broken and invisible.
    p.cols != cols || p.rows != rows || p.pinned
}

async fn restore(session: &str, cols: u32, rows: u32) {
    // Targets come from the L2 helpers, never hand-spelled: tmux resolves a
    // bare `-t foo` by PREFIX, and `amux-amux` is a prefix of eight live
    // sessions — so a hand-written target is correct until the exact session
    // is briefly absent and then silently resizes a SIBLING. `stq` is the
    // session form (`=<ref>`) for the session option; `ptq` is the
    // window/pane form (`=<ref>:`) for the two window-level commands.
    let stq = crate::backend::tmux::session_target(session);
    let ptq = crate::backend::tmux::pane_target(session);
    let size = format!("{cols}x{rows}");
    let c = cols.to_string();
    let r = rows.to_string();
    // Order matters — see the module docs.
    let _ = tmux(&["set-option", "-t", &stq, "default-size", &size]).await;
    let _ = tmux(&["resize-window", "-t", &ptq, "-x", &c, "-y", &r]).await;
    let _ = tmux(&["set-option", "-w", "-t", &ptq, "window-size", "latest"]).await;
}

/// Run one pass. `ignore_leases` is the one-shot repair mode: it restores every
/// shrunk managed pane regardless of whether a viewer claims to be looking,
/// used once at startup to converge a fleet that drifted while no restorer
/// existed. Returns the sessions it restored.
pub async fn sweep(ignore_leases: bool) -> Vec<String> {
    let cols = configured_cols();
    let rows = configured_rows();
    let Some(out) = tmux(&[
        "list-windows",
        "-a",
        "-F",
        // NOTE the HYPHEN in `#{window-size}`: that spells the tmux OPTION.
        // `#{window_size}` (underscore) is not a format variable and expands
        // to the empty string — which parses fine, reports every window as
        // unpinned, and silently disables half this check. Verified live.
        "#{session_name}\t#{window_width}\t#{window_height}\t#{session_attached}\t#{window-size}",
    ])
    .await
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let mut fixed = Vec::new();
    for p in parse_windows(&String::from_utf8_lossy(&out.stdout)) {
        let fresh = !ignore_leases && lease_is_fresh(&p.session);
        if !needs_restore(&p, cols, rows, fresh) {
            continue;
        }
        tracing::info!(
            session = %p.session, from = %format!("{}x{}", p.cols, p.rows),
            to = %format!("{cols}x{rows}"), pinned = p.pinned,
            "pane-size: restoring detached window to configured size"
        );
        restore(&p.session, cols, rows).await;
        fixed.push(p.session);
    }
    fixed
}

/// The one-shot repair (AMUX-2634): converge every currently-shrunk managed
/// pane at boot, then hand over to the periodic sweep.
///
/// Logged at INFO with a COUNT even when zero, because "the repair found
/// nothing" and "the repair never ran" are otherwise the same silence — and
/// this whole class of bug is one where silence was read as health.
pub fn spawn() -> PeriodicTask {
    // The first periodic tick is immediate. Keep boot repair INSIDE the same
    // guarded constructor: a separate startup task bypassed fleet isolation
    // even while the registry correctly reported this job disabled (AF-789).
    let mut first = true;
    super::spawn_periodic("pane_size", TICK_SECS, move || {
        let boot = std::mem::replace(&mut first, false);
        async move {
            let fixed = sweep(boot).await;
            if boot {
                tracing::info!(
                    count = fixed.len(),
                    sessions = ?fixed,
                    "pane-size: one-shot repair complete (AMUX-2634)"
                );
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(session: &str, cols: u32, rows: u32, attached: bool, pinned: bool) -> PaneState {
        PaneState {
            session: session.into(),
            cols,
            rows,
            attached,
            pinned,
        }
    }

    #[test]
    fn parses_the_real_list_windows_format() {
        // Byte-for-byte the shape our -F string produces.
        let out = "amux-foo\t50\t50\t0\tmanual\n\
                   amux-bar\t220\t50\t1\tlatest\n\
                   66\t153\t21\t0\tlatest\n\
                   garbage line\n";
        let v = parse_windows(out);
        assert_eq!(v.len(), 3, "malformed lines are dropped, not panicked on");
        assert_eq!(v[0], p("amux-foo", 50, 50, false, true));
        assert_eq!(v[1], p("amux-bar", 220, 50, true, false));
        assert_eq!(v[2], p("66", 153, 21, false, false));
    }

    // THE INCIDENT SPECIMEN, rebuilt from the live measurement: a phone peek
    // left amux-mixpeek-autopilot at 50x50, pinned, detached. Any fix that
    // cannot restore THIS is theatre (ethos rule 7 — test against the artifact
    // that motivated it, not the convenient case).
    #[test]
    fn restores_the_50x50_phone_peek_specimen() {
        assert!(needs_restore(
            &p("amux-mixpeek-autopilot", 50, 50, false, true),
            220,
            50,
            false
        ));
    }

    #[test]
    fn never_touches_a_session_with_a_real_terminal_attached() {
        // 181x44 attached — a human's own terminal, and its size wins.
        assert!(!needs_restore(
            &p("amux-gtm-videos", 181, 44, true, true),
            220,
            50,
            false
        ));
    }

    #[test]
    fn never_touches_a_session_someone_is_peeking_right_now() {
        assert!(
            !needs_restore(&p("amux-amux", 102, 50, false, true), 220, 50, true),
            "a fresh lease means a live reader — python's restorer fought open peeks"
        );
    }

    #[test]
    fn never_touches_a_session_we_do_not_manage() {
        assert!(!needs_restore(
            &p("66", 153, 21, false, true),
            220,
            50,
            false
        ));
        assert!(!needs_restore(
            &p("some-dev-shell", 80, 24, false, false),
            220,
            50,
            false
        ));
    }

    // The half-fix guard: right width, still pinned. list-panes would show
    // this as healthy while the window no longer follows default-size.
    #[test]
    fn a_correctly_sized_but_pinned_window_is_still_repaired() {
        assert!(needs_restore(
            &p("amux-ethan-dev", 220, 50, false, true),
            220,
            50,
            false
        ));
    }

    #[test]
    fn a_healthy_window_is_left_alone() {
        assert!(!needs_restore(
            &p("amux-self", 220, 50, false, false),
            220,
            50,
            false
        ));
    }

    // Sessions created without -x/-y sit at the global default 80x24 and are
    // also below the configured width — the restorer must claim them too.
    #[test]
    fn an_unsized_session_at_the_global_default_is_repaired() {
        assert!(needs_restore(
            &p("amux-bdq-empty", 80, 24, false, false),
            220,
            50,
            false
        ));
    }

    #[test]
    fn lease_expires() {
        note_resize("amux-lease-test");
        assert!(lease_is_fresh("amux-lease-test"));
        // Backdate past the TTL: a lease that never expires is the same bug as
        // no restorer at all.
        if let Ok(mut g) = LEASES.lock() {
            g.get_or_insert_with(HashMap::new).insert(
                "amux-lease-test".into(),
                Instant::now() - LEASE_TTL - Duration::from_secs(1),
            );
        }
        assert!(!lease_is_fresh("amux-lease-test"));
        assert!(!lease_is_fresh("amux-never-peeked"));
    }

    #[test]
    fn configured_size_defaults_match_session_creation() {
        // If these ever disagree with session_verbs::tmux_cols/tmux_rows the
        // restorer converges on a width the creator never uses.
        assert_eq!(configured_cols(), 220);
        assert_eq!(configured_rows(), 50);
    }
}
