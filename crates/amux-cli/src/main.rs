//! `amux-rs` — the Rust CLI (Phase 8). Ships beside the bash `amux` until
//! Phase 11 cutover renames it; verbs mirror the bash script's core surface
//! so muscle memory transfers.
//!
//! Talks to the RUST server with the shared bearer token from
//! ~/.amux/auth-token; see `resolve_url` for where it looks. Gate 409s are surfaced
//! LOUDLY with the exact retry command (the AMUX-2325 lesson: the sanctioned
//! escape must be walkable from the sanctioned tool, or agents hand-roll
//! curl and lose attribution).

use clap::{Parser, Subcommand};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(
    name = "amux-rs",
    version,
    about = "amux command-line interface (Rust server)"
)]
struct Cli {
    /// Server base URL. Falls back to $AMUX_URL, then the local server.
    #[arg(long, env = "AMUX_RS_URL")]
    url: Option<String>,
    /// Session/worker name stamped as X-Amux-Session on mutations.
    #[arg(long, env = "AMUX_SESSION")]
    session: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Board operations.
    Board {
        #[command(subcommand)]
        cmd: BoardCmd,
    },
    /// Worker operations.
    Workers {
        #[command(subcommand)]
        cmd: WorkerCmd,
    },
    /// Send a message to a worker.
    Send {
        worker: String,
        /// Message text; reads stdin when omitted (fleet convention: --stdin
        /// semantics — shell interpolation never touches piped bytes).
        text: Option<String>,
    },
    /// Schedule operations.
    Schedules {
        #[command(subcommand)]
        cmd: SchedCmd,
    },
    /// Server health.
    Health,
    /// Universal search across cards, messages, memories, workers, journal
    /// and schedules (RR-0110).
    Search {
        /// Query text. Ordinary typing: two words means AND, "quoted phrases"
        /// stay phrases, punctuation is literal.
        query: Option<String>,
        /// Restrict to entity types, comma-separated
        /// (task,message,memory,worker,journal,schedule).
        #[arg(long)]
        types: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Index health: per-type index counts vs the live tables.
        #[arg(long)]
        status: bool,
        /// Rebuild the index from the source tables and print the counts.
        #[arg(long)]
        reindex: bool,
        #[arg(long)]
        json: bool,
    },
    /// Why did this happen? Provenance over the durable trails (RR-0109).
    ///
    /// `amux-rs why task AMUX-42` · `why worker backend` · `why command cmd_…`
    /// `why schedule SCHED-108` · `why session my-lane` · `why integration gmail`
    /// `why window --since 2026-08-09T10:00:00Z`
    Why {
        /// task | worker | command | schedule | session | integration | window
        kind: String,
        /// The entity id/name. Omitted for `window`.
        id: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Print the raw JSON (every field, including per-source predicates).
        #[arg(long)]
        json: bool,
    },
    /// Route bulk file navigation through the configured fast helper model.
    Delegate {
        #[command(subcommand)]
        cmd: DelegateCmd,
    },
}

#[derive(Subcommand)]
enum BoardCmd {
    /// Add a card.
    Add {
        title: String,
        #[arg(long)]
        desc: Option<String>,
        #[arg(long, default_value = "todo")]
        status: String,
        #[arg(long)]
        r#type: Option<String>,
    },
    /// List cards.
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        session: Option<String>,
    },
    /// Show one card in full.
    Show { id: String },
    /// Move a card to done (gate-aware).
    Done {
        id: String,
        /// Acknowledge specific gate criteria as TRUE.
        #[arg(long, num_args = 1..)]
        checked: Vec<String>,
    },
    /// Move a card to doing.
    Doing { id: String },
    /// Move a card back to todo.
    Todo { id: String },
}

#[derive(Subcommand)]
enum WorkerCmd {
    List,
    Start { name: String },
    Stop { name: String },
}

#[derive(Subcommand)]
enum SchedCmd {
    List,
    Run { id: String },
}

#[derive(Subcommand)]
enum DelegateCmd {
    /// Read and compress files without spending the primary model's context.
    Read {
        #[arg(long)]
        question: String,
        /// Attach the receipt to this task. Otherwise the current task is
        /// resolved from the worker's durable message link.
        #[arg(long = "for")]
        task: Option<String>,
        #[arg(required = true, num_args = 1..)]
        files: Vec<std::path::PathBuf>,
        /// Print the complete measured server receipt.
        #[arg(long)]
        json: bool,
    },
}

