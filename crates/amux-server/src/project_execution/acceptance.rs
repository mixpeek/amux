//! Independent project acceptance.
//!
//! The operator's contract (`ExecutionPolicy::acceptance`) is judged on one immutable project
//! candidate assembled from the CURRENT remote main plus every verified task head. The candidate
//! is not published until human approval. All tasks Verified plus a failing contract is not
//! Accepted. Receipts are append-only `session_events` (session `project:<name>`):
//!
//! * `project.acceptance_observed`  the base main, task heads and assembled candidate last seen
//! * `project.acceptance`           one immutable evaluation: per-criterion command, result, evidence
//! * `project.acceptance_approval`  operator approval or rejection of one human criterion
//! * `project.acceptance_rerun`     operator request to run the same inputs again
//!
//! Current state is DERIVED from those receipts and the current inputs. A fingerprint over the
//! contract revision, the project intent and candidate SHA identifies one evaluation, so unchanged
//! inputs never run a check twice, and any relevant change makes the old success stale while keeping
//! it inspectable. No model is called anywhere in this module.
use super::{planner, store};
use crate::api::AppState;
use crate::db::{board_store as bs, PendingEvent, WriteOutcome};
use crate::fanout_workspace as workspace;
use amux_core::{
    project::{
        phase, AcceptanceContract, ContractVerifier, ExecutionAssertion,
        ExecutionAssertionOperator, Phase,
    },
    revision::{EntityType, MutationKind},
};
use rusqlite::{params, Connection};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Operational failures (git, timeout, process) retry this many times per fingerprint, then hold
/// until the operator asks for a rerun. Semantic failures return to their owning task
/// for bounded repair; an unchanged failed candidate is never re-run in a tight loop.
const MAX_OPERATIONAL_ATTEMPTS: usize = 4;
const OBSERVE_EVERY: Duration = Duration::from_secs(30);

fn stream(project: &str) -> String {
    format!("project:{project}")
}
fn sha(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}
fn tail(text: &str, n: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    chars[chars.len().saturating_sub(n)..].iter().collect()
}

fn task_intent_hash(row: &bs::IssueRow) -> String {
    sha(&json!([
        row.id,
        row.item_type,
        row.title,
        row.desc,
        row.acceptance_criteria,
        row.depends_on
    ])
    .to_string())
}

/// What the project was asked to do: every live task's durable requirements plus the newest request
/// id. New or edited intent changes it; execution progress, leases, pause/resume state, and
/// delivery prompt bookkeeping do not. This is deliberately narrower than the planner input hash:
/// executors still use `planner::input_hash` to reject stale packets, while project acceptance
/// binds human review to the integrated artifact-producing requirements.
pub fn intent_revision(conn: &Connection, project: &str) -> anyhow::Result<String> {
    let mut rows = bs::project_issues(conn, project)?;
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    let newest: i64 = conn.query_row(
        "SELECT COALESCE(MAX(id),0) FROM cmd_history WHERE project_group=?1",
        [project],
        |r| r.get(0),
    )?;
    let parts: Vec<(String, String)> = rows
        .iter()
        .map(|r| (r.id.clone(), task_intent_hash(r)))
        .collect();
    Ok(sha(&json!([parts, newest]).to_string()))
}

pub fn fingerprint(
    project: &str,
    contract: &AcceptanceContract,
    main: &str,
    intent: &str,
) -> String {
    sha(&json!([project, contract, main, intent]).to_string())
}

fn events(
    conn: &Connection,
    project: &str,
    kind: &str,
    fp: Option<&str>,
) -> anyhow::Result<Vec<(i64, Value)>> {
    let mut q = conn.prepare(
        "SELECT id,data FROM session_events WHERE session=?1 AND type=?2 AND (?3 IS NULL OR json_extract(data,'$.fingerprint')=?3) ORDER BY id",
    )?;
    let rows = q
        .query_map(params![stream(project), kind, fp], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(id, data)| (id, serde_json::from_str(&data).unwrap_or(Value::Null)))
        .collect())
}

fn insert(
    conn: &Connection,
    project: &str,
    kind: &str,
    data: &Value,
    source: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO session_events(ts,session,type,data,source) VALUES(?1,?2,?3,?4,?5)",
        params![
            crate::config::now_f64(),
            stream(project),
            kind,
            data.to_string(),
            source
        ],
    )?;
    Ok(())
}

fn changed(project: &str, state: &str) -> WriteOutcome {
    WriteOutcome {
        applied: true,
        events: vec![PendingEvent {
            entity_type: EntityType::Other("group_config".into()),
            entity_id: project.into(),
            mutation: MutationKind::Updated,
            payload: Some(json!({"acceptance": state})),
        }],
    }
}

/// Project executors are disposable compute, but their review context is not. A project with a
/// human criterion keeps every completed executor registered, stopped and backed by its worktree
/// until the exact current main/intent/contract fingerprint is Accepted. This is the final disposal
/// authority used by `fanout_retirement`; callers cannot accidentally bypass it.
pub fn retirement_allowed(conn: &Connection, project: &str) -> anyhow::Result<Value> {
    let Some(p) = store::get(conn, project)? else {
        return Ok(
            json!({"allowed":false,"state":"project_missing","reason":"project configuration is unavailable"}),
        );
    };
    let Some(contract) = &p.policy.acceptance else {
        return Ok(
            json!({"allowed":false,"state":"review_not_configured","reason":"configure whole-project acceptance with a human artifact review before executors can expire"}),
        );
    };
    if let Err(error) = contract.validate() {
        let key = format!("invalid-contract:{}", p.name);
        if let Ok(mut seen) = observed_at().lock() {
            if seen
                .get(&key)
                .is_none_or(|at| at.elapsed() >= OBSERVE_EVERY)
            {
                tracing::warn!(project=%p.name, %error, measured=true, n_considered=contract.criteria.len(), verdict="project.acceptance_contract_invalid", "project acceptance cannot run");
                seen.insert(key, Instant::now());
            }
        }
        return Ok(
            json!({"measured":true,"n_considered":contract.criteria.len(),"state":"invalid_contract","reason":error,
            "criteria":criteria_view(contract,None,&HashMap::new()),"review_assets":review_assets(conn,&p.name,None)?}),
        );
    }
    if !contract.criteria.iter().any(|c| c.verifier.is_human()) {
        return Ok(
            json!({"allowed":false,"state":"review_not_configured","reason":"add a human criterion to the acceptance contract before executors can expire"}),
        );
    }
    let view = status(conn, &p)?;
    let allowed = view["state"] == "accepted";
    Ok(
        json!({"allowed":allowed,"state":view["state"],"reason":if allowed {"human artifact review accepted"} else {"completed executors are stopped and retained for human artifact review"},"fingerprint":view["fingerprint"]}),
    )
}

fn asset_key(asset: &Value) -> Option<String> {
    asset
        .get("path")
        .and_then(Value::as_str)
        .or_else(|| asset.pointer("/source/sha256").and_then(Value::as_str))
        .or_else(|| asset.pointer("/source/path").and_then(Value::as_str))
        .map(str::to_string)
}

fn review_assets(
    conn: &Connection,
    project: &str,
    acceptance: Option<&Value>,
) -> anyhow::Result<Vec<Value>> {
    let mut assets = Vec::new();
    let mut seen = HashSet::new();
    if let Some(acceptance) = acceptance {
        for result in acceptance["results"].as_array().into_iter().flatten() {
            let criterion = result["criterion"].as_str().unwrap_or("acceptance");
            for asset in result["evidence"].as_array().into_iter().flatten() {
                let Some(key) = asset_key(asset) else {
                    continue;
                };
                if !seen.insert(key) {
                    continue;
                }
                assets.push(json!({
                    "task": format!("acceptance:{criterion}"),
                    "title": result["criterion"].as_str().unwrap_or("Project acceptance evidence"),
                    "worker": "project-acceptance",
                    "criterion": criterion,
                    "acceptance": true,
                    "asset": asset,
                }));
            }
        }
        return Ok(assets);
    }
    for row in bs::project_issues(conn, project)? {
        if row.item_type == "epic" {
            continue;
        }
        let execution = planner::execution(conn, &row.id)?;
        for asset in execution.retained_assets {
            let asset = json!(asset);
            if let Some(key) = asset_key(&asset) {
                if !seen.insert(key) {
                    continue;
                }
            }
            assets.push(
                json!({"task":row.id,"title":row.title,"worker":execution.worker,"asset":asset}),
            );
        }
    }
    Ok(assets)
}

/// Every task is Verified or Closed, nothing is in flight, no request awaits intake, and at least
/// one task is Verified. An empty project is never settled, so it can never be accepted.
pub fn settled(conn: &Connection, project: &str) -> anyhow::Result<bool> {
    let rows = bs::project_issues(conn, project)?;
    let mut verified = false;
    for row in &rows {
        match phase(&row.status, bs::has_execution_details(row)) {
            Phase::Verified => verified = true,
            Phase::Closed => {}
            _ => return Ok(false),
        }
        if row.item_type != "epic"
            && matches!(
                planner::execution(conn, &row.id)?.stage.as_str(),
                "reserved" | "working" | "reported" | "verifying"
            )
        {
            return Ok(false);
        }
    }
    let pending: i64 = conn.query_row(
        "SELECT COUNT(*) FROM cmd_history WHERE project_group=?1 AND capture_pending!=0",
        [project],
        |r| r.get(0),
    )?;
    Ok(verified && pending == 0)
}

fn observed_main(conn: &Connection, project: &str) -> anyhow::Result<Option<Value>> {
    Ok(events(conn, project, "project.acceptance_observed", None)?
        .pop()
        .map(|(_, v)| v))
}

/// A published project is an achieved outcome, not a moving evaluation of every future commit
/// on main. The integration receipts are written only after the approved candidate is pushed and
/// each verified worker head is contained in it. Both intent and heads must still match today.
fn published_anchor(
    conn: &Connection,
    p: &store::Project,
    contract: &AcceptanceContract,
    intent: &str,
) -> anyhow::Result<Option<(Value, Value)>> {
    let heads = match verified_heads(conn, &p.name) {
        Ok(heads) => heads,
        Err(_) => return Ok(None),
    };
    let workers: HashSet<_> = planner::plan(conn, p)?
        .into_iter()
        .map(|plan| plan.execution.worker)
        .filter(|worker| !worker.trim().is_empty())
        .collect();
    if workers.is_empty() {
        return Ok(None);
    }
    let observations = events(conn, &p.name, "project.acceptance_observed", None)?;
    let lost = events(conn, &p.name, "project.publication_lost", None)?;
    for (result_id, result) in events(conn, &p.name, "project.acceptance", None)?
        .into_iter()
        .rev()
    {
        let Some(candidate) = result["candidate"]
            .as_str()
            .or_else(|| result["main"].as_str())
        else {
            continue;
        };
        if result["intent"] != intent
            || result["contract_revision"] != contract.revision
            || !matches!(
                result["state"].as_str(),
                Some("awaiting_human" | "accepted")
            )
            || result["publish_gate"]["state"] != "passed"
            || lost
                .iter()
                .any(|(id, e)| *id > result_id && e["candidate"] == candidate)
            || events(
                conn,
                &p.name,
                "project.acceptance_rerun",
                result["fingerprint"].as_str(),
            )?
            .iter()
            .any(|(id, _)| *id > result_id)
        {
            continue;
        }
        let Some((_, observed)) = observations.iter().rev().find(|(_, o)| {
            o["candidate"] == candidate
                && o["intent"] == intent
                && o["contract_revision"] == contract.revision
                && o["heads"] == json!(heads)
        }) else {
            continue;
        };
        let approvals = events(
            conn,
            &p.name,
            "project.acceptance_approval",
            result["fingerprint"].as_str(),
        )?;
        if contract
            .criteria
            .iter()
            .filter(|c| c.verifier.is_human())
            .any(|c| {
                approvals
                    .iter()
                    .rev()
                    .find(|(_, a)| a["criterion"] == c.id)
                    .is_none_or(|(id, a)| *id <= result_id || a["decision"] != "approve")
            })
        {
            continue;
        }
        let home = crate::config::amux_home();
        if !workers.iter().all(|worker| {
            let integration = workspace::integration_status(&home, worker);
            integration["status"] == "integrated"
                && integration["approved_candidate"] == true
                && integration["merged"] == candidate
        }) {
            continue;
        }
        return Ok(Some((result, observed.clone())));
    }
    Ok(None)
}

fn verified_heads(conn: &Connection, project: &str) -> anyhow::Result<Vec<(String, String)>> {
    let mut heads = Vec::new();
    for row in bs::project_issues(conn, project)? {
        if row.item_type == "epic"
            || phase(&row.status, bs::has_execution_details(&row)) == Phase::Closed
        {
            continue;
        }
        anyhow::ensure!(
            phase(&row.status, bs::has_execution_details(&row)) == Phase::Verified,
            "project task {} is not verified",
            row.id
        );
        let execution = planner::execution(conn, &row.id)?;
        let head = execution
            .report
            .as_ref()
            .map(|report| report.head.trim())
            .filter(|head| !head.is_empty())
            .ok_or_else(|| anyhow::anyhow!("verified task {} has no candidate head", row.id))?;
        heads.push((row.id, head.to_string()));
    }
    heads.sort();
    anyhow::ensure!(!heads.is_empty(), "project has no verified candidate heads");
    Ok(heads)
}

#[derive(Debug)]
struct CompositionFailure { task: String, head: String, partial: String, error: String }
impl std::fmt::Display for CompositionFailure {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result { write!(f,"task {} cannot be composed into the project candidate: {}",self.task,self.error) }
}
impl std::error::Error for CompositionFailure {}

async fn assemble_candidate(
    p: &store::Project,
    base_main: &str,
    heads: &[(String, String)],
) -> anyhow::Result<String> {
    let repo = &p.policy.repository;
    let temp = tempfile::Builder::new()
        .prefix("amux-project-candidate-")
        .tempdir()?;
    let candidate = temp.path().join("candidate").to_string_lossy().into_owned();
    workspace::git(
        repo,
        &["worktree", "add", "--detach", &candidate, base_main],
    )
    .await
    .map_err(anyhow::Error::msg)?;
    let outcome = async {
        // A repair may already include several previous task heads. Merge only
        // maximal heads so superseded divergent tips cannot conflict first.
        let mut args=vec!["merge-base","--independent"];
        args.extend(heads.iter().map(|(_,head)|head.as_str()));
        let independent=workspace::git(repo,&args).await.map_err(anyhow::Error::msg)?;
        let independent=independent.lines().collect::<HashSet<_>>();
        for (task, head) in heads {
            if !independent.contains(head.as_str()) { continue; }
            workspace::git(repo, &["cat-file", "-e", &format!("{head}^{{commit}}")])
                .await
                .map_err(|error| {
                    anyhow::anyhow!("task {task} candidate {head} is unavailable: {error}")
                })?;
            if workspace::git(&candidate, &["merge-base", "--is-ancestor", head, "HEAD"])
                .await
                .is_ok()
            {
                continue;
            }
            if let Err(error)=workspace::git(&candidate, &["merge", "--no-ff", "--no-edit", head]).await {
                let partial=workspace::git(&candidate,&["rev-parse","HEAD"]).await.map_err(anyhow::Error::msg)?;
                workspace::git(repo,&["update-ref",&format!("refs/amux/projects/{}/repair/{}",sha(&p.name),sha(task)),&partial]).await.map_err(anyhow::Error::msg)?;
                return Err(anyhow::Error::new(CompositionFailure{task:task.clone(),head:head.clone(),partial,error}));
            }
        }
        let head = workspace::git(&candidate, &["rev-parse", "HEAD"])
            .await
            .map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            workspace::project_clean_status(&candidate)
                .await
                .map_err(anyhow::Error::msg)?
                .is_empty(),
            "project candidate assembly left a dirty checkout"
        );
        workspace::git(
            repo,
            &[
                "update-ref",
                &format!("refs/amux/projects/{}/candidate", sha(&p.name)),
                &head,
            ],
        )
        .await
        .map_err(anyhow::Error::msg)?;
        Ok::<_, anyhow::Error>(head)
    }
    .await;
    let _ = workspace::git(repo, &["worktree", "remove", "--force", &candidate]).await;
    outcome
}

