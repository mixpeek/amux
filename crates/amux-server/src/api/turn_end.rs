//! What a worker SAID at the end of its turn, read as a request for action.
//!
//! Consumers of the turn-end edge (the Stop hook's `idle` report, or a native
//! `Stop` lifecycle event), driven by the final assistant text in the lane's
//! own transcript:
//!
//! 1. OWNER ASK (AMUX-5234). The fleet stall review of 2026-09-26
//!    (docs/fleet-stall-review-2026-09-26.md) found nine of 24 lanes parked on
//!    a question to the owner written in prose: "say go and I'll...",
//!    "awaiting your word", "unless you want it sooner", "needs one thing from
//!    you", a `BLOCKED-ASK` line in a STATUS file. Most of those were inside
//!    the standing authority the owner had already granted (mixpeek-cicd sat on
//!    three in-boundary code fixes; gtm-engine spent a round trip asking "say
//!    go" for a rewrite), and the ones that were not (a paid staging suite, a
//!    $749 pass, a prod roll without a standby) never became `needsyou` cards,
//!    so nothing surfaced them. So: an in-boundary ask is steered back ONCE with
//!    "proceed, standing authority covers this"; a boundary ask becomes a
//!    deduplicated card carrying the question and what unblocks it: a
//!    `needsyou` card when the owner's `AMUX_APPROVAL_TYPES` policy covers its
//!    ask type, otherwise a `type=decision` card (see [`AskPath`]).
//!    While a `/goal` is active, `AskUserQuestion` is converted the same way at
//!    PreToolUse (gs-4 sat 19 minutes on a picker, then found more levers it
//!    could pull alone).
//!
//! CONSERVATIVE BY CONSTRUCTION. Every classifier here returns "nothing" when
//! unsure, and every decision is logged with a `verdict=` so a sweep can count
//! the misses. The dangerous direction is steering a lane into an action the
//! owner must approve, so boundary detection is deliberately generous (a false
//! boundary costs one card) while ask detection is narrow (a false ask costs an
//! unwanted nudge). Isolated lanes are never touched (CLAUDE.md: no automated
//! steering or board capture), and `steer_enqueue` enforces that again at its
//! chokepoint.
//!
//! Kill switch `AMUX_OWNER_ASK_STEER`, default ON, scoped worker > group >
//! global with the process env winning (the `needsyou_ask_required` resolver).

use super::session_verbs as sv;
use super::AppState;
use axum::{http::HeaderMap, http::StatusCode, response::IntoResponse, response::Response, Json};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::OnceLock;

pub(crate) const OWNER_ASK_KEY: &str = "AMUX_OWNER_ASK_STEER";
/// Steering guard labels. Non-empty on purpose: `steer_enqueue` treats an
/// empty guard as the owner's own send, which would bypass the isolation and
/// pause refusals this automation must respect.
pub(crate) const OWNER_ASK_GUARD: &str = "owner-ask";
pub(crate) const SIGKILL_RESUME_GUARD: &str = "turn-end-sigkill";
pub(crate) fn re(
    cell: &'static OnceLock<regex::Regex>,
    pat: &str,
) -> &'static regex::Regex {
    cell.get_or_init(|| regex::Regex::new(pat).expect("static turn_end pattern"))
}
/// Compile-once regex (the `cached_re!` idiom from session_verbs, which is
/// file-local there). Static patterns only.
macro_rules! rx {
    ($pat:expr) => {{
        static CELL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        $crate::api::turn_end::re(&CELL, $pat)
    }};
}
pub(crate) use rx;


// ---------------------------------------------------------------------------
// Kill switches
// ---------------------------------------------------------------------------

/// Pure resolution: process env wins, then the scoped value, default ON.
fn switch_on(env: Option<&str>, scoped: Option<&str>) -> bool {
    fn off(v: &str) -> bool {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    }
    match env.filter(|v| !v.trim().is_empty()) {
        Some(v) => !off(v),
        None => !scoped.is_some_and(off),
    }
}

pub(crate) fn enabled(lane: &str, key: &str) -> bool {
    let env = std::env::var(key).ok();
    let scoped = sv::scoped_setting_in(&sv::home(), lane, key);
    switch_on(env.as_deref(), scoped.as_deref())
}

// ---------------------------------------------------------------------------
// Text normalisation
// ---------------------------------------------------------------------------

