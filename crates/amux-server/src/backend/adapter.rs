//! Terminal-output -> WorkerEvent adapter (RR-0033, Invariant 5).
//!
//! Invariant 5 is the D1 exit: the terminal is an adapter, not the control
//! plane. Hooks and the OpenCode protocol emit [`WorkerEvent`]s directly;
//! this module exists ONLY for the signals they do not expose structurally
//! today — provider rate-limit banners, in-band API errors, prompt/idle
//! renders, a dead CLI leaving a bare shell. It is a FALLBACK. As hook
//! coverage grows this file shrinks toward a liveness check, which is why
//! every pattern cites its Python source line: the pattern table is ported
//! knowledge, not new design.
//!
//! Source of truth for every regex: the live Python server,
//! `amux-server.py` at repo root ("py NNNN" comments below cite its line
//! numbers as of 2026-08-09). Patterns were EXTRACTED, not invented — the
//! Python's false-positive scars (quoted banners, subagent deaths, echoed
//! prompts) are encoded in the guards here and each guard cites the incident
//! comment it was ported from.
//!
//! Purity contract: [`TerminalAdapter::scan`] is a pure function of the
//! captured text. It reads no clock, keeps no state, and reports every event
//! the capture evidences on THIS call. Deduplication across repeated scans —
//! the Python's two-scan persistence gate (py 7297-7310: "no regex fully
//! separates 'this session is gated' from 'this session is displaying text
//! about the gate' ... the two differ in TIME") — is the CALLER's job,
//! because it requires memory of the previous scan.
//!
//! Purity also means the wall-clock reset formats ("resets 9:50am",
//! "Resets in: 4 hours 23 minutes" — py 6261-6291, 6541-6548) are
//! deliberately NOT parsed here: "today or tomorrow" needs a clock and a
//! local timezone. The one absolute, timezone-stated format (the in-band 4xx
//! quota reset, py 6392-6394) IS parsed. For everything else `RateLimit.raw`
//! keeps the banner text so a clock-holding caller can parse it — ethos rule
//! 4: a wrong normalization must be diagnosable from the data we keep.

use amux_core::protocol::{ExitStatus, Failure, RateLimit, RateLimitKind, WaitReason, WorkerEvent};
use amux_core::provider::ProviderId;
use chrono::{DateTime, TimeZone, Utc};
use regex::Regex;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Pattern sources (the RR-0033 tally). One const per Python regex so
// `rate_limit_patterns` can enumerate coverage and the compiled statics
// share the same source string — one source of truth per pattern.
// ---------------------------------------------------------------------------

// -- Claude ------------------------------------------------------------------

/// The /rate-limit-options menu, rendered when a subscription usage cap is
/// hit (py 6240-6255). Anchored on the option-1 line: the "1." prefix
/// confirms a menu render, not the phrase in scrollback noise.
const P_CLAUDE_MENU: &str = r"(?i)1\.\s*stop and wait for limit to reset"; // py 6254

/// Weekly/usage-cap banner, menu-less (py 6293-6311). NOTE: the "usage"
/// alternate also matches Codex's cap phrasing — in the Python one loop
/// scans every pane, so codex banners are disambiguated by checking
/// `_PROVIDER_LIMIT_RES` first (py 7230-7254, AMUX-2088: a codex cap was
/// classified as a Claude weekly limit and auto-resume was scheduled off the
/// WRONG provider's reset). Here provider identity is a constructor input,
/// so a codex pane never meets this regex — the precedence hazard is closed
/// structurally rather than by check ordering.
const P_CLAUDE_WEEKLY: &str =
    r"(?i)(?:you'?ve\s+)?(?:hit|reached)\s+your\s+(?:weekly|usage)\s+limit"; // py 6308-6311

/// Session-limit banner — the 5-hour window, resets same-day (py 6313-6322).
const P_CLAUDE_SESSION: &str = r"(?i)(?:you'?ve\s+)?(?:hit|reached)\s+your\s+session\s+limit"; // py 6319-6322

/// Per-model credit exhaustion banner anchor (py 6324-6334). No reset time
/// exists; "continue" won't unblock it.
const P_CLAUDE_CREDIT_ANCHOR: &str = r"(?i)/usage-credits\b|switch\s+models?\s+with\s+/model"; // py 6331-6334

/// Required corroboration for the anchor above: the banner always STATES the
/// limit; prose merely citing "/usage-credits" does not (py 6335-6343).
const P_CLAUDE_CREDIT_CTX: &str = r"(?i)reached\s+your\s+[A-Za-z0-9][A-Za-z0-9.\s-]{0,19}?\s+limit"; // py 6340-6343

/// The menu-style credit gate ("1. Set up usage credits on claude.ai") —
/// carries NEITHER anchor above; missed entirely until 2026-07-27
/// (py 6344-6358, 6512-6515).
const P_CLAUDE_CREDIT_MENU_OPT: &str = r"(?im)^\s*\d+\.\s*set\s+up\s+usage\s+credits"; // py 6512-6515

/// Corroborating prose for the credit menu (py 6516-6520).
const P_CLAUDE_CREDIT_MENU_PROSE: &str = r"(?i)(?:uses|runs\s+on|have)\s+usage\s+credits"; // py 6517-6520

/// Spend-limit adjustment menu, detection only — its default-selected row
/// adjusts MONEY, so the Python never auto-answers it (py 6521-6532).
const P_CLAUDE_SPEND_OPT: &str = r"(?i)Adjust\s+monthly\s+spend\s+limit"; // py 6530
const P_CLAUDE_SPEND_WAIT: &str = r"(?i)Wait\s+for\s+limit\s+to\s+reset"; // py 6531
const P_CLAUDE_SPEND_BAL: &str = r"(?i)Usage\s+credit\s+balance"; // py 6532

/// Transient 5xx (529 Overloaded etc), anchored: the marker line must START
/// with the error, which is how Claude Code emits it — a session EXPLAINING
/// the error also contains it mid-line and once false-positived (py
/// 6359-6374, 6500-6507). The loose count-only form is `RE_API_5XX_LOOSE`.
const P_CLAUDE_API_5XX: &str = r"(?i)^(?:[⏺●]\s*)?API Error:\s*(5\d\d)\b"; // py 6374

/// In-band 4xx API-key quota exhaustion — a HARD gate with a
/// machine-readable UTC reset (py 6375-6394, AMUX-2111). The phrase and
/// timestamp are matched with line breaks REMOVED because tmux hard-wraps
/// mid-word ("You will rega/in access", observed live).
const P_CLAUDE_API_QUOTA_MARKER: &str = r"(?i)^(?:[⏺●]\s*)?API Error:\s*4\d\d\b"; // py 6388-6389
const P_CLAUDE_API_QUOTA_PHRASE: &str = r"(?i)API\s*usage\s*limits"; // py 6390-6391
const P_CLAUDE_API_QUOTA_RESET: &str =
    r"(?i)regain\s*access\s*on\s*(\d{4})-(\d{2})-(\d{2})\s*at\s*(\d{1,2}):(\d{2})\s*UTC"; // py 6392-6394

/// The gateway's 402 budget wall (py 6395-6403, AMUX-2113). No reset time —
/// credit-path semantics.
const P_CLAUDE_API_BUDGET_MARKER: &str = r"(?i)^(?:[⏺●]\s*)?API Error:\s*402\b"; // py 6400-6401
const P_CLAUDE_API_BUDGET_PHRASE: &str = r"(?i)budget\s*exhausted"; // py 6402-6403

/// The Claude tally: 16 regexes forming 9 detection classes (menu, weekly,
/// session, credit banner, credit menu, spend menu, 5xx, 4xx quota, 402
/// budget). The plan's "~14" predates the spend-menu (GMA-29) and 402
/// (AMUX-2113) additions.
const CLAUDE_PATTERNS: &[&str] = &[
    P_CLAUDE_MENU,
    P_CLAUDE_WEEKLY,
    P_CLAUDE_SESSION,
    P_CLAUDE_CREDIT_ANCHOR,
    P_CLAUDE_CREDIT_CTX,
    P_CLAUDE_CREDIT_MENU_OPT,
    P_CLAUDE_CREDIT_MENU_PROSE,
    P_CLAUDE_SPEND_OPT,
    P_CLAUDE_SPEND_WAIT,
    P_CLAUDE_SPEND_BAL,
    P_CLAUDE_API_5XX,
    P_CLAUDE_API_QUOTA_MARKER,
    P_CLAUDE_API_QUOTA_PHRASE,
    P_CLAUDE_API_QUOTA_RESET,
    P_CLAUDE_API_BUDGET_MARKER,
    P_CLAUDE_API_BUDGET_PHRASE,
];

// -- Gemini ------------------------------------------------------------------

// The Python holds these as ONE regex with a top-level alternation
// (`_PROVIDER_LIMIT_RES`, py 6303-6306); split here into its two alternates
// so the per-pattern tally (RR-0033: "2 for Gemini") is enumerable.
// Bounded span (`.{0,80}`) so DOTALL can't stitch prose to a distant
// "Gemini" mention (py 6300-6302).
const P_GEMINI_QUOTA_EXCEEDED: &str = r"(?is)Quota exceeded.{0,80}Gemini"; // py 6305 (alternate 1)
const P_GEMINI_QUOTA_REACHED: &str = r"(?is)You have reached your (?:daily )?quota.{0,80}Gemini"; // py 6305 (alternate 2)

const GEMINI_PATTERNS: &[&str] = &[P_GEMINI_QUOTA_EXCEEDED, P_GEMINI_QUOTA_REACHED];

// -- Codex -------------------------------------------------------------------

/// Codex usage-limit banner (py 6296-6304, AMUX-2088: sat on a session card
/// reading HEALTHY while the agent was hard-stopped until Aug 27). The
/// Python deliberately does NOT lift a reset time from it — that is the
/// wrong-provider-reset bug — so `reset_at` stays `None` here too.
const P_CODEX_USAGE_LIMIT: &str = r"(?is)You've hit your usage limit\..{0,160}?(?:Codex|ChatGPT)"; // py 6304

const CODEX_PATTERNS: &[&str] = &[P_CODEX_USAGE_LIMIT];

// -- Ollama ------------------------------------------------------------------
// ollama workers now run `codex --oss --local-provider ollama`, so they emit
// the same structured events as Codex and are scanned by the same patterns.
// OLLAMA_PATTERNS was removed; pattern routing in rate_limit_patterns() and
// TerminalAdapter::scan() both forward "ollama" to the codex code paths.

// ---------------------------------------------------------------------------
// Compiled statics (LazyLock: compiled once, no new deps)
// ---------------------------------------------------------------------------

macro_rules! lazy_re {
    ($name:ident, $src:expr) => {
        static $name: LazyLock<Regex> =
            LazyLock::new(|| Regex::new($src).expect(concat!("static regex ", stringify!($name))));
    };
}

