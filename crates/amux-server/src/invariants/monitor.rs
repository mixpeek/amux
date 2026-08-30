//! The periodic driver (AMUX-2622).
//!
//! Binds the pure checks in [`super::checks`] to real system state and records
//! the results. The checks stay pure and table-driven precisely so their
//! negative controls can inject failures without a live fleet; this module is
//! the only place that touches the world.

use std::collections::BTreeSet;
use super::{checks, store, Confidence, InvariantResult, Status};
use crate::api::AppState;
use serde_json::json;

/// Tick interval. 30s is chosen against the spec's SLOs (§31: backend/DB worker
/// drift < 10s, stuck command < 30s) for the checks that are cheap; anything
/// needing faster detection belongs on a post-mutation hook, not here. A poll
/// interval is a ceiling on detection latency, never a substitute for a
/// postcondition at the mutation site.
/// pub so lib.rs registers the cadence this loop actually sleeps with
/// `runtime_jobs::registry`, instead of a second copy of the number.
pub const TICK_SECS: u64 = 30;

/// Run every registered invariant once against live state.
///
/// Returns the results rather than only persisting them so the HTTP handler can
/// serve a FRESH evaluation on demand — a health endpoint that can only replay
/// the last poll cannot answer "is it broken right now".
pub async fn evaluate_all(state: &AppState) -> Vec<InvariantResult> {
    let mut out = Vec::new();

    // -- 1. route contract: do shipped clients call routes that exist?
    let mounted: Vec<(&str, &[&str])> = crate::api::request_log::ROUTE_TABLE
        .iter()
        .map(|e| (e.path, e.methods))
        .collect();
    let callers = extract_caller_paths();
    out.extend(checks::route_callers_have_routes(&mounted, &callers));

    // -- 1b. no two lanes share a Claude conversation (AMUX-1730 / AMUX-2819).
    //
    // Reads the session meta files, which are the same store the resume path
    // reads `cc_conversation_id` from — so this cannot disagree with the thing
    // it describes.
    {
        let sessions_dir = crate::config::ServerConfig::from_process_env()
            .amux_home
            .join("sessions");
        let mut pairs: Vec<(String, String)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&sessions_dir) {
            for e in rd.flatten() {
                let p = e.path();
                let Some(fname) = p.file_name().and_then(|f| f.to_str()) else { continue };
                let Some(name) = fname.strip_suffix(".meta.json") else { continue };
                let Ok(text) = std::fs::read_to_string(&p) else { continue };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
                if let Some(c) = v.get("cc_conversation_id").and_then(|c| c.as_str()) {
                    if !c.trim().is_empty() {
                        pairs.push((name.to_string(), c.trim().to_string()));
                    }
                }
            }
        }
        out.extend(checks::conversations_are_not_shared(&pairs));
    }

    // -- 2. config provenance: did server.env reach the process?
    //
    // Reads the FILE and compares against the live process env. Deliberately
    // not against `ServerConfig.env`, which is the merged in-memory view and
    // would agree with itself by construction: the incident was that
    // `std::env::var()` call sites saw nothing while the config struct looked
    // correct, so the config struct is exactly the wrong oracle here.
    let env_path = crate::config::ServerConfig::from_process_env()
        .amux_home
        .join("server.env");
    match std::fs::read_to_string(&env_path) {
        Ok(text) => out.extend(checks::config_env_reaches_process(&text, &|k| {
            std::env::var(k).ok()
        })),
        Err(e) => out.push(InvariantResult::unknown(
            "config.env_reaches_process",
            format!("server.env unreadable: {e}"),
        )),
    }

    // -- 3. queue liveness: is anything queued in front of an IDLE target?
    out.extend(steering_queue_check(state).await);

    // -- 4. status truth: does the card agree with the pane?
    out.extend(status_pane_check(state));

    // -- 5. is the report control plane up? (2026-08-13 fleet-wide outage)
    out.extend(self_reports_check(state));

    // -- 5b. do the `ts` columns hold what their readers assume? (AF-184)
    out.extend(timestamp_units_check(state));
    out.extend(arrival_follows_boot_check(state));

    // -- 6. shared-checkout git guard: does the RUNNING hook match its committed
    // source? (AMUX-3033). AF-132: the committed side is read from HEAD at CHECK
    // time — these scripts deploy on COMMIT (install), not on binary rebuild, so
    // a sha baked at build time goes stale on every script-only commit and the
    // check then fires on the healthy state. The include_str! remains ONLY as
    // the no-repo fallback (cloud image), where the message hedges instead of
    // asserting a hand-edit.
    {
        const BAKED_GUARD: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/git-hooks/git-shared-guard.py"
        ));
        let repo = crate::api::self_update::repo_dir();
        let read_head = |rel: &str| -> Option<String> {
            let dir = repo.as_ref()?;
            let out = std::process::Command::new("git")
                .args(["-C", &dir.to_string_lossy(), "show", &format!("HEAD:{rel}")])
                .output()
                .ok()?;
            out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        };
        let read_worktree = |rel: &str| -> Option<String> {
            repo.as_ref().and_then(|d| std::fs::read_to_string(d.join(rel)).ok())
        };
        let amux_home = crate::config::ServerConfig::from_process_env().amux_home;
        let runtime = std::fs::read_to_string(amux_home.join("hooks/git-shared-guard.py"))
            .map_err(|e| e.to_string());
        let head = read_head(checks::GIT_SHARED_GUARD.committed_path);
        let wt = read_worktree(checks::GIT_SHARED_GUARD.committed_path);
        out.extend(checks::installed_script_matches_committed(
            &checks::GIT_SHARED_GUARD,
            BAKED_GUARD,
            head.as_deref(),
            wt.as_deref(),
            runtime,
        ));

        // -- 6a. the REPORT hook, same rule, same function (AMUX-2936). It is
        // the D1 control plane and auto-compact's only token source, and it
        // spent months as an unversioned runtime file whose own header warned
        // about the forking that then happened anyway — a warning nobody reads
        // before editing is not a control.
        const BAKED_REPORT_HOOK: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/hooks/hook-report.sh"
        ));
        let runtime = std::fs::read_to_string(amux_home.join("hook-report.sh"))
            .map_err(|e| e.to_string());
        let head = read_head(checks::REPORT_HOOK.committed_path);
        let wt = read_worktree(checks::REPORT_HOOK.committed_path);
        out.extend(checks::installed_script_matches_committed(
            &checks::REPORT_HOOK,
            BAKED_REPORT_HOOK,
            head.as_deref(),
            wt.as_deref(),
            runtime,
        ));
    }

    // -- 6b. and is anything WIRED to that script? The sha check above would
    // have passed green through the entire AMUX-2936 regression, because the
    // file it compares was correct the whole time and settings.json simply
    // pointed elsewhere. This is the leg that fails on the actual incident.
    out.extend(report_hooks_check());

    // -- 6c. are session reports ATTRIBUTED? (AF-67). The largest signal in the
    // request log had no automated consumer: autofix reads only status>=500 and
    // a report is a 200, so 7,708 unattributed reports/day were visible to
    // nobody until a human-named trigger happened to say "unattributed-http".
    out.extend(reports_attributed_check(state));

    // -- 6d. are auto-filed cards DISPATCHABLE? (AF-137: 215 session=NULL
    // reports invisible to auto-pickup's session-keyed predicate, both
    // halves reporting success for 11 days).
    out.extend(autofix_dispatchable_check(state));
    out.extend(card_type_vocabulary_check(state));

    // -- 6f. does the frustrations LEDGER agree with the board? (AF-191).
    // `grep '^STATUS: open' frustrations.md` is that file's own documented
    // primary grep and it reported 78 while 52 of those entries had a card
    // already done or verified. The ledger and the cards were two stores of one
    // fact with nothing between them.
    out.extend(frustration_ledger_check(state));
    out.extend(schedule_kind_check(state));

    // -- 6e. is the invariant system's OWN evaluation log bounded? (AMUX-3489:
    // 8M rows / ~2GB from a flat 7-day retention on ~13 green rows/sec — the
    // watcher was the one thing no watcher covered).
    out.extend(result_log_bounded_check(state));

    // -- 7. capture pipeline: does a DELIVERED user prompt reach the board?
    // (AMUX-3148). The mint's own comment names "the cmd_history.card_id NULL
    // rate" as its detector but nothing read it; this closes that loop.
    out.extend(capture_pipeline_check(state));

    // -- 8. provider launch agrees with its adapter (RR-0043 / AMUX-3153): does
    // the server launch each provider with the same binary its adapter — and its
    // capability report — describes? The launcher and the adapter are two
    // independent command constructions (the D6 seam), and nothing looked
    // between them until ollama shipped a bare `ollama run` under an adapter
    // advertising hooks=true. This joins them so the next divergence
    // self-announces instead of a lying capability report.
    out.extend(provider_launch_check());

    // -- 10. host memory + kernel-panic tripwire (AMUX-3397): the 08-19
    // memory-exhaustion panic killed all 45 lanes and left no trace in any
    // amux instrument. The pressure check publishes the kernel's own verdict;
    // the panic check makes the artifact self-announce for a week instead of
    // waiting for a human to read /Library/Logs/DiagnosticReports.
    out.extend(host_memory_check());
    out.extend(kernel_panic_check());

    // -- 9. fire-alarm reachability: can the owner-alert channel reach a human?
    // (AMUX-3203). Both channels read healthy-enabled while neither had a
    // destination (0 push subs, empty phone), so five serious pages (a prod-down
    // and two security holes) dropped silently while 171 cards waited on the
    // owner. The per-send WARN only fires on a dropped page; this makes a
    // disconnected alarm a CONTINUOUS health failure instead.
    out.extend(alert_channel_check(state));

    out
}

