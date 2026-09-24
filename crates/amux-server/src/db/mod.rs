//! SQLite store: WAL mode, single-writer task, read pool, migrations,
//! global revision counter (RR-0019, Invariants 35/36).
//!
//! Concurrency design (plan §SQLite concurrency design):
//! - One dedicated writer thread owns the only write connection. Mutations
//!   arrive over an mpsc channel as closures; the writer applies each inside
//!   a transaction that ALSO bumps the global revision when the mutation
//!   reports itself as a real change. Python's GIL serialized writes by
//!   accident; this serializes them by construction, so `SQLITE_BUSY` cannot
//!   happen under load.
//! - Readers come from an r2d2 pool of read-only connections with a 5s busy
//!   timeout.
//! - The revision lives in `_amux_rev` (single row) and is returned from
//!   every mutation so SSE/delta-sync can publish revisioned StateEvents
//!   (Invariant 35).

pub mod advance;
pub mod artifact_store;
pub mod attempts;
pub mod board_store;
pub mod commands;
pub mod harness_store;
pub mod interactions;
pub mod memories;
pub mod migrate;
pub mod queries;
pub mod replay;
pub mod task_graph_store;
pub mod telegram;
pub mod throughput_store;
pub mod trace_store;
pub mod verification_store;
pub mod workflow_store;

use amux_core::revision::{MutationKind, StateEvent, StateRevision};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;

pub type ReadPool = r2d2::Pool<SqliteConnectionManager>;

pub(crate) enum ProjectionRead {
    Dedicated(Connection),
    Pooled(r2d2::PooledConnection<SqliteConnectionManager>),
}

impl std::ops::Deref for ProjectionRead {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Dedicated(conn) => conn,
            Self::Pooled(conn) => conn,
        }
    }
}

/// What a write closure reports back: did it change anything, and what
/// StateEvents should be published if it did. `applied: false` writes do NOT
/// bump the revision (Invariant 37: no-op mutations must be visible as
/// no-ops, not disguised as changes).
pub struct WriteOutcome {
    pub applied: bool,
    pub events: Vec<PendingEvent>,
}

/// A StateEvent minus the revision, which the writer assigns at commit time
/// so event order and revision order can never disagree.
pub struct PendingEvent {
    pub entity_type: amux_core::revision::EntityType,
    pub entity_id: String,
    pub mutation: MutationKind,
    /// RR-0111a: the POST-MUTATION snapshot of the entity row, journaled in
    /// the same transaction as the mutation so state can be replayed from
    /// events alone (plan Invariant 24, EventPayload::Inline). The row is in
    /// the writer's hand when the event is built, so a snapshot costs one
    /// serialization, never a re-read.
    ///
    /// `None` is honest, not lazy: it means this event records THAT the
    /// entity changed, without the state it changed into. Replay
    /// (`db::replay`) reports such entities under `pre_payload_horizon`
    /// instead of pretending an older snapshot is current. Worker and board
    /// (task) mutations populate this; other sites may stay `None` until
    /// their entities need replay.
    pub payload: Option<serde_json::Value>,
}

type WriteFn = Box<dyn FnOnce(&Connection) -> rusqlite::Result<WriteOutcome> + Send>;

/// Only these fixed maintenance operations may use the serialized writer
/// outside a transaction. Pooled readers stay query-only.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Maintenance {
    Checkpoint,
    Vacuum,
}

enum WriteWork {
    Mutation(WriteFn),
    Maintenance(Maintenance),
}

struct WriteRequest {
    work: WriteWork,
    origin: &'static str,
    /// AMUX-4781: WHERE the write was issued, not just which function.
    /// `origin` is `type_name::<F>()`, and every closure inside one function
    /// renders identically, so `Runtime::tick_once`'s twelve `write_async`
    /// call sites all logged one indistinguishable string. That is what made
    /// the top writer hold unattributable.
    site: &'static std::panic::Location<'static>,
    queued_at: std::time::Instant,
    interaction_id: Option<String>,
    reply: mpsc::Sender<rusqlite::Result<WriteReply>>,
}

pub struct WriteReply {
    pub applied: bool,
    pub rev: StateRevision,
    pub events: Vec<StateEvent>,
}

/// Handle to the store: cheap to clone, shared across the router and
/// background jobs.
#[derive(Clone)]
pub struct Store {
    write_tx: mpsc::Sender<WriteRequest>,
    /// `pub(crate)` so a test outside this module can put the pool under real
    /// saturation. The property "a request path takes no blocking acquire" is
    /// only testable by holding every connection, and the paths that must hold
    /// it (`api::policy::enforce`) live in other modules.
    pub(crate) read_pool: ReadPool,
    /// AF-937: held only for its lifetime -- an RAII guard, not state. An
    /// flock on a sidecar file next to `db_path`, acquired in
    /// [`claim_sole_writer`] at open time. The OS releases it automatically
    /// when every clone of this `Arc` (and so the underlying `File`) drops,
    /// including on a crash or SIGKILL, so a stale lock cannot outlive the
    /// process that took it. `None` means either another live process
    /// already held it (a WARN was logged) or the probe itself could not run
    /// (never treated as contention). Read only by the `#[cfg(test)]`
    /// accessor below; production code never inspects it, only outlives it.
    #[allow(dead_code, reason = "RAII guard: outlived, not read, outside tests")]
    pub(crate) writer_lock: Option<Arc<std::fs::File>>,
    db_path: Arc<std::path::PathBuf>,
    pub(crate) health_probe: Arc<tokio::sync::Semaphore>,
    pub(crate) health_probe_started: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) health_probe_last_success: Arc<std::sync::atomic::AtomicU64>,
    /// Writes submitted to the writer thread and not yet answered (AMUX-4744).
    ///
    /// The writer is ONE thread behind an unbounded channel and `write_correlated`
    /// waits on `recv()` with no timeout, so a slow write delays every write
    /// behind it by an unbounded amount while reads are untouched. That
    /// asymmetry is invisible today: nothing times the wait and nothing counts
    /// the queue, so an 80s POST produces no log line at all.
    pub(crate) write_inflight: Arc<std::sync::atomic::AtomicUsize>,
    /// Longest write wait observed since start, in milliseconds. A gauge that
    /// only rises, so a stall that has already ended is still reportable.
    pub(crate) write_wait_max_ms: Arc<std::sync::atomic::AtomicU64>,
    /// Longest wait for a `spawn_blocking` thread, milliseconds, rising only.
    /// See `record_blocking_dispatch`: this is the one number on the write path
    /// that is not measured from a thread we already hold.
    pub(crate) blocking_dispatch_max_ms: Arc<std::sync::atomic::AtomicU64>,
    /// Broadcast of committed StateEvents for SSE fan-out.
    events_tx: tokio::sync::broadcast::Sender<StateEvent>,
}

/// A write wait past this is reported. A healthy write on this box is
/// sub-millisecond; the stalls on AMUX-4744 were 62s to 84s. One second is far
/// enough above normal contention to stay quiet and far enough below the
/// observed failures to catch all of them.
pub(crate) const WRITE_WAIT_WARN_MS: u64 = 1_000;

/// Record how long a `spawn_blocking` task waited to be given a thread, and say
/// so when it is long enough to be the reason a request is hanging.
///
/// Getting a blocking thread is normally instant. A large value here means the
/// blocking pool is the bottleneck, which is a DIFFERENT fault from a slow
/// query or a busy writer and has a different fix, so it gets its own verdict
/// rather than being folded into `writer_slow`.
pub(crate) fn record_blocking_dispatch(
    gauge: &std::sync::atomic::AtomicU64,
    waited: std::time::Duration,
) {
    let ms = waited.as_millis() as u64;
    gauge.fetch_max(ms, std::sync::atomic::Ordering::Relaxed);
    if ms >= WRITE_WAIT_WARN_MS {
        tracing::warn!(
            target: "store",
            verdict = "blocking_pool_saturated",
            waited_ms = ms,
            measured = true,
            n_considered = 1,
            "a db task waited for a blocking thread; the pool, not the query, is the delay"
        );
    }
}

/// Sidecar to `db_path` recording which live process holds it open for
/// writing. A `.holder-lock` suffix, distinct from SQLite's own `-wal`,
/// `-shm` and `-journal` files, so nothing that globs those is affected.
fn holder_lock_path(db_path: &Path) -> std::path::PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push(".holder-lock");
    std::path::PathBuf::from(s)
}

/// What a contended lock probe learned about the process that beat us to it.
/// `pid`/`port` are `None` only when the lock file's content could not be
/// parsed (an old-format or corrupt write) -- never a stand-in for "nobody
/// holds it", which is the `Ok` case in [`claim_sole_writer`]'s caller.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ExistingWriter {
    pub pid: Option<u32>,
    pub port: Option<u16>,
}

pub(crate) enum WriterLockOutcome {
    /// We hold the lock. Keep the `File` alive for the store's lifetime, or
    /// the OS releases it immediately.
    Acquired(std::fs::File),
    /// Another live OS process already holds it.
    HeldByOther(Option<ExistingWriter>),
    /// The recorded holder IS this process (a same-process reopen of the
    /// same `db_path` while the first handle is still alive -- e.g. a test
    /// simulating a restart without an actual exec). flock() is scoped to
    /// the open file description, not the process, so a second open()
    /// within the same process still contends; that is not a second OS
    /// process and must not be reported as one.
    HeldBySelf,
}

