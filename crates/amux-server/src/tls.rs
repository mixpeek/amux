//! Self-signed TLS (RR-0022).
//!
//! Certificate + key persist under `~/.amux/tls/` so browsers that accepted
//! the cert once keep working across restarts. Regenerated automatically
//! when missing or unreadable.

use std::path::Path;

pub struct TlsMaterial {
    pub cert_pem: String,
    pub key_pem: String,
}

pub fn load_or_generate(dir: &Path) -> anyhow::Result<TlsMaterial> {
    // One atomic bundle is authoritative after a validated UI installation.
    let bundle = dir.join("connection.pem");
    if bundle.exists() {
        let pem = std::fs::read_to_string(bundle)?;
        load_certified_key(&pem, &pem)?;
        return Ok(TlsMaterial { cert_pem: pem.clone(), key_pem: pem });
    }
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    if let (Ok(cert_pem), Ok(key_pem)) = (
        std::fs::read_to_string(&cert_path),
        std::fs::read_to_string(&key_path),
    ) {
        if !cert_pem.is_empty() && !key_pem.is_empty() {
            return Ok(TlsMaterial { cert_pem, key_pem });
        }
    }
    let mut params = rcgen::CertificateParams::new(vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
    ])?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "amux");
    let key_pair = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;
    let material = TlsMaterial {
        cert_pem: cert.pem(),
        key_pem: key_pair.serialize_pem(),
    };
    std::fs::create_dir_all(dir)?;
    std::fs::write(&cert_path, &material.cert_pem)?;
    std::fs::write(&key_path, &material.key_pem)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(material)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_then_reuses() {
        let dir = std::env::temp_dir().join(format!("amux-tls-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let m1 = load_or_generate(&dir).unwrap();
        assert!(m1.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(m1.key_pem.contains("PRIVATE KEY"));
        let m2 = load_or_generate(&dir).unwrap();
        assert_eq!(m1.cert_pem, m2.cert_pem, "must reuse persisted cert");
        std::fs::remove_dir_all(&dir).ok();
    }
}

// ---------------------------------------------------------------------------
// SNI dual-cert serving (Tailscale parity with the Python server)
// ---------------------------------------------------------------------------

use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::sync::Arc;

/// SNI resolver: the REAL Tailscale Let's Encrypt cert for the tailnet
/// hostname, the self-signed fallback for localhost/IPs — byte-for-byte the
/// Python server's `_sni_cb` behavior (amux-server.py:77931), so
/// https://desktop.tail5ce8f5.ts.net:8824 carries a browser-trusted cert
/// and the service worker can register.
#[derive(Debug)]
pub struct SniCerts {
    pub fallback: Arc<CertifiedKey>,
    pub ts_hostname: Option<String>,
    pub ts_cert: Option<Arc<CertifiedKey>>,
}

impl ResolvesServerCert for SniCerts {
    fn resolve(&self, hello: ClientHello) -> Option<Arc<CertifiedKey>> {
        if let (Some(name), Some(ts), Some(cert)) =
            (hello.server_name(), &self.ts_hostname, &self.ts_cert)
        {
            if name.eq_ignore_ascii_case(ts) {
                return Some(cert.clone());
            }
        }
        Some(self.fallback.clone())
    }
}

fn load_certified_key(cert_pem: &str, key_pem: &str) -> anyhow::Result<CertifiedKey> {
    let certs: Vec<_> = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .collect::<Result<_, _>>()?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())?
        .ok_or_else(|| anyhow::anyhow!("no private key in PEM"))?;
    for cert in &certs {
        rustls::server::ParsedCertificate::try_from(cert).map_err(|_| anyhow::anyhow!("certificate_invalid"))?;
    }
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| anyhow::anyhow!("unsupported key type: {e}"))?;
    let certified = CertifiedKey::new(certs, signing_key);
    certified.keys_match().map_err(|_| anyhow::anyhow!("certificate_key_mismatch"))?;
    Ok(certified)
}

