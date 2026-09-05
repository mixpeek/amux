//! POST /api/orchestrate/plan — the voice fleet-orchestrator's brain (AMUX-3074).
//!
//! A human speaks one command; it is transcribed by the existing /api/dictate,
//! and this endpoint turns the transcript into a ROUTING PLAN: which workers
//! should receive which messages. This is a pure composition of primitives —
//! dictation (transcript) + the fast helper model (the routing JUDGMENT) +
//! workers (the roster: names, groups, descriptions) + messages (the client
//! sends each plan entry through the existing send path). No new subsystem.
//!
//! Why the model and not keyword matching (ethos rule 2): choosing which worker
//! owns which slice of a spoken, run-on, homophone-ridden command IS judgment,
//! and it gets better as the helper model does. The endpoint only PLANS — it
//! never sends — so a mis-route is caught in the review step before any message
//! goes out, and every routed name is validated against the live roster so the
//! model can never invent a recipient.

use super::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use std::path::Path;

pub fn routes() -> Router<AppState> {
    Router::new().route("/plan", post(plan))
}

/// (name, groups, description) for every non-blocked, non-archived worker — read
/// from the same `~/.amux/sessions/*.env` files the fleet list uses, so the
/// router reasons over exactly the workers a human sees.
fn fleet_roster(home: &Path) -> Vec<(String, Vec<String>, String)> {
    let blocked = crate::api::groups::blocked_names(home);
    let Ok(entries) = std::fs::read_dir(home.join("sessions")) else {
        return vec![];
    };
    let mut rows: Vec<(String, Vec<String>, String)> = vec![];
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("env") {
            continue;
        }
        let Some(name) = p.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        if blocked.contains(&name) {
            continue;
        }
        let env = crate::config::parse_env_file(&p);
        if env.get("CC_ARCHIVED").map(|v| v == "1").unwrap_or(false) {
            continue;
        }
        let groups: Vec<String> = env
            .get("CC_TAGS")
            .map(String::as_str)
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(String::from)
            .collect();
        let desc = env.get("CC_DESC").cloned().unwrap_or_default();
        rows.push((name, groups, desc));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

fn build_prompt(roster: &[(String, Vec<String>, String)], transcript: &str) -> String {
    let mut roster_txt = String::new();
    for (name, groups, desc) in roster {
        let d = desc.trim();
        roster_txt.push_str(&format!(
            "- {name} [{}] — {}\n",
            groups.join(", "),
            if d.is_empty() { "(no description)" } else { d }
        ));
    }
    format!(
        "You are the amux fleet router. A human spoke a command and it was transcribed \
         roughly — expect dictation errors, run-ons, and homophones. Decide which ACTIONS \
         should be proposed against which WORKERS.\n\n\
         Rules:\n\
         - A command may fan out to several workers or target just one.\n\
         - Rewrite each worker's message as a clear, direct instruction to THAT worker \
           (imperative, second person), carrying ONLY the part of the command relevant to \
           it. Fix obvious transcription errors.\n\
         - Route by what each worker DOES (its description and groups), not by keyword \
           matching. Use EXACT worker names from the roster.\n\
         - If the command names a group, target the workers in that group.\n\
         - Prefer `send`. Use `board` when the human is FILING work rather than \
           instructing (\"make a card for…\", \"add a todo…\", \"note on AMUX-123 that…\"). \
           Use `verb` only when the human is plainly asking for a lifecycle change \
           (\"start\", \"stop\", \"wake\").\n\
         - {} are the ONLY verbs available. Anything else — delete, archive, reset, \
           clear, rename, deploy — is NOT yours to propose. Do NOT silently skip it: \
           emit {{\"action\":\"refused\",\"worker\":\"<name>\",\"verb\":\"<what they asked for>\"}} \
           so the human is told you understood and declined, and can do it by hand.\n\
         - Return an empty array ONLY when the command asked for nothing at all.\n\n\
         WORKERS:\n{roster_txt}\n\
         SPOKEN COMMAND:\n\"{transcript}\"\n\n\
         Return ONLY a JSON array, no prose and no markdown fence. Each entry is ONE of:\n\
         [{{\"action\":\"send\",\"worker\":\"<exact name>\",\"message\":\"<instruction>\",\"why\":\"<short reason>\"}},\n\
          {{\"action\":\"board\",\"worker\":\"<exact name>\",\"title\":\"<card title>\",\"why\":\"…\"}},\n\
          {{\"action\":\"board\",\"worker\":\"<exact name>\",\"card\":\"<EXISTING-CARD-ID>\",\"note\":\"<progress note>\",\"why\":\"…\"}},\n\
          {{\"action\":\"verb\",\"worker\":\"<exact name>\",\"verb\":\"<one of the above>\",\"why\":\"…\"}}]",
        VOICE_VERBS.join(" / ")
    )
}

/// The verbs a SPOKEN command may propose.
///
/// Deliberately three, and the omissions are the point. This endpoint's safety
/// property is that it plans and never executes, but the review step is a
/// checkbox list and a human skims it — so the allow-list has to hold on its
/// own, against an input that is a transcription full of homophones.
///
/// These three are what a person actually says out loud about a lane, and their
/// worst case is a lane restarted. `delete`, `archive`, `reset`, `clear`,
/// `rename` and `deploy` are all reachable verbs and none of them belong on the
/// far side of a misheard word: `deploy` pushes to origin, `clear` drops a
/// lane's context, `delete` is unrecoverable.
///
/// A verb outside this list is REFUSED and reported as refused, which is a
/// different fact from a verb that does not exist — the model understood and
/// amux declined, and a human reading the plan should be told which.
pub(crate) const VOICE_VERBS: &[&str] = &["start", "stop", "wake"];

/// Pull the first balanced JSON array out of a model reply that may wrap it in
/// prose or a ```json fence. String-aware so a `]` inside a message value does
/// not close the array early.
fn extract_json_array(s: &str) -> Option<Value> {
    let trimmed = s.trim();
    if let Ok(v @ Value::Array(_)) = serde_json::from_str::<Value>(trimmed) {
        return Some(v);
    }
    let start = s.find('[')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, c) in s.char_indices().skip(start) {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    let cand = &s[start..=i];
                    return serde_json::from_str::<Value>(cand).ok().filter(Value::is_array);
                }
            }
            _ => {}
        }
    }
    None
}

