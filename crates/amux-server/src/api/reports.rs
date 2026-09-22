//! /api/reports — the Metrics tab's report cards (AMUX-2884).
//!
//! A PORT of a live contract: the SPA's Metrics tab lists/creates/refreshes
//! report cards and has 404'd since the python retirement, while the `reports`
//! table still holds real rows (2 on this machine). Contract from the SPA's
//! own calls + the on-disk python registry (cloud/docker/amux-server.py:1785):
//!
//!   GET    /api/reports/types           registry metadata (no fetch fns)
//!   GET    /api/reports                 list, ordered position,created
//!   POST   /api/reports                 create -> the full row (201)
//!   DELETE /api/reports/{id}
//!   PATCH  /api/reports/{id}            rename ({name})
//!   POST   /api/reports/{id}/refresh    run fetchers, cache, return data
//!   GET    /api/reports/{id}/data       cached_data + last_refresh
//!
//! VERIFICATION BOUNDARY, stated because it is the honest shape of this port:
//! CRUD + /types + /data are pure DB and fully verified. REFRESH depends on
//! vendor credentials that are NOT in this machine's server.env, so the
//! fetchers are exercised only on their credential-absent ERROR path here —
//! which is the real behavior in amux's env. The two ops-server fetchers the
//! live reports use (mixpeek-vendor-spend, posthog-analytics) are simple bearer
//! GETs, ported faithfully; when AMUX_MIXPEEK_OPS_URL/TOKEN are set they work.
//!
//! The five `infra-spend` vendor fetchers (gcp/anyscale/render/mongo/qdrant)
//! are each a bespoke billing-API client, NONE verifiable without that vendor's
//! creds, and NO report row uses the infra-spend type. Rather than ship ~140
//! lines of blind vendor code, refresh returns a LOUD per-vendor "not ported"
//! (ethos rule 3 / AMUX-2626: no silent fallback) — a follow-up ports them
//! against real creds. The type still appears in /types so nothing about the
//! UI silently loses an option.

use super::{internal, AppState};
use crate::db::WriteOutcome;
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/types", get(types))
        .route("/", get(list).post(create))
        .route("/{id}", axum::routing::delete(delete_report).patch(rename))
        .route("/{id}/refresh", post(refresh))
        .route("/{id}/data", get(data))
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

// ---- registry --------------------------------------------------------------
//
// The three python report types, verbatim metadata. A `Single` fetch is one
// ops-server call for the whole report; `PerVendor` calls each vendor's
// fetcher. `env_vars` are surfaced so the UI can tell a user which creds a
// vendor needs — the python exposed them the same way.

struct Vendor {
    id: &'static str,
    label: &'static str,
    color: &'static str,
    env_vars: &'static [&'static str],
}
struct ReportType {
    id: &'static str,
    label: &'static str,
    description: &'static str,
    display: Option<&'static str>,
    /// None => per-vendor fetch; Some => one ops-server fetch for the report.
    single: Option<SingleKind>,
    vendors: &'static [Vendor],
}
#[derive(Clone, Copy)]
enum SingleKind {
    MixpeekOpsSpend,
    PosthogAnalytics,
}

