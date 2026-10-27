//! Conversations: the Messages tab's channels and direct messages.
//!
//! Three kinds, all addressed by a `conv` key:
//! - `g:<group>`: a channel. Every group (a worker tag) is one, plus `g:general`
//!   for people with workspace-wide access. Readers are the people whose access
//!   covers the group and the workers in it.
//! - `p:<a>|<b>`: a direct message between two people (sorted keys). Private to
//!   those two.
//! - `w:<worker>`: the shared thread with one worker. Anyone who can reach the
//!   worker can read it; posting there delivers to the worker.
//!
//! People are invited members (`org_members`) plus the owner. A post that
//! @mentions a worker (or any post in a worker's thread) is delivered with the
//! ordinary send path under the poster's identity, so attribution, group
//! scoping and the worker's own Messages tab are unchanged; the outcome of each
//! send is stored on the post. Workers answer by POSTing to the same route
//! (plain-text body), with their `X-Amux-Session` as the author.

use super::org::{self, MemberScope};
use super::AppState;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/conversations", get(list))
        .route("/api/conversations/search", get(search))
        .route("/api/conversations/mentions", get(mentions))
        .route("/api/conversations/{conv}/posts", get(posts).post(create_post))
        .route("/api/conversations/{conv}/read", post(mark_read))
        .route("/api/conversations/{conv}/typing", post(typing))
}

const GENERAL: &str = "general";
const MAX_BODY: usize = 8000;
const ONLINE_FOR: Duration = Duration::from_secs(45);
const TYPING_FOR: Duration = Duration::from_secs(6);

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({"ok": false, "error": msg.into()}))).into_response()
}

// ---- who is asking ---------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) enum Who {
    Person { email: String, scope: MemberScope },
    Worker { name: String, groups: BTreeSet<String> },
}

impl Who {
    fn key(&self) -> String {
        match self {
            Who::Person { email, .. } => format!("person:{email}"),
            Who::Worker { name, .. } => format!("worker:{name}"),
        }
    }
}

fn gateway_email(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-amux-user-email")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
}

fn who(headers: &HeaderMap, lanes: &[Lane]) -> Who {
    if let Some(email) = org::local_member_actor(headers) {
        // The verified actor is `member:<email>`; people are keyed by email.
        let email = email.strip_prefix("member:").unwrap_or(email);
        return Who::Person {
            email: email.to_ascii_lowercase(),
            scope: org::local_member_scope(headers).unwrap_or(MemberScope::Global),
        };
    }
    let claimed = super::groups::hdr_worker(headers);
    // A worker's Chat companion (`<worker>@chat`) posts as that worker. It fell
    // through to the owner, so "send this to @doron" from mtp-demo's chat went
    // out as the owner person, in a thread the member who asked could not see
    // (2026-10-27).
    let base = claimed.strip_suffix("@chat").unwrap_or(&claimed);
    if let Some(lane) = lanes.iter().find(|l| l.name == base) {
        return Who::Worker { name: lane.name.clone(), groups: lane.groups.clone() };
    }
    // CLOUD: several people share one org container, and the gateway (the sole
    // writer of X-Amux-*) names each with X-Amux-User-Email. Falling through
    // to "owner" made them one person, so each could read the others' direct
    // messages and post as them. Locally the header is absent.
    if let Some(email) = gateway_email(headers) {
        return Who::Person { email, scope: MemberScope::Global };
    }
    Who::Person { email: "owner".into(), scope: MemberScope::Global }
}

/// `who`, refusing a worker claim that names no worker. Such a claim used to
/// read as the owner, so any caller with a stale or mistyped X-Amux-Session
/// (or an isolated worker, which has no Messages route) read and wrote the
/// owner's conversations as the owner.
/// The refusal is the message; callers answer it with a 403.
fn identify(headers: &HeaderMap, lanes: &[Lane]) -> Result<Who, String> {
    let me = who(headers, lanes);
    let claimed = super::groups::hdr_worker(headers);
    let person_by_header = org::local_member_actor(headers).is_some() || gateway_email(headers).is_some();
    if matches!(me, Who::Person { .. }) && !claimed.is_empty() && !person_by_header {
        tracing::warn!(claimed = %claimed, verdict = "conversation_unknown_worker_claim",
            "Messages refused a session that names no worker instead of treating it as the owner");
        return Err(format!(
            "X-Amux-Session '{claimed}' names no worker with a Messages route here (isolated workers have none); refusing rather than acting as the owner"
        ));
    }
    Ok(me)
}

// ---- the fleet and the people ---------------------------------------------

#[derive(Clone, Debug)]
struct Lane {
    name: String,
    groups: BTreeSet<String>,
}

fn lanes() -> Vec<Lane> {
    scan_lanes(false)
}

/// The ISOLATED lanes. They get no thread, no channel membership and no reply
/// route (no amux routing reaches them), but a PERSON may still @mention one:
/// a person speaks for the owner, and an owner's send already reaches an
/// isolated worker (session_verbs `isolated_peer_refusal` lets it through). On
/// sandbox.amux.io (2026-10-25) "@amux-helper can you make an mdai ..." in a
/// direct message resolved nothing and delivered nothing, and Ethan's answer
/// was "this should work".
fn isolated_lanes() -> Vec<Lane> {
    scan_lanes(true)
}

fn scan_lanes(isolated: bool) -> Vec<Lane> {
    let home = super::groups::amux_home();
    let blocked = super::groups::blocked_names(&home);
    let mut out: Vec<Lane> = std::fs::read_dir(home.join("sessions"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("env") {
                return None;
            }
            let name = p.file_stem()?.to_str()?.to_string();
            if blocked.contains(&name) {
                return None;
            }
            let env = crate::config::parse_env_file(&p);
            let unq = |k: &str| env.get(k).map(|v| v.trim().trim_matches('"').to_string()).unwrap_or_default();
            if matches!(unq("CC_ARCHIVED").to_ascii_lowercase().as_str(), "1" | "true" | "yes") {
                return None;
            }
            // ISOLATED lanes get no amux routing: no thread, no channel, no
            // reply route. They are listed apart (isolated_lanes).
            if matches!(unq("CC_ISOLATED").to_ascii_lowercase().as_str(), "1" | "true" | "yes") != isolated {
                return None;
            }
            let groups = unq("CC_TAGS")
                .split(',')
                .map(|t| t.trim().trim_matches('"').to_lowercase())
                .filter(|t| !t.is_empty())
                .collect();
            Some(Lane { name, groups })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[derive(Clone, Debug)]
struct Person {
    email: String,
    name: String,
    scope: MemberScope,
}

fn display_for(email: &str, name: &str) -> String {
    if !name.trim().is_empty() {
        return name.trim().to_string();
    }
    email.split('@').next().unwrap_or(email).to_string()
}

fn people(conn: &rusqlite::Connection, include_owner: bool) -> Vec<Person> {
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT m.email, COALESCE(m.name,''), \
                CASE WHEN t.id IS NOT NULL THEN t.scope_level \
                     WHEN COALESCE(m.team_id,'')='' THEN m.scope_level ELSE 'worker' END, \
                CASE WHEN t.id IS NOT NULL THEN t.scope_name \
                     WHEN COALESCE(m.team_id,'')='' THEN m.scope_name ELSE '__invalid_team_scope__' END \
         FROM org_members m LEFT JOIN org_teams t ON t.id=m.team_id ORDER BY m.email",
    ) {
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
        });
        for (email, name, level, sname) in rows.into_iter().flatten().flatten() {
            let scope = match level.as_str() {
                "global" => MemberScope::Global,
                "group" => MemberScope::Group(sname.to_lowercase()),
                _ => MemberScope::Worker(sname),
            };
            let email = email.to_ascii_lowercase();
            out.push(Person { name: display_for(&email, &name), email, scope });
        }
    }
    if include_owner {
        out.push(Person { email: "owner".into(), name: "owner".into(), scope: MemberScope::Global });
    }
    out
}

