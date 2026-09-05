//! Legacy-port accounting — who is still stranded on the retired 8822 address,
//! now that its bind is gone.
//!
//! # History (why this module exists)
//!
//! When the Python server was retired, the Rust server answered a second,
//! historical port (`AMUX_RS_LEGACY_PORT`, 8822) because ~60 live fleet sessions
//! carried `AMUX_URL=https://localhost:8822` in their PROCESS env, and a live
//! process's env cannot be rotated. That bind was pure carry-over, always a
//! countdown rather than an address, and the only responsible way to decide "is
//! anything still calling it?" was to measure rather than guess (Ethos rule 4:
//! if a wrong answer would not be detectable from the data we keep, the missing
//! instrument IS the bug). So this module counted hits on the legacy listener
//! BY LAYER (below), turning the retirement decision into a number.
//!
//! # The bind is GONE (Ethan, 2026-08-11: "no more 8822 just rust")
//!
//! Ethan dropped it ahead of the automatic 7-day-quiet exit; `lib.rs` records
//! why and forbids re-adding it. Two consequences the rest of this file is
//! shaped by:
//!
//!   1. The hit counter still exists but reads 0 BY CONSTRUCTION — nothing
//!      listens on 8822, so a stranded lane's `curl $AMUX_URL/...` fails at
//!      connect and records no hit. Hit-based "still in use" therefore collapses
//!      to CLEAR while lanes are in fact broken, which is why the LIVE signal is
//!      now the process-env scan (`scan_stranded_sessions`, AMUX-2988), reported
//!      hourly (WARN naming the sessions) and by `GET /api/debug/legacy-port` as
//!      `stranded_count` / `stranded_sessions`.
//!   2. The remedy for a stranded lane is `$(amux url)` — which reads
//!      `endpoint.json` and self-heals past the dead env — or restarting the
//!      lane. NOT restoring the bind (AMUX-3046).
//!
//! # How the count was taken (retained for the retired-port guard + tests)
//!
//! By LAYER on the legacy listener's router clone, not by sniffing the `Host`
//! header: the two listeners shared one `app`, so a request was counted iff it
//! physically arrived on that socket. A `Host` header is client-supplied and
//! would have counted a request that merely *said* 8822 while arriving on 8824.
//! Kept because `RETIRED_PORTS` and the endpoint contract still drive the
//! stranded-lane detection and its tests.

use axum::extract::{ConnectInfo, Request};
use axum::middleware::Next;
use axum::response::Response;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Distinct (ip, user-agent) pairs retained. A cap is needed so a scanner
/// cannot grow this without bound, and the number of pairs it DROPPED is
/// reported alongside — an evidence cap that hides the fact it capped is how
/// you conclude "only these three callers" from a truncated list.
const MAX_CLIENTS: usize = 200;

/// Distinct paths remembered per caller. `last_path` alone cannot tell two
/// callers wearing the same user-agent apart, and on this port they were:
/// `curl/8.7.1` is BOTH the Claude Code report hooks and every agent's own
/// `curl $AMUX_URL/api/...`, while `Python-urllib` is BOTH the git pre-commit
/// guard and the PreToolUse Bash guard. Draining the port means fixing callers
/// one at a time and watching the number fall, and a tally that collapses four
/// callers into two rows cannot show which fix worked (ethos rule 4 — the
/// instrument must be able to express the discriminator). Capped, with the
/// drop count reported, for the same reason [`MAX_CLIENTS`] is.
const MAX_PATHS_PER_CLIENT: usize = 24;

/// One distinct caller of the legacy port.
#[derive(Clone, Debug, Default)]
pub struct ClientTally {
    pub count: u64,
    pub first_seen: u64,
    pub last_seen: u64,
    pub last_path: String,
    /// path -> hits. See [`MAX_PATHS_PER_CLIENT`].
    pub paths: BTreeMap<String, u64>,
    pub paths_dropped: u64,
}

/// Marker inserted into the request extensions by [`count`], so a handler can
/// tell that THIS request arrived on the retired socket.
///
/// The dashboard shell is the one response that has to care: a client that
/// loaded the SPA from the legacy origin keeps every relative `/api/...` fetch
/// on that origin forever, so the shell is the only place a browser-side
/// migration can be offered. Everything else must stay byte-identical between
/// the two listeners — sessions are depending on it.
#[derive(Clone, Copy, Debug)]
pub struct OnLegacyListener(pub u16);

#[derive(Debug, Default)]
struct Inner {
    /// The port being answered. 0 = the bind is not enabled at all.
    port: u16,
    /// Process start (== window start; the count is per-uptime, and the
    /// debug surface says so rather than implying an all-time total).
    started_at: u64,
    hits_total: u64,
    last_hit: u64,
    /// Reset each hourly report, so the log line reads "in the last hour".
    hour_hits: u64,
    hour_started: u64,
    clients: BTreeMap<String, ClientTally>,
    clients_dropped: u64,
}

/// Where the observation window survives a restart (AMUX-2769).
///
/// The window used to be per-UPTIME, and the reason was sound: a counter that
/// resets and then reports itself as a lifetime total "would make the
/// retirement window look satisfied when it had just been reset". Safe — but
/// it made the exit UNREACHABLE, which is the failure this file is supposed to
/// prevent, one level up. `ready_to_retire` required `hours_elapsed >= 168`
/// (7d) on a process that re-execs every time anyone commits to `crates/`:
/// measured 2026-08-11, the builder had installed 316 times and the window
/// stood at 0.71 hours. The countdown reset before it could ever finish, so
/// the bind that CLAUDE.md calls "a countdown, not an address" had no way to
/// reach zero.
///
/// Persisting `last_hit` fixes it without giving up the safety, because the
/// verdict now keys on QUIET TIME (`now - last_hit`) rather than on process
/// uptime. That is also what the prose always said — "zero hits for 7 days" —
/// and unlike an uptime window it cannot be satisfied by a restart: a restart
/// does not make traffic older.
fn persist_path() -> std::path::PathBuf {
    // Overridable so the lifecycle test below drives the SHIPPED load/save path
    // against a temp file instead of this machine's real window. Without it the
    // test reads whatever traffic the running fleet has recorded and
    // "arming must not fabricate a hit" fails with hits_total=1 — a test whose
    // verdict depends on the host, which is the same trap as an env-mutating
    // test racing its siblings (see this module's own test doc).
    if let Ok(p) = std::env::var("AMUX_LEGACY_PORT_STATE") {
        if !p.trim().is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    crate::config::amux_home().join("legacy-port-window.json")
}

/// Load the persisted window. Absent/corrupt file = a fresh window, which is
/// the conservative direction: it delays retirement, never advances it.
fn load_persisted() -> (u64, u64, u64) {
    let Ok(txt) = std::fs::read_to_string(persist_path()) else { return (0, 0, 0) };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else { return (0, 0, 0) };
    (
        v.get("first_armed").and_then(|x| x.as_u64()).unwrap_or(0),
        v.get("hits_total").and_then(|x| x.as_u64()).unwrap_or(0),
        v.get("last_hit").and_then(|x| x.as_u64()).unwrap_or(0),
    )
}

fn save_persisted(first_armed: u64, hits_total: u64, last_hit: u64) {
    let body = serde_json::json!({
        "first_armed": first_armed,
        "hits_total": hits_total,
        "last_hit": last_hit,
        "note": "legacy 8822 observation window; survives restarts so the 7-day \
                 quiet period can actually accumulate (AMUX-2769)",
    });
    let p = persist_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&p, body.to_string());
}

