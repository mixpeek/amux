//! RR-0150 — persistent-data restart suite: create -> kill -> restart ->
//! reconcile -> verify, across every durable subsystem.
//!
//! WHY THIS DRIVES A REAL PROCESS. The claim under test is "nothing that
//! matters lives only in process memory". A `Store` reopen cannot test that
//! claim, because a `Store` reopen is the one operation that CANNOT observe
//! the failure: the in-memory copy is exactly what disappears, so a test that
//! never had a process to lose would pass against a server that keeps
//! everything in a `Mutex<HashMap>` and writes nothing. So this suite spawns
//! the REAL server binary, talks to it over its REAL TLS socket, **SIGKILLs
//! it**, and respawns it on the same port against the same DB.
//!
//! SIGKILL, not a graceful stop, on purpose. A shutdown hook that flushes
//! state on the way out is indistinguishable from durable writes right up
//! until the process is killed, OOMs, or is replaced by the self-adoption
//! exec — all of which happen to this server routinely. If a subsystem only
//! survives a polite exit, this suite must call that a failure.
//!
//! SAFETY — this machine hosts a LIVE amux fleet, and the server binary runs
//! three loops that drive it (`steer_deliver_loop` -> tmux keystrokes,
//! `ghost_rescue` -> presses Enter, `board_drive` -> pickup/advance nudges).
//! All three enumerate their targets from `$AMUX_HOME/sessions/*.env`
//! (`all_lane_names`), so the suite gives the server a TEMP `AMUX_HOME` and a
//! TEMP database. The single lane env it does create carries a `rr0150-`
//! prefix plus a random suffix — a name no fleet session has and no tmux
//! session answers to, so `is_running` is false and the delivery path stops
//! before it can send anything. Nothing here reads or writes `~/.amux/amux.db`.
//!
//! HOW TO SEE IT FAIL (the demonstration this suite is worthless without —
//! ethos rule 7). The server binary is overridable, so the whole suite can be
//! pointed at a deliberately-broken build:
//!
//!   AMUX_RESTART_BIN=/tmp/broken/amux-server \
//!     cargo test -p amux-server --test restart_persistence -- --nocapture
//!
//! Break one persistence path in a scratch copy of the crate (e.g. make the
//! journal INSERT a no-op that still answers 200), build it, point the env var
//! at it, and the matching subsystem must go RED while the others stay green.
//! A persistence suite that has never failed proves nothing.
//!
//! Subsystems that have NO API write path are seeded directly through SQLite
//! and labelled `seeded` in the report — `_amux_conversations` (written only
//! by the protocol's ConversationSink), `_amux_leases` (written only by the
//! orchestrator's command pump), and `_amux_media_jobs` (written only by a
//! live transcode). That is a finding in itself and is reported as one: a
//! durable table with no API reader/writer cannot be verified "through the
//! API" the way RR-0150 asks for.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Rig: a real server process, killable and respawnable
// ---------------------------------------------------------------------------

struct Rig {
    home: PathBuf,
    db: PathBuf,
    port: u16,
    child: Option<Child>,
    client: reqwest::Client,
    log: PathBuf,
    /// Local OAuth fixtures own the wire; suppress external health probes.
    no_external_probes: bool,
    server_binary: Option<PathBuf>,
    #[cfg(unix)]
    server_uid: Option<u32>,
    /// Keeps the temp dir alive for the rig's lifetime.
    _tmp: tempfile::TempDir,
}

fn server_bin() -> PathBuf {
    std::env::var("AMUX_RESTART_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_BIN_EXE_amux-server")))
}

/// A free port, by binding :0 and immediately releasing it.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0");
    l.local_addr().unwrap().port()
}

impl Rig {
    fn new() -> Self {
        Self::in_temp(tempfile::tempdir().expect("tempdir"))
    }