/// Claim sole-writer status on `db_path` via an advisory `flock` on a sidecar
/// file, or report who already holds it.
///
/// AF-937 / AEAB-11 recurrence: a manual `amux-server-rs` run with no
/// `AMUX_RS_PORT` falls onto `DEFAULT_PORT` and the default `AMUX_HOME`,
/// landing on the exact `db_path` the real server already has open -- and
/// nothing said so for 13 hours (both processes reported healthy the whole
/// time; SQLite's own WAL locking serializes the writes correctly, so this
/// was never about corruption, only a silent second writer). This makes that
/// coexistence explicit and prevents a second scheduler from acting on the
/// same workers. Refuse startup if exclusive ownership cannot be established;
/// read-only clients must use the running server rather than start another driver.
fn claim_sole_writer(db_path: &Path) -> WriterLockOutcome {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::io::AsRawFd;

    let lock_path = holder_lock_path(db_path);
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        // Explicit: opening must NOT clear a prior holder's content -- the
        // contended branch below reads it before this fn's caller ever
        // decides whether to overwrite it (only the lock-winning branch
        // truncates, deliberately, via `set_len(0)`).
        .truncate(false)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(_) => return WriterLockOutcome::HeldByOther(None),
    };

    // SAFETY: `file` is a valid, open fd for the lifetime of this call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        let mut f = &file;
        let _ = f.set_len(0);
        let _ = f.seek(SeekFrom::Start(0));
        let payload =
            serde_json::json!({"pid": std::process::id(), "port": crate::config::canonical_port()});
        let _ = write!(f, "{payload}");
        let _ = f.flush();
        return WriterLockOutcome::Acquired(file);
    }

    let mut buf = String::new();
    let mut f = &file;
    let _ = f.read_to_string(&mut buf);
    let existing = serde_json::from_str::<serde_json::Value>(&buf)
        .ok()
        .map(|v| ExistingWriter {
            pid: v
                .get("pid")
                .and_then(serde_json::Value::as_u64)
                .map(|x| x as u32),
            port: v
                .get("port")
                .and_then(serde_json::Value::as_u64)
                .map(|x| x as u16),
        });
    if existing.as_ref().and_then(|e| e.pid) == Some(std::process::id()) {
        WriterLockOutcome::HeldBySelf
    } else {
        WriterLockOutcome::HeldByOther(existing)
    }
}

/// Tokio worker threads, which is also what `available_parallelism` returns.
pub(crate) fn worker_threads() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4)
}