fn owner_has_posted(conn: &rusqlite::Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM conversation_posts WHERE author_key='person:owner' LIMIT 1",
        [],
        |_| Ok(()),
    )
    .is_ok()
}

// ---- conversation keys and who may see them --------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum Conv {
    Group(String),
    People(String, String),
    Worker(String),
}

impl Conv {
    fn parse(raw: &str) -> Option<Conv> {
        let (kind, rest) = raw.split_once(':')?;
        let rest = rest.trim();
        if rest.is_empty() {
            return None;
        }
        match kind {
            "g" => Some(Conv::Group(rest.to_lowercase())),
            "w" => Some(Conv::Worker(rest.to_string())),
            "p" => {
                let (a, b) = rest.split_once('|')?;
                let (a, b) = (a.trim().to_ascii_lowercase(), b.trim().to_ascii_lowercase());
                if a.is_empty() || b.is_empty() || a == b {
                    return None;
                }
                Some(if a < b { Conv::People(a, b) } else { Conv::People(b, a) })
            }
            _ => None,
        }
    }
    fn key(&self) -> String {
        match self {
            Conv::Group(g) => format!("g:{g}"),
            Conv::People(a, b) => format!("p:{a}|{b}"),
            Conv::Worker(w) => format!("w:{w}"),
        }
    }
}

fn dm_key(a: &str, b: &str) -> String {
    let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
    if a < b { format!("p:{a}|{b}") } else { format!("p:{b}|{a}") }
}

fn person_sees_group(scope: &MemberScope, group: &str) -> bool {
    match scope {
        MemberScope::Global => true,
        MemberScope::Group(g) => g.eq_ignore_ascii_case(group),
        MemberScope::Worker(_) => false,
    }
}

/// `MemberScope::allows_worker`, answered from the lanes already loaded (one
/// directory scan per request instead of one env read per check).
fn person_allows(scope: &MemberScope, worker: &str, lanes: &[Lane]) -> bool {
    match scope {
        MemberScope::Global => true,
        MemberScope::Worker(w) => w.eq_ignore_ascii_case(worker),
        MemberScope::Group(g) => lanes.iter().any(|l| l.name == worker && l.groups.contains(g)),
    }
}

fn may_read(who: &Who, conv: &Conv, lanes: &[Lane]) -> bool {
    // A channel exists only for a real group (or #general). Without this a
    // workspace-wide member could post to any `g:<typo>` and create a
    // conversation nobody lists.
    if let Conv::Group(g) = conv {
        if g != GENERAL && !lanes.iter().any(|l| l.groups.contains(g)) {
            return false;
        }
    }
    match (who, conv) {
        (Who::Person { scope, .. }, Conv::Group(g)) if g == GENERAL => scope.is_global(),
        (Who::Person { scope, .. }, Conv::Group(g)) => person_sees_group(scope, g),
        (Who::Person { scope, .. }, Conv::Worker(w)) => {
            lanes.iter().any(|l| &l.name == w) && person_allows(scope, w, lanes)
        }
        (Who::Person { email, .. }, Conv::People(a, b)) => email == a || email == b,
        (Who::Worker { .. }, Conv::Group(g)) if g == GENERAL => true,
        (Who::Worker { groups, .. }, Conv::Group(g)) => groups.contains(g),
        (Who::Worker { name, .. }, Conv::Worker(w)) => name == w,
        (Who::Worker { .. }, Conv::People(..)) => false,
    }
}

/// Workers a poster can reach with an @mention in this conversation.
fn mentionable_workers(who: &Who, conv: &Conv, lanes: &[Lane]) -> Vec<String> {
    match conv {
        Conv::Group(g) if g == GENERAL => lanes
            .iter()
            .filter(|l| match who {
                Who::Person { scope, .. } => person_allows(scope, &l.name, lanes),
                Who::Worker { groups, .. } => l.groups.iter().any(|x| groups.contains(x)),
            })
            .map(|l| l.name.clone())
            .collect(),
        Conv::Group(g) => lanes.iter().filter(|l| l.groups.contains(g)).map(|l| l.name.clone()).collect(),
        Conv::Worker(w) => vec![w.clone()],
        // A direct message is two people: either may hand a worker a task
        // from it, the same workers they could reach from #general.
        Conv::People(..) => match who {
            Who::Person { scope, .. } => {
                lanes.iter().filter(|l| person_allows(scope, &l.name, lanes)).map(|l| l.name.clone()).collect()
            }
            Who::Worker { .. } => Vec::new(),
        },
    }
}

/// Isolated workers a PERSON can @mention here. Never a worker poster: an
/// isolated lane refuses peer sends. Never a worker's own thread (that is
/// another worker's conversation).
fn mentionable_isolated(who: &Who, conv: &Conv, iso: &[Lane]) -> Vec<String> {
    let Who::Person { scope, .. } = who else { return Vec::new() };
    iso.iter()
        .filter(|l| match conv {
            Conv::Group(g) if g == GENERAL => person_allows(scope, &l.name, iso),
            Conv::Group(g) => l.groups.contains(g) && person_allows(scope, &l.name, iso),
            Conv::People(..) => person_allows(scope, &l.name, iso),
            Conv::Worker(_) => false,
        })
        .map(|l| l.name.clone())
        .collect()
}

/// People who can read a conversation.
fn readers(conv: &Conv, all: &[Person], lanes: &[Lane]) -> Vec<Person> {
    all.iter()
        .filter(|p| {
            may_read(&Who::Person { email: p.email.clone(), scope: p.scope.clone() }, conv, lanes)
        })
        .cloned()
        .collect()
}

fn workers_in(conv: &Conv, lanes: &[Lane]) -> Vec<String> {
    match conv {
        Conv::Group(g) if g == GENERAL => Vec::new(),
        Conv::Group(g) => lanes.iter().filter(|l| l.groups.contains(g)).map(|l| l.name.clone()).collect(),
        Conv::Worker(w) => vec![w.clone()],
        Conv::People(..) => Vec::new(),
    }
}

