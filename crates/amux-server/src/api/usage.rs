//! GET /api/usage — provider subscription usage for the Settings meter.
//!
//! The legacy top-level response remains Anthropic's body plus `available`, so
//! older clients keep working. The `providers[]` collection is the complete
//! Settings contract: Claude, Codex, Gemini, and the honest unmetered state for
//! local Ollama. Provider-specific fields stay under their provider row rather
//! than being collapsed into a lowest-common-denominator percentage.
//!
//! # Why the detailed rows do not go through `ProviderAdapter::usage()`
//!
//! The Claude meter used to, and that is why it was dark. The adapter returns
//! NORMALIZED [`UsageWindow`]s for capacity routing, which is a deliberately
//! lossy view: it keeps kind/percent/reset and discards the provider-specific
//! fields this SPA renders — `limits[].scope.model.display_name` (the
//! per-model weekly rows), `limits[].group`, and the exact `kind` spelling
//! the renderer switches on. Re-deriving those from a normalized window is
//! guesswork, so the endpoint consumes the probe DIRECTLY and passes
//! Anthropic's body through verbatim, exactly as Python did. The SPA's
//! `loadUsage()` therefore sees byte-identical fields to the Python server.
//!
//! The adapter is still the owner of the probe — [`probe_usage_raw`] lives in
//! `provider/claude.rs` next to the credential reader, and
//! `ProviderAdapter::usage()` calls the same function. One token
//! acquisition, one HTTP call, two consumers with different honesty
//! requirements: routing needs a number or nothing (Invariant 20), a human
//! needs to know WHICH thing went wrong.
//!
//! # Discriminated degradation (ethos rule 4)
//!
//! The previous single sentence — "no token, expired token, or probe failed"
//! — was true and useless: it is three causes with three different remedies,
//! so nobody could tell a broken login from a transient rate limit, and the
//! meter stayed dark without anyone knowing which to fix. During development
//! (2026-08-09) this host's probe was answering **HTTP 429** with a perfectly
//! valid, unexpired keychain token — a self-healing condition that read as a
//! missing credential. Every cause now has its own reason string, and an HTTP
//! failure carries its status code.
//!
//! # Secrets
//!
//! No response on any path can contain a token. Claude's [`UsageProbe`] cannot
//! carry one, and the Codex/Gemini shapers allow-list quota, plan, and credit
//! fields instead of forwarding either provider's account envelope.

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json, Router};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::AppState;
use crate::provider::claude::{probe_usage_raw, UsageProbe};

// ---------------------------------------------------------------------------
// BACKGROUND RESERVE (AMUX-3545) — a share of the plan window background work
// may not consume.
// ---------------------------------------------------------------------------

/// The share of the plan window reserved for the human, as a percent.
///
/// Ethan's call, 2026-08-25: **30**. Background work pauses once the session
/// window is 70% used, so roughly a third of every window is still there when he
/// sits down.
///
/// The measurement behind the question: in the 5-hour window that day, 2,270
/// turns and 82.1% background — 1,059 peer-message turns, 552 schedule, 50
/// pickup, against 505 of his own. A person on a $20 plan typing 2-3 prompts was
/// competing with all of that inside the window their plan meters.
///
/// A PREF, NOT A CONSTANT. D4 is this repo's record of what a compiled-in
/// context policy costs: it becomes the ceiling silently. `0` disables the
/// reserve entirely, which is the honest off switch.
pub fn background_reserve_pct() -> i64 {
    std::env::var("AMUX_BACKGROUND_RESERVE_PCT")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|n| (0..=95).contains(n))
        .unwrap_or(30)
}

/// Should background work pause right now?
///
/// PURE, so the decision is testable without a plan window — and so the one
/// property that matters can be pinned: **it fails OPEN.**
///
/// `window_pct == None` means the reading is missing or stale, and the answer is
/// then "do not pause". Pausing on an unknown would stop every schedule and
/// every pickup across the fleet the moment the usage probe rate-limits or the
/// OAuth token expires — turning a credit guard into a fleet outage triggered by
/// a third party. Under-protecting for one window is recoverable; the inverse is
/// not, and it would be blamed on anything but the probe.
pub fn background_should_pause(window_pct: Option<i64>, reserve_pct: i64) -> bool {
    if reserve_pct <= 0 {
        return false;
    }
    match window_pct {
        Some(p) => p >= 100 - reserve_pct,
        None => false,
    }
}

/// Last observed session-window utilisation, and when it was observed.
///
/// Written by `get_usage` as a side effect of serving a request, so on a box
/// with an open dashboard this stays fresh for free. `window_pct_fresh` is the
/// only reader and it enforces the age bound, because a percentage from an hour
/// ago is not a reading of the current window.
static WINDOW_PCT: std::sync::Mutex<Option<(Instant, i64)>> = std::sync::Mutex::new(None);

/// Record a fresh session-window reading. Idempotent, cheap, never fails.
pub fn note_window_pct(pct: i64) {
    if let Ok(mut g) = WINDOW_PCT.lock() {
        *g = Some((Instant::now(), pct));
    }
}

/// The reading, if it is younger than `max_age`. `None` is a real answer and
/// the caller must treat it as "unknown", never as zero.
pub fn window_pct_fresh(max_age: Duration) -> Option<i64> {
    let g = WINDOW_PCT.lock().ok()?;
    let (at, pct) = (*g)?;
    (at.elapsed() < max_age).then_some(pct)
}

/// How stale a window reading may be and still bound a decision.
///
/// A 5-hour window moves slowly, but a percentage from an hour ago is not a
/// reading of the CURRENT one. Deliberately longer than the probe TTL so a
/// dashboard that is merely idle does not tip the fleet into fail-open.
const RESERVE_MAX_AGE: Duration = Duration::from_secs(600);

/// Minimum gap between reserve-driven probes, so the background consumers
/// cannot amplify a rate limit by asking harder when the answer is missing —
/// the failure the usage cache's own comment already records for the HTTP path.
const RESERVE_PROBE_EVERY: Duration = Duration::from_secs(120);

static LAST_RESERVE_PROBE: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

/// Should background work pause right now, probing at most once per
/// [`RESERVE_PROBE_EVERY`] if no fresh reading is on hand (AMUX-3545).
///
/// THE CONSUMER REFRESHES WHAT IT CONSUMES. `get_usage` notes a reading as a
/// side effect of serving the dashboard, which keeps this free on a box someone
/// is looking at; a headless box has nobody polling, and a reserve that silently
/// never engages there would be the ethos-1 failure — capability that exists and
/// reaches nobody. So the schedulers top it up themselves, rate-limited.
///
/// Fails OPEN at every step: no reading, a failed probe, a probe we declined to
/// make because one was recent — all of them mean "do not pause".
pub async fn background_should_pause_now() -> bool {
    let reserve = background_reserve_pct();
    if reserve <= 0 {
        return false;
    }
    if let Some(p) = window_pct_fresh(RESERVE_MAX_AGE) {
        return background_should_pause(Some(p), reserve);
    }
    // No fresh reading. Probe, but only if we have not just tried.
    {
        let mut g = match LAST_RESERVE_PROBE.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        if let Some(at) = *g {
            if at.elapsed() < RESERVE_PROBE_EVERY {
                return false;
            }
        }
        *g = Some(Instant::now());
    }
    let probe = crate::provider::claude::probe_usage_raw().await;
    if let Some(body) = probe.exact_body() {
        if let Some(pct) = session_pct_of(&shape_probe(UsageProbe::Ok(body.clone()))) {
            note_window_pct(pct);
            return background_should_pause(Some(pct), reserve);
        }
    }
    false
}

/// The sentence a paused consumer prints, so a skipped fire is never silent.
pub fn reserve_pause_note(kind: &str) -> String {
    let reserve = background_reserve_pct();
    let pct = window_pct_fresh(RESERVE_MAX_AGE).unwrap_or(-1);
    format!(
        "{kind} PAUSED: the plan window is {pct}% used and {reserve}% is reserved for the human \
         (AMUX_BACKGROUND_RESERVE_PCT). Background work stops here so a prompt you type still \
          has room; direct sends are never gated. Set the pref to 0 to disable the reserve."
    )
}

/// Pull the `session` window's percent out of a shaped usage body.
///
/// The `limits` array is the shape `/api/usage` already returns; `kind` is the
/// discriminator and `session` is the 5-hour window the plan meters.
pub fn session_pct_of(body: &Value) -> Option<i64> {
    body.get("limits")?
        .as_array()?
        .iter()
        .find(|l| l.get("kind").and_then(Value::as_str) == Some("session"))
        .and_then(|l| l.get("percent"))
        .and_then(Value::as_i64)
}

/// Default cache TTL. Python used 30s; this defaults to 60 because the probe
/// is a NETWORK call made on a settings render, and the endpoint is
/// rate-limited per account — on a host running a fleet of Claude Code
/// processes, that budget is shared with all of them. Override with
/// `AMUX_USAGE_TTL_S` (0 disables caching, for debugging).
const DEFAULT_USAGE_TTL_S: u64 = 60;

/// How long a SUCCESSFUL reading keeps being served after a later probe
/// fails. `AMUX_USAGE_STALE_S`; 0 disables the fallback.
///
/// This exists because the failure is INTERMITTENT, which was only visible by
/// watching two servers at once: on 2026-08-09 one build served real limits
/// while another got HTTP 429 twenty seconds later, same host, same account.
/// With no fallback the meter flickers between real numbers and an error —
/// and "it says unavailable" is the bug being fixed here. Ten minutes is well
/// inside the resolution of the thing being measured (5-hour and 7-day
/// windows), so a reading this old is still true to the precision displayed.
const DEFAULT_USAGE_STALE_S: u64 = 600;

fn env_secs(key: &str, default: u64) -> Duration {
    Duration::from_secs(
        std::env::var(key)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(default),
    )
}

fn usage_ttl() -> Duration {
    env_secs("AMUX_USAGE_TTL_S", DEFAULT_USAGE_TTL_S)
}

fn usage_stale_window() -> Duration {
    env_secs("AMUX_USAGE_STALE_S", DEFAULT_USAGE_STALE_S)
}

/// A probe as an injectable dependency, so tests exercise the real handler —
/// cache, shaping and all — against fixtures, and never touch the network or
/// this machine's keychain.
pub type ProbeFn = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = UsageProbe> + Send>> + Send + Sync>;

#[derive(Debug, Clone)]
enum ProviderProbe {
    Ok(Value),
    Unavailable { cause: &'static str, reason: String },
}

type ProviderProbeFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ProviderProbe> + Send>> + Send + Sync>;

#[derive(Clone)]
struct UsageProbes {
    claude: ProbeFn,
    codex: ProviderProbeFn,
    gemini: ProviderProbeFn,
}

#[derive(Default)]
struct UsageCache {
    /// The shaped body WITHOUT its age field — age is stamped per response,
    /// because a cached reading gets older while it sits here.
    data: Option<Value>,
    at: Option<Instant>,
    /// The last body that actually carried numbers, kept separately so a
    /// transient failure cannot evict a good reading.
    last_good: Option<Value>,
    last_good_at: Option<Instant>,
}

/// Production wiring: every provider's real read-only usage surface.
pub fn routes() -> Router<AppState> {
    routes_with_probes(UsageProbes {
        claude: Arc::new(|| Box::pin(probe_usage_raw())),
        codex: Arc::new(|| Box::pin(probe_codex_usage())),
        gemini: Arc::new(|| Box::pin(probe_gemini_usage())),
    })
}

/// Existing Claude-focused test seam. Other providers degrade explicitly so
/// old tests stay hermetic while the response still proves total coverage.
pub fn routes_with(probe: ProbeFn) -> Router<AppState> {
    let unavailable = |provider: &'static str| -> ProviderProbeFn {
        Arc::new(move || {
            Box::pin(async move {
                ProviderProbe::Unavailable {
                    cause: "test_probe_not_configured",
                    reason: format!("{provider} usage test probe is not configured"),
                }
            })
        })
    };
    routes_with_probes(UsageProbes {
        claude: probe,
        codex: unavailable("Codex"),
        gemini: unavailable("Gemini"),
    })
}