/// AMUX-3203. Reads the SAME config keys and `push_subscriptions` table
/// `api::alerts` reads when it sends, so the check cannot disagree with the
/// sender. The verdict is reachability (config + subscription count); the recent
/// drop tally is corroborating evidence, computed from the `owner_alerts` ledger
/// whose `ts` is in SECONDS (unlike cmd_history's ms — the ethos rule 7 landmine).
fn alert_channel_check(state: &AppState) -> Vec<InvariantResult> {
    const ID: &str = "alert.channel_can_deliver";
    let home = crate::config::amux_home();
    let ev = |k: &str| crate::api::settings::effective_env(&home, k);
    let push_enabled = ev("AMUX_URGENT_PUSH").unwrap_or_else(|| "1".into()) != "0";
    let sms_enabled = ev("AMUX_URGENT_SMS").unwrap_or_else(|| "1".into()) != "0";
    let phone_configured = !ev("AMUX_OWNER_PHONE").unwrap_or_default().trim().is_empty();
    let email_enabled = ev("AMUX_URGENT_EMAIL").unwrap_or_else(|| "1".into()) != "0";
    // Reachable only if a connected Gmail account has a plausibly-LIVE token. "An
    // account exists" is NOT enough: the alarm reached nobody during a cloud outage
    // because the selected account's refresh_token was dead (invalid_grant) while
    // it still had a token FILE (amux-cloud, 2026-08-16). Proxy for liveness: the
    // freshest token file was rewritten within EMAIL_TOKEN_STALE_S. An active
    // account refreshes its token ~hourly; the incident's dead account was a month
    // stale. It is a proxy, not a live refresh, but the sender now tries EVERY
    // account newest-first, so actual delivery is more robust than this check.
    const EMAIL_TOKEN_STALE_S: u64 = 14 * 24 * 3600;
    let email_reachable = crate::integrations::email::newest_token_age_secs_in(
        &home,
        std::time::SystemTime::now(),
    )
    .map(|age| age < EMAIL_TOKEN_STALE_S)
    .unwrap_or(false);

    let Ok(conn) = state.store.read() else {
        return vec![InvariantResult::unknown(ID, "store unreadable")];
    };
    let push_sub_count: usize = conn
        .query_row("SELECT COUNT(*) FROM push_subscriptions", [], |r| r.get::<_, i64>(0))
        .map(|n| n.max(0) as usize)
        .unwrap_or(0);
    // owner_alerts written in the last 24h, and how many reached zero channels. A
    // deduped/muted row is a suppression, not a delivery attempt (channels "{}"),
    // so exclude it; a delivered row carries a success token in its channels JSON
    // ("imessage"/"twilio"/"sent").
    let (recent_alerts, recent_zero_delivery) = {
        let mut total = 0usize;
        let mut dropped = 0usize;
        if let Ok(mut stmt) = conn.prepare(
            "SELECT channels, deduped FROM owner_alerts WHERE ts > (strftime('%s','now') - 86400)",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            }) {
                for (channels, deduped) in rows.flatten() {
                    if deduped != 0 {
                        continue;
                    }
                    total += 1;
                    let delivered = channels.contains("imessage")
                        || channels.contains("twilio")
                        || channels.contains("\"sent\"");
                    if !delivered {
                        dropped += 1;
                    }
                }
            }
        }
        (total, dropped)
    };
    drop(conn);

    checks::alert_channel_can_deliver(&checks::AlertChannelState {
        push_enabled,
        push_sub_count,
        sms_enabled,
        phone_configured,
        email_enabled,
        email_reachable,
        recent_alerts,
        recent_zero_delivery,
    })
}

/// AMUX-3153 / RR-0043: for each provider, compare the binary the SERVER launch
/// builder invokes (`session_verbs::launch_base_binary`, the launcher's OWN
/// source) against the binary and hooks its registered ADAPTER advertises.
/// Reading the launcher's own function is the point — the check cannot disagree
/// with what the launcher runs. Pure over the static registry + launch table, so
/// its negative control drives it with plain rows and needs no live fleet.
/// AF-216: read enabled schedules and hand (title, kind) to the pure check.
///
/// `deleted` and `enabled` are filtered HERE rather than in the check, because a
/// disabled or deleted schedule costs nothing per fire — it does not fire. The
/// claim under test is about what a LIVE schedule spends.
fn schedule_kind_check(state: &AppState) -> Vec<InvariantResult> {
    let Ok(conn) = state.store.read() else {
        // Cannot read: Unknown, never Pass. A store we could not open is not a
        // store with nothing wrong in it.
        return vec![InvariantResult::new(
            "schedules.cost_title_matches_kind",
            Status::Unknown,
        )];
    };
    let rows: Vec<checks::ScheduleKindRow> = conn
        .prepare(
            "SELECT id, COALESCE(title,''), COALESCE(kind,'tmux'), COALESCE(session,''), \
                    COALESCE(command,'') \
             FROM schedules WHERE enabled=1 AND COALESCE(deleted,0)=0",
        )
        .and_then(|mut st| {
            st.query_map([], |r| {
                Ok(checks::ScheduleKindRow {
                    id: r.get::<_, String>(0)?,
                    title: r.get::<_, String>(1)?,
                    kind: r.get::<_, String>(2)?,
                    session: r.get::<_, String>(3)?,
                    command: r.get::<_, String>(4)?,
                })
            })
            .map(|it| it.flatten().collect::<Vec<_>>())
        })
        .unwrap_or_default();
    if rows.is_empty() {
        // No enabled schedules at all is not evidence that none are mislabelled.
        return vec![InvariantResult::new(
            "schedules.cost_title_matches_kind",
            Status::Unknown,
        )];
    }
    checks::schedule_cost_titles_match_kind(&rows)
}

fn provider_launch_check() -> Vec<InvariantResult> {
    use crate::provider::PromptMode;
    let reg = crate::provider::default_registry();
    let rows: Vec<checks::ProviderLaunch> = crate::api::session_verbs::SESSION_PROVIDERS
        .iter()
        .map(|p| {
            let launch_binary = crate::api::session_verbs::launch_base_binary(p).to_string();
            let (adapter_binary, adapter_hooked) = match reg.resolve(p) {
                Some(a) => (
                    a.build_command(PromptMode::Interactive).into_iter().next(),
                    a.capabilities().hooks,
                ),
                None => (None, false),
            };
            checks::ProviderLaunch {
                provider: (*p).to_string(),
                launch_binary,
                adapter_binary,
                adapter_hooked,
            }
        })
        .collect();
    checks::launch_matches_adapter(&rows)
}

/// AMUX-3148: read recent user prompts from `cmd_history`, compute per-session
/// capture stats over the SAME `title_from_prompt` predicate the mint uses, and
/// hand them to the pure check. Running the identical function is the point —
/// the invariant's "cardable" count is exactly what the mint would have carded,
/// so the two cannot drift into a view that disagrees with the mechanism.
fn capture_pipeline_check(state: &AppState) -> Vec<InvariantResult> {
    const ID: &str = "pipeline.user_prompts_card";
    let env_i = |k: &str, d: i64| -> i64 {
        std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
    };
    // WINDOW_H is now a CEILING, not the window itself: the real horizon is this
    // BUILD's uptime (capture_lookback_s), so residue a prior — buggy — build left
    // cannot fire the check for the code running now. The 24h fixed window fired
    // on six healthy lanes whose only uncarded prompts predated the capture fix
    // (2026-08-15). Default dropped 24h -> 6h so a long-lived build still reflects
    // RECENT health.
    let ceiling_h = env_i("AMUX_CAPTURE_INVARIANT_WINDOW_H", 6);
    let dedup_window_s = env_i("AMUX_CAPTURE_DEDUP_WINDOW_S", 45);
    let min_cardable = env_i("AMUX_CAPTURE_INVARIANT_MIN", 3);
    let uptime_s = state.started.elapsed().as_secs() as i64;
    let lookback_s = checks::capture_lookback_s(uptime_s, ceiling_h * 3600);
    let Ok(conn) = state.store.read() else {
        return vec![InvariantResult::unknown(ID, "store unreadable")];
    };
    let rows: Vec<(String, String, bool, i64)> = {
        let Ok(mut stmt) = conn.prepare(
            "SELECT session, text, card_id IS NOT NULL, ts FROM cmd_history \
             WHERE type = 'user' AND ts > (strftime('%s','now') - ?1) * 1000",
        ) else {
            return vec![InvariantResult::unknown(ID, "cmd_history query failed")];
        };
        stmt.query_map(rusqlite::params![lookback_s], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0, r.get(3)?))
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default()
    };
    // Group per session over the mint's predicate. A prompt whose text yields no
    // title is not something the mint would card, so it is excluded from BOTH
    // numerator and denominator — the denominator is "prompts that SHOULD card".
    struct Acc {
        cardable: i64,
        carded: i64,
        min_ts: i64,
        max_ts: i64,
    }
    let mut map: std::collections::HashMap<String, Acc> = std::collections::HashMap::new();
    for (session, text, carded, ts) in rows {
        if session.is_empty() || amux_core::board::title_from_prompt(&text).is_none() {
            continue;
        }
        let e = map.entry(session).or_insert(Acc {
            cardable: 0,
            carded: 0,
            min_ts: ts,
            max_ts: ts,
        });
        e.cardable += 1;
        if carded {
            e.carded += 1;
        }
        e.min_ts = e.min_ts.min(ts);
        e.max_ts = e.max_ts.max(ts);
    }
    let stats: Vec<checks::SessionPromptStats> = map
        .into_iter()
        .map(|(session, a)| checks::SessionPromptStats {
            session,
            cardable: a.cardable,
            carded: a.carded,
            span_s: (a.max_ts - a.min_ts) / 1000,
        })
        .collect();
    checks::user_prompts_produce_cards(&stats, min_cardable, dedup_window_s)
}

/// The derived card status against the physical pane, per lane (AMUX-2646).
///
/// Reads BOTH sides through `FleetSignals` — the same struct, the same
/// capture, the same detectors the derivation itself uses. Re-deriving either
/// side here would produce a check that can disagree with the mechanism it
/// audits, which is the failure this whole module exists to catch.
///
/// Cost is bounded by `capture_panes`, which probes only lanes that painted
/// inside the contradiction window: 4 of 63 on the fleet this was measured on.
/// Gather what [`checks::timestamp_units_are_what_readers_assume`] needs (AF-184).
///
/// Two halves, and the second is the one that keeps working as the schema grows:
/// the MAX of every DECLARED timestamp column, and the names of any
/// timestamp-shaped column the schema has that the declaration does not.
///
/// The undeclared half is why this reads the live schema instead of a fixed
/// list. Five tables here name a column `ts` and use two different units; a
/// sixth added tomorrow would inherit the trap silently, and the only thing that
/// makes an author state the unit is a check that goes red until they do.
fn timestamp_units_check(state: &AppState) -> Vec<InvariantResult> {
    const ID: &str = "schema.timestamp_units_declared";
    let Ok(conn) = state.store.read() else {
        return vec![InvariantResult::unknown(ID, "store unreadable")];
    };
    // Every column whose name looks like a wall-clock stamp, across every table.
    let mut found: Vec<(String, String)> = Vec::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
    {
        if let Ok(tables) = stmt.query_map([], |r| r.get::<_, String>(0)) {
            for t in tables.flatten() {
                let Ok(mut cs) = conn.prepare(&format!("PRAGMA table_info(\"{t}\")")) else {
                    continue;
                };
                // (name, declared type)
                let cols: Vec<(String, String)> =
                    match cs.query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?))) {
                        Ok(rows) => rows.flatten().collect(),
                        Err(_) => continue,
                    };
                {
                    for (c, ty) in cols {
                        // NAME *AND* TYPE. The first draft keyed on `ts` alone,
                        // which exempted THREE of the five millisecond columns in
                        // this schema — a check written to catch the unit trap
                        // could not see most of it (amux caught this in review;
                        // it is the rule-1 exemption shape, where the narrowing
                        // does not make it cheap, it makes it invisible).
                        //
                        // Type filters out ISO-8601 text columns, which are out
                        // of scope because a string says its own unit — the exact
                        // property the numeric ones lack. DECLARED type, not a
                        // sampled value, so an empty table stays in scope.
                        let name_matches = c == "ts"
                            || c.ends_with("_ts")
                            || c.ends_with("_at")
                            || c == "time"
                            || c == "timestamp";
                        let up = ty.to_ascii_uppercase();
                        let numeric = ["INT", "REAL", "NUM", "FLOA", "DOUB"]
                            .iter()
                            .any(|k| up.contains(k));
                        if name_matches && numeric {
                            found.push((t.clone(), c));
                        }
                    }
                }
            }
        }
    }
    if found.is_empty() {
        // The schema read failed or matched nothing. Not a pass: an empty result
        // from a query that should always find `_amux_request_log.ts` means the
        // instrument is broken, not that the schema is clean.
        return vec![InvariantResult::unknown(
            ID,
            "no timestamp-shaped columns found — the schema read failed, this is not a clean bill",
        )];
    }
    let declared: std::collections::HashSet<String> = checks::TIMESTAMP_COLUMNS
        .iter()
        .map(|(t, c, _)| format!("{t}.{c}"))
        .collect();
    let mut undeclared: Vec<String> = found
        .iter()
        .map(|(t, c)| format!("{t}.{c}"))
        .filter(|n| !declared.contains(n))
        .collect();
    undeclared.sort();
    let mut observed: Vec<(String, Option<f64>)> = Vec::new();
    for (t, c, _) in checks::TIMESTAMP_COLUMNS {
        let max: Option<f64> = conn
            .query_row(&format!("SELECT MAX(\"{c}\") FROM \"{t}\""), [], |r| r.get(0))
            .ok()
            .flatten();
        observed.push((format!("{t}.{c}"), max));
    }
    let now = crate::runtime_jobs::registry::unix_now();
    checks::timestamp_units_are_what_readers_assume(&observed, &undeclared, now)
}