/// Read connections, DECOUPLED from the tokio worker count (AMUX-4955).
///
/// This was exactly `available_parallelism`, which is ALSO tokio's default
/// worker count. The comment on the builder has said since AF-640 that such a
/// pool "can pin every worker at once" and that the result "is
/// self-sustaining" — and it is, because a worker blocked in `read()` waiting
/// for a connection cannot release the connection it is waiting behind.
///
/// Measured 2026-09-23 over ~57k blocking-poll samples: 824
/// `read_pool_slow_acquire` events, 97% of them with `idle=0` at 28/28, waits
/// up to 4.3s — the 5s `connection_timeout` is what bounds them, not demand.
/// With the pool strictly LARGER than the worker count, a tokio worker cannot
/// be made to wait for a read connection by other tokio workers, which removes
/// the self-sustaining half of that loop.
///
/// HEADROOM, NOT A GUARANTEE, and the difference is worth stating: `read_async`
/// runs on spawn_blocking threads that draw from this same pool, and there are
/// far more of those than workers. This bounds worker-to-worker starvation; it
/// does not bound the pool.
pub(crate) fn read_pool_size() -> u32 {
    std::env::var("AMUX_READ_POOL_SIZE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| worker_threads().saturating_mul(2).max(8))
}

impl Store {
    /// Open the store: apply migrations, start the writer thread, build the
    /// read pool.
    pub fn open(db_path: &Path) -> anyhow::Result<Store> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // AF-937: probe BEFORE opening anything else, so a warning is the
        // first thing this open logs if it fires, and so the probe never
        // depends on migrations or the writer thread having succeeded.
        let writer_lock = match claim_sole_writer(db_path) {
            WriterLockOutcome::Acquired(f) => Some(Arc::new(f)),
            WriterLockOutcome::HeldBySelf => None,
            WriterLockOutcome::HeldByOther(existing) => {
                tracing::warn!(
                    target: "store",
                    verdict = "concurrent_writer_detected",
                    db = %db_path.display(),
                    other_pid = existing.as_ref().and_then(|e| e.pid),
                    other_port = existing.as_ref().and_then(|e| e.port),
                    measured = true,
                    "another live process already has this database open for writing -- \
                     AEAB-11/AF-937: a manual amux-server-rs run with no AMUX_RS_PORT set \
                     falls onto the compiled-in default port and can silently collide with \
                     the real server on this exact db_path"
                );
                anyhow::bail!("database runtime ownership unavailable for {} (holder pid {:?}, port {:?}); use the existing server or a separate AMUX_HOME", db_path.display(), existing.as_ref().and_then(|e|e.pid), existing.as_ref().and_then(|e|e.port));
            }
        };
        // Migrations run on a dedicated connection before anything else may
        // touch the DB. Health returns 503 until `open` completes.
        let mut conn = Connection::open(db_path)?;
        configure_connection(&conn)?;
        migrate::apply_all_guarded(&mut conn, db_path)?;

        let (write_tx, write_rx) = mpsc::channel::<WriteRequest>();
        let (events_tx, _) = tokio::sync::broadcast::channel(4096);
        let events_for_writer = events_tx.clone();

        // The writer thread. Plain OS thread, not a tokio task: rusqlite is
        // synchronous and a blocked writer must never stall the async
        // runtime's worker pool.
        std::thread::Builder::new()
            .name("amux-writer".into())
            .spawn(move || writer_loop(conn, write_rx, events_for_writer))
            .expect("spawn writer thread");

        let manager = SqliteConnectionManager::file(db_path).with_init(|c| {
            configure_connection(c)?;
            // Readers never write; enforce it so a bug cannot sneak a write
            // past the single-writer discipline.
            c.pragma_update(None, "query_only", "ON")?;
            Ok(())
        });
        let read_pool = r2d2::Pool::builder()
            .max_size(read_pool_size())
            // FAIL FAST, because a blocked acquire pins a tokio worker (AF-640).
            //
            // r2d2's default is 30 SECONDS and it was never set, which is why
            // the 2026-09-08 outage produced rows at exactly 30032, 30100 and
            // 30104 ms: sixteen 500s in 22 minutes, every one a caller that
            // waited half a minute to be told no.
            //
            // WHY WAITING IS WORSE THAN FAILING HERE. `read()` is synchronous
            // and — when this was written — there was no `read_async` to match
            // `write_async`. There is one now, used in five modules, so the
            // claim below is narrower than it reads: it holds for the ~440
            // BLOCKING `read()` sites, not for the codebase (AMUX-4955). So every one of the ~440 `read()` call
            // sites blocks its thread for the whole acquire. The pool's
            // max_size is `available_parallelism`, which is ALSO tokio's default
            // worker count, so a saturated pool can pin every worker at once
            // and each one holds for 30s. That is self-sustaining, which is why
            // it lasted 22 minutes and recurred five more times that day.
            //
            // A healthy acquire is microseconds. Anything approaching seconds
            // means the pool is already saturated, and a caller that waits
            // longer does not make a connection appear; it just holds a worker
            // that could be shedding load. Five seconds keeps a generous margin
            // over any legitimate contention while cutting the pin by 6x.
            .connection_timeout(std::time::Duration::from_secs(5))
            // AND THE SAME KNOB GOVERNS POOL STARTUP, which the paragraph above
            // did not account for (AMUX-4739).
            //
            // `Pool::build` calls `wait_for_initialization`, which waits for
            // `min_idle.unwrap_or(max_size)` connections to exist and bounds
            // that wait by THIS timeout (r2d2 0.8.10 lib.rs:391-395). min_idle
            // was unset, so opening a store waited for `available_parallelism`
            // connections. Cutting 30s -> 5s to bound acquisition therefore made
            // startup six times more likely to fail outright, and it fails as
            // `Store::open` returning Err("timed out waiting for connection"),
            // which reads as a broken database rather than a busy one.
            //
            // MEASURED, 2026-09-17: a full `cargo test -p amux-server --lib` on
            // an unmodified origin/main produced 39 of these and 43 failed tests
            // across modules that share nothing but this call. A second run
            // failed a DIFFERENT 29, which is what made it look like unrelated
            // flakiness for as long as it did.
            //
            // One connection is enough to serve the first reader; the pool still
            // grows to max_size on demand. This changes what `open` WAITS FOR,
            // not how many connections a loaded server ends up with.
            .min_idle(Some(1))
            .build(manager)?;

        Ok(Store {
            write_tx,
            read_pool,
            writer_lock,
            db_path: Arc::new(db_path.to_path_buf()),
            health_probe: Arc::new(tokio::sync::Semaphore::new(1)),
            health_probe_started: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            health_probe_last_success: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            write_inflight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            write_wait_max_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            blocking_dispatch_max_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            events_tx,
        })
    }

    /// AF-937: whether this instance won the sidecar `flock` at open time.
    /// `false` covers both "another live process already held it" (a WARN
    /// was logged) and "this process already held it" (a same-process
    /// reopen, silently benign) -- callers that need to tell those apart
    /// read the log, not this method.
    #[cfg(test)]
    pub(crate) fn holds_writer_lock(&self) -> bool {
        self.writer_lock.is_some()
    }

    /// Run a mutation on the writer thread and wait for commit. Returns the
    /// revision assigned to this write (unchanged if the write was a no-op).
    #[track_caller]
    pub fn write<F>(&self, f: F) -> anyhow::Result<WriteReply>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<WriteOutcome> + Send + 'static,
    {
        self.write_correlated(
            f,
            interactions::current_id(),
            std::panic::Location::caller(),
        )
    }

    fn write_correlated<F>(
        &self,
        f: F,
        interaction_id: Option<String>,
        site: &'static std::panic::Location<'static>,
    ) -> anyhow::Result<WriteReply>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<WriteOutcome> + Send + 'static,
    {
        self.submit_write(
            WriteWork::Mutation(Box::new(f)),
            std::any::type_name::<F>(),
            site,
            interaction_id,
        )
    }

    fn submit_write(
        &self,
        work: WriteWork,
        origin: &'static str,
        site: &'static std::panic::Location<'static>,
        interaction_id: Option<String>,
    ) -> anyhow::Result<WriteReply> {
        use std::sync::atomic::Ordering;
        let (reply_tx, reply_rx) = mpsc::channel();
        self.write_inflight.fetch_add(1, Ordering::Relaxed);
        let started = std::time::Instant::now();
        let sent = self
            .write_tx
            .send(WriteRequest {
                work,
                origin,
                site,
                queued_at: std::time::Instant::now(),
                interaction_id,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("writer thread is gone"));
        let out = match sent {
            Ok(()) => reply_rx
                .recv()
                .map_err(anyhow::Error::from)
                .and_then(|r| r.map_err(anyhow::Error::from)),
            Err(e) => Err(e),
        };
        self.write_inflight.fetch_sub(1, Ordering::Relaxed);

        // THE GAUGE, NOT A SECOND WARNING (AMUX-4744). `recv()` above has no
        // timeout, so an 80-second write used to produce exactly as much output
        // as a fast one: none.
        //
        // The LOG half of that gap is already closed, by `writer_slow` in
        // `writer_loop` (1f0cca3e, codex-board-execution-contract), which lands
        // the same minute as this and is strictly better placed: it carries
        // `origin`, the type name of the blocking closure, so it NAMES the slow
        // mutation instead of only reporting that something was slow. A second
        // warn here would fire on the same event with less information, and two
        // lines per stall is how a verdict becomes noise a sweep learns to skip.
        //
        // What that warn cannot answer is "is it happening RIGHT NOW, and how
        // deep", because a log line is a record of something already over.
        // These two atomics are readable on /api/health at any instant, which
        // is where someone looks while a POST is hanging in front of them.
        let waited_ms = started.elapsed().as_millis() as u64;
        self.write_wait_max_ms
            .fetch_max(waited_ms, Ordering::Relaxed);
        out
    }

    /// Async wrapper: parks the wait on the blocking pool so an API handler
    /// can await a write without pinning a runtime worker.
    /// NOT an `async fn` on purpose: `#[track_caller]` does not propagate
    /// through the generated future, and the call site is the whole point
    /// (AMUX-4781). Returning a `'static` future keeps every `.await` caller
    /// source-compatible.
    #[track_caller]
    pub fn write_async<F>(
        &self,
        f: F,
    ) -> impl std::future::Future<Output = anyhow::Result<WriteReply>> + Send
    where
        F: FnOnce(&Connection) -> rusqlite::Result<WriteOutcome> + Send + 'static,
    {
        let this = self.clone();
        let interaction_id = interactions::current_id();
        let dispatch = self.blocking_dispatch_max_ms.clone();
        let queued = std::time::Instant::now();
        let site = std::panic::Location::caller();
        async move {
            tokio::task::spawn_blocking(move || {
                // TIME SPENT WAITING FOR A BLOCKING THREAD, which every other
                // instrument on this path is structurally blind to (AMUX-4744).
                //
                // `writer_slow` and `write_wait_max_ms` are both measured INSIDE
                // `write_correlated`, which by then is already running on a blocking
                // thread. Neither can see the wait to GET that thread. So if the
                // blocking pool is saturated, a request stalls for a minute and
                // every existing verdict stays silent and truthful.
                //
                // That makes this the discriminator rather than another counter:
                // a 90s request with a small `queued_ms` and a large value here
                // means the writer was never the problem.
                record_blocking_dispatch(&dispatch, queued.elapsed());
                this.write_correlated(f, interaction_id, site)
            })
            .await?
        }
    }

    /// Run a read WITHOUT pinning a runtime worker (AF-640 / AMUX-4744).
    ///
    /// THE MISSING HALF THAT AF-640 NAMES. The comment on the read pool's
    /// `connection_timeout` says it plainly: "`read()` is synchronous and there
    /// is no `read_async` to match `write_async`, whose own doc says it exists
    /// so a handler can await a write without pinning a runtime worker. So
    /// every one of the ~440 `read()` call sites blocks its thread for the
    /// whole acquire."
    ///
    /// That is self-sustaining, and the same comment says why: the pool's
    /// `max_size` is `available_parallelism`, which is ALSO tokio's default
    /// worker count, so a saturated pool can pin every worker at once. The
    /// 2026-09-08 outage ran 22 minutes and recurred five times that day. The
    /// timeout was cut from 30s to 5s, which bounds the pin; it does not remove
    /// it. This removes it, for callers that can await.
    ///
    /// The acquire happens on the BLOCKING pool, where blocking is what the
    /// threads are for, so a saturated read pool costs latency instead of
    /// costing the runtime its ability to poll anything else.
    ///
    /// ADDITIVE ON PURPOSE. `read()` keeps working and keeps its slow-acquire
    /// warning; converting ~440 call sites in one change is not a reviewable
    /// diff and most of them are on background jobs that already run on the
    /// maintenance runtime (AMUX-4225) where a pinned thread costs far less.
    /// The callers worth moving are the ones on the request path.
    pub async fn read_async<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&Connection) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let this = self.clone();
        let dispatch = self.blocking_dispatch_max_ms.clone();
        let queued = std::time::Instant::now();
        tokio::task::spawn_blocking(move || {
            // Same blind spot as `write_async`: everything below this line runs
            // on a blocking thread, so nothing below can measure the wait to be
            // GIVEN one. Reads only started paying this cost when `read_async`
            // was introduced, so if the pool is the bottleneck, that change
            // moved reads into the same queue as writes rather than out of it.
            record_blocking_dispatch(&dispatch, queued.elapsed());
            let conn = this.read()?;
            f(&conn)
        })
        .await?
    }

    /// A read acquire this slow means the pool is already saturated. Well under
    /// `connection_timeout` so the warning arrives BEFORE the failures do, which
    /// is the difference between a signal and a post-mortem.
    const SLOW_ACQUIRE: std::time::Duration = std::time::Duration::from_millis(250);

    /// Submit fixed SQLite maintenance to the same writer as mutations. It
    /// cannot run in an Immediate transaction and must never relax query_only
    /// on a pooled reader. No domain revision or event is fabricated.
    pub(crate) async fn maintenance_async(&self, operation: Maintenance) -> anyhow::Result<()> {
        let store = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            store.submit_write(
                WriteWork::Maintenance(operation),
                "storage-maintenance",
                std::panic::Location::caller(),
                None,
            )
        }).await?;
        match &result {
            Ok(_) => tracing::info!(?operation, measured=true, n_considered=1,
                verdict="storage_maintenance_completed", "serialized maintenance completed outside a transaction"),
            Err(error) => tracing::warn!(?operation, %error, measured=true, n_considered=1,
                verdict="storage_maintenance_failed", "maintenance failed; readers remain query-only"),
        }
        result.map(|_| ())
    }

    /// Borrow a read-only connection from the pool.
    ///
    /// SAYS WHEN IT IS SLOW, because the only signal the 2026-09-08 exhaustion
    /// left was a 30-second 500 with `timed out waiting for connection` and no
    /// pool state beside it (AF-640). "How many connections were out, and how
    /// many were idle" is the first question anyone asks and nothing recorded
    /// it, so the cause had to be reconstructed from the source afterwards.
    ///
    /// Silent on the happy path: a healthy acquire is microseconds, so the
    /// threshold below is never reached in normal operation and this stays off
    /// a hot path rather than logging 200k times a day.
    pub fn read(&self) -> anyhow::Result<r2d2::PooledConnection<SqliteConnectionManager>> {
        let t0 = std::time::Instant::now();
        let got = self.read_pool.get();
        let waited = t0.elapsed();
        match got {
            Ok(conn) => {
                if waited >= Self::SLOW_ACQUIRE {
                    let st = self.read_pool.state();
                    tracing::warn!(
                        verdict = "read_pool_slow_acquire",
                        waited_ms = waited.as_millis() as u64,
                        connections = st.connections,
                        idle = st.idle_connections,
                        max_size = self.read_pool.max_size(),
                        "read pool acquire was slow; the pool is saturated and every waiter                          is pinning a thread (AF-640)"
                    );
                }
                Ok(conn)
            }
            Err(e) => {
                let st = self.read_pool.state();
                tracing::warn!(
                    verdict = "read_pool_exhausted",
                    waited_ms = waited.as_millis() as u64,
                    connections = st.connections,
                    idle = st.idle_connections,
                    max_size = self.read_pool.max_size(),
                    error = %e,
                    "read pool acquire FAILED; callers are getting 500s (AF-640)"
                );
                Err(e.into())
            }
        }
    }

    /// Health must report pool exhaustion without waiting behind fleet probes.
    pub fn try_read(&self) -> Option<r2d2::PooledConnection<SqliteConnectionManager>> {
        self.read_pool.try_get()
    }

    /// Open a read-only connection outside the request pool for a bounded,
    /// heavyweight projection.
    ///
    /// The sessions projection deliberately shells out while it assembles its
    /// answer. Even with one build in flight, lending that work one of the
    /// request pool's connections makes unrelated, cheap API reads wait behind
    /// tmux/git. A dedicated reader keeps the pool available while preserving
    /// SQLite's WAL snapshot semantics; callers must still single-flight and
    /// bound their external work.
    pub(crate) fn dedicated_read(&self) -> anyhow::Result<ProjectionRead> {
        // SQLite gives each `:memory:` connection an independent database, so
        // a new connection would silently see an empty store. Preserve the
        // previous pooled behavior for that test/development configuration.
        if self.db_path.as_path() == Path::new(":memory:") {
            return Ok(ProjectionRead::Pooled(self.read()?));
        }
        let conn = Connection::open_with_flags(
            self.db_path.as_ref(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "query_only", "ON")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(ProjectionRead::Dedicated(conn))
    }

    /// Current global revision.
    pub fn current_rev(&self) -> anyhow::Result<StateRevision> {
        let conn = self.read()?;
        let rev: u64 =
            conn.query_row("SELECT rev FROM _amux_rev WHERE id = 1", [], |r| r.get(0))?;
        Ok(StateRevision(rev))
    }

    /// Subscribe to committed StateEvents (SSE fan-out).
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<StateEvent> {
        self.events_tx.subscribe()
    }

    /// StateEvents since a revision, for delta sync (RR-0024). Returns
    /// (events, full_sync_required): when the requested window is no longer
    /// in the event journal the client must full-sync rather than trust a
    /// silently incomplete delta (Invariant 40 — an omission must announce
    /// itself).
    pub fn events_since(
        &self,
        since: StateRevision,
        limit: usize,
    ) -> anyhow::Result<(Vec<StateEvent>, bool)> {
        let conn = self.read()?;
        let oldest: Option<u64> = conn
            .query_row("SELECT MIN(rev) FROM _amux_state_events", [], |r| r.get(0))
            .unwrap_or(None);
        // Gap check: if the journal's oldest retained event is newer than
        // since+1 and the client is behind that, the delta would be missing
        // events it has no way to detect.
        if let Some(oldest) = oldest {
            if since.0 + 1 < oldest {
                return Ok((vec![], true));
            }
        }
        let mut stmt = conn.prepare(
            "SELECT rev, entity_type, entity_id, mutation, at FROM _amux_state_events
             WHERE rev > ?1 ORDER BY rev ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![since.0, limit as i64], |r| {
            let rev: u64 = r.get(0)?;
            let entity_type: String = r.get(1)?;
            let entity_id: String = r.get(2)?;
            let mutation: String = r.get(3)?;
            let at: String = r.get(4)?;
            Ok((rev, entity_type, entity_id, mutation, at))
        })?;
        let mut events = Vec::new();
        for row in rows {
            let (rev, entity_type, entity_id, mutation, at) = row?;
            events.push(StateEvent {
                rev: StateRevision(rev),
                entity_type: parse_entity_type(&entity_type),
                entity_id,
                mutation: serde_json::from_str(&mutation).unwrap_or(MutationKind::Updated),
                at: at.parse().unwrap_or_default(),
            });
        }
        Ok((events, false))
    }
}

