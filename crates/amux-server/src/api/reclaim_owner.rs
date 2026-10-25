//! Ownership guard for `/api/reclaim`: who would lose work if this path moved.
//!
//! `guard_path` in reclaim.rs is a static deny-list (system trees, home's
//! top-level dirs, amux state). It has no notion of an OWNER, and two
//! incidents on 2026-10-20 were both ownership failures:
//!
//! 1. 18:45Z: a session asked to "use the amux metrics to clean up my disk"
//!    ran `rm -rf ~/.amux/tmp/mxp-gs12` while worker mxp-gs12 was mid-run.
//!    31.6 GB of live scripts, state and clones went; it was restored from
//!    backup. The disk metrics named a big directory and nothing said its
//!    owner was alive.
//! 2. 20:13Z: a generated `disk-cleanup.sh` removed every "clean" Mixpeek
//!    worktree regardless of owner, a customer test-data directory with 74
//!    git-tracked files (customers/tubescience/ff-e2e), and every
//!    `~/.amux/tmp/<worker>` over 10 MB including live workers.
//!
//! So a path is OWNED, and refused, when moving it would take work from
//! someone who still needs it:
//!
//! - `~/.amux/tmp` itself, or anything at or under `~/.amux/tmp/<name>` while
//!   worker `<name>` is running (`amux-<name>` tmux session);
//! - anything inside a git work tree that has git-TRACKED files under it.
//!   Untracked and ignored build output (node_modules, target, .venv) inside a
//!   repo has no tracked files and stays eligible, which is most of what a
//!   cleanup is for;
//! - a git worktree root (its `.git` is a FILE) that is dirty, carries commits
//!   no remote has, or is named after a running worker;
//! - a directory that CONTAINS any of the above within two levels.
//!
//! Every check that cannot run (tmux silent, git timed out, a gitdir gone)
//! produces an `unmeasured` refusal rather than a pass. Being wrong in the
//! refuse direction costs disk space; being wrong in the allow direction is
//! the two incidents above.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Wall-clock cap for any one git or tmux probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// How many directories the nested-repo search may read under one candidate.
/// Past this the search is incomplete, and an incomplete search refuses.
const NESTED_DIR_CAP: usize = 2000;

pub(crate) const CLASS_TMP_ROOT: &str = "amux_tmp_root";
pub(crate) const CLASS_LIVE_SCRATCH: &str = "live_worker_scratch";
pub(crate) const CLASS_GIT_TRACKED: &str = "git_tracked";
pub(crate) const CLASS_WT_LIVE: &str = "worktree_live_worker";
pub(crate) const CLASS_WT_DIRTY: &str = "worktree_dirty";
pub(crate) const CLASS_WT_UNPUSHED: &str = "worktree_unpushed";
pub(crate) const CLASS_UNMEASURED: &str = "unmeasured";

/// One refusal: a stable class for greps and the UI, and a sentence for a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub class: &'static str,
    pub reason: String,
}

impl Refusal {
    fn new(class: &'static str, reason: impl Into<String>) -> Self {
        Self {
            class,
            reason: reason.into(),
        }
    }
    pub(crate) fn unmeasured(why: impl std::fmt::Display) -> Self {
        Self::new(CLASS_UNMEASURED, format!("unmeasured: {why}"))
    }
}

/// What the guard needs to know about the fleet, probed once per request.
#[derive(Debug, Clone)]
pub(crate) struct Owners {
    /// `~/.amux/tmp`, resolved.
    pub tmp_root: PathBuf,
    /// Running worker names, or why they could not be listed.
    pub live: Result<BTreeSet<String>, String>,
}

impl Owners {
    pub(crate) async fn probe() -> Self {
        Self {
            tmp_root: resolved(&crate::config::amux_home().join("tmp")),
            live: live_workers().await,
        }
    }
}

