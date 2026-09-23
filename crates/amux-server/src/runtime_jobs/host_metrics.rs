//! Host utilization history: the analysis `/api/metrics/host` already serves,
//! sampled on an interval, so "what was this machine doing at 03:00" has an
//! answer.
//!
//! # Why this exists
//!
//! Every host instrument amux had was a SPOT READ: `/health`,
//! `/api/metrics/host`, `scripts/mac-pressure-tripwire.sh`. Asked on
//! 2026-09-15 where 151 GB of free disk had gone overnight, none of them could
//! say, because no reading is kept. `disk_watch` persists samples of cache
//! DIRECTORIES, which answers a different question.
//!
//! # Why it samples the same function the endpoint calls
//!
//! [`crate::api::metrics::run_host_analysis`] runs `scripts/host-analysis.sh`,
//! embedded in the binary. Sampling that rather than re-deriving CPU, memory
//! and disk here keeps one source of truth: a series that disagrees with the
//! live endpoint would be worse than no series, because both look measured.
//!
//! # A failed probe is RECORDED, not skipped
//!
//! A tick whose analysis fails writes a row with `measured = 0` and the reason
//! it failed. Writing nothing would make "the probe could not run" identical
//! to "the server was down" in the series, which is exactly the shape ethos
//! rule 4 exists to prevent.
//!
//! Retention lives with every other table's, in `storage.rs`
//! (`AMUX_HOST_METRICS_RETAIN_DAYS`, default 30). At ~1.8 KB per sample and
//! the default 300 s interval that is ~520 KB/day, ~15 MB retained.

use crate::api::AppState;
use serde_json::Value;

const JOB: &str = super::registry::ids::HOST_METRICS;

/// Seconds between samples (`AMUX_HOST_METRICS_EVERY_SECS`, default 300).
///
/// FLOORED AT 60, and no "off" value is offered, because
/// [`super::spawn_periodic`] clamps its interval with `secs.max(1)`: a 0 here
/// would sample every second forever rather than switch the job off. A knob
/// whose documented off switch does not stop the job is the lie
/// `registry::EnvControl::off` warns about, so this one does not claim it.
pub fn tick_secs() -> u64 {
    std::env::var("AMUX_HOST_METRICS_EVERY_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|v| v.max(60))
        .unwrap_or(300)
}

/// The columns lifted out of a sample so a series is queryable without parsing
/// JSON per row. Every field is optional: a probe that could not read a
/// resource records NULL for it, never 0.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Row {
    pub cpu_count: Option<i64>,
    pub load1: Option<f64>,
    pub load_per_core: Option<f64>,
    pub mem_total_mb: Option<f64>,
    pub mem_used_mb: Option<f64>,
    pub mem_percent: Option<f64>,
    pub mem_pressure: Option<String>,
    pub swap_used_mb: Option<f64>,
    pub swap_total_mb: Option<f64>,
    pub disk_free_gb: Option<f64>,
    pub disk_total_gb: Option<f64>,
    pub proc_total: Option<i64>,
}

