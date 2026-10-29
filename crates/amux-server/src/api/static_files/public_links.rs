//! What the Pages tab provisions besides Pages themselves, one store, three
//! kinds (Ethan, 2026-10-25: "pages = internal apps, external apps, external
//! facing files", proxies consolidated into it):
//!
//! - `internal`: an app on a port, for the signed-in owner only. Opened inside
//!   the amux UI from Pages, served on the apps origin at `/app/<id>/`
//!   (apps_origin.rs), never on the dashboard's own origin.
//! - `app`: the same, public: `/pub/<id>/`, anyone with the link, no amux
//!   sign-in, until turned off.
//! - `file`: a file or folder, public at `/pub/<id>/`, opt in per path. A
//!   folder lists its files; nothing outside it, nothing hidden and nothing
//!   amux's own file guard refuses is ever served.
//!
//! The id is 128 random bits and the only credential. Every public HTML
//! response carries a CSP sandbox, so a published page runs in an opaque
//! origin and cannot act as the owner who happens to open it, and amux's
//! cookies and Authorization never reach a relayed app.
//!
//! Links are built from the server's own address ([`public_url`]), so a worker
//! never hands the owner `https://<raw ip>:8824/...` (sandbox, 2026-10-25:
//! mtp-demo did exactly that, because nothing told it the address).
use super::app_relay;
use super::AppState;
use axum::body::Body;
use axum::extract::{Path as AxPath, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

const LINKS_FILE: &str = "public-links.json";
const LEARNED_URL_FILE: &str = "public-url.json";
static WRITE: Mutex<()> = Mutex::new(());

/// The sandbox `/api/file/raw-path` already puts on HTML, minus nothing.
const HTML_SANDBOX: &str = "sandbox allow-scripts allow-popups allow-forms allow-popups-to-escape-sandbox allow-modals allow-downloads";

fn default_kind() -> String {
    "app".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Link {
    pub id: String,
    /// `internal`, `app` or `file`. An `internal` link names a port (an app)
    /// or a `path` (a file or folder shown inside amux).
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub created_at: i64,
    /// The person who approved a public link. Empty means pending: a link a
    /// worker asked for is served to nobody until the owner approves it in
    /// Pages (Ethan, 2026-10-25: exposure is opt in). A link saved before this
    /// field existed reads as pending too, so an upgrade fails closed.
    #[serde(default)]
    pub approved_by: String,
}

impl Link {
    fn is_public(&self) -> bool {
        self.kind == "app" || self.kind == "file"
    }
    /// Served to anyone with the link: public, and approved by a person.
    fn is_live(&self) -> bool {
        self.is_public() && !self.approved_by.is_empty()
    }
    fn is_pending(&self) -> bool {
        self.approved_by.is_empty()
    }
}

/// The app inside amux with this id, if a person approved it: the only apps
/// the apps origin serves (apps_origin.rs).
pub(crate) fn approved_internal(id: &str) -> Option<Link> {
    load(&crate::config::amux_home()).into_iter().find(|l| l.id == id && l.kind == "internal" && !l.approved_by.is_empty())
}

/// The signed-in PERSON behind a request, never a worker: a server-verified
/// member (the cloud door's sign-in; its marker headers are stripped from
/// every inbound request, so a lane cannot assert it) or the owner's browser
/// session cookie. A worker's bearer token, a session header or a loopback
/// address is not a person, so a worker (or a prompt injected into one) can
/// ask for a public link but cannot make one live.
pub(crate) fn person_of(state: &AppState, headers: &HeaderMap) -> Option<String> {
    if let Some(m) = crate::api::org::local_member_actor(headers) {
        return Some(m.to_string());
    }
    super::has_owner_session(state, headers).then(|| "owner".to_string())
}

fn links_path(home: &Path) -> PathBuf {
    home.join(LINKS_FILE)
}

pub(crate) fn load(home: &Path) -> Vec<Link> {
    std::fs::read_to_string(links_path(home))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| serde_json::from_value(v["links"].clone()).ok())
        .unwrap_or_default()
}

fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", ulid::Ulid::new()));
    let r = std::fs::write(&tmp, body).and_then(|_| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path)
    });
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

fn save(home: &Path, links: &[Link]) -> std::io::Result<()> {
    write_atomic(&links_path(home), &serde_json::to_string_pretty(&json!({ "links": links })).unwrap_or_default())
}

/// 128 bits from the OS. No weaker fallback: the id is the whole credential.
pub(crate) fn new_id() -> Option<String> {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom").ok()?.read_exact(&mut b).ok()?;
    Some(hex::encode(b))
}

