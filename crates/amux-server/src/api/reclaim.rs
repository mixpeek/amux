//! /api/reclaim — disk scan, reclaim findings, treemap, and a quarantine-first
//! cleanup path.
//!
//! Three properties this module exists to get right, each because the obvious
//! implementation gets it wrong:
//!
//! 1. **Reclaim is measured as a `df` delta, never as a sum of file sizes.**
//!    On APFS those disagree by the whole answer. Deleting 22GB of ollama blobs
//!    on 2026-08-16 moved `du` by 22GB and moved `df` by roughly zero, because
//!    24 hourly local Time Machine snapshots still referenced the freed blocks.
//!    A cleanup UI that reports the SUM tells the user it reclaimed space they
//!    cannot see on their own disk — a confident wrong answer, which is worse
//!    than no answer (ethos rule 7). So every batch records free-space before
//!    and after, and `snapshot_held` is surfaced as its own finding.
//!
//! 2. **The scan must not become part of the problem.** It runs on a dedicated
//!    thread with macOS background I/O policy (`setiopolicy_np`), which makes
//!    the kernel deprioritize its reads behind everything else, and yields
//!    periodically. A `du -sh` over this home directory took >3 minutes of
//!    wall-clock during development while load average sat at 95; an
//!    unthrottled walker would have made the machine it is diagnosing slower.
//!
//! 3. **Nothing is deleted on the scanner's judgment.** Quarantine moves a path
//!    aside, keeping it restorable, and purge is a separate explicit act. Every
//!    candidate path must additionally pass `guard_path`, which is a deny-list
//!    of things no heuristic is allowed to touch regardless of how much space
//!    they would free (ethos rule 8 — never bulk-delete user content).

use super::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/scan", axum::routing::post(start_scan).get(list_scans))
        .route("/scan/{id}", axum::routing::get(get_scan))
        .route("/scan/{id}/cancel", axum::routing::post(cancel_scan))
        .route("/tree/{id}", axum::routing::get(get_tree))
        .route(
            "/quarantine",
            axum::routing::get(list_quarantine).post(create_quarantine),
        )
        .route(
            "/quarantine/{id}/restore",
            axum::routing::post(restore_quarantine),
        )
        .route("/quarantine/{id}", axum::routing::delete(purge_quarantine))
        .route("/snapshots", axum::routing::get(list_snapshots))
        .route(
            "/skipped",
            axum::routing::get(list_skipped).delete(unskip),
        )
}

/// Mark scans left `running` by a previous process as interrupted.
///
/// A scan lives on a plain thread, so it dies with the process. On this repo
/// the builder restarts the server whenever ANYONE commits to the shared
/// checkout, so a scan being killed mid-walk is the common case, not an edge
/// one — during development three consecutive scans were orphaned this way by
/// the author's own deploys.
///
/// Without this, the row keeps claiming `running` forever: the dashboard spins
/// on a scan that has no thread, and the "one scan at a time" guard refuses
/// every future scan with 409. Silence would read as work in progress, which
/// is the failure ethos rule 4 is about — so the row is reaped AND says why.
pub(crate) fn reap_orphaned_scans(store: &crate::db::SharedStore) {
    let res = store.write(|c| {
        let n = c.execute(
            // current_path and current_phase are deliberately PRESERVED. The
            // first version of this cleared them, which destroyed the only
            // record of where a dead scan had been standing — so a scan that
            // wedged at 1,087 directories came back saying it had been
            // restarted and refusing to say where, and the reaper meant to make
            // the failure legible was the thing erasing it.
            "UPDATE reclaim_scans
                SET status='interrupted', finished_at=?1,
                    error='server restarted mid-scan; the scan thread did not survive'
                          || COALESCE(' (last position: ' || current_phase || ' at '
                                      || current_path || ')', '')
              WHERE status='running'",
            rusqlite::params![now_secs()],
        )?;
        Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
    });
    match res {
        Ok(r) if r.applied => {
            tracing::warn!("reaped reclaim scan(s) orphaned by a server restart")
        }
        Ok(_) => {}
        Err(e) => tracing::error!(error = %e, "failed to reap orphaned reclaim scans"),
    }
}

// ── cancellation registry ────────────────────────────────────────────────────

fn cancel_registry() -> &'static Mutex<HashMap<String, Arc<AtomicBool>>> {
    static R: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

// ── where the walk thread is, and what it is doing there ─────────────────────

/// Quiet for this long is worth a log line. Nothing more.
///
/// The first version treated 45s as proof of a block and ended the scan on it.
/// Its first production run stalled on `~/Downloads` and permanently exempted
/// it — a directory that answers `readdir` in 2 seconds with 318 entries, and
/// one of the places a cleanup scan most needs to look. The threshold was below
/// the baseline: this machine runs ~50 agent sessions at load 95, and the scan
/// is itself competing for the disk it is measuring, so quiet for a minute is
/// contention, not a hang.
const STALL_WARN_SECS: u64 = 45;

/// Quiet for THIS long is a block, and the scan ends rather than spinning.
///
/// The discriminator is duration, because it is what actually separates the two
/// cases: a wedged FileProvider root returns nothing ever (`~/Library/Mobile
/// Documents` gave zero entries in 90s and was still blocked), while a merely
/// slow directory returns. Five minutes of zero movement on ONE directory is
/// not a load spike.
const STALL_ABORT_SECS: u64 = 300;

/// A directory is only routed around after it has hung this many separate
/// scans. One observation is an anecdote, and the cost of believing it is a
/// permanent hole in the scan that nothing announces.
const STALL_HITS_TO_SKIP: i64 = 2;

/// The walk thread's position, published once per directory.
///
/// `dirs_walked` freezing tells you a scan is stuck and NOTHING else. A
/// `read_dir` blocked in the kernel and a SQLite flush blocked on the write
/// lock produce a byte-identical stall from outside the process: same frozen
/// counter, same `running` status, same silence. On 2026-08-20 a scan wedged at
/// 1,087 directories and the row could name neither the path nor the phase, so
/// the two hypotheses could only be separated by re-walking the tree by hand.
///
/// Two things make this able to answer. The position is published BEFORE each
/// syscall that can block, so it names the directory the thread is sitting in
/// rather than the last one it finished. And it is published separately from
/// the throttled DB write that persists it, so a flush that never returns does
/// not also erase the record of where the walk was.
pub(crate) struct ScanPos {
    inner: Mutex<PosInner>,
}

struct PosInner {
    path: String,
    phase: &'static str,
    since: std::time::Instant,
}

impl ScanPos {
    pub(crate) fn new() -> Self {
        ScanPos {
            inner: Mutex::new(PosInner {
                path: String::new(),
                phase: "starting",
                since: std::time::Instant::now(),
            }),
        }
    }

    /// Enter `phase` at `path`. Resets the stall clock: this is progress.
    fn set(&self, path: &FsPath, phase: &'static str) {
        if let Ok(mut p) = self.inner.lock() {
            p.path = path.to_string_lossy().into_owned();
            p.phase = phase;
            p.since = std::time::Instant::now();
        }
    }

    /// Change phase without moving. Also progress — a flush that starts is a
    /// different thing to hang in than the readdir that preceded it.
    fn phase(&self, phase: &'static str) {
        if let Ok(mut p) = self.inner.lock() {
            p.phase = phase;
            p.since = std::time::Instant::now();
        }
    }

    /// (path, phase, seconds since that position was last advanced)
    fn read(&self) -> (String, &'static str, f64) {
        match self.inner.lock() {
            Ok(p) => (p.path.clone(), p.phase, p.since.elapsed().as_secs_f64()),
            // A poisoned mutex means the walk thread panicked holding it. Say
            // so rather than returning a position that looks live.
            Err(_) => (String::new(), "poisoned", f64::MAX),
        }
    }
}

impl Default for ScanPos {
    fn default() -> Self {
        Self::new()
    }
}

// ── directories the scan will not enter ──────────────────────────────────────

/// FileProvider roots, skipped on the merits rather than defensively.
///
/// Their contents are online-only: the bytes live on someone else's disk and
/// occupy no local blocks, so a CLEANUP scan has nothing to reclaim in them,
/// and enumerating one can force downloads — the scan would consume disk
/// instead of freeing it. That `~/Library/Mobile Documents` also happens to
/// hang readdir forever on this machine is a second reason, not the reason.
///
/// Home-relative, because these are per-user paths.
const PROVIDER_ROOTS: &[&str] = &["Library/Mobile Documents", "Library/CloudStorage"];

/// Seed the provider roots, then hand back every path the walk must not enter.
///
/// Skips are rows rather than constants so the scan can REPORT what it did not
/// look at, and so a learned one can be deleted when the filesystem behind it
/// starts answering again. An exemption that leaves no trace is invisible
/// (ethos rule 1) — and invisible is how a "cleanup scan" quietly stops
/// covering a third of a home directory.
fn load_skips(store: &crate::db::SharedStore) -> std::collections::HashSet<PathBuf> {
    let home = home_dir();
    let now = now_secs();
    let seed: Vec<String> = PROVIDER_ROOTS
        .iter()
        .map(|r| home.join(r).to_string_lossy().into_owned())
        .collect();
    let _ = store.write(move |c| {
        for p in &seed {
            c.execute(
                "INSERT OR IGNORE INTO reclaim_skipped
                   (path, reason, detail, first_seen, last_seen, hits)
                 VALUES (?1,'provider',?2,?3,?3,0)",
                rusqlite::params![
                    p,
                    "cloud FileProvider root: online-only content holds no local blocks to reclaim",
                    now
                ],
            )?;
        }
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    });

    let mut out = std::collections::HashSet::new();
    if let Ok(conn) = store.read() {
        // A learned skip needs corroboration. `provider` rows are a standing
        // decision and apply immediately; a `stalled` row is one observation
        // until a second scan hangs on the same path, because a single 5-minute
        // quiet spell on a loaded machine is not proof a directory is broken.
        if let Ok(mut stmt) = conn.prepare(
            "SELECT path FROM reclaim_skipped
              WHERE reason='provider' OR (reason='stalled' AND hits >= ?1)",
        ) {
            if let Ok(rows) = stmt.query_map([STALL_HITS_TO_SKIP], |r| r.get::<_, String>(0)) {
                for p in rows.flatten() {
                    out.insert(PathBuf::from(p));
                }
            }
        }
    }
    out
}

