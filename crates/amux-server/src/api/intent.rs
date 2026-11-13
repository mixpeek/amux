//! `POST /api/intent/plan`: one routing decision for a plain request
//! (docs/design/intent-first.md, phase 1).
//!
//! A person says what they want; this decides how it gets done, using the
//! workers that already exist and creating one only when none fits. It merges
//! the two routers amux had: `outcome_plan` (always created a worker) and
//! `orchestrate/plan` (routed a spoken command, never created). Four actions:
//!
//! - `continue`: a follow-up inside an outcome goes to that outcome's worker.
//!   Computed, no model call (ethos rule 2).
//! - `answer`: a quick question the helper model answers itself.
//! - `send`: an existing worker owns this work.
//! - `create`: nothing fits; a new worker is named, placed and typed.
//!
//! The helper model makes the judgment. Every worker, directory and group it
//! names is validated against what exists, and every correction it needed is
//! reported beside the decision rather than swallowed (ethos rule 4). An
//! isolated worker is never a routing target (A9). The response carries
//! `measured`, `n_considered`, `why`, the candidates considered and `sources`,
//! the messages the decision came from.

use super::orchestrate::RosterEntry;
use super::outcome_plan::{self, Place};
use super::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/intent/plan", post(plan))
}

/// The worker types a router may create. Chat runs headless turns with no
/// terminal, which is what an everyday request wants; coding gets a terminal.
pub(crate) const CREATE_TYPES: &[&str] = &["chat", "coding"];
/// Model aliases a router may pick for a created worker. Anything else is
/// dropped to "" (the worker's default).
pub(crate) const CREATE_MODELS: &[&str] = &["haiku", "sonnet", "opus"];
/// Reasoning effort a router may pick per outcome (phase 5: self-configuration).
pub(crate) const CREATE_EFFORTS: &[&str] = &["low", "medium", "high"];

/// What the router decided, after validation.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Decision {
    pub action: String,
    pub worker: String,
    pub title: String,
    pub message: String,
    pub answer: String,
    pub why: String,
    pub candidates: Vec<Value>,
    pub create: Option<Value>,
}

/// Each correction the validator made, by reason. A router that silently
/// rewrites half a decision looks the same as one that got it right.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Corrections {
    pub unknown_worker: Option<String>,
    pub isolated_worker: Option<String>,
    pub empty_answer: bool,
    pub invented_dir: Option<String>,
    pub dropped_groups: Vec<String>,
    pub unknown_type: Option<String>,
    pub unknown_model: Option<String>,
    pub unknown_action: Option<String>,
}

impl Corrections {
    pub fn to_json(&self) -> Value {
        json!({
            "unknown_worker": self.unknown_worker,
            "isolated_worker": self.isolated_worker,
            "empty_answer": self.empty_answer,
            "invented_dir": self.invented_dir,
            "dropped_groups": self.dropped_groups,
            "unknown_type": self.unknown_type,
            "unknown_model": self.unknown_model,
            "unknown_action": self.unknown_action,
        })
    }
}

