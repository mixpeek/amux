//! Where a send's time goes (owner, 2026-10-29: "improve the speed at which we
//! send shit to the tmux make sure we have profiling").
//!
//! Every send through `send_text_inner_bound` runs inside `profiled`, a
//! task-local scope. Inside it, each tmux call (`note_tmux`), each fixed wait
//! (`note_sleep`) and each named step (`mark`) is added to that send's
//! profile, so nothing at a call site has to carry a timer. When the send
//! returns, the profile is:
//! - logged at INFO as `verdict=send_profile` with the total and each bucket,
//!   and at WARN as `verdict=send_slow` over `SLOW_MS`, naming the slowest step;
//! - kept in a ring of the last `RING` sends for GET /api/debug/send-profile,
//!   which reports percentiles beside `measured` and `n_considered`.
//!
//! The ring is process memory and resets on restart; the log lines are the
//! durable copy a sweep reads.

use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Instant;

const RING: usize = 300;
const SLOW_MS: u64 = 2000;

#[derive(Default)]
struct Prof {
    tmux: BTreeMap<&'static str, (u32, u64)>,
    tmux_control: u32,
    sleep_ms: u64,
    sleeps: u32,
    marks: Vec<(&'static str, u64)>,
}

tokio::task_local! {
    static PROF: RefCell<Prof>;
    static STARTED: Instant;
}

static RECENT: Mutex<VecDeque<Value>> = Mutex::new(VecDeque::new());

fn since_start_us() -> Option<u64> {
    STARTED.try_with(|t| t.elapsed().as_micros() as u64).ok()
}

/// A tmux call of `sub` that took `us`, `control` when it went over the
/// control connection. No-op outside a send.
pub(crate) fn note_tmux(sub: &str, us: u64, control: bool) {
    let _ = PROF.try_with(|p| {
        let mut p = p.borrow_mut();
        let key: &'static str = match sub {
            "capture-pane" => "capture-pane",
            "send-keys" => "send-keys",
            "display-message" => "display-message",
            "load-buffer" => "load-buffer",
            "paste-buffer" => "paste-buffer",
            "has-session" => "has-session",
            "list-panes" => "list-panes",
            _ => "other",
        };
        let e = p.tmux.entry(key).or_default();
        e.0 += 1;
        e.1 += us;
        if control {
            p.tmux_control += 1;
        }
    });
}

/// A fixed wait of `ms`. No-op outside a send.
pub(crate) fn note_sleep(ms: u64) {
    let _ = PROF.try_with(|p| {
        let mut p = p.borrow_mut();
        p.sleep_ms += ms;
        p.sleeps += 1;
    });
}

/// The send reached `step`: the time since the previous mark is that step's.
pub(crate) fn mark(step: &'static str) {
    if let Some(at) = since_start_us() {
        let _ = PROF.try_with(|p| p.borrow_mut().marks.push((step, at)));
    }
}

/// Run `f` (one send to `name`) with a profile, then log and keep it.
pub(crate) async fn profiled<F: std::future::Future<Output = (bool, String)>>(name: &str, chars: usize, f: F) -> (bool, String) {
    let t0 = Instant::now();
    let (out, prof) = STARTED
        .scope(t0, PROF.scope(RefCell::new(Prof::default()), async {
            let out = f.await;
            let prof = PROF.with(|p| std::mem::take(&mut *p.borrow_mut()));
            (out, prof)
        }))
        .await;
    record(name, chars, t0.elapsed().as_micros() as u64, &out, prof);
    out
}

