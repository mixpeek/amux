//! What a worker may do: one resolver over the facts amux already keeps
//! (governance PR 11).
//!
//! A capability is "a credential and its limits" (VISION A2), not a new
//! object (A1). Most of them already have a home, and this module reads that
//! home rather than keeping a second copy (A10):
//!
//! | capability | source of truth |
//! |---|---|
//! | `connectors` | the `connectors` scope layer (`scope::effective_connectors`, PR 4's entitlement) |
//! | `outbound` | `AMUX_EMAIL_EXTERNAL_ALLOW` / `AMUX_EMAIL_EXTERNAL_EXEMPT` (`email_scope::bypass_rule`) |
//! | `spend` | the vault items' own limits (`vault.rs`) |
//! | `folders` | NEW: `AMUX_CAP_FOLDERS`, on the worker, group or global env layer |
//! | `publish` | NEW: `AMUX_CAP_PUBLISH` = `public` (default) / `internal` / `none` |
//! | `message` | the group boundary, widened by NEW `AMUX_CAP_MESSAGE` (workers or `group:<name>`) |
//!
//! Broad by default. Inside its own folder a worker is never limited, and a
//! folder no worker owns is open. The one new default limit is the customer
//! boundary: a path inside ANOTHER worker's folder (one that is not an
//! ancestor of the worker's own) needs a grant. No tool or website lists live
//! here (A2: amux is never the ceiling on the model).
//!
//! `AMUX_CAP_*` keys are owner-only (`scope::OWNER_ONLY_ENV_PREFIXES`): a
//! worker writing its own layer cannot grant itself one.
//!
//! `AMUX_CAPABILITIES` (A11):
//! - `report` (default): a boundary crossing is allowed, counted and logged
//!   `capability_would_refuse`. No card is filed.
//! - `enforce`: refused with a 403 naming the capability and how to grant it,
//!   and ONE Needs-you card per (worker, capability) asks the owner (ethos 3:
//!   every refusal has a truthful next step). Each refusal leaves a policy
//!   receipt, which the ledger copies.
//! - `off`: the new folder and publish checks are skipped; refusals that
//!   existed before this module (group boundary, connector entitlement) are
//!   unchanged but file no card.

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

pub use super::worker_identity::Mode;

pub const MODE_ENV: &str = "AMUX_CAPABILITIES";
pub const FOLDERS_KEY: &str = "AMUX_CAP_FOLDERS";
pub const PUBLISH_KEY: &str = "AMUX_CAP_PUBLISH";
pub const MESSAGE_KEY: &str = "AMUX_CAP_MESSAGE";

pub fn mode() -> Mode {
    Mode::parse(std::env::var(MODE_ENV).ok().as_deref())
}

static STORE: OnceLock<crate::db::SharedStore> = OnceLock::new();

/// Called once by the router so refusals can file their card.
pub fn install_store(store: crate::db::SharedStore) {
    let _ = STORE.set(store);
}

// ── Layers ──────────────────────────────────────────────────────────────────

/// One value of a key and the layer it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layered {
    pub value: String,
    pub layer: String,
}

fn env_value(path: &Path, key: &str) -> Option<String> {
    super::session_verbs::EnvFile::load(path)
        .get(key)
        .map(|v| v.trim().trim_matches('"').trim_matches('\'').trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Every layer that sets `key` for `lane`, most specific first: the worker,
/// then each of its groups (its own `CC_TAGS`), then the global `amux.env`.
pub fn layers(home: &Path, lane: &str, key: &str) -> Vec<Layered> {
    let mut out = Vec::new();
    if let Some(v) = env_value(&home.join("sessions").join(format!("{lane}.env")), key) {
        out.push(Layered { value: v, layer: format!("worker {lane}") });
    }
    for g in super::scope::session_tags_of(home, lane) {
        if let Some(v) = env_value(&home.join("env").join(format!("{g}.env")), key) {
            out.push(Layered { value: v, layer: format!("group {g}") });
        }
    }
    if let Some(v) = env_value(&home.join("amux.env"), key) {
        out.push(Layered { value: v, layer: "global amux.env".into() });
    }
    out
}

fn split_list(v: &str) -> Vec<String> {
    v.split([',', ';', '\n']).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

// ── Folders ─────────────────────────────────────────────────────────────────

fn expand(home_dir: &Path, raw: &str) -> PathBuf {
    let raw = raw.trim();
    let p = if raw == "~" {
        home_dir.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home_dir.join(rest)
    } else {
        PathBuf::from(raw)
    };
    normalize(&p)
}

/// The path with `.` and `..` folded and symlinks resolved as far as the path
/// exists, so `customers/wexus/../rothco` and a link into another folder are
/// judged where they land.
pub fn normalize(p: &Path) -> PathBuf {
    let mut lexical = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                lexical.pop();
            }
            Component::CurDir => {}
            other => lexical.push(other.as_os_str()),
        }
    }
    // Resolve the longest existing prefix, then re-append the rest.
    let mut existing = lexical.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(c) = existing.canonicalize() {
            let mut out = c;
            for t in tail.iter().rev() {
                out.push(t);
            }
            return out;
        }
        match (existing.file_name().map(|s| s.to_os_string()), existing.parent().map(Path::to_path_buf)) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                existing = parent;
            }
            _ => return lexical,
        }
    }
}

