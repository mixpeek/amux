//! Relaying an app that listens on this machine's loopback to a browser
//! elsewhere: the apps origin's `/app/<id>/` (apps_origin.rs) and public links'
//! `/pub/<id>/` (public_links.rs). Bodies stream, `Range` passes through so
//! video seeks, HTML has its root-relative `src`/`href` routed back under the
//! prefix, redirects stay inside it, and amux's cookies and the owner's bearer
//! never reach the app, nor may the app set amux's cookies.
//!
//! This replaced `/proxy/<port>/` (removed 2026-10-25, Ethan: "remove the
//! proxies"): an app is opened from the Apps tab, on its own origin.
use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use std::sync::OnceLock;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);

/// The host part of a Host header, without its port. Keeps IPv6 brackets so
/// the result can be put straight back into a URL.
pub(crate) fn host_only(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority
            .find(']')
            .map_or(authority, |i| &authority[..=i]);
    }
    authority.split(':').next().unwrap_or(authority)
}

pub(crate) fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\">\
         <meta name=\"color-scheme\" content=\"dark light\"><title>{t}</title>\
         <style>body{{font-family:system-ui,sans-serif;max-width:36em;margin:18vh auto 0;padding:0 16px;line-height:1.5}}code{{word-break:break-all}}</style>\
         </head><body><h1 style=\"font-size:1.3em\">{t}</h1>{body}<p><a href=\"/\">Back to amux</a></p></body></html>",
        t = html_escape(title),
    );
    (status, Html(html)).into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

async fn answers(host: &str, port: u16) -> bool {
    let name = host.trim_start_matches('[').trim_end_matches(']').to_string();
    let addrs = match tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::lookup_host((name, port))).await {
        Ok(Ok(a)) => a.collect::<Vec<_>>(),
        _ => return false,
    };
    for addr in addrs {
        if let Ok(Ok(_)) = tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(addr)).await {
            return true;
        }
    }
    false
}

/// Where something on this machine listens on `port`: IPv4 loopback, then IPv6.
pub(crate) async fn local_listener(port: u16) -> Option<&'static str> {
    if answers("127.0.0.1", port).await {
        Some("127.0.0.1")
    } else if answers("::1", port).await {
        Some("[::1]")
    } else {
        None
    }
}

/// HTML larger than this is passed through unrewritten rather than buffered.
const HTML_REWRITE_MAX: usize = 16 * 1024 * 1024;

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("relay http client")
    })
}

/// Forward one request to `local:port` and answer with its response, for a
/// dev server the browser cannot reach directly. `prefix` is the path the app
/// is mounted under (`/app/<id>` or `/pub/<id>`), `tail` the page and query.
pub(crate) async fn relay(local: &str, port: u16, prefix: &str, tail: &str, method: Method, headers: &HeaderMap, body: Body) -> Response {
    let body = match axum::body::to_bytes(body, 64 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return page(StatusCode::PAYLOAD_TOO_LARGE, "Request too large", "<p>The request body is over 64 MB.</p>"),
    };
    let method = reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut req = client()
        .request(method, format!("http://{local}:{port}{tail}"))
        .header("Host", format!("localhost:{port}"));
    for (k, v) in headers {
        if forward_request_header(k) {
            req = req.header(k.as_str(), v.as_bytes());
        }
    }
    if let Some(c) = strip_amux_cookies(headers) {
        req = req.header("Cookie", c);
    }
    if !body.is_empty() {
        req = req.body(body);
    }
    let upstream = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(port, error = %e, measured = true, n_considered = 1,
                verdict = "app_relay_failed", "relay to a loopback-only dev server failed");
            return page(StatusCode::BAD_GATEWAY, "The dev server did not answer",
                &format!("<p>Port {port} accepted a connection but the request failed.</p>"));
        }
    };
    let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let is_html = upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/html"));
    let rewrite = is_html && upstream.content_length().is_none_or(|n| n <= HTML_REWRITE_MAX as u64);
    let mut out = HeaderMap::new();
    for (k, v) in upstream.headers() {
        let name = k.as_str();
        if matches!(name, "connection" | "keep-alive" | "transfer-encoding" | "upgrade")
            || (rewrite && name == "content-length")
        {
            continue;
        }
        // An app relayed on amux's origin must not set amux's own cookies: a
        // `Set-Cookie: amux_member=...` from it would replace the owner's sign-in
        // for the whole dashboard (session fixation through a dev server).
        if name == "set-cookie" && v.to_str().ok().is_some_and(sets_amux_cookie) {
            tracing::warn!(port, measured = true, n_considered = 1, verdict = "app_relay_amux_cookie_dropped",
                "a relayed app tried to set one of amux's cookies; dropped");
            continue;
        }
        let Ok(hn) = HeaderName::from_bytes(name.as_bytes()) else { continue };
        let value = if name == "location" {
            v.to_str().ok().map(|l| rewrite_location(l, port, prefix)).and_then(|l| HeaderValue::from_str(&l).ok())
        } else {
            HeaderValue::from_bytes(v.as_bytes()).ok()
        };
        if let Some(value) = value {
            out.append(hn, value);
        }
    }
    let body = if rewrite {
        match upstream.bytes().await {
            Ok(b) => Body::from(rewrite_html(&String::from_utf8_lossy(&b), prefix)),
            Err(e) => {
                tracing::warn!(port, error = %e, measured = true, n_considered = 1,
                    verdict = "app_relay_failed", "the relayed page stopped mid-response");
                return page(StatusCode::BAD_GATEWAY, "The page stopped mid-response", "<p>Try again.</p>");
            }
        }
    } else if buffer_whole(status, upstream.content_length()) {
        // Redirects and small bodies go out with an exact length. WebKit drops the
        // connection on a 302 relayed as an empty chunked stream (MSG-90852).
        match upstream.bytes().await {
            Ok(b) => Body::from(b),
            Err(e) => {
                tracing::warn!(port, error = %e, measured = true, n_considered = 1,
                    verdict = "app_relay_failed", "the relayed response stopped mid-body");
                return page(StatusCode::BAD_GATEWAY, "The page stopped mid-response", "<p>Try again.</p>");
            }
        }
    } else {
        Body::from_stream(futures::stream::unfold(Some(upstream), |r| async move {
            let mut r = r?;
            match r.chunk().await {
                Ok(Some(b)) => Some((Ok::<_, std::io::Error>(b), Some(r))),
                Ok(None) => None,
                Err(e) => Some((Err(std::io::Error::other(e)), None)),
            }
        }))
    };
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    *resp.headers_mut() = out;
    resp
}

