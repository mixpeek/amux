//! A held rollout must not prevent producing an independently testable local candidate.
//! The original task, authorization and project acceptance remain unchanged.
use super::{outputs, planner, store};
use crate::db::{board_store as bs, PendingEvent, WriteOutcome};
use amux_core::revision::{EntityType, MutationKind};
use rusqlite::{params, Connection};

const SOURCE: &str = "project-authorization-preparation:";

/// One preparatory outcome per held code task, including across restarts and
/// explicit archive/delete. Never recursively prepare a preparation task.
pub(crate) fn reconcile(conn: &Connection, name: &str) -> anyhow::Result<WriteOutcome> {
    reconcile_in_home(conn, name, &crate::config::amux_home())
}

fn reconcile_in_home(
    conn: &Connection,
    name: &str,
    home: &std::path::Path,
) -> anyhow::Result<WriteOutcome> {
    let mut result = WriteOutcome {
        applied: false,
        events: vec![],
    };
    let Some(project) = store::get(conn, name)? else {
        return Ok(result);
    };
    if !project.policy.enabled || project.policy.paused {
        return Ok(result);
    }
    for row in bs::project_issues(conn, name)? {
        if row.archived != 0
            || row.item_type != "code"
            || bs::is_terminal_status(&row.status)
            || row.source.as_deref().is_some_and(|s| s.starts_with(SOURCE))
        {
            continue;
        }
        let execution = planner::execution(conn, &row.id)?;
        if execution.stage != "waiting"
            || execution.suspended
            || execution.report.is_some()
            || !outputs::authorization_hold(conn, &row)?
        {
            continue;
        }
        let source = format!("{SOURCE}{}", row.id);
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM issues WHERE project_group=?1 AND source=?2)",
            params![name, source],
            |r| r.get(0),
        )?;
        if exists {
            continue;
        }
        let env = crate::config::parse_env_file(
            &home
                .join("sessions")
                .join(format!("{}.env", execution.worker)),
        );
        if ["CC_PAUSED", "CC_ARCHIVED", "CC_ISOLATED"]
            .iter()
            .any(|key| env.get(*key).map(String::as_str) == Some("1"))
        {
            continue;
        }
        // No usage-ledger scan on ordinary scheduler ticks or an already
        // prepared hold. Check admission once when there is actual new work.
        if !result.applied {
            let pending: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM cmd_history WHERE project_group=?1 AND capture_pending!=0)",
                [name], |r| r.get(0),
            )?;
            if pending || super::usage::waiting(conn, &project)?.is_some() {
                return Ok(result);
            }
        }
        let description = format!(
            "Prepare the independently testable LOCAL implementation for {}: {}. The original task remains held and owns its original criteria. Do not execute the gated action or claim the original task or project complete. Reuse checked same-project candidates. Implement and commit whatever source, local fixture tests, dry-run tooling and rollback procedure can be completed without increased spend, production access/mutation or customer outbound. Preserve runtime checks and real local API readbacks where applicable; a local fixture does not prove production parity. Retain a human-readable report distinguishing tested local behavior from the exact remaining approval and production evidence. If no independently useful safe implementation exists, retain that concrete finding; do not manufacture code or a passing runtime result.\nOriginal description: {}\nOriginal requirements (context, NOT authorization): {}\nOriginal next action: {}\nHeld action: {}",
            row.id, row.title, row.desc, row.acceptance_criteria.as_deref().unwrap_or("[]"),
            row.next_action.as_deref().unwrap_or(""),
            execution.waiting.as_deref().unwrap_or("Owner authorization required"),
        );
        let mut new = crate::api::board_lifecycle::new_issue(
            &format!("project:{name}"),
            &format!("Prepare local candidate for {}: {}", row.id, row.title),
            &description,
            "code",
        );
        new.creator = "project-harness".into();
        new.source = Some(source);
        new.next_action = Some("Inspect the held task and checked local candidates; implement and test its safe local portion in this task's checkout, then retain the candidate and a precise approval handoff. Do not wait for production rollout to prepare it.".into());
        new.acceptance_criteria = Some(serde_json::to_string(&[
            "Produce a committed, independently testable local candidate for the held outcome, with executable checks and retained evidence; do not perform the approval-gated operation or claim unrun runtime/production checks passed.",
            "Retain a report identifying implemented local behavior, exact checks and results, reusable candidate commit, rollback/dry-run instructions where applicable, and the specific still-unapproved action. If no safe implementation is possible, provide concrete repository evidence of why; preparation never satisfies the original authorization or project acceptance.",
        ])?);
        let created = bs::create_issue(conn, &new, chrono::Utc::now().timestamp())?;
        conn.execute(
            "UPDATE issues SET project_group=?2,session=NULL WHERE id=?1",
            params![created.id, name],
        )?;
        let task = bs::get_issue(conn, &created.id)?
            .ok_or_else(|| anyhow::anyhow!("preparation task missing"))?;
        tracing::info!(project=name, task=%task.id, held_task=%row.id, measured=true, n_considered=1,
            verdict="project.authorization_preparation_created",
            "one local candidate task admitted; original authorization and acceptance remain held");
        result.applied = true;
        result.events.push(PendingEvent {
            entity_type: EntityType::Task,
            entity_id: task.id.clone(),
            mutation: MutationKind::Created,
            payload: Some(task.snapshot()),
        });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Connection {
        let c = crate::db::migrate::test_memdb();
        let policy = serde_json::from_value(json!({"repository":"/repo", "enabled":true,
            "coordinator":{"provider":"codex","model":"gpt-6-luna","effort":"low"},
            "executor":{"provider":"codex","model":"gpt-6-luna","effort":"low"},
            "verify_command":"git diff --check", "max_executors":2}))
        .unwrap();
        store::save(&c, "p", 0, &policy, "test").unwrap();
        c.execute("INSERT INTO issues(id,title,desc,status,type,project_group,created,updated,next_action,acceptance_criteria) VALUES('A','Canonical cutover','Local implementation and gated production rollout','blocked','code','p',1,1,'Implement local fixture parity','[\"Production parity before rollout\"]')", []).unwrap();
        let row = bs::get_issue(&c, "A").unwrap().unwrap();
        let e = planner::Execution {
            stage: "waiting".into(),
            attempt: 2,
            generation: 2,
            input_hash: planner::input_hash(&row),
            worker: "held-worker".into(),
            wait_category: Some("spend".into()),
            waiting: Some("spend: production backfill not approved".into()),
            ..Default::default()
        };
        planner::save_execution(&c, &row, &e, "project.waiting").unwrap();
        c
    }

    #[test]
    fn project_authorization_preparation_claims_local_work_without_releasing_hold() {
        let c = fixture();
        let before = serde_json::to_value(planner::execution(&c, "A").unwrap()).unwrap();
        let created = reconcile(&c, "p").unwrap();
        assert!(created.applied);
        assert_eq!(created.events.len(), 1);
        let id = &created.events[0].entity_id;
        let task = bs::get_issue(&c, id).unwrap().unwrap();
        assert_eq!(task.project_group.as_deref(), Some("p"));
        assert_eq!(task.creator, "project-harness");
        assert_eq!(
            task.source.as_deref(),
            Some("project-authorization-preparation:A")
        );
        assert!(task.depends_on.is_empty());
        assert!(task.session.is_none());
        assert!(!reconcile(&c, "p").unwrap().applied);
        assert_eq!(
            serde_json::to_value(planner::execution(&c, "A").unwrap()).unwrap(),
            before
        );
        assert!(!planner::claim(&c, "p", "A").unwrap().applied);
        assert!(planner::claim(&c, "p", id).unwrap().applied);
        let project = store::get(&c, "p").unwrap().unwrap();
        let plans = planner::plan(&c, &project).unwrap();
        assert_eq!(
            plans
                .iter()
                .find(|p| p.id == "A")
                .unwrap()
                .waiting_reason
                .as_deref(),
            Some("authorization_required")
        );
        let e = planner::execution(&c, id).unwrap();
        assert_eq!(e.stage, "reserved");
        assert_eq!(e.attempt, 1);
        assert_eq!(
            c.query_row("SELECT count(*) FROM cmd_history", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0,
            "harness recovery must not forge a human message"
        );
        // Even a successful local candidate cannot clear the original approval.
        c.execute("UPDATE issues SET status='verified' WHERE id=?1", [id])
            .unwrap();
        assert!(!planner::claim(&c, "p", "A").unwrap().applied);
        assert!(!reconcile(&c, "p").unwrap().applied);
    }

    #[test]
    fn project_authorization_preparation_does_not_loop_recreate_or_cross_projects() {
        let c = fixture();
        let id = reconcile(&c, "p").unwrap().events[0].entity_id.clone();
        let row = bs::get_issue(&c, &id).unwrap().unwrap();
        let e = planner::execution(&c, "A").unwrap();
        planner::save_execution(&c, &row, &e, "project.waiting").unwrap();
        assert!(
            !reconcile(&c, "p").unwrap().applied,
            "a held preparation cannot generate another preparation"
        );
        c.execute("UPDATE issues SET archived=1,deleted=1 WHERE id=?1", [&id])
            .unwrap();
        assert!(
            !reconcile(&c, "p").unwrap().applied,
            "explicit archive/delete is respected"
        );
        assert!(!reconcile(&c, "other").unwrap().applied);
        assert_eq!(
            c.query_row("SELECT count(*) FROM issues", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn project_authorization_preparation_respects_protected_worker() {
        let c = fixture();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("sessions")).unwrap();
        let env = home.path().join("sessions/held-worker.env");
        for key in ["CC_PAUSED", "CC_ARCHIVED", "CC_ISOLATED"] {
            std::fs::write(&env, format!("{key}=\"1\"\n")).unwrap();
            assert!(
                !reconcile_in_home(&c, "p", home.path()).unwrap().applied,
                "{key}"
            );
        }
        std::fs::write(
            &env,
            "CC_PAUSED=\"0\"\nCC_ARCHIVED=\"0\"\nCC_ISOLATED=\"0\"\n",
        )
        .unwrap();
        assert!(reconcile_in_home(&c, "p", home.path()).unwrap().applied);
    }

    #[test]
    fn project_authorization_preparation_respects_lifecycle_and_budget() {
        for variant in [
            "paused",
            "disabled",
            "budget",
            "pending",
            "archived",
            "terminal",
            "suspended",
            "reported",
            "decision",
            "operational",
            "working",
        ] {
            let c = fixture();
            let mut p = store::get(&c, "p").unwrap().unwrap();
            let row = bs::get_issue(&c, "A").unwrap().unwrap();
            let mut e = planner::execution(&c, "A").unwrap();
            match variant {
                "paused" => p.policy.paused = true,
                "disabled" => p.policy.enabled = false,
                "budget" => {
                    p.policy.token_budget = Some(1);
                    let (id, _) =
                        super::super::intake::receive(&c, "p", "spent", "Original request")
                            .unwrap();
                    c.execute(
                        "UPDATE cmd_history SET capture_pending=0,intake_result=?2 WHERE id=?1",
                        params![
                            id,
                            json!({"telemetry":{"attempt_usage":[{"usage":{"input_tokens":2}}]}})
                                .to_string()
                        ],
                    )
                    .unwrap();
                }
                "pending" => {
                    super::super::intake::receive(&c, "p", "pending", "Refine the task scope")
                        .unwrap();
                }
                "archived" => {
                    c.execute("UPDATE issues SET archived=1 WHERE id='A'", [])
                        .unwrap();
                }
                "terminal" => {}
                "suspended" => e.suspended = true,
                "reported" => {
                    e.report = Some(planner::Report {
                        head: "a".repeat(40),
                        checks: vec![],
                        assets: vec![],
                        summary: "Candidate exists".into(),
                    })
                }
                "decision" => {
                    c.execute("UPDATE issues SET type='decision' WHERE id='A'", [])
                        .unwrap();
                }
                "operational" => {
                    e.wait_category = Some("operational".into());
                    e.waiting = Some("operational: unavailable fixture".into());
                }
                "working" => e.stage = "working".into(),
                _ => unreachable!(),
            }
            store::save(&c, "p", p.revision, &p.policy, "test").unwrap();
            planner::save_execution(&c, &row, &e, "test").unwrap();
            if variant == "terminal" {
                c.execute("UPDATE issues SET status='verified' WHERE id='A'", [])
                    .unwrap();
            }
            assert!(!reconcile(&c, "p").unwrap().applied, "{variant}");
        }
    }
}
