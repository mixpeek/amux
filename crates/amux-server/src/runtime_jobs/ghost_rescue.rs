//! `[ghost-rescue]` — the sweep that finds an amux message sitting UNSUBMITTED
//! in a lane's Claude Code input box and presses Enter for it (py:9153-9178).
//!
//! # This is a FALLBACK, and it has an exit condition
//!
//! A rescue is not a routine event. It is EVIDENCE THAT THE SEND PATH FAILED:
//! amux typed a message into a TUI and the Enter did not register, so the
//! message would have sat there forever. Every rescue is therefore logged at
//! WARN with the session and the text, and emits a `message.rescued` session
//! event — if this job is firing regularly, the bug to fix is upstream in
//! `send_text_inner`, not here.
//!
//! **EXIT CONDITION: delete this module when interactive lanes are driven
//! through `AgentProtocol` instead of keystrokes.** Submission over the
//! protocol is an ACK, not an inference, so there is no state in which text
//! can be "typed but not submitted" and nothing to sweep for. This job only
//! ever runs against lanes for which `lane_has_protocol_path` is false; on the
//! day that returns true for the fleet, the sweep will select nothing and can
//! be removed outright. It exists because the keystroke path is the ONLY path
//! today (measured 2026-08-09: `_amux_sessions` held 0 live rows, so 100% of
//! delivery is keystrokes), not because keystroke delivery is acceptable.
//!
//! # The guards, ported exactly — none of them are optional
//!
//! 1. **Never submit something a HUMAN is composing.** Python's discriminator
//!    is the only one that is actually sound, so it is the only one ported:
//!    every message amux's dashboard sends carries a client-side `[H:MM AM]`
//!    prefix, so pending input that STARTS with one is provably ours and can
//!    never be a person mid-sentence at a real terminal. Anything else is left
//!    alone — including amux's own agent-to-agent messages, which is a real
//!    coverage gap and the correct trade: a false rescue submits a human's
//!    half-written thought.
//! 2. **Never fight a picker, a modal, or a live turn.** The lane must read
//!    `idle`; a selector or a spinner means the Enter belongs to something
//!    else, and an Escape/Enter into a live turn interrupts it.
//! 3. **Never race a send.** The per-session send lock is held across the
//!    re-check and the keys, so a rescue cannot fire an Enter into a pane that
//!    `send_text_inner` is halfway through typing into.
//! 4. **Never resubmit the same text twice.** A rescue is recorded by
//!    (session, text) and suppressed for `RESCUE_COOLDOWN` afterwards, so a
//!    lane whose Enter is being eaten repeatedly produces one rescue and one
//!    warning, not a submit storm.
//!
//! Guard 2's re-check is deliberately a SECOND capture taken under the lock,
//! matching python: the first capture is what selected the lane, and acting on
//! a single torn frame is what once sent Escape into a just-started turn.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::api::AppState;

/// How often the sweep runs. Python's ran inside the ~13s status loop; 15s
/// keeps the same order without coupling to the scan cadence.
pub const GHOST_RESCUE_SECS: u64 = 15;

/// A rescued (session, text) pair is not rescued again for this long. Long
/// enough that a lane whose Enter is being eaten produces ONE warning per
/// message rather than a submit storm; short enough that a genuinely new
/// identical message hours later is still rescued.
const RESCUE_COOLDOWN: Duration = Duration::from_secs(30 * 60);

