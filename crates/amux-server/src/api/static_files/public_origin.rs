//! A public app on an origin of its own: `amux publish --origin <port>` serves
//! it at `https://<id>.<AMUX_PUBLIC_APPS_DOMAIN>/`, at `/`, with its own cookies.
//!
//! Under `/pub/<id>/` (public_links.rs) a public app shares the dashboard's
//! origin, so every response is sandboxed into an opaque origin and no cookie
//! passes either way. That is right for a page, and fatal for an app with a
//! session: DocuSeal's signing form (Rails) needs its own session cookie for
//! the CSRF check, its fetches from an opaque origin carry `Origin: null`, and
//! its Vue code builds `fetch('/s/<slug>/...')` with no base, so the request
//! leaves the `/pub/<id>/` prefix (2026-11-12, wexus). On a host of its own the
//! app sees the paths it was written for and is isolated by origin instead:
//!
//! - The browser keeps the app's cookies to its own host. A `Domain=` attribute
//!   is taken off every cookie it sets, so it cannot reach the dashboard's host
//!   or another app, and one named like amux's is dropped (app_relay.rs).
//! - amux's cookies and the owner's bearer never reach the app (app_relay.rs);
//!   the dashboard's are host-only, so the browser does not send them here.
//! - The dashboard's host is the same SITE, so a page here could make the
//!   owner's browser write to amux's API; cross_site.rs refuses every
//!   `Sec-Fetch-Site: same-site` write, as it does between amux.io servers.
//!
//! These requests reach the apps listener (apps_origin.rs), which serves no
//! dashboard and no API, and are told apart by their Host. The app sees
//! `X-Forwarded-Host`/`X-Forwarded-Proto` naming its public address, so a
//! framework's same-origin check (Rails' `valid_request_origin?`) and the
//! absolute URLs it builds are right.
//!
//! The DNS name and certificate are infrastructure: a wildcard record and a
//! certificate for `*.<domain>` pointing at the reverse proxy in front of the
//! apps listener (cloud/desktop/caddy-public-origins.caddy). Without
//! `AMUX_PUBLIC_APPS_DOMAIN` an `--origin` link is refused when it is made.
use super::app_relay;
use super::public_links::{self, Link};
use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::path::Path;

pub(crate) const DOMAIN_KEY: &str = "AMUX_PUBLIC_APPS_DOMAIN";

/// `sandbox.amux.io` from `sandbox.amux.io`, `https://Sandbox.amux.io/` or
/// `.sandbox.amux.io`; None for anything that is not a plain DNS name with a dot.
pub(crate) fn normalize_domain(raw: &str) -> Option<String> {
    let d = raw.trim();
    let d = d.strip_prefix("https://").or_else(|| d.strip_prefix("http://")).unwrap_or(d);
    let d = d.trim_end_matches('/').trim_start_matches("*.").trim_matches('.').to_ascii_lowercase();
    let ok = d.contains('.')
        && d.len() <= 200
        && d.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-'))
        && d.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.');
    ok.then_some(d)
}

/// The domain public apps get hosts under, or None when this server has none.
pub(crate) fn domain(home: &Path) -> Option<String> {
    crate::api::settings::effective_env(home, DOMAIN_KEY).as_deref().and_then(normalize_domain)
}

/// Whether a link name can be a DNS label (valid_id allows 64; a label is 63).
pub(crate) fn fits_label(id: &str) -> bool {
    public_links::valid_id(id) && id.len() <= 63
}

/// `https://<id>.<domain>`, or None when the name cannot be a host label.
pub(crate) fn origin_url(domain: &str, id: &str) -> Option<String> {
    fits_label(id).then(|| format!("https://{id}.{domain}"))
}

/// The link name a host names: `<label>.<domain>` with exactly one label in
/// front. The domain itself, deeper names and other hosts are not apps.
pub(crate) fn label_for(host: &str, domain: &str) -> Option<String> {
    let h = app_relay::host_only(host.trim()).trim_end_matches('.').to_ascii_lowercase();
    let label = h.strip_suffix(domain)?.strip_suffix('.')?;
    (!label.contains('.') && fits_label(label)).then(|| label.to_string())
}

