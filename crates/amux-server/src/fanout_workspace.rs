//! Durable, per-worker git workspaces. Stopping a process never disposes work.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workspace {
    pub repo: String,
    pub path: String,
    pub branch: String,
    pub base: String,
}

/// Configured repository aliases and Git's recorded canonical root share identity.
/// Failed resolution never makes different paths equivalent.
pub(crate) fn same_repository(left: &str, right: &str) -> bool {
    left == right
        || matches!((std::fs::canonicalize(left), std::fs::canonicalize(right)),
        (Ok(left), Ok(right)) if left == right)
}

pub(crate) fn status_without_harness_receipts(raw: &str) -> String {
    raw.lines()
        .filter(|line| {
            let path = line.get(3..).unwrap_or(line).trim();
            !matches!(
                path,
                ".amux/project-report.json" | ".amux/project-wait.json" | ".amux/project-required-outputs.json"
            ) && !path.ends_with(" -> .amux/project-report.json")
                && !path.ends_with(" -> .amux/project-wait.json")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) async fn project_clean_status(repo: &str) -> Result<String, String> {
    git(repo, &["status", "--porcelain", "--untracked-files=all"])
        .await
        .map(|raw| status_without_harness_receipts(&raw))
}

pub(crate) async fn git(repo: &str, args: &[&str]) -> Result<String, String> {
    let mut argv = vec!["-C", repo];
    argv.extend_from_slice(args);
    // Publishing runs the repository's pre-push gate. Keep ordinary reads bounded,
    // while allowing that gate to finish before deciding whether the push passed.
    let timeout = if args.first() == Some(&"push") {
        Duration::from_secs(1800)
    } else {
        Duration::from_secs(120)
    };
    let out = crate::api::session_verbs::run_cmd("git", &argv, timeout)
        .await
        .ok_or_else(|| {
            format!(
                "git {} timed out after {}s or failed to start",
                args.first().unwrap_or(&""),
                timeout.as_secs()
            )
        })?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr)
            .chars()
            .take(2000)
            .collect());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn record_path(home: &Path, name: &str) -> PathBuf {
    home.join("workspaces").join(format!("{name}.json"))
}
pub(crate) fn expected_path(repo: &str, name: &str) -> PathBuf {
    Path::new(repo).join(".worktrees").join(name)
}
pub fn load(home: &Path, name: &str) -> Option<Workspace> {
    serde_json::from_slice(&std::fs::read(record_path(home, name)).ok()?).ok()
}
pub(crate) fn save(home: &Path, name: &str, workspace: &Workspace) -> Result<(), String> {
    let p = record_path(home, name);
    std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(workspace).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(tmp, p).map_err(|e| e.to_string())
}

/// The commit an ADOPTED workspace's history is known to descend from.
///
/// Never returns an empty string: a caller that cannot name a base must fail
/// at creation, where the cause is still visible, rather than persist a record
/// that can never integrate (AMUX-4921).
///
/// The fork point with `origin/main` is preferred because it is the weakest
/// true statement about this history, so the ancestry guard stays meaningful.
/// When there is no common ancestor, or no `origin/main` to compare against,
/// the workspace's own HEAD is still a real commit its history descends from,
/// and recording it catches any later rewrite that drops the adopted work.
async fn adopted_base(repo: &str, path: &str) -> Result<String, String> {
    let head = git(path, &["rev-parse", "HEAD"]).await?;
    if head.is_empty() {
        return Err("adopted workspace has no HEAD to derive a creation base from".into());
    }
    match git(repo, &["merge-base", &head, "origin/main"]).await {
        Ok(fork) if !fork.is_empty() => Ok(fork),
        _ => Ok(head),
    }
}

/// Called under the worker operation lock, before its provider is launched.
/// Existing files/index/commits are retained even after an interrupted start.
pub async fn ensure(home: &Path, name: &str, configured_repo: &str) -> Result<Workspace, String> {
    ensure_named(home, name, configured_repo, &format!("amux/fanout/{name}")).await
}

/// A project owns its branch independently of worker attempt identities.
pub(crate) async fn ensure_named(home: &Path, name: &str, configured_repo: &str, branch: &str) -> Result<Workspace, String> {
    if !crate::api::session_verbs::valid_session_name(name) {
        return Err("invalid worker name".into());
    }
    let old = load(home, name);
    let repo = old
        .as_ref()
        .map(|w| w.repo.as_str())
        .unwrap_or(configured_repo);
    let repo = git(repo, &["rev-parse", "--show-toplevel"]).await?;
    let path = expected_path(&repo, name).to_string_lossy().into_owned();
    let branch = branch.to_string();
    let existing = Path::new(&path).join(".git").exists();
    // THE RECORDED BASE IS THE ANCESTRY GUARD'S ONLY ANCHOR (AMUX-4921), so an
    // empty one disables that guard for the life of the workspace and blocks
    // automatic integration forever. This used to be reachable two ways, and
    // the second one is not legacy at all:
    //
    //   1. adopting a worktree that predates base tracking, and
    //   2. a RACE. `save` lands at the END of this function, after `worktree
    //      add`, so a second ensure() for the same worker inside that window
    //      sees the directory without the record and cannot tell itself apart
    //      from case 1. Measured 2026-09-20: three workers from ONE launch
    //      call, two with a sha and one empty.
    //
    // Both now resolve to a MEASURED commit. The merge-base of the workspace
    // HEAD with origin/main is the fork point its history actually descends
    // from: evidence rather than an invention, and the weakest claim that
    // still lets the guard catch a later rewrite. Anchoring it HERE is the
    // point. Re-deriving it at integration time instead would make the guard
    // vacuous, because a fork point is always an ancestor of the head it was
    // computed from.
    //
    // An empty RECORDED base is also repaired rather than copied forward, so
    // a workspace stranded by the old path recovers on its next adoption
    // instead of stalling permanently.
    let recorded = old
        .as_ref()
        .map(|w| w.base.clone())
        .filter(|b| !b.is_empty());
    let base = match recorded {
        Some(b) => b,
        None if existing => adopted_base(&repo, &path).await?,
        None => git(&repo, &["rev-parse", "--verify", "origin/main"])
            .await
            .or(git(&repo, &["rev-parse", "HEAD"]).await)?,
    };
    if existing {
        let common = git(
            &repo,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?;
        let actual = git(
            &path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?;
        if std::fs::canonicalize(common).map_err(|e| e.to_string())?
            != std::fs::canonicalize(actual).map_err(|e| e.to_string())?
        {
            return Err("existing workspace belongs to a different repository; preserved".into());
        }
        // A .git file alone did not detect interrupted checkouts. An empty
        // index over a nonempty commit must be recovered, never launched.
        if git(&path, &["ls-files"]).await?.is_empty()
            && !git(&path, &["ls-tree", "--name-only", "HEAD"])
                .await?
                .is_empty()
        {
            return Err("workspace index is empty over a nonempty commit; preserve and recover the interrupted checkout".into());
        }
        let current = git(&path, &["branch", "--show-current"]).await?;
        if current.is_empty() {
            // Adoption only names the current commit. Checkout can invoke
            // filters/hooks and rewrite files even when HEAD stays the same.
            let head = git(&path, &["rev-parse", "HEAD"]).await?;
            let reference = format!("refs/heads/{branch}");
            if let Ok(existing_head) = git(&repo, &["rev-parse", "--verify", &reference]).await {
                let worktrees = git(&repo, &["worktree", "list", "--porcelain"]).await?;
                if existing_head != head
                    || worktrees
                        .lines()
                        .any(|line| line == format!("branch {reference}"))
                {
                    return Err(
                        "workspace branch already belongs to another head or checkout; preserved"
                            .into(),
                    );
                }
            } else {
                git(&repo, &["branch", &branch, &head]).await?;
            }
            if git(&path, &["rev-parse", "HEAD"]).await? != head {
                return Err("workspace changed during adoption; preserved for retry".into());
            }
            git(&path, &["symbolic-ref", "HEAD", &reference]).await?;
        } else if current != branch {
            return Err(format!(
                "workspace uses {current}, expected {branch}; preserved"
            ));
        }
    } else {
        if Path::new(&path).exists() {
            return Err(
                "workspace directory exists without a valid git registration; preserved".into(),
            );
        }
        std::fs::create_dir_all(Path::new(&repo).join(".worktrees")).map_err(|e| e.to_string())?;
        // Only remove the missing path's stale registration. Branch commits
        // survive; never prune registrations belonging to other workers.
        let _ = git(&repo, &["worktree", "unlock", &path]).await;
        let _ = git(&repo, &["worktree", "remove", &path]).await;
        if git(
            &repo,
            &["show-ref", "--verify", &format!("refs/heads/{branch}")],
        )
        .await
        .is_ok()
        {
            git(&repo, &["worktree", "add", &path, &branch]).await?;
        } else {
            git(&repo, &["worktree", "add", "-b", &branch, &path, &base]).await?;
        }
        if !Path::new(&path).join(".git").exists()
            || !git(&path, &["status", "--porcelain"]).await?.is_empty()
        {
            return Err("new workspace did not materialize cleanly; preserved for recovery".into());
        }
    }
    let workspace = Workspace {
        repo,
        path,
        branch,
        base,
    };
    save(home, name, &workspace)?;
    if integration_status(home, name)["status"] == "workspace_requires_recovery" {
        write_integration_status(
            home,
            name,
            &serde_json::json!({
                "status":"workspace_ready","detail":"Workspace recovered; continuing its owned board",
                "at":crate::config::now_f64(),"worktree":workspace.path,"branch":workspace.branch
            }),
        );
    }
    tracing::info!(session=name,worktree=%workspace.path,branch=%workspace.branch,reused=existing,
        verdict="fanout_workspace_ready","durable fan-out workspace ready");
    Ok(workspace)
}

/// Keep the child waitable until its pipes close so cancellation can stop the
/// process group without risking a recycled PID. Retain only bounded output.
struct OwnedCommand(tokio::process::Child);
impl Drop for OwnedCommand {
    fn drop(&mut self) {
        if let Some(pid) = self.0.id() {
            // SAFETY: this command owns a new process group whose leader has
            // not been reaped. No unrelated worker shares this group.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}
async fn output_tail(mut pipe: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut tail = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = pipe.read(&mut buf).await?;
        if n == 0 {
            return Ok(tail);
        }
        tail.extend_from_slice(&buf[..n]);
        if tail.len() > 32768 {
            tail.drain(..tail.len() - 32768);
        }
    }
}
pub(crate) async fn checked_command<F: Fn() -> Result<(), String>>(
    mut cmd: tokio::process::Command,
    permit: &F,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, String), String> {
    permit()?;
    cmd.process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut owned = OwnedCommand(cmd.spawn().map_err(|e| e.to_string())?);
    let stdout = owned.0.stdout.take().unwrap();
    let stderr = owned.0.stderr.take().unwrap();
    let deadline = tokio::time::Instant::now() + timeout;
    let output = async { tokio::try_join!(output_tail(stdout), output_tail(stderr)) };
    tokio::pin!(output);
    let (out, err) = loop {
        tokio::select! {
            result=&mut output => break result.map_err(|e|e.to_string())?,
            _=tokio::time::sleep_until(deadline) => return Err(format!("Command timed out after {} seconds",timeout.as_secs())),
            _=tokio::time::sleep(Duration::from_millis(250)) => permit()?,
        }
    };
    let status = loop {
        tokio::select! {
            result=owned.0.wait() => break result.map_err(|e|e.to_string())?,
            _=tokio::time::sleep_until(deadline) => return Err(format!("Command timed out after {} seconds",timeout.as_secs())),
            _=tokio::time::sleep(Duration::from_millis(250)) => permit()?,
        }
    };
    Ok((
        status,
        format!(
            "{}{}",
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(&err)
        ),
    ))
}

async fn integration_git<F: Fn() -> Result<(), String>>(
    workspace: &Workspace,
    repo: &str,
    args: &[&str],
    permit: &F,
) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(repo).args(args).env(
        "AMUX_SESSION",
        workspace.branch.trim_start_matches("amux/fanout/"),
    );
    // Repository pre-push hooks run the actual content gates. They routinely
    // take longer than a network-only Git operation, but remain cancellable
    // as soon as lifecycle/admission changes.
    let timeout = if args.first() == Some(&"push") {
        Duration::from_secs(1800)
    } else {
        Duration::from_secs(120)
    };
    let (status, output) = checked_command(cmd, permit, timeout).await?;
    if !status.success() {
        return Err(output);
    }
    Ok(output.trim().to_string())
}

/// Integrate an immutable worker head in a separate checkout. The shared main
/// checkout is never switched, reset or merged into. A competing remote push
/// rebuilds a fresh candidate and reruns validation without asking the model
/// to resolve ordinary Git contention.
const MAIN_ADVANCED_RETRY: &str = "Remote main advanced through three integration attempts; the harness will retry automatically with a fresh candidate";

/// Catch source-checkout references that accidentally validate stale bytes.
/// This is a configuration guard, not a shell sandbox: validation scripts must
/// still use candidate-relative source paths, including inside invoked scripts.
pub(crate) fn validate_verification_command(
    workspace: &Workspace,
    command: &str,
) -> Result<(), String> {
    if command.contains("$(") || command.contains('`') {
        return Err("verification commands must be static candidate-relative commands; put dynamic logic in a committed script and call that script".into());
    }
    if command.contains(".amux/") || command.split_whitespace().any(|part| part == ".amux") {
        return Err("verification commands cannot depend on .amux receipt files; report receipts are harness plumbing, not committed candidate evidence".into());
    }
    for source in [&workspace.path, &workspace.repo] {
        let mut spellings = vec![source.clone()];
        if let Ok(path) = std::fs::canonicalize(source) {
            spellings.push(path.to_string_lossy().into_owned());
        }
        if let Some(home) = std::env::var_os("HOME") {
            if let Ok(relative) = Path::new(source).strip_prefix(Path::new(&home)) {
                for prefix in ["~", "$HOME", "${HOME}"] {
                    spellings.push(format!("{prefix}/{}", relative.display()));
                }
            }
        }
        if spellings
            .iter()
            .filter(|s| !s.is_empty())
            .any(|s| command.contains(s.as_str()))
        {
            tracing::warn!(session=%workspace.branch.trim_start_matches("amux/fanout/"),
                verdict="fanout_verification_source_path", "validation references the original source checkout");
            return Err("worktree_verify references the original worker or shared checkout. Use source paths relative to the merged candidate (for example: cd server && python -m pytest tests); remove fallback cd commands. Use a runtime installed outside those source checkouts if needed.".into());
        }
    }
    Ok(())
}

/// Shared semantics for source and merged candidates: preflight ALL distinct
/// commands before any process, then run each with its own bounded timeout.
pub(crate) fn distinct_verification_commands<'a>(
    commands: impl IntoIterator<Item = &'a str>,
) -> Vec<&'a str> {
    let mut seen = std::collections::HashSet::new();
    commands.into_iter().filter(|c| seen.insert(*c)).collect()
}
pub(crate) async fn verification_tool_path<F:Fn()->Result<(),String>>(permit:&F)->Result<String,String> {
    let shell=std::env::var("SHELL").unwrap_or_else(|_|"/bin/sh".into());
    let mut probe=tokio::process::Command::new(shell);
    probe.args(["-lc","printf '\\nAMUX_VERIFY_PATH=%s\\n' \"$PATH\""]);
    let (status,output)=checked_command(probe,permit,Duration::from_secs(15)).await?;
    if !status.success() {return Err("verification tool environment discovery failed".into())}
    output.lines().rev().find_map(|line|line.strip_prefix("AMUX_VERIFY_PATH=")).filter(|path|!path.is_empty()).map(str::to_owned).ok_or_else(||"verification tool environment returned no PATH".into())
}

pub(crate) fn verification_process(tool_path:&str,candidate:&str,command:&str)->tokio::process::Command {
    // Discover tools once before checks. Profiles cannot change the candidate
    // cwd or consume each command's verification budget.
    let mut process=tokio::process::Command::new("sh");
    process.args(["-c",command]).current_dir(candidate).env("PATH",tool_path);
    process
}

pub(crate) async fn verify_commands<F: Fn() -> Result<(), String>>(
    workspace: &Workspace,
    candidate: &str,
    commands: &[&str],
    timeout: Duration,
    permit: &F,
) -> Result<(), String> {
    verify_commands_with_cleanup(workspace, candidate, commands, timeout, permit, || async {
        Ok(())
    })
    .await
}

/// Run a verifier's output cleanup before testing candidate cleanliness. Some
/// approved runtime checks write declared diagnostic files even on failure;
/// callers may retain those outside Git without treating them as source edits.
pub(crate) async fn verify_commands_with_cleanup<F, C, Fut>(
    workspace: &Workspace,
    candidate: &str,
    commands: &[&str],
    timeout: Duration,
    permit: &F,
    cleanup: C,
) -> Result<(), String>
where
    F: Fn() -> Result<(), String>,
    C: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    if !(Duration::from_secs(1)
        ..=Duration::from_secs(amux_core::project::MAX_VERIFICATION_TIMEOUT_SECS))
        .contains(&timeout)
    {
        return Err("verification timeout must be 1..3600 seconds".into());
    }
    let commands = distinct_verification_commands(commands.iter().copied());
    if commands.is_empty() || commands.iter().any(|c| c.trim().is_empty()) {
        return Err("verification command is required".into());
    }
    for command in &commands {
        validate_verification_command(workspace, command)?;
    }
    permit()?;
    cleanup().await?;
    let head = git(candidate, &["rev-parse", "HEAD"]).await?;
    if !project_clean_status(candidate).await?.is_empty() {
        return Err("worktree has uncommitted changes".into());
    }
    let tool_path=verification_tool_path(permit).await?;
    for command in commands {
        let mut cmd = verification_process(&tool_path,candidate,command);
        cmd.env(
            "AMUX_SESSION",
            workspace.branch.trim_start_matches("amux/fanout/"),
        );
        let started = std::time::Instant::now();
        let result = checked_command(cmd, permit, timeout).await;
        tracing::info!(
            candidate,
            command,
            timeout_secs = timeout.as_secs(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            measured = true,
            n_considered = 1,
            ok = result.as_ref().is_ok_and(|(s, _)| s.success()),
            verdict = "candidate_verification_command",
            "bounded candidate check completed"
        );
        let cleanup_result = cleanup().await;
        let (status, output) = result?;
        cleanup_result?;
        if !status.success() {
            return Err(format!(
                "verification failed ({command}): candidate validation exited {}. {output}",
                status.code().unwrap_or(-1)
            ));
        }
        if git(candidate, &["rev-parse", "HEAD"]).await? != head
            || !project_clean_status(candidate).await?.is_empty()
        {
            return Err("verification changed the reported worktree".into());
        }
    }
    permit()
}

pub async fn integrate<F: Fn() -> Result<(), String>>(
    workspace: &Workspace,
    verification: &str,
    permit: F,
) -> Result<String, String> {
    integrate_checks(
        workspace,
        &[verification],
        Duration::from_secs(amux_core::project::verification_timeout_default()),
        permit,
    )
    .await
}

pub(crate) async fn integrate_checks<F: Fn() -> Result<(), String>>(
    workspace: &Workspace,
    verification: &[&str],
    timeout: Duration,
    permit: F,
) -> Result<String, String> {
    let head = git(&workspace.path, &["rev-parse", "HEAD"]).await?;
    for attempt in 1..=3 {
        permit()?;
        if git(&workspace.path, &["rev-parse", "HEAD"]).await? != head {
            return Err("Worker workspace changed during integration; integration deferred".into());
        }
        if let Some(merged) =
            integrate_attempt(workspace, verification, timeout, &head, &permit).await?
        {
            return Ok(merged);
        }
        tracing::info!(session=%workspace.branch.trim_start_matches("amux/fanout/"), attempt,
            verdict="fanout_main_advanced_retry", "remote main advanced; rebuilding and revalidating the candidate");
    }
    Err(MAIN_ADVANCED_RETRY.into())
}

async fn integrate_attempt<F: Fn() -> Result<(), String>>(
    workspace: &Workspace,
    verification: &[&str],
    timeout: Duration,
    head: &str,
    permit: &F,
) -> Result<Option<String>, String> {
    permit()?;
    if !project_clean_status(&workspace.path).await?.is_empty() {
        return Err("Commit and verify the remaining workspace changes before integration".into());
    }
    git(&workspace.repo, &["fetch", "origin", "main"]).await?;
    let main = git(&workspace.repo, &["rev-parse", "origin/main"]).await?;
    if git(
        &workspace.repo,
        &["merge-base", "--is-ancestor", head, &main],
    )
    .await
    .is_ok()
    {
        return Ok(Some(main));
    }
    if workspace.base.is_empty() {
        // NAME WHICH OF THE TWO SITUATIONS THIS IS (AMUX-4921). This used to
        // say "Legacy workspace", which sent every reader toward migration or
        // manual reconciliation. It was wrong for the case that actually
        // happened: a worker created minutes earlier by the current code,
        // emptied by a race in `ensure`. The two are now distinguishable,
        // because since that fix the creation path CANNOT persist an empty
        // base, so an empty one here can only predate the fix.
        let created = std::fs::metadata(Path::new(&workspace.path).join(".git"))
            .and_then(|m| m.modified())
            .ok()
            .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339())
            .unwrap_or_else(|| "unknown".into());
        return Err(format!(
            "workspace has no recorded creation base, so its history cannot be checked for a \
             rewrite and automatic integration will not run. This record PREDATES the AMUX-4921 \
             fix: the creation path can no longer persist an empty base, it fails at creation \
             instead. Worktree {} created {created}, branch {}. RECOVERY: the next adoption \
             re-derives the base from the merge-base of its HEAD with origin/main, so restarting \
             this worker clears the stall without reconciling anything by hand.",
            workspace.path, workspace.branch
        ));
    }
    git(
        &workspace.repo,
        &["merge-base", "--is-ancestor", &workspace.base, head],
    )
    .await
    .map_err(|_| {
        "Workspace history no longer descends from its recorded base; reconcile it locally"
            .to_string()
    })?;
    if verification.is_empty() || verification.iter().any(|c| c.trim().is_empty()) {
        return Err("Set CC_WORKTREE_VERIFY to the repository's validation command; the harness will run it on the merged candidate".into());
    }
    for command in verification {
        validate_verification_command(workspace, command)?;
    }
    let temp = tempfile::Builder::new()
        .prefix("amux-integrate-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    let candidate = temp.path().join("candidate").to_string_lossy().into_owned();
    git(
        &workspace.repo,
        &["worktree", "add", "--detach", &candidate, &main],
    )
    .await?;
    let result=async {
        integration_git(workspace,&candidate,&["merge","--no-ff","--no-edit",head],permit).await
            .map_err(|e|format!("Integration conflict: rebase the worker branch on origin/main and resolve it there. {e}"))?;
        let merged=git(&candidate,&["rev-parse","HEAD"]).await?;
        // The configured command is run from the merged checkout. Existing git
        // hooks remain enabled, including the repository's pre-push gates.
        verify_commands(workspace,&candidate,verification,timeout,permit).await?;
        if git(&candidate,&["rev-parse","HEAD"]).await?!=merged
            || !project_clean_status(&candidate).await?.is_empty() {
            return Err("Validation modified the candidate; commit the required changes in the worker workspace".into());
        }
        permit()?;
        if git(&workspace.path,&["rev-parse","HEAD"]).await?!=head
            || !project_clean_status(&workspace.path).await?.is_empty() {
            return Err("Worker workspace changed during validation; integration deferred".into());
        }
        // Detect a stale parent BEFORE an expensive pre-push hook. A remote
        // race after this check is handled the same way, using observed refs
        // rather than parsing a hook's possibly misleading error text.
        git(&workspace.repo,&["fetch","origin","main"]).await?;
        if git(&workspace.repo,&["rev-parse","origin/main"]).await? != main {
            return Ok(None);
        }
        if let Err(error) = integration_git(workspace,&candidate,&["push","origin",&format!("{merged}:refs/heads/main")],permit).await {
            permit()?;
            git(&workspace.repo,&["fetch","origin","main"]).await?;
            if git(&workspace.repo,&["rev-parse","origin/main"]).await? != main {
                return Ok(None);
            }
            return Err(error);
        }
        // Exact remote read-back; another successful merge may already follow us.
        git(&workspace.repo,&["fetch","origin","main"]).await?;
        git(&workspace.repo,&["merge-base","--is-ancestor",&merged,"origin/main"]).await?;
        Ok(Some(merged))
    }.await;
    // Only this temporary candidate is disposable. Worker files and branches
    // survive failed tests, conflicts, rejected pushes and process restarts.
    let _ = git(
        &workspace.repo,
        &["worktree", "remove", "--force", &candidate],
    )
    .await;
    result
}

pub fn integration_status(home: &Path, name: &str) -> serde_json::Value {
    std::fs::read(
        home.join("workspaces")
            .join(format!("{name}.integration.json")),
    )
    .ok()
    .and_then(|b| serde_json::from_slice(&b).ok())
    .unwrap_or(serde_json::Value::Null)
}

/// Did this integration receipt say the work reached origin/main? (AMUX-4956)
///
/// `integrated` is the only status that means landed. Everything else —
/// `integrating`, `requires_work`, `workspace_ready`,
/// `workspace_requires_recovery`, and an ABSENT receipt — means the commits are
/// still sitting in a worktree.
///
/// Measured 2026-09-23 on this box: of 16 receipts, 6 are `integrated`. Ten
/// fan-out workers had not landed, while their cards read `done`.
pub fn integration_landed(receipt: &serde_json::Value) -> bool {
    receipt["status"] == "integrated"
}

/// What a terminal fan-out card should say about its own delivery, or `None`
/// when there is nothing to say (AMUX-4956).
///
/// DERIVED, NEVER STAMPED. A marker written at the `done` transition goes stale
/// the moment integration later succeeds, turning a true warning into a false
/// one — the card that asked for this named that hazard first. Deriving from
/// the receipt means the note disappears by itself when the work lands, with no
/// resolution path to get wrong.
///
/// NOT A REFUSAL, also deliberately. Integration can be impossible for reasons
/// outside the worker's control (AMUX-4921 was live proof: a workspace whose
/// creation base was empty could never integrate). A gate with no truthful exit
/// is the ethos rule 3 failure this board keeps repairing, so this reports and
/// does not block.
pub fn not_landed_note(receipt: &serde_json::Value) -> Option<serde_json::Value> {
    if integration_landed(receipt) {
        return None;
    }
    let status = receipt["status"].as_str().unwrap_or("absent");
    Some(serde_json::json!({
        "landed": false,
        "integration_status": status,
        "detail": receipt["detail"].as_str().unwrap_or(""),
        "why": "this card is terminal but its commits are not on origin/main.                 `done` means implemented, not delivered.",
    }))
}

/// Stable output identity for verification, independent of retries and receipt
/// timestamps. Failed or in-flight integration never replaces a successful head.
pub(crate) fn integrated_head(
    conn: &rusqlite::Connection,
    name: &str,
) -> rusqlite::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT json_extract(data,'$.head') FROM session_events WHERE session=?1 \
         AND type='fanout.integrated' ORDER BY id DESC LIMIT 1",
        [name],
        |row| row.get(0),
    )
    .optional()
    .map(Option::flatten)
}