pub async fn plan(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let started = std::time::Instant::now();
    let transcript = body
        .get("transcript")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if transcript.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "transcript is required" })))
            .into_response();
    }
    let home = crate::api::groups::amux_home();
    let roster = fleet_roster(&home);
    if roster.is_empty() {
        return Json(json!({ "plan": [], "transcript": transcript, "note": "no workers available" }))
            .into_response();
    }
    let prompt = build_prompt(&roster, &transcript);
    // TIME THE MODEL SEPARATELY FROM THE REQUEST (AMUX-3818). This endpoint's
    // work IS an LLM round-trip: measured p50 7.1s against a flat 10s latency
    // threshold, so ordinary variance files a defect. The answer is not to
    // exempt the route — amux's own time around the call must stay measured —
    // but to know which of the two was slow, and say so on the way out.
    let model_started = std::time::Instant::now();
    let (via, answer) = match crate::api::lookup::helper_answer(&prompt).await {
        Ok(x) => x,
        Err((code, msg)) => {
            return (code, Json(json!({ "error": msg, "transcript": transcript }))).into_response()
        }
    };
    let model_ms = model_started.elapsed().as_millis();
    let Some(arr) = extract_json_array(&answer) else {
        // A router that cannot be parsed is a routing FAILURE, said plainly, with
        // the raw reply so the miss is diagnosable — never a silent empty plan
        // that reads as "nothing to route" (ethos rule 4).
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": "the router model did not return a parseable plan",
                "raw": answer.chars().take(1200).collect::<String>(),
                "via": via,
                "transcript": transcript,
            })),
        )
            .into_response();
    };
    // Validate every routed name against the live roster: the model may never
    // invent a recipient. Dropped names are REPORTED, not swallowed.
    let names: std::collections::HashSet<&str> = roster.iter().map(|(n, _, _)| n.as_str()).collect();
    let open_cards = live_card_ids(&state);
    let mut v = Validation::default();
    let out_plan = validate_plan(&arr, &names, open_cards.as_ref(), &mut v);
    let body = Json(json!({
        "plan": out_plan,
        "transcript": transcript,
        "via": via,
        // Published, not just used for the header: the caller can see which
        // part of its wait was the model, and so can anyone reading the
        // response later without the request log to hand.
        "model_ms": model_ms,
        "took_ms": started.elapsed().as_millis() as u64,
        "dropped_unknown_workers": v.unknown_workers,
        // Each dropped class is its own list because each is a different
        // situation for the reader: the model invented a name, named a card
        // that is gone, hallucinated a verb, or asked for a verb amux refuses
        // from a spoken command. One merged "dropped" count would tell a human
        // that something was discarded without telling them whether to worry.
        "dropped_unknown_cards": v.unknown_cards,
        "dropped_unknown_verbs": v.unknown_verbs,
        "refused_verbs": v.refused_verbs,
        // WHETHER THE CARD CHECK COULD RUN (ethos rule 4). `dropped_unknown_cards: []`
        // means "every named card exists" only if the board was readable; if it
        // was not, no card can be validated and an empty list would read as a
        // clean bill of health. Then `card` actions are dropped rather than
        // waved through, and this says why.
        "cards_checked": open_cards.is_some(),
        "verbs_available": VOICE_VERBS,
    }))
    .into_response();
    // Declare the wait as the MODEL's only when the model actually dominated
    // it. A plan that took 11s around a 200ms model call is amux being slow and
    // must still file — which is what makes this a per-request declaration
    // rather than a route exemption.
    let total_ms = started.elapsed().as_millis();
    if crate::api::dominated_by_external(total_ms, model_ms) {
        return crate::api::slow_ok(body, &format!("helper-model {via} {model_ms}ms"));
    }
    body
}