/// What one sweep did. Returned (not just logged) because a skip that leaves
/// no trace is indistinguishable from a sweep that found nothing — ethos rule
/// 4, and the handle every test in this file asserts on.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RescueReport {
    /// Lanes whose stuck message was submitted this pass.
    pub rescued: Vec<String>,
    /// Lanes examined (had a pane worth reading).
    pub examined: usize,
    /// Lanes holding pending input that did NOT carry the amux prefix — a
    /// human composing, or an agent-origin message this guard cannot claim.
    /// Counted so the coverage gap is visible instead of silent.
    pub left_alone: Vec<String>,
    /// Lanes whose stuck text was already rescued inside the cooldown.
    pub deduped: Vec<String>,
    /// Lanes whose composer held only Claude Code's dim SUGGESTION — i.e.
    /// nothing at all. Counted rather than ignored because this is exactly the
    /// population a stripped-ANSI reader mistook for stuck messages; if this
    /// number is large and `rescued` is zero, the sweep is working.
    pub placeholders: usize,
    /// Lanes whose composer holds a COLLAPSED PASTE CHIP — Claude Code's
    /// `[Pasted text #N +M lines]` — rather than readable text.
    ///
    /// This is a subset of `left_alone` and it is called out separately
    /// because the reason the sweep cannot claim it is different in kind, and
    /// worse. For ordinary text the discriminator READ the content and decided
    /// it was not ours. Here the content is not on screen at all: the TUI
    /// replaced it with a summary, so the `[H:MM AM]` prefix that
    /// [`is_amux_ghost`] looks for is inside the chip and unreadable by
    /// construction. Every amux message over ~400 chars goes out through
    /// tmux `paste-buffer` and renders exactly like this, which means the
    /// rescue is structurally blind to the LARGEST messages — and a larger
    /// message is more keystrokes, so a wider window for the Enter to be lost.
    ///
    /// It is reported and NOT rescued, deliberately. A human pastes too, and
    /// with the content hidden there is no evidence on screen that separates
    /// amux's paste from theirs. Submitting on a guess is the false rescue
    /// guard 1 exists to prevent, and choosing for the human is rule 8. So the
    /// honest move is to make it loud and let a person or the sender decide.
    pub chips: Vec<String>,
}

/// The pane operations a sweep needs. A trait so the guards can be tested
/// against planted frames without a tmux server — the alternative (mocking
/// `subprocess`) proves only that the mock was called, which is the D6 lesson.
#[allow(async_fn_in_trait)]
pub trait Pane: Send + Sync {
    /// Lane names to examine (non-archived, keystroke-hosted).
    fn lanes(&self) -> Vec<String>;
    /// Raw pane capture, ANSI included.
    async fn capture(&self, lane: &str) -> String;
    /// Send one tmux key name to the lane.
    async fn key(&self, lane: &str, key: &str);
}

/// Does this pending-input text provably belong to amux?
///
/// py:9160 — "Every message amux sends carries a client-side `[H:MM AM]`
/// prefix, so pending input starting with one is OURS — never something the
/// user is composing at a real terminal." Space-insensitive because
/// `pending_input` strips whitespace when it un-wraps the box.
pub fn is_amux_ghost(pending: &str) -> bool {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"^\[\d{1,2}:\d{2}[AP]M\]").expect("ghost prefix regex"));
    re.is_match(pending)
}

/// Is this composer showing a COLLAPSED PASTE rather than readable text?
///
/// Claude Code renders a large or multi-line paste as `[Pasted text #137 +44
/// lines]` and keeps the real content off screen. Matched loosely on the two
/// stable parts (the literal `Pasted text` and a `+<n> line` count) rather than
/// the exact punctuation, because the chip is another program's UI string and
/// pinning its spacing would make this silently stop matching on their next
/// release — failing CLOSED, back to the silence this exists to end.
pub fn is_collapsed_paste(pending: &str) -> bool {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // WHITESPACE-OPTIONAL, and that is not defensive styling. The input is
        // `composer_state`'s `Typed`, built with
        // `plain.extend(p.split_whitespace())` — words concatenated with the
        // separators DISCARDED — so the real specimen reaching this function is
        // `[Pastedtext#137+44lines]`, not the `[Pasted text #137 +44 lines]`
        // you read off the pane. A `\s+` here matches the string a human sees
        // and never the string the code passes, which is a check that cannot
        // fire, written against a paraphrase of its own input (ethos rule 7).
        regex::Regex::new(r"(?i)pasted\s*text.*?\+\s*\d+\s*lines?").expect("paste chip regex")
    });
    re.is_match(pending)
}

/// The sweep, and the only mutable state it has: which (lane, text) pairs it
/// already rescued. Deliberately NOT a process-global — a global made the
/// guards untestable in parallel (one test's reset clears another's record),
/// and "the dedupe is global" is exactly the kind of hidden coupling that
/// makes a rescue storm hard to reason about. One `Sweeper` lives for the
/// process; each test makes its own.
#[derive(Default)]
pub struct Sweeper {
    seen: Mutex<HashMap<(String, String), Instant>>,
}