/// Gather what [`checks::request_arrival_follows_boot`] needs (AMUX-3647).
///
/// ONE query, both numbers, over the indexed `ts` range. Counting the rows that
/// CARRY a boot_at in the same pass is what lets the check distinguish "the
/// invariant holds" from "the column stopped being written", which are the two
/// states a bare violation count cannot tell apart.
fn arrival_follows_boot_check(state: &AppState) -> Vec<InvariantResult> {
    const ID: &str = "reqlog.arrival_follows_boot";
    const WINDOW_H: f64 = 24.0;
    let Ok(conn) = state.store.read() else {
        return vec![InvariantResult::unknown(ID, "store unreadable")];
    };
    let cutoff = crate::runtime_jobs::registry::unix_now() - WINDOW_H * 3600.0;
    match conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(ts < boot_at), 0) FROM _amux_request_log \
         WHERE ts >= ?1 AND boot_at IS NOT NULL",
        [cutoff],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    ) {
        Ok((with_boot, before)) => {
            checks::request_arrival_follows_boot(with_boot, before, WINDOW_H)
        }
        Err(e) => vec![InvariantResult::unknown(ID, format!("request log unreadable: {e}"))],
    }
}

fn status_pane_check(state: &AppState) -> Vec<InvariantResult> {
    const ID: &str = "status.agrees_with_pane";
    let Ok(conn) = state.store.read() else {
        return vec![InvariantResult::unknown(ID, "store unreadable")];
    };
    let mut signals = crate::api::sessions_legacy::FleetSignals::load(&conn);
    if signals.running.is_empty() {
        // No tmux fleet is a real state (a fresh box), but it is also exactly
        // what a failed `tmux list-sessions` looks like — and that has shipped
        // here before, serving running=0 for 116 live cards. Do not call it a
        // pass.
        return vec![InvariantResult::unknown(ID, "no running tmux sessions visible")];
    }
    signals.capture_panes();
    let lanes: Vec<checks::LaneTruth> = signals
        .probed_lanes()
        .into_iter()
        .map(|(name, pane_says_working)| {
            let rep = signals.reports.get(&name).cloned().unwrap_or(json!({}));
            checks::LaneTruth {
                status: signals.derive_status(&name, true),
                pane_says_working,
                report_state: rep["state"].as_str().unwrap_or("").into(),
                report_age_s: signals.now - rep["ts"].as_f64().unwrap_or(signals.now),
                report_source: rep["source"].as_str().unwrap_or("").into(),
                report_origin: rep["origin"].as_str().unwrap_or("").into(),
                name,
            }
        })
        .collect();
    // Both checks read the SAME `lanes` (one capture, one derivation) so they
    // cannot disagree with each other or with the mechanism they audit. The
    // first flags a working pane read idle; the second (AMUX-3047) flags the
    // inverse sharp case — `active` derived over a FRESH idle self-report and a
    // quiet pane, i.e. the harness's own report being overridden.
    let mut results = checks::status_agrees_with_pane(&lanes);
    results.extend(checks::status_contradicts_fresh_idle_report(&lanes));
    results
}

/// Is the report control plane UP — are self-reports landing at all?
///
/// Reads the SAME `FleetSignals.reports` blob the status derivation reads, so
/// this cannot disagree with the mechanism it audits about what "reported"
/// means. The discriminator is the FLEET MINIMUM report age, not any per-lane
/// age: one idle lane going quiet for hours is normal, the youngest report
/// across the whole fleet being hours old is the control plane down (the
/// 2026-08-13 outage, where baked-in report hooks POSTed to the dead 8822).
fn self_reports_check(state: &AppState) -> Vec<InvariantResult> {
    const ID: &str = "session.self_reports_landing";
    let Ok(conn) = state.store.read() else {
        return vec![InvariantResult::unknown(ID, "store unreadable")];
    };
    let signals = crate::api::sessions_legacy::FleetSignals::load(&conn);
    if signals.running.is_empty() {
        // Same reasoning as status_pane_check: no fleet is a real state but also
        // what a failed `tmux list-sessions` looks like. Not a pass.
        return vec![InvariantResult::unknown(ID, "no running tmux sessions visible")];
    }
    // NAMESPACE: `signals.running` holds the tmux session names, which are the
    // `amux-<n>` form; `signals.reports` is keyed by the BARE `AMUX_SESSION`
    // name (`<n>`) the report POST carries. `probed_lanes()` bridges the two
    // with `format!("amux-{n}")` — do the same here, or every lookup misses and
    // the check reports "0 of N reporting" against a blob that is actually full
    // (caught immediately on first deploy, 2026-08-13). `agent_running` also
    // drops shell-only panes, which are not lanes and never report.
    let lanes: Vec<checks::LaneReport> = signals
        .running
        .iter()
        .filter_map(|t| t.strip_prefix("amux-"))
        .filter(|n| signals.agent_running(&format!("amux-{n}")))
        .map(|n| {
            let age = signals
                .reports
                .get(n)
                .and_then(|r| r["ts"].as_f64())
                .map(|ts| signals.now - ts);
            checks::LaneReport { name: n.to_string(), report_age_s: age }
        })
        .collect();
    // Policy in config, not baked in (ethos D4). Defaults: a fleet of >=10 lanes
    // with NObody reporting in an hour is unambiguously broken — a healthy fleet
    // transitions within minutes, and the incident's freshest was 2h.
    let min_lanes = std::env::var("AMUX_REPORT_MIN_LANES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let max_freshest_s = std::env::var("AMUX_REPORT_FRESHEST_MAX_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600.0);
    checks::self_reports_landing(&lanes, min_lanes, max_freshest_s)
}

/// Read the report hooks OUT of `~/.claude/settings.json` (AMUX-2936).
///
/// Selection predicate: a command that mentions the report SCRIPT or the report
/// ENDPOINT. Both halves matter — matching only `hook-report.sh` would filter
/// every fork out of the denominator, leaving a check that can only ever pass,
/// and the fork this card exists for is exactly a command that hits the endpoint
/// without going through the script.
fn report_hooks_check() -> Vec<InvariantResult> {
    const ID: &str = "hooks.report_hooks_wired";
    let path = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".claude/settings.json");
    let parsed = std::fs::read_to_string(&path)
        .map_err(|e| format!("{} unreadable: {e}", path.display()))
        .and_then(|text| {
            serde_json::from_str::<serde_json::Value>(&text)
                .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))
        })
        .map(|v| extract_report_hooks(&v));
    if let Err(ref why) = parsed {
        tracing::debug!(target: "invariants", "{ID}: {why}");
    }
    checks::report_hooks_wired(parsed)
}

/// PURE so it can be driven by the incident's own settings.json shape.
///
/// Split out deliberately: an extractor that silently selects nothing makes the
/// whole check Unknown forever, which reads as "not applicable here" rather than
/// "broken" — the silent-probe failure, and the one shape a live green run
/// cannot distinguish. Tested below against both the correct wiring and the
/// forked wiring, so a selection bug fails a test instead of muting an invariant.
fn extract_report_hooks(v: &serde_json::Value) -> Vec<checks::ReportHookEntry> {
    let mut entries = Vec::new();
    for (event, groups) in v["hooks"].as_object().into_iter().flatten() {
        for g in groups.as_array().into_iter().flatten() {
            for h in g["hooks"].as_array().into_iter().flatten() {
                let command = h["command"].as_str().unwrap_or_default().to_string();
                if !(command.contains("hook-report.sh")
                    || command.contains("amux-report.sh")
                    || command.contains("/report"))
                {
                    continue;
                }
                entries.push(checks::ReportHookEntry {
                    event: event.clone(),
                    command,
                    matcher: g["matcher"].as_str().map(String::from),
                });
            }
        }
    }
    entries
}

#[cfg(test)]
mod report_hook_wiring_tests {
    use super::*;
    use crate::invariants::Status;

    /// The settings.json shapes verbatim: the one running now (correct), and the
    /// AMUX-2936 fork it replaced. Both must be SELECTED — the fork especially,
    /// since filtering it out is what would leave a check that cannot fail.
    #[test]
    fn the_extractor_selects_both_the_wired_and_the_forked_shape() {
        let wired = serde_json::json!({"hooks": {
            "Stop": [{"hooks": [{"type": "command",
                "command": "bash \"$HOME/.amux/hook-report.sh\" idle stop-hook"}]}],
            "PostToolUse": [{"matcher": ".*", "hooks": [{"type": "command",
                "command": "bash \"$HOME/.amux/hook-report.sh\" active tool-hook"}]}]
        }});
        let got = extract_report_hooks(&wired);
        assert_eq!(got.len(), 2, "both report hooks must be selected");
        assert_eq!(
            got.iter().find(|e| e.event == "PostToolUse").unwrap().matcher.as_deref(),
            Some(".*"),
            "the matcher lives on the GROUP, not the hook — reading the wrong level \
             reports every tool entry as matcher-less"
        );
        assert_eq!(checks::report_hooks_wired(Ok(got))[0].status, Status::Pass);

        let forked = serde_json::json!({"hooks": {
            "Stop": [{"hooks": [{"type": "command",
                "command": "curl -sk -X POST -d '{\"state\":\"idle\"}' \
                            \"$AMUX_URL/api/sessions/$AMUX_SESSION/report\""}]}]
        }});
        let got = extract_report_hooks(&forked);
        assert_eq!(got.len(), 1, "an inline fork must be IN the denominator, not filtered out");
        assert_eq!(
            checks::report_hooks_wired(Ok(got))[0].status,
            Status::Fail,
            "the incident shape must fail end to end, extractor included"
        );

        // A settings.json full of unrelated hooks must not be dragged in.
        let unrelated = serde_json::json!({"hooks": {
            "PostToolUse": [{"matcher": "Write|Edit", "hooks": [{"type": "command",
                "command": "bash .claude/check-and-commit.sh"}]}]
        }});
        assert!(extract_report_hooks(&unrelated).is_empty(), "unrelated hooks must not be selected");
    }

