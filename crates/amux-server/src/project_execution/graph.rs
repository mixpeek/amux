//! One validation and readiness seam for a project's explicit task dependencies.
//!
//! Intake commits, generic board edits, output declarations, ownership migration and the planner all
//! ask this module, so a dependency is judged by one set of rules everywhere. An edge is a task
//! output relation inside ONE project. It never names a worker, a project or a task that does not
//! exist. A historical edge that breaks these rules is reported as invalid and blocks its task; it is
//! never silently rewritten or treated as satisfied.
use crate::db::board_store as bs;
use amux_core::task_graph::{analyze, path_to, Adjacency};
use rusqlite::Connection;
use std::collections::BTreeSet;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeError {
    SelfEdge,
    Duplicate(String),
    /// No live task has this id (unknown, deleted, or a worker/project identity).
    Missing(String),
    Archived(String),
    Foreign { dep: String, group: Option<String> },
    Cycle(Vec<String>),
    Unreadable(String),
}

impl EdgeError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::SelfEdge => "self",
            Self::Duplicate(_) => "duplicate",
            Self::Missing(_) => "missing",
            Self::Archived(_) => "archived",
            Self::Foreign { .. } => "foreign",
            Self::Cycle(_) => "cycle",
            Self::Unreadable(_) => "unreadable",
        }
    }
}

impl fmt::Display for EdgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SelfEdge => write!(f, "a task cannot depend on itself"),
            Self::Duplicate(id) => write!(f, "dependency {id} is listed twice"),
            Self::Missing(id) => write!(f, "dependency {id} is not a live task (workers and projects cannot be dependencies)"),
            Self::Archived(id) => write!(f, "dependency {id} is archived"),
            Self::Foreign { dep, group } => write!(f, "dependency {dep} belongs to {} , not this project", group.as_deref().unwrap_or("no project")),
            Self::Cycle(path) => write!(f, "circular dependency: {}", path.join(" -> ")),
            Self::Unreadable(e) => write!(f, "dependencies could not be checked: {e}"),
        }
    }
}

fn adjacency(rows: &[bs::IssueRow], task: &str, deps: &[String]) -> Adjacency {
    let mut graph = Adjacency::new();
    for row in rows.iter().filter(|r| r.id != task) {
        graph.insert(row.id.clone(), row.depends_on.iter().cloned().collect::<BTreeSet<_>>());
    }
    graph.insert(task.to_string(), deps.iter().cloned().collect());
    graph
}

fn reachable_cycle(graph: &Adjacency, starts: &[String]) -> Option<Vec<String>> {
    let nodes: BTreeSet<String> = graph.keys().cloned().collect();
    for cycle in analyze(&nodes, graph).cycles {
        if let Some(entry) = cycle.iter().find_map(|node| path_to(graph, starts, node)) {
            let mut witness = entry;
            witness.extend(cycle);
            return Some(witness);
        }
    }
    None
}

/// Structural rules for one edge that do not depend on the rest of the graph.
fn edge(conn: &Connection, project: &str, task: &str, dep: &str) -> Result<(), EdgeError> {
    if dep == task {
        return Err(EdgeError::SelfEdge);
    }
    let row = bs::get_issue(conn, dep).map_err(|e| EdgeError::Unreadable(e.to_string()))?;
    let Some(row) = row else { return Err(EdgeError::Missing(dep.into())) };
    if row.project_group.as_deref() != Some(project) {
        return Err(EdgeError::Foreign { dep: dep.into(), group: row.project_group });
    }
    if row.archived != 0 {
        return Err(EdgeError::Archived(dep.into()));
    }
    Ok(())
}

fn refused(project: &str, task: &str, error: &EdgeError, surface: &str) {
    tracing::warn!(project, task, surface, code = error.code(), measured = true, n_considered = 1,
        verdict = "project.dependency_refused", "{error}");
}

/// Judge the complete proposed dependency list of `task` before it is written. `task` may be a
/// placeholder for a card that does not exist yet. `surface` names the caller for the refusal log.
pub fn validate(conn: &Connection, project: &str, task: &str, deps: &[String], surface: &str) -> Result<(), EdgeError> {
    let result = (|| {
        let mut seen = BTreeSet::new();
        for dep in deps {
            if !seen.insert(dep.as_str()) {
                return Err(EdgeError::Duplicate(dep.clone()));
            }
            edge(conn, project, task, dep)?;
        }
        if deps.is_empty() {
            return Ok(());
        }
        let rows = bs::project_issues(conn, project).map_err(|e| EdgeError::Unreadable(e.to_string()))?;
        let graph = adjacency(&rows, task, deps);
        match path_to(&graph, deps, task) {
            Some(path) => {
                let mut cycle = vec![task.to_string()];
                cycle.extend(path);
                Err(EdgeError::Cycle(cycle))
            }
            None => reachable_cycle(&graph, deps).map_or(Ok(()), |path| Err(EdgeError::Cycle(path))),
        }
    })();
    if let Err(error) = &result {
        refused(project, task, error, surface);
    }
    result
}