// The canonical ANSI/OSC escape stripper (py 8223-8226), verbatim: CSI
// sequences, OSC-8 hyperlinks, BEL- and ST-terminated OSC, charset
// selection, and the generic two-byte escape form.
lazy_re!(
    RE_STRIP_ANSI,
    r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b\]8;[^\x1b]*\x1b\\|\x1b\][^\x07]*\x07|\x1b\][^\x1b]*\x1b\\|\x1b[()][A-Z0-9]|\x1b[\x20-\x2f]*[\x40-\x7e]"
);

lazy_re!(RE_CLAUDE_MENU, P_CLAUDE_MENU);
lazy_re!(RE_CLAUDE_WEEKLY, P_CLAUDE_WEEKLY);
lazy_re!(RE_CLAUDE_SESSION, P_CLAUDE_SESSION);
lazy_re!(RE_CREDIT_ANCHOR, P_CLAUDE_CREDIT_ANCHOR);
lazy_re!(RE_CREDIT_CTX, P_CLAUDE_CREDIT_CTX);
lazy_re!(RE_CREDIT_MENU_OPT, P_CLAUDE_CREDIT_MENU_OPT);
lazy_re!(RE_CREDIT_MENU_PROSE, P_CLAUDE_CREDIT_MENU_PROSE);
lazy_re!(RE_SPEND_OPT, P_CLAUDE_SPEND_OPT);
lazy_re!(RE_SPEND_WAIT, P_CLAUDE_SPEND_WAIT);
lazy_re!(RE_SPEND_BAL, P_CLAUDE_SPEND_BAL);
lazy_re!(RE_API_5XX_ANCHORED, P_CLAUDE_API_5XX);
lazy_re!(RE_API_QUOTA_MARKER, P_CLAUDE_API_QUOTA_MARKER);
lazy_re!(RE_API_QUOTA_PHRASE, P_CLAUDE_API_QUOTA_PHRASE);
lazy_re!(RE_API_QUOTA_RESET, P_CLAUDE_API_QUOTA_RESET);
lazy_re!(RE_API_BUDGET_MARKER, P_CLAUDE_API_BUDGET_MARKER);
lazy_re!(RE_API_BUDGET_PHRASE, P_CLAUDE_API_BUDGET_PHRASE);
lazy_re!(RE_GEMINI_QUOTA_EXCEEDED, P_GEMINI_QUOTA_EXCEEDED);
lazy_re!(RE_GEMINI_QUOTA_REACHED, P_GEMINI_QUOTA_REACHED);
lazy_re!(RE_CODEX_USAGE_LIMIT, P_CODEX_USAGE_LIMIT);

// Support regexes (structural anchors and status detection; not part of the
// rate-limit tally).

// Proof the session produced output on a line: assistant message (⏺/●),
// echoed user message (❯ [09:32 …]), or tool-run summary. Both ⏺ U+23FA and
// ● U+25CF accepted: live `tmux capture-pane` renders one glyph, the peek
// endpoint the other — fixtures matched while live panes did not until both
// were listed (py 6927-6934, AMUX-2111).
lazy_re!(RE_LIMIT_ACTIVITY, r"^(?:[⏺●]\s|❯\s*\[\d|Ran \d+ shell command)"); // py 6934

// Loose 5xx form, used only to COUNT occurrences once the anchored form
// made the decision (py 6370-6373): one blip vs a wedged retry loop.
lazy_re!(RE_API_5XX_LOOSE, r"(?i)API Error:\s*(5\d\d)\b"); // py 6370

// Spinner token-counter suffix — generation evidence for the activity veto
// (py 7129).
lazy_re!(RE_TOKENS_SUFFIX, r"(?m)tokens\)\s*$"); // py 7129

// "❯ 1. Yes"-style selector — a live multi-choice prompt (py 18570).
lazy_re!(RE_SELECTOR, r"❯\s*\d+\."); // py 18570

// Completed-turn spinner suffix ("Brewed for 1m 8s") — requires a real
// duration so task-list checkmark items aren't misread as idle
// (py 18554-18561, 18639).
lazy_re!(RE_COMPLETED_TURN, r" for \d+\s*[hms]\b"); // py 18639

// Pending tool approvals shown in the status bar when bypass is off
// (py 18584).
lazy_re!(
    RE_TOOL_APPROVAL_BAR,
    r"\d+\s+(bash|tool|read|edit|write|glob|grep|notebook)"
); // py 18584

// Tool execution progress lines (py 18512, 18551).
lazy_re!(RE_READING_FILES, r"^Reading \d+ file"); // py 18512

// A turn that has YIELDED to background subagents and is blocked on them.
//
// This is a working lane that looks exactly like an idle one to every other
// signal: Claude Code draws the empty composer, and the status bar loses
// "esc to interrupt" because the main loop is not generating — it is waiting.
// Ethan, 2026-08-15: gtm-playbooks showed IDLE while its pane read "Waiting for
// 9 background agents to finish" with nine subagents at 6-8 minutes each.
//
// Matched on the PANE text rather than the status bar's "N agents · ↓ to
// manage" on purpose: this line is a positive statement that the main loop is
// blocked and it disappears when the agents land, whereas an agent COUNT can
// linger and would pin a finished lane to Active forever — the opposite error,
// and the worse one, since a lane stuck Active never gets picked up.
lazy_re!(
    RE_BG_AGENTS_WAIT,
    r"(?i)^waiting for [1-9]\d* background agents? to finish$"
);

/// Is this Claude Code's provider-owned "waiting on background agents" row?
///
/// The leading spinner glyph is the chrome anchor: accepting the sentence
/// anywhere in pane prose makes a worker discussing this detector report
/// active forever. Both the adapter and the legacy session-status path call
/// this function so the two views cannot disagree about the same frame again.
pub(crate) fn claude_background_agents_waiting(line: &str) -> bool {
    // Provider rows start at column zero. Submitted/pasted continuation lines
    // are indented by the TUI; trimming first would turn quoted frame text into
    // provider chrome.
    let line = line.trim_end();
    let Some(first) = line.chars().next() else { return false };
    // Claude's known spinner cycle. The old whole-dingbat range included `❯`,
    // the input-prompt glyph, so a user typing the exact sentence was itself
    // classified as a live agent.
    let provider_chrome = matches!(
        first,
        '*' | '\u{b7}' | '\u{2722}' | '\u{2733}' | '\u{2736}' | '\u{273b}' | '\u{273d}'
    );
    if !provider_chrome {
        return false;
    }
    RE_BG_AGENTS_WAIT.is_match(line[first.len_utf8()..].trim())
}

/// Does the newest provider-owned Claude turn marker still say the parent is
/// waiting on background agents?
///
/// A tmux capture is scrollback, not a state packet. The waiting row remains
/// visible after the agent finishes, followed by Claude's completed-turn row
/// and the final prompt. A presence-only scan therefore pins a finished lane
/// active until the old row scrolls out. Read the provider rows newest-first:
/// a later completed-turn marker is the terminal edge for every older wait,
/// while a newer wait still wins over a completion from the preceding turn.
/// Raw lines are retained so pasted/indented replicas cannot become chrome.
fn claude_background_wait_verdict(raw: &str) -> (bool, bool) {
    let lines: Vec<&str> = raw.lines().filter(|line| !line.trim().is_empty()).collect();
    for line in lines[lines.len().saturating_sub(12)..].iter().rev() {
        let line = line.trim_end();
        if claude_background_agents_waiting(line) {
            return (true, false);
        }
        let Some(first) = line.chars().next() else { continue };
        let provider_chrome = matches!(
            first,
            '*' | '\u{b7}' | '\u{2722}' | '\u{2733}' | '\u{2736}' | '\u{273b}' | '\u{273d}'
        );
        if provider_chrome && RE_COMPLETED_TURN.is_match(&line[first.len_utf8()..]) {
            let older_wait_seen = lines.iter().any(|candidate| claude_background_agents_waiting(candidate));
            return (false, older_wait_seen);
        }
    }
    (false, false)
}

pub(crate) fn claude_background_agents_working(raw: &str) -> bool {
    claude_background_wait_verdict(raw).0
}

pub(crate) fn claude_background_wait_superseded(raw: &str) -> bool {
    claude_background_wait_verdict(raw).1
}

// Bash/zsh prompt at line end — the CLI process is gone (py 8315).
lazy_re!(RE_SHELL_PROMPT_END, r"[$%]\s*$"); // py 8315

// Post-kill leak: status-bar text garbled after the prompt char, e.g.
// "mixpeek$ ss permissions on · 5 shells" (py 8317-8319).
lazy_re!(RE_SHELL_PROMPT_LEAK, r"^\S+[$%]\s"); // py 8319

// Status-bar lines that are really a shell prompt with leaked text — must
// not be read as Claude UI (py 8233-8240).
lazy_re!(RE_SHELL_BAR_SKIP, r"^.*[$%]\s"); // py 8236

// Gemini generation-in-progress markers (py 18703-18706): "esc to cancel"
// renders only DURING generation.
lazy_re!(RE_GEMINI_ACTIVE, r"(?i)esc to cancel|\(esc\s"); // py 18705

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Strip ANSI/OSC escape sequences (py 8223-8226 verbatim).
pub fn strip_ansi(s: &str) -> String {
    RE_STRIP_ANSI.replace_all(s, "").into_owned()
}

/// The provider-specific detection patterns (RR-0033 tally), so tests and
/// docs can enumerate coverage per provider. Unknown providers get an EMPTY
/// slice, not a guess: `ProviderId` is an open set (Invariant 8) and a
/// pattern table is per-provider knowledge we simply do not have — the same
/// conservative default as `ProviderCapabilities::default()`.
///
/// Note: Ollama's single entry maps to `WorkerEvent::Failed`, not
/// `RateLimited` — see `P_OLLAMA_ERROR`.
pub fn rate_limit_patterns(provider: &ProviderId) -> &'static [&'static str] {
    match provider.as_str() {
        "claude" | "claude-code" => CLAUDE_PATTERNS,
        "gemini" => GEMINI_PATTERNS,
        "codex" => CODEX_PATTERNS,
        // ollama workers now run codex --oss --local-provider ollama, so they
        // emit the same structured events as codex. Route through codex patterns.
        "ollama" => CODEX_PATTERNS,
        _ => &[],
    }
}

/// The terminal-fallback translator: captured pane text in, typed
/// [`WorkerEvent`]s out. Pure — see the module doc for the purity contract
/// and for what the caller owns (cross-scan dedup, wall-clock reset parsing).
#[derive(Debug, Clone)]
pub struct TerminalAdapter {
    provider: ProviderId,
}

impl TerminalAdapter {
    pub fn new(provider: ProviderId) -> Self {
        Self { provider }
    }