/// A short title for an outcome when the model gives none: the request's
/// first sentence, at most 60 characters, on a word boundary.
pub(crate) fn fallback_title(text: &str) -> String {
    let first = text.split(['\n', '.', '?', '!']).map(str::trim).find(|s| !s.is_empty()).unwrap_or("New request");
    if first.chars().count() <= 60 {
        return first.to_string();
    }
    let mut out = String::new();
    for w in first.split_whitespace() {
        if out.chars().count() + w.chars().count() + 1 > 57 {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    format!("{out}...")
}

pub(crate) fn build_prompt(
    text: &str,
    roster: &[RosterEntry],
    places: &BTreeMap<String, Place>,
    groups: &BTreeSet<String>,
    hints: &str,
) -> String {
    let mut p = String::from(
        "You route a person's request inside amux, where AI workers do work for them. \
         Decide ONE action:\n\
         - \"answer\": a quick question you can answer fully yourself, right now, from general \
         knowledge (no reading their email, calendar, files or accounts, no follow-up work). Put the \
         whole answer in \"answer\".\n\
         - \"send\": an existing worker below already owns this kind of work (judge by its \
         description, groups and directory, not by keywords). Name it exactly.\n\
         - \"create\": no worker fits. Describe a new one in \"create\".\n\
         Prefer send when a worker clearly fits; prefer create over sending to a poor fit.\n\
         For create: name the AREA the helper will keep handling, not this one request, so later requests \
         reuse it (family-admin, meals, money, school, work-crm): 1 to 3 lowercase words joined by '-', at \
         most 24 characters, never filler such as need, worker, want. desc says that area in one plain \
         sentence (\"Handles the family's school forms, camp and calendar\"). dir is a directory from the list where this \
         work's files live, copied exactly, or \"\" when the work is not about files (email, calendar, \
         errands, planning). worker_type is \"chat\" for everyday tasks, research and questions about \
         email or calendar, \"coding\" when it changes code or files in a repository. model is haiku, \
         sonnet or opus (sonnet unless the work is unusually hard or trivial). effort is low, medium or \
         high: how hard it should think (low for a quick lookup, high for planning or reconciling).\n\
         Always give: title (at most 7 plain words naming the outcome, no worker names), why (one \
         short sentence a non-technical person understands, about the request, never the words worker, \
         agent, session, model, terminal or AI; say \"helper\" if you must), and candidates (up to 3 workers you \
         considered, each with a short why).\n\
         Reply with ONLY a JSON object:\n\
         {\"action\":\"answer|send|create\",\"worker\":\"<exact name or empty>\",\"answer\":\"...\",\
         \"title\":\"...\",\"why\":\"...\",\"candidates\":[{\"worker\":\"...\",\"why\":\"...\"}],\
         \"create\":{\"name\":\"...\",\"dir\":\"...\",\"groups\":[...],\"desc\":\"...\",\
         \"worker_type\":\"chat|coding\",\"model\":\"...\",\"effort\":\"low|medium|high\"}}\n\n",
    );
    if !hints.trim().is_empty() {
        p.push_str("PAST CORRECTIONS BY THE OWNER (route like these):\n");
        p.push_str(hints.trim());
        p.push_str("\n\n");
    }
    p.push_str(&format!("REQUEST:\n{}\n\nWORKERS:\n", text.trim()));
    for w in roster.iter().filter(|w| !w.isolated) {
        let d = w.desc.trim();
        p.push_str(&format!(
            "- {} [{}; {}] {}{}\n",
            w.name,
            w.worker_type,
            w.groups.join(", "),
            if d.is_empty() { "(no description)" } else { d },
            if w.dir.is_empty() { String::new() } else { format!(" (dir {})", w.dir) },
        ));
    }
    p.push_str("\nDIRECTORIES:\n");
    for dir in places.keys() {
        p.push_str(&format!("- {dir}\n"));
    }
    p.push_str(&format!("\nGROUPS: {}\n", groups.iter().cloned().collect::<Vec<_>>().join(", ")));
    p
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// Turn the model's JSON into a decision every name in which exists.
///
/// The rules, each one tested by name below:
/// - a `send` to a worker that does not exist becomes a `create` (never a send
///   into nothing);
/// - a `send` to an isolated worker becomes a `create` (A9: no routing);
/// - an `answer` with no answer text becomes a `create`;
/// - a `create` keeps only an existing directory, existing groups, a known
///   worker type and a known model.
pub(crate) fn validate(
    raw: &Value,
    text: &str,
    roster: &[RosterEntry],
    places: &BTreeMap<String, Place>,
    groups: &BTreeSet<String>,
    taken: &BTreeSet<String>,
) -> (Decision, Corrections) {
    let mut c = Corrections::default();
    let mut d = Decision {
        title: s(raw, "title"),
        why: s(raw, "why"),
        ..Default::default()
    };
    if d.title.is_empty() {
        d.title = fallback_title(text);
    }
    d.title = d.title.chars().take(80).collect();
    let by_name: BTreeMap<&str, &RosterEntry> = roster.iter().map(|w| (w.name.as_str(), w)).collect();
    d.candidates = raw
        .get("candidates")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|x| by_name.get(s(x, "worker").as_str()).is_some_and(|w| !w.isolated))
                .take(3)
                .map(|x| json!({"worker": s(x, "worker"), "why": s(x, "why")}))
                .collect()
        })
        .unwrap_or_default();
    let mut action = s(raw, "action").to_lowercase();
    if !["answer", "send", "create"].contains(&action.as_str()) {
        c.unknown_action = Some(action.clone());
        action = "create".into();
    }
    if action == "answer" {
        d.answer = s(raw, "answer");
        if d.answer.is_empty() {
            c.empty_answer = true;
            action = "create".into();
        }
    }
    if action == "send" {
        let w = s(raw, "worker");
        match by_name.get(w.as_str()) {
            Some(e) if e.isolated => {
                c.isolated_worker = Some(w);
                action = "create".into();
            }
            Some(_) => d.worker = w,
            None => {
                c.unknown_worker = Some(w);
                action = "create".into();
            }
        }
    }
    if action == "create" {
        let spec = raw.get("create").cloned().unwrap_or(json!({}));
        let name = spec
            .get("name")
            .and_then(Value::as_str)
            .and_then(outcome_plan::clean_name)
            .unwrap_or_else(|| outcome_plan::fallback_name(text));
        let name = outcome_plan::unique(&name, taken);
        let dir = s(&spec, "dir").trim_end_matches('/').to_string();
        let dir = if dir.is_empty() || places.contains_key(&dir) {
            dir
        } else {
            c.invented_dir = Some(dir);
            String::new()
        };
        let mut kept = vec![];
        for g in spec.get("groups").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if groups.contains(g) && kept.len() < 2 {
                kept.push(g.to_string());
            } else {
                c.dropped_groups.push(g.to_string());
            }
        }
        let mut wtype = s(&spec, "worker_type").to_lowercase();
        if !CREATE_TYPES.contains(&wtype.as_str()) {
            if !wtype.is_empty() {
                c.unknown_type = Some(wtype.clone());
            }
            wtype = if dir.is_empty() { "chat".into() } else { "coding".into() };
        }
        let mut model = s(&spec, "model").to_lowercase();
        if !model.is_empty() && !CREATE_MODELS.contains(&model.as_str()) {
            c.unknown_model = Some(model.clone());
            model = String::new();
        }
        let mut effort = s(&spec, "effort").to_lowercase();
        if !CREATE_EFFORTS.contains(&effort.as_str()) {
            effort = String::new();
        }
        let desc: String = s(&spec, "desc").chars().take(140).collect();
        d.worker = name.clone();
        d.create = Some(json!({
            "name": name, "dir": dir, "groups": kept, "desc": desc,
            "worker_type": wtype, "model": model, "effort": effort,
        }));
    }
    d.action = action;
    d.message = text.trim().to_string();
    (d, c)
}

