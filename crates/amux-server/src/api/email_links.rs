//! Link check at the email park step (owner 2026-11-24: "ensure all emails
//! always have a proper link"; "make sure links are always checked").
//!
//! Incident: worker `personal` parked apr_86ec3abdc160accc to an outside
//! recipient with "https://cal.mixpeek.com", a booking link it guessed. The
//! host has no DNS record. Nothing between the draft and the owner's Approve
//! button fetched it.
//!
//! Every URL in a worker's held send or reply is fetched (GET, redirects
//! followed, a browser user agent, 8 s each, all at once). A host that does
//! not resolve, a connection that fails, or a 404/410 refuses the park with
//! the dead URLs named (422 `email_dead_links`). A 403/429, a 5xx or a
//! timeout is a warning: bot walls answer curl that way while the page is
//! fine (zenodo returns 403). The result rides the approval's preview and
//! its card, so the owner sees "links: N checked, all live" or the warnings
//! before Approve. `AMUX_EMAIL_LINK_CHECK=0` turns it off.
//!
//! The fetch comes from this machine, and the worker learns the verdict, so a
//! link (or a redirect it takes) to a loopback, private, link-local or
//! tailnet address is never fetched: it is a warning, "not a public
//! address", and the connection is pinned to the address that was checked
//! so a second DNS answer cannot swap one in. Otherwise a held send could
//! make the server GET its own loopback routes or the LAN and report back.

use serde_json::{json, Value};
use std::time::Duration;

const MAX_URLS: usize = 20;
const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/130.0 Safari/537.36";

/// Every distinct http(s) URL in `text`, in order, trailing punctuation off.
pub(crate) fn urls(text: &str) -> Vec<String> {
    static RE: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    let Some(re) = RE.get_or_init(|| regex::Regex::new(r#"https?://[^\s<>"'`\]\[(){}|\\^]+"#).ok()) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for m in re.find_iter(text) {
        let u = m.as_str().trim_end_matches(['.', ',', ';', ':', '!', '?', '*', '_']);
        // "https://" alone names no host.
        let host = u.split("://").nth(1).unwrap_or("").split(['/', '?', '#']).next().unwrap_or("");
        if host.is_empty() {
            continue;
        }
        if !out.iter().any(|x| x == u) {
            out.push(u.to_string());
        }
    }
    out
}

/// One URL's verdict: "live", "warn" or "dead", with what was seen.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Link {
    pub url: String,
    pub verdict: &'static str,
    pub why: String,
}

/// The verdict for an HTTP status.
pub(crate) fn status_verdict(code: u16) -> &'static str {
    match code {
        404 | 410 => "dead",
        403 | 429 => "warn",
        500..=599 => "warn",
        _ => "live",
    }
}

const MAX_HOPS: usize = 5;

/// Where one hop may connect, decided before any byte is sent.
enum Target {
    Addr(std::net::SocketAddr),
    NoDns(String),
    NotPublic,
}

#[cfg(test)]
static TEST_ALLOW: std::sync::Mutex<Vec<std::net::SocketAddr>> = std::sync::Mutex::new(Vec::new());

/// Tests serve their links on loopback. Only an address a test registers here
/// is let through; every other non-public address stays refused in tests too.
#[cfg(test)]
pub(crate) fn allow_in_test(a: std::net::SocketAddr) {
    TEST_ALLOW.lock().unwrap().push(a);
}

fn target(host: &str, port: u16) -> Target {
    use std::net::ToSocketAddrs;
    let h = host.trim_start_matches('[').trim_end_matches(']');
    #[cfg(test)]
    if let Ok(ip) = h.parse::<std::net::IpAddr>() {
        let a = std::net::SocketAddr::new(ip, port);
        if TEST_ALLOW.lock().unwrap().contains(&a) {
            return Target::Addr(a);
        }
    }
    match (h, port).to_socket_addrs() {
        Err(e) => Target::NoDns(format!("dns error: {e}")),
        Ok(it) => {
            let addrs: Vec<_> = it.collect();
            if addrs.is_empty() {
                Target::NoDns("dns error: no address".into())
            } else if addrs.iter().all(|a| super::conversations::public_ip(a.ip())) {
                Target::Addr(addrs[0])
            } else {
                Target::NotPublic
            }
        }
    }
}