    fn in_temp(tmp: tempfile::TempDir) -> Self {
        let home = tmp.path().join("amux-home");
        std::fs::create_dir_all(&home).unwrap();
        let db = tmp.path().join("test.db");
        let log = tmp.path().join("server.log");
        let client = reqwest::Client::builder()
            // self-signed cert minted into the temp home
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(30))
            .build()
            .expect("client");
        Rig {
            home,
            db,
            port: free_port(),
            child: None,
            client,
            log,
            no_external_probes: false,
            server_binary: None,
            #[cfg(unix)]
            server_uid: None,
            _tmp: tmp,
        }
    }

    /// Root ignores chmod-based write faults. Run only this fixture's server
    /// without root privileges; retain the same failed-write assertion in CI.
    #[cfg(unix)]
    fn permission_fault_fixture() -> Self {
        #[cfg(target_os = "linux")]
        if unsafe { libc::geteuid() } == 0 {
            // /tmp is traversable after dropping uid; a root-private TMPDIR is not.
            let tmp = tempfile::Builder::new()
                .prefix("amux-oauth-permission-")
                .tempdir_in("/tmp")
                .expect("unprivileged fixture tempdir");
            let mut rig = Self::in_temp(tmp);
            rig.server_uid = Some(65534);
            return rig;
        }
        Self::new()
    }

    #[cfg(unix)]
    fn prepare_permission_fault_server(&mut self) {
        let Some(uid) = self.server_uid else { return; };
        let binary = self._tmp.path().join("amux-server-fixture");
        // The shared build-cache ancestors may be root-private. Never chown them.
        std::fs::copy(server_bin(), &binary).expect("copy fixture server");
        fn chown_fixture(path: &std::path::Path, uid: u32) {
            let meta = std::fs::symlink_metadata(path).unwrap();
            assert!(!meta.file_type().is_symlink(), "fixture must contain no symlinks");
            if meta.is_dir() {
                for entry in std::fs::read_dir(path).unwrap() {
                    chown_fixture(&entry.unwrap().path(), uid);
                }
            }
            std::os::unix::fs::chown(path, Some(uid), Some(uid)).unwrap();
        }
        chown_fixture(self._tmp.path(), uid);
        self.server_binary = Some(binary);
        eprintln!("permission_fault_server uid={uid} gid={uid} private_home={}", self.home.display());
    }

    fn spawn(&mut self) {
        let out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .expect("log");
        let err = out.try_clone().unwrap();
        let mut command = Command::new(self.server_binary.clone().unwrap_or_else(server_bin));
        #[cfg(unix)]
        if let Some(uid) = self.server_uid {
            use std::os::unix::process::CommandExt;
            command.uid(uid).gid(uid);
        }
        let child = command
            .env_clear()
            .env("HOME", self.home.join("fixture-home"))
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("AMUX_HOME", &self.home)
            .env("AMUX_DB", &self.db)
            .env("TMUX_TMPDIR", self._tmp.path())
            .env("AMUX_NO_SELF_ADOPT", "1")
            .envs(self.no_external_probes.then_some(("AMUX_AUTOFIX_SECS", "0")))
            .env("AMUX_RS_PORT", self.port.to_string())
            // Auth off: this is a loopback-only temp server.
            .env("AMUX_AUTH_TOKEN", "none")
            // Bootstrap would try to give a created worker a real terminal.
            // Nothing here starts a worker; push it out of the way anyway.
            .env("AMUX_RS_BOOTSTRAP_SECS", "3600")
            .env("RUST_LOG", "warn,amux_server::api::contract=info")
            .stdout(out)
            .stderr(err)
            .spawn()
            .expect("spawn server");
        self.child = Some(child);
        #[cfg(target_os = "linux")]
        if let Some(uid) = self.server_uid {
            let status = std::fs::read_to_string(format!("/proc/{}/status", self.child.as_ref().unwrap().id())).unwrap();
            for field in ["Uid:", "Gid:"] {
                let line = status.lines().find(|line| line.starts_with(field)).unwrap();
                let ids = line.split_whitespace().skip(1).map(|id| id.parse::<u32>().unwrap()).collect::<Vec<_>>();
                assert_eq!(ids, vec![uid; 4], "{line}");
            }
        }
    }

    async fn wait_healthy(&self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut last = String::new();
        while Instant::now() < deadline {
            match self.client.get(self.url("/health")).send().await {
                Ok(r) if r.status().is_success() => {
                    return r.json().await.unwrap_or(json!({}));
                }
                Ok(r) => last = format!("status {}", r.status()),
                Err(e) => last = e.to_string(),
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        panic!(
            "server never became healthy on port {} ({last})\n--- server log ---\n{}",
            self.port,
            std::fs::read_to_string(&self.log).unwrap_or_default()
        );
    }

    /// SIGKILL — see module doc. A graceful stop would let a flush-on-exit
    /// implementation pass a test it should fail.
    fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        // The port must actually be free before the respawn, or the new
        // process logs "address in use" and the suite blames persistence for
        // what is really a bind race.
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", self.port)).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    async fn restart(&mut self) -> Value {
        self.kill();
        self.spawn();
        self.wait_healthy().await
    }

    fn url(&self, path: &str) -> String {
        format!("https://127.0.0.1:{}{}", self.port, path)
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let r = self.client.get(self.url(path)).send().await.expect("GET");
        let code = r.status().as_u16();
        (code, r.json().await.unwrap_or(Value::Null))
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: Value) -> (u16, Value) {
        let r = self
            .client
            .request(method, self.url(path))
            .header("content-type", "application/json")
            .header("x-amux-session", "rr0150-suite")
            .json(&body)
            .send()
            .await
            .expect("request");
        let code = r.status().as_u16();
        (code, r.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, body).await
    }

    async fn patch(&self, path: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::PATCH, path, body).await
    }

    /// Direct SQLite write, for the three tables with no API writer. Used
    /// ONLY where that is true, and always labelled `seeded` in the report.
    fn seed(&self, sql: &str, params: &[&dyn rusqlite::ToSql]) {
        let conn = rusqlite::Connection::open(&self.db).expect("open db");
        conn.execute(sql, params).expect("seed");
    }

    fn count(&self, sql: &str) -> i64 {
        let conn = rusqlite::Connection::open(&self.db).expect("open db");
        conn.query_row(sql, [], |r| r.get(0)).unwrap_or(-1)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.kill();
    }
}

// ---------------------------------------------------------------------------
// Per-subsystem verdicts — every subsystem is reported, not just the first
// failure. A suite that aborts on subsystem #2 hides #3..#10.
// ---------------------------------------------------------------------------

struct Report {
    rows: Vec<(String, bool, String)>,
}