fn routes_with_probes(probes: UsageProbes) -> Router<AppState> {
    Router::new()
        .route("/", axum::routing::get(get_usage))
        .route("/attribution", axum::routing::get(get_attribution))
        // AMUX-4584: the same numbers as markdown, deterministic, for MDAI.
        .route("/report.md", axum::routing::get(get_usage_report_md))
        .layer(Extension(probes))
        .layer(Extension(Arc::new(tokio::sync::Mutex::new(
            UsageCache::default(),
        ))))
}

async fn get_usage(
    Extension(probes): Extension<UsageProbes>,
    Extension(cache): Extension<Arc<tokio::sync::Mutex<UsageCache>>>,
) -> Response {
    let ttl = usage_ttl();
    let mut c = cache.lock().await;

    // Serve from cache while fresh. Failures are cached too, deliberately: a
    // rate-limited probe must not be retried on every settings render, which
    // is the behaviour that provokes the rate limit in the first place.
    let fresh = matches!((&c.data, c.at), (Some(_), Some(at)) if at.elapsed() < ttl);
    if !fresh {
        let (claude, codex, gemini) =
            tokio::join!((probes.claude)(), (probes.codex)(), (probes.gemini)());
        let shaped = shape_all_providers(claude, codex, gemini);
        if shaped.get("available") == Some(&json!(true))
            && shaped.get("stale") != Some(&json!(true))
        {
            c.last_good = Some(shaped.clone());
            c.last_good_at = Some(Instant::now());
        }
        c.data = Some(shaped);
        c.at = Some(Instant::now());
    }

    let mut body = c.data.clone().unwrap_or_else(|| json!({}));
    let mut age = c.at.map(|t| t.elapsed()).unwrap_or(Duration::ZERO);

    // A failed probe with a recent good reading in hand: serve the reading
    // rather than nothing. Everything here was really fetched — only its age
    // changed — and both the age and the live failure travel with it, so the
    // response never claims to be something it is not.
    let mut stale_reason: Option<Value> = None;
    if body.get("available") != Some(&json!(true))
        && body.get("cache_managed") != Some(&json!(true))
        && matches!(
            body.get("cause").and_then(Value::as_str),
            Some("rate_limited" | "probe_failed" | "unexpected_shape")
        )
    {
        let stale_window = usage_stale_window();
        if let (Some(good), Some(at)) = (&c.last_good, c.last_good_at) {
            if !stale_window.is_zero() && at.elapsed() < stale_window {
                stale_reason = body.get("reason").cloned();
                // Preserve today's Codex/Gemini result, not the old envelope
                // that happened to accompany the last Claude success.
                let providers = body.get("providers").cloned();
                body = good.clone();
                if let Some(providers) = providers {
                    body["providers"] = providers;
                }
                age = at.elapsed();
            }
        }
    }

    if let Some(observed) = body.get("observed_at").and_then(Value::as_i64) {
        age = Duration::from_secs((chrono::Utc::now().timestamp() - observed).max(0) as u64);
    }
    // How old is this reading? A meter that silently shows a minute-old
    // number is fine; one that cannot tell you it is doing so is not.
    if let Some(obj) = body.as_object_mut() {
        obj.insert("cache_age_s".into(), json!(age.as_secs()));
        obj.insert("cache_ttl_s".into(), json!(ttl.as_secs()));
        if let Some(reason) = stale_reason {
            obj.insert("stale".into(), json!(true));
            // Why the live probe failed, kept beside the served numbers so
            // "these are 4 minutes old because of a 429" is answerable.
            obj.insert("stale_reason".into(), reason);
        }
    }
    // Keep the provider row's age/recovery metadata alongside its own numbers.
    let claude = shape_claude_provider(&body);
    if let Some(providers) = body.get_mut("providers").and_then(Value::as_array_mut) {
        if let Some(row) = providers.iter_mut().find(|p| p["id"] == "claude") {
            *row = claude;
        }
    }
    Json(body).into_response()
}

/// Read Codex's supported account/rateLimits/read JSON-RPC surface. The
/// app-server receives no prompt and no mutation method; only the usage result
/// crosses this boundary, never account identity.
fn codex_probe_process(shell: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(shell);
    command.args([
        "-lc",
        "exec codex app-server --stdio --disable remote_control",
    ]);
    command
}

async fn probe_codex_usage() -> ProviderProbe {
    // The server is normally launched by launchd/systemd, whose PATH is not
    // the user's interactive PATH. On this machine launchd found an abandoned
    // `/usr/local/bin/codex` wrapper first; the wrapper itself existed, so
    // spawn succeeded, but its packaged native binary did not. Every worker
    // launched from amux runs through the user's login shell and found the
    // current nvm-installed Codex instead. Do the same here: the usage probe
    // must measure the provider binary the user actually runs, not whichever
    // stale shim the service manager happens to put first (AMUX-4154).
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let mut child = match codex_probe_process(&shell)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return provider_probe_unavailable(
                "codex",
                "shell_missing",
                "The user's login shell is unavailable, so Codex usage cannot be read.",
            )
        }
        Err(_) => {
            return provider_probe_unavailable(
                "codex",
                "probe_failed",
                "Codex account usage probe could not start.",
            )
        }
    };
    let request = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":",
        "{\"clientInfo\":{\"name\":\"amux-usage-probe\",\"version\":\"1\"},",
        "\"capabilities\":{\"experimentalApi\":true}}}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"account/rateLimits/read\",\"params\":null}\n",
    );
    let Some(mut stdin) = child.stdin.take() else {
        return provider_probe_unavailable(
            "codex",
            "probe_failed",
            "Codex account usage probe has no input channel.",
        );
    };
    let Some(stdout) = child.stdout.take() else {
        return provider_probe_unavailable(
            "codex",
            "probe_failed",
            "Codex account usage probe has no output channel.",
        );
    };
    if stdin.write_all(request.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
        let _ = child.kill().await;
        return provider_probe_unavailable(
            "codex",
            "probe_failed",
            "Codex account usage request could not be sent.",
        );
    }
    let read = async {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next_line().await? {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("id") == Some(&json!(2)) {
                return Ok::<Option<Value>, std::io::Error>(message.get("result").cloned());
            }
        }
        Ok(None)
    };
    let result = tokio::time::timeout(Duration::from_secs(12), read).await;
    let _ = child.kill().await;
    let _ = child.wait().await;
    match result {
        Ok(Ok(Some(body))) => {
            tracing::debug!(target: "amux::usage_probe", provider = "codex", verdict = "measured",
                "subscription usage probe succeeded");
            ProviderProbe::Ok(body)
        }
        Ok(Ok(None)) => provider_probe_unavailable(
            "codex",
            "unexpected_shape",
            "Codex account usage returned no rate-limit snapshot.",
        ),
        Ok(Err(_)) | Err(_) => provider_probe_unavailable(
            "codex",
            "probe_failed",
            "Codex account usage probe timed out or disconnected.",
        ),
    }
}

/// Run the Gemini helper through Node stdin so the installed CLI's own OAuth
/// client is reused without adding its private implementation as a Rust API.
async fn probe_gemini_usage() -> ProviderProbe {
    let mut child = match tokio::process::Command::new("node")
        .args(["--input-type=module", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return provider_probe_unavailable(
                "gemini",
                "runtime_missing",
                "Node.js is not installed, so Gemini CLI quota cannot be read.",
            )
        }
        Err(_) => {
            return provider_probe_unavailable(
                "gemini",
                "probe_failed",
                "Gemini account usage probe could not start.",
            )
        }
    };
    let script = include_str!("../../../../scripts/provider-usage-gemini.mjs");
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(script.as_bytes()).await.is_err() {
            let _ = child.kill().await;
            return provider_probe_unavailable(
                "gemini",
                "probe_failed",
                "Gemini account usage request could not be sent.",
            );
        }
    }
    let output = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output()).await;
    match output {
        Ok(Ok(output)) => match serde_json::from_slice::<Value>(&output.stdout) {
            Ok(body) if body.get("available") == Some(&json!(true)) => {
                tracing::debug!(target: "amux::usage_probe", provider = "gemini", verdict = "measured",
                    "subscription usage probe succeeded");
                ProviderProbe::Ok(body)
            }
            Ok(body) => {
                let cause = match body.get("cause").and_then(Value::as_str) {
                    Some("account_quota_not_reported") => "account_quota_not_reported",
                    Some("cli_missing") => "cli_missing",
                    Some("unsupported_cli") => "unsupported_cli",
                    Some("quota_project_unavailable") => "quota_project_unavailable",
                    _ => "probe_failed",
                };
                let reason = body
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("Gemini account usage is unavailable.");
                provider_probe_unavailable("gemini", cause, reason)
            }
            Err(_) => provider_probe_unavailable(
                "gemini",
                "unexpected_shape",
                "Gemini account usage returned an unexpected response.",
            ),
        },
        Ok(Err(_)) | Err(_) => provider_probe_unavailable(
            "gemini",
            "probe_failed",
            "Gemini account usage probe timed out or disconnected.",
        ),
    }
}

fn provider_probe_unavailable(
    provider: &'static str,
    cause: &'static str,
    reason: &str,
) -> ProviderProbe {
    tracing::warn!(target: "amux::usage_probe", provider, cause, verdict = "unavailable", "{reason}");
    ProviderProbe::Unavailable {
        cause,
        reason: reason.to_string(),
    }
}

fn shape_all_providers(
    claude_probe: UsageProbe,
    codex_probe: ProviderProbe,
    gemini_probe: ProviderProbe,
) -> Value {
    let mut body = shape_probe(claude_probe);
    let providers = vec![
        shape_claude_provider(&body),
        shape_codex_provider(codex_probe),
        shape_gemini_provider(gemini_probe),
        json!({
            "id": "ollama", "label": "Ollama", "available": true,
            "measured": true, "n_considered": 0, "metered": false, "local": true,
            "summary": "Local models have no subscription limit", "windows": [],
        }),
        // Muse Code is the first provider that is METERED but UNREADABLE, and
        // neither existing spelling tells that truth. `metered: false` renders
        // "Unlimited" (ollama's row, correct for a local model and a lie for a
        // hosted one); `metered: true` with no windows renders "No active
        // limits", which asserts a measurement nobody took. Meta ships no usage
        // API for muse today, so `usage_unknown` says exactly that and the
        // dashboard prints "Usage unknown" — Invariant 20's whole point is that
        // an absent number stays absent instead of resolving to a flattering
        // default. Delete this field the day muse exposes a quota endpoint.
        json!({
            "id": "muse", "label": "Muse Code", "available": true,
            "measured": false, "n_considered": 0, "metered": true,
            "local": false, "usage_unknown": true,
            "summary": "Muse Code exposes no usage API; consumption is unknown",
            "windows": [],
        }),
    ];
    if let Some(obj) = body.as_object_mut() {
        let measured = providers
            .iter()
            .filter(|provider| provider.get("metered") != Some(&json!(false)))
            .any(|provider| provider.get("measured") == Some(&json!(true)));
        let n_considered = providers
            .iter()
            .filter_map(|provider| provider.get("n_considered").and_then(Value::as_u64))
            .sum::<u64>();
        obj.insert("measured".into(), json!(measured));
        obj.insert("n_considered".into(), json!(n_considered));
        obj.insert("provider_count".into(), json!(providers.len()));
        obj.insert("providers".into(), Value::Array(providers));
    }
    body
}

