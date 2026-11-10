//! Orchestration contract rule 10, phase 1 (AH-387): a worker's identity is a
//! token the server minted, not a header it asserts.
//!
//! docs/orchestration-contract.md rule 10. For a lane where
//! `contract::rule_on(home, lane, "10")` holds:
//!
//! - **Identity.** Each launch mints a random token. The server keeps only its
//!   sha256 (`~/.amux/worker-tokens/<lane>.sha256`, 0600); the value reaches the
//!   lane as `AMUX_WORKER_TOKEN` through the vault channel (tmux
//!   set-environment, then imported by name), never argv or a typed line. The
//!   CLI and hooks send it as `X-Amux-Worker-Token`. A mutating request over a
//!   real socket that claims `X-Amux-Session: <lane>` without the lane's token
//!   is refused 403 (`worker_identity_refused`). A lane launched before the
//!   rule reached it has no hash yet and passes with `worker_identity_unminted`
//!   until its next launch.
//! - **Owner.** On a rule-10 lane's card, the owner is a request that presented
//!   the owner bearer, never one that merely omitted the worker headers
//!   (`owner_by_absence_refused`, applied in the board PATCH route).
//! - **Every lane, by default (governance PR 1, 2026-11-09).** Rule 10 began
//!   opt-in per lane, so most of a fleet still spoke by assertion. Now every
//!   lane launch mints a token, Claude, Codex, Gemini and isolated workers
//!   alike (the token is environment, not a prompt, so A9 holds), unless the
//!   lane lists 10 in `AMUX_CONTRACT_RULES_OFF`. What a missing or wrong token
//!   costs a lane that is NOT on the contract is set by `AMUX_WORKER_IDENTITY`:
//!   `report` (the default) lets the request through and logs
//!   `worker_identity_would_refuse`; `enforce` refuses it 403 like a contract
//!   lane; `off` mints nothing and checks nothing (the old opt-in behaviour).
//!   Contract lanes are refused in every mode except `off`, as before. The
//!   counts are in `GET /api/contract/counters` under `worker_identity`
//!   (measured, n_considered), so `enforce` is switched on when
//!   `would_refuse` has read zero for long enough, not on faith (A11).
//! - **Credentials and sandbox** are reported, not changed, in phase 1:
//!   `report()` names (never values) the credential-like keys each rule-10
//!   lane's launch env sources, and whether it runs a permission-bypass flag
//!   without an OS sandbox. See `report()` for why neither is enforced yet.
use axum::extract::{ConnectInfo, Request};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

pub const TOKEN_ENV: &str = "AMUX_WORKER_TOKEN";
pub const TOKEN_HEADER: &str = "x-amux-worker-token";
pub const RULE: &str = "10";
pub const MODE_ENV: &str = "AMUX_WORKER_IDENTITY";

/// What a lane that is not on the contract pays for a missing or wrong token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Mint nothing outside contract lanes and check nothing there.
    Off,
    /// Mint for every lane; a failed check is logged, not refused.
    Report,
    /// Mint for every lane; a failed check is refused 403.
    Enforce,
}

impl Mode {
    pub fn parse(v: Option<&str>) -> Mode {
        match v.map(|v| v.trim().trim_matches('"').to_ascii_lowercase()).as_deref() {
            Some("off") | Some("0") | Some("false") => Mode::Off,
            Some("enforce") => Mode::Enforce,
            _ => Mode::Report,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Report => "report",
            Mode::Enforce => "enforce",
        }
    }
}

/// The server's mode, from its environment (server.env is loaded into it at
/// start). Read per request so a test or an operator flip needs no rebuild.
pub fn mode() -> Mode {
    Mode::parse(std::env::var(MODE_ENV).ok().as_deref())
}

/// How a lane's identity is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// No token is minted or checked for this lane.
    None,
    /// Minted by default; a failure costs what `Mode` says.
    Default(Mode),
    /// On the orchestration contract with rule 10: always refused.
    Contract,
}

/// Rule 10 explicitly held back for the lane (`AMUX_CONTRACT_RULES_OFF`).
fn opted_out(home: &Path, lane: &str) -> bool {
    super::contract::lane_setting(home, lane, "AMUX_CONTRACT_RULES_OFF")
        .is_some_and(|v| v.trim().trim_matches('"').split(',').any(|r| r.trim().eq_ignore_ascii_case(RULE)))
}

