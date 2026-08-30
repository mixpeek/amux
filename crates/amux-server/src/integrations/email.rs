//! Gmail REST integration (RR-0088), ported from the Python server's
//! `_gmail_*` helpers — same token files, same request shapes, same response
//! fields, so the two servers are interchangeable against one
//! `~/.amux/gmail-tokens/` directory.
//!
//! Ported flows:
//! - token refresh: `POST https://oauth2.googleapis.com/token` with the
//!   stored refresh token; the refreshed access token is written back to the
//!   token file in Python's exact JSON shape.
//! - send: `users/messages/send` with a base64url-encoded RFC822
//!   multipart/alternative message (text + HTML + the account's real Gmail
//!   send-as signature, fetched from `users/settings/sendAs`).
//! - reply: the EXACT `_gmail_reply_send` header logic — In-Reply-To /
//!   References chaining, account-local threadId only when the message
//!   lives in the sending account, external-recipient resolution that can
//!   never email ourselves without `allow_self`. The Python docstrings warn
//!   that a naive reply sends BLANK emails; this port never touches
//!   AppleScript, so that failure class is structurally absent — but the
//!   recipient/threading derivation is kept byte-comparable.
//! - inbox/search: `users/messages/list` + per-message metadata gets, with
//!   the authoritative `after:`/`newer_than:` date filters (AMUX-1886) and
//!   the `truncated` marker so a caller can tell a real 0 from a capped
//!   scan.
//!
//! NO credential values live in this file or its tests. Tokens come from
//! `<home>/gmail-tokens/<email>.json` at runtime; tests use temp dirs with
//! synthetic placeholder strings.
//!
//! All HTTP goes through the [`HttpTransport`] trait so tests mock the wire
//! and never touch the network.

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const GMAIL_BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
pub const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

/// Python's own-domain exemption list (`_gmail_reply_send` / the new-thread
/// guard), ported verbatim.
pub const OUR_DOMAINS: [&str; 4] =
    ["mixpeek.com", "trymixpeek.com", "joinmixpeek.com", "getmixpeek.com"];

// ---------------------------------------------------------------------------
// base64 (std + urlsafe) — implemented here because the workspace forbids new
// deps; ~40 lines beats pulling a crate for two alphabets.
// ---------------------------------------------------------------------------

const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64_encode(data: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(alphabet[(n >> 18) as usize & 63] as char);
        out.push(alphabet[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(alphabet[(n >> 6) as usize & 63] as char);
        } else if pad {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(alphabet[n as usize & 63] as char);
        } else if pad {
            out.push('=');
        }
    }
    out
}

/// Python `base64.urlsafe_b64encode` (keeps padding) — the shape Gmail's
/// `raw` field expects.
pub fn base64url(data: &[u8]) -> String {
    b64_encode(data, B64_URL, true)
}

/// `secrets.token_urlsafe`-style: urlsafe, padding stripped.
pub fn base64url_nopad(data: &[u8]) -> String {
    b64_encode(data, B64_URL, false)
}

/// Standard base64 with padding (MIME body encoding).
pub fn base64_std(data: &[u8]) -> String {
    b64_encode(data, B64_STD, true)
}

/// Decode urlsafe base64 (padding optional). Used by tests to open the
/// `raw` payload and by future message-body reads.
pub fn base64url_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' | b'\r' | b'\n' => continue,
            _ => return Err(format!("invalid base64 byte {c}")),
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

/// Wrap a base64 body at 76 chars (RFC 2045) with CRLF line ends.
fn wrap76(s: &str) -> String {
    s.as_bytes()
        .chunks(76)
        .map(|c| std::str::from_utf8(c).expect("base64 is ascii"))
        .collect::<Vec<_>>()
        .join("\r\n")
}

// ---------------------------------------------------------------------------
// Small ports of Python stdlib behavior the flows depend on
// ---------------------------------------------------------------------------

/// Python `html.escape(s)` with quote=True (the default used by
/// `_gmail_compose_send`).
pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Python `_sig_html_to_text`: best-effort plain-text rendering of an HTML
/// signature — keeps line breaks, drops tags/images.
pub fn sig_html_to_text(sig_html: &str) -> String {
    if sig_html.is_empty() {
        return String::new();
    }
    // `<br` PLUS ANY ATTRIBUTES (Ethan, 2026-08-24: "formatting wasn't
    // retained"). This was `<\s*br\s*/?>`, which matches `<br>` and `<br/>` and
    // NOT `<br style="...">` — and Gmail's signature editor puts an inline style
    // on every single break. Unmatched, they fell through to the generic
    // `<[^>]+>` stripper below and were DELETED, so every line break in a real
    // signature silently vanished.
    //
    // What the recipient saw, from the primis reply of that evening, read back
    // off the SENT message rather than reasoned about:
    //
    //   Ethan Steininger
    //   Founder & CEO @ Mixpeek LinkedInSchedule time with me
    //   915 Broadway, Suite 1200
    //
    // Two link labels concatenated into `LinkedInSchedule` because the break
    // between them was dropped. `\b` after `br` so `<brother>` is still not a
    // break.
    //
    // Only the text/plain alternative was affected. The HTML alternative
    // carries the signature HTML verbatim and was correct throughout, which is
    // exactly why this survived: whoever checked in Gmail saw the HTML part and
    // it looked fine.
    let br = regex::Regex::new(r"(?i)<\s*br\b[^>]*>").expect("static regex");
    let blocks = regex::Regex::new(r"(?i)</\s*(p|div|tr|table|h[1-6])\s*>").expect("static regex");
    let tags = regex::Regex::new(r"<[^>]+>").expect("static regex");
    let trail_ws = regex::Regex::new(r"[ \t]+\n").expect("static regex");
    let many_nl = regex::Regex::new(r"\n{3,}").expect("static regex");
    let mut t = br.replace_all(sig_html, "\n").into_owned();
    t = blocks.replace_all(&t, "\n").into_owned();
    t = tags.replace_all(&t, "").into_owned();
    t = html_unescape(&t);
    t = trail_ws.replace_all(&t, "\n").into_owned();
    t = many_nl.replace_all(&t, "\n\n").into_owned();
    let out = t.trim().to_string();
    // MAKE THE NEXT ONE SELF-ANNOUNCING. This bug shipped in the text/plain
    // alternative of every email for as long as the signature had styled breaks,
    // and nothing anywhere could have reported it: the HTML alternative was
    // correct, so the send looked right to anyone who checked in Gmail.
    //
    // The condition cannot false-positive. A signature with no `<br` at all is
    // legitimately one line and does not fire; one that HAS breaks and produced
    // zero newlines has lost every one of them, which is never correct.
    if !out.is_empty() && !out.contains('\n') && sig_html.to_ascii_lowercase().contains("<br") {
        tracing::warn!(
            html_len = sig_html.len(),
            text_len = out.len(),
            "email signature: HTML has <br> breaks but the plain-text rendering has NO line \
             breaks — the text/plain alternative is collapsing the signature onto one line \
             (this is how `LinkedInSchedule time with me` reached a customer on 2026-08-24)"
        );
    }
    out
}

/// The handful of entities `html.unescape` sees in real signatures.
fn html_unescape(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Python `email.utils.getaddresses` — enough of it for real From/To/Cc
/// headers: quoted display names may contain commas; the address is the
/// `<...>` payload when present, else the bare token.
pub fn parse_addresses(hdr: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut in_angle = false;
    for ch in hdr.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(ch);
            }
            '<' if !in_quotes => {
                in_angle = true;
                cur.push(ch);
            }
            '>' if !in_quotes => {
                in_angle = false;
                cur.push(ch);
            }
            ',' if !in_quotes && !in_angle => {
                push_addr(&mut out, &cur);
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    push_addr(&mut out, &cur);
    out
}

fn push_addr(out: &mut Vec<String>, token: &str) {
    let t = token.trim();
    if t.is_empty() {
        return;
    }
    let addr = match (t.rfind('<'), t.rfind('>')) {
        (Some(a), Some(b)) if b > a => &t[a + 1..b],
        _ => t,
    };
    let addr = addr.trim();
    if !addr.is_empty() {
        out.push(addr.to_string());
    }
}

/// Percent-encode a query-string value (RFC 3986 unreserved set kept).
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
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

/// RFC 2047 encoded-word for non-ASCII header values (what Python's email
/// lib does implicitly for Subject etc.). ASCII passes through verbatim.
pub fn encode_header_value(s: &str) -> String {
    if s.is_ascii() {
        s.to_string()
    } else {
        format!("=?utf-8?b?{}?=", base64_std(s.as_bytes()))
    }
}

// ---------------------------------------------------------------------------
// RFC822 assembly (Python `_gmail_compose_send`'s MIME construction)
// ---------------------------------------------------------------------------

/// Everything needed to assemble one outgoing message. `boundary` is
/// injected so tests pin the full RFC822 output.
/// One file attachment (AMUX-3357). `data` is the raw bytes; `build_rfc822`
/// base64-encodes it into a multipart/mixed part.
#[derive(Clone, Debug)]
pub struct Attachment {
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

pub struct MimeSpec<'a> {
    pub from: &'a str,
    pub to: &'a str,
    pub cc: &'a str,
    pub subject: &'a str,
    pub in_reply_to: &'a str,
    pub references: &'a str,
    pub plain: &'a str,
    pub html: &'a str,
    pub boundary: &'a str,
    /// File attachments (AMUX-3357). Empty = the classic multipart/alternative
    /// message, byte-for-byte as before; non-empty wraps that body in a
    /// multipart/mixed with one base64 part per file.
    pub attachments: &'a [Attachment],
}

/// A header-safe filename: no quotes or CR/LF that could break the
/// Content-Disposition line.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name.chars().filter(|c| *c != '"' && *c != '\r' && *c != '\n').collect();
    let base = cleaned.rsplit(['/', '\\']).next().unwrap_or(&cleaned).trim();
    if base.is_empty() { "attachment".to_string() } else { base.to_string() }
}

/// Build the RFC822 message (CRLF line ends, base64 parts). With no attachments
/// this is a multipart/alternative (text+html), unchanged. With attachments the
/// alternative body becomes the first sub-part of a multipart/mixed container,
/// followed by one base64 attachment part each (AMUX-3357). Threading headers
/// appear iff `in_reply_to` is non-empty; References defaults to In-Reply-To.
pub fn build_rfc822(spec: &MimeSpec) -> String {
    let has_att = !spec.attachments.is_empty();
    let inner = spec.boundary;
    let outer = format!("{}_mix", spec.boundary);
    let mut lines: Vec<String> = vec!["MIME-Version: 1.0".into()];
    if has_att {
        lines.push(format!("Content-Type: multipart/mixed; boundary=\"{outer}\""));
    } else {
        lines.push(format!("Content-Type: multipart/alternative; boundary=\"{inner}\""));
    }
    lines.push(format!("To: {}", spec.to));
    lines.push(format!("From: {}", spec.from));
    lines.push(format!("Subject: {}", encode_header_value(spec.subject)));
    if !spec.cc.is_empty() {
        lines.push(format!("Cc: {}", spec.cc));
    }
    if !spec.in_reply_to.is_empty() {
        lines.push(format!("In-Reply-To: {}", spec.in_reply_to));
        let refs = if spec.references.is_empty() { spec.in_reply_to } else { spec.references };
        lines.push(format!("References: {refs}"));
    }
    lines.push(String::new());
    // The multipart/alternative body — a sub-part of the mixed container when
    // there are attachments, otherwise the top-level body.
    if has_att {
        lines.push(format!("--{outer}"));
        lines.push(format!("Content-Type: multipart/alternative; boundary=\"{inner}\""));
        lines.push(String::new());
    }
    for (ctype, body) in [("text/plain", spec.plain), ("text/html", spec.html)] {
        lines.push(format!("--{inner}"));
        lines.push(format!("Content-Type: {ctype}; charset=\"utf-8\""));
        lines.push("MIME-Version: 1.0".into());
        lines.push("Content-Transfer-Encoding: base64".into());
        lines.push(String::new());
        lines.push(wrap76(&base64_std(body.as_bytes())));
    }
    lines.push(format!("--{inner}--"));
    if has_att {
        lines.push(String::new());
        for att in spec.attachments {
            let fname = sanitize_filename(&att.filename);
            let ct = if att.content_type.trim().is_empty() {
                "application/octet-stream"
            } else {
                att.content_type.trim()
            };
            lines.push(format!("--{outer}"));
            lines.push(format!("Content-Type: {ct}; name=\"{fname}\""));
            lines.push("Content-Transfer-Encoding: base64".into());
            lines.push(format!("Content-Disposition: attachment; filename=\"{fname}\""));
            lines.push(String::new());
            lines.push(wrap76(&base64_std(&att.data)));
        }
        lines.push(format!("--{outer}--"));
    }
    lines.push(String::new());
    lines.join("\r\n")
}

