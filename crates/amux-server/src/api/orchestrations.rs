//! A compact projection of existing boards, not a second orchestration store.
use super::{org, AppState};
use crate::db::board_store;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

fn project(
    conn: &Connection,
    tracked_workers: &HashSet<String>,
    allowed: Option<&HashSet<String>>,
) -> rusqlite::Result<Value> {
    let mut stmt = conn.prepare("SELECT id,title,status,COALESCE(type,'code'),epic,session,COALESCE(archived,0),updated FROM issues WHERE deleted IS NULL")?;
    let rows = stmt.query_map([], |r| {
        let status: String = r.get(2)?;
        let kind: String = r.get(3)?;
        Ok(json!({"id":r.get::<_,String>(0)?,"title":r.get::<_,String>(1)?,
            "execution_terminal":board_store::execution_is_terminal(&status,&kind),
            "status":status,"type":kind,"epic":r.get::<_,Option<String>>(4)?,
            "session":r.get::<_,Option<String>>(5)?,"archived":r.get::<_,i64>(6)?,"updated":r.get::<_,f64>(7)?}))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    // Scope before following graph edges: an allowed child must not disclose a
    // foreign parent's title. Missing parents remain explicit to the renderer.
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|r| allowed.is_none_or(|a| r["session"].as_str().is_some_and(|s| a.contains(s))))
        .collect();
    let n = rows.len();
    // A board epic alone is not an orchestration. Start with actual worker
    // ownership, then retain its ancestors for outcome context. Follow-ups on
    // those workers remain visible even when they have no epic link.
    let by_id: HashMap<_, _> = rows
        .iter()
        .filter_map(|r| r["id"].as_str().map(|id| (id, r)))
        .collect();
    let mut included = HashSet::new();
    for row in rows.iter().filter(|r| {
        r["session"]
            .as_str()
            .is_some_and(|s| tracked_workers.contains(s))
    }) {
        let mut current = Some(row);
        while let Some(card) = current {
            let Some(id) = card["id"].as_str() else { break };
            if !included.insert(id) {
                break;
            }
            current = card["epic"]
                .as_str()
                .and_then(|parent| by_id.get(parent).copied());
        }
    }
    let cards: Vec<_> = rows
        .iter()
        .filter(|r| r["id"].as_str().is_some_and(|id| included.contains(id)))
        .cloned()
        .collect();
    let mut workers: Vec<_> = tracked_workers
        .iter()
        .filter(|name| allowed.is_none_or(|scope| scope.contains(*name)))
        .cloned()
        .collect();
    workers.sort();
    tracing::debug!(
        verdict = "orchestration_projection",
        measured = true,
        n_considered = n,
        n_included = cards.len(),
        n_excluded = n - cards.len(),
        n_workers = workers.len(),
        "projected actual orchestration worker boards and their ancestors"
    );
    Ok(
        json!({"measured":true,"n_considered":n,"n_excluded_unrelated":n-cards.len(),"cards":cards,"ephemeral_workers":workers}),
    )
}

fn inventory(
    home: &std::path::Path,
    allowed: Option<&HashSet<String>>,
) -> std::io::Result<Vec<Value>> {
    let entries = match std::fs::read_dir(home.join("sessions")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut configs = BTreeMap::new();
    for entry in entries {
        let entry = entry?;
        let file = entry.file_name();
        let Some(file) = file.to_str() else { continue };
        let Some(name) = file
            .strip_suffix(".env")
            .or_else(|| file.strip_suffix(".env.reaped"))
        else {
            continue;
        };
        if allowed.is_some_and(|scope| !scope.contains(name)) {
            continue;
        }
        let retired = file.ends_with(".env.reaped");
        // A retained retirement receipt must not override a current worker.
        if retired && configs.contains_key(name) {
            continue;
        }
        configs.insert(
            name.to_string(),
            (crate::config::parse_env_file(&entry.path()), retired),
        );
    }
    let parents: HashSet<_> = configs
        .values()
        .filter(|(env, _)| env.get("CC_EPHEMERAL").is_some_and(|v| v == "1"))
        .filter_map(|(env, _)| env.get("CC_PARENT"))
        .filter(|name| !name.is_empty())
        .cloned()
        .collect();
    Ok(configs.iter().filter_map(|(name,(env,retired))| {
        let ephemeral = env.get("CC_EPHEMERAL").is_some_and(|v|v=="1");
        let orchestrator = env.get("CC_ORCHESTRATOR").is_some_and(|v|v=="1");
        if !ephemeral && !orchestrator && !parents.contains(name) { return None; }
        let lifecycle = if *retired { "expired" }
            else if env.get("CC_ARCHIVED").is_some_and(|v|v=="1") { "archived" }
            else if env.get("CC_PAUSED").is_some_and(|v|v=="1") { "paused" } else { "active" };
        let provider = env.get("CC_PROVIDER").map(String::as_str).unwrap_or("claude");
        let model = super::session_verbs::configured_model_for(provider,
            env.get("CC_MODEL").map(String::as_str).unwrap_or(""), env.get("CC_FLAGS").map(String::as_str).unwrap_or(""));
        let parent = env.get("CC_PARENT").filter(|p| !p.is_empty() && allowed.is_none_or(|scope|scope.contains(*p)));
        Some(json!({"name":name,"ephemeral":ephemeral,"orchestrator":orchestrator,"lifecycle":lifecycle,
            "project":env.get("CC_PROJECT").map(String::as_str).unwrap_or(""),
            "role":if orchestrator || parents.contains(name) {"orchestrator"} else {"fan-out"},
            "ephemeral_parent":parent,"profile":{"measured":true,"provider":provider,"model":model},
            "running":if *retired {json!(false)} else {Value::Null},
            "worktree_active":home.join("worktrees").join(name).join(".git").exists(),
            "branch":crate::fanout_workspace::load(home,name).map(|w|w.branch),
            "worktree_integration":crate::fanout_workspace::integration_status(home,name)}))
    }).collect())
}

pub async fn list(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let allowed = org::local_member_scope(&headers)
        .filter(|s| !s.is_global())
        .map(|s| {
            org::scoped_worker_names(&s)
                .into_iter()
                .collect::<HashSet<_>>()
        });
    let workers = match inventory(&crate::config::amux_home(), allowed.as_ref()) {
        Ok(workers) => workers,
        Err(e) => {
            tracing::warn!(verdict="orchestration_inventory_failed",error=%e,"could not measure fan-out inventory");
            return (StatusCode::SERVICE_UNAVAILABLE,Json(json!({"measured":false,"n_considered":0,"error":"Could not read fan-out inventory"}))).into_response();
        }
    };
    let tracked_workers: HashSet<_> = workers
        .iter()
        .filter(|w| w["ephemeral"] == true || w["orchestrator"] == true)
        .filter_map(|w| w["name"].as_str().map(str::to_string))
        .collect();
    let result = state
        .store
        .read()
        .and_then(|conn| Ok(project(&conn, &tracked_workers, allowed.as_ref())?));
    match result {
        Ok(mut v) => {
            v["ephemeral_workers"] = json!(workers
                .iter()
                .filter(|w| w["ephemeral"] == true)
                .filter_map(|w| w["name"].as_str())
                .collect::<Vec<_>>());
            v["workers"] = json!(workers);
            Json(v).into_response()
        }
        Err(e) => {
            tracing::warn!(verdict="orchestration_projection_failed",error=%e,"orchestration board projection failed");
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"measured":false,"n_considered":0,"error":"Could not read orchestration boards"}))).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projection_includes_full_child_board_and_does_not_return_history_prose() {
        let c = crate::db::migrate::test_memdb();
        c.execute_batch(
            "INSERT INTO issues(id,title,status,type,session,epic,created,updated,desc) VALUES
            ('E','Epic','doing','epic','parent',NULL,1,1,'private long prompt'),
            ('A','Assignment','done','code','child','E',1,1,''),
            ('B','Follow-up','backlog','code','child',NULL,1,1,''),
            ('C','Unrelated history','done','code','other',NULL,1,1,''),
            ('U','Ordinary epic','doing','epic','parent',NULL,1,1,''),
            ('V','Ordinary subtask','todo','code','parent','U',1,1,'');",
        )
        .unwrap();
        let eph = HashSet::from(["child".into()]);
        let v = project(&c, &eph, None).unwrap();
        assert_eq!(v["cards"].as_array().unwrap().len(), 3);
        assert_eq!(v["n_considered"], 6);
        assert_eq!(v["n_excluded_unrelated"], 3);
        assert!(!v.to_string().contains("Ordinary"));
        assert!(!v.to_string().contains("private long prompt"));
        assert!(!v["cards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == "A")
            .unwrap()["execution_terminal"]
            .as_bool()
            .unwrap());
        let v = project(&c, &eph, Some(&HashSet::from(["child".into()]))).unwrap();
        assert_eq!(v["cards"].as_array().unwrap().len(), 2);
        assert!(!v.to_string().contains("\"title\":\"Epic\""));
    }

    #[test]
    fn projection_keeps_nested_ancestors_without_cycles_or_unrelated_siblings() {
        let c = crate::db::migrate::test_memdb();
        c.execute_batch(
            "INSERT INTO issues(id,title,status,type,session,epic,created,updated) VALUES
            ('ROOT','Root','doing','epic','parent','NEST',1,1),
            ('NEST','Nested','doing','epic','parent','ROOT',1,1),
            ('A','Assigned','todo','code','child','NEST',1,1),
            ('B','Sibling outside fanout','todo','code','other','ROOT',1,1),
            ('O','Coordinate outcome','todo','research','coordinator',NULL,1,1);",
        )
        .unwrap();
        let v = project(
            &c,
            &HashSet::from(["child".into(), "coordinator".into()]),
            None,
        )
        .unwrap();
        let ids: HashSet<_> = v["cards"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, HashSet::from(["ROOT", "NEST", "A", "O"]));
        let empty = project(&c, &HashSet::new(), None).unwrap();
        assert_eq!(empty["cards"], json!([]));
        assert_eq!(empty["n_considered"], 5);
    }

    #[test]
    fn inventory_includes_actual_parents_without_promoting_all_their_board_work() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("sessions");
        std::fs::create_dir(&dir).unwrap();
        for (name, body) in [
            (
                "parent.env",
                "CC_PROVIDER=codex\nCC_FLAGS='--model gpt-5'\nCC_PAUSED=1\n",
            ),
            (
                "child.env",
                "CC_EPHEMERAL=1\nCC_PARENT=parent\nCC_FLAGS='--model haiku'\n",
            ),
            (
                "child.env.reaped",
                "CC_EPHEMERAL=1\nCC_PARENT=wrong-parent\n",
            ),
            (
                "px-live.env",
                "CC_EPHEMERAL=1\nCC_PARENT=project-coordinator\n",
            ),
            (
                "px-live.env.reaped",
                "CC_EPHEMERAL=1\nCC_PARENT=old-project\n",
            ),
            (
                "px-retired.env.reaped",
                "CC_EPHEMERAL=1\nCC_PARENT=project-coordinator\nCC_PROJECT=sample\n",
            ),
            ("retired.env.reaped", "CC_EPHEMERAL=1\nCC_PARENT=parent\n"),
            (
                "new-coordinator.env",
                "CC_ORCHESTRATOR=1\nCC_PROVIDER=gemini\n",
            ),
            ("ordinary.env", "CC_PROVIDER=claude\n"),
        ] {
            std::fs::write(dir.join(name), body).unwrap();
        }
        let rows = inventory(home.path(), None).unwrap();
        // Distinct identities: parent, child, px-live, px-retired, retired, new-coordinator.
        // Live and .reaped files of one name collapse; the ordinary worker is excluded.
        assert_eq!(rows.len(), 6);
        assert!(rows.iter().all(|w| w["name"] != "ordinary"));
        let find = |name: &str| rows.iter().find(|w| w["name"] == name).unwrap();
        assert_eq!(find("px-retired")["project"], "sample");
        assert_eq!(find("parent")["role"], "orchestrator");
        assert_eq!(find("parent")["orchestrator"], false); // ordinary parent's unrelated board stays out
        assert_eq!(find("parent")["profile"]["model"], "gpt-5");
        assert_eq!(find("parent")["lifecycle"], "paused");
        assert_eq!(find("child")["lifecycle"], "active");
        assert_eq!(find("child")["ephemeral_parent"], "parent");
        assert_eq!(find("px-live")["lifecycle"], "active");
        assert_eq!(find("px-live")["ephemeral_parent"], "project-coordinator");
        assert_eq!(find("px-retired")["lifecycle"], "expired");
        assert_eq!(
            find("px-retired")["ephemeral_parent"],
            "project-coordinator"
        );
        assert_eq!(find("retired")["lifecycle"], "expired");
        assert_eq!(find("new-coordinator")["orchestrator"], true);
        let scoped = inventory(
            home.path(),
            Some(&HashSet::from(["child".into(), "px-retired".into()])),
        )
        .unwrap();
        assert_eq!(scoped.len(), 2);
        let scoped_child = scoped.iter().find(|w| w["name"] == "child").unwrap();
        assert!(scoped_child["ephemeral_parent"].is_null());
        assert_eq!(scoped_child["role"], "fan-out");
        let scoped_retired = scoped.iter().find(|w| w["name"] == "px-retired").unwrap();
        assert_eq!(scoped_retired["lifecycle"], "expired");
        assert!(scoped_retired["ephemeral_parent"].is_null());
    }
}
