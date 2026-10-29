//! backend::tmux_ctl against a real tmux server: the control connection
//! answers as a spawned `tmux` does, in order under concurrency, and survives
//! the server going away. One test, run in order, because TMUX_TMPDIR is
//! process-wide and the client connects once per process.

use amux_server::backend::tmux_ctl::{run, stats, CTL_SESSION};
use std::process::Command;
use std::time::{Duration, Instant};

const T: Duration = Duration::from_secs(5);

fn tmux(args: &[&str]) -> std::process::Output {
    Command::new("tmux").args(args).output().expect("tmux runs")
}

fn text(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_control_connection_answers_as_spawned_tmux_does() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux is not installed; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("TMUX_TMPDIR", dir.path());
    std::env::remove_var("TMUX");

    // No server: the client does not start one.
    let none = run(&["list-sessions", "-F", "#{session_name}"], T).await.expect("answers");
    assert!(!none.status.success(), "no server, so list-sessions fails");
    assert!(!tmux(&["has-session", "-t", CTL_SESSION]).status.success(), "the control client started a tmux server");

    assert!(tmux(&["new-session", "-d", "-s", "amux-t", "-x", "120", "-y", "30", "cat"]).status.success());
    // The fallback may have recorded a recent failed connect; wait out its retry window.
    tokio::time::sleep(Duration::from_millis(2100)).await;

    // Same answer as spawning, for a read and for a format.
    let want = text(&tmux(&["display-message", "-p", "-t", "amux-t", "#{session_name}|#{window_width}"]));
    let got = run(&["display-message", "-p", "-t", "amux-t", "#{session_name}|#{window_width}"], T).await.unwrap();
    assert!(got.status.success());
    assert_eq!(text(&got), want);
    let s = stats();
    assert_eq!(s["connected"], true, "{s}");
    assert!(s["control_calls"].as_u64().unwrap() >= 1, "{s}");
    let workers = text(&tmux(&["list-sessions", "-F", "#{session_name}"]));
    assert!(workers.lines().any(|l| l == CTL_SESSION) && workers.lines().all(|l| l == CTL_SESSION || l.starts_with("amux-")), "{workers}");

    // An error is an error, with tmux's message on stderr.
    let missing = run(&["has-session", "-t", "amux-nope"], T).await.unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("amux-nope"), "{missing:?}");

    // Typed text arrives literally: quotes, $, ;, # and a format.
    let typed = "it's $HOME; #{pane_id} done";
    assert!(run(&["send-keys", "-t", "amux-t", "-l", typed], T).await.unwrap().status.success());
    let end = Instant::now() + Duration::from_secs(3);
    let mut pane = String::new();
    while Instant::now() < end {
        pane = text(&run(&["capture-pane", "-p", "-t", "amux-t"], T).await.unwrap());
        if pane.contains(typed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(pane.contains(typed), "{pane}");
    // The pane and the spawned capture agree line for line.
    assert_eq!(pane, text(&tmux(&["capture-pane", "-p", "-t", "amux-t"])));
    // A pane line that looks like the end of a block does not end it.
    assert!(run(&["send-keys", "-t", "amux-t", "-l", "%end 1 2 1"], T).await.unwrap().status.success());
    assert!(run(&["send-keys", "-t", "amux-t", "Enter"], T).await.unwrap().status.success());
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after = text(&run(&["capture-pane", "-p", "-t", "amux-t"], T).await.unwrap());
    assert_eq!(after, text(&tmux(&["capture-pane", "-p", "-t", "amux-t"])));

    // Fifty at once: every caller gets its own answer.
    let mut tasks = Vec::new();
    for i in 0..50 {
        tasks.push(tokio::spawn(async move {
            let o = run(&["display-message", "-p", &format!("answer-{i}")], T).await.unwrap();
            (i, text(&o))
        }));
    }
    for t in tasks {
        let (i, o) = t.await.unwrap();
        assert_eq!(o, format!("answer-{i}\n"));
    }

    // Faster than spawning, by a wide margin.
    let t0 = Instant::now();
    for _ in 0..20 {
        run(&["display-message", "-p", "x"], T).await.unwrap();
    }
    let ctl = t0.elapsed();
    let t0 = Instant::now();
    for _ in 0..20 {
        tmux(&["display-message", "-p", "x"]);
    }
    let spawned = t0.elapsed();
    eprintln!("20 calls: control {ctl:?}, spawned {spawned:?}");
    assert!(ctl < spawned, "control {ctl:?} vs spawned {spawned:?}");

    // The server goes away and comes back: reads still answer, and the
    // client reconnects.
    tmux(&["kill-server"]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let gone = run(&["has-session", "-t", "amux-t"], T).await.expect("answers by spawning");
    assert!(!gone.status.success());
    assert!(tmux(&["new-session", "-d", "-s", "amux-t", "cat"]).status.success());
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let back = run(&["has-session", "-t", "amux-t"], T).await.unwrap();
    assert!(back.status.success());
    let s = stats();
    assert!(s["connects"].as_u64().unwrap() >= 2, "it reconnected: {s}");
    assert_eq!(s["connected"], true, "{s}");
    tmux(&["kill-server"]);
}
