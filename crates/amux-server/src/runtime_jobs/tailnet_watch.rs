//! Tailnet watch — node-key expiry and reachability, sampled and cached.
//!
//! # Why this exists
//!
//! On 2026-08-23 an audit of this machine found node `desktop`'s Tailscale key
//! expiring 2027-02-13, roughly 175 days out. Nothing in amux watched it. A
//! Tailscale node key expiring does not degrade anything gradually: the node
//! drops off the tailnet and needs INTERACTIVE re-auth in the GUI app. On this
//! host that is also the moment nobody can reach the box to perform it, because
//! SSH and the dashboard both arrive over the tailnet (DESKT-21, DESKT-23).
//!
//! So the failure is scheduled, silent, and self-sealing. The only useful
//! signal is one that fires with enough runway to act, on a surface someone
//! already reads.
//!
//! # Why a cached job rather than a read inside /health
//!
//! `health.rs` says it plainly for its own fields: "no subprocess, for the
//! fd_health reason: spawning costs the resources being measured, and fails
//! exactly when the condition it reports is present." A fork+exec of the
//! tailscale CLI on every /health request would be a detector whose cost is
//! paid in the same resource as the fault (ethos rule 7), on the endpoint the
//! whole fleet polls. This job samples on a slow tick and publishes the cached
//! verdict; /health only reads memory.
//!
//! # What the thresholds are, and why they are not tunable knobs
//!
//! Expiry is a DATE, so the threshold is runway rather than a tuned level:
//! warn at 30 days (time to notice and act without urgency), critical at 7
//! (act now). Reachability is not thresholded at all — `BackendState` other
//! than `Running` is the structurally-absent signal, present only when
//! something is genuinely wrong, which is the shape ethos rule 7 prefers over
//! a number someone picked.
//!
//! # The case that must NOT alarm
//!
//! A host with no tailscale binary — the cloud container, or any OSS user who
//! does not use Tailscale — reports `unknown`, not `critical`. amux genuinely
//! cannot tell whether such a host was ever meant to be on a tailnet, and an
//! alarm that fires on every install without Tailscale is noise that teaches
//! people to ignore the one that matters. Equally, a node whose key expiry has
//! been DISABLED reports `ok` with no expiry, because that is the recommended
//! end state for a server, not a missing reading.

use serde::Serialize;
use std::sync::RwLock;

pub const JOB: &str = crate::runtime_jobs::registry::ids::TAILNET_WATCH;

/// Key expiry moves on a scale of days and reachability on a scale of minutes;
/// 15 minutes is far inside the runway for the first and responsive enough for
/// the second, at one subprocess per quarter hour.
const TICK_SECS: u64 = 900;

const WARN_DAYS: f64 = 30.0;
const CRITICAL_DAYS: f64 = 7.0;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TailnetHealth {
    /// "ok" | "warn" | "critical" | "unknown"
    pub state: &'static str,
    /// Tailscale's own word for what the backend is doing: `Running`,
    /// `Stopped`, `NeedsLogin`, … Reported verbatim rather than mapped, so a
    /// state this code has never heard of still reaches the reader.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_state: Option<String>,
    /// `None` when key expiry is DISABLED, which is the good end state for a
    /// server and must not read as a missing measurement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_expiry_days: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_expiry: Option<String>,
    /// Why the state is what it is, in words. The field alone is only useful to
    /// someone who already knows the vocabulary.
    pub detail: String,
    /// Unix seconds of the sample this verdict came from. A stale cache and a
    /// fresh one are different facts, so the reader gets the timestamp rather
    /// than an implicit promise of freshness.
    pub measured_at: f64,
}

static LAST: RwLock<Option<TailnetHealth>> = RwLock::new(None);

/// The cached verdict, for `/health`. `None` before the first tick completes —
/// deliberately distinct from `unknown`, which means a tick RAN and could not
/// determine the answer.
pub fn cached() -> Option<TailnetHealth> {
    LAST.read().ok().and_then(|g| g.clone())
}

/// Everything the CLI told us, already parsed. Separated from the verdict so
/// [`classify`] is testable against recorded shapes with no tailscale present.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Reading {
    pub backend_state: Option<String>,
    pub self_online: Option<bool>,
    /// RFC3339 as Tailscale prints it, or `None` when expiry is disabled.
    pub key_expiry: Option<String>,
    /// Set when the CLI could not be run or its output not parsed. Keeps
    /// "could not look" from being laundered into a health verdict.
    pub unavailable: Option<String>,
}