pub(crate) fn valid_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Ports an app link may never reach: amux itself (its API and dashboard),
/// the cloud front door, and SSH.
pub(crate) fn refused_port(port: u16) -> Option<&'static str> {
    if port == 0 {
        return Some("not a port");
    }
    if port == crate::config::canonical_port() || (8822..=8824).contains(&port) {
        return Some("that is amux's own port; a link never exposes amux itself");
    }
    match port {
        8890 => Some("that is the cloud sign-in door"),
        22 => Some("that is SSH"),
        // Local admin and data services: a link to one hands its control or its
        // data to whoever holds the link.
        2019 => Some("that is Caddy's admin API"),
        2375 | 2376 => Some("that is the Docker API"),
        2379 | 2380 => Some("that is etcd"),
        3306 | 5432 | 6379 | 11211 | 27017 | 5984 | 9200 | 9300 | 8086 | 7474 | 7687 => Some("that is a database"),
        8500 | 8200 => Some("that is Consul or Vault"),
        1..=1023 => Some("ports below 1024 are system services"),
        _ => None,
    }
}

/// Whether a canonical path may be published: amux's own file guard allows it
/// (no ~/.ssh, credentials, system paths), it is outside amux's home, and no
/// part of it is hidden (a dotfile or dot-directory).
pub(crate) fn file_refused(canon: &Path) -> Option<&'static str> {
    if !canon.is_absolute() {
        return Some("give an absolute path");
    }
    if canon.components().any(|c| matches!(c, Component::Normal(s) if s.to_string_lossy().starts_with('.'))) {
        return Some("hidden files and folders are never published");
    }
    if canon.file_name().is_some_and(|n| secret_like_name(&n.to_string_lossy())) {
        return Some("that name looks like a key, credential or secret, which is never published");
    }
    if let Ok(home) = crate::config::amux_home().canonicalize() {
        if canon.starts_with(&home) {
            return Some("amux's own files are never published");
        }
    }
    if !crate::api::fs::is_path_allowed(canon) {
        return Some("amux's file guard does not allow that path");
    }
    None
}

/// Names that hold keys, credentials or secrets by convention, refused even
/// when they are not hidden and sit inside a folder the owner shared.
pub(crate) fn secret_like_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let exact = ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", "authorized_keys", "known_hosts", "credentials",
        "credentials.json", "secrets.json", "secrets.yaml", "secrets.yml", "server.env", "wallet.dat",
        "terraform.tfstate", "terraform.tfstate.backup", "htpasswd", "shadow", "passwd"];
    let suffixes = [".pem", ".key", ".p12", ".pfx", ".jks", ".keystore", ".kdbx", ".env", ".tfstate",
        ".ppk", ".asc", ".gpg", ".sqlite", ".sqlite3", ".db"];
    exact.contains(&n.as_str())
        || n.starts_with("id_rsa") || n.starts_with("id_ed25519") || n.starts_with("id_ecdsa")
        || suffixes.iter().any(|s| n.ends_with(s))
        || n.contains("secret") || n.contains("credential") || n.contains("password") || n.contains("token")
}

/// `https://host[:port]` with no trailing slash, or None.
fn normalize_url(u: &str) -> Option<String> {
    let u = u.trim().trim_end_matches('/');
    let rest = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://"))?;
    (!rest.is_empty() && !rest.contains('/') && !rest.contains(char::is_whitespace)).then(|| u.to_string())
}

/// The address the owner reaches this amux at. `AMUX_PUBLIC_URL` (server.env;
/// a cloud box sets it to https://<its domain>) wins; otherwise the address the
/// owner last opened the dashboard at. None when neither is known: callers say
/// so rather than guessing.
/// The owner's link to a file in amux's file viewer (deep_links.rs), built
/// the way the dashboard's copy-link builds it: `<address>/f/<path>`, each
/// segment encoded. Relative to the server when its address is unknown.
pub(crate) fn file_link(home: &Path, path: &str) -> String {
    let enc: Vec<String> = path.split('/').map(|seg| {
        seg.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
    }).collect();
    let p = enc.join("/");
    let p = if p.starts_with('/') { p } else { format!("/{p}") };
    format!("{}/f{p}", public_url(home).unwrap_or_default().trim_end_matches('/'))
}

pub(crate) fn public_url(home: &Path) -> Option<String> {
    if let Some(u) = crate::api::settings::effective_env(home, "AMUX_PUBLIC_URL").as_deref().and_then(normalize_url) {
        return Some(u);
    }
    std::fs::read_to_string(home.join(LEARNED_URL_FILE))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["url"].as_str().and_then(normalize_url))
}

fn is_loopback_host(host: &str) -> bool {
    let h = app_relay::host_only(host).trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h == "0.0.0.0" || h.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The origin a dashboard request was made at, from its Host header. None for
/// loopback (a worker on the box, not the owner's address).
pub(crate) fn origin_of(headers: &HeaderMap) -> Option<String> {
    let host = headers.get(header::HOST)?.to_str().ok()?.trim();
    if host.is_empty() || is_loopback_host(host) || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b)) {
        return None;
    }
    let proto = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).filter(|p| *p == "http").unwrap_or("https");
    Some(format!("{proto}://{host}"))
}

