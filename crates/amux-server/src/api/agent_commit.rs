//! Who an agent's commit says made it (governance PR 15).
//!
//! Agent lanes committed as the human ("Ethan <ethan@mixpeek.com>"), and the
//! one field that did name the lane, `Amux-Session`, names a lane but not a
//! host. On 2026-11-10 this sandbox's deploy-wake told its `amux-helper` that
//! three commits "of yours" had gone live; all three were made by a different
//! lane, also called `amux-helper`, on Ethan's Mac (`Amux-Agent: pid=94822`).
//!
//! Three facts fix it, each from one source:
//! - **Author:** a per-worker bot identity, `<worker> (amux)
//!   <worker+<host8>@<domain>>`, set as GIT_AUTHOR_*/GIT_COMMITTER_* at launch.
//!   The domain is not a mailbox (`AMUX_AGENT_COMMIT_DOMAIN`, default
//!   `agents.noreply.amux.io`); GitHub shows the bot name, which is the point.
//! - **Host:** `Amux-Host: <host id>`, the id the ledger checkpoints already
//!   sign (`ledger_checkpoint::host_id`), delivered as `AMUX_HOST_ID`.
//! - **On whose behalf:** `On-behalf-of: owner | member:<email>`, written per
//!   delivery to `<home>/trace/<worker>.on-behalf-of` from the proven sender
//!   (instruction_provenance), else the worker's sponsor (schedule_authority),
//!   else the owner. The git hook stamps it.
//!
//! `AMUX_AGENT_COMMIT_IDENTITY=human` keeps the human author (nothing set); a
//! worker whose own scope file sets GIT_AUTHOR_NAME keeps that. Humans
//! committing outside a lane are untouched either way.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub const MODE_ENV: &str = "AMUX_AGENT_COMMIT_IDENTITY";
pub const DOMAIN_ENV: &str = "AMUX_AGENT_COMMIT_DOMAIN";
pub const DEFAULT_DOMAIN: &str = "agents.noreply.amux.io";
pub const HOST_ENV: &str = "AMUX_HOST_ID";

static BOT_N: AtomicU64 = AtomicU64::new(0);
static HUMAN_N: AtomicU64 = AtomicU64::new(0);
static KEPT_EXPLICIT_N: AtomicU64 = AtomicU64::new(0);
static ON_BEHALF_WRITTEN_N: AtomicU64 = AtomicU64::new(0);
static ON_BEHALF_FAILED_N: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Bot,
    Human,
}

pub fn mode_from(v: Option<&str>) -> Mode {
    match v.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("human") | Some("off") | Some("0") | Some("false") => Mode::Human,
        _ => Mode::Bot,
    }
}

pub fn mode() -> Mode {
    mode_from(std::env::var(MODE_ENV).ok().as_deref())
}

fn domain() -> String {
    std::env::var(DOMAIN_ENV)
        .ok()
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty() && !d.contains(char::is_whitespace) && !d.contains('@'))
        .unwrap_or_else(|| DEFAULT_DOMAIN.to_string())
}

/// `<worker> (amux)` and `<worker>+<host8>@<domain>`. The local part keeps
/// only what an address may hold, so any worker name yields a valid email.
pub fn identity(worker: &str, host: &str, domain: &str) -> (String, String) {
    let local: String = worker
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let local = if local.trim_matches(['-', '.']).is_empty() { "worker".to_string() } else { local };
    let host8: String = host.chars().filter(|c| c.is_ascii_hexdigit()).take(8).collect();
    let email = if host8.is_empty() {
        format!("{local}@{domain}")
    } else {
        format!("{local}+{host8}@{domain}")
    };
    (format!("{worker} (amux)"), email)
}

/// The environment a worker launches with. `explicit_author` is the
/// GIT_AUTHOR_NAME a person set for this worker in its own scope file; when
/// present it is kept, so a deliberate choice is never overwritten.
pub fn launch_env(home: &Path, worker: &str, mode: Mode, explicit_author: Option<&str>) -> Vec<(String, String)> {
    let host = super::ledger_checkpoint::host_id(home);
    let mut out = vec![(HOST_ENV.to_string(), host.clone())];
    let explicit = explicit_author.map(str::trim).filter(|s| !s.is_empty());
    let verdict = match (mode, explicit) {
        (Mode::Human, _) => {
            HUMAN_N.fetch_add(1, Ordering::Relaxed);
            "human"
        }
        (Mode::Bot, Some(_)) => {
            KEPT_EXPLICIT_N.fetch_add(1, Ordering::Relaxed);
            "kept_explicit"
        }
        (Mode::Bot, None) => {
            let (name, email) = identity(worker, &host, &domain());
            for k in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"] {
                out.push((k.to_string(), name.clone()));
            }
            for k in ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"] {
                out.push((k.to_string(), email.clone()));
            }
            BOT_N.fetch_add(1, Ordering::Relaxed);
            "bot"
        }
    };
    tracing::info!(agent_commit_identity = verdict, worker, measured = true, n_considered = 1,
        "a worker's commits will carry this authorship");
    out
}

/// Who a delivery acts for, from its proven source: the owner and members name
/// themselves; any other source (a worker, a schedule, an app, a relay) acts
/// for the worker's sponsor.
pub fn on_behalf_of(kind: Option<&str>, reference: Option<&str>, sponsor: impl FnOnce() -> String) -> String {
    match (kind, reference) {
        (Some("owner"), _) => "owner".into(),
        (Some("member"), Some(email)) if !email.trim().is_empty() => format!("member:{}", email.trim()),
        _ => sponsor(),
    }
}