/// Build the full rustls ServerConfig: self-signed fallback always; the
/// Tailscale cert layered in when `<host>.ts.net.crt/.key` exist in the TLS
/// dir (the same files `tailscale cert` writes and the Python server loads).
pub fn build_server_config(dir: &std::path::Path) -> anyhow::Result<rustls::ServerConfig> {
    let (config, active) = prepare_server_config(dir)?;
    *active_certificates().write().expect("TLS state lock") = Some(active);
    tracing::info!(target: "amux::tls", verdict="connection_certificate_loaded", "TLS resolver adopted certificates; client trust remains unmeasured");
    Ok(config)
}
fn prepare_server_config(dir: &Path) -> anyhow::Result<(rustls::ServerConfig, ActiveCertificates)> {
    let material = load_or_generate(dir)?;
    let fallback = Arc::new(load_certified_key(&material.cert_pem, &material.key_pem)?);

    let mut ts_hostname = None;
    let mut ts_cert = None;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(host) = name.strip_suffix(".crt") {
                if host.contains(".ts.net") {
                    let key_path = dir.join(format!("{host}.key"));
                    if let (Ok(c), Ok(k)) = (
                        std::fs::read_to_string(e.path()),
                        std::fs::read_to_string(&key_path),
                    ) {
                        match load_certified_key(&c, &k) {
                            Ok(ck) => {
                                tracing::info!(host, "tailscale cert loaded for SNI");
                                ts_hostname = Some(host.to_string());
                                ts_cert = Some(Arc::new(ck));
                            }
                            Err(err) => {
                                tracing::warn!(host, error = %err, "tailscale cert unusable — fallback only");
                            }
                        }
                    }
                }
            }
        }
    }

    // Capture precisely the certificates adopted by this resolver, never infer
    // active state (or browser trust) from files which may change after startup.
    let active = ActiveCertificates {
        fallback: Some(certificate_metadata(&material.cert_pem).unwrap_or_else(|_| {
            tracing::warn!(target:"amux::tls", verdict="connection_certificate_metadata_unmeasured", "TLS key loaded but public date/issuer metadata could not be inspected");
            serde_json::json!({"sha256":fingerprint(&fallback.cert[0]),"metadata_measured":false,"trust":"unknown_to_server"})
        })),
        fallback_pem: public_certificate_pem(&material.cert_pem).unwrap_or_default(),
        tailscale: ts_hostname.as_ref().zip(ts_cert.as_ref()).map(|(host, cert)| {
            serde_json::json!({"hostname": host, "sha256": fingerprint(&cert.cert[0])})
        }),
    };
    let mut cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(SniCerts {
            fallback,
            ts_hostname,
            ts_cert,
        }));
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok((cfg, active))
}

// ---------------------------------------------------------------------------
// Plain-HTTP redirect on the TLS port (RR-0022's redirect requirement)
// ---------------------------------------------------------------------------