fn user_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// (worker, folder) for every worker that has one, archived ones included:
/// an archived customer's files are still that customer's.
pub fn worker_folders(home: &Path) -> Vec<(String, PathBuf)> {
    let hd = user_home();
    let Ok(rd) = std::fs::read_dir(home.join("sessions")) else { return vec![] };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("env") {
            continue;
        }
        let Some(name) = p.file_stem().and_then(|s| s.to_str()).map(String::from) else { continue };
        if let Some(dir) = env_value(&p, "CC_DIR") {
            out.push((name, expand(&hd, &dir)));
        }
    }
    out.sort();
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderDecision {
    /// Allowed, and why (own folder, a grant and its layer, unowned...).
    Allowed(String),
    /// Inside another worker's folder with no grant.
    Boundary { owner: String, owner_dir: PathBuf },
}

/// May `lane` touch `path`?
pub fn folder_decision(home: &Path, lane: &str, path: &Path) -> FolderDecision {
    let hd = user_home();
    let target = normalize(path);
    for l in layers(home, lane, FOLDERS_KEY) {
        for g in split_list(&l.value) {
            if g == "*" {
                return FolderDecision::Allowed(format!("{FOLDERS_KEY}=* ({})", l.layer));
            }
            if target.starts_with(expand(&hd, &g)) {
                return FolderDecision::Allowed(format!("{FOLDERS_KEY} {g} ({})", l.layer));
            }
        }
    }
    let folders = worker_folders(home);
    let own = folders.iter().find(|(n, _)| n == lane).map(|(_, d)| d.clone());
    // The deepest folder that holds the target decides whose it is.
    let deepest = folders
        .iter()
        .filter(|(_, d)| target.starts_with(d))
        .max_by_key(|(_, d)| d.components().count());
    let Some((owner, owner_dir)) = deepest else {
        return FolderDecision::Allowed("outside every worker's folder".into());
    };
    if owner == lane || own.as_ref() == Some(owner_dir) {
        return FolderDecision::Allowed("the worker's own folder".into());
    }
    // A folder that CONTAINS the worker's own (a home-folder worker, a repo
    // root) is shared ground, not another customer's.
    if own.as_ref().is_some_and(|o| o.starts_with(owner_dir)) {
        return FolderDecision::Allowed(format!("{owner}'s folder contains this worker's own"));
    }
    FolderDecision::Boundary { owner: owner.clone(), owner_dir: owner_dir.clone() }
}

// ── Publish and message ────────────────────────────────────────────────────

/// `public` (default), `internal` or `none`, and the layer that set it.
pub fn publish_level(home: &Path, lane: &str) -> (String, String) {
    match layers(home, lane, PUBLISH_KEY).into_iter().next() {
        Some(l) => {
            let v = l.value.to_ascii_lowercase();
            let v = if matches!(v.as_str(), "public" | "internal" | "none") { v } else { "none".into() };
            (v, l.layer)
        }
        None => ("public".into(), "default (publish waits for a person's approval)".into()),
    }
}

/// Does a link of `kind` fit the publish level? `internal` admits internal
/// links only; `none` admits nothing.
pub fn publish_allows(level: &str, kind: &str) -> bool {
    match level {
        "public" => true,
        "internal" => kind == "internal",
        _ => false,
    }
}

/// Extra targets `lane` may message beyond its groups: worker names and
/// `group:<name>` entries, union over layers.
pub fn message_grants(home: &Path, lane: &str) -> Vec<Layered> {
    let mut out = Vec::new();
    for l in layers(home, lane, MESSAGE_KEY) {
        for v in split_list(&l.value) {
            out.push(Layered { value: v, layer: l.layer.clone() });
        }
    }
    out
}