/// Remember the address an OWNER opened the dashboard at (the caller checks
/// that it is the owner, so a stranger's Host header cannot redirect workers'
/// links). Written only when it changes.
pub(crate) fn learn_public_url(home: &Path, headers: &HeaderMap) {
    let Some(url) = origin_of(headers) else { return };
    let path = home.join(LEARNED_URL_FILE);
    let current = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    if current.as_ref().and_then(|v| v["url"].as_str()) == Some(url.as_str()) {
        return;
    }
    let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    if write_atomic(&path, &json!({ "url": url, "learned_at": crate::config::now_f64() as i64 }).to_string()).is_ok() {
        tracing::info!(url = %url, measured = true, n_considered = 1, verdict = "public_url_learned",
            "learned the address the owner opens amux at; workers build their links from it");
    }
}

/// What a worker is told about links to what it builds, with this server's
/// real address in it (compose_worker_context).
pub(crate) fn worker_links_guide(home: &Path) -> String {
    let (base, known) = match public_url(home) {
        Some(u) => (u, true),
        None => ("<this amux's address>".to_string(), false),
    };
    let address = if known {
        format!("This amux is reached at {base}. Build every link you give the owner from that address, \
                 never from localhost, 127.0.0.1 or this machine's IP address.")
    } else {
        "This amux's address is not known yet (AMUX_PUBLIC_URL in server.env sets it; otherwise it is \
         learned the first time the owner opens the dashboard). Give the owner the path, such as \
         /#view=pages, and say the address is unknown. Do not guess it from an IP address."
            .to_string()
    };
    format!(
        "{address}\n\n\
         A file for the owner (a workbook, a report, a deck, an HTML page) is not a Page. Name each file \
         by its absolute path, one per file, never a folder: amux shows the path as a link that opens in \
         its file viewer, signed in, with nothing to approve. Give the web link, {base}/f/<absolute path>, \
         only when the owner asks for a link (to copy, send or open outside amux). Never start a web \
         server (such as `python3 -m http.server`) to show files.\n\n\
         Pages holds apps with a backend (a server, inside amux or outside it) and files shared outside \
         amux. Use it only when the owner asks for one of those:\n\
         - An app for the owner, inside amux: `amux publish --internal <port> [title]`. Its link is \
         {base}/#view=pages&open=<id>, with the id `amux publish` prints. Never hand the owner localhost \
         or a port. A Page is {base}/pages/<name>.\n\
         - An app for anyone with the link (no amux sign-in): `amux publish <port> [title]`, which prints \
         {base}/pub/<id>/.\n\
         - A file or folder for anyone with the link: `amux publish --file <absolute path> [title]`. Never \
         link a file as {base}/api/file/...: that needs the owner's sign-in.\n\
         Anything you add stays pending until the owner approves it in Pages; tell them it is waiting \
         there. Never try to approve it yourself.\n\
         `amux publish --list` lists them and `amux publish --revoke <id>` turns one off.\n\n\
         Build demos and documents as files in your workspace and link them as above. Never \
         publish to claude.ai (the Artifact tool): that lives outside this amux, where the owner cannot \
         list, serve or turn it off, and amux denies the tool."
    )
}

fn link_json(home: &Path, l: &Link) -> Value {
    let base = public_url(home);
    // An app inside amux is reached through its Pages entry, which opens it on
    // the apps origin with a one-time handoff; there is no direct link to it.
    let path = if l.is_public() { format!("/pub/{}/", l.id) } else { format!("/#view=pages&open={}", l.id) };
    json!({
        "id": l.id, "kind": l.kind, "port": l.port, "file": l.path, "title": l.title,
        "created_by": l.created_by, "created_at": l.created_at,
        "public": l.is_public(),
        "pending": l.is_pending(),
        "approved_by": l.approved_by,
        "path": path,
        "url": base.as_ref().map(|b| format!("{b}{path}")),
        "app_path": (!l.is_public()).then(|| format!("/app/{}/", l.id)),
    })
}

fn bad(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

pub(crate) async fn list(State(_): State<AppState>) -> Response {
    let home = crate::config::amux_home();
    let links = load(&home);
    // Whether each app answers right now (Pages shows a live preview, or a
    // placeholder for an app that is not running), checked together.
    let running = futures::future::join_all(links.iter().map(|l| async move {
        l.port > 0 && app_relay::local_listener(l.port).await.is_some()
    }))
    .await;
    let rows: Vec<Value> = links
        .iter()
        .zip(running)
        .map(|(l, up)| {
            let mut v = link_json(&home, l);
            if l.port > 0 {
                v["running"] = json!(up);
            }
            if !l.path.is_empty() {
                let (bytes, files) = shared_size(Path::new(&l.path));
                v["size_bytes"] = json!(bytes);
                v["files"] = json!(files);
            }
            v
        })
        .collect();
    Json(json!({
        "links": rows,
        "public_url": public_url(&home),
        "measured": true, "n_considered": links.len(),
    }))
    .into_response()
}

/// What a file link shares: its size and file count, counting only what
/// /pub/ would serve (visible, inside, allowed), and bounded so a huge folder
/// cannot stall the list. Missing reads as zero.
fn shared_size(path: &Path) -> (u64, u64) {
    let Ok(root) = path.canonicalize() else { return (0, 0) };
    if root.is_file() {
        return (std::fs::metadata(&root).map(|m| m.len()).unwrap_or(0), 1);
    }
    let (mut bytes, mut files, mut seen) = (0u64, 0u64, 0usize);
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > 5000 {
                return (bytes, files);
            }
            let Ok(c) = e.path().canonicalize() else { continue };
            if !c.starts_with(&root) || file_refused(&c).is_some() {
                continue;
            }
            if c.is_dir() {
                stack.push(c);
            } else if let Ok(m) = std::fs::metadata(&c) {
                bytes += m.len();
                files += 1;
            }
        }
    }
    (bytes, files)
}

