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
        let tmp = tempfile::tempdir().expect("tempdir");
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
            _tmp: tmp,
        }
    }

    fn spawn(&mut self) {
        let out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .expect("log");
        let err = out.try_clone().unwrap();
        let child = Command::new(server_bin())
            .env_clear()
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("AMUX_HOME", &self.home)
            .env("AMUX_DB", &self.db)
            .env("AMUX_RS_PORT", self.port.to_string())
            // Auth off: this is a loopback-only temp server.
            .env("AMUX_AUTH_TOKEN", "none")
            // Bootstrap would try to give a created worker a real terminal.
            // Nothing here starts a worker; push it out of the way anyway.
            .env("AMUX_RS_BOOTSTRAP_SECS", "3600")
            .env("RUST_LOG", "warn")
            .stdout(out)
            .stderr(err)
            .spawn()
            .expect("spawn server");
        self.child = Some(child);
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
    // NOTE (reported, not asserted here): /api/metrics reports
    // `leases.live` as `SELECT COUNT(*) FROM _amux_leases` — no expiry
    // predicate — so an expired lease is still counted as live. That is a
    // labelling defect in the metric, not a restart-persistence failure, so
    // it is surfaced in the detail rather than failing this subsystem.
    rep.add(
        "leases",
        present == 1 && unexpired == 0,
        format!(
            "row_present={present} still_expired={} · /api/metrics leases.live={} \
                 (metric counts ALL lease rows, expired included — no expiry predicate)",
            unexpired == 0,
            m["leases"]["live"]
        ),
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
