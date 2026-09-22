//! Project execution uses the existing provider launcher, durable steering and
//! Git integration. The project planner is the only claim authority.
use super::{
    planner::{self, Execution},
    store,
};
use crate::{
    api::{session_verbs as sv, AppState},
    db::{board_store as bs, WriteOutcome},
    fanout_workspace as workspace,
    runtime_jobs::board_drive::{Fleet, LiveFleet},
};
use rusqlite::params;
use serde_json::json;
use std::sync::{Arc, OnceLock};

fn permit(state: &AppState, project: &str, id: &str, expected: &Execution) -> Result<(), String> {
    let c = state.store.read().map_err(|e| e.to_string())?;
    let p = store::get(&c, project)
        .map_err(|e| e.to_string())?
        .ok_or("project disappeared")?;
    if !p.policy.enabled || p.policy.paused {
        return Err("project_paused_or_disabled".into());
    }
    let row = bs::get_issue(&c, id)
        .map_err(|e| e.to_string())?
        .ok_or("task disappeared")?;
    let actual = planner::execution(&c, id).map_err(|e| e.to_string())?;
    if actual.generation != expected.generation
        || actual.stage != expected.stage
        || actual.worker != expected.worker
        || actual.report != expected.report
        || actual.verification_retry_pending != expected.verification_retry_pending
        || actual.input_hash != expected.input_hash
        || planner::input_hash(&row) != expected.input_hash
        || row.project_group.as_deref() != Some(project)
        || row.archived != 0
    {
        return Err("claim or requirements changed".into());
    }
    if expected.verification_retry_pending {
        if let Some(reason) = super::usage::waiting(&c, &p).map_err(|e| e.to_string())? {
            return Err(reason);
        }
        if actual.suspended
            || actual.wait_category.is_some()
            || super::outputs::authorization_hold(&c, &row).map_err(|e| e.to_string())?
        {
            return Err("verification retry held by current authorization".into());
        }
    }
    if !super::outputs::ready(&c, &row).map_err(|e| e.to_string())? {
        return Err("required output no longer verified".into());
    }
    if ["CC_PAUSED", "CC_ARCHIVED", "CC_ISOLATED"]
        .iter()
        .any(|key| sv::parse_env(&expected.worker).get(key) == Some("1"))
    {
        return Err("executor is protected".into());
    }
    Ok(())
}

async fn prepare(
    state: &AppState,
    p: &store::Project,
    row: &bs::IssueRow,
    e: &Execution,
) -> Result<(), String> {
    permit(state, &p.name, &row.id, e)?;
    let path = sv::env_path(&e.worker);
    let mut env = sv::EnvFile::load(&path);
    if !p.policy.worktree {
        sync_shared_checkout(&p.policy.repository).await?;
    }
    if path.exists()
        && (env.get("CC_PROJECT") != Some(p.name.as_str())
            || env.get("CC_BOARD_CARD") != Some(row.id.as_str()))
    {
        return Err("executor name collision".into());
    }
    configure_executor_env(&mut env, p, row);
    env.write(&path).map_err(|e| e.to_string())?;
    if p.policy.worktree {
        workspace::ensure(&crate::config::amux_home(), &e.worker, &p.policy.repository).await?;
    }
    permit(state, &p.name, &row.id, e)?;
    if !sv::is_running(&e.worker).await {
        sv::start_for_board_dispatch(state, &e.worker).await?;
    }
    if let Err(error) = permit(state, &p.name, &row.id, e) {
        // Pause can commit while process startup is awaiting the backend.
        sv::stop_for_pause(state, &e.worker)
            .await
            .map_err(|e| e.to_string())?;
        return Err(error);
    }
    Ok(())
}

// Only diagnostic history is previewed; requirements and criteria stay exact.
fn previous_result(p: &store::Project, row: &bs::IssueRow, e: &Execution) -> serde_json::Value {
    let Some(failure) = e.last_failure.as_deref() else {
        return serde_json::Value::Null;
    };
    let chars = failure.chars().count();
    if chars <= 2048 {
        return json!(failure);
    }
    tracing::info!(project=%p.name,task=%row.id,generation=e.generation,measured=true,n_considered=chars,preview_chars=2048,verdict="project.retry_diagnostic_preview","retry packet elides diagnostic middle; full failure retained in project read");
    json!({"preview":crate::api::board::chars_elide_middle(failure,1024,1024),"truncated":true,"original_chars":chars,"original_bytes":failure.len(),"full_diagnostic":{"method":"GET","path":format!("/api/projects/{}",p.name),"card_id":row.id,"field":"cards[id == card_id].execution_plan.execution.last_failure","instruction":"Read the matching card's last_failure for full exact diagnostics before inspecting omitted details."}})
}

pub fn packet(p: &store::Project, row: &bs::IssueRow, e: &Execution) -> String {
    let output_protocol=format!("For an unavailable concrete same-project output, POST /api/projects/{}/tasks/{}/required-outputs with generation, input_hash, idempotency_key, required_outputs (explicit task IDs), reason, and replaces_wait (null for a new wait; exact prior waiting string to replace an operational wait). Never turn spend/customer authorization into outputs. Stop after declaration. When outputs are Verified the harness continues the SAME attempt with a fresh generation and delivery ID. On continuation fetch the accepted local origin/main and compose required commits into your own candidate without resetting your existing work, then rerun/report every criterion; an output arriving is not verification of your task. Required output receipts below identify accepted reports and integration evidence.",p.name,row.id);
    let checkout_instruction = if p.policy.worktree {
        "Execute this finite project task in your isolated worktree. Own all required implementation locally."
    } else {
        "Execute this finite project task in the project's shared checkout. This project is single-lane in shared-checkout mode; keep the checkout clean, commit the exact result, and do not start unrelated work."
    };
    format!(
        r#"{output_protocol}
{checkout_instruction} Do not create worker boards, delegate, change task status directly, send customer outbound, or increase spend. The harness controls claims, verification, main integration and retirement. Commit your changes, then report the exact HEAD and one executable candidate-relative check for EVERY acceptance criterion. The harness reruns these checks and the project gate. Report through POST /api/projects/{}/tasks/{}/report with X-Amux-Session set to your worker name. Body: {{"generation":{},"input_hash":"{}","report":{{"head":"40-character SHA","summary":"output","checks":[{{"criterion":"exact criterion","command":"falsifiable check"}}],"assets":[{{"path":"candidate-relative-report.md","sha256":"lowercase-hex-sha256"}}]}}}}. report.assets is required for new completed project tasks. Markdown/JSON reports must be committed at reported HEAD; PNG/WebM may be ignored candidate-local captures. Only these passive formats are retained and linked; never use prose paths as asset declarations. Stop after reporting. If blocked, POST /api/projects/{}/tasks/{}/wait with generation, input_hash, reason and category (operational, spend, customer_outbound). Never assert success without artifacts.
Task packet:
{}"#,
        p.name,
        row.id,
        e.generation,
        e.input_hash,
        p.name,
        row.id,
        json!({"id":row.id,"project":p.name,"worker":e.worker,"title":row.title,"description":row.desc,"criteria":row.acceptance_criteria.as_deref().and_then(|v|serde_json::from_str::<serde_json::Value>(v).ok()),"next_action":row.next_action,"required_outputs":row.depends_on,"output_handoff":e.output_wait,"attempt":e.attempt,"max_attempts":e.attempt_limit(p.policy.max_attempts),"previous_result":previous_result(p,row,e),"verification":p.policy.verify_command})
    )
}

async fn transition(
    state: &AppState,
    id: &str,
    expected: &Execution,
    stage: &str,
    waiting: Option<String>,
) -> anyhow::Result<()> {
    let id = id.to_string();
    let expected = expected.clone();
    let stage = stage.to_string();
    state
        .store
        .write_async(move |c| {
            let row = bs::get_issue(c, &id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            let mut e = planner::execution(c, &id).map_err(store::sql_error)?;
            if e.generation != expected.generation || e.stage != expected.stage {
                return Ok(WriteOutcome {
                    applied: false,
                    events: vec![],
                });
            }
            if waiting.is_some() && e.report.is_some() && matches!(stage.as_str(),"repair"|"waiting") {
                tracing::warn!(task=%id,action="verify",measured=true,n_considered=1,verdict="project_verification_failed","verification failed; exact diagnostic remains in execution waiting details");
            }
            if matches!(stage.as_str(),"waiting"|"repair") {e.verification_retry_pending=false;}
            e.stage = stage;
            e.waiting = waiting;
            e.observed_at = chrono::Utc::now().timestamp();
            planner::save_execution(c, &row, &e, "project.execution").map_err(store::sql_error)
        })
        .await?;
    Ok(())
}

fn verification_commands<'a>(gate: &'a str, report: &'a planner::Report) -> Vec<&'a str> {
    workspace::distinct_verification_commands(
        std::iter::once(gate).chain(report.checks.iter().map(|c| c.command.as_str())),
    )
}

/// One source-path policy, applied to the entire set before any shell command.
pub(crate) fn validated_verification_commands<'a>(
    w: &workspace::Workspace,
    gate: &'a str,
    report: &'a planner::Report,
) -> Result<Vec<&'a str>, String> {
    let commands = verification_commands(gate, report);
    for command in &commands {
        workspace::validate_verification_command(w, command)?;
    }
    Ok(commands)
}

