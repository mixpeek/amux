pub const MAX_LOG_LINES: usize = 500;
pub const MAX_LOG_BYTES: usize = 256 * 1024;

pub fn bound_log(log: &str) -> String {
    log.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_thousand_lines_are_bounded_with_cumulative_marker() {
        let input = (0..10_000).map(|i| format!("entry {i}\n")).collect::<String>();
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