/// Build the 301 for a plain-HTTP request that arrived on the TLS port.
/// Without this, `http://host:8824/` receives raw TLS bytes and the browser
/// shows ERR_INVALID_HTTP_RESPONSE (Ethan hit exactly this). Pure so it is
/// testable: parses the request head for Host + path, falls back to the
/// listener's own address when absent.
pub fn http_redirect_response(head: &[u8], fallback_host: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.lines();
    let path = lines
        .next()
        .and_then(|req| req.split_whitespace().nth(1))
        .unwrap_or("/");
    let host = lines
        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim(), v.trim())))
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| fallback_host.to_string());
    let location = format!("https://{host}{path}");
    format!(
        "HTTP/1.1 301 Moved Permanently\r\nLocation: {location}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// Extract the request path (with query) from a raw HTTP request head.
pub fn http_head_path(head: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(head);
    text.lines().next().and_then(|req| req.split_whitespace().nth(1)).map(str::to_string)
}

/// OAuth callback paths must COMPLETE on the plain-HTTP leg (AMUX-3427).
/// Google redirects the browser to the registered `http://localhost:<port>`
/// URI; answering that with a 301 into https strands the consent tab on the
/// self-signed-cert interstitial and the grant silently never lands — the
/// exact incident: the user pasted the stranded URL by hand and the exchange
/// only completed because a human curl'd it. These paths are deliberately
/// public (single-use state is the guard), so serving them over loopback
/// plain HTTP adds no exposure the registered http URI did not already have.
pub fn is_oauth_callback_path(path: &str) -> bool {
    let bare = path.split('?').next().unwrap_or(path);
    if bare.contains("..") {
        return false;
    }
    bare == "/api/gmail/callback"
        || (bare.starts_with("/api/connectors/") && bare.ends_with("/callback"))
}

#[cfg(test)]
mod redirect_tests {
    use super::*;

    #[test]
    fn redirects_preserve_host_and_path() {
        let head = b"GET /board?x=1 HTTP/1.1\r\nHost: desktop.tail5ce8f5.ts.net:8824\r\nAccept: */*\r\n\r\n";
        let resp = String::from_utf8(http_redirect_response(head, "fallback:1")).unwrap();
        assert!(resp.starts_with("HTTP/1.1 301"));
        assert!(resp.contains("Location: https://desktop.tail5ce8f5.ts.net:8824/board?x=1"));
    }

    #[test]
    fn missing_host_uses_fallback() {
        let resp = String::from_utf8(http_redirect_response(b"GET / HTTP/1.0\r\n\r\n", "127.0.0.1:8824")).unwrap();
        assert!(resp.contains("Location: https://127.0.0.1:8824/"));
    }

    #[test]
    fn oauth_callbacks_are_recognised_and_nothing_else_is() {
        // These must be SERVED on the plain leg (AMUX-3427: a 301 into https
        // strands the consent tab on the cert interstitial).
        assert!(is_oauth_callback_path("/api/gmail/callback?state=x&code=y"));
        assert!(is_oauth_callback_path("/api/gmail/callback"));
        assert!(is_oauth_callback_path("/api/connectors/google/callback?code=y"));
        assert!(is_oauth_callback_path("/api/connectors/slack/callback"));
        // Everything else keeps the https redirect — the carve-out must not
        // quietly grow into serving the whole API over plain HTTP.
        assert!(!is_oauth_callback_path("/"));
        assert!(!is_oauth_callback_path("/board"));
        assert!(!is_oauth_callback_path("/api/sessions"));
        assert!(!is_oauth_callback_path("/api/gmail/accounts"));
        assert!(!is_oauth_callback_path("/api/connectors/google/token"));
        assert!(!is_oauth_callback_path("/api/gmail/callback/../../sessions"));
        assert!(!is_oauth_callback_path("/api/connectors/../admin/callback"));
    }

    #[test]
    fn head_path_parses_the_request_line() {
        assert_eq!(
            http_head_path(b"GET /api/gmail/callback?state=x HTTP/1.1\r\nHost: h\r\n\r\n").as_deref(),
            Some("/api/gmail/callback?state=x")
        );
        assert_eq!(http_head_path(b"").as_deref(), None);
    }
}

/// Complete an OAuth callback that arrived on the plain-HTTP leg by replaying
/// it against our own TLS listener on loopback (cert unverified: it is our own
/// self-signed cert, and the hop never leaves the machine). The response body
/// is the callback's own HTML ("✓ connected" / the error page), so the consent
/// tab finishes without the browser ever crossing the interstitial.
async fn proxy_oauth_callback(path: &str, fallback_host: &str) -> Result<Vec<u8>, String> {
    let port = fallback_host.rsplit(':').next().and_then(|p| p.parse::<u16>().ok())
        .unwrap_or_else(crate::config::canonical_port);
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let res = client
        .get(format!("https://127.0.0.1:{port}{path}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = res.status();
    let body = res.text().await.map_err(|e| e.to_string())?;
    Ok(format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("OK"),
        body.len(),
        body
    )
    .into_bytes())
}

/// axum-server Acceptor that answers plain-HTTP requests on the TLS port
/// with a 301 to https:// (Chrome shows ERR_INVALID_HTTP_RESPONSE
/// otherwise), by peeking the first byte: a TLS ClientHello starts 0x16;
/// printable ASCII means an HTTP verb.
#[derive(Clone)]
pub struct RedirectingAcceptor {
    inner: axum_server::tls_rustls::RustlsAcceptor,
    fallback_host: String,
}

impl RedirectingAcceptor {
    pub fn new(inner: axum_server::tls_rustls::RustlsAcceptor, fallback_host: String) -> Self {
        Self { inner, fallback_host }
    }
}

impl<S> axum_server::accept::Accept<tokio::net::TcpStream, S> for RedirectingAcceptor
where
    S: Send + 'static,
{
    type Stream = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Service = S;
    type Future = std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::io::Result<(Self::Stream, Self::Service)>>
                + Send,
        >,
    >;

    fn accept(&self, stream: tokio::net::TcpStream, service: S) -> Self::Future {
        let inner = self.inner.clone();
        let fallback = self.fallback_host.clone();
        Box::pin(async move {
            let mut first = [0u8; 1];
            let n = stream.peek(&mut first).await?;
            if n == 1 && first[0] != 0x16 {
                // Plain HTTP: read the head. OAuth callbacks are SERVED in
                // place (see is_oauth_callback_path — a 301 into https strands
                // the consent tab on the cert interstitial); everything else
                // gets the redirect, as before.
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut stream = stream;
                let mut head = vec![0u8; 4096];
                let read = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                    let mut used=0;
                    while used < head.len() {
                        let n=stream.read(&mut head[used..]).await?;
                        if n==0 {break;} used+=n;
                        if head[..used].windows(4).any(|w|w==b"\r\n\r\n") {break;}
                    }
                    Ok::<usize,std::io::Error>(used)
                }).await.unwrap_or(Ok(0)).unwrap_or(0);
                let path = http_head_path(&head[..read]);
                let resp = if let Some(help) = local_setup_response(&head[..read], stream.peer_addr().ok().map(|a| a.ip())) {
                    help
                } else { match path.as_deref().filter(|p| is_oauth_callback_path(p)) {
                    Some(p) => match proxy_oauth_callback(p, &fallback).await {
                        Ok(r) => {
                            tracing::info!(
                                path = p.split('?').next().unwrap_or(p),
                                "plain-http oauth callback served in place (no TLS interstitial)"
                            );
                            r
                        }
                        Err(e) => {
                            // Fall back to the old behavior so the flow is
                            // never WORSE than before the fix — but say so.
                            tracing::warn!(error = %e, "oauth callback proxy failed — falling back to https redirect (the consent tab may strand on the cert interstitial)");
                            http_redirect_response(&head[..read], &fallback)
                        }
                    },
                    None => http_redirect_response(&head[..read], &fallback),
                }};
                let _ = stream.write_all(&resp).await;
                let _ = stream.shutdown().await;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "plain http redirected",
                ));
            }
            axum_server::accept::Accept::accept(&inner, stream, service).await
        })
    }
}