impl Report {
    fn new() -> Self {
        Report { rows: vec![] }
    }
    fn add(&mut self, subsystem: &str, ok: bool, detail: String) {
        println!(
            "[{}] {:<16} {}",
            if ok { "PASS" } else { "FAIL" },
            subsystem,
            detail
        );
        self.rows.push((subsystem.into(), ok, detail));
    }
    fn finish(self) {
        let failed: Vec<_> = self.rows.iter().filter(|r| !r.1).collect();
        println!(
            "\nRR-0150: {} of {} subsystems survived restart",
            self.rows.len() - failed.len(),
            self.rows.len()
        );
        assert!(
            failed.is_empty(),
            "subsystems did NOT survive restart: {}",
            failed
                .iter()
                .map(|r| format!("{} ({})", r.0, r.2))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
}

fn uniq(prefix: &str) -> String {
    format!("{prefix}-{}", ulid::Ulid::new().to_string().to_lowercase())
}

// ---------------------------------------------------------------------------
// The suite
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn every_durable_subsystem_survives_a_hard_restart() {
    let mut rig = Rig::new();
    rig.spawn();
    let h0 = rig.wait_healthy().await;
    let build0 = h0["build"].as_str().unwrap_or("").to_string();
    let pid0 = h0["pid"].as_i64().unwrap_or(0);

    // The one lane env the suite creates. Prefix + ULID: no fleet session and
    // no tmux session answers to it, so the delivery loop stops at is_running.
    let lane = uniq("rr0150");
    let sessions = rig.home.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join(format!("{lane}.env")),
        format!("CC_DIR=/tmp\nCC_CREATOR=rr0150-suite\nCC_NAME={lane}\n"),
    )
    .unwrap();

    // ---------------- phase A: write one row per subsystem ----------------
    let (c, board) = rig
        .post(
            "/api/board",
            json!({"title": "rr0150 board row", "status": "todo"}),
        )
        .await;
    assert!((200..300).contains(&c), "board create: {board}");
    let board_id = board["id"].as_str().expect("board id").to_string();

    let worker_name = uniq("rr0150-worker");
    let (c, worker) = rig.post("/api/workers", json!({"name": worker_name})).await;
    assert!((200..300).contains(&c), "worker create: {worker}");
    let worker_id = worker["id"].as_str().expect("worker id").to_string();

    let (c, sched) = rig
        .post(
            "/api/schedules",
            json!({"title": "rr0150 schedule", "session": lane,
                   "command": "noop", "schedule_expr": "every 15m"}),
        )
        .await;
    assert!((200..300).contains(&c), "schedule create: {sched}");
    let sched_id = sched["id"].as_str().expect("sched id").to_string();
    let next_run_before = sched["next_run"].clone();

    let (c, msg) = rig
        .post(
            "/api/messages",
            json!({"to": "human", "body": "rr0150 message body"}),
        )
        .await;
    assert!((200..300).contains(&c), "message create: {msg}");
    let msg_id = msg["message"]["id"]
        .as_str()
        .or_else(|| msg["id"].as_str())
        .expect("message id")
        .to_string();

    let (c, _) = rig
        .post(
            &format!("/api/sessions/{lane}/steer"),
            json!({"text": "rr0150 queued one"}),
        )
        .await;
    assert!((200..300).contains(&c), "steer enqueue 1: {c}");
    let (c, _) = rig
        .post(
            &format!("/api/sessions/{lane}/steer"),
            json!({"text": "rr0150 queued two"}),
        )
        .await;
    assert!((200..300).contains(&c), "steer enqueue 2: {c}");

    let (c, jrn) = rig
        .post("/api/journal", json!({"text": "rr0150 journal entry"}))
        .await;
    assert!((200..300).contains(&c), "journal create: {jrn}");
    let jrn_id = jrn["id"].as_str().expect("journal id").to_string();

    let (c, _) = rig
        .post(
            "/api/history",
            json!({"text": "rr0150 history line", "session": lane}),
        )
        .await;
    let history_written = (200..300).contains(&c);

    // Seeded tables (no API writer — see module doc).
    let now = chrono::Utc::now();
    rig.seed(
        "INSERT INTO _amux_conversations (worker_id, provider, conversation_ref, updated_at)
         VALUES (?1, 'claude', 'conv-rr0150', ?2)",
        &[&worker_id, &now.to_rfc3339()],
    );
    // A lease that has ALREADY expired: the question after restart is not
    // "is the row there" but "does the server still treat it as expired".
    rig.seed(
        "INSERT INTO _amux_leases (task_id, worker_id, acquired_at, expires_at, generation)
         VALUES ('tsk_rr0150_expired', ?1, ?2, ?3, 0)",
        &[
            &worker_id,
            &(now - chrono::Duration::hours(2)).to_rfc3339(),
            &(now - chrono::Duration::hours(1)).to_rfc3339(),
        ],
    );
    rig.seed(
        "INSERT INTO _amux_leases (task_id, worker_id, acquired_at, expires_at, generation)
         VALUES ('tsk_rr0150_live', ?1, ?2, ?3, 0)",
        &[&worker_id, &now.to_rfc3339(), &(now + chrono::Duration::hours(1)).to_rfc3339()],
    );
    let (_, metrics_before) = rig.get("/api/metrics").await;
    assert_eq!(metrics_before["leases"]["live"], 1, "expired RFC3339 leases must not appear live");
    assert_eq!(metrics_before["leases"]["total"], 2);
    // A 'running' transcode whose heartbeat is long stale — the exact state
    // migration 0009 exists to make survivable (python held this in memory,
    // so a restart orphaned it invisibly).
    let stale = now.timestamp() - 3600;
    rig.seed(
        "INSERT INTO _amux_media_jobs (key, src_path, out_path, status, progress, error, pid, created_at, updated_at)
         VALUES ('rr0150key', '/tmp/rr0150.mov', '/tmp/rr0150.mp4', 'running', 0.42, '', 4242, ?1, ?2)",
        &[&stale, &stale],
    );

    let (_, logs_before) = rig.get("/api/logs?limit=5").await;
    let reqlog_before = logs_before["total_matched"].as_i64().unwrap_or(0);

    // ---------------- the hard restart ----------------
    let h1 = rig.restart().await;
    let pid1 = h1["pid"].as_i64().unwrap_or(0);
    assert_ne!(pid0, pid1, "server did not actually restart (same pid)");
    assert_eq!(
        build0,
        h1["build"].as_str().unwrap_or(""),
        "build hash moved across the restart — a different binary answered, \
         so nothing measured across it is comparable (CLAUDE.md build rule)"
    );
    println!("restarted: pid {pid0} -> {pid1}, build {build0} unchanged\n");

    // ---------------- phase B: still there AND still functional ----------------
    let mut rep = Report::new();

    // 1. board — row survives, AND the status machine still runs. The
    //    transition is gated, so the check walks the SANCTIONED escape: read
    //    the criteria back off the 409 and re-PATCH with `gate_checked`. Two
    //    reasons it is done this way rather than with `force` or a hardcoded
    //    criteria list: `force` is the bypass whose whole point is that a
    //    named human took the judgment (ethos rule 6), and a hardcoded list
    //    silently stops testing the gate the moment the gate's wording moves.
    //    The criteria are answered honestly — for a suite-created card scope
    //    IS clear and the owner IS this suite.
    let (c, v) = rig.get(&format!("/api/board/{board_id}")).await;
    let survived = (200..300).contains(&c) && v["title"] == "rr0150 board row";
    let (mut pc, mut pv) = rig
        .patch(
            &format!("/api/board/{board_id}"),
            json!({
                "status": "doing",
                "next_action": "Prove the restarted board still accepts a lifecycle transition",
            }),
        )
        .await;
    let mut gate_note = String::new();
    if pc == 409 {
        let criteria = pv["gate"].as_array().cloned().unwrap_or_default();
        gate_note = format!(" · gate acked {criteria:?}");
        (pc, pv) = rig
            .patch(
                &format!("/api/board/{board_id}"),
                json!({
                    "status": "doing",
                    "gate_checked": criteria,
                    "next_action": "Prove the restarted board still accepts a lifecycle transition",
                }),
            )
            .await;
    }
    let (_, after) = rig.get(&format!("/api/board/{board_id}")).await;
    let functional = (200..300).contains(&pc) && after["status"] == "doing";
    rep.add(
        "board",
        survived && functional,
        format!(
            "read-back {c} title={} · PATCH->doing {pc} status={}{gate_note} {}",
            v["title"],
            after["status"],
            if !(200..300).contains(&pc) {
                format!("({pv})")
            } else {
                String::new()
            }
        ),
    );

    // 2. workers
    let (c, v) = rig.get(&format!("/api/workers/{worker_id}")).await;
    let survived = (200..300).contains(&c) && v["name"] == json!(worker_name.clone());
    let (lc, lv) = rig.get("/api/workers").await;
    let listed = lv["items"]
        .as_array()
        .or_else(|| lv.as_array())
        .map(|a| a.iter().any(|w| w["id"] == json!(worker_id.clone())))
        .unwrap_or(false);
    rep.add(
        "workers",
        survived && listed,
        format!(
            "read-back {c} name={} · list {lc} contains_id={listed}",
            v["name"]
        ),
    );

    // 3. schedules — the row, and the cron expression still parsing into a
    //    next_run (a schedule that survives but can never fire is not alive).
    let (c, v) = rig.get(&format!("/api/schedules/{sched_id}")).await;
    let row = v
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|s| s["id"] == json!(sched_id.clone()))
                .cloned()
        })
        .unwrap_or(v.clone());
    let survived = (200..300).contains(&c) && row["command"] == "noop";
    let next_ok = row["next_run"].is_string() || row["computed_next_run"].is_string();
    let (pc, _) = rig
        .patch(&format!("/api/schedules/{sched_id}"), json!({"enabled": 0}))
        .await;
    rep.add(
        "schedules",
        survived && next_ok && (200..300).contains(&pc),
        format!(
            "read-back {c} cmd={} · next_run before={} after={} · PATCH enabled=0 {pc}",
            row["command"], next_run_before, row["next_run"]
        ),
    );

    // 4. messages — the row, and the delivery state machine still advancing.
    let (c, v) = rig.get(&format!("/api/messages/{msg_id}")).await;
    let survived = (200..300).contains(&c) && v["body"] == "rr0150 message body";
    let (ac, av) = rig
        .post(&format!("/api/messages/{msg_id}/ack"), json!({}))
        .await;
    rep.add(
        "messages",
        survived && (200..300).contains(&ac),
        format!(
            "read-back {c} body_ok={} · ack {ac} delivery={}",
            v["body"] == "rr0150 message body",
            av["delivery"].clone()
        ),
    );

    // 5. steering queue — rows survive AND keep their queued_at ordering,
    //    which is what makes "oldest first, one per tick" mean anything.
    let (c, v) = rig.get(&format!("/api/sessions/{lane}/steer")).await;
    let all = v.as_array().cloned().unwrap_or_default();
    // The invariant here is the HUMAN steering queue surviving restart, in order.
    // Post-07424e3 a SYSTEM push (board-drive, schedules, the accountability
    // sweep — any non-empty guard except `selector-answer`) shares the
    // steering_queue table but is a SEPARATE surface. On a hard restart the
    // accountability sweep legitimately fires against this lane (it seeded an
    // unaccounted cmd_history message with no card) and lands a guarded system
    // row. Assert on the human subset only, filtered by the SAME guard rule the
    // server classifies with — so a system push that LEAKED as human (empty
    // guard when it should be guarded) would still FAIL here, not hide.
    let items: Vec<&Value> = all
        .iter()
        .filter(|i| {
            let g = i["guard"].as_str().unwrap_or("");
            g.is_empty() || g == "selector-answer"
        })
        .collect();
    let texts: Vec<&str> = items.iter().filter_map(|i| i["text"].as_str()).collect();
    let ordered = texts == vec!["rr0150 queued one", "rr0150 queued two"];
    let have_ts = items
        .iter()
        .all(|i| i["queued_at"].as_f64().unwrap_or(0.0) > 0.0);
    rep.add(
        "steering_queue",
        (200..300).contains(&c) && items.len() == 2 && ordered && have_ts,
        format!(
            "read-back {c} rows={} ordered={ordered} queued_at_preserved={have_ts}",
            items.len()
        ),
    );

    // 6. journal
    let (c, v) = rig.get(&format!("/api/journal/{jrn_id}")).await;
    let survived = (200..300).contains(&c) && v.to_string().contains("rr0150 journal entry");
    let (pc, _) = rig
        .patch(
            &format!("/api/journal/{jrn_id}"),
            json!({"text": "rr0150 journal edited"}),
        )
        .await;
    let (_, after) = rig.get(&format!("/api/journal/{jrn_id}")).await;
    rep.add(
        "journal",
        survived && (200..300).contains(&pc) && after.to_string().contains("rr0150 journal edited"),
        format!(
            "read-back {c} · PATCH {pc} · edit_visible={}",
            after.to_string().contains("rr0150 journal edited")
        ),
    );

    // 7. request log — entries written BEFORE the kill are still counted.
    let (_, v) = rig.get("/api/logs?limit=5").await;
    let after_total = v["total_matched"].as_i64().unwrap_or(0);
    rep.add(
        "request_log",
        after_total >= reqlog_before && reqlog_before > 0,
        format!("total_matched before_kill={reqlog_before} after_restart={after_total}"),
    );

    // 8. cmd history
    let (c, v) = rig
        .get(&format!("/api/history?limit=200&session={lane}"))
        .await;
    let found = v
        .as_array()
        .map(|a| {
            a.iter().any(|r| {
                r["text"]
                    .as_str()
                    .unwrap_or("")
                    .contains("rr0150 history line")
            })
        })
        .unwrap_or(false);
    rep.add(
        "cmd_history",
        !history_written || ((200..300).contains(&c) && found),
        format!("POST accepted={history_written} · read-back {c} found={found}"),
    );

    // 9. conversations (seeded) — bootstrap re-hydrates protocol conversation
    //    refs from here; an in-memory-only ref is fiction across an exec.
    rep.add(
        "conversations",
        rig.count("SELECT COUNT(*) FROM _amux_conversations WHERE conversation_ref='conv-rr0150'")
            == 1,
        "seeded row survived (NO API surface: written only by ConversationSink, \
         read only by backend::bootstrap — not verifiable through the API)"
            .into(),
    );

    // 10. leases (seeded) — the row survives AND is still EXPIRED. A lease
    //     whose expiry resets on restart would let two workers hold one task.
    //
    //     COMPARE RFC3339 AGAINST RFC3339. The first version of this probe
    //     used `expires_at > datetime('now')` and reported a one-hour-old
    //     lease as live: `expires_at` is RFC3339 ("2026-08-10T01:25:54+00:00")
    //     while `datetime('now')` yields "2026-08-10 02:25:54" — SQLite
    //     compares them as TEXT, and 'T' (0x54) sorts above ' ' (0x20), so the
    //     predicate is true for EVERY row regardless of time. It produced a
    //     confident red against correct code (ethos rule 7: the instrument is
    //     a candidate before the code is). Both sides are RFC3339 now, so the
    //     lexicographic order really is chronological.
    let now_rfc = chrono::Utc::now().to_rfc3339();
    let unexpired = {
        let conn = rusqlite::Connection::open(&rig.db).unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM _amux_leases WHERE task_id='tsk_rr0150_expired' AND expires_at > ?1",
            [&now_rfc],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(-1)
    };
    let present = rig.count("SELECT COUNT(*) FROM _amux_leases WHERE task_id='tsk_rr0150_expired'");
    let (_, m) = rig.get("/api/metrics").await;
    rep.add(
        "leases",
        present == 1 && unexpired == 0 && m["leases"]["live"] == 1 && m["leases"]["total"] == 2,
        format!("expired_row_present={present} still_expired={} · live={} total={} (one future lease)",
            unexpired == 0, m["leases"]["live"], m["leases"]["total"]),
    );

    // 11. media jobs (seeded) — the stale 'running' row survives, so the next
    //     poll can decide it is stale and restart it. Python kept this in
    //     memory and orphaned it invisibly on every restart.
    let job_status: String = {
        let conn = rusqlite::Connection::open(&rig.db).unwrap();
        conn.query_row(
            "SELECT status FROM _amux_media_jobs WHERE key='rr0150key'",
            [],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "<missing>".into())
    };
    rep.add(
        "media_jobs",
        job_status == "running",
        format!(
            "stale running job survived: status={job_status} \
                 (end-to-end restart-on-stale needs ffmpeg + a media fixture — not covered here)"
        ),
    );

    rep.finish();
}