/// The fingerprint of the inputs as they stand now, when the assembled candidate SHA is known.
pub fn current_fingerprint(
    conn: &Connection,
    p: &store::Project,
) -> anyhow::Result<Option<String>> {
    let Some(contract) = &p.policy.acceptance else {
        return Ok(None);
    };
    let Some(candidate) = observed_main(conn, &p.name)?.and_then(|o| {
        o["candidate"]
            .as_str()
            .or_else(|| o["main"].as_str())
            .map(String::from)
    }) else {
        return Ok(None);
    };
    Ok(Some(fingerprint(
        &p.name,
        contract,
        &candidate,
        &intent_revision(conn, &p.name)?,
    )))
}

fn criteria_view(
    contract: &AcceptanceContract,
    result: Option<&Value>,
    approvals: &HashMap<String, Value>,
) -> Vec<Value> {
    contract
        .criteria
        .iter()
        .map(|c| {
            let recorded = result.and_then(|r| r["results"].as_array()).and_then(|rs| rs.iter().find(|x| x["criterion"] == c.id));
            let mut outcome = recorded.cloned().unwrap_or(Value::Null);
            if let (Some(approval), true) = (approvals.get(&c.id), c.verifier.is_human()) {
                outcome["state"] = json!(if approval["decision"] == "approve" { "approved" } else { "rejected" });
                outcome["approval"] = approval.clone();
            }
            let verifier = match &c.verifier {
                ContractVerifier::Command { id, command, timeout_secs } => json!({"type":"command","id":id,"command":command,"timeout_secs":timeout_secs}),
                ContractVerifier::Execution { id, command, timeout_secs, receipt, required_stages, assertions } => json!({"type":"execution","id":id,"command":command,"timeout_secs":timeout_secs,"receipt":receipt,"required_stages":required_stages,"assertions":assertions}),
                ContractVerifier::Human { id, instructions } => json!({"type":"human","id":id,"instructions":instructions}),
            };
            json!({"id":c.id,"requirement":c.requirement,"verifier":verifier,"evidence_required":c.evidence,"result":outcome})
        })
        .collect()
}