#[derive(Deserialize)]
pub(crate) struct CreateBody {
    #[serde(default)]
    kind: Option<String>,
    port: Option<u16>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    title: String,
}

pub(crate) async fn create(State(state): State<AppState>, headers: HeaderMap, body: Option<Json<CreateBody>>) -> Response {
    let Some(Json(body)) = body else { return bad(StatusCode::BAD_REQUEST, "send {\"kind\": ..., \"port\" or \"path\": ...}") };
    let kind = body.kind.clone().unwrap_or_else(|| if body.path.is_some() { "file".into() } else { "app".into() });
    let (port, path) = match kind.as_str() {
        "app" | "internal" if !(kind == "internal" && body.path.is_some()) => {
            let Some(port) = body.port else { return bad(StatusCode::BAD_REQUEST, "port is required") };
            if let Some(why) = refused_port(port) {
                tracing::warn!(port, kind = %kind, measured = true, n_considered = 1, verdict = "public_link_refused", why, "link refused");
                return bad(StatusCode::BAD_REQUEST, why);
            }
            (port, String::new())
        }
        // A file or folder: public ("file"), or for the owner inside amux
        // ("internal" with a path), served from the apps origin with no server
        // to start (2026-10-26: rothco ran `python3 -m http.server` over a
        // whole folder of build scripts to show the owner one HTML file).
        "file" | "internal" => {
            let raw = body.path.as_deref().unwrap_or("").trim();
            let Ok(canon) = Path::new(raw).canonicalize() else {
                return bad(StatusCode::BAD_REQUEST, "no such file or folder");
            };
            if let Some(why) = file_refused(&canon) {
                tracing::warn!(kind = %kind, measured = true, n_considered = 1, verdict = "public_link_refused", why, "file link refused");
                return bad(StatusCode::BAD_REQUEST, why);
            }
            // A FILE FOR THE OWNER IS NOT A PAGE (Ethan, 2026-10-28: "it is
            // making page apps for just files. pages apps should only be used
            // if we're being explicitly asked"). A worker showed three
            // workbooks and a rules doc as four Pages, each waiting for
            // approval, because this module's own guidance told it to. A
            // worker gets the file link instead; the owner adding one in
            // Pages is the explicit ask, so that still works.
            if kind == "internal" && person_of(&state, &headers).is_none() {
                let link = file_link(&crate::config::amux_home(), &canon.to_string_lossy());
                tracing::warn!(created_by = headers.get("x-amux-session").and_then(|v| v.to_str().ok()).unwrap_or("-"),
                    measured = true, n_considered = 1, verdict = "pages_internal_file_refused", "file for the owner sent to Pages");
                return (StatusCode::BAD_REQUEST, Json(json!({
                    "error": format!("a file for the owner is not a Page: Pages holds apps with a backend and files shared outside amux. \
                        Give the owner {link}, which opens it in amux's file viewer with nothing to approve."),
                    "code": "file_not_a_page", "file_link": link,
                }))).into_response();
            }
            (0, canon.to_string_lossy().into_owned())
        }
        _ => return bad(StatusCode::BAD_REQUEST, "kind is internal, app or file"),
    };
    let Some(id) = new_id() else {
        return bad(StatusCode::INTERNAL_SERVER_ERROR, "no secure randomness available; no link was made");
    };
    let created_by = headers
        .get("x-amux-session")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("owner")
        .to_string();
    let title: String = body.title.trim().chars().take(120).collect();
    let person = person_of(&state, &headers);
    // Every kind needs a person: a public link hands the app or file to
    // anyone, and an internal app runs with the owner's amux access.
    let approved_by = person.clone().unwrap_or_default();
    let link = Link { id, kind: kind.clone(), port, path, title, created_by: created_by.clone(), created_at: crate::config::now_f64() as i64, approved_by };
    let home = crate::config::amux_home();
    {
        let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
        let mut links = load(&home);
        links.push(link.clone());
        if let Err(e) = save(&home, &links) {
            return bad(StatusCode::INTERNAL_SERVER_ERROR, &format!("could not save the link: {e}"));
        }
    }
    let listening = port > 0 && app_relay::local_listener(port).await.is_some();
    tracing::warn!(id = %link.id, kind = %kind, port, created_by = %created_by, by_person = person.is_some(), listening,
        public = link.is_public(), pending = link.is_pending(),
        measured = true, n_considered = 1, verdict = "public_link_created",
        "Pages link created; a public one asked for by a worker serves nobody until the owner approves it in Pages");
    let mut out = link_json(&home, &link);
    out["listening"] = json!(listening);
    (StatusCode::CREATED, Json(out)).into_response()
}