/// Lift the queryable columns out of one host-analysis payload.
///
/// Absent stays absent: `v["memory"]["used_mb"]` missing yields `None`, so the
/// row cannot claim a machine used zero memory because a probe was silent.
pub fn extract(v: &Value) -> Row {
    let f = |a: &str, b: &str| v.get(a).and_then(|o| o.get(b)).and_then(|x| x.as_f64());
    let i = |a: &str, b: &str| v.get(a).and_then(|o| o.get(b)).and_then(|x| x.as_i64());
    Row {
        cpu_count: i("cpu", "count"),
        load1: v
            .get("cpu")
            .and_then(|c| c.get("load_avg"))
            .and_then(|l| l.get(0))
            .and_then(|x| x.as_f64()),
        load_per_core: f("cpu", "load_per_core"),
        mem_total_mb: f("memory", "total_mb"),
        mem_used_mb: f("memory", "used_mb"),
        mem_percent: f("memory", "percent"),
        mem_pressure: v
            .get("memory")
            .and_then(|m| m.get("pressure"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        swap_used_mb: f("swap", "used_mb"),
        swap_total_mb: f("swap", "total_mb"),
        disk_free_gb: f("disk", "free_gb"),
        disk_total_gb: f("disk", "total_gb"),
        proc_total: i("process_counts", "total"),
    }
}

/// Insert one sample. `measured` false carries `why` and a default row, so the
/// gap is IN the series rather than missing from it.
async fn record(
    state: &AppState,
    ts: i64,
    measured: bool,
    why: Option<String>,
    row: Row,
    sample: String,
) {
    let outcome = state
        .store
        .write_async(move |conn| {
            let r = &row;
            conn.execute(
                "INSERT INTO host_metrics (ts, measured, why_unmeasured, cpu_count, load1, \
                 load_per_core, mem_total_mb, mem_used_mb, mem_percent, mem_pressure, \
                 swap_used_mb, swap_total_mb, disk_free_gb, disk_total_gb, proc_total, sample) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                rusqlite::params![
                    ts,
                    if measured { 1 } else { 0 },
                    why,
                    r.cpu_count,
                    r.load1,
                    r.load_per_core,
                    r.mem_total_mb,
                    r.mem_used_mb,
                    r.mem_percent,
                    r.mem_pressure,
                    r.swap_used_mb,
                    r.swap_total_mb,
                    r.disk_free_gb,
                    r.disk_total_gb,
                    r.proc_total,
                    sample,
                ],
            )?;
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await;
    if let Err(e) = outcome {
        // WARN because the failure mode this job exists to fix is a series with
        // holes nobody can explain; a write that failed silently would be one.
        tracing::warn!(job = JOB, %e, "host-metrics sample was not stored");
    }
}

/// Store one analysis outcome. Split from [`tick`] so the FAILURE arm is
/// reachable from a test: `run_host_analysis` shells out, and a test cannot
/// make bash fail on demand without shipping a seam nobody else needs.
pub async fn record_result(state: &AppState, ts: i64, res: Result<Value, String>) {
    match res {
        Ok(v) => {
            let row = extract(&v);
            record(state, ts, true, None, row, v.to_string()).await;
        }
        Err(why) => {
            tracing::warn!(job = JOB, %why, "host analysis failed; recording an unmeasured sample");
            record(
                state,
                ts,
                false,
                Some(why),
                Row::default(),
                "{}".to_string(),
            )
            .await;
        }
    }
}

pub async fn tick(state: AppState) {
    let ts = chrono::Utc::now().timestamp();
    let res = match tokio::task::spawn_blocking(crate::api::metrics::run_host_analysis).await {
        Ok(r) => r,
        Err(e) => Err(format!("host-analysis task panicked: {e}")),
    };
    record_result(&state, ts, res).await;
}

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, tick_secs(), move || {
        let st = state.clone();
        async move { tick(st).await }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The real payload shape, trimmed to the fields this job reads.
    fn full_sample() -> Value {
        json!({
            "cpu": {"count": 28, "load_avg": [25.09, 18.0, 14.88], "load_per_core": 0.9, "physical": 28},
            "memory": {"total_mb": 98304.0, "used_mb": 88773.8, "free_mb": 9530.2, "percent": 90.3, "pressure": "normal"},
            "swap": {"total_mb": 71680.0, "used_mb": 70668.38},
            "disk": {"free_gb": 410.6, "total_gb": 1858.2, "used_gb": 1354.4, "percent": 72.9},
            "process_counts": {"total": 950, "claude": 14},
            "verdicts": {"cpu": "ok", "memory": "ok", "disk": "ok"}
        })
    }

    #[test]
    fn a_full_sample_yields_every_queryable_column() {
        let r = extract(&full_sample());
        assert_eq!(r.cpu_count, Some(28));
        assert_eq!(r.load1, Some(25.09));
        assert_eq!(r.load_per_core, Some(0.9));
        assert_eq!(r.mem_total_mb, Some(98304.0));
        assert_eq!(r.mem_used_mb, Some(88773.8));
        assert_eq!(r.mem_percent, Some(90.3));
        assert_eq!(r.mem_pressure.as_deref(), Some("normal"));
        assert_eq!(r.swap_used_mb, Some(70668.38));
        assert_eq!(r.swap_total_mb, Some(71680.0));
        assert_eq!(r.disk_free_gb, Some(410.6));
        assert_eq!(r.disk_total_gb, Some(1858.2));
        assert_eq!(r.proc_total, Some(950));
    }

    /// A resource the probe could not read must be NULL, never 0: a stored 0
    /// reads as a measurement, and a chart cannot tell it from a real zero.
    #[test]
    fn a_resource_the_probe_did_not_report_is_null_not_zero() {
        let mut v = full_sample();
        v.as_object_mut().unwrap().remove("memory");
        v.as_object_mut().unwrap().remove("swap");
        let r = extract(&v);
        assert_eq!(r.mem_used_mb, None, "absent memory must not become 0");
        assert_eq!(r.mem_percent, None);
        assert_eq!(r.mem_pressure, None);
        assert_eq!(r.swap_used_mb, None);
        // Positive control: the fields that ARE present still parse, so this
        // cell fails on a broken extractor rather than on an empty payload.
        assert_eq!(r.disk_free_gb, Some(410.6));
        assert_eq!(r.cpu_count, Some(28));
    }

    /// An empty load list must not silently read as load 0.
    #[test]
    fn an_empty_load_average_is_null() {
        let mut v = full_sample();
        v["cpu"]["load_avg"] = json!([]);
        assert_eq!(extract(&v).load1, None);
        assert_eq!(
            extract(&v).cpu_count,
            Some(28),
            "control: the rest still parses"
        );
    }

    /// The interval floor: `spawn_periodic` clamps to 1 s, so 0 must not reach
    /// it. Serialized with the other env-reading cell by the shared lock.
    #[test]
    fn the_interval_floor_holds() {
        let _g = env_lock();
        let restore = std::env::var("AMUX_HOST_METRICS_EVERY_SECS").ok();
        std::env::remove_var("AMUX_HOST_METRICS_EVERY_SECS");
        assert_eq!(tick_secs(), 300, "default");
        std::env::set_var("AMUX_HOST_METRICS_EVERY_SECS", "0");
        assert_eq!(tick_secs(), 60, "0 is floored, never passed through");
        std::env::set_var("AMUX_HOST_METRICS_EVERY_SECS", "900");
        assert_eq!(tick_secs(), 900);
        std::env::set_var("AMUX_HOST_METRICS_EVERY_SECS", "not-a-number");
        assert_eq!(tick_secs(), 300, "garbage falls back to the default");
        match restore {
            Some(v) => std::env::set_var("AMUX_HOST_METRICS_EVERY_SECS", v),
            None => std::env::remove_var("AMUX_HOST_METRICS_EVERY_SECS"),
        }
    }

    /// The insert must match the migration, and an unmeasured tick must leave a
    /// ROW rather than a gap: a hole in the series cannot say whether the probe
    /// failed or the server was down.
    #[tokio::test]
    async fn a_measured_and_an_unmeasured_sample_both_land_with_their_columns() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("t.db")).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        record(
            &state,
            1_700_000_000,
            true,
            None,
            extract(&full_sample()),
            full_sample().to_string(),
        )
        .await;
        record(
            &state,
            1_700_000_060,
            false,
            Some("sysctl missing".into()),
            Row::default(),
            "{}".to_string(),
        )
        .await;

        let conn = state.store.read().unwrap();
        let (n, unmeasured): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), SUM(CASE WHEN measured=0 THEN 1 ELSE 0 END) FROM host_metrics",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (n, unmeasured),
            (2, 1),
            "both rows land, exactly one unmeasured"
        );
        let (mem, why): (Option<f64>, Option<String>) = conn
            .query_row(
                "SELECT mem_used_mb, why_unmeasured FROM host_metrics WHERE ts=1700000060",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(mem, None, "a failed probe stores NULL, never 0");
        assert_eq!(why.as_deref(), Some("sysctl missing"));
        let load: Option<f64> = conn
            .query_row(
                "SELECT load1 FROM host_metrics WHERE ts=1700000000",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            load,
            Some(25.09),
            "control: the measured row kept its columns"
        );
    }

    /// A probe that FAILED must leave a row saying so. Without it the series
    /// has a hole, and a hole cannot distinguish "the probe could not run" from
    /// "the server was down" (ethos rule 4, the reason this job exists).
    #[tokio::test]
    async fn a_failed_probe_lands_as_an_unmeasured_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("t.db")).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        record_result(
            &state,
            1_700_001_000,
            Err("host-analysis.sh exited 127".into()),
        )
        .await;
        // Control: the success arm on the same path, so this cell fails on a
        // broken writer rather than passing because nothing was written at all.
        record_result(&state, 1_700_001_060, Ok(full_sample())).await;

        let conn = state.store.read().unwrap();
        let (measured, why): (i64, Option<String>) = conn
            .query_row(
                "SELECT measured, why_unmeasured FROM host_metrics WHERE ts=1700001000",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("the failed probe must leave a row");
        assert_eq!(measured, 0);
        assert_eq!(why.as_deref(), Some("host-analysis.sh exited 127"));
        let (measured_ok, load): (i64, Option<f64>) = conn
            .query_row(
                "SELECT measured, load1 FROM host_metrics WHERE ts=1700001060",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (measured_ok, load),
            (1, Some(25.09)),
            "control: the success arm still records"
        );
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        L.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }
}