impl Sweeper {
    pub fn new() -> Self {
        Self::default()
    }

    /// True if this (lane, text) may be rescued now; records it when so, so
    /// the caller may proceed exactly once per cooldown.
    fn claim(&self, lane: &str, ghost: &str) -> bool {
        let key = (lane.to_string(), ghost.to_string());
        let mut t = self.seen.lock().unwrap();
        t.retain(|_, at| at.elapsed() < RESCUE_COOLDOWN);
        if t.contains_key(&key) {
            return false;
        }
        t.insert(key, Instant::now());
        true
    }
}

/// One sweep. `state` is optional so the guards can be exercised without a DB;
/// when present, a rescue emits the `message.rescued` session event.
pub async fn sweep<P: Pane>(sweeper: &Sweeper, pane: &P, state: Option<&AppState>) -> RescueReport {
    use crate::api::session_verbs::{composer_state, detect_claude_status, ComposerState};

    let mut report = RescueReport::default();
    for lane in pane.lanes() {
        let raw = pane.capture(&lane).await;
        if raw.trim().is_empty() {
            continue;
        }
        report.examined += 1;
        // Guard 2: only an IDLE lane. "active" means the Enter belongs to the
        // running turn; "waiting" means a selector owns it.
        if detect_claude_status(&raw) != "idle" {
            continue;
        }
        // Guard 0, and the one that decides whether there is anything to do at
        // all: REAL typed input, not Claude Code's dim suggestion. Keying this
        // on the ANSI-stripped ❯ line (as python did) reported 13 live lanes as
        // "stuck for hours" when all 13 composers were empty — see
        // `composer_state`. A sweep that "rescues" a placeholder submits
        // nothing and logs that it did, which is the worst of both.
        let ComposerState::Typed(ghost) = composer_state(&raw) else {
            if let ComposerState::Placeholder(_) = composer_state(&raw) {
                report.placeholders += 1;
            }
            continue;
        };
        if ghost.is_empty() {
            continue;
        }
        // Guard 1: the human-composing discriminator.
        if !is_amux_ghost(&ghost) {
            report.left_alone.push(lane.clone());
            if is_collapsed_paste(&ghost) {
                report.chips.push(lane.clone());
                // ONE warning per stuck chip, not one every 15s, by reusing
                // guard 4's own dedupe rather than a second cooldown. A lane
                // holding a paste for an hour is 240 sweeps; this is the
                // difference between a signal and a reason to filter the log.
                if sweeper.claim(&lane, &ghost) {
                    tracing::warn!(
                        session = %lane, chip = %ghost,
                        "[ghost-rescue] lane holds an UNSUBMITTED collapsed paste — the sweep cannot claim it (content hidden by the TUI, so ownership is unprovable) and will not submit on a guess"
                    );
                }
            }
            continue;
        }
        // Guard 4: exactly once per (lane, text).
        if !sweeper.claim(&lane, &ghost) {
            report.deduped.push(lane.clone());
            continue;
        }
        // Guard 3 + python's re-check: hold the send lock, take a SECOND
        // capture, and require the lane to still be idle with the SAME text.
        let lock = crate::api::session_verbs::session_send_lock(&lane);
        let _guard = lock.lock().await;
        let raw2 = pane.capture(&lane).await;
        if detect_claude_status(&raw2) != "idle" || composer_state(&raw2).typed() != Some(ghost.as_str()) {
            continue;
        }
        // Escape closes any picker WITHOUT selecting an entry (a bare Enter
        // would pick one and rewrite an @path), then Enter submits the literal
        // text. py:9169.
        pane.key(&lane, "Escape").await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        pane.key(&lane, "Enter").await;
        // `ghost` is a PANE SCRAPE, and `composer_state` builds it with
        // `plain.extend(p.split_whitespace())` — words concatenated with the
        // whitespace DISCARDED. That is right for its real job (comparing two
        // captures of a wrapped terminal line, where whitespace is an artifact of
        // the wrap), and wrong for anything a human reads.
        //
        // AEAB-38: on 2026-08-20 this line logged a real 12:50AM message as
        // "Canyoufightthisforme?Don'tsendbutdodraftandaddresstotherightperson",
        // which reads as the user's prompt having been corrupted in delivery. It
        // was not — the board card auto-captured from the same send has the text
        // intact, and the rescue presses Enter on whatever is already in the
        // composer rather than retyping it, so delivery cannot mangle anything.
        //
        // The storage stays as-is: changing it would alter the equality check
        // above and `is_amux_ghost`, for a display problem. So the LINE says what
        // the preview is instead of implying it is the message.
        let preview: String = ghost.chars().take(80).collect();
        // LOUD, because a rescue means the send path failed.
        tracing::warn!(
            session = %lane,
            preview = %preview,
            "[ghost-rescue] session={lane} submitted stuck amux message; the send path \
             failed to submit it, which is a defect, not routine. Pane-scrape preview \
             (WHITESPACE STRIPPED — this is not the literal message, and the message \
             itself is unaltered): {preview:?}"
        );
        if let Some(st) = state {
            crate::api::session_verbs::emit_event(
                st,
                &lane,
                "message.rescued",
                Some(serde_json::json!({
                    "preview": ghost.chars().take(120).collect::<String>(),
                    "notice": format!("Rescued a stuck message in '{lane}' — it was typed but never submitted"),
                })),
                None,
                "ghost-rescue",
            )
            .await;
        }
        report.rescued.push(lane);
    }
    report
}

