//! Browser API (RR-0092 + AMUX-2598 cutover): the FULL `/api/browser/*`
//! family, answered natively. Route names and request/response shapes follow
//! the Python server's handlers (amux-server.py `/api/browser` block,
//! py:74324-74828); the mechanics are `integrations::browser` — a native
//! Chrome launch plus a real CDP WebSocket client ([`chrome::CdpClient`]).
//!
//! What replaced the proxy (this module's rows used to be PROXIED_FAMILIES
//! row "browser driver verbs"):
//! - `/navigate`, `/screenshot`, `/state`, `/action`, `/inspect`(+`/clear`),
//!   `/search` — mechanical CDP against the Chrome the native `/start`
//!   launched. The pre-cutover seam (native /start's Chrome vs Python's own
//!   driver browser) is GONE: one browser, one owner.
//! - `/sessions`, `/pw-profiles`, `/save-profile`, `DELETE /profile/{name}`
//!   — process/filesystem state this server owns.
//! - `/agent` — an HONEST 501, not a port. The Python implementation runs a
//!   server-side model loop (Anthropic Computer Use driving browser-use);
//!   pinning a model + prompt loop inside the harness is the D1/D3 shape the
//!   ethos names — it cannot improve as models improve. The exit is the amux
//!   WORKER driving the browser through the native verbs with its own model:
//!   capability compounds with the model, and the harness stays mechanical.
//!
//! HARD INVARIANT (owner directive): **browser automation always executes on
//! the server machine; the dashboard is a remote viewer.** Every verb here
//! operates exclusively on the Chrome `integrations::browser` spawned — the
//! CDP client refuses non-loopback endpoints outright — and nothing in any
//! response asks the DASHBOARD-VIEWING browser to act. Artifacts a remote
//! viewer needs (screenshots) are served through the API
//! (`/screenshot/file`), never as a filesystem path the client is expected
//! to open itself.
//!
//! Session semantics (AC-293, ported): every verb resolves its session as
//! explicit `session` (body/query) → `X-Amux-Session` header → `"amux"`, and
//! each session binds to its own tab — two lanes never silently drive one
//! page (see `chrome::resolve_page`). With no amux-launched browser running,
//! driver verbs answer 409 + a pointer at `/start`; they NEVER attach to a
//! browser this process did not launch (a human's Chrome is not ours).
//!
//! Routes (nested at `/api/browser`):
//! - `POST /start {profile?, url?}`     — launch Chrome on a profile
//! - `GET  /status`                     — running child + CDP tab list
//! - `POST /stop`                       — SIGTERM, wait, clean stale locks
//! - `POST /identify {profile?}`       — label/raise the amux window (AF-496)
//! - `GET  /profiles?sizes=1`           — profile inventory
//! - `POST /profile/create {name,url?}` — create profile dir (+ sign-in window)
//! - `DELETE /profile/{name}`           — delete an amux-owned profile
//! - `POST /navigate {url, session?}`   — navigate the session's tab
//! - `GET  /screenshot?session=&url=`   — PNG to ~/.amux/browser-screenshots
//! - `GET  /screenshot/file?session=`   — those bytes, served via the API
//! - `GET  /state?session=`             — url/title/text + indexed elements
//! - `POST /action {action, session?, ...}` — click/type/input/key/scroll/
//!   eval/wait/extract/back/viewport (schema ported from Python)
//! - `GET  /inspect` + `POST /inspect/clear` — console/network/error capture
//! - `GET  /search?q=`                  — google scrape (mechanical)
//! - `GET  /sessions`                   — session→tab bindings
//! - `GET  /pw-profiles`                — playwright profile dirs
//! - `POST /save-profile`               — register profile↔domain
//! - `POST /agent`                      — 501 (see above)
//! - anything else                      — the route CATALOG as a 404 (ported:
//!   two sessions guessed /status for /state and read a bare "not found" as
//!   "the browser API is down")

use super::AppState;
use crate::integrations::browser as chrome;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/start", post(start))
        .route("/status", get(status))
        .route("/stop", post(stop))
        .route("/identify", post(identify))
        .route("/profiles", get(profiles))
        .route("/profile/create", post(profile_create))
        .route("/profile/{name}", delete(profile_delete))
        .route("/navigate", post(navigate))
        .route("/screenshot", get(screenshot))
        .route("/screenshot/file", get(screenshot_file))
        .route("/state", get(state_verb))
        .route("/action", post(action))
        .route("/inspect", get(inspect))
        .route("/inspect/clear", post(inspect_clear))
        .route("/search", get(search))
        .route("/sessions", get(sessions))
        .route("/pw-profiles", get(pw_profiles_list))
        .route("/save-profile", post(save_profile))
        .route("/agent", post(agent))
        // Unknown /api/browser paths answer the route CATALOG (ported from
        // Python). EXPLICIT wildcard routes, not `.fallback()`: in the full
        // composition the static SPA catch-all (`/{*path}`) out-competes a
        // nested fallback and would serve index.html instead (AMUX-2594).
        .route("/", axum::routing::any(catalog_404))
        .route("/{*rest}", axum::routing::any(catalog_404))
}

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

/// Render an error WITH ITS CAUSES, for any body or log line a human reads.
///
/// AMUX-3886. `with_cause(&e)` on an `anyhow::Error` prints the OUTERMOST frame
/// and silently drops the source chain, and every CDP error in this file was
/// built that way. For the `reqwest` errors underneath `cdp_list` that outer
/// frame carries no diagnosis at all:
///
///   to_string(): error sending request for url (http://127.0.0.1:49731/json/list)
///   {e:#}:       error sending request for url (http://127.0.0.1:49731/json/list): \
///                client error (Connect): tcp connect error: Connection refused (os error 61)
///
/// The first string is what two 502s from `general-canvas-apps` left on record
/// on 2026-08-29, and it is IDENTICAL for a refused connect, a DNS failure and
/// a 3s timeout — three different faults with three different fixes. The cause
/// that separates them was one format specifier away the whole time.
fn with_cause(e: &impl std::fmt::Display) -> String {
    format!("{e:#}")
}