/// Backfill an existing successful receipt as well as recording new integrations.
/// Restart/retry of the same head is a no-op; a new head re-arms verification.
pub(crate) async fn record_integrated_head(store: &crate::db::SharedStore, name: &str, head: &str) {
    if head.is_empty() {
        return;
    }
    let (worker, head) = (name.to_string(), head.to_string());
    let result = store.write_async(move |conn| {
        if integrated_head(conn, &worker)?.as_deref() == Some(head.as_str()) {
            return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
        }
        conn.execute(
            "INSERT INTO session_events(ts,session,type,data,source) VALUES(?1,?2,'fanout.integrated',?3,'board-drive')",
            rusqlite::params![crate::config::now_f64(), worker, serde_json::json!({"head":head}).to_string()],
        )?;
        tracing::info!(session=%worker,head=%head,measured=true,n_considered=1,
            verdict="integration_rearms_verification","integrated output changed; verification may resume immediately");
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await;
    if let Err(error) = result {
        tracing::warn!(session=name,%error,verdict="integration_verification_receipt_retry",
            "could not record integrated output; the next board boundary retries");
    }
}

pub(crate) fn write_integration_status(home: &Path, name: &str, record: &serde_json::Value) {
    let path = home
        .join("workspaces")
        .join(format!("{name}.integration.json"));
    let tmp = path.with_extension("json.tmp");
    // A failed first checkout has no workspace record yet. Its recovery state
    // must still survive restart and appear in the orchestration projection.
    let result = std::fs::create_dir_all(path.parent().unwrap())
        .and_then(|()| std::fs::write(&tmp, serde_json::to_vec_pretty(record)?))
        .and_then(|()| std::fs::rename(tmp, path));
    if let Err(error) = result {
        tracing::error!(session=name,%error,verdict="fanout_status_write_failed", "could not persist fan-out integration state");
    }
}

/// Verification certifies the current artifact, not the worker's acknowledgement.
/// Run Git outside the SQLite writer; the caller binds this observation to its
/// card revision before changing state. Ordinary workers retain their own gates.
/// AMUX-4922. `done` on an EPHEMERAL fan-out worker, whose worktree is deleted
/// when it retires.
///
/// DELIBERATELY NOT verification_ready's gate, and the difference is the whole
/// design. This does NOT require an integration receipt. Integration can be
/// impossible for reasons outside the worker's control, and a gate with no
/// truthful exit is the ethos rule 3 failure this board keeps repairing.
/// AMUX-4921 was exactly such a reason and was live while this was written: a
/// workspace recorded an empty creation base, so its integration could never
/// succeed no matter what the worker did.
///
/// What it DOES refuse is the half the worker can always fix in one command:
/// UNCOMMITTED changes. Measured 2026-09-20 on the fan-out lifecycle, a worker
/// marked its card `done` with `M docs/reference/diagnostics.md` still modified
/// in its worktree. Not merely unmerged, uncommitted, in a directory scheduled
/// for deletion. That `done` was a claim about work that was about to become
/// both unverifiable and gone.
///
/// Silent OK when there is no workspace record or no worktree on disk: there is
/// nothing truthful to measure, and refusing on an absence would be a gate with
/// no exit of a different kind.
pub(crate) async fn done_ready(name: &str) -> Result<(), String> {
    let env = crate::api::session_verbs::parse_env(name);
    if env.get("CC_EPHEMERAL") != Some("1") || env.get("CC_WORKTREE_AUTO_MERGE") == Some("0") {
        return Ok(());
    }
    let home = crate::config::amux_home();
    let Some(workspace) = load(&home, name) else {
        return Ok(());
    };
    if !Path::new(&workspace.path).join(".git").exists() {
        return Ok(());
    }
    let dirty = git(&workspace.path, &["status", "--porcelain"]).await?;
    if dirty.is_empty() {
        return Ok(());
    }
    let named = dirty.lines().take(5).collect::<Vec<_>>().join("; ");
    let more = dirty.lines().count().saturating_sub(5);
    Err(format!(
        "This worker is ephemeral, so its worktree is deleted when it retires, and it has          uncommitted changes: `done` would claim work whose only copy is scheduled for          destruction. Commit them, then move the card. Uncommitted: {named}{}",
        if more > 0 { format!(" (+{more} more)") } else { String::new() }
    ))
}

pub(crate) async fn verification_ready(name: &str) -> Result<(), String> {
    let env = crate::api::session_verbs::parse_env(name);
    // An explicit manual-integration configuration retains its existing gates.
    // Requiring a receipt from a controller the owner disabled has no exit.
    if env.get("CC_EPHEMERAL") != Some("1") || env.get("CC_WORKTREE_AUTO_MERGE") == Some("0") {
        return Ok(());
    }
    let home = crate::config::amux_home();
    let workspace = load(&home, name)
        .ok_or("Fan-out workspace is not recorded; recover this worker's own workspace first")?;
    let record = integration_status(&home, name);
    if record["status"] != "integrated" {
        return Err(format!(
            "Fan-out integration is not complete: {}",
            record["detail"]
                .as_str()
                .unwrap_or("no successful integration receipt")
        ));
    }
    let head = git(&workspace.path, &["rev-parse", "HEAD"]).await?;
    if record["head"].as_str() != Some(head.as_str()) {
        return Err("The integration receipt covers an older worktree head; integrate the current commit first".into());
    }
    if !project_clean_status(&workspace.path).await?.is_empty() {
        return Err("The worktree has uncommitted changes; preserve, commit and integrate them before verification".into());
    }
    Ok(())
}

fn ready_board(state: &crate::api::AppState, name: &str) -> Result<(String, String, i64), String> {
    let env = crate::api::session_verbs::parse_env(name);
    if env.get("CC_PROJECT").is_some()
        || env.get("CC_EPHEMERAL") != Some("1")
        || env.get("CC_PAUSED") == Some("1")
        || env.get("CC_ARCHIVED") == Some("1")
        || env.get("CC_ISOLATED") == Some("1")
        || env.get("CC_WORKTREE_AUTO_MERGE") == Some("0")
    {
        return Err("worker lifecycle excludes integration".into());
    }
    let conn = state.store.read().map_err(|e| e.to_string())?;
    board_snapshot(&conn, name)
}

fn board_snapshot(
    conn: &rusqlite::Connection,
    name: &str,
) -> Result<(String, String, i64), String> {
    let mut stmt=conn.prepare("SELECT id,rev,status,COALESCE(type,'code'),COALESCE(evidence,''),COALESCE(depends_on,'[]'),COALESCE(blocked_on,'') FROM issues WHERE session=?1 AND deleted IS NULL AND COALESCE(archived,0)=0 ORDER BY id").map_err(|e|e.to_string())?;
    let rows = stmt
        .query_map([name], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    let tasks: Vec<_> = rows.iter().filter(|r| r.3 != "epic").collect();
    // Integrate completed prerequisites before dispatching their successors.
    // Waiting for the entire board made A(done) -> B(todo, needs A verified)
    // impossible once verification correctly requires integration. Real Doing
    // work still owns the workspace; containers/held captures use the same WIP
    // predicate as pickup. Git independently refuses a dirty candidate.
    if !crate::runtime_jobs::board_drive::wip_holding_ids(conn, name, None)
        .map_err(|e| e.to_string())?
        .is_empty()
    {
        return Err("worker still owns active implementation work".into());
    }
    let candidates: Vec<_> = tasks
        .iter()
        .copied()
        .filter(|r| matches!(r.2.as_str(), "review" | "done" | "verified"))
        .collect();
    if candidates.is_empty() || candidates.iter().any(|r| r.4.trim().is_empty()) {
        return Err("worker has no fully evidenced integration candidate".into());
    }
    for task in &candidates {
        let deps: Vec<String> = serde_json::from_str(&task.5).map_err(|e| e.to_string())?;
        if !task.6.trim().is_empty()
            || deps
                .iter()
                .any(|id| !crate::db::board_store::dependency_resolved(conn, id).unwrap_or(false))
        {
            return Err("worker still owns unresolved prerequisites".into());
        }
    }
    let card = candidates
        .iter()
        .find(|r| matches!(r.2.as_str(), "review" | "done"))
        .unwrap_or(&candidates[0]);
    Ok((
        serde_json::to_string(&rows).map_err(|e| e.to_string())?,
        card.0.clone(),
        card.1,
    ))
}

/// A stopped provider can still owe validation or repair after implementation.
/// This is concrete board work, so it uses the existing wake/dispatch path.
pub fn integration_followup_needed(state: &crate::api::AppState, name: &str) -> bool {
    let status = integration_status(&crate::config::amux_home(), name);
    matches!(status["status"].as_str(),Some("requires_work"|"integrated"))
        && ready_board(state,name).is_ok()
        && (status["status"]=="requires_work" || state.store.read().ok().is_some_and(|c| {
            let Ok(mut q)=c.prepare("SELECT status,COALESCE(type,'code') FROM issues WHERE session=?1 AND deleted IS NULL AND COALESCE(archived,0)=0") else { return false; };
            q.query_map([name],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).ok()
                .is_some_and(|rows|rows.flatten().any(|(st,ty)|!crate::db::board_store::execution_is_terminal(&st,&ty)))
        }))
}

/// Cheap admission at a confirmed turn boundary; expensive git/tests run off
/// the board loop, one candidate at a time. Ordinary failures return to this
/// worker, not Needs You or another worker's board.
pub async fn queue_integration(state: &crate::api::AppState, name: &str) -> bool {
    use serde_json::json;
    use std::sync::{Arc, OnceLock};
    let Ok((board, card, rev)) = ready_board(state, name) else {
        return false;
    };
    let home = crate::config::amux_home();
    let Some(workspace) = load(&home, name) else {
        return false;
    };
    let verify = crate::api::session_verbs::parse_env(name)
        .get_or("CC_WORKTREE_VERIFY", "")
        .to_string();
    static BUSY: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let previous = integration_status(&home, name);
    let Ok(permit) = BUSY
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
        .clone()
        .try_acquire_owned()
    else {
        return previous["status"] == "integrating";
    };
    let head = git(&workspace.path, &["rev-parse", "HEAD"])
        .await
        .unwrap_or_default();
    let fingerprint = json!([board, head, verify, workspace.base]).to_string();
    let now = crate::config::now_f64();
    if previous["fingerprint"] == fingerprint
        && previous["status"] != "integrating"
        && (previous["status"] == "integrated"
            || now - previous["at"].as_f64().unwrap_or(0.0) < 300.0)
    {
        if previous["status"] == "integrated" {
            record_integrated_head(&state.store, name, &head).await;
        }
        return false;
    }
    write_integration_status(
        &home,
        name,
        &json!({"status":"integrating","detail":"Validating the merged candidate","at":now,"fingerprint":fingerprint,"head":head}),
    );
    let state = state.clone();
    let name = name.to_string();
    tokio::spawn(async move {
        let _single = permit;
        let same_board = || {
            let current = ready_board(&state, &name)?;
            if current.0 != board {
                return Err("Board changed during integration; work the new state first".into());
            }
            if crate::api::session_verbs::parse_env(&name).get_or("CC_WORKTREE_VERIFY", "")
                != verify
                || load(&home, &name).is_none_or(|current| {
                    current.base != workspace.base
                        || current.path != workspace.path
                        || current.repo != workspace.repo
                })
            {
                return Err(
                    "Integration configuration changed; validate the new contract first".into(),
                );
            }
            Ok(())
        };
        let result = integrate(&workspace, &verify, same_board).await;
        let (status,detail)=match result { Ok(sha)=>("integrated",format!("Remote main contains {sha}; integration checks passed or the exact head was already present")),Err(e) if e==MAIN_ADVANCED_RETRY=>("retrying",e),Err(e)=>("requires_work",crate::api::session_verbs::redact_secrets(&e).chars().take(4000).collect()) };
        let record = json!({"status":status,"detail":detail,"head":head,"at":crate::config::now_f64(),"fingerprint":fingerprint,"worktree":workspace.path,"branch":workspace.branch});
        write_integration_status(&home, &name, &record);
        if status == "integrated" {
            record_integrated_head(&state.store, &name, &head).await;
        }
        tracing::info!(session=%name,verdict="fanout_integration",%status,%detail,"fan-out integration outcome");
        if status != "retrying"
            && previous["detail"] != detail
            && ready_board(&state, &name).is_ok()
        {
            let text=format!("[amux fan-out integration] {detail}. Continue on your own board and durable worktree {}. Configure the relevant repository test/lint command with PATCH /api/sessions/{name}/config {{\"worktree_verify\":\"<command>\"}}; the harness runs it on the exact merged candidate. For a legacy workspace, first inspect your unmerged history and then set worktree_base to the exact reviewed common ancestor of HEAD and origin/main through the same configuration endpoint. Resolve conflicts and test failures locally, commit the fix, and continue every remaining outcome through its gates. Do not create cross-worker dependencies or ordinary Needs You asks. Integration is evidence, not permission to acknowledge unverified criteria.",workspace.path);
            let key = format!("fanout-integration:{name}:{head}:{status}:{detail}");
            let _ = crate::api::session_verbs::enqueue_state_reminder(
                &state.store,
                &name,
                &text,
                "board-drive",
                &card,
                rev,
                &key,
            )
            .await;
        }
    });
    true
}

/// Adopt legacy workspaces only at a confirmed provider boundary. Never infer
/// a creation base for old history, overwrite a broken index, or move a live
/// process into a different directory. A missing workspace requires a restart.
pub async fn adopt_at_boundary(state: &crate::api::AppState, name: &str) {
    let lock = crate::api::session_verbs::session_op_lock(name);
    let _op = lock.lock().await;
    let home = crate::config::amux_home();
    let env = crate::api::session_verbs::parse_env(name);
    if env.get("CC_PROJECT").is_some()
        || env.get("CC_EPHEMERAL") != Some("1")
        || env.get("CC_PAUSED") == Some("1")
        || env.get("CC_ARCHIVED") == Some("1")
        || env.get("CC_ISOLATED") == Some("1")
        || (load(&home, name).is_some()
            && integration_status(&home, name)["status"] != "workspace_requires_recovery")
    {
        return;
    }
    let prior = integration_status(&home, name);
    if crate::config::now_f64() - prior["at"].as_f64().unwrap_or(0.0) < 300.0 {
        return;
    }
    let path = {
        let new_loc = std::path::Path::new(env.get_or("CC_DIR", "")).join(".worktrees").join(name);
        if new_loc.join(".git").exists() { new_loc } else { home.join("worktrees").join(name) }
    };
    let result = if path.join(".git").exists() {
        ensure(&home, name, env.get_or("CC_DIR", ""))
            .await
            .map(|_| ())
    } else {
        Err("The running worker has no durable workspace. Commit or preserve its current changes, then restart it to create its own worktree before continuing implementation".into())
    };
    if let Err(detail) = result {
        write_integration_status(
            &home,
            name,
            &serde_json::json!({"status":"workspace_requires_recovery","detail":detail,"at":crate::config::now_f64()}),
        );
        tracing::warn!(session=name,verdict="fanout_workspace_requires_recovery",%detail,"legacy workspace cannot be adopted automatically");
        let card=state.store.read().ok().and_then(|c|c.query_row("SELECT id,rev FROM issues WHERE session=?1 AND deleted IS NULL AND COALESCE(archived,0)=0 ORDER BY CASE status WHEN 'doing' THEN 0 ELSE 1 END,updated DESC LIMIT 1",[name],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?))).ok());
        if prior["detail"] != detail {
            if let Some((card, rev)) = card {
                let text=format!("[amux workspace recovery] {detail}. Own this recovery within your current board. Preserve the existing files, index and commits before repairing; never delete/reset an incomplete workspace or turn this into an outside dependency. Resume the full board after recovery.");
                let _ = crate::api::session_verbs::enqueue_state_reminder(
                    &state.store,
                    name,
                    &text,
                    "board-drive",
                    &card,
                    rev,
                    &format!("workspace-recovery:{name}:{detail}"),
                )
                .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_checkout_failure_retains_recovery_state_without_workspace_record() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("new-home");
        let status = serde_json::json!({"status":"workspace_requires_recovery","detail":"Interrupted checkout"});
        write_integration_status(&home, "child", &status);
        assert!(load(&home, "child").is_none());
        assert_eq!(integration_status(&home, "child"), status);
    }

    #[tokio::test]
    async fn legacy_adoption_names_head_without_checkout_hooks_or_file_changes() {
        use std::os::unix::fs::PermissionsExt;
        let (d, w) = fixture().await;
        git(&w.path, &["switch", "--detach"]).await.unwrap();
        git(&w.repo, &["branch", "-D", &w.branch]).await.unwrap();
        std::fs::remove_file(record_path(&d.path().join("home"), "child-a")).unwrap();
        std::fs::write(Path::new(&w.path).join("app.txt"), "uncommitted work\n").unwrap();
        let hook = Path::new(&w.repo).join(".git/hooks/post-checkout");
        std::fs::write(
            &hook,
            "#!/bin/sh\nprintf 'checkout must not run during adoption' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let head = git(&w.path, &["rev-parse", "HEAD"]).await.unwrap();
        let index = git(&w.path, &["ls-files", "--stage"]).await.unwrap();
        write_integration_status(
            &d.path().join("home"),
            "child-a",
            &serde_json::json!({"status":"workspace_requires_recovery"}),
        );
        let adopted = ensure(&d.path().join("home"), "child-a", &w.repo)
            .await
            .unwrap();
        // AMUX-4921 CHANGED THIS ASSERTION ON PURPOSE. It used to pin
        // `adopted.base.is_empty()`, which is the defect: an empty base
        // disables the ancestry guard for good and blocks integration
        // forever. Adoption now records a MEASURED commit. Here HEAD,
        // origin/main and their fork point are all the same commit, so that
        // is what lands.
        assert_eq!(
            adopted.base, head,
            "adoption must record a real base, never an empty string"
        );
        assert_eq!(
            integration_status(&d.path().join("home"), "child-a")["status"],
            "workspace_ready"
        );
        assert_eq!(git(&w.path, &["rev-parse", "HEAD"]).await.unwrap(), head);
        assert_eq!(git(&w.path, &["ls-files", "--stage"]).await.unwrap(), index);
        assert_eq!(
            git(&w.path, &["branch", "--show-current"]).await.unwrap(),
            w.branch
        );
        assert_eq!(
            std::fs::read_to_string(Path::new(&w.path).join("app.txt")).unwrap(),
            "uncommitted work\n"
        );
    }

    /// AMUX-4921. The measured shape: several fan-out workers created by ONE
    /// launch call, one of which recorded an empty base. Asserts on the
    /// PERSISTED records rather than the returned values, because the record
    /// is what integration reads later.
    #[tokio::test]
    async fn every_workspace_in_one_launch_records_a_base() {
        let (d, w) = fixture().await;
        let home = d.path().join("home");
        for name in ["child-b", "child-c", "child-d"] {
            ensure(&home, name, &w.repo).await.unwrap();
        }
        for name in ["child-a", "child-b", "child-c", "child-d"] {
            let rec = load(&home, name).unwrap_or_else(|| panic!("{name} has no workspace record"));
            assert!(
                !rec.base.is_empty(),
                "{name} persisted an EMPTY base; integration can never run for it"
            );
        }
    }

    /// AMUX-4921. `save` lands at the END of ensure(), so a second call for the
    /// same worker can observe the worktree directory without its record and
    /// mistake itself for a legacy adoption. Running the two concurrently
    /// exercises that window; the fix has to hold whichever way they interleave.
    #[tokio::test]
    async fn concurrent_ensure_never_records_an_empty_base() {
        let (d, w) = fixture().await;
        let home = d.path().join("home");
        let (repo, h1, h2) = (w.repo.clone(), home.clone(), home.clone());
        let (a, b) = tokio::join!(
            ensure(&h1, "child-race", &repo),
            ensure(&h2, "child-race", &w.repo),
        );
        // One arm may legitimately refuse (the worktree is being built by the
        // other). What must never happen is a PERSISTED record with no base.
        assert!(
            a.is_ok() || b.is_ok(),
            "both concurrent ensure() calls failed: {a:?} / {b:?}"
        );
        for done in [a, b].into_iter().flatten() {
            assert!(
                !done.base.is_empty(),
                "concurrent ensure() returned an empty base"
            );
        }
        if let Some(rec) = load(&home, "child-race") {
            assert!(
                !rec.base.is_empty(),
                "concurrent ensure() PERSISTED an empty base, which strands the worker forever"
            );
        }
    }

    /// AMUX-4921 criterion 3: a workspace already stranded by the old path must
    /// have a way out, not a permanent stall. The next adoption repairs it.
    #[tokio::test]
    async fn a_recorded_empty_base_is_repaired_rather_than_copied_forward() {
        let (d, w) = fixture().await;
        let home = d.path().join("home");
        let mut stranded = load(&home, "child-a").unwrap();
        assert!(!stranded.base.is_empty());
        stranded.base = String::new(); // exactly what the old creation path wrote
        save(&home, "child-a", &stranded).unwrap();
        let repaired = ensure(&home, "child-a", &w.repo).await.unwrap();
        assert!(
            !repaired.base.is_empty(),
            "an empty recorded base was copied forward; the stall is permanent"
        );
        assert!(!load(&home, "child-a").unwrap().base.is_empty());
    }

    /// AMUX-4922. `done` is refused for UNCOMMITTED work and allowed otherwise.
    ///
    /// Three cells, and the two permissive ones are the point: this gate must
    /// not become the thing it was written to avoid. It does NOT require an
    /// integration receipt (that would have no truthful exit when integration
    /// is impossible, which AMUX-4921 was live proof of), and it does not
    /// touch a worker that is not an ephemeral fan-out.
    #[tokio::test]
    async fn done_refuses_uncommitted_work_only_on_an_ephemeral_fanout_worker() {
        let (d, w) = fixture().await;
        let home = d.path().join("home");
        let _guard = crate::api::settings::test_env::set_home(&home);
        let env = home.join("sessions");
        std::fs::create_dir_all(&env).unwrap();
        let env_file = env.join("child-a.env");
        std::fs::write(&env_file, "CC_EPHEMERAL=1\n").unwrap();

        // CLEAN: nothing to refuse. The fixture committed everything.
        assert!(
            done_ready("child-a").await.is_ok(),
            "a clean ephemeral worktree must be allowed to reach done"
        );

        // DIRTY: the measured case. A worker marked done with a modified file
        // still sitting in a worktree scheduled for deletion.
        std::fs::write(Path::new(&w.path).join("app.txt"), "uncommitted work\n").unwrap();
        let refused = done_ready("child-a").await;
        let detail = refused.expect_err("uncommitted work in a doomed worktree must refuse done");
        assert!(
            detail.contains("app.txt"),
            "the refusal must name what is uncommitted, not just that something is: {detail}"
        );
        assert!(
            detail.contains("Commit"),
            "the refusal must name the one-command exit, or it is a gate with no way out: {detail}"
        );

        // NOT EPHEMERAL: same dirty worktree, no opinion. `done` on an ordinary
        // worker is a self-report and this gate is not about that.
        std::fs::write(&env_file, "CC_WORKER=1\n").unwrap();
        assert!(
            done_ready("child-a").await.is_ok(),
            "a non-ephemeral worker keeps its existing done semantics"
        );
    }

    async fn fixture() -> (tempfile::TempDir, Workspace) {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let repo = repo.to_string_lossy().into_owned();
        git(&repo, &["init", "-b", "main"]).await.unwrap();
        git(&repo, &["config", "user.email", "test@example.invalid"])
            .await
            .unwrap();
        git(&repo, &["config", "user.name", "Test Worker"])
            .await
            .unwrap();
        std::fs::write(Path::new(&repo).join("app.txt"), "base\n").unwrap();
        git(&repo, &["add", "app.txt"]).await.unwrap();
        git(&repo, &["commit", "-m", "base"]).await.unwrap();
        let remote = d.path().join("remote.git").to_string_lossy().into_owned();
        git(&repo, &["clone", "--bare", &repo, &remote])
            .await
            .unwrap();
        git(&repo, &["remote", "add", "origin", &remote])
            .await
            .unwrap();
        git(&repo, &["fetch", "origin"]).await.unwrap();
        let w = ensure(&d.path().join("home"), "child-a", &repo)
            .await
            .unwrap();
        (d, w)
    }
    async fn commit(w: &Workspace, file: &str, text: &str) {
        std::fs::write(Path::new(&w.path).join(file), text).unwrap();
        git(&w.path, &["add", file]).await.unwrap();
        git(&w.path, &["commit", "-m", "worker change"])
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn workspace_restart_preserves_dirty_files_commits_and_distinct_branches() {
        let (d, w) = fixture().await;
        commit(&w, "done.txt", "committed\n").await;
        let head = git(&w.path, &["rev-parse", "HEAD"]).await.unwrap();
        std::fs::write(Path::new(&w.path).join("draft.txt"), "uncommitted").unwrap();
        let again = ensure(&d.path().join("home"), "child-a", &w.repo)
            .await
            .unwrap();
        assert_eq!(again.base, w.base);
        assert_eq!(git(&w.path, &["rev-parse", "HEAD"]).await.unwrap(), head);
        assert_eq!(
            std::fs::read_to_string(Path::new(&again.path).join("draft.txt")).unwrap(),
            "uncommitted"
        );
        let other = ensure(&d.path().join("home"), "child-b", &w.repo)
            .await
            .unwrap();
        assert_ne!(other.path, w.path);
        assert_ne!(other.branch, w.branch);
        assert_eq!(
            git(&w.repo, &["branch", "--show-current"]).await.unwrap(),
            "main"
        );
        assert!(!Path::new(&other.path).join("done.txt").exists());
    }
    #[tokio::test]
    async fn integration_runs_checks_on_combined_changes_and_preserves_main_checkout() {
        let (_d, w) = fixture().await;
        commit(&w, "child.txt", "child\n").await;
        std::fs::write(Path::new(&w.repo).join("peer.txt"), "peer\n").unwrap();
        git(&w.repo, &["add", "peer.txt"]).await.unwrap();
        git(&w.repo, &["commit", "-m", "peer"]).await.unwrap();
        git(&w.repo, &["push", "origin", "main"]).await.unwrap();
        let local = git(&w.repo, &["rev-parse", "HEAD"]).await.unwrap();
        let merged = integrate(&w, "test -f peer.txt && test -f child.txt", || Ok(()))
            .await
            .unwrap();
        assert_eq!(
            git(&w.repo, &["rev-parse", "origin/main"]).await.unwrap(),
            merged
        );
        assert_eq!(git(&w.repo, &["rev-parse", "HEAD"]).await.unwrap(), local);
        assert_eq!(
            git(&w.path, &["branch", "--show-current"]).await.unwrap(),
            w.branch
        );
        assert!(Path::new(&w.path).join("child.txt").exists());
        assert_eq!(
            integrate(&w, "exit 1", || Ok(())).await.unwrap(),
            merged,
            "already integrated is an idempotent read"
        );
    }
    #[tokio::test]
    async fn failed_checks_and_changed_admission_never_push_or_dispose_worker_changes() {
        let (_d, w) = fixture().await;
        commit(&w, "child.txt", "child\n").await;
        let main = git(&w.repo, &["rev-parse", "origin/main"]).await.unwrap();
        assert!(integrate(&w, "exit 1", || Ok(()))
            .await
            .unwrap_err()
            .contains("validation exited"));
        let calls = std::cell::Cell::new(0);
        assert!(integrate(&w, "test -f child.txt", || {
            calls.set(calls.get() + 1);
            if calls.get() > 1 {
                Err("paused".into())
            } else {
                Ok(())
            }
        })
        .await
        .unwrap_err()
        .contains("paused"));
        assert_eq!(
            git(&w.repo, &["rev-parse", "origin/main"]).await.unwrap(),
            main
        );
        assert!(Path::new(&w.path).join("child.txt").exists());
    }

    #[tokio::test]
    async fn verification_cannot_pass_against_original_checkout_or_fallback_to_it() {
        let (_d, w) = fixture().await;
        commit(&w, "child.txt", "child\n").await;
        std::fs::write(Path::new(&w.repo).join("peer.txt"), "peer\n").unwrap();
        git(&w.repo, &["add", "peer.txt"]).await.unwrap();
        git(&w.repo, &["commit", "-m", "peer"]).await.unwrap();
        git(&w.repo, &["push", "origin", "main"]).await.unwrap();
        let before = git(&w.repo, &["rev-parse", "origin/main"]).await.unwrap();
        for command in [
            format!("cd '{}' && test -f child.txt && test ! -f peer.txt", w.path),
            format!("test -f missing.txt || cd '{}'; test -f child.txt", w.path),
            format!("cd '{}' && test -f peer.txt", w.repo),
        ] {
            let error = integrate(&w, &command, || Ok(())).await.unwrap_err();
            assert!(
                error.contains("original worker or shared checkout"),
                "{error}"
            );
            assert_eq!(
                git(&w.repo, &["rev-parse", "origin/main"]).await.unwrap(),
                before
            );
        }
        // Independent positive control: only the actual combined candidate
        // has both files. Rejecting every command is not a passing guard.
        let merged = integrate(&w, "test -f child.txt && test -f peer.txt", || Ok(()))
            .await
            .unwrap();
        assert_ne!(merged, before);
        assert!(!Path::new(&w.path).join("peer.txt").exists());
    }
    async fn peer_checkout(d: &Path, w: &Workspace) -> String {
        let peer = d.join("peer").to_string_lossy().into_owned();
        let remote = git(&w.repo, &["remote", "get-url", "origin"])
            .await
            .unwrap();
        git(&w.repo, &["clone", &remote, &peer]).await.unwrap();
        git(&peer, &["config", "user.email", "peer@example.invalid"])
            .await
            .unwrap();
        git(&peer, &["config", "user.name", "Peer"]).await.unwrap();
        peer
    }

    /// The peer really pushes to a local bare remote; no mocked Git error text.
    fn peer_push_script(peer: &str) -> String {
        // Git exports repository-local variables to hooks. The other checkout
        // must not inherit them or its push recursively invokes this hook.
        format!("unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_INDEX_FILE GIT_PREFIX\nprintf 'peer\\n' >> '{peer}/peer.txt'\ngit -C '{peer}' add peer.txt\ngit -C '{peer}' commit -m peer\ngit -C '{peer}' push origin main\n")
    }

    #[tokio::test]
    async fn main_advance_during_validation_rebuilds_and_revalidates_without_moving_workers() {
        let (d, w) = fixture().await;
        commit(&w, "child.txt", "child\n").await;
        let peer = peer_checkout(d.path(), &w).await;
        let marker = d.path().join("validated").to_string_lossy().into_owned();
        let local = git(&w.repo, &["rev-parse", "HEAD"]).await.unwrap();
        let child = git(&w.path, &["rev-parse", "HEAD"]).await.unwrap();
        let verify = format!("set -eu\ntest -f child.txt\nif test -f '{marker}'; then test -f peer.txt; else\n{}fi\nprintf 'checked\\n' >> '{marker}'\n", peer_push_script(&peer));
        let merged = integrate(&w, &verify, || Ok(())).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "checked\nchecked\n"
        );
        assert_eq!(
            git(&w.repo, &["show", &format!("{merged}:peer.txt")])
                .await
                .unwrap(),
            "peer"
        );
        assert_eq!(
            git(&w.repo, &["show", &format!("{merged}:child.txt")])
                .await
                .unwrap(),
            "child"
        );
        assert_eq!(git(&w.repo, &["rev-parse", "HEAD"]).await.unwrap(), local);
        assert_eq!(git(&w.path, &["rev-parse", "HEAD"]).await.unwrap(), child);
    }

    #[tokio::test]
    async fn main_advance_in_pre_push_retries_but_unchanged_remote_gate_failure_does_not() {
        use std::os::unix::fs::PermissionsExt;
        let (d, w) = fixture().await;
        commit(&w, "child.txt", "child\n").await;
        let peer = peer_checkout(d.path(), &w).await;
        let hooks = d.path().join("hook-calls").to_string_lossy().into_owned();
        let checks = d.path().join("checks").to_string_lossy().into_owned();
        let hook = Path::new(&w.repo).join(".git/hooks/pre-push");
        std::fs::write(&hook, format!("#!/bin/sh\nset -eu\nif ! test -f '{hooks}'; then\n{}fi\nprintf 'hook\\n' >> '{hooks}'\n", peer_push_script(&peer))).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let verify = format!("test -f child.txt && printf 'checked\\n' >> '{checks}'");
        let merged = integrate(&w, &verify, || Ok(())).await.unwrap();
        assert_eq!(std::fs::read_to_string(&hooks).unwrap(), "hook\nhook\n");
        assert_eq!(
            std::fs::read_to_string(&checks).unwrap(),
            "checked\nchecked\n"
        );
        assert_eq!(
            git(&w.repo, &["show", &format!("{merged}:peer.txt")])
                .await
                .unwrap(),
            "peer"
        );

        commit(&w, "later.txt", "unmerged\n").await;
        std::fs::write(&hook, format!("#!/bin/sh\nprintf 'refused\\n' >> '{hooks}'\necho real-content-gate-refusal >&2\nexit 1\n")).unwrap();
        let error = integrate(&w, &verify, || Ok(())).await.unwrap_err();
        assert!(error.contains("real-content-gate-refusal"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&hooks).unwrap(),
            "hook\nhook\nrefused\n"
        );
        assert_eq!(
            git(&w.repo, &["rev-parse", "origin/main"]).await.unwrap(),
            merged
        );
        assert!(Path::new(&w.path).join("later.txt").exists());
    }

    #[tokio::test]
    async fn main_advance_retry_is_bounded_and_revalidation_failure_never_pushes() {
        let (d, w) = fixture().await;
        commit(&w, "child.txt", "child\n").await;
        let peer = peer_checkout(d.path(), &w).await;
        let checks = d.path().join("checks").to_string_lossy().into_owned();
        let verify = format!(
            "set -eu\n{}printf 'checked\\n' >> '{checks}'\n",
            peer_push_script(&peer)
        );
        assert_eq!(
            integrate(&w, &verify, || Ok(())).await.unwrap_err(),
            MAIN_ADVANCED_RETRY
        );
        assert_eq!(
            std::fs::read_to_string(&checks).unwrap(),
            "checked\nchecked\nchecked\n"
        );
        assert!(git(&w.repo, &["show", "origin/main:child.txt"])
            .await
            .is_err());

        let once = d.path().join("once").to_string_lossy().into_owned();
        let fail = format!("set -eu\nif test -f '{once}'; then echo combined-regression >&2; exit 9; fi\ntouch '{once}'\n{}", peer_push_script(&peer));
        let error = integrate(&w, &fail, || Ok(())).await.unwrap_err();
        assert!(error.contains("combined-regression"), "{error}");
        assert!(git(&w.repo, &["show", "origin/main:child.txt"])
            .await
            .is_err());
    }
    #[test]
    fn integration_admits_completed_prerequisites_before_their_successors() {
        let c = crate::db::migrate::test_memdb();
        c.execute_batch(
            "INSERT INTO issues(id,title,status,type,session,created,updated,evidence) VALUES
            ('A','assignment','done','code','child',1,1,'tests passed'),
            ('B','followup','backlog','code','child',1,1,'');",
        )
        .unwrap();
        c.execute("UPDATE issues SET depends_on='[\"A\"]' WHERE id='B'", [])
            .unwrap();
        assert!(
            board_snapshot(&c, "child").is_ok(),
            "completed prerequisite can integrate before its queued successor"
        );
        c.execute(
            "UPDATE issues SET status='doing',depends_on='[]' WHERE id='B'",
            [],
        )
        .unwrap();
        assert!(
            board_snapshot(&c, "child").is_err(),
            "active implementation still owns the worktree"
        );
        c.execute("UPDATE issues SET status='review' WHERE id='B'", [])
            .unwrap();
        assert!(
            board_snapshot(&c, "child").is_err(),
            "review without evidence must not integrate"
        );
        c.execute(
            "UPDATE issues SET evidence='proof of implementation' WHERE id='B'",
            [],
        )
        .unwrap();
        let snapshot = board_snapshot(&c, "child").unwrap();
        c.execute("UPDATE issues SET rev=rev+1 WHERE id='B'", [])
            .unwrap();
        assert_ne!(
            snapshot.0,
            board_snapshot(&c, "child").unwrap().0,
            "any revised task invalidates an in-flight candidate"
        );
        c.execute(
            "UPDATE issues SET depends_on='[\"missing\"]' WHERE id='B'",
            [],
        )
        .unwrap();
        assert!(board_snapshot(&c, "child").is_err());
    }
    #[tokio::test]
    async fn project_verification_source_and_merged_use_distinct_per_command_timeouts() {
        let (d, w) = fixture().await;
        commit(&w, "done.txt", "done\n").await;
        let log = d.path().join("checked-paths");
        let a = format!("sleep 3; printf '%s\\n' \"$PWD\" >> '{}'", log.display());
        let b = format!("sleep 3; printf '%s\\n' \"$PWD\" >> '{}'", log.display());
        // A byte-distinct command also runs. Combined wall time exceeds the
        // five-second bound (6s total); each command has 2s scheduling slack.
        let b = format!("{b}; true");
        let commands = [a.as_str(), a.as_str(), b.as_str()];
        verify_commands(&w, &w.path, &commands, Duration::from_secs(5), &|| Ok(()))
            .await
            .unwrap();
        integrate_checks(&w, &commands, Duration::from_secs(5), || Ok(()))
            .await
            .unwrap();
        let text = std::fs::read_to_string(log).unwrap();
        let paths: Vec<_> = text.lines().collect();
        assert_eq!(
            paths.len(),
            4,
            "two distinct commands, once per immutable phase"
        );
        let source = std::fs::canonicalize(&w.path).unwrap();
        assert_eq!(std::fs::canonicalize(paths[0]).unwrap(), source);
        assert_eq!(std::fs::canonicalize(paths[1]).unwrap(), source);
        // Integration has disposed its temporary candidate; compare its recorded
        // physical paths without resolving a directory that no longer exists.
        assert_ne!(paths[2], paths[0]);
        assert_ne!(paths[2], paths[1]);
        assert_eq!(paths[2], paths[3]);
        let err = verify_commands(
            &w,
            &w.path,
            &["true", "exit 9"],
            Duration::from_secs(1),
            &|| Ok(()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("verification failed (exit 9)"));
    }
    #[tokio::test]
    async fn declared_runtime_outputs_can_be_retained_without_ignoring_source_edits() {
        let (dir, workspace) = fixture().await;
        commit(&workspace, "candidate.txt", "ready\n").await;
        let candidate = workspace.path.clone();
        let archive = dir.path().join("runtime-output.txt");
        verify_commands_with_cleanup(
            &workspace, &candidate, &["printf 'measured\\n' > runtime-output.txt"],
            Duration::from_secs(5), &|| Ok(()),
            || {
                let candidate = candidate.clone();
                let archive = archive.clone();
                async move {
                    let output = std::path::Path::new(&candidate).join("runtime-output.txt");
                    if output.exists() {
                        std::fs::rename(output, archive).map_err(|error| error.to_string())?;
                    }
                    Ok(())
                }
            },
        ).await.unwrap();
        assert_eq!(std::fs::read_to_string(&archive).unwrap(), "measured\n");
        assert!(project_clean_status(&candidate).await.unwrap().is_empty());

        let error = verify_commands_with_cleanup(
            &workspace, &candidate, &["printf 'unapproved\\n' > source-edit.txt"],
            Duration::from_secs(5), &|| Ok(()), || async { Ok(()) },
        ).await.unwrap_err();
        assert_eq!(error, "verification changed the reported worktree");
    }
    #[tokio::test]
    async fn project_verification_timeout_kills_children_in_both_candidate_phases() {
        for merged in [false, true] {
            let (d, w) = fixture().await;
            commit(&w, "done.txt", "done\n").await;
            let marker = d.path().join("leaked-child");
            let command = format!("(sleep 2; echo leaked > '{}') & wait", marker.display());
            let result = if merged {
                integrate_checks(&w, &[&command], Duration::from_secs(1), || Ok(()))
                    .await
                    .map(|_| ())
            } else {
                verify_commands(&w, &w.path, &[&command], Duration::from_secs(1), &|| Ok(())).await
            };
            assert!(
                result.unwrap_err().contains("timed out after 1 seconds"),
                "merged={merged}"
            );
            tokio::time::sleep(Duration::from_millis(2200)).await;
            assert!(
                !marker.exists(),
                "timeout leaked verification descendant, merged={merged}"
            );
            assert!(git(&w.path, &["status", "--porcelain"])
                .await
                .unwrap()
                .is_empty());
        }
    }
    #[tokio::test]
    async fn cancellation_kills_validation_descendants_and_retains_failure_output() {
        let d = tempfile::tempdir().unwrap();
        let marker = d.path().join("should-not-exist");
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "(sleep 1; echo leaked > \"$1\") & wait", "validation"])
            .arg(&marker);
        let started = std::time::Instant::now();
        let result = checked_command(
            cmd,
            &|| {
                if started.elapsed() > Duration::from_millis(100) {
                    Err("worker paused".into())
                } else {
                    Ok(())
                }
            },
            Duration::from_secs(5),
        )
        .await;
        assert!(result.unwrap_err().contains("paused"));
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(
            !marker.exists(),
            "a validation descendant kept working after pause"
        );
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "echo assertion-failed >&2; exit 7"]);
        let (status, output) = checked_command(cmd, &|| Ok(()), Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(status.code(), Some(7));
        assert!(output.contains("assertion-failed"));
    }
    #[tokio::test]
    async fn project_verification_profile_keeps_checkout_and_command_as_separate_arguments() {
        let d=tempfile::tempdir().unwrap();let path=d.path().join("candidate with ' quote");std::fs::create_dir(&path).unwrap();
        let mut process=verification_process("/usr/bin:/bin",path.to_str().unwrap(),"pwd; printf '%s' 'literal $not_expanded'");
        let out=process.output().await.unwrap();assert!(out.status.success());
        let text=String::from_utf8_lossy(&out.stdout);let lines=text.lines().collect::<Vec<_>>();
        assert_eq!(std::fs::canonicalize(lines[0]).unwrap(),std::fs::canonicalize(&path).unwrap());
        assert_eq!(lines[1],"literal $not_expanded");
    }

    #[test]
    fn project_status_ignores_only_durable_harness_receipt() {
        let raw = "\
?? .amux/project-report.json
?? .amux/notes.json
 M src/lib.rs
R  old-report.md -> .amux/project-report.json
A  artifacts/project-report.json
";
        assert_eq!(
            status_without_harness_receipts(raw),
            "\
?? .amux/notes.json
 M src/lib.rs
A  artifacts/project-report.json"
        );
    }

    #[test]
    fn verification_command_policy_rejects_dynamic_shell_expansion() {
        let workspace = Workspace {
            repo: "/tmp/source-repo".into(),
            path: "/tmp/source-repo/.amux/worktrees/child".into(),
            branch: "amux/fanout/child".into(),
            base: "a".repeat(40),
        };
        assert!(validate_verification_command(&workspace, "python3 scripts/verify.py").is_ok());
        let err = validate_verification_command(
            &workspace,
            r#"grep -Fx "worktree: $(pwd)" artifacts/report.md"#,
        )
        .unwrap_err();
        assert!(err.contains("static candidate-relative"));
        let err =
            validate_verification_command(&workspace, "test `pwd` = /tmp/source-repo").unwrap_err();
        assert!(err.contains("static candidate-relative"));
        let err = validate_verification_command(&workspace, "test -f .amux/project-report.json")
            .unwrap_err();
        assert!(err.contains("receipt files"));
    }

    #[tokio::test]
    async fn conflicts_stay_local_and_an_incomplete_checkout_is_preserved() {
        let (d, w) = fixture().await;
        commit(&w, "app.txt", "child\n").await;
        std::fs::write(Path::new(&w.repo).join("app.txt"), "peer\n").unwrap();
        git(&w.repo, &["add", "app.txt"]).await.unwrap();
        git(&w.repo, &["commit", "-m", "peer"]).await.unwrap();
        git(&w.repo, &["push", "origin", "main"]).await.unwrap();
        let main = git(&w.repo, &["rev-parse", "HEAD"]).await.unwrap();
        assert!(integrate(&w, "true", || Ok(()))
            .await
            .unwrap_err()
            .contains("Integration conflict"));
        assert_eq!(
            git(&w.repo, &["rev-parse", "origin/main"]).await.unwrap(),
            main
        );
        assert_eq!(
            std::fs::read_to_string(Path::new(&w.path).join("app.txt")).unwrap(),
            "child\n"
        );
        git(&w.path, &["read-tree", "--empty"]).await.unwrap();
        assert!(ensure(&d.path().join("home"), "child-a", &w.repo)
            .await
            .unwrap_err()
            .contains("index is empty"));
        assert!(Path::new(&w.path).join("app.txt").exists());
    }
}

#[cfg(test)]
mod not_landed_tests {
    use super::*;
    use serde_json::json;

    /// AMUX-4956. `done` means implemented; delivered is a different claim.
    #[test]
    fn only_an_integrated_receipt_counts_as_landed() {
        assert!(integration_landed(&json!({"status": "integrated"})));
        assert!(not_landed_note(&json!({"status": "integrated"})).is_none());

        // Every other real status, taken from what this box actually holds:
        // 6 integrated, 8 requires_work, 1 workspace_ready, 1 recovery.
        for status in [
            "requires_work",
            "workspace_ready",
            "workspace_requires_recovery",
            "integrating",
        ] {
            let receipt = json!({"status": status, "detail": "why it stopped"});
            assert!(!integration_landed(&receipt), "{status} is not landed");
            let note = not_landed_note(&receipt).expect("a note");
            assert_eq!(note["landed"], json!(false));
            assert_eq!(note["integration_status"], json!(status));
            assert_eq!(
                note["detail"],
                json!("why it stopped"),
                "the receipt's reason must travel"
            );
        }
    }

    /// An ABSENT receipt is the measured specimen: the worker never integrated
    /// at all. It must read as "absent", not as an empty string that looks like
    /// a status nobody set.
    #[test]
    fn an_absent_receipt_says_absent_rather_than_nothing() {
        let note = not_landed_note(&serde_json::Value::Null).expect("a note");
        assert_eq!(note["integration_status"], json!("absent"));
        assert_eq!(note["landed"], json!(false));
    }

    /// The note is DERIVED, so a later successful integration removes it with
    /// no resolution path to get wrong. This is the staleness hazard the card
    /// warned about, pinned.
    #[test]
    fn the_note_disappears_by_itself_once_integration_succeeds() {
        let before = json!({"status": "requires_work", "detail": "merge conflict"});
        assert!(not_landed_note(&before).is_some());
        let after = json!({"status": "integrated", "head": "abc123"});
        assert!(
            not_landed_note(&after).is_none(),
            "a derived note must vanish when the work lands; a stamped one would not"
        );
    }
}