const REGISTRY: &[ReportType] = &[
    ReportType {
        id: "infra-spend",
        label: "Infrastructure Spend",
        description: "Aggregate cloud & infrastructure spend across vendors (daily/weekly/monthly)",
        display: None,
        single: None,
        vendors: &[
            Vendor { id: "gcp", label: "GCP", color: "#4285F4", env_vars: &["AMUX_GCP_SA_KEY_PATH", "AMUX_GCP_BILLING_ACCOUNT"] },
            Vendor { id: "anyscale", label: "Anyscale", color: "#FF6B35", env_vars: &["AMUX_ANYSCALE_API_KEY"] },
            Vendor { id: "render", label: "Render", color: "#46E3B7", env_vars: &["AMUX_RENDER_API_KEY"] },
            Vendor { id: "mongo", label: "MongoDB", color: "#47A248", env_vars: &["AMUX_MONGO_PUBLIC_KEY", "AMUX_MONGO_PRIVATE_KEY", "AMUX_MONGO_ORG_ID"] },
            Vendor { id: "qdrant", label: "Qdrant", color: "#DC244C", env_vars: &["AMUX_QDRANT_CLOUD_API_KEY"] },
        ],
    },
    ReportType {
        id: "mixpeek-vendor-spend",
        label: "Mixpeek Vendor Spend",
        description: "Infrastructure vendor spend via Mixpeek ops server (Render, GCP Cloud Run, MongoDB Atlas, GKE Autopilot, Qdrant Cloud). Config: months (default 12), ops_url, ops_token.",
        display: None,
        single: Some(SingleKind::MixpeekOpsSpend),
        vendors: &[
            Vendor { id: "render", label: "Render", color: "#46E3B7", env_vars: &["AMUX_MIXPEEK_OPS_URL", "AMUX_MIXPEEK_OPS_TOKEN"] },
            Vendor { id: "gcp_cloud_run", label: "GCP Cloud Run", color: "#4285F4", env_vars: &[] },
            Vendor { id: "mongodb_atlas", label: "MongoDB Atlas", color: "#47A248", env_vars: &[] },
            Vendor { id: "gke", label: "GKE Autopilot", color: "#FF6B35", env_vars: &[] },
            Vendor { id: "qdrant_cloud", label: "Qdrant Cloud", color: "#DC244C", env_vars: &[] },
        ],
    },
    ReportType {
        id: "posthog-analytics",
        label: "PostHog Analytics",
        description: "Product analytics — PostHog active/new users, total events, plus auth signups and new orgs from MongoDB (daily/weekly/monthly). Config: days (default 90), ops_url, ops_token.",
        display: Some("count"),
        single: Some(SingleKind::PosthogAnalytics),
        vendors: &[
            Vendor { id: "active_users", label: "Active Users", color: "#F64E0F", env_vars: &[] },
            Vendor { id: "new_users", label: "New Users", color: "#1D4ED8", env_vars: &[] },
            Vendor { id: "total_events", label: "Total Events", color: "#059669", env_vars: &[] },
            Vendor { id: "auth_signups", label: "Auth Signups", color: "#7C3AED", env_vars: &[] },
            Vendor { id: "auth_new_orgs", label: "New Orgs", color: "#D97706", env_vars: &[] },
        ],
    },
];

fn find_type(id: &str) -> Option<&'static ReportType> {
    REGISTRY.iter().find(|t| t.id == id)
}

// ---- GET /api/reports/types ------------------------------------------------

async fn types() -> Response {
    let mut out = serde_json::Map::new();
    for t in REGISTRY {
        let vendors: serde_json::Map<String, Value> = t
            .vendors
            .iter()
            .map(|v| {
                (
                    v.id.to_string(),
                    json!({ "label": v.label, "color": v.color, "env_vars": v.env_vars }),
                )
            })
            .collect();
        let mut meta = json!({
            "label": t.label,
            "description": t.description,
            "vendors": vendors,
        });
        if let Some(d) = t.display {
            meta["display"] = json!(d);
        }
        out.insert(t.id.to_string(), meta);
    }
    Json(Value::Object(out)).into_response()
}

// ---- GET /api/reports ------------------------------------------------------

fn row_json(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_, String>(0)?,
        "name": r.get::<_, String>(1)?,
        "type": r.get::<_, String>(2)?,
        "config": r.get::<_, String>(3)?,
        "position": r.get::<_, i64>(4)?,
        "created": r.get::<_, i64>(5)?,
        "last_refresh": r.get::<_, Option<i64>>(6)?,
        "cached_data": r.get::<_, Option<String>>(7)?,
    }))
}
const COLS: &str = "id,name,type,config,position,created,last_refresh,cached_data";

async fn list(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let conn = store.read()?;
        let mut st = conn.prepare(&format!(
            "SELECT {COLS} FROM reports ORDER BY position, created"
        ))?;
        let mut rows = st.query([])?;
        let mut out = vec![];
        while let Some(r) = rows.next()? {
            out.push(row_json(r)?);
        }
        Ok(Value::Array(out))
    })
    .await;
    match res {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(anyhow::anyhow!("join: {e}")),
    }
}

// ---- POST /api/reports -----------------------------------------------------

