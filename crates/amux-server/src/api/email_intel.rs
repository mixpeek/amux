//! Email intelligence: what matters to the owner, inferred, then applied to the
//! inbox (AMUX-3998).
//!
//! # The split, and why it is the whole design
//!
//! Ethan asked for an inbox scored and ranked by "properties that are important
//! to the user", where those properties are inferred from the themes of the
//! messages he sends amux, recomputed periodically, using the Meta-task model
//! from Settings.
//!
//! That decomposes into two very different jobs, and keeping them apart is the
//! point:
//!
//!   THEMES are judgment. What does this person actually care about? Only a
//!   model can read 200 messages and answer that, and it is worth a model call.
//!   It runs PERIODICALLY, over the corpus, once.
//!
//!   SCORING is arithmetic. Given the themes, does this email match them? That
//!   is string and sender matching over a few hundred bytes, and spending a
//!   model call per message would be ethos rule 2 exactly — "spend model calls
//!   on judgment, not string manipulation" — while also making the inbox cost
//!   money to open and impossible to rank offline.
//!
//! So the model is asked ONE question periodically and the inbox is ranked from
//! its answer instantly, for free, as many times as you like.
//!
//! # Which model
//!
//! `mdai::resolve_model(None)`, which is `AMUX_HELPER_MODEL` — the "Meta-task
//! model" in Settings, the same knob that already drives Look Up, orchestrate
//! routing and worker summaries. Named there once, read here; no literal is
//! spelled at the call site, so the tier improves when the setting does (ethos
//! D3).
//!
//! # Honesty about the measurement
//!
//! A ranked inbox with no themes is indistinguishable from a ranked inbox whose
//! themes say nothing, and both would render as "everything scores 0". So the
//! themes document carries `measured`, `n_considered` and `why_unmeasured`
//! alongside its content, and every ranked response repeats them. A zero score
//! next to `measured: false` is a probe that never ran; next to
//! `measured: true, n_considered: 187` it is a genuine "this email is not about
//! anything you have ever asked for". Those are different facts (ethos rule 4).

use crate::config::{amux_home, now_f64};
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// How often the themes are recomputed. A person's priorities move over weeks,
/// not minutes, and this is a model call over the whole corpus — hourly would
/// be paying repeatedly for an answer that has not changed.
pub const THEME_REFRESH_SECS: u64 = 6 * 3600;

/// How many recent human messages the inference reads.
const CORPUS_LIMIT: usize = 300;

pub fn themes_path(home: &Path) -> PathBuf {
    home.join("email-themes.json")
}

/// One inferred priority, with the handles scoring uses.
///
/// `keywords` and `senders` exist so ranking needs no model. The model's job is
/// to NAME them; matching them is arithmetic.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Theme {
    pub label: String,
    /// 1-10, the model's own weighting of how much this matters.
    #[serde(default)]
    pub weight: f64,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub senders: Vec<String>,
    /// Why the model thinks this is a theme — shown in the UI so a wrong
    /// ranking is arguable rather than mysterious.
    #[serde(default)]
    pub rationale: String,
}

/// The stored inference, with its own provenance.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Themes {
    #[serde(default)]
    pub themes: Vec<Theme>,
    #[serde(default)]
    pub computed_at: f64,
    #[serde(default)]
    pub model: String,
    /// How many human messages the inference actually read.
    #[serde(default)]
    pub n_considered: usize,
    /// Did the inference RUN? A themes file with an empty list and
    /// `measured: true` means "this corpus has no clear themes"; the same list
    /// with `measured: false` means nobody asked.
    #[serde(default)]
    pub measured: bool,
    #[serde(default)]
    pub why_unmeasured: String,
}

impl Default for Themes {
    fn default() -> Self {
        Themes {
            themes: vec![],
            computed_at: 0.0,
            model: String::new(),
            n_considered: 0,
            measured: false,
            why_unmeasured: "themes have never been computed on this machine".into(),
        }
    }
}

pub fn load_themes(home: &Path) -> Themes {
    std::fs::read_to_string(themes_path(home))
        .ok()
        .and_then(|t| serde_json::from_str::<Themes>(&t).ok())
        .unwrap_or_default()
}

pub fn save_themes(home: &Path, t: &Themes) -> std::io::Result<()> {
    std::fs::create_dir_all(home)?;
    std::fs::write(themes_path(home), serde_json::to_vec_pretty(t).unwrap_or_default())
}