/// `@handle` tokens. A handle is a worker name, a person's display name, or the
/// local part of their email; matching is case-insensitive.
fn mention_tokens(body: &str) -> Vec<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' && (i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '.')) {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].is_alphanumeric() || matches!(chars[j], '-' | '_' | '.')) {
                j += 1;
            }
            let tok: String = chars[i + 1..j].iter().collect::<String>().trim_end_matches('.').to_lowercase();
            if !tok.is_empty() && !out.contains(&tok) {
                out.push(tok);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

fn resolve_mentions(body: &str, workers: &[String], people: &[Person]) -> Vec<String> {
    let mut keys = Vec::new();
    for tok in mention_tokens(body) {
        if let Some(w) = workers.iter().find(|w| w.to_lowercase() == tok) {
            keys.push(format!("worker:{w}"));
        } else if let Some(p) = people.iter().find(|p| {
            p.name.to_lowercase() == tok || p.email.split('@').next().unwrap_or("") == tok
        }) {
            keys.push(format!("person:{}", p.email));
        }
    }
    keys.dedup();
    keys
}

// ---- presence and typing (in memory: they are only true right now) --------

fn presence() -> &'static Mutex<HashMap<String, Instant>> {
    static P: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

fn seen(key: &str) {
    if let Ok(mut m) = presence().lock() {
        m.insert(key.to_string(), Instant::now());
    }
}

fn online(key: &str) -> bool {
    presence().lock().ok().and_then(|m| m.get(key).map(|t| t.elapsed() < ONLINE_FOR)).unwrap_or(false)
}

type TypingMap = HashMap<String, HashMap<String, (String, Instant)>>;
fn typing_map() -> &'static Mutex<TypingMap> {
    static T: OnceLock<Mutex<TypingMap>> = OnceLock::new();
    T.get_or_init(Default::default)
}

fn typing_now(conv: &str, except: &str) -> Vec<String> {
    let Ok(m) = typing_map().lock() else { return Vec::new() };
    m.get(conv)
        .map(|by| {
            by.iter()
                .filter(|(k, (_, t))| k.as_str() != except && t.elapsed() < TYPING_FOR)
                .map(|(_, (name, _))| name.clone())
                .collect()
        })
        .unwrap_or_default()
}

// ---- reading -----------------------------------------------------------------

fn me_json(who: &Who, all: &[Person]) -> Value {
    match who {
        Who::Person { email, scope } => {
            let name = all.iter().find(|p| &p.email == email).map(|p| p.name.clone())
                .unwrap_or_else(|| display_for(email, ""));
            json!({"key": who.key(), "kind": "person", "email": email, "name": name,
                   "scope_level": scope.level(), "scope_name": scope.name()})
        }
        Who::Worker { name, .. } => json!({"key": who.key(), "kind": "worker", "name": name}),
    }
}

/// (unread, mentions, last_ts) per conversation for one reader.
fn counts(conn: &rusqlite::Connection, reader: &str) -> HashMap<String, (i64, i64, i64)> {
    let mut out: HashMap<String, (i64, i64, i64)> = HashMap::new();
    let pat = format!("%\"{reader}\"%");
    if let Ok(mut stmt) = conn.prepare(
        "SELECT p.conv, \
                SUM(CASE WHEN p.id > COALESCE(r.last_id,0) AND p.author_key<>?1 THEN 1 ELSE 0 END), \
                SUM(CASE WHEN p.id > COALESCE(r.last_id,0) AND p.author_key<>?1 AND p.mentions LIKE ?2 THEN 1 ELSE 0 END), \
                MAX(p.ts) \
         FROM conversation_posts p LEFT JOIN conversation_reads r ON r.reader=?1 AND r.conv=p.conv \
         GROUP BY p.conv",
    ) {
        let rows = stmt.query_map(rusqlite::params![reader, pat], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
        });
        for (conv, u, m, ts) in rows.into_iter().flatten().flatten() {
            out.insert(conv, (u, m, ts));
        }
    }
    out
}

async fn list(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let lanes = lanes();
    let me = match identify(&headers, &lanes) {
        Ok(w) => w,
        Err(m) => return err(StatusCode::FORBIDDEN, m),
    };
    let my_key = me.key();
    seen(&my_key);
    let Ok(conn) = state.store.read() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "store unavailable");
    };
    let include_owner = matches!(&me, Who::Person { email, .. } if email == "owner") || owner_has_posted(&conn);
    let all = people(&conn, include_owner);
    let c = counts(&conn, &my_key);
    let stat = |k: &str| c.get(k).copied().unwrap_or((0, 0, 0));
    let (mut tu, mut tm) = (0i64, 0i64);

    let mut groups: BTreeSet<String> = lanes.iter().flat_map(|l| l.groups.iter().cloned()).collect();
    groups.insert(GENERAL.into());
    let mut channels = Vec::new();
    for g in groups {
        let conv = Conv::Group(g.clone());
        if !may_read(&me, &conv, &lanes) {
            continue;
        }
        let (u, m, ts) = stat(&conv.key());
        tu += u;
        tm += m;
        channels.push(json!({"conv": conv.key(), "name": g,
            "people": readers(&conv, &all, &lanes).len(), "workers": workers_in(&conv, &lanes).len(),
            "unread": u, "mentions": m, "last_ts": ts}));
    }

    // People: those who share at least one conversation with me (a scoped
    // member does not see the whole directory).
    let mut ppl = Vec::new();
    if let Who::Person { email: my_email, scope: my_scope } = &me {
        for p in &all {
            let shares = p.email == *my_email
                || match (my_scope, &p.scope) {
                    (MemberScope::Global, _) | (_, MemberScope::Global) => true,
                    (MemberScope::Group(a), MemberScope::Group(b)) => a == b,
                    (MemberScope::Group(g), MemberScope::Worker(w))
                    | (MemberScope::Worker(w), MemberScope::Group(g)) => {
                        super::session_verbs::lane_groups(w).contains(g)
                    }
                    (MemberScope::Worker(a), MemberScope::Worker(b)) => a == b,
                };
            if !shares {
                continue;
            }
            let conv = (p.email != *my_email).then(|| dm_key(my_email, &p.email));
            let (u, m, ts) = conv.as_deref().map(stat).unwrap_or((0, 0, 0));
            tu += u;
            tm += m;
            ppl.push(json!({"key": format!("person:{}", p.email), "email": p.email, "name": p.name,
                "me": p.email == *my_email, "online": online(&format!("person:{}", p.email)),
                "conv": conv, "unread": u, "mentions": m, "last_ts": ts}));
        }
    }

    let mut wks = Vec::new();
    for l in &lanes {
        let conv = Conv::Worker(l.name.clone());
        if !may_read(&me, &conv, &lanes) {
            continue;
        }
        let (u, m, ts) = stat(&conv.key());
        tu += u;
        tm += m;
        wks.push(json!({"name": l.name, "conv": conv.key(), "groups": l.groups,
            "unread": u, "mentions": m, "last_ts": ts}));
    }
    Json(json!({"me": me_json(&me, &all), "channels": channels, "people": ppl, "workers": wks,
        "totals": {"unread": tu, "mentions": tm},
        "measured": true, "n_considered": lanes.len()}))
    .into_response()
}