/// Bodies small enough to hold are relayed whole; video, downloads and
/// event streams of unknown length keep streaming.
fn buffer_whole(status: StatusCode, len: Option<u64>) -> bool {
    status.is_informational()
        || status.is_redirection()
        || status == StatusCode::NO_CONTENT
        || len.is_some_and(|n| n <= 256 * 1024)
}

/// Hop-by-hop and identity headers stay here: amux cookies and the owner's
/// bearer never reach a local app, and an encoded HTML body could not be
/// rewritten.
fn forward_request_header(k: &HeaderName) -> bool {
    !matches!(
        k.as_str(),
        "host" | "content-length" | "connection" | "keep-alive" | "transfer-encoding" | "upgrade"
            | "authorization" | "cookie" | "accept-encoding"
    )
}

/// A `Set-Cookie` value naming one of amux's (or the cloud door's) cookies.
pub(crate) fn sets_amux_cookie(set_cookie: &str) -> bool {
    let name = set_cookie.split('=').next().unwrap_or("").trim();
    name.starts_with("amux_") || name.starts_with("__Host-amux") || name.starts_with("__Secure-amux")
}

fn strip_amux_cookies(headers: &HeaderMap) -> Option<String> {
    // Every `cookie` field: HTTP/2 browsers send one field per cookie.
    let kept: Vec<&str> = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|all| all.split(';'))
        .map(str::trim)
        .filter(|c| {
            let name = c.split('=').next().unwrap_or("");
            !name.is_empty() && !name.starts_with("__Host-amux") && !name.starts_with("amux_")
        })
        .collect();
    (!kept.is_empty()).then(|| kept.join("; "))
}

fn rewrite_location(loc: &str, port: u16, prefix: &str) -> String {
    for host in ["localhost", "127.0.0.1", "0.0.0.0", "[::1]"] {
        for scheme in ["http", "https"] {
            if let Some(rest) = loc.strip_prefix(&format!("{scheme}://{host}:{port}")) {
                return format!("{prefix}{}", if rest.is_empty() { "/" } else { rest });
            }
        }
    }
    if loc.starts_with('/') && !loc.starts_with("//") && !loc.starts_with(&format!("{prefix}/")) {
        return format!("{prefix}{loc}");
    }
    loc.to_string()
}