/// Parse a stored `entity_type` column back into the enum: the bare tag
/// ("worker", "fleet_progress" — the current storage format), with tolerance
/// for the legacy adjacently-tagged object ({"kind":"worker"} /
/// {"kind":"other","data":"x"}) that rows written before the bare-tag fix
/// still carry. Unknown tags land in `Other(tag)` — the open-enum contract.
/// (The previous reader wrapped the raw value in quotes and fed it to serde,
/// which CANNOT parse an adjacently-tagged unit variant from a JSON string —
/// so every event round-tripped as Other(...), for typed variants too.)
fn parse_entity_type(raw: &str) -> amux_core::revision::EntityType {
    use amux_core::revision::EntityType;
    if raw.starts_with('{') {
        if let Ok(t) = serde_json::from_str::<EntityType>(raw) {
            return t;
        }
    }
    serde_json::from_str::<EntityType>(&format!("{{\"kind\":\"{raw}\"}}"))
        .unwrap_or_else(|_| EntityType::Other(raw.to_string()))
}

/// `cache_size` AND `mmap_size` ARE DELIBERATELY LEFT AT SQLITE'S DEFAULTS.
///
/// They are absent on purpose, not by oversight, and the measurement is written
/// down here so the next person to notice "2MB of page cache against a 4.6GB
/// database" does not re-derive it (AMUX-4842 — which I filed myself, arguing
/// the cache should be raised, and then disproved).
///
/// RAISING IT IS SLOWER ON THIS BOX. Measured 2026-09-19 against the live 4.62GB
/// database, 600 random point lookups per round over `_amux_request_log`
/// (3.45M rows) and `token_ledger`, 16 rounds, arms INTERLEAVED with the order
/// reshuffled each round and an identical key sequence per round:
///   cache_size=-2000    (2MB)    median 3.8ms   p25 3.5  p75 4.0
///   cache_size=-65536   (64MB)   median 4.3ms   p25 4.2  p75 5.0   +14.7%
///   cache_size=-262144  (256MB)  median 4.2ms   p25 4.2  p75 4.3   +13.0%
///
/// WHY, and the arithmetic is the whole argument: 3.8ms for 600 lookups is
/// 6.3us each. An NVMe read is ~100us, so those pages were already in memory.
/// The host has 103GB of RAM with 54GB free or reclaimable against a 4.62GB
/// file, so the OS page cache holds the entire database. SQLite's own cache
/// therefore saves no I/O at all and can only add hash and LRU bookkeeping on
/// top of a cache that already has the page. That is the cost the numbers show.
///
/// AND THE FIRST MEASUREMENT SAID THE OPPOSITE, which is why the method is
/// recorded and not just the result. A block-ordered A-B-A run (2MB, then
/// 512MB, then 2MB again) showed 48ms -> 36ms -> 48ms and looked conclusive:
/// the control returned to baseline, so OS warming appeared to be ruled out.
/// It was not. A-B-A only controls for MONOTONIC drift, and this box compiles
/// and tests continuously, so a quieter middle arm produces exactly that shape.
/// Interleaving the arms is what a noisy host actually requires, and the effect
/// disappeared and then reversed once it was applied.
///
/// `mmap_size` stays 0 for a different reason, a risk one rather than a
/// measured one: with mmap, an I/O error reaches the process as SIGBUS instead
/// of a return code, so a read fault becomes a crash of the fleet's control
/// plane rather than a handled error. There is no measured benefit here to pay
/// for that, since the OS already holds the file.
///
/// What would change this: a host where the database no longer fits in RAM, or
/// a working set that outgrows the page cache. Re-measure with interleaved arms
/// before changing either value.
fn configure_connection(c: &Connection) -> rusqlite::Result<()> {
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "synchronous", "NORMAL")?;
    c.pragma_update(None, "foreign_keys", "ON")?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

fn writer_loop(
    conn: Connection,
    rx: mpsc::Receiver<WriteRequest>,
    events_tx: tokio::sync::broadcast::Sender<StateEvent>,
) {
    while let Ok(req) = rx.recv() {
        let queued_ms = req.queued_at.elapsed().as_millis() as u64;
        let started = std::time::Instant::now();
        // Reset first: a mutation that fails before committing must not report
        // the PREVIOUS write's commit time as its own.
        LAST_COMMIT_MS.store(0, std::sync::atomic::Ordering::Relaxed);
        LAST_BEGIN_MS.store(0, std::sync::atomic::Ordering::Relaxed);
        // A panicking caller must not kill the sole writer and strand every
        // later mutation. The transaction guard rolls back during unwinding.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            match req.work {
                WriteWork::Mutation(work) => apply_write(&conn, work, &events_tx, req.interaction_id.as_deref()),
                WriteWork::Maintenance(operation) => apply_maintenance(&conn, operation),
            }
        }))
        .unwrap_or_else(|_| {
            tracing::error!(target: "store", verdict = "writer_mutation_panicked",
                "mutation panicked; transaction rolled back, writer remains available");
            Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                std::io::Error::other("writer mutation panicked; transaction rolled back"),
            )))
        });
        let work_ms = started.elapsed().as_millis() as u64;
        // SPLIT THE HOLD (AMUX-4830, corrected by AMUX-4837). This comment used
        // to assert "on this box it is the commit". THE MEASUREMENT SAYS THE
        // OPPOSITE and the wrong sentence is what kept AMUX-4781's chain aimed
        // at the commit: across 130 writer_slow lines carrying the split, all of
        // them ok=true, `commit_ms` was 3.2% of `work_ms` and SUB-MILLISECOND on
        // 118 of 130. The commit is free; the hold is everything before it.
        //
        // So the split goes one level further. `stmt_ms` was derived as
        // everything-that-is-not-commit, which lumps acquiring the write lock in
        // with running the statements. Those need separating, because the
        // cheapest site in the fleet refutes the statement explanation on its
        // own: api/interactions.rs does ONE indexed SELECT plus ONE INSERT and
        // returns no events, so it never touches the `_amux_state_events` loop,
        // and it still averaged 466ms with a 0ms commit. One indexed lookup and
        // one insert are not 466ms of statement execution.
        //
        // `begin_ms` is the candidate that fits the rest of the evidence:
        // BEGIN IMMEDIATE takes the write lock, and SQLite's busy handler backs
        // off in 1/2/5/10/25/50/100ms steps, which quantises a contended
        // acquisition into exactly the site-independent floor observed here.
        // It also explains why the number does not track host load (measured
        // r = -0.002 against load1 over the same 130 lines): a backoff schedule
        // is a function of contention, not of CPU.
        let commit_ms = LAST_COMMIT_MS.load(std::sync::atomic::Ordering::Relaxed);
        let begin_ms = LAST_BEGIN_MS.load(std::sync::atomic::Ordering::Relaxed);
        let stmt_ms = work_ms.saturating_sub(commit_ms).saturating_sub(begin_ms);
        if work_ms >= 250 || queued_ms >= 1000 {
            // Function identity only; never record the request body or SQL
            // values. Separate the slow writer from callers waiting behind it.
            tracing::warn!(target: "store", verdict = "writer_slow", origin = req.origin,
                site = %req.site, queued_ms, work_ms, commit_ms, begin_ms, stmt_ms, ok = result.is_ok(),
                measured = true, n_considered = 1,
                "serialized write delayed; origin identifies the blocking mutation");
        }
        if let Err(error) = &result {
            tracing::warn!(target: "store", verdict = "writer_mutation_failed", %error,
                autocommit = conn.is_autocommit(), "mutation failed; no acknowledgement was issued");
        }
        // A dropped reply receiver just means the caller gave up waiting;
        // the write itself has already committed either way.
        let _ = req.reply.send(result);
    }
    // Channel closed = Store dropped = shutdown. Nothing to clean up: WAL
    // checkpoints on connection close.
}