/// The whole verdict, as a pure function of a reading and a clock.
pub fn classify(r: &Reading, now: f64) -> TailnetHealth {
    let mk = |state: &'static str, detail: String, days: Option<f64>| TailnetHealth {
        state,
        backend_state: r.backend_state.clone(),
        key_expiry_days: days,
        key_expiry: r.key_expiry.clone(),
        detail,
        measured_at: now,
    };

    if let Some(why) = &r.unavailable {
        return mk("unknown", why.clone(), None);
    }

    // Reachability first: a node that is off the tailnet right now outranks a
    // key that expires in a month.
    match r.backend_state.as_deref() {
        Some("Running") => {}
        Some(other) => {
            return mk(
                "critical",
                format!(
                    "tailscale backend is {other}, not Running — this host is OFF the \
                     tailnet, so anything reaching it over the tailnet (ssh, the \
                     dashboard) is unreachable right now"
                ),
                None,
            )
        }
        None => return mk("unknown", "tailscale reported no backend state".into(), None),
    }
    if r.self_online == Some(false) {
        return mk(
            "critical",
            "tailscale backend is Running but this node reports itself OFFLINE — it \
             cannot reach the control plane or any peer"
                .into(),
            None,
        );
    }

    let Some(expiry) = &r.key_expiry else {
        return mk(
            "ok",
            "backend Running, node online, and key expiry is DISABLED — the recommended \
             end state for an always-on host"
                .into(),
            None,
        );
    };
    let Some(exp_ts) = parse_rfc3339(expiry) else {
        return mk("unknown", format!("could not parse key expiry {expiry:?}"), None);
    };
    let days = (exp_ts - now) / 86_400.0;
    let rounded = (days * 10.0).round() / 10.0;
    if days < CRITICAL_DAYS {
        mk(
            "critical",
            format!(
                "node key expires in {rounded} days ({expiry}). On expiry this node drops \
                 off the tailnet and needs INTERACTIVE re-auth in the GUI — which is also \
                 when nobody can reach this host remotely to do it. Disable key expiry in \
                 the admin console (Machines -> this node -> Disable key expiry)"
            ),
            Some(rounded),
        )
    } else if days < WARN_DAYS {
        mk(
            "warn",
            format!(
                "node key expires in {rounded} days ({expiry}) — disable key expiry in the \
                 admin console before it takes remote access with it"
            ),
            Some(rounded),
        )
    } else {
        mk(
            "ok",
            format!("backend Running, node online, key valid for {rounded} more days"),
            Some(rounded),
        )
    }
}

fn parse_rfc3339(s: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp() as f64)
}

/// Locations to try, in order, before giving up. `tailscale` on PATH covers
/// Linux and Homebrew; the .app path is where the macOS standalone build puts
/// the real binary, and `/usr/local/bin/tailscale` there is only a shell shim
/// to it — which is fine to exec, but the direct path survives the shim being
/// absent.
const CANDIDATES: &[&str] = &[
    "tailscale",
    "/usr/local/bin/tailscale",
    "/Applications/Tailscale.app/Contents/MacOS/tailscale",
    "/usr/bin/tailscale",
];

fn read_status() -> Reading {
    for bin in CANDIDATES {
        let out = std::process::Command::new(bin).args(["status", "--json"]).output();
        let Ok(out) = out else { continue };
        if !out.status.success() {
            return Reading {
                unavailable: Some(format!(
                    "`{bin} status --json` exited {}",
                    out.status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into())
                )),
                ..Default::default()
            };
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
            return Reading {
                unavailable: Some(format!("`{bin} status --json` printed unparseable JSON")),
                ..Default::default()
            };
        };
        return Reading {
            backend_state: v.get("BackendState").and_then(|b| b.as_str()).map(String::from),
            self_online: v.get("Self").and_then(|s| s.get("Online")).and_then(|o| o.as_bool()),
            key_expiry: v
                .get("Self")
                .and_then(|s| s.get("KeyExpiry"))
                .and_then(|k| k.as_str())
                .map(String::from),
            unavailable: None,
        };
    }
    // No binary anywhere. NOT an alarm — see the module header.
    Reading {
        unavailable: Some(
            "no tailscale binary found — this host may simply not use Tailscale".into(),
        ),
        ..Default::default()
    }
}

fn tick() {
    let now = crate::runtime_jobs::registry::unix_now();
    let health = classify(&read_status(), now);
    // Log only when there is something to say. A line every 15 minutes saying
    // the tailnet is fine is the noise that buries the line that matters.
    match health.state {
        "critical" => tracing::warn!(job = JOB, state = "critical", detail = %health.detail, "tailnet"),
        "warn" => tracing::warn!(job = JOB, state = "warn", detail = %health.detail, "tailnet"),
        _ => tracing::debug!(job = JOB, state = health.state, detail = %health.detail, "tailnet"),
    }
    if let Ok(mut g) = LAST.write() {
        *g = Some(health);
    }
}