struct Client {
    base: String,
    token: Option<String>,
    session: Option<String>,
    http: reqwest::blocking::Client,
}

impl Client {
    fn new(base: String, session: Option<String>) -> Self {
        let token = std::env::var("AMUX_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".amux")
            })
            .join("auth-token")
            .pipe(|p| std::fs::read_to_string(p).ok())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        Client {
            base,
            token,
            session,
            http: reqwest::blocking::Client::builder()
                // Self-signed localhost cert is the product behavior.
                .danger_accept_invalid_certs(true)
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("http client"),
        }
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::blocking::RequestBuilder {
        let mut r = self.http.request(method, format!("{}{}", self.base, path));
        if let Some(t) = &self.token {
            r = r.bearer_auth(t);
        }
        if let Some(s) = &self.session {
            r = r.header("X-Amux-Session", s);
        }
        r
    }

    fn get(&self, path: &str) -> anyhow::Result<Value> {
        Ok(self.req(reqwest::Method::GET, path).send()?.json()?)
    }

    fn send_json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Value,
    ) -> anyhow::Result<(u16, Value)> {
        let resp = self.req(method, path).json(&body).send()?;
        let status = resp.status().as_u16();
        let v = resp.json().unwrap_or(Value::Null);
        Ok((status, v))
    }
}

// Small pipe helper so token loading reads top-to-bottom.
trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

/// Restore the default SIGPIPE disposition (AMUX-2653).
///
/// Rust's runtime sets SIGPIPE to SIG_IGN before `main`, so a write to a closed
/// pipe returns EPIPE instead of killing the process. Every bare `println!` then
/// unwraps that error and panics — `amux-rs board list | head -2` exited 101 with
/// a panic message, which is the most ordinary thing a user does with a CLI.
///
/// This is the ROOT fix rather than converting the ~30 `println!` sites to the
/// `outln!` guard: it covers every existing verb AND every verb written later,
/// and it makes `amux-rs` behave like `ls`/`git`/`cat` — die quietly on EPIPE.
/// `outln!` stays for the paths that want a clean `Ok(0)` instead of death.
///
/// Safety: `signal(2)` with SIG_DFL is async-signal-safe and this runs before any
/// thread is spawned, so there is no race with another thread's signal state.
#[cfg(unix)]
fn restore_default_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}
#[cfg(not(unix))]
fn restore_default_sigpipe() {}

/// Where a client should look for the server, in order (AMUX-2672).
///
/// The old default was `https://localhost:8823` — the port the Rust server used
/// while Python still owned 8822. Python retired and the Rust server took over
/// BOTH 8822 and 8824, so nothing has listened on 8823 since, and every bare
/// `amux-rs <verb>` failed with a connection error.
///
/// That is worse than it sounds: a connection error is indistinguishable from
/// the server being down, so the CLI's own misconfiguration reads as a server
/// fault. It cost a wrong diagnosis on AMUX-2653, where "exit 1 on every verb"
/// was taken for the bug under investigation reproducing everywhere.
///
/// `$AMUX_URL` is the fix for the general case rather than a second hardcoded
/// port: every running amux session already has it in its process env, so a
/// session's CLI reaches the same server its shell does, including when that is
/// not localhost. The literal is only the last resort.
///
/// Note this is the CLIENT's default and deliberately differs from the SERVER's
/// `DEFAULT_PORT` (8823), which stays put so a dev `cargo run -p amux-server`
/// binds a free port instead of colliding with the running service.
///
/// 8824, not 8822 (2026-08-10): 8824 is what `install.sh` sets
/// (`AMUX_RS_PORT`, the launchd agent's port) and therefore the port a working
/// install answers on. 8822 is the RETIRED address, kept alive only by a
/// countdown bind for pre-cutover processes whose env cannot be rotated
/// (`amux_server::legacy_port`). A client default pointing at a port scheduled
/// for deletion is a connection error with a date on it — and per the incident
/// above, a connection error from the CLIENT's own misconfiguration reads as
/// the server being down.
const DEFAULT_CLIENT_URL: &str = "https://localhost:8824";

fn resolve_url(explicit: Option<String>) -> String {
    [explicit, std::env::var("AMUX_URL").ok()]
        .into_iter()
        .flatten()
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .find(|u| !u.is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_URL.to_string())
}