/// Record a directory that hung a scan, so the NEXT scan routes around it.
///
/// Called from the watchdog, which is by definition running while something is
/// blocked — so this write may itself be the thing that cannot complete. It is
/// deliberately attempted only AFTER the WARN has already reached the log.
fn record_stalled_dir(store: &crate::db::SharedStore, path: String, detail: String) {
    let now = now_secs();
    let res = store.write(move |c| {
        let n = c.execute(
            "INSERT INTO reclaim_skipped (path, reason, detail, first_seen, last_seen, hits)
             VALUES (?1,'stalled',?2,?3,?3,1)
             ON CONFLICT(path) DO UPDATE SET
               reason='stalled', detail=excluded.detail,
               last_seen=excluded.last_seen, hits=hits+1",
            rusqlite::params![path, detail, now],
        )?;
        Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
    });
    if let Err(e) = res {
        tracing::error!(error = %e, "failed to record a stalled reclaim directory");
    }
}

// ── volume state ─────────────────────────────────────────────────────────────

pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Free/total bytes on the volume holding `path`, straight from statfs.
///
/// This is the number the user sees in Finder, and the ONLY honest measure of
/// whether a cleanup worked. Deliberately not derived from summing file sizes.
///
/// Shared with `api::metrics` on purpose. Asking about a PATH rather than a
/// mount point is what makes it correct on APFS: `/` is the sealed read-only
/// system snapshot (~11GB), while everything a user owns lives on
/// `/System/Volumes/Data`. `df -k /` therefore answers about the wrong volume,
/// which is how the metrics gauge read 0.6% used on a machine that was 90%
/// full and failing writes.
pub(crate) fn df_bytes(path: &FsPath) -> Option<(u64, u64)> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let bsize = st.f_bsize as u64;
    Some((st.f_bavail * bsize, st.f_blocks * bsize))
}