/// The live pane: the tmux fleet, minus archived lanes, minus anything a
/// structured protocol can reach (there is nothing to rescue when submission
/// is an ACK — and nothing to press keys into either).
pub struct TmuxFleet {
    pub state: AppState,
}

impl Pane for TmuxFleet {
    fn lanes(&self) -> Vec<String> {
        crate::api::session_verbs::keystroke_lanes(&self.state)
    }
    async fn capture(&self, lane: &str) -> String {
        crate::api::session_verbs::tmux_capture(lane, 25).await
    }
    async fn key(&self, lane: &str, key: &str) {
        crate::api::session_verbs::send_key(lane, key).await;
    }
}

/// Spawn the sweep as an internal periodic job. Ephemeral by design (see
/// `runtime_jobs`' module docs): no persistence, no audit — a maintenance loop
/// must never masquerade as a user schedule.
/// The last completed sweep, or None if it has never run — which is itself the
/// answer to "is the rescue alive?".
///
/// This existed as a return value that only the tests read: the periodic job
/// logged ONLY when it rescued something, so a sweep that saw a lane holding an
/// unsubmitted message and correctly declined to claim it produced no log line,
/// no counter and no endpoint. `left_alone`'s own docstring already claimed the
/// gap was "visible instead of silent" and nothing anywhere read the field
/// (ethos rule 1: it existed, it reached no one). A human had to notice a stuck
/// pane by looking at it.
static LAST_REPORT: std::sync::OnceLock<std::sync::RwLock<Option<RescueReport>>> =
    std::sync::OnceLock::new();

pub fn last_report() -> Option<RescueReport> {
    LAST_REPORT.get()?.read().ok()?.clone()
}

fn publish(r: &RescueReport) {
    let cell = LAST_REPORT.get_or_init(|| std::sync::RwLock::new(None));
    if let Ok(mut slot) = cell.write() {
        *slot = Some(r.clone());
    }
}

pub fn spawn(state: AppState) -> super::PeriodicTask {
    let sweeper = std::sync::Arc::new(Sweeper::new());
    super::spawn_periodic("ghost-rescue", GHOST_RESCUE_SECS, move || {
        let fleet = TmuxFleet { state: state.clone() };
        let sweeper = sweeper.clone();
        async move {
            let st = fleet.state.clone();
            let report = sweep(&sweeper, &fleet, Some(&st)).await;
            publish(&report);
            if !report.rescued.is_empty() {
                tracing::warn!(
                    rescued = ?report.rescued,
                    examined = report.examined,
                    "[ghost-rescue] sweep submitted stuck messages"
                );
            }
        }
    })
}

#[cfg(test)]
mod tests {

