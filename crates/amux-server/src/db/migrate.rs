//! Migration runner (RR-0019).
//!
//! Migrations are numbered SQL files embedded at compile time from
//! `crates/amux-server/migrations/`. Applied in order inside one exclusive
//! transaction each, tracked in `_amux_migrations`. Two special forms:
//!
//! - Plain SQL statements: executed as a batch.
//! - `-- ADDCOL: <table> <column> <decl...>` directive lines: applied as
//!   `ALTER TABLE ... ADD COLUMN` ONLY when the column is absent. SQLite has
//!   no `ADD COLUMN IF NOT EXISTS`, and the baseline schema mirrors a live
//!   Python database whose tables may or may not already carry the column —
//!   the directive makes the migration idempotent against both shapes.
//!
//! The Python server must be able to keep opening the same DB file after
//! these run (Phase 11 rollback requirement), so migrations are ADDITIVE
//! ONLY: no drops, no renames, no type changes.

use rusqlite::{Connection, OptionalExtension};

struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

/// Recorded names that are safe predecessors of the registered migration.
///
/// A version/name mismatch normally means a migration was skipped and remains
/// a startup error. This one is different and was already proven/documented in
/// this file: version 35's regenerable-samples migration was renamed from a
/// stale `0029_` filename prefix to `0035_` without changing its schema work.
/// Keeping that fact executable prevents every healthy boot from raising a
/// false migration-collision alarm while preserving the alarm for every
/// unrecognized mismatch. Versions 82/83 were assigned independently on the
/// project-lifecycle branch. Migration 85 is an idempotent union of both
/// histories, so either deployed history converges before the server uses the
/// schema.
const MIGRATION_NAME_ALIASES: &[(i64, &str, &str)] = &[
    (35, "0029_regenerable_samples", "0035_regenerable_samples"),
    (82, "0082_project_execution", "0082_nudge_budget"),
    (83, "0083_project_attempts", "0083_project_execution"),
];

fn known_name_alias(version: i64, recorded: &str, registered: &str) -> bool {
    MIGRATION_NAME_ALIASES
        .iter()
        .any(|(v, old, new)| *v == version && *old == recorded && *new == registered)
}

