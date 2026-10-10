//! Private production-router rig for connectors E2E. No fleet jobs.
//! Never point AMUX_HOME at the owner's live state. The fixture routes serve only synthetic data.
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
        build_hash: "connectors-flow-e2e-private".into(),
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
    tracing::info!(port, "private connectors E2E rig ready");
    axum_server::bind_rustls(([0, 0, 0, 0], port).into(), tls)
        .serve(
            router(state)
                .merge(
                    axum::Router::new()
                        .route("/e2e/authorize", axum::routing::get(authorize))
                        .route("/e2e/approve", axum::routing::get(approve)),
                )
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
}

async fn authorize(
    axum::extract::Query(q): axum::extract::Query<std::collections::BTreeMap<String, String>>,
) -> axum::response::Html<String> {
    let hidden = q
        .iter()
        .map(|(k, v)| {
            format!(
                "<input type=hidden name=\"{}\" value=\"{}\">",
                amux_server::integrations::email::html_escape(k),
                amux_server::integrations::email::html_escape(v)
            )
        })
        .collect::<String>();
    axum::response::Html(format!(
        "<h1>Synthetic OAuth provider</h1><p>Test accounts only. Read access to synthetic invoices.</p><form action=/e2e/approve>{hidden}<label>Fixture identity<select name=identity><option>alice</option><option>beth</option></select></label><button>Approve fixture</button></form>"
    ))
}
async fn approve(
    axum::extract::Query(q): axum::extract::Query<std::collections::BTreeMap<String, String>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut endpoint = reqwest::Url::parse(
        &std::env::var("AMUX_E2E_PROVIDER_ORIGIN").expect("fixture provider origin"),
    )
    .unwrap();
    assert_eq!(endpoint.host_str(), Some("127.0.0.1"));
    endpoint.set_path("/issue");
    endpoint.query_pairs_mut().extend_pairs(&q);
    let body: serde_json::Value = reqwest::Client::new()
        .get(endpoint)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut redirect = reqwest::Url::parse(q.get("redirect_uri").unwrap()).unwrap();
    redirect.query_pairs_mut().extend_pairs([
        ("code", body["code"].as_str().unwrap()),
        ("state", q.get("state").unwrap()),
    ]);
    axum::response::Redirect::to(redirect.as_str()).into_response()
}
