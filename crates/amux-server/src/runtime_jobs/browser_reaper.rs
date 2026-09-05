//! Release a browser that has had nothing open for a while (AMUX-3829).
//!
//! Ethan, 2026-08-28: "multiple browsers shouldnt be an issue, but it should
//! clean up automatically after idle use." The prompting incident was a browser
//! held 18.1 HOURS with ZERO tabs, blocking him, because nothing in amux reaped
//! one — `runtime_jobs::registry` had no browser job at all.
//!
//! WHY THIS IS SAFE TO DO AUTOMATICALLY, which is not obvious in a subsystem
//! whose comments record three separate incidents of destroying people's staged
//! logins (AMUX-3063, AMUX-3414, AMUX-3610). A COMPLETED login lives on disk in
//! `playwright-auth/profiles/<name>/` and survives a stop: `stop_profile_as`
//! sends SIGTERM precisely so storage flushes, and the only `remove_dir_all` on
//! a profile is the explicit delete verb. So the question is never "will this
//! lose an account" — it is "will this lose an OPEN PAGE", and this job only
//! ever runs when there are none.
//!
//! IDLE IS CONTINUOUS EMPTINESS, NOT AGE. Reaping on "started N ago" would kill
//! a browser someone used a minute ago, and reaping the instant the last tab
//! closes would kill one they are about to reuse. So a profile has to be seen
//! empty on every check across the whole window; one real page anywhere in it
//! resets the clock. That also makes the signal honest under the multi-slot
//! registry (AMUX-3828): emptiness is per profile, so one worker's idle browser
//! is reaped without touching the neighbour a second worker is driving.
//!
//! WHAT IT WILL NOT DO. A browser whose CDP does not answer is NOT reaped.
//! Silence is not zero — the same distinction the takeover refusal draws — and
//! killing a browser because it failed to answer a poll would turn a transient
//! wedge into a destroyed session.

use std::collections::HashMap;
use std::time::Duration;

/// How long a profile must be CONTINUOUSLY empty before it is released.
/// `0` disables this arm entirely.
///
/// One hour by default. The window is generous against the thing actually at
/// risk, which is only a relaunch: with no pages open there is no in-memory
/// state to lose and the profile's logins are on disk either way. It is short
/// enough that the 18-hour zombie that prompted this cannot recur.
pub fn reap_after_s() -> u64 {
    std::env::var("AMUX_BROWSER_IDLE_REAP_S")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(3600)
}

/// Activity-based TTL: kill any browser that has had no verb (navigate /
/// screenshot / action / state) for this many seconds, even with pages open.
/// `0` disables this arm. Default 300s (5 minutes).
///
/// This is the fastest arm. A session that opens a browser and walks away
/// without closing it holds memory, GPU buffers, and a CDP socket for nothing.
/// Five minutes of silence is a strong signal of abandonment.
pub fn activity_reap_s() -> u64 {
    std::env::var("AMUX_BROWSER_ACTIVITY_REAP_S")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(300)
}

/// Hard TTL: kill any browser older than this, even if it has pages open.
/// `0` disables this arm entirely. Default 4 hours.
///
/// WHY THIS EXISTS (2026-08-30). The idle reaper only fires when there are NO
/// real pages. Sessions that opened a page and never closed it — or that were
/// abandoned mid-task — accumulated indefinitely: 20+ Chrome instances on the
/// Mac dock, each holding memory, GPU buffers, and a CDP socket. The idle arm
/// cannot reach them. A hard ceiling can.
///
/// WHAT IT WILL LOSE. An open page is lost. The profile's saved login is NOT
/// lost (it is on disk). This is the same trade the idle reaper makes, with the
/// explicit acknowledgement that a page may be present. Sessions that need a
/// browser for longer than the TTL should increase it via the env var or stop
/// and restart the browser themselves to reset the clock.
pub fn ttl_s() -> u64 {
    std::env::var("AMUX_BROWSER_TTL_S")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(4 * 3600)
}

