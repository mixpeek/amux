//! Serve transport client for OpenCode (M3.1 of the opencode-provider plan).
//!
//! WHY THIS EXISTS. M2's ACP transport is an adapter *over* `opencode serve`
//! (the live process tree proves it: `opencode acp` spawns its own
//! `opencode serve --stdio` child). This module speaks to the substrate
//! directly, over HTTP, and buys three things ACP structurally cannot:
//!
//! 1. **Asynchronous prompt admission.** `POST /api/session/{id}/prompt` returns
//!    the admitted user message immediately and the agent loop runs in the
//!    background (measured 2026-10-26 against opencode 2.1.4: returned in well
//!    under 2s while a 700-word essay turn was still streaming). ACP's
//!    `session/prompt` blocks for the whole turn, which is why M2 needed a
//!    pending-request map, an id demux and a concurrent-prompt rejection. A
//!    send here is fire-and-forget admission; completion is OBSERVED (via
//!    [`ServeClient::messages`]), never awaited.
//! 2. **Mid-turn delivery.** The prompt payload carries
//!    `delivery: "steer" | "queue"` — a steer lands inside a RUNNING turn
//!    (M3.0 spike: the steered reply closed the run and the essay turn never
//!    got an idle marker of its own). ACP answers that same case with
//!    `-32603 Session already has an active ACP prompt`.
//! 3. **Typed errors.** Quota failures arrive as
//!    `{"type":"provider.quota","status":402,"retryAfterMs":…}` and a busy
//!    session as a `_tag: "SessionBusyError"` body — parsed by tag, not
//!    text-sniffed (see [`ServeError::classify`]).
//!
//! MEASURED FACTS this client is built on (all 2026-10-26, opencode 2.1.4):
//!
//! - **Auth is HTTP Basic, username literally `opencode`.** Bearer, empty-user
//!   Basic and `?password=` all return 401. The shared `--service` reads its
//!   password from `~/.config/opencode/service.json`; a DEDICATED
//!   `opencode serve` does NOT — it generates its own 43-char password and
//!   prints `server password <43 chars>` to its startup log, so an amux-owned
//!   serve's credential is captured from the spawn output (decision D1: ONE
//!   serve for the whole fleet, ~188 MB idle, turn spikes contained to one
//!   process — the fix for the per-worker OOM class).
//! - Sessions are scoped by `location.directory` = the worker cwd: one serve
//!   process serves N workers in N directories. Create with
//!   `POST /api/session {"location":{"directory":<cwd>},"model":{…}}`.
//! - `Model.Ref` is `{providerID, id}`. A bare `{providerID, modelID}` is
//!   rejected: `InvalidRequestError "Missing key at [\"model\"][\"id\"]"`.
//! - Durable history is `GET /api/session/{id}/message` (returns full history
//!   incl. interrupted turns). The `…/log?after=` cursor did NOT replay
//!   history in the M3.0 spike — this client does not depend on it.
//!
//! The HTTP surface self-describes as experimental upstream; ACP and `run`
//! stay fully reachable fallbacks (nothing is deleted by this module).
//!
//! REQWEST `stream` FEATURE: **not needed — M3.2 proved it.** The SSE reader
//! consumes the body with `Response::chunk()`, an UNCONDITIONAL method in
//! reqwest 0.12.28 (only `bytes_stream()` is behind `#[cfg(feature =
//! "stream")]` — verified in the vendored source). The workspace Cargo.toml is
//! untouched by this milestone.
//!
//! ## M3.2 — the SSE event channel (measured live 2026-10-26, opencode 2.1.4)
//!
//! `GET /api/event` → `text/event-stream`. Frames are SSE `data:` lines each
//! carrying one JSON event `{id:"evt_<hex-ms><random>", created, type,
//! location?, data, durable?}` (the id is NOT a ULID — see the
//! `execution.started` bullet below); `: heartbeat` comment lines are
//! keep-alives. The full kind
//! catalog below was captured by driving one real tool-using turn against the
//! shared service, not guessed:
//!
//! - `server.connected` → `Started` (transport-level: broadcast to every
//!   registered worker — one reader serves the whole fleet, D1).
//! - `session.text.delta` / `session.reasoning.delta` → `Progress` (the
//!   text/thought chunks, same vocabulary as ACP's message/thought chunks).
//! - `session.tool.input.started` records the call's NAME (`session.tool.called`
//!   carries the `input` but NOT the name — measured), `session.tool.called`
//!   → `ToolUsed` with `tool_detail(input)`.
//! - `session.usage.updated` → `Progress{usage, tokens}` (total = input +
//!   output + reasoning + cache.read + cache.write — the same sum ACP reports
//!   as `totalTokens`: 5335+401+18+384 = 6138, measured).
//! - `session.execution.started` → `TurnStarted`. MEASURED CORRECTION to an
//!   earlier draft of this header: the event's own `evt_` id is **not** a
//!   ULID (hex-millisecond prefix + Crockford-excluded letters, e.g.
//!   `evt_17d06e93a001URzvK6D4b7JSX5` — `U` is not in the alphabet), so it
//!   cannot back a `TurnId`; the turn id is minted here instead, and
//!   correlating it with the turn's outcome stays D3's job via
//!   `GET /api/session/{id}/message` (see [`translate_v2_event`]).
//! - `session.execution.failed` carries the typed `Session.StructuredError`
//!   `{type, message, status, response:{body}, retryAfterMs}` (schema extracted
//!   from the shipped 2.1.4 binary; the quota shape matches the M3.0 spike's
//!   402 body) → `RateLimited` when quota-shaped, else `Failed`.
//! - `session.execution.interrupted` → `Progress` (the `reason` enum: user /
//!   shutdown / superseded / inactivity); `session.execution.succeeded` →
//!   **known-noop**: decision D3 forbids deriving `TurnCompleted` from SSE.
//! - `session.step.*`, `session.text.started/ended`, `session.tool.success`,
//!   config noise (`*.updated`, `vcs.branch.updated`) → known-noop (counted,
//!   never dispatched).
//! - anything else → `Unknown`: counted, logged ONCE with a verdict, no panic.
//!
//! **The contract, verbatim from the openapi:** *"Volatile by contract: a slow
//! consumer overflows and fails the stream, and events during disconnection are
//! missed."* So SSE is a PROGRESS channel only: turn outcome comes from
//! `GET /api/session/{id}/message` (D3, proven in the spike), never from here.
//! Every gap — stream EOF, read error, or a lagging worker consumer — is
//! counted, logged with a `verdict=`, and marks the affected sessions as owed
//! a `/message` backfill ([`ServeEventHub::take_backfill_owed`], fetched by
//! M3.3; the `…/log?after=` cursor failed to replay in the spike and is not
//! depended on).

//! ## M3.3 — steering + interrupt (measured from the same 2026-10-27 openapi)
//!
//! - `POST …/prompt` with `delivery:"steer"` lands INSIDE a running turn (M3.0
//!   spike: the steered reply closed the run and the essay turn never got an
//!   idle marker of its own); `"queue"` waits for the turn boundary. ACP
//!   answers the steer case with `-32603 Session already has an active ACP
//!   prompt`, which is the whole reason this transport exists.
//! - `POST …/interrupt` → `SessionInterruptResponse {interrupted: bool}` at
//!   the **TOP LEVEL** (no `data:` wrapper, unlike every other session
//!   endpoint). `false` is a legitimate answer — nothing was running — not an
//!   error. The openapi scopes it: *"Whether an active execution owned by this
//!   OpenCode process was interrupted"* → **interrupt is per process, i.e. per
//!   serve**; with D1 (one serve for the fleet) one worker's cancel cannot stop
//!   another worker's turn. Its optional `?resume=<true|false>` query is NOT
//!   sent — the effect is unmeasured.
//! - Turn OUTCOME is read from `GET …/message` (D3), never from the SSE
//!   stream: [`idle_outcomes`] reads the **measured** structured field
//!   `{"type":"idle","outcome":"interrupted"}` off a newest-first `data` list
//!   (opencode 2.2.1, captured 2026-10-29).
//!
//! ## M3.4 — permissions (measured from the same 2026-10-27 openapi)
//!
//! Serve surfaces permission asks as **resources, not as agent requests** (ACP's
//! `session/request_permission` is a blocking JSON-RPC request; here the agent
//! asks opencode, and opencode exposes the ask over REST):
//!
//! - `GET /api/session/{id}/permission` → `{data: Permission.Request[]}`;
//!   `Permission.Request = {id ("per_…"), sessionID, action, resources[],
//!   save[], metadata, source:{type:"tool",messageID,id}, message}`.
//! - `POST …/permission/{requestID}/reply` with `{decision, message}` →
//!   **204 No Content** (an empty body — so this path checks the STATUS, it
//!   does not parse JSON). `decision` is the measured `Permission.Reply`
//!   enum: `"once" | "always" | "reject"` — the same vocabulary M2's ACP
//!   options carry, so one `ApprovalPolicy` serves both transports.
//! - A 404 carries the typed `PermissionNotFoundError {_tag, requestID,
//!   message}`; an unknown request id must surface as that error, never as an
//!   invented reply.
//! - The existing policy seam is reused verbatim (`acp::{Approval,
//!   ApprovalPolicy, PermissionAsk}`): allow-once is the default consent, and
//!   an ask is always ANSWERED — never left hanging, because an unanswered ask
//!   is a permanent wait in the agent.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use amux_core::ids::TurnId;
use amux_core::protocol::{Failure, ProgressReport, RateLimit, RateLimitKind, ToolEvent, WorkerEvent};
use amux_core::provider::ProviderId;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use super::acp::{Approval, ApprovalPolicy, PermissionAsk};
use super::events::{is_rate_limit_message, summarize, tool_detail};
use super::{ProtocolError, Result};

/// HTTP Basic username. Measured: the literal string `opencode`; an empty
/// username 401s.
pub const AUTH_USER: &str = "opencode";

/// Connect timeout. serve is a localhost process; a bind that does not answer
/// in 5s is down, and a failure must self-announce rather than hang.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Ceiling on any single request. Prompt ADMISSION is async and fast
/// (measured: well under 2s mid-turn); the messages reader is a localhost
/// JSON read. Nothing here is allowed to wait for a turn — that is the whole
/// point of the transport — so 30s is generous and still bounded.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How a message is delivered relative to a running turn.
///
/// `Steer` lands inside the RUNNING turn (the ACP `-32603` case); `Queue`
/// waits for the turn boundary. Exposed now, but per plan D4 the queue's
/// `AtTurnBoundary` timing is only relaxed once a live steer is proven (M3.3)
/// — no silent contract change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Steer,
    Queue,
}

impl Delivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Delivery::Steer => "steer",
            Delivery::Queue => "queue",
        }
    }
}

// ---------------------------------------------------------------------------
// Typed errors
// ---------------------------------------------------------------------------

/// A non-2xx serve response, classified by the tags opencode actually sends —
/// never by sniffing prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeError {
    /// `_tag: "SessionBusyError"` — the session is mid-turn. Not a failure of
    /// the transport: the caller decides whether a steer or a retry fits.
    Busy { session_id: Option<String> },
    /// `type: "provider.quota"` — measured body carries a machine-readable
    /// `retryAfterMs` (M3.6 maps this to `RateLimited`; ACP can only
    /// text-sniff the same condition).
    Quota { retry_after_ms: Option<u64> },
    /// Anything else. Body is bounded to a one-line summary (Invariant 30:
    /// headlines, not transcripts).
    Other {
        status: u16,
        endpoint: String,
        body: String,
    },
}

impl ServeError {
    /// Classify a non-2xx body. `endpoint` is kept on `Other` so the enum is
    /// self-describing in logs and tests.
    pub fn classify(status: u16, endpoint: &str, body: &str) -> Self {
        let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        if v.get("_tag").and_then(Value::as_str) == Some("SessionBusyError") {
            return ServeError::Busy {
                session_id: v
                    .get("sessionID")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            };
        }
        if v.get("type").and_then(Value::as_str) == Some("provider.quota") {
            return ServeError::Quota {
                retry_after_ms: v.get("retryAfterMs").and_then(Value::as_u64),
            };
        }
        ServeError::Other {
            status,
            endpoint: endpoint.to_string(),
            body: summarize(body),
        }
    }
}

/// Every failure the caller sees names the HTTP method, the endpoint and the
/// status, so a log sweep can find the failing call (repo rule: a bug fix
/// surfaces in the logs; each arm also emits its own WARN/ERROR line).
fn serve_failure(method: &str, endpoint: &str, status: u16, body: &str) -> ProtocolError {
    let kind = ServeError::classify(status, endpoint, body);
    match kind {
        ServeError::Busy { ref session_id } => {
            tracing::warn!(method, endpoint, status, session_id = ?session_id,
                "serve: session busy (SessionBusyError)");
            let who = session_id
                .as_deref()
                .map(|s| format!(" ({s})"))
                .unwrap_or_default();
            ProtocolError::Rejected(format!(
                "serve {method} {endpoint} -> HTTP {status} SessionBusyError{who}: the session is mid-turn"
            ))
        }
        ServeError::Quota { retry_after_ms } => {
            tracing::warn!(method, endpoint, status, retry_after_ms,
                "serve: provider quota exceeded (provider.quota)");
            let hint = retry_after_ms
                .map(|ms| format!(" retryAfterMs={ms}"))
                .unwrap_or_default();
            ProtocolError::Transport(format!(
                "serve {method} {endpoint} -> HTTP {status} provider.quota{hint}"
            ))
        }
        ServeError::Other { ref body, .. } => {
            tracing::error!(method, endpoint, status, body = %body,
                "serve request failed");
            ProtocolError::Transport(format!(
                "serve {method} {endpoint} -> HTTP {status}: {body}"
            ))
        }
    }
}

/// A request that never reached the server (connection refused, DNS, body
/// read). Still names method and endpoint — there is no status to name.
fn transport_failure(method: &str, endpoint: &str, e: &reqwest::Error) -> ProtocolError {
    tracing::error!(method, endpoint, error = %e, "serve: request could not be completed");
    ProtocolError::Transport(transport_failure_msg(method, endpoint, &e.to_string()))
}

/// [`transport_failure`]'s message, factored out so the M3.6 table test can
/// exercise the connection-refused shape (there is no public way to build a
/// `reqwest::Error`) **without** the template drifting from production.
fn transport_failure_msg(method: &str, endpoint: &str, detail: &str) -> String {
    format!("serve {method} {endpoint}: request failed: {detail}")
}

/// The HTTP status a `ProtocolError` came from, if it came from an HTTP call.
///
/// This parses a message **we** construct, in the fixed format
/// `serve {method} {endpoint} -> HTTP {status} …` — not a provider's prose.
/// A transport failure has no status, and that absence is meaningful (see
/// [`serve_worker_event`]).
fn http_status(msg: &str) -> Option<u16> {
    msg.split("HTTP ")
        .nth(1)?
        .get(0..3)?
        .parse::<u16>()
        .ok()
}

