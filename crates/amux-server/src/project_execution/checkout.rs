//! One checkout belongs to the project, never to a worker attempt.
//! Worker workspace records are references to that checkout, not new worktrees.
use crate::fanout_workspace::{self as workspace, Workspace};
use sha2::{Digest, Sha256};
use std::path::Path;

pub(super) fn owner(home: &Path, project: &str) -> String {
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let hash = format!("{:x}", Sha256::digest(home.to_string_lossy().as_bytes()));
    format!("project-{project}-{}", &hash[..8])
}

pub(super) fn branch(home: &Path, project: &str) -> String {
    format!("amux/project/{}", owner(home, project))
}

pub(crate) fn load(home: &Path, project: &str) -> Option<Workspace> {
    workspace::load(home, &owner(home, project))
}

pub(crate) fn belongs_to(home: &Path, project: &str, w: &Workspace) -> bool {
    w.branch == branch(home, project)
        && load(home, project).is_some_and(|registered| {
            registered.path == w.path && registered.repo == w.repo && registered.base == w.base
        })
}

/// Existing in-flight assignments can report before the one-time consolidation.
/// This does not authorize creating another per-worker checkout.
pub(crate) fn assignment_matches(home: &Path, project: &str, worker: &str, w: &Workspace) -> bool {
    belongs_to(home, project, w)
        || (load(home, project).is_none()
            && w.branch == format!("amux/fanout/{worker}")
            && Path::new(&w.path) == workspace::expected_path(&w.repo, worker))
}

/// A direct Start click must obey the same durable checkout claim as dispatch.
pub(crate) fn start_permit(
    conn: &rusqlite::Connection,
    project: &str,
    worker: &str,
) -> Result<(), String> {
    let p = super::store::get(conn, project)
        .map_err(|e| e.to_string())?
        .ok_or("project missing")?;
    if p.policy.paused || !p.policy.enabled {
        return Err("project is paused or disabled".into());
    }
    let rows = crate::db::board_store::project_issues(conn, project).map_err(|e| e.to_string())?;
    let mut owned = false;
    for row in rows {
        let e = super::planner::execution(conn, &row.id).map_err(|e| e.to_string())?;
        if matches!(
            e.stage.as_str(),
            "reserved" | "working" | "reported" | "verifying"
        ) {
            if e.worker != worker {
                return Err("another project task owns the shared checkout".into());
            }
            owned = !e.suspended && matches!(e.stage.as_str(), "reserved" | "working");
        }
    }
    if !owned {
        return Err("project workers start when their task owns the checkout; resume work through the project".into());
    }
    Ok(())
}

/// Stop completed task processes before another task can write to the checkout.
pub(crate) async fn quiesce_others(
    state: &crate::api::AppState,
    project: &str,
    worker: &str,
) -> Result<(), String> {
    let workers = {
        let c = state.store.read().map_err(|e| e.to_string())?;
        crate::db::board_store::project_issues(&c, project)
            .map_err(|e| e.to_string())?
            .iter()
            .map(|r| super::planner::execution(&c, &r.id))
            .collect::<anyhow::Result<Vec<_>>>()
            .map_err(|e| e.to_string())?
    };
    for e in workers {
        if e.worker.is_empty() || e.worker == worker {
            continue;
        }
        if crate::api::session_verbs::is_running(&e.worker).await {
            if matches!(
                e.stage.as_str(),
                "reserved" | "working" | "reported" | "verifying"
            ) {
                return Err("another project task still owns the checkout".into());
            }
            crate::api::session_verbs::stop_for_pause(state, &e.worker)
                .await
                .map_err(|e| e.to_string())?;
            tracing::info!(project, worker=%e.worker, measured=true, n_considered=1,
                verdict="project.checkout_writer_released", "finished project executor stopped before checkout handoff");
        }
    }
    Ok(())
}