/// Is `target` (in `target_groups`) among `origin`'s message grants?
pub fn message_granted(home: &Path, origin: &str, target: &str, target_groups: &BTreeSet<String>) -> Option<String> {
    message_grants(home, origin).into_iter().find_map(|g| {
        let hit = if g.value == "*" {
            true
        } else if let Some(group) = g.value.strip_prefix("group:") {
            target_groups.contains(group)
        } else {
            g.value.eq_ignore_ascii_case(target)
        };
        hit.then(|| format!("{MESSAGE_KEY} {} ({})", g.value, g.layer))
    })
}

// ── The view ────────────────────────────────────────────────────────────────

/// The effective capabilities of `lane`, each with where it came from.
pub fn for_worker(home: &Path, conn: Option<&rusqlite::Connection>, lane: &str) -> Value {
    let groups = super::scope::session_tags_of(home, lane);
    let own = worker_folders(home).into_iter().find(|(n, _)| n == lane).map(|(_, d)| d);
    let connectors = match super::scope::effective_connectors(conn, home, lane) {
        Ok(m) => json!({"measured": true, "entitled": Value::Object(m),
            "source": "connectors scope (global, then groups, then worker)"}),
        Err(e) => json!({"measured": false, "error": e}),
    };
    let outbound = match super::email_scope::bypass_rule(home, lane) {
        Some(rule) => json!({"value": "allow", "source": rule}),
        None => json!({"value": "held", "source": "default: external email waits for a person's approval"}),
    };
    let (publish, publish_src) = publish_level(home, lane);
    let folder_grants: Vec<Value> = layers(home, lane, FOLDERS_KEY)
        .into_iter()
        .map(|l| json!({"paths": split_list(&l.value), "source": l.layer}))
        .collect();
    let message: Vec<Value> = message_grants(home, lane)
        .into_iter()
        .map(|g| json!({"target": g.value, "source": g.layer}))
        .collect();
    json!({
        "worker": lane,
        "measured": true,
        "mode": mode().as_str(),
        "groups": groups,
        "connectors": connectors,
        "folders": {
            "own": own.map(|p| p.to_string_lossy().into_owned()),
            "grants": folder_grants,
            "default": "the worker's own folder, any folder containing it, and every folder no worker owns; \
                        another worker's folder needs AMUX_CAP_FOLDERS",
        },
        "outbound": outbound,
        "spend": {"source": "vault items' max_usd_without_approval and spend_limits_usd (GET /api/vault)"},
        "publish": {"value": publish, "source": publish_src},
        "message": {"groups": groups, "grants": message,
            "default": "workers sharing a group, open groups, and the worker's own Chat tab"},
    })
}

// ── Refusal ─────────────────────────────────────────────────────────────────

pub(crate) mod counters {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    static C: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());
    pub fn bump(key: &str) {
        if let Ok(mut m) = C.lock() {
            *m.entry(key.to_string()).or_default() += 1;
        }
    }
    pub fn snapshot() -> BTreeMap<String, u64> {
        C.lock().map(|m| m.clone()).unwrap_or_default()
    }
}

/// What to do about a crossing in the current mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    WouldRefuse,
    Refuse,
}

pub fn verdict_for(mode: Mode) -> Verdict {
    match mode {
        Mode::Off => Verdict::Allow,
        Mode::Report => Verdict::WouldRefuse,
        Mode::Enforce => Verdict::Refuse,
    }
}

/// How the owner grants `capability` to `lane`.
pub fn grant_hint(capability: &str, lane: &str, detail: &str) -> String {
    match capability {
        "folders" => format!(
            "set {FOLDERS_KEY}={detail} on worker {lane} (PUT /api/scope level=worker name={lane} cap=env), \
             or on one of its groups"
        ),
        "publish" => format!("set {PUBLISH_KEY}=public (or internal) on worker {lane}"),
        "message" => format!("set {MESSAGE_KEY}={detail} on worker {lane}, or put both workers in one group"),
        "connectors" => format!("enable {detail} for worker {lane} in its Connectors scope"),
        _ => format!("grant {capability} to {lane}"),
    }
}

fn asks_file(home: &Path) -> PathBuf {
    home.join("capability-asks.json")
}