static STATE: OnceLock<Mutex<Inner>> = OnceLock::new();

fn state() -> &'static Mutex<Inner> {
    STATE.get_or_init(|| Mutex::new(Inner::default()))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Record that the bind came up, so the debug surface can distinguish
/// "enabled, zero traffic" (the state we are waiting for) from "not enabled"
/// (nothing to decide). Without this they both render as zero hits.
pub fn arm(port: u16) {
    if let Ok(mut s) = state().lock() {
        let t = now();
        let (first_armed, hits, last) = load_persisted();
        s.port = port;
        // The window CONTINUES across restarts; only a never-armed machine
        // starts one. hits_total and last_hit are restored with it, so the
        // quiet period is measured from real traffic rather than from boot.
        s.started_at = if first_armed > 0 { first_armed } else { t };
        s.hits_total = hits;
        s.last_hit = last;
        s.hour_started = t;
        // The PER-UPTIME fields start empty, because that is what arming means:
        // this process has begun answering. In production arm() runs once on a
        // fresh process so these are already default — stating it makes the
        // split explicit (persisted: window + traffic recency; per-uptime:
        // who is calling right now) and lets the test model a restart honestly
        // instead of inheriting the previous phase's clients.
        s.clients.clear();
        s.clients_dropped = 0;
        s.hour_hits = 0;
        save_persisted(s.started_at, s.hits_total, s.last_hit);
    }
}

/// Middleware for the LEGACY listener only. Counts the request, tags who sent
/// it, and passes it through unchanged — the legacy port must stay
/// byte-identical to the primary one, since sessions are depending on it.
pub async fn count(mut req: Request, next: Next) -> Response {
    let ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(a)| a.ip().to_string())
        .unwrap_or_else(|| "unknown".into());
    let ua = req
        .headers()
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .chars()
        .take(120)
        .collect::<String>();
    // The discriminator this counter was missing (AMUX-2769). ip+ua collapses
    // the WHOLE fleet into one row — every lane is 127.0.0.1 wearing curl/8 —
    // and the exit decision needs to separate two populations that look
    // identical there:
    //   * a pre-cutover SESSION whose process env still says 8822. Cannot be
    //     rotated in a live process, harmless, drains when that lane restarts.
    //   * an UNATTRIBUTED caller — new code, a doc, a script someone just
    //     wrote against the retired address. That is a real defect to fix.
    // Waiting cannot retire the port while the first group is running, and
    // until now nothing could tell the reader which group the traffic was.
    let session = crate::api::groups::hdr_worker(req.headers())
        .trim()
        .chars()
        .take(64)
        .collect::<String>();
    let path = req.uri().path().to_string();

    let port = record(&ip, &ua, &session, &path);
    // Tell the SHELL handler it is answering on the retired socket. Only the
    // dashboard document acts on this; the API surface stays identical. Set
    // outside the lock so a poisoned counter cannot silently disable the
    // migration — the two failures are unrelated and must not be coupled.
    req.extensions_mut()
        .insert(OnLegacyListener(if port == 0 { crate::config::DEFAULT_PORT } else { port }));
    next.run(req).await
}

/// The whole tally, extracted from [`count`] so the test drives THIS and not a
/// hand-written imitation of it. The previous test poked `state()` directly and
/// so could not have caught a bug in the accounting itself — it asserted about
/// numbers it had written by hand (ethos rule 7: test the shipped code path).
///
/// Returns the armed legacy port, or 0 when the bind is not enabled.
fn record(ip: &str, ua: &str, session: &str, path: &str) -> u16 {
    let Ok(mut s) = state().lock() else { return 0 };
    let t = now();
    s.hits_total += 1;
    s.hour_hits += 1;
    let prev_last = s.last_hit;
    s.last_hit = t;
    let port = s.port;
    // THROTTLED to once a minute. `last_hit` is what the retirement verdict
    // reads, and it only needs to be accurate to well within the 7-day quiet
    // period — writing a file on every request would put a syscall in the path
    // of a listener whose entire purpose is to be byte-identical to the
    // primary one. A crash can lose at most 60s of recency, which moves the
    // verdict in the CONSERVATIVE direction (an older last_hit would only ever
    // make retirement look closer, and we round the other way by persisting
    // the first hit in a window immediately).
    let should_persist = prev_last == 0 || t.saturating_sub(prev_last) >= 60;
    let persist_args = (s.started_at, s.hits_total, s.last_hit);
    // TAB-separated: a user-agent contains spaces, so the old
    // `"{ip} {ua}"` + `split_once(' ')` only worked because ip has none.
    // A third field needs an unambiguous separator.
    let key = format!("{ip}\t{ua}\t{session}");
    // Decide admission BEFORE taking the entry: `get_mut(..) else entry(..)`
    // borrows `s.clients` across both arms and does not compile.
    let admit = s.clients.contains_key(&key) || s.clients.len() < MAX_CLIENTS;
    if !admit {
        s.clients_dropped += 1;
        drop(s);
        if should_persist {
            save_persisted(persist_args.0, persist_args.1, persist_args.2);
        }
        return port;
    }
    let e = s.clients.entry(key).or_insert(ClientTally { first_seen: t, ..Default::default() });
    e.count += 1;
    e.last_seen = t;
    e.last_path = path.to_string();
    if let Some(c) = e.paths.get_mut(path) {
        *c += 1;
    } else if e.paths.len() < MAX_PATHS_PER_CLIENT {
        e.paths.insert(path.to_string(), 1);
    } else {
        e.paths_dropped += 1;
    }
    // PERSIST LAST, never with an early return. The first version returned
    // straight after saving and so skipped the client tally below — the hour
    // WARN line lost its caller and the module's own lifecycle test caught it
    // ("the WARN line must be able to name the caller"). The save is a side
    // effect of recording a hit; it must not decide whether the hit is
    // recorded.
    drop(s);
    if should_persist {
        save_persisted(persist_args.0, persist_args.1, persist_args.2);
    }
    port
}