/// Worker steering can arrive while a project executor is stopping. Keep such
/// input visible at human review rather than letting an invisible queue block
/// retirement after the project is approved.
fn pending_owner_messages(conn: &Connection, project: &str) -> anyhow::Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT q.id,q.session,q.precond_card,q.text,q.queued_at FROM steering_queue q \
         JOIN issues i ON i.id=q.precond_card AND i.session=q.session \
         WHERE i.project_group=?1 AND COALESCE(i.deleted,0)=0 AND q.guard='project-steering' \
         ORDER BY q.queued_at,q.id",
    )?;
    let rows = stmt.query_map([project], |r| {
        Ok(json!({"id":r.get::<_,String>(0)?,"worker":r.get::<_,String>(1)?,
            "task":r.get::<_,String>(2)?,"text":crate::api::session_verbs::redact_secrets(&r.get::<_,String>(3)?),
            "queued_at":r.get::<_,f64>(4)?,"delivered":false}))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Approval covers only owner messages already queued before the review. They
/// remain in the worker's history as explicitly undelivered; messages arriving
/// after approval still block retirement and must be handled separately.
pub(crate) fn settle_approved_owner_messages(
    conn: &Connection,
    project: &str,
    worker: &str,
    fingerprint: &str,
) -> anyhow::Result<WriteOutcome> {
    let reviewed_at: f64 = conn.query_row(
        "SELECT COALESCE(MAX(ts),0) FROM session_events WHERE session=?1 AND type='project.acceptance_approval' AND json_extract(data,'$.fingerprint')=?2 AND json_extract(data,'$.decision')='approve'",
        params![stream(project), fingerprint],
        |r| r.get(0),
    )?;
    if reviewed_at <= 0.0 {
        return Ok(WriteOutcome {
            applied: false,
            events: vec![],
        });
    }
    let mut stmt = conn.prepare(
        "SELECT q.id,q.precond_card FROM steering_queue q JOIN issues i ON i.id=q.precond_card AND i.session=q.session \
         WHERE q.session=?1 AND q.guard='project-steering' AND q.delivering_since IS NULL \
         AND q.queued_at<=?2 AND i.project_group=?3 AND i.status='verified' AND COALESCE(i.deleted,0)=0",
    )?;
    let rows = stmt.query_map(params![worker, reviewed_at, project], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let candidates = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut settled = 0;
    for (id, task) in candidates {
        let now = crate::config::now_f64();
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO steering_history(id,session,text,queued_at,delivered_at,outcome,guard,sender) \
             SELECT id,session,text,queued_at,?3,'void:project-review-approved',guard,sender FROM steering_queue \
             WHERE id=?1 AND session=?2 AND guard='project-steering' AND delivering_since IS NULL",
            params![id, worker, now],
        )?;
        if inserted == 0 {
            continue;
        }
        conn.execute(
            "DELETE FROM steering_queue WHERE id=?1 AND session=?2 AND guard='project-steering' AND delivering_since IS NULL",
            params![id, worker],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO session_events(ts,session,type,data,idem,source) VALUES(?1,?2,'message.voided',?3,?4,'steering')",
            params![now, worker, json!({"id":id,"task":task,"project":project,
                "fingerprint":fingerprint,"reason":"project-review-approved","delivered":false,
                "measured":true,"n_considered":1}).to_string(), format!("void:{id}")],
        )?;
        settled += 1;
    }
    tracing::info!(
        project,
        worker,
        settled,
        measured = true,
        n_considered = settled,
        verdict = "project_review_owner_messages_retained",
        "reviewed undelivered owner messages retained in worker history before retirement"
    );
    Ok(WriteOutcome {
        applied: settled > 0,
        events: vec![],
    })
}

/// The project's acceptance as one canonical projection, consumed unchanged by every surface.
pub fn status(conn: &Connection, p: &store::Project) -> anyhow::Result<Value> {
    let Some(contract) = &p.policy.acceptance else {
        return Ok(
            json!({"measured":true,"n_considered":0,"state":"not_configured",
            "reason":"No acceptance contract is configured. Task Verified is per task and is not project acceptance.",
            "review_assets":review_assets(conn,&p.name,None)?}),
        );
    };
    let intent = intent_revision(conn, &p.name)?;
    let is_settled = settled(conn, &p.name)?;
    let anchor = if is_settled {
        published_anchor(conn, p, contract, &intent)?
    } else {
        None
    };
    let observed = if let Some((_, observation)) = &anchor {
        Some(observation.clone())
    } else {
        observed_main(conn, &p.name)?
    };
    let candidate = observed.as_ref().and_then(|o| {
        o["candidate"]
            .as_str()
            .or_else(|| o["main"].as_str())
            .map(String::from)
    });
    let base_main = anchor
        .as_ref()
        .and_then(|(result, _)| result["base_main"].as_str().map(String::from))
        .or_else(|| {
            observed
                .as_ref()
                .and_then(|o| o["base_main"].as_str().map(String::from))
        });
    let heads = observed
        .as_ref()
        .and_then(|o| o["heads"].as_array().cloned())
        .unwrap_or_default();
    let mut view = json!({"measured":true,"n_considered":contract.criteria.len(),"contract_revision":contract.revision,"intent":intent,"main":base_main,"candidate":candidate,"task_heads":heads,
        "pending_owner_messages":pending_owner_messages(conn,&p.name)?,
        "review_assets":review_assets(conn,&p.name,None)?,
        "criteria":criteria_view(contract, None, &HashMap::new())});
    if let Some((result, _)) = &anchor {
        view["published"] = result["candidate"]
            .as_str()
            .or_else(|| result["main"].as_str())
            .map_or(Value::Null, |sha| json!(sha));
    }
    let pending = |view: &mut Value, reason: &str| {
        view["state"] = json!("pending");
        view["reason"] = json!(reason);
    };
    let Some(candidate) = candidate else {
        pending(
            &mut view,
            if is_settled {
                "waiting_for_main_observation"
            } else {
                "waiting_for_tasks"
            },
        );
        return Ok(view);
    };
    let fp = fingerprint(&p.name, contract, &candidate, &intent);
    view["fingerprint"] = json!(fp);
    let rerun = events(conn, &p.name, "project.acceptance_rerun", Some(&fp))?
        .last()
        .map_or(0, |(id, _)| *id);
    let live: Vec<_> = events(conn, &p.name, "project.acceptance", Some(&fp))?
        .into_iter()
        .filter(|(id, _)| *id > rerun)
        .collect();
    view["operational_attempts"] = json!(live
        .iter()
        .filter(|(_, e)| retryable_operational_result(e))
        .count());
    // The last evaluation for OTHER inputs stays inspectable as history; it is never the current answer.
    if let Some((_, previous)) = events(conn, &p.name, "project.acceptance", None)?
        .into_iter()
        .rev()
        .find(|(_, e)| e["fingerprint"] != json!(fp))
    {
        view["previous"] = json!({"state":previous["state"],"main":previous["main"],"contract_revision":previous["contract_revision"],"finished":previous["finished"]});
    }
    if !is_settled {
        if let Some((_, prior)) = live.last() {
            view["invalidated"] = json!({"state":prior["state"],"main":prior["main"],"finished":prior["finished"],"reason":"project_work_reopened"});
        }
        pending(&mut view, "waiting_for_tasks");
        return Ok(view);
    }
    let Some((_, result)) = live.last() else {
        let reason = if rerun > 0 {
            "rerun_requested"
        } else {
            "not_yet_evaluated"
        };
        pending(&mut view, reason);
        return Ok(view);
    };
    let mut approvals = HashMap::new();
    for (_, a) in events(conn, &p.name, "project.acceptance_approval", Some(&fp))? {
        approvals.insert(a["criterion"].as_str().unwrap_or_default().to_string(), a);
    }
    view["criteria"] = json!(criteria_view(contract, Some(result), &approvals));
    view["review_assets"] = json!(review_assets(conn, &p.name, Some(result))?);
    view["evaluated_candidate"] = result["main"].clone();
    view["publish_gate"] = result["publish_gate"].clone();
    view["finished"] = result["finished"].clone();
    if matches!(
        result["state"].as_str(),
        Some("awaiting_human" | "accepted")
    ) && result["publish_gate"]["state"] != "passed"
    {
        pending(&mut view, "repository_publication_gate_pending");
        return Ok(view);
    }
    let humans: Vec<&str> = contract
        .criteria
        .iter()
        .filter(|c| c.verifier.is_human())
        .map(|c| c.id.as_str())
        .collect();
    let state = match result["state"].as_str().unwrap_or("failed") {
        "awaiting_human" => {
            let decision = |id: &&str| {
                approvals
                    .get(*id)
                    .and_then(|a| a["decision"].as_str().map(String::from))
            };
            if humans
                .iter()
                .any(|id| decision(id).as_deref() == Some("reject"))
            {
                "failed"
            } else if humans
                .iter()
                .all(|id| decision(id).as_deref() == Some("approve"))
            {
                "accepted"
            } else {
                "awaiting_human"
            }
        }
        "accepted" => "accepted",
        "operational_failure" => "operational_failure",
        "failed" if retryable_operational_result(result) => "operational_failure",
        _ => "failed",
    };
    view["state"] = json!(state);
    Ok(view)
}

fn runnable(conn: &Connection, project: &str, fp: &str) -> anyhow::Result<bool> {
    let rerun = events(conn, project, "project.acceptance_rerun", Some(fp))?
        .last()
        .map_or(0, |(id, _)| *id);
    let live: Vec<_> = events(conn, project, "project.acceptance", Some(fp))?
        .into_iter()
        .filter(|(id, _)| *id > rerun)
        .collect();
    // Older receipts may have passed the outcome checks without running the repository's
    // publication gate. Re-evaluate them before they can be published; this costs no model turn.
    if live.last().is_some_and(|(_, e)| {
        matches!(e["state"].as_str(), Some("awaiting_human" | "accepted"))
            && e["publish_gate"]["state"] != "passed"
    }) {
        return Ok(true);
    }
    if live.iter().any(|(_, e)| !retryable_operational_result(e)) {
        return Ok(false);
    }
    Ok(live.len() < MAX_OPERATIONAL_ATTEMPTS)
}

/// A pre-push gate checks the bytes that would be published. The signature contains both sides
/// of every changed path and the hook tree, so a later main commit touching unrelated files can
/// reuse the gate, while a change to the proposed patch or hook forces a fresh run.
async fn publish_gate_signature(repo: &str, base: &str, candidate: &str) -> anyhow::Result<String> {
    let raw = workspace::git(repo, &["diff", "--raw", "--no-renames", base, candidate])
        .await
        .map_err(anyhow::Error::msg)?;
    let hooks = workspace::git(repo, &["rev-parse", &format!("{candidate}:.githooks")])
        .await
        .unwrap_or_else(|_| "no-committed-hooks".into());
    Ok(sha(&json!([repo, raw, hooks]).to_string()))
}

async fn ensure_publish_gate(
    state: &AppState,
    p: &store::Project,
    base: &str,
    candidate: &str,
) -> anyhow::Result<Value> {
    let signature = publish_gate_signature(&p.policy.repository, base, candidate).await?;
    let prior = {
        let conn = state.store.read()?;
        events(&conn, &p.name, "project.publish_gate", None)?
            .into_iter()
            .rev()
            .map(|(_, event)| event)
            .find(|event| event["signature"] == signature && event["state"] == "passed")
    };
    if let Some(mut prior) = prior {
        prior["reused"] = json!(true);
        return Ok(prior);
    }
    // Dry-run executes the real pre-push hook without publishing. A unique throwaway ref avoids
    // racing main during the lengthy hook; no remote branch is created by --dry-run. The actual
    // main push below may omit the already-attested hook only for this identical patch signature.
    let preflight_ref = format!("{candidate}:refs/heads/amux-preflight/{candidate}");
    workspace::git(
        &p.policy.repository,
        &["push", "--dry-run", "origin", &preflight_ref],
    )
    .await
    .map_err(anyhow::Error::msg)?;
    let receipt = json!({"state":"passed","signature":signature,"candidate":candidate,"base_main":base,"reused":false});
    let project = p.name.clone();
    let stored = receipt.clone();
    state
        .store
        .write_async(move |c| {
            insert(c, &project, "project.publish_gate", &stored, "harness")?;
            Ok(changed(&project, "publish_gate"))
        })
        .await?;
    tracing::info!(project=%p.name,candidate,signature,measured=true,n_considered=1,verdict="project.publish_gate_passed","repository pre-push gate passed before human review");
    Ok(receipt)
}

/// Record the exact unpublished candidate assembled from a base main plus verified task heads.
/// Idempotent while all three identities are unchanged.
pub fn observe(
    conn: &Connection,
    p: &store::Project,
    base_main: &str,
    candidate: &str,
    heads: &[(String, String)],
) -> anyhow::Result<WriteOutcome> {
    let Some(contract) = &p.policy.acceptance else {
        return Ok(WriteOutcome {
            applied: false,
            events: vec![],
        });
    };
    let intent = intent_revision(conn, &p.name)?;
    if let Some(last) = observed_main(conn, &p.name)? {
        if last["candidate"] == candidate
            && last["base_main"] == base_main
            && last["heads"] == json!(heads)
            && last["intent"] == json!(intent)
            && last["contract_revision"] == json!(contract.revision)
        {
            return Ok(WriteOutcome {
                applied: false,
                events: vec![],
            });
        }
    }
    insert(
        conn,
        &p.name,
        "project.acceptance_observed",
        &json!({"main":candidate,"candidate":candidate,"base_main":base_main,"heads":heads,"intent":intent,"contract_revision":contract.revision}),
        "harness",
    )?;
    tracing::info!(project = %p.name, base_main, candidate, task_heads=heads.len(), measured = true, n_considered = heads.len(), verdict = "project.acceptance_observed", "unpublished project candidate assembled for acceptance");
    Ok(changed(&p.name, "observed"))
}

/// Reopen the existing owner of failed integrated behavior. Keep every earlier
/// report/evaluation and use the normal claim, concurrency, budget and delivery
/// path. This is implementation work, never approval of a failed artifact.
fn repair_owner(conn: &Connection, p: &store::Project, task: &str, expected_head: &str, context: &Value) -> anyhow::Result<WriteOutcome> {
    let no_change=||WriteOutcome{applied:false,events:vec![]};
    if !p.policy.enabled || p.policy.paused || super::usage::waiting(conn,p)?.is_some() { return Ok(no_change()); }
    let Some(row)=bs::get_issue(conn,task)? else { return Ok(no_change()); };
    let mut e=planner::execution(conn,task)?;
    if row.project_group.as_deref()!=Some(&p.name) || row.archived!=0 || row.status!="verified" || e.stage!="verified" || e.suspended
        || super::outputs::authorization_hold(conn,&row)? || e.input_hash!=planner::input_hash(&row)
        || e.report.as_ref().is_none_or(|r|r.head!=expected_head) { return Ok(no_change()); }
    let revision=p.policy.acceptance.as_ref().map_or(0,|c|c.revision);
    let prefix=format!("acceptance-repair:{revision}:{}:",&e.input_hash[..12.min(e.input_hash.len())]);
    let attempts=e.retry_grants.iter().filter(|g|g.request.idempotency_key.starts_with(&prefix)).count();
    if attempts>=2 {
        tracing::warn!(project=%p.name,task,measured=true,n_considered=attempts,verdict="project.acceptance_repair_exhausted","integrated repair limit reached; failed evidence remains visible");
        return Ok(no_change());
    }
    let key=format!("{prefix}{}",e.generation);
    let request=super::task_retry::Request{idempotency_key:key,expect_generation:e.generation,expect_revision:row.rev,input_hash:e.input_hash.clone()};
    e.retry_grants.push(super::task_retry::Grant{request,allowed_through:e.attempt.saturating_add(1),previous_result:json!({"report":e.report,"retained_assets":e.retained_assets,"acceptance_failure":context})});
    e.stage="repair".into();e.wait_category=None;e.output_wait=None;
    e.waiting=Some(format!("Independent whole-project verification requires repair in this task's existing checkout. Compose the supplied local candidate commit if present, preserve other tasks' changes, repair the measured failure, and submit a new committed report. Do not approve artifacts or weaken the acceptance contract. Failure context: {}",context));
    let out=planner::save_execution(conn,&row,&e,"project.acceptance_repair_scheduled")?;
    // An epic cannot keep claiming all children verified after a repair opens.
    while conn.execute("UPDATE issues SET status='backlog',evidence=NULL,rev=rev+1,version=version+1 WHERE project_group=?1 AND type='epic' AND status='verified' AND EXISTS(SELECT 1 FROM json_each(issues.depends_on) d JOIN issues child ON child.id=d.value WHERE child.status!='verified')",[&p.name])?>0 {}
    insert(conn,&p.name,"project.acceptance_repair",&json!({"task":task,"generation":e.generation,"context":context,"previous_head":expected_head}),"harness")?;
    Ok(out)
}

fn repair_failed_criteria(conn: &Connection, p: &store::Project, result: &Value) -> anyhow::Result<WriteOutcome> {
    let mut out=WriteOutcome{applied:false,events:vec![]};
    if result["state"]!="failed" { return Ok(out); }
    let Some(contract)=p.policy.acceptance.as_ref() else { return Ok(out); };
    let failures=result["results"].as_array().into_iter().flatten().filter(|r|r["state"]=="failed" && r["criterion"].as_str().and_then(|id|contract.criterion(id)).is_some_and(|c|!c.verifier.is_human())).collect::<Vec<_>>();
    for row in bs::project_issues(conn,&p.name)? {
        let criteria:Vec<String>=serde_json::from_str(row.acceptance_criteria.as_deref().unwrap_or("[]"))?;
        let owned=failures.iter().filter(|r|r["criterion"].as_str().is_some_and(|id|criteria.contains(&format!("contract:{id}")))).collect::<Vec<_>>();
        if owned.is_empty() { continue; }
        let e=planner::execution(conn,&row.id)?;
        let Some(report)=e.report else { continue; };
        let failures=owned.iter().map(|r|json!({"criterion":r["criterion"],"command":r["command"],"state":r["state"],"exit":r["exit"],"output":tail(r["output"].as_str().unwrap_or(""),4000),"evidence_error":r["evidence_error"],"evidence":r["evidence"]})).collect::<Vec<_>>();
        let context=json!({"fingerprint":result["fingerprint"],"candidate":result["candidate"],"failures":failures});
        let changed=repair_owner(conn,p,&row.id,&report.head,&context)?;
        out.applied|=changed.applied;out.events.extend(changed.events);
    }
    Ok(out)
}

/// Compare-and-write: a result is recorded only if the contract and the intent it was computed for
/// are still current. A stale result is discarded and logged; the newer inputs are evaluated instead.
pub fn record(
    conn: &Connection,
    project: &str,
    contract: &AcceptanceContract,
    intent: &str,
    result: &Value,
) -> anyhow::Result<WriteOutcome> {
    let current = store::get(conn, project)?;
    let result_main = result["main"].as_str().unwrap_or_default();
    let still = current.as_ref().is_some_and(|c| {
        c.policy.acceptance.as_ref() == Some(contract) && !c.policy.paused && c.policy.enabled
    }) && intent_revision(conn, project)? == intent
        && settled(conn, project)?
        && observed_main(conn, project)?
            .and_then(|o| o["main"].as_str().map(String::from))
            .as_deref()
            == Some(result_main);
    if !still {
        tracing::warn!(project, fingerprint = %result["fingerprint"], measured = true, n_considered = 1, verdict = "project.acceptance_stale_discarded",
            "acceptance result discarded: the contract, intent or run state changed while it was computed");
        return Ok(WriteOutcome {
            applied: false,
            events: vec![],
        });
    }
    insert(conn, project, "project.acceptance", result, "harness")?;
    let state = result["state"].as_str().unwrap_or("failed");
    let failing = result["results"].as_array().map_or(0, |r| {
        r.iter()
            .filter(|x| matches!(x["state"].as_str(), Some("failed" | "operational")))
            .count()
    });
    tracing::info!(project, state, failing, main = %result["main"], contract_revision = %result["contract_revision"], measured = true,
        n_considered = result["results"].as_array().map_or(0, Vec::len), verdict = "project.acceptance_recorded", "project acceptance recorded");
    let mut out=changed(project,state);
    if let Some(current)=current.as_ref() {
        let repairs=repair_failed_criteria(conn,current,result)?;
        out.events.extend(repairs.events);
    }
    Ok(out)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub criterion: String,
    /// The fingerprint the operator reviewed. It binds the decision to the exact contract, intent and candidate SHA.
    pub fingerprint: String,
    pub decision: String,
    #[serde(default)]
    pub note: String,
}

/// Operator approval of ONE configured human criterion, for exactly the inputs that were reviewed.
pub fn approve(
    conn: &Connection,
    p: &store::Project,
    body: &Approval,
) -> anyhow::Result<WriteOutcome> {
    let refuse = |why: &str| {
        tracing::warn!(project = %p.name, criterion = %body.criterion, why, measured = true, n_considered = 1, verdict = "project.acceptance_approval_refused", "human approval refused");
        anyhow::anyhow!("{why}")
    };
    let contract = p
        .policy
        .acceptance
        .as_ref()
        .ok_or_else(|| refuse("no acceptance contract is configured"))?;
    let criterion = contract
        .criterion(&body.criterion)
        .ok_or_else(|| refuse("unknown criterion"))?;
    if !criterion.verifier.is_human() {
        return Err(refuse(
            "only a criterion configured for human review can be approved by a person",
        ));
    }
    if !matches!(body.decision.as_str(), "approve" | "reject") || body.note.len() > 1000 {
        return Err(refuse(
            "decision must be approve or reject with a note of at most 1000 characters",
        ));
    }
    let view = status(conn, p)?;
    if view["fingerprint"] != json!(body.fingerprint) {
        return Err(refuse(
            "the reviewed inputs are no longer current; reload and review the current evidence",
        ));
    }
    let prior = events(
        conn,
        &p.name,
        "project.acceptance_approval",
        Some(&body.fingerprint),
    )?
    .into_iter()
    .rev()
    .find(|(_, a)| a["criterion"] == json!(body.criterion));
    if let Some((_, prior)) = prior {
        if prior["decision"] == json!(body.decision) {
            return Ok(WriteOutcome {
                applied: false,
                events: vec![],
            });
        }
        return Err(refuse(
            "a decision is already recorded for these inputs; new inputs reopen review",
        ));
    }
    if view["state"] != "awaiting_human" {
        return Err(refuse(
            "human review opens only after every automated criterion passed for the current inputs",
        ));
    }
    insert(
        conn,
        &p.name,
        "project.acceptance_approval",
        &json!({"criterion":body.criterion,"fingerprint":body.fingerprint,
        "contract_revision":contract.revision,"main":view["main"],"decision":body.decision,"note":body.note,"verifier":criterion.verifier.id(),"actor":"operator"}),
        "operator",
    )?;
    tracing::info!(project = %p.name, criterion = %body.criterion, decision = %body.decision, measured = true, n_considered = 1, verdict = "project.acceptance_approval_recorded", "human criterion decided");
    let after = status(conn, p)?;
    Ok(changed(
        &p.name,
        after["state"].as_str().unwrap_or("failed"),
    ))
}

/// Operator request to evaluate the same inputs again (a flaky or repaired environment).
pub fn request_rerun(
    conn: &Connection,
    p: &store::Project,
    fingerprint: &str,
) -> anyhow::Result<WriteOutcome> {
    let view = status(conn, p)?;
    anyhow::ensure!(
        view["fingerprint"] == json!(fingerprint),
        "inputs changed; the current inputs are evaluated automatically"
    );
    anyhow::ensure!(
        matches!(
            view["state"].as_str(),
            Some("failed" | "operational_failure")
        ),
        "only a failed or operationally failed evaluation can be rerun"
    );
    insert(
        conn,
        &p.name,
        "project.acceptance_rerun",
        &json!({"fingerprint":fingerprint}),
        "operator",
    )?;
    tracing::info!(project = %p.name, fingerprint, measured = true, n_considered = 1, verdict = "project.acceptance_rerun_requested", "operator requested another evaluation of the same inputs");
    Ok(changed(&p.name, "rerun_requested"))
}

/// A task binds itself to approved verifiers with `contract:<id>` acceptance criteria. Its report may
/// only carry the approved command and required evidence for each, so an executor cannot swap a
/// check for `true`, invent or repeat a criterion, satisfy a human review, or silently retain the
/// wrong artifact.
pub fn contract_binding(
    criteria: &[String],
    report: &planner::Report,
    contract: Option<&AcceptanceContract>,
) -> anyhow::Result<()> {
    let refs: Vec<&str> = criteria
        .iter()
        .filter_map(|c| c.strip_prefix("contract:"))
        .collect();
    if refs.is_empty() {
        return Ok(());
    }
    let contract = contract.ok_or_else(|| {
        anyhow::anyhow!(
            "task references contract criteria but the project has no acceptance contract"
        )
    })?;
    let mut seen = std::collections::HashSet::new();
    for id in refs {
        anyhow::ensure!(seen.insert(id), "contract:{id} is referenced twice");
        let criterion = contract
            .criterion(id)
            .ok_or_else(|| anyhow::anyhow!("contract:{id} is not an approved criterion"))?;
        let command = match &criterion.verifier {
            ContractVerifier::Command { command, .. }
            | ContractVerifier::Execution { command, .. } => command,
            ContractVerifier::Human { .. } => anyhow::bail!("contract:{id} is a human criterion; it is approved at project acceptance, never by a task report"),
        };
        let checks: Vec<_> = report
            .checks
            .iter()
            .filter(|c| c.criterion == format!("contract:{id}"))
            .collect();
        anyhow::ensure!(
            checks.len() == 1 && checks[0].command.trim() == command.trim(),
            "the check for contract:{id} must be exactly the approved verifier command"
        );
        let assets: std::collections::HashSet<&str> = report
            .assets
            .iter()
            .map(|asset| asset.path.as_str())
            .collect();
        // Execution evidence is produced by the independent, fresh project
        // acceptance run. Requiring it in the worker's task report forces the
        // worker to run privileged infrastructure (or submit stale proof).
        let task_evidence: &[String] =
            if matches!(criterion.verifier, ContractVerifier::Execution { .. }) {
                &[]
            } else {
                &criterion.evidence
            };
        for evidence in task_evidence {
            anyhow::ensure!(
                assets.contains(evidence.as_str()),
                "contract:{id} requires reported asset {evidence}"
            );
        }
    }
    Ok(())
}

/// Intake plans may reference approved command criteria only; the planner cannot invent verifiers.
pub fn check_plan_refs(contract: &AcceptanceContract, tasks: &[Vec<String>]) -> Result<(), String> {
    for criteria in tasks {
        let mut seen = std::collections::HashSet::new();
        for id in criteria.iter().filter_map(|c| c.strip_prefix("contract:")) {
            if !seen.insert(id) {
                return Err(format!("contract:{id} is referenced twice by one task"));
            }
            match contract.criterion(id).map(|c| &c.verifier) {
                None => return Err(format!("contract:{id} is not an approved criterion")),
                Some(v) if v.is_human() => {
                    return Err(format!(
                        "contract:{id} is a human criterion; do not attach it to a task"
                    ))
                }
                Some(_) => {}
            }
        }
    }
    Ok(())
}

/// The bounded catalogue appended to the planning prompt. It is part of the one intake call; it
/// never causes another.
pub fn catalogue(contract: &AcceptanceContract) -> String {
    let lines: Vec<String> = contract
        .criteria
        .iter()
        .filter(|c| !c.verifier.is_human())
        .map(|c| {
            let evidence = if c.evidence.is_empty() {
                String::new()
            } else {
                format!("; required evidence assets: {}", c.evidence.join(", "))
            };
            let proof = match &c.verifier {
                ContractVerifier::Execution { receipt, required_stages, assertions, .. } => format!(
                    "; runtime proof: a fresh candidate-bound receipt at {receipt} with passed stages {} and {} harness-checked raw evidence assertions",
                    required_stages.join(", "), assertions.len()
                ),
                _ => String::new(),
            };
            format!("contract:{} = {}{}{}", c.id, c.requirement, proof, evidence)
        })
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    format!("\nApproved project verifiers. When a task must satisfy one, add its exact `contract:<id>` string as one of that task's acceptance_criteria; never invent ids or write your own command for them:\n{}", lines.join("\n"))
}

async fn retain_evidence(
    home: &std::path::Path,
    candidate: &str,
    main: &str,
    paths: &[String],
    generated_paths: &[String],
) -> anyhow::Result<Vec<Value>> {
    if paths.is_empty() {
        return Ok(vec![]);
    }
    let mut assets = Vec::new();
    for path in paths.iter().filter(|path| !generated_paths.contains(path)) {
        let bytes = std::fs::read(std::path::Path::new(candidate).join(path))?;
        assets.push(super::assets::Asset {
            path: path.clone(),
            sha256: hex::encode(Sha256::digest(&bytes)),
        });
    }
    let mut retained = if assets.is_empty() {
        vec![]
    } else {
        let report = planner::Report {
            assets,
            head: main.into(),
            checks: vec![],
            summary: String::new(),
        };
        super::assets::retain(home, std::path::Path::new(candidate), &report).await?
    };
    for path in generated_paths {
        retained.push(
            super::assets::retain_generated_execution_artifact(
                home,
                std::path::Path::new(candidate),
                main,
                path,
            )
            .await?,
        );
    }
    Ok(retained.iter().map(|r| json!(r)).collect())
}

async fn retain_failed_execution_evidence(
    home: &std::path::Path,
    candidate: &str,
    main: &str,
    paths: &[String],
) -> (Vec<Value>, Vec<String>) {
    let mut evidence = Vec::new();
    let mut errors = Vec::new();
    for path in paths {
        if !std::path::Path::new(candidate).join(path).exists() {
            continue;
        }
        match super::assets::retain_generated_execution_artifact(
            home,
            std::path::Path::new(candidate),
            main,
            path,
        )
        .await
        {
            Ok(asset) => evidence.push(json!(asset)),
            Err(error) => errors.push(format!("{path}: {error}")),
        }
    }
    (evidence, errors)
}

fn validate_execution_receipt(
    path: &std::path::Path,
    run_id: &str,
    main: &str,
    invocation_started: f64,
    invocation_finished: f64,
    required_stages: &[String],
) -> anyhow::Result<Value> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|e| anyhow::anyhow!("fresh execution receipt was not produced: {e}"))?;
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "execution receipt must be a regular file"
    );
    anyhow::ensure!(
        metadata.len() <= 16 * 1024 * 1024,
        "execution receipt exceeds 16 MiB"
    );
    let bytes = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("fresh execution receipt was not produced: {e}"))?;
    let receipt: Value = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("execution receipt is not valid JSON: {e}"))?;
    anyhow::ensure!(
        receipt["schema"] == "amux.execution_receipt.v1",
        "execution receipt schema must be amux.execution_receipt.v1"
    );
    anyhow::ensure!(
        receipt["run_id"] == run_id,
        "execution receipt belongs to a different invocation"
    );
    anyhow::ensure!(
        receipt["candidate_sha"] == main,
        "execution receipt belongs to a different candidate commit"
    );
    anyhow::ensure!(
        receipt["state"] == "passed",
        "execution receipt state is not passed"
    );
    let began = receipt["started_at"]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("execution receipt started_at must be a Unix timestamp"))?;
    let finished = receipt["finished_at"]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("execution receipt finished_at must be a Unix timestamp"))?;
    anyhow::ensure!(
        began >= invocation_started - 1.0
            && began <= finished
            && finished <= invocation_finished + 1.0,
        "execution receipt timestamps are outside this verifier invocation"
    );
    let subject = receipt["subject"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("execution receipt needs a subject object"))?;
    anyhow::ensure!(
        subject
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.trim().is_empty())
            && subject
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|v| !v.trim().is_empty()),
        "execution receipt subject needs non-empty kind and id"
    );
    let stages = receipt["stages"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("execution receipt needs a stages array"))?;
    let mut seen = HashSet::new();
    for stage in stages {
        let id = stage["id"]
            .as_str()
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("execution stage needs an id"))?;
        anyhow::ensure!(seen.insert(id), "duplicate execution stage {id}");
        anyhow::ensure!(
            stage["state"] == "passed",
            "execution receipt contains non-passing stage {id}"
        );
        anyhow::ensure!(
            stage["evidence"]
                .as_array()
                .is_some_and(|items| !items.is_empty()
                    && items
                        .iter()
                        .all(|item| item.as_object().is_some_and(|o| !o.is_empty()))),
            "execution stage {id} needs structured evidence"
        );
    }
    for required in required_stages {
        let matching: Vec<_> = stages
            .iter()
            .filter(|stage| stage["id"] == required.as_str())
            .collect();
        anyhow::ensure!(
            matching.len() == 1,
            "required execution stage {required} must occur exactly once"
        );
        let stage = matching[0];
        anyhow::ensure!(
            stage["state"] == "passed",
            "required execution stage {required} did not pass"
        );
        anyhow::ensure!(
            stage["evidence"]
                .as_array()
                .is_some_and(|items| !items.is_empty()),
            "required execution stage {required} has no machine-readable evidence"
        );
    }
    Ok(receipt)
}