/// Python `_gmail_compose_send`'s body derivation: escaped-HTML rendering of
/// the plain body, signature appended to both alternatives. Returns
/// `(plain_full, html_full, signature_included)`.
pub fn compose_bodies(body: &str, sig_html: &str) -> (String, String, bool) {
    // BOTH `<br>` AND `white-space:pre-wrap`, and the combination is the fix
    // (Ethan, 2026-08-25, with a rendered screenshot).
    //
    // `pre-wrap` ALONE was not enough, and the proof is inside one delivered
    // message. The welcome email to siripuru.v@media.net went out with a correct
    // MIME — 11 blank-line breaks in text/plain, `pre-wrap` plus raw newlines in
    // the HTML, both verified by reading the SENT message back. It rendered as a
    // wall of text, breaks lost mid-sentence ("adtech-platform-docs You work in
    // adtech"). In the SAME message the signature rendered correctly, and the
    // signature is built from real `<br>` elements.
    //
    // Same client, same message, two mechanisms: `<br>` survived and
    // `white-space` did not. Mail clients sanitise CSS; they do not strip `<br>`.
    // Structure that depends on a style property is structure the recipient may
    // never see, and nothing in the send path can detect that — the bytes we
    // sent were right, which is exactly why this survived a verification that
    // read them back.
    //
    // NO RAW NEWLINE IS LEFT BEHIND, which is the trap the previous comment was
    // written to avoid. Under `pre-wrap` a `<br>` followed by a literal newline
    // renders as TWO breaks, so converting while also keeping the newline would
    // double every paragraph gap. Converting REPLACES it, so:
    //   - `pre-wrap` honoured  -> `<br>` breaks, nothing doubles
    //   - `pre-wrap` stripped  -> `<br>` breaks anyway
    // The div stays for the one thing `<br>` cannot carry, leading indentation,
    // which degrades to collapsed rather than to lost lines.
    let body_html = html_escape(body).replace('\n', "<br>");
    let mut html_full = format!("<div style=\"white-space:pre-wrap;\">{body_html}</div>");
    if !sig_html.is_empty() {
        html_full.push_str(&format!("<br><br>{sig_html}"));
    }
    let sig_text = sig_html_to_text(sig_html);
    let plain_full =
        if sig_text.is_empty() { body.to_string() } else { format!("{body}\n\n{sig_text}") };
    (plain_full, html_full, !sig_html.is_empty())
}

// ---------------------------------------------------------------------------
// Reply derivation (Python `_gmail_reply_send`, the pure part)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ReplyPlan {
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub in_reply_to: String,
    pub references: String,
}

/// Derive recipients + threading headers for a reply, given the original
/// message's headers (LOWERCASE keys). Ported exactly, including:
/// - the reply goes to the EXTERNAL party, never one of our own accounts
///   (the reply-to-self bug fix);
/// - `allow_self` is the sanctioned threading self-test between owned
///   accounts (AMUX-1739 follow-up);
/// - the error string when no recipient survives is Python's, verbatim.
pub fn derive_reply_plan(
    headers: &HashMap<String, String>,
    fallback_msgid: &str,
    account: &str,
    connected: &[String],
    reply_all: bool,
    allow_self: bool,
) -> Result<ReplyPlan, String> {
    let hdr = |k: &str| headers.get(k).map(String::as_str).unwrap_or("");
    let mut orig_msgid = if hdr("message-id").is_empty() {
        fallback_msgid.to_string()
    } else {
        hdr("message-id").to_string()
    };
    if !orig_msgid.starts_with('<') {
        // Python: prepends '<' only (no trailing '>' fixup) — kept identical
        // so both servers send byte-identical headers for the same input.
        orig_msgid = format!("<{orig_msgid}>");
    }
    let subject_raw = hdr("subject");
    let subject = if subject_raw.to_lowercase().starts_with("re:") {
        subject_raw.to_string()
    } else {
        format!("Re: {subject_raw}")
    };

    let connected_lc: Vec<String> = connected
        .iter()
        .map(|a| a.to_lowercase())
        .chain(std::iter::once(account.to_lowercase()))
        .collect();
    let external = |h: &str| -> Vec<String> {
        parse_addresses(h)
            .into_iter()
            .filter(|a| {
                let al = a.to_lowercase();
                !(connected_lc.contains(&al)
                    || OUR_DOMAINS.iter().any(|d| al.ends_with(&format!("@{d}"))))
            })
            .collect()
    };
    let from_ext = external(hdr("from"));
    let to_ext = external(hdr("to"));
    let cc_ext = external(hdr("cc"));

    let (mut to_addr, cc) = if reply_all {
        let mut all_ext: Vec<String> = Vec::new();
        for a in from_ext.iter().chain(&to_ext).chain(&cc_ext) {
            if !all_ext.contains(a) {
                all_ext.push(a.clone());
            }
        }
        (all_ext.join(", "), String::new())
    } else {
        // The party who last spoke (From) if external, else the original
        // recipient(s).
        let pick = if from_ext.is_empty() { &to_ext } else { &from_ext };
        (pick.join(", "), String::new())
    };
    if to_addr.is_empty() && allow_self {
        let not_me = |h: &str| -> Vec<String> {
            parse_addresses(h)
                .into_iter()
                .filter(|a| a.to_lowercase() != account.to_lowercase())
                .collect()
        };
        let raw_from = not_me(hdr("from"));
        let raw_to = not_me(hdr("to"));
        to_addr = if raw_from.is_empty() { raw_to.join(", ") } else { raw_from.join(", ") };
    }
    if to_addr.is_empty() {
        // NEVER fall back to emailing ourselves (without explicit allow_self).
        return Err("no external recipient on this thread (would email ourselves) — \
                    pass an explicit 'to' via /api/email/send, or use \
                    {\"allow_self\": true} to run a threading self-test between owned accounts"
            .into());
    }
    let references = format!("{} {}", hdr("references"), orig_msgid).trim().to_string();
    Ok(ReplyPlan { to: to_addr, cc, subject, in_reply_to: orig_msgid, references })
}

// ---------------------------------------------------------------------------
// HTTP transport seam
// ---------------------------------------------------------------------------

#[async_trait]
pub trait HttpTransport: Send + Sync {
    async fn get(&self, url: &str, bearer: Option<&str>) -> Result<(u16, Value), String>;
    async fn post_json(
        &self,
        url: &str,
        bearer: Option<&str>,
        body: &Value,
    ) -> Result<(u16, Value), String>;
    async fn post_form(
        &self,
        url: &str,
        form: &[(String, String)],
    ) -> Result<(u16, Value), String>;
    /// Raw-body POST with a caller-chosen content type, response as TEXT —
    /// the Gmail batch endpoint (AMUX-3520) speaks multipart/mixed in both
    /// directions, which the JSON-shaped verbs above cannot carry. Defaulted
    /// to an ERROR, not a stub success: a transport that never implemented
    /// it (older test mocks) makes the batch path fall back to single
    /// fetches loudly rather than pretend a batch happened.
    async fn post_raw(
        &self,
        _url: &str,
        _bearer: Option<&str>,
        _content_type: &str,
        _body: String,
    ) -> Result<(u16, String), String> {
        Err("post_raw not implemented by this transport".into())
    }
}

/// Production transport (reqwest, 30s timeout — the Python side bounds each
/// account at 20s so one wedged token cannot hang the unified inbox).
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
        }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

async fn to_pair(res: reqwest::Response) -> Result<(u16, Value), String> {
    let status = res.status().as_u16();
    let text = res.text().await.map_err(|e| e.to_string())?;
    let v = serde_json::from_str(&text).unwrap_or(Value::String(text));
    Ok((status, v))
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn get(&self, url: &str, bearer: Option<&str>) -> Result<(u16, Value), String> {
        let mut req = self.client.get(url);
        if let Some(t) = bearer {
            req = req.bearer_auth(t);
        }
        to_pair(req.send().await.map_err(|e| e.to_string())?).await
    }
    async fn post_json(
        &self,
        url: &str,
        bearer: Option<&str>,
        body: &Value,
    ) -> Result<(u16, Value), String> {
        let mut req = self.client.post(url).json(body);
        if let Some(t) = bearer {
            req = req.bearer_auth(t);
        }
        to_pair(req.send().await.map_err(|e| e.to_string())?).await
    }
    async fn post_form(
        &self,
        url: &str,
        form: &[(String, String)],
    ) -> Result<(u16, Value), String> {
        let req = self.client.post(url).form(form);
        to_pair(req.send().await.map_err(|e| e.to_string())?).await
    }
    async fn post_raw(
        &self,
        url: &str,
        bearer: Option<&str>,
        content_type: &str,
        body: String,
    ) -> Result<(u16, String), String> {
        let mut req = self.client.post(url).header("content-type", content_type).body(body);
        if let Some(t) = bearer {
            req = req.bearer_auth(t);
        }
        let res = req.send().await.map_err(|e| e.to_string())?;
        let status = res.status().as_u16();
        let text = res.text().await.map_err(|e| e.to_string())?;
        Ok((status, text))
    }
}

// ---------------------------------------------------------------------------
// Gmail batch (AMUX-3520): N metadata GETs in ceil(N/100) HTTP round-trips
// ---------------------------------------------------------------------------
//
// The inbox's per-message fetches are quota-bound at ~12-15s for count=480
// (8-wide, deliberately under Gmail's 250 units/s). The batch endpoint packs
// up to 100 sub-requests per multipart/mixed POST, so the same fetch is ~5
// round-trips. Builder and parser are PURE so every framing cell is testable
// against canned Google shapes without a transport.

pub(crate) const GMAIL_BATCH_URL: &str = "https://gmail.googleapis.com/batch/gmail/v1";
pub(crate) const GMAIL_BATCH_BOUNDARY: &str = "amux-gmail-batch";

/// The multipart/mixed request body: one application/http part per sub-path,
/// Content-IDs `item{i}` in order (Google echoes them back as
/// `response-item{i}`, which is how out-of-order responses re-align).
pub(crate) fn batch_request_body(boundary: &str, sub_paths: &[String]) -> String {
    let mut out = String::new();
    for (i, p) in sub_paths.iter().enumerate() {
        out.push_str(&format!(
            "--{boundary}\r\nContent-Type: application/http\r\nContent-ID: <item{i}>\r\n\r\nGET {p} HTTP/1.1\r\n\r\n"
        ));
    }
    out.push_str(&format!("--{boundary}--\r\n"));
    out
}

/// Parse a Google batch response into (item index, embedded status, JSON).
/// The boundary is SNIFFED from the body's own first `--` line rather than
/// trusted from a header the caller may not have kept; parts that fail any
/// step are skipped — the caller's fallback path re-fetches the holes, so a
/// malformed part degrades to one extra GET, never to a wrong message.
pub(crate) fn parse_batch_parts(body: &str) -> Vec<(usize, u16, Value)> {
    let norm = body.replace("\r\n", "\n");
    let Some(bline) = norm.lines().find(|l| l.starts_with("--")) else { return vec![] };
    let boundary = bline.trim_end_matches('-').trim();
    let mut out = Vec::new();
    for part in norm.split(boundary) {
        let part = part.trim_matches(['-', '\n', ' ']);
        if part.is_empty() {
            continue;
        }
        // Content-ID: <response-item{N}> (angle brackets optional).
        let Some(idx) = part
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("content-id:"))
            .and_then(|l| {
                let tail = l.rsplit("item").next()?;
                tail.trim_matches(['<', '>', ' ']).parse::<usize>().ok()
            })
        else {
            continue;
        };
        // Embedded HTTP status line, then the JSON after its header block.
        let Some(http_at) = part.find("HTTP/1.1 ") else { continue };
        let embedded = &part[http_at..];
        let Some(status) = embedded
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
        else {
            continue;
        };
        let Some(blank) = embedded.find("\n\n") else { continue };
        let json_text = embedded[blank..].trim();
        let Ok(v) = serde_json::from_str::<Value>(json_text) else { continue };
        out.push((idx, status, v));
    }
    out
}

// ---------------------------------------------------------------------------
// Token files (~/.amux/gmail-tokens/<email>.json)
// ---------------------------------------------------------------------------

pub fn default_amux_home() -> PathBuf {
    std::env::var("AMUX_HOME").map(PathBuf::from).unwrap_or_else(|_| {
        std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/")).join(".amux")
    })
}

/// Accounts with stored tokens (Python `_gmail_connected_accounts`): sorted
/// stems of `<home>/gmail-tokens/*.json`.
pub fn connected_accounts_in(home: &Path) -> Vec<String> {
    let dir = home.join("gmail-tokens");
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            (p.extension().and_then(|x| x.to_str()) == Some("json"))
                .then(|| p.file_stem()?.to_str().map(String::from))
                .flatten()
        })
        .collect();
    out.sort();
    out
}

/// Connected accounts ordered NEWEST-TOKEN-FIRST (AMUX-3203 / amux-cloud incident
/// 2026-08-16). The alert email channel used `connected_accounts_in` (alphabetical)
/// and blindly picked the FIRST account, whose refresh_token was dead
/// (`invalid_grant`), while a healthy account existed. A refreshed token rewrites
/// its file, so ordering by token-file mtime puts the account most likely to have a
/// LIVE token first, and the fire alarm tries a working sender before a stale one.
/// Accounts whose mtime is unreadable sort last (treated as oldest).
pub fn connected_accounts_by_freshness_in(home: &Path) -> Vec<String> {
    let dir = home.join("gmail-tokens");
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out: Vec<(std::time::SystemTime, String)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                return None;
            }
            let stem = p.file_stem()?.to_str()?.to_string();
            let mtime = p.metadata().ok().and_then(|m| m.modified().ok());
            Some((mtime.unwrap_or(std::time::UNIX_EPOCH), stem))
        })
        .collect();
    // Newest first; ties broken by name so the order is deterministic in tests.
    out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    out.into_iter().map(|(_, s)| s).collect()
}

/// Age in seconds of the FRESHEST connected-account token file (None if there are
/// no accounts). The alert invariant uses this: a fleet whose newest Gmail token
/// has not refreshed in a long time is one whose alert email is at real risk of
/// `invalid_grant` (the amux-cloud incident), so "a connected account exists" is
/// not enough to call email deliverable. `now` is passed for testability.
pub fn newest_token_age_secs_in(home: &Path, now: std::time::SystemTime) -> Option<u64> {
    let acct = connected_accounts_by_freshness_in(home).into_iter().next()?;
    let p = home.join("gmail-tokens").join(format!("{acct}.json"));
    let mtime = p.metadata().ok()?.modified().ok()?;
    now.duration_since(mtime).ok().map(|d| d.as_secs())
}

