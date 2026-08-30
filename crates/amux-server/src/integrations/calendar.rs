//! iCal (RFC 5545) generation + the feed-publisher seam (RR-0089).
//!
//! Ported from the Python server's `_generate_ical` and its helpers
//! (`_ical_escape`, `_ical_fold`, `_ical_date`, `_ical_date_plus_days`,
//! `_ical_utc`). The behavioral contract carried over verbatim:
//! - timed events are emitted as UTC (`DTSTART:...Z`) so Google/Apple show
//!   the correct local time instead of misreading a floating value as UTC;
//! - all-day events use `VALUE=DATE` with an EXCLUSIVE DTEND (default one
//!   day after start);
//! - content lines fold at 75 octets with a single-space continuation, on
//!   character boundaries so multi-byte UTF-8 is never split;
//! - lines are CRLF-joined with a trailing CRLF.
//!
//! Timezone deviation, named on purpose: Python interprets stored local
//! datetimes in `AMUX_GCAL_TZ` (default America/New_York) via zoneinfo.
//! This crate has no tz database dep, so production uses the SYSTEM local
//! timezone via `chrono::Local` (identical on the deployment machine, whose
//! system tz is America/New_York) and falls back to a floating value when
//! conversion is ambiguous — the same fallback Python uses when zoneinfo is
//! unavailable. The converter is injected so tests pin output against a
//! fixed offset instead of the build machine's tz.

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde_json::Value;

/// A `cal_events` row as JSON (Python column shapes: unix-second ints,
/// nullable text columns). Kept as `Value` so appended live-DB columns pass
/// through untouched, exactly like Python's `dict(zip(cols, row))`.
pub type CalEventRow = Value;

/// Converts a stored local wall-clock datetime to UTC. `None` means "cannot
/// convert" and callers fall back to a floating (no `Z`) value.
pub type TzConvert<'a> = &'a dyn Fn(NaiveDateTime) -> Option<DateTime<Utc>>;

/// Production converter: the system local timezone (see module docs for the
/// deviation from Python's AMUX_GCAL_TZ).
pub fn local_to_utc(naive: NaiveDateTime) -> Option<DateTime<Utc>> {
    match chrono::Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => Some(dt.with_timezone(&Utc)),
        chrono::LocalResult::Ambiguous(dt, _) => Some(dt.with_timezone(&Utc)),
        chrono::LocalResult::None => None,
    }
}

/// Escape a TEXT value per RFC 5545 §3.3.11 — backslash first, then `;` `,`
/// and newlines (Python `_ical_escape`, order preserved).
pub fn ical_escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace("\r\n", "\\n")
        .replace('\n', "\\n")
        .replace('\r', "")
        .replace(',', "\\,")
        .replace(';', "\\;")
}

/// Fold a content line to <=75 octets per RFC 5545 §3.1 — continuation lines
/// start with a single space (so they carry 74 octets of payload). Folds on
/// character boundaries so multi-byte UTF-8 isn't split (Python
/// `_ical_fold`, byte-for-byte).
pub fn ical_fold(line: &str) -> String {
    let mut segments: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    let mut first = true;
    for ch in line.chars() {
        let clen = ch.len_utf8();
        let limit = if first { 75 } else { 74 };
        if cur_len + clen > limit {
            segments.push(std::mem::take(&mut cur));
            cur.push(ch);
            cur_len = clen;
            first = false;
        } else {
            cur.push(ch);
            cur_len += clen;
        }
    }
    segments.push(cur);
    segments.join("\r\n ")
}

/// Stored date/datetime -> all-day DATE value (YYYYMMDD). Python `_ical_date`.
pub fn ical_date(val: &str) -> Option<String> {
    let s = val.trim();
    let b = s.as_bytes();
    if b.len() < 10 {
        return None;
    }
    let ok = b[0..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit);
    if !ok {
        return None;
    }
    Some(format!("{}{}{}", &s[0..4], &s[5..7], &s[8..10]))
}

/// YYYYMMDD + N days (for an exclusive all-day DTEND). Python
/// `_ical_date_plus_days`.
pub fn ical_date_plus_days(datestr: &str, days: i64) -> Option<String> {
    let y: i32 = datestr.get(0..4)?.parse().ok()?;
    let m: u32 = datestr.get(4..6)?.parse().ok()?;
    let d: u32 = datestr.get(6..8)?.parse().ok()?;
    let date = NaiveDate::from_ymd_opt(y, m, d)? + chrono::Duration::days(days);
    Some(date.format("%Y%m%d").to_string())
}

