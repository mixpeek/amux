//! AF-784: a retained dead pane must not leave the typed worker API idle/running.
//! Only a fresh worker-shaped tmux session and a temporary SQLite store are mutated.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use amux_core::ids::{SessionId, WorkerId};
use amux_core::provider::ProviderId;
use amux_core::session::BackendId;
use amux_core::worker::{WorkerConfig, WorkerState};
use amux_server::api::{router, AppState};
use amux_server::backend::{
    backend_ref, tmux::TmuxBackend, BackendStatus, SessionBackend, SessionSpec,
};
use amux_server::db::queries::{self, SessionRow, WorkerRow};
use amux_server::db::{SharedStore, Store, WriteOutcome};
use amux_server::orchestrator::scan::ScanLoop;
use axum::body::{to_bytes, Body};
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;

async fn body(app: &axum::Router, worker: &WorkerId) -> Value {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/workers/{worker}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_success());
    serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
}

/// AMUX-4636: what tmux and the OS know about a pane tmux calls dead with no
/// exit status. The first CI diagnostic showed the command ran (its text is in
/// the pane) and exited, yet tmux reported neither pane_dead_status nor
/// pane_dead_signal. Whether tmux reaped the process, and whether its server
/// can receive SIGCHLD at all, separates the candidate causes. Each probe
/// reports its own failure inline, so a missing tool reads as a failure rather
/// than as an empty answer.
fn dead_pane_host_evidence(backend_ref: &str) -> String {
    fn run(args: &[&str]) -> String {
        match std::process::Command::new(args[0])
            .args(&args[1..])
            .output()
        {
            Ok(o) => format!(
                "{}{}(exit {:?})",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr),
                o.status.code()
            ),
            Err(e) => format!("<{} failed: {e}>", args[0]),
        }
    }
    let target = format!("={backend_ref}:");
    let panes = run(&[
        "tmux", "list-panes", "-t", &target, "-F",
        "dead=#{pane_dead} status=#{pane_dead_status} signal=#{pane_dead_signal} time=#{pane_dead_time} pane_pid=#{pane_pid} server_pid=#{pid} version=#{version}",
    ]);
    let mut out = format!(
        "tmux -V: {}\nlist-panes: {}\n",
        run(&["tmux", "-V"]).trim(),
        panes.trim()
    );
    let field = |key: &str| {
        panes
            .split_whitespace()
            .find_map(|w| w.strip_prefix(key))
            .unwrap_or("")
            .to_string()
    };
    let pane_pid = field("pane_pid=");
    let server_pid = field("server_pid=");
    if !pane_pid.is_empty() {
        let ps = run(&["ps", "-o", "pid,ppid,stat,args", "-p", &pane_pid]);
        out.push_str(&format!(
            "ps pane_pid (a Z row means tmux never reaped it): {}\n",
            ps.trim()
        ));
    }
    if !server_pid.is_empty() {
        let ps = run(&["ps", "-o", "pid,ppid,stat,args", "-p", &server_pid]);
        out.push_str(&format!("ps server: {}\n", ps.trim()));
        match std::fs::read_to_string(format!("/proc/{server_pid}/status")) {
            Ok(status) => {
                // SigBlk/SigIgn/SigCgt are hex masks; SIGCHLD is signal 17 on
                // Linux, bit 0x10000.
                for line in status
                    .lines()
                    .filter(|l| l.starts_with("Sig") || l.starts_with("Shd"))
                {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            Err(e) => out.push_str(&format!("/proc/{server_pid}/status: <unreadable: {e}>\n")),
        }
        let children = run(&["ps", "-o", "pid,ppid,stat,args", "--ppid", &server_pid]);
        out.push_str(&format!("children of server: {}\n", children.trim()));
    }
    out
}

#[tokio::test]
async fn retained_dead_pane_cannot_remain_idle_in_worker_api() {
    let tmux = std::process::Command::new("tmux").arg("-V").output();
    if !tmux.is_ok_and(|r| r.status.success()) {
        assert_ne!(
            std::env::var("AMUX_REQUIRE_TMUX").ok().as_deref(),
            Some("1"),
            "tmux required for this positive specimen"
        );
        eprintln!("SKIPPED: dead-pane API specimen; tmux unavailable");
        return;
    }
    // Process-scoped isolation: never mutate the parent test process's env or
    // let this specimen attach to the developer/fleet tmux socket.
    if std::env::var_os("AMUX_DEAD_PANE_TEST_CHILD").is_none() {
        let socket_dir = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "retained_dead_pane_cannot_remain_idle_in_worker_api",
                "--nocapture",
            ])
            .env_remove("TMUX")
            .env_remove("ENV")
            .env_remove("BASH_ENV")
            .env("TMUX_TMPDIR", socket_dir.path())
            .env("AMUX_DEAD_PANE_TEST_CHILD", "1")
            .env("AMUX_REQUIRE_TMUX", "1")
            .output()
            .unwrap();
        // This server belongs exclusively to the newly minted private dir,
        // including when an assertion in the child failed before cleanup.
        let _ = std::process::Command::new("tmux")
            .arg("kill-server")
            .env_remove("TMUX")
            .env("TMUX_TMPDIR", socket_dir.path())
            .output();
        eprintln!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success(),
            "isolated dead-pane specimen failed"
        );
        return;
    }
    // A private socket alone still loads ~/.tmux.conf and a login shell's
    // profile. Slow host startup can leave the typed fixture command unexecuted
    // for the entire observation window. This specimen tests exit detection,
    // not the developer's shell initialization; use a private minimal server.
    let bootstrap = std::process::Command::new("tmux")
        .args([
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "dead-pane-fixture",
            "/bin/sh",
        ])
        .status()
        .unwrap();
    assert!(bootstrap.success(), "private fixture server must start");
    for (key, value) in [
        ("default-shell", "/bin/sh"),
        ("default-command", "exec /bin/sh"),
    ] {
        assert!(std::process::Command::new("tmux")
            .args(["set-option", "-g", key, value])
            .status()
            .unwrap()
            .success());
    }
    let dir = tempfile::tempdir().unwrap();
    let store: SharedStore = Arc::new(Store::open(&dir.path().join("probe.db")).unwrap());
    let worker = WorkerId::from_ulid(ulid::Ulid::new());
    let reference = backend_ref(&worker);
    assert!(reference.starts_with("amux-wrk_"));
    let flag = dir.path().join("exit-now");
    let backend = Arc::new(TmuxBackend::new());
    let proc = backend.spawn(&SessionSpec {
        worker: worker.clone(),
        command: vec!["sh".into(), "-c".into(), "n=0; while [ ! -f \"$AMUX_DEAD_PANE_FLAG\" ] && [ $n -lt 300 ]; do sleep 0.1; n=$((n+1)); done; printf '\\033[2J\\033[3J\\033[H%s\\n' '--dangerously-skip-permissions cannot be used with root/sudo privileges for security reasons'; exit 1".into()],
        cwd: dir.path().to_string_lossy().into_owned(),
        env: BTreeMap::from([("AMUX_DEAD_PANE_FLAG".into(), flag.to_string_lossy().into_owned())]),
        human_label: None,
    }).await.unwrap();

    // Catch assertion failures so our exact process is cleaned up before reporting.
    let checked = tokio::spawn({
        let backend = backend.clone();
        let proc = proc.clone();
        let worker = worker.clone();
        let store = store.clone();
        let cwd = dir.path().to_string_lossy().into_owned();
        async move {
            assert_eq!(backend.status(&proc).await.unwrap(), BackendStatus::Running);
            let socket = std::process::Command::new("tmux")
                .args(["display-message", "-p", "-t", &format!("={}:", proc.backend_ref), "#{socket_path}"])
                .output().unwrap();
            assert!(socket.status.success());
            let socket_text = String::from_utf8_lossy(&socket.stdout);
            let actual_socket = std::fs::canonicalize(socket_text.trim()).unwrap();
            let expected_directory = std::fs::canonicalize(std::env::var("TMUX_TMPDIR").unwrap()).unwrap();
            assert!(actual_socket.starts_with(&expected_directory),
                "specimen socket {actual_socket:?} must be under {expected_directory:?}");
            eprintln!("AF-784 private_socket={actual_socket:?}");
            let now = chrono::Utc::now();
            let config = WorkerConfig { worker_type: Default::default(), display_name: worker.to_string(), name_aliases: vec![], cwd,
                provider: ProviderId::new("claude"), model: None, backend: BackendId::tmux(),
                environment: BTreeMap::new(), permissions: vec![], group: None };
            let row = WorkerRow::new(&worker, &config, &now.to_rfc3339());
            let wid = worker.clone();
            let reference = proc.backend_ref.clone();
            store.write_async(move |conn| {
                queries::insert_worker(conn, &row)?;
                queries::update_worker_state(conn, wid.as_str(), &WorkerState::Idle { since: now }, &now.to_rfc3339())?;
                queries::insert_session(conn, &SessionRow { id: SessionId::from_ulid(ulid::Ulid::new()).to_string(),
                    worker_id: wid.to_string(), backend: "tmux".into(), backend_ref: reference,
                    pid: None, started_at: now.to_rfc3339(), ended_at: None, exit_reason: None })?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            }).await.unwrap();
            let app = router(AppState { store: store.clone(), started: Instant::now(), build_hash: "AF-784-specimen".into(), auth_token: None,
                reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)) });
            let before = body(&app, &worker).await;
            assert_eq!(before["running"], true);
            assert_eq!(before["state"]["state"], "idle");
            std::fs::write(&flag, b"exit").unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let seen = backend.status(&proc).await.unwrap();
                if seen == (BackendStatus::Completed { exit_code: 1 }) { break; }
                if Instant::now() >= deadline {
                    // AMUX-4636. "never exited" alone could not tell a command that
                    // never ran (the typed line lost, or the flag variable missing
                    // in the pane) from one that exited and was reported as
                    // something else. Name the last status and the pane's text so a
                    // CI-only failure can be diagnosed from its log.
                    let pane = backend.capture(&proc, 40).await.unwrap_or_else(|e| format!("<capture failed: {e}>"));
                    let host = dead_pane_host_evidence(&proc.backend_ref);
                    panic!(
                        "controlled process never exited: last status {seen:?} after 10 s; flag written={}; pane:\n{pane}\nhost evidence:\n{host}",
                        flag.exists()
                    );
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(std::process::Command::new("tmux").args(["has-session", "-t", &format!("={}", proc.backend_ref)]).status().unwrap().success(), "positive control: dead pane's session must still exist");
            let captured = backend.capture(&proc, 60).await.unwrap();
            let adapter = amux_server::backend::adapter::TerminalAdapter::new(ProviderId::new("claude"));
            let inferred = adapter.scan(&captured);
            eprintln!("AF-784 terminal={captured:?} adapter_events={inferred:?}");
            assert!(inferred.is_empty(), "negative control: no shell prompt or provider marker may supply the exit");
            let scan = ScanLoop::new(store.clone(), vec![backend], None);
            let report = scan.scan_once().await.unwrap();
            let after = body(&app, &worker).await;
            eprintln!("AF-784 measured=true n_considered=1 retained_pane_exit=1 before={before} after={after} scan={report:?}");
            assert_eq!(after["running"], false, "retained dead pane is not a running agent");
            assert_ne!(after["state"]["state"], "idle", "confirmed process exit cannot remain idle");
            assert_eq!(report.process_exits[worker.as_str()].code, Some(1));
            assert_eq!(report.events_applied, 1);
            let diagnostic = amux_server::api::health::debug_scan().await.0;
            assert_eq!(diagnostic["measured"], true);
            assert_eq!(diagnostic["n_considered"], 1);
            assert_eq!(diagnostic["process_exits"][worker.as_str()]["code"], 1);
            assert_eq!(scan.scan_once().await.unwrap().events_applied, 0,
                "an ended session must not emit another exit next tick");
            let conn = store.read().unwrap();
            assert!(queries::live_session_for(&conn, worker.as_str()).unwrap().is_none());
        }
    }).await;
    assert_eq!(proc.backend_ref, reference);
    backend.terminate(&proc).await.unwrap();
    checked.unwrap();
}