fn main() {
    restore_default_sigpipe();
    let cli = Cli::parse();
    let client = Client::new(resolve_url(cli.url.clone()), cli.session.clone());
    let result = run(&cli.cmd, &client);
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn run(cmd: &Cmd, c: &Client) -> anyhow::Result<i32> {
    match cmd {
        Cmd::Health => {
            let v = c.get("/health")?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(0)
        }
        Cmd::Board { cmd } => board(cmd, c),
        Cmd::Workers { cmd } => workers(cmd, c),
        Cmd::Schedules { cmd } => schedules(cmd, c),
        Cmd::Delegate { cmd } => delegate(cmd, c),
        Cmd::Search {
            query,
            types,
            limit,
            status,
            reindex,
            json,
        } => search(
            query.as_deref(),
            types.as_deref(),
            *limit,
            *status,
            *reindex,
            *json,
            c,
        ),
        Cmd::Why {
            kind,
            id,
            since,
            until,
            limit,
            json,
        } => why(
            kind,
            id.as_deref(),
            since.as_deref(),
            until.as_deref(),
            *limit,
            *json,
            c,
        ),
        Cmd::Send { worker, text } => {
            let body_text = match text {
                Some(t) => t.clone(),
                None => {
                    use std::io::Read;
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf)?;
                    buf
                }
            };
            let (status, v) = c.send_json(
                reqwest::Method::POST,
                "/api/messages",
                json!({"target": {"worker_name": worker}, "body": body_text}),
            )?;
            if (200..300).contains(&status) {
                println!("sent to {worker}");
                Ok(0)
            } else {
                eprintln!("send failed ({status}): {v}");
                Ok(3)
            }
        }
    }
}

fn delegate(cmd: &DelegateCmd, c: &Client) -> anyhow::Result<i32> {
    match cmd {
        DelegateCmd::Read {
            question,
            task,
            files,
            json: as_json,
        } => {
            if question.trim().is_empty() {
                eprintln!("delegate read requires a non-empty --question");
                return Ok(2);
            }
            if files.len() > 16 {
                eprintln!("delegate read accepts at most 16 files");
                return Ok(2);
            }
            let mut total = 0usize;
            let mut payload = Vec::with_capacity(files.len());
            for path in files {
                let bytes = std::fs::read(path)?;
                total = total.saturating_add(bytes.len());
                if total > 512 * 1024 {
                    eprintln!("delegate read file content exceeds the 512 KiB request limit");
                    return Ok(2);
                }
                let content = String::from_utf8(bytes)
                    .map_err(|_| anyhow::anyhow!("{} is not UTF-8 text", path.display()))?;
                payload.push(json!({"path": path.to_string_lossy(), "content": content}));
            }
            let mut body = json!({"question": question, "files": payload});
            if let Some(task) = task {
                body["task"] = json!(task);
            }
            let response = c
                .req(reqwest::Method::POST, "/api/lookup/bulk")
                .timeout(std::time::Duration::from_secs(100))
                .json(&body)
                .send()?;
            let status = response.status().as_u16();
            let value: Value = response.json().unwrap_or(Value::Null);
            if !(200..300).contains(&status) {
                eprintln!(
                    "delegate read failed ({status}): {}",
                    value["error"]
                        .as_str()
                        .unwrap_or("non-JSON or empty response")
                );
                return Ok(3);
            }
            if *as_json {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("{}", value["text"].as_str().unwrap_or(""));
                eprintln!(
                    "amux delegate: verdict=completed via={} files={} lines={} bytes={} elapsed_ms={} board={}:{}",
                    value["via"].as_str().unwrap_or("?"),
                    value["n_considered"],
                    value["input_lines"],
                    value["input_bytes"],
                    value["elapsed_ms"],
                    value["board_evidence"]["verdict"].as_str().unwrap_or("?"),
                    value["board_evidence"]["card_id"].as_str().unwrap_or("-")
                );
            }
            Ok(0)
        }
    }
}