/// How a serve-side [`ProtocolError`] surfaces to the harness (M3.6).
///
/// `structured.rs` only holds the `ProtocolError` by the time it reports a
/// failure — the [`ServeError`] that classified the response is gone — so this
/// reads back the fields [`serve_failure`] wrote, in a format this module owns:
/// `serve {method} {endpoint} -> HTTP {status} …`, optionally suffixed with
/// `provider.quota retryAfterMs={ms}` or `: {body}`.
///
/// - **`None` = not a failure.** A rejection (the guard's busy refusal or a
///   `SessionBusyError`) is handled by the queue's `AtTurnBoundary` retry, so
///   emitting a `Failed` for it would send the fleet chasing a turn boundary.
/// - **Rate limits become `RateLimited`**, typed first (`provider.quota`,
///   HTTP 402/429) with [`is_rate_limit_message`] kept as the wording
///   fallback (the repo's sanctioned backup — and the only thing that catches
///   `provider.free-limit`, measured 2026-10-30). `reset_at` comes from
///   `retryAfterMs` **only**; a limit with no clock gets `None` rather than an
///   invented one, and `kind` stays `Unknown` for the same reason M3.2's SSE
///   path keeps it: `Credit` explicitly has no clock.
/// - **Everything else is `Failed{retryable}`** naming method+endpoint:
///   4xx will not fix itself (`false`), a 5xx or a missing status — a
///   connection refused — says `true`.
pub fn serve_worker_event(error: &ProtocolError) -> Option<WorkerEvent> {
    let msg: &str = match error {
        ProtocolError::Rejected(_) => return None,
        ProtocolError::NoSession(w) => {
            return Some(WorkerEvent::Failed(Failure {
                reason: format!("no live agent session for {w}"),
                retryable: false,
            }));
        }
        ProtocolError::Transport(m) => m,
    };
    let status = http_status(msg);
    let rate_limited = msg.contains("provider.quota")
        || status == Some(402)
        || status == Some(429)
        || is_rate_limit_message(msg);
    if rate_limited {
        // `split().nth(1)` is None when the marker is absent — `rsplit` would
        // hand back the whole message and parse "402" out of `HTTP 402`.
        let reset_at = msg
            .split("retryAfterMs=")
            .nth(1)
            .and_then(|tail| tail.split(|c: char| !c.is_ascii_digit()).next())
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse::<i64>().ok())
            .and_then(chrono::Duration::try_milliseconds)
            .map(|d| chrono::Utc::now() + d);
        return Some(WorkerEvent::RateLimited(RateLimit {
            kind: RateLimitKind::Unknown,
            reset_at,
            provider: ProviderId::new("opencode"),
            raw: Some(summarize(msg)),
        }));
    }
    Some(WorkerEvent::Failed(Failure {
        reason: summarize(msg),
        retryable: !matches!(status, Some(s) if (400..500).contains(&s)),
    }))
}

// ---------------------------------------------------------------------------
// Model.Ref
// ---------------------------------------------------------------------------

/// Encode a worker model string (`"opencode/deepseek-v4.1-flash-free"`) as the
/// serve `Model.Ref` `{providerID, id}`. A bare `{providerID, modelID}` is
/// rejected upstream (`InvalidRequestError "Missing key at [\"model\"][\"id\"]"`
/// — measured 2026-10-26). A string with no `/` is treated as the model id
/// under the `opencode` provider.
pub fn model_ref(model: &str) -> Value {
    let model = model.trim();
    let (provider, id) = match model.split_once('/') {
        Some((p, id)) if !p.is_empty() && !id.is_empty() => (p, id),
        _ => (AUTH_USER, model),
    };
    json!({"providerID": provider, "id": id})
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// An authenticated HTTP client for one serve process. Cheap to clone
/// (reqwest's pool is shared); N workers on the same serve share the
/// connection pool by cloning this.
#[derive(Clone)]
pub struct ServeClient {
    http: reqwest::Client,
    base: String,
    password: String,
}

impl ServeClient {
    /// Connect to a serve process at `base_url` (e.g. `http://127.0.0.1:49374`)
    /// with the Basic-auth password. The password's provenance is the
    /// caller's business: for the shared service it comes from
    /// `service.json` / env (see [`ServeClient::from_env`]); for an
    /// amux-OWNED serve it is the `server password <43 chars>` line captured
    /// from the spawn output — a dedicated serve does not read `service.json`
    /// (measured).
    pub fn connect(base_url: impl Into<String>, password: impl Into<String>) -> Result<Self> {
        let base = base_url.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            // Bounded on EVERY call, by construction — no request path in this
            // module can hang a worker turn.
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| {
                ProtocolError::Transport(format!("serve: build HTTP client for {base}: {e}"))
            })?;
        Ok(Self {
            http,
            base,
            password: password.into(),
        })
    }

    /// Connect to the SHARED service via the env contract:
    /// `AMUX_OPENCODE_SERVICE_URL` names the endpoint (mandatory — the shared
    /// service's port is dynamic and guessing it is not a thing),
    /// `AMUX_OPENCODE_SERVICE_PASSWORD` overrides, else
    /// `~/.config/opencode/service.json` carries the password. This is the
    /// explicit shared-service opt-in (decision D1); an amux-owned serve
    /// bypasses this entirely by handing its captured password to
    /// [`ServeClient::connect`].
    pub fn from_env() -> Result<Self> {
        let base = std::env::var("AMUX_OPENCODE_SERVICE_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                ProtocolError::Transport(
                    "serve: AMUX_OPENCODE_SERVICE_URL is not set — the shared-service \
                     opt-in needs an explicit URL (an amux-owned serve is spawned with \
                     its password captured from the startup log instead)"
                        .into(),
                )
            })?;
        let password = std::env::var("AMUX_OPENCODE_SERVICE_PASSWORD")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .or_else(|| {
                let path =
                    PathBuf::from(std::env::var("HOME").ok()?).join(".config/opencode/service.json");
                password_from_service_json(&std::fs::read_to_string(path).ok()?)
            })
            .ok_or_else(|| {
                ProtocolError::Transport(
                    "serve: no password — set AMUX_OPENCODE_SERVICE_PASSWORD or provide \
                     ~/.config/opencode/service.json with a password field"
                        .into(),
                )
            })?;
        Self::connect(base, password)
    }

    /// Create a session scoped to `cwd`, pinned to `model` when one is given.
    /// Returns the `ses_…` id — persist it as the conversation ref.
    pub async fn create_session(&self, cwd: &Path, model: Option<&str>) -> Result<String> {
        let mut body = json!({"location": {"directory": cwd.to_string_lossy()}});
        if let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) {
            body["model"] = model_ref(model);
        }
        let v = self
            .send_json(reqwest::Method::POST, "/api/session", Some(body))
            .await?;
        self.data_id("/api/session", 200, &v)
    }

    /// Prompt admission. POSTs the text and returns the ADMITTED user
    /// message's id. This call does NOT wait for the turn — the agent loop
    /// runs in the background and its outcome is read later from
    /// [`ServeClient::messages`] (or the event stream, M3.2).
    pub async fn prompt_async(
        &self,
        session_id: &str,
        text: &str,
        delivery: Delivery,
    ) -> Result<String> {
        let endpoint = format!("/api/session/{session_id}/prompt");
        let body = json!({"text": text, "delivery": delivery.as_str()});
        let v = self
            .send_json(reqwest::Method::POST, &endpoint, Some(body))
            .await?;
        self.data_id(&endpoint, 200, &v)
    }

    /// The durable message history for a session — the reconciliation path
    /// (D3). Full history including interrupted turns, whose outcome rides as
    /// a structured `{type: "idle", outcome: …}` row (MEASURED on opencode
    /// 2.2.1, 2026-10-29; the body is `{data: [...], cursor}`, newest-first).
    /// Returned opaquely — [`idle_outcomes`] does the outcome parsing.
    pub async fn messages(&self, session_id: &str) -> Result<Value> {
        let endpoint = format!("/api/session/{session_id}/message");
        self.send_json(reqwest::Method::GET, &endpoint, None).await
    }

    /// Interrupt whatever turn this session is running, returning whether an
    /// execution was actually interrupted. `false` is a MEASURED answer —
    /// nothing was running — and must never be confused with a failure.
    ///
    /// MEASURED (openapi, captured 2026-10-27): the 200 body is
    /// `SessionInterruptResponse {interrupted: bool}` at the TOP LEVEL — no
    /// `data:` wrapper, unlike every other session endpoint — and it is scoped
    /// to "an active execution **owned by this OpenCode process**", i.e. an
    /// interrupt reaches the sessions of THIS serve and no others (D1's
    /// one-serve-per-fleet makes that a fleet-visible property). The optional
    /// `?resume=<true|false>` query is deliberately not sent: its effect is
    /// unmeasured, so the server's own default applies.
    pub async fn interrupt(&self, session_id: &str) -> Result<bool> {
        let endpoint = format!("/api/session/{session_id}/interrupt");
        let v = self
            .send_json(reqwest::Method::POST, &endpoint, None)
            .await?;
        v.get("interrupted")
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                ProtocolError::Transport(format!(
                    "serve POST {endpoint} -> HTTP 200: no `interrupted` in the measured \
                     SessionInterruptResponse: {}",
                    summarize(&v.to_string())
                ))
            })
    }

    /// This session's pending permission asks (MEASURED: `GET …/permission` →
    /// `{data: Permission.Request[]}`). Returned opaquely; decode with
    /// [`permission_requests`]. The fleet-wide sibling
    /// `GET /api/permission/request` answers the same array for a location —
    /// not implemented: the session-scoped form is the one a worker needs.
    pub async fn permission_requests(&self, session_id: &str) -> Result<Value> {
        let endpoint = format!("/api/session/{session_id}/permission");
        self.send_json(reqwest::Method::GET, &endpoint, None).await
    }

    /// Answer one ask. MEASURED: `POST …/permission/{requestID}/reply` with a
    /// `{decision, message}` body answers **204 No Content** — an EMPTY body —
    /// so this path checks the STATUS and deliberately does not parse JSON
    /// (every other path here goes through [`ServeClient::send_json`], which
    /// would fail on an empty body). A 404 carries the typed
    /// `PermissionNotFoundError {_tag, requestID, message}`.
    pub async fn reply_permission(
        &self,
        session_id: &str,
        request_id: &str,
        decision: PermissionReply,
    ) -> Result<()> {
        let endpoint = format!("/api/session/{session_id}/permission/{request_id}/reply");
        let body = json!({"decision": decision.as_str(), "message": Value::Null});
        let (status, text) = self
            .request(reqwest::Method::POST, &endpoint, Some(body))
            .await?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(serve_failure("POST", &endpoint, status, &text))
        }
    }

    /// Does a persisted `ses_…` id still resolve? Probed with the proven
    /// `/message` endpoint: 2xx = alive, 404 = gone (the caller drops the ref
    /// per the M1 dead-ref rule), anything else is a real failure that must
    /// NOT silently drop a good ref.
    pub async fn session_exists(&self, session_id: &str) -> Result<bool> {
        let endpoint = format!("/api/session/{session_id}/message");
        let (status, text) = self.request(reqwest::Method::GET, &endpoint, None).await?;
        match status {
            200..=299 => Ok(true),
            404 => Ok(false),
            _ => Err(serve_failure("GET", &endpoint, status, &text)),
        }
    }

    /// One request round trip: send, read the body, return `(status, text)`.
    /// Transport failures are ProtocolErrors naming method+endpoint; non-2xx
    /// is NOT an error here — classification is the caller's step, so
    /// [`ServeClient::session_exists`] can tell 404 from a real failure.
    async fn request(
        &self,
        method: reqwest::Method,
        endpoint: &str,
        body: Option<Value>,
    ) -> std::result::Result<(u16, String), ProtocolError> {
        let method_str = method.as_str().to_string();
        let url = format!("{}{}", self.base, endpoint);
        let mut req = self
            .http
            .request(method, &url)
            .basic_auth(AUTH_USER, Some(&self.password));
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| transport_failure(&method_str, endpoint, &e))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| transport_failure(&method_str, endpoint, &e))?;
        Ok((status, text))
    }

    /// Send, require 2xx, parse JSON. Every failure names
    /// method/endpoint/status.
    async fn send_json(
        &self,
        method: reqwest::Method,
        endpoint: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let method_str = method.as_str().to_string();
        let (status, text) = self.request(method, endpoint, body).await?;
        if !(200..300).contains(&status) {
            return Err(serve_failure(&method_str, endpoint, status, &text));
        }
        serde_json::from_str(&text).map_err(|e| {
            ProtocolError::Transport(format!(
                "serve {method_str} {endpoint} -> HTTP {status}: non-JSON response ({e}): {}",
                summarize(&text)
            ))
        })
    }

    /// Pull `data.id` out of a `{"data":{"id":…}}` envelope, naming the
    /// endpoint and status when the shape is not what was measured.
    fn data_id(&self, endpoint: &str, status: u16, v: &Value) -> Result<String> {
        v.pointer("/data/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                ProtocolError::Transport(format!(
                    "serve POST {endpoint} -> HTTP {status}: no data.id in response: {}",
                    summarize(&v.to_string())
                ))
            })
    }
}

/// The password out of the shared service's `~/.config/opencode/service.json`
/// (`{"password": "…"}`). Pure, so the parse is testable without a home dir.
pub fn password_from_service_json(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text).ok()?;
    v.get("password")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Turn outcomes out of a `GET …/message` body — D3's reconciliation path.
