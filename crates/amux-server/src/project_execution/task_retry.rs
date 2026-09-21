//! An operator grant permits one attempt, never resets execution history.
use super::{planner, store};
use crate::db::{board_store as bs, WriteOutcome};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub idempotency_key: String,
    pub expect_generation: i64,
    pub expect_revision: i64,
    pub input_hash: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    pub request: Request,
    pub allowed_through: u32,
    pub previous_result: Value,
}
/// Shared UI/server admission; request identity is checked separately by grant.
pub fn eligible(
    c: &Connection,
    p: &store::Project,
    row: &bs::IssueRow,
    e: &planner::Execution,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        p.policy.enabled && !p.policy.paused,
        "project paused or disabled"
    );
    anyhow::ensure!(
        e.input_hash == planner::input_hash(row),
        "requirements changed"
    );
    anyhow::ensure!(
        matches!(row.status.as_str(), "doing" | "review")
            && row.archived == 0
            && e.stage == "waiting"
            && e.waiting.is_some(),
        "only a failed waiting execution can be retried"
    );
    anyhow::ensure!(
        !super::outputs::authorization_hold(c, row)?
            && !matches!(
                e.wait_category.as_deref(),
                Some("spend" | "customer_outbound" | "required_outputs")
            )
            && row.ask_type.is_none(),
        "resolve the declared hold rather than retrying it"
    );
    anyhow::ensure!(
        super::usage::waiting(c, p)?.is_none(),
        "project budget prevents retry"
    );
    Ok(())
}

