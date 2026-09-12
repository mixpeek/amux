//! Replays of provider chrome through the actual status/preview projection.
//! Model names and effort labels are inputs, never classification rules.
use super::*;
use super::tests::signals;
use crate::backend::adapter::{TerminalAdapter, claude_auto_resume_banner};
use amux_core::provider::ProviderId;
use amux_core::protocol::WorkerEvent;

const AUTO: &str = "⏺ Usage limit reached · continuing automatically at 6:10pm · esc or type to cancel\n✻ Baked for 1m 57s · done 2:33 PM\n✔ Update installed · Restart to update\n❯\n⚠ Usage limit reached · continuing automatically at 6:10pm · esc to cancel\n⚠ /limit-reset to reset your session limit now · uses weekly limit · 1/week\n⚠ /usage-credits to continue now\n⏵⏵ bypass permissions on (shift+tab to cycle) · ← 5 agents\n/rc failed";
const ASK: &str = "Which output format?\n❯ 1. JSON\n  2. Plain text\nEnter to select · Esc to cancel";

#[test]
fn quota_never_becomes_a_human_question_after_preview() {
    for model in ["claude-sonnet-5", "claude-opus-5", "claude-haiku-4-5"] {
        for previous in ["active", "idle", "waiting", "blocked", "rate_limited", "api_error"] {
            for raw in [AUTO.to_string(), format!("Model: {model} low\n{AUTO}"), format!("\x1b[31m{AUTO}\x1b[0m"), AUTO.replace(" · esc to cancel", "\n · esc to cancel")] {
                let mut v = json!({"name":"chaos", "model":model, "running":true, "status":previous});
                apply_preview_waiting_status(&mut v, &raw);
                assert_eq!(v["status"], "rate_limited", "{model}/{previous}: {v}");
                assert_eq!(v["waiting_reason"], "rate_limit");
                assert!(v["rate_limited_until"].as_i64().unwrap() > 0);
                assert_eq!(v["credit_limited"], false);
                let events = TerminalAdapter::new(ProviderId::new("claude")).scan(&raw);
                assert!(matches!(events.as_slice(), [WorkerEvent::RateLimited(_)]), "{events:?}");
            }
        }
    }
}

#[test]
fn preview_cancellation_is_not_input_across_models() {
    let cases = [
        ("claude-sonnet-5", "✻ Thinking… (2s · 40 tokens)\n❯\n⏵⏵ bypass permissions on · esc to interrupt"),
        ("gpt-5 low", "• Working (1s • esc to interrupt)\n›\ngpt-5 low · ~/test"),
        ("gemini-2.5-flash-lite", "⠙ Thinking... (esc to cancel, 9s)\n│ Type your message"),
        ("gemini-2.5-flash", "✦ Thinking about the request (esc to cancel)"),
        ("claude-sonnet-5", "Continuing automatically at 6:10pm · esc to cancel\n❯\n⏵⏵ bypass permissions on"),
    ];
    for (model, raw) in cases {
        assert_eq!(derive_waiting_reason(raw), "", "{model}: {raw}");
        let mut v = json!({"running":true, "model":model, "status":"active"});
        apply_preview_waiting_status(&mut v, raw);
        assert_eq!(v["status"], "active", "{model}");
    }
}

#[test]
fn current_questions_survive_but_quoted_questions_do_not() {
    for (raw, reason) in [
        (ASK, "user_input"),
        ("Do you want to proceed?\n❯ 1. Yes\n  2. No\nEnter to select · Esc to cancel", "permission_prompt"),
        ("Do you trust the contents of this directory?\n› 1. Yes, continue\n  2. No\nPress enter to continue", "user_input"),
        ("╭ Allow execution?\n│ ● 1. Allow once\n│   2. No\n╰ Enter to select", "user_input"),
    ] {
        assert_eq!(derive_waiting_reason(raw), reason, "{raw}");
        let quoted = format!("⏺ Example output:\n{raw}\n⏺ Finished normally.\n❯\n⏵⏵ bypass permissions on");
        assert_eq!(derive_waiting_reason(&quoted), "", "{quoted}");
        assert_eq!(crate::api::session_verbs::detect_claude_status(&quoted), "idle", "{quoted}");
    }
    for status in ["rate_limited", "api_error", "error", "starting", "active"] {
        let mut v = json!({"status":status,"running":true});
        apply_preview_waiting_status(&mut v, ASK);
        assert_eq!(v["status"], status);
    }
    let mut stopped = json!({"status":"", "running":false});
    apply_preview_waiting_status(&mut stopped, AUTO);
    assert_eq!(stopped["status"], "");
}

#[test]
fn quota_footer_recovery_and_stale_transcript_chaos() {
    for suffix in ["\n⏺ Resumed\n❯\n⏵⏵ bypass permissions on", "\n❯\n⏵⏵ bypass permissions on · esc to interrupt"] {
        let recovered = format!("{AUTO}{suffix}");
        assert!(claude_auto_resume_banner(&recovered).is_none());
        assert_ne!(derive_waiting_reason(&recovered), "rate_limit");
    }
    assert!(claude_auto_resume_banner("⚠ Usage limit reached · continuing automatically at 6:10pm · esc to cancel").is_none(), "unowned prose cannot declare provider state");
    for raw in [AUTO.to_string(), format!("{}\n{AUTO}", "old output\n".repeat(1000))] {
        let mut s = signals();
        s.activity.insert("amux-chaos".into(), s.now as i64);
        s.panes.insert("chaos".into(), raw);
        for report in ["active", "idle", "waiting", "blocked"] {
            s.reports = json!({"chaos":{"state":report,"ts":s.now - 5.0}});
            let (status, ex) = s.derive_status_explain("chaos", true);
            assert_eq!(status, "rate_limited", "{report}: {ex}");
            assert_eq!(ex["decided_by"], "provider_auto_resume_quota");
            assert_eq!(s.derive_status("chaos", false), "");
            s.activity.insert("amux-chaos".into(), (s.now - 7200.0) as i64);
            assert_eq!(s.derive_status("chaos", true), "rate_limited", "a quiet quota footer is still current evidence");
        }
    }
}

