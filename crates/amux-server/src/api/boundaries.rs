//! Worker boundaries and resource awareness.
//!
//! `boundaries.toml` (repo root) is the one registry. This module reads the
//! same files the PreToolUse guard reads (`scripts/hooks/boundary-guard.py`):
//! the installed copy at `~/.amux/hooks/boundaries.toml` (falling back to the
//! copy embedded in this binary) plus the owner's `~/.amux/boundaries.toml`
//! overrides, merged by `id`.
//!
//! - `GET /api/boundaries`: every boundary, whether its hook is actually
//!   installed and wired, and how often it fired in the last 24h.
//! - `GET /api/resources[?worker=<name>]`: the machine's capacity and
//!   pressure, the heaviest workers by CPU and memory, where the CPU no worker
//!   owns is going (`machine`), and one worker's own usage (default: the
//!   caller's `X-Amux-Session`) against the resource profile it declared
//!   (`AMUX_WORKER_CPUS`, `AMUX_WORKER_MEM_GB`).
//! - [`boundaries_context_block`] is composed into every non-isolated
//!   worker's launch context, so the rule text reaches each worker by default.

use axum::{extract::Query, http::HeaderMap, response::IntoResponse, routing::get, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use super::AppState;

/// The registry as committed, so a box without the installed copy still has
/// the text to show and to compose into worker context.
pub const DEFAULT_REGISTRY: &str = include_str!("../../../../boundaries.toml");

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/boundaries", get(boundaries))
        .route("/api/resources", get(resources))
}

fn home() -> PathBuf {
    crate::config::amux_home()
}

fn installed_registry() -> PathBuf {
    home().join("hooks").join("boundaries.toml")
}

fn override_registry() -> PathBuf {
    home().join("boundaries.toml")
}

/// Where the base layer came from, for the payload.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Installed,
    Embedded,
}

/// Merge `over` onto `base` by `id`: fields replace, a new id appends.
pub fn merge(base: Vec<toml::Table>, over: Vec<toml::Table>) -> Vec<toml::Table> {
    let mut out = base;
    for b in over {
        let id = b.get("id").and_then(|v| v.as_str()).map(str::to_string);
        match out.iter_mut().find(|x| x.get("id").and_then(|v| v.as_str()).map(str::to_string) == id) {
            Some(existing) => existing.extend(b),
            None => out.push(b),
        }
    }
    out
}

fn layer(text: &str) -> Result<Vec<toml::Table>, String> {
    let t: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
    Ok(t.get("boundary")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_table().cloned()).collect())
        .unwrap_or_default())
}

/// The effective registry: base layer, then owner overrides. Errors name the
/// file that did not parse; a parse error in the override keeps the base.
pub fn load() -> (Vec<toml::Table>, Source, Vec<String>) {
    let mut errors = Vec::new();
    let (base, source) = match std::fs::read_to_string(installed_registry()) {
        Ok(t) => match layer(&t) {
            Ok(l) => (l, Source::Installed),
            Err(e) => {
                errors.push(format!("{}: {e}", installed_registry().display()));
                (layer(DEFAULT_REGISTRY).unwrap_or_default(), Source::Embedded)
            }
        },
        Err(_) => (layer(DEFAULT_REGISTRY).unwrap_or_default(), Source::Embedded),
    };
    let merged = match std::fs::read_to_string(override_registry()) {
        Ok(t) => match layer(&t) {
            Ok(over) => merge(base, over),
            Err(e) => {
                errors.push(format!("{}: {e}", override_registry().display()));
                base
            }
        },
        Err(_) => base,
    };
    (merged, source, errors)
}

/// Top-level `apply_to_isolated` (default true), the override file winning.
pub fn applies_to_isolated() -> bool {
    let read = |t: &str| toml::from_str::<toml::Table>(t).ok().and_then(|t| t.get("apply_to_isolated").and_then(|v| v.as_bool()));
    let base = std::fs::read_to_string(installed_registry()).ok().and_then(|t| read(&t)).or_else(|| read(DEFAULT_REGISTRY));
    let over = std::fs::read_to_string(override_registry()).ok().and_then(|t| read(&t));
    over.or(base).unwrap_or(true)
}

fn enabled(b: &toml::Table) -> bool {
    b.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true)
}

fn s<'a>(b: &'a toml::Table, k: &str) -> &'a str {
    b.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