fn record(name: &str, chars: usize, total_us: u64, out: &(bool, String), p: Prof) {
    let ms = |us: u64| (us as f64 / 100.0).round() / 10.0;
    let tmux_us: u64 = p.tmux.values().map(|v| v.1).sum();
    let tmux_calls: u32 = p.tmux.values().map(|v| v.0).sum();
    // Steps: each mark's time since the one before.
    let mut steps = Vec::new();
    let mut prev = 0u64;
    for (step, at) in &p.marks {
        steps.push((*step, at.saturating_sub(prev)));
        prev = *at;
    }
    steps.push(("finish", total_us.saturating_sub(prev)));
    let slowest = steps.iter().max_by_key(|s| s.1).map(|s| (s.0, s.1)).unwrap_or(("finish", total_us));
    let total_ms = ms(total_us);
    let other_ms = ms(total_us.saturating_sub(tmux_us).saturating_sub(p.sleep_ms * 1000));
    let row = json!({
        "session": name,
        "at": chrono::Utc::now().to_rfc3339(),
        "ok": out.0,
        "chars": chars,
        "total_ms": total_ms,
        "tmux_ms": ms(tmux_us),
        "tmux_calls": tmux_calls,
        "tmux_over_control": p.tmux_control,
        "sleep_ms": p.sleep_ms,
        "sleeps": p.sleeps,
        "other_ms": other_ms,
        "tmux": p.tmux.iter().map(|(k, v)| (k.to_string(), json!({"calls": v.0, "ms": ms(v.1)}))).collect::<serde_json::Map<_, _>>(),
        "steps": steps.iter().map(|(s, us)| json!([s, ms(*us)])).collect::<Vec<_>>(),
        "slowest_step": slowest.0,
    });
    tracing::info!(session = %name, total_ms, tmux_ms = ms(tmux_us), tmux_calls, tmux_over_control = p.tmux_control,
        sleep_ms = p.sleep_ms, other_ms, slowest_step = slowest.0, slowest_ms = ms(slowest.1), ok = out.0,
        measured = true, n_considered = 1, verdict = "send_profile", "send profile");
    if total_us / 1000 > SLOW_MS {
        tracing::warn!(session = %name, total_ms, slowest_step = slowest.0, slowest_ms = ms(slowest.1),
            tmux_ms = ms(tmux_us), sleep_ms = p.sleep_ms, other_ms, verdict = "send_slow", measured = true, n_considered = 1,
            "a send to tmux took over {SLOW_MS} ms; slowest_step says where");
    }
    if let Ok(mut r) = RECENT.lock() {
        if r.len() >= RING {
            r.pop_front();
        }
        r.push_back(row);
    }
}

fn pct(sorted: &[f64], q: f64) -> Value {
    if sorted.is_empty() {
        return Value::Null;
    }
    let i = ((sorted.len() - 1) as f64 * q).round() as usize;
    json!(sorted[i])
}

/// GET /api/debug/send-profile: the last sends' timing, and the tmux transport's counters.
pub(crate) async fn debug_send_profile() -> axum::Json<Value> {
    let rows: Vec<Value> = RECENT.lock().map(|r| r.iter().cloned().collect()).unwrap_or_default();
    let col = |k: &str| {
        let mut v: Vec<f64> = rows.iter().filter_map(|r| r[k].as_f64()).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v
    };
    let summary = |k: &str| {
        let v = col(k);
        json!({"p50": pct(&v, 0.5), "p95": pct(&v, 0.95), "max": v.last().copied()})
    };
    // Which step is slowest, and how often.
    let mut slowest: BTreeMap<String, u32> = BTreeMap::new();
    for r in &rows {
        *slowest.entry(r["slowest_step"].as_str().unwrap_or("?").to_string()).or_default() += 1;
    }
    let mut by_total = rows.clone();
    by_total.sort_by(|a, b| b["total_ms"].as_f64().partial_cmp(&a["total_ms"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
    by_total.truncate(10);
    axum::Json(json!({
        "measured": true,
        "n_considered": rows.len(),
        "why_empty": if rows.is_empty() { json!("no send since this server started") } else { Value::Null },
        "kept": RING,
        "total_ms": summary("total_ms"),
        "tmux_ms": summary("tmux_ms"),
        "sleep_ms": summary("sleep_ms"),
        "other_ms": summary("other_ms"),
        "tmux_calls": summary("tmux_calls"),
        "slowest_step": slowest,
        "slowest_sends": by_total,
        "recent": rows.iter().rev().take(20).cloned().collect::<Vec<_>>(),
        "tmux_transport": crate::backend::tmux_ctl::stats(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_send_profile_adds_up_its_tmux_calls_waits_and_steps() {
        let name = "send-profile-unit";
        let out = profiled(name, 5, async {
            note_tmux("capture-pane", 2000, true);
            note_tmux("send-keys", 1000, false);
            note_tmux("send-keys", 1000, true);
            note_sleep(20);
            mark("typed");
            (true, "sent".to_string())
        }).await;
        assert!(out.0);
        let r = RECENT.lock().unwrap().iter().rev().find(|r| r["session"] == name).cloned().expect("the send was kept");
        assert_eq!(r["tmux_calls"], 3);
        assert_eq!(r["tmux_over_control"], 2);
        assert_eq!(r["tmux"]["send-keys"]["calls"], 2);
        assert_eq!(r["tmux_ms"], 4.0);
        assert_eq!(r["sleep_ms"], 20);
        assert_eq!(r["steps"][0][0], "typed");
        assert_eq!(r["steps"][1][0], "finish");
    }

    #[test]
    fn outside_a_send_nothing_is_recorded_and_nothing_panics() {
        note_tmux("capture-pane", 1, true);
        note_sleep(1);
        mark("nowhere");
    }
}