#[derive(Debug, Clone, Default)]
struct TokenFile {
    token: Option<String>,
    refresh_token: Option<String>,
    token_uri: String,
    client_id: String,
    client_secret: String,
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

pub struct GmailClient {
    http: Arc<dyn HttpTransport>,
    home: PathBuf,
    /// account -> live access token (in-memory; the file may hold a stale
    /// one, which the 401-retry path replaces).
    token_cache: Mutex<HashMap<String, String>>,
}

impl GmailClient {
    pub fn new(http: Arc<dyn HttpTransport>, home: PathBuf) -> Self {
        Self { http, home, token_cache: Mutex::new(HashMap::new()) }
    }

    pub fn new_default() -> Self {
        Self::new(Arc::new(ReqwestTransport::new()), default_amux_home())
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn connected_accounts(&self) -> Vec<String> {
        connected_accounts_in(&self.home)
    }

    fn token_path(&self, account: &str) -> PathBuf {
        self.home.join("gmail-tokens").join(format!("{account}.json"))
    }

    /// Load the token file, merging client id/secret from
    /// `gmail-oauth-client.json` (`installed` or `web`) when the token file
    /// lacks them — Python `_gmail_load_creds`.
    fn load_token_file(&self, account: &str) -> Option<TokenFile> {
        let data: Value =
            serde_json::from_str(&std::fs::read_to_string(self.token_path(account)).ok()?).ok()?;
        let s = |k: &str| data.get(k).and_then(Value::as_str).map(String::from);
        let mut tf = TokenFile {
            token: s("token").filter(|t| !t.is_empty()),
            refresh_token: s("refresh_token").filter(|t| !t.is_empty()),
            token_uri: s("token_uri").unwrap_or_else(|| DEFAULT_TOKEN_URI.into()),
            client_id: s("client_id").unwrap_or_default(),
            client_secret: s("client_secret").unwrap_or_default(),
        };
        if tf.client_id.is_empty() || tf.client_secret.is_empty() {
            if let Ok(raw) = std::fs::read_to_string(self.home.join("gmail-oauth-client.json")) {
                if let Ok(cfg) = serde_json::from_str::<Value>(&raw) {
                    let node = cfg.get("installed").or_else(|| cfg.get("web"));
                    if let Some(n) = node {
                        if tf.client_id.is_empty() {
                            tf.client_id = n
                                .get("client_id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                        }
                        if tf.client_secret.is_empty() {
                            tf.client_secret = n
                                .get("client_secret")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                        }
                    }
                }
            }
        }
        Some(tf)
    }

    /// Current access token; `force_refresh` bypasses cache + stored token
    /// (the 401-retry path). Persists the refreshed token in Python's exact
    /// file shape so both servers keep working off one file.
    async fn access_token(&self, account: &str, force_refresh: bool) -> Result<String, String> {
        if !force_refresh {
            if let Some(t) = self.token_cache.lock().expect("token cache").get(account) {
                return Ok(t.clone());
            }
        }
        let tf = self.load_token_file(account).ok_or_else(|| "not_connected".to_string())?;
        if !force_refresh {
            if let Some(t) = &tf.token {
                self.token_cache.lock().expect("token cache").insert(account.into(), t.clone());
                return Ok(t.clone());
            }
        }
        let refresh = tf
            .refresh_token
            .clone()
            .ok_or_else(|| "not_connected (no refresh_token stored)".to_string())?;
        let form = vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh.clone()),
            ("client_id".to_string(), tf.client_id.clone()),
            ("client_secret".to_string(), tf.client_secret.clone()),
        ];
        let (status, body) = self.http.post_form(&tf.token_uri, &form).await?;
        if status >= 400 {
            // invalid_grant (revoked/expired) must stay visible in the error
            // text — it is the discriminator between "re-auth needed" and
            // "not connected" (the 2026-08-07 wrong-probe incident).
            return Err(format!("token refresh failed ({status}): {body}"));
        }
        let access = body
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("token refresh response missing access_token: {body}"))?
            .to_string();
        self.token_cache.lock().expect("token cache").insert(account.into(), access.clone());
        // Best-effort persist, Python's exact shape (a failed write must not
        // fail the send).
        let persisted = json!({
            "token": access,
            "refresh_token": refresh,
            "token_uri": tf.token_uri,
            "client_id": tf.client_id,
            "client_secret": tf.client_secret,
        });
        let _ = std::fs::write(self.token_path(account), persisted.to_string());
        Ok(access)
    }

    /// One authenticated Gmail call with the 401-refresh-retry that
    /// google-auth's AuthorizedSession does implicitly for Python.
    async fn api(
        &self,
        account: &str,
        method: &str,
        url: &str,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let mut token = self.access_token(account, false).await?;
        let mut refreshed = false;
        let mut last: Option<(u16, Value)> = None;
        for attempt in 0..4u32 {
            let (status, v) = match (method, body) {
                ("GET", _) => self.http.get(url, Some(&token)).await?,
                ("POST", Some(b)) => self.http.post_json(url, Some(&token), b).await?,
                _ => return Err(format!("unsupported method {method}")),
            };
            if status == 401 && !refreshed {
                refreshed = true;
                token = self.access_token(account, true).await?;
                continue;
            }
            // AMUX-3495: quota pushback retries instead of failing. The 8-wide
            // inbox fetch (AMUX-3337) sits just under Gmail's 250 units/s
            // budget, and its per-message caller `.ok()`s failures — so before
            // this, a 429 didn't slow the inbox down, it silently DROPPED the
            // message from the response. Three short backoffs (250/500/1000ms)
            // absorb a transient limit; a sustained one still errors, loudly,
            // at the caller's WARN.
            if (status == 429 || status == 503) && attempt < 3 {
                tokio::time::sleep(std::time::Duration::from_millis(250 << attempt)).await;
                last = Some((status, v));
                continue;
            }
            if status >= 400 {
                return Err(format!("gmail api {status}: {v}"));
            }
            return Ok(v);
        }
        let (status, v) = last.unwrap_or((0, Value::Null));
        Err(format!("gmail api {status} after retries: {v}"))
    }

    fn metadata_url(&self, id: &str, headers: &[&str]) -> String {
        let hs: String = headers.iter().map(|h| format!("&metadataHeaders={h}")).collect();
        format!("{GMAIL_BASE}/messages/{id}?format=metadata{hs}")
    }

    /// Best-effort batch metadata fetch (AMUX-3520): up to 100 sub-requests
    /// per HTTP call. Returns a Vec ALIGNED with `mids`; a hole means that
    /// part failed (or the whole batch did) and the caller re-fetches it
    /// through the single-message path, which keeps the 429-retry and
    /// loud-drop semantics that path already carries.
    pub async fn batch_metadata(
        &self,
        account: &str,
        mids: &[String],
        headers: &[&str],
    ) -> Vec<Option<Value>> {
        let mut out: Vec<Option<Value>> = vec![None; mids.len()];
        let Ok(mut token) = self.access_token(account, false).await else { return out };
        let ct = format!("multipart/mixed; boundary={GMAIL_BATCH_BOUNDARY}");
        for (chunk_i, chunk) in mids.chunks(100).enumerate() {
            let base = chunk_i * 100;
            let subs: Vec<String> = chunk
                .iter()
                .map(|id| {
                    self.metadata_url(id, headers)
                        .trim_start_matches("https://gmail.googleapis.com")
                        .to_string()
                })
                .collect();
            let body = batch_request_body(GMAIL_BATCH_BOUNDARY, &subs);
            let mut resp =
                self.http.post_raw(GMAIL_BATCH_URL, Some(&token), &ct, body.clone()).await;
            if matches!(&resp, Ok((401, _))) {
                if let Ok(t2) = self.access_token(account, true).await {
                    token = t2;
                    resp = self.http.post_raw(GMAIL_BATCH_URL, Some(&token), &ct, body).await;
                }
            }
            match resp {
                Ok((status, text)) if (200..300).contains(&status) => {
                    for (idx, part_status, v) in parse_batch_parts(&text) {
                        if (200..300).contains(&part_status) && idx < chunk.len() {
                            out[base + idx] = Some(v);
                        }
                    }
                }
                Ok((status, text)) => {
                    tracing::warn!(
                        %status,
                        "gmail batch call failed — falling back to single fetches for {} ids: {}",
                        chunk.len(),
                        text.chars().take(200).collect::<String>()
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "gmail batch transport error — falling back to single fetches for {} ids: {e}",
                        chunk.len()
                    );
                }
            }
        }
        out
    }

    async fn list_ids(
        &self,
        account: &str,
        q: &str,
        max_results: usize,
        page_token: Option<&str>,
    ) -> Result<Value, String> {
        let mut url = format!("{GMAIL_BASE}/messages?q={}&maxResults={max_results}", urlencode(q));
        if let Some(pt) = page_token {
            url.push_str(&format!("&pageToken={}", urlencode(pt)));
        }
        self.api(account, "GET", &url, None).await
    }