fn board(cmd: &BoardCmd, c: &Client) -> anyhow::Result<i32> {
    match cmd {
        BoardCmd::Add {
            title,
            desc,
            status,
            r#type,
        } => {
            let mut body = json!({"title": title, "status": status});
            if let Some(d) = desc {
                body["desc"] = json!(d);
            }
            if let Some(t) = r#type {
                body["type"] = json!(t);
            }
            if let Some(s) = &c.session {
                body["session"] = json!(s);
            }
            let (code, v) = c.send_json(reqwest::Method::POST, "/api/board", body)?;
            if code == 201 {
                println!("{} → {}", v["id"].as_str().unwrap_or("?"), status);
                Ok(0)
            } else {
                eprintln!("create failed ({code}): {v}");
                Ok(3)
            }
        }
        BoardCmd::List { status, session } => {
            let mut path = "/api/board?done_limit=100".to_string();
            if let Some(s) = status {
                path.push_str(&format!("&status={s}"));
            }
            if let Some(s) = session {
                path.push_str(&format!("&session={s}"));
            }
            let v = c.get(&path)?;
            for item in v.as_array().unwrap_or(&vec![]) {
                println!(
                    "{:<12} {:<9} {:<18} {}",
                    item["id"].as_str().unwrap_or("?"),
                    item["status"].as_str().unwrap_or("?"),
                    item["session"].as_str().unwrap_or("-"),
                    item["title"].as_str().unwrap_or("")
                );
            }
            Ok(0)
        }
        BoardCmd::Show { id } => {
            let v = c.get(&format!("/api/board/{id}"))?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(0)
        }
        BoardCmd::Done { id, checked } => move_status(c, id, "done", checked),
        BoardCmd::Doing { id } => move_status(c, id, "doing", &[]),
        BoardCmd::Todo { id } => move_status(c, id, "todo", &[]),
    }
}

/// Status move with LOUD gate handling: a 409 prints the criteria and the
/// exact retry command instead of a silent bounce (AMUX-1769/2325 lessons).
fn move_status(c: &Client, id: &str, status: &str, checked: &[String]) -> anyhow::Result<i32> {
    let mut body = json!({"status": status});
    if !checked.is_empty() {
        body["gate_checked"] = json!(checked);
    }
    let (code, v) = c.send_json(reqwest::Method::PATCH, &format!("/api/board/{id}"), body)?;
    if (200..300).contains(&code) {
        println!("{id} → {status}");
        return Ok(0);
    }
    if code == 409 && v["gate"].is_array() {
        eprintln!("{}", serde_json::to_string_pretty(&v)?);
        eprintln!("\nSatisfy these, then acknowledge the ones that are TRUE:");
        let gate: Vec<String> = v["gate"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|g| g.as_str().map(String::from))
            .collect();
        for g in &gate {
            eprintln!("   [ ] {g}");
        }
        let quoted: Vec<String> = gate.iter().map(|g| format!("{g:?}")).collect();
        eprintln!("\n  amux-rs board done {id} --checked {}", quoted.join(" "));
        return Ok(3);
    }
    eprintln!("move failed ({code}): {v}");
    Ok(3)
}

fn workers(cmd: &WorkerCmd, c: &Client) -> anyhow::Result<i32> {
    match cmd {
        WorkerCmd::List => {
            let v = c.get("/api/workers")?;
            for w in v["items"].as_array().unwrap_or(&vec![]) {
                println!(
                    "{:<22} {:<10} {:<8} {}",
                    w["display_name"].as_str().unwrap_or("?"),
                    w["state"]["state"].as_str().unwrap_or("?"),
                    w["provider"].as_str().unwrap_or("?"),
                    w["id"].as_str().unwrap_or("")
                );
            }
            Ok(0)
        }
        WorkerCmd::Start { name } => {
            let (code, v) = c.send_json(
                reqwest::Method::POST,
                &format!("/api/workers/{name}/start"),
                json!({}),
            )?;
            if code == 202 {
                println!("{name} starting");
                Ok(0)
            } else {
                eprintln!("start failed ({code}): {v}");
                Ok(3)
            }
        }
        WorkerCmd::Stop { name } => {
            let (code, v) = c.send_json(
                reqwest::Method::POST,
                &format!("/api/workers/{name}/stop"),
                json!({}),
            )?;
            if (200..300).contains(&code) {
                println!("{name} stopped");
                Ok(0)
            } else {
                eprintln!("stop failed ({code}): {v}");
                Ok(3)
            }
        }
    }
}