/// How long the LAST `apply_write` spent inside `transaction.commit()`.
///
/// AMUX-4830: `work_ms` spans statements AND the commit, and on this box the
/// commit dominates. Thirteen writer sites doing completely different work
/// clustered between 589ms and 2189ms (coefficient of variation 0.32) — a
/// single small insert at `api/interactions.rs:101` cost 589ms, which cannot be
/// statement time. That floor was only visible by comparing sites against each
/// other; splitting it out makes it readable in one line.
///
/// A static counter rather than a thread-local: the writer is a single thread
/// (`writer_loop` owns the connection) so both are exact, but a thread-local is
/// invisible to a test, which reads its OWN thread's copy and sees 0 forever.
/// My first version of this was a thread-local and both of its mutations passed
/// green, which is how I found out.
static LAST_COMMIT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Milliseconds spent acquiring the write lock in `BEGIN IMMEDIATE`, published
/// the same way and for the same reason as [`LAST_COMMIT_MS`]: an atomic rather
/// than a thread-local, because `writer_loop` owns the connection and a
/// thread-local reads 0 forever from a test's own thread.
static LAST_BEGIN_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn apply_maintenance(conn: &Connection, operation: Maintenance) -> rusqlite::Result<WriteReply> {
    // Checkpoint reports contention in its result row, not as a SQL error.
    let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    if busy != 0 {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("checkpoint deferred by an active reader; maintenance not completed".into()),
        ));
    }
    if matches!(operation, Maintenance::Vacuum) {
        conn.execute_batch("VACUUM;")?;
    }
    let rev = conn.query_row("SELECT rev FROM _amux_rev WHERE id=1", [], |row| row.get(0))?;
    Ok(WriteReply { applied:false, rev:StateRevision(rev), events:vec![] })
}

fn apply_write(
    conn: &Connection,
    work: WriteFn,
    events_tx: &tokio::sync::broadcast::Sender<StateEvent>,
    interaction_id: Option<&str>,
) -> rusqlite::Result<WriteReply> {
    // Roll back EVERY failure path, including revision/event writes, failed
    // COMMIT and unwinding. A bare BEGIN left the connection in a transaction
    // after those errors, making all later mutations fail until restart.
    // STAMPED AROUND THE BEGIN ITSELF (AMUX-4837), because this is where the
    // busy handler sleeps. Stamping after `work` would fold the lock wait back
    // into statement time, which is the conflation this split exists to end.
    let begin_started = std::time::Instant::now();
    let transaction =
        rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    LAST_BEGIN_MS.store(
        begin_started.elapsed().as_millis() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
    let outcome = work(&transaction)?;
    let mut committed_events = Vec::new();
    let rev = if outcome.applied {
        // Bump the global revision once per applied transaction; every event
        // from this transaction shares the revision, which is what makes
        // "give me everything after rev N" exact.
        conn.execute("UPDATE _amux_rev SET rev = rev + 1 WHERE id = 1", [])?;
        let rev: u64 =
            conn.query_row("SELECT rev FROM _amux_rev WHERE id = 1", [], |r| r.get(0))?;
        let now = chrono::Utc::now();
        if let Some(id) = interaction_id {
            conn.execute(
                "UPDATE _amux_interactions SET applied_writes=applied_writes+1,
                unjournaled_writes=unjournaled_writes+?2, updated_at=?3 WHERE id=?1",
                rusqlite::params![
                    id,
                    i64::from(outcome.events.is_empty()),
                    now.timestamp_millis()
                ],
            )?;
        }
        for ev in outcome.events {
            // The COLUMN stores the BARE tag ("worker", "task",
            // "fleet_progress"), never serde's adjacently-tagged object.
            // Three consumers filter on `entity_type = '<tag>'` — the
            // redistribute dedupe, /api/metrics/fleet's last-event lookups,
            // and the breaker's window_stats — and all three silently
            // matched NOTHING while this column held {"kind":"task"}
            // (the previous trim_matches('"') stripped quotes from a shape
            // serde never produces for this enum; caught by RR-0111a's
            // replay work + the redistribute dedupe test). Old rows in
            // existing DBs may still carry the object shape, so READERS
            // stay tolerant of both: parse_entity_type below,
            // db::replay::entity_tag.
            let entity_type_str = match &ev.entity_type {
                amux_core::revision::EntityType::Other(s) => s.clone(),
                t => serde_json::to_value(t)
                    .ok()
                    .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
                    .unwrap_or_else(|| "other".into()),
            };
            let mutation_json = serde_json::to_string(&ev.mutation).unwrap_or_default();
            // Snapshot rides in the same INSERT as the event it describes —
            // journal row and payload cannot disagree about which transaction
            // produced them (RR-0111a).
            let payload_json = ev.payload.as_ref().map(|p| p.to_string());
            conn.execute(
                "INSERT INTO _amux_state_events (rev, entity_type, entity_id, mutation, at, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![rev, entity_type_str, ev.entity_id, mutation_json, now.to_rfc3339(), payload_json],
            )?;
            if let Some(id) = interaction_id {
                conn.execute("INSERT INTO _amux_interaction_effects (interaction_id,event_id,kind,entity_kind,entity_id,rev)
                    VALUES (?1,?2,?3,?4,?5,?6)", rusqlite::params![id, conn.last_insert_rowid(),
                        mutation_json, entity_type_str, ev.entity_id, rev])?;
            }
            committed_events.push(StateEvent {
                rev: StateRevision(rev),
                entity_type: ev.entity_type,
                entity_id: ev.entity_id,
                mutation: ev.mutation,
                at: now,
            });
        }
        StateRevision(rev)
    } else {
        let rev: u64 =
            conn.query_row("SELECT rev FROM _amux_rev WHERE id = 1", [], |r| r.get(0))?;
        StateRevision(rev)
    };
    let commit_started = std::time::Instant::now();
    transaction.commit()?;
    LAST_COMMIT_MS.store(
        commit_started.elapsed().as_millis() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
    // Publish only after commit: a subscriber must never see an event whose
    // transaction later rolled back.
    for ev in &committed_events {
        let _ = events_tx.send(ev.clone());
    }
    Ok(WriteReply {
        applied: outcome.applied,
        rev,
        events: committed_events,
    })
}

/// Shared handle used by API state.
pub type SharedStore = Arc<Store>;

#[cfg(test)]
mod amux4739_pool_startup_tests {
    use super::*;

    /// AMUX-4739: opening the store must not wait for a FULL read pool.
    ///
    /// `Pool::build` waits for `min_idle.unwrap_or(max_size)` connections and
    /// bounds that wait by `connection_timeout` (r2d2 0.8.10, lib.rs:391-395).
    /// With min_idle unset that is every connection, so AF-640's 5s acquisition
    /// timeout silently became a 5s STARTUP budget for `available_parallelism`
    /// connections.
    ///
    /// The failure mode is the expensive part: `Store::open` returns
    /// Err("timed out waiting for connection"), so a busy machine presents as a
    /// broken database. Measured on an unmodified origin/main, one full lib run
    /// produced 39 of these across modules sharing nothing but this call, and a
    /// second run failed a different set, which is why it read as flakiness.
    #[test]
    fn opening_the_store_waits_for_one_connection_not_the_whole_pool() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("startup.db")).unwrap();

        let min_idle = store.read_pool.min_idle();
        let max_size = store.read_pool.max_size();
        assert_eq!(
            min_idle,
            Some(1),
            "open must block on a single connection; None means max_size ({max_size}) \
             connections inside the {:?} connection_timeout",
            store.read_pool.connection_timeout()
        );
        // The point is the RELATIONSHIP, not the literal. A min_idle equal to
        // max_size would satisfy "is set" while restoring the whole defect.
        assert!(
            min_idle.is_some_and(|m| m < max_size.max(2)),
            "min_idle {min_idle:?} is not below max_size {max_size}; startup would still \
             wait for the full pool"
        );
        // The pool must still be ABLE to grow, or this traded a startup stall
        // for a permanent one-connection bottleneck.
        assert!(
            max_size > 1,
            "max_size {max_size} leaves no room to grow beyond the startup minimum"
        );
    }

    /// The acquisition bound AF-640 set must survive this change. Startup and
    /// acquisition read the same field, so it is exactly the kind of pair where
    /// fixing one silently relaxes the other.
    #[test]
    fn the_acquisition_timeout_af640_set_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("timeout.db")).unwrap();
        assert_eq!(
            store.read_pool.connection_timeout(),
            std::time::Duration::from_secs(5),
            "AF-640 cut this from 30s to 5s because a blocked acquire pins a tokio \
             worker; raising it to make startup easier would undo that"
        );
    }
}