/// The block every non-isolated worker's launch carries: each enabled
/// boundary's title and rule text. Empty when nothing is enabled.
pub fn boundaries_context_block() -> String {
    let (reg, _, _) = load();
    let items: Vec<String> = reg
        .iter()
        .filter(|b| enabled(b))
        .map(|b| format!("## {}\n\n{}", s(b, "title"), s(b, "rule").trim()))
        .collect();
    if items.is_empty() {
        return String::new();
    }
    format!(
        "# Boundaries (binding, enforced)\n\nThese hold for every amux worker. Tool calls that cross one are \
         refused with the reason. `amux boundaries` lists them with their enforcement.\n\n{}",
        items.join("\n\n")
    )
}

/// The guard as Claude settings reference it, and whether it is there.
fn hook_status() -> Value {
    let guard = home().join("hooks").join("boundary-guard.py");
    let settings = dirs_home().join(".claude").join("settings.json");
    let settings_text = std::fs::read_to_string(&settings).ok();
    let wired = settings_text.as_deref().map(|t| t.contains("boundary-guard.py"));
    let installed_text = std::fs::read_to_string(installed_registry()).ok();
    json!({
        "guard_installed": guard.exists(),
        "registry_installed": installed_text.is_some(),
        "registry_matches_build": installed_text.as_deref().map(|t| t == DEFAULT_REGISTRY),
        "wired_in_settings": wired,
        "settings_path": settings.display().to_string(),
        "measured": settings_text.is_some(),
        "why_unmeasured": settings_text.is_none().then_some("could not read Claude settings"),
    })
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Counts per boundary and verdict over the last `since_s` seconds of the
/// guard's log.
pub fn event_counts(log: &Path, since_s: i64) -> (serde_json::Map<String, Value>, usize, bool) {
    let Ok(text) = std::fs::read_to_string(log) else { return (Default::default(), 0, false) };
    let cutoff = chrono::Utc::now().timestamp() as f64 - since_s as f64;
    let mut counts = serde_json::Map::new();
    let mut n = 0;
    for line in text.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let ts = v.get("ts").and_then(|t| t.as_f64()).unwrap_or(0.0);
        if ts < cutoff {
            break;
        }
        n += 1;
        let key = v.get("boundary").and_then(|b| b.as_str()).unwrap_or("_guard").to_string();
        let verdict = v.get("verdict").and_then(|b| b.as_str()).unwrap_or("unknown").to_string();
        let e = counts.entry(key).or_insert_with(|| json!({}));
        let c = e.get(&verdict).and_then(|x| x.as_u64()).unwrap_or(0) + 1;
        e[&verdict] = json!(c);
    }
    (counts, n, true)
}

fn isolated_workers() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(home().join("sessions")) {
        for e in rd.flatten() {
            let f = e.file_name().to_string_lossy().into_owned();
            if let Some(name) = f.strip_suffix(".env") {
                if !name.contains('.') && crate::api::session_verbs::session_is_isolated(name) {
                    out.push(name.to_string());
                }
            }
        }
    }
    out.sort();
    out
}

async fn boundaries() -> impl IntoResponse {
    let (reg, source, errors) = load();
    let log = home().join("logs").join("boundaries.jsonl");
    let (counts, n_events, log_read) = event_counts(&log, 86_400);
    let items: Vec<Value> = reg
        .iter()
        .map(|b| {
            let id = s(b, "id");
            json!({
                "id": id,
                "title": s(b, "title"),
                "kind": b.get("kind").and_then(|v| v.as_str()).unwrap_or("deny"),
                "enabled": enabled(b),
                "rule": s(b, "rule").trim(),
                "enforced_by": b.get("enforced_by").cloned().map(toml_to_json).unwrap_or(json!([])),
                "deny_rules": b.get("deny").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0),
                "last_24h": counts.get(id).cloned().unwrap_or(json!({})),
            })
        })
        .collect();
    let isolated = isolated_workers();
    let covered = applies_to_isolated();
    Json(json!({
        "measured": true,
        "n_considered": items.len(),
        "source": if source == Source::Installed { "installed" } else { "embedded (no ~/.amux/hooks/boundaries.toml)" },
        "override_file": override_registry().display().to_string(),
        "override_present": override_registry().exists(),
        "errors": errors,
        "hook": hook_status(),
        "boundaries": items,
        "guard_errors_24h": counts.get("_guard").cloned().unwrap_or(json!({})),
        "events_24h": { "n": n_events, "log_read": log_read, "log": log.display().to_string() },
        "isolated": {
            "covered_by_hook": covered,
            "n_workers": isolated.len(),
            "why": if covered {
                "apply_to_isolated = true: the guard refuses the same calls for isolated workers (CLAUDE.md names this exception). Rule text is never injected into them."
            } else {
                "apply_to_isolated = false: isolated workers are exempt from the guard. Server gates (vault approval, email external-send hold) still apply."
            },
        },
        "not_covered_by_hook": {
            "isolated_workers": if covered { Vec::new() } else { isolated },
        },
    }))
}