/// Parse Python's `^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2})(?::(\d{2}))?`
/// prefix match into a NaiveDateTime plus the zero-padded string pieces the
/// floating fallback needs.
fn parse_local(val: &str) -> Option<(NaiveDateTime, String)> {
    let s = val.trim();
    let b = s.as_bytes();
    if b.len() < 16 {
        return None;
    }
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    if !(digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && (b[10] == b'T' || b[10] == b' ')
        && digits(11..13)
        && b[13] == b':'
        && digits(14..16))
    {
        return None;
    }
    let ss = if b.len() >= 19 && b[16] == b':' && digits(17..19) { &s[17..19] } else { "00" };
    let naive = NaiveDate::from_ymd_opt(
        s[0..4].parse().ok()?,
        s[5..7].parse().ok()?,
        s[8..10].parse().ok()?,
    )?
    .and_hms_opt(s[11..13].parse().ok()?, s[14..16].parse().ok()?, ss.parse().ok()?)?;
    let floating = format!("{}{}{}T{}{}{}", &s[0..4], &s[5..7], &s[8..10], &s[11..13], &s[14..16], ss);
    Some((naive, floating))
}

/// Stored local datetime -> UTC DATE-TIME (`YYYYMMDDTHHMMSSZ`), falling back
/// to a floating value (no `Z`) when the converter cannot answer — the same
/// fallback Python takes when zoneinfo is unavailable (`_ical_utc`).
pub fn ical_utc(val: &str, tz: TzConvert) -> Option<String> {
    let (naive, floating) = parse_local(val)?;
    match tz(naive) {
        Some(utc) => Some(utc.format("%Y%m%dT%H%M%SZ").to_string()),
        None => Some(floating),
    }
}

fn row_str<'a>(ev: &'a Value, key: &str) -> Option<&'a str> {
    ev.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// RFC 5545 iCalendar feed from calendar EVENTS only (Python
/// `_generate_ical`). `dtstamp` is injected for deterministic fixtures;
/// production passes now-UTC.
pub fn generate_ical(events: &[CalEventRow], dtstamp: &str, tz: TzConvert) -> String {
    let mut lines: Vec<String> = vec![
        "BEGIN:VCALENDAR".into(),
        "VERSION:2.0".into(),
        "PRODID:-//amux//amux calendar//EN".into(),
        "CALSCALE:GREGORIAN".into(),
        "METHOD:PUBLISH".into(),
        "X-WR-CALNAME:amux".into(),
        "X-WR-CALDESC:amux calendar events".into(),
        "REFRESH-INTERVAL;VALUE=DURATION:PT15M".into(),
        "X-PUBLISHED-TTL:PT15M".into(),
    ];
    for ev in events {
        let all_day = ev
            .get("all_day")
            .map(|v| v.as_i64().unwrap_or(0) != 0 || v.as_bool().unwrap_or(false))
            .unwrap_or(false);
        let uid = format!("{}@amux", ev.get("id").and_then(Value::as_str).unwrap_or("evt"));
        let summary = ical_escape(row_str(ev, "title").unwrap_or("Event"));
        let mut block: Vec<String> =
            vec!["BEGIN:VEVENT".into(), format!("UID:{uid}"), format!("DTSTAMP:{dtstamp}")];
        if all_day {
            let Some(d0) = row_str(ev, "start").and_then(ical_date) else { continue };
            // All-day DTEND is exclusive; default to the day after start.
            let d1 = row_str(ev, "end")
                .and_then(ical_date)
                .or_else(|| ical_date_plus_days(&d0, 1))
                .unwrap_or_else(|| d0.clone());
            block.push(format!("DTSTART;VALUE=DATE:{d0}"));
            block.push(format!("DTEND;VALUE=DATE:{d1}"));
        } else {
            let Some(dtstart) = row_str(ev, "start").and_then(|s| ical_utc(s, tz)) else {
                continue;
            };
            block.push(format!("DTSTART:{dtstart}"));
            match row_str(ev, "end").and_then(|s| ical_utc(s, tz)) {
                Some(dtend) => block.push(format!("DTEND:{dtend}")),
                None => block.push("DURATION:PT1H".into()),
            }
        }
        block.push(format!("SUMMARY:{summary}"));
        if let Some(loc) = row_str(ev, "location") {
            block.push(format!("LOCATION:{}", ical_escape(loc)));
        }
        if let Some(desc) = row_str(ev, "description") {
            block.push(format!("DESCRIPTION:{}", ical_escape(desc)));
        }
        if let Some(rrule) = row_str(ev, "rrule") {
            block.push(format!("RRULE:{rrule}"));
        }
        block.push("STATUS:CONFIRMED".into());
        block.push("SEQUENCE:0".into());
        block.push("END:VEVENT".into());
        lines.append(&mut block);
    }
    lines.push("END:VCALENDAR".into());
    lines.iter().map(|l| ical_fold(l)).collect::<Vec<_>>().join("\r\n") + "\r\n"
}