// Embedded at compile time so the binary is self-contained (single-artifact
// deploy is one of the four reasons this rewrite exists).
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "0001_baseline",
        sql: include_str!("../../migrations/0001_baseline.sql"),
    },
    Migration {
        version: 2,
        name: "0002_rust_additions",
        sql: include_str!("../../migrations/0002_rust_additions.sql"),
    },
    Migration {
        version: 3,
        name: "0003_workers",
        sql: include_str!("../../migrations/0003_workers.sql"),
    },
    Migration {
        version: 4,
        name: "0004_commands",
        sql: include_str!("../../migrations/0004_commands.sql"),
    },
    Migration {
        version: 5,
        name: "0005_memories_snapshots",
        sql: include_str!("../../migrations/0005_memories_snapshots.sql"),
    },
    Migration {
        version: 6,
        name: "0006_turns_messages",
        sql: include_str!("../../migrations/0006_turns_messages.sql"),
    },
    Migration {
        version: 7,
        name: "0007_criteria",
        sql: include_str!("../../migrations/0007_criteria.sql"),
    },
    Migration {
        version: 8,
        name: "0008_event_payload",
        sql: include_str!("../../migrations/0008_event_payload.sql"),
    },
    Migration {
        version: 9,
        name: "0009_media_jobs",
        sql: include_str!("../../migrations/0009_media_jobs.sql"),
    },
    Migration {
        version: 10,
        name: "0010_request_log",
        sql: include_str!("../../migrations/0010_request_log.sql"),
    },
    Migration {
        version: 11,
        name: "0011_conversations",
        sql: include_str!("../../migrations/0011_conversations.sql"),
    },
    Migration {
        version: 12,
        name: "0012_invariants",
        sql: include_str!("../../migrations/0012_invariants.sql"),
    },
    Migration {
        version: 13,
        name: "0013_search",
        sql: include_str!("../../migrations/0013_search.sql"),
    },
    Migration {
        version: 14,
        name: "0014_message_delivery_meta",
        sql: include_str!("../../migrations/0014_message_delivery_meta.sql"),
    },
    Migration {
        version: 15,
        name: "0015_schedule_run_delivery",
        sql: include_str!("../../migrations/0015_schedule_run_delivery.sql"),
    },
    Migration {
        version: 16,
        name: "0016_submit_verdict",
        sql: include_str!("../../migrations/0016_submit_verdict.sql"),
    },
    Migration {
        version: 17,
        name: "0017_status_gate_custom",
        sql: include_str!("../../migrations/0017_status_gate_custom.sql"),
    },
    Migration {
        version: 18,
        name: "0018_issue_epic",
        sql: include_str!("../../migrations/0018_issue_epic.sql"),
    },
    Migration {
        version: 19,
        name: "0019_reclaim",
        sql: include_str!("../../migrations/0019_reclaim.sql"),
    },
    Migration {
        version: 20,
        name: "0020_mdai_runs",
        sql: include_str!("../../migrations/0020_mdai_runs.sql"),
    },
    Migration {
        version: 21,
        name: "0021_heartbeat",
        sql: include_str!("../../migrations/0021_heartbeat.sql"),
    },
    Migration {
        version: 22,
        name: "0022_downtime_requests_during",
        sql: include_str!("../../migrations/0022_downtime_requests_during.sql"),
    },
    Migration {
        version: 23,
        name: "0023_downtime_backfill",
        sql: include_str!("../../migrations/0023_downtime_backfill.sql"),
    },
    Migration {
        version: 24,
        name: "0024_steering_dead_letter",
        sql: include_str!("../../migrations/0024_steering_dead_letter.sql"),
    },
    Migration {
        version: 25,
        name: "0025_mdai_run_duration",
        sql: include_str!("../../migrations/0025_mdai_run_duration.sql"),
    },
    Migration {
        version: 26,
        name: "0026_reclaim_skipped",
        sql: include_str!("../../migrations/0026_reclaim_skipped.sql"),
    },
    Migration {
        version: 27,
        name: "0027_guard_verdicts",
        sql: include_str!("../../migrations/0027_guard_verdicts.sql"),
    },
    Migration {
        version: 28,
        name: "0028_downtime_cause",
        sql: include_str!("../../migrations/0028_downtime_cause.sql"),
    },
    Migration {
        version: 29,
        name: "0029_steering_history_source",
        sql: include_str!("../../migrations/0029_steering_history_source.sql"),
    },
    Migration {
        version: 30,
        name: "0030_request_log_boot_at",
        sql: include_str!("../../migrations/0030_request_log_boot_at.sql"),
    },
    Migration {
        version: 31,
        name: "0031_issues_closed_at",
        sql: include_str!("../../migrations/0031_issues_closed_at.sql"),
    },
    Migration {
        version: 32,
        name: "0032_state_events_entity_index",
        sql: include_str!("../../migrations/0032_state_events_entity_index.sql"),
    },
    Migration {
        version: 33,
        name: "0033_steering_precondition",
        sql: include_str!("../../migrations/0033_steering_precondition.sql"),
    },
    Migration {
        version: 34,
        name: "0034_request_log_load1",
        sql: include_str!("../../migrations/0034_request_log_load1.sql"),
    },
    Migration {
        version: 35,
        name: "0035_regenerable_samples",
        sql: include_str!("../../migrations/0035_regenerable_samples.sql"),
    },
    Migration {
        version: 36,
        name: "0036_issues_evidence",
        sql: include_str!("../../migrations/0036_issues_evidence.sql"),
    },
    Migration {
        version: 37,
        name: "0037_issues_typed_ask",
        sql: include_str!("../../migrations/0037_issues_typed_ask.sql"),
    },
    Migration {
        version: 38,
        name: "0038_nudge_feedback",
        sql: include_str!("../../migrations/0038_nudge_feedback.sql"),
    },
    Migration {
        version: 39,
        name: "0039_issues_continuation",
        sql: include_str!("../../migrations/0039_issues_continuation.sql"),
    },
    Migration {
        version: 40,
        name: "0040_issues_entered_state_at",
        sql: include_str!("../../migrations/0040_issues_entered_state_at.sql"),
    },
    Migration {
        version: 41,
        name: "0041_issues_blocked_on",
        sql: include_str!("../../migrations/0041_issues_blocked_on.sql"),
    },
    // Renumbered from 35 (collision with regenerable_samples) per merge of PR #174
    Migration {
        version: 42,
        name: "0042_reclaim_skipped_hits_repair",
        sql: include_str!("../../migrations/0042_reclaim_skipped_hits_repair.sql"),
    },
    Migration {
        version: 43,
        name: "0043_issues_source",
        sql: include_str!("../../migrations/0043_issues_source.sql"),
    },
    Migration {
        version: 44,
        name: "0044_verifications",
        sql: include_str!("../../migrations/0044_verifications.sql"),
    },
    Migration {
        version: 45,
        name: "0045_issues_workflow_fields",
        sql: include_str!("../../migrations/0045_issues_workflow_fields.sql"),
    },
    Migration {
        version: 46,
        name: "0046_task_artifacts",
        sql: include_str!("../../migrations/0046_task_artifacts.sql"),
    },
    Migration {
        version: 47,
        name: "0047_stage_contracts",
        sql: include_str!("../../migrations/0047_stage_contracts.sql"),
    },
    Migration {
        version: 48,
        name: "0048_issues_waiting_on",
        sql: include_str!("../../migrations/0048_issues_waiting_on.sql"),
    },
    // Renumbered from 35-38, then AGAIN from 49-52 (second collision, same
    // contributor-collision case this pattern has hit repeatedly on this
    // repo — main's own 0049_email_annotations landed independently while
    // this branch also claimed 49; see versions_are_dense_and_match_their_
    // filenames). This branch's telegram migrations are the losing side
    // both times, renumbered to the next free slots after main's 49.
    Migration {
        version: 49,
        name: "0049_email_annotations",
        sql: include_str!("../../migrations/0049_email_annotations.sql"),
    },
    Migration {
        version: 50,
        name: "0050_telegram",
        sql: include_str!("../../migrations/0050_telegram.sql"),
    },
    Migration {
        version: 51,
        name: "0051_telegram_relay",
        sql: include_str!("../../migrations/0051_telegram_relay.sql"),
    },
    Migration {
        version: 52,
        name: "0052_telegram_routed_session",
        sql: include_str!("../../migrations/0052_telegram_routed_session.sql"),
    },
    Migration {
        version: 53,
        name: "0053_telegram_relay_dedup",
        sql: include_str!("../../migrations/0053_telegram_relay_dedup.sql"),
    },
    Migration {
        version: 54,
        name: "0054_telegram_chat_type",
        sql: include_str!("../../migrations/0054_telegram_chat_type.sql"),
    },
    Migration {
        version: 55,
        name: "0055_issue_callbacks",
        sql: include_str!("../../migrations/0055_issue_callbacks.sql"),
    },
    Migration {
        version: 56,
        name: "0056_search_prompts",
        sql: include_str!("../../migrations/0056_search_prompts.sql"),
    },
    Migration {
        version: 57,
        name: "0057_board_overlap_reconciliation",
        sql: include_str!("../../migrations/0057_board_overlap_reconciliation.sql"),
    },
    Migration {
        version: 58,
        name: "0058_harness_enforcement",
        sql: include_str!("../../migrations/0058_harness_enforcement.sql"),
    },
    // Renumbered 0057 -> 0058 -> 0059 across two successive merges of
    // origin/main into feat/local-member-scopes. Both branches independently
    // claim the next free slot each time, which is correct on each branch
    // alone and a duplicate together; main's side had already shipped and may
    // be recorded in live `_amux_migrations` rows, so THIS side moved. It will
    // keep moving on every refresh until this branch lands.
    Migration {
        version: 59,
        name: "0059_local_member_scope",
        sql: include_str!("../../migrations/0059_local_member_scope.sql"),
    },
    Migration {
        version: 60,
        name: "0060_org_teams",
        sql: include_str!("../../migrations/0060_org_teams.sql"),
    },
    Migration {
        version: 61,
        name: "0061_board_cdc",
        sql: include_str!("../../migrations/0061_board_cdc.sql"),
    },
    Migration {
        version: 62,
        name: "0062_self_driving_control_plane",
        sql: include_str!("../../migrations/0062_self_driving_control_plane.sql"),
    },
    Migration {
        version: 63,
        name: "0063_schedule_delivery_outcome",
        sql: include_str!("../../migrations/0063_schedule_delivery_outcome.sql"),
    },
    Migration {
        version: 64,
        name: "0064_steering_delivery_claim",
        sql: include_str!("../../migrations/0064_steering_delivery_claim.sql"),
    },
    Migration {
        version: 65,
        name: "0065_interaction_receipts",
        sql: include_str!("../../migrations/0065_interaction_receipts.sql"),
    },
    Migration {
        version: 66,
        name: "0066_send_receipt_identity",
        sql: include_str!("../../migrations/0066_send_receipt_identity.sql"),
    },
    Migration {
        version: 67,
        name: "0067_message_capture_pending",
        sql: include_str!("../../migrations/0067_message_capture_pending.sql"),
    },
    Migration {
        version: 68,
        name: "0068_task_lease",
        sql: include_str!("../../migrations/0068_task_lease.sql"),
    },
    Migration {
        version: 69,
        name: "0069_worker_lifecycle",
        sql: include_str!("../../migrations/0069_worker_lifecycle.sql"),
    },
    Migration {
        version: 70,
        name: "0070_token_ledger_message_id",
        sql: include_str!("../../migrations/0070_token_ledger_message_id.sql"),
    },
    Migration {
        version: 71,
        name: "0071_issues_epic_index",
        sql: include_str!("../../migrations/0071_issues_epic_index.sql"),
    },
    Migration {
        version: 72,
        name: "0072_host_metrics",
        sql: include_str!("../../migrations/0072_host_metrics.sql"),
    },
    Migration {
        version: 73,
        name: "0073_command_intake",
        sql: include_str!("../../migrations/0073_command_intake.sql"),
    },
    Migration {
        version: 74,
        name: "0074_cmd_history_client_meta",
        sql: include_str!("../../migrations/0074_cmd_history_client_meta.sql"),
    },
    Migration {
        version: 75,
        name: "0075_issues_pre_archive_status",
        sql: include_str!("../../migrations/0075_issues_pre_archive_status.sql"),
    },
    Migration {
        version: 76,
        name: "0076_cmd_history_session_ts_index",
        sql: include_str!("../../migrations/0076_cmd_history_session_ts_index.sql"),
    },
    Migration {
        version: 77,
        name: "0077_search_docs_prompt_seconds",
        sql: include_str!("../../migrations/0077_search_docs_prompt_seconds.sql"),
    },
    Migration {
        version: 78,
        name: "0078_invariant_result_build",
        sql: include_str!("../../migrations/0078_invariant_result_build.sql"),
    },
    Migration {
        version: 79,
        name: "0079_schedules_worktree",
        sql: include_str!("../../migrations/0079_schedules_worktree.sql"),
    },
    Migration {
        version: 80,
        name: "0080_invariant_result_status_ts",
        sql: include_str!("../../migrations/0080_invariant_result_status_ts.sql"),
    },
    Migration {
        version: 81,
        name: "0081_invariant_incident_episode",
        sql: include_str!("../../migrations/0081_invariant_incident_episode.sql"),
    },
    Migration {
        version: 82,
        name: "0082_nudge_budget",
        sql: include_str!("../../migrations/0082_nudge_budget.sql"),
    },
    Migration {
        version: 83,
        name: "0083_project_execution",
        sql: include_str!("../../migrations/0083_project_execution.sql"),
    },
    Migration {
        version: 84,
        name: "0084_project_attempts",
        sql: include_str!("../../migrations/0084_project_attempts.sql"),
    },
    Migration {
        version: 85,
        name: "0085_project_migration_collision_reconcile",
        sql: include_str!("../../migrations/0085_project_migration_collision_reconcile.sql"),
    },
    Migration {
        version: 86,
        name: "0086_steering_message_receipts",
        sql: include_str!("../../migrations/0086_steering_message_receipts.sql"),
    },
    Migration {
        version: 87,
        name: "0087_worker_type",
        sql: include_str!("../../migrations/0087_worker_type.sql"),
    },
    Migration {
        version: 88,
        name: "0088_window_stats_indexes",
        sql: include_str!("../../migrations/0088_window_stats_indexes.sql"),
    },
    Migration {
        version: 89,
        name: "0089_board_signals",
        sql: include_str!("../../migrations/0089_board_signals.sql"),
    },
    Migration {
        version: 90,
        name: "0090_schedule_run_shell_output",
        sql: include_str!("../../migrations/0090_schedule_run_shell_output.sql"),
    },
    Migration {
        version: 91,
        name: "0091_cmd_history_repeat_of",
        sql: include_str!("../../migrations/0091_cmd_history_repeat_of.sql"),
    },
    Migration {
        version: 92,
        name: "0092_pinned_notes",
        sql: include_str!("../../migrations/0092_pinned_notes.sql"),
    },
    Migration {
        version: 93,
        name: "0093_standing_approvals",
        sql: include_str!("../../migrations/0093_standing_approvals.sql"),
    },
    Migration {
        version: 94,
        name: "0094_trace_archive",
        sql: include_str!("../../migrations/0094_trace_archive.sql"),
    },
    Migration {
        version: 95,
        name: "0095_request_log_client",
        sql: include_str!("../../migrations/0095_request_log_client.sql"),
    },
    Migration {
        version: 96,
        name: "0096_skill_usage",
        sql: include_str!("../../migrations/0096_skill_usage.sql"),
    },
    Migration {
        version: 97,
        name: "0097_steering_delay_after_idle",
        sql: include_str!("../../migrations/0097_steering_delay_after_idle.sql"),
    },
    Migration {
        version: 98,
        name: "0098_chatgpt_app_oauth",
        sql: include_str!("../../migrations/0098_chatgpt_app_oauth.sql"),
    },
    Migration {
        version: 99,
        name: "0099_location_history",
        sql: include_str!("../../migrations/0099_location_history.sql"),
    },
    Migration {
        version: 100,
        name: "0100_location_raw",
        sql: include_str!("../../migrations/0100_location_raw.sql"),
    },
    Migration {
        version: 101,
        name: "0101_location_place_names",
        sql: include_str!("../../migrations/0101_location_place_names.sql"),
    },
    Migration {
        version: 102,
        name: "0102_schedule_run_queue_id",
        sql: include_str!("../../migrations/0102_schedule_run_queue_id.sql"),
    },
    Migration {
        version: 103,
        name: "0103_card_contracts",
        sql: include_str!("../../migrations/0103_card_contracts.sql"),
    },
    Migration {
        version: 104,
        name: "0104_card_contract_amended",
        sql: include_str!("../../migrations/0104_card_contract_amended.sql"),
    },
    Migration {
        version: 105,
        name: "0105_card_contract_deploy",
        sql: include_str!("../../migrations/0105_card_contract_deploy.sql"),
    },
    Migration {
        version: 106,
        name: "0106_card_contract_review",
        sql: include_str!("../../migrations/0106_card_contract_review.sql"),
    },
    Migration {
        version: 107,
        name: "0107_card_contract_left_undone",
        sql: include_str!("../../migrations/0107_card_contract_left_undone.sql"),
    },
    Migration {
        version: 108,
        name: "0108_done_lines",
        sql: include_str!("../../migrations/0108_done_lines.sql"),
    },
    Migration {
        version: 109,
        name: "0109_prod_change_notices",
        sql: include_str!("../../migrations/0109_prod_change_notices.sql"),
    },
    Migration {
        version: 110,
        name: "0110_land_queue",
        sql: include_str!("../../migrations/0110_land_queue.sql"),
    },
    Migration {
        version: 111,
        name: "0111_session_events_type_index",
        sql: include_str!("../../migrations/0111_session_events_type_index.sql"),
    },
    Migration {
        version: 112,
        name: "0112_card_contract_prereview",
        sql: include_str!("../../migrations/0112_card_contract_prereview.sql"),
    },
    Migration {
        version: 113,
        name: "0113_card_prereviews",
        sql: include_str!("../../migrations/0113_card_prereviews.sql"),
    },
    Migration {
        version: 114,
        name: "0114_review_capacity",
        sql: include_str!("../../migrations/0114_review_capacity.sql"),
    },

    Migration {
        version: 115,
        name: "0115_steering_retention_index",
        sql: include_str!("../../migrations/0115_steering_retention_index.sql"),
    },
    Migration {
        version: 116,
        name: "0116_issues_updated_integer",
        sql: include_str!("../../migrations/0116_issues_updated_integer.sql"),
    },
    Migration {
        version: 117,
        name: "0117_skills_created",
        sql: include_str!("../../migrations/0117_skills_created.sql"),
    },
    Migration {
        version: 118,
        name: "0118_conversations",
        sql: include_str!("../../migrations/0118_conversations.sql"),
    },
    Migration {
        version: 119,
        name: "0119_history_fts",
        sql: include_str!("../../migrations/0119_history_fts.sql"),
    },
    Migration {
        version: 120,
        name: "0120_schedule_failure_notice",
        sql: include_str!("../../migrations/0120_schedule_failure_notice.sql"),
    },
    Migration {
        version: 121,
        name: "0121_mdai_run_usage",
        sql: include_str!("../../migrations/0121_mdai_run_usage.sql"),
    },
    Migration {
        version: 122,
        name: "0122_steering_deliver_at",
        sql: include_str!("../../migrations/0122_steering_deliver_at.sql"),
    },
    Migration {
        version: 123,
        name: "0123_file_opens",
        sql: include_str!("../../migrations/0123_file_opens.sql"),
    },
    Migration {
        version: 124,
        name: "0124_pinned_notes_groups",
        sql: include_str!("../../migrations/0124_pinned_notes_groups.sql"),
    },
    Migration {
        version: 125,
        name: "0125_schedule_lifecycle",
        sql: include_str!("../../migrations/0125_schedule_lifecycle.sql"),
    },
    Migration {
        version: 126,
        name: "0126_land_queue_guard_overrides",
        sql: include_str!("../../migrations/0126_land_queue_guard_overrides.sql"),
    },
    Migration {
        version: 127,
        name: "0127_board_cdc_all_rendered_columns",
        sql: include_str!("../../migrations/0127_board_cdc_all_rendered_columns.sql"),
    },
    Migration {
        version: 128,
        name: "0128_map_store",
        sql: include_str!("../../migrations/0128_map_store.sql"),
    },
    Migration {
        version: 129,
        name: "0129_claude_account_usage",
        sql: include_str!("../../migrations/0129_claude_account_usage.sql"),
    },
    Migration {
        version: 130,
        name: "0130_signal_subscriptions",
        sql: include_str!("../../migrations/0130_signal_subscriptions.sql"),
    },
    Migration {
        version: 131,
        name: "0131_worker_resource_samples",
        sql: include_str!("../../migrations/0131_worker_resource_samples.sql"),
    },
    Migration {
        version: 132,
        name: "0132_token_ledger_task_session_ts_index",
        sql: include_str!("../../migrations/0132_token_ledger_task_session_ts_index.sql"),
    },
    Migration {
        version: 133,
        name: "0133_layout_presets_scope",
        sql: include_str!("../../migrations/0133_layout_presets_scope.sql"),
    },
    Migration {
        version: 134,
        name: "0134_social_posts",
        sql: include_str!("../../migrations/0134_social_posts.sql"),
    },
    Migration {
        version: 135,
        name: "0135_schedule_pass_check",
        sql: include_str!("../../migrations/0135_schedule_pass_check.sql"),
    },
    Migration {
        version: 136,
        name: "0136_bridge",
        sql: include_str!("../../migrations/0136_bridge.sql"),
    },
    Migration {
        version: 137,
        name: "0137_messages_threads",
        sql: include_str!("../../migrations/0137_messages_threads.sql"),
    },
    Migration {
        version: 138,
        name: "0138_social_queue",
        sql: include_str!("../../migrations/0138_social_queue.sql"),
    },
    Migration {
        version: 139,
        name: "0139_remove_projects",
        sql: include_str!("../../migrations/0139_remove_projects.sql"),
    },
    Migration {
        version: 140,
        name: "0140_conversation_channels",
        sql: include_str!("../../migrations/0140_conversation_channels.sql"),
    },
    Migration {
        version: 141,
        name: "0141_schedule_when",
        sql: include_str!("../../migrations/0141_schedule_when.sql"),
    },
    Migration {
        version: 142,
        name: "0142_issues_source_ref_index",
        sql: include_str!("../../migrations/0142_issues_source_ref_index.sql"),
    },
    Migration {
        version: 143,
        name: "0143_schedule_runs_running_index",
        sql: include_str!("../../migrations/0143_schedule_runs_running_index.sql"),
    },
    Migration {
        version: 144,
        name: "0144_conversation_pins",
        sql: include_str!("../../migrations/0144_conversation_pins.sql"),
    },
    Migration {
        version: 145,
        name: "0145_lease_generations",
        sql: include_str!("../../migrations/0145_lease_generations.sql"),
    },
    Migration {
        version: 146,
        name: "0146_one_live_session",
        sql: include_str!("../../migrations/0146_one_live_session.sql"),
    },
    Migration {
        version: 147,
        name: "0147_invariant_episode_occurrences",
        sql: include_str!("../../migrations/0147_invariant_episode_occurrences.sql"),
    },
    Migration {
        version: 148,
        name: "0148_board_light_rows_covering",
        sql: include_str!("../../migrations/0148_board_light_rows_covering.sql"),
    },
    Migration {
        version: 149,
        name: "0149_message_pins",
        sql: include_str!("../../migrations/0149_message_pins.sql"),
    },
    Migration {
        version: 150,
        name: "0150_land_queue_composed_evidence",
        sql: include_str!("../../migrations/0150_land_queue_composed_evidence.sql"),
    },
    Migration {
        version: 151,
        name: "0151_board_planning_covering",
        sql: include_str!("../../migrations/0151_board_planning_covering.sql"),
    },
    Migration {
        version: 152,
        name: "0152_decision_digest",
        sql: include_str!("../../migrations/0152_decision_digest.sql"),
    },
    Migration {
        version: 153,
        name: "0153_land_red",
        sql: include_str!("../../migrations/0153_land_red.sql"),
    },
    Migration {
        version: 154,
        name: "0154_issues_session_status",
        sql: include_str!("../../migrations/0154_issues_session_status.sql"),
    },
    Migration {
        version: 155,
        name: "0155_goal_samples",
        sql: include_str!("../../migrations/0155_goal_samples.sql"),
    },
    Migration {
        version: 156,
        name: "0156_directives",
        sql: include_str!("../../migrations/0156_directives.sql"),
    },
    Migration {
        version: 157,
        name: "0157_event_subscriptions",
        sql: include_str!("../../migrations/0157_event_subscriptions.sql"),
    },
    Migration {
        version: 158,
        name: "0158_calls",
        sql: include_str!("../../migrations/0158_calls.sql"),
    },
    Migration {
        version: 159,
        name: "0159_intent",
        sql: include_str!("../../migrations/0159_intent.sql"),
    },
    Migration {
        version: 160,
        name: "0160_intent_on_primitives",
        sql: include_str!("../../migrations/0160_intent_on_primitives.sql"),
    },
    Migration {
        version: 161,
        name: "0161_ledger",
        sql: include_str!("../../migrations/0161_ledger.sql"),
    },
    Migration {
        version: 162,
        name: "0162_trace",
        sql: include_str!("../../migrations/0162_trace.sql"),
    },
    Migration {
        version: 163,
        name: "0163_cli_telemetry",
        sql: include_str!("../../migrations/0163_cli_telemetry.sql"),
    },
    Migration {
        version: 164,
        name: "0164_ledger_checkpoints",
        sql: include_str!("../../migrations/0164_ledger_checkpoints.sql"),
    },
    Migration {
        version: 165,
        name: "0165_approval_cards",
        sql: include_str!("../../migrations/0165_approval_cards.sql"),
    },
    Migration {
        version: 166,
        name: "0166_schedule_authority",
        sql: include_str!("../../migrations/0166_schedule_authority.sql"),
    },
    Migration {
        version: 167,
        name: "0167_audit_governance",
        sql: include_str!("../../migrations/0167_audit_governance.sql"),
    },
    Migration {
        version: 168,
        name: "0168_sso_mfa_scim",
        sql: include_str!("../../migrations/0168_sso_mfa_scim.sql"),
    },
    Migration {
        version: 169,
        name: "0169_crm_accounts_deals",
        sql: include_str!("../../migrations/0169_crm_accounts_deals.sql"),
    },
    Migration {
        version: 170,
        name: "0170_crm_channels_touches",
        sql: include_str!("../../migrations/0170_crm_channels_touches.sql"),
    },
    Migration {
        version: 171,
        name: "0171_crm_sequences",
        sql: include_str!("../../migrations/0171_crm_sequences.sql"),
    },
    Migration {
        version: 172,
        name: "0172_crm_voices",
        sql: include_str!("../../migrations/0172_crm_voices.sql"),
    },
    Migration {
        version: 173,
        name: "0173_schedule_critical",
        sql: include_str!("../../migrations/0173_schedule_critical.sql"),
    },
    Migration {
        version: 174,
        name: "0174_crm_account_activity_deal_tags",
        sql: include_str!("../../migrations/0174_crm_account_activity_deal_tags.sql"),
    },
    Migration {
        version: 175,
        name: "0175_outbound_requests",
        sql: include_str!("../../migrations/0175_outbound_requests.sql"),
    },
    Migration {
        version: 176,
        name: "0176_point_of_use_tables",
        sql: include_str!("../../migrations/0176_point_of_use_tables.sql"),
    },
    Migration {
        version: DROP_AMUX_MESSAGES,
        name: "0177_drop_amux_messages",
        sql: include_str!("../../migrations/0177_drop_amux_messages.sql"),
    },
    Migration {
        version: 178,
        name: "0178_telegram_links_in_bridge",
        sql: include_str!("../../migrations/0178_telegram_links_in_bridge.sql"),
    },
    Migration {
        version: DROP_INTERACTION_LOG,
        name: "0179_drop_interaction_log",
        sql: include_str!("../../migrations/0179_drop_interaction_log.sql"),
    },
    Migration {
        version: DROP_DORMANT_PLANNING,
        name: "0180_drop_dormant_planning_tables",
        sql: include_str!("../../migrations/0180_drop_dormant_planning_tables.sql"),
    },
    Migration {
        version: 181,
        name: "0181_schedule_managed_by",
        sql: include_str!("../../migrations/0181_schedule_managed_by.sql"),
    },
    Migration {
        version: 182,
        name: "0182_crm_closing",
        sql: include_str!("../../migrations/0182_crm_closing.sql"),
    },
];

/// The migration that drops `interaction_log`. Before it runs,
/// [`super::audit_archive::interaction_log`] copies the rows it removes.
const DROP_INTERACTION_LOG: i64 = 179;