    /// Wiring, on the REAL file. Vacuous where there is no settings.json (CI),
    /// and it says so rather than passing quietly — but on a machine that HAS
    /// report hooks, the monitor must reach a real verdict about them. This is
    /// the assertion that catches the extractor going blind against the actual
    /// on-disk shape, which no synthetic fixture can.
    #[test]
    fn the_report_hook_check_reaches_a_verdict_on_the_real_settings_file() {
        let path = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(".claude/settings.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            eprintln!("no {} — vacuous here, real on a fleet machine", path.display());
            return;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return };
        if extract_report_hooks(&v).is_empty() {
            eprintln!("no report hooks configured — vacuous here");
            return;
        }
        let rs = report_hooks_check();
        assert_ne!(
            rs[0].status,
            Status::Unknown,
            "settings.json HAS report hooks but the monitor reached no verdict — the \
             extractor and the live file disagree: {rs:?}"
        );
    }
}

/// Count this window's session reports, split on whether the write was stamped.
///
/// Reads `_amux_request_log` directly because that is where the ANSWER is: the
/// `amux_session` column is the header stamp, and it is the same column the
/// attribution audit and the send-ledger read. Deriving it from anywhere else
/// would let the check disagree with the thing it describes.
/// AMUX-3397. The check itself only fails at critical, but the WARN
/// transition still lands in the server log — that is the "death spiral in
/// progress" line the card asks a log sweep to be able to catch, emitted on
/// the crossing rather than every cycle so it cannot wallpaper the log.
fn host_memory_check() -> Vec<InvariantResult> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static LAST_LEVEL: AtomicU32 = AtomicU32::new(0);
    let m = crate::api::health::mem_health();
    let lvl = m.pressure_level.unwrap_or(0);
    let prev = LAST_LEVEL.swap(lvl, Ordering::Relaxed);
    if lvl >= 2 && prev < 2 {
        tracing::warn!(
            pressure_level = lvl,
            swap_used_mb = m.swap_used_mb.unwrap_or(0.0),
            swap_total_mb = m.swap_total_mb.unwrap_or(0.0),
            "host memory pressure crossed to {} — the 08-19 panic class (AMUX-3397); \
             watch swap growth and consider shedding lanes",
            m.pressure,
        );
    }
    checks::host_memory_not_critical(m.pressure_level, m.swap_used_mb, m.swap_total_mb)
}

/// AMUX-3397. Filename + mtime only — most artifacts in this directory are
/// root-owned and unreadable to the server user, but the LISTING is enough
/// for the tripwire, and the fail text routes a human to the file.
fn kernel_panic_check() -> Vec<InvariantResult> {
    let window_s = std::env::var("AMUX_PANIC_FRESH_S")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(7.0 * 86400.0);
    let dir = std::env::var("AMUX_PANIC_DIR")
        .unwrap_or_else(|_| "/Library/Logs/DiagnosticReports".into());
    let now = std::time::SystemTime::now();
    let mut files: Vec<(String, f64)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            // Dotfiles are the OS's staging copies (`.contents.panic` is
            // rewritten in place); the named artifact is the durable one.
            if name.starts_with('.') || !name.ends_with(".panic") {
                continue;
            }
            if let Ok(mt) = e.metadata().and_then(|md| md.modified()) {
                let age_s = now.duration_since(mt).map(|d| d.as_secs_f64()).unwrap_or(0.0);
                files.push((name, age_s));
            }
        }
    }
    // Same `now` the ages above were measured against (AMUX-3645): the check
    // derives its declared heal epoch as now - age + window, and a second
    // clock read here would offset every one of them by the scan duration.
    let now_epoch = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    checks::no_fresh_kernel_panic(&files, window_s, now_epoch)
}

/// AMUX-3489. The budget is env-tunable (AMUX_INVARIANT_RESULT_BUDGET) so a
/// deliberate fan-out increase moves the number in config rather than
/// re-tuning a constant; 500k sits ~10x above the post-differential-retention
/// steady state (~50k) and ~16x below the 8M incident specimen.
fn result_log_bounded_check(state: &AppState) -> Vec<InvariantResult> {
    const ID: &str = "store.result_log_bounded";
    let budget = std::env::var("AMUX_INVARIANT_RESULT_BUDGET")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(500_000);
    match super::store::result_log_stats(&state.store) {
        Ok((rows, oldest_age_s)) => checks::result_log_bounded(rows, budget, oldest_age_s),
        Err(e) => vec![InvariantResult::unknown(ID, format!("could not count the log: {e}"))],
    }
}

fn card_type_vocabulary_check(state: &AppState) -> Vec<InvariantResult> {
    const ID: &str = "board.card_types_are_in_vocabulary";
    let Ok(conn) = state.store.read() else {
        return vec![InvariantResult::unknown(ID, "store unreadable")];
    };
    // The vocabulary comes from KNOWN_TYPES, not from a literal here: a second
    // copy of the list is the drift this check exists to catch, one layer up.
    let placeholders = crate::db::board_store::KNOWN_TYPES
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT id, type FROM issues WHERE deleted IS NULL AND COALESCE(archived,0)=0 \
         AND status NOT IN ('done','verified','discarded') \
         AND COALESCE(type,'') NOT IN ({placeholders}) ORDER BY created"
    );
    let params: Vec<&dyn rusqlite::types::ToSql> = crate::db::board_store::KNOWN_TYPES
        .iter()
        .map(|t| t as &dyn rusqlite::types::ToSql)
        .collect();
    let rows: Result<Vec<(String, String)>, _> = conn.prepare(&sql).and_then(|mut st| {
        st.query_map(params.as_slice(), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1).unwrap_or_default()))
        })
        .map(|it| it.flatten().collect())
    });
    match rows {
        Ok(offenders) => checks::card_types_are_in_vocabulary(&offenders),
        Err(e) => vec![InvariantResult::unknown(ID, format!("could not read card types: {e}"))],
    }
}

/// AF-191. Joins `frustrations.md` against the board so the ledger's own
/// primary grep cannot silently drift from what the cards say.
///
/// SOURCE ORDER, and it is the whole reason this is not a plain `include_str!`:
/// `frustrations.md` deploys on COMMIT, not on binary rebuild. The builder only
/// rebuilds when `crates/` or `Cargo.*` move, so a baked copy goes stale on
/// every ledger-only commit and the check would then fire on the healthy state —
/// the identical trap AF-132 already caught in the git-guard check above.
/// Worktree first (that is what a `grep` in this checkout sees, which is what
/// the file's header promises), then `HEAD`, then the baked copy as the no-repo
/// fallback (cloud image). Which one was used rides in the message and the
/// evidence of every evaluation, because "the report records WHICH source last
/// wrote it" is the only thing that distinguishes a stale read from a real
/// disagreement.
fn frustration_ledger_check(state: &AppState) -> Vec<InvariantResult> {
    const AGREE: &str = "frustrations.ledger_agrees_with_board";
    const REACH: &str = "frustrations.cards_are_reachable";
    const BAKED: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../frustrations.md"
    ));
    let repo = crate::api::self_update::repo_dir();
    let (md, source) = repo
        .as_ref()
        .and_then(|d| {
            std::fs::read_to_string(d.join("frustrations.md")).ok().map(|s| (s, "worktree"))
        })
        .or_else(|| {
            let dir = repo.as_ref()?;
            let out = std::process::Command::new("git")
                .args(["-C", &dir.to_string_lossy(), "show", "HEAD:frustrations.md"])
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| (String::from_utf8_lossy(&out.stdout).into_owned(), "HEAD"))
        })
        .unwrap_or_else(|| (BAKED.to_string(), "baked-at-build"));

    let entries = checks::parse_frustration_entries(&md);
    if entries.is_empty() {
        // Zero entries makes BOTH checks pass vacuously, which is the exact
        // theatre this module forbids. An empty ledger is either a drained file
        // or a broken parse and nothing here can tell them apart, so say so.
        let msg = format!("parsed 0 entries from {source} ({} bytes)", md.len());
        return vec![
            InvariantResult::unknown(AGREE, msg.clone()),
            InvariantResult::unknown(REACH, msg),
        ];
    }
    let Ok(conn) = state.store.read() else {
        return vec![
            InvariantResult::unknown(AGREE, "store unreadable"),
            InvariantResult::unknown(REACH, "store unreadable"),
        ];
    };
    // The prefixes THIS instance mints, read off the board rather than
    // hardcoded, so a new lane's prefix needs no edit here. An id whose prefix
    // is absent from every card we own belongs to another amux install.
    let local_prefixes: BTreeSet<String> = conn
        .prepare("SELECT DISTINCT substr(id, 1, instr(id,'-')-1) FROM issues WHERE instr(id,'-')>1")
        .and_then(|mut st| {
            st.query_map([], |r| r.get::<_, String>(0)).map(|it| it.flatten().collect())
        })
        .unwrap_or_default();
    let mut rows: Vec<checks::LedgerRow> = Vec::new();
    let mut cardless: Vec<(usize, String)> = Vec::new();
    for (line, title, file_status, session, cards) in entries {
        if cards.is_empty() {
            cardless.push((line, title.clone()));
        }
        for card in cards {
            // `deleted IS NULL` only: an ARCHIVED card is still readable and
            // still carries its status, and filtering it out here would report a
            // live link as broken — the archived-filter trap ethos rule 1 logs
            // five instances of.
            // `archived` comes back ALONGSIDE status, never as a filter
            // (AF-246). The comment above is still right that filtering it out
            // would report a live link as broken; the defect was that the flag
            // was not SELECTED at all, so `LedgerRow` could not express
            // "reachable" and the check compared the one axis it had. An
            // instrument that cannot state the discriminator is the bug
            // (ethos rule 4), and here it made an archived card behind a live
            // entry read as agreeing.
            let row: Option<(String, i64)> = conn
                .query_row(
                    "SELECT status, COALESCE(archived,0) FROM issues \
                     WHERE id=?1 AND deleted IS NULL",
                    [&card],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
                )
                .ok();
            let (st, archived) = match row {
                Some((s, a)) => (Some(s), a != 0),
                None => (None, false),
            };
            rows.push(checks::LedgerRow {
                line,
                card,
                file_status: file_status.clone(),
                session: session.clone(),
                title: title.clone(),
                card_status: st,
                card_archived: archived,
            });
        }
    }
    let mut out = checks::frustration_ledger_agrees_with_board(&rows, source);
    out.extend(checks::frustration_cards_are_reachable(
        &rows,
        &cardless,
        &local_prefixes,
        source,
    ));
    out
}

