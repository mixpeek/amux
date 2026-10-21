//! Artifact details live in the registry; the card keeps one summary per minute.
use super::board_store::append_log;

pub fn append_artifact_log(
    existing: Option<&str>,
    stamp: &str,
    actor: &str,
    kind: &str,
    state: &str,
    reference: &str,
) -> String {
    let current = existing.unwrap_or("");
    let today = chrono::Local::now().format("`%Y-%m-%d`").to_string();
    let prefix = format!("`{stamp}` artifact ({actor}): {kind}/{state} ");
    let parent = reference
        .rsplit_once('/')
        .map(|(p, _)| format!("{p}/"))
        .unwrap_or_else(|| ".".into());
    let mut lines: Vec<String> = current.lines().map(str::to_owned).collect();
    // A minute bucket is at most 60 seconds. Date boundary avoids merging
    // yesterday's identical clock time after a long idle period.
    let date_start = lines.iter().rposition(|line| line == &today);
    if let Some(start) = date_start {
        for line in lines[start..].iter_mut().rev() {
            let Some(summary) = line.strip_prefix(&prefix) else {
                continue;
            };
            let Some((count, old_parent)) = summary.split_once(" files under ") else {
                continue;
            };
            let Ok(count) = count.parse::<u64>() else {
                continue;
            };
            let common = common_directory(old_parent, &parent);
            *line = format!("{prefix}{} files under {common}", count.saturating_add(1));
            tracing::debug!(
                worker = actor,
                count = count.saturating_add(1),
                "board artifact log aggregated"
            );
            return super::card_log::bound_log(&lines.join("\n"));
        }
    }
    append_log(
        existing,
        stamp,
        &format!("artifact ({actor}): {kind}/{state} 1 files under {parent}"),
    )
}

fn common_directory(a: &str, b: &str) -> String {
    let end = a.bytes().zip(b.bytes()).take_while(|(a, b)| a == b).count();
    // Slash bytes cannot occur inside a multibyte UTF-8 character.
    let slash = a.as_bytes()[..end].iter().rposition(|b| *b == b'/');
    slash.map(|i| a[..=i].to_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_thousand_files_without_losing_registry_locations() {
        let mut log = String::new();
        for i in 0..1000 {
            log = append_artifact_log(
                Some(&log),
                "05:10",
                "Codex-W3",
                "implementation",
                "created",
                &format!(
                    "SchaffVault:/srv/storage/datasets/markets/finmtm/sub{}/file{i}.png",
                    i % 2
                ),
            );
        }
        assert!(log.lines().count() <= 5, "{log}");
        assert!(log.contains("1000 files under SchaffVault:/srv/storage/datasets/markets/finmtm/"));
        let next = append_artifact_log(
            Some(&log),
            "05:11",
            "Codex-W3",
            "implementation",
            "created",
            "/other/file",
        );
        assert_eq!(next.lines().count(), log.lines().count() + 1);
        let peer = append_artifact_log(
            Some(&log),
            "05:10",
            "Codex-W4",
            "implementation",
            "created",
            "/other/file",
        );
        assert_eq!(peer.lines().count(), log.lines().count() + 1);
    }
}