/// The migration that drops `_amux_messages`. Before it runs,
/// [`archive_before_drop`] copies the rows it removes.
const DROP_AMUX_MESSAGES: i64 = 177;

/// The migration that drops the dormant typed model's planning and budget
/// tables and the legacy `tasks` table (audit step 2). Archived the same way.
const DROP_DORMANT_PLANNING: i64 = 180;

/// The tables [`DROP_DORMANT_PLANNING`] drops, each archived as `<table>.json`.
const DORMANT_PLANNING_TABLES: &[&str] = &[
    "_amux_goal_contracts",
    "_amux_goal_contract_revisions",
    "_amux_planning_nodes",
    "_amux_current_plans",
    "_amux_plan_revisions",
    "_amux_task_budgets",
    "tasks",
];

/// One file of rows a dropping migration keeps: written only when `table`
/// exists and the query returns rows.
struct ArchiveSet {
    file: String,
    table: &'static str,
    sql: String,
}

/// What a migration must archive before it runs; empty for most.
fn archive_sets(version: i64) -> Vec<ArchiveSet> {
    match version {
        DROP_AMUX_MESSAGES => vec![
            ArchiveSet {
                file: "_amux_messages.json".into(),
                table: "_amux_messages",
                sql: "SELECT * FROM _amux_messages ORDER BY created_at, id".into(),
            },
            ArchiveSet {
                file: "_amux_commands.deliver_message.json".into(),
                table: "_amux_commands",
                sql: "SELECT * FROM _amux_commands WHERE json_extract(command, '$.kind') = 'deliver_message' \
                      AND json_type(command, '$.data') = 'text' ORDER BY queued_at, id"
                    .into(),
            },
        ],
        DROP_DORMANT_PLANNING => DORMANT_PLANNING_TABLES
            .iter()
            .map(|t| ArchiveSet { file: format!("{t}.json"), table: t, sql: format!("SELECT * FROM {t} ORDER BY rowid") })
            .collect(),
        _ => Vec::new(),
    }
}

/// Where the primitive integrity audit's dropped rows are kept, under the amux
/// home (the database's directory).
pub(crate) const AUDIT_ARCHIVE_DIR: &str = "archive/primitive-audit-2026-11-26";

/// Rows a query returns, as JSON objects keyed by column name.
pub(crate) fn rows_json(conn: &Connection, sql: &str) -> anyhow::Result<Vec<serde_json::Value>> {
    use rusqlite::types::ValueRef;
    let mut stmt = conn.prepare(sql)?;
    let cols: Vec<String> = stmt.column_names().map(|c| c.to_string()).collect();
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let mut o = serde_json::Map::new();
        for (i, c) in cols.iter().enumerate() {
            let v = match r.get_ref(i)? {
                ValueRef::Null => serde_json::Value::Null,
                ValueRef::Integer(n) => serde_json::json!(n),
                ValueRef::Real(f) => serde_json::json!(f),
                ValueRef::Text(t) => serde_json::Value::String(String::from_utf8_lossy(t).into_owned()),
                ValueRef::Blob(b) => serde_json::Value::String(String::from_utf8_lossy(b).into_owned()),
            };
            o.insert(c.clone(), v);
        }
        out.push(serde_json::Value::Object(o));
    }
    Ok(out)
}

/// Before a dropping migration runs (0177 `_amux_messages`, 0180 the dormant
/// planning tables), write the rows [`archive_sets`] names to
/// `<home>/archive/primitive-audit-2026-11-26/` as JSON. A file already there
/// is kept as it is (a re-run never overwrites the first copy). With rows to
/// keep and no home to keep them in, the migration is refused rather than
/// run: the drop must never be the only copy's end.
fn archive_before_drop(conn: &Connection, home: Option<&std::path::Path>, migration: &str, sets: Vec<ArchiveSet>) -> anyhow::Result<()> {
    for set in sets {
        let table_exists = conn
            .query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1", [set.table], |_| Ok(()))
            .optional()?
            .is_some();
        let rows = if table_exists { rows_json(conn, &set.sql)? } else { Vec::new() };
        let file = set.file.as_str();
        if rows.is_empty() {
            tracing::info!(migration, file, table_exists, rows = 0, measured = true, n_considered = 0,
                verdict = "audit_archive_nothing_to_keep", "no rows to archive for this file");
            continue;
        }
        let Some(home) = home else {
            anyhow::bail!("{migration} would drop {} row(s) archived as {file} but this database has no home to archive them in", rows.len());
        };
        let dir = home.join(AUDIT_ARCHIVE_DIR);
        let path = dir.join(file);
        if path.exists() {
            tracing::info!(migration, path = %path.display(), rows = rows.len(), measured = true, n_considered = rows.len(),
                verdict = "audit_archive_kept_existing", "an archive is already there; left as it is");
            continue;
        }
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!(".{file}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec_pretty(&rows)?)?;
        std::fs::rename(&tmp, &path)?;
        tracing::warn!(migration, path = %path.display(), rows = rows.len(), measured = true, n_considered = rows.len(),
            verdict = "audit_archive_written", "rows archived before their table is dropped");
    }
    Ok(())
}

/// Migrations embedded in THIS binary that the DB has not recorded yet.
///
/// Deliberately mirrors `apply_all`'s own "already applied?" test, including
/// the `unwrap_or(false)` — if `_amux_migrations` does not exist yet the query
/// fails and everything is pending, which is the truth for a fresh DB. A
/// separate predicate here would be a second spelling that drifts.
///
/// A version recorded under a DIFFERENT name that has not been reconciled is
/// pending too: `apply_all` will run that migration's SQL (see
/// [`MismatchAction`]), so the live-DB guard must count it.
pub fn pending(conn: &Connection) -> Vec<&'static str> {
    MIGRATIONS
        .iter()
        .filter(|m| match recorded_row(conn, m.version) {
            Ok(Some(row)) => mismatch_action(m, &row) == MismatchAction::Apply,
            Ok(None) | Err(_) => true,
        })
        .map(|m| m.name)
        .collect()
}

/// What `_amux_migrations` holds for one version.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedRow {
    name: String,
    /// The registered name this version was reconciled to after a name
    /// mismatch, if any. The recorded `name` is kept as evidence.
    reconciled_name: Option<String>,
    checksum: Option<String>,
}

/// Read one version's bookkeeping row. Tolerates a table that predates the
/// `reconciled_name`/`checksum` columns (`pending` runs before `apply_all`
/// adds them), reading those as NULL.
fn recorded_row(conn: &Connection, version: i64) -> rusqlite::Result<Option<RecordedRow>> {
    let full = conn
        .query_row(
            "SELECT name, reconciled_name, checksum FROM _amux_migrations WHERE version = ?1",
            [version],
            |r| {
                Ok(RecordedRow {
                    name: r.get(0)?,
                    reconciled_name: r.get(1)?,
                    checksum: r.get(2)?,
                })
            },
        )
        .optional();
    match full {
        Ok(row) => Ok(row),
        Err(e) if e.to_string().contains("no such column") => conn
            .query_row(
                "SELECT name FROM _amux_migrations WHERE version = ?1",
                [version],
                |r| {
                    Ok(RecordedRow {
                        name: r.get(0)?,
                        reconciled_name: None,
                        checksum: None,
                    })
                },
            )
            .optional(),
        Err(e) => Err(e),
    }
}

/// What the runner does with a version that already has a bookkeeping row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MismatchAction {
    /// The row is this migration (same name, a documented alias, or already
    /// reconciled to it). Nothing to run.
    AlreadyApplied,
    /// The row names a DIFFERENT migration: this one's SQL never ran here.
    Apply,
}

fn mismatch_action(m: &Migration, row: &RecordedRow) -> MismatchAction {
    if row.name == m.name
        || known_name_alias(m.version, &row.name, m.name)
        || row.reconciled_name.as_deref() == Some(m.name)
    {
        MismatchAction::AlreadyApplied
    } else {
        MismatchAction::Apply
    }
}

/// sha256 of a migration's embedded SQL, hex. Recorded on apply so an edit to
/// an already-applied file is visible (`migration_checksum_changed`).
fn checksum(sql: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(sql.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Add the bookkeeping columns this runner records. `ADD COLUMN` of a nullable
/// column with no default is a schema-only change in SQLite (no table rewrite),
/// and `_amux_migrations` is a couple of hundred rows regardless.
fn ensure_bookkeeping_columns(conn: &Connection) -> anyhow::Result<()> {
    for (column, decl) in [
        ("checksum", "TEXT"),
        ("reconciled_name", "TEXT"),
        ("reconciled_at", "TEXT"),
    ] {
        if !column_exists(conn, "_amux_migrations", column)? {
            conn.execute_batch(&format!(
                "ALTER TABLE _amux_migrations ADD COLUMN {column} {decl};"
            ))?;
        }
    }
    Ok(())
}

/// True when this binary is a `cargo build` artifact run straight from a
/// source checkout, rather than an installed one.
///
/// # This matched the BANNED build dir and not the MANDATED one (AMUX-2799)
///
/// It used to test `s.contains("/target/debug/")`, and a previous version of
/// this doc called the resulting exclusion deliberate: `rust-build-target`
/// contains the substring `target` without a leading slash, so the shared build
/// dir never tripped it. That reasoning was sound for the auto-builder, which
/// compiles into `rust-build-target/release/` and then INSTALLS — it never runs
/// the artifact.
///
/// It was exactly backwards for the hazard this guard exists for. CLAUDE.md
/// tells every session to export `CARGO_TARGET_DIR=~/.amux/rust-build-target`,
/// so an ordinary `cargo run -p amux-server` produces
/// `~/.amux/rust-build-target/debug/amux-server` — which did NOT match, so the
/// guard returned Ok and a working-tree build was free to apply an uncommitted
/// migration to the live database. That is AMUX-2652 verbatim, the incident
/// this guard was written for. Measured:
///
/// ```text
///   false   ~/.amux/rust-build-target/debug/amux-server   <- MANDATED, unguarded
///   true    ~/Dev/amux/target/debug/amux-server           <- BANNED, guarded
///   false   ~/.local/bin/amux-server-rs                   <- installed, correct
/// ```
///
/// Protection was on for the convention nobody is allowed to use and off for
/// the one everybody must. Nothing reported it, because the test had been
/// changed to inject a cargo-shaped fixture rather than read `current_exe()` —
/// which fixed a red test by removing the only assertion that touched the real
/// environmental input (ethos rule 7: a check that cannot fail).
///
/// # The rule now
///
/// A cargo artifact always sits under a `debug` or `release` PROFILE DIRECTORY,
/// whatever `CARGO_TARGET_DIR` is set to; an installed server never does. Both
/// real deployments are `bin/` paths with no such component —
/// `~/.local/bin/amux-server-rs` locally and `/usr/local/bin/amux-server-rs` in
/// the cloud image — so matching the component is precise in both directions
/// and, unlike a substring of one hardcoded layout, does not need revisiting
/// the next time the fleet moves its build directory.
fn is_cargo_target_build(exe: &std::path::Path) -> bool {
    exe.components()
        .any(|c| matches!(c.as_os_str().to_str(), Some("debug") | Some("release")))
}

/// True when `db_path` is the fleet's real database, `$HOME/.amux/amux.db`.
///
/// Compared after `canonicalize` where possible so `AMUX_HOME=~/.amux` (an
/// explicit spelling of the same file) is still recognised as live; a plain
/// string compare would miss it and the guard would not fire.
fn is_live_db(db_path: &std::path::Path, home: Option<&std::path::Path>) -> bool {
    let Some(home) = home else { return false };
    let live = home.join(".amux").join("amux.db");
    let norm = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    norm(db_path) == norm(&live)
}

/// AMUX-2652: a peer's ordinary `cargo run` must not migrate the LIVE database.
///
/// Migrations are `include_str!`-embedded at compile time, so a working-tree
/// build carries whatever migration files that tree has — including a
/// half-written one nobody has committed. The default DB path is the live file,
/// so `cargo run -p amux-server` in this checkout applied 0013_search.sql to
/// ~/.amux/amux.db at 22:18:23, 101 seconds after its author created it and
/// before they had made their read-only copy. It applied cleanly that time.
///
/// The point is not that it broke something; it is that no agent can honour
/// "never touch the live DB" while a PEER's build does it on their behalf. The
/// person who runs the command and the person whose migration runs are
/// different people, so care cannot prevent it — only a check can.
///
/// Scoped as narrowly as the hazard: it fires only for an UNAPPLIED migration,
/// from a cargo target build, against the live file. Anything else — the
/// installed server, a temp `AMUX_HOME`, a container (whose binary is not a
/// target artifact), a dev run with pending migrations against a scratch DB —
/// is untouched. Refusing too eagerly would be worse than the hazard: a server
/// that will not start is a live outage, and this must never be that.
fn guard_live_db(db_path: &std::path::Path, pending: &[&str]) -> anyhow::Result<()> {
    let Ok(exe) = std::env::current_exe() else {
        return Ok(()); // cannot tell what we are: do not block a real server
    };
    guard_live_db_with_exe(db_path, pending, &exe)
}

/// The guard with its one environmental input injected.
///
/// Split out so the positive test can supply a cargo-target-shaped path
/// instead of asserting something about where IT happens to have been built —
/// a test whose precondition depends on the build location cannot survive a
/// change in build location.
///
/// NB an earlier version of this paragraph described `is_cargo_target_build`
/// as "deliberately not matching ~/.amux/rust-build-target/" — PAST behaviour,
/// stated in the present tense. That non-match was the AMUX-2799 hole (guard
/// off for the mandated build dir, on for the banned one) and it is FIXED: the
/// matcher now keys on the debug/release profile component, which the shared
/// dir does contain. See is_cargo_target_build's own doc for the full arc. The
/// paragraph is corrected because a comment describing a closed hole as
/// current design is how the hole gets faithfully re-implemented.
fn guard_live_db_with_exe(
    db_path: &std::path::Path,
    pending: &[&str],
    exe: &std::path::Path,
) -> anyhow::Result<()> {
    if pending.is_empty() || truthy_env("AMUX_ALLOW_LIVE_DB") {
        return Ok(());
    }
    if !is_cargo_target_build(exe) {
        return Ok(());
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    if !is_live_db(db_path, home.as_deref()) {
        return Ok(());
    }
    anyhow::bail!(
        "refusing to apply {n} unapplied migration(s) to the LIVE database.\n\
         \n\
         Migration(s): {names}\n\
         Database    : {db}\n\
         Binary      : {exe} (a cargo target build, not the installed server)\n\
         \n\
         Migrations are embedded at COMPILE time, so this build carries whatever\n\
         is in its working tree — possibly a peer's uncommitted, half-written\n\
         migration. Applying it here would change the schema of the database the\n\
         whole fleet is using, on their behalf and without their knowledge.\n\
         \n\
         Point it somewhere else instead:\n\
           AMUX_HOME=$(mktemp -d) cargo run -p amux-server\n\
           AMUX_DB=/tmp/scratch.db cargo run -p amux-server\n\
         \n\
         Or, if you genuinely mean to migrate the live DB from a working-tree\n\
         build, say so explicitly: AMUX_ALLOW_LIVE_DB=1",
        n = pending.len(),
        names = pending.join(", "),
        db = db_path.display(),
        exe = exe.display(),
    )
}

fn truthy_env(key: &str) -> bool {
    std::env::var(key)
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false" && v != "no"
        })
        .unwrap_or(false)
}

/// Guard + apply. `Store::open` calls this; the bare [`apply_all`] stays for
/// tests, which run against temp and in-memory databases.
pub fn apply_all_guarded(conn: &mut Connection, db_path: &std::path::Path) -> anyhow::Result<()> {
    guard_live_db(db_path, &pending(conn))?;
    apply_all_with_home(conn, db_path.parent())
}

/// An in-memory DB carrying the REAL schema, for tests (AF-328).
///
/// Four test fixtures used to hand-write `CREATE TABLE issues (...)` mirroring
/// this crate's migrations, and nothing kept them in step. Adding a column meant
/// finding all four, and the failure when you missed one was badly misleading:
/// `COLS` selects the new column, `prepare` fails, an `unwrap_or_default()`
/// swallows the error, the query returns None, and the test reports its OWN
/// assertion. Migration 0037 produced 38 failures across `board_drive` and not
/// one of them mentioned a schema or named a column — the top one read
/// "the 3-day-old card must be worked before the fresh one, left: None", which
/// sends you to read the scoring logic. The same tax was paid on 0036.
///
/// Building the fixture FROM the migrations removes the class rather than
/// detecting it: there is one schema, so drift is not possible. A new column is
/// present in every fixture the moment its migration is registered.
///
/// The two deliberately NARROW fixtures are left alone on purpose — they declare
/// only the columns their test uses, so they mirror nothing and cannot drift.
/// [`test_memdb`] for INTEGRATION tests, which are separate crates and cannot
/// see `#[cfg(test)]` items (AMUX-3952). Same chain, same guarantee.
pub fn test_memdb_pub() -> Connection {
    let mut conn = Connection::open_in_memory().expect("in-memory db");
    apply_all(&mut conn).expect("migrations must apply cleanly to a fresh db");
    conn
}

#[cfg(test)]
pub(crate) fn test_memdb() -> Connection {
    let mut conn = Connection::open_in_memory().expect("in-memory db");
    apply_all(&mut conn).expect("migrations must apply cleanly to a fresh db");
    conn
}

pub fn apply_all(conn: &mut Connection) -> anyhow::Result<()> {
    apply_all_with_home(conn, None)
}

/// [`apply_all`], with the amux home a migration may archive rows into before
/// it drops them (the database's directory; None for an in-memory database).
pub fn apply_all_with_home(conn: &mut Connection, home: Option<&std::path::Path>) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _amux_migrations (
            version    INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            applied_at TEXT NOT NULL,
            -- Fresh databases get this from the start; existing ones get it
            -- from 0032's ADDCOL. Both paths, because CREATE TABLE IF NOT
            -- EXISTS is a no-op against a table that already exists and would
            -- silently leave every deployed database without the column.
            duration_ms INTEGER
        );",
    )?;
    // checksum / reconciled_name / reconciled_at: added by ALTER on every
    // database (fresh or not), for the same reason duration_ms needed two paths.
    ensure_bookkeeping_columns(conn)?;
    for m in MIGRATIONS {
        let sum = checksum(m.sql);
        if let Some(row) = recorded_row(conn, m.version)? {
            match mismatch_action(m, &row) {
                MismatchAction::AlreadyApplied => {
                    note_checksum(conn, m, &row, &sum);
                    continue;
                }
                MismatchAction::Apply => {
                    reconcile_name_mismatch(conn, m, &row, &sum)?;
                    continue;
                }
            }
        }
        // TIME EVERY MIGRATION. This function used to log nothing, so 0031
        // holding the connection for 186 seconds on 2026-08-24 was
        // indistinguishable from a crash, a slow build, or a launchd problem:
        // the only symptom anyone could see was /health not answering. A
        // migration is the one startup step that can take arbitrarily long
        // while looking exactly like a dead process.
        let started = std::time::Instant::now();
        let sets = archive_sets(m.version);
        if !sets.is_empty() {
            archive_before_drop(conn, home, m.name, sets)?;
        }
        if m.version == DROP_INTERACTION_LOG {
            super::audit_archive::interaction_log(conn, home)?;
        }
        let tx = conn.transaction()?;
        apply_one(&tx, m.sql)?;
        let ms = started.elapsed().as_millis() as i64;
        tx.execute(
            "INSERT INTO _amux_migrations (version, name, applied_at, checksum) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![m.version, m.name, chrono::Utc::now().to_rfc3339(), sum],
        )?;
        // Best-effort: the column arrives in 0032, so every migration before it
        // on an existing database has nowhere to put this. Failing to RECORD a
        // duration must never fail the migration itself, and the tracing line
        // below carries the number regardless.
        let _ = tx.execute(
            "UPDATE _amux_migrations SET duration_ms = ?1 WHERE version = ?2",
            rusqlite::params![ms, m.version],
        );
        tx.commit()?;
        // WARN, not INFO, above a threshold a human would notice as downtime.
        // 2s is well above every migration here except the one that caused the
        // outage, so this fires on the shape that matters and stays quiet
        // otherwise.
        if ms >= 2_000 {
            tracing::warn!(
                migration = m.name, duration_ms = ms,
                "migration held the database for {:.1}s — the server is unreachable for this whole period",
                ms as f64 / 1000.0
            );
        } else {
            tracing::info!(migration = m.name, duration_ms = ms, "migration applied");
        }
    }
    report_renumbered_migrations(conn);
    Ok(())
}