/// Validate every task of a project after a bulk change (intake commit, migration).
pub fn validate_tasks(conn: &Connection, project: &str, ids: &[String], surface: &str) -> Result<(), (String, EdgeError)> {
    for id in ids {
        let row = bs::get_issue(conn, id)
            .map_err(|e| (id.clone(), EdgeError::Unreadable(e.to_string())))?;
        let Some(row) = row else { continue };
        if row.project_group.as_deref() != Some(project) {
            continue;
        }
        validate(conn, project, id, &row.depends_on, surface).map_err(|e| (id.clone(), e))?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    /// The named required output is not yet integrated and verified.
    WaitingOn(String),
    /// A required output exists but is held for authorization.
    Held(String),
    /// A dependency breaks the graph rules. The task cannot run and nothing rewrites the edge.
    Invalid(String, EdgeError),
}

impl Readiness {
    pub fn blocker(&self) -> Option<String> {
        match self {
            Self::Ready => None,
            Self::WaitingOn(dep) => Some(format!("required_output:{dep}")),
            Self::Held(_) => Some("required_output_unavailable".into()),
            Self::Invalid(dep, e) => Some(format!("invalid_dependency:{}:{dep}", e.code())),
        }
    }
}

/// The one readiness predicate: every dependency is a live same-project task whose output is
/// currently integrated and verified, and no authorization hold applies to it.
pub fn readiness(conn: &Connection, row: &bs::IssueRow) -> anyhow::Result<Readiness> {
    let Some(project) = row.project_group.as_deref() else {
        return Ok(if row.depends_on.is_empty() { Readiness::Ready } else { Readiness::Invalid(row.depends_on[0].clone(), EdgeError::Foreign { dep: row.depends_on[0].clone(), group: None }) });
    };
    for dep in &row.depends_on {
        if let Err(error) = edge(conn, project, &row.id, dep) {
            return Ok(Readiness::Invalid(dep.clone(), error));
        }
    }
    if !row.depends_on.is_empty() {
        let rows = bs::project_issues(conn, project)?;
        let graph = adjacency(&rows, &row.id, &row.depends_on);
        if let Some(path) = path_to(&graph, &row.depends_on, &row.id).or_else(|| reachable_cycle(&graph, &row.depends_on)) {
            let mut cycle = vec![row.id.clone()];
            cycle.extend(path);
            return Ok(Readiness::Invalid(row.depends_on[0].clone(), EdgeError::Cycle(cycle)));
        }
    }
    for dep in &row.depends_on {
        let output = bs::get_issue(conn, dep)?.ok_or_else(|| anyhow::anyhow!("dependency disappeared"))?;
        let execution = super::planner::execution(conn, dep)?;
        let current = output.status == "verified"
            && execution.stage == "verified"
            && execution.input_hash == super::planner::input_hash(&output)
            && execution.report.is_some();
        if !current {
            return Ok(Readiness::WaitingOn(dep.clone()));
        }
        super::assets::check(&execution.retained_assets)?;
        if super::outputs::authorization_hold(conn, &output)? {
            return Ok(Readiness::Held(dep.clone()));
        }
    }
    Ok(Readiness::Ready)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::WriteOutcome;

    fn task(c: &Connection, id: &str, group: Option<&str>, status: &str, deps: &str) {
        c.execute(
            "INSERT INTO issues(id,title,status,project_group,created,updated,depends_on) VALUES(?1,?1,?2,?3,1,1,?4)",
            rusqlite::params![id, status, group, deps],
        )
        .unwrap();
        if status == "verified" {
            current_verified(c, id);
        }
    }
    fn current_verified(c: &Connection, id: &str) {
        c.execute("UPDATE issues SET status='verified' WHERE id=?1", [id]).unwrap();
        let row = bs::get_issue(c, id).unwrap().unwrap();
        let mut execution = super::super::planner::execution(c, id).unwrap();
        execution.stage = "verified".into();
        execution.input_hash = super::super::planner::input_hash(&row);
        execution.report = Some(super::super::planner::Report {
            assets: vec![], head: "a".repeat(40), checks: vec![], summary: "verified output".into(),
        });
        super::super::planner::save_execution(c, &row, &execution, "test.verified").unwrap();
    }
    fn deps(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn edges_are_same_project_live_task_relations_and_acyclic() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Store::open(&dir.path().join("db")).unwrap();
        db.write(|c| {
            task(c, "A", Some("p"), "backlog", "[]");
            task(c, "B", Some("p"), "backlog", "[\"A\"]");
            task(c, "C", Some("p"), "backlog", "[\"B\"]");
            task(c, "OTHER", Some("q"), "verified", "[]");
            task(c, "LOOSE", None, "verified", "[]");
            task(c, "OLD", Some("p"), "verified", "[]");
            c.execute("UPDATE issues SET archived=1 WHERE id='OLD'", [])?;
            assert_eq!(validate(c, "p", "C", &deps(&["A", "B"]), "test"), Ok(()));
            assert_eq!(validate(c, "p", "C", &[], "test"), Ok(()));
            assert_eq!(validate(c, "p", "C", &deps(&["C"]), "test"), Err(EdgeError::SelfEdge));
            assert_eq!(validate(c, "p", "C", &deps(&["A", "A"]), "test"), Err(EdgeError::Duplicate("A".into())));
            for ghost in ["missing", "some-worker", "px-abc"] {
                assert_eq!(validate(c, "p", "C", &deps(&[ghost]), "test"), Err(EdgeError::Missing(ghost.into())), "{ghost}");
            }
            assert_eq!(validate(c, "p", "C", &deps(&["OTHER"]), "test"), Err(EdgeError::Foreign { dep: "OTHER".into(), group: Some("q".into()) }));
            assert_eq!(validate(c, "p", "C", &deps(&["LOOSE"]), "test"), Err(EdgeError::Foreign { dep: "LOOSE".into(), group: None }));
            assert_eq!(validate(c, "p", "C", &deps(&["OLD"]), "test"), Err(EdgeError::Archived("OLD".into())));
            // A -> C would close A -> C -> B -> A.
            let Err(EdgeError::Cycle(path)) = validate(c, "p", "A", &deps(&["C"]), "test") else { panic!("cycle expected") };
            assert_eq!(path.first().map(String::as_str), Some("A"));
            assert_eq!(path.last().map(String::as_str), Some("A"));
            // A brand new card cannot itself be on a cycle, and a diamond is not one.
            assert_eq!(validate(c, "p", "NEW", &deps(&["B", "C"]), "test"), Ok(()));
            Ok(WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();
    }

    #[test]
    fn readiness_is_shared_survives_reverse_completion_and_never_relabels_bad_edges() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Store::open(&dir.path().join("db")).unwrap();
        db.write(|c| {
            // Diamond: root -> left, right -> join. Left and right are independent and both ready.
            task(c, "ROOT", Some("p"), "verified", "[]");
            task(c, "LEFT", Some("p"), "backlog", "[\"ROOT\"]");
            task(c, "RIGHT", Some("p"), "backlog", "[\"ROOT\"]");
            task(c, "JOIN", Some("p"), "backlog", "[\"LEFT\",\"RIGHT\"]");
            let get = |id: &str| bs::get_issue(c, id).unwrap().unwrap();
            assert_eq!(readiness(c, &get("LEFT")).unwrap(), Readiness::Ready);
            assert_eq!(readiness(c, &get("RIGHT")).unwrap(), Readiness::Ready);
            assert_eq!(readiness(c, &get("JOIN")).unwrap().blocker().as_deref(), Some("required_output:LEFT"));
            // Reverse completion order: RIGHT finishes first, JOIN still waits for LEFT.
            current_verified(c, "RIGHT");
            assert_eq!(readiness(c, &get("JOIN")).unwrap(), Readiness::WaitingOn("LEFT".into()));
            current_verified(c, "LEFT");
            assert_eq!(readiness(c, &get("JOIN")).unwrap(), Readiness::Ready);
            // A stale failed output is not an output.
            c.execute("UPDATE issues SET status='doing' WHERE id='LEFT'", [])?;
            assert_eq!(readiness(c, &get("JOIN")).unwrap(), Readiness::WaitingOn("LEFT".into()));
            // A historical invalid edge blocks with a visible reason and is left exactly as stored.
            task(c, "BAD", Some("p"), "backlog", "[\"nobody\",\"ROOT\"]");
            task(c, "FOREIGN", Some("p"), "backlog", "[\"THEIRS\"]");
            task(c, "THEIRS", Some("q"), "verified", "[]");
            assert_eq!(readiness(c, &get("BAD")).unwrap().blocker().as_deref(), Some("invalid_dependency:missing:nobody"));
            assert_eq!(readiness(c, &get("FOREIGN")).unwrap().blocker().as_deref(), Some("invalid_dependency:foreign:THEIRS"));
            assert_eq!(get("BAD").depends_on, deps(&["nobody", "ROOT"]));
            // A historical cycle cannot run either.
            task(c, "X", Some("p"), "backlog", "[\"Y\"]");
            task(c, "Y", Some("p"), "verified", "[\"X\"]");
            assert_eq!(readiness(c, &get("X")).unwrap().blocker().as_deref(), Some("invalid_dependency:cycle:Y"));
            // Bulk validation names the first offending task.
            let bad = validate_tasks(c, "p", &["JOIN".into(), "BAD".into()], "test").unwrap_err();
            assert_eq!((bad.0.as_str(), bad.1.code()), ("BAD", "missing"));
            Ok(WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();
    }
}