fn post_row(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let parse = |s: String| serde_json::from_str::<Value>(&s).unwrap_or(json!([]));
    Ok(json!({
        "id": r.get::<_, i64>(0)?, "conv": r.get::<_, String>(1)?,
        "author_key": r.get::<_, String>(2)?, "author_name": r.get::<_, String>(3)?,
        "body": r.get::<_, String>(4)?, "mentions": parse(r.get(5)?),
        "deliveries": parse(r.get(6)?), "ts": r.get::<_, i64>(7)?,
    }))
}

const POST_COLS: &str = "id, conv, author_key, author_name, body, mentions, deliveries, ts";

fn qs(raw: &Option<String>) -> BTreeMap<String, String> {
    super::fs::parse_qs(raw.as_deref().unwrap_or("")).into_iter().collect()
}

async fn posts(
    State(state): State<AppState>,
    Path(conv): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let lanes = lanes();
    let me = match identify(&headers, &lanes) {
        Ok(w) => w,
        Err(m) => return err(StatusCode::FORBIDDEN, m),
    };
    let Some(conv) = Conv::parse(&conv) else { return err(StatusCode::BAD_REQUEST, "unknown conversation key") };
    if !may_read(&me, &conv, &lanes) {
        return err(StatusCode::FORBIDDEN, "not a conversation you can read");
    }
    seen(&me.key());
    let q = qs(&raw);
    let after: i64 = q.get("after").and_then(|v| v.parse().ok()).unwrap_or(0);
    let limit: i64 = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(100).clamp(1, 500);
    let Ok(conn) = state.store.read() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "store unavailable");
    };
    let key = conv.key();
    let sql = format!(
        "SELECT * FROM (SELECT {POST_COLS} FROM conversation_posts WHERE conv=?1 AND id>?2 ORDER BY id DESC LIMIT ?3) ORDER BY id"
    );
    let rows: Vec<Value> = conn
        .prepare(&sql)
        .and_then(|mut s| s.query_map(rusqlite::params![key, after, limit], post_row)?.collect())
        .unwrap_or_default();
    let last_read: i64 = conn
        .query_row("SELECT last_id FROM conversation_reads WHERE reader=?1 AND conv=?2",
            rusqlite::params![me.key(), key], |r| r.get(0))
        .unwrap_or(0);
    let include_owner = owner_has_posted(&conn) || matches!(&me, Who::Person { email, .. } if email == "owner");
    let all = people(&conn, include_owner);
    let ppl: Vec<Value> = readers(&conv, &all, &lanes)
        .iter()
        .map(|p| json!({"key": format!("person:{}", p.email), "name": p.name, "online": online(&format!("person:{}", p.email))}))
        .collect();
    let mut mentionable = mentionable_workers(&me, &conv, &lanes);
    mentionable.extend(mentionable_isolated(&me, &conv, &isolated_lanes()));
    Json(json!({"conv": key, "posts": rows, "last_read": last_read,
        "people": ppl, "workers": workers_in(&conv, &lanes),
        "mentionable_workers": mentionable,
        "typing": typing_now(&key, &me.key()), "measured": true}))
    .into_response()
}

async fn mentions(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let lanes = lanes();
    let me = match identify(&headers, &lanes) {
        Ok(w) => w,
        Err(m) => return err(StatusCode::FORBIDDEN, m),
    };
    let Ok(conn) = state.store.read() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "store unavailable");
    };
    let sql = format!("SELECT {POST_COLS} FROM conversation_posts WHERE mentions LIKE ?1 ORDER BY id DESC LIMIT 200");
    let rows: Vec<Value> = conn
        .prepare(&sql)
        .and_then(|mut s| s.query_map([format!("%\"{}\"%", me.key())], post_row)?.collect())
        .unwrap_or_default();
    let rows: Vec<Value> = rows
        .into_iter()
        .filter(|p| p["conv"].as_str().and_then(Conv::parse).is_some_and(|c| may_read(&me, &c, &lanes)))
        .take(50)
        .collect();
    Json(json!({"posts": rows, "measured": true})).into_response()
}

async fn search(State(state): State<AppState>, headers: HeaderMap, RawQuery(raw): RawQuery) -> Response {
    let lanes = lanes();
    let me = match identify(&headers, &lanes) {
        Ok(w) => w,
        Err(m) => return err(StatusCode::FORBIDDEN, m),
    };
    let q = qs(&raw).get("q").cloned().unwrap_or_default();
    if q.trim().is_empty() {
        return Json(json!({"posts": [], "measured": false, "why_unmeasured": "empty query"})).into_response();
    }
    let Ok(conn) = state.store.read() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "store unavailable");
    };
    let like = format!("%{}%", q.trim().replace('%', "\\%").replace('_', "\\_"));
    let sql = format!("SELECT {POST_COLS} FROM conversation_posts WHERE body LIKE ?1 ESCAPE '\\' ORDER BY id DESC LIMIT 500");
    let rows: Vec<Value> = conn
        .prepare(&sql)
        .and_then(|mut s| s.query_map([like], post_row)?.collect())
        .unwrap_or_default();
    let considered = rows.len();
    let rows: Vec<Value> = rows
        .into_iter()
        .filter(|p| p["conv"].as_str().and_then(Conv::parse).is_some_and(|c| may_read(&me, &c, &lanes)))
        .take(50)
        .collect();
    Json(json!({"posts": rows, "measured": true, "n_considered": considered})).into_response()
}

// ---- writing -------------------------------------------------------------

fn body_text(headers: &HeaderMap, bytes: &Bytes) -> String {
    let ct = headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("");
    let raw = String::from_utf8_lossy(bytes).to_string();
    if ct.contains("json") || raw.trim_start().starts_with('{') {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(s) = v.get("body").or_else(|| v.get("text")).and_then(Value::as_str) {
                return s.trim().to_string();
            }
        }
    }
    raw.trim().to_string()
}

/// Headers that make an internal send carry the poster's identity: a member's
/// verified identity, a worker's session, or nothing (the owner).
fn send_headers(who: &Who, req: &HeaderMap) -> HeaderMap {
    let mut h = HeaderMap::new();
    match who {
        // A cloud user is not a local member: carry the gateway's own identity
        // header rather than minting member headers they never had.
        Who::Person { email, .. } if org::local_member_actor(req).is_none() && gateway_email(req).as_ref() == Some(email) => {
            if let Ok(v) = HeaderValue::from_str(email) {
                h.insert("x-amux-user-email", v);
            }
        }
        Who::Person { email, scope } if email != "owner" => {
            org::stamp_member_headers(&mut h, email, scope);
        }
        Who::Worker { name, .. } => {
            if let Ok(v) = HeaderValue::from_str(name) {
                h.insert("x-amux-session", v);
            }
        }
        Who::Person { .. } => {}
    }
    h
}

const WORKER_CHAIN: usize = 4;

/// True when the newest WORKER_CHAIN posts (newest first) are all workers'.
fn worker_chain_capped(recent: &[String]) -> bool {
    recent.len() >= WORKER_CHAIN && recent.iter().all(|a| a.starts_with("worker:"))
}