///
/// MEASURED on opencode **2.2.1** (2026-10-29, dedicated serve): the body is
/// `{data: [...], cursor}`, `data` is **newest-first**, and an outcome is a
/// **structured field on an `idle` row**:
///
/// ```json
/// {"id": "msg_…", "time": {…}, "type": "idle", "outcome": "interrupted"}
/// ```
///
/// sibling rows look like `{"type": "user", "text": …}` and
/// `{"type": "assistant", "content": [{"type": "reasoning", …}], "finish": …}`.
/// There are **no `info`/`parts` wrappers** in the measured body — those lived
/// only in a hand-written fixture since M3.1 and never in a capture.
///
/// Rows are returned in the server's order (newest first), so "last one wins"
/// stays the CALLER's decision rather than a guess baked in here.
///
/// The `idle outcome=<x>` TEXT sentinel is kept as a **compatibility path**:
/// it was assumed, never captured, and no `sse-live`/`sse-passive` line
/// contains it. A hit on it is therefore unverified — prefer the structured
/// field's verdict.
pub fn idle_outcomes(messages: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(rows) = messages.get("data").and_then(Value::as_array) else {
        return out;
    };
    for row in rows {
        // Measured path: a structured `outcome` field (2.2.1 puts it on an
        // `idle` row). Matched on the field, not on `type`, so a renamed row
        // type cannot silently hide the outcome.
        if let Some(o) = row.get("outcome").and_then(Value::as_str) {
            out.push(o.to_string());
            continue;
        }
        // Unverified compatibility path: the `idle outcome=<x>` text sentinel,
        // in a row-level `text` or the assumed `parts[].text`.
        let mut texts: Vec<&str> = Vec::new();
        if let Some(t) = row.get("text").and_then(Value::as_str) {
            texts.push(t);
        }
        if let Some(parts) = row.get("parts").and_then(Value::as_array) {
            for part in parts {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    texts.push(t);
                }
            }
        }
        for t in texts {
            for line in t.lines() {
                if let Some(rest) = line.trim().strip_prefix("idle outcome=") {
                    out.push(rest.trim().to_string());
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// The result of opening a session. `resume_dropped` names a persisted
/// `ses_…` id that turned out to be gone — the caller forgets it so the next
/// open does not fail identically forever (the M1 dead-ref rule).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Opened {
    pub session_id: String,
    pub resumed: bool,
    pub resume_dropped: Option<String>,
}

/// A conversation with one serve process, scoped to one worker cwd. Holds no
/// process — the serve is shared fleet-wide (decision D1); this is just the
/// client plus the `ses_…` id the caller persists as the conversation ref.
pub struct ServeSession {
    client: ServeClient,
    session_id: String,
}

impl ServeSession {
    /// Open a session in `cwd`, pinned to `model` when given. A persisted
    /// `resume` id that still resolves continues the same conversation; one
    /// the server has forgotten (404) is DROPPED and reported in
    /// [`Opened::resume_dropped`] while a fresh session is created — a dead
    /// ref must never fail every future turn forever.
    pub async fn open(
        client: ServeClient,
        cwd: &Path,
        model: Option<&str>,
        resume: Option<&str>,
    ) -> Result<(Self, Opened)> {
        let mut out = Opened::default();
        if let Some(id) = resume.map(str::trim).filter(|s| !s.is_empty()) {
            match client.session_exists(id).await {
                Ok(true) => {
                    tracing::info!(session = %id, cwd = %cwd.display(),
                        "serve session resumed");
                    out.session_id = id.to_string();
                    out.resumed = true;
                    return Ok((
                        Self {
                            client,
                            session_id: id.to_string(),
                        },
                        out,
                    ));
                }
                Ok(false) => {
                    tracing::warn!(session = %id,
                        "serve resume: stored session is gone; starting fresh and dropping the ref");
                    out.resume_dropped = Some(id.to_string());
                }
                // Server unreachable / typed failure: propagate. Dropping a
                // good ref because serve had a bad minute would silently fork
                // the conversation.
                Err(e) => return Err(e),
            }
        }
        let session_id = client.create_session(cwd, model).await?;
        tracing::info!(session = %session_id, cwd = %cwd.display(),
            resumed = out.resumed, "serve session created");
        out.session_id = session_id.clone();
        Ok((Self { client, session_id }, out))
    }

    /// The provider-native conversation id — persist this; it resumes the
    /// conversation after an amux-server restart.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Admit a prompt without waiting for the turn (see
    /// [`ServeClient::prompt_async`]). Returns the admitted message id.
    pub async fn prompt_async(&self, text: &str, delivery: Delivery) -> Result<String> {
        self.client
            .prompt_async(&self.session_id, text, delivery)
            .await
    }

    /// The durable history of this session (the reconciliation path).
    pub async fn messages(&self) -> Result<Value> {
        self.client.messages(&self.session_id).await
    }

    /// Deliver `text` INSIDE this session's running turn — the case ACP
    /// refuses with `-32603 Session already has an active ACP prompt`. Returns
    /// the admitted message id; the turn's outcome is still read from
    /// [`ServeSession::messages`] (D3), never awaited here.
    ///
    /// D4 caveat: this relaxes the message queue's `AtTurnBoundary` timing for
    /// serve workers. The queued path stays intact for `run`/`acp`.
    pub async fn steer(&self, text: &str) -> Result<String> {
        self.prompt_async(text, Delivery::Steer).await
    }

    /// Interrupt this session's running turn and report the ack. `Ok(false)`
    /// means nothing was running — a normal answer, not a failure (see
    /// [`ServeClient::interrupt`]). The ack is logged so a sweep can see who
    /// cancelled what; the OUTCOME still comes from `/message` (D3).
    pub async fn interrupt(&self) -> Result<bool> {
        let interrupted = self.client.interrupt(&self.session_id).await?;
        tracing::info!(
            session = %self.session_id,
            interrupted,
            verdict = "serve interrupt acknowledged; turn outcome still read from /message (D3)",
            "serve: interrupt"
        );
        Ok(interrupted)
    }

    /// The turn outcomes recorded in this session's durable history, in the
    /// order the server returned them (see [`idle_outcomes`]).
    pub async fn turn_outcomes(&self) -> Result<Vec<String>> {
        Ok(idle_outcomes(&self.messages().await?))
    }

    /// This session's pending permission asks, decoded into the shape the
    /// policy already speaks (see [`permission_requests`]).
    pub async fn poll_permissions(&self) -> Result<Vec<PermissionRequest>> {
        Ok(permission_requests(
            &self.client.permission_requests(&self.session_id).await?,
        ))
    }

    /// Answer one ask from `policy` (M2's seam, reused verbatim).
    ///
    /// Two rules make this safe rather than merely convenient:
    /// - **Never invent a reply.** The decision is only sent if the server
    ///   offered that vocabulary; anything the ask does not offer degrades to
    ///   `reject` — a refusal the protocol understands, not an option we made
    ///   up. (Serve's vocabulary is the fixed measured enum, so this guard is
    ///   belt-and-braces today — it exists because ACP's options are per-ask
    ///   and the same policy code serves both.)
    /// - **Never leave an ask hanging.** Every path here replies, including
    ///   the refusal path: an unanswered ask is a permanent wait in the agent.
    ///
    /// The decision is logged with the tool, the action, the resources and the
    /// verdict — the same two-fix visibility ACP gives its asks.
    pub async fn answer_permission(
        &self,
        request: &PermissionRequest,
        policy: &dyn ApprovalPolicy,
    ) -> Result<PermissionReply> {
        let approval = policy.decide(&request.ask);
        let offered = |wanted: &str| request.ask.option_ids.iter().any(|o| o == wanted);
        let reply = match approval {
            Approval::AllowOnce if offered("once") => PermissionReply::AllowOnce,
            Approval::AllowAlways if offered("always") => PermissionReply::AllowAlways,
            Approval::Reject if offered("reject") => PermissionReply::Reject,
            // A decision whose vocabulary the ask did not offer: refuse with
            // the one reply every ask accepts, and say so loudly.
            other => {
                tracing::warn!(
                    session = %self.session_id,
                    request = %request.id,
                    tool = %request.ask.tool,
                    decision = ?other,
                    offered = ?request.ask.option_ids,
                    verdict = "policy decision not offered by the ask; answering reject",
                    "serve permission ask had no matching option"
                );
                PermissionReply::Reject
            }
        };
        tracing::info!(
            session = %self.session_id,
            request = %request.id,
            tool = %request.ask.tool,
            kind = request.ask.kind.as_deref().unwrap_or(""),
            resources = ?request.ask.locations,
            decision = ?approval,
            reply = %reply.as_str(),
            verdict = "serve permission ask answered; an unanswered ask would be a permanent wait",
            "serve permission ask answered"
        );
        self.client
            .reply_permission(&self.session_id, &request.id, reply)
            .await?;
        Ok(reply)
    }
}

// ---------------------------------------------------------------------------
// Permissions (M3.4)
// ---------------------------------------------------------------------------

/// How to answer a permission ask — the measured `Permission.Reply` enum
/// (`"once" | "always" | "reject"`), which is the same vocabulary M2's ACP
/// options carry, so one policy drives both transports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionReply {
    /// Allow this call only. M2's default consent (`AllowOncePolicy`).
    AllowOnce,
    /// Allow this call and SAVE the rule (`PermissionSaved`), so the ask stops
    /// recurring for this action+resource.
    AllowAlways,
    /// Refuse the call.
    Reject,
}

impl PermissionReply {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionReply::AllowOnce => "once",
            PermissionReply::AllowAlways => "always",
            PermissionReply::Reject => "reject",
        }
    }
}

/// One pending permission ask, decoded from the measured
/// `Permission.Request` into the shape M2's [`PermissionAsk`] already speaks,
/// so `dyn ApprovalPolicy` serves both transports unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    /// `per_…` — the id the reply names (measured pattern `^per`).
    pub id: String,
    /// The session that asked.
    pub session_id: String,
    /// The ask itself. `option_ids` is filled from the measured
    /// `Permission.Reply` enum so the "never invent a reply" guard has
    /// something real to check.
    pub ask: PermissionAsk,
}

/// Decode `GET …/permission`'s `{data: [Permission.Request]}` (measured
/// shape: `{id, sessionID, action, resources[], save[], metadata, source,
/// message}`).
///
/// A row missing a required field is **skipped, not guessed**: an ask we
/// cannot name is an ask we must not answer, and inventing the missing parts
/// is exactly the failure mode this transport exists to avoid. The server's
/// `message` text becomes [`PermissionAsk::title`], which is where ACP keeps
/// its human title.
pub fn permission_requests(body: &Value) -> Vec<PermissionRequest> {
    let Some(rows) = body.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let id = row.get("id").and_then(Value::as_str)?.to_string();
            let session_id = row.get("sessionID").and_then(Value::as_str)?.to_string();
            let action = row.get("action").and_then(Value::as_str)?.to_string();
            let resources = row
                .get("resources")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Some(PermissionRequest {
                id,
                session_id,
                ask: PermissionAsk {
                    tool: action,
                    title: row
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    // Serve has no per-ask `kind`; `source.type` is the
                    // measured constant `"tool"`.
                    kind: Some("tool".to_string()),
                    locations: resources,
                    option_ids: vec![
                        "once".to_string(),
                        "always".to_string(),
                        "reject".to_string(),
                    ],
                },
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// SSE event channel (M3.2)
// ---------------------------------------------------------------------------

/// One item parsed from the SSE byte stream: a complete frame payload (the
/// joined `data:` lines), a heartbeat comment (`: …` — keep-alive, not an
/// event), or an oversized frame that was dropped (bounded: a frame larger
/// than [`MAX_FRAME_BYTES`] is not a valid serve event and must not grow
/// memory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseFrame {
    Data(String),
    Heartbeat,
    Oversize,
}

/// Upper bound on one SSE frame (and on an unterminated line). Measured
/// frames are a few hundred bytes; 1 MiB is generous and keeps a broken or
/// hostile stream from growing the parser buffer without limit.
pub const MAX_FRAME_BYTES: usize = 1 << 20;

/// Incremental SSE frame parser: feed byte chunks as they arrive, get complete
/// frames out. Handles frames split across TCP chunks, CRLF line endings, and
/// multi-`data:` frames (joined with `\n` per the SSE spec). `event:`/`id:`/
/// `retry:` fields and unknown fields are ignored — the measured frames carry
/// everything in the JSON payload of a single `data:` line.
#[derive(Default)]
pub struct SseParser {
    pending: Vec<u8>,
    data_lines: Vec<String>,
}

impl SseParser {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseFrame> {
        let mut out = Vec::new();
        self.pending.extend_from_slice(bytes);
        while let Some(pos) = self.pending.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=pos).collect();
            line.pop(); // '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.len() > MAX_FRAME_BYTES {
                self.data_lines.clear();
                out.push(SseFrame::Oversize);
                continue;
            }
            if line.is_empty() {
                // Frame boundary: dispatch accumulated data, if any.
                if !self.data_lines.is_empty() {
                    let payload = std::mem::take(&mut self.data_lines).join("\n");
                    out.push(SseFrame::Data(payload));
                }
                continue;
            }
            if line[0] == b':' {
                out.push(SseFrame::Heartbeat);
                continue;
            }
            if let Some(rest) = line.strip_prefix(b"data:") {
                let rest = rest.strip_prefix(b" ").unwrap_or(rest);
                self.data_lines.push(String::from_utf8_lossy(rest).into_owned());
            }
            // event:/id:/retry:/unknown fields: ignored per the SSE spec.
        }
        if self.pending.len() > MAX_FRAME_BYTES {
            self.pending.clear();
            self.data_lines.clear();
            out.push(SseFrame::Oversize);
        }
        out
    }
}

/// What [`translate_v2_event`] decided for one frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Translated {
    /// Events to dispatch. `session: Some(id)` routes to that worker's
    /// channel; `session: None` (only `server.connected`) broadcasts to every
    /// registered worker — one reader serves the whole fleet (D1).
    Events {
        session: Option<String>,
        events: Vec<WorkerEvent>,
    },
    /// A recognized kind that deliberately emits nothing (config pings, turn
    /// OUTCOMES — decision D3: `TurnCompleted` is never derived from SSE —
    /// and terminal tool/step frames whose content belongs in logs).
    KnownNoop,
    /// A kind outside the measured 2.1.4 catalog: counted, logged once with a
    /// verdict, never dispatched (the drift detector for opencode upgrades).
    Unknown(String),
    /// Unusable shape: not JSON, no `type`, or a session-scoped event with no
    /// session id to route it by. Counted, never dispatched.
    Malformed,
}

/// V2 kinds present in the measured opencode 2.1.4 event catalog (live capture
/// plus schema dump from the shipped binary) that deliberately emit no event.
/// Anything NOT in this list and not handled by name in
/// [`translate_v2_event`] is [`Translated::Unknown`] — the honest signal that
/// the surface drifted and needs re-measuring.
const KNOWN_NOOP_KINDS: &[&str] = &[
    // observed live during the M3.2 capture
    "session.created",
    "session.instructions.updated",
    "session.inbox.enqueued",
    "session.inbox.delivered",
    "session.inbox.cancelled",
    "session.inbox.delivery.changed",
    "session.step.started",
    "session.step.streamed",
    "session.step.ended",
    // A step failure may be retried (`session.retry.scheduled`) and the
    // execution still succeed; the TERMINAL typed error arrives on
    // `session.execution.failed`, so surfacing step failures would be a
    // false alarm for recovered turns.
    "session.step.failed",
    "session.text.started",
    "session.text.ended",
    "session.reasoning.started",
    "session.reasoning.ended",
    "session.usage.recorded",
    // config/lifecycle pings observed live — no worker semantics
    "provider.updated",
    "model.updated",
    "project.updated",
    "agent.updated",
    "command.updated",
    "skill.updated",
    "websearch.updated",
    "reference.updated",
    "plugin.updated",
    "integration.updated",
    "vcs.branch.updated",
    // session lifecycle kinds in the 2.1.4 catalog (binary schema dump)
    "session.viewed",
    "session.archived",
    "session.unarchived",
    "session.moved",
    "session.renamed",
    "session.deleted",
    "session.forked",
    "session.permissions",
    "session.metadata.updated",
    "session.agent.selected",
    "session.model.selected",
    "session.skill.activated",
    "session.shell.started",
    "session.shell.ended",
    "session.synthetic",
    "session.message.content.updated",
    "session.retry.scheduled",
    "session.compaction.started",
    "session.compaction.delta",
    "session.compaction.ended",
    "session.compaction.failed",
];

/// The session a frame belongs to: `data.sessionID` (measured on every
/// session.* event) with `durable.aggregateID` as fallback (measured equal to
/// the session id on durable events).
fn frame_session(frame: &Value) -> Option<String> {
    frame
        .pointer("/data/sessionID")
        .and_then(Value::as_str)
        .or_else(|| frame.pointer("/durable/aggregateID").and_then(Value::as_str))
        .filter(|s| s.starts_with("ses_"))
        .map(str::to_string)
}