/// The rig itself must be able to fail. If `wait_healthy` accepted a dead
/// server, or `kill` did not actually kill, every verdict above would be
/// theatre. Both are asserted directly.
#[tokio::test(flavor = "multi_thread")]
async fn the_rig_can_tell_a_dead_server_from_a_live_one() {
    let mut rig = Rig::new();
    rig.spawn();
    let h = rig.wait_healthy().await;
    assert_eq!(h["status"], "ok");

    rig.kill();
    // After kill the port must refuse — proving the "restart" in the suite
    // above is a real process replacement and not a no-op that left the
    // original server answering.
    let refused = rig.client.get(rig.url("/health")).send().await.is_err();
    assert!(
        refused,
        "server still answered after kill — the restart in this suite proves nothing"
    );
}

/// Recovery is more than retaining table rows: pending observations are replayed,
/// uncertain side effects stay uncertain, finished reviewers are adopted, and
/// their full evidence remains after disposable checkout cleanup.
#[tokio::test]
async fn interrupted_harness_work_recovers_without_duplicate_effects_or_false_passes() {
    recover_completed_review("0", "fail", false).await;
    recover_completed_review("37", "pass", false).await;
    recover_completed_review("0", "fail", true).await;
}

async fn recover_completed_review(exit: &str, verdict: &str, live: bool) {
    let mut rig = Rig::new();
    // A cached completion review must be adopted. Advisory plan reviews after
    // reopening are a distinct lifecycle step; count them separately.
    let repo = rig.home.join("fixture-repo");
    std::fs::create_dir_all(&repo).unwrap();
    for args in [vec!["init", "-q"], vec!["-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-qm", "fixture"], vec!["update-ref", "refs/remotes/origin/main", "HEAD"]] {
        assert!(Command::new("git").arg("-C").arg(&repo).args(args).status().unwrap().success());
    }
    let sha = String::from_utf8(Command::new("git").arg("-C").arg(&repo).args(["rev-parse", "HEAD"]).output().unwrap().stdout).unwrap().trim().to_string();
    let lane = uniq("rr-recovery");
    let card = "RR-RECOVERY";
    std::fs::create_dir_all(rig.home.join("sessions")).unwrap();
    let forbidden = rig.home.join("must-not-rerun-review");
    let cli = rig.home.join("review-cli.sh");
    let advisory = rig.home.join("advisory-plan-review");
    let advisory_script = format!("#!/bin/sh\ncase \"$PWD\" in *'/RR-RECOVERY-pre-'*) echo advisory >> '{}'; echo '{{\"verdict\":\"gaps\",\"findings\":[\"missing required measurement\"]}}'; exit 0;; esac\n", advisory.display());
    let completion_script = if live {
        format!("#!/bin/sh\necho launch >> '{}'\nsleep 20\necho '{{\"verdict\":\"fail\",\"findings\":[\"missing required measurement\"]}}'\nexit 0\n", forbidden.display())
    } else { format!("#!/bin/sh\necho launch >> '{}'\nexit 1\n", forbidden.display()) };
    std::fs::write(&cli, advisory_script + &completion_script.replace("#!/bin/sh\n", "")).unwrap();
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap(); }
    std::fs::write(rig.home.join(format!("sessions/{lane}.env")), format!("CC_DIR={}\nCC_NAME={lane}\nCC_ISOLATED=1\nAMUX_CONTRACT_DONE=1\nAMUX_CONTRACT_REVIEW_CLI={}\n", repo.display(), cli.display())).unwrap();
    rig.spawn();
    let before = rig.wait_healthy().await;
    // Create through the API before the crash, then seed only the internal
    // execution states for which there is no API writer.
    let (status, schedule) = rig.post("/api/schedules", json!({"title":"Interrupted shell", "kind":"shell", "command":"exit 0", "schedule_expr":"daily at 3am", "enabled":0})).await;
    assert_eq!(status, 201, "{schedule}");
    let sid = schedule["id"].as_str().unwrap();
    // These internal execution states and detached-review artifacts form one
    // crash specimen. Seed them while down; a live contract clock could consume
    // the artificial running intent before its cached outcome is on disk.
    rig.kill();
    rig.seed("INSERT INTO schedule_runs(schedule_id,ran_at,status,source,delivery,note) VALUES(?1,1,'running','manual:fixture','shell','execution interrupted')", &[&sid]);
    rig.seed("INSERT INTO schedule_runs(schedule_id,ran_at,status,source,delivery,note) VALUES(?1,1,'running','cron-rs',NULL,'delivery interrupted')", &[&sid]);
    rig.seed("INSERT INTO steering_queue(id,session,text,queued_at,delivering_since) VALUES('rr-claimed',?1,'uncertain delivery',1,1)", &[&lane]);
    rig.seed("INSERT INTO steering_queue(id,session,text,queued_at) VALUES('rr-waiting',?1,'never attempted',2)", &[&lane]);
    rig.seed("INSERT INTO issues(id,title,type,status,session,created,updated,evidence,desc) VALUES(?1,'Recovered independent review','ops','done',?2,1,1,'recorded server check','original full measurement requirements')", &[&card,&lane]);
    rig.seed("INSERT INTO card_contracts(card,acceptance,command,hash,frozen_at,state,sha,review_state,review_at) VALUES(?1,'original criteria','exit 0','fixed-hash',1,'passed',?2,'running',1)", &[&card,&sha]);
    let conn = rusqlite::Connection::open(&rig.db).unwrap();
    let frozen = amux_server::api::contract::load(&conn, card).unwrap().unwrap();
    let row = amux_server::db::board_store::get_issue(&conn, card).unwrap().unwrap();
    let input = amux_server::api::contract::review_input_hash(&frozen, &row, 1);
    drop(conn);
    let review = rig.home.join("tmp/contract").join(format!("{card}-review-{}-{}", &sha[..12], &input[..16]));
    if live {
        // Start with the seeded intent present. The normal contract clock is
        // five minutes; inserting after its first poll cannot launch in 45s.
        rig.restart().await;
        let deadline = Instant::now() + Duration::from_secs(45);
        while !forbidden.exists() && Instant::now() < deadline { tokio::time::sleep(Duration::from_millis(100)).await; }
        assert!(forbidden.exists(), "the real detached review must launch before the crash: {}", std::fs::read_to_string(&rig.log).unwrap_or_default());
        assert!(review.join(".amux-review.pid").exists(), "launched reviewer PID is durable");
    } else {
        assert!(Command::new("git").arg("-C").arg(&repo).args(["worktree", "add", "--detach"]).arg(&review).arg(&sha).status().unwrap().success());
        std::fs::write(review.join(".amux-review.out"), json!({"verdict":verdict,"findings":["missing required measurement"]}).to_string()).unwrap();
        std::fs::write(review.join(".amux-review.exit"), exit).unwrap();
        std::fs::write(review.join("card-source.md"), "original full measurement requirements").unwrap();
        std::fs::write(review.join(".amux-review.prompt"), "original immutable review prompt").unwrap();
    }
    rig.kill();
    assert!(rig.client.get(rig.url("/health")).send().await.is_err());
    if live {
        let pid = std::fs::read_to_string(review.join(".amux-review.pid")).unwrap();
        assert!(Command::new("kill").args(["-0",pid.trim()]).status().unwrap().success(), "reviewer outlives the server PID");
    }
    // Provider hooks can arrive while the server is down. They must remain
    // durable locally and be consumed by the next image without new prompts.
    let run = "deadbeef";
    let native = rig.home.join("status-events").join(&lane);
    std::fs::create_dir_all(&native).unwrap();
    let now = chrono::Utc::now().timestamp() as f64;
    std::fs::write(native.join("current.json"), json!({"run_id":run,"provider":"claude","started":now-10.0}).to_string()).unwrap();
    for event in ["UserPromptSubmit", "Stop"] {
        use std::io::Write;
        let mut hook = Command::new("python3").arg(rig.home.join("native-status.py")).arg("claude")
            .env("AMUX_STATUS_HOME", &rig.home).env("AMUX_STATUS_WORKER", &lane).env("AMUX_STATUS_RUN_ID", run)
            .env("AMUX_STATUS_URL", rig.url("")).stdin(std::process::Stdio::piped()).spawn().unwrap();
        hook.stdin.take().unwrap().write_all(json!({"hook_event_name":event,"turn_id":"fixture-turn"}).to_string().as_bytes()).unwrap();
        assert!(hook.wait().unwrap().success());
    }
    assert_eq!(std::fs::read_dir(native.join(run)).unwrap().filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == "json") && e.as_ref().unwrap().file_name() != "counter.json").count(), 2);
    // Installation drift is repaired by passive recovery, not by restarting
    // twenty workers or injecting a new context into an isolated worker.
    std::fs::write(rig.home.join("native-status.py"), "obsolete observer").unwrap();
    rig.spawn();
    let after = rig.wait_healthy().await;
    assert_ne!(before["pid"], after["pid"]);
    assert_eq!(before["build"], after["build"]);
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        let events = rig.count("SELECT COUNT(*) FROM session_events WHERE type='session.native_status' AND json_extract(data,'$.run_id')='deadbeef'");
        let unfinished = rig.count("SELECT COUNT(*) FROM schedule_runs WHERE status='running'");
        let failed_review = rig.count("SELECT COUNT(*) FROM card_contracts WHERE card='RR-RECOVERY' AND review_state='failed'");
        if events == 2 && unfinished == 0 && failed_review == 1 { break; }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(rig.count("SELECT COUNT(*) FROM session_events WHERE type='session.native_status' AND json_extract(data,'$.run_id')='deadbeef'"), 2);
    assert_eq!(rig.count("SELECT COUNT(*) FROM schedule_runs WHERE status='error'"), 2, "unknown execution and delivery must not become success");
    assert_eq!(rig.count("SELECT COUNT(*) FROM steering_queue WHERE id='rr-waiting'"), 1, "unattempted owner input survives");
    assert_eq!(rig.count("SELECT COUNT(*) FROM steering_history WHERE id='rr-claimed' AND outcome LIKE 'interrupted%'"), 1, "uncertain delivery remains explicit");
    assert_eq!(rig.count("SELECT COUNT(*) FROM card_contracts WHERE card='RR-RECOVERY' AND review_state='failed'"), 1);
    assert_eq!(rig.count("SELECT COUNT(*) FROM issues WHERE id='RR-RECOVERY' AND status='verified'"), 0);
    let conn = rusqlite::Connection::open(&rig.db).unwrap();
    let recovered = amux_server::api::contract::load(&conn, card).unwrap().unwrap();
    let recovered_row = amux_server::db::board_store::get_issue(&conn, card).unwrap().unwrap();
    let recovered_input = amux_server::api::contract::review_input_hash(&recovered, &recovered_row, 1);
    assert_eq!(std::fs::read_to_string(&forbidden).unwrap_or_default().lines().count(), usize::from(live),
        "review must be adopted, never duplicated (live={live}, exit={exit}, seeded_input={input}, recovered_input={recovered_input}, cached_path={}); server log: {}",
        review.display(), std::fs::read_to_string(&rig.log).unwrap_or_default());
    drop(conn);
    assert_eq!(rig.count("SELECT review_rounds FROM card_contracts WHERE card='RR-RECOVERY'"), 1, "a completed failed/unmeasured attempt spends one bounded round");
    if exit != "0" {
        let conn = rusqlite::Connection::open(&rig.db).unwrap();
        let log: String = conn.query_row("SELECT review_log FROM card_contracts WHERE card='RR-RECOVERY'", [], |r| r.get(0)).unwrap();
        assert!(log.contains("no trustworthy verdict") && log.contains("37"), "{log}");
    }
    assert!(std::fs::read_to_string(rig.home.join("native-status.py")).unwrap().contains("X-Amux-Worker-Token"));
    let archives = rig.home.join("review-evidence").join(card);
    // Reopening also creates an advisory `pre-...` archive. Directory order
    // cannot identify the completion review whose immutable inputs we seeded.
    let generation = format!("{sha}-1-");
    let candidates: Vec<_> = std::fs::read_dir(&archives).unwrap()
        .map(|entry| entry.unwrap().path()).collect();
    let matching: Vec<_> = candidates.iter().filter(|path| {
        path.file_name().unwrap().to_string_lossy().starts_with(&generation)
    }).collect();
    eprintln!("review_archive_selection: measured=true n_considered={} generation={generation} matching={} candidates={candidates:?}", candidates.len(), matching.len());
    assert_eq!(matching.len(), 1, "exactly one retained completion review for {generation}; all archives: {candidates:?}");
    let artifact = matching[0];
    assert!(std::fs::read_to_string(artifact.join(".amux-review.out")).unwrap().contains("missing required measurement"));
    assert_eq!(std::fs::read_to_string(artifact.join(".amux-review.exit")).unwrap().trim(), exit);
    assert_eq!(std::fs::read_to_string(artifact.join("card-source.md")).unwrap(), "original full measurement requirements");
    let prompt = std::fs::read_to_string(artifact.join(".amux-review.prompt")).unwrap();
    if live { assert!(prompt.contains("original criteria") && prompt.contains("card-source.md")); }
    else { assert_eq!(prompt, "original immutable review prompt"); }
    rig.restart().await;
    assert_eq!(rig.count("SELECT COUNT(*) FROM schedule_runs WHERE status='error'"), 2);
    assert_eq!(rig.count("SELECT COUNT(*) FROM session_events WHERE type='session.native_status' AND json_extract(data,'$.run_id')='deadbeef'"), 2);
    assert_eq!(std::fs::read_to_string(&forbidden).unwrap_or_default().lines().count(), usize::from(live));
}

