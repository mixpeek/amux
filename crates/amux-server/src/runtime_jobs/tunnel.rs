//! The tunnel RELAY CLIENT — the half of AMUX-2888 that was never ported.
//!
//! Dials out to the amux cloud gateway and long-polls for public requests
//! hitting our tunnel URL (`cloud.amux.io/t/<tid>/…`), serving each one from a
//! local target. Exposes localhost publicly with no inbound port.
//!
//! Ported from python `_tunnel_start` (py:77931), `_tunnel_loop` (py:77848),
//! `_tunnel_serve_one` (py:77817) and `_tunnel_stop` (py:77966). The HTTP
//! surface lives in `api/tunnel.rs`; the proxy CRUD that picks a target lives
//! in `api/proxies.rs`. Both call in here.
//!
//! THREE THINGS CARRY THE WEIGHT, and each was load-bearing in python:
//!
//! 1. **The self-port refusal.** amux's own control plane has no request auth
//!    (that is the cloud gateway's job for the hosted tier) and
//!    `POST /api/sessions/<n>/send` is ungated, so publishing this port is
//!    unauthenticated RCE on YOLO lanes. Refused unless
//!    `AMUX_TUNNEL_ALLOW_SELF=1`.
//! 2. **Generation fencing.** A loop parked in a 40s long-poll must never
//!    serve a request against a target the user has since changed. `stop` and
//!    `start` both bump `gen`; a loop whose `gen` is stale is dead on its next
//!    check and answers any request it already holds with a retryable 503.
//! 3. **Rate limiting BEFORE the local fetch.** A tunnel is a public front
//!    door. The cap is enforced on the relay side so a scanner drains the
//!    gateway queue with fast 429s and never reaches the local app at all.

use base64::Engine;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// The gateway this client dials. Overridable so the OSS gateway in
/// `cloud/gateway/` can be self-hosted (the Proxies tab says so in its hint).
pub fn gateway() -> String {
    std::env::var("AMUX_TUNNEL_GATEWAY")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://cloud.amux.io".into())
}

pub fn token() -> String {
    std::env::var("AMUX_TUNNEL_TOKEN").unwrap_or_default().trim().to_string()
}

pub fn configured() -> bool {
    !token().is_empty()
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).filter(|n| *n > 0).unwrap_or(default)
}

pub fn rate_per_min() -> u32 {
    env_u32("AMUX_TUNNEL_RATE_PER_MIN", 180)
}

pub fn max_concurrent() -> u32 {
    env_u32("AMUX_TUNNEL_MAX_CONCURRENT", 8)
}

/// The optional boot target (`AMUX_TUNNEL_PORT`). `None` means "do not
/// auto-start", which is the default and deliberately so: auto-starting with no
/// port would tunnel amux itself, and rule 1 above is exactly why that must
/// never be the default.
pub fn boot_target_port() -> Option<u16> {
    std::env::var("AMUX_TUNNEL_PORT").ok()?.trim().parse::<u16>().ok().filter(|p| *p > 0)
}

fn allow_self() -> bool {
    matches!(
        std::env::var("AMUX_TUNNEL_ALLOW_SELF").unwrap_or_default().trim(),
        "1" | "true" | "yes"
    )
}

#[derive(Debug, Clone, Default)]
pub struct TunnelState {
    pub running: bool,
    pub url: Option<String>,
    pub tid: Option<String>,
    pub error: Option<String>,
    pub target: Option<String>,
    pub requests: u64,
    pub gen: u64,
    pub proxy_id: Option<String>,
}

fn state() -> &'static Mutex<TunnelState> {
    static S: OnceLock<Mutex<TunnelState>> = OnceLock::new();
    S.get_or_init(Default::default)
}

pub fn snapshot() -> TunnelState {
    state().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Requests the limiter refused. A counter, not a flag: "the tunnel is up and
/// serving" and "the tunnel is up and shedding every request" look identical
/// without it (ethos rule 4).
static DROPPED: AtomicU64 = AtomicU64::new(0);

pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

fn rl_window() -> &'static Mutex<VecDeque<f64>> {
    static W: OnceLock<Mutex<VecDeque<f64>>> = OnceLock::new();
    W.get_or_init(Default::default)
}

fn now_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Sliding 60s window: `true` if under the per-minute cap, and RECORDS the hit.
///
/// Split from the window so the policy is testable without a clock
/// (ethos rule 7): a limiter that can only be exercised by sending 180 real
/// requests is a limiter nobody tests.
fn rate_ok_in(win: &mut VecDeque<f64>, now: f64, cap: u32) -> bool {
    while win.front().is_some_and(|t| now - t > 60.0) {
        win.pop_front();
    }
    if win.len() as u32 >= cap {
        return false;
    }
    win.push_back(now);
    true
}

