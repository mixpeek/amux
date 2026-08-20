//! Backend conformance suite (Invariant 21, RR-0031/RR-0032).
//!
//! ONE generic lifecycle test, parameterized over `&dyn SessionBackend`, run
//! unchanged against every real backend. No assertion below knows which
//! backend it is exercising — that is the invariant: backends are
//! interchangeable process hosts (Invariant 33), so the suite that certifies
//! one must certify all of them byte-for-byte.
//!
//! Lifecycle exercised: spawn -> status Running -> reconcile lists it ->
//! capture succeeds -> attach_info succeeds -> terminate -> status
//! NotFound/Completed -> reconcile no longer lists it.
//!
//! SAFETY — this machine hosts a LIVE amux fleet (60+ real tmux sessions named
//! `amux-<name>`, plus a live herdr session). The rules this file obeys:
//! - refs come only from `backend_ref(WorkerId::from_ulid(fresh ulid))`, i.e.
//!   `amux-wrk_<ulid>` — a shape no human-named fleet session has;
//! - before ANY terminate, assert the target ref contains `wrk_`;
//! - `reconcile()` results are only SEARCHED for our own ref; other listed
//!   sessions are never touched, asserted on, or acted upon.
//!
//! Skips are LOUD: a conformance suite that silently skips both backends is a
//! green lie (ethos rule 7), so every skip prints exactly what was not tested
//! and why (visible via `cargo test -- --nocapture`, and always visible in
//! the summary as a suspiciously fast pass — check stderr when it matters).

use std::collections::BTreeMap;
use std::process::Stdio;

use amux_core::ids::WorkerId;
use amux_server::backend::herdr::HerdrBackend;
use amux_server::backend::tmux::TmuxBackend;
use amux_server::backend::{backend_ref, BackendStatus, SessionBackend, SessionSpec};

// ---------------------------------------------------------------------------
// The generic suite
// ---------------------------------------------------------------------------

/// Full process-lifecycle conformance run. Panics (with the backend's name)
/// on the first violated expectation. Always attempts cleanup of the session
/// it spawned, even when a mid-lifecycle assertion failed.
async fn run_conformance(backend: &dyn SessionBackend) {
    let worker = WorkerId::from_ulid(ulid::Ulid::new());
    let ref_ = backend_ref(&worker);
    // Safety precondition for everything below: throwaway refs are
    // worker-shaped and can never collide with a human-named fleet session.
    assert!(
        ref_.contains("wrk_"),
        "test ref {ref_:?} is not worker-shaped; refusing to run against a live fleet"
    );

    let spec = SessionSpec {
        worker: worker.clone(),
        // A trivial long-lived process: long enough to outlive the suite,
        // short enough to self-reap if cleanup ever fails.
        command: vec!["sleep".into(), "30".into()],
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        env: BTreeMap::from([("AMUX_CONFORMANCE".to_string(), "1".to_string())]),
        human_label: None,
    };

    let proc = backend
        .spawn(&spec)
        .await
        .unwrap_or_else(|e| panic!("[{}] spawn failed: {e}", backend.name()));
    assert_eq!(
        proc.backend_ref,
        ref_,
        "[{}] spawn must return the canonical backend ref",
        backend.name()
    );

    // Everything between spawn and terminate is collected as a Result so a
    // failed expectation cannot skip cleanup and leak a session into the
    // live fleet's namespace.
    let mid = mid_lifecycle(backend, &proc, &ref_).await;

    // SAFETY GUARD: never point a kill at anything that is not one of our
    // throwaway workers. Checked immediately before the destructive call, on
    // the exact value passed to it.
    assert!(
        proc.backend_ref.contains("wrk_"),
        "REFUSING terminate: {:?} is not a throwaway conformance ref",
        proc.backend_ref
    );
    let term = backend.terminate(&proc).await;

    if let Err(msg) = mid {
        // Cleanup was attempted above; now surface the real failure.
        panic!("[{}] {msg}", backend.name());
    }
    term.unwrap_or_else(|e| panic!("[{}] terminate failed: {e}", backend.name()));

    // After terminate: the process host must report the session gone.
    // NotFound (host reaped it) and Completed (host kept a corpse with an
    // exit status) are both honest answers; Running is a failure.
    match backend.status(&proc).await {
        Ok(BackendStatus::NotFound) | Ok(BackendStatus::Completed { .. }) => {}
        Ok(other) => panic!(
            "[{}] status after terminate: expected NotFound or Completed, got {other:?}",
            backend.name()
        ),
        Err(e) => panic!("[{}] status after terminate errored: {e}", backend.name()),
    }

    let after = backend
        .reconcile()
        .await
        .unwrap_or_else(|e| panic!("[{}] reconcile after terminate failed: {e}", backend.name()));
    assert!(
        !after.iter().any(|s| s.backend_ref == ref_),
        "[{}] reconcile still lists {ref_} after terminate",
        backend.name()
    );

    eprintln!("[{}] backend conformance: PASS ({ref_})", backend.name());
}