fn shape_claude_provider(body: &Value) -> Value {
    if body.get("available") != Some(&json!(true)) {
        return json!({
            "id": "claude", "label": "Claude", "available": false,
            "measured": false, "n_considered": 0,
            "cause": body.get("cause").cloned().unwrap_or(Value::Null),
            "reason": body.get("reason").cloned()
                .unwrap_or_else(|| json!("Claude usage is unavailable.")),
            "retry_at": body.get("retry_at"),
            "windows": [],
        });
    }
    let windows = body
        .get("limits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|limit| {
            let used = limit.get("percent")?.as_f64()?;
            let kind = limit.get("kind").and_then(Value::as_str).unwrap_or("limit");
            let model = limit
                .pointer("/scope/model/display_name")
                .and_then(Value::as_str);
            let label = if kind == "session" || kind == "worker" {
                "5-hour session".to_string()
            } else if let Some(model) = model {
                format!("{model} · weekly")
            } else if kind.starts_with("weekly")
                || limit.get("group").and_then(Value::as_str) == Some("weekly")
            {
                "Weekly · all models".to_string()
            } else {
                kind.replace('_', " ")
            };
            Some(json!({
                "label": label, "kind": kind,
                "group": limit.get("group").cloned().unwrap_or(Value::Null),
                "scope": limit.get("scope").cloned().unwrap_or(Value::Null),
                "used_percent": used, "remaining_percent": (100.0 - used).max(0.0),
                "resets_at": limit.get("resets_at").cloned().unwrap_or(Value::Null),
                "severity": limit.get("severity").cloned().unwrap_or(Value::Null),
                "active": limit.get("is_active").cloned().unwrap_or(Value::Null),
            }))
        })
        .collect::<Vec<_>>();
    json!({
        "id": "claude", "label": "Claude", "available": true, "measured": true,
        "n_considered": windows.len(), "metered": true,
        "source": "Anthropic subscription API", "windows": windows,
        "observed_at": body.get("observed_at"), "retry_at": body.get("retry_at"),
        "cache_age_s": body.get("cache_age_s"), "stale": body.get("stale"),
        "stale_reason": body.get("stale_reason"),
        "spend": body.get("spend").cloned().unwrap_or(Value::Null),
        "extra_usage": body.get("extra_usage").cloned().unwrap_or(Value::Null),
    })
}

fn shape_codex_provider(probe: ProviderProbe) -> Value {
    let body = match probe {
        ProviderProbe::Ok(body) => body,
        ProviderProbe::Unavailable { cause, reason } => {
            return json!({
                "id": "codex", "label": "Codex", "available": false,
                "measured": false, "n_considered": 0,
                "cause": cause, "reason": reason, "windows": [],
            })
        }
    };
    let fallback;
    let buckets = if let Some(map) = body
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
        .filter(|map| !map.is_empty())
    {
        map
    } else {
        fallback = serde_json::Map::from_iter([(
            "codex".to_string(),
            body.get("rateLimits").cloned().unwrap_or_else(|| json!({})),
        )]);
        &fallback
    };
    let mut windows = Vec::new();
    let mut details = Vec::new();
    for (bucket_id, snapshot) in buckets {
        let name = snapshot
            .get("limitName")
            .and_then(Value::as_str)
            .unwrap_or(bucket_id);
        for (position, key) in [("primary", "primary"), ("secondary", "secondary")] {
            let Some(window) = snapshot.get(key).and_then(Value::as_object) else {
                continue;
            };
            let Some(used) = window.get("usedPercent").and_then(Value::as_f64) else {
                continue;
            };
            let minutes = window.get("windowDurationMins").and_then(Value::as_i64);
            let duration = match minutes {
                Some(300) => "5-hour".to_string(),
                Some(10_080) => "7-day".to_string(),
                Some(mins) if mins % 1_440 == 0 => format!("{}-day", mins / 1_440),
                Some(mins) if mins % 60 == 0 => format!("{}-hour", mins / 60),
                Some(mins) => format!("{mins}-minute"),
                None => position.to_string(),
            };
            let label = if name == "codex" {
                duration
            } else {
                format!("{name} · {duration}")
            };
            windows.push(json!({
                "label": label, "kind": position, "limit_id": bucket_id,
                "limit_name": snapshot.get("limitName").cloned().unwrap_or(Value::Null),
                "used_percent": used, "remaining_percent": (100.0 - used).max(0.0),
                "window_minutes": minutes,
                "resets_at": window.get("resetsAt").cloned().unwrap_or(Value::Null),
            }));
        }
        details.push(json!({
            "id": bucket_id,
            "name": snapshot.get("limitName").cloned().unwrap_or(Value::Null),
            "plan_type": snapshot.get("planType").cloned().unwrap_or(Value::Null),
            "credits": snapshot.get("credits").cloned().unwrap_or(Value::Null),
            "individual_limit": snapshot.get("individualLimit").cloned().unwrap_or(Value::Null),
            "spend_control_reached": snapshot.get("spendControlReached").cloned().unwrap_or(Value::Null),
            "rate_limit_reached_type": snapshot.get("rateLimitReachedType").cloned().unwrap_or(Value::Null),
        }));
    }
    let plan = body
        .pointer("/rateLimits/planType")
        .cloned()
        .or_else(|| {
            details
                .iter()
                .find_map(|bucket| bucket.get("plan_type").cloned())
        })
        .unwrap_or(Value::Null);
    json!({
        "id": "codex", "label": "Codex", "available": true, "measured": true,
        "n_considered": windows.len(), "metered": true, "source": "Codex account API",
        "plan": plan, "windows": windows, "buckets": details,
        "reset_credits": body.get("rateLimitResetCredits").cloned().unwrap_or(Value::Null),
        "upsell": body.get("rateLimitUpsell").cloned().unwrap_or(Value::Null),
    })
}

fn shape_gemini_provider(probe: ProviderProbe) -> Value {
    let body = match probe {
        ProviderProbe::Ok(body) => body,
        ProviderProbe::Unavailable { cause, reason } => {
            return json!({
                "id": "gemini", "label": "Gemini", "available": false,
                "measured": false, "n_considered": 0,
                "cause": cause, "reason": reason, "windows": [],
            })
        }
    };
    let windows = body
        .pointer("/quota/buckets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|bucket| {
            let remaining = bucket.get("remainingFraction")?.as_f64()?.clamp(0.0, 1.0);
            let model = bucket
                .get("modelId")
                .and_then(Value::as_str)
                .unwrap_or("Model");
            Some(json!({
                "label": model, "kind": "model",
                "used_percent": (1.0 - remaining) * 100.0,
                "remaining_percent": remaining * 100.0,
                "remaining_amount": bucket.get("remainingAmount").cloned().unwrap_or(Value::Null),
                "resets_at": bucket.get("resetTime").cloned().unwrap_or(Value::Null),
            }))
        })
        .collect::<Vec<_>>();
    json!({
        "id": "gemini", "label": "Gemini", "available": true, "measured": true,
        "n_considered": windows.len(), "metered": true,
        "source": "Gemini Code Assist quota API",
        "auth_type": body.get("auth_type").cloned().unwrap_or(Value::Null),
        "plan": body.pointer("/tier/name").cloned().unwrap_or(Value::Null),
        "tier": body.get("tier").cloned().unwrap_or(Value::Null),
        "credits": body.get("credits").cloned().unwrap_or(Value::Null),
        "windows": windows,
    })
}

/// One probe outcome -> the wire body the SPA consumes.
///
/// Success is Anthropic's body VERBATIM plus `available: true` — Python's
/// exact behaviour (`data["available"] = True; return data`), which is what
/// keeps `limits[].kind` / `.percent` / `.resets_at` / `.scope` / `.group`
/// spelled the way `loadUsage()` reads them.
fn shape_probe(probe: UsageProbe) -> Value {
    match probe {
        UsageProbe::Snapshot {
            body,
            observed_at,
            retry_at,
            failure,
        } => {
            let mut shaped = shape_probe(UsageProbe::Ok(body));
            shaped["cache_managed"] = json!(true);
            shaped["observed_at"] = json!(observed_at);
            shaped["retry_at"] = json!(retry_at);
            if let Some(failure) = failure {
                shaped["stale"] = json!(true);
                shaped["stale_reason"] = shape_probe(*failure)["reason"].clone();
            }
            shaped
        }
        UsageProbe::Deferred { failure, retry_at } => {
            let mut shaped = shape_probe(*failure);
            // The credential-scoped cache owns fallback. The route's legacy
            // last-good envelope could belong to the previous login.
            shaped["cache_managed"] = json!(true);
            shaped["retry_at"] = json!(retry_at);
            shaped
        }
        UsageProbe::Ok(body) => match body {
            Value::Object(mut map) => {
                map.insert("available".into(), json!(true));
                Value::Object(map)
            }
            // 2xx whose body is not a JSON object: there is nowhere to put
            // `available`, and guessing a shape would be inventing one.
            // Python raised here and reported a failed fetch; this is that,
            // named precisely.
            _ => degraded(
                "unexpected_shape",
                "Usage endpoint returned an unexpected response shape".into(),
            ),
        },
        // Each arm is a different remedy, which is the whole point of the type.
        UsageProbe::NoToken => degraded(
            "no_token",
            "No Claude subscription token on this host".into(),
        ),
        UsageProbe::Expired => degraded(
            "expired_token",
            "Token expired. Run any Claude command to refresh it.".into(),
        ),
        UsageProbe::Http(401) | UsageProbe::Http(403) => degraded(
            "token_rejected",
            "Token rejected (401). Run any Claude command to refresh it.".into(),
        ),
        // Called out from the generic HTTP arm because it is the one failure
        // that is neither the user's fault nor persistent: it clears on its
        // own, and telling someone to re-login would be actively wrong.
        UsageProbe::Http(429) => degraded(
            "rate_limited",
            "Anthropic paused usage refreshes (HTTP 429). amux will retry automatically.".into(),
        ),
        UsageProbe::Http(code) => {
            degraded("probe_failed", format!("Usage fetch failed (HTTP {code})"))
        }
        UsageProbe::Transport(what) => degraded(
            "probe_failed",
            format!("Usage fetch failed (network: {what})"),
        ),
        UsageProbe::BadShape => degraded(
            "unexpected_shape",
            "Usage endpoint returned an unexpected response shape".into(),
        ),
    }
}

/// The degraded body. `reason` is the human sentence the SPA prints; `cause`
/// is the stable machine tag, so a future consumer can branch without
/// string-matching prose (and so a reason can be reworded without breaking
/// anything).
fn degraded(cause: &str, reason: String) -> Value {
    json!({ "available": false, "cause": cause, "reason": reason })
}