/// Open, non-archived board card ids, or `None` when the board could not be
/// read. `None` is a real answer: it is what makes "no unknown cards" separable
/// from "nothing was checked".
fn live_card_ids(state: &AppState) -> Option<std::collections::HashSet<String>> {
    let conn = state.store.read().ok()?;
    let mut st = conn
        .prepare(
            "SELECT id FROM issues \
             WHERE (deleted IS NULL OR deleted = 0) AND (archived IS NULL OR archived = 0)",
        )
        .ok()?;
    let rows = st.query_map([], |r| r.get::<_, String>(0)).ok()?;
    Some(rows.flatten().collect())
}

/// What the validator threw away, by reason.
#[derive(Default, Debug)]
struct Validation {
    unknown_workers: Vec<String>,
    unknown_cards: Vec<String>,
    unknown_verbs: Vec<String>,
    refused_verbs: Vec<String>,
}

/// Turn the model's array into the validated plan.
///
/// Split out of the handler so the whole vocabulary has a test that needs no
/// model, no roster on disk and no store (ethos rule 7). Every rejection lands
/// in `v` rather than being dropped on the floor: a router that silently
/// discards half a plan looks identical to one that understood half a command.
fn validate_plan(
    arr: &Value,
    names: &std::collections::HashSet<&str>,
    cards: Option<&std::collections::HashSet<String>>,
    v: &mut Validation,
) -> Vec<Value> {
    let mut out: Vec<Value> = vec![];
    for item in arr.as_array().into_iter().flatten() {
        let w = item.get("worker").and_then(Value::as_str).unwrap_or("").trim();
        if w.is_empty() {
            continue;
        }
        if !names.contains(w) {
            v.unknown_workers.push(w.to_string());
            continue;
        }
        let why = item.get("why").and_then(Value::as_str).unwrap_or("");
        // Absent `action` is a SEND. The router shipped send-only for two weeks
        // and a model that omits the field is describing the old shape, not an
        // unknown one.
        let action = item.get("action").and_then(Value::as_str).unwrap_or("send").trim();
        let s = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
        match action {
            "board" => {
                let card = s("card");
                let note = s("note");
                let title = s("title");
                if !card.is_empty() {
                    // An APPEND names an existing card, so it is checkable and
                    // must be checked — the same discipline as the roster.
                    match cards {
                        None => v.unknown_cards.push(format!("{card} (board unreadable)")),
                        Some(set) if !set.contains(&card) => v.unknown_cards.push(card),
                        Some(_) if note.is_empty() => {}
                        Some(_) => out.push(
                            json!({"action":"board","worker":w,"card":card,"note":note,"why":why}),
                        ),
                    }
                } else if !title.is_empty() {
                    out.push(json!({"action":"board","worker":w,"title":title,"why":why}));
                }
            }
            // THE MODEL DECLINING IS A RESULT, NOT AN ABSENCE. Measured live:
            // asked to "delete the tubescience worker", the router obeyed the
            // allow-list and returned an EMPTY array — correct, and the UI
            // rendered "No workers matched", which is the one thing that did
            // not happen. The prompt now asks for this marker so a refusal
            // reaches the human through the same field a validator-side
            // refusal does.
            "refused" => {
                let verb = s("verb").to_lowercase();
                if !verb.is_empty() {
                    v.refused_verbs.push(verb);
                }
            }
            "verb" => {
                let verb = s("verb").to_lowercase();
                if verb.is_empty() {
                    continue;
                }
                if VOICE_VERBS.contains(&verb.as_str()) {
                    out.push(json!({"action":"verb","worker":w,"verb":verb,"why":why}));
                } else if ALL_KNOWN_VERBS.contains(&verb.as_str()) {
                    // Understood and DECLINED. Told apart from a hallucination
                    // because the remedy differs: this one the human can still
                    // do by hand, and should be told so rather than left
                    // thinking the router misheard them.
                    v.refused_verbs.push(verb);
                } else {
                    v.unknown_verbs.push(verb);
                }
            }
            // "send" and anything unrecognised: an unknown action word with a
            // real message is a routing the human can still use, and dropping
            // it would lose a correct instruction over a wrong label.
            _ => {
                let m = s("message");
                if !m.is_empty() {
                    out.push(json!({"action":"send","worker":w,"message":m,"why":why}));
                }
            }
        }
    }
    out
}

