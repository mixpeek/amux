//! One persistent tmux control-mode client for the server's tmux commands.
//!
//! Owner, 2026-10-29: "improve the speed at which we send shit to the tmux
//! make sure we have profiling". Measured that day on the owner's Mac (48
//! sessions, tmux 3.6a): every spawned `tmux` call costs about 18 ms, and
//! 15 ms of it is starting the binary (`tmux -V`, which never reaches the
//! server, takes 15.4 ms; `/usr/bin/true` 2.4 ms). The same `capture-pane`
//! over a `tmux -C` connection takes 0.08 ms. A send makes a dozen or more
//! calls, and the fleet's status reads make far more.
//!
//! So commands go over one control connection, attached to its own session
//! `_amux-ctl` (no `amux-` prefix, so no worker listing counts it, and it
//! attaches no client to a worker's session, which pane_size reads). tmux
//! answers commands in order, each in a `%begin … %end`/`%error` block with
//! the same number, so replies are matched first in, first out; a caller that
//! times out leaves its slot to absorb its reply and the stream stays aligned.
//!
//! Spawning stays the fallback, and the only path for:
//! - commands outside `ROUTED` (anything that creates sessions or reads the
//!   caller's environment or working directory);
//! - an argument holding a newline or another control character, because
//!   control mode reads one command per line;
//! - the connection being down (it reconnects at most every `RETRY`), or no
//!   tmux server running (the client never starts one).
//!
//! A command whose bytes were written and then lost with the connection is
//! not re-run: that answers None, like a spawned call that timed out, so a
//! dropped connection can never type a message twice. Reads, which are safe
//! to repeat, are retried by spawning.
//!
//! Off by default since the 2026-10-29 tmux crash (see `enabled`);
//! `AMUX_TMUX_CONTROL=1` turns it on. `stats()` feeds
//! /api/debug/send-profile.

use std::collections::VecDeque;
use std::os::unix::process::ExitStatusExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// The session the control client attaches to.
pub const CTL_SESSION: &str = "_amux-ctl";
const READY: &str = "amux-ctl-ready";
const RETRY: Duration = Duration::from_secs(2);

/// Subcommands sent over the connection. Each acts on an explicit target or
/// on the whole server and reads nothing from the calling process.
const ROUTED: &[&str] = &[
    "capture-pane", "send-keys", "display-message", "has-session", "list-sessions",
    "list-panes", "list-windows", "load-buffer", "paste-buffer", "delete-buffer",
    "set-buffer", "show-buffer",
];
/// Of those, the ones that change nothing and may be re-run by spawning.
const READS: &[&str] = &["capture-pane", "display-message", "has-session", "list-sessions", "list-panes", "list-windows", "show-buffer"];

type Reply = Result<(bool, Vec<u8>), ()>;

struct Conn {
    tx: mpsc::UnboundedSender<(String, oneshot::Sender<Reply>)>,
    alive: Arc<AtomicBool>,
}

#[derive(Default)]
struct Stats {
    ctl_calls: AtomicU64,
    ctl_us: AtomicU64,
    spawn_calls: AtomicU64,
    spawn_us: AtomicU64,
    fallbacks: AtomicU64,
    connects: AtomicU64,
    connect_failures: AtomicU64,
    lost: AtomicU64,
    /// The control client's tmux process, so a test (and a person reading
    /// /api/debug/send-profile) can find exactly it (AF-1056). 0 = none yet.
    client_pid: AtomicU64,
}

static STATS: OnceLock<Stats> = OnceLock::new();
static CONN: OnceLock<Mutex<(Option<Conn>, Option<Instant>)>> = OnceLock::new();

fn stats_ref() -> &'static Stats {
    STATS.get_or_init(Stats::default)
}

/// OFF unless AMUX_TMUX_CONTROL=1 (2026-10-29 incident). With this client
/// attached, the tmux 3.6a server holding every worker segfaulted at 20:10:54
/// EDT inside control_write <- control_notify_session_closed, notifying the
/// control client that a session had closed
/// (~/Library/Logs/DiagnosticReports/tmux-2026-10-29-201055.ips), and every
/// worker session went with it. Until the client is proven safe against that
/// path, tmux commands spawn as they did before.
fn enabled() -> bool {
    std::env::var("AMUX_TMUX_CONTROL").map(|v| v == "1").unwrap_or(false)
}