/// Production entry: now-UTC DTSTAMP, system-local tz conversion.
pub fn generate_ical_now(events: &[CalEventRow]) -> String {
    let dtstamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    generate_ical(events, &dtstamp, &local_to_utc)
}

// ---------------------------------------------------------------------------
// Feed publisher seam (S3)
// ---------------------------------------------------------------------------

/// Where the generated feed gets published (Python `_upload_ical_to_s3`).
///
/// The aws-sdk-s3 dependency is deliberately NOT in the workspace yet;
/// implementations land behind this trait when that decision is taken.
/// Meanwhile the default is an HONEST no-op: it refuses (so callers surface
/// `calendar_s3: unavailable` in the IntegrationRegistry) rather than
/// pretending the upload happened.
// TODO(RR-0089): S3 publisher lands with the aws-sdk-s3 dep decision.
pub trait IcalPublisher: Send + Sync {
    /// True when this publisher can actually deliver the feed somewhere.
    /// Callers skip generation entirely when false — no wasted work, no
    /// fake success.
    fn is_configured(&self) -> bool;
    fn publish(&self, ical: &str) -> Result<(), String>;
}

/// The default until aws-sdk-s3 lands: cannot publish, says so.
pub struct NoopPublisher;

impl IcalPublisher for NoopPublisher {
    fn is_configured(&self) -> bool {
        false
    }
    fn publish(&self, _ical: &str) -> Result<(), String> {
        Err("S3 publisher not built (aws-sdk-s3 dep decision pending, RR-0089)".into())
    }
}

/// The publisher that ships (AMUX/SA-124, 2026-08-22): `aws s3 cp` via the
/// CLI already on this machine, stdin-fed, no new workspace dependency —
/// the same call shape Python's boto3 `put_object` made, headers included.
///
/// This exists because the NoopPublisher's honesty was not enough: the
/// registry said `calendar_s3: unavailable` from the day of the Rust
/// cutover, nobody read it, and Ethan's REAL Google Calendar silently
/// served an Aug-21 snapshot while 14 events (a 12-installment settlement
/// schedule among them, first payment days out) accumulated only in the
/// live feed. A degraded state a human must poll IS the rule-4 second
/// layer: a tag in a store the reader never opens.
pub struct CliS3Publisher {
    pub bucket: String,
    pub key: String,
    pub region: String,
}

impl CliS3Publisher {
    /// Some only when the deployment configured a bucket+key AND the aws CLI
    /// exists — is_configured stays truthful about DELIVERABILITY, not intent.
    pub fn from_env() -> Option<Self> {
        let get = |k: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty());
        let bucket = get("AMUX_S3_BUCKET")?;
        let key = get("AMUX_S3_KEY")?;
        let region = get("AMUX_S3_REGION").unwrap_or_else(|| "us-east-1".into());
        Some(Self { bucket, key, region })
    }

    fn aws_binary() -> Option<std::path::PathBuf> {
        ["/usr/local/bin/aws", "/opt/homebrew/bin/aws", "/usr/bin/aws"]
            .iter()
            .map(std::path::PathBuf::from)
            .find(|p| p.exists())
    }
}

