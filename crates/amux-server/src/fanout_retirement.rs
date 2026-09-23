//! Completion disposal is separate from stop/pause and from forceful deletion.
//! Keep board history and branch refs; remove only a verified, published checkout.
use crate::{
    api::AppState,
    fanout_workspace::{self as workspace, git},
    runtime_jobs::board_drive::Fleet,
};
use std::path::Path;

type Board = Vec<(String, i64)>;

#[derive(Debug, PartialEq)]
pub(crate) enum Outcome {
    Deferred,
    NeedsIntegration,
    ReviewHeld,
    Expired,
}

fn flag(env: &std::collections::BTreeMap<String, String>, key: &str) -> bool {
    env.get(key).is_some_and(|v| v == "1")
}

fn enabled(env: &std::collections::BTreeMap<String, String>) -> bool {
    if !flag(env, "CC_EPHEMERAL") || flag(env, "CC_ARCHIVED") || flag(env, "CC_ISOLATED") {
        return false;
    }
    // A project executor can be paused because it is held after verified work
    // until human artifact review accepts the current project. Once accepted,
    // retirement must still be able to remove the worktree and expire the
    // worker; otherwise review-held executors accumulate forever in Paused.
    // Ordinary paused work is preserved because it lacks CC_REVIEW_HELD.
    !flag(env, "CC_PAUSED") || flag(env, "CC_REVIEW_HELD")
}

/// Done for another item type is deliberately insufficient for auto-disposal.
/// Include every non-archived card, including epics and non-code work.
fn verified_board(conn: &rusqlite::Connection, name: &str) -> rusqlite::Result<Option<Board>> {
    let mut q = conn.prepare("SELECT id,rev,status FROM issues WHERE session=?1 AND deleted IS NULL AND COALESCE(archived,0)=0 ORDER BY id")?;
    let rows = q
        .query_map([name], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.is_empty() || rows.iter().any(|r| r.2 != "verified") {
        return Ok(None);
    }
    let pending: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM steering_queue WHERE session=?1)",
        [name],
        |r| r.get(0),
    )?;
    if pending {
        return Ok(None);
    }
    Ok(Some(
        rows.into_iter().map(|(id, rev, _)| (id, rev)).collect(),
    ))
}

fn board(state: &AppState, name: &str) -> Result<Option<Board>, String> {
    let conn = state.store.read().map_err(|e| e.to_string())?;
    verified_board(&conn, name).map_err(|e| e.to_string())
}