/// Worker names with a running `amux-<name>` tmux session.
///
/// Differs from `session_verbs::running_workers` in one way that matters here:
/// that helper answers an empty set when tmux fails, which this guard would
/// read as "nobody is running, move anything". A failed probe is an `Err`.
async fn live_workers() -> Result<BTreeSet<String>, String> {
    let out = super::session_verbs::run_cmd(
        "tmux",
        &["list-sessions", "-F", "#{session_name}"],
        Duration::from_secs(3),
    )
    .await
    .ok_or_else(|| "tmux list-sessions did not answer within 3s".to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // No server at all is a real, measured "nothing is running".
        if err.contains("no server running") || err.contains("No such file or directory") {
            return Ok(BTreeSet::new());
        }
        return Err(format!("tmux list-sessions failed: {}", err.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().strip_prefix("amux-").map(str::to_string))
        .collect())
}

/// Resolve symlinks in the PARENT chain so `/tmp/link/customers` is judged as
/// the repo path it really moves. The final component is kept as given: moving
/// a symlink moves the link, which is harmless to whatever it points at.
pub(crate) fn resolved(p: &Path) -> PathBuf {
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf())
            .join(name),
        _ => p.to_path_buf(),
    }
}

/// `None` when nothing owns `p`; otherwise why it must stay where it is.
pub(crate) async fn refusal(p: &Path, o: &Owners) -> Option<Refusal> {
    let p = resolved(p);
    if let Some(r) = scratch_rule(&p, o) {
        return Some(r);
    }
    match enclosing_repo(&p) {
        Some(root) if root == p => {
            if let Some(r) = repo_root_rule(&root, o).await {
                return Some(r);
            }
        }
        Some(root) => {
            if let Some(r) = tracked_rule(&root, &p).await {
                return Some(r);
            }
        }
        None => {}
    }
    // A directory ABOVE a repo or worktree: `~/Dev/x/.worktrees` holds every
    // worktree, and moving it moves all of them.
    match nested_repos(&p) {
        Err(why) => Some(Refusal::unmeasured(why)),
        Ok(repos) => {
            for repo in repos {
                if let Some(r) = repo_root_rule(&repo, o).await {
                    return Some(Refusal::new(
                        r.class,
                        format!("contains {}: {}", repo.display(), r.reason),
                    ));
                }
            }
            None
        }
    }
}

fn scratch_rule(p: &Path, o: &Owners) -> Option<Refusal> {
    if p == o.tmp_root {
        return Some(Refusal::new(
            CLASS_TMP_ROOT,
            "~/.amux/tmp holds every worker's scratch space",
        ));
    }
    let rel = p.strip_prefix(&o.tmp_root).ok()?;
    let name = rel.components().next()?.as_os_str().to_string_lossy();
    match &o.live {
        Err(why) => Some(Refusal::unmeasured(format!(
            "cannot tell whether worker {name} is running ({why})"
        ))),
        Ok(live) if live.contains(name.as_ref()) => Some(Refusal::new(
            CLASS_LIVE_SCRATCH,
            format!("scratch space of running worker {name}"),
        )),
        Ok(_) => None,
    }
}

/// The nearest directory at or above `p` that has a `.git` entry.
fn enclosing_repo(p: &Path) -> Option<PathBuf> {
    let start = if p.is_dir() { Some(p) } else { p.parent() };
    let mut cur = start;
    while let Some(d) = cur {
        if std::fs::symlink_metadata(d.join(".git")).is_ok() {
            return Some(d.to_path_buf());
        }
        cur = d.parent();
    }
    None
}

fn is_linked_worktree(root: &Path) -> bool {
    std::fs::symlink_metadata(root.join(".git")).is_ok_and(|m| m.is_file())
}

