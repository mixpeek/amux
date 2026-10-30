//! Storage tiers: watched folders move between this Mac's internal disk and a
//! chosen cold drive by how recently they were used.
//!
//! Ethan, 2026-10-29: "folders that we watch and migrate them to and from our
//! selected hdd to local mac drive based on access patterns using whatever is
//! the native approach for doing so and its configurable in the disk amux tab".
//!
//! # What "used" means
//! macOS has no tiering of its own for arbitrary folders (Fusion Drive cannot
//! take a secondary disk on the Apple Silicon boot container). The native
//! signals are file times: APFS bumps a file's atime on read, at most once a day
//! per file, and its mtime on every write. Measured on this Mac 2026-10-29 on
//! both the internal SSD and the USB mirror. So a folder was used when any file
//! under it was written, or read more than about a day after its last atime
//! bump. Directory atimes are ignored, because listing a folder (including our
//! own measuring walk) updates them.
//!
//! # The policy, and where each rule comes from
//! Tiering systems (hierarchical storage managers, and caches such as ARC and
//! 2Q) converge on the same few rules, and this follows them:
//! - **Recency to demote.** A folder idle for its `idle_days` moves out.
//! - **Frequency to recall, and scan resistance.** One read does not recall a
//!   folder: reads must land on `recall_read_days` separate days within a week.
//!   A pass that touches at least 90% of a folder's files at once is a scan
//!   (the nightly restic backup reads all of `/Volumes/Bulk/archive`; so do
//!   indexers and `grep -r`) and does not count at all.
//! - **Writes recall at once.** A write means active work, and a spinning disk
//!   is slow for it.
//! - **Hysteresis.** A folder that just came back stays at least
//!   `residency_hours` before it may move out again, so nothing ping-pongs.
//! - **Capacity watermarks.** Below `pressure_free_pct` free on the Mac, the
//!   idle limit drops to a quarter (at least a day), coldest folders first,
//!   until `target_free_pct` is free again.
//!
//! The point of all of it is that a person or an agent never notices: paths
//! keep working through the link, nothing moves while it is open, and moves are
//! rare enough to be predictable.
//!
//! # Folders that keep growing
//! A folder that gains new runs every day never goes idle as a whole, so a
//! watched folder can be marked `each_subfolder`: it becomes a root that never
//! moves itself, and every immediate subfolder is aged and moved on its own,
//! discovered on every tick as it appears. Old runs go to the cold drive while
//! the newest stay on the Mac, which is how tiering systems age append-only
//! data: per item, not per container.
//!
//! # A move is a journal, not a task
//! The builder re-execs this binary on most commits to main, often several
//! times an hour, so a copy of a million files cannot live in one in-memory
//! task. Each folder's `Op { kind, phase }` is saved before every step, every
//! step is idempotent, and `run_op` resumes whatever phase it finds: on every
//! tick and right after startup. The copy is rsync, so a resumed copy sends only
//! what is missing.
//!
//! ```text
//! to HDD:  Copy  rsync src -> dest until a dry run shows no difference
//!          Swap  src -> src.amux-tier-old, symlink src -> dest,
//!                then merge old into dest without deleting or overwriting newer
//!          Cleanup  remove src.amux-tier-old
//! to Mac:  Copy  rsync dest -> src.amux-tier-new until it verifies
//!          Swap  final sync, link -> src.amux-tier-link, new -> src
//!          Cleanup  merge anything written to dest mid-swap, remove link and dest
//! ```
//!
//! Nothing on the source side changes until the copy verifies, and a source is
//! removed only after the swapped path resolves to the copy.
//!
//! # Who decides what may move
//! The same rules as Disk Cleanup: `reclaim::guard_path` (system trees, home
//! itself and its top-level folders, amux's own state) and
//! `reclaim_owner::refusal` (a running worker's scratch, git-tracked files, a
//! worktree with unpushed work). Two more apply because a moved folder leaves a
//! symlink behind: inside a repo the folder itself must be git-ignored, or the
//! link would appear in every lane's `git status`; and nothing may be open in
//! it. Every refusal is recorded on the folder with its class and logged with
//! `verdict = "tier_refused"`.

use super::reclaim::{df_bytes, guard_path, home_dir, now_secs};
use super::reclaim_owner;
use super::AppState;
use axum::extract::Query;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const JOB: &str = "tiering";
const TICK_SECS: u64 = 300;
/// A folder is re-measured at most this often unless a move or a click asks.
const STATS_TTL_S: i64 = 6 * 3600;
const MEASURE_BUDGET: Duration = Duration::from_secs(180);
/// Walking time one interval or startup tick spends re-measuring stale folders.
/// Hog RCA 20261030-044026: the watched folders hold 14.3M files (323
/// subfolders of one lane's scratch root among them), all measured within
/// minutes of each other, so all of them went stale together and each tick
/// walked every one back to back: 9.7M files between 09:00 and 09:20Z, and
/// measure_since was 46% of the server's CPU in a 30 s sample. A stale folder
/// past this budget waits for the next tick, which spreads a wave over ticks.
const MEASURE_TICK_BUDGET: Duration = Duration::from_secs(45);
/// A use must land this long after the move finished to count. Every write the
/// move itself makes happens before that moment, so this only absorbs
/// timestamp rounding (one second on APFS).
const USE_GRACE_S: i64 = 10;
/// A pass touching at least this share of a folder's files is a scan.
const SCAN_SHARE: f64 = 0.9;
/// Reads are counted over this many days.
const READ_WINDOW_DAYS: i64 = 7;
/// A copy that keeps failing is abandoned after this many ticks.
const MAX_COPY_ATTEMPTS: u32 = 5;
const OLD: &str = ".amux-tier-old";
const NEW: &str = ".amux-tier-new";
const LINK: &str = ".amux-tier-link";

static FILE_LOCK: Mutex<()> = Mutex::new(());
static TICKING: AtomicBool = AtomicBool::new(false);
/// Set when a click arrives while a tick runs, so its request is served as soon
/// as that tick ends instead of at the next interval.
static RERUN: AtomicBool = AtomicBool::new(false);