pub fn coverage_with(home: &Path, lane: &str, mode: Mode) -> Coverage {
    if super::contract::rule_on(home, lane, RULE) {
        return if mode == Mode::Off { Coverage::None } else { Coverage::Contract };
    }
    if mode == Mode::Off || opted_out(home, lane) {
        return Coverage::None;
    }
    Coverage::Default(mode)
}

pub fn coverage(home: &Path, lane: &str) -> Coverage {
    coverage_with(home, lane, mode())
}

/// Counts since this process started, for the A5 diagnostic.
pub(crate) mod counters {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub static CONSIDERED: AtomicU64 = AtomicU64::new(0);
    pub static VALID: AtomicU64 = AtomicU64::new(0);
    pub static UNMINTED: AtomicU64 = AtomicU64::new(0);
    pub static WOULD_REFUSE: AtomicU64 = AtomicU64::new(0);
    pub static REFUSED: AtomicU64 = AtomicU64::new(0);
    pub fn bump(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

/// Log a (lane, kind) once per process: the counters carry every instance,
/// the log names each lane once, so a fleet of unminted lanes polling the API
/// does not become a log (ethos 5).
fn first_time(lane: &str, kind: &str) -> bool {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    SEEN.get_or_init(Default::default).lock().map(|mut s| s.insert(format!("{kind}\u{0}{lane}"))).unwrap_or(true)
}

fn hash_path(home: &Path, lane: &str) -> PathBuf {
    home.join("worker-tokens").join(format!("{lane}.sha256"))
}

fn digest(token: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn random_token() -> Option<String> {
    use std::io::Read;
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom").ok()?.read_exact(&mut b).ok()?;
    Some(hex::encode(b))
}

/// Mint a fresh token for a launch of `lane`, or None when the lane is not
/// covered (a stale hash is then removed, so the lane is not held to a token it
/// no longer receives). The caller delivers the value; it is never logged.
pub fn mint(home: &Path, lane: &str) -> Option<String> {
    let path = hash_path(home, lane);
    if !super::session_verbs::valid_session_name(lane) || coverage(home, lane) == Coverage::None {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let Some(token) = random_token() else {
        tracing::warn!(lane, measured = false, n_considered = 1, verdict = "worker_token_unminted",
            why_unmeasured = "no /dev/urandom", "rule 10: could not mint a worker token; the lane launches unminted");
        return None;
    };
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(home));
    let tmp = path.with_extension("tmp");
    let wrote = std::fs::write(&tmp, digest(&token)).and_then(|_| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &path)
    });
    if let Err(e) = wrote {
        tracing::warn!(lane, error = %e, measured = true, n_considered = 1, verdict = "worker_token_unminted",
            "rule 10: could not store the worker token hash; the lane launches unminted");
        return None;
    }
    tracing::info!(lane, measured = true, n_considered = 1, verdict = "worker_token_minted",
        "rule 10: minted a worker token for this launch");
    if let Some(flags) = bypass_flag(home, lane) {
        tracing::warn!(lane, flag = flags, measured = true, n_considered = 1, verdict = "bypass_without_sandbox",
            "rule 10: the lane runs a permission-bypass flag without an OS sandbox (report-only in phase 1)");
    }
    Some(token)
}

// A SCHEDULED SHELL RUN SPEAKS AS ITS LANE (gtm-engine, 2026-10-07: SCHED-173,
// gtm-ticker's tick_runner.sh, was refused worker_identity_refused on every
// board write, so a signup that needed a human left no needsyou card). The
// lane's launch token cannot be handed over: the server keeps only its hash,
// and minting a new one would invalidate the live lane's. Each shell run gets
// its own token instead, accepted beside the launch token while the run lives
// and removed when it ends (RunToken's Drop). A hash left by a run that died
// with the server is ignored and removed after RUN_TOKEN_MAX_AGE_S.
const RUN_TOKEN_MAX_AGE_S: u64 = 6 * 3600;

fn runs_dir(home: &Path, lane: &str) -> PathBuf {
    home.join("worker-tokens").join(format!("{lane}.runs"))
}

/// A token for one scheduler-launched run of `lane`. The value goes into the
/// run's env as AMUX_WORKER_TOKEN; dropping this removes its hash.
pub struct RunToken {
    path: PathBuf,
    pub value: String,
}

impl Drop for RunToken {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn mint_run(home: &Path, lane: &str) -> Option<RunToken> {
    if !super::session_verbs::valid_session_name(lane) || coverage(home, lane) == Coverage::None {
        return None;
    }
    let token = random_token()?;
    let dir = runs_dir(home, lane);
    std::fs::create_dir_all(&dir).ok()?;
    let d = digest(&token);
    let path = dir.join(format!("{}.sha256", &d[..16]));
    std::fs::write(&path, &d).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    tracing::info!(lane, measured = true, n_considered = 1, verdict = "worker_run_token_minted",
        "rule 10: minted a token for one scheduled run of this lane");
    Some(RunToken { path, value: token })
}

fn run_token_valid(home: &Path, lane: &str, presented: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(runs_dir(home, lane)) else { return false };
    let want = digest(presented);
    let mut ok = false;
    for e in entries.flatten() {
        let p = e.path();
        let stale = e.metadata().ok().and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() > RUN_TOKEN_MAX_AGE_S);
        if stale {
            let _ = std::fs::remove_file(&p);
            continue;
        }
        if let Ok(h) = std::fs::read_to_string(&p) {
            ok |= super::auth::constant_time_eq(want.as_bytes(), h.trim().as_bytes());
        }
    }
    ok
}

#[derive(Debug, PartialEq)]
pub enum Check {
    /// The lane is not covered: headers are trusted as before.
    Unenforced,
    /// Rule 10 is on but the lane has not relaunched since: no hash to check.
    Unminted,
    Valid,
    Invalid,
}

pub fn check(home: &Path, lane: &str, presented: Option<&str>) -> Check {
    check_with(home, lane, presented, mode())
}

pub fn check_with(home: &Path, lane: &str, presented: Option<&str>, mode: Mode) -> Check {
    if coverage_with(home, lane, mode) == Coverage::None {
        return Check::Unenforced;
    }
    let presented = presented.map(str::trim).filter(|t| !t.is_empty());
    if presented.is_some_and(|t| run_token_valid(home, lane, t)) {
        return Check::Valid;
    }
    let Ok(want) = std::fs::read_to_string(hash_path(home, lane)) else {
        return Check::Unminted;
    };
    match presented {
        Some(t) if super::auth::constant_time_eq(digest(t).as_bytes(), want.trim().as_bytes()) => Check::Valid,
        _ => Check::Invalid,
    }
}

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers().get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty())
}

