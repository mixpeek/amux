//! Names the boot step that is keeping the listener from binding.
//!
//! A boot binds in well under a second (`ready_after_boot_ms` 120-900 over the
//! week before 2026-10-21). That morning it took 85 s for the one process that
//! survived and longer than five minutes for the 94 before it, which the
//! external watchdog killed mid-boot. No line said which step was slow; the
//! only clue was one thread at 99% CPU in a watchdog diagnostic. This watch
//! logs the step in progress every `AMUX_BOOT_WATCH_S` until the bind, and
//! samples the process once if the bind is still not reached after four
//! intervals, so the next slow boot names its own cause.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

static STEP: Mutex<(&str, Option<std::time::Instant>)> = Mutex::new(("start", None));
static BOUND: AtomicBool = AtomicBool::new(false);

/// Record the boot step about to run.
pub fn step(name: &'static str) {
    if let Ok(mut s) = STEP.lock() {
        *s = (name, Some(std::time::Instant::now()));
    }
}

/// The listener is bound: the watch ends.
pub fn bound() {
    BOUND.store(true, Ordering::SeqCst);
}

/// What the watch says after `elapsed` with the current step: None while the
/// boot is inside its budget.
pub fn verdict(elapsed: std::time::Duration, interval: std::time::Duration, step: &str, step_age: std::time::Duration) -> Option<String> {
    (elapsed >= interval).then(|| {
        format!("boot has not bound its listener after {}s; in step {step} for {}s",
            elapsed.as_secs(), step_age.as_secs())
    })
}

pub fn start() {
    let interval = std::env::var("AMUX_BOOT_WATCH_S")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(15)
        .max(1);
    let interval = std::time::Duration::from_secs(interval);
    let t0 = std::time::Instant::now();
    let _ = std::thread::Builder::new().name("boot-watch".into()).spawn(move || {
        let mut sampled = false;
        loop {
            std::thread::sleep(interval);
            if BOUND.load(Ordering::SeqCst) {
                return;
            }
            let (name, at) = STEP.lock().map(|s| *s).unwrap_or(("unknown", None));
            let step_age = at.map(|a| a.elapsed()).unwrap_or_default();
            let elapsed = t0.elapsed();
            if let Some(msg) = verdict(elapsed, interval, name, step_age) {
                tracing::warn!(target: "amux::boot", verdict = "boot_bind_slow", measured = true, n_considered = 1,
                    elapsed_s = elapsed.as_secs(), step = name, step_s = step_age.as_secs(), "{msg}");
            }
            if !sampled && elapsed >= interval * 4 && cfg!(target_os = "macos") {
                sampled = true;
                let path = crate::config::amux_home().join("logs").join(format!(
                    "boot-stall-{}.txt",
                    chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
                ));
                let ok = std::process::Command::new("/usr/bin/sample")
                    .args([std::process::id().to_string().as_str(), "3", "-file"])
                    .arg(&path)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                tracing::warn!(target: "amux::boot", verdict = "boot_stall_stack_captured", captured = ok,
                    measured = ok, n_considered = 1, step = name, path = %path.display(),
                    "stack of a boot that has not reached its bind");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_boot_inside_its_budget_says_nothing_and_a_late_one_names_its_step() {
        let s = Duration::from_secs;
        assert_eq!(verdict(s(3), s(15), "router", s(1)), None);
        let msg = verdict(s(85), s(15), "tls", s(70)).expect("an 85s boot is reported");
        assert!(msg.contains("after 85s") && msg.contains("in step tls for 70s"), "{msg}");
    }
}