impl IcalPublisher for CliS3Publisher {
    fn is_configured(&self) -> bool {
        !self.bucket.is_empty() && !self.key.is_empty() && Self::aws_binary().is_some()
    }
    fn publish(&self, ical: &str) -> Result<(), String> {
        use std::io::Write;
        let aws = Self::aws_binary().ok_or("aws CLI not found")?;
        let mut child = std::process::Command::new(aws)
            .args([
                "s3", "cp", "-",
                &format!("s3://{}/{}", self.bucket, self.key),
                "--region", &self.region,
                // Same headers Python set: content-type for Google, ETag-able
                // caching so conditional refetches stay cheap.
                "--content-type", "text/calendar; charset=utf-8",
                "--cache-control", "public, max-age=900, must-revalidate",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("aws spawn failed: {e}"))?;
        child
            .stdin
            .take()
            .ok_or("aws stdin unavailable")?
            .write_all(ical.as_bytes())
            .map_err(|e| format!("writing feed to aws failed: {e}"))?;
        let out = child.wait_with_output().map_err(|e| format!("aws wait failed: {e}"))?;
        if out.status.success() {
            tracing::info!(bucket = %self.bucket, key = %self.key, "iCal published to S3");
            Ok(())
        } else {
            Err(format!(
                "aws s3 cp exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).chars().take(300).collect::<String>()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use serde_json::json;

    /// Deterministic converter for fixtures: interpret local wall time as
    /// UTC-5 (America/New_York standard time), like the Python default tz.
    fn minus5(naive: NaiveDateTime) -> Option<DateTime<Utc>> {
        let off = FixedOffset::west_opt(5 * 3600).unwrap();
        off.from_local_datetime(&naive).single().map(|dt| dt.with_timezone(&Utc))
    }

    #[test]
    fn escape_backslash_first_then_separators_and_newlines() {
        assert_eq!(ical_escape(r"a\b"), r"a\\b");
        assert_eq!(ical_escape("a,b;c"), r"a\,b\;c");
        assert_eq!(ical_escape("l1\r\nl2\nl3\r"), r"l1\nl2\nl3");
        // Escaping a backslash must not double-escape the inserted ones.
        assert_eq!(ical_escape("\\n"), "\\\\n");
    }

    #[test]
    fn fold_at_75_octets_with_space_continuations() {
        let line = format!("DESCRIPTION:{}", "x".repeat(200));
        let folded = ical_fold(&line);
        for (i, seg) in folded.split("\r\n").enumerate() {
            if i == 0 {
                assert!(seg.len() <= 75, "first segment {} octets", seg.len());
            } else {
                assert!(seg.starts_with(' '), "continuation must start with a space");
                assert!(seg.len() <= 75, "continuation {} octets incl. space", seg.len());
            }
        }
        // Reassembles losslessly.
        assert_eq!(folded.replace("\r\n ", ""), line);
    }

    #[test]
    fn fold_never_splits_multibyte_utf8() {
        // 3-byte chars land a fold mid-character if folding is byte-based.
        let line = format!("SUMMARY:{}", "€".repeat(60));
        let folded = ical_fold(&line);
        for seg in folded.split("\r\n") {
            assert!(seg.len() <= 75);
            // Would panic on a broken char boundary:
            let _ = seg.chars().count();
        }
        assert_eq!(folded.replace("\r\n ", ""), line);
    }

    #[test]
    fn short_lines_are_untouched() {
        assert_eq!(ical_fold("BEGIN:VCALENDAR"), "BEGIN:VCALENDAR");
    }

    #[test]
    fn date_helpers() {
        assert_eq!(ical_date("2026-08-09"), Some("20260809".into()));
        assert_eq!(ical_date("2026-08-09T10:00:00"), Some("20260809".into()));
        assert_eq!(ical_date("garbage"), None);
        assert_eq!(ical_date_plus_days("20261231", 1), Some("20270101".into()));
        assert_eq!(ical_date_plus_days("20260228", 1), Some("20260301".into()));
    }

    #[test]
    fn utc_conversion_and_floating_fallback() {
        // 10:30 local at UTC-5 => 15:30Z. Both 'T' and ' ' separators parse.
        assert_eq!(ical_utc("2026-08-09T10:30:00", &minus5), Some("20260809T153000Z".into()));
        assert_eq!(ical_utc("2026-08-09 10:30", &minus5), Some("20260809T153000Z".into()));
        // Converter cannot answer -> floating value, seconds zero-padded.
        assert_eq!(ical_utc("2026-08-09T10:30", &|_| None), Some("20260809T103000".into()));
        assert_eq!(ical_utc("2026-08-09", &minus5), None); // date-only is not a timed value
    }

    #[test]
    fn full_feed_pinned_fixture() {
        let events = vec![
            json!({
                "id": "EVT-1", "title": "Call; with, IRS", "start": "2026-08-09T10:00:00",
                "end": "2026-08-09T10:30:00", "all_day": 0,
                "location": "Phone", "description": "line1\nline2",
                "rrule": null, "created": 1, "updated": 1, "deleted": null
            }),
            json!({
                "id": "EVT-2", "title": "Offsite", "start": "2026-09-01",
                "end": null, "all_day": 1, "location": null, "description": null,
                "rrule": "FREQ=WEEKLY;BYDAY=MO", "created": 1, "updated": 1, "deleted": null
            }),
        ];
        let got = generate_ical(&events, "20260809T120000Z", &minus5);
        let want = concat!(
            "BEGIN:VCALENDAR\r\n",
            "VERSION:2.0\r\n",
            "PRODID:-//amux//amux calendar//EN\r\n",
            "CALSCALE:GREGORIAN\r\n",
            "METHOD:PUBLISH\r\n",
            "X-WR-CALNAME:amux\r\n",
            "X-WR-CALDESC:amux calendar events\r\n",
            "REFRESH-INTERVAL;VALUE=DURATION:PT15M\r\n",
            "X-PUBLISHED-TTL:PT15M\r\n",
            "BEGIN:VEVENT\r\n",
            "UID:EVT-1@amux\r\n",
            "DTSTAMP:20260809T120000Z\r\n",
            "DTSTART:20260809T150000Z\r\n",
            "DTEND:20260809T153000Z\r\n",
            "SUMMARY:Call\\; with\\, IRS\r\n",
            "LOCATION:Phone\r\n",
            "DESCRIPTION:line1\\nline2\r\n",
            "STATUS:CONFIRMED\r\n",
            "SEQUENCE:0\r\n",
            "END:VEVENT\r\n",
            "BEGIN:VEVENT\r\n",
            "UID:EVT-2@amux\r\n",
            "DTSTAMP:20260809T120000Z\r\n",
            "DTSTART;VALUE=DATE:20260901\r\n",
            "DTEND;VALUE=DATE:20260902\r\n",
            "SUMMARY:Offsite\r\n",
            "RRULE:FREQ=WEEKLY;BYDAY=MO\r\n",
            "STATUS:CONFIRMED\r\n",
            "SEQUENCE:0\r\n",
            "END:VEVENT\r\n",
            "END:VCALENDAR\r\n",
        );
        assert_eq!(got, want);
    }

    #[test]
    fn timed_event_without_end_gets_one_hour_duration() {
        let events =
            vec![json!({ "id": "EVT-3", "title": "t", "start": "2026-08-09T10:00", "all_day": 0 })];
        let got = generate_ical(&events, "20260809T120000Z", &minus5);
        assert!(got.contains("DURATION:PT1H\r\n"), "{got}");
        assert!(!got.contains("DTEND:"), "{got}");
    }

    #[test]
    fn long_description_folds_inside_feed() {
        let events = vec![json!({
            "id": "EVT-4", "title": "t", "start": "2026-08-09T10:00", "all_day": 0,
            "description": "d".repeat(150),
        })];
        let got = generate_ical(&events, "20260809T120000Z", &minus5);
        for line in got.split("\r\n") {
            assert!(line.len() <= 75, "unfolded line: {} octets", line.len());
        }
        assert!(got.contains("DESCRIPTION:"));
    }

    #[test]
    fn unparseable_start_skips_event_not_feed() {
        let events = vec![
            json!({ "id": "EVT-5", "title": "broken", "start": "not-a-date", "all_day": 0 }),
            json!({ "id": "EVT-6", "title": "fine", "start": "2026-08-09T10:00", "all_day": 0 }),
        ];
        let got = generate_ical(&events, "20260809T120000Z", &minus5);
        assert!(!got.contains("EVT-5@amux"));
        assert!(got.contains("EVT-6@amux"));
    }

    #[test]
    fn noop_publisher_is_honest() {
        let p = NoopPublisher;
        assert!(!p.is_configured());
        assert!(p.publish("BEGIN:VCALENDAR").is_err());
    }
}