/// The receipt is a claim by the runner. The harness compares each required stage with a
/// separate, freshly created raw measurement file and retains that file by content hash.
fn validate_execution_assertions(
    candidate: &std::path::Path,
    assertions: &[ExecutionAssertion],
) -> anyhow::Result<Vec<Value>> {
    let mut measurements = Vec::with_capacity(assertions.len());
    for assertion in assertions {
        let path = candidate.join(&assertion.artifact);
        let metadata = std::fs::symlink_metadata(&path)?;
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "raw evidence {} must be a regular file",
            assertion.artifact
        );
        anyhow::ensure!(
            metadata.len() <= 16 * 1024 * 1024,
            "raw evidence {} exceeds 16 MiB",
            assertion.artifact
        );
        let bytes = std::fs::read(&path)?;
        let raw: Value = serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("raw evidence {} is not JSON: {e}", assertion.artifact))?;
        let actual = raw.pointer(&assertion.pointer).ok_or_else(|| {
            anyhow::anyhow!(
                "raw evidence {} lacks {} for stage {}",
                assertion.artifact,
                assertion.pointer,
                assertion.stage
            )
        })?;
        let expected: Value = serde_json::from_str(&assertion.expected)?;
        let passed = match assertion.operator {
            ExecutionAssertionOperator::Equals => actual == &expected,
            ExecutionAssertionOperator::AtLeast => actual
                .as_f64()
                .zip(expected.as_f64())
                .is_some_and(|(a, b)| a.is_finite() && b.is_finite() && a >= b),
        };
        anyhow::ensure!(
            passed,
            "raw evidence assertion failed for stage {} at {}{}: expected {:?} {:?}, observed {}",
            assertion.stage,
            assertion.artifact,
            assertion.pointer,
            assertion.operator,
            expected,
            actual
        );
        measurements.push(json!({
            "stage": assertion.stage,
            "artifact": assertion.artifact,
            "sha256": hex::encode(Sha256::digest(&bytes)),
            "pointer": assertion.pointer,
            "operator": assertion.operator,
            "expected": expected,
            "observed": actual
        }));
    }
    Ok(measurements)
}

/// Docker is an independent witness for image identity. The candidate's verifier may name an
/// image, but it cannot satisfy this check by writing a JSON claim about that image.
async fn attest_docker_image(
    receipt: &Value,
    candidate_sha: &str,
) -> anyhow::Result<Option<Value>> {
    if receipt["subject"]["kind"] != "docker_image" {
        return Ok(None);
    }
    let context = receipt["environment"]["docker_context"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Docker execution receipt needs a docker_context"))?;
    let tag = receipt["subject"]["tag"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Docker execution receipt needs an image tag"))?;
    let observed = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("docker")
            .args([
                "--context",
                context,
                "image",
                "inspect",
                tag,
                "--format",
                "{{json .}}",
            ])
            .output(),
    )
    .await??;
    anyhow::ensure!(
        observed.status.success(),
        "Docker cannot inspect the claimed image: {}",
        tail(&String::from_utf8_lossy(&observed.stderr), 1000)
    );
    let image: Value = serde_json::from_slice(&observed.stdout)?;
    validate_docker_image_metadata(receipt, &image, candidate_sha)?;
    Ok(Some(json!({
        "source":"docker image inspect",
        "context":context,
        "tag":tag,
        "image_id":image["Id"],
        "candidate_sha":candidate_sha,
        "inspect_sha256":hex::encode(Sha256::digest(&observed.stdout))
    })))
}

fn validate_docker_image_metadata(
    receipt: &Value,
    image: &Value,
    candidate_sha: &str,
) -> anyhow::Result<()> {
    let id = image["Id"].as_str().unwrap_or_default();
    anyhow::ensure!(
        id == receipt["subject"]["id"].as_str().unwrap_or_default()
            && id.starts_with("sha256:")
            && id.len() == 71
            && id[7..].bytes().all(|b| b.is_ascii_hexdigit()),
        "Docker image ID does not match the execution receipt"
    );
    anyhow::ensure!(
        image["Config"]["Labels"]["org.amux.candidate"] == candidate_sha,
        "Docker image is not labeled with the exact candidate SHA"
    );
    Ok(())
}

/// A verifier can distinguish an infrastructure failure from a failed outcome without claiming
/// success. The existing fresh-run identity still has to match before the harness retries it.
fn declared_operational_failure(path: &std::path::Path, run_id: &str, candidate: &str) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Ok(receipt) = serde_json::from_slice::<Value>(&bytes) else {
        return false;
    };
    receipt["schema"] == "amux.execution_receipt.v1"
        && receipt["state"] == "operational_failure"
        && receipt["run_id"] == run_id
        && receipt["candidate_sha"] == candidate
}

/// Registry metadata resolution can fail before an image or container exists. This is an
/// infrastructure retry, never a successful acceptance result. Inspect only the fresh verifier's
/// declared log in its disposable candidate checkout; a worker's report cannot trigger a retry.
fn transient_docker_metadata_log(log: &str) -> bool {
    log.contains("$ docker ")
        && log.contains(" build ")
        && log.contains("load metadata for ")
        && [
            "DeadlineExceeded: context deadline exceeded",
            "TLS handshake timeout",
            "i/o timeout",
        ]
        .iter()
        .any(|reason| log.contains(reason))
}

fn transient_execution_failure(candidate: &str, evidence: &[String]) -> bool {
    evidence
        .iter()
        .filter(|path| path.ends_with("/execution.txt"))
        .any(|path| {
            let Ok(log) = std::fs::read_to_string(std::path::Path::new(candidate).join(path))
            else {
                return false;
            };
            transient_docker_metadata_log(&log)
        })
}

/// A failure recorded before this classifier shipped retries after restart if its retained,
/// harness-generated log proves the same transient failure before any container was started.
fn retryable_operational_result(result: &Value) -> bool {
    retryable_operational_result_at(
        result,
        &crate::config::amux_home().join("artifacts/project-reports"),
    )
}

fn retryable_operational_result_at(result: &Value, root: &std::path::Path) -> bool {
    if result["state"] == "operational_failure" {
        return true;
    }
    if result["state"] != "failed" {
        return false;
    }
    result["results"].as_array().is_some_and(|results| {
        results.iter().any(|criterion| {
            criterion["type"] == "execution"
                && criterion["state"] == "failed"
                && criterion["evidence"].as_array().is_some_and(|evidence| {
                    evidence.iter().any(|asset| {
                        let Some(source) = asset["source"]["path"].as_str() else {
                            return false;
                        };
                        let Some(path) = asset["path"].as_str() else {
                            return false;
                        };
                        let path = std::path::Path::new(path);
                        source.ends_with("/execution.txt")
                            && path.starts_with(root)
                            && std::fs::read_to_string(path)
                                .is_ok_and(|log| transient_docker_metadata_log(&log))
                    })
                })
        })
    })
}