fn yes() -> bool {
    true
}
fn default_days() -> u32 {
    14
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Config {
    /// Folder on the cold drive that receives moved folders, e.g.
    /// `/Volumes/Bulk/amux-tier`. Each folder keeps its absolute path below it.
    #[serde(default)]
    pub cold_root: String,
    /// Apply the policy on the periodic tick. Manual moves work either way.
    #[serde(default = "yes")]
    pub auto: bool,
    #[serde(default)]
    pub folders: Vec<Folder>,
    #[serde(default)]
    pub last_tick: Option<TickReport>,
    /// Recall a folder from the cold drive after reads on this many separate
    /// days within a week.
    #[serde(default = "default_recall")]
    pub recall_read_days: u32,
    /// Below this share of free space on the Mac, cold folders move out sooner.
    #[serde(default = "default_pressure")]
    pub pressure_free_pct: u32,
    /// ...until this share is free again.
    #[serde(default = "default_target")]
    pub target_free_pct: u32,
    /// A folder that came back stays at least this long before moving out again.
    #[serde(default = "default_residency")]
    pub residency_hours: u32,
}

fn default_recall() -> u32 {
    3
}
fn default_pressure() -> u32 {
    15
}
fn default_target() -> u32 {
    25
}
fn default_residency() -> u32 {
    24
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cold_root: String::new(),
            auto: true,
            folders: Vec::new(),
            last_tick: None,
            recall_read_days: default_recall(),
            pressure_free_pct: default_pressure(),
            target_free_pct: default_target(),
            residency_hours: default_residency(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Folder {
    pub path: String,
    /// Move to the cold drive after this many days without a use.
    #[serde(default = "default_days")]
    pub idle_days: u32,
    /// Bring it back to the Mac when it is used while on the cold drive.
    #[serde(default = "yes")]
    pub promote_on_use: bool,
    #[serde(default)]
    pub added_at: i64,
    /// Where the folder lives on the cold drive, while it is there.
    #[serde(default)]
    pub dest: Option<String>,
    #[serde(default)]
    pub demoted_at: Option<i64>,
    /// When the move to the cold drive finished. Anything under the folder newer
    /// than this (plus `USE_GRACE_S`) is a use made while it lived there.
    #[serde(default)]
    pub baseline_used: Option<i64>,
    #[serde(default)]
    pub op: Option<Op>,
    #[serde(default)]
    pub stats: Option<Stats>,
    #[serde(default)]
    pub last_refusal: Option<Note>,
    #[serde(default)]
    pub last_move: Option<Note>,
    #[serde(default)]
    pub last_error: Option<Note>,
    /// Days (unix day numbers) on which reads, not scans, were seen while the
    /// folder was on the cold drive.
    #[serde(default)]
    pub read_days: Vec<i64>,
    /// When it last came back to the Mac.
    #[serde(default)]
    pub promoted_at: Option<i64>,
    /// A root: never moves itself; each immediate subfolder is its own entry.
    #[serde(default)]
    pub each_subfolder: bool,
    /// For a subfolder found under a root, that root's path.
    #[serde(default)]
    pub parent: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    ToHdd,
    ToMac,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Queued,
    Copy,
    Swap,
    Cleanup,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Op {
    pub kind: Kind,
    pub phase: Phase,
    pub dest: String,
    pub manual: bool,
    #[serde(default)]
    pub requested_by: String,
    pub started_at: i64,
    #[serde(default)]
    pub attempts: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Stats {
    pub bytes: u64,
    pub files: u64,
    /// Newest file mtime/atime or directory mtime found, unix seconds.
    pub last_used: i64,
    pub measured_at: i64,
    /// False when the walk ran out of its time budget; the numbers are then
    /// lower bounds and the policy will not move the folder on them.
    pub complete: bool,
    /// Newest mtime of any file or directory: the last write.
    #[serde(default)]
    pub newest_write: i64,
    /// Files whose atime moved past the previous measurement: reads since then.
    #[serde(default)]
    pub touched: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Note {
    pub at: i64,
    pub class: String,
    pub text: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct TickReport {
    pub at: i64,
    pub trigger: String,
    pub considered: u32,
    pub moved: u32,
    pub refused: u32,
    pub resumed: u32,
    pub notes: Vec<String>,
    /// The Mac was under the low-space watermark when the tick began.
    #[serde(default)]
    pub pressure: bool,
    /// Time this tick spent walking folders to measure them.
    #[serde(default)]
    pub measured_ms: u64,
    /// Stale folders left for the next tick because the walking budget ran out.
    #[serde(default)]
    pub deferred: u32,
}

/// Whether a stale folder is re-measured in this tick. A person asking (a click,
/// a folder just added) and a folder with a move under way always are; the
/// interval and startup ticks stop once they have walked MEASURE_TICK_BUDGET.
fn measure_this_tick(forced: bool, has_op: bool, walked: Duration) -> bool {
    forced || has_op || walked < MEASURE_TICK_BUDGET
}

fn note(class: &str, text: impl Into<String>) -> Note {
    Note { at: now_secs(), class: class.into(), text: text.into() }
}

// ── persistence ─────────────────────────────────────────────────────────────

fn config_path() -> PathBuf {
    crate::config::amux_home().join("tiering.json")
}

/// A missing file is a fresh config. An unreadable one is an error, so a save
/// never clobbers a config somebody can still repair.
pub(crate) fn load() -> Result<Config, String> {
    match std::fs::read_to_string(config_path()) {
        Ok(s) => serde_json::from_str(&s).map_err(|e| format!("tiering.json is not valid: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("cannot read tiering.json: {e}")),
    }
}

fn save(c: &Config) -> Result<(), String> {
    let p = config_path();
    let tmp = p.with_extension(format!("json.tmp.{}", std::process::id()));
    let body = serde_json::to_string_pretty(c).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, body).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("rename {}: {e}", p.display()))
}

fn update<R>(f: impl FnOnce(&mut Config) -> R) -> Result<R, String> {
    let _g = FILE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut c = load()?;
    let r = f(&mut c);
    save(&c)?;
    Ok(r)
}

fn update_folder(path: &str, f: impl FnOnce(&mut Folder)) {
    let r = update(|c| {
        if let Some(x) = c.folders.iter_mut().find(|x| x.path == path) {
            f(x);
        }
    });
    if let Err(e) = r {
        tracing::warn!(path, error = %e, verdict = "tier_state_write_failed", "tiering: could not save folder state");
    }
}

// ── filesystem facts ────────────────────────────────────────────────────────

fn sibling(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

pub(crate) fn dest_for(cold_root: &Path, src: &Path) -> PathBuf {
    cold_root.join(src.strip_prefix("/").unwrap_or(src))
}

/// Where the folder is right now, read from the filesystem.
pub(crate) fn location(f: &Folder) -> &'static str {
    let p = Path::new(&f.path);
    match std::fs::symlink_metadata(p) {
        Err(_) => "missing",
        Ok(m) if m.file_type().is_symlink() => {
            let ours = match (std::fs::read_link(p), &f.dest) {
                (Ok(t), Some(d)) => t == Path::new(d),
                _ => false,
            };
            if !ours {
                "foreign_link"
            } else if p.is_dir() {
                "hdd"
            } else {
                "hdd_unavailable"
            }
        }
        Ok(m) if m.is_dir() => "mac",
        Ok(_) => "not_a_folder",
    }
}

/// Size and newest use under `root`. Follows `root` itself when it is a link,
/// never a link below it. File atime and mtime count; directory atime does not.
pub(crate) fn measure(root: &Path, budget: Duration) -> Stats {
    measure_since(root, budget, i64::MAX)
}

/// `measure`, also counting files read (atime) after `since`.
pub(crate) fn measure_since(root: &Path, budget: Duration, since: i64) -> Stats {
    let start = Instant::now();
    let mut st = Stats { bytes: 0, files: 0, last_used: 0, measured_at: now_secs(), complete: true, newest_write: 0, touched: 0 };
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if start.elapsed() > budget {
            st.complete = false;
            break;
        }
        if let Ok(m) = std::fs::metadata(&dir) {
            st.last_used = st.last_used.max(m.mtime());
            st.newest_write = st.newest_write.max(m.mtime());
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            // DirEntry::metadata does not follow symlinks on unix.
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
                continue;
            }
            st.files += 1;
            st.bytes += m.blocks() * 512;
            st.last_used = st.last_used.max(m.mtime()).max(m.atime());
            st.newest_write = st.newest_write.max(m.mtime());
            if m.atime() > since {
                st.touched += 1;
            }
        }
    }
    st
}

/// Is any file under `p` open, or any process working inside it?
fn open_under(p: &Path) -> Result<bool, String> {
    let exact = p.to_string_lossy().to_string();
    let prefix = format!("{exact}/");
    let hit = |s: &str| s == exact || s.starts_with(&prefix);
    #[cfg(target_os = "linux")]
    {
        let rd = std::fs::read_dir("/proc").map_err(|e| format!("/proc: {e}"))?;
        for pid in rd.flatten() {
            let base = pid.path();
            if let Ok(cwd) = std::fs::read_link(base.join("cwd")) {
                if hit(&cwd.to_string_lossy()) {
                    return Ok(true);
                }
            }
            if let Ok(fds) = std::fs::read_dir(base.join("fd")) {
                for fd in fds.flatten() {
                    if let Ok(t) = std::fs::read_link(fd.path()) {
                        if hit(&t.to_string_lossy()) {
                            return Ok(true);
                        }
                    }
                }
            }
        }
        Ok(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // lsof lives in /usr/sbin on macOS, which a launchd-started server's
        // PATH does not include (found by e2e: "lsof: No such file or
        // directory"). Resolve it ourselves rather than trust the environment.
        let lsof = ["/usr/sbin/lsof", "/usr/bin/lsof"].into_iter().find(|p| Path::new(p).exists()).unwrap_or("lsof");
        // lsof exits 1 when it could not stat some file; the listing is still good.
        let out = Command::new(lsof)
            .args(["-n", "-P", "-Fn"])
            .output()
            .map_err(|e| format!("lsof: {e}"))?;
        let s = String::from_utf8_lossy(&out.stdout);
        if s.trim().is_empty() {
            return Err("lsof printed nothing".into());
        }
        Ok(s.lines().filter_map(|l| l.strip_prefix('n')).any(hit))
    }
}

fn slash(p: &Path) -> String {
    format!("{}/", p.display())
}

/// rsync by absolute path, then on PATH, or None. Checked before a move is
/// queued: CI's e2e container had no rsync, every copy failed and the folder
/// sat in "moving" through five retries (fixed in CI by 50d4a37f8; this makes
/// the server say so at once instead).
pub(crate) fn rsync_bin() -> Option<PathBuf> {
    ["/usr/bin/rsync", "/opt/homebrew/bin/rsync", "/usr/local/bin/rsync"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .or_else(|| std::env::var_os("PATH").and_then(|v| std::env::split_paths(&v).map(|d| d.join("rsync")).find(|p| p.exists())))
}

fn rsync(args: &[&str]) -> Result<String, String> {
    let bin = rsync_bin().ok_or_else(|| "rsync is not installed".to_string())?;
    let out = Command::new(bin).args(args).output().map_err(|e| format!("rsync: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "rsync {} exited {}: {}",
            args.join(" "),
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Make `to` an exact copy of `from`.
fn mirror(from: &Path, to: &Path) -> Result<(), String> {
    rsync(&["-aH", "--delete", &slash(from), &slash(to)]).map(|_| ())
}

/// Copy what `to` lacks, never deleting and never replacing a newer file.
fn merge_newer(from: &Path, to: &Path) -> Result<(), String> {
    rsync(&["-aH", "--update", &slash(from), &slash(to)]).map(|_| ())
}

fn differs(from: &Path, to: &Path) -> Result<bool, String> {
    let o = rsync(&["-aHn", "--delete", "--itemize-changes", &slash(from), &slash(to)])?;
    Ok(o.lines().any(|l| !l.trim().is_empty()))
}

fn copy_verified(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("create {}: {e}", to.display()))?;
    for _ in 0..3 {
        mirror(from, to)?;
        if !differs(from, to)? {
            return Ok(());
        }
    }
    Err(format!("{} kept changing while it was copied", from.display()))
}

/// A destination we are about to delete must be a real folder well below `/`.
fn safe_to_remove(p: &Path) -> bool {
    p.is_absolute()
        && p.components().count() >= 4
        && !p.components().any(|c| c.as_os_str() == "..")
        && std::fs::symlink_metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

fn io(what: &str, p: &Path, e: std::io::Error) -> String {
    format!("{what} {}: {e}", p.display())
}

// ── the move itself ─────────────────────────────────────────────────────────

/// Advance `op` on `path` to completion, saving each phase through `set_phase`
/// before doing it. Safe to call again from any phase it was interrupted in.
pub(crate) fn run_op(path: &str, op: &Op, set_phase: &dyn Fn(Phase)) -> Result<(), String> {
    let src = Path::new(path);
    let dest = PathBuf::from(&op.dest);
    let mut phase = op.phase;
    match op.kind {
        Kind::ToHdd => {
            let old = sibling(src, OLD);
            if matches!(phase, Phase::Queued | Phase::Copy) {
                set_phase(Phase::Copy);
                let real = std::fs::symlink_metadata(src).map(|m| m.is_dir()).unwrap_or(false);
                if real {
                    copy_verified(src, &dest)?;
                } else if !old.is_dir() {
                    return Err(format!("{} is not a folder on this Mac", src.display()));
                }
                phase = Phase::Swap;
                set_phase(phase);
            }
            if phase == Phase::Swap {
                match std::fs::symlink_metadata(src) {
                    Ok(m) if m.is_dir() => {
                        std::fs::rename(src, &old).map_err(|e| io("rename", src, e))?;
                        if let Err(e) = symlink(&dest, src) {
                            let _ = std::fs::rename(&old, src);
                            return Err(io("symlink", src, e));
                        }
                    }
                    Ok(m) if m.file_type().is_symlink() => {
                        if std::fs::read_link(src).ok().as_deref() != Some(dest.as_path()) {
                            return Err(format!("{} is a symlink to somewhere else", src.display()));
                        }
                    }
                    Err(_) if old.is_dir() => symlink(&dest, src).map_err(|e| io("symlink", src, e))?,
                    _ => return Err(format!("{} vanished mid-move and no copy was kept", src.display())),
                }
                if old.is_dir() {
                    merge_newer(&old, &dest)?;
                }
                if !src.is_dir() {
                    return Err(format!("{} does not resolve; the Mac copy is kept at {}", src.display(), old.display()));
                }
                phase = Phase::Cleanup;
                set_phase(phase);
            }
            if old.exists() {
                std::fs::remove_dir_all(&old).map_err(|e| io("remove", &old, e))?;
            }
        }
        Kind::ToMac => {
            let new = sibling(src, NEW);
            let link = sibling(src, LINK);
            let is_link = || std::fs::symlink_metadata(src).map(|m| m.file_type().is_symlink()).unwrap_or(false);
            if matches!(phase, Phase::Queued | Phase::Copy) {
                set_phase(Phase::Copy);
                if is_link() {
                    if !dest.is_dir() {
                        return Err(format!("{} is not reachable; is the drive connected?", dest.display()));
                    }
                    copy_verified(&dest, &new)?;
                }
                phase = Phase::Swap;
                set_phase(phase);
            }
            if phase == Phase::Swap {
                if is_link() {
                    mirror(&dest, &new)?;
                    std::fs::rename(src, &link).map_err(|e| io("rename", src, e))?;
                    if let Err(e) = std::fs::rename(&new, src) {
                        let _ = std::fs::rename(&link, src);
                        return Err(io("rename", &new, e));
                    }
                } else if std::fs::symlink_metadata(src).is_err() && new.is_dir() {
                    std::fs::rename(&new, src).map_err(|e| io("rename", &new, e))?;
                }
                if !std::fs::symlink_metadata(src).map(|m| m.is_dir()).unwrap_or(false) {
                    return Err(format!("{} is not a real folder after the swap", src.display()));
                }
                phase = Phase::Cleanup;
                set_phase(phase);
            }
            if dest.is_dir() {
                merge_newer(&dest, src)?;
                if safe_to_remove(&dest) {
                    std::fs::remove_dir_all(&dest).map_err(|e| io("remove", &dest, e))?;
                }
            }
            if std::fs::symlink_metadata(&link).is_ok() {
                std::fs::remove_file(&link).map_err(|e| io("remove", &link, e))?;
            }
        }
    }
    Ok(())
}

// ── gates ───────────────────────────────────────────────────────────────────

/// `None` when the folder may move to the cold drive; otherwise (class, why).
async fn gate_to_hdd(f: &Folder, cold_root: &Path, owners: &reclaim_owner::Owners) -> Option<(String, String)> {
    let src = PathBuf::from(&f.path);
    if rsync_bin().is_none() {
        return Some(("no_rsync".into(), "rsync is not installed on this machine, so nothing can be copied".into()));
    }
    if cold_root.as_os_str().is_empty() {
        return Some(("no_drive".into(), "no cold drive is selected".into()));
    }
    if let Err(e) = ensure_cold_root(cold_root) {
        return Some(("drive_unavailable".into(), e));
    }
    if src.starts_with(cold_root) {
        return Some(("inside_cold_root".into(), "the folder is already on the cold drive".into()));
    }
    if let Err(why) = guard_path(&src) {
        return Some(("protected".into(), why));
    }
    let (owned, live_sub) = reclaim_owner::refusal_for_tiering(&src, owners).await;
    if let Some(r) = owned {
        return Some((r.class.to_string(), r.reason));
    }
    if live_sub {
        // A running worker's scratch: only a subfolder idle for its full limit,
        // whoever asks (Ethan, 2026-10-29: "Yes, idle subfolders").
        let idle_ok = f.stats.as_ref().is_some_and(|s| s.complete && now_secs() - s.last_used >= i64::from(f.idle_days) * 86_400);
        if !idle_ok {
            return Some((
                "live_scratch_recent".into(),
                format!("it is inside a running worker's scratch and was used within its {} idle days", f.idle_days),
            ));
        }
    }
    let probe = src.clone();
    let blocking = tokio::task::spawn_blocking(move || -> Option<(String, String)> {
        if let Some(why) = link_would_show_in_git(&probe) {
            return Some(("git_visible".into(), why));
        }
        match open_under(&probe) {
            Ok(true) => return Some(("in_use".into(), "a process has files open inside it".into())),
            Ok(false) => {}
            Err(e) => return Some(("unmeasured".into(), format!("could not check open files: {e}"))),
        }
        None
    })
    .await;
    match blocking {
        Ok(Some(r)) => return Some(r),
        Ok(None) => {}
        Err(e) => return Some(("unmeasured".into(), e.to_string())),
    }
    let need = f.stats.as_ref().map(|s| s.bytes).unwrap_or(0);
    match df_bytes(cold_root) {
        Some((free, _)) if free > need + need / 20 + (1 << 30) => None,
        Some((free, _)) => Some(("no_space".into(), format!("the cold drive has {} free, the folder needs {}", gb(free), gb(need)))),
        None => Some(("unmeasured".into(), "could not read the cold drive's free space".into())),
    }
}

async fn gate_to_mac(f: &Folder) -> Option<(String, String)> {
    if rsync_bin().is_none() {
        return Some(("no_rsync".into(), "rsync is not installed on this machine, so nothing can be copied".into()));
    }
    let Some(dest) = f.dest.clone().map(PathBuf::from) else {
        return Some(("not_on_hdd".into(), "the folder is not on the cold drive".into()));
    };
    if !dest.is_dir() {
        return Some(("drive_unavailable".into(), format!("{} is not reachable; is the drive connected?", dest.display())));
    }
    let probe = dest.clone();
    match tokio::task::spawn_blocking(move || open_under(&probe)).await {
        Ok(Ok(true)) => return Some(("in_use".into(), "a process has files open inside it".into())),
        Ok(Ok(false)) => {}
        Ok(Err(e)) => return Some(("unmeasured".into(), format!("could not check open files: {e}"))),
        Err(e) => return Some(("unmeasured".into(), e.to_string())),
    }
    let need = f.stats.as_ref().map(|s| s.bytes).unwrap_or(0);
    let parent = Path::new(&f.path).parent().unwrap_or(Path::new("/")).to_path_buf();
    match df_bytes(&parent) {
        Some((free, total)) => {
            // Keep 5% of the Mac's disk free, between 1 GB and 50 GB.
            let reserve = (total / 20).clamp(1 << 30, 50 << 30);
            if free > need + reserve {
                None
            } else {
                Some(("no_space".into(), format!("the Mac has {} free; bringing back {} would leave less than {}", gb(free), gb(need), gb(reserve))))
            }
        }
        None => Some(("unmeasured".into(), "could not read the Mac disk's free space".into())),
    }
}

/// Inside a repo, a symlink left where an un-ignored folder was would show up
/// as a new untracked file in every lane's `git status`.
fn link_would_show_in_git(p: &Path) -> Option<String> {
    let parent = p.parent()?;
    let top = Command::new("git").arg("-C").arg(parent).args(["rev-parse", "--show-toplevel"]).output().ok()?;
    if !top.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&top.stdout).trim().to_string();
    let ignored = Command::new("git").arg("-C").arg(&root).args(["check-ignore", "-q"]).arg(p).status().ok()?;
    if ignored.success() {
        None
    } else {
        Some(format!("it is inside the git repo {root} and is not git-ignored, so the link left behind would show up in git status"))
    }
}

/// Is `p` a mount point: a different device from its parent. A plain folder
/// named `/Volumes/Bulk` can sit on the boot disk while the drive is unplugged
/// (macOS leaves stale mount folders), and `is_dir()` alone would take it for
/// the drive: the status read "available" with the Mac's own free space, and a
/// move would have filled the Mac under the drive's name (land review F1).
pub(crate) fn is_mount(p: &Path) -> bool {
    let Some(parent) = p.parent() else { return true };
    match (std::fs::metadata(p), std::fs::metadata(parent)) {
        (Ok(a), Ok(b)) => a.is_dir() && a.dev() != b.dev(),
        _ => false,
    }
}

/// For a path under `/Volumes/<name>/...`, that volume; otherwise None.
fn volume_of(p: &Path) -> Option<PathBuf> {
    let mut comps = p.components();
    match (comps.next(), comps.next(), comps.next()) {
        (Some(_), Some(v), Some(name)) if v.as_os_str() == "Volumes" => Some(Path::new("/Volumes").join(name.as_os_str())),
        _ => None,
    }
}

fn ensure_cold_root(cold_root: &Path) -> Result<(), String> {
    // Under /Volumes, the volume itself must be MOUNTED, checked by device, not
    // just present: writing under an unplugged drive's leftover folder would
    // fill the Mac under the drive's name.
    if let Some(vol) = volume_of(cold_root) {
        if !is_mount(&vol) {
            return Err(format!("{} is not connected", vol.display()));
        }
    }
    if cold_root.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(cold_root).map_err(|e| format!("cannot create {}: {e}", cold_root.display()))
}

/// Human size for notes a person reads, in the same units the Disk tab shows
/// (its `_fmtBytes`, binary, plus decimal TB): a 272 KB folder once said
/// "0.0 GB", then "279 KB" beside a card reading "272.0 KB" (e2e, 2026-10-29).
/// The nearest existing folder at or above `p`. A cold folder that has not been
/// created yet has no free space of its own to read; its drive does. Reading the
/// missing path itself showed "0 B free of 0 B" on the live Disk tab.
pub(crate) fn nearest_existing(p: &Path) -> PathBuf {
    let mut cur = Some(p);
    while let Some(c) = cur {
        if c.exists() {
            return c.to_path_buf();
        }
        cur = c.parent();
    }
    PathBuf::from("/")
}

fn gb(b: u64) -> String {
    let f = b as f64;
    if b >= 1_000_000_000_000 {
        format!("{:.1} TB", f / 1e12)
    } else if b >= 1 << 30 {
        format!("{:.2} GB", f / (1u64 << 30) as f64)
    } else if b >= 1 << 20 {
        format!("{:.1} MB", f / (1u64 << 20) as f64)
    } else if b >= 1 << 10 {
        format!("{:.1} KB", f / 1024.0)
    } else {
        format!("{b} B")
    }
}

// ── roots ───────────────────────────────────────────────────────────────────

fn child(path: String, root: &Folder, adopted: Option<String>, now: i64) -> Folder {
    Folder {
        path,
        idle_days: root.idle_days,
        promote_on_use: root.promote_on_use,
        added_at: now,
        demoted_at: adopted.as_ref().map(|_| now),
        baseline_used: adopted.as_ref().map(|_| now),
        dest: adopted,
        op: None,
        stats: None,
        last_refusal: None,
        last_move: None,
        last_error: None,
        read_days: Vec::new(),
        promoted_at: None,
        each_subfolder: false,
        parent: Some(root.path.clone()),
    }
}

/// Bring each root's entries in line with what is on disk: a new subfolder is
/// added, one gone from disk (and not mid-move, and not on the cold drive) is
/// dropped. Hidden entries and our own `.amux-tier-*` staging names are skipped.
/// Returns (added, dropped).
pub(crate) fn discover(c: &mut Config) -> (usize, usize) {
    let now = now_secs();
    let roots: Vec<Folder> = c.folders.iter().filter(|f| f.each_subfolder).cloned().collect();
    let mut added = 0;
    for root in &roots {
        let Ok(rd) = std::fs::read_dir(&root.path) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || [OLD, NEW, LINK].iter().any(|s| name.ends_with(s)) {
                continue;
            }
            let p = e.path();
            let ps = p.to_string_lossy().to_string();
            if c.folders.iter().any(|x| x.path == ps) {
                continue;
            }
            let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
            let adopted = if m.file_type().is_symlink() {
                match std::fs::read_link(&p) {
                    Ok(t) if t.is_absolute() && t.starts_with("/Volumes") => Some(t.to_string_lossy().to_string()),
                    _ => continue,
                }
            } else if m.is_dir() {
                None
            } else {
                continue;
            };
            c.folders.push(child(ps, root, adopted, now));
            added += 1;
        }
    }
    let root_paths: Vec<String> = roots.iter().map(|r| r.path.clone()).collect();
    let before = c.folders.len();
    c.folders.retain(|f| match &f.parent {
        None => true,
        Some(r) => root_paths.contains(r) && (f.op.is_some() || f.dest.is_some() || std::fs::symlink_metadata(&f.path).is_ok()),
    });
    (added, before - c.folders.len())
}

// ── the tick ────────────────────────────────────────────────────────────────

pub fn spawn(_state: AppState) -> crate::runtime_jobs::PeriodicTask {
    // First pass shortly after startup, so a move interrupted by the restart
    // resumes in minutes rather than at the first interval.
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(30)).await;
        tick("startup").await;
    });
    crate::runtime_jobs::spawn_periodic(JOB, TICK_SECS, || async {
        tick("interval").await;
    })
}

/// Queue a tick without waiting for it. Returns false if one is running.
fn kick(trigger: &'static str) -> bool {
    if TICKING.load(Ordering::SeqCst) {
        RERUN.store(true, Ordering::SeqCst);
        return false;
    }
    tokio::spawn(async move {
        tick(trigger).await;
    });
    true
}

pub(crate) async fn tick(trigger: &'static str) -> Option<TickReport> {
    if TICKING.swap(true, Ordering::SeqCst) {
        return None;
    }
    let mut rep = tick_inner(trigger).await;
    // Bounded: a click that lands during the rerun gets one more pass, not a loop.
    for _ in 0..3 {
        if !RERUN.swap(false, Ordering::SeqCst) {
            break;
        }
        rep = tick_inner("rerun").await;
    }
    TICKING.store(false, Ordering::SeqCst);
    rep
}

async fn tick_inner(trigger: &'static str) -> Option<TickReport> {
    match update(discover) {
        Ok((a, d)) if a + d > 0 => tracing::info!(added = a, dropped = d, verdict = "tier_discovered", "tiering: subfolders of watched roots changed"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, verdict = "tier_state_write_failed", "tiering: could not refresh watched roots"),
    }
    let cfg = match load() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, verdict = "tier_config_unreadable", "tiering: config unreadable; nothing moves until it is fixed");
            return None;
        }
    };
    if cfg.folders.is_empty() {
        return None;
    }
    let cold_root = PathBuf::from(&cfg.cold_root);
    let owners = reclaim_owner::Owners::probe().await;
    let mut rep = TickReport { at: now_secs(), trigger: trigger.into(), ..Default::default() };
    // Capacity watermarks: low on space, move the coldest out sooner, until the
    // target is free again. `freed` is what this tick has moved off so far.
    let (mac_free, mac_total) = df_bytes(&home_dir()).unwrap_or((0, 0));
    let pct = |free: u64| free.saturating_mul(100).checked_div(mac_total).unwrap_or(100);
    let mut pressure = mac_total > 0 && pct(mac_free) < u64::from(cfg.pressure_free_pct);
    let mut freed: u64 = 0;
    rep.pressure = pressure;
    let mut folders = cfg.folders.clone();
    folders.sort_by_key(|f| f.stats.as_ref().map(|s| s.last_used).unwrap_or(0));
    let mut walked = Duration::ZERO;
    for f in folders {
        if f.each_subfolder {
            continue; // a root never moves; its subfolders are entries of their own
        }
        rep.considered += 1;
        let path = f.path.clone();

        // 1. Finish whatever was started. Gates already passed for it.
        if let Some(op) = f.op.clone().filter(|o| o.phase != Phase::Queued) {
            rep.resumed += 1;
            run_and_record(&path, op, &mut rep).await;
            continue;
        }

        // 2. Keep the measurement fresh enough to decide on.
        // A person asking ("Check now", a folder just added) re-measures
        // everything; the interval honours the TTL so it is not walking large
        // folders on a spinning disk every five minutes.
        let forced = matches!(trigger, "manual" | "added");
        let stale = forced || f.stats.as_ref().map(|s| now_secs() - s.measured_at > STATS_TTL_S).unwrap_or(true);
        if (stale || f.op.is_some()) && !measure_this_tick(forced, f.op.is_some(), walked) {
            // Left for the next tick, policy included: deciding on numbers
            // this old could move a folder that was used since.
            rep.deferred += 1;
            continue;
        }
        let mut f = f;
        if stale || f.op.is_some() {
            let root = PathBuf::from(&path);
            let started = Instant::now();
            let since = f.stats.as_ref().map(|s| s.measured_at).or(f.baseline_used).unwrap_or(i64::MAX);
            if let Ok(st) = tokio::task::spawn_blocking(move || measure_since(&root, MEASURE_BUDGET, since)).await {
                if location(&f) == "hdd" {
                    if let Some(kind) = observe_reads(&mut f, &st, now_secs()) {
                        tracing::info!(path = %path, touched = st.touched, files = st.files, verdict = if kind == "scan" { "tier_scan_ignored" } else { "tier_read_seen" }, "tiering: reads on a cold folder");
                        if kind == "scan" {
                            rep.notes.push(format!("{path}: ignored a scan that read {} of {} files", st.touched, st.files));
                        }
                    }
                }
                let days = f.read_days.clone();
                update_folder(&path, |x| {
                    x.stats = Some(st.clone());
                    x.read_days = days;
                });
                f.stats = Some(st);
            }
            walked += started.elapsed();
        }

        // 3. A queued click, or the policy.
        let want = match &f.op {
            Some(op) => Some((op.kind, true, op.requested_by.clone())),
            None if cfg.auto => {
                let ctx = PolicyCtx { now: now_secs(), pressure, recall_read_days: cfg.recall_read_days, residency_s: i64::from(cfg.residency_hours) * 3600 };
                policy(&f, &ctx).map(|k| (k, false, if pressure { "policy (low disk)" } else { "policy" }.to_string()))
            }
            None => None,
        };
        let Some((kind, manual, by)) = want else { continue };
        let refusal = match kind {
            Kind::ToHdd => gate_to_hdd(&f, &cold_root, &owners).await,
            Kind::ToMac => gate_to_mac(&f).await,
        };
        if let Some((class, why)) = refusal {
            rep.refused += 1;
            rep.notes.push(format!("{path}: {why}"));
            tracing::warn!(path = %path, class = %class, reason = %why, manual, verdict = "tier_refused", "tiering: move refused");
            let n = note(&class, why);
            update_folder(&path, |x| {
                x.last_refusal = Some(n);
                x.op = None;
            });
            continue;
        }
        let dest = match kind {
            Kind::ToHdd => dest_for(&cold_root, Path::new(&path)).to_string_lossy().to_string(),
            Kind::ToMac => f.dest.clone().unwrap_or_default(),
        };
        let op = Op { kind, phase: Phase::Copy, dest, manual, requested_by: by, started_at: now_secs(), attempts: 0 };
        let saved = op.clone();
        update_folder(&path, |x| {
            x.op = Some(saved);
            x.last_refusal = None;
        });
        let moved_before = rep.moved;
        run_and_record(&path, op, &mut rep).await;
        if kind == Kind::ToHdd && rep.moved > moved_before {
            freed += f.stats.as_ref().map(|s| s.bytes).unwrap_or(0);
            if pressure && pct(mac_free + freed) >= u64::from(cfg.target_free_pct) {
                pressure = false;
            }
        }
    }
    rep.measured_ms = walked.as_millis() as u64;
    if rep.deferred > 0 {
        tracing::info!(trigger, measured_ms = rep.measured_ms, deferred = rep.deferred, measured = true, n_considered = rep.considered,
            verdict = "tier_measure_deferred", "tiering: walking budget spent; stale folders wait for the next tick");
    }
    let out = rep.clone();
    if let Err(e) = update(|c| c.last_tick = Some(rep)) {
        tracing::warn!(error = %e, verdict = "tier_state_write_failed", "tiering: could not save tick report");
    }
    if out.moved + out.refused + out.resumed > 0 {
        tracing::info!(trigger, considered = out.considered, moved = out.moved, refused = out.refused, resumed = out.resumed, measured_ms = out.measured_ms, verdict = "tier_tick", "tiering: tick");
    }
    Some(out)
}

pub(crate) struct PolicyCtx {
    pub now: i64,
    pub pressure: bool,
    pub recall_read_days: u32,
    pub residency_s: i64,
}

/// Record reads seen on a cold folder since its last measurement. Returns
/// "read" when a read day was recorded, "scan" when the pass touched so much of
/// the folder at once that it was a sweep, not a use, and None for no reads.
pub(crate) fn observe_reads(f: &mut Folder, st: &Stats, now: i64) -> Option<&'static str> {
    if st.touched == 0 {
        return None;
    }
    if st.files > 0 && st.touched as f64 / st.files as f64 >= SCAN_SHARE {
        return Some("scan");
    }
    let day = now.div_euclid(86_400);
    if !f.read_days.contains(&day) {
        f.read_days.push(day);
    }
    f.read_days.retain(|d| *d > day - 30);
    Some("read")
}

/// What the policy wants for a folder with no op, or nothing.
pub(crate) fn policy(f: &Folder, ctx: &PolicyCtx) -> Option<Kind> {
    let st = f.stats.as_ref()?;
    match location(f) {
        "mac" => {
            if !st.complete {
                return None;
            }
            if f.promoted_at.map(|t| ctx.now - t < ctx.residency_s).unwrap_or(false) {
                return None;
            }
            let full = i64::from(f.idle_days) * 86_400;
            let limit = if ctx.pressure { (full / 4).max(86_400) } else { full };
            (ctx.now - st.last_used >= limit).then_some(Kind::ToHdd)
        }
        "hdd" if f.promote_on_use => {
            let base = f.baseline_used?;
            if st.newest_write > base + USE_GRACE_S {
                return Some(Kind::ToMac);
            }
            let today = ctx.now.div_euclid(86_400);
            let recent = f.read_days.iter().filter(|d| **d > today - READ_WINDOW_DAYS && **d * 86_400 + 86_399 > base).count();
            (recent as u32 >= ctx.recall_read_days.max(1)).then_some(Kind::ToMac)
        }
        _ => None,
    }
}

async fn run_and_record(path: &str, op: Op, rep: &mut TickReport) {
    let p = path.to_string();
    let job = op.clone();
    let res = tokio::task::spawn_blocking(move || {
        let save_phase = |ph: Phase| {
            update_folder(&p, |x| {
                if let Some(o) = x.op.as_mut() {
                    o.phase = ph;
                }
            })
        };
        run_op(&p, &job, &save_phase).map(|_| {
            let st = measure(Path::new(&p), MEASURE_BUDGET);
            (st, job)
        })
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);
    match res {
        Ok((st, job)) => {
            rep.moved += 1;
            let where_ = if job.kind == Kind::ToHdd { "the cold drive" } else { "this Mac" };
            let text = format!("moved to {where_} ({}{})", gb(st.bytes), if job.manual { ", on request" } else { ", by policy" });
            rep.notes.push(format!("{path}: {text}"));
            tracing::info!(path, kind = ?job.kind, bytes = st.bytes, manual = job.manual, by = %job.requested_by, verdict = if job.kind == Kind::ToHdd { "tier_demoted" } else { "tier_promoted" }, "tiering: moved");
            let n = note(if job.kind == Kind::ToHdd { "to_hdd" } else { "to_mac" }, text);
            update_folder(path, |x| {
                x.op = None;
                x.last_error = None;
                x.last_move = Some(n);
                x.read_days.clear();
                if job.kind == Kind::ToHdd {
                    x.dest = Some(job.dest.clone());
                    x.demoted_at = Some(now_secs());
                    x.baseline_used = Some(now_secs());
                } else {
                    x.dest = None;
                    x.demoted_at = None;
                    x.baseline_used = None;
                    x.promoted_at = Some(now_secs());
                }
                x.stats = Some(st);
            });
        }
        Err(e) => {
            rep.notes.push(format!("{path}: {e}"));
            tracing::warn!(path, kind = ?op.kind, error = %e, verdict = "tier_move_failed", "tiering: move step failed; it resumes next tick");
            let n = note("move_failed", e);
            update_folder(path, |x| {
                x.last_error = Some(n.clone());
                if let Some(o) = x.op.as_mut() {
                    o.attempts += 1;
                    // A copy that never succeeds is abandoned; nothing on the
                    // source side has changed in that phase. Swap and cleanup
                    // must converge, so they keep retrying.
                    if o.phase == Phase::Copy && o.attempts >= MAX_COPY_ATTEMPTS {
                        x.op = None;
                        x.last_refusal = Some(note("copy_abandoned", format!("gave up after {MAX_COPY_ATTEMPTS} attempts: {}", n.text)));
                    }
                }
            });
        }
    }
}

// ── HTTP ────────────────────────────────────────────────────────────────────

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(status))
        .route("/config", put(set_config))
        .route("/folders", post(add_folder).delete(remove_folder))
        .route("/move", post(request_move))
        .route("/run", post(run_now))
}

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({"error": msg.into()}))).into_response()
}