/// Record (first sight) or compare (later boots) an applied migration's
/// checksum. Never fails the boot: this is bookkeeping, not schema.
fn note_checksum(conn: &Connection, m: &Migration, row: &RecordedRow, sum: &str) {
    match row.checksum.as_deref() {
        Some(recorded) if recorded == sum => {}
        Some(recorded) => {
            // An applied migration's file was edited. Its new text will not
            // run again here, so whatever the edit changed is not in this
            // database. Logged once: the new checksum is then recorded.
            tracing::warn!(
                verdict = "migration_checksum_changed",
                migration_version = m.version,
                migration = m.name,
                recorded_checksum = recorded,
                file_checksum = sum,
                "migration {} was edited after this database applied it; the edit will not run here",
                m.name
            );
            let _ = conn.execute(
                "UPDATE _amux_migrations SET checksum = ?1 WHERE version = ?2",
                rusqlite::params![sum, m.version],
            );
        }
        None => {
            // Applied before checksums were recorded: adopt the file's.
            let _ = conn.execute(
                "UPDATE _amux_migrations SET checksum = ?1 WHERE version = ?2",
                rusqlite::params![sum, m.version],
            );
        }
    }
}

/// A version this database recorded under a DIFFERENT migration's name.
///
/// The runner used to skip it, because it deduped on version alone: a dirty
/// build that consumed version N meant the real N never ran, with a clean boot
/// (AMUX-4533; the reason nine tables were created at their point of use, see
/// 0176). Now the registered migration runs in its own transaction and, when it
/// succeeds, the row records `reconciled_name` so it runs once. The original
/// `name` is left as found: it is the evidence of what consumed the number.
///
/// Failure never bricks the boot. A migration that cannot run against this
/// database's shape (a plain CREATE TABLE of a table the other migration also
/// made, say) is rolled back whole, logged at ERROR with
/// `migration_name_mismatch_apply_failed`, and retried on the next boot; the
/// server still starts, exactly as it did when the version was skipped.
fn reconcile_name_mismatch(
    conn: &mut Connection,
    m: &Migration,
    row: &RecordedRow,
    sum: &str,
) -> anyhow::Result<()> {
    tracing::warn!(
        verdict = "migration_name_mismatch",
        migration_version = m.version,
        recorded_name = %row.name,
        file_name = m.name,
        "migration version {} is recorded as {:?} but this binary registers {:?}; \
         applying {:?} now rather than skipping it",
        m.version,
        row.name,
        m.name,
        m.name
    );
    let started = std::time::Instant::now();
    let tx = conn.transaction()?;
    match apply_one(&tx, m.sql) {
        Ok(()) => {
            tx.execute(
                "UPDATE _amux_migrations SET reconciled_name = ?1, reconciled_at = ?2, checksum = ?3 \
                 WHERE version = ?4",
                rusqlite::params![m.name, chrono::Utc::now().to_rfc3339(), sum, m.version],
            )?;
            tx.commit()?;
            tracing::warn!(
                verdict = "migration_name_mismatch_applied",
                migration_version = m.version,
                recorded_name = %row.name,
                file_name = m.name,
                duration_ms = started.elapsed().as_millis() as i64,
                "applied {} under version {} (recorded name {:?} kept)",
                m.name,
                m.version,
                row.name
            );
        }
        Err(e) => {
            drop(tx); // rollback: nothing of the failed migration persists
            tracing::error!(
                verdict = "migration_name_mismatch_apply_failed",
                migration_version = m.version,
                recorded_name = %row.name,
                file_name = m.name,
                error = %e,
                "could not apply {} over version {} (recorded as {:?}): {e}. Its schema is \
                 missing from this database; renumber it or reconcile by hand. Retried next boot.",
                m.name,
                m.version,
                row.name
            );
        }
    }
    Ok(())
}

