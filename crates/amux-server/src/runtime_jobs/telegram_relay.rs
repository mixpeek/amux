//! Auto-relay: when sessions reply to Telegram-routed messages, send replies back to Telegram.
//!
//! # Design
//!
//! For each session with an active Telegram mapping:
//! 1. Peek its output (last 300 lines)
//! 2. Find the LAST `[from Telegram @...]` message in the pane
//! 3. Extract all NEW text after that message (using line number checkpoint)
//! 4. Send it back to Telegram with HTML formatting
//! 5. Update checkpoint so we don't send dupes
//!
//! Runs every 30 seconds. No state in-process; everything checkpointed in DB.
//! Failures are logged but never propagated — a Telegram send error doesn't kill the relay job
//! or block other sessions. The user can always resend from the session via curl if needed.

use super::registry;
use crate::api::sessions_legacy::{is_chrome_line, strip_ansi};
use crate::api::session_verbs::tmux_capture;
use crate::api::AppState;
use crate::db::telegram as tg_db;
use std::time::Duration;

const JOB: &str = super::registry::ids::TELEGRAM_RELAY;

pub async fn run(state: AppState) {
    loop {
        registry::tick(JOB);
        if let Err(e) = relay_cycle(&state).await {
            tracing::warn!("telegram_relay: scan error: {e}");
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

async fn relay_cycle(state: &AppState) -> Result<(), String> {
    // Get all Telegram mappings (which sessions are linked to which chats)
    let mappings = {
        let conn = state.store.read().map_err(|e| e.to_string())?;
        tg_db::list(&conn).map_err(|e| e.to_string())?
    };

    for mapping in &mappings {
        if let Err(e) = check_and_relay(state, mapping).await {
            // Log but continue — don't let one session's error block others
            tracing::debug!(
                "telegram_relay: chat {} (watching '{}'): {}",
                mapping.chat_id, mapping.routed_session(), e
            );
        }
    }

    Ok(())
}

/// Pure text half of the relay: given a session's raw pane capture (as
/// `tmux capture-pane -e` returns it, ANSI intact), finds the LAST
/// `[from Telegram @...]` marker and returns the cleaned reply text after it,
/// plus the total line count (the checkpoint `check_and_relay` records via
/// `mark_relayed`). Kept separate from `check_and_relay` — which also does
/// real tmux I/O and a real HTTP send — specifically so this half is
/// unit-testable against a captured incident shape, not just against the
/// live pane.
///
/// `tmux capture-pane -e` preserves raw ANSI escape sequences — needed
/// elsewhere for color-aware rendering, but Telegram has no ANSI renderer, so
/// left in place they show up as literal garbage ("[38;5;231m..."). Strip
/// ONCE, up front, so both the marker search and the reply extraction work on
/// the same clean text. The lines after the marker are then filtered through
/// `is_chrome_line` — the same predicate `preview_of` uses to build session
/// previews — to drop TUI chrome: the bottom status bar ("bypass
/// permissions... shift+tab to cycle"), box-drawing dividers, and other
/// terminal furniture a live pane always has BELOW the actual reply text.
///
/// Found live 2026-08-30: raw escape codes AND leaked TUI chrome both reached
/// Telegram verbatim before either strip existed.
fn extract_reply(raw_output: &str) -> Option<(String, i64)> {
    let output = strip_ansi(raw_output);
    let lines: Vec<&str> = output.lines().collect();

    // Find the LAST `[from Telegram @...]` line
    let tg_idx = lines.iter().rposition(|line| line.contains("[from Telegram @"))?;

    // Extract new output lines AFTER the Telegram message, dropping chrome.
    let reply_lines: Vec<&str> =
        lines[(tg_idx + 1)..].iter().map(|s| s.trim()).filter(|s| !is_chrome_line(s)).collect();

    if reply_lines.is_empty() {
        return None;
    }
    Some((reply_lines.join("\n"), lines.len() as i64))
}

async fn check_and_relay(state: &AppState, mapping: &tg_db::TelegramMapping) -> Result<(), String> {
    // Watch wherever the chat's LAST inbound message actually routed to — the
    // `/link`'d default, or an `@lane` target (migration 0040). Reading
    // `mapping.session` directly here is the exact bug found 2026-08-30:
    // `@frontstage status` runs frontstage correctly, but a relay pinned to
    // the static default never looks at frontstage's pane, so the reply is
    // never seen and Telegram gets no feedback at all.
    let watch_session = mapping.routed_session();
    let raw_output = tmux_capture(watch_session, 300).await;

    let Some((reply_text, last_line)) = extract_reply(&raw_output) else {
        // Either no Telegram message in this session's pane yet, or no new
        // (non-chrome) output since it — session probably still working.
        return Ok(());
    };

    // DEDUP GATE (2026-08-30): a content hash of the extracted reply, not a
    // line-number checkpoint. `last_relayed_line` looked like a checkpoint
    // and was never one — it was write-only, nothing ever read it back, so
    // this job resent the SAME reply every 30s tick for as long as it stayed
    // the newest thing in the watched pane (found live, reported directly:
    // "I get answers multiple times"). If this reply hashes the same as the
    // last one actually sent to this chat, there is nothing new to say.
    let reply_hash = content_hash(&reply_text);
    if mapping.last_relayed_hash.as_deref() == Some(reply_hash.as_str()) {
        return Ok(());
    }

    // Convert markdown to Telegram HTML
    let html = markdown_to_html(&reply_text);

    // Try sending; fall back to plain text on format error
    match send_reply_to_telegram(mapping.chat_id, &html, Some("HTML")).await {
        Ok(_) => {
            // Record success (the dedup hash, plus the capture depth as an
            // informational breadcrumb only — see mark_relayed's doc).
            let _ = state
                .store
                .write_async({
                    let chat_id = mapping.chat_id;
                    let hash = reply_hash.clone();
                    move |conn| {
                        tg_db::mark_relayed(conn, chat_id, last_line, &hash)?;
                        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                    }
                })
                .await;
            tracing::info!(
                "telegram_relay: relayed {} bytes from '{}' to chat {}",
                html.len(),
                watch_session,
                mapping.chat_id
            );
            Ok(())
        }
        Err(e) => {
            // Record error (for observability), but don't fail the job
            let _ = state
                .store
                .write_async({
                    let chat_id = mapping.chat_id;
                    let err_msg = e.clone();
                    move |conn| {
                        let _ = tg_db::mark_relay_error(conn, chat_id, &err_msg);
                        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                    }
                })
                .await;

            // On HTML format error, try plain text
            if e.contains("parse entities") {
                tracing::warn!("telegram_relay: HTML format failed, retrying as plain text for chat {}", mapping.chat_id);
                let plain = strip_html(&html);
                match send_reply_to_telegram(mapping.chat_id, &plain, None).await {
                    Ok(_) => {
                        // Update the dedup gate even though we downgraded to plain text —
                        // the underlying reply content, and thus its hash, is unchanged.
                        let _ = state
                            .store
                            .write_async({
                                let chat_id = mapping.chat_id;
                                let hash = reply_hash.clone();
                                move |conn| {
                                    tg_db::mark_relayed(conn, chat_id, last_line, &hash)?;
                                    Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                                }
                            })
                            .await;
                        return Ok(());
                    }
                    Err(e2) => return Err(format!("plain text also failed: {e2}")),
                }
            }

            Err(e)
        }
    }
}

/// Stable content hash for the dedup gate — deliberately SHA-256 (already a
/// dependency, used the same way elsewhere in this crate), not
/// `std::hash::Hash`/`DefaultHasher`: the stdlib hasher's algorithm is
/// explicitly NOT guaranteed stable across compiler/std versions, and this
/// hash is persisted to SQLite and compared across server restarts — which
/// happen on every commit-triggered deploy via the auto-builder. A hasher
/// that quietly changed algorithms would reset every chat's dedup state on
/// the next deploy, at best causing one stray duplicate per chat rather than
/// a hard bug, but there's no reason to accept even that when a stable hash
/// is one dependency already in Cargo.toml away.
fn content_hash(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// Send text to a Telegram chat. Reuses the existing send_message logic from telegram_poll.rs
/// to ensure consistent behavior (including parse_mode support and fallback).
async fn send_reply_to_telegram(
    chat_id: i64,
    text: &str,
    parse_mode: Option<&str>,
) -> Result<(), String> {
    // Use the same send_message from telegram_poll
    crate::runtime_jobs::telegram_poll::send_message(&bot_token()?, chat_id, text, parse_mode).await
}

fn bot_token() -> Result<String, String> {
    std::env::var("TELEGRAM_BOT_TOKEN")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "TELEGRAM_BOT_TOKEN not set".to_string())
}

/// Markdown → Telegram HTML converter, subset matching what
/// `send_message`'s doc comment says Telegram itself accepts: bold, italic,
/// strikethrough, inline code, fenced code blocks (`<pre>`), links,
/// blockquotes, GFM tables (rendered as a monospace `<pre>` block — Telegram
/// has no `<table>` entity, so column-aligned monospace is the closest
/// available approximation), and ATX headings (`#`..`######`, rendered
/// `<b>bold</b>` — Telegram has no `<h1>`..`<h6>` either). Native
/// lists pass through as literal text unchanged, which already renders fine
/// (Telegram doesn't need special markup for a leading "- " or "1. ").
///
/// Fenced ```blocks``` and tables are found on a LINE basis first (Telegram's
/// own entity model is line-oriented for `<pre>`/`<blockquote>`), then each
/// remaining non-fenced, non-quote, non-table, non-heading line runs through
/// [`convert_inline`] for `**bold**`/`*italic*`/`~~strike~~`/`` `code` ``/
/// `[text](url)`. This two-pass split is why triple backticks used to
/// corrupt: the old single-pass char-walker toggled `<code>` three times per
/// fence line (open/close/open), leaving an empty `<code></code>` pair and
/// the language tag bleeding into the visible block instead of one clean
/// `<pre>`.
fn markdown_to_html(md: &str) -> String {
    let mut result = String::with_capacity(md.len() * 2);
    let mut in_fence = false;
    let mut quote_buf: Vec<String> = Vec::new();

    let flush_quote = |buf: &mut Vec<String>, out: &mut String| {
        if buf.is_empty() {
            return;
        }
        out.push_str("<blockquote>");
        out.push_str(&buf.join("\n"));
        out.push_str("</blockquote>\n");
        buf.clear();
    };

    // Indexed rather than `for line in md.split('\n')`: a table's shape
    // (header row, then a separator row of `---`/`:--:`) can only be
    // recognized by looking one line AHEAD, which an iterator-per-line loop
    // can't do without re-buffering.
    let lines: Vec<&str> = md.split('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            flush_quote(&mut quote_buf, &mut result);
            if in_fence {
                result.push_str("</pre>\n");
            } else {
                result.push_str("<pre>");
            }
            in_fence = !in_fence;
            i += 1;
            continue;
        }
        if in_fence {
            // Inside a fence: raw text, only HTML-escaped, no inline markdown.
            result.push_str(&escape_html(line));
            result.push('\n');
            i += 1;
            continue;
        }
        if let Some(quoted) = line.strip_prefix("> ").or_else(|| line.strip_prefix(">")) {
            quote_buf.push(convert_inline(quoted));
            i += 1;
            continue;
        }
        flush_quote(&mut quote_buf, &mut result);
        if trimmed.contains('|') && i + 1 < lines.len() && is_table_separator(lines[i + 1]) {
            let (table_html, consumed) = render_table(&lines[i..]);
            result.push_str(&table_html);
            i += consumed;
            continue;
        }
        if let Some(heading) = parse_heading(trimmed) {
            result.push_str("<b>");
            result.push_str(&convert_inline(heading));
            result.push_str("</b>\n");
            i += 1;
            continue;
        }
        result.push_str(&convert_inline(line));
        result.push('\n');
        i += 1;
    }
    flush_quote(&mut quote_buf, &mut result);
    if in_fence {
        // Unclosed fence in the source: close it rather than leave a
        // dangling `<pre>` that makes Telegram reject the whole message.
        result.push_str("</pre>\n");
    }
    // The line-based loop adds a trailing '\n' that split('\n') didn't have
    // when `md` itself had none — match the input's own trailing newline
    // convention rather than always appending one.
    if !md.ends_with('\n') && result.ends_with('\n') {
        result.pop();
    }

    result
}

/// ATX heading (`# text` through `###### text`, standard markdown — a `#`
/// followed by 1-6 hashes total and a space before the text). Deliberately
/// requires the space: Claude's replies use bare `#123` (issue refs) and
/// `#deploy`-style hashtag-looking text often enough that treating every
/// leading `#` as a heading would be the more common false positive.
fn parse_heading(trimmed: &str) -> Option<&str> {
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    rest.strip_prefix(' ').map(|s| s.trim_start())
}

/// Is `line` a GFM table separator row — cells of only `-` (optionally
/// bracketed by `:` for alignment), e.g. `|---|:--:|--:|` or `---|---`? This
/// is the ONE unambiguous signal that the line above it is a table header
/// (plain text can contain `|` for all sorts of non-table reasons — a pipe
/// used as a visual separator, shell output, etc. — so detection anchors on
/// the separator row, never on `|` alone).
fn is_table_separator(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    let inner = t.trim_start_matches('|').trim_end_matches('|');
    if inner.trim().is_empty() {
        return false;
    }
    inner.split('|').all(|cell| {
        let c = cell.trim();
        if c.is_empty() {
            return false;
        }
        let core = c.trim_start_matches(':').trim_end_matches(':');
        !core.is_empty() && core.chars().all(|ch| ch == '-')
    })
}

/// Splits one table row into its cells, tolerating an optional leading and
/// trailing `|` (GFM allows `| a | b |` and bare `a | b` alike).
fn parse_table_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let inner = t.trim_start_matches('|').trim_end_matches('|');
    inner.split('|').map(|c| c.trim().to_string()).collect()
}

/// Renders a GFM table (`lines[0]` = header, `lines[1]` = separator,
/// `lines[2..]` = data rows until a blank line or a line with no `|`) as a
/// column-aligned monospace `<pre>` block — the closest Telegram gets to a
/// real table, since it has no `<table>` entity at all. Returns the HTML and
/// how many lines of `lines` were consumed, so the caller's index can skip
/// past the whole block in one step.
///
/// Column widths come from the DATA (`chars().count()` on each cell, before
/// escaping), not from the source separator row's dash count — Claude's own
/// tables are rarely pre-aligned, and re-deriving the widths is what makes
/// the monospace block actually line up instead of reproducing the source's
/// misalignment verbatim.
fn render_table(lines: &[&str]) -> (String, usize) {
    let header = parse_table_row(lines[0]);
    let ncols = header.len().max(1);
    let mut rows: Vec<Vec<String>> = vec![header];
    let mut consumed = 2; // header + separator row
    let mut idx = 2;
    while idx < lines.len() {
        let t = lines[idx].trim();
        if t.is_empty() || !t.contains('|') {
            break;
        }
        rows.push(parse_table_row(lines[idx]));
        consumed += 1;
        idx += 1;
    }

    let mut widths = vec![1usize; ncols];
    for row in &rows {
        for (c, cell) in row.iter().enumerate().take(ncols) {
            widths[c] = widths[c].max(cell.chars().count());
        }
    }

    let render_row = |row: &[String], out: &mut String| {
        for (c, &w) in widths.iter().enumerate() {
            let cell = row.get(c).map(String::as_str).unwrap_or("");
            let pad = w.saturating_sub(cell.chars().count());
            out.push_str(&escape_html(cell));
            out.push_str(&" ".repeat(pad));
            if c + 1 < ncols {
                out.push_str(" | ");
            }
        }
    };

    let mut out = String::from("<pre>");
    for (ri, row) in rows.iter().enumerate() {
        let mut line_out = String::new();
        render_row(row, &mut line_out);
        out.push_str(line_out.trim_end());
        out.push('\n');
        if ri == 0 {
            // Separator, regenerated to the REAL widths above rather than
            // copied from the source — the source's dash run only had to
            // satisfy `is_table_separator`, not match the data's width.
            for (c, &w) in widths.iter().enumerate() {
                out.push_str(&"-".repeat(w));
                if c + 1 < ncols {
                    out.push_str("-+-");
                }
            }
            out.push('\n');
        }
    }
    out.push_str("</pre>\n");
    (out, consumed)
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Inline formatting for one non-fenced, non-quote line.
fn convert_inline(md: &str) -> String {
    let mut result = String::with_capacity(md.len() * 2);
    let mut chars = md.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '`' => {
                // Inline `code`: escape entities inside, same as a fence.
                // Unterminated still closes honestly (best-effort, matches
                // the original converter's behavior) rather than leaving a
                // dangling `<code>` that would make Telegram reject the
                // whole message.
                result.push_str("<code>");
                for c in chars.by_ref() {
                    if c == '`' {
                        break;
                    }
                    result.push_str(&escape_html(&c.to_string()));
                }
                result.push_str("</code>");
            }
            '*' if chars.peek() == Some(&'*') => {
                // **bold**
                chars.next();
                result.push_str("<b>");
                #[allow(clippy::while_let_on_iterator)]
                while let Some(c) = chars.next() {
                    if c == '*' && chars.peek() == Some(&'*') {
                        chars.next();
                        break;
                    }
                    result.push_str(&escape_html(&c.to_string()));
                }
                result.push_str("</b>");
            }
            '*' => {
                // *italic*
                result.push_str("<i>");
                for c in chars.by_ref() {
                    if c == '*' {
                        break;
                    }
                    result.push_str(&escape_html(&c.to_string()));
                }
                result.push_str("</i>");
            }
            '~' if chars.peek() == Some(&'~') => {
                // ~~strikethrough~~
                chars.next();
                result.push_str("<s>");
                #[allow(clippy::while_let_on_iterator)]
                while let Some(c) = chars.next() {
                    if c == '~' && chars.peek() == Some(&'~') {
                        chars.next();
                        break;
                    }
                    result.push_str(&escape_html(&c.to_string()));
                }
                result.push_str("</s>");
            }
            '[' => {
                // [text](url) — only consumed as a link when the full
                // pattern is present; otherwise the '[' is literal (Claude's
                // replies use bare brackets often enough, e.g. "[done]",
                // that guessing wrong here would be the more common bug).
                let rest: String = chars.clone().collect();
                if let Some((label, url, consumed)) = try_parse_link(&rest) {
                    result.push_str("<a href=\"");
                    result.push_str(&escape_html(&url).replace('"', "&quot;"));
                    result.push_str("\">");
                    result.push_str(&escape_html(&label));
                    result.push_str("</a>");
                    for _ in 0..consumed {
                        chars.next();
                    }
                } else {
                    result.push('[');
                }
            }
            _ => result.push_str(&escape_html(&ch.to_string())),
        }
    }
    result
}