fn autofix_dispatchable_check(state: &AppState) -> Vec<InvariantResult> {
    const ID: &str = "board.autofix_cards_are_dispatchable";
    let Ok(conn) = state.store.read() else {
        return vec![InvariantResult::unknown(ID, "store unreadable")];
    };
    // Same open-statuses shape the board's own views use; desc marker is the
    // filer's fixed first line, so the check and the filer cannot drift apart
    // without this going red.
    let rows: Result<Vec<String>, _> = conn
        .prepare(
            "SELECT id FROM issues WHERE deleted IS NULL AND COALESCE(archived,0)=0 \
             AND status NOT IN ('done','verified','discarded') \
             AND COALESCE(session,'')='' \
             AND desc LIKE '%Filed automatically by amux%' ORDER BY created DESC",
        )
        .and_then(|mut st| {
            st.query_map([], |r| r.get::<_, String>(0)).map(|it| it.flatten().collect())
        });
    match rows {
        Ok(ids) => {
            let examples: Vec<String> = ids.iter().take(3).cloned().collect();
            checks::autofix_cards_are_dispatchable(ids.len() as i64, &examples)
        }
        Err(e) => vec![InvariantResult::unknown(ID, format!("query failed: {e}"))],
    }
}

fn reports_attributed_check(state: &AppState) -> Vec<InvariantResult> {
    const ID: &str = "hooks.reports_are_attributed";
    let Ok(conn) = state.store.read() else {
        return vec![InvariantResult::unknown(ID, "store unreadable")];
    };
    let since = crate::config::now_f64() - 3600.0;
    let row = conn.query_row(
        "SELECT COUNT(*), SUM(CASE WHEN COALESCE(amux_session,'')='' THEN 1 ELSE 0 END) \
         FROM _amux_request_log \
         WHERE method='POST' AND path LIKE '/api/sessions/%/report' AND ts >= ?1",
        [since],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0))),
    );
    match row {
        Ok((total, unattr)) => checks::reports_are_attributed(total, unattr),
        Err(e) => vec![InvariantResult::unknown(ID, format!("query failed: {e}"))],
    }
}

/// The steering queue, joined against each target's reported state.
///
/// Reads the same `session_reports` blob the delivery gate reads, so the check
/// and the mechanism it audits cannot disagree about what "idle" means — the
/// ethos rule about a view sharing the predicate of the mechanism it describes.
async fn steering_queue_check(state: &AppState) -> Vec<InvariantResult> {
    const ID: &str = "queue.has_live_consumer";
    // Read everything from the store in a scope that ENDS before any await: the
    // rusqlite Connection guard and Statement are !Send, so they must be fully
    // out of scope (not merely dropped) before lane_block_reason's tmux await,
    // or the whole invariant future stops being Send. This also releases the
    // read lock before that terminal I/O rather than holding it across the await.
    let (reports, rows): (serde_json::Value, Vec<(String, f64)>) = {
        let Ok(conn) = state.store.read() else {
            return vec![InvariantResult::unknown(ID, "store unreadable")];
        };
        let reports = conn
            .query_row("SELECT value FROM prefs WHERE key='session_reports'", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({}));
        let Ok(mut stmt) = conn.prepare(
            "SELECT session, MIN(queued_at) FROM steering_queue GROUP BY session",
        ) else {
            // The table not existing is a real answer (nothing queued), but an
            // unreadable one is not; do not turn a failed read into a clean pass.
            return vec![InvariantResult::unknown(ID, "steering_queue unreadable")];
        };
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))
            .map(|it| it.flatten().collect())
            .unwrap_or_default();
        (reports, rows)
    };

    let mut items: Vec<checks::QueuedItem> = Vec::with_capacity(rows.len());
    for (session, queued_at) in rows {
        let report = &reports[&session];
        let idle = report["state"].as_str() == Some("idle");
        // The report's own timestamp IS when the lane went idle: the Stop hook
        // writes the row at the end of the turn and nothing rewrites it until
        // the next state change, so `ts` on an idle report is the moment of the
        // busy->idle transition (AMUX-3572).
        let idle_since = if idle { report["ts"].as_f64() } else { None };
        // block_reason is the SAME predicate the delivery loop gates on
        // (session_verbs::lane_block_reason), so the check cannot disagree with
        // the mechanism about ROUTABILITY either, which is the missing half that
        // made a renamed-away ghost target read as a dead consumer (AMUX-3084).
        let block_reason = crate::api::session_verbs::lane_block_reason(&session)
            .await
            .map(str::to_string);
        items.push(checks::QueuedItem {
            queue: "steering".into(),
            target: session,
            queued_at,
            target_idle: idle,
            block_reason,
            idle_since,
        });
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    // 300s: comfortably more than several delivery ticks, so a normal
    // busy->idle transition never trips it, but far below the 2h6m the real
    // incident reached.
    checks::queue_has_live_consumer(&items, now, 300.0, crate::api::session_verbs::steer_dead_letter_s())
}

/// Client call sites, extracted from the shipped artifacts.
///
/// Sourced from the EMBEDDED dashboard bytes rather than from disk: the binary
/// serves what it embedded, so checking a file on disk would audit a different
/// artifact than the one users load — the same class of mistake as verifying a
/// fix against a file the server is not running.
fn extract_caller_paths() -> Vec<checks::CallerPath> {
    let mut out = Vec::new();
    if let Some(js) = amux_dashboard::DashboardAssets::get("app.js") {
        let text = String::from_utf8_lossy(&js.data);
        out.extend(scan_js_calls(&text, "spa:app.js"));
    }
    // THE CLI HALF (AMUX-2917). CallerPath::source has documented
    // `"spa:app.js" / "cli:amux"` since this check was written, and CLAUDE.md's
    // observability table describes the invariant as enumerating "SPA/CLI call
    // sites" — but only the SPA was ever scanned. The CLI is the fleet's other
    // real client (every `amux board`, `amux send`, `amux crm` is a curl), so
    // half the callers were outside the only check that can name an unrouted
    // one.
    //
    // Embedded at BUILD time, like app.js, deliberately: reading it off disk
    // would check whatever `amux` happens to be sitting in the checkout —
    // possibly a peer's mid-edit — instead of the source this binary was built
    // from. Same reason the e2e harness builds HEAD (AMUX-2924).
    out.extend(scan_shell_calls(include_str!("../../../../amux"), "cli:amux"));
    out
}

/// Pull `"$AMUX_URL/api/..."` call sites out of the bash CLI, with their method.
///
/// METHOD IS KNOWABLE HERE, unlike in the SPA, because curl's own rules decide
/// it: an explicit `-X VERB` wins, otherwise `-d`/`--data`/`--data-binary`
/// means POST, otherwise GET. That is not a heuristic, it is curl's documented
/// behaviour — so these call sites carry `method_known: true` and a genuine
/// 405 (route exists, wrong verb) is detectable in the CLI as well.
///
/// Anchored on the `curl` token rather than on the path, and that is the whole
/// accuracy story: this file is 2400 lines of shell in which `/api/...` also
/// appears inside help text, echoed examples and comments. Requiring a `curl`
/// within the preceding window is what keeps a printed example (`echo "  curl
/// -sk \"$AMUX_URL/api/workers/...\""`) from being reported as a live caller —
/// though an echoed example that really is malformed will still be caught,
/// which is a feature.
/// The last `curl` in `w` that is a real INVOCATION, not the word inside a
/// longer identifier or a comment (AF-191).
///
/// Two tests, and neither is a guess:
///
/// * the character AFTER the token must not continue the word. This is the one
///   that fixes the reported bug: `rfind("curl")` matched `curl_exit`, a JSON
///   field name in a printf format 251 chars before a path mentioned in prose.
/// * the token's own line must not be a `#` comment. A curl in a comment is a
///   recipe, not a call.
///
/// What it deliberately does NOT require is a shell operator before the token.
/// I borrowed that from `cli_curl_timeout_guard.rs` on the first attempt and it
/// broke 42 of the CLI's 63 curl mentions, because PR 143 routed every real call
/// site through the `_curl` WRAPPER — so the preceding character is `_`. That
/// test's polarity is the opposite of this one's: it hunts BARE curl and
/// excludes `_curl` on purpose, where this hunts call sites and `_curl` IS the
/// call site. Same discriminator, inverted sign; copying it unchanged inverted
/// the check. The existing CLI-census cells caught it, which is what they are for.
fn rfind_curl_invocation(w: &str) -> Option<usize> {
    let b = w.as_bytes();
    let mut from = w.len();
    while let Some(rel) = w[..from].rfind("curl") {
        let after_ok = b
            .get(rel + 4)
            .is_none_or(|c| !c.is_ascii_alphanumeric() && *c != b'_' && *c != b'-');
        let before_ok = rel
            .checked_sub(1)
            .map(|i| b[i])
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        let line_start = w[..rel].rfind('\n').map_or(0, |i| i + 1);
        let commented = w[line_start..rel].trim_start().starts_with('#');
        if after_ok && before_ok && !commented {
            return Some(rel);
        }
        if rel == 0 {
            break;
        }
        from = rel;
    }
    None
}

