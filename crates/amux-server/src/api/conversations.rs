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
        .route("/api/conversations/channels", get(list_channels).post(create_channel))
        .route("/api/conversations/channels/{name}", axum::routing::patch(patch_channel))
        .route("/api/conversations/channels/{name}/members", get(channel_members).post(add_member))
        .route("/api/conversations/channels/{name}/members/{email}", axum::routing::delete(remove_member))
        .route("/api/conversations/{conv}/posts", get(posts).post(create_post))
        .route("/api/conversations/{conv}/read", post(mark_read))
        .route("/api/conversations/{conv}/typing", post(typing))
        .route("/api/conversations/posts/{id}", axum::routing::patch(edit_post).delete(delete_post))
        .route("/api/conversations/posts/{id}/reactions", post(react_post))
        .route("/api/conversations/unfurl", get(unfurl))
        .route("/api/profiles", get(list_profiles))
        .route("/api/profiles/avatar/{name}", get(get_avatar))
        .route("/api/profiles/{key}/avatar", post(set_avatar))
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

// ---- message history rows as thread authors --------------------------------

/// Who a message-history row is, keyed the way a post is keyed.
///
/// A worker's thread merges in that worker's message history (what reached it
/// from its own page). The dashboard used to build the author from the raw
/// `origin` stamp, so a member's send from the worker page read as
/// `person:member:ethan@mixpeek.com` with an "M" initial, while the same
/// person's post in Messages is `person:ethan@mixpeek.com` (who() strips the
/// `member:` prefix) with their name and picture (AA-53, 2026-10-28). This is
/// the one mapping, computed when history is read, so stored rows render right.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryAuthor {
    pub key: String,
    pub name: String,
    /// person, worker, schedule, harness, or unresolved.
    pub verdict: &'static str,
}

/// Invited members' display names by email, as Messages shows them.
pub(crate) fn member_names(conn: &rusqlite::Connection) -> HashMap<String, String> {
    people(conn, false).into_iter().map(|p| (p.email, p.name)).collect()
}

fn looks_like_email(s: &str) -> bool {
    let mut parts = s.splitn(2, '@');
    let (a, b) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    !a.is_empty() && b.contains('.') && !s.chars().any(|c| c.is_whitespace() || c == ':')
}

pub(crate) fn history_author(mtype: &str, origin: &str, names: &HashMap<String, String>) -> HistoryAuthor {
    let t = mtype.trim().to_ascii_lowercase();
    let o = origin.trim();
    let person = |email: &str| {
        let e = email.trim().to_ascii_lowercase();
        let name = names.get(&e).cloned().unwrap_or_else(|| display_for(&e, ""));
        HistoryAuthor { key: format!("person:{e}"), name, verdict: "person" }
    };
    if t == "session" && !o.is_empty() {
        // A worker's Chat companion sends as `<worker>@chat`.
        let base = o.strip_suffix("@chat").unwrap_or(o);
        return HistoryAuthor { key: format!("worker:{base}"), name: base.to_string(), verdict: "worker" };
    }
    if t == "schedule" {
        return HistoryAuthor { key: "schedule:scheduler".into(), name: "scheduler".into(), verdict: "schedule" };
    }
    // A verified local member (org.rs stamps `member:<email>`).
    if let Some(email) = o.strip_prefix("member:").filter(|e| looks_like_email(e)) {
        return person(email);
    }
    match super::history::msg_kind(&t) {
        "human" | "chat" => {
            if o.is_empty() || (t == "chat" && o == "chat-companion") || o == "owner" {
                return HistoryAuthor { key: "person:owner".into(), name: "owner".into(), verdict: "person" };
            }
            // A cloud gateway user is stamped with their email.
            if looks_like_email(o) {
                return person(o);
            }
        }
        "amux" => {
            let n = if o.is_empty() { "amux" } else { o };
            return HistoryAuthor { key: format!("harness:{n}"), name: n.to_string(), verdict: "harness" };
        }
        _ => {}
    }
    note_unresolved_author(&t, o);
    let n = if o.is_empty() { "unknown" } else { o };
    HistoryAuthor { key: format!("origin:{n}"), name: n.to_string(), verdict: "unresolved" }
}

/// One WARN per distinct (type, origin) per process, so a sweep sees each
/// stamp Messages could not put a person or worker to, without a line per row.
fn note_unresolved_author(mtype: &str, origin: &str) {
    static SEEN: OnceLock<Mutex<BTreeSet<(String, String)>>> = OnceLock::new();
    let k = (mtype.to_string(), origin.to_string());
    let fresh = SEEN.get_or_init(|| Mutex::new(BTreeSet::new())).lock().map(|mut s| s.insert(k)).unwrap_or(false);
    if fresh {
        tracing::warn!(verdict = "history_author_unresolved", r#type = %mtype, origin = %origin,
            "Messages could not resolve a message-history origin to a person or worker; it shows the raw origin");
    }
}

fn owner_has_posted(conn: &rusqlite::Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM conversation_posts WHERE author_key='person:owner' LIMIT 1",
        [],
        |_| Ok(()),
    )
    .is_ok()
}

// ---- channels an owner or admin configured ----------------------------------
//
// A channel IS a group: `g:<group>` and the group's name are the same key.
// What an owner or admin configures about one lives in the group's own row,
// `group_config` (title, private, archived, and `channel_created_at` for a
// group that exists before any worker carries its tag). Who reads a channel
// still starts from each member's team scope; `conversation_members` keeps
// only the exceptions to that rule (a person added outside their team, or
// removed from a channel their team covers). Teams cannot express this: a
// member has exactly one team, and adding someone to a second channel must
// not move them out of their first.

#[derive(Clone, Debug, Default)]
struct ChannelMeta {
    title: String,
    created: bool,
    archived: bool,
    private: bool,
}

#[derive(Clone, Debug, Default)]
struct Channels {
    meta: BTreeMap<String, ChannelMeta>,
    /// conv key -> email -> true (added) | false (removed).
    members: HashMap<String, BTreeMap<String, bool>>,
    /// The configuration could not be read (an old schema, a locked store).
    unreadable: bool,
}

impl Channels {
    fn meta(&self, g: &str) -> ChannelMeta {
        self.meta.get(g).cloned().unwrap_or_default()
    }
    fn member_override(&self, conv: &str, email: &str) -> Option<bool> {
        self.members.get(conv).and_then(|m| m.get(email).copied())
    }
    fn load(conn: &rusqlite::Connection) -> Channels {
        let mut out = Channels::default();
        let meta = conn.prepare(
            "SELECT name, COALESCE(channel_title,''), channel_created_at IS NOT NULL, \
                    channel_archived_at IS NOT NULL, COALESCE(channel_private,0) \
             FROM group_config WHERE channel_created_at IS NOT NULL OR channel_archived_at IS NOT NULL \
                OR COALESCE(channel_title,'')<>'' OR COALESCE(channel_private,0)<>0",
        ).and_then(|mut q| {
            q.query_map([], |r| Ok((r.get::<_, String>(0)?, ChannelMeta {
                title: r.get(1)?, created: r.get(2)?, archived: r.get(3)?, private: r.get::<_, i64>(4)? != 0,
            })))?.collect::<rusqlite::Result<Vec<_>>>()
        });
        match meta {
            Ok(rows) => out.meta = rows.into_iter().map(|(n, m)| (n.to_lowercase(), m)).collect(),
            Err(e) => {
                out.unreadable = true;
                tracing::warn!(error = %e, measured = false, verdict = "conversation_channels_unreadable",
                    "channel configuration could not be read; channels fall back to team access");
            }
        }
        let rows = conn.prepare("SELECT conv, email, state FROM conversation_members").and_then(|mut q| {
            q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()
        });
        match rows {
            Ok(rows) => {
                for (conv, email, state) in rows {
                    out.members.entry(conv).or_default().insert(email.to_ascii_lowercase(), state == "added");
                }
            }
            Err(e) => {
                out.unreadable = true;
                tracing::warn!(error = %e, measured = false, verdict = "conversation_channels_unreadable",
                    "channel membership could not be read; channels fall back to team access");
            }
        }
        out
    }
}

/// The lanes plus the channel configuration: everything `may_read` needs.
/// Derefs to the lanes, so every helper that only needs workers takes it.
#[derive(Clone, Debug, Default)]
struct Fleet {
    lanes: Vec<Lane>,
    ch: Channels,
}

impl std::ops::Deref for Fleet {
    type Target = [Lane];
    fn deref(&self) -> &[Lane] {
        &self.lanes
    }
}

impl Fleet {
    #[cfg(test)]
    fn of(lanes: Vec<Lane>) -> Fleet {
        Fleet { lanes, ch: Channels::default() }
    }
}

fn fleet(state: &AppState) -> Fleet {
    let ch = state.store.read().map(|c| Channels::load(&c)).unwrap_or_else(|_| Channels { unreadable: true, ..Default::default() });
    Fleet { lanes: lanes(), ch }
}

/// A channel exists for #general, every group a worker carries, and every
/// group an owner or admin created here (archived ones included).
fn channel_exists(g: &str, f: &Fleet) -> bool {
    g == GENERAL || f.lanes.iter().any(|l| l.groups.contains(g)) || f.ch.meta(g).created
}

/// Who reads a channel when nobody added or removed them by hand: the team
/// scope, unless the channel is private (then only the people added).
fn default_channel_access(scope: &MemberScope, g: &str, f: &Fleet) -> bool {
    if f.ch.meta(g).private {
        return false;
    }
    if g == GENERAL { scope.is_global() } else { person_sees_group(scope, g) }
}

// ---- conversation keys and who may see them --------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Conv {
    Group(String),
    People(String, String),
    Worker(String),
}