pub(crate) async fn revoke(State(state): State<AppState>, headers: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    let home = crate::config::amux_home();
    let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let mut links = load(&home);
    let before = links.len();
    links.retain(|l| l.id != id);
    if links.len() == before {
        return bad(StatusCode::NOT_FOUND, "no such link");
    }
    if let Err(e) = save(&home, &links) {
        return bad(StatusCode::INTERNAL_SERVER_ERROR, &format!("could not save: {e}"));
    }
    let who = person_of(&state, &headers)
        .or_else(|| headers.get("x-amux-session").and_then(|v| v.to_str().ok()).map(str::to_string))
        .unwrap_or_else(|| "unknown".into());
    tracing::warn!(id = %id, by = %who, measured = true, n_considered = 1, verdict = "public_link_revoked", "Pages link removed");
    Json(json!({ "ok": true })).into_response()
}

/// Make a pending public link live. Only a person may (see [`person_of`]): a
/// worker that asked for the link cannot also approve it.
pub(crate) async fn approve(State(state): State<AppState>, headers: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    let Some(person) = person_of(&state, &headers) else {
        tracing::warn!(id = %id, measured = true, n_considered = 1, verdict = "public_link_approve_refused",
            "a public link approval came from something other than a signed-in person");
        return bad(StatusCode::FORBIDDEN, "only the owner, signed in to the dashboard, can approve a Pages link");
    };
    let home = crate::config::amux_home();
    let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let mut links = load(&home);
    let Some(link) = links.iter_mut().find(|l| l.id == id) else {
        return bad(StatusCode::NOT_FOUND, "no such link");
    };
    link.approved_by = person.clone();
    let out = link_json(&home, link);
    if let Err(e) = save(&home, &links) {
        return bad(StatusCode::INTERNAL_SERVER_ERROR, &format!("could not save: {e}"));
    }
    tracing::warn!(id = %id, by = %person, measured = true, n_considered = 1, verdict = "public_link_approved",
        "a Pages link was approved (a public one now opens for anyone with it)");
    Json(out).into_response()
}

/// `/pub/<id>/a/b?x=1` -> ("<id>", "/a/b?x=1").
pub(crate) fn parse_pub(uri: &Uri) -> Option<(String, String)> {
    let rest = uri.path().strip_prefix("/pub/")?;
    let (id, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let mut tail = tail.to_string();
    if let Some(q) = uri.query() {
        tail.push('?');
        tail.push_str(q);
    }
    Some((id.to_string(), tail))
}

/// Plain text, not an HTML page: a 404 text/html is the SPA shell's signature,
/// and tests/route_table.rs reads it as "this route fell through to the
/// dashboard" (it did not, and the plain sentence reads the same to a person).
fn not_found() -> Response {
    (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "Link not found. It does not exist or was turned off by the person who shared it.\n").into_response()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn pct_encode_segment(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// The file a public file link's `tail` names: the shared file itself, or a
/// path inside the shared folder. None for anything that leaves the folder,
/// is hidden, or is refused by [`file_refused`].
pub(crate) fn resolve_shared(root: &Path, tail: &str) -> Option<PathBuf> {
    let rel = pct_decode(tail.split('?').next().unwrap_or("/"));
    let rel = rel.trim_start_matches('/');
    if root.is_file() {
        return rel.is_empty().then(|| root.to_path_buf());
    }
    if Path::new(rel).components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    let canon = root.join(rel).canonicalize().ok()?;
    (canon.starts_with(root) && file_refused(&canon).is_none()).then_some(canon)
}

fn folder_listing(link: &Link, dir: &Path, tail: &str) -> Response {
    let mut entries: Vec<(String, bool)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().canonicalize().is_ok_and(|c| c.starts_with(dir) && file_refused(&c).is_none()))
                .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path().is_dir()))
                .collect()
        })
        .unwrap_or_default();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
    let title = if link.title.is_empty() { Path::new(&link.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() } else { link.title.clone() };
    let here = tail.split('?').next().unwrap_or("/");
    let rows: String = entries
        .iter()
        .map(|(n, d)| format!("<li><a href=\"{}{}\">{}{}</a></li>", pct_encode_segment(n), if *d { "/" } else { "" }, esc(n), if *d { "/" } else { "" }))
        .collect();
    let up = if here.trim_matches('/').is_empty() { String::new() } else { "<li><a href=\"../\">..</a></li>".to_string() };
    let html = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\">\
         <meta name=\"color-scheme\" content=\"dark light\"><title>{t}</title>\
         <style>body{{font-family:system-ui,sans-serif;max-width:48em;margin:6vh auto;padding:0 16px;line-height:1.7}}li{{list-style:none}}</style>\
         </head><body><h1 style=\"font-size:1.3em\">{t}<span style=\"opacity:.6\">{h}</span></h1><ul style=\"padding:0\">{up}{rows}</ul></body></html>",
        t = esc(&title), h = esc(if here == "/" { "" } else { here }),
    );
    sandbox_all((StatusCode::OK, Html(html)).into_response())
}

