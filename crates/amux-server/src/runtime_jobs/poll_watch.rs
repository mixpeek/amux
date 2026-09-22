//! Name synchronous job polls that hold a maintenance worker without yielding.
use std::{
    future::Future,
    task::Poll,
    time::{Duration, Instant},
};

pub(super) async fn watch<T>(job: &str, future: impl Future<Output = T>) -> T {
    observe(future, |timing| {
        if timing.elapsed >= Duration::from_millis(250) {
            tracing::warn!(target: "runtime", verdict = "runtime_job_blocking_poll", job,
                elapsed_ms = timing.elapsed.as_millis() as u64, pid = std::process::id(),
                thread_cpu_measured = timing.cpu.is_some(),
                thread_cpu_ms = timing.cpu.map(|d| d.as_millis() as u64).unwrap_or(0),
                runtime_pool = super::executor::pool_name(),
                commit = env!("AMUX_BUILD_COMMIT_FULL"),
                "job held an async runtime thread without yielding; other jobs in this pool may stall");
        }
    }).await
}

struct PollTiming {
    elapsed: Duration,
    cpu: Option<Duration>,
}

// Wall time includes synchronous IO and OS descheduling. Naming the job alone
// must not turn an 89s wall-clock poll into a claim of 89s spent burning CPU.
#[cfg(unix)]
fn thread_cpu() -> Option<Duration> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: time is a valid writable timespec; no pointers outlive the call.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } != 0 {
        return None;
    }
    Some(Duration::new(
        time.tv_sec.try_into().ok()?,
        time.tv_nsec.try_into().ok()?,
    ))
}

#[cfg(not(unix))]
fn thread_cpu() -> Option<Duration> {
    None
}

async fn observe<T>(future: impl Future<Output = T>, mut report: impl FnMut(PollTiming)) -> T {
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(|cx| {
        let started = Instant::now();
        let cpu_started = thread_cpu();
        let result: Poll<T> = future.as_mut().poll(cx);
        report(PollTiming {
            elapsed: started.elapsed(),
            cpu: cpu_started.and_then(|start| thread_cpu()?.checked_sub(start)),
        });
        result
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_time_inside_poll_instead_of_time_awaiting_io() {
        let mut blocking = Duration::ZERO;
        let mut cpu = None;
        observe(
            async {
                std::thread::sleep(Duration::from_millis(80));
            },
            |d| {
                blocking = blocking.max(d.elapsed);
                cpu = d.cpu;
            },
        )
        .await;
        assert!(blocking >= Duration::from_millis(80));
        #[cfg(unix)]
        assert!(
            cpu.is_some_and(|d| d < Duration::from_millis(40)),
            "sleeping inside a poll blocks the runtime but is not CPU work: {cpu:?}"
        );
        let mut yielding = Duration::ZERO;
        observe(tokio::time::sleep(Duration::from_millis(80)), |d| {
            yielding = yielding.max(d.elapsed)
        })
        .await;
        assert!(
            yielding < Duration::from_millis(40),
            "awaited IO must not be blamed as a blocking poll: {yielding:?}"
        );
    }
}