fn who(h: &HeaderMap) -> String {
    h.get("X-Amux-Session").and_then(|v| v.to_str().ok()).unwrap_or("dashboard").to_string()
}

/// Normalise what a person typed: `~/x` expands, trailing slashes go, and the
/// result must be absolute with no `..`.
fn clean_path(raw: &str) -> Result<PathBuf, String> {
    let t = raw.trim();
    let s = if let Some(rest) = t.strip_prefix("~/") {
        home_dir().join(rest).to_string_lossy().to_string()
    } else {
        t.to_string()
    };
    let s = s.trim_end_matches('/');
    let p = PathBuf::from(if s.is_empty() { "/" } else { s });
    if !p.is_absolute() {
        return Err("give an absolute path, like ~/Dev/mixpeek/customers/primis".into());
    }
    if p.components().any(|c| c.as_os_str() == "..") {
        return Err("the path may not contain ..".into());
    }
    if p == Path::new("/") {
        return Err("not the whole disk".into());
    }
    Ok(p)
}

fn volumes() -> Vec<Value> {
    let data_dev = std::fs::metadata("/System/Volumes/Data").or_else(|_| std::fs::metadata("/")).map(|m| m.dev()).ok();
    let Ok(rd) = std::fs::read_dir("/Volumes") else { return Vec::new() };
    let mut out: Vec<Value> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let m = std::fs::metadata(&p).ok()?;
            if !m.is_dir() || Some(m.dev()) == data_dev || std::fs::metadata("/").map(|r| r.dev() == m.dev()).unwrap_or(false) {
                return None;
            }
            let (free, total) = df_bytes(&p)?;
            Some(json!({"path": p.to_string_lossy(), "free": free, "total": total}))
        })
        .collect();
    out.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    out
}