/// EVERY response a public link serves runs in an opaque origin, whatever its
/// type. /pub/ shares the dashboard's origin, and an SVG, XML or XHTML file
/// runs script as surely as HTML does: with the sandbox on HTML only, a shared
/// SVG could call amux's API as the owner who opened it. No content sniffing,
/// no referrer to the app, and no cookie set by the app on amux's origin.
fn sandbox_all(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(HTML_SANDBOX));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.remove(header::SET_COOKIE);
    r
}

/// One line per public request, so "who opened this link" has an answer
/// (2026-10-25: a link to a page with real names was live for hours and the
/// server could not say whether anyone had opened it). Bounded: rotated at
/// 5 MB, one previous file kept.
fn access_log(id: &str, status: StatusCode, headers: &HeaderMap, path: &str) {
    let dir = crate::config::amux_home().join("logs");
    let file = dir.join("public-links-access.jsonl");
    if std::fs::metadata(&file).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
        let _ = std::fs::rename(&file, dir.join("public-links-access.1.jsonl"));
    }
    let ip = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next()).map(str::trim).unwrap_or("");
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
    let line = json!({"ts": crate::config::now_f64() as i64, "id": id, "status": status.as_u16(),
        "path": path.chars().take(300).collect::<String>(), "ip": ip, "ua": ua.chars().take(200).collect::<String>()});
    use std::io::Write;
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&file) {
        let _ = writeln!(f, "{line}");
    }
}

/// A file link's file, or a file inside its folder (an index.html, else a
/// listing). Shared by public file links (/pub/) and files inside amux (the
/// apps origin's /app/), so both refuse exactly the same paths.
pub(crate) async fn serve_link_file(link: &Link, uri_path: &str, tail: &str, method: &Method, headers: &HeaderMap) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return not_found();
    }
    let Some(root) = Path::new(&link.path).canonicalize().ok().filter(|r| file_refused(r).is_none()) else {
        tracing::warn!(id = %link.id, kind = %link.kind, measured = true, n_considered = 1, verdict = "public_link_file_gone",
            "file link opened while its file is gone or no longer allowed");
        return not_found();
    };
    let Some(target) = resolve_shared(&root, tail) else { return not_found() };
    if target.is_dir() {
        if !tail.split('?').next().unwrap_or("").ends_with('/') {
            return axum::response::Redirect::permanent(&format!("{uri_path}/")).into_response();
        }
        let index = target.join("index.html");
        if index.is_file() {
            return crate::api::file_viewer::serve_file(&index, headers).await;
        }
        return folder_listing(link, &target, tail);
    }
    crate::api::file_viewer::serve_file(&target, headers).await
}

/// Anyone with the link: no amux sign-in. Apps are relayed from loopback,
/// never redirected to the raw port (a cloud box's firewall closes those).
pub(crate) async fn serve(method: Method, headers: HeaderMap, uri: Uri, body: Body) -> Response {
    let id = parse_pub(&uri).map(|(id, _)| id).filter(|id| valid_id(id)).unwrap_or_default();
    let r = sandbox_all(serve_inner(method, &headers, &uri, body).await);
    if !id.is_empty() {
        access_log(&id, r.status(), &headers, uri.path());
    }
    r
}

