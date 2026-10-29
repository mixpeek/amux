//! Wake the lanes whose commits just went live on this server (AMUX-5239).
//!
//! amux-helper pushed dashboard fixes on 2026-09-26 and ended its turn with
//! "goes live when the builder picks it up (~35-40 min)". The builder did pick
//! it up and the server self-adopted the new binary, and nothing told the lane.
//! Its UI check was never run; the card sat `done`, unverified, because the one
//! event the lane was waiting on has no consumer.
//!
//! THE EVENT IS THE BOOT, NOT THE BUILDER. `scripts/rust-auto-build.sh`
//! installs a binary; the running server notices the mtime move and exec's it
//! (`activation::Candidate`, the self-adoption loop in lib.rs). The new image
//! is the first place that knows both "this commit is now serving" and which
//! commit served before it, so it runs this once shortly after start:
//!
//!   1. Read the commit that was live last time (`~/.amux/deploy-wake.json`).
//!   2. `git log prev..served` in `AMUX_REPO_DIR`, the same checkout the
//!      builder and the `served_commit_check` invariant read.
//!   3. Group commits by their `Amux-Session:` trailer, which the commit hook
//!      stamps on every lane's commit.
//!   4. Queue one message per lane through the steering queue (guard
//!      `deploy-wake`), deduped per (lane, served sha) in `session_events`.
//!
//! Isolated and paused lanes are refused by `steer_enqueue` itself, the same
//! as every other automated producer, and that refusal is logged here.
//!
//! Kill switch: `AMUX_DEPLOY_WAKE=0`, scoped (process env, then worker > group
//! > global scope files). Default ON.

use crate::api::AppState;
use crate::db::WriteOutcome;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

pub const DEPLOY_WAKE_KEY: &str = "AMUX_DEPLOY_WAKE";
/// The guard every wake carries on the steering queue.
pub const GUARD: &str = "deploy-wake";
/// How many commits one boot will read. A deploy after a long outage can span
/// hundreds; past this the log says so rather than silently waking on a slice.
const MAX_COMMITS: usize = 400;
/// Let the rest of startup (steering loop, tmux probes) come up first.
const BOOT_DELAY_S: u64 = 45;

/// One commit in the deployed range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployedCommit {
    pub sha: String,
    pub subject: String,
    pub lanes: Vec<String>,
}

/// What one lane is told.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaneWake {
    pub shas: Vec<String>,
    pub cards: Vec<String>,
}

/// Is this setting on? Same spelling the other scoped gates accept.
fn is_on(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "on" | "yes"
    )
}

/// May amux wake this lane on deploy? Process env wins (the operator switch in
/// `~/.amux/server.env`), then worker > group > global. Default ON.
pub(crate) fn deploy_wake_enabled_in(home: &Path, lane: &str, process_value: Option<&str>) -> bool {
    if let Some(v) = process_value.filter(|v| !v.trim().is_empty()) {
        return is_on(v);
    }
    crate::api::session_verbs::scoped_setting_in(home, lane, DEPLOY_WAKE_KEY)
        .as_deref()
        .map(is_on)
        .unwrap_or(true)
}