/// Rules for a path that IS a repository or worktree root.
async fn repo_root_rule(root: &Path, o: &Owners) -> Option<Refusal> {
    if !is_linked_worktree(root) {
        // A primary clone. Its tracked files exist nowhere else on this disk.
        return tracked_rule(root, root).await;
    }
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    match &o.live {
        Err(why) => {
            return Some(Refusal::unmeasured(format!(
                "cannot tell whether worker {name} is running ({why})"
            )))
        }
        Ok(live) if live.contains(&name) => {
            return Some(Refusal::new(
                CLASS_WT_LIVE,
                format!("git worktree of running worker {name}"),
            ))
        }
        Ok(_) => {}
    }
    let status = match git(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
    )
    .await
    {
        Ok(s) => s,
        Err(why) => return Some(Refusal::unmeasured(format!("git status: {why}"))),
    };
    let dirty = status.split('\0').filter(|l| !l.is_empty()).count();
    if dirty > 0 {
        return Some(Refusal::new(
            CLASS_WT_DIRTY,
            format!("git worktree with {dirty} uncommitted change(s)"),
        ));
    }
    let ahead = match git(root, &["rev-list", "--count", "HEAD", "--not", "--remotes"]).await {
        Ok(s) => s.trim().parse::<u64>().unwrap_or(u64::MAX),
        Err(why) => return Some(Refusal::unmeasured(format!("git rev-list: {why}"))),
    };
    if ahead > 0 {
        return Some(Refusal::new(
            CLASS_WT_UNPUSHED,
            if ahead == u64::MAX {
                "git worktree whose unpushed commit count could not be read".to_string()
            } else {
                format!("git worktree with {ahead} commit(s) on no remote")
            },
        ));
    }
    None
}

/// Refuse when any git-tracked file lives at or under `p` in the repo at `root`.
async fn tracked_rule(root: &Path, p: &Path) -> Option<Refusal> {
    let rel = p.strip_prefix(root).unwrap_or(Path::new(""));
    let rel_s = rel.to_string_lossy().to_string();
    let mut args = vec!["ls-files", "-z", "--cached"];
    if !rel_s.is_empty() {
        args.push("--");
        args.push(&rel_s);
    }
    let out = match git(root, &args).await {
        Ok(o) => o,
        Err(why) => return Some(Refusal::unmeasured(format!("git ls-files: {why}"))),
    };
    let mut files = out.split('\0').filter(|l| !l.is_empty());
    let first = files.next()?.to_string();
    let n = 1 + files.count();
    Some(Refusal::new(
        CLASS_GIT_TRACKED,
        format!(
            "{n} git-tracked file(s) in {} (e.g. {first})",
            root.display()
        ),
    ))
}

/// Repo or worktree roots one or two levels below `p`. Regenerable build
/// directories are not searched: their contents are the thing being reclaimed.
fn nested_repos(p: &Path) -> Result<Vec<PathBuf>, String> {
    if !p.is_dir() || is_build_dir(p) {
        return Ok(vec![]);
    }
    let mut found = Vec::new();
    let mut reads = 0usize;
    let mut frontier = vec![p.to_path_buf()];
    for _depth in 0..2 {
        let mut next = Vec::new();
        for dir in frontier {
            reads += 1;
            if reads > NESTED_DIR_CAP {
                return Err(format!(
                    "more than {NESTED_DIR_CAP} directories two levels under {}, nested repositories not fully checked",
                    p.display()
                ));
            }
            let Ok(rd) = std::fs::read_dir(&dir) else {
                return Err(format!("cannot read {}", dir.display()));
            };
            for e in rd.flatten() {
                let Ok(ft) = e.file_type() else { continue };
                if !ft.is_dir() || ft.is_symlink() {
                    continue;
                }
                let child = e.path();
                if std::fs::symlink_metadata(child.join(".git")).is_ok() {
                    found.push(child);
                } else if !is_build_dir(&child) {
                    next.push(child);
                }
            }
        }
        frontier = next;
    }
    Ok(found)
}

fn is_build_dir(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| super::reclaim::BUILD_DIRS.contains(&n))
}