/// How long an UNREGISTERED profile may sit unused before it is deleted.
/// `0` disables this arm entirely. Thirty days by default.
///
/// WHY THIS EXISTS (Ethan, 2026-08-30): "we should have a ttl for the browser
/// profiles. that way we don't create so many at any point and never use them."
/// Measured that day: 50 profiles, 5.4 GB. Thirty-two of them had not been
/// touched in 60+ days and the oldest in 171 — `studio-e2e-test2`,
/// `anonymous-1776949356827`, `mxp-studio-v3`, the sediment of one-off e2e runs
/// that each created a profile and none cleaned up.
///
/// WHY THIRTY DAYS. The distribution has a natural gap rather than a slope:
/// everything in real rotation was under 31 days, the next profile after that
/// was 60 days, and nothing sat between. So the line is drawn where the data
/// already separates, and moving it to 45 or 60 would delete the same 32.
///
/// WHY REGISTERED PROFILES ARE EXEMT AT ANY AGE. A registry entry is a
/// deliberate save with domains and a label — a login someone set up on purpose
/// and may need twice a year (`recreation-gov` was 31 days idle and is exactly
/// the kind of thing this must never eat). Ad-hoc profiles are made by a script
/// that needed a browser once. The registry is the line between the two, and it
/// is the user's own declaration rather than our inference.
pub fn profile_ttl_days() -> u64 {
    std::env::var("AMUX_PROFILE_TTL_DAYS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(30)
}

/// Should this profile be deleted? Pure, so the policy is testable without a
/// filesystem full of real logins.
///
/// The three exemptions are not interchangeable and each has its own reason:
///   `registered` — a deliberate save; see `profile_ttl_days`.
///   `running`    — a browser is open on it right now. Age is measured from the
///                  directory mtime, and a browser can be driven for hours
///                  without Chrome touching the dir, so a long-lived session on
///                  an old profile would otherwise be deleted from under a peer.
///                  This is also what makes the arm safe under the multi-slot
///                  registry: N workers on N profiles, and a reap of one must
///                  never touch another's.
///   `default`    — `delete_profile` refuses it too; belt and braces, because
///                  the reaper reaching it at all would be the bug.
pub fn should_reap_profile(
    name: &str,
    registered: bool,
    running: bool,
    age_days: Option<f64>,
    ttl_days: u64,
) -> bool {
    if ttl_days == 0 || registered || running || name == "default" {
        return false;
    }
    // AN UNREADABLE MTIME IS UNKNOWN AGE, NOT INFINITE AGE. `last_used` is an
    // Option because the stat can fail, and the tempting readings are both
    // wrong: `None` as 0 keeps junk forever, `None` as huge deletes a profile
    // whose age was never measured. Unknown declines to act, same rule the
    // conversation-claim expiry follows.
    match age_days {
        Some(d) => d >= ttl_days as f64,
        None => false,
    }
}

fn tick_secs() -> u64 {
    std::env::var("AMUX_BROWSER_REAP_TICK_S")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(120)
}

/// When each profile was FIRST seen empty. Absent = not currently empty.
///
/// ON DISK, NOT IN MEMORY, AND THAT IS THE WHOLE POINT (AMUX-3829, second pass).
///
/// The first version held this in a process-global map. The builder installs a
/// new binary and the server self-adopts on EVERY commit, which resets it, so
/// the window restarted every time anyone committed. Measured on the day this
/// was found: 22 builds between 06:33 and 16:26, median gap 16.7 minutes, and
/// only TWO gaps of 60 minutes or more. Against a 3600s window that is a reaper
/// which on a working day can almost never fire, while reporting `spawned:
/// true, ticks: N, status: ok` throughout.
///
/// So the card's claim that "the 18-hour zombie cannot recur" was false as
/// shipped. Nothing would have shown it: the job's own health is about the LOOP
/// running, and the loop was running perfectly.
///
/// The file is the reaper's alone. It is rewritten each tick, so a stale entry
/// for a profile that is no longer running is pruned rather than accumulated.
fn idle_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join("browser-idle.json")
}

fn read_idle(home: &std::path::Path) -> HashMap<String, f64> {
    std::fs::read_to_string(idle_path(home))
        .ok()
        .and_then(|raw| serde_json::from_str::<HashMap<String, f64>>(&raw).ok())
        .unwrap_or_default()
}

fn write_idle(home: &std::path::Path, m: &HashMap<String, f64>) {
    if let Ok(text) = serde_json::to_string(m) {
        let _ = std::fs::write(idle_path(home), text);
    }
}

/// Does this CDP listing contain a real page?
///
/// Split out and pure so the rule is testable without a Chrome (ethos rule 7):
/// every live browser test in this repo is `#[ignore]`d and never runs in CI, so
/// a live-only test here would be a check that cannot fail.
///
/// `about:blank` and `chrome://` internals do NOT count as real. They are what
/// Chrome spawns on its own — a new-tab page, a popup opener — and counting
/// them would mean a browser Chrome keeps re-blanking is never reapable, which
/// is exactly the 18-hour case that prompted this.
pub fn has_real_page(targets: &[serde_json::Value]) -> bool {
    targets.iter().any(|t| {
        if t.get("type").and_then(serde_json::Value::as_str) != Some("page") {
            return false;
        }
        let u = t.get("url").and_then(serde_json::Value::as_str).unwrap_or("");
        !(u.is_empty() || u == "about:blank" || u.starts_with("chrome://"))
    })
}

/// Should this profile be released now (idle arm)?
///
/// Pure, so the whole decision has cells rather than only its plumbing. `None`
/// for `first_empty` means "not empty at this check", which must reset rather
/// than accumulate — otherwise a browser used every ten minutes still reaps.
pub fn should_reap(first_empty: Option<f64>, now: f64, after_s: u64) -> bool {
    if after_s == 0 {
        return false;
    }
    first_empty.is_some_and(|t| now - t >= after_s as f64)
}