/// Run the approved command criteria in a throwaway checkout of `main`. Returns None if the run was
/// cancelled because the inputs changed or the project paused; nothing is recorded then.
async fn run(
    state: &AppState,
    p: &store::Project,
    contract: &AcceptanceContract,
    main: &str,
    intent: &str,
    fp: &str,
) -> anyhow::Result<Option<Value>> {
    contract.validate().map_err(anyhow::Error::msg)?;
    let permit = || -> Result<(), String> {
        let c = state.store.read().map_err(|e| e.to_string())?;
        let cur = store::get(&c, &p.name)
            .map_err(|e| e.to_string())?
            .ok_or("acceptance cancelled: project disappeared")?;
        if cur.policy.paused || !cur.policy.enabled {
            return Err("acceptance cancelled: project paused".into());
        }
        if cur.policy.acceptance.as_ref() != Some(contract) {
            return Err("acceptance cancelled: contract changed".into());
        }
        if intent_revision(&c, &p.name).map_err(|e| e.to_string())? != intent {
            return Err("acceptance cancelled: project intent changed".into());
        }
        Ok(())
    };
    let started = crate::config::now_f64();
    let repo = p.policy.repository.clone();
    let temp = tempfile::Builder::new().prefix("amux-accept-").tempdir()?;
    let candidate = temp.path().join("candidate").to_string_lossy().into_owned();
    workspace::git(&repo, &["worktree", "add", "--detach", &candidate, main])
        .await
        .map_err(anyhow::Error::msg)?;
    let w = workspace::Workspace {
        repo: repo.clone(),
        path: candidate.clone(),
        branch: format!("amux/fanout/acceptance-{}", p.name),
        base: main.into(),
    };
    let home = crate::config::amux_home();
    let outcome = async {
        let mut results = Vec::new();
        for c in &contract.criteria {
            let (id, command, timeout_secs, execution) = match &c.verifier {
                ContractVerifier::Command { id, command, timeout_secs } => (id, command, timeout_secs, None),
                ContractVerifier::Execution { id, command, timeout_secs, receipt, required_stages, assertions } => (id, command, timeout_secs, Some((receipt, required_stages, assertions))),
                ContractVerifier::Human { .. } => {
                    match retain_evidence(&home, &candidate, main, &c.evidence, &[]).await {
                        Ok(evidence) => results.push(json!({"criterion":c.id,"verifier":c.verifier.id(),"type":"human","state":"pending_human","evidence":evidence})),
                        Err(error) => results.push(json!({"criterion":c.id,"verifier":c.verifier.id(),"type":"human","state":"failed","evidence_error":error.to_string()})),
                    }
                    continue;
                }
            };
            let base = json!({"criterion":c.id,"verifier":id,"type":if execution.is_some(){"execution"}else{"command"},"command":command});
            let with = |state: &str, extra: Value| {
                let mut r = base.clone();
                r["state"] = json!(state);
                if let (Some(o), Some(e)) = (r.as_object_mut(), extra.as_object()) {
                    o.extend(e.clone());
                }
                r
            };
            if let Err(e) = workspace::validate_verification_command(&w, command) {
                results.push(with("operational", json!({"output": tail(&e, 4000)})));
                continue;
            }
            if let Some((receipt, _, _)) = execution {
                let preexisting = std::iter::once(receipt.as_str())
                    .chain(c.evidence.iter().map(String::as_str))
                    .find(|path| std::path::Path::new(&candidate).join(path).exists());
                if let Some(path) = preexisting {
                    tracing::warn!(project=%p.name, criterion=%c.id, path, measured=true, n_considered=1, verdict="project.execution_evidence_preexisting", "execution verifier refused historical raw evidence");
                    results.push(with("failed", json!({"receipt_error":format!("execution evidence {path} already existed before this invocation; historical proof cannot satisfy a runtime gate")})));
                    continue;
                }
            }
            let timeout = Duration::from_secs(timeout_secs.unwrap_or(p.policy.verification_timeout_secs));
            let tool_path=workspace::verification_tool_path(&permit).await.map_err(anyhow::Error::msg)?;
            let mut cmd = workspace::verification_process(&tool_path,&candidate,command);
            let run_id = format!("{}-{}-{}", p.name, std::process::id(), chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default());
            cmd.env("AMUX_SESSION", format!("acceptance-{}", p.name))
                .env("AMUX_ACCEPTANCE_RUN_ID", &run_id)
                .env("AMUX_ACCEPTANCE_MAIN", main)
                .env("AMUX_ACCEPTANCE_STARTED_AT", format!("{started:.6}"))
                .env("AMUX_ACCEPTANCE_CANDIDATE", &candidate)
                .env("AMUX_ACCEPTANCE_ASSET_DIR", home.join("artifacts/project-reports"));
            if let Some((receipt, _, _)) = execution {
                cmd.env("AMUX_ACCEPTANCE_RECEIPT", receipt);
            }
            let began = Instant::now();
            let ran = workspace::checked_command(cmd, &permit, timeout).await;
            let elapsed_ms = began.elapsed().as_millis() as u64;
            match ran {
                Err(e) if e.starts_with("acceptance cancelled") => return Ok::<Option<Vec<Value>>, anyhow::Error>(None),
                Err(e) => results.push(with("operational", json!({"output": tail(&e, 4000), "elapsed_ms": elapsed_ms}))),
                Ok((status, output)) => {
                    if workspace::git(&candidate, &["rev-parse", "HEAD"]).await.map_err(anyhow::Error::msg)? != main {
                        results.push(with("operational", json!({"output":"the verifier moved the candidate checkout","elapsed_ms":elapsed_ms})));
                    } else if !status.success() {
                        let operational = execution.is_some_and(|(path, _, _)| {
                            declared_operational_failure(
                                &std::path::Path::new(&candidate).join(path),
                                &run_id,
                                main,
                            )
                        }) || (execution.is_some() && transient_execution_failure(&candidate, &c.evidence));
                        let (evidence, evidence_errors) = if execution.is_some() {
                            retain_failed_execution_evidence(&home, &candidate, main, &c.evidence).await
                        } else { (Vec::new(), Vec::new()) };
                        tracing::warn!(project=%p.name, criterion=%c.id, exit=?status.code(), operational, retained=evidence.len(), measured=true, n_considered=1, verdict="project.execution_command_failed", "project acceptance verifier failed with retained diagnostics");
                        results.push(with(if operational { "operational" } else { "failed" }, json!({"exit":status.code(),"output":tail(&output,4000),"elapsed_ms":elapsed_ms,"evidence":evidence,"evidence_errors":evidence_errors})));
                    } else {
                        let finished = crate::config::now_f64();
                        let receipt = if let Some((path, stages, assertions)) = execution {
                            Some(async {
                            let receipt = validate_execution_receipt(
                                &std::path::Path::new(&candidate).join(path),
                                &run_id,
                                main,
                                started,
                                finished,
                                stages,
                            )?;
                            let measurements = validate_execution_assertions(std::path::Path::new(&candidate), assertions)?;
                            let docker_attestation = attest_docker_image(&receipt, main).await?;
                            Ok::<_, anyhow::Error>((receipt, measurements, docker_attestation))
                            }.await)
                        } else { None };
                        let generated_paths = if execution.is_some() { c.evidence.clone() } else { Vec::new() };
                        let retained = retain_evidence(
                            &home,
                            &candidate,
                            main,
                            &c.evidence,
                            &generated_paths,
                        )
                        .await;
                        match (receipt, retained) {
                            (Some(Err(error)), evidence) => {
                                tracing::warn!(project=%p.name, criterion=%c.id, %error, measured=true, n_considered=1, verdict="project.execution_receipt_rejected", "runtime acceptance proof rejected");
                                results.push(with("failed", json!({"exit":0,"output":tail(&output,4000),"elapsed_ms":elapsed_ms,"receipt_error":error.to_string(),"evidence":evidence.unwrap_or_default()})));
                            }
                            (Some(Ok((receipt, measurements, docker_attestation))), Ok(evidence)) => results.push(with("passed", json!({"exit":0,"output":tail(&output,4000),"elapsed_ms":elapsed_ms,"evidence":evidence,"receipt":receipt,"measurements":measurements,"docker_attestation":docker_attestation}))),
                            (None, Ok(evidence)) => results.push(with("passed", json!({"exit":0,"output":tail(&output,4000),"elapsed_ms":elapsed_ms,"evidence":evidence}))),
                            (_, Err(e)) => results.push(with("failed", json!({"exit":0,"output":tail(&output,4000),"elapsed_ms":elapsed_ms,"evidence_error":e.to_string()}))),
                        }
                    }
                }
            }
        }
        Ok(Some(results))
    }
    .await;
    // Only this temporary checkout is disposable; retained evidence lives in the private asset store.
    let _ = workspace::git(&repo, &["worktree", "remove", "--force", &candidate]).await;
    let Some(results) = outcome? else {
        return Ok(None);
    };
    let has = |s: &str| results.iter().any(|r| r["state"] == s);
    let state = if has("operational") {
        "operational_failure"
    } else if has("failed") {
        "failed"
    } else if has("pending_human") {
        "awaiting_human"
    } else {
        "accepted"
    };
    Ok(Some(
        json!({"fingerprint":fp,"contract_revision":contract.revision,"main":main,"intent":intent,"state":state,
        "started":started,"finished":crate::config::now_f64(),"results":results}),
    ))
}

fn observed_at() -> &'static Mutex<HashMap<String, Instant>> {
    static AT: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    AT.get_or_init(Default::default)
}

