use crate::db::{board_store as bs, PendingEvent, WriteOutcome};
use amux_core::{
    project::{ExecutionPolicy, Phase},
    revision::{EntityType, MutationKind},
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub fn sql_error(error: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(error.to_string())))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub revision: i64,
    pub policy: ExecutionPolicy,
}

pub fn get(conn: &Connection, name: &str) -> anyhow::Result<Option<Project>> {
    let raw: Option<(String,i64)> = conn.query_row(
        "SELECT execution_policy,execution_rev FROM group_config WHERE name=?1 AND execution_policy IS NOT NULL",
        [name], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    raw.map(|(raw, revision)| {
        Ok(Project {
            name: name.into(),
            revision,
            policy: serde_json::from_str(&raw)?,
        })
    })
    .transpose()
}

pub fn list(conn: &Connection) -> anyhow::Result<Vec<Project>> {
    let mut q = conn.prepare(
        "SELECT name FROM group_config WHERE execution_policy IS NOT NULL ORDER BY name",
    )?;
    let names = q
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    names
        .into_iter()
        .map(|name| {
            get(conn, &name)?.ok_or_else(|| anyhow::anyhow!("project disappeared during read"))
        })
        .collect()
}

fn acceptance_revision_high_water(conn: &Connection, name: &str) -> anyhow::Result<u32> {
    let revision: i64 = conn.query_row(
        "SELECT COALESCE(MAX(revision),0) FROM (
            SELECT CAST(json_extract(data,'$.policy.acceptance.revision') AS INTEGER) revision
              FROM session_events WHERE session=?1 AND type='project.policy'
            UNION ALL
            SELECT CAST(json_extract(data,'$.contract_revision') AS INTEGER) revision
              FROM session_events WHERE session=?1 AND type='project.acceptance'
        )",
        [format!("project:{name}")],
        |r| r.get(0),
    )?;
    Ok(revision.max(0) as u32)
}

pub fn save(
    conn: &Connection,
    name: &str,
    expected: i64,
    policy: &ExecutionPolicy,
    actor: &str,
) -> anyhow::Result<WriteOutcome> {
    anyhow::ensure!(amux_core::project::valid_name(name), "invalid project name");
    policy.validate().map_err(anyhow::Error::msg)?;
    if let Some(contract) = &policy.acceptance {
        contract.validate().map_err(anyhow::Error::msg)?;
    }
    if policy.mode == amux_core::project::ProjectExecutionMode::Lead {
        let contract=policy.acceptance.as_ref().ok_or_else(||anyhow::anyhow!("lead projects need an independent acceptance contract"))?;
        anyhow::ensure!(contract.criteria.iter().any(|c|c.verifier.is_human()),"lead projects need human artifact review");
        anyhow::ensure!(contract.criteria.iter().any(|c|!c.verifier.is_human()),"lead projects need at least one automated outcome check");
        anyhow::ensure!(contract.criteria.iter().any(|c| match &c.verifier {
            amux_core::project::ContractVerifier::Command { command, .. } => !matches!(command.trim(), "git diff --check" | "true" | ":"),
            amux_core::project::ContractVerifier::Execution { .. } => true,
            amux_core::project::ContractVerifier::Human { .. } => false,
        }), "replace the starter git diff --check with an outcome-specific automated check before creating a lead project");
    }
    let current = get(conn, name)?;
    let revision = current.as_ref().map(|p| p.revision).unwrap_or(0);
    // The contract revision is server-owned: it moves only when the criteria change, so a receipt
    // can always be tied to the exact contract it was judged against.
    let mut normalized = policy.clone();
    if let Some(contract) = normalized.acceptance.as_mut() {
        let high_water = acceptance_revision_high_water(conn, name)?;
        contract.revision = match current.as_ref().and_then(|p| p.policy.acceptance.as_ref()) {
            Some(old) if old.criteria == contract.criteria => old.revision,
            Some(old) => {
                let next = high_water.max(old.revision).saturating_add(1);
                tracing::info!(project=name,from=old.revision,to=next,measured=true,n_considered=contract.criteria.len(),verdict="project.acceptance_contract_revised","acceptance contract criteria changed; prior receipts stay bound to the old revision");
                next
            }
            None => high_water.saturating_add(1),
        };
    }
    let policy = &normalized;
    anyhow::ensure!(
        revision == expected,
        "project revision conflict: expected {expected}, current {revision}"
    );
    if current.as_ref().is_some_and(|p| &p.policy == policy) {
        return Ok(WriteOutcome {
            applied: false,
            events: vec![],
        });
    }
    if let Some(current) = &current {
        if current.policy.mode != policy.mode {
            let has_intent:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM cmd_history WHERE project_group=?1)",[name],|r|r.get(0))?;
            let has_tasks:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM issues WHERE project_group=?1)",[name],|r|r.get(0))?;
            anyhow::ensure!(!has_intent && !has_tasks,"project execution mode cannot change after work begins; create a new project for the lead model");
        }
        if (current.policy.paused || !current.policy.enabled) && !policy.paused && policy.enabled {
            let stopping = bs::project_issues(conn, name)?
                .iter()
                .map(|row| super::planner::execution(conn, &row.id))
                .collect::<anyhow::Result<Vec<_>>>()?
                .iter()
                .any(|e| {
                    !e.worker.is_empty()
                        && !e.suspended
                        && !matches!(e.stage.as_str(), "verified" | "")
                });
            anyhow::ensure!(
                !stopping,
                "pause is still stopping executors; wait for Paused before resuming"
            );
        }
        let active: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM issues WHERE project_group=?1 AND execution_state IS NOT NULL AND json_extract(execution_state,'$.stage') NOT IN ('verified',''))", [name], |r|r.get(0))?;
        // A paused project has stopped its executors and marked every retained
        // execution suspended. It is safe to change the model/effort of the
        // same provider there; the next dispatch refreshes the worker env from
        // policy before starting the process. An active lane keeps the original
        // immutability rule so a live attempt cannot change models mid-turn.
        let suspended: bool = conn.query_row("SELECT NOT EXISTS(SELECT 1 FROM issues WHERE project_group=?1 AND execution_state IS NOT NULL AND json_extract(execution_state,'$.stage') NOT IN ('verified','') AND COALESCE(json_extract(execution_state,'$.suspended'),0) != 1)", [name], |r|r.get(0))?;
        let profile_safe = current.policy.executor == policy.executor
            || (current.policy.paused
                && policy.paused
                && suspended
                && current.policy.executor.provider == policy.executor.provider);
        anyhow::ensure!(!active || (current.policy.repository == policy.repository
            && current.policy.verify_command == policy.verify_command
            && profile_safe
            && current.policy.worktree == policy.worktree),
            "repository, verification gate and checkout mode are fixed while executions retain work; pause and settle the project before changing executor model or effort");
    }
    conn.execute("INSERT INTO group_config(name,execution_policy,execution_rev,updated) VALUES(?1,?2,?3,?4) ON CONFLICT(name) DO UPDATE SET execution_policy=excluded.execution_policy,execution_rev=excluded.execution_rev,updated=excluded.updated",
        params![name,serde_json::to_string(policy)?,revision+1,chrono::Utc::now().timestamp()])?;
    invalidate_changed_contract_tasks(conn, name, current.as_ref().and_then(|p|p.policy.acceptance.as_ref()), policy.acceptance.as_ref())?;
    let project = Project {
        name: name.into(),
        revision: revision + 1,
        policy: policy.clone(),
    };
    conn.execute("INSERT INTO session_events(ts,session,type,data,source) VALUES(?1,?2,'project.policy',?3,?4)",
        params![crate::config::now_f64(),format!("project:{name}"),serde_json::to_string(&project)?,actor])?;
    Ok(WriteOutcome {
        applied: true,
        events: vec![PendingEvent {
            entity_type: EntityType::Other("group_config".into()),
            entity_id: name.into(),
            mutation: MutationKind::Updated,
            payload: Some(json!(project)),
        }],
    })
}