    /// Every event the capture evidences: rate limits, in-band API failures,
    /// waiting-at-prompt, a dead CLI. Repeated scans of an unchanged pane
    /// return the same events — deduplication is the caller's job.
    pub fn scan(&self, captured: &str) -> Vec<WorkerEvent> {
        let clean = strip_ansi(captured);
        if clean.trim().is_empty() {
            return Vec::new();
        }
        // Authentication failure outranks a provider's idle prompt. Codex
        // renders both at once; allowing the provider-specific scan to fall
        // through made an unusable isolated worker read IDLE for 49 minutes.
        // A person must sign in, so this is intentionally non-retryable.
        if matches!(self.provider.as_str(), "codex" | "ollama") {
            if let Some(marker) = auth_failure_state(&clean) {
                return vec![WorkerEvent::Failed(Failure {
                    reason: format!("provider authentication required: {marker}"),
                    retryable: false,
                })];
            }
        }
        match self.provider.as_str() {
            "claude" | "claude-code" => scan_claude(&clean, &self.provider),
            "gemini" => scan_gemini(&clean, &self.provider),
            "codex" => scan_codex(&clean, &self.provider),
            // ollama workers run codex --oss --local-provider ollama; same output format.
            "ollama" => scan_codex(&clean, &self.provider),
            // Unknown provider: no pattern knowledge, no invented events
            // (Invariant 8 / Invariant 20's rule generalized: never report
            // what was not observed through a pattern we actually hold).
            _ => Vec::new(),
        }
    }

    /// Does the pane show the worker actively GENERATING, by scrape?
    ///
    /// Kept SEPARATE from [`scan`](Self::scan) because "active" has no
    /// `WorkerEvent` to carry it: `WorkerState::Active` is produced only by a
    /// turn STARTING, and a turn id must be minted by the clock-holding caller
    /// — `scan` is pure (module purity contract: reads no clock, keeps no
    /// state). So the scanner reports the boolean and the caller
    /// (`orchestrator::scan`) mints the turn, exactly as it already does for a
    /// backend's native `working` report.
    ///
    /// Only codex/ollama answer true. claude and gemini report their active
    /// state through hooks / the structured protocol (the D1 exit), never the
    /// scrape — so their `scan` already, correctly, emits nothing on active and
    /// this stays false for them. A hookless codex/ollama worker has no such
    /// voice: no Stop/UserPromptSubmit hooks, no structured session, so this
    /// scrape of "• Working (…esc to interrupt)" is the ONLY signal that reaches
    /// the store that the worker is running (AMUX-3165: a working ollama lane
    /// read `running=false` the whole time because nothing emitted this).
    pub fn generating(&self, captured: &str) -> bool {
        let clean = strip_ansi(captured);
        match self.provider.as_str() {
            // ollama runs codex --oss --local-provider ollama; same pane format.
            "codex" | "ollama" => codex_generating(&clean),
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn rate_limited(
    kind: RateLimitKind,
    reset_at: Option<DateTime<Utc>>,
    provider: &ProviderId,
    raw: String,
) -> WorkerEvent {
    WorkerEvent::RateLimited(RateLimit {
        kind,
        reset_at,
        provider: provider.clone(),
        raw: Some(raw),
    })
}

/// The trimmed line containing byte offset `idx` — used to fill
/// `RateLimit.raw` with the actual banner line, not just the matched
/// fragment (ethos rule 4: keep the original provider text).
fn line_containing(text: &str, idx: usize) -> String {
    let start = text[..idx].rfind('\n').map_or(0, |i| i + 1);
    let end = text[idx..].find('\n').map_or(text.len(), |i| idx + i);
    text[start..end].trim().to_string()
}

fn join_tail(lines: &[&str], n: usize) -> String {
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn last_n_raw_lines(clean: &str, n: usize) -> String {
    let lines: Vec<&str> = clean.lines().collect();
    join_tail(&lines, n)
}

fn nonempty_trimmed(text: &str) -> Vec<&str> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).collect()
}

/// A composer/echoed-prompt line, which must never be read as UI chrome:
/// ❯ U+276F sits INSIDE the U+2700-27BF dingbat range the spinner tests use,
/// so a delivered prompt once read as an active spinner for 44 minutes
/// (py 18463-18476, sherpa incident).
fn is_prompt_line(s: &str) -> bool {
    matches!(s.chars().next(), Some('❯' | '›' | '>'))
}

fn is_dingbat_lead(s: &str) -> bool {
    matches!(s.chars().next(), Some(c) if ('\u{2700}'..='\u{27bf}').contains(&c) || c == '\u{b7}')
}

/// A provider authentication failure that leaves the CLI drawn at an input
/// prompt but unable to accept work. The leading solid square is Codex's own
/// error marker and is load-bearing: matching the sentence alone would flag a
/// worker that is merely discussing an old auth incident in its transcript.
///
/// Live specimen (amux-codex, 2026-09-03):
/// `■ Your access token could not be refreshed because you have since logged
/// out or signed in to another account. Please sign in again.`
fn auth_failure_state(clean: &str) -> Option<String> {
    nonempty_trimmed(clean).iter().rev().take(12).find_map(|line| {
        let trimmed = line.trim();
        if !trimmed.starts_with('■') {
            return None;
        }
        let low = trimmed.to_ascii_lowercase();
        let refresh_failed = low.contains("access token could not be refreshed")
            || low.contains("authentication token could not be refreshed")
            || low.contains("failed to refresh authentication token");
        let login_required = low.contains("please sign in again")
            || low.contains("please log in again")
            || low.contains("signed out")
            || low.contains("logged out");
        (refresh_failed && login_required).then(|| trimmed.to_string())
    })
}

// ---------------------------------------------------------------------------
// Structural anchors (ported from the Python's live-region logic)
// ---------------------------------------------------------------------------

/// The slice of the live screen a usage-limit banner may legitimately occupy
/// (py 6937-6969). A real banner is the last transcript content before the
/// input box; the SAME sentence lands in scrollback when a subagent dies on
/// its own limit while the main loop keeps working. Drop the input box and
/// everything under it, then keep only what follows the last activity
/// marker. A ❯ with transcript markers BELOW it is an echoed message, not
/// the input box — cutting there blinded every banner class on manual-mode
/// panes (py 6958-6963, AMUX-2111 respecimen).
fn live_limit_region(clean: &str) -> String {
    let all: Vec<&str> = clean.lines().collect();
    let start = all.len().saturating_sub(30);
    let lines = &all[start..];
    let mut input_box: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        if l.trim().starts_with('❯') {
            input_box = Some(i);
        }
    }
    if let Some(b) = input_box {
        if lines[b + 1..].iter().any(|l| RE_LIMIT_ACTIVITY.is_match(l.trim())) {
            input_box = None;
        }
    }
    let region = match input_box {
        Some(b) => &lines[..b],
        None => lines,
    };
    let mut cut: Option<usize> = None;
    for (i, l) in region.iter().enumerate() {
        if RE_LIMIT_ACTIVITY.is_match(l.trim()) {
            cut = Some(i);
        }
    }
    match cut {
        Some(c) => region[c + 1..].join("\n"),
        None => region.join("\n"),
    }
}

/// (last activity-marker line, capture-tail lines) — the shared structural
/// anchor for in-band API-error detection (py 6406-6433). The anchor is the
/// LAST activity marker above the input box: on a 5xx Claude Code prints the
/// error and STOPS, so that last marker IS the error; any output afterwards
/// makes the last marker that output instead and nothing flags — which is
/// what keeps a session merely QUOTING the string from flagging itself
/// (py 6488-6495, social-media false positive 2026-07-27).
fn api_error_region(clean: &str) -> (String, Vec<String>) {
    let all: Vec<String> = clean.lines().map(|l| l.trim_end().to_string()).collect();
    let start = all.len().saturating_sub(60);
    let lines: Vec<String> = all[start..].to_vec();
    let mut input_box: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        if l.trim().starts_with('❯') {
            input_box = Some(i);
        }
    }
    if let Some(b) = input_box {
        if lines[b + 1..].iter().any(|l| RE_LIMIT_ACTIVITY.is_match(l.trim())) {
            input_box = None; // echo, not the live box (py 6416-6427)
        }
    }
    let region = match input_box {
        Some(b) => &lines[..b],
        None => &lines[..],
    };
    let mut last_marker = String::new();
    for l in region {
        if RE_LIMIT_ACTIVITY.is_match(l.trim()) {
            last_marker = l.trim().to_string();
        }
    }
    (last_marker, lines)
}

/// UTC reset when an in-band 4xx usage-limit error is the session's CURRENT
/// state (py 6436-6459). Line breaks REMOVED, not joined: tmux hard-wraps at
/// pane width and splits mid-word; raw concat self-heals mid-word splits and
/// the `\s*` between words absorbs boundary wraps (py 6383-6387, 6445-6447).
/// Only flags when the timestamp parses — a 4xx without one stays unflagged
/// rather than badging with no target (py 6439-6441).
fn api_quota_reset(clean: &str) -> Option<(DateTime<Utc>, String)> {
    let (marker, lines) = api_error_region(clean);
    if marker.is_empty() || !RE_API_QUOTA_MARKER.is_match(&marker) {
        return None;
    }
    let flat = lines.concat();
    if !RE_API_QUOTA_PHRASE.is_match(&flat) {
        return None;
    }
    let c = RE_API_QUOTA_RESET.captures(&flat)?;
    let y: i32 = c[1].parse().ok()?;
    let mo: u32 = c[2].parse().ok()?;
    let d: u32 = c[3].parse().ok()?;
    let h: u32 = c[4].parse().ok()?;
    let mi: u32 = c[5].parse().ok()?;
    let ts = Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).single()?;
    Some((ts, marker))
}

/// The gateway's 402 budget wall as the session's CURRENT state
/// (py 6462-6472). Returns the marker line as the event's raw text.
fn api_budget_gated(clean: &str) -> Option<String> {
    let (marker, lines) = api_error_region(clean);
    if marker.is_empty() || !RE_API_BUDGET_MARKER.is_match(&marker) {
        return None;
    }
    RE_API_BUDGET_PHRASE.is_match(&lines.concat()).then_some(marker)
}

/// (code, occurrences, marker line) when a transient 5xx is the session's
/// CURRENT state (py 6475-6509). Occurrence count across the tail separates
/// one blip from a wedged retry loop (py 6508-6509).
fn api_error_state(clean: &str) -> Option<(String, usize, String)> {
    let (marker, lines) = api_error_region(clean);
    if marker.is_empty() {
        return None;
    }
    let code = RE_API_5XX_ANCHORED.captures(&marker)?[1].to_string();
    let n = RE_API_5XX_LOOSE.find_iter(&lines.join("\n")).count();
    Some((code, n, marker))
}