/// Parses a `text](url)` tail (the `[` itself already consumed by the
/// caller) into `(label, url, chars_consumed)`. Returns `None` if `rest`
/// isn't actually a well-formed link — no nested `[`/`]`, matching a real
/// markdown link but not e.g. `[note]` followed unrelatedly by `(parens)`
/// later in the line.
fn try_parse_link(rest: &str) -> Option<(String, String, usize)> {
    let close_bracket = rest.find(']')?;
    let label = &rest[..close_bracket];
    if label.contains('[') {
        return None;
    }
    let after = &rest[close_bracket + 1..];
    let open_paren = after.strip_prefix('(')?;
    let close_paren = open_paren.find(')')?;
    let url = &open_paren[..close_paren];
    if url.contains('(') || url.contains(')') {
        return None;
    }
    // consumed = everything up to and including the ')', counted in chars
    // of `rest` (the caller already consumed the leading '[').
    let consumed = rest[..close_bracket + 1 + 1 + close_paren + 1].chars().count();
    Some((label.to_string(), url.to_string(), consumed))
}

fn strip_html(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;

    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }

    result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    registry::spawn_loop(JOB, Some(Duration::from_secs(30)), run(state))
}

#[cfg(test)]
mod tests {
    use super::{content_hash, extract_reply, markdown_to_html};