/// APFS local snapshots. These retain blocks belonging to deleted files, which
/// is why a delete can free `du` space without freeing `df` space.
fn local_snapshots() -> Vec<String> {
    std::process::Command::new("/usr/bin/tmutil")
        .args(["listlocalsnapshots", "/"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| l.contains("com.apple.TimeMachine"))
                .map(|l| l.trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

// ── classification ───────────────────────────────────────────────────────────

/// Directory names that are build output or vendored dependencies: large,
/// regenerable, and safe to reclaim at the cost of a rebuild.
const BUILD_DIRS: &[&str] = &[
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".nuxt",
    ".turbo",
    ".parcel-cache",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".gradle",
    "DerivedData",
    "Pods",
    "elm-stuff",
    ".terraform",
    ".dart_tool",
];

/// Absolute paths (after `~` expansion) that are caches or dev-tool stores.
/// Matched as prefixes, longest first, so `~/Library/Caches` wins over `~`.
fn devtool_roots() -> Vec<(PathBuf, &'static str, &'static str)> {
    let home = home_dir();
    vec![
        (home.join(".ollama/models"), "devtool", "Ollama model blobs"),
        (home.join(".docker"), "devtool", "Docker data"),
        (home.join(".cargo/registry"), "devtool", "Cargo registry cache"),
        (home.join(".rustup/toolchains"), "devtool", "Rust toolchains"),
        (home.join(".npm"), "devtool", "npm cache"),
        (home.join(".cache"), "cache", "Generic tool cache"),
        (home.join("Library/Caches"), "cache", "Application caches"),
        (
            home.join("Library/Developer/Xcode/DerivedData"),
            "cache",
            "Xcode DerivedData",
        ),
        (
            home.join("Library/Developer/CoreSimulator"),
            "cache",
            "iOS simulator runtimes",
        ),
        (
            home.join("Library/Application Support/MobileSync"),
            "cache",
            "iOS device backups",
        ),
        (
            home.join(".amux/rust-build-target"),
            "build",
            "Shared cargo target dir",
        ),
        // The BIGGER sibling, and it was missing from this list for as long as
        // it has existed. Measured 2026-08-19: 4.2GB here against 1.0GB in the
        // shared dir, on a machine that had reached 1GB free — so the tool whose
        // whole job is finding reclaimable space was blind to the largest
        // reclaimable directory amux owns.
        //
        // This is the hand-maintained-list failure: the list agrees with reality
        // until something new appears, and the day it disagrees is the day it is
        // needed. Written by e2e/serve-head.sh (a debug-profile build), so it is
        // fully regenerable and costs one cold e2e run.
        (
            home.join(".amux/rust-build-target-e2e-head"),
            "build",
            "e2e head-build cargo target dir",
        ),
    ]
}

pub(crate) fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/Users/ethan".into()))
}

/// Coarse file-type bucket, used to color the treemap.
fn kind_of(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("gguf" | "safetensors" | "bin" | "pt" | "pth" | "onnx" | "mlmodel") => "model",
        Some("mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" | "wav" | "mp3" | "aiff") => "media",
        Some("png" | "jpg" | "jpeg" | "gif" | "heic" | "webp" | "tiff" | "svg" | "psd") => "image",
        Some("zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "dmg" | "iso" | "pkg" | "rar") => {
            "archive"
        }
        Some("db" | "sqlite" | "sqlite3" | "raw" | "vmdk" | "qcow2") => "data",
        Some("rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "c" | "h" | "cpp" | "java"
        | "rb" | "sh" | "json" | "toml" | "yaml" | "yml" | "md" | "html" | "css") => "code",
        Some("o" | "a" | "so" | "dylib" | "rlib" | "class" | "pyc" | "d" | "rmeta") => "build",
        Some("pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "key" | "pages") => "doc",
        _ => "other",
    }
}

// ── the guard: what may never be quarantined ─────────────────────────────────

/// Returns `Err(reason)` if `p` must not be moved, whatever the scan thinks.
///
/// Deny-list rather than allow-list because the failure modes are asymmetric:
/// wrongly refusing to clean something costs disk space, wrongly moving
/// something costs the user's data or a broken machine. Everything here is a
/// path a size-ranking heuristic would otherwise happily nominate.
fn guard_path(p: &FsPath) -> Result<(), String> {
    let home = home_dir();
    if !p.is_absolute() {
        return Err("path is not absolute".into());
    }
    if p.components().any(|c| c.as_os_str() == "..") {
        return Err("path contains ..".into());
    }
    let s = p.to_string_lossy().to_string();

    // System trees: never.
    for sys in [
        "/System", "/usr", "/bin", "/sbin", "/Library/Apple", "/Applications", "/private/var/db",
        "/dev", "/Volumes",
    ] {
        if s == sys || s.starts_with(&format!("{sys}/")) {
            return Err(format!("system path ({sys})"));
        }
    }
    // Home itself, and the top-level dirs inside it. A cleaner is never
    // right to move ~/Documents wholesale.
    if p == home {
        return Err("home directory itself".into());
    }
    if p.parent() == Some(home.as_path()) {
        let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
        // The few depth-1 dirs that ARE pure caches stay eligible.
        if !matches!(name.as_str(), ".cache" | ".npm") {
            return Err(format!("top-level home directory (~/{name})"));
        }
    }
    // amux's own state. rust-build-target is regenerable and stays eligible;
    // the store, sessions, memory and logs are not.
    for protected in [".amux/amux.db", ".amux/sessions", ".amux/memory", ".amux/logs", ".amux/quarantine"] {
        let full = home.join(protected);
        if p == full || s.starts_with(&format!("{}/", full.to_string_lossy())) {
            return Err(format!("amux state (~/{protected})"));
        }
    }
    if s.contains("/.git/") || s.ends_with("/.git") {
        return Err("git metadata".into());
    }
    Ok(())
}

// ── the walker ───────────────────────────────────────────────────────────────

#[derive(Default)]
struct Acc {
    bytes: u64,
    files: u64,
    newest_mtime: i64,
    kinds: HashMap<&'static str, u64>,
}

/// Bytes this file actually occupies on disk.
///
/// `len()` is the APPARENT size and it is the wrong number for a disk tool.
/// It ignores sparse files (which report a huge logical size while occupying
/// almost nothing) and APFS transparent compression (which occupies less than
/// it reports). The first run of this scanner used `len()` and reported 1.14 TB
/// for a home directory that `du` measures at ~820 GB — a 40% overstatement,
/// which in a cleanup UI reads as "there is way more to reclaim than there is".
/// `st_blocks` is in 512-byte units by definition and is what `du` counts.
fn disk_bytes(md: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.blocks().saturating_mul(512)
}

/// Hardlink/clone de-duplication key. A file with `nlink > 1` is reachable at
/// several paths; counting it once per path inflates every total that contains
/// it. `du` dedupes by inode and so must this, or two directories both "contain"
/// the same gigabytes and the treemap adds up to more than the disk holds.
#[derive(Default)]
struct SeenInodes(std::collections::HashSet<(u64, u64)>);

impl SeenInodes {
    /// True if this file should be counted (first sighting, or not hardlinked).
    fn count(&mut self, md: &std::fs::Metadata) -> bool {
        use std::os::unix::fs::MetadataExt;
        if md.nlink() <= 1 {
            return true;
        }
        self.0.insert((md.dev(), md.ino()))
    }
}

struct ScanCfg {
    roots: Vec<PathBuf>,
    tree_depth: i32,
    large_file_floor: u64,
    stale_age_secs: i64,
    /// Also size the dev-tool stores in `devtool_roots()`, which are REAL
    /// absolute paths in the home directory and are walked regardless of
    /// `roots`.
    ///
    /// It exists because that unconditional behaviour turned every unit test
    /// that calls `walk()` on a three-file temp directory into a full pass over
    /// this machine's caches — `~/.cache`, `~/Library/Caches` and a 15GB shared
    /// cargo target dir — under background I/O priority. Two such tests ran for
    /// fourteen hours at 0% CPU and, because cargo serialises on one build
    /// lock, took every other lane's `cargo test` hostage with them. A test
    /// that silently scans the whole machine is not a unit test.
    include_devtools: bool,
}

/// Ask the kernel to treat this thread's reads as background work.
///
/// Without this the scan competes with the amux server for disk bandwidth on a
/// machine already at load 95 — it would slow down the thing it is measuring,
/// and the measurement would be of a machine the scan made worse.
#[cfg(target_os = "macos")]
fn set_background_io() {
    // IOPOL_TYPE_DISK = 0, IOPOL_SCOPE_THREAD = 1, IOPOL_THROTTLE = 3
    unsafe extern "C" {
        fn setiopolicy_np(iotype: libc::c_int, scope: libc::c_int, policy: libc::c_int)
            -> libc::c_int;
    }
    unsafe {
        setiopolicy_np(0, 1, 3);
    }
}

#[cfg(not(target_os = "macos"))]
fn set_background_io() {}

struct WalkOut {
    /// path -> rolled-up totals, for every dir shallower than `tree_depth`
    tree: HashMap<PathBuf, Acc>,
    findings: Vec<Finding>,
    dirs: u64,
    files: u64,
    bytes: u64,
    /// Directories the skip list kept us out of, so the scan can say what it
    /// did not look at instead of silently under-reporting a home directory.
    skipped: Vec<PathBuf>,
}

struct Finding {
    category: &'static str,
    path: PathBuf,
    bytes: u64,
    file_count: u64,
    mtime: i64,
    regenerable: bool,
    detail: String,
}

/// Walk `cfg.roots`, staying on one device, never following symlinks.
///
/// Returns rolled-up sizes for the treemap plus the reclaim findings. Prunes
/// at build/cache directories: their total is recorded as one finding and the
/// walker does not descend, which is both faster and the right granularity —
/// nobody wants 40,000 individual `node_modules` files listed.
/// Progress + flush callback: (dirs, files, bytes, current path, pending findings).
type ProgressFn<'a> = dyn FnMut(u64, u64, u64, &FsPath, &mut Vec<Finding>) + 'a;

/// `progress` is also the FLUSH point: it drains the findings accumulated so
/// far. Findings are independent per-path facts, complete the moment they are
/// discovered, so there is no reason to hold them until the end — and every
/// reason not to, since a scan killed at minute 19 of 20 would otherwise
/// persist nothing at all. (Tree rollups genuinely cannot be flushed early:
/// an ancestor's total is not known until its last descendant is walked.)
fn walk(
    cfg: &ScanCfg,
    cancel: &AtomicBool,
    pos: &ScanPos,
    skip: &std::collections::HashSet<PathBuf>,
    progress: &mut ProgressFn<'_>,
) -> WalkOut {
    set_background_io();

    let mut out = WalkOut {
        tree: HashMap::new(),
        findings: Vec::new(),
        dirs: 0,
        files: 0,
        bytes: 0,
        skipped: Vec::new(),
    };
    let devtools = if cfg.include_devtools { devtool_roots() } else { vec![] };
    let now = now_secs();
    // size -> paths, for the duplicate fingerprint pass
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    // Shared across roots: a hardlink spanning two roots must still count once.
    let mut seen = SeenInodes::default();

    for root in &cfg.roots {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let root_dev = match std::fs::symlink_metadata(root) {
            Ok(m) => {
                use std::os::unix::fs::MetadataExt;
                m.dev()
            }
            Err(_) => continue,
        };

        // (path, depth). Explicit stack — recursion would blow the stack on
        // pathological trees and gives no way to check cancellation.
        let mut stack: Vec<(PathBuf, i32)> = vec![(root.clone(), 0)];
        let mut since_yield = 0u32;

        while let Some((dir, depth)) = stack.pop() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            if skip.contains(&dir) {
                out.skipped.push(dir);
                continue;
            }
            // Published BEFORE the syscall that can block. `read_dir` on an
            // unresponsive FileProvider root never returns, and this line is
            // then the only record anywhere of which directory that was.
            pos.set(&dir, "readdir");

            since_yield += 1;
            if since_yield >= 64 {
                since_yield = 0;
                // Cheap politeness on a loaded box: the throttled I/O policy
                // handles disk, this keeps us off a core.
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            // Called every directory now, not every 64th. The write it triggers
            // is throttled to 700ms inside the callback, so the cost here is an
            // `Instant::elapsed`, while the benefit is that the persisted
            // `current_path` names the directory the walk is IN rather than one
            // up to 64 directories behind it.
            progress(out.dirs, out.files, out.bytes, &dir, &mut out.findings);

            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue, // permission denied etc — skip, do not fail the scan
            };
            pos.set(&dir, "entries");
            out.dirs += 1;
            let mut local = Acc::default();

            let mut n_entries = 0u32;
            for entry in entries.flatten() {
                n_entries += 1;
                // Statting 200k entries under background I/O priority is slow
                // work, not stuck work. The clock has to advance through it or
                // the watchdog reports a wedge on the busiest directory in the
                // tree — a detector that fires hardest where the scan is
                // working hardest is worse than none.
                if n_entries.is_multiple_of(512) {
                    pos.set(&dir, "entries");
                }
                let path = entry.path();
                let md = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                use std::os::unix::fs::MetadataExt;
                if md.is_symlink() {
                    continue;
                }
                if md.dev() != root_dev {
                    continue; // different filesystem — `du -x` semantics
                }
                let name = entry.file_name().to_string_lossy().to_string();

                if md.is_dir() {
                    // Prune at a known build/dep dir: record it whole.
                    if BUILD_DIRS.contains(&name.as_str()) {
                        let sub = subtree_totals(&path, root_dev, cancel, pos);
                        pos.set(&dir, "entries"); // back out of the pruned subtree
                        out.bytes += sub.bytes;
                        out.files += sub.files;
                        local.bytes += sub.bytes;
                        local.files += sub.files;
                        *local.kinds.entry("build").or_default() += sub.bytes;
                        if sub.bytes >= 64 * 1024 * 1024 && guard_path(&path).is_ok() {
                            let age_d = (now - sub.newest_mtime).max(0) / 86400;
                            out.findings.push(Finding {
                                category: "build",
                                path: path.clone(),
                                bytes: sub.bytes,
                                file_count: sub.files,
                                mtime: sub.newest_mtime,
                                regenerable: true,
                                detail: format!("{name} · untouched {age_d}d · rebuildable"),
                            });
                        }
                        continue;
                    }
                    stack.push((path, depth + 1));
                    continue;
                }

                // regular file. `sz` is on-disk bytes (see `disk_bytes`); the
                // large-file threshold uses apparent size, because a user
                // hunting "large files" means the 8GB video, not its footprint.
                if !seen.count(&md) {
                    continue;
                }
                let sz = disk_bytes(&md);
                let apparent = md.len();
                let mt = md.mtime();
                out.files += 1;
                out.bytes += sz;
                local.files += 1;
                local.bytes += sz;
                local.newest_mtime = local.newest_mtime.max(mt);
                *local.kinds.entry(kind_of(&name)).or_default() += sz;

                if apparent >= cfg.large_file_floor {
                    if guard_path(&path).is_ok() {
                        let age = now - mt;
                        let age_d = age.max(0) / 86400;
                        if age >= cfg.stale_age_secs {
                            out.findings.push(Finding {
                                category: "stale",
                                path: path.clone(),
                                bytes: sz,
                                file_count: 1,
                                mtime: mt,
                                regenerable: false,
                                detail: format!("{} · untouched {age_d}d", kind_of(&name)),
                            });
                        } else {
                            out.findings.push(Finding {
                                category: "large",
                                path: path.clone(),
                                bytes: sz,
                                file_count: 1,
                                mtime: mt,
                                regenerable: false,
                                detail: format!("{} · modified {age_d}d ago", kind_of(&name)),
                            });
                        }
                    }
                    by_size.entry(sz).or_default().push(path.clone());
                }
            }

            // Roll `local` up into every ancestor within tree_depth.
            if depth <= cfg.tree_depth {
                let e = out.tree.entry(dir.clone()).or_default();
                e.bytes += local.bytes;
                e.files += local.files;
                for (k, v) in &local.kinds {
                    *e.kinds.entry(k).or_default() += v;
                }
            }
            // Roll `local` up the ancestor chain. `depth` is already known from
            // the stack, so this walks at most `tree_depth` levels and never
            // recomputes component counts — the previous version called
            // `components().count()` twice per ancestor per directory, which is
            // O(depth^2) string work on every one of ~90k directories.
            let mut cur = dir.as_path();
            let mut pd = depth - 1;
            while pd >= 0 {
                let Some(parent) = cur.parent() else { break };
                if pd <= cfg.tree_depth {
                    let e = out.tree.entry(parent.to_path_buf()).or_default();
                    e.bytes += local.bytes;
                    e.files += local.files;
                    for (k, v) in &local.kinds {
                        *e.kinds.entry(k).or_default() += v;
                    }
                }
                if parent == root {
                    break;
                }
                cur = parent;
                pd -= 1;
            }
        }
    }

    // Cache / dev-tool stores, sized directly. These are prefix matches rather
    // than name matches, so they are collected after the walk.
    for (p, cat, label) in &devtools {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if !p.exists() || guard_path(p).is_err() {
            continue;
        }
        // The skip list governs here too. A dev-tool root that hangs would
        // otherwise wedge the scan from the one code path that never consulted
        // the mechanism built to prevent exactly that.
        if skip.contains(p) {
            out.skipped.push(p.clone());
            continue;
        }
        let sub = subtree_totals(p, 0, cancel, pos);
        if sub.bytes >= 64 * 1024 * 1024 {
            let age_d = (now - sub.newest_mtime).max(0) / 86400;
            out.findings.push(Finding {
                category: match *cat {
                    "devtool" => "devtool",
                    "build" => "build",
                    _ => "cache",
                },
                path: p.clone(),
                bytes: sub.bytes,
                file_count: sub.files,
                mtime: sub.newest_mtime,
                regenerable: true,
                detail: format!("{label} · last write {age_d}d ago"),
            });
        }
    }

    // Duplicate candidates: same exact size, then a cheap head+tail
    // fingerprint. Deliberately NOT a full hash — a full pass over multi-GB
    // files on a machine at load 95 costs more than the space is worth, and
    // these are surfaced for human review rather than auto-selected.
    for (sz, paths) in by_size.iter().filter(|(_, v)| v.len() > 1) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let mut seen: HashMap<String, PathBuf> = HashMap::new();
        for p in paths {
            // Reads two chunks off every same-size candidate. On a large-file
            // set that is real seek time with no directory changing, so the
            // position has to advance here too or the watchdog reads the
            // duplicate pass as a wedge on whatever directory came last.
            pos.set(p, "fingerprint");
            let Some(fp) = fingerprint(p, *sz) else { continue };
            if let Some(first) = seen.get(&fp) {
                out.findings.push(Finding {
                    category: "duplicate",
                    path: p.clone(),
                    bytes: *sz,
                    file_count: 1,
                    mtime: 0,
                    regenerable: false,
                    detail: format!("same size + head/tail as {}", first.display()),
                });
            } else {
                seen.insert(fp, p.clone());
            }
        }
    }

    out
}

/// Total a subtree without recording per-dir detail.
///
/// Same accounting rules as the main walk: on-disk blocks rather than apparent
/// size, and hardlinked files counted once. A cargo `target/` dir is full of
/// hardlinks, so this is not a corner case here.
fn subtree_totals(root: &FsPath, dev_filter: u64, cancel: &AtomicBool, pos: &ScanPos) -> Acc {
    use std::os::unix::fs::MetadataExt;
    let mut acc = Acc::default();
    let mut seen = SeenInodes::default();
    let mut stack = vec![root.to_path_buf()];
    let mut n = 0u32;
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        n += 1;
        if n.is_multiple_of(128) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        // This prune walks a whole subtree — a 400k-file node_modules takes far
        // longer than the stall threshold. Without publishing here, the
        // watchdog would read a legitimately-working scan as wedged, which is
        // the false positive that turns a detector into noise.
        pos.set(&dir, "prune");
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(md) = entry.metadata() else { continue };
            if md.is_symlink() {
                continue;
            }
            if dev_filter != 0 && md.dev() != dev_filter {
                continue;
            }
            if md.is_dir() {
                stack.push(entry.path());
            } else if seen.count(&md) {
                acc.bytes += disk_bytes(&md);
                acc.files += 1;
                acc.newest_mtime = acc.newest_mtime.max(md.mtime());
            }
        }
    }
    acc
}