/// Reuse ordinary stale-requirement reconciliation. Only tasks bound to a
/// changed criterion are invalidated; their old reports remain in history and
/// active providers settle at a boundary before a refreshed packet is claimed.
fn invalidate_changed_contract_tasks(conn: &Connection, name: &str, old: Option<&amux_core::project::AcceptanceContract>, new: Option<&amux_core::project::AcceptanceContract>) -> anyhow::Result<()> {
    let Some(old)=old else { return Ok(()) };
    let changed:std::collections::HashSet<_>=old.criteria.iter().filter(|c|new.and_then(|n|n.criterion(&c.id))!=Some(*c)).map(|c|c.id.as_str()).collect();
    if changed.is_empty() { return Ok(()) }
    for row in bs::project_issues(conn,name)? {
        if row.archived!=0 || matches!(row.status.as_str(),"discarded"|"deleted") { continue; }
        let criteria:Vec<String>=serde_json::from_str(row.acceptance_criteria.as_deref().unwrap_or("[]"))?;
        if !criteria.iter().any(|criterion|criterion.strip_prefix("contract:").or_else(||criterion.strip_prefix("review-evidence:")).is_some_and(|id|changed.contains(id))) { continue; }
        let mut state=super::planner::execution(conn,&row.id)?;
        if state.stage.is_empty() { continue; }
        // Clearing identity rejects old receipts and takes the existing
        // requirements-changed path. It does not approve a held task or retry
        // unchanged policy. Explicit authorization and suspension survive.
        state.input_hash.clear();
        let reserved_slot=matches!(state.stage.as_str(),"reserved"|"working"|"reported"|"verifying");
        if !reserved_slot { state.stage="waiting".into(); }
        state.last_failure=state.waiting.clone().or(state.last_failure);
        if !super::outputs::authorization_hold(conn,&row)? && state.wait_category.as_deref()!=Some("required_outputs") {
            state.waiting=Some("requirements_changed: bound project acceptance criterion changed; refresh the same task against the new contract".into());
        }
        super::planner::save_execution(conn,&row,&state,"project.contract_requirements_changed")?;
        if !reserved_slot { conn.execute("UPDATE issues SET status='blocked' WHERE id=?1 AND status<>'needsyou'",[&row.id])?; }
        tracing::info!(project=name,task=%row.id,measured=true,n_considered=1,verdict="project.contract_task_invalidated","changed acceptance criterion invalidated its task input; old receipts and authorization holds retained");
    }
    Ok(())
}

fn phase_workspace_priority(phase: Phase) -> u8 {
    match phase {
        Phase::Working | Phase::Verifying => 0,
        Phase::Ready | Phase::Intake => 1,
        Phase::Waiting => 2,
        Phase::Verified => 3,
        Phase::Closed | Phase::Unrecognized => 4,
    }
}

fn project_summary_workspace(
    project: &Project,
    plans: &[super::planner::CardPlan],
) -> Option<Value> {
    if !project.policy.worktree {
        return None;
    }
    let home = crate::config::amux_home();
    if let Some(workspace) = super::checkout::load(&home, &project.name) {
        return Some(json!({"project":project.name,"repo":workspace.repo,"path":workspace.path,
            "available":std::path::Path::new(&workspace.path).is_dir(),"branch":workspace.branch,"base":workspace.base}));
    }
    let mut candidates: Vec<(u8, String)> = plans
        .iter()
        .filter_map(|plan| {
            let worker = plan.execution.worker.trim();
            (!worker.is_empty()).then(|| (phase_workspace_priority(plan.phase), worker.to_string()))
        })
        .collect();
    candidates.sort();
    candidates.dedup_by(|left, right| left.1 == right.1);
    candidates.into_iter().find_map(|(_, worker)| {
        crate::fanout_workspace::load(&home, &worker).map(|workspace| {
            let available = std::path::Path::new(&workspace.path).is_dir();
            json!({
                "worker": worker,
                "repo": workspace.repo,
                "path": workspace.path,
                "available": available,
                "branch": workspace.branch,
                "base": workspace.base,
            })
        })
    })
}

pub fn summary(conn: &Connection, project: &Project) -> anyhow::Result<Value> {
    let rows = bs::project_issues(conn, &project.name)?;
    let plans = super::planner::plan(conn, project)?;
    let acceptance = super::acceptance::status(conn, project)?;
    let retirement = super::acceptance::retirement_allowed(conn, &project.name)?;
    let task_count = plans.len();
    let verified_tasks = plans.iter().filter(|p| p.phase == Phase::Verified).count();
    let closed_tasks = plans.iter().filter(|p| p.phase == Phase::Closed).count();
    let waiting_tasks = plans.iter().filter(|p| p.phase == Phase::Waiting).count();
    let active_tasks = task_count.saturating_sub(verified_tasks + closed_tasks);
    let running_executions = plans
        .iter()
        .filter(|p| {
            matches!(
                p.execution.stage.as_str(),
                "reserved" | "working" | "reported" | "verifying"
            )
        })
        .count();
    // The planner stage is a durable assignment, not proof that its provider
    // is doing work. Let the client join these names with the fresh session
    // inventory before claiming the project is actively driving.
    let working_workers: Vec<&str> = plans.iter()
        .filter(|p| p.execution.stage == "working" && !p.execution.worker.is_empty())
        .map(|p| p.execution.worker.as_str())
        .collect();
    let queued_repairs = plans.iter().filter(|p| p.action == "claim" && p.execution.stage == "repair").count();
    Ok(json!({
        "task_count": task_count,
        "verified_tasks": verified_tasks,
        "closed_tasks": closed_tasks,
        "active_tasks": active_tasks,
        "waiting_tasks": waiting_tasks,
        "running_executions": running_executions,
        "working_workers": working_workers,
        "queued_repairs": queued_repairs,
        "acceptance_state": acceptance.get("state").and_then(Value::as_str).unwrap_or("unknown"),
        "acceptance_reason": acceptance.get("reason").and_then(Value::as_str),
        "retirement_state": retirement.get("state").and_then(Value::as_str).unwrap_or("unknown"),
        "retirement_reason": retirement.get("reason").and_then(Value::as_str),
        "worktree": project.policy.worktree,
        "workspace": project_summary_workspace(project, &plans),
        "measured": true,
        "n_considered": rows.len(),
    }))
}