/// Every verb `/api/sessions/{name}/{verb}` dispatches, so a REFUSED verb can
/// be told from an INVENTED one. Kept beside [`VOICE_VERBS`] rather than
/// derived from the dispatcher, and the test says why that is a known gap.
const ALL_KNOWN_VERBS: &[&str] = &[
    "start", "stop", "clear", "duplicate", "clone", "archive", "wake", "reset", "commit-report",
    "report", "apply-template", "delete", "rename", "deploy", "send",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_array_handles_fences_prose_and_nested_brackets() {
        // Bare array.
        assert!(extract_json_array(r#"[{"worker":"a","message":"go"}]"#).is_some());
        // Fenced + prose around it.
        let fenced = "Here is the plan:\n```json\n[{\"worker\":\"a\",\"message\":\"do [x]\"}]\n```\nDone.";
        let v = extract_json_array(fenced).expect("must find the fenced array");
        assert_eq!(v.as_array().unwrap().len(), 1);
        // A ']' inside a string value must not close the array early.
        let v = extract_json_array(r#"[{"worker":"a","message":"finish item [2]"}]"#).unwrap();
        assert_eq!(v[0]["message"], json!("finish item [2]"));
        // No array at all.
        assert!(extract_json_array("no json here").is_none());
    }

    fn names() -> std::collections::HashSet<&'static str> {
        ["backend", "amux"].into_iter().collect()
    }

    /// AMUX-2984: the plan vocabulary beyond send-only, and every rejection.
    ///
    /// The router shipped send-only (AMUX-3074) with one guard that mattered:
    /// a routed name is validated against the live roster, so the model cannot
    /// invent a recipient. Board and verb actions name things too — a card id,
    /// a verb — and each gets the same discipline, dropped AND counted.
    #[test]
    fn the_plan_vocabulary_validates_every_name_it_is_given() {
        let cards: std::collections::HashSet<String> = ["AMUX-1".to_string()].into_iter().collect();
        let arr = json!([
            {"action":"send","worker":"backend","message":"ship it","why":"a"},
            // No `action` at all is a SEND: the model may still describe the
            // send-only shape this endpoint had for two weeks.
            {"worker":"amux","message":"legacy shape","why":"b"},
            {"action":"board","worker":"backend","title":"new card","why":"c"},
            {"action":"board","worker":"backend","card":"AMUX-1","note":"progress","why":"d"},
            {"action":"verb","worker":"amux","verb":"stop","why":"e"},
            // Rejections, one per class.
            {"action":"send","worker":"ghost-lane","message":"hi"},
            {"action":"board","worker":"backend","card":"AMUX-999","note":"n"},
            {"action":"verb","worker":"amux","verb":"delete"},
            {"action":"verb","worker":"amux","verb":"frobnicate"},
        ]);
        let mut v = Validation::default();
        let out = validate_plan(&arr, &names(), Some(&cards), &mut v);

        assert_eq!(out.len(), 5, "five valid actions survive: {out:#?}");
        assert_eq!(out[0]["action"], json!("send"));
        assert_eq!(out[1]["action"], json!("send"), "an absent action is a send: {:?}", out[1]);
        assert_eq!(out[2]["title"], json!("new card"));
        assert_eq!(out[3]["card"], json!("AMUX-1"));
        assert_eq!(out[4]["verb"], json!("stop"));

        // EACH REJECTION IN ITS OWN BUCKET. One merged count would tell a human
        // that something was discarded without telling them whether to worry:
        // an invented worker is a mis-hearing, a refused verb is amux declining
        // something it understood perfectly.
        assert_eq!(v.unknown_workers, vec!["ghost-lane"]);
        assert_eq!(v.unknown_cards, vec!["AMUX-999"]);
        assert_eq!(v.refused_verbs, vec!["delete"], "understood and declined");
        assert_eq!(v.unknown_verbs, vec!["frobnicate"], "not a verb at all");
    }

    /// The destructive verbs must not be one misheard word away.
    ///
    /// The endpoint plans and never executes, but the review step is a checkbox
    /// list a human skims, so the allow-list has to hold on its own. This test
    /// is the list's justification, written as assertions: each named verb is
    /// REACHABLE (so `ALL_KNOWN_VERBS` is not quietly wrong) and each is
    /// refused rather than planned.
    #[test]
    fn a_spoken_command_can_never_propose_a_destructive_verb() {
        for bad in ["delete", "archive", "reset", "clear", "rename", "deploy"] {
            assert!(ALL_KNOWN_VERBS.contains(&bad), "{bad} must be a real verb or this proves nothing");
            assert!(!VOICE_VERBS.contains(&bad), "{bad} must not be voice-proposable");
            let arr = json!([{"action":"verb","worker":"amux","verb":bad}]);
            let mut v = Validation::default();
            let out = validate_plan(&arr, &names(), None, &mut v);
            assert!(out.is_empty(), "{bad} must not reach the plan: {out:#?}");
            assert_eq!(v.refused_verbs, vec![bad], "and must be reported as REFUSED, not lost");
        }
        // THE CONTROL: the three that ARE allowed must get through, or this
        // test would pass with an empty allow-list and no verb support at all.
        for good in VOICE_VERBS {
            let arr = json!([{"action":"verb","worker":"amux","verb":good}]);
            let mut v = Validation::default();
            assert_eq!(validate_plan(&arr, &names(), None, &mut v).len(), 1, "{good} must plan");
        }
    }

    /// A REFUSAL THE MODEL MAKES MUST REACH THE HUMAN (ethos rule 4).
    ///
    /// Measured live: asked to "delete the tubescience worker and archive the
    /// backend one", the router obeyed the prompt's allow-list and returned an
    /// empty array. Correct, and the UI rendered "No workers matched" — the one
    /// thing that had not happened. The prompt now asks for a `refused` marker
    /// so a model-side refusal lands in the same field a validator-side one
    /// does, and an empty plan goes back to meaning what it says.
    #[test]
    fn a_refusal_the_model_makes_itself_is_reported_not_swallowed() {
        let arr = json!([
            {"action":"refused","worker":"amux","verb":"delete"},
            {"action":"send","worker":"backend","message":"do the safe part"},
        ]);
        let mut v = Validation::default();
        let out = validate_plan(&arr, &names(), None, &mut v);
        assert_eq!(v.refused_verbs, vec!["delete"], "the refusal must survive to the response");
        // CONTROL 1: a refusal is NOT a plan entry — it must never become
        // something the human can tick and run.
        assert_eq!(out.len(), 1, "only the send is runnable: {out:#?}");
        assert_eq!(out[0]["action"], json!("send"));
        // CONTROL 2: the prompt has to ask for the marker, or the model never
        // emits one and this branch is dead code that tests green forever.
        let p = build_prompt(&[("amux".into(), vec![], "d".into())], "delete amux");
        assert!(p.contains("\"action\":\"refused\""), "the prompt must request it: {p}");
    }

    /// An UNREADABLE board is not a clean board (ethos rule 4).
    ///
    /// `dropped_unknown_cards: []` means "every named card exists" only if the
    /// board could be read. With no card set, a `card` action is unverifiable,
    /// so it is dropped and SAID to be dropped rather than waved through on the
    /// strength of a check that never ran.
    #[test]
    fn a_card_action_is_not_waved_through_when_the_board_cannot_be_read() {
        let arr = json!([
            {"action":"board","worker":"backend","card":"AMUX-1","note":"n"},
            // The control: a CREATE names no card, so it is unaffected — the
            // board being unreadable must not take the whole vocabulary down.
            {"action":"board","worker":"backend","title":"still fine"},
        ]);
        let mut v = Validation::default();
        let out = validate_plan(&arr, &names(), None, &mut v);
        assert_eq!(out.len(), 1, "only the create survives: {out:#?}");
        assert_eq!(out[0]["title"], json!("still fine"));
        assert_eq!(v.unknown_cards.len(), 1);
        assert!(
            v.unknown_cards[0].contains("board unreadable"),
            "the reason must be the one that applies: {:?}",
            v.unknown_cards
        );
    }

    /// The prompt must name the verbs it will accept. A router told to emit
    /// verbs, with no list, proposes `delete` and gets everything refused —
    /// the allow-list would read as a broken feature rather than a policy.
    #[test]
    fn the_prompt_names_the_verbs_it_will_accept_and_the_ones_it_will_not() {
        let p = build_prompt(&[("backend".into(), vec![], "api".into())], "stop the backend");
        for v in VOICE_VERBS {
            assert!(p.contains(v), "the prompt must offer {v}: {p}");
        }
        assert!(p.contains("delete"), "and must name what it will not take: {p}");
        assert!(p.contains("\"action\":\"board\""), "the board shape must be in the schema");
    }

    #[test]
    fn build_prompt_lists_workers_with_groups_and_desc() {
        let roster = vec![
            ("backend".into(), vec!["ops".into()], "The backend API".into()),
            ("gtm".into(), vec![], "".into()),
        ];
        let p = build_prompt(&roster, "ship the thing");
        assert!(p.contains("- backend [ops] — The backend API"));
        assert!(p.contains("- gtm [] — (no description)"), "{p}");
        assert!(p.contains("ship the thing"));
    }
}