fn rate_ok() -> bool {
    let mut w = rl_window().lock().unwrap_or_else(|e| e.into_inner());
    rate_ok_in(&mut w, now_f64(), rate_per_min())
}

/// The reply payload shape the gateway expects: status, headers, base64 body.
fn reply_payload(status: u16, msg: &str, retry: u32) -> serde_json::Value {
    serde_json::json!({
        "status": status,
        "headers": { "Content-Type": "text/plain", "Retry-After": retry.to_string() },
        "body": base64::engine::general_purpose::STANDARD.encode(msg.as_bytes()),
    })
}

/// Headers that describe THIS hop and must not be forwarded to the local app.
/// `host` would make the app render gateway URLs; `content-length` is recomputed
/// by the client; `accept-encoding` would hand back a compressed body that the
/// gateway then relays with the encoding header stripped, which is a corrupt
/// response rather than a slow one.
const HOP_REQ_HEADERS: &[&str] = &["host", "content-length", "connection", "accept-encoding"];
/// The mirror image on the way back.
const HOP_RESP_HEADERS: &[&str] = &["transfer-encoding", "connection", "content-encoding"];

/// Would `port` publish amux's own control plane?
///
/// A pure predicate so the security refusal has a test that does not need a
/// gateway, a token, or a running tunnel — the one rule here whose failure is
/// not "the feature does not work" but "the machine is owned".
pub fn refuses_self_port(port: u16, self_port: u16, allow: bool) -> bool {
    port == self_port && !allow
}

/// Start the relay. Returns the state after a brief wait for first
/// registration, or an error string the caller renders.
pub async fn start(target_port: Option<u16>) -> Result<TunnelState, String> {
    {
        let s = state().lock().unwrap_or_else(|e| e.into_inner());
        if s.running {
            return Ok(s.clone());
        }
    }
    let token = token();
    if token.is_empty() {
        return Err("no tunnel token — set AMUX_TUNNEL_TOKEN (from your amux cloud account)".into());
    }
    let self_port = crate::legacy_port::canonical_port();
    let port = target_port.unwrap_or(self_port);
    if refuses_self_port(port, self_port, allow_self()) {
        return Err(format!(
            "refusing to tunnel amux itself (port {port}) — its control plane is unauthenticated. \
             Point the proxy at a specific app port instead, or set AMUX_TUNNEL_ALLOW_SELF=1 to \
             override."
        ));
    }
    // amux serves TLS on its own port; anything else is a plain local app.
    let scheme = if port == self_port { "https" } else { "http" };
    let target_base = format!("{scheme}://127.0.0.1:{port}");

    let generation = {
        let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
        s.gen += 1;
        s.running = true;
        s.target = Some(target_base.clone());
        s.error = None;
        s.requests = 0;
        s.url = None;
        s.tid = None;
        s.gen
    };
    DROPPED.store(0, Ordering::Relaxed);
    rl_window().lock().unwrap_or_else(|e| e.into_inner()).clear();

    let h = tokio::spawn(run(token, gateway(), target_base, generation));
    // Registered so a dead relay is visible on /api/system-jobs rather than
    // silently absent. `None` interval: this is a long-poll, not a tick, so a
    // staleness verdict computed from an interval would be meaningless.
    crate::runtime_jobs::registry::adopt(crate::runtime_jobs::registry::ids::TUNNEL, None, &h);

    // Wait briefly for the first registration so the caller's response can
    // carry the URL instead of a null the UI has to poll for.
    for _ in 0..40 {
        let s = snapshot();
        if s.url.is_some() || s.error.is_some() || !s.running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(snapshot())
}

/// Stop the relay.
///
/// Bumps the generation as well as clearing `running`: the loop may be parked
/// in a 40s long-poll, and without the fence it would resume serving against a
/// stale target as soon as a later `start` flipped `running` back on.
pub fn stop() {
    let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
    s.running = false;
    s.gen += 1;
    s.url = None;
    s.tid = None;
}

/// Which saved proxy is live, or `None` when nothing is running.
pub fn active_proxy_id() -> Option<String> {
    let s = state().lock().unwrap_or_else(|e| e.into_inner());
    s.running.then(|| s.proxy_id.clone()).flatten()
}

pub fn set_proxy_id(id: Option<String>) {
    state().lock().unwrap_or_else(|e| e.into_inner()).proxy_id = id;
}

fn live(generation: u64) -> bool {
    let s = state().lock().unwrap_or_else(|e| e.into_inner());
    s.running && s.gen == generation
}

fn set_err(generation: u64, msg: String) {
    let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
    if s.gen == generation {
        s.error = Some(msg);
    }
}

/// The gateway client verifies certs normally. The LOCAL client does not: amux
/// serves a self-signed cert on its own port, and the target is `127.0.0.1` by
/// construction, so there is no name to verify and nothing between us to
/// intercept. The two are separate clients precisely so the lax setting cannot
/// leak onto the gateway leg.
fn gateway_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .build()
        .unwrap_or_default()
}