/// The ATTRIBUTION resolution: explicit `session` (body/query) →
/// `X-Amux-Session` header → None. No default constant — amux-cloud's
/// validation of the takeover guard caught the harm: a header-less curl's
/// refusal said requested_by:"amux", framing that lane for every anonymous
/// call (AMUX-1768's class), and worse, the guard's same-session shortcut
/// let any TWO anonymous callers stomp each other's browsers because both
/// resolved to the same constant. Ownership records and guard comparisons
/// use THIS; an unattributed caller never matches any owner, its own
/// browsers included — anonymity forfeits the shortcut, and the refusal
/// says "(unattributed)".
fn explicit_session(explicit: Option<&str>, headers: &HeaderMap) -> Option<String> {
    if let Some(s) = explicit {
        let s = s.trim();
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    headers
        .get("x-amux-session")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
}

/// AC-293's resolution order, ported: explicit `session` (body/query) →
/// `X-Amux-Session` header → `"amux"`. The header half is what keeps two
/// well-behaved lanes from silently driving one tab. The `"amux"` default is
/// the shared TAB-BINDING bucket and nothing more — it must never reach an
/// ownership record or an attribution field (see `explicit_session`).
fn resolve_session(explicit: Option<&str>, headers: &HeaderMap) -> String {
    if let Some(s) = explicit {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    if let Some(h) = headers.get("x-amux-session").and_then(|v| v.to_str().ok()) {
        let h = h.trim();
        if !h.is_empty() {
            return h.to_string();
        }
    }
    "amux".into()
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The takeover refusal, with enough evidence to JUDGE it (AMUX-3610).
///
/// THE REPORT: mvs-research hit `a browser is already running under session
/// '(unattributed)'`, declined the takeover — correctly — and was left with no
/// non-destructive move. Measured at the time: pid alive for 18.5 hours, ZERO
/// tabs open, `started_by` empty. The process being alive rules out a crashed
/// lock, so nobody could justify a takeover on those grounds; zero tabs for
/// eighteen hours says it is almost certainly abandoned. Nothing in the API
/// distinguished those two states, so every caller had to choose between
/// blocking forever and destroying possibly-live state including staged logins.
/// A shared single-instance resource whose only escape hatch is destructive is
/// ethos rule 6, and the same shape as the gate whose only exit was `force`.
///
/// TWO OF THE CARD'S FOUR FIXES WERE ALREADY DONE, which is worth recording so
/// nobody re-does them: `started_by` IS written from the resolved attribution
/// (see the `chrome::start` call below), and the refusal HAS named the owner
/// since AMUX-3063. The field was empty because the CALLER sent no
/// `X-Amux-Session`, not because the server drops it. That is a doc defect in
/// the browser example, and it belongs to the file's owner.
///
/// AN EMPTY OWNER AND AN EMPTY-NAMED SESSION READ IDENTICALLY, so `attribution`
/// says which out loud. A machine consumer reading `started_by: ""` cannot tell
/// "nobody claimed this" from "a session whose name is the empty string", and an
/// absent attribution that announces itself is actionable where an empty string
/// is not.
///
/// IT DOES NOT DECIDE THE TAKEOVER. The card asks whether an unattributed holder
/// should be reapable by default; making that automatic would have amux destroy
/// a browser's state — possibly a staged login — on its own judgment, which is
/// rule 8. So this reports and recommends, and the caller still has to say
/// `takeover: true` out loud.
/// The real PAGES behind a CDP target list, as (title, host).
///
/// AMUX-3711. `cdp_list` returns every target, and most of them are not tabs a
/// human would recognise: the specimen was 4 targets — two `browser_ui` omnibox
/// popups, one `iframe`, and ONE page, which was a live Google sign-in. Saying
/// "4 tab(s)" both overstated the activity and hid the one item that mattered.
///
/// HOST ONLY, NEVER THE FULL URL. A sign-in URL carries its `continue=` and
/// state parameters, which are credentials-adjacent, and this string is going
/// into an error body that gets logged and pasted around. The host plus the tab
/// TITLE is everything a reader needs to judge, and neither is a secret.
///
/// Deliberately NOT a sign-in classifier. A keyword list would be a stop-gap
/// that goes stale and that a better model does not need: "Sign in - Google
/// Accounts (accounts.google.com)" is self-explanatory to whoever reads it, and
/// reporting the page rather than a verdict about the page keeps this useful for
/// the cases nobody thought to enumerate.
fn summarize_pages(targets: &[Value]) -> Vec<(String, String)> {
    targets
        .iter()
        .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        .map(|t| {
            let title = t.get("title").and_then(Value::as_str).unwrap_or("").trim().to_string();
            let url = t.get("url").and_then(Value::as_str).unwrap_or("");
            // Host only. Parsing by hand rather than pulling a URL crate in for
            // one field; anything unrecognisable degrades to the scheme-ish
            // prefix rather than leaking the query string.
            let host = url
                .split_once("://")
                .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or("").to_string())
                .unwrap_or_else(|| url.split(['/', '?', '#']).next().unwrap_or("").to_string());
            let title = if title.is_empty() { "(untitled)".to_string() } else { title };
            (title, host)
        })
        .collect()
}

/// What the request log could say about an UNATTRIBUTED holder's start (AF-183).
///
/// 83% of `/api/browser/start` calls carry no `X-Amux-Session` (373 of 447
/// all-time), so for most collisions the refusal's whole safety property —
/// naming the owner you are about to destroy — is unavailable and the caller
/// is handed "(unattributed)", which names nobody and supports no decision.
///
/// But the facts are not gone, they are one table away: `_amux_request_log`
/// records `client_ip` and `user_agent` on every request, and BOTH are present
/// on all 447 of those rows. That is enough to separate the two cases that
/// matter and currently look identical:
///
///   127.0.0.1    + curl/8.7.1                    -> an agent or script on this box
///   100.66.26.84 + Mozilla/5.0 (Macintosh; ...)  -> a HUMAN at a browser
///
/// Both are real rows from the live log. The second is exactly the case this
/// entry's COST names — an agent silently destroying a human's signed-in
/// session — and under "(unattributed)" it is indistinguishable from the first.
///
/// THREE STATES, NOT TWO, because "we looked and found nothing" and "we did not
/// look" must not collapse into the same silence. That collapse is the failure
/// the `attribution` field below already exists to prevent, one level up.
enum StartOrigin {
    /// A start row matched: the caller can be described even if not named.
    Found { ip: String, ua: String },
    /// The lookup RAN against the request log and matched no row.
    NotFound,
    /// No lookup was attempted — the holder is named, or the store was
    /// unavailable. Never rendered as "no origin".
    NotLooked,
}

impl StartOrigin {
    /// How the refusal sentence should describe the holder. Returns None only
    /// for `NotLooked`, where the caller keeps its existing wording.
    fn clause(&self) -> Option<String> {
        match self {
            StartOrigin::Found { ip, ua } => {
                let ua_short: String = ua.chars().take(48).collect();
                // The JUDGEMENT, not just the fields. A caller reading an IP and
                // a user-agent still has to decide what they mean, and the whole
                // point of this refusal is that it is asked to decide under time
                // pressure. Loopback plus a CLI agent is the cheap case; anything
                // from another host in a real browser is a person.
                let loopback = ip.starts_with("127.") || ip == "::1";
                let browser_ua =
                    ua.contains("Mozilla") || ua.contains("Safari") || ua.contains("Chrome");
                let verdict = if !loopback && browser_ua {
                    ", which is a REAL BROWSER on another host and so very probably a PERSON: a takeover destroys a human's session and reopening the tab does not undo it"
                } else if loopback {
                    ", i.e. loopback and a scripted client, so very probably an agent on this box"
                } else {
                    ""
                };
                Some(format!(
                    "an UNATTRIBUTED holder, started from {ip} by {ua_short}{verdict}"
                ))
            }
            StartOrigin::NotFound => Some(
                "an UNATTRIBUTED holder whose origin the request log cannot recover either (no matching start row)"
                    .to_string(),
            ),
            StartOrigin::NotLooked => None,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            StartOrigin::Found { .. } => "request-log start row",
            StartOrigin::NotFound => "none — no matching row in the request log",
            StartOrigin::NotLooked => "not looked up",
        }
    }
}

/// Find the `POST /api/browser/start` row that most likely began this holder.
///
/// Matched on the row nearest at-or-before `started_at`, within a 120s window:
/// the holder's `started_at` is stamped when Chrome comes up, which is after
/// the request row is written, and a launch has never taken anywhere near two
/// minutes. A wider window would start attributing one holder's start to an
/// earlier unrelated call, which is worse than saying nothing.
fn lookup_start_origin(store: &crate::db::SharedStore, started_at: i64) -> StartOrigin {
    let Ok(conn) = store.read() else {
        return StartOrigin::NotLooked;
    };
    // AF-419: ONLY ROWS THAT WERE THEMSELVES UNATTRIBUTED CAN BE THIS HOLDER'S.
    //
    // This function is called for an UNATTRIBUTED holder and nowhere else (the
    // call site gates on `r_owner.trim().is_empty()`). A holder is unattributed
    // exactly when its start carried no session — neither a body/query `session`
    // nor an `X-Amux-Session` header, since `explicit_session` reads both. The
    // log stores the raw header, so that start's row MUST have an empty
    // `amux_session`. A row carrying a session therefore cannot be the one that
    // began this holder, whatever its timestamp.
    //
    // Without this clause the nearest-row rule can hand an unattributed holder
    // the ip and user-agent of a DIFFERENT lane's start, and the refusal then
    // describes the wrong caller with full confidence — the wrong-but-confident
    // attribution class of AF-411, AF-179 and AMUX-3954. Measured live: 115 of
    // 265 all-time start rows are unattributed, and 5 windows exist where an
    // attributed start falls within 120s after an unattributed one (e.g.
    // 2026-08-29 09:08:10 unattributed, ai-video-editor 20.0s later).
    //
    // NOT PROVEN TO HAVE FIRED. `started_at` is not in the log, so I can show
    // the windows exist and not that a wrong answer was produced. The clause is
    // worth it anyway because it makes the wrong answer UNREACHABLE rather than
    // unlikely, and it costs one predicate.
    let row = conn.query_row(
        "SELECT COALESCE(client_ip,''), COALESCE(user_agent,'')          FROM _amux_request_log          WHERE path = '/api/browser/start' AND method = 'POST'            AND COALESCE(amux_session,'') = ''            AND ts <= ?1 AND ts >= ?1 - 120          ORDER BY ts DESC LIMIT 1",
        [started_at as f64],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    );
    match row {
        Ok((ip, ua)) if !ip.is_empty() || !ua.is_empty() => StartOrigin::Found { ip, ua },
        Ok(_) => StartOrigin::NotFound,
        Err(rusqlite::Error::QueryReturnedNoRows) => StartOrigin::NotFound,
        // A BROKEN LOOKUP IS NOT AN ABSENT ROW. Reporting "no origin" here would
        // let a schema change or a locked db read as evidence about the holder.
        Err(_) => StartOrigin::NotLooked,
    }
}

#[cfg(test)]
mod af419_start_origin_tests {
    use super::*;

    fn store_with(rows: &[(f64, &str, &str, &str)]) -> crate::db::SharedStore {
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let store = std::sync::Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        let owned: Vec<(f64, String, String, String)> = rows
            .iter()
            .map(|(t, i, u, s)| (*t, (*i).to_string(), (*u).to_string(), (*s).to_string()))
            .collect();
        store
            .write(move |conn| {
                for (ts, ip, ua, sess) in &owned {
                    conn.execute(
                        "INSERT INTO _amux_request_log \
                         (ts, method, path, family, status, latency_ms, client_ip, user_agent, amux_session, answered_by) \
                         VALUES (?1,'POST','/api/browser/start','/api/browser',200,1.0,?2,?3,?4,'native')",
                        rusqlite::params![ts, ip, ua, sess],
                    )?;
                }
                Ok(crate::db::WriteOutcome { applied: false, events: vec![] })
            })
            .unwrap();
        store
    }

    /// THE CARD'S NAMED CHECK. Two starts inside one 120s window from different
    /// callers: an UNATTRIBUTED one, then an ATTRIBUTED one that is NEARER to
    /// the holder's started_at. The holder is unattributed, so only the first
    /// can be its start — and the nearest-row rule alone would pick the second.
    #[test]
    fn an_unattributed_holder_is_never_described_by_an_attributed_start() {
        let s = store_with(&[
            (1000.0, "100.66.26.84", "Mozilla/5.0 (Macintosh)", ""),
            (1050.0, "127.0.0.1", "curl/8.7.1", "ai-video-editor"),
        ]);
        match lookup_start_origin(&s, 1060) {
            StartOrigin::Found { ip, ua } => {
                assert_eq!(ip, "100.66.26.84", "must be the UNATTRIBUTED row, not the nearer one");
                assert!(ua.contains("Mozilla"), "got {ua}");
            }
            other => panic!("expected the unattributed row, got {}", other.label()),
        }
    }

    /// CONTROL, and the one that makes this honest: with ONLY an attributed row
    /// in the window there is no candidate at all, and the answer must be
    /// NotFound rather than a confident description of the wrong caller.
    /// Without this cell, "return the oldest row" would pass the test above.
    #[test]
    fn only_attributed_rows_in_the_window_means_no_candidate() {
        let s = store_with(&[(1050.0, "127.0.0.1", "curl/8.7.1", "ai-video-editor")]);
        assert!(
            matches!(lookup_start_origin(&s, 1060), StartOrigin::NotFound),
            "an attributed row cannot describe an unattributed holder"
        );
    }

    /// CONTROL: the ordinary case still works — nearest unattributed row wins
    /// among several, so the fix narrows the candidates without inverting them.
    #[test]
    fn among_unattributed_rows_the_nearest_still_wins() {
        let s = store_with(&[
            (1000.0, "10.0.0.1", "old-client", ""),
            (1055.0, "10.0.0.2", "new-client", ""),
        ]);
        match lookup_start_origin(&s, 1060) {
            StartOrigin::Found { ip, .. } => assert_eq!(ip, "10.0.0.2"),
            other => panic!("expected the nearer row, got {}", other.label()),
        }
    }

    /// CONTROL: the 120s window is still enforced — an unattributed row older
    /// than the window must not be adopted just because it is the only one.
    #[test]
    fn the_window_still_bounds_the_search() {
        let s = store_with(&[(900.0, "10.0.0.1", "too-old", "")]);
        assert!(matches!(lookup_start_origin(&s, 1060), StartOrigin::NotFound));
    }
}

#[cfg(test)]
mod launch_latency_tests {
    /// AMUX-3832: `profile/create` with a `url` spawns a HEADED Chrome and
    /// waits for CDP. Seconds is the work, not a fault — its entire purpose is
    /// opening a window for a human to sign in. A flat 10s threshold filed one
    /// such request at 10.5s under 1.09x host load.
    ///
    /// Same seam as AMUX-3818, and the same rule: PER REQUEST, not per route.
    #[test]
    fn only_a_create_the_launch_dominated_declares_itself_slow() {
        use crate::api::dominated_by_external;
        // The filed specimen: 10.5s, essentially all of it Chrome starting.
        assert!(dominated_by_external(10_489, 10_300));

        // CONTROL 1 — a create with NO url launches nothing, so launch_ms is 0
        // and it can never qualify however slow it is. If profile creation
        // itself starts taking ten seconds, that is amux and must file.
        assert!(!dominated_by_external(10_489, 0), "no launch means no excuse");

        // CONTROL 2 — a fast launch inside a slow request leaves time that is
        // amux's own. Without this the declaration is a route exemption wearing
        // a per-request header.
        assert!(!dominated_by_external(11_000, 200));
    }
}

#[cfg(test)]
mod idle_takeover_tests {
    /// The auto-takeover predicate, as a table (Ethan, 2026-08-28).
    ///
    /// He hit the refusal twice in twenty minutes on a browser held 18h with
    /// ZERO tabs. "Nothing to lose" must not be a conflict — but every other
    /// case must still refuse, and those are what this pins.
    fn auto_takes(pages_empty: bool, cdp_answered: bool, held_s: i64, grace_s: i64) -> bool {
        pages_empty && cdp_answered && held_s > grace_s
    }

    #[test]
    fn only_an_idle_browser_with_no_real_pages_is_taken_without_asking() {
        // Ethan's case: 18.4h, zero real pages, CDP answered.
        assert!(auto_takes(true, true, 66_240, 600));

        // CONTROL 1 — a single real page is STATE. The refusal's own history is
        // that "4 tabs" was two omnibox popups, an iframe, and one live Google
        // sign-in; destroying that is not recoverable by reopening a tab.
        assert!(!auto_takes(false, true, 66_240, 600), "an open page must still refuse");

        // CONTROL 2 — CDP SILENCE IS NOT ZERO. This file draws that distinction
        // deliberately ("that is not the same as zero"); unknown must refuse.
        assert!(!auto_takes(true, false, 66_240, 600), "unknown tab count must still refuse");

        // CONTROL 3 — the grace window. A browser started seconds ago has not
        // opened its first tab yet; without this, two concurrent starts resolve
        // by stomping each other.
        assert!(!auto_takes(true, true, 30, 600), "inside the grace window must still refuse");
        assert!(!auto_takes(true, true, 600, 600), "at the boundary, not past it");
        assert!(auto_takes(true, true, 601, 600), "one second past is past");
    }
}

#[cfg(test)]
mod takeover_wording_tests {
    use super::*;

    /// The refusal must not claim something the code cannot do (Ethan, 2026-08-28).
    ///
    /// It used to say a start "would DESTROY its state (staged logins
    /// included)". A completed staged login lives on disk under
    /// `playwright-auth/profiles/<name>/` and survives both a stop (SIGTERM, so
    /// storage flushes) and a takeover — the only `remove_dir_all` on a profile
    /// is the explicit `delete_profile` verb. Ethan hit that sentence on a
    /// browser held 18.1h with ZERO tabs, which is how a guard teaches people to
    /// stop reading guards.
    #[test]
    fn the_refusal_names_what_a_takeover_actually_destroys() {
        let v = takeover_refusal(
            "default", "tubescience", 1_000, 42, Some(0), &[], 1_000 + 65_160,
            Some("amux"), StartOrigin::NotLooked,
        );
        let err = v["error"].as_str().unwrap_or_default();
        assert!(
            !err.contains("staged logins"),
            "the headline must not claim on-disk logins are lost: {err}"
        );
        assert!(err.contains("OPEN TABS"), "it must name what IS lost: {err}");
        // CONTROLS: the refusal still refuses, and still carries the evidence
        // that makes it judgeable. A fix that softened it into an approval, or
        // dropped the idle facts, would pass the assertions above.
        assert!(err.contains("already running"), "it must still refuse: {err}");
        assert!(err.contains("ZERO tabs"), "and still carry the evidence: {err}");
        assert!(err.contains("takeover"), "and still name the deliberate escape: {err}");
    }
}

#[allow(clippy::too_many_arguments)]
fn takeover_refusal(
    profile: &str,
    owner: &str,
    started_at: i64,
    pid: u32,
    tabs: Option<usize>,
    pages: &[(String, String)],
    now: i64,
    requested_by: Option<&str>,
    origin: StartOrigin,
) -> Value {
    let held_s = (now - started_at).max(0);
    let held_h = held_s as f64 / 3600.0;
    let named = !owner.trim().is_empty();
    // The evidence sentence, built only from facts actually in hand. `tabs:
    // None` means CDP did not answer within 3s, which is itself worth saying:
    // silence there is a DIFFERENT state from "zero tabs" and collapsing them
    // would recreate the ambiguity this whole card is about.
    // WHAT IS OPEN, not how many targets exist (AMUX-3711). The count alone
    // pointed the wrong way on the specimen: "4 tab(s)" was two omnibox popups,
    // an iframe, and one live Google sign-in, and "0.0h" read as brand new. Both
    // numbers argued for takeover while the thing at risk was the single most
    // costly thing to destroy — which the headline could only gesture at in the
    // abstract and could not point to. (That headline no longer claims staged
    // logins are lost; they are on disk and survive. See the note at the
    // refusal itself.)
    let page_list = pages
        .iter()
        .map(|(t, h)| {
            let t: String = t.chars().take(60).collect();
            if h.is_empty() { format!("\"{t}\"") } else { format!("\"{t}\" ({h})") }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let idle = match tabs {
        None => format!(
            "held {held_h:.1}h; its CDP port did not answer within 3s, so the tab count is \
             unknown (that is not the same as zero)"
        ),
        // Kept verbatim from AMUX-3610: with no targets at all there is nothing
        // to describe, and this is the abandoned-holder wording that card's
        // reporter needed to see.
        Some(0) => format!("held {held_h:.1}h with ZERO tabs open"),
        Some(n) if pages.is_empty() => format!(
            "held {held_h:.1}h with {n} CDP target(s) but ZERO real pages open — all of them are \
             chrome-internal, so there is no page state to lose"
        ),
        Some(n) => format!(
            "held {held_h:.1}h with {} page(s) open: {page_list}. READ THOSE TITLES BEFORE YOU \
             DECIDE — a takeover destroys them, and an auth flow mid-completion is not \
             recoverable by reopening the tab. ({n} CDP targets in total; the rest are \
             chrome-internal)",
            pages.len()
        ),
    };
    let options: Vec<String> = if named {
        vec![
            format!("message the holder: amux send {owner} --stdin"),
            "drive the running browser via the session-scoped verbs instead of starting your own"
                .into(),
            "if they agree, or you judge the risk acceptable: {\"takeover\": true}".into(),
        ]
    } else {
        vec![
            "the holder CANNOT be messaged: it sent no X-Amux-Session, so there is no session \
             to ask. That is the whole reason this refusal now carries the idle evidence above."
                .into(),
            "drive the running browser via the session-scoped verbs instead of starting your own"
                .into(),
            // SYMMETRIC (AMUX-3711). This used to name only the abandoned case,
            // so the single recommendation on offer always argued FOR the
            // destructive action — including on the specimen, an unattributed
            // holder two minutes old with a live Google sign-in. Advice that can
            // only point one way is not advice. (It also said "tabes".)
            "judge from the evidence above, then say it out loud: {\"takeover\": true}. An \
             unattributed holder with zero pages for hours is very likely abandoned. An \
             unattributed holder with pages open, however recently it started, is somebody \
             mid-task — a fresh start is the WEAKER reason to take over, not the stronger \
             one, because nothing has been finished yet. Either way the call is YOURS, not \
             amux's."
                .into(),
        ]
    };
    json!({
        // THE ESCAPE STAYS IN THE SENTENCE, not only in `your_options`. Plenty
        // of callers print `.error` and nothing else, so moving `takeover` into
        // a sibling field would hide the one documented exit behind a field
        // nobody reads — the ethos rule 6 shape, introduced by a change made to
        // fix ethos rule 6. `cross_session_start_refuses_without_takeover_naming_the_owner`
        // caught exactly that in the first draft of this function.
        // THE FACTS GO IN THE SENTENCE, not only in the body (AF-183). Plenty
        // of callers print `.error` and nothing else — the same reason the
        // takeover escape stays here — and "(unattributed)" alone names nobody,
        // supports no decision, and reads identically whether the holder is a
        // script on this box or a person signed in from another host.
        //
        // NAMES WHAT IS ACTUALLY AT RISK (Ethan, 2026-08-28). This used to say
        // "would DESTROY its state (staged logins included)", which is wrong on
        // its most alarming term and fired regardless of the evidence below it.
        // A COMPLETED staged login lives on disk in
        // `playwright-auth/profiles/<name>/` — 24M for `atlas` — and survives
        // both a stop and a takeover: `stop_as` sends SIGTERM specifically so
        // storage flushes, SIGKILL is a last resort, and the ONLY
        // `remove_dir_all` on a profile is the explicit `delete_profile` verb.
        // Nothing on the start path touches it.
        //
        // What a takeover does destroy is open tabs and an auth flow that has
        // not persisted a cookie yet, which is exactly what the `idle` sentence
        // already says correctly. The headline overstating it is how a guard
        // trains people to ignore guards: Ethan hit this on a browser held 18.1h
        // with ZERO tabs and was told staged logins were at stake.
        //
        // The refusal itself is UNCHANGED and still fires. Only the claim is
        // now one the code can support.
        "error": format!(
            "a browser is already running under {} — starting yours would replace it, losing \
             its OPEN TABS and any auth flow mid-completion. It is {idle}. Pass \
             {{\"takeover\": true}} to replace it deliberately, or drive the running one via \
             session-scoped verbs; `your_options` below spells out the non-destructive moves.",
            if named {
                format!("session '{owner}'")
            } else {
                origin.clause().unwrap_or_else(|| "session '(unattributed)'".to_string())
            }
        ),
        "running": {
            "profile": profile,
            "started_by": owner,
            // The one-output-two-states fix. Never infer this from the emptiness
            // of `started_by`, which is exactly the read that fails.
            "attribution": if named {
                "session".to_string()
            } else {
                "none — the holder sent no X-Amux-Session header, so it cannot be identified \
                 or messaged".to_string()
            },
            // WHERE IT CAME FROM, and whether we could tell (AF-183). `origin`
            // is never absent-by-omission: `origin_source` says which of the
            // three states produced it, so "we looked and found nothing" cannot
            // be read as "we did not look".
            "origin": origin.clause(),
            "origin_source": origin.label(),
            "started_at": started_at,
            "held_secs": held_s,
            "held_h": (held_h * 10.0).round() / 10.0,
            // `tabs_open` counts CDP TARGETS and always has; keeping it stable
            // rather than silently redefining it, because a consumer comparing
            // across versions would never see the change. `pages_open` is the
            // number a human means by "tabs" (AMUX-3711: 4 targets, 1 page).
            "tabs_open": tabs,
            "pages_open": pages.len(),
            "pages": pages.iter().map(|(t, h)| json!({"title": t, "host": h}))
                .collect::<Vec<_>>(),
            "pages_note": "host only, never the full URL: a sign-in URL carries its continue= \
                           and state parameters, and this body gets logged",
            "pid": pid,
        },
        "requested_by": requested_by.unwrap_or("(unattributed)"),
        "your_options": options,
    })
}

/// Map driver-layer failures: NotRunning is the caller's fixable state (409
/// + the fix), CDP trouble is the browser misbehaving (502).
fn driver_err(e: chrome::DriverError) -> Response {
    match e {
        chrome::DriverError::NotRunning => err(
            StatusCode::CONFLICT,
            json!({
                "error": "no amux-launched browser is running — POST /api/browser/start {\"profile\":\"...\",\"url\":\"...\"} first",
                "hint": "native driver verbs operate the Chrome started by /api/browser/start; \
                         GET /api/browser/status shows what is running. They never attach to a \
                         browser this server did not launch.",
            }),
        ),
        chrome::DriverError::Cdp(e) => err(cdp_status(&e), json!({ "error": with_cause(&e) })),
    }
}

/// 504 for a CDP call that never answered, 400 for a page-side exception
/// (AMUX-98), 502 for everything else — a real transport/protocol failure
/// (AMUX-3672).
///
/// All three used to be 502, and `/api/logs/analyze` groups by (status,
/// method, target) — so "the browser is WEDGED", "the browser REJECTED our
/// call", and "the CALLER'S OWN SELECTOR was malformed" landed in the same
/// row and only the error body separated them. A wedged browser means
/// something else is wrong (a contended profile, a hung tab); a protocol
/// error usually means the caller asked for something impossible; a page
/// exception from a bad selector (e.g. Playwright-style `text=...` syntax
/// reaching a native `querySelector`) is a plain caller mistake — the same
/// class `click_outcome`'s NOELEMENT/NOTVISIBLE/STALE sentinels already
/// answer 400 for, just arriving as an exception instead of a string.
/// Splitting all three into different status codes makes them different
/// groups in every log view, for free.
///
/// Decided on the TYPE, never by matching the message: a status code that
/// depends on error wording breaks the first time someone rephrases it.
fn cdp_status(e: &anyhow::Error) -> StatusCode {
    if e.downcast_ref::<chrome::CdpTimeout>().is_some() {
        StatusCode::GATEWAY_TIMEOUT
    } else if e.downcast_ref::<chrome::PageException>().is_some() {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::BAD_GATEWAY
    }
}

/// Resolve session → page → connected CDP client, or the mapped error.
async fn connect_session(session: &str, create_url: Option<&str>) -> Result<(chrome::DriverPage, chrome::CdpClient), Response> {
    // A server restart leaves the browser running and the in-process handle
    // empty (AC-325). Re-adopt lazily HERE, on the path every verb takes, so a
    // sequence that spans a rebuild continues instead of reporting "no
    // amux-launched browser is running" about a browser that is right there.
    chrome::adopt_if_orphaned(&chrome::amux_home()).await;
    let page = chrome::resolve_page(session, create_url).await.map_err(driver_err)?;
    let first = match chrome::CdpClient::connect(&page.ws_url).await {
        Ok(cdp) => return Ok((page, cdp)),
        Err(e) => e,
    };
    // ONE retry, against a DIFFERENT target (AMUX-3708).
    //
    // A socket that will not open on a target Chrome is still listing means the
    // renderer is wedged, which `resolve_page` cannot see: it reads
    // `/json/list`, and a wedged tab is listed exactly like a healthy one. The
    // failing caller is the only thing in the system that knows which target is
    // bad, so it is the only thing that can say so.
    //
    // Bounded at one attempt on purpose. The recovery is "pick another tab, or
    // open one", and if THAT socket also fails the fault is Chrome or the CDP
    // port, which retrying cannot fix and which a 502 should report honestly
    // rather than hide behind a loop.
    let retried = chrome::resolve_page_avoiding(session, create_url, Some(&page.target_id)).await;
    if let Ok(p2) = retried {
        if p2.target_id != page.target_id {
            if let Ok(cdp) = chrome::CdpClient::connect(&p2.ws_url).await {
                return Ok((p2, cdp));
            }
        }
    }
    // Report the FIRST error, not the retry's. The original names what actually
    // went wrong with the session's own tab; a second failure on a freshly
    // opened tab would describe a different target and send the reader after the
    // wrong one.
    tracing::warn!(
        "[browser] session {session:?} could not open a CDP socket on target {} and the rebind \
         did not recover: {first}",
        page.target_id
    );
    Err(err(StatusCode::BAD_GATEWAY, json!({ "error": with_cause(&first) })))
}

// ---------------------------------------------------------------------------
// Launch / lifecycle (native since RR-0092)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct StartBody {
    #[serde(default)]
    profile: String,
    #[serde(default)]
    url: String,
    /// Same resolution as every driver verb. `start` needs it for the reason
    /// AC-336 exists: the tab Chrome opens for `url` has to be OWNED by the
    /// caller, or a peer adopts it.
    #[serde(default)]
    session: Option<String>,
    /// Viewport at launch (AMUX-3403). Both start-time guesses a caller
    /// naturally makes now work: `device` (same preset table as the viewport
    /// action) or `width`+`height`. Applied right after the tab opens, so
    /// start-at-phone-width is one call instead of start-then-action.
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    /// AMUX-3508: launch with no window (`--headless=new`), same profile
    /// dirs — log in headfully once, reuse the cookies headlessly forever.
    #[serde(default)]
    headless: Option<bool>,
    /// Explicit consent to replace ANOTHER session's running browser
    /// (AMUX-3063). The browser is a machine singleton, so start stomps
    /// whoever staged it — on 2026-08-20 an unattributed default-profile
    /// start killed amux-gtm's staged NetSuite login mid-handoff. Without
    /// this flag, a cross-session start refuses and names the owner.
    #[serde(default)]
    takeover: bool,
    /// Every unknown field lands here and is echoed back as
    /// `ignored_fields` (the board API's pattern). Silently swallowing a
    /// misspelled or misplaced field manufactured "the feature does not
    /// work" evidence: two AF-18 validation probes measured a default
    /// viewport against ok:true responses whose viewport request had been
    /// dropped on the floor (AMUX-3403).
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

// `HeaderMap` before `Json` — axum requires the body extractor last.
async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<StartBody>>,
) -> Response {
    let Json(body) = body.unwrap_or_default();
    // Viewport request validated BEFORE launching — a 400 must not cost a
    // Chrome start, and the contract wording matches the viewport action's.
    let dev = body.device.as_deref().unwrap_or("").trim().to_lowercase();
    let named =
        VIEWPORT_DEVICES.iter().find(|(n, ..)| *n == dev).map(|(_, w, h)| (*w, *h));
    if !dev.is_empty() && named.is_none() {
        let mut names: Vec<&str> = VIEWPORT_DEVICES.iter().map(|(n, ..)| *n).collect();
        names.sort_unstable();
        return err(
            StatusCode::BAD_REQUEST,
            json!({ "error": format!("unknown device {dev:?} (one of {})", names.join(", ")) }),
        );
    }
    if body.width.is_some() != body.height.is_some() {
        return err(
            StatusCode::BAD_REQUEST,
            json!({ "error": "width and height go together" }),
        );
    }
    let viewport_wh = named.or(match (body.width, body.height) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => None,
    });
    let home = chrome::amux_home();
    let session = resolve_session(body.session.as_deref(), &headers);
    // ATTRIBUTION is explicit-only (amux-cloud's validation catch): the
    // tab-binding default must never become an ownership record or a
    // same-session match, or two anonymous callers stomp each other freely
    // and every anonymous refusal frames the default lane.
    let attrib = explicit_session(body.session.as_deref(), &headers);
    // TAKEOVER GUARD (AMUX-3063). One browser per machine means start REPLACES
    // whatever is running — including another session's staged, logged-in
    // page. Refuse a cross-session replace unless the caller says takeover
    // out loud, and name the owner in the refusal so the caller knows whose
    // work they are about to destroy. Adopt first so a browser surviving a
    // server restart is guarded too, not just one this process spawned.
    chrome::adopt_if_orphaned(&home).await;
    // THIS PROFILE ONLY (AMUX-3828). A browser on a different profile is a
    // legitimate neighbour: two workers on two profiles must both run, which is
    // the whole point. The conflict question is per user_data_dir, never global.
    let want_profile = body.profile.clone();
    if let Some((r_profile, r_owner, r_started, r_pid, r_port)) =
        chrome::running_snapshot_for(&want_profile)
    {
        // Same-session requires BOTH sides attributed and equal. An
        // unattributed caller matches nothing — anonymity forfeits the
        // shortcut, including against an unattributed owner (two anonymous
        // callers are not one session).
        let same = attrib.as_deref().is_some_and(|a| !r_owner.is_empty() && a == r_owner);
        if !same && !body.takeover {
            // ASK THE BROWSER WHAT IT IS DOING (AMUX-3610). The refusal used to
            // name an owner and stop, which leaves a caller facing an
            // unattributed holder with no non-destructive move at all: block
            // forever, or destroy state nobody can vouch for. `cdp_list` has its
            // own 3s timeout, and a failure degrades the FIELD to null rather
            // than the refusal — a wedged CDP must not turn a 409 into a hang.
            let listed = chrome::cdp_list(r_port).await.ok();
            let tabs = listed.as_ref().and_then(|v| v.as_array().map(|a| a.len()));
            // KEEP THE ARRAY (AMUX-3711). This used to take `.len()` here and
            // drop the rest, which threw away the only evidence that can decide
            // the question a line before building the refusal that asks it.
            let pages = listed
                .as_ref()
                .and_then(|v| v.as_array())
                .map(|a| summarize_pages(a))
                .unwrap_or_default();
            // NOTHING TO LOSE IS NOT A CONFLICT (Ethan, 2026-08-28 12:39: "the
            // browser shit needs to be more intuitive"). He hit this refusal
            // TWICE in twenty minutes against a browser held 18+ hours with
            // ZERO tabs, and the second time the wording was already accurate.
            // Accuracy was not the problem: being made to read a paragraph and
            // adjudicate a judgement call, to open a browser, when the honest
            // answer to "what would this destroy" is NOTHING.
            //
            // The evidence to decide is already in hand three lines up, and
            // this file already draws the conclusion in prose — the Some(n)
            // arm of the refusal says "ZERO real pages open ... so there is no
            // page state to lose". It said it and then refused anyway.
            //
            // So: no real pages, CDP answered, and held past the grace window
            // -> take it over and SAY SO, rather than asking a human to
            // approve the obvious. Every condition is load-bearing:
            //   pages.is_empty() — a single real page is state, and the
            //     tab-count arm of the refusal exists because "4 tabs" was
            //     once one live Google sign-in.
            //   tabs.is_some()   — CDP silence is NOT zero (this file's own
            //     distinction). Unknown means refuse, as before.
            //   held > grace     — a browser started seconds ago by another
            //     lane has not opened its first tab yet; without this, a race
            //     between two starts resolves by stomping.
            let grace_s = std::env::var("AMUX_BROWSER_IDLE_TAKEOVER_S")
                .ok()
                .and_then(|v| v.trim().parse::<i64>().ok())
                .unwrap_or(600);
            let held_s = (now_epoch() - r_started).max(0);
            if pages.is_empty() && tabs.is_some() && held_s > grace_s {
                tracing::info!(
                    profile = %r_profile, owner = %r_owner, held_s, pid = r_pid,
                    requested_by = attrib.as_deref().unwrap_or("(unattributed)"),
                    "browser: auto-takeover — holder had ZERO real pages and was past the \
                     idle grace window, so the start proceeded without asking (AMUX-3828)"
                );
            } else {
            return err(
                StatusCode::CONFLICT,
                takeover_refusal(
                    &r_profile,
                    &r_owner,
                    r_started,
                    r_pid,
                    tabs,
                    &pages,
                    now_epoch(),
                    attrib.as_deref(),
                    // Only for an UNATTRIBUTED holder. A named one is already
                    // messageable, so a db read there would buy nothing and the
                    // NotLooked state says exactly that rather than implying a
                    // lookup came back empty.
                    if r_owner.trim().is_empty() {
                        lookup_start_origin(&state.store, r_started)
                    } else {
                        StartOrigin::NotLooked
                    },
                ),
            );
            }
        }
        if !same {
            tracing::warn!(
                requested_by = %attrib.as_deref().unwrap_or("(unattributed)"),
                owner = %r_owner, profile = %r_profile, pid = r_pid,
                "browser TAKEOVER: replacing another session's running browser (explicit takeover flag)"
            );
        }
    }
    match chrome::start(&home, &body.profile, &body.url, &session, attrib.as_deref().unwrap_or(""), body.headless.unwrap_or(false))
        .await
    {
        Ok(info) => {
            let mut v = serde_json::to_value(&info).unwrap_or_else(|_| json!({}));
            v["ok"] = json!(true);
            // Apply the requested viewport to the tab start just opened —
            // same CDP call as the viewport action. A failure here degrades
            // the FIELD (`viewport_error`), never the start: the browser is
            // up either way, and the caller can retry via the action.
            if let Some((w, h)) = viewport_wh {
                match connect_session(&session, None).await {
                    Ok((_page, mut cdp)) => {
                        let r = cdp
                            .call(
                                "Emulation.setDeviceMetricsOverride",
                                json!({ "width": w, "height": h, "deviceScaleFactor": 0, "mobile": w <= 500 }),
                                Duration::from_secs(10),
                            )
                            .await;
                        match r {
                            Ok(_) => {
                                let seen = cdp
                                    .eval("({w:window.innerWidth,h:window.innerHeight})", 10)
                                    .await
                                    .unwrap_or(Value::Null);
                                v["viewport"] = json!({ "w": w, "h": h, "measured": seen });
                            }
                            Err(e) => v["viewport_error"] = json!(with_cause(&e)),
                        }
                    }
                    Err(_) => {
                        v["viewport_error"] =
                            json!("could not attach to the started tab to apply the viewport — retry with the viewport action")
                    }
                }
            }
            if let Some(p) = headed_launch_pointer(body.headless.unwrap_or(false), &body.profile) {
                v["tell_the_human"] = json!(p);
            }
            // Echo what was dropped (AMUX-3403): an accepted-and-ignored
            // field reads as "the feature does not work" to every caller who
            // guessed the contract. Same pattern as the board API's
            // ignored_fields.
            if !body.extra.is_empty() {
                v["ignored_fields"] = json!(body.extra.keys().collect::<Vec<_>>());
                v["ignored_note"] = json!(
                    "not part of POST /api/browser/start and did nothing; viewport at start is device or width+height"
                );
            }
            Json(v).into_response()
        }
        Err(e) => {
            let (status, body) = start_failure(&e);
            err(status, body)
        }
    }
}

/// 409 for a launch the CALLER can resolve, 502 for one that genuinely failed.
///
/// AF-381: `start` mapped every launch failure to 502, so "another Chrome
/// already holds this --user-data-dir" — whose own hint says "Try again, or GET
/// /api/browser/status and stop it first" — went out as "the upstream is broken,
/// your request was fine". A client that retries on 5xx retries a conflict that
/// will not clear itself, and 5xx is what alerting keys on, so a resolvable
/// state inflated the error budget of a server behaving correctly.
///
/// 409 rather than 400 because it matches the taxonomy this file already has:
/// `driver_err` answers 409 for `NotRunning`, the other state the caller fixes
/// by acting on the browser rather than by changing the request.
///
/// Decided on the TYPE, exactly as `cdp_status` above states and for the same
/// reason: this error's message is a hint someone will reword, and a status that
/// depends on wording breaks the first time they do.
fn start_status(e: &anyhow::Error) -> StatusCode {
    if e.downcast_ref::<chrome::ProfileDelegated>().is_some()
        || e.downcast_ref::<chrome::ExternalProfileInUse>().is_some()
    {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_GATEWAY
    }
}

/// Status plus machine-readable launch verdict. The external-profile conflict
/// is the one state where an undifferentiated error string is actively unsafe:
/// retrying it opens another tab in the human's Chrome. Keep the ordinary
/// failure shape unchanged; enrich only the typed pre-spawn refusal.
fn start_failure(e: &anyhow::Error) -> (StatusCode, Value) {
    let status = start_status(e);
    let mut body = json!({ "error": with_cause(e) });
    if let Some(conflict) = e.downcast_ref::<chrome::ExternalProfileInUse>() {
        body["error_code"] = json!("human_chrome_profile_in_use");
        body["retryable"] = json!(false);
        body["spawned"] = json!(false);
        body["profile"] = json!(conflict.profile);
        body["lock_owner_pid"] = json!(conflict.owner_pid);
        body["user_data_dir"] = json!(conflict.user_data_dir.display().to_string());
        body["use_instead"] = json!({
            "live_browser": "/chrome-cdp after enabling chrome://inspect/#remote-debugging",
            "saved_profile": "fully quit Chrome, then retry POST /api/browser/start once"
        });
    }
    (status, body)
}

async fn status() -> Response {
    // Adopt BEFORE reading (AMUX-3414): status is the first place anyone looks
    // after a death, and it was the one verb that never ran the adopt — so a
    // browser surviving a restart read as "running": false here while every
    // driver verb would have found it, and a browser that DIED with the old
    // server left its corpse-record untriggered because nothing on the
    // observability path opened the running-file.
    chrome::adopt_if_orphaned(&chrome::amux_home()).await;
    // Read the registry under the lock, then drop it before any await —
    // holding a std::sync::Mutex across an await point deadlocks the runtime.
    // EVERY BROWSER (AMUX-3828). `browsers` is the real answer now; the
    // top-level single-browser fields below are kept and describe the FIRST
    // one, because the SPA and the CLI read them and a shape change would break
    // both. `browser_count` is what tells a reader the top level is a summary
    // rather than the whole truth (ethos rule 4: an omission announces itself).
    let all = chrome::running_all();
    let snapshot = all
        .first()
        .map(|(p, o, st, _pid, port, _lv)| (p.clone(), *port, *st, o.clone()));
    // AMUX-3414: why the LAST browser is gone. In-memory, so a server restart
    // clears it — absent means "no exit recorded by this process", not "no
    // exit happened"; the field's note says so rather than letting the two
    // read identically.
    // AF-378: fall back to the PERSISTED record when this process has none.
    //
    // The in-memory one is correct and short-lived. The commonest death is
    // "killed alongside a server restart", which the adopt path records in the
    // NEW process, so the record survives the restart that caused the death and
    // is then erased by the NEXT one. On this box the builder swaps the server on
    // every commit, so the explanation usually evaporates before anyone asks.
    // Reported by tubescience: two browsers dying minutes apart, and all they saw
    // was ConnectionRefusedError on the CDP socket.
    //
    // The disk copy is tagged `from_disk` by `persisted_last_exit`, so a record
    // THIS server observed stays distinguishable from one it inherited.
    let last_exit = chrome::LAST_EXIT
        .lock()
        .expect("last-exit poisoned")
        .clone()
        .or_else(|| chrome::persisted_last_exit(&chrome::amux_home()));
    let Some((profile, cdp_port, started_at, started_by)) = snapshot else {
        return Json(json!({
            "running": false,
            "last_exit": last_exit,
            "last_exit_note": "in-memory first, then the persisted copy (tagged from_disk); null means no exit has been recorded at all",
            // THE COUNTERS BELONG HERE TOO (AMUX-3886 follow-up). Both used to
            // appear only in the running branch, so they vanished in exactly
            // the state they describe: a browser that died leaves `running:
            // false`, and "how many times did a verb find a corpse" is the
            // question you ask AFTER that, not during. Found by reading the
            // live endpoint for the evidence on this card's own close and
            // getting three keys back.
            "stale_binding_recoveries":
                chrome::STALE_BINDING_RECOVERIES.load(std::sync::atomic::Ordering::Relaxed),
            "dead_browser_recoveries":
                chrome::DEAD_BROWSER_RECOVERIES.load(std::sync::atomic::Ordering::Relaxed),
            "recoveries_note": "in-memory; a server restart resets both to 0, so 0 means \
                                \"none since this process started\", not \"never\"",
        }))
        .into_response();
    };
    // Tabs are best-effort: a hung Chrome should degrade the field, not the
    // endpoint. `tabs: null` + `tabs_error` says "could not ask", which is a
    // different fact from "no tabs" (ethos rule 4).
    let (tabs, tabs_error) = match chrome::cdp_list(cdp_port).await {
        Ok(t) => (t, Value::Null),
        Err(e) => (Value::Null, json!(with_cause(&e))),
    };
    // THE REAP COUNTDOWN, VISIBLE (AMUX-3829, second pass). It used to live only
    // in the reaper's process memory, so a reaper about to fire and one that
    // could never fire looked identical from outside — which is exactly how the
    // in-memory clock's restart bug survived being shipped.
    let idle = crate::runtime_jobs::browser_reaper::idle_ages(
        &chrome::amux_home(),
        crate::config::now_f64(),
    );
    let reap_after = crate::runtime_jobs::browser_reaper::reap_after_s();
    let ttl = crate::runtime_jobs::browser_reaper::ttl_s();
    let now_f = crate::config::now_f64();
    // Per-browser rows, so two workers can each see their own.
    let mut browsers: Vec<Value> = Vec::new();
    for (p, o, st, pid, port, last_verb) in &all {
        let t = chrome::cdp_list(*port).await.ok();
        let age_s = now_f - *st as f64;
        let since_verb_s = now_f - *last_verb as f64;
        let ttl_remaining_s = if ttl > 0 {
            let r = ttl as f64 - age_s;
            if r > 0.0 { Some(r.round()) } else { Some(0.0) }
        } else {
            None
        };
        let activity_reap = crate::runtime_jobs::browser_reaper::activity_reap_s();
        let activity_reap_val = if activity_reap == 0 { Value::Null } else { json!(activity_reap) };
        browsers.push(json!({
            "profile": p,
            "started_by": o,
            "started_at": st,
            "age_s": age_s.round(),
            "pid": pid,
            "cdp_port": port,
            "tabs": t.clone().unwrap_or(Value::Null),
            "tab_count": t.as_ref().and_then(|v| v.as_array().map(|a| a.len())),
            // Absent (null) when the profile is not currently empty, which is a
            // different fact from "empty for 0 seconds" and the one a reader
            // needs: null here means nothing is counting down.
            "idle_s": idle.get(p).map(|v| v.round()),
            "reap_after_s": if reap_after == 0 { Value::Null } else { json!(reap_after) },
            // TTL countdown: how many seconds until the hard TTL fires.
            // Null when AMUX_BROWSER_TTL_S=0 (TTL arm disabled).
            "ttl_remaining_s": ttl_remaining_s,
            "ttl_s": if ttl == 0 { Value::Null } else { json!(ttl) },
            // How long since any verb (navigate/screenshot/action) was called.
            "since_verb_s": since_verb_s.round(),
            "activity_reap_s": activity_reap_val,
        }));
    }
    Json(json!({
        "running": true,
        "browsers": browsers,
        "browser_count": all.len(),
        "profile": profile,
        "cdp_port": cdp_port,
        "started_at": started_at,
        "started_by": started_by,
        "tabs": tabs,
        "tabs_error": tabs_error,
        "last_exit": last_exit,
        // AMUX-3708. A wedged renderer leaves a target that `/json/list` still
        // reports as healthy, so nothing in `tabs` above can express it and the
        // recovery is invisible: the request that hit it 502s, the next one
        // succeeds, and the pair reads as a transient blip. Counting it makes a
        // browser that keeps wedging self-announcing instead of arriving as an
        // autofix card twice a week. In-memory, so a restart clears it — zero
        // means "none since this process started", which is why the note says so
        // rather than letting the two read identically.
        "stale_binding_recoveries":
            chrome::STALE_BINDING_RECOVERIES.load(std::sync::atomic::Ordering::Relaxed),
        "stale_binding_recoveries_note":
            "tabs Chrome still listed whose CDP socket would not open, so the session was \
             rebound. In-memory; a server restart resets it to 0.",
        // AMUX-3886. The other half of the same blindness: a browser whose
        // PROCESS is gone while the registry still names it. The exit monitor
        // polls at 5s, so every verb in that gap used to 502 with a cause-free
        // reqwest string; now the verb itself clears the corpse and counts it.
        // Read it BESIDE `last_exit`: a non-zero count with a `found dead on a
        // verb` reason means the monitor was not the thing that noticed.
        "dead_browser_recoveries":
            chrome::DEAD_BROWSER_RECOVERIES.load(std::sync::atomic::Ordering::Relaxed),
        "dead_browser_recoveries_note":
            "verbs that found the registry naming a browser whose process was gone, cleared it, \
             and answered 409 rather than 502. In-memory; a server restart resets it to 0.",
    }))
    .into_response()
}

/// What the lane that just started a browser should tell the human (AF-496).
///
/// amux launches Chrome into its OWN `--user-data-dir`, so a headed start puts a
/// SECOND, visually identical Chrome beside the human's — same icon, same frame,
/// no badge. Measured in a live onboarding session (2026-09-04): the user signed
/// into the wrong window, was corrected three times, and asked the question the
/// product could not answer ("it does look like two different browsers").
///
/// It goes in the reply to `start` because that is the moment the caller is
/// about to write "please log in" to a person, and it is the surface every
/// caller already reads without opting in (ethos rule 1).
///
/// `None` for a headless launch: there is no window, so the sentence would be a
/// lie about the one state where it is easiest to believe.
fn headed_launch_pointer(headless: bool, profile: &str) -> Option<String> {
    if headless {
        return None;
    }
    Some(format!(
        "This opened a SECOND Chrome window beside their own and the two look identical. \
         Before asking anyone to sign in, run `amux browser identify` (or POST \
         /api/browser/identify) — it raises the amux window and draws a blue bar across it \
         naming profile {profile:?}. A Chrome window WITHOUT that bar is theirs."
    ))
}

#[cfg(test)]
mod headed_pointer_tests {
    use super::*;

    #[test]
    fn a_headed_start_is_told_how_to_point_at_the_window_it_opened() {
        let p = headed_launch_pointer(false, "hubspot").expect("a headed start opens a window");
        assert!(p.contains("amux browser identify"), "the pointer names no verb: {p}");
        assert!(p.contains("hubspot"), "the pointer does not name the profile: {p}");
        assert!(
            p.contains("WITHOUT that bar is theirs"),
            "the pointer never says what an unlabelled window means, which is the answer"
        );
    }

    /// The one cell that can catch this being pasted everywhere: a headless
    /// launch has no window at all, and "run identify to see it" would be a
    /// confident instruction to look at nothing.
    #[test]
    fn a_headless_start_is_told_nothing_because_there_is_no_window() {
        assert_eq!(headed_launch_pointer(true, "hubspot"), None);
    }
}

/// Which running browsers an `identify` addresses (AF-496).
///
/// Pure, because the interesting failure is the CHOICE, not the CDP call: a
/// caller with three browsers running and no profile named must not get one
/// arbitrary window raised, and a caller naming a profile that is not running
/// must be told WHICH are, or the refusal sends them back to the same two
/// identical windows they could not tell apart.
#[derive(Debug, PartialEq)]
pub(crate) enum IdentifyScope {
    /// No amux browser is running at all — so every Chrome on screen is the
    /// human's own, which is itself the answer.
    NothingRunning,
    /// A named profile that IS running: label it and raise it.
    Named(String),
    /// A named profile that is not running, plus what is.
    NotRunning { asked: String, running: Vec<String> },
    /// No profile named: label them ALL, so the windows sort themselves out by
    /// elimination. Raise only when there is exactly one — raising three in
    /// sequence leaves the LAST one in front, which answers the question
    /// wrongly rather than not answering it.
    All { profiles: Vec<String>, raise: bool },
}

fn identify_scope(asked: Option<&str>, running: &[String]) -> IdentifyScope {
    let asked = asked.map(str::trim).filter(|s| !s.is_empty());
    if running.is_empty() {
        return IdentifyScope::NothingRunning;
    }
    match asked {
        Some(a) if running.iter().any(|r| r == a) => IdentifyScope::Named(a.to_string()),
        Some(a) => IdentifyScope::NotRunning {
            asked: a.to_string(),
            running: running.to_vec(),
        },
        None => IdentifyScope::All {
            profiles: running.to_vec(),
            raise: running.len() == 1,
        },
    }
}

#[cfg(test)]
mod identify_scope_tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn nothing_running_is_its_own_answer_and_never_an_error() {
        assert_eq!(identify_scope(None, &[]), IdentifyScope::NothingRunning);
        assert_eq!(identify_scope(Some("default"), &[]), IdentifyScope::NothingRunning);
    }

    #[test]
    fn a_named_running_profile_is_the_one_addressed() {
        assert_eq!(
            identify_scope(Some("hubspot"), &v(&["default", "hubspot"])),
            IdentifyScope::Named("hubspot".into())
        );
    }

    /// The refusal has to carry the inventory. Doron's whole problem was that
    /// he could not name the window; "no such profile" alone hands him back the
    /// same question.
    #[test]
    fn a_named_profile_that_is_not_running_still_names_what_is() {
        assert_eq!(
            identify_scope(Some("hubspot"), &v(&["default"])),
            IdentifyScope::NotRunning { asked: "hubspot".into(), running: v(&["default"]) }
        );
    }

    #[test]
    fn one_browser_and_no_name_is_labelled_and_raised() {
        assert_eq!(
            identify_scope(None, &v(&["default"])),
            IdentifyScope::All { profiles: v(&["default"]), raise: true }
        );
    }

    /// THE CELL THIS FUNCTION EXISTS FOR. With several running, raising one is
    /// worse than raising none: the human asked "which is amux's" and a raise
    /// asserts a single answer to a question that has three. Label them all and
    /// let the unlabelled window be the human's by elimination.
    #[test]
    fn several_browsers_and_no_name_labels_every_one_and_raises_none() {
        assert_eq!(
            identify_scope(None, &v(&["default", "hubspot", "gmail"])),
            IdentifyScope::All { profiles: v(&["default", "hubspot", "gmail"]), raise: false }
        );
    }

    /// A blank or whitespace `profile` is "no profile", not a profile named "".
    /// A JSON client that sends `{"profile": ""}` for "unset" would otherwise
    /// get the NotRunning refusal every time.
    #[test]
    fn a_blank_profile_reads_as_unset() {
        assert_eq!(
            identify_scope(Some("   "), &v(&["default"])),
            IdentifyScope::All { profiles: v(&["default"]), raise: true }
        );
    }
}

/// The one-line answer, computed from what actually HAPPENED (AF-496).
///
/// Every argument is an outcome. The first live call of this endpoint said "the
/// amux window is now in front" over a raise that had failed with a real error,
/// because the verdict read the REQUEST (`raise` was asked for) rather than the
/// RESULT (`raised` came back 0). A summary a reader takes as the result of the
/// lines above must be computed from those lines, or it is an independent claim
/// sitting beside them that cannot disagree with them.
fn identify_verdict(labeled: usize, browsers: usize, raised: usize, raise_attempted: bool) -> String {
    if labeled == 0 {
        return "no window could be labelled — read `errors` on each browser below".into();
    }
    let bar = if browsers == 1 {
        "the amux window now carries a blue bar naming its profile".to_string()
    } else {
        format!("all {browsers} amux windows now carry a blue bar naming their profile")
    };
    let tail = "; a Chrome window WITHOUT one is your own";
    match (raise_attempted, raised) {
        (false, _) => format!("{bar}{tail}"),
        (true, 0) => format!(
            "{bar}{tail}. It could NOT be brought to the front — see `raise_error`; \
             find it by the bar instead"
        ),
        (true, n) => format!("{bar}{tail}. {n} of them raised to the front"),
    }
}

#[cfg(test)]
mod identify_verdict_tests {
    use super::*;

    /// THE CELL THIS FUNCTION EXISTS FOR, and the bug it was written over: the
    /// verdict must not claim a window is in front when the raise failed. The
    /// endpoint shipped saying exactly that, over a `raise_error` in the same
    /// payload.
    #[test]
    fn a_failed_raise_is_never_reported_as_a_window_in_front() {
        let v = identify_verdict(1, 1, 0, true);
        assert!(!v.contains("in front,"), "{v}");
        assert!(v.contains("could NOT be brought to the front"), "{v}");
        assert!(v.contains("raise_error"), "the reader is not told where the reason is: {v}");
    }

    #[test]
    fn a_raise_that_worked_says_so() {
        let v = identify_verdict(1, 1, 1, true);
        assert!(v.contains("raised to the front"), "{v}");
        assert!(!v.contains("could NOT"), "{v}");
    }

    /// The multi-window case never attempts a raise, so its verdict must not
    /// mention one either way — the bar and the elimination rule ARE the answer.
    #[test]
    fn several_windows_are_answered_by_the_bar_and_the_elimination_rule() {
        let v = identify_verdict(3, 3, 0, false);
        assert!(v.contains("all 3 amux windows"), "{v}");
        assert!(v.contains("WITHOUT one is your own"), "{v}");
        assert!(!v.contains("front"), "no raise was attempted, so none should be mentioned: {v}");
    }

    #[test]
    fn labelling_nothing_points_at_the_errors_rather_than_claiming_success() {
        let v = identify_verdict(0, 2, 0, false);
        assert!(v.contains("no window could be labelled"), "{v}");
        assert!(v.contains("errors"), "{v}");
    }
}

#[derive(Deserialize, Default)]
struct IdentifyBody {
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    session: Option<String>,
}

/// POST /api/browser/identify — make the amux window say that it is the amux
/// window (AF-496).
async fn identify(headers: HeaderMap, body: Option<Json<IdentifyBody>>) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or_default();
    let asked_by = resolve_session(body.session.as_deref(), &headers);
    // Same reason `status` adopts first: a browser that survived a server
    // restart is not in this process's registry until something adopts it, and
    // "no browser is running" is the single most misleading answer this verb
    // could give a human looking straight at one.
    chrome::adopt_if_orphaned(&chrome::amux_home()).await;
    let all = chrome::running_all();
    let profiles: Vec<String> = all.iter().map(|(p, ..)| p.clone()).collect();
    let scope = identify_scope(body.profile.as_deref(), &profiles);

    let (targets, raise) = match &scope {
        IdentifyScope::NothingRunning => {
            return Json(json!({
                "ok": true,
                "identified": [],
                "running": 0,
                "verdict": "no amux browser is running, so every Chrome window on screen is your \
                            own. POST /api/browser/start {\"profile\":\"...\"} opens one.",
            }))
            .into_response();
        }
        IdentifyScope::NotRunning { asked, running } => {
            return err(
                StatusCode::NOT_FOUND,
                json!({
                    "error": format!("no amux browser is running on profile {asked:?}"),
                    "running_profiles": running,
                    "hint": "POST with no profile to label every amux window at once",
                }),
            );
        }
        IdentifyScope::Named(p) => (vec![p.clone()], true),
        IdentifyScope::All { profiles, raise } => (profiles.clone(), *raise),
    };

    let mut identified = Vec::new();
    for name in &targets {
        let Some((_, started_by, _, pid, port, _)) =
            all.iter().find(|(p, ..)| p == name).cloned()
        else {
            continue;
        };
        identified.push(chrome::identify_one(name, &started_by, pid, port, raise).await);
    }
    let labeled: usize = identified.iter().map(|i| i.labeled).sum();
    let pages: usize = identified.iter().map(|i| i.pages).sum();
    let raised: usize = identified.iter().filter(|i| i.raised).count();
    tracing::info!(
        asked_by,
        browsers = identified.len(),
        labeled,
        pages,
        raise_attempted = raise,
        raised,
        "browser: identify labelled the amux window(s) (AF-496)"
    );
    Json(json!({
        "ok": true,
        "identified": identified,
        "running": all.len(),
        // The number is the claim; `pages` is the denominator that says whether
        // it could have been higher (ethos rule 4). `labeled: 0, pages: 0` is a
        // browser with no page tab, which is a different fact from every tab
        // refusing the banner.
        "labeled": labeled,
        "pages": pages,
        // ATTEMPT AND OUTCOME, NAMED APART. The first live call of this endpoint
        // returned `raised: true` beside a per-browser `raised: false` and a
        // real `raise_error`, because the top-level field carried the REQUEST
        // and the nested one carried the RESULT under one name. Same word, same
        // payload, opposite meanings.
        "raise_attempted": raise,
        "raised": raised,
        "verdict": identify_verdict(labeled, identified.len(), raised, raise),
        "banner_ms": chrome::identify_banner_ms(),
    }))
    .into_response()
}

