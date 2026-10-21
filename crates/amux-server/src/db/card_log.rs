pub const MAX_LOG_LINES: usize = 500;
pub const MAX_LOG_BYTES: usize = 256 * 1024;

pub fn bound_log(log: &str) -> String {
    if log.len() <= MAX_LOG_BYTES && log.lines().count() <= MAX_LOG_LINES {
        return log.to_string();
    }
    let mut lines: Vec<&str> = log.lines().collect();
    let previous = lines
        .first()
        .and_then(|line| line.split_once(" older lines trimmed"))
        .and_then(|(count, _)| count.parse::<usize>().ok());
    if previous.is_some() {
        lines.remove(0);
    }
    // These two legacy log consumers control retry conflict detection and
    // capture callbacks. Keep their small metadata while dropping old prose.
    let plan = log
        .split_whitespace()
        .rev()
        .filter_map(|word| word.strip_prefix("plan_sha256="))
        .find(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()));
    let mut fold = None;
    let mut folded_lines = 0usize;
    lines.retain(|line| {
        let Some((target, inferred)) = super::board_store::folded_into_detail(Some(line)) else {
            return true;
        };
        folded_lines += 1;
        let canonical = format!(
            "capture folded into {target}{}",
            if inferred { " [inferred]" } else { "" }
        );
        // A real minted card id is small. An arbitrarily long, malformed
        // target must not make the protected header violate the hard byte cap.
        fold = (canonical.len() <= MAX_LOG_BYTES - 256).then_some(canonical);
        if fold.is_none() {
            tracing::warn!("board card oversized fold metadata dropped while trimming log");
        }
        false
    });
    let redundant_folds = folded_lines.saturating_sub(usize::from(fold.is_some()));
    let mut start = lines
        .len()
        .saturating_sub(MAX_LOG_LINES - 1 - usize::from(fold.is_some()));
    let mut bytes: usize = lines[start..].iter().map(|line| line.len() + 1).sum();
    loop {
        let count = previous
            .unwrap_or(0)
            .saturating_add(start)
            .saturating_add(redundant_folds);
        let mut marker = format!("{count} older lines trimmed");
        if let Some(plan) = plan {
            marker.push_str(&format!(" plan_sha256={plan}"));
        }
        let header = match fold.as_deref() {
            Some(fold) => format!("{marker}\n{fold}\n"),
            None => format!("{marker}\n"),
        };
        if header.len() + bytes.saturating_sub(1) <= MAX_LOG_BYTES {
            return format!("{header}{}", lines[start..].join("\n"));
        }
        if start + 1 < lines.len() {
            bytes -= lines[start].len() + 1;
            start += 1;
            continue;
        }
        let last = lines.last().copied().unwrap_or("");
        let mut offset = last
            .len()
            .saturating_sub(MAX_LOG_BYTES.saturating_sub(header.len()));
        while !last.is_char_boundary(offset) {
            offset += 1;
        }
        return format!("{header}{}", &last[offset..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_thousand_lines_are_bounded_with_cumulative_marker() {
        let input = (0..10_000)
            .map(|i| format!("entry {i}\n"))
            .collect::<String>();
        let bounded = bound_log(&input);
        assert!(bounded.lines().count() <= MAX_LOG_LINES);
        assert!(bounded.len() <= MAX_LOG_BYTES);
        assert!(bounded.contains("9501 older lines trimmed"));
        assert!(bounded.ends_with("entry 9999"));
        let again = bound_log(&format!("{bounded}\nnew entry"));
        assert!(again.contains("9502 older lines trimmed"));
        assert!(again.ends_with("new entry"));
        assert_eq!(bound_log(&again), again);
    }

    #[test]
    fn oversized_utf8_line_keeps_newest_bytes_safely() {
        let bounded = bound_log(&"\u{1f600}".repeat(100_000));
        assert!(bounded.len() <= MAX_LOG_BYTES);
        assert!(bounded.lines().count() <= MAX_LOG_LINES);
        assert!(bounded.contains("older lines trimmed"));
        assert!(bounded.ends_with('\u{1f600}'));
    }

    #[test]
    fn small_logs_are_unchanged() {
        assert_eq!(bound_log(""), "");
        assert_eq!(bound_log("one\ntwo"), "one\ntwo");
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn semantic_metadata_survives_repeated_trims() {
        let hash = "a".repeat(64);
        let mut log = format!("decomposed plan_sha256={hash}\n`05:10` worker: capture folded into CW-42 [inferred]\n{}", "old entry\n".repeat(1000));
        for _ in 0..1000 {
            log = bound_log(&format!("{log}\nnew entry"));
        }
        assert!(log.contains(&format!("plan_sha256={hash}")));
        assert!(log.contains("capture folded into CW-42 [inferred]"));
        assert!(log.lines().count() <= MAX_LOG_LINES);
        assert!(log.len() <= MAX_LOG_BYTES);
    }

    #[test]
    fn byte_limit_keeps_newest_of_multiple_utf8_lines() {
        let input = format!(
            "{}\n{}\nlatest",
            "x".repeat(200_000),
            "\u{1f600}".repeat(50_000)
        );
        let bounded = bound_log(&input);
        assert!(bounded.len() <= MAX_LOG_BYTES);
        assert!(bounded.ends_with("latest"));
        assert!(!bounded.contains(&"x".repeat(100)));
    }
}

#[cfg(test)]
mod fold_order_tests {
    use super::*;
    use crate::db::board_store::{append_log, folded_into_detail};

    #[test]
    fn latest_callback_target_survives_multiple_folds_and_repeated_trims() {
        let input = format!("{}\n`05:10` worker: capture folded into CW-11\n`05:11` worker: capture folded into CW-22 [inferred]\n{}", "old entry\n".repeat(200), "new entry\n".repeat(400));
        assert_eq!(
            folded_into_detail(Some(&input)),
            Some(("CW-22".into(), true))
        );
        let mut bounded = bound_log(&input);
        assert_eq!(
            folded_into_detail(Some(&bounded)),
            Some(("CW-22".into(), true))
        );
        assert!(!bounded.contains("capture folded into CW-11"));
        for _ in 0..600 {
            bounded = append_log(Some(&bounded), "05:12", "current work");
        }
        assert_eq!(
            folded_into_detail(Some(&bounded)),
            Some(("CW-22".into(), true))
        );
        assert!(bounded.lines().count() <= MAX_LOG_LINES);
        assert!(bounded.len() <= MAX_LOG_BYTES);
        assert!(bounded.contains("older lines trimmed"));
    }

    #[test]
    fn long_fold_lines_use_the_existing_parser_and_a_compact_target() {
        let input = format!(
            "{}\ncapture folded into CW-11 {}\ncapture folded into [inferred] CW-22\n{}",
            "old\n".repeat(200),
            "note".repeat(400),
            "new\n".repeat(400)
        );
        assert_eq!(
            folded_into_detail(Some(&input)),
            Some(("CW-22".into(), true))
        );
        let bounded = bound_log(&input);
        assert_eq!(
            folded_into_detail(Some(&bounded)),
            Some(("CW-22".into(), true))
        );
        assert!(!bounded.contains("capture folded into CW-11"));
        assert!(bounded.len() <= MAX_LOG_BYTES);
    }

    #[test]
    fn oversized_fold_target_cannot_expand_the_header_past_the_cap() {
        let input = format!(
            "capture folded into {}\nlatest",
            "x".repeat(MAX_LOG_BYTES * 2)
        );
        let bounded = bound_log(&input);
        assert!(bounded.len() <= MAX_LOG_BYTES);
        assert!(bounded.ends_with("latest"));
    }
}
