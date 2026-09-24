//! Explicit output edges use the existing project task graph. No prose inference.
use super::{planner, store};
use crate::db::{board_store as bs, WriteOutcome};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub generation: i64,
    pub input_hash: String,
    pub idempotency_key: String,
    pub required_outputs: Vec<String>,
    pub reason: String,
    /// Exact explicit acknowledgement when replacing an already recorded wait.
    pub replaces_wait: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputWait {
    pub request: Request,
    pub previous_wait: Option<String>,
    pub continued_generation: Option<i64>,
    #[serde(default)]
    pub available: Vec<Value>,
}

/// Reuse current, checked local commits without turning discovery into an edge.
/// A candidate is not a whole-project runtime or human-approval receipt.
pub(crate) fn candidate_catalog(conn: &Connection, project: &str) -> anyhow::Result<Vec<Value>> {
    bs::project_issues(conn,project)?.into_iter().filter(|row|row.archived==0).map(|row| {
        let e=planner::execution(conn,&row.id)?;
        let candidate=if row.status=="verified" && e.stage=="verified" && e.input_hash==planner::input_hash(&row) {
            e.report.as_ref().map(|report|json!({"head":report.head,"summary":crate::api::board::chars_elide_middle(&report.summary,240,80),"verification":"task_candidate_only"}))
        } else { None };
        Ok(json!({"id":row.id,"title":row.title,"candidate":candidate}))
    }).collect()
}

pub(crate) fn authorization_hold(conn: &Connection, row: &bs::IssueRow) -> anyhow::Result<bool> {
    let e = planner::execution(conn, &row.id)?;
    let category_hold = matches!(
        e.wait_category.as_deref(),
        Some("spend" | "budget" | "customer_outbound")
    );
    let legacy_category_hold = e.wait_category.is_none()
        && e.waiting.as_deref().is_some_and(|w| {
            ["spend: ", "budget: ", "customer_outbound: "]
                .iter()
                .any(|prefix| w.starts_with(prefix))
        });
    Ok(row
        .ask_type
        .as_deref()
        .is_some_and(|s| matches!(s, "spend" | "budget" | "customer_outbound"))
        || (e.stage == "waiting" && (category_hold || legacy_category_hold)))
}

pub fn ready(conn: &Connection, row: &bs::IssueRow) -> anyhow::Result<bool> {
    Ok(super::graph::readiness(conn, row)? == super::graph::Readiness::Ready)
}