    /// Python `_gmail_get_signature`: the account's configured send-as HTML
    /// signature; "" on any failure.
    pub async fn get_signature(&self, account: &str) -> String {
        let url = format!("{GMAIL_BASE}/settings/sendAs");
        let Ok(v) = self.api(account, "GET", &url, None).await else { return String::new() };
        let empty = vec![];
        let sendas = v.get("sendAs").and_then(Value::as_array).unwrap_or(&empty);
        let target = account.to_lowercase();
        let chosen = sendas
            .iter()
            .find(|sa| {
                sa.get("sendAsEmail").and_then(Value::as_str).unwrap_or("").to_lowercase() == target
            })
            .or_else(|| {
                sendas.iter().find(|sa| sa.get("isPrimary").and_then(Value::as_bool) == Some(true))
            })
            .or_else(|| sendas.first());
        chosen
            .and_then(|sa| sa.get("signature"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    /// Python `_gmail_compose_send`: multipart/alternative with signature,
    /// optional threading headers + threadId. Returns
    /// `{ok, id, thread_id, signature_included}`.
    #[allow(clippy::too_many_arguments)]
    pub async fn compose_send(
        &self,
        account: &str,
        to: &str,
        subject: &str,
        body: &str,
        cc: &str,
        in_reply_to: &str,
        references: &str,
        thread_id: &str,
        include_signature: bool,
        attachments: &[Attachment],
    ) -> Result<Value, String> {
        let sig_html =
            if include_signature { self.get_signature(account).await } else { String::new() };
        let (plain, html, sig_included) = compose_bodies(body, &sig_html);
        // Boundary needs no cryptographic strength, only absence from the
        // payload; nanos + a fixed tag matches Python's uniqueness level.
        let boundary = format!(
            "=_amux_{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let rfc822 = build_rfc822(&MimeSpec {
            from: account,
            to,
            cc,
            subject,
            in_reply_to,
            references,
            plain: &plain,
            html: &html,
            boundary: &boundary,
            attachments,
        });
        let mut send_body = Map::new();
        send_body.insert("raw".into(), json!(base64url(rfc822.as_bytes())));
        if !thread_id.is_empty() {
            send_body.insert("threadId".into(), json!(thread_id));
        }
        let url = format!("{GMAIL_BASE}/messages/send");
        let res = self.api(account, "POST", &url, Some(&Value::Object(send_body))).await?;
        Ok(json!({
            "ok": true,
            "id": res.get("id").cloned().unwrap_or(Value::Null),
            "thread_id": res.get("threadId").cloned().unwrap_or(Value::Null),
            "signature_included": sig_included,
        }))
    }

    /// Python `_gmail_find_message_by_rfc822`: indexed `rfc822msgid:` lookup
    /// -> metadata resource (or None).
    pub async fn find_message_by_rfc822(&self, account: &str, rfc822_id: &str) -> Option<Value> {
        let rid = rfc822_id.trim().trim_start_matches('<').trim_end_matches('>');
        let list = self.list_ids(account, &format!("rfc822msgid:{rid}"), 1, None).await.ok()?;
        let mid = list.get("messages")?.as_array()?.first()?.get("id")?.as_str()?.to_string();
        let url = self.metadata_url(
            &mid,
            &["From", "To", "Cc", "Subject", "Message-ID", "References", "In-Reply-To"],
        );
        self.api(account, "GET", &url, None).await.ok()
    }

    /// Fetch ONE message with `format=full` and return its FULL parsed body (not
    /// the snippet) plus the attachment list. The read-one endpoint reused the
    /// LIST path (metadata + `snippet`), so bodies were truncated and attachments
    /// invisible (AMUX-3354, autodesk).
    pub async fn get_message_full(&self, account: &str, gmail_id: &str) -> Result<Value, String> {
        let url = format!("{GMAIL_BASE}/messages/{}?format=full", urlencode(gmail_id));
        let full = self.api(account, "GET", &url, None).await?;
        let payload = full.get("payload").cloned().unwrap_or(json!({}));
        let h = header_map_of(&payload);
        let hget = |k: &str| h.get(k).cloned().unwrap_or_default();
        let (html_body, text_body) = decode_body(&payload);
        let body = if !text_body.is_empty() { text_body.clone() } else { html_body.clone() };
        Ok(json!({
            "account": account,
            "gmail_id": gmail_id,
            "thread_id": full.get("threadId").cloned().unwrap_or(Value::Null),
            "message_id": hget("message-id"),
            "from": hget("from"),
            "to": hget("to"),
            "cc": hget("cc"),
            "subject": hget("subject"),
            "date": hget("date"),
            "body": body,
            "body_html": html_body,
            "snippet": full.get("snippet").cloned().unwrap_or(json!("")),
            "attachments": collect_attachments(&payload),
        }))
    }

    /// Download one attachment's raw bytes by its Gmail `attachmentId` (from
    /// `get_message_full`'s attachments list) — AMUX-3354/autodesk.
    pub async fn get_attachment(
        &self,
        account: &str,
        gmail_id: &str,
        attachment_id: &str,
    ) -> Result<Vec<u8>, String> {
        let url = format!(
            "{GMAIL_BASE}/messages/{}/attachments/{}",
            urlencode(gmail_id),
            urlencode(attachment_id)
        );
        let resp = self.api(account, "GET", &url, None).await?;
        let data = resp.get("data").and_then(Value::as_str).ok_or("attachment has no data")?;
        base64url_decode(data)
    }

    /// Python `_gmail_list_messages` (AMUX-2883): header-level summaries for
    /// the Mail view. `q` overrides `label` (python's exact precedence). The
    /// N metadata fetches after the id list are python's own shape — one
    /// round trip per message — kept for contract fidelity; a batch endpoint
    /// exists but changes error semantics per message.
    pub async fn list_messages(
        &self,
        account: &str,
        label: &str,
        page_token: &str,
        q: &str,
        max_results: usize,
    ) -> Value {
        let mut url = format!("{GMAIL_BASE}/messages?maxResults={max_results}");
        if !q.is_empty() {
            url.push_str(&format!("&q={}", urlencode(q)));
        } else {
            url.push_str(&format!("&labelIds={}", urlencode(label)));
        }
        if !page_token.is_empty() {
            url.push_str(&format!("&pageToken={}", urlencode(page_token)));
        }
        let listed = match self.api(account, "GET", &url, None).await {
            Ok(v) => v,
            Err(e) => return self.gmail_error_shape(account, &e),
        };
        let empty = vec![];
        let ids = listed.get("messages").and_then(Value::as_array).unwrap_or(&empty);
        let next = listed.get("nextPageToken").and_then(Value::as_str).unwrap_or("");
        let mut summaries = vec![];
        for m in ids {
            let Some(mid) = m.get("id").and_then(Value::as_str) else { continue };
            let murl = self.metadata_url(mid, &["From", "To", "Subject", "Date"]);
            let Ok(hdr) = self.api(account, "GET", &murl, None).await else { continue };
            let h = header_map(&hdr);
            let lids: Vec<&str> = hdr
                .get("labelIds")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            summaries.push(json!({
                "id": mid,
                "thread_id": hdr.get("threadId").and_then(Value::as_str).unwrap_or(mid),
                "from": h.get("from").cloned().unwrap_or_default(),
                "to": h.get("to").cloned().unwrap_or_default(),
                "subject": h.get("subject").cloned().unwrap_or_else(|| "(no subject)".into()),
                "date": h.get("date").cloned().unwrap_or_default(),
                "snippet": hdr.get("snippet").and_then(Value::as_str).unwrap_or(""),
                "unread": lids.contains(&"UNREAD"),
                "starred": lids.contains(&"STARRED"),
                "internal_date": internal_date(&hdr),
            }));
        }
        json!({ "messages": summaries, "next_page_token": next })
    }

    /// Python `_gmail_get_thread`: full thread with decoded bodies; every
    /// unread message in it is marked read (python's read-on-open behavior,
    /// best-effort — a failed modify never fails the read).
    pub async fn get_thread(&self, account: &str, thread_id: &str) -> Value {
        let url = format!("{GMAIL_BASE}/threads/{}?format=full", urlencode(thread_id));
        let thread = match self.api(account, "GET", &url, None).await {
            Ok(v) => v,
            Err(e) => return self.gmail_error_shape(account, &e),
        };
        let empty = vec![];
        let msgs = thread.get("messages").and_then(Value::as_array).unwrap_or(&empty);
        let mut out = vec![];
        let mut unread_ids = vec![];
        for msg in msgs {
            let payload = msg.get("payload").cloned().unwrap_or(json!({}));
            let h = header_map_of(&payload);
            let (html_body, text_body) = decode_body(&payload);
            let lids: Vec<&str> = msg
                .get("labelIds")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let mid = msg.get("id").and_then(Value::as_str).unwrap_or("");
            if lids.contains(&"UNREAD") {
                unread_ids.push(mid.to_string());
            }
            out.push(json!({
                "id": mid,
                "thread_id": msg.get("threadId").and_then(Value::as_str).unwrap_or(""),
                "from": h.get("from").cloned().unwrap_or_default(),
                "to": h.get("to").cloned().unwrap_or_default(),
                "cc": h.get("cc").cloned().unwrap_or_default(),
                "subject": h.get("subject").cloned().unwrap_or_else(|| "(no subject)".into()),
                "date": h.get("date").cloned().unwrap_or_default(),
                "message_id_header": h.get("message-id").cloned().unwrap_or_default(),
                "html_body": html_body,
                "text_body": text_body,
                "unread": lids.contains(&"UNREAD"),
                "labels": lids,
                "internal_date": internal_date(msg),
            }));
        }
        for uid in unread_ids {
            let murl = format!("{GMAIL_BASE}/messages/{uid}/modify");
            let _ = self
                .api(account, "POST", &murl, Some(&json!({"removeLabelIds": ["UNREAD"]})))
                .await;
        }
        json!({ "thread_id": thread_id, "messages": out })
    }

    /// Python `_gmail_list_labels`: system labels first (INBOX, STARRED,
    /// SENT, DRAFTS, SPAM, TRASH — python's exact order), then user labels
    /// alphabetically. `[]` on any failure (python parity — the Mail view
    /// renders an empty label rail rather than an error).
    pub async fn list_labels(&self, account: &str) -> Vec<Value> {
        let url = format!("{GMAIL_BASE}/labels");
        let Ok(v) = self.api(account, "GET", &url, None).await else { return vec![] };
        let mut labels: Vec<Value> =
            v.get("labels").and_then(Value::as_array).cloned().unwrap_or_default();
        const PRIO: [&str; 6] = ["INBOX", "STARRED", "SENT", "DRAFTS", "SPAM", "TRASH"];
        labels.sort_by_key(|l| {
            let n = l.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            match PRIO.iter().position(|p| *p == n) {
                Some(i) => (0, i.to_string()),
                None => (1, n.to_lowercase()),
            }
        });
        labels
    }

    /// Python's undifferentiated `{"error": str(e)}` sent readers to check
    /// whether the account was connected when it WAS — this is the improved
    /// shape python later grew for list_messages, applied to every entry
    /// point: name the state, and when it is re-authable, name the fix.
    fn gmail_error_shape(&self, account: &str, err: &str) -> Value {
        if err.contains("invalid_grant") {
            json!({
                "error": "needs_reauth",
                "account": account,
                "fix": format!("GET /api/gmail/auth?account={account} — open the url and approve"),
            })
        } else if err.contains("not_connected") {
            json!({ "error": "not_connected", "account": account })
        } else {
            json!({ "error": err, "account": account })
        }
    }

    /// The RESOLVED recipients of a would-be reply, WITHOUT sending
    /// (AMUX-3510). The approval gate must classify a reply's fan-out
    /// before anything reaches the wire — the incident's own vector was a
    /// reply whose one call reached four people the caller never named.
    /// Runs the same lookup + derive_reply_plan the real send runs, so the
    /// gate and the send can never disagree about who a reply goes to.
    pub async fn resolve_reply_recipients(
        &self,
        account: &str,
        rfc822_message_id: &str,
        reply_all: bool,
        allow_self: bool,
    ) -> Result<(String, String, String), String> {
        let mut orig = self.find_message_by_rfc822(account, rfc822_message_id).await;
        if orig.is_none() {
            for acct in self.connected_accounts() {
                if acct == account {
                    continue;
                }
                if let Some(found) = self.find_message_by_rfc822(&acct, rfc822_message_id).await {
                    orig = Some(found);
                    break;
                }
            }
        }
        let Some(orig) = orig else {
            return Err("message not found in any connected account — check message_id".into());
        };
        let headers = header_map(&orig);
        let connected = self.connected_accounts();
        let plan = derive_reply_plan(
            &headers,
            rfc822_message_id,
            account,
            &connected,
            reply_all,
            allow_self,
        )?;
        Ok((plan.to, plan.cc, plan.subject))
    }

    /// Python `_gmail_reply_send`: reply in-thread (clean body + signature,
    /// correct In-Reply-To/References/threadId) to the message identified by
    /// its RFC822 Message-ID. Cross-account: a thread living on another
    /// connected account threads via headers, not the account-local
    /// threadId.
    #[allow(clippy::too_many_arguments)]
    pub async fn reply_send(
        &self,
        account: &str,
        rfc822_message_id: &str,
        body: &str,
        include_signature: bool,
        reply_all: bool,
        allow_self: bool,
        attachments: &[Attachment],
    ) -> Result<Value, String> {
        let mut orig = self.find_message_by_rfc822(account, rfc822_message_id).await;
        let mut thread_account = account.to_string();
        if orig.is_none() {
            for acct in self.connected_accounts() {
                if acct == account {
                    continue;
                }
                if let Some(found) = self.find_message_by_rfc822(&acct, rfc822_message_id).await {
                    orig = Some(found);
                    thread_account = acct;
                    break;
                }
            }
        }
        let Some(orig) = orig else {
            return Err("message not found in any connected account — check message_id".into());
        };
        let headers = header_map(&orig);
        // threadId is account-local; only valid if the message lives in the
        // SENDING account.
        let orig_thread_id =
            orig.get("threadId").and_then(Value::as_str).unwrap_or("").to_string();
        let thread_id =
            if thread_account == account { orig_thread_id.clone() } else { String::new() };
        let connected = self.connected_accounts();
        let plan = derive_reply_plan(
            &headers,
            rfc822_message_id,
            account,
            &connected,
            reply_all,
            allow_self,
        )?;
        let mut res = self
            .compose_send(
                account,
                &plan.to,
                &plan.subject,
                body,
                &plan.cc,
                &plan.in_reply_to,
                &plan.references,
                &thread_id,
                include_signature,
                attachments,
            )
            .await?;
        // Threading proof for the caller (assertable evidence, ethos rule 4).
        let threaded = !thread_id.is_empty()
            && res.get("thread_id").and_then(Value::as_str) == Some(thread_id.as_str());
        res["orig_thread_id"] =
            json!(if thread_id.is_empty() { orig_thread_id } else { thread_id });
        res["threaded"] = json!(threaded);
        // Resolved envelope back to the caller (GT-58): the send-audit ledger
        // must record WHO a reply went to and under WHAT subject — reply rows
        // logged both as null while send rows carried them, so AMUX-1897
        // forensics could not attribute any reply to a recipient. Both are
        // computed right here for the RFC822 headers; returning them costs
        // nothing and spares every caller a re-resolve of the anchor message.
        res["to"] = json!(plan.to);
        res["subject"] = json!(plan.subject);
        if !plan.cc.is_empty() {
            res["cc"] = json!(plan.cc);
        }
        Ok(res)
    }

    /// Python `_gmail_inbox_messages` minus the 30s cache: unified inbox
    /// shape with the RFC822 Message-ID as `message_id` so it round-trips
    /// into /reply. `days` is AUTHORITATIVE when set (an `after:` filter,
    /// AMUX-1886); `truncated` distinguishes a real 0 from a capped slice.
    pub async fn inbox_messages(
        &self,
        account: &str,
        count: usize,
        q: &str,
        days: f64,
    ) -> Result<Value, String> {
        let want = count.max(1);
        let mut query = q.to_string();
        if query.is_empty() {
            query = "in:inbox".into();
            if days > 0.0 {
                let after = chrono::Utc::now().timestamp() - (days * 86400.0) as i64;
                query.push_str(&format!(" after:{after}"));
            }
        }
        let mut ids: Vec<Value> = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            if ids.len() >= want {
                break;
            }
            let resp = self
                .list_ids(account, &query, (want - ids.len()).min(100), page_token.as_deref())
                .await?;
            if let Some(msgs) = resp.get("messages").and_then(Value::as_array) {
                ids.extend(msgs.iter().cloned());
            }
            page_token =
                resp.get("nextPageToken").and_then(Value::as_str).map(String::from);
            if page_token.is_none() {
                break;
            }
        }
        let truncated = page_token.is_some() && ids.len() >= want;
        ids.truncate(want);
        // AMUX-3337: per-message metadata fetches run with BOUNDED CONCURRENCY,
        // not serially. The serial loop was the latency engine behind the
        // 28s/53s/95s inbox reads the SLA tick ate every 4 hours: N messages
        // cost N sequential Gmail round-trips, so a routine 20-message scan on
        // a slow morning was ~30s and the /api/email p95 reached 106s against
        // an already-chronic 32s baseline. Eight in flight keeps order
        // (`buffered`, not unordered — callers see newest-first as listed) and
        // stays far under Gmail's per-user concurrency limits.
        //
        // AMUX-3520 layered on top: ABOVE the threshold, one batch call per
        // 100 ids does the bulk (count=480: ~5 round-trips instead of ~60
        // 8-wide waves), and only the HOLES a batch part failed for go
        // through the single path — which keeps its 429-retry and loud-drop
        // semantics load-bearing for exactly the requests that need them.
        // Below the threshold the single path runs alone, so its test cells
        // stay pinned to real behavior.
        const BATCH_THRESHOLD: usize = 20;
        const META_HEADERS: [&str; 5] = ["From", "To", "Subject", "Date", "Message-ID"];
        use futures::stream::{self, StreamExt};
        let mids: Vec<String> = ids
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        let batched: Vec<Option<Value>> = if mids.len() > BATCH_THRESHOLD {
            self.batch_metadata(account, &mids, &META_HEADERS).await
        } else {
            vec![None; mids.len()]
        };
        let fetched: Vec<Option<(String, Value)>> = stream::iter(
            mids.into_iter().zip(batched),
        )
        .map(|(mid, pre)| async move {
            if let Some(full) = pre {
                return Some((mid, full));
            }
            let url = self.metadata_url(&mid, &META_HEADERS);
            match self.api(account, "GET", &url, None).await {
                Ok(full) => Some((mid, full)),
                Err(e) => {
                    // AMUX-3495 two-fixes half: this `.ok()` used to swallow
                    // the failure, so a rate-limited fetch didn't slow the
                    // inbox — it silently OMITTED the message. The drop is
                    // now greppable where sweeps look.
                    tracing::warn!(
                        "[email] inbox metadata fetch DROPPED message {mid} for {account}: {e} — the response is missing it"
                    );
                    None
                }
            }
        })
        .buffered(8)
        .collect()
        .await;
        let mut out = Vec::with_capacity(ids.len());
        for (mid, full) in fetched.into_iter().flatten() {
            let h = header_map(&full);
            let hv = |k: &str| h.get(k).cloned().unwrap_or_default();
            let unread = full
                .get("labelIds")
                .and_then(Value::as_array)
                .map(|l| l.iter().any(|x| x.as_str() == Some("UNREAD")))
                .unwrap_or(false);
            out.push(json!({
                "account": account,
                "from": hv("from"),
                "to": hv("to"),
                "date": hv("date"),
                "subject": if hv("subject").is_empty() { "(no subject)".to_string() } else { hv("subject") },
                "message_id": hv("message-id"),
                "thread_id": full.get("threadId").cloned().unwrap_or(json!("")),
                "gmail_id": mid,
                "read": !unread,
                "body": full.get("snippet").cloned().unwrap_or(json!("")),
            }));
        }
        Ok(json!({ "messages": out, "truncated": truncated }))
    }

    /// Python `_gmail_latest_matching`: resolve "the latest message
    /// from/with X" to its RFC822 Message-ID + meta. Fail-open (None) on
    /// any API error — callers use this as a guard, not a gate.
    pub async fn latest_matching(
        &self,
        account: &str,
        from_addr: &str,
        with_addr: &str,
        subject_contains: &str,
        newer_days: i64,
    ) -> Option<Value> {
        let mut q = if !from_addr.is_empty() {
            format!("from:{from_addr}")
        } else if !with_addr.is_empty() {
            format!("(from:{with_addr} OR to:{with_addr})")
        } else {
            return None;
        };
        if newer_days > 0 {
            q.push_str(&format!(" newer_than:{newer_days}d"));
        }
        let res = self.list_ids(account, &q, 10, None).await.ok()?;
        for m in res.get("messages")?.as_array()? {
            let mid = m.get("id")?.as_str()?;
            let url =
                self.metadata_url(mid, &["From", "To", "Subject", "Message-ID", "Date"]);
            let Ok(meta) = self.api(account, "GET", &url, None).await else { continue };
            let h = header_map(&meta);
            let hv = |k: &str| h.get(k).cloned().unwrap_or_default();
            if !subject_contains.is_empty()
                && !hv("subject").to_lowercase().contains(&subject_contains.to_lowercase())
            {
                continue;
            }
            return Some(json!({
                "message_id": hv("message-id"),
                "subject": hv("subject"),
                "from": hv("from"),
                "to": hv("to"),
                "date": hv("date"),
                "gmail_message_id": mid,
                "thread_id": meta.get("threadId").cloned().unwrap_or(json!("")),
                "account": account,
            }));
        }
        None
    }
}

/// Lowercased header-name -> value map from a Gmail message resource.
fn header_map(msg: &Value) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(hs) = msg.pointer("/payload/headers").and_then(Value::as_array) {
        for h in hs {
            if let (Some(n), Some(v)) =
                (h.get("name").and_then(Value::as_str), h.get("value").and_then(Value::as_str))
            {
                out.insert(n.to_lowercase(), v.to_string());
            }
        }
    }
    out
}

/// Same map from a bare PAYLOAD object (a thread's per-message payloads have
/// no `/payload` prefix — `header_map` above reads a full message resource).
fn header_map_of(payload: &Value) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(hs) = payload.get("headers").and_then(Value::as_array) {
        for h in hs {
            if let (Some(n), Some(v)) =
                (h.get("name").and_then(Value::as_str), h.get("value").and_then(Value::as_str))
            {
                out.insert(n.to_lowercase(), v.to_string());
            }
        }
    }
    out
}