impl Conv {
    pub(crate) fn parse(raw: &str) -> Option<Conv> {
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
    pub(crate) fn key(&self) -> String {
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

/// A worker a PERSON @mentioned in a direct message, or in a channel outside
/// its groups, may read and answer that one thread, and nothing else of the
/// conversation. `root` is any post id; it is resolved to its thread's root.
fn worker_invited(conn: &rusqlite::Connection, conv: &Conv, root: i64, worker: &str) -> bool {
    if matches!(conv, Conv::Worker(_)) {
        return false;
    }
    conn.query_row(
        "SELECT 1 FROM conversation_posts WHERE conv=?1 AND author_key LIKE 'person:%' AND mentions LIKE ?3 \
         AND (id=(SELECT COALESCE(parent_id, id) FROM conversation_posts WHERE id=?2 AND conv=?1) \
           OR parent_id=(SELECT COALESCE(parent_id, id) FROM conversation_posts WHERE id=?2 AND conv=?1)) LIMIT 1",
        rusqlite::params![conv.key(), root, format!("%\"worker:{worker}\"%")],
        |_| Ok(()),
    )
    .is_ok()
}

fn may_read(who: &Who, conv: &Conv, lanes: &Fleet) -> bool {
    // A channel exists only for a real group, #general, or one an owner or
    // admin created. Without this a workspace-wide member could post to any
    // `g:<typo>` and create a conversation nobody lists. An archived channel
    // is closed to everyone until it is restored.
    if let Conv::Group(g) = conv {
        if !channel_exists(g, lanes) || lanes.ch.meta(g).archived {
            return false;
        }
    }
    match (who, conv) {
        // The owner reads every channel; nobody can remove them.
        (Who::Person { email, .. }, Conv::Group(_)) if email == "owner" => true,
        (Who::Person { email, scope }, Conv::Group(g)) => match lanes.ch.member_override(&conv.key(), email) {
            Some(added) => added,
            None => default_channel_access(scope, g, lanes),
        },
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
        // A channel's own workers first; a PERSON may also bring in any other
        // worker they can reach, as in Slack (Ethan, 2026-10-28: "@workflows"
        // in #wexus reached nobody because workflows is in another group). That
        // worker answers in the post's thread and reads only that thread
        // (worker_invited). A worker poster stays inside the channel's group.
        // A person added to a channel by hand reaches only the workers their
        // own access covers: channel membership grants the conversation, not
        // the group's workers.
        Conv::Group(g) => match who {
            Who::Person { scope, .. } => {
                let mut v: Vec<String> = lanes.iter().filter(|l| l.groups.contains(g) && person_allows(scope, &l.name, lanes)).map(|l| l.name.clone()).collect();
                v.extend(lanes.iter().filter(|l| !l.groups.contains(g) && person_allows(scope, &l.name, lanes)).map(|l| l.name.clone()));
                v
            }
            Who::Worker { .. } => lanes.iter().filter(|l| l.groups.contains(g)).map(|l| l.name.clone()).collect(),
        },
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
            // Any channel, as for other workers: a person brings in a worker
            // they can reach. An isolated one answers in its terminal.
            Conv::Group(_) | Conv::People(..) => person_allows(scope, &l.name, iso),
            Conv::Worker(_) => false,
        })
        .map(|l| l.name.clone())
        .collect()
}

/// People who can read a conversation.
fn readers(conv: &Conv, all: &[Person], lanes: &Fleet) -> Vec<Person> {
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

/// The newest post id in a conversation, thread replies included: what a
/// reader looking at it has caught up to.
fn latest_post_id(conn: &rusqlite::Connection, conv: &str) -> i64 {
    conn.query_row("SELECT COALESCE(MAX(id),0) FROM conversation_posts WHERE conv=?1", rusqlite::params![conv], |r| r.get(0))
        .unwrap_or(0)
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
    let lanes = fleet(&state);
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

    let mut channels = Vec::new();
    for g in channel_names(&lanes) {
        let conv = Conv::Group(g.clone());
        if !may_read(&me, &conv, &lanes) {
            continue;
        }
        let (u, m, ts) = stat(&conv.key());
        tu += u;
        tm += m;
        let meta = lanes.ch.meta(&g);
        channels.push(json!({"conv": conv.key(), "name": g, "title": meta.title, "private": meta.private,
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
    for l in lanes.iter() {
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
    let mut me_v = me_json(&me, &all);
    // Shown so the Messages tab can offer channel settings; every settings
    // route checks again on its own.
    me_v["can_admin_channels"] = json!(channel_admin(&headers, &lanes).is_ok());
    Json(json!({"me": me_v, "channels": channels, "people": ppl, "workers": wks,
        "totals": {"unread": tu, "mentions": tm},
        "measured": true, "n_considered": lanes.len()}))
    .into_response()
}

fn post_row(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let parse = |s: String| serde_json::from_str::<Value>(&s).unwrap_or(json!([]));
    let deleted = r.get::<_, Option<i64>>(10)?;
    Ok(json!({
        "id": r.get::<_, i64>(0)?, "conv": r.get::<_, String>(1)?,
        "author_key": r.get::<_, String>(2)?, "author_name": r.get::<_, String>(3)?,
        // A deleted post keeps its place in the conversation, not its words.
        "body": if deleted.is_some() { String::new() } else { r.get::<_, String>(4)? },
        "mentions": parse(r.get(5)?),
        "deliveries": parse(r.get(6)?), "ts": r.get::<_, i64>(7)?,
        "parent_id": r.get::<_, Option<i64>>(8)?, "edited_at": r.get::<_, Option<i64>>(9)?,
        "deleted": deleted.is_some(),
        "reactions": serde_json::from_str::<Value>(&r.get::<_, String>(11)?).unwrap_or(json!({})),
        "attachments": if deleted.is_some() { json!([]) } else { parse(r.get(12)?) },
    }))
}

/// Reply counts, the last reply's time and up to three repliers, for the
/// top-level posts shown (Slack's "3 replies" line under a post).
fn add_thread_summaries(conn: &rusqlite::Connection, rows: &mut [Value]) {
    for row in rows.iter_mut() {
        let Some(id) = row["id"].as_i64() else { continue };
        let (n, last): (i64, Option<i64>) = conn.query_row(
            "SELECT COUNT(*), MAX(ts) FROM conversation_posts WHERE parent_id=?1 AND deleted_at IS NULL",
            rusqlite::params![id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap_or((0, None));
        if n == 0 {
            continue;
        }
        let who: Vec<String> = conn.prepare(
            "SELECT author_name FROM conversation_posts WHERE parent_id=?1 AND deleted_at IS NULL GROUP BY author_key ORDER BY MAX(id) DESC LIMIT 3")
            .and_then(|mut q| q.query_map(rusqlite::params![id], |r| r.get(0))?.collect()).unwrap_or_default();
        row["reply_count"] = json!(n);
        row["last_reply_ts"] = json!(last);
        row["repliers"] = json!(who);
    }
}

/// The pictures and names of the authors on a page (`profiles` rows).
fn profiles_for(conn: &rusqlite::Connection, rows: &[Value]) -> Value {
    let mut out = serde_json::Map::new();
    for row in rows {
        let Some(k) = row["author_key"].as_str() else { continue };
        if out.contains_key(k) {
            continue;
        }
        if let Ok((avatar, name)) = conn.query_row("SELECT avatar, display_name FROM profiles WHERE key=?1", rusqlite::params![k],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?))) {
            out.insert(k.to_string(), json!({"avatar": avatar.map(|a| format!("/api/profiles/avatar/{a}")), "display_name": name}));
        }
    }
    Value::Object(out)
}

const POST_COLS: &str = "id, conv, author_key, author_name, body, mentions, deliveries, ts, parent_id, edited_at, deleted_at, reactions, attachments";

fn qs(raw: &Option<String>) -> BTreeMap<String, String> {
    super::fs::parse_qs(raw.as_deref().unwrap_or("")).into_iter().collect()
}

async fn posts(
    State(state): State<AppState>,
    Path(conv): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let lanes = fleet(&state);
    let me = match identify(&headers, &lanes) {
        Ok(w) => w,
        Err(m) => return err(StatusCode::FORBIDDEN, m),
    };
    let Some(conv) = Conv::parse(&conv) else { return err(StatusCode::BAD_REQUEST, "unknown conversation key") };
    let q = qs(&raw);
    let invited = |t: Option<i64>| match (&me, t) {
        (Who::Worker { name, .. }, Some(t)) => state.store.read().is_ok_and(|c| worker_invited(&c, &conv, t, name)),
        _ => false,
    };
    if !may_read(&me, &conv, &lanes) && !invited(q.get("thread").and_then(|v| v.parse().ok())) {
        return err(StatusCode::FORBIDDEN, "not a conversation you can read");
    }
    seen(&me.key());
    let after: i64 = q.get("after").and_then(|v| v.parse().ok()).unwrap_or(0);
    // Older history, a page at a time (scrolling up).
    let before: i64 = q.get("before").and_then(|v| v.parse().ok()).unwrap_or(i64::MAX);
    let limit: i64 = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(100).clamp(1, 500);
    // `thread=<root>`: the root and its replies. Otherwise the top level.
    let thread: Option<i64> = q.get("thread").and_then(|v| v.parse().ok());
    let Ok(conn) = state.store.read() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "store unavailable");
    };
    let key = conv.key();
    let (sql, tid) = match thread {
        Some(t) => (format!(
            "SELECT * FROM (SELECT {POST_COLS} FROM conversation_posts WHERE conv=?1 AND id>?2 AND id<?4 AND (id=?5 OR parent_id=?5) ORDER BY id DESC LIMIT ?3) ORDER BY id"), t),
        None => (format!(
            "SELECT * FROM (SELECT {POST_COLS} FROM conversation_posts WHERE conv=?1 AND id>?2 AND id<?4 AND parent_id IS NULL AND ?5=?5 ORDER BY id DESC LIMIT ?3) ORDER BY id"), 0),
    };
    let mut rows: Vec<Value> = conn
        .prepare(&sql)
        .and_then(|mut s| s.query_map(rusqlite::params![key, after, limit, before, tid], post_row)?.collect())
        .unwrap_or_default();
    if thread.is_none() {
        add_thread_summaries(&conn, &mut rows);
    }
    let has_more = rows.first().and_then(|r| r["id"].as_i64()).is_some_and(|first| {
        conn.query_row("SELECT 1 FROM conversation_posts WHERE conv=?1 AND id<?2 AND parent_id IS NULL LIMIT 1",
            rusqlite::params![key, first], |_| Ok(())).is_ok()
    });
    let profiles = profiles_for(&conn, &rows);
    let last_read: i64 = conn
        .query_row("SELECT last_id FROM conversation_reads WHERE reader=?1 AND conv=?2",
            rusqlite::params![me.key(), key], |r| r.get(0))
        .unwrap_or(0);
    // The newest post in the conversation, thread replies included. The
    // unread count covers replies, but the top-level page cannot see a reply
    // to an older thread, so a reader marking read up to the page's newest id
    // kept the badge on a channel they were looking at (Ethan, 2026-10-28).
    let latest_id = latest_post_id(&conn, &key);
    let include_owner = owner_has_posted(&conn) || matches!(&me, Who::Person { email, .. } if email == "owner");
    let all = people(&conn, include_owner);
    let ppl: Vec<Value> = readers(&conv, &all, &lanes)
        .iter()
        .map(|p| json!({"key": format!("person:{}", p.email), "name": p.name, "online": online(&format!("person:{}", p.email))}))
        .collect();
    let mut mentionable = mentionable_workers(&me, &conv, &lanes);
    mentionable.extend(mentionable_isolated(&me, &conv, &isolated_lanes()));
    Json(json!({"conv": key, "posts": rows, "last_read": last_read, "latest_id": latest_id, "thread": thread, "has_more": has_more && thread.is_none(),
        "profiles": profiles,
        "people": ppl, "workers": workers_in(&conv, &lanes),
        "mentionable_workers": mentionable,
        "typing": typing_now(&key, &me.key()), "measured": true}))
    .into_response()
}

async fn mentions(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let lanes = fleet(&state);
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
    let lanes = fleet(&state);
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
    // `in:<conv or #group>` and `from:<name>` narrow it, as in Slack.
    let mut in_conv: Option<String> = None;
    let mut from: Option<String> = None;
    for w in q.split_whitespace() {
        if let Some(v) = w.strip_prefix("in:") {
            let v = v.trim_start_matches('#');
            in_conv = Some(if v.contains(':') { v.to_string() } else { format!("g:{}", v.to_lowercase()) });
        } else if let Some(v) = w.strip_prefix("from:") {
            from = Some(v.trim_start_matches('@').to_lowercase());
        }
    }
    let terms = fts_query(&q);
    let sql = format!(
        "SELECT {cols}, snippet(conversation_posts_fts, 0, '[[', ']]', '…', 16) FROM conversation_posts_fts f
         JOIN conversation_posts p ON p.id = f.rowid
         WHERE conversation_posts_fts MATCH ?1 AND p.deleted_at IS NULL ORDER BY rank LIMIT 500",
        cols = POST_COLS.split(", ").map(|c| format!("p.{c}")).collect::<Vec<_>>().join(", "));
    let rows: Vec<Value> = if terms.is_empty() {
        // Only filters: newest matching posts.
        conn.prepare(&format!("SELECT {POST_COLS}, '' FROM conversation_posts WHERE deleted_at IS NULL ORDER BY id DESC LIMIT 500"))
            .and_then(|mut s| s.query_map([], |r| { let mut v = post_row(r)?; v["snippet"] = json!(""); Ok(v) })?.collect())
            .unwrap_or_default()
    } else {
        conn.prepare(&sql)
            .and_then(|mut s| s.query_map([terms.clone()], |r| { let mut v = post_row(r)?; v["snippet"] = json!(r.get::<_, String>(13)?); Ok(v) })?.collect())
            .unwrap_or_default()
    };
    let considered = rows.len();
    let rows: Vec<Value> = rows
        .into_iter()
        .filter(|p| p["conv"].as_str().and_then(Conv::parse).is_some_and(|c| may_read(&me, &c, &lanes)))
        .filter(|p| in_conv.as_deref().is_none_or(|c| p["conv"] == c))
        .filter(|p| from.as_deref().is_none_or(|f| p["author_name"].as_str().unwrap_or("").to_lowercase().contains(f)
            || p["author_key"].as_str().unwrap_or("").to_lowercase().ends_with(f)))
        .take(100)
        .collect();
    Json(json!({"posts": rows, "measured": true, "n_considered": considered})).into_response()
}

// ---- writing -------------------------------------------------------------

/// `--thread 4`, `--thread 12 --stdin`: a message that is nothing but the
/// flags of `amux post`, which only a CLI that does not parse them sends.
fn looks_like_cli_flags(body: &str) -> bool {
    let mut w = body.split_whitespace().peekable();
    if w.peek() != Some(&"--thread") {
        return false;
    }
    w.next();
    w.next().is_some_and(|n| n.parse::<i64>().is_ok()) && w.all(|t| t == "--stdin" || t == "-")
}

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
        // A direct message: the answer goes in the thread under the post that
        // asked, where both people read it (Ethan, 2026-10-28: "@ workers to
        // get them to do work ... and it does the thread shit inline").
        Conv::People(..) => (format!("{author} mentioned you in a direct message (amux Messages):"), "that message's thread".to_string()),
    };
    // WHO SENT THIS, said outright. Twice on 2026-10-28 a worker read a
    // delivery as "text you pasted with nothing from you asking me to act" and
    // posted a refusal in the thread instead of doing the task (mtp-demo from
    // a DM, workflows from #wexus, the second from Ethan himself).
    format!(
        "{opening}\n\n{body}\n\nNobody pasted this. amux delivered {author}'s post to you because it mentions you, \
         and {author} is waiting for your answer there. Treat it as {author}'s own request to you: do the work, \
         within your rules, and ask any question you need in the same place. Your answer must be posted to {place}, \
         because that is where {author} is reading (this pane is not). Post it with:\n\
         amux post '{}' <<'EOF'\n<your answer>\nEOF",
        conv.key()
    )
}

/// What one mentioned worker receives, and where its answer will be: `here`
/// (the conversation; in a channel or a direct message, the thread under the
/// post that asked) or `pane` (an isolated worker: the owner's words only, no injected
/// instructions, so its answer stays in its terminal).
fn delivery_for(conv: &Conv, author: &str, body: &str, isolated: bool) -> (String, &'static str) {
    if isolated {
        let place = match conv {
            Conv::Group(g) => format!("#{g}"),
            _ => "a direct message".to_string(),
        };
        return (format!("{author} (amux Messages, {place}): {body}"), "pane");
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
    let lanes = fleet(&state);
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
    let thread_hint = serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| v.get("parent_id").and_then(Value::as_i64))
        .or_else(|| headers.get("x-amux-thread").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok()));
    let invited = match (&me, thread_hint) {
        (Who::Worker { name, .. }, Some(t)) => state.store.read().is_ok_and(|c| worker_invited(&c, &conv, t, name)),
        _ => false,
    };
    if !may_read(&me, &conv, &lanes) && !invited {
        if let (Who::Worker { .. }, Conv::People(..)) = (&me, &conv) {
            return err(StatusCode::FORBIDDEN, "a worker posts in two people's direct message only in a thread where a person \
                @mentioned it (amux post 'p:a|b' --thread <id>); to write to one person use p:<email>, which goes in your thread");
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
    // An `amux` CLI older than `post --thread` sends its flags as the message,
    // and the chat bridge would forward "--thread 4" to WhatsApp (2026-10-27).
    if looks_like_cli_flags(&body) {
        tracing::warn!(author = %me.key(), body = %body, measured = true, n_considered = 1, verdict = "conversation_post_stale_cli",
            "a post whose whole body is CLI flags was refused: the sender's amux CLI predates `post --thread`");
        return err(StatusCode::BAD_REQUEST,
            "the message is only CLI flags: this amux CLI predates `amux post --thread`; run `make install-cli` and post again");
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
    let hdrs = send_headers(&me, &headers);
    let json_body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let parent = json_body.get("parent_id").and_then(Value::as_i64)
        .or_else(|| headers.get("x-amux-thread").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok()));
    let attachments = json_body.get("attachments").cloned().filter(Value::is_array).unwrap_or(json!([]));
    match post_inner(&state, conv, &me, me.key(), author_name, body, hdrs, None, parent, attachments, dm_to, None).await {
        Ok(v) => (StatusCode::CREATED, Json(v)).into_response(),
        Err(r) => *r,
    }
}

/// A post into a conversation, from a person, a worker, or the chat bridge on
/// someone's behalf (`author_key` and `author_name` then name the external
/// sender, and `note` goes first in what a worker receives, e.g. that the
/// sender is not the owner). Stores it, delivers it to the workers it
/// reaches, and records each outcome on the post. Returns the created post.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn post_inner(
    state: &AppState,
    conv: Conv,
    me: &Who,
    author_key: String,
    author_name: String,
    body: String,
    hdrs: HeaderMap,
    note: Option<String>,
    parent: Option<i64>,
    attachments: Value,
    // A worker's `p:<person>`: that person, @mentioned in the worker's thread.
    dm_to: Option<String>,
    // Written in the same transaction as the post (the chat bridge records
    // which chat a post came from, so the post is never echoed back to it).
    also: Option<AlsoWrite>,
) -> Result<Value, Box<Response>> {
    let lanes = fleet(state);
    // A reply belongs to its thread's ROOT, in this same conversation.
    let parent: Option<i64> = match parent {
        None => None,
        Some(pid) => {
            let root = state.store.read().ok().and_then(|c| c.query_row(
                "SELECT COALESCE(parent_id, id) FROM conversation_posts WHERE id=?1 AND conv=?2",
                rusqlite::params![pid, conv.key()], |r| r.get::<_, i64>(0)).ok());
            match root {
                Some(r) => Some(r),
                None => return Err(Box::new(err(StatusCode::BAD_REQUEST, "that thread is not in this conversation"))),
            }
        }
    };
    let all = state.store.read().map(|c| people(&c, true)).unwrap_or_default();
    let iso_lanes = isolated_lanes();
    let iso = mentionable_isolated(me, &conv, &iso_lanes);
    let mut workers = mentionable_workers(me, &conv, &lanes);
    workers.extend(iso.iter().cloned());
    let thread_readers = readers(&conv, &all, &lanes);
    let mut mentions = resolve_mentions(&body, &workers, &thread_readers);
    // An @name that is a worker this poster cannot reach used to vanish: the
    // post landed as plain text and nobody was told. Name it in the reply and
    // the log instead.
    let unreachable: Vec<String> = mention_tokens(&body).into_iter()
        .filter(|t| !workers.iter().any(|w| w.to_lowercase() == *t))
        .filter(|t| lanes.iter().chain(iso_lanes.iter()).any(|l| l.name.to_lowercase() == *t))
        .collect();
    if !unreachable.is_empty() {
        tracing::warn!(conv = %conv.key(), author = %author_key, workers = ?unreachable, measured = true,
            n_considered = unreachable.len(), verdict = "conversation_mention_unreachable",
            "a post @mentioned workers this poster cannot reach here; they were not sent anything");
    }
    if let Some(to) = &dm_to {
        let Some(p) = find_person(&all, to) else {
            return Err(Box::new(err(StatusCode::BAD_REQUEST, format!("no person '{to}' in this workspace"))));
        };
        if !thread_readers.iter().any(|r| r.email == p.email) {
            tracing::warn!(conv = %conv.key(), to = %p.email, verdict = "conversation_worker_dm_unreadable",
                "a worker's message to a person was refused: they cannot read the worker's thread");
            return Err(Box::new(err(StatusCode::FORBIDDEN, format!(
                "{} cannot read {}'s thread, so the message would not reach them; the owner can widen their access",
                p.email, conv.key()))));
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
    // A reply reaches every worker already in the thread (it was mentioned in
    // it, or posted in it), as a Slack thread does: no need to @ it again.
    if let Some(root) = parent {
        let inthread: Vec<String> = state.store.read().ok().and_then(|c| {
            let mut q = c.prepare("SELECT author_key, mentions FROM conversation_posts WHERE id=?1 OR parent_id=?1").ok()?;
            let rows = q.query_map(rusqlite::params![root], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).ok()?;
            let mut out = Vec::new();
            for (a, m) in rows.flatten() {
                if let Some(w) = a.strip_prefix("worker:") { out.push(w.to_string()); }
                for k in serde_json::from_str::<Vec<String>>(&m).unwrap_or_default() {
                    if let Some(w) = k.strip_prefix("worker:") { out.push(w.to_string()); }
                }
            }
            Some(out)
        }).unwrap_or_default();
        for w in inthread {
            if workers.contains(&w) && !targets.contains(&w) {
                targets.push(w);
            }
        }
    }
    if let Who::Worker { name, .. } = me {
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
    // What the channel said just before, so a mentioned worker answers in
    // context the way a person scrolling up would.
    let context = recent_context(state, &conv.key(), 8);
    let (key, ts) = (conv.key(), now_ms());
    let id_cell = Arc::new(Mutex::new(0i64));
    let att2 = attachments.to_string();
    let (k2, a2, n2, b2, m2, id2) = (key.clone(), author_key.clone(), author_name.clone(), body.clone(),
        serde_json::to_string(&mentions).unwrap_or_else(|_| "[]".into()), id_cell.clone());
    let wrote = state
        .store
        .write_async(move |c| {
            c.execute(
                "INSERT INTO conversation_posts (conv, author_key, author_name, body, mentions, ts, parent_id, attachments) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                rusqlite::params![k2, a2, n2, b2, m2, ts, parent, att2],
            )?;
            let id = c.last_insert_rowid();
            if let Some(f) = also {
                f(c, id)?;
            }
            // Your own post is read by you.
            c.execute(
                "INSERT INTO conversation_reads (reader, conv, last_id) VALUES (?1,?2,?3) \
                 ON CONFLICT(reader, conv) DO UPDATE SET last_id=MAX(last_id, excluded.last_id)",
                rusqlite::params![a2, k2, id],
            )?;
            if let Ok(mut g) = id2.lock() {
                *g = id;
            }
            let evk = k2.clone();
            Ok(crate::db::WriteOutcome { applied: true, events: vec![post_event(&evk, id, parent, "created")] })
        })
        .await;
    if let Err(e) = wrote {
        return Err(Box::new(err(StatusCode::SERVICE_UNAVAILABLE, format!("could not store the message: {e}"))));
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
        let (st, conv2, author, text_body) = (state.clone(), conv.clone(), author_name.clone(), body.clone());
        let channel_workers = workers_in(&conv, &lanes);
        tokio::spawn(async move {
            let mut outcomes = Vec::new();
            for w in targets {
                let isolated = iso.contains(&w);
                let (mut text, reply) = delivery_for(&conv2, &author, &text_body, isolated);
                // In a channel, the answer goes in a thread under the post that
                // asked (Slack's way); a reply stays in its thread.
                let thread_root = parent.or(if matches!(conv2, Conv::Group(_) | Conv::People(..)) { Some(id) } else { None });
                if let (Some(root), false) = (thread_root, isolated) {
                    let k = conv2.key();
                    text = text.replace(&format!("amux post '{k}' <<'EOF'"), &format!("amux post '{k}' --thread {root} <<'EOF'"));
                }
                // A channel's recent messages go only to workers who could read
                // the channel anyway (its group's, or anyone in #general); a
                // worker brought in from another group sees just the post. In a
                // DM the poster chose to bring the worker in.
                let member = match &conv2 {
                    Conv::Group(g) => g == GENERAL || channel_workers.contains(&w),
                    _ => true,
                };
                if !isolated && member && !context.is_empty() {
                    text = format!("Recent messages in {} before this one:\n{}\n\n{text}", conv2.key(), context.join("\n"));
                }
                if let Some(n) = &note {
                    text = format!("{n}\n\n{text}");
                }
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
    crate::bridge::notify_outbound();
    notify_owner(state, &conv, &author_key, &author_name, &body, id, parent, &mentions).await;
    Ok(json!({"ok": true, "id": id, "conv": key, "parent_id": parent, "author": author_key, "author_name": author_name, "mentions": mentions,
        "delivering_to": mentions.iter().filter(|m| m.starts_with("worker:")).count(), "unreachable": unreachable}))
}

/// A write made in the same transaction as a post, given its id.
pub(crate) type AlsoWrite = Box<dyn FnOnce(&rusqlite::Connection, i64) -> rusqlite::Result<()> + Send>;

/// The conversation's last `n` posts, oldest first, as `- Name: text` lines.
fn recent_context(state: &AppState, conv: &str, n: usize) -> Vec<String> {
    let Ok(c) = state.store.read() else { return vec![] };
    let Ok(mut q) = c.prepare("SELECT author_name, body FROM conversation_posts WHERE conv=?1 ORDER BY id DESC LIMIT ?2") else { return vec![] };
    let rows = q.query_map(rusqlite::params![conv, n as i64], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)));
    let mut out: Vec<String> = rows.into_iter().flatten().flatten()
        .map(|(a, b)| format!("- {a}: {}", b.chars().take(500).collect::<String>().replace('\n', " ")))
        .collect();
    out.reverse();
    out
}

async fn mark_read(
    State(state): State<AppState>,
    Path(conv): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let lanes = fleet(&state);
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
    let lanes = fleet(&state);
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

/// The conversation a delivered prompt came from: every delivery tells the
/// worker `amux post <conv>`, so the key is read back from that line.
fn conv_of_prompt(prompt: &str) -> Option<(String, Option<i64>)> {
    let i = prompt.rfind("amux post ")?;
    let rest = &prompt[i + "amux post ".len()..];
    let key: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    let key = key.trim_matches('\'');
    let thread = rest.split_once("--thread ").and_then(|(_, t)| t.split_whitespace().next()).and_then(|t| t.parse().ok());
    Conv::parse(key).map(|c| (c.key(), thread))
}

/// From the turn-end hook: a worker whose turn began with a Messages post and
/// ended without posting its answer gets that turn's final text posted for
/// it, so whoever wrote sees the answer where they wrote (the worker's
/// terminal is not where they are reading). A worker that posted during the
/// turn is left alone.
pub(crate) async fn relay_turn(state: &AppState, worker: &str, prompt: &str, final_text: &str) {
    let text = final_text.trim();
    if text.is_empty() || !prompt.contains("amux Messages") {
        return;
    }
    let Some((key, thread)) = conv_of_prompt(prompt) else { return };
    let author = format!("worker:{worker}");
    let mention = format!("%\"worker:{worker}\"%");
    let already = state.store.read().ok().and_then(|c| c.query_row(
        "SELECT 1 FROM conversation_posts WHERE conv=?1 AND author_key=?2 AND id > COALESCE(
           (SELECT MAX(id) FROM conversation_posts WHERE conv=?1 AND (mentions LIKE ?3 OR conv=?4)), 0) LIMIT 1",
        rusqlite::params![key, author, mention, format!("w:{worker}")], |_| Ok(())).ok()).is_some();
    if already {
        tracing::debug!(conv = %key, worker, verdict = "conversation_turn_already_posted", "turn-end: the worker posted its answer itself");
        return;
    }
    let lanes = fleet(state);
    let Some(lane) = lanes.iter().find(|l| l.name == worker) else { return };
    let me = Who::Worker { name: lane.name.clone(), groups: lane.groups.clone() };
    let Some(conv) = Conv::parse(&key) else { return };
    let mut hdrs = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(worker) {
        hdrs.insert("x-amux-session", v);
    }
    let body: String = text.chars().take(MAX_BODY).collect();
    match post_inner(state, conv, &me, author, worker.to_string(), body, hdrs, None, thread, json!([]), None, None).await {
        Ok(v) => tracing::info!(conv = %key, worker, post = %v["id"], measured = true, n_considered = 1,
            verdict = "conversation_turn_relayed", "turn-end: a worker's answer was posted where it was asked"),
        Err(_) => tracing::warn!(conv = %key, worker, verdict = "conversation_turn_relay_failed",
            "turn-end: a worker's answer could not be posted to the conversation"),
    }
}

/// The chat bridge's `/link <target>`: a worker's thread (`w:<worker>`) or a
/// group's channel (`g:<group>`, `general` included). An existing key passes
/// through. Returns the key and a label for the confirmation.
pub(crate) fn resolve_link_target(target: &str) -> Option<(String, String)> {
    let t = target.trim().trim_start_matches('@').trim_start_matches('#');
    if let Some(c) = Conv::parse(t) {
        if !matches!(c, Conv::People(..)) {
            return Some((c.key(), c.key()));
        }
    }
    let lanes = lanes();
    if lanes.iter().any(|l| l.name == t) {
        return Some((format!("w:{t}"), format!("worker {t}")));
    }
    let g = t.to_lowercase();
    if g == GENERAL || lanes.iter().any(|l| l.groups.contains(&g)) {
        let members: Vec<String> = lanes.iter().filter(|l| l.groups.contains(&g) || g == GENERAL).map(|l| format!("@{}", l.name)).collect();
        return Some((format!("g:{g}"), format!("#{g} ({})", members.join(", "))));
    }
    None
}

/// Every worker Messages can route to: (name, groups, description). The
/// Recorder files a meeting with the workers it concerns from this list.
pub(crate) fn fleet_directory() -> Vec<(String, Vec<String>, String)> {
    let home = super::groups::amux_home();
    lanes().into_iter().map(|l| {
        let env = crate::config::parse_env_file(&home.join("sessions").join(format!("{}.env", l.name)));
        let desc = env.get("CC_DESC").map(|v| v.trim().trim_matches('"').to_string()).unwrap_or_default();
        (l.name, l.groups.into_iter().collect(), desc)
    }).collect()
}

/// Every worker, isolated ones too: (name, groups, description, isolated).
/// A recording can be sent to any of them; the owner's send reaches all.
pub(crate) fn fleet_directory_all() -> Vec<(String, Vec<String>, String, bool)> {
    let home = super::groups::amux_home();
    let desc = |n: &str| crate::config::parse_env_file(&home.join("sessions").join(format!("{n}.env")))
        .get("CC_DESC").map(|v| v.trim().trim_matches('"').to_string()).unwrap_or_default();
    let mut out: Vec<(String, Vec<String>, String, bool)> = lanes().into_iter()
        .map(|l| { let d = desc(&l.name); (l.name, l.groups.into_iter().collect(), d, false) }).collect();
    out.extend(isolated_lanes().into_iter().map(|l| { let d = desc(&l.name); (l.name, l.groups.into_iter().collect(), d, true) }));
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The owner's post into a worker's own thread (the Recorder sending a
/// recording the owner tagged it on). The worker answers in the thread.
pub(crate) async fn owner_post_to_worker(state: &AppState, worker: &str, body: &str) -> Result<String, String> {
    let me = Who::Person { email: "owner".into(), scope: MemberScope::Global };
    let name = display_for("owner", "");
    post_inner(state, Conv::Worker(worker.to_string()), &me, me.key(), name, body.to_string(), HeaderMap::new(),
        None, None, json!([]), None, None).await
        .map(|v| format!("posted in w:{worker} (post {})", v["id"]))
        .map_err(|_| format!("the post into {worker}'s thread was refused"))
}

/// The owner's direct send to a worker Messages has no route to (isolated).
pub(crate) async fn owner_send_direct(state: &AppState, worker: &str, body: &str) -> Result<String, String> {
    let resp = super::session_verbs::send_post(state, worker, &HeaderMap::new(), &json!({"text": body, "record_history": true})).await;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap_or_default();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    if status.is_success() && v["ok"].as_bool().unwrap_or(false) {
        Ok(format!("sent directly ({})", v["submission"].as_str().unwrap_or("accepted")))
    } else {
        Err(format!("HTTP {}: {}", status.as_u16(), v["error"].as_str().unwrap_or("not accepted")))
    }
}

/// A post into a group's channel by the Recorder (recording_live.rs): its own
/// author, never the owner's name, and no @mention, so it wakes no worker.
pub(crate) async fn recorder_post(state: &AppState, group: &str, body: &str) -> Result<(), String> {
    let conv = Conv::Group(group.to_string());
    let me = Who::Person { email: "owner".into(), scope: MemberScope::Global };
    post_inner(state, conv, &me, "recorder:amux".into(), "Recorder".into(), body.replace('@', ""), HeaderMap::new(),
        None, None, json!([]), None, None).await.map(|_| ()).map_err(|_| format!("the post into #{group} was refused"))
}

/// The chat bridge posting a platform message into a conversation. It posts
/// with the owner's reach (the owner linked the conversation); `author_key`
/// and `author_name` name the actual sender, and `note` (for anyone who is
/// not the owner) is what the worker reads first.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn bridge_post(state: &AppState, conv_key: &str, author_key: &str, author_name: &str, body: &str,
    note: Option<String>, record: AlsoWrite) -> Result<Value, String> {
    let conv = Conv::parse(conv_key).ok_or_else(|| format!("not a conversation: {conv_key}"))?;
    let me = Who::Person { email: "owner".into(), scope: MemberScope::Global };
    let body: String = body.chars().take(MAX_BODY).collect();
    post_inner(state, conv, &me, author_key.to_string(), author_name.to_string(), body, HeaderMap::new(), note, None, json!([]), None, Some(record)).await
        .map_err(|_| "the post was not stored".to_string())
}


// ---- live events and notifications -------------------------------------------

/// The SSE state event for a post: which conversation, which post, what
/// happened. No body: every connected client receives it, and the post
/// itself is fetched through the read-checked route.
fn post_event(conv: &str, id: i64, parent: Option<i64>, op: &str) -> crate::db::PendingEvent {
    crate::db::PendingEvent {
        entity_type: amux_core::revision::EntityType::Other("conversation".into()),
        // The SSE StateEvent carries entity_id and not the payload, so the
        // client's whole need (which conversation, which post, which thread,
        // what happened) rides here as JSON. Still no body.
        entity_id: json!({"conv": conv, "id": id, "parent_id": parent, "op": op}).to_string(),
        mutation: match op {
            "created" => amux_core::revision::MutationKind::Created,
            "deleted" => amux_core::revision::MutationKind::Deleted,
            _ => amux_core::revision::MutationKind::Updated,
        },
        payload: Some(json!({"conv": conv, "id": id, "parent_id": parent, "op": op})),
    }
}

/// Should this post reach the owner as a notification? A post written by
/// someone else that is addressed to the owner: in a worker's thread (the
/// worker answering), a direct message, an @mention of the owner, or a reply
/// in a thread the owner posted in. Channel chatter is not.
fn notifies_owner(conn: &rusqlite::Connection, conv: &Conv, author_key: &str, mentions: &[String], parent: Option<i64>) -> bool {
    if author_key == "person:owner" {
        return false;
    }
    if mentions.iter().any(|m| m == "person:owner") {
        return true;
    }
    match conv {
        Conv::Worker(_) => return true,
        Conv::People(a, b) if a == "owner" || b == "owner" => return true,
        _ => {}
    }
    parent.is_some_and(|root| conn.query_row(
        "SELECT 1 FROM conversation_posts WHERE (id=?1 OR parent_id=?1) AND author_key='person:owner' LIMIT 1",
        rusqlite::params![root], |_| Ok(())).is_ok())
}

/// Web push to the owner's subscribed devices (`push::send_all`); the Mac
/// and iOS apps notify natively from the live event (amuxNotify bridge).
#[allow(clippy::too_many_arguments)]
async fn notify_owner(state: &AppState, conv: &Conv, author_key: &str, author_name: &str, body: &str, id: i64,
    parent: Option<i64>, mentions: &[String]) {
    let yes = state.store.read().map(|c| notifies_owner(&c, conv, author_key, mentions, parent)).unwrap_or(false);
    if !yes {
        return;
    }
    let place = match conv {
        Conv::Group(g) => format!("#{g}"),
        Conv::Worker(w) => w.clone(),
        Conv::People(..) => "a direct message".into(),
    };
    let title = if parent.is_some() { format!("{author_name} replied in {place}") } else { format!("{author_name} in {place}") };
    let snippet: String = body.chars().take(180).collect();
    let url = format!("/#messages={}&post={}", conv.key(), parent.unwrap_or(id));
    let sent = crate::push::send_all(state, &title, &snippet, "", &format!("conv-{}", conv.key()), &url).await;
    tracing::info!(conv = %conv.key(), post = id, devices = sent.len(), measured = true, n_considered = 1,
        verdict = "conversation_owner_notified", "Messages: the owner was notified of a post addressed to them");
}

// ---- edit, delete, react --------------------------------------------------------

fn author_of(conn: &rusqlite::Connection, id: i64) -> Option<(String, String, Option<i64>)> {
    conn.query_row("SELECT author_key, conv, parent_id FROM conversation_posts WHERE id=?1 AND deleted_at IS NULL",
        rusqlite::params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).ok()
}

/// Only the author edits or deletes a post; the owner may delete any.
async fn edit_post(State(state): State<AppState>, Path(id): Path<i64>, headers: HeaderMap, bytes: Bytes) -> Response {
    let lanes = lanes();
    let me = match identify(&headers, &lanes) {
        Ok(w) => w,
        Err(m) => return err(StatusCode::FORBIDDEN, m),
    };
    let Some((author, conv, parent)) = state.store.read().ok().and_then(|c| author_of(&c, id)) else {
        return err(StatusCode::NOT_FOUND, "no such post");
    };
    if author != me.key() {
        return err(StatusCode::FORBIDDEN, "only its author can edit a post");
    }
    let body = body_text(&headers, &bytes);
    if body.is_empty() || body.chars().count() > MAX_BODY {
        return err(StatusCode::BAD_REQUEST, "a post needs 1 to 8000 characters");
    }
    let (b, c2) = (body.clone(), conv.clone());
    let r = state.store.write_async(move |c| {
        c.execute("UPDATE conversation_posts SET body=?1, edited_at=?2 WHERE id=?3", rusqlite::params![b, now_ms(), id])?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![post_event(&c2, id, parent, "edited")] })
    }).await;
    match r {
        Ok(_) => {
            tracing::info!(post = id, conv = %conv, verdict = "conversation_post_edited", "Messages: a post was edited");
            crate::bridge::notify_outbound();
            Json(json!({"ok": true, "id": id, "body": body})).into_response()
        }
        Err(e) => err(StatusCode::SERVICE_UNAVAILABLE, e.to_string()),
    }
}

async fn delete_post(State(state): State<AppState>, Path(id): Path<i64>, headers: HeaderMap) -> Response {
    let lanes = lanes();
    let me = match identify(&headers, &lanes) {
        Ok(w) => w,
        Err(m) => return err(StatusCode::FORBIDDEN, m),
    };
    let Some((author, conv, parent)) = state.store.read().ok().and_then(|c| author_of(&c, id)) else {
        return err(StatusCode::NOT_FOUND, "no such post");
    };
    let owner = matches!(&me, Who::Person { email, .. } if email == "owner");
    if author != me.key() && !owner {
        return err(StatusCode::FORBIDDEN, "only its author or the owner can delete a post");
    }
    let c2 = conv.clone();
    let r = state.store.write_async(move |c| {
        // The words go; the row stays, so its thread and read markers hold.
        c.execute("UPDATE conversation_posts SET body='', attachments='[]', deleted_at=?1 WHERE id=?2", rusqlite::params![now_ms(), id])?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![post_event(&c2, id, parent, "deleted")] })
    }).await;
    match r {
        Ok(_) => {
            tracing::info!(post = id, conv = %conv, by = %me.key(), verdict = "conversation_post_deleted", "Messages: a post was deleted");
            Json(json!({"ok": true, "id": id, "deleted": true})).into_response()
        }
        Err(e) => err(StatusCode::SERVICE_UNAVAILABLE, e.to_string()),
    }
}

/// Toggle the caller's reaction (one emoji) on a post.
async fn react_post(State(state): State<AppState>, Path(id): Path<i64>, headers: HeaderMap, Json(b): Json<Value>) -> Response {
    let lanes = fleet(&state);
    let me = match identify(&headers, &lanes) {
        Ok(w) => w,
        Err(m) => return err(StatusCode::FORBIDDEN, m),
    };
    let emoji: String = b["emoji"].as_str().unwrap_or("").trim().chars().take(16).collect();
    if emoji.is_empty() {
        return err(StatusCode::BAD_REQUEST, "emoji is required");
    }
    let Some((_, conv, parent)) = state.store.read().ok().and_then(|c| author_of(&c, id)) else {
        return err(StatusCode::NOT_FOUND, "no such post");
    };
    let Some(cv) = Conv::parse(&conv) else { return err(StatusCode::NOT_FOUND, "no such post") };
    if !may_read(&me, &cv, &lanes) {
        return err(StatusCode::FORBIDDEN, "not a conversation you can read");
    }
    let (key, c2) = (me.key(), conv.clone());
    let out = Arc::new(Mutex::new(json!({})));
    let o2 = out.clone();
    let r = state.store.write_async(move |c| {
        let raw: String = c.query_row("SELECT reactions FROM conversation_posts WHERE id=?1", rusqlite::params![id], |r| r.get(0))?;
        let mut m: serde_json::Map<String, Value> = serde_json::from_str(&raw).unwrap_or_default();
        let list = m.entry(emoji.clone()).or_insert_with(|| json!([]));
        let mut who: Vec<String> = serde_json::from_value(list.clone()).unwrap_or_default();
        if let Some(i) = who.iter().position(|k| k == &key) { who.remove(i); } else { who.push(key.clone()); }
        if who.is_empty() { m.remove(&emoji); } else { m.insert(emoji.clone(), json!(who)); }
        let js = Value::Object(m);
        c.execute("UPDATE conversation_posts SET reactions=?1 WHERE id=?2", rusqlite::params![js.to_string(), id])?;
        if let Ok(mut g) = o2.lock() { *g = js; }
        Ok(crate::db::WriteOutcome { applied: true, events: vec![post_event(&c2, id, parent, "reacted")] })
    }).await;
    match r {
        Ok(_) => Json(json!({"ok": true, "id": id, "reactions": out.lock().map(|g| g.clone()).unwrap_or(json!({}))})).into_response(),
        Err(e) => err(StatusCode::SERVICE_UNAVAILABLE, e.to_string()),
    }
}

// ---- search ---------------------------------------------------------------------

/// The person's words as an FTS5 query: each word a quoted prefix term, so
/// punctuation can never be read as query syntax.
fn fts_query(q: &str) -> String {
    q.split_whitespace()
        .filter(|w| !w.contains(':'))
        .map(|w| w.chars().filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '@')).collect::<String>())
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{}\"*", w.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---- link previews --------------------------------------------------------------

/// An address on the public internet. Loopback, private, link-local, CGNAT,
/// multicast and reserved are not, and neither is an IPv6 address that only
/// wraps one of those (::ffff:127.0.0.1, ::10.0.0.1, 64:ff9b::a9fe:a9fe).
fn public_ip(ip: std::net::IpAddr) -> bool {
    use std::net::{IpAddr, Ipv4Addr};
    let v4 = |v: Ipv4Addr| {
        let o = v.octets();
        !(v.is_loopback() || v.is_private() || v.is_link_local() || v.is_unspecified() || v.is_broadcast()
            || v.is_multicast() || v.is_documentation() || o[0] == 0 || o[0] >= 240
            || (o[0] == 100 && (64..128).contains(&o[1])) || (o[0] == 192 && o[1] == 0 && o[2] == 0)
            || (o[0] == 198 && (o[1] == 18 || o[1] == 19)))
    };
    match ip {
        IpAddr::V4(v) => v4(v),
        IpAddr::V6(v) => {
            let s = v.segments();
            if let Some(m) = v.to_ipv4_mapped() {
                return v4(m);
            }
            // IPv4-compatible (::a.b.c.d) and NAT64 (64:ff9b::/96) carry a v4 inside.
            if s[..6] == [0; 6] || (s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0; 4]) {
                return !(s[6] == 0 && s[7] <= 1) && v4(Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8));
            }
            !(v.is_loopback() || v.is_unspecified() || v.is_multicast() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
                || (s[0] == 0x2001 && s[1] == 0x0db8))
        }
    }
}

/// The one address a preview fetch may connect to for `host`, or None when
/// any of its addresses is not public. The fetch is pinned to it, so a second
/// DNS answer cannot swap in a private address (rebinding).
fn public_addr(host: &str, port: u16) -> Option<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    let addrs: Vec<_> = (host.trim_start_matches('[').trim_end_matches(']'), port).to_socket_addrs().ok()?.collect();
    (!addrs.is_empty() && addrs.iter().all(|a| public_ip(a.ip()))).then(|| addrs[0])
}

fn meta(html: &str, names: &[&str]) -> Option<String> {
    for n in names {
        for pat in [format!("property=\"{n}\""), format!("name=\"{n}\""), format!("property='{n}'"), format!("name='{n}'")] {
            let Some(i) = html.find(&pat) else { continue };
            let tag_start = html[..i].rfind('<').unwrap_or(i);
            let tag_end = html[i..].find('>').map(|e| i + e).unwrap_or(html.len());
            let tag = &html[tag_start..tag_end];
            if let Some(c) = tag.find("content=") {
                // Quoted, unquoted or empty (`content=` at the end of a tag).
                let mut rest = tag[c + 8..].chars().peekable();
                let v: String = match rest.peek().copied() {
                    Some(q @ ('"' | '\'')) => rest.skip(1).take_while(|ch| *ch != q).collect(),
                    Some(_) => rest.take_while(|ch| !ch.is_whitespace() && *ch != '/').collect(),
                    None => String::new(),
                };
                let v = v.replace("&amp;", "&").replace("&quot;", "\"").replace("&#39;", "'").trim().to_string();
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}

async fn unfurl(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    let q = qs(&raw);
    let url = q.get("url").cloned().unwrap_or_default();
    let Ok(parsed) = reqwest::Url::parse(&url) else { return err(StatusCode::BAD_REQUEST, "not a URL") };
    if !matches!(parsed.scheme(), "http" | "https") {
        return err(StatusCode::BAD_REQUEST, "only http and https links unfurl");
    }
    let day = 24 * 3600 * 1000;
    if let Some(v) = state.store.read().ok().and_then(|c| c.query_row(
        "SELECT status, title, description, image, site, fetched_at FROM link_previews WHERE url=?1", rusqlite::params![url],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?, r.get::<_, i64>(5)?))).ok()) {
        if now_ms() - v.5 < day {
            return Json(json!({"url": url, "status": v.0, "title": v.1, "description": v.2, "image": v.3, "site": v.4, "cached": true})).into_response();
        }
    }
    let host = parsed.host_str().unwrap_or("").to_string();
    // Redirects are followed here, not by reqwest: every hop's host is
    // checked and the connection pinned to the address that was checked.
    let mut at = parsed.clone();
    let mut sent: Result<reqwest::Response, String> = Err("more than 3 redirects".into());
    for _hop in 0..4 {
        let h = at.host_str().unwrap_or("").to_string();
        let port = at.port_or_known_default().unwrap_or(443);
        let h2 = h.clone();
        let Some(addr) = tokio::task::spawn_blocking(move || public_addr(&h2, port)).await.ok().flatten() else {
            tracing::info!(host = %h, first_host = %host, verdict = "unfurl_refused_private",
                "Messages: a link (or a redirect it took) to a non-public address is not fetched for a preview");
            return Json(json!({"url": url, "status": 0, "refused": "not a public address"})).into_response();
        };
        let client = reqwest::Client::builder().timeout(Duration::from_secs(6)).redirect(reqwest::redirect::Policy::none())
            .resolve(h.trim_start_matches('[').trim_end_matches(']'), addr)
            .user_agent("amux-link-preview/1 (+https://amux.io)").build().unwrap_or_default();
        match client.get(at.clone()).send().await {
            Ok(r) if r.status().is_redirection() => {
                match r.headers().get("location").and_then(|v| v.to_str().ok()).and_then(|l| at.join(l).ok()) {
                    Some(n) if matches!(n.scheme(), "http" | "https") => at = n,
                    _ => { sent = Ok(r); break; }
                }
            }
            Ok(r) => { sent = Ok(r); break; }
            Err(e) => { sent = Err(e.to_string()); break; }
        }
    }
    let (status, html) = match sent {
        Ok(r) => {
            let st = r.status().as_u16() as i64;
            let ok_type = r.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|t| t.contains("html"));
            let mut body = Vec::new();
            let mut r = r;
            while ok_type && body.len() < 512 * 1024 {
                match r.chunk().await { Ok(Some(c)) => body.extend_from_slice(&c), _ => break }
            }
            (st, String::from_utf8_lossy(&body).to_string())
        }
        Err(e) => {
            tracing::info!(host = %host, error = %e, verdict = "unfurl_fetch_failed", "Messages: a link preview could not be fetched");
            (0, String::new())
        }
    };
    let title = meta(&html, &["og:title", "twitter:title"]).or_else(|| {
        let i = html.find("<title")?; let s = html[i..].find('>')? + i + 1; let e = html[s..].find("</title")? + s;
        Some(html[s..e].trim().to_string()).filter(|t| !t.is_empty())
    });
    let desc = meta(&html, &["og:description", "twitter:description", "description"]);
    let image = meta(&html, &["og:image", "twitter:image"]).and_then(|i| at.join(&i).ok()).map(|u| u.to_string());
    let site = meta(&html, &["og:site_name"]).or(Some(host.clone()));
    let (u2, t2, d2, i2, s2) = (url.clone(), title.clone(), desc.clone(), image.clone(), site.clone());
    let _ = state.store.write_async(move |c| {
        c.execute("INSERT OR REPLACE INTO link_previews (url, status, title, description, image, site, fetched_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![u2, status, t2, d2, i2, s2, now_ms()])?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await;
    Json(json!({"url": url, "status": status, "title": title, "description": desc, "image": image, "site": site, "cached": false})).into_response()
}

// ---- profile pictures -----------------------------------------------------------

/// Not `avatars/`: that folder is api/avatars.rs's, one file per email, and
/// it lists every file in it.
fn avatars_dir() -> std::path::PathBuf {
    crate::config::amux_home().join("profile-pictures")
}

/// Set a picture: the owner for anyone, a person or worker for itself.
async fn set_avatar(State(state): State<AppState>, Path(key): Path<String>, headers: HeaderMap, bytes: Bytes) -> Response {
    let lanes = lanes();
    let me = match identify(&headers, &lanes) {
        Ok(w) => w,
        Err(m) => return err(StatusCode::FORBIDDEN, m),
    };
    let owner = matches!(&me, Who::Person { email, .. } if email == "owner");
    if key != me.key() && !owner {
        return err(StatusCode::FORBIDDEN, "you can set only your own picture");
    }
    if !(key.starts_with("person:") || key.starts_with("worker:")) {
        return err(StatusCode::BAD_REQUEST, "a profile key is person:<email> or worker:<name>");
    }
    // A person with an email has one picture, kept by api/avatars.rs.
    if key.starts_with("person:") && key.contains('@') {
        return err(StatusCode::BAD_REQUEST, "a person's picture is set with PUT /api/avatars/<email>");
    }
    // The type from the bytes, never the header, so nothing but an image is served.
    let ext = match super::avatars::sniff(&bytes) {
        Some("image/png") => "png", Some("image/jpeg") => "jpg", Some("image/webp") => "webp", Some("image/gif") => "gif",
        _ => return err(StatusCode::UNSUPPORTED_MEDIA_TYPE, "a picture is PNG, JPEG, WebP or GIF"),
    };
    if bytes.is_empty() || bytes.len() > 2 * 1024 * 1024 {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "a picture is at most 2 MB");
    }
    let name = format!("{}.{ext}", &hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes))[..24]);
    let dir = avatars_dir();
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(&name), &bytes)) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    let (k2, n2) = (key.clone(), name.clone());
    let r = state.store.write_async(move |c| {
        c.execute("INSERT INTO profiles (key, avatar, updated_at) VALUES (?1,?2,?3) ON CONFLICT(key) DO UPDATE SET avatar=excluded.avatar, updated_at=excluded.updated_at",
            rusqlite::params![k2, n2, now_ms()])?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await;
    match r {
        Ok(_) => Json(json!({"ok": true, "key": key, "avatar": format!("/api/profiles/avatar/{name}")})).into_response(),
        Err(e) => err(StatusCode::SERVICE_UNAVAILABLE, e.to_string()),
    }
}

async fn get_avatar(Path(name): Path<String>) -> Response {
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') || name.contains("..") {
        return err(StatusCode::BAD_REQUEST, "bad picture name");
    }
    let Ok(bytes) = std::fs::read(avatars_dir().join(&name)) else { return err(StatusCode::NOT_FOUND, "no such picture") };
    let ct = match name.rsplit('.').next() { Some("png") => "image/png", Some("jpg") => "image/jpeg", Some("webp") => "image/webp", _ => "image/gif" };
    // Content-addressed by its hash: safe to cache for good.
    ([(axum::http::header::CONTENT_TYPE, ct), (axum::http::header::CACHE_CONTROL, "private, max-age=31536000, immutable"),
        (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff")], bytes).into_response()
}

async fn list_profiles(State(state): State<AppState>) -> Response {
    let rows: Vec<Value> = state.store.read().ok().and_then(|c| c.prepare("SELECT key, avatar, display_name FROM profiles").ok().and_then(|mut q| {
        q.query_map([], |r| Ok(json!({"key": r.get::<_, String>(0)?, "avatar": r.get::<_, Option<String>>(1)?.map(|a| format!("/api/profiles/avatar/{a}")),
            "display_name": r.get::<_, Option<String>>(2)?}))).ok().map(|it| it.flatten().collect())
    })).unwrap_or_default();
    Json(json!({"profiles": rows, "measured": true, "n_considered": rows.len()})).into_response()
}

// ---- channel settings (owner and admins only) -------------------------------
//
// Create a channel, rename it, make it private, archive it, and add or remove
// people. The owner and admin members (Settings > Workspace access, role
// admin) may; everyone else, workers included, gets a 403 with
// `code: "channel_admin_only"` and a WARN line with the same verdict.

const CHANNEL_NAME_MAX: usize = 40;

fn refuse(code: StatusCode, code_name: &str, msg: impl Into<String>) -> Response {
    (code, Json(json!({"ok": false, "error": msg.into(), "code": code_name}))).into_response()
}

/// The owner, or an admin member speaking as themselves. Returns who, for the
/// audit line. A request carrying a worker session is never an admin, even
/// with an admin's cookie (the vault's rule, vault_secrets.rs `admin_member`).
fn channel_admin(headers: &HeaderMap, lanes: &[Lane]) -> Result<String, String> {
    let me = identify(headers, lanes)?;
    match &me {
        Who::Person { email, .. } if email == "owner" && gateway_email(headers).is_none() => Ok("owner".into()),
        Who::Person { email, .. } => {
            let actor = org::local_member_actor(headers).unwrap_or("");
            let own = headers.get("x-amux-session").and_then(|v| v.to_str().ok()).is_none_or(|s| s.trim() == actor);
            if org::local_member_is_admin(headers) && own && !headers.contains_key("x-amux-worker") {
                Ok(format!("member:{email}"))
            } else {
                Err(format!("{} is not the owner or an admin; only they change channel settings", me.key()))
            }
        }
        Who::Worker { name, .. } => Err(format!("worker:{name} cannot change channel settings; ask the owner or an admin")),
    }
}

fn require_admin(headers: &HeaderMap, lanes: &[Lane], action: &str) -> Result<String, Box<Response>> {
    channel_admin(headers, lanes).map_err(|why| {
        tracing::warn!(action, why = %why, measured = true, n_considered = 1, verdict = "conversation_channel_admin_refused",
            "a channel settings change was refused: the caller is not the owner or an admin");
        Box::new(refuse(StatusCode::FORBIDDEN, "channel_admin_only", why))
    })
}

fn valid_channel_name(name: &str) -> bool {
    let mut cs = name.chars();
    matches!(cs.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.len() <= CHANNEL_NAME_MAX
        && cs.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Every channel that exists, archived ones included, sorted with #general first.
fn channel_names(f: &Fleet) -> Vec<String> {
    let mut g: BTreeSet<String> = f.lanes.iter().flat_map(|l| l.groups.iter().cloned()).collect();
    g.extend(f.ch.meta.iter().filter(|(_, m)| m.created).map(|(n, _)| n.clone()));
    g.remove(GENERAL);
    std::iter::once(GENERAL.to_string()).chain(g).collect()
}

fn channel_json(g: &str, f: &Fleet, all: &[Person]) -> Value {
    let conv = Conv::Group(g.to_string());
    let meta = f.ch.meta(g);
    // Readers as if it were open, so an archived channel still shows who it had.
    let open = Fleet { lanes: f.lanes.clone(), ch: Channels { meta: f.ch.meta.iter().map(|(k, m)| (k.clone(),
        ChannelMeta { archived: false, ..m.clone() })).collect(), members: f.ch.members.clone(), unreadable: f.ch.unreadable } };
    json!({"conv": conv.key(), "name": g, "title": meta.title, "private": meta.private, "archived": meta.archived,
        "custom": meta.created && !f.lanes.iter().any(|l| l.groups.contains(g)), "general": g == GENERAL,
        "people": readers(&conv, all, &open).len(), "workers": workers_in(&conv, f)})
}

fn channel_event(name: &str, op: &str) -> crate::db::PendingEvent {
    crate::db::PendingEvent {
        entity_type: amux_core::revision::EntityType::Other("conversation_channel".into()),
        entity_id: json!({"conv": format!("g:{name}"), "op": op}).to_string(),
        mutation: amux_core::revision::MutationKind::Updated,
        payload: Some(json!({"conv": format!("g:{name}"), "op": op})),
    }
}

async fn list_channels(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let f = fleet(&state);
    if let Err(r) = require_admin(&headers, &f, "list") {
        return *r;
    }
    let all = state.store.read().map(|c| people(&c, true)).unwrap_or_default();
    let names = channel_names(&f);
    let rows: Vec<Value> = names.iter().map(|g| channel_json(g, &f, &all)).collect();
    Json(json!({"channels": rows, "measured": !f.ch.unreadable, "n_considered": names.len(),
        "why_unmeasured": f.ch.unreadable.then_some("channel configuration could not be read")})).into_response()
}

async fn create_channel(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<Value>) -> Response {
    let f = fleet(&state);
    let actor = match require_admin(&headers, &f, "create") {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let name = b.get("name").and_then(Value::as_str).unwrap_or("").trim().trim_start_matches('#').to_lowercase();
    if !valid_channel_name(&name) {
        return refuse(StatusCode::BAD_REQUEST, "channel_name_invalid",
            format!("a channel name is 1 to {CHANNEL_NAME_MAX} lowercase letters, digits, - or _"));
    }
    if channel_exists(&name, &f) {
        let hint = if f.ch.meta(&name).archived { " (it is archived: restore it instead)" } else { "" };
        return refuse(StatusCode::CONFLICT, "channel_exists", format!("#{name} already exists{hint}"));
    }
    let title = b.get("title").and_then(Value::as_str).unwrap_or("").trim().chars().take(80).collect::<String>();
    let private = b.get("private").and_then(Value::as_bool).unwrap_or(false) as i64;
    let (n2, t2, now) = (name.clone(), title, chrono::Utc::now().timestamp());
    let wrote = state.store.write_async(move |c| {
        c.execute(
            "INSERT INTO group_config (name, updated, channel_title, channel_created_at, channel_private, channel_archived_at) \
             VALUES (?1, ?2, ?3, ?2, ?4, NULL) ON CONFLICT(name) DO UPDATE SET updated=?2, channel_title=?3, \
             channel_created_at=?2, channel_private=?4, channel_archived_at=NULL",
            rusqlite::params![n2, now, t2, private],
        )?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![channel_event(&n2, "created")] })
    }).await;
    if let Err(e) = wrote {
        return err(StatusCode::SERVICE_UNAVAILABLE, format!("could not store the channel: {e}"));
    }
    tracing::info!(channel = %name, actor = %actor, private = private != 0, measured = true, n_considered = 1,
        verdict = "conversation_channel_created", "a channel was created");
    (StatusCode::CREATED, Json(json!({"ok": true, "conv": format!("g:{name}"), "name": name}))).into_response()
}

async fn patch_channel(State(state): State<AppState>, Path(name): Path<String>, headers: HeaderMap, Json(b): Json<Value>) -> Response {
    let f = fleet(&state);
    let actor = match require_admin(&headers, &f, "update") {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let name = name.trim().trim_start_matches("g:").to_lowercase();
    if !channel_exists(&name, &f) {
        return refuse(StatusCode::NOT_FOUND, "channel_not_found", format!("no channel #{name}"));
    }
    let title = b.get("title").and_then(Value::as_str).map(|t| t.trim().chars().take(80).collect::<String>());
    let private = b.get("private").and_then(Value::as_bool);
    let archived = b.get("archived").and_then(Value::as_bool);
    if name == GENERAL && (archived == Some(true) || private == Some(true)) {
        return refuse(StatusCode::BAD_REQUEST, "general_is_permanent", "#general cannot be archived or made private");
    }
    if title.is_none() && private.is_none() && archived.is_none() {
        return refuse(StatusCode::BAD_REQUEST, "nothing_to_change", "send title, private or archived");
    }
    let (n2, now) = (name.clone(), chrono::Utc::now().timestamp());
    let (t2, p2, a2) = (title.clone(), private, archived);
    let wrote = state.store.write_async(move |c| {
        c.execute("INSERT OR IGNORE INTO group_config (name, updated) VALUES (?1, ?2)", rusqlite::params![n2, now])?;
        if let Some(t) = &t2 {
            c.execute("UPDATE group_config SET channel_title=?2, updated=?3 WHERE name=?1", rusqlite::params![n2, t, now])?;
        }
        if let Some(p) = p2 {
            c.execute("UPDATE group_config SET channel_private=?2, updated=?3 WHERE name=?1", rusqlite::params![n2, p as i64, now])?;
        }
        if let Some(a) = a2 {
            c.execute("UPDATE group_config SET channel_archived_at=?2, updated=?3 WHERE name=?1",
                rusqlite::params![n2, a.then_some(now), now])?;
        }
        Ok(crate::db::WriteOutcome { applied: true, events: vec![channel_event(&n2, "updated")] })
    }).await;
    if let Err(e) = wrote {
        return err(StatusCode::SERVICE_UNAVAILABLE, format!("could not store the change: {e}"));
    }
    tracing::info!(channel = %name, actor = %actor, title = ?title, private = ?private, archived = ?archived,
        measured = true, n_considered = 1, verdict = "conversation_channel_updated", "a channel's settings changed");
    let f = fleet(&state);
    let all = state.store.read().map(|c| people(&c, true)).unwrap_or_default();
    Json(json!({"ok": true, "channel": channel_json(&name, &f, &all)})).into_response()
}

/// A person's place in a channel and why: `owner`, `added`, `removed`, `team`
/// (their team covers the group) or `none`.
fn membership(p: &Person, g: &str, f: &Fleet) -> (bool, &'static str) {
    if p.email == "owner" {
        return (true, "owner");
    }
    match f.ch.member_override(&format!("g:{g}"), &p.email) {
        Some(true) => (true, "added"),
        Some(false) => (false, "removed"),
        None if default_channel_access(&p.scope, g, f) => (true, "team"),
        None => (false, "none"),
    }
}

async fn channel_members(State(state): State<AppState>, Path(name): Path<String>, headers: HeaderMap) -> Response {
    let f = fleet(&state);
    if let Err(r) = require_admin(&headers, &f, "members") {
        return *r;
    }
    let name = name.trim().trim_start_matches("g:").to_lowercase();
    if !channel_exists(&name, &f) {
        return refuse(StatusCode::NOT_FOUND, "channel_not_found", format!("no channel #{name}"));
    }
    let all = state.store.read().map(|c| people(&c, true)).unwrap_or_default();
    let rows: Vec<Value> = all.iter().map(|p| {
        let (member, via) = membership(p, &name, &f);
        json!({"email": p.email, "name": p.name, "member": member, "via": via,
            "scope_level": p.scope.level(), "scope_name": p.scope.name()})
    }).collect();
    Json(json!({"channel": channel_json(&name, &f, &all), "people": rows,
        "measured": !f.ch.unreadable, "n_considered": all.len(),
        "why_unmeasured": f.ch.unreadable.then_some("channel configuration could not be read")})).into_response()
}

async fn set_member(state: &AppState, headers: &HeaderMap, name: &str, email: &str, add: bool) -> Response {
    let f = fleet(state);
    let actor = match require_admin(headers, &f, if add { "add_member" } else { "remove_member" }) {
        Ok(a) => a,
        Err(r) => return *r,
    };
    let name = name.trim().trim_start_matches("g:").to_lowercase();
    if !channel_exists(&name, &f) {
        return refuse(StatusCode::NOT_FOUND, "channel_not_found", format!("no channel #{name}"));
    }
    let email = email.trim().to_ascii_lowercase();
    if email == "owner" {
        return refuse(StatusCode::BAD_REQUEST, "owner_always_member", "the owner is in every channel");
    }
    let all = state.store.read().map(|c| people(&c, false)).unwrap_or_default();
    let Some(p) = all.iter().find(|p| p.email == email) else {
        return refuse(StatusCode::NOT_FOUND, "unknown_person", format!("{email} is not a member of this workspace"));
    };
    // A row is kept only where it differs from the team rule, so removing a
    // hand-added person (or re-adding a removed one) simply deletes it.
    let by_team = default_channel_access(&p.scope, &name, &f);
    let conv = format!("g:{name}");
    let (c2, e2, a2, now) = (conv.clone(), email.clone(), actor.clone(), now_ms());
    let wrote = state.store.write_async(move |c| {
        if add == by_team {
            c.execute("DELETE FROM conversation_members WHERE conv=?1 AND email=?2", rusqlite::params![c2, e2])?;
        } else {
            c.execute(
                "INSERT INTO conversation_members (conv, email, state, by_key, ts) VALUES (?1,?2,?3,?4,?5) \
                 ON CONFLICT(conv, email) DO UPDATE SET state=excluded.state, by_key=excluded.by_key, ts=excluded.ts",
                rusqlite::params![c2, e2, if add { "added" } else { "removed" }, a2, now],
            )?;
        }
        Ok(crate::db::WriteOutcome { applied: true, events: vec![channel_event(&c2[2..], "members")] })
    }).await;
    if let Err(e) = wrote {
        return err(StatusCode::SERVICE_UNAVAILABLE, format!("could not store the change: {e}"));
    }
    tracing::info!(channel = %name, person = %email, add, by_team, actor = %actor, measured = true, n_considered = 1,
        verdict = if add { "conversation_channel_member_added" } else { "conversation_channel_member_removed" },
        "a channel's membership changed");
    let f = fleet(state);
    let (member, via) = membership(p, &name, &f);
    Json(json!({"ok": true, "conv": conv, "email": email, "member": member, "via": via})).into_response()
}

async fn add_member(State(state): State<AppState>, Path(name): Path<String>, headers: HeaderMap, Json(b): Json<Value>) -> Response {
    let email = b.get("email").and_then(Value::as_str).unwrap_or("").to_string();
    set_member(&state, &headers, &name, &email, true).await
}

async fn remove_member(State(state): State<AppState>, Path((name, email)): Path<(String, String)>, headers: HeaderMap) -> Response {
    set_member(&state, &headers, &name, &email, false).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preview_never_reaches_a_private_address_however_it_is_spelled() {
        for a in ["127.0.0.1", "10.1.2.3", "169.254.169.254", "100.64.0.1", "0.0.0.0", "::1", "fd00::1", "fe80::1",
            "::ffff:127.0.0.1", "::ffff:10.0.0.1", "::ffff:169.254.169.254", "::127.0.0.1", "64:ff9b::a9fe:a9fe"] {
            assert!(!public_ip(a.parse().unwrap()), "{a} must not be fetched");
        }
        for a in ["93.184.216.34", "2606:2800:220:1:248:1893:25c8:1946", "::ffff:93.184.216.34"] {
            assert!(public_ip(a.parse().unwrap()), "{a} is public");
        }
        assert!(public_addr("127.0.0.1", 80).is_none() && public_addr("[::ffff:127.0.0.1]", 80).is_none());
    }

    #[test]
    fn a_meta_tag_with_an_empty_or_unquoted_content_does_not_panic() {
        assert_eq!(meta(r#"<meta property="og:title" content=>"#, &["og:title"]), None);
        assert_eq!(meta(r#"<meta property="og:title" content="#, &["og:title"]), None);
        assert_eq!(meta(r#"<meta property="og:title" content=Hello>"#, &["og:title"]).as_deref(), Some("Hello"));
        assert_eq!(meta(r#"<meta property="og:title" content='A &amp; B'>"#, &["og:title"]).as_deref(), Some("A & B"));
    }

    #[test]
    fn a_stale_cli_posting_its_flags_is_recognised_and_real_text_is_not() {
        assert!(looks_like_cli_flags("--thread 4"));
        assert!(looks_like_cli_flags("--thread 12 --stdin"));
        assert!(!looks_like_cli_flags("--thread 4 here is the answer"));
        assert!(!looks_like_cli_flags("use --thread 4 to reply"));
        assert!(!looks_like_cli_flags("--thread"));
    }

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
        let lanes = Fleet::of(vec![lane("ed", &["wexus"]), lane("other", &["mtp"])]);
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
        let lanes = Fleet::of(vec![lane("ed", &["wexus"])]);
        let owner = person("owner", MemberScope::Global);
        assert!(may_read(&owner, &Conv::Group("wexus".into()), &lanes));
        assert!(may_read(&owner, &Conv::Group(GENERAL.into()), &lanes));
        assert!(!may_read(&owner, &Conv::Group("nope-nothing".into()), &lanes));
    }

    #[test]
    fn workers_read_their_groups_and_own_thread_only() {
        let lanes = Fleet::of(vec![lane("ed", &["wexus"]), lane("other", &["mtp"])]);
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
        let lanes = Fleet::of(vec![lane("ed", &["wexus"])]);
        let iso = vec![lane("amux-helper", &[])];
        let owner = Who::Person { email: "ethan@x.com".into(), scope: MemberScope::Global };
        let dm = Conv::People("doron@z.com".into(), "ethan@x.com".into());
        assert_eq!(mentionable_isolated(&owner, &dm, &iso), vec!["amux-helper".to_string()]);
        assert_eq!(mentionable_isolated(&owner, &Conv::Group(GENERAL.into()), &iso), vec!["amux-helper".to_string()]);
        assert_eq!(mentionable_isolated(&owner, &Conv::Group("wexus".into()), &iso), vec!["amux-helper".to_string()],
            "a person reaches it from any channel, as any other worker they can reach");
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
    fn a_worker_mentioned_in_a_dm_reads_and_answers_only_that_thread() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE conversation_posts (id INTEGER PRIMARY KEY, conv TEXT, author_key TEXT, mentions TEXT, parent_id INTEGER);").unwrap();
        let dm = Conv::People("doron@z.com".into(), "ethan@x.com".into());
        let k = dm.key();
        let add = |id: i64, author: &str, mentions: &str, parent: Option<i64>| {
            c.execute("INSERT INTO conversation_posts VALUES (?1,?2,?3,?4,?5)", rusqlite::params![id, k, author, mentions, parent]).unwrap();
        };
        add(1, "person:ethan@x.com", r#"["worker:ed"]"#, None); // asked ed
        add(2, "worker:ed", "[]", Some(1));                        // ed's answer
        add(3, "person:doron@z.com", "[]", None);                  // an unrelated message
        add(4, "worker:ed", r#"["worker:mallory"]"#, None);         // a worker cannot invite another
        assert!(worker_invited(&c, &dm, 1, "ed"));
        assert!(worker_invited(&c, &dm, 2, "ed"), "a reply id resolves to its thread");
        assert!(!worker_invited(&c, &dm, 3, "ed"), "another thread of the same DM");
        assert!(!worker_invited(&c, &dm, 1, "other"), "a worker nobody mentioned");
        assert!(!worker_invited(&c, &dm, 4, "mallory"), "only a person's mention invites");
        assert!(!worker_invited(&c, &Conv::Group("wexus".into()), 1, "ed"), "a thread of another conversation");
        assert!(!worker_invited(&c, &dm, 99, "ed"));
        // A channel: a person brought a worker from another group into one thread.
        c.execute("INSERT INTO conversation_posts VALUES (10,'g:wexus','person:ethan@x.com','[\"worker:workflows\"]',NULL)", []).unwrap();
        c.execute("INSERT INTO conversation_posts VALUES (11,'g:wexus','person:ethan@x.com','[]',NULL)", []).unwrap();
        let wexus = Conv::Group("wexus".into());
        assert!(worker_invited(&c, &wexus, 10, "workflows"));
        assert!(!worker_invited(&c, &wexus, 11, "workflows"), "another thread of the channel");
        assert!(!worker_invited(&c, &Conv::Worker("workflows".into()), 10, "workflows"));
    }

    #[test]
    fn reading_a_channel_up_to_its_latest_post_clears_a_reply_the_page_never_lists() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE conversation_posts (id INTEGER PRIMARY KEY, conv TEXT, author_key TEXT, mentions TEXT, parent_id INTEGER, ts INTEGER);
                         CREATE TABLE conversation_reads (reader TEXT, conv TEXT, last_id INTEGER, PRIMARY KEY (reader, conv));").unwrap();
        let add = |id: i64, author: &str, parent: Option<i64>| {
            c.execute("INSERT INTO conversation_posts VALUES (?1,'g:wexus',?2,'[]',?3,?1)", rusqlite::params![id, author, parent]).unwrap();
        };
        add(1, "person:ethan@x.com", None);
        add(2, "worker:ed", Some(1)); // a reply: the top-level page never lists it
        let me = "person:ethan@x.com";
        let read_to = |n: i64| c.execute("INSERT OR REPLACE INTO conversation_reads VALUES (?1,'g:wexus',?2)", rusqlite::params![me, n]).unwrap();
        read_to(1); // the newest TOP-LEVEL post, which is what the page used to send
        assert_eq!(counts(&c, me)["g:wexus"].0, 1, "the reply stays unread");
        assert_eq!(latest_post_id(&c, "g:wexus"), 2);
        read_to(latest_post_id(&c, "g:wexus"));
        assert_eq!(counts(&c, me)["g:wexus"].0, 0, "reading up to the latest post clears it");
    }

    #[test]
    fn the_reply_recipe_round_trips_a_dm_key_and_its_thread() {
        let dm = Conv::People("doron@z.com".into(), "ethan@x.com".into());
        let t = delivery_text(&dm, "ethan", "@ed get me customer A's reply")
            .replace(&format!("amux post '{}' <<", dm.key()), &format!("amux post '{}' --thread 7 <<", dm.key()));
        assert_eq!(conv_of_prompt(&t), Some((dm.key(), Some(7))));
        let g = delivery_text(&Conv::Group("wexus".into()), "ethan", "@ed run");
        assert_eq!(conv_of_prompt(&g), Some(("g:wexus".to_string(), None)));
    }

    #[test]
    fn an_isolated_worker_gets_the_owners_words_and_a_dm_answer_goes_to_the_thread() {
        let dm = Conv::People("doron@z.com".into(), "ethan@x.com".into());
        let (t, reply) = delivery_for(&dm, "ethan", "@amux-helper make an mdai", true);
        assert_eq!(reply, "pane");
        assert_eq!(t, "ethan (amux Messages, a direct message): @amux-helper make an mdai");
        assert!(!t.contains("amux post"), "no injected instructions into an isolated worker");
        let (t, reply) = delivery_for(&dm, "ethan", "@ed run it", false);
        assert_eq!(reply, "here", "a DM answer goes in the thread under the post that asked");
        assert!(t.contains("amux post 'p:doron@z.com|ethan@x.com' <<'EOF'"), "{t}");
        assert!(!t.contains('$'), "{t}");
        let (_, reply) = delivery_for(&Conv::Group("wexus".into()), "ethan", "@ed run", false);
        assert_eq!(reply, "here");
    }

    #[test]
    fn general_mentions_stay_inside_the_posters_access() {
        let lanes = Fleet::of(vec![lane("ed", &["wexus"]), lane("other", &["mtp"])]);
        let elliot = person("elliot@y.com", MemberScope::Group("wexus".into()));
        assert_eq!(mentionable_workers(&elliot, &Conv::Group(GENERAL.into()), &lanes), vec!["ed".to_string()]);
        let global = person("ethan@x.com", MemberScope::Global);
        assert_eq!(mentionable_workers(&global, &Conv::Group(GENERAL.into()), &lanes).len(), 2);
        // In a channel: its own workers first, then any other the person can reach.
        assert_eq!(mentionable_workers(&global, &Conv::Group("mtp".into()), &lanes), vec!["other".to_string(), "ed".to_string()]);
        assert_eq!(mentionable_workers(&elliot, &Conv::Group("mtp".into()), &lanes), vec!["other".to_string(), "ed".to_string()],
            "elliot reaches ed through wexus, so may bring ed into #mtp");
        let scoped = person("w@y.com", MemberScope::Worker("other".into()));
        assert_eq!(mentionable_workers(&scoped, &Conv::Group("wexus".into()), &lanes), vec!["ed".to_string(), "other".to_string()]);
        let ed = Who::Worker { name: "ed".into(), groups: ["wexus".to_string()].into() };
        assert_eq!(mentionable_workers(&ed, &Conv::Group("mtp".into()), &lanes), vec!["other".to_string()], "a worker stays inside the group");
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
        let lanes = Fleet::of(vec![Lane { name: "mtp-demo".into(), groups: ["video".to_string()].into_iter().collect() }]);
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
        assert!(!may_read(&w, &dm, &Fleet::default()), "a worker never posts into two people's DM, so never as the owner");
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
        assert!(!may_read(&ann, &dm, &Fleet::default()), "ann cannot read bob and carol's DM");
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
    fn a_page_send_by_a_member_is_the_same_person_as_their_messages_post() {
        let names: HashMap<String, String> = [("ethan@mixpeek.com".to_string(), "Ethan".to_string())].into();
        let a = history_author("user", "member:Ethan@mixpeek.com", &names);
        // The key a direct post by the same caller gets from who().
        let direct = person("ethan@mixpeek.com", MemberScope::Global).key();
        assert_eq!(a.key, direct);
        assert_eq!((a.name.as_str(), a.verdict), ("Ethan", "person"));
        // A member with no name reads as their email's local part, as in Messages.
        let d = history_author("steering", "member:dorongreenspan@gmail.com", &names);
        assert_eq!((d.key.as_str(), d.name.as_str()), ("person:dorongreenspan@gmail.com", "dorongreenspan"));
        assert_eq!(history_author("user", "", &names).key, "person:owner");
        assert_eq!(history_author("chat", "chat-companion", &names).key, "person:owner");
        assert_eq!(history_author("user", "hello@amux.io", &names).key, "person:hello@amux.io");
        assert_eq!(history_author("session", "mxp-gs12@chat", &names).key, "worker:mxp-gs12");
        assert_eq!(history_author("schedule", "[SCHED-1] x", &names).verdict, "schedule");
        assert_eq!(history_author("pickup", "board-drive", &names).key, "harness:board-drive");
        let u = history_author("user", "pane", &names);
        assert_eq!((u.key.as_str(), u.verdict), ("origin:pane", "unresolved"));
        assert!(!u.key.starts_with("person:"), "an unknown stamp must not read as a person");
    }

    #[test]
    fn delivery_text_names_the_source_and_the_reply_route() {
        let t = delivery_text(&Conv::Group("wexus".into()), "elliot", "@ed run 10/11");
        assert!(t.starts_with("elliot mentioned you in #wexus"), "{t}");
        assert!(t.contains("\n\n@ed run 10/11\n\n"));
        assert!(t.contains("Nobody pasted this") && t.contains("Treat it as elliot's own request"),
            "a worker must read it as a request from the person, not pasted text: {t}");
        assert!(t.contains("amux post 'g:wexus' <<'EOF'"));
        // Claude Code asks before running a command with unverifiable
        // variables, so the reply recipe must not contain any.
        assert!(!t.contains('$'), "{t}");
    }
}