fn schedules(cmd: &SchedCmd, c: &Client) -> anyhow::Result<i32> {
    match cmd {
        SchedCmd::List => {
            let v = c.get("/api/schedules")?;
            let items = v
                .as_array()
                .cloned()
                .unwrap_or_else(|| v["items"].as_array().cloned().unwrap_or_default());
            for s in items {
                println!(
                    "{:<10} {:<3} {:<24} {}",
                    s["id"].as_str().unwrap_or("?"),
                    if s["enabled"].as_i64().unwrap_or(0) == 1 {
                        "on"
                    } else {
                        "off"
                    },
                    s["schedule_expr"].as_str().unwrap_or("?"),
                    s["title"].as_str().unwrap_or("")
                );
            }
            Ok(0)
        }
        SchedCmd::Run { id } => {
            let (code, v) = c.send_json(
                reqwest::Method::POST,
                &format!("/api/schedules/{id}/run"),
                json!({}),
            )?;
            if (200..300).contains(&code) {
                println!("{id} fired");
                Ok(0)
            } else {
                // Shadow-mode 409 is EXPECTED while the Python scheduler owns
                // firing — print the server's own explanation.
                eprintln!("run refused ({code}): {v}");
                Ok(3)
            }
        }
    }
}

/// Print a line, stopping the command cleanly when the reader has gone away.
///
/// `println!` PANICS on EPIPE (Rust ignores SIGPIPE, so the write returns
/// `BrokenPipe` and the macro unwraps it), which means `amux-rs why schedule
/// SCHED-30 | head` exits **101 with a panic message** instead of 0 — measured
/// on a real schedule with 220 output lines. A CLI that appears to crash when
/// you pipe it into `head` teaches you not to trust its other exit codes,
/// which is the opposite of what the `why` verdict codes are for.
macro_rules! outln {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let mut h = std::io::stdout().lock();
        if writeln!(h, $($arg)*).is_err() {
            // Reader closed the pipe: that is a normal end, not a failure.
            return Ok(0);
        }
    }};
}

// ---------------------------------------------------------------------------
// RR-0110 — search
// ---------------------------------------------------------------------------

