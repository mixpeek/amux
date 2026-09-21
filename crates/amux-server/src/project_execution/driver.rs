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
        || actual.input_hash != expected.input_hash
        || planner::input_hash(&row) != expected.input_hash
        || row.project_group.as_deref() != Some(project)
        || row.archived != 0
    {
        return Err("claim or requirements changed".into());
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
    if path.exists() {
        if env.get("CC_PROJECT") != Some(p.name.as_str())
            || env.get("CC_BOARD_CARD") != Some(row.id.as_str())
        {
            return Err("executor name collision".into());
        }
    } else {
        for (key, value) in [
            ("CC_DIR", p.policy.repository.as_str()),
            ("CC_PROJECT", p.name.as_str()),
            ("CC_BOARD_CARD", row.id.as_str()),
            ("CC_EPHEMERAL", "1"),
            ("CC_WORKTREE", "1"),
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
        let flags = if p.policy.executor.provider == "claude" {
            "--dangerously-skip-permissions"
        } else {
            ""
        };
        let flags = sv::route_model_to_env(
            &mut env,
            &p.policy.executor.provider,
            &p.policy.executor.model,
            flags,
        );
        env.set("CC_FLAGS", &flags);
        env.write(&path).map_err(|e| e.to_string())?;
    }
    workspace::ensure(&crate::config::amux_home(), &e.worker, &p.policy.repository).await?;
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
    let Some(failure) = e.last_failure.as_deref() else { return serde_json::Value::Null; };
    let chars=failure.chars().count();
    if chars<=2048 { return json!(failure); }
    tracing::info!(project=%p.name,task=%row.id,generation=e.generation,measured=true,n_considered=chars,preview_chars=2048,verdict="project.retry_diagnostic_preview","retry packet elides diagnostic middle; full failure retained in project read");
    json!({"preview":crate::api::board::chars_elide_middle(failure,1024,1024),"truncated":true,"original_chars":chars,"original_bytes":failure.len(),"full_diagnostic":{"method":"GET","path":format!("/api/projects/{}",p.name),"card_id":row.id,"field":"cards[id == card_id].execution_plan.execution.last_failure","instruction":"Read the matching card's last_failure for full exact diagnostics before inspecting omitted details."}})
}

pub fn packet(p: &store::Project, row: &bs::IssueRow, e: &Execution) -> String {
    let output_protocol=format!("For an unavailable concrete same-project output, POST /api/projects/{}/tasks/{}/required-outputs with generation, input_hash, idempotency_key, required_outputs (explicit task IDs), reason, and replaces_wait (null for a new wait; exact prior waiting string to replace an operational wait). Never turn spend/customer authorization into outputs. Stop after declaration. When outputs are Verified the harness continues the SAME attempt with a fresh generation and delivery ID. On continuation fetch the accepted local origin/main and compose required commits into your own candidate without resetting your existing work, then rerun/report every criterion; an output arriving is not verification of your task. Required output receipts below identify accepted reports and integration evidence.",p.name,row.id);
    format!("{output_protocol}\nExecute this finite project task in your isolated worktree. Own all required implementation locally. Do not create worker boards, delegate, change task status directly, send customer outbound, or increase spend. The harness controls claims, verification, main integration and retirement. Commit your changes, then report the exact HEAD and one executable candidate-relative check for EVERY acceptance criterion. The harness reruns these checks and the project gate. Report through POST /api/projects/{}/tasks/{}/report with X-Amux-Session set to your worker name. Body: {{\"generation\":{},\"input_hash\":\"{}\",\"report\":{{\"head\":\"40-character SHA\",\"summary\":\"output\",\"checks\":[{{\"criterion\":\"exact criterion\",\"command\":\"falsifiable check\"}}]}}}}. Optionally include report.assets as an array of objects with path (candidate-relative) and sha256 (lowercase hex). Markdown/JSON reports must be committed at reported HEAD; PNG/WebM may be ignored candidate-local captures. Only these passive formats are retained and linked; never use prose paths as asset declarations. Stop after reporting. If blocked, POST /api/projects/{}/tasks/{}/wait with generation, input_hash, reason and category (operational, spend, customer_outbound). Never assert success without artifacts.\nTask packet:\n{}",p.name,row.id,e.generation,e.input_hash,p.name,row.id,json!({"id":row.id,"project":p.name,"worker":e.worker,"title":row.title,"description":row.desc,"criteria":row.acceptance_criteria.as_deref().and_then(|v|serde_json::from_str::<serde_json::Value>(v).ok()),"next_action":row.next_action,"required_outputs":row.depends_on,"output_handoff":e.output_wait,"attempt":e.attempt,"max_attempts":e.attempt_limit(p.policy.max_attempts),"previous_result":previous_result(p,row,e),"verification":p.policy.verify_command}))
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
            e.stage = stage;
            e.waiting = waiting;
            e.observed_at = chrono::Utc::now().timestamp();
            planner::save_execution(c, &row, &e, "project.execution").map_err(store::sql_error)
        })
        .await?;
    Ok(())
}