fn toml_to_json(v: toml::Value) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

#[derive(Deserialize)]
struct ResQuery {
    worker: Option<String>,
}

/// A worker's declared resource profile, resolved worker > group > global.
pub fn profile(worker: &str) -> Value {
    let get = |k: &str| crate::api::session_verbs::scoped_setting_in(&home(), worker, k).and_then(|v| v.parse::<f64>().ok());
    let (cpus, mem) = (get("AMUX_WORKER_CPUS"), get("AMUX_WORKER_MEM_GB"));
    json!({
        "declared": cpus.is_some() || mem.is_some(),
        "cpus": cpus,
        "mem_gb": mem,
        "disk_gb": get("AMUX_WORKER_DISK_GB"),
        "how": "Set AMUX_WORKER_CPUS / AMUX_WORKER_MEM_GB for this worker in the Scope tab. A future per-worker VM is sized from the same keys.",
    })
}

/// The worker's scratch-disk reading against its budget (mxp-gs12 review,
/// 2026-10-18). The boundary guard measures ~/.amux/tmp/<worker> (plus
/// AMUX_WORKER_DISK_PATHS) with `du` in a detached process and caches it in
/// ~/.amux/state/disk-usage/<worker>.json; this reads the same file, so the
/// number here is the number a refusal quotes. The budget is the scoped
/// AMUX_WORKER_DISK_GB, else the registry's `disk_budget_gb_default`.
pub fn disk_reading(home: &std::path::Path, worker: &str, profile: &Value) -> Value {
    let budget_gb = profile["disk_gb"].as_f64().or_else(|| {
        let t: toml::Table = toml::from_str(DEFAULT_REGISTRY).ok()?;
        let v = t.get("boundary")?.as_array()?.iter()
            .find(|b| b.get("id").and_then(|v| v.as_str()) == Some("resources"))?
            .get("resources")?.get("disk_budget_gb_default")?.clone();
        v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
    }).filter(|g| *g > 0.0);
    let path = home.join("state").join("disk-usage").join(format!("{worker}.json"));
    let rec: Option<Value> = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok());
    let Some(rec) = rec else {
        return json!({
            "measured": false, "n_considered": 0, "budget_gb": budget_gb,
            "why_unmeasured": "no reading yet: the boundary guard measures on this worker's next heavy command",
        });
    };
    let measured = rec["measured"].as_bool().unwrap_or(false);
    let used = rec["bytes"].as_f64();
    let used_gb = used.map(|b| (b / 1024f64.powi(3) * 100.0).round() / 100.0);
    let fraction = match (used, budget_gb) {
        (Some(u), Some(b)) if b > 0.0 => Some((u / (b * 1024f64.powi(3)) * 1000.0).round() / 1000.0),
        _ => None,
    };
    let age = rec["ts"].as_f64().map(|t| (chrono::Utc::now().timestamp() as f64 - t).round());
    json!({
        "measured": measured, "n_considered": rec["n_considered"].clone(),
        "why_unmeasured": rec["why_unmeasured"].clone(),
        "used_gb": used_gb, "budget_gb": budget_gb, "fraction_of_budget": fraction,
        "paths": rec["paths"].clone(), "age_s": age,
        "over_budget": fraction.map(|f| f >= 1.0),
    })
}

/// Usage against the profile: which declared limits the worker is over.
pub fn over_profile(cpu_percent: f64, rss_mb: f64, profile: &Value) -> Vec<String> {
    let mut over = Vec::new();
    if let Some(c) = profile["cpus"].as_f64() {
        if cpu_percent / 100.0 > c {
            over.push(format!("cpu {:.1} cores over the declared {c}", cpu_percent / 100.0));
        }
    }
    if let Some(m) = profile["mem_gb"].as_f64() {
        if rss_mb / 1024.0 > m {
            over.push(format!("memory {:.1} GB over the declared {m} GB", rss_mb / 1024.0));
        }
    }
    over
}

