//! `/proxy/<port>/<path>`: open a dev server running on this machine from
//! any device that can reach amux.
//!
//! Workers hand the owner links of this shape for a local dev server
//! (`https://desktop.<tailnet>.ts.net:8824/proxy/3077/?hero=texture`). Until
//! 2026-10-17 no such route existed: the SPA catch-all answered with the
//! dashboard, so the owner on a phone landed on amux instead of the page.
//!
//! The route sends the browser to the SAME host on the dev server's own port
//! rather than relaying bytes. Dev servers write root-relative URLs
//! (`/@vite/client`, `/fonts/x.woff2`, `/_next/...`) and open HMR websockets
//! on their own origin; under a `/proxy/3077/` prefix every one of those
//! resolves against amux and breaks. On its own port the page is exactly what
//! the worker saw locally.
//!
//! So the route first proves the port answers at the address the browser
//! used. A server bound to 127.0.0.1 only cannot be reached that way, so amux
//! relays it instead (`relay`): bodies stream, `Range` passes through so video
//! seeks, and HTML has its root-relative `src`/`href` routed back under the
//! prefix. That covers static pages and media; a page that builds URLs in
//! script still needs the dev server on `--host 0.0.0.0`, which the WARN says.
use super::{has_owner_session, peer_ip, tailnet_auth, AppState, Peer};
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Redirect, Response};
use std::net::IpAddr;
use std::sync::OnceLock;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);

/// `/proxy/3077/a/b?x=1` -> (3077, "/a/b?x=1"). None when the port is not a
/// usable TCP port number.
pub(crate) fn parse_target(uri: &Uri) -> Option<(u16, String)> {
    let rest = uri.path().strip_prefix("/proxy/")?;
    let (port, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let port: u16 = port.parse().ok().filter(|p| *p > 0)?;
    let mut tail = tail.to_string();
    if let Some(q) = uri.query() {
        tail.push('?');
        tail.push_str(q);
    }
    Some((port, tail))
}

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

fn page(status: StatusCode, title: &str, body: &str) -> Response {
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

pub(crate) async fn proxy(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    peer: Peer,
    body: Body,
) -> Response {
    let peer = peer_ip(peer);
    // The same loopback switch as the rest of the server, so a test that turns
    // the bypass off sees the refusal a remote device gets.
    let loopback_bypass = !std::env::var("AMUX_RS_NO_LOOPBACK_BYPASS").is_ok_and(|v| v == "1");
    let owner = state.auth_token.is_none()
        || has_owner_session(&state, &headers)
        || (loopback_bypass && peer.is_some_and(|ip| ip.is_loopback()))
        || match peer {
            Some(ip) => tailnet_auth::verified(ip).await,
            None => false,
        };
    if !owner {
        tracing::info!(path = %uri.path(), measured = true, n_considered = 1,
            verdict = "port_proxy_not_signed_in", "proxy link opened without an owner session; sent to sign in");
        return Redirect::to("/").into_response();
    }
    let Some((port, tail)) = parse_target(&uri) else {
        return page(StatusCode::BAD_REQUEST, "Not a port",
            "<p>A proxy link looks like <code>/proxy/3077/</code>: a port number from 1 to 65535, then the page.</p>");
    };
    let authority = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| uri.authority().map(|a| a.to_string()))
        .unwrap_or_else(|| "localhost".to_string());
    let host = host_only(&authority).to_string();
    let own_port = authority
        .rsplit_once(':')
        .filter(|(h, _)| !h.is_empty() && !authority.ends_with(']'))
        .and_then(|(_, p)| p.parse::<u16>().ok());
    if own_port == Some(port) {
        return page(StatusCode::BAD_REQUEST, "That is amux's own port",
            &format!("<p>Port {port} is the amux server itself. Use the port the dev server printed.</p>"));
    }
    let local = if answers("127.0.0.1", port).await {
        Some("127.0.0.1")
    } else if answers("::1", port).await {
        Some("[::1]")
    } else {
        None
    };
    let Some(local) = local else {
        tracing::warn!(port, host = %host, measured = true, n_considered = 1,
            verdict = "port_proxy_nothing_listening", "proxy link to a port nothing on this machine listens on");
        return page(StatusCode::BAD_GATEWAY, "Nothing is running on that port",
            &format!("<p>No server is listening on port {port} on this machine. It may have stopped; ask the worker that sent the link to start it again.</p>"));
    };
    if !answers(&host, port).await {
        tracing::warn!(port, host = %host, measured = true, n_considered = 1,
            verdict = "port_proxy_loopback_only",
            "dev server listens on loopback only; relaying through amux (script-built URLs and HMR need --host 0.0.0.0)");
        return relay(local, port, &tail, method, &headers, body).await;
    }
    let target = format!("http://{host}:{port}{tail}");
    let from_loopback = peer.is_some_and(|ip: IpAddr| ip.is_loopback());
    tracing::info!(port, host = %host, from_loopback, measured = true, n_considered = 1,
        verdict = "port_proxy_redirect", "proxy link sent to the dev server's own port");
    Redirect::temporary(&target).into_response()
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
            .expect("proxy http client")
    })
}

