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
    Expired,
}

fn enabled(env: &std::collections::BTreeMap<String, String>) -> bool {
    env.get("CC_EPHEMERAL").is_some_and(|v| v == "1")
        && ["CC_PAUSED", "CC_ARCHIVED", "CC_ISOLATED"]
            .iter()
            .all(|k| env.get(*k).is_none_or(|v| v != "1"))
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
    if !enabled(&crate::config::parse_env_file(&source)) || fleet.is_isolated(name) {
        return Ok(Outcome::Deferred);
    }
    let Some(snapshot) = board(state, name)? else {
        return Ok(Outcome::Deferred);
    };
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
    let w = workspace::load(home, name).ok_or("verified worker has no workspace record")?;
    let head = receipt["head"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or("integration receipt has no head")?
        .to_string();
    // A corrupt/mispointed record must never delete a shared checkout.
    let expected = home.join("worktrees").join(name);
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
    fleet.stop_for_retirement(name).await?;
    if fleet.is_running(name).await {
        return Err("provider did not stop; workspace retained".into());
    }
    if !still_current()? {
        return Ok(Outcome::Deferred);
    }
    check_checkout(&w, &head).await?;
    // Only our durable worktree lock is released. Never force, rm -rf, or
    // globally prune registrations: new drafts must make Git refuse removal.
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
    if !git(&w.path, &["status", "--porcelain", "--untracked-files=all"])
        .await?
        .is_empty()
    {
        return Err("workspace has uncommitted or untracked work; preserved".into());
    }
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

    #[tokio::test]
    async fn verified_published_board_stops_idle_provider_removes_worktree_and_expires_with_history(
    ) {
        let f = Fixture::new().await;
        let fleet = TestFleet::default();
        git(&f.w.repo, &["worktree", "lock", &f.w.path])
            .await
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
