//! A `kind=shell` schedule run that outlives the server process.
//!
//! Before this, the server owned each run's stdout/stderr pipes, so a restart
//! (every deploy, every self-adoption exec) closed them and the run was
//! recorded "interrupted: server restarted before the shell result was
//! recorded". To avoid that the builder and the server held deploys while a
//! shell run was in flight: 16 of 109 Mac deploys were held, for up to 530 s
//! (docs/design/cicd-two-minutes.md, item 1).
//!
//! Now each run is a detached process tree with files as its only interface:
//!
//! ```text
//! ~/.amux/shell-runs/<schedule>/<start_ms>.run.json  meta: run row id, attempt, start
//! ~/.amux/shell-runs/<schedule>/<start_ms>.pid       written by the wrapper itself
//! ~/.amux/shell-runs/<schedule>/<start_ms>.stdout    raw output (0600, removed once read)
//! ~/.amux/shell-runs/<schedule>/<start_ms>.stderr
//! ~/.amux/shell-runs/<schedule>/<start_ms>.rc        exit code, written atomically at exit
//! ~/.amux/shell-runs/<schedule>/<start_ms>.log       the redacted log the Shell tab tails
//! ```
//!
//! The child runs in its own session (`setsid`), so neither a launchd/systemd
//! stop of the server nor an exec of a new server image reaches it. The server
//! polls the files; a new server process finds the run by its meta file and
//! ADOPTS it (keeps watching, or records an exit code that landed while it was
//! down). Only a run whose process is gone with no exit code is lost.

use std::path::{Path, PathBuf};
use std::sync::Arc;

tokio::task_local! {
    /// The `schedule_runs` row a shell delivery is filling. Set by the cron
    /// fire and by Run now around `Deliverer::deliver`, so the run's meta file
    /// can name its row and a later server can adopt it.
    pub static SHELL_RUN_ID: i64;
}

/// The run row id of the delivery in progress, if a caller set one.
pub fn current_run_id() -> Option<i64> {
    SHELL_RUN_ID.try_with(|v| *v).ok()
}

/// How often a watcher reads the run's files.
const POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// What the server writes before it spawns the run.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RunMeta {
    pub run_id: Option<i64>,
    pub schedule_id: String,
    pub started_ms: i64,
    /// 1, or 2 for `retry_once_then_alert`'s second attempt.
    pub attempt: u32,
    /// The worker run token's hash file, removed when the run is recorded.
    #[serde(default)]
    pub token_path: Option<PathBuf>,
}

/// The files of one run, by their shared stem (`<dir>/<start_ms>`).
#[derive(Clone, Debug)]
pub struct RunFiles {
    stem: PathBuf,
}

impl RunFiles {
    pub fn from_stem(stem: PathBuf) -> Self {
        Self { stem }
    }
    fn with(&self, ext: &str) -> PathBuf {
        let mut s = self.stem.clone().into_os_string();
        s.push(".");
        s.push(ext);
        PathBuf::from(s)
    }
    pub fn stem(&self) -> &Path {
        &self.stem
    }
    pub fn log(&self) -> PathBuf {
        self.with("log")
    }
    pub fn stdout(&self) -> PathBuf {
        self.with("stdout")
    }
    pub fn stderr(&self) -> PathBuf {
        self.with("stderr")
    }
    pub fn rc(&self) -> PathBuf {
        self.with("rc")
    }
    pub fn pid(&self) -> PathBuf {
        self.with("pid")
    }
    pub fn meta(&self) -> PathBuf {
        self.with("run.json")
    }
    pub fn read_rc(&self) -> Option<i32> {
        std::fs::read_to_string(self.rc()).ok()?.trim().parse().ok()
    }
    pub fn read_pid(&self) -> Option<i32> {
        std::fs::read_to_string(self.pid()).ok()?.trim().parse().ok().filter(|p: &i32| *p > 1)
    }
    pub fn read_meta(&self) -> Option<RunMeta> {
        serde_json::from_slice(&std::fs::read(self.meta()).ok()?).ok()
    }
    pub fn write_meta(&self, meta: &RunMeta) -> std::io::Result<()> {
        let tmp = self.with("run.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(meta).unwrap_or_default())?;
        std::fs::rename(tmp, self.meta())
    }
    /// The raw output: it may hold vault values, so it goes once it is read.
    pub fn remove_raw(&self) {
        for p in [self.stdout(), self.stderr(), self.pid()] {
            remove_quietly(&p);
        }
    }
    /// Everything but the log, once the run's row is recorded.
    pub fn remove_all_but_log(&self) {
        self.remove_raw();
        for p in [self.rc(), self.meta(), self.with("rc.tmp")] {
            remove_quietly(&p);
        }
    }
}

/// Remove a run file that may already be gone. Anything but "not found" is
/// said, so a run directory that cannot be cleaned shows in the log.
fn remove_quietly(p: &Path) {
    if let Err(e) = std::fs::remove_file(p) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(path = %p.display(), error = %e, verdict = "shell_run_file_not_removed", measured = true, n_considered = 1,
                "could not remove a shell run file");
        }
    }
}