/// The prompt. Asks for STRUCTURE, not prose: the whole point is that scoring
/// afterwards needs no model, which requires machine-usable handles.
fn theme_prompt(corpus: &[String]) -> String {
    let joined = corpus
        .iter()
        .enumerate()
        .map(|(i, m)| format!("{}. {}", i + 1, m.chars().take(600).collect::<String>()))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Below are {} recent messages one person sent to their AI worker fleet. They reveal what \
this person actually spends attention on.\n\n\
Infer the 5-9 THEMES that characterise their priorities. For each, give concrete handles that a \
plain string matcher can use to spot a related EMAIL: keywords (lowercase, single words or short \
phrases that would literally appear in a relevant subject or body) and senders.\n\n\
SENDERS MUST BE EMAIL DOMAINS OR ADDRESSES — \"acme.com\", \"billing@stripe.com\" — and nothing \
else. The messages below are addressed to internal AI workers, so they are full of worker names \
like \"gtm-playbooks\" or \"mixpeek-general\". Those are NOT email senders and will never match \
the From line of a real message; putting them here silently disables half the ranking. If a theme \
implies no real email domain, return an EMPTY senders list — that is the correct answer, not a \
worker name.\n\n\
Return ONLY a JSON array, no prose, no code fence:\n\
[{{\"label\":\"...\",\"weight\":1-10,\"keywords\":[\"...\"],\"senders\":[\"...\"],\"rationale\":\"one sentence\"}}]\n\n\
Rules: weight is how much this person cares, 10 highest. Prefer specific keywords over generic \
ones — \"invoice\" and \"stripe\" are useful, \"work\" and \"update\" are not, because a generic \
keyword matches everything and ranks nothing. If the messages do not support a theme, return \
fewer; an empty array is a valid answer.\n\n\
MESSAGES:\n{joined}",
        corpus.len()
    )
}

/// Parse the model's answer. Tolerates a code fence and surrounding prose,
/// because a model told "JSON only" still sometimes explains itself, and failing
/// the whole inference over a markdown fence would be brittle for no reason.
pub fn parse_themes(raw: &str) -> Option<Vec<Theme>> {
    let start = raw.find('[')?;
    let end = raw.rfind(']')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Vec<Theme>>(&raw[start..=end]).ok()
}

/// Score one email against the themes. PURE and cheap — no model, no network.
///
/// Returns (score, matched theme labels). Score is the summed weight of every
/// theme with a hit, so an email touching two things you care about outranks one
/// touching either alone, which is the behaviour a person expects from "rank by
/// what matters to me".
pub fn score_email(themes: &Themes, from: &str, subject: &str, snippet: &str) -> (f64, Vec<String>) {
    let hay = format!("{} {} {}", from, subject, snippet).to_lowercase();
    let from_l = from.to_lowercase();
    let mut score = 0.0;
    let mut hits: Vec<String> = Vec::new();
    for t in &themes.themes {
        let w = if t.weight <= 0.0 { 1.0 } else { t.weight.min(10.0) };
        // A SENDER MATCH OUTWEIGHS A KEYWORD MATCH, deliberately. "from my
        // accountant" is a much stronger signal than the word "invoice"
        // appearing somewhere, and treating them equally lets a newsletter
        // mentioning the right noun outrank a real message from the right
        // person.
        let sender_hit = t
            .senders
            .iter()
            .any(|s| !s.trim().is_empty() && from_l.contains(&s.trim().to_lowercase()));
        let kw_hit = t
            .keywords
            .iter()
            .any(|k| !k.trim().is_empty() && hay.contains(&k.trim().to_lowercase()));
        if sender_hit {
            score += w * 2.0;
        }
        if kw_hit {
            score += w;
        }
        if sender_hit || kw_hit {
            hits.push(t.label.clone());
        }
    }
    (score, hits)
}

// ---------------------------------------------------------------------------
// Annotations: the human's judgments, and the signal that makes ranking improve
// ---------------------------------------------------------------------------

/// One human judgment on one message.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Annotation {
    pub verdict: String,
    pub flagged: bool,
    pub rank_delta: f64,
    pub note: String,
}

fn db_path(home: &Path) -> PathBuf {
    home.join("amux.db")
}

fn open_rw(home: &Path) -> Result<rusqlite::Connection, String> {
    let c = rusqlite::Connection::open(db_path(home)).map_err(|e| e.to_string())?;
    // WAL is already on for this database; saying so here documents why a second
    // connection alongside the main store is safe rather than leaving it to be
    // rediscovered.
    let _ = c.busy_timeout(std::time::Duration::from_secs(5));
    Ok(c)
}

/// Every annotation for an account, keyed by message id.
pub fn annotations_for(home: &Path, account: &str) -> HashMap<String, Annotation> {
    let mut out = HashMap::new();
    let Ok(c) = open_rw(home) else { return out };
    let Ok(mut st) = c.prepare(
        "SELECT message_id, COALESCE(verdict,''), flagged, rank_delta, COALESCE(note,'') \
         FROM email_annotations WHERE account = ?1",
    ) else {
        return out;
    };
    let rows = st.query_map([account], |r| {
        Ok((
            r.get::<_, String>(0)?,
            Annotation {
                verdict: r.get::<_, String>(1)?,
                flagged: r.get::<_, i64>(2)? != 0,
                rank_delta: r.get::<_, f64>(3)?,
                note: r.get::<_, String>(4)?,
            },
        ))
    });
    if let Ok(rows) = rows {
        for (k, v) in rows.flatten() {
            out.insert(k, v);
        }
    }
    out
}