/// Versions whose RECORDED name differs from the name now registered for them
/// AND that `apply_all` could not reconcile, as `(version, recorded, registered)`.
///
/// AF-353. The runner used to dedupe on VERSION alone, so a version this
/// database had already applied was skipped no matter WHICH migration now
/// claimed it. Renumbering an incoming migration is the documented fix for a
/// contributor collision (see `versions_are_dense_and_match_their_filenames`),
/// which frees a number for a different migration, and every database that
/// recorded the old one at that version skipped the new one forever.
///
/// `apply_all` now runs the registered migration over a mismatched row and
/// records `reconciled_name` (see `reconcile_name_mismatch`). What is left here
/// is the residue: mismatches whose apply failed, so the schema really lags.
/// Documented same-migration renames (`MIGRATION_NAME_ALIASES`) are excluded.
pub(crate) fn renumbered_migrations(conn: &Connection) -> Vec<(i64, String, String)> {
    let mut stmt = match conn.prepare("SELECT version FROM _amux_migrations") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let versions: Vec<i64> = match stmt.query_map([], |r| r.get::<_, i64>(0)) {
        Ok(r) => r.flatten().collect(),
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for version in versions {
        let Some(m) = MIGRATIONS.iter().find(|m| m.version == version) else { continue };
        let Ok(Some(row)) = recorded_row(conn, version) else { continue };
        // A mismatch `apply_all` reconciled (ran the registered migration) is no
        // longer a lagging schema; one whose apply failed still is.
        if mismatch_action(m, &row) == MismatchAction::Apply {
            out.push((version, row.name, m.name.to_string()));
        }
    }
    out.sort();
    out
}

/// Log the above. Separate from the query so the decision is testable without a
/// tracing subscriber, and so the count is emitted even when the list is empty
/// — a silent probe and a clean database are the same output otherwise, which is
/// the `measured` contract (ethos rule 4) at the point where someone greps.
fn report_renumbered_migrations(conn: &Connection) {
    let found = renumbered_migrations(conn);
    if found.is_empty() {
        tracing::info!(
            renumbered_migrations = 0,
            "migration version/name binding checked"
        );
        return;
    }
    for (version, recorded, registered) in &found {
        tracing::warn!(
            migration_version = version,
            recorded_name = %recorded,
            registered_name = %registered,
            verdict = "migration_name_mismatch_unreconciled",
            "migration version {version} was applied as {recorded:?} but is now registered as \
             {registered:?}, and applying {registered:?} over it failed (see \
             migration_name_mismatch_apply_failed above). This database's schema lags the code."
        );
    }
    tracing::warn!(
        renumbered_migrations = found.len(),
        "{} migration version(s) do not match their recorded name — see the lines above",
        found.len()
    );
}

/// Apply ONE migration's body, honouring `-- ADDCOL:` directives.
///
/// `pub(crate)` so tests can build a schema through the SHIPPED path. A fixture
/// that `execute_batch`es a migration file silently SKIPS every ADDCOL line —
/// they are SQL comments — so it would prove a column exists that production
/// has and the test does not, or vice versa (AF-99).
pub(crate) fn apply_one(conn: &Connection, sql: &str) -> anyhow::Result<()> {
    // Split ADDCOL directives from plain SQL. Directives are full-line
    // comments so the file stays valid SQL for external tools.
    let mut plain = String::new();
    let mut addcols: Vec<(String, String, String)> = Vec::new();
    for line in sql.lines() {
        if let Some(rest) = line.trim().strip_prefix("-- ADDCOL:") {
            let mut parts = rest.trim().splitn(3, ' ');
            let (Some(table), Some(column), Some(decl)) =
                (parts.next(), parts.next(), parts.next())
            else {
                anyhow::bail!("malformed ADDCOL directive: {line:?}");
            };
            addcols.push((table.to_string(), column.to_string(), decl.to_string()));
        } else {
            plain.push_str(line);
            plain.push('\n');
        }
    }
    // ADDCOL BEFORE the plain SQL (AMUX-3609). "Add a column, then populate it"
    // is the natural shape for a backfill migration, and with the old order it
    // was impossible: the UPDATE ran first and died on `no such column`. Nobody
    // had hit it because every ADDCOL migration so far only added.
    //
    // Safe for the existing 31 because ADDCOL targets tables that already exist
    // — that is what the directive is FOR (a live Python DB whose tables may or
    // may not already carry the column). Audited before reordering: no
    // migration both CREATEs and ADDCOLs the same table, which is the only
    // arrangement this order would break. `addcol_can_be_used_by_the_same_
    // migration` pins the new guarantee.
    for (table, column, decl) in addcols {
        if !column_exists(conn, &table, &column)? {
            // Identifiers cannot be bound as parameters; they come from our
            // own embedded migration files, not user input.
            conn.execute_batch(&format!(
                "ALTER TABLE \"{table}\" ADD COLUMN \"{column}\" {decl};"
            ))?;
        }
    }
    if !plain.trim().is_empty() {
        conn.execute_batch(&plain)?;
    }
    Ok(())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> anyhow::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod registration_guard {
    use super::MIGRATIONS;

    /// A `.sql` file nobody registered is a migration that NEVER RUNS, and
    /// nothing anywhere says so (AF-99).
    ///
    /// MIGRATIONS is a hand-maintained array, so adding a file and forgetting the
    /// entry is silent in every direction: the server boots clean, the schema is
    /// simply older than the code, and the first symptom is a query failing at
    /// runtime somewhere far away. That is exactly what happened — 0022/0023 sat
    /// on disk unregistered while `/api/debug/downtime` returned an empty list,
    /// which reads identically to "there were never any outages".
    #[test]
    fn every_migration_file_on_disk_is_registered() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
            .expect("migrations dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".sql"))
            .map(|n| n.trim_end_matches(".sql").to_owned())
            .collect();
        on_disk.sort();

        // The control. If read_dir ever returned nothing — wrong path, renamed
        // directory — an equality assert could only fail loudly, but a future
        // rewrite into a subset check would pass vacuously. Pin it here.
        assert!(
            on_disk.len() >= 23,
            "found {} migration files; the scan is looking in the wrong place",
            on_disk.len()
        );

        let mut registered: Vec<String> = MIGRATIONS.iter().map(|m| m.name.to_owned()).collect();
        registered.sort();

        let unregistered: Vec<&String> =
            on_disk.iter().filter(|n| !registered.contains(n)).collect();
        let missing_file: Vec<&String> =
            registered.iter().filter(|n| !on_disk.contains(n)).collect();

        // Name the offending FILE rather than printing two sorted lists. On this
        // shared checkout the usual cause is a peer mid-work who wrote the .sql
        // before editing the array, and a diff of 24 names does not say that.
        assert!(
            unregistered.is_empty(),
            "migration file(s) on disk but NOT in the MIGRATIONS array: {unregistered:?}. \
             An unregistered migration never runs: the server boots clean, the schema \
             silently lags the code, and the first symptom is a query failing somewhere \
             far away. Add a Migration{{version, name, sql: include_str!(..)}} entry. \
             (If this is not your file, a peer is mid-work — tell them rather than \
             registering it for them.)"
        );
        assert!(
            missing_file.is_empty(),
            "MIGRATIONS references file(s) that do not exist: {missing_file:?}"
        );
    }

    /// Versions must be dense and ascending, or `apply_all`'s ordering and the
    /// `_amux_migrations` bookkeeping stop lining up with the filenames.
    #[test]
    fn versions_are_dense_and_match_their_filenames() {
        for (i, m) in MIGRATIONS.iter().enumerate() {
            let expected = i as i64 + 1;
            assert_eq!(
                m.version, expected,
                "{} is out of order — expected version {expected} at this position.\n\
                 If this fired while merging an OUTSIDE PR, it is very likely the \
                 contributor-collision case rather than their error: they pick a number \
                 against origin/main, this branch runs ahead of it, and a number that is free \
                 from outside can already be taken here. CI cannot see it (their branch builds \
                 against origin/main, where there is no conflict), so this guard is the first \
                 thing that can. Renumber the incoming migration and its MIGRATIONS entry; do \
                 not send it back as their bug. Twice on PR #160; see CONTRIBUTING.md.",
                m.name
            );
            let prefix = format!("{:04}_", m.version);
            assert!(
                m.name.starts_with(&prefix),
                "{} does not start with its own version prefix {prefix}",
                m.name
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_history_through(conn: &Connection, through: i64) {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS _amux_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                applied_at TEXT NOT NULL,
                duration_ms INTEGER
            );",
        )
        .unwrap();
        for migration in MIGRATIONS.iter().filter(|m| m.version <= through) {
            apply_one(conn, migration.sql).unwrap();
            conn.execute(
                "INSERT INTO _amux_migrations(version,name,applied_at,duration_ms) VALUES(?1,?2,'test',0)",
                rusqlite::params![migration.version, migration.name],
            )
            .unwrap();
        }
    }

    /// A database at 0175 holding `_amux_messages` rows and a queued command
    /// that names one.
    fn messages_fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        apply_history_through(&conn, DROP_AMUX_MESSAGES - 1);
        conn.execute_batch(
            "INSERT INTO _amux_messages (id, from_actor, target, body, thread, created_at, delivery) VALUES
               ('msg_01A', '{\"kind\":\"human\",\"name\":\"owner\"}', '{\"kind\":\"worker\",\"id\":\"wrk_1\"}', 'first kept body', NULL, '2026-09-01T00:00:00Z', '{\"state\":\"queued\"}'),
               ('msg_01B', '{\"kind\":\"human\",\"name\":\"owner\"}', '{\"kind\":\"worker\",\"id\":\"wrk_1\"}', 'second kept body', NULL, '2026-09-02T00:00:00Z', '{\"state\":\"delivered\"}');
             INSERT INTO _amux_commands (id, worker_id, command, state, idempotency_key, queued_at, attempts, timing) VALUES
               ('cmd_1', 'wrk_1', '{\"kind\":\"deliver_message\",\"data\":\"msg_01A\"}', '{\"state\":\"queued\"}', 'deliver-msg_01A', '2026-09-01T00:00:00Z', 0, '{\"kind\":\"at_turn_boundary\"}'),
               ('cmd_2', 'wrk_1', '{\"kind\":\"deliver_message\",\"data\":\"msg_01B\"}', '{\"state\":\"confirmed\"}', 'deliver-msg_01B', '2026-09-02T00:00:00Z', 1, '{\"kind\":\"at_turn_boundary\"}');",
        ).unwrap();
        conn
    }

    #[test]
    fn dropping_amux_messages_archives_its_rows_first_and_never_overwrites_the_archive() {
        let mut conn = messages_fixture();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM search_docs WHERE entity_type='message'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2, "premise: the old search trigger indexed both messages");
        let home = tempfile::tempdir().unwrap();
        apply_all_with_home(&mut conn, Some(home.path())).unwrap();
        let path = home.path().join(AUDIT_ARCHIVE_DIR).join("_amux_messages.json");
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(rows.len(), 2, "every row is kept");
        assert_eq!(rows[0]["id"], "msg_01A");
        assert_eq!(rows[1]["body"], "second kept body");
        assert_eq!(rows[0]["delivery"], "{\"state\":\"queued\"}", "columns are kept as stored");
        let cmds: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(
            home.path().join(AUDIT_ARCHIVE_DIR).join("_amux_commands.deliver_message.json")).unwrap()).unwrap();
        assert_eq!(cmds.len(), 2, "the commands that named them are kept too");
        let gone = conn.query_row("SELECT 1 FROM sqlite_master WHERE name='_amux_messages'", [], |_| Ok(())).optional().unwrap();
        assert!(gone.is_none(), "the table is dropped");
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM search_docs WHERE entity_type='message'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "and its search documents");
        // Both commands read again as WorkerCommand; the unfinished one is dead-lettered with its old id.
        for id in ["cmd_1", "cmd_2"] {
            let (command, state): (String, String) = conn.query_row(
                "SELECT command, state FROM _amux_commands WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            let parsed: amux_core::protocol::WorkerCommand = serde_json::from_str(&command).unwrap();
            assert_eq!(parsed, amux_core::protocol::WorkerCommand::DeliverMessage(amux_core::ids::HistoryId(0)), "{id}");
            let st: amux_core::protocol::CommandState = serde_json::from_str(&state).unwrap();
            if id == "cmd_1" {
                assert!(matches!(&st, amux_core::protocol::CommandState::DeadLettered { reason } if reason.contains("msg_01A")), "{state}");
            } else {
                assert_eq!(st, amux_core::protocol::CommandState::Confirmed, "a finished command keeps its state");
            }
        }
        // Another database dropping its table later never overwrites the first archive.
        let mut again = messages_fixture();
        again.execute("UPDATE _amux_messages SET body='changed'", []).unwrap();
        apply_all_with_home(&mut again, Some(home.path())).unwrap();
        let kept: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(kept[0]["body"], "first kept body");
    }

    #[test]
    fn rows_with_nowhere_to_archive_them_refuse_the_drop() {
        let mut conn = messages_fixture();
        let e = apply_all(&mut conn).unwrap_err().to_string();
        assert!(e.contains("no home to archive"), "{e}");
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM _amux_messages", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2, "nothing was dropped");
        // An empty table needs no archive.
        conn.execute_batch("DELETE FROM _amux_messages; DELETE FROM _amux_commands;").unwrap();
        apply_all(&mut conn).unwrap();
    }

    /// A database just before the drop, holding `interaction_log` rows.
    fn interaction_log_fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        apply_history_through(&conn, DROP_INTERACTION_LOG - 1);
        conn.execute_batch(
            "INSERT INTO interaction_log (ts, kind, actor, target, action, detail, before, result, seq) VALUES
               (1794675511113, 'scope', 'human', 'group:alpha', 'write:gates', '{\"capability\":\"gates\"}', '{}', '{\"done\":[]}', 1),
               (1795033259429, 'scope', 'amux', 'worker:w1', 'write:env', '{}', '', 'kept result', 1);",
        )
        .unwrap();
        conn
    }

    /// A database just before 0180 holding a `tasks` row and a planning node
    /// with the goal contract its foreign key requires.
    fn dormant_planning_fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        apply_history_through(&conn, DROP_DORMANT_PLANNING - 1);
        conn.execute_batch(
            "INSERT INTO tasks (id, session, text, done, pos, created, updated)
               VALUES ('19c7bf83a1e24746d', 'ops', 'tailscale ingress', 0, 1, 1771606391, 1771606391);
             INSERT INTO _amux_goal_contracts (id, objective, dependency_policy, release_policy,
               expected_scope_min, expected_scope_max, version, updated_by, updated_at)
               VALUES ('goal-1', 'ship it', 'none', 'manual', 1, 1, 1, 'lane-a', '2026-09-01T00:00:00Z');
             INSERT INTO _amux_planning_nodes (id, goal_id, parent_id, role, owner, title, objective, status, updated_at)
               VALUES ('pn_1', 'goal-1', NULL, 'root_planner', 'lane-a', 'root', 'plan it', 'active', '2026-09-01T00:00:00Z');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn dropping_interaction_log_archives_its_rows_first_and_never_overwrites_the_archive() {
        let mut conn = interaction_log_fixture();
        let home = tempfile::tempdir().unwrap();
        apply_all_with_home(&mut conn, Some(home.path())).unwrap();
        let path = home.path().join(AUDIT_ARCHIVE_DIR).join("interaction_log.json");
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(rows.len(), 2, "every row is kept");
        assert_eq!(rows[0]["target"], "group:alpha");
        assert_eq!(rows[0]["ts"], 1794675511113i64, "integers stay integers");
        assert_eq!(rows[1]["result"], "kept result");
        assert_eq!(rows[0]["detail"], "{\"capability\":\"gates\"}", "columns are kept as stored");
        assert!(!table_exists(&conn, "interaction_log"), "the table is dropped");

        // A second database migrating into the same home must not overwrite
        // the first copy.
        let mut again = interaction_log_fixture();
        again.execute("DELETE FROM interaction_log WHERE id = 1", []).unwrap();
        apply_all_with_home(&mut again, Some(home.path())).unwrap();
        let kept: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(kept.len(), 2, "the first archive is left as it is");
    }

    #[test]
    fn dropping_interaction_log_is_refused_when_its_rows_have_nowhere_to_go() {
        let mut conn = interaction_log_fixture();
        let err = apply_all_with_home(&mut conn, None).unwrap_err().to_string();
        assert!(err.contains("interaction_log") && err.contains("2 row"), "{err}");
        assert!(table_exists(&conn, "interaction_log"), "nothing is dropped when the archive cannot be written");
        let applied: Option<i64> = conn
            .query_row("SELECT version FROM _amux_migrations WHERE version=?1", [DROP_INTERACTION_LOG], |r| r.get(0))
            .optional()
            .unwrap();
        assert_eq!(applied, None, "the drop is not recorded as applied");

        // An archive directory that cannot be created refuses too.
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("archive"), b"a file where the directory goes").unwrap();
        let mut blocked = interaction_log_fixture();
        assert!(apply_all_with_home(&mut blocked, Some(home.path())).is_err());
        assert!(table_exists(&blocked, "interaction_log"));
    }

    #[test]
    fn dropping_an_empty_interaction_log_needs_no_archive() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_all_with_home(&mut conn, None).expect("a fresh database has nothing to keep");
        assert!(!table_exists(&conn, "interaction_log"));
    }

    #[test]
    fn dropping_the_dormant_planning_tables_archives_their_rows_first() {
        let mut conn = dormant_planning_fixture();
        let home = tempfile::tempdir().unwrap();
        apply_all_with_home(&mut conn, Some(home.path())).unwrap();
        let dir = home.path().join(AUDIT_ARCHIVE_DIR);
        let tasks: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(dir.join("tasks.json")).unwrap()).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["text"], "tailscale ingress", "columns are kept as stored");
        let nodes: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(dir.join("_amux_planning_nodes.json")).unwrap()).unwrap();
        assert_eq!(nodes[0]["owner"], "lane-a");
        let goals: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(dir.join("_amux_goal_contracts.json")).unwrap()).unwrap();
        assert_eq!(goals[0]["id"], "goal-1");
        assert!(!dir.join("_amux_task_budgets.json").exists(), "an empty table writes no file");
        for t in DORMANT_PLANNING_TABLES {
            let gone = conn.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1", [t], |_| Ok(())).optional().unwrap();
            assert!(gone.is_none(), "{t} is dropped");
        }
        for kept in ["_amux_leases", "_amux_attempts", "_amux_decompositions", "_amux_memories", "_amux_workers"] {
            let there = conn.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1", [kept], |_| Ok(())).optional().unwrap();
            assert!(there.is_some(), "{kept} is kept");
        }
        // A second database never overwrites the first archive.
        let mut again = dormant_planning_fixture();
        again.execute("UPDATE tasks SET text='changed'", []).unwrap();
        apply_all_with_home(&mut again, Some(home.path())).unwrap();
        let kept: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(dir.join("tasks.json")).unwrap()).unwrap();
        assert_eq!(kept[0]["text"], "tailscale ingress");
    }

    #[test]
    fn dormant_planning_rows_with_nowhere_to_archive_them_refuse_the_drop() {
        let mut conn = dormant_planning_fixture();
        let e = apply_all(&mut conn).unwrap_err().to_string();
        assert!(e.contains("no home to archive") && e.contains("0180"), "{e}");
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "nothing was dropped");
        conn.execute_batch("DELETE FROM tasks; DELETE FROM _amux_planning_nodes; DELETE FROM _amux_goal_contracts;").unwrap();
        apply_all(&mut conn).unwrap();
    }

    /// Either 0082/0083 history converges, and 0139 then removes the project
    /// columns while keeping what the main line still uses.
    fn assert_reconciled_project_schema(conn: &Connection) {
        for (table, column, want) in [
            ("group_config", "execution_policy", false),
            ("group_config", "execution_rev", false),
            ("issues", "project_group", false),
            ("issues", "execution_state", false),
            ("cmd_history", "project_group", false),
            ("steering_queue", "sender", true),
        ] {
            let present: bool = conn
                .query_row(
                    &format!(
                        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name=?1)"
                    ),
                    [column],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(present, want, "{table}.{column}");
        }
        let view: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='legacy_execution_issues')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!view, "the project view is dropped with its column");
        let nudge: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='board_drive_nudge_budget')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            nudge,
            "the main-line nudge budget must survive reconciliation"
        );
    }

    #[test]
    fn current_main_history_upgrades_through_the_project_removal() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_history_through(&conn, 82);
        apply_all(&mut conn).unwrap();
        assert_reconciled_project_schema(&conn);
        assert!(renumbered_migrations(&conn).is_empty());
    }

    #[test]
    fn deployed_project_branch_history_converges_without_collision_or_missing_main_schema() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_history_through(&conn, 81);
        apply_one(
            &conn,
            include_str!("../../migrations/0083_project_execution.sql"),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO _amux_migrations(version,name,applied_at,duration_ms) VALUES(82,'0082_project_execution','test',0)",
            [],
        )
        .unwrap();
        apply_one(
            &conn,
            include_str!("../../migrations/0084_project_attempts.sql"),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO _amux_migrations(version,name,applied_at,duration_ms) VALUES(83,'0083_project_attempts','test',0)",
            [],
        )
        .unwrap();

        apply_all(&mut conn).unwrap();
        assert_reconciled_project_schema(&conn);
        assert!(renumbered_migrations(&conn).is_empty());
        let recorded: String = conn
            .query_row(
                "SELECT name FROM _amux_migrations WHERE version=82",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            recorded, "0082_project_execution",
            "deployed history remains immutable"
        );
    }

    /// The nine tables that used to be created at their point of use (audit
    /// 2026-11-26 P1). Each must come from a migration now.
    const POINT_OF_USE_TABLES: &[&str] = &[
        "task_attempts",
        "task_episodes",
        "card_budgets",
        "group_config",
        "file_visits",
        "nav_visits",
        "nav_picks",
        "event_dispatch_cursor",
        "_amux_brex_spend",
    ];

    fn table_exists(conn: &Connection, table: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .is_ok()
    }

    #[derive(Clone)]
    struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    struct CapturedLogWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = CapturedLogWriter;
        fn make_writer(&'a self) -> Self::Writer {
            CapturedLogWriter(self.0.clone())
        }
    }
    impl std::io::Write for CapturedLogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn with_captured_logs<T>(f: impl FnOnce() -> T) -> (T, String) {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_writer(CapturedLogs(buf.clone()))
            .finish();
        let _scope = tracing::subscriber::set_default(subscriber);
        let result = f();
        let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        (result, logs)
    }

    #[test]
    fn every_point_of_use_table_exists_after_a_fresh_migrate() {
        let conn = test_memdb();
        let missing: Vec<&str> =
            POINT_OF_USE_TABLES.iter().copied().filter(|t| !table_exists(&conn, t)).collect();
        assert!(missing.is_empty(), "tables a fresh migrate did not create: {missing:?}");
        // The indexes the point-of-use DDL carried came with them.
        for idx in [
            "idx_task_attempts_card_attempt",
            "idx_task_attempts_running",
            "idx_task_attempts_worker",
            "idx_task_episodes_card",
            "idx_task_episodes_worker",
            "idx_task_episodes_open",
            "idx_brex_spend_card_time",
        ] {
            assert!(
                conn.query_row(
                    "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1",
                    [idx],
                    |_| Ok(())
                )
                .is_ok(),
                "index {idx} missing after a fresh migrate"
            );
        }
    }

    /// A `CREATE TABLE IF NOT EXISTS` for one of the moved tables anywhere in
    /// `src/` reintroduces the workaround (and a second spelling of the DDL
    /// that drifts from the migration). Migrations live in `migrations/`.
    #[test]
    fn point_of_use_tables_are_created_only_in_migrations() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        walk(&root.join("src"), &mut files);
        walk(&root.join("tests"), &mut files);
        // The control: the scan must actually see source, or it passes vacuously.
        assert!(files.len() > 50, "scanned only {} .rs files; wrong root", files.len());
        let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_uppercase();
        let mut offenders = Vec::new();
        for f in &files {
            let text = squash(&std::fs::read_to_string(f).unwrap());
            for t in POINT_OF_USE_TABLES {
                // api/brex.rs is owner-tier (review-tiers.txt), so its lazy
                // CREATE is removed in its own commit for the owner's approval.
                // Until then 0176 creates the table and this CREATE is a no-op.
                if *t == "_amux_brex_spend" && f.ends_with("api/brex.rs") {
                    continue;
                }
                // Built at runtime so this test's own source does not match.
                let needle = format!("{} {}", ["CREATE TABLE IF NOT", "EXISTS"].join(" "), t.to_ascii_uppercase());
                let hit = text.match_indices(&needle).any(|(i, _)| {
                    // whole-identifier match: `nav_visits` must not hit `nav_visits_x`
                    text[i + needle.len()..]
                        .chars()
                        .next()
                        .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
                });
                if hit {
                    offenders.push(format!("{}: {t}", f.strip_prefix(root).unwrap().display()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "point-of-use CREATE TABLE for a migrated table (put schema in a numbered \
             migration; the runner no longer skips a mismatched version): {offenders:?}"
        );
    }

    /// AF-353 / audit P1: a version whose recorded name is not the registered
    /// one is DETECTED (logged with `migration_name_mismatch`), the registered
    /// migration RUNS, and its tables exist afterwards. A clean database stays
    /// quiet.
    #[test]
    fn a_name_mismatch_on_an_applied_version_is_logged_and_applied() {
        let mut conn = Connection::open_in_memory().unwrap();
        let ((), quiet) = with_captured_logs(|| apply_all(&mut conn).unwrap());
        // THE CONTROL: a database in step with MIGRATIONS logs no mismatch.
        assert!(!quiet.contains("migration_name_mismatch"), "clean db logged a mismatch: {quiet}");
        assert_eq!(renumbered_migrations(&conn), Vec::new());

        // The AMUX-4533 shape: a dirty build consumed 0176's number for some
        // other migration, so 0176's tables were never made.
        let m = MIGRATIONS.iter().find(|m| m.name == "0176_point_of_use_tables").unwrap();
        conn.execute_batch(
            "DROP TABLE task_attempts; DROP TABLE task_episodes; DROP TABLE card_budgets;
             DROP TABLE file_visits; DROP TABLE nav_visits; DROP TABLE nav_picks;
             DROP TABLE _amux_brex_spend;",
        )
        .unwrap();
        conn.execute(
            "UPDATE _amux_migrations SET name = '0176_some_dirty_build', checksum = NULL WHERE version = ?1",
            [m.version],
        )
        .unwrap();
        assert_eq!(renumbered_migrations(&conn).len(), 1, "detected before the boot");
        assert_eq!(pending(&conn), vec![m.name], "the live-DB guard must count it as pending");

        let ((), logs) = with_captured_logs(|| apply_all(&mut conn).unwrap());
        assert!(logs.contains("migration_name_mismatch"), "no mismatch WARN: {logs}");
        assert!(logs.contains("0176_some_dirty_build") && logs.contains(m.name), "{logs}");
        assert!(logs.contains("migration_name_mismatch_applied"), "{logs}");
        for t in POINT_OF_USE_TABLES {
            assert!(table_exists(&conn, t), "{t} must exist after the mismatch was reconciled");
        }
        let (name, reconciled): (String, Option<String>) = conn
            .query_row(
                "SELECT name, reconciled_name FROM _amux_migrations WHERE version = ?1",
                [m.version],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(name, "0176_some_dirty_build", "the recorded name is kept as evidence");
        assert_eq!(reconciled.as_deref(), Some(m.name));
        assert!(renumbered_migrations(&conn).is_empty(), "reconciled is no longer lagging");
        assert!(pending(&conn).is_empty());

        // Once: a later boot neither re-runs nor re-logs it.
        let ((), again) = with_captured_logs(|| apply_all(&mut conn).unwrap());
        assert!(!again.contains("migration_name_mismatch"), "re-logged on a later boot: {again}");
    }

    /// A mismatched migration that CANNOT run against this database's shape
    /// must not brick the boot: rolled back whole, logged, still reported, and
    /// still pending for the live-DB guard.
    #[test]
    fn a_name_mismatch_whose_migration_fails_does_not_brick_the_boot() {
        let mut conn = test_memdb();
        // 0053 is a plain ALTER ADD COLUMN; on a db that already has the column
        // it fails with "duplicate column", which is the failing shape.
        let m = MIGRATIONS.iter().find(|m| m.name.contains("telegram_relay_dedup")).unwrap();
        conn.execute(
            "UPDATE _amux_migrations SET name = 'other_branch', checksum = NULL WHERE version = ?1",
            [m.version],
        )
        .unwrap();
        let (res, logs) = with_captured_logs(|| apply_all(&mut conn));
        res.expect("a failed reconcile must not fail the boot");
        assert!(logs.contains("migration_name_mismatch_apply_failed"), "{logs}");
        assert!(logs.contains("migration_name_mismatch_unreconciled"), "{logs}");
        let reconciled: Option<String> = conn
            .query_row(
                "SELECT reconciled_name FROM _amux_migrations WHERE version = ?1",
                [m.version],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reconciled, None, "a failed apply must not be recorded as reconciled");
        assert_eq!(renumbered_migrations(&conn).len(), 1);
        assert_eq!(pending(&conn), vec![m.name]);
    }

    #[test]
    fn a_known_same_migration_rename_is_not_reported_as_a_collision() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_all(&mut conn).unwrap();
        conn.execute(
            "UPDATE _amux_migrations SET name = ?1 WHERE version = 35",
            ["0029_regenerable_samples"],
        )
        .unwrap();
        let m35 = MIGRATIONS.iter().find(|m| m.version == 35).unwrap();
        let row = |name: &str| RecordedRow { name: name.into(), reconciled_name: None, checksum: None };
        assert_eq!(
            mismatch_action(m35, &row("0029_regenerable_samples")),
            MismatchAction::AlreadyApplied,
            "the documented rename performed the same schema work"
        );
        assert!(
            renumbered_migrations(&conn).is_empty(),
            "known aliases must not keep every healthy startup in a false error state"
        );
        assert_eq!(
            mismatch_action(m35, &row("0035_unrelated")),
            MismatchAction::Apply,
            "only the exact documented alias may be suppressed"
        );
        // And a healthy boot over the alias does not re-run 0035.
        let ((), logs) = with_captured_logs(|| apply_all(&mut conn).unwrap());
        assert!(!logs.contains("migration_name_mismatch"), "{logs}");
    }

    #[test]
    fn an_edited_applied_migration_is_logged_once() {
        let mut conn = test_memdb();
        conn.execute("UPDATE _amux_migrations SET checksum = 'stale' WHERE version = 1", []).unwrap();
        let ((), logs) = with_captured_logs(|| apply_all(&mut conn).unwrap());
        assert!(logs.contains("migration_checksum_changed"), "{logs}");
        let ((), again) = with_captured_logs(|| apply_all(&mut conn).unwrap());
        assert!(!again.contains("migration_checksum_changed"), "{again}");
        let missing: i64 = conn
            .query_row("SELECT COUNT(*) FROM _amux_migrations WHERE checksum IS NULL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(missing, 0, "every applied migration records a checksum");
    }

    /// A version RECORDED but no longer registered at all is NOT a renumber and
    /// must not be reported as one.
    ///
    /// This is the false-positive edge: a database that ran a migration the code
    /// has since dropped has a row with no counterpart, and reporting it would
    /// send someone hunting a skipped migration that does not exist. The check
    /// joins on the registered side for exactly this reason, and a join is the
    /// kind of thing that gets "simplified" into a scan.
    #[test]
    fn a_recorded_version_the_code_no_longer_registers_is_not_a_renumber() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_all(&mut conn).unwrap();
        let orphan = MIGRATIONS.iter().map(|m| m.version).max().unwrap() + 999;
        conn.execute(
            "INSERT INTO _amux_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![orphan, "9999_dropped_long_ago", "2026-01-01T00:00:00+00:00"],
        )
        .unwrap();
        assert_eq!(
            renumbered_migrations(&conn),
            Vec::new(),
            "a version the code no longer registers has nothing to disagree with"
        );
    }

    #[test]
    fn migrations_apply_to_fresh_db_and_are_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_all(&mut conn).unwrap();
        // Applying again is a no-op, not an error.
        apply_all(&mut conn).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM _amux_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n as usize, super::MIGRATIONS.len());
        // The revision row exists and starts at 0.
        let rev: u64 = conn
            .query_row("SELECT rev FROM _amux_rev WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rev, 0);
    }

    /// The 2026-08-30 incident, reproduced directly: a version number this
    /// binary expects to mean one thing is already recorded in the DB under
    /// a completely different migration's name (an unrelated branch's
    /// migration having consumed the same number first). It used to be
    /// skipped forever; now `apply_all` runs the registered migration over it,
    /// keeps the colliding row's name as evidence, and every OTHER migration
    /// still applies normally.
    #[test]
    fn a_version_recorded_under_a_different_name_is_applied_not_skipped_or_fatal() {
        // Collide a NON-foundational migration (a one-column ALTER TABLE on a
        // table that stays, found by name rather than a hardcoded number so
        // this survives a future renumbering).
        let target = super::MIGRATIONS
            .iter()
            .find(|m| m.name.contains("schedule_critical"))
            .expect("the migration this test collides must still exist")
            .version;

        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE _amux_migrations (
                version INTEGER PRIMARY KEY, name TEXT NOT NULL,
                applied_at TEXT NOT NULL, duration_ms INTEGER
             );
             INSERT INTO _amux_migrations (version, name, applied_at, duration_ms)
             VALUES ({target}, 'some_other_branchs_migration', '2026-08-29T00:00:00Z', 1);"
        ))
        .unwrap();

        apply_all(&mut conn).unwrap();

        let (name, reconciled): (String, Option<String>) = conn
            .query_row(
                "SELECT name, reconciled_name FROM _amux_migrations WHERE version = ?1",
                [target],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(name, "some_other_branchs_migration", "the recorded name is kept as evidence");
        assert_eq!(reconciled.as_deref(), Some("0173_schedule_critical"));

        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM _amux_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n as usize, super::MIGRATIONS.len(), "every migration has a row");

        let has_col: bool = conn
            .query_row("SELECT COUNT(*) FROM pragma_table_info('schedules') WHERE name = 'critical'", [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|n| n > 0)
            .unwrap();
        assert!(has_col, "the colliding migration's SQL must have run");
        // And not twice: a second boot over the plain ALTER would fail with
        // "duplicate column" and log apply_failed.
        let ((), logs) = with_captured_logs(|| apply_all(&mut conn).unwrap());
        assert!(!logs.contains("migration_name_mismatch"), "{logs}");
    }

    /// 0178 folds Telegram's old link table into bridge_links and drops it.
    /// Seeded on the schema 0177 leaves: one chat the bridge already linked
    /// (its answers had moved to a mentioned worker), one only the old table
    /// knew (a supergroup whose last message went to a mentioned worker), and
    /// a link tagged with the old table's name.
    #[test]
    fn telegram_links_move_into_bridge_links_and_the_old_table_is_dropped() {
        let conn = Connection::open_in_memory().unwrap();
        apply_history_through(&conn, 177);
        let old = concat!("telegram", "_mappings");
        conn.execute_batch(&format!(
            "INSERT INTO conversation_posts (id, conv, author_key, author_name, body, ts) VALUES (40, 'w:x', 'k', 'n', 'b', 0);
             INSERT INTO {old} (chat_id, session, linked_at, chat_type, last_routed_session)
               VALUES (111, 'amux', '2026-11-01 00:00:00', 'private', 'other'),
                      (-222, 'ops', '2026-11-02 00:00:00', 'supergroup', 'sales');
             INSERT INTO bridge_links (platform, conversation, kind, conv, last_post_id, linked_by, linked_at)
               VALUES ('telegram', '111', 'dm', 'w:other', 7, '{old}', 5);"
        ))
        .unwrap();
        let m = MIGRATIONS.iter().find(|m| m.version == 178).unwrap();
        assert_eq!(m.name, "0178_telegram_links_in_bridge");
        apply_one(&conn, m.sql).unwrap();

        type Row = (String, String, String, Option<String>, i64, String, i64);
        let rows: Vec<Row> = conn
            .prepare("SELECT conversation, kind, conv, default_conv, last_post_id, linked_by, linked_at FROM bridge_links WHERE platform='telegram' ORDER BY conversation")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            rows,
            vec![
                ("-222".into(), "group".into(), "w:sales".into(), Some("w:ops".into()), 40, "telegram_poll".into(), 1_793_577_600_000),
                ("111".into(), "dm".into(), "w:other".into(), Some("w:amux".into()), 7, "telegram_poll".into(), 5),
            ]
        );
        let gone: i64 = conn
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = ?1", [old], |r| r.get(0))
            .unwrap();
        assert_eq!(gone, 0, "the old table is dropped");
    }

    /// AMUX-3609's backfill, driven through the SHIPPED migration body rather
    /// than a paraphrase of its SQL.
    ///
    /// The backfill can only recover what the reaper left: `_amux_state_events`
    /// is swept at 14 days, so ~10,700 of the board's 10,905 rows will end up
    /// NULL forever. That is the point of the negative cases below — a backfill
    /// that guessed a date for them would be worse than the NULL, and a test
    /// with only the positive case would pass against one that did.
    #[test]
    fn closed_at_backfill_recovers_only_what_the_journal_still_holds() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_all(&mut conn).unwrap();

        conn.execute_batch(
            "INSERT INTO issues (id,title,desc,status,creator,created,updated) VALUES
               ('C-CLOSED','a','', 'done',     'x',1,1),
               ('C-REOPEN','b','', 'doing',    'x',1,1),
               ('C-REAPED','c','', 'verified', 'x',1,1),
               ('C-TWICE' ,'d','', 'done',     'x',1,1);
             INSERT INTO _amux_state_events (rev,entity_type,entity_id,mutation,at,payload) VALUES
               (1,'task','C-CLOSED','{\"kind\":\"status_changed\",\"from\":\"doing\",\"to\":\"done\"}','2026-08-20T10:00:00.123456+00:00',NULL),
               -- reopened: its journal says it closed, but it is OPEN now, so
               -- the backfill must skip it or `closed_at IS NOT NULL` would
               -- start meaning \"was closed once\".
               (2,'task','C-REOPEN','{\"kind\":\"status_changed\",\"from\":\"doing\",\"to\":\"done\"}','2026-08-20T11:00:00+00:00',NULL),
               -- a NON-terminal transition must not be mistaken for a close.
               (3,'task','C-TWICE','{\"kind\":\"status_changed\",\"from\":\"todo\",\"to\":\"doing\"}','2026-08-19T09:00:00+00:00',NULL),
               (4,'task','C-TWICE','{\"kind\":\"status_changed\",\"from\":\"doing\",\"to\":\"done\"}','2026-08-20T09:00:00+00:00',NULL),
               (5,'task','C-TWICE','{\"kind\":\"status_changed\",\"from\":\"done\",\"to\":\"doing\"}','2026-08-21T09:00:00+00:00',NULL),
               (6,'task','C-TWICE','{\"kind\":\"status_changed\",\"from\":\"doing\",\"to\":\"done\"}','2026-08-22T09:00:00+00:00',NULL);",
        )
        .unwrap();

        // Re-run the shipped body. ADDCOL is idempotent; the UPDATE is the part
        // under test. Reading it from the same include_str the registry uses
        // means this cannot drift from what production applies.
        apply_one(
            &conn,
            super::MIGRATIONS
                .iter()
                .find(|m| m.version == 31)
                .unwrap()
                .sql,
        )
        .unwrap();

        let at = |id: &str| -> Option<i64> {
            conn.query_row("SELECT closed_at FROM issues WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
        };

        // 1787220000 = 2026-08-20T10:00:00Z. The microseconds and the +00:00
        // offset are parsed by strftime, which is the assumption the migration
        // rests on — asserted here rather than trusted.
        assert_eq!(
            at("C-CLOSED"),
            Some(1787220000),
            "a closed card recovers its real close time"
        );
        assert_eq!(
            at("C-REOPEN"),
            None,
            "a card that is open NOW must not be backfilled from an old close"
        );
        assert_eq!(
            at("C-REAPED"),
            None,
            "no journal row survives for it: NULL means NOT RECORDED, never a guess"
        );
        assert_eq!(
            at("C-TWICE"),
            Some(1787389200),
            "2026-08-22T09:00:00Z — the LATEST close, matching the live write rule; MIN would make backfilled rows disagree with every row written after this migration"
        );

        // Idempotent: the migration is guarded by `closed_at IS NULL`, so a
        // second run must not disturb what the first wrote.
        apply_one(
            &conn,
            super::MIGRATIONS
                .iter()
                .find(|m| m.version == 31)
                .unwrap()
                .sql,
        )
        .unwrap();
        assert_eq!(at("C-CLOSED"), Some(1787220000));
        assert_eq!(at("C-TWICE"), Some(1787389200));
    }

    /// A migration must be able to ADD a column and then USE it in the same
    /// file. Before AMUX-3609 the runner executed plain SQL first and the
    /// ADDCOLs after, so a backfill died on `no such column` — with nothing in
    /// the mechanism hinting at the ordering. Nobody hit it because every
    /// ADDCOL migration until then only added.
    /// The timing instrument itself must be able to fail. `apply_all` logged
    /// nothing until 0031 held the database for 186 seconds and the only visible
    /// symptom was /health not answering; an instrument added in response to
    /// that, and never checked, would be the same defect with more code.
    #[test]
    fn every_migration_records_how_long_it_held_the_database() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_all(&mut conn).unwrap();
        let missing: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _amux_migrations WHERE duration_ms IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            missing, 0,
            "on a fresh database every migration must record its duration, or the next \
             outage is a forensic exercise again"
        );
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM _amux_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            n as usize,
            super::MIGRATIONS.len(),
            "and every migration must be recorded at all"
        );
    }

    #[test]
    fn addcol_can_be_used_by_the_same_migration() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, n INTEGER);")
            .unwrap();
        conn.execute("INSERT INTO t VALUES ('a', 7)", []).unwrap();
        apply_one(
            &conn,
            "-- ADDCOL: t doubled INTEGER\nUPDATE t SET doubled = n * 2;",
        )
        .expect("a migration that adds a column and populates it must apply");
        let got: i64 = conn
            .query_row("SELECT doubled FROM t WHERE id='a'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(got, 14, "the plain SQL must run AFTER the column exists");
    }

    #[test]
    fn addcol_directive_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (a INTEGER);").unwrap();
        let sql = "-- ADDCOL: t b INTEGER NOT NULL DEFAULT 0\n";
        apply_one(&conn, sql).unwrap();
        apply_one(&conn, sql).unwrap(); // second run: column exists, skipped
        assert!(column_exists(&conn, "t", "b").unwrap());
    }

    #[test]
    fn team_migration_preserves_every_legacy_scope_without_widening_access() {
        let conn = Connection::open_in_memory().unwrap();
        for migration in MIGRATIONS
            .iter()
            .filter(|migration| migration.version <= 59)
        {
            apply_one(&conn, migration.sql).unwrap();
        }
        conn.execute(
            "INSERT INTO org_members (id,email,role,joined_at,scope_level,scope_name) \
             VALUES ('member-group','g@example.com','member',1,'group','research'), \
                    ('member-worker','w@example.com','member',2,'worker','tubescience'), \
                    ('member-global','a@example.com','member',3,'global','')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO org_invites (token,created_at,expires_at,scope_level,scope_name) \
             VALUES ('legacy-group',1,9999999999,'group','research')",
            [],
        )
        .unwrap();

        let migration = MIGRATIONS
            .iter()
            .find(|migration| migration.version == 60)
            .unwrap();
        apply_one(&conn, migration.sql).unwrap();

        let rows: Vec<(String, String, String)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT m.id,t.scope_level,t.scope_name FROM org_members m \
                     JOIN org_teams t ON t.id=m.team_id ORDER BY m.id",
                )
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(
            rows,
            vec![
                ("member-global".into(), "global".into(), "".into()),
                ("member-group".into(), "group".into(), "research".into()),
                (
                    "member-worker".into(),
                    "worker".into(),
                    "tubescience".into()
                ),
            ]
        );
        let invite_scope: (String, String) = conn
            .query_row(
                "SELECT t.scope_level,t.scope_name FROM org_invites i JOIN org_teams t ON t.id=i.team_id WHERE i.token='legacy-group'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(invite_scope, ("group".into(), "research".into()));
    }
}