async fn check_one(url: String) -> Link {
    let Ok(mut at) = reqwest::Url::parse(&url) else {
        return Link { url, verdict: "dead", why: "not a URL".into() };
    };
    for _ in 0..=MAX_HOPS {
        let host = at.host_str().unwrap_or("").to_string();
        let port = at.port_or_known_default().unwrap_or(443);
        let h2 = host.clone();
        let addr = match tokio::task::spawn_blocking(move || target(&h2, port)).await {
            Ok(Target::Addr(a)) => a,
            Ok(Target::NoDns(why)) => return Link { url, verdict: "dead", why },
            Ok(Target::NotPublic) | Err(_) => {
                tracing::warn!(host = %host, verdict = "email_link_not_public",
                    "[email-links] a held send's link (or a redirect it took) names a non-public address; not fetched");
                return Link { url, verdict: "warn", why: "not a public address, not fetched".into() };
            }
        };
        let client = match reqwest::Client::builder()
            .user_agent(UA)
            .timeout(Duration::from_secs(8))
            .redirect(reqwest::redirect::Policy::none())
            .resolve(host.trim_start_matches('[').trim_end_matches(']'), addr)
            .build()
        {
            Ok(c) => c,
            Err(e) => return Link { url, verdict: "warn", why: format!("http client: {e}") },
        };
        match client.get(at.clone()).send().await {
            Ok(r) if r.status().is_redirection() => {
                match r.headers().get("location").and_then(|v| v.to_str().ok()).and_then(|l| at.join(l).ok()) {
                    Some(n) if matches!(n.scheme(), "http" | "https") => at = n,
                    _ => {
                        let code = r.status().as_u16();
                        return Link { verdict: status_verdict(code), why: format!("HTTP {code}"), url };
                    }
                }
            }
            Ok(r) => {
                let code = r.status().as_u16();
                return Link { verdict: status_verdict(code), why: format!("HTTP {code}"), url };
            }
            Err(e) if e.is_timeout() => return Link { url, verdict: "warn", why: "no answer in 8s".into() },
            Err(e) => {
                // reqwest's Display hides the cause ("error sending request");
                // the source chain carries "dns error" / "Connection refused".
                let mut why = e.to_string();
                let mut src = std::error::Error::source(&e);
                while let Some(s) = src {
                    why = s.to_string();
                    src = s.source();
                }
                return Link { url, verdict: "dead", why };
            }
        }
    }
    Link { url, verdict: "warn", why: format!("more than {MAX_HOPS} redirects") }
}

/// Check every URL in `body`. `measured` is false when the check is off or
/// the client could not be built, so an empty `links` never reads as "all live".
pub(crate) async fn check(body: &str) -> Value {
    if std::env::var("AMUX_EMAIL_LINK_CHECK").map(|v| v == "0").unwrap_or(false) {
        return json!({"measured": false, "why_unmeasured": "AMUX_EMAIL_LINK_CHECK=0", "checked": 0});
    }
    let all = urls(body);
    let skipped = all.len().saturating_sub(MAX_URLS);
    let list: Vec<String> = all.into_iter().take(MAX_URLS).collect();
    let links = futures::future::join_all(list.into_iter().map(check_one)).await;
    report(&links, skipped)
}

pub(crate) fn report(links: &[Link], skipped: usize) -> Value {
    let pick = |v: &str| -> Vec<Value> {
        links.iter().filter(|l| l.verdict == v).map(|l| json!({"url": l.url, "why": l.why})).collect()
    };
    let (dead, warn) = (pick("dead"), pick("warn"));
    let summary = if links.is_empty() {
        "no links".to_string()
    } else if dead.is_empty() && warn.is_empty() {
        format!("{} checked, all live", links.len())
    } else {
        let mut s = format!("{} checked", links.len());
        if !dead.is_empty() {
            s += &format!(", {} dead", dead.len());
        }
        if !warn.is_empty() {
            s += &format!(", {} to look at ({})", warn.len(),
                warn.iter().map(|w| format!("{} {}", w["url"].as_str().unwrap_or(""), w["why"].as_str().unwrap_or("")))
                    .collect::<Vec<_>>().join("; "));
        }
        s
    };
    json!({"measured": true, "checked": links.len(), "skipped": skipped,
           "dead": dead, "warn": warn, "summary": summary})
}

