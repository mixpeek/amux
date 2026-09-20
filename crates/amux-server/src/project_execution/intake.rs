//! Durable receipts and bounded interpretation reuse the command lifecycle.
use super::store;
use crate::{
    api::{board_lifecycle, mdai, AppState},
    db::{PendingEvent, WriteOutcome},
};
use amux_core::revision::{EntityType, MutationKind};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::sync::Arc;

pub fn receive(
    conn: &Connection,
    project: &str,
    key: &str,
    text: &str,
) -> anyhow::Result<(i64, WriteOutcome)> {
    anyhow::ensure!(
        !key.is_empty() && key.len() <= 160,
        "a client idempotency key is required"
    );
    anyhow::ensure!(
        !text.trim().is_empty() && text.len() <= 100_000,
        "command must contain 1..100000 bytes"
    );
    anyhow::ensure!(store::get(conn, project)?.is_some(), "project not found");
    let prior:Option<(i64,String)>=conn.query_row("SELECT id,text FROM cmd_history WHERE session='project:'||project_group AND type='user' AND project_group=?1 AND json_extract(client_meta,'$.idempotency_key')=?2",params![project,key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((id, original)) = prior {
        anyhow::ensure!(
            original == text,
            "idempotency key already belongs to a different command"
        );
        return Ok((
            id,
            WriteOutcome {
                applied: false,
                events: vec![],
            },
        ));
    }
    conn.execute("INSERT INTO cmd_history(text,type,session,ts,origin,delivery,capture_pending,project_group,client_meta) VALUES(?1,'user',?2,?3,'operator','board',1,?4,?5)",params![text,format!("project:{project}"),chrono::Utc::now().timestamp(),project,json!({"idempotency_key":key}).to_string()])?;
    let id = conn.last_insert_rowid();
    Ok((
        id,
        WriteOutcome {
            applied: true,
            events: vec![PendingEvent {
                entity_type: EntityType::Message,
                entity_id: format!("MSG-{id}"),
                mutation: MutationKind::Created,
                payload: Some(json!({"project_group":project,"id":id})),
            }],
        },
    ))
}

pub fn receipts(conn: &Connection, project: &str) -> anyhow::Result<Vec<Value>> {
    let project_policy =
        store::get(conn, project)?.ok_or_else(|| anyhow::anyhow!("project not found"))?;
    let budget_wait = super::usage::waiting(conn, &project_policy)?;
    let mut q=conn.prepare("SELECT c.id,c.text,c.capture_pending,c.intake_attempts,c.intake_result,c.card_id,c.ts,p.intake_attempts,p.intake_result,c.client_meta,c.intake_retry_at,p.client_meta FROM cmd_history c LEFT JOIN cmd_history p ON p.id=json_extract(c.intake_result,'$.waiting_on') AND p.project_group=c.project_group AND p.capture_pending!=0 WHERE c.session='project:'||c.project_group AND c.type='user' AND c.project_group=?1 ORDER BY c.id DESC LIMIT 100")?;
    let rows=q.query_map([project],|r| {
        let raw:Option<String>=r.get(4)?;
        let mut result:Value=raw.and_then(|s|serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
        let pending:bool=r.get(2)?;let attempts:i64=r.get(3)?;let waiting_attempts=r.get::<_,Option<i64>>(7)?.unwrap_or(attempts);
        if result.get("error").is_none() {
            if let Some(parent) = r.get::<_,Option<String>>(8)?.and_then(|s|serde_json::from_str::<Value>(&s).ok()) {
                if let Some(error) = parent.get("error") { result["error"] = error.clone(); }
            }
        }
        let meta:Value=r.get::<_,Option<String>>(9)?.and_then(|s|serde_json::from_str(&s).ok()).unwrap_or(json!({}));
        let revision=meta["intake_retries"].as_array().map_or(0,Vec::len);
        let parent_meta:Value=r.get::<_,Option<String>>(11)?.and_then(|s|serde_json::from_str(&s).ok()).unwrap_or(json!({}));
        let limit=2+if result.get("waiting_on").is_some(){parent_meta["intake_retries"].as_array().map_or(0,Vec::len)}else{revision} as i64;
        let retry_available=pending && result.get("waiting_on").is_none() && attempts>=limit && result["state"]!="prepared" && result.get("error").is_some() && r.get::<_,i64>(10)?<=chrono::Utc::now().timestamp();
        let quota_wait = result.get("error").and_then(Value::as_str).is_some_and(|error| error.contains("provider quota wait:"));
        Ok(json!({"id":r.get::<_,i64>(0)?,"text":r.get::<_,String>(1)?,"pending":pending,"attempts":attempts,"attempt_limit":limit,"retry_revision":revision,"retry_available":retry_available,"retry_history":meta["intake_retries"],"result":result,"card_id":r.get::<_,Option<String>>(5)?,"ts":r.get::<_,f64>(6)?,"waiting_reason":if pending && budget_wait.is_some() {budget_wait.as_deref()}else if pending && quota_wait {Some("provider_quota_wait")}else if pending && waiting_attempts>=limit {Some("intake_attempts_exhausted")}else{None}}))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(crate) async fn interpret(
    state: &AppState,
    id: i64,
    project: &str,
    client: Arc<dyn mdai::ModelClient>,
) -> anyhow::Result<()> {
    {
        let c = state.store.read()?;
        let genuine:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM cmd_history WHERE id=?1 AND project_group=?2 AND session='project:'||project_group AND type='user')",params![id,project],|r|r.get(0))?;
        anyhow::ensure!(
            genuine,
            "within-task steering is not a project outcome receipt"
        );
    }
    let result =
        board_lifecycle::capture_inner(state, id, &format!("project:{project}"), client).await;
    if let Err(error) = &result {
        tracing::warn!(project,message_id=id,%error,measured=true,n_considered=1,verdict="project_intake_waiting","command preserved without creating raw executable tasks");
        let error = error.to_string();
        state.store.write_async(move|c| {
            c.execute("UPDATE cmd_history SET intake_retry_at=?3,intake_result=CASE WHEN json_valid(intake_result) THEN json_set(intake_result,'$.error',?2) ELSE json_object('state','pending','error',?2) END WHERE id=?1 AND capture_pending!=0",params![id,error,chrono::Utc::now().timestamp()+30])?;
            Ok(WriteOutcome{applied:true,events:vec![]})
        }).await?;
    }
    result
}

fn pending_receipts(conn: &Connection, now: i64) -> rusqlite::Result<Vec<(i64, String)>> {
    // Exhausted malformed responses and duplicates waiting on them must not
    // monopolize the bounded recovery batch and starve newer accepted commands.
    let mut q=conn.prepare("SELECT c.id,c.project_group FROM cmd_history c JOIN group_config g ON g.name=c.project_group WHERE c.session='project:'||c.project_group AND c.type='user' AND c.capture_pending!=0 AND c.intake_retry_at<=?1 AND json_extract(g.execution_policy,'$.paused')!=1 AND CASE WHEN json_extract(c.intake_result,'$.waiting_on') IS NOT NULL THEN EXISTS(SELECT 1 FROM cmd_history p WHERE p.id=json_extract(c.intake_result,'$.waiting_on') AND p.capture_pending=0) ELSE c.intake_attempts<2+coalesce(json_array_length(c.client_meta,'$.intake_retries'),0) OR json_extract(c.intake_result,'$.state')='prepared' OR (json_extract(c.intake_result,'$.state')='received' AND json_extract(c.intake_result,'$.error') IS NULL) END ORDER BY c.id LIMIT 2")?;
    let rows = q
        .query_map([now], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect();
    rows
}

/// The periodic caller only discovers durable receipts. It never prompts a
/// model for unchanged idle boards, and never holds up legacy dispatch.
pub(crate) async fn recover_pending(state: &AppState) {
    let Some(_) = crate::api::board_intake::model_client() else {
        return;
    };
    let client: Arc<dyn mdai::ModelClient> = Arc::new(mdai::ProjectIntakeModel);
    let pending = {
        let Ok(conn) = state.store.read() else { return };
        let Ok(rows) = pending_receipts(&conn, chrono::Utc::now().timestamp()) else {
            return;
        };
        rows
    };
    for (id, project) in pending {
        let state = state.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let _ = interpret(&state, id, &project, client).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{board_store as bs, Store};
    struct Fake {
        calls: std::sync::atomic::AtomicUsize,
        response: String,
    }
    impl mdai::ModelClient for Fake {
        fn complete(&self, _: &str, _: &str) -> Result<String, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.response.clone())
        }
    }
    fn fixture() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Store::open(&dir.path().join("db")).unwrap());
        db.write(|c|store::save(c,"sample",0,&serde_json::from_value(json!({"repository":"/tmp/project-repo","coordinator":{"provider":"claude","model":"haiku"},"executor":{"provider":"claude","model":"sonnet"},"verify_command":"./verify.sh"})).unwrap(),"test").map_err(store::sql_error)).unwrap();
        (
            dir,
            AppState {
                store: db,
                started: std::time::Instant::now(),
                build_hash: "test".into(),
                auth_token: None,
                reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            },
        )
    }
    #[tokio::test]
    async fn project_steering_never_counts_or_interprets_as_an_outcome() {
        let (_dir, state) = fixture();
        let id = receipt(&state, "original");
        state.store.write(move|c| {
            c.execute("INSERT INTO issues(id,title,status,project_group,created,updated) VALUES('A','Outcome','verified','sample',1,1)",[])?;
            c.execute("UPDATE cmd_history SET card_id='A',capture_pending=0 WHERE id=?1",[id])?;
            for n in 100..110 {c.execute("INSERT INTO cmd_history(id,text,type,session,ts,project_group,card_id,capture_pending) VALUES(?1,'steer','steering','executor',1,'sample','A',1)",[n])?;}
            Ok(WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
        let fake = Arc::new(Fake {
            calls: std::sync::atomic::AtomicUsize::new(0),
            response: "must not run".into(),
        });
        for id in 100..110 {
            assert!(interpret(&state, id, "sample", fake.clone()).await.is_err());
        }
        let c = state.store.read().unwrap();
        assert!(pending_receipts(&c, i64::MAX).unwrap().is_empty());
        assert_eq!(receipts(&c, "sample").unwrap().len(), 1);
        let usage = super::super::usage::summary(&c, "sample").unwrap();
        assert_eq!(usage["commands"], 1);
        assert_eq!(usage["requested_outcomes"], 1);
        assert_eq!(usage["verified_outcomes"], 1);
        assert_eq!(usage["intake_calls"], 0);
        assert_eq!(fake.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
    fn receipt(state: &AppState, key: &str) -> i64 {
        let key = key.to_string();
        let id = Arc::new(std::sync::Mutex::new(0));
        let got = id.clone();
        state
            .store
            .write(move |c| {
                let (id, out) = receive(
                    c,
                    "sample",
                    &key,
                    "Create a report summarizing the repository modules",
                )
                .map_err(store::sql_error)?;
                *got.lock().unwrap() = id;
                Ok(out)
            })
            .unwrap();
        let result = *id.lock().unwrap();
        result
    }
    #[tokio::test]
    async fn project_rejected_provider_response_retains_paid_usage() {
        struct Rejected;
        impl mdai::ModelClient for Rejected {
            fn complete(&self, _: &str, _: &str) -> Result<String, String> {
                unreachable!()
            }
            fn complete_for_provider(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<mdai::ModelCompletion, mdai::ModelFailure> {
                Err(mdai::ModelFailure {
                    message: "unexpected tool item".into(),
                    usage: Some(json!({"input_tokens":41,"output_tokens":9})),
                })
            }
        }
        let (_dir, state) = fixture();
        let id = receipt(&state, "paid-rejection");
        assert!(interpret(&state, id, "sample", Arc::new(Rejected))
            .await
            .is_err());
        let c = state.store.read().unwrap();
        let row = receipts(&c, "sample").unwrap().remove(0);
        assert_eq!(row["attempts"], 1);
        assert_eq!(row["pending"], true);
        assert_eq!(
            row["result"]["attempt_errors"][0]["usage"]["input_tokens"],
            41
        );
        let usage = super::super::usage::summary(&c, "sample").unwrap();
        assert_eq!(usage["tokens"], 50);
        assert_eq!(usage["intake_calls_measured"], 1);
        assert!(bs::project_issues(&c, "sample").unwrap().is_empty());
    }
    #[test]
    fn project_intake_retry_is_scoped_idempotent_bounded_and_retains_history() {
        use super::super::intake_retry::{grant, Request};
        let (_dir, state) = fixture();
        let id = receipt(&state, "retry");
        state.store.write(move |c| {
            let prior=json!({"state":"pending","error":"provider failure","telemetry":{"attempt_usage":[null,{"input_tokens":17,"output_tokens":3}]}});
            c.execute("UPDATE cmd_history SET intake_attempts=2,intake_result=?2 WHERE id=?1",params![id,prior.to_string()])?;
            let request=Request{idempotency_key:"click-1".into(),expect_attempts:2,expect_revision:0};
            assert!(grant(c,"other",id,&request,100).is_err());
            assert!(receipts(c,"sample").unwrap()[0]["retry_available"].as_bool().unwrap());
            assert!(grant(c,"sample",id,&request,100).unwrap().applied);
            assert!(!grant(c,"sample",id,&request,100).unwrap().applied);
            let stale=Request{idempotency_key:"another-tab".into(),..request.clone()};
            assert!(grant(c,"sample",id,&stale,100).is_err());
            let row=receipts(c,"sample").unwrap().remove(0);
            assert_eq!(row["attempts"],2);assert_eq!(row["attempt_limit"],3);
            assert_eq!(row["result"],prior);assert_eq!(row["retry_history"][0]["previous_result"],prior);
            assert_eq!(row["retry_available"],false);
            assert_eq!(pending_receipts(c,100)?,vec![(id,"sample".into())]);
            // A grant does not unpause a project.
            c.execute("UPDATE group_config SET execution_policy=json_set(execution_policy,'$.paused',1) WHERE name='sample'",[])?;
            assert!(pending_receipts(c,100)?.is_empty());
            c.execute("UPDATE group_config SET execution_policy=json_set(execution_policy,'$.paused',0) WHERE name='sample'",[])?;
            // One consumed grant cannot run endlessly or reset the original counter.
            c.execute("UPDATE cmd_history SET intake_attempts=3,intake_retry_at=150 WHERE id=?1",[id])?;
            let next=Request{idempotency_key:"click-2".into(),expect_attempts:3,expect_revision:1};
            assert!(grant(c,"sample",id,&next,100).is_err());
            assert!(pending_receipts(c,200)?.is_empty());
            assert!(grant(c,"sample",id,&next,200).unwrap().applied);
            assert!(!grant(c,"sample",id,&request,200).unwrap().applied);
            c.execute("UPDATE cmd_history SET capture_pending=0 WHERE id=?1",[id])?;
            assert!(!grant(c,"sample",id,&next,200).unwrap().applied);
            assert!(grant(c,"sample",id,&Request{idempotency_key:"new-after-completion".into(),expect_attempts:3,expect_revision:2},200).is_err());
            assert_eq!(c.query_row("SELECT count(*) FROM issues",[],|r|r.get::<_,i64>(0))?,0);
            Ok(WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
    }
    #[test]
    fn project_quota_wait_is_not_a_request_for_clarification() {
        let (_dir, state) = fixture();
        let original = receipt(&state, "limited");
        let duplicate = receipt(&state, "duplicate-limit");
        state.store.write(move |c| {
            c.execute("UPDATE cmd_history SET intake_attempts=2,intake_result=?2 WHERE id=?1",params![original,json!({"state":"pending","error":"provider quota wait: You've hit your weekly limit · resets Sep 23 at 11am"}).to_string()])?;
            c.execute("UPDATE cmd_history SET intake_result=?2 WHERE id=?1",params![duplicate,json!({"state":"waiting","waiting_on":original}).to_string()])?;
            for row in receipts(c, "sample").unwrap() {
                assert_eq!(row["waiting_reason"], "provider_quota_wait");
                assert!(row["result"]["error"].as_str().unwrap().contains("Sep 23"));
            }
            assert!(pending_receipts(c,chrono::Utc::now().timestamp())?.is_empty());
            Ok(WriteOutcome {applied:false,events:vec![]})
        }).unwrap();
    }

    #[test]
    fn project_exhausted_intake_and_its_duplicates_do_not_starve_new_commands() {
        let (_dir, state) = fixture();
        let original = receipt(&state, "bad");
        let duplicate = receipt(&state, "duplicate");
        let next = receipt(&state, "new");
        state
            .store
            .write(move |c| {
                c.execute(
                    "UPDATE cmd_history SET intake_attempts=2,intake_result=?2 WHERE id=?1",
                    params![
                        original,
                        json!({"state":"received","error":"malformed JSON"}).to_string()
                    ],
                )?;
                c.execute(
                    "UPDATE cmd_history SET intake_result=?2 WHERE id=?1",
                    params![
                        duplicate,
                        json!({"state":"waiting","waiting_on":original}).to_string()
                    ],
                )?;
                assert_eq!(
                    pending_receipts(c, chrono::Utc::now().timestamp())?,
                    vec![(next, "sample".into())]
                );
                let receipts = receipts(c, "sample").unwrap();
                assert_eq!(
                    receipts.iter().find(|r| r["id"] == duplicate).unwrap()["waiting_reason"],
                    "intake_attempts_exhausted"
                );
                c.execute(
                    "UPDATE cmd_history SET capture_pending=0 WHERE id=?1",
                    [original],
                )?;
                assert_eq!(
                    pending_receipts(c, chrono::Utc::now().timestamp())?,
                    vec![(duplicate, "sample".into()), (next, "sample".into())]
                );
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .unwrap();
    }
    #[test]
    fn project_provider_routing_reaches_real_cli_callsite_once_per_attempt() {
        use std::os::unix::fs::PermissionsExt;
        let scope = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(scope.path());
        struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                for (key, value) in &self.0 {
                    match value {
                        Some(v) => std::env::set_var(key, v),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
        let _restore = Restore(
            ["AMUX_HELPER_CLI", "AMUX_CODEX_HELPER_CLI"]
                .into_iter()
                .map(|k| (k, std::env::var_os(k)))
                .collect(),
        );
        let plan = json!({"kind":"tasks","reason":"one report requested","confidence":0.99,"tasks":[{"key":"a","title":"Repository module report","description":"Describe the repository modules","type":"doc","action":"create","existing_id":null,"next_action":"Inspect modules and write report","acceptance_criteria":["Report lists each module"],"needs":[],"dependency_reason":""}]}).to_string();
        let calls = scope.path().join("calls");
        for (provider, env_key) in [
            ("claude", "AMUX_HELPER_CLI"),
            ("codex", "AMUX_CODEX_HELPER_CLI"),
        ] {
            let events = if provider == "claude" {
                json!({"type":"result","result":plan,"usage":{"input_tokens":20,"output_tokens":5}})
                    .to_string()
            } else {
                format!(
                    "{}\n{}",
                    json!({"type":"item.completed","item":{"type":"agent_message","text":plan}}),
                    json!({"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":5}})
                )
            };
            let cli = scope.path().join(provider);
            std::fs::write(&cli,format!("#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{provider}' >> '{}'\nprintf '%s\\n' \"$@\" >> '{}.args'\ncat <<'EVENTS'\n{events}\nEVENTS\n",calls.display(),cli.display())).unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::env::set_var(env_key, cli);
        }
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Deliberately misleading model names: routing is exclusively policy-driven.
            for (provider, model) in [
                ("codex", "claude-looking-custom"),
                ("claude", "gpt-looking-custom"),
            ] {
                let (_dir, state) = fixture();
                state
                    .store
                    .write(move |c| {
                        let mut p = store::get(c, "sample").unwrap().unwrap();
                        p.policy.coordinator.provider = provider.into();
                        p.policy.coordinator.model = model.into();
                        store::save(c, "sample", p.revision, &p.policy, "test")
                            .map_err(store::sql_error)
                    })
                    .unwrap();
                let id = receipt(&state, "routing");
                for _ in 0..2 {
                    interpret(&state, id, "sample", Arc::new(mdai::ProjectIntakeModel))
                        .await
                        .unwrap();
                }
                let c = state.store.read().unwrap();
                let attempts: i64 = c
                    .query_row(
                        "SELECT intake_attempts FROM cmd_history WHERE id=?1",
                        [id],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(attempts, 1);
                let raw: String = c
                    .query_row(
                        "SELECT intake_result FROM cmd_history WHERE id=?1",
                        [id],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert!(raw.contains(provider) && raw.contains(model), "{raw}");
                let usage = super::super::usage::summary(&c, "sample").unwrap();
                assert_eq!(usage["tokens"], if provider == "codex" { 105 } else { 25 });
                assert!(usage["estimated_cost_usd"].is_null());
                assert_eq!(bs::project_issues(&c, "sample").unwrap().len(), 1);
            }
        });
        assert_eq!(std::fs::read_to_string(calls).unwrap(), "codex\nclaude\n");
        let args = std::fs::read_to_string(scope.path().join("codex.args")).unwrap();
        assert!(args.lines().any(|s| s == "exec"));
        assert!(args.lines().any(|s| s == "--ignore-user-config"));
        assert!(args.lines().any(|s| s == "claude-looking-custom"));
        // A nonzero process can put quota details after a large JSONL prelude.
        // Preserve that detail and one recorded attempt, without falling back.
        let codex = scope.path().join("codex");
        std::fs::write(&codex,format!("#!/bin/sh\ncat >/dev/null\nprintf 'codex-error\\n' >> '{}'\ncat <<'EVENTS'\n{{\"type\":\"thread.started\",\"thread_id\":\"{}\"}}\n{{\"type\":\"turn.failed\",\"error\":{{\"message\":\"usage limit resets tomorrow\"}}}}\nEVENTS\nexit 7\n",scope.path().join("calls").display(),"x".repeat(2000))).unwrap();
        rt.block_on(async {
            let (_dir, state) = fixture();
            state
                .store
                .write(|c| {
                    let mut p = store::get(c, "sample").unwrap().unwrap();
                    p.policy.coordinator.provider = "codex".into();
                    store::save(c, "sample", p.revision, &p.policy, "test")
                        .map_err(store::sql_error)
                })
                .unwrap();
            let id = receipt(&state, "quota");
            let error = interpret(&state, id, "sample", Arc::new(mdai::ProjectIntakeModel))
                .await
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("provider quota wait: usage limit resets tomorrow"),
                "{error}"
            );
            interpret(&state, id, "sample", Arc::new(mdai::ProjectIntakeModel))
                .await
                .unwrap();
            let c = state.store.read().unwrap();
            let rows = receipts(&c, "sample").unwrap();
            assert_eq!(rows[0]["attempts"], 1);
            assert_eq!(rows[0]["waiting_reason"], "provider_quota_wait");
            assert!(bs::project_issues(&c, "sample").unwrap().is_empty());
        });
        assert_eq!(
            std::fs::read_to_string(scope.path().join("calls")).unwrap(),
            "codex\nclaude\ncodex-error\n"
        );
    }

    #[tokio::test]
    async fn project_intake_retries_preserve_receipt_and_reuse_one_interpretation() {
        let (_dir, state) = fixture();
        let id = receipt(&state, "one");
        assert_eq!(receipt(&state, "one"), id);
        let fake=Arc::new(Fake{calls:Default::default(),response:json!({"kind":"tasks","reason":"one report requested","confidence":0.99,"tasks":[{"key":"a","title":"Repository module report","description":"Describe the repository modules","type":"doc","action":"create","existing_id":null,"next_action":"Inspect modules and write report","acceptance_criteria":["Report lists each module"],"needs":[],"dependency_reason":""}]}).to_string()});
        interpret(&state, id, "sample", fake.clone()).await.unwrap();
        interpret(&state, id, "sample", fake.clone()).await.unwrap();
        let duplicate = receipt(&state, "two");
        interpret(&state, duplicate, "sample", fake.clone())
            .await
            .unwrap();
        assert_eq!(fake.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let conn = state.store.read().unwrap();
        let rows = bs::project_issues(&conn, "sample").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session, None);
        assert_eq!(rows[0].status, "backlog");
        assert_eq!(receipts(&conn, "sample").unwrap().len(), 2);
        assert!(receive(&conn, "sample", "one", "different command").is_err());
    }
    #[tokio::test]
    async fn project_refinement_reopens_same_verified_task_and_invalidates_old_execution() {
        let (_dir, state) = fixture();
        let id = receipt(&state, "original");
        let response = |existing: Option<String>| {
            json!({"kind":"tasks","reason":"requested report","confidence":0.99,"tasks":[{"key":"a","title":"Repository module report","description":"Describe the repository modules","type":"doc","action":if existing.is_some(){"verify"}else{"create"},"existing_id":existing,"next_action":"Inspect modules and write report","acceptance_criteria":["Report lists each module"],"needs":[],"dependency_reason":""}]}).to_string()
        };
        interpret(
            &state,
            id,
            "sample",
            Arc::new(Fake {
                calls: Default::default(),
                response: response(None),
            }),
        )
        .await
        .unwrap();
        let task = bs::project_issues(&state.store.read().unwrap(), "sample").unwrap()[0]
            .id
            .clone();
        let task2 = task.clone();
        state.store.write(move|c|{
            let row=bs::get_issue(c,&task2)?.unwrap();
            c.execute("UPDATE issues SET status='verified',execution_state=?2 WHERE id=?1",params![task2,json!({"stage":"verified","attempt":1,"generation":1,"input_hash":super::super::planner::input_hash(&row),"worker":"retired-executor","delivery_id":"old","waiting":null,"report":null,"usage":null,"observed_at":1}).to_string()])?;
            let (_,out)=receive(c,"sample","refinement","Verify the repository report against current modules").map_err(store::sql_error)?;
            Ok(out)
        }).unwrap();
        let new_id = receipts(&state.store.read().unwrap(), "sample").unwrap()[0]["id"]
            .as_i64()
            .unwrap();
        interpret(
            &state,
            new_id,
            "sample",
            Arc::new(Fake {
                calls: Default::default(),
                response: response(Some(task.clone())),
            }),
        )
        .await
        .unwrap();
        let c = state.store.read().unwrap();
        let rows = bs::project_issues(&c, "sample").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, task);
        assert_eq!(rows[0].status, "backlog");
        let e = super::super::planner::execution(&c, &task).unwrap();
        assert!(e.stage.is_empty());
        assert!(e.report.is_none());
        assert_eq!(e.generation, 1);
        assert_eq!(
            c.query_row(
                "SELECT typeof(ts) FROM cmd_history WHERE id=?1",
                [new_id],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "integer"
        );
    }
    #[tokio::test]
    async fn project_budget_holds_intake_without_another_model_call() {
        let (_dir, state) = fixture();
        let id = receipt(&state, "spent");
        state.store.write(move|c| {
            let mut p=store::get(c,"sample").unwrap().unwrap();p.policy.token_budget=Some(100);
            store::save(c,"sample",p.revision,&p.policy,"test").map_err(store::sql_error)?;
            c.execute("UPDATE cmd_history SET capture_pending=0,intake_attempts=1,intake_result=?2 WHERE id=?1",params![id,json!({"telemetry":{"attempt_usage":[{"usage":{"input_tokens":40,"output_tokens":80}}]}}).to_string()])?;
            Ok(WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
        let new = receipt(&state, "new");
        let fake = Arc::new(Fake {
            calls: Default::default(),
            response: "never called".into(),
        });
        assert!(interpret(&state, new, "sample", fake.clone())
            .await
            .is_err());
        assert_eq!(fake.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            receipts(&state.store.read().unwrap(), "sample").unwrap()[0]["waiting_reason"],
            "token_budget_reached"
        );
    }
    #[tokio::test]
    async fn project_intake_malformed_output_stops_after_two_calls_without_junk_tasks() {
        let (_dir, state) = fixture();
        let id = receipt(&state, "invalid");
        let fake = Arc::new(Fake {
            calls: Default::default(),
            response: "not json".into(),
        });
        for _ in 0..4 {
            let _ = interpret(&state, id, "sample", fake.clone()).await;
            state
                .store
                .write(move |c| {
                    c.execute("UPDATE cmd_history SET intake_retry_at=0 WHERE id=?1", [id])?;
                    Ok(WriteOutcome {
                        applied: true,
                        events: vec![],
                    })
                })
                .unwrap();
        }
        assert_eq!(fake.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        state
            .store
            .write(move |c| {
                super::super::intake_retry::grant(
                    c,
                    "sample",
                    id,
                    &super::super::intake_retry::Request {
                        idempotency_key: "explicit-retry".into(),
                        expect_attempts: 2,
                        expect_revision: 0,
                    },
                    chrono::Utc::now().timestamp(),
                )
                .map_err(store::sql_error)
            })
            .unwrap();
        for _ in 0..3 {
            let _ = interpret(&state, id, "sample", fake.clone()).await;
            state
                .store
                .write(move |c| {
                    c.execute("UPDATE cmd_history SET intake_retry_at=0 WHERE id=?1", [id])?;
                    Ok(WriteOutcome {
                        applied: true,
                        events: vec![],
                    })
                })
                .unwrap();
        }
        assert_eq!(fake.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert!(bs::project_issues(&state.store.read().unwrap(), "sample")
            .unwrap()
            .is_empty());
        assert_eq!(
            receipts(&state.store.read().unwrap(), "sample").unwrap()[0]["waiting_reason"],
            "intake_attempts_exhausted"
        );
    }
}