pub fn grant(
    c: &Connection,
    project: &str,
    id: &str,
    body: &Request,
) -> anyhow::Result<WriteOutcome> {
    let p = store::get(c, project)?.ok_or_else(|| anyhow::anyhow!("project missing"))?;
    let row = bs::get_issue(c, id)?.ok_or_else(|| anyhow::anyhow!("task missing"))?;
    anyhow::ensure!(
        row.project_group.as_deref() == Some(project),
        "outside project"
    );
    let mut e = planner::execution(c, id)?;
    anyhow::ensure!(
        e.input_hash == planner::input_hash(&row) && body.input_hash == e.input_hash,
        "requirements changed"
    );
    if let Some(old) = e
        .retry_grants
        .iter()
        .find(|g| g.request.idempotency_key == body.idempotency_key)
    {
        anyhow::ensure!(old.request == *body, "retry key belongs to another request");
        return Ok(WriteOutcome {
            applied: false,
            events: vec![],
        });
    }
    anyhow::ensure!(
        p.policy.enabled && !p.policy.paused,
        "project paused or disabled"
    );
    anyhow::ensure!(
        !body.idempotency_key.trim().is_empty() && body.idempotency_key.len() <= 160,
        "bounded idempotency key required"
    );
    anyhow::ensure!(
        row.rev == body.expect_revision && e.generation == body.expect_generation,
        "stale retry revision or generation"
    );
    eligible(c, &p, &row, &e)?;
    let allowed = e
        .attempt
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("attempt counter exhausted"))?;
    e.retry_grants.push(Grant{request:body.clone(),allowed_through:allowed,previous_result:json!({"attempt":e.attempt,"generation":e.generation,"waiting":e.waiting,"last_failure":e.last_failure,"report":e.report})});
    e.stage = "repair".into();
    planner::save_execution(c, &row, &e, "project.retry_granted")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn project_failed_review_retry_preserves_report_and_reports_new_generation() {
        let (_dir, db, _) = super::super::outputs::tests::fixture();
        db.write(|c| {
            let row = bs::get_issue(c, "A")?.unwrap();
            let mut e = planner::execution(c, "A").unwrap();
            e.stage = "working".into();
            e.waiting = None;
            planner::save_execution(c, &row, &e, "project.execution").unwrap();
            let old = planner::Report {
                head: "a".repeat(40),
                summary: "candidate before integration failure".into(),
                assets: vec![],
                checks: vec![planner::Check {
                    criterion: "Output passes".into(),
                    command: "test -f output".into(),
                }],
            };
            planner::record_report(
                c,
                "sample",
                "A",
                &e.worker,
                e.generation,
                &e.input_hash,
                &old,
            )
            .unwrap();
            let row = bs::get_issue(c, "A")?.unwrap();
            assert_eq!(row.status, "review");
            e = planner::execution(c, "A").unwrap();
            assert!(
                eligible(c, &store::get(c, "sample").unwrap().unwrap(), &row, &e).is_err(),
                "reported success cannot retry"
            );
            // Same durable failure transition used after an integration gate fails.
            e.stage = "waiting".into();
            e.waiting = Some("integration gate failed: exit 1".into());
            planner::save_execution(c, &row, &e, "project.execution").unwrap();
            let row = bs::get_issue(c, "A")?.unwrap();
            let request = Request {
                idempotency_key: "review-retry".into(),
                expect_revision: row.rev,
                expect_generation: e.generation,
                input_hash: e.input_hash.clone(),
            };
            assert!(eligible(c, &store::get(c, "sample").unwrap().unwrap(), &row, &e).is_ok());
            assert!(grant(c, "sample", "A", &request).unwrap().applied);
            assert!(!grant(c, "sample", "A", &request).unwrap().applied);
            let repair = planner::execution(c, "A").unwrap();
            assert_eq!(repair.stage, "repair");
            assert_eq!(repair.report, Some(old.clone()));
            assert!(planner::claim(c, "sample", "A").unwrap().applied);
            let next = planner::execution(c, "A").unwrap();
            assert_eq!(next.attempt, e.attempt + 1);
            assert_eq!(next.generation, e.generation + 1);
            assert_ne!(next.delivery_id, e.delivery_id);
            assert_eq!(bs::get_issue(c, "A")?.unwrap().status, "doing");
            assert_eq!(next.retry_grants[0].previous_result["report"], json!(old));
            assert_eq!(next.last_failure, e.waiting);
            assert!(planner::record_report(
                c,
                "sample",
                "A",
                &e.worker,
                e.generation,
                &e.input_hash,
                &old
            )
            .is_err());
            let new = planner::Report {
                head: "b".repeat(40),
                ..old
            };
            planner::record_report(
                c,
                "sample",
                "A",
                &next.worker,
                next.generation,
                &next.input_hash,
                &new,
            )
            .unwrap();
            assert_eq!(bs::get_issue(c, "A")?.unwrap().status, "review");
            assert_eq!(planner::execution(c, "A").unwrap().report, Some(new));
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .unwrap();
    }
    #[test]
    fn project_task_retry_is_one_monotonic_attempt_and_idempotent() {
        let (_dir, db, _) = super::super::outputs::tests::fixture();
        db.write(|c| {
            let row = bs::get_issue(c, "A")?.unwrap();
            let e = planner::execution(c, "A").unwrap();
            let request = Request {
                idempotency_key: "click".into(),
                expect_generation: e.generation,
                expect_revision: row.rev,
                input_hash: e.input_hash.clone(),
            };
            assert!(grant(c, "other", "A", &request).is_err());
            let mut stale = request.clone();
            stale.expect_generation -= 1;
            assert!(grant(c, "sample", "A", &stale).is_err());
            assert!(grant(c, "sample", "A", &request).unwrap().applied);
            assert!(!grant(c, "sample", "A", &request).unwrap().applied);
            assert_eq!(planner::execution(c, "A").unwrap().attempt, 2);
            planner::claim(c, "sample", "A").unwrap();
            let mut current = planner::execution(c, "A").unwrap();
            assert_eq!(current.attempt, 3);
            assert!(!grant(c, "sample", "A", &request).unwrap().applied);
            let row = bs::get_issue(c, "A")?.unwrap();
            let new = Request {
                idempotency_key: "other-click".into(),
                expect_generation: current.generation,
                expect_revision: row.rev,
                input_hash: current.input_hash.clone(),
            };
            assert!(
                grant(c, "sample", "A", &new).is_err(),
                "active retry refused"
            );
            current.stage = "repair".into();
            planner::save_execution(c, &row, &current, "project.execution").unwrap();
            assert!(
                !planner::claim(c, "sample", "A").unwrap().applied,
                "grant cannot authorize a fourth automatic attempt"
            );
            current.stage = "waiting".into();
            current.waiting = Some("spend: approval".into());
            planner::save_execution(c, &row, &current, "project.waiting").unwrap();
            let mut new = new;
            new.expect_revision = bs::get_issue(c, "A")?.unwrap().rev;
            assert!(
                grant(c, "sample", "A", &new).is_err(),
                "legacy spend hold refused"
            );
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM task_attempts WHERE card='A'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                3
            );
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .unwrap();
    }
    #[test]
    fn project_task_retry_refuses_pause_stale_and_nonwaiting_states() {
        for (variant, status) in [
            "paused",
            "disabled",
            "reported",
            "working",
            "verified",
            "requirements",
            "budget",
        ]
        .into_iter()
        .flat_map(|variant| ["doing", "review"].map(|status| (variant, status)))
        {
            let (_dir, db, _) = super::super::outputs::tests::fixture();
            let variant = variant.to_string();
            db.write(move |c| {
                c.execute("UPDATE issues SET status=?1 WHERE id='A'", [status])?;
                let row = bs::get_issue(c, "A")?.unwrap();
                let mut e = planner::execution(c, "A").unwrap();
                let mut request = Request {
                    idempotency_key: "one".into(),
                    expect_generation: e.generation,
                    expect_revision: row.rev,
                    input_hash: e.input_hash.clone(),
                };
                match variant.as_str() {
                    "paused" | "disabled" | "budget" => {
                        let mut p = store::get(c, "sample").unwrap().unwrap();
                        if variant == "paused" {
                            p.policy.paused = true;
                        } else if variant == "disabled" {
                            p.policy.enabled = false;
                        } else {
                            p.policy.token_budget = Some(1);
                        }
                        store::save(c, "sample", p.revision, &p.policy, "test").unwrap();
                    }
                    "requirements" => {
                        c.execute("UPDATE issues SET title='changed' WHERE id='A'", [])?;
                    }
                    stage => {
                        e.stage = stage.into();
                        planner::save_execution(c, &row, &e, "project.execution").unwrap();
                        request.expect_revision = bs::get_issue(c, "A")?.unwrap().rev;
                    }
                }
                assert!(grant(c, "sample", "A", &request).is_err(), "{variant}");
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .unwrap();
        }
    }
}