/// The host a request was made to: its Host header, or the HTTP/2 authority.
/// Never X-Forwarded-Host, which a client can write.
fn request_host(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| uri.authority().map(|a| a.as_str().to_string()))
}

/// The apps listener's first stop: a request to `<id>.<domain>` is that public
/// app's, whatever its path; anything else goes on to `/app/<id>/`.
pub(crate) async fn dispatch(req: Request, next: Next) -> Response {
    let home = crate::config::amux_home();
    if let Some(d) = domain(&home) {
        if let Some(label) = request_host(req.headers(), req.uri()).and_then(|h| label_for(&h, &d)) {
            return serve(&home, &d, &label, req).await;
        }
    }
    next.run(req).await
}

/// A `Set-Cookie` value with its `Domain` attribute taken off: the cookie stays
/// on the app's own host and never reaches the dashboard or a sibling app.
pub(crate) fn host_only_cookie(set_cookie: &str) -> String {
    let mut parts = set_cookie.split(';');
    let mut out = vec![parts.next().unwrap_or("").to_string()];
    out.extend(
        parts
            .filter(|a| !a.trim().to_ascii_lowercase().starts_with("domain="))
            .map(str::to_string),
    );
    out.join(";")
}

fn with_host_only_cookies(mut r: Response, id: &str) -> Response {
    let cookies: Vec<HeaderValue> = r.headers().get_all(header::SET_COOKIE).iter().cloned().collect();
    if cookies.is_empty() {
        return r;
    }
    let h = r.headers_mut();
    h.remove(header::SET_COOKIE);
    let mut scoped = 0usize;
    for c in cookies {
        let Ok(s) = c.to_str() else { continue };
        let kept = host_only_cookie(s);
        if kept.len() != s.len() {
            scoped += 1;
        }
        if let Ok(v) = HeaderValue::from_str(&kept) {
            h.append(header::SET_COOKIE, v);
        }
    }
    if scoped > 0 {
        tracing::info!(id = %id, scoped, measured = true, n_considered = scoped, verdict = "public_origin_cookie_domain_dropped",
            "a public app set a cookie for a wider domain; it was kept to the app's own host");
    }
    r
}

async fn serve(home: &Path, domain: &str, label: &str, req: Request) -> Response {
    let headers = req.headers().clone();
    let path = req.uri().path().to_string();
    let r = serve_inner(home, domain, label, req).await;
    public_links::access_log(label, r.status(), &headers, &path);
    r
}