/// Gmail sends `internalDate` as a STRING of epoch millis; python coerced
/// with int(). Accept both encodings.
fn internal_date(msg: &Value) -> i64 {
    match msg.get("internalDate") {
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        _ => 0,
    }
}

/// Python `_gmail_decode_body`: recursively extract (html, text) from a
/// message payload — first text/html and first text/plain part win.
fn decode_body(payload: &Value) -> (String, String) {
    let mime = payload.get("mimeType").and_then(Value::as_str).unwrap_or("");
    let data = |p: &Value| -> String {
        let d = p.pointer("/body/data").and_then(Value::as_str).unwrap_or("");
        if d.is_empty() {
            return String::new();
        }
        base64url_decode(d)
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .unwrap_or_default()
    };
    if mime == "text/html" {
        return (data(payload), String::new());
    }
    if mime == "text/plain" {
        return (String::new(), data(payload));
    }
    let mut html = String::new();
    let mut text = String::new();
    if let Some(parts) = payload.get("parts").and_then(Value::as_array) {
        for part in parts {
            let (h, t) = decode_body(part);
            if !h.is_empty() && html.is_empty() {
                html = h;
            }
            if !t.is_empty() && text.is_empty() {
                text = t;
            }
        }
    }
    (html, text)
}

/// Walk a full-format message payload and list its attachments (parts that carry
/// a non-empty `filename`). Each entry names the filename, mime type, size and
/// the Gmail `attachmentId` (fetchable at
/// `/messages/{id}/attachments/{attachmentId}`). Used by the read-one endpoint
/// (AMUX-3354/autodesk): a snippet-only read hid attachments entirely.
pub fn collect_attachments(payload: &Value) -> Vec<Value> {
    fn header(node: &Value, name: &str) -> String {
        node.get("headers")
            .and_then(Value::as_array)
            .and_then(|hs| {
                hs.iter()
                    .find(|h| {
                        h.get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|n| n.eq_ignore_ascii_case(name))
                    })
                    .and_then(|h| h.get("value").and_then(Value::as_str))
            })
            .unwrap_or("")
            .to_string()
    }
    fn walk(node: &Value, out: &mut Vec<Value>) {
        let filename = node.get("filename").and_then(Value::as_str).unwrap_or("");
        if !filename.is_empty() {
            // `filename` is set on INLINE images too (signature logos, cid: images
            // in the HTML), so "has a filename" is not "is a real attachment"
            // (autodesk, AMUX-3354: 9 of 13 parts were one repeated inline logo).
            // Inline iff Content-Disposition says so, or it is absent but the part
            // carries a Content-ID the HTML body references.
            let disp = header(node, "Content-Disposition").to_lowercase();
            let inline = disp.contains("inline")
                || (!disp.contains("attachment") && !header(node, "Content-ID").is_empty());
            out.push(json!({
                "filename": filename,
                "mime_type": node.get("mimeType").and_then(Value::as_str).unwrap_or(""),
                "size": node.pointer("/body/size").and_then(Value::as_i64).unwrap_or(0),
                "attachment_id": node.pointer("/body/attachmentId").and_then(Value::as_str).unwrap_or(""),
                "inline": inline,
            }));
        }
        if let Some(parts) = node.get("parts").and_then(Value::as_array) {
            for p in parts {
                walk(p, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(payload, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Send-audit ledger (Python `_email_log` / GET /api/email/log, AMUX-1897)
// ---------------------------------------------------------------------------

pub fn email_log_path(home: &Path) -> PathBuf {
    home.join("logs").join("email-sent.jsonl")
}

/// Append one JSON audit record per SENT email. Fail-safe: a logging error
/// must never break or block a send (Python parity). Only sends through
/// this API appear here.
pub fn email_log(home: &Path, mut record: Value) {
    let mut write = || -> std::io::Result<()> {
        if record.get("ts").is_none() {
            record["ts"] = json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true));
        }
        let p = email_log_path(home);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p)?;
        writeln!(f, "{record}")?;
        Ok(())
    };
    let _ = write();
}

/// Read the ledger (GET /api/email/log): `days` window, `limit` cap,
/// optional session filter (`unattributed` matches records sent without the
/// header). Response shape identical to Python.
pub fn read_email_log(home: &Path, days: i64, limit: usize, session_filter: &str) -> Value {
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(days))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let mut out: Vec<Value> = Vec::new();
    if let Ok(content) = std::fs::read_to_string(email_log_path(home)) {
        for line in content.lines() {
            let Ok(mut rec) = serde_json::from_str::<Value>(line) else { continue };
            let ts = rec.get("ts").and_then(Value::as_str).unwrap_or("");
            if ts < cutoff.as_str() {
                continue;
            }
            let rsess = match rec.get("session").and_then(Value::as_str) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => "unattributed".to_string(),
            };
            if !session_filter.is_empty() && rsess != session_filter {
                continue;
            }
            rec["session"] = json!(rsess);
            out.push(rec);
        }
    }
    let keep: Vec<Value> = out.iter().rev().take(limit).cloned().collect();
    json!({ "count": keep.len(), "days": days, "log": keep })
}