/// Should this profile be released now (TTL arm)?
///
/// `started_at` is the Unix timestamp the browser was launched. Pure function
/// so the boundary is testable without a live Chrome.
pub fn should_reap_ttl(started_at: i64, now: f64, ttl: u64) -> bool {
    if ttl == 0 {
        return false;
    }
    now - started_at as f64 >= ttl as f64
}

fn now_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// One pass. Returns the profiles it released, so the caller can log a fact
/// rather than an intention.
/// Delete unregistered profiles nobody has used inside the TTL.
///
/// Goes through `delete_profile`, which already refuses `default` and anything
/// resolving outside the amux-owned tree, rather than adding a second
/// `remove_dir_all` to a subsystem whose comments record three separate
/// incidents of destroying people's staged logins. One producer, one set of
/// guards.
///
/// Returns the names removed, for the caller's log line.
pub async fn reap_stale_profiles(home: &std::path::Path) -> Vec<String> {
    let ttl = profile_ttl_days();
    if ttl == 0 {
        return vec![];
    }
    let running: std::collections::HashSet<String> = crate::integrations::browser::running_all()
        .into_iter()
        .map(|(profile, ..)| profile)
        .collect();
    let now = now_f64();
    let mut removed = vec![];
    for p in crate::integrations::browser::list_profiles(home, false) {
        let age_days = p.last_used.map(|lu| (now - lu as f64) / 86_400.0);
        if !should_reap_profile(&p.name, p.registered, running.contains(&p.name), age_days, ttl) {
            continue;
        }
        match crate::integrations::browser::delete_profile(home, &p.name) {
            Ok(_) => {
                tracing::info!(
                    profile = %p.name, age_days = age_days.unwrap_or(-1.0) as i64, ttl_days = ttl,
                    "browser: deleted an unregistered profile unused for the whole TTL                      (AMUX_PROFILE_TTL_DAYS). Registered profiles are exempt at any age;                      save a profile to keep it."
                );
                removed.push(p.name);
            }
            // Reported, not swallowed: a profile that keeps failing to delete
            // would otherwise retry silently every tick forever.
            Err((code, body)) => tracing::warn!(
                profile = %p.name, code, error = %body,
                "browser: profile TTL could not delete this profile"
            ),
        }
    }
    removed
}


/// Why a browser was released. One variant per arm of `tick`, so the notice
/// cannot drift from the reason the log line gives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReapReason {
    /// No driver verb for the whole activity window — nobody was driving it.
    NoActivity { since_verb_s: i64, window_s: u64 },
    /// Hard age ceiling, regardless of what was open.
    Ttl { age_s: i64, ttl_s: u64 },
    /// No real page open for the whole idle window.
    Idle { idle_s: i64, window_s: u64 },
}

impl ReapReason {
    fn sentence(&self) -> String {
        match *self {
            ReapReason::NoActivity { since_verb_s, window_s } => format!(
                "nothing had driven it for {}min (the activity window is {}min)",
                since_verb_s / 60,
                window_s / 60
            ),
            ReapReason::Ttl { age_s, ttl_s } => format!(
                "it hit the hard age ceiling — open {}min, limit {}min",
                age_s / 60,
                ttl_s / 60
            ),
            ReapReason::Idle { idle_s, window_s } => format!(
                "it had no page open for {}min (the idle window is {}min)",
                idle_s / 60,
                window_s / 60
            ),
        }
    }
}

/// What the owning lane is told when amux closes its browser (AF-497).
///
/// The reaper already knows the profile, the owner and the reason, and writes
/// all three to the log and to LAST_EXIT. It told the one party who needed it
/// nothing. Measured in a live onboarding session (2026-09-04): a Chrome window
/// disappeared mid-task and the exchange was "where'd the browser go?" / "did it
/// close?" / "it did, but I didn't close it" — a human watching a window vanish,
/// guessing, and landing on "it has, like, a zombie thing".
///
/// Addressed to the LANE and explicitly asks it to tell the person, because the
/// person is the one who saw it happen and amux has no channel to them.
///
/// Pure, so what it says is testable without a Chrome or a reaper tick.
pub fn reap_notice(profile: &str, reason: ReapReason) -> String {
    format!(
        "[amux] I closed your browser on profile {profile:?}: {}.\n\n\
         NOTHING WAS LOST that was saved: the profile's logins, cookies and \
         sessions are on disk and survive. What is gone is the running window and \
         whatever was only in it — an unsubmitted form, an unsaved page.\n\n\
         IF A PERSON WAS USING THAT WINDOW, TELL THEM. They watched it disappear \
         and amux has no way to reach them; from where they sit a window vanished \
         for no reason. Say that amux closed it, say why, and say their logins \
         survived.\n\n\
         Reopen it whenever you need it:\n\
         POST /api/browser/start {{\"profile\":\"{profile}\"}}\n\n\
         To stop this happening mid-task, widen or disable the window: \
         AMUX_BROWSER_ACTIVITY_REAP_S, AMUX_BROWSER_TTL_S, AMUX_BROWSER_REAP_AFTER_S \
         (0 disables an arm) in ~/.amux/server.env.",
        reason.sentence()
    )
}