fn rewrite_html(html: &str, prefix: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r#"(\s(?:src|href|action|poster)=)(["'])/([^/"'][^"']*|)(["'])"#).expect("relay html regex")
    });
    re.replace_all(html, |c: &regex::Captures| {
        if format!("/{}", &c[3]).starts_with(&format!("{prefix}/")) {
            return c[0].to_string();
        }
        format!("{}{}{prefix}/{}{}", &c[1], &c[2], &c[3], &c[4])
    })
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_keeps_its_name_and_drops_its_port() {
        assert_eq!(host_only("desktop.tail5ce8f5.ts.net:8824"), "desktop.tail5ce8f5.ts.net");
        assert_eq!(host_only("localhost"), "localhost");
        assert_eq!(host_only("[fd7a:115c:a1e0::1]:8824"), "[fd7a:115c:a1e0::1]");
    }

    #[test]
    fn relayed_html_routes_absolute_paths_back_under_the_prefix() {
        let p = "/app/0123456789abcdef0123456789abcdef";
        let out = rewrite_html(
            r#"<link href="/a.css"><script src='/b.js'></script><a href="//cdn.x/y">c</a><video poster="/p.jpg" src="rel.webm"><a href="/app/0123456789abcdef0123456789abcdef/z">"#,
            p,
        );
        assert!(out.contains(r#"href="/app/0123456789abcdef0123456789abcdef/a.css""#), "{out}");
        assert!(out.contains("src='/app/0123456789abcdef0123456789abcdef/b.js'"), "{out}");
        assert!(out.contains(r#"href="//cdn.x/y""#), "protocol-relative stays: {out}");
        assert!(out.contains(r#"poster="/app/0123456789abcdef0123456789abcdef/p.jpg""#), "{out}");
        assert!(out.contains(r#"src="rel.webm""#), "relative stays: {out}");
        assert!(out.contains(r#"href="/app/0123456789abcdef0123456789abcdef/z""#), "already prefixed stays: {out}");
        assert!(rewrite_html(r#"<a href="/">home</a>"#, p).contains(r#"href="/app/0123456789abcdef0123456789abcdef/""#));
    }

    #[test]
    fn a_relayed_app_cannot_set_amux_cookies() {
        for c in ["amux_member=x; Path=/", "amux_door=x", "__Host-amux_owner=x; Secure", "__Secure-amux=x"] {
            assert!(sets_amux_cookie(c), "{c}");
        }
        assert!(!sets_amux_cookie("appsid=1; Path=/"));
    }

    #[test]
    fn relayed_redirects_stay_under_the_prefix() {
        let p = "/app/0123456789abcdef0123456789abcdef";
        assert_eq!(rewrite_location("/login", 3077, p), "/app/0123456789abcdef0123456789abcdef/login");
        assert_eq!(rewrite_location("http://localhost:3077/x?y=1", 3077, p), "/app/0123456789abcdef0123456789abcdef/x?y=1");
        assert_eq!(rewrite_location("http://127.0.0.1:3077", 3077, p), "/app/0123456789abcdef0123456789abcdef/");
        assert_eq!(rewrite_location("https://example.com/a", 3077, p), "https://example.com/a");
        assert_eq!(rewrite_location("/app/0123456789abcdef0123456789abcdef/a", 3077, p), "/app/0123456789abcdef0123456789abcdef/a");
    }

    #[test]
    fn amux_credentials_never_reach_the_relayed_app() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_static("__Host-amux_owner=abc; theme=dark; amux_member=m; sid=1"));
        assert_eq!(strip_amux_cookies(&h).as_deref(), Some("theme=dark; sid=1"));
        // HTTP/2 crumbs: one field per cookie, the amux one first.
        let mut h2 = HeaderMap::new();
        h2.append(header::COOKIE, HeaderValue::from_static("__Host-amux_owner=abc"));
        h2.append(header::COOKIE, HeaderValue::from_static("appsid=1"));
        assert_eq!(strip_amux_cookies(&h2).as_deref(), Some("appsid=1"));
        for k in ["authorization", "cookie", "accept-encoding", "host"] {
            assert!(!forward_request_header(&HeaderName::from_static(k)), "{k} must not be forwarded");
        }
        assert!(forward_request_header(&HeaderName::from_static("range")), "Range passes through for video");
    }

    #[tokio::test]
    async fn an_empty_chunked_redirect_is_relayed_with_an_exact_length() {
        use axum::body::HttpBody as _;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await;
            s.write_all(b"HTTP/1.1 302 Found\r\nLocation: /landed\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n0\r\n\r\n")
                .await
                .unwrap();
        });
        let r = relay("127.0.0.1", port, "/app/0123456789abcdef0123456789abcdef", "/redir", Method::GET, &HeaderMap::new(), Body::empty()).await;
        assert_eq!(r.status(), StatusCode::FOUND);
        assert_eq!(r.headers().get("location").unwrap(), &"/app/0123456789abcdef0123456789abcdef/landed");
        assert!(r.headers().get("transfer-encoding").is_none());
        assert_eq!(r.body().size_hint().exact(), Some(0), "a streamed body has no exact length");
        assert!(buffer_whole(StatusCode::OK, Some(10)));
        assert!(!buffer_whole(StatusCode::OK, None), "unknown-length streams keep streaming");
        assert!(!buffer_whole(StatusCode::PARTIAL_CONTENT, Some(50 << 20)), "large video ranges keep streaming");
    }

    #[tokio::test]
    async fn a_listening_port_answers_and_a_closed_one_does_not() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(answers("127.0.0.1", port).await);
        drop(l);
        assert!(!answers("127.0.0.1", port).await);
    }
}