/// Upsert a judgment, freezing what the ranker believed at that moment.
#[allow(clippy::too_many_arguments)]
pub fn annotate(
    home: &Path,
    account: &str,
    message_id: &str,
    verdict: Option<&str>,
    flagged: Option<bool>,
    rank_delta: Option<f64>,
    note: Option<&str>,
    score_now: Option<f64>,
    themes_now: &[String],
    from_addr: &str,
    subject: &str,
) -> Result<(), String> {
    let c = open_rw(home)?;
    let now = now_f64();
    c.execute(
        "INSERT INTO email_annotations \
           (account, message_id, verdict, flagged, rank_delta, note, score_at_annotation, \
            themes_at_annotation, from_addr, subject, created, updated) \
         VALUES (?1,?2,?3,COALESCE(?4,0),COALESCE(?5,0),?6,?7,?8,?9,?10,?11,?11) \
         -- COALESCE on INSERT because a column DEFAULT only applies when the column is
         -- OMITTED; binding an explicit NULL to a NOT NULL column fails regardless of
         -- its default. Caught by the cells rather than in production.
         ON CONFLICT(account, message_id) DO UPDATE SET \
           verdict    = COALESCE(?3, email_annotations.verdict), \
           flagged    = COALESCE(?4, email_annotations.flagged), \
           rank_delta = COALESCE(?5, email_annotations.rank_delta), \
           note       = COALESCE(?6, email_annotations.note), \
           score_at_annotation  = COALESCE(?7, email_annotations.score_at_annotation), \
           themes_at_annotation = COALESCE(?8, email_annotations.themes_at_annotation), \
           updated    = ?11",
        rusqlite::params![
            account,
            message_id,
            verdict,
            flagged.map(|b| if b { 1i64 } else { 0 }),
            rank_delta,
            note,
            score_now,
            if themes_now.is_empty() { None } else { serde_json::to_string(themes_now).ok() },
            from_addr,
            subject,
            now,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// What the human's judgments say about the themes, as text the next inference
/// can read.
///
/// THIS IS THE FEEDBACK LOOP. Without it the model re-derives the same themes
/// from the same messages forever and every correction is thrown away. With it,
/// "you ranked these high and I rejected them" is part of the next prompt.
pub fn annotation_signal(home: &Path) -> (String, usize) {
    let Ok(c) = open_rw(home) else { return (String::new(), 0) };
    let Ok(mut st) = c.prepare(
        "SELECT verdict, COALESCE(themes_at_annotation,'[]'), COALESCE(score_at_annotation,0) \
         FROM email_annotations WHERE verdict IS NOT NULL AND verdict <> '' \
         ORDER BY updated DESC LIMIT 400",
    ) else {
        return (String::new(), 0);
    };
    let rows = st.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?))
    });
    let mut approved: HashMap<String, usize> = HashMap::new();
    let mut rejected: HashMap<String, usize> = HashMap::new();
    let mut n = 0usize;
    if let Ok(rows) = rows {
        for (verdict, themes_json, _score) in rows.flatten() {
            n += 1;
            let ts: Vec<String> = serde_json::from_str(&themes_json).unwrap_or_default();
            let bucket = if verdict == "approved" { &mut approved } else { &mut rejected };
            for t in ts {
                *bucket.entry(t).or_insert(0) += 1;
            }
        }
    }
    if n == 0 {
        return (String::new(), 0);
    }
    let fmt = |m: &HashMap<String, usize>| -> String {
        let mut v: Vec<(&String, &usize)> = m.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        v.iter().take(8).map(|(k, c)| format!("{k} ({c})")).collect::<Vec<_>>().join(", ")
    };
    (
        format!(
            "\n\nTHE PERSON HAS ALSO JUDGED {n} RANKED EMAILS. Themes they KEPT: {}. Themes they \
REJECTED: {}. Weight the kept ones up and the rejected ones down, or drop a rejected theme \
entirely if it only ever produced mail they did not want.",
            if approved.is_empty() { "(none yet)".to_string() } else { fmt(&approved) },
            if rejected.is_empty() { "(none yet)".to_string() } else { fmt(&rejected) },
        ),
        n,
    )
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