/// The canonical port this server answers on — the one every client should be
/// using. Read from the same place [`crate::config`] reads it, so there is one
/// definition of "where amux is" and not a second literal to keep in step.
pub fn canonical_port() -> u16 {
    std::env::var("AMUX_RS_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(crate::config::DEFAULT_PORT)
}

/// Ports this server used to answer and no longer does. Consumed by
/// `endpoint.json` so a hook inheriting a stale `AMUX_URL` can still self-heal
/// after the bind is gone (AMUX-2946).
const RETIRED_PORTS: &[u16] = &[8822];

/// Running sessions whose `AMUX_URL` still names a retired port — GROUND TRUTH
/// for "who is stranded", measured by scanning process envs rather than by
/// counting hits (AMUX-2988, ethos rule 4).
///
/// The hit counter above answers "who is CALLING the retired port". Once Ethan
/// dropped the 8822 bind (2026-08-11, "no more 8822 just rust"), nothing listens
/// there, so a stranded lane's `curl $AMUX_URL/...` fails at connect and records
/// NO hit — the hit-based `sessions_still_on_legacy` collapses to `[]` and the
/// verdict reads CLEAR while 52 lanes are in fact broken. A process-env scan is
/// independent of hits and therefore still answers the question after the bind
/// is gone, which is exactly when it matters. This mirrors `/api/debug/tmux`:
/// discovery from INSIDE the server process, reported as a number.
///
/// Recycling those lanes is the owner's call (ethos rule 8 — a restart can
/// interrupt in-flight customer work); this only makes the count visible.
fn scan_stranded_sessions() -> Vec<serde_json::Value> {
    scan_stranded_sessions_checked().unwrap_or_default()
}

/// The same scan, but `None` when `ps` never ran (AF-320).
///
/// The difference is load-bearing and used to be invisible. `ps axeww` failing
/// produced an empty vec, `stranded_count: 0`, and therefore a `ready_to_retire`
/// verdict computed as though nothing was stranded — a confident yes derived
/// from a measurement that did not happen. That is the ethos-rule-4 shape this
/// module's own doc comment warns about, one function below where it says it.
fn scan_stranded_sessions_checked() -> Option<Vec<serde_json::Value>> {
    // `ps axeww` appends each process's environment (space-separated KEY=VALUE)
    // after its command, on both BSD (macOS) and GNU (the Linux container) ps.
    let output = std::process::Command::new("ps").arg("axeww").output();
    match output {
        Ok(o) if o.status.success() => {
            Some(parse_stranded(&String::from_utf8_lossy(&o.stdout), RETIRED_PORTS))
        }
        _ => None,
    }
}

/// Parse `ps axeww` output for sessions whose `AMUX_URL` names a retired port,
/// deduped to one row per distinct `AMUX_SESSION`. Pure over the text so the
/// discriminating logic is tested without depending on what is running
/// (ethos rule 7 — the `Command` call is the thin untested shell, this is not).
fn parse_stranded(ps_output: &str, retired: &[u16]) -> Vec<serde_json::Value> {
    let needles: Vec<String> = retired.iter().map(|p| format!(":{p}")).collect();
    // session -> (pid, url); dedup by session (a lane's tmux wrapper AND its
    // claude child both carry the env, so pid-dedup would double-count).
    let mut seen: std::collections::BTreeMap<String, (Option<i64>, String)> =
        std::collections::BTreeMap::new();
    for line in ps_output.lines() {
        let url = line.split_whitespace().find(|t| t.starts_with("AMUX_URL="));
        let Some(url_tok) = url else { continue };
        if !needles.iter().any(|n| url_tok.contains(n.as_str())) {
            continue;
        }
        let session = line
            .split_whitespace()
            .find(|t| t.starts_with("AMUX_SESSION="))
            .map(|t| t.trim_start_matches("AMUX_SESSION=").to_string())
            .unwrap_or_default();
        if session.is_empty() {
            continue; // no session name — not an attributable lane
        }
        let pid = line.split_whitespace().next().and_then(|p| p.parse::<i64>().ok());
        let url = url_tok.trim_start_matches("AMUX_URL=").to_string();
        seen.entry(session).or_insert((pid, url));
    }
    seen.into_iter()
        .map(|(session, (pid, url))| serde_json::json!({"session": session, "pid": pid, "amux_url": url}))
        .collect()
}

/// Publish where this server actually is, into `<amux_home>/endpoint.json`.
///
/// # Why a file, and why the server writes it
///
/// The legacy bind exists because ~55 live `claude` processes carry
/// `AMUX_URL=https://localhost:8822` in their PROCESS env, which cannot be
/// rotated without killing them. Every hook those processes spawn — the report
/// hooks, the PreToolUse git guard, the pre-commit staged-guard — inherits that
/// stale value, so "fix the hook's DEFAULT" (`${AMUX_URL:-…8824}`) changes
/// nothing at all: the default only fires when the variable is UNSET, and it
/// never is. That fix was shipped and was inert for all 55 (ethos rule 1 — the
/// capability existed and reached nobody).
///
/// A one-shot hook has exactly one way to notice its inherited URL is stale:
/// ask something on the machine that knows better. This file is that. The
/// SERVER writes it because the server is the only party that knows both ports
/// without guessing, and because it must stay true in the cloud image too,
/// where the numbers are reversed (`AMUX_RS_PORT=8822` there) — a hook with a
/// hardcoded `8822 -> 8824` rewrite would break that deployment, while a hook
/// that reads this file is correct in both. Single codebase, no build flags.
///
/// The consumer rule is deliberately narrow, and is documented here because
/// this file is its contract: **if your URL is `localhost:<legacy_port>`, use
/// `canonical_url` instead.** Not "always prefer canonical" — a session
/// pointing at a dev instance on another port, or at a remote amux, is making a
/// deliberate choice and must be left alone.
/// Is `pid` a currently-live process? `kill(pid, 0)` sends no signal but
/// performs the existence+permission check — Ok/EPERM means alive, ESRCH means
/// gone. No `/proc` (macOS has none) and no extra crate. A recycled pid is a
/// negligible risk here: the worst case is declining to overwrite for one
/// startup, which fails safe (the file just keeps its current contents).
fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 is the canonical liveness probe; it touches
    // no memory and cannot affect the target.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn publish_endpoint(amux_home: &std::path::Path, canonical: u16, legacy: Option<u16>) {
    // `legacy_port` NAMES THE SINGLE STALE PORT pre-cutover sessions carry in
    // their inherited `AMUX_URL`, and it MUST stay non-null while any such
    // session is alive. This `.or_else(RETIRED_PORTS…)` reverses the `None` that
    // shipped when the 8822 bind was dropped.
    //
    // That earlier change fixed ONE consumer — the git staged-guard resolver,
    // migrated to read the `retired_ports` array — and silently stranded the
    // OTHER: the Stop / PostToolUse / UserPromptSubmit state-report hooks baked
    // into ~48 live `claude` processes. Those hooks rewrite a stale URL by
    // matching THIS field (`case "$U" in *localhost:$L)`), and cannot be changed
    // in place — Claude Code loads hook config at session start, so the only
    // thing that reaches an already-running lane is the file it reads at RUNTIME,
    // which is this one. With `legacy_port:null` the pattern degenerated to
    // `*localhost:` and matched nothing, so every one of those lanes POSTed its
    // state report to the dead 8822 and failed silently. Measured 2026-08-13:
    // 0 of 48 running lanes with a fresh self-report, status fell back fleet-wide
    // to terminal scraping — the exact D1 path this file exists to demote, and
    // the "worker status is inaccurate/delayed" symptom the owner reported.
    //
    // The file's own `consumer_rule` (below) still documents `legacy_port` as the
    // rewrite trigger, so nulling it made the file contradict its own contract.
    // `retired_ports` is the newer, richer signal for resolvers that can read an
    // array; `legacy_port` is the original single-value one the baked-in hooks
    // read — the two must AGREE on the port sessions actually carry. This is the
    // same countdown shim the bind itself was, one layer down: it returns to null
    // on its own the day `RETIRED_PORTS` is emptied, which is the day no live
    // session can still be carrying that port. `session.self_reports_landing`
    // (invariants/checks.rs) now fails loudly if reporting ever goes dark again,
    // so the next occurrence self-announces instead of waiting to be noticed.
    //
    // Cloud safety (single codebase, no branch): there canonical == 8822 == the
    // sole retired port, so `legacy_field` is `Some(8822)` and every rewrite is
    // 8822 -> 8822, a no-op — identical to the `retired_ports` array already
    // published there today.
    let legacy_field = legacy.or_else(|| RETIRED_PORTS.first().copied());
    let body = serde_json::json!({
        "canonical_url": format!("https://localhost:{canonical}"),
        "canonical_port": canonical,
        "legacy_port": legacy_field,
        // A retired address needs to stay KNOWN after it stops being served,
        // because the processes that still name it are precisely the ones that
        // cannot be told anything else.
        "retired_ports": RETIRED_PORTS,
        "pid": std::process::id(),
        "written_at": now(),
        "consumer_rule": "if your AMUX_URL is https://localhost:<legacy_port>, use canonical_url",
    });
    let path = amux_home.join("endpoint.json");
    // DO NOT CLOBBER A LIVE FLEET SERVER'S FILE (AMUX-2971). This home's
    // endpoint.json is the ONE control file every session's git hooks resolve
    // the server through, and it is shared: a throwaway dev server started on
    // an alt port but the DEFAULT home (to read the live DB, say) published
    // its own port here, and when it was killed the file named a dead port —
    // turning off staged-guard enforcement for every session at once
    // (measured 2026-08-12: "staged-guard NOT ENFORCED — connection refused").
    //
    // The distinguisher is not the port (canonical_port() follows AMUX_RS_PORT,
    // which the dev instance also set) — it is that a DIFFERENT process is
    // still alive and already owns this file. If so, that process is the fleet
    // server for this home; a second instance must not overwrite its pointer.
    // Our own stale entry (same pid) and a dead predecessor (kickstart restart)
    // are both fine to overwrite — only a live foreign owner is protected.
    if let Some(owner) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("pid").and_then(serde_json::Value::as_u64))
    {
        let me = std::process::id() as u64;
        if owner != me && pid_alive(owner as u32) {
            tracing::warn!(
                ?path, owner_pid = owner, my_pid = me, canonical,
                "endpoint.json is owned by a live server (pid {owner}); NOT overwriting it \
                 from this instance — a non-fleet server must not repoint the shared hook lookup \
                 (AMUX-2971). Give a dev server its own AMUX_HOME."
            );
            return;
        }
    }
    // Write-then-rename: a hook reading this file mid-write must never see a
    // truncated JSON document and fall back to the stale env, which is the
    // exact failure it is here to prevent.
    let tmp = amux_home.join("endpoint.json.tmp");
    let ok = std::fs::write(&tmp, format!("{body}\n"))
        .and_then(|_| std::fs::rename(&tmp, &path))
        .is_ok();
    if ok {
        tracing::info!(?path, canonical, ?legacy_field, "published endpoint.json (stale-URL self-heal)");
    } else {
        // Loud: silently failing here turns every hook's self-heal off with no
        // trace, and the symptom (traffic on the retired port) looks identical
        // to nobody having fixed the hooks at all.
        tracing::warn!(?path, "could not publish endpoint.json — hooks cannot self-heal a stale AMUX_URL");
    }
}