async fn status() -> Response {
    let cfg = match load() {
        Ok(c) => c,
        Err(e) => return Json(json!({"measured": false, "why_unmeasured": e, "n_considered": 0})).into_response(),
    };
    let cold = PathBuf::from(&cfg.cold_root);
    let home_dev = std::fs::metadata(home_dir()).map(|m| m.dev()).ok();
    let cold_status = if cfg.cold_root.is_empty() {
        json!(null)
    } else {
        // Available only when its own volume is mounted. Space is then read
        // from the nearest existing folder, which stays on that volume; an
        // unplugged drive reports nothing rather than the boot disk's numbers.
        let available = match volume_of(&cold) {
            Some(vol) => is_mount(&vol),
            None => cold.is_dir() || ensure_parent_mounted(&cold),
        };
        let probe = nearest_existing(&cold);
        let (free, total) = if available { df_bytes(&probe).unwrap_or((0, 0)) } else { (0, 0) };
        let dev = std::fs::metadata(&probe).ok().map(|m| m.dev());
        json!({
            "path": cfg.cold_root,
            "available": available,
            "free": free, "total": total,
            "same_disk_as_mac": available && dev.is_some() && dev == home_dev,
        })
    };
    let folders: Vec<Value> = cfg
        .folders
        .iter()
        .map(|f| {
            let mut v = serde_json::to_value(f).unwrap_or(json!({}));
            v["location"] = json!(if f.op.is_some() { "moving" } else { location(f) });
            v["at_rest"] = json!(location(f));
            v
        })
        .collect();
    Json(json!({
        "measured": true,
        "n_considered": folders.len(),
        "cold_root": cold_status,
        "auto": cfg.auto,
        "interval_s": TICK_SECS,
        "ticking": TICKING.load(Ordering::SeqCst),
        "volumes": volumes(),
        "policy": {
            "recall_read_days": cfg.recall_read_days,
            "pressure_free_pct": cfg.pressure_free_pct,
            "target_free_pct": cfg.target_free_pct,
            "residency_hours": cfg.residency_hours,
            "read_window_days": READ_WINDOW_DAYS,
        },
        "mac_disk": df_bytes(&home_dir()).map(|(free, total)| json!({"free": free, "total": total})),
        "folders": folders,
        "last_tick": cfg.last_tick,
    }))
    .into_response()
}