/// size + first 64KB + last 64KB, hashed. Cheap enough to run on a loaded box.
fn fingerprint(p: &FsPath, size: u64) -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(p).ok()?;
    let mut h = Sha256::new();
    h.update(size.to_le_bytes());
    let mut buf = vec![0u8; 64 * 1024];
    let n = f.read(&mut buf).ok()?;
    h.update(&buf[..n]);
    if size > 128 * 1024 {
        f.seek(SeekFrom::End(-(64 * 1024))).ok()?;
        let n = f.read(&mut buf).ok()?;
        h.update(&buf[..n]);
    }
    Some(hex::encode(h.finalize()))
}

// ── scan orchestration ───────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct StartScan {
    /// Extra roots beyond the default home + dev-cache set.
    #[serde(default)]
    roots: Vec<String>,
    #[serde(default)]
    large_file_mb: Option<u64>,
    #[serde(default)]
    stale_days: Option<i64>,
}

fn default_roots() -> Vec<PathBuf> {
    let home = home_dir();
    let mut v = vec![home.clone()];
    for extra in ["/private/tmp", "/private/var/folders"] {
        let p = PathBuf::from(extra);
        if p.exists() {
            v.push(p);
        }
    }
    v
}

/// Start a scan the way the HTTP endpoint does, for callers that are not HTTP.
///
/// The disk-watch job needs to kick a scan on a schedule. Giving it its own
/// copy of the setup would be a second spelling of the roots, the config
/// defaults and the one-at-a-time guard, and CLAUDE.md's own record of what a
/// duplicated seam costs applies directly: the two would drift the first time
/// either changed, and the drift would be invisible because both still work.
/// So the handler is a thin wrapper over this.
pub(crate) async fn start_background_scan(state: &AppState) -> Result<String, String> {
    begin_scan(state, StartScan { roots: vec![], large_file_mb: None, stale_days: None }, "disk-watch")
        .await
}

async fn begin_scan(
    state: &AppState,
    body: StartScan,
    session: &str,
) -> Result<String, String> {
    // One scan at a time: two concurrent walkers on a loaded disk is exactly
    // the disruption this feature is supposed to avoid.
    if let Ok(conn) = state.store.read() {
        let running: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM reclaim_scans WHERE status = 'running'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if running > 0 {
            return Err("a scan is already running".into());
        }
    }

    let session_owned = session.to_string();
    let scan_id = ulid::Ulid::new().to_string();
    let mut roots = default_roots();
    for r in &body.roots {
        let p = PathBuf::from(shellexpand_home(r));
        if p.is_absolute() && p.exists() {
            roots.push(p);
        }
    }
    let cfg = ScanCfg {
        roots: roots.clone(),
        tree_depth: 3,
        large_file_floor: body.large_file_mb.unwrap_or(500) * 1024 * 1024,
        stale_age_secs: body.stale_days.unwrap_or(90) * 86400,
        include_devtools: true,
    };

    let (free, total) = df_bytes(&home_dir()).unwrap_or((0, 0));
    let snaps = local_snapshots().len() as i64;
    let roots_json = serde_json::to_string(
        &roots.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".into());

    let sid = scan_id.clone();
    let started = now_secs();
    if let Err(e) = state
        .store
        .write_async(move |c| {
            c.execute(
                "INSERT INTO reclaim_scans (id, started_at, status, roots, df_total, df_free, snapshot_count, session)
                 VALUES (?1, ?2, 'running', ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![sid, started, roots_json, total as i64, free as i64, snaps, session_owned],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await
    {
        return Err(e.to_string());
    }

    let cancel = Arc::new(AtomicBool::new(false));
    cancel_registry()
        .lock()
        .map(|mut m| m.insert(scan_id.clone(), cancel.clone()))
        .ok();

    let store = state.store.clone();
    let sid = scan_id.clone();
    std::thread::Builder::new()
        .name(format!("reclaim-scan-{sid}"))
        .spawn(move || run_scan(store, sid, cfg, cancel))
        .ok();

    tracing::info!(scan = %scan_id, roots = roots.len(), by = %session, "reclaim scan started");
    Ok(scan_id)
}

async fn start_scan(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: Option<Json<StartScan>>,
) -> Response {
    let body = body.map(|Json(b)| b).unwrap_or(StartScan {
        roots: vec![],
        large_file_mb: None,
        stale_days: None,
    });
    let session = headers
        .get("X-Amux-Session")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    match begin_scan(&state, body, &session).await {
        Ok(scan_id) => Json(json!({"ok": true, "scan_id": scan_id, "status": "running"}))
            .into_response(),
        Err(e) if e.contains("already running") => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": e,
                "hint": "GET /api/reclaim/scan for its id, or POST /api/reclaim/scan/<id>/cancel"
            })),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

fn shellexpand_home(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        home_dir().join(rest).to_string_lossy().into_owned()
    } else {
        s.to_string()
    }
}

/// One insert path, used by both the incremental flush and the final write, so
/// a column added in one place can never be silently missing from the other.
fn insert_finding(c: &rusqlite::Connection, scan_id: &str, f: &Finding) -> rusqlite::Result<()> {
    c.execute(
        "INSERT OR REPLACE INTO reclaim_findings
           (scan_id, category, path, bytes, file_count, mtime, regenerable, detail)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        rusqlite::params![
            scan_id,
            f.category,
            f.path.to_string_lossy(),
            f.bytes as i64,
            f.file_count as i64,
            f.mtime,
            f.regenerable as i64,
            f.detail
        ],
    )?;
    Ok(())
}

/// Watch the walk thread's position and end the scan honestly if it wedges.
///
/// It cannot UNBLOCK the walker: a thread stuck in `read_dir` on an
/// unresponsive filesystem is stuck in the kernel, ignores the cancel flag, and
/// will sit there until the process exits. What it can do is stop the scan
/// LYING — naming the path and the phase, marking the row terminal so the
/// dashboard stops spinning and the one-scan-at-a-time guard stops refusing
/// every future scan with 409, and recording the directory so the next scan
/// routes around it.
///
/// Order matters. The WARN goes out BEFORE any database access, because the
/// other thing a scan can hang on is the SQLite write lock — and a watchdog
/// that reached for the store first would block in exactly the place the walker
/// did and report nothing at all. That log line is what makes the next
/// occurrence self-announcing in `~/.amux/logs/server-rs.log` instead of
/// needing someone to notice a frozen counter.
fn spawn_stall_watchdog(
    store: crate::db::SharedStore,
    scan_id: String,
    pos: Arc<ScanPos>,
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut warned_at = String::new();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            if done.load(Ordering::Relaxed) {
                return;
            }
            let (path, phase, stalled) = pos.read();

            // Notice early, act late. The WARN costs a log line and is what
            // makes a slow directory visible without deciding anything about
            // it; ending the scan is a verdict and needs the longer evidence.
            // Collapsing the two is what exempted ~/Downloads on one reading.
            if stalled >= STALL_WARN_SECS as f64
                && stalled < STALL_ABORT_SECS as f64
                && warned_at != path
            {
                warned_at = path.clone();
                tracing::warn!(
                    scan = %scan_id, path = %path, phase, quiet_s = stalled as u64,
                    "reclaim scan is quiet on one directory (still waiting, not yet a verdict)"
                );
            }
            if stalled < STALL_ABORT_SECS as f64 {
                continue;
            }

            // A poisoned position means the walk thread panicked holding the
            // lock. That is a crash, not a stall, and reporting it as one would
            // print a nonsense duration (f64::MAX seconds) next to a real
            // finding — the kind of plausible-looking wrong number that gets
            // believed.
            if phase == "poisoned" {
                tracing::error!(scan = %scan_id, "reclaim walk thread panicked mid-scan");
                let sid = scan_id.clone();
                let _ = store.write(move |c| {
                    let n = c.execute(
                        "UPDATE reclaim_scans SET status='failed', finished_at=?2,
                             error='the walk thread panicked; see server-rs.log for the backtrace'
                           WHERE id=?1 AND status='running'",
                        rusqlite::params![sid, now_secs()],
                    )?;
                    Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
                });
                return;
            }

            let stalled_s = stalled as u64;
            tracing::warn!(
                scan = %scan_id,
                path = %path,
                phase,
                stalled_s,
                "reclaim scan stalled: the walk thread is blocked and is not coming back"
            );

            // Only a blocked READDIR indicts the directory. A stalled flush is
            // the database being slow, and blaming the path the walker happened
            // to be standing on would poison the skip list with innocent dirs.
            if phase == "readdir" && !path.is_empty() {
                record_stalled_dir(
                    &store,
                    path.clone(),
                    format!(
                        "readdir did not return after {stalled_s}s (needs {STALL_HITS_TO_SKIP} \
                         such scans before it is skipped)"
                    ),
                );
            }

            cancel.store(true, Ordering::Relaxed);
            let (sid, p, err) = (
                scan_id.clone(),
                path.clone(),
                format!(
                    "stalled {stalled_s}s in {phase} at {path} — the walk thread never returned \
                     from this position"
                ),
            );
            let res = store.write(move |c| {
                let n = c.execute(
                    "UPDATE reclaim_scans
                        SET status='stalled', finished_at=?2, error=?3,
                            current_path=?4, current_phase=?5
                      WHERE id=?1 AND status='running'",
                    rusqlite::params![sid, now_secs(), err, p, phase],
                )?;
                Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
            });
            if let Err(e) = res {
                tracing::error!(scan = %scan_id, error = %e, "could not mark a stalled scan");
            }
            return;
        }
    });
}