fn verification_commands<'a>(gate: &'a str, report: &'a planner::Report) -> Vec<&'a str> {
    let mut seen = std::collections::HashSet::new();
    std::iter::once(gate)
        .chain(report.checks.iter().map(|c| c.command.as_str()))
        .filter(|c| seen.insert(*c))
        .collect()
}

/// One source-path policy, applied to the entire set before any shell command.
pub(crate) fn validated_verification_commands<'a>(w: &workspace::Workspace, gate: &'a str, report: &'a planner::Report) -> Result<Vec<&'a str>,String> {
    let commands=verification_commands(gate,report);
    for command in &commands { workspace::validate_verification_command(w,command)?; }
    Ok(commands)
}

async fn verify(
    state: &AppState,
    p: &store::Project,
    id: &str,
    e: &Execution,
) -> Result<(), String> {
    let report = e.report.as_ref().ok_or("no report")?;
    permit(state, &p.name, id, e)?;
    let home = crate::config::amux_home();
    let w = workspace::load(&home, &e.worker).ok_or("workspace missing")?;
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
    // Each check runs independently; a later success cannot mask an earlier failure.
    for command in &commands {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", command]).current_dir(&w.path);
        let (status, output) = workspace::checked_command(
            cmd,
            &|| permit(state, &p.name, id, e),
            std::time::Duration::from_secs(600),
        )
        .await?;
        if !status.success() {
            return Err(format!("verification failed ({command}): {output}"));
        }
    }
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
    let merged = workspace::integrate(
        &w,
        &commands
            .iter()
            .map(|c| format!("( {c}\n )"))
            .collect::<Vec<_>>()
            .join(" &&\n"),
        || permit(state, &p.name, id, e),
    )
    .await?;
    permit(state, &p.name, id, e)?;
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
        &json!({"status":"integrated","head":report.head,"merged":merged}),
    );
    let (id, expected, project) = (id.to_string(), e.clone(), p.name.clone());
    state.store.write_async(move|c| {
        let row=bs::get_issue(c,&id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        let mut current=planner::execution(c,&id).map_err(store::sql_error)?;
        let policy=store::get(c,&project).map_err(store::sql_error)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        if current.generation!=expected.generation || planner::input_hash(&row)!=expected.input_hash || policy.policy.paused || !policy.policy.enabled || !super::outputs::ready(c,&row).map_err(store::sql_error)? {return Err(rusqlite::Error::InvalidQuery);}
        super::assets::check(&retained).map_err(store::sql_error)?;
        super::assets::register(c,&id,&retained)?;
        current.retained_assets=retained;
        current.stage="verified".into();current.waiting=None;
        c.execute("UPDATE issues SET status='verified',evidence=?2,lease_owner=NULL,lease_expires_at=NULL WHERE id=?1",params![id,json!({"report":current.report,"merged":merged,"gate":policy.policy.verify_command}).to_string()])?;
        planner::save_execution(c,&row,&current,"project.verified").map_err(store::sql_error)
    }).await.map_err(|e|e.to_string())?;
    Ok(())
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
                if !fleet.is_running(&e.worker).await
                    && chrono::Utc::now().timestamp() - e.observed_at > 30
                {
                    Err("executor_stopped_before_result".into())
                } else if chrono::Utc::now().timestamp() - e.observed_at > 30
                    && !fleet.active_child_work(&e.worker)
                    && fleet.at_boundary(&e.worker).await
                    && {
                        let c = state.store.read()?;
                        planner::delivery_attempt_ended(&c, &e)?
                    }
                {
                    Err("executor_returned_without_result".into())
                } else {
                    Ok(())
                }
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
            let repair = e.attempt < e.attempt_limit(p.policy.max_attempts)
                && (plan.action == "verify"
                    || matches!(
                        error.as_str(),
                        "executor_stopped_before_result" | "executor_returned_without_result"
                    ));
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
mod command_tests {
    #[test]
    fn project_verification_preflights_all_commands_before_any_execution() {
        use super::*;
        let home=tempfile::tempdir().unwrap();
        let _home=crate::api::settings::test_env::set_home(home.path());
        let (_dir,db,_)=super::super::outputs::tests::fixture();
        let repo=home.path().join("candidate");std::fs::create_dir(&repo).unwrap();
        let git=|args:&[&str]| {
            let result=std::process::Command::new("git").current_dir(&repo)
                .env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").env_remove("GIT_INDEX_FILE")
                .args(args).output().unwrap();
            assert!(result.status.success(),"{}",String::from_utf8_lossy(&result.stderr));
            String::from_utf8(result.stdout).unwrap().trim().to_string()
        };
        git(&["init","-q"]);
        git(&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","core.hooksPath=/dev/null","commit","--allow-empty","-m","fixture"]);
        let head=git(&["rev-parse","HEAD"]);
        let marker=home.path().join("must-not-run");
        let mut e=planner::execution(&db.read().unwrap(),"A").unwrap();
        let w=workspace::Workspace{repo:"/original-checkout".into(),path:repo.to_string_lossy().into_owned(),branch:format!("amux/fanout/{}",e.worker),base:head.clone()};
        workspace::save(home.path(),&e.worker,&w).unwrap();
        let first=format!("touch {}",marker.display());
        let mut p=store::get(&db.read().unwrap(),"sample").unwrap().unwrap();
        p.policy.verify_command=first.clone();
        let state=AppState{store:Arc::new(db),started:std::time::Instant::now(),build_hash:"test".into(),auth_token:None,reconciled:Arc::new(std::sync::atomic::AtomicBool::new(true))};
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for bad_gate in [false,true] {
                let mut report=planner::Report{head:head.clone(),summary:"old persisted report".into(),assets:vec![],checks:vec![planner::Check{criterion:"first".into(),command:first.clone()},planner::Check{criterion:"last".into(),command:"/original-checkout/venv/bin/python check.py".into()}]};
                if bad_gate {p.policy.verify_command=report.checks.pop().unwrap().command;}
                e.report=Some(report);e.stage="reported".into();e.waiting=None;
                let current = e.clone();
                state.store.write(move |c| {let row=bs::get_issue(c,"A")?.unwrap();planner::save_execution(c,&row,&current,"project.execution").map_err(store::sql_error)}).unwrap();
                let error=verify(&state,&p,"A",&e).await.unwrap_err();
                assert!(error.contains("source"),"{error}");
                assert!(!marker.exists(),"even the first valid command must not execute");
                assert_eq!(planner::execution(&state.store.read().unwrap(),"A").unwrap().report,e.report);
            }
        });
        // Path identity accepts aliases, never unrelated or unresolved paths.
        let alias=home.path().join("alias");std::os::unix::fs::symlink(&repo,&alias).unwrap();
        assert!(workspace::same_repository(repo.to_str().unwrap(),alias.to_str().unwrap()));
        assert!(!workspace::same_repository(repo.to_str().unwrap(),home.path().to_str().unwrap()));
        assert!(!workspace::same_repository("/missing-one","/missing-two"));
    }

    #[test]
    fn project_retry_packet_previews_only_long_diagnostics_and_preserves_full_read() {
        use super::*;
        let (_dir, db, _) = super::super::outputs::tests::fixture();
        db.write(|c| {
            let p=store::get(c,"sample").unwrap().unwrap();
            let row=bs::get_issue(c,"A")?.unwrap();
            let mut e=planner::execution(c,"A").unwrap();
            for failure in ["short exact error".to_string(),format!("HEAD{}TAIL","α🧪\n".repeat(9000))] {
                e.last_failure=Some(failure.clone());
                planner::save_execution(c,&row,&e,"project.execution").unwrap();
                let original=serde_json::to_value(&e).unwrap();
                let text=packet(&p,&row,&e);
                let value:serde_json::Value=serde_json::from_str(text.split("Task packet:\n").nth(1).unwrap()).unwrap();
                assert_eq!(serde_json::to_value(&e).unwrap(),original,"packet cannot mutate history");
                assert_eq!(value["criteria"],json!(["Output passes"]));
                if failure.chars().count()<=2048 {
                    assert_eq!(value["previous_result"],failure);
                } else {
                    let preview=&value["previous_result"];
                    assert_eq!(preview["truncated"],true);
                    assert_eq!(preview["original_chars"],failure.chars().count());
                    assert_eq!(preview["original_bytes"],failure.len());
                    assert!(preview["preview"].as_str().unwrap().starts_with("HEAD"));
                    assert!(preview["preview"].as_str().unwrap().ends_with("TAIL"));
                    assert!(preview["preview"].as_str().unwrap().chars().count()<2100);
                    assert_eq!(preview["full_diagnostic"]["path"],"/api/projects/sample");
                }
                // This is the exact read model served by existing GET /api/projects/{name}.
                let board=store::board(c,"sample").unwrap();
                let card=board["cards"].as_array().unwrap().iter().find(|v|v["id"]=="A").unwrap();
                assert_eq!(card["execution_plan"]["execution"]["last_failure"],failure);
                assert_eq!(planner::execution(c,"A").unwrap().last_failure,Some(failure));
            }
            Ok(WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
    }
    #[test]
    fn project_verification_deduplicates_only_identical_bytes() {
        use super::planner::{Check, Report};
        let report = Report {
            head: "a".repeat(40),
            summary: String::new(),
            assets: vec![],
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