#[cfg(test)]
mod amux4744_write_queue_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// AMUX-4744: a write that waits behind the writer thread must be
    /// MEASURABLE afterwards.
    ///
    /// The writer is one OS thread behind an unbounded channel, and
    /// `write_correlated` waits on `recv()` with no timeout. So a slow write
    /// delays every write behind it without bound while reads, which never
    /// touch this path, keep answering at full speed. Measured live on build
    /// 24716ccf: 20 paired samples gave POSTs of 81.4s and 83.8s while every
    /// GET in the same loop returned in ~8ms.
    ///
    /// Before this the wait produced NO output at all. Nothing timed it and
    /// nothing counted the queue, so the only instrument was a human holding a
    /// stopwatch on the client, which is how the stall survived five rounds of
    /// elimination.
    ///
    /// THE GAUGE MUST SURVIVE THE STALL IT RECORDS. `write_wait_max_ms` only
    /// rises, because a decaying gauge reads zero exactly when someone arrives
    /// to look at it, and a zero that means "recovered" is indistinguishable
    /// from a zero that means "never happened" (ethos rule 4).
    #[test]
    fn a_write_that_queues_behind_a_slow_one_is_measurable_afterwards() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("wq.db")).unwrap();
        assert_eq!(
            store.write_wait_max_ms.load(Ordering::Relaxed),
            0,
            "a fresh store has recorded no wait; otherwise this cell cannot \
             tell its own write from leftover state"
        );

        // Occupy the writer for longer than the warn threshold. This is the
        // real writer thread and a real queued write, not a simulated delay.
        let hold = std::time::Duration::from_millis(WRITE_WAIT_WARN_MS + 400);
        let blocker = {
            let s = store.clone();
            std::thread::spawn(move || {
                s.write(move |_conn| {
                    std::thread::sleep(hold);
                    Ok(WriteOutcome {
                        applied: false,
                        events: vec![],
                    })
                })
            })
        };
        // Let the slow write reach the writer before queueing behind it.
        std::thread::sleep(std::time::Duration::from_millis(150));

        let started = std::time::Instant::now();
        store
            .write(|conn| {
                conn.execute_batch("CREATE TABLE IF NOT EXISTS wq_probe (id INTEGER)")?;
                Ok(WriteOutcome {
                    applied: false,
                    events: vec![],
                })
            })
            .expect("the queued write still completes");
        let observed = started.elapsed();
        blocker
            .join()
            .expect("blocker joins")
            .expect("blocker write");

        assert!(
            observed >= std::time::Duration::from_millis(WRITE_WAIT_WARN_MS),
            "the second write did not actually queue ({observed:?}); the cell would \
             then be asserting about a gauge nothing exercised"
        );
        let recorded = store.write_wait_max_ms.load(Ordering::Relaxed);
        assert!(
            recorded >= WRITE_WAIT_WARN_MS,
            "a write waited {observed:?} and the store reports a maximum of \
             {recorded}ms; the wait is still invisible"
        );
        // The queue drains: a gauge that pinned in-flight high would report a
        // permanent stall on a healthy server.
        assert_eq!(
            store.write_inflight.load(Ordering::Relaxed),
            0,
            "in-flight must return to zero once writes are answered"
        );
    }

    /// The DIAGNOSTIC half. The gauge test above stays green if the warn is
    /// deleted, because an atomic and a log line are independent, and amux's
    /// two-fix rule asks for the log signal specifically: a fix with no
    /// counter, WARN or verdict field cannot announce its own regression.
    ///
    /// THIS PINS A PEER'S WARN, NOT ONE OF MY OWN, deliberately. `writer_slow`
    /// (1f0cca3e, codex-board-execution-contract) landed on this path the same
    /// minute as these gauges and is better placed than a caller-side warn:
    /// it carries `origin`, the type name of the blocking closure, so it names
    /// WHICH mutation held the writer. I dropped my duplicate rather than emit
    /// two lines per stall, which leaves these gauges depending on a verdict I
    /// do not own. Hence a test: nothing else here would notice if a later edit
    /// dropped `origin` and left a bare "something was slow".
    #[test]
    fn a_slow_write_reports_itself_under_a_greppable_verdict() {
        let src = include_str!("mod.rs");
        let body = src
            .split_once("\nfn writer_loop(")
            .expect("writer_loop exists")
            .1;
        // BOUND IT. An unbounded window sweeps past the function into the test
        // module below, which contains these same literals in its own asserts
        // and doc comments, and the scan then passes by matching itself. That
        // trap fired three separate times in one day across two repos.
        let body = body.split_once("\n}\n").expect("its closing brace").0;

        // STRIP COMMENTS BEFORE ASSERTING ANYTHING. A source scan that reads
        // prose passes on the description of the code instead of the code, and
        // it is invisible because the description is usually accurate.
        //
        // Measured, this cell, today: asserting `body.contains("origin")`
        // survived a mutation that deleted `origin = req.origin` outright,
        // because the line's own comment says "origin identifies the blocking
        // mutation". The scan matched the sentence explaining the field while
        // the field was gone. That is the fourth variant of this trap in a day
        // (AMUX-4720, an ugrep window, a 3000-char sweep, this), so it is
        // handled structurally here rather than by picking better literals.
        let body: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        // LANDMARK FIRST: prove the window is the function, not some other
        // region that happens to mention the same words. Two of them, because
        // the first landmark I picked ("reply_rx.recv()") stopped existing the
        // moment the expression was split across lines, and a landmark that
        // breaks on formatting gets deleted as flaky rather than trusted.
        assert!(
            body.contains("rx.recv()") && body.contains("catch_unwind"),
            "the scan is not reading writer_loop; it has {} chars of something else",
            body.len()
        );
        assert!(
            body.contains("writer_slow"),
            "a delayed serialized write must report a greppable verdict; \
             writer_loop no longer names writer_slow"
        );
        // A verdict with no numbers says something was slow and not how slow,
        // and without `origin` it cannot say WHAT was slow, which is the field
        // that turns this line into a lead instead of a notification.
        // BOUND TO THE WARN, not the function. `commit_ms` also appears in the
        // `stmt_ms` arithmetic above the macro, so a version of this that
        // scanned the whole body stayed GREEN when the field was deleted from
        // the warn itself. Measured: that mutation passed.
        let warn_at = body
            .find("writer_slow")
            .expect("the verdict is in this function");
        let warn = &body[warn_at
            ..body[warn_at..]
                .find(");")
                .map(|i| warn_at + i)
                .unwrap_or(body.len())];
        for field in [
            "queued_ms",
            "work_ms",
            "origin = req.origin",
            "site = %req.site",
            "commit_ms",
            "begin_ms",
            "stmt_ms",
        ] {
            assert!(
                warn.contains(field),
                "the writer_slow verdict must carry `{field}`; without it the line \
                 reports that a stall happened and not what caused it"
            );
        }
    }

    /// AMUX-4830: a slow write must say whether it was slow WORK or a slow COMMIT.
    ///
    /// `work_ms` spans both, and conflating them sent this card at the wrong
    /// target. Thirteen writer sites doing completely different work clustered
    /// between 589ms and 2189ms (CV 0.32) on the live box: a single small insert
    /// at `api/interactions.rs:101` cost 589ms, which cannot be statement time.
    /// Per-site optimisation cannot move a shared commit floor, and without this
    /// split the only way to see that was to compare sites against each other.
    #[test]
    fn a_slow_write_separates_its_commit_from_its_statements() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("split.db")).unwrap();
        // A real write, so a real commit happens and the thread-local is set by
        // the shipped path rather than by the test.
        store
            .write(|conn| {
                conn.execute("CREATE TABLE IF NOT EXISTS t_split (k INTEGER)", [])?;
                conn.execute("INSERT INTO t_split (k) VALUES (1)", [])?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .unwrap();

        // The arithmetic the warn publishes must hold: stmt + commit == work,
        // and neither half may exceed the whole. `saturating_sub` makes the
        // second one silent if the reset is ever dropped, which is why it is
        // asserted rather than assumed.
        let commit = LAST_COMMIT_MS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            commit < 60_000,
            "a commit time of {commit}ms is not a measurement, it is a stuck clock"
        );

        // THE RESET IS LOAD-BEARING. Without it a write that fails BEFORE
        // committing reports the previous write's commit time as its own, which
        // is a wrong number that looks entirely plausible.
        // SEED a non-zero value first. Without this the assertion below passes
        // whether or not the reset exists, because a fast commit leaves 0
        // behind anyway — the exact way my first version of this test was
        // vacuous, confirmed by a mutation that dropped the reset and stayed
        // green.
        LAST_COMMIT_MS.store(4242, std::sync::atomic::Ordering::Relaxed);
        let failed = store.write(|_conn| {
            Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                std::io::Error::other("deliberate failure before commit"),
            )))
        });
        assert!(failed.is_err(), "the fixture must actually fail");
        assert_eq!(
            LAST_COMMIT_MS.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a write that never committed must report commit_ms 0, not the last \
             successful write's commit time"
        );
    }

    /// AMUX-4837: `begin_ms` must capture the WRITE-LOCK WAIT, not round to zero.
    ///
    /// This is the half `stmt_ms` was hiding. `stmt_ms` was derived as
    /// everything-that-is-not-commit, so a write that spent 400ms asleep in
    /// SQLite's busy handler waiting for the lock reported 400ms of "statement"
    /// time, and the fleet then went looking for a slow query that does not
    /// exist. The live shape that motivated this: `api/interactions.rs` does one
    /// indexed SELECT and one INSERT, emits no events, and still averaged 466ms
    /// with a 0ms commit.
    ///
    /// A RESET TEST CANNOT COVER THIS and that is why the test is contention
    /// rather than a sentinel. `writer_loop` zeroes the atomic before every
    /// write, and an uncontended BEGIN also leaves 0, so "stamped 0" and "never
    /// stamped" are the same observation on a quiet database. Only a BEGIN that
    /// genuinely has to wait can tell them apart.
    #[test]
    fn begin_ms_measures_the_wait_for_the_write_lock() {
        use std::sync::mpsc;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("beginwait.db");
        let store = Store::open(&path).unwrap();
        store
            .write(|conn| {
                conn.execute("CREATE TABLE IF NOT EXISTS t_begin (k INTEGER)", [])?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .unwrap();

        // A SECOND connection holds the write lock for a while. The store's
        // writer must sit in BEGIN IMMEDIATE until this one commits.
        let blocker = Connection::open(&path).unwrap();
        blocker
            .busy_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        blocker
            .execute_batch("BEGIN IMMEDIATE; INSERT INTO t_begin (k) VALUES (99);")
            .unwrap();

        let (tx, rx) = mpsc::channel();
        let held_ms = 400;
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(held_ms));
            blocker.execute_batch("COMMIT;").unwrap();
            let _ = tx.send(());
        });

        store
            .write(|conn| {
                conn.execute("INSERT INTO t_begin (k) VALUES (1)", [])?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .unwrap();
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .expect("blocker committed");

        let begin = LAST_BEGIN_MS.load(std::sync::atomic::Ordering::Relaxed);
        // Generous floor: the point is that the wait lands in `begin_ms` at all,
        // not that the busy handler wakes on any particular step of its backoff.
        assert!(
            begin >= held_ms / 2,
            "begin_ms must carry the write-lock wait; the lock was held ~{held_ms}ms and \
             begin_ms reported {begin}ms. A 0 here means the wait is being charged to \
             stmt_ms, which is the conflation AMUX-4837 exists to end"
        );
        assert!(
            begin < 60_000,
            "a begin time of {begin}ms is not a measurement, it is a stuck clock"
        );
    }

    /// AMUX-4781: `origin` names the FUNCTION, and that is not enough to act on.
    ///
    /// `origin` is `type_name::<F>()`, and every closure inside one function
    /// renders as the identical string. `Runtime::tick_once` holds twelve
    /// `write_async` call sites and was the largest writer hold on this box
    /// (n=4, median 1917ms over one build), yet all twelve logged
    /// `Runtime::tick_once::{{closure}}::{{closure}}`. There was no way to ask
    /// which write was slow, which is why the card that sent me here could not
    /// name a target.
    ///
    /// The fix is `#[track_caller]` plus `Location::caller()`. This test is the
    /// reason it had to stop being an `async fn`: `#[track_caller]` is accepted
    /// on one and silently does not propagate through the generated future, so
    /// every site would have reported db/mod.rs itself.
    #[test]
    fn two_writes_from_different_call_sites_are_told_apart() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("sites.db")).unwrap();
        let noop = |_: &Connection| {
            Ok(WriteOutcome {
                applied: false,
                events: vec![],
            })
        };
        // Two calls, two LINES, one identical closure body: `origin` cannot
        // separate these and `site` must.
        let a = std::panic::Location::caller();
        store.write(noop).unwrap();
        let b = std::panic::Location::caller();
        store.write(noop).unwrap();
        assert_eq!(a.file(), b.file(), "same file, so only the line can differ");

        // The real assertion is on the mechanism the writer uses. A call from
        // THIS file must attribute here, not to db/mod.rs's own internals.
        #[track_caller]
        fn site_of() -> &'static std::panic::Location<'static> {
            std::panic::Location::caller()
        }
        let here = site_of();
        let there = site_of();
        assert!(
            here.file().ends_with("mod.rs"),
            "track_caller must report the CALLER's file, got {}",
            here.file()
        );
        assert_ne!(
            here.line(),
            there.line(),
            "two call sites on different lines must produce different locations; \
             if these are equal the caller location is being captured in the callee"
        );
    }

    /// The threshold has to be crossable in the direction that matters. A warn
    /// that fires on every write is noise a sweep learns to ignore.
    #[test]
    fn a_fast_write_reports_no_wait_and_no_warning() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("wq2.db")).unwrap();
        for _ in 0..20 {
            store
                .write(|_conn| {
                    Ok(WriteOutcome {
                        applied: false,
                        events: vec![],
                    })
                })
                .expect("write");
        }
        let recorded = store.write_wait_max_ms.load(Ordering::Relaxed);
        assert!(
            recorded < WRITE_WAIT_WARN_MS,
            "20 trivial writes recorded a {recorded}ms maximum, at or above the \
             {WRITE_WAIT_WARN_MS}ms warn threshold; the signal would fire constantly"
        );
    }
}