async fn serve_inner(home: &Path, domain: &str, label: &str, req: Request) -> Response {
    let tail = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    // Only a live public app made with --origin answers here; anything else,
    // pending ones included, looks like nothing at all.
    let Some(link): Option<Link> = public_links::lookup(public_links::load(home), label).filter(|l| l.serves_on_origin()) else {
        tracing::info!(label = %label, measured = true, n_considered = 1, verdict = "public_origin_unknown",
            "a request to a public app host that names no live --origin app");
        return public_links::not_found();
    };
    // A name it had before a rename: the same page on the current name's host.
    if link.id != label {
        let Some(to) = origin_url(domain, &link.id) else { return public_links::not_found() };
        tracing::info!(from = %label, to = %link.id, measured = true, n_considered = 1, verdict = "public_origin_old_name",
            "a public app was opened by a name it had before a rename");
        return (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, format!("{to}{tail}"))]).into_response();
    }
    if public_links::refused_port(link.port).is_some() {
        return public_links::not_found();
    }
    let Some(local) = app_relay::local_listener(link.port).await else {
        tracing::warn!(id = %link.id, port = link.port, measured = true, n_considered = 1, verdict = "public_link_nothing_listening",
            "public app opened on its own origin while nothing listens on its port");
        return app_relay::page(StatusCode::BAD_GATEWAY, "This app is not running",
            "<p>The app behind this link is not running right now. Ask the person who shared it.</p>");
    };
    if super::app_ports::is_upgrade(req.headers()) {
        return super::app_ports::relay_ws(local, link.port, &tail, req).await;
    }
    let method = req.method().clone();
    let headers = req.headers().clone();
    let public_host = format!("{}.{domain}", link.id);
    let r = app_relay::relay_as(local, link.port, "", &tail, method, &headers, req.into_body(), Some(&public_host)).await;
    with_host_only_cookies(r, &link.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Method;

    #[test]
    fn the_domain_is_a_plain_dns_name() {
        assert_eq!(normalize_domain("sandbox.amux.io").as_deref(), Some("sandbox.amux.io"));
        assert_eq!(normalize_domain(" https://Sandbox.amux.io/ ").as_deref(), Some("sandbox.amux.io"));
        assert_eq!(normalize_domain("*.sandbox.amux.io").as_deref(), Some("sandbox.amux.io"));
        for bad in ["", "localhost", "a/b.c", "sandbox.amux.io:8443", "-x.amux.io", "a..b"] {
            assert_eq!(normalize_domain(bad), None, "{bad}");
        }
    }

    #[test]
    fn only_one_label_in_front_of_the_domain_is_an_app() {
        let d = "sandbox.amux.io";
        assert_eq!(label_for("docuseal-wexus.sandbox.amux.io", d).as_deref(), Some("docuseal-wexus"));
        assert_eq!(label_for("Docuseal-Wexus.Sandbox.amux.io:443", d).as_deref(), Some("docuseal-wexus"));
        let hex = "0123456789abcdef0123456789abcdef";
        assert_eq!(label_for(&format!("{hex}.{d}"), d).as_deref(), Some(hex));
        for not_app in ["sandbox.amux.io", "a.b.sandbox.amux.io", "xsandbox.amux.io", "evil.com", "ab.sandbox.amux.io",
            "x_y.sandbox.amux.io", "abc.sandbox.amux.io.evil.com"] {
            assert_eq!(label_for(not_app, d), None, "{not_app}");
        }
        // A 64-character name is a valid link name but not a DNS label.
        let long = "a".repeat(64);
        assert!(public_links::valid_id(&long) && !fits_label(&long));
        assert_eq!(origin_url(d, &long), None);
        assert_eq!(origin_url(d, "docuseal").as_deref(), Some("https://docuseal.sandbox.amux.io"));
    }

    #[test]
    fn an_app_cookie_stays_on_the_apps_own_host() {
        assert_eq!(host_only_cookie("_docuseal_session=x; path=/; domain=.sandbox.amux.io; secure; HttpOnly; SameSite=Lax"),
            "_docuseal_session=x; path=/; secure; HttpOnly; SameSite=Lax");
        assert_eq!(host_only_cookie("a=1; Domain=amux.io"), "a=1");
        assert_eq!(host_only_cookie("a=1; Path=/"), "a=1; Path=/");
        // The value itself may say "domain=": only attributes are dropped.
        assert_eq!(host_only_cookie("q=domain=x; Path=/"), "q=domain=x; Path=/");
        let r = (StatusCode::OK, [(header::SET_COOKIE, "s=1; Domain=sandbox.amux.io; Path=/")]).into_response();
        let r = with_host_only_cookies(r, "x");
        assert_eq!(r.headers().get(header::SET_COOKIE).unwrap(), "s=1; Path=/");
    }

    /// The whole path, against a real app on loopback: the request reaches the
    /// app at `/` with its own cookie and its public address, amux's cookie and
    /// the bearer do not, and the response keeps the app's cookie without the
    /// opaque-origin sandbox. A pending link, an app made without --origin and
    /// a host outside the domain are not served.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // the AMUX_HOME guard must span the requests
    async fn an_origin_app_is_served_at_root_with_its_own_cookies_and_no_sandbox() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                // local_listener's probe connects and closes without a request.
                if n == 0 {
                    continue;
                }
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nSet-Cookie: _app_session=new; path=/; domain=.sandbox.example; HttpOnly\r\nSet-Cookie: amux_member=evil; Path=/\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await;
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(dir.path());
        std::fs::write(dir.path().join("server.env"), "AMUX_PUBLIC_APPS_DOMAIN=sandbox.example\n").unwrap();
        let mk = |id: &str, origin: bool, approved: &str| Link { id: id.into(), kind: "app".into(), port, path: String::new(),
            title: String::new(), created_by: String::new(), created_at: 0, approved_by: approved.into(), aliases: Vec::new(), origin };
        let links = serde_json::json!({ "links": [mk("signing", true, "owner"), mk("plain-pub", false, "owner"), mk("waiting", true, "")] });
        std::fs::write(dir.path().join("public-links.json"), links.to_string()).unwrap();

        let app = axum::Router::new()
            .fallback(|| async { (StatusCode::IM_A_TEAPOT, "apps origin") })
            .layer(axum::middleware::from_fn(dispatch));
        let call = |host: &str, path: &str| {
            Request::builder()
                .method(Method::POST)
                .uri(path)
                .header(header::HOST, host)
                .header(header::ORIGIN, format!("https://{host}"))
                .header(header::COOKIE, "_app_session=old; __Host-amux_owner=secret; amux_member=m")
                .header(header::AUTHORIZATION, "Bearer owner-token")
                .header("x-forwarded-host", "attacker.example")
                .body(Body::from("a=1"))
                .unwrap()
        };
        use tower::ServiceExt;
        let r = app.clone().oneshot(call("signing.sandbox.example", "/s/abc/values?x=1")).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let seen = rx.recv().await.unwrap().to_ascii_lowercase();
        assert!(seen.starts_with("post /s/abc/values?x=1 "), "the app sees its own path, not a /pub/ prefix: {seen}");
        assert!(seen.contains("cookie: _app_session=old"), "the app's own cookie reaches it: {seen}");
        assert!(!seen.contains("amux") && !seen.contains("owner-token"), "amux's cookies or the bearer reached the app: {seen}");
        assert!(seen.contains("x-forwarded-host: signing.sandbox.example"), "{seen}");
        assert!(seen.contains("x-forwarded-proto: https"), "{seen}");
        assert!(!seen.contains("attacker.example"), "a client's X-Forwarded-Host reached the app: {seen}");
        assert!(r.headers().get(header::CONTENT_SECURITY_POLICY).is_none(), "an origin app is not sandboxed");
        let set: Vec<&str> = r.headers().get_all(header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()).collect();
        assert_eq!(set, vec!["_app_session=new; path=/; HttpOnly"], "the app's cookie, host-only; amux's dropped");

        for (host, why) in [("plain-pub.sandbox.example", "made without --origin"), ("waiting.sandbox.example", "pending"),
            ("nope.sandbox.example", "unknown")] {
            let r = app.clone().oneshot(call(host, "/")).await.unwrap();
            assert_eq!(r.status(), StatusCode::NOT_FOUND, "{why}");
        }
        let r = app.clone().oneshot(call("sandbox.example:8443", "/app/x/")).await.unwrap();
        assert_eq!(r.status(), StatusCode::IM_A_TEAPOT, "the apps origin itself goes on to /app/<id>/");

        // A /pub/ link to it, already shared, goes to its host.
        let r = public_links::serve(Method::GET, HeaderMap::new(), "/pub/signing/s/abc?x=1".parse().unwrap(), Body::empty()).await;
        assert_eq!(r.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(r.headers().get(header::LOCATION).unwrap(), "https://signing.sandbox.example/s/abc?x=1");
        // The default is unchanged: an app made without --origin stays under
        // /pub/, sandboxed, with no cookie either way.
        let r = public_links::serve(Method::GET, HeaderMap::new(), "/pub/plain-pub/".parse().unwrap(), Body::empty()).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert!(r.headers().get(header::CONTENT_SECURITY_POLICY).is_some_and(|v| v.to_str().unwrap().starts_with("sandbox")));
        assert!(r.headers().get(header::SET_COOKIE).is_none());
        let _ = rx.recv().await;
    }
}