#[cfg(test)]
mod guard_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// SUPERSEDES `the_auto_build_target_dir_is_not_mistaken_for_a_cargo_run`,
    /// which asserted the exact opposite and was WRONG (AMUX-2799).
    ///
    /// That test required `~/.amux/rust-build-target/release/` NOT to match, on
    /// the stated grounds that "it INSTALLS, and blocking it would stop the real
    /// server booting". The premise is false: `scripts/rust-auto-build.sh` runs
    /// `install -m 0755 .../release/amux-server ~/.local/bin/amux-server-rs`, so
    /// the artifact under the build dir is never the thing that boots. Nothing
    /// executes it in place.
    ///
    /// What the assertion actually bought was a guard that was OFF for the build
    /// dir CLAUDE.md mandates and ON for the per-session dirs it bans — so an
    /// ordinary `cargo run -p amux-server` under the sanctioned environment
    /// could apply an uncommitted migration to the live DB, which is AMUX-2652
    /// verbatim. A test can pin a defect as firmly as it pins a fix.
    #[test]
    fn any_profile_dir_reads_as_a_cargo_build_whatever_the_target_dir_is() {
        // A per-session checkout dir (banned, but must still be caught).
        assert!(is_cargo_target_build(Path::new(
            "/Users/x/Dev/amux/target/debug/amux-server"
        )));
        assert!(is_cargo_target_build(Path::new(
            "/Users/x/Dev/amux/target/release/amux-server"
        )));
        // THE MANDATED SHARED DIR — the case that was unguarded. A `cargo run`
        // here is exactly the working-tree-against-live-DB hazard.
        assert!(
            is_cargo_target_build(Path::new(
                "/Users/x/.amux/rust-build-target/debug/amux-server"
            )),
            "the build dir every session is told to use must be guarded, not exempt"
        );
        assert!(is_cargo_target_build(Path::new(
            "/Users/x/.amux/rust-build-target/release/amux-server"
        )));
        // Any OTHER target dir, because the next move must not need a code change.
        assert!(is_cargo_target_build(Path::new(
            "/tmp/whatever-42/debug/deps/amux_server-abc"
        )));

        // The two paths that actually boot a server, both of which must stay
        // exempt or the fleet cannot start with a pending migration.
        assert!(!is_cargo_target_build(Path::new(
            "/Users/x/.local/bin/amux-server-rs"
        )));
        assert!(
            !is_cargo_target_build(Path::new("/usr/local/bin/amux-server-rs")),
            "the cloud image's path — CMD [\"amux-server-rs\"] off /usr/local/bin"
        );
    }

    /// THE ASSERTION WHOSE REMOVAL HID THIS. The predicate's only real input is
    /// `current_exe()`, and the previous fix for a red test here was to stop
    /// consulting it and inject a hand-built path instead — which made the test
    /// pass under every target dir by testing nothing about any of them.
    ///
    /// This is stable, not fragile: a test binary IS a cargo artifact by
    /// construction, under any `CARGO_TARGET_DIR`, so the assertion holds
    /// wherever it is built — while still failing the moment the predicate stops
    /// recognising the real environment.
    #[test]
    fn this_very_test_binary_is_recognised_as_a_cargo_build() {
        let exe = std::env::current_exe().expect("current_exe must be readable");
        assert!(
            is_cargo_target_build(&exe),
            "a test binary must read as a cargo build, or the guard is off for whoever \
             is running the suite from this target dir: {}",
            exe.display()
        );
    }

    #[test]
    fn only_the_real_home_db_counts_as_live() {
        let home = PathBuf::from("/Users/x");
        assert!(is_live_db(Path::new("/Users/x/.amux/amux.db"), Some(&home)));
        assert!(!is_live_db(Path::new("/tmp/scratch/amux.db"), Some(&home)));
        assert!(!is_live_db(
            Path::new("/Users/x/.amux/other.db"),
            Some(&home)
        ));
        // No HOME at all: cannot prove it is live, so do not block.
        assert!(!is_live_db(Path::new("/Users/x/.amux/amux.db"), None));
    }

    // The guard must be silent in every legitimate case, or it becomes an
    // outage. A refusal that fires on the installed server is strictly worse
    // than the hazard it prevents.
    #[test]
    fn nothing_pending_never_refuses_even_on_the_live_db() {
        assert!(guard_live_db(Path::new("/Users/x/.amux/amux.db"), &[]).is_ok());
    }

    #[test]
    fn a_scratch_db_is_never_refused_however_it_was_built() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("amux.db");
        assert!(guard_live_db(&db, &["0099_experiment"]).is_ok());
    }

    #[test]
    fn truthy_env_treats_the_usual_falsey_spellings_as_off() {
        for (v, want) in [
            ("1", true),
            ("true", true),
            ("YES", true),
            ("0", false),
            ("false", false),
            ("no", false),
            ("", false),
            ("  ", false),
        ] {
            std::env::set_var("AMUX_TEST_TRUTHY", v);
            assert_eq!(truthy_env("AMUX_TEST_TRUTHY"), want, "value {v:?}");
        }
        std::env::remove_var("AMUX_TEST_TRUTHY");
        assert!(!truthy_env("AMUX_TEST_TRUTHY"));
    }

    // THE POSITIVE CASE. Everything above proves the guard stays QUIET; a guard
    // that only ever returns Ok would pass all of them. This is the one that
    // proves it can fire.
    //
    // Safe to run against the real live path because guard_live_db only
    // INSPECTS paths — it never opens, reads, or migrates the database. And the
    // test binary is itself a cargo target artifact (target/debug/deps/...),
    // which is exactly the build shape the hazard describes, so this exercises
    // the real predicate rather than a stubbed one.
    #[test]
    fn it_actually_refuses_a_pending_migration_against_the_live_db() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            panic!(
                "HOME unset — this test cannot express its precondition, \
                    which is a broken test, not a passing one"
            );
        };
        // A cargo-target-shaped path, supplied rather than inherited: the real
        // hazard is a binary built from a working tree, and that shape is a
        // property of the PATH, not of wherever this test binary was written.
        // (Asserting on `current_exe()` made the test fail under
        // CARGO_TARGET_DIR=~/.amux/rust-build-target — the shared build dir the
        // workflow now standardises on — while the guard itself was correct.)
        let exe = PathBuf::from("/Users/someone/Dev/amux/target/debug/deps/amux_server-abc123");
        assert!(
            is_cargo_target_build(&exe),
            "the fixture must be the shape the guard looks for"
        );

        let live = home.join(".amux").join("amux.db");
        std::env::remove_var("AMUX_ALLOW_LIVE_DB");
        let err = guard_live_db_with_exe(&live, &["0099_pretend_uncommitted"], &exe)
            .expect_err("the guard MUST refuse a pending migration against the live DB");
        let msg = err.to_string();
        assert!(msg.contains("LIVE database"), "{msg}");
        assert!(msg.contains("0099_pretend_uncommitted"), "{msg}");
        // The refusal has to publish its own escape, or the next agent
        // hand-rolls something worse to get past it (the AMUX-2325 lesson).
        assert!(msg.contains("AMUX_ALLOW_LIVE_DB=1"), "{msg}");
        assert!(msg.contains("AMUX_HOME"), "{msg}");

        // The sanctioned escape must actually work, or the message above is a
        // lie and the guard is unwalkable (the AMUX-2325 shape: a constraint
        // whose documented exit does not exist gets routed around by something
        // worse). Same test, so nothing can race on this env var.
        // Injected exe here too. With `current_exe()` this assertion passed
        // VACUOUSLY under a target dir outside the checkout: the guard returns
        // Ok at the is_cargo_target_build check, before it ever reads the env
        // var, so it would have gone green with the escape hatch deleted.
        std::env::set_var("AMUX_ALLOW_LIVE_DB", "1");
        let permitted = guard_live_db_with_exe(&live, &["0099_pretend_uncommitted"], &exe);
        std::env::remove_var("AMUX_ALLOW_LIVE_DB");
        assert!(
            permitted.is_ok(),
            "AMUX_ALLOW_LIVE_DB=1 must permit it: {permitted:?}"
        );
    }

    // NOTE: the refuse and permit cases are ONE test on purpose. As two, they
    // raced on AMUX_ALLOW_LIVE_DB — process-global env, threaded harness — and
    // the escape case failed roughly at random. That is precisely the flake
    // class AMUX-2675 was about; merging removes it by construction instead of
    // adding another lock that the next test must remember to take.
}