async fn get_themes() -> Response {
    let t = load_themes(&amux_home());
    Json(json!({
        "themes": t.themes,
        "computed_at": t.computed_at,
        "model": t.model,
        // Published BESIDE the content, never inferred from its emptiness.
        "measured": t.measured,
        "n_considered": t.n_considered,
        "why_unmeasured": if t.measured { Value::Null } else { json!(t.why_unmeasured) },
        "refresh_every_s": THEME_REFRESH_SECS as i64,
    }))
    .into_response()
}

/// The ranked inbox for one account.
///
/// Reads the existing `/api/email/inbox` path rather than re-implementing IMAP
/// or the Gmail client: ranking is a VIEW over the inbox amux already has.
async fn ranked(
    axum::Extension(ctx): axum::Extension<std::sync::Arc<super::email::EmailCtx>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let home = amux_home();
    let themes = load_themes(&home);
    let account = q.get("account").cloned().unwrap_or_default();
    let count: usize = q.get("count").and_then(|v| v.parse().ok()).unwrap_or(40).min(200);
    let days: f64 = q.get("days").and_then(|v| v.parse().ok()).unwrap_or(7.0);

    // The SAME client `/api/email/inbox` uses, reached through the SAME
    // extension. Ranking is a VIEW over the inbox amux already has; a second
    // fetch path would be a second set of auth, retry and truncation rules to
    // keep in step.
    let fetched = ctx
        .client
        .inbox_messages(&account, count, "", days)
        .await
        .map(|v| v.get("messages").and_then(Value::as_array).cloned().unwrap_or_default());
    let msgs = match fetched {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "ok": false,
                    "error": format!("{e:?}"),
                    "account": account,
                    // The ranking half did not fail; say which half did, so a
                    // reader does not go looking at the themes for an IMAP
                    // problem.
                    "stage": "inbox_fetch",
                })),
            )
                .into_response();
        }
    };

    // FOLDERS. `inbox` is what has not been judged yet — deliberately NOT
    // "everything", because a list that keeps showing mail you already filed is
    // one you stop reading. approved/rejected are the two filed sets.
    let folder = q.get("folder").cloned().unwrap_or_else(|| "inbox".into());
    let anns = annotations_for(&home, &account);

    let mut scored: Vec<Value> = msgs
        .iter()
        .filter_map(|m| {
            let id = m.get("message_id").and_then(Value::as_str)
                .or_else(|| m.get("id").and_then(Value::as_str))
                .unwrap_or("");
            let a = anns.get(id);
            let verdict = a.map(|x| x.verdict.as_str()).unwrap_or("");
            let keep = match folder.as_str() {
                "approved" => verdict == "approved",
                "rejected" => verdict == "rejected",
                // Unfiled. An empty verdict is genuinely different from both.
                _ => verdict.is_empty(),
            };
            if !keep {
                return None;
            }
            let from = m.get("from").and_then(Value::as_str).unwrap_or("");
            let subject = m.get("subject").and_then(Value::as_str).unwrap_or("");
            let snippet = m.get("snippet").and_then(Value::as_str).unwrap_or("");
            let (base, hits) = score_email(&themes, from, subject, snippet);
            // The human's nudge is ADDITIVE and kept separate in the payload, so
            // a surprising position can be explained: "we scored it 6, you added
            // 20" is arguable; a single blended number is not.
            let delta = a.map(|x| x.rank_delta).unwrap_or(0.0);
            let flagged = a.map(|x| x.flagged).unwrap_or(false);
            // A flag is a person saying "this one, whatever the model thinks".
            // It floats above unflagged mail rather than nudging within it.
            let flag_boost = if flagged { 1000.0 } else { 0.0 };
            let mut v = m.clone();
            v["score"] = json!(base + delta + flag_boost);
            v["base_score"] = json!(base);
            v["rank_delta"] = json!(delta);
            v["flagged"] = json!(flagged);
            v["verdict"] = json!(verdict);
            v["matched_themes"] = json!(hits);
            v["note"] = json!(a.map(|x| x.note.clone()).unwrap_or_default());
            Some(v)
        })
        .collect();
    scored.sort_by(|a, b| {
        b["score"]
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&a["score"].as_f64().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Json(json!({
        "ok": true,
        "account": account,
        "folder": folder,
        "messages": scored,
        // THE THEMES' PROVENANCE TRAVELS WITH EVERY RANKING. Without this an
        // all-zero list looks like "nothing matters today" when it may mean the
        // inference has never run.
        "ranking": {
            "measured": themes.measured,
            "n_considered": themes.n_considered,
            "themes": themes.themes.len(),
            "model": themes.model,
            "computed_at": themes.computed_at,
            "why_unmeasured": if themes.measured { Value::Null } else { json!(themes.why_unmeasured) },
        },
    }))
    .into_response()
}

/// Nested INSIDE `/api/email` so these inherit that router's `EmailCtx`
/// extension — the ranked view needs the same Gmail client the inbox uses.
/// POST /api/email/annotate — file, flag, or nudge one message.
///
/// Body: `{account, message_id, verdict?, flagged?, rank_delta?, note?, score?,
///         matched_themes?, from?, subject?}`
///
/// `score` and `matched_themes` are what the CLIENT was showing when the human
/// acted. They are stored as the training signal, because by the next inference
/// the themes will have moved and the ranker's belief at the moment of
/// disagreement is unrecoverable otherwise.
async fn annotate_msg(Json(b): Json<Value>) -> Response {
    let home = amux_home();
    let sget = |k: &str| b.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let account = sget("account");
    let message_id = sget("message_id");
    if account.is_empty() || message_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "account and message_id are required"})),
        )
            .into_response();
    }
    let verdict = b.get("verdict").and_then(Value::as_str).map(str::trim);
    if let Some(v) = verdict {
        if !matches!(v, "approved" | "rejected" | "") {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "error": format!("unknown verdict '{v}'"),
                    "how": "verdict is \"approved\", \"rejected\", or \"\" to unfile it",
                })),
            )
                .into_response();
        }
    }
    let themes_now: Vec<String> = b
        .get("matched_themes")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    match annotate(
        &home,
        &account,
        &message_id,
        verdict,
        b.get("flagged").and_then(Value::as_bool),
        b.get("rank_delta").and_then(Value::as_f64),
        b.get("note").and_then(Value::as_str),
        b.get("score").and_then(Value::as_f64),
        &themes_now,
        &sget("from"),
        &sget("subject"),
    ) {
        Ok(()) => Json(json!({"ok": true, "account": account, "message_id": message_id}))
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": e})),
        )
            .into_response(),
    }
}