fn ask_key(lane: &str, capability: &str) -> String {
    format!("{lane}\u{0}{capability}")
}

/// Record a crossing: counted, logged, receipted, and in enforce mode a card
/// is filed (once per worker and capability). Returns the 403 to send when
/// the mode refuses, `None` when the request goes ahead.
pub fn cross(home: &Path, lane: &str, capability: &str, resource: &str, detail: &str) -> Option<Response> {
    cross_with(home, lane, capability, resource, detail, mode())
}

pub fn cross_with(home: &Path, lane: &str, capability: &str, resource: &str, detail: &str, mode: Mode) -> Option<Response> {
    let v = verdict_for(mode);
    let hint = grant_hint(capability, lane, detail);
    match v {
        Verdict::Allow => {
            counters::bump(&format!("{capability}.unchecked"));
            None
        }
        Verdict::WouldRefuse => {
            counters::bump(&format!("{capability}.would_refuse"));
            tracing::warn!(worker = lane, capability, resource, measured = true, n_considered = 1,
                verdict = "capability_would_refuse", "a worker crossed a capability boundary (report mode: allowed)");
            receipt(lane, capability, resource, "would_refuse", &hint);
            None
        }
        Verdict::Refuse => {
            counters::bump(&format!("{capability}.refused"));
            tracing::warn!(worker = lane, capability, resource, measured = true, n_considered = 1,
                verdict = "capability_refused", "a worker was refused outside its capabilities");
            receipt(lane, capability, resource, "deny", &hint);
            file_card(home, lane, capability, resource, &hint, grant_write(home, lane, capability, detail));
            Some(refusal(lane, capability, resource, &hint))
        }
    }
}

/// A capability that already refuses on its own (connector entitlement, the
/// group boundary) asks the owner too, in enforce mode only.
pub fn existing_refusal(home: &Path, lane: &str, capability: &str, resource: &str, detail: &str) {
    if mode() != Mode::Enforce {
        counters::bump(&format!("{capability}.existing_refusal_uncarded"));
        return;
    }
    counters::bump(&format!("{capability}.refused"));
    let hint = grant_hint(capability, lane, detail);
    file_card(home, lane, capability, resource, &hint, grant_write(home, lane, capability, detail));
}

/// The worker crossed nothing this time: a later crossing may ask again once
/// an earlier card was answered.
pub fn clear_ask(home: &Path, lane: &str, capability: &str) {
    let f = asks_file(home);
    let mut all: Map<String, Value> = std::fs::read_to_string(&f)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    if all.remove(&ask_key(lane, capability)).is_some() {
        let _ = crate::integrations::secure_store::write(&f, Value::Object(all).to_string().as_bytes());
    }
}

/// The capabilities a scope write touches: the `connectors` layer, or an env
/// write naming an `AMUX_CAP_*` key (an object write's keys, or a text
/// replace's `KEY=` lines).
pub fn caps_written(key: &str, value: &Value) -> Vec<&'static str> {
    if key == "connectors" {
        return vec!["connectors"];
    }
    if key != "env" {
        return vec![];
    }
    let keys: Vec<String> = match value {
        Value::Object(m) => m.keys().cloned().collect(),
        Value::String(t) => t
            .lines()
            .filter_map(|l| l.trim().trim_start_matches("export ").split_once('=').map(|(k, _)| k.trim().to_string()))
            .collect(),
        _ => vec![],
    };
    let mut out = Vec::new();
    for (k, cap) in [(FOLDERS_KEY, "folders"), (PUBLISH_KEY, "publish"), (MESSAGE_KEY, "message")] {
        if keys.iter().any(|x| x == k) && !out.contains(&cap) {
            out.push(cap);
        }
    }
    out
}

/// Clear the open asks of `caps` for each of `workers`.
pub fn clear_asks(home: &Path, workers: &[String], caps: &[&str]) {
    for w in workers {
        for c in caps {
            clear_ask(home, w, c);
            // A grant answers the ask's card too, however it was written
            // (governance PR 12): the card and the env never disagree.
            let item = format!("{w}:{c}");
            if let Ok(h) = tokio::runtime::Handle::try_current() {
                h.spawn(super::trace::carry(async move {
                    super::approval_cards::resolved("capability", &item, "approved", "a scope grant", None).await;
                }));
            }
        }
    }
    counters::bump("asks_cleared_by_grant");
}