/// The text a worker receives. It names where it came from and how to answer
/// there, since the worker cannot see the Messages tab.
fn delivery_text(conv: &Conv, author: &str, body: &str) -> String {
    // Addressed to the worker, in the second person. The first version
    // ("[#chan] who: body (From amux Messages...)") read as a third party's
    // message: a live worker on sandbox.amux.io asked "are you asking me
    // to...?" in its own pane instead of answering, so the person waiting in
    // the channel saw nothing (2026-10-11).
    let (opening, place) = match conv {
        Conv::Group(g) => (format!("{author} mentioned you in #{g} (amux Messages):"), format!("#{g}")),
        Conv::Worker(_) => (format!("{author} wrote to you in your amux Messages thread:"), "your thread".to_string()),
        Conv::People(..) => (format!("{author} wrote to you in amux Messages:"), "that conversation".to_string()),
    };
    format!(
        "{opening}\n\n{body}\n\nThis is addressed to you: act on it now. Your answer must be posted to {place}, \
         because that is where {author} is reading (this pane is not). Post it with:\n\
         amux post {} <<'EOF'\n<your answer>\nEOF",
        conv.key()
    )
}

/// What one mentioned worker receives, and where its answer will be: `here`
/// (the conversation), `thread` (its own thread, for a direct message) or
/// `pane` (an isolated worker: the owner's words only, no injected
/// instructions, so its answer stays in its terminal).
fn delivery_for(conv: &Conv, author: &str, body: &str, worker: &str, isolated: bool) -> (String, &'static str) {
    if isolated {
        let place = match conv {
            Conv::Group(g) => format!("#{g}"),
            _ => "a direct message".to_string(),
        };
        return (format!("{author} (amux Messages, {place}): {body}"), "pane");
    }
    if let Conv::People(..) = conv {
        let thread = Conv::Worker(worker.to_string());
        return (format!(
            "{author} mentioned you in a direct message (amux Messages):\n\n{body}\n\nThis is addressed to you: act on it now. \
             Your answer must be posted to your amux Messages thread, where {author} can read it (this pane is not). Post it with:\n\
             amux post {} <<'EOF'\n<your answer>\nEOF",
            thread.key()
        ), "thread");
    }
    (delivery_text(conv, author, body), "here")
}

/// A worker's `p:<person>` (one person, no `|`) is its own thread plus that
/// person; anything else is unchanged.
fn route_worker_dm(me: &Who, conv: String) -> (String, Option<String>) {
    match (me, conv.strip_prefix("p:")) {
        (Who::Worker { name, .. }, Some(rest)) if !rest.contains('|') && !rest.trim().is_empty() => {
            (format!("w:{name}"), Some(rest.trim().trim_start_matches('@').to_ascii_lowercase()))
        }
        _ => (conv, None),
    }
}

/// A person by email, display name or the local part of their email.
fn find_person<'a>(all: &'a [Person], to: &str) -> Option<&'a Person> {
    all.iter().find(|p| p.email == to)
        .or_else(|| all.iter().find(|p| p.name.to_lowercase() == to || p.email.split('@').next() == Some(to)))
}

