//! Coverage travels with usage. Ledger dollar values are price estimates,
//! never presented as billed subscription spend or as a provider hard cap.
use super::store::Project;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

// Disjoint indexed branches preserve task ownership without scanning the ledger.
const EXECUTION_USAGE_SQL: &str = "SELECT model,count(*),sum(input),sum(cache_read),sum(cache_write),sum(output),sum(outside),sum(CASE WHEN outside=1 THEN input+cache_read+cache_write+output ELSE 0 END) FROM (
 SELECT l.model,l.input,l.cache_read,l.cache_write,l.output,0 AS outside
 FROM issues i CROSS JOIN token_ledger l INDEXED BY idx_ledger_task ON l.task=i.id WHERE i.project_group=?1
 UNION ALL
 SELECT model,input,cache_read,cache_write,output,1 AS outside FROM token_ledger INDEXED BY idx_ledger_session WHERE session IN (SELECT value FROM json_each(?2)) AND task=''
) GROUP BY model";

/// Dedicated workspace identity supplements project accounting, never task claims.
fn executor_owners(conn: &Connection, name: &str) -> anyhow::Result<Vec<String>> {
    let home = crate::config::amux_home();
    let validated =
        crate::runtime_jobs::codex_ledger::workspace_workdirs(&home, Default::default());
    let canonical = |path: &String| std::fs::canonicalize(path).unwrap_or_else(|_| path.into());
    let mut path_counts = std::collections::HashMap::new();
    for path in validated.values() {
        *path_counts.entry(canonical(path)).or_insert(0usize) += 1;
    }
    let mut q=conn.prepare("SELECT DISTINCT json_extract(execution_state,'$.worker') FROM issues WHERE project_group=?1 AND json_valid(execution_state) AND json_extract(execution_state,'$.worker')<>''")?;
    let workers = q
        .query_map([name], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut owners = Vec::new();
    for worker in workers {
        if !validated
            .get(&worker)
            .is_some_and(|path| path_counts.get(&canonical(path)) == Some(&1))
        {
            continue;
        }
        let path = home.join("sessions").join(format!("{worker}.env"));
        let path = if path.exists() {
            path
        } else {
            path.with_extension("env.reaped")
        };
        if crate::config::parse_env_file(&path)
            .get("CC_PROJECT")
            .map(String::as_str)
            != Some(name)
        {
            continue;
        }
        let foreign:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM issues WHERE COALESCE(project_group,'')<>?1 AND json_valid(execution_state) AND json_extract(execution_state,'$.worker')=?2)",params![name,worker],|r|r.get(0))?;
        if !foreign {
            owners.push(worker);
        }
    }
    Ok(owners)
}