async fn serve_inner(method: Method, headers: &HeaderMap, uri: &Uri, body: Body) -> Response {
    let headers = headers.clone();
    let Some((id, tail)) = parse_pub(uri) else { return not_found() };
    if !valid_id(&id) {
        return not_found();
    }
    let home = crate::config::amux_home();
    let Some(link) = load(&home).into_iter().find(|l| l.id == id) else {
        tracing::info!(measured = true, n_considered = 1, verdict = "public_link_unknown", "public link not found");
        return not_found();
    };
    // A pending link (asked for, not yet approved by a person) looks exactly
    // like one that does not exist.
    if !link.is_live() {
        return not_found();
    }
    if uri.path() == format!("/pub/{id}") {
        // Without the slash the app's relative URLs would resolve under /pub/.
        return axum::response::Redirect::permanent(&format!("/pub/{id}/{}", uri.query().map(|q| format!("?{q}")).unwrap_or_default())).into_response();
    }
    if link.kind == "file" {
        return serve_link_file(&link, uri.path(), &tail, &method, &headers).await;
    }
    if refused_port(link.port).is_some() {
        return not_found();
    }
    // The sandbox makes the app's origin opaque, so its own fetches to
    // /pub/<id>/... are cross-origin with `Origin: null`. Answer those, and
    // their preflight, so a sandboxed app still works. Never with credentials.
    let from_sandbox = headers.get(header::ORIGIN).is_some_and(|o| o == "null");
    if from_sandbox && method == Method::OPTIONS {
        let mut r = StatusCode::NO_CONTENT.into_response();
        let h = r.headers_mut();
        h.insert("access-control-allow-origin", HeaderValue::from_static("null"));
        h.insert("access-control-allow-methods", HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"));
        h.insert("access-control-allow-headers", HeaderValue::from_static("*"));
        h.insert("access-control-max-age", HeaderValue::from_static("600"));
        return r;
    }
    let Some(local) = app_relay::local_listener(link.port).await else {
        tracing::warn!(port = link.port, measured = true, n_considered = 1, verdict = "public_link_nothing_listening",
            "public link opened while nothing listens on its port");
        return app_relay::page(StatusCode::BAD_GATEWAY, "This app is not running",
            "<p>The app behind this link is not running right now. Ask the person who shared it.</p>");
    };
    // No cookie of any kind reaches a public app: amux's are the owner's, and
    // the app runs sandboxed, so it could not have set its own.
    let mut fwd = headers.clone();
    fwd.remove(header::COOKIE);
    fwd.remove(header::AUTHORIZATION);
    let mut r = app_relay::relay(local, link.port, &format!("/pub/{id}"), &tail, method, &fwd, body).await;
    if from_sandbox {
        r.headers_mut().insert("access-control-allow-origin", HeaderValue::from_static("null"));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    // Owner, 2026-10-29: workers handed him <address>/f/<path> web links for
    // every file "without me explicitly saying so". A file is named by its
    // path; the web link is for when he asks for one.
    #[test]
    fn workers_name_a_file_by_its_path_and_link_it_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(LEARNED_URL_FILE), r#"{"url":"https://desk.example:8824"}"#).unwrap();
        let g = worker_links_guide(dir.path());
        assert!(g.contains("Name each file by its absolute path"), "{g}");
        assert!(g.contains("only when the owner asks for a link"), "{g}");
        assert!(!g.contains("never a bare path"), "the old rule told workers to link every file: {g}");
        assert!(g.contains("https://desk.example:8824/f/<absolute path>"), "the link, when asked, is still built on the real address: {g}");
    }

    #[test]
    fn ids_are_random_hex_and_only_that_shape_is_accepted() {
        let a = new_id().unwrap();
        let b = new_id().unwrap();
        assert!(valid_id(&a) && valid_id(&b) && a != b);
        for bad in ["", "abc", "../../etc", &a.to_uppercase(), &format!("{a}0")] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    #[test]
    fn a_public_link_is_live_only_once_a_person_approved_it() {
        let mut l = Link { id: new_id().unwrap(), kind: "file".into(), port: 0, path: "/x".into(), title: String::new(),
            created_by: "mtp-demo".into(), created_at: 0, approved_by: String::new() };
        assert!(!l.is_live(), "asked for by a worker: pending, served to nobody");
        l.approved_by = "member:ethan@mixpeek.com".into();
        assert!(l.is_live());
        l.kind = "internal".into();
        assert!(!l.is_live(), "an internal app is never public");
        // A link saved before approvals existed reads as pending (fails closed).
        let old: Link = serde_json::from_value(json!({"id": "a", "kind": "app", "port": 8891})).unwrap();
        assert!(!old.is_live());
    }

    #[test]
    fn every_public_response_is_sandboxed_and_cannot_set_cookies() {
        let r = (StatusCode::OK, [(header::CONTENT_TYPE, "image/svg+xml"), (header::SET_COOKIE, "amux_member=evil; Path=/")], "<svg/>").into_response();
        let r = sandbox_all(r);
        assert_eq!(r.headers().get(header::CONTENT_SECURITY_POLICY).unwrap(), HTML_SANDBOX, "an SVG runs script too");
        assert_eq!(r.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
        assert!(r.headers().get(header::SET_COOKIE).is_none());
    }

    #[test]
    fn secrets_are_refused_by_name_even_unhidden() {
        for n in ["id_rsa", "id_ed25519.pub", "server.pem", "tls.key", "prod.env", "credentials.json", "my-secrets.txt",
            "app.sqlite", "terraform.tfstate", "api_token.txt"] {
            assert!(secret_like_name(n), "{n}");
            assert!(file_refused(Path::new(&format!("/home/x/share/{n}"))).is_some(), "{n}");
        }
        for n in ["index.html", "report.pdf", "styles.css", "data.csv"] {
            assert!(!secret_like_name(n), "{n}");
        }
    }

    #[test]
    fn a_link_never_reaches_local_admin_services() {
        for p in [2019, 2375, 5432, 6379, 27017, 9200, 80, 443, 1023] {
            assert!(refused_port(p).is_some(), "{p}");
        }
        assert!(refused_port(3000).is_none() && refused_port(8891).is_none());
    }

    #[test]
    fn a_link_never_reaches_amux_itself() {
        assert!(refused_port(crate::config::canonical_port()).is_some());
        assert!(refused_port(8824).is_some() && refused_port(8823).is_some());
        assert!(refused_port(8890).is_some());
        assert!(refused_port(22).is_some());
        assert!(refused_port(8891).is_none());
    }

    #[test]
    fn the_pub_path_names_its_id_and_keeps_the_page_and_query() {
        let u: Uri = "/pub/abc/x/y.js?v=1".parse().unwrap();
        assert_eq!(parse_pub(&u), Some(("abc".into(), "/x/y.js?v=1".into())));
        let u: Uri = "/pub/abc".parse().unwrap();
        assert_eq!(parse_pub(&u), Some(("abc".into(), "/".into())));
    }

    #[test]
    fn a_shared_folder_serves_only_what_is_inside_it_and_visible() {
        // Under HOME with a visible name: the OS temp dir is ".tmpXXXX" (hidden)
        // and may sit under a system prefix the file guard refuses.
        // Read HOME under the shared env lock: two session_verbs tests repoint
        // HOME at a tempdir while holding it, and under one-process cargo test
        // (rust-nightly-deep) this read used to land on that tempdir after it
        // was deleted, so tempdir_in failed NotFound (nightly runs 40286105439, 40466394121).
        let _env = crate::api::settings::test_env::LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap());
        let base = tempfile::Builder::new().prefix("pubtest-").tempdir_in(&home).unwrap();
        let root = base.path().canonicalize().unwrap().join("share");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        std::fs::write(root.join("sub/b b.txt"), "b").unwrap();
        std::fs::write(root.join(".env"), "secret").unwrap();
        std::fs::write(base.path().join("outside.txt"), "x").unwrap();
        assert_eq!(resolve_shared(&root, "/a.txt"), Some(root.join("a.txt")));
        assert_eq!(resolve_shared(&root, "/sub/b%20b.txt"), Some(root.join("sub/b b.txt")));
        assert_eq!(resolve_shared(&root, "/"), Some(root.clone()));
        for bad in ["/../outside.txt", "/sub/../../outside.txt", "/%2e%2e/outside.txt", "/.env", "/nope"] {
            assert_eq!(resolve_shared(&root, bad), None, "{bad}");
        }
        // A shared FILE serves itself and nothing beside it.
        let f = root.join("a.txt");
        assert_eq!(resolve_shared(&f, "/"), Some(f.clone()));
        assert_eq!(resolve_shared(&f, "/sub/b%20b.txt"), None);
    }

    #[test]
    fn hidden_and_amux_paths_are_never_published() {
        assert!(file_refused(Path::new("/home/x/.ssh/id_ed25519")).is_some());
        assert!(file_refused(Path::new("/home/x/proj/.git/config")).is_some());
        assert!(file_refused(Path::new("relative/path")).is_some());
        let amux = crate::config::amux_home();
        if let Ok(h) = amux.canonicalize() {
            assert!(file_refused(&h.join("server.env")).is_some());
        }
    }

    #[test]
    fn the_learned_address_is_the_owners_and_never_loopback() {
        let mut h = HeaderMap::new();
        h.insert("host", "sandbox.amux.io".parse().unwrap());
        assert_eq!(origin_of(&h).as_deref(), Some("https://sandbox.amux.io"));
        h.insert("host", "desktop.tail5ce8f5.ts.net:8824".parse().unwrap());
        assert_eq!(origin_of(&h).as_deref(), Some("https://desktop.tail5ce8f5.ts.net:8824"));
        for lo in ["localhost:8824", "127.0.0.1:8824", "[::1]:8824", "0.0.0.0"] {
            h.insert("host", lo.parse().unwrap());
            assert_eq!(origin_of(&h), None, "{lo}");
        }
        h.insert("host", "evil.com/x".parse().unwrap());
        assert_eq!(origin_of(&h), None);
    }

    #[test]
    fn the_configured_address_wins_and_is_normalized() {
        assert_eq!(normalize_url("https://sandbox.amux.io/").as_deref(), Some("https://sandbox.amux.io"));
        assert_eq!(normalize_url("sandbox.amux.io"), None);
        assert_eq!(normalize_url("https://a.b/c"), None);
    }

    #[tokio::test]
    async fn an_unknown_or_malformed_id_is_a_404_and_never_a_relay() {
        let r = serve(Method::GET, HeaderMap::new(), "/pub/0123456789abcdef0123456789abcdef/".parse().unwrap(), Body::empty()).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let r = serve(Method::GET, HeaderMap::new(), "/pub/../../api/sessions".parse().unwrap(), Body::empty()).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }
}
