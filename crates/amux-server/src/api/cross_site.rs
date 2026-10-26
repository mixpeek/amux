//! Refuse state-changing requests a browser sends on behalf of another site.
//!
//! On an amux Cloud server the API is authorized by the cloud door's cookie
//! (`AMUX_AUTH_TOKEN=none` behind Caddy), and that cookie is `SameSite=Lax`.
//! Every customer server lives under amux.io, so to a browser
//! `tubescience.amux.io` and `sandbox.amux.io` are the SAME site: a page on one
//! could make the owner's browser POST to the other's /api/ with the door
//! cookie attached, and amux acted on it (2026-10-25: no Origin or Sec-Fetch
//! check existed anywhere). A body-less POST such as an approve or a stop needs
//! no CORS preflight, and a WebSocket handshake is never subject to CORS.
//!
//! The rule is about what a BROWSER says: `Sec-Fetch-Site: cross-site` or
//! `same-site` on a write (or a WebSocket upgrade) is refused; when that header
//! is absent, an `Origin` that is not this server's own is refused. A request
//! with neither header is not a browser acting for a page (the CLI, workers,
//! webhooks, the native apps) and passes, as before.
//!
//! `/pub/` and `/app/` are exempt: they relay to apps whose own requests are the
//! app's business, and they never carry the owner's authority.
use axum::extract::Request;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Why a request is refused, or None to let it through.
pub(crate) fn refusal(method: &Method, path: &str, authority: Option<&str>, headers: &HeaderMap) -> Option<&'static str> {
    if path.starts_with("/pub/") || path.starts_with("/app/") {
        return None;
    }
    let upgrade = headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let writes = !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    if !writes && !upgrade {
        return None;
    }
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        return match site.trim().to_ascii_lowercase().as_str() {
            "cross-site" => Some("cross-site"),
            "same-site" => Some("same-site"),
            _ => None, // same-origin, none (typed URL, bookmark)
        };
    }
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())?.trim();
    if origin.eq_ignore_ascii_case("null") {
        return Some("opaque-origin");
    }
    // HTTP/2 carries the host as :authority (in the URI), not a Host header.
    let own = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .or(authority)
        .map(|h| h.split(',').next().unwrap_or("").trim())
        .unwrap_or("");
    match authority_of(origin) {
        Some(o) if same_authority(&o, own, origin.starts_with("https://")) => None,
        _ => Some("foreign-origin"),
    }
}

/// `https://Host:port` -> `host:port` (lowercase), None if not an http(s) origin.
fn authority_of(origin: &str) -> Option<String> {
    let rest = origin.strip_prefix("https://").or_else(|| origin.strip_prefix("http://"))?;
    let a = rest.split('/').next().unwrap_or("");
    (!a.is_empty()).then(|| a.to_ascii_lowercase())
}

/// Same host and port, filling in the scheme's default port on either side.
fn same_authority(origin: &str, own: &str, https: bool) -> bool {
    let default = if https { "443" } else { "80" };
    let split = |a: &str| -> (String, String) {
        let a = a.to_ascii_lowercase();
        match a.rsplit_once(':') {
            Some((h, p)) if !h.is_empty() && !a.ends_with(']') && p.bytes().all(|b| b.is_ascii_digit()) => (h.to_string(), p.to_string()),
            _ => (a.clone(), default.to_string()),
        }
    };
    !own.is_empty() && split(origin) == split(own)
}

pub(crate) async fn guard(req: Request, next: Next) -> Response {
    if let Some(site) = refusal(req.method(), req.uri().path(), req.uri().authority().map(|a| a.as_str()), req.headers()) {
        tracing::warn!(method = %req.method(), path = %req.uri().path(), site, measured = true, n_considered = 1,
            verdict = "cross_site_request_refused",
            "a browser sent a state-changing request on behalf of another site; refused");
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":"cross-site request refused: open amux at its own address and try again"}"#).into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        m
    }

    #[test]
    fn the_decision_table() {
        let post = Method::POST;
        let api = "/api/page-links/x/approve";
        // A browser on another site, or another origin of the same site.
        assert_eq!(refusal(&post, api, None, &h(&[("sec-fetch-site", "cross-site")])), Some("cross-site"));
        assert_eq!(refusal(&post, api, None, &h(&[("sec-fetch-site", "same-site")])), Some("same-site"));
        assert_eq!(refusal(&Method::DELETE, api, None, &h(&[("sec-fetch-site", "same-site")])), Some("same-site"));
        // The dashboard itself, and a typed URL.
        assert_eq!(refusal(&post, api, None, &h(&[("sec-fetch-site", "same-origin")])), None);
        assert_eq!(refusal(&post, api, None, &h(&[("sec-fetch-site", "none")])), None);
        // No browser headers: the CLI, a worker, a webhook, a native app.
        assert_eq!(refusal(&post, api, None, &HeaderMap::new()), None);
        // Reads pass; a WebSocket handshake is checked like a write.
        assert_eq!(refusal(&Method::GET, api, None, &h(&[("sec-fetch-site", "cross-site")])), None);
        assert_eq!(refusal(&Method::GET, "/api/rt", None, &h(&[("upgrade", "websocket"), ("sec-fetch-site", "same-site")])), Some("same-site"));
        assert_eq!(refusal(&Method::GET, "/api/rt", None, &h(&[("upgrade", "websocket"), ("origin", "https://tubescience.amux.io"), ("host", "sandbox.amux.io")])), Some("foreign-origin"));
        // Without Sec-Fetch-Site, Origin decides.
        assert_eq!(refusal(&post, api, None, &h(&[("origin", "https://sandbox.amux.io"), ("host", "sandbox.amux.io")])), None);
        assert_eq!(refusal(&post, api, None, &h(&[("origin", "https://sandbox.amux.io"), ("host", "127.0.0.1:8824"), ("x-forwarded-host", "sandbox.amux.io")])), None);
        assert_eq!(refusal(&post, api, None, &h(&[("origin", "https://tubescience.amux.io"), ("host", "sandbox.amux.io")])), Some("foreign-origin"));
        assert_eq!(refusal(&post, api, None, &h(&[("origin", "https://sandbox.amux.io:8443"), ("host", "sandbox.amux.io")])), Some("foreign-origin"));
        assert_eq!(refusal(&post, api, None, &h(&[("origin", "null"), ("host", "sandbox.amux.io")])), Some("opaque-origin"));
        assert_eq!(refusal(&post, api, None, &h(&[("origin", "https://localhost:8824"), ("host", "localhost:8824")])), None);
        // HTTP/2: no Host header, the authority comes from the URI.
        assert_eq!(refusal(&post, api, Some("sandbox.amux.io"), &h(&[("origin", "https://sandbox.amux.io")])), None);
        assert_eq!(refusal(&post, api, Some("sandbox.amux.io"), &h(&[("origin", "https://other.amux.io")])), Some("foreign-origin"));
        // Relays to apps are the app's business.
        assert_eq!(refusal(&post, "/pub/abc/form", None, &h(&[("sec-fetch-site", "cross-site")])), None);
        assert_eq!(refusal(&post, "/app/abc/form", None, &h(&[("sec-fetch-site", "same-site")])), None);
    }
}
