//! A Project has one durable lead. Its plan is an observation, never a lease.
//! The existing independent acceptance contract remains the only success gate.
use super::{acceptance, checkout, driver, store};
use crate::{
    api::{session_verbs as sv, AppState},
    db::WriteOutcome,
    fanout_workspace as workspace,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub title: String,
    pub state: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub command_id: i64,
    pub state: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub plan: Vec<Step>,
    #[serde(default)]
    pub head: Option<String>,
}

fn valid(progress: &Progress) -> bool {
    matches!(
        progress.state.as_str(),
        "working"
            | "needs_input"
            | "needs_approval"
            | "blocked_external"
            | "ready_for_verification"
    ) && progress.note.len() <= 4000
        && progress.plan.len() <= 100
        && progress.plan.iter().all(|s| {
            !s.title.trim().is_empty()
                && s.title.len() <= 300
                && matches!(s.state.as_str(), "pending" | "working" | "done" | "skipped")
        })
}

pub fn latest(conn: &Connection, project: &str) -> anyhow::Result<Option<Value>> {
    Ok(conn.query_row(
        "SELECT data FROM session_events WHERE session=?1 AND type='project.lead_progress' ORDER BY id DESC LIMIT 1",
        [format!("project:{project}")], |r| r.get::<_, String>(0)
    ).optional()?.and_then(|raw| serde_json::from_str(&raw).ok()))
}

pub fn command_id(conn: &Connection, project: &str) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COALESCE(MAX(id),0) FROM cmd_history WHERE project_group=?1 AND session='project:'||project_group AND type='user'", [project], |r| r.get(0))
}

pub fn ready(conn: &Connection, project: &str) -> anyhow::Result<Option<(String, String)>> {
    let Some(progress) = latest(conn, project)? else {
        return Ok(None);
    };
    if progress["state"] != "ready_for_verification"
        || progress["command_id"].as_i64() != Some(command_id(conn, project)?)
        || progress["intent"] != acceptance::intent_revision(conn, project)?
    {
        return Ok(None);
    }
    let Some(head) = progress["head"].as_str() else {
        return Ok(None);
    };
    let worker = checkout::owner(&crate::config::amux_home(), project);
    Ok(Some((worker, head.to_string())))
}

fn append(
    conn: &Connection,
    project: &str,
    progress: &Value,
    source: &str,
) -> rusqlite::Result<()> {
    conn.execute("INSERT INTO session_events(ts,session,type,data,source) VALUES(?1,?2,'project.lead_progress',?3,?4)",
        params![crate::config::now_f64(), format!("project:{project}"), progress.to_string(), source])?;
    Ok(())
}

fn latest_delivery(conn: &Connection, project: &str) -> rusqlite::Result<Option<(String, f64)>> {
    conn.query_row("SELECT json_extract(data,'$.id'),ts FROM session_events WHERE session=?1 AND type='project.lead_delivery' ORDER BY id DESC LIMIT 1",
        [format!("project:{project}")], |r| Ok((r.get(0)?, r.get(1)?))).optional()
}

fn checkout_path(worker: &str) -> Option<std::path::PathBuf> {
    workspace::load(&crate::config::amux_home(), worker)
        .map(|w| std::path::PathBuf::from(w.path))
        .or_else(|| {
            let env = sv::EnvFile::load(&sv::env_path(worker));
            (env.get("CC_PROJECT_LEAD") == Some("1") && env.get("CC_WORKTREE") == Some("0"))
                .then_some(())
                .and_then(|_| env.get("CC_DIR"))
                .map(std::path::PathBuf::from)
        })
}

fn file_path(worker: &str) -> Option<std::path::PathBuf> {
    checkout_path(worker).map(|p| p.join(".amux/project-lead.json"))
}