/// Consolidate persisted worker checkouts once, retaining their original records
/// and refs. Conflicts go through the existing task repair path, not a human hold.
pub(crate) async fn consolidate(
    state: &crate::api::AppState,
    p: &super::store::Project,
) -> anyhow::Result<()> {
    if !p.policy.worktree || p.policy.paused || !p.policy.enabled {
        return Ok(());
    }
    let home = crate::config::amux_home();
    let assignments = {
        let c = state.store.read()?;
        let rows = crate::db::board_store::project_issues(&c, &p.name)?;
        let mut result = Vec::new();
        for row in rows {
            let e = super::planner::execution(&c, &row.id)?;
            // Let an in-flight task finish in its current checkout first.
            if matches!(e.stage.as_str(), "working" | "reported" | "verifying") {
                return Ok(());
            }
            if let Some(w) = workspace::load(&home, &e.worker) {
                if !belongs_to(&home, &p.name, &w) && Path::new(&w.path).exists() {
                    result.push((row.id, e, w));
                }
            }
        }
        result
    };
    if assignments.is_empty() {
        return Ok(());
    }
    for (_, e, w) in &assignments {
        // Never adopt an arbitrary checkout or delete the repository itself.
        anyhow::ensure!(
            workspace::same_repository(&w.repo, &p.policy.repository)
                && Path::new(&w.path) == workspace::expected_path(&w.repo, &e.worker)
                && w.branch == format!("amux/fanout/{}", e.worker),
            "project import workspace identity differs; preserved"
        );
        if crate::api::session_verbs::is_running(&e.worker).await {
            crate::api::session_verbs::stop_verified_worker(state, &e.worker)
                .await
                .map_err(anyhow::Error::msg)?;
        }
        anyhow::ensure!(
            workspace::project_clean_status(&w.path)
                .await
                .map_err(anyhow::Error::msg)?
                .is_empty(),
            "project import contains uncommitted work at {}; preserved",
            w.path
        );
    }
    let owner = owner(&home, &p.name);
    let lock = crate::api::session_verbs::session_op_lock(&owner);
    let _guard = lock.lock().await;
    let w = workspace::ensure_named(&home, &owner, &p.policy.repository, &branch(&home, &p.name))
        .await
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        workspace::project_clean_status(&w.path)
            .await
            .map_err(anyhow::Error::msg)?
            .is_empty(),
        "project checkout has uncommitted work; imports preserved"
    );
    let archive = home.join("project-checkout-imports").join(&owner);
    std::fs::create_dir_all(&archive)?;
    for (task, e, old) in &assignments {
        let head = workspace::git(&old.path, &["rev-parse", "HEAD"])
            .await
            .map_err(anyhow::Error::msg)?;
        let record = archive.join(format!("{}.json", e.worker));
        if !record.exists() {
            let receipts = [
                "project-report.json",
                "project-wait.json",
                "project-required-outputs.json",
            ]
            .into_iter()
            .filter_map(|name| {
                std::fs::read_to_string(Path::new(&old.path).join(".amux").join(name))
                    .ok()
                    .map(|body| (name, body))
            })
            .collect::<std::collections::BTreeMap<_, _>>();
            let tmp = record.with_extension("tmp");
            std::fs::write(
                &tmp,
                serde_json::to_vec_pretty(
                    &serde_json::json!({"worker":e.worker,"task":task,"workspace":old,"head":head,"receipts":receipts}),
                )?,
            )?;
            std::fs::rename(tmp, &record)?;
        }
        if workspace::git(&w.path, &["merge-base", "--is-ancestor", &head, "HEAD"])
            .await
            .is_err()
        {
            if let Err(error) =
                workspace::git(&w.path, &["merge", "--no-ff", "--no-edit", &head]).await
            {
                let _ = workspace::git(&w.path, &["merge", "--abort"]).await;
                let partial = workspace::git(&w.path, &["rev-parse", "HEAD"])
                    .await
                    .map_err(anyhow::Error::msg)?;
                let expected_head = e
                    .report
                    .as_ref()
                    .map(|r| r.head.clone())
                    .unwrap_or_else(|| head.clone());
                let (project, task, head) = (p.clone(), task.clone(), head.clone());
                state.store.write_async(move |c| super::acceptance::repair_owner(c, &project, &task, &expected_head,
                    &serde_json::json!({"candidate":partial,"import_head":head,"composition_error":error,"instruction":"Merge the preserved import_head into the shared project checkout, resolve conflicts preserving both tasks, then verify and report."})).map_err(super::store::sql_error)).await?;
            }
        }
        workspace::save(&home, &e.worker, &w).map_err(anyhow::Error::msg)?;
    }
    prune_imports(&home, &p.name, &w, false)
        .await
        .map_err(anyhow::Error::msg)?;
    tracing::info!(project=%p.name, path=%w.path, measured=true, n_considered=assignments.len(),
        verdict="project.checkouts_consolidated", "project worker records now reference one checkout; original refs and receipts retained");
    Ok(())
}