/// AF-193 / AMUX-3609: a migration's COST, which the rest of the suite cannot express.
///
/// 0031 backfilled `issues.closed_at` with a correlated subquery over
/// `_amux_state_events`, which carried exactly one index, on `rev`. It
/// full-scanned ~79,000 rows for each of 7,281 terminal cards, inside the
/// exclusive transaction a migration runs in, at server startup, and the server
/// was unreachable for 186 seconds. Every test was green throughout, because
/// migration tests apply their SQL to four fixture rows — and on four rows an
/// index scan and a table scan are indistinguishable. Correctness and cost are
/// different questions and the suite only ever answered the first.
///
/// WHY THE PLAN AND NOT A ROW COUNT. The obvious fix is a fixture with realistic
/// row counts, and it is the wrong one: it needs a threshold, the threshold has
/// to be guessed, and a migration that is 10x too slow on 79,000 rows still
/// passes on whatever number gets picked. Ethos rule 7 — prefer the
/// structurally-absent signal over the tuned parameter. `EXPLAIN QUERY PLAN`
/// answers the actual question at any table size, in microseconds, on an empty
/// database.
///
/// THE RULE: inside a CORRELATED subquery, every table access must use an index.
/// A correlated subquery runs once per outer row, so an unindexed access there is
/// O(outer x inner) by construction — that is the shape that took the server
/// down, and it is a property of the SQL, not of how much data happens to exist.
/// A `SCAN` at the top level is left alone: one pass over the table being
/// updated is unavoidable and is O(n) once.
///
/// MEASURED, not assumed, because the obvious matcher does not work. The plan
/// prints the ALIAS, not the table name, so a check that greps for
/// `_amux_state_events` finds nothing and passes on the incident itself:
///
///   without the index:  `--SEARCH e
///   with the index:     `--SEARCH e USING INDEX idx_amux_state_events_entity
///
/// Note also that it is SEARCH in both cases. A `SCAN` vs `SEARCH` check — the
/// other obvious one — is green on the specimen too. The discriminator is the
/// presence of USING INDEX, and nothing else in that output separates the two.
#[cfg(test)]
mod cost_tests {
    use super::*;