/// Make a fresh run's files under `dir`, keeping at most `keep` logs. The
/// stem is the start time in ms, bumped past any stem already in use.
pub fn new_run_files(dir: &Path, keep: usize) -> Option<RunFiles> {
    std::fs::create_dir_all(dir).ok()?;
    let mut logs: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "log"))
        .map(|e| e.path())
        .collect();
    logs.sort();
    while logs.len() >= keep.max(1) {
        let old = logs.remove(0);
        let files = RunFiles::from_stem(old.with_extension(""));
        // Never prune a run that is still going.
        if files.meta().exists() && files.read_rc().is_none() && files.read_pid().is_some_and(pid_alive) {
            continue;
        }
        files.remove_all_but_log();
        remove_quietly(&old);
    }
    let mut ms = chrono::Utc::now().timestamp_millis();
    loop {
        let files = RunFiles::from_stem(dir.join(ms.to_string()));
        if !files.log().exists() && !files.meta().exists() {
            return Some(files);
        }
        ms += 1;
    }
}

/// The wrapper: record its own pid, run the command with pipefail and its
/// output in files, then publish the exit code with a rename so a reader never
/// sees a half-written one. `$1` is the command, `$2` the stem.
const WRAPPER: &str = r#"p=$2; echo $$ > "$p.pid"; /bin/bash -o pipefail -c "$1" >>"$p.stdout" 2>>"$p.stderr" </dev/null; rc=$?; echo "$rc" > "$p.rc.tmp" && mv -f "$p.rc.tmp" "$p.rc""#;

/// The detached command for one run. The caller adds the environment.
pub fn detached_command(cmd: &str, files: &RunFiles) -> std::io::Result<tokio::process::Command> {
    // The raw files exist before the child does, private, so the command's
    // own umask is untouched and a vault value echoed is never world-readable.
    for p in [files.stdout(), files.stderr()] {
        let f = std::fs::OpenOptions::new().create(true).append(true).open(&p)?;
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = f.set_permissions(std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(path = %p.display(), error = %e, verdict = "shell_run_raw_output_not_private", measured = true, n_considered = 1,
                "could not make a shell run's raw output file private");
        }
    }
    let mut command = tokio::process::Command::new("/bin/bash");
    command
        .args(["-c", WRAPPER, "amux-shell-run"])
        .arg(cmd)
        .arg(files.stem())
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Not kill_on_drop: a dropped watcher (server shutdown) must leave
        // the run going for the next server to adopt.
        .kill_on_drop(false);
    // SAFETY: setsid is async-signal-safe and touches only the child. A new
    // session also makes the wrapper a process-group leader (pgid == pid), so
    // a timeout can still kill everything it started.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command)
}

/// Is `pid` a live process? Reaps it first when it is this process's own
/// child (an exec'd server inherits the old image's children), so a zombie
/// never reads as alive.
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 1 {
        return false;
    }
    let mut st = 0;
    // SAFETY: WNOHANG never blocks; for a pid that is not our child this is ECHILD.
    if unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) } == pid {
        return false;
    }
    // SAFETY: signal 0 only checks that the pid exists.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// SIGKILL a run's whole process group, only if `pid` still leads one.
