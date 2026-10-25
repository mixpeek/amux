//! The model and compaction window a Claude worker actually launched with.
//!
//! # Why this exists (gs12 review, 2026-10-18, finding 12)
//!
//! mxp-gs12 compacted at about 967k tokens through 10-17 08:33Z, then at 584k
//! and from 10-17 21:43Z at about 367k: 98 compactions over 10-17/18 against
//! 1 to 11 a day before. The model never changed. Its transcript shows
//! `claude-opus-5-5` on both sides and CC_FLAGS carried the same `--model`
//! across every restart. What changed was a Claude Code SETTING the restarted
//! process loaded: `~/.claude/settings.json` holds a global
//! `autoCompactWindow: 1000000` and also
//! `modelSettings.claude-opus-5-5.autoCompactWindow: 400000`, and the
//! per-model entry wins for that model. A running process keeps the settings
//! it started with, so the change only took effect at the next launch, which
//! is why it looked like the restart shrank the window.
//!
//! amux does not own that file and does not rewrite it (ethos rule 8). It
//! records what each launch resolved and says so when a launch resolves less
//! than the one before: `verdict=launch_effective_context` on every Claude
//! launch, `verdict=launch_context_shrank` (WARN) when the window fell by more
//! than [`SHRINK_RATIO`], naming the settings key that set it.

use serde_json::Value;
use std::path::Path;

/// A launch whose window is below this share of the previous launch's is
/// reported. 0.7 is the review's "fell by more than 30%".
pub(crate) const SHRINK_RATIO: f64 = 0.7;

/// One settings file Claude Code reads, lowest precedence first.
pub(crate) struct SettingsLayer {
    pub label: String,
    pub value: Value,
}

/// The settings files Claude Code layers for a worker in `work_dir`: user,
/// then project, then project-local. A missing or unparsable file is absent,
/// never an empty object that would read as "no window configured".
pub(crate) fn settings_layers(claude_home: &Path, work_dir: &str) -> Vec<SettingsLayer> {
    let mut out = Vec::new();
    let mut push = |label: String, path: std::path::PathBuf| {
        if let Some(v) = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            out.push(SettingsLayer { label, value: v });
        }
    };
    push("~/.claude/settings.json".into(), claude_home.join("settings.json"));
    if !work_dir.is_empty() {
        let wd = Path::new(work_dir);
        push(format!("{work_dir}/.claude/settings.json"), wd.join(".claude/settings.json"));
        push(format!("{work_dir}/.claude/settings.local.json"), wd.join(".claude/settings.local.json"));
    }
    out
}

/// The compaction window a launch of `model` resolves, and the key that set
/// it. A per-model `modelSettings.<model>.autoCompactWindow` beats the global
/// `autoCompactWindow`; within each, the highest-precedence layer wins. The
/// `[1m]` suffix is tried with and without, since the picker writes either.
///
/// `None` means no layer sets a window: Claude Code's own default applies,
/// which amux cannot read and does not guess.
pub(crate) fn effective_compact_window(layers: &[SettingsLayer], model: &str) -> Option<(u64, String)> {
    let base = model.trim().trim_end_matches("[1m]");
    let per_model = layers.iter().rev().find_map(|l| {
        [model.trim(), base].iter().find_map(|m| {
            l.value
                .pointer(&format!("/modelSettings/{}/autoCompactWindow", m.replace('~', "~0").replace('/', "~1")))
                .and_then(Value::as_u64)
                .map(|w| (w, format!("{} modelSettings.{m}.autoCompactWindow", l.label)))
        })
    });
    per_model.or_else(|| {
        layers.iter().rev().find_map(|l| {
            l.value
                .get("autoCompactWindow")
                .and_then(Value::as_u64)
                .map(|w| (w, format!("{} autoCompactWindow", l.label)))
        })
    })
}

/// Did this launch resolve a smaller window than the previous one? Only a
/// comparison of two MEASURED windows can say so; an unknown on either side
/// is not a shrink.
pub(crate) fn window_shrank(previous: Option<u64>, now: Option<u64>) -> bool {
    match (previous, now) {
        (Some(p), Some(n)) if p > 0 => (n as f64) < (p as f64) * SHRINK_RATIO,
        _ => false,
    }
}