pub fn on_behalf_file(home: &Path, worker: &str) -> Option<std::path::PathBuf> {
    super::trace::worker_file(home, worker).map(|p| p.with_extension("on-behalf-of"))
}

/// Write `<home>/trace/<worker>.on-behalf-of`. Never fails the delivery.
pub fn note_on_behalf(home: &Path, worker: &str, who: &str) {
    let Some(path) = on_behalf_file(home, worker) else {
        ON_BEHALF_FAILED_N.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("on-behalf-of.tmp");
        std::fs::write(&tmp, format!("{who}\n"))?;
        std::fs::rename(&tmp, &path)
    };
    match write() {
        Ok(()) => {
            ON_BEHALF_WRITTEN_N.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => {
            ON_BEHALF_FAILED_N.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(verdict = "agent_commit_on_behalf_unwritten", worker, error = %e,
                measured = true, n_considered = 1, "this worker's next commit will carry no On-behalf-of");
        }
    }
}

pub fn report() -> serde_json::Value {
    serde_json::json!({
        "mode": match mode() { Mode::Bot => "bot", Mode::Human => "human" },
        "domain": domain(),
        "measured": true,
        "n_considered": BOT_N.load(Ordering::Relaxed) + HUMAN_N.load(Ordering::Relaxed) + KEPT_EXPLICIT_N.load(Ordering::Relaxed),
        "launches_bot": BOT_N.load(Ordering::Relaxed),
        "launches_human": HUMAN_N.load(Ordering::Relaxed),
        "launches_kept_explicit": KEPT_EXPLICIT_N.load(Ordering::Relaxed),
        "on_behalf_written": ON_BEHALF_WRITTEN_N.load(Ordering::Relaxed),
        "on_behalf_failed": ON_BEHALF_FAILED_N.load(Ordering::Relaxed),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bot_identity_names_the_worker_and_the_host() {
        let (n, e) = identity("amux-helper", "0123456789abcdef", "agents.noreply.amux.io");
        assert_eq!(n, "amux-helper (amux)");
        assert_eq!(e, "amux-helper+01234567@agents.noreply.amux.io");
        // Two hosts' lanes with the same name are two authors.
        let (_, e2) = identity("amux-helper", "fedcba9876543210", "agents.noreply.amux.io");
        assert_ne!(e, e2);
        let (_, odd) = identity("My Worker!", "", "d.example");
        assert_eq!(odd, "my-worker-@d.example");
    }

    #[test]
    fn launch_env_sets_a_bot_author_unless_human_or_explicit() {
        let home = tempfile::tempdir().unwrap();
        let env = launch_env(home.path(), "w", Mode::Bot, None);
        let get = |k: &str| env.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        assert_eq!(get("GIT_AUTHOR_NAME").as_deref(), Some("w (amux)"));
        assert_eq!(get("GIT_COMMITTER_EMAIL"), get("GIT_AUTHOR_EMAIL"));
        let host = get(HOST_ENV).unwrap();
        assert_eq!(host, super::super::ledger_checkpoint::host_id(home.path()), "one host id, the ledger's");
        assert!(get("GIT_AUTHOR_EMAIL").unwrap().contains(&host[..8]));

        let human = launch_env(home.path(), "w", Mode::Human, None);
        assert!(human.iter().all(|(k, _)| !k.starts_with("GIT_")), "{human:?}");
        assert!(human.iter().any(|(k, _)| k == HOST_ENV));
        let kept = launch_env(home.path(), "w", Mode::Bot, Some("Ethan"));
        assert!(kept.iter().all(|(k, _)| !k.starts_with("GIT_")), "{kept:?}");
    }

    #[test]
    fn on_behalf_of_comes_from_the_proven_sender_else_the_sponsor() {
        assert_eq!(on_behalf_of(Some("owner"), Some("owner"), || "member:s@x".into()), "owner");
        assert_eq!(on_behalf_of(Some("member"), Some("doron@x.com"), || "owner".into()), "member:doron@x.com");
        assert_eq!(on_behalf_of(Some("worker"), Some("gtm"), || "member:s@x".into()), "member:s@x");
        assert_eq!(on_behalf_of(Some("email"), Some("email:a@b"), || "owner".into()), "owner");
        assert_eq!(on_behalf_of(None, None, || "owner".into()), "owner");
    }

    #[test]
    fn mode_defaults_to_bot() {
        assert_eq!(mode_from(None), Mode::Bot);
        assert_eq!(mode_from(Some("bot")), Mode::Bot);
        assert_eq!(mode_from(Some("human")), Mode::Human);
        assert_eq!(mode_from(Some("off")), Mode::Human);
    }

    #[test]
    fn note_on_behalf_writes_beside_the_trace_file() {
        let home = tempfile::tempdir().unwrap();
        note_on_behalf(home.path(), "w", "member:a@b");
        let body = std::fs::read_to_string(home.path().join("trace/w.on-behalf-of")).unwrap();
        assert_eq!(body, "member:a@b\n");
        note_on_behalf(home.path(), "../evil", "owner");
        assert!(!home.path().join("evil.on-behalf-of").exists());
    }
}