/// Run git in `dir` with a timeout. Literal pathspecs so a path with `*` in
/// it means itself; optional locks off so a probe never contends for the
/// index with a lane that is committing.
async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("--literal-pathspecs")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(PROBE_TIMEOUT, cmd.output()).await {
        Err(_) => Err(format!("timed out after {}s", PROBE_TIMEOUT.as_secs())),
        Ok(Err(e)) => Err(format!("could not run git: {e}")),
        Ok(Ok(out)) if !out.status.success() => Err(format!(
            "exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Ok(Ok(out)) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let st = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            st.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&st.stderr)
        );
    }

    fn owners(tmp_root: &Path, live: &[&str]) -> Owners {
        Owners {
            tmp_root: resolved(tmp_root),
            live: Ok(live.iter().map(|s| s.to_string()).collect()),
        }
    }

    /// A primary repo with one tracked file under data/, an ignored
    /// node_modules and target, pushed to a bare remote.
    fn repo(base: &Path) -> PathBuf {
        let remote = base.join("remote.git");
        let r = base.join("repo");
        std::fs::create_dir_all(r.join("customers/acme/fixtures")).unwrap();
        std::fs::write(r.join("customers/acme/fixtures/case.json"), "{}").unwrap();
        std::fs::write(r.join(".gitignore"), "node_modules/\ntarget/\n").unwrap();
        std::fs::create_dir_all(r.join("web/node_modules/pkg")).unwrap();
        std::fs::write(r.join("web/node_modules/pkg/index.js"), "x").unwrap();
        std::fs::create_dir_all(r.join("target/debug")).unwrap();
        std::fs::write(r.join("target/debug/bin"), "x").unwrap();
        std::fs::create_dir_all(r.join("scratch-untracked")).unwrap();
        std::fs::write(r.join("scratch-untracked/a"), "x").unwrap();
        sh(base, &["init", "-q", "--bare", "remote.git"]);
        sh(&r, &["init", "-q"]);
        sh(&r, &["add", "-A", "customers", ".gitignore"]);
        sh(&r, &["commit", "-qm", "init"]);
        sh(&r, &["remote", "add", "origin", remote.to_str().unwrap()]);
        sh(&r, &["push", "-q", "origin", "HEAD:main"]);
        sh(&r, &["fetch", "-q", "origin"]);
        r
    }

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[tokio::test]
    async fn refuses_the_amux_tmp_root_itself() {
        let t = tmpdir();
        let root = t.path().join("tmp");
        std::fs::create_dir_all(&root).unwrap();
        let r = refusal(&root, &owners(&root, &[])).await.expect("refused");
        assert_eq!(r.class, CLASS_TMP_ROOT);
    }

    #[tokio::test]
    async fn refuses_a_running_workers_scratch_and_frees_a_stopped_ones() {
        let t = tmpdir();
        let root = t.path().join("tmp");
        std::fs::create_dir_all(root.join("mxp-gs12/clones/big")).unwrap();
        std::fs::create_dir_all(root.join("old-worker")).unwrap();
        let o = owners(&root, &["mxp-gs12"]);
        let r = refusal(&root.join("mxp-gs12"), &o).await.expect("refused");
        assert_eq!(r.class, CLASS_LIVE_SCRATCH);
        let deep = refusal(&root.join("mxp-gs12/clones/big"), &o)
            .await
            .expect("deep path refused");
        assert_eq!(deep.class, CLASS_LIVE_SCRATCH);
        assert_eq!(refusal(&root.join("old-worker"), &o).await, None);
    }

    #[tokio::test]
    async fn an_unanswered_fleet_probe_refuses_scratch_as_unmeasured() {
        let t = tmpdir();
        let root = t.path().join("tmp");
        std::fs::create_dir_all(root.join("old-worker")).unwrap();
        let o = Owners {
            tmp_root: resolved(&root),
            live: Err("tmux list-sessions did not answer within 3s".into()),
        };
        let r = refusal(&root.join("old-worker"), &o)
            .await
            .expect("refused");
        assert_eq!(r.class, CLASS_UNMEASURED);
        assert!(r.reason.starts_with("unmeasured: "), "{}", r.reason);
    }

    #[tokio::test]
    async fn refuses_tracked_files_inside_a_repo() {
        let t = tmpdir();
        let r = repo(t.path());
        let o = owners(&t.path().join("no-tmp"), &[]);
        for p in [r.join("customers/acme"), r.join("customers"), r.clone()] {
            let got = refusal(&p, &o).await.expect("tracked path refused");
            assert_eq!(got.class, CLASS_GIT_TRACKED, "{}", p.display());
        }
        let file = refusal(&r.join("customers/acme/fixtures/case.json"), &o)
            .await
            .expect("tracked file refused");
        assert_eq!(file.class, CLASS_GIT_TRACKED);
    }

    #[tokio::test]
    async fn still_allows_untracked_build_output_inside_a_repo() {
        let t = tmpdir();
        let r = repo(t.path());
        let o = owners(&t.path().join("no-tmp"), &[]);
        for p in [
            r.join("web/node_modules"),
            r.join("target"),
            r.join("scratch-untracked"),
        ] {
            assert_eq!(refusal(&p, &o).await, None, "{} must stay eligible", p.display());
        }
    }

    #[tokio::test]
    async fn worktree_roots_refuse_dirty_unpushed_and_live_and_free_a_clean_pushed_one() {
        let t = tmpdir();
        let r = repo(t.path());
        let wts = r.join(".worktrees");
        sh(&r, &["worktree", "add", "-q", "--detach", ".worktrees/clean", "origin/main"]);
        sh(&r, &["worktree", "add", "-q", "--detach", ".worktrees/dirty", "origin/main"]);
        sh(&r, &["worktree", "add", "-q", "--detach", ".worktrees/ahead", "origin/main"]);
        sh(&r, &["worktree", "add", "-q", "--detach", ".worktrees/mxp-gs12", "origin/main"]);
        std::fs::write(wts.join("dirty/new.txt"), "work").unwrap();
        std::fs::write(wts.join("ahead/more.txt"), "work").unwrap();
        sh(&wts.join("ahead"), &["add", "more.txt"]);
        sh(&wts.join("ahead"), &["commit", "-qm", "local only"]);
        let o = owners(&t.path().join("no-tmp"), &["mxp-gs12"]);

        assert_eq!(refusal(&wts.join("clean"), &o).await, None, "clean pushed worktree");
        let d = refusal(&wts.join("dirty"), &o).await.expect("dirty refused");
        assert_eq!(d.class, CLASS_WT_DIRTY);
        let a = refusal(&wts.join("ahead"), &o).await.expect("ahead refused");
        assert_eq!(a.class, CLASS_WT_UNPUSHED);
        let l = refusal(&wts.join("mxp-gs12"), &o).await.expect("live refused");
        assert_eq!(l.class, CLASS_WT_LIVE);

        // The directory holding them all is refused for what it contains.
        let all = refusal(&wts, &o).await.expect(".worktrees refused");
        assert!(all.reason.starts_with("contains "), "{}", all.reason);
    }

    #[tokio::test]
    async fn a_directory_above_a_repo_is_refused_for_the_repo_it_holds() {
        let t = tmpdir();
        repo(t.path());
        let o = owners(&t.path().join("no-tmp"), &[]);
        let r = refusal(t.path(), &o).await.expect("parent of a repo refused");
        assert_eq!(r.class, CLASS_GIT_TRACKED);
        assert!(r.reason.starts_with("contains "), "{}", r.reason);
    }

    #[tokio::test]
    async fn a_worktree_whose_gitdir_is_gone_fails_closed() {
        let t = tmpdir();
        let wt = t.path().join("orphan");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: /nonexistent/worktrees/orphan\n").unwrap();
        let o = owners(&t.path().join("no-tmp"), &[]);
        let r = refusal(&wt, &o).await.expect("refused");
        assert_eq!(r.class, CLASS_UNMEASURED);
    }
}
