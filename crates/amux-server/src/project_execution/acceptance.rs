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
    project::{phase, AcceptanceContract, ContractVerifier, Phase},
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
/// until the operator asks for a rerun. Semantic outcomes never retry on their own.
const MAX_OPERATIONAL_ATTEMPTS: usize = 2;
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
        for (task, head) in heads {
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
            workspace::git(&candidate, &["merge", "--no-ff", "--no-edit", head])
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "task {task} cannot be composed into the project candidate: {error}"
                    )
                })?;
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
                ContractVerifier::Execution { id, command, timeout_secs, receipt, required_stages } => json!({"type":"execution","id":id,"command":command,"timeout_secs":timeout_secs,"receipt":receipt,"required_stages":required_stages}),
                ContractVerifier::Human { id, instructions } => json!({"type":"human","id":id,"instructions":instructions}),
            };
            json!({"id":c.id,"requirement":c.requirement,"verifier":verifier,"evidence_required":c.evidence,"result":outcome})
        })
        .collect()
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
    let observed = observed_main(conn, &p.name)?;
    let candidate = observed.as_ref().and_then(|o| {
        o["candidate"]
            .as_str()
            .or_else(|| o["main"].as_str())
            .map(String::from)
    });
    let base_main = observed
        .as_ref()
        .and_then(|o| o["base_main"].as_str().map(String::from));
    let heads = observed
        .as_ref()
        .and_then(|o| o["heads"].as_array().cloned())
        .unwrap_or_default();
    let mut view = json!({"measured":true,"n_considered":contract.criteria.len(),"contract_revision":contract.revision,"intent":intent,"main":base_main,"candidate":candidate,"task_heads":heads,
        "review_assets":review_assets(conn,&p.name,None)?,
        "criteria":criteria_view(contract, None, &HashMap::new())});
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
        .filter(|(_, e)| e["state"] == "operational_failure")
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
    view["finished"] = result["finished"].clone();
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
    if live
        .iter()
        .any(|(_, e)| e["state"] != "operational_failure")
    {
        return Ok(false);
    }
    Ok(live.len() < MAX_OPERATIONAL_ATTEMPTS)
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
    Ok(changed(project, state))
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
        for evidence in &criterion.evidence {
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
                ContractVerifier::Execution { receipt, required_stages, .. } => format!(
                    "; runtime proof: a fresh candidate-bound receipt at {receipt} with passed stages {}",
                    required_stages.join(", ")
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
    generated_receipt: Option<&str>,
) -> anyhow::Result<Vec<Value>> {
    if paths.is_empty() {
        return Ok(vec![]);
    }
    let mut assets = Vec::new();
    for path in paths
        .iter()
        .filter(|path| generated_receipt != Some(path.as_str()))
    {
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
    if let Some(path) = generated_receipt {
        retained.push(
            super::assets::retain_generated_json(home, std::path::Path::new(candidate), main, path)
                .await?,
        );
    }
    Ok(retained.iter().map(|r| json!(r)).collect())
}

fn validate_execution_receipt(
    path: &std::path::Path,
    run_id: &str,
    main: &str,
    invocation_started: f64,
    invocation_finished: f64,
    required_stages: &[String],
) -> anyhow::Result<Value> {
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
                ContractVerifier::Execution { id, command, timeout_secs, receipt, required_stages } => (id, command, timeout_secs, Some((receipt, required_stages))),
                ContractVerifier::Human { .. } => {
                    match retain_evidence(&home, &candidate, main, &c.evidence, None).await {
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
            if let Some((receipt, _)) = execution {
                if std::path::Path::new(&candidate).join(receipt).exists() {
                    tracing::warn!(project=%p.name, criterion=%c.id, receipt, measured=true, n_considered=1, verdict="project.execution_receipt_preexisting", "execution verifier refused a receipt already present before this invocation");
                    results.push(with("failed", json!({"receipt_error":"execution receipt already existed before this invocation; historical proof cannot satisfy a runtime gate"})));
                    continue;
                }
            }
            let timeout = Duration::from_secs(timeout_secs.unwrap_or(p.policy.verification_timeout_secs));
            let mut cmd = tokio::process::Command::new("sh");
            let run_id = format!("{}-{}-{}", p.name, std::process::id(), chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default());
            cmd.args(["-c", command])
                .current_dir(&candidate)
                .env("AMUX_SESSION", format!("acceptance-{}", p.name))
                .env("AMUX_ACCEPTANCE_RUN_ID", &run_id)
                .env("AMUX_ACCEPTANCE_MAIN", main)
                .env("AMUX_ACCEPTANCE_STARTED_AT", format!("{started:.6}"))
                .env("AMUX_ACCEPTANCE_CANDIDATE", &candidate);
            if let Some((receipt, _)) = execution {
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
                        results.push(with("failed", json!({"exit":status.code(),"output":tail(&output,4000),"elapsed_ms":elapsed_ms})));
                    } else {
                        let finished = crate::config::now_f64();
                        let receipt = execution.map(|(path, stages)| {
                            validate_execution_receipt(
                                &std::path::Path::new(&candidate).join(path),
                                &run_id,
                                main,
                                started,
                                finished,
                                stages,
                            )
                        });
                        let retained = retain_evidence(
                            &home,
                            &candidate,
                            main,
                            &c.evidence,
                            execution.map(|(path, _)| path.as_str()),
                        )
                        .await;
                        match (receipt, retained) {
                            (Some(Err(error)), evidence) => {
                                tracing::warn!(project=%p.name, criterion=%c.id, %error, measured=true, n_considered=1, verdict="project.execution_receipt_rejected", "runtime acceptance proof rejected");
                                results.push(with("failed", json!({"exit":0,"output":tail(&output,4000),"elapsed_ms":elapsed_ms,"receipt_error":error.to_string(),"evidence":evidence.unwrap_or_default()})));
                            }
                            (Some(Ok(receipt)), Ok(evidence)) => results.push(with("passed", json!({"exit":0,"output":tail(&output,4000),"elapsed_ms":elapsed_ms,"evidence":evidence,"receipt":receipt}))),
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
    let (settled, intent, heads, seen) = {
        let c = state.store.read()?;
        (
            settled(&c, &name)?,
            intent_revision(&c, &name)?,
            verified_heads(&c, &name)?,
            observed_main(&c, &name)?,
        )
    };
    if !settled {
        return Ok(());
    }
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
                    let candidate = assemble_candidate(p, &base_main, &heads).await?;
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
    workspace::git(
        &p.policy.repository,
        &["push", "origin", &format!("{candidate}:refs/heads/main")],
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
        rejected(failed_stage, "api-lifecycle did not pass");
        let mut prose_only = base;
        prose_only["stages"][1]["evidence"] = json!([]);
        rejected(prose_only, "no machine-readable evidence");
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
path.write_text(json.dumps({
  "schema": "amux.execution_receipt.v1",
  "run_id": os.environ["AMUX_ACCEPTANCE_RUN_ID"],
  "candidate_sha": os.environ["AMUX_ACCEPTANCE_MAIN"],
  "state": "passed",
  "started_at": now,
  "finished_at": now,
  "subject": {"kind": "docker_image", "id": "sha256:fresh"},
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
            "verifier":{"type":"execution","id":"fresh-run","command":"python3 scripts/prove.py","receipt":"artifacts/execution.json","required_stages":["api-lifecycle"]},
            "evidence":["artifacts/execution.json"]
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
        assert_eq!(
            result["results"][0]["receipt"]["subject"]["id"],
            "sha256:fresh"
        );
        let retained = result["results"][0]["evidence"][0]["path"]
            .as_str()
            .unwrap();
        assert!(std::path::Path::new(retained).is_file(), "{retained}");
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
    fn result(
        fp: &str,
        state: &str,
        contract: &AcceptanceContract,
        main: &str,
        intent: &str,
        results: Value,
    ) -> Value {
        json!({"fingerprint":fp,"contract_revision":contract.revision,"main":main,"intent":intent,"state":state,"finished":1.0,"results":results})
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
                (Some("operational_failure"), Some(2))
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