fn local_client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default()
}

/// Fetch one relayed request against the local target and pack the reply.
async fn serve_one(
    client: &reqwest::Client,
    req: &serde_json::Value,
    target_base: &str,
) -> serde_json::Value {
    let b64 = base64::engine::general_purpose::STANDARD;
    let path = req.get("path").and_then(|v| v.as_str()).unwrap_or("/");
    let qs = req.get("qs").and_then(|v| v.as_str()).unwrap_or("");
    let url = format!("{target_base}{path}{}", if qs.is_empty() { String::new() } else { format!("?{qs}") });
    let method = req
        .get("method")
        .and_then(|v| v.as_str())
        .and_then(|m| reqwest::Method::from_bytes(m.as_bytes()).ok())
        .unwrap_or(reqwest::Method::GET);

    let mut rb = client.request(method, &url);
    if let Some(hs) = req.get("headers").and_then(|v| v.as_object()) {
        for (k, v) in hs {
            if HOP_REQ_HEADERS.contains(&k.to_lowercase().as_str()) {
                continue;
            }
            if let Some(v) = v.as_str() {
                rb = rb.header(k, v);
            }
        }
    }
    if let Some(body) = req.get("body").and_then(|v| v.as_str()).filter(|b| !b.is_empty()) {
        match b64.decode(body) {
            Ok(raw) => rb = rb.body(raw),
            Err(e) => return reply_payload(400, &format!("tunnel body decode error: {e}"), 0),
        }
    }

    match rb.send().await {
        // An HTTP error status is the APP's answer and is relayed verbatim; only
        // a transport failure becomes a 502 from the relay itself. Collapsing
        // the two would make every 404 in the tunneled app look like a broken
        // tunnel.
        Ok(resp) => {
            let status = resp.status().as_u16();
            let mut headers = serde_json::Map::new();
            for (k, v) in resp.headers() {
                if HOP_RESP_HEADERS.contains(&k.as_str().to_lowercase().as_str()) {
                    continue;
                }
                if let Ok(v) = v.to_str() {
                    headers.insert(k.as_str().to_string(), serde_json::Value::String(v.into()));
                }
            }
            let raw = resp.bytes().await.unwrap_or_default();
            serde_json::json!({
                "status": status,
                "headers": headers,
                "body": b64.encode(&raw),
            })
        }
        Err(e) => reply_payload(502, &format!("tunnel local fetch error: {e}"), 0),
    }
}

async fn reply(client: &reqwest::Client, gw: &str, token: &str, rid: &str, payload: serde_json::Value) {
    let r = client
        .post(format!("{gw}/tunnel/reply?rid={rid}"))
        .bearer_auth(token)
        .json(&payload)
        .send()
        .await;
    if let Err(e) = r {
        tracing::warn!(rid, error = %e, "tunnel: reply to gateway failed");
    }
}