pub fn kill_group(pid: i32) {
    // SAFETY: getpgid/kill on a pid we started; the group check keeps a
    // reused pid that leads no group of its own from being signalled.
    unsafe {
        if pid > 1 && libc::getpgid(pid) == pid {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

/// How the watcher knows the run's process is alive.
pub enum Proc {
    /// Spawned by this process: tokio owns the reaping.
    Child(tokio::process::Child),
    /// Adopted from a previous server process.
    Pid(i32),
}

impl Proc {
    fn pid(&self) -> Option<i32> {
        match self {
            Proc::Child(c) => c.id().map(|p| p as i32),
            Proc::Pid(p) => Some(*p),
        }
    }
    fn alive(&mut self) -> bool {
        match self {
            Proc::Child(c) => matches!(c.try_wait(), Ok(None)),
            Proc::Pid(p) => pid_alive(*p),
        }
    }
}

/// How a watched run ended.
#[derive(Debug)]
pub enum WatchEnd {
    /// (exit code, stdout, stderr, combined tail), all redacted.
    Exited(i32, String, String, String),
    TimedOut,
    /// The process is gone and wrote no exit code.
    Vanished,
}

/// Run rows whose files this process is watching right now, so a scheduler
/// loop restarted in-process does not adopt a run it is already watching.
static WATCHED: std::sync::Mutex<Vec<i64>> = std::sync::Mutex::new(Vec::new());

pub struct Watched(Option<i64>);
impl Watched {
    pub fn enter(run_id: Option<i64>) -> Self {
        if let (Some(id), Ok(mut v)) = (run_id, WATCHED.lock()) {
            v.push(id);
        }
        Self(run_id)
    }
}
impl Drop for Watched {
    fn drop(&mut self) {
        if let (Some(id), Ok(mut v)) = (self.0, WATCHED.lock()) {
            if let Some(i) = v.iter().position(|x| *x == id) {
                v.remove(i);
            }
        }
    }
}
pub fn is_watched(run_id: i64) -> bool {
    WATCHED.lock().map(|v| v.contains(&run_id)).unwrap_or(false)
}

struct Pump {
    off_out: u64,
    off_err: u64,
    out: Vec<u8>,
    err: Vec<u8>,
    combined: Vec<u8>,
    dropped: usize,
}

fn read_from(p: &Path, off: &mut u64) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(p) else { return Vec::new() };
    if f.seek(SeekFrom::Start(*off)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    // A partial read keeps what it got; the next poll reads the rest.
    if f.read_to_end(&mut buf).is_err() && buf.is_empty() {
        return buf;
    }
    *off += buf.len() as u64;
    buf
}

fn keep_tail(v: &mut Vec<u8>, keep: usize) -> usize {
    if v.len() > keep {
        let cut = v.len() - keep;
        v.drain(..cut);
        cut
    } else {
        0
    }
}

/// Watch a run until it exits, times out (`timeout_s` from `started_ms`) or
/// vanishes. Output is copied, redacted, into `log` as it lands.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn watch(
    files: &RunFiles,
    started_ms: i64,
    mut proc: Proc,
    vault: Arc<Vec<(String, String)>>,
    log: &mut super::scheduler::ShellRunLog,
    timeout_s: u64,
    keep: usize,
    tail_bytes: usize,
) -> WatchEnd {
    let mut p = Pump { off_out: 0, off_err: 0, out: Vec::new(), err: Vec::new(), combined: Vec::new(), dropped: 0 };
    let red = |b: &[u8]| -> Vec<u8> {
        if vault.is_empty() { b.to_vec() } else { crate::api::vault_secrets::redact_bytes(b, &vault) }
    };
    let pump = |p: &mut Pump, log: &mut super::scheduler::ShellRunLog| {
        for which in 0..2 {
            let (path, off) = if which == 0 { (files.stdout(), &mut p.off_out) } else { (files.stderr(), &mut p.off_err) };
            let b = read_from(&path, off);
            if b.is_empty() {
                continue;
            }
            log.write(&red(&b));
            let dst = if which == 0 { &mut p.out } else { &mut p.err };
            dst.extend_from_slice(&b);
            keep_tail(dst, keep);
            p.combined.extend_from_slice(&b);
            p.dropped += keep_tail(&mut p.combined, keep);
        }
    };
    let code = loop {
        pump(&mut p, log);
        if let Some(code) = files.read_rc() {
            pump(&mut p, log);
            break code;
        }
        if !proc.alive() {
            // The wrapper publishes the code before it exits.
            if let Some(code) = files.read_rc() {
                pump(&mut p, log);
                break code;
            }
            return WatchEnd::Vanished;
        }
        let elapsed_ms = chrono::Utc::now().timestamp_millis() - started_ms;
        if elapsed_ms >= (timeout_s as i64) * 1000 {
            if let Some(pid) = proc.pid() {
                kill_group(pid);
            }
            pump(&mut p, log);
            log.write(format!("\n[timed out after {timeout_s}s; its process group was killed]\n").as_bytes());
            return WatchEnd::TimedOut;
        }
        tokio::time::sleep(POLL).await;
    };
    // Reap our own child promptly rather than leaving it to the orphan queue.
    if let Proc::Child(c) = &mut proc {
        if let Err(e) = c.try_wait() {
            tracing::debug!(error = %e, "shell run's child could not be reaped here; tokio's orphan queue will");
        }
    }
    log.write(format!("\n[exit {code}]\n").as_bytes());
    let tail = super::scheduler::shell_output_tail(&p.combined, p.dropped, tail_bytes);
    let r = |b: &[u8]| crate::api::vault_secrets::redact(&String::from_utf8_lossy(b), &vault);
    WatchEnd::Exited(code, r(&p.out), r(&p.err), crate::api::vault_secrets::redact(&tail, &vault))
}

/// One running shell row found at startup, and what its files say.
#[derive(Debug)]
pub struct Found {
    pub files: RunFiles,
    pub meta: RunMeta,
    /// Exit code already on disk: the run ended while no server was watching.
    pub rc: Option<i32>,
    pub pid: Option<i32>,
}

/// The newest attempt's files for run row `run_id` of `schedule_id`.
pub fn find_run(dir: &Path, run_id: i64) -> Option<(RunFiles, RunMeta)> {
    let mut best: Option<(RunFiles, RunMeta)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".run.json") else { continue };
        let files = RunFiles::from_stem(dir.join(stem));
        let Some(meta) = files.read_meta() else { continue };
        if meta.run_id != Some(run_id) {
            continue;
        }
        if best.as_ref().is_none_or(|(_, b)| (meta.attempt, meta.started_ms) > (b.attempt, b.started_ms)) {
            best = Some((files, meta));
        }
    }
    best
}