async fn create_post(
    State(state): State<AppState>,
    Path(conv): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let lanes = lanes();
    let me = match identify(&headers, &lanes) {
        Ok(w) => w,
        Err(m) => return err(StatusCode::FORBIDDEN, m),
    };
    // A worker writes to one person as `p:<email or handle>`. The post goes in
    // the worker's own thread, as the worker, @mentioning that person: they see
    // who wrote and can answer there, and whoever asked the worker to send it
    // can read it too. A worker never writes into two people's private DM.
    let (conv, dm_to) = route_worker_dm(&me, conv);
    let Some(conv) = Conv::parse(&conv) else { return err(StatusCode::BAD_REQUEST, "unknown conversation key") };
    if !may_read(&me, &conv, &lanes) {
        if let (Who::Worker { .. }, Conv::People(..)) = (&me, &conv) {
            return err(StatusCode::FORBIDDEN, "a worker cannot post in two people's direct message; \
                write to one person with p:<email> and it goes in your thread, from you");
        }
        return err(StatusCode::FORBIDDEN, "not a conversation you can post in");
    }
    let body = body_text(&headers, &bytes);
    if body.is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty message");
    }
    if body.chars().count() > MAX_BODY {
        return err(StatusCode::PAYLOAD_TOO_LARGE, format!("message over {MAX_BODY} characters"));
    }
    seen(&me.key());
    let (all, author_name) = {
        let Ok(conn) = state.store.read() else {
            return err(StatusCode::SERVICE_UNAVAILABLE, "store unavailable");
        };
        let all = people(&conn, true);
        let name = match &me {
            Who::Person { email, .. } => all.iter().find(|p| &p.email == email).map(|p| p.name.clone())
                .unwrap_or_else(|| display_for(email, "")),
            Who::Worker { name, .. } => name.clone(),
        };
        (all, name)
    };
    // A direct message must name two real people.
    if let Conv::People(a, b) = &conv {
        if ![a, b].iter().all(|k| all.iter().any(|p| &&p.email == k)) {
            return err(StatusCode::BAD_REQUEST, "a direct message needs two known people");
        }
    }
    let iso_lanes = isolated_lanes();
    let iso = mentionable_isolated(&me, &conv, &iso_lanes);
    let mut workers = mentionable_workers(&me, &conv, &lanes);
    workers.extend(iso.iter().cloned());
    let thread_readers = readers(&conv, &all, &lanes);
    let mut mentions = resolve_mentions(&body, &workers, &thread_readers);
    if let Some(to) = &dm_to {
        let Some(p) = find_person(&all, to) else {
            return err(StatusCode::BAD_REQUEST, format!("no person '{to}' in this workspace"));
        };
        if !thread_readers.iter().any(|r| r.email == p.email) {
            tracing::warn!(conv = %conv.key(), to = %p.email, verdict = "conversation_worker_dm_unreadable",
                "a worker's message to a person was refused: they cannot read the worker's thread");
            return err(StatusCode::FORBIDDEN, format!(
                "{} cannot read {}'s thread, so the message would not reach them; the owner can widen their access",
                p.email, conv.key()));
        }
        let k = format!("person:{}", p.email);
        if !mentions.contains(&k) {
            mentions.push(k);
        }
    }
    let mut targets: Vec<String> = mentions
        .iter()
        .filter_map(|k| k.strip_prefix("worker:").map(str::to_string))
        .collect();
    if let Conv::Worker(w) = &conv {
        if !targets.contains(w) {
            targets.push(w.clone());
            mentions.push(format!("worker:{w}"));
        }
    }
    if let Who::Worker { name, .. } = &me {
        targets.retain(|t| t != name);
        mentions.retain(|m| m != &format!("worker:{name}"));
        // Two workers @mentioning each other would reply forever. A worker's
        // post still lands, but delivers nothing once WORKER_CHAIN posts in a
        // row have been workers'; a person posting resets it.
        let recent: Vec<String> = state.store.read().ok().and_then(|c| {
            let mut q = c.prepare("SELECT author_key FROM conversation_posts WHERE conv=?1 ORDER BY id DESC LIMIT ?2").ok()?;
            let rows = q.query_map(rusqlite::params![conv.key(), WORKER_CHAIN as i64], |r| r.get::<_, String>(0)).ok()?;
            Some(rows.filter_map(Result::ok).collect())
        }).unwrap_or_default();
        if !targets.is_empty() && worker_chain_capped(&recent) {
            tracing::warn!(conv = %conv.key(), author = %name, held = targets.len(), measured = true, n_considered = recent.len(),
                verdict = "conversation_worker_chain_capped", "worker-to-worker mentions held: no person has posted in this chain");
            targets.clear();
        }
    }
    let (key, author_key, ts) = (conv.key(), me.key(), now_ms());
    let id_cell = Arc::new(Mutex::new(0i64));
    let (k2, a2, n2, b2, m2, id2) = (key.clone(), author_key.clone(), author_name.clone(), body.clone(),
        serde_json::to_string(&mentions).unwrap_or_else(|_| "[]".into()), id_cell.clone());
    let wrote = state
        .store
        .write_async(move |c| {
            c.execute(
                "INSERT INTO conversation_posts (conv, author_key, author_name, body, mentions, ts) VALUES (?1,?2,?3,?4,?5,?6)",
                rusqlite::params![k2, a2, n2, b2, m2, ts],
            )?;
            let id = c.last_insert_rowid();
            // Your own post is read by you.
            c.execute(
                "INSERT INTO conversation_reads (reader, conv, last_id) VALUES (?1,?2,?3) \
                 ON CONFLICT(reader, conv) DO UPDATE SET last_id=MAX(last_id, excluded.last_id)",
                rusqlite::params![a2, k2, id],
            )?;
            if let Ok(mut g) = id2.lock() {
                *g = id;
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    if let Err(e) = wrote {
        return err(StatusCode::SERVICE_UNAVAILABLE, format!("could not store the message: {e}"));
    }
    let id = *id_cell.lock().unwrap_or_else(|p| p.into_inner());
    if let Ok(mut m) = typing_map().lock() {
        if let Some(by) = m.get_mut(&key) {
            by.remove(&author_key);
        }
    }
    tracing::info!(conv = %key, author = %author_key, post = id, mentions = mentions.len(),
        deliveries = targets.len(), "conversation post");

    // Deliver to workers in the background; the post exists either way, and
    // each outcome is written back onto it.
    if !targets.is_empty() {
        let hdrs = send_headers(&me, &headers);
        let (st, conv2, author, text_body) = (state.clone(), conv.clone(), author_name.clone(), body.clone());
        tokio::spawn(async move {
            let mut outcomes = Vec::new();
            for w in targets {
                let isolated = iso.contains(&w);
                let (text, reply) = delivery_for(&conv2, &author, &text_body, &w, isolated);
                let resp = super::session_verbs::send_post(&st, &w, &hdrs, &json!({"text": text, "record_history": true})).await;
                let status = resp.status();
                let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap_or_default();
                let v: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
                let accepted = status.is_success() && v.get("ok").and_then(Value::as_bool).unwrap_or(false);
                let submission = v.get("submission").and_then(Value::as_str).unwrap_or("");
                // "confirmed" is the only state that means the worker has it now;
                // a queued send is accepted but not yet in front of the worker.
                let state = if !accepted { "failed" } else if submission == "confirmed" { "delivered" } else { "queued" };
                let ok = state != "failed";
                let detail = v.get("error").or_else(|| v.get("msg")).or_else(|| v.get("submission"))
                    .and_then(Value::as_str).unwrap_or("").to_string();
                if !ok {
                    tracing::warn!(conv = %conv2.key(), post = id, worker = %w, status = status.as_u16(), detail = %detail,
                        isolated, verdict = "conversation_delivery_failed", "a Messages post did not reach a worker");
                } else {
                    tracing::info!(conv = %conv2.key(), post = id, worker = %w, state, reply, isolated, measured = true,
                        n_considered = 1, verdict = "conversation_delivered", "a Messages post reached a worker");
                }
                outcomes.push(json!({"worker": w, "ok": ok, "state": state, "status": status.as_u16(), "detail": detail, "reply": reply}));
            }
            let js = Value::Array(outcomes).to_string();
            let _ = st
                .store
                .write_async(move |c| {
                    c.execute("UPDATE conversation_posts SET deliveries=?1 WHERE id=?2", rusqlite::params![js, id])?;
                    Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                })
                .await;
        });
    }
    (StatusCode::CREATED, Json(json!({"ok": true, "id": id, "conv": key, "author": author_key, "author_name": author_name, "mentions": mentions,
        "delivering_to": mentions.iter().filter(|m| m.starts_with("worker:")).count()})))
        .into_response()
}

async fn mark_read(
    State(state): State<AppState>,
    Path(conv): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let lanes = lanes();
    let me = match identify(&headers, &lanes) {
        Ok(w) => w,
        Err(m) => return err(StatusCode::FORBIDDEN, m),
    };
    let Some(conv) = Conv::parse(&conv) else { return err(StatusCode::BAD_REQUEST, "unknown conversation key") };
    if !may_read(&me, &conv, &lanes) {
        return err(StatusCode::FORBIDDEN, "not a conversation you can read");
    }
    let last: i64 = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|v| v.get("last_id").and_then(Value::as_i64))
        .unwrap_or(0);
    let (reader, key) = (me.key(), conv.key());
    let res = state
        .store
        .write_async(move |c| {
            let top: i64 = c.query_row("SELECT COALESCE(MAX(id),0) FROM conversation_posts WHERE conv=?1", [&key], |r| r.get(0))?;
            let upto = if last <= 0 { top } else { last.min(top) };
            c.execute(
                "INSERT INTO conversation_reads (reader, conv, last_id) VALUES (?1,?2,?3) \
                 ON CONFLICT(reader, conv) DO UPDATE SET last_id=MAX(last_id, excluded.last_id)",
                rusqlite::params![reader, key, upto],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    match res {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => err(StatusCode::SERVICE_UNAVAILABLE, e.to_string()),
    }
}

async fn typing(State(state): State<AppState>, Path(conv): Path<String>, headers: HeaderMap) -> Response {
    let lanes = lanes();
    let me = match identify(&headers, &lanes) {
        Ok(w) => w,
        Err(m) => return err(StatusCode::FORBIDDEN, m),
    };
    let Some(conv) = Conv::parse(&conv) else { return err(StatusCode::BAD_REQUEST, "unknown conversation key") };
    if !may_read(&me, &conv, &lanes) {
        return err(StatusCode::FORBIDDEN, "not a conversation you can read");
    }
    let name = match &me {
        Who::Person { email, .. } => state.store.read().ok()
            .map(|c| people(&c, true))
            .and_then(|all| all.into_iter().find(|p| &p.email == email).map(|p| p.name))
            .unwrap_or_else(|| display_for(email, "")),
        Who::Worker { name, .. } => name.clone(),
    };
    seen(&me.key());
    if let Ok(mut m) = typing_map().lock() {
        m.entry(conv.key()).or_default().insert(me.key(), (name, Instant::now()));
    }
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(name: &str, groups: &[&str]) -> Lane {
        Lane { name: name.into(), groups: groups.iter().map(|g| g.to_string()).collect() }
    }

    fn person(email: &str, scope: MemberScope) -> Who {
        Who::Person { email: email.into(), scope }
    }

    #[test]
    fn conv_keys_parse_and_normalize() {
        assert_eq!(Conv::parse("g:Wexus"), Some(Conv::Group("wexus".into())));
        assert_eq!(Conv::parse("p:b@x.com|A@x.com"), Some(Conv::People("a@x.com".into(), "b@x.com".into())));
        assert_eq!(Conv::parse("p:a@x.com|a@x.com"), None);
        assert_eq!(Conv::parse("w:ed-summary"), Some(Conv::Worker("ed-summary".into())));
        assert_eq!(Conv::parse("x:nope"), None);
        assert_eq!(Conv::parse("g:"), None);
        assert_eq!(dm_key("B@x.com", "a@x.com"), "p:a@x.com|b@x.com");
    }

    #[test]
    fn scoped_member_reads_only_their_group() {
        let lanes = vec![lane("ed", &["wexus"]), lane("other", &["mtp"])];
        let elliot = person("elliot@y.com", MemberScope::Group("wexus".into()));
        assert!(may_read(&elliot, &Conv::Group("wexus".into()), &lanes));
        assert!(!may_read(&elliot, &Conv::Group("mtp".into()), &lanes));
        assert!(!may_read(&elliot, &Conv::Group(GENERAL.into()), &lanes));
        assert!(may_read(&elliot, &Conv::Worker("ed".into()), &lanes));
        assert!(!may_read(&elliot, &Conv::Worker("other".into()), &lanes));
        // A DM is private to its two people, whatever their scope.
        let dm = Conv::People("doron@z.com".into(), "ethan@x.com".into());
        assert!(!may_read(&elliot, &dm, &lanes));
        assert!(!may_read(&person("ethan2@x.com", MemberScope::Global), &dm, &lanes));
        assert!(may_read(&person("doron@z.com", MemberScope::Global), &dm, &lanes));
    }

    #[test]
    fn a_channel_must_be_a_real_group() {
        let lanes = vec![lane("ed", &["wexus"])];
        let owner = person("owner", MemberScope::Global);
        assert!(may_read(&owner, &Conv::Group("wexus".into()), &lanes));
        assert!(may_read(&owner, &Conv::Group(GENERAL.into()), &lanes));
        assert!(!may_read(&owner, &Conv::Group("nope-nothing".into()), &lanes));
    }

    #[test]
    fn workers_read_their_groups_and_own_thread_only() {
        let lanes = vec![lane("ed", &["wexus"]), lane("other", &["mtp"])];
        let ed = Who::Worker { name: "ed".into(), groups: ["wexus".to_string()].into() };
        assert!(may_read(&ed, &Conv::Group("wexus".into()), &lanes));
        assert!(!may_read(&ed, &Conv::Group("mtp".into()), &lanes));
        assert!(may_read(&ed, &Conv::Worker("ed".into()), &lanes));
        assert!(!may_read(&ed, &Conv::Worker("other".into()), &lanes));
        assert!(!may_read(&ed, &Conv::People("a@x".into(), "b@x".into()), &lanes));
    }

    #[test]
    fn mentions_resolve_workers_and_people_in_the_conversation_only() {
        let people = vec![
            Person { email: "elliot@y.com".into(), name: "elliot".into(), scope: MemberScope::Global },
            Person { email: "doron@z.com".into(), name: "Doron G".into(), scope: MemberScope::Global },
        ];
        let workers = vec!["ed-summary-builder".to_string()];
        let got = resolve_mentions(
            "@ed-summary-builder run it, @Elliot and @doron check; mail me@here.com; @mtp-demo is not here.",
            &workers, &people);
        assert_eq!(got, vec!["worker:ed-summary-builder", "person:elliot@y.com", "person:doron@z.com"]);
        assert!(mention_tokens("ethan@x.com").is_empty(), "an email is not a mention");
    }

    #[test]
    fn mention_tokens_take_hyphens_a_leading_at_and_drop_trailing_punctuation() {
        assert_eq!(mention_tokens("@amux-helper can you make an mdai for @doron that has the weather"),
            vec!["amux-helper", "doron"]);
        assert_eq!(mention_tokens("ping @amux-helper, then @ed_x. and (@mtp-demo)"),
            vec!["amux-helper", "ed_x", "mtp-demo"]);
        let unknown = resolve_mentions("@nobody here", &["amux-helper".to_string()], &[]);
        assert!(unknown.is_empty(), "an unknown name resolves to nothing");
    }

    #[test]
    fn a_person_can_mention_an_isolated_worker_and_a_worker_cannot() {
        // Sandbox, 2026-10-25: "@amux-helper ..." in a direct message between
        // two people resolved nothing; amux-helper is isolated.
        let lanes = vec![lane("ed", &["wexus"])];
        let iso = vec![lane("amux-helper", &[])];
        let owner = Who::Person { email: "ethan@x.com".into(), scope: MemberScope::Global };
        let dm = Conv::People("doron@z.com".into(), "ethan@x.com".into());
        assert_eq!(mentionable_isolated(&owner, &dm, &iso), vec!["amux-helper".to_string()]);
        assert_eq!(mentionable_isolated(&owner, &Conv::Group(GENERAL.into()), &iso), vec!["amux-helper".to_string()]);
        assert!(mentionable_isolated(&owner, &Conv::Group("wexus".into()), &iso).is_empty(), "not in that group");
        assert!(mentionable_isolated(&owner, &Conv::Worker("ed".into()), &iso).is_empty(), "another worker's thread");
        let scoped = Who::Person { email: "doron@z.com".into(), scope: MemberScope::Worker("ed".into()) };
        assert!(mentionable_isolated(&scoped, &dm, &iso).is_empty(), "outside a member's access");
        let worker = Who::Worker { name: "ed".into(), groups: ["wexus".to_string()].into() };
        assert!(mentionable_isolated(&worker, &Conv::Group(GENERAL.into()), &iso).is_empty(), "isolated lanes refuse peer sends");
        // In a direct message, a person also reaches the workers they could from #general.
        assert_eq!(mentionable_workers(&owner, &dm, &lanes), vec!["ed".to_string()]);
        assert!(mentionable_workers(&scoped, &dm, &[lane("other", &[])]).is_empty());
    }

    #[test]
    fn an_isolated_worker_gets_the_owners_words_and_a_dm_answer_goes_to_the_thread() {
        let dm = Conv::People("doron@z.com".into(), "ethan@x.com".into());
        let (t, reply) = delivery_for(&dm, "ethan", "@amux-helper make an mdai", "amux-helper", true);
        assert_eq!(reply, "pane");
        assert_eq!(t, "ethan (amux Messages, a direct message): @amux-helper make an mdai");
        assert!(!t.contains("amux post"), "no injected instructions into an isolated worker");
        let (t, reply) = delivery_for(&dm, "ethan", "@ed run it", "ed", false);
        assert_eq!(reply, "thread");
        assert!(t.contains("amux post w:ed <<'EOF'"), "{t}");
        assert!(!t.contains('$'), "{t}");
        let (_, reply) = delivery_for(&Conv::Group("wexus".into()), "ethan", "@ed run", "ed", false);
        assert_eq!(reply, "here");
    }

    #[test]
    fn general_mentions_stay_inside_the_posters_access() {
        let lanes = vec![lane("ed", &["wexus"]), lane("other", &["mtp"])];
        let elliot = person("elliot@y.com", MemberScope::Group("wexus".into()));
        assert_eq!(mentionable_workers(&elliot, &Conv::Group(GENERAL.into()), &lanes), vec!["ed".to_string()]);
        let global = person("ethan@x.com", MemberScope::Global);
        assert_eq!(mentionable_workers(&global, &Conv::Group(GENERAL.into()), &lanes).len(), 2);
        assert_eq!(mentionable_workers(&global, &Conv::Group("mtp".into()), &lanes), vec!["other".to_string()]);
    }

    #[test]
    fn a_member_is_keyed_by_email_so_their_mentions_count() {
        // The verified actor header reads `member:<email>`; a mention stores
        // `person:<email>`. Keying the reader by the raw header made every
        // member's mention count read 0.
        let mut h = HeaderMap::new();
        org::stamp_member_headers(&mut h, "ann@test.io", &MemberScope::Group("wexus".into()));
        let me = who(&h, &[]);
        assert_eq!(me.key(), "person:ann@test.io");
        assert!(matches!(me, Who::Person { scope: MemberScope::Group(ref g), .. } if g == "wexus"));
        let people = vec![Person { email: "ann@test.io".into(), name: "ann".into(), scope: MemberScope::Global }];
        assert_eq!(resolve_mentions("@ann look", &[], &people), vec![me.key()]);
    }

    #[test]
    fn a_chat_companion_posts_as_its_worker_and_an_unknown_claim_is_not_the_owner() {
        let lanes = vec![Lane { name: "mtp-demo".into(), groups: ["video".to_string()].into_iter().collect() }];
        let hdr = |v: &'static str| {
            let mut h = HeaderMap::new();
            h.insert("x-amux-session", HeaderValue::from_static(v));
            h
        };
        assert_eq!(identify(&hdr("mtp-demo@chat"), &lanes).map(|w| w.key()).ok().as_deref(), Some("worker:mtp-demo"));
        assert_eq!(identify(&hdr("mtp-demo"), &lanes).map(|w| w.key()).ok().as_deref(), Some("worker:mtp-demo"));
        let refused = identify(&hdr("nobody@chat"), &lanes).expect_err("an unknown worker is refused");
        assert!(refused.contains("nobody@chat") && refused.contains("refusing"), "{refused}");
        assert_eq!(identify(&HeaderMap::new(), &lanes).map(|w| w.key()).ok().as_deref(), Some("person:owner"),
            "no claim at all is still the owner");
    }

    #[test]
    fn a_worker_writes_to_one_person_from_its_own_thread() {
        let w = Who::Worker { name: "mtp-demo".into(), groups: BTreeSet::new() };
        let owner = Who::Person { email: "owner".into(), scope: MemberScope::Global };
        assert_eq!(route_worker_dm(&w, "p:Doron@x.io".into()), ("w:mtp-demo".into(), Some("doron@x.io".into())));
        assert_eq!(route_worker_dm(&w, "p:@doron".into()), ("w:mtp-demo".into(), Some("doron".into())));
        assert_eq!(route_worker_dm(&w, "p:a@x.io|owner".into()), ("p:a@x.io|owner".into(), None), "a two-person DM is not rerouted");
        assert_eq!(route_worker_dm(&owner, "p:a@x.io".into()), ("p:a@x.io".into(), None), "only workers are rerouted");
        let dm = Conv::parse("p:a@x.io|owner").unwrap();
        assert!(!may_read(&w, &dm, &[]), "a worker never posts into two people's DM, so never as the owner");
        let people = vec![
            Person { email: "doron@x.io".into(), name: "doron".into(), scope: MemberScope::Global },
            Person { email: "ethan@x.io".into(), name: "Ethan".into(), scope: MemberScope::Global },
        ];
        assert_eq!(find_person(&people, "doron").map(|p| p.email.as_str()), Some("doron@x.io"));
        assert_eq!(find_person(&people, "ethan").map(|p| p.email.as_str()), Some("ethan@x.io"));
        assert!(find_person(&people, "nobody").is_none());
    }

    #[test]
    fn a_cloud_user_is_their_gateway_email_never_the_owner() {
        let mut a = HeaderMap::new();
        a.insert("x-amux-user-email", HeaderValue::from_static("Ann@Org.io"));
        let mut b = HeaderMap::new();
        b.insert("x-amux-user-email", HeaderValue::from_static("bob@org.io"));
        let (ann, bob) = (who(&a, &[]), who(&b, &[]));
        assert_eq!(ann.key(), "person:ann@org.io");
        assert_ne!(ann.key(), bob.key(), "two cloud users are two people");
        let dm = Conv::People("bob@org.io".into(), "carol@org.io".into());
        assert!(!may_read(&ann, &dm, &[]), "ann cannot read bob and carol's DM");
        assert_eq!(who(&HeaderMap::new(), &[]).key(), "person:owner", "locally the owner stays the owner");
        let h = send_headers(&ann, &a);
        assert_eq!(h.get("x-amux-user-email").and_then(|v| v.to_str().ok()), Some("ann@org.io"));
        assert!(org::local_member_actor(&h).is_none(), "no member identity is minted for a cloud user");
    }

    #[test]
    fn a_worker_only_chain_stops_delivering() {
        let w = |n: &str| format!("worker:{n}");
        assert!(worker_chain_capped(&[w("a"), w("b"), w("a"), w("b")]));
        assert!(!worker_chain_capped(&[w("a"), w("b"), "person:owner".into(), w("b")]), "a person resets it");
        assert!(!worker_chain_capped(&[w("a"), w("b")]), "short chains deliver");
    }

    #[test]
    fn delivery_text_names_the_source_and_the_reply_route() {
        let t = delivery_text(&Conv::Group("wexus".into()), "elliot", "@ed run 10/11");
        assert!(t.starts_with("elliot mentioned you in #wexus"), "{t}");
        assert!(t.contains("\n\n@ed run 10/11\n\n"));
        assert!(t.contains("addressed to you"), "a worker must read it as its own task");
        assert!(t.contains("amux post g:wexus <<'EOF'"));
        // Claude Code asks before running a command with unverifiable
        // variables, so the reply recipe must not contain any.
        assert!(!t.contains('$'), "{t}");
    }
}