    /// The chip is what amux-cloud was holding when Ethan asked "why are you
    /// sending unsubmitted text?" — a 44-line paste sitting in an idle lane's
    /// composer, invisible to every instrument amux had.
    #[test]
    fn a_collapsed_paste_is_recognised_and_ordinary_text_is_not() {
        // THE FORM THE CODE ACTUALLY RECEIVES. `composer_state` returns
        // `Typed` with every separator stripped, so this — not the spaced
        // version below — is the live amux-cloud specimen as this function
        // sees it. Asserting only the pretty form is how a predicate ships
        // green and matches nothing in production.
        assert!(is_collapsed_paste("[Pastedtext#137+44lines]"));
        assert!(is_collapsed_paste("[Pastedtext#2+1line]"));
        // The pane-readable form still matches, for any caller that has not
        // been through composer_state.
        assert!(is_collapsed_paste("[Pasted text #137 +44 lines]"));
        assert!(is_collapsed_paste("pasted text #9  + 12  Lines"));

        // IT MUST BE ABLE TO SAY NO. A predicate that matched ordinary text
        // would file every human mid-sentence as a stuck paste, and the
        // warning would become noise nobody reads.
        assert!(!is_collapsed_paste(""));
        assert!(!is_collapsed_paste("[10:24AM]goaheadandlandit"));
        assert!(!is_collapsed_paste("ipastedtheconfigintothefile"));
        // Named the concept but carries no line count: not a chip.
        assert!(!is_collapsed_paste("[Pastedtext#4]"));
        // A line count with no paste: not a chip either.
        assert!(!is_collapsed_paste("diffis+44lines"));
    }

    /// The two discriminators must not overlap: a chip can never satisfy the
    /// `[H:MM AM]` prefix, which is the whole reason the rescue is blind to
    /// large messages. If this ever fails, the blindness is gone and the chip
    /// branch is dead code that should be removed rather than left to rot.
    #[test]
    fn the_prefix_discriminator_cannot_see_inside_a_chip() {
        assert!(!is_amux_ghost("[Pastedtext#137+44lines]"));
        // ... while the same message, rendered in full, IS claimable. This is
        // the pair that shows the gap is about RENDERING, not about content.
        assert!(is_amux_ghost("[10:24AM]thesamemessage,notcollapsed"));
    }
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// A planted pane. `frames` is consumed one capture at a time so a test can
    /// make the lane change between the selecting capture and the re-check.
    struct FakePane {
        lanes: Vec<String>,
        frames: StdMutex<Vec<String>>,
        keys: StdMutex<Vec<(String, String)>>,
    }
    impl FakePane {
        fn new(lane: &str, frames: Vec<String>) -> Self {
            Self {
                lanes: vec![lane.to_string()],
                frames: StdMutex::new(frames),
                keys: StdMutex::new(vec![]),
            }
        }
        fn keys_sent(&self) -> Vec<(String, String)> {
            self.keys.lock().unwrap().clone()
        }
    }
    impl Pane for FakePane {
        fn lanes(&self) -> Vec<String> {
            self.lanes.clone()
        }
        async fn capture(&self, _lane: &str) -> String {
            let mut f = self.frames.lock().unwrap();
            if f.len() > 1 {
                f.remove(0)
            } else {
                f.first().cloned().unwrap_or_default()
            }
        }
        async fn key(&self, lane: &str, key: &str) {
            self.keys.lock().unwrap().push((lane.to_string(), key.to_string()));
        }
    }

