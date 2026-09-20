use crate::db::{board_store as bs, PendingEvent, WriteOutcome};
use amux_core::{
    project::{phase, ExecutionPolicy},
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
    let cards: Vec<_> = rows
        .iter()
        .map(|row| {
            let mut card = row.snapshot_slim();
            card["phase"] = json!(phase(&row.status, bs::has_execution_details(row)));
            card["assignee"] = json!(row.session);
            card
        })
        .collect();
    Ok(
        json!({"project":project,"cards":cards,"measured":true,"n_considered":rows.len(),
        "usage":{"measured":false,"reason":"project attempt attribution has not been observed","tokens":null,"cost_usd":null}}),
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
    let selected: std::collections::HashSet<_> = rows.iter().map(|r| r.id.as_str()).collect();
    let mut q=conn.prepare("SELECT i.id,d.value FROM issues i,json_each(CASE WHEN json_valid(i.depends_on) THEN i.depends_on ELSE '[]' END) d WHERE i.deleted IS NULL AND i.session IN (SELECT value FROM json_each(?1))")?;
    for pair in q.query_map([serde_json::to_string(&workers)?], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (card, dependency) = pair?;
        if selected.contains(dependency.as_str()) {
            continue;
        }
        let owning_project: Option<String> = conn
            .query_row(
                "SELECT project_group FROM issues WHERE id=?1 AND deleted IS NULL",
                [&dependency],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if owning_project.as_deref() != Some(name) {
            conflicts.push(format!("{card} references outside prerequisite {dependency}; retain its required artifact and resolve the edge before migration"));
        }
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
        assert_eq!(
            board(&store.read().unwrap(), "example").unwrap()["cards"][0]["phase"],
            "verifying"
        );
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