/// How old the host guard's sample may be before a request takes its own.
const SAMPLE_MAX_AGE_S: i64 = 90;

async fn resources(headers: HeaderMap, Query(q): Query<ResQuery>) -> impl IntoResponse {
    // The process census comes from the host guard's tick (one `ps`, one
    // `tmux list-panes -a`), never from a walk per request. This route used to
    // spawn `pgrep -P` for every process under every worker: 71s at load 87 on
    // 2026-10-17, past the CLI's 30s limit, so `amux resources` failed exactly
    // when the resources boundary told workers to run it.
    let (host, util) = tokio::task::spawn_blocking(|| {
        (crate::api::metrics::collect_system_metrics(), crate::runtime_jobs::host_guard::utilization_fresh(SAMPLE_MAX_AGE_S))
    })
    .await
    .unwrap_or((json!({}), None));
    let pressure = crate::runtime_jobs::host_guard::current();
    let now = chrono::Utc::now().timestamp();
    let workers: Vec<crate::runtime_jobs::host_guard::WorkerUse> = util.as_ref().map(|u| u.workers.clone()).unwrap_or_default();
    let row = |w: &crate::runtime_jobs::host_guard::WorkerUse| {
        json!({ "name": w.name, "cpu_percent": w.cpu_percent, "rss_mb": w.rss_mb, "n_processes": w.n_processes })
    };
    let mut by_mem: Vec<_> = workers.iter().collect();
    by_mem.sort_by(|a, b| b.rss_mb.partial_cmp(&a.rss_mb).unwrap_or(std::cmp::Ordering::Equal));
    let total_rss: f64 = workers.iter().map(|w| w.rss_mb).sum();
    let capacity = util.as_ref().map(|u| u.capacity_percent).unwrap_or(0.0);
    let caller = headers.get("x-amux-session").and_then(|v| v.to_str().ok()).map(str::to_string);
    let worker = q.worker.filter(|w| !w.is_empty()).or(caller).map(|w| {
        let prof = profile(&w);
        match workers.iter().find(|x| x.name == w) {
            Some(x) => json!({
                "name": w, "measured": true, "cpu_percent": x.cpu_percent, "rss_mb": x.rss_mb,
                "n_processes": x.n_processes,
                "share_of_worker_memory": if total_rss > 0.0 { x.rss_mb / total_rss } else { 0.0 },
                "share_of_machine_cpu": if capacity > 0.0 { x.cpu_percent / capacity } else { 0.0 },
                "profile": prof.clone(),
                "over_profile": over_profile(x.cpu_percent, x.rss_mb, &prof),
                "disk": disk_reading(&home(), &w, &prof),
            }),
            None => json!({
                "name": w, "measured": false,
                "why_unmeasured": if util.is_some() { "no live tmux pane for that worker in this sample" } else { "ps did not run, so no worker was measured" },
                "disk": disk_reading(&home(), &w, &prof),
                "profile": prof,
            }),
        }
    });
    let machine = match &util {
        Some(u) => json!({
            "measured": true, "why_unmeasured": null,
            "capacity_percent": u.capacity_percent, "cpu_percent_total": u.cpu_percent_total,
            "workers_cpu_percent": u.workers_cpu_percent, "unattributed_cpu_percent": u.unattributed_cpu_percent,
            "by_kind": u.by_kind, "kinds": u.kinds, "top_unattributed": u.top_unattributed,
            "sample_age_s": now - u.sampled_at, "sample_ms": u.sample_ms,
        }),
        None => json!({ "measured": false, "why_unmeasured": "ps did not run, so CPU could not be attributed" }),
    };
    Json(json!({
        "measured": host.get("cpu_count").is_some() && util.is_some(),
        "n_considered": util.as_ref().map(|u| u.n_processes).unwrap_or(0),
        "host": host,
        "pressure": pressure.as_ref().map(|p| json!({
            "level": p.level.as_str(), "reasons": p.reasons, "measured": p.measured,
            "why_unmeasured": p.why_unmeasured, "sampled_at": p.sampled_at,
        })),
        "workers_running": workers.len(),
        "workers_rss_mb": (total_rss * 10.0).round() / 10.0,
        "top_by_cpu": workers.iter().take(8).map(row).collect::<Vec<_>>(),
        "top_by_memory": by_mem.iter().take(8).map(|w| row(w)).collect::<Vec<_>>(),
        "machine": machine,
        "worker": worker,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_registry_parses_and_every_boundary_has_rule_text() {
        let l = layer(DEFAULT_REGISTRY).expect("boundaries.toml parses");
        assert!(l.len() >= 4, "{} boundaries", l.len());
        for b in &l {
            assert!(!s(b, "id").is_empty() && !s(b, "rule").trim().is_empty(), "{b:?}");
        }
        for id in ["payments", "email", "resources", "boundaries-self"] {
            assert!(l.iter().any(|b| s(b, "id") == id), "missing boundary {id}");
        }
    }

    #[test]
    fn an_override_replaces_fields_by_id_and_can_add_or_disable() {
        let base = layer("[[boundary]]\nid='a'\ntitle='A'\nrule='x'\n[[boundary]]\nid='b'\ntitle='B'\nrule='y'\n").unwrap();
        let over = layer("[[boundary]]\nid='a'\nenabled=false\n[[boundary]]\nid='c'\ntitle='C'\nrule='z'\n").unwrap();
        let m = merge(base, over);
        assert_eq!(m.len(), 3);
        assert!(!enabled(&m[0]) && s(&m[0], "title") == "A");
        assert!(enabled(&m[1]));
        assert_eq!(s(&m[2], "id"), "c");
    }

    #[test]
    fn the_committed_registry_applies_to_isolated_workers() {
        let t: toml::Table = toml::from_str(DEFAULT_REGISTRY).unwrap();
        assert_eq!(t.get("apply_to_isolated").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn over_profile_names_each_exceeded_limit() {
        let p = json!({"cpus": 2.0, "mem_gb": 1.0});
        assert_eq!(over_profile(350.0, 2048.0, &p).len(), 2);
        assert!(over_profile(150.0, 512.0, &p).is_empty());
        assert!(over_profile(900.0, 9000.0, &json!({"cpus": null, "mem_gb": null})).is_empty());
    }

    #[test]
    fn disk_reading_reports_the_guard_cache_against_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let none = disk_reading(dir.path(), "w", &json!({"disk_gb": 10.0}));
        assert_eq!(none["measured"], false);
        assert_eq!(none["budget_gb"], 10.0);
        let d = dir.path().join("state").join("disk-usage");
        std::fs::create_dir_all(&d).unwrap();
        let gb = 1024f64.powi(3);
        std::fs::write(d.join("w.json"), json!({"measured": true, "bytes": 15.0 * gb, "n_considered": 1,
            "paths": {"/x": 15.0 * gb}, "ts": chrono::Utc::now().timestamp()}).to_string()).unwrap();
        let r = disk_reading(dir.path(), "w", &json!({"disk_gb": 10.0}));
        assert_eq!(r["measured"], true);
        assert_eq!(r["over_budget"], true);
        assert_eq!(r["fraction_of_budget"], 1.5);
        let ok = disk_reading(dir.path(), "w", &json!({"disk_gb": 100.0}));
        assert_eq!(ok["over_budget"], false);
        // No scoped budget: the registry default applies.
        let dflt = disk_reading(dir.path(), "w", &json!({"disk_gb": null}));
        assert!(dflt["budget_gb"].as_f64().unwrap_or(0.0) > 0.0, "{dflt}");
    }

    #[test]
    fn event_counts_read_only_the_window() {
        let dir = std::env::temp_dir().join(format!("amux-bnd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("b.jsonl");
        let now = chrono::Utc::now().timestamp() as f64;
        std::fs::write(&log, format!(
            "{{\"ts\":{},\"boundary\":\"email\",\"verdict\":\"boundary_refused\"}}\n{{\"ts\":{},\"boundary\":\"email\",\"verdict\":\"boundary_refused\"}}\n{{\"ts\":{},\"verdict\":\"boundary_registry_unloadable\"}}\n",
            now - 200_000.0, now - 10.0, now - 5.0)).unwrap();
        let (c, n, read) = event_counts(&log, 86_400);
        assert!(read);
        assert_eq!(n, 2);
        assert_eq!(c["email"]["boundary_refused"], 1);
        assert_eq!(c["_guard"]["boundary_registry_unloadable"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