/// A real ffmpeg consumer must repair durable stale intent and lost output.
/// The interrupted heartbeat is seeded explicitly; the resumed consumer, TLS
/// polls, output and subsequent cache adoption are real, not metadata-only.
#[tokio::test]
async fn stale_media_intent_and_lost_output_recover_with_a_real_consumer() {
    let ffmpeg = ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"].into_iter()
        .find(|p| std::path::Path::new(p).is_file());
    let Some(ffmpeg) = ffmpeg else {
        assert_ne!(std::env::var("AMUX_REQUIRE_MEDIA_RECOVERY").as_deref(), Ok("1"), "required real ffmpeg recovery cannot run");
        eprintln!("UNMEASURED media consumer recovery: ffmpeg unavailable");
        return;
    };
    let mut rig = Rig::new();
    let src = rig.home.join("fixture-home/recovery.mkv");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    let generated = Command::new(ffmpeg).args(["-y", "-f", "lavfi", "-i", "testsrc=duration=0.5:size=64x64:rate=10", "-f", "lavfi", "-i", "sine=frequency=440:duration=0.5", "-shortest", "-c:v", "libx264", "-c:a", "aac"]).arg(&src).output().unwrap();
    assert!(generated.status.success(), "fixture generation: {}", String::from_utf8_lossy(&generated.stderr));
    let uri = format!("/api/file/prepare?path={}", src.to_string_lossy().bytes().map(|b| format!("%{b:02X}")).collect::<String>());
    rig.spawn();
    rig.wait_healthy().await;
    let (status, first) = rig.get(&uri).await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["started"], true, "{first}");
    async fn completed(rig: &Rig, uri: &str) -> PathBuf {
        let deadline = Instant::now() + Duration::from_secs(45);
        while Instant::now() < deadline {
            let (status, result) = rig.get(uri).await;
            assert_eq!(status, 200, "{result}");
            assert!(result.get("error").is_none(), "{result}");
            if result["ready"] == true {
                let path = PathBuf::from(result["cached_path"].as_str().unwrap());
                assert!(std::fs::metadata(&path).unwrap().len() > 0, "ready requires actual output");
                assert_eq!(rig.count("SELECT COUNT(*) FROM _amux_media_jobs WHERE status='done' AND progress=100"), 1);
                return path;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("real media consumer did not finish: {}", std::fs::read_to_string(&rig.log).unwrap_or_default());
    }
    let out = completed(&rig, &uri).await;
    // The seeded stale heartbeat reproduces a lost task without waiting an hour.
    rig.seed("UPDATE _amux_media_jobs SET status='running', progress=42, updated_at=?1", &[&(chrono::Utc::now().timestamp()-3600).to_string()]);
    rig.kill();
    std::fs::remove_file(&out).unwrap();
    rig.spawn(); rig.wait_healthy().await;
    let (_, restarted) = rig.get(&uri).await;
    assert_eq!(restarted["started"], true, "stale intent must restart its consumer: {restarted}");
    assert_eq!(completed(&rig, &uri).await, out);
    rig.restart().await;
    let (_, cached) = rig.get(&uri).await;
    assert_eq!(cached["ready"], true, "completed output must be adopted: {cached}");
    assert!(cached.get("started").is_none());
    rig.kill();
    std::fs::remove_file(&out).unwrap();
    rig.spawn(); rig.wait_healthy().await;
    let (_, lost) = rig.get(&uri).await;
    assert_eq!(lost["started"], true, "a done row without its output cannot grant ready: {lost}");
    assert_eq!(completed(&rig, &uri).await, out);
}

#[tokio::test]
async fn acknowledged_connector_state_survives_sigkill_without_lost_concurrent_updates() {
    let mut rig = Rig::new();
    rig.spawn(); rig.wait_healthy().await;
    let creates = (0..16).map(|i| rig.post("/api/connectors", json!({
        "id": format!("rr-connector-{i}"), "label": format!("Restart fixture {i}"),
        "kind": "api_key", "key_env": format!("RR_CONNECTOR_{i}_KEY")
    })));
    for (status, body) in futures::future::join_all(creates).await { assert_eq!(status, 200, "{body}"); }
    let writes = (0..16).map(|i| {
        let client = rig.client.clone(); let url = rig.url(&format!("/api/connectors/rr-connector-{i}/credentials"));
        async move {
            let response = client.post(url).json(&json!({format!("RR_CONNECTOR_{i}_KEY"): "fixture-only"})).send().await.unwrap();
            assert_eq!(response.status(), 200);
        }
    });
    futures::future::join_all(writes).await;
    assert_eq!(rig.post("/api/connectors/google-drive/credentials", json!({"GOOGLE_OAUTH_CLIENT_ID":"fixture-client", "GOOGLE_OAUTH_CLIENT_SECRET":"fixture-secret"})).await.0, 200);
    rig.restart().await; // SIGKILL, not graceful flush.
    let (status, list) = rig.get("/api/connectors").await; assert_eq!(status, 200);
    let rows = list["connectors"].as_array().unwrap();
    for i in 0..16 {
        let id = format!("rr-connector-{i}"); let row = rows.iter().find(|r| r["id"] == id).unwrap();
        assert_eq!(row["status"], "connected"); assert_eq!(row["env_keys"][0]["set"], true);
    }
    let values = amux_server::config::parse_env_file(&rig.home.join("server.env"));
    for i in 0..16 { assert_eq!(values.get(&format!("RR_CONNECTOR_{i}_KEY")).map(String::as_str), Some("fixture-only")); }
    assert_eq!(values.get("GOOGLE_OAUTH_CLIENT_SECRET").map(String::as_str), Some("fixture-secret"));
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        for relative in ["server.env", "connectors/custom.json"] {
            assert_eq!(std::fs::metadata(rig.home.join(relative)).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
    assert!(!list.to_string().contains("fixture-only"), "inventory never returns a credential");
}

/// Real TLS broker -> local OAuth fixture -> durable rotation -> SIGKILL ->
/// TLS broker. The fixture is an actual HTTP server, never a live provider.
#[cfg(unix)]
#[tokio::test]
async fn committed_oauth_rotation_is_served_after_sigkill() {
    use axum::{extract::Form, http::StatusCode, Json};
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let fixture_calls = calls.clone();
    let endpoint = axum::Router::new().route("/token", axum::routing::post(move |Form(form): Form<std::collections::HashMap<String,String>>| {
        let calls = fixture_calls.clone();
        async move {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n != 0 || form.get("refresh_token").map(String::as_str) != Some("old-refresh") || form.get("grant_type").map(String::as_str) != Some("refresh_token") {
                return (StatusCode::BAD_REQUEST, Json(json!({"error":"invalid_grant"})));
            }
            (StatusCode::OK, Json(json!({"access_token":"committed-access","refresh_token":"rotated-refresh","expires_in":3600})))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}/token", listener.local_addr().unwrap());
    let provider = tokio::spawn(async move {
        axum::serve(listener, endpoint).await.unwrap();
    });
    let mut rig = Rig::permission_fault_fixture();
    rig.no_external_probes = true;
    let account = "fixture@example.com";
    let canonical = rig
        .home
        .join("connectors/google")
        .join(format!("{account}.json"));
    let mirror = rig
        .home
        .join("gmail-tokens")
        .join(format!("{account}.json"));
    std::fs::create_dir_all(canonical.parent().unwrap()).unwrap();
    std::fs::create_dir_all(mirror.parent().unwrap()).unwrap();
    let old = json!({"token":"old-access","refresh_token":"old-refresh","client_id":"fixture-client","client_secret":"fixture-secret","token_uri":uri,"scopes":"https://www.googleapis.com/auth/drive https://www.googleapis.com/auth/gmail.modify","expires_at":0});
    std::fs::write(&canonical, old.to_string()).unwrap();
    std::fs::write(&mirror, old.to_string()).unwrap();
    rig.prepare_permission_fault_server();
    rig.spawn();
    rig.wait_healthy().await;
    // Make the compatibility write fail after the canonical commit. Both the
    // old copy and its fingerprint survive; readers must use the new grant.
    std::fs::set_permissions(
        mirror.parent().unwrap(),
        std::fs::Permissions::from_mode(0o500),
    )
    .unwrap();
    let response = rig
        .client
        .post(rig.url("/api/connectors/google-drive/token?account=fixture%40example.com"))
        .send()
        .await;
    std::fs::set_permissions(
        mirror.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let response = response.unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["access_token"], "committed-access");
    assert_eq!(body["source"], "user-grant (refreshed)");
    let committed: Value = serde_json::from_slice(&std::fs::read(&canonical).unwrap()).unwrap();
    assert_eq!(committed["refresh_token"], "rotated-refresh");
    assert_eq!(
        committed["gmail_mirror_refresh_sha256"],
        hex::encode(Sha256::digest(b"old-refresh"))
    );
    let stale: Value = serde_json::from_slice(&std::fs::read(&mirror).unwrap()).unwrap();
    assert_eq!(
        stale["refresh_token"], "old-refresh",
        "the failed mirror write must actually be exercised"
    );
    rig.restart().await;
    let response = rig
        .client
        .post(rig.url("/api/connectors/google-drive/token?account=fixture%40example.com"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["access_token"], "committed-access");
    assert_eq!(body["source"], "user-grant (stored)");
    let retained: Value = serde_json::from_slice(&std::fs::read(&canonical).unwrap()).unwrap();
    assert_eq!(retained["refresh_token"], "rotated-refresh");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "restart must adopt the committed refresh without replaying it"
    );
    assert!(
        std::fs::read_to_string(&rig.log)
            .unwrap()
            .contains("connector_gmail_mirror_deferred"),
        "the recoverable copy failure must self-announce"
    );
    rig.kill();
    provider.abort();
    let _ = provider.await;
}