/// Returns Deferred for normal ineligibility; errors are retryable and surfaced by
/// the caller. All network/Git work stays outside the SQLite writer.
pub(crate) async fn retire<F: Fleet>(
    state: &AppState,
    fleet: &F,
    home: &Path,
    name: &str,
) -> Result<Outcome, String> {
    let active = home.join("sessions").join(format!("{name}.env"));
    let expired = active.with_extension("env.reaped");
    let source = if active.exists() {
        active.clone()
    } else {
        expired.clone()
    };
    let env_bytes = std::fs::read(&source).map_err(|e| e.to_string())?;
    let env = crate::config::parse_env_file(&source);
    if !enabled(&env) || fleet.is_isolated(name) {
        return Ok(Outcome::Deferred);
    }
    let review_held_while_paused = flag(&env, "CC_PAUSED") && flag(&env, "CC_REVIEW_HELD");
    let Some(snapshot) = board(state, name)? else {
        return Ok(Outcome::Deferred);
    };
    if review_held_while_paused {
        tracing::info!(
            session = name,
            verdict = "fanout_retirement_review_pause_allowed",
            measured = true,
            n_considered = 1,
            "review-held paused project executor is eligible for retirement after acceptance"
        );
    }
    if fleet.active_child_work(name)
        || (fleet.is_running(name).await && !fleet.at_boundary(name).await)
    {
        return Ok(Outcome::Deferred);
    }
    let receipt = workspace::integration_status(home, name);
    if receipt["status"] != "integrated" {
        return Ok(if source == active {
            Outcome::NeedsIntegration
        } else {
            Outcome::Deferred
        });
    }
    if env.contains_key("CC_PROJECT") && env.get("CC_WORKTREE").is_some_and(|v| v == "0") {
        if source == expired {
            return Ok(Outcome::Deferred);
        }
        let lock = crate::api::session_verbs::session_op_lock(name);
        let Ok(_op) = lock.try_lock() else {
            return Ok(Outcome::Deferred);
        };
        let still_current = || -> Result<bool, String> {
            Ok(active.exists()
                && std::fs::read(&active).ok().as_deref() == Some(env_bytes.as_slice())
                && board(state, name)?.as_ref() == Some(&snapshot))
        };
        if fleet.is_running(name).await {
            fleet.stop_for_retirement(name).await?;
        }
        if fleet.is_running(name).await {
            return Err("provider did not stop; shared checkout worker retained".into());
        }
        if !still_current()? {
            return Ok(Outcome::Deferred);
        }
        if let Some(project) = env.get("CC_PROJECT") {
            let gate = {
                let conn = state.store.read().map_err(|e| e.to_string())?;
                crate::project_execution::acceptance::retirement_allowed(&conn, project)
                    .map_err(|e| e.to_string())?
            };
            if gate["allowed"] != true {
                crate::api::session_verbs::set_review_hold_at(&active, true)?;
                tracing::info!(session=name,project,state=%gate["state"],fingerprint=%gate["fingerprint"],verdict="project_executor_review_held",measured=true,n_considered=1,
                    "verified shared-checkout executor stopped and retained until human artifact review accepts the current project");
                return Ok(Outcome::ReviewHeld);
            }
        }
        let worker = name.to_string();
        let old_env = env_bytes;
        let expected_board = snapshot;
        let retired_head = receipt["head"].as_str().unwrap_or("").to_string();
        let retired_main = receipt["merged"].as_str().unwrap_or("").to_string();
        let finalized = state.store.write_async(move |conn| {
            if !active.exists()
                || verified_board(conn,&worker)?.as_ref() != Some(&expected_board)
                || std::fs::read(&active).ok().as_deref() != Some(old_env.as_slice()) {
                return Ok(crate::db::WriteOutcome {applied:false,events:vec![]});
            }
            conn.execute("INSERT INTO session_events(ts,session,type,data,source) VALUES(?1,?2,'fanout.decommissioned',?3,'board-drive')",
                rusqlite::params![crate::config::now_f64(),worker,serde_json::json!({"head":retired_head,"main":retired_main,"cards":expected_board,"worktree_removed":false,"mode":"shared_checkout"}).to_string()])?;
            std::fs::rename(&active,&expired).map_err(|e|rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            Ok(crate::db::WriteOutcome {applied:true,events:vec![]})
        }).await;
        return match finalized {
            Ok(outcome) if outcome.applied => {
                crate::api::session_verbs::dispose_verified_worker_terminal(name).await;
                crate::api::sessions_legacy::invalidate_sessions_cache();
                tracing::info!(session=name,verdict="project_shared_executor_decommissioned",measured=true,worktree_removed=false,
                    "fully verified shared-checkout project executor expired after confirming integration and review");
                Ok(Outcome::Expired)
            }
            result => result.map(|_| Outcome::Deferred).map_err(|e| e.to_string()),
        };
    }
    let w = workspace::load(home, name).ok_or("verified worker has no workspace record")?;
    let head = receipt["head"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or("integration receipt has no head")?
        .to_string();
    // A corrupt/mispointed record must never delete a shared checkout.
    let expected = workspace::expected_path(&w.repo, name);
    if Path::new(&w.path) != expected || w.branch != format!("amux/fanout/{name}") {
        return Err("workspace record does not name this worker's own path and branch".into());
    }
    if source == expired
        && !expected.exists()
        && !crate::api::session_verbs::worktree_is_registered(&w.repo, &w.path).await
    {
        return Ok(Outcome::Deferred); // Already disposed; no recurring remote fetch.
    }
    // Refresh the remote, not just a stale integration receipt or local main.
    git(&w.repo, &["fetch", "origin", "main"]).await?;
    let main = git(&w.repo, &["rev-parse", "origin/main"]).await?;
    git(&w.repo, &["merge-base", "--is-ancestor", &head, &main])
        .await
        .map_err(|_| "integrated worker head is not contained in current remote main")?;
    let lock = crate::api::session_verbs::session_op_lock(name);
    let Ok(_op) = lock.try_lock() else {
        return Ok(Outcome::Deferred);
    };
    let still_current = || -> Result<bool, String> {
        Ok((source == active || !active.exists())
            && std::fs::read(&source).ok().as_deref() == Some(env_bytes.as_slice())
            && board(state, name)?.as_ref() == Some(&snapshot))
    };
    if !still_current()? {
        return Ok(Outcome::Deferred);
    }
    // A crash after successful worktree removal but before expiration leaves
    // the active env. Reconstruct only this preserved, published branch so the
    // same checks and non-force disposal can be retried normally.
    if !expected.exists() {
        if source == expired
            && !crate::api::session_verbs::worktree_is_registered(&w.repo, &w.path).await
        {
            return Ok(Outcome::Deferred); // Already fully decommissioned.
        }
        if git(&w.repo, &["rev-parse", &w.branch]).await? != head {
            return Err("missing workspace branch changed; preserved for recovery".into());
        }
        git(&w.repo, &["worktree", "add", &w.path, &w.branch]).await?;
    }
    check_checkout(&w, &head).await?;
    if fleet.is_running(name).await {
        fleet.stop_for_retirement(name).await?;
    }
    if fleet.is_running(name).await {
        return Err("provider did not stop; workspace retained".into());
    }
    // REAPING RELEASES LEASES TOO, not just DELETE (AMUX-4954).
    //
    // A retired worker is as unable to heartbeat as a deleted one, so the same
    // rule applies: a holder that can never report must stop being a holder.
    //
    // Expected to release nothing in the normal case — retirement requires a
    // fully Verified board and a lease lives on `doing` — which is exactly why
    // it is here rather than assumed. The card's criterion is "deleting OR
    // reaping", and a property that holds only on the path someone happened to
    // check is the asymmetry AMUX-4914 was about.
    {
        let holder = name.to_string();
        let _ = state
            .store
            .write_async(move |conn| {
                let n = crate::db::board_store::release_leases_for_holder(conn, &holder)?;
                if n > 0 {
                    tracing::warn!(
                        target: "amux::board", session = %holder, released = n, measured = true,
                        n_considered = n, verdict = "lease_released_on_retirement",
                        "retired worker still held {n} board lease(s) on a board that should \
                         have been terminal; released (AMUX-4954)"
                    );
                }
                Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
            })
            .await;
    }
    if !still_current()? {
        return Ok(Outcome::Deferred);
    }
    check_checkout(&w, &head).await?;
    if let Some(project) = env.get("CC_PROJECT") {
        let gate = {
            let conn = state.store.read().map_err(|e| e.to_string())?;
            crate::project_execution::acceptance::retirement_allowed(&conn, project)
                .map_err(|e| e.to_string())?
        };
        if gate["allowed"] != true {
            crate::api::session_verbs::set_review_hold_at(&source, true)?;
            tracing::info!(session=name,project,state=%gate["state"],fingerprint=%gate["fingerprint"],verdict="project_executor_review_held",measured=true,n_considered=1,
                "verified executor stopped; worktree and worker retained until human artifact review accepts the current project");
            return Ok(Outcome::ReviewHeld);
        }
    }
    // Only our durable worktree lock is released. Never force, rm -rf, or
    // globally prune registrations: new drafts must make Git refuse removal.
    // The harness receipt is local control-plane state that was already
    // ingested before human acceptance; remove only that exact file so Git can
    // still protect any real draft or generated artifact.
    discard_harness_receipts(&w)?;
    let _ = git(&w.repo, &["worktree", "unlock", &w.path]).await;
    git(&w.repo, &["worktree", "remove", &w.path]).await?;
    if expected.exists()
        || crate::api::session_verbs::worktree_is_registered(&w.repo, &w.path).await
    {
        return Err("worktree removal did not clear both directory and registration".into());
    }
    let active_for_finalize = active.clone();
    let (worker, src, dst, old_env, expected_board, retired_head, retired_main) = (
        name.to_string(),
        source.clone(),
        expired.clone(),
        env_bytes,
        snapshot,
        head,
        main,
    );
    // Serialize only the final comparison + tiny env rename against board and
    // input writes. New work arriving during Git cleanup wins; restore the clean
    // checkout below instead of orphaning its board on an expired worker.
    let finalized = state.store.write_async(move |conn| {
        if (src != active_for_finalize && active_for_finalize.exists())
            || verified_board(conn,&worker)?.as_ref() != Some(&expected_board)
            || std::fs::read(&src).ok().as_deref() != Some(old_env.as_slice()) {
            return Ok(crate::db::WriteOutcome {applied:false,events:vec![]});
        }
        conn.execute("INSERT INTO session_events(ts,session,type,data,source) VALUES(?1,?2,'fanout.decommissioned',?3,'board-drive')",
            rusqlite::params![crate::config::now_f64(),worker,serde_json::json!({"head":retired_head,"main":retired_main,"cards":expected_board,"worktree_removed":true}).to_string()])?;
        if src != dst { std::fs::rename(src,dst).map_err(|e|rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?; }
        Ok(crate::db::WriteOutcome {applied:true,events:vec![]})
    }).await;
    match finalized {
        Ok(outcome) if outcome.applied => {
            crate::api::session_verbs::dispose_verified_worker_terminal(name).await;
            crate::api::sessions_legacy::invalidate_sessions_cache();
            tracing::info!(session=name,verdict="fanout_decommissioned",measured=true,worktree_removed=true,
                "fully verified worker expired after confirming remote main and removing its worktree");
            Ok(Outcome::Expired)
        }
        result => {
            // Preserve the worker's ability to handle any new work. No source
            // changes were discarded: non-force removal required a clean tree.
            if source == active && !active.exists() && expired.exists() {
                std::fs::rename(&expired, &active).map_err(|e| e.to_string())?;
            }
            git(&w.repo, &["worktree", "add", &w.path, &w.branch]).await?;
            tracing::info!(
                session = name,
                verdict = "fanout_retirement_changed",
                "retirement cancelled; restored workspace for changed completion state"
            );
            result.map(|_| Outcome::Deferred).map_err(|e| e.to_string())
        }
    }
}

async fn check_checkout(w: &workspace::Workspace, head: &str) -> Result<(), String> {
    let root = std::fs::canonicalize(&w.path).map_err(|e| e.to_string())?;
    let actual = git(&w.path, &["rev-parse", "--show-toplevel"]).await?;
    if root != std::fs::canonicalize(actual).map_err(|e| e.to_string())?
        || git(&w.path, &["branch", "--show-current"]).await? != w.branch
        || git(&w.path, &["rev-parse", "HEAD"]).await? != head
    {
        return Err("workspace identity/head differs from its integration receipt".into());
    }
    let common = git(
        &w.path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    let repo_common = git(
        &w.repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    if std::fs::canonicalize(common).map_err(|e| e.to_string())?
        != std::fs::canonicalize(repo_common).map_err(|e| e.to_string())?
    {
        return Err("workspace belongs to another repository".into());
    }
    if !workspace::project_clean_status(&w.path).await?.is_empty() {
        return Err("workspace has uncommitted or untracked work; preserved".into());
    }
    Ok(())
}

fn discard_harness_receipts(w: &workspace::Workspace) -> Result<(), String> {
    for name in ["project-report.json", "project-wait.json"] {
        let receipt = Path::new(&w.path).join(".amux").join(name);
        match std::fs::remove_file(&receipt) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    let _ = std::fs::remove_dir(Path::new(&w.path).join(".amux"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    };
    struct TestFleet {
        running: AtomicBool,
        boundary: bool,
        child: bool,
        stops: AtomicUsize,
        fail_stop: bool,
        on_stop: Option<Box<dyn Fn() + Send + Sync>>,
    }
    impl Default for TestFleet {
        fn default() -> Self {
            Self {
                running: AtomicBool::new(true),
                boundary: true,
                child: false,
                stops: AtomicUsize::new(0),
                fail_stop: false,
                on_stop: None,
            }
        }
    }
    impl Fleet for TestFleet {
        fn lanes(&self) -> Vec<String> {
            vec!["child".into()]
        }
        fn auto_pickup_enabled(&self, _: &str) -> bool {
            true
        }
        fn auto_continue_enabled(&self, _: &str) -> bool {
            true
        }
        fn tags(&self, _: &str) -> Vec<String> {
            vec![]
        }
        async fn is_running(&self, _: &str) -> bool {
            self.running.load(Ordering::SeqCst)
        }
        async fn at_boundary(&self, _: &str) -> bool {
            self.boundary
        }
        fn active_child_work(&self, _: &str) -> bool {
            self.child
        }
        async fn deliver(&self, _: &str, _: &str) {}
        async fn stop_for_retirement(&self, _: &str) -> Result<(), String> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            if self.fail_stop {
                return Err("provider failed to stop".into());
            }
            self.running.store(false, Ordering::SeqCst);
            if let Some(f) = &self.on_stop {
                f();
            }
            Ok(())
        }
    }
    struct Fixture {
        _dir: tempfile::TempDir,
        name: String,
        home: std::path::PathBuf,
        state: AppState,
        w: workspace::Workspace,
        head: String,
    }
    impl Fixture {
        async fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let name = format!("retirement-test-{}", NEXT.fetch_add(1, Ordering::SeqCst));
            let d = tempfile::tempdir().unwrap();
            let home = d.path().join("home");
            let repo = d.path().join("repo");
            std::fs::create_dir(&repo).unwrap();
            let repo = repo.to_string_lossy().into_owned();
            git(&repo, &["init", "-b", "main"]).await.unwrap();
            git(&repo, &["config", "user.email", "test@example.invalid"])
                .await
                .unwrap();
            git(&repo, &["config", "user.name", "Fixture"])
                .await
                .unwrap();
            std::fs::write(Path::new(&repo).join("base"), "base").unwrap();
            git(&repo, &["add", "base"]).await.unwrap();
            git(&repo, &["commit", "-m", "base"]).await.unwrap();
            let remote = d.path().join("remote.git").to_string_lossy().into_owned();
            git(&repo, &["clone", "--bare", &repo, &remote])
                .await
                .unwrap();
            git(&repo, &["remote", "add", "origin", &remote])
                .await
                .unwrap();
            git(&repo, &["fetch", "origin"]).await.unwrap();
            let w = workspace::ensure(&home, &name, &repo).await.unwrap();
            std::fs::write(Path::new(&w.path).join("result"), "completed artifact").unwrap();
            git(&w.path, &["add", "result"]).await.unwrap();
            git(&w.path, &["commit", "-m", "completed"]).await.unwrap();
            git(&w.path, &["push", "origin", "HEAD:main"])
                .await
                .unwrap();
            let head = git(&w.path, &["rev-parse", "HEAD"]).await.unwrap();
            std::fs::create_dir_all(home.join("sessions")).unwrap();
            std::fs::write(
                home.join(format!("sessions/{name}.env")),
                "CC_EPHEMERAL=1\nCC_PARENT=parent\n",
            )
            .unwrap();
            std::fs::write(
                home.join(format!("workspaces/{name}.integration.json")),
                serde_json::json!({"status":"integrated","head":head}).to_string(),
            )
            .unwrap();
            let store = Arc::new(crate::db::Store::open(&d.path().join("board.db")).unwrap());
            let state = AppState {
                store,
                started: std::time::Instant::now(),
                build_hash: "test".into(),
                auth_token: None,
                reconciled: Arc::new(AtomicBool::new(true)),
            };
            let f = Self {
                _dir: d,
                name,
                home,
                state,
                w,
                head,
            };
            f.sql("INSERT INTO issues(id,title,session,status,type,created,updated) VALUES('C-1','completed','child','verified','code',1,1)");
            f
        }
        fn env(&self) -> std::path::PathBuf {
            self.home.join(format!("sessions/{}.env", self.name))
        }
        fn sql(&self, s: &str) {
            let s = s.replace("'child'", &format!("'{}'", self.name));
            self.state
                .store
                .write(move |c| {
                    c.execute_batch(&s)?;
                    Ok(crate::db::WriteOutcome {
                        applied: true,
                        events: vec![],
                    })
                })
                .unwrap();
        }
        async fn retire(&self, f: &TestFleet) -> Result<Outcome, String> {
            retire(&self.state, f, &self.home, &self.name).await
        }
        fn kept(&self) {
            assert!(self.env().exists());
            assert!(Path::new(&self.w.path).join("result").exists());
        }
    }

    #[test]
    fn project_superseded_packet_history_unblocks_only_verified_retirement() {
        use crate::project_execution::{planner, store};
        let (_dir, db, _) = crate::project_execution::outputs::tests::fixture();
        db.write(|c| {
            let row=crate::db::board_store::get_issue(c,"A")?.unwrap();
            let mut current=planner::execution(c,"A").unwrap();
            let old:String=c.query_row("SELECT json_extract(data,'$.execution.delivery_id') FROM session_events WHERE type='project.claimed' AND json_extract(data,'$.task')='A' ORDER BY id LIMIT 1",[],|r|r.get(0))?;
            assert_ne!(old,current.delivery_id);
            c.execute("INSERT INTO steering_queue(id,session,text,queued_at,guard) VALUES(?1,?2,'old unsent packet',1,'project-execution')",rusqlite::params![old,current.worker])?;
            // Normal recovery is not retirement, even after stale input settles.
            assert!(verified_board(c,&current.worker)?.is_none());
            current.stage="verified".into();current.waiting=None;
            c.execute("UPDATE issues SET status='verified' WHERE id='A'",[])?;
            planner::save_execution(c,&row,&current,"project.verified").map_err(store::sql_error)?;
            assert!(verified_board(c,&current.worker)?.is_none(),"old pending packet really blocks retirement");
            assert!(planner::settle_superseded_packets(c)?.applied);
            assert!(verified_board(c,&current.worker)?.is_some(),"non-sent settlement releases existing retirement predicate");
            assert_eq!(c.query_row("SELECT text,outcome FROM steering_history WHERE id=?1",[&old],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?,("old unsent packet".into(),"void:project-execution-superseded".into()));
            c.execute("INSERT INTO steering_queue(id,session,text,queued_at,guard) VALUES('owner-note',?1,'retained owner input',2,'project-steering')",[&current.worker])?;
            assert!(!planner::settle_superseded_packets(c)?.applied);
            assert!(verified_board(c,&current.worker)?.is_none(),"owner note still blocks disposal");
            Ok(crate::db::WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
    }

    #[tokio::test]
    async fn verified_published_board_stops_idle_provider_removes_worktree_and_expires_with_history(
    ) {
        let f = Fixture::new().await;
        let fleet = TestFleet::default();
        git(&f.w.repo, &["worktree", "lock", &f.w.path])
            .await
            .unwrap();
        std::fs::create_dir_all(Path::new(&f.w.path).join(".amux")).unwrap();
        std::fs::write(
            Path::new(&f.w.path).join(".amux/project-report.json"),
            r#"{"generation":1,"input_hash":"test","report":{"head":"ignored"}}"#,
        )
        .unwrap();
        assert_eq!(f.retire(&fleet).await.unwrap(), Outcome::Expired);
        assert_eq!(fleet.stops.load(Ordering::SeqCst), 1);
        assert!(!Path::new(&f.w.path).exists());
        assert!(!crate::api::session_verbs::worktree_is_registered(&f.w.repo, &f.w.path).await);
        assert!(f.env().with_extension("env.reaped").exists());
        assert!(!f.env().exists());
        assert_eq!(
            git(&f.w.repo, &["rev-parse", &f.w.branch]).await.unwrap(),
            f.head
        );
        assert_eq!(
            f.state
                .store
                .read()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM issues WHERE status='verified'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            f.state
                .store
                .read()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM session_events WHERE type='fanout.decommissioned'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(f.retire(&fleet).await.unwrap(), Outcome::Deferred);
    }
    #[tokio::test]
    async fn project_executor_is_stopped_but_kept_until_human_review_then_expires() {
        use crate::project_execution::{acceptance, store};
        let f = Fixture::new().await;
        std::fs::write(f.env(), "CC_EPHEMERAL=1\nCC_PROJECT=review-project\n").unwrap();
        let repo = f.w.repo.clone();
        let head = f.head.clone();
        f.state.store.write(move |c| {
            c.execute("UPDATE issues SET project_group='review-project' WHERE id='C-1'", [])?;
            let policy: amux_core::project::ExecutionPolicy = serde_json::from_value(serde_json::json!({
                "repository":repo,"coordinator":{"provider":"claude","model":"haiku"},
                "executor":{"provider":"claude","model":"sonnet"},"verify_command":"true","enabled":true,
                "acceptance":{"criteria":[{"id":"owner","requirement":"Owner reviews produced artifacts",
                    "verifier":{"type":"human","id":"owner-review","instructions":"Inspect the retained task artifacts"}}]}
            })).unwrap();
            store::save(c,"review-project",0,&policy,"test").map_err(store::sql_error)?;
            let p=store::get(c,"review-project").map_err(store::sql_error)?.unwrap();
            acceptance::observe(c,&p,&head,&head,&[]).map_err(store::sql_error)?;
            Ok(crate::db::WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
        let fleet = TestFleet::default();
        assert_eq!(f.retire(&fleet).await.unwrap(), Outcome::ReviewHeld);
        assert_eq!(fleet.stops.load(Ordering::SeqCst), 1);
        assert_eq!(
            crate::config::parse_env_file(&f.env())
                .get("CC_REVIEW_HELD")
                .map(String::as_str),
            Some("1")
        );
        let held = std::fs::read_to_string(f.env()).unwrap();
        std::fs::write(
            f.env(),
            format!(
                "{held}CC_PROJECT_PAUSED=1
CC_PAUSED=1
"
            ),
        )
        .unwrap();
        f.kept();
        assert!(
            Path::new(&f.w.path).exists(),
            "review retains the exact executor checkout"
        );
        let head = f.head.clone();
        f.state.store.write(move |c| {
            let current=store::get(c,"review-project").map_err(store::sql_error)?.unwrap();
            let contract=current.policy.acceptance.as_ref().unwrap();
            let status=acceptance::status(c,&current).map_err(store::sql_error)?;
            let fp=status["fingerprint"].as_str().unwrap().to_string();
            let intent=acceptance::intent_revision(c,"review-project").map_err(store::sql_error)?;
            let receipt=serde_json::json!({"fingerprint":fp,"contract_revision":contract.revision,"main":head,
                "intent":intent,"state":"awaiting_human","finished":1.0,"results":[{"criterion":"owner","verifier":"owner-review","type":"human","state":"pending_human"}],
                "publish_gate":{"state":"passed","signature":"retirement-fixture"}});
            acceptance::record(c,"review-project",contract,&intent,&receipt).map_err(store::sql_error)?;
            acceptance::approve(c,&current,&acceptance::Approval{criterion:"owner".into(),fingerprint:fp,decision:"approve".into(),note:"Reviewed retained evidence".into()}).map_err(store::sql_error)?;
            assert_eq!(acceptance::status(c,&current).map_err(store::sql_error)?["state"],"accepted");
            Ok(crate::db::WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
        assert_eq!(f.retire(&fleet).await.unwrap(), Outcome::Expired);
        assert!(!Path::new(&f.w.path).exists());
        assert!(f.env().with_extension("env.reaped").exists());
        // Acceptance doesn't wake or spend tokens: the already-stopped executor remains stopped.
        assert_eq!(fleet.stops.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn every_card_must_be_verified_including_non_code_and_epics() {
        let f = Fixture::new().await;
        let fleet = TestFleet::default();
        for kind in ["code", "investigation", "epic"] {
            for status in [
                "backlog",
                "todo",
                "doing",
                "review",
                "done",
                "needsyou",
                "discarded",
                "cancelled",
            ] {
                f.sql(&format!("INSERT OR REPLACE INTO issues(id,title,session,status,type,created,updated) VALUES('C-2','other','child','{status}','{kind}',1,1)"));
                assert_eq!(
                    f.retire(&fleet).await.unwrap(),
                    Outcome::Deferred,
                    "{kind}/{status}"
                );
                f.kept();
            }
        }
        assert_eq!(fleet.stops.load(Ordering::SeqCst), 0);
        f.sql("DELETE FROM issues");
        assert_eq!(f.retire(&fleet).await.unwrap(), Outcome::Deferred);
    }
    #[tokio::test]
    async fn paused_archived_isolated_and_busy_workers_are_preserved() {
        let f = Fixture::new().await;
        for flag in ["CC_PAUSED", "CC_ARCHIVED", "CC_ISOLATED"] {
            std::fs::write(f.env(), format!("CC_EPHEMERAL=1\n{flag}=1\n")).unwrap();
            assert_eq!(
                f.retire(&TestFleet::default()).await.unwrap(),
                Outcome::Deferred
            );
            f.kept();
        }
        std::fs::write(f.env(), "CC_EPHEMERAL=1\n").unwrap();
        for fleet in [
            TestFleet {
                boundary: false,
                ..Default::default()
            },
            TestFleet {
                child: true,
                ..Default::default()
            },
        ] {
            assert_eq!(f.retire(&fleet).await.unwrap(), Outcome::Deferred);
            assert_eq!(fleet.stops.load(Ordering::SeqCst), 0);
            f.kept();
        }
        f.sql("INSERT INTO steering_queue(id,session,text,queued_at) VALUES('new','child','new command',1)");
        assert_eq!(
            f.retire(&TestFleet::default()).await.unwrap(),
            Outcome::Deferred
        );
        f.kept();
    }
    #[tokio::test]
    async fn dirty_untracked_and_unmerged_heads_are_never_removed() {
        let f = Fixture::new().await;
        for file in ["result", "draft"] {
            std::fs::write(Path::new(&f.w.path).join(file), "not committed").unwrap();
            assert!(f
                .retire(&TestFleet::default())
                .await
                .unwrap_err()
                .contains("uncommitted"));
            f.kept();
            if file == "result" {
                git(&f.w.path, &["restore", "result"]).await.unwrap();
            } else {
                std::fs::remove_file(Path::new(&f.w.path).join(file)).unwrap();
            }
        }
        std::fs::write(Path::new(&f.w.path).join("new"), "unmerged").unwrap();
        git(&f.w.path, &["add", "new"]).await.unwrap();
        git(&f.w.path, &["commit", "-m", "unmerged"]).await.unwrap();
        assert!(f
            .retire(&TestFleet::default())
            .await
            .unwrap_err()
            .contains("head differs"));
        f.kept();
    }
    #[tokio::test]
    async fn stale_integration_receipt_and_unreachable_remote_do_not_authorize_disposal() {
        let f = Fixture::new().await;
        // Remove the worker commit from the LOCAL TEST bare remote; tracking
        // refs and the successful receipt still claim the earlier integration.
        git(&f.w.repo, &["push", "--force", "origin", "main:main"])
            .await
            .unwrap();
        assert!(f
            .retire(&TestFleet::default())
            .await
            .unwrap_err()
            .contains("not contained"));
        f.kept();
        git(
            &f.w.repo,
            &["remote", "set-url", "origin", "/missing/test-only-remote"],
        )
        .await
        .unwrap();
        assert!(f.retire(&TestFleet::default()).await.is_err());
        f.kept();
    }
    #[tokio::test]
    async fn new_card_or_draft_arriving_while_stopping_cancels_cleanup() {
        let f = Fixture::new().await;
        let store = f.state.store.clone();
        let name = f.name.clone();
        let fleet = TestFleet {
            on_stop: Some(Box::new(move || {
                let name = name.clone();
                store.write(move|c|{c.execute("INSERT INTO issues(id,title,session,status,created,updated) VALUES('NEW','new',?1,'todo',1,1)",[&name])?;Ok(crate::db::WriteOutcome{applied:true,events:vec![]})}).unwrap();
            })),
            ..Default::default()
        };
        assert_eq!(f.retire(&fleet).await.unwrap(), Outcome::Deferred);
        f.kept();
        f.sql("DELETE FROM issues WHERE id='NEW'");
        let path = Path::new(&f.w.path).join("late-draft");
        let fleet = TestFleet {
            on_stop: Some(Box::new(move || std::fs::write(&path, "draft").unwrap())),
            ..Default::default()
        };
        assert!(f.retire(&fleet).await.unwrap_err().contains("uncommitted"));
        f.kept();
    }
    #[tokio::test]
    async fn failed_stop_or_finalization_preserves_an_active_recoverable_worker() {
        let f = Fixture::new().await;
        assert!(f
            .retire(&TestFleet {
                fail_stop: true,
                ..Default::default()
            })
            .await
            .is_err());
        f.kept();
        f.sql("CREATE TRIGGER fail_retirement BEFORE INSERT ON session_events WHEN NEW.type='fanout.decommissioned' BEGIN SELECT RAISE(ABORT,'injected finalization failure'); END");
        assert!(f.retire(&TestFleet::default()).await.is_err());
        f.kept();
        assert!(crate::api::session_verbs::worktree_is_registered(&f.w.repo, &f.w.path).await);
        f.sql("DROP TRIGGER fail_retirement");
        assert_eq!(
            f.retire(&TestFleet::default()).await.unwrap(),
            Outcome::Expired
        );
    }
    #[tokio::test]
    async fn retry_after_removal_before_expiration_and_legacy_expired_cleanup() {
        let f = Fixture::new().await;
        git(&f.w.repo, &["worktree", "remove", &f.w.path])
            .await
            .unwrap();
        assert_eq!(
            f.retire(&TestFleet::default()).await.unwrap(),
            Outcome::Expired
        );
        let legacy = Fixture::new().await;
        std::fs::rename(legacy.env(), legacy.env().with_extension("env.reaped")).unwrap();
        assert_eq!(
            legacy.retire(&TestFleet::default()).await.unwrap(),
            Outcome::Expired
        );
        assert!(!Path::new(&legacy.w.path).exists());
    }
}