/// Nested INSIDE `/api/email` so these inherit that router's `EmailCtx`
/// extension — the ranked view needs the same Gmail client the inbox uses.
pub fn nested_routes() -> Router<super::AppState> {
    Router::new()
        .route("/themes", get(get_themes))
        .route("/themes/refresh", post(refresh_now))
        .route("/ranked", get(ranked))
        .route("/annotate", post(annotate_msg))
}

async fn refresh_now() -> Response {
    match tokio::task::spawn_blocking(|| recompute_themes(&amux_home())).await {
        Ok(Ok(t)) => Json(json!({
            "ok": true, "themes": t.themes.len(), "n_considered": t.n_considered,
            "model": t.model, "measured": t.measured,
        }))
        .into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"ok": false, "error": e})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Read the corpus, ask the meta model once, store the answer.
///
/// Blocking on purpose: the model client is `reqwest::blocking`, matching mdai.
pub fn recompute_themes(home: &Path) -> Result<Themes, String> {
    let corpus = human_corpus(home, CORPUS_LIMIT)?;
    if corpus.len() < 5 {
        // NOT an error and NOT a silent empty. Too small a corpus is a real,
        // nameable state, and it is the honest reason for an unranked inbox.
        let t = Themes {
            n_considered: corpus.len(),
            measured: false,
            why_unmeasured: format!(
                "only {} human messages available; too few to infer stable themes",
                corpus.len()
            ),
            ..Themes::default()
        };
        let _ = save_themes(home, &t);
        return Ok(t);
    }
    let model = super::mdai::resolve_model(None);
    let client = super::mdai::best_model();
    let (signal, n_judged) = annotation_signal(home);
    let prompt = format!("{}{}", theme_prompt(&corpus), signal);
    let out = client
        .complete(&model, &prompt)
        .map_err(|e| format!("meta-task model call failed: {e}"))?;
    let themes = parse_themes(&out).ok_or_else(|| {
        format!("could not parse a JSON theme array out of the model's answer ({} bytes)", out.len())
    })?;
    let t = Themes {
        themes,
        computed_at: now_f64(),
        model,
        n_considered: corpus.len(),
        measured: true,
        why_unmeasured: String::new(),
    };
    save_themes(home, &t).map_err(|e| e.to_string())?;
    tracing::info!(
        themes = t.themes.len(), n_considered = t.n_considered, model = %t.model,
        judgments_fed_back = n_judged,
        "email-intel: recomputed owner themes from human message history"
    );
    Ok(t)
}

