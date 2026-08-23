//! The invariant checks themselves (AMUX-2622).
//!
//! EVERY check in here is derived from an incident that actually happened in
//! this repo, and each one tests the INVARIANT the incident revealed rather
//! than the implementation detail that broke (spec §29). The incident is named
//! in the doc comment so the next person can tell whether a "simplification"
//! would re-open it.
//!
//! Each check ships with a negative control at the bottom of this file: a test
//! that INJECTS the failure and asserts the check reports it. Per AMUX-2624, a
//! check that has never been demonstrated failing is not a valid health check —
//! this repo has shipped a green `if True:` fixture, a grep that could not
//! match, and a spin-catcher that ranked sleeping threads, all of which "passed".

use super::{InvariantResult, Status};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// 1. Route contract: every path a CLIENT calls must be mounted.
// ---------------------------------------------------------------------------

/// INCIDENT: `POST /api/workers/<n>/send` returned 405 after the Python
/// retirement while `/api/sessions/<n>/send` returned 200. The installed CLI
/// posts to the canonical spelling, so `amux send` degraded fleet-wide to raw
/// tmux keystroke injection — unstamped, unaudited, delivery unverified — and
/// two long inter-session messages were lost before a human noticed.
///
/// INVARIANT: a path that a shipped client (SPA or CLI) calls must resolve to a
/// mounted route. This is the check the spec names explicitly: "this should
/// have caught the /api/workers/<name>/send 405 before production".
///
/// Deliberately compares against the ROUTER'S OWN TABLE rather than a
/// hand-written expectation list, and normalises `{param}` segments, so adding
/// a caller without a route fails even if nobody remembers to update a fixture.
/// Paths the SPA calls that THIS SERVER NEVER OWNS — the cloud gateway answers
/// them, in front of this process, in the deployment where they exist at all.
///
/// NOT an environment branch (single-codebase rule): these are not served by
/// amux-server in cloud either, so the statement "this server does not own
/// them" is true everywhere and needs no `if IS_CLOUD`. Verified 2026-08-11
/// against cloud/gateway/gateway.py, which handles each one.
///
/// They are excluded because a failure list that can never reach zero stops
/// being read — the same reason the extractor refuses to guess a path. Seven
/// permanent rows would have trained everyone to skim past the real ones.
const GATEWAY_OWNED: &[&str] =
    &["/api/gateway/", "/api/stripe/", "/api/cloud-logout"];

/// An entry ending in `/` is a PREFIX (a whole family); one without is an EXACT
/// path. Applying prefix logic to both over-excluded — `/api/cloud-logout-extra`
/// matched `/api/cloud-logout` and would have been silently dropped from the
/// census. Caught by this function's own test, which is why it asserts the
/// near-misses and not just the hits: an exclusion list that swallows a sibling
/// hides exactly the work it was meant to make visible (ethos rule 1's
/// over-filtering corollary).
fn gateway_owned(path: &str) -> bool {
    GATEWAY_OWNED.iter().any(|p| {
        if let Some(prefix) = p.strip_suffix('/') {
            path == prefix || path.starts_with(p)
        } else {
            path == *p
        }
    })
}

/// Families whose ABSENCE is a documented product state with a GUARDED caller
/// (AMUX-3468). `/api/tunnel/*`: the python-era tunnel API was never ported;
/// the one caller (`amux tunnel`, AF-63) PREFLIGHTS /api/tunnel/status and
/// prints "not available in this server build" instead of failing blind — so
/// the census's own why_it_matters ("silent capability loss unless the client
/// fails loudly") does not apply, and a permanent red here trains readers to
/// skim the rows that matter (the AF-132 lesson). Entries are prefixes ending
/// in `/`. The exclusion is SELF-EXPIRING both ways: if the family gets
/// mounted, the stale entry FAILS the census naming itself for deletion; and
/// if the guarded caller is ever removed, the call site disappears from the
/// census with it. Porting-or-removing tunnel is a product call (Ethan's),
/// tracked on AMUX-3468.
const CALLER_GUARDED_ABSENT: &[&str] = &["/api/tunnel/"];

fn caller_guarded_absent(path: &str) -> bool {
    CALLER_GUARDED_ABSENT.iter().any(|p| path.starts_with(p))
}

pub fn route_callers_have_routes(
    mounted: &[(&str, &[&str])],
    callers: &[CallerPath],
) -> Vec<InvariantResult> {
    const ID: &str = "route.callers_have_routes";
    if callers.is_empty() {
        // An extractor that found nothing is broken, not vindicated. This is
        // the empty-grep trap: a probe that could not match reports the same
        // silence as a system with no problems.
        return vec![InvariantResult::unknown(
            ID,
            "no client call sites extracted — the extractor is broken, not the fleet clean",
        )];
    }
    let mut out = Vec::new();
    for c in callers {
        if gateway_owned(&c.path) {
            continue;
        }
        let mut verdict = if c.interpolated {
            match_prefix(mounted, &c.method, &c.path)
        } else {
            match_route_full(mounted, &c.method, &c.path)
        };
        // The verb was DEFAULTED, not observed: `const url = API + '/api/x';
        // ... fetch(url, {method:'POST'})` puts the literal outside the URL's
        // own statement. Asserting the default would file a 405 against a call
        // that never makes it — which is what `GET /api/dictate` was, while the
        // real call is a POST five lines down. Path existence is still checked;
        // only the method claim is withheld.
        if !c.method_known {
            if let RouteMatch::MethodNotAllowed(_) = verdict {
                verdict = RouteMatch::Ok;
            }
        }
        // Documented-absent family with a guarded caller: Missing is the
        // EXPECTED state and passes with the license named; anything else
        // (the family got mounted, or a verb mismatch) means the exclusion
        // is STALE and must fail so the entry gets deleted.
        if caller_guarded_absent(&c.path) {
            match verdict {
                RouteMatch::Missing => {
                    out.push(InvariantResult::pass(ID).entity(format!("{} {}", c.method, c.path)));
                }
                _ => out.push(
                    InvariantResult::fail(
                        ID,
                        format!("{} stays in CALLER_GUARDED_ABSENT only while unrouted", c.path),
                        format!(
                            "{} now has a mounted route — the CALLER_GUARDED_ABSENT entry is                              STALE; delete it so the census guards this family again",
                            c.path
                        ),
                    )
                    .entity(format!("{} {}", c.method, c.path)),
                ),
            }
            continue;
        }
        match verdict {
            RouteMatch::Missing => out.push(
                InvariantResult::fail(
                    ID,
                    format!("{} {} is mounted", c.method, c.path),
                    "no route matches this path".to_string(),
                )
                .entity(format!("{} {}", c.method, c.path))
                .evidence(json!({
                    "caller": c.source, "method": c.method, "path": c.path,
                    "class": "route-missing",
                    "why_it_matters": "the client calls this; a 404/405 here is a silent \
                                       capability loss unless the client fails loudly",
                })),
            ),
            RouteMatch::MethodNotAllowed(allowed) => out.push(
                InvariantResult::fail(
                    ID,
                    format!("{} allowed on {}", c.method, c.path),
                    format!("route exists but allows only {allowed:?} — {} would 405", c.method),
                )
                .entity(format!("{} {}", c.method, c.path))
                .evidence(json!({
                    "caller": c.source, "method": c.method, "path": c.path,
                    "allowed": allowed, "class": "verb-missing",
                    "incident": "amux send -> /api/workers/<n>/send 405 -> raw tmux fallback",
                })),
            ),
            RouteMatch::Ok => out.push(InvariantResult::pass(ID).entity(format!("{} {}", c.method, c.path))),
        }
    }
    out
}

/// A path a shipped client actually calls.
#[derive(Debug, Clone)]
pub struct CallerPath {
    pub method: String,
    pub path: String,
    /// Where it was found — "spa:app.js" / "cli:amux". Carried so a failure
    /// names the file to fix rather than just the path.
    pub source: String,
    /// The literal was followed by concatenation/interpolation, so `path` is a
    /// PREFIX, not the whole request path (`'/api/board/' + id`).
    ///
    /// This distinction is the difference between a usable check and an ignored
    /// one. Treating a prefix as an exact path produced 86 false failures on
    /// the first live run — every `/api/board/<id>` DELETE reported as "DELETE
    /// not allowed on /api/board" — which is precisely the cry-wolf outcome the
    /// module docs warn about. A prefix is satisfied when SOME mounted route
    /// lives under it with the right method.
    pub interpolated: bool,
    /// False when no method literal was found in the call's own statement, so
    /// `method` is the GET DEFAULT rather than something observed. A guessed
    /// verb produces a phantom 405 exactly the way a guessed path produces a
    /// phantom 404 — see the extractor's own note about not guessing paths.
    pub method_known: bool,
}

#[derive(Debug, PartialEq)]
enum RouteMatch {
    Ok,
    MethodNotAllowed(Vec<String>),
    Missing,
}

/// Segment-wise pattern match, with axum's semantics: `{name}` matches exactly
/// one segment, `{*rest}` matches the remainder.
///
/// Segment-wise and NOT substring, deliberately. A prefix matcher would report
/// `/api/workers/x/send` as covered by `/api/workers` — a false pass that would
/// let this entire check exist and still miss the incident it was built for.
/// `a_prefix_does_not_count_as_a_match` pins that.
fn segments_match(pat: &[&str], want: &[&str]) -> bool {
    let mut i = 0;
    while i < pat.len() {
        let p = pat[i];
        if p.starts_with("{*") {
            // wildcard tail: must have at least one segment left to consume
            return want.len() > i;
        }
        if i >= want.len() {
            return false;
        }
        if p.starts_with('{') {
            i += 1;
            continue; // one-segment param
        }
        if p != want[i] {
            return false;
        }
        i += 1;
    }
    pat.len() == want.len()
}

/// Resolve a concrete (method, path) against the mounted table.
///
/// Distinguishes Missing from MethodNotAllowed because the two have different
/// fixes — mount the route vs add the verb — and the incident that motivated
/// this check was the SECOND kind, which a boolean "is it routed" would have
/// called healthy.
/// Resolve an INTERPOLATED caller prefix: `'/api/board/' + id` can only be
/// checked as "does some route live under /api/board with this method".
///
/// Weaker than the exact match on purpose, and the weakness is the point: an
/// exact check on a prefix is not a stricter check, it is a WRONG one, and its
/// failures are noise that gets the whole monitor ignored. Exact literals still
/// go through `match_route_full`, so the /api/workers/<n>/send class — a fully
/// literal path in the CLI — keeps its precision.
fn match_prefix(mounted: &[(&str, &[&str])], method: &str, prefix: &str) -> RouteMatch {
    let want: Vec<&str> = prefix.trim_matches('/').split('/').collect();
    let mut method_seen: Option<Vec<String>> = None;
    for (pat, methods) in mounted {
        let pv: Vec<&str> = pat.trim_matches('/').split('/').collect();
        // The mounted route must be AT or BELOW the prefix: every literal
        // segment of the prefix has to line up with the pattern.
        if pv.len() < want.len() {
            continue;
        }
        let aligned = want.iter().enumerate().all(|(i, w)| {
            let p = pv[i];
            p.starts_with('{') || p == *w
        });
        if !aligned {
            continue;
        }
        if methods.contains(&"*") || methods.iter().any(|m| m.eq_ignore_ascii_case(method)) {
            return RouteMatch::Ok;
        }
        method_seen = Some(methods.iter().map(|s| s.to_string()).collect());
    }
    match method_seen {
        Some(a) => RouteMatch::MethodNotAllowed(a),
        None => RouteMatch::Missing,
    }
}

fn match_route_full(mounted: &[(&str, &[&str])], method: &str, path: &str) -> RouteMatch {
    let want: Vec<&str> = path.trim_matches('/').split('/').collect();
    let mut allowed_seen: Option<Vec<String>> = None;
    for (pat, methods) in mounted {
        let pv: Vec<&str> = pat.trim_matches('/').split('/').collect();
        if !segments_match(&pv, &want) {
            continue;
        }
        if methods.contains(&"*") || methods.iter().any(|m| m.eq_ignore_ascii_case(method)) {
            return RouteMatch::Ok;
        }
        allowed_seen = Some(methods.iter().map(|s| s.to_string()).collect());
    }
    match allowed_seen {
        Some(a) => RouteMatch::MethodNotAllowed(a),
        None => RouteMatch::Missing,
    }
}

// ---------------------------------------------------------------------------
// 2. Config provenance: a configured value must reach the process.
// ---------------------------------------------------------------------------

/// INCIDENT: `~/.amux/server.env` held flags that never reached
/// `std::env::var`, so every consumer read the default and the configuration
/// was silently dead ("server.env actually setdefaults into the process env —
/// flags read via std::env::var were silently dead").
///
/// INVARIANT: for every key in server.env, the process env agrees. Spec §14:
/// "this would have caught values existing in server.env but not reaching
/// std::env::var".
///
/// Values are never emitted — only key names and an agree/differ verdict — so
/// this is safe to expose on a health endpoint. server.env is the one place
/// credential VALUES live.
/// NO TWO LANES MAY SHARE A CLAUDE CONVERSATION (AMUX-1730 / AMUX-2819).
///
/// Two sessions pointed at one `cc_conversation_id` both RESUME it, so a message
/// steered to one surfaces in the other, and work done by one is attributed to
/// the other. It is not theoretical: on 2026-08-10 a fleet scan found two such
/// pairs among 101 lanes —
///     f035d084…  mixpeek-general + mixpeek-frustrations   (BOTH RUNNING)
///     a2f88163…  ts-gke + ts-troubleshooting
/// and the only reason anyone noticed is that a pane title rendered the wrong
/// worker's name. Nothing else reported it.
///
/// The WRITE path is already guarded — `conversation_owned_by_other` gates the
/// single writer of `cc_conversation_id` and both adoption sites — so this
/// check is not redundant with it: the guard prevents NEW cross-links and is
/// blind to the ones already on disk, which is the whole reason these two
/// survived. A guard that cannot see existing damage needs a detector beside
/// it, not a stronger version of itself.
///
/// Pure over (session, conversation) pairs so the real specimen is the test
/// corpus rather than a fixture.
pub fn conversations_are_not_shared(pairs: &[(String, String)]) -> Vec<InvariantResult> {
    const ID: &str = "conversation.one_lane_each";
    let mut by: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (session, conv) in pairs {
        if conv.trim().is_empty() {
            continue; // a lane with no conversation yet cannot collide
        }
        by.entry(conv.as_str()).or_default().push(session.as_str());
    }
    let mut out = Vec::new();
    for (conv, mut lanes) in by {
        lanes.sort();
        let short: String = conv.chars().take(8).collect();
        if lanes.len() == 1 {
            out.push(InvariantResult::pass(ID).entity(&short));
        } else {
            out.push(
                InvariantResult::fail(
                    ID,
                    format!("conversation {short} is held by exactly 1 lane"),
                    format!("held by {}: {}", lanes.len(), lanes.join(", ")),
                )
                .entity(&short),
            );
        }
    }
    out
}