// ---------------------------------------------------------------------------
// Claude TUI state (py 18479-18624, minus the server-side hysteresis)
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum TuiState {
    Active,
    Waiting { reason: &'static str, detail: String },
    Idle,
    Unknown,
}

fn waiting(reason: &'static str, detail: &str) -> TuiState {
    TuiState::Waiting { reason, detail: detail.to_string() }
}

/// Port of `_detect_claude_status` (py 18479-18624). Deviation, on purpose:
/// the prompt-glyph guard is applied to BOTH dingbat tests (spinner and
/// completed-turn) — `_is_prompt_line`'s own docstring says both need it
/// (py 18466-18476), but the Python's step-2 loop (py 18548) only carries it
/// in step 0. The `$`-prompt idle branch (py 18614) is also not ported:
/// here a bare shell prompt means the process EXITED (`at_shell_prompt` ->
/// `Exited`), not that the worker is idle.
fn claude_tui_state(clean: &str) -> TuiState {
    let ne = nonempty_trimmed(clean);
    if ne.is_empty() {
        return TuiState::Unknown;
    }

    // BLOCKED ON BACKGROUND SUBAGENTS = ACTIVE, and it must be decided here,
    // because every downstream signal says idle: the composer is empty, the
    // prompt is drawn, and the status bar has dropped "esc to interrupt" since
    // the main loop is not generating. Without this, a lane orchestrating nine
    // subagents for ten minutes reports IDLE (Ethan, 2026-08-15, gtm-playbooks).
    //
    // Checked BEFORE the interrupted/permission branches only in the sense of
    // being early — those return states this must not override, so the window is
    // the recent tail rather than the whole pane, and it looks for a positive
    // "still waiting" statement rather than the mere presence of agents.
    if claude_background_agents_working(clean) {
        return TuiState::Active;
    }

    // Step 0: active spinner — highest precedence, wide window
    // (py 18498-18513): dingbat lead + ellipsis renders only while
    // generating.
    for s in ne.iter().rev().take(30) {
        if is_prompt_line(s) {
            continue;
        }
        if is_dingbat_lead(s) && s.contains('…') {
            return TuiState::Active;
        }
        if s.starts_with("Running…") || RE_READING_FILES.is_match(s) {
            return TuiState::Active;
        }
        // claude-fable-5 uses a non-dingbat prefix for its spinner
        // ("« Quantumizing…"). U+00AB/U+00BB are outside the dingbat
        // range but serve the same role.
        if s.contains('…') && matches!(s.chars().next(), Some('«' | '»')) {
            return TuiState::Active;
        }
    }

    // Step 1: status bar in the bottom 3 lines (py 18515-18522).
    let mut status_bar = String::new();
    for s in ne.iter().rev().take(3) {
        let low = s.to_lowercase();
        if s.contains("⏵⏵") || low.contains("bypass permissions") || low.contains("plan mode") {
            status_bar = low;
            break;
        }
    }
    if status_bar.is_empty() {
        // Compaction dialog (py 18530-18532, also 8296-8304).
        if clean.contains("Resume from summary") && clean.contains("Resume full session") {
            return waiting("user_input", "resume-from-summary dialog");
        }
        // Background-tasks mode replaces the status bar with
        // "esc to interrupt · ctrl+t …" (py 18533-18538).
        for s in ne.iter().rev().take(5) {
            if s.to_lowercase().contains("esc to interrupt") {
                return TuiState::Active;
            }
        }
    }

    // Step 2: last 12 lines bottom-up for the most recent signal
    // (py 18540-18578).
    for s in ne.iter().rev().take(12) {
        let sl = s.to_lowercase();
        if !is_prompt_line(s) && is_dingbat_lead(s) {
            if s.contains('…') {
                return TuiState::Active;
            }
            if RE_COMPLETED_TURN.is_match(s) {
                return TuiState::Idle; // "✻ Brewed for 1m 8s" (py 18559-18561)
            }
        }
        if s.starts_with("Running…") || RE_READING_FILES.is_match(s) {
            return TuiState::Active;
        }
        if sl.contains("enter to select") {
            return waiting("user_input", s); // multi-choice (py 18565)
        }
        if sl.contains("do you want to proceed") {
            return waiting("permission_prompt", s); // py 18568
        }
        if RE_SELECTOR.is_match(s) {
            return waiting("user_input", s); // "❯ 1. Yes" (py 18570)
        }
        // Post-interrupt prompt is a fully usable prompt, not a question
        // (py 18572-18578).
        if sl.contains("interrupted") && sl.contains("what should claude do") {
            return TuiState::Idle;
        }
    }

    // Step 3: status bar secondary checks (py 18580-18589).
    if !status_bar.is_empty() {
        // "esc to interrupt" on the status bar means a turn is actively
        // running. This is checked BEFORE the bypass_on fallthrough to Idle
        // (the bar can contain both "bypass permissions on" AND "esc to
        // interrupt" simultaneously). Claude Code always draws the empty
        // composer (❯) during generation, so the prompt is NOT a
        // discriminator — use the hook report (hook_confirmed_idle in
        // send_text_inner) for that distinction.
        if status_bar.contains("esc to interrupt") {
            return TuiState::Active;
        }
        let bypass_on = status_bar.contains("bypass permissions on");
        if !bypass_on && RE_TOOL_APPROVAL_BAR.is_match(&status_bar) {
            return waiting("permission_prompt", &status_bar);
        }
        if status_bar.contains("approve") {
            return waiting("permission_prompt", &status_bar);
        }
        return TuiState::Idle; // bar visible, no signal -> idle at prompt
    }

    // Step 5 fallback: prompt characters (py 18608-18623).
    for s in ne.iter().rev().take(5) {
        if *s == "❯" || s.ends_with('❯') {
            return TuiState::Idle;
        }
    }
    // Empty-composer hint bar — unambiguous idle (py 18616-18623, gainz
    // incident 2026-07-20).
    for s in ne.iter().rev().take(4) {
        if s.to_lowercase().contains("? for shortcuts") {
            return TuiState::Idle;
        }
    }
    TuiState::Unknown
}

/// Claude's status bar / spinner present in the frame — the guard that keeps
/// a shell-prompt check from firing on a live UI (py 8229-8250; only the
/// Claude arm is ported because `at_shell_prompt` is only consulted for
/// Claude panes, matching where the Python applies it).
fn claude_ui_visible(clean: &str) -> bool {
    let ne = nonempty_trimmed(clean);
    for l in ne.iter().rev().take(3) {
        let low = l.to_lowercase();
        // Post-kill leak: status-bar text after a shell prompt char is not
        // a live bar (py 8233-8240).
        if RE_SHELL_BAR_SKIP.is_match(&low) {
            continue;
        }
        if l.contains("⏵⏵") || low.contains("bypass permissions") || low.contains("plan mode") {
            return true;
        }
    }
    for l in ne.iter().rev().take(12) {
        if !is_prompt_line(l) && is_dingbat_lead(l) {
            return true; // dingbat-prefixed status line (py 8245-8250)
        }
    }
    false
}

/// Bare shell prompt, no Claude UI: the CLI process is gone (py 8307-8321).
fn at_shell_prompt(clean: &str) -> bool {
    if claude_ui_visible(clean) {
        return false;
    }
    let ne = nonempty_trimmed(clean);
    for l in ne.iter().rev().take(5) {
        if l.contains('❯') {
            continue;
        }
        if RE_SHELL_PROMPT_END.is_match(l) || RE_SHELL_PROMPT_LEAK.is_match(l) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Per-provider scans
// ---------------------------------------------------------------------------

fn scan_claude(clean: &str, provider: &ProviderId) -> Vec<WorkerEvent> {
    let mut events = Vec::new();
    let tail12 = last_n_raw_lines(clean, 12);
    let tail30 = last_n_raw_lines(clean, 30);
    let state = claude_tui_state(clean);

    // Activity veto (py 7117-7154): an actively-generating session is, by
    // definition, not rate-limited — but a live limit banner/menu in the
    // tail VETOES the veto, because "waiting for limit to reset" renders a
    // spinner (py 7131-7140, mixpeek-autopilot flap 2026-08-08).
    let activity_evident = matches!(state, TuiState::Active)
        || RE_TOKENS_SUFFIX.is_match(&tail12)
        || tail12.lines().any(|l| RE_LIMIT_ACTIVITY.is_match(l.trim()));
    let limit_ui_live = RE_CLAUDE_SESSION.is_match(&tail12)
        || RE_CLAUDE_WEEKLY.is_match(&tail12)
        || RE_CLAUDE_MENU.is_match(&tail12);

    let mut menu_hit = false;
    if !activity_evident || limit_ui_live {
        // The interactive menu renders live at the bottom, so match the tail
        // only — the full capture false-matches stale scrollback and once
        // pressed "1" into a healthy session (py 7155-7163). Kind: the menu
        // appears when a subscription usage cap is hit (py 6240-6241).
        if let Some(m) = RE_CLAUDE_MENU.find(&tail30) {
            menu_hit = true;
            events.push(rate_limited(
                RateLimitKind::SubscriptionCap,
                None,
                provider,
                line_containing(&tail30, m.start()),
            ));
        } else {
            let live = live_limit_region(clean);
            if let Some(m) = RE_CLAUDE_WEEKLY.find(&live) {
                // py 7198-7207
                events.push(rate_limited(
                    RateLimitKind::Weekly,
                    None,
                    provider,
                    line_containing(&live, m.start()),
                ));
            } else if let Some(m) = RE_CLAUDE_SESSION.find(&live) {
                // The 5-hour subscription window (py 7208-7212, 6313-6318).
                events.push(rate_limited(
                    RateLimitKind::SubscriptionCap,
                    None,
                    provider,
                    line_containing(&live, m.start()),
                ));
            } else {
                // Credit gates (py 7213-7296). RateLimitKind has no Credit
                // cell, and credits are NOT the subscription ("purchased
                // separately from your plan", py 6346) nor any time window —
                // so `Unknown` is the honest cell (its own doc: a
                // first-class honest answer), with `raw` carrying the banner
                // as the discriminator. Reported upstream as a protocol.rs
                // gap rather than papered over with a wrong cell (ethos
                // rule 3).
                let live_ne = nonempty_trimmed(&live);
                let last5 = join_tail(&live_ne, 5);
                let last6 = join_tail(&live_ne, 6);
                // Banner: anchor + corroborating limit statement, final few
                // lines of the live region only (py 7259-7271).
                let banner_gate =
                    RE_CREDIT_ANCHOR.is_match(&last5) && RE_CREDIT_CTX.is_match(&last5);
                // Menu: the option line must be the region's LAST line —
                // position, not just phrasing, makes the render real
                // (py 7213-7258).
                let menu_gate = live_ne.last().is_some_and(|l| RE_CREDIT_MENU_OPT.is_match(l))
                    && RE_CREDIT_MENU_PROSE.is_match(&last6);
                // Spend menu: clean-screen tail, since the live region cuts
                // AT its highlighted row (py 7272-7287).
                let tail8 = join_tail(&nonempty_trimmed(clean), 8);
                let spend_gate = RE_SPEND_OPT.is_match(&tail8)
                    && (RE_SPEND_WAIT.is_match(&tail8) || RE_SPEND_BAL.is_match(&tail8));
                if banner_gate || menu_gate || spend_gate {
                    let raw = if banner_gate {
                        last5
                    } else if menu_gate {
                        last6
                    } else {
                        tail8
                    };
                    // Credit exhaustion clears on payment, not a clock
                    // (RateLimitKind::Credit, AF-14).
                    events.push(rate_limited(RateLimitKind::Credit, None, provider, raw));
                }
            }
        }
    }

    // In-band API-error classes. Deliberately OUTSIDE the activity veto: the
    // structural anchor (last activity marker above the input box) already
    // guarantees a working session cannot flag — its last marker is its own
    // newest output (py 6488-6495) — whereas the Python's tail-12 veto can
    // suppress these very detections when the ⏺-prefixed error line itself
    // counts as "activity". The anchor discriminates; the veto only guesses.
    if !menu_hit {
        if let Some(marker) = api_budget_gated(clean) {
            // 402: credit-path semantics, no reset (py 7288-7296).
            events.push(rate_limited(RateLimitKind::Credit, None, provider, marker));
        }
    }
    if let Some((ts, marker)) = api_quota_reset(clean) {
        // 4xx quota: hard gate WITH a machine-readable UTC reset —
        // auto-resume genuinely works once the key regains access
        // (py 6375-6382, 7334-7356). Kind Unknown: the provider states a
        // reset instant, not which quota window tripped (Invariant 20:
        // never invent what was not reported).
        events.push(rate_limited(RateLimitKind::Unknown, Some(ts), provider, marker));
    }
    if let Some((code, n, marker)) = api_error_state(clean) {
        // 5xx: server-side and immediately retryable — the correct bulk
        // action really is "send continue" (py 6359-6369, 7311-7333).
        events.push(WorkerEvent::Failed(Failure {
            reason: format!(
                "API Error: {code} — transient server error, {n} occurrence(s) in capture: {marker}"
            ),
            retryable: true,
        }));
    }

    match state {
        TuiState::Waiting { reason, detail } => events.push(WorkerEvent::Waiting(WaitReason {
            reason: reason.to_string(),
            detail: Some(detail),
        })),
        TuiState::Idle => events.push(WorkerEvent::Waiting(WaitReason {
            reason: "idle_prompt".to_string(),
            detail: None,
        })),
        TuiState::Active | TuiState::Unknown => {}
    }

    // Dead CLI: bare shell prompt, no Claude UI, nothing else explaining the
    // frame. Both exit fields None — the shell prompt evidences THAT the
    // process is gone, not how (Invariant 20: never invent a code that was
    // not reported).
    if events.is_empty() && at_shell_prompt(clean) {
        events.push(WorkerEvent::Exited(ExitStatus { code: None, signal: None }));
    }

    events
}

fn scan_gemini(clean: &str, provider: &ProviderId) -> Vec<WorkerEvent> {
    let mut events = Vec::new();
    let tail30 = last_n_raw_lines(clean, 30);
    // Quota banners (py 6303-6306). "daily quota" is the one place the
    // Python's tables name a daily window, so it maps to Daily; the generic
    // "Quota exceeded" form does not say which window -> Unknown.
    if let Some(m) = RE_GEMINI_QUOTA_REACHED.find(&tail30) {
        let kind = if m.as_str().to_lowercase().contains("daily") {
            RateLimitKind::Daily
        } else {
            RateLimitKind::Unknown
        };
        events.push(rate_limited(kind, None, provider, m.as_str().trim().to_string()));
    } else if let Some(m) = RE_GEMINI_QUOTA_EXCEEDED.find(&tail30) {
        events.push(rate_limited(
            RateLimitKind::Unknown,
            None,
            provider,
            m.as_str().trim().to_string(),
        ));
    }
    // Status (py 18694-18709): "esc to cancel" renders only DURING
    // generation; "Type your message" only at an empty, resting input.
    let mut start = clean.len().saturating_sub(2500);
    while !clean.is_char_boundary(start) {
        start += 1;
    }
    let tail = &clean[start..];
    if RE_GEMINI_ACTIVE.is_match(tail) {
        return events; // active -> nothing further to report
    }
    if tail.contains("Type your message") {
        events.push(WorkerEvent::Waiting(WaitReason {
            reason: "idle_prompt".to_string(),
            detail: Some("Type your message".to_string()),
        }));
    }
    events
}

/// The codex/ollama "generating" pane: a bullet-led status line carrying
/// "working" / "running" / "esc to interrupt" (py 18591-18599). Pure — the
/// single source of truth for the pattern, shared by `scan_codex` (to
/// short-circuit idle/trust) and [`TerminalAdapter::generating`] (which the
/// clock-holding caller uses to mint the turn that makes the worker Active).
fn codex_model_bar(line: &str) -> bool {
    let Some((identity, location)) = line.rsplit_once('\u{b7}') else {
        return false;
    };
    let location = location.trim();
    let identity_parts = identity.split_whitespace().count();
    identity_parts >= 2
        && (location == "~" || location.starts_with("~/") || location.starts_with('/'))
}

fn codex_active_status_line(line: &str) -> bool {
    let low = line.trim().to_ascii_lowercase();
    low.starts_with("• working (")
        || low.starts_with("• running (")
        || low.starts_with("• waiting for background terminal (")
        || low.starts_with("• waiting for background command (")
}

fn codex_generation_state_clean(clean: &str) -> Option<bool> {
    // Provider-known Codex scanning also receives short/partial captures from
    // native adapters where the footer is outside the window. It may use the
    // active row alone because the caller has already established provider
    // identity; the provider-agnostic compatibility path below may not.
    let lines = nonempty_trimmed(clean);
    let start = lines.len().saturating_sub(12);
    let tail = &lines[start..];
    if let Some(prompt_i) = tail.iter().rposition(|s| is_prompt_line(s)) {
        let active_before_prompt = prompt_i > 0 && codex_active_status_line(tail[prompt_i - 1]);
        let active_after_prompt = tail[prompt_i + 1..]
            .iter()
            .any(|s| codex_active_status_line(s));
        return Some(active_before_prompt || active_after_prompt);
    }
    for s in tail.iter().rev() {
        if codex_active_status_line(s) {
            return Some(true);
        }
        if codex_model_bar(s) {
            return Some(false);
        }
    }
    None
}

/// Parse the current Codex footer and return its structurally attached active
/// row. `Some(None)` is an identifiable idle Codex frame; `None` is not a
/// current Codex frame at all. Keeping that distinction lets consumers ask a
/// narrower question than generic generation without reimplementing the frame
/// anchors.
fn codex_structured_active_line_clean(clean: &str) -> Option<Option<&str>> {
    // The Codex prompt shell and model bar remain painted while a turn is
    // running. The decisive shape is therefore the line immediately ABOVE the
    // prompt: `• Working (…)` / `• Waiting for background terminal (…)` means
    // the prompt is disabled and the turn is live; completed turns replace it
    // with `Worked for …` or ordinary output. ATE-36 incorrectly made the mere
    // presence of the prompt shell an idle boundary, which hid live ATE-37.
    let lines = nonempty_trimmed(clean);
    let start = lines.len().saturating_sub(12);
    let tail = &lines[start..];
    // Provider identity must be the CURRENT footer, not a Codex-looking frame
    // pasted into another provider's prompt. The model/path bar is Codex's
    // final non-empty row and its prompt glyph is `›` (not Claude's `❯`).
    let model_i = tail.len().checked_sub(1).filter(|i| codex_model_bar(tail[*i]))?;
    let prompt_i = tail[..model_i].iter().rposition(|s| s.starts_with('›'))?;
    // Current Codex paints the active row immediately before its disabled
    // prompt. Older builds painted it immediately after the submitted prompt,
    // directly before the model bar. No arbitrary line between these anchors
    // may vote active: that was the pasted-frame false positive.
    if prompt_i > 0 && codex_active_status_line(tail[prompt_i - 1]) {
        return Some(Some(tail[prompt_i - 1]));
    }
    if prompt_i + 1 == model_i.saturating_sub(1)
        && codex_active_status_line(tail[prompt_i + 1])
    {
        return Some(Some(tail[prompt_i + 1]));
    }
    Some(None)
}

fn codex_structured_generation_state_clean(clean: &str) -> Option<bool> {
    codex_structured_active_line_clean(clean).map(|line| line.is_some())
}

fn codex_background_status_line(line: &str) -> bool {
    let low = line.trim().to_ascii_lowercase();
    low.starts_with("• waiting for background terminal (")
        || low.contains(" background terminal running")
        || low.contains(" background terminals running")
}

fn codex_generating(clean: &str) -> bool {
    codex_generation_state_clean(clean).unwrap_or(false)
}

/// The newest generation boundary when the capture is structurally
/// identifiable as a Codex-family TUI, or `None` when another provider (or an
/// unknown terminal) should keep using its own signals.
///
/// The sessions compatibility projection does not carry provider metadata,
/// but it does have the raw pane. Returning an option lets it share Codex's
/// ordered boundary logic without pretending every bullet containing
/// "working" belongs to Codex. The prompt glyph and model/path bar are stable
/// TUI structure; model and effort names stay intentionally open-ended.
pub(crate) fn codex_pane_generation_state(captured: &str) -> Option<bool> {
    let clean = strip_ansi(captured);
    codex_structured_generation_state_clean(&clean)
}

/// Is the structurally current Codex status row specifically waiting on live
/// background work? Generic foreground generation remains subject to the
/// normal steering max-age policy; only this provider-owned state is a hard
/// hold until its terminal row clears.
pub(crate) fn codex_pane_background_working(captured: &str) -> bool {
    let clean = strip_ansi(captured);
    codex_structured_active_line_clean(&clean)
        .flatten()
        .is_some_and(codex_background_status_line)
}

fn scan_codex(clean: &str, provider: &ProviderId) -> Vec<WorkerEvent> {
    let mut events = Vec::new();
    let tail30 = last_n_raw_lines(clean, 30);
    if let Some(m) = RE_CODEX_USAGE_LIMIT.find(&tail30) {
        // Credit-path semantics: no reset lifted, no auto-resume — lifting
        // "8:00 PM" from the wrong provider's banner is the AMUX-2088 bug
        // (py 7230-7254). Kind Unknown: the banner does not say which
        // window tripped.
        events.push(rate_limited(
            RateLimitKind::Unknown,
            None,
            provider,
            m.as_str().trim().to_string(),
        ));
    }
    // Active: "• Working (Xs • esc to interrupt)" (py 18591-18599). The active
    // STATE is applied by the caller via `TerminalAdapter::generating` (Active
    // needs a turn id, which the pure scanner cannot mint) — here it only
    // short-circuits the idle/trust checks below so a generating pane is never
    // misread as idle.
    if codex_generating(clean) {
        return events;
    }
    // Trust-directory picker: codex shows this at startup BEFORE the composer.
    // It blocks message delivery until the user selects "1. Yes, continue".
    // Detect by the question text or the selected option bullet so the delivery
    // path can see the block (AMUX-3159; auto-dismiss is a separate action there).
    let tail8 = last_n_raw_lines(clean, 8);
    if tail8.contains("Do you trust the contents of this directory")
        || tail8.contains("1. Yes, continue")
    {
        events.push(WorkerEvent::Waiting(WaitReason {
            reason: "trust_prompt".to_string(),
            detail: Some("codex directory trust dialog".to_string()),
        }));
        return events;
    }

    // Idle: › prompt or the model status line "gpt-5.5 xhigh · ~/path".
    // The model bar format for ALL providers is "<model> <effort> · <path>";
    // `codex_model_bar` deliberately does not enumerate model or effort names,
    // so a stronger future model does not fall outside the harness.
    let ne = nonempty_trimmed(clean);
    for s in ne.iter().rev().take(5) {
        let sl = s.to_lowercase();
        let prompt = s.starts_with('›') && (sl.contains("implement") || *s == "›");
        if codex_model_bar(s) || prompt {
            events.push(WorkerEvent::Waiting(WaitReason {
                reason: "idle_prompt".to_string(),
                detail: Some(s.to_string()),
            }));
            break;
        }
    }
    events
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(p: &str) -> TerminalAdapter {
        TerminalAdapter::new(ProviderId::new(p))
    }

    fn limits(events: &[WorkerEvent]) -> Vec<&RateLimit> {
        events
            .iter()
            .filter_map(|e| match e {
                WorkerEvent::RateLimited(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    fn waiting_reasons(events: &[WorkerEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                WorkerEvent::Waiting(w) => Some(w.reason.as_str()),
                _ => None,
            })
            .collect()
    }

    fn failures(events: &[WorkerEvent]) -> Vec<&Failure> {
        events
            .iter()
            .filter_map(|e| match e {
                WorkerEvent::Failed(f) => Some(f),
                _ => None,
            })
            .collect()
    }

    // -- fixtures (realistic captured frames; strings copied from the Python
    // patterns' own comments where they quote live renders) ----------------

    const FX_MENU: &str = "\
You've reached your usage limit.

1. Stop and wait for limit to reset
2. Add more funds to continue now
3. Upgrade your plan

Enter to confirm";

    // Banner text from py 6296.
    const FX_WEEKLY: &str = "\
⏺ Working through the queue.

You've hit your weekly limit · resets Jun 26 at 5am (America/New_York)";

    // Banner text from py 6314-6315.
    const FX_SESSION: &str = "\
⏺ Working through the queue.

You've hit your session limit · resets 9:50am (America/New_York)";

    // Banner text from py 6325-6327. Layout matters: in a real capture the
    // previous ⏺ transcript marker sits well above the tail (panes are
    // captured 300 lines deep with blank padding), so it must be OUTSIDE the
    // last-12-line activity-veto window — a marker adjacent to the banner
    // would veto detection in the Python too (py 7118-7154).
    const FX_CREDIT_BANNER: &str = "\
⏺ Attempting the next step.










You've reached your Fable 5 limit. Run /usage-credits to continue or switch
models with /model.

❯
? for shortcuts";

    // Menu render quoted verbatim at py 6344-6350.
    const FX_CREDIT_MENU: &str = "\
Fable 5 now uses usage credits

Fable 5 runs on usage credits, purchased separately from your plan.
You don't have usage credits yet.

  1. Set up usage credits on claude.ai
❯ 2. Switch to Opus 5 (1M context) and continue

Enter to confirm · Esc to cancel";

    // Menu shape from py 6521-6523.
    const FX_SPEND_MENU: &str = "\
What do you want to do?
Usage credit balance: $12.50

❯ Adjust monthly spend limit: $50
  Wait for limit to reset  Resets 1pm (America/New_York)";

    const FX_API_529: &str = "\
⏺ Reading the failing test

⏺ API Error: 529 Overloaded

⏺ API Error: 529 Overloaded

❯
⏵⏵ bypass permissions on · esc to interrupt · 2 shells";

    // Error text from py 6376-6378 (AMUX-2111 specimen).
    const FX_API_400_QUOTA: &str = "\
⏺ ping

⏺ API Error: 400 You have reached your specified API usage limits. You will regain access on 2026-08-01 at 00:00 UTC.

❯ ";

    // The same specimen hard-wrapped mid-word, as tmux renders it
    // ("You will rega/in access", py 6384-6385).
    const FX_API_400_QUOTA_WRAPPED: &str = "\
⏺ ping

⏺ API Error: 400 You have reached your specified API
usage limits. You will rega
in access on 2026-08-01 at 00:00 UTC.

❯ ";

    // Error text from py 6395-6397 (AMUX-2113).
    const FX_API_402_BUDGET: &str = "\
⏺ API Error: 402 amux trial budget exhausted ($25.03 of $25.00). Upgrade at
https://cloud.amux.io/billing

❯ ";

    const FX_PERMISSION_PROMPT: &str = "\
⏺ Edit(src/main.rs)

Do you want to proceed?";

    const FX_SELECTOR_PROMPT: &str = "\
⏺ Bash(rm -rf build)

Do you want to proceed?
❯ 1. Yes
  2. No, and tell Claude what to do differently";

    const FX_IDLE_PROMPT: &str = "\
⏺ Done. All tests green.

❯
? for shortcuts";

    // Banner ABOVE fresh activity + a live spinner: stale scrollback, not a
    // live limit (the case py 6940-6951 exists for).
    const FX_ACTIVE_WITH_STALE_BANNER: &str = "\
You've hit your weekly limit · resets Jun 26 at 5am (America/New_York)
⏺ Retrying the fetch now
⏺ Wrote 3 files
✻ Beaming… (12s · ↑ 1.2k tokens)";

    // A session DISCUSSING the patterns — the quoting hazard the two-scan
    // gate and live-region anchors exist for (py 7297-7302).
    const FX_QUOTED_BANNER: &str = "\
⏺ The weekly banner reads \"You've hit your weekly limit · resets Jun 26 at 5am\".
⏺ Detection anchors on /usage-credits and the reset phrase.
❯ ";

    const FX_SHELL_PROMPT: &str = "\
Saved session state.
[1]  + 84737 killed     claude --dangerously-skip-permissions
mixpeek$ ";

    const FX_ORDINARY: &str = "\
⏺ Compiling amux-server v0.1.0
⏺ Finished dev profile target(s) in 12.4s
⏺ All 34 tests passed; no warnings.";

    const FX_GEMINI_QUOTA: &str =
        "✦ Quota exceeded for quota metric 'Requests' of Gemini 2.5 Pro";
    const FX_GEMINI_DAILY: &str =
        "✦ You have reached your daily quota limit for Gemini 2.5 Pro.";
    const FX_GEMINI_IDLE: &str = "\
│ Type your message
gemini-2.5-pro";
    const FX_GEMINI_ACTIVE: &str = "\
✦ Thinking about the request (esc to cancel)";

    // Banner shape from the AMUX-2088 incident (py 6296-6300).
    const FX_CODEX_LIMIT: &str = "\
■ You've hit your usage limit. Upgrade to Pro (https://openai.com/chatgpt/pricing) or try again in 4 days 2 hours.";
    const FX_CODEX_IDLE: &str = "\
› implement {feature}
  gpt-5.5 xhigh · ~/Dev/amux";
    const FX_CODEX_AUTH: &str = "\
› [09:04 AM] Reply only with isolated-ok.

■ Your access token could not be refreshed because you have since logged out or signed in to another account. Please sign in again.

› Ask Codex to do anything
  gpt-5.5 xhigh · ~/Dev/amux";

    const FX_OLLAMA_NO_DAEMON: &str = "Error: could not connect to ollama app, is it running?";
    const FX_OLLAMA_NO_MODEL: &str = "Error: model 'llama3:70b' not found, try pulling it first";

    // -- ANSI stripping ------------------------------------------------------

    #[test]
    fn strip_ansi_removes_real_escape_sequences() {
        // Colors, bold, cursor moves, clears, hidden cursor, OSC title,
        // OSC-8 hyperlink, charset selection.
        let raw = "\x1b[2J\x1b[H\x1b[?25l\x1b[1;32m✻ Beaming…\x1b[0m \x1b[31mred\x1b[0m\
                   \x1b]0;my title\x07\x1b[10;5Hplain\
                   \x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\\x1b(Bdone";
        let out = strip_ansi(raw);
        assert_eq!(out, "✻ Beaming… redplainlinkdone");
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn strip_ansi_leaves_plain_text_alone() {
        let plain = "❯ ordinary text · with unicode ⏺ and $dollars";
        assert_eq!(strip_ansi(plain), plain);
    }

    // -- pattern enumeration -------------------------------------------------

    #[test]
    fn pattern_counts_per_provider() {
        assert_eq!(rate_limit_patterns(&ProviderId::new("claude-code")).len(), 16);
        assert_eq!(rate_limit_patterns(&ProviderId::new("claude")).len(), 16);
        assert_eq!(rate_limit_patterns(&ProviderId::new("gemini")).len(), 2);
        assert_eq!(rate_limit_patterns(&ProviderId::new("codex")).len(), 1);
        assert_eq!(rate_limit_patterns(&ProviderId::new("ollama")).len(), 1);
        assert!(rate_limit_patterns(&ProviderId::new("a-provider-from-2031")).is_empty());
    }

    #[test]
    fn every_pattern_compiles_and_has_a_positive_fixture() {
        // Anchored patterns (^...) match a line, not the whole frame, so a
        // fixture satisfies a pattern if the frame OR any trimmed line does.
        let corpus: &[&str] = &[
            FX_MENU,
            FX_WEEKLY,
            FX_SESSION,
            FX_CREDIT_BANNER,
            FX_CREDIT_MENU,
            FX_SPEND_MENU,
            FX_API_529,
            FX_API_400_QUOTA,
            FX_API_402_BUDGET,
            FX_GEMINI_QUOTA,
            FX_GEMINI_DAILY,
            FX_CODEX_LIMIT,
            FX_OLLAMA_NO_DAEMON,
            FX_OLLAMA_NO_MODEL,
        ];
        for provider in ["claude-code", "gemini", "codex", "ollama"] {
            for pat in rate_limit_patterns(&ProviderId::new(provider)) {
                let re = Regex::new(pat).unwrap_or_else(|e| panic!("{pat} failed to compile: {e}"));
                let hit = corpus.iter().any(|fx| {
                    re.is_match(fx) || fx.lines().any(|l| re.is_match(l.trim()))
                });
                assert!(hit, "pattern has no positive fixture: {pat}");
            }
        }
    }

    // -- Claude: rate-limit classes -----------------------------------------

    #[test]
    fn claude_menu_is_subscription_cap() {
        let ev = adapter("claude-code").scan(FX_MENU);
        let rl = limits(&ev);
        assert_eq!(rl.len(), 1, "{ev:?}");
        assert_eq!(rl[0].kind, RateLimitKind::SubscriptionCap);
        assert_eq!(rl[0].reset_at, None);
        assert!(rl[0].raw.as_deref().unwrap().contains("Stop and wait"));
    }

    #[test]
    fn claude_weekly_banner_is_weekly() {
        let ev = adapter("claude-code").scan(FX_WEEKLY);
        let rl = limits(&ev);
        assert_eq!(rl.len(), 1, "{ev:?}");
        assert_eq!(rl[0].kind, RateLimitKind::Weekly);
        assert!(rl[0].raw.as_deref().unwrap().contains("weekly limit"));
    }

    #[test]
    fn claude_session_banner_is_subscription_cap() {
        let ev = adapter("claude-code").scan(FX_SESSION);
        let rl = limits(&ev);
        assert_eq!(rl.len(), 1, "{ev:?}");
        assert_eq!(rl[0].kind, RateLimitKind::SubscriptionCap);
        assert!(rl[0].raw.as_deref().unwrap().contains("session limit"));
    }

    #[test]
    fn claude_credit_banner_is_credit_kind_with_raw() {
        let ev = adapter("claude-code").scan(FX_CREDIT_BANNER);
        let rl = limits(&ev);
        assert_eq!(rl.len(), 1, "{ev:?}");
        assert_eq!(rl[0].kind, RateLimitKind::Credit);
        assert_eq!(rl[0].reset_at, None);
        assert!(rl[0].raw.as_deref().unwrap().contains("/usage-credits"));
    }

    #[test]
    fn claude_credit_menu_flags_and_awaits_selection() {
        let ev = adapter("claude-code").scan(FX_CREDIT_MENU);
        let rl = limits(&ev);
        assert_eq!(rl.len(), 1, "{ev:?}");
        assert_eq!(rl[0].kind, RateLimitKind::Credit);
        // The ❯-selected option row is also a live question.
        assert!(waiting_reasons(&ev).contains(&"user_input"), "{ev:?}");
    }

    #[test]
    fn claude_spend_menu_flags_without_auto_answer_semantics() {
        let ev = adapter("claude-code").scan(FX_SPEND_MENU);
        let rl = limits(&ev);
        assert_eq!(rl.len(), 1, "{ev:?}");
        assert_eq!(rl[0].kind, RateLimitKind::Credit);
        assert!(rl[0].raw.as_deref().unwrap().contains("Adjust monthly spend limit"));
    }

    #[test]
    fn kinds_discriminate_daily_weekly_credit() {
        // The task's cross-check: daily (gemini) vs weekly (claude) vs
        // credit (claude) land in three DIFFERENT cells.
        let daily = limits(&adapter("gemini").scan(FX_GEMINI_DAILY))[0].kind;
        let weekly = limits(&adapter("claude-code").scan(FX_WEEKLY))[0].kind;
        let credit = limits(&adapter("claude-code").scan(FX_CREDIT_BANNER))[0].kind;
        assert_eq!(daily, RateLimitKind::Daily);
        assert_eq!(weekly, RateLimitKind::Weekly);
        assert_eq!(credit, RateLimitKind::Credit);
        assert_ne!(daily, weekly);
        assert_ne!(weekly, credit);
        assert_ne!(daily, credit);
    }

    // -- Claude: in-band API errors -----------------------------------------

    #[test]
    fn claude_5xx_is_retryable_failure_with_count() {
        let ev = adapter("claude-code").scan(FX_API_529);
        let fs = failures(&ev);
        assert_eq!(fs.len(), 1, "{ev:?}");
        assert!(fs[0].retryable, "5xx is immediately retryable (py 6367-6369)");
        assert!(fs[0].reason.contains("529"));
        assert!(fs[0].reason.contains("2 occurrence(s)"), "{}", fs[0].reason);
        assert!(limits(&ev).is_empty(), "a 5xx is a failure, not a rate limit");
    }

    #[test]
    fn claude_4xx_quota_parses_utc_reset() {
        for fx in [FX_API_400_QUOTA, FX_API_400_QUOTA_WRAPPED] {
            let ev = adapter("claude-code").scan(fx);
            let rl = limits(&ev);
            assert_eq!(rl.len(), 1, "{ev:?}");
            assert_eq!(
                rl[0].reset_at,
                Some(Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap()),
                "wrap-healed reset parse (py 6383-6394)"
            );
            assert_eq!(rl[0].kind, RateLimitKind::Unknown);
        }
    }

    #[test]
    fn claude_402_budget_is_limit_without_reset() {
        let ev = adapter("claude-code").scan(FX_API_402_BUDGET);
        let rl = limits(&ev);
        assert_eq!(rl.len(), 1, "{ev:?}");
        assert_eq!(rl[0].kind, RateLimitKind::Credit);
        assert_eq!(rl[0].reset_at, None, "402 carries no reset (py 6395-6399)");
        assert!(rl[0].raw.as_deref().unwrap().contains("402"));
        assert!(failures(&ev).is_empty(), "402 is a gate, not a transient failure");
    }

    // -- Claude: waiting / idle / crash -------------------------------------

    #[test]
    fn claude_permission_prompt_emits_waiting() {
        let ev = adapter("claude-code").scan(FX_PERMISSION_PROMPT);
        assert_eq!(waiting_reasons(&ev), vec!["permission_prompt"], "{ev:?}");
        assert!(limits(&ev).is_empty());
    }

    #[test]
    fn claude_selector_prompt_emits_waiting() {
        let ev = adapter("claude-code").scan(FX_SELECTOR_PROMPT);
        assert_eq!(waiting_reasons(&ev), vec!["user_input"], "{ev:?}");
    }

    #[test]
    fn claude_idle_prompt_emits_waiting_idle() {
        let ev = adapter("claude-code").scan(FX_IDLE_PROMPT);
        assert_eq!(waiting_reasons(&ev), vec!["idle_prompt"], "{ev:?}");
        assert!(limits(&ev).is_empty());
    }

    #[test]
    fn claude_shell_prompt_emits_exited_with_no_invented_code() {
        let ev = adapter("claude-code").scan(FX_SHELL_PROMPT);
        assert_eq!(
            ev,
            vec![WorkerEvent::Exited(ExitStatus { code: None, signal: None })],
            "Invariant 20: the shell prompt proves the process is gone, not how"
        );
    }

    // -- false-positive suite (the checks that must be able to fail) --------

    #[test]
    fn active_session_with_stale_banner_reports_nothing() {
        // Output BELOW the banner + a live spinner: the banner is history
        // (py 6940-6951, 7117-7154).
        let ev = adapter("claude-code").scan(FX_ACTIVE_WITH_STALE_BANNER);
        assert!(ev.is_empty(), "{ev:?}");
    }

    #[test]
    fn quoted_banner_in_transcript_is_not_a_limit() {
        // A session discussing the patterns must not flag itself
        // (py 6488-6495, 7259-7268; social-media 2026-07-27).
        let ev = adapter("claude-code").scan(FX_QUOTED_BANNER);
        assert!(limits(&ev).is_empty(), "{ev:?}");
        assert!(failures(&ev).is_empty(), "{ev:?}");
        // The idle prompt itself is real and may be reported.
    }

    #[test]
    fn ordinary_output_matches_no_pattern_for_any_provider() {
        for p in ["claude-code", "gemini", "codex", "ollama"] {
            let ev = adapter(p).scan(FX_ORDINARY);
            assert!(ev.is_empty(), "provider {p}: {ev:?}");
        }
    }

    #[test]
    fn unknown_provider_scans_empty_even_on_matching_text() {
        for frame in [FX_WEEKLY, FX_CODEX_AUTH] {
            let ev = adapter("a-provider-from-2031").scan(frame);
            assert!(ev.is_empty(), "unknown providers need explicit pattern knowledge: {ev:?}");
        }
    }

    // -- Gemini --------------------------------------------------------------

    #[test]
    fn gemini_quota_kinds() {
        let generic = adapter("gemini").scan(FX_GEMINI_QUOTA);
        assert_eq!(limits(&generic)[0].kind, RateLimitKind::Unknown);
        let daily = adapter("gemini").scan(FX_GEMINI_DAILY);
        assert_eq!(limits(&daily)[0].kind, RateLimitKind::Daily);
    }

    #[test]
    fn gemini_idle_and_active() {
        let idle = adapter("gemini").scan(FX_GEMINI_IDLE);
        assert_eq!(waiting_reasons(&idle), vec!["idle_prompt"], "{idle:?}");
        let active = adapter("gemini").scan(FX_GEMINI_ACTIVE);
        assert!(active.is_empty(), "{active:?}");
    }

    // -- Codex ---------------------------------------------------------------

    #[test]
    fn codex_usage_limit_has_no_reset() {
        let ev = adapter("codex").scan(FX_CODEX_LIMIT);
        let rl = limits(&ev);
        assert_eq!(rl.len(), 1, "{ev:?}");
        assert_eq!(rl[0].kind, RateLimitKind::Unknown);
        assert_eq!(
            rl[0].reset_at, None,
            "never lift a reset from the codex banner (AMUX-2088)"
        );
    }

    #[test]
    fn codex_idle_prompt() {
        let ev = adapter("codex").scan(FX_CODEX_IDLE);
        assert_eq!(waiting_reasons(&ev), vec!["idle_prompt"], "{ev:?}");
    }

    #[test]
    fn codex_auth_failure_is_failed_not_idle() {
        let ev = adapter("codex").scan(FX_CODEX_AUTH);
        let fs = failures(&ev);
        assert_eq!(fs.len(), 1, "the live auth wall must produce one failure: {ev:?}");
        assert!(!fs[0].retryable, "sign-in requires a person; retrying cannot repair it");
        assert!(fs[0].reason.contains("Please sign in again"));
        assert!(
            waiting_reasons(&ev).is_empty(),
            "the model bar below the auth wall must not overwrite failure with idle: {ev:?}"
        );
    }

    #[test]
    fn quoted_codex_auth_text_is_not_a_failure() {
        let frame = "› Explain the sentence: Your access token could not be refreshed. Please sign in again.\n\n  gpt-5.5 xhigh · ~/Dev/amux";
        let ev = adapter("codex").scan(frame);
        assert!(failures(&ev).is_empty(), "a user prompt quoting the error is not UI chrome: {ev:?}");
        assert_eq!(waiting_reasons(&ev), vec!["idle_prompt"]);
    }

    // -- Status detection regressions ----------------------------------------

    #[test]
    fn bypass_on_with_esc_to_interrupt_is_active() {
        // The status bar can contain both "bypass permissions on" AND
        // "esc to interrupt" simultaneously — the latter wins because
        // it only appears during active generation. Reported as IDLE
        // while a worker was visibly generating with 2 subagents.
        let frame = "\
Running 1 shell command · 5s…
  └ $ curl -sk \"$AMUX_URL/api/board\" | python3 -c \"
    import json,sys
    i=[x for x in json.load(sys.stdin) if x['id']=='BACKE-3045'][0]
    print('status:',i['status'],'| type:',i.get('type'))
    \" 2>&1 | head -60

« Quantumizing… (10s · ↓ 84 tokens)

                                                                        backend

⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt · ← 2 agents                /rc failed";
        let st = claude_tui_state(frame);
        assert!(matches!(st, TuiState::Active), "expected Active, got {st:?}");
    }

    #[test]
    fn blocked_on_background_subagents_is_active_not_idle() {
        // Ethan, 2026-08-15: gtm-playbooks showed IDLE while orchestrating nine
        // subagents. Frame transcribed from his screenshot — note what it does
        // NOT contain: no spinner, no "esc to interrupt", and a bare composer.
        // Every existing Active signal is absent, which is why it fell through
        // to "bar visible, no signal -> idle at prompt".
        let frame = "\u{2733} Waiting for 9 background agents to finish\n\n\
                     \u{203a}\n\n\
                     \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{b7} \u{2190} 2 agents \u{b7} \u{2193} to manage";
        let st = claude_tui_state(frame);
        assert!(matches!(st, TuiState::Active), "expected Active, got {st:?}");
    }

    #[test]
    fn finished_background_agents_do_not_pin_the_lane_active() {
        // The counterpart, and the reason this keys on the "waiting for N"
        // SENTENCE rather than on the status bar's agent COUNT: once the agents
        // land, the waiting line is gone and the lane must fall back to idle.
        // A count-based check would keep this frame Active forever, which is the
        // worse failure — a lane pinned Active is never picked up.
        let frame = "\u{203a}\n\n\
                     \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{b7} \u{2190} 2 agents \u{b7} \u{2193} to manage";
        let st = claude_tui_state(frame);
        assert!(matches!(st, TuiState::Idle), "expected Idle, got {st:?}");
    }

    #[test]
    fn a_completed_turn_after_a_background_wait_is_idle() {
        // ATE-45, Primis at 15:40. tmux keeps the provider-owned waiting row
        // in scrollback after the child and parent have both completed. The
        // later completed-turn row is the ordered terminal edge; counting the
        // older row by presence pinned this exact frame WORKING for minutes.
        let frame = "\
\u{2736} Waiting for 1 background agent to finish
\u{23fa} Agent \"Post-fix status verification\" finished \u{b7} 16s
\u{23fa} The background Explore agent finished and returned CLAUDE-POSTFIX-DONE.
CLAUDE-POSTFIX-COMPLETE
\u{273b} Churned for 23s \u{b7} done 3:40 PM
\u{276f}\u{a0}
\u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{b7} \u{2190} 2 agents";
        assert!(!claude_background_agents_working(frame));
        assert!(claude_background_wait_superseded(frame));
        let st = claude_tui_state(frame);
        assert!(matches!(st, TuiState::Idle), "later completion must own the frame, got {st:?}");

        let next_turn = format!(
            "{frame}\n\u{2733} Waiting for 2 background agents to finish\n\u{276f}\n\u{23f5}\u{23f5} bypass permissions on"
        );
        assert!(
            claude_background_agents_working(&next_turn),
            "a newer wait still wins over the previous turn's completion"
        );
    }

    #[test]
    fn angle_quote_spinner_is_active() {
        // claude-fable-5 uses « as its spinner prefix instead of a dingbat.
        let frame = "« Quantumizing… (10s · ↓ 84 tokens)\n\n⏵⏵ bypass permissions on";
        let st = claude_tui_state(frame);
        assert!(matches!(st, TuiState::Active), "expected Active, got {st:?}");
    }

    #[test]
    fn middle_dot_spinner_frame_is_active() {
        // AR-133: Claude Code cycles spinner glyphs including · (U+00B7),
        // which is outside the U+2700-27BF dingbat range. Measured: the
        // glyph appears in ~1/6 of frames.
        let frame = "\u{b7} Thinking\u{2026}\n\n\u{23f5}\u{23f5} bypass permissions on";
        let st = claude_tui_state(frame);
        assert!(matches!(st, TuiState::Active), "expected Active, got {st:?}");
    }

    #[test]
    fn esc_to_interrupt_without_prompt_is_active() {
        // No spinner visible, no prompt visible, but "esc to interrupt" on
        // the bar — the spinner scrolled off. This is genuinely active.
        let frame = "\
  some output text
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}
  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{b7} esc to interrupt \u{b7} \u{2190} 2 agents";
        let st = claude_tui_state(frame);
        assert!(matches!(st, TuiState::Active), "expected Active, got {st:?}");
    }

    // -- Ollama --------------------------------------------------------------
    // ollama workers now run codex --oss --local-provider ollama, so the
    // terminal adapter delegates to scan_codex. The old bare-ollama-repl error
    // strings (FX_OLLAMA_NO_DAEMON, FX_OLLAMA_NO_MODEL) no longer appear in
    // terminal output — codex wraps the daemon error in its own structured
    // event layer. What IS detectable is the codex usage-limit pattern, since
    // the underlying codex CLI may still emit that.

    #[test]
    fn ollama_codex_usage_limit_is_detected() {
        let ev = adapter("ollama").scan(FX_CODEX_LIMIT);
        assert!(!limits(&ev).is_empty(), "codex usage-limit must be detected on ollama workers");
    }

    #[test]
    fn codex_trust_dialog_is_detected_as_waiting() {
        // codex shows this at startup before the composer; message delivery
        // must see it as a block (AMUX-3159) so the session isn't marked idle.
        let frame = "Welcome to Codex, OpenAI's command-line coding agent\n\n\
                     > You are in /Users/ethan\n\n\
                     Do you trust the contents of this directory? Working with untrusted\n\
                     contents comes with higher risk of prompt injection.\n\n\
                     \u{203a} 1. Yes, continue\n\
                       2. No, quit\n\n\
                       Press enter to continue";
        let ev = adapter("codex").scan(frame);
        let reasons = waiting_reasons(&ev);
        assert!(
            reasons.contains(&"trust_prompt"),
            "codex trust dialog must produce trust_prompt waiting reason; got {reasons:?}"
        );
    }

    #[test]
    fn ollama_model_bar_is_detected_as_idle() {
        // ollama workers show "qwen3.8:27b low · ~" — the model bar uses a
        // local model name, not gpt-/o3/o4. The old check missed it entirely,
        // leaving the session in an unknown state instead of idle.
        let frame = "  Tip: Our most capable model\n\n\
                     \u{26a0} Model metadata for `qwen3.8:27b` not found.\n\n\
                     \u{203a} what is 2+2?\n\n\
                       qwen3.8:27b low \u{b7} ~";
        let ev = adapter("ollama").scan(frame);
        let reasons = waiting_reasons(&ev);
        assert!(
            reasons.contains(&"idle_prompt"),
            "ollama model bar must produce idle_prompt waiting reason; got {reasons:?}"
        );
    }

    // -- Codex/ollama active scrape (AMUX-3165) ------------------------------
    // The working pane carries no WorkerEvent (Active needs a turn id the pure
    // scanner cannot mint), so the state is driven by `generating`. A working
    // ollama worker read `running=false` for its whole run because nothing
    // reported this — codex/ollama are hookless, so the scrape is their voice.

    // "• Working (12s • esc to interrupt)" — the pane the live incident showed.
    const FX_CODEX_WORKING: &str = "\
› do the thing

• Working (12s • esc to interrupt)";

    // `amux-testing-e2e`, live 2026-09-04 while Codex was running ATE-37.
    // Codex keeps the prompt shell/model bar painted during generation; the
    // active row immediately above it is the current-state boundary.
    const FX_CODEX_WORKING_WITH_PROMPT_SHELL: &str = "\
• Waiting for background terminal (16m 29s • esc to interrupt)
› Ask Codex to do anything
  gpt-5.6-sol xhigh · ~/Dev/amux";

    const FX_CODEX_COMPLETED: &str = "\
• Worked for 4m 46s
› Ask Codex to do anything
  gpt-5.6-sol xhigh · ~/Dev/amux";

    // `amux-testing-e2e`, live 2026-09-04 after an interrupted ATE-45 turn.
    // The interruption is a provider-owned terminal row and the prompt below
    // it is ready input, not a quoted copy of either marker.
    const FX_CODEX_INTERRUPTED_AT_PROMPT: &str = "\
• Confirmed: b228d3dc is on origin/main, and live /health reports descendant commit b228d3dc1f03173fbccd07d7a36e0fbcdf10c5ef.

• Ran amux board discard ATE-52 --outcome-stdin
  └ ATE-52 → discarded

■ Conversation interrupted - tell the model what to do differently. Something went wrong? Hit `/feedback` to report the issue.

› Ask Codex to do anything

  gpt-5.6-sol xhigh · ~/Dev/amux · Main [default]";

    #[test]
    fn codex_working_pane_is_generating() {
        assert!(
            adapter("codex").generating(FX_CODEX_WORKING),
            "the '• Working (…esc to interrupt)' pane must read as generating"
        );
        // ollama runs codex --oss, same pane format.
        assert!(adapter("ollama").generating(FX_CODEX_WORKING));
    }

    #[test]
    fn codex_working_row_above_the_prompt_shell_is_generating() {
        assert!(
            adapter("codex").generating(FX_CODEX_WORKING_WITH_PROMPT_SHELL),
            "Codex paints its prompt shell during a live turn; the adjacent working row wins"
        );
        assert!(adapter("ollama").generating(FX_CODEX_WORKING_WITH_PROMPT_SHELL));
    }

    #[test]
    fn codex_completed_row_above_the_prompt_shell_is_idle() {
        assert!(!adapter("codex").generating(FX_CODEX_COMPLETED));
        assert_eq!(
            waiting_reasons(&adapter("codex").scan(FX_CODEX_COMPLETED)),
            vec!["idle_prompt"]
        );
        assert!(!adapter("ollama").generating(FX_CODEX_COMPLETED));
    }

    #[test]
    fn codex_interrupted_turn_above_the_prompt_shell_is_idle() {
        assert!(!adapter("codex").generating(FX_CODEX_INTERRUPTED_AT_PROMPT));
        // Ollama uses the same Codex TUI and must preserve the same edge.
        assert!(!adapter("ollama").generating(FX_CODEX_INTERRUPTED_AT_PROMPT));
    }

    #[test]
    fn codex_idle_and_trust_panes_are_not_generating() {
        // The idle model bar must NOT read as generating (would pin it active).
        assert!(!adapter("codex").generating(FX_CODEX_IDLE));
        let ollama_idle = "\u{203a} what is 2+2?\n\n  qwen3.8:27b low \u{b7} ~";
        assert!(!adapter("ollama").generating(ollama_idle));
        // The trust dialog is a block (Waiting), not active work.
        let trust = "Do you trust the contents of this directory?\n\n\u{203a} 1. Yes, continue";
        assert!(!adapter("codex").generating(trust));
    }

    #[test]
    fn claude_and_gemini_never_scrape_generating() {
        // Their active state is reported by hooks / the structured protocol
        // (the D1 exit), never the scrape — so `generating` is false for them
        // even on a visibly-active frame, and the caller mints no turn for a
        // lane whose harness speaks for itself.
        let claude_active = "\u{2733} Thinking\u{2026}\n\n\u{23f5}\u{23f5} bypass permissions on \u{b7} esc to interrupt";
        assert!(!adapter("claude-code").generating(claude_active));
        assert!(!adapter("gemini").generating(claude_active));
    }
}