/// Deliver the reap notice to the lane that started the browser.
///
/// Best-effort and never fatal: a reap that succeeded must not be reported as a
/// failure because a notice could not be delivered. An owner amux cannot name is
/// the one case with nothing to send to, and it is LOGGED rather than dropped —
/// a browser reaped with nobody told is exactly the silence this exists to end.
async fn notify_owner_of_reap(
    store: &crate::db::SharedStore,
    owner: &str,
    profile: &str,
    reason: ReapReason,
) {
    let owner = owner.trim();
    if owner.is_empty() {
        tracing::warn!(
            profile = %profile,
            "browser: reaped a profile with no recorded owner — nobody could be told (AF-497)"
        );
        return;
    }
    let text = reap_notice(profile, reason);
    match crate::api::session_verbs::steer_enqueue_store(
        store, owner, &text, "browser-reaper", "browser-reaper",
    )
    .await
    {
        Ok(_) => tracing::info!(
            profile = %profile, owner = %owner, reason = ?reason,
            "browser: told the owning lane its browser was released (AF-497)"
        ),
        // An isolated lane refuses harness sends by design, and that refusal is
        // correct rather than a fault — say which it was.
        Err(e) => tracing::warn!(
            profile = %profile, owner = %owner, reason = ?reason, error = %e,
            "browser: could not tell the owning lane its browser was released (AF-497)"
        ),
    }
}

async fn tick(home: &std::path::Path, store: Option<&crate::db::SharedStore>) -> Vec<String> {
    // Runs BEFORE the early return below: the profile TTL is about disk that
    // nobody is using, and the idle arm is about a running browser with no
    // pages. Disabling one must not silently disable the other — they were
    // independent knobs the moment there were two of them.
    reap_stale_profiles(home).await;
    let after_s = reap_after_s();
    if after_s == 0 {
        return vec![];
    }
    let mut reaped = vec![];
    let now = now_f64();
    // `None` when the process never recorded a boot (tests), which the log line
    // then reports as absent rather than as "none of the window predates this
    // process" — a 0 there would be a claim nobody measured.
    let boot = crate::runtime_jobs::heartbeat::boot_at();
    let prior = read_idle(home);
    let mut next: HashMap<String, f64> = HashMap::new();
    let ttl = ttl_s();
    let activity_ttl = activity_reap_s();
    for (profile, owner, started, _pid, port, last_verb) in crate::integrations::browser::running_all() {
        // ACTIVITY ARM: no verb for N seconds = abandoned, release it. Checked
        // before the page-presence arms because it fires fastest and the reason
        // is the most actionable ("nobody is driving this") not just "it is old".
        if activity_ttl > 0 {
            let since_verb = now - last_verb as f64;
            if since_verb >= activity_ttl as f64 {
                tracing::info!(
                    profile = %profile, owner = %owner,
                    since_verb_s = since_verb as i64, activity_ttl,
                    "browser: no activity for the whole window — releasing \
                     (AMUX_BROWSER_ACTIVITY_REAP_S). Logins survive on disk."
                );
                crate::integrations::browser::stop_profile_as(home, &profile, "activity-reaper").await;
                if let Some(st) = store {
                    notify_owner_of_reap(st, &owner, &profile, ReapReason::NoActivity {
                        since_verb_s: since_verb as i64,
                        window_s: activity_ttl,
                    })
                    .await;
                }
                next.remove(&profile);
                reaped.push(profile);
                continue;
            }
        }
        // TTL ARM: hard ceiling regardless of page state. Checked first so a
        // browser that hit the TTL is not also logged as "idle" — one reason,
        // one log line (2026-08-30: 20+ Chrome instances in the dock, none
        // caught by the idle arm because they all had at least one open page).
        if should_reap_ttl(started, now, ttl) {
            let age_s = now - started as f64;
            tracing::warn!(
                profile = %profile, owner = %owner, age_s = age_s as i64, ttl,
                "browser: TTL exceeded — releasing profile (AMUX_BROWSER_TTL_S). \
                 Any open page is lost; saved login is on disk and survives."
            );
            crate::integrations::browser::stop_profile_as(home, &profile, "ttl-reaper").await;
            if let Some(st) = store {
                notify_owner_of_reap(st, &owner, &profile, ReapReason::Ttl {
                    age_s: age_s as i64,
                    ttl_s: ttl,
                })
                .await;
            }
            next.remove(&profile);
            reaped.push(profile);
            continue;
        }
        // CDP SILENCE IS NOT EMPTINESS. A browser that will not answer is left
        // alone: killing it would turn a transient wedge into a destroyed
        // session, and this job's whole safety argument rests on knowing there
        // is nothing open. Dropping the entry (rather than carrying it) resets
        // the window, which is the conservative direction.
        let Ok(listed) = crate::integrations::browser::cdp_list(port).await else {
            continue;
        };
        let empty = listed.as_array().map(|a| !has_real_page(a)).unwrap_or(false);
        if !empty {
            // One real page anywhere resets the clock: the entry is simply not
            // carried into `next`, which is written back at the end.
            continue;
        }
        let first_empty = *prior.get(&profile).unwrap_or(&now);
        next.insert(profile.clone(), first_empty);
        if !should_reap(Some(first_empty), now, after_s) {
            continue;
        }
        let idle_s = now - first_empty;
        tracing::info!(
            profile = %profile, owner = %owner, idle_s = idle_s as i64, after_s,
            // How much of the window predates this process. A non-zero value here
            // IS the restart-survival working; the in-memory version could only
            // ever print 0 and nobody would have known to look (AMUX-3829).
            pre_boot_s = boot.map(|b| (b - first_empty).max(0.0) as i64).unwrap_or(-1),
            "browser: releasing a profile with no real page open for the whole idle window \
             (AMUX-3829). Logins are on disk and survive; only a relaunch is lost."
        );
        // `stop_profile_as` records WHO in LAST_EXIT, so the next start can say
        // what happened rather than leaving the AMUX-3414 silence — a browser
        // that vanished with nothing on record cost two sessions a morning.
        crate::integrations::browser::stop_profile_as(home, &profile, "idle-reaper").await;
        if let Some(st) = store {
            notify_owner_of_reap(st, &owner, &profile, ReapReason::Idle {
                idle_s: idle_s as i64,
                window_s: after_s,
            })
            .await;
        }
        next.remove(&profile);
        reaped.push(profile);
    }
    // Rewritten whole, so a profile that stopped running drops out instead of
    // accumulating: at 100x the browsers this is still one small file.
    if next != prior {
        write_idle(home, &next);
    }
    reaped
}