/// Snapshot for the debug surface / the reporter, as JSON.
pub fn snapshot() -> serde_json::Value {
    // The host probe (shells out to `ps`) is done HERE, outside the lock, and
    // passed in — separating it from the JSON builder keeps `snapshot_with`
    // deterministic in tests, which must not depend on how many lanes this
    // machine happens to have stranded (ethos rule 7: a test whose verdict
    // depends on the host is the trap this module already documents).
    snapshot_with(scan_stranded_sessions())
}

fn snapshot_with(stranded: Vec<serde_json::Value>) -> serde_json::Value {
    let stranded_count = stranded.len();
    let Ok(s) = state().lock() else {
        return serde_json::json!({"error": "legacy-port state poisoned"});
    };
    let t = now();
    let mut clients: Vec<_> = s
        .clients
        .iter()
        .map(|(k, v)| {
            let mut parts = k.splitn(3, '\t');
            let ip = parts.next().unwrap_or("-");
            let ua = parts.next().unwrap_or("-");
            let session = parts.next().unwrap_or("");
            // Loudest path first, for the same reason clients are sorted that
            // way: this list is read to decide WHICH caller wearing this
            // user-agent to go and fix next.
            let mut paths: Vec<_> =
                v.paths.iter().map(|(p, c)| serde_json::json!({"path": p, "count": c})).collect();
            paths.sort_by_key(|p| std::cmp::Reverse(p["count"].as_u64().unwrap_or(0)));
            serde_json::json!({
                "ip": ip,
                "user_agent": ua,
                // "" = unattributed. That is the row that matters: an
                // attributed lane drains on its next restart, an unattributed
                // caller is code still pointing at the retired address.
                "session": session,
                "attributed": !session.is_empty(),
                "count": v.count,
                "first_seen": v.first_seen,
                "last_seen": v.last_seen,
                "last_path": v.last_path,
                "paths": paths,
                "paths_dropped": v.paths_dropped,
            })
        })
        .collect();
    // Loudest caller first: the list is read to decide who to go and fix.
    clients.sort_by_key(|c| std::cmp::Reverse(c["count"].as_u64().unwrap_or(0)));

    // THE EXIT VERDICT, computed rather than left for a reader to eyeball.
    // "Zero hits for 7 days" is unsatisfiable by waiting while pre-cutover
    // lanes are alive — their process env cannot be rotated — so the useful
    // question is not "is it zero" but "is any of this traffic a DEFECT".
    // Attributed traffic drains on its lane's next restart; unattributed
    // traffic is code that still names the retired address.
    let mut sessions: Vec<&str> = clients
        .iter()
        .filter_map(|c| c["session"].as_str())
        .filter(|s| !s.is_empty())
        .collect();
    sessions.sort_unstable();
    sessions.dedup();
    let unattributed: u64 = clients
        .iter()
        .filter(|c| !c["attributed"].as_bool().unwrap_or(false))
        .map(|c| c["count"].as_u64().unwrap_or(0))
        .sum();
    let attributed: u64 = clients
        .iter()
        .filter(|c| c["attributed"].as_bool().unwrap_or(false))
        .map(|c| c["count"].as_u64().unwrap_or(0))
        .sum();
    let elapsed = t.saturating_sub(s.started_at);
    serde_json::json!({
        "enabled": s.port != 0,
        "port": s.port,
        "canonical_port": std::env::var("AMUX_RS_PORT").unwrap_or_else(|_| crate::config::DEFAULT_PORT.to_string()),
        "attributed_hits": attributed,
        "unattributed_hits": unattributed,
        "sessions_still_on_legacy": sessions,
        // GROUND TRUTH (AMUX-2988): running sessions whose AMUX_URL still names a
        // retired port, by process-env scan — independent of hits, so this stays
        // correct after the bind is dropped, when `sessions_still_on_legacy`
        // above (hit-derived) collapses to [] because a dead port records none.
        // On a live machine this is the count that tells the owner how many lanes
        // are running degraded and would benefit from a recycle.
        "stranded_sessions": stranded,
        "stranded_count": stranded_count,
        // The wording is deliberately weaker than "this is a defect". Not every
        // unattributed hit is one: a request that carries no X-Amux-Session —
        // /health polls, the SPA shell, a browser — is unattributed even when
        // it comes from a pre-cutover lane that merely needs restarting.
        // Measured on the first live run of this very change: my own
        // `curl https://localhost:8822/health` landed in the unattributed
        // bucket. Claiming certainty the key cannot support is how a verdict
        // stops being read.
        // Strandedness is checked FIRST because it is the only signal that
        // survives the bind being gone. With nothing listening on the retired
        // port, `unattributed`/`sessions` are both 0 by construction, so without
        // this branch a machine with 52 broken lanes reads "CLEAR" (AMUX-2988).
        "verdict": if stranded_count > 0 {
            format!("STRANDED: {stranded_count} running session(s) still carry a retired port in AMUX_URL and cannot reach the server via $AMUX_URL — every documented `curl $AMUX_URL/...` recipe fails for them. Clears only when those lanes RESTART; recycling them is the owner's call (could interrupt in-flight work). See `stranded_sessions`")
        } else if unattributed > 0 {
            "INVESTIGATE: traffic with no X-Amux-Session. Either code still naming 8822, or header-less requests (/health, the SPA shell) from a pre-cutover lane. Read `paths` — an /api/ path with no session is the defect shape; /health is not".to_string()
        } else if !sessions.is_empty() {
            "DRAINING: every hit is an attributed pre-cutover lane whose process env cannot be rotated; it clears when those lanes restart, not by waiting".to_string()
        } else {
            "CLEAR: no traffic on the retired port this uptime AND no running session env still names it".to_string()
        },
        // PERSISTED across restarts now (AMUX-2769). The old per-uptime
        // counter was chosen so a reset counter could not be misread as a
        // lifetime total — sound, but it made `ready_to_retire` unreachable:
        // it wanted 168h of window on a process the builder re-execs several
        // times an hour (316 installs, window standing at 0.71h when measured).
        // The verdict now keys on QUIET TIME instead, which a restart cannot
        // fake because a restart does not make traffic older.
        "hits_total": s.hits_total,
        "window": {
            "started_at": s.started_at,
            "hours_elapsed": elapsed as f64 / 3600.0,
            "resets_on_restart": false,
            "persisted_at": persist_path().to_string_lossy(),
        },
        "quiet_for_hours": if s.last_hit == 0 {
            serde_json::Value::Null
        } else {
            serde_json::json!(t.saturating_sub(s.last_hit) as f64 / 3600.0)
        },
        "last_hit": if s.last_hit == 0 { serde_json::Value::Null } else { s.last_hit.into() },
        "clients": clients,
        "clients_dropped": s.clients_dropped,
        "retire_when": "quiet_for_hours >= 168 (7d with no hit at all) AND clients empty this \
                        uptime -> drop AMUX_RS_LEGACY_PORT from ~/.amux/server.env and delete the \
                        bind block in lib.rs. See docs/rust-migration/server-boundary.md. \
                        Measured from a PERSISTED last_hit, so restarts no longer reset the \
                        countdown (they used to, several times an hour, which made this \
                        unreachable).",
        // A retired port is not fully retired while any live lane still names it
        // in its env, even at zero hits — those lanes' raw-curl recipes are
        // silently broken. `stranded_count == 0` is the condition the hit-based
        // window could never express (AMUX-2988).
        "ready_to_retire": stranded_count == 0
            && s.clients.is_empty()
            && match s.last_hit {
                // Armed, never hit, and observed for a week: the genuine
                // all-clear. `started_at` is persisted too, so this is real
                // observation time rather than uptime.
                0 => elapsed >= 7 * 24 * 3600,
                last => t.saturating_sub(last) >= 7 * 24 * 3600,
            },
    })
}