/// A card's reviewer must not be the lane that owns it (AMUX-2563).
///
/// The card asked for the SOPHISTICATED version of this — stamp assignments
/// with a conversation id and refuse a review authored from the same transcript
/// — on the theory that two differently-named lanes could share one
/// conversation. Measured 2026-08-11: 99 conversation ids in use, ZERO shared,
/// so that hazard has no live instances (`conversations_are_not_shared` above
/// is what keeps it that way).
///
/// The COARSE version does: 4 live cards carry `reviewer == session`. A lane
/// listed as its own reviewer is self-review by name alone, and no conversation
/// id is needed to see it. Building the fine-grained guard while this went
/// unchecked would have been a guard on the case that does not happen, next to
/// an open door on the case that does.
///
/// Reports rather than refuses: existing assignments belong to whoever made
/// them (ethos rule 8), and a check that surfaces four cards is what lets a
/// human decide, where a retroactive sweep would decide for them.
pub fn reviewer_is_independent(cards: &[(String, String, String)]) -> Vec<InvariantResult> {
    const ID: &str = "board.reviewer_is_independent";
    let mut out = Vec::new();
    for (id, session, reviewer) in cards {
        let (s, r) = (session.trim(), reviewer.trim());
        if r.is_empty() {
            continue; // no reviewer assigned — nothing to be independent of
        }
        if s.is_empty() {
            continue; // unowned card; independence is undefined, not violated
        }
        if s.eq_ignore_ascii_case(r) {
            out.push(
                InvariantResult::fail(
                    ID,
                    format!("{id}: reviewer differs from the owning lane"),
                    format!("both are {s} — the lane would be reviewing its own work"),
                )
                .entity(id),
            );
        } else {
            out.push(InvariantResult::pass(ID).entity(id));
        }
    }
    out
}

pub fn config_env_reaches_process(env_file: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Vec<InvariantResult> {
    const ID: &str = "config.env_reaches_process";
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for line in env_file.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim();
        if k.is_empty() || !seen.insert(k.to_string()) {
            continue;
        }
        // Quotes are stripped because a value read straight out of the file
        // with its quotes attached is its own documented incident in this repo
        // (an `[ -d ]` test reported an existing directory as missing).
        let want = v.trim().trim_matches('"').trim_matches('\'');
        match lookup(k) {
            Some(got) if got == want => out.push(InvariantResult::pass(ID).entity(k)),
            Some(_) => out.push(
                InvariantResult::fail(ID, format!("{k} = (server.env value)"), format!("{k} = (different process value)"))
                    .entity(k)
                    .evidence(json!({
                        "key": k, "class": "config-drift",
                        "note": "values intentionally omitted — server.env holds credentials",
                    })),
            ),
            None => out.push(
                InvariantResult::fail(ID, format!("{k} present in process env"), format!("{k} unset in process env"))
                    .entity(k)
                    .evidence(json!({
                        "key": k, "class": "config-not-reaching-process",
                        "incident": "server.env flags read via std::env::var were silently dead",
                    })),
            ),
        }
    }
    if out.is_empty() {
        return vec![InvariantResult::unknown(ID, "server.env unreadable or empty")];
    }
    out
}

// ---------------------------------------------------------------------------
// 3. Queue liveness: a producer must have a consumer.
// ---------------------------------------------------------------------------

/// INCIDENT (twice, same week): the steering queue had three producers and NO
/// consumer — messages were stored durably and never delivered, so a lane sat
/// IDLE with 9 QUEUED, the oldest 2h6m old. Separately, auto-pickup died with
/// the Python retirement and 6 idle lanes sat on 17 dispatchable cards.
///
/// INVARIANT: a queued item must be progressing. If the oldest undelivered item
/// is older than `stale_after_s` while its target is IDLE, the consumer is not
/// running — which is a different, louder fact than "the queue is deep".
///
/// The IDLE qualifier is load-bearing: a deep queue behind a busy worker is
/// correct behaviour, and flagging it would train everyone to ignore this.
/// `dead_letter_after_s` is `steer_dead_letter_s()` — the SAME deadline the
/// reaper uses, passed in so the check stays pure. AMUX-3473: this check used
/// to fail unroutable rows after `stale_after_s` (300s) while the dead-letter
/// deliberately waits an hour, so for 55 minutes per row the invariant flagged
/// a fate the system had already scheduled — the view disagreeing with the
/// predicate of the mechanism it describes, flapping across 18 entities and
/// refiling within hours of every retirement. And `not-running` rows are
/// KEPT by design (the 2026-08-19 panic: age cannot distinguish a 6.5h outage
/// from a dead lane, and every queued row delivered on restart), so failing
/// them forever was a permanent red that trains skimming.
pub fn queue_has_live_consumer(
    items: &[QueuedItem],
    now: f64,
    stale_after_s: f64,
    dead_letter_after_s: f64,
) -> Vec<InvariantResult> {
    const ID: &str = "queue.has_live_consumer";
    let mut out = Vec::new();
    for it in items {
        let age = now - it.queued_at;
        if age <= stale_after_s {
            // Recently queued: a normal delivery tick has not elapsed yet.
            out.push(InvariantResult::pass(ID).entity(&it.target));
            continue;
        }
        match it.block_reason.as_deref() {
            // A registered-but-stopped lane KEEPS its queue by design (the
            // 08-19 panic lesson above); the sender was told "queued" at send
            // time. Not a failure — a failing invariant on a deliberate state
            // is the AF-132 shape.
            Some("not-running") => {
                out.push(InvariantResult::pass(ID).entity(&it.target));
            }
            // no-env-file / archived: the dead-letter reaper OWNS this row's
            // fate. Inside its deadline the wait is sanctioned; PAST it, the
            // reaper failed to reap — a real wedge, and the louder fact.
            Some(reason) => {
                if age <= dead_letter_after_s {
                    out.push(InvariantResult::pass(ID).entity(&it.target));
                } else {
                    out.push(
                        InvariantResult::fail(
                            ID,
                            format!(
                                "an unroutable row is dead-lettered within {dead_letter_after_s:.0}s"
                            ),
                            format!(
                                "undelivered for {age:.0}s, {:.0}s PAST the dead-letter deadline; \
                                 target is UNROUTABLE ({reason}) and the reaper did not reap it",
                                age - dead_letter_after_s
                            ),
                        )
                        .entity(&it.target)
                        .evidence(json!({
                            "target": it.target, "age_s": age, "queue": it.queue,
                            "class": "dead-letter-wedged",
                            "block_reason": reason,
                            "dead_letter_after_s": dead_letter_after_s,
                            "fix": "the reaper (steer_dead_letter_verdict path) should have \
                                    moved this row to steering_history; find out why it did not",
                        })),
                    );
                }
            }
            // A live consumer sitting IDLE with an old item in front of it is
            // the original producer-without-consumer incident: it is not draining.
            None if it.target_idle => {
                out.push(
                    InvariantResult::fail(
                        ID,
                        format!("queued item delivered within {stale_after_s:.0}s of the target going idle"),
                        format!("undelivered for {age:.0}s while target is IDLE"),
                    )
                    .entity(&it.target)
                    .evidence(json!({
                        "target": it.target, "age_s": age, "queue": it.queue,
                        "class": "producer-without-consumer",
                        "incident": "steering queue had 3 producers and no consumer; auto-pickup \
                                     died with the python retirement",
                    })),
                );
            }
            // A deep queue behind a BUSY worker (routable, not idle) is correct.
            None => out.push(InvariantResult::pass(ID).entity(&it.target)),
        }
    }
    if out.is_empty() {
        out.push(InvariantResult::pass(ID));
    }
    out
}

#[derive(Debug, Clone)]
pub struct QueuedItem {
    pub queue: String,
    pub target: String,
    pub queued_at: f64,
    pub target_idle: bool,
    /// Why the target is not a deliverable consumer right now, taken from the
    /// SHARED delivery predicate `lane_block_reason` (`no-env-file` /
    /// `not-running` / `archived`), or `None` when the target is a live lane.
    /// Without it the check could not tell an unroutable ghost from an
    /// idle-but-lagging consumer (AMUX-3084 / AMUX-3111).
    pub block_reason: Option<String>,
}

// ---------------------------------------------------------------------------
// 4. Status truth: the card must agree with the pane.
// ---------------------------------------------------------------------------

/// INCIDENT (AMUX-2646): `amux-rust` showed `idle` on its card while its pane
/// read `esc to interrupt`. Its self-report was a fabricated
/// `{"state":"idle","source":"stop-hook-test"}` written by a hand-run hook
/// test onto a live lane, and the derivation's asymmetric freshness rule says
/// an `idle` report never decays — so nothing in the system could disagree
/// with it. A human spotted it by looking at a terminal.
///
/// INVARIANT: a lane whose pane is unambiguously mid-turn is not reported
/// `idle`. Two sources of truth — the derived card status and the physical
/// pane — and this is the seam between them, which is precisely where no
/// component health check ever looks: the report store was healthy, the
/// derivation was healthy, the pane was healthy, and they disagreed.
///
/// This is the check that would have caught it in seconds. It is cheap enough
/// to run on the monitor tick because the caller only probes lanes that
/// painted recently — a lane that has not painted cannot be mid-turn.
pub fn status_agrees_with_pane(lanes: &[LaneTruth]) -> Vec<InvariantResult> {
    const ID: &str = "status.agrees_with_pane";
    let mut out = Vec::new();
    for l in lanes {
        // Only ONE direction is a contradiction. A card reading `active` over
        // a quiet pane is not: a lane can be legitimately mid-turn with
        // nothing painting (a long tool call, a subagent), and flagging it
        // would fire constantly and train everyone to ignore this.
        // GRACE (AMUX-3474): only a disagreement that has AGED is a
        // contradiction. A fresh idle report under a working pane is the
        // routine turn-boundary race — Stop landed, the next steered prompt
        // began, its prompt-hook report is in flight — and this class filed
        // ~100 per-entity cards over weeks, flapping healed-by-read-time
        // every time. 120s keeps the incident this check exists for
        // (AMUX-2646's fabricated report was HOURS old) and the dropped-report
        // case (a lost prompt-hook report ages past the grace within two
        // minutes of real work, still fires, still files — and a dropped
        // report IS worth a card). The dominant drop producer, reports fired
        // into a 10s restart window, died with AMUX-3458's exec adoption;
        // this grace covers the residue.
        if l.pane_says_working && l.status == "idle" && l.report_age_s > 120.0 {
            out.push(
                InvariantResult::fail(
                    ID,
                    "a lane whose pane is mid-turn is not reported idle",
                    format!(
                        "card={} while the pane shows work (report={} age={:.0}s source={})",
                        l.status, l.report_state, l.report_age_s, l.report_source
                    ),
                )
                .entity(&l.name)
                .evidence(json!({
                    "session": l.name,
                    "card_status": l.status,
                    "pane_says_working": true,
                    "report_state": l.report_state,
                    "report_age_s": l.report_age_s,
                    "report_source": l.report_source,
                    "report_origin": l.report_origin,
                    "class": "report-outranks-physical-evidence",
                    "incident": "AMUX-2646: a hand-run hook test wrote idle onto a live \
                                 working lane; an idle report never decays, so nothing \
                                 could contradict it",
                })),
            );
        } else {
            out.push(InvariantResult::pass(ID).entity(&l.name));
        }
    }
    if out.is_empty() {
        // No lane painted inside the probe window. That is a real answer on a
        // quiet fleet, not a broken probe: the caller only enumerates lanes it
        // could actually read.
        out.push(InvariantResult::pass(ID));
    }
    out
}