fn run_scan(store: crate::db::SharedStore, scan_id: String, cfg: ScanCfg, cancel: Arc<AtomicBool>) {
    let t0 = std::time::Instant::now();
    let mut last_flush = std::time::Instant::now();
    let sid_p = scan_id.clone();
    let store_p = store.clone();

    let skip = load_skips(&store);
    let pos = Arc::new(ScanPos::new());
    let done = Arc::new(AtomicBool::new(false));
    spawn_stall_watchdog(
        store.clone(),
        scan_id.clone(),
        pos.clone(),
        cancel.clone(),
        done.clone(),
    );

    let mut flushed_findings = 0usize;
    let out = {
        let pos_p = pos.clone();
        let mut progress =
            |dirs: u64, files: u64, bytes: u64, cur: &FsPath, pending: &mut Vec<Finding>| {
                if last_flush.elapsed() < std::time::Duration::from_millis(700) {
                    return;
                }
                last_flush = std::time::Instant::now();
                let (sid, cur_s) = (sid_p.clone(), cur.to_string_lossy().into_owned());
                let batch: Vec<Finding> = std::mem::take(pending);
                flushed_findings += batch.len();
                // The phase flip is what separates "the disk is not answering"
                // from "the write lock is not answering" when this never
                // returns. Both freeze dirs_walked identically.
                pos_p.phase("flush");
                let phase_s = "flush";
                let _ = store_p.write(move |c| {
                    c.execute(
                        "UPDATE reclaim_scans SET dirs_walked=?2, files_walked=?3, bytes_seen=?4, current_path=?5, current_phase=?6 WHERE id=?1",
                        rusqlite::params![sid, dirs as i64, files as i64, bytes as i64, cur_s, phase_s],
                    )?;
                    for f in &batch {
                        insert_finding(c, &sid, f)?;
                    }
                    Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                });
                pos_p.set(cur, "readdir");
            };
        walk(&cfg, &cancel, &pos, &skip, &mut progress)
    };
    // Before the finalize write, so a slow rollup is not mistaken for a wedge.
    done.store(true, Ordering::Relaxed);

    let cancelled = cancel.load(Ordering::Relaxed);
    let elapsed = t0.elapsed().as_secs_f64();
    let n_find = flushed_findings + out.findings.len();
    let n_tree = out.tree.len();
    // Refresh the skips this scan actually leaned on. A skip row that stops
    // being hit is one whose filesystem may have come back — without a
    // last_seen that moves, the table can only ever grow and nobody can tell a
    // live exemption from a fossil.
    let hit_skips: Vec<String> = out
        .skipped
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let n_skipped = hit_skips.len();
    if !hit_skips.is_empty() {
        let now = now_secs();
        let _ = store.write(move |c| {
            for p in &hit_skips {
                c.execute(
                    "UPDATE reclaim_skipped SET last_seen=?2, hits=hits+1 WHERE path=?1",
                    rusqlite::params![p, now],
                )?;
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        });
    }

    // Snapshot-held space as a first-class finding. Without this the UI shows
    // a big reclaimable total that the disk will not actually hand back.
    let snaps = local_snapshots();
    let root = cfg.roots.first().cloned().unwrap_or_else(home_dir);
    let (free_now, _) = df_bytes(&root).unwrap_or((0, 0));

    let sid = scan_id.clone();
    let finished = now_secs();
    let res = store.write(move |c| {
        for f in &out.findings {
            insert_finding(c, &sid, f)?;
        }
        if !snaps.is_empty() {
            c.execute(
                "INSERT OR REPLACE INTO reclaim_findings
                   (scan_id, category, path, bytes, file_count, mtime, regenerable, detail)
                 VALUES (?1,'snapshot','APFS local snapshots',0,?2,0,0,?3)",
                rusqlite::params![
                    sid,
                    snaps.len() as i64,
                    format!(
                        "{} hourly local Time Machine snapshots retain blocks from deleted files. \
                         Until these expire or are thinned, deleting files will NOT increase free space. \
                         Oldest: {}",
                        snaps.len(),
                        snaps.first().map(|s| s.as_str()).unwrap_or("?")
                    )
                ],
            )?;
        }
        for (path, acc) in &out.tree {
            let kind = acc
                .kinds
                .iter()
                .max_by_key(|(_, v)| **v)
                .map(|(k, _)| *k)
                .unwrap_or("other");
            let parent = path.parent().map(|p| p.to_string_lossy().into_owned());
            let depth = path.components().count() as i64;
            c.execute(
                "INSERT OR REPLACE INTO reclaim_tree (scan_id, path, parent, depth, bytes, file_count, kind)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                rusqlite::params![
                    sid,
                    path.to_string_lossy(),
                    parent,
                    depth,
                    acc.bytes as i64,
                    acc.files as i64,
                    kind
                ],
            )?;
        }
        // `AND status='running'` so a walker that eventually unblocks cannot
        // overwrite the watchdog's verdict. A scan the watchdog declared
        // stalled stays stalled: the row that told the truth wins over the one
        // that arrived twenty minutes late claiming success.
        c.execute(
            "UPDATE reclaim_scans SET status=?2, finished_at=?3, dirs_walked=?4, files_walked=?5,
                 bytes_seen=?6, current_path=NULL, current_phase=NULL, df_free=?7
             WHERE id=?1 AND status='running'",
            rusqlite::params![
                sid,
                if cancelled { "cancelled" } else { "done" },
                finished,
                out.dirs as i64,
                out.files as i64,
                out.bytes as i64,
                free_now as i64
            ],
        )?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    });

    cancel_registry().lock().map(|mut m| m.remove(&scan_id)).ok();

    match res {
        Ok(_) => tracing::info!(
            scan = %scan_id, findings = n_find, tree_nodes = n_tree,
            elapsed_s = elapsed, cancelled, skipped_dirs = n_skipped,
            "reclaim scan finished"
        ),
        // A scan that dies here leaves a row stuck in 'running', which blocks
        // every future scan — so it must be loud, not a silent thread exit.
        Err(e) => tracing::error!(scan = %scan_id, error = %e, "reclaim scan failed to persist"),
    }
}

// ── read endpoints ───────────────────────────────────────────────────────────

async fn list_scans(State(state): State<AppState>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    let mut stmt = match conn.prepare(
        "SELECT id, started_at, finished_at, status, dirs_walked, files_walked, bytes_seen,
                current_path, df_total, df_free, snapshot_count
         FROM reclaim_scans ORDER BY started_at DESC LIMIT 20",
    ) {
        Ok(s) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let rows = stmt.query_map([], |r| {
        Ok(json!({
            "id": r.get::<_, String>(0)?,
            "started_at": r.get::<_, i64>(1)?,
            "finished_at": r.get::<_, Option<i64>>(2)?,
            "status": r.get::<_, String>(3)?,
            "dirs_walked": r.get::<_, i64>(4)?,
            "files_walked": r.get::<_, i64>(5)?,
            "bytes_seen": r.get::<_, i64>(6)?,
            "current_path": r.get::<_, Option<String>>(7)?,
            "df_total": r.get::<_, Option<i64>>(8)?,
            "df_free": r.get::<_, Option<i64>>(9)?,
            "snapshot_count": r.get::<_, Option<i64>>(10)?,
        }))
    });
    let list: Vec<_> = rows.map(|r| r.flatten().collect()).unwrap_or_default();
    Json(json!({"scans": list})).into_response()
}

#[derive(Deserialize)]
pub struct FindingsQuery {
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

async fn get_scan(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<FindingsQuery>,
) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    // `latest` resolves to the newest scan so the UI has a stable entry point.
    let id = if id == "latest" {
        conn.query_row(
            "SELECT id FROM reclaim_scans ORDER BY started_at DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or(id)
    } else {
        id
    };

    let scan = conn.query_row(
        "SELECT id, started_at, finished_at, status, roots, error, dirs_walked, files_walked,
                bytes_seen, current_path, df_total, df_free, snapshot_count, current_phase
         FROM reclaim_scans WHERE id = ?1",
        [&id],
        |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "started_at": r.get::<_, i64>(1)?,
                "finished_at": r.get::<_, Option<i64>>(2)?,
                "status": r.get::<_, String>(3)?,
                "roots": serde_json::from_str::<serde_json::Value>(&r.get::<_, String>(4)?).unwrap_or(json!([])),
                "error": r.get::<_, Option<String>>(5)?,
                "dirs_walked": r.get::<_, i64>(6)?,
                "files_walked": r.get::<_, i64>(7)?,
                "bytes_seen": r.get::<_, i64>(8)?,
                "current_path": r.get::<_, Option<String>>(9)?,
                "df_total": r.get::<_, Option<i64>>(10)?,
                "df_free": r.get::<_, Option<i64>>(11)?,
                "snapshot_count": r.get::<_, Option<i64>>(12)?,
                // Which of the two identical-looking stalls this was. Absent
                // from the first version, which is why a wedged scan could only
                // report that it had stopped, never where or in what.
                "current_phase": r.get::<_, Option<String>>(13)?,
            }))
        },
    );
    let Ok(scan) = scan else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "no such scan"}))).into_response();
    };

    let limit = q.limit.unwrap_or(300).clamp(1, 2000);
    let (sql, params): (&str, Vec<&dyn rusqlite::ToSql>) = match &q.category {
        Some(c) => (
            "SELECT category, path, bytes, file_count, mtime, regenerable, detail
             FROM reclaim_findings WHERE scan_id=?1 AND category=?2 ORDER BY bytes DESC LIMIT ?3",
            vec![&id, c, &limit],
        ),
        None => (
            "SELECT category, path, bytes, file_count, mtime, regenerable, detail
             FROM reclaim_findings WHERE scan_id=?1 ORDER BY bytes DESC LIMIT ?2",
            vec![&id, &limit],
        ),
    };
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let rows = stmt.query_map(params.as_slice(), |r| {
        Ok(json!({
            "category": r.get::<_, String>(0)?,
            "path": r.get::<_, String>(1)?,
            "bytes": r.get::<_, i64>(2)?,
            "file_count": r.get::<_, i64>(3)?,
            "mtime": r.get::<_, i64>(4)?,
            "regenerable": r.get::<_, i64>(5)? != 0,
            "detail": r.get::<_, Option<String>>(6)?,
        }))
    });
    let findings: Vec<_> = rows.map(|r| r.flatten().collect()).unwrap_or_default();

    // Per-category rollup, so the UI can show totals without pulling every row.
    let mut cstmt = conn
        .prepare(
            "SELECT category, COUNT(*), COALESCE(SUM(bytes),0) FROM reclaim_findings
             WHERE scan_id=?1 GROUP BY category ORDER BY 3 DESC",
        )
        .ok();
    let totals: Vec<_> = cstmt
        .as_mut()
        .and_then(|s| {
            s.query_map([&id], |r| {
                Ok(json!({
                    "category": r.get::<_, String>(0)?,
                    "count": r.get::<_, i64>(1)?,
                    "bytes": r.get::<_, i64>(2)?,
                }))
            })
            .map(|rows| rows.flatten().collect())
            .ok()
        })
        .unwrap_or_default();

    // Shipped with every scan, not behind its own call, because "what did you
    // NOT look at" is part of reading the result honestly: a home directory
    // total that silently omits a subtree is a confident wrong answer.
    let skipped = read_skips(&conn);

    Json(json!({"scan": scan, "findings": findings, "totals": totals, "skipped": skipped}))
        .into_response()
}