/// `GET /api/debug/legacy-port`.
pub async fn debug() -> axum::Json<serde_json::Value> {
    let scan = scan_stranded_sessions_checked();
    let body = snapshot_with(scan.clone().unwrap_or_default());
    // The population the retirement verdict is drawn from: every hit recorded
    // on a retired port. A zero with `measured: true` is the answer this whole
    // counter exists to produce; a zero with `measured: false` is a probe that
    // did not run, and the two used to be the same payload.
    let hits = body.get("hits_total").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
    axum::Json(match scan {
        Some(_) => crate::api::measured::measured(body, hits),
        None => crate::api::measured::unmeasured(
            body,
            "`ps axeww` did not run, so stranded_count is 0 by default rather than by \
             measurement — do not read ready_to_retire from this response",
        ),
    })
}

/// One hour's worth of accounting, as decided rather than as logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HourReport {
    pub port: u16,
    pub hits_last_hour: u64,
    pub hits_this_uptime: u64,
    /// Top callers, loudest first: `("<ip> <ua>", count)`.
    pub top: Vec<(String, u64)>,
    pub clients_dropped: u64,
    pub uptime_secs: u64,
    /// WARN vs INFO. `true` means the bind still cannot be dropped.
    pub still_in_use: bool,
}

/// Close the hour: snapshot it, reset the per-hour counter, decide the verdict.
///
/// Split out of the timer loop on purpose. A decision buried inside
/// `loop { interval.tick().await; … }` can only be tested by waiting an hour,
/// which means in practice it is not tested at all — and the thing most worth
/// pinning here is that ZERO still produces a report (see [`run_reporter`]).
///
/// `None` = the bind is not enabled, so there is nothing to say.
pub fn take_hour() -> Option<HourReport> {
    let mut s = state().lock().ok()?;
    if s.port == 0 {
        return None;
    }
    let mut top: Vec<(String, u64)> = s.clients.iter().map(|(k, v)| (k.clone(), v.count)).collect();
    top.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    top.truncate(5);
    let hits_last_hour = s.hour_hits;
    s.hour_hits = 0;
    s.hour_started = now();
    Some(HourReport {
        port: s.port,
        hits_last_hour,
        hits_this_uptime: s.hits_total,
        top,
        clients_dropped: s.clients_dropped,
        uptime_secs: now().saturating_sub(s.started_at),
        still_in_use: hits_last_hour > 0,
    })
}