/// The SHARPER contradiction `status_agrees_with_pane` deliberately declines to
/// flag. That check won't call `active` over a quiet pane a fault, because a
/// lane can be legitimately mid-turn with nothing painting (a long tool call, a
/// subagent). But when the harness ITSELF freshly reported `idle` — the main
/// turn stopped — and the pane is not generating, a derived `active` is not
/// "mid-turn with nothing painting": it is amux OVERRIDING the authoritative
/// self-report, the exact inversion of the D1 rule that a fresh report wins.
///
/// INCIDENT (AMUX-3047, 2026-08-13, Ethan "says working but it appears done"):
/// the subagent contradiction in `derive_status` flipped idle->active off a 240s
/// subagent-mtime window with NO report-age gate, so a lane whose stop-hook had
/// posted `idle` ~30s earlier read WORKING for up to four minutes. The root fix
/// gates that flip on report age like the pane contradictions already were; THIS
/// is the log-signal that makes the next instance of the class — any path that
/// derives `active` while a fresh idle self-report AND a non-generating pane both
/// say otherwise — self-announce in /api/health/invariants, instead of waiting
/// for a human to notice a stale badge (the two-fixes rule).
pub fn status_contradicts_fresh_idle_report(lanes: &[LaneTruth]) -> Vec<InvariantResult> {
    const ID: &str = "status.contradicts_fresh_idle_report";
    // Same window as the derivation's `contradiction_window` (D4: policy in
    // config, not baked in). A report younger than this is the authority; a
    // derived `active` over it, with a quiet pane, means the report was
    // overridden.
    let window = std::env::var("AMUX_IDLE_CONTRADICTION_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60.0);
    let mut out = Vec::new();
    for l in lanes {
        let fresh_idle = l.report_state == "idle" && l.report_age_s < window;
        if l.status == "active" && fresh_idle && !l.pane_says_working {
            out.push(
                InvariantResult::fail(
                    ID,
                    "a lane with a fresh idle self-report and a quiet pane is derived active",
                    format!(
                        "status=active while the harness reported idle {:.0}s ago \
                         (< {:.0}s window, source={}) and the pane is not generating",
                        l.report_age_s, window, l.report_source
                    ),
                )
                .entity(&l.name)
                .evidence(json!({
                    "session": l.name,
                    "derived_status": l.status,
                    "report_state": l.report_state,
                    "report_age_s": l.report_age_s,
                    "report_source": l.report_source,
                    "report_origin": l.report_origin,
                    "pane_says_working": l.pane_says_working,
                    "window_s": window,
                    "class": "derived-status-overrides-fresh-self-report",
                    "incident": "AMUX-3047: the subagent contradiction flipped \
                                 idle->active off a 240s mtime window with no \
                                 report-age gate, so a stopped lane read WORKING \
                                 for up to four minutes",
                })),
            );
        } else {
            out.push(InvariantResult::pass(ID).entity(&l.name));
        }
    }
    if out.is_empty() {
        // Same reasoning as status_agrees_with_pane: no lane painted inside the
        // probe window is a real answer on a quiet fleet, not a broken probe.
        out.push(InvariantResult::pass(ID));
    }
    out
}

/// One lane's two sources of truth, side by side.
#[derive(Debug, Clone)]
pub struct LaneTruth {
    pub name: String,
    /// What the card says (the derived status).
    pub status: String,
    /// What the pane says — computed with the SAME detectors the derivation
    /// uses, so the check and the mechanism cannot disagree about what
    /// "working" means.
    pub pane_says_working: bool,
    pub report_state: String,
    pub report_age_s: f64,
    pub report_source: String,
    pub report_origin: String,
}

// ---------------------------------------------------------------------------
// 5. The report control plane is UP: self-reports are landing at all.
// ---------------------------------------------------------------------------

/// INCIDENT (2026-08-13): the owner reported worker status "inaccurate/delayed"
/// fleet-wide. Root cause: `endpoint.json.legacy_port` went `null` when the 8822
/// bind was dropped, so the Stop/PostToolUse/UserPromptSubmit report hooks baked
/// into ~48 pre-cutover lanes stopped rewriting their stale inherited `AMUX_URL`
/// and POSTed every state report to the dead port — silently (`>/dev/null 2>&1;
/// exit 0`). Measured: 0 of 48 running lanes had a fresh self-report; status
/// fell back entirely to terminal scraping (the D1 path the report endpoint
/// exists to demote). NOTHING in amux surfaced it — the human noticed the
/// symptom, which is exactly the failure the two-fixes rule forbids.
///
/// INVARIANT: on a fleet of any real size, SOMEONE is always at a turn boundary,
/// so the FRESHEST self-report across all running lanes is minutes old, not
/// hours. The discriminator is the FLEET MINIMUM, not any per-lane age: an idle
/// lane legitimately reports once on Stop and then goes quiet for hours (the
/// derivation's asymmetric-freshness rule), so a single stale lane proves
/// nothing — but the youngest report across the WHOLE fleet being hours old
/// means the report control plane is down for everyone at once.
///
/// Gated on `>= min_lanes` running lanes so a one- or two-lane box, where a
/// genuine quiet spell is plausible, reads `Unknown` rather than crying wolf.
pub fn self_reports_landing(
    lanes: &[LaneReport],
    min_lanes: usize,
    max_freshest_s: f64,
) -> Vec<InvariantResult> {
    const ID: &str = "session.self_reports_landing";
    if lanes.len() < min_lanes {
        return vec![InvariantResult::unknown(
            ID,
            format!(
                "only {} running lane(s) (< {min_lanes}) — too few to distinguish a dead \
                 report hook from a genuinely quiet fleet",
                lanes.len()
            ),
        )];
    }
    // Youngest report across the whole fleet, and who it belongs to. A lane with
    // NO report at all contributes nothing to the minimum (it cannot lower it),
    // which is correct: one never-reporting lane is not the fleet-wide outage
    // this catches — a dark fleet minimum is.
    let mut freshest = f64::INFINITY;
    let mut freshest_name = String::new();
    let mut with_report = 0usize;
    for l in lanes {
        if let Some(age) = l.report_age_s {
            with_report += 1;
            if age < freshest {
                freshest = age;
                freshest_name = l.name.clone();
            }
        }
    }
    if with_report == 0 {
        // Not one running lane has EVER reported: the control plane is fully
        // down, not merely quiet.
        return vec![InvariantResult::fail(
            ID,
            format!("at least one of {} running lanes reporting", lanes.len()),
            format!(
                "0 of {} running lanes have any self-report — report hooks are not landing",
                lanes.len()
            ),
        )
        .evidence(json!({
            "running_lanes": lanes.len(),
            "lanes_with_report": 0,
            "class": "report-control-plane-down",
            "incident": "2026-08-13: endpoint.json.legacy_port went null; baked-in report \
                         hooks POSTed to the dead 8822 and failed silently",
            "likely_cause": "endpoint.json legacy_port/retired_ports not naming the port \
                             pre-cutover sessions carry — status is running blind on pane-scrape",
        }))];
    }
    if freshest > max_freshest_s {
        return vec![InvariantResult::fail(
            ID,
            format!("freshest self-report across the fleet < {max_freshest_s:.0}s"),
            format!(
                "youngest report across {} running lanes is {freshest:.0}s old (from {freshest_name}; \
                 {with_report} lanes carry any report) — report control plane down fleet-wide, \
                 status is on pane-scrape",
                lanes.len()
            ),
        )
        .evidence(json!({
            "running_lanes": lanes.len(),
            "lanes_with_report": with_report,
            "freshest_report_age_s": freshest,
            "freshest_lane": freshest_name,
            "threshold_s": max_freshest_s,
            "class": "report-control-plane-down",
            "incident": "2026-08-13: baked-in report hooks POSTed to the dead 8822 silently; \
                         0/48 fresh self-reports, worker status inaccurate/delayed",
        }))];
    }
    vec![InvariantResult::pass(ID).evidence(json!({
        "running_lanes": lanes.len(),
        "lanes_with_report": with_report,
        "freshest_report_age_s": freshest,
        "freshest_lane": freshest_name,
    }))]
}

/// One running lane's self-report age, `None` when the lane has never reported.
#[derive(Debug, Clone)]
pub struct LaneReport {
    pub name: String,
    pub report_age_s: Option<f64>,
}

// ---------------------------------------------------------------------------
// 6. Shared-checkout git guard: does the running hook match its committed source?
// ---------------------------------------------------------------------------

/// INCIDENT (AMUX-3033): `~/.amux/hooks/git-shared-guard.py` — the PreToolUse
/// Bash hook that gates git in shared checkouts on EVERY tool call, fleet-wide —
/// was a 32KB runtime file with no source in the repo. It could not be reviewed,
/// diffed, or rolled back; a bad edit changed git gating for every lane with no
/// version trail; and "can't reproduce on the current file" could not tell an
/// already-fixed guard from one that changed under us (the exact ambiguity that
/// cost AMUX-3003 an hour, and that AF-27 lived through — three hypotheses died
/// before a fired watch found the root).
///
/// INVARIANT: the guard actually running is byte-identical to the source
/// committed at `scripts/git-hooks/git-shared-guard.py`, which install.sh
/// installs from. A drift means someone hand-edited the runtime copy — the
/// unreviewable, un-rollback-able state this card exists to make impossible —
/// and now it self-announces in /api/health/invariants instead of hiding until
/// the next incident that "can't reproduce".
///
/// `committed_src` is embedded in the binary at build time (`include_str!`), so
/// the server always carries the canonical version and CI rebuilds catch a
/// tampered committed copy too. Pure: it shas both sides, so a test drives it
/// with plain strings. Unreadable (e.g. a single-tenant container that never
/// installed the hook) is Unknown, not Fail — no environment branch, the check
/// simply reports it could not reach a verdict there.
///
/// GENERALISED for AMUX-2936 (2026-08-15). The report hook needed the identical
/// check, and "mirror it exactly, it is a near-copy" is precisely how a repo
/// acquires two implementations of one rule that must then be kept in step
/// forever (ethos D6). The rule does not differ between the two scripts — what
/// RUNS must equal what is COMMITTED — so only the nouns are parameters, and a
/// third installed script costs one const rather than one more copy.
pub struct InstalledScript {
    /// Kept STABLE across this refactor: consumers match on `invariant_id`, so
    /// renaming one would silently orphan whatever is keyed to it.
    pub id: &'static str,
    /// What the reader must go look at, e.g. `~/.amux/hook-report.sh`.
    pub runtime_path: &'static str,
    /// Repo-relative source `install.sh` copies from.
    pub committed_path: &'static str,
    /// Noun for the prose ("guard", "report hook").
    pub noun: &'static str,
}

/// AMUX-3033: the PreToolUse Bash hook that gates git in shared checkouts.
pub const GIT_SHARED_GUARD: InstalledScript = InstalledScript {
    id: "hooks.shared_guard_matches_committed",
    runtime_path: "~/.amux/hooks/git-shared-guard.py",
    committed_path: "scripts/git-hooks/git-shared-guard.py",
    noun: "guard",
};

/// AMUX-2936: the Stop / UserPromptSubmit / PostToolUse hook that reports each
/// lane's state, model and token count — the D1 control plane, and the sole
/// input to auto-compact (D5). It spent months as an UNVERSIONED runtime file
/// carrying a warning about its own forking, which is where that warning died.
pub const REPORT_HOOK: InstalledScript = InstalledScript {
    id: "hooks.report_hook_matches_committed",
    runtime_path: "~/.amux/hook-report.sh",
    committed_path: "scripts/hooks/hook-report.sh",
    noun: "report hook",
};

/// AF-132: the committed side must be read at CHECK time, not baked at build
/// time. These scripts are not compiled into the binary's deploy unit — the
/// builder rebuilds only on crates//Cargo.* commits — so a script-only commit
/// (4f06e22) left the baked sha stale and this check fired on the HEALTHY
/// state, calling "runtime == HEAD, tree clean" an unreviewed hand-edit and
/// prescribing a remedy (reinstall from source) that produces the
/// byte-identical file already running. A loud wrong probe with an unwalkable
/// remedy is the AMUX-2140 shape: the sanctioned instruction and the failure
/// are the same action.
///
/// `head_src` is `git show HEAD:<path>` at check time (None when no repo is
/// reachable — the cloud image); `worktree_src` is the tracked source file as
/// it sits on disk. The verdict table, in order:
/// - runtime == HEAD                          -> PASS (the healthy state).
/// - runtime == worktree != HEAD              -> fail: an UNCOMMITTED edit is
///   installed — real, actionable, and a different claim from a hand-edit.
/// - runtime != both                          -> the original hand-edit alarm,
///   now true when it fires.
/// - no git (head_src None): fall back to the build-time baked source, and a
///   mismatch HEDGES — it names both possible causes and this binary's own
///   commit (AMUX_BUILD_COMMIT), because from a baked sha alone a hand-edit
///   and a binary predating a legitimate script commit are indistinguishable.
pub fn installed_script_matches_committed(
    spec: &InstalledScript,
    baked_src: &str,
    head_src: Option<&str>,
    worktree_src: Option<&str>,
    runtime: Result<String, String>,
) -> Vec<InvariantResult> {
    let id = spec.id;
    let content = match runtime {
        Err(e) => {
            return vec![InvariantResult::unknown(
                id,
                format!("runtime {} {} unreadable: {e}", spec.noun, spec.runtime_path),
            )]
        }
        Ok(c) => c,
    };
    let runtime_sha = sha256_hex(content.as_bytes());
    if let Some(head) = head_src {
        let head_sha = sha256_hex(head.as_bytes());
        if runtime_sha == head_sha {
            return vec![InvariantResult::pass(id)];
        }
        let wt_matches =
            worktree_src.map(|w| sha256_hex(w.as_bytes()) == runtime_sha).unwrap_or(false);
        let observed = if wt_matches {
            format!(
                "runtime {} matches an UNCOMMITTED edit of {} (runtime == worktree, sha {}, \
                 HEAD has {}) — commit the tracked source; the installed copy already \
                 carries the edit.",
                spec.noun,
                spec.committed_path,
                &runtime_sha[..12],
                &head_sha[..12],
            )
        } else {
            format!(
                "runtime {} sha {} DRIFTED from {} at HEAD ({}) and matches the worktree \
                 copy of neither — the fleet is running an unreviewed hand-edit. Reinstall \
                 from source (install.sh) or fold the edit back into the committed copy.",
                spec.noun,
                &runtime_sha[..12],
                spec.committed_path,
                &head_sha[..12],
            )
        };
        return vec![InvariantResult::fail(
            id,
            format!("runtime {} == committed sha {}", spec.noun, &head_sha[..12]),
            observed,
        )
        .evidence(json!({
            "committed_sha": head_sha,
            "runtime_sha": runtime_sha,
            "runtime_matches_worktree": wt_matches,
            "committed_source": "HEAD (read at check time)",
            "runtime_path": spec.runtime_path,
            "committed_path": spec.committed_path,
        }))];
    }
    // No repo reachable: baked fallback, hedged on mismatch.
    let baked_sha = sha256_hex(baked_src.as_bytes());
    if runtime_sha == baked_sha {
        return vec![InvariantResult::pass(id)];
    }
    vec![InvariantResult::fail(
        id,
        format!("runtime {} == baked sha {}", spec.noun, &baked_sha[..12]),
        format!(
            "runtime {} sha {} differs from the source baked into this binary (built at \
             commit {}) and no repo is reachable to read HEAD — EITHER a hand-edit of the \
             runtime copy OR this binary predates a legitimate commit of {} (script-only \
             commits do not trigger a rebuild). Confirm against /health's commit before \
             acting; reinstalling only helps in the first case.",
            spec.noun,
            &runtime_sha[..12],
            env!("AMUX_BUILD_COMMIT"),
            spec.committed_path,
        ),
    )
    .evidence(json!({
        "committed_sha": baked_sha,
        "runtime_sha": runtime_sha,
        "committed_source": "baked at build time (no repo reachable)",
        "build_commit": env!("AMUX_BUILD_COMMIT"),
        "runtime_path": spec.runtime_path,
        "committed_path": spec.committed_path,
    }))]
}

// ---------------------------------------------------------------------------
// 6b2. Are auto-filed cards DISPATCHABLE? (AF-137)
// ---------------------------------------------------------------------------

/// AF-137: 215 auto-filed cards sat in todo with session=NULL while
/// auto-pickup's predicate is `i.session=?1` — every card the autofix files
/// was structurally invisible to the mechanism that hands cards to lanes,
/// and BOTH halves reported success (the filer filed, the pickup found
/// nothing to do). AMUX-2872 said "this card is the only place it shows up"
/// and then sat unseen for 11 days while the nightly failed 13 of 13 runs.
/// Rule 1 in its exact shape: who receives this, by default? Nobody — and
/// rule 4's: the gap left no trace anywhere anyone looks. This check IS that
/// trace. The remedy it names is real: AMUX_AUTOFIX_SESSION routes new
/// filings; the backlog needs the recovery sweep, not a 215-card discharge
/// into one lane's queue (the migration-event shape rule 1 warns about).
pub fn autofix_cards_are_dispatchable(open_unowned: i64, examples: &[String]) -> Vec<InvariantResult> {
    const ID: &str = "board.autofix_cards_are_dispatchable";
    if open_unowned <= 0 {
        return vec![InvariantResult::pass(ID)];
    }
    vec![InvariantResult::fail(
        ID,
        "every open auto-filed card has a session, so auto-pickup can reach it".to_string(),
        format!(
            "{open_unowned} open auto-filed card(s) have NO session — auto-pickup selects on              i.session=?1, so no lane will EVER be offered them; the detector that filed them              is writing reports nobody receives (e.g. {}). New filings: set              AMUX_AUTOFIX_SESSION in server.env. Backlog: run the recovery sweep (close              reports whose subject has recovered, route the live ones) — do NOT bulk-assign              the backlog into one queue.",
            examples.join(", "),
        ),
    )
    .evidence(json!({"open_unowned": open_unowned, "examples": examples}))]
}

/// Every open card's type is IN THE VOCABULARY (AMUX-3552).
///
/// An unknown type is not inert: `core_item_type` maps anything it does not
/// recognise to `Code`, the STRICTEST gate. So a card typed `bug` silently
/// demands "Implemented and merged" and "Tests / lint pass", and its owner —
/// who believes they set something meaningful — has only a false ack, `force`,
/// or rot as exits. That is ethos rule 3 arriving without anybody choosing it.
///
/// WHY THIS EXISTS AS A CHECK RATHER THAN A FIX. Both write paths already
/// validate: `POST /api/board` and `PATCH .../type` each return 400 with the
/// vocabulary and the reason. I assumed CREATE was the hole and TESTED it — it
/// refuses. The 14 live offenders (`bug` x12, `decision`, `docs`, across eight
/// lanes) were all created between 2026-07-30 and 2026-08-08, and validation
/// landed 2026-08-09 in b538866. They are PRE-VALIDATION RESIDUE, and zero have
/// been created since.
///
/// Which makes the real defect a migration one, and the reason it needs a
/// standing check rather than a one-off cleanup: validation started refusing new
/// bad writes and said nothing about the rows already holding bad values. They
/// sat for two weeks in the strictest gate with nothing pointing at them. Any
/// future addition to `KNOWN_TYPES` has exactly the same shape.
///
/// It reads `KNOWN_TYPES` rather than restating the list, so a type added there
/// cannot make this check wrong — the two-spellings problem that `KNOWN_TYPES`
/// own doc already flags against `ItemType::ALL`.
///
/// It does NOT propose a bulk retype. The 14 belong to eight other lanes and
/// reclassifying someone else's work is ethos rule 8; the message names them so
/// their owners can decide.
pub fn card_types_are_in_vocabulary(offenders: &[(String, String)]) -> Vec<InvariantResult> {
    const ID: &str = "board.card_types_are_in_vocabulary";
    if offenders.is_empty() {
        return vec![InvariantResult::pass(ID)];
    }
    let shown: Vec<String> =
        offenders.iter().take(5).map(|(id, t)| format!("{id}({t})")).collect();
    vec![InvariantResult::fail(
        ID,
        "every open card's type is one of the known types, so its gate is the one its          owner chose"
            .to_string(),
        format!(
            "{} open card(s) carry a type outside the vocabulary ({}). An unknown type              falls through to the STRICTEST (code) gate, so these demand a merge their              owners never claimed and cannot exit honestly. Known types: {}. Do NOT bulk              retype — they belong to other lanes; surface each to its owner (AMUX-3552).",
            offenders.len(),
            shown.join(", "),
            crate::db::board_store::KNOWN_TYPES.join(" | "),
        ),
    )
    .evidence(json!({
        "count": offenders.len(),
        "offenders": offenders.iter().map(|(i, t)| json!({"id": i, "type": t})).collect::<Vec<_>>(),
        "known_types": crate::db::board_store::KNOWN_TYPES,
    }))]
}

// ---------------------------------------------------------------------------
// 10. Host memory + kernel-panic tripwire (AMUX-3397)
// ---------------------------------------------------------------------------

/// INCIDENT (AMUX-3396, 2026-08-19): the host kernel panicked on memory/swap
/// exhaustion at 14:03 and the entire 45-lane fleet died at once. Nothing in
/// amux recorded pressure before, the death during, or the cause after —
/// "why did every lane vanish" was answered by a human reading
/// /Library/Logs/DiagnosticReports by hand.
///
/// The verdict here is the KERNEL's, not a tuned amux threshold: level 4
/// (critical) means jetsam is imminent. Level 2 (warn) stays a pass — this
/// box visits warn routinely under normal load, and a flapping incident
/// teaches everyone to ignore it — but the level and swap numbers ride in
/// the evidence of every evaluation, and /health carries them continuously.
pub fn host_memory_not_critical(
    pressure_level: Option<u32>,
    swap_used_mb: Option<f64>,
    swap_total_mb: Option<f64>,
) -> Vec<InvariantResult> {
    const ID: &str = "host.memory_not_critical";
    let ev = json!({
        "pressure_level": pressure_level,
        "swap_used_mb": swap_used_mb,
        "swap_total_mb": swap_total_mb,
    });
    match pressure_level {
        Some(4) => vec![InvariantResult::fail(
            ID,
            "kernel memory pressure below critical".to_string(),
            format!(
                "kern.memorystatus_vm_pressure_level = 4 (CRITICAL), swap {:.0}/{:.0}MB — \
                 the state that preceded the 08-19 fleet-killing panic (AMUX-3396). Jetsam \
                 is imminent: shed lanes or memory before the kernel does it for you.",
                swap_used_mb.unwrap_or(0.0),
                swap_total_mb.unwrap_or(0.0),
            ),
        )
        .evidence(ev)],
        Some(_) => vec![InvariantResult::pass(ID).evidence(ev)],
        None => vec![InvariantResult::unknown(
            ID,
            "pressure level unmeasurable on this platform (no kern.memorystatus_vm_pressure_level)",
        )
        .evidence(ev)],
    }
}

/// The after-the-fact half of AMUX-3397: a fresh `.panic` artifact in the
/// diagnostic-reports directory means the host died out from under the fleet,
/// and the incident should be READ off amux instead of reconstructed from
/// "every lane's uptime reset at once". One result per file, entity-keyed on
/// the filename, so each panic is exactly one incident (the store's dedupe)
/// and each HEALS when its file ages past the dwell window — stale files get
/// an explicit entity-keyed pass, which is what resolves the incident row.
pub fn no_fresh_kernel_panic(panics: &[(String, f64)], window_s: f64) -> Vec<InvariantResult> {
    const ID: &str = "host.no_fresh_kernel_panic";
    if panics.is_empty() {
        return vec![InvariantResult::pass(ID)];
    }
    panics
        .iter()
        .map(|(name, age_s)| {
            if *age_s < window_s {
                InvariantResult::fail(
                    ID,
                    "no kernel panic artifact inside the dwell window".to_string(),
                    format!(
                        "{name} is {:.1}h old — the host kernel panicked and the whole fleet \
                         died at once (the 08-19 memory-exhaustion panic was invisible to \
                         every amux instrument: AMUX-3396). Read the artifact for the memory \
                         state at death; this stays visible {:.0} days (AMUX_PANIC_FRESH_S) \
                         so it cannot scroll away unacknowledged.",
                        age_s / 3600.0,
                        window_s / 86400.0,
                    ),
                )
                .entity(name.as_str())
                .evidence(json!({"file": name, "age_h": age_s / 3600.0}))
            } else {
                InvariantResult::pass(ID).entity(name.as_str())
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 6e. Is the invariant system's OWN evaluation log bounded? (AMUX-3489)
// ---------------------------------------------------------------------------

/// INCIDENT (AMUX-3489, 2026-08-22): `_amux_invariant_result` reached 8M rows
/// (~2GB of DB) — the 7-day flat retention was working exactly as written
/// while 15 invariants x per-entity fan-out wrote ~13 green heartbeats a
/// second. Nothing watched the watcher: the table that exists to make
/// failures visible was itself growing invisibly, and it surfaced only
/// because a perf calibration tripped over a 20-minute `.backup`.
///
/// The check is a row budget, not a growth ratio, because the healthy state
/// after differential retention is small and roughly constant (~50k rows);
/// any sustained excursion past the budget means retention broke or a new
/// fan-out multiplied the write rate.
pub fn result_log_bounded(rows: i64, budget: i64, oldest_age_s: f64) -> Vec<InvariantResult> {
    const ID: &str = "store.result_log_bounded";
    if rows <= budget {
        return vec![InvariantResult::pass(ID)];
    }
    vec![InvariantResult::fail(
        ID,
        format!("evaluation log holds <= {budget} rows (differential retention: pass 1h, fail 7d)"),
        format!(
            "{rows} rows in _amux_invariant_result (oldest {:.0}s old) — either the \
             opportunistic trim in invariants/store.rs stopped running, or a new \
             per-entity fan-out multiplied the write rate past what the batch cap \
             drains. A fresh deploy of AMUX-3489 legitimately shows this while the \
             8M-row backlog drains (~3h); sustained past that, it is real.",
            oldest_age_s,
        ),
    )
    .evidence(json!({"rows": rows, "budget": budget, "oldest_age_s": oldest_age_s}))]
}

// ---------------------------------------------------------------------------
// 6c. Are session reports ATTRIBUTED? (AF-67)
// ---------------------------------------------------------------------------

/// INCIDENT (AF-67, 2026-08-16): 77% of all mutating requests in 24h carried no
/// `X-Amux-Session`, dominated by `POST /api/sessions/<n>/report` -- 7,708 of
/// them, from lanes still running the pre-AMUX-2936 inline hook. The attributed
/// share was 0.0% in EVERY one of the last 12 hours across 40 lanes.
///
/// Nothing automated could see it. `autofix` reads only `status >= 500` from the
/// request log, and a report POST is a 200; none of the 13 live invariants
/// expressed attribution. The signal was in the store the whole time and the
/// only reason it surfaced is that a human-triggered sweep happened to be named
/// `unattributed-http`. That is ethos rule 4 exactly: a tag in a store the
/// reader never opens is the same as no tag.
///
/// WHY THIS SIGNAL AND NOT "unattributed writes" GENERALLY: a rate needs a
/// threshold, and a threshold below the baseline is not a detector (the
/// spin-catcher lesson). Unattributed writes are legitimately non-zero forever
/// -- the dashboard and the iPhone PWA have no session to declare. A session
/// REPORT is different: it is emitted by `hook-report.sh`, which always sends
/// the header, so the healthy value is structurally ZERO and no parameter has to
/// be guessed. It also doubles as the AMUX-2936 uptake meter: as lanes recycle
/// onto the new hook this falls on its own, and the breach clearing IS the
/// remediation landing.
pub fn reports_are_attributed(total: i64, unattributed: i64) -> Vec<InvariantResult> {
    const ID: &str = "hooks.reports_are_attributed";
    // No reports at all is not health: it is the control plane being down, which
    // `session.self_reports_landing` owns. Unknown here rather than a false pass.
    if total <= 0 {
        return vec![InvariantResult::unknown(
            ID,
            "no session reports in the window — see session.self_reports_landing",
        )];
    }
    if unattributed == 0 {
        return vec![InvariantResult::pass(ID)
            .evidence(json!({"reports": total, "unattributed": 0}))];
    }
    let pct = 100.0 * unattributed as f64 / total as f64;
    vec![InvariantResult::fail(
        ID,
        "every session report carries X-Amux-Session (hook-report.sh always sends it)",
        format!(
            "{unattributed} of {total} reports ({pct:.1}%) are UNATTRIBUTED — those lanes are \
             running the pre-AMUX-2936 inline hook, which posts no header and no model/tokens. \
             Hook config loads at SESSION START, so they cannot be fixed in place; they clear \
             only as lanes restart. This falling to 0 is what AMUX-2936 being in effect looks like."
        ),
    )
    .evidence(json!({
        "reports": total, "unattributed": unattributed, "pct_unattributed": pct,
        "card": "AF-67", "remedy": "lane restart picks up ~/.amux/hook-report.sh",
    }))]
}

// ---------------------------------------------------------------------------
// 6b. Are the report hooks WIRED to that script at all? (AMUX-2936)
// ---------------------------------------------------------------------------

/// One report-hook entry as configured in `~/.claude/settings.json`.
pub struct ReportHookEntry {
    /// `Stop` | `UserPromptSubmit` | `PostToolUse` | ...
    pub event: String,
    pub command: String,
    /// `None` = the group carries no `matcher` key at all.
    pub matcher: Option<String>,
}

/// INCIDENT (AMUX-2936, 2026-08-15): three implementations of "report state to
/// amux" existed — `~/.amux/hook-report.sh` (the good one: state + model +
/// tokens), `~/.amux/amux-report.sh`, and inline one-liners. Global
/// `settings.json` pointed at the INLINE one-liners, which POST only
/// `{state, source}`. So **model and tokens read zero fleet-wide**, and tokens
/// is auto-compact's only input (D5 / AMUX-2829) — lanes ran to the context wall
/// with the policy never called. `hook-report.sh` itself was correct and
/// untouched the entire time.
///
/// Which is exactly why the sha check above CANNOT be the whole answer: it would
/// have passed, green, every hour of that regression, because the file it
/// compares was never the broken thing. Shipping only the near-copy would be a
/// check that cannot fail on the incident that motivated it — the purest form of
/// ethos rule 7, certified by its own incident report.
///
/// INVARIANT: every report hook configured in settings.json actually INVOKES
/// `hook-report.sh`, and (the documented second trap, AMUX-2538) a tool event's
/// entry carries a matcher that is a valid REGEX — `"*"` is not one, and an
/// entry without one is silently ignored. Both failure modes leave a
/// settings.json that reads as correct and a hook that never runs or runs
/// impoverished.
///
/// Selection is by "does this command mention the report script or the report
/// ENDPOINT", so a fork is INSIDE the denominator rather than filtered out of
/// it — a wiring check that only looks at correctly-wired entries can only pass.
pub fn report_hooks_wired(entries: Result<Vec<ReportHookEntry>, String>) -> Vec<InvariantResult> {
    const ID: &str = "hooks.report_hooks_wired";
    let entries = match entries {
        Err(e) => return vec![InvariantResult::unknown(ID, e)],
        Ok(v) => v,
    };
    if entries.is_empty() {
        // Not a Fail: a container or a fresh box legitimately has no amux hooks
        // in ~/.claude/settings.json, and ACTUAL absence of reports already
        // fails `session.self_reports_landing` on the outcome. Named here so the
        // reader is routed there instead of concluding nothing checks it.
        return vec![InvariantResult::unknown(
            ID,
            "no report hook configured in ~/.claude/settings.json (absence of \
             reports is covered by session.self_reports_landing)",
        )];
    }
    let mut broken: Vec<String> = Vec::new();
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for e in &entries {
        let wired = e.command.contains("hook-report.sh");
        // A tool event without a valid regex matcher is INERT — it parses, it
        // reads as configured, and it never fires. Lifecycle events take none.
        let tool_event = matches!(e.event.as_str(), "PreToolUse" | "PostToolUse");
        let matcher_ok = !tool_event
            || e.matcher.as_deref().is_some_and(|m| regex::Regex::new(m).is_ok());
        if !wired {
            broken.push(format!(
                "{}: does not invoke hook-report.sh (an inline reimplementation — this is the \
                 fork that zeroed model+tokens fleet-wide)",
                e.event
            ));
        }
        if !matcher_ok {
            broken.push(match e.matcher.as_deref() {
                None => format!("{}: tool event with NO matcher — the entry is inert", e.event),
                Some(m) => format!(
                    "{}: matcher {m:?} is not a valid regex — the entry is inert (use \".*\")",
                    e.event
                ),
            });
        }
        let mut row = json!({
            "event": e.event,
            "invokes_hook_report": wired,
            "matcher_ok": matcher_ok,
            "matcher": e.matcher,
        });
        if !wired || !matcher_ok {
            // Only for FAILING rows, and only a head: enough to identify the
            // fork, without dumping a user's settings file into an API response.
            let head: String = e.command.chars().take(120).collect();
            row["command_head"] = json!(head);
        }
        rows.push(row);
    }
    let evidence = json!({ "entries": rows });
    if broken.is_empty() {
        vec![InvariantResult::pass(ID).evidence(evidence)]
    } else {
        vec![InvariantResult::fail(
            ID,
            "every report hook in ~/.claude/settings.json invokes ~/.amux/hook-report.sh, \
             and tool events carry a valid regex matcher",
            broken.join("; "),
        )
        .evidence(evidence)]
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Pipeline: a delivered user prompt must reach the board (AMUX-3148).
// ---------------------------------------------------------------------------

/// One session's capture-pipeline health over the recent window.
///
/// `cardable` and `carded` are counted over the SAME predicate the mint uses —
/// `title_from_prompt(text).is_some()` — computed in the monitor, so this
/// invariant's denominator can never disagree with what the mint would have
/// carded (the ethos view/predicate rule: copy the filter from the code that
/// acts, never re-derive a plausible-looking one). `span_s` is the wall-clock
/// spread of those cardable prompts, used to exclude a legitimate rapid re-send
/// that the mint's dedup window is SUPPOSED to collapse to one card.
#[derive(Debug, Clone)]
pub struct SessionPromptStats {
    pub session: String,
    /// User prompts in-window whose text yields a real title (would be carded).
    pub cardable: i64,
    /// Of those, how many actually have a linked capture card.
    pub carded: i64,
    /// Seconds between the earliest and latest cardable prompt.
    pub span_s: i64,
}

/// INCIDENT (AMUX-3148): the DIRECT send path minted a ledger card for a human
/// prompt, but the STEERING-QUEUE deliverer did not — so a prompt to a BUSY lane
/// (most prompts to an active agent) was delivered and left no board trace. The
/// `amux` session went from 89 capture cards to zero for a week; `roadtrip` had
/// 25 user prompts and 0 cards. Nothing failed: `cmd_history` recorded the
/// prompt, the lane received it, the send returned success. Only the SEAM — a
/// delivered user prompt vs a board card — disagreed, and no component health
/// check looks there. The mint even names "the cmd_history.card_id NULL rate" as
/// its own detector in a comment, but nothing READ that rate (ethos rule 4: a
/// signal in a store the reader never opens is the same as no signal).
///
/// INVARIANT: a session that received `min_cardable`+ cardable user prompts,
/// spread over MORE than the dedup window (so each was an independent task, not
/// one thought re-sent), must have minted at least one capture card. Zero cards
/// across several spaced tasks is not legitimate dedup — it is the pipeline
/// silently dropping the board leg.
///
/// The gates are load-bearing and each excludes a real false positive:
/// - `cardable >= min_cardable`: one `[no-board]` or control prompt (title None)
///   is already excluded from `cardable`; requiring several more excludes a lane
///   that legitimately sent only uncardable text.
/// - `span_s > dedup_window_s`: a burst inside the dedup window SHOULD collapse
///   to one card, so firing on it would flag correct behaviour — the exact
///   "filter that matches everything" trap.
/// - `carded == 0`: one card proves the pipeline works for this lane; a low
///   ratio is a separate, quieter concern, not this outage.
pub fn user_prompts_produce_cards(
    stats: &[SessionPromptStats],
    min_cardable: i64,
    dedup_window_s: i64,
) -> Vec<InvariantResult> {
    const ID: &str = "pipeline.user_prompts_card";
    let mut out = Vec::new();
    for s in stats {
        if s.cardable >= min_cardable && s.span_s > dedup_window_s && s.carded == 0 {
            out.push(
                InvariantResult::fail(
                    ID,
                    "a delivered cardable user prompt mints a capture card",
                    format!(
                        "{} cardable user prompt(s) over {}s, 0 carded — board leg silently dropped",
                        s.cardable, s.span_s
                    ),
                )
                .entity(&s.session)
                .evidence(serde_json::json!({
                    "session": s.session,
                    "cardable_user_prompts": s.cardable,
                    "carded": s.carded,
                    "span_s": s.span_s,
                    "class": "capture-pipeline-dropped",
                    "incident": "steering-queue deliverer never minted; direct path did (AMUX-3148)",
                    "fix": "mint on the queued-delivery path for guard=='' && sender=='' prompts",
                })),
            );
        } else {
            out.push(InvariantResult::pass(ID).entity(&s.session));
        }
    }
    if out.is_empty() {
        out.push(InvariantResult::pass(ID));
    }
    out
}

/// How far back the capture-pipeline check looks, in seconds: bounded by the
/// CURRENT BUILD's uptime, capped at `ceiling_s`.
///
/// WHY THE BUILD EPOCH, not a fixed window (self-correction, 2026-08-15): the
/// check first used a 24h window and fired on SIX healthy lanes because their
/// only uncarded prompts were RESIDUE a prior, buggy build left before the
/// capture fix (13d66f4) deployed. A health check that stays red for hours after
/// the fix is crying wolf — the exact failure that trains people to ignore the
/// invariants dashboard. No fixed window separated the residue cleanly: it was
/// only 1-2h old. The only honest boundary is the running binary itself — count
/// only the prompts THIS build has processed, so a prior build's residue cannot
/// speak for the code running now (the same reason `/health.build` discriminates
/// a code change). A build up only 10min sees only 10min of prompts and usually
/// PASSES for lack of evidence — correct: a fresh build has proven nothing yet.
/// The ceiling bounds a long-lived build's memory so the check reflects recent,
/// not all-time, health.
///
/// Named residual: a break followed by `>ceiling` of silence on a lane goes
/// unflagged, and a build that swaps before accruing `min_cardable` prompts is
/// silent. Both are the innocent-until-current-evidence trade, chosen over the
/// cry-wolf-on-residue trade the fixed window forced.
pub fn capture_lookback_s(uptime_s: i64, ceiling_s: i64) -> i64 {
    uptime_s.clamp(0, ceiling_s.max(0))
}

// ---------------------------------------------------------------------------
// Provider launch: the server launches each provider the way its adapter says.
// ---------------------------------------------------------------------------

/// One provider's server-launch binary against its adapter's, for
/// [`launch_matches_adapter`].
#[derive(Debug, Clone)]
pub struct ProviderLaunch {
    pub provider: String,
    /// First token of the command the SERVER launch builder emits, read from
    /// `session_verbs::launch_base_binary` — the SAME function the launch arms
    /// build from, so this cannot disagree with the launcher.
    pub launch_binary: String,
    /// First token of the provider ADAPTER's `build_command`, or `None` when the
    /// provider has no registered adapter (e.g. `iterm2`).
    pub adapter_binary: Option<String>,
    /// Whether the adapter advertises hooks — carried so the failure can name
    /// the capability the divergence makes untrue.
    pub adapter_hooked: bool,
}

/// INCIDENT (AMUX-3153, RR-0043): ollama was migrated from a bare `ollama run`
/// REPL to `codex --oss --local-provider ollama` in the CLI and the provider
/// ADAPTER, but the SERVER launch path (the `session_verbs` launch match) was
/// left emitting `ollama run`. So a dashboard/API-launched ollama worker got a
/// hookless bare REPL while the adapter's `capabilities()` advertised
/// `hooks=true` — the capability report LIED for that launch path, and nothing
/// joined the two to notice. The launcher and the adapter are two independent
/// command constructions (the D6 seam), and no component health check looks
/// between them (ethos rule 4).
///
/// INVARIANT: for a provider that HAS an adapter, the binary the server launch
/// builder invokes equals the binary the adapter's `build_command` invokes. The
/// adapter is the intended source of truth (RR-0043; the D6 exit is the launcher
/// DELEGATING to `build_command`), so until that delegation lands this asserts
/// the two hand-maintained constructions have not drifted. A mismatch means the
/// launched process differs from what the adapter — and its capability report —
/// describes.
///
/// A provider with no adapter (iterm2) is not a contradiction to flag: it is a
/// gap to close by adding the adapter, so it passes rather than failing. That
/// keeps the failure list to real drift, the same reason `route.callers_have_routes`
/// excludes gateway-owned paths.
pub fn launch_matches_adapter(rows: &[ProviderLaunch]) -> Vec<InvariantResult> {
    const ID: &str = "provider.launch_matches_adapter";
    let mut out = Vec::new();
    for r in rows {
        match &r.adapter_binary {
            None => out.push(InvariantResult::pass(ID).entity(&r.provider)),
            Some(ab) if ab == &r.launch_binary => {
                out.push(InvariantResult::pass(ID).entity(&r.provider))
            }
            Some(ab) => out.push(
                InvariantResult::fail(
                    ID,
                    format!("server launches {} via `{ab}` (its adapter's binary)", r.provider),
                    format!(
                        "server launches `{}` while the adapter builds `{ab}`{} — an \
                         API-launched {} worker differs from what its adapter describes",
                        r.launch_binary,
                        if r.adapter_hooked {
                            " and advertises hooks=true (a bare REPL has none)"
                        } else {
                            ""
                        },
                        r.provider,
                    ),
                )
                .entity(&r.provider)
                .evidence(json!({
                    "provider": r.provider,
                    "launch_binary": r.launch_binary,
                    "adapter_binary": ab,
                    "adapter_hooked": r.adapter_hooked,
                    "class": "launcher-adapter-divergence",
                    "incident": "AMUX-3153/RR-0043: server launch left on bare `ollama run` \
                                 while the adapter moved to codex --oss; the capability report lied",
                    "fix": "align the launch arm with the adapter binary; durable: the launcher \
                            delegates to adapter.build_command (D6 exit)",
                })),
            ),
        }
    }
    if out.is_empty() {
        out.push(InvariantResult::pass(ID));
    }
    out
}

// ---------------------------------------------------------------------------
// Fire-alarm reachability: the owner-alert channel must have a live destination.
// ---------------------------------------------------------------------------

/// The delivery state the owner-alert sender reads to decide where a page goes.
/// The monitor fills this from the SAME config keys and the SAME
/// `push_subscriptions` table the sender uses (`api::alerts`), so this check
/// cannot disagree with the path it describes.
#[derive(Debug, Clone)]
pub struct AlertChannelState {
    pub push_enabled: bool,
    pub push_sub_count: usize,
    pub sms_enabled: bool,
    pub phone_configured: bool,
    /// Email is the destination that needs no manual setup (AMUX-3203): a
    /// connected Gmail account's own inbox reaches the owner. `email_reachable`
    /// is true when AMUX_OWNER_EMAIL is set OR any Gmail account is connected.
    pub email_enabled: bool,
    pub email_reachable: bool,
    /// `owner_alerts` rows written in the lookback window, and how many reached
    /// zero channels. Corroboration, not the verdict: reachability decides
    /// Pass/Fail, but a nonzero drop count turns "config looks unfinished" into
    /// "N real pages were already lost".
    pub recent_alerts: usize,
    pub recent_zero_delivery: usize,
}

/// INCIDENT (AMUX-3203, measured 2026-08-16): both channels were ENABLED yet the
/// owner-alert channel had no destination, 0 `push_subscriptions` and an empty
/// `AMUX_OWNER_PHONE`, so `amux alert` reached nobody. The five most recent pages
/// were a prod-down, a fleet-burn and two security holes, every one dropped
/// silently, while 171 board cards waited on a decision that never escalated.
///
/// The per-alert WARN ("reached ZERO channels", AMUX-3151) fires only WHEN an
/// alert is sent and lands in a log nobody tails, so a disconnected alarm sat
/// load-bearing for weeks. This is the proactive leg: an alarm with no wire to a
/// human is a CONTINUOUS health failure, visible in `/api/health/invariants`
/// without waiting for the next dropped escalation.
///
/// INVARIANT: at least one owner-alert channel is enabled AND has a reachable
/// destination (a registered push subscription, or a configured phone).
///
/// Connecting the destination is the owner's action (the push subscription is
/// created when he grants the PWA notification permission; the phone is his to
/// set), so this REPORTS, it never repairs (ethos rule 8). Both channels
/// deliberately OFF is the owner's own choice to silence the alarm, so that is
/// `Skipped`-with-reason rather than a failure of his config.
pub fn alert_channel_can_deliver(s: &AlertChannelState) -> Vec<InvariantResult> {
    const ID: &str = "alert.channel_can_deliver";
    let push_state = match (s.push_enabled, s.push_sub_count) {
        (false, _) => "disabled (AMUX_URGENT_PUSH=0)".to_string(),
        (true, 0) => "enabled but 0 push subscriptions".to_string(),
        (true, n) => format!("enabled, {n} subscription(s)"),
    };
    let sms_state = match (s.sms_enabled, s.phone_configured) {
        (false, _) => "disabled (AMUX_URGENT_SMS=0)".to_string(),
        (true, false) => "enabled but AMUX_OWNER_PHONE is empty".to_string(),
        (true, true) => "enabled, phone configured".to_string(),
    };
    let email_state = match (s.email_enabled, s.email_reachable) {
        (false, _) => "disabled (AMUX_URGENT_EMAIL=0)".to_string(),
        (true, false) => "enabled but no connected Gmail account with a fresh token (a stale refresh_token fails invalid_grant, amux-cloud 2026-08-16)".to_string(),
        (true, true) => "enabled, a connected Gmail account with a fresh token".to_string(),
    };
    let evidence = json!({
        "class": "fire-alarm-reachability",
        "push": push_state,
        "sms": sms_state,
        "email": email_state,
        "push_sub_count": s.push_sub_count,
        "recent_alerts_24h": s.recent_alerts,
        "recent_zero_delivery_24h": s.recent_zero_delivery,
        "incident": "AMUX-3203: 0 subs + empty phone dropped 5 serious pages \
                     (prod-down, security x2) while 171 cards waited on the owner",
        "fix": "email now reaches the owner with no setup (a connected Gmail \
                account's own inbox). Or subscribe to push from the PWA, or set \
                AMUX_OWNER_PHONE / AMUX_OWNER_EMAIL in ~/.amux/server.env",
    });

    // All channels off is the owner deliberately silencing the alarm. Report it,
    // do not fail his choice.
    if !s.push_enabled && !s.sms_enabled && !s.email_enabled {
        let mut r = InvariantResult::new(ID, Status::Skipped).entity("owner-alert").evidence(evidence);
        r.observed = "owner-alert is OFF by config (AMUX_URGENT_PUSH, _SMS and _EMAIL are all 0)".into();
        return vec![r];
    }

    let push_ok = s.push_enabled && s.push_sub_count > 0;
    let sms_ok = s.sms_enabled && s.phone_configured;
    let email_ok = s.email_enabled && s.email_reachable;
    if push_ok || sms_ok || email_ok {
        return vec![InvariantResult::pass(ID).entity("owner-alert").evidence(evidence)];
    }
    // Armed but disconnected: at least one channel is enabled and none can reach a
    // human. This is the incident state, and it is unambiguously broken, not a choice.
    vec![InvariantResult::fail(
        ID,
        "owner-alert reaches a human: >=1 enabled channel with a destination",
        format!("no reachable destination. push: {push_state}; sms: {sms_state}; email: {email_state}"),
    )
    .entity("owner-alert")
    .evidence(evidence)]
}

// ---------------------------------------------------------------------------
// Negative controls (AMUX-2624). Each proves the check DETECTS the real bug.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod negative_controls {
    use super::*;
    use crate::invariants::Status;

    /// AMUX-3203, rebuilt from the incident artifact: both channels ENABLED with
    /// no destination (0 subs, empty phone) is the disconnected fire alarm that
    /// dropped five serious pages while reading healthy. The check must FAIL on
    /// that exact state and clear the moment ANY single destination appears; both
    /// channels OFF is the owner's deliberate silence and must be Skipped, not
    /// Failed (ethos rule 8).
    #[test]
    fn detects_the_fire_alarm_with_no_destination() {
        // The incident state: every channel armed, none with a destination.
        let incident = AlertChannelState {
            push_enabled: true,
            push_sub_count: 0,
            sms_enabled: true,
            phone_configured: false,
            email_enabled: true,
            email_reachable: false,
            recent_alerts: 5,
            recent_zero_delivery: 5,
        };
        let r = alert_channel_can_deliver(&incident);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].status, Status::Fail, "disconnected alarm must fail: {:?}", r[0]);
        assert!(r[0].observed.contains("no reachable destination"), "{:?}", r[0]);

        // A push subscription alone clears it.
        let with_push = AlertChannelState { push_sub_count: 1, ..incident.clone() };
        assert_eq!(alert_channel_can_deliver(&with_push)[0].status, Status::Pass);

        // A phone alone clears it.
        let with_phone = AlertChannelState { phone_configured: true, ..incident.clone() };
        assert_eq!(alert_channel_can_deliver(&with_phone)[0].status, Status::Pass);

        // Email alone clears it — the no-setup destination (AMUX-3203). This is
        // the case that goes green on the real machine, where a Gmail account is
        // connected but push has 0 subs and no phone is set.
        let with_email = AlertChannelState { email_reachable: true, ..incident.clone() };
        assert_eq!(
            alert_channel_can_deliver(&with_email)[0].status,
            Status::Pass,
            "a connected Gmail account is a reachable destination"
        );

        // One channel enabled+reachable, the OTHERS disabled, still passes.
        let push_only = AlertChannelState {
            push_enabled: true,
            push_sub_count: 2,
            sms_enabled: false,
            phone_configured: false,
            email_enabled: false,
            email_reachable: false,
            recent_alerts: 0,
            recent_zero_delivery: 0,
        };
        assert_eq!(alert_channel_can_deliver(&push_only)[0].status, Status::Pass);

        // Reachable destinations behind DISABLED channels do not count: with ALL
        // three channels off the alarm is silenced by choice -> Skipped, never a
        // cheerful Pass.
        let all_off = AlertChannelState {
            push_enabled: false,
            push_sub_count: 5,
            sms_enabled: false,
            phone_configured: true,
            email_enabled: false,
            email_reachable: true,
            recent_alerts: 0,
            recent_zero_delivery: 0,
        };
        assert_eq!(
            alert_channel_can_deliver(&all_off)[0].status,
            Status::Skipped,
            "all channels off is the owner's choice, reported not failed"
        );
    }

    /// AMUX-3153, rebuilt from the incident artifact: ollama's adapter builds
    /// `codex` (and advertises hooks), but the server launch arm still emits
    /// `ollama` (a bare REPL). The check must FAIL naming ollama; the post-fix
    /// row (launch `codex`) and a provider with NO adapter (iterm2) must PASS. A
    /// check that could not fail on this row is theatre — it is the exact shape
    /// the incident report certified.
    #[test]
    fn detects_the_launcher_that_diverged_from_its_adapter() {
        let rows = vec![
            // the pre-fix incident: launcher on the bare REPL, adapter on codex.
            ProviderLaunch {
                provider: "ollama".into(),
                launch_binary: "ollama".into(),
                adapter_binary: Some("codex".into()),
                adapter_hooked: true,
            },
            // post-fix: launcher and adapter agree.
            ProviderLaunch {
                provider: "codex".into(),
                launch_binary: "codex".into(),
                adapter_binary: Some("codex".into()),
                adapter_hooked: true,
            },
            // no adapter (iterm2): a gap to close, not a contradiction — passes.
            ProviderLaunch {
                provider: "iterm2".into(),
                launch_binary: "claude".into(),
                adapter_binary: None,
                adapter_hooked: false,
            },
        ];
        let rs = launch_matches_adapter(&rows);
        let failed: Vec<&str> = rs
            .iter()
            .filter(|r| r.status == Status::Fail)
            .map(|r| r.entity_key.as_str())
            .collect();
        assert_eq!(
            failed,
            vec!["ollama"],
            "only the launcher that diverged from its adapter fails; agree/no-adapter pass"
        );
        // The failure must name the capability the divergence makes untrue, so
        // the reader is not sent to re-derive why a bare REPL is wrong.
        let f = rs.iter().find(|r| r.status == Status::Fail).unwrap();
        assert!(f.observed.contains("hooks=true"), "must name the lied capability: {}", f.observed);
    }

    /// AMUX-3148: the exact live signature — several spaced cardable prompts, zero
    /// cards — must FAIL, and a healthy lane, a low-volume lane, and a rapid
    /// re-send burst must all PASS. A check that fired on the burst would be
    /// flagging the dedup working as designed.
    #[test]
    fn detects_a_lane_whose_prompts_never_reach_the_board() {
        let window = 45; // the mint's dedup window
        let stats = vec![
            // amux's real shape: 22 prompts over hours, 0 cards.
            SessionPromptStats { session: "amux".into(), cardable: 12, carded: 0, span_s: 7200 },
            // healthy: cards its prompts.
            SessionPromptStats { session: "amux-homepage".into(), cardable: 3, carded: 3, span_s: 1800 },
            // one card is enough to prove the pipeline works for the lane.
            SessionPromptStats { session: "tubescience".into(), cardable: 6, carded: 2, span_s: 3600 },
            // low volume: below the floor, not judged as an outage.
            SessionPromptStats { session: "quiet".into(), cardable: 2, carded: 0, span_s: 600 },
            // a rapid re-send burst INSIDE the window: 0 cards is CORRECT (dedup).
            SessionPromptStats { session: "burst".into(), cardable: 4, carded: 0, span_s: 30 },
        ];
        let rs = user_prompts_produce_cards(&stats, 3, window);
        let failed: Vec<&str> = rs
            .iter()
            .filter(|r| r.status == Status::Fail)
            .map(|r| r.entity_key.as_str())
            .collect();
        assert_eq!(
            failed,
            vec!["amux"],
            "only the spaced-prompts-zero-cards lane fails; healthy/low-volume/burst all pass"
        );
    }

    /// The build-epoch lookback must EXCLUDE a prior build's residue — the exact
    /// false positive that fired on six healthy lanes (2026-08-15). A build up
    /// only 10min looks back only 10min, so residue at 1-2h ago (older than the
    /// build) is out of window and cannot cry wolf; a long-lived build is capped
    /// at the ceiling so its memory stays recent, not all-time.
    #[test]
    fn build_epoch_lookback_excludes_a_prior_builds_residue() {
        let ceiling = 6 * 3600;
        // Fresh build (10min up): look back only 10min. Residue 90min old is
        // OUTSIDE this, so the check never sees it — the false positive is gone.
        assert_eq!(capture_lookback_s(600, ceiling), 600);
        assert!(capture_lookback_s(600, ceiling) < 90 * 60, "90min-old residue is out of a 10min-old build's window");
        // Long-lived build: capped at the ceiling, not unbounded all-time memory.
        assert_eq!(capture_lookback_s(50_000, ceiling), ceiling);
        // Just booted: empty window (no evidence yet) — pass, never a fire.
        assert_eq!(capture_lookback_s(0, ceiling), 0);
        // A negative/garbage ceiling can never produce a negative lookback.
        assert_eq!(capture_lookback_s(600, -1), 0);
    }

    /// CORPUS IS THE LIVE FLEET on 2026-08-10, not a fixture: two shared
    /// conversations among 101 lanes, one of them held by two RUNNING lanes.
    #[test]
    fn two_lanes_on_one_conversation_is_a_failure_naming_both() {
        let pairs: Vec<(String, String)> = vec![
            ("mixpeek-general".into(), "f035d084-b362-404f-8cd3-d5ae76d17c28".into()),
            ("mixpeek-frustrations".into(), "f035d084-b362-404f-8cd3-d5ae76d17c28".into()),
            ("ts-gke".into(), "a2f88163-1111-2222-3333-444444444444".into()),
            ("ts-troubleshooting".into(), "a2f88163-1111-2222-3333-444444444444".into()),
            ("amux".into(), "1dd2cd21-c4a7-46b9-9b97-51fccbe721a2".into()),
        ];
        let rs = conversations_are_not_shared(&pairs);
        let fails: Vec<&InvariantResult> = rs.iter().filter(|r| r.status != Status::Pass).collect();
        assert_eq!(fails.len(), 2, "both shared conversations must fail: {rs:?}");
        // BOTH lane names must appear in the observed value. "conversation
        // f035d084 is shared" without them sends the reader to the meta files to
        // work out who — which is the hand-search that found this originally.
        let obs: String = fails.iter().map(|f| f.observed.clone()).collect::<Vec<_>>().join(" ");
        for lane in ["mixpeek-general", "mixpeek-frustrations", "ts-gke", "ts-troubleshooting"] {
            assert!(obs.contains(lane), "{lane} missing from the failure: {obs}");
        }
        // The healthy lane passes — a check that fails for everyone is not a check.
        assert_eq!(rs.iter().filter(|r| r.status == Status::Pass).count(), 1);
    }

    /// A lane with no conversation yet cannot collide, and must not be reported
    /// as sharing the empty string with every other new lane — which is what a
    /// naive group-by does, turning a fresh fleet into one giant failure.
    #[test]
    fn lanes_without_a_conversation_are_not_a_collision() {
        let pairs: Vec<(String, String)> = vec![
            ("a".into(), "".into()),
            ("b".into(), "".into()),
            ("c".into(), "   ".into()),
        ];
        assert!(conversations_are_not_shared(&pairs).is_empty());
    }

    fn mounted() -> Vec<(&'static str, &'static [&'static str])> {
        vec![
            ("/api/sessions/{name}/{*verb}", &["*"][..]),
            ("/api/board", &["GET", "POST"][..]),
            ("/api/workers", &["GET"][..]),
        ]
    }

    /// NEGATIVE CONTROL for the exact production incident: the CLI calls
    /// /api/workers/<n>/send, only the /api/sessions spelling is mounted.
    /// Pre-fix this is what production looked like, and the check must FAIL.
    #[test]
    fn detects_the_workers_send_405_that_shipped() {
        let callers = vec![CallerPath {
            method: "POST".into(),
            path: "/api/workers/amux/send".into(),
            source: "cli:amux".into(),
            interpolated: false, method_known: true,
        }];
        let rs = route_callers_have_routes(&mounted(), &callers);
        assert!(
            rs.iter().any(|r| r.status == Status::Fail),
            "the census MUST fail on the /api/workers/<n>/send gap — this is the \
             bug the spec names as the thing it should have caught"
        );
    }

    /// Gateway-owned paths are excluded, and ONLY those. A list that can never
    /// reach zero stops being read, but over-excluding hides real misses — so
    /// this pins both directions.
    #[test]
    fn only_gateway_owned_paths_are_excluded() {
        assert!(gateway_owned("/api/gateway/orgs"));
        assert!(gateway_owned("/api/stripe/checkout"));
        assert!(gateway_owned("/api/cloud-logout"));
        // Near-misses that this server DOES own must still be checked.
        assert!(!gateway_owned("/api/gatewayish"), "prefix must not swallow a sibling");
        assert!(!gateway_owned("/api/board"));
        assert!(!gateway_owned("/api/sql"));
        assert!(!gateway_owned("/api/cloud-logout-extra"), "only the exact logout path");
    }

    #[test]
    fn detects_a_lane_listed_as_its_own_reviewer() {
        let cards = vec![
            ("A-1".into(), "amux".into(), "amux".into()),          // violation
            ("A-2".into(), "amux".into(), "AMUX".into()),          // same, case-folded
            ("A-3".into(), "amux".into(), "creative-dna".into()),  // fine
            ("A-4".into(), "amux".into(), "".into()),              // no reviewer: skipped
            ("A-5".into(), "".into(), "amux".into()),              // unowned: skipped
        ];
        let out = reviewer_is_independent(&cards);
        let failed: Vec<&str> = out
            .iter()
            .filter(|r| r.status != crate::invariants::Status::Pass)
            .map(|r| r.entity_key.as_str())
            .collect();
        assert_eq!(failed, vec!["A-1", "A-2"], "self-review, including case-folded");
        assert_eq!(out.len(), 3, "cards with no reviewer or no owner are not judged");
    }


    /// ...and must PASS once the canonical spelling is mounted, or it is a
    /// check that always fires, which is the same as no check.
    #[test]
    fn passes_once_the_canonical_route_is_mounted() {
        let mut m = mounted();
        m.push(("/api/workers/{name}/{*verb}", &["*"][..]));
        let callers = vec![CallerPath {
            method: "POST".into(),
            path: "/api/workers/amux/send".into(),
            source: "cli:amux".into(),
            interpolated: false, method_known: true,
        }];
        let rs = route_callers_have_routes(&m, &callers);
        assert!(rs.iter().all(|r| r.status == Status::Pass), "must pass after the fix");
    }

    /// A route that exists but lacks the VERB is the 405 case specifically, and
    /// must be reported differently from "missing" — the two have different
    /// fixes (mount vs add method).
    #[test]
    fn distinguishes_verb_missing_from_route_missing() {
        assert_eq!(
            match_route_full(&mounted(), "DELETE", "/api/board"),
            RouteMatch::MethodNotAllowed(vec!["GET".into(), "POST".into()])
        );
        assert_eq!(match_route_full(&mounted(), "GET", "/api/nope"), RouteMatch::Missing);
        assert_eq!(match_route_full(&mounted(), "POST", "/api/board"), RouteMatch::Ok);
    }

    /// THE FALSE-PASS GUARD. A substring/prefix matcher would call
    /// /api/workers/x/send "covered" by /api/workers and report health — the
    /// exact way this check could exist and still miss the incident.
    #[test]
    fn a_prefix_does_not_count_as_a_match() {
        assert_eq!(
            match_route_full(&mounted(), "POST", "/api/workers/amux/send"),
            RouteMatch::Missing,
            "/api/workers must NOT satisfy /api/workers/amux/send"
        );
    }

    /// An extractor that found nothing must report Unknown, never a clean pass.
    /// The empty-grep trap: silence from a broken probe is indistinguishable
    /// from silence from a healthy system unless it is typed differently.
    #[test]
    fn no_callers_extracted_is_unknown_not_pass() {
        let rs = route_callers_have_routes(&mounted(), &[]);
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].status, Status::Unknown, "empty extraction is a broken probe");
    }

    /// NEGATIVE CONTROL: server.env key that never reached the process.
    #[test]
    fn detects_config_that_never_reached_the_process() {
        let envf = "AMUX_RS_SCHEDULER=true\nAMUX_OK=1\n";
        let rs = config_env_reaches_process(envf, &|k| match k {
            "AMUX_OK" => Some("1".into()),
            _ => None, // AMUX_RS_SCHEDULER never made it — the real incident
        });
        let failed: Vec<_> = rs.iter().filter(|r| r.status == Status::Fail).collect();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].entity_key, "AMUX_RS_SCHEDULER");
    }

    /// Quoted values must not be reported as drift — a value read with its
    /// quotes still attached is its own incident in this repo.
    #[test]
    fn quoted_values_are_not_false_drift() {
        let rs = config_env_reaches_process("K=\"v\"\n", &|_| Some("v".into()));
        assert!(rs.iter().all(|r| r.status == Status::Pass), "quotes must be stripped before comparing");
    }

    /// NEGATIVE CONTROL: the producer-without-consumer shape. An old item in
    /// front of an IDLE target is proof the consumer is not running.
    #[test]
    fn detects_a_queue_whose_consumer_is_dead() {
        let items = vec![QueuedItem {
            queue: "steering".into(),
            target: "amux-rust".into(),
            queued_at: 0.0,
            target_idle: true,
            block_reason: None,
        }];
        let rs = queue_has_live_consumer(&items, 7_560.0, 300.0, 3_600.0); // 2h6m, the real age
        assert!(rs.iter().any(|r| r.status == Status::Fail), "must detect the dead consumer");
    }

    /// AMUX-3473, the flap that refiled across 18 entities: the check must
    /// share the predicates of the mechanisms it describes. An unroutable row
    /// INSIDE the dead-letter deadline has a scheduled fate (pass); PAST the
    /// deadline the reaper failed and it fails as dead-letter-wedged; and a
    /// `not-running` row is KEPT by design (the 08-19 panic: an outage and a
    /// dead lane are indistinguishable by age, and every row delivered on
    /// restart) so it passes however old.
    #[test]
    fn the_check_shares_the_reaper_and_outage_predicates() {
        let mk = |reason: &str, queued_at: f64| QueuedItem {
            queue: "steering".into(),
            target: "ETHAN".into(),
            queued_at,
            target_idle: false,
            block_reason: Some(reason.into()),
        };
        // Inside the reaper's deadline: sanctioned wait, pass.
        let rs = queue_has_live_consumer(&[mk("no-env-file", 6_000.0)], 7_560.0, 300.0, 3_600.0);
        assert!(
            rs.iter().all(|r| r.status == Status::Pass),
            "a row the reaper will reap is scheduled fate, not a failure: {rs:?}"
        );
        // Past the deadline: the reaper is wedged — the louder fact.
        let rs = queue_has_live_consumer(&[mk("no-env-file", 0.0)], 7_560.0, 300.0, 3_600.0);
        let f = rs.iter().find(|r| r.status == Status::Fail).expect("past-deadline must fail");
        assert_eq!(f.evidence["class"].as_str(), Some("dead-letter-wedged"), "{}", f.evidence);
        assert!(f.observed.contains("PAST the dead-letter deadline"), "{}", f.observed);
        // not-running: kept by design, passes at any age.
        let rs = queue_has_live_consumer(&[mk("not-running", 0.0)], 7_560.0, 300.0, 3_600.0);
        assert!(
            rs.iter().all(|r| r.status == Status::Pass),
            "a stopped-but-registered lane keeps its queue deliberately: {rs:?}"
        );
    }

    /// AMUX-3084 / AMUX-3111: a target that is not a live consumer at all (its
    /// env file is gone after the amux-rust->amux rename) must read as
    /// UNROUTABLE, not as an idle consumer with lagging delivery. Before this the
    /// invariant branched only on target_idle and reported the ghost as
    /// producer-without-consumer, sending the reader to "wait for the consumer"
    /// when the truth was "this consumer will never exist".
    #[test]
    fn a_ghost_target_reads_as_unroutable_not_a_dead_consumer() {
        let items = vec![QueuedItem {
            queue: "steering".into(),
            target: "amux-rust".into(),
            queued_at: 0.0,
            target_idle: true, // carries a stale, never-decaying idle report (AMUX-2646)
            block_reason: Some("no-env-file".into()),
        }];
        // Post-AMUX-3473: the ghost still fails, but only PAST the reaper's
        // deadline (2h6m old vs a 1h deadline here), and the class names the
        // wedged reaper — the discriminator one level deeper than AMUX-3084's.
        let rs = queue_has_live_consumer(&items, 7_560.0, 300.0, 3_600.0);
        let f = rs
            .iter()
            .find(|r| r.status == Status::Fail)
            .expect("a row past the dead-letter deadline must still fail");
        assert_eq!(
            f.evidence["class"].as_str(),
            Some("dead-letter-wedged"),
            "a ghost target must be classed unroutable, not producer-without-consumer: {}",
            f.evidence
        );
        assert!(
            f.observed.contains("UNROUTABLE"),
            "the observed sentence must name the routability fault: {}",
            f.observed
        );
    }

    /// NEGATIVE CONTROL, rebuilt from the incident's own artifact: the exact
    /// row `amux-rust` had on 2026-08-09 — a card reading `idle` behind a
    /// 1076-second-old `stop-hook-test` report, over a pane that was mid-turn.
    #[test]
    fn detects_the_card_that_said_idle_while_the_pane_was_working() {
        let lanes = vec![LaneTruth {
            name: "amux-rust".into(),
            status: "idle".into(),
            pane_says_working: true,
            report_state: "idle".into(),
            report_age_s: 1076.0,
            report_source: "stop-hook-test".into(),
            report_origin: String::new(),
        }];
        let rs = status_agrees_with_pane(&lanes);
        assert!(
            rs.iter().any(|r| r.status == Status::Fail),
            "must detect a card that contradicts its own pane"
        );
        assert_eq!(rs[0].entity_key, "amux-rust", "the failure must name the lane");
    }

    /// AMUX-3474, the flap that filed ~100 per-entity cards: a FRESH idle
    /// report under a working pane is the turn-boundary race (Stop landed,
    /// the next steered prompt began, its report in flight) and must PASS;
    /// the same disagreement AGED past the grace is the dropped-report /
    /// fabricated-report case and must still fail (the 1076s incident cell
    /// above stays red).
    #[test]
    fn a_fresh_idle_report_under_a_working_pane_is_a_race_not_a_contradiction() {
        let lanes = vec![LaneTruth {
            name: "amux-gtm".into(),
            status: "idle".into(),
            pane_says_working: true,
            report_state: "idle".into(),
            report_age_s: 8.0,
            report_source: "stop-hook".into(),
            report_origin: "amux-gtm".into(),
        }];
        assert!(
            status_agrees_with_pane(&lanes).iter().all(|r| r.status == Status::Pass),
            "a seconds-old idle report over a working pane is the routine race — \
             failing it is the flap that buried the board"
        );
    }

    /// ...and must NOT fire in the other direction. A lane reported `active`
    /// with a quiet pane is a long tool call or a subagent, which is normal —
    /// a check that fires on normal operation is one people switch off.
    #[test]
    fn does_not_fire_on_an_active_card_over_a_quiet_pane() {
        let lanes = vec![LaneTruth {
            name: "amux".into(),
            status: "active".into(),
            pane_says_working: false,
            report_state: "active".into(),
            report_age_s: 4.0,
            report_source: "tool-hook".into(),
            report_origin: "amux".into(),
        }];
        assert!(status_agrees_with_pane(&lanes).iter().all(|r| r.status == Status::Pass));
    }

    /// The agreeing case must PASS rather than being unrepresentable — a check
    /// whose only outcome is failure cannot tell health from silence.
    #[test]
    fn passes_when_the_card_and_the_pane_agree() {
        let lanes = vec![LaneTruth {
            name: "amux".into(),
            status: "active".into(),
            pane_says_working: true,
            report_state: "active".into(),
            report_age_s: 2.0,
            report_source: "tool-hook".into(),
            report_origin: "amux".into(),
        }];
        assert!(status_agrees_with_pane(&lanes).iter().all(|r| r.status == Status::Pass));
    }

    /// AMUX-3047, rebuilt from the incident artifact: gtm-engine derived
    /// `active` while its stop-hook had posted `idle` 30s earlier (inside the
    /// 60s window) and the pane was a quiet "✻ Crunched for 1m 7s" prompt. The
    /// log-signal must catch this class — a fresh self-report overridden.
    #[test]
    fn fresh_idle_report_contradiction_fires_on_active_over_fresh_idle() {
        let lanes = vec![LaneTruth {
            name: "gtm-engine".into(),
            status: "active".into(),
            pane_says_working: false,
            report_state: "idle".into(),
            report_age_s: 30.0,
            report_source: "stop-hook".into(),
            report_origin: "gtm-engine".into(),
        }];
        let rs = status_contradicts_fresh_idle_report(&lanes);
        assert!(
            rs.iter().any(|r| r.status == Status::Fail),
            "must flag active derived over a fresh idle self-report + quiet pane"
        );
        assert_eq!(rs[0].entity_key, "gtm-engine", "the failure must name the lane");
    }

    /// Must NOT fire once the idle report ages past the window: a still-writing
    /// subagent flipping it active then is the bounded late correction, not a
    /// bug. A check that fires on legitimate behaviour gets switched off.
    #[test]
    fn fresh_idle_report_contradiction_silent_on_a_stale_report() {
        let lanes = vec![LaneTruth {
            name: "gtm-engine".into(),
            status: "active".into(),
            pane_says_working: false,
            report_state: "idle".into(),
            report_age_s: 120.0, // past the 60s window
            report_source: "stop-hook".into(),
            report_origin: "gtm-engine".into(),
        }];
        assert!(status_contradicts_fresh_idle_report(&lanes)
            .iter()
            .all(|r| r.status == Status::Pass));
    }

    /// Must NOT fire when the pane genuinely IS generating — then `active` is
    /// correct regardless of any report, and this is not an override.
    #[test]
    fn fresh_idle_report_contradiction_silent_when_pane_is_working() {
        let lanes = vec![LaneTruth {
            name: "gtm-engine".into(),
            status: "active".into(),
            pane_says_working: true,
            report_state: "idle".into(),
            report_age_s: 30.0,
            report_source: "stop-hook".into(),
            report_origin: "gtm-engine".into(),
        }];
        assert!(status_contradicts_fresh_idle_report(&lanes)
            .iter()
            .all(|r| r.status == Status::Pass));
    }

    /// ...and must NOT fire for a deep queue behind a BUSY worker, which is
    /// correct behaviour. A check that flags normal operation gets ignored, and
    /// then it is not a check.
    #[test]
    fn does_not_fire_for_a_queue_behind_a_busy_worker() {
        let items = vec![QueuedItem {
            queue: "steering".into(),
            target: "amux-rust".into(),
            queued_at: 0.0,
            target_idle: false, // mid-turn: queueing is the POINT
            block_reason: None,
        }];
        let rs = queue_has_live_consumer(&items, 7_560.0, 300.0, 3_600.0);
        assert!(
            rs.iter().all(|r| r.status == Status::Pass),
            "a deep queue behind a busy worker is correct, not a fault"
        );
    }

    // -- session.self_reports_landing (the 2026-08-13 reporting outage) --------

    /// THE INCIDENT'S OWN ARTIFACT: the 2026-08-13 fleet, freshest report from
    /// `primis` at 7379s and everything else 40h+. The check must FAIL and name
    /// the freshest lane and its age — the fleet MINIMUM is what discriminates a
    /// dead control plane from a legitimately quiet lane.
    #[test]
    fn a_fleet_whose_youngest_report_is_hours_old_fails() {
        // Ages drawn from the real outage: one 2h outlier, the rest ~40h.
        let mut lanes: Vec<LaneReport> = (0..47)
            .map(|i| LaneReport {
                name: format!("lane-{i}"),
                report_age_s: Some(143_000.0 + i as f64),
            })
            .collect();
        lanes.push(LaneReport { name: "primis".into(), report_age_s: Some(7_379.0) });
        let rs = self_reports_landing(&lanes, 10, 3600.0);
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].status, Status::Fail, "youngest 7379s > 3600s must fail: {rs:?}");
        // Names the freshest lane and age, so the reader does not re-derive it.
        assert!(rs[0].observed.contains("primis"), "must name freshest lane: {}", rs[0].observed);
        assert!(rs[0].observed.contains("7379"), "must state the age: {}", rs[0].observed);
    }

    /// A healthy fleet: someone reported seconds ago, so the minimum is fresh
    /// even though most lanes are idle-and-quiet. Must PASS — a check that fires
    /// on the normal steady state gets ignored, and then it is not a check.
    #[test]
    fn a_fleet_with_one_fresh_report_passes_even_if_most_are_stale() {
        let mut lanes: Vec<LaneReport> = (0..40)
            .map(|i| LaneReport {
                name: format!("idle-{i}"),
                report_age_s: Some(30_000.0),
            })
            .collect();
        lanes.push(LaneReport { name: "busy".into(), report_age_s: Some(4.0) });
        let rs = self_reports_landing(&lanes, 10, 3600.0);
        assert!(
            rs.iter().all(|r| r.status == Status::Pass),
            "a fresh fleet minimum is healthy even behind idle lanes: {rs:?}"
        );
    }

    /// Not one lane has ever reported: the control plane is fully down — a
    /// distinct, louder failure than a merely-stale minimum.
    #[test]
    fn a_fleet_with_zero_reports_fails_as_control_plane_down() {
        let lanes: Vec<LaneReport> = (0..20)
            .map(|i| LaneReport { name: format!("l-{i}"), report_age_s: None })
            .collect();
        let rs = self_reports_landing(&lanes, 10, 3600.0);
        assert_eq!(rs[0].status, Status::Fail);
        assert!(rs[0].observed.contains("0 of 20"), "{}", rs[0].observed);
    }

    /// A one- or two-lane box must read Unknown, never fire: a genuine quiet
    /// spell is plausible there, and a false alarm trains the reader to skim.
    #[test]
    fn a_tiny_fleet_is_unknown_not_a_false_alarm() {
        let lanes = vec![LaneReport { name: "solo".into(), report_age_s: Some(999_999.0) }];
        let rs = self_reports_landing(&lanes, 10, 3600.0);
        assert_eq!(rs[0].status, Status::Unknown, "too-small fleet must be Unknown: {rs:?}");
    }

    /// AMUX-3468 both directions: a guarded-absent family (tunnel, AF-63
    /// preflight) PASSES while unrouted — a permanent red on a documented
    /// absence trains skimming — and the exclusion SELF-EXPIRES: mounting the
    /// family turns the entry itself into the failure. A sibling near-miss
    /// stays guarded (the over-exclusion hazard the GATEWAY_OWNED comment
    /// warns about).
    #[test]
    fn a_caller_guarded_absent_family_passes_until_it_is_mounted() {
        let mounted: Vec<(&str, &[&str])> = vec![("/api/board", &["GET"])];
        let callers = vec![
            CallerPath { method: "POST".into(), path: "/api/tunnel/start".into(),
                         source: "amux-cli".into(), interpolated: false, method_known: true },
            CallerPath { method: "GET".into(), path: "/api/tunnel2/x".into(),
                         source: "amux-cli".into(), interpolated: false, method_known: true },
        ];
        let rs = route_callers_have_routes(&mounted, &callers);
        let by_ent = |e: &str| rs.iter().find(|r| r.entity_key == e).unwrap();
        assert_eq!(by_ent("POST /api/tunnel/start").status, Status::Pass,
                   "documented absence with a preflighting caller must not be a permanent red");
        assert_eq!(by_ent("GET /api/tunnel2/x").status, Status::Fail,
                   "a sibling outside the prefix stays guarded");
        // Mount the family: the exclusion is now stale and must SAY SO.
        let mounted2: Vec<(&str, &[&str])> =
            vec![("/api/board", &["GET"]), ("/api/tunnel/start", &["POST"])];
        let rs2 = route_callers_have_routes(&mounted2, &callers);
        let row = rs2.iter().find(|r| r.entity_key == "POST /api/tunnel/start").unwrap();
        assert_eq!(row.status, Status::Fail);
        assert!(row.observed.contains("STALE"), "{}", row.observed);
    }

    /// AF-137 both directions: unowned auto-filed cards must go RED naming
    /// the count and the remedy (215 accumulated silently while both halves
    /// reported success); zero unowned must pass, or the check becomes the
    /// permanent-red that trains skimming.
    #[test]
    fn unowned_autofix_cards_fail_the_dispatchability_check() {
        let ok = autofix_cards_are_dispatchable(0, &[]);
        assert_eq!(ok[0].status, Status::Pass, "{ok:?}");
        let bad = autofix_cards_are_dispatchable(215, &["AMUX-2872".into(), "AMUX-3447".into()]);
        assert_eq!(bad[0].status, Status::Fail);
        assert!(bad[0].observed.contains("215"), "{}", bad[0].observed);
        assert!(bad[0].observed.contains("AMUX_AUTOFIX_SESSION"), "names the remedy: {}", bad[0].observed);
        assert!(bad[0].observed.contains("AMUX-2872"), "names examples: {}", bad[0].observed);
        assert!(
            bad[0].observed.contains("do NOT bulk-assign"),
            "carries the migration-event caution: {}", bad[0].observed
        );
    }

    /// AMUX-3033: an identical runtime guard passes, a hand-edit is DETECTED as a
    /// Fail (the whole point — an unreviewed fleet-wide edit must not hide), and
    /// an unreadable runtime (a container that never installed it) is Unknown,
    /// not a false pass.
    #[test]
    fn shared_guard_drift_is_detected() {
        let committed = "#!/usr/bin/env python3\n# canonical guard source\n";
        let same = installed_script_matches_committed(
            &GIT_SHARED_GUARD,
            committed,
            Some(committed),
            Some(committed),
            Ok(committed.into()),
        );
        assert_eq!(same[0].status, Status::Pass, "identical must pass: {same:?}");

        // AF-132, THE false-fire cell: runtime matches HEAD while the BAKED
        // source is stale (a script-only commit landed; no rebuild happened).
        // This is the healthy state, and the old build-time comparison called
        // it "an unreviewed hand-edit" with a remedy that reproduced the same
        // bytes. Must PASS.
        let stale_baked = installed_script_matches_committed(
            &GIT_SHARED_GUARD,
            "# OLD baked source from the running binary's commit\n",
            Some(committed),
            Some(committed),
            Ok(committed.into()),
        );
        assert_eq!(
            stale_baked[0].status,
            Status::Pass,
            "runtime == HEAD is healthy whatever the binary baked: {stale_baked:?}"
        );

        // Runtime matches an UNCOMMITTED worktree edit: a real warn, but a
        // DIFFERENT claim from a hand-edit — the remedy is committing the
        // tracked source, not reinstalling.
        let uncommitted = committed.to_string() + "# staged but not committed\n";
        let wt = installed_script_matches_committed(
            &GIT_SHARED_GUARD,
            committed,
            Some(committed),
            Some(&uncommitted),
            Ok(uncommitted.clone()),
        );
        assert_eq!(wt[0].status, Status::Fail);
        assert!(wt[0].observed.contains("UNCOMMITTED"), "{}", wt[0].observed);
        assert!(!wt[0].observed.contains("hand-edit"), "{}", wt[0].observed);

        let drifted = installed_script_matches_committed(
            &GIT_SHARED_GUARD,
            committed,
            Some(committed),
            Some(committed),
            Ok(committed.to_string() + "# HAND EDIT\n"),
        );
        assert_eq!(drifted[0].status, Status::Fail, "a hand-edit must fail: {drifted:?}");
        assert!(drifted[0].observed.contains("DRIFTED"), "{}", drifted[0].observed);

        // No repo reachable (cloud): baked fallback must HEDGE — a mismatch
        // there cannot distinguish a hand-edit from a binary predating a
        // legitimate script commit, and must say so with this binary's commit.
        let hedged = installed_script_matches_committed(
            &GIT_SHARED_GUARD,
            committed,
            None,
            None,
            Ok(committed.to_string() + "# newer legit commit\n"),
        );
        assert_eq!(hedged[0].status, Status::Fail);
        assert!(hedged[0].observed.contains("predates"), "{}", hedged[0].observed);
        assert!(
            !hedged[0].observed.contains("unreviewed hand-edit"),
            "the no-repo fallback must not ASSERT a hand-edit: {}",
            hedged[0].observed
        );

        let missing = installed_script_matches_committed(
            &GIT_SHARED_GUARD,
            committed,
            Some(committed),
            Some(committed),
            Err("No such file (os error 2)".into()),
        );
        assert_eq!(missing[0].status, Status::Unknown, "unreadable is Unknown not pass: {missing:?}");

        // The generalisation must not have silently renamed the ids consumers
        // match on, and the two specs must not collide onto one id.
        assert_eq!(same[0].invariant_id, "hooks.shared_guard_matches_committed");
        let rep = installed_script_matches_committed(
            &REPORT_HOOK,
            committed,
            Some(committed),
            Some(committed),
            Ok(committed.into()),
        );
        assert_eq!(rep[0].invariant_id, "hooks.report_hook_matches_committed");
        // ...and the prose must follow the spec, not stay hardcoded to the guard.
        let rep_drift = installed_script_matches_committed(
            &REPORT_HOOK,
            committed,
            Some(committed),
            Some(committed),
            Ok(committed.to_string() + "x"),
        );
        assert!(
            rep_drift[0].observed.contains("scripts/hooks/hook-report.sh"),
            "report-hook drift must name ITS OWN source, not the guard's: {}",
            rep_drift[0].observed
        );
    }

    /// AF-67. The healthy value is structurally ZERO, so this needs no tuned
    /// threshold — which is the point of picking reports over "unattributed
    /// writes" generally (those are legitimately non-zero forever: the dashboard
    /// and the PWA have no session).
    #[test]
    fn an_unattributed_session_report_is_a_failure_and_zero_is_a_pass() {
        // The live specimen: 0 of 1,652 attributed across 12h (AF-67).
        let bad = reports_are_attributed(1652, 1652);
        assert_eq!(bad[0].status, Status::Fail, "100% unattributed must fail: {bad:?}");
        assert!(bad[0].observed.contains("100.0%"), "{}", bad[0].observed);
        assert!(bad[0].observed.contains("SESSION START"), "must name why it cannot be fixed live");

        // A partially-recycled fleet still fails, so the breach tracks uptake
        // rather than flipping only at the very end.
        assert_eq!(reports_are_attributed(100, 3)[0].status, Status::Fail);

        // Full uptake passes — this clearing IS AMUX-2936 landing.
        let good = reports_are_attributed(100, 0);
        assert_eq!(good[0].status, Status::Pass, "zero unattributed must pass: {good:?}");

        // No reports at all is the control plane being DOWN, not health.
        assert_eq!(reports_are_attributed(0, 0)[0].status, Status::Unknown);
    }

    fn ent(event: &str, command: &str, matcher: Option<&str>) -> ReportHookEntry {
        ReportHookEntry {
            event: event.into(),
            command: command.into(),
            matcher: matcher.map(String::from),
        }
    }

    /// AMUX-2936. The load-bearing case is `the_incident`: the sha check above
    /// passes throughout the real regression, so this is the leg that has to
    /// fail on it. Built from the ACTUAL settings.json shape found on 2026-08-15
    /// — an inline curl posting `{state,source}` — not from a convenient
    /// fixture, because the convenient fixture is convenient precisely by
    /// lacking the property that made the incident.
    #[test]
    fn report_hook_wiring_faults_are_detected() {
        const GOOD: &str = r#"bash "$HOME/.amux/hook-report.sh" idle stop-hook"#;
        const INLINE: &str = r#"curl -sk -m 3 -X POST -H 'Content-Type: application/json' -d "{\"state\":\"idle\",\"source\":\"stop-hook\"}" "$AMUX_URL/api/sessions/$AMUX_SESSION/report""#;

        let healthy = report_hooks_wired(Ok(vec![
            ent("Stop", GOOD, None),
            ent("UserPromptSubmit", GOOD, None),
            ent("PostToolUse", GOOD, Some(".*")),
        ]));
        assert_eq!(healthy[0].status, Status::Pass, "correct wiring must pass: {healthy:?}");

        let the_incident = report_hooks_wired(Ok(vec![
            ent("Stop", INLINE, None),
            ent("UserPromptSubmit", INLINE, None),
            ent("PostToolUse", INLINE, Some(".*")),
        ]));
        assert_eq!(
            the_incident[0].status,
            Status::Fail,
            "THE incident (settings.json pointing at inline one-liners, hook-report.sh itself \
             untouched) must fail — this is the case the sha check cannot see: {the_incident:?}"
        );
        assert_eq!(
            the_incident[0].observed.matches("does not invoke").count(),
            3,
            "all three forked entries must be named, not just the first: {}",
            the_incident[0].observed
        );

        // AMUX-2538's trap: correctly wired, still inert. `"*"` is not a regex,
        // and a tool event with no matcher is ignored outright.
        let bad_matcher =
            report_hooks_wired(Ok(vec![ent("PostToolUse", GOOD, Some("*"))]));
        assert_eq!(bad_matcher[0].status, Status::Fail, "\"*\" is not a regex: {bad_matcher:?}");
        assert!(bad_matcher[0].observed.contains("inert"), "{}", bad_matcher[0].observed);

        let no_matcher = report_hooks_wired(Ok(vec![ent("PostToolUse", GOOD, None)]));
        assert_eq!(no_matcher[0].status, Status::Fail, "tool event needs a matcher: {no_matcher:?}");

        // A LIFECYCLE event legitimately has none — the check must not invent a
        // failure there, or it fires forever on a correct config and gets muted.
        let lifecycle = report_hooks_wired(Ok(vec![ent("Stop", GOOD, None)]));
        assert_eq!(lifecycle[0].status, Status::Pass, "Stop takes no matcher: {lifecycle:?}");

        // Absence and unreadability are Unknown, never a false pass.
        assert_eq!(report_hooks_wired(Ok(vec![]))[0].status, Status::Unknown);
        assert_eq!(report_hooks_wired(Err("no such file".into()))[0].status, Status::Unknown);

        // Evidence must carry the fork's command head for a FAILING row and
        // withhold it otherwise — the head is what identifies which of the three
        // implementations is wired, and dumping every command would put a user's
        // settings file into an API response.
        assert!(the_incident[0].evidence["entries"][0]["command_head"].is_string());
        assert!(healthy[0].evidence["entries"][0]["command_head"].is_null());
    }

    /// AMUX-3397 cells, built from the real incident artifact. The specimen
    /// panic file at 2.7 days must FAIL inside the 7-day dwell and PASS (with
    /// its entity, so the incident resolves) once the window shrinks past it.
    #[test]
    fn the_0819_panic_specimen_fails_inside_the_dwell_and_heals_past_it() {
        let specimen = vec![(
            "panic-base+socd-2026-08-19-210001.panic".to_string(),
            2.7 * 86400.0,
        )];

        let fresh = no_fresh_kernel_panic(&specimen, 7.0 * 86400.0);
        assert_eq!(fresh[0].status, Status::Fail, "{:?}", fresh[0]);
        assert_eq!(fresh[0].entity_key, "panic-base+socd-2026-08-19-210001.panic");
        assert!(fresh[0].observed.contains("AMUX-3396"), "{:?}", fresh[0].observed);

        // Past the window the SAME entity gets an explicit pass — that is
        // what resolves the incident row; a bare pass would leave it open
        // forever (the store resolves on matching (invariant, entity)).
        let aged = no_fresh_kernel_panic(&specimen, 2.0 * 86400.0);
        assert_eq!(aged[0].status, Status::Pass);
        assert_eq!(aged[0].entity_key, "panic-base+socd-2026-08-19-210001.panic");

        // No artifacts at all: a bare pass so the check reads alive.
        assert_eq!(no_fresh_kernel_panic(&[], 7.0 * 86400.0)[0].status, Status::Pass);
    }

    /// The pressure check carries the kernel's verdict: only critical fails,
    /// warn stays a pass (this box visits warn under normal load), and an
    /// unmeasurable platform is unknown, NOT a pass.
    #[test]
    fn only_critical_pressure_fails_and_unmeasurable_is_unknown_not_pass() {
        let crit = host_memory_not_critical(Some(4), Some(30000.0), Some(32768.0));
        assert_eq!(crit[0].status, Status::Fail, "{:?}", crit[0]);
        assert!(crit[0].observed.contains("CRITICAL"), "{:?}", crit[0].observed);

        assert_eq!(host_memory_not_critical(Some(1), Some(0.0), Some(0.0))[0].status, Status::Pass);
        assert_eq!(host_memory_not_critical(Some(2), Some(9000.0), Some(16384.0))[0].status, Status::Pass);

        let unk = host_memory_not_critical(None, None, None);
        assert_eq!(unk[0].status, Status::Unknown, "{:?}", unk[0]);
    }

    /// AMUX-3489 cells. The incident specimen (8M rows) must FAIL with the
    /// numbers in the observed text; the post-retention steady state passes.
    #[test]
    fn result_log_within_budget_passes_and_the_incident_specimen_fails() {
        let ok = result_log_bounded(50_000, 500_000, 3000.0);
        assert_eq!(ok[0].status, Status::Pass);

        let bad = result_log_bounded(7_993_107, 500_000, 604_800.0);
        assert_eq!(bad[0].status, Status::Fail, "{:?}", bad[0]);
        assert!(bad[0].observed.contains("7993107"), "{:?}", bad[0].observed);
        assert_eq!(bad[0].evidence["budget"], 500_000);

        // Exactly-at-budget is not an excursion.
        assert_eq!(result_log_bounded(500_000, 500_000, 1.0)[0].status, Status::Pass);
    }
}