/// Minimal percent-encoding for a query-string VALUE. Everything that is not
/// unreserved gets escaped, so a query containing `&`, `#`, `+` or a space
/// reaches the server as typed. (Getting this wrong is silent: `a&b` would
/// arrive as a stray parameter and the search would quietly run on `a`.)
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn search(
    query: Option<&str>,
    types: Option<&str>,
    limit: usize,
    status: bool,
    reindex: bool,
    as_json: bool,
    c: &Client,
) -> anyhow::Result<i32> {
    if reindex {
        let (code, v) = c.send_json(reqwest::Method::POST, "/api/search/reindex", json!({}))?;
        if !(200..300).contains(&code) {
            eprintln!("reindex failed ({code}): {v}");
            return Ok(3);
        }
        outln!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    if status {
        let v = c.get("/api/search/status")?;
        if as_json {
            outln!("{}", serde_json::to_string_pretty(&v)?);
            return Ok(0);
        }
        let consistent = v["consistent"].as_bool().unwrap_or(false);
        outln!(
            "index: {} docs, {} fts rows — {}",
            v["docs_total"],
            v["fts_rows"],
            if consistent { "consistent" } else { "DRIFTED" }
        );
        for f in v["families"].as_array().cloned().unwrap_or_default() {
            outln!(
                "  {:<10} indexed={:<7} live={:<7} {}",
                f["type"].as_str().unwrap_or("?"),
                f["indexed"],
                f["live"],
                if f["consistent"].as_bool().unwrap_or(false) {
                    "ok"
                } else {
                    "MISMATCH"
                }
            );
        }
        // A drifted index is a failure the caller should be able to branch on,
        // not a line of output they have to read.
        return Ok(if consistent { 0 } else { 4 });
    }

    let Some(q) = query else {
        eprintln!(
            "usage: amux-rs search <query> [--types t1,t2] [--limit N] | --status | --reindex"
        );
        return Ok(2);
    };
    let mut path = format!("/api/search?q={}&limit={limit}", urlencode(q));
    if let Some(t) = types {
        path.push_str(&format!("&types={}", urlencode(t)));
    }
    let v = c.get(&path)?;
    if as_json {
        outln!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    let hits = v["hits"].as_array().cloned().unwrap_or_default();
    let total = v["total"].as_i64().unwrap_or(0);
    let capped = v["total_capped"].as_bool().unwrap_or(false);
    outln!(
        "{} hit(s){} in {}ms",
        total,
        if capped { "+ (count capped)" } else { "" },
        v["took_ms"].as_u64().unwrap_or(0)
    );
    for h in &hits {
        outln!(
            "{:<9} {:<22} {}",
            h["type"].as_str().unwrap_or("?"),
            h["id"].as_str().unwrap_or("?"),
            h["title"].as_str().unwrap_or("")
        );
        // The snippet arrives HTML-escaped with <mark> around matches; a
        // terminal wants neither, so unwrap both here rather than asking the
        // server for a second rendering of the same text.
        let snip = h["snippet"]
            .as_str()
            .unwrap_or("")
            .replace("<mark>", "[")
            .replace("</mark>", "]")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&amp;", "&")
            .replace('\n', " ");
        if !snip.trim().is_empty() {
            outln!("          {snip}");
        }
    }
    if hits.is_empty() {
        // "No results" from a healthy index and "no results" from an index
        // that stopped being maintained look identical, so say which one.
        let st = c.get("/api/search/status")?;
        if !st["consistent"].as_bool().unwrap_or(false) {
            eprintln!(
                "warning: the search index is DRIFTED from the source tables — this empty result may not mean 'no matches'. Run `amux-rs search --status` for the per-type counts."
            );
            return Ok(4);
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// RR-0109 — why
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn why(
    kind: &str,
    id: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
    as_json: bool,
    c: &Client,
) -> anyhow::Result<i32> {
    // The CLI does NO correlation of its own: it asks the server the same
    // question the dashboard would and prints the answer. Two implementations
    // of the same joins is two places for the story to be wrong.
    let path = if kind == "window" {
        let mut p = format!("/api/why?limit={limit}");
        if let Some(s) = since {
            p.push_str(&format!("&since={}", urlencode(s)));
        }
        if let Some(u) = until {
            p.push_str(&format!("&until={}", urlencode(u)));
        }
        p
    } else {
        let Some(id) = id else {
            eprintln!("usage: amux-rs why <task|worker|command|schedule|session|integration> <id>");
            eprintln!("       amux-rs why window [--since T] [--until T]");
            return Ok(2);
        };
        format!("/api/why/{kind}/{}", urlencode(id))
    };
    let v = c.get(&path)?;
    if as_json {
        outln!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    if let Some(err) = v["error"].as_str() {
        eprintln!("error: {err}");
        if let Some(kinds) = v["kinds"].as_array() {
            eprintln!(
                "kinds: {}",
                kinds
                    .iter()
                    .filter_map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        return Ok(2);
    }

    let verdict = v["verdict"].as_str().unwrap_or("?");
    outln!("subject: {}", serde_json::to_string(&v["subject"])?);
    outln!(
        "verdict: {verdict} — {}",
        v["verdict_reason"].as_str().unwrap_or("")
    );
    outln!();

    outln!("timeline:");
    for e in v["timeline"].as_array().cloned().unwrap_or_default() {
        let at = e["at"].as_str().unwrap_or("(no recorded time)");
        let actor = e["actor"]
            .as_str()
            .map(|a| format!(" [{a}]"))
            .unwrap_or_default();
        let src = &e["source"];
        // Every line carries its table, which is the difference between a
        // story and a checkable claim.
        // 33 = the widest RFC3339 this server emits
        // ("2026-08-10T01:47:07.483327+00:00" is 32). Narrower and the
        // columns shear on exactly the rows with sub-second precision, which
        // are the ones you are reading when two events share a second.
        outln!(
            "  {:<33}{:<14}{}{}  <- {}",
            at,
            e["kind"].as_str().unwrap_or("?"),
            e["summary"].as_str().unwrap_or(""),
            actor,
            serde_json::to_string(src)?
        );
    }

    outln!("\nsources consulted (including the ones that found nothing):");
    for s in v["sources"].as_array().cloned().unwrap_or_default() {
        outln!(
            "  {:<24} rows={:<6} total={:<6} {}",
            s["table"].as_str().unwrap_or("?"),
            s["rows"],
            s["rows_total"],
            s["query"].as_str().unwrap_or("")
        );
        if let Some(note) = s["note"].as_str() {
            outln!("      note: {note}");
        }
    }

    let gaps = v["gaps"].as_array().cloned().unwrap_or_default();
    if !gaps.is_empty() {
        outln!("\nwhat this CANNOT tell you:");
        for g in &gaps {
            outln!("  - {}", g.as_str().unwrap_or(""));
        }
    }
    // Exit codes are the machine-readable verdict: 0 explained, 5 partial,
    // 6 cannot_tell. A script must not have to parse prose to learn that the
    // answer was "I do not know".
    Ok(match verdict {
        "explained" => 0,
        "partial" => 5,
        _ => 6,
    })
}