/// GET /api/usage/attribution — WHAT SPENT THE PLAN, in dollars, by why the
/// turn happened (AMUX-3544).
///
/// # The complaint this answers
///
/// A customer on the $20 plan, 2026-08-23: "I sent 2-3 prompts today and my
/// credits ran out ... going to investigate what's using all the credits."
/// They could not, and neither could we. `/api/usage` reports the plan's own
/// utilization and cannot attribute one point of it, so the only signal
/// available was the credits being gone. Ethos rule 4: a diagnosis being
/// impossible from the data we keep IS the bug.
///
/// Both halves already existed and nothing joined them. `token_ledger` has the
/// real cost per turn (from the Claude Code transcripts, priced per model) but
/// knows only WHICH SESSION spent it. `cmd_history` knows WHY each turn
/// happened — a human typed it, a schedule fired, a peer lane sent a message —
/// but nothing about cost. Attribution is the correlated lookup between them:
/// for each ledger row, the most recent prompt to that session at or before it.
///
/// Measured on this machine the day it was written, last 24h:
///
/// ```text
///   a peer lane messaged   $2413.83   5977 turns
///   you typed it           $1430.26   2471 turns
///   a schedule fired       $1028.29   2481 turns
///   -> 71% of spend is background
/// ```
///
/// # Why a separate endpoint rather than a field on /api/usage
///
/// The provider report and this local attribution ledger have independent
/// clocks and failure modes. Keeping attribution separate lets Settings still
/// show every provider limit if the token ledger is unreadable, and vice versa;
/// the legacy Anthropic fields at the top level remain byte-identical.
///
/// # The millisecond trap, stated because it already bit
///
/// `token_ledger.ts` is in SECONDS and `cmd_history.ts` is in MILLISECONDS.
/// Comparing them without the divide silently matches every row and returns a
/// clean, confident, wrong answer — it caught me on this exact query while
/// writing this, and ethos rule 7 records the same trap for
/// `interaction_log.ts`. The response therefore carries `rows_considered` and
/// `rows_excluded_by_window`, so a caller can confirm the window EXCLUDED
/// something rather than inferring correctness from plausible-looking output.
async fn get_attribution(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<AttributionQuery>,
) -> Response {
    let hours = q.hours.unwrap_or(24).clamp(1, 24 * 30);
    let store = state.store.clone();
    let out = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let conn = store.read()?;
        let cutoff: i64 = chrono::Utc::now().timestamp() - hours * 3600;

        // The control, not decoration: an unbounded match and a correct match
        // look identical from the rows alone.
        let total_rows: i64 =
            conn.query_row("SELECT COUNT(*) FROM token_ledger", [], |r| r.get(0))?;
        let in_window: i64 = conn.query_row(
            "SELECT COUNT(*) FROM token_ledger WHERE ts > ?1",
            [cutoff],
            |r| r.get(0),
        )?;

        let mut stmt = conn.prepare(
            &format!(
                "WITH lg AS (SELECT ts, session, cost_usd, input, output FROM token_ledger WHERE ts > ?1), {PROMPT_SOURCE_SRC} \
                 SELECT {PROMPT_SOURCE_TRIG} AS trig, SUM(cost_usd), COUNT(*), SUM(input), SUM(output) \
                 FROM src GROUP BY 1 ORDER BY 2 DESC"
            ),
        )?;
        let rows = stmt.query_map([cutoff], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;

        let mut sources = Vec::new();
        let (mut total_usd, mut bg_usd) = (0.0f64, 0.0f64);
        for row in rows {
            let (trig, usd, turns, inp, outp) = row?;
            total_usd += usd;
            let human = trig == "user";
            if !human {
                bg_usd += usd;
            }
            sources.push(json!({
                "source": if trig.is_empty() { "unattributed" } else { trig.as_str() },
                "label": trigger_label(&trig),
                "is_background": !human,
                "cost_usd": (usd * 100.0).round() / 100.0,
                "turns": turns,
                "input_tokens": inp,
                "output_tokens": outp,
            }));
        }

        // "A schedule fired" is actionable only when it names WHICH schedule,
        // and "a peer messaged" only when it names the lane. One schedule
        // accounted for 134 turns in 24h here and was invisible without this.
        let mut top = stmt_top(&conn, cutoff)?;
        top.truncate(10);

        Ok(json!({
            "window_hours": hours,
            "total_cost_usd": (total_usd * 100.0).round() / 100.0,
            "background_cost_usd": (bg_usd * 100.0).round() / 100.0,
            "background_pct": if total_usd > 0.0 {
                (bg_usd / total_usd * 1000.0).round() / 10.0
            } else { 0.0 },
            "by_source": sources,
            "top_origins": top,
            // Proof the window filtered. Equal counts mean the cutoff matched
            // everything and the numbers below are the whole table, not a window.
            "rows_considered": in_window,
            "rows_excluded_by_window": total_rows - in_window,
        }))
    })
    .await;

    match out {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// AMUX-4584: GET /api/usage/report.md — the token-usage dashboard as markdown.
//
// Deterministic by construction: SQL over token_ledger plus the same prompt
// attribution join /api/usage/attribution uses, rendered with format!. No model
// call, so an MDAI file that `fetch:`es this URL renders the same bytes for the
// same ledger (ethos rule 2: compute what you can compute).
//
// Every table states the population it was computed over and is sorted by cost,
// so "where is most of my token usage going" is answered by reading top-down.
// Known limits are printed in the report itself, not left to a comment, because
// the reader of a dashboard never reads the source.

#[derive(serde::Deserialize, Default)]
struct ReportQuery {
    days: Option<i64>,
    limit: Option<usize>,
}

fn md_money(v: f64) -> String {
    format!("${:.2}", v)
}

fn md_tokens(v: i64) -> String {
    let s = v.to_string();
    let bytes = s.as_bytes();
    let mut out = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

fn md_cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn pct(part: f64, total: f64) -> String {
    if total > 0.0 {
        format!("{:.1}%", part / total * 100.0)
    } else {
        "0.0%".to_string()
    }
}

/// One grouped breakdown: `(label, cost, tokens, turns)` rows, already sorted.
fn md_table(
    title: &str,
    key_header: &str,
    rows: &[(String, f64, i64, i64)],
    total_cost: f64,
) -> String {
    let mut s = format!("\n## {title}\n\n| {key_header} | Cost | Share | Tokens | Turns |\n|---|---:|---:|---:|---:|\n");
    if rows.is_empty() {
        s.push_str("| (none in window) | | | | |\n");
    }
    for (k, cost, tokens, turns) in rows {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            md_cell(if k.is_empty() { "(unattributed)" } else { k }),
            md_money(*cost),
            pct(*cost, total_cost),
            md_tokens(*tokens),
            turns
        ));
    }
    s
}

const REPORT_TOKENS: &str = "(input + cache_read + cache_write + output)";

fn grouped(
    conn: &rusqlite::Connection,
    select_key: &str,
    cutoff: i64,
    limit: usize,
) -> rusqlite::Result<Vec<(String, f64, i64, i64)>> {
    let sql = format!(
        "SELECT {select_key} AS k, COALESCE(SUM(cost_usd),0), COALESCE(SUM{REPORT_TOKENS},0), COUNT(*) \
         FROM token_ledger WHERE ts > ?1 GROUP BY k ORDER BY 2 DESC LIMIT ?2"
    );
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map(rusqlite::params![cutoff, limit as i64], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?.unwrap_or_default(),
            r.get::<_, f64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    rows.collect()
}

/// Render the report. Pure over a connection so a test can pin its shape.
pub(crate) fn render_usage_report(
    conn: &rusqlite::Connection,
    days: i64,
    limit: usize,
    now: i64,
) -> rusqlite::Result<String> {
    let cutoff = now - days * 86_400;
    let total_rows: i64 = conn.query_row("SELECT COUNT(*) FROM token_ledger", [], |r| r.get(0))?;
    let (total_cost, total_tokens, turns): (f64, i64, i64) = conn.query_row(
        &format!("SELECT COALESCE(SUM(cost_usd),0), COALESCE(SUM{REPORT_TOKENS},0), COUNT(*) FROM token_ledger WHERE ts > ?1"),
        [cutoff],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let fresh: Option<i64> =
        conn.query_row("SELECT MAX(ts) FROM token_ledger", [], |r| r.get(0))?;
    let (cache_read, input_side): (i64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(cache_read),0), COALESCE(SUM(input + cache_read + cache_write),0) FROM token_ledger WHERE ts > ?1",
        [cutoff],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    let mut md = String::new();
    md.push_str(&format!("# Token usage, last {days} day(s)\n\n"));
    md.push_str(&format!(
        "- **Total:** {} across {} API responses, {} tokens {REPORT_TOKENS}\n",
        md_money(total_cost),
        turns,
        md_tokens(total_tokens)
    ));
    md.push_str(&format!(
        "- **Cache hit:** {} of input-side tokens were cache reads\n",
        pct(cache_read as f64, input_side as f64)
    ));
    md.push_str(&format!(
        "- **Measured:** {} ledger rows in window of {} total; ledger fresh through {}\n",
        turns,
        total_rows,
        fresh
            .map(|t| chrono::DateTime::from_timestamp(t, 0)
                .map(|d| d.to_rfc3339())
                .unwrap_or_default())
            .unwrap_or_else(|| "never".into())
    ));
    md.push_str("- **Cost basis:** list price per model from runtime_jobs/token_ledger.rs (or ~/.amux/prices.json), not the plan invoice\n");

    md.push_str(&md_table(
        "By worker",
        "Worker",
        &grouped(conn, "session", cutoff, limit)?,
        total_cost,
    ));
    md.push_str(&md_table(
        "By model",
        "Model",
        &grouped(conn, "model", cutoff, limit)?,
        total_cost,
    ));
    md.push_str(&md_table(
        "Main conversation vs subagents",
        "Kind",
        &grouped(
            conn,
            "CASE WHEN conversation LIKE 'agent-%' THEN 'subagent' ELSE 'main' END",
            cutoff,
            limit,
        )?,
        total_cost,
    ));

    // Prompt source: the same definition /api/usage/attribution uses, now
    // including steering deliveries (AMUX-4582).
    let mut st = conn.prepare(
        &format!(
            "WITH lg AS (SELECT ts, session, cost_usd, input, cache_read, cache_write, output FROM token_ledger WHERE ts > ?1), {PROMPT_SOURCE_SRC} \
             SELECT {PROMPT_SOURCE_TRIG} AS trig, \
                    SUM(cost_usd), SUM(input + cache_read + cache_write + output), COUNT(*) \
             FROM src GROUP BY 1 ORDER BY 2 DESC"
        ),
    )?;
    let sources: Vec<(String, f64, i64, i64)> = st
        .query_map([cutoff], |r| {
            Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    md.push_str(&md_table(
        "By what triggered the turn",
        "Source (prompt or steering guard)",
        &sources,
        total_cost,
    ));

    // By card: attributed rows only; the stale task_windows problem is named.
    md.push_str(&md_table(
        "By board card",
        "Card",
        &grouped(conn, "NULLIF(task, '')", cutoff, limit)?,
        total_cost,
    ));

    md.push_str(&md_table(
        "By day (UTC)",
        "Day",
        &{
            let mut st = conn.prepare(&format!(
                "SELECT date(ts, 'unixepoch') AS d, SUM(cost_usd), SUM{REPORT_TOKENS}, COUNT(*) FROM token_ledger \
                 WHERE ts > ?1 GROUP BY d ORDER BY d DESC"
            ))?;
            let rows: Vec<(String, f64, i64, i64)> = st
                .query_map([cutoff], |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?;
            rows
        },
        total_cost,
    ));

    md.push_str(&md_table(
        "Top conversations",
        "Worker / conversation",
        &grouped(conn, "session || ' / ' || conversation", cutoff, limit)?,
        total_cost,
    ));

    md.push_str(
        "\n## Known limits of these numbers\n\n\
         - Subagent rows indexed before migration 70 can be billed more than once per API response (AMUX-4580); newer rows are keyed by message id.\n\
         - Card attribution reads `token_ledger.task`, filled from `task_windows`, which has had no writer since 2026-08-09 (AMUX-4581).\n\
         - Nudges, schedule fires and auto-compacts delivered through the steering queue are credited to the prompt before them (AMUX-4582).\n\
         - Codex and Gemini usage is not captured yet (AMUX-4583).\n",
    );
    Ok(md)
}

/// GET /api/usage/report.md?days=N&limit=M (AMUX-4584). `days` 1..365 (default 7),
/// `limit` rows per breakdown 1..200 (default 25). text/markdown.
async fn get_usage_report_md(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ReportQuery>,
) -> Response {
    let days = q.days.unwrap_or(7).clamp(1, 365);
    let limit = q.limit.unwrap_or(25).clamp(1, 200);
    let store = state.store.clone();
    let out = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let conn = store.read()?;
        Ok(render_usage_report(
            &conn,
            days,
            limit,
            chrono::Utc::now().timestamp(),
        )?)
    })
    .await;
    match out {
        Ok(Ok(md)) => (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/markdown; charset=utf-8",
            )],
            md,
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string(), "measured": false })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string(), "measured": false })),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize, Default)]
struct AttributionQuery {
    hours: Option<i64>,
}

/// Plain English, because the audience is a person wondering where their
/// credits went, not someone who knows what `cmd_history.type` is.
/// Which prompt a turn is credited to: the LATEST one the lane received at or
/// before it, from EITHER history table.
///
/// AMUX-4582. This used to read `cmd_history` alone. Steering deliveries write
/// only `steering_history` (board-drive nudges, board-progress, `sched:` fires,
/// the commit nudge, auto-compact, the browser reaper, the staged guard), so
/// every turn they started was credited to the prompt BEFORE them, which is
/// frequently the human's. That is the exact inversion this view exists to
/// prevent: it answers "did amux hand me this, or did I ask for it?".
///
/// Written once and interpolated into all three readers (by_source, top
/// origins, the markdown report) rather than restated in each: a view must
/// share the predicate of the mechanism it describes, and three copies of a
/// join is how two of them end up disagreeing.
///
/// # The cmd_history predicate is written for the index (AMUX-4710)
///
/// `h.ts` is milliseconds and `lg.ts` is seconds, so the comparison has to
/// convert. Doing it as `h.ts/1000 <= lg.ts` puts the computation on the
/// INDEXED side and no index on `h.ts` can serve it. `h.ts <= lg.ts*1000 + 999`
/// is the same predicate with the arithmetic moved to the constant side, and
/// the `+ 999` is not a fudge: integer division truncates, so `h.ts/1000 <= X`
/// admits every millisecond of second X, and dropping it would silently stop
/// crediting a prompt that landed later in the same second as the turn.
///
/// The predicate is the smaller half. Measured on a copy of the live DB,
/// hours=24: as shipped 3.41 s, predicate alone 3.31 s, migration 0076's
/// `cmd_history(session, ts)` index alone 0.09 s, both 0.05 s. Identical rows
/// in all four. The index is what does the work; this form lets it seek the
/// range rather than walk it.
const PROMPT_SOURCE_SRC: &str = "\
 src AS (SELECT lg.*, \
   (SELECT h.ts/1000 FROM cmd_history h \
      WHERE h.session = lg.session AND h.ts <= lg.ts * 1000 + 999 ORDER BY h.ts DESC LIMIT 1) AS c_ts, \
   (SELECT COALESCE(h.type,'') FROM cmd_history h \
      WHERE h.session = lg.session AND h.ts <= lg.ts * 1000 + 999 ORDER BY h.ts DESC LIMIT 1) AS c_type, \
   (SELECT COALESCE(h.origin,'') FROM cmd_history h \
      WHERE h.session = lg.session AND h.ts <= lg.ts * 1000 + 999 ORDER BY h.ts DESC LIMIT 1) AS c_origin, \
   (SELECT s.delivered_at FROM steering_history s \
      WHERE s.session = lg.session AND s.delivered_at <= lg.ts ORDER BY s.delivered_at DESC LIMIT 1) AS s_ts, \
   (SELECT COALESCE(NULLIF(s.guard,''),'steering') FROM steering_history s \
      WHERE s.session = lg.session AND s.delivered_at <= lg.ts ORDER BY s.delivered_at DESC LIMIT 1) AS s_guard, \
   (SELECT COALESCE(s.sender,'') FROM steering_history s \
      WHERE s.session = lg.session AND s.delivered_at <= lg.ts ORDER BY s.delivered_at DESC LIMIT 1) AS s_sender \
   FROM lg)";