// Connection setup shares the resolver's material and never claims OS/browser trust.
#[derive(Clone, Default)]
struct ActiveCertificates {
    fallback: Option<serde_json::Value>,
    fallback_pem: String,
    tailscale: Option<serde_json::Value>,
}
fn active_certificates() -> &'static std::sync::RwLock<Option<ActiveCertificates>> {
    static ACTIVE: std::sync::OnceLock<std::sync::RwLock<Option<ActiveCertificates>>> = std::sync::OnceLock::new();
    ACTIVE.get_or_init(Default::default)
}
fn fingerprint(der: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(der))
}
fn public_certificate_pem(pem: &str) -> anyhow::Result<String> {
    use base64::Engine;
    let certs = rustls_pemfile::certs(&mut pem.as_bytes()).collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(!certs.is_empty(), "certificate_missing");
    let mut public = String::new();
    for cert in certs {
        public.push_str("-----BEGIN CERTIFICATE-----\n");
        let encoded = base64::engine::general_purpose::STANDARD.encode(cert);
        for line in encoded.as_bytes().chunks(64) {
            public.push_str(std::str::from_utf8(line)?);
            public.push('\n');
        }
        public.push_str("-----END CERTIFICATE-----\n");
    }
    Ok(public)
}

/// OpenSSL is used only for public X.509 metadata/date parsing. No shell, key,
/// trust-store mutation, network access or unbounded process. Missing tooling
/// fails upload closed; rustls remains the key/name validation authority.
fn certificate_metadata(pem: &str) -> anyhow::Result<serde_json::Value> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    use wait_timeout::ChildExt;
    let public = public_certificate_pem(pem)?;
    let mut input = tempfile::NamedTempFile::new()?;
    input.write_all(public.as_bytes())?;
    let output = tempfile::NamedTempFile::new()?;
    let mut child = Command::new("openssl")
        .args(["x509", "-in"]).arg(input.path())
        .args(["-noout", "-startdate", "-enddate", "-subject", "-issuer"])
        .stdin(Stdio::null()).stdout(output.reopen()?).stderr(Stdio::null()).spawn()
        .map_err(|_| anyhow::anyhow!("certificate_inspector_unavailable"))?;
    let status = match child.wait_timeout(std::time::Duration::from_secs(3)) {
        Ok(status) => status,
        Err(_) => { let _=child.kill(); let _=child.wait(); anyhow::bail!("certificate_inspector_failed"); }
    };
    let Some(status) = status else {
        let _ = child.kill(); let _ = child.wait();
        anyhow::bail!("certificate_inspector_timeout");
    };
    anyhow::ensure!(status.success(), "certificate_invalid");
    let mut text = String::new();
    output.reopen()?.take(16384).read_to_string(&mut text)?;
    let field = |prefix: &str| text.lines().find_map(|l| l.strip_prefix(prefix)).unwrap_or("").trim().to_string();
    let before = field("notBefore=");
    let after = field("notAfter=");
    let parse = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%b %e %H:%M:%S %Y GMT").map(|t| t.and_utc().timestamp());
    let start = parse(&before).map_err(|_| anyhow::anyhow!("certificate_dates_invalid"))?;
    let end = parse(&after).map_err(|_| anyhow::anyhow!("certificate_dates_invalid"))?;
    let now = chrono::Utc::now().timestamp();
    let certs = rustls_pemfile::certs(&mut public.as_bytes()).collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::json!({"sha256": fingerprint(&certs[0]), "subject": field("subject="),
        "issuer": field("issuer="), "not_before":start, "not_after":end,
        "valid_now":start <= now && now < end, "trust":"unknown_to_server"}))
}