async fn exclude_progress_from_git(worker: &str) -> Result<(), String> {
    let checkout =
        checkout_path(worker).ok_or_else(|| "project lead checkout missing".to_string())?;
    let checkout = checkout.to_string_lossy();
    let exclude = workspace::git(&checkout, &["rev-parse", "--git-path", "info/exclude"]).await?;
    let path = std::path::Path::new(&exclude);
    let old = std::fs::read_to_string(path).unwrap_or_default();
    const RULE: &str = "/.amux/project-lead.json";
    if !old.lines().any(|line| line.trim() == RULE) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        if !old.is_empty() && !old.ends_with('\n') {
            file.write_all(b"\n").map_err(|e| e.to_string())?;
        }
        file.write_all(format!("{RULE}\n").as_bytes())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn read_file(worker: &str) -> Result<Option<(Progress, String)>, String> {
    let Some(path) = file_path(worker) else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let progress: Progress =
        serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
    if !valid(&progress) {
        return Err("lead progress contains an invalid state or plan".into());
    }
    let hash = hex::encode(Sha256::digest(raw.as_bytes()));
    Ok(Some((progress, hash)))
}

fn configure(p: &store::Project, worker: &str) -> Result<(), String> {
    let path = sv::env_path(worker);
    let mut env = sv::EnvFile::load(&path);
    if path.exists()
        && (env.get("CC_PROJECT") != Some(p.name.as_str())
            || env.get("CC_PROJECT_LEAD") != Some("1"))
    {
        return Err("lead worker name collision".into());
    }
    if env.get("CC_PROJECT_PAUSED") == Some("1") {
        env.remove("CC_PROJECT_PAUSED");
        env.remove("CC_PAUSED");
    }
    env.remove("CC_BOARD_CARD");
    env.remove("CC_REVIEW_HELD");
    for (key, value) in [
        ("CC_DIR", p.policy.repository.as_str()),
        ("CC_PROJECT", p.name.as_str()),
        ("CC_PROJECT_LEAD", "1"),
        ("CC_EPHEMERAL", "1"),
        ("CC_WORKTREE", if p.policy.worktree { "1" } else { "0" }),
        ("CC_AUTO_PICKUP", "0"),
        ("CC_AUTO_CONTINUE", "0"),
        ("CC_WORKTREE_AUTO_MERGE", "0"),
        ("AMUX_BOARD_DELEGATION", "0"),
        ("CC_PROVIDER", p.policy.executor.provider.as_str()),
        ("CC_DESC", p.name.as_str()),
        ("CC_WORKTREE_VERIFY", p.policy.verify_command.as_str()),
    ] {
        env.set(key, value);
    }
    env.set("CC_TAGS", &p.name);
    let flags = driver::executor_flags(
        &p.policy.executor.provider,
        p.policy.executor.effort.as_deref(),
        p.policy.executor_full_host_access,
    );
    let flags = sv::route_model_to_env(
        &mut env,
        &p.policy.executor.provider,
        &p.policy.executor.model,
        &flags,
    );
    env.set("CC_FLAGS", &flags);
    if path.exists() {
        let old = sv::EnvFile::load(&path);
        let keys = [
            "CC_DIR",
            "CC_PROJECT",
            "CC_PROJECT_LEAD",
            "CC_BOARD_CARD",
            "CC_REVIEW_HELD",
            "CC_EPHEMERAL",
            "CC_WORKTREE",
            "CC_AUTO_PICKUP",
            "CC_AUTO_CONTINUE",
            "CC_WORKTREE_AUTO_MERGE",
            "AMUX_BOARD_DELEGATION",
            "CC_PROVIDER",
            "CC_DESC",
            "CC_WORKTREE_VERIFY",
            "CC_TAGS",
            "CC_FLAGS",
            "CC_MODEL",
            "CODEX_MODEL",
            "CC_PAUSED",
            "CC_PROJECT_PAUSED",
        ];
        if keys.iter().all(|key| old.get(key) == env.get(key)) {
            return Ok(());
        }
    }
    env.write(&path).map_err(|e| e.to_string())
}

async fn ensure_worker(state: &AppState, p: &store::Project, worker: &str) -> Result<(), String> {
    configure(p, worker)?;
    if p.policy.worktree {
        let home = crate::config::amux_home();
        let ready = workspace::load(&home, worker).is_some_and(|w| {
            checkout::belongs_to(&home, &p.name, &w) && std::path::Path::new(&w.path).is_dir()
        });
        if !ready {
            checkout::ensure(&home, &p.name, worker, &p.policy.repository).await?;
        }
    } else {
        driver::sync_shared_checkout(&p.policy.repository).await?;
    }
    exclude_progress_from_git(worker).await?;
    if !sv::is_running(worker).await {
        sv::start_for_board_dispatch(state, worker).await?;
    }
    Ok(())
}

fn packet(p: &store::Project, command_id: i64, command: &str, repair: Option<&Value>) -> String {
    format!("You are the one durable lead worker for project {name}. Pursue the requested outcome in the assigned project checkout. You may use your provider's native tools and ephemeral subagents internally; do not create AMUX workers, task leases, or another worktree. Keep one mutable plan and revise it as facts change. The board displays that plan for the owner but never controls your work. Continue safe work autonomously. Ask for input only for a real owner decision; never treat routine implementation or another worker as a dependency. Do not increase spend or send customer outbound without owner authorization. Never claim completion from your own prose: Amux runs the independent acceptance contract against the committed candidate, then requires human artifact review before publication. Commit the candidate and retain human-readable evidence. Update `.amux/project-lead.json` as work progresses using exactly {{\"command_id\":{command_id},\"state\":\"working|needs_input|needs_approval|blocked_external|ready_for_verification\",\"note\":\"current result or concrete blocker\",\"plan\":[{{\"title\":\"step\",\"state\":\"pending|working|done|skipped\"}}]}}. The progress file is Git-ignored; do not force-add it. It is a progress receipt, not proof of success. When ready, ensure the candidate is committed and the project checkout is clean; Amux will measure its actual HEAD. If checks fail, fix the candidate and update the same receipt. The latest request and acceptance contract are authoritative.\nPROJECT REQUEST: {command}\nACCEPTANCE CONTRACT: {contract}\nBASELINE CHECK: {gate}\nPRIOR VERIFICATION: {repair}", name=p.name, contract=json!(p.policy.acceptance), gate=p.policy.verify_command, repair=repair.cloned().unwrap_or(Value::Null))
}

async fn deliver(
    state: &AppState,
    p: &store::Project,
    worker: &str,
    id: String,
    text: String,
) -> anyhow::Result<()> {
    sv::steer_enqueue_idempotent_report(state, worker, &text, "project-lead", "", &id)
        .await
        .map_err(anyhow::Error::msg)?;
    let name = p.name.clone();
    let worker = worker.to_string();
    state.store.write_async(move |c| {
        c.execute("INSERT INTO session_events(ts,session,type,data,source) VALUES(?1,?2,'project.lead_delivery',?3,'project-lead')",
            params![crate::config::now_f64(),format!("project:{name}"),json!({"id":id,"worker":worker}).to_string()])?;
        Ok(WriteOutcome{applied:true,events:vec![]})
    }).await?;
    Ok(())
}

pub async fn drive(state: &AppState, p: &store::Project) -> anyhow::Result<()> {
    let worker = checkout::owner(&crate::config::amux_home(), &p.name);
    if !p.policy.enabled || p.policy.paused {
        if sv::env_path(&worker).exists() {
            let path = sv::env_path(&worker);
            let mut env = sv::EnvFile::load(&path);
            if env.get("CC_PROJECT") == Some(p.name.as_str()) && env.get("CC_PAUSED") != Some("1") {
                env.set("CC_PROJECT_PAUSED", "1");
                env.set("CC_PAUSED", "1");
                env.write(&path)?;
            }
            if sv::is_running(&worker).await {
                sv::stop_for_pause(state, &worker)
                    .await
                    .map_err(anyhow::Error::msg)?;
            }
        }
        return Ok(());
    }
    let (latest_id, pending, acceptance_state, budget_hold, saved_progress) = {
        let c = state.store.read()?;
        let id = command_id(&c, &p.name)?;
        let pending:Option<(i64,String)>=c.query_row("SELECT id,text FROM cmd_history WHERE project_group=?1 AND session='project:'||project_group AND type='user' AND capture_pending!=0 ORDER BY id LIMIT 1",[&p.name],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        (
            id,
            pending,
            acceptance::status(&c, p)?,
            super::usage::waiting(&c, p)?,
            latest(&c, &p.name)?,
        )
    };
    if latest_id == 0
        || acceptance_state["state"] == "accepted"
        || (pending.is_none() && acceptance_state["state"] == "awaiting_human")
    {
        return Ok(());
    }
    if let Some(reason) = budget_hold {
        tracing::warn!(project=%p.name,%reason,verdict="project.lead_budget_hold","lead is held at the configured budget boundary");
        return Ok(());
    }
    if pending.is_none()
        && saved_progress.as_ref().is_some_and(|v| {
            v["command_id"].as_i64() == Some(latest_id)
                && matches!(
                    v["state"].as_str(),
                    Some("needs_input" | "needs_approval" | "blocked_external")
                )
        })
    {
        return Ok(());
    }
    let failed_current_head = matches!(
        acceptance_state["state"].as_str(),
        Some("failed" | "operational_failure")
    ) && saved_progress
        .as_ref()
        .and_then(|v| v["head"].as_str())
        .is_some_and(|head| {
            acceptance_state["heads"].as_array().is_some_and(|heads| {
                heads
                    .iter()
                    .any(|pair| pair.get(1).and_then(Value::as_str) == Some(head))
            })
        });
    if pending.is_none()
        && saved_progress.as_ref().is_some_and(|v| {
            v["command_id"].as_i64() == Some(latest_id) && v["state"] == "ready_for_verification"
        })
        && !failed_current_head
    {
        return Ok(());
    }
    ensure_worker(state, p, &worker)
        .await
        .map_err(anyhow::Error::msg)?;
    if let Some((id, command)) = pending {
        if let Some(path) = file_path(&worker) {
            let _ = std::fs::remove_file(path);
        }
        let text = packet(p, id, &command, None);
        deliver(
            state,
            p,
            &worker,
            format!("project-lead-command:{}:{id}", p.name),
            text,
        )
        .await?;
        let name = p.name.clone();
        let delivered_worker = worker.clone();
        state.store.write_async(move|c| {
            c.execute("UPDATE cmd_history SET capture_pending=0,intake_result=?2 WHERE id=?1 AND project_group=?3 AND capture_pending!=0",params![id,json!({"state":"delivered","worker":delivered_worker}).to_string(),name])?;
            Ok(WriteOutcome{applied:true,events:vec![]})
        }).await?;
        return Ok(());
    }
    let signals = sv::boundary_signals(state, Some(&worker)).await;
    let idle = signals
        .as_ref()
        .and_then(|s| s.turn_boundary_status(&worker))
        .as_deref()
        == Some("idle");
    if let Some((progress, hash)) = read_file(&worker).map_err(anyhow::Error::msg)? {
        if progress.command_id == latest_id {
            let prior = {
                let c = state.store.read()?;
                latest(&c, &p.name)?
            };
            if prior.as_ref().and_then(|v| v["source_hash"].as_str()) != Some(hash.as_str()) {
                let head = if progress.state == "ready_for_verification" && idle {
                    let checkout = checkout_path(&worker)
                        .ok_or_else(|| anyhow::anyhow!("lead checkout missing"))?;
                    let checkout = checkout.to_string_lossy();
                    let actual = workspace::git(&checkout, &["rev-parse", "HEAD"])
                        .await
                        .map_err(anyhow::Error::msg)?;
                    anyhow::ensure!(
                        workspace::project_clean_status(&checkout)
                            .await
                            .map_err(anyhow::Error::msg)?
                            .is_empty(),
                        "lead checkout has uncommitted work"
                    );
                    if progress
                        .head
                        .as_deref()
                        .is_some_and(|claimed| claimed != actual)
                    {
                        tracing::warn!(project=%p.name,claimed_head=?progress.head,actual_head=%actual,
                            verdict="project.lead_head_normalized","harness bound candidate to measured checkout HEAD");
                    }
                    Some(actual)
                } else {
                    None
                };
                if progress.state != "ready_for_verification" || head.is_some() {
                    let name = p.name.clone();
                    state
                        .store
                        .write_async(move |c| {
                            let intent =
                                acceptance::intent_revision(c, &name).map_err(store::sql_error)?;
                            let mut value = serde_json::to_value(progress).map_err(|e| {
                                rusqlite::Error::ToSqlConversionFailure(Box::new(e))
                            })?;
                            value["intent"] = json!(intent);
                            value["source_hash"] = json!(hash);
                            value["head"] = json!(head);
                            append(c, &name, &value, "project-lead")?;
                            Ok(WriteOutcome {
                                applied: true,
                                events: vec![],
                            })
                        })
                        .await?;
                }
            }
        }
    }
    let (progress, delivery) = {
        let c = state.store.read()?;
        (latest(&c, &p.name)?, latest_delivery(&c, &p.name)?)
    };
    let progress = progress.filter(|v| v["command_id"].as_i64() == Some(latest_id));
    if progress.as_ref().is_some_and(|v| {
        matches!(
            v["state"].as_str(),
            Some("needs_input" | "needs_approval" | "blocked_external")
        )
    }) {
        return Ok(());
    }
    if progress
        .as_ref()
        .is_some_and(|v| v["state"] == "ready_for_verification")
    {
        if failed_current_head
            && matches!(
                acceptance_state["state"].as_str(),
                Some("failed" | "operational_failure")
            )
        {
            if let Some(path) = file_path(&worker) {
                let _ = std::fs::remove_file(path);
            }
            let id = format!(
                "project-lead-repair:{}:{}",
                p.name,
                acceptance_state["fingerprint"]
                    .as_str()
                    .unwrap_or("unknown")
            );
            let text=packet(p,latest_id,"Repair the independently verified project candidate. Keep the original project requests and contract.",Some(&acceptance_state));
            deliver(state, p, &worker, id, text).await?;
            let name = p.name.clone();
            state.store.write_async(move|c| {append(c,&name,&json!({"state":"working","command_id":latest_id,"intent":acceptance::intent_revision(c,&name).map_err(store::sql_error)?,"note":"Repairing failed whole-project acceptance","plan":[]}),"project-lead")?;Ok(WriteOutcome{applied:true,events:vec![]})}).await?;
        }
        return Ok(());
    }
    if !idle {
        return Ok(());
    }
    let Some((last_id, queued_at)) = delivery else {
        return Ok(());
    };
    let delivered_at: Option<f64> = {
        let c = state.store.read()?;
        c.query_row(
            "SELECT delivered_at FROM steering_history WHERE id=?1 AND session=?2",
            params![last_id, worker],
            |r| r.get(0),
        )
        .optional()?
    };
    let Some(delivered_at) = delivered_at else {
        return Ok(());
    };
    let ended_at = signals
        .as_ref()
        .and_then(|s| {
            s.codex_turns
                .get(&worker)
                .filter(|t| t.state == "idle")
                .map(|t| t.ts)
        })
        .or_else(|| {
            signals
                .as_ref()
                .and_then(|s| s.reports.get(&worker).and_then(|r| r["ts"].as_f64()))
        });
    if ended_at.is_none_or(|t| t <= delivered_at || t <= queued_at) {
        return Ok(());
    }
    let id = format!(
        "project-lead-continue:{}:{}",
        p.name,
        ended_at.unwrap() as i64
    );
    let text=format!("Continue project {} from the current checkout, transcript and acceptance contract. Update .amux/project-lead.json with your current plan. Do not stop for routine implementation decisions. When the committed candidate is ready, write ready_for_verification with its exact HEAD; Amux will independently check it. If you need a real owner decision, state the concrete question and category.",p.name);
    deliver(state, p, &worker, id, text).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Store;

    #[test]
    fn lead_candidate_is_current_intent_and_does_not_depend_on_board_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::open(&dir.path().join("db")).unwrap();
        db.write(|c| {
            let policy:amux_core::project::ExecutionPolicy=serde_json::from_value(json!({
                "repository":"/tmp/project-repo","mode":"lead","enabled":true,
                "coordinator":{"provider":"codex","model":"gpt-6-luna"},
                "executor":{"provider":"codex","model":"gpt-6-luna"},
                "verify_command":"test -f outcome.txt","acceptance":{"criteria":[
                    {"id":"check","requirement":"Check output","verifier":{"type":"command","id":"check","command":"test -f outcome.txt"}},
                    {"id":"review","requirement":"Review output","verifier":{"type":"human","id":"review","instructions":"Inspect output"}}
                ]}
            })).unwrap();
            store::save(c,"sample",0,&policy,"test").map_err(store::sql_error)?;
            let (id,_)=super::super::intake::receive(c,"sample","one","Produce the result").map_err(store::sql_error)?;
            let delivery:String=c.query_row("SELECT delivery FROM cmd_history WHERE id=?1",[id],|r|r.get(0))?;
            assert_eq!(delivery,"lead");
            assert!(!acceptance::settled(c,"sample").map_err(store::sql_error)?);
            c.execute("UPDATE cmd_history SET capture_pending=0 WHERE id=?1",[id])?;
            let intent=acceptance::intent_revision(c,"sample").map_err(store::sql_error)?;
            append(c,"sample",&json!({"state":"ready_for_verification","command_id":id,"intent":intent,"head":"a".repeat(40),"plan":[{"title":"Build result","state":"done"}]}),"test")?;
            assert!(acceptance::settled(c,"sample").map_err(store::sql_error)?);
            let worker=checkout::owner(&crate::config::amux_home(),"sample");
            assert!(checkout::start_permit(c,"sample",&worker).is_ok());
            assert!(checkout::start_permit(c,"sample","other-worker").is_err());
            super::super::intake::receive(c,"sample","two","Add another requirement").map_err(store::sql_error)?;
            assert!(!acceptance::settled(c,"sample").map_err(store::sql_error)?);
            Ok(WriteOutcome{applied:false,events:vec![]})
        }).unwrap();
    }

    #[test]
    fn progress_states_are_bounded_and_never_self_verify() {
        let ok = Progress {
            command_id: 1,
            state: "ready_for_verification".into(),
            note: "candidate".into(),
            plan: vec![Step {
                title: "Run checks".into(),
                state: "done".into(),
            }],
            head: Some("a".repeat(40)),
        };
        assert!(valid(&ok));
        let mut false_success = ok.clone();
        false_success.state = "verified".into();
        assert!(!valid(&false_success));
        let mut empty = ok;
        empty.plan[0].title.clear();
        assert!(!valid(&empty));
    }
}