/// The source key. A steering guard becomes `steer:<family>`, the part before
/// its first colon, so `sched:SCHED-456` and `task-callback:TUBES-2790` group
/// as `steer:sched` and `steer:task-callback` instead of one bucket per id.
const PROMPT_SOURCE_TRIG: &str = "CASE WHEN src.s_ts IS NOT NULL AND (src.c_ts IS NULL OR src.s_ts > src.c_ts) \
   THEN 'steer:' || CASE WHEN instr(src.s_guard, ':') > 0 \
                         THEN substr(src.s_guard, 1, instr(src.s_guard, ':') - 1) ELSE src.s_guard END \
   ELSE COALESCE(src.c_type, '') END";

/// Who or what sent it, for the named-offenders list: the steering sender when
/// steering won, the cmd_history origin otherwise.
const PROMPT_SOURCE_ORIGIN: &str =
    "CASE WHEN src.s_ts IS NOT NULL AND (src.c_ts IS NULL OR src.s_ts > src.c_ts) \
   THEN src.s_sender ELSE COALESCE(src.c_origin, '') END";

fn trigger_label(t: &str) -> &'static str {
    match t {
        "user" => "you typed it",
        "schedule" => "a schedule fired",
        "session" => "a peer lane messaged",
        // AMUX-3547. Its own cell because "did amux hand me this, or did I ask
        // for it?" is the question this whole view exists to answer, and until
        // board_drive::record_prompt shipped, pickup turns had no row and were
        // credited to whatever prompt preceded them — including the human's.
        "pickup" => "amux handed this lane a board card",
        // Live on this box: 573 turns and $320 in a day reading as "other"
        // because only the steering spelling had a label (AMUX-4582).
        "task-callback" => "a task callback from a peer",
        "system" => "an amux nudge",
        "direct" | "steering" => "a steering message",
        "" => "no prompt matched; the turn predates this lane's history",
        // AMUX-4582: steering deliveries, named by what sent them. Each of
        // these was previously credited to the prompt before it.
        "steer:board-drive" => "amux nudged this lane about a card",
        "steer:board-progress" => "a board progress note",
        "steer:sched" => "a schedule fired",
        "steer:commit-nudge" => "the commit nudge",
        "steer:auto-compact" | "steer:compact" => "an auto-compact",
        "steer:task-callback" => "a task callback from a peer",
        "steer:browser-reaper" => "the browser reaper",
        "steer:staged-guard" => "the staged guard",
        "steer:deferred-automation" => "deferred automation",
        "steer:steering" => "a steering message with no guard",
        other if other.starts_with("steer:") => "an amux nudge",
        _ => "other",
    }
}