pub fn refusal(lane: &str, capability: &str, resource: &str, hint: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": format!("{lane} does not have the '{capability}' capability for {resource}"),
            "code": "capability_refused",
            "capability": capability,
            "resource": resource,
            "grant": hint,
            "next": "the owner has been asked on the board (Needs you); wait for the grant, do not route around it",
        })),
    )
        .into_response()
}

fn receipt(lane: &str, capability: &str, resource: &str, effect: &str, hint: &str) {
    let Some(store) = STORE.get().cloned() else { return };
    let id = format!("pol_{}", ulid::Ulid::new().to_string().to_lowercase());
    let actor = format!("worker:{lane}");
    let action = format!("capability_{capability}");
    let resource = resource.to_string();
    let effect = effect.to_string();
    let rationale = json!({"capability": capability, "grant": hint}).to_string();
    let trace = super::trace::current();
    let (trace_id, span_id) = (trace.as_ref().map(|t| t.trace_id.clone()), trace.map(|t| t.span_id));
    let fut = async move {
        let res = store
            .write_async(move |conn| {
                conn.execute(
                    "INSERT INTO _amux_policy_receipts
                     (id,actor,action,resource,trust,reversible,effect,rule_id,rationale,cost_microusd,created_at,trace_id,span_id)
                     VALUES(?1,?2,?3,?4,'untrusted',0,?5,'capabilities',?6,0,?7,?8,?9)",
                    rusqlite::params![id, actor, action, resource, effect, rationale,
                        chrono::Utc::now().to_rfc3339(), trace_id, span_id],
                )?;
                Ok(crate::db::WriteOutcome { applied: false, events: vec![] })
            })
            .await;
        if let Err(e) = res {
            counters::bump("receipt_failed");
            tracing::warn!(error = %e, measured = false, verdict = "capability_receipt_unwritten",
                "a capability decision was not recorded in the policy receipts");
        }
    };
    if let Ok(h) = tokio::runtime::Handle::try_current() {
        h.spawn(fut);
    }
}

/// The one worker-layer setting that grants `capability` to `lane`, merged
/// with what the worker already has, or `None` when granting is not a single
/// setting (connectors: the Connectors scope). The card's Approve writes it.
pub fn grant_write(home: &Path, lane: &str, capability: &str, detail: &str) -> Option<Value> {
    let merged = |key: &str| {
        let mut have: Vec<String> = layers(home, lane, key)
            .into_iter()
            .filter(|l| l.layer == format!("worker {lane}"))
            .flat_map(|l| split_list(&l.value))
            .collect();
        for d in split_list(detail) {
            if !have.contains(&d) {
                have.push(d);
            }
        }
        have.join(",")
    };
    match capability {
        "folders" if !detail.trim().is_empty() => Some(json!({ FOLDERS_KEY: merged(FOLDERS_KEY) })),
        "message" if !detail.trim().is_empty() => Some(json!({ MESSAGE_KEY: merged(MESSAGE_KEY) })),
        "publish" => Some(json!({ PUBLISH_KEY: if detail == "internal" { "internal" } else { "public" } })),
        _ => None,
    }
}