/// Hourly ticker. WARN while the legacy port is still being used (naming who is
/// using it, so the line is actionable without opening the API), INFO when it is
/// not — because ZERO is the signal the retirement is waiting for, and a
/// reporter that goes silent on zero cannot be told apart from a reporter that
/// died.
pub async fn run_reporter() {
    let mut tick = tokio::time::interval(Duration::from_secs(3600));
    tick.tick().await; // fires immediately; skip the t=0 tick
    loop {
        tick.tick().await;
        // Stranded-lane WARN runs EVERY tick, independent of the bind (AMUX-2988).
        // `take_hour()` returns None once the bind is dropped (port==0), which is
        // precisely when the fleet is most likely stranded — so keying the only
        // signal off the hit counter meant a broken fleet emitted nothing. This
        // makes the next stranding self-announce in `/api/logs` without a human
        // noticing first ("every fix needs a log signal").
        let stranded = scan_stranded_sessions();
        if !stranded.is_empty() {
            let names = stranded
                .iter()
                .filter_map(|s| s["session"].as_str())
                .take(20)
                .collect::<Vec<_>>()
                .join(", ");
            tracing::warn!(
                stranded_count = stranded.len(),
                retired_ports = ?RETIRED_PORTS,
                %names,
                "sessions STRANDED on a retired port — their $AMUX_URL raw-curl recipes fail; recycle to clear (GET /api/debug/legacy-port)"
            );
        }
        let Some(r) = take_hour() else { continue };
        let uptime_hours = format!("{:.1}", r.uptime_secs as f64 / 3600.0);
        if r.still_in_use {
            let callers = r
                .top
                .iter()
                .map(|(k, c)| format!("{k} x{c}"))
                .collect::<Vec<_>>()
                .join("; ");
            tracing::warn!(
                port = r.port,
                hits_last_hour = r.hits_last_hour,
                hits_this_uptime = r.hits_this_uptime,
                uptime_hours,
                clients_dropped = r.clients_dropped,
                %callers,
                "legacy port STILL IN USE — bind cannot be dropped yet"
            );
        } else {
            tracing::info!(
                port = r.port,
                hits_last_hour = 0,
                hits_this_uptime = r.hits_this_uptime,
                uptime_hours,
                "legacy port idle — retire when this stays 0 for 7d (GET /api/debug/legacy-port)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stranded-session parser (AMUX-2988). Pure over `ps` text, so it is
    /// safe as its own test — it never touches the process-global counter that
    /// forces the lifecycle test below to be a single function. Rebuilt from a
    /// real `ps axeww` line shape: pid first, env appended as KEY=VALUE tokens.
    #[test]
    fn parse_stranded_finds_retired_url_dedups_by_session_and_skips_the_rest() {
        let ps = "\
 5259   ??  Ss   0:01.00 tmux new-session AMUX_SESSION=ts-gke AMUX_URL=https://localhost:8822 TERM=xterm
 5260   ??  Ss   0:02.00 claude AMUX_SESSION=ts-gke AMUX_URL=https://localhost:8822 FOO=bar
 6001   ??  Ss   0:03.00 claude AMUX_SESSION=amux AMUX_URL=https://localhost:8824 OK=1
 6002   ??  Ss   0:04.00 claude AMUX_SESSION=backend AMUX_URL=https://localhost:8822
 7000   ??  Ss   0:05.00 some-daemon AMUX_URL=https://localhost:8822
 7001   ??  Ss   0:06.00 login -pf ethan";
        let got = parse_stranded(ps, &[8822]);
        let names: Vec<&str> = got.iter().filter_map(|r| r["session"].as_str()).collect();
        // ts-gke deduped to one row despite two matching processes; backend
        // included; amux EXCLUDED (on the live 8824); the session-less daemon
        // and the env-less login line both excluded.
        assert_eq!(names, vec!["backend", "ts-gke"], "got {got:?}");
        assert!(got.iter().all(|r| r["amux_url"].as_str().unwrap().contains(":8822")));
        // A machine fully migrated to 8824 yields nothing — no false positives.
        assert!(parse_stranded(ps, &[9999]).is_empty());
    }

    /// `legacy_port` in the published `endpoint.json` MUST name the retired port,
    /// never `null` — the Stop/PostToolUse/UserPromptSubmit report hooks baked
    /// into pre-cutover sessions self-heal a stale `AMUX_URL` by matching exactly
    /// this field. A `null` here is not cosmetic: it sent 48 live lanes' state
    /// reports to the dead 8822 and they failed silently (measured 2026-08-13:
    /// 0/48 fresh self-reports, worker status fell back fleet-wide to pane
    /// scraping). The caller passes `None`; `publish_endpoint` fills it from
    /// `RETIRED_PORTS`, so a future caller cannot re-null it by omission — which
    /// is precisely how it regressed the first time.
    #[test]
    fn published_endpoint_names_the_retired_port_for_baked_in_hooks() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // None, exactly as lib.rs passes it — the field must still resolve to the
        // retired port, because that is what the baked-in report hooks match.
        publish_endpoint(tmp.path(), 8824, None);
        let raw = std::fs::read_to_string(tmp.path().join("endpoint.json"))
            .expect("endpoint.json written");
        let ep: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
        assert_eq!(
            ep["legacy_port"], 8822,
            "legacy_port must name the retired port so baked-in report hooks \
             self-heal; a null here IS the 2026-08-13 reporting outage. got {ep}"
        );
        assert_eq!(ep["canonical_url"], "https://localhost:8824");
        // The two stale-port signals must AGREE: retired_ports (array, read by
        // the git resolver) and legacy_port (scalar, read by the report hooks)
        // have to name the same port, or a session heals one hook and not the
        // other.
        assert_eq!(ep["retired_ports"], serde_json::json!(RETIRED_PORTS));
        assert!(
            ep["retired_ports"]
                .as_array()
                .unwrap()
                .contains(&ep["legacy_port"]),
            "legacy_port must be one of retired_ports, got {ep}"
        );
    }

    /// ONE test, not several, on purpose.
    ///
    /// This module's counter is a process-global `OnceLock<Mutex<_>>` — which is
    /// correct for the thing it measures (one process, one legacy listener) and
    /// poison for tests, because cargo runs them as THREADS IN ONE PROCESS in
    /// nondeterministic order. Split across two `#[test]` fns, the "fresh state
    /// reads as disabled" assertion passed or failed depending on whether the
    /// sibling's `arm()` happened to run first: green on the first run, red on
    /// the next, with nothing changed. That exact shape is already in
    /// frustrations.md (env-mutating tests racing each other), and the fix that
    /// actually holds is not `--test-threads=1` — a flag no one passes — but
    /// keeping the sequence that shares the state inside a single test.
    ///
    /// The order below IS the assertion: never-armed, then armed-and-idle, then
    /// traffic, then the window reset.
    #[test]
    fn counter_lifecycle_disabled_then_armed_then_traffic_then_reset() {
        // (1) Never armed. The bind being enabled with zero traffic is the state
        // the retirement waits for; it must not render the same as the bind
        // being OFF, or the field the owner reads cannot distinguish "safe to
        // drop" from "already dropped, nothing was ever measured".
        let off = snapshot_with(vec![]);
        assert_eq!(off["enabled"], false, "unarmed must report enabled:false");
        assert_eq!(off["port"], 0);

        // Point the persisted window at a throwaway file. arm() restores from
        // it (AMUX-2769), so without this the test asserts against the live
        // fleet's traffic and its verdict depends on the host.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::env::set_var("AMUX_LEGACY_PORT_STATE", tmp.path().join("w.json"));

        arm(8822);
        let on = snapshot_with(vec![]);
        assert_eq!(on["enabled"], true, "armed must report enabled:true");
        assert_eq!(on["port"], 8822);
        assert_eq!(on["hits_total"], 0, "arming must not fabricate a hit");
        // Just-armed is NOT ready to retire: the 7d window has not elapsed. A
        // check that says "ready" the instant the server boots is the exact
        // false-green this instrument exists to prevent.
        assert_eq!(
            on["ready_to_retire"], false,
            "a fresh window must never read as ready to retire"
        );

        // (3) The hourly report must fire on ZERO, not only on traffic. This is
        // the whole design: silence is the signal the retirement waits for, so a
        // reporter that only speaks when something happened cannot be told apart
        // from one that died — and "no WARN lines lately" would read as progress
        // when it might be a dead task.
        //
        // Idle hour: reported, and flagged as NOT in use.
        let idle = take_hour().expect("armed bind must produce a report");
        assert_eq!(idle.port, 8822);
        assert_eq!(idle.hits_last_hour, 0);
        assert!(
            !idle.still_in_use,
            "a zero hour must not be flagged as in-use"
        );

        // Now drive the REAL accounting the middleware uses (`record`), not a
        // hand-written imitation of it.
        //
        // The two callers below wear the SAME user-agent and hit DIFFERENT
        // paths, because that is the case the tally could not express and the
        // drain depends on: on this machine `curl/8.7.1` is both the Claude
        // Code report hooks (`/api/sessions/*/report`) and every agent's own
        // ad-hoc `curl $AMUX_URL/...`. Fixing only the hooks must show up as
        // one path going quiet while the other does not — with a single
        // `last_path` field that is invisible, so "did my fix work?" was
        // unanswerable from what this module kept.
        assert_eq!(record("127.0.0.1", "curl/8", "", "/api/sessions/a/report"), 8822);
        record("127.0.0.1", "curl/8", "", "/api/sessions/a/report");
        record("127.0.0.1", "curl/8", "", "/api/board");
        let busy = take_hour().expect("still armed");
        assert_eq!(busy.hits_last_hour, 3);
        assert!(busy.still_in_use, "a non-zero hour must be flagged in-use");
        assert_eq!(
            busy.top.first().map(|(k, c)| (k.as_str(), *c)),
            Some(("127.0.0.1\tcurl/8\t", 3)),
            "the WARN line must be able to name the caller"
        );

        // THE DISCRIMINATOR. One client row, two callers, and the snapshot has
        // to separate them by path. This assertion fails on the pre-change
        // module (there was no `paths` key at all), which is what makes it a
        // check rather than decoration.
        let snap = snapshot_with(vec![]);
        let row = snap["clients"]
            .as_array()
            .and_then(|c| c.iter().find(|c| c["user_agent"] == "curl/8"))
            .expect("the curl caller must appear in the snapshot")
            .clone();
        let paths: Vec<(String, u64)> = row["paths"]
            .as_array()
            .expect("per-client path histogram")
            .iter()
            .map(|p| (p["path"].as_str().unwrap_or("").to_string(), p["count"].as_u64().unwrap_or(0)))
            .collect();
        assert_eq!(
            paths,
            vec![("/api/sessions/a/report".to_string(), 2), ("/api/board".to_string(), 1)],
            "paths must be split per caller and sorted loudest-first — one row \
             collapsing both callers is the state that made the drain unmeasurable"
        );
        assert_eq!(row["paths_dropped"], 0);

        // The window RESET is what makes "last hour" mean last hour; without it
        // the count would only ever climb and every hour after the first would
        // read as in-use forever.
        let after = take_hour().expect("still armed");
        assert_eq!(
            after.hits_last_hour, 0,
            "take_hour must reset the per-hour counter"
        );
        assert_eq!(
            after.hits_this_uptime, 3,
            "the uptime total must NOT reset — only the hour window does"
        );
        assert!(!after.still_in_use);

        // THE EXIT DISCRIMINATOR (AMUX-2769). Everything above is unattributed
        // traffic, so the verdict must say BLOCKED — that is code naming the
        // retired port and it is a defect someone has to go and fix.
        let snap = snapshot_with(vec![]);
        assert_eq!(snap["unattributed_hits"], 3);
        assert_eq!(snap["attributed_hits"], 0);
        assert!(
            snap["verdict"].as_str().unwrap_or("").starts_with("INVESTIGATE"),
            "unattributed traffic must not read as merely draining"
        );

        // Now an ATTRIBUTED lane — a pre-cutover session whose process env
        // still says 8822. Same ip, same user-agent: on the OLD key these were
        // byte-identical to the rows above and the whole fleet collapsed into
        // one number, which is exactly why "zero hits for 7 days" could never
        // be reasoned about.
        record("127.0.0.1", "curl/8", "mixpeek-studio", "/api/board");
        record("127.0.0.1", "curl/8", "amux", "/api/board");
        let snap = snapshot_with(vec![]);
        assert_eq!(snap["attributed_hits"], 2);
        assert_eq!(
            snap["sessions_still_on_legacy"],
            serde_json::json!(["amux", "mixpeek-studio"]),
            "the lanes to restart must be NAMED — an ip+ua tally cannot name them"
        );
        // Unattributed traffic still outranks: a defect is not cleared by a
        // lane that merely needs restarting.
        assert!(snap["verdict"].as_str().unwrap_or("").starts_with("INVESTIGATE"));

        // (5) THE COUNTDOWN MUST SURVIVE A RESTART (AMUX-2769). `ready_to_retire`
        // wanted 168h of observation on a process the builder re-execs several
        // times an hour — measured 2026-08-11: 316 installs, window standing at
        // 0.71h. The retirement this module exists to decide could never be
        // reached, and nothing said so: the field just read `false` forever,
        // which is indistinguishable from "still busy".
        //
        // Folded into THIS test rather than written as its own, for the exact
        // reason the doc above gives: arm() mutates shared state, so a second
        // arming test is order-dependent across threads. It failed that way on
        // the first run.
        let long_ago = now().saturating_sub(8 * 24 * 3600);
        save_persisted(long_ago, 42, long_ago);
        arm(8822); // "restart"
        let back = snapshot_with(vec![]);
        assert_eq!(back["hits_total"], 42, "a restart must not forget prior traffic");
        assert_eq!(
            back["window"]["resets_on_restart"], false,
            "the field must stop claiming a reset it no longer does"
        );
        let quiet = back["quiet_for_hours"].as_f64().expect("quiet_for_hours");
        assert!(quiet >= 168.0, "8 days of silence must read as >=168h, got {quiet}");
        assert_eq!(
            back["ready_to_retire"], true,
            "no hit for 8 days and no clients this uptime IS the all-clear"
        );

        // STRANDED overrides the all-clear (AMUX-2988). The SAME zero-traffic,
        // 8-days-quiet state that just read ready_to_retire:true must flip the
        // instant a live lane still names the retired port in its env — the
        // failure the hit-based window could never see, because a dead port
        // records no hit. This is the discriminating assertion: with the
        // stranded list non-empty and nothing else changed, verdict and
        // ready_to_retire must both move.
        let stranded_snap = snapshot_with(vec![
            serde_json::json!({"session": "probe", "pid": 1, "amux_url": "https://localhost:8822"}),
        ]);
        assert_eq!(stranded_snap["stranded_count"], 1);
        assert!(
            stranded_snap["verdict"].as_str().unwrap_or("").starts_with("STRANDED"),
            "a live lane on a retired port must read STRANDED, not CLEAR/ready"
        );
        assert_eq!(
            stranded_snap["ready_to_retire"], false,
            "a retired port with a stranded lane is not ready to retire, quiet or not"
        );

        // A fresh hit must revoke it — a one-way latch would be worse than a
        // verdict that never fires.
        record("127.0.0.1", "curl/8", "", "/api/board");
        let after = snapshot_with(vec![]);
        assert_eq!(after["ready_to_retire"], false, "a hit must revoke the all-clear");
        assert!(after["quiet_for_hours"].as_f64().unwrap_or(999.0) < 1.0);
    }
}