/// The decision when no model answered: a new chat worker named from the
/// request's own words. Said plainly through `fallback_reason`.
pub(crate) fn fallback_decision(text: &str, taken: &BTreeSet<String>) -> Decision {
    let name = outcome_plan::unique(&outcome_plan::fallback_name(text), taken);
    Decision {
        action: "create".into(),
        worker: name.clone(),
        title: fallback_title(text),
        message: text.trim().to_string(),
        why: "Started a new helper for this, since the router could not decide.".into(),
        create: Some(json!({"name": name, "dir": "", "groups": [], "desc": "",
            "worker_type": "chat", "model": ""})),
        ..Default::default()
    }
}

pub(crate) fn decision_json(d: &Decision) -> Value {
    json!({
        "action": d.action,
        "worker": d.worker,
        "title": d.title,
        "message": d.message,
        "answer": d.answer,
        "why": d.why,
        "candidates": d.candidates,
        "create": d.create,
    })
}

/// Plan one request. `outcome` (with its worker) makes it a follow-up.
pub(crate) async fn plan_request(text: &str, outcome: Option<(&str, &str)>, hints: &str) -> Value {
    let started = std::time::Instant::now();
    let home = crate::config::amux_home();
    let roster = super::orchestrate::roster_entries(&home);
    let routable = roster.iter().filter(|w| !w.isolated).count();
    if let Some((oid, worker)) = outcome.filter(|(_, w)| !w.is_empty()) {
        if roster.iter().any(|w| w.name == worker && !w.isolated) {
            let d = Decision {
                action: "continue".into(),
                worker: worker.to_string(),
                title: String::new(),
                message: text.trim().to_string(),
                why: "A follow-up in this conversation goes to the helper already working on it.".into(),
                ..Default::default()
            };
            tracing::info!(outcome = %oid, worker = %worker, measured = true, n_considered = 1,
                verdict = "intent_continue", "intent plan: a follow-up went to its outcome's worker with no routing call");
            let mut v = decision_json(&d);
            v["measured"] = json!(true);
            v["n_considered"] = json!(1);
            v["via"] = json!("outcome");
            v["outcome"] = json!(oid);
            v["took_ms"] = json!(started.elapsed().as_millis() as u64);
            return v;
        }
        tracing::warn!(outcome = %oid, worker = %worker, measured = true, n_considered = roster.len(),
            verdict = "intent_continue_worker_gone",
            "intent plan: a follow-up's outcome names a worker that no longer exists or is isolated; routing it fresh");
    }
    let places = outcome_plan::candidate_dirs(&home, &dirs_home());
    let groups: BTreeSet<String> = roster.iter().flat_map(|w| w.groups.iter().cloned()).collect();
    let taken = outcome_plan::taken_names(&home);
    let prompt = build_prompt(text, &roster, &places, &groups, hints);
    let model_started = std::time::Instant::now();
    let (via, decision, corrections, fallback_reason) = match super::lookup::helper_answer(&prompt).await {
        Ok((via, answer)) => match parse_object(&answer) {
            Some(raw) => {
                let (d, c) = validate(&raw, text, &roster, &places, &groups, &taken);
                (via, d, c, None)
            }
            None => (
                via,
                fallback_decision(text, &taken),
                Corrections::default(),
                Some(format!("the router model's answer was not JSON: {}", answer.chars().take(200).collect::<String>())),
            ),
        },
        Err((_, e)) => ("none".to_string(), fallback_decision(text, &taken), Corrections::default(), Some(e)),
    };
    let model_ms = model_started.elapsed().as_millis() as u64;
    if let Some(why) = &fallback_reason {
        tracing::warn!(error = %why, measured = true, n_considered = routable, verdict = "intent_plan_fallback",
            "intent plan: the router model did not decide; a new chat worker is proposed from the request's words");
    }
    tracing::info!(action = %decision.action, worker = %decision.worker, via = %via, model_ms,
        measured = true, n_considered = routable, verdict = "intent_planned",
        "intent plan: routed a request");
    let mut v = decision_json(&decision);
    v["measured"] = json!(true);
    v["n_considered"] = json!(routable);
    v["via"] = json!(via);
    v["model_ms"] = json!(model_ms);
    v["took_ms"] = json!(started.elapsed().as_millis() as u64);
    v["corrections"] = corrections.to_json();
    v["fallback_reason"] = json!(fallback_reason);
    v
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var("HOME").map(std::path::PathBuf::from).unwrap_or_else(|_| std::path::PathBuf::from("/"))
}