async fn create(State(state): State<AppState>, body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("New Report")
        .trim()
        .to_string();
    let rtype = body
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("infra-spend")
        .to_string();
    let config = body.get("config").cloned().unwrap_or(json!({})).to_string();
    let position = body.get("position").and_then(Value::as_i64).unwrap_or(0);
    // id = rpt-<ms>, python parity. The one millis stamp we need — passed in
    // is impossible here, so derive from the second clock * 1000 + a counter
    // so two creates in the same second still differ.
    let id = format!("rpt-{}", now_secs() * 1000 + next_suffix());
    // reports is a leaf table with no revision/event surface, so events is
    // empty — but the INSERT still goes through the WRITER (read() is a
    // read-only pool connection), or it would fail / bypass write serialization.
    let slot: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let id_w = id.clone();
    let res = state
        .store
        .write_async(move |conn| {
            let now = now_secs();
            conn.execute(
                "INSERT INTO reports (id,name,type,config,position,created) VALUES (?1,?2,?3,?4,?5,?6)",
                rusqlite::params![&id_w, &name, &rtype, &config, position, now],
            )?;
            let mut st = conn.prepare(&format!("SELECT {COLS} FROM reports WHERE id=?1"))?;
            let mut rows = st.query([&id_w])?;
            if let Some(r) = rows.next()? {
                *slot_w.lock().expect("slot") = Some(row_json(r)?);
            }
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    match res {
        Ok(_) => {
            let v = slot
                .lock()
                .expect("slot")
                .take()
                .unwrap_or(json!({ "id": id }));
            (StatusCode::CREATED, Json(v)).into_response()
        }
        Err(e) => internal(e),
    }
}

fn next_suffix() -> i64 {
    use std::sync::atomic::{AtomicI64, Ordering};
    static N: AtomicI64 = AtomicI64::new(0);
    N.fetch_add(1, Ordering::Relaxed) % 1000
}

// ---- DELETE /api/reports/{id} ----------------------------------------------

async fn delete_report(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    let res = state
        .store
        .write_async(move |conn| {
            conn.execute("DELETE FROM reports WHERE id=?1", [&id])?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await;
    match res {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

// ---- PATCH /api/reports/{id} (rename) --------------------------------------

async fn rename(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let slot: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let res = state
        .store
        .write_async(move |conn| {
            if let Some(name) = body.get("name").and_then(Value::as_str) {
                conn.execute(
                    "UPDATE reports SET name=?1 WHERE id=?2",
                    rusqlite::params![name, &id],
                )?;
            }
            let mut st = conn.prepare(&format!("SELECT {COLS} FROM reports WHERE id=?1"))?;
            let mut rows = st.query([&id])?;
            if let Some(r) = rows.next()? {
                *slot_w.lock().expect("slot") = Some(row_json(r)?);
            }
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await;
    match res {
        Ok(_) => match slot.lock().expect("slot").take() {
            Some(v) => Json(v).into_response(),
            None => (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response(),
        },
        Err(e) => internal(e),
    }
}

// ---- GET /api/reports/{id}/data --------------------------------------------

async fn data(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    let store = state.store.clone();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        let conn = store.read()?;
        let mut st = conn.prepare("SELECT cached_data, last_refresh FROM reports WHERE id=?1")?;
        let mut rows = st.query([&id])?;
        match rows.next()? {
            Some(r) => {
                let cached: Option<String> = r.get(0)?;
                let refreshed: Option<i64> = r.get(1)?;
                let parsed = cached
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .unwrap_or(json!({}));
                Ok(Some(json!({ "data": parsed, "refreshed_at": refreshed })))
            }
            None => Ok(None),
        }
    })
    .await;
    match res {
        Ok(Ok(Some(v))) => Json(v).into_response(),
        Ok(Ok(None)) => {
            (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response()
        }
        Ok(Err(e)) => internal(e),
        Err(e) => internal(anyhow::anyhow!("join: {e}")),
    }
}

// ---- POST /api/reports/{id}/refresh ----------------------------------------

async fn refresh(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    // Load the report (type + config) first.
    let store = state.store.clone();
    let id_r = id.clone();
    let loaded = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<(String, Value)>> {
        let conn = store.read()?;
        let mut st = conn.prepare("SELECT type, config FROM reports WHERE id=?1")?;
        let mut rows = st.query([&id_r])?;
        match rows.next()? {
            Some(r) => {
                let rtype: String = r.get(0)?;
                let cfg_s: String = r.get(1)?;
                let cfg = serde_json::from_str::<Value>(&cfg_s).unwrap_or(json!({}));
                Ok(Some((rtype, cfg)))
            }
            None => Ok(None),
        }
    })
    .await;
    let (rtype, config) = match loaded {
        Ok(Ok(Some(v))) => v,
        Ok(Ok(None)) => {
            return (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response()
        }
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(anyhow::anyhow!("join: {e}")),
    };
    let Some(t) = find_type(&rtype) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("unknown report type '{rtype}'") })),
        )
            .into_response();
    };

    // Run the fetch(es) — async HTTP, off the DB thread.
    let results = match t.single {
        Some(kind) => ops_fetch(kind, &config).await,
        None => {
            // Per-vendor (infra-spend): honest not-ported per vendor.
            let mut m = serde_json::Map::new();
            for v in t.vendors {
                m.insert(v.id.to_string(), json!({
                    "name": v.id,
                    "error": format!(
                        "vendor '{}' fetcher not yet ported to rust — needs {:?} + a verification pass with real creds (AMUX-2884 follow-up)",
                        v.id, v.env_vars
                    ),
                    "daily": [], "monthly": [],
                }));
            }
            Value::Object(m)
        }
    };

    // Cache it (through the writer, not a read-only connection).
    let now = now_secs();
    let cached = results.to_string();
    let id_w = id.clone();
    let _ = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "UPDATE reports SET last_refresh=?1, cached_data=?2 WHERE id=?3",
                rusqlite::params![now, &cached, &id_w],
            )?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await;
    Json(json!({ "ok": true, "data": results, "refreshed_at": now })).into_response()
}

/// The two ops-server fetchers, which differ only in endpoint + vendor set +
/// default period. Faithful port of `_report_fetch_{mixpeek_ops,posthog}_all`:
/// one bearer GET to the Mixpeek ops server; on any failure, the same
/// per-vendor error shape the python returned (so the UI renders per-vendor
/// error rather than a blank card).
async fn ops_fetch(kind: SingleKind, config: &Value) -> Value {
    let (path, period_key, period_default, vendors): (&str, &str, i64, &[&str]) = match kind {
        SingleKind::MixpeekOpsSpend => (
            "/api/dashboard/spend",
            "months",
            12,
            &[
                "render",
                "gcp_cloud_run",
                "mongodb_atlas",
                "gke",
                "qdrant_cloud",
            ],
        ),
        SingleKind::PosthogAnalytics => (
            "/api/dashboard/posthog",
            "days",
            90,
            &["active_users", "new_users", "total_events"],
        ),
    };
    let s = |k: &str, env: &str| -> String {
        config
            .get(k)
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| std::env::var(env).unwrap_or_default())
    };
    let url = s("ops_url", "AMUX_MIXPEEK_OPS_URL");
    let token = s("ops_token", "AMUX_MIXPEEK_OPS_TOKEN");
    let period = config
        .get(period_key)
        .and_then(Value::as_i64)
        .unwrap_or(period_default);
    let err_shape = |msg: &str| -> Value {
        let mut m = serde_json::Map::new();
        for v in vendors {
            m.insert(
                v.to_string(),
                json!({ "name": v, "error": msg, "daily": [], "monthly": [] }),
            );
        }
        Value::Object(m)
    };
    if url.is_empty() {
        return err_shape("AMUX_MIXPEEK_OPS_URL not set");
    }
    // Self-signed ops server (python used CERT_NONE); accept invalid certs to
    // match. Only reached once a URL is configured.
    let client = match reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(60))
        .build()
    {
        Ok(c) => c,
        Err(e) => return err_shape(&e.to_string()),
    };
    let full = format!(
        "{}{}?{}={}",
        url.trim_end_matches('/'),
        path,
        period_key,
        period
    );
    match client
        .get(&full)
        .bearer_auth(&token)
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(resp) => {
            // NON-2xx IS AN ERROR, python-parity. urllib.urlopen RAISES on
            // 4xx/5xx, so the python fetcher returned its per-vendor err_shape
            // for an auth failure; reqwest does not raise, so without this an
            // ops-server "401 {detail: Invalid token}" would leak straight
            // through as the report data and the SPA would render a card it
            // cannot parse (caught end-to-end: an expired ops token returned
            // exactly that body). Surface the status + body as the per-vendor
            // error instead.
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                return err_shape(&format!("ops server {}: {}", status.as_u16(), body.trim()));
            }
            match serde_json::from_str::<Value>(&body) {
                Ok(v) => v,
                Err(e) => err_shape(&format!("ops server returned non-JSON: {e}")),
            }
        }
        Err(e) => err_shape(&e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_the_three_python_types() {
        assert!(find_type("infra-spend").is_some());
        assert!(find_type("mixpeek-vendor-spend").is_some());
        assert!(find_type("posthog-analytics").is_some());
        assert!(find_type("nope").is_none());
    }

    /// The credential-absent path is the ACTUAL behavior in amux's env, so it
    /// is the one that must be right: a per-vendor error, never a blank/silent
    /// success.
    #[tokio::test]
    async fn ops_fetch_without_url_returns_per_vendor_error() {
        // Ensure the env var is unset for this check.
        std::env::remove_var("AMUX_MIXPEEK_OPS_URL");
        let v = ops_fetch(SingleKind::PosthogAnalytics, &json!({})).await;
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("active_users"));
        assert_eq!(
            obj["active_users"]["error"],
            json!("AMUX_MIXPEEK_OPS_URL not set")
        );
    }
}