/// Called from the project driver's existing tick. Cheap unless the project is settled and the
/// current inputs have not been evaluated: then it composes a candidate at most every 30 seconds,
/// runs the approved commands once per fingerprint, and records one immutable receipt.
pub(crate) async fn tick(state: &AppState, p: &store::Project) -> anyhow::Result<()> {
    let Some(contract) = p.policy.acceptance.clone() else {
        return Ok(());
    };
    if p.policy.paused || !p.policy.enabled {
        return Ok(());
    }
    let name = p.name.clone();
    let (is_settled, intent, seen, anchor) = {
        let c = state.store.read()?;
        let intent = intent_revision(&c, &name)?;
        (
            settled(&c, &name)?,
            intent.clone(),
            observed_main(&c, &name)?,
            published_anchor(&c, p, &contract, &intent)?,
        )
    };
    if !is_settled {
        return Ok(());
    }
    if let Some((result, _)) = anchor {
        let candidate = result["candidate"].as_str().unwrap_or_default().to_string();
        let key = format!("published:{name}");
        let due = observed_at()
            .lock()
            .unwrap()
            .get(&key)
            .is_none_or(|at| at.elapsed() >= OBSERVE_EVERY);
        if !due {
            return Ok(());
        }
        observed_at().lock().unwrap().insert(key, Instant::now());
        let repo = &p.policy.repository;
        let latest = async {
            workspace::git(repo, &["fetch", "origin", "main"]).await?;
            workspace::git(repo, &["rev-parse", "origin/main"]).await
        }
        .await;
        let latest = match latest {
            Ok(latest) => latest,
            Err(error) => {
                tracing::warn!(project=%name,%error,measured=false,n_considered=0,verdict="project.published_main_unavailable","published project remains accepted while main cannot be checked");
                return Ok(());
            }
        };
        if workspace::git(repo, &["merge-base", "--is-ancestor", &candidate, &latest])
            .await
            .is_ok()
        {
            return Ok(());
        }
        let project = name.clone();
        let lost_candidate = candidate.clone();
        state
            .store
            .write_async(move |c| {
                insert(
                    c,
                    &project,
                    "project.publication_lost",
                    &json!({"candidate":lost_candidate,"main":latest}),
                    "harness",
                )?;
                Ok(changed(&project, "publication_lost"))
            })
            .await?;
        tracing::warn!(project=%name,candidate,measured=true,n_considered=1,verdict="project.publication_lost","approved candidate is no longer in remote main; acceptance reopens");
    }
    let heads = {
        let c = state.store.read()?;
        verified_heads(&c, &name)?
    };
    let stale = seen.as_ref().is_none_or(|o| {
        o["intent"] != json!(intent)
            || o["contract_revision"] != json!(contract.revision)
            || o["heads"] != json!(heads)
    });
    let due = observed_at()
        .lock()
        .unwrap()
        .get(&name)
        .is_none_or(|t| t.elapsed() >= OBSERVE_EVERY);
    if stale || due {
        observed_at()
            .lock()
            .unwrap()
            .insert(name.clone(), Instant::now());
        let repo = p.policy.repository.clone();
        let observed = async {
            workspace::git(&repo, &["fetch", "origin", "main"]).await?;
            workspace::git(&repo, &["rev-parse", "origin/main"]).await
        }
        .await;
        match observed {
            Ok(base_main) => {
                let unchanged = seen.as_ref().is_some_and(|o| {
                    o["base_main"] == json!(base_main)
                        && o["heads"] == json!(heads)
                        && o["intent"] == json!(intent)
                        && o["contract_revision"] == json!(contract.revision)
                });
                if !unchanged {
                    let candidate = match assemble_candidate(p, &base_main, &heads).await {
                        Ok(candidate)=>candidate,
                        Err(error)=> {
                            if let Some(failure)=error.downcast_ref::<CompositionFailure>() {
                                let (project,task,head,context)=(p.clone(),failure.task.clone(),failure.head.clone(),json!({"candidate":failure.partial,"composition_error":failure.error,"task_heads":heads}));
                                state.store.write_async(move|c| {
                                    let current=store::get(c,&project.name).map_err(store::sql_error)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
                                    if current.revision!=project.revision { return Ok(WriteOutcome{applied:false,events:vec![]}); }
                                    repair_owner(c,&current,&task,&head,&context).map_err(store::sql_error)
                                }).await?;
                                return Ok(());
                            }
                            return Err(error);
                        }
                    };
                    let (project, base, composed, task_heads) =
                        (p.clone(), base_main, candidate, heads.clone());
                    state
                        .store
                        .write_async(move |c| {
                            observe(c, &project, &base, &composed, &task_heads)
                                .map_err(store::sql_error)
                        })
                        .await?;
                }
            }
            Err(error) => {
                tracing::warn!(project = %name, %error, measured = false, n_considered = 0, verdict = "project.acceptance_observe_failed", "base main could not be read; candidate assembly and acceptance stay pending");
                return Ok(());
            }
        }
    }
    let (fp, candidate, base_main, runnable) = {
        let c = state.store.read()?;
        let Some(fp) = current_fingerprint(&c, p)? else {
            return Ok(());
        };
        let observed = observed_main(&c, &name)?.unwrap_or(Value::Null);
        let candidate = observed["candidate"]
            .as_str()
            .or_else(|| observed["main"].as_str())
            .unwrap_or_default()
            .to_string();
        let base_main = observed["base_main"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let runnable = runnable(&c, &name, &fp)?;
        (fp, candidate, base_main, runnable)
    };
    if !runnable {
        return Ok(());
    }
    let Some(mut result) = run(state, p, &contract, &candidate, &intent, &fp).await? else {
        return Ok(());
    };
    result["candidate"] = json!(candidate);
    result["base_main"] = json!(base_main);
    if result["state"] == "awaiting_human" || result["state"] == "accepted" {
        match ensure_publish_gate(state, p, &base_main, &candidate).await {
            Ok(gate) => result["publish_gate"] = gate,
            Err(error) => {
                result["state"] = json!("operational_failure");
                result["publish_gate"] = json!({"state":"failed","error":error.to_string()});
                if let Some(items) = result["results"].as_array_mut() {
                    items.push(json!({"id":"publish-gate","state":"operational","output":error.to_string()}));
                }
            }
        }
    }
    // Commands can be long-running. Refresh the remote ref after them so an evaluation can never
    // be accepted for a commit that stopped being current while checks were running.
    workspace::git(&p.policy.repository, &["fetch", "origin", "main"])
        .await
        .map_err(anyhow::Error::msg)?;
    let after = workspace::git(&p.policy.repository, &["rev-parse", "origin/main"])
        .await
        .map_err(anyhow::Error::msg)?;
    if after != base_main {
        tracing::info!(project=%name, before=%base_main, after=%after, measured=true, n_considered=1, verdict="project.acceptance_main_advanced", "acceptance result discarded because main advanced during evaluation; the next tick will rebuild the candidate");
        return Ok(());
    }
    let (project, contract_w, intent_w) = (name.clone(), contract.clone(), intent.clone());
    state
        .store
        .write_async(move |c| {
            record(c, &project, &contract_w, &intent_w, &result).map_err(store::sql_error)
        })
        .await?;
    Ok(())
}

/// Publish exactly the candidate a human accepted. A changed main ref never receives a blind
/// merge: the project must be re-composed and re-reviewed against the new base instead.
pub(crate) async fn publish_accepted_candidate(
    state: &AppState,
    p: &store::Project,
) -> anyhow::Result<String> {
    let view = {
        let conn = state.store.read()?;
        status(&conn, p)?
    };
    anyhow::ensure!(
        view["state"] == "accepted",
        "project acceptance is not approved"
    );
    let candidate = view["candidate"]
        .as_str()
        .or_else(|| view["evaluated_candidate"].as_str())
        .ok_or_else(|| anyhow::anyhow!("accepted project has no candidate commit"))?;
    let base_main = view["main"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("accepted project has no base main commit"))?;
    workspace::git(&p.policy.repository, &["fetch", "origin", "main"])
        .await
        .map_err(anyhow::Error::msg)?;
    let current = workspace::git(&p.policy.repository, &["rev-parse", "origin/main"])
        .await
        .map_err(anyhow::Error::msg)?;
    if workspace::git(
        &p.policy.repository,
        &["merge-base", "--is-ancestor", candidate, &current],
    )
    .await
    .is_ok()
    {
        return Ok(current);
    }
    anyhow::ensure!(
        current == base_main,
        "main advanced after review; rebuild and review the project candidate again"
    );
    let signature = publish_gate_signature(&p.policy.repository, base_main, candidate).await?;
    anyhow::ensure!(
        view["publish_gate"]["state"] == "passed" && view["publish_gate"]["signature"] == signature,
        "accepted candidate is still waiting for its repository publication gate"
    );
    workspace::git(&p.policy.repository, &["fetch", "origin", "main"])
        .await
        .map_err(anyhow::Error::msg)?;
    let current = workspace::git(&p.policy.repository, &["rev-parse", "origin/main"])
        .await
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        current == base_main,
        "main advanced while the publication gate ran; rebuild and review the project candidate again"
    );
    workspace::git(
        &p.policy.repository,
        &[
            "push",
            "--no-verify",
            "origin",
            &format!("{candidate}:refs/heads/main"),
        ],
    )
    .await
    .map_err(anyhow::Error::msg)?;
    workspace::git(&p.policy.repository, &["fetch", "origin", "main"])
        .await
        .map_err(anyhow::Error::msg)?;
    let published = workspace::git(&p.policy.repository, &["rev-parse", "origin/main"])
        .await
        .map_err(anyhow::Error::msg)?;
    workspace::git(
        &p.policy.repository,
        &["merge-base", "--is-ancestor", candidate, &published],
    )
    .await
    .map_err(anyhow::Error::msg)?;
    tracing::info!(project=%p.name, candidate, published, measured=true, n_considered=1, verdict="project.accepted_candidate_published", "human-approved project candidate published to main");
    Ok(published)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_fresh_candidate_bound_operational_receipts_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipt.json");
        std::fs::write(&path, json!({"schema":"amux.execution_receipt.v1","state":"operational_failure","run_id":"run-now","candidate_sha":"candidate-now"}).to_string()).unwrap();
        assert!(declared_operational_failure(
            &path,
            "run-now",
            "candidate-now"
        ));
        assert!(!declared_operational_failure(
            &path,
            "run-old",
            "candidate-now"
        ));
        assert!(!declared_operational_failure(
            &path,
            "run-now",
            "candidate-old"
        ));
        std::fs::write(&path, json!({"schema":"amux.execution_receipt.v1","state":"failed","run_id":"run-now","candidate_sha":"candidate-now"}).to_string()).unwrap();
        assert!(!declared_operational_failure(
            &path,
            "run-now",
            "candidate-now"
        ));
    }

    #[test]
    fn transient_docker_metadata_failure_retries_without_claiming_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = "artifacts/goal/execution.txt".to_string();
        std::fs::create_dir_all(dir.path().join("artifacts/goal")).unwrap();
        let log = dir.path().join(&path);
        std::fs::write(&log, "$ docker --context local build --pull=false .\n#7 [internal] load metadata for docker.io/library/python:3.11\n#7 ERROR: DeadlineExceeded: context deadline exceeded\n[exit 1]\n").unwrap();
        assert!(transient_execution_failure(
            dir.path().to_str().unwrap(),
            std::slice::from_ref(&path)
        ));
        std::fs::write(&log, "$ docker --context local build --pull=false .\nAssertion failed: API did not create 100 objects\n[exit 1]\n").unwrap();
        assert!(!transient_execution_failure(
            dir.path().to_str().unwrap(),
            &[path]
        ));
    }

    #[test]
    fn retained_pre_classifier_failure_is_retried_but_a_semantic_failure_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("execution.txt");
        let result = json!({"state":"failed","results":[{"type":"execution","state":"failed","evidence":[{"source":{"path":"artifacts/goal/execution.txt"},"path":log}]}]});
        std::fs::write(&log, "$ docker build .\n#1 load metadata for docker.io/library/node:20-slim\nERROR: DeadlineExceeded: context deadline exceeded\n").unwrap();
        assert!(retryable_operational_result_at(&result, dir.path()));
        std::fs::write(
            &log,
            "Mixpeek lifecycle assertion failed: retrieval returned zero documents",
        )
        .unwrap();
        assert!(!retryable_operational_result_at(&result, dir.path()));
    }

    #[test]
    fn published_approval_survives_unrelated_main_but_not_changed_project_intent() {
        let dir = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(dir.path());
        let db = crate::db::Store::open(&dir.path().join("db")).unwrap();
        let home_path = dir.path().to_path_buf();
        db.write(move |c| {
            let p = project(c, Some(two()));
            let contract = p.policy.acceptance.clone().unwrap();
            verified(c, "T-1");
            let execution = planner::Execution {
                stage: "verified".into(),
                worker: "worker".into(),
                report: Some(report(&[])),
                ..Default::default()
            };
            c.execute("UPDATE issues SET execution_state=?1 WHERE id='T-1'", [serde_json::to_string(&execution).unwrap()])?;
            let heads = verified_heads(c, "p").unwrap();
            observe(c, &p, "base", "published", &heads).unwrap();
            let intent = intent_revision(c, "p").unwrap();
            let fp = status(c, &p).unwrap()["fingerprint"].as_str().unwrap().to_string();
            let mut outcome = result(&fp, "awaiting_human", &contract, "published", &intent,
                json!([{"criterion":"unit","state":"passed"},{"criterion":"owner","state":"pending_human"}]));
            outcome["base_main"] = json!("base");
            record(c, "p", &contract, &intent, &outcome).unwrap();
            insert(c, "p", "project.acceptance_approval", &json!({"fingerprint":fp,"criterion":"owner","decision":"approve"}), "operator")?;
            workspace::write_integration_status(&home_path, "worker", &json!({"status":"integrated","approved_candidate":true,"merged":"published"}));
            assert_eq!(status(c, &p).unwrap()["state"], "accepted");
            observe(c, &p, "later-base", "later-candidate", &heads).unwrap();
            let anchored = status(c, &p).unwrap();
            assert_eq!(anchored["state"], "accepted");
            assert_eq!(anchored["candidate"], "published");
            assert_eq!(anchored["main"], "base");
            insert(c, "p", "project.acceptance_approval", &json!({"fingerprint":fp,"criterion":"owner","decision":"reject"}), "operator")?;
            assert_eq!(status(c, &p).unwrap()["state"], "pending");
            insert(c, "p", "project.acceptance_approval", &json!({"fingerprint":fp,"criterion":"owner","decision":"approve"}), "operator")?;
            assert_eq!(status(c, &p).unwrap()["state"], "accepted");
            c.execute("UPDATE issues SET title='changed requirement' WHERE id='T-1'", [])?;
            assert_eq!(status(c, &p).unwrap()["state"], "pending");
            Ok(WriteOutcome { applied:false, events:vec![] })
        }).unwrap();
    }

    #[test]
    fn pre_gate_review_receipts_are_rechecked_before_publication() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE session_events(id INTEGER PRIMARY KEY,ts REAL,session TEXT,type TEXT,data TEXT,source TEXT);").unwrap();
        insert(
            &conn,
            "p",
            "project.acceptance",
            &json!({"fingerprint":"fp","state":"awaiting_human"}),
            "harness",
        )
        .unwrap();
        assert!(runnable(&conn, "p", "fp").unwrap());
        insert(
            &conn,
            "p",
            "project.acceptance",
            &json!({"fingerprint":"fp","state":"awaiting_human","publish_gate":{"state":"passed"}}),
            "harness",
        )
        .unwrap();
        assert!(!runnable(&conn, "p", "fp").unwrap());
    }

    #[tokio::test]
    async fn publication_gate_signature_reuses_only_the_same_patch_and_hook() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".githooks")).unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Gate Test"]);
        git(&["config", "user.email", "gate@example.invalid"]);
        std::fs::write(repo.join(".githooks/pre-push"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        let base = git(&["rev-parse", "HEAD"]);
        std::fs::write(repo.join("changed.txt"), "same patch\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "change"]);
        let first = git(&["rev-parse", "HEAD"]);
        let first_signature = publish_gate_signature(repo.to_str().unwrap(), &base, &first)
            .await
            .unwrap();
        git(&["checkout", "-q", "-b", "later", &base]);
        std::fs::write(repo.join("unrelated.txt"), "other worker\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "unrelated"]);
        let later_base = git(&["rev-parse", "HEAD"]);
        git(&["cherry-pick", &first]);
        let later_candidate = git(&["rev-parse", "HEAD"]);
        assert_eq!(
            first_signature,
            publish_gate_signature(repo.to_str().unwrap(), &later_base, &later_candidate)
                .await
                .unwrap()
        );
        std::fs::write(repo.join("changed.txt"), "different patch\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "change payload"]);
        let changed = git(&["rev-parse", "HEAD"]);
        assert_ne!(
            first_signature,
            publish_gate_signature(repo.to_str().unwrap(), &later_base, &changed)
                .await
                .unwrap()
        );
        std::fs::write(repo.join(".githooks/pre-push"), "#!/bin/sh\necho updated\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "change hook"]);
        let changed_hook = git(&["rev-parse", "HEAD"]);
        assert_ne!(
            publish_gate_signature(repo.to_str().unwrap(), &later_base, &changed)
                .await
                .unwrap(),
            publish_gate_signature(repo.to_str().unwrap(), &later_base, &changed_hook)
                .await
                .unwrap()
        );
    }

    #[test]
    fn approved_review_retains_only_preexisting_undelivered_owner_messages() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE issues(id TEXT,session TEXT,project_group TEXT,status TEXT,deleted INTEGER);\
            CREATE TABLE steering_queue(id TEXT PRIMARY KEY,session TEXT,text TEXT,queued_at REAL,guard TEXT,precond_card TEXT,delivering_since REAL,sender TEXT);\
            CREATE TABLE steering_history(id TEXT PRIMARY KEY,session TEXT,text TEXT,queued_at REAL,delivered_at REAL,outcome TEXT,guard TEXT,sender TEXT);\
            CREATE TABLE session_events(ts REAL,session TEXT,type TEXT,data TEXT,idem TEXT,source TEXT);\
            INSERT INTO issues VALUES('T-1','worker','project','verified',NULL);\
            INSERT INTO issues VALUES('T-2','other','other-project','verified',0);\
            INSERT INTO steering_queue VALUES('before','worker','include the image digest',100,'project-steering','T-1',NULL,'operator');\
            INSERT INTO steering_queue VALUES('after','worker','new work after approval',300,'project-steering','T-1',NULL,'operator');\
            INSERT INTO steering_queue VALUES('foreign','other','unrelated',100,'project-steering','T-2',NULL,'operator');\
            INSERT INTO session_events VALUES(200,'project:project','project.acceptance_approval','{\"fingerprint\":\"fp\",\"decision\":\"approve\"}',NULL,'operator');").unwrap();
        let preview = pending_owner_messages(&conn, "project").unwrap();
        assert_eq!(preview.len(), 2);
        assert!(preview.iter().all(|m| m["delivered"] == false));
        assert!(
            !settle_approved_owner_messages(&conn, "project", "worker", "wrong")
                .unwrap()
                .applied
        );
        assert!(
            settle_approved_owner_messages(&conn, "project", "worker", "fp")
                .unwrap()
                .applied
        );
        let retained: (String, String) = conn
            .query_row(
                "SELECT text,outcome FROM steering_history WHERE id='before'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            retained,
            (
                "include the image digest".into(),
                "void:project-review-approved".into()
            )
        );
        let remaining: Vec<String> = conn
            .prepare("SELECT id FROM steering_queue ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(remaining, vec!["after", "foreign"]);
        assert!(
            !settle_approved_owner_messages(&conn, "project", "worker", "fp")
                .unwrap()
                .applied
        );
    }

    #[test]
    fn execution_receipt_is_fresh_candidate_bound_and_stage_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipt.json");
        let base = json!({
            "schema":"amux.execution_receipt.v1",
            "run_id":"run-current",
            "candidate_sha":"main-current",
            "state":"passed",
            "started_at":100.0,
            "finished_at":110.0,
            "subject":{"kind":"docker_image","id":"sha256:current"},
            "stages":[
                {"id":"image-build","state":"passed","evidence":[{"digest":"sha256:current"}]},
                {"id":"api-lifecycle","state":"passed","evidence":[{"objects":100}]},
                {"id":"network-isolation","state":"passed","evidence":[{"network":"none"}]}
            ]
        });
        std::fs::write(&path, serde_json::to_vec(&base).unwrap()).unwrap();
        let required = vec![
            "image-build".into(),
            "api-lifecycle".into(),
            "network-isolation".into(),
        ];
        assert!(validate_execution_receipt(
            &path,
            "run-current",
            "main-current",
            99.0,
            111.0,
            &required
        )
        .is_ok());

        let rejected = |receipt: Value, expected: &str| {
            std::fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
            let error = validate_execution_receipt(
                &path,
                "run-current",
                "main-current",
                99.0,
                111.0,
                &required,
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains(expected), "{error}");
        };
        let mut historical = base.clone();
        historical["run_id"] = json!("old-run");
        rejected(historical, "different invocation");
        let mut wrong_candidate = base.clone();
        wrong_candidate["candidate_sha"] = json!("main-old");
        rejected(wrong_candidate, "different candidate");
        let mut failed_stage = base.clone();
        failed_stage["stages"][1]["state"] = json!("failed");
        rejected(failed_stage, "non-passing stage api-lifecycle");
        let mut prose_only = base;
        prose_only["stages"][1]["evidence"] = json!([]);
        rejected(prose_only, "structured evidence");
    }

    #[test]
    fn raw_measurement_must_match_contract_even_when_receipt_claims_passed() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join("proof.json");
        std::fs::write(&artifact, br#"{"uploaded":2}"#).unwrap();
        let assertion = ExecutionAssertion {
            stage: "object-ingest".into(),
            artifact: "proof.json".into(),
            pointer: "/uploaded".into(),
            operator: ExecutionAssertionOperator::AtLeast,
            expected: "100".into(),
        };
        let error = validate_execution_assertions(dir.path(), std::slice::from_ref(&assertion))
            .unwrap_err()
            .to_string();
        assert!(error.contains("expected"), "{error}");
        std::fs::write(&artifact, br#"{"uploaded":100}"#).unwrap();
        let passed = validate_execution_assertions(dir.path(), &[assertion]).unwrap();
        assert_eq!(passed[0]["observed"], 100);
        assert_eq!(passed[0]["sha256"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn docker_witness_rejects_a_claimed_image_or_commit_that_daemon_did_not_observe() {
        let id = format!("sha256:{}", "a".repeat(64));
        let receipt = json!({"subject":{"id":id}});
        let observed = json!({"Id":id,"Config":{"Labels":{"org.amux.candidate":"candidate-a"}}});
        assert!(validate_docker_image_metadata(&receipt, &observed, "candidate-a").is_ok());
        assert!(
            validate_docker_image_metadata(&receipt, &observed, "candidate-b")
                .unwrap_err()
                .to_string()
                .contains("candidate SHA")
        );
        let other = json!({"Id":format!("sha256:{}", "b".repeat(64)),
            "Config":{"Labels":{"org.amux.candidate":"candidate-a"}}});
        assert!(
            validate_docker_image_metadata(&receipt, &other, "candidate-a")
                .unwrap_err()
                .to_string()
                .contains("image ID")
        );
    }

    #[tokio::test]
    async fn project_candidate_composes_every_verified_worker_head_without_touching_main() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(&repo)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Candidate Test"]);
        git(&["config", "user.email", "candidate@example.invalid"]);
        git(&["config", "core.hooksPath", "/dev/null"]);
        std::fs::write(repo.join("README.md"), "base\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "base"]);
        git(&["branch", "-M", "main"]);
        let base = git(&["rev-parse", "HEAD"]);

        git(&["checkout", "-q", "-b", "worker-a"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-m", "worker a"]);
        let a = git(&["rev-parse", "HEAD"]);
        git(&["checkout", "-q", "main"]);
        git(&["checkout", "-q", "-b", "worker-b"]);
        std::fs::write(repo.join("b.txt"), "b\n").unwrap();
        git(&["add", "b.txt"]);
        git(&["commit", "-m", "worker b"]);
        let b = git(&["rev-parse", "HEAD"]);
        git(&["checkout", "-q", "main"]);

        let project = store::Project {
            name: "compose-two".into(),
            revision: 1,
            policy: serde_json::from_value(json!({
                "repository": repo,
                "coordinator":{"provider":"codex","model":"gpt-5.5","effort":"low"},
                "executor":{"provider":"codex","model":"gpt-5.5","effort":"low"},
                "verify_command":"true"
            }))
            .unwrap(),
        };
        let candidate = assemble_candidate(
            &project,
            &base,
            &[("T-1".into(), a.clone()), ("T-2".into(), b.clone())],
        )
        .await
        .unwrap();
        assert_eq!(
            git(&["rev-parse", "main"]),
            base,
            "assembly cannot move main"
        );
        for head in [&a, &b] {
            assert!(git(&["merge-base", "--is-ancestor", head, &candidate]).is_empty());
        }
        assert_eq!(git(&["show", &format!("{candidate}:a.txt")]), "a");
        assert_eq!(git(&["show", &format!("{candidate}:b.txt")]), "b");
        git(&["checkout","-q","-b","conflict-left","main"]);
        std::fs::write(repo.join("README.md"),"left\n").unwrap();git(&["commit","-am","left"]);
        let left=git(&["rev-parse","HEAD"]);
        git(&["checkout","-q","-b","conflict-right","main"]);
        std::fs::write(repo.join("README.md"),"right\n").unwrap();git(&["commit","-am","right"]);
        let right=git(&["rev-parse","HEAD"]);git(&["checkout","-q","main"]);
        let error=assemble_candidate(&project,&base,&[("C-1".into(),left.clone()),("C-2".into(),right.clone())]).await.unwrap_err();
        let failure=error.downcast_ref::<CompositionFailure>().unwrap();
        assert_eq!(failure.task,"C-2");assert_eq!(failure.head,right);
        assert!(git(&["merge-base","--is-ancestor",&left,&failure.partial]).is_empty());
        // Simulate the owner's committed resolution, including both heads.
        git(&["checkout","-q","conflict-right"]);
        git(&["merge","--no-ff","-s","ours","--no-edit",&left]);
        std::fs::write(repo.join("README.md"),"resolved left and right\n").unwrap();git(&["commit","-am","resolved"]);
        let resolved=git(&["rev-parse","HEAD"]);git(&["checkout","-q","main"]);
        let candidate=assemble_candidate(&project,&base,&[("C-1".into(),left),("C-2".into(),right),("C-3".into(),resolved)]).await.unwrap();
        assert_eq!(git(&["show",&format!("{candidate}:README.md")]),"resolved left and right");
        assert_eq!(git(&["rev-parse","main"]),base);

    }

    #[tokio::test]
    async fn execution_verifier_runs_fresh_and_retains_candidate_bound_proof() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        let repo = home.path().join("repo");
        std::fs::create_dir_all(repo.join("scripts")).unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(&repo)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Acceptance Test"]);
        git(&["config", "user.email", "acceptance@example.invalid"]);
        git(&["config", "core.hooksPath", "/dev/null"]);
        std::fs::write(
            repo.join("scripts/prove.py"),
            r#"import json, os, pathlib, time
path = pathlib.Path(os.environ["AMUX_ACCEPTANCE_RECEIPT"])
path.parent.mkdir(parents=True, exist_ok=True)
now = time.time()
path.with_name("raw.json").write_text(json.dumps({"objects": 100, "retention_dir": os.environ["AMUX_ACCEPTANCE_ASSET_DIR"]}))
path.write_text(json.dumps({
  "schema": "amux.execution_receipt.v1",
  "run_id": os.environ["AMUX_ACCEPTANCE_RUN_ID"],
  "candidate_sha": os.environ["AMUX_ACCEPTANCE_MAIN"],
  "state": "passed",
  "started_at": now,
  "finished_at": now,
  "subject": {"kind": "process", "id": "fixture-run"},
  "stages": [{"id": "api-lifecycle", "state": "passed", "evidence": [{"objects": 100}]}]
}))
"#,
        )
        .unwrap();
        std::fs::write(repo.join("README.md"), "# fixture\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "fixture"]);
        git(&["branch", "-M", "main"]);
        let main = git(&["rev-parse", "HEAD"]);

        let contract = contract(json!({"criteria":[{
            "id":"lifecycle",
            "requirement":"Full lifecycle passes in a running image",
            "verifier":{"type":"execution","id":"fresh-run","command":"python3 scripts/prove.py","receipt":"artifacts/execution.json","required_stages":["api-lifecycle"],
                "assertions":[{"stage":"api-lifecycle","artifact":"artifacts/raw.json","pointer":"/objects","operator":"at_least","expected":"100"}]},
            "evidence":["artifacts/execution.json","artifacts/raw.json"]
        }]}));
        let store = crate::db::Store::open(&home.path().join("amux.db")).unwrap();
        let repository = repo.to_string_lossy().into_owned();
        let contract_for_policy = contract.clone();
        store
            .write(move |c| {
                let mut policy: amux_core::project::ExecutionPolicy = serde_json::from_value(json!({
                    "repository": repository,
                    "coordinator":{"provider":"codex","model":"gpt-5.5","effort":"low"},
                    "executor":{"provider":"codex","model":"gpt-5.5","effort":"low"},
                    "verify_command":"true",
                    "enabled":true
                }))
                .unwrap();
                policy.acceptance = Some(contract_for_policy);
                store::save(c, "receipt-project", 0, &policy, "test").map_err(store::sql_error)?;
                c.execute("INSERT INTO issues(id,title,status,type,project_group,created,updated,next_action,acceptance_criteria) VALUES('T-1','runtime outcome','verified','code','receipt-project',1,1,'retain proof','[\"contract:lifecycle\"]')", [])?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let project = store::get(&state.store.read().unwrap(), "receipt-project")
            .unwrap()
            .unwrap();
        let contract = project.policy.acceptance.clone().unwrap();
        let intent = intent_revision(&state.store.read().unwrap(), "receipt-project").unwrap();
        let result = run(&state, &project, &contract, &main, &intent, "fingerprint")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result["state"], "accepted", "{result:#}");
        assert_eq!(result["results"][0]["receipt"]["candidate_sha"], main);
        assert_eq!(result["results"][0]["measurements"][0]["observed"], 100);
        assert_eq!(
            result["results"][0]["receipt"]["subject"]["id"],
            "fixture-run"
        );
        let retained = result["results"][0]["evidence"][0]["path"]
            .as_str()
            .unwrap();
        assert!(std::path::Path::new(retained).is_file(), "{retained}");
        let raw = result["results"][0]["evidence"][1]["path"]
            .as_str()
            .unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(raw).unwrap()).unwrap();
        assert_eq!(
            raw["retention_dir"],
            home.path()
                .join("artifacts/project-reports")
                .to_string_lossy()
                .as_ref()
        );

        // The same real call path must reject a runner that emits a passing receipt but
        // produces too few objects in its raw measurement file.
        let script = repo.join("scripts/prove.py");
        let original = std::fs::read_to_string(&script).unwrap();
        std::fs::write(
            &script,
            original.replace(
                "json.dumps({\"objects\": 100,",
                "json.dumps({\"objects\": 2,",
            ),
        )
        .unwrap();
        git(&["add", "scripts/prove.py"]);
        git(&["commit", "-m", "false raw result"]);
        let false_main = git(&["rev-parse", "HEAD"]);
        let rejected = run(
            &state,
            &project,
            &contract,
            &false_main,
            &intent,
            "false-result",
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(rejected["state"], "failed", "{rejected:#}");
        assert!(rejected["results"][0]["receipt_error"]
            .as_str()
            .unwrap()
            .contains("raw evidence assertion failed"));
        assert_eq!(
            rejected["results"][0]["evidence"].as_array().unwrap().len(),
            2
        );

        // A nonzero runner still retains the raw file that explains the failure.
        let failing = std::fs::read_to_string(&script).unwrap().replace(
            "path.write_text(json.dumps({",
            "raise SystemExit(1)\npath.write_text(json.dumps({",
        );
        std::fs::write(&script, failing).unwrap();
        git(&["add", "scripts/prove.py"]);
        git(&["commit", "-m", "failed execution"]);
        let failed_main = git(&["rev-parse", "HEAD"]);
        let failed = run(
            &state,
            &project,
            &contract,
            &failed_main,
            &intent,
            "failed-run",
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(failed["state"], "failed", "{failed:#}");
        assert_eq!(failed["results"][0]["exit"], 1);
        assert_eq!(
            failed["results"][0]["evidence"].as_array().unwrap().len(),
            1
        );
    }

    /// Opt-in hardware test: runs the real Mixpeek image lifecycle through this exact harness
    /// path. It is ignored in CI because it needs a local Docker daemon and can take minutes.
    #[tokio::test]
    #[ignore = "set AMUX_REAL_SINGLE_IMAGE_REPO and MIXPEEK_DOCKER_CONTEXT to run the real image lifecycle"]
    async fn real_single_image_lifecycle_requires_raw_evidence_and_docker_witness() {
        let repo = std::env::var("AMUX_REAL_SINGLE_IMAGE_REPO").expect("Mixpeek repository path");
        let main = workspace::git(&repo, &["rev-parse", "HEAD"]).await.unwrap();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        let contract: AcceptanceContract = serde_json::from_str(include_str!(
            "../../../../docs/examples/single-image-acceptance.json"
        ))
        .unwrap();
        contract.validate().unwrap();
        let store = crate::db::Store::open(&home.path().join("amux.db")).unwrap();
        let policy_contract = contract.clone();
        store.write(move |c| {
            let mut policy: amux_core::project::ExecutionPolicy = serde_json::from_value(json!({
                "repository":repo,
                "coordinator":{"provider":"codex","model":"gpt-5.5","effort":"low"},
                "executor":{"provider":"codex","model":"gpt-5.5","effort":"low"},
                "verify_command":"true",
                "enabled":true
            })).unwrap();
            policy.acceptance = Some(policy_contract);
            store::save(c, "single-image-real", 0, &policy, "test").map_err(store::sql_error)?;
            c.execute("INSERT INTO issues(id,title,status,type,project_group,created,updated,next_action,acceptance_criteria) VALUES('IMG-1','single image lifecycle','verified','code','single-image-real',1,1,'retain proof','[\"contract:single-image-lifecycle\"]')", [])?;
            Ok(WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let project = store::get(&state.store.read().unwrap(), "single-image-real")
            .unwrap()
            .unwrap();
        let contract = project.policy.acceptance.clone().unwrap();
        let intent = intent_revision(&state.store.read().unwrap(), "single-image-real").unwrap();
        let result = run(&state, &project, &contract, &main, &intent, "real-run")
            .await
            .unwrap()
            .unwrap();
        if let Ok(proof_dir) = std::env::var("AMUX_REAL_PROOF_DIR") {
            std::fs::create_dir_all(&proof_dir).unwrap();
            std::fs::write(
                std::path::Path::new(&proof_dir).join("acceptance-result.json"),
                serde_json::to_vec_pretty(&result).unwrap(),
            )
            .unwrap();
            for outcome in result["results"].as_array().unwrap() {
                for asset in outcome["evidence"].as_array().into_iter().flatten() {
                    if let Some(source) = asset["path"].as_str() {
                        let name = std::path::Path::new(source).file_name().unwrap();
                        std::fs::copy(source, std::path::Path::new(&proof_dir).join(name)).unwrap();
                    }
                }
            }
        }
        assert_eq!(result["state"], "awaiting_human", "{result:#}");
        assert_eq!(result["results"][0]["state"], "passed", "{result:#}");
        assert_eq!(
            result["results"][0]["measurements"]
                .as_array()
                .unwrap()
                .len(),
            12
        );
        assert_eq!(
            result["results"][0]["docker_attestation"]["candidate_sha"],
            main
        );
        assert_eq!(
            result["results"][0]["evidence"].as_array().unwrap().len(),
            5
        );
    }

    fn contract(v: Value) -> AcceptanceContract {
        serde_json::from_value(v).unwrap()
    }
    fn two() -> AcceptanceContract {
        contract(json!({"revision":1,"criteria":[
            {"id":"unit","requirement":"unit tests pass","verifier":{"type":"command","id":"unit-tests","command":"cargo test"}},
            {"id":"owner","requirement":"owner reviews","verifier":{"type":"human","id":"owner-review","instructions":"look at it"}}]}))
    }
    fn report(checks: &[(&str, &str)]) -> planner::Report {
        planner::Report {
            assets: vec![],
            head: "a".repeat(40),
            summary: String::new(),
            checks: checks
                .iter()
                .map(|(c, k)| planner::Check {
                    criterion: (*c).into(),
                    command: (*k).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_task_report_cannot_replace_invent_repeat_or_self_approve_contract_criteria() {
        let c = two();
        let refs = |ids: &[&str]| {
            ids.iter()
                .map(|i| format!("contract:{i}"))
                .collect::<Vec<_>>()
        };
        assert!(contract_binding(
            &refs(&["unit"]),
            &report(&[("contract:unit", "cargo test")]),
            Some(&c)
        )
        .is_ok());
        assert!(
            contract_binding(
                &refs(&["unit"]),
                &report(&[("contract:unit", "true")]),
                Some(&c)
            )
            .is_err(),
            "replaced with true"
        );
        assert!(
            contract_binding(
                &refs(&["ghost"]),
                &report(&[("contract:ghost", "true")]),
                Some(&c)
            )
            .is_err(),
            "invented id"
        );
        assert!(
            contract_binding(
                &refs(&["unit", "unit"]),
                &report(&[("contract:unit", "cargo test")]),
                Some(&c)
            )
            .is_err(),
            "repeated id"
        );
        assert!(
            contract_binding(
                &refs(&["owner"]),
                &report(&[("contract:owner", "true")]),
                Some(&c)
            )
            .is_err(),
            "executor cannot satisfy a human criterion"
        );
        assert!(
            contract_binding(&refs(&["unit"]), &report(&[]), Some(&c)).is_err(),
            "omitted check"
        );
        assert!(
            contract_binding(
                &refs(&["unit"]),
                &report(&[
                    ("contract:unit", "cargo test"),
                    ("contract:unit", "cargo test")
                ]),
                Some(&c)
            )
            .is_err(),
            "duplicated check"
        );
        assert!(
            contract_binding(
                &refs(&["unit"]),
                &report(&[("contract:unit", "cargo test")]),
                None
            )
            .is_err(),
            "no contract configured"
        );
        // Tasks that never mention the contract are untouched.
        assert!(contract_binding(
            &["plain prose".into()],
            &report(&[("plain prose", "true")]),
            Some(&c)
        )
        .is_ok());
    }

    #[test]
    fn contract_bound_reports_must_include_required_evidence_assets() {
        let c = contract(json!({"revision":1,"criteria":[
            {"id":"artifact","requirement":"artifact exists","verifier":{"type":"command","id":"artifact-check","command":"python3 scripts/verify.py"},"evidence":["docs/result.md"]}
        ]}));
        let refs = vec!["contract:artifact".to_string()];
        let mut missing = report(&[("contract:artifact", "python3 scripts/verify.py")]);
        missing.assets = vec![super::super::assets::Asset {
            path: "docs/wrong.md".into(),
            sha256: "0".repeat(64),
        }];
        let err = contract_binding(&refs, &missing, Some(&c)).unwrap_err();
        assert!(err
            .to_string()
            .contains("contract:artifact requires reported asset docs/result.md"));

        missing.assets = vec![super::super::assets::Asset {
            path: "docs/result.md".into(),
            sha256: "0".repeat(64),
        }];
        assert!(contract_binding(&refs, &missing, Some(&c)).is_ok());
    }

    #[test]
    fn execution_evidence_is_produced_by_project_acceptance_not_the_worker() {
        let c = contract(json!({"revision":1,"criteria":[{
            "id":"image","requirement":"fresh image lifecycle passes",
            "verifier":{"type":"execution","id":"run-image","command":"python3 scripts/run_image.py","receipt":"artifacts/image/receipt.json","required_stages":["image-build"],"timeout_secs":120},
            "evidence":["artifacts/image/receipt.json","artifacts/image/log.txt"]
        }]}));
        let refs = vec!["contract:image".to_string()];
        let mut candidate = report(&[("contract:image", "python3 scripts/run_image.py")]);
        candidate.assets.push(super::super::assets::Asset {
            path: "artifacts/candidate.md".into(),
            sha256: "0".repeat(64),
        });
        assert!(contract_binding(&refs, &candidate, Some(&c)).is_ok());
        candidate.checks[0].command = "true".into();
        assert!(contract_binding(&refs, &candidate, Some(&c)).is_err());
    }

    #[test]
    fn the_planner_may_reference_only_approved_command_criteria() {
        let c = two();
        assert!(check_plan_refs(&c, &[vec!["contract:unit".into(), "prose".into()]]).is_ok());
        assert!(check_plan_refs(&c, &[vec!["contract:ghost".into()]]).is_err());
        assert!(check_plan_refs(&c, &[vec!["contract:owner".into()]]).is_err());
        assert!(
            check_plan_refs(&c, &[vec!["contract:unit".into(), "contract:unit".into()]]).is_err()
        );
        let text = catalogue(&c);
        assert!(
            text.contains("contract:unit = unit tests pass") && !text.contains("contract:owner")
        );
    }

    fn project(db: &Connection, contract: Option<AcceptanceContract>) -> store::Project {
        let mut policy: amux_core::project::ExecutionPolicy = serde_json::from_value(json!({"repository":"/repo","coordinator":{"provider":"claude","model":"haiku"},
            "executor":{"provider":"claude","model":"sonnet"},"verify_command":"true","enabled":true})).unwrap();
        policy.acceptance = contract;
        store::save(
            db,
            "p",
            store::get(db, "p").unwrap().map_or(0, |x| x.revision),
            &policy,
            "test",
        )
        .unwrap();
        store::get(db, "p").unwrap().unwrap()
    }
    fn verified(db: &Connection, id: &str) {
        db.execute("INSERT INTO issues(id,title,status,type,project_group,created,updated,next_action,acceptance_criteria) VALUES(?1,?1,'verified','doc','p',1,1,'do it','[\"x\"]')", [id]).unwrap();
    }
    #[test]
    fn acceptance_failure_repairs_the_owner_preserves_receipts_and_respects_limits() {
        let c=crate::db::migrate::test_memdb();
        let mut p=project(&c,Some(two()));
        verified(&c,"T-1");verified(&c,"T-2");
        c.execute("UPDATE issues SET acceptance_criteria='[\"contract:unit\"]' WHERE id='T-1'",[]).unwrap();
        c.execute("INSERT INTO issues(id,title,status,type,project_group,created,updated,depends_on) VALUES('EP','parent','verified','epic','p',1,1,'[\"T-1\",\"T-2\"]')",[]).unwrap();
        let row=bs::get_issue(&c,"T-1").unwrap().unwrap();
        let mut e=planner::Execution{stage:"verified".into(),attempt:2,generation:2,worker:"owned-worker".into(),input_hash:planner::input_hash(&row),report:Some(report(&[("contract:unit","cargo test")])),..Default::default()};
        e.report.as_mut().unwrap().head="a".repeat(40);
        planner::save_execution(&c,&row,&e,"fixture").unwrap();
        let failure=json!({"state":"failed","fingerprint":"failure-1","candidate":"b".repeat(40),"results":[{"criterion":"unit","state":"failed","output":"real integration failed","exit":1},{"criterion":"owner","state":"failed","evidence_error":"missing approval"}]});
        p.policy.paused=true;assert!(!repair_failed_criteria(&c,&p,&failure).unwrap().applied);p.policy.paused=false;
        let mut held=e.clone();held.suspended=true;planner::save_execution(&c,&row,&held,"fixture").unwrap();
        assert!(!repair_failed_criteria(&c,&p,&failure).unwrap().applied);
        planner::save_execution(&c,&row,&e,"fixture").unwrap();
        assert!(!repair_owner(&c,&p,"T-1","wrong-head",&failure).unwrap().applied);
        assert!(repair_failed_criteria(&c,&p,&failure).unwrap().applied);
        let mut next=planner::execution(&c,"T-1").unwrap();
        assert_eq!(next.stage,"repair");assert_eq!(next.worker,"owned-worker");assert_eq!(next.attempt_limit(2),3);
        assert_eq!(next.retry_grants[0].previous_result["report"]["head"],"a".repeat(40));
        assert!(next.waiting.as_ref().unwrap().contains("real integration failed"));
        assert_eq!(bs::get_issue(&c,"EP").unwrap().unwrap().status,"backlog");
        assert_eq!(bs::get_issue(&c,"T-2").unwrap().unwrap().status,"verified");
        assert!(!repair_failed_criteria(&c,&p,&failure).unwrap().applied,"unchanged failed run cannot queue another turn");
        next.stage="verified".into();next.attempt=3;next.generation=3;planner::save_execution(&c,&row,&next,"fixture").unwrap();
        assert!(repair_failed_criteria(&c,&p,&failure).unwrap().applied);
        let mut next=planner::execution(&c,"T-1").unwrap();next.stage="verified".into();next.attempt=4;next.generation=4;
        planner::save_execution(&c,&row,&next,"fixture").unwrap();
        assert!(!repair_failed_criteria(&c,&p,&failure).unwrap().applied,"bounded integrated repairs conserve tokens");
        assert_eq!(events(&c,"p","project.acceptance_repair",None).unwrap().len(),2);
    }

    #[tokio::test]
    async fn empty_project_waits_without_trying_to_compose_candidate_heads() {
        let dir = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(crate::db::Store::open(&dir.path().join("db")).unwrap());
        db.write(|c| {
            project(c, Some(two()));
            Ok(crate::db::WriteOutcome {
                applied: false,
                events: vec![],
            })
        })
        .unwrap();
        let p = {
            let c = db.read().unwrap();
            store::get(&c, "p").unwrap().unwrap()
        };
        let state = crate::api::AppState {
            store: db,
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        assert!(!settled(&state.store.read().unwrap(), "p").unwrap());
        tick(&state, &p).await.unwrap();
    }
    fn result(
        fp: &str,
        state: &str,
        contract: &AcceptanceContract,
        main: &str,
        intent: &str,
        results: Value,
    ) -> Value {
        json!({"fingerprint":fp,"contract_revision":contract.revision,"main":main,"intent":intent,"state":state,"finished":1.0,"results":results,"publish_gate":{"state":"passed","signature":"test"}})
    }

    #[test]
    fn acceptance_is_derived_from_current_inputs_never_from_task_rollup() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Store::open(&dir.path().join("db")).unwrap();
        db.write(|c| {
            // Legacy project: not configured, never inferred from tasks.
            let legacy = project(c, None);
            verified(c, "T-1");
            assert_eq!(status(c, &legacy).unwrap()["state"], "not_configured");
            // Configured, all tasks verified, but main was never observed and nothing ran: pending, not accepted.
            let contract = two();
            let p = project(c, Some(contract.clone()));
            let contract = p.policy.acceptance.clone().unwrap();
            let s = status(c, &p).unwrap();
            assert_eq!((s["state"].as_str(), s["reason"].as_str()), (Some("pending"), Some("waiting_for_main_observation")));
            assert!(settled(c, "p").unwrap());
            // Observe main; still pending (not yet evaluated), and idempotent for the same SHA.
            assert!(observe(c, &p, "base1", "main1", &[]).unwrap().applied);
            assert!(!observe(c, &p, "base1", "main1", &[]).unwrap().applied);
            let s = status(c, &p).unwrap();
            assert_eq!(s["reason"], "not_yet_evaluated");
            let fp = s["fingerprint"].as_str().unwrap().to_string();
            assert!(runnable(c, "p", &fp).unwrap());
            let intent = intent_revision(c, "p").unwrap();
            // The automated check FAILS although every task is Verified: the project is not Accepted.
            let failing = result(&fp, "failed", &contract, "main1", &intent, json!([{"criterion":"unit","verifier":"unit-tests","type":"command","command":"cargo test","state":"failed","exit":1},{"criterion":"owner","verifier":"owner-review","type":"human","state":"pending_human"}]));
            assert!(record(c, "p", &contract, &intent, &failing).unwrap().applied);
            let s = status(c, &p).unwrap();
            assert_eq!(s["state"], "failed");
            assert_eq!(s["criteria"][0]["result"]["exit"], 1);
            assert!(!runnable(c, "p", &fp).unwrap(), "the same inputs are never checked twice");
            // Operator rerun reopens exactly those inputs once.
            assert!(request_rerun(c, &p, "wrong").is_err());
            assert!(request_rerun(c, &p, &fp).unwrap().applied);
            assert_eq!(status(c, &p).unwrap()["reason"], "rerun_requested");
            assert!(runnable(c, "p", &fp).unwrap());
            // Passing automated criteria leave the configured human criterion awaiting review.
            let passing = result(&fp, "awaiting_human", &contract, "main1", &intent, json!([{"criterion":"unit","verifier":"unit-tests","type":"command","command":"cargo test","state":"passed","exit":0},{"criterion":"owner","verifier":"owner-review","type":"human","state":"pending_human"}]));
            record(c, "p", &contract, &intent, &passing).unwrap();
            assert_eq!(status(c, &p).unwrap()["state"], "awaiting_human");
            // Approval is bound to the reviewed fingerprint, to a human criterion, and to the awaiting state.
            let ok = |crit: &str, fp: &str, d: &str| Approval { criterion: crit.into(), fingerprint: fp.into(), decision: d.into(), note: String::new() };
            assert!(approve(c, &p, &ok("unit", &fp, "approve")).is_err(), "automated criterion cannot be approved by a person");
            assert!(approve(c, &p, &ok("ghost", &fp, "approve")).is_err());
            assert!(approve(c, &p, &ok("owner", "stale-fingerprint", "approve")).is_err());
            assert!(approve(c, &p, &ok("owner", &fp, "maybe")).is_err());
            assert!(approve(c, &p, &ok("owner", &fp, "approve")).unwrap().applied);
            assert!(!approve(c, &p, &ok("owner", &fp, "approve")).unwrap().applied, "idempotent");
            assert!(approve(c, &p, &ok("owner", &fp, "reject")).is_err(), "a recorded decision is not flipped");
            let s = status(c, &p).unwrap();
            assert_eq!(s["state"], "accepted");
            assert_eq!(s["criteria"][1]["result"]["approval"]["decision"], "approve");
            // New intent makes the success stale immediately, keeps it as history, and needs a new evaluation.
            c.execute("INSERT INTO issues(id,title,status,type,project_group,created,updated,next_action,acceptance_criteria) VALUES('T-2','new ask','backlog','doc','p',1,1,'do it','[\"y\"]')", []).unwrap();
            let s = status(c, &p).unwrap();
            assert_eq!(s["state"], "pending");
            assert_eq!(s["previous"]["state"], "awaiting_human");
            assert!(s["fingerprint"] != json!(fp));
            c.execute("UPDATE issues SET status='verified' WHERE id='T-2'", []).unwrap();
            // A new composed candidate is a new fingerprint too.
            observe(c, &p, "base2", "main2", &[]).unwrap();
            assert_ne!(status(c, &p).unwrap()["fingerprint"], json!(fp));
            // A stale result computed for old intent is discarded by compare-and-write.
            let stale = result(&fp, "accepted", &contract, "main1", &intent, json!([]));
            assert!(!record(c, "p", &contract, &intent, &stale).unwrap().applied);
            assert_eq!(events(c, "p", "project.acceptance", None).unwrap().len(), 2);
            Ok(WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();
    }

    #[test]
    fn review_assets_include_acceptance_evidence_not_declared_by_executor_report() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Store::open(&dir.path().join("db")).unwrap();
        db.write(|c| {
            let p = project(c, Some(two()));
            let contract = p.policy.acceptance.clone().unwrap();
            verified(c, "T-1");
            observe(c, &p, "base1", "main1", &[]).unwrap();
            let s = status(c, &p).unwrap();
            let fp = s["fingerprint"].as_str().unwrap().to_string();
            let intent = intent_revision(c, "p").unwrap();
            let json_asset = json!({"path":"/retained/report.json","source":{"path":"artifacts/report.json","sha256":"b".repeat(64)},"head":"main1"});
            let md_asset = json!({"path":"/retained/report.md","source":{"path":"artifacts/report.md","sha256":"a".repeat(64)},"head":"main1"});
            let awaiting = result(
                &fp,
                "awaiting_human",
                &contract,
                "main1",
                &intent,
                json!([
                    {"criterion":"unit","verifier":"unit-tests","type":"command","command":"cargo test","state":"passed","exit":0,"evidence":[md_asset.clone()]},
                    {"criterion":"owner","verifier":"owner-review","type":"human","state":"pending_human","evidence":[md_asset,json_asset]}
                ]),
            );
            assert!(record(c, "p", &contract, &intent, &awaiting)
                .unwrap()
                .applied);
            let s = status(c, &p).unwrap();
            let sources: Vec<_> = s["review_assets"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|entry| entry["asset"]["source"]["path"].as_str())
                .collect();
            assert_eq!(sources, vec!["artifacts/report.md", "artifacts/report.json"]);
            Ok(WriteOutcome {
                applied: false,
                events: vec![],
            })
        })
        .unwrap();
    }

    #[test]
    fn acceptance_survives_pause_and_execution_bookkeeping_but_not_requirement_changes() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Store::open(&dir.path().join("db")).unwrap();
        db.write(|c| {
            let p = project(c, Some(two()));
            let contract = p.policy.acceptance.clone().unwrap();
            verified(c, "T-1");
            observe(c, &p, "base1", "main1", &[]).unwrap();
            let fp = status(c, &p).unwrap()["fingerprint"]
                .as_str()
                .unwrap()
                .to_string();
            let intent = intent_revision(c, "p").unwrap();
            let awaiting = result(
                &fp,
                "awaiting_human",
                &contract,
                "main1",
                &intent,
                json!([
                    {"criterion":"unit","verifier":"unit-tests","type":"command","command":"cargo test","state":"passed","exit":0},
                    {"criterion":"owner","verifier":"owner-review","type":"human","state":"pending_human"}
                ]),
            );
            record(c, "p", &contract, &intent, &awaiting).unwrap();
            approve(
                c,
                &p,
                &Approval {
                    criterion: "owner".into(),
                    fingerprint: fp.clone(),
                    decision: "approve".into(),
                    note: "looks good".into(),
                },
            )
            .unwrap();
            let accepted = status(c, &p).unwrap();
            assert_eq!(accepted["state"], "accepted");

            let mut paused = p.policy.clone();
            paused.paused = true;
            store::save(c, "p", p.revision, &paused, "operator").unwrap();
            c.execute(
                "UPDATE issues SET session='px-p',evidence='retained review asset',next_action='resume/retirement bookkeeping only' WHERE id='T-1'",
                [],
            )
            .unwrap();
            let row = bs::get_issue(c, "T-1").unwrap().unwrap();
            let mut execution = planner::execution(c, "T-1").unwrap();
            execution.stage = "verified".into();
            execution.worker = "px-p".into();
            execution.suspended = true;
            execution.input_hash = planner::input_hash(&row);
            planner::save_execution(c, &row, &execution, "project.paused").unwrap();
            let paused_project = store::get(c, "p").unwrap().unwrap();
            let still_accepted = status(c, &paused_project).unwrap();
            assert_eq!(still_accepted["state"], "accepted");
            assert_eq!(still_accepted["fingerprint"], json!(fp));

            c.execute("UPDATE issues SET desc='material new requirement' WHERE id='T-1'", [])
                .unwrap();
            let changed = status(c, &paused_project).unwrap();
            assert_eq!(changed["state"], "pending");
            assert_ne!(changed["fingerprint"], json!(fp));
            Ok(WriteOutcome {
                applied: false,
                events: vec![],
            })
        })
        .unwrap();
    }

    #[test]
    fn operational_failures_are_distinct_bounded_and_hold_until_the_operator_reruns() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Store::open(&dir.path().join("db")).unwrap();
        db.write(|c| {
            let p = project(c, Some(two()));
            let contract = p.policy.acceptance.clone().unwrap();
            verified(c, "T-1");
            observe(c, &p, "base1", "main1", &[]).unwrap();
            let fp = status(c, &p).unwrap()["fingerprint"]
                .as_str()
                .unwrap()
                .to_string();
            let intent = intent_revision(c, "p").unwrap();
            let op = result(
                &fp,
                "operational_failure",
                &contract,
                "main1",
                &intent,
                json!([{"criterion":"unit","state":"operational","output":"timed out"}]),
            );
            for attempt in 1..=MAX_OPERATIONAL_ATTEMPTS {
                assert!(runnable(c, "p", &fp).unwrap(), "attempt {attempt}");
                record(c, "p", &contract, &intent, &op).unwrap();
            }
            assert!(!runnable(c, "p", &fp).unwrap(), "no endless retry");
            let s = status(c, &p).unwrap();
            assert_eq!(
                (s["state"].as_str(), s["operational_attempts"].as_u64()),
                (
                    Some("operational_failure"),
                    Some(MAX_OPERATIONAL_ATTEMPTS as u64)
                )
            );
            request_rerun(c, &p, &fp).unwrap();
            assert!(runnable(c, "p", &fp).unwrap());
            // An empty project is never settled, so it can never be accepted vacuously.
            c.execute("DELETE FROM issues", []).unwrap();
            assert!(!settled(c, "p").unwrap());
            assert_eq!(status(c, &p).unwrap()["reason"], "waiting_for_tasks");
            Ok(WriteOutcome {
                applied: false,
                events: vec![],
            })
        })
        .unwrap();
    }
}