async fn stop(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    // Explicit attribution only — the tab-binding default must not sign the
    // stop record as "amux" for an anonymous caller (amux-cloud's catch).
    let attrib = explicit_session(body.get("session").and_then(Value::as_str), &headers);
    let actor = attrib.as_deref().unwrap_or("(unattributed)");
    let home = chrome::amux_home();
    // Cross-session stop stays PERMITTED (a wedged browser must be cleanable
    // by whoever notices) but LOUD: the log and the response both name owner
    // and actor, so an anonymous stop can no longer read as a mystery death
    // (AMUX-3063's other half — the 09:05 stop had no actor on record).
    let owner = chrome::running_all().into_iter().next().map(|(_, o, _, _, _, _)| o);
    if let Some(o) = owner.as_deref() {
        if attrib.as_deref() != Some(o) {
            tracing::warn!(
                stopped_by = %actor, owner = %o,
                "browser: cross-session STOP of another session's browser"
            );
        }
    }
    let report = chrome::stop_as(&home, attrib.as_deref().unwrap_or("")).await;
    let mut v = serde_json::to_value(&report).unwrap_or_else(|_| json!({}));
    v["ok"] = json!(true);
    v["stopped_by"] = json!(actor);
    if let Some(o) = owner {
        v["owner"] = json!(o);
    }
    Json(v).into_response()
}