/// The refusal for a park with dead links, or None.
pub(crate) fn refusal(report: &Value) -> Option<Value> {
    let dead = report["dead"].as_array().filter(|d| !d.is_empty())?;
    let names = dead.iter().map(|d| format!("{} ({})", d["url"].as_str().unwrap_or(""), d["why"].as_str().unwrap_or("")))
        .collect::<Vec<_>>().join(", ");
    Some(json!({
        "error": format!("not held: the email links to {} that does not load: {names}. Fix or remove the link and send again; nothing was parked.",
                         if dead.len() == 1 { "a page" } else { "pages" }),
        "code": "email_dead_links",
        "dead": dead,
        "links": report,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_found_deduped_and_trimmed() {
        let b = "Book here: https://calendly.com/mixpeek. Or (https://mixpeek.com/demo), \
                 again https://calendly.com/mixpeek and <https://x.io/a?b=1#c>; mailto:a@b.co http://";
        assert_eq!(urls(b), vec!["https://calendly.com/mixpeek", "https://mixpeek.com/demo", "https://x.io/a?b=1#c"]);
    }

    #[test]
    fn status_verdicts_match_the_rule() {
        assert_eq!(status_verdict(200), "live");
        assert_eq!(status_verdict(301), "live");
        assert_eq!(status_verdict(401), "live");
        assert_eq!(status_verdict(404), "dead");
        assert_eq!(status_verdict(410), "dead");
        assert_eq!(status_verdict(403), "warn");
        assert_eq!(status_verdict(429), "warn");
        assert_eq!(status_verdict(503), "warn");
    }

    async fn serve() -> String {
        use axum::{http::StatusCode, routing::get, Router};
        let app = Router::new()
            .route("/ok", get(|| async { "fine" }))
            .route("/gone", get(|| async { StatusCode::NOT_FOUND }))
            .route("/wall", get(|| async { StatusCode::FORBIDDEN }))
            .route("/moved", get(|| async { axum::response::Redirect::temporary("/ok") }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        allow_in_test(a);
        tokio::spawn(async move { axum::serve(l, app).await.ok() });
        format!("http://{a}")
    }

    // Red-team run 7: the check is a fetch from this machine whose verdict the
    // worker reads back, so a loopback or private link is never requested,
    // whether named directly or reached through a public-looking redirect.
    #[tokio::test]
    async fn a_link_to_a_non_public_address_is_never_fetched() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = axum::Router::new().fallback(move || {
            let h = h.clone();
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                "inside"
            }
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let inside = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.ok() });
        // A registered server that redirects to the unregistered one.
        let hop = axum::Router::new().fallback(move || async move {
            axum::response::Redirect::temporary(&format!("http://{inside}/admin"))
        });
        let l2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hop_at = l2.local_addr().unwrap();
        allow_in_test(hop_at);
        tokio::spawn(async move { axum::serve(l2, hop).await.ok() });

        let r = check(&format!("a http://{inside}/api/x b http://localhost:{}/y c http://{hop_at}/go",
                               inside.port())).await;
        assert_eq!(hits.load(Ordering::SeqCst), 0, "a non-public address was requested: {r}");
        let warns: Vec<&str> = r["warn"].as_array().unwrap().iter().map(|w| w["why"].as_str().unwrap()).collect();
        assert_eq!(warns, vec!["not a public address, not fetched"; 3], "{r}");
        assert!(refusal(&r).is_none(), "a private link warns, it does not refuse the park");
    }

    #[tokio::test]
    async fn a_dead_link_refuses_and_a_bot_wall_warns() {
        let base = serve().await;
        // A port nobody listens on: the connection fails.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let p = l.local_addr().unwrap().port();
            allow_in_test(l.local_addr().unwrap());
            drop(l);
            format!("http://127.0.0.1:{p}/x")
        };
        let body = format!("a {base}/ok b {base}/moved c {base}/wall d {base}/gone e {closed}");
        let r = check(&body).await;
        assert_eq!(r["measured"], true);
        assert_eq!(r["checked"], 5);
        let dead: Vec<&str> = r["dead"].as_array().unwrap().iter().map(|d| d["url"].as_str().unwrap()).collect();
        assert_eq!(dead, vec![format!("{base}/gone").as_str(), closed.as_str()]);
        assert_eq!(r["warn"][0]["url"], format!("{base}/wall"));
        let refused = refusal(&r).expect("dead links refuse");
        assert_eq!(refused["code"], "email_dead_links");
        assert!(refused["error"].as_str().unwrap().contains("/gone (HTTP 404)"));

        let ok = check(&format!("see {base}/ok and {base}/moved")).await;
        assert_eq!(ok["summary"], "2 checked, all live");
        assert!(refusal(&ok).is_none());
        assert_eq!(check("no links here").await["summary"], "no links");
    }
}