    /// The bug this module used to have: a single-pass char walker toggled
    /// `<code>` on every backtick, so a fenced block's opening ``` produced
    /// an empty `<code></code>` pair and the language tag ("rust") bled into
    /// the visible text instead of becoming one clean `<pre>` block.
    #[test]
    fn fenced_code_block_becomes_one_pre_not_three_toggled_code_tags() {
        let md = "before\n```rust\nfn main() {}\n```\nafter";
        let html = markdown_to_html(md);
        assert_eq!(html, "before\n<pre>fn main() {}\n</pre>\nafter");
    }

    #[test]
    fn fenced_block_html_escapes_but_does_not_apply_inline_markdown() {
        let md = "```\n*not italic* & <tag>\n```";
        let html = markdown_to_html(md);
        assert_eq!(html, "<pre>*not italic* &amp; &lt;tag&gt;\n</pre>");
    }

    #[test]
    fn unclosed_fence_still_closes_the_pre_tag() {
        let html = markdown_to_html("```\nno closing fence");
        assert!(html.ends_with("</pre>\n") || html.ends_with("</pre>"), "{html:?}");
        assert!(html.contains("no closing fence"), "{html:?}");
    }

    #[test]
    fn bold_italic_and_inline_code_still_work() {
        assert_eq!(markdown_to_html("**bold** and *italic* and `code`"), "<b>bold</b> and <i>italic</i> and <code>code</code>");
    }