#[derive(Deserialize, Default)]
struct ProfilesQuery {
    #[serde(default)]
    sizes: Option<String>,
}

async fn profiles(Query(q): Query<ProfilesQuery>) -> Response {
    let with_sizes = q.sizes.as_deref().is_some_and(|s| !s.is_empty() && s != "0");
    let home = chrome::amux_home();
    // Size walks touch a few hundred files per profile — off the runtime.
    let list =
        match tokio::task::spawn_blocking(move || chrome::list_profiles(&home, with_sizes)).await {
            Ok(l) => l,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": with_cause(&e) })),
        };
    let chrome_profiles = chrome::list_chrome_profiles();
    // THE TTL AND THE COUNTDOWN, BESIDE THE THING THEY DELETE. A reaper whose
    // only trace is a log line is one nobody knows about until a profile is
    // gone; `reap_in_days` on each row makes the schedule readable before it
    // fires, the same reason the idle arm publishes `idle_s`/`reap_after_s`.
    // Null means exempt, and `reap_exempt_reason` says which of the three
    // reasons applies rather than leaving the reader to infer it.
    let ttl_days = crate::runtime_jobs::browser_reaper::profile_ttl_days();
    let running: std::collections::HashSet<String> =
        chrome::running_all().into_iter().map(|(p, ..)| p).collect();
    let now = crate::config::now_f64();
    let rows: Vec<Value> = list
        .iter()
        .map(|p| {
            let age_days = p.last_used.map(|lu| (now - lu as f64) / 86_400.0);
            let is_running = running.contains(&p.name);
            let exempt = if ttl_days == 0 {
                Some("ttl disabled (AMUX_PROFILE_TTL_DAYS=0)")
            } else if p.registered {
                Some("registered — a deliberate save, exempt at any age")
            } else if is_running {
                Some("a browser is running on this profile")
            } else if p.name == "default" {
                Some("the default profile is never deleted")
            } else if age_days.is_none() {
                Some("last-used time could not be read, so age is unknown")
            } else {
                None
            };
            let mut v = serde_json::to_value(p).unwrap_or(Value::Null);
            if let Some(o) = v.as_object_mut() {
                o.insert("age_days".into(), json!(age_days.map(|d| (d * 10.0).round() / 10.0)));
                o.insert("reap_exempt_reason".into(), json!(exempt));
                o.insert(
                    "reap_in_days".into(),
                    json!(match (exempt, age_days) {
                        (None, Some(d)) => json!(((ttl_days as f64 - d).max(0.0) * 10.0).round() / 10.0),
                        _ => Value::Null,
                    }),
                );
            }
            v
        })
        .collect();
    Json(json!({
        "profiles": rows,
        "backends": ["native"],
        "chrome_profiles": chrome_profiles,
        "profile_ttl_days": ttl_days,
        "profile_ttl_note": "unregistered profiles unused for this many days are deleted by the \
                             reaper. Registered profiles (a deliberate save with domains/label) \
                             are exempt at any age, as is any profile with a browser running on \
                             it. 0 disables the arm.",
    }))
    .into_response()
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    session: Option<String>,
}