fn project_effort_flags(provider: &str, effort: &str) -> Option<String> {
    if effort.is_empty() {
        None
    } else if provider == "codex" {
        Some(format!("-c model_reasoning_effort={effort}"))
    } else if provider == "ollama" {
        None
    } else {
        Some(format!("--effort {effort}"))
    }
}

fn executor_flags(provider: &str, effort: Option<&str>) -> String {
    let mut flags = if provider == "claude" {
        "--dangerously-skip-permissions".to_string()
    } else {
        String::new()
    };
    if let Some(effort) = effort.and_then(|effort| project_effort_flags(provider, effort)) {
        if !flags.is_empty() {
            flags.push(' ');
        }
        flags.push_str(&effort);
    }
    flags
}

fn configure_executor_env(env: &mut sv::EnvFile, p: &store::Project, row: &bs::IssueRow) {
    for (key, value) in [
        ("CC_DIR", p.policy.repository.as_str()),
        ("CC_PROJECT", p.name.as_str()),
        ("CC_BOARD_CARD", row.id.as_str()),
        ("CC_EPHEMERAL", "1"),
        ("CC_WORKTREE", if p.policy.worktree { "1" } else { "0" }),
        ("CC_AUTO_PICKUP", "0"),
        ("CC_AUTO_CONTINUE", "0"),
        ("CC_WORKTREE_AUTO_MERGE", "0"),
        ("AMUX_BOARD_DELEGATION", "0"),
        ("CC_PROVIDER", p.policy.executor.provider.as_str()),
        ("CC_DESC", row.title.as_str()),
        ("CC_WORKTREE_VERIFY", p.policy.verify_command.as_str()),
    ] {
        env.set(key, value);
    }
    env.set("CC_TAGS", &format!("{},ephemeral", p.name));
    let flags = executor_flags(
        &p.policy.executor.provider,
        p.policy.executor.effort.as_deref(),
    );
    let flags = sv::route_model_to_env(
        env,
        &p.policy.executor.provider,
        &p.policy.executor.model,
        &flags,
    );
    env.set("CC_FLAGS", &flags);
}

async fn sync_shared_checkout(repo: &str) -> Result<(), String> {
    let root = workspace::git(repo, &["rev-parse", "--show-toplevel"]).await?;
    if !workspace::git(&root, &["status", "--porcelain"])
        .await?
        .is_empty()
    {
        return Err(
            "shared checkout has uncommitted changes; clean it or enable dedicated worktrees"
                .into(),
        );
    }
    let branch = workspace::git(&root, &["branch", "--show-current"]).await?;
    if branch != "main" {
        return Err(format!("shared-checkout project must run from the main branch, found {branch}; enable dedicated worktrees for branch work"));
    }
    if let Err(error) = workspace::git(&root, &["fetch", "origin", "main"]).await {
        tracing::warn!(%error,repo=%root,verdict="project_shared_checkout_fetch_failed",
            "shared-checkout project could not fetch origin/main before execution; continuing only if local main is all that exists");
        return Ok(());
    }
    let head = workspace::git(&root, &["rev-parse", "HEAD"]).await?;
    let main = workspace::git(&root, &["rev-parse", "origin/main"]).await?;
    if head == main {
        return Ok(());
    }
    workspace::git(&root, &["merge-base", "--is-ancestor", &head, &main])
        .await
        .map_err(|_| "shared checkout has local commits not contained in origin/main; reconcile it or enable dedicated worktrees".to_string())?;
    workspace::git(&root, &["merge", "--ff-only", "origin/main"]).await?;
    if workspace::git(&root, &["rev-parse", "HEAD"]).await? != main
        || !workspace::git(&root, &["status", "--porcelain"])
            .await?
            .is_empty()
    {
        return Err("shared checkout did not fast-forward cleanly to origin/main".into());
    }
    Ok(())
}

async fn integrate_shared_checkout(w: &workspace::Workspace, head: &str) -> Result<String, String> {
    if workspace::git(&w.path, &["rev-parse", "HEAD"]).await? != head {
        return Err("shared checkout changed after verification".into());
    }
    if !workspace::git(&w.path, &["status", "--porcelain"])
        .await?
        .is_empty()
    {
        return Err("shared checkout has uncommitted changes".into());
    }
    let main = match workspace::git(&w.repo, &["fetch", "origin", "main"]).await {
        Ok(_) => Some(workspace::git(&w.repo, &["rev-parse", "origin/main"]).await?),
        Err(error) => {
            if w.branch == "main" {
                tracing::warn!(branch=%w.branch,%error,verdict="project_shared_checkout_local_main",
                    "origin/main unavailable; treating the named local main checkout as the integrated target");
                return Ok(head.to_string());
            }
            return Err(format!("shared-checkout project cannot confirm origin/main from branch {}; either use the main checkout or enable dedicated worktrees: {error}", w.branch));
        }
    };
    if let Some(main) = main.as_deref() {
        if workspace::git(&w.repo, &["merge-base", "--is-ancestor", head, main])
            .await
            .is_ok()
        {
            return Ok(main.to_string());
        }
    }
    if w.branch != "main" {
        return Err("shared-checkout project report is not contained in origin/main; use the main checkout or enable dedicated worktrees for branch integration".into());
    }
    workspace::git(&w.repo, &["push", "origin", "HEAD:refs/heads/main"]).await?;
    workspace::git(&w.repo, &["fetch", "origin", "main"]).await?;
    let main = workspace::git(&w.repo, &["rev-parse", "origin/main"]).await?;
    workspace::git(&w.repo, &["merge-base", "--is-ancestor", head, &main])
        .await
        .map_err(|_| {
            "shared-checkout head is not contained in origin/main after push".to_string()
        })?;
    Ok(main)
}