pub fn summary(conn: &Connection, name: &str) -> anyhow::Result<Value> {
    use crate::runtime_jobs::token_ledger;
    let rates = token_ledger::prices(&crate::config::amux_home());
    let owners = executor_owners(conn, name)?;
    let owned = serde_json::to_string(&owners)?;
    let mut q = conn.prepare(EXECUTION_USAGE_SQL)?;
    let rows = q.query_map(params![name, owned], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            [
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ],
            r.get::<_, i64>(6)?,
            r.get::<_, i64>(7)?,
        ))
    })?;
    let (mut outside_turns, mut outside_tokens) = (0i64, 0i64);
    let (mut turns, mut tokens, mut cost_turns) = (0i64, 0i64, 0i64);
    let mut cost = 0.0;
    let mut unpriced_models = Vec::new();
    for row in rows {
        let (model, count, usage, unclaimed_turns, unclaimed_tokens) = row?;
        outside_turns += unclaimed_turns;
        outside_tokens += unclaimed_tokens;
        turns += count;
        tokens += usage.iter().sum::<i64>();
        if token_ledger::model_is_priced(&rates, &model) {
            cost_turns += count;
            // Current configured rates also distinguish genuinely free models
            // from legacy placeholder dollars; never rewrite the source ledger.
            cost += token_ledger::turn_cost_usd(&rates, &model, usage);
        } else {
            unpriced_models.push(model);
        }
    }
    let (calls, receipts): (i64, i64) = conn.query_row(
        "SELECT coalesce(sum(intake_attempts),0),count(*) FROM cmd_history WHERE session='project:'||project_group AND type='user' AND project_group=?1",
        [name],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut q = conn.prepare(
        "SELECT intake_result FROM cmd_history WHERE session='project:'||project_group AND type='user' AND project_group=?1 AND intake_attempts>0",
    )?;
    let raw = q
        .query_map([name], |r| r.get::<_, Option<String>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut intake_tokens = 0u64;
    let mut measured_calls = 0;
    let mut intake_cost = 0.0;
    let mut cost_calls = 0;
    for raw in raw.into_iter().flatten() {
        let v: Value = serde_json::from_str(&raw)?;
        let usage = v
            .pointer("/telemetry/attempt_usage")
            .or_else(|| v.pointer("/plan/telemetry/attempt_usage"))
            .and_then(Value::as_array);
        for usage in usage.into_iter().flatten() {
            let u = usage.get("usage").unwrap_or(usage);
            if let (Some(input), Some(output)) =
                (u["input_tokens"].as_u64(), u["output_tokens"].as_u64())
            {
                measured_calls += 1;
                intake_tokens += input
                    + output
                    + u["cache_read_input_tokens"].as_u64().unwrap_or(0)
                    + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            }
            if let Some(cost) = usage["total_cost_usd"]
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0)
            {
                cost_calls += 1;
                intake_cost += cost;
            }
        }
    }
    let (outcomes,verified):(i64,i64)=conn.query_row("SELECT count(DISTINCT c.card_id),count(DISTINCT CASE WHEN i.status='verified' THEN c.card_id END) FROM cmd_history c JOIN issues i ON i.id=c.card_id WHERE c.session='project:'||c.project_group AND c.type='user' AND c.project_group=?1 AND c.capture_pending=0",[name],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let attempts:i64=conn.query_row("SELECT count(*) FROM session_events WHERE type='project.claimed' AND json_extract(data,'$.project_group')=?1",[name],|r|r.get(0))?;
    let measured = turns > 0 || measured_calls > 0;
    let observed = measured.then_some(tokens as u64 + intake_tokens);
    let cost_measured = (turns > 0 || cost_calls > 0) && cost_turns == turns && cost_calls == calls;
    let cost_reason = if cost_turns < turns {
        Some("execution model rates missing")
    } else if cost_calls < calls {
        Some("intake cost has not been observed for every attempt")
    } else if !cost_measured {
        Some("provider cost has not been observed")
    } else {
        None
    };
    if cost_turns < turns
        && crate::log_dedupe::first_this_bucket(
            &format!("project-cost-unpriced:{name}"),
            crate::log_dedupe::hour_bucket(crate::config::now_f64()),
        )
    {
        tracing::warn!(project=name, measured=false, n_considered=turns, unpriced_turns=turns-cost_turns, models=?unpriced_models, verdict="project.cost_unmeasured", "execution tokens observed but model rates missing; aggregate cost withheld");
    }
    Ok(
        json!({"measured":measured,"n_considered":attempts+calls,"tokens":observed,"cost_usd":Value::Null,"estimated_cost_usd":cost_measured.then_some(cost+intake_cost),"cost_measured":cost_measured,"cost_reason":cost_reason,"execution_cost_turns_measured":cost_turns,"unpriced_models":unpriced_models,"execution_turns_measured":turns,"execution_attempt_turns_measured":turns-outside_turns,"executor_unattributed_turns_measured":outside_turns,"executor_unattributed_tokens":outside_tokens,"execution_attempts":attempts,"intake_calls":calls,"intake_calls_measured":measured_calls,"intake_cost_calls_measured":cost_calls,"commands":receipts,"requested_outcomes":outcomes,"verified_outcomes":verified,"tokens_per_verified_outcome":if verified>0 {observed.map(|n|n as f64 / verified as f64)}else{None},"budget_enforcement":"observed stop limit; in-flight provider tokens are not a hard cap","reason":if measured {None}else{Some("provider usage has not been observed")}}),
    )
}

pub fn waiting(conn: &Connection, project: &Project) -> anyhow::Result<Option<String>> {
    if project.policy.token_budget.is_none() && project.policy.cost_budget_usd.is_none() {
        return Ok(None);
    }
    let u = summary(conn, &project.name)?;
    if project
        .policy
        .token_budget
        .is_some_and(|limit| u["tokens"].as_u64().is_some_and(|n| n >= limit))
    {
        return Ok(Some("token_budget_reached".into()));
    }
    if project
        .policy
        .cost_budget_usd
        .is_some_and(|limit| u["estimated_cost_usd"].as_f64().is_some_and(|n| n >= limit))
    {
        return Ok(Some("cost_budget_reached".into()));
    }
    if project.policy.token_budget.is_some()
        && u["intake_calls_measured"].as_i64().unwrap_or(0)
            < u["intake_calls"].as_i64().unwrap_or(0)
    {
        return Ok(Some("budget_usage_unmeasured".into()));
    }
    if project.policy.cost_budget_usd.is_some()
        && (u["intake_cost_calls_measured"].as_i64().unwrap_or(0)
            < u["intake_calls"].as_i64().unwrap_or(0)
            || u["execution_cost_turns_measured"].as_i64().unwrap_or(0)
                < u["execution_turns_measured"].as_i64().unwrap_or(0))
    {
        return Ok(Some("budget_cost_unmeasured".into()));
    }
    // A configured cap cannot silently treat unmeasured finished turns as free.
    let unmeasured:i64=conn.query_row("SELECT count(*) FROM issues i WHERE i.project_group=?1 AND i.execution_state IS NOT NULL AND json_extract(i.execution_state,'$.stage') IN ('verified','waiting','repair') AND NOT EXISTS(SELECT 1 FROM token_ledger l WHERE l.task=i.id)",params![project.name],|r|r.get(0))?;
    let has_attempts: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='task_attempts')",
        [],
        |r| r.get(0),
    )?;
    let unmeasured_attempts: i64 = if has_attempts {
        conn.query_row("SELECT count(*) FROM task_attempts a JOIN issues i ON i.id=a.card WHERE i.project_group=?1 AND a.ended_at IS NOT NULL AND NOT EXISTS(SELECT 1 FROM token_ledger l WHERE l.task=a.card AND l.session=a.worker AND l.ts>=a.started_at AND l.ts<=a.ended_at)", [project.name.as_str()], |r|r.get(0))?
    } else {
        0
    };
    if unmeasured > 0 || unmeasured_attempts > 0 {
        return Ok(Some("budget_usage_unmeasured".into()));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn project_budget_does_not_hide_unmeasured_intake_cost_or_a_later_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Store::open(&dir.path().join("db")).unwrap();
        db.write(|c| {
            let policy=serde_json::from_value(json!({"repository":"/repo","coordinator":{"provider":"claude","model":"haiku"},"executor":{"provider":"claude","model":"sonnet"},"verify_command":"./verify.sh","cost_budget_usd":10})).unwrap();
            super::super::store::save(c,"coverage",0,&policy,"test").map_err(super::super::store::sql_error)?;
            let mut project=super::super::store::get(c,"coverage").unwrap().unwrap();
            let (id,_)=super::super::intake::receive(c,"coverage","receipt","Create verified report").unwrap();
            c.execute("UPDATE cmd_history SET intake_attempts=2,intake_result=?2 WHERE id=?1",params![id,json!({"telemetry":{"attempt_usage":[{"usage":{"input_tokens":1,"output_tokens":1},"total_cost_usd":0.1},{"usage":{"input_tokens":1,"output_tokens":1}}]}}).to_string()])?;
            assert_eq!(waiting(c,&project).unwrap().as_deref(),Some("budget_cost_unmeasured"));
            project.policy.cost_budget_usd=None;project.policy.token_budget=Some(1000);
            c.execute("INSERT INTO issues(id,title,status,project_group,created,updated) VALUES('C-1','Retried output','todo','coverage',1,1)",[])?;
            crate::db::attempts::record_lease_change(c,"C-1",None,Some("executor"),1,"doing","project-driver",None,10)?;
            crate::db::attempts::record_lease_change(c,"C-1",Some("executor"),None,1,"blocked","project-driver",None,20)?;
            c.execute("INSERT INTO token_ledger(ts,session,conversation,task,input,output) VALUES(15,'executor','first','C-1',1,1)",[])?;
            assert_eq!(waiting(c,&project).unwrap(),None);
            crate::db::attempts::record_lease_change(c,"C-1",None,Some("executor"),2,"doing","project-driver",None,30)?;
            crate::db::attempts::record_lease_change(c,"C-1",Some("executor"),None,2,"blocked","project-driver",None,40)?;
            assert_eq!(waiting(c,&project).unwrap().as_deref(),Some("budget_usage_unmeasured"));
            Ok(crate::db::WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
    }
    #[test]
    fn project_usage_unknown_is_not_zero_and_observed_cap_stops_claims() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Store::open(&dir.path().join("db")).unwrap();
        db.write(|c| {
            let policy=serde_json::from_value(json!({"repository":"/repo","coordinator":{"provider":"claude","model":"haiku"},"executor":{"provider":"claude","model":"sonnet"},"verify_command":"./verify.sh","token_budget":100})).unwrap();
            super::super::store::save(c,"usage",0,&policy,"test").map_err(super::super::store::sql_error)?;
            let project=super::super::store::get(c,"usage").unwrap().unwrap();
            assert_eq!(summary(c,"usage").unwrap()["tokens"],Value::Null);
            assert_eq!(waiting(c,&project).unwrap(),None);
            let (id,_)=super::super::intake::receive(c,"usage","receipt","Create verified report").unwrap();
            c.execute("UPDATE cmd_history SET intake_attempts=1,intake_result=?2 WHERE id=?1",params![id,json!({"telemetry":{"attempt_usage":[{"usage":{"input_tokens":40,"output_tokens":80}}]}}).to_string()])?;
            let usage=summary(c,"usage").unwrap();
            assert_eq!(usage["tokens"],120);
            assert_eq!(usage["intake_calls_measured"],1);
            assert_eq!(usage["execution_turns_measured"],0);
            assert_eq!(usage["cost_usd"],Value::Null);
            assert_eq!(usage["tokens_per_verified_outcome"],Value::Null);
            assert_eq!(waiting(c,&project).unwrap().as_deref(),Some("token_budget_reached"));
            let mut project=project;project.policy.token_budget=Some(1000);
            c.execute("INSERT INTO issues(id,title,status,project_group,created,updated,execution_state) VALUES('U-1','Completed but unmeasured','verified','usage',1,1,'{\"stage\":\"verified\"}')",[])?;
            assert_eq!(waiting(c,&project).unwrap().as_deref(),Some("budget_usage_unmeasured"));
            Ok(crate::db::WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
    }
    #[test]
    fn project_usage_prices_and_executor_owned_unattributed_rows_preserve_claims() {
        let dir = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(dir.path());
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        let env = dir.path().join("sessions/executor.env");
        std::fs::write(&env, "CC_DIR=/repo\nCC_PROJECT=coverage\nCC_BOARD_CARD=A\n").unwrap();
        crate::fanout_workspace::save(
            dir.path(),
            "executor",
            &crate::fanout_workspace::Workspace {
                repo: "/repo".into(),
                path: dir
                    .path()
                    .join("worktrees/executor")
                    .to_string_lossy()
                    .into(),
                branch: "amux/fanout/executor".into(),
                base: "a".repeat(40),
            },
        )
        .unwrap();
        let db = crate::db::Store::open(&dir.path().join("db")).unwrap();
        let home = dir.path().to_path_buf();
        db.write(move|c| {
            let policy=serde_json::from_value(json!({"repository":"/repo","enabled":true,"coordinator":{"provider":"codex","model":"gpt-6-astra"},"executor":{"provider":"codex","model":"gpt-6-astra"},"verify_command":"./verify.sh","cost_budget_usd":1})).unwrap();
            super::super::store::save(c,"coverage",0,&policy,"test").unwrap();
            let execution=super::super::planner::Execution{worker:"executor".into(),stage:"working".into(),..Default::default()};
            c.execute("INSERT INTO issues(id,title,status,session,project_group,created,updated,execution_state) VALUES('A','Outcome','doing','executor','coverage',1,1,?1)",[serde_json::to_string(&execution).unwrap()])?;
            let row=crate::db::board_store::get_issue(c,"A")?.unwrap();
            let mut execution=execution;execution.attempt=1;execution.generation=1;execution.input_hash=super::super::planner::input_hash(&row);
            super::super::planner::save_execution(c,&row,&execution,"project.execution").unwrap();
            c.execute("INSERT INTO issues(id,title,status,project_group,created,updated) VALUES('F','Foreign','todo','other',1,1)",[])?;
            let (receipt,_)=super::super::intake::receive(c,"coverage","original","One outcome").unwrap();
            c.execute("UPDATE cmd_history SET capture_pending=0,card_id='A' WHERE id=?1",[receipt])?;
            for (session,task,tokens) in [("executor","A",100),("executor","",200),("foreign","",900),("","",900),("executor","F",900)] {
                c.execute("INSERT INTO token_ledger(ts,session,conversation,task,model,input,cost_usd) VALUES(1,?1,'coverage-fixture',?2,'gpt-6-astra',?3,0)",params![session,task,tokens])?;
            }
            let mut explain=c.prepare(&format!("EXPLAIN QUERY PLAN {EXECUTION_USAGE_SQL}"))?;
            let plan=explain.query_map(params!["coverage", "[\"executor\"]"],|r|r.get::<_,String>(3))?.collect::<rusqlite::Result<Vec<_>>>()?.join("\n");
            assert!(plan.contains("SEARCH l USING INDEX idx_ledger_task"),"{plan}");
            assert!(plan.contains("SEARCH token_ledger USING INDEX idx_ledger_session"),"{plan}");
            assert!(!plan.contains("SCAN l ") && !plan.contains("SCAN token_ledger"),"{plan}");
            let before=summary(c,"coverage").unwrap();assert_eq!(before["tokens"],300);assert_eq!(before["executor_unattributed_tokens"],200);assert_eq!(before["executor_unattributed_turns_measured"],1);assert_eq!(before["execution_attempt_turns_measured"],1);
            assert_eq!(before["estimated_cost_usd"],Value::Null);assert_eq!(before["cost_measured"],false);assert_eq!(before["execution_cost_turns_measured"],0);assert_eq!(before["cost_reason"],"execution model rates missing");assert_eq!(before["requested_outcomes"],1);
            let mut project=super::super::store::get(c,"coverage").unwrap().unwrap();assert_eq!(waiting(c,&project).unwrap().as_deref(),Some("budget_cost_unmeasured"));
            assert!(!crate::api::projects::executor_steering_allowed(c,"executor").unwrap());
            // True configured free rates remain measured zero, not unknown.

            c.execute("UPDATE token_ledger SET model='qwen3.8:27b',cost_usd=9 WHERE session='executor'",[])?;
            let free=summary(c,"coverage").unwrap();assert_eq!(free["cost_measured"],true);assert_eq!(free["estimated_cost_usd"],0.0);assert_eq!(free["execution_cost_turns_measured"],2);
            assert!(crate::api::projects::executor_steering_allowed(c,"executor").unwrap());
            c.execute("UPDATE token_ledger SET model='gpt-6-astra' WHERE task='A'",[])?;
            assert_eq!(summary(c,"coverage").unwrap()["estimated_cost_usd"],Value::Null,"mixed coverage is not a full dollar total");
            c.execute("UPDATE token_ledger SET model='qwen3.8:27b' WHERE task='A'",[])?;
            project.policy.paused=true;super::super::store::save(c,"coverage",project.revision,&project.policy,"test").unwrap();
            assert!(!crate::api::projects::executor_steering_allowed(c,"executor").unwrap());
            std::fs::rename(home.join("sessions/executor.env"),home.join("sessions/executor.env.reaped")).unwrap();assert_eq!(summary(c,"coverage").unwrap()["tokens"],300);
            // Two validated records pointing at the same physical worktree are ambiguous.
            #[cfg(unix)] {
                std::fs::create_dir_all(home.join("worktrees/executor")).unwrap();
                std::os::unix::fs::symlink(home.join("worktrees/executor"),home.join("worktrees/alias")).unwrap();
                std::fs::write(home.join("sessions/alias.env"),"CC_DIR=/repo\nCC_PROJECT=coverage\n").unwrap();
                crate::fanout_workspace::save(&home,"alias",&crate::fanout_workspace::Workspace{repo:"/repo".into(),path:home.join("worktrees/alias").to_string_lossy().into(),branch:"amux/fanout/alias".into(),base:"a".repeat(40)}).unwrap();
                assert_eq!(summary(c,"coverage").unwrap()["tokens"],100,"same physical workspace cannot have two owners");
                std::fs::remove_file(home.join("worktrees/alias")).unwrap();
                assert_eq!(summary(c,"coverage").unwrap()["tokens"],300);
            }
            c.execute("UPDATE issues SET execution_state=?1 WHERE id='F'",[serde_json::to_string(&execution).unwrap()])?;
            assert_eq!(summary(c,"coverage").unwrap()["tokens"],100,"ambiguous executor ownership excluded");
            assert_eq!(c.query_row("SELECT COUNT(*) FROM token_ledger WHERE task=''",[],|r|r.get::<_,i64>(0))?,3,"claim-window attribution remains untouched");
            Ok(crate::db::WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
    }
}