/// Record and log one Claude launch. Returns the meta updates to persist.
pub(crate) fn record_launch(
    session: &str,
    model: &str,
    work_dir: &str,
    claude_home: &Path,
    prev_model: &str,
    prev_window: Option<u64>,
) -> Vec<(&'static str, Value)> {
    let layers = settings_layers(claude_home, work_dir);
    let resolved = effective_compact_window(&layers, model);
    let window = resolved.as_ref().map(|(w, _)| *w);
    let source = resolved.as_ref().map(|(_, s)| s.clone()).unwrap_or_default();
    let model_shown = if model.is_empty() { "(provider default)" } else { model };
    tracing::info!(session, model = model_shown, compact_window = ?window, source = %source,
        settings_layers_read = layers.len(), measured = true, n_considered = layers.len(),
        verdict = "launch_effective_context",
        "claude worker launched; compact window is what its settings resolve, None = Claude Code's default");
    if window_shrank(prev_window, window) {
        tracing::warn!(session, model = model_shown, previous_model = prev_model,
            compact_window = ?window, previous_compact_window = ?prev_window, source = %source,
            measured = true, n_considered = layers.len(), verdict = "launch_context_shrank",
            "this launch resolves a much smaller compaction window than the last one; the worker will compact far more often. Fix the settings key named in `source`");
    }
    if !prev_model.is_empty() && prev_model != model {
        tracing::warn!(session, model = model_shown, previous_model = prev_model,
            verdict = "launch_model_changed", measured = true, n_considered = 1,
            "this launch runs a different model from the previous one");
    }
    vec![
        ("launch_model", Value::from(model)),
        ("launch_compact_window", window.map(Value::from).unwrap_or(Value::Null)),
        ("launch_compact_window_source", Value::from(source)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn layer(label: &str, v: Value) -> SettingsLayer {
        SettingsLayer { label: label.into(), value: v }
    }

    /// The live user settings on 2026-10-18: a 1M global and a 400k entry for
    /// claude-opus-5-5. mxp-gs12 runs that model, so 400k is what it gets.
    #[test]
    fn a_per_model_window_beats_the_global_one() {
        let user = layer("~/.claude/settings.json", json!({
            "autoCompactWindow": 1000000,
            "modelSettings": {"claude-opus-5-5": {"autoCompactWindow": 400000}}}));
        let layers = vec![user];
        let (w, src) = effective_compact_window(&layers, "claude-opus-5-5").unwrap();
        assert_eq!(w, 400_000);
        assert!(src.contains("modelSettings.claude-opus-5-5"), "{src}");
        // Another model reads the global.
        assert_eq!(effective_compact_window(&layers, "claude-sonnet-5").unwrap().0, 1_000_000);
        // The [1m] spelling finds the base entry too.
        assert_eq!(effective_compact_window(&layers, "claude-opus-5-5[1m]").unwrap().0, 400_000);
    }

    #[test]
    fn a_project_layer_overrides_the_user_layer_and_none_is_unknown() {
        let layers = vec![
            layer("user", json!({"autoCompactWindow": 1000000})),
            layer("project", json!({"autoCompactWindow": 600000})),
        ];
        let (w, src) = effective_compact_window(&layers, "claude-opus-5-5").unwrap();
        assert_eq!((w, src.as_str()), (600_000, "project autoCompactWindow"));
        assert_eq!(effective_compact_window(&[layer("user", json!({}))], "x"), None);
        assert_eq!(effective_compact_window(&[], "x"), None);
    }

    #[test]
    fn only_a_measured_drop_past_the_ratio_is_a_shrink() {
        assert!(window_shrank(Some(1_000_000), Some(400_000)), "the gs12 case");
        assert!(!window_shrank(Some(1_000_000), Some(800_000)));
        assert!(!window_shrank(None, Some(400_000)), "first launch has nothing to compare");
        assert!(!window_shrank(Some(1_000_000), None), "an unknown is not a shrink");
        assert!(!window_shrank(Some(400_000), Some(1_000_000)));
    }

    /// The transcript scan reads each compaction's size and flags the gs12
    /// drop, incrementally, as the file grows.
    #[test]
    fn the_boundary_scan_reads_compaction_sizes_and_flags_a_drop() {
        use crate::api::session_verbs::{compact_pre_tokens, compact_window_shrank};
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("t.jsonl");
        let boundary = |pre: u64| format!(
            "{{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"compactMetadata\":{{\"trigger\":\"auto\",\"preTokens\":{pre}}}}}\n");
        std::fs::write(&p, format!("{}{{\"type\":\"user\"}}\n{}", boundary(968_103), boundary(967_115))).unwrap();
        let s = compact_pre_tokens(&p).unwrap();
        assert_eq!((s.count, s.pre_tokens_prev, s.pre_tokens_last), (2, Some(968_103), Some(967_115)));
        assert!(!compact_window_shrank(&s));
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(boundary(367_656).as_bytes()).unwrap();
        // A boundary without metadata still counts, and keeps the sizes.
        f.write_all(b"{\"type\":\"system\",\"subtype\":\"compact_boundary\"}\n").unwrap();
        drop(f);
        let s = compact_pre_tokens(&p).unwrap();
        assert_eq!((s.count, s.pre_tokens_prev, s.pre_tokens_last), (4, Some(967_115), Some(367_656)));
        assert!(compact_window_shrank(&s), "the 10-17 drop is flagged");
    }

    #[test]
    fn record_launch_reads_the_files_and_returns_the_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("claude");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("settings.json"),
            r#"{"autoCompactWindow":1000000,"modelSettings":{"claude-opus-5-5":{"autoCompactWindow":400000}}}"#).unwrap();
        let wd = tmp.path().join("repo");
        std::fs::create_dir_all(wd.join(".claude")).unwrap();
        std::fs::write(wd.join(".claude/settings.local.json"), "not json").unwrap();
        let up = record_launch("w", "claude-opus-5-5", wd.to_str().unwrap(), &home, "claude-opus-5-5", Some(1_000_000));
        assert_eq!(up[0], ("launch_model", json!("claude-opus-5-5")));
        assert_eq!(up[1], ("launch_compact_window", json!(400000)));
        assert!(up[2].1.as_str().unwrap().contains("~/.claude/settings.json modelSettings"), "{:?}", up[2]);
    }
}
