use crate::db::{board_store as bs, PendingEvent, WriteOutcome};
use amux_core::{
    project::ExecutionPolicy,
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

pub fn save(
    conn: &Connection,
    name: &str,
    expected: i64,
    policy: &ExecutionPolicy,
    actor: &str,
) -> anyhow::Result<WriteOutcome> {
    anyhow::ensure!(amux_core::project::valid_name(name), "invalid project name");
    policy.validate().map_err(anyhow::Error::msg)?;
    let current = get(conn, name)?;
    let revision = current.as_ref().map(|p| p.revision).unwrap_or(0);
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
        anyhow::ensure!(!active || (current.policy.repository == policy.repository
            && current.policy.verify_command == policy.verify_command
            && current.policy.executor == policy.executor),
            "repository, verification gate and executor profile are fixed while executions retain work; finish or reconcile those executions first");
    }
    conn.execute("INSERT INTO group_config(name,execution_policy,execution_rev,updated) VALUES(?1,?2,?3,?4) ON CONFLICT(name) DO UPDATE SET execution_policy=excluded.execution_policy,execution_rev=excluded.execution_rev,updated=excluded.updated",
        params![name,serde_json::to_string(policy)?,revision+1,chrono::Utc::now().timestamp()])?;
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

pub fn board(conn: &Connection, name: &str) -> anyhow::Result<Value> {
    let project = get(conn, name)?.ok_or_else(|| anyhow::anyhow!("project not found"))?;
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
            card["retry_available"] = json!(plans.iter().find(|p| p.id == row.id).is_some_and(|plan| super::task_retry::eligible(conn, &project, row, &plan.execution).is_ok()));
            card
        })
        .collect();
    Ok(
        json!({"project":project,"pause_settled":pause_settled,"cards":cards,"migrations":migrations,"commands":super::intake::receipts(conn,name)?,"measured":true,"n_considered":rows.len(),
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
    let changes:Vec<_>=rows.iter().map(|r|(r.id.clone(),bs::BoardOwner::new(Some(name),None))).collect();
    if let Err(error)=bs::validate_owner_changes(conn,&changes) {
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
        assert_eq!(disabled["cards"][0]["execution_plan"]["waiting_reason"], "project_disabled");
        assert_eq!(disabled["cards"][0]["execution_plan"]["action"], "wait");
        store.write(|c| {
            let mut project = get(c, "example").map_err(sql_error)?.unwrap();
            project.policy.enabled = true;
            save(c, "example", project.revision, &project.policy, "test").map_err(sql_error)
        }).unwrap();
        // Once enabled, missing executable details still prevent verification.
        let before = board(&store.read().unwrap(), "example").unwrap();
        assert_eq!(before["cards"][0]["phase"], "waiting");
        assert_eq!(before["cards"][0]["execution_plan"]["waiting_reason"], "intake_required");
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
        assert_eq!(view["cards"][0]["execution_plan"]["waiting_reason"], "intake_required");
        assert_eq!(view["cards"][0]["execution_plan"]["action"], "wait");
        assert_eq!(bs::get_issue(&store.read().unwrap(), "A-1").unwrap().unwrap().status, "done");
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
    let changes:Vec<_>=before.rows.iter().map(|r|(r.id.clone(),bs::BoardOwner::new(None,r.session.as_deref()))).collect();
    bs::validate_owner_changes(conn,&changes)?;
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