/// How long each running profile has been continuously empty, for
/// `/api/browser/status` (AMUX-3829). The countdown was invisible: the only way
/// to know whether a browser was about to be reaped was to read the process's
/// memory, so a reaper that could never fire looked identical to one that was
/// about to.
pub fn idle_ages(home: &std::path::Path, now: f64) -> HashMap<String, f64> {
    read_idle(home).into_iter().map(|(k, t)| (k, (now - t).max(0.0))).collect()
}

/// Spawn the loop, registered so a dead reaper is visible on
/// `/api/system-jobs` rather than silently absent.
pub fn spawn(store: crate::db::SharedStore) {
    let interval = Duration::from_secs(tick_secs());
    let h = tokio::spawn(async move {
        let mut t = tokio::time::interval(interval);
        loop {
            t.tick().await;
            crate::runtime_jobs::registry::tick(crate::runtime_jobs::registry::ids::BROWSER_REAPER);
            let home = crate::integrations::browser::amux_home();
            let _ = tick(&home, Some(&store)).await;
        }
    });
    crate::runtime_jobs::registry::adopt(
        crate::runtime_jobs::registry::ids::BROWSER_REAPER,
        Some(interval),
        &h,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// What counts as a real page, and what Chrome spawns on its own.
    /// THE WIRING, not the wording.
    ///
    /// Every other cell here reads `reap_notice`'s TEXT, and all of them stay
    /// green if `tick` never calls it — measured: disabling the call site left
    /// 11/11 passing, which is the feature silently deleted and the suite
    /// reporting success. This one drives the real `tick` and reads the queue.
    ///
    /// The activity arm is the one exercised because it fires before any CDP
    /// call, so the reap happens without a Chrome. `u32::MAX` is intentionally
    /// retained as the regression sentinel: Linux procps interpreted that
    /// unsigned value as the process-group pid -1 and terminated the CI runner
    /// until the browser signal boundary began rejecting it (ATE-44).
    #[tokio::test]
    async fn a_reap_actually_enqueues_the_notice_for_the_owning_lane() {
        let home = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let store: crate::db::SharedStore =
            std::sync::Arc::new(crate::db::Store::open(&db.path().join("t.db")).unwrap());

        // Prove the target exists through the durable worker row that the
        // enqueue chokepoint already understands. Do not take HomeGuard here:
        // this test also exercises the process-global browser registry, and a
        // parallel browser test can take those two locks in the opposite order.
        // The first guarded draft wedged 14 unrelated tests in the full suite.
        store
            .write_async(|conn| {
                conn.execute(
                    "INSERT INTO _amux_workers \
                     (id, display_name, name_aliases, cwd, provider, model, state, created_at, updated_at) \
                     VALUES ('wrk_reaper_notice', 'gtm-engine', '[]', '/tmp', 'claude', \
                             'test', '{\"state\":\"stopped\"}', \
                             '2026-09-04T00:00:00Z', '2026-09-04T00:00:00Z')",
                    [],
                )?;
                Ok(crate::db::WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .await
            .unwrap();

        // SAFETY: this is the only test that mutates these two variables, and
        // both are restored below before the test returns.
        let prior_after = std::env::var("AMUX_BROWSER_REAP_AFTER_S").ok();
        let prior_act = std::env::var("AMUX_BROWSER_ACTIVITY_REAP_S").ok();
        unsafe { std::env::set_var("AMUX_BROWSER_REAP_AFTER_S", "60") };
        unsafe { std::env::set_var("AMUX_BROWSER_ACTIVITY_REAP_S", "1") };

        crate::integrations::browser::test_clear_running();
        crate::integrations::browser::test_seed_running_port("hubspot", "gtm-engine", u32::MAX, 1);

        let reaped = crate::integrations::browser::test_with_kill_capture(async {
            let reaped = tick(home.path(), Some(&store)).await;
            assert!(
                crate::integrations::browser::test_kill_commands().is_empty(),
                "u32::MAX reached /bin/kill through the real reaper stop path"
            );
            assert!(
                crate::integrations::browser::test_normal_pid_reaches_kill(std::process::id())
                    .await,
                "the normal positive-pid control did not invoke a successful kill -0"
            );
            assert_eq!(
                crate::integrations::browser::test_kill_commands(),
                vec![["-0".to_string(), std::process::id().to_string()]],
                "only the normal control may reach /bin/kill"
            );
            reaped
        })
        .await;
        assert_eq!(reaped, vec!["hubspot".to_string()], "the activity arm did not fire");

        let rows: Vec<(String, String)> = {
            let conn = store.read().unwrap();
            let mut st = conn.prepare("SELECT session, text FROM steering_queue").unwrap();
            let out = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .filter_map(Result::ok)
                .collect();
            out
        };
        crate::integrations::browser::test_clear_running();
        match prior_after {
            Some(v) => unsafe { std::env::set_var("AMUX_BROWSER_REAP_AFTER_S", v) },
            None => unsafe { std::env::remove_var("AMUX_BROWSER_REAP_AFTER_S") },
        }
        match prior_act {
            Some(v) => unsafe { std::env::set_var("AMUX_BROWSER_ACTIVITY_REAP_S", v) },
            None => unsafe { std::env::remove_var("AMUX_BROWSER_ACTIVITY_REAP_S") },
        }

        let mine: Vec<&(String, String)> =
            rows.iter().filter(|(s, _)| s == "gtm-engine").collect();
        assert_eq!(
            mine.len(),
            1,
            "the reap did not enqueue exactly one notice for the owning lane: {rows:?}"
        );
        assert!(
            mine[0].1.contains("hubspot") && mine[0].1.contains("TELL THEM"),
            "the queued text is not the reap notice: {}",
            mine[0].1
        );
    }

    /// THE CELL THIS EXISTS FOR. The reaper already knew the profile, the owner
    /// and the reason and wrote all three to the log; the one party who needed
    /// them got nothing. The notice has to carry each, and it has to tell the
    /// lane to pass it on — the PERSON is the one who watched the window vanish
    /// and amux has no channel to them.
    #[test]
    fn the_notice_names_the_profile_the_reason_and_asks_the_lane_to_tell_the_person() {
        let n = reap_notice("hubspot", ReapReason::Idle { idle_s: 1800, window_s: 900 });
        assert!(n.contains("hubspot"), "the profile is not named: {n}");
        assert!(n.contains("no page open for 30min"), "the reason is not stated: {n}");
        assert!(
            n.contains("IF A PERSON WAS USING THAT WINDOW, TELL THEM"),
            "the notice never asks the lane to pass it on, which is the whole gap: {n}"
        );
    }

    /// The sentence that stops a panic. A vanished browser reads as lost logins,
    /// and the specimen's user had just spent minutes recovering a password.
    #[test]
    fn the_notice_says_the_logins_survived() {
        for r in [
            ReapReason::Idle { idle_s: 900, window_s: 900 },
            ReapReason::Ttl { age_s: 7200, ttl_s: 3600 },
            ReapReason::NoActivity { since_verb_s: 600, window_s: 600 },
        ] {
            let n = reap_notice("default", r);
            assert!(n.contains("survive"), "no reassurance about saved state in {r:?}: {n}");
            assert!(n.contains("/api/browser/start"), "no way back in {r:?}: {n}");
        }
    }

    /// Each arm says something DIFFERENT. Three reaper arms fire for three
    /// different reasons and need three different responses from the reader —
    /// "nobody was driving it" and "it hit a hard ceiling" are not the same
    /// news, and a shared sentence would make them read as one.
    #[test]
    fn every_reap_reason_produces_its_own_sentence() {
        let a = reap_notice("p", ReapReason::NoActivity { since_verb_s: 600, window_s: 600 });
        let b = reap_notice("p", ReapReason::Ttl { age_s: 7200, ttl_s: 3600 });
        let c = reap_notice("p", ReapReason::Idle { idle_s: 900, window_s: 900 });
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        assert!(a.contains("nothing had driven it"), "{a}");
        assert!(b.contains("hard age ceiling"), "{b}");
        assert!(c.contains("no page open"), "{c}");
    }

    /// The knobs are named. Without them the only response to "amux closed my
    /// browser mid-task" is to hit start again and wait for it to happen twice
    /// more.
    #[test]
    fn the_notice_names_the_knobs_that_prevent_a_recurrence() {
        let n = reap_notice("default", ReapReason::Ttl { age_s: 7200, ttl_s: 3600 });
        for knob in ["AMUX_BROWSER_ACTIVITY_REAP_S", "AMUX_BROWSER_TTL_S", "AMUX_BROWSER_REAP_AFTER_S"] {
            assert!(n.contains(knob), "{knob} is not offered: {n}");
        }
    }

    #[test]
    fn only_a_real_page_keeps_a_browser_alive() {
        let page = |u: &str| json!({"type": "page", "url": u});
        assert!(has_real_page(&[page("https://studio.mixpeek.com/x")]));
        // THE CASE THAT PROMPTED THIS: Chrome respawns blanks, so counting them
        // would make a browser permanently unreapable — which is the 18-hour
        // zombie Ethan hit.
        assert!(!has_real_page(&[page("about:blank")]));
        assert!(!has_real_page(&[page("chrome://newtab/")]));
        assert!(!has_real_page(&[page("")]));
        assert!(!has_real_page(&[]), "no targets at all is empty");
        // Non-page targets (iframes, service workers) are not pages.
        assert!(!has_real_page(&[json!({"type": "iframe", "url": "https://x.com/"})]));
        // CONTROL: one real page among blanks keeps it alive. A predicate that
        // ignored the real one would reap a browser someone is using.
        assert!(has_real_page(&[page("about:blank"), page("https://x.com/")]));
    }

    /// Activity arm fires when no verb has been called for the whole window.
    #[test]
    fn activity_arm_fires_on_verb_silence() {
        let five_min = 300u64;
        let now = 10_000.0f64;
        // Last verb 6 minutes ago -> reap.
        assert!(should_reap_ttl(now as i64 - 360, now, five_min),
            "360s silence with 300s window should reap");
        // Last verb 4 minutes ago -> keep.
        assert!(!should_reap_ttl(now as i64 - 240, now, five_min),
            "240s silence with 300s window should keep");
        // Disabled (0) -> never.
        assert!(!should_reap_ttl(0, now, 0));
    }

    /// TTL arm fires on age, not emptiness — even a browser with open pages is
    /// released once it passes the ceiling (2026-08-30: 20+ Chrome instances).
    #[test]
    fn ttl_reaps_old_browsers_regardless_of_pages() {
        let four_h = 4 * 3600u64;
        let now = 10_000.0f64;
        // Started just over 4 hours ago -> release.
        assert!(should_reap_ttl(now as i64 - four_h as i64 - 1, now, four_h));
        // Started exactly at the boundary -> release (>= not >).
        assert!(should_reap_ttl(now as i64 - four_h as i64, now, four_h));
        // Started 1 second short of the TTL -> keep.
        assert!(!should_reap_ttl(now as i64 - four_h as i64 + 1, now, four_h));
        // TTL disabled (0) -> never reap, regardless of age.
        assert!(!should_reap_ttl(0, now, 0));
    }

    /// AMUX-3829, second pass. The window must SURVIVE a restart, because the
    /// builder installs a new binary and the server self-adopts on every commit.
    /// Measured the day this was found: 22 builds in 10 hours, median gap 16.7
    /// minutes, only TWO gaps past the 3600s window. An in-memory clock made
    /// this a reaper that could almost never fire while reporting itself
    /// healthy, since the loop was running perfectly the whole time.
    #[test]
    fn the_idle_clock_is_read_back_from_disk_rather_than_restarting() {
        let tmp = std::env::temp_dir().join(format!("amux-reap-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("tmpdir");

        // Nothing recorded yet: absent, not zero.
        assert!(super::read_idle(&tmp).is_empty());
        assert!(super::idle_ages(&tmp, 1000.0).is_empty());

        // A window that began 50 minutes ago, written by a PREVIOUS process.
        let mut m = std::collections::HashMap::new();
        m.insert("atlas".to_string(), 1000.0);
        super::write_idle(&tmp, &m);

        // A fresh process reads it back and the clock keeps running. This is the
        // assertion the in-memory version could not pass.
        assert_eq!(super::read_idle(&tmp).get("atlas"), Some(&1000.0));
        assert_eq!(super::idle_ages(&tmp, 4000.0).get("atlas"), Some(&3000.0));
        // And the reap decision therefore fires on the CARRIED window: 3600s
        // after the original first-empty, not 3600s after this process started.
        assert!(should_reap(Some(1000.0), 4600.0, 3600));
        assert!(!should_reap(Some(1000.0), 4599.0, 3600));

        // A corrupt or truncated file reads as "nothing recorded" rather than
        // panicking or inventing a timestamp — the conservative direction, since
        // it restarts the window instead of reaping early.
        std::fs::write(super::idle_path(&tmp), "{not json").expect("write");
        assert!(super::read_idle(&tmp).is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Idle is CONTINUOUS emptiness, and the window is a floor not a ceiling.
    #[test]
    fn a_profile_is_released_only_after_the_whole_window_empty() {
        // Empty for the full hour -> release.
        assert!(should_reap(Some(0.0), 3600.0, 3600));
        assert!(should_reap(Some(0.0), 99_999.0, 3600));
        // Short of it -> keep.
        assert!(!should_reap(Some(0.0), 3599.0, 3600));
        // NOT EMPTY AT THIS CHECK -> never. This is the cell that stops a
        // browser used every ten minutes from accumulating its way to a reap:
        // the caller passes None whenever a real page is present, which resets.
        assert!(!should_reap(None, 99_999.0, 3600));
        // DISABLED means disabled, at any age — the off switch has to work or
        // it is not an off switch (ethos rule 6).
        assert!(!should_reap(Some(0.0), 99_999.0, 0));
    }
}

#[cfg(test)]
mod profile_ttl_tests {
    use super::*;

    /// Ethan, 2026-08-30: "we should have a ttl for the browser profiles. that
    /// way we don't create so many at any point and never use them."
    ///
    /// The danger in a profile reaper is not failing to delete. It is deleting a
    /// login someone needs, in a subsystem whose own module doc records three
    /// separate incidents of destroying people's staged logins. So every
    /// exemption below carries the reaping case next to it: an exemption that
    /// never lets anything through is indistinguishable from a disabled job.
    #[test]
    fn the_profile_ttl_reaps_sediment_and_spares_everything_with_a_reason() {
        const TTL: u64 = 30;
        let old = Some(171.1); // `test-heroku`, the oldest real one on 2026-08-30
        let new = Some(0.2);

        // The 32 profiles this exists for: unregistered, unused, not running.
        assert!(should_reap_profile("test-heroku", false, false, old, TTL));
        // CONTROL, or the assertion above only proves the function returns true.
        assert!(
            !should_reap_profile("propelauth", false, false, new, TTL),
            "a profile used today must survive"
        );

        // REGISTERED IS EXEMPT AT ANY AGE. `recreation-gov` was 30.7 days idle
        // and is a deliberate save someone may need twice a year.
        assert!(
            !should_reap_profile("recreation-gov", true, false, old, TTL),
            "a registered profile is a deliberate login and must never age out"
        );

        // RUNNING IS EXEMPT. Age comes from the directory mtime and a browser
        // can be driven for hours without Chrome touching the dir, so this is
        // what stops one worker's reap deleting the profile another worker is
        // driving right now.
        assert!(
            !should_reap_profile("mixpeek-studio", false, true, old, TTL),
            "a profile with a live browser must not be deleted under it"
        );

        // `default` is refused here as well as in delete_profile.
        assert!(!should_reap_profile("default", false, false, old, TTL));

        // UNKNOWN AGE DECLINES TO ACT. `last_used` is an Option because the stat
        // can fail; reading None as "very old" would delete a profile whose age
        // was never measured.
        assert!(
            !should_reap_profile("unmeasurable", false, false, None, TTL),
            "an unreadable mtime is unknown age, not infinite age"
        );

        // The knob genuinely disables the arm.
        assert!(!should_reap_profile("test-heroku", false, false, old, 0));

        // The boundary is a decision, not an accident of `>` vs `>=`.
        assert!(should_reap_profile("x", false, false, Some(30.0), TTL));
        assert!(!should_reap_profile("x", false, false, Some(29.9), TTL));
    }

    /// MULTIPLE PROFILES AT ONCE (Ethan's other requirement in the same breath).
    /// The reap decision is per profile and keys on THAT profile's own running
    /// flag, so N workers on N browsers is N independent verdicts. A rule that
    /// consulted "is any browser running" would spare everything whenever one
    /// lane held a browser, which reads as working and quietly never reaps.
    #[test]
    fn one_running_profile_does_not_spare_or_doom_its_neighbours() {
        const TTL: u64 = 30;
        let old = Some(200.0);
        // Worker A is driving `alpha`; worker B's `beta` is ancient junk.
        assert!(!should_reap_profile("alpha", false, true, old, TTL));
        assert!(should_reap_profile("beta", false, false, old, TTL));
    }
}