/// One tunnel session: register, then long-poll and serve until the generation
/// is fenced or `running` clears.
async fn run(token: String, gw: String, target_base: String, generation: u64) {
    let gwc = gateway_client();
    let localc = local_client();
    let inflight = Arc::new(tokio::sync::Semaphore::new(max_concurrent().max(1) as usize));
    let mut backoff = 1u64;

    while live(generation) {
        // --- register -------------------------------------------------------
        let reg = gwc
            .post(format!("{gw}/tunnel/register"))
            .bearer_auth(&token)
            .send()
            .await
            .and_then(|r| r.error_for_status());
        let reg: serde_json::Value = match reg {
            Ok(r) => r.json().await.unwrap_or_default(),
            Err(e) => {
                set_err(generation, format!("register failed: {e}"));
                tokio::time::sleep(Duration::from_secs(backoff.min(30))).await;
                backoff = (backoff * 2).min(30);
                continue;
            }
        };
        let Some(tid) = reg.get("tid").and_then(|v| v.as_str()).map(String::from) else {
            set_err(generation, "register returned no tid".into());
            tokio::time::sleep(Duration::from_secs(backoff.min(30))).await;
            backoff = (backoff * 2).min(30);
            continue;
        };
        let url = reg.get("url").and_then(|v| v.as_str()).map(String::from);
        {
            let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
            if s.gen != generation {
                return;
            }
            s.tid = Some(tid.clone());
            s.url = url.clone();
            s.error = None;
        }
        backoff = 1;
        tracing::info!(url = ?url, target = %target_base, "tunnel: registered");

        // --- poll -----------------------------------------------------------
        while live(generation) {
            let poll = gwc
                .get(format!("{gw}/tunnel/poll?tid={tid}"))
                .bearer_auth(&token)
                .send()
                .await;
            let item: serde_json::Value = match poll {
                Ok(r) if r.status().as_u16() == 409 => {
                    // The gateway lost our tunnel (its restart). Re-register.
                    tracing::info!("tunnel: gateway returned 409, re-registering");
                    break;
                }
                Ok(r) if !r.status().is_success() => {
                    set_err(generation, format!("poll HTTP {}", r.status().as_u16()));
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
                Ok(r) => r.json().await.unwrap_or_default(),
                // A long-poll timeout is the NORMAL idle path, not an error.
                Err(_) => continue,
            };
            if item.get("idle").and_then(|v| v.as_bool()).unwrap_or(false) {
                continue;
            }
            let Some(rid) = item.get("rid").and_then(|v| v.as_str()).map(String::from) else {
                continue;
            };

            if !live(generation) {
                // Superseded while parked in the long-poll. Serving this against
                // the stale target is the exact bug the generation fence exists
                // for; hand the caller something retryable instead.
                reply(
                    &gwc,
                    &gw,
                    &token,
                    &rid,
                    reply_payload(503, "tunnel reconfigured, retry", 1),
                )
                .await;
                break;
            }
            // BEFORE the local fetch, always. A fast 429 drains the gateway
            // queue without the flood ever reaching the app.
            if !rate_ok() {
                DROPPED.fetch_add(1, Ordering::Relaxed);
                reply(&gwc, &gw, &token, &rid, reply_payload(429, "rate limited", 10)).await;
                continue;
            }
            {
                let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
                if s.gen == generation {
                    s.requests += 1;
                }
            }

            let (gwc2, localc2, gw2, token2, target2, sem) = (
                gwc.clone(),
                localc.clone(),
                gw.clone(),
                token.clone(),
                target_base.clone(),
                inflight.clone(),
            );
            let item2 = item.clone();
            tokio::spawn(async move {
                // Concurrency cap, so a burst cannot hammer the local app with
                // unbounded simultaneous fetches.
                let permit =
                    tokio::time::timeout(Duration::from_secs(8), sem.acquire_owned()).await;
                let Ok(Ok(_permit)) = permit else {
                    DROPPED.fetch_add(1, Ordering::Relaxed);
                    reply(&gwc2, &gw2, &token2, &rid, reply_payload(503, "server busy, retry", 3))
                        .await;
                    return;
                };
                let payload = serve_one(&localc2, &item2, &target2).await;
                reply(&gwc2, &gw2, &token2, &rid, payload).await;
            });
        }
    }
    // Do not clobber a newer loop's state.
    {
        let mut s = state().lock().unwrap_or_else(|e| e.into_inner());
        if s.gen == generation {
            s.url = None;
            s.tid = None;
        }
    }
    tracing::info!("tunnel: stopped");
}

/// Start the relay at boot when, and only when, an explicit target port is
/// configured. Never defaults to amux's own port: see rule 1 in the module
/// header. Returns what it did so the caller can log a fact rather than an
/// intention.
pub async fn maybe_boot_start() -> &'static str {
    if !configured() {
        return "no token";
    }
    let Some(port) = boot_target_port() else {
        return "no AMUX_TUNNEL_PORT";
    };
    match start(Some(port)).await {
        Ok(s) => {
            tracing::info!(port, url = ?s.url, "tunnel: auto-started from AMUX_TUNNEL_PORT");
            "started"
        }
        Err(e) => {
            tracing::warn!(port, error = %e, "tunnel: auto-start refused");
            "refused"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusal whose failure mode is not "the feature is broken" but "the
    /// box is owned": amux's control plane has no request auth, and
    /// `POST /api/sessions/<n>/send` is ungated, so publishing this port is
    /// unauthenticated RCE on a YOLO lane.
    ///
    /// The override must WORK too — a refusal with no honest way past it gets
    /// removed by the next person who needs it (ethos rule 3).
    #[test]
    fn the_self_port_is_refused_unless_explicitly_allowed() {
        assert!(refuses_self_port(8824, 8824, false), "amux's own port must be refused");
        assert!(!refuses_self_port(8824, 8824, true), "AMUX_TUNNEL_ALLOW_SELF must work");
        // The controls: any OTHER port is the normal case and must pass, or the
        // check would refuse everything and pass its first assertion anyway.
        assert!(!refuses_self_port(3000, 8824, false));
        assert!(!refuses_self_port(8825, 8824, false), "adjacent, not the same");
        // And it follows the canonical port rather than a hardcoded 8822: the
        // retired port would be refused while the LIVE one sailed through.
        assert!(refuses_self_port(9999, 9999, false), "whatever the self port is");
    }

    /// Sliding 60s window, exercised without a clock.
    #[test]
    fn the_rate_limiter_sheds_over_the_cap_and_recovers_as_the_window_slides() {
        let mut w = VecDeque::new();
        for i in 0..3 {
            assert!(rate_ok_in(&mut w, 100.0, 3), "under the cap, hit {i} must pass");
        }
        assert!(!rate_ok_in(&mut w, 100.0, 3), "the 4th in the same second is over");
        // CONTROL 1: still refused 59s later — the window is 60s, not per-call.
        assert!(!rate_ok_in(&mut w, 159.0, 3), "59s later the window still holds all three");
        // CONTROL 2: once they age out it recovers. A limiter that latches is a
        // limiter that takes the tunnel down for good on one burst.
        assert!(rate_ok_in(&mut w, 161.0, 3), "past 60s the oldest expires");
        // All three were stamped at 100.0, so at 161.0 all three age out and the
        // window holds only the hit just recorded. It EVICTS rather than
        // accumulating: a window that only appended would grow without bound
        // under sustained traffic and take the process with it.
        assert_eq!(w.len(), 1, "expired entries are dropped, not kept: {w:?}");
    }

    /// Hop-by-hop headers describe THIS connection and must not cross it.
    /// `accept-encoding` is the one that bites: forward it and the app returns
    /// a compressed body, the relay strips `content-encoding` on the way back,
    /// and the caller gets bytes it cannot decode — a corrupt response rather
    /// than a slow one.
    #[test]
    fn hop_headers_are_stripped_in_both_directions() {
        for h in ["Host", "HOST", "content-length", "Accept-Encoding"] {
            assert!(HOP_REQ_HEADERS.contains(&h.to_lowercase().as_str()), "{h} must not be forwarded");
        }
        for h in ["Transfer-Encoding", "content-encoding", "Connection"] {
            assert!(HOP_RESP_HEADERS.contains(&h.to_lowercase().as_str()), "{h} must not come back");
        }
        // CONTROLS: the headers that MUST survive. A list that stripped these
        // would pass every assertion above and break every tunneled app.
        for h in ["content-type", "authorization", "cookie", "user-agent"] {
            assert!(!HOP_REQ_HEADERS.contains(&h), "{h} is the app's business");
            assert!(!HOP_RESP_HEADERS.contains(&h), "{h} is the app's business");
        }
    }

    /// Defaults match python's, because the Proxies tab renders them as the
    /// live policy and a silently different cap is a UI that lies.
    #[test]
    fn the_env_knobs_default_to_pythons_values() {
        assert_eq!(rate_per_min(), 180);
        assert_eq!(max_concurrent(), 8);
        assert_eq!(gateway(), "https://cloud.amux.io");
        // 0 and empty are "unset", not "a cap of zero" — a literal 0 would shed
        // every request while reporting the tunnel healthy.
        assert_eq!(env_u32("AMUX_TUNNEL_NOPE_XYZ", 7), 7);
    }

    /// `maybe_boot_start` must refuse to do anything without BOTH halves. The
    /// dangerous shape is a token with no port: python's comment says it, and
    /// this is the assertion behind the comment.
    #[tokio::test]
    async fn boot_start_needs_a_token_and_an_explicit_port() {
        // No token in the test environment, so this is the first gate.
        if !configured() {
            assert_eq!(maybe_boot_start().await, "no token");
        }
        // And the port must be explicit: an unset AMUX_TUNNEL_PORT means the
        // target would be amux itself, which rule 1 forbids.
        assert_eq!(boot_target_port(), None, "no port configured in the test env");
    }
}