pub(super) fn imports(home: &Path, project: &str) -> anyhow::Result<Vec<serde_json::Value>> {
    let dir = home
        .join("project-checkout-imports")
        .join(owner(home, project));
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let record: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
            rows.push(serde_json::json!({"task":record["task"],"worker":record["worker"],"head":record["head"],"path":record["workspace"]["path"]}));
        }
    }
    rows.sort_by(|a, b| a["task"].as_str().cmp(&b["task"].as_str()));
    Ok(rows)
}

async fn prune_imports(
    home: &Path,
    project: &str,
    w: &Workspace,
    require_complete: bool,
) -> Result<(), String> {
    let dir = home
        .join("project-checkout-imports")
        .join(owner(home, project));
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        let old: Workspace =
            serde_json::from_value(record["workspace"].clone()).map_err(|e| e.to_string())?;
        let worker = record["worker"].as_str().ok_or("import worker missing")?;
        let head = record["head"].as_str().ok_or("import head missing")?;
        if !Path::new(&old.path).exists() {
            continue;
        }
        if Path::new(&old.path) != workspace::expected_path(&old.repo, worker)
            || old.branch != format!("amux/fanout/{worker}")
            || !workspace::same_repository(&old.repo, &w.repo)
        {
            return Err("project import cleanup identity mismatch; preserved".into());
        }
        if workspace::git(&w.path, &["merge-base", "--is-ancestor", head, "HEAD"])
            .await
            .is_err()
        {
            if require_complete {
                return Err(format!(
                    "project import {worker} still has uncomposed commits; checkout preserved"
                ));
            }
            continue; // A normal project repair owns this conflict; never discard its source.
        }
        crate::fanout_retirement::check_checkout(&old, head).await?;
        crate::fanout_retirement::discard_harness_receipts(&old)?;
        let _ = workspace::git(&old.repo, &["worktree", "unlock", &old.path]).await;
        workspace::git(&old.repo, &["worktree", "remove", &old.path]).await?;
        tracing::info!(project, worker, path=%old.path, measured=true, n_considered=1,
            verdict="project.import_checkout_removed", "clean imported checkout removed after proving all its commits are in the project branch");
    }
    Ok(())
}

/// Remove the single checkout only after all outputs are accepted and published.
/// Retained worker records and content-addressed artifacts are never removed.
pub(crate) async fn cleanup<F: crate::runtime_jobs::board_drive::Fleet>(
    state: &crate::api::AppState,
    fleet: &F,
    home: &Path,
    project: &str,
) -> Result<bool, String> {
    let Some(w) = load(home, project) else {
        return Ok(false);
    };
    let name = owner(home, project);
    let lock = crate::api::session_verbs::session_op_lock(&name);
    let _guard = lock.lock().await;
    let check = || -> Result<Vec<String>, String> {
        let c = state.store.read().map_err(|e| e.to_string())?;
        if super::acceptance::retirement_allowed(&c, project).map_err(|e| e.to_string())?["allowed"]
            != true
        {
            return Err("project awaits artifact review".into());
        }
        let mut workers = Vec::new();
        for row in crate::db::board_store::project_issues(&c, project).map_err(|e| e.to_string())? {
            if !matches!(
                amux_core::project::phase(
                    &row.status,
                    crate::db::board_store::has_execution_details(&row)
                ),
                amux_core::project::Phase::Verified | amux_core::project::Phase::Closed
            ) {
                return Err("project has unfinished tasks; checkout retained".into());
            }
            let e = super::planner::execution(&c, &row.id).map_err(|e| e.to_string())?;
            super::assets::check(&e.retained_assets).map_err(|e| e.to_string())?;
            if !e.worker.is_empty() {
                workers.push(e.worker);
            }
        }
        Ok(workers)
    };
    let workers = check()?;
    if Path::new(&w.path) != workspace::expected_path(&w.repo, &name)
        || !belongs_to(home, project, &w)
    {
        return Err("project checkout identity differs; preserved".into());
    }
    if !Path::new(&w.path).exists() {
        if crate::api::session_verbs::worktree_is_registered(&w.repo, &w.path).await {
            return Err("project worktree registration remains without its directory".into());
        }
        return Ok(false);
    }
    for worker in &workers {
        if fleet.active_child_work(worker) {
            return Err("project worker still has child work".into());
        }
        if fleet.is_running(worker).await {
            fleet.stop_for_retirement(worker).await?;
        }
        if fleet.is_running(worker).await {
            return Err("project worker did not stop".into());
        }
    }
    workspace::git(&w.repo, &["fetch", "origin", "main"]).await?;
    let head = workspace::git(&w.path, &["rev-parse", "HEAD"]).await?;
    workspace::git(
        &w.repo,
        &["merge-base", "--is-ancestor", &head, "origin/main"],
    )
    .await
    .map_err(|_| "project checkout contains unpublished commits; preserved")?;
    crate::fanout_retirement::check_checkout(&w, &head).await?;
    check()?;
    prune_imports(home, project, &w, true).await?;
    crate::fanout_retirement::discard_harness_receipts(&w)?;
    let _ = workspace::git(&w.repo, &["worktree", "unlock", &w.path]).await;
    workspace::git(&w.repo, &["worktree", "remove", &w.path]).await?;
    if Path::new(&w.path).exists()
        || crate::api::session_verbs::worktree_is_registered(&w.repo, &w.path).await
    {
        return Err("project checkout removal incomplete".into());
    }
    tracing::info!(project, head, path=%w.path, measured=true, n_considered=workers.len(),
        verdict="project.checkout_removed", "published project checkout removed; worker history and artifacts retained");
    Ok(true)
}

