//! Manual real-provider Chat UI rig. No fleet reconciliation or background jobs.
//! See docs/codex-chat-e2e.md; never point AMUX_HOME at the owner's live state.
use amux_server::api::{AppState, router};
use amux_server::db::Store;
use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

#[tokio::main]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter("amux_server=info")
        .init();
    let home = PathBuf::from(std::env::var("AMUX_HOME").expect("private AMUX_HOME required"));
    let owner_home = PathBuf::from(std::env::var("HOME").unwrap()).join(".amux");
    std::fs::create_dir_all(&home).unwrap();
    assert_ne!(
        home.canonicalize().unwrap(),
        owner_home.canonicalize().unwrap(),
        "refusing live state"
    );
    let port: u16 = std::env::var("AMUX_RS_PORT")
        .expect("test port required")
        .parse()
        .unwrap();
    assert!(
        ![amux_server::config::DEFAULT_PORT, 8824].contains(&port),
        "refusing production port"
    );
    let state = AppState {
        store: Arc::new(Store::open(&home.join("test.db")).unwrap()),
        started: Instant::now(),
        build_hash: "codex-chat-e2e-private".into(),
        auth_token: Some(
            std::fs::read_to_string(owner_home.join("auth_token"))
                .unwrap()
                .trim()
                .into(),
        ),
        reconciled: Arc::new(AtomicBool::new(true)),
    };
    // The existing TLS resolver loads the owner's already-trusted SNI certificate
    // read-only. This neither installs trust nor changes any live key/certificate.
    let tls = amux_server::tls::build_server_config(&owner_home.join("tls")).unwrap();
    let tls = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls));
    let recovered = amux_server::api::chat_worker::recover_all(&state).await;
    tracing::info!(?recovered, port, "private Chat E2E rig ready");
    axum_server::bind_rustls(([0, 0, 0, 0], port).into(), tls)
        .serve(router(state).into_make_service_with_connect_info::<std::net::SocketAddr>())
        .await
        .unwrap();
}