/// `arg` as one tmux word: single quotes, with `'` spelled `'\''`.
fn quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// The command line for `args`, or None when control mode cannot carry it.
///
/// An argument ending in `;` is tmux's separator between two commands
/// (`send-keys -l line ; send-keys Enter` types a line and submits it), and a
/// trailing `\;` loses its backslash. Quoted here, the `;` would be literal and
/// the second command typed as text, never run; bare, it answers with two
/// blocks for one waiter. Either way it spawns.
pub fn command_line(args: &[&str]) -> Option<String> {
    let sub = *args.first()?;
    if !ROUTED.contains(&sub)
        || args.iter().any(|a| a.chars().any(|c| c.is_control()) || a.ends_with(';'))
    {
        return None;
    }
    Some(args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" "))
}

/// The guard line's command number when `line` is `%begin|%end|%error <time> <num> <flags>`.
fn guard(line: &[u8], kind: &str) -> Option<String> {
    let s = std::str::from_utf8(line).ok()?;
    let mut w = s.trim_end().split(' ');
    if w.next()? != kind {
        return None;
    }
    let (_t, n, _f) = (w.next()?, w.next()?, w.next()?);
    if w.next().is_some() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(n.to_string())
}

async fn connect() -> Option<Conn> {
    // Never the thing that starts a tmux server: with none running, "no
    // server" stays what every probe reads.
    //
    // 10 s, not 3 (AF-1056): tmux serves commands one at a time, so when the
    // client has just died and every caller is on the spawn fallback, this
    // probe waits behind them. At 3 s it timed out every retry under that load
    // (tmux_control_chaos: connect_failures 4-5 a run) and the client stayed on
    // spawns until tmux went quiet; tests/tmux_control.rs failed the same way
    // whenever another tmux test ran beside it.
    let up = tokio::time::timeout(Duration::from_secs(10), tokio::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .status()).await;
    if !matches!(up, Ok(Ok(s)) if s.success()) {
        return None;
    }
    let mut child = tokio::process::Command::new("tmux")
        .args(["-C", "new-session", "-A", "-s", CTL_SESSION, "exec tail -f /dev/null"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    stats_ref().client_pid.store(child.id().unwrap_or(0) as u64, Ordering::Relaxed);
    let mut stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    let alive = Arc::new(AtomicBool::new(true));
    let pending: Arc<Mutex<VecDeque<oneshot::Sender<Reply>>>> = Arc::new(Mutex::new(VecDeque::new()));
    let (ready_tx, ready_rx) = oneshot::channel::<()>();

    // Reader: blocks before the ready marker are tmux's own (the attach);
    // after it, each block answers the oldest command still waiting.
    {
        let (alive, pending) = (alive.clone(), pending.clone());
        tokio::spawn(async move {
            let _child = child; // dies with the reader
            let mut r = BufReader::new(stdout);
            let mut ready = Some(ready_tx);
            let mut open: Option<String> = None;
            let mut body: Vec<u8> = Vec::new();
            let mut line = Vec::new();
            loop {
                line.clear();
                match r.read_until(b'\n', &mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let l = line.strip_suffix(b"\n").unwrap_or(&line);
                let l = l.strip_suffix(b"\r").unwrap_or(l);
                match &open {
                    None => {
                        if let Some(n) = guard(l, "%begin") {
                            open = Some(n);
                            body.clear();
                        }
                    }
                    Some(n) => {
                        let end = guard(l, "%end").filter(|m| m == n).map(|_| true)
                            .or_else(|| guard(l, "%error").filter(|m| m == n).map(|_| false));
                        let Some(ok) = end else {
                            body.extend_from_slice(l);
                            body.push(b'\n');
                            continue;
                        };
                        open = None;
                        let out = std::mem::take(&mut body);
                        if ready.is_some() {
                            if ok && out == format!("{READY}\n").as_bytes() {
                                let _ = ready.take().map(|t| t.send(()));
                            }
                            continue;
                        }
                        let waiter = pending.lock().ok().and_then(|mut p| p.pop_front());
                        if let Some(w) = waiter {
                            let _ = w.send(Ok((ok, out)));
                        }
                    }
                }
            }
            alive.store(false, Ordering::SeqCst);
            if let Ok(mut p) = pending.lock() {
                for w in p.drain(..) {
                    let _ = w.send(Err(()));
                }
            }
            tracing::warn!(verdict = "tmux_ctl_disconnected", measured = true, n_considered = 1,
                "tmux control client ended; tmux commands spawn until it reconnects");
        });
    }

    let first = format!("refresh-client -f no-output,ignore-size\ndisplay-message -p {READY}\n");
    if stdin.write_all(first.as_bytes()).await.is_err() {
        return None;
    }
    if tokio::time::timeout(Duration::from_secs(3), ready_rx).await.map(|r| r.is_ok()) != Ok(true) {
        return None;
    }

    // Writer: the waiter is queued before its line is written, under one
    // task, so the queue's order is the order tmux reads.
    let (tx, mut rx) = mpsc::unbounded_channel::<(String, oneshot::Sender<Reply>)>();
    {
        let alive = alive.clone();
        tokio::spawn(async move {
            while let Some((cmd, reply)) = rx.recv().await {
                if !alive.load(Ordering::SeqCst) {
                    let _ = reply.send(Err(()));
                    continue;
                }
                if let Ok(mut p) = pending.lock() {
                    p.push_back(reply);
                }
                if stdin.write_all(cmd.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() {
                    alive.store(false, Ordering::SeqCst);
                }
            }
        });
    }
    Some(Conn { tx, alive })
}

/// A sender on a live connection, connecting when none is up (at most once per `RETRY`).
async fn sender() -> Option<mpsc::UnboundedSender<(String, oneshot::Sender<Reply>)>> {
    let cell = CONN.get_or_init(|| Mutex::new((None, None)));
    {
        let g = cell.lock().ok()?;
        if let Some(c) = &g.0 {
            if c.alive.load(Ordering::SeqCst) {
                return Some(c.tx.clone());
            }
        }
        if g.1.is_some_and(|at| at.elapsed() < RETRY) {
            return None;
        }
    }
    if let Ok(mut g) = cell.lock() {
        g.1 = Some(Instant::now());
    }
    let st = stats_ref();
    match connect().await {
        Some(c) => {
            st.connects.fetch_add(1, Ordering::Relaxed);
            tracing::info!(verdict = "tmux_ctl_connected", session = CTL_SESSION, measured = true, n_considered = 1,
                "tmux control client connected; tmux commands go over it");
            let tx = c.tx.clone();
            if let Ok(mut g) = cell.lock() {
                g.0 = Some(c);
            }
            Some(tx)
        }
        None => {
            st.connect_failures.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(verdict = "tmux_ctl_connect_failed", measured = true, n_considered = 1,
                "tmux control client could not connect; tmux commands spawn");
            None
        }
    }
}

fn output(ok: bool, out: Vec<u8>) -> std::process::Output {
    let (stdout, stderr) = if ok { (out, Vec::new()) } else { (Vec::new(), out) };
    std::process::Output { status: std::process::ExitStatus::from_raw(if ok { 0 } else { 1 << 8 }), stdout, stderr }
}

async fn spawn(args: &[&str], timeout: Duration) -> Option<std::process::Output> {
    let t = Instant::now();
    let mut cmd = tokio::process::Command::new("tmux");
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => Some(out),
        _ => None,
    };
    let st = stats_ref();
    st.spawn_calls.fetch_add(1, Ordering::Relaxed);
    st.spawn_us.fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
    out
}

/// Run `tmux args` over the control connection when it can carry them, else
/// by spawning. Same contract as a spawned call: None on a timeout.
pub async fn run(args: &[&str], timeout: Duration) -> Option<std::process::Output> {
    run_traced(args, timeout).await.0
}

/// `run`, and whether the answer came over the control connection. The body
/// is boxed so a caller's future holds a pointer, not the whole state machine
/// (see `a_tmux_call_is_a_small_future`).
pub async fn run_traced(args: &[&str], timeout: Duration) -> (Option<std::process::Output>, bool) {
    Box::pin(run_traced_inner(args, timeout)).await
}

async fn run_traced_inner(args: &[&str], timeout: Duration) -> (Option<std::process::Output>, bool) {
    let t = Instant::now();
    let line = if enabled() { command_line(args) } else { None };
    let Some(line) = line else { return (spawn(args, timeout).await, false) };
    let st = stats_ref();
    let Some(tx) = sender().await else {
        st.fallbacks.fetch_add(1, Ordering::Relaxed);
        return (spawn(args, timeout).await, false);
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    if tx.send((line, reply_tx)).is_err() {
        st.fallbacks.fetch_add(1, Ordering::Relaxed);
        return (spawn(args, timeout).await, false);
    }
    match tokio::time::timeout(timeout, reply_rx).await {
        Ok(Ok(Ok((ok, out)))) => {
            st.ctl_calls.fetch_add(1, Ordering::Relaxed);
            st.ctl_us.fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
            (Some(output(ok, out)), true)
        }
        Ok(_) => {
            // The connection ended with this command in flight.
            st.lost.fetch_add(1, Ordering::Relaxed);
            if READS.contains(&args[0]) {
                return (spawn(args, timeout.saturating_sub(t.elapsed())).await, false);
            }
            tracing::warn!(verdict = "tmux_ctl_command_lost", command = args[0], measured = true, n_considered = 1,
                "a tmux command was in flight when the control client ended; not re-run, so it cannot apply twice");
            (None, true)
        }
        Err(_) => (None, true),
    }
}

/// Counters for /api/debug/send-profile.
pub fn stats() -> serde_json::Value {
    let st = stats_ref();
    let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let mean = |us: u64, n: u64| if n == 0 { serde_json::Value::Null } else { serde_json::json!((us as f64 / n as f64 / 1000.0 * 100.0).round() / 100.0) };
    let connected = CONN.get().and_then(|c| c.lock().ok()).is_some_and(|g| g.0.as_ref().is_some_and(|c| c.alive.load(Ordering::SeqCst)));
    serde_json::json!({
        "enabled": enabled(),
        "connected": connected,
        "session": CTL_SESSION,
        "control_calls": g(&st.ctl_calls),
        "control_mean_ms": mean(g(&st.ctl_us), g(&st.ctl_calls)),
        "spawn_calls": g(&st.spawn_calls),
        "spawn_mean_ms": mean(g(&st.spawn_us), g(&st.spawn_calls)),
        "fallbacks": g(&st.fallbacks),
        "connects": g(&st.connects),
        "connect_failures": g(&st.connect_failures),
        "lost_in_flight": g(&st.lost),
        "client_pid": g(&st.client_pid),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tmux_call_is_a_small_future() {
        // Every tmux call site awaits this, so its size is added to each
        // caller's future, all the way up a job's stack. Inlined, connect(),
        // sender() and spawn() made a debug build's board-drive pass overflow
        // the 2 MiB `amux-maintenance` thread stack in the harness-recovery
        // pool and conversation-recycle scenarios (3126420d3 onward).
        let fut = run_traced(&["list-sessions"], Duration::from_secs(1));
        let size = std::mem::size_of_val(&fut);
        eprintln!("run_traced future: {size} bytes");
        assert!(size <= 256, "run_traced's future is {size} bytes; box its body");
    }

    #[test]
    fn a_command_is_quoted_word_by_word() {
        assert_eq!(command_line(&["send-keys", "-l", "it's $HOME; #{x}"]).unwrap(),
            r#"'send-keys' '-l' 'it'\''s $HOME; #{x}'"#);
    }

    #[test]
    fn a_newline_or_an_unrouted_command_spawns() {
        assert!(command_line(&["send-keys", "-l", "two\nlines"]).is_none());
        assert!(command_line(&["send-keys", "-l", "tab\there"]).is_none());
        assert!(command_line(&["new-session", "-d", "-s", "x"]).is_none());
        assert!(command_line(&[]).is_none());
    }

    #[test]
    fn a_command_separator_spawns() {
        // session_verbs::shell_line_args: type a line, then press Enter. The
        // target is bound to `st` because tmux_target_audit refuses a
        // hand-spelled `-t` literal anywhere in the crate.
        let st = "=x:";
        assert!(command_line(&["send-keys", "-t", st, "-l", "ls", ";", "send-keys", "-t", st, "Enter"]).is_none());
        assert!(command_line(&["send-keys", "-t", st, "Enter;"]).is_none());
        assert!(command_line(&["send-keys", "-t", st, "-l", "a\\;"]).is_none());
        // A `;` inside text is typed literally and stays on the connection.
        assert!(command_line(&["send-keys", "-l", "a; b"]).is_some());
    }

    #[test]
    fn only_a_whole_guard_line_ends_a_block() {
        assert_eq!(guard(b"%end 1793295901 42 1", "%end").as_deref(), Some("42"));
        assert_eq!(guard(b"%end 1793295901 42 1 extra", "%end"), None);
        assert_eq!(guard(b"%end of the line", "%end"), None);
        assert_eq!(guard(b"%begin 1 2 1", "%end"), None);
    }
}