/// The typed error carried by `session.execution.failed`
/// (`Session.StructuredError` = `{type, message, status, response:{body},
/// retryAfterMs}`, schema extracted from the shipped 2.1.4 binary; the quota
/// shape matches the M3.0 spike's 402 body verbatim) → `RateLimited` when
/// quota-shaped, else `Failed`. Reuses `events::is_rate_limit_message` as the
/// wording-drift fallback (repo convention: typed first, sniff as backup).
fn execution_failure_events(frame: &Value) -> Vec<WorkerEvent> {
    let err = frame.pointer("/data/error");
    let msg = err
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .or_else(|| err.and_then(|e| e.get("type")).and_then(Value::as_str))
        .unwrap_or("session execution failed");
    let status = err.and_then(|e| e.get("status")).and_then(Value::as_i64);
    let etype = err
        .and_then(|e| e.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if status == Some(429)
        || etype.contains("quota")
        || etype.contains("rate")
        || is_rate_limit_message(msg)
    {
        // The window is machine-readable here (retryAfterMs) — a real
        // improvement over the text-sniffed ACP path. `Unknown` kind is the
        // honest cell (quota bucket not machine-labelled; Credit is wrong —
        // this one has a clock, Credit explicitly has none).
        let reset_at = err
            .and_then(|e| e.get("retryAfterMs"))
            .and_then(Value::as_u64)
            .and_then(|ms| i64::try_from(ms).ok())
            .and_then(chrono::Duration::try_milliseconds)
            .map(|d| chrono::Utc::now() + d);
        vec![WorkerEvent::RateLimited(RateLimit {
            kind: RateLimitKind::Unknown,
            reset_at,
            provider: ProviderId::new("opencode"),
            raw: Some(msg.to_string()),
        })]
    } else {
        vec![WorkerEvent::Failed(Failure {
            reason: summarize(msg),
            // 4xx won't fix itself; unknown/5xx will.
            retryable: !matches!(status, Some(s) if (400..500).contains(&s)),
        })]
    }
}

/// Translate one V2 event frame into the existing [`WorkerEvent`] vocabulary.
/// `tool_names` carries call-id → tool-name across frames: measured,
/// `session.tool.input.started` carries the `name` but no input, while
/// `session.tool.called` carries the `input` but no name — one ToolUsed is
/// assembled from both.
///
/// Turn identity: `session.execution.started` mints a fresh amux
/// [`TurnId`](amux_core::ids::TurnId) — opencode's `evt_` ids embed a
/// HEX-MILLISECOND timestamp plus random chars (measured:
/// `evt_17d06e93a001URzvK6D4b7JSX5`, whose prefix decodes to the frame's own
/// `created` ms) and contain Crockford-excluded letters, so they are NOT
/// ULIDs and cannot back a `TurnId`. Correlating this start with the turn's
/// OUTCOME is M3.3/M3.5's job via `GET /message` (D3), not this channel.
pub fn translate_v2_event(frame: &Value, tool_names: &mut ToolNameBook) -> Translated {
    let Some(kind) = frame.get("type").and_then(Value::as_str) else {
        return Translated::Malformed;
    };
    // Session-scoped arms must be routable; a session.* frame without a
    // session id is a shape we never measured → malformed (counted, logged).
    let scoped = |events: Vec<WorkerEvent>| match frame_session(frame) {
        Some(session) => Translated::Events {
            session: Some(session),
            events,
        },
        None => Translated::Malformed,
    };
    let delta_progress = |prefix: &str| {
        let text = frame
            .pointer("/data/delta")
            .and_then(Value::as_str)
            .unwrap_or("");
        if text.trim().is_empty() {
            return Translated::KnownNoop;
        }
        scoped(vec![WorkerEvent::Progress(ProgressReport {
            summary: format!("{prefix}{}", summarize(text)),
            tokens_used: None,
        })])
    };
    match kind {
        // Transport-level handshake — no session: broadcast to all workers.
        "server.connected" => Translated::Events {
            session: None,
            events: vec![WorkerEvent::Started],
        },
        "session.execution.started" => scoped(vec![WorkerEvent::TurnStarted {
            turn_id: TurnId::from_ulid(ulid::Ulid::new()),
        }]),
        // Text/thought chunks — the same vocabulary as ACP's
        // agent_message_chunk / agent_thought_chunk.
        "session.text.delta" => delta_progress(""),
        "session.reasoning.delta" => delta_progress("thinking: "),
        "session.tool.input.started" => {
            // Records the name; the ToolUsed fires on `session.tool.called`
            // (which carries the input). Announcement-only: no event, mirroring
            // ACP's rule that an inputless announcement is not a headline.
            if let (Some(id), Some(name)) = (
                frame.pointer("/data/id").and_then(Value::as_str),
                frame.pointer("/data/name").and_then(Value::as_str),
            ) {
                tool_names.record(id.to_string(), name.to_string());
            }
            Translated::KnownNoop
        }
        "session.tool.called" => {
            let id = frame.pointer("/data/id").and_then(Value::as_str);
            let tool = id
                .and_then(|id| tool_names.take(id))
                .unwrap_or_else(|| "tool".to_string());
            let input = frame.pointer("/data/input");
            let detail = match input {
                Some(v) if v.is_object() && !v.as_object().is_some_and(|o| o.is_empty()) => {
                    tool_detail(v)
                }
                _ => None,
            };
            scoped(vec![WorkerEvent::ToolUsed(ToolEvent { tool, detail })])
        }
        "session.tool.input.ended" => {
            // NOT terminal: measured order is input.started (name) →
            // input.ended (serialized input text) → tool.called (input
            // object). Dropping the pending name here broke the name→input
            // assembly — caught by the real-capture replay test. Keep it.
            Translated::KnownNoop
        }
        "session.tool.success" | "session.tool.failed" => {
            // Terminal tool frames: output belongs in logs and the
            // durable trace, never a second ToolUsed (ACP precedent). Drop
            // the pending name so the book stays bounded.
            if let Some(id) = frame.pointer("/data/id").and_then(Value::as_str) {
                tool_names.take(id);
            }
            Translated::KnownNoop
        }
        "session.usage.updated" => {
            let Some(tokens) = frame.pointer("/data/tokens") else {
                return Translated::KnownNoop;
            };
            let get = |path: &str| {
                tokens
                    .pointer(path)
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            };
            // Same sum ACP reports as totalTokens (measured: 5335 + 401 + 18
            // + 384 = 6138) — input + output + reasoning + cache read/write.
            let total = get("/input")
                + get("/output")
                + get("/reasoning")
                + get("/cache/read")
                + get("/cache/write");
            scoped(vec![WorkerEvent::Progress(ProgressReport {
                summary: "usage".to_string(),
                tokens_used: Some(total),
            })])
        }
        // D3: a turn OUTCOME is read from GET /api/session/{id}/message, never
        // derived from the volatile SSE stream — `execution.succeeded` is a
        // counted no-op (no `TurnCompleted` is ever derived from SSE).
        "session.execution.succeeded" => Translated::KnownNoop,
        // …but `execution.interrupted` is NOT an outcome: the header contract
        // maps it to `Progress` carrying the `reason` enum (user / shutdown /
        // superseded / inactivity), so the worker learns the run stopped
        // without the stream claiming how it ended. The payload shape is not
        // in the M3.2 capture (no interruption occurred) — `reason` is read
        // defensively and a missing one still dispatches.
        "session.execution.interrupted" => {
            let reason = frame
                .pointer("/data/reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            scoped(vec![WorkerEvent::Progress(ProgressReport {
                summary: summarize(&format!("interrupted: {reason}")),
                tokens_used: None,
            })])
        }
        "session.execution.failed" => scoped(execution_failure_events(frame)),
        _ if KNOWN_NOOP_KINDS.contains(&kind) => Translated::KnownNoop,
        _ => Translated::Unknown(kind.to_string()),
    }
}

/// Snapshot of the hub's counters (repo rule: a defect must be visible as a
/// counter a log sweep can read).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EventStats {
    /// Complete `data:` frames handled (malformed included).
    pub frames: u64,
    /// Heartbeat comment lines seen.
    pub heartbeats: u64,
    /// WorkerEvents actually handed to a worker channel.
    pub events_emitted: u64,
    /// Recognized kinds that deliberately emit nothing.
    pub known_noop: u64,
    /// Unknown kinds (drift detector).
    pub unknown_kind: u64,
    /// Unusable frames (bad JSON / missing sessionID) + dropped oversize frames.
    pub malformed: u64,
    /// Session frames with no registered route (or a dead channel).
    pub unrouted: u64,
    /// Events dropped for a lagging broadcast consumer (sum of missed counts).
    pub overflowed: u64,
    /// Stream disconnects (EOF, read error, idle silence, connect failure).
    pub gaps: u64,
    /// Successful event-stream connections.
    pub connects: u64,
}

/// What [`ServeEventHub::handle_frame`] did with a frame (test seam).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameOutcome {
    Routed(String),
    RoutedAll,
    Unrouted(String),
    KnownNoop,
    Unknown(String),
    Malformed,
}

/// How many distinct unknown kinds get a WARN before logging goes
/// count-only (bounded: a broken server must not grow the logged set).
const UNKNOWN_LOG_CAP: usize = 64;

struct RouteTable {
    map: HashMap<String, broadcast::Sender<WorkerEvent>>,
    /// Whether the SSE stream is currently up. Starts false: a session
    /// registered before the first connect (or during an outage) can have
    /// missed events, so registration while down owes a `/message` backfill —
    /// never a silent gap.
    stream_up: bool,
}

/// Bounded call-id → tool-name book (see [`translate_v2_event`]): one entry
/// per in-flight tool call, FIFO-evicted past [`TOOL_NAME_CAP`] so a
/// long-lived serve process cannot grow it without bound.
#[derive(Default)]
pub struct ToolNameBook {
    names: HashMap<String, String>,
    order: VecDeque<String>,
}

/// How many tool-call names one serve process remembers.
pub const TOOL_NAME_CAP: usize = 4096;

impl ToolNameBook {
    pub fn new() -> Self {
        Self {
            names: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn record(&mut self, id: String, name: String) {
        self.names.insert(id.clone(), name);
        self.order.push_back(id);
        while self.order.len() > TOOL_NAME_CAP {
            if let Some(old) = self.order.pop_front() {
                self.names.remove(&old);
            }
        }
    }

    pub fn take(&mut self, id: &str) -> Option<String> {
        self.names.remove(id)
    }
}

/// The fleet-wide event demux: ONE SSE reader per serve process (D1) fans
/// frames out to per-worker `tokio` broadcast channels, counts everything it
/// sees, and records every gap as a `/message` backfill owed (D3: the volatile
/// stream is progress-only; reconciliation reads the durable message list —
/// the `…/log?after=` cursor did NOT replay history in the M3.0 spike and is
/// deliberately not depended on anywhere here).
pub struct ServeEventHub {
    routes: Mutex<RouteTable>,
    tool_names: Mutex<ToolNameBook>,
    backfill: Mutex<HashSet<String>>,
    unknown_logged: Mutex<HashSet<String>>,
    malformed_logged: std::sync::atomic::AtomicBool,
    frames: AtomicU64,
    heartbeats: AtomicU64,
    events_emitted: AtomicU64,
    known_noop: AtomicU64,
    unknown_kind: AtomicU64,
    malformed: AtomicU64,
    unrouted: AtomicU64,
    overflowed: AtomicU64,
    gaps: AtomicU64,
    connects: AtomicU64,
}

/// Default per-worker broadcast capacity (matches the ACP event channels).
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

impl ServeEventHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            routes: Mutex::new(RouteTable {
                map: HashMap::new(),
                stream_up: false,
            }),
            tool_names: Mutex::new(ToolNameBook::new()),
            backfill: Mutex::new(HashSet::new()),
            unknown_logged: Mutex::new(HashSet::new()),
            malformed_logged: std::sync::atomic::AtomicBool::new(false),
            frames: AtomicU64::new(0),
            heartbeats: AtomicU64::new(0),
            events_emitted: AtomicU64::new(0),
            known_noop: AtomicU64::new(0),
            unknown_kind: AtomicU64::new(0),
            malformed: AtomicU64::new(0),
            unrouted: AtomicU64::new(0),
            overflowed: AtomicU64::new(0),
            gaps: AtomicU64::new(0),
            connects: AtomicU64::new(0),
        })
    }

    /// Register (if new) and subscribe to a worker's event channel. The
    /// returned receiver transparently detects lag (see
    /// [`ServeEventReceiver::recv`]). Registering while the stream is down
    /// records a `/message` backfill owed for that session.
    ///
    /// (An associated function, not a method: the receiver must hold an
    /// `Arc<ServeEventHub>` to report lag, and `self: &Arc<Self>` is not a
    /// valid receiver type.)
    pub fn subscribe(hub: &Arc<Self>, session_id: &str) -> ServeEventReceiver {
        Self::subscribe_with_capacity(hub, session_id, EVENT_CHANNEL_CAPACITY)
    }

    /// As [`subscribe`](Self::subscribe) with an explicit channel capacity
    /// (tests drive real broadcast lag with a capacity of 1).
    pub fn subscribe_with_capacity(
        hub: &Arc<Self>,
        session_id: &str,
        capacity: usize,
    ) -> ServeEventReceiver {
        let sender = {
            let mut table = hub.routes.lock().unwrap();
            let sender = table
                .map
                .entry(session_id.to_string())
                .or_insert_with(|| broadcast::channel(capacity.max(1)).0)
                .clone();
            if !table.stream_up {
                hub.backfill.lock().unwrap().insert(session_id.to_string());
            }
            sender
        };
        ServeEventReceiver {
            inner: sender.subscribe(),
            hub: Arc::clone(hub),
            session: session_id.to_string(),
        }
    }

    pub fn unsubscribe(&self, session_id: &str) {
        self.routes.lock().unwrap().map.remove(session_id);
    }

    pub fn stats(&self) -> EventStats {
        EventStats {
            frames: self.frames.load(Ordering::Relaxed),
            heartbeats: self.heartbeats.load(Ordering::Relaxed),
            events_emitted: self.events_emitted.load(Ordering::Relaxed),
            known_noop: self.known_noop.load(Ordering::Relaxed),
            unknown_kind: self.unknown_kind.load(Ordering::Relaxed),
            malformed: self.malformed.load(Ordering::Relaxed),
            unrouted: self.unrouted.load(Ordering::Relaxed),
            overflowed: self.overflowed.load(Ordering::Relaxed),
            gaps: self.gaps.load(Ordering::Relaxed),
            connects: self.connects.load(Ordering::Relaxed),
        }
    }

    /// Sessions owed a durable backfill read (`GET /api/session/{id}/message`,
    /// D3) — snapshot without claiming.
    pub fn backfill_owed(&self) -> Vec<String> {
        let set = self.backfill.lock().unwrap();
        let mut v: Vec<String> = set.iter().cloned().collect();
        v.sort();
        v
    }

    /// Claim the backfill set (M3.3 drains it after fetching `/message`).
    pub fn take_backfill_owed(&self) -> Vec<String> {
        let mut set = self.backfill.lock().unwrap();
        let mut v: Vec<String> = set.drain().collect();
        v.sort();
        v
    }

    pub fn note_heartbeat(&self) {
        self.heartbeats.fetch_add(1, Ordering::Relaxed);
    }

    /// Dispatch one complete frame payload (JSON) — counts and routes; never
    /// panics on input (repo rule: malformed input stays silent as an EVENT
    /// but is always COUNTED and self-announced).
    pub fn handle_frame(&self, payload: &str) -> FrameOutcome {
        self.frames.fetch_add(1, Ordering::Relaxed);
        let frame: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => return self.note_malformed(payload),
        };
        let translated = {
            let mut book = self.tool_names.lock().unwrap();
            translate_v2_event(&frame, &mut book)
        };
        match translated {
            Translated::Malformed => self.note_malformed(payload),
            Translated::KnownNoop => {
                self.known_noop.fetch_add(1, Ordering::Relaxed);
                FrameOutcome::KnownNoop
            }
            Translated::Unknown(kind) => {
                self.unknown_kind.fetch_add(1, Ordering::Relaxed);
                let mut logged = self.unknown_logged.lock().unwrap();
                if logged.len() < UNKNOWN_LOG_CAP && logged.insert(kind.clone()) {
                    tracing::warn!(
                        kind,
                        verdict = "unknown V2 event kind — counted, no event emitted; re-measure the event surface on opencode upgrade",
                        "serve event: unknown kind"
                    );
                }
                FrameOutcome::Unknown(kind)
            }
            Translated::Events { session, events } => {
                let table = self.routes.lock().unwrap();
                let send = |tx: &broadcast::Sender<WorkerEvent>| {
                    let mut delivered = true;
                    for event in &events {
                        // Send EVERY event of the frame even if an earlier
                        // send failed — `.all()` would short-circuit and
                        // silently drop the rest of the frame.
                        delivered &= tx.send(event.clone()).is_ok();
                    }
                    delivered
                };
                let n = events.len() as u64;
                match session {
                    Some(sid) => match table.map.get(&sid) {
                        Some(tx) => {
                            if send(tx) {
                                self.events_emitted.fetch_add(n, Ordering::Relaxed);
                                FrameOutcome::Routed(sid)
                            } else {
                                // Receiver dropped without unsubscribing.
                                self.unrouted.fetch_add(1, Ordering::Relaxed);
                                FrameOutcome::Unrouted(sid)
                            }
                        }
                        None => {
                            self.unrouted.fetch_add(1, Ordering::Relaxed);
                            FrameOutcome::Unrouted(sid)
                        }
                    },
                    None => {
                        let mut any = false;
                        for tx in table.map.values() {
                            any |= send(tx);
                        }
                        if any {
                            self.events_emitted
                                .fetch_add(n * table.map.len() as u64, Ordering::Relaxed);
                        }
                        FrameOutcome::RoutedAll
                    }
                }
            }
        }
    }

    /// An unusable frame: counted, and logged ONCE (first occurrence) with a
    /// verdict so a sweep sees it without the log flooding.
    fn note_malformed(&self, payload: &str) -> FrameOutcome {
        self.malformed.fetch_add(1, Ordering::Relaxed);
        if !self.malformed_logged.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                excerpt = %summarize(payload),
                verdict = "unusable frame (bad JSON or missing sessionID) — counted, no event emitted",
                "serve event: unusable frame"
            );
        }
        FrameOutcome::Malformed
    }

    /// The SSE stream went away (EOF, read error, idle silence, connect
    /// failure). EVERY registered session is marked owed a `/message`
    /// backfill — the measured contract says events during disconnection are
    /// missed, and the volatile stream can never prove which ones.
    pub fn note_stream_gap(&self, reason: &str) {
        self.gaps.fetch_add(1, Ordering::Relaxed);
        let sids = {
            let mut table = self.routes.lock().unwrap();
            table.stream_up = false;
            table.map.keys().cloned().collect::<Vec<_>>()
        };
        {
            let mut backfill = self.backfill.lock().unwrap();
            for sid in &sids {
                backfill.insert(sid.clone());
            }
        }
        tracing::error!(
            sessions = sids.len(),
            reason,
            verdict = "events during disconnection are unrecoverable from the volatile SSE stream; backfill owed: GET /api/session/{id}/message per session (D3; the …/log?after= cursor did not replay in the M3.0 spike)",
            "serve event stream gap"
        );
    }

    /// The stream is (re)connected; registrations from here on are covered.
    pub fn note_stream_connected(&self) {
        self.connects.fetch_add(1, Ordering::Relaxed);
        self.routes.lock().unwrap().stream_up = true;
    }

    /// A lagging worker consumer: the broadcast channel dropped `missed`
    /// events for it (tokio broadcast overwrites oldest on overflow and tells
    /// the RECEIVER via `Lagged`). Counted, recorded as backfill owed, logged
    /// with a verdict.
    pub fn note_lagged(&self, session_id: &str, missed: u64) {
        self.overflowed.fetch_add(missed, Ordering::Relaxed);
        self.backfill
            .lock()
            .unwrap()
            .insert(session_id.to_string());
        tracing::error!(
            session = session_id,
            missed,
            verdict = "slow consumer dropped events; backfill owed: GET /api/session/{id}/message (D3)",
            "serve event: consumer lagged"
        );
    }
}

