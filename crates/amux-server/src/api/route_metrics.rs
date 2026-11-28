//! RC-13: per-route request counters, exposed on `GET /api/metrics` as `routes`.
//!
//! Counted in the request-log middleware only for requests the request log
//! itself records (`should_log`: /api paths minus /api/events and /api/debug),
//! so non-/api paths (pages, /pub/<app id>, /f/...) never become labels and
//! cannot fill the label cap. Counters live in
//! memory and start at zero when the server process starts (`uptime_s` on the
//! same endpoint says how long they have been counting).
//!
//! Labels are coarse on purpose: METHOD plus the first two path segments, with
//! `/*` appended when the path goes deeper. `GET /api/sessions/alpha/history`
//! and `GET /api/sessions` are therefore `GET /api/sessions/*` and
//! `GET /api/sessions`. Worker names never become labels, so the label set
//! stays small. At most [`MAX_ROUTES`] distinct labels are kept; any further
//! label is counted under [`OVERFLOW_LABEL`].

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

pub const MAX_ROUTES: usize = 256;
pub const OVERFLOW_LABEL: &str = "(other)";

fn counters() -> &'static Mutex<BTreeMap<String, u64>> {
    static COUNTERS: OnceLock<Mutex<BTreeMap<String, u64>>> = OnceLock::new();
    COUNTERS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// The coarse label for a request: METHOD and the first two path segments.
pub fn route_label(method: &str, path: &str) -> String {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut label = format!("{method} /");
    label.push_str(
        &segments
            .iter()
            .take(2)
            .copied()
            .collect::<Vec<_>>()
            .join("/"),
    );
    if segments.len() > 2 {
        label.push_str("/*");
    }
    label
}

/// Count one request for `method` `path`.
pub fn record(method: &str, path: &str) {
    let label = route_label(method, path);
    let mut map = counters().lock().unwrap_or_else(|p| p.into_inner());
    record_into(&mut map, label);
}

/// Count `label` in `map`, folding new labels into [`OVERFLOW_LABEL`] once
/// [`MAX_ROUTES`] distinct labels exist. Split out so the cap is testable on a
/// fresh map.
fn record_into(map: &mut BTreeMap<String, u64>, label: String) {
    if let Some(n) = map.get_mut(&label) {
        *n += 1;
    } else if map.len() < MAX_ROUTES {
        map.insert(label, 1);
    } else {
        *map.entry(OVERFLOW_LABEL.to_string()).or_insert(0) += 1;
    }
}

/// Snapshot for the metrics endpoint: per-label counts plus their total.
pub fn snapshot() -> Value {
    let map = counters().lock().unwrap_or_else(|p| p.into_inner());
    let total: u64 = map.values().sum();
    json!({
        "total": total,
        "by_route": map.iter().map(|(k, v)| (k.clone(), json!(v))).collect::<serde_json::Map<_, _>>(),
        "scope": "in-memory, since this server process started",
    })
}

#[cfg(test)]
mod tests {
    use super::{record, record_into, route_label, snapshot, MAX_ROUTES, OVERFLOW_LABEL};
    use std::collections::BTreeMap;

    #[test]
    fn labels_collapse_worker_names_and_verbs() {
        assert_eq!(route_label("GET", "/api/sessions"), "GET /api/sessions");
        assert_eq!(
            route_label("GET", "/api/sessions/alpha/history"),
            "GET /api/sessions/*"
        );
        assert_eq!(
            route_label("GET", "/api/sessions/alpha"),
            "GET /api/sessions/*"
        );
        assert_eq!(
            route_label("POST", "/api/workers/w-1/x/y"),
            "POST /api/workers/*"
        );
        assert_eq!(route_label("GET", "/api"), "GET /api");
    }

    #[test]
    fn counts_accumulate_per_label_and_total() {
        // Unique label prefix so parallel tests do not interfere.
        record("GET", "/api/rtest-counter-zz");
        record("GET", "/api/rtest-counter-zz/a/b");
        let snap = snapshot();
        let by = snap["by_route"].as_object().unwrap();
        assert_eq!(by["GET /api/rtest-counter-zz"].as_u64().unwrap(), 1);
        assert_eq!(by["GET /api/rtest-counter-zz/*"].as_u64().unwrap(), 1);
        assert!(snap["total"].as_u64().unwrap() >= 2);
    }

    #[test]
    fn cap_folds_new_labels_into_overflow_on_a_fresh_map() {
        let mut map = BTreeMap::new();
        for i in 0..MAX_ROUTES {
            record_into(&mut map, format!("GET /api/r{i}"));
        }
        assert_eq!(map.len(), MAX_ROUTES);
        record_into(&mut map, "GET /api/new".to_string());
        record_into(&mut map, "GET /api/new2".to_string());
        assert_eq!(map[OVERFLOW_LABEL], 2);
        assert!(!map.contains_key("GET /api/new"));
        // An existing label keeps counting under its own name past the cap.
        record_into(&mut map, "GET /api/r0".to_string());
        assert_eq!(map["GET /api/r0"], 2);
    }
}