/// The most recent human-authored messages, newest first.
fn human_corpus(home: &Path, limit: usize) -> Result<Vec<String>, String> {
    let db = home.join("amux.db");
    let conn = rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| format!("open {}: {e}", db.display()))?;
    // The SAME predicate `msg_kind` uses, rather than a re-spelling of it: a
    // corpus that disagreed with the Messages view about what counts as human
    // would be inferring themes from machine traffic.
    let types: Vec<String> =
        super::history::HUMAN_TYPES.iter().map(|s| s.to_string()).collect();
    let placeholders = types.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT text FROM cmd_history WHERE COALESCE(type,'') IN ({placeholders}) \
         AND text IS NOT NULL AND length(text) > 20 ORDER BY id DESC LIMIT {limit}"
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let refs: Vec<&dyn rusqlite::types::ToSql> =
        types.iter().map(|t| t as &dyn rusqlite::types::ToSql).collect();
    let rows = stmt
        .query_map(refs.as_slice(), |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

/// The periodic recompute (AMUX-3998).
///
/// Runs on the schedule rather than on demand because inferring what someone
/// cares about is a model call over the whole corpus, and their priorities move
/// over weeks. Every tick is one call at most, and a failure is logged and
/// retried next tick rather than left to a reader to discover from an inbox that
/// silently stopped ranking.
pub async fn theme_refresh_loop() {
    // AN INFINITE LOOP, because `spawn_loop` AWAITS THIS FUTURE ONCE. Its
    // `interval` argument is registry metadata for staleness reporting, not a
    // scheduler — a body that returned would run at boot and never again while
    // /api/debug/jobs advertised a 6h cadence. Exactly the shape where the view
    // and the mechanism disagree.
    loop {
        one_theme_pass().await;
        // AMUX-123: this loop never called the registry's tick() — every
        // sibling loop of this exact "do the work, then sleep for the
        // interval" shape does (see pipe_reconcile_loop). Without it, `ticks`
        // stays 0 and `last_tick_at` stays null FOREVER, regardless of
        // whether the loop is actually healthy: classify()'s own "never
        // ticked past the stall budget" rule then reports this job as
        // `stalled` on every single run, permanently, whether or not a real
        // recompute (or a legitimate skip-because-fresh) happened. Recorded
        // here rather than inside one_theme_pass() itself, so a skip counts
        // as a real, healthy pass — the loop's job is deciding whether to
        // recompute, not only the recompute itself.
        crate::runtime_jobs::registry::tick(crate::runtime_jobs::registry::ids::EMAIL_THEMES);
        tokio::time::sleep(std::time::Duration::from_secs(THEME_REFRESH_SECS)).await;
    }
}

/// One pass. Split out so the loop body is testable and the skip logic is not
/// buried inside a `loop {}` nothing can call.
async fn one_theme_pass() {
    let home = amux_home();
    // Skip the recompute when a fresh one already exists — a restart must not
    // buy a model call. The auto-builder restarts this server on every commit,
    // which on this machine is often (AMUX-3500 learned the same lesson about a
    // circuit breaker whose state lived in-process).
    let existing = load_themes(&home);
    let age = now_f64() - existing.computed_at;
    if existing.measured && age < THEME_REFRESH_SECS as f64 {
        tracing::debug!(
            age_s = age as i64, themes = existing.themes.len(),
            "email-intel: themes are fresh, skipping recompute"
        );
        return;
    }
    match tokio::task::spawn_blocking(move || recompute_themes(&amux_home())).await {
        Ok(Ok(t)) if t.measured => tracing::info!(
            themes = t.themes.len(), n_considered = t.n_considered,
            "email-intel: themes refreshed"
        ),
        // A corpus too small is a REPORTED state, not a failure and not silence.
        Ok(Ok(t)) => tracing::info!(
            n_considered = t.n_considered, why = %t.why_unmeasured,
            "email-intel: themes not computed"
        ),
        Ok(Err(e)) => tracing::warn!(
            "email-intel: theme recompute failed, inbox ranking stays on the last \
             good themes (or unranked if there are none): {e}"
        ),
        Err(e) => tracing::warn!("email-intel: theme recompute task panicked: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(label: &str, weight: f64, kw: &[&str], senders: &[&str]) -> Theme {
        Theme {
            label: label.into(),
            weight,
            keywords: kw.iter().map(|s| s.to_string()).collect(),
            senders: senders.iter().map(|s| s.to_string()).collect(),
            rationale: String::new(),
        }
    }
    fn themes(v: Vec<Theme>) -> Themes {
        Themes { themes: v, measured: true, n_considered: 100, ..Themes::default() }
    }

    /// Ranking must actually rank: an email matching a heavy theme outranks one
    /// matching a light theme, which outranks one matching nothing.
    #[test]
    fn emails_are_ordered_by_what_the_owner_cares_about() {
        let th = themes(vec![
            t("Billing", 9.0, &["invoice", "stripe"], &[]),
            t("Newsletters", 1.0, &["digest"], &[]),
        ]);
        let (hi, _) = score_email(&th, "a@b.com", "Your invoice is ready", "");
        let (lo, _) = score_email(&th, "a@b.com", "Weekly digest", "");
        let (zero, hits) = score_email(&th, "a@b.com", "lunch?", "");
        assert!(hi > lo, "heavy theme must outrank light: {hi} vs {lo}");
        assert!(lo > zero, "a light match must still outrank no match");
        assert_eq!(zero, 0.0);
        assert!(hits.is_empty(), "no match must claim no themes");
    }

    /// A SENDER match outweighs a keyword match. "from my accountant" is a
    /// stronger signal than the word "invoice" appearing somewhere, and treating
    /// them equally lets a newsletter mentioning the right noun outrank a real
    /// message from the right person.
    #[test]
    fn who_it_is_from_beats_a_word_in_the_subject() {
        let th = themes(vec![t("Billing", 5.0, &["invoice"], &["accountant.com"])]);
        let (from_person, _) = score_email(&th, "jo@accountant.com", "hello", "");
        let (mentions_word, _) = score_email(&th, "news@spam.io", "invoice tips weekly", "");
        assert!(
            from_person > mentions_word,
            "sender {from_person} must beat keyword {mentions_word}"
        );
    }

    /// Two themes hit outranks one, or "rank by what matters" fails the case
    /// where something matters twice.
    #[test]
    fn matching_two_themes_outranks_matching_one() {
        let th = themes(vec![
            t("Billing", 5.0, &["invoice"], &[]),
            t("Customer", 5.0, &["autodesk"], &[]),
        ]);
        let (both, hits) = score_email(&th, "a@b.com", "Autodesk invoice", "");
        let (one, _) = score_email(&th, "a@b.com", "Autodesk hello", "");
        assert!(both > one);
        assert_eq!(hits.len(), 2, "and it must SAY which two: {hits:?}");
    }

    /// Matching is case-insensitive across from/subject/snippet, or ranking
    /// depends on how a sender happened to capitalise.
    #[test]
    fn matching_ignores_case_and_reads_the_snippet_too() {
        let th = themes(vec![t("Billing", 5.0, &["invoice"], &[])]);
        assert!(score_email(&th, "a@b.com", "INVOICE #3", "").0 > 0.0);
        assert!(score_email(&th, "a@b.com", "hello", "your Invoice is attached").0 > 0.0);
    }

    /// THE ETHOS-RULE-4 CELL. An unmeasured themes doc and a measured one with
    /// no themes both score zero, and they mean completely different things.
    /// The distinction has to survive in the DOCUMENT, because the scores alone
    /// cannot carry it.
    #[test]
    fn an_unranked_inbox_says_whether_the_inference_ever_ran() {
        let never = Themes::default();
        assert!(!never.measured);
        assert!(
            never.why_unmeasured.contains("never been computed"),
            "an absent inference must name itself: {}", never.why_unmeasured
        );
        assert_eq!(score_email(&never, "a@b.com", "invoice", "").0, 0.0);

        // A REAL measurement that found nothing scores the same and reads
        // differently. Same number, different fact.
        let empty_but_measured = Themes { measured: true, n_considered: 187, ..Themes::default() };
        assert_eq!(score_email(&empty_but_measured, "a@b.com", "invoice", "").0, 0.0);
        assert!(empty_but_measured.measured);
        assert_eq!(empty_but_measured.n_considered, 187);
    }

    /// The model is told "JSON only" and sometimes explains itself anyway.
    /// Failing the whole inference over a code fence would be brittle for no
    /// reason.
    #[test]
    fn theme_parsing_survives_a_fence_and_surrounding_prose() {
        let raw = "Here are the themes:\n```json\n[{\"label\":\"Billing\",\"weight\":8,\
                   \"keywords\":[\"invoice\"],\"senders\":[],\"rationale\":\"asks about money\"}]\n```\nHope that helps!";
        let parsed = parse_themes(raw).expect("must parse through the noise");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].label, "Billing");
        assert_eq!(parsed[0].weight, 8.0);
        // CONTROL: genuinely unusable output must return None, not an empty vec
        // that would be stored as a measured "no themes".
        assert!(parse_themes("I could not do that").is_none());
        assert!(parse_themes("").is_none());
    }

    /// A zero or absent weight must not silently delete a theme from the
    /// ranking — the model omitting a field should degrade to "counts once",
    /// not "counts never".
    #[test]
    fn a_missing_weight_still_counts() {
        let th = themes(vec![t("Billing", 0.0, &["invoice"], &[])]);
        assert!(score_email(&th, "a@b.com", "invoice", "").0 > 0.0);
    }

    #[test]
    fn themes_round_trip_through_disk_with_their_provenance() {
        let d = tempfile::tempdir().unwrap();
        let src = Themes {
            themes: vec![t("Billing", 7.0, &["invoice"], &["acme.com"])],
            computed_at: 1234.0,
            model: "claude-haiku".into(),
            n_considered: 42,
            measured: true,
            why_unmeasured: String::new(),
        };
        save_themes(d.path(), &src).unwrap();
        let back = load_themes(d.path());
        assert!(back.measured);
        assert_eq!(back.n_considered, 42, "provenance must survive, not just content");
        assert_eq!(back.model, "claude-haiku");
        assert_eq!(back.themes[0].senders, vec!["acme.com".to_string()]);
    }
}