async fn profile_create(headers: HeaderMap, Json(body): Json<CreateBody>) -> Response {
    let started = std::time::Instant::now();
    let name = body.name.trim().to_string();
    if name.is_empty()
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return err(
            StatusCode::BAD_REQUEST,
            json!({ "error": "profile name must be [A-Za-z0-9._-]+" }),
        );
    }
    if name == "default" {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "'default' already exists" }));
    }
    let home = chrome::amux_home();
    // Create in the amux-owned location so create-path == use-path (the L7
    // mismatch this subsystem exists to end). resolve_profile_dir will find
    // it here from now on because the dir exists.
    let dir = home.join("playwright-auth").join("profiles").join(&name);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": with_cause(&e) }));
    }
    // A sign-in URL means "open a headed window on the new profile so a
    // human can log in" — same intent as the Python create flow.
    let mut launched = false;
    let mut launch_error = Value::Null;
    // TIME THE LAUNCH SEPARATELY FROM THE REQUEST (AMUX-3832, the AMUX-3818
    // pattern). With a `url` this endpoint's work IS spawning a HEADED Chrome
    // and waiting for CDP — seconds, by construction, since its entire purpose
    // is opening a window for a human to sign in. A flat 10s threshold files
    // that as a defect: one such request was reported at 10.5s under 1.09x host
    // load, which is the work, not a fault.
    let mut launch_ms: u128 = 0;
    if !body.url.trim().is_empty() {
        let session = resolve_session(body.session.as_deref(), &headers);
        let attrib = explicit_session(body.session.as_deref(), &headers);
        let launch_started = std::time::Instant::now();
        match chrome::start(&home, &name, body.url.trim(), &session, attrib.as_deref().unwrap_or(""),
            // create-profile launches headfully by definition: its purpose is a
            // human logging in (AMUX-3508's headless is for REUSING the result).
            false)
            .await
        {
            Ok(_) => launched = true,
            Err(e) => launch_error = json!(with_cause(&e)),
        }
        launch_ms = launch_started.elapsed().as_millis();
    }
    let body_v = Json(json!({
        "ok": true,
        "profile": name,
        "path": dir.display().to_string(),
        "launched": launched,
        "launch_ms": launch_ms,
        "launch_error": launch_error,
        "note": "sign in through the opened window, then POST /api/browser/stop to flush the profile",
    }))
    .into_response();
    // Declare the wait as the LAUNCH's only when the launch dominated it. A
    // create that took 11s around a 200ms launch is amux being slow and must
    // still file — which is what keeps this a per-request declaration rather
    // than a route exemption. A create with no `url` launches nothing, so
    // `launch_ms` is 0 and it can never qualify.
    let total_ms = started.elapsed().as_millis();
    if crate::api::dominated_by_external(total_ms, launch_ms) {
        return crate::api::slow_ok(body_v, &format!("chrome-launch {launch_ms}ms"));
    }
    body_v
}

async fn profile_delete(Path(name): Path<String>) -> Response {
    // Python's route regex admits [A-Za-z0-9._-]+ only; anything else falls
    // through to its catalog 404. Same here.
    if name.is_empty()
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return catalog_body(&format!("/api/browser/profile/{name}"));
    }
    match chrome::delete_profile(&chrome::amux_home(), &name) {
        Ok(v) => Json(v).into_response(),
        Err((code, v)) => {
            err(StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), v)
        }
    }
}

// ---------------------------------------------------------------------------
// Driver verbs (native since AMUX-2598)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct NavigateBody {
    #[serde(default)]
    url: String,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    profile: Option<String>,
}

async fn navigate(headers: HeaderMap, body: Option<Json<NavigateBody>>) -> Response {
    let Json(b) = body.unwrap_or_default();
    let url = b.url.trim().to_string();
    if url.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "url required" }));
    }
    let session = resolve_session(b.session.as_deref(), &headers);
    let (page, mut cdp) = match connect_session(&session, Some(&url)).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    match chrome::navigate_and_settle(&mut cdp, &url).await {
        Ok(mut v) => {
            crate::integrations::browser::touch_verb_for_session(&session);
            if let Some(o) = v.as_object_mut() {
                o.insert("backend".into(), json!("native"));
                o.insert("session".into(), json!(session));
                o.insert("target".into(), json!(page.target_id));
                // Name the ignored field rather than dropping it silently
                // (the cold-outbound `ignored_fields` lesson, ethos rule 7).
                if b.profile.as_deref().is_some_and(|p| !p.trim().is_empty()) {
                    o.insert(
                        "profile_note".into(),
                        json!("profile is chosen at POST /api/browser/start; /navigate drives the already-started browser"),
                    );
                }
            }
            Json(v).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, json!({ "error": with_cause(&e) })),
    }
}

#[derive(Deserialize, Default)]
struct ShotQuery {
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

async fn screenshot(headers: HeaderMap, Query(q): Query<ShotQuery>) -> Response {
    let session = resolve_session(q.session.as_deref(), &headers);
    let (page, mut cdp) = match connect_session(&session, None).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Some(u) = q.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        if let Err(e) = chrome::navigate_and_settle(&mut cdp, u).await {
            // Log signal (two-fixes rule): a wedged navigation / capture shows as
            // a blank live view in the dashboard with no other trace. Surface it
            // so a log sweep / /api/logs catches "browser session X can't render"
            // without a human noticing the empty viewport first.
            tracing::warn!("[browser] navigate failed for session {session:?} → {u}: {e}");
            return err(StatusCode::BAD_GATEWAY, json!({ "error": with_cause(&e) }));
        }
    }
    match chrome::screenshot_to_file(&mut cdp, &chrome::amux_home(), &session).await {
        Ok((path, size)) => {
            crate::integrations::browser::touch_verb_for_session(&session);
            let url = cdp
                .eval("location.href", 10)
                .await
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or(page.url);
            Json(json!({
                "ok": true,
                // WHICH SESSION'S BINDING THIS IS (AMUX-3834, tubescience).
                // `resolve_session` falls back explicit-param -> X-Amux-Session
                // -> "amux", silently. A caller who omits the header on ONE verb
                // drives a different binding than their other calls and has no
                // way to see it: tubescience spent a session on "screenshot and
                // eval are on different targets" that was two SESSIONS, not two
                // resolvers, and only found it by curling both ways side by
                // side. Echoing the resolved name makes the next occurrence
                // self-evident from a single response.
                "session": session,
                "backend": "native",
                "path": path.display().to_string(),
                "size": size,
                "url": url,
                // `path` is a SERVER-machine filesystem path (Python returns
                // only that; its dashboard bridges it via /api/file/raw). A
                // remote viewer cannot read it directly, so the bytes are
                // also served through this family:
                "serve": format!("/api/browser/screenshot/file?session={}", urlenc(&session)),
            }))
            .into_response()
        }
        Err(e) => {
            // e.g. "CDP Page.captureScreenshot timed out after 30s" (amux-gtm,
            // 2026-08-13) — the tab wedged and the viewport went blank. WARN so
            // the failure is visible in the logs, not only as an empty view.
            tracing::warn!("[browser] screenshot capture failed for session {session:?}: {e}");
            err(StatusCode::BAD_GATEWAY, json!({ "error": with_cause(&e) }))
        }
    }
}