pub fn connection_status(dir: &Path) -> serde_json::Value {
    let active = active_certificates().read().expect("TLS state lock").clone();
    connection_snapshot(dir, active.as_ref())
}
fn connection_snapshot(dir: &Path, active: Option<&ActiveCertificates>) -> serde_json::Value {
    let persisted = load_persisted_fingerprint(dir);
    let current = active.and_then(|a| a.fallback.as_ref());
    let loaded = current.and_then(|c| c["sha256"].as_str());
    serde_json::json!({"measured":active.is_some(), "n_considered":usize::from(active.is_some()),
        "active":current, "tailscale":active.and_then(|a| a.tailscale.as_ref()),
        "saved_sha256":persisted, "restart_required":persisted.as_deref().zip(loaded).map(|(p,a)| p!=a),
        "trust":"unknown_to_server", "setup_path":"/connection-setup",
        "why":"The server reports the loaded certificate, not browser or OS trust. Certificate changes take effect after an operator restart."})
}
fn load_persisted_fingerprint(dir: &Path) -> Option<String> {
    let path = if dir.join("connection.pem").exists() { dir.join("connection.pem") } else { dir.join("cert.pem") };
    let pem = std::fs::read_to_string(path).ok()?;
    let cert = rustls_pemfile::certs(&mut pem.as_bytes()).next()?.ok()?;
    Some(fingerprint(&cert))
}
fn local_setup_authority(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else { return false; };
    if host.eq_ignore_ascii_case("localhost") { return true; }
    host.trim_matches(['[',']']).parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Validate everything before the one atomic replacement. The active resolver
/// continues serving its old immutable key until a deliberate process restart.
pub fn install_connection_certificate(dir: &Path, cert: &str, key: &str, hostname: &str) -> anyhow::Result<serde_json::Value> {
    use std::io::Write;
    anyhow::ensure!(cert.len() <= 65536 && key.len() <= 32768, "certificate_too_large");
    let certified = load_certified_key(cert, key).map_err(|_| anyhow::anyhow!("certificate_invalid_or_key_mismatch"))?;
    let leaf = certified.cert.first().ok_or_else(|| anyhow::anyhow!("certificate_missing"))?;
    let parsed = rustls::server::ParsedCertificate::try_from(leaf).map_err(|_| anyhow::anyhow!("certificate_invalid"))?;
    let name = rustls::pki_types::ServerName::try_from(hostname.to_string()).map_err(|_| anyhow::anyhow!("certificate_hostname_invalid"))?;
    rustls::client::verify_server_name(&parsed, &name).map_err(|_| anyhow::anyhow!("certificate_hostname_mismatch"))?;
    let metadata = certificate_metadata(cert)?;
    anyhow::ensure!(metadata["valid_now"] == true, "certificate_not_currently_valid");
    // Re-encode only certificate PEM; reject accidentally supplied private keys
    // in that field rather than ever returning them as public metadata/download.
    anyhow::ensure!(!cert.contains("PRIVATE KEY"), "certificate_field_contains_private_key");
    let public = public_certificate_pem(cert)?;
    std::fs::create_dir_all(dir)?;
    let mut pending = tempfile::NamedTempFile::new_in(dir)?; // 0600, same filesystem
    pending.write_all(public.as_bytes())?;
    pending.write_all(b"\n")?;
    // Persist only the parsed key, not additional PEM blocks supplied beside it.
    use base64::Engine;
    let key = rustls_pemfile::private_key(&mut key.as_bytes())?.ok_or_else(||anyhow::anyhow!("private_key_missing"))?;
    let label = match &key {
        rustls::pki_types::PrivateKeyDer::Pkcs1(_) => "RSA PRIVATE KEY",
        rustls::pki_types::PrivateKeyDer::Sec1(_) => "EC PRIVATE KEY",
        rustls::pki_types::PrivateKeyDer::Pkcs8(_) => "PRIVATE KEY",
        _ => anyhow::bail!("private_key_unsupported"),
    };
    writeln!(pending,"-----BEGIN {label}-----")?;
    let encoded=base64::engine::general_purpose::STANDARD.encode(key.secret_der());
    for line in encoded.as_bytes().chunks(64) {pending.write_all(line)?;pending.write_all(b"\n")?;}
    writeln!(pending,"-----END {label}-----")?;
    pending.as_file().sync_all()?;
    pending.persist(dir.join("connection.pem")).map_err(|_| anyhow::anyhow!("certificate_save_failed"))?;
    tracing::info!(target:"amux::tls", verdict="connection_certificate_saved", sha256=metadata["sha256"].as_str().unwrap_or(""), "validated certificate saved; operator restart required");
    Ok(metadata)
}

/// A static help/public-certificate surface only, on the existing HTTP leg.
/// Both socket peer and authority must be local. Never handles tokens, uploads,
/// API calls or a dashboard on plaintext, including DNS-rebinding requests.
fn local_setup_response(head: &[u8], peer: Option<std::net::IpAddr>) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(head).ok()?;
    let request = text.lines().next()?;
    let mut words = request.split_whitespace();
    if words.next()? != "GET" { return None; }
    let path = words.next()?;
    if !matches!(path, "/" | "/connection-setup" | "/connection-certificate.pem") { return None; }
    if !peer.is_some_and(|ip| ip.is_loopback()) { return None; }
    let host = text.lines().skip(1).filter_map(|l| l.split_once(':')).find(|(k,_)| k.eq_ignore_ascii_case("host"))?.1.trim();
    let url = reqwest::Url::parse(&format!("http://{host}")).ok()?;
    if !local_setup_authority(&url) { return None; }
    let (kind, body) = if path == "/connection-certificate.pem" {
        ("application/x-pem-file", active_certificates().read().ok()?.as_ref()?.fallback_pem.clone())
    } else {
        ("text/html; charset=utf-8", r#"<!doctype html><html><meta name="viewport" content="width=device-width"><title>Amux connection setup</title><body style="font:16px system-ui;max-width:42em;margin:2em auto;padding:1em"><h1>Connect securely to Amux</h1><p>This local page provides instructions only. Never paste an owner token or private key into an HTTP page.</p><p>A browser cannot grant certificate trust. Ask the machine owner to configure a certificate trusted by this browser, with localhost and 127.0.0.1 in its Subject Alternative Names.</p><ol><li>On the server, use an existing trusted local CA (for example mkcert). Installing its CA in an OS/browser trust store is an explicit owner action. Never share its CA private key.</li><li>Generate a server certificate: <code>mkcert -cert-file localhost.pem -key-file localhost-key.pem localhost 127.0.0.1 ::1</code>.</li><li>If HTTPS is already accessible, open Settings → Connect → Connection &amp; security and upload that server certificate and key, then arrange a server restart.</li><li>If HTTPS is blocked before the app loads, the operator can atomically install the pair in the active Amux home's <code>tls/connection.pem</code>: concatenate certificate then private key into a new mode-0600 file in that directory, then rename it to connection.pem and restart the server. Do not change another Amux home's files.</li><li>Open the original HTTPS address again. The UI reports loaded and saved fingerprints; verify the loaded fingerprint and browser certificate status.</li></ol><p><a href="/connection-certificate.pem">Download the currently served fallback certificate (public only)</a> for fingerprint inspection. Downloading it does not make it trusted. Do not bypass browser TLS checks.</p></body></html>"#.to_string())
    };
    tracing::info!(target:"amux::tls", verdict="local_connection_setup", path, "served local read-only TLS setup guidance");
    Some(format!("HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",body.len()).into_bytes())
}

#[cfg(test)]
mod connection_tests {
    use super::*;
    #[test]
    fn connection_certificate_atomic_save_validates_key_name_dates_and_survives_restart() {
        let dir=tempfile::tempdir().unwrap();
        let original=load_or_generate(dir.path()).unwrap();
        let first=load_certified_key(&original.cert_pem,&original.key_pem).unwrap();
        let one=rcgen::generate_simple_self_signed(vec!["localhost".into(),"127.0.0.1".into()]).unwrap();
        let other=rcgen::generate_simple_self_signed(vec!["elsewhere.test".into()]).unwrap();
        for (cert,key,host) in [
            ("invalid".into(),one.key_pair.serialize_pem(),"localhost"),
            (one.cert.pem(),other.key_pair.serialize_pem(),"localhost"),
            (other.cert.pem(),other.key_pair.serialize_pem(),"localhost"),
        ] {
            assert!(install_connection_certificate(dir.path(),&cert,&key,host).is_err());
            assert_eq!(load_or_generate(dir.path()).unwrap().cert_pem,original.cert_pem);
        }
        for (from,to) in [(2000,2001),(2090,2091)] {
            let mut p=rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
            p.not_before=rcgen::date_time_ymd(from,1,1);p.not_after=rcgen::date_time_ymd(to,1,1);
            let expired=p.self_signed(&one.key_pair).unwrap();
            assert_eq!(install_connection_certificate(dir.path(),&expired.pem(),&one.key_pair.serialize_pem(),"localhost").unwrap_err().to_string(),"certificate_not_currently_valid");
        }
        let saved=install_connection_certificate(dir.path(),&one.cert.pem(),&one.key_pair.serialize_pem(),"localhost").unwrap();
        assert_eq!(load_persisted_fingerprint(dir.path()).unwrap(),saved["sha256"]);
        // Live resolver still owns the old immutable key; restart adopts pair.
        assert_ne!(fingerprint(&first.cert[0]),saved["sha256"]);
        let adopted=load_or_generate(dir.path()).unwrap();
        let restored=load_certified_key(&adopted.cert_pem,&adopted.key_pem).unwrap();
        assert_eq!(fingerprint(&restored.cert[0]),saved["sha256"]);
        assert!(!public_certificate_pem(&adopted.cert_pem).unwrap().contains("PRIVATE KEY"));
        #[cfg(unix)] {use std::os::unix::fs::PermissionsExt;assert_eq!(std::fs::metadata(dir.path().join("connection.pem")).unwrap().permissions().mode() & 0o777,0o600);}
        // A failed atomic replace cannot damage the active pair or legacy files.
        let blocked=tempfile::tempdir().unwrap();
        std::fs::create_dir(blocked.path().join("connection.pem")).unwrap();
        assert!(install_connection_certificate(blocked.path(),&one.cert.pem(),&one.key_pair.serialize_pem(),"localhost").is_err());
        assert!(blocked.path().join("connection.pem").is_dir());
        assert_eq!(load_certified_key(&adopted.cert_pem,&adopted.key_pem).unwrap().cert,restored.cert);
    }
    #[test]
    fn connection_certificate_status_uses_loaded_resolver_and_preserves_tailscale_on_restart() {
        let dir=tempfile::tempdir().unwrap();
        let ts=rcgen::generate_simple_self_signed(vec!["machine.test.ts.net".into()]).unwrap();
        std::fs::write(dir.path().join("machine.test.ts.net.crt"),ts.cert.pem()).unwrap();
        std::fs::write(dir.path().join("machine.test.ts.net.key"),ts.key_pair.serialize_pem()).unwrap();
        let (_,old)=prepare_server_config(dir.path()).unwrap();
        let before=connection_snapshot(dir.path(),Some(&old));
        assert_eq!(before["restart_required"],false);
        assert_eq!(before["tailscale"]["hostname"],"machine.test.ts.net");
        let next=rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let saved=install_connection_certificate(dir.path(),&next.cert.pem(),&next.key_pair.serialize_pem(),"localhost").unwrap();
        // Files exist but the live resolver still contains the previous cert.
        let pending=connection_snapshot(dir.path(),Some(&old));
        assert_eq!(pending["active"],before["active"]);
        assert_eq!(pending["saved_sha256"],saved["sha256"]);
        assert_eq!(pending["restart_required"],true);
        assert_eq!(pending["trust"],"unknown_to_server");
        let (_,restarted)=prepare_server_config(dir.path()).unwrap();
        let after=connection_snapshot(dir.path(),Some(&restarted));
        assert_eq!(after["active"]["sha256"],saved["sha256"]);
        assert_eq!(after["restart_required"],false);
        assert_eq!(after["tailscale"],before["tailscale"]);
        std::fs::write(dir.path().join("machine.test.ts.net.key"),next.key_pair.serialize_pem()).unwrap();
        let (_,bad_sni)=prepare_server_config(dir.path()).unwrap();
        assert!(bad_sni.tailscale.is_none(),"file presence cannot claim a loaded SNI pair");
    }
    #[tokio::test]
    async fn connection_setup_reaches_real_plain_http_acceptor_without_serving_owner_api() {
        use tokio::io::{AsyncReadExt,AsyncWriteExt};
        let dir=tempfile::tempdir().unwrap();
        let config=prepare_server_config(dir.path()).unwrap().0;
        let rustls=axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(config));
        for (path,expected) in [("/connection-setup","200 OK"),("/api/connection/session","301 Moved Permanently")] {
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address=listener.local_addr().unwrap();
            let acceptor=RedirectingAcceptor::new(axum_server::tls_rustls::RustlsAcceptor::new(rustls.clone()),address.to_string());
            let server=tokio::spawn(async move {
                let (stream,_)=listener.accept().await.unwrap();
                assert!(axum_server::accept::Accept::accept(&acceptor,stream,()).await.is_err());
            });
            let mut client=tokio::net::TcpStream::connect(address).await.unwrap();
            client.write_all(format!("GET {path} HTTP/1.1\r\n").as_bytes()).await.unwrap();
            tokio::task::yield_now().await;
            client.write_all(b"Host: localhost:18972\r\n\r\n").await.unwrap();
            let mut response=String::new();
            tokio::time::timeout(std::time::Duration::from_secs(5),client.read_to_string(&mut response)).await.unwrap().unwrap();
            assert!(response.starts_with(&format!("HTTP/1.1 {expected}")),"{response}");
            assert!(!response.contains("Set-Cookie:"));server.await.unwrap();
        }
    }
    #[test]
    fn connection_setup_is_read_only_local_peer_and_local_authority_only() {
        let local=Some("127.0.0.1".parse().unwrap());
        let page=local_setup_response(b"GET /connection-setup HTTP/1.1\r\nHost: localhost:18972\r\n\r\n",local).unwrap();
        let page=String::from_utf8(page).unwrap();
        assert!(page.contains("200 OK"));assert!(page.contains("mkcert"));assert!(page.contains("explicit owner action"));
        assert!(!page.contains("<script"));assert!(!page.contains("<input"));
        assert!(local_setup_response(b"GET /connection-setup HTTP/1.1\r\nHost: [::1]:18972\r\n\r\n",Some("::1".parse().unwrap())).is_some());
        assert!(local_setup_authority(&reqwest::Url::parse("http://[::1]:18972").unwrap()));
        assert!(local_setup_authority(&reqwest::Url::parse("http://127.0.0.1:18972").unwrap()));
        assert!(local_setup_authority(&reqwest::Url::parse("http://localhost:18972").unwrap()));
        assert!(!local_setup_authority(&reqwest::Url::parse("http://evil.test:18972").unwrap()));
        for head in [
            "POST /connection-setup HTTP/1.1\r\nHost: localhost:18972\r\n\r\n",
            "GET /connection-setup HTTP/1.1\r\nHost: evil.test:18972\r\n\r\n",
            "GET /api/connection/session HTTP/1.1\r\nHost: localhost:18972\r\n\r\n",
            "GET /connection-setup?token=secret HTTP/1.1\r\nHost: localhost:18972\r\n\r\n",
        ] {assert!(local_setup_response(head.as_bytes(),local).is_none());}
        assert!(local_setup_response(b"GET /connection-setup HTTP/1.1\r\nHost: localhost:18972\r\n\r\n",Some("192.0.2.1".parse().unwrap())).is_none());
        assert!(local_setup_response(b"GET /connection-setup HTTP/1.1\r\nHost: localhost:18972\r\n\r\n",None).is_none());
    }
}