/// A worker's view of its event channel. Wraps the broadcast receiver so
/// `Lagged` is DETECTED HERE (the sender cannot see receiver lag), logged
/// with a verdict, and recorded as a `/message` backfill owed — detection and
/// recording are M3.2; the actual backfill fetch is M3.3.
pub struct ServeEventReceiver {
    inner: broadcast::Receiver<WorkerEvent>,
    hub: Arc<ServeEventHub>,
    session: String,
}

impl ServeEventReceiver {
    /// Next event; on lag, note it (log + backfill record) and keep going —
    /// a laggy consumer degrades to a backfill, never to a silent gap.
    pub async fn recv(&mut self) -> Option<WorkerEvent> {
        loop {
            match self.inner.recv().await {
                Ok(event) => return Some(event),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    self.hub.note_lagged(&self.session, missed);
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    /// Non-blocking variant with the same lag detection.
    pub fn try_recv(&mut self) -> std::result::Result<WorkerEvent, broadcast::error::TryRecvError> {
        loop {
            match self.inner.try_recv() {
                Err(broadcast::error::TryRecvError::Lagged(missed)) => {
                    self.hub.note_lagged(&self.session, missed);
                }
                other => return other,
            }
        }
    }
}

/// Liveness bound for the stream: serve sends `: heartbeat` comments
/// (measured); their cadence is NOT measured, so 90s is a deliberately
/// generous design choice — silence that long means a dead half-open
/// connection, which must become a logged gap + reconnect, never a silent
/// hang.
pub const EVENT_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

impl ServeClient {
    /// Open the fleet-wide `GET /api/event` stream (`text/event-stream`,
    /// measured). Uses a DEDICATED reqwest client on purpose: the shared
    /// client carries a 30s TOTAL request timeout, which reqwest applies until
    /// the response body finishes — it would sever a healthy SSE stream at
    /// 30s. Only the connect timeout is bounded here; stream liveness is
    /// guarded by [`EVENT_IDLE_TIMEOUT`] inside the pump.
    pub async fn open_event_stream(&self) -> Result<reqwest::Response> {
        let endpoint = "/api/event";
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| {
                ProtocolError::Transport(format!("serve: build stream client for {endpoint}: {e}"))
            })?;
        let url = format!("{}{}", self.base, endpoint);
        let resp = http
            .get(&url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .basic_auth(AUTH_USER, Some(&self.password))
            .send()
            .await
            .map_err(|e| transport_failure("GET", endpoint, &e))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let body = resp.text().await.unwrap_or_default();
            return Err(serve_failure("GET", endpoint, status, &body));
        }
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !ct.starts_with("text/event-stream") {
            return Err(ProtocolError::Transport(format!(
                "serve GET {endpoint} -> HTTP {status}: content-type {ct:?}, expected text/event-stream"
            )));
        }
        Ok(resp)
    }
}

/// Read one event-stream connection to its end: parse frames, dispatch them
/// to the hub, count heartbeats. Every way out (EOF, read error, idle
/// silence) notes a stream gap first — the measured contract says events
/// during disconnection are missed, so each end records a backfill owed.
/// Connect failures note the gap too (the stream was unavailable).
pub async fn read_event_stream_once(client: &ServeClient, hub: &ServeEventHub) -> Result<()> {
    let mut resp = match client.open_event_stream().await {
        Ok(r) => r,
        Err(e) => {
            hub.note_stream_gap(&format!("event stream unavailable: {e}"));
            return Err(e);
        }
    };
    hub.note_stream_connected();
    let mut parser = SseParser::default();
    loop {
        let step = tokio::time::timeout(EVENT_IDLE_TIMEOUT, resp.chunk()).await;
        match step {
            Ok(Ok(Some(bytes))) => {
                for frame in parser.push(&bytes) {
                    match frame {
                        SseFrame::Data(payload) => {
                            hub.handle_frame(&payload);
                        }
                        SseFrame::Heartbeat => hub.note_heartbeat(),
                        SseFrame::Oversize => {
                            hub.note_malformed("<frame exceeds MAX_FRAME_BYTES>");
                        }
                    }
                }
            }
            Ok(Ok(None)) => {
                hub.note_stream_gap("serve closed the event stream (EOF)");
                return Ok(());
            }
            Ok(Err(e)) => {
                hub.note_stream_gap(&format!("event stream read failed: {e}"));
                return Err(transport_failure("GET", "/api/event", &e));
            }
            Err(_) => {
                hub.note_stream_gap(&format!(
                    "no bytes for {EVENT_IDLE_TIMEOUT:?} (heartbeat silence)"
                ));
                return Err(ProtocolError::Transport(format!(
                    "serve GET /api/event: idle for {EVENT_IDLE_TIMEOUT:?} — heartbeat silence; reconnecting"
                )));
            }
        }
    }
}

/// The ONE long-lived reader for a serve process (D1: one reader for the
/// fleet, never one per worker). Reconnects with capped exponential backoff
/// (1s → 30s, reset after a healthy read); every disconnect is a logged gap.
/// Stopping or dropping the reader aborts the task (the AcpSession pattern).
pub struct EventReader {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

impl EventReader {
    pub fn spawn(client: ServeClient, hub: Arc<ServeEventHub>) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&stop);
        let handle = tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            while !flag.load(Ordering::SeqCst) {
                match read_event_stream_once(&client, &hub).await {
                    // Healthy stream that ended (server closed it): retry
                    // promptly — the gap is already noted inside.
                    Ok(()) => backoff = Duration::from_secs(1),
                    // Connect/read failure: gap already noted inside; log the
                    // retry decision with its delay.
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            backoff_s = backoff.as_secs(),
                            "serve event reader: reconnecting after failure"
                        );
                    }
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        });
        Self { stop, handle }
    }

    /// Ask the reader to stop after its current step; the next loop
    /// iteration exits. `Drop` aborts immediately as a backstop.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for EventReader {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