fn file_card(home: &Path, lane: &str, capability: &str, resource: &str, hint: &str, grant: Option<Value>) {
    let f = asks_file(home);
    let key = ask_key(lane, capability);
    {
        let mut all: Map<String, Value> = std::fs::read_to_string(&f)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        if all.contains_key(&key) {
            counters::bump(&format!("{capability}.card_deduped"));
            return;
        }
        all.insert(key.clone(), json!({"worker": lane, "capability": capability, "resource": resource,
            "asked_at": crate::config::now_f64(), "card": null}));
        if crate::integrations::secure_store::write(&f, Value::Object(all).to_string().as_bytes()).is_err() {
            counters::bump("card_failed");
            return;
        }
    }
    let Some(store) = STORE.get().cloned() else {
        counters::bump("card_failed");
        return;
    };
    let new = crate::db::board_store::NewIssue {
        title: format!("Let {lane} use '{capability}' for {}", resource.chars().take(100).collect::<String>()),
        desc: format!(
            "{lane} was refused outside its capabilities (governance: AMUX_CAPABILITIES=enforce).\n\n\
             Capability: {capability}\nResource: {resource}\n\nTo grant it: {hint}.\n\
             To keep the refusal, move this card to discarded. GET /api/workers/{lane}/capabilities shows what it has now."
        ),
        status: "needsyou".into(),
        session: None,
        item_type: "decision".into(),
        creator: "capabilities".into(),
        owner_type: "human".into(),
        due: None,
        due_time: None,
        reviewer: None,
        shepherd: None,
        gate: vec![],
        depends_on: vec![],
        tags: vec!["capability".into(), format!("capability:{capability}")],
        ask_type: Some("access".into()),
        next_action: None,
        acceptance_criteria: None,
        ask_question: Some(format!("Should {lane} get the '{capability}' capability for {}?",
            resource.chars().take(120).collect::<String>())),
        ask_unblocks: Some(format!("Granting it lets {lane} continue the work that was refused.")),
        ask_actor: Some(crate::api::turn_end::owner_name()),
        source: Some("capabilities".into()),
        requested_by: Some(lane.to_string()),
        callback_session: None,
        callback_prompt: None,
    };
    let home = home.to_path_buf();
    let capability = capability.to_string();
    let lane = lane.to_string();
    let (link_lane, link_cap) = (lane.clone(), capability.clone());
    let link_meta = json!({"grant": grant, "hint": hint, "resource": resource});
    let fut = async move {
        let made = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let made_w = made.clone();
        let now = crate::config::now_f64() as i64;
        let wrote = store
            .write_async(move |conn| {
                let row = crate::db::board_store::create_issue(conn, &new, now)?;
                // Governance PR 12: the card is this ask's approval card.
                super::approval_cards::link_in(conn, "capability", &format!("{link_lane}:{link_cap}"), &row.id,
                    &link_lane, &link_meta, now as f64)?;
                if let Ok(mut m) = made_w.lock() {
                    *m = Some(row.id.clone());
                }
                Ok(crate::db::WriteOutcome {
                    applied: true,
                    events: vec![crate::db::PendingEvent {
                        entity_type: amux_core::revision::EntityType::Task,
                        entity_id: row.id.clone(),
                        mutation: amux_core::revision::MutationKind::Created,
                        payload: Some(json!({"id": row.id, "source": "capabilities"})),
                    }],
                })
            })
            .await;
        let card = made.lock().ok().and_then(|m| m.clone());
        match (&wrote, &card) {
            (Ok(_), Some(id)) => {
                counters::bump(&format!("{capability}.card_filed"));
                let f = asks_file(&home);
                let mut all: Map<String, Value> = std::fs::read_to_string(&f)
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or_default();
                if let Some(Value::Object(row)) = all.get_mut(&ask_key(&lane, &capability)) {
                    row.insert("card".into(), json!(id));
                }
                let _ = crate::integrations::secure_store::write(&f, Value::Object(all).to_string().as_bytes());
                tracing::info!(worker = %lane, capability = %capability, card = %id, measured = true, n_considered = 1,
                    verdict = "capability_card_filed", "asked the owner to grant a capability");
            }
            _ => {
                counters::bump("card_failed");
                tracing::warn!(worker = %lane, capability = %capability, error = ?wrote.err(), measured = false,
                    verdict = "capability_card_unfiled", "a capability refusal could not file its card");
            }
        }
    };
    if let Ok(h) = tokio::runtime::Handle::try_current() {
        h.spawn(fut);
    }
}

// ── Who is asking ──────────────────────────────────────────────────────────

/// The worker a request acts as, when the worker itself is acting: its own
/// token, an unminted lane, or a bare claim (a name only narrows). A person or
/// an in-process job acting for a worker is not held to its folders.
pub fn acting_worker(home: &Path, headers: &HeaderMap) -> Option<String> {
    use super::connector_scope::{Principal, WorkerProof};
    match super::connector_scope::resolve(home, headers) {
        Principal::Worker { lane, proof } => match proof {
            WorkerProof::Human(_) | WorkerProof::Job(_) => None,
            _ => Some(lane),
        },
        _ => None,
    }
}

// ── The /api/fs gate ───────────────────────────────────────────────────────

fn qs_paths(query: &str) -> Vec<String> {
    super::fs::parse_qs(query)
        .into_iter()
        .filter(|(k, _)| matches!(k.as_str(), "path" | "dir"))
        .map(|(_, v)| v)
        .collect()
}

fn body_paths(body: &[u8]) -> Vec<String> {
    let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(body) else { return vec![] };
    ["path", "dest", "dest_dir", "src", "dir"]
        .iter()
        .filter_map(|k| m.get(*k).and_then(Value::as_str).map(String::from))
        .filter(|s| !s.trim().is_empty())
        .collect()
}