#[cfg(test)]
mod af640_read_pool_tests {
    use super::*;

    /// AF-640 / AMUX-4744: a read must not pin the runtime worker that awaits it.
    ///
    /// The pool's `max_size` is `available_parallelism`, which is ALSO tokio's
    /// default worker count, so a saturated pool could pin every worker at once.
    /// That is what made the 2026-09-08 exhaustion self-sustaining for 22
    /// minutes and recur five times the same day. Cutting the timeout 30s -> 5s
    /// bounded the pin; `read_async` removes it for callers that can await.
    ///
    /// SINGLE-WORKER RUNTIME ON PURPOSE. With one worker thread a blocking
    /// acquire makes everything else on that runtime unrunnable, so this cell
    /// cannot pass by falling back on a spare worker.
    #[test]
    fn a_read_async_leaves_the_runtime_worker_free_to_poll() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("ra.db")).unwrap();

        rt.block_on(async move {
            // Hold EVERY read connection, so any further acquire must wait.
            let held: Vec<_> = (0..store.read_pool.max_size())
                .map(|_| store.read_pool.get().expect("prefill"))
                .collect();

            let s2 = store.clone();
            let reader = tokio::spawn(async move {
                s2.read_async(|c| Ok(c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?))
                    .await
            });

            // THE POINT: while that read waits for a connection, the single
            // runtime worker must still poll something else. Before read_async
            // this task could not be polled at all, because the blocked acquire
            // owned the worker.
            let ticked = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                tokio::spawn(async { 42u8 }),
            )
            .await
            .expect("the runtime worker was pinned by a blocked read acquire")
            .expect("join");
            assert_eq!(ticked, 42);

            drop(held);
            let got = reader.await.expect("join");
            assert_eq!(
                got.unwrap(),
                1,
                "the read still returns once a connection frees"
            );
        });
    }

    /// The DIAGNOSTIC half, which the timeout test does not cover: mutating the
    /// warn away leaves that cell green, because a pool can fail fast and say
    /// nothing about why.
    ///
    /// `read_pool_exhausted` is the string the health payload already reports
    /// and the one a log sweep greps for, so it is the name that has to survive
    /// a rename, not just the presence of some warning.
    #[test]
    fn a_saturated_pool_reports_its_state_under_greppable_verdicts() {
        let src = include_str!("mod.rs");
        let body = src
            .split_once("\n    pub fn read(&self)")
            .expect("Store::read exists")
            .1;
        let body = body.split_once("\n    }\n").expect("its closing brace").0;

        // LANDMARK FIRST: prove the scan is reading `read`, not some other
        // region. Anchoring on a name that also appears quoted elsewhere has
        // silently read the wrong block three times today.
        assert!(
            body.contains("self.read_pool.get()"),
            "the scan is not reading Store::read; it has {} chars of something else",
            body.len()
        );

        for verdict in ["read_pool_exhausted", "read_pool_slow_acquire"] {
            assert!(
                body.contains(&format!("verdict = \"{verdict}\"")),
                "a saturated pool must report under `{verdict}`, which is what the health \
                 payload uses and what a sweep greps for"
            );
        }
        // The state is what makes it diagnosable. A verdict with no numbers is
        // the 30-second 500 again, wearing a better name.
        for field in ["connections", "idle", "max_size", "waited_ms"] {
            assert!(
                body.contains(&format!("{field} =")),
                "the warn must carry `{field}`; without it nobody can tell a saturated \
                 pool from a slow query"
            );
        }
    }

    /// AF-640. The pool must FAIL rather than pin a thread for half a minute,
    /// and the bound must be the one we set rather than r2d2's default.
    ///
    /// EXHAUSTS THE POOL FOR REAL. Asserting the builder was called with a
    /// duration would pass on a value that never reaches the pool; this holds
    /// every connection and measures what a caller actually experiences.
    #[test]
    fn an_exhausted_read_pool_fails_fast_instead_of_pinning_a_thread() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("pool.db")).unwrap();
        let max = store.read_pool.max_size() as usize;
        assert!(max >= 1, "a pool with no connections cannot be exhausted");

        // Hold every connection, so the next acquire has nowhere to go.
        let held: Vec<_> = (0..max)
            .map(|_| store.read().expect("initial fill"))
            .collect();
        assert_eq!(
            store.read_pool.state().idle_connections,
            0,
            "the pool must be empty"
        );

        let t0 = std::time::Instant::now();
        let denied = store.read();
        let waited = t0.elapsed();

        assert!(
            denied.is_err(),
            "an exhausted pool must refuse, not hand out a 29th connection"
        );
        // THE POINT: it fails in ~5s, not r2d2's default 30s. The upper bound is
        // what this card is about; the lower bound catches a timeout set so
        // small that ordinary contention would start failing.
        assert!(
            waited < std::time::Duration::from_secs(12),
            "waited {waited:?}: that is r2d2's 30s default, not our timeout, and every \
             one of those seconds pins a tokio worker"
        );
        assert!(
            waited >= std::time::Duration::from_secs(2),
            "waited only {waited:?}: the timeout is so short that normal contention \
             would 500 rather than queue"
        );
        drop(held);

        // CONTROL: after releasing, a read must succeed again. Without this the
        // assertions above are satisfied by a pool that is simply broken.
        assert!(
            store.read().is_ok(),
            "the pool must recover once connections are returned"
        );
    }
}

/// AF-937 / AEAB-11 recurrence: a second `amux-server-rs` opened the same
/// production db_path as the real server for ~13h with nothing anywhere
/// warning that two writers existed. `claim_sole_writer` closes that gap.
#[cfg(test)]
mod af937_writer_lock_tests {
    use super::*;

    #[derive(Clone)]
    struct CapturedLogs(Arc<std::sync::Mutex<Vec<u8>>>);