async fn verify(
    state: &AppState,
    p: &store::Project,
    id: &str,
    e: &Execution,
) -> Result<(), String> {
    let verification_permit = || {
        permit(state, &p.name, id, e)?;
        let c = state.store.read().map_err(|e| e.to_string())?;
        let current = store::get(&c, &p.name)
            .map_err(|e| e.to_string())?
            .ok_or("project disappeared")?;
        if current.policy.repository != p.policy.repository
            || current.policy.verify_command != p.policy.verify_command
            || current.policy.verification_timeout_secs != p.policy.verification_timeout_secs
        {
            return Err("verification policy changed; rerun checks with current policy".into());
        }
        Ok(())
    };
    let report = e.report.as_ref().ok_or("no report")?;
    verification_permit()?;
    let home = crate::config::amux_home();
    let w = if p.policy.worktree {
        let w = workspace::load(&home, &e.worker).ok_or("workspace missing")?;
        if !workspace::same_repository(&w.repo, &p.policy.repository)
            || w.branch != format!("amux/fanout/{}", e.worker)
        {
            return Err("registered workspace does not match project executor".into());
        }
        w
    } else {
        let repo = workspace::git(&p.policy.repository, &["rev-parse", "--show-toplevel"]).await?;
        let branch = workspace::git(&repo, &["branch", "--show-current"]).await?;
        if branch.trim().is_empty() {
            return Err("shared-checkout project executor is detached; use a named branch or enable dedicated worktrees".into());
        }
        workspace::Workspace {
            repo: repo.clone(),
            path: repo,
            branch,
            base: String::new(),
        }
    };
    if workspace::git(&w.path, &["rev-parse", "HEAD"]).await? != report.head {
        return Err("reported head is stale".into());
    }
    if !workspace::git(&w.path, &["status", "--porcelain"])
        .await?
        .is_empty()
    {
        return Err("worktree has uncommitted changes".into());
    }
    let commands = validated_verification_commands(&w, &p.policy.verify_command, report)?;
    tracing::info!(task=id,measured=true,n_considered=report.checks.len()+1,distinct=commands.len(),verdict="project.verification_commands","byte-identical commands run once per immutable candidate phase; criterion mappings retained");
    let timeout = std::time::Duration::from_secs(p.policy.verification_timeout_secs);
    workspace::verify_commands(&w, &w.path, &commands, timeout, &verification_permit).await?;
    if workspace::git(&w.path, &["rev-parse", "HEAD"]).await? != report.head
        || !workspace::git(&w.path, &["status", "--porcelain"])
            .await?
            .is_empty()
    {
        return Err("verification changed the reported worktree".into());
    }
    let retained = super::assets::retain(&home, std::path::Path::new(&w.path), report)
        .await
        .map_err(|e| e.to_string())?;
    let merged = if p.policy.worktree {
        workspace::integrate_checks(&w, &commands, timeout, &verification_permit).await?
    } else {
        integrate_shared_checkout(&w, &report.head).await?
    };
    verification_permit()?;
    if workspace::git(&w.path, &["rev-parse", "HEAD"]).await? != report.head
        || !workspace::git(&w.path, &["status", "--porcelain"])
            .await?
            .is_empty()
    {
        return Err("worktree changed during verification".into());
    }
    workspace::write_integration_status(
        &home,
        &e.worker,
        &json!({"status":"integrated","head":report.head,"merged":merged,"mode":if p.policy.worktree {"worktree"} else {"shared_checkout"},"branch":w.branch}),
    );
    let expected_policy = p.policy.clone();
    let (id, expected, project) = (id.to_string(), e.clone(), p.name.clone());
    state.store.write_async(move|c| {
        let row=bs::get_issue(c,&id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        let mut current=planner::execution(c,&id).map_err(store::sql_error)?;
        let policy=store::get(c,&project).map_err(store::sql_error)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        if policy.policy!=expected_policy || current.report!=expected.report || current.verification_retry_pending!=expected.verification_retry_pending || current.stage!=expected.stage || current.generation!=expected.generation || planner::input_hash(&row)!=expected.input_hash || policy.policy.paused || !policy.policy.enabled || !super::outputs::ready(c,&row).map_err(store::sql_error)? {return Err(rusqlite::Error::InvalidQuery);}
        super::assets::check(&retained).map_err(store::sql_error)?;
        super::assets::register(c,&id,&retained)?;
        current.retained_assets=retained;
        current.stage="verified".into();current.waiting=None;current.verification_retry_pending=false;
        c.execute("UPDATE issues SET status='verified',evidence=?2,lease_owner=NULL,lease_expires_at=NULL WHERE id=?1",params![id,json!({"report":current.report,"merged":merged,"gate":policy.policy.verify_command}).to_string()])?;
        planner::save_execution(c,&row,&current,"project.verified").map_err(store::sql_error)
    }).await.map_err(|e|e.to_string())?;
    Ok(())
}

#[derive(Clone)]
struct TurnObservation {
    running: bool,
    idle: bool,
    ended_at: Option<f64>,
    report: serde_json::Value,
}

fn turn_observation(
    signals: &crate::api::sessions_legacy::FleetSignals,
    worker: &str,
) -> TurnObservation {
    let running = signals.agent_running(&format!("amux-{worker}"));
    let (_, explain) = signals.derive_status_explain(worker, running);
    let idle = signals.turn_boundary_status(worker).as_deref() == Some("idle")
        && explain["subagents_working"] != true
        && explain["provider_background_working"] != true;
    let report = signals
        .reports
        .get(worker)
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let ended_at = if explain["decided_by"] == "report" && report["state"] == "idle" {
        report["ts"].as_f64()
    } else if let Some(turn) = signals
        .codex_turns
        .get(worker)
        .filter(|s| s.state == "idle")
    {
        Some(turn.ts)
    } else if signals.hookless_workers.contains(worker) && idle {
        signals
            .activity
            .get(&format!("amux-{worker}"))
            .map(|ts| *ts as f64)
    } else {
        None
    };
    TurnObservation {
        running,
        idle,
        ended_at,
        report,
    }
}

/// Receipt first, fresh boundary second, compare-and-transition last. Never pair
/// a pre-delivery idle snapshot with a newly written terminal delivery receipt.
async fn observe_with<F, Fut>(
    state: &AppState,
    project: &str,
    id: &str,
    expected: &Execution,
    probe: F,
) -> anyhow::Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Option<TurnObservation>>,
{
    if chrono::Utc::now().timestamp() - expected.observed_at <= 30 {
        return Ok(());
    }
    let receipt = {
        let c = state.store.read()?;
        planner::settled_delivery(&c, expected)?
    };
    let Some(observation) = probe().await else {
        return Ok(());
    };
    let interrupted = receipt
        .as_ref()
        .is_some_and(|r| r.outcome.starts_with("interrupted:"));
    let ended = observation.idle
        && receipt.as_ref().is_some_and(|r| {
            interrupted || observation.ended_at.is_some_and(|ts| ts >= r.submitted_at)
        });
    if observation.running && !ended {
        if receipt.is_some()
            && observation.idle
            && crate::log_dedupe::first_this_bucket(
                &format!("project-stale-idle:{}", expected.delivery_id),
                chrono::Utc::now().timestamp() / 3600,
            )
        {
            tracing::info!(task=id,delivery_id=%expected.delivery_id,measured=true,n_considered=1,verdict="project_stale_idle_held","idle evidence predates this delivery; attempt retained");
        }
        return Ok(());
    }
    let (project, id, expected) = (project.to_string(), id.to_string(), expected.clone());
    state.store.write_async(move|c| {
        let row=bs::get_issue(c,&id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        let mut current=planner::execution(c,&id).map_err(store::sql_error)?;
        let p=store::get(c,&project).map_err(store::sql_error)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        let reports:serde_json::Value=c.query_row("SELECT value FROM prefs WHERE key='session_reports'",[],|r|r.get::<_,String>(0)).ok().and_then(|raw|serde_json::from_str(&raw).ok()).unwrap_or(serde_json::Value::Null);
        let report=reports.get(&expected.worker).cloned().unwrap_or(serde_json::Value::Null);
        if current.generation!=expected.generation || current.stage!="working" || current.report.is_some()
            || current.worker!=expected.worker || current.delivery_id!=expected.delivery_id
            || current.attempt!=expected.attempt || current.observed_at!=expected.observed_at
            || current.input_hash!=expected.input_hash || planner::input_hash(&row)!=expected.input_hash
            || current.suspended || current.waiting.is_some() || current.wait_category.is_some() || row.project_group.as_deref()!=Some(project.as_str())
            || row.status!="doing" || row.archived!=0 || !p.policy.enabled || p.policy.paused
            || !super::outputs::ready(c,&row).map_err(store::sql_error)?
            || super::outputs::authorization_hold(c,&row).map_err(store::sql_error)?
            || planner::settled_delivery(c,&current)?!=receipt || report!=observation.report
            || c.query_row("SELECT EXISTS(SELECT 1 FROM steering_queue WHERE session=?1 AND delivering_since IS NOT NULL)",[&current.worker],|r|r.get::<_,bool>(0))? {
            return Ok(WriteOutcome{applied:false,events:vec![]});
        }
        current.stage=if current.attempt<current.attempt_limit(p.policy.max_attempts){"repair"}else{"waiting"}.into();
        current.waiting=Some(if observation.running{"executor_returned_without_result"}else{"executor_stopped_before_result"}.into());
        current.observed_at=chrono::Utc::now().timestamp();
        tracing::warn!(task=%id,delivery_id=%current.delivery_id,measured=true,n_considered=1,verdict="project_current_turn_ended_without_result",interrupted,"current delivery/boundary or stopped executor permits bounded recovery; unsent packet remains retained");
        planner::save_execution(c,&row,&current,"project.execution").map_err(store::sql_error)
    }).await?;
    Ok(())
}

fn repair_after_failure(e: &Execution, max_attempts: u32, action: &str, error: &str) -> bool {
    !e.verification_retry_pending
        && e.attempt < e.attempt_limit(max_attempts)
        && (action == "verify"
            || matches!(
                error,
                "executor_stopped_before_result" | "executor_returned_without_result"
            ))
}

pub(crate) async fn drive_project(state: &AppState, name: &str) -> anyhow::Result<()> {
    let p = {
        let c = state.store.read()?;
        store::get(&c, name)?.ok_or_else(|| anyhow::anyhow!("project missing"))?
    };
    let plans = {
        let c = state.store.read()?;
        planner::plan(&c, &p)?
    };
    if p.policy.paused || !p.policy.enabled {
        apply_pause(state, name, true).await?;
        return Ok(());
    }
    let fleet = LiveFleet::snapshot(state.clone()).await;
    for plan in plans {
        let id = plan.id;
        let e = plan.execution;
        let result: Result<(), String> = match plan.action.as_str() {
            "claim" | "resume_outputs" => {
                if !e.worker.is_empty()
                    && (fleet.active_child_work(&e.worker)
                        || (fleet.is_running(&e.worker).await
                            && !fleet.at_boundary(&e.worker).await))
                {
                    continue;
                }
                let continuation = plan.action == "resume_outputs";
                let (id, project) = (id.clone(), name.to_string());
                state
                    .store
                    .write_async(move |c| {
                        if continuation {
                            super::outputs::resume(c, &project, &id).map_err(store::sql_error)
                        } else {
                            planner::claim(c, &project, &id).map_err(store::sql_error)
                        }
                    })
                    .await?;
                Ok(())
            }
            "deliver" => {
                let row = {
                    let c = state.store.read()?;
                    bs::get_issue(&c, &id)?.ok_or_else(|| anyhow::anyhow!("task missing"))?
                };
                match prepare(state, &p, &row, &e).await {
                    Ok(()) => match sv::steer_enqueue_idempotent_report(
                        state,
                        &e.worker,
                        &packet(&p, &row, &e),
                        "project-execution",
                        "",
                        &e.delivery_id,
                    )
                    .await
                    {
                        Ok(_) => {
                            transition(state, &id, &e, "working", None).await?;
                            Ok(())
                        }
                        Err(error) => Err(error.into()),
                    },
                    Err(error) => Err(error),
                }
            }
            "verify" => {
                if fleet.active_child_work(&e.worker)
                    || (fleet.is_running(&e.worker).await && !fleet.at_boundary(&e.worker).await)
                {
                    continue;
                }
                verify(state, &p, &id, &e).await
            }
            "observe" => {
                observe_with(state, name, &id, &e, || async {
                    sv::boundary_signals(state, Some(&e.worker))
                        .await
                        .map(|signals| turn_observation(&signals, &e.worker))
                })
                .await?;
                Ok(())
            }
            "complete_epic" => {
                let id = id.clone();
                state.store.write_async(move|c| {
                    c.execute("UPDATE issues SET status='verified',evidence='Every required project outcome verified',rev=rev+1,version=version+1 WHERE id=?1 AND NOT EXISTS(SELECT 1 FROM json_each(issues.depends_on) d LEFT JOIN issues p ON p.id=d.value WHERE p.status IS NULL OR p.status!='verified')",[id])?;
                    Ok(WriteOutcome{applied:true,events:vec![]})
                }).await?;
                Ok(())
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            let paused = {
                let c = state.store.read()?;
                store::get(&c, name)?.is_none_or(|p| p.policy.paused || !p.policy.enabled)
            };
            if paused {
                continue;
            }
            let repair = repair_after_failure(&e, p.policy.max_attempts, &plan.action, &error);
            transition(
                state,
                &id,
                &e,
                if repair { "repair" } else { "waiting" },
                Some(error),
            )
            .await?;
        }
        if e.stage == "verified" && crate::api::session_verbs::env_path(&e.worker).exists() {
            if let Err(error) = super::assets::check(&e.retained_assets) {
                tracing::warn!(task=%id,%error,verdict="project.asset_retention_failed",measured=true,n_considered=e.retained_assets.len(),"workspace disposal refused");
                continue;
            }
            if let Err(error) = crate::fanout_retirement::retire(
                state,
                &fleet,
                &crate::config::amux_home(),
                &e.worker,
            )
            .await
            {
                tracing::warn!(project=name,task=%id,%error,verdict="project_cleanup_deferred",measured=true,n_considered=1,"verified output retained until safe disposal");
            }
        }
    }
    Ok(())
}

/// In-memory exclusion bounds concurrent I/O. Durable claims/reports remain the
/// restart authority; dropping this set cannot create another assignment.
pub(crate) async fn tick(state: &AppState) {
    static RUNNING: OnceLock<Arc<std::sync::Mutex<std::collections::HashSet<String>>>> =
        OnceLock::new();
    let running = RUNNING.get_or_init(Default::default).clone();
    super::intake::recover_pending(state).await;
    let projects = {
        let Ok(c) = state.store.read() else { return };
        let Ok(p) = store::list(&c) else { return };
        p
    };
    for project in projects {
        if !running
            .lock()
            .expect("project runners")
            .insert(project.name.clone())
        {
            continue;
        }
        let (state, running) = (state.clone(), running.clone());
        tokio::spawn(async move {
            if let Err(error) = drive_project(&state, &project.name).await {
                tracing::warn!(project=%project.name,%error,verdict="project_tick_failed",measured=true,n_considered=1,"durable project state retained for recovery");
            }
            let current = state
                .store
                .read()
                .ok()
                .and_then(|c| store::get(&c, &project.name).ok().flatten());
            if let Some(current) = current {
                if let Err(error) = super::acceptance::tick(&state, &current).await {
                    tracing::warn!(project=%project.name,%error,verdict="project_acceptance_tick_failed",measured=true,n_considered=1,"project acceptance remains pending for bounded retry");
                }
            }
            running
                .lock()
                .expect("project runners")
                .remove(&project.name);
        });
    }
}

pub(crate) async fn apply_pause(state: &AppState, name: &str, paused: bool) -> anyhow::Result<()> {
    let rows = {
        let c = state.store.read()?;
        bs::project_issues(&c, name)?
    };
    for row in rows {
        let e = {
            let c = state.store.read()?;
            planner::execution(&c, &row.id)?
        };
        if e.worker.is_empty() {
            continue;
        }
        let path = sv::env_path(&e.worker);
        let mut env = sv::EnvFile::load(&path);
        if path.exists() && env.get("CC_PROJECT") != Some(name) {
            continue;
        }
        if paused {
            if path.exists() {
                // Preserve an independent pause when the project resumes.
                if env.get("CC_PAUSED") != Some("1") {
                    env.set("CC_PROJECT_PAUSED", "1");
                    env.set("CC_PAUSED", "1");
                    env.write(&path)?;
                }
                if !e.suspended || sv::is_running(&e.worker).await {
                    sv::stop_for_pause(state, &e.worker).await?;
                }
            }
            if !e.suspended {
                let id = row.id;
                state
                    .store
                    .write_async(move |c| {
                        let row =
                            bs::get_issue(c, &id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
                        let mut current = planner::execution(c, &id).map_err(store::sql_error)?;
                        current.suspended = true;
                        planner::save_execution(c, &row, &current, "project.paused")
                            .map_err(store::sql_error)
                    })
                    .await?;
            }
        } else if env.get("CC_PROJECT_PAUSED") == Some("1") || (!path.exists() && e.suspended) {
            if path.exists() {
                env.remove("CC_PROJECT_PAUSED");
                env.remove("CC_PAUSED");
                env.write(&path)?;
            }
            let id = row.id;
            state.store.write_async(move|c| {
                let row=bs::get_issue(c,&id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
                let mut e=planner::execution(c,&id).map_err(store::sql_error)?;
                e.suspended=false;
                if matches!(e.stage.as_str(),"working"|"reserved") {
                    let removed=c.execute("DELETE FROM steering_queue WHERE session=?1 AND guard='project-execution'",[&e.worker])?;
                    tracing::info!(session=%e.worker,removed,verdict="project_resume_superseded_deliveries",measured=true,n_considered=removed,"resume replaces only its prior execution packets");
                    e.generation+=1;e.delivery_id=format!("project-resume:{}:{}",id,e.generation);
                    if let Some(output)=e.output_wait.as_mut().filter(|o|o.continued_generation.is_some()) {output.continued_generation=Some(e.generation);}
                    e.stage="reserved".into();e.waiting=None;
                    c.execute("UPDATE issues SET lease_generation=?2 WHERE id=?1",params![id,e.generation])?;
                }
                planner::save_execution(c,&row,&e,"project.resumed").map_err(store::sql_error)
            }).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    fn write_report(c: &rusqlite::Connection, worker: &str, ts: f64) {
        c.execute("INSERT INTO prefs(key,value) VALUES('session_reports',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[json!({worker:{"state":"idle","source":"stop-hook","ts":ts}}).to_string()]).unwrap();
    }
    fn observation(worker: &str, ts: f64) -> TurnObservation {
        let _ = worker;
        TurnObservation {
            running: true,
            idle: true,
            ended_at: Some(ts),
            report: json!({"state":"idle","source":"stop-hook","ts":ts}),
        }
    }
    #[test]
    fn project_observation_rejects_delayed_delivery_old_idle_and_report_races() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        let (_dir, db, _) = super::super::outputs::tests::fixture();
        db.write(|c| {
            let row=bs::get_issue(c,"A")?.unwrap();let mut e=planner::execution(c,"A").unwrap();
            e.stage="working".into();e.waiting=None;e.attempt=1;e.observed_at=1;
            planner::register_test_workspace(&e.worker,"/repo");
            write_report(c,&e.worker,95.0);
            c.execute("INSERT INTO steering_queue(id,session,text,queued_at,guard,delivering_since) VALUES(?1,?2,'packet',94,'project-execution',100)",params![e.delivery_id,e.worker])?;
            c.execute("INSERT INTO session_events(ts,session,type,data) VALUES(100,?1,'project.delivery_started',?2)",params![e.worker,json!({"delivery_id":e.delivery_id}).to_string()])?;
            planner::save_execution(c,&row,&e,"project.execution").map_err(store::sql_error)
        }).unwrap();
        let state = AppState {
            store: Arc::new(db),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let e = planner::execution(&state.store.read().unwrap(), "A").unwrap();
        let old_idle = observation(&e.worker, 95.0);
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let probes=std::sync::atomic::AtomicUsize::new(0);
            observe_with(&state,"sample","A",&e,||async {probes.fetch_add(1,std::sync::atomic::Ordering::SeqCst);Some(old_idle.clone())}).await.unwrap();
            assert_eq!(probes.load(std::sync::atomic::Ordering::SeqCst),1,"liveness remains observable even during delivery");
            assert_eq!(planner::execution(&state.store.read().unwrap(),"A").unwrap().stage,"working");
            let delivery=e.clone();state.store.write(move|c| {
                c.execute("DELETE FROM steering_queue WHERE id=?1",[&delivery.delivery_id])?;
                c.execute("INSERT INTO steering_history(id,session,text,queued_at,delivered_at,outcome) VALUES(?1,?2,'packet',94,150,'sent')",params![delivery.delivery_id,delivery.worker])?;
                Ok(WriteOutcome{applied:true,events:vec![]})
            }).unwrap();
            // Boot idle 95 is AFTER enqueue 94 but BEFORE actual typing 100.
            // A terminal receipt at 150 cannot make that old idle current.
            observe_with(&state,"sample","A",&e,||async {Some(old_idle.clone())}).await.unwrap();
            let current=planner::execution(&state.store.read().unwrap(),"A").unwrap();
            assert_eq!(current.stage,"working","old idle plus new receipt must not consume a repair");assert_eq!(current.attempt,1);
            // A real stop after submission may precede acknowledgment; it is still current.
            let worker=e.worker.clone();state.store.write(move|c| {write_report(c,&worker,120.0);Ok(WriteOutcome{applied:true,events:vec![]})}).unwrap();
            let ended=observation(&e.worker,120.0);
            // Writer revalidation: newer prompt activity arrives during the probe.
            let worker=e.worker.clone();let store=state.store.clone();
            observe_with(&state,"sample","A",&e,||async move {
                store.write(move|c| {c.execute("UPDATE prefs SET value=?1 WHERE key='session_reports'",[json!({worker:{"state":"active","source":"prompt-hook","ts":151}}).to_string()])?;Ok(WriteOutcome{applied:true,events:vec![]})}).unwrap();Some(ended)
            }).await.unwrap();
            assert_eq!(planner::execution(&state.store.read().unwrap(),"A").unwrap().stage,"working");
            // A concurrent valid result wins even if observation had a valid idle.
            let worker=e.worker.clone();state.store.write(move|c| {write_report(c,&worker,120.0);Ok(WriteOutcome{applied:true,events:vec![]})}).unwrap();
            let store=state.store.clone();let report_e=e.clone();let ended=observation(&e.worker,120.0);
            observe_with(&state,"sample","A",&e,||async move {
                store.write(move|c| planner::record_report(c,"sample","A",&report_e.worker,report_e.generation,&report_e.input_hash,&planner::Report{head:"a".repeat(40),summary:"valid concurrent report".into(),assets:vec![super::super::assets::Asset{path:"report.md".into(),sha256:"0".repeat(64)}],checks:vec![planner::Check{criterion:"Output passes".into(),command:"true".into()}]}).map_err(store::sql_error)).unwrap();Some(ended)
            }).await.unwrap();
            let current=planner::execution(&state.store.read().unwrap(),"A").unwrap();assert_eq!(current.stage,"reported");assert_eq!(current.attempt,1);assert!(current.report.is_some());
        });
    }
    #[test]
    fn project_observation_stopped_before_submission_retains_packet_and_bounds_recovery() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        let (_dir, db, _) = super::super::outputs::tests::fixture();
        db.write(|c| {
            let row=bs::get_issue(c,"A")?.unwrap();let mut e=planner::execution(c,"A").unwrap();
            e.stage="working".into();e.waiting=None;e.attempt=1;e.observed_at=1;
            write_report(c,&e.worker,95.0);
            c.execute("INSERT INTO steering_queue(id,session,text,queued_at,guard) VALUES(?1,?2,'original unsent packet',94,'project-execution')",params![e.delivery_id,e.worker])?;
            planner::save_execution(c,&row,&e,"project.execution").map_err(store::sql_error)
        }).unwrap();
        let state = AppState {
            store: Arc::new(db),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let e = planner::execution(&state.store.read().unwrap(), "A").unwrap();
        let path = sv::env_path(&e.worker);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "CC_PROJECT=sample\nCC_BOARD_CARD=A\n").unwrap();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            // A delivery acquiring the packet during the probe wins the writer race.
            let db = state.store.clone();
            let id = e.delivery_id.clone();
            let worker = e.worker.clone();
            observe_with(&state, "sample", "A", &e, || async move {
                db.write(move |c| {
                    c.execute(
                        "UPDATE steering_queue SET delivering_since=100 WHERE id=?1",
                        [id],
                    )?;
                    Ok(WriteOutcome {
                        applied: true,
                        events: vec![],
                    })
                })
                .unwrap();
                Some(TurnObservation {
                    running: false,
                    ..observation(&worker, 95.0)
                })
            })
            .await
            .unwrap();
            assert_eq!(
                planner::execution(&state.store.read().unwrap(), "A")
                    .unwrap()
                    .stage,
                "working"
            );
            state
                .store
                .write(|c| {
                    c.execute("UPDATE steering_queue SET delivering_since=NULL", [])?;
                    Ok(WriteOutcome {
                        applied: true,
                        events: vec![],
                    })
                })
                .unwrap();
            for _ in 0..2 {
                observe_with(&state, "sample", "A", &e, || async {
                    Some(TurnObservation {
                        running: false,
                        ..observation(&e.worker, 95.0)
                    })
                })
                .await
                .unwrap();
            }
        });
        {
            let c = state.store.read().unwrap();
            let current = planner::execution(&c, "A").unwrap();
            assert_eq!(current.stage, "repair");
            assert_eq!(current.attempt, 1);
            assert_eq!(
                current.waiting.as_deref(),
                Some("executor_stopped_before_result")
            );
            assert_eq!(
                c.query_row(
                    "SELECT text FROM steering_queue WHERE id=?1",
                    [&e.delivery_id],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
                "original unsent packet"
            );
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM steering_history WHERE id=?1",
                    [&e.delivery_id],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                0,
                "never manufacture sent evidence"
            );
            assert!(
                crate::api::projects::steering_delivery_hold(&c, &e.worker, &e.delivery_id)
                    .unwrap()
                    .is_some()
            );
        }
        state
            .store
            .write(|c| planner::claim(c, "sample", "A").map_err(store::sql_error))
            .unwrap();
        {
            let c = state.store.read().unwrap();
            let next = planner::execution(&c, "A").unwrap();
            assert_eq!(next.attempt, 2);
            assert_eq!(next.generation, e.generation + 1);
            assert!(
                crate::api::projects::steering_delivery_hold(&c, &e.worker, &e.delivery_id)
                    .unwrap()
                    .is_some(),
                "old packet cannot cross into the new claim"
            );
        }
        state
            .store
            .write(|c| {
                let row = bs::get_issue(c, "A")?.unwrap();
                let mut e = planner::execution(c, "A").unwrap();
                e.stage = "working".into();
                e.observed_at = 1;
                planner::save_execution(c, &row, &e, "project.execution").map_err(store::sql_error)
            })
            .unwrap();
        let last = planner::execution(&state.store.read().unwrap(), "A").unwrap();
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(observe_with(&state, "sample", "A", &last, || async {
                Some(TurnObservation {
                    running: false,
                    ..observation(&last.worker, 95.0)
                })
            }))
            .unwrap();
        assert_eq!(
            planner::execution(&state.store.read().unwrap(), "A")
                .unwrap()
                .stage,
            "waiting"
        );
        let snapshot = || {
            let c = state.store.read().unwrap();
            let execution = serde_json::to_value(planner::execution(&c, "A").unwrap()).unwrap();
            let rows = ["task_attempts", "steering_queue", "steering_history"].map(|table| {
                let mut stmt = c
                    .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                    .unwrap();
                let columns = stmt.column_count();
                let rows = stmt
                    .query_map([], |row| {
                        Ok((0..columns)
                            .map(|i| format!("{:?}", row.get_ref(i).unwrap()))
                            .collect::<Vec<_>>())
                    })
                    .unwrap();
                rows.collect::<Result<Vec<_>, _>>().unwrap()
            });
            (execution, rows)
        };
        let before = snapshot();
        let refused = state
            .store
            .write(|c| planner::claim(c, "sample", "A").map_err(store::sql_error))
            .unwrap();
        assert!(
            !refused.applied,
            "stopped process does not create unlimited attempts"
        );
        assert_eq!(snapshot(),before,"exhausted claim preserves exact attempt/generation, attempt history and original queue/delivery identity");
    }
    #[test]
    fn project_observation_current_ended_and_interrupted_turns_recover_boundedly() {
        for outcome in ["sent", "interrupted: server restart"] {
            let (_dir, db, _) = super::super::outputs::tests::fixture();
            db.write(move|c| {
                let row=bs::get_issue(c,"A")?.unwrap();let mut e=planner::execution(c,"A").unwrap();e.stage="working".into();e.waiting=None;e.attempt=1;e.observed_at=1;
                write_report(c,&e.worker,120.0);
                c.execute("INSERT INTO steering_history(id,session,text,delivered_at,outcome) VALUES(?1,?2,'packet',150,?3)",params![e.delivery_id,e.worker,outcome])?;
                c.execute("INSERT INTO session_events(ts,session,type,data) VALUES(100,?1,'project.delivery_started',?2)",params![e.worker,json!({"delivery_id":e.delivery_id}).to_string()])?;
                planner::save_execution(c,&row,&e,"project.execution").map_err(store::sql_error)
            }).unwrap();
            let state = AppState {
                store: Arc::new(db),
                started: std::time::Instant::now(),
                build_hash: "test".into(),
                auth_token: None,
                reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            };
            let e = planner::execution(&state.store.read().unwrap(), "A").unwrap();
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                // Foreground/background work or a real draft closes the common boundary.
                observe_with(&state, "sample", "A", &e, || async {
                    Some(TurnObservation {
                        idle: false,
                        ..observation(&e.worker, 120.0)
                    })
                })
                .await
                .unwrap();
                assert_eq!(
                    planner::execution(&state.store.read().unwrap(), "A")
                        .unwrap()
                        .stage,
                    "working"
                );
                for _ in 0..2 {
                    observe_with(&state, "sample", "A", &e, || async {
                        Some(observation(&e.worker, 120.0))
                    })
                    .await
                    .unwrap();
                }
            });
            let current = planner::execution(&state.store.read().unwrap(), "A").unwrap();
            assert_eq!(current.stage, "repair");
            assert_eq!(current.attempt, 1);
            state
                .store
                .write(|c| planner::claim(c, "sample", "A").map_err(store::sql_error))
                .unwrap();
            let next = planner::execution(&state.store.read().unwrap(), "A").unwrap();
            assert_eq!(next.attempt, 2);
            assert_eq!(next.generation, e.generation + 1);
        }
    }
}

#[cfg(test)]
mod command_tests {
    #[test]
    fn project_executor_effort_uses_provider_launch_syntax() {
        assert_eq!(
            super::project_effort_flags("codex", "low").as_deref(),
            Some("-c model_reasoning_effort=low")
        );
        assert_eq!(
            super::project_effort_flags("claude", "low").as_deref(),
            Some("--effort low")
        );
        assert_eq!(super::project_effort_flags("ollama", "low"), None);
        assert_eq!(super::project_effort_flags("codex", ""), None);
    }
    #[test]
    fn project_existing_executor_env_refreshes_provider_model_and_effort() {
        use super::*;
        use serde_json::json;

        let (_dir, db, _) = super::super::outputs::tests::fixture();
        let row = bs::get_issue(&db.read().unwrap(), "A").unwrap().unwrap();
        let policy = serde_json::from_value(json!({
            "repository": "/repo",
            "worktree": true,
            "coordinator": {"provider": "codex", "model": "gpt-5.5", "effort": "low"},
            "executor": {"provider": "codex", "model": "gpt-5.5", "effort": "low"},
            "verify_command": "git diff --check",
            "enabled": true
        }))
        .unwrap();
        let project = store::Project {
            name: "sample".into(),
            revision: 1,
            policy,
        };
        let mut env = sv::EnvFile::default();
        env.set("CC_PROJECT", "sample");
        env.set("CC_BOARD_CARD", "A");
        env.set("CC_PROVIDER", "codex");
        env.set("CC_MODEL", "stale-ollama-model");
        env.set("CC_FLAGS", "--model gpt-5.5 --effort low");

        configure_executor_env(&mut env, &project, &row);

        assert_eq!(env.get("CC_PROVIDER"), Some("codex"));
        assert_eq!(
            env.get("CC_FLAGS"),
            Some("--model gpt-5.5 -c model_reasoning_effort=low")
        );
        assert_eq!(env.get("CC_MODEL"), None);
        assert_eq!(env.get("CC_WORKTREE"), Some("1"));
        assert_eq!(env.get("AMUX_BOARD_DELEGATION"), Some("0"));
    }
    #[test]
    fn project_verification_retry_failure_stays_waiting_without_model_repair() {
        use super::*;
        let (_dir, db, _) = super::super::outputs::tests::fixture();
        db.write(|c| {
            c.execute("UPDATE issues SET status='review' WHERE id='A'", [])?;
            let row = bs::get_issue(c, "A")?.unwrap();
            let mut e = planner::execution(c, "A").unwrap();
            e.stage = "reported".into();
            e.waiting = None;
            e.attempt = 1;
            e.verification_retry_pending = true;
            e.report = Some(planner::Report {
                head: "a".repeat(40),
                summary: "retained".into(),
                assets: vec![super::super::assets::Asset {
                    path: "report.md".into(),
                    sha256: "0".repeat(64),
                }],
                checks: vec![planner::Check {
                    criterion: "Output passes".into(),
                    command: "true".into(),
                }],
            });
            planner::save_execution(c, &row, &e, "project.verification_retry_granted")
                .map_err(store::sql_error)
        })
        .unwrap();
        let state = AppState {
            store: Arc::new(db),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let e = planner::execution(&state.store.read().unwrap(), "A").unwrap();
        let stage = if repair_after_failure(&e, 2, "verify", "Command timed out after 1 seconds") {
            "repair"
        } else {
            "waiting"
        };
        assert_eq!(
            stage, "waiting",
            "explicit verification retry never grants worker work even below repair cap"
        );
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(transition(
                &state,
                "A",
                &e,
                stage,
                Some("Command timed out after 1 seconds".into()),
            ))
            .unwrap();
        let c = state.store.read().unwrap();
        let after = planner::execution(&c, "A").unwrap();
        assert_eq!(after.report, e.report);
        assert_eq!(after.attempt, 1);
        assert_eq!(after.generation, e.generation);
        assert!(!after.verification_retry_pending);
        assert_eq!(
            planner::plan(&c, &store::get(&c, "sample").unwrap().unwrap())
                .unwrap()
                .into_iter()
                .find(|p| p.id == "A")
                .unwrap()
                .action,
            "wait"
        );
        assert_eq!(
            c.query_row("SELECT COUNT(*) FROM steering_queue", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn project_verification_preflights_all_commands_before_any_execution() {
        use super::*;
        let home = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        let repo = home.path().join("candidate");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let result = std::process::Command::new("git")
                .current_dir(&repo)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .args(args)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            String::from_utf8(result.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ]);
        let head = git(&["rev-parse", "HEAD"]);
        let marker = home.path().join("must-not-run");
        let first = format!("touch {}", marker.display());
        // Each scenario fixes repository and gate before claiming any execution.
        // Do not mutate live immutable policy to manufacture a persisted report.
        let setup = |name: &str, gate: &str| {
            let db = crate::db::Store::open(&home.path().join(name)).unwrap();
            let gate = gate.to_string();
            db.write(move|c| {
                let policy=serde_json::from_value(json!({"repository":"/original-checkout","coordinator":{"provider":"codex","model":"gpt-6-astra"},"executor":{"provider":"codex","model":"gpt-6-astra"},"verify_command":gate,"enabled":true})).unwrap();
                store::save(c,"sample",0,&policy,"test").map_err(store::sql_error)?;
                c.execute("INSERT INTO issues(id,title,desc,status,type,project_group,created,updated,next_action,acceptance_criteria) VALUES('A','Output','Build output','todo','code','sample',1,1,'Implement and test','[\"Output passes\"]')",[])?;
                planner::claim(c,"sample","A").map_err(store::sql_error)
            }).unwrap();
            let e = planner::execution(&db.read().unwrap(), "A").unwrap();
            let p = store::get(&db.read().unwrap(), "sample").unwrap().unwrap();
            let w = workspace::Workspace {
                repo: p.policy.repository.clone(),
                path: repo.to_string_lossy().into_owned(),
                branch: format!("amux/fanout/{}", e.worker),
                base: head.clone(),
            };
            workspace::save(home.path(), &e.worker, &w).unwrap();
            let state = AppState {
                store: Arc::new(db),
                started: std::time::Instant::now(),
                build_hash: "test".into(),
                auth_token: None,
                reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            };
            (state, p, e)
        };
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for bad_gate in [false, true] {
                let mut report = planner::Report {
                    head: head.clone(),
                    summary: "old persisted report".into(),
                    assets: vec![],
                    checks: vec![
                        planner::Check {
                            criterion: "first".into(),
                            command: first.clone(),
                        },
                        planner::Check {
                            criterion: "last".into(),
                            command: "/original-checkout/venv/bin/python check.py".into(),
                        },
                    ],
                };
                let gate = if bad_gate {
                    report.checks.pop().unwrap().command
                } else {
                    first.clone()
                };
                let (state, p, mut e) =
                    setup(if bad_gate { "bad-gate" } else { "bad-check" }, &gate);
                e.report = Some(report);
                e.stage = "reported".into();
                e.waiting = None;
                let current = e.clone();
                state
                    .store
                    .write(move |c| {
                        let row = bs::get_issue(c, "A")?.unwrap();
                        planner::save_execution(c, &row, &current, "project.execution")
                            .map_err(store::sql_error)
                    })
                    .unwrap();
                let error = verify(&state, &p, "A", &e).await.unwrap_err();
                assert!(
                    error.contains("original worker or shared checkout"),
                    "{error}"
                );
                assert!(
                    !marker.exists(),
                    "even the first valid command must not execute"
                );
                assert_eq!(
                    planner::execution(&state.store.read().unwrap(), "A")
                        .unwrap()
                        .report,
                    e.report
                );
            }
        });
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for dirty in [false, true] {
                let (state, p, mut e) =
                    setup(if dirty { "dirty-head" } else { "stale-head" }, &first);
                e.stage = "reported".into();
                e.waiting = None;
                e.verification_retry_pending = true;
                e.report = Some(planner::Report {
                    head: if dirty { head.clone() } else { "b".repeat(40) },
                    summary: "retained report".into(),
                    assets: vec![super::super::assets::Asset {
                        path: "report.md".into(),
                        sha256: "0".repeat(64),
                    }],
                    checks: vec![planner::Check {
                        criterion: "Output passes".into(),
                        command: first.clone(),
                    }],
                });
                let current = e.clone();
                state
                    .store
                    .write(move |c| {
                        let row = bs::get_issue(c, "A")?.unwrap();
                        planner::save_execution(c, &row, &current, "project.execution")
                            .map_err(store::sql_error)
                    })
                    .unwrap();
                if dirty {
                    std::fs::write(repo.join("dirty"), "uncommitted").unwrap();
                }
                let error = verify(&state, &p, "A", &e).await.unwrap_err();
                assert!(
                    error.contains(if dirty {
                        "uncommitted"
                    } else {
                        "reported head is stale"
                    }),
                    "{error}"
                );
                assert!(
                    !marker.exists(),
                    "retained report cannot bypass actual candidate identity/cleanliness"
                );
            }
        });
        // Path identity accepts aliases, never unrelated or unresolved paths.
        let alias = home.path().join("alias");
        std::os::unix::fs::symlink(&repo, &alias).unwrap();
        assert!(workspace::same_repository(
            repo.to_str().unwrap(),
            alias.to_str().unwrap()
        ));
        assert!(!workspace::same_repository(
            repo.to_str().unwrap(),
            home.path().to_str().unwrap()
        ));
        assert!(!workspace::same_repository("/missing-one", "/missing-two"));
    }

    #[test]
    fn project_retry_packet_previews_only_long_diagnostics_and_preserves_full_read() {
        use super::*;
        let (_dir, db, _) = super::super::outputs::tests::fixture();
        db.write(|c| {
            let p = store::get(c, "sample").unwrap().unwrap();
            let row = bs::get_issue(c, "A")?.unwrap();
            let mut e = planner::execution(c, "A").unwrap();
            for failure in [
                "short exact error".to_string(),
                format!("HEAD{}TAIL", "α🧪\n".repeat(9000)),
            ] {
                e.last_failure = Some(failure.clone());
                planner::save_execution(c, &row, &e, "project.execution").unwrap();
                let original = serde_json::to_value(&e).unwrap();
                let text = packet(&p, &row, &e);
                let value: serde_json::Value =
                    serde_json::from_str(text.split("Task packet:\n").nth(1).unwrap()).unwrap();
                assert_eq!(
                    serde_json::to_value(&e).unwrap(),
                    original,
                    "packet cannot mutate history"
                );
                assert_eq!(value["criteria"], json!(["Output passes"]));
                if failure.chars().count() <= 2048 {
                    assert_eq!(value["previous_result"], failure);
                } else {
                    let preview = &value["previous_result"];
                    assert_eq!(preview["truncated"], true);
                    assert_eq!(preview["original_chars"], failure.chars().count());
                    assert_eq!(preview["original_bytes"], failure.len());
                    assert!(preview["preview"].as_str().unwrap().starts_with("HEAD"));
                    assert!(preview["preview"].as_str().unwrap().ends_with("TAIL"));
                    assert!(preview["preview"].as_str().unwrap().chars().count() < 2100);
                    assert_eq!(preview["full_diagnostic"]["path"], "/api/projects/sample");
                }
                // This is the exact read model served by existing GET /api/projects/{name}.
                let board = store::board(c, "sample").unwrap();
                let card = board["cards"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["id"] == "A")
                    .unwrap();
                assert_eq!(card["execution_plan"]["execution"]["last_failure"], failure);
                assert_eq!(
                    planner::execution(c, "A").unwrap().last_failure,
                    Some(failure)
                );
            }
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .unwrap();
    }

    #[test]
    fn project_lifecycle_integrates_worktree_artifact_to_main_and_accepts_review() {
        use super::*;
        use crate::api::{
            mdai::{ModelClient, ModelCompletion},
            AppState,
        };
        use crate::project_execution::{planner, store};
        use serde_json::json;
        use sha2::Digest;

        struct FakeIntake {
            response: String,
        }
        impl ModelClient for FakeIntake {
            fn complete(&self, _: &str, _: &str) -> Result<String, String> {
                Ok(self.response.clone())
            }
            fn complete_measured(&self, _: &str, _: &str) -> Result<ModelCompletion, String> {
                Ok(ModelCompletion {
                    text: self.response.clone(),
                    usage: Some(json!({"input_tokens": 1, "output_tokens": 1})),
                })
            }
        }

        let home = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        let remote = home.path().join("origin.git");
        let repo = home.path().join("project-repo");
        let git = |cwd: &std::path::Path, args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .current_dir(cwd)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed\nstdout={}\nstderr={}",
                args,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, &["init", "--bare", "-q"]);
        git(
            home.path(),
            &["clone", remote.to_str().unwrap(), repo.to_str().unwrap()],
        );
        git(&repo, &["config", "user.name", "Project Lifecycle Test"]);
        git(
            &repo,
            &["config", "user.email", "project-lifecycle@example.invalid"],
        );
        git(&repo, &["config", "core.hooksPath", "/dev/null"]);
        std::fs::write(repo.join("README.md"), "# project lifecycle fixture\n").unwrap();
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "base"]);
        git(&repo, &["branch", "-M", "main"]);
        git(&repo, &["push", "-u", "origin", "main"]);

        let gate = "test -f docs/lifecycle-report.md && grep -q 'complex lifecycle verified' docs/lifecycle-report.md";
        let db = crate::db::Store::open(&home.path().join("amux.db")).unwrap();
        let repo_for_policy = repo.to_string_lossy().into_owned();
        let gate_for_policy = gate.to_string();
        db.write(move |c| {
            let policy: amux_core::project::ExecutionPolicy = serde_json::from_value(json!({
                "repository": repo_for_policy,
                "worktree": true,
                "coordinator": {"provider": "codex", "model": "gpt-5.5", "effort": "low"},
                "executor": {"provider": "codex", "model": "gpt-5.5", "effort": "low"},
                "max_executors": 1,
                "max_attempts": 1,
                "enabled": true,
                "verify_command": gate_for_policy,
                "verification_timeout_secs": 60,
                "acceptance": {"criteria": [
                    {"id": "artifact", "requirement": "The integrated main branch contains the lifecycle report artifact.", "verifier": {"type": "command", "id": "artifact-check", "command": gate_for_policy, "timeout_secs": 60}, "evidence": ["docs/lifecycle-report.md"]},
                    {"id": "owner", "requirement": "A human can inspect the retained report before executor retirement.", "verifier": {"type": "human", "id": "owner-review", "instructions": "Review docs/lifecycle-report.md and confirm the produced artifact is linkable and complete."}, "evidence": ["docs/lifecycle-report.md"]}
                ]}
            }))
            .unwrap();
            store::save(c, "lifecycle-e2e", 0, &policy, "test").map_err(store::sql_error)
        })
        .unwrap();
        let state = AppState {
            store: std::sync::Arc::new(db),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let plan = json!({
            "kind": "tasks",
            "reason": "one complex outcome with retained evidence",
            "confidence": 0.99,
            "tasks": [{
                "key": "artifact",
                "title": "Produce a verified lifecycle report artifact",
                "description": "Create docs/lifecycle-report.md with a human-verifiable summary and commit it.",
                "type": "doc",
                "action": "create",
                "existing_id": null,
                "next_action": "Create the committed report artifact and report its retained asset.",
                "acceptance_criteria": ["contract:artifact"],
                "needs": [],
                "dependency_reason": ""
            }]
        })
        .to_string();
        let receipt_id = std::sync::Arc::new(std::sync::Mutex::new(0_i64));
        let receipt_out = receipt_id.clone();
        state
            .store
            .write(move |c| {
                let (id, out) = crate::project_execution::intake::receive(
                    c,
                    "lifecycle-e2e",
                    "lifecycle-command",
                    "Build a complex project lifecycle artifact, verify it, retain review evidence, integrate the worktree to main, and hold for human approval.",
                )
                .map_err(store::sql_error)?;
                *receipt_out.lock().unwrap() = id;
                Ok(out)
            })
            .unwrap();
        let receipt = *receipt_id.lock().unwrap();
        rt.block_on(crate::project_execution::intake::interpret(
            &state,
            receipt,
            "lifecycle-e2e",
            std::sync::Arc::new(FakeIntake { response: plan }),
        ))
        .unwrap();
        let task_id = {
            let c = state.store.read().unwrap();
            crate::db::board_store::project_issues(&c, "lifecycle-e2e")
                .unwrap()
                .into_iter()
                .find(|r| {
                    r.acceptance_criteria
                        .as_deref()
                        .is_some_and(|v| v.contains("contract:artifact"))
                })
                .expect("intake must create the executable task")
                .id
        };
        let task_for_claim = task_id.clone();
        state
            .store
            .write(move |c| {
                planner::claim(c, "lifecycle-e2e", &task_for_claim).map_err(store::sql_error)
            })
            .unwrap();
        let mut execution = planner::execution(&state.store.read().unwrap(), &task_id).unwrap();
        rt.block_on(crate::fanout_workspace::ensure(
            home.path(),
            &execution.worker,
            repo.to_str().unwrap(),
        ))
        .unwrap();
        let workspace = crate::fanout_workspace::load(home.path(), &execution.worker).unwrap();
        let report_path = std::path::Path::new(&workspace.path).join("docs/lifecycle-report.md");
        std::fs::create_dir_all(report_path.parent().unwrap()).unwrap();
        let body = format!(
            "# Lifecycle E2E Report\n\ncomplex lifecycle verified\n\nworker: {}\ntask: {}\n",
            execution.worker, task_id
        );
        std::fs::write(&report_path, body.as_bytes()).unwrap();
        git(
            std::path::Path::new(&workspace.path),
            &["add", "docs/lifecycle-report.md"],
        );
        git(
            std::path::Path::new(&workspace.path),
            &["commit", "-m", "produce lifecycle report artifact"],
        );
        let head = git(
            std::path::Path::new(&workspace.path),
            &["rev-parse", "HEAD"],
        );
        let sha = hex::encode(sha2::Sha256::digest(body.as_bytes()));
        let report = planner::Report {
            head: head.clone(),
            summary: "Produced committed lifecycle report artifact".into(),
            checks: vec![planner::Check {
                criterion: "contract:artifact".into(),
                command: gate.into(),
            }],
            assets: vec![super::super::assets::Asset {
                path: "docs/lifecycle-report.md".into(),
                sha256: sha.clone(),
            }],
        };
        let task_for_report = task_id.clone();
        let worker_for_report = execution.worker.clone();
        let generation_for_report = execution.generation;
        let input_hash_for_report = execution.input_hash.clone();
        let report_for_record = report.clone();
        state
            .store
            .write(move |c| {
                planner::record_report(
                    c,
                    "lifecycle-e2e",
                    &task_for_report,
                    &worker_for_report,
                    generation_for_report,
                    &input_hash_for_report,
                    &report_for_record,
                )
                .map_err(store::sql_error)
            })
            .unwrap();
        execution = planner::execution(&state.store.read().unwrap(), &task_id).unwrap();
        let project = store::get(&state.store.read().unwrap(), "lifecycle-e2e")
            .unwrap()
            .unwrap();
        rt.block_on(verify(&state, &project, &task_id, &execution))
            .unwrap();
        rt.block_on(drive_project(&state, "lifecycle-e2e")).unwrap();
        let main = git(&repo, &["rev-parse", "origin/main"]);
        assert!(git(
            &repo,
            &["merge-base", "--is-ancestor", &head, "origin/main"]
        )
        .is_empty());
        assert_eq!(
            git(&repo, &["show", "origin/main:docs/lifecycle-report.md"]),
            body.trim()
        );

        let project = store::get(&state.store.read().unwrap(), "lifecycle-e2e")
            .unwrap()
            .unwrap();
        rt.block_on(crate::project_execution::acceptance::tick(&state, &project))
            .unwrap();
        let status =
            crate::project_execution::acceptance::status(&state.store.read().unwrap(), &project)
                .unwrap();
        assert_eq!(status["state"], "awaiting_human");
        assert_eq!(status["main"], main);
        assert!(status["review_assets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|asset| asset["asset"]["source"]["sha256"] == sha));
        assert!(status["criteria"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "owner" && c["result"]["state"] == "pending_human"));
        let fingerprint = status["fingerprint"].as_str().unwrap().to_string();
        let project_for_approval = project.clone();
        let fingerprint_for_approval = fingerprint.clone();
        state
            .store
            .write(move |c| {
                crate::project_execution::acceptance::approve(
                    c,
                    &project_for_approval,
                    &crate::project_execution::acceptance::Approval {
                        criterion: "owner".into(),
                        fingerprint: fingerprint_for_approval,
                        decision: "approve".into(),
                        note: "artifact reviewed in lifecycle e2e".into(),
                    },
                )
                .map_err(store::sql_error)
            })
            .unwrap();
        let accepted =
            crate::project_execution::acceptance::status(&state.store.read().unwrap(), &project)
                .unwrap();
        assert_eq!(accepted["state"], "accepted");
        let row = crate::db::board_store::get_issue(&state.store.read().unwrap(), &task_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "verified");
        let final_execution = planner::execution(&state.store.read().unwrap(), &task_id).unwrap();
        assert_eq!(final_execution.stage, "verified");
        assert_eq!(final_execution.retained_assets.len(), 1);
        assert!(std::path::Path::new(&final_execution.retained_assets[0].path).is_file());
        assert_eq!(
            crate::fanout_workspace::integration_status(home.path(), &execution.worker)["mode"],
            "worktree"
        );
    }

    #[test]
    fn project_verification_deduplicates_only_identical_bytes() {
        use super::planner::{Check, Report};
        let report = Report {
            head: "a".repeat(40),
            summary: String::new(),
            assets: vec![super::super::assets::Asset {
                path: "report.md".into(),
                sha256: "0".repeat(64),
            }],
            checks: vec![
                Check {
                    criterion: "one".into(),
                    command: "./suite".into(),
                },
                Check {
                    criterion: "two".into(),
                    command: "./suite".into(),
                },
                Check {
                    criterion: "three".into(),
                    command: "./distinct".into(),
                },
                Check {
                    criterion: "four".into(),
                    command: "./suite ".into(),
                },
            ],
        };
        assert_eq!(
            super::verification_commands("./suite", &report),
            vec!["./suite", "./distinct", "./suite "]
        );
        assert_eq!(report.checks.len(), 4);
    }
}