/// Middleware on `/api/fs/*`: a worker touching another worker's folder.
pub async fn fs_gate(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let home = crate::config::amux_home();
    let Some(lane) = acting_worker(&home, req.headers()) else {
        return next.run(req).await;
    };
    let query = req.uri().query().unwrap_or("").to_string();
    let mut paths = qs_paths(&query);
    let is_json = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("json"));
    let req = if is_json {
        let (parts, body) = req.into_parts();
        let bytes = match axum::body::to_bytes(body, 8 << 20).await {
            Ok(b) => b,
            Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({"error": "body too large"}))).into_response(),
        };
        paths.extend(body_paths(&bytes));
        axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes))
    } else {
        req
    };
    let hd = user_home();
    let mut crossed = false;
    for raw in &paths {
        let p = expand(&hd, raw);
        if let FolderDecision::Boundary { owner, owner_dir } = folder_decision(&home, &lane, &p) {
            crossed = true;
            let resource = format!("folder {} ({owner}'s)", owner_dir.display());
            let detail = owner_dir.to_string_lossy().into_owned();
            if let Some(resp) = cross(&home, &lane, "folders", &resource, &detail) {
                return resp;
            }
        }
    }
    if !crossed && !paths.is_empty() {
        counters::bump("folders.allowed");
    }
    next.run(req).await
}

// ── Route and diagnostic ───────────────────────────────────────────────────

/// `GET /api/workers/{name}/capabilities`: a person sees any worker; a worker
/// sees only itself.
pub async fn view(
    axum::extract::State(state): axum::extract::State<super::AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    headers: HeaderMap,
) -> Response {
    let home = crate::config::amux_home();
    let human = super::human_proof::proven(&headers).is_some();
    let me = acting_worker(&home, &headers);
    if !human && me.as_deref() != Some(name.as_str()) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "a worker may read only its own capabilities", "code": "capability_view_refused"})),
        )
            .into_response();
    }
    if !home.join("sessions").join(format!("{name}.env")).exists() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": format!("no worker named {name}")}))).into_response();
    }
    let conn = state.store.read().ok();
    Json(for_worker(&home, conn.as_deref(), &name)).into_response()
}