/// Checks while the session is live. String errors instead of panics so the
/// caller can guarantee terminate() runs first.
async fn mid_lifecycle(
    backend: &dyn SessionBackend,
    proc: &amux_server::backend::ProcessRef,
    ref_: &str,
) -> Result<(), String> {
    match backend.status(proc).await {
        Ok(BackendStatus::Running) => {}
        Ok(other) => return Err(format!("status after spawn: expected Running, got {other:?}")),
        Err(e) => return Err(format!("status after spawn errored: {e}")),
    }

    // Reconcile must list our session. CONTAINMENT ONLY: the same call also
    // returns every other amux-* session on this machine (the live fleet
    // under tmux); those are read, never asserted on, never acted upon.
    let listed = backend
        .reconcile()
        .await
        .map_err(|e| format!("reconcile errored: {e}"))?;
    let ours = listed
        .iter()
        .find(|s| s.backend_ref == ref_)
        .ok_or_else(|| format!("reconcile does not list {ref_}"))?;
    if !matches!(ours.status, BackendStatus::Running) {
        return Err(format!(
            "reconcile lists {ref_} but with status {:?}, expected Running",
            ours.status
        ));
    }

    // Capture is a diagnostic view (D1): the contract here is "returns
    // without error", not any particular content — the pane may still be
    // mid shell-init and legitimately near-empty.
    backend
        .capture(proc, 50)
        .await
        .map_err(|e| format!("capture errored: {e}"))?;

    // send_text NEVER SILENTLY SUCCEEDS. The probe pane runs a plain command,
    // not an agent, so no backend can accept a prompt for it: tmux has no
    // delivery here at all (session_verbs owns its keystroke path) and herdr's
    // `agent prompt` refuses a pane it has not recognized as an agent. `Ok`
    // would mean the caller was told "delivered" about a message that went
    // nowhere — the one outcome that costs a real message.
    if let Ok(()) = backend.send_text(proc, "conformance probe").await {
        return Err(format!(
            "send_text reported success for {ref_}, whose pane hosts a plain command and \
             cannot accept a prompt — a delivery claim nobody can honour"
        ));
    }

    let attach = backend
        .attach_info(proc)
        .await
        .map_err(|e| format!("attach_info errored: {e}"))?;
    if attach.command.trim().is_empty() {
        return Err("attach_info returned an empty command".into());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Availability gates — every skip is printed, never silent
// ---------------------------------------------------------------------------

/// True if `<bin> <probe_arg>` runs and exits 0. A spawn error (binary not on
/// PATH) is the expected "not installed" signal.
async fn binary_available(bin: &str, probe_arg: &str) -> bool {
    tokio::process::Command::new(bin)
        .arg(probe_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// herdr's CLI is a socket-API client: without a running herdr server session
/// every verb fails with `server_not_running`, so the suite needs the name of
/// a live session to target. Prefers the fleet's long-running `amux` session,
/// else any running one.
async fn running_herdr_session() -> Option<String> {
    let out = tokio::process::Command::new("herdr")
        .args(["session", "list", "--json"])
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let running: Vec<String> = v["sessions"]
        .as_array()?
        .iter()
        .filter(|s| s["running"].as_bool() == Some(true))
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    running
        .iter()
        .find(|n| n.as_str() == "amux")
        .or_else(|| running.first())
        .cloned()
}

// ---------------------------------------------------------------------------
// Per-backend entry points (same suite, different host — Invariant 21)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tmux_backend_conformance() {
    if !binary_available("tmux", "-V").await {
        eprintln!("SKIPPED: tmux backend conformance — `tmux` not found on PATH; the tmux backend was NOT tested");
        return;
    }
    run_conformance(&TmuxBackend::new()).await;
}

#[tokio::test]
async fn herdr_backend_conformance() {
    if !binary_available("herdr", "--version").await {
        eprintln!("SKIPPED: herdr backend conformance — `herdr` not found on PATH; the herdr backend was NOT tested");
        return;
    }
    let Some(session) = running_herdr_session().await else {
        eprintln!(
            "SKIPPED: herdr backend conformance — herdr is installed but no herdr server \
             session is running (herdr 0.8.0's CLI is a socket-API client and cannot host \
             panes without one; start `herdr --session amux` to enable); the herdr backend \
             was NOT tested"
        );
        return;
    };
    run_conformance(&HerdrBackend::new(session)).await;
}