/// GET /api/browser/screenshot/file?session= — the screenshot BYTES, served
/// through the API. The dashboard may be a phone across the network; a bare
/// filesystem path only resolves on the server machine (where the browser
/// and this file both live, per the server-machine invariant), so the API
/// itself serves the image for remote viewers.
async fn screenshot_file(headers: HeaderMap, Query(q): Query<SessionQuery>) -> Response {
    let session = resolve_session(q.session.as_deref(), &headers);
    let file = chrome::amux_home()
        .join("browser-screenshots")
        .join(format!("native-{}.png", chrome::safe_file_component(&session)));
    match tokio::fs::read(&file).await {
        Ok(bytes) => (
            [
                (axum::http::header::CONTENT_TYPE, "image/png"),
                (axum::http::header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => err(
            StatusCode::NOT_FOUND,
            json!({
                "error": format!("no native screenshot for session {session:?} — GET /api/browser/screenshot first"),
                "path": file.display().to_string(),
            }),
        ),
    }
}

#[derive(Deserialize, Default)]
struct SessionQuery {
    #[serde(default)]
    session: Option<String>,
}

/// The state payload both `/state` and `action:extract` serve: url/title/
/// viewport/indexed elements plus capped page text.
async fn state_payload(cdp: &mut chrome::CdpClient, session: &str) -> Result<Value, Response> {
    let mut v = cdp
        .eval(&chrome::state_js(), 20)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, json!({ "error": with_cause(&e) })))?;
    let text = v.get("text").and_then(Value::as_str).unwrap_or("").to_string();
    if let Some(o) = v.as_object_mut() {
        o.insert("text".into(), json!(chrome::obs_cap(&text, chrome::obs_state_cap())));
        o.insert("ok".into(), json!(true));
        o.insert("backend".into(), json!("native"));
        o.insert("session".into(), json!(session));
    }
    Ok(v)
}

async fn state_verb(headers: HeaderMap, Query(q): Query<SessionQuery>) -> Response {
    let session = resolve_session(q.session.as_deref(), &headers);
    let (_page, mut cdp) = match connect_session(&session, None).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    crate::integrations::browser::touch_verb_for_session(&session);
    match state_payload(&mut cdp, &session).await {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

/// Python's viewport device table, same names and dimensions (AF-18).
const VIEWPORT_DEVICES: &[(&str, u32, u32)] = &[
    ("iphone", 390, 844),
    ("iphone-se", 375, 667),
    ("ipad", 820, 1180),
    ("desktop", 1280, 900),
];

async fn action(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let action = body.get("action").and_then(Value::as_str).unwrap_or("").to_string();
    let session = resolve_session(body.get("session").and_then(Value::as_str), &headers);

    let get_str = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_string);
    let get_f64 = |k: &str| body.get(k).and_then(Value::as_f64);
    let get_usize = |k: &str| body.get(k).and_then(Value::as_u64).map(|v| v as usize);

    // -- Request-shape validation FIRST: a 400 must not depend on whether a
    //    browser happens to be running (and the schema stays testable
    //    without Chrome). Messages match the Python handler's.
    let mut viewport_wh: Option<(u32, u32)> = None;
    match action.as_str() {
        "click" => {
            if get_str("selector").is_none()
                && get_usize("index").is_none()
                && !(get_f64("x").is_some() && get_f64("y").is_some())
            {
                return err(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": "click needs selector, index, or x,y" }),
                );
            }
        }
        "eval" => {
            if get_str("script").map(|s| s.trim().is_empty()).unwrap_or(true) {
                return err(StatusCode::BAD_REQUEST, json!({ "error": "script required" }));
            }
        }
        "input" => {
            if get_usize("index").is_none() || get_str("text").is_none() {
                return err(StatusCode::BAD_REQUEST, json!({ "error": "input needs index and text" }));
            }
        }
        "key" => {
            let k = get_str("key").unwrap_or_default();
            if !chrome::CDP_KEYS.iter().any(|(n, ..)| *n == k) {
                let supported =
                    chrome::CDP_KEYS.iter().map(|(n, ..)| *n).collect::<Vec<_>>().join(", ");
                return err(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": format!("unsupported key {k:?} (supported: {supported})") }),
                );
            }
        }
        "viewport" => {
            let dev = get_str("device").unwrap_or_default().trim().to_lowercase();
            let named = VIEWPORT_DEVICES.iter().find(|(n, ..)| *n == dev).map(|(_, w, h)| (*w, *h));
            let explicit = match (body.get("width").and_then(Value::as_u64), body.get("height").and_then(Value::as_u64)) {
                (Some(w), Some(h)) => Some((w as u32, h as u32)),
                _ => None,
            };
            viewport_wh = named.or(explicit);
            if viewport_wh.is_none() {
                let mut names: Vec<&str> = VIEWPORT_DEVICES.iter().map(|(n, ..)| *n).collect();
                names.sort_unstable();
                return err(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": format!("viewport needs width+height, or device (one of {})", names.join(", ")) }),
                );
            }
        }
        "wait" => {
            let has = |k: &str| get_str(k).map(|s| !s.trim().is_empty()).unwrap_or(false);
            if !has("selector") && !has("text") {
                return err(StatusCode::BAD_REQUEST, json!({ "error": "wait needs selector or text" }));
            }
        }
        // TUBES-2343. Validated here with the rest, so a bad request is a 400
        // whether or not a browser happens to be running and the schema stays
        // testable without Chrome.
        "files" => {
            if get_str("selector").map(|s| s.trim().is_empty()).unwrap_or(true) {
                return err(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": "files needs a selector for the <input type=file>" }),
                );
            }
            let paths = body.get("files").and_then(Value::as_array);
            let Some(paths) = paths.filter(|a| !a.is_empty()) else {
                return err(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": "files needs a non-empty `files` array of absolute paths" }),
                );
            };
            for p in paths {
                let Some(p) = p.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
                    return err(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": "every entry in `files` must be a non-empty string path" }),
                    );
                };
                // ABSOLUTE ONLY. CDP resolves a relative path against the
                // BROWSER's working directory, not the caller's, so a relative
                // path does not fail — it attaches the wrong file or nothing,
                // and the upload under test then "passes" against a file the
                // author never chose.
                if !std::path::Path::new(p).is_absolute() {
                    return err(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": format!(
                            "file path must be absolute, got {p:?} — CDP resolves a relative \
                             path against the browser's working directory, not yours, so it \
                             would silently attach the wrong file"
                        ) }),
                    );
                }
                // EXISTENCE, checked before the round trip. `DOM.setFileInputFiles`
                // accepts a missing path and reports success; the page then sees
                // an input with a file that has no bytes, which reads as a broken
                // upload rather than as a bad request.
                if !std::path::Path::new(p).exists() {
                    return err(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": format!(
                            "no such file: {p:?} (resolved on the machine running Chrome). \
                             setFileInputFiles reports success for a missing path, so this is \
                             refused here rather than surfacing later as a broken upload"
                        ) }),
                    );
                }
            }
        }
        "type" | "scroll" | "back" | "extract" => {}
        other => {
            return err(StatusCode::BAD_REQUEST, json!({ "error": format!("unknown action: {other}") }))
        }
    }

    let (_page, mut cdp) = match connect_session(&session, None).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    crate::integrations::browser::touch_verb_for_session(&session);
    let ten = Duration::from_secs(10);

    match action.as_str() {
        "click" => {
            let out = if let Some(sel) = get_str("selector") {
                // Selector first, matching Python's precedence (AMUX-2272).
                chrome::click_selector(&mut cdp, &sel).await
            } else if let Some(i) = get_usize("index") {
                chrome::click_index(&mut cdp, i).await
            } else {
                let (x, y) = (get_f64("x").unwrap_or(0.0), get_f64("y").unwrap_or(0.0));
                chrome::click_xy(&mut cdp, x, y)
                    .await
                    .map(|()| json!({ "ok": true, "clicked": { "x": x, "y": y } }))
            };
            match out {
                Ok(v) if v.get("error").is_some() => err(StatusCode::BAD_REQUEST, v),
                Ok(v) => Json(v).into_response(),
                // AMUX-98: a malformed selector (e.g. Playwright-style
                // `text=...` reaching a native `querySelector`) throws
                // INSIDE click_selector's own eval, before click_outcome
                // ever sees a NOELEMENT/NOTVISIBLE/STALE string to classify
                // — so this arm used to blanket-502 the caller's own typo.
                // cdp_status now tells that PageException apart from a real
                // transport failure by TYPE.
                Err(e) => err(cdp_status(&e), json!({ "error": with_cause(&e) })),
            }
        }
        "type" => {
            let text = get_str("text").unwrap_or_default();
            match cdp.call("Input.insertText", json!({ "text": text }), ten).await {
                Ok(_) => Json(json!({ "ok": true, "typed": text.chars().count(), "backend": "native" }))
                    .into_response(),
                Err(e) => err(cdp_status(&e), json!({ "error": with_cause(&e) })),
            }
        }
        "input" => {
            let idx = get_usize("index").unwrap_or(0);
            let text = get_str("text").unwrap_or_default();
            // Focus + clear element[idx] of the /state list, then type.
            let js = format!(
                "(function(){{var els=window.__amux_els||[];var e=els[{idx}];\
                 if(!e)return 'NOELEMENT';if(!e.isConnected)return 'STALE';\
                 e.scrollIntoView({{block:'center'}});e.focus();\
                 if('value' in e)e.value='';return 'FOCUSED';}})()"
            );
            let raw = match cdp.eval(&js, 20).await {
                Ok(r) => r,
                Err(e) => return err(cdp_status(&e), json!({ "error": with_cause(&e) })),
            };
            if raw.as_str() != Some("FOCUSED") {
                let v = chrome::click_outcome(
                    &raw,
                    &format!("element index {idx}"),
                    "indexes come from GET /api/browser/state — re-fetch it",
                );
                return err(StatusCode::BAD_REQUEST, v);
            }
            match cdp.call("Input.insertText", json!({ "text": text }), ten).await {
                Ok(_) => Json(json!({ "ok": true, "index": idx, "typed": text.chars().count() }))
                    .into_response(),
                Err(e) => err(cdp_status(&e), json!({ "error": with_cause(&e) })),
            }
        }
        "key" => {
            let k = get_str("key").unwrap_or_default();
            match chrome::dispatch_key(&mut cdp, &k).await {
                Ok(()) => Json(json!({ "ok": true, "key": k })).into_response(),
                Err(e) => err(cdp_status(&e), json!({ "error": with_cause(&e) })),
            }
        }
        "scroll" => {
            let dy = body.get("dy").and_then(Value::as_i64).unwrap_or(500);
            match cdp.eval(&format!("window.scrollBy(0,{dy})"), 10).await {
                Ok(_) => Json(json!({ "ok": true, "dy": dy })).into_response(),
                Err(e) => err(cdp_status(&e), json!({ "error": with_cause(&e) })),
            }
        }
        "eval" => {
            let script = get_str("script").unwrap_or_default();
            match cdp.eval(&script, 30).await {
                Ok(mut result) => {
                    // AMUX-3062: an eval result over the cap was truncated with the
                    // notice appended INTO the string while the envelope still read
                    // {ok:true} — silent data loss that reads as success (amux-gtm
                    // scraped a 10589-char page, got 23 of 34 records, and the run
                    // would have passed review). Surface the cut in the ENVELOPE so
                    // a run/reviewer detects it structurally, not by substring match.
                    let mut cut: Option<(usize, usize)> = None; // (full_len, cap)
                    if let Some(s) = result.as_str() {
                        let cap = chrome::obs_eval_cap();
                        cut = chrome::obs_truncation(s, cap).map(|full| (full, cap));
                        result = json!(chrome::obs_cap(s, cap));
                    }
                    // `data.result` mirrors the browser-use CLI shape some
                    // dashboard readers still consume (viewport probe).
                    let mut env = json!({
                        "ok": true,
                        "result": result,
                        "data": { "result": result },
                        "backend": "native",
                    });
                    if let Some((full, cap)) = cut {
                        // Two-fixes log signal: the next silent truncation is now
                        // visible in the request log without comparing to a direct
                        // fetch — grep "browser eval truncated".
                        tracing::warn!(
                            session = %session, full_length = full, cap,
                            "browser eval truncated at the cap — {} chars past the cap are NOT in the result (AMUX-3062); envelope carries truncated=true",
                            full - cap
                        );
                        env["truncated"] = json!(true);
                        env["full_length"] = json!(full);
                        env["eval_cap"] = json!(cap);
                    }
                    Json(env).into_response()
                }
                // Page exceptions AND transport failures both answer 400
                // with the description — Python's eval contract.
                Err(e) => err(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": with_cause(&e), "backend": "native" }),
                ),
            }
        }
        // TUBES-2343: attach local files to an <input type=file>.
        //
        // Every other action here drives the page through `Runtime.evaluate`,
        // and this one CANNOT: `input.files` is not assignable from page script
        // by design — that restriction is the browser's file-upload security
        // model, not an oversight to work around. So the only route is the one
        // the debugger protocol reserves for a driver,
        // `DOM.setFileInputFiles`, which is also what page.setInputFiles is
        // built on in Playwright.
        //
        // Consequence for the reader: this is the one action whose paths are
        // resolved on the machine running CHROME, not by the page. Both
        // validations above (absolute, exists) are about that seam.
        "files" => {
            let selector = get_str("selector").unwrap_or_default();
            let files: Vec<String> = body
                .get("files")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            match chrome::set_input_files(&mut cdp, &selector, &files).await {
                Ok(v) if v.get("error").is_some() => err(StatusCode::BAD_REQUEST, v),
                Ok(v) => Json(v).into_response(),
                // Same AMUX-98 class as "click": set_input_files' own probe
                // interpolates the caller's selector into a querySelector
                // eval, so a malformed one throws here too.
                Err(e) => err(cdp_status(&e), json!({ "error": with_cause(&e) })),
            }
        }
        "back" => match cdp.eval("history.back()", 10).await {
            Ok(_) => Json(json!({ "ok": true })).into_response(),
            Err(e) => err(cdp_status(&e), json!({ "error": with_cause(&e) })),
        },
        "extract" => match state_payload(&mut cdp, &session).await {
            Ok(v) => Json(v).into_response(),
            Err(r) => r,
        },
        "wait" => {
            let timeout_ms =
                body.get("timeout").and_then(Value::as_u64).unwrap_or(5000).min(60_000);
            let (what, probe) = if let Some(sel) = get_str("selector").filter(|s| !s.trim().is_empty()) {
                (format!("selector {sel:?}"), format!("!!document.querySelector({})", json!(sel)))
            } else {
                let text = get_str("text").unwrap_or_default();
                (
                    format!("text {text:?}"),
                    format!(
                        "(((document.body&&document.body.innerText)||'').indexOf({}) !== -1)",
                        json!(text)
                    ),
                )
            };
            let started = std::time::Instant::now();
            let deadline = started + Duration::from_millis(timeout_ms);
            // Declares requested-wait semantics to the request log
            // (AMUX-3513): a wait's latency is the CALLER's budget, and a
            // timed-out wait is a 200 at exactly that budget — twelve of
            // those read as a 7.1x /api/browser p95 regression to the
            // latency detector, which now skips rows carrying this marker.
            let slow_ok = [("x-amux-slow-ok", "wait-budget")];
            loop {
                match cdp.eval(&probe, 10).await {
                    Ok(v) if v.as_bool() == Some(true) => {
                        return (
                            slow_ok,
                            Json(json!({
                                "ok": true,
                                "found": what,
                                "waited_ms": started.elapsed().as_millis() as u64,
                            })),
                        )
                            .into_response();
                    }
                    Ok(_) => {}
                    // AMUX-98: `probe` interpolates the caller's own
                    // selector when one was given (the text-search branch
                    // above has no selector to be malformed), so this is
                    // the same class of caller mistake as "click".
                    Err(e) => return err(cdp_status(&e), json!({ "error": with_cause(&e) })),
                }
                if std::time::Instant::now() >= deadline {
                    // A timeout is an OUTCOME, not a malformed request: 200
                    // with ok:false, like the CLI shape Python relays.
                    return (
                        slow_ok,
                        Json(json!({
                            "ok": false,
                            "error": format!("timed out after {timeout_ms}ms waiting for {what}"),
                        })),
                    )
                        .into_response();
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
        "viewport" => {
            let (w, h) = viewport_wh.unwrap_or((1280, 900));
            let r = cdp
                .call(
                    "Emulation.setDeviceMetricsOverride",
                    // mobile:true under 500px wide — phone-width testing is
                    // what AF-18 exists for.
                    json!({ "width": w, "height": h, "deviceScaleFactor": 0, "mobile": w <= 500 }),
                    ten,
                )
                .await;
            match r {
                Ok(_) => {
                    let seen = cdp
                        .eval("({w:window.innerWidth,h:window.innerHeight})", 10)
                        .await
                        .unwrap_or(Value::Null);
                    Json(json!({ "ok": true, "viewport": { "w": w, "h": h }, "measured": seen }))
                        .into_response()
                }
                Err(e) => err(cdp_status(&e), json!({ "error": with_cause(&e) })),
            }
        }
        _ => unreachable!("validated above"),
    }
}

#[derive(Deserialize, Default)]
struct InspectQuery {
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    clear: Option<String>,
    #[serde(default)]
    limit: Option<String>,
    #[serde(default, rename = "type")]
    type_: Option<String>,
}

async fn inspect(headers: HeaderMap, Query(q): Query<InspectQuery>) -> Response {
    let session = resolve_session(q.session.as_deref(), &headers);
    let clear = matches!(q.clear.as_deref(), Some("1") | Some("true") | Some("yes"));
    let limit = q
        .limit
        .as_deref()
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(200)
        .clamp(1, 500);
    let (_page, mut cdp) = match connect_session(&session, None).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    match inspect_payload(&mut cdp, clear, limit).await {
        Ok(data) => {
            // `type` filter, ported: keep url/installed/counts + one bucket.
            let tf = q.type_.as_deref().unwrap_or("all");
            if matches!(tf, "console" | "network" | "errors" | "resources") {
                return Json(json!({
                    "url": data.get("url"),
                    "installed": data.get("installed"),
                    tf: data.get(tf),
                    "counts": data.get("counts"),
                }))
                .into_response();
            }
            Json(data).into_response()
        }
        Err(r) => r,
    }
}

async fn inspect_payload(cdp: &mut chrome::CdpClient, clear: bool, limit: usize) -> Result<Value, Response> {
    // Install the capture shim first (idempotent per page) so counts start
    // accruing from the first inspect even when nobody navigated natively;
    // Resource Timing back-fills earlier requests either way.
    let _ = cdp.eval(chrome::CAPTURE_JS, 15).await;
    cdp.eval(&chrome::inspect_js(limit, clear), 15)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, json!({ "error": with_cause(&e) })))
}

async fn inspect_clear(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let session = resolve_session(body.get("session").and_then(Value::as_str), &headers);
    let (_page, mut cdp) = match connect_session(&session, None).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    match inspect_payload(&mut cdp, true, 1).await {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

#[derive(Deserialize, Default)]
struct SearchQuery {
    #[serde(default)]
    q: Option<String>,
}

/// Python's google-scrape search, mechanically: navigate the dedicated
/// `search` session's tab, settle, scrape result cards.
async fn search(Query(sq): Query<SearchQuery>) -> Response {
    let Some(q) = sq.q.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "q required" }));
    };
    let url = format!("https://www.google.com/search?q={}", urlenc(&q));
    let (_page, mut cdp) = match connect_session("search", Some(&url)).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Err(e) = chrome::navigate_and_settle(&mut cdp, &url).await {
        return err(StatusCode::BAD_GATEWAY, json!({ "error": with_cause(&e) }));
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let scrape = r#"
        Array.from(document.querySelectorAll('div.g')).slice(0,8).map(el => ({
            title: (el.querySelector('h3') || {}).textContent || '',
            url: (el.querySelector('a') || {}).href || '',
            snippet: (el.querySelector('.VwiC3b, [data-sncf]') || {}).textContent || ''
        })).filter(r => r.title)
    "#;
    match cdp.eval(scrape, 30).await {
        Ok(v) if v.is_array() => Json(json!({ "results": v })).into_response(),
        Ok(v) => Json(json!({ "results": [], "raw": v, "note": "scrape matched nothing — google may be showing a consent/challenge page" })).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, json!({ "error": with_cause(&e) })),
    }
}

fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn sessions() -> Response {
    let bindings = chrome::session_bindings().await;
    Json(json!({
        "sessions": bindings
            .into_iter()
            .map(|(name, target, alive)| json!({
                "name": name,
                "status": if alive { "active" } else { "closed" },
                "target": target,
            }))
            .collect::<Vec<_>>(),
    }))
    .into_response()
}

async fn pw_profiles_list() -> Response {
    Json(json!({ "profiles": chrome::pw_profiles(&chrome::amux_home()) })).into_response()
}

#[derive(Deserialize, Default)]
struct SaveProfileBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    label: Option<String>,
}

async fn save_profile(headers: HeaderMap, body: Option<Json<SaveProfileBody>>) -> Response {
    let Json(b) = body.unwrap_or_default();
    let session = resolve_session(b.session.as_deref(), &headers);
    let mut name = b.name.unwrap_or_default().trim().to_string();
    if name.is_empty() {
        // Python defaults to the session's active profile; natively the one
        // running browser's profile IS that (or 'default' with none running).
        name = {
            // The SESSION'S browser, not "the" browser (AMUX-3828): with
            // several running, saving the wrong profile would write one
            // worker's auth state under another's name.
            let guard = chrome::RUNNING.lock().expect("browser registry poisoned");
            guard
                .values()
                .find(|r| !session.is_empty() && r.started_by == session)
                .or_else(|| if guard.len() == 1 { guard.values().next() } else { None })
                .map(|r| r.profile.clone())
                .unwrap_or_default()
        };
        if name.trim().is_empty() {
            name = "default".into();
        }
    }
    let label = b.label.unwrap_or_default().trim().to_string();
    let mut host = b.host.unwrap_or_default().trim().to_lowercase();
    if host.is_empty() {
        // Best-effort: derive from the session's current page. No browser →
        // the registration still succeeds with no domain, like Python when
        // its eval fails.
        if let Ok(page) = chrome::resolve_page(&session, None).await {
            if let Ok(mut cdp) = chrome::CdpClient::connect(&page.ws_url).await {
                if let Ok(v) = cdp.eval("location.hostname", 10).await {
                    host = v.as_str().unwrap_or("").trim().to_lowercase();
                }
            }
        }
    }
    match chrome::registry_register(&chrome::amux_home(), &name, &host, &label) {
        Ok(entry) => Json(json!({
            "success": true,
            "profile": name,
            "host": host,
            "domains": entry.get("domains").cloned().unwrap_or_else(|| json!([])),
            "label": entry.get("label").cloned().unwrap_or_else(|| json!("")),
        }))
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": with_cause(&e) })),
    }
}

/// POST /api/browser/agent — an honest 501, deliberately NOT a port. The
/// Python endpoint runs a server-side model loop (Computer Use driving
/// browser-use). A model + prompt loop pinned inside the harness is the
/// D1/D3 shape: it cannot improve as models improve, and it spends the
/// harness's judgment where the WORKER's model should spend its own. The
/// exit is the amux worker driving the browser itself through the native
/// verbs below — capability that compounds with the model.
async fn agent() -> Response {
    err(
        StatusCode::NOT_IMPLEMENTED,
        json!({
            "error": "the model-driven browser agent loop is not implemented on this server",
            "capability": "POST /api/browser/agent — server-side vision/act loop over the page",
            "use_instead": [
                "POST /api/browser/start", "POST /api/browser/navigate",
                "GET /api/browser/state (indexed elements)", "GET /api/browser/screenshot",
                "POST /api/browser/action (click/type/input/key/scroll/eval/wait/viewport)",
                "GET /api/browser/inspect",
            ],
            "why": "a server-side agent loop pins a model + prompt inside the harness; the \
                    session's own model drives the browser through the native verbs instead, \
                    so browsing capability compounds as models improve (ethos D1/D3)",
        }),
    )
}

async fn catalog_404(uri: OriginalUri) -> Response {
    catalog_body(uri.0.path())
}