#[test]
fn mixpeek_frustrations_live_recovery_capture_is_no_longer_limited() {
    let raw = include_str!("status_recovered_specimen.txt");
    assert!(claude_auto_resume_banner(raw).is_none());
    assert_eq!(derive_waiting_reason(raw), "");
    let mut v = json!({"name":"mixpeek-frustrations", "running":true, "status":"active"});
    apply_preview_waiting_status(&mut v, raw);
    assert_eq!(v["status"], "active");
}

#[test]
fn quoted_limit_menu_does_not_override_recovery_or_allow_an_automatic_keypress() {
    use crate::api::session_verbs::is_rate_limit_menu;
    let menu = "What do you want to do?\n❯ 1. Stop and wait for limit to reset\n2. Switch to usage credits\nEnter to confirm · Esc to cancel";
    assert!(is_rate_limit_menu(menu));
    let recovered = format!("{menu}\n⏺ Done\n❯\n⏵⏵ bypass permissions on");
    assert!(!is_rate_limit_menu(&recovered));
    assert_eq!(derive_waiting_reason(&recovered), "");
}

#[test]
fn auto_resume_observation_retains_the_clock_across_reset_and_restart() {
    use crate::api::session_verbs::observe_claude_limit;
    use chrono::TimeZone;
    let now = chrono::Local.with_ymd_and_hms(2026, 9, 11, 17, 0, 0).single().unwrap();
    let reset = chrono::Local.with_ymd_and_hms(2026, 9, 11, 18, 10, 0).single().unwrap().timestamp();
    let before = observe_claude_limit(AUTO, 0, now).unwrap();
    assert_eq!(before.kind, "auto-resume");
    assert!(!before.menu, "no key may be pressed into an automatic wait");
    assert_eq!(before.reset_at, reset);
    let after = observe_claude_limit(AUTO, reset, now + chrono::Duration::hours(2)).unwrap();
    assert_eq!(after.reset_at, reset, "a stale warning must not roll its reset forward a day");
    assert!(observe_claude_limit("❯\n⏵⏵ bypass permissions on", reset, now).is_none());
}

#[test]
fn low_effort_model_status_chaos_matrix() {
    let models = [
        ("claude-sonnet-5", "✻ Thinking… (2s · 10 tokens)\n❯\n⏵⏵ bypass permissions on · esc to interrupt", "❯\n⏵⏵ bypass permissions on"),
        ("claude-haiku-4-5", "· Thinking… (2s · 10 tokens)\n❯\n⏵⏵ bypass permissions on · esc to interrupt", "❯\n⏵⏵ bypass permissions on"),
        ("gpt-5 low", "• Working (1s • esc to interrupt)\n›\ngpt-5 low · ~/test", "›\ngpt-5 low · ~/test"),
        ("gpt-5.6-luna low", "• Waiting for background terminal (1s • esc to interrupt)\n›\ngpt-5.6-luna low · ~/test", "›\ngpt-5.6-luna low · ~/test"),
        ("gemini-2.5-flash-lite", "⠙ Thinking... (esc to cancel, 9s)", "│ Type your message"),
        ("gemini-2.5-flash", "⠋ Thinking... (esc to cancel, 9s)", "│ Type your message"),
    ];
    for (model, active, idle) in models {
        for (frame, expected) in [(active, "active"), (idle, "idle")] {
            for variant in 0..5 {
                let frame = match variant {
                    0 => frame.to_string(),
                    1 => format!("\x1b[32m{frame}\x1b[0m"),
                    2 => frame.replace('\n', "\r\n"),
                    3 => format!("{}\n{frame}", "old transcript\n".repeat(1000)),
                    _ => format!("\n\n{frame}\n\n"),
                };
                let mut sig = signals();
                sig.activity.insert("amux-matrix".into(), sig.now as i64);
                sig.reports = json!({"matrix":{"state":"idle","ts":sig.now - 1076.0}});
                sig.panes.insert("matrix".into(), frame.clone());
                // An idle provider has a fresh stop report; an active provider
                // contradicts an old one. Neither is an input request.
                if expected == "idle" { sig.reports["matrix"]["ts"] = json!(sig.now - 1.0); }
                let (status, ex) = sig.derive_status_explain("matrix", true);
                assert_eq!(status, expected, "{model}/variant={variant}: {ex}");
                assert_eq!(derive_waiting_reason(&frame), "", "{model}/variant={variant}");
                assert_eq!(sig.derive_status("matrix", false), "", "death outranks retained chrome");
            }
        }
    }
}

#[test]
fn all_core_states_project_by_tag_not_model_output_or_reason() {
    for state in ["active", "idle", "waiting", "rate_limited", "error", "starting", "stopped"] {
        for detail in ["active", "idle", "waiting", "rate_limited", "error"] {
            let raw = json!({"state":state,"detail":detail,"reason":detail}).to_string();
            assert_eq!(python_status(&raw), if state == "stopped" { "" } else { state }, "{raw}");
        }
    }
}