    /// An idle Claude Code frame with `text` sitting unsubmitted in the box.
    /// Shaped from the real thing: box borders, the ❯ composer, the bypass
    /// status bar. `detect_claude_status` must read this as idle.
    fn idle_frame(text: &str) -> String {
        format!(
            "\u{2500}\u{2500}\u{2500} lane \u{2500}\u{2500}\n\
             \u{276f} {text}\n\
             \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
             \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle)\n"
        )
    }

    fn active_frame(text: &str) -> String {
        format!(
            "\u{2731} Galloping\u{2026} (12s)\n\
             \u{2500}\u{2500}\u{2500}\u{2500}\n\
             \u{276f} {text}\n\
             \u{2500}\u{2500}\u{2500}\u{2500}\n\
             \u{23f5}\u{23f5} bypass permissions on \u{b7} esc to interrupt\n"
        )
    }

    #[tokio::test]
    async fn planted_unsubmitted_amux_message_is_submitted_exactly_once() {
        let sweeper = Sweeper::new();
        let ghost = "[04:27 PM] this status doesnt seem right";
        let pane = FakePane::new("lane-a", vec![idle_frame(ghost)]);
        let r1 = sweep(&sweeper, &pane, None).await;
        assert_eq!(r1.rescued, vec!["lane-a".to_string()], "first sweep must rescue");
        assert_eq!(
            pane.keys_sent(),
            vec![
                ("lane-a".to_string(), "Escape".to_string()),
                ("lane-a".to_string(), "Enter".to_string())
            ],
            "rescue is Escape then Enter, in that order"
        );
        // Second sweep on the SAME stuck text: the dedupe must hold, or a lane
        // whose Enter is eaten gets a submit storm every 15s.
        let r2 = sweep(&sweeper, &pane, None).await;
        assert!(r2.rescued.is_empty(), "second sweep must not resubmit: {r2:?}");
        assert_eq!(r2.deduped, vec!["lane-a".to_string()]);
        assert_eq!(pane.keys_sent().len(), 2, "no extra keys on the deduped pass");
    }

    #[tokio::test]
    async fn human_composing_is_never_submitted() {
        let sweeper = Sweeper::new();
        // No `[H:MM AM]` prefix: this is a person typing at a real terminal.
        // The whole guard is that amux cannot tell the difference any other
        // way, so it must refuse.
        for typed in [
            "i was in the middle of writing this",
            "/compact",
            "@src/main.rs please look at",
            "[not a timestamp] hello",
            "04:27 PM] missing bracket",
        ] {
            let pane = FakePane::new("lane-h", vec![idle_frame(typed)]);
            let r = sweep(&sweeper, &pane, None).await;
            assert!(r.rescued.is_empty(), "must not rescue human text {typed:?}: {r:?}");
            assert!(pane.keys_sent().is_empty(), "must not press any key for {typed:?}");
            assert_eq!(r.left_alone, vec!["lane-h".to_string()]);
        }
    }

    #[tokio::test]
    async fn a_generating_lane_is_never_touched() {
        let sweeper = Sweeper::new();
        // Mid-turn the Escape would INTERRUPT the running response and the
        // Enter would resubmit into it — the 2026-07-13 duplicate.
        let pane = FakePane::new("lane-b", vec![active_frame("[04:27 PM] queued while working")]);
        let r = sweep(&sweeper, &pane, None).await;
        assert!(r.rescued.is_empty(), "must not rescue a generating lane: {r:?}");
        assert!(pane.keys_sent().is_empty());
    }

    #[tokio::test]
    async fn recheck_under_the_lock_aborts_when_the_pane_moved() {
        let sweeper = Sweeper::new();
        // Selected on an idle frame; by the re-check the lane has started a
        // turn (or a send typed something else). Acting on the first frame is
        // exactly the torn-look bug.
        let ghost = "[04:27 PM] this status doesnt seem right";
        let pane = FakePane::new(
            "lane-c",
            vec![idle_frame(ghost), active_frame(ghost)],
        );
        let r = sweep(&sweeper, &pane, None).await;
        assert!(r.rescued.is_empty(), "re-check must abort: {r:?}");
        assert!(pane.keys_sent().is_empty(), "no keys after an aborted re-check");
    }

    #[tokio::test]
    async fn an_empty_composer_is_not_a_ghost() {
        let sweeper = Sweeper::new();
        let pane = FakePane::new("lane-d", vec![idle_frame("")]);
        let r = sweep(&sweeper, &pane, None).await;
        assert!(r.rescued.is_empty());
        assert!(r.left_alone.is_empty(), "an empty box is not 'left alone', it is nothing");
        assert!(pane.keys_sent().is_empty());
    }

    #[test]
    fn ghost_prefix_matches_the_real_dashboard_stamp() {
        // The live specimens, space-stripped exactly as pending_input returns
        // them (AMUX-2629's own stuck message is the first).
        assert!(is_amux_ghost("[06:55PM]thereare9queuedcommands"));
        assert!(is_amux_ghost("[4:05AM]something"));
        assert!(!is_amux_ghost("hello [06:55 PM] not at the start"));
        assert!(!is_amux_ghost(""));
    }
}