// ---------------------------------------------------------------------------
// Tests — a fixture serve (axum on 127.0.0.1:0) drives every measured shape
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path as RoutePath, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use base64::Engine;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    const PW: &str = "test-password-43-chars-test-password-43";
    const MODEL: &str = "opencode/deepseek-v4.1-flash-free";
    const CWD: &str = "/tmp/serve-fixture-ws";

    /// The measured quota body, verbatim (M3.0 spike).
    const QUOTA_BODY: &str = r#"{"type":"provider.quota","status":402,"message":"You have exceeded your monthly quota","retryAfterMs":429981254}"#;
    /// The measured `SessionBusyErrorEncoded` shape (openapi), fixture status 409.
    const BUSY_BODY: &str = r#"{"_tag":"SessionBusyError","sessionID":"ses_busy","message":"session is busy"}"#;
    /// The refusal the **M3.5 live smoke** hit (2026-10-30, opencode 2.2.3):
    /// the free-model quota. Kept verbatim because it is the case that proved
    /// `provider.quota` alone was too narrow a classifier.
    const FREE_LIMIT_BODY: &str = r#"{"type":"provider.free-limit","status":429,"message":"Free usage limit reached. Connect a provider or subscribe to OpenCode Go to keep going, or switch models.","response":{"body":"{\"type\":\"error\",\"error\":{\"type\":\"FreeUsageLimitError\"}}"}}"#;

    fn expected_auth() -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("opencode:{PW}"))
        )
    }

    /// What the fixture observed from the client.
    #[derive(Default)]
    struct Seen {
        create_body: Mutex<Option<Value>>,
        prompt_bodies: Mutex<Vec<(String, Value)>>,
        auth_headers: Mutex<Vec<String>>,
        /// (request id, decision) for every permission reply the fixture
        /// accepted — the wire's own `decision` field, not our enum.
        permission_replies: Mutex<Vec<(String, String)>>,
        /// Set when the fixture's background "turn" would have ended. A prompt
        /// admission must return while this is still false.
        turn_ended: AtomicBool,
    }

    #[derive(Clone, Copy)]
    enum PromptMode {
        Admit,
        Quota,
        Busy,
        Error500,
    }

    #[derive(Clone)]
    struct Fixture {
        seen: Arc<Seen>,
        mode: PromptMode,
    }

    impl Fixture {
        fn authorized(&self, headers: &HeaderMap) -> bool {
            let got = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            got == expected_auth()
        }
    }

    async fn create_session(
        State(fx): State<Fixture>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Response {
        fx.seen
            .auth_headers
            .lock()
            .unwrap()
            .push(headers.get(axum::http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string());
        if !fx.authorized(&headers) {
            return (StatusCode::UNAUTHORIZED, Json(json!({"message": "unauthorized"})))
                .into_response();
        }
        *fx.seen.create_body.lock().unwrap() = Some(body.clone());
        Json(json!({
            "data": {"id": "ses_fixture", "projectID": "prj_1", "cost": 0,
                     "location": body.get("location")}
        }))
        .into_response()
    }

    async fn prompt(
        State(fx): State<Fixture>,
        RoutePath(id): RoutePath<String>,
        Json(body): Json<Value>,
    ) -> Response {
        fx.seen.prompt_bodies.lock().unwrap().push((id, body));
        match fx.mode {
            PromptMode::Admit => {
                // The agent loop keeps running in the background: a real turn
                // end would only arrive much later. Model that as a flag that
                // flips only after a long sleep, so the test can assert the
                // admission returned BEFORE any turn end existed.
                let seen = fx.seen.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(600)).await;
                    seen.turn_ended.store(true, Ordering::SeqCst);
                });
                Json(json!({"data": {"id": "msg_admitted", "type": "user",
                                     "sessionID": "ses_fixture", "text": "do the thing"}}))
                    .into_response()
            }
            PromptMode::Quota => (
                StatusCode::PAYMENT_REQUIRED,
                Json(serde_json::from_str::<Value>(QUOTA_BODY).unwrap()),
            )
                .into_response(),
            PromptMode::Busy => (
                StatusCode::CONFLICT,
                Json(serde_json::from_str::<Value>(BUSY_BODY).unwrap()),
            )
                .into_response(),
            PromptMode::Error500 => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"message": "boom"})),
            )
                .into_response(),
        }
    }

    /// MEASURED `/message` body — opencode 2.2.1, dedicated serve,
    /// 2026-10-29: `{data: [...], cursor}`, `data` **newest-first**, rows
    /// typed `user` / `assistant` / `idle`. The interrupted turn's outcome is
    /// the structured `outcome` field on the `idle` row. There are no
    /// `info`/`parts` wrappers — that shape existed only in an earlier
    /// hand-written fixture and in no capture.
    async fn messages(RoutePath(id): RoutePath<String>) -> Response {
        if !matches!(id.as_str(), "ses_fixture" | "ses_busy") {
            return (StatusCode::NOT_FOUND, Json(json!({"message": "no such session"})))
                .into_response();
        }
        Json(json!({
            "data": [
                {"id": "msg_idle", "time": {"created": 5}, "type": "idle",
                 "outcome": "interrupted"},
                {"id": "msg_steered", "time": {"created": 4}, "type": "assistant",
                 "content": [{"type": "text", "text": "STEERED-OK"}],
                 "finish": "stop"},
                {"id": "msg_essay", "time": {"created": 3}, "type": "assistant",
                 "content": [{"type": "text", "text": "essay…"}],
                 "finish": "tool_calls"},
                {"id": "msg_steer", "time": {"created": 2}, "type": "user",
                 "text": "STOP the essay. Reply with exactly: STEERED-OK"},
                {"id": "msg_task", "time": {"created": 1}, "type": "user",
                 "text": "Write a 700 word essay…"}
            ],
            "cursor": "cur_fixture"
        }))
        .into_response()
    }

    /// MEASURED `SessionInterruptResponse`: `{interrupted: bool}` at the top
    /// level, no `data:` wrapper. `ses_fixture` has a turn running, `ses_idle`
    /// has none — so `false` is reachable as a normal answer.
    async fn interrupt(RoutePath(id): RoutePath<String>) -> Response {
        match id.as_str() {
            "ses_fixture" => Json(json!({"interrupted": true})).into_response(),
            "ses_idle" => Json(json!({"interrupted": false})).into_response(),
            _ => (
                StatusCode::NOT_FOUND,
                Json(json!({"_tag": "SessionNotFoundError"})),
            )
                .into_response(),
        }
    }

    /// A MEASURED `Permission.Request` (shape from the captured openapi:
    /// `{id, sessionID, action, resources[], save[], metadata, source, message}`).
    const PERMISSION_REQUEST: &str = r#"{"data":[{"id":"per_189a2d347001perm01","sessionID":"ses_fixture","action":"write","resources":["/tmp/ws/perm.txt"],"message":"Write /tmp/ws/perm.txt?","source":{"type":"tool","messageID":"msg_189a2d347001","id":"call_00_perm"}}]}"#;

    /// MEASURED: `GET …/permission` → `{data: Permission.Request[]}`.
    async fn permissions(RoutePath(id): RoutePath<String>) -> Response {
        if id == "ses_fixture" {
            Json(serde_json::from_str::<Value>(PERMISSION_REQUEST).unwrap()).into_response()
        } else {
            Json(json!({"data": []})).into_response()
        }
    }

    /// MEASURED: `POST …/permission/{requestID}/reply` answers **204 No
    /// Content** — an empty body — and an unknown id answers 404 with the
    /// typed `PermissionNotFoundError {_tag, requestID, message}`.
    async fn reply_permission(
        State(fx): State<Fixture>,
        RoutePath((id, rid)): RoutePath<(String, String)>,
        Json(body): Json<Value>,
    ) -> Response {
        if id != "ses_fixture" {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"_tag": "SessionNotFoundError"})),
            )
                .into_response();
        }
        if rid != "per_189a2d347001perm01" {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "_tag": "PermissionNotFoundError",
                    "requestID": rid,
                    "message": "permission request not found"
                })),
            )
                .into_response();
        }
        fx.seen
            .permission_replies
            .lock()
            .unwrap()
            .push((rid, body["decision"].as_str().unwrap_or("").to_string()));
        StatusCode::NO_CONTENT.into_response()
    }

    /// Spin a fixture serve; returns (base_url, observed state).
    async fn fixture(mode: PromptMode) -> (String, Arc<Seen>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Seen::default());
        let state = Fixture {
            seen: seen.clone(),
            mode,
        };
        let app = Router::new()
            .route("/api/session", post(create_session))
            .route("/api/session/{id}/prompt", post(prompt))
            .route("/api/session/{id}/message", get(messages))
            .route("/api/session/{id}/interrupt", post(interrupt))
            .route("/api/session/{id}/permission", get(permissions))
            .route(
                "/api/session/{id}/permission/{rid}/reply",
                post(reply_permission),
            )
            .with_state(state);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), seen)
    }

    #[tokio::test]
    async fn connect_open_and_prompt_admit_without_awaiting_the_turn() {
        let (base, seen) = fixture(PromptMode::Admit).await;
        let client = ServeClient::connect(&base, PW).unwrap();
        let (session, opened) =
            ServeSession::open(client, Path::new(CWD), Some(MODEL), None)
                .await
                .unwrap();
        assert_eq!(opened.session_id, "ses_fixture");
        assert!(!opened.resumed);
        assert_eq!(session.session_id(), "ses_fixture");

        // The admission returns while the fixture's turn is still running —
        // prompt is async; awaiting turn completion would hang past this
        // timeout (the turn "ends" only after 600s in the fixture).
        let admitted = tokio::time::timeout(
            Duration::from_secs(5),
            session.prompt_async("do the thing", Delivery::Steer),
        )
        .await
        .expect("prompt_async must not wait for the turn to end")
        .unwrap();
        assert_eq!(admitted, "msg_admitted");
        assert!(
            !seen.turn_ended.load(Ordering::SeqCst),
            "the admission returned while no turn end had been emitted"
        );

        // Every request carried the measured Basic auth.
        let headers = seen.auth_headers.lock().unwrap();
        assert!(!headers.is_empty());
        for h in headers.iter() {
            assert_eq!(h, &expected_auth());
        }
    }

    #[tokio::test]
    async fn the_create_session_body_carries_directory_and_model_ref() {
        let (base, seen) = fixture(PromptMode::Admit).await;
        let client = ServeClient::connect(&base, PW).unwrap();
        ServeSession::open(client, Path::new(CWD), Some(MODEL), None)
            .await
            .unwrap();
        let body = seen.create_body.lock().unwrap().clone().unwrap();
        assert_eq!(body["location"]["directory"], CWD);
        // Model.Ref is {providerID, id} — NOT {providerID, modelID}.
        assert_eq!(
            body["model"],
            json!({"providerID": "opencode", "id": "deepseek-v4.1-flash-free"})
        );
    }

    #[test]
    fn model_ref_encodes_the_measured_shape() {
        assert_eq!(
            model_ref("opencode/deepseek-v4.1-flash-free"),
            json!({"providerID": "opencode", "id": "deepseek-v4.1-flash-free"})
        );
        assert_eq!(
            model_ref("opencode-go/deepseek-v4.1-flash"),
            json!({"providerID": "opencode-go", "id": "deepseek-v4.1-flash"})
        );
        // A bare model id stays under the opencode provider.
        assert_eq!(
            model_ref("deepseek-v4.1-flash-free"),
            json!({"providerID": "opencode", "id": "deepseek-v4.1-flash-free"})
        );
    }

    #[tokio::test]
    async fn a_quota_error_body_maps_to_quota_with_its_retry_hint() {
        let (base, _) = fixture(PromptMode::Quota).await;
        let client = ServeClient::connect(&base, PW).unwrap();
        let (session, _) =
            ServeSession::open(client, Path::new(CWD), Some(MODEL), None)
                .await
                .unwrap();
        let err = session
            .prompt_async("anything", Delivery::Queue)
            .await
            .unwrap_err();
        // The typed mapping…
        assert_eq!(
            ServeError::classify(402, "/api/session/ses_fixture/prompt", QUOTA_BODY),
            ServeError::Quota {
                retry_after_ms: Some(429981254)
            }
        );
        // …and the ProtocolError names method, endpoint and status.
        let msg = err.to_string();
        assert!(msg.contains("POST /api/session/ses_fixture/prompt"), "{msg}");
        assert!(msg.contains("HTTP 402"), "{msg}");
        assert!(msg.contains("provider.quota"), "{msg}");
        assert!(msg.contains("retryAfterMs=429981254"), "{msg}");
    }

    #[tokio::test]
    async fn a_session_busy_error_body_maps_to_busy() {
        let (base, _) = fixture(PromptMode::Busy).await;
        let client = ServeClient::connect(&base, PW).unwrap();
        let (session, _) =
            ServeSession::open(client, Path::new(CWD), Some(MODEL), None)
                .await
                .unwrap();
        let err = session
            .prompt_async("anything", Delivery::Steer)
            .await
            .unwrap_err();
        assert_eq!(
            ServeError::classify(409, "/api/session/ses_busy/prompt", BUSY_BODY),
            ServeError::Busy {
                session_id: Some("ses_busy".into())
            }
        );
        // Busy is a rejection, not a transport failure.
        assert!(
            matches!(err, ProtocolError::Rejected(_)),
            "busy must be Rejected: {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("POST /api/session/ses_fixture/prompt"), "{msg}");
        assert!(msg.contains("HTTP 409"), "{msg}");
        assert!(msg.contains("SessionBusyError"), "{msg}");
    }

    #[tokio::test]
    async fn a_500_with_no_tag_maps_to_other_naming_endpoint_and_status() {
        let (base, _) = fixture(PromptMode::Error500).await;
        let client = ServeClient::connect(&base, PW).unwrap();
        let (session, _) =
            ServeSession::open(client, Path::new(CWD), Some(MODEL), None)
                .await
                .unwrap();
        let err = session
            .prompt_async("anything", Delivery::Queue)
            .await
            .unwrap_err();
        assert_eq!(
            ServeError::classify(500, "/api/session/ses_fixture/prompt", r#"{"message":"boom"}"#),
            ServeError::Other {
                status: 500,
                endpoint: "/api/session/ses_fixture/prompt".into(),
                body: r#"{"message":"boom"}"#.into()
            }
        );
        let msg = err.to_string();
        assert!(msg.contains("POST /api/session/ses_fixture/prompt"), "{msg}");
        assert!(msg.contains("HTTP 500"), "{msg}");
        assert!(msg.contains("boom"), "{msg}");
    }

    #[tokio::test]
    async fn the_messages_reader_returns_the_durable_history() {
        let (base, _) = fixture(PromptMode::Admit).await;
        let client = ServeClient::connect(&base, PW).unwrap();
        let (session, _) =
            ServeSession::open(client, Path::new(CWD), Some(MODEL), None)
                .await
                .unwrap();
        let history = session.messages().await.unwrap();
        let rows = history["data"].as_array().unwrap();
        assert_eq!(rows.len(), 5);
        // MEASURED shape (opencode 2.2.1): rows are newest-first and an
        // interrupted turn's outcome is a STRUCTURED field on the leading
        // `idle` row. This is the reconciliation path, so the reader must pass
        // the envelope through untouched.
        assert_eq!(rows[0]["type"], "idle");
        assert_eq!(rows[0]["outcome"], "interrupted");
        assert_eq!(
            history["cursor"], "cur_fixture",
            "the envelope's cursor rides along with the data"
        );
    }

    #[tokio::test]
    async fn a_live_resume_ref_continues_the_conversation() {
        let (base, seen) = fixture(PromptMode::Admit).await;
        let client = ServeClient::connect(&base, PW).unwrap();
        let (_, opened) = ServeSession::open(
            client,
            Path::new(CWD),
            Some(MODEL),
            Some("ses_fixture"),
        )
        .await
        .unwrap();
        assert!(opened.resumed);
        assert_eq!(opened.session_id, "ses_fixture");
        assert_eq!(opened.resume_dropped, None);
        // Resumed: no create call was made.
        assert!(seen.create_body.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn a_dead_resume_ref_is_dropped_and_a_fresh_session_created() {
        let (base, seen) = fixture(PromptMode::Admit).await;
        let client = ServeClient::connect(&base, PW).unwrap();
        let (_, opened) = ServeSession::open(
            client,
            Path::new(CWD),
            Some(MODEL),
            Some("ses_gone"),
        )
        .await
        .unwrap();
        assert!(!opened.resumed);
        assert_eq!(opened.resume_dropped.as_deref(), Some("ses_gone"));
        assert_eq!(opened.session_id, "ses_fixture");
        assert!(seen.create_body.lock().unwrap().is_some());
    }

    #[test]
    fn service_json_password_parse_is_tolerant_and_strict_where_it_matters() {
        assert_eq!(
            password_from_service_json(r#"{"password":"pw1"}"#),
            Some("pw1".into())
        );
        assert_eq!(
            password_from_service_json("{ not json"),
            None,
            "unparseable file is a missing password, not a panic"
        );
        assert_eq!(password_from_service_json(r#"{"password":""}"#), None);
        assert_eq!(password_from_service_json(r#"{}"#), None);
    }

    #[test]
    fn delivery_serializes_to_the_measured_strings() {
        assert_eq!(Delivery::Steer.as_str(), "steer");
        assert_eq!(Delivery::Queue.as_str(), "queue");
    }

    // -----------------------------------------------------------------------
    // M3.2 — the SSE event channel
    // -----------------------------------------------------------------------

    /// The SSE wire bytes the fixture serve emits, written by hand (the exact
    /// format measured on /api/event: `data: <json>\n\n` frames, `: …`
    /// heartbeat comments). One frame per known kind, then an unknown kind
    /// and a malformed JSON frame — the required sequence in one stream.
    const SSE_SCRIPT: &str = "data: {\"id\":\"evt_1\",\"type\":\"server.connected\",\"data\":{}}\n\n\
        : heartbeat\n\n\
        data: {\"id\":\"evt_2\",\"type\":\"session.execution.started\",\"data\":{\"sessionID\":\"ses_sse\"}}\n\n\
        data: {\"id\":\"evt_3\",\"type\":\"session.text.delta\",\"data\":{\"sessionID\":\"ses_sse\",\"delta\":\"hello world\"}}\n\n\
        data: {\"id\":\"evt_4\",\"type\":\"session.reasoning.delta\",\"data\":{\"sessionID\":\"ses_sse\",\"delta\":\"thinking hard\"}}\n\n\
        data: {\"id\":\"evt_5\",\"type\":\"session.tool.input.started\",\"data\":{\"sessionID\":\"ses_sse\",\"id\":\"call_1\",\"name\":\"write\"}}\n\n\
        data: {\"id\":\"evt_6\",\"type\":\"session.tool.called\",\"data\":{\"sessionID\":\"ses_sse\",\"id\":\"call_1\",\"input\":{\"path\":\"/tmp/ws/hello.txt\",\"content\":\"hi\"}}}\n\n\
        data: {\"id\":\"evt_7\",\"type\":\"session.execution.succeeded\",\"data\":{\"sessionID\":\"ses_sse\"}}\n\n\
        data: {\"id\":\"evt_8\",\"type\":\"session.usage.updated\",\"data\":{\"sessionID\":\"ses_sse\",\"cost\":0,\"tokens\":{\"input\":100,\"output\":50,\"reasoning\":0,\"cache\":{\"read\":10,\"write\":0}}}}\n\n\
        data: {\"id\":\"evt_9\",\"type\":\"session.execution.failed\",\"data\":{\"sessionID\":\"ses_sse\",\"error\":{\"type\":\"provider.quota\",\"status\":402,\"message\":\"You have exceeded your monthly quota\",\"retryAfterMs\":429981254}}}\n\n\
        data: {\"id\":\"evt_10\",\"type\":\"session.mystery.thing\",\"data\":{\"sessionID\":\"ses_sse\"}}\n\n\
        data: {not json\n\n";

    /// Fixture event stream: Basic-auth checked, then the raw SSE script as a
    /// `text/event-stream` body that closes after emitting (so
    /// `read_event_stream_once` ends on EOF and records the gap).
    async fn sse_fixture(script: &'static str) -> (String, Arc<std::sync::atomic::AtomicBool>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let authorized = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&authorized);
        let app = axum::Router::new().route(
            "/api/event",
            axum::routing::get(move |headers: HeaderMap| {
                let flag = Arc::clone(&flag);
                async move {
                    let got = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if got != expected_auth() {
                        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
                    }
                    flag.store(true, Ordering::SeqCst);
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        script,
                    )
                        .into_response()
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), authorized)
    }

    /// The full measured sequence: one WorkerEvent sequence for the known
    /// kinds, NOTHING for heartbeat / execution.succeeded (D3) / unknown /
    /// malformed — but all of them COUNTED — plus the demux proof (a second
    /// registered session sees only the transport-level Started) and the
    /// gap-recording proof (fixture EOF marks every session backfill-owed).
    #[tokio::test]
    async fn the_event_stream_translates_the_measured_sequence_without_emitting_junk() {
        let (base, authorized) = sse_fixture(SSE_SCRIPT).await;
        let client = ServeClient::connect(&base, PW).unwrap();
        let hub = ServeEventHub::new();
        let mut rx = ServeEventHub::subscribe(&hub, "ses_sse");
        let mut rx_other = ServeEventHub::subscribe(&hub, "ses_other");
        // Simulate M3.3 having fetched the pre-connect backfill, so anything
        // owed AFTER the read is attributable to the read's gap handling.
        assert_eq!(hub.take_backfill_owed().len(), 2, "pre-connect owed");

        read_event_stream_once(&client, &hub)
            .await
            .expect("fixture stream reads to EOF as Ok");
        assert!(
            authorized.load(Ordering::SeqCst),
            "the stream request must carry the same Basic auth"
        );

        // The observed WorkerEvent sequence, in order.
        let mut got = Vec::new();
        while let Ok(e) = rx.try_recv() {
            got.push(e);
        }
        assert_eq!(got.len(), 7, "sequence: {got:?}");
        assert_eq!(got[0], WorkerEvent::Started);
        assert!(matches!(&got[1], WorkerEvent::TurnStarted { .. }), "{got:?}");
        assert_eq!(
            got[2],
            WorkerEvent::Progress(ProgressReport {
                summary: "hello world".into(),
                tokens_used: None,
            })
        );
        assert_eq!(
            got[3],
            WorkerEvent::Progress(ProgressReport {
                summary: "thinking: thinking hard".into(),
                tokens_used: None,
            })
        );
        // The tool name came from tool.input.started, the detail from
        // tool.called — assembled across frames.
        assert_eq!(
            got[4],
            WorkerEvent::ToolUsed(ToolEvent {
                tool: "write".into(),
                detail: Some("/tmp/ws/hello.txt".into()),
            })
        );
        assert_eq!(
            got[5],
            WorkerEvent::Progress(ProgressReport {
                summary: "usage".into(),
                // 100 + 50 + 0 + 10 + 0 (the ACP totalTokens arithmetic).
                tokens_used: Some(160),
            })
        );
        match &got[6] {
            WorkerEvent::RateLimited(rl) => {
                assert_eq!(rl.provider, ProviderId::new("opencode"));
                let reset = rl.reset_at.expect("retryAfterMs → reset_at");
                assert!(reset > chrono::Utc::now(), "reset ~5 days out: {reset}");
                assert_eq!(
                    rl.raw.as_deref(),
                    Some("You have exceeded your monthly quota")
                );
            }
            other => panic!("typed quota error → RateLimited, got {other:?}"),
        }
        assert!(
            !got.iter().any(|e| matches!(e, WorkerEvent::TurnCompleted(_))),
            "D3: TurnCompleted is never derived from the SSE stream"
        );

        // The demux: a session with no frames of its own sees only the
        // transport-level broadcast (server.connected → Started).
        let mut other = Vec::new();
        while let Ok(e) = rx_other.try_recv() {
            other.push(e);
        }
        assert_eq!(other, vec![WorkerEvent::Started], "demux by session id");

        // Counters: every junk frame counted, nothing dispatched for it.
        let s = hub.stats();
        assert_eq!(s.connects, 1);
        assert_eq!(s.heartbeats, 1, "the : heartbeat comment");
        assert_eq!(
            s.frames, 11,
            "server.connected, execution.started, text, reasoning, tool.input.started, tool.called, execution.succeeded, usage.updated, execution.failed, unknown, malformed"
        );
        assert_eq!(s.known_noop, 2, "tool.input.started + execution.succeeded");
        assert_eq!(s.unknown_kind, 1, "session.mystery.thing");
        assert_eq!(s.malformed, 1, "the malformed not-json frame");
        assert_eq!(s.unrouted, 0);
        // Started broadcast to 2 sessions + 6 session-scoped events.
        assert_eq!(s.events_emitted, 8);
        // EOF is a gap: both sessions are owed a /message backfill (D3).
        assert_eq!(s.gaps, 1);
        assert_eq!(
            hub.backfill_owed(),
            vec!["ses_other".to_string(), "ses_sse".to_string()]
        );
    }

    /// A stream the client cannot read (401) fails naming method + endpoint +
    /// status, and still records the gap — auth failures never fail silently.
    #[tokio::test]
    async fn an_event_stream_auth_failure_names_endpoint_and_status() {
        let (base, _) = sse_fixture(SSE_SCRIPT).await;
        let client = ServeClient::connect(&base, "wrong-password").unwrap();
        let hub = ServeEventHub::new();
        let _rx = ServeEventHub::subscribe(&hub, "ses_sse");
        hub.take_backfill_owed();

        let err = read_event_stream_once(&client, &hub)
            .await
            .expect_err("401 must fail");
        let msg = err.to_string();
        assert!(msg.contains("GET /api/event"), "{msg}");
        assert!(msg.contains("HTTP 401"), "{msg}");
        assert_eq!(hub.stats().gaps, 1, "failed open is a gap");
        assert_eq!(
            hub.backfill_owed(),
            vec!["ses_sse".to_string()],
            "gap marks the registered session owed a backfill"
        );
    }

    /// Overflow, driven for real: a capacity-1 channel receives 3 events
    /// unread → the receiver observes `Lagged(2)` → detection logs a verdict,
    /// counts the drop, and records the `/message` backfill owed (M3.2
    /// detects + records; M3.3 fetches).
    #[test]
    fn a_lagging_consumer_is_detected_logged_and_owed_a_backfill() {
        let bytes = Arc::new(Mutex::new(Vec::<u8>::new()));
        #[derive(Clone)]
        struct Writer(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Writer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let writer = Writer(Arc::clone(&bytes));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let hub = ServeEventHub::new();
            let mut rx = ServeEventHub::subscribe_with_capacity(&hub, "ses_lag", 1);
            hub.take_backfill_owed();
            hub.note_stream_connected();

            // Three events into a capacity-1 channel, none read.
            for i in 0..3 {
                let frame = format!(
                    r#"{{"id":"evt_l{i}","type":"session.text.delta","data":{{"sessionID":"ses_lag","delta":"chunk{i}"}}}}"#
                );
                hub.handle_frame(&frame);
            }

            // First read observes the lag (detected + recorded inside), then
            // returns the surviving event — the consumer degrades to a
            // backfill, never to a silent gap.
            let e = rx.try_recv().expect("event after lag recovery");
            assert!(matches!(e, WorkerEvent::Progress(_)));

            let s = hub.stats();
            assert_eq!(s.overflowed, 2, "two events dropped for the laggy consumer");
            assert_eq!(s.unrouted, 0, "lag is not an unrouted frame");
            assert_eq!(
                hub.backfill_owed(),
                vec!["ses_lag".to_string()],
                "lag records the backfill owed"
            );
        });

        let logs = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("serve event: consumer lagged"), "{logs}");
        assert!(logs.contains("verdict="), "{logs}");
        assert!(logs.contains("backfill owed"), "{logs}");
        assert!(logs.contains("missed=2"), "{logs}");
    }

    // -- SseParser: pure framing tests (chunk boundaries are TCP's, not ours)

    #[test]
    fn sse_parser_reassembles_frames_split_across_pushes() {
        let mut p = SseParser::default();
        assert!(p.push(b"data: {\"a\"").is_empty(), "partial line, no frame yet");
        assert!(p.push(b":1}\n").is_empty(), "line done, frame boundary not yet");
        let items = p.push(b"\n");
        assert_eq!(items, vec![SseFrame::Data(r#"{"a":1}"#.into())]);
    }

    #[test]
    fn sse_parser_handles_heartbeats_crlf_multidata_and_unknown_fields() {
        let mut p = SseParser::default();
        let items = p.push(b": heartbeat\r\n\r\ndata: a\ndata: b\r\n\r\nevent: x\nid: 7\ndata: {}\n\n");
        assert_eq!(
            items,
            vec![
                SseFrame::Heartbeat,
                // multi-data joined per the SSE spec; trailing \r stripped
                SseFrame::Data("a\nb".into()),
                // event:/id: fields ignored, frame still dispatched
                SseFrame::Data("{}".into()),
            ]
        );
    }

    #[test]
    fn sse_parser_drops_oversize_frames_instead_of_growing() {
        let mut p = SseParser::default();
        let mut bomb = vec![b'x'; MAX_FRAME_BYTES + 1];
        bomb.push(b'\n');
        let items = p.push(&bomb);
        assert_eq!(items, vec![SseFrame::Oversize]);
        assert_eq!(p.pending.len(), 0, "buffer not left holding the bomb");
        // …and the parser still works afterwards.
        assert_eq!(
            p.push(b"data: ok\n\n"),
            vec![SseFrame::Data("ok".into())]
        );
    }

    #[tokio::test]
    async fn the_spawned_event_reader_delivers_events_and_stops() {
        // Fixture emits `server.connected` and closes on every connection;
        // the reader reconnects (gaps recorded) until stopped.
        let (base, _) = sse_fixture("data: {\"id\":\"evt_r\",\"type\":\"server.connected\",\"data\":{}}\n\n").await;
        let client = ServeClient::connect(&base, PW).unwrap();
        let hub = ServeEventHub::new();
        let mut rx = ServeEventHub::subscribe(&hub, "ses_r");

        let reader = EventReader::spawn(client, Arc::clone(&hub));
        let started = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(rx.try_recv(), Ok(WorkerEvent::Started)) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(started.is_ok(), "reader must deliver Started within 5s");
        assert!(hub.stats().connects >= 1);

        reader.stop();
        drop(reader); // Drop aborts the task — cooperative stop has the flag.
    }

    // -- The REAL capture: memory-bank/evidence/m32/sse-live.txt --

    /// Session id every session-scoped frame of the capture carries.
    const CAPTURE_SESSION: &str = "ses_e82f91c70ffe7T9L1wlU5FmkDf";

    /// The measured capture, read at TEST RUN time — never `include_str!`:
    /// the memory bank is local-only in this checkout (`.git/info/exclude`,
    /// projectDecisions #1 — never staged, never committed), so baking the
    /// bytes into the binary would break every fresh clone. `None` means this
    /// machine has no evidence, and the caller says so and returns rather
    /// than pretending it replayed something.
    fn measured_capture() -> Option<String> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../memory-bank/evidence/m32/sse-live.txt");
        match std::fs::read_to_string(&path) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!(
                    "M3.2 capture replay NOT RUN: {} unreadable ({e}) — the 2026-10-27 \
                     live capture is local-only evidence, not committed",
                    path.display()
                );
                None
            }
        }
    }

    /// Replay the bytes opencode 2.1.4 actually emitted — not a hand-written
    /// script — in 37-byte slices, so lines are reassembled across chunk
    /// boundaries the way `Response::chunk()` hands them over, and assert the
    /// exact dispatched sequence plus the counters.
    #[test]
    fn the_measured_capture_replays_to_the_expected_event_sequence() {
        let Some(raw) = measured_capture() else { return; };

        // Pin the fixture first: if the evidence file ever changes, this
        // fails before any assertion about our translation of it.
        let data_lines = raw.lines().filter(|l| l.starts_with("data: ")).count();
        let heartbeats = raw.lines().filter(|l| l.starts_with(": ")).count();
        assert_eq!(data_lines, 58, "measured capture: 58 data frames");
        assert_eq!(heartbeats, 3, "measured capture: 3 heartbeats");

        let hub = ServeEventHub::new();
        hub.note_stream_connected();
        let mut rx = ServeEventHub::subscribe(&hub, CAPTURE_SESSION);
        assert!(
            hub.take_backfill_owed().is_empty(),
            "registering on an up stream owes no backfill"
        );

        let mut parser = SseParser::default();
        let mut payloads = Vec::new();
        for chunk in raw.as_bytes().chunks(37) {
            for frame in parser.push(chunk) {
                match frame {
                    SseFrame::Data(p) => payloads.push(p),
                    SseFrame::Heartbeat => hub.note_heartbeat(),
                    SseFrame::Oversize => {
                        hub.note_malformed("<oversize>");
                    }
                }
            }
        }
        assert_eq!(
            payloads.len(),
            data_lines,
            "every data: line becomes exactly one frame"
        );
        for p in &payloads {
            hub.handle_frame(p);
        }

        let mut got = Vec::new();
        while let Ok(e) = rx.try_recv() {
            got.push(e);
        }
        assert_eq!(got.len(), 6, "dispatched sequence: {got:?}");
        assert_eq!(got[0], WorkerEvent::Started, "server.connected broadcasts");
        assert!(
            matches!(&got[1], WorkerEvent::TurnStarted { .. }),
            "execution.started: {got:?}"
        );
        // Name from tool.input.started, input from tool.called — one ToolUsed
        // assembled across two frames.
        assert_eq!(
            got[2],
            WorkerEvent::ToolUsed(ToolEvent {
                tool: "write".into(),
                detail: Some("/tmp/opencode/m32-probe-ws/ok.txt".into()),
            })
        );
        // Token totals are the capture's own numbers, summed the way ACP
        // reports totalTokens: 9115+67+0+1536 = 10718, then
        // 9227+69+0+12160 = 21456.
        assert_eq!(
            got[3],
            WorkerEvent::Progress(ProgressReport {
                summary: "usage".into(),
                tokens_used: Some(10718),
            })
        );
        assert_eq!(
            got[4],
            WorkerEvent::Progress(ProgressReport {
                summary: "done".into(),
                tokens_used: None,
            })
        );
        assert_eq!(
            got[5],
            WorkerEvent::Progress(ProgressReport {
                summary: "usage".into(),
                tokens_used: Some(21456),
            })
        );

        // Every one of the capture's 29 distinct kinds is classified: nothing
        // fell through to Unknown, nothing was unusable, nothing unrouted.
        let s = hub.stats();
        assert_eq!(s.frames, data_lines as u64, "{s:?}");
        assert_eq!(s.heartbeats, heartbeats as u64, "{s:?}");
        assert_eq!(s.events_emitted, 6, "{s:?}");
        assert_eq!(s.known_noop, data_lines as u64 - 6, "{s:?}");
        assert_eq!(s.unknown_kind, 0, "measured kinds are all mapped: {s:?}");
        assert_eq!(s.malformed, 0, "{s:?}");
        assert_eq!(s.unrouted, 0, "{s:?}");
        assert_eq!(s.overflowed, 0, "{s:?}");
        assert_eq!(s.gaps, 0, "{s:?}");
        assert!(hub.take_backfill_owed().is_empty(), "{s:?}");
    }

    // ------------------------------------------------------------------
    // M3.3 — steering + interrupt
    // ------------------------------------------------------------------

    /// The WIRE contract: a steer is a prompt carrying `delivery:"steer"`
    /// (which the M3.0 spike proved lands inside a RUNNING turn — the case ACP
    /// answers with `-32603`), while a queued delivery stays `"queue"` and
    /// waits for the turn boundary (D4: no silent contract change). That a
    /// steer reaches a turn that is actually running is proven by the M3.3
    /// live smoke, not by a fixture — a fixture cannot run an agent loop.
    #[tokio::test]
    async fn steer_lands_inside_a_running_turn_while_queue_waits_for_the_boundary() {
        let (base, seen) = fixture(PromptMode::Admit).await;
        let client = ServeClient::connect(&base, PW).unwrap();
        let (session, _) = ServeSession::open(client, Path::new(CWD), Some(MODEL), None)
            .await
            .unwrap();

        session
            .steer("STOP the essay. Reply with exactly: STEERED-OK")
            .await
            .unwrap();
        session
            .prompt_async("next task", Delivery::Queue)
            .await
            .unwrap();

        let bodies = seen.prompt_bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0].1["delivery"], "steer", "{:?}", bodies[0].1);
        assert_eq!(bodies[1].1["delivery"], "queue", "{:?}", bodies[1].1);
        // Steering is a prompt, not a side channel: the text rides verbatim.
        assert_eq!(
            bodies[0].1["text"],
            "STOP the essay. Reply with exactly: STEERED-OK"
        );
    }

    /// The ack is the measured TOP-LEVEL `{interrupted: bool}` (no `data:`
    /// wrapper). `false` — nothing was running — must be indistinguishable
    /// from neither success nor transport failure: a 404 names its endpoint
    /// and status instead of pretending nothing was running.
    #[tokio::test]
    async fn an_interrupt_ack_is_measured_top_level_and_never_confused_with_a_failure() {
        let (base, _) = fixture(PromptMode::Admit).await;
        let client = ServeClient::connect(&base, PW).unwrap();

        assert!(
            client.interrupt("ses_fixture").await.unwrap(),
            "a running turn is interrupted"
        );
        assert!(
            !client.interrupt("ses_idle").await.unwrap(),
            "nothing running is a normal `false`, not an error"
        );

        let err = client.interrupt("ses_gone").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("POST /api/session/ses_gone/interrupt"), "{msg}");
        assert!(msg.contains("HTTP 404"), "{msg}");
    }

    /// The session-level wrapper logs the ack with a verdict, so a sweep can
    /// see who cancelled what (two-fix rule: the behaviour AND its trace).
    #[test]
    fn the_interrupt_ack_is_logged_with_a_verdict() {
        let bytes = Arc::new(Mutex::new(Vec::<u8>::new()));
        #[derive(Clone)]
        struct Writer(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Writer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let writer = Writer(Arc::clone(&bytes));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            // current_thread, so every tracing call runs on THIS thread and
            // the subscriber above sees it.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (base, _) = fixture(PromptMode::Admit).await;
                let client = ServeClient::connect(&base, PW).unwrap();
                let (session, _) =
                    ServeSession::open(client, Path::new(CWD), Some(MODEL), None)
                        .await
                        .unwrap();
                assert!(session.interrupt().await.unwrap());
            });
        });

        let logs = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("serve: interrupt"), "{logs}");
        assert!(logs.contains("verdict="), "{logs}");
        assert!(logs.contains("interrupted=true"), "{logs}");
    }

    /// D3 end to end: steer, interrupt, then read the OUTCOME from the durable
    /// `GET …/message` list — never from the SSE stream. The fixture's
    /// `/message` payload is the measured shape (an interrupted turn's
    /// structured `outcome` field on an `idle` row).
    #[tokio::test]
    async fn the_outcome_after_an_interrupt_is_read_from_the_durable_message_list() {
        let (base, _) = fixture(PromptMode::Admit).await;
        let client = ServeClient::connect(&base, PW).unwrap();
        let (session, _) = ServeSession::open(client, Path::new(CWD), Some(MODEL), None)
            .await
            .unwrap();

        session.steer("STOP").await.unwrap();
        assert!(session.interrupt().await.unwrap());

        assert_eq!(
            session.turn_outcomes().await.unwrap(),
            vec!["interrupted".to_string()]
        );
    }

    /// Outcome extraction against the MEASURED body (opencode 2.2.1), then
    /// the legacy text-sentinel path. Rows come back in the server's order —
    /// "last wins" stays the caller's decision, not something this parser
    /// decides for them.
    #[test]
    fn idle_outcomes_reads_the_measured_structured_field_in_document_order() {
        // MEASURED: {data, cursor}, newest-first, outcome is a field on an
        // `idle` row; siblings are typed `user` / `assistant`.
        let measured = json!({
            "data": [
                {"id": "msg_i", "time": {"created": 3}, "type": "idle",
                 "outcome": "interrupted"},
                {"id": "msg_a", "time": {"created": 2}, "type": "assistant",
                 "content": [{"type": "text", "text": "STEERED-OK"}],
                 "finish": "stop"},
                {"id": "msg_u", "time": {"created": 1}, "type": "user",
                 "text": "Write a 700 word essay…"}
            ],
            "cursor": "cur_1"
        });
        assert_eq!(idle_outcomes(&measured), vec!["interrupted"]);

        // Two outcomes: document order preserved (server sends newest first).
        let two = json!({"data": [
            {"type": "idle", "outcome": "interrupted"},
            {"type": "idle", "outcome": "succeeded"}
        ]});
        assert_eq!(idle_outcomes(&two), vec!["interrupted", "succeeded"]);

        // Compatibility path for the text sentinel M3.1 assumed: never
        // captured, kept only so an older body still parses.
        let legacy = json!({"data": [
            {"parts": [{"type": "text", "text": "idle outcome=succeeded"}]},
            {"type": "user", "text": "nothing to find here"}
        ]});
        assert_eq!(idle_outcomes(&legacy), vec!["succeeded"]);

        // Nothing to find is empty, never a panic.
        assert!(idle_outcomes(&Value::Null).is_empty());
        assert!(idle_outcomes(&json!({"data": []})).is_empty());
        assert!(idle_outcomes(&json!({"data": [{"type": "user", "text": "hello"}]})).is_empty());
    }

    /// M3.3's done-condition, against a REAL `opencode serve`. A fixture
    /// cannot run an agent loop, so "a steer reaches a turn that is actually
    /// running" can only be proven live — this is that proof, and it is
    /// `#[ignore]`d because it needs a serve process and provider quota.
    ///
    /// Driven by hand (D1 shape: one dedicated serve, password from its
    /// startup log):
    ///
    /// ```text
    /// opencode serve --hostname 127.0.0.1 --port <p> > serve.log 2>&1 &
    /// pw=$(sed -n 's/^server password //p' serve.log)
    /// AMUX_SMOKE_URL=http://127.0.0.1:<p> AMUX_SMOKE_PASSWORD="$pw" \
    ///   cargo test -p amux-server --lib live_smoke -- --ignored --nocapture
    /// ```
    ///
    /// The load-bearing assertion is `interrupt() == true`: the server only
    /// interrupts an execution that is running, so a `true` AFTER a steer was
    /// admitted proves the turn was still running when we steered into it —
    /// the exact case ACP answers with `-32603`.
    #[tokio::test]
    #[ignore = "live smoke: needs a running `opencode serve` and provider quota"]
    async fn a_live_smoke_steers_into_a_running_turn_and_interrupts_it() {
        let (Ok(url), Ok(pw)) = (
            std::env::var("AMUX_SMOKE_URL"),
            std::env::var("AMUX_SMOKE_PASSWORD"),
        ) else {
            eprintln!("LIVE SMOKE NOT RUN: set AMUX_SMOKE_URL and AMUX_SMOKE_PASSWORD");
            return;
        };

        let client = ServeClient::connect(url, pw).expect("connect to the smoke serve");
        let cwd = std::env::temp_dir().join("m33-smoke-ws");
        std::fs::create_dir_all(&cwd).unwrap();
        let (session, _) = ServeSession::open(client, &cwd, Some(MODEL), None)
            .await
            .expect("open a scratch session");

        // A long task, queued (nothing is running yet, so it starts at once).
        session
            .prompt_async(
                "Write a 4000-word essay about tide tables into essay.txt. \
                 Do not stop early.",
                Delivery::Queue,
            )
            .await
            .expect("admit the long task");

        // Give the agent loop time to actually be mid-turn.
        tokio::time::sleep(Duration::from_secs(4)).await;

        // The steer: admitted while the turn runs.
        let steered = session
            .steer("STOP immediately. Reply with exactly: STEERED-OK")
            .await
            .expect("a steer must be admitted while a turn is running");
        assert!(!steered.is_empty(), "steer returns the admitted message id");
        eprintln!("smoke: steered, admitted as {steered}");

        // The interrupt: `true` proves an execution was still running.
        let interrupted = session.interrupt().await.expect("interrupt ack");
        assert!(interrupted, "the turn was still running, so this is true");
        eprintln!("smoke: interrupt acked, interrupted={interrupted}");

        // D3: the OUTCOME comes from the durable /message list.
        let outcomes = session.turn_outcomes().await.expect("read /message");
        eprintln!("smoke: outcomes read from /message = {outcomes:?}");
        assert!(
            outcomes.iter().any(|o| o == "interrupted"),
            "an interrupted turn leaves a structured row with outcome=\"interrupted\" \
             in /message (measured on 2.2.1): {outcomes:?}"
        );
    }

    // ------------------------------------------------------------------
    // M3.4 — permissions
    // ------------------------------------------------------------------

    /// A measured ask decodes into the shape M2's policy already speaks, and a
    /// row missing required fields is skipped — never guessed into shape.
    #[test]
    fn a_measured_permission_ask_decodes_into_the_shape_the_policy_speaks() {
        let body = serde_json::from_str::<Value>(PERMISSION_REQUEST).unwrap();
        let reqs = permission_requests(&body);
        assert_eq!(reqs.len(), 1);
        let r = &reqs[0];
        assert_eq!(r.id, "per_189a2d347001perm01");
        assert_eq!(r.session_id, "ses_fixture");
        assert_eq!(r.ask.tool, "write");
        assert_eq!(r.ask.title.as_deref(), Some("Write /tmp/ws/perm.txt?"));
        assert_eq!(r.ask.kind.as_deref(), Some("tool"));
        assert_eq!(r.ask.locations, vec!["/tmp/ws/perm.txt"]);
        // The vocabulary a reply may name is the measured enum, so the
        // "never invent" guard has something real to check.
        assert_eq!(r.ask.option_ids, vec!["once", "always", "reject"]);

        assert!(permission_requests(&json!({"data": [{"id": "per_x", "sessionID": "s"}]})).is_empty());
        assert!(permission_requests(&Value::Null).is_empty());
    }

    /// Allow and reject both settle the ask on the WIRE: the fixture records
    /// the `decision` field it actually received, and a 204 (no body) is a
    /// success — not the "non-JSON response" failure the other paths raise.
    #[tokio::test]
    async fn allow_once_replies_once_and_reject_all_replies_reject() {
        use super::super::acp::{AllowOncePolicy, RejectAllPolicy};

        let (base, seen) = fixture(PromptMode::Admit).await;
        let client = ServeClient::connect(&base, PW).unwrap();
        let (session, _) = ServeSession::open(client, Path::new(CWD), Some(MODEL), None)
            .await
            .unwrap();

        let asks = session.poll_permissions().await.unwrap();
        assert_eq!(asks.len(), 1, "the measured ask, decoded");

        assert_eq!(
            session.answer_permission(&asks[0], &AllowOncePolicy).await.unwrap(),
            PermissionReply::AllowOnce
        );
        assert_eq!(
            session.answer_permission(&asks[0], &RejectAllPolicy).await.unwrap(),
            PermissionReply::Reject
        );

        let replies = seen.permission_replies.lock().unwrap();
        assert_eq!(
            replies
                .iter()
                .map(|(_, d)| d.as_str())
                .collect::<Vec<_>>(),
            vec!["once", "reject"],
            "the wire carries the measured enum's own strings"
        );
    }

    /// An unknown request id is a TYPED error naming endpoint, status and tag —
    /// never an invented reply and never a silent `Ok`.
    #[tokio::test]
    async fn an_unknown_request_id_is_a_typed_error_not_an_invented_reply() {
        let (base, seen) = fixture(PromptMode::Admit).await;
        let client = ServeClient::connect(&base, PW).unwrap();

        let err = client
            .reply_permission("ses_fixture", "per_nope", PermissionReply::AllowOnce)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("POST /api/session/ses_fixture/permission/per_nope/reply"),
            "{msg}"
        );
        assert!(msg.contains("HTTP 404"), "{msg}");
        assert!(msg.contains("PermissionNotFoundError"), "{msg}");
        assert!(matches!(err, ProtocolError::Transport(_)), "{err:?}");
        assert!(
            seen.permission_replies.lock().unwrap().is_empty(),
            "an unanswered request id must never be recorded as answered"
        );
    }

    /// The answer is logged with the tool, the resources and the verdict — the
    /// same two-fix visibility ACP gives its asks.
    #[test]
    fn the_answer_is_logged_with_tool_action_and_resources() {
        use super::super::acp::AllowOncePolicy;

        let bytes = Arc::new(Mutex::new(Vec::<u8>::new()));
        #[derive(Clone)]
        struct Writer(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Writer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let writer = Writer(Arc::clone(&bytes));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (base, _) = fixture(PromptMode::Admit).await;
                let client = ServeClient::connect(&base, PW).unwrap();
                let (session, _) =
                    ServeSession::open(client, Path::new(CWD), Some(MODEL), None)
                        .await
                        .unwrap();
                let asks = session.poll_permissions().await.unwrap();
                session
                    .answer_permission(&asks[0], &AllowOncePolicy)
                    .await
                    .unwrap();
            });
        });

        let logs = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("serve permission ask answered"), "{logs}");
        assert!(logs.contains("tool=write"), "{logs}");
        assert!(logs.contains("/tmp/ws/perm.txt"), "{logs}");
        assert!(logs.contains("verdict="), "{logs}");
        assert!(logs.contains("reply=once"), "{logs}");
    }

    /// Belt-and-braces: a decision whose vocabulary the ask did NOT offer is
    /// refused with `reject`, never invented. Serve's enum always offers all
    /// three today, so the ask is hand-built short to reach the guard.
    #[tokio::test]
    async fn a_decision_the_ask_did_not_offer_is_refused_not_invented() {
        use super::super::acp::AllowOncePolicy;

        let (base, seen) = fixture(PromptMode::Admit).await;
        let client = ServeClient::connect(&base, PW).unwrap();
        let (session, _) = ServeSession::open(client, Path::new(CWD), Some(MODEL), None)
            .await
            .unwrap();

        let short = PermissionRequest {
            id: "per_189a2d347001perm01".into(),
            session_id: "ses_fixture".into(),
            ask: PermissionAsk {
                tool: "write".into(),
                title: None,
                kind: Some("tool".into()),
                locations: vec!["/tmp/ws/perm.txt".into()],
                // An ask that only ever offered "reject".
                option_ids: vec!["reject".to_string()],
            },
        };
        assert_eq!(
            session.answer_permission(&short, &AllowOncePolicy).await.unwrap(),
            PermissionReply::Reject,
            "allow-once on an ask that never offered `once` must degrade to a refusal"
        );
        assert_eq!(
            seen.permission_replies.lock().unwrap()[0].1,
            "reject",
            "the wire carries the refusal, not an invented `once`"
        );
    }

    // ------------------------------------------------------------------
    // M3.6 — typed error → WorkerEvent (table-driven)
    // ------------------------------------------------------------------

    /// M3.6's done-when: every **measured** error tag maps to the right
    /// `WorkerEvent`. Each row runs through the REAL path — `serve_failure`
    /// classifies the response, [`serve_worker_event`] surfaces it — so the
    /// assertions can never drift from the message format they read back.
    ///
    /// The bodies are the real shapes: the quota body is the M3.0 spike's 402,
    /// `provider.free-limit` is what the M3.5 live smoke hit on 2026-10-30
    /// (opencode 2.2.3), busy/401/404 come from the openapi, and 503 is a
    /// plain server error.
    #[test]
    fn every_measured_error_tag_maps_to_the_right_worker_event() {
        let cases: &[(u16, &str, &str)] = &[
            (402, QUOTA_BODY, "rate_limited_with_clock"),
            (409, BUSY_BODY, "rejected_no_event"),
            (429, FREE_LIMIT_BODY, "rate_limited_no_clock"),
            (
                401,
                r#"{"_tag":"UnauthorizedErrorEncoded","message":"unauthorized"}"#,
                "failed_no_retry",
            ),
            (503, r#"{"message":"upstream exploded"}"#, "failed_retryable"),
            (
                404,
                r#"{"_tag":"SessionNotFoundErrorEncoded","message":"no such session"}"#,
                "failed_no_retry",
            ),
        ];
        for &(status, body, expected) in cases {
            let err = serve_failure("POST", "/api/session/ses_x/prompt", status, body);
            let event = serve_worker_event(&err);
            match expected {
                // Milestone bullet 1: a rejection is NOT a failure — the
                // queue's turn-boundary retry is the whole handling.
                "rejected_no_event" => assert_eq!(
                    event, None,
                    "HTTP {status}: a rejection must surface as no event at all"
                ),
                // Bullet 2: rate limits wait. Typed first (provider.quota /
                // 402 / 429), and a clock ONLY when retryAfterMs was given —
                // inventing a reset time is how a fleet schedules a wait for
                // something a timer cannot fix (AF-14).
                "rate_limited_with_clock" => match event {
                    Some(WorkerEvent::RateLimited(rl)) => {
                        assert_eq!(rl.kind, RateLimitKind::Unknown, "{status}");
                        assert_eq!(rl.provider, ProviderId::new("opencode"), "{status}");
                        assert!(
                            rl.reset_at.is_some(),
                            "provider.quota carries retryAfterMs, so a real clock exists"
                        );
                        assert!(rl.raw.is_some(), "the original message stays for diagnosis");
                    }
                    other => panic!("HTTP {status}: want RateLimited with a clock, got {other:?}"),
                },
                "rate_limited_no_clock" => match event {
                    Some(WorkerEvent::RateLimited(rl)) => {
                        assert_eq!(rl.reset_at, None, "no retryAfterMs → no invented clock");
                        assert_eq!(rl.kind, RateLimitKind::Unknown, "{status}");
                    }
                    other => panic!("HTTP {status}: want RateLimited without a clock, got {other:?}"),
                },
                // Bullet 3: everything else is Failed, and 4xx vs 5xx decides
                // whether saying so helps anyone.
                "failed_no_retry" => match event {
                    Some(WorkerEvent::Failed(f)) => {
                        assert!(!f.retryable, "HTTP {status} is a client error: {f:?}");
                        assert!(
                            f.reason.contains("/api/session/ses_x/prompt"),
                            "the failing endpoint is named: {f:?}"
                        );
                    }
                    other => panic!("HTTP {status}: want Failed{{retryable:false}}, got {other:?}"),
                },
                "failed_retryable" => match event {
                    Some(WorkerEvent::Failed(f)) => {
                        assert!(f.retryable, "HTTP {status} is a server error: {f:?}");
                        assert!(f.reason.contains("/api/session/ses_x/prompt"), "{f:?}");
                    }
                    other => panic!("HTTP {status}: want Failed{{retryable:true}}, got {other:?}"),
                },
                other => unreachable!("unknown verdict {other}"),
            }
        }

        // No status at all — a connection refused: retryable, endpoint named,
        // and NOT a rate limit no matter what the URL looks like.
        let refused = ProtocolError::Transport(transport_failure_msg(
            "GET",
            "/api/session/ses_x/message",
            "error sending request for url (http://127.0.0.1:1/api/session/ses_x/message)",
        ));
        match serve_worker_event(&refused) {
            Some(WorkerEvent::Failed(f)) => {
                assert!(f.retryable, "a refused connection is worth retrying: {f:?}");
                assert!(f.reason.contains("/api/session/ses_x/message"), "{f:?}");
            }
            other => panic!("connection refused → Failed{{retryable:true}}, got {other:?}"),
        }

        // A rejection that never came from an HTTP status — the guard's own
        // busy refusal — is just as much "not a failure".
        assert_eq!(
            serve_worker_event(&ProtocolError::Rejected(
                "worker has a turn in progress; redeliver at the turn boundary".into()
            )),
            None
        );
        // And a worker with no session at all is not retryable: there is no
        // endpoint to retry until something registers it.
        match serve_worker_event(&ProtocolError::NoSession("wrk_1".into())) {
            Some(WorkerEvent::Failed(f)) => assert!(!f.retryable, "{f:?}"),
            other => panic!("NoSession → Failed{{retryable:false}}, got {other:?}"),
        }
    }
}