pub fn declare(
    conn: &Connection,
    project: &str,
    id: &str,
    worker: &str,
    request: &Request,
) -> anyhow::Result<WriteOutcome> {
    let mut row = bs::get_issue(conn, id)?.ok_or_else(|| anyhow::anyhow!("task missing"))?;
    let mut e = planner::execution(conn, id)?;
    anyhow::ensure!(
        row.project_group.as_deref() == Some(project) && !worker.is_empty() && e.worker == worker,
        "outside project or foreign executor"
    );
    anyhow::ensure!(
        row.archived == 0 && !authorization_hold(conn, &row)?,
        "task cannot declare an output wait"
    );
    anyhow::ensure!(
        e.input_hash == planner::input_hash(&row),
        "requirements changed"
    );
    if let Some(prior) = &e.output_wait {
        if prior.request.idempotency_key == request.idempotency_key {
            anyhow::ensure!(
                prior.request == *request,
                "idempotency key belongs to another output declaration"
            );
            return Ok(WriteOutcome {
                applied: false,
                events: vec![],
            });
        }
        anyhow::ensure!(
            prior.continued_generation.is_some(),
            "an output declaration is already pending"
        );
    }
    anyhow::ensure!(
        e.generation == request.generation && e.input_hash == request.input_hash,
        "stale output declaration"
    );
    anyhow::ensure!(
        matches!(e.stage.as_str(), "working" | "reserved" | "waiting"),
        "execution no longer accepts output declarations"
    );
    anyhow::ensure!(
        !request.idempotency_key.trim().is_empty()
            && request.idempotency_key.len() <= 160
            && !request.reason.trim().is_empty()
            && request.reason.len() <= 4000,
        "bounded key and reason required"
    );
    // Only the old API's exact machine-written category prefix is interpreted,
    // never IDs or intent from prose. New waits carry their category separately.
    let operational = e.wait_category.as_deref() == Some("operational")
        || (e.wait_category.is_none()
            && e.waiting
                .as_deref()
                .is_some_and(|w| w.starts_with("operational: ")));
    anyhow::ensure!(
        row.status == "doing" || (row.status == "blocked" && e.stage == "waiting" && operational),
        "task no longer active"
    );
    if e.stage == "waiting" {
        anyhow::ensure!(
            operational && request.replaces_wait == e.waiting,
            "only an explicitly acknowledged operational wait can become an output wait"
        );
    } else {
        anyhow::ensure!(
            request.replaces_wait.is_none(),
            "no recorded wait to replace"
        );
    }
    anyhow::ensure!(
        !request.required_outputs.is_empty() && request.required_outputs.len() <= 12,
        "declare 1..12 explicit required outputs"
    );
    let unique: HashSet<_> = request.required_outputs.iter().collect();
    anyhow::ensure!(
        unique.len() == request.required_outputs.len(),
        "duplicate output reference"
    );
    anyhow::ensure!(
        request
            .required_outputs
            .iter()
            .any(|d| !row.depends_on.contains(d)),
        "no new required output; do not replay a completed continuation"
    );
    // The shared graph seam owns existence, same-project, archived and cycle rules for every surface.
    let mut proposed = row.depends_on.clone();
    for output in &request.required_outputs {
        if !proposed.contains(output) {
            proposed.push(output.clone());
        }
    }
    super::graph::validate(conn, project, id, &proposed, "required_outputs")
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    for output in &request.required_outputs {
        let target = bs::get_issue(conn, output)?
            .ok_or_else(|| anyhow::anyhow!("required output missing: {output}"))?;
        anyhow::ensure!(
            !authorization_hold(conn, &target)?,
            "authorization wait is not a required output: {output}"
        );
    }
    for output in &request.required_outputs {
        if !row.depends_on.contains(output) {
            row.depends_on.push(output.clone());
        }
    }
    conn.execute(
        "UPDATE issues SET depends_on=?2 WHERE id=?1",
        params![id, serde_json::to_string(&row.depends_on)?],
    )?;
    e.output_wait = Some(OutputWait {
        request: request.clone(),
        previous_wait: e.waiting.clone(),
        continued_generation: None,
        available: vec![],
    });
    e.input_hash = planner::input_hash(&row);
    e.stage = "waiting".into();
    e.wait_category = Some("required_outputs".into());
    e.waiting = Some(format!(
        "required_outputs: {}",
        request.required_outputs.join(", ")
    ));
    planner::save_execution(conn, &row, &e, "project.outputs_declared")
}

/// Same serialized planner predicate as discovery. An arrival consumes no repair
/// attempt and never rewrites an ended attempt's evidence or timestamps.
pub fn resume(conn: &Connection, project: &str, id: &str) -> anyhow::Result<WriteOutcome> {
    let p = store::get(conn, project)?.ok_or_else(|| anyhow::anyhow!("project missing"))?;
    let item = planner::plan(conn, &p)?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| anyhow::anyhow!("task missing"))?;
    if item.action != "resume_outputs" {
        return Ok(WriteOutcome {
            applied: false,
            events: vec![],
        });
    }
    let row = bs::get_issue(conn, id)?.ok_or_else(|| anyhow::anyhow!("task missing"))?;
    let mut e = item.execution;
    let mut available = vec![];
    for output in &row.depends_on {
        let r = bs::get_issue(conn, output)?.ok_or_else(|| anyhow::anyhow!("output missing"))?;
        let state = planner::execution(conn, output)?;
        available.push(json!({"id":output,"report":state.report,"evidence":r.evidence}));
    }
    e.generation += 1;
    e.delivery_id = format!("project:{project}:{id}:{}", e.generation);
    let output_wait = e
        .output_wait
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("structured wait missing"))?;
    output_wait.continued_generation = Some(e.generation);
    output_wait.available = available;
    e.stage = "reserved".into();
    e.waiting = None;
    e.wait_category = None;
    e.suspended = false;
    e.observed_at = chrono::Utc::now().timestamp();
    conn.execute("UPDATE issues SET status='doing',lease_owner=?2,lease_generation=?3,lease_acquired_at=?4,lease_heartbeat_at=?4,lease_expires_at=?5 WHERE id=?1",params![id,e.worker,e.generation,e.observed_at,e.observed_at+300])?;
    planner::save_execution(conn, &row, &e, "project.outputs_continued")
}