fn scan_shell_calls(sh: &str, source: &str) -> Vec<checks::CallerPath> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(p) = sh[i..].find("/api/") {
        let start = i + p;
        // The literal runs until a shell word terminator. A `$id` that is a WHOLE
        // mid-path segment is kept as a `:id` placeholder and scanning continues,
        // so a suffix after the param (the `/claim` in `/api/board/$id/claim`) is
        // checked, not discarded (AMUX-3134); a trailing or mid-segment `$` (and a
        // backtick) still ends the literal as an interpolated PREFIX.
        let (raw, consumed, mut interpolated) = extract_shell_path(&sh[start..]);
        i = start + consumed.max(1);

        // NOT trimming prose punctuation here, deliberately (AF-191). I wrote
        // that trim first — `/api/x,` in a sentence is a mention of `/api/x` —
        // and then could not construct a case where it mattered that the two
        // checks above do not already reject. A change no cell can pin is one
        // that ships on faith, so it came back out. If a specimen turns up
        // (an unquoted path with trailing punctuation on a non-comment line
        // after a real curl), add the trim WITH the cell that fails without it.
        let path = raw.trim_end_matches('/');
        if path.len() < 5 || !path.starts_with("/api/") {
            continue;
        }
        // A GLOB IS DOCUMENTATION, NOT A CALL SITE. `amux crm help` prints
        // "HTTP: /api/crm/*", and the curl anchor does NOT save it — the help
        // heredoc sits within 600 chars of the branch's real curls. So the
        // anchor narrows the phantom class, it does not eliminate it, and the
        // remaining shapes have to be named. `*` cannot appear in a request
        // path this server routes, so a literal containing one is prose.
        //
        // Caught by dumping what the scanner actually reported instead of
        // trusting the failure count: the census was clean, because the SPA
        // catch-all happens to match `/api/crm/*` today. A phantom that is
        // currently harmless is still a phantom, and it would have surfaced as
        // a false failure the moment that matching changed.
        if path.contains('*') || path.contains('{') || path.contains('%') {
            continue;
        }
        // A trailing slash is also a prefix (`/api/board/` before a space);
        // `extract_shell_path` already set `interpolated` for a cutting `$`/backtick.
        interpolated = interpolated || raw.ends_with('/');

        // Backward to the anchoring `curl`. Bash puts flags BEFORE the URL, so
        // backward is correct here — the opposite of the SPA, where the method
        // literal follows the URL.
        // THE PATH'S OWN LINE MUST NOT BE A COMMENT (AF-191). The reported
        // specimen was `# Ship the backlog to /api/client-debug, which logs …`
        // in the CLI — a sentence about an endpoint, not a call to it. Checked
        // here rather than only at the curl, because the two can sit on
        // different lines and it is the PATH whose line decides what it is.
        {
            let ls = sh[..start].rfind('\n').map_or(0, |i| i + 1);
            if sh[ls..start].trim_start().starts_with('#') {
                continue;
            }
        }
        let win_start = start.saturating_sub(600);
        let mut win_start = win_start;
        while win_start > 0 && !sh.is_char_boundary(win_start) {
            win_start -= 1;
        }
        let window = &sh[win_start..start];
        // A `curl` TOKEN IS NOT A CURL INVOCATION (AF-191).
        //
        // This was `rfind("curl")`, and on 2026-08-24 it matched `curl_exit` —
        // a JSON field name inside a printf format string 251 chars before a
        // path mentioned in a `#` comment. The guard that establishes "this
        // path is preceded by a curl call" was satisfied by a substring of an
        // identifier, so the invariant reported `GET /api/client-debug,` (note
        // the comma: prose punctuation) as an unmounted caller while the route
        // was mounted with both methods and answering 200.
        //
        // The discriminator is tsukimiya's, from `cli_curl_timeout_guard.rs` in
        // the same PR whose comment tripped this: a command starts a line or
        // follows a shell operator, so the character immediately before the
        // token decides it. Borrowed rather than re-derived — two spellings of
        // "is this a real invocation" is how they drift.
        let Some(curl_at) = rfind_curl_invocation(window) else { continue };
        let cmd = &window[curl_at..];

        let method = if let Some(x) = cmd.find("-X ") {
            cmd[x + 3..]
                .split_whitespace()
                .next()
                .unwrap_or("GET")
                .trim_matches(|c: char| !c.is_ascii_alphabetic())
                .to_uppercase()
        } else if cmd.contains(" -d ") || cmd.contains("--data") {
            // curl: a body without an explicit verb is a POST.
            "POST".to_string()
        } else {
            "GET".to_string()
        };
        if method.is_empty() {
            continue;
        }
        out.push(checks::CallerPath {
            method,
            path: path.to_string(),
            source: source.to_string(),
            interpolated,
            method_known: true,
        });
    }
    out
}

/// Extract the request path of a `/api/...` shell call, KEEPING a mid-path shell
/// expansion (`$id`, `${id}`, `$1`) that forms a WHOLE segment as a `:id`
/// placeholder and continuing past it — so the LITERAL SUFFIX after the param
/// (the `/claim` in `/api/board/$id/claim`) is checked against the route table
/// instead of discarded (AMUX-3134). Returns `(path, bytes_consumed, interpolated)`.
///
/// The pre-AMUX-3134 scanner stopped at the first `$` and marked the whole thing
/// an interpolated PREFIX, so `/api/board/$id/claim` was recorded as
/// `/api/board/` and the `/claim` suffix — the part that was unrouted — was never
/// checked. That is why the claim endpoint was invisible to the very invariant
/// that exists to catch it.
///
/// Phantom-failure resistance is preserved (amux-frustrations' constraint: never
/// trade a false negative for a false positive on a check people act on): a `$`
/// is reconstructed ONLY when it is a whole segment (preceded by `/`, followed by
/// `/`). A trailing `$id`, a mid-segment `$` (`foo$bar`), or a `` ` `` still ends
/// the literal as an interpolated prefix, exactly as before — the reconstruction
/// only ADDS the mid-path-param shape, and the `:id` placeholder matches only at a
/// route PARAM position (via `segments_match`), so a wrong suffix finds no route
/// and a right one matches, with no path ever guessed.
fn extract_shell_path(rest: &str) -> (String, usize, bool) {
    let b = rest.as_bytes();
    let mut out = String::new();
    let mut j = 0usize;
    let mut interpolated = false;
    while j < b.len() {
        let c = b[j];
        if c == b'`' {
            interpolated = true;
            break;
        }
        if c.is_ascii_whitespace()
            || matches!(c, b'"' | b'\'' | b'?' | b'\\' | b')' | b';' | b'|' | b'>')
        {
            break;
        }
        if c == b'$' {
            // Bound the `$var` / `${var}` token.
            let tok_end = if j + 1 < b.len() && b[j + 1] == b'{' {
                rest[j..].find('}').map(|k| j + k + 1)
            } else {
                let mut k = j + 1;
                while k < b.len() && (b[k].is_ascii_alphanumeric() || b[k] == b'_') {
                    k += 1;
                }
                (k > j + 1).then_some(k)
            };
            match tok_end {
                // A WHOLE mid-path segment: preceded by '/', followed by '/'.
                Some(te) if out.ends_with('/') && te < b.len() && b[te] == b'/' => {
                    out.push_str(":id");
                    j = te;
                    continue;
                }
                // Trailing or mid-segment expansion: a prefix, stop here.
                _ => {
                    interpolated = true;
                    break;
                }
            }
        }
        out.push(c as char);
        j += 1;
    }
    (out, j, interpolated)
}