/// All startup paths (including a direct UI restart) share this lock and record.
/// A worker can never fall back to manufacturing a private checkout.
pub(crate) async fn ensure(
    home: &Path,
    project: &str,
    worker: &str,
    repo: &str,
) -> Result<Workspace, String> {
    if !crate::api::session_verbs::valid_session_name(project) {
        return Err("invalid project workspace identity".into());
    }
    let name = owner(home, project);
    let lock = crate::api::session_verbs::session_op_lock(&name);
    let _guard = lock.lock().await;
    if let Some(old) = workspace::load(home, worker) {
        if !belongs_to(home, project, &old) && Path::new(&old.path).exists() {
            tracing::warn!(project, worker, path=%old.path, measured=true, n_considered=1,
                verdict="project.checkout_consolidation_required",
                "prior worker checkout retained until its work is consolidated");
            return Err("project checkout consolidation required; existing work preserved".into());
        }
    }
    let w = workspace::ensure_named(home, &name, repo, &branch(home, project)).await?;
    if !workspace::same_repository(&w.repo, repo) {
        return Err("project repository changed; existing checkout preserved".into());
    }
    workspace::save(home, worker, &w)?;
    tracing::info!(project, worker, path=%w.path, branch=%w.branch, measured=true,
        n_considered=1, verdict="project.shared_checkout_ready",
        "worker assigned to the project's single checkout");
    Ok(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_parallel_limit_cannot_grant_two_checkout_writers() {
        let (_dir, db, _) = super::super::outputs::tests::fixture();
        db.write(|c| {
            c.execute("UPDATE group_config SET execution_policy=json_set(execution_policy,'$.max_executors',3) WHERE name='sample'", [])?;
            for id in ["B", "C"] { super::super::planner::claim(c, "sample", id).unwrap(); }
            let p = super::super::store::get(c, "sample").unwrap().unwrap();
            let plans = super::super::planner::plan(c, &p).unwrap();
            let writers = plans.iter().filter(|p| p.execution.stage == "reserved").collect::<Vec<_>>();
            assert_eq!(writers.len(), 1);
            assert!(start_permit(c, "sample", &writers[0].execution.worker).is_ok());
            assert!(start_permit(c, "sample", "unassigned").is_err());
            Ok(crate::db::WriteOutcome {applied:false,events:vec![]})
        }).unwrap();
    }

    #[test]
    fn existing_clean_worker_checkouts_are_consolidated_without_losing_commits() {
        let dir = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(dir.path());
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let repo = repo.to_str().unwrap().to_string();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            for args in [vec!["init", "-b", "main"], vec!["config", "user.name", "Test"], vec!["config", "user.email", "test@local"], vec!["commit", "--allow-empty", "-m", "base"]] {
                workspace::git(&repo, &args).await.unwrap();
            }
            let db = crate::db::Store::open(&dir.path().join("db")).unwrap();
            let configured = repo.clone();
            db.write(move|c| {
                let policy = serde_json::from_value(serde_json::json!({"repository":configured,"enabled":true,"coordinator":{"provider":"codex","model":"gpt-6-luna"},"executor":{"provider":"codex","model":"gpt-6-luna"},"verify_command":"git diff --check"})).unwrap();
                super::super::store::save(c,"demo",0,&policy,"test").map_err(super::super::store::sql_error)
            }).unwrap();
            let mut originals = Vec::new();
            for id in ["one", "two"] {
                let worker_name = format!("checkout-import-test-{id}-{}", std::process::id());
                let old = workspace::ensure(dir.path(), &worker_name, &repo).await.unwrap();
                let file = format!("{id}.txt");
                std::fs::write(Path::new(&old.path).join(&file), id).unwrap();
                workspace::git(&old.path, &["add", &file]).await.unwrap();
                workspace::git(&old.path, &["commit", "-m", id]).await.unwrap();
                let head = workspace::git(&old.path, &["rev-parse", "HEAD"]).await.unwrap();
                let worker = worker_name;
                db.write(move |c| {
                    c.execute("INSERT INTO issues(id,title,status,type,project_group,next_action,acceptance_criteria,created,updated) VALUES(?1,?1,'verified','code','demo','Keep output','[\"Output exists\"]',1,1)", [&worker])?;
                    let row=crate::db::board_store::get_issue(c,&worker)?.unwrap();
                    let e=super::super::planner::Execution{worker:worker.clone(),stage:"verified".into(),..Default::default()};
                    super::super::planner::save_execution(c,&row,&e,"test").map_err(super::super::store::sql_error)
                }).unwrap();
                originals.push((old, head));
            }
            let state=crate::api::AppState{store:std::sync::Arc::new(db),started:std::time::Instant::now(),build_hash:"test".into(),auth_token:None,reconciled:std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true))};
            let p=super::super::store::get(&state.store.read().unwrap(),"demo").unwrap().unwrap();
            consolidate(&state,&p).await.unwrap();
            let w=load(dir.path(),"demo").unwrap();
            for id in ["one","two"] {
                assert_eq!(std::fs::read_to_string(Path::new(&w.path).join(format!("{id}.txt"))).unwrap(),id);
                assert_eq!(workspace::load(dir.path(),&format!("checkout-import-test-{id}-{}",std::process::id())).unwrap().path,w.path);
            }
            for (old,head) in originals {
                assert!(!Path::new(&old.path).exists());
                workspace::git(&w.path,&["merge-base","--is-ancestor",&head,"HEAD"]).await.unwrap();
                assert_eq!(workspace::git(&repo,&["rev-parse",&old.branch]).await.unwrap(),head,"original commit remains reviewable");
            }
            consolidate(&state,&p).await.unwrap();
            assert_eq!(workspace::git(&repo,&["worktree","list","--porcelain"]).await.unwrap().lines().filter(|l|l.starts_with("worktree ")).count(),2);
        });
    }

    #[tokio::test]
    async fn workers_and_retries_share_exactly_one_project_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let repo = repo.to_str().unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@local",
                "commit",
                "--allow-empty",
                "-m",
                "base",
            ],
        ] {
            workspace::git(repo, &args).await.unwrap();
        }
        let a = ensure(&home, "demo", "worker-a", repo).await.unwrap();
        std::fs::write(Path::new(&a.path).join("first.txt"), "first task").unwrap();
        workspace::git(&a.path, &["add", "first.txt"])
            .await
            .unwrap();
        workspace::git(
            &a.path,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@local",
                "commit",
                "-m",
                "first task",
            ],
        )
        .await
        .unwrap();
        let (b, retry) = tokio::join!(
            ensure(&home, "demo", "worker-b", repo),
            ensure(&home, "demo", "worker-a-retry", repo)
        );
        for w in [b.unwrap(), retry.unwrap()] {
            assert_eq!(w.path, a.path);
            assert_eq!(w.branch, a.branch);
            assert_eq!(
                std::fs::read_to_string(Path::new(&w.path).join("first.txt")).unwrap(),
                "first task"
            );
        }
        let list = workspace::git(repo, &["worktree", "list", "--porcelain"])
            .await
            .unwrap();
        assert_eq!(
            list.lines().filter(|l| l.starts_with("worktree ")).count(),
            2,
            "main plus exactly one project checkout"
        );
        let other = ensure(&home, "other", "worker-c", repo).await.unwrap();
        assert_ne!(a.path, other.path);
        assert!(belongs_to(&home, "demo", &a));
        assert!(!belongs_to(&home, "other", &a));
    }
}