/// Durable continuation claim windows are bounded by the next non-active or
/// different-generation execution event. Old attempt rows remain immutable.
pub(crate) fn usage_windows(
    conn: &Connection,
    now: i64,
    max_window: i64,
) -> rusqlite::Result<Vec<(String, String, i64, i64)>> {
    let mut q=conn.prepare("SELECT type,data,CAST(ts AS INTEGER) FROM session_events WHERE type LIKE 'project.%' ORDER BY id")?;
    let events = q.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let mut open = std::collections::HashMap::<String, (String, i64, i64)>::new();
    let mut windows = vec![];
    for event in events {
        let (kind, raw, ts) = event?;
        let Some(value) = raw.and_then(|v| serde_json::from_str::<Value>(&v).ok()) else {
            continue;
        };
        let (Some(id), Some(e)) = (value["task"].as_str(), value.get("execution")) else {
            continue;
        };
        let Some(generation) = e["generation"].as_i64() else {
            continue;
        };
        let active =
            matches!(e["stage"].as_str(), Some("reserved" | "working")) && e["suspended"] != true;
        if open
            .get(id)
            .is_some_and(|(_, prior, _)| *prior != generation || !active)
        {
            let (worker, _, start) = open.remove(id).unwrap();
            windows.push((
                id.to_string(),
                worker,
                start,
                (ts - 1).min(start + max_window),
            ));
        }
        if active
            && (kind == "project.outputs_continued"
                || e.pointer("/output_wait/continued_generation")
                    .and_then(Value::as_i64)
                    == Some(generation))
        {
            if let Some(worker) = e["worker"].as_str().filter(|w| !w.is_empty()) {
                open.entry(id.to_string())
                    .or_insert((worker.into(), generation, ts));
            }
        }
    }
    windows.extend(
        open.into_iter()
            .map(|(id, (worker, _, start))| (id, worker, start, now.min(start + max_window))),
    );
    Ok(windows)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::{attempts, Store};
    #[test]
    fn project_candidate_catalog_shares_only_current_checked_local_heads() {
        let (_dir,db,_)=fixture();
        db.write(|c| {
            let row=bs::get_issue(c,"A")?.unwrap();let mut e=planner::execution(c,"A").unwrap();e.stage="verified".into();e.waiting=None;e.wait_category=None;e.report=Some(planner::Report{head:"a".repeat(40),summary:"Existing repository gate repaired".into(),checks:vec![],assets:vec![]});
            c.execute("UPDATE issues SET status='verified' WHERE id='A'",[])?;planner::save_execution(c,&row,&e,"test.verified").map_err(store::sql_error)?;
            c.execute("INSERT INTO issues(id,title,status,type,project_group,created,updated) VALUES('FOREIGN','other project','verified','code','other',1,1)",[])?;
            let catalog=candidate_catalog(c,"sample").unwrap();assert!(!catalog.iter().any(|r|r["id"]=="FOREIGN"));
            assert_eq!(catalog.iter().find(|r|r["id"]=="A").unwrap()["candidate"]["head"],"a".repeat(40));
            assert!(catalog.iter().find(|r|r["id"]=="B").unwrap()["candidate"].is_null());
            c.execute("UPDATE issues SET title='Changed requirements' WHERE id='A'",[])?;
            assert!(candidate_catalog(c,"sample").unwrap().iter().find(|r|r["id"]=="A").unwrap()["candidate"].is_null());
            Ok(WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
    }

    pub(crate) fn fixture() -> (tempfile::TempDir, Store, Request) {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::open(&dir.path().join("db")).unwrap();
        db.write(|c| {
            let policy=serde_json::from_value(json!({"repository":"/repo","coordinator":{"provider":"codex","model":"gpt-6-astra"},"executor":{"provider":"codex","model":"gpt-6-astra"},"verify_command":"./verify.sh","max_executors":2,"enabled":true})).unwrap();
            store::save(c,"sample",0,&policy,"test").map_err(store::sql_error)?;
            for id in ["A","B","C"] {
                c.execute("INSERT INTO issues(id,title,desc,status,type,project_group,created,updated,next_action,acceptance_criteria) VALUES(?1,'Output','Build output','todo','code','sample',1,1,'Implement and test','[\"Output passes\"]')",[id])?;
            }
            planner::claim(c,"sample","A").map_err(store::sql_error)?;
            let row=bs::get_issue(c,"A")?.unwrap();let mut e=planner::execution(c,"A").unwrap();
            e.stage="repair".into();e.waiting=Some("browser gate failed for missing backend".into());
            planner::save_execution(c,&row,&e,"project.execution").map_err(store::sql_error)?;
            planner::claim(c,"sample","A").map_err(store::sql_error)?;
            let row=bs::get_issue(c,"A")?.unwrap();let mut e=planner::execution(c,"A").unwrap();
            e.stage="waiting".into();e.waiting=Some("operational: backend source unavailable; keep this exact evidence".into());
            planner::save_execution(c,&row,&e,"project.waiting").map_err(store::sql_error)
        }).unwrap();
        let e = planner::execution(&db.read().unwrap(), "A").unwrap();
        let request = Request {
            generation: e.generation,
            input_hash: e.input_hash,
            idempotency_key: "explicit-output-1".into(),
            required_outputs: vec!["B".into()],
            reason: "Browser gate needs backend source".into(),
            replaces_wait: e.waiting,
        };
        (dir, db, request)
    }
    fn worker(c: &Connection) -> String {
        planner::execution(c, "A").unwrap().worker
    }
    fn current_verified(c: &Connection, id: &str) {
        c.execute(
            "UPDATE issues SET status='verified',evidence='Integrated current output' WHERE id=?1",
            [id],
        )
        .unwrap();
        let row = bs::get_issue(c, id).unwrap().unwrap();
        let mut execution = planner::execution(c, id).unwrap();
        execution.stage = "verified".into();
        execution.input_hash = planner::input_hash(&row);
        execution.report = Some(planner::Report {
            assets: vec![],
            head: "b".repeat(40),
            checks: vec![],
            summary: "verified output".into(),
        });
        planner::save_execution(c, &row, &execution, "test.verified").unwrap();
    }
    #[tokio::test]
    async fn project_outputs_continuation_usage_is_attributed_only_inside_its_window() {
        let (_dir, db, request) = fixture();
        let db = std::sync::Arc::new(db);
        db.write(move|c| {
            let now=chrono::Utc::now().timestamp();let w=worker(c);
            c.execute("UPDATE task_attempts SET started_at=?1,ended_at=?2",params![now-120,now-100])?;
            c.execute("UPDATE session_events SET ts=?1",[now-100])?;
            declare(c,"sample","A",&w,&request).unwrap();
            current_verified(c, "B");
            resume(c,"sample","A").unwrap();
            c.execute("UPDATE session_events SET ts=?1 WHERE type='project.outputs_continued' OR (type='task.claimed' AND json_extract(data,'$.continuation')=1)",[now-60])?;
            let row=bs::get_issue(c,"A")?.unwrap();let mut e=planner::execution(c,"A").unwrap();
            let packet=super::super::driver::packet(&store::get(c,"sample").unwrap().unwrap(),&row,&e);
            assert!(packet.contains("required-outputs") && packet.contains("required_outputs") && packet.contains("executable candidate-relative check"));
            e.stage="waiting".into();e.waiting=Some("operational: stopped after continuation".into());
            planner::save_execution(c,&row,&e,"project.waiting").unwrap();
            c.execute("UPDATE session_events SET ts=?1 WHERE id=(SELECT max(id) FROM session_events)",[now-40])?;
            for (conversation,ts) in [("before",now-70),("inside",now-50),("after",now-30)] {
                c.execute("INSERT INTO token_ledger(ts,session,conversation,input,output) VALUES(?1,?2,?3,10,5)",params![ts,w,conversation])?;
            }
            Ok(WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
        crate::runtime_jobs::token_ledger::attribute_tasks(&db)
            .await
            .unwrap();
        let c = db.read().unwrap();
        for (conversation, expected) in [("before", ""), ("inside", "A"), ("after", "")] {
            assert_eq!(
                c.query_row(
                    "SELECT task FROM token_ledger WHERE conversation=?1",
                    [conversation],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
                expected,
                "{conversation}"
            );
        }
        assert_eq!(attempts::list_for_card(&c, "A").unwrap().len(), 2);
        assert_eq!(planner::execution(&c, "A").unwrap().attempt, 2);
        assert_eq!(
            super::super::usage::summary(&c, "sample").unwrap()["tokens"],
            15
        );
    }
    #[test]
    fn project_outputs_continue_once_without_spending_attempt_or_erasing_failure() {
        let (_dir, db, request) = fixture();
        db.write(move |c| {
            let before = planner::execution(c, "A").unwrap();
            let attempts = attempts::list_for_card(c, "A")?;
            // Negative control: even a Verified producer cannot wake a prose wait.
            current_verified(c, "B");
            assert!(!resume(c, "sample", "A").unwrap().applied);
            let view = store::board(c, "sample").unwrap();
            assert_eq!(
                view["cards"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|c| c["id"] == "A")
                    .unwrap()["phase"],
                "waiting"
            );
            c.execute("UPDATE issues SET status='review' WHERE id='B'", [])?;
            assert!(
                declare(c, "sample", "A", &before.worker, &request)
                    .unwrap()
                    .applied
            );
            assert!(
                !declare(c, "sample", "A", &before.worker, &request)
                    .unwrap()
                    .applied
            );
            assert!(
                !resume(c, "sample", "A").unwrap().applied,
                "reported is not Verified"
            );
            let waiting = planner::execution(c, "A").unwrap();
            assert_eq!(
                waiting.output_wait.as_ref().unwrap().previous_wait,
                before.waiting
            );
            assert_eq!(waiting.last_failure, before.last_failure);
            current_verified(c, "B");
            assert!(resume(c, "sample", "A").unwrap().applied);
            assert!(!resume(c, "sample", "A").unwrap().applied);
            let after = planner::execution(c, "A").unwrap();
            assert_eq!(after.attempt, 2);
            assert_eq!(after.generation, before.generation + 1);
            assert_ne!(after.delivery_id, before.delivery_id);
            assert_eq!(after.last_failure, before.last_failure);
            assert_eq!(
                attempts::list_for_card(c, "A")?,
                attempts,
                "ended attempt history is immutable"
            );
            assert!(
                !declare(c, "sample", "A", &before.worker, &request)
                    .unwrap()
                    .applied
            );
            assert!(
                planner::delivery_current(c, "sample", &after.worker, &after.delivery_id).unwrap()
            );
            assert!(
                !planner::delivery_current(c, "sample", &before.worker, &before.delivery_id)
                    .unwrap()
            );
            // Output regression after the reservation must prevent delivery.
            c.execute("UPDATE issues SET status='review' WHERE id='B'", [])?;
            assert!(
                !planner::delivery_current(c, "sample", &after.worker, &after.delivery_id).unwrap()
            );
            let report = planner::Report {
                assets: vec![],
                head: "a".repeat(40),
                summary: "claim success".into(),
                checks: vec![],
            };
            assert!(planner::record_report(
                c,
                "sample",
                "A",
                &before.worker,
                before.generation,
                &before.input_hash,
                &report
            )
            .is_err());
            assert!(
                planner::record_report(
                    c,
                    "sample",
                    "A",
                    &after.worker,
                    after.generation,
                    &after.input_hash,
                    &report
                )
                .is_err(),
                "executable criteria still required"
            );
            assert_eq!(bs::get_issue(c, "A")?.unwrap().status, "doing");
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .unwrap();
    }
    #[test]
    fn project_outputs_refuse_foreign_stale_missing_cycles_and_authorization_holds() {
        let (_dir, db, request) = fixture();
        db.write(move |c| {
            let w = worker(c);
            assert!(declare(c, "sample", "A", "foreign", &request).is_err());
            assert!(declare(c, "foreign-project", "A", &w, &request).is_err());
            let mut bad = request.clone();
            bad.generation += 1;
            assert!(declare(c, "sample", "A", &w, &bad).is_err());
            bad = request.clone();
            bad.input_hash = "old".into();
            assert!(declare(c, "sample", "A", &w, &bad).is_err());
            bad = request.clone();
            bad.replaces_wait = None;
            assert!(declare(c, "sample", "A", &w, &bad).is_err());
            for ids in [vec![], vec!["A"], vec!["missing"], vec!["B", "B"]] {
                bad = request.clone();
                bad.required_outputs = ids.into_iter().map(String::from).collect();
                assert!(declare(c, "sample", "A", &w, &bad).is_err());
            }
            c.execute("UPDATE issues SET project_group='other' WHERE id='B'", [])?;
            assert!(declare(c, "sample", "A", &w, &request).is_err());
            c.execute(
                "UPDATE issues SET project_group='sample',depends_on='[\"A\"]' WHERE id='B'",
                [],
            )?;
            assert!(declare(c, "sample", "A", &w, &request).is_err());
            c.execute("UPDATE issues SET depends_on='[\"C\"]' WHERE id='B'", [])?;
            c.execute("UPDATE issues SET depends_on='[\"B\"]' WHERE id='C'", [])?;
            assert!(declare(c, "sample", "A", &w, &request).is_err());
            c.execute(
                "UPDATE issues SET depends_on='[]' WHERE id IN ('B','C')",
                [],
            )?;
            for category in ["spend", "budget", "customer_outbound"] {
                c.execute("UPDATE issues SET ask_type=?1 WHERE id='B'", [category])?;
                assert!(declare(c, "sample", "A", &w, &request).is_err());
            }
            c.execute("UPDATE issues SET ask_type=NULL WHERE id='B'", [])?;
            let row = bs::get_issue(c, "A")?.unwrap();
            let old = planner::execution(c, "A").unwrap();
            for category in ["spend", "customer_outbound"] {
                let mut e = old.clone();
                e.wait_category = Some(category.into());
                e.waiting = Some(format!("{category}: owner authorization required"));
                planner::save_execution(c, &row, &e, "project.waiting")
                    .map_err(store::sql_error)?;
                bad = request.clone();
                bad.replaces_wait = e.waiting;
                assert!(declare(c, "sample", "A", &w, &bad).is_err());
            }
            assert!(
                bs::get_issue(c, "A")?.unwrap().depends_on.is_empty(),
                "refusals must not add edges"
            );
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .unwrap();
    }
    #[test]
    fn project_outputs_resume_rechecks_pause_capacity_requirements_and_budget() {
        let (_dir, db, request) = fixture();
        db.write(move|c| {
            declare(c,"sample","A",&worker(c),&request).unwrap();
            current_verified(c, "B");
            let p=store::get(c,"sample").unwrap().unwrap();
            for field in ["paused","enabled","max_executors","token_budget"] {
                let mut policy=p.policy.clone();
                match field {"paused"=>policy.paused=true,"enabled"=>policy.enabled=false,"max_executors"=>{policy.max_executors=1;planner::claim(c,"sample","C").unwrap();},_=>{policy.token_budget=Some(1);c.execute("INSERT INTO cmd_history(text,type,session,ts,project_group,intake_attempts) VALUES('unmeasured','user','project:sample',1,'sample',1)",[])?;}}
                c.execute("UPDATE group_config SET execution_policy=?1 WHERE name='sample'",[serde_json::to_string(&policy).unwrap()])?;
                assert!(!resume(c,"sample","A").unwrap().applied,"{field}");
                c.execute("UPDATE group_config SET execution_policy=?1 WHERE name='sample'",[serde_json::to_string(&p.policy).unwrap()])?;
            }
            c.execute("UPDATE issues SET title='changed requirements' WHERE id='A'",[])?;
            assert!(!resume(c,"sample","A").unwrap().applied);
            assert_eq!(planner::execution(c,"A").unwrap().attempt,2);
            Ok(WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
    }
}