/// The A5 diagnostic, served in `GET /api/contract/counters` as `capabilities`.
pub fn report() -> Value {
    let c = counters::snapshot();
    let n: u64 = c.values().sum();
    json!({
        "mode": mode().as_str(),
        "measured": true,
        "n_considered": n,
        "counts": c,
        "store_installed": STORE.get().is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(home: &Path, name: &str, dir: &Path, tags: &str, extra: &str) {
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            home.join("sessions").join(format!("{name}.env")),
            format!("CC_DIR={}\nCC_TAGS={tags}\n{extra}", dir.display()),
        )
        .unwrap();
    }

    fn fleet() -> (tempfile::TempDir, tempfile::TempDir) {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        worker(home.path(), "wexus", &r.join("customers/wexus"), "customers", "");
        worker(home.path(), "rothco", &r.join("customers/rothco"), "customers", "");
        worker(home.path(), "helper", r, "amux", "");
        (home, root)
    }

    #[test]
    fn a_worker_keeps_its_own_folder_and_shared_ground() {
        let (home, root) = fleet();
        let r = root.path();
        assert_eq!(
            folder_decision(home.path(), "wexus", &r.join("customers/wexus/data/x.csv")),
            FolderDecision::Allowed("the worker's own folder".into())
        );
        assert!(matches!(folder_decision(home.path(), "wexus", &r.join("notes.md")), FolderDecision::Allowed(_)),
            "the helper's root folder contains wexus's own: shared ground");
        assert!(matches!(folder_decision(home.path(), "wexus", Path::new("/usr/share")), FolderDecision::Allowed(_)));
    }

    #[test]
    fn another_customers_folder_is_a_boundary_even_through_dot_dot() {
        let (home, root) = fleet();
        let r = root.path();
        let via = r.join("customers/wexus/../rothco/lead.md");
        match folder_decision(home.path(), "wexus", &via) {
            FolderDecision::Boundary { owner, .. } => assert_eq!(owner, "rothco"),
            other => panic!("expected the rothco boundary, got {other:?}"),
        }
        // A worker whose folder holds everything still meets the deeper owner.
        assert!(matches!(folder_decision(home.path(), "helper", &r.join("customers/rothco")),
            FolderDecision::Boundary { .. }));
    }

    #[test]
    fn a_grant_on_any_layer_opens_the_folder() {
        let (home, root) = fleet();
        let r = root.path();
        std::fs::create_dir_all(home.path().join("env")).unwrap();
        std::fs::write(home.path().join("env/customers.env"),
            format!("{FOLDERS_KEY}={}\n", r.join("customers/rothco").display())).unwrap();
        match folder_decision(home.path(), "wexus", &r.join("customers/rothco/lead.md")) {
            FolderDecision::Allowed(why) => assert!(why.contains("group customers"), "{why}"),
            other => panic!("expected a grant, got {other:?}"),
        }
    }

    #[test]
    fn enforce_refuses_report_allows_off_skips() {
        let home = tempfile::tempdir().unwrap();
        assert!(cross_with(home.path(), "w", "folders", "x", "/x", Mode::Report).is_none());
        assert!(cross_with(home.path(), "w", "folders", "x", "/x", Mode::Off).is_none());
        let r = cross_with(home.path(), "w", "folders", "x", "/x", Mode::Enforce).expect("enforce refuses");
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn one_ask_per_worker_and_capability_until_cleared() {
        let home = tempfile::tempdir().unwrap();
        file_card(home.path(), "w", "folders", "a", "h", None);
        file_card(home.path(), "w", "folders", "b", "h", None);
        let all: Map<String, Value> =
            serde_json::from_str(&std::fs::read_to_string(asks_file(home.path())).unwrap()).unwrap();
        assert_eq!(all.len(), 1, "a second crossing of the same capability files no second ask");
        assert_eq!(all[&ask_key("w", "folders")]["resource"], "a",
            "the first ask stands: a second crossing neither re-asks nor rewrites it");
        clear_ask(home.path(), "w", "folders");
        let all: Map<String, Value> =
            serde_json::from_str(&std::fs::read_to_string(asks_file(home.path())).unwrap()).unwrap();
        assert!(all.is_empty());
    }

    #[test]
    fn an_owner_grant_through_the_scope_api_reopens_the_ask() {
        let home = tempfile::tempdir().unwrap();
        file_card(home.path(), "w", "folders", "a", "h", None);
        file_card(home.path(), "w", "publish", "a", "h", None);
        assert_eq!(caps_written("env", &json!({FOLDERS_KEY: "/x"})), vec!["folders"]);
        assert_eq!(caps_written("env", &json!("CC_MODEL=x\nAMUX_CAP_PUBLISH=none\n")), vec!["publish"]);
        assert_eq!(caps_written("connectors", &json!({})), vec!["connectors"]);
        assert!(caps_written("env", &json!({"CC_MODEL": "x"})).is_empty());
        clear_asks(home.path(), &["w".to_string()], &caps_written("env", &json!({FOLDERS_KEY: "/x"})));
        let all: Map<String, Value> =
            serde_json::from_str(&std::fs::read_to_string(asks_file(home.path())).unwrap()).unwrap();
        assert_eq!(all.len(), 1, "the folders ask is cleared, the publish ask stays");
        assert!(all.contains_key(&ask_key("w", "publish")));
    }

    #[test]
    fn publish_and_message_levels() {
        let (home, _root) = fleet();
        assert_eq!(publish_level(home.path(), "wexus").0, "public");
        assert!(publish_allows("internal", "internal") && !publish_allows("internal", "app"));
        assert!(!publish_allows("none", "internal"));
        let env = home.path().join("sessions/wexus.env");
        let mut t = std::fs::read_to_string(&env).unwrap();
        t.push_str(&format!("{PUBLISH_KEY}=internal\n{MESSAGE_KEY}=group:ops,helper\n"));
        std::fs::write(&env, t).unwrap();
        assert_eq!(publish_level(home.path(), "wexus"), ("internal".into(), "worker wexus".into()));
        let ops: BTreeSet<String> = ["ops".to_string()].into();
        assert!(message_granted(home.path(), "wexus", "x", &ops).is_some());
        assert!(message_granted(home.path(), "wexus", "helper", &BTreeSet::new()).is_some());
        assert!(message_granted(home.path(), "wexus", "rothco", &BTreeSet::new()).is_none());
    }

    #[test]
    fn capability_keys_are_owner_only() {
        assert!(super::super::scope::is_owner_only_env_key_pub("AMUX_CAP_FOLDERS"));
        assert!(super::super::scope::is_owner_only_env_key_pub("AMUX_CAP_MESSAGE"));
    }
}