    #[test]
    fn strikethrough_converts_to_s_tag() {
        assert_eq!(markdown_to_html("~~gone~~"), "<s>gone</s>");
    }

    #[test]
    fn markdown_link_becomes_anchor() {
        assert_eq!(
            markdown_to_html("see [the PR](https://github.com/mixpeek/amux/pull/170) for details"),
            "see <a href=\"https://github.com/mixpeek/amux/pull/170\">the PR</a> for details"
        );
    }

    #[test]
    fn bare_bracket_without_a_link_is_left_alone() {
        // Claude's own replies say things like "[done]" or "status: [ok]"
        // often enough that guessing these are broken links would be the
        // more common failure.
        assert_eq!(markdown_to_html("task [done]"), "task [done]");
    }

    #[test]
    fn blockquote_lines_group_into_one_blockquote() {
        assert_eq!(
            markdown_to_html("> first\n> second\nafter"),
            "<blockquote>first\nsecond</blockquote>\nafter"
        );
    }

    #[test]
    fn ampersand_and_angle_brackets_outside_any_span_are_escaped() {
        assert_eq!(markdown_to_html("a < b && b > c"), "a &lt; b &amp;&amp; b &gt; c");
    }

    /// Reproduces the incident's own artifact (ethos rule 7: test against
    /// what actually happened, not a convenient shape) — a capture built from
    /// the live `@frontstage status` failure reported 2026-08-30: raw SGR
    /// escape sequences interleaved with the reply text, then TUI chrome
    /// (bypass-permissions status bar, box-drawing dividers, a session-link
    /// card) filling the rest of the 300-line capture below it, exactly as
    /// `tmux capture-pane -e` returns a live Claude Code pane.
    #[test]
    fn strips_ansi_and_chrome_but_keeps_the_real_reply() {
        let raw = "\
> @frontstage status
[from Telegram @ivomarino]: @frontstage status
\u{1b}[38;5;231m\u{1b}[49m\u{1b}[39m\u{1b}[1mFrame Phase 1 Status Update:\u{1b}[0m
\u{1b}[32m✅\u{1b}[0m \u{1b}[1mCode Review Complete\u{1b}[0m — All Markdown files reviewed
\u{1b}[1mKey Finding:\u{1b}[0m Phase 1 is deployment + compliance, NOT development.
- Node.js Express app is 100% complete and ready to deploy

────────────────────────────────────────────
────────────────────────────────────────────
────────────────────────────────────────────

 \u{1b}[39m  \u{1b}[38;5;211m⏵⏵ bypass permissions on \u{1b}[38;5;246m(shift+tab to cycle) · ← for agents \u{1b}[39m
\u{1b}[38;5;114m ]8;id=fpvemu;https://claude.ai/code/session_01WvxNFGUdXVcHPpuuGHTvEJ?from=cli\u{1b}\\/rc \u{1b}[39m]8;;\u{1b}\\
Claude Code
A shared Claude Code session on claude.ai/code";

        let (reply, last_line) = extract_reply(raw).expect("a reply was present");

        // The garbage that was reaching Telegram verbatim must be gone.
        assert!(!reply.contains('\u{1b}'), "raw ANSI escape byte leaked: {reply:?}");
        assert!(!reply.contains("[38;5;"), "an escape sequence's tail leaked as literal text: {reply:?}");
        assert!(!reply.contains("bypass permissions"), "status-bar chrome leaked: {reply:?}");
        assert!(!reply.contains("shift+tab"), "status-bar chrome leaked: {reply:?}");
        assert!(!reply.contains("────"), "a box-drawing divider row leaked: {reply:?}");
        assert!(!reply.contains("claude.ai/code/session_"), "the session-link card leaked: {reply:?}");
        assert!(
            !reply.lines().any(|l| l.trim() == "Claude Code"),
            "the session-link card's standalone heading leaked: {reply:?}"
        );

        // The actual reply content must survive, readable.
        assert!(reply.contains("Frame Phase 1 Status Update:"), "{reply:?}");
        assert!(reply.contains("Code Review Complete"), "{reply:?}");
        assert!(reply.contains("Node.js Express app is 100% complete"), "{reply:?}");

        assert_eq!(last_line, raw.lines().count() as i64);
    }

    #[test]
    fn no_marker_in_pane_means_nothing_to_relay() {
        assert!(extract_reply("some unrelated pane output\nwith no telegram marker at all").is_none());
    }

    #[test]
    fn marker_with_no_new_output_yet_means_nothing_to_relay() {
        // The session is still working: the marker is the LAST real content,
        // trailing chrome does not count as "a reply".
        let raw = "[from Telegram @ivomarino]: @frontstage status\n\
                   \u{1b}[39m  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents \u{1b}[39m";
        assert!(extract_reply(raw).is_none());
    }

    #[test]
    fn only_the_last_marker_counts_when_a_chat_has_multiple_rounds() {
        let raw = "[from Telegram @ivomarino]: first question\n\
                   first answer, already relayed\n\
                   [from Telegram @ivomarino]: second question\n\
                   second answer, this is the new one";
        let (reply, _) = extract_reply(raw).expect("a reply was present");
        assert_eq!(reply, "second answer, this is the new one");
        assert!(!reply.contains("first answer"), "{reply:?}");
    }

    // ── Tables ───────────────────────────────────────────────────────────

    #[test]
    fn gfm_table_becomes_a_column_aligned_pre_block() {
        let md = "| Site | Status |\n\
                   |------|--------|\n\
                   | northstage.io | live |\n\
                   | floads.io | live |";
        let html = markdown_to_html(md);
        assert_eq!(
            html,
            "<pre>Site          | Status\n\
             --------------+-------\n\
             northstage.io | live\n\
             floads.io     | live\n\
             </pre>"
        );
    }

    #[test]
    fn table_columns_widen_to_the_longest_cell_not_the_source_dash_count() {
        // The source separator only had to satisfy is_table_separator (any
        // run of dashes) — the rendered widths must come from the DATA, or a
        // short `---` under a long header would misalign every row.
        let md = "| a | b |\n|-|-|\n| short | a much longer cell |";
        let html = markdown_to_html(md);
        assert_eq!(html, "<pre>a     | b\n------+-------------------\nshort | a much longer cell\n</pre>");
    }

    #[test]
    fn table_cell_content_is_html_escaped() {
        let md = "| x |\n|---|\n| <b>&amp;</b> |";
        let html = markdown_to_html(md);
        assert!(html.contains("&lt;b&gt;&amp;amp;&lt;/b&gt;"), "{html:?}");
    }

    #[test]
    fn a_pipe_with_no_separator_row_is_not_treated_as_a_table() {
        // Plain text containing '|' (shell output, a visual separator) must
        // not be mistaken for a table just because it has a pipe character —
        // detection anchors on the separator ROW, not on '|' alone.
        let md = "usage: foo | bar | baz";
        assert_eq!(markdown_to_html(md), "usage: foo | bar | baz");
    }

    #[test]
    fn table_stops_at_a_blank_line() {
        let md = "| a |\n|---|\n| 1 |\n\nafter the table";
        let html = markdown_to_html(md);
        assert!(html.ends_with("</pre>\n\nafter the table"), "{html:?}");
    }

    // ── Headings ─────────────────────────────────────────────────────────

    #[test]
    fn atx_headings_become_bold() {
        assert_eq!(markdown_to_html("# Title"), "<b>Title</b>");
        assert_eq!(markdown_to_html("### Sub-heading"), "<b>Sub-heading</b>");
    }

    #[test]
    fn heading_text_still_gets_inline_formatting() {
        assert_eq!(markdown_to_html("## **Frame** Phase 1"), "<b><b>Frame</b> Phase 1</b>");
    }

    #[test]
    fn hash_with_no_space_is_not_a_heading() {
        // Claude's replies say "#123" (issue refs) and "#deploy" often enough
        // that guessing every leading '#' is a heading would be the more
        // common false positive.
        assert_eq!(markdown_to_html("see #123 for context"), "see #123 for context");
        assert_eq!(markdown_to_html("#deploy"), "#deploy");
    }

    #[test]
    fn seven_hashes_is_not_a_valid_heading_level() {
        assert_eq!(markdown_to_html("####### too deep"), "####### too deep");
    }

    // ── Dedup hash ───────────────────────────────────────────────────────

    #[test]
    fn content_hash_is_deterministic_and_discriminates() {
        assert_eq!(content_hash("same text"), content_hash("same text"));
        assert_ne!(content_hash("text a"), content_hash("text b"));
    }
}