/// Pull `API + '/api/...'` call sites out of the SPA, with their method.
///
/// Conservative by construction: only literal paths are extracted, and a path
/// built by interpolation is skipped rather than guessed at. A guessed path
/// would produce a phantom failure, and a check that cries wolf is one people
/// turn off — worse than the gap it was covering.
fn scan_js_calls(js: &str, source: &str) -> Vec<checks::CallerPath> {
    // Byte offsets from `find` are not necessarily char boundaries, and app.js
    // is full of box-drawing characters in comments. Slicing blind panicked on
    // the real bundle — caught by this module's own extractor test, which is
    // the argument for having one.
    let clamp = |i: usize| -> usize {
        let mut i = i.min(js.len());
        while i > 0 && !js.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = js[i..].find("'/api/") {
        let start = i + p + 1;
        let Some(endrel) = js[start..].find('\'') else { break };
        let end = start + endrel;
        let raw = &js[start..end];
        i = end;
        // Interpolated or query-bearing paths: keep the literal prefix only,
        // and skip if that leaves nothing addressable. Guessing at an
        // interpolated path produces phantom failures, and a check that cries
        // wolf gets turned off.
        let path = raw.split(['?', '$', '`']).next().unwrap_or("").trim_end_matches('/');
        if path.len() < 5 || !path.starts_with("/api/") {
            continue;
        }
        // Is the literal followed by concatenation/interpolation? Then `path`
        // is a PREFIX (`'/api/board/' + id`) and must be matched leniently.
        // Also true when the literal itself ended in '/' or carried a template
        // marker. Getting this wrong is not a stricter check but a wrong one —
        // it produced 86 false failures on the first live run.
        let tail = &js[clamp(end)..clamp(end + 12)];
        let interpolated = raw.ends_with('/')
            || raw.contains('$')
            || raw.contains('`')
            || tail.trim_start().starts_with('+');
        // Method literal, looking FORWARD first: `fetch(API + '/x', {method:
        // 'POST'})` puts it after the URL, which is the overwhelmingly common
        // shape. A backward-only window read every POST as a GET and would
        // have made the whole 405 class invisible — the exact bug this census
        // exists to catch. Backward is still consulted for the
        // `const opts = {method:'PATCH'}; fetch(url, opts)` shape.
        // BOUNDED TO THE SAME STATEMENT. A flat 200-char window bleeds past the
        // end of this call and attaches a neighbour's verb: `fetch('/api/
        // layout-presets')` (a GET) sat within 200 chars of a later
        // `{method:'DELETE'}` and was reported as `DELETE /api/layout-presets`
        // — a route that does not exist, while the DELETE the client really
        // makes (`/api/layout-presets/{name}`) is mounted and fine. One
        // confirmed false positive in the 2026-08-11 census, on a check whose
        // entire output is a work list. The options object lives inside the
        // same statement, so stopping at the first `;` keeps every real shape
        // and drops the bleed.
        let fwd_raw = &js[clamp(end)..clamp(end + 200)];
        let fwd = fwd_raw.split(';').next().unwrap_or(fwd_raw);
        // BOUNDED THE SAME WAY, and for the same reason. Bounding only forward
        // left the bug alive by the other door: app.js:3826's plain GET picked
        // up the `{method:'DELETE'}` from the DIFFERENT call six lines earlier
        // and was still reported as `DELETE /api/layout-presets`. Take only the
        // text after the previous `;` — the current statement.
        //
        // The shape this fallback was kept for (`const opts = {method:'PATCH'};
        // fetch(url, opts)`) does not occur in app.js: the one indirect call
        // (apiCall, :1842) passes a VARIABLE url, so this extractor — which
        // scans for literal '/api/...' strings — never reaches it. The fallback
        // was buying nothing and costing a phantom row.
        let back_raw = &js[clamp(start.saturating_sub(200))..clamp(start)];
        let back = back_raw.rsplit(';').next().unwrap_or(back_raw);
        let find_m = |hay: &str| {
            ["POST", "PATCH", "DELETE", "PUT"]
                .iter()
                .find(|m| {
                    hay.contains(&format!("'{m}'")) || hay.contains(&format!("\"{m}\""))
                })
                .map(|m| m.to_string())
        };
        let observed = find_m(fwd).or_else(|| find_m(back));
        let method_known = observed.is_some();
        let method = observed.unwrap_or_else(|| "GET".into());
        out.push(checks::CallerPath {
            method,
            path: path.to_string(),
            source: source.to_string(),
            interpolated,
            method_known,
        });
    }
    out.sort_by(|a, b| (&a.path, &a.method).cmp(&(&b.path, &b.method)));
    out.dedup_by(|a, b| {
        a.path == b.path && a.method == b.method && a.interpolated == b.interpolated
    });
    out
}

/// One evaluation pass: check, persist, reconcile incidents.
pub async fn tick(state: &AppState) -> (Confidence, usize) {
    let t0 = std::time::Instant::now();
    let results = evaluate_all(state).await;
    let conf = super::rollup(&results);
    // Publish for /health (AMUX-2625): the endpoint everyone polls reads this
    // cached verdict rather than re-running the suite per request.
    super::record_confidence(conf, chrono::Utc::now().timestamp() as f64);
    let opened = store::record(&state.store, results, t0.elapsed().as_millis() as i64).await;
    if opened > 0 {
        tracing::warn!(opened, confidence = conf.as_str(), "invariant incidents opened");
    }
    (conf, opened)
}

/// Background driver.
pub async fn run(state: AppState) {
    // Stagger past boot so the first pass sees a settled process rather than
    // half-initialised state, which would open incidents that immediately heal
    // and teach everyone the monitor is noisy.
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    loop {
        crate::runtime_jobs::registry::tick(crate::runtime_jobs::registry::ids::INVARIANTS);
        let st = state.clone();
        // A panic in one pass must not kill the monitor for the process
        // lifetime — a dead monitor is the failure this whole module exists to
        // make visible, so it must not be able to die quietly itself.
        if let Err(e) = tokio::spawn(async move { tick(&st).await }).await {
            tracing::error!(error = %e, "invariant tick panicked");
        }
        tokio::time::sleep(std::time::Duration::from_secs(TICK_SECS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The BINDING for the provider-launch invariant (RR-0043 / AMUX-3153):
    /// `provider_launch_check` must reach a verdict, carry only its own id, and
    /// PASS on the REAL registry + launch table — because today every provider's
    /// launch binary matches its adapter's. A FAIL here is a genuine
    /// launcher/adapter divergence, which is the whole point; a dropped binding
    /// would return nothing, and the incident it guards was invisible for exactly
    /// that reason.
    #[test]
    fn the_provider_launch_check_is_wired_and_green_on_the_real_tables() {
        let rs = provider_launch_check();
        assert!(!rs.is_empty(), "the binding must reach a verdict");
        assert!(
            rs.iter().all(|r| r.invariant_id == "provider.launch_matches_adapter"),
            "the sweep contract greps for this exact id"
        );
        let fails: Vec<_> = rs
            .iter()
            .filter(|r| r.status == crate::invariants::Status::Fail)
            .collect();
        assert!(
            fails.is_empty(),
            "launcher/adapter divergence on the live tables: {fails:?}"
        );
    }

    /// The BINDING, not the check. `status_agrees_with_pane` has negative
    /// controls of its own, but a pure check that `evaluate_all` never calls is
    /// a check nobody runs — the "capability that exists but reaches nobody"
    /// failure, one layer down. This asserts the wiring produces a verdict.
    ///
    /// Machine-independent by construction: on a box with a live tmux fleet it
    /// returns one result per probed lane, on one without it returns a single
    /// `Unknown`. What it may never do is return NOTHING, which is what a
    /// silently-dropped binding looks like.
    /// AF-246: the LOADER must actually read `archived`, not merely have a
    /// field for it.
    ///
    /// This test exists because a mutation proved the check-level tests cannot
    /// catch its absence. `checks::negative_controls::an_archived_card_behind_
    /// a_live_entry_is_reported_as_its_own_state` builds `LedgerRow`s directly,
    /// so gutting this loader to `(Some(s), false)` leaves it green — a real
    /// property, correctly asserted, one layer above where the defect would be
    /// introduced (AF-161's shape, found in my own work this time).
    ///
    /// So this one runs the SHIPPED path: a real store, a real archived issue,
    /// a real frustrations.md in a temp home, through `frustration_ledger_check`.
    #[test]
    fn the_ledger_loader_reads_archived_from_the_store() {
        let home = tempfile::tempdir().unwrap();
        // The ledger is read from `repo_dir()`, NOT from AMUX_HOME. The first
        // draft of this test wrote the fixture into the temp home and asserted
        // on the result — it read the REAL repo's frustrations.md the whole
        // time, whose cards are absent from this temp store, so every row was
        // skipped and the check passed for a reason that had nothing to do with
        // the property. It only surfaced because the assertion was written to
        // fail loudly rather than to confirm.
        //
        // AMUX_REPO_DIR is written into the fixture's server.env as well as the
        // process env, deliberately: HomeGuard restores exactly the fixture's
        // OWN server.env keys on drop (AMUX-3719), and Drop runs during unwind,
        // so a panic here cannot leak a temp repo dir into every later test.
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        std::fs::write(
            repo.path().join("frustrations.md"),
            // The header's own template is indented on purpose; entries start at
            // a column-0 `## ` after the `---`, which is what the parser keys on.
            "# amux frustrations\n\n---\n\n\
             ## an entry whose card is put away\n\
             AREA: cli\nSEVERITY: slows\nSTATUS: open\nDATE: 2026-08-26\n\
             SESSION: amux\nCARD: ZZ-1\nSYMPTOM: x\nCOST: minutes\nFIX: y\n",
        )
        .unwrap();
        let _h = crate::api::settings::test_env::set_home(home.path());
        crate::api::settings::set_server_env_key(
            home.path(),
            "AMUX_REPO_DIR",
            &repo.path().to_string_lossy(),
        )
        .unwrap();
        std::env::set_var("AMUX_REPO_DIR", repo.path());
        // Set-up assertion: if the override did not take, every assertion below
        // is about the real repo's ledger and means nothing.
        assert_eq!(
            crate::api::self_update::repo_dir().as_deref(),
            Some(repo.path()),
            "AMUX_REPO_DIR override did not take; the test would read the real ledger"
        );

        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("t.db")).unwrap();
        store
            .write(|conn| {
                conn.execute(
                    "INSERT INTO issues (id, title, status, archived, created, updated) \
                     VALUES ('ZZ-1', 't', 'todo', 1, 0, 0)",
                    [],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .expect("seed an ARCHIVED card");
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };

        let rs = frustration_ledger_check(&state);
        let agree = rs
            .iter()
            .find(|r| r.invariant_id == "frustrations.ledger_agrees_with_board")
            .expect("the agreement check must reach a verdict");

        // `todo` over an open entry AGREES on status. The only thing that can
        // make this fail is the archived flag having survived the query.
        assert_eq!(
            agree.status,
            crate::invariants::Status::Fail,
            "an archived card behind a live entry must fail; observed: {}",
            agree.observed
        );
        assert_eq!(
            agree.evidence["archived_open"].as_array().map(Vec::len),
            Some(1),
            "the loader dropped `archived` on the floor: {}",
            agree.evidence
        );

        // CONTROL, in the same test: unarchive the same card and the same
        // fixture must PASS. Without it, a build that failed every open entry
        // would satisfy the assertions above.
        state
            .store
            .write(|conn| {
                conn.execute("UPDATE issues SET archived=0 WHERE id='ZZ-1'", [])?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .expect("unarchive");
        let rs2 = frustration_ledger_check(&state);
        let agree2 = rs2
            .iter()
            .find(|r| r.invariant_id == "frustrations.ledger_agrees_with_board")
            .expect("verdict");
        assert_eq!(
            agree2.status,
            crate::invariants::Status::Pass,
            "the SAME entry over a LIVE todo card is agreement: {}",
            agree2.observed
        );
    }

    #[test]
    fn the_status_pane_check_is_actually_wired_into_the_monitor() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("t.db")).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
        let rs = status_pane_check(&state);
        assert!(!rs.is_empty(), "the binding must always reach a verdict");
        // status_pane_check now wires TWO checks over the SAME lanes: the
        // pane-agreement check and its sharper inverse (AMUX-3047,
        // status.contradicts_fresh_idle_report). Which ids appear is
        // machine-dependent — a box with no tmux fleet early-returns a single
        // `status.agrees_with_pane` Unknown before the lanes (and so the second
        // check) are built, while a box with a live fleet emits both — so this
        // asserts the binding reaches a verdict with NO id outside the expected
        // pair (a third id leaking in is the failure). The second check's own
        // discrimination is proven machine-independently in
        // `checks::tests::fresh_idle_report_contradiction_*`.
        assert!(
            rs.iter().all(|r| {
                r.invariant_id == "status.agrees_with_pane"
                    || r.invariant_id == "status.contradicts_fresh_idle_report"
            }),
            "unexpected invariant id — the sweep contract greps for these exact strings"
        );
    }

    /// AMUX-3134: a mid-path `$id` is kept as a `:id` placeholder so the suffix
    /// after it is still extracted, while a trailing or mid-segment `$` stays an
    /// interpolated prefix (no path guessed). The absence of this is what hid
    /// /api/board/{id}/claim from route.callers_have_routes.
    #[test]
    fn extract_shell_path_keeps_a_mid_path_param_suffix() {
        let (p, _, interp) = extract_shell_path("/api/board/$id/claim\"");
        assert_eq!(p, "/api/board/:id/claim");
        assert!(!interp, "a whole path with a param is NOT an interpolated prefix");
        let (p, _, interp) = extract_shell_path("/api/board/${card}/archive ");
        assert_eq!(p, "/api/board/:id/archive");
        assert!(!interp);
        // trailing $id -> prefix, still interpolated (unchanged, no guess)
        let (p, _, interp) = extract_shell_path("/api/board/$id\"");
        assert_eq!(p, "/api/board/");
        assert!(interp);
        // mid-segment $ (not a whole segment) -> prefix, interpolated
        let (p, _, interp) = extract_shell_path("/api/foo$bar/x ");
        assert_eq!(p, "/api/foo");
        assert!(interp);
    }

    /// The end-to-end regression, and the whole point of AMUX-3134: a CLI curl to
    /// /api/board/$id/claim is now a FULL caller path, so route.callers_have_routes
    /// reports Missing when the route is unmounted (the catch AMUX-3131 evaded and
    /// a human sweep had to make) and Ok once it is mounted — with no phantom on a
    /// path that IS routed.
    #[test]
    fn a_mid_path_param_caller_is_checked_against_the_route_table() {
        use crate::invariants::Status;
        let sh = "curl -sk -X POST \"$AMUX_API/api/board/$id/claim\"";
        let callers = scan_shell_calls(sh, "cli:amux");
        let claim = callers
            .iter()
            .find(|c| c.path.contains("claim"))
            .expect("the claim caller must be extracted");
        assert_eq!(claim.path, "/api/board/:id/claim");
        assert!(!claim.interpolated, "a full path, not a lenient prefix");
        assert_eq!(claim.method, "POST");

        // Unmounted -> Missing (the pre-AMUX-3131 world; THIS is the catch).
        let without: &[(&str, &[&str])] = &[("/api/board/{id}", &["GET", "PATCH"])];
        assert!(
            checks::route_callers_have_routes(without, &callers)
                .iter()
                .any(|r| r.status == Status::Fail && r.entity_key == "POST /api/board/:id/claim"),
            "an unrouted mid-path suffix must FAIL"
        );

        // Mounted -> Ok (no phantom on a routed path).
        let with: &[(&str, &[&str])] = &[("/api/board/{id}/claim", &["POST"])];
        assert!(
            checks::route_callers_have_routes(with, &callers)
                .iter()
                .any(|r| r.status == Status::Pass && r.entity_key == "POST /api/board/:id/claim"),
            "a mounted mid-path route must PASS"
        );
    }

    /// The SPA extractor must find real call sites in the shipped bundle. If it
    /// returns nothing the census silently covers nothing — the empty-probe
    /// trap this repo has hit repeatedly — so this is a control on the
    /// EXTRACTOR, not on the fleet.
    #[test]
    fn the_spa_extractor_finds_real_call_sites() {
        let calls = extract_caller_paths();
        assert!(
            calls.len() > 20,
            "expected many /api/ call sites in app.js, got {} — extractor is broken",
            calls.len()
        );
        assert!(
            calls.iter().any(|c| c.path.starts_with("/api/board")),
            "the SPA certainly calls /api/board; not finding it means the scan is wrong"
        );
    }

    /// Interpolated paths must be skipped, not guessed — a phantom failure
    /// trains people to ignore the check.
    /// A verb belonging to a LATER call must not be attached to this one.
    /// The flat 200-char forward window did exactly that and produced a
    /// confirmed false positive in the live census (2026-08-11): a plain GET
    /// reported as `DELETE /api/layout-presets`, a path that is not mounted,
    /// while the real DELETE goes to /api/layout-presets/{name} and works.
    /// A URL built into a variable puts the verb outside the URL's own
    /// statement, so no method is observable. The extractor must SAY so rather
    /// than default to GET and let the census file a phantom 405 — which is
    /// what `GET /api/dictate` was, while the real call five lines down is a
    /// POST.
    #[test]
    fn a_defaulted_verb_is_marked_as_not_observed() {
        let js = "const url = API + '/api/dictate?session=' + s;\n\
                  const r = await fetch(url, { method: 'POST' });";
        let got = scan_js_calls(js, "t");
        let d: Vec<_> = got.iter().filter(|c| c.path == "/api/dictate").collect();
        assert!(!d.is_empty(), "the path must still be extracted");
        for c in &d {
            assert!(!c.method_known, "no verb is in this statement — it must not be claimed as observed");
        }
        // A verb in the SAME statement is still observed.
        let js2 = "await fetch(API + '/api/dictate', {method:'POST'});";
        let got2 = scan_js_calls(js2, "t");
        let d2: Vec<_> = got2.iter().filter(|c| c.path == "/api/dictate").collect();
        assert!(d2.iter().all(|c| c.method_known && c.method == "POST"), "{got2:?}");
    }

    #[test]
    fn a_neighbours_method_is_not_attached_to_this_call() {
        let js = "const r = await fetch('/api/layout-presets');\n\
                  async function del(name) {\n\
                    await fetch('/api/layout-presets/' + name, {method:'DELETE'});\n\
                  }";
        let got = scan_js_calls(js, "t");
        let base: Vec<_> = got.iter().filter(|c| c.path == "/api/layout-presets" && !c.interpolated).collect();
        assert!(!base.is_empty(), "the plain GET must still be extracted");
        for c in &base {
            assert_eq!(c.method, "GET", "a later {{method:'DELETE'}} must not become this call's verb");
        }
        // ...and the real DELETE is still found, as an interpolated prefix.
        assert!(
            got.iter().any(|c| c.path == "/api/layout-presets" && c.interpolated && c.method == "DELETE"),
            "the parameterised DELETE must still be extracted: {got:?}"
        );
    }

    #[test]
    fn interpolated_paths_are_not_guessed() {
        let js = r#"fetch(API + '/api/workers/' + name + '/send', {method:'POST'})"#;
        let calls = scan_js_calls(js, "t");
        assert!(
            calls.iter().all(|c| !c.path.contains("${") && !c.path.contains('`')),
            "must not emit interpolation fragments as paths"
        );
    }

    /// Method detection must see an explicit POST rather than defaulting to GET,
    /// or every verb-missing bug (the 405 class) is invisible to the census.
    #[test]
    fn explicit_method_is_detected() {
        let js = r#"await fetch(API + '/api/board', {method:'POST', body:x})"#;
        let calls = scan_js_calls(js, "t");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, "POST", "a POST read as GET hides 405s");
    }
}


#[cfg(test)]
mod shell_scanner_tests {
    use super::*;

    /// curl's own rules, which is why these carry method_known=true: an
    /// explicit -X wins; a body without one is a POST; otherwise GET.
    #[test]
    fn the_method_comes_from_curls_rules_not_a_guess() {
        let sh = r#"
          curl -sk "$AMUX_URL/api/board"
          curl -sk -X PATCH -H 'x: y' -d "$body" "$AMUX_URL/api/prefs"
          curl -sk -d "$json" "$AMUX_URL/api/alert/owner"
          curl -sk -X DELETE "$AMUX_URL/api/schedules/SCHED-1"
        "#;
        let got: Vec<(String, String)> =
            scan_shell_calls(sh, "t").into_iter().map(|c| (c.method, c.path)).collect();
        assert_eq!(
            got,
            vec![
                ("GET".into(), "/api/board".into()),
                ("PATCH".into(), "/api/prefs".into()),
                ("POST".into(), "/api/alert/owner".into()),
                ("DELETE".into(), "/api/schedules/SCHED-1".into()),
            ]
        );
    }

    /// AF-191, the reported specimen verbatim: an endpoint named in a `#`
    /// comment was reported as an unmounted caller while the route was mounted
    /// with both methods and answering 200.
    ///
    /// Three things had to line up and all three are asserted:
    ///   the path carried prose punctuation           `/api/client-debug,`
    ///     (the visible symptom — NOT fixed by trimming it, see the scanner)
    ///   its line was a comment                        `# Ship the backlog to …`
    ///   the "is there a curl in front of it" guard    matched `curl_exit`, a
    ///     JSON field name in a printf format 251 chars earlier
    ///
    /// The third is the load-bearing one: a substring standing in for a
    /// structural test. Fixing only the punctuation would make this pass while
    /// leaving the scanner reading comments — the endpoint IS mounted, so the
    /// symptom disappears and the defect does not.
    #[test]
    fn a_path_named_in_a_comment_after_a_curl_shaped_identifier_is_not_a_caller() {
        let sh = "  printf '{\"ts\":%s,\"curl_exit\":%s,\"method\":\"%s\"}\\n' \\\n\
                  \x20   \"$(date -u +%s)\" \"$rc\" \"$method\"\n\
                  }\n\
                  \n\
                  # Ship the backlog to /api/client-debug, which logs the payload at INFO\n";
        let found = scan_shell_calls(sh, "cli:amux");
        assert!(
            found.is_empty(),
            "a path in a comment, preceded only by the identifier `curl_exit`, is prose: {found:?}"
        );

        // CONTROL 1 — the same path through the real wrapper IS a caller, or the
        // fix has not narrowed the scanner, it has blinded it. `_curl` is how
        // every call site in the CLI is written since PR 143.
        let real = "_curl -sk \"$AMUX_URL/api/client-debug\"\n";
        assert_eq!(
            scan_shell_calls(real, "cli:amux").len(),
            1,
            "a `_curl` call site must still be found — that is 42 of the CLI's 63 curl mentions"
        );

        // CONTROL 2 — a bare curl is still a caller too.
        let bare = "curl -sk \"$AMUX_URL/api/client-debug\"\n";
        assert_eq!(scan_shell_calls(bare, "cli:amux").len(), 1);

        // THE CELL THAT PINS THE CURL FIX ITSELF. Above, the comment check
        // catches the specimen first, so reverting the lookback leaves this
        // test green — I mutated it and it did, which is why this exists. Here
        // the path is on an ORDINARY line and the only `curl` in the lookback
        // is the identifier `curl_exit`, so the invocation test is the only
        // thing between prose and a false caller.
        let ident_only = "  printf '{\"curl_exit\":%s}\\n' \"$rc\"\n                          echo \"see $AMUX_URL/api/client-debug for details\"\n";
        assert!(
            scan_shell_calls(ident_only, "cli:amux").is_empty(),
            "`curl_exit` is an identifier, not an invocation — a substring match here is the \
             defect: {:?}",
            scan_shell_calls(ident_only, "cli:amux")
        );

        // THE CELL THAT PINS THE COMMENT CHECK. With a REAL curl earlier in the
        // file, the invocation test above is satisfied and only the path's own
        // line decides. This is the commoner shape in 2400 lines of shell than
        // the `curl_exit` coincidence: a genuine call, then prose mentioning a
        // different endpoint within the 600-char window.
        let real_curl_then_prose = "curl -sk \"$AMUX_URL/api/sessions\"\n                                    # see $AMUX_URL/api/client-debug for the payload format\n";
        let got = scan_shell_calls(real_curl_then_prose, "cli:amux");
        assert_eq!(
            got.len(),
            1,
            "the real call counts and the comment does not: {got:?}"
        );
        assert!(got[0].path.ends_with("/api/sessions"), "{:?}", got[0].path);

        // CONTROL 3 — punctuation is stripped, not the path.
        let punct = "_curl -sk \"$AMUX_URL/api/client-debug\",\n";
        let p = scan_shell_calls(punct, "cli:amux");
        assert_eq!(p.len(), 1);
        assert!(p[0].path.ends_with("client-debug"), "trailing comma must be gone: {:?}", p[0].path);
    }

    /// The anchor is what stops help text and comments being reported as live
    /// callers. Without it this file's 2400 lines of shell would produce
    /// phantom failures, and a check that cries wolf gets turned off.
    #[test]
    fn a_path_with_no_curl_in_front_of_it_is_not_a_caller() {
        let sh = r#"
          # see also /api/does-not-exist for the old contract
          echo "  try: $AMUX_URL/api/also-not-real"
        "#;
        assert!(scan_shell_calls(sh, "t").is_empty(), "comments and echoes are not call sites");
    }

    /// `$` cuts the literal, so `/api/board/$id` is a PREFIX. Treating it as an
    /// exact path is what produced 86 false failures on the SPA scanner's first
    /// live run.
    #[test]
    fn an_expansion_makes_the_path_a_prefix() {
        let got = scan_shell_calls(r#"curl -sk "$AMUX_URL/api/board/$id""#, "t");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "/api/board");
        assert!(got[0].interpolated, "an expansion means match leniently");
    }

    /// Documentation shapes are not call sites, even with a curl nearby — the
    /// anchor narrows the phantom class but does not eliminate it.
    #[test]
    fn a_glob_in_the_path_is_prose_not_a_caller() {
        let sh = r#"
          curl -sk "$AMUX_URL/api/crm/contacts"
          cat <<'EOH'
          amux crm — contacts (HTTP: /api/crm/*)
EOH
        "#;
        let got = scan_shell_calls(sh, "t");
        assert!(
            got.iter().all(|c| !c.path.contains('*')),
            "a glob is documentation: {:?}",
            got.iter().map(|c| &c.path).collect::<Vec<_>>()
        );
        assert!(got.iter().any(|c| c.path == "/api/crm/contacts"), "the real call still counts");
    }

    /// The real CLI must yield real call sites — an extractor that finds
    /// nothing is broken, not vindicated (the empty-grep trap).
    #[test]
    fn the_real_cli_yields_call_sites() {
        let found = scan_shell_calls(include_str!("../../../../amux"), "cli:amux");
        assert!(found.len() > 20, "only {} call sites scraped from the CLI", found.len());
        assert!(
            found.iter().any(|c| c.path.starts_with("/api/board")),
            "the CLI certainly calls /api/board"
        );
    }
}

#[cfg(test)]
mod extractor_wiring_tests {
    /// The CLI scan must actually be WIRED into the census, not merely exist.
    /// A scanner nobody calls and a codebase with no CLI defects produce the
    /// identical result — zero new failures — so the count alone cannot tell
    /// them apart (ethos rule 4).
    #[test]
    fn extract_caller_paths_includes_the_cli() {
        let all = super::extract_caller_paths();
        let cli: Vec<_> = all.iter().filter(|c| c.source == "cli:amux").collect();
        let spa: Vec<_> = all.iter().filter(|c| c.source == "spa:app.js").collect();
        assert!(!spa.is_empty(), "the SPA scan regressed");
        assert!(
            cli.len() > 10,
            "the CLI scan is not reaching the census — found {} cli callers out of {} total",
            cli.len(),
            all.len()
        );
    }
}