/// True when `p` sits under a mounted /Volumes/<name> even if `p` itself does
/// not exist yet, so a fresh choice of cold folder reads as available.
fn ensure_parent_mounted(p: &Path) -> bool {
    match volume_of(p) {
        Some(vol) => is_mount(&vol),
        None => p.parent().map(|x| x.is_dir()).unwrap_or(false),
    }
}

#[derive(Deserialize)]
struct ConfigReq {
    #[serde(default)]
    cold_root: Option<String>,
    #[serde(default)]
    auto: Option<bool>,
    #[serde(default)]
    recall_read_days: Option<u32>,
    #[serde(default)]
    pressure_free_pct: Option<u32>,
    #[serde(default)]
    target_free_pct: Option<u32>,
    #[serde(default)]
    residency_hours: Option<u32>,
}

async fn set_config(headers: HeaderMap, Json(body): Json<ConfigReq>) -> Response {
    let cold = match body.cold_root.as_deref() {
        Some(raw) => match clean_path(raw) {
            Ok(p) if !ensure_parent_mounted(&p) && !p.is_dir() => {
                return err(StatusCode::UNPROCESSABLE_ENTITY, format!("{} is not on a connected drive", p.display()))
            }
            Ok(p) => {
                // Create it now, so it exists before the first move needs it.
                if let Err(e) = ensure_cold_root(&p) {
                    return err(StatusCode::UNPROCESSABLE_ENTITY, e);
                }
                Some(p.to_string_lossy().to_string())
            }
            Err(e) => return err(StatusCode::UNPROCESSABLE_ENTITY, e),
        },
        None => None,
    };
    if let Some(n) = body.recall_read_days {
        if !(1..=7).contains(&n) {
            return err(StatusCode::UNPROCESSABLE_ENTITY, "reads must be seen on 1 to 7 days");
        }
    }
    for (name, v) in [("low-space level", body.pressure_free_pct), ("target free space", body.target_free_pct)] {
        if let Some(n) = v {
            if !(1..=90).contains(&n) {
                return err(StatusCode::UNPROCESSABLE_ENTITY, format!("the {name} must be between 1 and 90 percent"));
            }
        }
    }
    if let Some(h) = body.residency_hours {
        if h > 24 * 90 {
            return err(StatusCode::UNPROCESSABLE_ENTITY, "the minimum stay must be at most 90 days");
        }
    }
    let by = who(&headers);
    let current = match load() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let low = body.pressure_free_pct.unwrap_or(current.pressure_free_pct);
    let target = body.target_free_pct.unwrap_or(current.target_free_pct);
    if target <= low {
        return err(StatusCode::UNPROCESSABLE_ENTITY, format!("the target free space ({target}%) must be above the low-space level ({low}%)"));
    }
    match update(|c| {
        if let Some(p) = cold.clone() {
            c.cold_root = p;
        }
        if let Some(a) = body.auto {
            c.auto = a;
        }
        if let Some(n) = body.recall_read_days {
            c.recall_read_days = n;
        }
        c.pressure_free_pct = low;
        c.target_free_pct = target;
        if let Some(h) = body.residency_hours {
            c.residency_hours = h;
        }
    }) {
        Ok(()) => {
            tracing::info!(by = %by, cold_root = ?cold, auto = ?body.auto, verdict = "tier_config_set", "tiering: config changed");
            status().await
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize)]
struct AddReq {
    path: String,
    #[serde(default)]
    idle_days: Option<u32>,
    #[serde(default)]
    promote_on_use: Option<bool>,
    #[serde(default)]
    each_subfolder: Option<bool>,
}

async fn add_folder(headers: HeaderMap, Json(body): Json<AddReq>) -> Response {
    let p = match clean_path(&body.path) {
        Ok(p) => p,
        Err(e) => return err(StatusCode::UNPROCESSABLE_ENTITY, e),
    };
    let days = body.idle_days.unwrap_or_else(default_days);
    if !(1..=3650).contains(&days) {
        return err(StatusCode::UNPROCESSABLE_ENTITY, "idle days must be between 1 and 3650");
    }
    let meta = match std::fs::symlink_metadata(&p) {
        Ok(m) => m,
        Err(_) => return err(StatusCode::UNPROCESSABLE_ENTITY, format!("{} does not exist", p.display())),
    };
    // Adopt a folder already moved by hand: a symlink to a folder on a drive.
    let adopted = if meta.file_type().is_symlink() {
        match std::fs::read_link(&p) {
            Ok(t) if t.is_absolute() && t.is_dir() && t.starts_with("/Volumes") => Some(t.to_string_lossy().to_string()),
            _ => return err(StatusCode::UNPROCESSABLE_ENTITY, format!("{} is a symlink amux did not create", p.display())),
        }
    } else if meta.is_dir() {
        None
    } else {
        return err(StatusCode::UNPROCESSABLE_ENTITY, format!("{} is a file; watch the folder that holds it", p.display()));
    };
    if adopted.is_none() {
        if let Err(why) = guard_path(&p) {
            return err(StatusCode::UNPROCESSABLE_ENTITY, format!("amux never moves this: {why}"));
        }
    }
    let ps = p.to_string_lossy().to_string();
    let by = who(&headers);
    let r = update(|c| {
        if !c.cold_root.is_empty() && p.starts_with(&c.cold_root) {
            return Err("that folder is on the cold drive itself".to_string());
        }
        for x in &c.folders {
            let xp = Path::new(&x.path);
            if xp == p {
                return Err("already watched".to_string());
            }
            if p.starts_with(xp) || xp.starts_with(&p) {
                return Err(format!("overlaps the watched folder {}", x.path));
            }
        }
        c.folders.push(Folder {
            path: ps.clone(),
            idle_days: days,
            promote_on_use: body.promote_on_use.unwrap_or(true),
            added_at: now_secs(),
            dest: adopted.clone(),
            demoted_at: adopted.as_ref().map(|_| now_secs()),
            baseline_used: adopted.as_ref().map(|_| now_secs()),
            op: None,
            stats: None,
            last_refusal: None,
            last_move: None,
            last_error: None,
            read_days: Vec::new(),
            promoted_at: None,
            each_subfolder: body.each_subfolder.unwrap_or(false) && adopted.is_none(),
            parent: None,
        });
        Ok(())
    });
    match r {
        Ok(Ok(())) => {
            tracing::info!(path = %ps, by = %by, idle_days = days, adopted = adopted.is_some(), verdict = "tier_folder_added", "tiering: folder watched");
            kick("added");
            status().await
        }
        Ok(Err(e)) => err(StatusCode::CONFLICT, e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize)]
struct PathQuery {
    path: String,
}

async fn remove_folder(headers: HeaderMap, Query(q): Query<PathQuery>) -> Response {
    let by = who(&headers);
    let r = update(|c| {
        let Some(i) = c.folders.iter().position(|x| x.path == q.path) else {
            return Err((StatusCode::NOT_FOUND, "not watched".to_string()));
        };
        if let Some(root) = &c.folders[i].parent {
            return Err((StatusCode::CONFLICT, format!("it is watched as part of {root}; stop watching that folder instead")));
        }
        let mine = |x: &Folder| x.path == q.path || x.parent.as_deref() == Some(q.path.as_str());
        if c.folders.iter().filter(|x| mine(x)).any(|x| x.op.as_ref().map(|o| o.phase != Phase::Queued).unwrap_or(false)) {
            return Err((StatusCode::CONFLICT, "it is being moved; wait for the move to finish".to_string()));
        }
        let f = c.folders.remove(i);
        c.folders.retain(|x| x.parent.as_deref() != Some(q.path.as_str()));
        Ok(f)
    });
    match r {
        Ok(Ok(f)) => {
            tracing::info!(path = %q.path, by = %by, location = location(&f), verdict = "tier_folder_removed", "tiering: folder no longer watched");
            status().await
        }
        Ok(Err((code, e))) => err(code, e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize)]
struct MoveReq {
    path: String,
    /// "hdd" or "mac".
    to: String,
}

async fn request_move(headers: HeaderMap, Json(body): Json<MoveReq>) -> Response {
    let kind = match body.to.as_str() {
        "hdd" => Kind::ToHdd,
        "mac" => Kind::ToMac,
        _ => return err(StatusCode::UNPROCESSABLE_ENTITY, "to must be \"hdd\" or \"mac\""),
    };
    let by = who(&headers);
    let r = update(|c| {
        let Some(f) = c.folders.iter_mut().find(|x| x.path == body.path) else {
            return Err((StatusCode::NOT_FOUND, "not watched".to_string()));
        };
        if f.each_subfolder {
            return Err((StatusCode::CONFLICT, "each subfolder of it moves on its own; move a subfolder instead".to_string()));
        }
        if f.op.is_some() {
            return Err((StatusCode::CONFLICT, "a move is already queued or running".to_string()));
        }
        let here = location(f);
        match (kind, here) {
            (Kind::ToHdd, "mac") | (Kind::ToMac, "hdd") => {}
            (Kind::ToHdd, "hdd") => return Err((StatusCode::CONFLICT, "already on the cold drive".to_string())),
            (Kind::ToMac, "mac") => return Err((StatusCode::CONFLICT, "already on this Mac".to_string())),
            (_, other) => return Err((StatusCode::CONFLICT, format!("cannot move it from where it is ({other})"))),
        }
        f.op = Some(Op { kind, phase: Phase::Queued, dest: String::new(), manual: true, requested_by: by.clone(), started_at: now_secs(), attempts: 0 });
        Ok(())
    });
    match r {
        Ok(Ok(())) => {
            tracing::info!(path = %body.path, to = %body.to, by = %by, verdict = "tier_move_requested", "tiering: move requested");
            let started = kick("request");
            (StatusCode::ACCEPTED, Json(json!({"queued": true, "started": started}))).into_response()
        }
        Ok(Err((code, e))) => err(code, e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn run_now() -> Response {
    let started = kick("manual");
    (StatusCode::ACCEPTED, Json(json!({"started": started, "already_running": !started}))).into_response()
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_interval_tick_stops_measuring_once_its_walking_budget_is_spent() {
        let under = super::MEASURE_TICK_BUDGET - Duration::from_millis(1);
        let spent = super::MEASURE_TICK_BUDGET;
        assert!(super::measure_this_tick(false, false, under));
        assert!(!super::measure_this_tick(false, false, spent), "an interval tick kept walking past its budget");
        // A person asking, or a move under way, is measured whatever was spent.
        assert!(super::measure_this_tick(true, false, spent));
        assert!(super::measure_this_tick(false, true, spent));
    }

    use super::*;
    use std::fs;

    fn tree(root: &Path) {
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/data.bin"), vec![7u8; 4096]).unwrap();
        fs::write(root.join("top.txt"), "hello").unwrap();
        fs::hard_link(root.join("top.txt"), root.join("a/hard.txt")).unwrap();
    }

    fn op(kind: Kind, phase: Phase, dest: &Path) -> Op {
        Op { kind, phase, dest: dest.to_string_lossy().to_string(), manual: true, requested_by: "test".into(), started_at: 0, attempts: 0 }
    }

    fn folder(src: &Path, dest: Option<&Path>) -> Folder {
        Folder {
            path: src.to_string_lossy().to_string(),
            idle_days: 14,
            promote_on_use: true,
            added_at: 0,
            dest: dest.map(|d| d.to_string_lossy().to_string()),
            demoted_at: None,
            baseline_used: None,
            op: None,
            stats: None,
            last_refusal: None,
            last_move: None,
            last_error: None,
            read_days: Vec::new(),
            promoted_at: None,
            each_subfolder: false,
            parent: None,
        }
    }

    fn ctx(now: i64) -> PolicyCtx {
        PolicyCtx { now, pressure: false, recall_read_days: 3, residency_s: 24 * 3600 }
    }

    fn stats(now: i64, last_used: i64, newest_write: i64) -> Stats {
        Stats { bytes: 1, files: 10, last_used, measured_at: now, complete: true, newest_write, touched: 0 }
    }

    #[test]
    fn to_hdd_then_back_round_trips_contents_and_hard_links() {
        let mac = tempfile::tempdir().unwrap();
        let cold = tempfile::tempdir().unwrap();
        let src = mac.path().join("proj/data");
        tree(&src);
        let dest = dest_for(cold.path(), &src);
        let phases = std::cell::RefCell::new(Vec::new());
        run_op(src.to_str().unwrap(), &op(Kind::ToHdd, Phase::Queued, &dest), &|p| phases.borrow_mut().push(p)).unwrap();
        assert_eq!(*phases.borrow(), vec![Phase::Copy, Phase::Swap, Phase::Cleanup]);
        assert!(fs::symlink_metadata(&src).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_link(&src).unwrap(), dest);
        assert_eq!(fs::read_to_string(src.join("top.txt")).unwrap(), "hello");
        assert_eq!(fs::metadata(dest.join("a/hard.txt")).unwrap().nlink(), 2, "hard links survive the copy");
        assert!(!sibling(&src, OLD).exists());
        assert_eq!(location(&folder(&src, Some(&dest))), "hdd");

        run_op(src.to_str().unwrap(), &op(Kind::ToMac, Phase::Queued, &dest), &|_| {}).unwrap();
        let m = fs::symlink_metadata(&src).unwrap();
        assert!(m.is_dir() && !m.file_type().is_symlink());
        assert_eq!(fs::read(src.join("a/b/data.bin")).unwrap(), vec![7u8; 4096]);
        assert!(!dest.exists(), "the cold copy is removed after it comes back");
        assert!(!sibling(&src, LINK).exists() && !sibling(&src, NEW).exists());
        assert_eq!(location(&folder(&src, None)), "mac");
    }

    #[test]
    fn to_hdd_resumes_after_a_crash_between_rename_and_symlink() {
        let mac = tempfile::tempdir().unwrap();
        let cold = tempfile::tempdir().unwrap();
        let src = mac.path().join("d");
        tree(&src);
        let dest = dest_for(cold.path(), &src);
        copy_verified(&src, &dest).unwrap();
        // The crash: source renamed aside, no link yet, journal still says Swap.
        fs::rename(&src, sibling(&src, OLD)).unwrap();
        run_op(src.to_str().unwrap(), &op(Kind::ToHdd, Phase::Swap, &dest), &|_| {}).unwrap();
        assert_eq!(fs::read_link(&src).unwrap(), dest);
        assert!(!sibling(&src, OLD).exists());
        assert_eq!(fs::read_to_string(src.join("top.txt")).unwrap(), "hello");
    }

    #[test]
    fn to_hdd_final_merge_keeps_writes_made_through_the_link_mid_swap() {
        let mac = tempfile::tempdir().unwrap();
        let cold = tempfile::tempdir().unwrap();
        let src = mac.path().join("d");
        tree(&src);
        let dest = dest_for(cold.path(), &src);
        copy_verified(&src, &dest).unwrap();
        // The crash: renamed aside and linked, final merge not yet run. Meanwhile
        // a writer used the folder through the link: a new file, and a newer
        // version of an existing one.
        let old = sibling(&src, OLD);
        fs::rename(&src, &old).unwrap();
        symlink(&dest, &src).unwrap();
        fs::write(src.join("new.txt"), "written after the move").unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        fs::write(src.join("top.txt"), "edited after the move").unwrap();
        run_op(src.to_str().unwrap(), &op(Kind::ToHdd, Phase::Swap, &dest), &|_| {}).unwrap();
        assert_eq!(fs::read_to_string(dest.join("new.txt")).unwrap(), "written after the move");
        assert_eq!(fs::read_to_string(dest.join("top.txt")).unwrap(), "edited after the move");
        assert!(!old.exists());
        // And a replay after completion changes nothing.
        run_op(src.to_str().unwrap(), &op(Kind::ToHdd, Phase::Swap, &dest), &|_| {}).unwrap();
        assert_eq!(fs::read_to_string(dest.join("top.txt")).unwrap(), "edited after the move");
    }

    #[test]
    fn to_mac_resumes_after_a_crash_between_the_two_renames() {
        let mac = tempfile::tempdir().unwrap();
        let cold = tempfile::tempdir().unwrap();
        let src = mac.path().join("d");
        tree(&src);
        let dest = dest_for(cold.path(), &src);
        run_op(src.to_str().unwrap(), &op(Kind::ToHdd, Phase::Queued, &dest), &|_| {}).unwrap();
        let new = sibling(&src, NEW);
        copy_verified(&dest, &new).unwrap();
        // The crash: link renamed aside, new copy not yet in place.
        fs::rename(&src, sibling(&src, LINK)).unwrap();
        run_op(src.to_str().unwrap(), &op(Kind::ToMac, Phase::Swap, &dest), &|_| {}).unwrap();
        assert!(!fs::symlink_metadata(&src).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(src.join("top.txt")).unwrap(), "hello");
        assert!(!dest.exists() && !sibling(&src, LINK).exists());
    }

    #[test]
    fn a_missing_source_with_no_kept_copy_is_an_error_not_a_success() {
        let mac = tempfile::tempdir().unwrap();
        let cold = tempfile::tempdir().unwrap();
        let src = mac.path().join("never-existed");
        let dest = dest_for(cold.path(), &src);
        let e = run_op(src.to_str().unwrap(), &op(Kind::ToHdd, Phase::Queued, &dest), &|_| {}).unwrap_err();
        assert!(e.contains("not a folder"), "{e}");
    }

    #[test]
    fn recency_moves_an_idle_folder_out_and_a_partial_count_never_does() {
        let mac = tempfile::tempdir().unwrap();
        let src = mac.path().join("d");
        tree(&src);
        let now = now_secs();
        let mut f = folder(&src, None);
        f.stats = Some(stats(now, now - 20 * 86_400, now - 20 * 86_400));
        assert_eq!(policy(&f, &ctx(now)), Some(Kind::ToHdd));
        f.stats.as_mut().unwrap().complete = false;
        assert_eq!(policy(&f, &ctx(now)), None, "a partial walk never moves a folder");
        f.stats = Some(stats(now, now - 86_400, now - 86_400));
        assert_eq!(policy(&f, &ctx(now)), None, "used yesterday, idle limit 14 days");
    }

    #[test]
    fn low_disk_moves_cold_folders_out_sooner_but_not_fresh_ones() {
        let mac = tempfile::tempdir().unwrap();
        let src = mac.path().join("d");
        tree(&src);
        let now = now_secs();
        let mut f = folder(&src, None);
        f.stats = Some(stats(now, now - 5 * 86_400, now - 5 * 86_400));
        let mut c = ctx(now);
        assert_eq!(policy(&f, &c), None, "5 idle days is under the 14-day limit");
        c.pressure = true;
        assert_eq!(policy(&f, &c), Some(Kind::ToHdd), "under pressure the limit is a quarter: 3.5 days");
        f.stats = Some(stats(now, now - 3600, now - 3600));
        assert_eq!(policy(&f, &c), None, "pressure never takes a folder used within a day");
    }

    #[test]
    fn a_folder_that_just_came_back_stays_for_its_residency() {
        let mac = tempfile::tempdir().unwrap();
        let src = mac.path().join("d");
        tree(&src);
        let now = now_secs();
        let mut f = folder(&src, None);
        f.stats = Some(stats(now, now - 20 * 86_400, now - 20 * 86_400));
        f.promoted_at = Some(now - 3600);
        assert_eq!(policy(&f, &ctx(now)), None, "came back an hour ago");
        f.promoted_at = Some(now - 25 * 3600);
        assert_eq!(policy(&f, &ctx(now)), Some(Kind::ToHdd));
    }

    #[test]
    fn a_write_on_the_cold_drive_recalls_at_once() {
        let mac = tempfile::tempdir().unwrap();
        let cold = tempfile::tempdir().unwrap();
        let src = mac.path().join("d");
        tree(&src);
        let dest = dest_for(cold.path(), &src);
        run_op(src.to_str().unwrap(), &op(Kind::ToHdd, Phase::Queued, &dest), &|_| {}).unwrap();
        let now = now_secs();
        let mut g = folder(&src, Some(&dest));
        g.baseline_used = Some(now - 100);
        g.stats = Some(stats(now, now - 100, now - 100));
        assert_eq!(policy(&g, &ctx(now)), None, "nothing since the move");
        g.stats.as_mut().unwrap().newest_write = now - 100 + USE_GRACE_S + 1;
        assert_eq!(policy(&g, &ctx(now)), Some(Kind::ToMac));
        g.promote_on_use = false;
        assert_eq!(policy(&g, &ctx(now)), None);
    }

    #[test]
    fn reads_recall_only_after_enough_separate_days_and_scans_never_count() {
        let mac = tempfile::tempdir().unwrap();
        let cold = tempfile::tempdir().unwrap();
        let src = mac.path().join("d");
        tree(&src);
        let dest = dest_for(cold.path(), &src);
        run_op(src.to_str().unwrap(), &op(Kind::ToHdd, Phase::Queued, &dest), &|_| {}).unwrap();
        let day = 86_400;
        let t0 = now_secs() - 10 * day;
        let mut g = folder(&src, Some(&dest));
        g.baseline_used = Some(t0);
        let mut st = stats(t0, t0, t0);
        // The nightly backup reads every file, three nights running: a scan each time.
        for n in 1..=3 {
            st.touched = 10;
            assert_eq!(observe_reads(&mut g, &st, t0 + n * day), Some("scan"));
        }
        assert!(g.read_days.is_empty(), "scans record no read days");
        // A person reads two files on one day: one read day, twice over.
        st.touched = 2;
        assert_eq!(observe_reads(&mut g, &st, t0 + 4 * day), Some("read"));
        assert_eq!(observe_reads(&mut g, &st, t0 + 4 * day + 60), Some("read"));
        g.stats = Some(st.clone());
        assert_eq!(policy(&g, &ctx(t0 + 4 * day)), None, "one day of reads is not a habit");
        observe_reads(&mut g, &st, t0 + 5 * day);
        assert_eq!(policy(&g, &ctx(t0 + 5 * day)), None, "two days, the threshold is three");
        observe_reads(&mut g, &st, t0 + 6 * day);
        assert_eq!(policy(&g, &ctx(t0 + 6 * day)), Some(Kind::ToMac), "reads on three days within a week");
        // Old read days age out of the window.
        assert_eq!(policy(&g, &ctx(t0 + 20 * day)), None, "those reads are two weeks old");
        st.touched = 0;
        assert_eq!(observe_reads(&mut g, &st, t0 + 21 * day), None);
    }

    #[test]
    fn a_root_discovers_new_subfolders_and_drops_deleted_ones() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("runs");
        for n in ["r1", "r2", ".hidden", "r3.amux-tier-new"] {
            fs::create_dir_all(root.join(n)).unwrap();
        }
        fs::write(root.join("notes.txt"), "a file, not a unit").unwrap();
        let mut c = Config::default();
        let mut r = folder(&root, None);
        r.each_subfolder = true;
        r.idle_days = 30;
        c.folders.push(r);
        assert_eq!(discover(&mut c), (2, 0));
        let kids: Vec<_> = c.folders.iter().filter(|f| f.parent.is_some()).map(|f| f.path.clone()).collect();
        assert_eq!(kids.len(), 2, "{kids:?}");
        assert!(c.folders.iter().filter(|f| f.parent.is_some()).all(|f| f.idle_days == 30), "children inherit the root's limit");
        assert_eq!(discover(&mut c), (0, 0), "idempotent");
        fs::create_dir(root.join("r4")).unwrap();
        fs::remove_dir(root.join("r1")).unwrap();
        assert_eq!(discover(&mut c), (1, 1));
        assert!(c.folders.iter().any(|f| f.path.ends_with("/r4")));
        assert!(!c.folders.iter().any(|f| f.path.ends_with("/r1")));
    }

    #[test]
    fn a_child_on_the_cold_drive_is_kept_even_when_its_link_is_unreachable() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("runs");
        fs::create_dir_all(root.join("r1")).unwrap();
        let mut c = Config::default();
        let mut r = folder(&root, None);
        r.each_subfolder = true;
        c.folders.push(r);
        discover(&mut c);
        // r1 moved to a drive that is now unplugged: no folder on disk, dest set.
        let k = c.folders.iter_mut().find(|f| f.path.ends_with("/r1")).unwrap();
        k.dest = Some("/Volumes/gone/r1".into());
        fs::remove_dir(root.join("r1")).unwrap();
        assert_eq!(discover(&mut c), (0, 0), "its data lives on the drive; dropping it would orphan it");
    }

    #[test]
    fn measure_counts_a_write_and_ignores_directory_listing() {
        let d = tempfile::tempdir().unwrap();
        tree(d.path());
        let before = measure(d.path(), Duration::from_secs(10));
        assert!(before.complete && before.files == 3);
        let _ = fs::read_dir(d.path().join("a")).unwrap().count();
        let again = measure(d.path(), Duration::from_secs(10));
        assert_eq!(before.last_used, again.last_used, "listing a folder is not a use");
    }

    #[test]
    fn free_space_of_a_cold_folder_not_yet_created_is_read_from_its_drive() {
        let d = tempfile::tempdir().unwrap();
        let missing = d.path().join("not/yet/created");
        assert_eq!(nearest_existing(&missing), d.path());
        assert!(df_bytes(&nearest_existing(&missing)).is_some_and(|(_, total)| total > 0));
        assert_eq!(nearest_existing(d.path()), d.path());
    }

    #[test]
    fn a_leftover_folder_is_not_a_mounted_drive() {
        let d = tempfile::tempdir().unwrap();
        let fake = d.path().join("Bulk");
        fs::create_dir(&fake).unwrap();
        assert!(!is_mount(&fake), "a plain folder sits on its parent's device");
        assert!(is_mount(Path::new("/")));
        assert_eq!(volume_of(Path::new("/Volumes/Bulk/amux-tier/x")), Some(PathBuf::from("/Volumes/Bulk")));
        assert_eq!(volume_of(Path::new("/Users/e/x")), None);
        let e = ensure_cold_root(Path::new("/Volumes/amux-test-no-such-drive/tier")).unwrap_err();
        assert!(e.contains("not connected"), "{e}");
        assert!(!Path::new("/Volumes/amux-test-no-such-drive").exists(), "nothing was created");
    }

    #[test]
    fn clean_path_rejects_relative_dotdot_and_root() {
        assert!(clean_path("relative/x").is_err());
        assert!(clean_path("/a/../b").is_err());
        assert!(clean_path("/").is_err());
        assert_eq!(clean_path(" /a/b/ ").unwrap(), PathBuf::from("/a/b"));
    }

    #[test]
    fn location_reads_the_filesystem() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        fs::create_dir(&real).unwrap();
        assert_eq!(location(&folder(&real, None)), "mac");
        assert_eq!(location(&folder(&d.path().join("gone"), None)), "missing");
        let link = d.path().join("link");
        symlink(&real, &link).unwrap();
        assert_eq!(location(&folder(&link, None)), "foreign_link");
        assert_eq!(location(&folder(&link, Some(&real))), "hdd");
        fs::remove_dir(&real).unwrap();
        assert_eq!(location(&folder(&link, Some(&real))), "hdd_unavailable");
    }
}
