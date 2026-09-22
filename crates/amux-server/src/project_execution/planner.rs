//! One read model and claim predicate for project execution and the dashboard.
use super::store;
use crate::db::{board_store as bs, PendingEvent, WriteOutcome};
use amux_core::{
    project::{phase, Phase},
    revision::{EntityType, MutationKind},
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Check {
    pub criterion: String,
    pub command: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Report {
    #[serde(default)]
    pub assets: Vec<super::assets::Asset>,
    pub head: String,
    pub checks: Vec<Check>,
    pub summary: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Execution {
    #[serde(default)]
    pub retained_assets: Vec<super::assets::Retained>,
    pub stage: String,
    #[serde(default)]
    pub retry_grants: Vec<super::task_retry::Grant>,
    #[serde(default)]
    pub verification_retries: Vec<super::task_retry::VerificationGrant>,
    #[serde(default)]
    pub verification_retry_pending: bool,
    pub attempt: u32,
    pub generation: i64,
    pub input_hash: String,
    pub worker: String,
    pub delivery_id: String,
    pub waiting: Option<String>,
    #[serde(default)]
    pub wait_category: Option<String>,
    #[serde(default)]
    pub output_wait: Option<super::outputs::OutputWait>,
    #[serde(default)]
    pub last_failure: Option<String>,
    pub report: Option<Report>,
    pub usage: Option<Value>,
    pub observed_at: i64,
    #[serde(default)]
    pub suspended: bool,
}
impl Execution {
    pub fn attempt_limit(&self, default: u32) -> u32 {
        self.retry_grants
            .last()
            .map_or(default, |g| g.allowed_through)
    }
}
pub fn execution(conn: &Connection, id: &str) -> anyhow::Result<Execution> {
    let raw: Option<String> = conn.query_row(
        "SELECT execution_state FROM issues WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    raw.map(|s| serde_json::from_str(&s))
        .transpose()
        .map(|v| v.unwrap_or_default())
        .map_err(Into::into)
}
/// Immutable terminal delivery evidence, captured before observing provider state.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DeliveryReceipt {
    pub completed_at: f64,
    pub submitted_at: f64,
    pub outcome: String,
}
pub(crate) fn settled_delivery(
    conn: &Connection,
    e: &Execution,
) -> rusqlite::Result<Option<DeliveryReceipt>> {
    let pending:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM steering_queue WHERE session=?1 AND (id=?2 OR delivering_since IS NOT NULL))",params![e.worker,e.delivery_id],|r|r.get(0))?;
    if pending {
        return Ok(None);
    }
    conn.query_row("SELECT delivered_at,outcome,COALESCE((SELECT MAX(ts) FROM session_events WHERE session=?2 AND type='project.delivery_started' AND json_extract(data,'$.delivery_id')=?1),delivered_at) FROM steering_history WHERE id=?1 AND session=?2 AND (outcome LIKE 'sent%' OR outcome LIKE 'interrupted:%')",params![e.delivery_id,e.worker],|r|Ok(DeliveryReceipt{completed_at:r.get(0)?,outcome:r.get(1)?,submitted_at:r.get(2)?})).optional()
}

/// Run only inside the existing serialized store writer. A newer durable claim,
/// not a temporary delivery hold, is the authority to void an unsent old packet.
pub(crate) fn settle_superseded_packets(conn: &Connection) -> rusqlite::Result<WriteOutcome> {
    let candidates = {
        let mut q = conn.prepare("SELECT id,session FROM steering_queue WHERE guard='project-execution' AND delivering_since IS NULL")?;
        let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut settled = 0;
    for (id, worker) in candidates {
        // Existing structured execution events bind delivery identity to task,
        // project, generation and input. Never parse packet prose or ID prefixes.
        let witnesses = {
            let mut q = conn.prepare("SELECT data FROM session_events WHERE session=?1 AND source='project-driver' AND json_valid(data) AND json_extract(data,'$.execution.delivery_id')=?2")?;
            let rows = q.query_map(params![worker, id], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut binding = None;
        let mut proven = !witnesses.is_empty();
        for raw in witnesses {
            let witness = (|| {
                let v: Value = serde_json::from_str(&raw).ok()?;
                let e: Execution = serde_json::from_value(v.get("execution")?.clone()).ok()?;
                if e.worker != worker || e.delivery_id != id || e.input_hash.is_empty() {
                    return None;
                }
                Some((
                    v.get("task")?.as_str()?.to_string(),
                    v.get("project_group")?.as_str()?.to_string(),
                    e.generation,
                    e.input_hash,
                ))
            })();
            let Some(witness) = witness else {
                proven = false;
                break;
            };
            if binding.as_ref().is_some_and(|b| b != &witness) {
                proven = false;
                break;
            }
            binding = Some(witness);
        }
        if !proven {
            continue;
        }
        let Some((task, project, old_generation, _old_input)) = binding else {
            continue;
        };
        // get_issue filters deleted IS NULL in this same writer transaction;
        // soft-deleted cards cannot authorize packet settlement.
        let Some(row) = bs::get_issue(conn, &task)? else {
            continue;
        };
        let current = execution(conn, &task).map_err(store::sql_error)?;
        if store::get(conn, &project)
            .map_err(store::sql_error)?
            .is_none()
            || old_generation <= 0
            || row.archived != 0
            || row.session.as_deref() != Some(worker.as_str())
            || row.project_group.as_deref() != Some(project.as_str())
            || current.worker != worker
            || current.input_hash != input_hash(&row)
            || current.generation <= old_generation
            || current.delivery_id.is_empty()
            || current.delivery_id == id
        {
            continue;
        }
        // A conflicting receipt is not permission to overwrite history or drop
        // queued bytes. Successful transactions leave no queue/history overlap.
        let history: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM steering_history WHERE id=?1)",
            [&id],
            |r| r.get(0),
        )?;
        if history {
            continue;
        }
        let now = crate::config::now_f64();
        let inserted = conn.execute(
            "INSERT INTO steering_history(id,session,text,queued_at,delivered_at,outcome,guard,sender) SELECT id,session,text,queued_at,?3,'void:project-execution-superseded',guard,sender FROM steering_queue WHERE id=?1 AND session=?2 AND guard='project-execution' AND delivering_since IS NULL",
            params![id,worker,now],
        )?;
        if inserted == 0 {
            continue;
        }
        conn.execute("DELETE FROM steering_queue WHERE id=?1 AND session=?2 AND guard='project-execution' AND delivering_since IS NULL", params![id,worker])?;
        conn.execute("INSERT OR IGNORE INTO session_events(ts,session,type,data,idem,source) VALUES(?1,?2,'message.voided',?3,?4,'steering')", params![now,worker,json!({"id":id,"task":task,"project":project,"old_generation":old_generation,"current_generation":current.generation,"current_delivery_id":current.delivery_id,"reason":"project-execution-superseded","delivered":false,"measured":true,"n_considered":1}).to_string(),format!("void:{id}")])?;
        tracing::info!(session=%worker,delivery_id=%id,task=%task,old_generation,current_generation=current.generation,measured=true,n_considered=1,verdict="project_execution_packet_superseded","superseded unsent packet retained in history; not delivered");
        settled += inserted;
    }
    Ok(WriteOutcome {
        applied: settled > 0,
        events: vec![],
    })
}

pub fn input_hash(row: &bs::IssueRow) -> String {
    hex::encode(Sha256::digest(
        json!([
            row.project_group,
            row.title,
            row.desc,
            row.acceptance_criteria,
            row.depends_on,
            row.next_action
        ])
        .to_string()
        .as_bytes(),
    ))
}
#[derive(Debug, Clone, Serialize)]
pub struct CardPlan {
    pub id: String,
    pub phase: Phase,
    pub action: String,
    pub waiting_reason: Option<String>,
    pub waiting_label: Option<String>,
    pub execution: Execution,
}

/// Short presentation is harness-owned; arbitrary command output stays in details.
fn waiting_label(reason: &str, e: &Execution) -> String {
    match reason {
        "project_disabled" => "Project disabled",
        "project_paused" => "Project paused",
        "attempts_exhausted" => "Attempt limit reached",
        "authorization_required" => "Authorization required",
        "requirements_changed" => "Requirements changed",
        "intake_required" => "Intake required",
        "executor_capacity" => "Waiting for executor capacity",
        "token_budget_reached" => "Token budget reached",
        "cost_budget_reached" => "Cost budget reached",
        "budget_usage_unmeasured" => "Token usage unmeasured",
        "budget_cost_unmeasured" => "Cost unmeasured",
        _ if reason.starts_with("refusing to spawn a worker:") => "Spawn blocked",
        _ if reason.starts_with("required_output:") => "Required output",
        _ if reason == "executor_returned_without_result" => "Executor returned without result",
        _ if reason == "executor_stopped_before_result" => "Executor stopped before result",
        _ if reason.starts_with("invalid_dependency:") => "Invalid dependency",
        _ if e.report.is_some()
            && e.waiting.as_deref() == Some(reason)
            && e.wait_category.is_none() =>
        {
            "Verification failed"
        }
        _ => "Execution held",
    }
    .into()
}

pub fn plan(conn: &Connection, project: &store::Project) -> anyhow::Result<Vec<CardPlan>> {
    let rows = bs::project_issues(conn, &project.name)?;
    let budget_wait = super::usage::waiting(conn, project)?;
    let states = rows
        .iter()
        .map(|r| execution(conn, &r.id))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let used = states
        .iter()
        .filter(|s| {
            matches!(
                s.stage.as_str(),
                "reserved" | "working" | "reported" | "verifying"
            )
        })
        .count();
    let mut available = project.policy.max_executors.saturating_sub(used);
    let mut result = Vec::new();
    for (row, state) in rows.iter().zip(states) {
        let phase =
            if row.item_type == "epic" && !row.depends_on.is_empty() && row.status != "verified" {
                if rows.iter().any(|r| {
                    row.depends_on.contains(&r.id)
                        && matches!(r.status.as_str(), "doing" | "review" | "done")
                }) {
                    Phase::Working
                } else {
                    Phase::Ready
                }
            } else {
                phase(&row.status, bs::has_execution_details(row))
            };
        let mut action = "wait";
        let output_continuation = state.stage == "waiting"
            && state
                .output_wait
                .as_ref()
                .is_some_and(|w| w.continued_generation.is_none());
        let stale_requirements = !state.stage.is_empty() && state.input_hash != input_hash(row);
        let waiting = if phase == Phase::Verified || phase == Phase::Closed {
            None
        } else if !project.policy.enabled {
            Some("project_disabled".into())
        } else if project.policy.paused {
            Some("project_paused".into())
        } else if stale_requirements {
            if let Some(reason) = &budget_wait {
                Some(reason.clone())
            } else if available == 0 {
                Some("executor_capacity".into())
            } else {
                available -= 1;
                action = "claim";
                None
            }
        } else if state.waiting.is_some() && state.stage != "repair" && !output_continuation {
            state.waiting.clone()
        } else if !bs::has_execution_details(row) && row.item_type != "epic" {
            Some("intake_required".into())
        } else if row
            .ask_type
            .as_deref()
            .is_some_and(|v| matches!(v, "spend" | "budget" | "customer_outbound"))
            && row.status == "needsyou"
        {
            Some("authorization_required".into())
        } else if let Some(blocker) = super::graph::readiness(conn, row)?.blocker() {
            // One shared predicate for edges, verified outputs and holds; invalid edges stay visible.
            Some(blocker)
        } else if output_continuation {
            if let Some(reason) = &budget_wait {
                Some(reason.clone())
            } else if available == 0 {
                Some("executor_capacity".into())
            } else {
                available -= 1;
                action = "resume_outputs";
                None
            }
        } else if row.item_type == "epic" {
            if !row.depends_on.is_empty() {
                action = "complete_epic";
                None
            } else {
                Some("intake_required".into())
            }
        } else if matches!(state.stage.as_str(), "reported" | "verifying") {
            action = "verify";
            None
        } else if state.stage == "reserved" {
            action = "deliver";
            None
        } else if state.stage == "working" {
            action = "observe";
            None
        } else if state.attempt >= state.attempt_limit(project.policy.max_attempts) {
            Some("attempts_exhausted".into())
        } else if phase == Phase::Unrecognized {
            Some("unrecognized_status".into())
        } else if budget_wait.is_some() {
            budget_wait.clone()
        } else if available == 0 {
            Some("executor_capacity".into())
        } else {
            available -= 1;
            action = "claim";
            None
        };
        // Execution truth owns the display too: an idle wait is not work.
        let phase = if waiting.is_some()
            && !matches!(
                phase,
                Phase::Verified | Phase::Closed | Phase::Intake | Phase::Unrecognized
            ) {
            Phase::Waiting
        } else {
            phase
        };
        result.push(CardPlan {
            id: row.id.clone(),
            phase,
            action: action.into(),
            waiting_label: waiting.as_deref().map(|r| waiting_label(r, &state)),
            waiting_reason: waiting,
            execution: state,
        });
    }
    Ok(result)
}

pub fn save_execution(
    conn: &Connection,
    row: &bs::IssueRow,
    state: &Execution,
    event: &str,
) -> anyhow::Result<WriteOutcome> {
    conn.execute(
        "UPDATE issues SET execution_state=?2,updated=?3,rev=rev+1,version=version+1 WHERE id=?1",
        params![
            row.id,
            serde_json::to_string(state)?,
            chrono::Utc::now().timestamp()
        ],
    )?;
    let terminal_attempt_status = match state.stage.as_str() {
        "reported" => Some("review"),
        "verified" => Some("verified"),
        "waiting" | "repair" => Some("blocked"),
        _ => None,
    };
    if event == "project.claimed" {
        crate::db::attempts::record_lease_change(
            conn,
            &row.id,
            row.lease_owner.as_deref(),
            Some(&state.worker),
            state.generation,
            "doing",
            "project-driver",
            None,
            chrono::Utc::now().timestamp(),
        )?;
    } else if let Some(status) = terminal_attempt_status {
        crate::db::attempts::record_lease_change(
            conn,
            &row.id,
            row.lease_owner.as_deref(),
            None,
            state.generation,
            status,
            "project-driver",
            state.waiting.as_deref(),
            chrono::Utc::now().timestamp(),
        )?;
        conn.execute(
            "UPDATE issues SET lease_owner=NULL,lease_expires_at=NULL WHERE id=?1",
            [&row.id],
        )?;
    }
    // Session/terminal projections use this existing causal identity. The project
    // transition alone is not consumed by them and would show active-without-card.
    if matches!(event, "project.claimed" | "project.outputs_continued") {
        conn.execute(
            "INSERT INTO session_events(ts,session,type,data,source) VALUES(?1,?2,'task.claimed',?3,'project-driver')",
            params![crate::config::now_f64(), state.worker,
                json!({"issue":row.id,"status":"doing","from":row.status,"continuation":event=="project.outputs_continued"}).to_string()],
        )?;
    }
    // A valid structured response is positive receipt evidence, even if the
    // transport died after typing but before moving its claim to history.
    if matches!(
        event,
        "project.reported" | "project.waiting" | "project.outputs_declared"
    ) {
        let text: Option<String> = conn.query_row(
            "SELECT text FROM steering_queue WHERE id=?1 AND session=?2 AND guard='project-execution'",
            params![state.delivery_id, state.worker], |r| r.get(0),
        ).optional()?;
        let acknowledged=conn.execute(
            "INSERT OR IGNORE INTO steering_history(id,session,text,queued_at,delivered_at,outcome,guard,sender) SELECT id,session,?4,queued_at,?1,'sent:project-result',guard,sender FROM steering_queue WHERE id=?2 AND session=?3 AND guard='project-execution'",
            params![crate::config::now_f64(), state.delivery_id, state.worker,
                text.as_deref().map(crate::api::session_verbs::redact_secrets)],
        )?;
        conn.execute(
            "DELETE FROM steering_queue WHERE id=?1 AND session=?2 AND guard='project-execution'",
            params![state.delivery_id, state.worker],
        )?;
        if acknowledged > 0 {
            tracing::info!(session=%state.worker,delivery_id=%state.delivery_id,verdict="project_delivery_acknowledged",measured=true,n_considered=acknowledged,"structured result settled its exact execution delivery");
        }
    }
    let payload = json!({"project_group":row.project_group,"task":row.id,"execution":state});
    conn.execute("INSERT INTO session_events(ts,session,type,data,source) VALUES(?1,?2,?3,?4,'project-driver')",params![crate::config::now_f64(),state.worker,event,payload.to_string()])?;
    tracing::info!(project=?row.project_group,task=%row.id,stage=%state.stage,waiting=?state.waiting,verdict=event,measured=true,n_considered=1,"project execution transition");
    Ok(WriteOutcome {
        applied: true,
        events: vec![PendingEvent {
            entity_type: EntityType::Task,
            entity_id: row.id.clone(),
            mutation: MutationKind::Updated,
            payload: None,
        }],
    })
}

/// Reservation and concurrency check share the SQLite writer transaction. Only
/// this function may claim project work; the legacy dispatchers exclude it.
pub fn claim(conn: &Connection, project: &str, id: &str) -> anyhow::Result<WriteOutcome> {
    let project = store::get(conn, project)?.ok_or_else(|| anyhow::anyhow!("project not found"))?;
    let item = plan(conn, &project)?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| anyhow::anyhow!("task not in project"))?;
    if item.action != "claim" {
        return Ok(WriteOutcome {
            applied: false,
            events: vec![],
        });
    }
    let row = bs::get_issue(conn, id)?.ok_or_else(|| anyhow::anyhow!("task missing"))?;
    let mut state = item.execution;
    if !state.stage.is_empty() && state.input_hash != input_hash(&row) {
        state.attempt = 0;
        state.retry_grants.clear();
        state.verification_retries.clear();
        state.verification_retry_pending = false;
        state.output_wait = None;
        state.wait_category = None;
        state.report = None;
        state.retained_assets.clear();
    }
    state.attempt += 1;
    state.generation += 1;
    state.input_hash = input_hash(&row);
    state.stage = "reserved".into();
    state.suspended = false;
    state.observed_at = chrono::Utc::now().timestamp();
    if state.worker.is_empty() {
        state.worker = format!(
            "px-{}-{}",
            project.name.chars().take(24).collect::<String>(),
            &hex::encode(Sha256::digest(id.as_bytes()))[..10]
        );
    }
    state.delivery_id = format!("project:{}:{}:{}", project.name, id, state.generation);
    state.last_failure = state.waiting.take().or(state.last_failure);
    state.report = None;
    state.verification_retry_pending = false;
    conn.execute("UPDATE issues SET status='doing',session=?2,lease_owner=?2,lease_generation=?3,lease_acquired_at=?4,lease_heartbeat_at=?4,lease_expires_at=?5 WHERE id=?1 AND project_group=?6",params![id,state.worker,state.generation,chrono::Utc::now().timestamp(),chrono::Utc::now().timestamp()+300,project.name])?;
    save_execution(conn, &row, &state, "project.claimed")
}

pub(crate) fn validate_report(row: &bs::IssueRow, report: &Report) -> anyhow::Result<()> {
    let criteria: Vec<String> =
        serde_json::from_str(row.acceptance_criteria.as_deref().unwrap_or("[]"))?;
    anyhow::ensure!(
        !report.head.is_empty()
            && report.head.bytes().all(|c| c.is_ascii_hexdigit())
            && report.head.len() == 40,
        "report needs exact commit SHA"
    );
    super::assets::validate_manifest(&report.assets)?;
    anyhow::ensure!(
        !criteria.is_empty()
            && report.checks.len() == criteria.len()
            && criteria.iter().all(|c| report
                .checks
                .iter()
                .filter(|check| &check.criterion == c && !check.command.trim().is_empty())
                .count()
                == 1),
        "each current criterion needs exactly one executable check"
    );
    Ok(())
}

pub fn record_report(
    conn: &Connection,
    project: &str,
    id: &str,
    worker: &str,
    generation: i64,
    hash: &str,
    report: &Report,
) -> anyhow::Result<WriteOutcome> {
    let row = bs::get_issue(conn, id)?.ok_or_else(|| anyhow::anyhow!("task missing"))?;
    anyhow::ensure!(
        row.project_group.as_deref() == Some(project),
        "outside project"
    );
    let mut state = execution(conn, id)?;
    anyhow::ensure!(
        state.worker == worker
            && state.generation == generation
            && state.input_hash == hash
            && input_hash(&row) == hash,
        "stale or foreign execution report"
    );
    if state.report.as_ref() == Some(report) {
        return Ok(WriteOutcome {
            applied: false,
            events: vec![],
        });
    }
    anyhow::ensure!(
        matches!(state.stage.as_str(), "reserved" | "working"),
        "claim no longer accepts reports"
    );
    if let Err(error) = validate_report(&row, report) {
        tracing::warn!(
            project,
            task = id,
            worker,
            generation,
            measured = true,
            n_considered = report.assets.len(),
            verdict = "project.report_contract_refused",
            %error,
            "project report refused before state mutation; exact criteria, checks and retained assets are mandatory for new completions"
        );
        return Err(error);
    }
    let policy = store::get(conn, project)?.ok_or_else(|| anyhow::anyhow!("project missing"))?;
    let criteria: Vec<String> =
        serde_json::from_str(row.acceptance_criteria.as_deref().unwrap_or("[]"))?;
    super::acceptance::contract_binding(&criteria, report, policy.policy.acceptance.as_ref())?;
    let workspace = if policy.policy.worktree {
        let workspace=crate::fanout_workspace::load(&crate::config::amux_home(),worker)
            .ok_or_else(||anyhow::anyhow!("registered executor workspace missing; restore its workspace record before reporting"))?;
        anyhow::ensure!(
            crate::fanout_workspace::same_repository(&workspace.repo, &policy.policy.repository)
                && workspace.branch == format!("amux/fanout/{worker}"),
            "registered workspace does not match project executor"
        );
        workspace
    } else {
        crate::fanout_workspace::Workspace {
            repo: policy.policy.repository.clone(),
            path: policy.policy.repository.clone(),
            branch: format!("shared-checkout/{worker}"),
            base: String::new(),
        }
    };
    if let Err(error) = super::driver::validated_verification_commands(
        &workspace,
        &policy.policy.verify_command,
        report,
    ) {
        tracing::warn!(project,task=id,worker,generation,measured=true,n_considered=report.checks.len()+1,verdict="project.report_commands_refused",%error,"report unchanged; correct candidate-relative commands and resubmit this generation");
        anyhow::bail!("report command refused before verification; correct the command and resubmit the same generation: {error}");
    }
    state.stage = "reported".into();
    state.report = Some(report.clone());
    state.waiting = None;
    conn.execute("UPDATE issues SET status='review' WHERE id=?1", [id])?;
    save_execution(conn, &row, &state, "project.reported")
}

/// Rechecked in the steering writer immediately before any provider delivery.
pub fn delivery_current(
    conn: &Connection,
    project: &str,
    worker: &str,
    delivery: &str,
) -> anyhow::Result<bool> {
    let Some(p) = store::get(conn, project)? else {
        return Ok(false);
    };
    if p.policy.paused || !p.policy.enabled {
        return Ok(false);
    }
    for row in bs::project_issues(conn, project)? {
        let e = execution(conn, &row.id)?;
        if e.worker == worker
            && e.delivery_id == delivery
            && matches!(e.stage.as_str(), "reserved" | "working")
            && e.input_hash == input_hash(&row)
            && super::outputs::ready(conn, &row)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
pub(crate) fn register_test_workspace(worker: &str, repo: &str) {
    let home = crate::config::amux_home();
    crate::fanout_workspace::save(
        &home,
        worker,
        &crate::fanout_workspace::Workspace {
            repo: repo.into(),
            path: home.join("worktrees").join(worker).to_string_lossy().into(),
            branch: format!("amux/fanout/{worker}"),
            base: "a".repeat(40),
        },
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture_asset() -> super::super::assets::Asset {
        super::super::assets::Asset {
            path: "report.md".into(),
            sha256: "0".repeat(64),
        }
    }
    fn fixture() -> (tempfile::TempDir, crate::db::Store) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Store::open(&dir.path().join("db")).unwrap();
        db.write(|c| {
            let policy=serde_json::from_value(json!({"repository":"/repo","coordinator":{"provider":"claude","model":"haiku"},"executor":{"provider":"codex","model":"gpt-configured"},"verify_command":"./verify.sh","enabled":true,"max_executors":2})).unwrap();
            store::save(c,"sample",0,&policy,"test").map_err(store::sql_error)?;
            for (id,deps) in [("A","[]"),("B","[]"),("C","[\"A\"]")] {
                c.execute("INSERT INTO issues(id,title,desc,status,type,project_group,created,updated,next_action,acceptance_criteria,depends_on) VALUES(?1,'Specific output','Implement a concrete output','todo','code','sample',1,1,'Implement and test output','[\"Output passes its test\"]',?2)",params![id,deps])?;
            }
            Ok(WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
        (dir, db)
    }
    #[test]
    fn project_superseded_packet_reconciliation_preserves_identity_and_guards() {
        for case in [
            "superseded",
            "current",
            "inflight",
            "owner",
            "foreign-worker",
            "foreign-project",
            "unproven",
            "input-changed",
            "history-conflict",
            "ambiguous",
            "deleted",
            "archived",
            "equal-generation",
        ] {
            let (_dir, db) = fixture();
            db.write(move |c| {
                claim(c,"sample","A").unwrap();
                let old = execution(c,"A").unwrap();
                let row = bs::get_issue(c,"A")?.unwrap();
                let mut failed = old.clone();failed.stage="repair".into();failed.waiting=Some("executor_stopped_before_result".into());
                save_execution(c,&row,&failed,"project.execution").unwrap();
                assert!(claim(c,"sample","A").unwrap().applied);
                let mut current = execution(c,"A").unwrap();
                let id = match case {"current"=>current.delivery_id.clone(),"unproven"=>"unknown-delivery".into(),_=>old.delivery_id.clone()};
                let guard = if case=="owner" {"project-steering"} else {"project-execution"};
                let text = "Exact original packet α\nsecond line; no rewritten prefix";
                c.execute("INSERT INTO steering_queue(id,session,text,queued_at,guard,sender,delivering_since) VALUES(?1,?2,?3,123,?4,'project-driver',?5)",params![id,old.worker,text,guard,if case=="inflight"{Some(124.0)}else{None}])?;
                match case {
                    "deleted" => {c.execute("UPDATE issues SET deleted=1 WHERE id='A'",[])?;assert!(bs::get_issue(c,"A")?.is_none(),"shared board read excludes soft-deleted rows");},
                    "archived" => {c.execute("UPDATE issues SET archived=1 WHERE id='A'",[])?;},
                    "equal-generation" => {current.generation=old.generation;let row=bs::get_issue(c,"A")?.unwrap();save_execution(c,&row,&current,"project.execution").unwrap();},
                    "foreign-worker" => {c.execute("UPDATE issues SET session='other-worker' WHERE id='A'",[])?;},
                    "foreign-project" => {c.execute("UPDATE issues SET project_group='other-project' WHERE id='A'",[])?;},
                    "input-changed" => {current.input_hash="stale-input".into();let row=bs::get_issue(c,"A")?.unwrap();save_execution(c,&row,&current,"project.execution").unwrap();},
                    "history-conflict" => {c.execute("INSERT INTO steering_history(id,session,text,delivered_at,outcome) VALUES(?1,?2,'prior receipt',1,'interrupted: prior')",params![id,old.worker])?;},
                    "ambiguous" => {c.execute("INSERT INTO session_events(ts,session,type,data,source) VALUES(1,?1,'project.claimed',?2,'project-driver')",params![old.worker,json!({"task":"B","project_group":"sample","execution":old}).to_string()])?;},
                    _=>{}
                }
                if case=="superseded" {
                    c.execute("INSERT INTO steering_queue(id,session,text,queued_at,guard) VALUES(?1,?2,'current packet',125,'project-execution'),('owner-note',?2,'owner note',126,'project-steering')",params![current.delivery_id,old.worker])?;
                }
                let before = serde_json::to_value(execution(c,"A").unwrap()).unwrap();
                let attempt_rows:i64=c.query_row("SELECT COUNT(*) FROM task_attempts",[],|r|r.get(0))?;
                let result=settle_superseded_packets(c)?;
                assert_eq!(result.applied,case=="superseded","{case}");
                assert_eq!(serde_json::to_value(execution(c,"A").unwrap()).unwrap(),before);
                assert_eq!(c.query_row("SELECT COUNT(*) FROM task_attempts",[],|r|r.get::<_,i64>(0))?,attempt_rows);
                if case=="superseded" {
                    let retained:(String,String,String,f64,String,String,String)=c.query_row("SELECT id,session,text,queued_at,outcome,guard,sender FROM steering_history WHERE id=?1",[&id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?;
                    assert_eq!(retained,(id.clone(),old.worker.clone(),text.into(),123.0,"void:project-execution-superseded".into(),guard.into(),"project-driver".into()));
                    assert_eq!(c.query_row("SELECT COUNT(*) FROM steering_queue WHERE id=?1",[&id],|r|r.get::<_,i64>(0))?,0);
                    assert_eq!(c.query_row("SELECT COUNT(*) FROM steering_queue",[],|r|r.get::<_,i64>(0))?,2,"current packet and owner note retained");
                    assert!(!settle_superseded_packets(c)?.applied);
                    assert_eq!(c.query_row("SELECT COUNT(*) FROM session_events WHERE type='message.voided'",[],|r|r.get::<_,i64>(0))?,1);
                } else {
                    assert_eq!(c.query_row("SELECT text FROM steering_queue WHERE id=?1",[&id],|r|r.get::<_,String>(0))?,text);
                    assert_eq!(c.query_row("SELECT COUNT(*) FROM steering_history WHERE outcome='void:project-execution-superseded'",[],|r|r.get::<_,i64>(0))?,0);
                }
                Ok(WriteOutcome{applied:true,events:vec![]})
            }).unwrap();
        }
    }

    #[test]
    fn project_failure_label_never_uses_a_passing_stdout_prefix() {
        let failure = "tree-revert: OK\nrepository guard: refused invalid source";
        let e = Execution {
            stage: "waiting".into(),
            waiting: Some(failure.into()),
            report: Some(Report {
                head: "a".repeat(40),
                summary: String::new(),
                assets: vec![],
                checks: vec![],
            }),
            ..Default::default()
        };
        assert_eq!(waiting_label(failure, &e), "Verification failed");
        assert_eq!(e.waiting.as_deref(), Some(failure));
        assert_eq!(
            waiting_label("untrusted: PASS", &Execution::default()),
            "Execution held"
        );
        assert_eq!(waiting_label("required_output:B", &e), "Required output");
        for (reason, label) in [
            ("token_budget_reached", "Token budget reached"),
            ("cost_budget_reached", "Cost budget reached"),
            ("budget_usage_unmeasured", "Token usage unmeasured"),
            ("budget_cost_unmeasured", "Cost unmeasured"),
        ] {
            assert_eq!(waiting_label(reason, &e), label);
            assert_eq!(
                waiting_label(
                    &format!("{reason}: unrelated stdout"),
                    &Execution::default()
                ),
                "Execution held"
            );
        }
    }
    #[test]
    fn project_claims_are_atomic_bounded_and_excluded_from_both_legacy_planners() {
        let (_dir, db) = fixture();
        for id in ["A", "B", "A", "C"] {
            db.write(move |c| claim(c, "sample", id).map_err(store::sql_error))
                .unwrap();
        }
        let c = db.read().unwrap();
        let p = store::get(&c, "sample").unwrap().unwrap();
        let plans = plan(&c, &p).unwrap();
        assert_eq!(plans.iter().filter(|p| p.action == "deliver").count(), 2);
        assert_eq!(execution(&c, "A").unwrap().attempt, 1);
        let claims: Vec<(String, String)> = c.prepare(
            "SELECT session,json_extract(data,'$.issue') FROM session_events WHERE type='task.claimed' ORDER BY id"
        ).unwrap().query_map([], |r| Ok((r.get(0)?,r.get(1)?))).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(
            claims,
            vec![
                (execution(&c, "A").unwrap().worker, "A".into()),
                (execution(&c, "B").unwrap().worker, "B".into())
            ]
        );

        assert_eq!(
            plans[2].waiting_reason.as_deref(),
            Some("required_output:A")
        );
        assert!(bs::planning_tasks(&c, bs::ArchivedFilter::ActiveOnly)
            .unwrap()
            .is_empty());
        assert_eq!(
            c.query_row("SELECT count(*) FROM legacy_execution_issues", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn project_result_settles_only_its_exact_delivery_even_after_sender_restart() {
        let (_dir, db) = fixture();
        let _home = crate::api::settings::test_env::set_home(_dir.path());
        db.write(|c| {
            claim(c,"sample","A").map_err(store::sql_error)?;
            let e=execution(c,"A").unwrap();
            register_test_workspace(&e.worker,"/repo");
            c.execute("INSERT INTO steering_queue(id,session,text,queued_at,guard,delivering_since) VALUES(?1,?2,'Task packet API_KEY=fixture-secret',1,'project-execution',2)",params![e.delivery_id,e.worker])?;
            c.execute("INSERT INTO steering_queue(id,session,text,queued_at,guard) VALUES('unrelated',?1,'Owner input',1,'')",[&e.worker])?;
            let report=Report{assets:vec![fixture_asset()],head:"a".repeat(40),summary:"Measured output".into(),checks:vec![Check{criterion:"Output passes its test".into(),command:"./check-output.sh".into()}]};
            assert!(record_report(c,"sample","A",&e.worker,e.generation+1,&e.input_hash,&report).is_err());
            assert_eq!(c.query_row("SELECT count(*) FROM steering_queue",[],|r|r.get::<_,i64>(0))?,2);
            record_report(c,"sample","A",&e.worker,e.generation,&e.input_hash,&report).map_err(store::sql_error)?;
            assert_eq!(c.query_row("SELECT outcome FROM steering_history WHERE id=?1",[&e.delivery_id],|r|r.get::<_,String>(0))?,"sent:project-result");
            assert_eq!(c.query_row("SELECT text FROM steering_history WHERE id=?1",[&e.delivery_id],|r|r.get::<_,String>(0))?,"Task packet API_KEY=REDACTED");
            assert_eq!(c.query_row("SELECT id FROM steering_queue",[],|r|r.get::<_,String>(0))?,"unrelated");
            assert!(!record_report(c,"sample","A",&e.worker,e.generation,&e.input_hash,&report).unwrap().applied);
            Ok(WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
    }
    #[test]
    fn project_interrupted_delivery_can_recover_without_treating_pending_input_as_delivered() {
        let (_dir, db) = fixture();
        db.write(|c| {
            claim(c, "sample", "A").map_err(store::sql_error)?;
            let state = execution(c, "A").unwrap();
            assert!(settled_delivery(c, &state)?.is_none());
            c.execute("INSERT INTO steering_history(id,session,text,delivered_at,outcome) VALUES(?1,?2,'task packet',1,'interrupted: server restart')", params![state.delivery_id, state.worker])?;
            assert!(settled_delivery(c, &state)?.is_some());
            let wrong_worker = Execution { worker: "another-executor".into(), ..state.clone() };
            assert!(settled_delivery(c, &wrong_worker)?.is_none());
            c.execute("UPDATE steering_history SET outcome='void:stale'", [])?;
            assert!(settled_delivery(c, &state)?.is_none());
            Ok(WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
    }

    #[tokio::test]
    async fn project_attempts_feed_existing_provider_usage_attribution() {
        let (_dir, db) = fixture();
        let db = std::sync::Arc::new(db);
        db.write(|c| {
            claim(c,"sample","A").map_err(store::sql_error)?;
            let e=execution(c,"A").unwrap();
            assert_eq!(crate::db::attempts::list_for_card(c,"A")?.len(),1);
            c.execute("INSERT INTO token_ledger(ts,session,conversation,input,output) VALUES(?1,?2,'fixture',40,80)",params![chrono::Utc::now().timestamp(),e.worker])?;
            Ok(WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
        crate::runtime_jobs::token_ledger::attribute_tasks(&db)
            .await
            .unwrap();
        let c = db.read().unwrap();
        let usage = super::super::usage::summary(&c, "sample").unwrap();
        assert_eq!(usage["tokens"], 120);
        assert_eq!(usage["execution_turns_measured"], 1);
        assert_eq!(
            c.query_row(
                "SELECT task FROM token_ledger WHERE conversation='fixture'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "A"
        );
    }
    #[test]
    fn project_delivery_rechecks_generation_pause_and_policy_identity() {
        let (_dir, db) = fixture();
        db.write(|c| {
            claim(c, "sample", "A").map_err(store::sql_error)?;
            let e = execution(c, "A").unwrap();
            assert!(delivery_current(c, "sample", &e.worker, &e.delivery_id).unwrap());
            assert!(!delivery_current(c, "sample", "foreign", &e.delivery_id).unwrap());
            assert!(!delivery_current(c, "sample", &e.worker, "old-generation").unwrap());
            let mut p = store::get(c, "sample").unwrap().unwrap();
            p.policy.repository = "/different".into();
            assert!(store::save(c, "sample", p.revision, &p.policy, "test").is_err());
            p.policy.repository = "/repo".into();
            p.policy.paused = true;
            store::save(c, "sample", p.revision, &p.policy, "test").map_err(store::sql_error)?;
            assert!(!delivery_current(c, "sample", &e.worker, &e.delivery_id).unwrap());
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .unwrap();
    }
    #[test]
    fn project_reports_bind_identity_generation_requirements_and_criteria() {
        let (_dir, db) = fixture();
        let _home = crate::api::settings::test_env::set_home(_dir.path());
        db.write(|c| claim(c, "sample", "A").map_err(store::sql_error))
            .unwrap();
        let e = execution(&db.read().unwrap(), "A").unwrap();
        register_test_workspace(&e.worker, "/repo");
        let report = Report {
            assets: vec![fixture_asset()],
            head: "a".repeat(40),
            summary: "implemented".into(),
            checks: vec![Check {
                criterion: "Output passes its test".into(),
                command: "./verify.sh".into(),
            }],
        };
        db.write(move |c| {
            assert!(record_report(
                c,
                "sample",
                "A",
                "foreign",
                e.generation,
                &e.input_hash,
                &report
            )
            .is_err());
            assert!(record_report(
                c,
                "sample",
                "A",
                &e.worker,
                e.generation + 1,
                &e.input_hash,
                &report
            )
            .is_err());
            assert!(record_report(
                c,
                "sample",
                "A",
                &e.worker,
                e.generation,
                "wrong-hash",
                &report
            )
            .is_err());
            assert!(record_report(
                c,
                "sample",
                "A",
                &e.worker,
                e.generation,
                &e.input_hash,
                &Report {
                    assets: vec![],
                    ..report.clone()
                }
            )
            .is_err());
            assert!(record_report(
                c,
                "sample",
                "A",
                &e.worker,
                e.generation,
                &e.input_hash,
                &Report {
                    checks: vec![],
                    ..report.clone()
                }
            )
            .is_err());
            let out = record_report(
                c,
                "sample",
                "A",
                &e.worker,
                e.generation,
                &e.input_hash,
                &report,
            )
            .map_err(store::sql_error)?;
            assert!(
                !record_report(
                    c,
                    "sample",
                    "A",
                    &e.worker,
                    e.generation,
                    &e.input_hash,
                    &report
                )
                .unwrap()
                .applied
            );
            assert_eq!(bs::get_issue(c, "A")?.unwrap().status, "review");
            c.execute(
                "UPDATE issues SET title='Changed requirement' WHERE id='A'",
                [],
            )?;
            assert!(record_report(
                c,
                "sample",
                "A",
                &e.worker,
                e.generation,
                &e.input_hash,
                &report
            )
            .is_err());
            Ok(out)
        })
        .unwrap();
    }
    #[test]
    fn project_read_model_explains_pause_capacity_and_never_spins_after_failure() {
        let (_dir, db) = fixture();
        db.write(|c| {
            let mut p = store::get(c, "sample").unwrap().unwrap();
            p.policy.paused = true;
            store::save(c, "sample", p.revision, &p.policy, "test").map_err(store::sql_error)?;
            assert!(!claim(c, "sample", "A").unwrap().applied);
            assert!(plan(c, &store::get(c, "sample").unwrap().unwrap())
                .unwrap()
                .iter()
                .all(|p| p.waiting_reason.as_deref() == Some("project_paused")));
            p.policy.paused = false;
            store::save(c, "sample", 2, &p.policy, "test").map_err(store::sql_error)?;
            claim(c, "sample", "A").map_err(store::sql_error)?;
            let row = bs::get_issue(c, "A")?.unwrap();
            let mut e = execution(c, "A").unwrap();
            e.stage = "waiting".into();
            e.waiting = Some("access unavailable".into());
            save_execution(c, &row, &e, "project.waiting").map_err(store::sql_error)?;
            for _ in 0..100 {
                assert!(!claim(c, "sample", "A").unwrap().applied);
            }
            assert_eq!(execution(c, "A").unwrap().attempt, 1);
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .unwrap();
    }
}