/// The first JSON object in a model reply that may wrap it in prose or a fence.
pub(crate) fn parse_object(raw: &str) -> Option<Value> {
    let (a, b) = (raw.find('{')?, raw.rfind('}')?);
    serde_json::from_str::<Value>(raw.get(a..=b)?).ok().filter(Value::is_object)
}

async fn plan(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let text = body["text"].as_str().unwrap_or("").trim().to_string();
    if text.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "say what you want first"}))).into_response();
    }
    let outcome = body["outcome"].as_str().unwrap_or("");
    let worker = body["worker"].as_str().unwrap_or("");
    let hints = super::intent_store::override_hints(&state);
    let mut v = plan_request(&text, (!outcome.is_empty()).then_some((outcome, worker)), &hints).await;
    v["sources"] = json!([{"kind": "request", "text": text}]);
    Json(v).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(name: &str, desc: &str, isolated: bool) -> RosterEntry {
        RosterEntry { name: name.into(), desc: desc.into(), worker_type: "chat".into(), isolated, ..Default::default() }
    }

    fn fixture() -> (Vec<RosterEntry>, BTreeMap<String, Place>, BTreeSet<String>, BTreeSet<String>) {
        let roster = vec![
            w("family", "Handles the family's school, camp and calendar errands", false),
            w("gtm-crm", "Tracks outreach in the CRM", false),
            w("private-notes", "Owner's private scratch", true),
        ];
        let mut places = BTreeMap::new();
        places.insert("/home/u/site".to_string(), Place::default());
        let groups: BTreeSet<String> = ["home".to_string(), "gtm".to_string()].into();
        let taken: BTreeSet<String> = roster.iter().map(|w| w.name.clone()).collect();
        (roster, places, groups, taken)
    }

    #[test]
    fn a_send_to_an_existing_worker_is_kept() {
        let (r, p, g, t) = fixture();
        let raw = json!({"action":"send","worker":"family","title":"Camp waitlist call","why":"Owns camp errands",
            "candidates":[{"worker":"family","why":"camp"},{"worker":"ghost","why":"x"},{"worker":"private-notes","why":"y"}]});
        let (d, c) = validate(&raw, "Remind me to call the camp director", &r, &p, &g, &t);
        assert_eq!(d.action, "send");
        assert_eq!(d.worker, "family");
        assert_eq!(c, Corrections::default());
        assert_eq!(d.candidates.len(), 1, "invented and isolated candidates are dropped");
    }

    #[test]
    fn a_send_to_an_unknown_worker_becomes_a_create() {
        let (r, p, g, t) = fixture();
        let (d, c) = validate(&json!({"action":"send","worker":"ghost"}), "Plan dinners this week", &r, &p, &g, &t);
        assert_eq!(d.action, "create");
        assert_eq!(c.unknown_worker.as_deref(), Some("ghost"));
        assert!(d.create.is_some());
    }

    #[test]
    fn a_send_to_an_isolated_worker_becomes_a_create() {
        let (r, p, g, t) = fixture();
        let (d, c) = validate(&json!({"action":"send","worker":"private-notes"}), "Note this", &r, &p, &g, &t);
        assert_eq!(d.action, "create");
        assert_eq!(c.isolated_worker.as_deref(), Some("private-notes"));
        assert_ne!(d.worker, "private-notes");
    }

    #[test]
    fn an_answer_with_no_text_becomes_a_create() {
        let (r, p, g, t) = fixture();
        let (d, c) = validate(&json!({"action":"answer","answer":"  "}), "What is 2+2", &r, &p, &g, &t);
        assert!(c.empty_answer);
        assert_eq!(d.action, "create");
        let (d, _) = validate(&json!({"action":"answer","answer":"4"}), "What is 2+2", &r, &p, &g, &t);
        assert_eq!((d.action.as_str(), d.answer.as_str()), ("answer", "4"));
        assert!(d.create.is_none());
    }

    #[test]
    fn a_create_keeps_only_what_exists() {
        let (r, p, g, t) = fixture();
        let raw = json!({"action":"create","create":{"name":"Family","dir":"/home/u/invented",
            "groups":["home","made-up"],"worker_type":"robot","model":"gpt-9"}});
        let (d, c) = validate(&raw, "Plan dinners", &r, &p, &g, &t);
        let spec = d.create.unwrap();
        assert_eq!(spec["name"], "family-2", "a taken name is made unique");
        assert_eq!(spec["dir"], "");
        assert_eq!(c.invented_dir.as_deref(), Some("/home/u/invented"));
        assert_eq!(spec["groups"], json!(["home"]));
        assert_eq!(c.dropped_groups, vec!["made-up".to_string()]);
        assert_eq!(spec["worker_type"], "chat", "an unknown type with no directory is chat");
        assert_eq!(c.unknown_type.as_deref(), Some("robot"));
        assert_eq!(spec["model"], "");
        assert_eq!(c.unknown_model.as_deref(), Some("gpt-9"));
        let (d, _) = validate(&json!({"action":"create","create":{"name":"site","dir":"/home/u/site/"}}), "fix the site", &r, &p, &g, &t);
        assert_eq!(d.create.unwrap()["worker_type"], "coding", "work in a directory defaults to coding");
    }

    #[test]
    fn an_unknown_action_becomes_a_create() {
        let (r, p, g, t) = fixture();
        let (d, c) = validate(&json!({"action":"delete"}), "Plan dinners", &r, &p, &g, &t);
        assert_eq!(d.action, "create");
        assert_eq!(c.unknown_action.as_deref(), Some("delete"));
    }

    #[test]
    fn the_prompt_never_offers_an_isolated_worker() {
        let (r, p, g, _) = fixture();
        let prompt = build_prompt("x", &r, &p, &g, "");
        assert!(prompt.contains("- family [chat"));
        assert!(!prompt.contains("private-notes"));
    }

    #[test]
    fn titles_fall_back_to_the_first_sentence() {
        assert_eq!(fallback_title("Plan dinners this week. Thanks"), "Plan dinners this week");
        let long = "Go through my inbox and find every single thing that relates to my kids school this term please";
        let t = fallback_title(long);
        assert!(t.ends_with("...") && t.chars().count() <= 60, "{t}");
    }

    #[test]
    fn json_is_found_inside_prose() {
        assert_eq!(parse_object("Sure:\n```json\n{\"action\":\"send\"}\n```").unwrap()["action"], "send");
        assert!(parse_object("no").is_none());
    }
}