/// The named offenders inside the background bucket.
fn stmt_top(conn: &rusqlite::Connection, cutoff: i64) -> anyhow::Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        &format!(
            "WITH lg AS (SELECT ts, session, cost_usd FROM token_ledger WHERE ts > ?1), {PROMPT_SOURCE_SRC} \
             SELECT {PROMPT_SOURCE_TRIG}, {PROMPT_SOURCE_ORIGIN}, src.session, SUM(cost_usd), COUNT(*) \
             FROM src GROUP BY 1,2,3 ORDER BY 4 DESC LIMIT 10"
        ),
    )?;
    let rows = stmt.query_map([cutoff], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, f64>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (trig, origin, session, usd, turns) = row?;
        out.push(json!({
            "source": if trig.is_empty() { "unattributed" } else { trig.as_str() },
            "label": trigger_label(&trig),
            // `is_background` SHIPS HERE TOO (AMUX-3550). The client filtered
            // these rows on it and it existed only on `by_source`, so the
            // predicate `x.is_background !== false` kept 10 of 10 — a filter
            // that reads as a deliberate exclusion and excludes nothing. Three
            // of those ten were the human's own typing, listed under "biggest
            // background sources" on the panel built to tell a customer what
            // spent their credits BESIDES them. Fixed by making the field the
            // client already reaches for exist, rather than by teaching the
            // client a second spelling of it.
            "is_background": trig != "user",
            "origin": origin,
            "session": session,
            "cost_usd": (usd * 100.0).round() / 100.0,
            "turns": turns,
        }));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    /// AMUX-3545: the reserve decision, and the property that matters most is
    /// that it FAILS OPEN.
    ///
    /// Ethan's call, 2026-08-25: 30%, so background pauses at 70% window use.
    /// The measurement behind the question, from the 5h window that day: 2,270
    /// turns, 82.1% background — 1,059 peer-message, 552 schedule, 50 pickup,
    /// against 505 of his own.
    #[test]
    fn the_reserve_pauses_at_the_threshold_and_fails_open_on_an_unknown() {
        // Ethan's 30: pause at 70 and above, run below it.
        assert!(!super::background_should_pause(Some(69), 30));
        assert!(
            super::background_should_pause(Some(70), 30),
            "the boundary is inclusive"
        );
        assert!(super::background_should_pause(Some(99), 30));

        // THE CELL THAT MATTERS. An unknown reading must NOT pause. The usage
        // probe is a network call to a third party; if a rate limit or an
        // expired token made `None` mean "pause", every schedule and every
        // pickup across the fleet would stop, and the outage would be blamed on
        // anything but the probe. Under-protecting for one window is
        // recoverable; that is not.
        assert!(
            !super::background_should_pause(None, 30),
            "an unknown window must never pause background work"
        );

        // 0 is the honest off switch, and it must beat even a maxed window —
        // otherwise "disabled" would still gate at 100%.
        assert!(!super::background_should_pause(Some(100), 0));
        assert!(!super::background_should_pause(None, 0));

        // A larger reserve bites earlier; a smaller one later. Pinned so the
        // arithmetic cannot invert without a red test — the direction is the
        // whole meaning of the number.
        assert!(
            super::background_should_pause(Some(51), 50),
            "50% reserve pauses at 50"
        );
        assert!(
            !super::background_should_pause(Some(51), 20),
            "20% reserve does not"
        );
    }

    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    /// The service manager's PATH may contain a stale but executable shim.
    /// Keep the probe on the same login-shell resolution path as a real worker.
    #[test]
    fn codex_usage_probe_resolves_the_users_login_shell_binary() {
        let command = codex_probe_process("/bin/example-shell");
        let command = command.as_std();
        assert_eq!(command.get_program(), "/bin/example-shell");
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            [
                "-lc",
                "exec codex app-server --stdio --disable remote_control"
            ]
        );
    }

    /// AMUX-3544. Spend is attributed to WHY the turn happened, and the window
    /// proves it filtered.
    ///
    /// The customer complaint this endpoint exists for was "I sent 2-3 prompts
    /// and my credits ran out", with no way to see what spent them. So the
    /// assertions are the two things a person in that position needs: which
    /// bucket the money is in, and whether the number they are reading covers
    /// the window they think it does.
    ///
    /// THE UNIT MISMATCH IS THE POINT OF THE THIRD ASSERTION. `token_ledger.ts`
    /// is in SECONDS and `cmd_history.ts` is in MILLISECONDS. A join that
    /// forgets the divide matches every prompt and still returns a tidy-looking
    /// answer — it did exactly that to me while I was writing this query. The
    /// fixture puts an OLD ledger row outside the window, so
    /// `rows_excluded_by_window` must be non-zero: an unbounded match and a
    /// correct one are indistinguishable from the totals alone.
    #[tokio::test]
    async fn spend_is_attributed_to_what_triggered_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("attr.db")).unwrap();
        let now = chrono::Utc::now().timestamp();
        store
            .write(move |conn| {
            // Three prompts into one lane: a human, a schedule, a peer.
                for (t, origin, at) in [
                    ("user", "", now - 300),
                    // AMUX-3547: a pickup lands right after the human's prompt.
                    // Before board_drive::record_prompt existed this row was
                    // never written, so the turn below was matched to the
                    // HUMAN's row and counted as foreground.
                    ("pickup", "board-drive", now - 250),
                    ("schedule", "poll the inbox", now - 200),
                    ("session", "peer-lane", now - 100),
                ] {
                    conn.execute(
                        "INSERT INTO cmd_history (text, type, session, ts, origin) VALUES (?,?,?,?,?)",
                        rusqlite::params!["p", t, "lane", at * 1000, origin],
                    )?;
                }
                // A turn after each prompt, plus one far outside the window.
                for (at, cost) in [
                    (now - 290, 1.0),
                    (now - 240, 7.0), // the pickup's turn
                    (now - 190, 5.0),
                    (now - 90, 20.0),
                    (now - 86_400 * 30, 999.0),
                ] {
                    conn.execute(
                        "INSERT INTO token_ledger (ts, session, conversation, model, input, cache_read, \
                         cache_write, output, cost_usd, task) VALUES (?,?,?,?,?,?,?,?,?,?)",
                        rusqlite::params![at, "lane", "c", "opus", 10, 0, 0, 5, cost, ""],
                    )?;
                }
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let state = AppState {
            store: Arc::new(store),
            started: Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let app = axum::Router::new()
            .nest(
                "/api/usage",
                routes_with(probe_fn(
                    UsageProbe::Ok(json!({})),
                    Arc::new(AtomicUsize::new(0)),
                )),
            )
            .with_state(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/usage/attribution?hours=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();

        // 1. The money is in the right buckets, and the labels are for a human.
        let by: std::collections::HashMap<String, f64> = v["by_source"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["source"].as_str().unwrap().to_string(),
                    r["cost_usd"].as_f64().unwrap(),
                )
            })
            .collect();
        assert_eq!(by.get("user"), Some(&1.0), "the human's turn: {v}");
        assert_eq!(by.get("schedule"), Some(&5.0), "the schedule's turn: {v}");
        assert_eq!(by.get("session"), Some(&20.0), "the peer's turn: {v}");

        // AMUX-3547. A PICKUP IS ITS OWN BUCKET AND IT IS BACKGROUND.
        //
        // Measured on the live fleet before the fix: 247 auto-pickup deliveries
        // in `steering_history` over 24h and TWO in `cmd_history`. Pickup went
        // through `steer_enqueue`, which writes the steering tables; only the
        // send path wrote `cmd_history`. So a pickup's turns were matched to
        // whatever prompt preceded them — here the human's — and counted as
        // FOREGROUND.
        //
        // The bias ran toward under-reporting background, which is the one
        // direction that matters: AMUX-3542 is a customer whose plan window was
        // eaten by background work, and the instrument built to prove it was
        // crediting some of that work to their own typing.
        assert_eq!(
            by.get("pickup"),
            Some(&7.0),
            "the pickup's turn is its own bucket: {v}"
        );
        let pickup_row = v["by_source"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["source"] == "pickup")
            .expect("pickup row");
        assert_eq!(
            pickup_row["is_background"],
            json!(true),
            "amux handing a lane a card is not the human typing: {pickup_row}"
        );
        assert!(
            pickup_row["label"]
                .as_str()
                .unwrap()
                .contains("amux handed"),
            "the label is read by someone wondering where their credits went: {pickup_row}"
        );
        assert_eq!(
            by.get("user"),
            Some(&1.0),
            "AND THE HUMAN'S BUCKET MUST NOT HAVE ABSORBED IT — 8.0 here is the pre-fix \
             behaviour, and it is the whole defect: {v}"
        );

        // 2. Background share is the headline the complaint was about.
        assert_eq!(v["total_cost_usd"], json!(33.0));
        assert_eq!(v["background_cost_usd"], json!(32.0));
        assert_eq!(v["background_pct"], json!(97.0));

        // 3. THE CONTROL. The 30-day-old row must be OUTSIDE a 1-hour window.
        //    If this is 0 the join matched everything and every number above is
        //    the whole table wearing a window's label.
        assert_eq!(v["rows_considered"], json!(4));
        assert!(
            v["rows_excluded_by_window"].as_i64().unwrap() >= 1,
            "the window excluded nothing — an unbounded match returns a confident wrong \
             answer and looks exactly like this one: {v}"
        );

        // 4. Named offenders, or "a schedule fired" is not actionable.
        let top = v["top_origins"].as_array().unwrap();
        assert!(
            top.iter().any(|r| r["origin"] == "poll the inbox"),
            "the expensive schedule must be NAMED: {v}"
        );

        // 5. EVERY top_origins row carries `is_background`, and it is correct.
        //
        //    AMUX-3550: the client filtered these rows on this field and the
        //    field existed only on `by_source`, so `x.is_background !== false`
        //    kept 10 of 10 — a filter that reads as a deliberate exclusion and
        //    excludes nothing. The panel headed "biggest background sources"
        //    then listed the human's own typing, on the very screen built to
        //    tell a customer what spent their credits BESIDES them.
        //
        //    Asserting PRESENCE is the load-bearing half. A test that only
        //    checked the values of rows that happen to have the key would pass
        //    against the version where no row has it at all.
        for r in top {
            assert!(
                r.get("is_background")
                    .map(|b| b.is_boolean())
                    .unwrap_or(false),
                "every top_origins row must carry a boolean is_background — its ABSENCE is \
                 what made the client's filter match everything: {r}"
            );
        }
        assert!(
            top.iter()
                .any(|r| r["source"] == "user" && r["is_background"] == json!(false)),
            "the human's own row must be present AND flagged not-background, so a consumer \
             can exclude it: {v}"
        );
        assert!(
            top.iter()
                .any(|r| r["source"] == "schedule" && r["is_background"] == json!(true)),
            "a schedule's row must be flagged background: {v}"
        );
    }

    /// THE `+ 999` IN THE cmd_history PREDICATE IS LOAD-BEARING (AMUX-4710).
    ///
    /// The join was written `h.ts/1000 <= lg.ts`, which puts the arithmetic on
    /// the indexed column and no index can serve it. Moving it to the constant
    /// side is what lets `idx_cmd_history_session_ts` seek, and the naive move
    /// is `h.ts <= lg.ts * 1000`. That is a DIFFERENT predicate: integer
    /// division truncates, so the original admits every millisecond of the
    /// ledger row's own second, and the naive form admits only millisecond
    /// zero.
    ///
    /// The fixture is that second. A prompt at `now-60` with 500 ms, a turn at
    /// `now-60` exactly. Under the original and under `+ 999` the turn is the
    /// peer's; under `lg.ts * 1000` the peer's prompt is invisible and the turn
    /// falls back to the human's, which is the same foreground/background
    /// inversion AMUX-4582 fixed one layer up. A sub-second gap between a
    /// delivery and the turn it started is the ORDINARY case here, not an edge.
    ///
    /// A STEERING ROW SITS BETWEEN THEM ON PURPOSE, so that BOTH cmd_history
    /// subqueries this predicate appears in are load-bearing. `c_type` supplies
    /// the answer and `c_ts` decides whether cmd_history beats steering at all;
    /// with steering empty, a mutation of `c_ts` alone would leave the test
    /// green and the coverage would be for one of two copies.
    #[tokio::test]
    async fn a_prompt_later_in_the_same_second_as_the_turn_still_credits_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("attr-ms.db")).unwrap();
        let now = chrono::Utc::now().timestamp();
        store
            .write(move |conn| {
                // The human, a clear second earlier.
                conn.execute(
                    "INSERT INTO cmd_history (text, type, session, ts, origin) VALUES (?,?,?,?,?)",
                    rusqlite::params!["p", "user", "lane", (now - 120) * 1000, ""],
                )?;
                // A steering delivery in between: newer than the human, older
                // than the peer. It wins only if the peer's row is dropped.
                conn.execute(
                    "INSERT INTO steering_history (id, session, text, delivered_at, guard, sender) \
                     VALUES (?,?,?,?,?,?)",
                    rusqlite::params!["s1", "lane", "nudge", (now - 90) as f64, "", "amux"],
                )?;
                // The peer's message, in the SAME second as the turn, 500 ms in.
                conn.execute(
                    "INSERT INTO cmd_history (text, type, session, ts, origin) VALUES (?,?,?,?,?)",
                    rusqlite::params!["p", "session", "lane", (now - 60) * 1000 + 500, "peer-lane"],
                )?;
                conn.execute(
                    "INSERT INTO token_ledger (ts, session, conversation, model, input, cache_read, \
                     cache_write, output, cost_usd, task) VALUES (?,?,?,?,?,?,?,?,?,?)",
                    rusqlite::params![now - 60, "lane", "c", "opus", 10, 0, 0, 5, 4.0, ""],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let state = AppState {
            store: Arc::new(store),
            started: Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let app = axum::Router::new()
            .nest(
                "/api/usage",
                routes_with(probe_fn(
                    UsageProbe::Ok(json!({})),
                    Arc::new(AtomicUsize::new(0)),
                )),
            )
            .with_state(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/usage/attribution?hours=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let by: std::collections::HashMap<String, f64> = v["by_source"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["source"].as_str().unwrap_or("").to_string(),
                    r["cost_usd"].as_f64().unwrap_or(0.0),
                )
            })
            .collect();
        assert_eq!(
            by.get("session"),
            Some(&4.0),
            "the prompt 500 ms into the turn's own second must still win: {v}"
        );
        assert_eq!(
            by.get("user"),
            None,
            "4.0 here is the truncation bug — the human absorbed a peer-triggered turn: {v}"
        );
        assert_eq!(
            by.get("steer:steering"),
            None,
            "and 4.0 HERE is the same bug seen through c_ts: with the peer's row dropped, the \
             steering delivery at now-90 becomes the newest thing this lane received: {v}"
        );
        assert_eq!(v["background_pct"], json!(100.0), "{v}");
    }

    /// A fixture probe that counts how many times it was called.
    fn probe_fn(outcome: UsageProbe, calls: Arc<AtomicUsize>) -> ProbeFn {
        Arc::new(move || {
            let outcome = outcome.clone();
            let calls = calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                outcome
            })
        })
    }

    fn app(probe: ProbeFn) -> axum::Router {
        app_routes(routes_with(probe))
    }
    fn app_routes(routes: Router<AppState>) -> axum::Router {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("usage-test.db")).unwrap();
        std::mem::forget(dir);
        let state = AppState {
            store: Arc::new(store),
            started: Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        Router::new().nest("/api/usage", routes).with_state(state)
    }

    async fn get(app: &axum::Router) -> (StatusCode, Value) {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/usage")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// The real endpoint's success body, built from the live response SHAPE
    /// (both the top-level windows and the `limits[]` array Anthropic sends
    /// together). Numbers are invented; no credential is involved.
    fn live_shaped_body() -> Value {
        json!({
            "five_hour": {"utilization": 34.4, "resets_at": "2026-08-09T18:00:00+00:00"},
            "seven_day": {"utilization": 71.6, "resets_at": "2026-08-12T00:00:00+00:00"},
            "seven_day_opus": null,
            "limits": [
                {"kind": "session", "percent": 34.4, "resets_at": "2026-08-09T18:00:00Z"},
                {"kind": "weekly_all", "group": "weekly", "percent": 71.6,
                 "resets_at": "2026-08-12T00:00:00Z"},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 12.0,
                 "scope": {"model": {"display_name": "Opus"}},
                 "resets_at": "2026-08-12T00:00:00Z"}
            ]
        })
    }

    /// AMUX-4154: "all provider usage" is a totalizing claim. Compare the
    /// response against the production registry, not a second hand-written
    /// list, so provider five makes this fail until Settings covers it too.
    #[test]
    fn settings_usage_covers_every_default_provider_with_full_windows() {
        let codex = json!({
            "accountId": "must-never-reach-settings",
            "rateLimits": {"planType": "pro"},
            "rateLimitsByLimitId": {
                "codex": {
                    "limitName": "codex", "planType": "pro",
                    "primary": {"usedPercent": 45, "windowDurationMins": 300, "resetsAt": 1788652800},
                    "secondary": {"usedPercent": 61, "windowDurationMins": 10080, "resetsAt": 1789084800},
                    "credits": {"hasCredits": true, "unlimited": false, "balance": "12.50"}
                },
                "codex_bengalfox": {
                    "limitName": "Spark", "planType": "pro",
                    "primary": {"usedPercent": 3, "windowDurationMins": 300, "resetsAt": 1788656400},
                    "secondary": {"usedPercent": 7, "windowDurationMins": 10080, "resetsAt": 1789088400}
                }
            },
            "rateLimitResetCredits": {"availableCount": 2, "credits": [{"expiresAt": 1789257600}]},
            "rateLimitUpsell": {"eligible": false}
        });
        let gemini = json!({
            "available": true, "auth_type": "oauth-personal",
            "tier": {"id": "standard", "name": "Google AI Pro"}, "credits": 1000,
            "quota": {"buckets": [
                {"modelId": "gemini-3.5-pro", "remainingFraction": 0.72,
                 "remainingAmount": 144, "resetTime": "2026-09-06T20:00:00Z"},
                {"modelId": "gemini-3.5-flash", "remainingFraction": 0.91,
                 "remainingAmount": 910, "resetTime": "2026-09-06T20:00:00Z"}
            ]}
        });
        let body = shape_all_providers(
            UsageProbe::Ok(live_shaped_body()),
            ProviderProbe::Ok(codex),
            ProviderProbe::Ok(gemini),
        );
        let providers = body["providers"].as_array().expect("provider rows");
        let mut response_ids = providers
            .iter()
            .map(|provider| provider["id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        response_ids.sort();
        let mut registry_ids = crate::provider::default_registry()
            .ids()
            .into_iter()
            .map(|id| match id.as_str() {
                "claude-code" => "claude".to_string(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>();
        registry_ids.sort();
        assert_eq!(
            response_ids, registry_ids,
            "every shipped provider needs a Settings row"
        );
        assert_eq!(body["provider_count"], json!(providers.len()));

        let provider = |id: &str| {
            providers
                .iter()
                .find(|provider| provider["id"] == id)
                .unwrap_or_else(|| panic!("missing {id}: {body}"))
        };
        let codex = provider("codex");
        assert_eq!(codex["windows"].as_array().unwrap().len(), 4);
        assert!(codex["windows"].as_array().unwrap().iter().any(|window| {
            window["label"] == "Spark · 7-day"
                && window["remaining_percent"].as_f64() == Some(93.0)
                && window["resets_at"].as_i64() == Some(1789088400i64)
        }));
        assert_eq!(codex["reset_credits"]["availableCount"], 2);
        assert_eq!(codex["buckets"][0]["credits"]["balance"], "12.50");
        let gemini = provider("gemini");
        assert_eq!(gemini["plan"], "Google AI Pro");
        assert_eq!(gemini["windows"].as_array().unwrap().len(), 2);
        assert_eq!(gemini["windows"][0]["remaining_amount"], 144);
        assert_eq!(provider("ollama")["metered"], false);
        assert_eq!(body["n_considered"], 9);
        let wire = serde_json::to_string(&body).unwrap();
        assert!(
            !wire.contains("must-never-reach-settings"),
            "account identity leaked: {wire}"
        );
        assert!(
            !wire.contains("accountId"),
            "account identity field leaked: {wire}"
        );
    }

    #[test]
    fn one_unavailable_probe_does_not_hide_the_other_provider_rows() {
        let body = shape_all_providers(
            UsageProbe::NoToken,
            ProviderProbe::Unavailable {
                cause: "cli_missing",
                reason: "Codex CLI is not installed on this host.".into(),
            },
            ProviderProbe::Unavailable {
                cause: "account_quota_not_reported",
                reason: "This Gemini authentication mode has no account-wide quota.".into(),
            },
        );
        let providers = body["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 5);
        assert_eq!(
            providers.iter().filter(|p| p["available"] == false).count(),
            3
        );
        assert_eq!(
            providers.iter().find(|p| p["id"] == "ollama").unwrap()["available"],
            true,
            "local usage remains truthful when every subscription probe is unavailable"
        );
        // Muse has no probe to fail: it exposes no usage API at all, so a dead
        // Codex/Gemini probe cannot make it unavailable. "Available with unknown
        // usage" and "unavailable" are different states and the row must not
        // collapse them — unavailable would read as "muse is broken".
        let muse = providers.iter().find(|p| p["id"] == "muse").unwrap();
        assert_eq!(muse["available"], true);
        assert_eq!(muse["usage_unknown"], true);
        assert_eq!(muse["measured"], false, "nothing was measured, so say so");
    }

    #[tokio::test]
    async fn success_passes_anthropics_body_through_verbatim() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(probe_fn(UsageProbe::Ok(live_shaped_body()), calls.clone()));
        let (st, v) = get(&app).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["available"], json!(true));
        assert!(v.get("reason").is_none());

        // Every field loadUsage() reads must survive, spelled exactly as
        // Anthropic sent it — this is the regression that made the meter
        // useless when the endpoint normalized through UsageWindow.
        let limits = v["limits"].as_array().unwrap();
        assert_eq!(limits.len(), 3);
        assert_eq!(limits[0]["kind"], json!("session"));
        assert_eq!(limits[0]["percent"], json!(34.4)); // not rounded away
        assert_eq!(limits[0]["resets_at"], json!("2026-08-09T18:00:00Z"));
        assert_eq!(limits[1]["group"], json!("weekly"));
        // The per-model row the SPA labels "<model> · weekly": normalized
        // windows discard this entirely.
        assert_eq!(limits[2]["scope"]["model"]["display_name"], json!("Opus"));
        // Top-level windows pass through untouched too.
        assert_eq!(v["five_hour"]["utilization"], json!(34.4));
    }

    #[tokio::test]
    async fn each_failure_cause_has_its_own_reason() {
        // The rule this enforces: no two causes may share a reason string,
        // and none may be the old catch-all. A test that only checked
        // `available == false` would have passed against the collapsed
        // message that motivated this work.
        let cases = vec![
            (UsageProbe::NoToken, "no_token"),
            (UsageProbe::Expired, "expired_token"),
            (UsageProbe::Http(401), "token_rejected"),
            (UsageProbe::Http(429), "rate_limited"),
            (UsageProbe::Http(500), "probe_failed"),
            (UsageProbe::Transport("timeout"), "probe_failed"),
            (UsageProbe::BadShape, "unexpected_shape"),
            (UsageProbe::Ok(json!([1, 2, 3])), "unexpected_shape"),
        ];
        let mut seen_reasons: Vec<String> = Vec::new();
        for (outcome, expect_cause) in cases {
            let app = app(probe_fn(outcome.clone(), Arc::new(AtomicUsize::new(0))));
            let (st, v) = get(&app).await;
            assert_eq!(st, StatusCode::OK, "{outcome:?}");
            assert_eq!(v["available"], json!(false), "{outcome:?}");
            assert_eq!(v["cause"], json!(expect_cause), "{outcome:?}");
            let reason = v["reason"].as_str().unwrap().to_string();
            assert!(!reason.is_empty());
            // Never the collapsed sentence again.
            assert!(
                !reason.contains("no token, expired token, or probe failed"),
                "{outcome:?} still serves the catch-all"
            );
            // No numbers invented on a degraded path.
            assert!(v.get("limits").is_none(), "{outcome:?}");
            seen_reasons.push(reason);
        }
        // Distinctness where the cause differs: 500 and timeout share a
        // cause tag but must still read differently (status vs network).
        let mut uniq = seen_reasons.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(
            uniq.len(),
            seen_reasons.len() - 1, // BadShape and non-object Ok share one
            "reasons collapsed: {seen_reasons:?}"
        );
    }

    #[tokio::test]
    async fn http_failures_carry_their_status_code() {
        for code in [500u16, 502, 429] {
            let app = app(probe_fn(
                UsageProbe::Http(code),
                Arc::new(AtomicUsize::new(0)),
            ));
            let (_, v) = get(&app).await;
            assert!(
                v["reason"].as_str().unwrap().contains(&code.to_string()),
                "HTTP {code} reason omits the status: {}",
                v["reason"]
            );
        }
    }

    #[tokio::test]
    async fn no_response_on_any_path_can_contain_a_token() {
        // Assemble a secret-shaped string at runtime so the repo's scanner
        // never sees one, and prove it cannot reach the wire even when the
        // upstream body echoes it.
        let fake = format!("sk-ant-{}-{}", "oat01", "AAAAdeadbeefdeadbeef");
        let outcomes = vec![
            UsageProbe::NoToken,
            UsageProbe::Expired,
            UsageProbe::Http(401),
            UsageProbe::Http(429),
            UsageProbe::Transport("connect"),
            UsageProbe::BadShape,
        ];
        for outcome in outcomes {
            let app = app(probe_fn(outcome.clone(), Arc::new(AtomicUsize::new(0))));
            let (_, v) = get(&app).await;
            let text = serde_json::to_string(&v).unwrap();
            assert!(!text.contains(&fake), "{outcome:?}");
            assert!(!text.contains("Bearer"), "{outcome:?}");
            assert!(!text.contains("sk-ant"), "{outcome:?}");
        }
        // The one path that echoes upstream content is 2xx. The type system
        // is what guarantees the rest: no failure variant can hold a token.
    }

    #[tokio::test]
    async fn cache_serves_repeat_opens_from_one_probe_and_reports_age() {
        // Reads AMUX_USAGE_TTL_S; five sibling tests set it to "0" (AMUX-4963).
        let _env = default_env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(probe_fn(UsageProbe::Ok(live_shaped_body()), calls.clone()));
        let (_, first) = get(&app).await;
        let (_, second) = get(&app).await;
        let (_, third) = get(&app).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "settings reopens must not hammer Anthropic"
        );
        assert_eq!(first["available"], json!(true));
        assert_eq!(second["limits"], first["limits"]);
        assert_eq!(third["limits"], first["limits"]);
        // Age is present on every response and TTL is advertised.
        for r in [&first, &second, &third] {
            assert!(r["cache_age_s"].is_number(), "{r}");
            assert_eq!(r["cache_ttl_s"], json!(DEFAULT_USAGE_TTL_S));
        }
    }

    #[tokio::test]
    async fn failures_are_cached_too_so_a_rate_limit_is_not_amplified() {
        // Retrying a 429 on every render is what provokes the 429.
        // Same unlocked read as its neighbour above; it had simply not lost
        // the race yet, which is not a difference worth waiting on (AMUX-4963).
        let _env = default_env().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(probe_fn(UsageProbe::Http(429), calls.clone()));
        for _ in 0..5 {
            let (_, v) = get(&app).await;
            assert_eq!(v["cause"], json!("rate_limited"));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// A probe whose outcome changes between calls — the intermittent 429
    /// that actually happens on this host, which a fixed fixture cannot show.
    fn probe_sequence(outcomes: Vec<UsageProbe>) -> (ProbeFn, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let f: ProbeFn = Arc::new(move || {
            let outcomes = outcomes.clone();
            let c = c.clone();
            Box::pin(async move {
                let i = c.fetch_add(1, Ordering::SeqCst);
                outcomes
                    .get(i)
                    .cloned()
                    .unwrap_or(outcomes.last().cloned().unwrap())
            })
        });
        (f, calls)
    }

    #[tokio::test]
    async fn a_transient_failure_serves_the_last_good_reading_marked_stale() {
        // Success, then 429 — the sequence observed live on 2026-08-09.
        let (probe, _calls) = probe_sequence(vec![
            UsageProbe::Ok(live_shaped_body()),
            UsageProbe::Http(429),
        ]);
        let app = app(probe);
        temp_env_ttl("0", || async {
            let (_, first) = get(&app).await;
            assert_eq!(first["available"], json!(true));
            assert!(first.get("stale").is_none(), "a live reading is not stale");

            let (_, second) = get(&app).await;
            // The meter keeps working across the blip...
            assert_eq!(second["available"], json!(true));
            assert_eq!(second["limits"], first["limits"]);
            // ...and says so, with the live failure attached.
            assert_eq!(second["stale"], json!(true));
            assert!(second["stale_reason"].as_str().unwrap().contains("429"));
        })
        .await;
    }

    #[test]
    fn dated_snapshot_reaches_the_provider_row_and_cold_retry_has_no_numbers() {
        let body = shape_probe(UsageProbe::Snapshot {
            body: live_shaped_body(),
            observed_at: 1000,
            retry_at: 1660,
            failure: Some(Box::new(UsageProbe::Http(429))),
        });
        let row = shape_claude_provider(&body);
        assert_eq!(row["observed_at"], 1000);
        assert_eq!(row["retry_at"], 1660);
        assert_eq!(row["stale"], true);
        assert_eq!(row["windows"].as_array().unwrap().len(), 3);
        let cold = shape_claude_provider(&shape_probe(UsageProbe::Deferred {
            failure: Box::new(UsageProbe::Http(429)),
            retry_at: 1660,
        }));
        assert_eq!(cold["available"], false);
        assert_eq!(cold["retry_at"], 1660);
        assert!(cold["windows"].as_array().unwrap().is_empty());
    }
    #[tokio::test]
    async fn route_cache_cannot_resurrect_previous_credentials_reading() {
        let (probe, _) = probe_sequence(vec![
            UsageProbe::Ok(live_shaped_body()),
            UsageProbe::Deferred {
                failure: Box::new(UsageProbe::Http(429)),
                retry_at: 1660,
            },
            UsageProbe::NoToken,
        ]);
        let app = app(probe);
        temp_env_ttl("0", || async {
            let (_, first) = get(&app).await;
            assert_eq!(first["available"], true);
            for _ in 0..2 {
                let (_, next) = get(&app).await;
                assert_eq!(next["available"], false);
                assert!(next["providers"][0]["windows"]
                    .as_array()
                    .unwrap()
                    .is_empty());
            }
        })
        .await;
    }
    #[tokio::test]
    async fn claude_stale_fallback_does_not_rewind_other_providers() {
        let (claude, _) = probe_sequence(vec![
            UsageProbe::Ok(live_shaped_body()),
            UsageProbe::Http(429),
        ]);
        let n = Arc::new(AtomicUsize::new(0));
        let codex: ProviderProbeFn = Arc::new(move || {
            let used = n.fetch_add(1, Ordering::SeqCst) * 20;
            Box::pin(async move {
                ProviderProbe::Ok(
                    json!({"rateLimits":{"primary":{"usedPercent":used,"windowDurationMins":300}}}),
                )
            })
        });
        let probes = UsageProbes {
            claude,
            codex,
            gemini: Arc::new(|| {
                Box::pin(async {
                    ProviderProbe::Unavailable {
                        cause: "test",
                        reason: "test".into(),
                    }
                })
            }),
        };
        let app = app_routes(routes_with_probes(probes));
        temp_env_ttl("0", || async {
            let (_, first) = get(&app).await;
            let (_, second) = get(&app).await;
            assert_eq!(first["providers"][1]["windows"][0]["used_percent"], 0.0);
            assert_eq!(second["providers"][1]["windows"][0]["used_percent"], 20.0);
            assert_eq!(second["providers"][0]["stale"], true);
        })
        .await;
    }

    #[tokio::test]
    async fn a_failure_with_no_prior_reading_stays_degraded() {
        // The fallback must never manufacture a first reading.
        let app = app(probe_fn(
            UsageProbe::Http(429),
            Arc::new(AtomicUsize::new(0)),
        ));
        let (_, v) = get(&app).await;
        assert_eq!(v["available"], json!(false));
        assert_eq!(v["cause"], json!("rate_limited"));
        assert!(v.get("limits").is_none());
        assert!(v.get("stale").is_none());
    }

    #[tokio::test]
    async fn stale_fallback_expires_and_can_be_disabled() {
        let (probe, _c) = probe_sequence(vec![
            UsageProbe::Ok(live_shaped_body()),
            UsageProbe::Http(429),
        ]);
        let app = app(probe);
        // TTL 0 forces a fresh probe each call; STALE 0 disables the
        // fallback, so the second call must degrade honestly rather than
        // reach for the reading it still holds.
        temp_env_both("0", "0", || async {
            let (_, first) = get(&app).await;
            assert_eq!(first["available"], json!(true));
            let (_, second) = get(&app).await;
            assert_eq!(
                second["available"],
                json!(false),
                "stale window 0 must not serve a prior reading"
            );
            assert_eq!(second["cause"], json!("rate_limited"));
        })
        .await;
    }

    #[tokio::test]
    async fn zero_ttl_disables_the_cache() {
        // The env knob has to actually reach the handler; a knob that reads
        // the env once at startup would pass a weaker test than this.
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(probe_fn(UsageProbe::Ok(live_shaped_body()), calls.clone()));
        temp_env_ttl("0", || async {
            get(&app).await;
            get(&app).await;
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// Set both knobs around a block (same serialization as `temp_env_ttl`).
    async fn temp_env_both<F, Fut>(ttl: &str, stale: &str, f: F)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = ()>,
    {
        let _g = env_lock().lock().await;
        std::env::set_var("AMUX_USAGE_TTL_S", ttl);
        std::env::set_var("AMUX_USAGE_STALE_S", stale);
        f().await;
        std::env::remove_var("AMUX_USAGE_TTL_S");
        std::env::remove_var("AMUX_USAGE_STALE_S");
    }

    /// Hold the env steady at its DEFAULTS for a test that READS it (AMUX-4963).
    ///
    /// The lock below used to be documented as "for every env-MUTATING test",
    /// and that word was the gap: it serialized writers against each other and
    /// left readers racing them. `cargo` runs these on parallel threads in one
    /// process, so a reader observes a writer's temporarily-set value.
    ///
    /// Measured in CI on e33f14c2: five sites set AMUX_USAGE_TTL_S="0", which
    /// DISABLES the cache by design, and `cache_serves_repeat_opens...` read
    /// that 0 and made three probes for three opens — `left: 3, right: 1`. The
    /// count was never wrong about caching; caching was genuinely off, because
    /// of somebody else's env.
    async fn default_env() -> tokio::sync::MutexGuard<'static, ()> {
        env_lock().lock().await
    }

    /// One process-wide async lock for every test that READS OR WRITES this
    /// module's env knobs. Reading is not the safe half (AMUX-4963).
    fn env_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    /// Set AMUX_USAGE_TTL_S around a block. Serialized because env is
    /// process-global and these tests share a process — and the lock is an
    /// ASYNC mutex, since it is held across the awaited block (a std
    /// `MutexGuard` across an await is a real deadlock risk, not just a lint).
    async fn temp_env_ttl<F, Fut>(val: &str, f: F)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = ()>,
    {
        let _g = env_lock().lock().await;
        std::env::set_var("AMUX_USAGE_TTL_S", val);
        f().await;
        std::env::remove_var("AMUX_USAGE_TTL_S");
    }

    // Async only so it can await the same lock its mutators hold: it asserts
    // the DEFAULT ttl, which is exactly what a concurrent setter breaks.
    #[tokio::test]
    async fn ttl_default_and_override() {
        let _env = default_env().await;
        assert_eq!(usage_ttl(), Duration::from_secs(DEFAULT_USAGE_TTL_S));
    }
}

#[cfg(test)]
mod usage_report_tests {
    use super::render_usage_report;

    use super::trigger_label;

    fn ledger_row(conn: &rusqlite::Connection, session: &str, ts: i64, cost: f64) {
        conn.execute(
            "INSERT INTO token_ledger (ts, session, conversation, model, input, cache_read, cache_write, output, cost_usd, task) \
             VALUES (?1, ?2, 'conv', 'claude-opus-5', 10, 0, 0, 5, ?3, '')",
            rusqlite::params![ts, session, cost],
        )
        .unwrap();
    }

    fn typed_prompt(conn: &rusqlite::Connection, session: &str, ts_secs: i64, kind: &str) {
        conn.execute(
            "INSERT INTO cmd_history (text, type, session, ts, origin) VALUES ('do the thing', ?1, ?2, ?3, 'ethan')",
            rusqlite::params![kind, session, ts_secs * 1000],
        )
        .unwrap();
    }

    fn steering_delivery(
        conn: &rusqlite::Connection,
        id: &str,
        session: &str,
        ts_secs: i64,
        guard: &str,
        sender: &str,
    ) {
        conn.execute(
            "INSERT INTO steering_history (id, session, text, queued_at, delivered_at, guard, sender) \
             VALUES (?1, ?2, 'nudge text', ?3, ?4, ?5, ?6)",
            rusqlite::params![id, session, ts_secs as f64, ts_secs as f64, guard, sender],
        )
        .unwrap();
    }

    fn source_keys(md: &str) -> Vec<String> {
        // Stop at the next heading: the report has more tables below this one,
        // and reading past it collected their rows as sources.
        let section = md.split("## By what triggered the turn").nth(1).unwrap();
        let section = section.split("\n## ").next().unwrap_or(section);
        section
            .lines()
            .filter(|l| l.starts_with("| ") && !l.contains("---"))
            .skip(1) // the table's own header row is not a source
            .map(|l| {
                l.trim_start_matches("| ")
                    .split(" |")
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
            .collect()
    }

    /// AMUX-4582: a turn a NUDGE started is credited to the nudge.
    ///
    /// Before this, the nudge wrote only `steering_history`, the join saw only
    /// `cmd_history`, and the turn was charged to the human prompt that
    /// happened to precede it. That inverts the one question this view exists
    /// to answer, and it inflates what looks like human-requested spend.
    #[test]
    fn a_steering_delivered_turn_is_credited_to_the_steering_guard() {
        let conn = crate::db::migrate::test_memdb();
        let now = 1_789_400_000i64;

        // The human typed something an hour before; then amux nudged; then the
        // lane spent tokens. The spend belongs to the nudge.
        typed_prompt(&conn, "amux", now - 3600, "user");
        steering_delivery(&conn, "s1", "amux", now - 120, "board-drive", "");
        ledger_row(&conn, "amux", now - 60, 4.0);

        // A lane whose newest prompt really is the human's keeps it: the CONTROL
        // that separates "reads steering" from "always says steering".
        typed_prompt(&conn, "solo", now - 300, "user");
        ledger_row(&conn, "solo", now - 60, 1.0);

        // A schedule fire groups by family, not one bucket per schedule id.
        steering_delivery(&conn, "s2", "cron", now - 200, "sched:SCHED-456", "");
        ledger_row(&conn, "cron", now - 60, 2.0);

        let md = render_usage_report(&conn, 1, 25, now).unwrap();
        let keys = source_keys(&md);
        assert!(
            keys.contains(&"steer:board-drive".to_string()),
            "the nudge is its own source: {keys:?}"
        );
        assert!(
            keys.contains(&"steer:sched".to_string()),
            "sched:SCHED-456 groups as its family: {keys:?}"
        );
        assert!(
            keys.contains(&"user".to_string()),
            "a genuinely human-prompted turn still reads user: {keys:?}"
        );
        assert!(
            !keys.iter().any(|k| k.contains("SCHED-456")),
            "not one bucket per schedule id: {keys:?}"
        );

        // And the money moved with it: $4 of nudge-driven spend that used to be
        // filed under the human prompt.
        let section = md.split("## By what triggered the turn").nth(1).unwrap();
        let section = section.split("\n## ").next().unwrap_or(section);
        let nudge_line = section
            .lines()
            .find(|l| l.starts_with("| steer:board-drive"))
            .unwrap();
        assert!(nudge_line.contains("$4.00"), "{nudge_line}");
        let human_line = section.lines().find(|l| l.starts_with("| user")).unwrap();
        assert!(
            human_line.contains("$1.00"),
            "only the lane that really typed it: {human_line}"
        );
    }

    /// A steering row OLDER than the lane's newest prompt must not win: the rule
    /// is "the latest prompt", not "steering beats everything".
    #[test]
    fn the_newest_prompt_wins_whichever_table_it_came_from() {
        let conn = crate::db::migrate::test_memdb();
        let now = 1_789_400_000i64;
        steering_delivery(&conn, "s1", "amux", now - 600, "board-drive", "");
        typed_prompt(&conn, "amux", now - 120, "user");
        ledger_row(&conn, "amux", now - 60, 3.0);
        let keys = source_keys(&render_usage_report(&conn, 1, 25, now).unwrap());
        assert_eq!(
            keys,
            vec!["user".to_string()],
            "the human typed after the nudge: {keys:?}"
        );
    }

    #[test]
    fn every_steering_family_reads_as_itself_and_an_unknown_one_still_reads_as_a_nudge() {
        assert_eq!(
            trigger_label("steer:board-drive"),
            "amux nudged this lane about a card"
        );
        assert_eq!(trigger_label("steer:sched"), "a schedule fired");
        assert_eq!(trigger_label("steer:commit-nudge"), "the commit nudge");
        assert_eq!(trigger_label("steer:auto-compact"), "an auto-compact");
        assert_eq!(
            trigger_label("steer:task-callback"),
            "a task callback from a peer"
        );
        // A guard nobody has taught this table still reads as amux, not "other":
        // the question is "did amux hand me this", and the answer is yes.
        assert_eq!(trigger_label("steer:some-future-job"), "an amux nudge");
        assert_eq!(trigger_label("user"), "you typed it");
        // Both spellings of the same thing read the same: cmd_history writes
        // `task-callback`, steering writes the guard.
        assert_eq!(
            trigger_label("task-callback"),
            trigger_label("steer:task-callback")
        );
    }

    #[test]
    fn the_markdown_report_is_sorted_by_cost_and_states_its_population() {
        let conn = crate::db::migrate::test_memdb();
        let now = 1_789_400_000i64;
        for (session, conv, model, cost, out) in [
            ("amux", "c1", "claude-opus-5", 5.0, 100),
            ("amux", "agent-x", "claude-opus-5", 1.0, 10),
            ("studio-plg", "c2", "claude-sonnet-5", 9.0, 50),
        ] {
            conn.execute(
                "INSERT INTO token_ledger (ts, session, conversation, model, input, cache_read, cache_write, output, cost_usd, task) \
                 VALUES (?1, ?2, ?3, ?4, 10, 20, 30, ?5, ?6, '')",
                rusqlite::params![now - 60, session, conv, model, out, cost],
            ).unwrap();
        }
        // Outside a 1-day window: must not be counted.
        conn.execute(
            "INSERT INTO token_ledger (ts, session, conversation, model, input, cache_read, cache_write, output, cost_usd, task) \
             VALUES (?1, 'old', 'c9', 'claude-opus-5', 1, 1, 1, 1, 100.0, '')",
            [now - 3 * 86_400],
        ).unwrap();
        let md = render_usage_report(&conn, 1, 25, now).unwrap();
        assert!(md.starts_with("# Token usage, last 1 day(s)"), "{md}");
        assert!(
            md.contains("$15.00 across 3 API responses"),
            "window excludes the old row: {md}"
        );
        assert!(
            md.contains("3 ledger rows in window of 4 total"),
            "population stated: {md}"
        );
        let worker = md.split("## By worker").nth(1).unwrap();
        let studio = worker.find("studio-plg").unwrap();
        let amux = worker.find("| amux |").unwrap();
        assert!(
            studio < amux,
            "sorted by cost, the $9 worker first: {worker}"
        );
        assert!(
            md.contains("| subagent | $1.00 |"),
            "subagent vs main split: {md}"
        );
        assert!(
            !md.contains("| old |"),
            "the out-of-window worker is absent"
        );
        assert!(md.contains("## Known limits of these numbers"));
        // Deterministic: the same ledger renders the same bytes.
        assert_eq!(md, render_usage_report(&conn, 1, 25, now).unwrap());
    }
}
