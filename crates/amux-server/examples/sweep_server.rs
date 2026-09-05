//! ROUTER-ONLY sweep harness (RR-0130 / RR-0131a / RR-0131b / RR-0131d).
//!
//! The live-data acceptance sweeps have to answer one question: does the Rust
//! API expose every row the shared DB holds, in the shape the SPA reads? That
//! requires pointing a server at a COPY of the live DB **and at the real
//! `$AMUX_HOME`** — because a large share of the surface (session memory,
//! steering history, browser-profile inventory, the `amux` flag on token
//! rows) is keyed off `$AMUX_HOME/sessions/*.env`, `$AMUX_HOME/memory/` and
//! `$AMUX_HOME/playwright-auth/`. Run with an isolated home and every one of
//! those surfaces answers empty — which reads exactly like a porting gap and
//! is really just the harness blindfolding itself.
//!
//! WHY THIS EXISTS AND NOT `amux-server` ITSELF. The real binary spawns three
//! loops that DRIVE THE LIVE FLEET, and all three enumerate their targets from
//! `$AMUX_HOME/sessions/*.env`:
//!
//! - `steer_deliver_loop` -> `send_text_inner` -> real tmux keystrokes into
//!   whichever lane a `steering_queue` row names (the copy DB has 16 such rows
//!   pointing at live lanes);
//! - `ghost_rescue` -> presses Enter in any lane whose pane shows an
//!   unsubmitted amux message;
//! - `board_drive` -> auto-pickup + advance nudges to every lane holding a card.
//!
//! Point the real binary at the real home and those three fire against the
//! running fleet using a STALE COPY of the board as their input. So the sweep
//! server is the router and nothing else: `api::router(AppState)` has no
//! `tokio::spawn` anywhere in it (verified against api/mod.rs), which makes
//! "cannot touch the fleet" a structural property of the process rather than a
//! promise about which requests I remembered not to send.
//!
//! It is still not a toy: it is the SAME `api::router` composition the
//! production binary serves, over the same `Store`, so a shape or count
//! difference found here is a real difference. What it deliberately cannot
//! exercise is behaviour that only the background loops produce — those belong
//! to the restart suite (`tests/restart_persistence.rs`), which drives the real
//! binary against a scratch DB where the loops have nothing live to act on.
//!
//! Plain HTTP on purpose (the production binary is HTTPS): TLS here would only
//! add a self-signed cert to every curl in the sweep and prove nothing about
//! data exposure.
//!
//! Usage:
//!   cargo run -p amux-server --example sweep_server -- <db-path> <port>
//!
//! The DB path MUST be a `.backup` copy. The harness refuses to open the live
//! database by path so a mistyped argument cannot turn a read-only sweep into
//! a write against real user data (ethos rule 8).

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use std::sync::Arc;
use std::time::Instant;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let db = args.get(1).cloned().unwrap_or_else(|| {
        eprintln!("usage: sweep_server <db-path> <port>");
        std::process::exit(2);
    });
    let port: u16 = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(19001);

    // REFUSE the live DB. Not a style check — the whole method depends on the
    // live file never being opened read-write by this process, and an argument
    // is exactly the kind of thing that gets copy-pasted wrong at 2am.
    let canon = std::fs::canonicalize(&db).unwrap_or_else(|e| {
        eprintln!("cannot resolve {db}: {e}");
        std::process::exit(2);
    });
    let live = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".amux")
        .join("amux.db");
    if canon == live {
        eprintln!(
            "REFUSING to open the LIVE database ({}).\n\
             Take a copy first:  sqlite3 ~/.amux/amux.db \".backup '/tmp/copy.db'\"",
            canon.display()
        );
        std::process::exit(2);
    }

    let store = Arc::new(Store::open(&canon).expect("open store"));
    let state = AppState {
        secrets: std::sync::Arc::new(amux_server::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
        store,
        started: Instant::now(),
        build_hash: amux_server::build_hash(),
        auth_token: None, // local read-only harness
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("bind");
    eprintln!(
        "sweep_server: http://127.0.0.1:{port}  db={}  AMUX_HOME={}",
        canon.display(),
        std::env::var("AMUX_HOME").unwrap_or_else(|_| "(default ~/.amux)".into())
    );
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .expect("serve");
}
