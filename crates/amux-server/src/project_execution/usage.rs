//! Coverage travels with usage. Ledger dollar values are price estimates,
//! never presented as billed subscription spend or as a provider hard cap.
use super::store::Project;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

pub fn summary(conn: &Connection, name: &str) -> anyhow::Result<Value> {
    let (turns,tokens,cost):(i64,Option<i64>,Option<f64>)=conn.query_row("SELECT count(*),sum(l.input+l.output+l.cache_read+l.cache_write),sum(l.cost_usd) FROM token_ledger l JOIN issues i ON i.id=l.task WHERE i.project_group=?1",[name],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
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
    let observed = measured.then_some(tokens.unwrap_or(0) as u64 + intake_tokens);
    Ok(
        json!({"measured":measured,"n_considered":attempts+calls,"tokens":observed,"cost_usd":Value::Null,"estimated_cost_usd":(turns>0 || cost_calls>0).then_some(cost.unwrap_or(0.0)+intake_cost),"execution_turns_measured":turns,"execution_attempts":attempts,"intake_calls":calls,"intake_calls_measured":measured_calls,"intake_cost_calls_measured":cost_calls,"commands":receipts,"requested_outcomes":outcomes,"verified_outcomes":verified,"tokens_per_verified_outcome":if verified>0 {observed.map(|n|n as f64 / verified as f64)}else{None},"budget_enforcement":"observed stop limit; in-flight provider tokens are not a hard cap","reason":if measured {None}else{Some("provider usage has not been observed")}}),
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
        && u["intake_cost_calls_measured"].as_i64().unwrap_or(0)
            < u["intake_calls"].as_i64().unwrap_or(0)
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
}