fn read_skips(conn: &rusqlite::Connection) -> Vec<serde_json::Value> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT path, reason, detail, first_seen, last_seen, hits
           FROM reclaim_skipped ORDER BY reason, path",
    ) else {
        return vec![];
    };
    stmt.query_map([], |r| {
        let reason: String = r.get(1)?;
        let hits: i64 = r.get(5)?;
        Ok(json!({
            "path": r.get::<_, String>(0)?,
            "reason": reason,
            "detail": r.get::<_, Option<String>>(2)?,
            "first_seen": r.get::<_, i64>(3)?,
            "last_seen": r.get::<_, i64>(4)?,
            "hits": hits,
            // Whether this row is actually EXCLUDING anything, as opposed to
            // being a recorded observation waiting for corroboration. Without
            // it the list reads as "these are all skipped", which is the
            // view-disagrees-with-the-mechanism failure: a reader would see
            // ~/Downloads listed and conclude it was no longer scanned.
            "active": reason == "provider" || hits >= STALL_HITS_TO_SKIP,
        }))
    })
    .map(|rows| rows.flatten().collect())
    .unwrap_or_default()
}

async fn list_skipped(State(state): State<AppState>) -> Response {
    match state.store.read() {
        Ok(c) => Json(json!({"skipped": read_skips(&c)})).into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct SkipPath {
    path: String,
}

/// Forget a skip, so the next scan tries that directory again.
///
/// The exit condition for a learned exemption. A filesystem that hung once may
/// answer next week, and without this the skip list is a one-way ratchet whose
/// coverage only ever shrinks — the failure mode where a cleanup scan quietly
/// stops looking at the places most worth cleaning.
async fn unskip(State(state): State<AppState>, Query(q): Query<SkipPath>) -> Response {
    let path = q.path.clone();
    match state.store.write(move |c| {
        let n = c.execute("DELETE FROM reclaim_skipped WHERE path=?1", [&path])?;
        Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
    }) {
        Ok(r) if r.applied => {
            tracing::info!(path = %q.path, "reclaim skip cleared; next scan will try it again");
            Json(json!({"ok": true, "path": q.path})).into_response()
        }
        Ok(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "not skipped", "path": q.path})),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn cancel_scan(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let found = cancel_registry()
        .lock()
        .ok()
        .and_then(|m| m.get(&id).cloned());
    match found {
        Some(flag) => {
            flag.store(true, Ordering::Relaxed);
            tracing::warn!(scan = %id, "reclaim scan cancelled by request");
            Json(json!({"ok": true, "cancelling": true})).into_response()
        }
        None => {
            // Not in the registry: either already finished, or the process
            // restarted mid-scan and left the row stuck. Reap it either way so
            // a stale 'running' row can never wedge every future scan.
            let sid = id.clone();
            let _ = state
                .store
                .write_async(move |c| {
                    c.execute(
                        "UPDATE reclaim_scans SET status='cancelled', finished_at=?2,
                         error=COALESCE(error,'reaped: no live scan thread') WHERE id=?1 AND status='running'",
                        rusqlite::params![sid, now_secs()],
                    )?;
                    Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                })
                .await;
            Json(json!({"ok": true, "cancelling": false, "reaped": true})).into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct TreeQuery {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    depth: Option<i64>,
}

/// Hierarchical sizes for the treemap. Returns the children of `path` (default:
/// the scan's first root) with rolled-up bytes and a dominant file-type bucket.
async fn get_tree(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<TreeQuery>,
) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    let id = if id == "latest" {
        conn.query_row(
            "SELECT id FROM reclaim_scans WHERE status IN ('done','cancelled') ORDER BY started_at DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or(id)
    } else {
        id
    };
    let root = match q.path.clone() {
        Some(p) => shellexpand_home(&p),
        None => home_dir().to_string_lossy().into_owned(),
    };
    let limit = q.depth.unwrap_or(200).clamp(1, 2000);

    let self_row = conn
        .query_row(
            "SELECT bytes, file_count, kind FROM reclaim_tree WHERE scan_id=?1 AND path=?2",
            rusqlite::params![id, root],
            |r| {
                Ok(json!({
                    "path": root,
                    "bytes": r.get::<_, i64>(0)?,
                    "file_count": r.get::<_, i64>(1)?,
                    "kind": r.get::<_, Option<String>>(2)?,
                }))
            },
        )
        .ok();

    let mut stmt = match conn.prepare(
        "SELECT path, bytes, file_count, kind FROM reclaim_tree
         WHERE scan_id=?1 AND parent=?2 ORDER BY bytes DESC LIMIT ?3",
    ) {
        Ok(s) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let rows = stmt.query_map(rusqlite::params![id, root, limit], |r| {
        Ok(json!({
            "path": r.get::<_, String>(0)?,
            "bytes": r.get::<_, i64>(1)?,
            "file_count": r.get::<_, i64>(2)?,
            "kind": r.get::<_, Option<String>>(3)?,
        }))
    });
    let children: Vec<_> = rows.map(|r| r.flatten().collect()).unwrap_or_default();
    Json(json!({"scan_id": id, "root": self_row, "children": children})).into_response()
}

async fn list_snapshots(State(_state): State<AppState>) -> Response {
    let snaps = local_snapshots();
    let (free, total) = df_bytes(&home_dir()).unwrap_or((0, 0));
    Json(json!({
        "snapshots": snaps,
        "count": snaps.len(),
        "df_free": free,
        "df_total": total,
        "note": "Local APFS snapshots retain blocks from deleted files. While these exist, \
                 deleting files can free `du` space without freeing `df` space.",
        "thin_command": "sudo tmutil thinlocalsnapshots / 21474836480 4",
    }))
    .into_response()
}

// ── quarantine ───────────────────────────────────────────────────────────────

fn quarantine_root() -> PathBuf {
    home_dir().join(".amux/quarantine")
}

#[derive(Deserialize)]
pub struct QuarantineReq {
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    scan_id: Option<String>,
}

/// Move selected paths aside. Reversible until purged.
async fn create_quarantine(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<QuarantineReq>,
) -> Response {
    if body.paths.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no paths given"})),
        )
            .into_response();
    }
    let session = headers
        .get("X-Amux-Session")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let batch = ulid::Ulid::new().to_string();
    let dest_root = quarantine_root().join(&batch);
    if let Err(e) = std::fs::create_dir_all(&dest_root) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("cannot create quarantine dir: {e}")})),
        )
            .into_response();
    }

    let (free_before, _) = df_bytes(&home_dir()).unwrap_or((0, 0));
    let cancel = AtomicBool::new(false);
    let mut moved = Vec::new();
    let mut refused = Vec::new();
    let mut total: u64 = 0;

    for raw in &body.paths {
        let p = PathBuf::from(shellexpand_home(raw));
        if let Err(reason) = guard_path(&p) {
            // Refusals are returned, not silently dropped: a caller that asked
            // for 10 paths and got 7 moved must be able to see which 3 and why.
            tracing::warn!(path = %p.display(), reason = %reason, "quarantine refused by guard");
            refused.push(json!({"path": raw, "reason": reason}));
            continue;
        }
        if !p.exists() {
            refused.push(json!({"path": raw, "reason": "does not exist"}));
            continue;
        }
        // Sizing one quarantine candidate, not a scan: no watchdog is reading
        // this position, so it gets its own throwaway.
        let sz = subtree_totals(&p, 0, &cancel, &ScanPos::new()).bytes.max(
            std::fs::symlink_metadata(&p).map(|m| m.len()).unwrap_or(0),
        );
        // Preserve the original path shape under the batch dir so restore is
        // unambiguous and a human can read the staging area directly.
        let rel = p.strip_prefix("/").unwrap_or(&p);
        let dest = dest_root.join(rel);
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::rename(&p, &dest) {
            Ok(_) => {
                total += sz;
                moved.push((p.to_string_lossy().into_owned(), dest.to_string_lossy().into_owned(), sz));
            }
            Err(e) => {
                tracing::warn!(path = %p.display(), error = %e, "quarantine move failed");
                refused.push(json!({"path": raw, "reason": format!("move failed: {e}")}));
            }
        }
    }

    let (free_after, _) = df_bytes(&home_dir()).unwrap_or((0, 0));
    let bid = batch.clone();
    let scan_id = body.scan_id.clone();
    let items = moved.clone();
    let count = moved.len() as i64;
    let res = state
        .store
        .write_async(move |c| {
            c.execute(
                "INSERT INTO reclaim_quarantine (id, created_at, scan_id, status, item_count, bytes, df_free_before, df_free_after, session)
                 VALUES (?1,?2,?3,'staged',?4,?5,?6,?7,?8)",
                rusqlite::params![bid, now_secs(), scan_id, count, total as i64, free_before as i64, free_after as i64, session],
            )?;
            for (orig, staged, sz) in &items {
                c.execute(
                    "INSERT INTO reclaim_quarantine_items (batch_id, original_path, staged_path, bytes, status)
                     VALUES (?1,?2,?3,?4,'staged')",
                    rusqlite::params![bid, orig, staged, *sz as i64],
                )?;
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    if let Err(e) = res {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }

    tracing::info!(
        batch = %batch, moved = moved.len(), refused = refused.len(),
        bytes = total, freed = free_after.saturating_sub(free_before),
        "reclaim quarantine staged"
    );
    Json(json!({
        "ok": true,
        "batch_id": batch,
        "moved": moved.len(),
        "bytes": total,
        "refused": refused,
        "df_free_before": free_before,
        "df_free_after": free_after,
        // The honest number. A move within one volume frees nothing until
        // purge, and even then snapshots may hold the blocks.
        "actually_freed": free_after.saturating_sub(free_before),
        "note": "Staged, not deleted. Space is not returned until you purge this batch, \
                 and APFS local snapshots may hold the blocks after that."
    }))
    .into_response()
}

async fn list_quarantine(State(state): State<AppState>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    let mut stmt = match conn.prepare(
        "SELECT id, created_at, status, item_count, bytes, df_free_before, df_free_after, purged_at, session
         FROM reclaim_quarantine ORDER BY created_at DESC LIMIT 50",
    ) {
        Ok(s) => s,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let rows = stmt.query_map([], |r| {
        Ok(json!({
            "id": r.get::<_, String>(0)?,
            "created_at": r.get::<_, i64>(1)?,
            "status": r.get::<_, String>(2)?,
            "item_count": r.get::<_, i64>(3)?,
            "bytes": r.get::<_, i64>(4)?,
            "df_free_before": r.get::<_, Option<i64>>(5)?,
            "df_free_after": r.get::<_, Option<i64>>(6)?,
            "purged_at": r.get::<_, Option<i64>>(7)?,
            "session": r.get::<_, Option<String>>(8)?,
        }))
    });
    let batches: Vec<_> = rows.map(|r| r.flatten().collect()).unwrap_or_default();

    let mut istmt = conn
        .prepare(
            "SELECT batch_id, original_path, staged_path, bytes, status FROM reclaim_quarantine_items
             ORDER BY bytes DESC LIMIT 500",
        )
        .ok();
    let items: Vec<_> = istmt
        .as_mut()
        .and_then(|s| {
            s.query_map([], |r| {
                Ok(json!({
                    "batch_id": r.get::<_, String>(0)?,
                    "original_path": r.get::<_, String>(1)?,
                    "staged_path": r.get::<_, String>(2)?,
                    "bytes": r.get::<_, i64>(3)?,
                    "status": r.get::<_, String>(4)?,
                }))
            })
            .map(|rows| rows.flatten().collect())
            .ok()
        })
        .unwrap_or_default();

    Json(json!({"batches": batches, "items": items, "root": quarantine_root().to_string_lossy()}))
        .into_response()
}

async fn restore_quarantine(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    // Scoped so the pooled connection is definitively dropped before the
    // `.await` below — a live `PooledConnection` across an await makes the
    // handler future non-Send and axum rejects it.
    let pairs: Vec<(String, String)> = {
        let conn = match state.store.read() {
            Ok(c) => c,
            Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
        };
        let mut stmt = match conn.prepare(
            "SELECT original_path, staged_path FROM reclaim_quarantine_items WHERE batch_id=?1 AND status='staged'",
        ) {
            Ok(s) => s,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
        stmt.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    };

    let mut restored = 0usize;
    let mut failed = Vec::new();
    for (orig, staged) in &pairs {
        let op = PathBuf::from(orig);
        if op.exists() {
            failed.push(json!({"path": orig, "reason": "something already exists at the original path"}));
            continue;
        }
        if let Some(parent) = op.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::rename(staged, &op) {
            Ok(_) => restored += 1,
            Err(e) => failed.push(json!({"path": orig, "reason": e.to_string()})),
        }
    }

    let bid = id.clone();
    let ok_all = failed.is_empty();
    let _ = state
        .store
        .write_async(move |c| {
            c.execute(
                "UPDATE reclaim_quarantine_items SET status='restored' WHERE batch_id=?1 AND status='staged'",
                [&bid],
            )?;
            c.execute(
                "UPDATE reclaim_quarantine SET status=?2 WHERE id=?1",
                rusqlite::params![bid, if ok_all { "restored" } else { "failed" }],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;

    tracing::info!(batch = %id, restored, failed = failed.len(), "reclaim quarantine restored");
    Json(json!({"ok": ok_all, "restored": restored, "failed": failed})).into_response()
}

#[derive(Deserialize)]
pub struct PurgeQuery {
    /// Must be the batch id. A purge is the one irreversible step here, so it
    /// requires the caller to name what they are destroying rather than
    /// accepting a bare DELETE on a URL they might have arrived at by accident.
    #[serde(default)]
    confirm: Option<String>,
}

async fn purge_quarantine(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PurgeQuery>,
) -> Response {
    if q.confirm.as_deref() != Some(id.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "purge requires ?confirm=<batch_id>",
                "batch_id": id,
                "hint": "this permanently deletes the staged files"
            })),
        )
            .into_response();
    }
    let dir = quarantine_root().join(&id);
    if !dir.starts_with(quarantine_root()) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad batch id"}))).into_response();
    }
    let (free_before, _) = df_bytes(&home_dir()).unwrap_or((0, 0));
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::error!(batch = %id, error = %e, "reclaim purge failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    }
    let (free_after, _) = df_bytes(&home_dir()).unwrap_or((0, 0));
    let snaps = local_snapshots().len();

    let bid = id.clone();
    let _ = state
        .store
        .write_async(move |c| {
            c.execute(
                "UPDATE reclaim_quarantine_items SET status='purged' WHERE batch_id=?1",
                [&bid],
            )?;
            c.execute(
                "UPDATE reclaim_quarantine SET status='purged', purged_at=?2, df_free_after=?3 WHERE id=?1",
                rusqlite::params![bid, now_secs(), free_after as i64],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;

    let freed: u64 = free_after.saturating_sub(free_before);
    tracing::info!(batch = %id, freed, snapshots = snaps, "reclaim quarantine purged");
    Json(json!({
        "ok": true,
        "batch_id": id,
        "actually_freed": freed,
        "df_free": free_after,
        "snapshot_count": snaps,
        // Say it at the moment the number disappoints, not in a doc nobody
        // reads: this is exactly where a user concludes the feature is broken.
        "note": if snaps > 0 && freed < 1024 * 1024 * 64 {
            format!("Files are deleted, but free space barely moved because {snaps} APFS local snapshots \
                     still reference the blocks. Run `sudo tmutil thinlocalsnapshots / 21474836480 4` \
                     to release them.")
        } else {
            String::new()
        }
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard is the only thing standing between a size-ranking heuristic
    /// and the user's data, so it gets a check that can actually fail. Each
    /// path below is one a "biggest directories first" ranking WOULD nominate:
    /// on this machine ~/Library is 372G and ~/Dev is 321G, which are the two
    /// largest things in home and both catastrophic to move.
    #[test]
    fn guard_refuses_paths_no_heuristic_may_touch() {
        let home = home_dir();
        let must_refuse: Vec<PathBuf> = vec![
            home.clone(),
            home.join("Library"),
            home.join("Dev"),
            home.join("Documents"),
            home.join("Downloads"),
            home.join(".amux/sessions"),
            home.join(".amux/memory"),
            home.join(".amux/logs"),
            home.join(".amux/amux.db"),
            home.join("Dev/amux/.git"),
            home.join("Dev/amux/.git/objects"),
            PathBuf::from("/System/Library"),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/Applications"),
            PathBuf::from("/Volumes/Backup"),
            PathBuf::from("relative/path"),
            PathBuf::from("/Users/ethan/Dev/../../etc"),
        ];
        for p in must_refuse {
            assert!(
                guard_path(&p).is_err(),
                "guard MUST refuse {} but allowed it",
                p.display()
            );
        }
    }

    /// The mirror half: a guard that refuses everything would pass the test
    /// above while making the feature useless, so assert the legitimate
    /// targets still get through.
    #[test]
    fn guard_allows_real_reclaim_targets() {
        let home = home_dir();
        let must_allow: Vec<PathBuf> = vec![
            home.join(".amux/rust-build-target"),
            home.join(".amux/rust-build-target-e2e-head"),
            home.join("Library/Caches"),
            home.join("Library/Developer/Xcode/DerivedData"),
            home.join(".ollama/models"),
            home.join("Dev/amux/target"),
            home.join("Dev/someproject/node_modules"),
            home.join(".cache"),
            PathBuf::from("/private/tmp/amux-build-target"),
        ];
        for p in must_allow {
            assert!(
                guard_path(&p).is_ok(),
                "guard must ALLOW {} but refused: {:?}",
                p.display(),
                guard_path(&p)
            );
        }
    }

    /// `df_bytes` is the honest-reclaim measurement. If it silently returns
    /// None the UI would show "0 freed" forever and read as a broken feature,
    /// so confirm it answers for a path that certainly exists.
    #[test]
    fn df_bytes_answers_for_home() {
        let (free, total) = df_bytes(&home_dir()).expect("statfs must answer for $HOME");
        assert!(total > 0, "total capacity should be positive");
        assert!(free <= total, "free ({free}) cannot exceed total ({total})");
    }

    /// The bug this pins: a sparse file reports a huge `len()` while occupying
    /// almost no disk. Sizing by `len()` made the first scan report 1.14 TB for
    /// a home directory `du` measures at ~820 GB. Built from a real sparse file
    /// rather than a mock, because the whole point is what the filesystem does.
    #[test]
    fn sizing_uses_allocated_blocks_not_apparent_size() {
        use std::io::{Seek, SeekFrom, Write};
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("sparse.bin");
        let mut f = std::fs::File::create(&p).expect("create");
        // 512MB apparent, one byte written at the end => nearly zero allocated.
        f.seek(SeekFrom::Start(512 * 1024 * 1024)).expect("seek");
        f.write_all(b"x").expect("write");
        f.sync_all().ok();
        drop(f);

        let md = std::fs::symlink_metadata(&p).expect("stat");
        let apparent = md.len();
        let on_disk = disk_bytes(&md);
        assert!(apparent > 512 * 1024 * 1024, "apparent size should be huge");
        assert!(
            on_disk < apparent / 2,
            "on-disk ({on_disk}) should be far below apparent ({apparent}) for a sparse file — \
             if these are equal, disk_bytes has regressed to len()"
        );
    }

    /// A cargo `target/` dir is full of hardlinks, so counting per-path
    /// double-counts real gigabytes. Asserts the dedup actually suppresses the
    /// second sighting, and that it does NOT suppress ordinary files.
    #[test]
    fn hardlinks_are_counted_once_but_normal_files_are_not_suppressed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.bin");
        std::fs::write(&a, vec![7u8; 4096]).expect("write");
        let b = dir.path().join("b.bin");
        std::fs::hard_link(&a, &b).expect("hard_link");

        let mut seen = SeenInodes::default();
        let ma = std::fs::symlink_metadata(&a).expect("stat a");
        let mb = std::fs::symlink_metadata(&b).expect("stat b");
        assert!(seen.count(&ma), "first sighting of a hardlink must count");
        assert!(!seen.count(&mb), "second path to the same inode must NOT count");

        // A plain file with nlink == 1 must always count, even twice-seen
        // metadata objects, or ordinary files would go missing from totals.
        let c = dir.path().join("c.bin");
        std::fs::write(&c, vec![1u8; 128]).expect("write c");
        let mc = std::fs::symlink_metadata(&c).expect("stat c");
        assert!(seen.count(&mc), "an unlinked-once file must count");
    }

    #[test]
    fn kind_buckets_cover_the_types_the_treemap_colors() {
        assert_eq!(kind_of("model.gguf"), "model");
        assert_eq!(kind_of("clip.mp4"), "media");
        assert_eq!(kind_of("Docker.raw"), "data");
        assert_eq!(kind_of("lib.rlib"), "build");
        assert_eq!(kind_of("main.rs"), "code");
        assert_eq!(kind_of("no-extension"), "other");
    }

    fn cfg_for(root: &FsPath) -> ScanCfg {
        ScanCfg {
            roots: vec![root.to_path_buf()],
            tree_depth: 8,
            large_file_floor: u64::MAX, // no findings; these tests are about the walk
            stale_age_secs: i64::MAX,
            // Never true in a test. See the field's own comment: this is what
            // made two of these tests walk the whole machine.
            include_devtools: false,
        }
    }

    /// A walk must stay inside the roots it was given.
    ///
    /// It did not. `devtool_roots()` is a list of REAL absolute paths, walked
    /// unconditionally at the end of every `walk()`, so a unit test pointed at
    /// a three-file temp directory also scanned `~/.cache`, `~/Library/Caches`
    /// and a 15GB shared cargo target dir under background I/O priority. The
    /// two tests that call `walk()` ran for fourteen hours at 0% CPU, and
    /// because cargo serialises on a single build lock they took every other
    /// lane's `cargo test` down with them — a peer had to start gating with
    /// `--skip api::reclaim` to get a green suite.
    ///
    /// The assertion is on PATHS rather than on elapsed time, because a timing
    /// assertion on a shared machine at load 95 is a flake generator. A leaked
    /// devtool root shows up as a finding outside the tempdir, which is exact.
    #[test]
    fn a_walk_stays_inside_the_roots_it_was_given() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("f.bin"), vec![0u8; 512]).expect("write");

        let pos = ScanPos::new();
        let cancel = AtomicBool::new(false);
        let skip = std::collections::HashSet::new();
        let mut noop = |_: u64, _: u64, _: u64, _: &FsPath, _: &mut Vec<Finding>| {};
        let out = walk(&cfg_for(dir.path()), &cancel, &pos, &skip, &mut noop);

        // Non-vacuity: a walk that produced nothing would satisfy the stray
        // check trivially, and an empty filter result is indistinguishable from
        // a correct one on the rows alone.
        assert!(!out.tree.is_empty(), "the walk must have recorded something");

        let root = dir.path();
        let strays: Vec<String> = out
            .findings
            .iter()
            .map(|f| f.path.clone())
            .chain(out.tree.keys().cloned())
            .filter(|p| !p.starts_with(root))
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            strays.is_empty(),
            "the walk left its roots and touched: {strays:?}"
        );
    }

    /// The property that makes a stall diagnosable: at the moment the walk is
    /// about to enter a directory, the published position IS that directory.
    ///
    /// It fails if `pos.set` moves after `read_dir` — which is the version that
    /// cannot answer, because the syscall that hangs is the one in between.
    /// The old code published every 64th directory instead, so a wedge named
    /// some ancestor up to 64 pops earlier and the real culprit was never
    /// written down anywhere.
    #[test]
    fn position_names_the_directory_about_to_be_read_not_the_last_one_finished() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..12 {
            let sub = dir.path().join(format!("sub{i}"));
            std::fs::create_dir(&sub).expect("mkdir");
            std::fs::write(sub.join("f.bin"), vec![0u8; 64]).expect("write");
        }

        let pos = ScanPos::new();
        let cancel = AtomicBool::new(false);
        let skip = std::collections::HashSet::new();
        let mut mismatches: Vec<String> = Vec::new();
        let mut calls = 0usize;
        {
            let mut progress =
                |_d: u64, _f: u64, _b: u64, cur: &FsPath, _p: &mut Vec<Finding>| {
                    calls += 1;
                    let (path, phase, _) = pos.read();
                    if path != cur.to_string_lossy() || phase != "readdir" {
                        mismatches.push(format!("at {cur:?}: published {phase} at {path}"));
                    }
                };
            walk(&cfg_for(dir.path()), &cancel, &pos, &skip, &mut progress);
        }

        // The 700ms throttle lives in run_scan's callback, not here, so every
        // directory reports — 13 of them (root + 12 subdirectories).
        assert!(calls >= 13, "progress must fire per directory, saw {calls}");
        assert!(mismatches.is_empty(), "position lagged the walk: {mismatches:?}");
    }

    /// A directory on the skip list is stepped over, and SAID to be stepped
    /// over, while its siblings are still walked.
    ///
    /// The silent-skip version of this passes a scan that quietly omits a
    /// subtree, which is the same wrong answer as a scan that crashes, except
    /// it is delivered with a completion status.
    #[test]
    fn a_skipped_directory_is_reported_and_its_siblings_still_walked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hazard = dir.path().join("hazard");
        let fine = dir.path().join("fine");
        std::fs::create_dir(&hazard).expect("mkdir");
        std::fs::create_dir(&fine).expect("mkdir");
        std::fs::write(hazard.join("big.bin"), vec![9u8; 40_000]).expect("write");
        std::fs::write(fine.join("small.bin"), vec![1u8; 4_000]).expect("write");

        let pos = ScanPos::new();
        let cancel = AtomicBool::new(false);
        let mut skip = std::collections::HashSet::new();
        skip.insert(hazard.clone());
        let mut noop = |_: u64, _: u64, _: u64, _: &FsPath, _: &mut Vec<Finding>| {};
        let out = walk(&cfg_for(dir.path()), &cancel, &pos, &skip, &mut noop);

        assert_eq!(out.skipped, vec![hazard.clone()], "the skip must be reported");
        assert!(
            out.bytes >= 4_000 && out.bytes < 40_000,
            "sibling walked, hazard not: got {} bytes",
            out.bytes
        );

        // Without the skip the same tree yields the hazard's bytes too, so the
        // assertion above is discriminating rather than merely satisfiable.
        let empty = std::collections::HashSet::new();
        let full = walk(&cfg_for(dir.path()), &cancel, &pos, &empty, &mut noop);
        assert!(
            full.bytes > out.bytes,
            "control: an unskipped walk must see MORE ({} vs {})",
            full.bytes,
            out.bytes
        );
    }

    /// The reaper must not destroy the evidence it exists to expose.
    ///
    /// The first version cleared `current_path`, so the scan that wedged at
    /// 1,087 directories came back saying only that the server had restarted —
    /// and locating the actual culprit took a hand-run walk with a stopwatch.
    #[test]
    fn reap_preserves_the_position_a_dead_scan_died_at() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store: crate::db::SharedStore =
            Arc::new(crate::db::Store::open(&dir.path().join("t.db")).expect("open"));
        store
            .write(|c| {
                c.execute(
                    "INSERT INTO reclaim_scans
                       (id, started_at, status, roots, dirs_walked, files_walked, bytes_seen,
                        current_path, current_phase)
                     VALUES ('S1', 1, 'running', '[]', 1087, 28209, 0,
                             '/Users/ethan/Library/Mobile Documents', 'readdir')",
                    [],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .expect("seed");

        reap_orphaned_scans(&store);

        let conn = store.read().expect("read");
        let (status, path, err): (String, Option<String>, String) = conn
            .query_row(
                "SELECT status, current_path, error FROM reclaim_scans WHERE id='S1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("row");
        assert_eq!(status, "interrupted");
        assert_eq!(path.as_deref(), Some("/Users/ethan/Library/Mobile Documents"));
        assert!(
            err.contains("readdir at /Users/ethan/Library/Mobile Documents"),
            "the error must name where it died, got: {err}"
        );
    }

    /// Provider roots are skipped by default, and a skip can be undone.
    ///
    /// A one-way ratchet would let the scan's coverage shrink silently over
    /// time, which is worse than the hang it is avoiding: a hang is visible.
    #[test]
    fn provider_roots_seed_as_skips_and_can_be_cleared() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store: crate::db::SharedStore =
            Arc::new(crate::db::Store::open(&dir.path().join("t.db")).expect("open"));

        let skips = load_skips(&store);
        let icloud = home_dir().join("Library/Mobile Documents");
        assert!(skips.contains(&icloud), "iCloud provider root must be skipped by default");

        // Seeding twice must not duplicate or resurrect a cleared row.
        let again = load_skips(&store);
        assert_eq!(skips.len(), again.len(), "seeding must be idempotent");

        let p = icloud.to_string_lossy().into_owned();
        store
            .write(move |c| {
                let n = c.execute("DELETE FROM reclaim_skipped WHERE path=?1", [&p])?;
                Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
            })
            .expect("delete");
        let conn = store.read().expect("read");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM reclaim_skipped", [], |r| r.get(0))
            .expect("count");
        assert_eq!(n as usize, skips.len() - 1, "clearing a skip must remove exactly one");
    }

    /// A learned skip records WHY, so a future reader can tell an intentional
    /// exemption from a filesystem that hung once during a transient outage.
    /// One stall records an observation; it does not create an exemption.
    ///
    /// The version without this shipped, and its first production run ended on
    /// `~/Downloads` after 48 quiet seconds and permanently excluded it. That
    /// directory answers readdir in 2 seconds with 318 entries — it was
    /// contention on a machine running ~50 agents, on a disk the scan itself
    /// was busy reading. A detector whose action is a silent permanent hole in
    /// the scan has to need more than one reading.
    #[test]
    fn one_stall_records_but_does_not_exempt_and_two_does() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store: crate::db::SharedStore =
            Arc::new(crate::db::Store::open(&dir.path().join("t.db")).expect("open"));
        let victim = "/Users/someone/Downloads";

        record_stalled_dir(&store, victim.into(), "quiet 300s".into());
        let after_one = load_skips(&store);
        assert!(
            !after_one.contains(&PathBuf::from(victim)),
            "one stall must NOT exempt a directory"
        );

        record_stalled_dir(&store, victim.into(), "quiet 300s".into());
        let after_two = load_skips(&store);
        assert!(
            after_two.contains(&PathBuf::from(victim)),
            "a second stall on the same path must take effect"
        );

        // Control: the row existed the whole time, so the first assertion is
        // about the SKIP rule and not about a missing record.
        let conn = store.read().expect("read");
        let hits: i64 = conn
            .query_row(
                "SELECT hits FROM reclaim_skipped WHERE path=?1",
                [victim],
                |r| r.get(0),
            )
            .expect("the observation must be recorded from the first stall");
        assert_eq!(hits, 2);
    }

    #[test]
    fn a_stalled_directory_is_recorded_with_its_reason_and_counts_repeats() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store: crate::db::SharedStore =
            Arc::new(crate::db::Store::open(&dir.path().join("t.db")).expect("open"));

        record_stalled_dir(&store, "/mnt/dead-nfs".into(), "readdir did not return after 45s".into());
        record_stalled_dir(&store, "/mnt/dead-nfs".into(), "readdir did not return after 45s".into());

        let conn = store.read().expect("read");
        let (reason, detail, hits): (String, String, i64) = conn
            .query_row(
                "SELECT reason, detail, hits FROM reclaim_skipped WHERE path='/mnt/dead-nfs'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("row");
        assert_eq!(reason, "stalled");
        assert!(detail.contains("45s"), "detail must carry the stall duration: {detail}");
        assert_eq!(hits, 2, "a repeat stall must increment rather than insert a second row");
    }
}