fn shell_words(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => cur.push(ch),
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            None if ch == '\\' => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            None => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn codex_model_from_flags(raw: Option<&String>) -> Option<String> {
    let words = shell_words(raw?);
    for (idx, word) in words.iter().enumerate() {
        if word == "--model" || word == "-m" {
            return words.get(idx + 1).cloned();
        }
        if let Some(value) = word.strip_prefix("--model=") {
            return Some(value.to_string());
        }
    }
    None
}

fn codex_effort_from_flags(raw: Option<&String>) -> Option<String> {
    let words = shell_words(raw?);
    for (idx, word) in words.iter().enumerate() {
        let value = if word == "-c" || word == "--config" {
            words.get(idx + 1).map(String::as_str)
        } else {
            word.strip_prefix("-c=")
                .or_else(|| word.strip_prefix("--config="))
        };
        let Some(value) = value else { continue };
        for key in ["model_reasoning_effort", "reasoning_effort"] {
            if let Some(effort) = value.strip_prefix(&format!("{key}=")) {
                return Some(effort.trim_matches('"').trim_matches('\'').to_string());
            }
        }
    }
    None
}

fn project_worker_lifecycle(
    home: &std::path::Path,
    worker: &str,
) -> (String, Option<serde_json::Value>) {
    let active = home.join("sessions").join(format!("{worker}.env"));
    let expired = active.with_extension("env.reaped");
    let source = if active.exists() {
        Some(active)
    } else if expired.exists() {
        Some(expired)
    } else {
        None
    };
    let Some(source) = source else {
        return ("missing".into(), None);
    };
    let env = crate::config::parse_env_file(&source);
    let lifecycle = if source.extension().and_then(|x| x.to_str()) == Some("reaped") {
        "expired"
    } else if env.get("CC_ARCHIVED").is_some_and(|v| v == "1") {
        "archived"
    } else if env.get("CC_PAUSED").is_some_and(|v| v == "1") {
        "paused"
    } else if env.get("CC_REVIEW_HELD").is_some_and(|v| v == "1") {
        "review"
    } else {
        "active"
    };
    let model = env
        .get("CC_MODEL")
        .or_else(|| env.get("CODEX_MODEL"))
        .cloned()
        .or_else(|| codex_model_from_flags(env.get("CC_FLAGS")));
    let effort = env
        .get("CC_REASONING_EFFORT")
        .or_else(|| env.get("CC_EFFORT"))
        .cloned()
        .or_else(|| codex_effort_from_flags(env.get("CC_FLAGS")));
    (
        lifecycle.into(),
        Some(json!({
            "path": source.to_string_lossy(),
            "provider": env.get("CC_PROVIDER"),
            "model": model,
            "effort": effort,
            "dir": env.get("CC_DIR"),
            "project": env.get("CC_PROJECT"),
            "worktree": env.get("CC_WORKTREE"),
            "ephemeral": env.get("CC_EPHEMERAL").is_some_and(|v| v == "1"),
        })),
    )
}

fn project_workers(rows: &[bs::IssueRow], plans: &[super::planner::CardPlan]) -> Vec<Value> {
    let home = crate::config::amux_home();
    let mut by_worker: std::collections::BTreeMap<String, Vec<Value>> =
        std::collections::BTreeMap::new();
    let mut by_id: std::collections::BTreeMap<&str, &super::planner::CardPlan> =
        std::collections::BTreeMap::new();
    for plan in plans {
        by_id.insert(plan.id.as_str(), plan);
    }
    for row in rows {
        let Some(plan) = by_id.get(row.id.as_str()) else {
            continue;
        };
        let worker = plan.execution.worker.trim();
        if worker.is_empty() {
            continue;
        }
        by_worker
            .entry(worker.to_string())
            .or_default()
            .push(json!({
                "id": row.id,
                "title": row.title,
                "status": row.status,
                "phase": plan.phase,
                "stage": plan.execution.stage,
                "updated": row.updated,
                "retained_assets": plan.execution.retained_assets.len(),
                "waiting_label": plan.waiting_label,
                "waiting_reason": plan.waiting_reason,
                "execution_waiting": plan.execution.waiting,
            }));
    }
    by_worker
        .into_iter()
        .map(|(worker, tasks)| {
            let (lifecycle, env) = project_worker_lifecycle(&home, &worker);
            let workspace = crate::fanout_workspace::load(&home, &worker);
            let workspace_available = workspace
                .as_ref()
                .is_some_and(|w| std::path::Path::new(&w.path).is_dir());
            let integration = crate::fanout_workspace::integration_status(&home, &worker);
            let verified_tasks = tasks
                .iter()
                .filter(|t| t.get("phase").and_then(Value::as_str) == Some("verified"))
                .count();
            let active_tasks = tasks.len().saturating_sub(verified_tasks);
            let retained_assets: usize = tasks
                .iter()
                .map(|t| {
                    t.get("retained_assets")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize
                })
                .sum();
            let blocked_reason = tasks
                .iter()
                .filter_map(|t| t.get("waiting_reason").and_then(Value::as_str))
                .find(|reason| reason.starts_with("refusing to spawn a worker:"))
                .map(str::to_string);
            json!({
                "name": worker,
                "lifecycle": lifecycle,
                "openable": lifecycle != "missing" && lifecycle != "expired" && blocked_reason.is_none(),
                "blocked_reason": blocked_reason,
                "resumable": lifecycle == "expired",
                "task_count": tasks.len(),
                "verified_tasks": verified_tasks,
                "active_tasks": active_tasks,
                "retained_assets": retained_assets,
                "env": env,
                "workspace": workspace,
                "workspace_available": workspace_available,
                "integration": integration,
                "tasks": tasks,
            })
        })
        .collect()
}

pub fn board(conn: &Connection, name: &str) -> anyhow::Result<Value> {
    let project = get(conn, name)?.ok_or_else(|| anyhow::anyhow!("project not found"))?;
    if project.policy.mode == amux_core::project::ProjectExecutionMode::Lead {
        let home=crate::config::amux_home();
        let worker=super::checkout::owner(&home,name);
        let (lifecycle,env)=project_worker_lifecycle(&home,&worker);
        let workspace=crate::fanout_workspace::load(&home,&worker);
        let available=workspace.as_ref().is_some_and(|w|std::path::Path::new(&w.path).is_dir());
        let history_log=home.join("logs").join(format!("{worker}.log"));
        let history_log=history_log.is_file().then(||history_log.to_string_lossy().into_owned());
        let integration=crate::fanout_workspace::integration_status(&home,&worker);
        let lead=super::lead::latest(conn,name)?.unwrap_or(json!({"state":"pending","plan":[]}));
        let mut history_query=conn.prepare("SELECT id,ts,data FROM session_events WHERE session=?1 AND type='project.lead_progress' ORDER BY id DESC LIMIT 20")?;
        let lead_history=history_query.query_map([format!("project:{name}")],|r|{
            let raw:String=r.get(2)?;
            Ok(json!({"id":r.get::<_,i64>(0)?,"ts":r.get::<_,f64>(1)?,"progress":serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null)}))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut acceptance=super::acceptance::status(conn,&project)?;
        acceptance["executor_retirement"]=super::acceptance::retirement_allowed(conn,name)?;
        return Ok(json!({"project":project,"pause_settled":lifecycle!="active","cards":[],"lead":lead,"lead_history":lead_history,
            "workers":[{"name":worker,"lifecycle":lifecycle,"openable":lifecycle!="missing" && lifecycle!="expired",
                "resumable":lifecycle=="expired","task_count":0,"verified_tasks":0,"active_tasks":0,
                "retained_assets":0,"env":env,"workspace":workspace,"workspace_available":available,
                "integration":integration,"history_log":history_log,"tasks":[]}],"migrations":[],
            "commands":super::intake::receipts(conn,name)?,"acceptance":acceptance,
            "measured":true,"n_considered":1,"usage":super::usage::summary(conn,name)?}));
    }
    let rows = bs::project_issues(conn, name)?;
    let plans = super::planner::plan(conn, &project)?;
    let mut q=conn.prepare("SELECT idem,type,data FROM session_events WHERE session=?1 AND type IN ('project.migrated','project.migration_rolled_back') ORDER BY id DESC LIMIT 20")?;
    let migrations=q.query_map([format!("project:{name}")],|r|Ok(json!({"id":r.get::<_,Option<String>>(0)?,"event":r.get::<_,String>(1)?,"data":r.get::<_,String>(2)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let pause_settled = plans.iter().all(|p| {
        p.execution.worker.is_empty()
            || p.execution.suspended
            || matches!(p.execution.stage.as_str(), "verified" | "")
    });
    let cards: Vec<_> = rows
        .iter()
        .map(|row| {
            let mut card = row.snapshot_slim();
            card["phase"] = json!(plans.iter().find(|p| p.id == row.id).map(|p| p.phase));
            card["assignee"] = json!(row.session);
            card["execution_plan"] = json!(plans.iter().find(|p| p.id == row.id));
            card["retry_available"] =
                json!(plans.iter().find(|p| p.id == row.id).is_some_and(|plan| {
                    super::task_retry::eligible(conn, &project, row, &plan.execution).is_ok()
                }));
            card["verification_retry_available"] =
                json!(plans.iter().find(|p| p.id == row.id).is_some_and(|plan| {
                    super::task_retry::verification_eligible(conn, &project, row, &plan.execution)
                        .is_ok()
                }));
            card
        })
        .collect();
    let mut acceptance = super::acceptance::status(conn, &project)?;
    acceptance["executor_retirement"] = super::acceptance::retirement_allowed(conn, name)?;
    let workers = project_workers(&rows, &plans);
    Ok(
        json!({"project":project,"pause_settled":pause_settled,"cards":cards,"workers":workers,"migrations":migrations,"commands":super::intake::receipts(conn,name)?,"acceptance":acceptance,"measured":true,"n_considered":rows.len(),
        "usage":super::usage::summary(conn,name)?}),
    )
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationRow {
    pub id: String,
    pub revision: i64,
    pub session: Option<String>,
    pub project_group: Option<String>,
    pub status: String,
    pub lease_owner: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPreview {
    pub project: String,
    pub project_revision: i64,
    pub source_workers: Vec<String>,
    pub fingerprint: String,
    pub rows: Vec<MigrationRow>,
    pub conflicts: Vec<String>,
    pub measured: bool,
    pub n_considered: usize,
}

/// This is an inventory, not a semantic merge. It never closes, reassigns or
/// verifies a card and does not infer project membership from a shared repo.
pub fn preview(
    conn: &Connection,
    name: &str,
    workers: &[String],
) -> anyhow::Result<MigrationPreview> {
    let project = get(conn, name)?.ok_or_else(|| anyhow::anyhow!("project not found"))?;
    anyhow::ensure!(
        workers.len() <= 100,
        "preview accepts at most 100 explicit worker names"
    );
    let mut workers = workers.to_vec();
    workers.sort();
    workers.dedup();
    anyhow::ensure!(
        workers
            .iter()
            .all(|w| crate::api::session_verbs::valid_session_name(w)),
        "invalid source worker name"
    );
    let mut q=conn.prepare("SELECT id,rev,session,project_group,status,lease_owner FROM issues WHERE deleted IS NULL AND session IN (SELECT value FROM json_each(?1)) ORDER BY id")?;
    let rows = q
        .query_map([serde_json::to_string(&workers)?], |r| {
            Ok(MigrationRow {
                id: r.get(0)?,
                revision: r.get(1)?,
                session: r.get(2)?,
                project_group: r.get(3)?,
                status: r.get(4)?,
                lease_owner: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut conflicts = Vec::new();
    for row in &rows {
        if row.project_group.as_deref().is_some_and(|p| p != name) {
            conflicts.push(format!("{} already belongs to another project", row.id));
        }
        if row.status == "doing" || row.lease_owner.is_some() {
            conflicts.push(format!(
                "{} has active work; finish or release its claim before migration",
                row.id
            ));
        }
    }
    for row in &rows {
        let task = bs::get_issue(conn, &row.id)?.ok_or_else(|| anyhow::anyhow!("task missing"))?;
        let mut groups = task
            .session
            .as_deref()
            .map(crate::api::session_verbs::lane_groups)
            .unwrap_or_default();
        groups.insert(name.to_string());
        for target in [
            amux_core::board::TaskStatus::Doing,
            amux_core::board::TaskStatus::Review,
            amux_core::board::TaskStatus::Done,
            amux_core::board::TaskStatus::Verified,
        ] {
            let (_, source) = bs::effective_gate_with_source(conn, &task, target, &groups);
            if !matches!(source, bs::GateSource::TypeDefault) {
                conflicts.push(format!("{} has a custom {:?} gate; translate its requirements into project criteria and checks before migration",row.id,target));
                break;
            }
        }
    }
    let changes: Vec<_> = rows
        .iter()
        .map(|r| (r.id.clone(), bs::BoardOwner::new(Some(name), None)))
        .collect();
    if let Err(error) = bs::validate_owner_changes(conn, &changes) {
        conflicts.push(format!("dependency ownership conflict: {error}"));
    }
    let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&(
        name,
        project.revision,
        &workers,
        &rows,
    ))?));
    Ok(MigrationPreview {
        project: name.into(),
        project_revision: project.revision,
        source_workers: workers,
        fingerprint,
        n_considered: rows.len(),
        rows,
        conflicts,
        measured: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Store;
    fn policy() -> ExecutionPolicy {
        serde_json::from_value(json!({"repository":"/repo","coordinator":{"provider":"claude","model":"haiku"},"executor":{"provider":"claude","model":"sonnet"},"verify_command":"./verify.sh"})).unwrap()
    }
    #[test]
    fn project_contract_edit_refreshes_only_bound_tasks_and_preserves_holds() {
        let dir=tempfile::tempdir().unwrap();let db=Store::open(&dir.path().join("db")).unwrap();
        db.write(|c| {
            let mut p=policy();p.enabled=true;p.max_executors=1;p.acceptance=Some(serde_json::from_value(json!({"revision":1,"criteria":[
                {"id":"api","requirement":"Fixture API checks","verifier":{"type":"command","id":"api","command":"python3 api.py"},"evidence":["api.json"]},
                {"id":"docs","requirement":"Review docs","verifier":{"type":"human","id":"docs","instructions":"Read docs"},"evidence":["docs.md"]}]})).unwrap());
            save(c,"example",0,&p,"test").unwrap();
            for (id,criterion,stage,category) in [("active","contract:api","working",None),("checked","contract:api","verified",None),("held","contract:api","waiting",Some("spend")),("unrelated","review-evidence:docs","verified",None)] {
                c.execute("INSERT INTO issues(id,title,desc,status,type,project_group,created,updated,next_action,acceptance_criteria) VALUES(?1,'Output','Concrete outcome',?2,'code','example',1,1,'Verify output',?3)",params![id,if stage=="verified"{"verified"}else{"doing"},json!([criterion]).to_string()])?;
                let row=bs::get_issue(c,id)?.unwrap();let e=super::super::planner::Execution{stage:stage.into(),generation:3,worker:format!("worker-{id}"),input_hash:super::super::planner::input_hash(&row),wait_category:category.map(str::to_owned),waiting:category.map(|_|"spend: approval required".into()),..Default::default()};
                super::super::planner::save_execution(c,&row,&e,"test.setup").unwrap();
            }
            p.acceptance.as_mut().unwrap().criteria[0].requirement.push_str(" with nonzero readbacks");
            save(c,"example",1,&p,"test").unwrap();
            let live=get(c,"example").unwrap().unwrap();
            let plans=super::super::planner::plan(c,&live).unwrap();
            for id in ["active","checked"] {
                let e=super::super::planner::execution(c,id).unwrap();assert_eq!(e.stage,if id=="active"{"working"}else{"waiting"});assert!(e.input_hash.is_empty());assert_eq!(e.generation,3);
                assert_eq!(bs::get_issue(c,id)?.unwrap().status,if id=="active"{"doing"}else{"blocked"});
            }
            assert_eq!(plans.iter().find(|p|p.id=="active").unwrap().action,"claim","the existing slot can refresh at capacity one");
            assert_ne!(plans.iter().find(|p|p.id=="checked").unwrap().action,"claim","a still-settling provider retains its concurrency slot");
            let held=super::super::planner::execution(c,"held").unwrap();assert_eq!(held.wait_category.as_deref(),Some("spend"));assert_eq!(held.waiting.as_deref(),Some("spend: approval required"));
            assert_eq!(plans.iter().find(|p|p.id=="held").unwrap().waiting_reason.as_deref(),Some("authorization_required"));
            assert_eq!(super::super::planner::execution(c,"unrelated").unwrap().stage,"verified");
            assert!(!save(c,"example",2,&p,"test").unwrap().applied);
            let count:i64=c.query_row("SELECT COUNT(*) FROM session_events WHERE type='project.contract_requirements_changed'",[],|r|r.get(0))?;assert_eq!(count,3);
            Ok(WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
    }

    #[test]
    fn project_policy_is_revisioned_durable_and_does_not_replace_group_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let store = Store::open(&path).unwrap();
        store
            .write(|c| {
                c.execute(
                    "INSERT INTO group_config(name,goal) VALUES('example','preserve me')",
                    [],
                )?;
                save(c, "example", 0, &policy(), "test").map_err(sql_error)
            })
            .unwrap();
        store
            .write(|c| {
                assert!(
                    !save(c, "example", 1, &policy(), "test")
                        .map_err(sql_error)?
                        .applied
                );
                assert!(save(c, "example", 0, &policy(), "test").is_err());
                Ok(WriteOutcome {
                    applied: false,
                    events: vec![],
                })
            })
            .unwrap();
        drop(store);
        let reopened = Store::open(&path).unwrap();
        let conn = reopened.read().unwrap();
        assert_eq!(get(&conn, "example").unwrap().unwrap().revision, 1);
        assert_eq!(
            conn.query_row(
                "SELECT goal FROM group_config WHERE name='example'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "preserve me"
        );
        assert_eq!(list(&conn).unwrap().len(), 1);
    }
    #[test]
    fn active_executor_model_changes_only_at_a_settled_pause_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).unwrap();
        store.write(|c| {
            let mut p = policy();
            p.executor = serde_json::from_value(json!({"provider":"codex","model":"gpt-5.5","effort":"low"})).unwrap();
            p.enabled = true;
            save(c, "example", 0, &p, "test").map_err(sql_error)?;
            c.execute("INSERT INTO issues(id,title,status,type,project_group,created,updated,execution_state) VALUES('A','Work','blocked','code','example',1,1,?1)",
                [json!({"stage":"waiting","suspended":false}).to_string()])?;
            let mut cheaper = p.clone();
            cheaper.executor.model = "gpt-6-luna".into();
            assert!(save(c, "example", 1, &cheaper, "test").is_err());
            p.paused = true;
            save(c, "example", 1, &p, "test").map_err(sql_error)?;
            assert!(save(c, "example", 2, &cheaper, "test").is_err());
            c.execute("UPDATE issues SET execution_state=?1 WHERE id='A'",
                [json!({"stage":"waiting","suspended":true}).to_string()])?;
            cheaper.paused = true;
            save(c, "example", 2, &cheaper, "test").map_err(sql_error)?;
            assert_eq!(get(c,"example").unwrap().unwrap().policy.executor.model,"gpt-6-luna");
            cheaper.executor.provider = "claude".into();
            assert!(save(c, "example", 3, &cheaper, "test").is_err());
            Ok(WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
    }
    #[test]
    fn acceptance_revision_never_reuses_an_old_identity_after_removal() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).unwrap();
        store.write(|c| {
            let mut p = policy();
            p.acceptance = Some(serde_json::from_value(json!({"criteria":[{"id":"format","requirement":"artifact format passes","verifier":{"type":"command","id":"format-command","command":"./format-check.sh"}}]})).unwrap());
            save(c, "example", 0, &p, "test").map_err(sql_error)?;
            assert_eq!(get(c, "example").unwrap().unwrap().policy.acceptance.unwrap().revision, 1);
            p.acceptance = None;
            save(c, "example", 1, &p, "test").map_err(sql_error)?;
            p.acceptance = Some(serde_json::from_value(json!({"criteria":[{"id":"format","requirement":"artifact format passes","verifier":{"type":"command","id":"format-command","command":"./format-check.sh"}}]})).unwrap());
            save(c, "example", 2, &p, "test").map_err(sql_error)?;
            let restored = get(c, "example").unwrap().unwrap();
            assert_eq!(restored.policy.acceptance.as_ref().unwrap().revision, 2);
            let mut changed = restored.policy.clone();
            changed.acceptance.as_mut().unwrap().criteria[0].requirement = "artifact format and metadata pass".into();
            save(c, "example", restored.revision, &changed, "test").map_err(sql_error)?;
            assert_eq!(get(c, "example").unwrap().unwrap().policy.acceptance.unwrap().revision, 3);
            Ok(WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
    }
    #[test]
    fn project_summary_reports_real_lifecycle_state_without_full_board_payload() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).unwrap();
        store.write(|c| {
            save(c,"example",0,&policy(),"test").map_err(sql_error)?;
            c.execute("INSERT INTO issues(id,title,status,type,project_group,created,updated,next_action,acceptance_criteria) VALUES('A-1','Outcome','verified','doc','example',1,1,'Review artifact','[\"artifact exists\"]')",[])?;
            Ok(WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
        let conn = store.read().unwrap();
        let project = get(&conn, "example").unwrap().unwrap();
        let view = summary(&conn, &project).unwrap();
        assert_eq!(view["task_count"], 1);
        assert_eq!(view["verified_tasks"], 1);
        assert_eq!(view["active_tasks"], 0);
        assert_eq!(view["running_executions"], 0);
        assert_eq!(view["acceptance_state"], "not_configured");
        assert_eq!(view["retirement_state"], "review_not_configured");
        assert_eq!(view["worktree"], true);
    }
    #[test]
    fn preview_names_active_claims_and_foreign_edges_without_changing_cards() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).unwrap();
        store.write(|c| {
            save(c,"example",0,&policy(),"test").map_err(sql_error)?;
            c.execute("INSERT INTO issues(id,title,status,session,created,updated,depends_on) VALUES('A-1','Outcome','todo','source',1,1,'[\"OTHER-1\"]')",[])?;
            c.execute("INSERT INTO issues(id,title,status,session,created,updated,lease_owner) VALUES('A-2','Active','doing','source',1,1,'source')",[])?;
            Ok(WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
        let conn = store.read().unwrap();
        let p = preview(&conn, "example", &["source".into()]).unwrap();
        assert_eq!(p.n_considered, 2);
        assert_eq!(p.conflicts.len(), 2);
        assert_eq!(
            p.fingerprint,
            preview(&conn, "example", &["source".into(), "source".into()])
                .unwrap()
                .fingerprint
        );
        let row = bs::get_issue(&conn, "A-1").unwrap().unwrap();
        assert_eq!(row.project_group, None);
        assert_eq!(row.rev, 0);
        assert_eq!(row.depends_on, vec!["OTHER-1"]);
    }
    #[test]
    fn project_board_projects_workers_even_after_retirement() {
        let dir = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(dir.path());
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        std::fs::create_dir_all(dir.path().join("workspaces")).unwrap();
        std::fs::write(
            dir.path().join("sessions/project-worker.env.reaped"),
            "CC_PROVIDER=codex
CC_MODEL=gpt-5.5
CC_REASONING_EFFORT=low
CC_PROJECT=example
CC_WORKTREE=1
CC_EPHEMERAL=1
CC_DIR=/tmp/project-worker
",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("workspaces/project-worker.json"),
            r#"{"repo":"/repo","path":"/tmp/project-worker","branch":"amux/fanout/project-worker","base":"abc"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path()
                .join("workspaces/project-worker.integration.json"),
            r#"{"status":"integrated","head":"deadbeef","merged":true}"#,
        )
        .unwrap();
        let store = Store::open(&dir.path().join("db")).unwrap();
        store
            .write(|c| {
                let mut p = policy();
                p.enabled = true;
                save(c, "example", 0, &p, "test").map_err(sql_error)?;
                let execution = super::super::planner::Execution {
                    stage: "verified".into(),
                    worker: "project-worker".into(),
                    retained_assets: vec![super::super::assets::Retained {
                        path: "/tmp/artifacts/report.md".into(),
                        head: "deadbeef".into(),
                        source: super::super::assets::Asset {
                            path: "report.md".into(),
                            sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                        },
                    }],
                    ..Default::default()
                };
                c.execute(
                    r#"INSERT INTO issues(id,title,status,type,project_group,created,updated,next_action,acceptance_criteria,execution_state) VALUES('A-1','Outcome','verified','doc','example',1,2,'Review artifact','["artifact exists"]',?1)"#,
                    [serde_json::to_string(&execution).unwrap()],
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let view = board(&store.read().unwrap(), "example").unwrap();
        assert_eq!(view["workers"].as_array().unwrap().len(), 1);
        let worker = &view["workers"][0];
        assert_eq!(worker["name"], "project-worker");
        assert_eq!(worker["lifecycle"], "expired");
        assert_eq!(worker["openable"], false);
        assert_eq!(worker["resumable"], true);
        assert_eq!(worker["task_count"], 1);
        assert_eq!(worker["verified_tasks"], 1);
        assert_eq!(worker["retained_assets"], 1);
        assert_eq!(worker["env"]["provider"], "codex");
        assert_eq!(worker["env"]["model"], "gpt-5.5");
        assert_eq!(worker["env"]["effort"], "low");
        assert_eq!(worker["workspace"]["branch"], "amux/fanout/project-worker");
        assert_eq!(worker["workspace_available"], false);
        assert_eq!(worker["integration"]["status"], "integrated");
        assert_eq!(worker["tasks"][0]["id"], "A-1");
    }

    #[test]
    fn project_worker_env_projects_codex_model_and_effort_from_flags() {
        let dir = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(dir.path());
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        std::fs::write(
            dir.path().join("sessions/project-worker.env"),
            "CC_PROVIDER=codex
CC_FLAGS=\"--model gpt-5.5 -c model_reasoning_effort=low\"
CC_PROJECT=example
CC_WORKTREE=1
CC_EPHEMERAL=1
CC_DIR=/tmp/project-worker
",
        )
        .unwrap();
        let (lifecycle, env) = project_worker_lifecycle(dir.path(), "project-worker");
        let env = env.expect("env projection");
        assert_eq!(lifecycle, "active");
        assert_eq!(env["provider"], "codex");
        assert_eq!(env["model"], "gpt-5.5");
        assert_eq!(env["effort"], "low");
    }

    #[test]
    fn project_worker_env_projects_review_hold_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(dir.path());
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        std::fs::write(
            dir.path().join("sessions/project-worker.env"),
            "CC_PROVIDER=codex
CC_PROJECT=example
CC_REVIEW_HELD=1
CC_DIR=/tmp/project-worker
",
        )
        .unwrap();
        let (lifecycle, env) = project_worker_lifecycle(dir.path(), "project-worker");
        assert_eq!(lifecycle, "review");
        assert_eq!(env.unwrap()["project"], "example");
    }

    #[test]
    fn project_board_does_not_open_spawn_blocked_workers() {
        let dir = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(dir.path());
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        std::fs::write(
            dir.path().join("sessions/spawn-blocked.env"),
            "CC_PROVIDER=codex
CC_PROJECT=example
CC_WORKTREE=1
CC_EPHEMERAL=1
CC_DIR=/tmp/spawn-blocked
",
        )
        .unwrap();
        let store = Store::open(&dir.path().join("db")).unwrap();
        store
            .write(|c| {
                let mut p = policy();
                p.enabled = true;
                save(c, "example", 0, &p, "test").map_err(sql_error)?;
                let execution = super::super::planner::Execution {
                    stage: "waiting".into(),
                    worker: "spawn-blocked".into(),
                    waiting: Some("refusing to spawn a worker: test home requires explicit allowance".into()),
                    ..Default::default()
                };
                c.execute(
                    r#"INSERT INTO issues(id,title,status,type,project_group,created,updated,next_action,acceptance_criteria,execution_state) VALUES('A-1','Outcome','doing','doc','example',1,2,'Review artifact','["artifact exists"]',?1)"#,
                    [serde_json::to_string(&execution).unwrap()],
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let view = board(&store.read().unwrap(), "example").unwrap();
        let worker = &view["workers"][0];
        assert_eq!(worker["name"], "spawn-blocked");
        assert_eq!(worker["lifecycle"], "active");
        assert_eq!(worker["openable"], false);
        assert!(worker["blocked_reason"]
            .as_str()
            .unwrap()
            .starts_with("refusing to spawn"));
        assert_eq!(worker["tasks"][0]["waiting_label"], "Spawn blocked");
    }

    #[test]
    fn project_workspace_prefers_current_work_over_waiting_and_retained_candidates() {
        assert!(phase_workspace_priority(Phase::Working)<phase_workspace_priority(Phase::Waiting));
        assert!(phase_workspace_priority(Phase::Verifying)<phase_workspace_priority(Phase::Waiting));
        assert!(phase_workspace_priority(Phase::Waiting)<phase_workspace_priority(Phase::Verified));
    }

    #[test]
    fn project_board_survives_an_executor_change_and_never_projects_done_as_verified() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db")).unwrap();
        store.write(|c| {
            save(c,"example",0,&policy(),"test").map_err(sql_error)?;
            c.execute("INSERT INTO issues(id,title,status,session,created,updated,project_group,evidence) VALUES('A-1','Outcome','done','old-executor',1,1,'example','retained evidence')",[])?;
            Ok(WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
        let disabled = board(&store.read().unwrap(), "example").unwrap();
        assert_eq!(disabled["cards"][0]["phase"], "waiting");
        assert_eq!(
            disabled["cards"][0]["execution_plan"]["waiting_reason"],
            "project_disabled"
        );
        assert_eq!(disabled["cards"][0]["execution_plan"]["action"], "wait");
        store
            .write(|c| {
                let mut project = get(c, "example").map_err(sql_error)?.unwrap();
                project.policy.enabled = true;
                save(c, "example", project.revision, &project.policy, "test").map_err(sql_error)
            })
            .unwrap();
        // Once enabled, missing executable details still prevent verification.
        let before = board(&store.read().unwrap(), "example").unwrap();
        assert_eq!(before["cards"][0]["phase"], "waiting");
        assert_eq!(
            before["cards"][0]["execution_plan"]["waiting_reason"],
            "intake_required"
        );
        assert_eq!(before["cards"][0]["execution_plan"]["action"], "wait");
        store
            .write(|c| {
                c.execute(
                    "UPDATE issues SET session='new-executor' WHERE id='A-1'",
                    [],
                )?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .unwrap();
        let view = board(&store.read().unwrap(), "example").unwrap();
        assert_eq!(view["n_considered"], 1);
        assert_eq!(view["cards"][0]["project_group"], "example");
        assert_eq!(view["cards"][0]["assignee"], "new-executor");
        assert_eq!(view["cards"][0]["phase"], "waiting");
        assert_eq!(
            view["cards"][0]["execution_plan"]["waiting_reason"],
            "intake_required"
        );
        assert_eq!(view["cards"][0]["execution_plan"]["action"], "wait");
        assert_eq!(
            bs::get_issue(&store.read().unwrap(), "A-1")
                .unwrap()
                .unwrap()
                .status,
            "done"
        );
        assert_eq!(
            bs::get_issue(&store.read().unwrap(), "A-1")
                .unwrap()
                .unwrap()
                .evidence
                .as_deref(),
            Some("retained evidence")
        );
    }
}

/// Explicit migration of idle source boards. The preview fingerprint binds the
/// selected rows and policy revision; IDs, evidence and source messages survive.
pub fn apply_migration(
    conn: &Connection,
    name: &str,
    workers: &[String],
    fingerprint: &str,
) -> anyhow::Result<WriteOutcome> {
    let p = preview(conn, name, workers)?;
    anyhow::ensure!(p.fingerprint == fingerprint, "migration preview is stale");
    anyhow::ensure!(
        p.conflicts.is_empty(),
        "migration has unresolved conflicts: {}",
        p.conflicts.join("; ")
    );
    let policy = get(conn, name)?.ok_or_else(|| anyhow::anyhow!("project missing"))?;
    anyhow::ensure!(
        !policy.policy.enabled || policy.policy.paused,
        "pause project before migration"
    );
    let id = format!("project-migration:{}", ulid::Ulid::new());
    for row in &p.rows {
        anyhow::ensure!(row.project_group.is_none(), "{} already migrated", row.id);
        conn.execute("UPDATE issues SET project_group=?2,session=NULL,rev=rev+1,version=version+1 WHERE id=?1 AND rev=?3 AND project_group IS NULL",params![row.id,name,row.revision])?;
        let current =
            bs::get_issue(conn, &row.id)?.ok_or_else(|| anyhow::anyhow!("task disappeared"))?;
        if !bs::has_execution_details(&current)
            && current.item_type != "epic"
            && !bs::is_terminal_status(&current.status)
        {
            super::intake::receive(conn,name,&format!("migration:{}:{}",id,row.id),&format!("Structure existing task {} on this project board. Update or merge its canonical outcome, retaining every original constraint; do not create a duplicate. Original title: {}\nOriginal request: {}",row.id,current.title,current.desc))?;
        }
    }
    // Ownership changes must not turn a task's edges into invalid ones; the graph seam judges them
    // after the rows joined the project. A failure aborts the whole migration.
    let migrated: Vec<String> = p.rows.iter().map(|r| r.id.clone()).collect();
    if let Err((task, error)) = super::graph::validate_tasks(conn, name, &migrated, "migration") {
        anyhow::bail!("migration would leave {task} with an invalid dependency: {error}");
    }
    conn.execute("INSERT INTO session_events(ts,session,type,data,idem,source) VALUES(?1,?2,'project.migrated',?3,?4,'operator')",params![crate::config::now_f64(),format!("project:{name}"),serde_json::to_string(&p)?,id])?;
    tracing::info!(project=name,migration=%id,measured=true,n_considered=p.rows.len(),verdict="project_migrated","explicit board ownership migration committed");
    Ok(WriteOutcome {
        applied: true,
        events: vec![PendingEvent {
            entity_type: EntityType::Other("group_config".into()),
            entity_id: name.into(),
            mutation: MutationKind::Updated,
            payload: Some(json!({"migration":id,"rows":p.rows.len()})),
        }],
    })
}

pub fn rollback_migration(conn: &Connection, name: &str, id: &str) -> anyhow::Result<WriteOutcome> {
    let p = get(conn, name)?.ok_or_else(|| anyhow::anyhow!("project missing"))?;
    anyhow::ensure!(
        p.policy.paused || !p.policy.enabled,
        "pause project before rollback"
    );
    let raw: String = conn.query_row(
        "SELECT data FROM session_events WHERE session=?1 AND type='project.migrated' AND idem=?2",
        params![format!("project:{name}"), id],
        |r| r.get(0),
    )?;
    let before: MigrationPreview = serde_json::from_str(&raw)?;
    let undone: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_events WHERE idem=?1)",
        [format!("rollback:{id}")],
        |r| r.get(0),
    )?;
    if undone {
        return Ok(WriteOutcome {
            applied: false,
            events: vec![],
        });
    }
    for old in &before.rows {
        let row = bs::get_issue(conn, &old.id)?
            .ok_or_else(|| anyhow::anyhow!("{} disappeared", old.id))?;
        anyhow::ensure!(
            row.project_group.as_deref() == Some(name)
                && row.rev == old.revision + 1
                && row.lease_owner.is_none(),
            "{} changed since migration; reconcile instead of overwriting work",
            old.id
        );
    }
    let changes: Vec<_> = before
        .rows
        .iter()
        .map(|r| {
            (
                r.id.clone(),
                bs::BoardOwner::new(None, r.session.as_deref()),
            )
        })
        .collect();
    bs::validate_owner_changes(conn, &changes)?;
    for old in &before.rows {
        conn.execute("UPDATE issues SET project_group=NULL,session=?2,rev=rev+1,version=version+1 WHERE id=?1",params![old.id,old.session])?;
    }
    conn.execute("UPDATE cmd_history SET capture_pending=0,intake_result=json_object('state','cancelled','reason','source migration rolled back') WHERE project_group=?1 AND json_extract(client_meta,'$.idempotency_key') LIKE ?2 AND capture_pending!=0",params![name,format!("migration:{id}:%")])?;
    conn.execute("INSERT INTO session_events(ts,session,type,data,idem,source) VALUES(?1,?2,'project.migration_rolled_back',?3,?4,'operator')",params![crate::config::now_f64(),format!("project:{name}"),json!({"migration":id,"rows":before.rows.len()}).to_string(),format!("rollback:{id}")])?;
    Ok(WriteOutcome {
        applied: true,
        events: vec![PendingEvent {
            entity_type: EntityType::Other("group_config".into()),
            entity_id: name.into(),
            mutation: MutationKind::Updated,
            payload: None,
        }],
    })
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    #[test]
    fn project_migration_rollback_preserves_evidence_cancels_intake_and_refuses_changed_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Store::open(&dir.path().join("db")).unwrap();
        db.write(|c| {
            let policy=serde_json::from_value(json!({"repository":"/repo","coordinator":{"provider":"claude","model":"haiku"},"executor":{"provider":"claude","model":"sonnet"},"verify_command":"./verify.sh"})).unwrap();
            save(c,"migration",0,&policy,"test").map_err(sql_error)?;
            c.execute("INSERT INTO issues(id,title,status,session,evidence,created,updated) VALUES('M-1','Raw request','backlog','source','retain evidence',1,1)",[])?;
            let p=preview(c,"migration",&["source".into()]).unwrap();
            assert!(apply_migration(c,"migration",&p.source_workers,"stale").is_err());
            apply_migration(c,"migration",&p.source_workers,&p.fingerprint).map_err(sql_error)?;
            let row=bs::get_issue(c,"M-1")?.unwrap();
            assert_eq!(row.project_group.as_deref(),Some("migration"));
            assert_eq!(row.session,None);
            assert_eq!(row.evidence.as_deref(),Some("retain evidence"));
            assert_eq!(c.query_row("SELECT count(*) FROM legacy_execution_issues",[],|r|r.get::<_,i64>(0))?,0);
            assert_eq!(c.query_row("SELECT count(*) FROM cmd_history WHERE capture_pending=1",[],|r|r.get::<_,i64>(0))?,1);
            let id:String=c.query_row("SELECT idem FROM session_events WHERE type='project.migrated'",[],|r|r.get(0))?;
            c.execute("UPDATE issues SET rev=rev+1 WHERE id='M-1'",[])?;
            assert!(rollback_migration(c,"migration",&id).is_err());
            c.execute("UPDATE issues SET rev=rev-1 WHERE id='M-1'",[])?;
            c.execute("INSERT INTO issues(id,title,status,project_group,created,updated,depends_on) VALUES('M-IN','New dependent','backlog','migration',1,1,'[\"M-1\"]')",[])?;
            assert!(rollback_migration(c,"migration",&id).is_err(),"incoming project dependent prevents ownership rollback even when migrated row revision is unchanged");
            assert_eq!(bs::get_issue(c,"M-1")?.unwrap().project_group.as_deref(),Some("migration"));
            c.execute("DELETE FROM issues WHERE id='M-IN'",[])?;
            rollback_migration(c,"migration",&id).map_err(sql_error)?;
            assert!(!rollback_migration(c,"migration",&id).unwrap().applied);
            let row=bs::get_issue(c,"M-1")?.unwrap();
            assert_eq!(row.project_group,None);
            assert_eq!(row.session.as_deref(),Some("source"));
            assert_eq!(row.evidence.as_deref(),Some("retain evidence"));
            assert_eq!(c.query_row("SELECT count(*) FROM cmd_history WHERE capture_pending=1",[],|r|r.get::<_,i64>(0))?,0);
            Ok(WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
    }
}
