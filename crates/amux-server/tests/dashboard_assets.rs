//! The dashboard's shipped assets must be INTACT and IN STEP — a guard for two
//! classes the standing checks provably cannot catch.
//!
//! 1. TRUNCATION. On 2026-08-11 a one-liner of the shape
//!    `open(p,'w').write(open(p).read().replace(...))` emptied `sw.js`: the
//!    write handle truncates the file before the argument is evaluated, so the
//!    read returned "" and 6123 bytes became 0 — committed and shipped. The
//!    PostToolUse hook runs `node --check`, which PASSED, because an empty
//!    program is valid JavaScript. A parse check is not a content check, and no
//!    amount of care substitutes for one that can fail (ethos rule 7).
//!
//! 2. VERSION SKEW. CLAUDE.md requires `APP_VER` (app.js) and `CACHE` (sw.js)
//!    to be bumped together — a browser holding the cached script otherwise
//!    never receives the fix. That rule has lived only in prose, so the one
//!    thing every client-side deploy depends on was enforced by memory.
//!
//! These read the SAME files `static_files.rs` embeds at compile time, so a
//! green run is about the bytes that actually ship.

use std::path::PathBuf;

fn asset(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../amux-dashboard/static")
        .join(name);
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// `const NAME = '...'` / `"..."` — the two declarations this repo actually uses.
fn const_str(src: &str, name: &str) -> Option<String> {
    let i = src.find(&format!("const {name}"))?;
    let rest = &src[i..];
    let eq = rest.find('=')? + 1;
    let tail = rest[eq..].trim_start();
    let quote = tail.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let body = &tail[1..];
    let end = body.find(quote)?;
    Some(body[..end].to_string())
}

#[test]
fn the_service_worker_still_contains_a_service_worker() {
    let sw = asset("sw.js");
    // The size floor is the blunt half and it is the one that would have caught
    // the incident: 0 bytes parses clean.
    assert!(
        sw.len() > 2000,
        "sw.js is {} bytes — suspiciously small; it was 0 once and `node --check` passed",
        sw.len()
    );
    // The specific half: name the handlers whose absence breaks a PWA, so a
    // partial write is caught too, not just a total one.
    for needle in [
        "addEventListener('install'",
        "addEventListener('activate'",
        "addEventListener('fetch'",
        "addEventListener('push'",
        "addEventListener('notificationclick'",
        "SHELL_URLS",
        "caches.open",
    ] {
        assert!(sw.contains(needle), "sw.js lost `{needle}` — a partial write, or a deletion nobody meant");
    }
}

#[test]
fn the_app_bundle_still_contains_an_app() {
    let app = asset("app.js");
    assert!(app.len() > 500_000, "app.js is {} bytes — far below the shipped bundle", app.len());
    let html = asset("index.html");
    assert!(html.len() > 50_000, "index.html is {} bytes — far below the shipped shell", html.len());
    // The SPA is unusable without these, and each has been broken by a delete
    // at least once in this repo's history.
    for needle in ["function openPeek", "function closePeek", "serviceWorker"] {
        assert!(app.contains(needle) || html.contains(needle), "the SPA lost `{needle}`");
    }
}

/// CLAUDE.md: "Client JS changes need APP_VER and the CACHE version bumped
/// together, or a browser holding the cached script never receives the fix."
/// Enforced here rather than remembered.
/// A WORKER BRANCH IS ISOLATION, NOT DELIVERY (AF-495).
///
/// From the 2026-09-04 Doron session. His worker was off main with nothing
/// pushed, and Ethan read both facts off the screen while Doron could not:
///
///   Ethan: "First off, your amux worker is in a different branch."
///   Doron: "No, I don't know. I don't know why that is."
///   Ethan (later): "It says it's in a different branch. It says nothing is
///                   pushed yet."
///   Doron: "Still. No, I think I'm on main again."   (he was not)
///
/// The branch popover's verdict for that exact state was a GREEN TICK reading
/// "Isolated on worker branch". True, and it is a reassuring signal over the
/// question that mattered: whether anything on the branch had ever left. The
/// popover has no push data and should not pretend to, so the fix is to stop the
/// green line from reading as "all good" and say what isolation does NOT cover.
///
/// Pinned here because it is a CLAIM the UI makes, and this file already holds
/// context-management copy to the available provider behavior. Prose in a
/// template is exactly what rots silently.
#[test]
fn the_branch_popover_does_not_read_isolation_as_delivery() {
    let js = asset("app.js");
    assert!(
        js.contains("Isolation is not delivery."),
        "the branch popover must say what being on a worker branch does NOT mean; \
         a bare green tick over an unmeasured condition is the defect (AF-495)"
    );
    assert!(
        js.contains("nothing here reaches anyone until it is merged or pushed"),
        "and name the consequence in the reader's terms, not as jargon"
    );
    // NEGATIVE: the old copy asserted a state it had not measured. If it comes
    // back, so does the false verdict.
    assert!(
        !js.contains("Isolated on worker branch"),
        "the old verdict is back: it reads as 'all good' for a branch nothing has \
         ever left"
    );
    // CONTROL: the conflict warning is a DIFFERENT and genuinely measured signal
    // (another worker shares the branch) and must survive untouched.
    assert!(
        js.contains("Another worker shares this branch"),
        "the conflict warning is measured and must not be lost to this change"
    );
}

#[test]
fn app_ver_and_the_sw_cache_version_agree() {
    let app_ver = const_str(&asset("app.js"), "APP_VER")
        .expect("app.js must declare `const APP_VER = '<version>'`");
    let cache = const_str(&asset("sw.js"), "CACHE")
        .expect("sw.js must declare `const CACHE = 'amux-v<version>'`");

    let expected = format!("amux-v{app_ver}");
    assert_eq!(
        cache, expected,
        "APP_VER ({app_ver}) and the sw.js CACHE ({cache}) disagree. Bump BOTH: a client \
         holding the cached script never receives a fix shipped under a stale cache key."
    );
}

/// Model choices used to be copied into app.js and two index.html selects,
/// which is how current OpenAI, Claude, and Gemini releases each appeared in
/// only part of the UI. Pin the one-source shape as well as the custom-id exit.
#[test]
fn every_dashboard_model_control_uses_the_shared_open_catalog() {
    let app = asset("app.js");
    let html = asset("index.html");
    assert!(
        app.contains("/api/models"),
        "dashboard never loads the shared catalog"
    );
    assert!(
        app.contains("_fillWorkerModelSelect"),
        "worker controls bypass the catalog helper"
    );
    assert!(
        app.contains("Custom model ID"),
        "future model ids have no open-string escape hatch"
    );
    for duplicate in ["const claudeModels", "const codexModels", "const geminiModels"] {
        assert!(
            !app.contains(duplicate),
            "duplicated provider list returned: {duplicate}"
        );
    }
    assert!(
        html.contains("settings-default-model-options"),
        "Claude default lost catalog-backed suggestions"
    );
    assert!(
        html.contains("create-model-custom"),
        "create-worker flow lost custom model ids"
    );
}

#[test]
fn idle_ready_work_names_the_queue_and_keeps_real_stalls_distinct() {
    let app = asset("app.js");
    let start = app.find("function _stalledChip(s)").expect("frontier chip renderer must exist");
    let tail = &app[start..];
    let end = tail.find("function updatePeekStatus()").expect("frontier chip must precede peek status");
    let chip = &tail[..end];

    for required in ["readyCards: d.ready", "queued behind", "_openWorkQueue(", "worker-queue", "data-queue-retry"] {
        assert!(app.contains(required), "queued-WIP rendering lost `{required}`");
    }
    assert!(chip.contains("work-queued-chip"), "the holding card must be a semantic control");
    assert!(chip.contains("queued-behind-wip"), "healthy WIP waits need a logged verdict");
    assert!(chip.contains("'stalled'"), "the no-holding control must preserve real stalled detection");
    assert!(chip.contains("no current work explains the block"), "stalled must say why it is alarming");
}

#[test]
fn worker_card_and_peek_share_actions_and_the_canonical_file_entry() {
    let app = asset("app.js");
    let html = asset("index.html");
    let css = asset("app.css");

    for required in [
        "function _workerActionDefinitions(s)",
        "function _renderWorkerActionMenu(s, surface)",
        "_renderWorkerActionMenu(s, 'card')",
        "_renderWorkerActionMenu(s, 'peek')",
        "data-worker-action",
        "data-peek-action=\"file-browser\"",
        "id=\"peek-focus-btn\"",
        "worker-action-menu-parity",
        "worker-file-entry",
    ] {
        assert!(app.contains(required), "shared worker-action contract lost `{required}`");
    }
    let inventory_start = app.find("function _workerActionDefinitions(s)")
        .expect("shared worker-action inventory must exist");
    let inventory_tail = &app[inventory_start..];
    let inventory_end = inventory_tail.find("function _renderWorkerActionMenu")
        .expect("the shared renderer must follow its inventory");
    let inventory = &inventory_tail[..inventory_end];
    assert_eq!(
        inventory.matches("{ key: '").count(),
        27,
        "the full running Claude worker fixture has 27 shared worker actions"
    );

    let browse_start = app.find("function _browseWorkerFiles(name, source)")
        .expect("canonical worker file entry must exist");
    let browse_tail = &app[browse_start..];
    let browse_end = browse_tail.find("function _reportWorkerActionParity")
        .expect("file entry must precede the parity diagnostic");
    let browse = &browse_tail[..browse_end];
    assert!(browse.contains("openExplore(root, name)"), "worker file entry must use full Files route");
    assert!(!browse.contains("togglePeekSplit"), "worker file entry must not retain the split-pane fork");
    assert!(
        html.contains("_browseWorkerFiles(peekSession,'peek-directory')"),
        "the displayed directory must use the canonical worker file entry"
    );
    assert_eq!(html.matches("id=\"peek-worker-menu-btn\"").count(), 1, "peek header action id must be unique");
    assert_eq!(html.matches("id=\"peek-composer-more-btn\"").count(), 1, "peek composer action id must be unique");
    assert_eq!(html.matches("id=\"peek-more-btn\"").count(), 0, "ambiguous duplicate peek-more-btn returned");
    assert!(
        css.contains(".peek-more-dropdown") && css.contains("overflow-y:auto") && css.contains("max-height:min(500px"),
        "the complete peek menu must remain scrollable on desktop and mobile"
    );
}

#[test]
fn board_worker_actions_group_wrapped_lines_under_their_timestamp() {
    let app = asset("app.js");
    let start = app
        .find("function _bdParseHistory(log)")
        .expect("board history parser must exist");
    let rest = &app[start..];
    let end = rest
        .find("function _bdWorkerActivity(item)")
        .expect("worker activity parser must follow history parser");
    let parser = &rest[..end];
    assert!(parser.contains("const grouped = []"), "parser no longer groups physical lines");
    assert!(
        parser.contains("grouped[grouped.length - 1].body += '\\n' + body.trim()"),
        "an untimestamped continuation must append to the preceding timestamped action"
    );
    assert!(
        !parser.contains("split('\\n').filter(l => l.trim()).map(line =>"),
        "the old one-physical-line-equals-one-action parser returned"
    );
}

#[test]
fn messages_link_schedule_ids_to_the_scheduler() {
    let app = asset("app.js");
    let start = app
        .find("async function _openScheduleFromMessage(id)")
        .expect("Messages must expose schedule navigation");
    let tail = &app[start..];
    let end = tail
        .find("function _linkifyUrls")
        .expect("schedule linkifier must precede URL linkification");
    let body = &tail[..end];
    for needle in [
        "switchView('scheduler')",
        "fetchSchedules()",
        "fetchSchedulerRuns()",
        "fetchSchedulerAudit()",
        "openSchedModal(sid)",
        "function _linkifyScheduleIds(safeHtml)",
    ] {
        assert!(body.contains(needle), "schedule navigation lost `{needle}`");
    }
    assert!(
        app.contains("_linkifyScheduleIds(_linkifyCardIds(safe))"),
        "the shared message-row renderer must link schedule ids in message text"
    );
    assert!(
        app.contains("_linkifyScheduleIds(origin.replace"),
        "scheduled-message origin is where the canonical SCHED-N token lives"
    );
}

#[test]
fn message_card_links_survive_the_capped_board_working_set() {
    let app = asset("app.js");
    let start = app
        .find("function _msgCardChip(cardId, message, linkedCard)")
        .expect("message card chip must accept authoritative card metadata");
    let tail = &app[start..];
    let end = tail
        .find("function _msgCtxPeek")
        .expect("message card chip must precede the shared message renderer");
    let body = &tail[..end];
    for needle in [
        "message.card_title",
        "message.card_status",
        "message.card_archived",
        "message.card_deleted",
        "const recorded = linkedCard ||",
        "const c = live ||",
        "<button type=\"button\" class=\"msg-card-chip\"",
    ] {
        assert!(
            body.contains(needle),
            "message card chip lost authoritative history metadata `{needle}`"
        );
    }
    assert!(
        app.contains("function _msgOpenCard(cardId)")
            && app.contains("_bdAudit('message-card-nav'")
            && body.contains("_msgOpenCard("),
        "message-card controls must use the shared navigation helper and emit a durable client-debug verdict"
    );
    assert!(
        app.contains("+ _msgCardChips(e);"),
        "the shared history row must render every authoritative task relation"
    );
    assert!(
        app.contains("card_title: x.card_title, card_status: x.card_status"),
        "normalizing history rows must preserve card metadata"
    );
    assert!(
        app.contains("async function openBoardDetail(id)")
            && app.contains("await apiCall(API + '/api/board/' + encodeURIComponent(id))"),
        "clicking a message's older/terminal task must hydrate it even when the capped board list omitted it"
    );
    assert!(
        !body.contains("<span class=\"msg-card-chip\""),
        "the message-to-card relation must be a semantic keyboard-accessible control, not a click-handled span"
    );
}

#[test]
fn long_shell_runs_have_an_immediate_visible_state() {
    let app = asset("app.js");
    for needle in [
        "case 'running':",
        "Already running on the host",
        "Started on the host",
        "running: 'running'",
        "_schedRunDotClass(r)",
    ] {
        assert!(
            app.contains(needle),
            "the scheduler UI lost its in-progress/overlap rendering `{needle}`"
        );
    }
    let css = asset("app.css");
    assert!(
        css.contains(".sched-run-dot.running"),
        "a durable running row must not render as the unknown grey dot"
    );
}

#[test]
fn cross_group_default_can_initialize_before_the_main_api_constant() {
    let app = asset("app.js");
    let read_start = app
        .find("async function readCrossGroupDefault()")
        .expect("cross-group settings need an authoritative reader");
    let init_end = app[read_start..]
        .find("async function toggleYoloDefault")
        .map(|n| read_start + n)
        .expect("cross-group initialization must precede the next settings helper");
    let early_boot = &app[read_start..init_end];
    let api_decl = app
        .find("const API = ''")
        .expect("the main API transport constant must still exist");

    assert!(
        init_end < api_decl,
        "this regression guard is specifically about the early settings initializer"
    );
    assert!(
        early_boot.contains("fetch('/api/config/cross-group'"),
        "the early reader/writer must use the root-relative endpoint"
    );
    assert!(
        !early_boot.contains("fetch(API + '/api/config/cross-group'"),
        "referencing API before its declaration throws in the temporal dead zone and silently leaves the toggle off"
    );
    for needle in [
        "saved.note || 'An explicit empty group or worker setting can deny this for that scope.'",
        "s.spans_groups_source",
        "s.spans_groups_reason",
        "refused (' + esc(source) + ' deny)",
    ] {
        assert!(
            app.contains(needle),
            "cross-group UI must expose the effective source/reason instead of contradicting enforcement: `{needle}`"
        );
    }
}

#[test]
fn all_worker_backlog_drain_is_a_persistent_settings_control() {
    let app = asset("app.js");
    let html = asset("index.html");
    for needle in [
        "async function readBoardDrainDefault()",
        "async function toggleBoardDrainDefault(checked)",
        "fetch('/api/config/board-drain'",
        "initBoardDrainDefault",
    ] {
        assert!(app.contains(needle), "board-drain settings lost `{needle}`");
    }
    for needle in [
        "board-drain-default-checkbox",
        "Auto-drain backlog for all workers",
        "Default ON: when To Do is empty",
    ] {
        assert!(html.contains(needle), "worker settings lost `{needle}`");
    }
}

#[test]
fn sse_message_invalidation_refreshes_each_visible_message_surface() {
    let app = asset("app.js");
    let start = app
        .find("if (key === 'messages')")
        .expect("SSE invalidation must recognize committed Messages writes");
    let body = &app[start..start + 1100.min(app.len() - start)];
    for needle in [
        "_messagesLoad(true)",
        "_peekMessagesLoad()",
        "_loadCmdHistoryFromServer()",
        "_renderCmdHistoryList()",
    ] {
        assert!(body.contains(needle), "message invalidation no longer refreshes `{needle}`");
    }
}

#[test]
fn only_the_explicitly_claimed_card_is_live_without_a_synthetic_unclaimed_state() {
    let app = asset("app.js");
    let index = asset("index.html");
    let helper_start = app
        .find("function _runtimeBoardCardId(s)")
        .expect("dashboard must derive the live doing card from the server's measured runtime truth");
    let helper_tail = &app[helper_start..];
    let helper_end = helper_tail
        .find("function _nudgeWorkersOnBoardChange()")
        .expect("live-card helper must precede board-change invalidation");
    let helper = &helper_tail[..helper_end];
    for needle in [
        "function _runtimeBoardCardId(s)",
        "truth.measured !== true",
        "truth.status !== 'linked'",
        "truth.card_id",
        "c.id === claimed",
        "c.session === name",
        "c.status === 'doing'",
        "!c.deleted && !c.archived",
    ] {
        assert!(helper.contains(needle), "live-card selection lost `{needle}`");
    }

    let render_start = app
        .find("function _renderSessionCard(s)")
        .expect("session-card renderer must exist");
    let render = &app[render_start..render_start + 16_000.min(app.len() - render_start)];
    for needle in [
        "const runtimeBoard = _runtimeBoardPresentation(s);",
        "runtimeBoard.cardId",
        "const displayTaskName = s.task_name || runtimeBoard.cardId || '';",
        "_workerExecutionBadge(s, runtimeBoard)",
        "_activeTaskLink(s.name, displayTaskBoardId, displayTaskName)",
    ] {
        assert!(render.contains(needle), "session card lost live board linkage `{needle}`");
    }
    // Execution badges are shared with worker details; verify the call above
    // and its implementation rather than demanding the old inline expression.
    let badge_start = app.find("function _workerExecutionBadge(s, runtimeBoard)").unwrap();
    let badge_tail = &app[badge_start..];
    let badge = &badge_tail[..badge_tail.find("function updatePeekStatus()").unwrap()];
    assert!(badge.contains("runtimeBoard.syncing") && badge.contains("_runtimeBoardSyncBadge()"));
    assert!(
        !render.contains("_cardDoingItem(s.name)"),
        "the worker card must not rebuild runtime truth from an independently refreshed boardItems snapshot"
    );
    assert!(
        app.contains("board-card-live-label\"><span class=\"board-live-dot\"></span>Working now"),
        "a live board card needs an explicit visible label, not only a border or tooltip"
    );
    assert!(
        app.contains("const _liveNow = !!(_liveCard && _liveCard.id === item.id)"),
        "only the explicitly claimed card may say Working now"
    );
    for needle in [
        "function _runtimeBoardSplitBadge(s)",
        "s.status !== 'unattributed'",
        "active-conflicting-claims",
        "automatically reconciles multiple live task claims",
        // `>card syncing</span>` was pinned here by 03061448 and deliberately
        // REMOVED from app.js by 9127257d ("remove false 'card syncing'
        // badges from worker cards"), which named three distinct causes of
        // the badge being wrong and left this needle behind. The test then
        // demanded a treatment the dashboard had stopped rendering on
        // purpose, so it reddened `rust` on main from 9127257d onward while
        // describing the failure as lost functionality.
        "truth.verdict",
    ] {
        assert!(app.contains(needle), "unattributed runtime lost its server-verdict treatment `{needle}`");
    }
    assert!(!app.contains(">card syncing</span>"),
        "normal runtime attribution lag must not manufacture a card-syncing warning");
    assert!(
        app.contains("verdict === 'active-conflicting-claims'")
            && app.contains("automatically reconciles multiple live task claims")
            && app.contains("status-badge waiting"),
        "competing live claims should stay internal while other unattributed idle states remain visible"
    );
    assert!(!app.contains(">card conflict</span>"),
        "claim reconciliation is harness work, not a human-facing status");
    assert!(
        !app.contains(">runtime/board split</span>"),
        "a recoverable task-link lag must not be presented as a red runtime failure"
    );
    for needle in [
        "let _sessionsSnapshotEpoch = 0",
        "snapshotEpoch !== _sessionsSnapshotEpoch",
        "let _boardSnapshotEpoch = 0",
        "snapshotEpoch !== _boardSnapshotEpoch",
        "function _runtimeBoardPresentation(s)",
        "if (status !== 'linked' || !cardId)",
        "_runtimeBoardSyncBadge()",
    ] {
        assert!(app.contains(needle), "a stale poll may publish an unmeasured or stale card link without `{needle}`");
    }
    for rejected in ["no board task claimed", "board-unclaimed-mount", "_activeWithoutClaim"] {
        assert!(!app.contains(rejected), "runtime activity must not manufacture the board pseudo-state `{rejected}`");
        assert!(!index.contains(rejected), "the removed pseudo-state must not retain a dead mount `{rejected}`");
    }
}

#[test]
fn idle_workers_explain_blocked_and_parked_board_work() {
    let app = asset("app.js");
    let start = app
        .find("function _boardDriveCardReason(drive)")
        .expect("worker cards need a board-drive explanation helper");
    let helper = &app[start..start + 2200.min(app.len() - start)];
    for needle in [
        "all-candidates-refused",
        "(dependency root)",
        "backlog auto-drain off",
        "backlog parked on human/trigger",
        "missing next action",
    ] {
        assert!(helper.contains(needle), "board-drive explanation lost `{needle}`");
    }

    let render_start = app
        .find("function _renderSessionCard(s)")
        .expect("session-card renderer must exist");
    let render = &app[render_start..render_start + 16_000.min(app.len() - render_start)];
    assert!(
        render.contains("(todo || backlog || d || review) && driveFresh"),
        "the explanation must cover every non-terminal work column, including backlog-only lanes"
    );
    assert!(
        render.contains("_boardDriveCardReason(drive)"),
        "the card must render the mechanism's explanation"
    );
}

/// The parser above must be able to FAIL, or the test above it is theatre —
/// a `const_str` that always returned None would make both sides `expect`-panic,
/// but one that silently returned the same string for everything would make the
/// comparison vacuous.
#[test]
fn the_version_parser_reads_real_values_and_rejects_junk() {
    assert_eq!(const_str("const APP_VER = '1.2.3';", "APP_VER").as_deref(), Some("1.2.3"));
    assert_eq!(const_str("const CACHE = \"amux-v1.2.3\";", "CACHE").as_deref(), Some("amux-v1.2.3"));
    // A trailing comment must not be swallowed into the value — app.js's real
    // line carries one ("// bump together with the sw.js CACHE version").
    assert_eq!(
        const_str("const APP_VER = '9.9.9';   // bump together", "APP_VER").as_deref(),
        Some("9.9.9")
    );
    assert_eq!(const_str("const APP_VER = 5;", "APP_VER"), None, "unquoted is not a version");
    assert_eq!(const_str("nothing here", "APP_VER"), None);
}

/// 3. DUPLICATE TOP-LEVEL FUNCTION NAMES. A third class the parse check cannot
///    see, and the one that shipped a live regression on 2026-08-25.
///
/// AMUX-3715 added `function _renderArchivedSection(container)` for the board's
/// archived section. The SESSIONS view already had a `_renderArchivedSection`
/// eleven thousand lines earlier. Function declarations hoist and the LAST one
/// wins, so the board version silently replaced the sessions version — and every
/// sessions call site passes no arguments, so it hit `container.appendChild` on
/// `undefined` and threw before the loading overlay could be hidden. The main
/// dashboard view was dead. gtm-research diagnosed and fixed it (7607ee46).
///
/// WHY EVERY EXISTING CHECK WAS GREEN, and this is the part worth keeping: the
/// LANGUAGE makes one of the two shapes an error and the other legal. A
/// duplicate `let`/`const` at the same scope is a SyntaxError that `node --check`
/// catches. A duplicate `function` is valid JavaScript. So the parse check gave
/// real coverage on half the failure and none on the other half, and nothing
/// distinguished the two halves from the outside.
///
/// The author's own commit message that day said every function the new code
/// CALLED had been checked to exist — which is the one-directional version of
/// this check, and the direction that was already covered. Every name you call
/// must exist; every name you define must not already. This is the mirror.
/// Body of a top-level `function NAME(` in app.js, brace-matched.
///
/// MATCHES `async function NAME(` TOO (AF-639, 2026-09-10). This helper
/// originally matched only the bare keyword, written against
/// `_staleShellRecover` when it was synchronous. A later, independent fix
/// made that same function `async` to await a real fetch, and this helper
/// went stale silently: `.unwrap_or_else(|| panic!(...))` fires on ANY
/// missing match, so "the function was renamed" and "the function grew an
/// `async` keyword" produce an identical panic, and the message names the
/// former. Confirmed live: `src.find("\nfunction _staleShellRecover(")` is
/// `None` against the current app.js, where the declaration is
/// `async function _staleShellRecover()`. Two-fix rule: search both forms
/// rather than special-case one caller, so the next function that becomes
/// async does not repeat this.
fn fn_body(src: &str, name: &str) -> String {
    let sync_head = format!("\nfunction {name}(");
    let async_head = format!("\nasync function {name}(");
    let i = src
        .find(&sync_head)
        .or_else(|| src.find(&async_head))
        .unwrap_or_else(|| panic!("no top-level function {name} in app.js"));
    let open = src[i..].find('{').expect("function has a body") + i;
    let bytes = src.as_bytes();
    let (mut depth, mut end) = (0usize, open);
    for (k, b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = k;
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(end > open, "unbalanced braces reading {name}");
    src[open..=end].to_string()
}

/// AF-639. A browser the server refuses to bootstrap 401s on every request for
/// the life of the window, and `_staleShellRecover` was written for a
/// DIFFERENT cause (a service-worker-cached shell holding a rotated-away
/// token) where reloading genuinely fetches a fresh token. Firing it here
/// reloads into an identical tokenless shell, once every ten minutes, forever,
/// with nothing on screen. Measured 2026-09-09: 22.5 hours and 28,355 401s
/// from one laptop, showing a dashboard that looked fine.
#[test]
fn a_shell_the_server_withheld_the_token_from_stops_reloading_and_says_so() {
    let src = asset("app.js");

    // The client must read the server's reason. Deriving it from an empty
    // token is exactly what it cannot do: auth-disabled looks identical and
    // must stay silent.
    assert!(
        src.contains("window._AMUX_AUTH_WITHHELD"),
        "app.js never reads the server's withheld flag, so it cannot tell \
         'auth is off' from 'this browser was refused'"
    );

    let body = fn_body(&src, "_staleShellRecover");
    let guard = body
        .find("_authWithheld")
        .expect("_staleShellRecover must special-case the withheld shell");

    // THE ANCHOR MOVED (AF-639, 2026-09-10). This assertion originally pinned
    // `location.reload`, because a blind reload into an identical shell was the
    // futile act the guard existed to skip. A separate fix landed on main the
    // same day and rewrote the recovery path to FETCH the real bootstrap and
    // act only on a genuinely DIFFERENT token (`location.replace('/?_fresh=auth'
    // ...)`), which already stops the reload storm and does it better. The
    // string "location.reload" no longer appears in this function's body at
    // all, so the old `.expect(...)` PANICS rather than failing an assertion --
    // confirmed live on origin/main's current app.js, where the merge that
    // combined both fixes kept the code correct (the withheld guard still
    // precedes the recovery, and still returns) but left this test anchored to
    // code that had already moved. A clean merge with no conflict markers is
    // not proof the result stayed testable (CLAUDE.md's own warning, applied to
    // a test rather than a feature). What survives is the same property this
    // cell always checked: the guard must precede whatever the OTHER arm does,
    // and must return before it. The other arm is now the recovery fetch.
    let recovery = body
        .find("fetch('/?_fresh=auth'")
        .expect("the recovery fetch is the other arm; if it moved, re-anchor this deliberately");
    assert!(
        guard < recovery,
        "the withheld check must come BEFORE the recovery fetch, or a browser that already \
         knows it was refused spends a 12s timeout re-learning it"
    );
    assert!(
        body[..guard].find("sessionStorage").is_none(),
        "the withheld arm must return before the reload rate-limiter, otherwise it burns the \
         once-per-10-minutes budget that the real stale-shell case needs"
    );
    assert!(
        body[guard..recovery].contains("return"),
        "the withheld arm must RETURN; falling through reaches the recovery it exists to skip"
    );

    // And it must leave something a human can act on, not just skip the
    // reload. A page that silently stops trying is the failure this card is
    // about (ethos rule 6: walk the escape).
    assert!(body.contains("_amuxAuthWithheldBanner"), "{body}");
    let banner = fn_body(&src, "_amuxAuthWithheldBanner");
    assert!(
        banner.contains("?_token="),
        "the banner must carry the ONE action that fixes this, not only the diagnosis"
    );
    assert!(
        banner.contains("min-height:44px"),
        "mobile rule: the sign-in controls are touch targets"
    );
    assert!(
        banner.contains("env(safe-area-inset-top"),
        "mobile rule: a fixed top bar must clear the iOS notch"
    );
}

#[test]
fn no_two_top_level_functions_in_app_js_share_a_name() {
    let src = asset("app.js");
    // Column-0 anchored: nested functions are indented, and this file's
    // top-level declarations are not. `const x = function` is not a
    // declaration and cannot collide by hoisting, so it is correctly excluded.
    let mut seen: std::collections::BTreeMap<String, usize> = Default::default();
    for line in src.lines() {
        let rest = match line.strip_prefix("async function ") {
            Some(r) => r,
            None => match line.strip_prefix("function ") {
                Some(r) => r,
                None => continue,
            },
        };
        let name: String =
            rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect();
        if !name.is_empty() {
            *seen.entry(name).or_insert(0) += 1;
        }
    }

    // PREMISE, asserted: the extractor found the population it is meant to
    // check. An anchor that stopped matching would make this pass over an empty
    // map forever, which is the vacuous green this whole file exists to refuse.
    assert!(
        seen.len() > 200,
        "extracted only {} top-level functions from app.js — the extractor is broken, not the \
         code. Fix it; do not delete the assert.",
        seen.len()
    );
    // And a name known to be there, so a match that silently narrowed is caught
    // as well as one that broke outright.
    assert!(seen.contains_key("renderBoard"), "extractor regressed: renderBoard not found");

    let dupes: Vec<String> =
        seen.iter().filter(|(_, n)| **n > 1).map(|(k, n)| format!("{k} ({n}x)")).collect();
    assert!(
        dupes.is_empty(),
        "two top-level functions share a name in app.js. Declarations HOIST, so the last one \
         silently replaces the earlier one and every earlier call site starts running the wrong \
         body — `node --check` cannot see this because a duplicate `function` is legal (a \
         duplicate `let` would be a SyntaxError, which is why that half was already covered). \
         Rename one: {}",
        dupes.join(", ")
    );
}

/// The old toggle wrote a preference no Rust consumer read. Do not offer a
/// control that claims to disable the provider's native context management.
#[test]
fn context_management_copy_does_not_offer_an_ineffective_toggle() {
    let html = asset("index.html");
    assert!(html.contains("Claude Code compacts context and continues the task automatically."));
    assert!(html.contains("/config menu"));
    assert!(!html.contains("auto-compact-checkbox"));
    let js = asset("app.js");
    assert!(!js.contains("auto_compact_enabled"));
}

#[test]
fn board_create_uses_the_server_field_names() {
    let app = asset("app.js");
    let start = app
        .find("async function addBoardItem(")
        .expect("addBoardItem exists");
    let tail = &app[start..];
    let end = tail.find("\n}\n").expect("addBoardItem closes") + 3;
    let body = &tail[..end];
    assert!(
        body.contains("session: worker || ''"),
        "board create must send `session`: {body}"
    );
    assert!(
        body.contains("tags: groups || []"),
        "board create must send `tags`: {body}"
    );
    assert!(
        !body.contains("worker: worker || ''") && !body.contains("groups: groups || []"),
        "`worker`/`groups` are UI names, not POST /api/board fields; the server reports them ignored"
    );
}

#[test]
fn board_detail_hydration_refreshes_authoritative_state_and_relations() {
    let app = asset("app.js");
    let start = app
        .find("async function _bdHydrate(")
        .expect("_bdHydrate exists");
    let tail = &app[start..];
    // END AT THIS FUNCTION'S OWN TOP-LEVEL CLOSE, not at the next function's
    // declaration. This used to look for "\n}\n\nfunction openBoardDetail", so it
    // pinned _bdHydrate's extent to the literal TEXT of an unrelated neighbour.
    // c6fd9832 ("fix(ui): open terminal tasks from message history") made
    // openBoardDetail `async`, and main went red with "_bdHydrate closes" — a
    // correct production change failing a test about a function it did not touch.
    // Nothing in _bdHydrate had changed, and the assertions below all still held.
    //
    // A check pinned to the wrong layer is exactly as green as one pinned to the
    // right layer, until it is not (ethos rule 7). "\n}\n" is the function's own
    // terminator: inner braces are indented, so a `}` at column 0 ends it whatever
    // follows.
    let end = tail.find("\n}\n").expect("_bdHydrate closes");
    let body = &tail[..end];
    assert!(
        !body.contains("function openBoardDetail"),
        "the extent ran past _bdHydrate into its neighbour — the anchor is wrong again"
    );
    for needle in [
        "boardDetailStatus = full.status",
        "_populateSessionSelect('bd-session', full.session",
        "_bdRenderMeta(merged)",
        "previewTab.classList.contains('active')",
        "renderMarkdown(d.value)",
        "full.due_time",
        "full.tags",
    ] {
        assert!(
            body.contains(needle),
            "hydration still leaves `{needle}` stale"
        );
    }
}

#[test]
fn board_detail_leads_with_actionable_task_context() {
    let html = asset("index.html");
    let meta = html.find("id=\"bd-meta\"").expect("task context container");
    let tabs = html.find("class=\"board-detail-tabs\"").expect("detail tabs");
    let edit = html.find("id=\"bd-edit-fields\"").expect("edit-only fields");
    assert!(
        tabs < meta && meta < edit,
        "Details must lead with source, epic, gates and assets before edit-only controls"
    );
    assert!(html.contains(">Details</button>"));
    assert!(html.contains(">Worker actions<span id=\"bd-hist-n\""));
    assert!(html.contains("id=\"bd-edit-fields\" style=\"display:none;\""));
    assert!(html.contains("id=\"bd-edit-footer\"") && html.contains("id=\"bd-delete\""));
    assert!(
        !html.contains("id=\"bd-tab-lineage\""),
        "database lineage is not the task card's primary content"
    );

    let app = asset("app.js");
    assert!(
        !app.contains("_bdRenderLineage") && !app.contains("_bdLineageHtml"),
        "the retired Lineage tab must not leave a hidden renderer or network path"
    );
    assert!(
        app.contains("maybeTab === 'lineage' ? 'preview'"),
        "old Lineage deep links must still resolve to the card's Details view"
    );
    for needle in [
        "item.gate_requirements",
        "item.asset_links",
        "a.resolved_ref",
        "_bdArtifactHref(",
        "window.location.origin",
        "Retired artifacts (",
        "const retiredArtifacts =",
        "const explicitPath =",
        "const serverResolvedPath =",
        "<button type=\"button\" class=\"file-link board-artifact-file\"",
        "targetPath = target.replace(/#.*$/",
        "Produced assets (",
        "Source message",
        "Worker request",
        "Terminal callback",
        "item.requested_by",
        "_bdOpenMessage(",
        "_bdWorkerActivity(",
        "Worker actions",
    ] {
        assert!(app.contains(needle), "card detail omitted `{needle}`");
    }
    let summary = app.find("const summary = [").expect("work summary");
    let assets = app[summary..].find("const artifacts = []").expect("asset section") + summary;
    assert!(
        !app[summary..assets].contains("['Evidence', item.evidence]"),
        "raw shell evidence must not dominate the default card"
    );
}

#[test]
fn group_suggestions_are_autocomplete_not_an_unprompted_wall() {
    let app = asset("app.js");
    let start = app
        .find("function _beTagInputUpdate(prefix)")
        .expect("tag autocomplete exists");
    let body = &app[start..start + 900.min(app.len() - start)];
    let empty = body.find("if (!q) { el.innerHTML = ''; return; }").expect("empty-query guard");
    let suggest = body.find("_tagSuggestions(prefix, q)").expect("typed suggestions remain");
    assert!(empty < suggest, "the empty query must stop before fleet groups are suggested");
}

#[test]
fn worker_cards_do_not_call_parked_work_active() {
    let app = asset("app.js");
    let start = app
        .find("const byStatus = _cardBoardStatusCounts(s.name)")
        .expect("worker card status breakdown exists");
    let body = &app[start..start + 1800.min(app.len() - start)];
    for label in ["backlog", "needs you", "review", "done"] {
        assert!(body.contains(label), "worker card omitted `{label}` count");
    }
    assert!(
        !body.contains("${active}</span> active"),
        "parked and done cards must not be collapsed into a misleading active count"
    );
}

#[test]
fn worker_configurations_are_editable_from_backlog_through_terminal_states() {
    let html = asset("index.html");
    assert!(
        html.contains("<span class=\"tab-lbl\">Configurations</span>"),
        "the worker surface must be named for what a user can do there"
    );

    let app = asset("app.js");
    assert!(
        app.contains("const _visCaps = (lvl === 'worker') ? d.capabilities"),
        "worker Configurations must show every capability returned by the server"
    );
    assert!(
        !app.contains("Edited where it lives"),
        "a writable worker configuration must not send the user to an unnamed second UI"
    );
    for needle in [
        "Every durable worker setting, grouped by what it changes",
        "Identity & organization",
        "Runtime & model",
        "Permissions & communication",
        "Display & advanced",
        "Task lifecycle",
        "_workerConfigurationRow('name'",
        "_workerConfigurationRow('provider'",
        "_workerConfigurationRow('model'",
        "_workerConfigurationRow('mcp'",
        "_workerConfigurationRow('cross_group'",
        "_workerConfigurationRow('external_email'",
        "_workerConfigurationRow('advanced_environment'",
        "_scopeEditOpen(\\'",
        "skin: 'JSON object",
        "connectors: 'JSON object",
        "Backlog → To Do",
        "To Do → In Progress",
        "Continue non-terminal work",
        "Pickup / continue master",
        "On by default; parked and human-owned cards stay put",
        "Status availability and Board gates below define transition requirements",
        "external_email_allowed",
        "Send external email without approval",
    ] {
        assert!(app.contains(needle), "Configurations omitted `{needle}`");
    }
    for field in [
        "auto_drain_backlog",
        "board_auto_pickup",
        "board_auto_continue",
        "board_standing_orders",
    ] {
        assert!(app.contains(&format!("field: '{field}'")), "missing runtime control for {field}");
    }
    assert!(
        app.contains("if (!present.has(k)) out[k] = null"),
        "removing a masked environment row must delete that worker-level key"
    );
    assert!(
        app.contains("if (z && typeof z === 'object') return Object.keys(z).length > 0"),
        "nested skin/connector settings must not render as an unset configuration"
    );
    assert!(
        app.contains("_workerBoardConfigurationSet(\\'")
            && app.contains("\\',null)\">Inherit</button>"),
        "worker overrides need an explicit path back to inherited configuration"
    );
    let css = asset("app.css");
    for needle in [
        ".worker-config-grid",
        ".worker-config-section",
        ".worker-config-row",
        "grid-template-columns:repeat(2,minmax(0,1fr))",
    ] {
        assert!(css.contains(needle), "Configurations layout lost `{needle}`");
    }
}

#[test]
fn worker_board_opens_current_work_without_expanding_every_idle_lane() {
    let app = asset("app.js");
    let start = app
        .find("function toggleSessionGroup(name, currentlyCollapsed)")
        .expect("worker board needs a visible-state toggle");
    let tail = &app[start..];
    let end = tail
        .find("function _issueRowHTML")
        .expect("collapse predicate must remain in the board-view section");
    let board = &tail[..end];

    assert!(
        board.contains("Object.prototype.hasOwnProperty.call(_sessionGroupCollapsed, name)"),
        "a saved user choice must beat the automatic default"
    );
    assert!(
        board.contains("status === 'doing' || status === 'review'"),
        "in-flight work must default open"
    );
    assert!(
        board.contains("_sessionGroupCollapsed[name] = !currentlyCollapsed"),
        "the first click must invert the state on screen, including a default-closed group"
    );
    assert!(
        app.contains("kind: 'board-worker-density'")
            && app.contains("verdict: 'idle-history-collapsed'")
            && app.contains("n_considered: sessionNames.length"),
        "the density fix needs a measured client-log signal"
    );
    assert!(
        !board.contains("const collapsed = _sessionGroupCollapsed[name || '__none__']"),
        "the old undefined-means-every-worker-open default returned"
    );
}

/// AF-390 fixed `#email-approvals-banner` swallowing clicks on the peek
/// overlay's fixed-position controls (`.overlay { z-index: 100 }`): an
/// in-flow global banner with `z-index: 200` painted over it once the banner
/// grew tall enough (narrow viewport -> its text wraps -> its box reaches
/// further down the screen). The fix set that ONE banner to `z-index: 90`
/// and left a comment stating the rule for every future one: "NOTHING IN
/// NORMAL FLOW MAY OUTRANK THESE TWO... If you add another global strip, put
/// it under 100 too."
///
/// AMUX-126 (2026-09-07): three more global banners violated exactly that
/// rule — `#no-apikey-banner`, `#org-banner`, `#org-invite-banner` all still
/// carried `z-index: 200`, inherited from before AF-390 landed and never
/// updated to match. CI caught the symptom (a real `locator.click` timeout on
/// mobile/ios-safari in `terminal-message-navigation.spec.ts`, `#no-apikey-
/// banner` named in the error as the element "intercepting pointer events")
/// but nothing had checked the RULE itself — a comment stating an invariant
/// is not a check that can fail (ethos rule 7). This scans every global
/// banner div for its inline z-index and fails if a new one is ever added (or
/// an old one edited) above the overlay's own 100.
/// The peek toolbar's message filter is a BUTTON, and a `<select>` must not
/// come back into that band (AF-591, AMUX-4242).
///
/// The old control was `<label class="peek-msg-filter" for="peek-msg-kind">`
/// wrapping a full-width `<select>`, which spent over half the toolbar's width
/// on one input and squeezed up, down, find, worker-menu and close to the right
/// edge. `amux` replaced it in c07923e6 with a funnel button plus a filter
/// panel, keeping the count badge so the number was not dropped.
///
/// This is the half that shipped without a signal. The UI fix is real and there
/// was nothing to catch it regressing: a later edit could reintroduce a select
/// into that band and every test would stay green. The two-fix rule asks for the
/// fix AND the thing that self-announces, and the spec for this change named
/// exactly this check as the one worth adding.
///
/// It slices the BAND rather than the file, because the dashboard has a dozen
/// legitimate `<select>` elements elsewhere (settings, library facets, proxy
/// form) and a file-wide assertion would either be false or have to whitelist
/// them, which is a list that rots.
#[test]
fn the_peek_toolbar_filter_is_a_button_and_not_a_select() {
    let index = asset("index.html");
    let lines: Vec<&str> = index.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.contains(r#"class="peek-toolbar""#))
        .expect("the peek toolbar band is gone from index.html; did the class change?");
    let end = lines[start..]
        .iter()
        .position(|l| l.contains("peek-find-wrap"))
        .map(|offset| start + offset)
        .expect("the find-wrap that closes the toolbar band is gone; re-anchor this test");
    let band = lines[start..end].join("\n");

    assert!(
        !band.contains("<select"),
        "a <select> is back in the peek toolbar band. It is the control this change removed, \
         because at full width it leaves no room for up/down/find/menu/close:\n{band}"
    );

    // POSITIVE CONTROLS. Without these, deleting the whole band passes the
    // assertion above, and so does a button that silently dropped the count.
    // Matched as the full id ATTRIBUTE, not as a substring. `contains("peek-msg
    // -count")` still matches `peek-msg-count-gone`, so a rename would have
    // slipped through: caught by mutating exactly that and watching this cell
    // stay green.
    assert!(
        band.contains(r#"id="peek-filter-btn""#),
        "the filter BUTTON is missing from the toolbar band:\n{band}"
    );
    assert!(
        band.contains(r#"id="peek-msg-count""#),
        "the message count badge was dropped; the number the <select> used to show must survive:\n{band}"
    );
    // And the band must still hold the actions the select was crowding out.
    for needle in ["peekMsgPrev", "peekMsgNext", "togglePeekFind", "closePeek"] {
        assert!(
            band.contains(needle),
            "the toolbar lost `{needle}`, which is what the space was reclaimed FOR:\n{band}"
        );
    }
}

#[test]
fn global_banners_never_outrank_the_peek_overlay() {
    let html = asset("index.html");
    // Every id in this list is a banner that renders in NORMAL DOCUMENT FLOW
    // (not `position: fixed`) at the top of the page, in the same screen band
    // as `.overlay` (z-index 100) and `#board-detail-overlay` (z-index 150) —
    // exactly the AF-390 hazard. A banner added under a NEW id needs adding
    // here too, or this test cannot see it.
    let banner_ids =
        ["no-apikey-banner", "org-banner", "org-invite-banner", "email-approvals-banner"];
    for id in banner_ids {
        let needle = format!("id=\"{id}\" style=\"");
        let start = html.find(&needle).unwrap_or_else(|| panic!("banner #{id} not found in index.html — did it move or get renamed?"));
        let tail = &html[start..];
        let tag_end = tail.find('>').expect("unterminated div tag");
        let style_attr = &tail[..tag_end];
        let zi_key = "z-index:";
        let zi_start = style_attr
            .find(zi_key)
            .unwrap_or_else(|| panic!("banner #{id} has no inline z-index at all — add one under 100, don't rely on the cascade default"))
            + zi_key.len();
        let zi_rest = &style_attr[zi_start..];
        let zi_end = zi_rest.find(';').unwrap_or(zi_rest.len());
        let z: i32 = zi_rest[..zi_end]
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("banner #{id}'s z-index isn't a plain integer: {e}"));
        assert!(
            z < 100,
            "banner #{id} has z-index:{z} -- AF-390's rule is nothing in normal flow may outrank \
             the peek overlay (z-index:100); a tall-wrapped banner at {z} will paint over and \
             swallow clicks on the overlay's own controls exactly like AF-390 did. Use 90, matching \
             #email-approvals-banner."
        );
    }
}

#[test]
fn workspace_invites_and_members_are_assigned_through_scoped_teams() {
    let app = asset("app.js");
    let html = asset("index.html");
    for needle in [
        "function openTeamEditor",
        "fetch('/api/org/teams')",
        "JSON.stringify({email, team_id})",
        "JSON.stringify({team_id})",
        "_workspaceTeamScope",
    ] {
        assert!(app.contains(needle), "workspace team UI lost `{needle}`");
    }
    for needle in ["Workspace access", "settings-teams-list", "+ Team", "+ Invite"] {
        assert!(html.contains(needle), "workspace access shell lost `{needle}`");
    }
    assert!(
        !app.contains("JSON.stringify({email, scope_level, scope_name})"),
        "the invite UI regressed to copying a one-off scope onto the user instead of assigning a team"
    );
}