/// Decide what a server starting up does with one running shell row:
/// `Some(found)` to adopt it, `None` when it is lost.
pub fn inspect(dir: &Path, run_id: i64) -> Option<Found> {
    let (files, meta) = find_run(dir, run_id)?;
    let rc = files.read_rc();
    let pid = files.read_pid();
    if rc.is_some() || pid.is_some_and(pid_alive) {
        return Some(Found { files, meta, rc, pid });
    }
    None
}

/// Remove the leftovers of runs no row is waiting on: finished (rc on disk)
/// or dead, and not being watched. Returns how many were removed.
pub fn sweep_leftovers(root: &Path, keep_run_ids: &[i64]) -> usize {
    let mut n = 0;
    let Ok(dirs) = std::fs::read_dir(root) else { return 0 };
    for d in dirs.flatten() {
        let Ok(entries) = std::fs::read_dir(d.path()) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".run.json") else { continue };
            let files = RunFiles::from_stem(d.path().join(stem));
            let meta = files.read_meta();
            if meta.as_ref().and_then(|m| m.run_id).is_some_and(|id| keep_run_ids.contains(&id) || is_watched(id)) {
                continue;
            }
            if files.read_rc().is_none() && files.read_pid().is_some_and(pid_alive) {
                continue;
            }
            if let Some(t) = meta.and_then(|m| m.token_path) {
                remove_quietly(&t);
            }
            files.remove_all_but_log();
            n += 1;
        }
    }
    n
}
