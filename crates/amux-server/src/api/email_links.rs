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

async fn check_one(client: &reqwest::Client, url: String) -> Link {
    match client.get(&url).send().await {
        Ok(r) => {
            let code = r.status().as_u16();
            Link { verdict: status_verdict(code), why: format!("HTTP {code}"), url }
        }
        Err(e) if e.is_timeout() => Link { url, verdict: "warn", why: "no answer in 8s".into() },
        Err(e) => {
            // reqwest's Display hides the cause ("error sending request");
            // the source chain carries "dns error" / "Connection refused".
            let mut why = e.to_string();
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                why = s.to_string();
                src = s.source();
            }
            Link { url, verdict: "dead", why }
        }
    }
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
    let client = match reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => return json!({"measured": false, "why_unmeasured": format!("http client: {e}"), "checked": 0}),
    };
    let links = futures::future::join_all(list.into_iter().map(|u| check_one(&client, u))).await;
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
        tokio::spawn(async move { axum::serve(l, app).await.ok() });
        format!("http://{a}")
    }

    #[tokio::test]
    async fn a_dead_link_refuses_and_a_bot_wall_warns() {
        let base = serve().await;
        // A port nobody listens on: the connection fails.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let p = l.local_addr().unwrap().port();
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