#[cfg(test)]
mod annotation_tests {
    use super::*;

    fn home_with_db() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let c = rusqlite::Connection::open(d.path().join("amux.db")).unwrap();
        c.execute_batch(include_str!("../../migrations/0049_email_annotations.sql")).unwrap();
        d
    }

    /// A judgment must survive with the CONTEXT it was made in. The verdict
    /// alone teaches nothing once the themes are recomputed.
    #[test]
    fn an_annotation_freezes_what_the_ranker_believed_at_the_time() {
        let d = home_with_db();
        annotate(
            d.path(), "a@b.com", "m1", Some("rejected"), None, None, None,
            Some(14.0), &["Newsletters".into(), "Billing".into()], "spam@x.io", "Weekly digest",
        )
        .unwrap();
        let (signal, n) = annotation_signal(d.path());
        assert_eq!(n, 1);
        assert!(signal.contains("Newsletters"), "the rejected theme must reach the next prompt: {signal}");
        assert!(signal.contains("REJECTED"), "and be on the rejected side: {signal}");
    }

    /// THE FEEDBACK LOOP IS THE POINT. Kept and rejected themes must land on
    /// opposite sides of the signal, or the next inference learns nothing.
    #[test]
    fn kept_and_rejected_themes_are_reported_separately() {
        let d = home_with_db();
        annotate(d.path(), "a", "m1", Some("approved"), None, None, None, Some(9.0), &["Customer".into()], "", "").unwrap();
        annotate(d.path(), "a", "m2", Some("rejected"), None, None, None, Some(9.0), &["Newsletters".into()], "", "").unwrap();
        let (signal, n) = annotation_signal(d.path());
        assert_eq!(n, 2);
        let kept_at = signal.find("KEPT").unwrap();
        let rej_at = signal.find("REJECTED").unwrap();
        let kept_side = &signal[kept_at..rej_at];
        let rej_side = &signal[rej_at..];
        assert!(kept_side.contains("Customer"), "kept side: {kept_side}");
        assert!(rej_side.contains("Newsletters"), "rejected side: {rej_side}");
        assert!(!kept_side.contains("Newsletters"), "a rejected theme must not read as kept");
    }

    /// An upsert must not wipe fields the caller did not send. Flagging a
    /// message should not silently clear its verdict.
    #[test]
    fn a_partial_update_preserves_the_fields_it_did_not_mention() {
        let d = home_with_db();
        annotate(d.path(), "a", "m1", Some("approved"), None, Some(5.0), None, None, &[], "", "").unwrap();
        // Flag only.
        annotate(d.path(), "a", "m1", None, Some(true), None, None, None, &[], "", "").unwrap();
        let all = annotations_for(d.path(), "a");
        let a = all.get("m1").expect("row");
        assert_eq!(a.verdict, "approved", "the verdict must survive a flag-only update");
        assert_eq!(a.rank_delta, 5.0, "and so must the nudge");
        assert!(a.flagged);
    }

    /// Unfiled is a THIRD state. If an unjudged message read as approved or
    /// rejected, the inbox folder would be wrong on day one.
    #[test]
    fn an_unjudged_message_has_no_verdict() {
        let d = home_with_db();
        annotate(d.path(), "a", "m1", None, Some(true), None, None, None, &[], "", "").unwrap();
        let all = annotations_for(d.path(), "a");
        assert_eq!(all.get("m1").unwrap().verdict, "", "flagged is not filed");
        let (_, n) = annotation_signal(d.path());
        assert_eq!(n, 0, "a message with no verdict teaches the inference nothing");
    }

    /// Annotations are per-account. Two mailboxes must not share judgments.
    #[test]
    fn annotations_do_not_leak_between_accounts() {
        let d = home_with_db();
        annotate(d.path(), "a@x.com", "m1", Some("approved"), None, None, None, None, &[], "", "").unwrap();
        assert_eq!(annotations_for(d.path(), "a@x.com").len(), 1);
        assert!(annotations_for(d.path(), "b@y.com").is_empty());
    }

    /// With no judgments the signal is EMPTY, not a sentence claiming none.
    /// An empty string appends nothing to the prompt; a "none so far" paragraph
    /// would be tokens telling the model about an absence it can do nothing with.
    #[test]
    fn no_judgments_appends_nothing_to_the_prompt() {
        let d = home_with_db();
        let (signal, n) = annotation_signal(d.path());
        assert_eq!(n, 0);
        assert!(signal.is_empty(), "got {signal:?}");
    }
}