// ---------------------------------------------------------------------------
// Tests — fixtures + mocked transport only, no network, no live files.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// AMUX-3203 / amux-cloud 2026-08-16: the alert email picked the FIRST-
    /// ALPHABETICAL account, whose refresh_token was dead, while a fresh account
    /// existed, so a live cloud outage paged nobody. Freshness ordering must put
    /// the newest-token account first and, crucially, must NOT pick the first-
    /// alphabetical account when that is the stale one.
    #[test]
    fn accounts_by_freshness_prefers_the_newest_token_over_first_alphabetical() {
        let dir = tempfile::tempdir().unwrap();
        let tok = dir.path().join("gmail-tokens");
        std::fs::create_dir_all(&tok).unwrap();
        for a in ["esteininger21@gmail.com", "ethan@mixpeek.com", "info@mixpeek.com"] {
            std::fs::write(tok.join(format!("{a}.json")), "{}").unwrap();
        }
        // touch -t [[CC]YY]MMDDhhmm: esteininger21 stale (Jul), ethan newest (Aug 16).
        let set = |name: &str, stamp: &str| {
            std::process::Command::new("touch")
                .args(["-t", stamp, tok.join(format!("{name}.json")).to_str().unwrap()])
                .status()
                .unwrap();
        };
        set("esteininger21@gmail.com", "202607160000");
        set("info@mixpeek.com", "202608150000");
        set("ethan@mixpeek.com", "202608161200");

        let order = connected_accounts_by_freshness_in(dir.path());
        assert_eq!(order.first().map(String::as_str), Some("ethan@mixpeek.com"), "newest first: {order:?}");
        assert_eq!(order.last().map(String::as_str), Some("esteininger21@gmail.com"), "stale last: {order:?}");
        // The incident, made explicit: first-alphabetical IS the dead account, and
        // freshness ordering must not pick it.
        let mut alpha = order.clone();
        alpha.sort();
        assert_eq!(alpha.first().map(String::as_str), Some("esteininger21@gmail.com"));
        assert_ne!(order.first(), alpha.first(), "freshness must not pick the dead first-alphabetical account");

        // Age helper: None with no accounts, small for a just-written token.
        assert_eq!(newest_token_age_secs_in(&dir.path().join("nope"), std::time::SystemTime::now()), None);
    }

    #[test]
    fn base64_vectors() {
        // RFC 4648 vectors.
        assert_eq!(base64_std(b""), "");
        assert_eq!(base64_std(b"f"), "Zg==");
        assert_eq!(base64_std(b"fo"), "Zm8=");
        assert_eq!(base64_std(b"foo"), "Zm9v");
        assert_eq!(base64_std(b"foobar"), "Zm9vYmFy");
        // urlsafe alphabet: 0xfb 0xff -> "-_8=" (would be "+/8=" in std).
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8=");
        assert_eq!(base64url_nopad(&[0xfb, 0xff]), "-_8");
        // round trip
        let data = b"amux \xf0\x9f\x93\xa7 bytes";
        assert_eq!(base64url_decode(&base64url(data)).unwrap(), data.to_vec());
    }

    #[test]
    fn html_escape_matches_python_quote_true() {
        assert_eq!(html_escape(r#"<a href="x">&'b'</a>"#), "&lt;a href=&quot;x&quot;&gt;&amp;&#x27;b&#x27;&lt;/a&gt;");
    }

    #[test]
    fn sig_html_to_text_keeps_breaks_drops_tags() {
        let sig = "<div><b>Ethan</b><br>Founder, Mixpeek</div><p>ethan&amp;co</p>";
        assert_eq!(sig_html_to_text(sig), "Ethan\nFounder, Mixpeek\nethan&co");
        assert_eq!(sig_html_to_text(""), "");

        // THE REAL SIGNATURE, not a convenient one (Ethan, 2026-08-24:
        // "formatting wasn't retained"). Lifted from the sent MIME of the
        // primis reply — Gmail's editor puts an inline style on EVERY break,
        // and the old `<\s*br\s*/?>` matched none of them, so they fell through
        // to the tag stripper and were deleted.
        //
        // The bare-`<br>` fixture above passed the whole time. It is convenient
        // precisely because it lacks the property that made the incident.
        let real = concat!(
            r#"Ethan Steininger<br>Founder &amp; CEO @ "#,
            r#"<a href="https://mixpeek.com/" style="font-size:13px">Mixpeek</a> "#,
            r#"<br style="color:rgb(213,218,222);font-size:13px">"#,
            r#"<br style="color:rgb(213,218,222);font-size:13px">"#,
            r#"<a href="https://www.linkedin.com/in/ethansteininger/" style="font-size:13px">LinkedIn</a>"#,
            r#"<br style="color:rgb(213,218,222);font-size:13px">"#,
            r#"<a href="https://calendly.com/mixpeek" style="font-size:13px">Schedule time with me</a>"#,
        );
        let txt = sig_html_to_text(real);
        assert!(
            !txt.contains("LinkedInSchedule"),
            "the exact string a customer received: two link labels concatenated because the \
             styled <br> between them was dropped: {txt:?}"
        );
        assert!(
            txt.contains("Mixpeek") && txt.contains("LinkedIn") && txt.contains("Schedule time"),
            "every label must survive — the fix must not trade a lost break for lost text: {txt:?}"
        );
        // Pinned as the exact rendering rather than a line count, because my
        // first draft asserted 4 and the truth is 5: the doubled `<br><br>`
        // between the title and the links becomes a real BLANK line, which is
        // correct and which `lines()` counts. The code was right and the
        // expectation was wrong, so the expectation moved.
        assert_eq!(
            txt,
            "Ethan Steininger\nFounder & CEO @ Mixpeek\n\nLinkedIn\nSchedule time with me",
            "the whole point is the shape, so assert the shape: {txt:?}"
        );

        // The boundary that keeps the widened pattern honest: `<br` must not
        // swallow a tag that merely starts with those letters.
        assert_eq!(
            sig_html_to_text("a<brother>b"),
            "ab",
            "<brother> is stripped as an unknown tag, NOT treated as a line break"
        );
        // Self-closing and upper-case with attributes, both real in the wild.
        assert_eq!(sig_html_to_text("a<BR CLASS='x'/>b"), "a\nb");
    }

    #[test]
    fn address_parsing_handles_quoted_names_and_lists() {
        assert_eq!(
            parse_addresses(r#""Howard, Mark" <mhoward@lucihub.com>, jane@x.co, Bob <bob@y.z>"#),
            vec!["mhoward@lucihub.com", "jane@x.co", "bob@y.z"]
        );
        assert_eq!(parse_addresses(""), Vec::<String>::new());
    }

    /// Structure must survive a client that strips CSS (Ethan, 2026-08-25).
    ///
    /// THE SPECIMEN: the welcome email to siripuru.v@media.net rendered as a
    /// wall of text — "adtech-platform-docs You work in adtech", breaks lost
    /// mid-sentence — while its MIME was correct: 11 blank-line breaks in
    /// text/plain and `pre-wrap` with raw newlines in the HTML, both read back
    /// off the SENT message.
    ///
    /// The signature in that same message rendered correctly, and it is built
    /// from real `<br>`. One client, one message, two mechanisms, one survivor.
    ///
    /// This cell asserts on the MECHANISM rather than on the bytes, because
    /// reading the bytes back is exactly the verification that passed while the
    /// defect was live.
    #[test]
    fn html_breaks_survive_a_client_that_strips_white_space_css() {
        let (_plain, html, _) = compose_bodies("para one\n\npara two\nnext line", "");

        // A blank line is TWO breaks; a single newline is one.
        assert!(html.contains("para one<br><br>para two<br>next line"), "{html}");

        // NO RAW NEWLINE SURVIVES IN THE BODY. Under `pre-wrap` a `<br>`
        // followed by a literal newline renders as TWO breaks, so keeping both
        // would double every paragraph gap on any client that DOES honour the
        // style — trading a wall of text for a sparse one.
        let body_part = html.split("</div>").next().unwrap();
        assert!(
            !body_part.contains('\n'),
            "a raw newline beside a <br> doubles the gap under pre-wrap: {body_part:?}"
        );

        // The div stays: it is the only thing carrying leading indentation,
        // which <br> cannot express.
        assert!(html.contains("white-space:pre-wrap"), "{html}");

        // AND THE PLAIN PART IS UNTOUCHED. It is the alternative a text-only
        // client renders, and it was never the broken half — changing it to
        // chase an HTML bug would break the one part that worked.
        let (plain, _, _) = compose_bodies("para one\n\npara two", "");
        assert_eq!(plain, "para one\n\npara two");
    }

    #[test]
    fn compose_bodies_escapes_and_appends_signature() {
        let (plain, html, inc) = compose_bodies("hi <b>\nline2", "<b>Sig</b>");
        assert_eq!(plain, "hi <b>\nline2\n\nSig");
        // pre-wrap AND `<br>`, with the newline REPLACED rather than kept
        // (Ethan, 2026-08-25). This cell used to demand the opposite — "newline
        // preserved, not <br>-ified" — and that was a reasonable reading of the
        // 2026-08-13 evidence, which was about `white-space:normal` collapsing
        // indentation. It was overturned by a rendered screenshot: a message
        // whose MIME was verifiably correct still arrived as a wall of text,
        // because the client sanitised the style. The signature in the same
        // message, built from real `<br>`, rendered fine.
        //
        // The doubling worry the old comment names is real and is why the
        // newline is REPLACED, not accompanied: `<br>` plus a literal newline
        // renders as two breaks wherever pre-wrap IS honoured.
        assert_eq!(
            html,
            "<div style=\"white-space:pre-wrap;\">hi &lt;b&gt;<br>line2</div><br><br><b>Sig</b>"
        );
        assert!(html.contains("pre-wrap"), "the div still carries indentation: {html}");
        assert!(inc);
        let (plain2, html2, inc2) = compose_bodies("hi", "");
        assert_eq!(plain2, "hi");
        assert_eq!(html2, "<div style=\"white-space:pre-wrap;\">hi</div>");
        assert!(!inc2);
    }

    #[test]
    fn rfc822_with_attachment_is_multipart_mixed() {
        // AMUX-3357: attachments wrap the alternative body in a multipart/mixed.
        let att = Attachment {
            filename: "report.pdf".into(),
            content_type: "application/pdf".into(),
            data: b"PDF-BYTES".to_vec(),
        };
        let spec = MimeSpec {
            from: "a@x.com",
            to: "b@y.com",
            cc: "",
            subject: "hi",
            in_reply_to: "",
            references: "",
            plain: "hello",
            html: "<p>hello</p>",
            boundary: "=_bnd",
            attachments: std::slice::from_ref(&att),
        };
        let msg = build_rfc822(&spec);
        assert!(msg.contains("Content-Type: multipart/mixed; boundary=\"=_bnd_mix\""), "{msg}");
        assert!(msg.contains("Content-Type: multipart/alternative; boundary=\"=_bnd\""));
        assert!(msg.contains("Content-Disposition: attachment; filename=\"report.pdf\""));
        assert!(msg.contains("Content-Type: application/pdf; name=\"report.pdf\""));
        assert!(msg.contains(&base64_std(b"PDF-BYTES")), "attachment bytes not base64-embedded");
        assert!(msg.trim_end().ends_with("--=_bnd_mix--"), "mixed container must close last: {msg}");
        // A path-traversing filename is reduced to its basename in the headers.
        let att2 = Attachment { filename: "../../etc/passwd".into(), content_type: String::new(), data: vec![1] };
        let spec2 = MimeSpec { attachments: std::slice::from_ref(&att2), ..spec };
        let msg2 = build_rfc822(&spec2);
        assert!(msg2.contains("filename=\"passwd\""), "filename must be a basename: {msg2}");
        assert!(msg2.contains("Content-Type: application/octet-stream"), "empty ct -> octet-stream");
    }

    #[test]
    fn collect_attachments_walks_nested_parts() {
        // AMUX-3354: a full payload's attachments are enumerated from any depth.
        let payload = json!({
            "mimeType": "multipart/mixed",
            "parts": [
                { "mimeType": "multipart/alternative", "parts": [
                    { "mimeType": "text/plain", "body": {"data": "aGk"} },
                    { "mimeType": "text/html", "body": {"data": "PHA+"} }
                ]},
                { "mimeType": "application/pdf", "filename": "report.pdf",
                  "headers": [{"name":"Content-Disposition","value":"attachment; filename=report.pdf"}],
                  "body": {"size": 1234, "attachmentId": "att-1"} },
                // A cid: signature logo — has a filename but is INLINE, not a file.
                { "mimeType": "image/png", "filename": "logo.png",
                  "headers": [{"name":"Content-ID","value":"<logo@sig>"},{"name":"Content-Disposition","value":"inline"}],
                  "body": {"size": 50, "attachmentId": "att-2"} }
            ]
        });
        let atts = collect_attachments(&payload);
        assert_eq!(atts.len(), 2);
        let pdf = atts.iter().find(|a| a["filename"] == json!("report.pdf")).unwrap();
        assert_eq!(pdf["attachment_id"], json!("att-1"));
        assert_eq!(pdf["size"], json!(1234));
        assert_eq!(pdf["inline"], json!(false), "a real file is not inline");
        let logo = atts.iter().find(|a| a["filename"] == json!("logo.png")).unwrap();
        assert_eq!(logo["inline"], json!(true), "a cid: signature logo is inline");
        // A plain body-only message has no attachments.
        assert!(collect_attachments(&json!({"mimeType":"text/plain","body":{"data":"aGk"}})).is_empty());
    }

    #[test]
    fn rfc822_fixture_with_threading_headers() {
        let spec = MimeSpec {
            from: "sender@example.com",
            to: "rcpt@example.org",
            cc: "cc@example.org",
            subject: "Hello",
            in_reply_to: "<orig@id>",
            references: "<root@id> <orig@id>",
            plain: "plain body",
            html: "<div>plain body</div>",
            boundary: "=_amux_test",
            attachments: &[],
        };
        let msg = build_rfc822(&spec);
        let want = concat!(
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/alternative; boundary=\"=_amux_test\"\r\n",
            "To: rcpt@example.org\r\n",
            "From: sender@example.com\r\n",
            "Subject: Hello\r\n",
            "Cc: cc@example.org\r\n",
            "In-Reply-To: <orig@id>\r\n",
            "References: <root@id> <orig@id>\r\n",
            "\r\n",
            "--=_amux_test\r\n",
            "Content-Type: text/plain; charset=\"utf-8\"\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "cGxhaW4gYm9keQ==\r\n",
            "--=_amux_test\r\n",
            "Content-Type: text/html; charset=\"utf-8\"\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "PGRpdj5wbGFpbiBib2R5PC9kaXY+\r\n",
            "--=_amux_test--\r\n",
        );
        assert_eq!(msg, want);
    }

    #[test]
    fn rfc822_new_message_has_no_threading_headers() {
        let spec = MimeSpec {
            from: "a@b.c",
            to: "d@e.f",
            cc: "",
            subject: "S",
            in_reply_to: "",
            references: "",
            plain: "p",
            html: "h",
            boundary: "=_b",
            attachments: &[],
        };
        let msg = build_rfc822(&spec);
        assert!(!msg.contains("In-Reply-To"));
        assert!(!msg.contains("References"));
        assert!(!msg.contains("Cc:"));
    }

    #[test]
    fn rfc822_references_defaults_to_in_reply_to() {
        let spec = MimeSpec {
            from: "a@b.c",
            to: "d@e.f",
            cc: "",
            subject: "S",
            in_reply_to: "<only@id>",
            references: "",
            plain: "p",
            html: "h",
            boundary: "=_b",
            attachments: &[],
        };
        let msg = build_rfc822(&spec);
        assert!(msg.contains("References: <only@id>\r\n"), "{msg}");
    }

    #[test]
    fn non_ascii_subject_is_rfc2047_encoded() {
        assert_eq!(encode_header_value("plain"), "plain");
        let enc = encode_header_value("héllo");
        assert!(enc.starts_with("=?utf-8?b?") && enc.ends_with("?="), "{enc}");
        assert_eq!(base64url_decode(&enc[10..enc.len() - 2]).unwrap(), "héllo".as_bytes());
    }

    // ---- reply derivation -------------------------------------------------

    fn hdrs(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    const ME: &str = "owner@mixpeek.com";
    fn connected() -> Vec<String> {
        vec!["owner@mixpeek.com".into(), "info@mixpeek.com".into()]
    }

    #[test]
    fn reply_targets_external_sender_and_chains_references() {
        let h = hdrs(&[
            ("message-id", "<orig@ext>"),
            ("subject", "Deal"),
            ("from", "Prospect <p@customer.com>"),
            ("to", "owner@mixpeek.com"),
            ("references", "<root@ext>"),
        ]);
        let plan = derive_reply_plan(&h, "<orig@ext>", ME, &connected(), false, false).unwrap();
        assert_eq!(plan.to, "p@customer.com");
        assert_eq!(plan.subject, "Re: Deal");
        assert_eq!(plan.in_reply_to, "<orig@ext>");
        assert_eq!(plan.references, "<root@ext> <orig@ext>");
        assert_eq!(plan.cc, "");
    }

    #[test]
    fn reply_to_own_sent_message_goes_to_original_recipients() {
        // The reply-to-self bug: replying to our OWN sent message must
        // target the external To, not us.
        let h = hdrs(&[
            ("message-id", "<sent@us>"),
            ("subject", "Re: Deal"),
            ("from", "Us <owner@mixpeek.com>"),
            ("to", "p@customer.com, other@customer.com"),
        ]);
        let plan = derive_reply_plan(&h, "<sent@us>", ME, &connected(), false, false).unwrap();
        assert_eq!(plan.to, "p@customer.com, other@customer.com");
        // subject already Re: — not doubled.
        assert_eq!(plan.subject, "Re: Deal");
    }

    #[test]
    fn reply_all_dedupes_and_excludes_all_owned() {
        let h = hdrs(&[
            ("message-id", "<m@x>"),
            ("subject", "s"),
            ("from", "a@ext.com"),
            ("to", "owner@mixpeek.com, b@ext.com, a@ext.com"),
            ("cc", "info@mixpeek.com, c@ext.com, teammate@trymixpeek.com"),
        ]);
        let plan = derive_reply_plan(&h, "<m@x>", ME, &connected(), true, false).unwrap();
        assert_eq!(plan.to, "a@ext.com, b@ext.com, c@ext.com");
    }

    #[test]
    fn no_external_recipient_is_a_hard_error_with_pythons_message() {
        let h = hdrs(&[
            ("message-id", "<m@x>"),
            ("subject", "s"),
            ("from", "owner@mixpeek.com"),
            ("to", "info@mixpeek.com"),
        ]);
        let err = derive_reply_plan(&h, "<m@x>", ME, &connected(), false, false).unwrap_err();
        assert!(err.starts_with("no external recipient on this thread (would email ourselves)"), "{err}");
        assert!(err.contains("allow_self"), "{err}");
    }

    #[test]
    fn allow_self_enables_owned_account_threading_test() {
        let h = hdrs(&[
            ("message-id", "<m@x>"),
            ("subject", "s"),
            ("from", "info@mixpeek.com"),
            ("to", "owner@mixpeek.com"),
        ]);
        let plan = derive_reply_plan(&h, "<m@x>", ME, &connected(), false, true).unwrap();
        assert_eq!(plan.to, "info@mixpeek.com"); // the other owned account, never the sender
    }

    #[test]
    fn missing_angle_bracket_gets_pythons_exact_fixup() {
        let h = hdrs(&[("subject", "s"), ("from", "x@ext.com"), ("message-id", "bare@id>")]);
        let plan = derive_reply_plan(&h, "bare@id>", ME, &connected(), false, false).unwrap();
        // Python wraps unconditionally when no leading '<' (amux-server.py:26957
        // f"<{id}>"), so a trailing '>' doubles. Parity beats prettiness: the
        // doubled form is what the Python server puts on the wire today.
        assert_eq!(plan.in_reply_to, "<bare@id>>");
    }

    // ---- mocked transport: token refresh + API flows ----------------------

    /// Scripted transport: matches on URL substrings, records every call.
    struct MockHttp {
        calls: Mutex<Vec<(String, String, Option<Value>)>>,
        /// (method, url_substring, status, response) — first match wins,
        /// single-use entries pop so a retry can get a different answer.
        script: Mutex<Vec<(String, String, u16, Value)>>,
    }

    impl MockHttp {
        fn new(script: Vec<(&str, &str, u16, Value)>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                script: Mutex::new(
                    script
                        .into_iter()
                        .map(|(m, u, s, v)| (m.to_string(), u.to_string(), s, v))
                        .collect(),
                ),
            })
        }
        fn answer(&self, method: &str, url: &str, body: Option<&Value>) -> Result<(u16, Value), String> {
            self.calls.lock().unwrap().push((method.into(), url.into(), body.cloned()));
            let mut script = self.script.lock().unwrap();
            if let Some(pos) = script
                .iter()
                .position(|(m, sub, _, _)| m == method && url.contains(sub.as_str()))
            {
                let (_, _, status, v) = script.remove(pos);
                return Ok((status, v));
            }
            Err(format!("mock has no answer for {method} {url}"))
        }
    }

    #[async_trait]
    impl HttpTransport for MockHttp {
        async fn get(&self, url: &str, _bearer: Option<&str>) -> Result<(u16, Value), String> {
            self.answer("GET", url, None)
        }
        async fn post_json(
            &self,
            url: &str,
            _bearer: Option<&str>,
            body: &Value,
        ) -> Result<(u16, Value), String> {
            self.answer("POST", url, Some(body))
        }
        async fn post_form(
            &self,
            url: &str,
            form: &[(String, String)],
        ) -> Result<(u16, Value), String> {
            let v = Value::Object(
                form.iter().map(|(k, val)| (k.clone(), json!(val))).collect(),
            );
            self.answer("FORM", url, Some(&v))
        }
        async fn post_raw(
            &self,
            url: &str,
            _bearer: Option<&str>,
            _content_type: &str,
            body: String,
        ) -> Result<(u16, String), String> {
            // Scripted like every other verb; the multipart RESPONSE rides in
            // the script's Value as a plain string.
            let (st, v) = self.answer("RAW", url, Some(&Value::String(body)))?;
            Ok((st, v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
        }
    }

    /// Temp home with a synthetic token file. PLACEHOLDER strings only — no
    /// credential values anywhere in tests.
    fn temp_home_with_token(account: &str, with_access_token: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let tokens = dir.path().join("gmail-tokens");
        std::fs::create_dir_all(&tokens).unwrap();
        let mut tf = json!({
            "refresh_token": "PLACEHOLDER_REFRESH",
            "token_uri": "https://oauth2.googleapis.com/token",
            "client_id": "PLACEHOLDER_CLIENT_ID",
            "client_secret": "PLACEHOLDER_CLIENT_SECRET",
        });
        if with_access_token {
            tf["token"] = json!("PLACEHOLDER_STALE_ACCESS");
        }
        std::fs::write(tokens.join(format!("{account}.json")), tf.to_string()).unwrap();
        dir
    }

    #[tokio::test]
    async fn token_refresh_posts_correct_form_and_persists() {
        let home = temp_home_with_token("acct@example.com", false);
        let http = MockHttp::new(vec![
            ("FORM", "oauth2.googleapis.com/token", 200, json!({ "access_token": "FRESH", "expires_in": 3599 })),
            ("GET", "/settings/sendAs", 200, json!({ "sendAs": [] })),
        ]);
        let client = GmailClient::new(http.clone(), home.path().to_path_buf());
        // get_signature forces one authenticated call -> refresh first.
        let sig = client.get_signature("acct@example.com").await;
        assert_eq!(sig, "");
        let calls = http.calls.lock().unwrap();
        let (_, _, form) = &calls[0];
        let form = form.as_ref().unwrap();
        assert_eq!(form["grant_type"], json!("refresh_token"));
        assert_eq!(form["refresh_token"], json!("PLACEHOLDER_REFRESH"));
        assert_eq!(form["client_id"], json!("PLACEHOLDER_CLIENT_ID"));
        assert_eq!(form["client_secret"], json!("PLACEHOLDER_CLIENT_SECRET"));
        // Refreshed token persisted back in Python's file shape.
        let persisted: Value = serde_json::from_str(
            &std::fs::read_to_string(
                home.path().join("gmail-tokens").join("acct@example.com.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(persisted["token"], json!("FRESH"));
        assert_eq!(persisted["refresh_token"], json!("PLACEHOLDER_REFRESH"));
    }

    #[tokio::test]
    async fn stale_access_token_is_refreshed_on_401_and_retried() {
        let home = temp_home_with_token("acct@example.com", true);
        let http = MockHttp::new(vec![
            // First API call uses the stale stored token -> 401.
            ("POST", "/messages/send", 401, json!({ "error": { "code": 401 } })),
            ("FORM", "oauth2.googleapis.com/token", 200, json!({ "access_token": "FRESH2" })),
            // Retry succeeds.
            ("POST", "/messages/send", 200, json!({ "id": "m1", "threadId": "t1" })),
        ]);
        let client = GmailClient::new(http.clone(), home.path().to_path_buf());
        let res = client
            .compose_send("acct@example.com", "x@ext.com", "S", "B", "", "", "", "", false, &[])
            .await
            .unwrap();
        assert_eq!(res["ok"], json!(true));
        assert_eq!(res["id"], json!("m1"));
        assert_eq!(res["thread_id"], json!("t1"));
        assert_eq!(res["signature_included"], json!(false));
        // Exactly one refresh happened, after the 401.
        let calls = http.calls.lock().unwrap();
        let kinds: Vec<&str> = calls.iter().map(|(m, _, _)| m.as_str()).collect();
        assert_eq!(kinds, vec!["POST", "FORM", "POST"]);
    }

    #[tokio::test]
    async fn invalid_grant_surfaces_in_the_error_text() {
        let home = temp_home_with_token("acct@example.com", false);
        let http = MockHttp::new(vec![(
            "FORM",
            "oauth2.googleapis.com/token",
            400,
            json!({ "error": "invalid_grant", "error_description": "Token has been revoked." }),
        )]);
        let client = GmailClient::new(http, home.path().to_path_buf());
        let err = client
            .compose_send("acct@example.com", "x@ext.com", "S", "B", "", "", "", "", false, &[])
            .await
            .unwrap_err();
        assert!(err.contains("invalid_grant"), "{err}");
    }

    #[tokio::test]
    async fn compose_send_raw_decodes_to_rfc822_with_signature_and_thread() {
        let home = temp_home_with_token("acct@example.com", true);
        let http = MockHttp::new(vec![
            ("GET", "/settings/sendAs", 200, json!({ "sendAs": [
                { "sendAsEmail": "acct@example.com", "signature": "<b>Sig</b>" },
            ]})),
            ("POST", "/messages/send", 200, json!({ "id": "m2", "threadId": "T9" })),
        ]);
        let client = GmailClient::new(http.clone(), home.path().to_path_buf());
        let res = client
            .compose_send(
                "acct@example.com",
                "x@ext.com",
                "Subj",
                "hello",
                "c@ext.com",
                "<orig@id>",
                "<root@id> <orig@id>",
                "T9",
                true,
                &[],
            )
            .await
            .unwrap();
        assert_eq!(res["signature_included"], json!(true));
        let calls = http.calls.lock().unwrap();
        let (_, _, body) = calls.iter().find(|(m, u, _)| m == "POST" && u.contains("/messages/send")).unwrap();
        let body = body.as_ref().unwrap();
        assert_eq!(body["threadId"], json!("T9"));
        let raw = body["raw"].as_str().unwrap();
        let decoded = String::from_utf8(base64url_decode(raw).unwrap()).unwrap();
        assert!(decoded.contains("To: x@ext.com\r\n"), "{decoded}");
        assert!(decoded.contains("Cc: c@ext.com\r\n"));
        assert!(decoded.contains("In-Reply-To: <orig@id>\r\n"));
        assert!(decoded.contains("References: <root@id> <orig@id>\r\n"));
        // Signature reached both alternatives (encoded in base64 parts).
        assert!(decoded.contains(&wrap76(&base64_std("hello\n\nSig".as_bytes()))));
        assert!(decoded.contains(&wrap76(&base64_std(
            "<div style=\"white-space:pre-wrap;\">hello</div><br><br><b>Sig</b>".as_bytes()
        ))));
    }

    #[tokio::test]
    async fn reply_send_threads_in_account_and_reports_proof() {
        let home = temp_home_with_token("acct@example.com", true);
        let orig = json!({
            "id": "g1", "threadId": "T1",
            "payload": { "headers": [
                { "name": "Message-ID", "value": "<orig@ext>" },
                { "name": "Subject", "value": "Deal" },
                { "name": "From", "value": "P <p@customer.com>" },
                { "name": "To", "value": "acct@example.com" },
                { "name": "References", "value": "<root@ext>" },
            ]},
        });
        let http = MockHttp::new(vec![
            ("GET", "q=rfc822msgid", 200, json!({ "messages": [{ "id": "g1" }] })),
            ("GET", "/messages/g1", 200, orig),
            ("POST", "/messages/send", 200, json!({ "id": "m3", "threadId": "T1" })),
        ]);
        let client = GmailClient::new(http.clone(), home.path().to_path_buf());
        let res = client
            .reply_send("acct@example.com", "<orig@ext>", "thanks!", false, false, false, &[])
            .await
            .unwrap();
        assert_eq!(res["threaded"], json!(true));
        assert_eq!(res["orig_thread_id"], json!("T1"));
        // GT-58: the resolved envelope comes back so the audit ledger can
        // record recipient + subject for a reply, not just for a send.
        assert_eq!(res["to"], json!("p@customer.com"));
        assert_eq!(res["subject"], json!("Re: Deal"));
        let calls = http.calls.lock().unwrap();
        let (_, _, body) = calls.iter().find(|(m, u, _)| m == "POST" && u.contains("/messages/send")).unwrap();
        let body = body.as_ref().unwrap();
        assert_eq!(body["threadId"], json!("T1"));
        let decoded =
            String::from_utf8(base64url_decode(body["raw"].as_str().unwrap()).unwrap()).unwrap();
        assert!(decoded.contains("To: p@customer.com\r\n"), "{decoded}");
        assert!(decoded.contains("Subject: Re: Deal\r\n"));
        assert!(decoded.contains("In-Reply-To: <orig@ext>\r\n"));
        assert!(decoded.contains("References: <root@ext> <orig@ext>\r\n"));
    }

    #[tokio::test]
    async fn reply_send_missing_message_is_pythons_error() {
        let home = temp_home_with_token("acct@example.com", true);
        let http = MockHttp::new(vec![(
            "GET",
            "q=rfc822msgid",
            200,
            json!({ "messages": [] }),
        )]);
        let client = GmailClient::new(http, home.path().to_path_buf());
        let err = client
            .reply_send("acct@example.com", "<gone@id>", "b", false, false, false, &[])
            .await
            .unwrap_err();
        assert_eq!(err, "message not found in any connected account — check message_id");
    }

    #[tokio::test]
    async fn inbox_builds_authoritative_after_query_and_marks_truncation() {
        let home = temp_home_with_token("acct@example.com", true);
        let meta = |mid: &str, msgid: &str| {
            json!({
                "id": mid, "threadId": "T", "snippet": "snip", "labelIds": ["INBOX", "UNREAD"],
                "payload": { "headers": [
                    { "name": "From", "value": "a@ext.com" },
                    { "name": "To", "value": "acct@example.com" },
                    { "name": "Subject", "value": "s" },
                    { "name": "Date", "value": "Sun, 09 Aug 2026 10:00:00 -0400" },
                    { "name": "Message-ID", "value": msgid },
                ]},
            })
        };
        let http = MockHttp::new(vec![
            ("GET", "/messages?q=", 200, json!({
                "messages": [{ "id": "g1" }, { "id": "g2" }],
                "nextPageToken": "more",
            })),
            ("GET", "/messages/g1", 200, meta("g1", "<m1@x>")),
            ("GET", "/messages/g2", 200, meta("g2", "<m2@x>")),
        ]);
        let client = GmailClient::new(http.clone(), home.path().to_path_buf());
        let res = client.inbox_messages("acct@example.com", 2, "", 3.0).await.unwrap();
        // Window held more than the cap: a caller can tell 0 from capped.
        assert_eq!(res["truncated"], json!(true));
        let msgs = res["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["message_id"], json!("<m1@x>"));
        assert_eq!(msgs[0]["read"], json!(false));
        assert_eq!(msgs[0]["body"], json!("snip"));
        assert_eq!(msgs[0]["account"], json!("acct@example.com"));
        // The list call carried in:inbox + an epoch after: filter.
        let calls = http.calls.lock().unwrap();
        let url = &calls.iter().find(|(m, u, _)| m == "GET" && u.contains("/messages?q=")).unwrap().1;
        assert!(url.contains(&urlencode("in:inbox after:")[..20]), "{url}");
    }

    /// AMUX-3495 — a 429 must RETRY, not silently drop the message. The
    /// 8-wide inbox fetch sits just under Gmail's quota budget and its
    /// per-message caller used to `.ok()` failures, so quota pushback OMITTED
    /// messages from the response with nothing anywhere saying so — a reader
    /// concludes "no such mail". One 429 then 200 for the same id: the
    /// message must arrive; a SUSTAINED 429 (all retries eaten) must drop
    /// with the other message still served, never fail the whole inbox.
    #[tokio::test]
    async fn a_transient_429_retries_and_the_message_still_arrives() {
        let home = temp_home_with_token("acct@example.com", true);
        let meta = json!({
            "id": "g1", "threadId": "T", "snippet": "snip", "labelIds": ["INBOX"],
            "payload": { "headers": [
                { "name": "From", "value": "a@ext.com" },
                { "name": "Subject", "value": "s" },
                { "name": "Message-ID", "value": "<m1@x>" },
            ]},
        });
        let http = MockHttp::new(vec![
            ("GET", "/messages?q=", 200, json!({ "messages": [{ "id": "g1" }] })),
            ("GET", "/messages/g1", 429, json!({ "error": "rateLimitExceeded" })),
            ("GET", "/messages/g1", 200, meta),
        ]);
        let client = GmailClient::new(http.clone(), home.path().to_path_buf());
        let res = client.inbox_messages("acct@example.com", 1, "", 3.0).await.unwrap();
        let msgs = res["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1, "the 429'd message must be retried into the response");
        assert_eq!(msgs[0]["message_id"], json!("<m1@x>"));
        // Both the 429 and the retry hit the wire.
        let gets = http.calls.lock().unwrap().iter().filter(|(m, u, _)| m == "GET" && u.contains("/messages/g1")).count();
        assert_eq!(gets, 2);
    }

    /// AMUX-3520 — the framing is the contract: the builder's Content-IDs
    /// are what re-align Google's out-of-order response parts, and the
    /// parser must survive exactly the shapes Google sends (CRLF, angle
    /// brackets, a non-JSON error part) by SKIPPING, never by mis-mapping.
    #[test]
    fn batch_multipart_builder_and_parser_round_google_shapes() {
        let body = batch_request_body("B", &["/gmail/v1/users/me/messages/a?x=1".into(),
                                             "/gmail/v1/users/me/messages/b?x=1".into()]);
        assert!(body.contains("--B\r\nContent-Type: application/http\r\nContent-ID: <item0>"));
        assert!(body.contains("GET /gmail/v1/users/me/messages/b?x=1 HTTP/1.1"));
        assert!(body.ends_with("--B--\r\n"));

        // Google-style response: parts OUT OF ORDER, one embedded 404, one
        // garbage part. Only the good parts map, each to its own index.
        let resp = "--batch_r\r\nContent-Type: application/http\r\nContent-ID: <response-item1>\r\n\r\nHTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"id\":\"g1\"}\r\n\r\n--batch_r\r\nContent-Type: application/http\r\nContent-ID: <response-item0>\r\n\r\nHTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\n\r\n{\"error\":{\"code\":404}}\r\n\r\n--batch_r\r\nContent-ID: <response-item2>\r\n\r\nnot http at all\r\n--batch_r--\r\n";
        let parts = parse_batch_parts(resp);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts.contains(&(1, 200, serde_json::json!({"id":"g1"}))));
        assert!(parts.iter().any(|(i, st, _)| *i == 0 && *st == 404));
    }

    /// AMUX-3520 end to end: above the threshold ONE batch call does the
    /// bulk and only the batch's holes go through the single path — which is
    /// also the cell proving a partial batch cannot lose a message.
    #[tokio::test]
    async fn inbox_batches_above_threshold_and_single_fetches_the_holes() {
        let home = temp_home_with_token("acct@example.com", true);
        let n = 25usize;
        let ids: Vec<Value> = (0..n).map(|i| json!({"id": format!("g{i}")})).collect();
        // Canned RESPONSE multipart: every part except item7 (the hole).
        let mut resp = String::new();
        for i in (0..n).rev() {
            // reverse order on purpose: Content-ID is what re-aligns
            if i == 7 {
                continue;
            }
            resp.push_str(&format!(
                "--batch_r\r\nContent-Type: application/http\r\nContent-ID: <response-item{i}>\r\n\r\nHTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{{\"id\":\"g{i}\",\"threadId\":\"T\",\"snippet\":\"s\",\"payload\":{{\"headers\":[{{\"name\":\"Message-ID\",\"value\":\"<m{i}@x>\"}}]}}}}\r\n\r\n"
            ));
        }
        resp.push_str("--batch_r--\r\n");
        let meta7 = json!({
            "id": "g7", "threadId": "T", "snippet": "s",
            "payload": { "headers": [ { "name": "Message-ID", "value": "<m7@x>" } ] },
        });
        let http = MockHttp::new(vec![
            ("GET", "/messages?q=", 200, json!({ "messages": ids })),
            ("RAW", "/batch/gmail/v1", 200, Value::String(resp)),
            // ONLY the hole may fall back to a single GET.
            ("GET", "/messages/g7", 200, meta7),
        ]);
        let client = GmailClient::new(http.clone(), home.path().to_path_buf());
        let res = client.inbox_messages("acct@example.com", n, "", 3.0).await.unwrap();
        let msgs = res["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), n, "batch + hole-fallback must deliver every message");
        assert_eq!(msgs[7]["message_id"], json!("<m7@x>"), "the hole came via fallback, in place");
        let calls = http.calls.lock().unwrap();
        let raw_calls = calls.iter().filter(|(m, _, _)| m == "RAW").count();
        let single_gets =
            calls.iter().filter(|(m, u, _)| m == "GET" && u.contains("/messages/g")).count();
        assert_eq!(raw_calls, 1, "one batch round-trip for 25 ids");
        assert_eq!(single_gets, 1, "only the hole single-fetches: {single_gets}");
    }

    #[tokio::test]
    async fn a_sustained_429_drops_one_message_never_the_whole_inbox() {
        let home = temp_home_with_token("acct@example.com", true);
        let meta = json!({
            "id": "g2", "threadId": "T", "snippet": "ok", "labelIds": ["INBOX"],
            "payload": { "headers": [ { "name": "Message-ID", "value": "<m2@x>" } ] },
        });
        let http = MockHttp::new(vec![
            ("GET", "/messages?q=", 200, json!({ "messages": [{ "id": "g1" }, { "id": "g2" }] })),
            // g1: four 429s — every attempt eaten, the message drops (loudly, WARN).
            ("GET", "/messages/g1", 429, json!({ "error": "rateLimitExceeded" })),
            ("GET", "/messages/g1", 429, json!({ "error": "rateLimitExceeded" })),
            ("GET", "/messages/g1", 429, json!({ "error": "rateLimitExceeded" })),
            ("GET", "/messages/g1", 429, json!({ "error": "rateLimitExceeded" })),
            ("GET", "/messages/g2", 200, meta),
        ]);
        let client = GmailClient::new(http.clone(), home.path().to_path_buf());
        let res = client.inbox_messages("acct@example.com", 2, "", 3.0).await.unwrap();
        let msgs = res["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1, "g2 must survive g1's sustained quota failure");
        assert_eq!(msgs[0]["message_id"], json!("<m2@x>"));
    }

    #[tokio::test]
    async fn latest_matching_filters_by_subject_and_fails_open() {
        let home = temp_home_with_token("acct@example.com", true);
        let meta = json!({
            "id": "g9", "threadId": "T7",
            "payload": { "headers": [
                { "name": "From", "value": "ceo@customer.com" },
                { "name": "To", "value": "acct@example.com" },
                { "name": "Subject", "value": "Pilot kickoff" },
                { "name": "Message-ID", "value": "<pilot@x>" },
                { "name": "Date", "value": "Sun, 09 Aug 2026 10:00:00 -0400" },
            ]},
        });
        let http = MockHttp::new(vec![
            ("GET", "/messages?q=", 200, json!({ "messages": [{ "id": "g9" }] })),
            ("GET", "/messages/g9", 200, meta),
        ]);
        let client = GmailClient::new(http.clone(), home.path().to_path_buf());
        let hit = client
            .latest_matching("acct@example.com", "", "ceo@customer.com", "pilot", 14)
            .await
            .unwrap();
        assert_eq!(hit["message_id"], json!("<pilot@x>"));
        assert_eq!(hit["thread_id"], json!("T7"));
        // ONE guard, values CLONED out before the next await: `&lock()[i]`
        // extends the temporary guard to end of scope — a second lock()
        // self-deadlocks and a held guard across an await blocks the runtime.
        let (url, list_url) = {
            let calls = http.calls.lock().unwrap();
            (calls[1].1.clone(), calls[0].1.clone())
        };
        assert!(list_url.contains(&urlencode("(from:ceo@customer.com OR to:ceo@customer.com) newer_than:14d")), "{list_url} {url}");
        // API error -> None (fail-open guard, never a gate).
        let http2 = MockHttp::new(vec![("GET", "/messages?q=", 500, json!({ "error": "boom" }))]);
        let client2 = GmailClient::new(http2, home.path().to_path_buf());
        assert!(client2.latest_matching("acct@example.com", "x@y.z", "", "", 14).await.is_none());
    }

    // ---- send-audit ledger ------------------------------------------------

    #[test]
    fn email_log_appends_and_reads_with_session_filter() {
        let dir = tempfile::tempdir().unwrap();
        email_log(
            dir.path(),
            json!({ "endpoint": "send", "via": "gmail", "from": "a@b.c", "to": "x@y.z",
                    "subject": "s", "session": "sess-1" }),
        );
        email_log(
            dir.path(),
            json!({ "endpoint": "reply", "via": "gmail", "from": "a@b.c",
                    "in_reply_to": "<m@x>", "session": null }),
        );
        let all = read_email_log(dir.path(), 7, 50, "");
        assert_eq!(all["count"], json!(2));
        // Newest first (Python: out[-limit:][::-1]).
        assert_eq!(all["log"][0]["endpoint"], json!("reply"));
        assert_eq!(all["log"][0]["session"], json!("unattributed"));
        let mine = read_email_log(dir.path(), 7, 50, "sess-1");
        assert_eq!(mine["count"], json!(1));
        assert_eq!(mine["log"][0]["session"], json!("sess-1"));
        let unattr = read_email_log(dir.path(), 7, 50, "unattributed");
        assert_eq!(unattr["count"], json!(1));
        assert_eq!(unattr["log"][0]["endpoint"], json!("reply"));
    }

    #[test]
    fn connected_accounts_lists_token_stems_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let tokens = dir.path().join("gmail-tokens");
        std::fs::create_dir_all(&tokens).unwrap();
        std::fs::write(tokens.join("b@x.com.json"), "{}").unwrap();
        std::fs::write(tokens.join("a@x.com.json"), "{}").unwrap();
        std::fs::write(tokens.join("notes.txt"), "").unwrap();
        assert_eq!(connected_accounts_in(dir.path()), vec!["a@x.com", "b@x.com"]);
        assert!(connected_accounts_in(&dir.path().join("missing")).is_empty());
    }
}