fn deploy_wake_enabled(lane: &str) -> bool {
    let process_value = std::env::var(DEPLOY_WAKE_KEY).ok();
    deploy_wake_enabled_in(
        &crate::api::session_verbs::home(),
        lane,
        process_value.as_deref(),
    )
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_lane(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `Amux-Session:` trailers in one commit body. All of them, because a graft
/// can carry a peer's trailer beside the grafter's, and both lanes' work is in
/// the deploy.
pub fn trailer_lanes(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in body.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if !k.trim().eq_ignore_ascii_case("amux-session") {
            continue;
        }
        let v = v.trim();
        if valid_lane(v) && !out.iter().any(|x| x == v) {
            out.push(v.to_string());
        }
    }
    out
}

/// `git log --format=%H%x1f%s%x1f%B%x1e` output, parsed.
pub fn parse_log(raw: &str) -> Vec<DeployedCommit> {
    raw.split('\x1e')
        .filter_map(|rec| {
            let rec = rec.trim_start_matches('\n');
            let mut parts = rec.splitn(3, '\x1f');
            let sha = parts.next()?.trim().to_string();
            if !is_full_sha(&sha) {
                return None;
            }
            let subject = parts.next().unwrap_or("").trim().to_string();
            let body = parts.next().unwrap_or("");
            Some(DeployedCommit {
                sha,
                subject,
                lanes: trailer_lanes(body),
            })
        })
        .collect()
}

/// Card ids named in a subject (`AMUX-5239`, `AF-176`). Digit count and a short
/// deny-list keep `SHA-256` and `UTF-8` out.
pub fn card_ids(subject: &str) -> Vec<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\b([A-Z]{2,10})-(\d{2,6})\b").unwrap());
    let mut out: Vec<String> = Vec::new();
    for c in re.captures_iter(subject) {
        if matches!(&c[1], "SHA" | "UTF" | "ISO" | "RFC" | "HTTP" | "TLS" | "CVE") {
            continue;
        }
        let id = c[0].to_string();
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Group the deployed commits by lane. Commits with no trailer are counted by
/// the caller and attributed to nobody: guessing an author is how a lane gets
/// told to verify work it never did.
pub fn plan_wakes(commits: &[DeployedCommit]) -> BTreeMap<String, LaneWake> {
    let mut out: BTreeMap<String, LaneWake> = BTreeMap::new();
    for c in commits {
        for lane in &c.lanes {
            let w = out.entry(lane.clone()).or_default();
            if !w.shas.contains(&c.sha) {
                w.shas.push(c.sha.clone());
            }
            for id in card_ids(&c.subject) {
                if !w.cards.contains(&id) {
                    w.cards.push(id);
                }
            }
        }
    }
    out
}

/// Open "CI red" cards the autofix CI detector filed (AMUX-5371), newest first,
/// whose red stretch is still current.
///
/// A card autofix has noted "workflow is green again" on (idem
/// `autofix-ci-green:<id>`) stays open for its owner to close, by design, but
/// its stretch is over, and a NEW red stretch files a new card. Counting the
/// old one made every deploy-wake on 2026-10-29 say "CI on main is RED:
/// AH-413" while main's rust was green; AH-413's owner lane had every board
/// switch off and could never close it.
///
/// A red main blocks every card behind it from reaching `verified`, and the
/// card that says so is routed through board-drive, which skips any lane with
/// standing orders off (40 of 41 lanes on 2026-09-30). The wake is the one
/// message a lane that just shipped is sure to read, and that lane is the
/// likeliest cause, so the CI state rides on it. A board read, no network.
pub fn open_ci_red(conn: &rusqlite::Connection) -> Vec<(String, String)> {
    let Ok(mut st) = conn.prepare(&format!(
        "SELECT id, title FROM issues WHERE source_ref LIKE 'autofix:ci|%' \
           AND status IN ({live}) AND COALESCE(archived,0)=0 AND deleted IS NULL \
           AND NOT EXISTS (SELECT 1 FROM session_events e \
                            WHERE e.idem = 'autofix-ci-green:' || issues.id) \
         ORDER BY created DESC LIMIT 4",
        live = crate::db::board_store::live_work_status_list()
    )) else {
        return Vec::new();
    };
    st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

/// The text the lane receives. `ci_red` is [`open_ci_red`]'s answer.
pub fn wake_text(served: &str, w: &LaneWake, ci_red: &[(String, String)]) -> String {
    let short = &served[..served.len().min(12)];
    let n = w.shas.len();
    let listed: Vec<&str> = w.shas.iter().take(8).map(|s| &s[..s.len().min(10)]).collect();
    let more = if n > 8 {
        format!(" and {} more", n - 8)
    } else {
        String::new()
    };
    let cards = if w.cards.is_empty() {
        "the work those commits belong to (no card id was named in their subjects)".to_string()
    } else {
        w.cards.join(", ")
    };
    format!(
        "[amux] {short} is live on this server. It includes {n} commit(s) of yours: {}{more}. \
         Run your UI/prod check for {cards} now; nothing else will prompt you. If the change \
         is not visible, hard-reload the dashboard first (the service worker caches). \
         (deploy-wake, AMUX-5239; opt out with AMUX_DEPLOY_WAKE=0 in your scope){ci}",
        listed.join(", "),
        ci = if ci_red.is_empty() {
            String::new()
        } else {
            format!(
                " CI on main is RED: {}. A done card cannot reach verified while it is; \
                 read the failing jobs and fix what is yours.",
                ci_red
                    .iter()
                    .map(|(id, t)| format!("{id} ({t})"))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        }
    )
}

fn marker_path(home: &Path) -> std::path::PathBuf {
    home.join("deploy-wake.json")
}

fn read_marker(home: &Path) -> Option<String> {
    let bytes = std::fs::read(marker_path(home)).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("commit")
        .and_then(|c| c.as_str())
        .filter(|c| is_full_sha(c))
        .map(str::to_string)
}

fn write_marker(home: &Path, commit: &str) {
    let path = marker_path(home);
    let tmp = path.with_extension("json.tmp");
    let body = json!({"commit": commit, "ts": crate::config::now_f64()}).to_string();
    // rename(2): a crash mid-write cannot leave a half marker that reads as
    // "no predecessor" and silently skips a deploy.
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// What the boot decided before touching git. Pure, so the decision is tested
/// in the shapes the live marker produces, including its absence.
#[derive(Debug, PartialEq, Eq)]
pub enum BootDecision {
    /// Served build is dirty or unknown; nothing to attribute.
    Unmeasured,
    /// First run with this feature: record, wake nobody.
    Baseline,
    /// Same commit as last boot: an ordinary restart.
    SameCommit,
    /// A new commit is serving; read `prev..served`.
    Deployed { prev: String },
}

pub fn boot_decision(served: &str, prev: Option<&str>) -> BootDecision {
    if !is_full_sha(served) {
        return BootDecision::Unmeasured;
    }
    match prev {
        None => BootDecision::Baseline,
        Some(p) if p == served => BootDecision::SameCommit,
        Some(p) => BootDecision::Deployed { prev: p.to_string() },
    }
}

async fn git_log(repo: &str, prev: &str, served: &str) -> Result<String, String> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "log",
            &format!("--max-count={}", MAX_COMMITS + 1),
            "--format=%H%x1f%s%x1f%B%x1e",
            &format!("{prev}..{served}"),
        ])
        .output()
        .await
        .map_err(|e| format!("git did not run: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn already_woken(state: &AppState, idem: &str) -> bool {
    let Ok(conn) = state.store.read() else {
        return false;
    };
    conn.query_row(
        "SELECT 1 FROM session_events WHERE idem=?1 LIMIT 1",
        rusqlite::params![idem],
        |_| Ok(()),
    )
    .is_ok()
}

async fn record_woken(state: &AppState, lane: &str, served: &str, idem: &str, queue_id: &str) {
    let (lane, idem, data) = (
        lane.to_string(),
        idem.to_string(),
        json!({"commit": served, "queue_id": queue_id}).to_string(),
    );
    let _ = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO session_events (ts, session, type, data, idem, source) \
                 VALUES (?1, ?2, 'deploy.wake', ?3, ?4, 'deploy-wake')",
                rusqlite::params![crate::config::now_f64(), lane, data, idem],
            )?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await;
}

/// One pass. Returns the lanes that were queued a wake.
pub async fn run_once(state: &AppState, home: &Path, served: &str) -> Vec<String> {
    let prev = read_marker(home);
    let prev = match boot_decision(served, prev.as_deref()) {
        BootDecision::Unmeasured => {
            tracing::info!(served, verdict = "deploy_wake_unmeasured",
                "deploy-wake: served build has no clean commit, nothing to attribute (AMUX-5239)");
            return Vec::new();
        }
        BootDecision::Baseline => {
            write_marker(home, served);
            tracing::info!(served, verdict = "deploy_wake_baseline",
                "deploy-wake: no previous commit on record; recorded this one, woke nobody (AMUX-5239)");
            return Vec::new();
        }
        BootDecision::SameCommit => {
            tracing::info!(served, verdict = "deploy_wake_same_commit",
                "deploy-wake: restart on the same commit, nothing new is live (AMUX-5239)");
            return Vec::new();
        }
        BootDecision::Deployed { prev } => prev,
    };
    let Some(repo) = std::env::var("AMUX_REPO_DIR")
        .or_else(|_| std::env::var("AMUX_REPO"))
        .ok()
        .filter(|p| !p.trim().is_empty())
    else {
        // Advance anyway: with no checkout this server can never read a range,
        // and keeping the old marker would later wake lanes about stale work.
        write_marker(home, served);
        tracing::warn!(served, prev = %prev, verdict = "deploy_wake_unmeasured",
            "deploy-wake: AMUX_REPO_DIR is unset, so the deployed range cannot be read (AMUX-5239)");
        return Vec::new();
    };
    let raw = match git_log(&repo, &prev, served).await {
        Ok(r) => r,
        Err(e) => {
            // NOT advanced: the next boot retries with a range that still
            // covers these commits.
            tracing::warn!(served, prev = %prev, repo = %repo, error = %e,
                verdict = "deploy_wake_unmeasured",
                "deploy-wake: git log over the deployed range failed (AMUX-5239)");
            return Vec::new();
        }
    };
    let mut commits = parse_log(&raw);
    let truncated = commits.len() > MAX_COMMITS;
    commits.truncate(MAX_COMMITS);
    let unattributed = commits.iter().filter(|c| c.lanes.is_empty()).count();
    let plan = plan_wakes(&commits);
    let ci_red = match state.store.read() {
        Ok(conn) => open_ci_red(&conn),
        Err(_) => Vec::new(),
    };
    let mut woken = Vec::new();
    for (lane, w) in &plan {
        let idem = format!("deploy-wake:{lane}:{served}");
        let skip = if !deploy_wake_enabled(lane) {
            Some("deploy_wake_disabled")
        } else if !crate::api::session_verbs::env_path(lane).exists() {
            Some("deploy_wake_unknown_lane")
        } else if already_woken(state, &idem).await {
            Some("deploy_wake_deduped")
        } else if !crate::api::session_verbs::is_running(lane).await {
            Some("deploy_wake_not_running")
        } else {
            None
        };
        if let Some(verdict) = skip {
            tracing::info!(lane = %lane, served, commits = w.shas.len(), verdict,
                "deploy-wake: lane not woken (AMUX-5239)");
            continue;
        }
        let text = wake_text(served, w, &ci_red);
        match crate::api::session_verbs::steer_enqueue(state, lane, &text, GUARD, "").await {
            Ok(queue_id) => {
                record_woken(state, lane, served, &idem, &queue_id).await;
                tracing::info!(lane = %lane, served, commits = w.shas.len(), cards = ?w.cards,
                    ci_red = ci_red.len(), queue_id = %queue_id, verdict = "deploy_wake_sent",
                    "deploy-wake: lane told its commits are live (AMUX-5239)");
                woken.push(lane.clone());
            }
            Err(reason) => {
                tracing::info!(lane = %lane, served, reason, verdict = "deploy_wake_refused",
                    "deploy-wake: the steering queue refused the wake (AMUX-5239)");
            }
        }
    }
    write_marker(home, served);
    // UNCONDITIONAL, including zero, so a boot that woke nobody is still on
    // the record as having looked.
    tracing::info!(
        served,
        prev = %prev,
        commits = commits.len(),
        truncated,
        unattributed,
        lanes = plan.len(),
        woken = woken.len(),
        verdict = "deploy_wake_summary",
        "deploy-wake: deployed range read (AMUX-5239)"
    );
    woken
}

/// One-shot after boot. Not periodic: the event is "this process started on
/// a new commit", which happens exactly once per process.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(BOOT_DELAY_S)).await;
        let home = crate::config::amux_home();
        run_once(&state, &home, env!("AMUX_BUILD_COMMIT_FULL")).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ci_card_noted_green_again_does_not_make_main_read_red() {
        let c = crate::db::migrate::test_memdb();
        c.execute_batch(
            "INSERT INTO issues(id,title,status,session,created,updated,source_ref) VALUES
               ('AH-413','CI red: rust','todo','amux',1,1,'autofix:ci|mixpeek/amux|rust|1|2'),
               ('AF-2000','CI red: checks','todo','x',2,2,'autofix:ci|mixpeek/amux|checks|3|4');
             INSERT INTO session_events(ts,session,type,data,source,idem)
               VALUES(3,'autofix','note',NULL,'autofix','autofix-ci-green:AH-413');",
        )
        .unwrap();
        let ids: Vec<String> = open_ci_red(&c).into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec!["AF-2000".to_string()], "only the stretch that is still red");
    }

    fn sha(c: char) -> String {
        std::iter::repeat_n(c, 40).collect()
    }

    /// The shape `git log --format=%H%x1f%s%x1f%B%x1e` prints, with the
    /// trailers the commit hook stamps (see the real ones on this repo).
    fn fixture() -> String {
        let (a, b, c) = (sha('a'), sha('b'), sha('c'));
        format!(
            "{a}\x1ffix(dashboard): peek stays pinned (AMUX-5100)\x1ffix(dashboard): peek stays pinned (AMUX-5100)\n\nbody\n\nAmux-Session: amux-helper\nAmux-Conversation: x\n\x1e\n\
             {b}\x1ffix(sw): cache bust, SHA-256 of assets\x1ffix(sw): cache bust\n\nAmux-Session: amux-helper\n\x1e\n\
             {c}\x1fdocs: no trailer here\x1fdocs: no trailer here\n\x1e\n"
        )
    }

    #[test]
    fn a_deploy_range_is_grouped_by_trailer_with_cards() {
        let commits = parse_log(&fixture());
        assert_eq!(commits.len(), 3);
        assert_eq!(commits[2].lanes, Vec::<String>::new());
        let plan = plan_wakes(&commits);
        assert_eq!(plan.len(), 1, "an untrailered commit is attributed to nobody");
        let w = &plan["amux-helper"];
        assert_eq!(w.shas, vec![sha('a'), sha('b')]);
        assert_eq!(w.cards, vec!["AMUX-5100".to_string()], "SHA-256 is not a card");
        let text = wake_text(&sha('b'), w, &[]);
        assert!(text.starts_with("[amux] bbbbbbbbbbbb is live on this server"), "{text}");
        assert!(text.contains("AMUX-5100") && text.contains("2 commit(s)"), "{text}");
        assert!(!text.contains("CI on main is RED"), "no open CI card, no claim: {text}");
        // AMUX-5371: an open CI-red card rides on the wake, by id.
        let red = [("AMUX-5358".to_string(), "CI red: checks — 2x consecutive".to_string())];
        let text = wake_text(&sha('b'), w, &red);
        assert!(text.contains("CI on main is RED: AMUX-5358 (CI red: checks"), "{text}");
    }

    #[test]
    fn a_graft_with_two_trailers_wakes_both_lanes() {
        let body = "x\n\nAmux-Session: backend\namux-session: grafter\nAmux-Session: backend\nAmux-Session: bad name!\n";
        assert_eq!(trailer_lanes(body), vec!["backend", "grafter"]);
    }

    /// The boot decision in every shape the marker file can be in. The absent
    /// marker is the pre-fix world (no server has ever written one), and it
    /// must wake NOBODY: it cannot know the range.
    #[test]
    fn boot_decision_covers_first_boot_restart_and_deploy() {
        let (a, b) = (sha('a'), sha('b'));
        assert_eq!(boot_decision(&a, None), BootDecision::Baseline);
        assert_eq!(boot_decision(&a, Some(&a)), BootDecision::SameCommit);
        assert_eq!(
            boot_decision(&b, Some(&a)),
            BootDecision::Deployed { prev: a.clone() }
        );
        assert_eq!(boot_decision("unknown", Some(&a)), BootDecision::Unmeasured);
        assert_eq!(boot_decision("c3ffbffe-dirty", None), BootDecision::Unmeasured);
    }

    #[test]
    fn marker_round_trips_and_rejects_junk() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(read_marker(d.path()), None);
        write_marker(d.path(), &sha('e'));
        assert_eq!(read_marker(d.path()), Some(sha('e')));
        std::fs::write(marker_path(d.path()), r#"{"commit":"nope"}"#).unwrap();
        assert_eq!(read_marker(d.path()), None);
    }

    #[test]
    fn the_kill_switch_resolves_process_then_scope_and_defaults_on() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        assert!(deploy_wake_enabled_in(home, "lane", None), "default ON");
        std::fs::write(home.join("amux.env"), "AMUX_DEPLOY_WAKE=0\n").unwrap();
        assert!(!deploy_wake_enabled_in(home, "lane", None), "global off");
        std::fs::write(home.join("sessions/lane.env"), "AMUX_DEPLOY_WAKE=1\n").unwrap();
        assert!(deploy_wake_enabled_in(home, "lane", None), "worker beats global");
        assert!(!deploy_wake_enabled_in(home, "lane", Some("0")), "process env wins");
    }

    /// End to end over a real git repo and a real store: first boot records,
    /// the deploy boot reads the range, and a lane with no session file is
    /// skipped (never queued) while the marker still advances. Dedupe is
    /// per (lane, sha), checked through the same idem row the wake writes.
    #[tokio::test]
    async fn run_once_baselines_then_reads_the_range() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(out.status.success(), "{:?}", out);
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "--no-verify", "-m", "base"]);
        let base = git(&["rev-parse", "HEAD"]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "--no-verify",
            "-m", "fix: x (AMUX-12)\n\nAmux-Session: nosuch-lane-deploy-wake-test"]);
        let head = git(&["rev-parse", "HEAD"]);

        let store = std::sync::Arc::new(crate::db::Store::open(&d.path().join("t.db")).unwrap());
        let st = AppState {
            store,
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let home = d.path().join("home");
        std::fs::create_dir_all(&home).unwrap();

        assert!(run_once(&st, &home, &base).await.is_empty());
        assert_eq!(read_marker(&home), Some(base.clone()), "first boot records");

        let raw = git_log(repo.to_str().unwrap(), &base, &head).await.unwrap();
        let plan = plan_wakes(&parse_log(&raw));
        assert_eq!(plan.keys().collect::<Vec<_>>(), vec!["nosuch-lane-deploy-wake-test"]);
        assert_eq!(plan["nosuch-lane-deploy-wake-test"].cards, vec!["AMUX-12".to_string()]);

        // Idempotence key: once recorded, the same (lane, sha) is deduped.
        let idem = format!("deploy-wake:nosuch-lane-deploy-wake-test:{head}");
        assert!(!already_woken(&st, &idem).await);
        record_woken(&st, "nosuch-lane-deploy-wake-test", &head, &idem, "q").await;
        assert!(already_woken(&st, &idem).await);
    }
}