/// Lowercase, straighten quotes, and drop the parts of a message that are
/// QUOTED rather than SAID: fenced code, inline code, block quotes and quoted
/// phrases. A worker discussing "say go" (this module's own commit message, a
/// review of another lane) must not read as asking it. `say "go"` is folded to
/// `say go` first, because that one quoted word IS the ask.
pub(crate) fn said_text(text: &str) -> String {
    let t = text
        .replace(['\u{2018}', '\u{2019}'], "'")
        .replace(['\u{201c}', '\u{201d}'], "\"")
        .to_lowercase();
    let t = rx!(r#"\bsay (["'])go(["'])"#).replace_all(&t, "say go");
    let t = rx!(r"(?s)```.*?```").replace_all(&t, " ");
    let t = rx!(r"`[^`\n]*`").replace_all(&t, " ");
    let t = rx!(r#""[^"\n]*""#).replace_all(&t, " ");
    // Single-quoted phrases too. The OPENING quote must follow whitespace and
    // the closing one precede whitespace or punctuation, so an apostrophe
    // inside a word ("i'll") neither opens nor closes a quote.
    let t = rx!(r"(?m)(^|\s)'[^\n]{3,}?'([\s.,;:!?)]|$)").replace_all(&t, "$1 $2");
    t.lines()
        .filter(|l| !l.trim_start().starts_with('>'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The last `n` non-empty paragraphs. The ask that parks a lane is the one it
/// ENDS on; a question in the middle of a long report has already been
/// answered by the report continuing.
pub(crate) fn tail_paragraphs(text: &str, n: usize) -> String {
    let paras: Vec<&str> = rx!(r"\n[ \t]*\n")
        .split(text)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let start = paras.len().saturating_sub(n);
    paras[start..].join("\n\n")
}

/// The paragraphs an ask can sit in: the last two, plus the one before them
/// when it ends with a colon, because then it INTRODUCES the list below it.
/// gs-4, 2026-09-26: "Everything else remaining is waiting on your reply to my
/// previous message:" followed by a two-item list and a closing line; the ask
/// was the third paragraph from the end and read as no ask at all.
pub(crate) fn ask_tail(said: &str) -> String {
    let paras: Vec<&str> = rx!(r"\n[ \t]*\n")
        .split(said)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let mut start = paras.len().saturating_sub(2);
    while start > 0 && paras[start - 1].trim_end().ends_with(':') && paras.len() - start < 4 {
        start -= 1;
    }
    paras[start..].join("\n\n")
}

/// Sentences, split on terminal punctuation and on line breaks (a bullet is a
/// sentence), with list markers and markdown emphasis stripped.
pub(crate) fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = rx!(r"^\s*(?:[-*+]|\d+[.)])\s+").replace(line, "");
        let line = line.replace("**", "").replace("__", "");
        let mut cur = String::new();
        let chars: Vec<char> = line.chars().collect();
        for (i, c) in chars.iter().enumerate() {
            cur.push(*c);
            let end = matches!(c, '.' | '?' | '!')
                && chars.get(i + 1).is_none_or(|n| n.is_whitespace());
            if end {
                let s = cur.trim().to_string();
                if !s.is_empty() {
                    out.push(s);
                }
                cur.clear();
            }
        }
        let s = cur.trim().to_string();
        if !s.is_empty() {
            out.push(s);
        }
    }
    out
}

/// The same sentence split over the ORIGINAL text, for quoting back to the
/// worker and onto a card in its own casing.
pub(crate) fn original_sentence(original: &str, lowered: &str) -> String {
    let flat = |s: &str| {
        s.to_lowercase()
            .replace(['\u{2018}', '\u{2019}'], "'")
            .replace(['\u{201c}', '\u{201d}'], "\"")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let want = flat(lowered);
    if want.is_empty() {
        return lowered.to_string();
    }
    // Compare like with like: `lowered` had code spans and quotes cut by
    // said_text, so cut them from each candidate too, then return the
    // candidate UNCUT. A code span can also split the ORIGINAL where the cut
    // text is one sentence (a command starting with `!`), so adjacent pairs
    // are candidates as well; the shortest match wins. Without this the steer
    // quoted the cut text back: "still waiting on you: , to record the two
    // standing approvals." (amux, 2026-09-27).
    let sents = sentences(original);
    let mut cands: Vec<String> = sents.clone();
    for w in sents.windows(2) {
        cands.push(format!("{} {}", w[0], w[1]));
    }
    cands
        .into_iter()
        .rev()
        .filter(|s| {
            let f = flat(&said_text(s));
            f.contains(&want) || (want.contains(&f) && f.len() > 8 && f.len() * 2 > want.len())
        })
        .min_by_key(|s| s.len())
        .unwrap_or_else(|| lowered.to_string())
}

pub(crate) fn clip(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        s
    } else {
        format!("{}...", s.chars().take(max).collect::<String>().trim_end())
    }
}

// ---------------------------------------------------------------------------
// F1: owner-ask classifier
// ---------------------------------------------------------------------------

/// The standing-authority boundary (~/.claude/CLAUDE.md "Standing authority"),
/// plus the one class of ask only the owner can physically satisfy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Boundary {
    /// Spending money, paid infrastructure, a billed quota.
    Money,
    /// Anything a customer or outside person reads.
    ExternalSend,
    /// Deleting, overwriting or migrating customer/production data, or a
    /// production change with no way back.
    ProdData,
    /// `git push` to main over somebody else's commits.
    ForeignPush,
    /// A sign-in, grant or credential only the owner can supply. Not in the
    /// standing-authority list, but "proceed" cannot be obeyed, so steering
    /// would only produce a second copy of the same ask.
    OwnerOnly,
}

impl Boundary {
    pub(crate) fn ask_type(self) -> &'static str {
        match self {
            Boundary::Money => "budget",
            Boundary::ExternalSend => "customer_outbound",
            Boundary::ProdData | Boundary::ForeignPush => "decision",
            Boundary::OwnerOnly => "credential",
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Boundary::Money => "money",
            Boundary::ExternalSend => "external send",
            Boundary::ProdData => "production data",
            Boundary::ForeignPush => "push over foreign commits",
            Boundary::OwnerOnly => "owner-only access",
        }
    }
    fn unblocks(self, owner: &str, lane: &str) -> String {
        match self {
            Boundary::Money => format!(
                "{owner} approves or declines the spend on this card; {lane} does not spend until it is approved."
            ),
            Boundary::ExternalSend => format!(
                "{owner} approves the outbound message on this card or sends it; {lane} does not send until then."
            ),
            Boundary::ProdData => format!(
                "{owner} approves or declines the production change on this card; {lane} holds that step until then."
            ),
            Boundary::ForeignPush => format!(
                "The authors of the foreign commits (or {owner}) consent to the push on this card."
            ),
            Boundary::OwnerOnly => format!(
                "{owner} completes the sign-in, grant or credential step named in the question and notes it on this card."
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OwnerAsk {
    /// No owner-directed ask at the end of the turn, or not sure there is one.
    None,
    /// An ask the standing authority already answers.
    InBoundary { sentence: String },
    /// An ask that touches the boundary: file it, do not steer.
    Boundary { sentence: String, kind: Boundary },
}

/// Owner-directed ask phrasings. Each is second person or an explicit request
/// for a go-ahead; none matches a worker merely reporting that it asked
/// somebody. Real specimens from the 2026-09-26 review are the test fixtures.
fn is_ask_sentence(s: &str) -> bool {
    let pats: [&regex::Regex; 14] = [
        rx!(r"\bsay go\b|\bsay the word\b"),
        rx!(r"\b(awaiting|waiting (on|for)|need|needs|want) your (word|go|go-?ahead|call|decisions?|approvals?|sign-?off|ok|okay|green ?light|confirmation|answers?|reply|replies|input)\b"),
        rx!(r"\bon your (word|go|signal|say-?so)\b"),
        rx!(r"\bunless you (want|would like|'d like|prefer|say|object)\b"),
        rx!(r"\bneeds? (one|a|1|two|2) (thing|things|decision|decisions|answer|call|input) from you\b|\bone thing from you\b"),
        rx!(r"^(want me to|do you want me to|would you like me to|should i|shall i|ok to|okay to|ok if i|may i|can i go ahead)\b.*\?$"),
        rx!(r"\b(want me to|do you want me to|would you like me to|should i|shall i)\b[^.!]*\?$"),
        rx!(r"\bif you('d| would)? ?(like|want)( me to)?,? i (can|could|will|'ll)\b"),
        rx!(r"\blet me know (if|whether|when) you('d| would)? ?(like|want)\b|\blet me know which\b"),
        rx!(r"\bready (to go |to proceed )?(when|once) you (are|say|give)\b"),
        rx!(r"\byour call\b|\bgo/no-?go\b"),
        rx!(r"^blocked-ask\b"),
        // gs-4-gke-minimization, 2026-09-26: "Still waiting for your answers:
        // the six permission lines, the re-login, and the ten decisions".
        rx!(r"\bstill (waiting|blocked) (on|for) (you|your)\b"),
        // tubescience-parity, 2026-09-27, 2,206 turns in a row: "The only
        // thing left is TP-37: sign in once at https://... in the
        // `ethan-tubescience` Chrome profile." An imperative sign-in aimed at
        // a URL or a browser profile has no "you" in it and is still an ask.
        sign_in_at_re(),
    ];
    pats.iter().any(|p| p.is_match(s))
}

/// An imperative sign-in, log-in or re-auth pointed at a URL or a browser
/// profile: only the owner can type that password. Matched over `said_text`,
/// so the profile name in backticks is already gone and "profile" remains.
/// First person ("i'll sign in at") is excluded by requiring the verb to open
/// the sentence, follow a colon, or follow "please"/"you (need to)".
fn sign_in_at_re() -> &'static regex::Regex {
    rx!(r"(^|[:;]\s*|\bplease\s+|\byou\s+(need to |must |have to |should |can )?)(sign|log) ?(back )?in\b[^\n]{0,160}?(https?://|\bprofile\b)|(^|[:;]\s*|\bplease\s+)re-?auth\w*\b[^\n]{0,160}?(https?://|\bprofile\b)")
}

/// Things the owner said in THIS turn's prompt that make "proceed" wrong: an
/// explicit request to only plan, only report, or wait. Steering past those
/// would override the owner, which standing authority does not cover.
pub(crate) fn owner_said_hold(prompt: &str) -> bool {
    let p = prompt.to_lowercase().replace(['\u{2018}', '\u{2019}'], "'");
    rx!(r"\b(don'?t|do not) (do|start|change|touch|make|run|push|proceed|implement|act)\b|\bjust (tell|plan|answer|report|explain|list|show)\b|\bonly (plan|report|answer|explain)\b|\b(wait for|check with|ask) me (first|before)\b|\bhold off\b|\bwait for me\b|\bno changes\b|\bread-?only\b")
        .is_match(&p)
}

/// Which part of the boundary, if any, `context` touches. Generous on purpose:
/// a false positive here files one card, a false negative steers a lane into
/// spending money or mailing a customer.
pub(crate) fn boundary_of(context: &str) -> Option<Boundary> {
    let c = context.to_lowercase();
    let has = |r: &regex::Regex| r.is_match(&c);
    if has(rx!(r"\bpush\w*\b")) && has(rx!(r"\bmain\b"))
        && has(rx!(r"\b(foreign|other lanes?'?|peers?'?|someone else'?s|not mine|others'|their commits|force-?push)"))
    {
        return Some(Boundary::ForeignPush);
    }
    if has(rx!(r"\$\s?\d|\b\d+\s?(usd|dollars)\b|\b(spend|spending|pay|paying|payment|purchase|buy|buying|billing|billed|invoice|subscription|paid|budget|credit card|pricing|quota increase|raise the quota|upgrade (the |our )?(plan|tier))\b")) {
        return Some(Boundary::Money);
    }
    let send = rx!(r"\b(send|sending|sent|email|emailing|e-mail|dm|reply|replying|respond|post|posting|publish|publishing|tweet|announce|submit|outreach)\b");
    let outside = rx!(r"\b(customer|customers|client|clients|prospect|prospects|lead|leads|vendor|partner|investor|contact|contacts|recipient|recipients|external|outside|public|publicly|linkedin|twitter|buffer|ghost|blog|newsletter|campaign|instantly|apollo|inbox|thread|them|him|her|upstream|mailing list)\b");
    // AN ADDRESS OR A MESSAGE CHANNEL IS OUTBOUND ON ITS OWN. With in-boundary
    // asks now auto-answered "proceed" (owner policy), "Want me to send the
    // welcome email to partners@thefasttrackgirl.com?" must never read as
    // in-boundary just because no word from the `outside` list appears.
    let address = rx!(r"[a-z0-9._%+-]+@[a-z0-9-]+\.[a-z0-9.-]+");
    let channel = rx!(r"\b(email|e-mail|emails|dm|dms|message|messages|inmail|tweet|post|posts|reply|newsletter|campaign|sequence)\b");
    if has(address)
        || (has(send) && has(channel))
        || (has(send) && has(outside))
        || has(rx!(r"\bpr comment|\bcomment on (the |their )?(pr|issue)\b|\bpublish(ed|ing)?\b|\bgo live\b|\bpress release\b"))
        || has(rx!(r"\b(launch|start|kick off|enable|activate|turn on)\b[\w/ -]{0,40}\b(sequence|campaign|outreach|drip|cadence)\b"))
    {
        return Some(Boundary::ExternalSend);
    }
    let destructive = rx!(r"\b(delete|deleting|drop|dropping|truncate|purge|wipe|erase|destroy|migrate|migrating|migration|backfill|overwrite|restore over)\b");
    let data = rx!(r"\b(prod|production|customer|customers|tenant|tenants|table|tables|collection|collections|database|db|bucket|buckets|index|indexes|namespace|namespaces|data|records|rows|documents|mongo|postgres|bigquery|s3|gcs)\b");
    let prod_risk = rx!(r"\b(roll|rolling|cutover|cut over|failover|fail over|switch|promote|restart|disable|shut ?down|scale (down|to zero)|without (a )?(warm )?standby)\b");
    if (has(destructive) && has(data)) || (has(rx!(r"\b(prod|production)\b")) && has(prod_risk)) {
        return Some(Boundary::ProdData);
    }
    if sign_in_at_re().is_match(&c) {
        return Some(Boundary::OwnerOnly);
    }
    if has(rx!(r"\b(sign[- ]?in|log ?in|re-?auth\w*|oauth|credentials?|password|2fa|mfa|api key|secret|iam|grant|role binding|act ?as|permission|console access|admin rights)\b"))
        && has(rx!(r"\b(you|your|ethan|owner)\b"))
    {
        return Some(Boundary::OwnerOnly);
    }
    // A DECISION THE TEXT RESERVES FOR THE OWNER. amux-helper, 2026-10-01: a
    // turn ended "That's when to decide between cutting scope and moving the
    // date, which is your call, not the orchestrator's", the steer answered
    // "proceed ... take your recommended one", and the lane sent goal spec 12's
    // orchestrator a scope cut Ethan had not made ("wait hang on ur limiting
    // the scope of GS-12?"). It fired again on the apology that explained it.
    // When the lane itself says a choice is the owner's, "proceed" decides it
    // in his name; a card asks him instead.
    if reserved_for_owner(&c) {
        return Some(Boundary::OwnerOnly);
    }
    None
}

/// The text says a choice is the owner's to make (see boundary_of).
fn reserved_for_owner(lower: &str) -> bool {
    rx!(r"\b(your call|your decision|your choice|yours to (decide|make|call)|up to you|you decide|you choose|you'?ll decide|(ethan|the owner)'?s (call|decision|choice))\b")
        .is_match(lower)
}

/// The first card id in `sentence` whose card is in `needsyou`, if any.
fn needsyou_card_named(state: &AppState, sentence: &str) -> Option<String> {
    let conn = state.store.read().ok()?;
    super::goal_loop::card_ids(sentence).into_iter().find(|id| {
        conn.query_row(
            "SELECT status FROM issues WHERE id=?1 AND deleted IS NULL",
            rusqlite::params![id],
            |r| r.get::<_, String>(0),
        )
        .map(|st| st == "needsyou")
        .unwrap_or(false)
    })
}

/// Classify the final assistant text of a turn.
pub(crate) fn classify_owner_ask(text: &str) -> OwnerAsk {
    let said = said_text(text);
    // A BLOCKED-ASK line is a structured marker wherever it sits (gs-10 wrote
    // them into STATUS.md and echoed them in its report).
    let marker = said
        .lines()
        .map(str::trim)
        .rev()
        .find(|l| l.starts_with("blocked-ask"))
        .map(str::to_string);
    let tail = ask_tail(&said);
    let sents = sentences(&tail);
    let hit = sents
        .iter()
        .enumerate()
        .rev()
        .find(|(_, s)| is_ask_sentence(s))
        .map(|(i, s)| (i, s.clone()));
    // Paragraph of each sentence, so the one BEFORE the ask is read only
    // when it belongs to the ask. amux-meta-helper, 2026-09-27 19:29: a
    // report's last bullet ("MVS production promote, a CI merge-gate policy
    // change, ...") sat a blank line above "Given the size, want me to keep
    // this as a reference list, or would it help more to work through one
    // category at a time?", and the pair read as a production-data boundary
    // (AMH-17). `sentences` drops blank lines, so per-paragraph sentences
    // concatenate to exactly `sents`.
    let para_of: Vec<usize> = tail
        .split("\n\n")
        .enumerate()
        .flat_map(|(p, chunk)| std::iter::repeat_n(p, sentences(chunk).len()))
        .collect();
    // A reserved decision ("which is your call") in the ask's OWN paragraph
    // counts even when the picked ask is the sentence before it (the
    // 2026-10-01 scope-cut steer). An earlier paragraph does not: a report's
    // "Needs your call:" bullets above a standalone ask stay out (AMH-17).
    let mut reserved_in_para = false;
    let (sentence, context) = match (hit, marker) {
        (Some((i, s)), _) => {
            if para_of.len() == sents.len() {
                reserved_in_para = sents
                    .iter()
                    .zip(&para_of)
                    .any(|(t, p)| *p == para_of[i] && reserved_for_owner(t));
            }
            // A short ask ("Want me to do it?") leans on the sentence before it
            // wherever that sits; a full one stands alone across a paragraph.
            let same_para = para_of.len() == sents.len() && i > 0 && para_of[i - 1] == para_of[i];
            let leans = s.split_whitespace().count() <= 10;
            let prev = if i > 0 && (same_para || leans) { sents[i - 1].as_str() } else { "" };
            let ctx = format!("{prev} {s}");
            (s, ctx)
        }
        (None, Some(m)) => (m.clone(), m),
        (None, None) => return OwnerAsk::None,
    };
    let sentence = original_sentence(text, &sentence);
    // The sign-in can sit in the list BELOW the ask sentence. tubescience-parity,
    // 12:41Z 2026-09-27: "...TP-37 is still waiting on you." then "1. TP-37:
    // sign in at https://...". Reading only the ask sentence steered it to
    // "proceed" on a password only the owner can type. A false boundary costs
    // one card, so the whole ask tail may escalate to OwnerOnly.
    let boundary = boundary_of(&context).or_else(|| {
        sentences(&tail)
            .iter()
            .any(|s| sign_in_at_re().is_match(s))
            .then_some(Boundary::OwnerOnly)
            .or(reserved_in_para.then_some(Boundary::OwnerOnly))
    });
    match boundary {
        Some(kind) => OwnerAsk::Boundary { sentence, kind },
        None => OwnerAsk::InBoundary { sentence },
    }
}

// ---------------------------------------------------------------------------
// Transcript readers (pure over the records `iter_jsonl_tail` returns)
// ---------------------------------------------------------------------------

pub(crate) fn rec_ts(r: &Value) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(r["timestamp"].as_str()?)
        .ok()
        .map(|t| t.timestamp_millis() as f64 / 1000.0)
}

pub(crate) fn text_blocks(content: &Value) -> Vec<String> {
    match content {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str().map(str::to_string))
            .collect(),
        _ => vec![],
    }
}

/// A user record that is a new instruction rather than a tool result or
/// harness chrome. Task notifications count: they start a new turn.
fn is_real_prompt(r: &Value) -> bool {
    if r["type"] != "user" || r["isMeta"] == true || r["isSidechain"] == true {
        return false;
    }
    let content = &r["message"]["content"];
    if let Some(a) = content.as_array() {
        if a.iter().any(|b| b["type"] == "tool_result") {
            return false;
        }
    }
    !text_blocks(content).join("").trim().is_empty()
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TurnTail {
    /// `uuid` of the final assistant record: the dedupe key for both nudges.
    pub uuid: String,
    pub text: String,
    pub ts: f64,
    /// The prompt that started this turn, for [`owner_said_hold`].
    pub prompt: String,
}

/// The final assistant TEXT of the most recent turn, if the turn ended on it.
///
/// Claude writes one message as several records sharing `message.id`
/// (thinking, text, tool_use). The turn ended on text only if the LAST
/// assistant record carries text and no tool_use; a turn that stopped mid-tool
/// (interrupt, API error) has no final statement to classify and returns None.
/// A real prompt after the final assistant record also returns None: the lane
/// has already been spoken to.
/// The turn's last tool result was a SIGKILL ("Exit code 137") and the text
/// it ended on is a short sign-off rather than a report.
pub(crate) fn ended_after_sigkill(records: &[Value], turn: &TurnTail) -> bool {
    if turn.text.trim().chars().count() > 200 {
        return false;
    }
    let end = records.iter().rposition(|r| r["uuid"].as_str() == Some(turn.uuid.as_str())).unwrap_or(records.len());
    for r in records[..end].iter().rev() {
        if r["type"] != "user" {
            continue;
        }
        let Some(blocks) = r["message"]["content"].as_array() else { continue };
        if let Some(tr) = blocks.iter().rev().find(|b| b["type"] == "tool_result") {
            let body = match &tr["content"] {
                Value::String(s) => s.clone(),
                Value::Array(a) => text_blocks(&Value::Array(a.clone())).join("\n"),
                _ => String::new(),
            };
            return body.trim_start().starts_with("Exit code 137");
        }
        if r["message"]["content"].is_string() || blocks.iter().any(|b| b["type"] == "text") {
            return false; // a real prompt came after the last tool: not this shape
        }
    }
    false
}

pub(crate) fn final_turn(records: &[Value]) -> Option<TurnTail> {
    let last_asst = records
        .iter()
        .rposition(|r| r["type"] == "assistant" && r["isSidechain"] != true)?;
    if records[last_asst + 1..].iter().any(is_real_prompt) {
        return None;
    }
    let last = &records[last_asst];
    let blocks = last["message"]["content"].as_array()?;
    if blocks.iter().any(|b| b["type"] == "tool_use") {
        return None;
    }
    let mid = last["message"]["id"].as_str().unwrap_or("");
    let mut parts: Vec<String> = Vec::new();
    for r in records[..=last_asst].iter().rev() {
        if r["type"] != "assistant" {
            continue;
        }
        if mid.is_empty() || r["message"]["id"].as_str() != Some(mid) {
            break;
        }
        let t = text_blocks(&r["message"]["content"]).join("\n\n");
        if !t.trim().is_empty() {
            parts.push(t);
        }
    }
    if parts.is_empty() {
        return None;
    }
    parts.reverse();
    let prompt = records[..last_asst]
        .iter()
        .rev()
        .find(|r| is_real_prompt(r))
        .map(|r| text_blocks(&r["message"]["content"]).join("\n"))
        .unwrap_or_default();
    Some(TurnTail {
        uuid: last["uuid"].as_str().unwrap_or("").to_string(),
        text: parts.join("\n\n"),
        ts: rec_ts(last).unwrap_or(0.0),
        prompt,
    })
}

/// The condition of the `/goal` active in this transcript, if one is.
///
/// Claude Code records the goal as `attachment.type == "goal_status"` with
/// `met` and `condition` (written when the goal is set and at every goal
/// check), and the command itself as a `<command-name>/goal` user record.
/// Measured 2026-09-26 on gs-3-bucket-objects: 239 goal_status attachments,
/// 19 `/goal` commands. The NEWEST record decides: `met: true` or a `/goal`
/// with empty/clear arguments ends it. Nothing found means no goal, which is
/// the conservative answer (the question is left alone).
pub(crate) fn goal_condition(records: &[Value]) -> Option<String> {
    for r in records.iter().rev() {
        if r["type"] == "attachment" && r["attachment"]["type"] == "goal_status" {
            if r["attachment"]["met"] == true {
                return None;
            }
            return Some(
                r["attachment"]["condition"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
            )
            .filter(|c| !c.trim().is_empty());
        }
        if r["type"] == "user" {
            let t = text_blocks(&r["message"]["content"]).join("");
            if t.contains("<command-name>/goal</command-name>") {
                let args = rx!(r"(?s)<command-args>(.*?)</command-args>")
                    .captures(&t)
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str().trim().to_lowercase())
                    .unwrap_or_default();
                if matches!(args.as_str(), "" | "clear" | "off" | "stop" | "cancel" | "none") {
                    return None;
                }
                return Some(args);
            }
        }
    }
    None
}
// ---------------------------------------------------------------------------
// Side effects
// ---------------------------------------------------------------------------

pub(crate) fn owner_name() -> String {
    std::env::var("AMUX_OWNER_NAME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("AMUX_OWNER_EMAIL")
                .ok()
                .and_then(|e| e.split('@').next().map(str::to_string))
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| "ethan".into())
}

pub(crate) fn transcript_for(name: &str, session_id: &str) -> Option<PathBuf> {
    (!session_id.is_empty())
        .then(|| sv::lifecycle_transcript_path(name, session_id))
        .flatten()
        .or_else(|| sv::session_jsonl_path(name))
}

/// Record `idem` once. True only for the call that inserted it, so a restart,
/// a duplicate Stop, or a legacy and native report for the same turn cannot
/// act twice.
async fn claim_once(state: &AppState, session: &str, etype: &str, idem: String, data: Value) -> bool {
    let session = session.to_string();
    let etype = etype.to_string();
    state
        .store
        .write_async(move |conn| {
            sv::ensure_fleet_tables(conn)?;
            let n = conn.execute(
                "INSERT OR IGNORE INTO session_events (ts, session, type, data, idem, source) \
                 VALUES (?1,?2,?3,?4,?5,'turn-end')",
                rusqlite::params![crate::config::now_f64(), session, etype, data.to_string(), idem],
            )?;
            Ok(crate::db::WriteOutcome { applied: n == 1, events: vec![] })
        })
        .await
        .map(|o| o.applied)
        .unwrap_or(false)
}

/// Record `idem` unless an event of `etype` for this lane carrying `key` in
/// its data was recorded in the last `window_s` seconds. True only for the call
/// that inserted.
///
/// Why a window and not the turn uuid (tubescience-parity, 2026-09-27): a lane
/// looping under a /goal ends a NEW turn every five seconds on the same ask, so
/// a per-turn claim steered it "proceed" at 12:41:27Z and again at 12:42:22Z,
/// and each steer fed the loop another turn.
async fn claim_within(
    state: &AppState,
    session: &str,
    etype: &str,
    key: &str,
    window_s: f64,
    idem: String,
    data: Value,
) -> bool {
    let (session, etype, key) = (session.to_string(), etype.to_string(), key.to_string());
    state
        .store
        .write_async(move |conn| {
            sv::ensure_fleet_tables(conn)?;
            let now = crate::config::now_f64();
            let recent: i64 = conn.query_row(
                "SELECT COUNT(*) FROM session_events WHERE session = ?1 AND type = ?2 \
                 AND ts > ?3 AND json_extract(data, '$.key') = ?4",
                rusqlite::params![session, etype, now - window_s, key],
                |r| r.get(0),
            )?;
            if recent > 0 {
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            }
            let mut data = data;
            data["key"] = json!(key);
            let n = conn.execute(
                "INSERT OR IGNORE INTO session_events (ts, session, type, data, idem, source) \
                 VALUES (?1,?2,?3,?4,?5,'turn-end')",
                rusqlite::params![now, session, etype, data.to_string(), idem],
            )?;
            Ok(crate::db::WriteOutcome { applied: n == 1, events: vec![] })
        })
        .await
        .map(|o| o.applied)
        .unwrap_or(false)
}

/// How long one in-boundary steer covers the same question on the same lane.
const OWNER_ASK_STEER_WINDOW_S: f64 = 6.0 * 3600.0;

/// [`claim_once`] for sibling modules (the goal-loop guard).
pub(crate) async fn claim_once_pub(state: &AppState, session: &str, etype: &str, idem: String, data: Value) -> bool {
    claim_once(state, session, etype, idem, data).await
}

/// Origin label for cards the goal-loop guard files (AMUX-5277).
pub(crate) const GOAL_LOOP_ORIGIN: &str = "goal-loop guard";

/// File (or find) the card for a lane goal-looping on an ask that named no card.
pub(crate) async fn file_goal_loop_card(
    state: &AppState,
    lane: &str,
    kind: Option<Boundary>,
    question: &str,
    context: &str,
    isolated: bool,
) -> Result<(String, bool, AskPath), String> {
    let ask_type = kind.map(Boundary::ask_type).unwrap_or("decision");
    let path = AskPath::for_type(lane, ask_type);
    file_ask_card_via(state, lane, kind, question, context, GOAL_LOOP_ORIGIN, path, isolated).await
}

fn question_key(q: &str) -> String {
    let norm: String = q
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let digest = Sha256::digest(norm.as_bytes());
    format!("owner-ask:{}", hex_prefix(&digest, 6))
}
fn hex_prefix(bytes: &[u8], n: usize) -> String {
    bytes.iter().take(n).map(|b| format!("{b:02x}")).collect()
}

/// A question as a card can carry it: one sentence ending in `?`.
pub(crate) fn as_question(sentence: &str, lane: &str) -> String {
    let s = clip(sentence, 280);
    if s.trim_end().ends_with('?') {
        s
    } else {
        format!("May {lane} proceed with this: {}?", s.trim_end_matches(['.', '!', ':']))
    }
}

/// Which board shape an ask takes. `needsyou` is reserved for the owner's
/// configured authorization categories (`AMUX_APPROVAL_TYPES`; the board API
/// answers 409 `needsyou_outside_approval_policy` for any other ask_type, and
/// this box's global policy is `budget,customer_outbound`). An ask outside that
/// policy is recorded the way the refusal directs: a `type=decision` card in
/// `backlog` carrying `decision_question`, `decision_rationale` and a
/// structured `waiting_on`, so it is visible without claiming an approval
/// category the owner did not configure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AskPath {
    NeedsYou,
    Decision,
}
impl AskPath {
    pub(crate) fn for_type(lane: &str, ask_type: &str) -> Self {
        if crate::db::board_store::approval_type_allowed(Some(lane), ask_type) {
            AskPath::NeedsYou
        } else {
            AskPath::Decision
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            AskPath::NeedsYou => "needsyou",
            AskPath::Decision => "decision",
        }
    }
}

/// File (or find) the card for an ask. Returns (id, created, path).
async fn file_ask_card(
    state: &AppState,
    lane: &str,
    kind: Option<Boundary>,
    question: &str,
    context: &str,
    origin: &str,
) -> Result<(String, bool, AskPath), String> {
    let ask_type = kind.map(Boundary::ask_type).unwrap_or("decision");
    file_ask_card_via(state, lane, kind, question, context, origin, AskPath::for_type(lane, ask_type), false).await
}

#[allow(clippy::too_many_arguments)]
async fn file_ask_card_via(
    state: &AppState,
    lane: &str,
    kind: Option<Boundary>,
    question: &str,
    context: &str,
    origin: &str,
    path: AskPath,
    isolated: bool,
) -> Result<(String, bool, AskPath), String> {
    use crate::db::board_store as bs;
    let tag = question_key(question);
    let owner = owner_name();
    let ask_type = kind.map(Boundary::ask_type).unwrap_or("decision");
    let label = kind.map(Boundary::label).unwrap_or("decision");
    let title = format!("Owner ask ({label}): {}", clip(question, 110));
    let why = match kind {
        _ if origin == GOAL_LOOP_ORIGIN => format!(
            "because {lane} ended three turns in a row on this ask while its /goal kept re-prompting it \
             (AMUX-5277), so only the owner can move it"
        ),
        _ if isolated => format!(
            "because {lane} is isolated: amux may not steer it and only the owner's own messages reach it, \
             so the ask is recorded here instead of living only in its terminal"
        ),
        Some(_) => format!(
            "because the ask touches the standing-authority boundary ({label}), so it was not steered"
        ),
        None => "because a /goal was active; the worker was told to proceed on its \
                 recommended option, so this card is the record of the choice"
            .to_string(),
    };
    let unblocks = match kind {
        None if isolated => format!(
            "{owner} answers on this card and sends the answer to {lane} directly (an isolated lane only accepts the owner's messages)."
        ),
        Some(k) => k.unblocks(&owner, lane),
        None => format!(
            "{owner} confirms or overrides the choice on this card; {lane} has already proceeded on its recommended option."
        ),
    };
    let desc = format!(
        "{lane} asked the owner. Filed automatically by the {origin} (AMUX-5234) {why}.\n\n\
         Question: {question}\n\nContext, in the worker's words:\n\n{}",
        clip(context, if isolated { 6000 } else { 1500 })
    );
    let needsyou = path == AskPath::NeedsYou;
    let mut tags = vec![tag.clone(), TURN_END_ASK_TAG.to_string()];
    if isolated {
        tags.push(ISOLATED_ASK_TAG.to_string());
    }
    if needsyou {
        tags.insert(0, bs::NEEDS_YOU_TAG.to_string());
    }
    let new = bs::NewIssue {
        acceptance_criteria: None,
        next_action: None,
        title,
        desc,
        status: if needsyou { "needsyou" } else { "backlog" }.into(),
        session: Some(lane.to_string()),
        item_type: if needsyou { "chore" } else { "decision" }.into(),
        creator: "amux".into(),
        owner_type: "agent".into(),
        due: None,
        due_time: None,
        reviewer: None,
        shepherd: None,
        gate: vec![],
        depends_on: vec![],
        tags,
        ask_type: needsyou.then(|| ask_type.to_string()),
        ask_question: needsyou.then(|| question.to_string()),
        ask_unblocks: needsyou.then(|| unblocks.clone()),
        ask_actor: needsyou.then(|| owner.clone()),
        source: Some("turn_end".into()),
        requested_by: None,
        callback_session: None,
        callback_prompt: None,
    };
    let waiting_on = json!({"actor": owner, "type": ask_type, "question": question, "unblocks": unblocks})
        .to_string();
    let rationale = format!("{why}. {unblocks}");
    let lane_s = lane.to_string();
    let q = question.to_string();
    let update_ctx = context.to_string();
    let found = std::sync::Arc::new(std::sync::Mutex::new(None::<(String, bool)>));
    let slot = found.clone();
    state
        .store
        .write_async(move |conn| {
            use rusqlite::OptionalExtension;
            let existing: Option<String> = conn
                .query_row(
                    "SELECT i.id FROM issues i JOIN issue_tags t ON t.issue_id = i.id \
                     WHERE i.session = ?1 AND t.tag = ?2 AND i.deleted IS NULL \
                     AND i.status NOT IN ('done','verified','discarded') LIMIT 1",
                    rusqlite::params![lane_s, tag],
                    |r| r.get(0),
                )
                .optional()?;
            // An isolated lane repeats its ask every turn in slightly different
            // words (gs-4 did so nine times in a row under a goal check). One
            // open card per lane, updated, beats a card per rephrasing.
            let existing = match existing {
                Some(id) => Some((id, false)),
                None if isolated => conn
                    .query_row(
                        "SELECT i.id FROM issues i JOIN issue_tags t ON t.issue_id = i.id \
                         WHERE i.session = ?1 AND t.tag = ?2 AND i.deleted IS NULL \
                         AND i.status NOT IN ('done','verified','discarded') \
                         ORDER BY i.updated DESC LIMIT 1",
                        rusqlite::params![lane_s, ISOLATED_ASK_TAG],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?
                    .map(|id| (id, true)),
                None => {
                    // REPHRASED REPEATS UPDATE THE OPEN CARD ON EVERY LANE, not
                    // only isolated ones. tubescience-parity filed TP-38..TP-48 on
                    // 2026-09-27: "Still blocked on your OK to spend about $16 on
                    // Gemini embedding?", "Still waiting on your go for about $16
                    // of Gemini embedding?", and so on, each a new needsyou card
                    // because the exact-question key differed by a few words.
                    let mut st = conn.prepare(
                        "SELECT i.id, COALESCE(i.ask_question, i.decision_question, i.title) FROM issues i \
                         JOIN issue_tags t ON t.issue_id = i.id \
                         WHERE i.session = ?1 AND t.tag = ?2 AND i.deleted IS NULL \
                         AND i.status NOT IN ('done','verified','discarded') ORDER BY i.updated DESC",
                    )?;
                    let open: Vec<(String, String)> = st
                        .query_map(rusqlite::params![lane_s, TURN_END_ASK_TAG], |r| Ok((r.get(0)?, r.get(1)?)))?
                        .flatten()
                        .collect();
                    open.into_iter()
                        .find(|(_, oq)| same_ask(oq, &q))
                        .map(|(id, _)| (id, true))
                }
            };
            let out = match existing {
                Some((id, false)) => (id, false),
                Some((id, true)) => {
                    if let Some(mut row) = bs::get_issue(conn, &id)? {
                        row.desc.push_str(&format!(
                            "\n\n--- Update {}: the lane asked again ---\n\nQuestion: {q}\n\n{}",
                            chrono::Local::now().format("%Y-%m-%d %H:%M"),
                            clip(&update_ctx, 4000)
                        ));
                        bs::save_patched(conn, &mut row)?;
                    }
                    (id, false)
                }
                None => {
                    let row = bs::create_issue(conn, &new, crate::config::now_f64() as i64)?;
                    if !needsyou {
                        // NewIssue has no decision/wait fields; set them the way
                        // PATCH does, through the one row writer.
                        if let Some(mut row) = bs::get_issue(conn, &row.id)? {
                            row.decision_question = Some(q.clone());
                            row.decision_rationale = Some(rationale);
                            row.waiting_on = Some(waiting_on);
                            bs::save_patched(conn, &mut row)?;
                        }
                    }
                    (row.id, true)
                }
            };
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(out);
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await
        .map_err(|e| e.to_string())?;
    let out = found.lock().unwrap_or_else(|e| e.into_inner()).take();
    out.map(|(id, created)| (id, created, path))
        .ok_or_else(|| "card write returned no id".into())
}

/// Tag on every card the turn-end recorder files, on any lane.
pub(crate) const TURN_END_ASK_TAG: &str = "turn-end-owner-ask";

/// The ask without the "May <lane> proceed with this:" wrapper as_question
/// adds, which every auto-filed card shares and which would make any two of
/// them look alike.
fn ask_core(q: &str) -> String {
    let t = q.trim();
    match t.find(" proceed with this:") {
        Some(i) if t.starts_with("May ") => t[i + " proceed with this:".len()..].trim().to_string(),
        _ => t.to_string(),
    }
}

/// Same ask, reworded? Overlap relative to the SHORTER ask (not Jaccard):
/// a long restatement of a short ask adds words that would sink a union-based
/// score, measured on TP-44 ("about $16 of Gemini embedding?") against TP-45
/// (the same plus a cost breakdown). Tokens come from goal_loop::tokens, so
/// URLs and numbers are normalized. Asks with fewer than three content tokens never merge.
pub(crate) fn same_ask(a: &str, b: &str) -> bool {
    // Waiting-and-asking filler says HOW the lane asked, not WHAT it asks for.
    const FILLER: &[&str] = &[
        "still", "blocked", "waiting", "wait", "your", "you", "ok", "okay", "go", "goahead", "only",
        "remaining", "step", "steps", "need", "needs", "needed", "approve", "approval", "say", "word",
        "want", "should", "shall", "may", "can", "could", "would", "the", "and", "for", "on", "of",
        "to", "about", "this", "that", "with", "from", "please", "i", "im", "me", "my", "it", "is",
        "are", "be", "am", "a", "an", "in", "at", "yet",
    ];
    let content = |t: &str| {
        let mut set = super::goal_loop::tokens(&ask_core(t));
        set.retain(|w| !FILLER.contains(&w.as_str()));
        set
    };
    let (x, y) = (content(a), content(b));
    let small = x.len().min(y.len());
    if small < 3 {
        return false;
    }
    x.intersection(&y).count() as f64 / small as f64 >= ASK_MERGE_OVERLAP
}
const ASK_MERGE_OVERLAP: f64 = 0.6;

/// Kill switch for answering an isolated lane's in-boundary asks "proceed"
/// under owner policy (default on).
pub(crate) const AUTO_PROCEED_KEY: &str = "AMUX_ISOLATED_AUTO_PROCEED";
/// The owner-policy guard: owner configuration, so it reaches isolated lanes.
pub(crate) const OWNER_POLICY_GUARD: &str = "owner-policy:auto-proceed";

fn day_bucket() -> i64 {
    (crate::config::now_f64() as i64) / 86_400
}

fn policy_text(sentence: &str) -> String {
    format!(
        "[amux owner-policy] Your last turn ended by asking: \"{}\". Ethan's standing \
         authority covers this, so proceed now; if you offered options, take your \
         recommended one. Stop and file a needsyou card only for {BOUNDARY_TEXT}.",
        clip(sentence, 240)
    )
}

/// Deliver the owner-policy "proceed" to a lane.
async fn policy_proceed(state: &AppState, name: &str, sentence: &str, id: &str) -> Result<(), String> {
    sv::steer_enqueue_idempotent_report(state, name, &policy_text(sentence), OWNER_POLICY_GUARD, "", id)
        .await
        .map(|_| ())?;
    sv::steer_deliver_for_session(state, name).await;
    Ok(())
}

/// Clear in-boundary asks already parked on isolated lanes before auto-proceed
/// existed: an idle lane never reaches another turn end, so nothing else would
/// answer them. Only cards this recorder filed with no boundary (title
/// "Owner ask (decision): ..."), still open, on an isolated lane that is idle.
/// Each card is answered once, then closed with the answer as evidence.
pub(crate) async fn auto_proceed_open_isolated_asks(state: &AppState, idle_isolated: &[String]) {
    use crate::db::board_store as bs;
    for lane in idle_isolated {
        if !enabled(lane, AUTO_PROCEED_KEY) {
            continue;
        }
        let ids: Vec<String> = {
            let Ok(conn) = state.store.read() else { continue };
            let Ok(mut st) = conn.prepare(
                "SELECT i.id FROM issues i JOIN issue_tags t ON t.issue_id = i.id \
                 WHERE i.session = ?1 AND t.tag = ?2 AND i.deleted IS NULL \
                 AND i.status IN ('needsyou','backlog','todo') AND i.title LIKE 'Owner ask (decision):%'",
            ) else { continue };
            st.query_map(rusqlite::params![lane, ISOLATED_ASK_TAG], |r| r.get::<_, String>(0))
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default()
        };
        for id in ids {
            let row = {
                let Ok(conn) = state.store.read() else { continue };
                bs::get_issue(&conn, &id).ok().flatten()
            };
            let Some(row) = row else { continue };
            let q = row.ask_question.clone().or(row.decision_question.clone()).unwrap_or(row.title.clone());
            if !claim_once(state, lane, "turn_end.isolated_proceed_sweep", format!("iso-sweep:{id}"), json!({"card": id})).await {
                continue;
            }
            match policy_proceed(state, lane, &q, &format!("iso-sweep-{id}")).await {
                Ok(()) => {
                    let id2 = id.clone();
                    let _ = state.store.write_async(move |conn| {
                        if let Some(mut r) = bs::get_issue(conn, &id2)? {
                            r.desc.push_str("\n\n--- answered by owner policy ---\n\nSent proceed to the lane (AMUX_ISOLATED_AUTO_PROCEED).");
                            r.evidence = Some("owner-policy auto-proceed delivered to the lane (in-boundary ask, isolated lane)".into());
                            r.status = "done".into();
                            bs::save_patched(conn, &mut r)?;
                        }
                        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                    }).await;
                    tracing::warn!(session = %lane, card = %id, verdict = "isolated_ask_swept_proceed",
                        "owner policy answered a parked in-boundary ask and closed its card");
                }
                Err(e) => tracing::warn!(session = %lane, card = %id, verdict = "isolated_ask_sweep_refused", error = %e,
                    "owner-policy proceed refused for a parked ask; card left open"),
            }
        }
    }
}

/// Kill switch for recording an ISOLATED lane's owner asks as cards.
pub(crate) const ISOLATED_ASK_KEY: &str = "AMUX_ISOLATED_ASK_CARDS";
/// Tag on every card this path files, so a repeat updates the open card.
pub(crate) const ISOLATED_ASK_TAG: &str = "isolated-owner-ask";

/// Items in a list: numbered or bulleted lines, and numbered table rows
/// (gs-4 put its ten decisions in a `| # | Decision | My default |` table).
fn enumerated_lines(text: &str) -> usize {
    text.lines()
        .filter(|l| {
            rx!(r"^\s*(\d{1,2}[.)]|[-*\u{2022}])\s+\S").is_match(l)
                || rx!(r"^\s*\|\s*#?\d{1,2}\s*\|").is_match(l)
        })
        .count()
}

/// A message that only points back at an earlier one ("in my earlier
/// message", "from two messages ago", "still waiting"). gs-4 sent six of those
/// in a row; attaching one of them would hand the owner another pointer.
fn is_pointer_message(lower: &str) -> bool {
    rx!(r"\b(previous|earlier|last|prior) message\b|\bmessages? ago\b|\bmessage above\b|\bstill (waiting|stopped|blocked)\b")
        .is_match(lower)
}

/// The list an ask points back to. gs-4's turns end on "Still waiting for your
/// answers: the six permission lines, the re-login, and the ten decisions",
/// while the lines and the decisions themselves are in an EARLIER message. A
/// card carrying only the pointer is useless to the owner, so when the final
/// text enumerates fewer than three items, attach the newest earlier assistant
/// message (main thread, last 60 messages) that enumerates at least three and
/// talks about decisions, permissions, grants or answers.
pub(crate) fn earlier_ask_list(records: &[Value], final_text: &str) -> Option<String> {
    if enumerated_lines(final_text) >= 3 {
        return None;
    }
    let final_norm = final_text.trim();
    let mut seen = 0;
    for r in records.iter().rev() {
        if r["type"] != "assistant" || r["isSidechain"] == true {
            continue;
        }
        let t = text_blocks(&r["message"]["content"]).join("\n\n");
        let t = t.trim();
        if t.is_empty() || t == final_norm || final_norm.contains(t) {
            continue;
        }
        seen += 1;
        if seen > 60 {
            break;
        }
        let lower = t.to_lowercase();
        if is_pointer_message(&lower) {
            continue;
        }
        if enumerated_lines(t) >= 3
            && rx!(r"\b(decisions?|permissions?|grants?|answers?|approve|approval|sign[- ]?in|re-?login|questions?|defaults?)\b").is_match(&lower)
        {
            return Some(t.to_string());
        }
    }
    None
}

const BOUNDARY_TEXT: &str = "spending money, anything a customer or outside person \
reads (email, DM, PR comment, post), deleting or migrating customer or production data, \
and pushing to main over someone else's commits";

fn steer_text(sentence: &str) -> String {
    format!(
        "[amux owner-ask] Your last turn ended by asking the owner: \"{}\". Standing \
         authority covers this, so proceed without waiting; if you offered options, take \
         your recommended one. The only boundary where you stop and ask is {BOUNDARY_TEXT}. \
         If this really crosses one of those, put the question and what unblocks it on \
         a card for the owner, then keep going on other work.",
        clip(sentence, 240)
    )
}

/// The turn-end consumer. Spawned from the report handler on the idle edge
/// (legacy Stop hook) and on an applied native `Stop` event.
/// Worker types the turn-end classifier leaves alone. A chat worker IS a
/// conversation with the owner, so a reply ending on a question is the product
/// working, not a lane parked on an ask. Measured 2026-09-27 on the live
/// server: a haiku chat worker answered "What should I pick up next? Give me
/// the task" and this classifier steered it to "proceed", which it could not
/// do and which spent a turn. The promise nudge rides the same exit.
pub(crate) fn skips_turn_end_classifier(worker_type: &str) -> bool {
    worker_type == "chat"
}

pub(crate) async fn on_turn_end(state: AppState, name: String, session_id: String) {
    if sv::provider_of(&sv::parse_env(&name)) != "claude" {
        return;
    }
    let wtype = crate::api::worker_exec::worker_type_of(&name);
    if skips_turn_end_classifier(wtype.as_str()) {
        tracing::debug!(session = %name, worker_type = wtype.as_str(), verdict = "turn_end_skipped_worker_type",
            "turn-end: conversational worker type; owner-ask and promise classifiers do not apply");
        return;
    }
    let isolated = sv::session_is_isolated(&name);
    // The Stop hook fires as the final record is written; give the transcript
    // writer a moment so the classifier reads the turn that just ended.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let Some(path) = transcript_for(&name, &session_id) else {
        tracing::debug!(session = %name, verdict = "turn_end_no_transcript", "turn-end: no transcript to classify");
        return;
    };
    let records = sv::iter_jsonl_tail(&path, 2_000_000);
    let Some(turn) = final_turn(&records) else {
        // Under a looping /goal the next turn can already have started by the
        // time this reads (tubescience-parity turned over every ~5s), so the
        // goal-loop guard still gets the last turn END it can see.
        if let Some(end) = super::goal_loop::turn_ends(&records).pop() {
            if crate::config::now_f64() - end.ts < 30.0 {
                let turn = TurnTail { uuid: end.id, text: end.text, ts: end.ts, prompt: String::new() };
                super::goal_loop::on_turn_end(&state, &name, isolated, &records, &turn).await;
            }
        }
        tracing::debug!(session = %name, verdict = "turn_end_no_final_text", "turn-end: turn did not end on text");
        return;
    };
    // A turn that ended right after its last tool was SIGKILLed (exit 137) is
    // an abnormal stop, not an idle: amux-cloud 2026-10-06 ~12:31 (`pkill -f`
    // matched its own shell) and amux the same day ended on "No response
    // requested." with a card in doing, and sat idle until the owner asked.
    if !isolated && ended_after_sigkill(&records, &turn) {
        if let Some(card) = state.store.read().ok()
            .and_then(|c| crate::runtime_jobs::board_drive::exact_resume_card(&c, &name).ok().flatten())
        {
            let text = format!("[amux] Your last command was killed (exit 137) and the turn ended there, \
                with {card} still in doing. Resume {card}: re-run what was killed (check the command \
                does not match its own shell, e.g. `pkill -f`), then continue.");
            let id = format!("sigkill-{}", turn.uuid);
            match sv::steer_enqueue_idempotent_report(&state, &name, &text, SIGKILL_RESUME_GUARD, "", &id).await {
                Ok(_) => {
                    tracing::warn!(session = %name, card = %card, verdict = "turn_end_after_sigkill",
                        measured = true, n_considered = 1,
                        "turn-end: the turn ended right after a SIGKILLed tool with a card in doing; nudged to resume");
                    sv::steer_deliver_for_session(&state, &name).await;
                }
                Err(e) => tracing::warn!(session = %name, verdict = "turn_end_after_sigkill_refused", error = e,
                    "turn-end: a SIGKILLed turn could not be nudged"),
            }
            return;
        }
    }
    // Record which cards the lane's latest turn named, for the session list's
    // "Needs input" projection (a lane blocked on its own needsyou card).
    let named = super::goal_loop::card_ids(&turn.text);
    sv::update_meta(&name, &[("last_turn_cards", json!(named)), ("last_turn_ts", json!(turn.ts))]);
    // AMUX-5277: a lane goal-looping on the owner is not steered. Steering it
    // "proceed" only bought tubescience-parity another turn of the same ask.
    if super::goal_loop::on_turn_end(&state, &name, isolated, &records, &turn).await {
        super::promise_nudge::forget_promise(&name);
        return;
    }
    let verdict = classify_owner_ask(&turn.text);
    if isolated {
        isolated_owner_ask(&state, &name, &records, &turn, verdict).await;
        return;
    }
    match verdict {
        OwnerAsk::None => {
            tracing::debug!(session = %name, verdict = "owner_ask_none", "turn-end: no owner ask at the end of the turn");
            // AMUX-5236: an ask wins over a promise, so only a turn with no
            // ask is remembered for the idle-stall nudge.
            super::promise_nudge::record_promise(&name, &turn);
        }
        OwnerAsk::InBoundary { ref sentence } => {
            super::promise_nudge::forget_promise(&name);
            if !enabled(&name, OWNER_ASK_KEY) {
                tracing::info!(session = %name, verdict = "owner_ask_disabled", sentence = %clip(sentence, 160),
                    "turn-end: in-boundary owner ask left alone ({OWNER_ASK_KEY} is off)");
                return;
            }
            if owner_said_hold(&turn.prompt) {
                tracing::info!(session = %name, verdict = "owner_ask_owner_said_hold", sentence = %clip(sentence, 160),
                    "turn-end: in-boundary owner ask not steered; this turn's prompt asked the lane to hold or only report");
                return;
            }
            // The sentence names a card ALREADY waiting on the owner
            // (needsyou): it is a status line, not a new ask, and the steer's
            // own remedy ("put the question on a card") is already done.
            // 2026-10-01: "AH-296, full scope versus Sunday, is still waiting
            // for you." was steered to "proceed ... take your recommended one".
            if let Some(carded) = needsyou_card_named(&state, sentence) {
                tracing::info!(session = %name, card = %carded, verdict = "owner_ask_already_carded", sentence = %clip(sentence, 160),
                    "turn-end: owner ask names a card already in needsyou; not steered");
                return;
            }
            let qkey = question_key(sentence);
            if !claim_within(&state, &name, "turn_end.owner_ask_steer", &qkey, OWNER_ASK_STEER_WINDOW_S,
                format!("owner-ask:{name}:{}", turn.uuid),
                json!({"sentence": sentence, "uuid": turn.uuid, "verdict": "in_boundary"})).await
            {
                tracing::info!(session = %name, verdict = "owner_ask_steer_deduped", key = %qkey,
                    sentence = %clip(sentence, 160),
                    "turn-end: in-boundary owner ask already steered on this lane in the last 6h (or this turn); not steered again");
                return;
            }
            let id = format!("owner-ask-{}", turn.uuid);
            match sv::steer_enqueue_idempotent_report(&state, &name, &steer_text(sentence), OWNER_ASK_GUARD, "", &id).await {
                Ok(r) => {
                    tracing::warn!(session = %name, verdict = "owner_ask_steered", steer_id = %r.id,
                        sentence = %clip(sentence, 160),
                        "turn-end: lane ended on an in-boundary owner ask; steered once to proceed (AMUX-5234)");
                    sv::steer_deliver_for_session(&state, &name).await;
                }
                Err(e) => tracing::warn!(session = %name, verdict = "owner_ask_steer_refused", error = e,
                    "turn-end: in-boundary owner ask could not be steered"),
            }
        }
        OwnerAsk::Boundary { ref sentence, kind } => {
            super::promise_nudge::forget_promise(&name);
            if !enabled(&name, OWNER_ASK_KEY) {
                tracing::info!(session = %name, verdict = "owner_ask_disabled", boundary = kind.label(),
                    "turn-end: boundary owner ask left alone ({OWNER_ASK_KEY} is off)");
                return;
            }
            if !claim_once(&state, &name, "turn_end.owner_ask", format!("owner-ask:{name}:{}", turn.uuid),
                json!({"sentence": sentence, "uuid": turn.uuid, "verdict": "boundary", "boundary": kind.label()})).await
            {
                return;
            }
            let question = as_question(sentence, &name);
            let context = tail_paragraphs(&turn.text, 2);
            match file_ask_card(&state, &name, Some(kind), &question, &context, "turn-end owner-ask classifier").await {
                Ok((id, true, path)) => tracing::warn!(session = %name, verdict = "owner_ask_card_filed", card = %id,
                    boundary = kind.label(), path = path.label(),
                    "turn-end: boundary owner ask filed as a card (needsyou if the approval policy allows its type, else a decision card) (AMUX-5234)"),
                Ok((id, false, path)) => tracing::info!(session = %name, verdict = "owner_ask_card_duplicate", card = %id,
                    boundary = kind.label(), path = path.label(), "turn-end: boundary owner ask already has an open card"),
                Err(e) => tracing::warn!(session = %name, verdict = "owner_ask_card_failed", error = %e,
                    "turn-end: boundary owner ask could not be filed"),
            }
        }
    }
}

/// An ISOLATED lane's turn ended on an owner ask. Nothing may steer it (only the
/// owner's own messages reach it), so the ask becomes a card on the lane's own
/// board with the full list it refers to. Measured 2026-09-26: gs-4 ended nine
/// turns in a row on "Still waiting for your answers: the six permission lines,
/// the re-login, and the ten decisions", the goal paused, and the only record
/// of those ten decisions was its terminal.
async fn isolated_owner_ask(state: &AppState, name: &str, records: &[Value], turn: &TurnTail, verdict: OwnerAsk) {
    let (sentence, kind) = match verdict {
        OwnerAsk::None => return,
        OwnerAsk::InBoundary { sentence } => (sentence, None),
        OwnerAsk::Boundary { sentence, kind } => (sentence, Some(kind)),
    };
    // AUTO-PROCEED (Ethan, 2026-09-27 13:20: "it needs to be optimized for
    // auto pushing this is ridiculous"). Of 17 Needs-input items that hour, the
    // in-boundary ones ("Want me to start on 1 and 2?", "Should I start on
    // 3-5?", "say the word and i'll update") were all from ISOLATED lanes,
    // parked as cards because amux may not steer them. The owner configured
    // this answer, so it is delivered as owner policy; boundary asks (money,
    // outbound, prod data, credentials) still become cards.
    if kind.is_none() && enabled(name, AUTO_PROCEED_KEY) && !owner_said_hold(&turn.prompt) {
        let key = format!("isolated-proceed:{name}:{}:{}", question_key(&sentence), day_bucket());
        if claim_once(state, name, "turn_end.isolated_proceed", key, json!({"sentence": sentence, "uuid": turn.uuid})).await {
            match policy_proceed(state, name, &sentence, &format!("iso-proceed-{}", turn.uuid)).await {
                Ok(()) => {
                    tracing::warn!(session = %name, verdict = "isolated_ask_auto_proceeded", sentence = %clip(&sentence, 160),
                        "turn-end: isolated lane's in-boundary ask answered proceed under owner policy");
                    return;
                }
                Err(e) => tracing::warn!(session = %name, verdict = "isolated_ask_auto_proceed_refused", error = %e,
                    "turn-end: owner-policy proceed refused; recording the ask as a card instead"),
            }
        } else {
            return;
        }
    }
    if !enabled(name, ISOLATED_ASK_KEY) {
        tracing::info!(session = %name, verdict = "isolated_ask_disabled", sentence = %clip(&sentence, 160),
            "turn-end: isolated lane's owner ask not recorded ({ISOLATED_ASK_KEY} is off)");
        return;
    }
    if !claim_once(state, name, "turn_end.isolated_ask", format!("isolated-ask:{name}:{}", turn.uuid),
        json!({"sentence": sentence, "uuid": turn.uuid})).await
    {
        return;
    }
    let question = as_question(&sentence, name);
    let mut context = tail_paragraphs(&turn.text, 3);
    let earlier = earlier_ask_list(records, &turn.text);
    if let Some(list) = &earlier {
        context.push_str("\n\nThe earlier message this ask refers to:\n\n");
        context.push_str(list);
    }
    let ask_type = kind.map(Boundary::ask_type).unwrap_or("decision");
    let path = AskPath::for_type(name, ask_type);
    match file_ask_card_via(state, name, kind, &question, &context, "turn-end isolated-lane recorder", path, true).await {
        Ok((id, created, path)) => tracing::warn!(session = %name, verdict = if created { "isolated_ask_card_filed" } else { "isolated_ask_card_updated" },
            card = %id, path = path.label(), with_earlier_list = earlier.is_some(),
            boundary = kind.map(Boundary::label).unwrap_or("none"),
            "turn-end: isolated lane ended on an owner ask; recorded on its board (not steered)"),
        Err(e) => tracing::warn!(session = %name, verdict = "isolated_ask_card_failed", error = %e,
            "turn-end: isolated lane's owner ask could not be recorded"),
    }
}

// ---------------------------------------------------------------------------
// AskUserQuestion under an active /goal (PreToolUse, scripts/hooks/ask-guard.py)
// ---------------------------------------------------------------------------

/// Flatten `AskUserQuestion.tool_input` into (question text, options text).
pub(crate) fn describe_questions(input: &Value) -> (String, String) {
    let mut qs = Vec::new();
    let mut opts = Vec::new();
    for q in input["questions"].as_array().into_iter().flatten() {
        if let Some(t) = q["question"].as_str() {
            qs.push(t.trim().to_string());
        }
        let labels: Vec<String> = q["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| o["label"].as_str().or(o.as_str()).map(str::to_string))
            .collect();
        if !labels.is_empty() {
            opts.push(labels.join(" | "));
        }
    }
    (qs.join(" "), opts.join("; "))
}

/// POST /api/sessions/{name}/owner-ask. Called by the PreToolUse hook before an
/// `AskUserQuestion` runs. Answers `{"decision":"allow"}` or
/// `{"decision":"deny","reason":...}`; the hook fails open on anything else.
pub(crate) async fn ask_user_question_post(
    state: &AppState,
    name: &str,
    headers: &HeaderMap,
    body: &Value,
) -> Response {
    let origin = sv::hdr_worker(headers);
    if !origin.is_empty() && origin != name {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "an ask may only be reported by the lane asking it"})))
            .into_response();
    }
    let allow = |why: &str| {
        tracing::debug!(session = %name, verdict = why, "ask intercept: question allowed through");
        Json(json!({"decision": "allow", "why": why})).into_response()
    };
    if sv::session_is_isolated(name) {
        // Never denied (the owner answers isolated lanes directly), but
        // recorded, so the question is on the board and not only in a picker
        // nobody is looking at (gs-4's sat ~19 minutes on 2026-09-26).
        if enabled(name, ISOLATED_ASK_KEY) {
            let (question, options) = describe_questions(&body["tool_input"]);
            if !question.trim().is_empty() {
                let kind = boundary_of(&format!("{question} {options}"));
                let q = as_question(&question, name);
                let ctx = format!("AskUserQuestion on an isolated lane.\n\nOptions: {options}");
                let path = AskPath::for_type(name, kind.map(Boundary::ask_type).unwrap_or("decision"));
                match file_ask_card_via(state, name, kind, &q, &ctx, "AskUserQuestion isolated recorder", path, true).await {
                    Ok((id, _, _)) => tracing::warn!(session = %name, verdict = "ask_intercept_isolated_recorded", card = %id,
                        "ask intercept: isolated lane's AskUserQuestion recorded as a card and allowed through"),
                    Err(e) => tracing::warn!(session = %name, verdict = "ask_intercept_isolated_card_failed", error = %e,
                        "ask intercept: could not record isolated lane's question"),
                }
            }
        }
        return allow("ask_intercept_isolated");
    }
    if !enabled(name, OWNER_ASK_KEY) {
        return allow("ask_intercept_disabled");
    }
    let sid = body["session_id"].as_str().unwrap_or("");
    let Some(path) = transcript_for(name, sid) else {
        return allow("ask_intercept_no_transcript");
    };
    let records = sv::iter_jsonl_tail(&path, 8_000_000);
    let Some(goal) = goal_condition(&records) else {
        return allow("ask_intercept_no_goal");
    };
    let (question, options) = describe_questions(&body["tool_input"]);
    if question.trim().is_empty() {
        return allow("ask_intercept_unreadable_question");
    }
    // Not every goal-time question touches the boundary, but the owner is not
    // at the keyboard, so the card is the record either way. `decision` is the
    // honest type for an in-boundary one.
    let boundary = boundary_of(&format!("{question} {options}"));
    let q = as_question(&question, name);
    let context = format!("AskUserQuestion while /goal is active (goal: {}).\n\nOptions: {options}", clip(&goal, 300));
    let filed = file_ask_card(state, name, boundary, &q, &context, "AskUserQuestion goal intercept").await;
    let card = match filed {
        Ok((id, created, path)) => {
            tracing::warn!(session = %name, verdict = "ask_intercept_converted", card = %id, created, path = path.label(),
                boundary = boundary.map(Boundary::label).unwrap_or("none"),
                "ask intercept: AskUserQuestion under an active /goal filed as needsyou and denied (AMUX-5234)");
            id
        }
        Err(e) => {
            tracing::warn!(session = %name, verdict = "ask_intercept_card_failed", error = %e,
                "ask intercept: could not file the card; letting the question through");
            return allow("ask_intercept_card_failed");
        }
    };
    let reason = match boundary {
        Some(b) => format!(
            "amux: a /goal is active on this lane, so this question was filed as board card {card} \
             instead of waiting on the owner. It touches the boundary ({}): do NOT take that action. \
             Continue with other in-boundary work toward the goal; the card waits for the owner.",
            b.label()
        ),
        None => format!(
            "amux: a /goal is active on this lane, so this question was filed as board card {card} \
             instead of waiting on the owner. Standing authority covers it: proceed with your \
             recommended option (the first one if you marked none) and keep driving the goal. \
             The boundary where you must not proceed is {BOUNDARY_TEXT}."
        ),
    };
    Json(json!({"decision": "deny", "reason": reason, "card": card})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(text: &str) -> OwnerAsk {
        classify_owner_ask(text)
    }
    fn in_boundary(text: &str) -> bool {
        matches!(ask(text), OwnerAsk::InBoundary { .. })
    }
    fn boundary(text: &str) -> Option<Boundary> {
        match ask(text) {
            OwnerAsk::Boundary { kind, .. } => Some(kind),
            _ => None,
        }
    }

    // --- F1 specimens from docs/fleet-stall-review-2026-09-26.md ------------

    #[test]
    fn gtm_engine_say_go_for_a_rewrite_is_in_boundary() {
        let t = "Rewrote 152 of the 192 emails and loaded them into the sequence drafts.\n\n\
                 The remaining 40 still use the old opener. Say go and I'll rewrite those too.";
        assert!(in_boundary(t), "{:?}", ask(t));
        match ask(t) {
            OwnerAsk::InBoundary { sentence } => assert_eq!(sentence, "Say go and I'll rewrite those too."),
            other => panic!("{other:?}"),
        }
        // The quoted form of the same ask: the one quoted word IS the ask.
        assert!(in_boundary("Loaded.\n\nSay \"go\" and I'll rewrite the rest."));
    }

    #[test]
    fn mixpeek_cicd_awaiting_your_word_is_in_boundary() {
        let t = "MC-2092, MC-2094 and MC-2096 each have a one-line fix ready in the deploy resolver.\n\n\
                 Awaiting your word on all three.";
        assert!(in_boundary(t), "{:?}", ask(t));
    }

    #[test]
    fn mvs_infra_unless_you_want_it_sooner_is_in_boundary() {
        let t = "Watchers are armed on shard 3.\n\nI'll run the auto-fix canary at 02:00 unless you want it sooner.";
        assert!(in_boundary(t), "{:?}", ask(t));
    }

    #[test]
    fn want_me_to_and_shall_i_are_asks() {
        assert!(in_boundary("All green.\n\nWant me to open the follow-up for the flaky test?"));
        assert!(in_boundary("Done with the refactor. Shall I also split the module?"));
    }

    #[test]
    fn gtm_engine_money_ask_is_a_boundary() {
        let t = "The Advertising Week pass is $749 and the early rate ends Friday.\n\nSay go and I'll buy it.";
        assert_eq!(boundary(t), Some(Boundary::Money));
    }

    #[test]
    fn gs10_blocked_ask_lines_are_boundary_asks() {
        let t = "Status written to STATUS.md.\n\n\
                 BLOCKED-ASK: may I roll the MVS prod primary without a warm standby?\n\n\
                 Everything else is green.";
        assert_eq!(boundary(t), Some(Boundary::ProdData));
        let paid = "BLOCKED-ASK: enable the paid Staging E2E suite (about $40/day)?";
        assert_eq!(boundary(paid), Some(Boundary::Money));
    }

    #[test]
    fn tubescience_sign_in_needs_one_thing_from_you() {
        let t = "Backfill is at 1,200 of 3,310 vectors.\n\n\
                 Needs one thing from you: a semantic-search sign-in so I can validate parity.";
        assert_eq!(boundary(t), Some(Boundary::OwnerOnly));
    }

    #[test]
    fn external_send_ask_is_a_boundary() {
        let t = "Drafted the reply to Garik.\n\nWant me to send it to him?";
        assert_eq!(boundary(t), Some(Boundary::ExternalSend));
        let seq = "Rewrite is loaded.\n\nNeeds your go to launch the Programmatic I/O sequence.";
        assert_eq!(boundary(seq), Some(Boundary::ExternalSend));
    }

    #[test]
    fn foreign_push_ask_is_a_boundary() {
        let t = "Rebased on origin/main; there are 4 foreign commits from other lanes in the range.\n\n\
                 Should I push to main anyway?";
        assert_eq!(boundary(t), Some(Boundary::ForeignPush));
    }

    // --- Negative controls ---------------------------------------------------

    #[test]
    fn a_plain_final_summary_is_not_an_ask() {
        let t = "Shipped the fix in abc1234. Tests: 212 passed. Card AMUX-99 moved to done with evidence.";
        assert_eq!(ask(t), OwnerAsk::None);
    }

    #[test]
    fn discussing_asks_in_quotes_is_not_an_ask() {
        let t = "The review found lanes ending on \"say go\" and \"awaiting your word\", and \
                 gtm-engine wrote 'Say go and I'll rewrite them'.\n\n\
                 I added the classifier and its tests; `want me to ...?` phrasing is covered.";
        assert_eq!(ask(t), OwnerAsk::None);
    }

    #[test]
    fn reporting_that_it_asked_a_peer_is_not_an_ask() {
        let t = "I asked mvs-infra whether the shard is clear and am waiting for their answer.\n\n\
                 Meanwhile the loader is building.";
        assert_eq!(ask(t), OwnerAsk::None);
    }

    #[test]
    fn a_question_mid_report_that_the_report_moves_past_is_not_the_ending() {
        let t = "Want me to check the logs? I did anyway.\n\nThe logs were clean.\n\nThe second pass is clean too.\n\nAll done.";
        assert_eq!(ask(t), OwnerAsk::None);
    }

    #[test]
    fn let_me_know_if_you_have_questions_is_a_sign_off() {
        assert_eq!(ask("Everything is merged. Let me know if you have questions."), OwnerAsk::None);
    }

    #[test]
    fn owner_hold_instructions_are_recognised() {
        assert!(owner_said_hold("just tell me what is remaining, don't change anything"));
        assert!(owner_said_hold("Plan it out but wait for me before pushing"));
        assert!(!owner_said_hold("what is remaining for gs3?"));
    }

    // --- Transcript shapes ---------------------------------------------------

    fn asst(uuid: &str, mid: &str, ts: &str, content: Value) -> Value {
        json!({"type":"assistant","uuid":uuid,"timestamp":ts,"message":{"id":mid,"role":"assistant","content":content}})
    }
    fn user_text(ts: &str, text: &str) -> Value {
        json!({"type":"user","timestamp":ts,"message":{"role":"user","content":text}})
    }

    #[test]
    fn a_turn_ending_right_after_a_sigkilled_tool_is_recognised() {
        let tool = |content: &str| json!({"type":"user","uuid":"u-t","message":{"content":[
            {"type":"tool_result","content":content,"is_error":true,"tool_use_id":"t1"}]}});
        let recs = vec![
            user_text("2026-10-06T12:30:00Z", "deploy it"),
            asst("a1", "m1", "2026-10-06T12:30:05Z", json!([{"type":"tool_use","id":"t1","name":"Bash","input":{}}])),
            tool("Exit code 137"),
            asst("a2", "m2", "2026-10-06T12:30:06Z", json!([{"type":"text","text":"No response requested."}])),
        ];
        let t = final_turn(&recs).unwrap();
        assert!(ended_after_sigkill(&recs, &t), "137 then a short sign-off is the shape");
        let mut ok = recs.clone();
        ok[2] = tool("Exit code 1\nboom");
        assert!(!ended_after_sigkill(&ok, &final_turn(&ok).unwrap()), "an ordinary failure is not a SIGKILL");
        let mut long = recs.clone();
        long[3] = asst("a2", "m2", "2026-10-06T12:30:06Z", json!([{"type":"text","text":"x".repeat(300)}]));
        assert!(!ended_after_sigkill(&long, &final_turn(&long).unwrap()), "a real report after the kill is left alone");
    }

    #[test]
    fn final_turn_reads_the_last_text_record_and_its_prompt() {
        let recs = vec![
            user_text("2026-09-26T10:00:00Z", "what is remaining?"),
            asst("a1", "m1", "2026-09-26T10:00:05Z", json!([{"type":"thinking","thinking":"..."}])),
            asst("a2", "m1", "2026-09-26T10:00:06Z", json!([{"type":"text","text":"Two things. Want me to start?"}])),
            json!({"type":"system","subtype":"stop_hook_summary","timestamp":"2026-09-26T10:00:07Z"}),
        ];
        let t = final_turn(&recs).unwrap();
        assert_eq!(t.uuid, "a2");
        assert_eq!(t.prompt, "what is remaining?");
        assert!(t.text.ends_with("Want me to start?"));
    }

    #[test]
    fn a_turn_that_stopped_mid_tool_or_was_answered_has_no_final_text() {
        let tool = vec![asst("a1", "m1", "2026-09-26T10:00:05Z",
            json!([{"type":"text","text":"Running it."},{"type":"tool_use","id":"t","name":"Bash","input":{}}]))];
        assert_eq!(final_turn(&tool), None);
        let answered = vec![
            asst("a1", "m1", "2026-09-26T10:00:05Z", json!([{"type":"text","text":"Want me to go?"}])),
            user_text("2026-09-26T10:01:00Z", "yes"),
        ];
        assert_eq!(final_turn(&answered), None);
    }

    #[test]
    fn goal_is_read_from_the_newest_goal_record() {
        let set = json!({"type":"attachment","attachment":{"type":"goal_status","met":false,"sentinel":true,
            "condition":"drive it to completion you have full authority"}});
        assert_eq!(goal_condition(std::slice::from_ref(&set)).as_deref(), Some("drive it to completion you have full authority"));
        let met = json!({"type":"attachment","attachment":{"type":"goal_status","met":true,"condition":"x"}});
        assert_eq!(goal_condition(&[set.clone(), met]), None);
        let cleared = user_text("2026-09-26T10:00:00Z",
            "<command-name>/goal</command-name>\n<command-message>goal</command-message>\n<command-args>clear</command-args>");
        assert_eq!(goal_condition(&[set.clone(), cleared]), None);
        assert_eq!(goal_condition(&[]), None);
    }

    #[test]
    fn kill_switch_resolution_env_wins_then_scope_default_on() {
        assert!(switch_on(None, None));
        assert!(!switch_on(None, Some("0")));
        assert!(!switch_on(Some("off"), Some("1")));
        assert!(switch_on(Some("1"), Some("0")));
        assert!(switch_on(Some(" "), None));
    }

    #[test]
    fn ask_card_question_is_a_question() {
        assert_eq!(as_question("Want me to send it?", "lane"), "Want me to send it?");
        assert!(as_question("BLOCKED-ASK: enable paid E2E", "gs-10").ends_with('?'));
        assert_eq!(question_key("Want me to send it?"), question_key("want me to SEND it"));
    }

    #[test]
    fn ask_user_question_input_is_flattened_with_options() {
        let input = json!({"questions":[{"question":"Which pool should I scale?","header":"Pool",
            "options":[{"label":"cpu-workers (Recommended)","description":"a"},{"label":"gpu","description":"b"}],"multiSelect":false}]});
        let (q, o) = describe_questions(&input);
        assert_eq!(q, "Which pool should I scale?");
        assert_eq!(o, "cpu-workers (Recommended) | gpu");
    }

    fn hermetic_state() -> (tempfile::TempDir, AppState) {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&tmp.path().join("test.db")).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        (tmp, state)
    }

    #[tokio::test]
    async fn a_boundary_ask_files_one_needsyou_card_and_dedupes_the_repeat() {
        let (_tmp, state) = hermetic_state();
        let q = "May gs-10 proceed with this: enable the paid Staging E2E suite?";
        let (id, created, path) =
            file_ask_card_via(&state, "gs-10", Some(Boundary::Money), q, "ctx", "test", AskPath::NeedsYou, false)
                .await
                .unwrap();
        assert!(created);
        assert_eq!(path, AskPath::NeedsYou);
        // Same question, different casing and punctuation: the same ask.
        let again = "may gs-10 proceed with this -- enable the PAID staging e2e suite?";
        let (id2, created2, _) =
            file_ask_card_via(&state, "gs-10", Some(Boundary::Money), again, "ctx", "test", AskPath::NeedsYou, false)
                .await
                .unwrap();
        assert_eq!((id2.as_str(), created2), (id.as_str(), false));
        let conn = state.store.read().unwrap();
        let row = crate::db::board_store::get_issue(&conn, &id).unwrap().unwrap();
        assert_eq!(row.status, "needsyou");
        assert_eq!(row.ask_type.as_deref(), Some("budget"));
        assert!(row.ask_question.as_deref().unwrap().ends_with('?'));
        assert!(crate::db::board_store::ask_verdict(
            row.ask_actor.as_deref().unwrap_or(""),
            row.ask_type.as_deref().unwrap_or(""),
            row.ask_question.as_deref().unwrap_or(""),
            row.ask_unblocks.as_deref().unwrap_or(""),
        ) == crate::db::board_store::AskVerdict::Ok, "the card must pass the board's own typed-ask gate");
    }

    /// AMUX_APPROVAL_TYPES="budget,customer_outbound" (this box's global
    /// policy): the board refuses needsyou for any other ask_type with a 409,
    /// so a credential or prod-data ask must land as a decision card instead.
    #[tokio::test]
    async fn an_ask_outside_the_approval_policy_becomes_a_decision_card() {
        let (_tmp, state) = hermetic_state();
        let q = "Needs one thing from you: a semantic-search sign-in so I can validate parity?";
        let (id, created, path) = file_ask_card_via(
            &state, "tubescience-parity", Some(Boundary::OwnerOnly), q, "ctx", "test", AskPath::Decision, false,
        )
        .await
        .unwrap();
        assert!(created);
        assert_eq!(path, AskPath::Decision);
        let conn = state.store.read().unwrap();
        let row = crate::db::board_store::get_issue(&conn, &id).unwrap().unwrap();
        assert_eq!(row.status, "backlog");
        assert_eq!(row.item_type, "decision");
        assert_eq!(row.ask_type, None, "no needsyou ask outside the configured categories");
        assert_eq!(row.decision_question.as_deref(), Some(q));
        assert!(row.decision_rationale.is_some());
        let wait: Value = serde_json::from_str(row.waiting_on.as_deref().unwrap()).unwrap();
        assert_eq!(wait["type"], "credential");
        assert!(!row.tags.iter().any(|t| t == crate::db::board_store::NEEDS_YOU_TAG));
        drop(conn);
        // Dedupe works across both shapes: the same question finds this card.
        let (id2, created2, _) = file_ask_card_via(
            &state, "tubescience-parity", Some(Boundary::OwnerOnly), q, "ctx", "test", AskPath::NeedsYou, false,
        )
        .await
        .unwrap();
        assert_eq!((id2, created2), (id, false));
    }

    // --- Isolated lanes (gs-4-gke-minimization, 2026-09-26) ------------------

    const GS4_FINAL: &str = "Everything else remaining is waiting on your reply to my previous message:\n\n\
        - The six permission lines, plus the info@mixpeek.com re-login.\n\
        - The ten decisions. \"defaults\" accepts all of them.\n\n\
        With those I can drive the rest to completion.";

    #[test]
    fn gs4_still_waiting_for_your_answers_is_an_ask() {
        let t = "Still waiting for your answers: the six permission lines, the re-login, and the ten decisions (\"defaults\" accepts all).";
        assert_ne!(ask(t), OwnerAsk::None, "{:?}", ask(t));
        assert_ne!(ask(GS4_FINAL), OwnerAsk::None, "{:?}", ask(GS4_FINAL));
        // Control: a lane reporting that SOMEONE ELSE is waiting is not an ask.
        assert_eq!(ask("mvs-infra is still waiting for the shard roll to finish."), OwnerAsk::None);
    }

    fn asst_text(uuid: &str, mid: &str, text: &str) -> Value {
        json!({"type":"assistant","uuid":uuid,"message":{"id":mid,"content":[{"type":"text","text":text}]}})
    }

    #[test]
    fn a_pointer_ask_carries_the_earlier_list_it_points_to() {
        let list = "Ten decisions, each with my default:\n\n\
            1. Delete ClickHouse after export (default: yes)\n\
            2. Move web apps to Cloud Run (default: yes)\n\
            3. Drop the load-test namespace (default: yes)\n\
            4. Smaller node pools after grant #1 (default: yes)";
        let recs = vec![
            asst_text("a1", "m1", list),
            asst_text("a2", "m2", "Checked the autoscaler; no scale-down because of disk pinning."),
            asst_text("a3", "m3", GS4_FINAL),
        ];
        let got = earlier_ask_list(&recs, GS4_FINAL).expect("the list is found");
        assert!(got.contains("1. Delete ClickHouse"));
        // A final message that already enumerates its asks needs nothing more.
        assert_eq!(earlier_ask_list(&recs, list), None);
        // No enumerated earlier message: nothing attached rather than a guess.
        let plain = vec![asst_text("b1", "n1", "Working on it."), asst_text("b2", "n2", GS4_FINAL)];
        assert_eq!(earlier_ask_list(&plain, GS4_FINAL), None);
    }

    #[test]
    fn the_list_behind_a_chain_of_pointers_is_the_one_attached() {
        // The live shape, 2026-09-26 20:33-20:44Z: the decisions as a table,
        // then messages that only point back at it.
        let original = "Here is everything I need from you, in one pass.\n\n\
            ## 2. Decisions (reply \"defaults\" to accept all)\n\n\
            | # | Decision | My default |\n|---|---|---|\n\
            | 1 | ClickHouse data: delete its disks | Export first, keep 7 days |\n\
            | 2 | Stored data: delete untouched objects | Scratch buckets only |\n\
            | 3 | Container images: prune old versions | Yes |\n\
            | 10 | Alert email | Keep |";
        let pointer = "Everything else remaining needs your answers from two messages ago:\n\
            1. The six permission lines.\n2. The ten decisions.\n3. The re-login.";
        let recs = vec![
            asst_text("o", "m0", original),
            asst_text("p", "m1", pointer),
            asst_text("f", "m2", GS4_FINAL),
        ];
        let got = earlier_ask_list(&recs, GS4_FINAL).expect("found");
        assert!(got.contains("| 1 | ClickHouse data"), "the table, not the pointer: {got}");
    }

    #[tokio::test]
    async fn an_isolated_lane_gets_one_card_that_is_updated_on_each_rephrasing() {
        let (_tmp, state) = hermetic_state();
        let lane = "gs-4-gke-minimization";
        let (id, created, _) = file_ask_card_via(
            &state, lane, None, "Still waiting for your answers?", "ctx one", "test", AskPath::NeedsYou, true,
        ).await.unwrap();
        assert!(created);
        let (id2, created2, _) = file_ask_card_via(
            &state, lane, None, "Everything else is waiting on your reply?", "ctx two", "test", AskPath::NeedsYou, true,
        ).await.unwrap();
        assert_eq!((id2.as_str(), created2), (id.as_str(), false), "a rephrased repeat updates the open card");
        let conn = state.store.read().unwrap();
        let row = crate::db::board_store::get_issue(&conn, &id).unwrap().unwrap();
        assert_eq!(row.status, "needsyou");
        assert!(row.tags.iter().any(|t| t == ISOLATED_ASK_TAG));
        assert!(row.desc.contains("is isolated"), "{}", row.desc);
        assert!(row.desc.contains("ctx two"), "the update is appended");
        drop(conn);
        // A NON-isolated lane never merges different questions.
        let (a, _, _) = file_ask_card_via(&state, "peer", None, "First ask?", "c", "test", AskPath::NeedsYou, false).await.unwrap();
        let (b, created_b, _) = file_ask_card_via(&state, "peer", None, "Second ask?", "c", "test", AskPath::NeedsYou, false).await.unwrap();
        assert!(created_b && a != b);
    }

    #[tokio::test]
    async fn claim_once_acts_exactly_once_per_key() {
        let (_tmp, state) = hermetic_state();
        assert!(claim_once(&state, "lane", "turn_end.owner_ask", "k1".into(), json!({})).await);
        assert!(!claim_once(&state, "lane", "turn_end.owner_ask", "k1".into(), json!({})).await);
        assert!(claim_once(&state, "lane", "turn_end.owner_ask", "k2".into(), json!({})).await);
    }

    #[test]
    fn chat_workers_are_left_alone_and_coding_workers_are_not() {
        assert!(skips_turn_end_classifier("chat"));
        assert!(!skips_turn_end_classifier("coding"));
        assert!(!skips_turn_end_classifier(""));
    }

    #[test]
    fn a_quoted_ask_keeps_its_command_and_casing() {
        let t = "The CLI sync works.\n\nStill waiting on you: `! ~/.amux/seed-standing-approvals.sh`, to record the two standing approvals.";
        match ask(t) {
            OwnerAsk::InBoundary { sentence } | OwnerAsk::Boundary { sentence, .. } => {
                assert!(sentence.starts_with("Still waiting on you:"), "{sentence}");
                assert!(sentence.contains("seed-standing-approvals.sh"), "{sentence}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// AMUX-5277: the exact sentences tubescience-parity looped on. Both were
    /// steered "proceed" (12:41:27Z, 12:42:22Z); a sign-in only the owner can
    /// type is OwnerOnly, so it becomes a card and is never steered.
    #[test]
    fn tp37_sign_in_at_a_url_is_an_owner_only_ask() {
        let tp37 = "The results still don't match production. The only thing left is TP-37: sign in once at https://semantic-search-tawny.vercel.app in the `ethan-tubescience` Chrome profile.";
        assert_eq!(boundary(tp37), Some(Boundary::OwnerOnly));
        let listed = "The results still don't match production. The semantic-search token still returns 401, and TP-37 is still waiting on you.\n\n\
            1. **TP-37:** sign in at https://semantic-search-tawny.vercel.app in the `ethan-tubescience` Chrome profile.\n\
            2. **gs-3:** decide what happens to commit 804e1a5 on TubeScience's GitHub: push it, rebase it, or close it.";
        assert_eq!(boundary(listed), Some(Boundary::OwnerOnly));
        // First person is a plan, not an ask.
        assert_eq!(ask("Token refreshed. I'll sign in at https://x.example once the build is green."), OwnerAsk::None);
    }

    #[tokio::test]
    async fn the_in_boundary_steer_is_deduped_per_question_for_six_hours_not_per_turn() {
        let (_tmp, state) = hermetic_state();
        let key = question_key("That repo is outside my lane, so it's your call.");
        let claim = |uuid: &'static str| {
            let (state, key) = (state.clone(), key.clone());
            async move {
                claim_within(&state, "tubescience-parity", "turn_end.owner_ask_steer", &key, OWNER_ASK_STEER_WINDOW_S,
                    format!("owner-ask:tubescience-parity:{uuid}"), json!({"uuid": uuid})).await
            }
        };
        assert!(claim("turn-1").await, "the first steer goes out");
        assert!(!claim("turn-2").await, "a new turn with the same ask is not steered again");
        // Another lane, or another question, is its own claim.
        assert!(claim_within(&state, "other-lane", "turn_end.owner_ask_steer", &key, OWNER_ASK_STEER_WINDOW_S,
            "owner-ask:other-lane:t".into(), json!({})).await);
        assert!(claim_within(&state, "tubescience-parity", "turn_end.owner_ask_steer", &question_key("Want me to rerun it?"),
            OWNER_ASK_STEER_WINDOW_S, "owner-ask:tubescience-parity:turn-3".into(), json!({})).await);
        // Outside the window the same question may be steered again.
        assert!(claim_within(&state, "tubescience-parity", "turn_end.owner_ask_steer", &key, -1.0,
            "owner-ask:tubescience-parity:turn-4".into(), json!({})).await);
    }

    #[test]
    fn a_report_bullet_above_a_standalone_ask_does_not_set_its_boundary() {
        // Live, amux-meta-helper 2026-09-27 19:29 (AMH-17).
        let t = "Needs your call:\n- Substantive production/policy decisions (19): MVS production promote, a CI merge-gate policy change, and similar calls.\n\nGiven the size, want me to keep this as a reference list, or would it help more to work through one category at a time (starting with the urgent security ones)?";
        assert!(matches!(classify_owner_ask(t), OwnerAsk::InBoundary { .. }));
        // A short ask still leans on the sentence above it, even across a blank line.
        let t = "I can truncate the prod events table now.\n\nWant me to do it?";
        assert!(matches!(classify_owner_ask(t), OwnerAsk::Boundary { .. }));
        let t = "I can truncate the prod events table now. Should I go ahead with that cleanup before the backfill job runs tonight?";
        assert!(matches!(classify_owner_ask(t), OwnerAsk::Boundary { .. }));
    }

    #[test]
    fn owner_policy_is_owner_configuration_and_reads_clean() {
        assert!(sv::is_owner_configured_guard(OWNER_POLICY_GUARD));
        assert!(!sv::is_owner_configured_guard("owner-ask"));
        let t = policy_text("Want me to start on 1 and 2?");
        assert!(t.contains("Want me to start on 1 and 2?") && !t.contains('\u{2014}'));
        // The live in-boundary specimens stay in-boundary, the boundary ones do not.
        assert!(matches!(classify_owner_ask("Done.\n\nWant me to start on 1 and 2?"), OwnerAsk::InBoundary { .. }));
        assert!(matches!(classify_owner_ask("Say the word and I'll update in one pass."), OwnerAsk::InBoundary { .. }));
        assert!(!matches!(classify_owner_ask("Approve the welcome email to partners@thefasttrackgirl.com?"), OwnerAsk::InBoundary { .. }));
        assert_eq!(boundary("Want me to send the welcome email to partners@thefasttrackgirl.com?"), Some(Boundary::ExternalSend));
        assert_eq!(boundary("Shall I post the 15 drafts to LinkedIn?"), Some(Boundary::ExternalSend));
        assert_eq!(boundary("Want me to reply to the thread?"), Some(Boundary::ExternalSend));
    }

    #[test]
    fn rephrased_repeats_are_one_ask_and_different_asks_are_not() {
        let spend = [
            "May tubescience-parity proceed with this: The only remaining step needs your OK: about $16 of Gemini embedding?",
            "May tubescience-parity proceed with this: I'm waiting for your OK on about $16 of Gemini embedding: roughly $15 to vector the ~750 remaining scenes and under $1 for search text on the 1,116 new ones?",
            "May tubescience-parity proceed with this: Still waiting on your go for about $16 of Gemini embedding?",
            "May tubescience-parity proceed with this: I'm still blocked on your OK to spend about $16 on Gemini embedding?",
            "May tubescience-parity proceed with this: Still blocked on your OK to spend about $16 on Gemini embedding?",
        ];
        let signin = [
            "May tubescience-parity proceed with this: Still blocked on your sign-in (TP-37) and the gs-3 push decision?",
            "May tubescience-parity proceed with this: Still blocked on your sign-in (TP-37) and the gs-3 decision?",
        ];
        for a in &spend { for b in &spend { assert!(same_ask(a, b), "should merge:\n{a}\n{b}"); } }
        for a in &signin { for b in &signin { assert!(same_ask(a, b), "should merge:\n{a}\n{b}"); } }
        for a in &spend { for b in &signin { assert!(!same_ask(a, b), "must stay apart:\n{a}\n{b}"); } }
        assert!(!same_ask("Want me to start on 1 and 2?", "Approve the welcome email to partners@thefasttrackgirl.com?"));
    }

    #[test]
    fn steer_text_carries_no_em_dash() {
        assert!(!steer_text("Say go and I'll do it.").contains('\u{2014}'));
    }

    #[test]
    fn a_sentence_naming_a_needsyou_card_is_not_a_new_ask() {
        let state = crate::api::standing_approvals::tests::test_state();
        state
            .store
            .write(|conn| {
                for (id, st) in [("AH-296", "needsyou"), ("AH-297", "todo")] {
                    conn.execute(
                        "INSERT INTO issues (id, title, status, created, updated) VALUES (?1, ?1, ?2, 1, 1)",
                        rusqlite::params![id, st],
                    )?;
                }
                Ok(crate::db::WriteOutcome { applied: false, events: vec![] })
            })
            .unwrap();
        assert_eq!(
            needsyou_card_named(&state, "AH-296, full scope versus Sunday, is still waiting for you."),
            Some("AH-296".to_string())
        );
        assert_eq!(needsyou_card_named(&state, "AH-297 is ready; want me to start it?"), None,
            "a card that is not waiting on the owner does not suppress the steer");
    }

    #[test]
    fn a_decision_the_lane_reserves_for_the_owner_is_never_steered_to_proceed() {
        for text in [
            "If plan items are under 1.5/h, I'll tell you. That's when to decide between cutting scope and moving the date, which is your call, not the orchestrator's.",
            "How it happened: I had told you that narrowing scope versus moving the date was your call.",
            "Keep full scope or keep Sunday? That one is up to you.",
        ] {
            if let OwnerAsk::InBoundary { sentence } = classify_owner_ask(text) {
                panic!("steered to proceed: {sentence}")
            }
        }
        assert!(boundary_of("which is your call, not the orchestrator's.").is_some(), "direct boundary_of");
        assert!(boundary_of("want me to rerun the flaky test?").is_none(),
            "an ordinary in-lane ask stays in boundary");
    }
}