    /// An EXPLAIN QUERY PLAN row: (id, parent, detail).
    ///
    /// `Err` means "not explainable here", which is NOT the same as "costs
    /// nothing" and must never quietly become a pass. Trigger bodies split out
    /// of a CREATE TRIGGER reference `new.*` and cannot prepare standalone, so
    /// this cannot hard-fail — instead `explained` below counts what actually
    /// got checked, and a named statement pins that the check reached the one
    /// that caused the outage.
    fn plan(conn: &Connection, stmt: &str) -> Option<Vec<(i64, i64, String)>> {
        let mut s = conn.prepare(&format!("EXPLAIN QUERY PLAN {stmt}")).ok()?;
        let rows = s
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .ok()?
            .flatten()
            .collect::<Vec<_>>();
        Some(rows)
    }

    /// Table accesses that run inside a CORRELATED subquery without an index.
    fn unindexed_correlated_access(rows: &[(i64, i64, String)]) -> Vec<String> {
        let parent_of = |id: i64| rows.iter().find(|r| r.0 == id).map(|r| r.1);
        let detail_of = |id: i64| rows.iter().find(|r| r.0 == id).map(|r| r.2.clone());
        let mut bad = Vec::new();
        for (_, parent, d) in rows {
            let is_access = d.starts_with("SCAN ") || d.starts_with("SEARCH ");
            let indexed = d.contains("USING INDEX")
                || d.contains("USING COVERING INDEX")
                || d.contains("USING INTEGER PRIMARY KEY")
                || d.contains("USING ROWID SEARCH");
            if !is_access || indexed {
                continue;
            }
            let mut cur = *parent;
            let mut hops = 0;
            while cur != 0 && hops < 32 {
                let Some(p) = detail_of(cur) else { break };
                if p.contains("CORRELATED") {
                    bad.push(d.clone());
                    break;
                }
                cur = parent_of(cur).unwrap_or(0);
                hops += 1;
            }
        }
        bad
    }

    /// Statements a migration body executes, minus comments and DDL.
    fn dml(sql: &str) -> Vec<String> {
        let stripped: String = sql
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        stripped
            .split(';')
            .map(|s| s.trim().to_string())
            .filter(|s| {
                let u = s.to_uppercase();
                u.starts_with("UPDATE ")
                    || u.starts_with("DELETE ")
                    || u.starts_with("INSERT ")
                    || u.starts_with("SELECT ")
                    || u.starts_with("WITH ")
                    || u.starts_with("REPLACE ")
            })
            .collect()
    }

    fn squash(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Indexes created after a DML statement that actually READS their table
    /// through their leading column. The statement's WRITE table is excluded:
    /// building that index after the backfill is deliberate and avoids paying
    /// index maintenance on every changed row.
    fn late_read_side_indexes(name: &str, sql: &str) -> Vec<String> {
        fn identifiers(s: &str) -> Vec<String> {
            s.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .filter(|t| !t.is_empty())
                .map(str::to_uppercase)
                .collect()
        }
        fn indexed_table_and_leading_column(stmt_upper: &str) -> Option<(String, String)> {
            let on = stmt_upper.find(" ON ")? + 4;
            let rest = stmt_upper[on..].trim_start();
            let open = rest.find('(')?;
            let table = rest[..open].trim().trim_matches('"').to_string();
            let columns = &rest[open + 1..];
            let leading = columns
                .split([',', ')'])
                .next()?
                .split_whitespace()
                .next()?
                .trim_matches('"')
                .to_string();
            (!table.is_empty() && !leading.is_empty()).then_some((table, leading))
        }
        fn written_table(stmt_upper: &str) -> Option<String> {
            let rest = stmt_upper.strip_prefix("UPDATE ")?;
            let end = rest.find([' ', '\n']).unwrap_or(rest.len());
            Some(rest[..end].trim().trim_matches('"').to_string())
        }
        fn reads_table(stmt_upper: &str, table: &str) -> bool {
            let ids = identifiers(stmt_upper);
            ids.windows(2)
                .any(|w| matches!(w[0].as_str(), "FROM" | "JOIN") && w[1].as_str() == table)
        }
        fn mentions_column(stmt_upper: &str, column: &str) -> bool {
            identifiers(stmt_upper).iter().any(|t| t == column)
        }

        let body: String = sql
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        // (offset, table written, complete statement) for each DML seen so far.
        let mut dml_seen: Vec<(usize, Option<String>, String)> = Vec::new();
        let mut offset = 0usize;
        let mut bad = Vec::new();
        for raw in body.split(';') {
            let t = raw.trim().to_uppercase();
            if t.starts_with("UPDATE ") || t.starts_with("DELETE ") || t.starts_with("WITH ") {
                dml_seen.push((offset, written_table(&t), t.clone()));
            }
            if t.starts_with("CREATE INDEX") || t.starts_with("CREATE UNIQUE INDEX") {
                if let Some((idx_table, leading_column)) = indexed_table_and_leading_column(&t) {
                    for (d_off, written, dml) in &dml_seen {
                        if offset > *d_off
                            && written.as_deref() != Some(idx_table.as_str())
                            && reads_table(dml, &idx_table)
                            && mentions_column(dml, &leading_column)
                        {
                            bad.push(format!(
                                "{name}: CREATE INDEX on {idx_table}({leading_column}, ...) appears AFTER a statement that reads it through that column — same final schema, same outage"
                            ));
                        }
                    }
                }
            }
            offset += raw.len() + 1;
        }
        bad
    }

    /// Correlated scans that are bounded by construction, as (migration, plan
    /// detail, why). Each entry must still match a plan, or the test fails, so
    /// a stale exemption cannot hide a new offender under its name.
    ///
    /// 0060: `org_teams` is created empty by 0060 itself and then holds one
    /// row per distinct legacy scope grant, so each member row scans tens of
    /// teams at most. SQLite 3.46 reported this as `SCAN t USING INDEX` (the
    /// primary key's autoindex, chosen for `ORDER BY id`), which this check
    /// counted as indexed though it read the whole table too; 3.53 and later
    /// report a plain `SCAN t`. Migrations are append-only, so 0060 stays.
    const BOUNDED_CORRELATED_SCANS: &[(&str, &str, &str)] = &[(
        "0060_org_teams",
        "SCAN t",
        "org_teams is created by 0060 and holds one row per distinct legacy grant",
    )];

    /// Every migration, applied in order, with each one's DML plan-checked
    /// against the schema THAT MIGRATION LEAVES BEHIND — previous migrations
    /// plus its own DDL, and nothing from later ones.
    ///
    /// Checking against the FINAL schema would be vacuous for exactly the
    /// incident: 0032 adds the index 0031 needed, so a final-schema check reads
    /// 0031 as fine no matter what 0031 does.
    #[test]
    fn no_migration_runs_an_unindexed_correlated_subquery() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS _amux_migrations (
                version INTEGER PRIMARY KEY, name TEXT NOT NULL,
                applied_at TEXT NOT NULL, duration_ms INTEGER);",
        )
        .unwrap();

        let mut offenders: Vec<String> = Vec::new();
        let mut explained = 0usize;
        let mut saw_the_0031_backfill = false;
        let mut exemptions_used = vec![false; BOUNDED_CORRELATED_SCANS.len()];

        for m in MIGRATIONS {
            let tx = conn.transaction().unwrap();
            apply_one(&tx, m.sql).unwrap_or_else(|e| panic!("{} failed to apply: {e}", m.name));
            tx.execute(
                "INSERT INTO _amux_migrations (version, name, applied_at) VALUES (?1, ?2, 'test')",
                rusqlite::params![m.version, m.name],
            )
            .unwrap();
            tx.commit().unwrap();

            for stmt in dml(m.sql) {
                let Some(rows) = plan(&conn, &stmt) else {
                    continue;
                };
                explained += 1;
                let sq = squash(&stmt);
                if sq.starts_with("UPDATE issues SET closed_at") {
                    saw_the_0031_backfill = true;
                }
                for bad in unindexed_correlated_access(&rows) {
                    if let Some(i) = BOUNDED_CORRELATED_SCANS.iter().position(|(n, d, _)| *n == m.name && *d == bad) {
                        exemptions_used[i] = true;
                        continue;
                    }
                    offenders.push(format!(
                        "{}: `{}` runs once per outer row with no index\n    statement: {}",
                        m.name,
                        bad,
                        sq.chars().take(200).collect::<String>()
                    ));
                }
            }
        }

        // VACUITY GUARDS. This check silently passed on the real incident during
        // development because every statement failed to prepare and the helper
        // returned "nothing to see". A count alone is weak, so the statement
        // that caused the outage is pinned BY NAME: if 0031's backfill ever
        // stops being reached, this fails instead of going quietly green.
        assert!(
            explained >= 5,
            "only {explained} statements were explainable — the check is not reaching the SQL \
             it claims to check"
        );
        assert!(
            saw_the_0031_backfill,
            "0031's `UPDATE issues SET closed_at ...` was never explained. That is the statement \
             that held the database for 186 seconds, and this check exists to see it."
        );

        for (used, (name, detail, why)) in exemptions_used.iter().zip(BOUNDED_CORRELATED_SCANS) {
            assert!(*used, "exemption ({name}, `{detail}`: {why}) matched no plan; remove it rather than keep a name that hides nothing");
        }
        assert!(
            offenders.is_empty(),
            "a migration would full-scan a table once per outer row, which is how 0031 held \
             the database for 186 seconds at startup. Create the index BEFORE the backfill, \
             in the same migration.\n\n{}",
            offenders.join("\n")
        );
    }

    /// The ordering variant the plan check cannot see.
    ///
    /// `apply_one` hands the plain SQL to `execute_batch`, so statements run in
    /// FILE ORDER. An index created after the backfill that needs it produces
    /// exactly the same final schema and exactly the same 186 seconds. The plan
    /// check above is blind to it — it explains against the finished migration,
    /// by which time the index exists either way.
    ///
    /// ONLY THE READ SIDE. An index on the table the DML WRITES is a different
    /// thing and belongs after: building `issues(closed_at)` once the backfill
    /// has populated it is correct practice, and cheaper than maintaining the
    /// index through the update. 0031 does exactly that, so a rule of "no
    /// CREATE INDEX after any DML" fires on the CORRECT file — measured, it did.
    /// What must come first is an index on a table the statement READS, which is
    /// the one that turns into a scan per outer row.
    #[test]
    fn a_migration_creates_its_read_side_indexes_before_the_dml_that_needs_them() {
        let mut bad = Vec::new();
        for m in MIGRATIONS {
            bad.extend(late_read_side_indexes(m.name, m.sql));
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    /// The ordering check used to equate "another DML appeared earlier" with
    /// "that DML read this index's table". 0060 alternates writes to
    /// org_members and org_invites, then correctly builds each WRITE-side
    /// team_id index after the backfill; the old heuristic called each sibling
    /// UPDATE a read of the other table and failed four times. Preserve both
    /// directions: sibling writes stay quiet and the actual 0031 shape fires.
    #[test]
    fn read_side_index_ordering_distinguishes_a_read_from_a_sibling_write() {
        let siblings = "
            UPDATE org_members SET team_id='x' WHERE team_id IS NULL;
            UPDATE org_invites SET team_id='x' WHERE team_id IS NULL;
            CREATE INDEX idx_members_team ON org_members(team_id);
            CREATE INDEX idx_invites_team ON org_invites(team_id);";
        assert!(
            late_read_side_indexes("siblings", siblings).is_empty(),
            "an UPDATE of one table is not a read of its sibling"
        );

        let incident = "
            UPDATE issues SET closed_at=(
                SELECT MAX(at) FROM _amux_state_events e
                WHERE e.entity_type='task' AND e.entity_id=issues.id);
            CREATE INDEX idx_events_entity
                ON _amux_state_events(entity_type, entity_id);";
        let bad = late_read_side_indexes("0031-planted", incident);
        assert_eq!(
            bad.len(),
            1,
            "the real late read-side index must still fail: {bad:?}"
        );
        assert!(bad[0].contains("_AMUX_STATE_EVENTS(ENTITY_TYPE"), "{bad:?}");
    }

    /// NEGATIVE CONTROL, built from the incident rather than from a convenient
    /// shape: 0031's own UPDATE, against the schema it actually met on
    /// 2026-08-24 — `_amux_state_events` with its single index on `rev`.
    ///
    /// 88af1ff3 edited 0031 to create the index first, so the checks above now
    /// pass on a fresh database, and a check that passes has told you nothing
    /// until you have seen it fail on the thing it was written for.
    #[test]
    fn the_real_0031_specimen_without_its_index_is_caught() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE _amux_state_events (rev INTEGER, entity_type TEXT, entity_id TEXT,
                                              mutation TEXT, at TEXT);
             CREATE INDEX idx_rev ON _amux_state_events(rev);
             CREATE TABLE issues (id TEXT, status TEXT, closed_at INTEGER);",
        )
        .unwrap();

        let backfill = "UPDATE issues SET closed_at = (
                SELECT CAST(MAX(strftime('%s', e.at)) AS INTEGER)
                  FROM _amux_state_events e
                 WHERE e.entity_type = 'task' AND e.entity_id = issues.id)
             WHERE status IN ('done','verified','discarded') AND closed_at IS NULL";

        let before = unindexed_correlated_access(&plan(&conn, backfill).expect("explainable"));
        assert!(
            !before.is_empty(),
            "the check did not fire on the statement that caused the outage — it cannot fail, \
             so its green tells you nothing"
        );

        conn.execute_batch(
            "CREATE INDEX idx_amux_state_events_entity
                 ON _amux_state_events(entity_type, entity_id);",
        )
        .unwrap();
        let after = unindexed_correlated_access(&plan(&conn, backfill).expect("explainable"));
        assert!(
            after.is_empty(),
            "the index that fixed the incident does not clear the check: {after:?}"
        );
    }
}