/// Forward one request to `local:port` and answer with its response, for a
/// dev server the browser cannot reach directly. `tail` is the page and query.
async fn relay(local: &str, port: u16, tail: &str, method: Method, headers: &HeaderMap, body: Body) -> Response {
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
                verdict = "port_proxy_relay_failed", "relay to a loopback-only dev server failed");
            return page(StatusCode::BAD_GATEWAY, "The dev server did not answer",
                &format!("<p>Port {port} accepted a connection but the request failed.</p>"));
        }
    };
    let prefix = format!("/proxy/{port}");
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
        let Ok(hn) = HeaderName::from_bytes(name.as_bytes()) else { continue };
        let value = if name == "location" {
            v.to_str().ok().map(|l| rewrite_location(l, port, &prefix)).and_then(|l| HeaderValue::from_str(&l).ok())
        } else {
            HeaderValue::from_bytes(v.as_bytes()).ok()
        };
        if let Some(value) = value {
            out.append(hn, value);
        }
    }
    let body = if rewrite {
        match upstream.bytes().await {
            Ok(b) => Body::from(rewrite_html(&String::from_utf8_lossy(&b), &prefix)),
            Err(e) => {
                tracing::warn!(port, error = %e, measured = true, n_considered = 1,
                    verdict = "port_proxy_relay_failed", "the relayed page stopped mid-response");
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
                    verdict = "port_proxy_relay_failed", "the relayed response stopped mid-body");
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
        regex::Regex::new(r#"(\s(?:src|href|action|poster)=)(["'])/([^/"'][^"']*|)(["'])"#).expect("proxy html regex")
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
    fn a_proxy_path_names_its_port_and_keeps_the_page_and_query() {
        let u: Uri = "/proxy/3077/?hero=texture".parse().unwrap();
        assert_eq!(parse_target(&u), Some((3077, "/?hero=texture".to_string())));
        let u: Uri = "/proxy/5173".parse().unwrap();
        assert_eq!(parse_target(&u), Some((5173, "/".to_string())));
        let u: Uri = "/proxy/8080/a/b.js".parse().unwrap();
        assert_eq!(parse_target(&u), Some((8080, "/a/b.js".to_string())));
        for bad in ["/proxy/0/", "/proxy/70000/", "/proxy/abc/", "/proxy/"] {
            assert_eq!(parse_target(&bad.parse().unwrap()), None, "{bad}");
        }
    }

    #[test]
    fn the_host_keeps_its_name_and_drops_its_port() {
        assert_eq!(host_only("desktop.tail5ce8f5.ts.net:8824"), "desktop.tail5ce8f5.ts.net");
        assert_eq!(host_only("localhost"), "localhost");
        assert_eq!(host_only("[fd7a:115c:a1e0::1]:8824"), "[fd7a:115c:a1e0::1]");
    }

    #[test]
    fn relayed_html_routes_absolute_paths_back_through_the_proxy() {
        let p = "/proxy/3077";
        let out = rewrite_html(
            r#"<link href="/a.css"><script src='/b.js'></script><a href="//cdn.x/y">c</a><video poster="/p.jpg" src="rel.webm"><a href="/proxy/3077/z">"#,
            p,
        );
        assert!(out.contains(r#"href="/proxy/3077/a.css""#), "{out}");
        assert!(out.contains("src='/proxy/3077/b.js'"), "{out}");
        assert!(out.contains(r#"href="//cdn.x/y""#), "protocol-relative stays: {out}");
        assert!(out.contains(r#"poster="/proxy/3077/p.jpg""#), "{out}");
        assert!(out.contains(r#"src="rel.webm""#), "relative stays: {out}");
        assert!(out.contains(r#"href="/proxy/3077/z""#), "already proxied stays: {out}");
        assert!(rewrite_html(r#"<a href="/">home</a>"#, p).contains(r#"href="/proxy/3077/""#));
    }

    #[test]
    fn relayed_redirects_stay_inside_the_proxy() {
        let p = "/proxy/3077";
        assert_eq!(rewrite_location("/login", 3077, p), "/proxy/3077/login");
        assert_eq!(rewrite_location("http://localhost:3077/x?y=1", 3077, p), "/proxy/3077/x?y=1");
        assert_eq!(rewrite_location("http://127.0.0.1:3077", 3077, p), "/proxy/3077/");
        assert_eq!(rewrite_location("https://example.com/a", 3077, p), "https://example.com/a");
        assert_eq!(rewrite_location("/proxy/3077/a", 3077, p), "/proxy/3077/a");
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
        let r = relay("127.0.0.1", port, "/redir", Method::GET, &HeaderMap::new(), Body::empty()).await;
        assert_eq!(r.status(), StatusCode::FOUND);
        assert_eq!(r.headers().get("location").unwrap(), &format!("/proxy/{port}/landed"));
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