pub fn spawn() -> super::PeriodicTask {
    super::spawn_periodic(JOB, TICK_SECS, || async {
        let _ = tokio::task::spawn_blocking(tick).await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 1_787_500_000.0; // 2026-08-23

    fn running(expiry: Option<&str>) -> Reading {
        Reading {
            backend_state: Some("Running".into()),
            self_online: Some(true),
            key_expiry: expiry.map(String::from),
            unavailable: None,
        }
    }

    /// The reading this job was written for: node `desktop`, 2026-08-23,
    /// key expiring 2027-02-13. Far out, so it must be QUIET — a canary that
    /// fires 175 days early is one nobody reads on day 170.
    #[test]
    fn the_real_2026_08_23_reading_is_quiet() {
        let h = classify(&running(Some("2027-02-13T11:51:57Z")), NOW);
        assert_eq!(h.state, "ok", "{}", h.detail);
        assert!(h.key_expiry_days.unwrap() > 170.0);
    }

    /// …and the same node, walked forward, must escalate on schedule.
    #[test]
    fn the_same_node_escalates_as_the_date_approaches() {
        let expiry = "2027-02-13T11:51:57Z";
        let exp = parse_rfc3339(expiry).unwrap();
        assert_eq!(classify(&running(Some(expiry)), exp - 31.0 * 86_400.0).state, "ok");
        assert_eq!(classify(&running(Some(expiry)), exp - 29.0 * 86_400.0).state, "warn");
        assert_eq!(classify(&running(Some(expiry)), exp - 6.0 * 86_400.0).state, "critical");
        assert_eq!(classify(&running(Some(expiry)), exp + 86_400.0).state, "critical",
            "already expired is not suddenly fine again");
    }

    /// Expiry DISABLED is the recommended end state for an always-on host. It
    /// must read ok, never `unknown` — conflating "no expiry set" with "could
    /// not measure" would make doing the right thing look like a broken probe.
    #[test]
    fn disabled_key_expiry_is_ok_not_unknown() {
        let h = classify(&running(None), NOW);
        assert_eq!(h.state, "ok");
        assert_eq!(h.key_expiry_days, None);
        assert!(h.detail.contains("DISABLED"), "{}", h.detail);
    }

    /// A host with no tailscale must not alarm — the cloud container and any
    /// OSS user without Tailscale would otherwise sit permanently critical,
    /// which is how a real alarm gets trained out of people.
    #[test]
    fn a_host_without_tailscale_is_unknown_not_critical() {
        let r = Reading {
            unavailable: Some("no tailscale binary found".into()),
            ..Default::default()
        };
        assert_eq!(classify(&r, NOW).state, "unknown");
    }

    /// Reachability outranks expiry: a node that is off the tailnet NOW is not
    /// made healthy by a key that is valid for another year.
    #[test]
    fn being_off_the_tailnet_outranks_a_healthy_key() {
        let r = Reading {
            backend_state: Some("Stopped".into()),
            self_online: Some(true),
            key_expiry: Some("2027-02-13T11:51:57Z".into()),
            unavailable: None,
        };
        let h = classify(&r, NOW);
        assert_eq!(h.state, "critical");
        assert!(h.detail.contains("Stopped"), "the unmapped state reaches the reader: {}", h.detail);
    }

    /// Running-but-offline is its own fault and must not be swallowed by the
    /// BackendState check passing.
    #[test]
    fn running_but_offline_is_critical() {
        let mut r = running(Some("2027-02-13T11:51:57Z"));
        r.self_online = Some(false);
        assert_eq!(classify(&r, NOW).state, "critical");
    }

    /// The NEGATIVE half for the parser: an expiry we cannot read must not be
    /// silently treated as "no expiry", which would render as a confident ok.
    #[test]
    fn an_unparseable_expiry_is_unknown_not_ok() {
        let h = classify(&running(Some("whenever")), NOW);
        assert_eq!(h.state, "unknown");
    }

    /// Before the first tick, /health must be able to say "not measured yet"
    /// rather than inventing a verdict. No test in this module calls `tick`, so
    /// the cache is genuinely untouched here and this assertion is real.
    #[test]
    fn the_cache_starts_empty_so_health_can_omit_the_field() {
        assert!(cached().is_none(), "an unticked cache must not present a verdict");
    }

    /// The binary lookup must actually find tailscale ON A HOST THAT HAS IT.
    /// `CANDIDATES` is a guess about install locations, and a guess that misses
    /// renders as a permanent, quiet `unknown` — the exact failure this job
    /// exists to prevent, wearing the job's own uniform. Skipped rather than
    /// failed where tailscale is genuinely absent (CI, the cloud container),
    /// because there the `unknown` is correct.
    #[test]
    fn the_binary_lookup_works_where_tailscale_is_installed() {
        let installed = CANDIDATES.iter().any(|c| {
            std::path::Path::new(c).exists()
                || std::process::Command::new(c).arg("version").output().is_ok_and(|o| o.status.success())
        });
        if !installed {
            eprintln!("skipped: no tailscale on this host, so `unknown` is the right answer");
            return;
        }
        let r = read_status();
        assert!(
            r.unavailable.is_none(),
            "tailscale is installed but read_status could not use it: {:?}",
            r.unavailable
        );
        assert!(r.backend_state.is_some(), "a working CLI must report a BackendState");
        // And the verdict built from it must be a real one, not `unknown`.
        let h = classify(&r, crate::runtime_jobs::registry::unix_now());
        assert_ne!(h.state, "unknown", "live reading classified as unknown: {}", h.detail);
        eprintln!("live tailnet verdict: {} — {}", h.state, h.detail);
    }
}