/// The one place a claimed worker identity is checked. Mutating requests over
/// a real socket only: the server's own in-process calls (no ConnectInfo) act
/// for lanes by design and are not a lane asserting itself.
pub async fn enforce(req: Request, next: Next) -> Response {
    enforce_in(mode(), req, next).await
}

/// `enforce` with the mode given, so a test can drive each mode on a real
/// router without setting a process-wide variable other tests read.
pub async fn enforce_in(mode: Mode, req: Request, next: Next) -> Response {
    let mutating = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    let socket = req.extensions().get::<ConnectInfo<SocketAddr>>().is_some();
    let claimed = header(&req, "x-amux-session").or_else(|| header(&req, "x-amux-worker")).map(String::from);
    if !mutating || !socket {
        return next.run(req).await;
    }
    let Some(lane) = claimed.filter(|l| super::session_verbs::valid_session_name(l)) else {
        return next.run(req).await;
    };
    let home = crate::config::amux_home();
    let covered = coverage_with(&home, &lane, mode);
    let result = check_with(&home, &lane, header(&req, TOKEN_HEADER), mode);
    if result != Check::Unenforced {
        counters::bump(&counters::CONSIDERED);
    }
    match result {
        Check::Unenforced => next.run(req).await,
        Check::Valid => {
            counters::bump(&counters::VALID);
            next.run(req).await
        }
        Check::Unminted => {
            counters::bump(&counters::UNMINTED);
            if first_time(&lane, "unminted") {
                tracing::info!(lane, measured = true, n_considered = 1, verdict = "worker_identity_unminted",
                    "rule 10: lane has no minted token yet (it gets one at its next launch); the header is trusted until then");
            }
            next.run(req).await
        }
        Check::Invalid if covered == Coverage::Default(Mode::Report) => {
            counters::bump(&counters::WOULD_REFUSE);
            if first_time(&lane, "would_refuse") {
                let presented = header(&req, TOKEN_HEADER).map(|t| digest(t)[..8].to_string());
                tracing::warn!(lane, method = %req.method(), path = %req.uri().path(), measured = true, n_considered = 1,
                    token = presented.as_deref().unwrap_or("none"), mode = Mode::Report.as_str(),
                    verdict = "worker_identity_would_refuse",
                    "a request claimed a lane without that lane's token; allowed in report mode (AMUX_WORKER_IDENTITY=enforce refuses it)");
            }
            next.run(req).await
        }
        Check::Invalid => {
            counters::bump(&counters::REFUSED);
            // Which failure: no token at all, or a token that matches no live
            // hash (a relaunched lane's old value, or another lane's). Only a
            // digest prefix is logged, never the token (2026-10-07: gs12-extra-2
            // was refused in bursts while its process held the right token, and
            // the line could not say which of the two it was).
            let presented = header(&req, TOKEN_HEADER).map(|t| digest(t)[..8].to_string());
            tracing::warn!(lane, method = %req.method(), path = %req.uri().path(), measured = true, n_considered = 1,
                token = presented.as_deref().unwrap_or("none"), contract = covered == Coverage::Contract,
                verdict = "worker_identity_refused", "rule 10: a request claimed a lane without that lane's token");
            (StatusCode::FORBIDDEN, Json(json!({
                "ok": false,
                "code": "worker_identity_refused",
                "error": format!("this request claims to be {lane} but does not carry {lane}'s worker token"),
                "how_to_fix": format!("send the {TOKEN_ENV} from the lane's own environment as X-Amux-Worker-Token (the amux CLI and hooks do this); a peer cannot act as {lane}"),
                "contract": "docs/orchestration-contract.md (rule 10)",
            }))).into_response()
        }
    }
}