/// Name the routes (ported from Python): two sessions independently guessed
/// /api/browser/status for /state and read the bare "not found" as "the
/// browser API is down" — an error that sends you chasing the wrong cause is
/// the bug, not the typo.
fn catalog_body(path: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": format!("browser route not found: {path}"),
            "routes": [
                "GET /api/browser/status", "GET /api/browser/state", "GET /api/browser/screenshot",
                "GET /api/browser/profiles", "GET /api/browser/pw-profiles", "GET /api/browser/sessions",
                "GET /api/browser/inspect", "GET /api/browser/search",
                "POST /api/browser/start (profile, url, session; viewport at launch via device or width+height)",
                "POST /api/browser/navigate", "POST /api/browser/action",
                "POST /api/browser/stop", "POST /api/browser/inspect/clear",
                "POST /api/browser/save-profile", "POST /api/browser/profile/create",
                "DELETE /api/browser/profile/{name}",
                "POST /api/browser/agent (answers 501 — the session's model drives the native verbs)",
            ],
            "actions": ["click (selector|index|x,y)", "type", "input", "key",
                        "scroll", "eval", "wait", "extract", "back",
                        "viewport (width+height, or device=iphone|iphone-se|ipad|desktop)",
                        "files (selector + files[]: absolute paths, sets an <input type=file>)"],
            "eval_contract": "script must be a bare EXPRESSION; a `return` statement yields null",
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Tests — schema + routing only (live CDP paths need a real Chrome; the
// gated e2e lives in integrations::browser's #[ignore]d test + AMUX-2598's
// scripted run).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn app() -> Router {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        std::mem::forget(dir);
        let state = AppState {
            secrets: std::sync::Arc::new(crate::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
            store,
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        Router::new().nest("/api/browser", routes()).with_state(state)
    }

    async fn send(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<&str>,
    ) -> (StatusCode, Value, bool) {
        let mut req = axum::http::Request::builder().method(method).uri(uri);
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                axum::body::Body::from(b.to_string())
            }
            None => axum::body::Body::empty(),
        };
        let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let proxied = res.headers().get("x-amux-answered-by").is_some();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v, proxied)
    }

    /// AMUX-3886. `with_cause` is the renderer every error body in this file
    /// goes through, and the ONLY thing it has to do is not drop the chain.
    ///
    /// `tests/browser_errors_carry_cause.rs` proves the call sites USE it. This
    /// proves it is worth using: without this, `with_cause` could be rewritten
    /// to `format!("{e}")` and every check in the pair would stay green while
    /// the 502 went back to being undiagnosable.
    #[test]
    fn with_cause_keeps_the_whole_chain_and_plain_display_does_not() {
        let e = anyhow::anyhow!("tcp connect error: Connection refused (os error 61)")
            .context("client error (Connect)")
            .context("error sending request for url (http://127.0.0.1:49731/json/list)");

        let rendered = with_cause(&e);
        for frame in [
            "error sending request for url",
            "client error (Connect)",
            "Connection refused (os error 61)",
        ] {
            assert!(rendered.contains(frame), "with_cause dropped {frame:?}: {rendered}");
        }
        // CONTROL: the thing this replaced keeps ONLY the outermost frame. If
        // this stops holding, plain Display started carrying causes and the
        // assertions above no longer distinguish the two renderers.
        let plain = e.to_string();
        assert!(
            !plain.contains("Connection refused"),
            "the control has stopped holding: plain Display now carries the cause ({plain})"
        );
        assert!(rendered.len() > plain.len(), "the chain must ADD to the message");
    }

    /// AMUX-3672. A wedged browser and a rejected call must not share a status,
    /// because `/api/logs/analyze` groups by (status, method, target) and would
    /// otherwise fold two different faults into one row.
    ///
    /// The specimen: `Emulation.setDeviceMetricsOverride timed out after 10s`
    /// on 2026-08-24 15:57, which was five Chromes contending one profile
    /// (AMUX-3674). Diagnosing it needed the error BODY off the card, because
    /// the grouping could not say "this one is a hang".
    ///
    /// This citation read AMUX-3669 until AMUX-3688. That card is an autofix
    /// LATENCY report about `/api/sessions-git`; the stray-Chrome work is
    /// AMUX-3674. Six comments across three files carried the wrong id, all from
    /// writing it into prose before the card existed. AMUX-3669's own body
    /// records the slip. Commits 02197674 and a19bedbf still quote the wrong id
    /// in their subjects and cannot be rewritten.
    #[test]
    fn a_cdp_timeout_is_504_and_a_protocol_error_is_502() {
        let timeout = anyhow::Error::new(chrome::CdpTimeout {
            method: "Emulation.setDeviceMetricsOverride".into(),
            secs: 10,
        });
        assert_eq!(cdp_status(&timeout), StatusCode::GATEWAY_TIMEOUT);
        // The wording is quoted in existing card bodies and the AMUX-3207
        // notes, so it must not drift — but nothing about the STATUS depends on
        // it any more, which is the point of deciding on the type.
        assert_eq!(
            timeout.to_string(),
            "CDP Emulation.setDeviceMetricsOverride timed out after 10s"
        );

        // CONTROL: an ordinary CDP failure stays 502. A change that answered
        // 504 for everything would pass the assertion above and relabel every
        // browser fault as a hang — the same row-folding defect, new status.
        let protocol = anyhow::anyhow!("CDP websocket closed during Page.navigate");
        assert_eq!(cdp_status(&protocol), StatusCode::BAD_GATEWAY);

        // CONTROL: a timeout wrapped in CONTEXT is still a timeout. `?` adds
        // context freely on this path, and a downcast inspecting only the
        // outermost error would silently regress to 502 the first time someone
        // wrote `.context(...)`.
        let wrapped = anyhow::Error::new(chrome::CdpTimeout { method: "X".into(), secs: 1 })
            .context("while resizing the viewport");
        assert_eq!(cdp_status(&wrapped), StatusCode::GATEWAY_TIMEOUT);
    }

    /// AF-381: a launch the CALLER can resolve is 409, not 502.
    ///
    /// `start` mapped every launch failure to BAD_GATEWAY, so "another Chrome
    /// already holds this --user-data-dir" — whose own hint says "Try again, or
    /// GET /api/browser/status and stop it first" — went out as "the upstream is
    /// broken, your request was fine". Measured live on 2026-09-03: 3 such 502s
    /// in a 24h window carrying that exact body.
    #[test]
    fn a_delegated_launch_is_409_and_a_real_launch_failure_stays_502() {
        let msg = "Chrome (pid Some(33823)) exited exit status: 0 before CDP on port 64178 \
                   came up — exit 0 is the delegation signature: another Chrome already holds \
                   this --user-data-dir.";
        let delegated = anyhow::Error::new(chrome::ProfileDelegated(msg.into()));
        assert_eq!(start_status(&delegated), StatusCode::CONFLICT);

        // CONTROL 1, and the one that matters: a launch that genuinely failed
        // must stay 502. A classifier answering 409 for everything passes the
        // assertion above and hides every real browser fault behind a status
        // nothing alerts on — the inverse of the bug being fixed.
        let real = anyhow::anyhow!(
            "Chrome (pid Some(41)) exited exit status: 1 before CDP on port 9 came up"
        );
        assert_eq!(start_status(&real), StatusCode::BAD_GATEWAY);

        // CONTROL 2: wrapped in CONTEXT it is still delegated. `?` adds context
        // freely on this path and a downcast reading only the outermost error
        // regresses to 502 the first time someone writes `.context(...)` —
        // exactly the trap cdp_status's own third control names.
        let wrapped = anyhow::Error::new(chrome::ProfileDelegated(msg.into()))
            .context("while starting the browser");
        assert_eq!(start_status(&wrapped), StatusCode::CONFLICT);

        // CONTROL 3: the MESSAGE is unchanged. The body is quoted in AF-443's
        // card, in the log-sweep contract's cleared AMUX-3689 line, and in the
        // hint callers read. Typing the error must not reword it.
        assert_eq!(delegated.to_string(), msg);

        // MO-3146: the PRE-SPAWN version of the conflict is also 409, and its
        // body is explicit that retrying is unsafe. This is what prevents an
        // agent's retry watcher from turning one locked profile into dozens of
        // user-visible tabs.
        let external = anyhow::Error::new(chrome::ExternalProfileInUse {
            profile: "ethan-tubescience".into(),
            user_data_dir: "/Users/ethan/Library/Application Support/Google/Chrome".into(),
            owner_pid: 97230,
        });
        let (status, body) = start_failure(&external);
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error_code"], "human_chrome_profile_in_use");
        assert_eq!(body["retryable"], false);
        assert_eq!(body["spawned"], false);
        assert_eq!(body["lock_owner_pid"], 97230);
        assert_eq!(body["profile"], "ethan-tubescience");
        assert!(body["use_instead"]["live_browser"].as_str().unwrap().contains("/chrome-cdp"));
        let error = body["error"].as_str().unwrap();
        assert!(error.contains("every retry would open another tab"), "{error}");
        assert!(error.contains("Do not retry"), "{error}");
    }

    /// The SEAM between "what Chrome did" and "which error type gets built".
    ///
    /// `a_delegated_launch_is_409...` pins the classifier and this pins the
    /// decision that feeds it. Neither reaches the other: mutating the
    /// construction site to never build `ProfileDelegated` passed the entire
    /// suite, because the classifier's test hands it a value the launch loop
    /// never had to produce. Same shape as AF-438, closed the same way.
    #[cfg(unix)]
    #[test]
    fn exit_zero_before_cdp_is_delegation_and_any_other_code_is_a_real_failure() {
        use std::os::unix::process::ExitStatusExt;
        let code = |c: i32| std::process::ExitStatus::from_raw(c << 8);
        assert!(
            chrome::is_delegation_exit(&code(0)),
            "exit 0 before CDP is the delegation signature — another Chrome took the URL"
        );
        // The CONTROL: a real launch failure must not read as delegation, or
        // every broken start answers 409 and stops alerting.
        assert!(!chrome::is_delegation_exit(&code(1)), "exit 1 is a genuine launch failure");
        assert!(!chrome::is_delegation_exit(&code(127)), "exit 127 is a genuine launch failure");
    }

    /// AMUX-98. A malformed selector (Playwright-style `text=...` syntax
    /// reaching a native `document.querySelector`) throws INSIDE the page,
    /// which `CdpClient::eval` now carries as a `PageException` rather than
    /// a bare anyhow message — the caller's own mistake, same class as the
    /// NOELEMENT/NOTVISIBLE/STALE sentinels `click_outcome` already answers
    /// 400 for, not amux's infrastructure failing (502).
    #[test]
    fn a_page_exception_is_400_not_502() {
        let page_ex = anyhow::Error::new(chrome::PageException(
            "SyntaxError: Failed to execute 'querySelector' on 'Document': \
             'text=Create credentials' is not a valid selector."
                .into(),
        ));
        assert_eq!(cdp_status(&page_ex), StatusCode::BAD_REQUEST);
        // Message text is unchanged from the old bail!("eval: {desc}") this
        // replaced, so nothing reading the error body sees a diff.
        assert!(page_ex.to_string().starts_with("eval: SyntaxError"));

        // CONTROL: a real transport failure is untouched by this change —
        // still 502, not folded into the new 400 case.
        let protocol = anyhow::anyhow!("CDP websocket closed during Page.navigate");
        assert_eq!(cdp_status(&protocol), StatusCode::BAD_GATEWAY);

        // CONTROL: a timeout still wins over both of the above.
        let timeout = anyhow::Error::new(chrome::CdpTimeout { method: "Y".into(), secs: 2 });
        assert_eq!(cdp_status(&timeout), StatusCode::GATEWAY_TIMEOUT);

        // CONTROL: a PageException wrapped in CONTEXT is still a
        // PageException — same reasoning as the CdpTimeout control above.
        let wrapped = anyhow::Error::new(chrome::PageException("boom".into()))
            .context("while clicking a selector");
        assert_eq!(cdp_status(&wrapped), StatusCode::BAD_REQUEST);
    }

    /// AMUX-3403: an unknown field posted to start is CAPTURED, not silently
    /// dropped — the serde seam that feeds `ignored_fields`. Both of the wrong
    /// guesses that motivated the card land in `extra`; the fields that
    /// became real ones do not.
    #[test]
    fn start_body_captures_unknown_fields_and_owns_viewport_params() {
        let b: StartBody = serde_json::from_str(
            r#"{"profile":"default","url":"x","viewport":"iphone","emulate":"ipad","device":"iphone"}"#,
        )
        .unwrap();
        let mut extras: Vec<&String> = b.extra.keys().collect();
        extras.sort();
        assert_eq!(extras, ["emulate", "viewport"], "unknown fields must be captured for echo");
        assert_eq!(b.device.as_deref(), Some("iphone"), "device is a real start field now");
    }

    /// AMUX-3063 incident replay: the EXACT anonymous curl that destroyed
    /// amux-gtm's staged NetSuite login on 2026-08-20 09:24:57 — POST /start
    /// {"profile":"default"}, no session header, while another session's
    /// browser was running — must now refuse, naming the owner, and must
    /// refuse BEFORE any Chrome is touched (hermetic: seeded registry, temp
    /// home, no launch). Takeover by the OWNER's own session passes the guard
    /// (their browser, their restart).
    /// AMUX-3610, built from mvs-research's MEASURED specimen rather than a
    /// convenient one: pid alive 18.5h, ZERO tabs, `started_by` empty.
    ///
    /// They declined the takeover, correctly, and had no non-destructive move.
    /// The process being alive rules out a crashed lock, so nobody could justify
    /// a takeover on those grounds; zero tabs for eighteen hours says abandoned.
    /// Nothing in the API separated those two states.
    ///
    /// THE THIRD CELL IS THE ONE THAT KEEPS THIS HONEST. `tabs: None` (CDP did
    /// not answer) must NOT render as zero. Collapsing "I asked and got nothing"
    /// into "there are no tabs" rebuilds the exact ambiguity this card is about,
    /// one field further down, and it is the reading that would let a caller
    /// destroy a live browser on the strength of a timeout.
    /// AMUX-3711: the refusal must name WHAT is open, because the counts pointed
    /// the wrong way.
    ///
    /// THE SPECIMEN IS THE FIXTURE, exactly as Ethan hit it on 2026-08-25. The
    /// refusal said "held 0.0h with 4 tab(s) open" while GET /api/browser/status
    /// showed those four targets were two `browser_ui` omnibox popups, one
    /// `iframe`, and ONE page: a live Google sign-in on the
    /// `hello-at-amux-io-google` profile. Both reported numbers argued for
    /// takeover ("0.0h" reads brand new, "4 tabs" reads trivial) while the thing
    /// at risk was the single most costly thing to destroy — the exact hazard
    /// the refusal's own first sentence names in the abstract and could not
    /// point at. `cdp_list` had the titles in hand and `.len()` threw them away.
    #[test]
    fn the_refusal_names_the_pages_it_is_protecting_not_just_a_target_count() {
        // The four targets Chrome actually reported, verbatim in shape.
        let targets = vec![
            json!({"type":"browser_ui","title":"Omnibox Popup","url":"chrome://omnibox-popup.top-chrome/"}),
            json!({"type":"browser_ui","title":"Omnibox Popup","url":"chrome://omnibox-popup.top-chrome/omnibox_popup_aim.html"}),
            json!({"type":"page","title":"Sign in - Google Accounts","url":"https://accounts.google.com/v3/signin/identifier?continue=https%3A%2F%2Fmail.google.com%2F&state=SECRET"}),
            json!({"type":"iframe","title":"check","url":"https://accounts.youtube.com/accounts/CheckConnection?pmpo=h"}),
        ];
        let pages = summarize_pages(&targets);
        assert_eq!(pages.len(), 1, "4 targets, 1 real page: {pages:?}");
        assert_eq!(pages[0].0, "Sign in - Google Accounts");
        assert_eq!(pages[0].1, "accounts.google.com");

        let started = 1_787_674_083i64;
        let v = takeover_refusal(
            "hello-at-amux-io-google",
            "",
            started,
            9999,
            Some(4),
            &pages,
            started + 120, // 0.0h, as reported
            Some("amux"),
            StartOrigin::NotLooked,
        );
        let e = v["error"].as_str().unwrap();
        assert!(
            e.contains("Sign in - Google Accounts") && e.contains("accounts.google.com"),
            "the sentence a caller prints must name the sign-in it is about to destroy: {e}"
        );
        assert!(
            e.contains("1 page(s)"),
            "1 page is the honest number; '4 tab(s)' overstated it three-fold: {e}"
        );

        // NEVER THE FULL URL. This body is logged and pasted around, and a
        // sign-in URL carries continue= and state=. The host answers the
        // question; the query string only leaks.
        let whole = serde_json::to_string(&v).unwrap();
        assert!(!whole.contains("SECRET"), "the state parameter must not reach the body: {whole}");
        // Scoped to the `pages` array and the sentence, NOT the whole body: the
        // first draft asserted on the whole body and went red against correct
        // code, because `pages_note` says the words "continue=" while EXPLAINING
        // that it omits them. A probe that cannot tell a leak from a note about
        // leaks reports working code as broken.
        let rendered = format!("{}{}", serde_json::to_string(&v["running"]["pages"]).unwrap(), e);
        for leak in ["continue=", "state=", "?", "/v3/signin"] {
            assert!(
                !rendered.contains(leak),
                "{leak:?} reached the page evidence — host and title only: {rendered}"
            );
        }

        // The recommendation must not be one-directional. Before this card the
        // only advice offered was "very likely abandoned", i.e. the sole
        // recommendation always argued for the destructive action.
        let opts = serde_json::to_string(&v["your_options"]).unwrap();
        assert!(
            opts.contains("mid-task"),
            "the live-work case has to be named too, or the advice can only ever point at \
             takeover: {opts}"
        );
        assert!(!opts.contains("tabes"), "typo: {opts}");

        // AND the abandoned case must still read as abandoned, or this fix would
        // just have inverted the bias instead of removing it.
        let abandoned = takeover_refusal("default", "", started, 9999, Some(4), &[], started + 66_600, None, StartOrigin::NotLooked);
        let ae = abandoned["error"].as_str().unwrap();
        assert!(
            ae.contains("ZERO real pages") && ae.contains("no page state to lose"),
            "4 chrome-internal targets and nothing else is a browser nobody is using: {ae}"
        );
    }

    #[test]
    fn an_unattributed_holder_is_described_from_the_request_log_when_it_cannot_be_named() {
        let started = 1_700_000_000;
        let now = started + 600;

        // THE CASE THIS ENTRY'S COST IS ABOUT: a real browser on another host,
        // i.e. a person. Under "(unattributed)" it is indistinguishable from a
        // curl on loopback, and the caller is asked to decide anyway.
        let human = takeover_refusal(
            "default", "", started, 7623, Some(2),
            &[("Sign in".into(), "accounts.google.com".into())], now, Some("amux"),
            StartOrigin::Found {
                ip: "100.66.26.84".into(),
                ua: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)".into(),
            },
        );
        let e = human["error"].as_str().unwrap();
        // IN THE SENTENCE, which is the entry's actual ask — plenty of callers
        // print `.error` and nothing else.
        assert!(
            e.contains("100.66.26.84") && e.contains("Mozilla/5.0"),
            "the recoverable facts must reach the sentence, not just the body: {e}"
        );
        assert!(
            e.contains("PERSON"),
            "an IP and a user-agent still leave the caller to judge; the refusal is asked \
             to decide under time pressure, so it must say what they MEAN: {e}"
        );
        assert!(
            !e.contains("(unattributed)"),
            "'(unattributed)' names nobody and supports no decision — it must be replaced \
             once the origin is known: {e}"
        );

        // The cheap case must read differently, or the verdict is decoration.
        let agent = takeover_refusal(
            "default", "", started, 7623, Some(0), &[], now, Some("amux"),
            StartOrigin::Found { ip: "127.0.0.1".into(), ua: "curl/8.7.1".into() },
        );
        let ae = agent["error"].as_str().unwrap();
        assert!(
            ae.contains("127.0.0.1") && ae.contains("agent on this box") && !ae.contains("PERSON"),
            "loopback plus a scripted client is the OTHER verdict; if both render the same \
             the classification cannot inform anything: {ae}"
        );
    }

    #[test]
    fn a_failed_origin_lookup_is_not_reported_as_an_absent_one() {
        let started = 1_700_000_000;
        let now = started + 600;
        // THREE STATES, and this is the pair that collapses. "we looked and
        // found nothing" is evidence about the holder; "we did not look" is
        // evidence about us. Rendering both as no-origin is the one-output-
        // two-states failure the `attribution` field already guards one level up.
        let missing =
            takeover_refusal("default", "", started, 7623, Some(0), &[], now, None,
                             StartOrigin::NotFound);
        let absent =
            takeover_refusal("default", "", started, 7623, Some(0), &[], now, None,
                             StartOrigin::NotLooked);

        assert_ne!(
            missing["running"]["origin_source"], absent["running"]["origin_source"],
            "a lookup that RAN and found nothing must not read the same as one that never ran"
        );
        assert_eq!(absent["running"]["origin"], json!(null));
        assert!(
            missing["error"].as_str().unwrap().contains("no matching start row"),
            "when the log cannot recover the origin the refusal says so, rather than \
             falling back to a bare '(unattributed)' that hides which of the two happened"
        );
        // And the not-looked path must still be honest rather than silent.
        assert_eq!(absent["running"]["origin_source"], json!("not looked up"));
    }

    #[test]
    fn an_unattributed_holder_is_judgeable_from_the_refusal() {
        let started = 1_787_494_071i64; // 2026-08-23T10:07:51, the reported hold
        let now = started + 66_600; // 18.5h later
        let v = takeover_refusal("default", "", started, 7623, Some(0), &[], now, None, StartOrigin::NotLooked);

        assert_eq!(v["running"]["tabs_open"], json!(0));
        assert_eq!(v["running"]["held_h"], json!(18.5), "{v}");
        let e = v["error"].as_str().unwrap();
        assert!(e.contains("18.5h") && e.contains("ZERO tabs"),
                "the idle evidence must be in the SENTENCE — a caller who prints only .error \
                 is the caller who filed this: {e}");
        assert!(e.contains("takeover"), "the escape must survive in the sentence too: {e}");

        // ONE OUTPUT, TWO STATES. An empty `started_by` reads identically to a
        // session whose name is empty; `attribution` has to say which.
        let attr = v["running"]["attribution"].as_str().unwrap();
        assert!(
            attr.contains("no X-Amux-Session"),
            "an absent attribution must announce itself, or a consumer cannot tell 'nobody \
             claimed this' from 'a session named \"\"': {attr}"
        );
        let opts = serde_json::to_string(&v["your_options"]).unwrap();
        assert!(
            opts.contains("CANNOT be messaged"),
            "with no owner there is nobody to ask, and saying so IS the actionable part: {opts}"
        );

        // NAMED HOLDER: the normal fleet path exists and must be offered.
        let named = takeover_refusal(
            "default",
            "mvs-research",
            started,
            7623,
            Some(3),
            &[("Docs".into(), "docs.example.com".into())],
            now,
            Some("amux"),
            StartOrigin::NotLooked,
        );
        assert_eq!(named["running"]["attribution"], json!("session"));
        let nopts = serde_json::to_string(&named["your_options"]).unwrap();
        assert!(
            nopts.contains("amux send mvs-research"),
            "naming the holder turns a dead end into 'go message that session': {nopts}"
        );
        // The PROPERTY, not the wording: the tab evidence has to reach the
        // sentence a caller prints. This cell pinned the literal "3 tab(s)"
        // until AMUX-3711 replaced the count with the page list, which is a
        // strictly better answer to the same question — a wording pin would
        // have made improving the message look like a regression.
        let ne = named["error"].as_str().unwrap();
        assert!(
            ne.contains("Docs") && ne.contains("docs.example.com"),
            "the sentence must name what is open, not merely how much: {ne}"
        );
        assert_eq!(named["running"]["tabs_open"], json!(3), "the target count is still reported: {named}");
        assert_eq!(named["running"]["pages_open"], json!(1), "1 page out of 3 targets: {named}");

        // CDP SILENT: must not read as zero.
        let quiet = takeover_refusal("default", "", started, 7623, None, &[], now, None, StartOrigin::NotLooked);
        assert!(quiet["running"]["tabs_open"].is_null(), "{quiet}");
        let qe = quiet["error"].as_str().unwrap();
        assert!(
            qe.contains("unknown") && qe.contains("not the same as zero"),
            "an unanswered CDP port is a THIRD state and must say so — reading it as zero is \
             how a caller destroys a live browser on the strength of a timeout: {qe}"
        );
    }

    #[tokio::test]
    async fn cross_session_start_refuses_without_takeover_naming_the_owner() {
        let dir = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(dir.path());

        // AMUX-3063's incident, and the reason this test changed shape in
        // AMUX-3828: an anonymous `default` start killed amux-gtm's staged
        // NetSuite login. That was possible because the browser was a MACHINE
        // singleton, so any start replaced any browser. It is now impossible by
        // CONSTRUCTION rather than by refusal — `default` and `netsuite` are
        // different profiles and do not touch. Structural beats a guard the
        // caller has to respect.
        chrome::test_seed_running("netsuite", "amux-gtm", 424242);
        let app = app();
        let (status, _v, _) =
            send(&app, "POST", "/api/browser/start", Some(r#"{"profile":"default"}"#)).await;
        // The victim is UNHARMED — that is the property AMUX-3063 wanted, and
        // asserting it is worth more than asserting the 409 that used to stand
        // in for it.
        let survivor = chrome::running_snapshot_for("netsuite");
        assert_eq!(
            survivor.as_ref().map(|x| x.1.clone()),
            Some("amux-gtm".to_string()),
            "a start on ANOTHER profile must leave the staged browser alone (status {status})"
        );
        assert_eq!(survivor.map(|x| x.3), Some(424242), "same process, not a replacement");

        // STOP WHAT THIS TEST ACTUALLY STARTED (AMUX-3973).
        //
        // The `default` start above is not a stub. `netsuite` is seeded but
        // `default` is free, so the handler takes the real path and SPAWNS A
        // HEADED CHROME into `<tempdir>/playwright-auth/profile`. Nothing here
        // stopped it, so every `cargo test -p amux-server` on a machine with
        // Chrome installed left one live browser and an ~87 MB profile behind.
        // Measured 2026-08-31: 66 leaked profile dirs, 5.5 GB, oldest a day old,
        // and a dock full of empty windows that Ethan reported twice.
        //
        // IT IS HERMETIC ON CI AND NOT ON A LAPTOP, which is why it survived.
        // `chrome_binary()` probes two fixed macOS paths and then PATH; on a
        // Linux runner it returns None, the launch fails, and the assertions
        // above still pass because they only inspect the `netsuite` registry
        // entry. Same shape as AMUX-3962 and AMUX-3969: behaviour that depends
        // on the machine, green in the place nobody watches.
        //
        // Stopping rather than skipping the launch on purpose: the assertions
        // are about what the REAL start path does to a peer's entry, so
        // stubbing the spawn would leave them passing over code that no longer
        // runs. Stop it instead, and the TempDir drop can then remove the
        // profile it was holding open.
        let _ = chrome::stop_profile(dir.path(), "default").await;
        // ASSERTED, not assumed. A cleanup nobody checks is how this started:
        // the launch was already invisible here, and a silent stop would be too.
        assert!(
            chrome::running_snapshot_for("default").is_none(),
            "this test must not leave a browser running on `default`"
        );

        // AND THE SAME-PROFILE CASE STILL REFUSES. This is the cell that keeps
        // the guard honest: two Chromes on one user_data_dir corrupt it, so a
        // cross-session start onto an OCCUPIED profile must still 409 and name
        // the owner and the escape. Without this, "multiple browsers" would
        // have quietly become "no protection at all".
        chrome::test_clear_running();
        chrome::test_seed_running("netsuite", "amux-gtm", 424242);
        let (status, v, _) =
            send(&app, "POST", "/api/browser/start", Some(r#"{"profile":"netsuite"}"#)).await;
        chrome::test_clear_running();
        assert_eq!(status, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["running"]["started_by"], json!("amux-gtm"), "{v}");
        assert_eq!(v["running"]["profile"], json!("netsuite"), "{v}");
        assert!(
            v["error"].as_str().unwrap_or("").contains("takeover"),
            "the refusal must name the escape: {v}"
        );
    }

    /// The action schema answers 400 for malformed requests BEFORE any
    /// browser state is consulted — hermetic, message parity with Python.
    #[tokio::test]
    async fn action_schema_validates_before_browser_state() {
        let app = app();
        for (body, needle) in [
            (r#"{"action":"click"}"#, "selector, index, or x,y"),
            (r#"{"action":"eval"}"#, "script required"),
            (r#"{"action":"input","text":"x"}"#, "input needs index and text"),
            (r#"{"action":"key","key":"F13"}"#, "unsupported key"),
            (r#"{"action":"viewport"}"#, "viewport needs width+height, or device"),
            (r#"{"action":"wait"}"#, "wait needs selector or text"),
            (r#"{"action":"dance"}"#, "unknown action: dance"),
            // TUBES-2343.
            (r#"{"action":"files","files":["/tmp/x"]}"#, "files needs a selector"),
            (r##"{"action":"files","selector":"#f"}"##, "non-empty `files` array"),
            (r##"{"action":"files","selector":"#f","files":[]}"##, "non-empty `files` array"),
            (r##"{"action":"files","selector":"#f","files":[""]}"##, "must be a non-empty string"),
            // RELATIVE PATHS ARE REFUSED, and the message says why: CDP
            // resolves them against the BROWSER's cwd, so the attach would
            // silently succeed against the wrong file.
            (r##"{"action":"files","selector":"#f","files":["fixture.png"]}"##, "must be absolute"),
            (r##"{"action":"files","selector":"#f","files":["./fixture.png"]}"##, "must be absolute"),
        ] {
            let (status, v, proxied) = send(&app, "POST", "/api/browser/action", Some(body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {v}");
            assert!(!proxied, "{body} must answer natively");
            assert!(
                v["error"].as_str().unwrap_or("").contains(needle),
                "{body}: {v}"
            );
        }
        // A MISSING FILE IS A BAD REQUEST, NOT A BROKEN UPLOAD (TUBES-2343).
        // `DOM.setFileInputFiles` accepts a path that is not there and reports
        // SUCCESS, leaving the page with an input whose file has no bytes — so
        // the fault surfaces later, inside whatever upload was under test,
        // wearing the shape of a product bug. Refused here instead.
        let missing = std::env::temp_dir().join("tubes-2343-does-not-exist.png");
        let _ = std::fs::remove_file(&missing);
        let body = format!(
            r##"{{"action":"files","selector":"#f","files":[{}]}}"##,
            json!(missing.to_string_lossy())
        );
        let (status, v, _) = send(&app, "POST", "/api/browser/action", Some(&body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert!(v["error"].as_str().unwrap_or("").contains("no such file"), "{v}");

        // CONTROL: an existing absolute path passes the SCHEMA and is refused
        // only for want of a browser. Without this cell the assertions above
        // would all pass against a handler that rejected every `files` request,
        // which is a working schema and a dead action.
        let present = std::env::temp_dir().join("tubes-2343-present.png");
        std::fs::write(&present, b"x").expect("write fixture");
        let body = format!(
            r##"{{"action":"files","selector":"#f","files":[{}]}}"##,
            json!(present.to_string_lossy())
        );
        let (status, v, _) = send(&app, "POST", "/api/browser/action", Some(&body)).await;
        let _ = std::fs::remove_file(&present);
        assert_ne!(status, StatusCode::BAD_REQUEST, "a valid files request must clear the schema: {v}");

        // AND THE ACTION IS DISCOVERABLE. An action the contract does not list
        // reaches nobody, which is the gap this card was filed about — the
        // reporter read GET /api/browser, found no file verb, and concluded the
        // upload path could not be driven (ethos rule 1).
        let (_, v, _) = send(&app, "GET", "/api/browser", None).await;
        assert!(
            v["actions"].as_array().map(|a| a.iter().any(|x| x.as_str().unwrap_or("").starts_with("files"))).unwrap_or(false),
            "the contract must list `files`: {v}"
        );

        // start's viewport validation runs BEFORE any Chrome launch (AMUX-3403),
        // so a bad request is hermetic too.
        for (body, needle) in [
            (r#"{"device":"nokia"}"#, "unknown device"),
            (r#"{"width":390}"#, "width and height go together"),
        ] {
            let (status, v, _) = send(&app, "POST", "/api/browser/start", Some(body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {v}");
            assert!(v["error"].as_str().unwrap_or("").contains(needle), "{body}: {v}");
        }
        // navigate's own required field.
        let (status, v, _) = send(&app, "POST", "/api/browser/navigate", Some("{}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "url required");
        // search's.
        let (status, v, _) = send(&app, "GET", "/api/browser/search", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "q required", "{v}");
    }

    /// Post-cutover invariant: NO browser verb reaches a proxy. With no
    /// amux-launched Chrome the driver verbs answer an honest 409 naming
    /// /start, the catalog 404 answers unknown paths, /agent answers 501 — all
    /// natively, with no `x-amux-answered-by` header anywhere.
    ///
    /// This used to arm a fake python listener and assert it received nothing.
    /// AMUX-2906 deleted the forwarder, so that half could no longer fail and
    /// was removed (ethos rule 7). The `!proxied` assertions below are KEPT:
    /// unlike the fake listener, they would still fail if a proxy hop were
    /// ever reintroduced here, which makes them a live regression guard rather
    /// than theatre.
    #[tokio::test]
    async fn driver_verbs_answer_natively_never_proxy() {
        // AF-109: this test asserts 409-when-not-running, but connect_session
        // deliberately runs adopt_if_orphaned(&amux_home()) first (AC-325),
        // and with no home guard that probe reaches the DEVELOPER'S REAL
        // ~/.amux — on a machine with an amux-launched Chrome there is
        // genuinely something to adopt, so the test raced the adopt probe and
        // failed ~6.5% of runs (13/200 measured), while staying green in CI
        // where no Chrome exists. The sanctioned fix is the existing
        // HomeGuard: a temp home has no browser-running.json, so the adopt is
        // a deterministic no-op and the 409 is about the fixture, not about
        // whatever the host happens to be running.
        let dir = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(dir.path());
        let app = app();

        for (method, uri, body) in [
            ("GET", "/api/browser/state?session=t1", None),
            ("GET", "/api/browser/screenshot?session=t1", None),
            ("GET", "/api/browser/inspect?session=t1", None),
            ("GET", "/api/browser/search?q=rust", None),
            ("POST", "/api/browser/action", Some(r#"{"action":"click","selector":"a","session":"t1"}"#)),
            ("POST", "/api/browser/navigate", Some(r#"{"url":"https://example.com","session":"t1"}"#)),
            ("POST", "/api/browser/inspect/clear", Some(r#"{"session":"t1"}"#)),
        ] {
            let (status, v, proxied) = send(&app, method, uri, body).await;
            assert_eq!(status, StatusCode::CONFLICT, "{method} {uri}: {v}");
            assert!(!proxied, "{method} {uri} must not proxy");
            assert!(
                v["error"].as_str().unwrap_or("").contains("/api/browser/start"),
                "{method} {uri}: 409 must point at /start: {v}"
            );
        }

        // /agent: honest 501 naming the native verbs, never a proxy hop.
        let (status, v, proxied) =
            send(&app, "POST", "/api/browser/agent", Some(r#"{"task":"buy milk"}"#)).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{v}");
        assert!(!proxied);
        assert!(v["use_instead"].is_array(), "{v}");
        assert!(v["error"].as_str().unwrap().contains("agent loop"), "{v}");

        // Unknown routes: the ported catalog, natively.
        let (status, v, proxied) = send(&app, "GET", "/api/browser/definitely-not", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(!proxied);
        assert!(
            v["error"].as_str().unwrap().starts_with("browser route not found"),
            "{v}"
        );
        assert!(v["routes"].is_array() && v["actions"].is_array(), "{v}");

        // Local inventory verbs answer without a browser or a proxy.
        let (status, v, proxied) = send(&app, "GET", "/api/browser/sessions", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!proxied);
        assert!(v["sessions"].is_array(), "{v}");
        let (status, v, proxied) = send(&app, "GET", "/api/browser/pw-profiles", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!proxied);
        assert!(v["profiles"].is_array(), "{v}");

        // /status stays native and honest.
        let (status, v, proxied) = send(&app, "GET", "/api/browser/status", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!proxied);
        assert_eq!(v["running"], json!(false));
    }
}