    struct CapturedLogWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = CapturedLogWriter;
        fn make_writer(&'a self) -> Self::Writer {
            CapturedLogWriter(self.0.clone())
        }
    }

    impl std::io::Write for CapturedLogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn with_captured_logs<T>(f: impl FnOnce() -> T) -> (T, String) {
        let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_writer(CapturedLogs(buf.clone()))
            .finish();
        let _scope = tracing::subscriber::set_default(subscriber);
        let result = f();
        let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        (result, logs)
    }

    /// A fabricated holder-lock, as a genuinely different OS process would
    /// leave one: a real flock held (so a real probe really contends), with a
    /// pid that is NOT this test's own. Kept alive by returning the `File` --
    /// dropping it releases the lock, same as the codebase's own real usage.
    fn plant_foreign_holder(db_path: &Path, fake_pid: u32, fake_port: u16) -> std::fs::File {
        use std::io::Write;
        use std::os::unix::io::AsRawFd;
        let lock_path = holder_lock_path(db_path);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0, "test setup must win an uncontended lock");
        let mut f = &file;
        write!(f, r#"{{"pid":{fake_pid},"port":{fake_port}}}"#).unwrap();
        f.flush().unwrap();
        file
    }

    /// The plain case first: nothing else has this db_path open. Opening must
    /// both succeed AND leave no trace of the warning this whole card is
    /// about -- the single-writer path must be silent, not merely non-fatal.
    #[test]
    fn a_lone_opener_holds_the_lock_and_logs_no_contention_warning() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.db");
        let (store, logs) = with_captured_logs(|| Store::open(&db_path).unwrap());
        assert!(
            store.holds_writer_lock(),
            "the only opener must win the lock"
        );
        assert!(
            !logs.contains("concurrent_writer_detected"),
            "a lone opener must not warn about contention: {logs}"
        );
    }

    /// The defect this card fixes: a second process (simulated here by a
    /// planted foreign lock, not this test's own pid) already has db_path
    /// open. A second scheduler must not start; log and return the owning
    /// pid/port before touching the database.
    #[test]
    fn a_foreign_holder_blocks_duplicate_driver_before_database_open() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.db");
        let fake_pid = std::process::id().wrapping_add(9973); // never our own pid
        let _foreign = plant_foreign_holder(&db_path, fake_pid, 8823);

        let (opened, logs) = with_captured_logs(|| Store::open(&db_path));
        assert!(opened.is_err(), "a second runtime must not start");
        assert!(!db_path.exists(), "refusal must precede migrations and writer startup");
        assert!(
            logs.contains("concurrent_writer_detected"),
            "must warn under the greppable verdict: {logs}"
        );
        assert!(
            logs.contains(&fake_pid.to_string()),
            "the warning must name the OTHER process's pid, not just that one exists: {logs}"
        );
        assert!(
            logs.contains("8823"),
            "the warning must carry the other process's port too: {logs}"
        );
        drop(_foreign);
        assert!(Store::open(&db_path).unwrap().holds_writer_lock(), "ownership recovers when the prior process exits");
    }

    /// The false-positive this design specifically avoids: the SAME process
    /// reopening db_path while its first handle is still alive (exactly what
    /// board_drive.rs's crash-recovery tests do to simulate a restart without
    /// an actual exec). flock() is scoped to the open file description, so
    /// the second open() genuinely contends -- but it must read as "this is
    /// me", not get reported as a second OS process.
    #[test]
    fn a_same_process_reopen_is_not_reported_as_a_foreign_writer() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.db");
        let first = Store::open(&db_path).unwrap();
        assert!(first.holds_writer_lock());

        let (second, logs) = with_captured_logs(|| Store::open(&db_path));
        let second = second.expect("a same-process reopen must not fail startup");
        assert!(
            !second.holds_writer_lock(),
            "the second handle cannot ALSO hold an exclusive flock the first still has"
        );
        assert!(
            !logs.contains("concurrent_writer_detected"),
            "a same-process reopen is not a second OS process and must not warn: {logs}"
        );
        drop(first);
    }
}

/// AMUX-4842: the absent pragmas are a DECISION, and a decision that lives only
/// in a comment gets deleted by the next person who reads the comment as an
/// oversight.
#[cfg(test)]
mod pragma_decision_tests {
    /// Setting `cache_size` or `mmap_size` must redden, so whoever does it has
    /// to read why they are absent first.
    ///
    /// This reads the SOURCE because there is no runtime observation that
    /// distinguishes "deliberately default" from "nobody thought about it":
    /// both produce a connection reporting cache_size=-2000. The guard is bound
    /// to `configure_connection`'s own body rather than the file, because
    /// `cache_size` appears in the doc comment above it and a file-wide search
    /// would fail on the explanation rather than on the behaviour.
    #[test]
    fn cache_size_and_mmap_stay_at_their_defaults_until_someone_re_measures() {
        let src = include_str!("mod.rs");
        let at = src
            .find("fn configure_connection(c: &Connection)")
            .expect("configure_connection exists");
        let rest = &src[at..];
        let end = rest.find("\n}").map(|i| at + i).unwrap_or(src.len());
        let body = &src[at..end];

        for pragma in ["cache_size", "mmap_size"] {
            assert!(
                !body.contains(pragma),
                "`{pragma}` is set in configure_connection, but it is absent BY MEASUREMENT: \
                 on a host whose OS page cache already holds the whole database, raising \
                 cache_size measured 13-15% SLOWER (600 point lookups, 16 interleaved rounds, \
                 2026-09-19) because SQLite's cache then saves no I/O and only adds \
                 bookkeeping. If the host changed, re-measure with INTERLEAVED arms (a \
                 block-ordered A-B-A gave the opposite answer on this box) and rewrite the \
                 doc comment above this function before setting it. Body was: {body}"
            );
        }
    }

    /// And the reasoning has to survive too: a guard that only checks absence
    /// would stay green if someone deleted the explanation and left the pragma
    /// out by accident, which is how the next person ends up re-deriving it.
    ///
    /// SCOPED TO THE PRODUCTION HALF, and that is not a detail. The first
    /// version of this searched the whole file, which includes this test's own
    /// assertion message — so it matched its own label and would have stayed
    /// green with the doc comment gutted. Found by mutating the comment and
    /// watching nothing redden.
    #[test]
    fn the_measurement_behind_that_decision_is_still_written_down() {
        let src = include_str!("mod.rs");
        // The DOC BLOCK of configure_connection, not the file and not
        // "everything before the first test module" — there is a `#[cfg(test)]`
        // earlier in this file, so that split lands above this comment and the
        // guard fails for the wrong reason.
        let at = src
            .find("fn configure_connection(c: &Connection)")
            .expect("configure_connection exists");
        let doc = &src[at.saturating_sub(2600)..at];
        for needle in ["AMUX-4842", "INTERLEAVED", "SIGBUS"] {
            assert!(
                doc.contains(needle),
                "the pragma decision's rationale lost `{needle}`; without it the absence \
                 reads as an oversight and gets 'fixed'"
            );
        }
    }
}

#[cfg(test)]
mod read_pool_sizing_tests {
    use super::*;

    /// These two tests MUTATE AND READ the same process env var, so they must
    /// not run concurrently — `cargo` runs them on parallel threads in one
    /// process. Caught by mutation: reverting the pool size reddened only the
    /// tunable test, because the other had already observed the 64 its
    /// neighbour set. That is AMUX-4963's defect, in a test I was about to
    /// ship while fixing it elsewhere.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// AMUX-4955. The property, not the number: the read pool must be able to
    /// serve every tokio worker at once.
    ///
    /// It used to be exactly `available_parallelism`, which is also tokio's
    /// default worker count, so a saturated pool could pin every worker
    /// simultaneously — the builder's own comment calls that self-sustaining,
    /// because a worker blocked waiting for a connection cannot release the one
    /// it is waiting behind. Measured 2026-09-23: 824 read_pool_slow_acquire
    /// events, 97% with idle=0 at 28/28.
    #[test]
    fn the_read_pool_is_strictly_larger_than_the_worker_count() {
        let _env = env_guard();
        std::env::remove_var("AMUX_READ_POOL_SIZE");
        let workers = worker_threads();
        assert!(
            read_pool_size() > workers,
            "a pool no larger than the worker count lets workers starve each other: \
             pool {} vs {workers} workers",
            read_pool_size()
        );
        // A tiny box must still get a usable pool rather than 2.
        assert!(read_pool_size() >= 8);
    }

    /// Operable without a rebuild: the measurement that justified the default
    /// came from one box, and the next one may disagree.
    #[test]
    fn the_read_pool_size_is_tunable_and_rejects_nonsense() {
        let _env = env_guard();
        std::env::set_var("AMUX_READ_POOL_SIZE", "64");
        assert_eq!(read_pool_size(), 64);
        // Garbage and zero fall back to the computed default rather than
        // configuring a pool nobody can acquire from.
        for bad in ["0", "-1", "", "lots"] {
            std::env::set_var("AMUX_READ_POOL_SIZE", bad);
            assert!(
                read_pool_size() > worker_threads(),
                "bad value {bad:?} must fall back"
            );
        }
        std::env::remove_var("AMUX_READ_POOL_SIZE");
    }
}

#[cfg(test)]
mod storage_maintenance_tests {
    use super::*;

    #[tokio::test]
    async fn maintenance_uses_writer_without_weakening_readers_or_creating_revisions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("maintenance.db")).unwrap();
        store.write_async(|c| {
            c.execute_batch("CREATE TABLE maintenance_fixture(id INTEGER PRIMARY KEY, body BLOB); INSERT INTO maintenance_fixture VALUES(1,zeroblob(16384));")?;
            Ok(WriteOutcome{applied:true,events:vec![]})
        }).await.unwrap();
        let rev = store.current_rev().unwrap();
        assert!(
            store
                .read_async(|c| {
                    c.execute_batch("VACUUM")?;
                    Ok(())
                })
                .await
                .is_err(),
            "negative control: readers must remain query-only"
        );
        store
            .maintenance_async(Maintenance::Checkpoint)
            .await
            .unwrap();
        store.maintenance_async(Maintenance::Vacuum).await.unwrap();
        assert_eq!(
            store.current_rev().unwrap(),
            rev,
            "maintenance is not a domain mutation"
        );
        store
            .read_async(|c| {
                assert_eq!(
                    c.query_row("PRAGMA query_only", [], |r| r.get::<_, i64>(0))?,
                    1
                );
                assert_eq!(
                    c.query_row(
                        "SELECT length(body) FROM maintenance_fixture WHERE id=1",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    16384
                );
                Ok(())
            })
            .await
            .unwrap();
        store
            .write_async(|c| {
                c.execute(
                    "INSERT INTO maintenance_fixture VALUES(2,'after maintenance')",
                    [],
                )?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .await
            .unwrap();
        assert!(
            store.current_rev().unwrap().0 > rev.0,
            "subsequent ordinary writes still commit"
        );
    }

    #[test]
    fn maintenance_checkpoint_contention_is_not_reported_as_completed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("busy.db");
        let writer = Connection::open(&path).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE example(n); INSERT INTO example VALUES(1);",
            )
            .unwrap();
        writer
            .busy_timeout(std::time::Duration::from_millis(1))
            .unwrap();
        let reader = Connection::open(&path).unwrap();
        reader
            .execute_batch("BEGIN; SELECT * FROM example;")
            .unwrap();
        writer.execute("INSERT INTO example VALUES(2)", []).unwrap();
        let error = match apply_maintenance(&writer, Maintenance::Checkpoint) {
            Err(error) => error,
            Ok(_) => panic!("active snapshot should defer checkpoint"),
        };
        assert!(error.to_string().contains("checkpoint deferred"), "{error}");
        assert!(writer.is_autocommit());
        reader.execute_batch("ROLLBACK").unwrap();
    }
}