/// Whether a request may act as the owner on `lane`'s card: on a rule-10 lane,
/// only with the owner bearer, never by omitting the worker headers.
pub fn owner_allowed(home: &Path, lane: &str, header_owner: bool, has_owner_token: bool, card: &str) -> bool {
    if !header_owner || has_owner_token || !super::contract::rule_on(home, lane, RULE) {
        return header_owner;
    }
    tracing::warn!(card, lane, measured = true, n_considered = 1, verdict = "owner_by_absence_refused",
        "rule 10: a request with no worker headers and no owner bearer was not treated as the owner");
    false
}

fn bypass_flag(home: &Path, lane: &str) -> Option<&'static str> {
    let flags = super::contract::lane_setting(home, lane, "CC_FLAGS").unwrap_or_default();
    ["--dangerously-skip-permissions", "--dangerously-bypass-approvals-and-sandbox", "--yolo"]
        .into_iter()
        .find(|f| flags.split_whitespace().any(|t| t.trim_matches('"') == *f))
}

/// A key name that looks like it holds a credential.
pub fn credential_like(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    if k == TOKEN_ENV || k.ends_with("_KEYS") || k.ends_with("_TOKENS_FILE") {
        return false;
    }
    ["TOKEN", "SECRET", "PASSWORD", "PASSWD", "API_KEY", "PRIVATE_KEY", "CREDENTIAL", "ACCESS_KEY"]
        .iter()
        .any(|w| k.contains(w))
}

/// The default-coverage diagnostic (A5): the mode, what the middleware saw
/// since this process started, and which known lanes hold a minted token.
/// `would_refuse` reading zero beside a non-zero `considered` is what says
/// `enforce` is safe; a zero `considered` says nothing has been measured yet.
pub fn identity_report(home: &Path, lanes: &[String]) -> Value {
    let m = mode();
    let (mut minted, mut unminted, mut uncovered) = (Vec::new(), Vec::new(), Vec::new());
    for lane in lanes {
        match coverage_with(home, lane, m) {
            Coverage::None => uncovered.push(lane.clone()),
            _ if hash_path(home, lane).is_file() => minted.push(lane.clone()),
            _ => unminted.push(lane.clone()),
        }
    }
    use counters::*;
    let considered = get(&CONSIDERED);
    json!({
        "mode": m.as_str(),
        "measured": true,
        "n_considered": considered,
        "since": "server start",
        "requests": {
            "considered": considered,
            "valid": get(&VALID),
            "unminted": get(&UNMINTED),
            "would_refuse": get(&WOULD_REFUSE),
            "refused": get(&REFUSED),
        },
        "lanes": {
            "n_considered": lanes.len(),
            "minted": minted,
            "unminted": unminted,
            "not_covered": uncovered,
        },
        "how_to_enforce": format!("{MODE_ENV}=enforce in server.env, then restart, once would_refuse stays 0"),
    })
}

/// Phase 1 report for every rule-10 lane: credential-like key NAMES in the env
/// files its launch sources, and whether it runs a bypass flag unsandboxed.
///
/// Nothing is stripped or sandboxed yet, deliberately:
/// - the push credential is needed in the lane until rule 5's server land queue
///   pushes for it; provider keys are needed by the provider CLI itself; vault
///   secrets are the lane's own keys, delivered by design;
/// - the hooks read `~/.amux/auth_token` (the owner bearer) from disk to post
///   status, so any process under this uid can present it; closing that needs
///   the sandbox;
/// - an OS sandbox around an interactive lane (sandbox-exec is deprecated, and
///   the delegate profile only denies writes under named paths) would also cut
///   its git writes to shared object stores and its tmux socket, so it waits
///   for per-lane worktrees (rule 4) to define what a lane may write.
pub fn report(home: &Path, lanes: &[String]) -> Value {
    let mut rows = Vec::new();
    for lane in lanes.iter().filter(|l| super::contract::rule_on(home, l, RULE)) {
        let mut keys = std::collections::BTreeSet::new();
        for layer in super::session_verbs::scope_env_layers(home, lane) {
            for k in crate::config::parse_env_file(&layer).keys() {
                if credential_like(k) {
                    keys.insert(k.clone());
                }
            }
        }
        if !keys.is_empty() {
            tracing::info!(lane, keys = keys.len(), measured = true, n_considered = 1, verdict = "credentials_in_worker_env",
                "rule 10: credential-like keys reach this lane's launch env (names only)");
        }
        rows.push(json!({
            "lane": lane,
            "token_minted": hash_path(home, lane).is_file(),
            "credential_keys": keys.into_iter().collect::<Vec<_>>(),
            "bypass_flag": bypass_flag(home, lane),
            "sandboxed": false,
        }));
    }
    json!({
        "measured": true,
        "n_considered": rows.len(),
        "lanes": rows,
        "phase": "1: identity enforced; credentials and sandbox reported, not changed (see worker_identity::report)",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(env: &str) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("sessions")).unwrap();
        std::fs::write(d.path().join("sessions/w.env"), env).unwrap();
        d
    }

    #[test]
    fn a_minted_token_is_the_only_identity_for_a_rule_10_lane() {
        let d = home_with("AMUX_CONTRACT_DONE=1\n");
        let h = d.path();
        assert_eq!(check(h, "w", None), Check::Unminted, "before a launch the header is trusted");
        let t = mint(h, "w").expect("rule 10 on: a token is minted");
        assert!(!std::fs::read_to_string(hash_path(h, "w")).unwrap().contains(&t), "only the hash is stored");
        assert_eq!(check(h, "w", Some(&t)), Check::Valid);
        assert_eq!(check(h, "w", None), Check::Invalid, "a bare header is refused");
        assert_eq!(check(h, "w", Some("forged")), Check::Invalid);
        let t2 = mint(h, "w").unwrap();
        assert_eq!(check(h, "w", Some(&t)), Check::Invalid, "a relaunch retires the old token");
        assert_eq!(check(h, "w", Some(&t2)), Check::Valid);
    }

    /// gtm-engine, 2026-10-07 (SCHED-173): a scheduled shell run speaks as
    /// its lane with its own token, beside the live lane's, until it ends.
    #[test]
    fn a_scheduled_runs_token_is_valid_beside_the_lanes_until_the_run_ends() {
        let d = home_with("AMUX_CONTRACT_DONE=1\n");
        let h = d.path();
        let lane = mint(h, "w").unwrap();
        let run = mint_run(h, "w").expect("rule 10 on: a run token is minted");
        assert_eq!(check(h, "w", Some(&run.value)), Check::Valid, "the run speaks as the lane");
        assert_eq!(check(h, "w", Some(&lane)), Check::Valid, "the lane's own token still works");
        assert_eq!(check_with(h, "x", Some(&run.value), Mode::Report), Check::Unminted,
            "another lane is not given w's run token: it has no token of its own yet");
        let v = run.value.clone();
        drop(run);
        assert_eq!(check(h, "w", Some(&v)), Check::Invalid, "a finished run's token is retired");
        assert_eq!(check(h, "w", Some(&lane)), Check::Valid);
    }

    #[test]
    fn rule_10_off_mints_nothing_and_clears_a_stale_hash() {
        let d = home_with("AMUX_CONTRACT_DONE=1\n");
        let h = d.path();
        mint(h, "w").unwrap();
        std::fs::write(h.join("sessions/w.env"), "AMUX_CONTRACT_DONE=1\nAMUX_CONTRACT_RULES_OFF=\"6,7,10\"\n").unwrap();
        assert_eq!(mint(h, "w"), None);
        assert!(!hash_path(h, "w").exists(), "the lane is not held to a token it no longer receives");
        assert_eq!(check(h, "w", None), Check::Unenforced);
    }

    #[test]
    fn the_owner_on_a_rule_10_card_presents_the_bearer() {
        let d = home_with("AMUX_CONTRACT_DONE=1\n");
        let h = d.path();
        assert!(!owner_allowed(h, "w", true, false, "T-1"), "omitting headers is not ownership");
        assert!(owner_allowed(h, "w", true, true, "T-1"), "the owner bearer is");
        assert!(!owner_allowed(h, "w", false, true, "T-1"), "a worker header is never the owner");
        let off = home_with("");
        assert!(owner_allowed(off.path(), "w", true, false, "T-1"), "rule 10 off: unchanged");
    }

    /// The shipped middleware on a real router: a claimed rule-10 lane needs its
    /// token over a socket; the server's in-process calls (no ConnectInfo) and
    /// reads pass.
    #[tokio::test]
    async fn the_middleware_refuses_a_claimed_lane_without_its_token() {
        use tower::ServiceExt;
        let d = home_with("AMUX_CONTRACT_DONE=1\n");
        let _g = crate::api::settings::test_env::set_home(d.path());
        let token = mint(d.path(), "w").unwrap();
        let app = axum::Router::new()
            .route("/x", axum::routing::post(|| async { "ok" }).get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(enforce));
        let call = |method: &str, tok: Option<&str>, socket: bool| {
            let mut b = axum::http::Request::builder().method(method).uri("/x").header("x-amux-session", "w");
            if let Some(t) = tok {
                b = b.header(TOKEN_HEADER, t);
            }
            let mut req = b.body(axum::body::Body::empty()).unwrap();
            if socket {
                req.extensions_mut().insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
            }
            app.clone().oneshot(req)
        };
        assert_eq!(call("POST", None, true).await.unwrap().status(), StatusCode::FORBIDDEN);
        assert_eq!(call("POST", Some("forged"), true).await.unwrap().status(), StatusCode::FORBIDDEN);
        assert_eq!(call("POST", Some(&token), true).await.unwrap().status(), StatusCode::OK);
        assert_eq!(call("GET", None, true).await.unwrap().status(), StatusCode::OK, "reads are not identity claims");
        assert_eq!(call("POST", None, false).await.unwrap().status(), StatusCode::OK, "in-process harness calls");
    }

    /// Governance PR 1 (2026-11-09): a lane that is NOT on the contract gets a
    /// token at launch too, unless it opts out of rule 10 or the mode is off.
    #[test]
    fn every_lane_is_covered_by_default_and_opts_out_by_name() {
        let plain = home_with("");
        let h = plain.path();
        assert_eq!(coverage_with(h, "w", Mode::Report), Coverage::Default(Mode::Report));
        assert_eq!(coverage_with(h, "w", Mode::Enforce), Coverage::Default(Mode::Enforce));
        assert_eq!(coverage_with(h, "w", Mode::Off), Coverage::None, "off is the old opt-in behaviour");
        assert_eq!(check_with(h, "w", None, Mode::Report), Check::Unminted, "a lane that has not relaunched is reported, not refused");
        let t = mint(h, "w").expect("no contract, default mode: a token is minted anyway");
        assert_eq!(check_with(h, "w", Some(&t), Mode::Report), Check::Valid);
        assert_eq!(check_with(h, "w", None, Mode::Report), Check::Invalid);
        assert_eq!(check_with(h, "w", None, Mode::Off), Check::Unenforced);

        let out = home_with("AMUX_CONTRACT_RULES_OFF=\"10\"\n");
        assert_eq!(coverage_with(out.path(), "w", Mode::Enforce), Coverage::None, "a lane can opt out by name");
        assert_eq!(mint(out.path(), "w"), None);

        let contract = home_with("AMUX_CONTRACT_DONE=1\n");
        assert_eq!(coverage_with(contract.path(), "w", Mode::Report), Coverage::Contract,
            "a contract lane is held as before, whatever the default mode");
        assert_eq!(coverage_with(contract.path(), "w", Mode::Off), Coverage::None);
        assert_eq!(Mode::parse(None), Mode::Report, "report is the default");
        assert_eq!(Mode::parse(Some("\"enforce\"")), Mode::Enforce);
        assert_eq!(Mode::parse(Some("off")), Mode::Off);
    }

    /// The shipped middleware for a lane that is not on the contract: report
    /// lets a tokenless claim through and counts it, enforce refuses it, and a
    /// lane's own token passes in both.
    #[tokio::test]
    async fn report_mode_counts_what_enforce_mode_refuses() {
        use tower::ServiceExt;
        let d = home_with("");
        let _g = crate::api::settings::test_env::set_home(d.path());
        let token = mint(d.path(), "w").unwrap();
        let app = |m: Mode| axum::Router::new()
            .route("/x", axum::routing::post(|| async { "ok" }))
            .layer(axum::middleware::from_fn(move |r, n| enforce_in(m, r, n)));
        let call = |m: Mode, tok: Option<&str>| {
            let mut b = axum::http::Request::builder().method("POST").uri("/x").header("x-amux-session", "w");
            if let Some(t) = tok {
                b = b.header(TOKEN_HEADER, t);
            }
            let mut req = b.body(axum::body::Body::empty()).unwrap();
            req.extensions_mut().insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
            app(m).oneshot(req)
        };
        let before = counters::get(&counters::WOULD_REFUSE);
        assert_eq!(call(Mode::Report, None).await.unwrap().status(), StatusCode::OK, "report: allowed");
        assert!(counters::get(&counters::WOULD_REFUSE) > before, "report: counted as would_refuse");
        assert_eq!(call(Mode::Enforce, None).await.unwrap().status(), StatusCode::FORBIDDEN, "enforce: refused");
        assert_eq!(call(Mode::Enforce, Some("forged")).await.unwrap().status(), StatusCode::FORBIDDEN);
        assert_eq!(call(Mode::Enforce, Some(&token)).await.unwrap().status(), StatusCode::OK, "its own token passes");
        assert_eq!(call(Mode::Off, None).await.unwrap().status(), StatusCode::OK, "off: unchecked");
        let r = identity_report(d.path(), &["w".to_string()]);
        assert_eq!(r["measured"], true);
        assert!(r["n_considered"].as_u64().unwrap() >= 4);
        assert_eq!(r["lanes"]["minted"], json!(["w"]));
    }

    #[test]
    fn the_report_names_credential_keys_and_unsandboxed_bypass_flags() {
        let d = home_with("AMUX_CONTRACT_DONE=1\nCC_FLAGS=\"--dangerously-skip-permissions --model x\"\nGITHUB_TOKEN=abc\nAMUX_VAULT_KEYS=a\nPLAIN_SETTING=1\n");
        let r = report(d.path(), &["w".to_string(), "absent".to_string()]);
        assert_eq!(r["n_considered"], 1);
        let row = &r["lanes"][0];
        assert_eq!(row["credential_keys"], json!(["GITHUB_TOKEN"]));
        assert_eq!(row["bypass_flag"], "--dangerously-skip-permissions");
        assert!(!r.to_string().contains("abc"), "names only, never values");
    }
}
