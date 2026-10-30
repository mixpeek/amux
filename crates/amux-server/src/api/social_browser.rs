//! Social publishers that post through the amux browser, for platforms whose
//! posting API is gated (LinkedIn's Community Management approval, X's paid
//! tier). The browser is the saved profile signed in to the account, driven
//! through the same in-process handlers as /api/browser (browser::drive), so
//! scope, ownership and the audit trail apply as they do to a worker.
//!
//! LinkedIn has no private post, so without `AMUX_SOCIAL_PUBLIC=1` a draft is
//! refused when it is queued. `privacy: "draft"` composes the post in the real
//! composer and discards it: the way to check the flow without posting.
//! Anything that goes wrong after Post is clicked is `uncertain`, never
//! retried, because a second send is a duplicate public post.
//!
//! The composer is found by role and text (div[role=textbox].ProseMirror, a
//! button labelled "Media", a button reading "Post"); LinkedIn's class names
//! are generated. Read 2026-10-30 from the live page.

use super::social::{self, SocialCtx};
use super::social_publish::{BoxFut, Draft, Failure, Published, Publisher};
use super::AppState;
use axum::http::{HeaderMap, StatusCode};
use serde_json::{json, Value};
use std::time::Duration;

pub struct LinkedIn;

/// The saved browser profile signed in to the posting account.
const PROFILE: &str = "ethan";
/// The browser session the publisher drives, apart from every worker's.
const SESSION: &str = "social-linkedin";
const FEED: &str = "https://www.linkedin.com/feed/";
/// LinkedIn's limit for a post's text.
const MAX_CHARS: usize = 3000;

const EDITOR: &str = "div[role=textbox].ProseMirror";

fn is_draft(d: &Draft) -> bool {
    d.privacy.as_deref().map(str::trim) == Some("draft")
}

fn account_ok(account: &str) -> bool {
    matches!(account.trim(), "" | "me" | "ethan")
}

/// Run a JavaScript expression in the page and return its value.
async fn eval(state: &AppState, script: &str) -> Result<Value, String> {
    let v = super::browser::drive(state, SESSION, "eval", json!({"script": script})).await?;
    Ok(v.get("result").cloned().or_else(|| v.pointer("/data/result").cloned()).unwrap_or(Value::Null))
}

/// Poll `script` until it returns true, up to `secs`.
async fn until(state: &AppState, script: &str, secs: u64) -> Result<bool, String> {
    for _ in 0..secs * 2 {
        if eval(state, script).await? == json!(true) {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(false)
}

fn start_button_js(click: bool) -> String {
    format!(
        "(()=>{{const e=[...document.querySelectorAll('[role=button],button')].find(e=>/^\\s*start a post\\s*$/i.test(e.innerText||''));if(!e)return false;{}return true}})()",
        if click { "e.click();" } else { "" }
    )
}

const POST_BUTTON_JS: &str = "(()=>{const b=[...document.querySelectorAll('button')].find(b=>/^\\s*post\\s*$/i.test(b.innerText||''));return b&&!b.disabled?b:null})()";

/// Close the composer without posting, discarding what was typed.
async fn discard(state: &AppState) {
    let _ = super::browser::drive(state, SESSION, "key", json!({"key": "Escape"})).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let _ = eval(state, "(()=>{const b=[...document.querySelectorAll('button')].find(b=>/^\\s*discard\\s*$/i.test(b.innerText||''));if(b){b.click();return true}return false})()").await;
}

/// Open the composer with the caption (and file) in it. Nothing is sent.
async fn compose(state: &AppState, d: &Draft) -> Result<(), Failure> {
    let drive = |verb: &'static str, body: Value| super::browser::drive(state, SESSION, verb, body);
    drive("start", json!({"profile": PROFILE, "url": FEED, "session": SESSION})).await.map_err(Failure::retry)?;
    drive("navigate", json!({"url": FEED})).await.map_err(Failure::retry)?;
    if !until(state, &start_button_js(false), 30).await.map_err(Failure::retry)? {
        return Err(Failure::refused(
            StatusCode::FAILED_DEPENDENCY,
            format!("LinkedIn shows no \"Start a post\" on the feed: is the `{PROFILE}` browser profile signed in?"),
        ));
    }
    eval(state, &start_button_js(true)).await.map_err(Failure::retry)?;
    if !until(state, &format!("!!document.querySelector('{EDITOR}')"), 15).await.map_err(Failure::retry)? {
        return Err(Failure::retry("the LinkedIn composer did not open"));
    }
    drive("click", json!({"selector": EDITOR})).await.map_err(Failure::retry)?;
    drive("type", json!({"text": d.caption})).await.map_err(Failure::retry)?;
    let head: String = d.caption.trim().chars().take(40).collect();
    let typed = eval(state, &format!("(document.querySelector('{EDITOR}')?.innerText||'').includes({})", json!(head)))
        .await
        .map_err(Failure::retry)?;
    if typed != json!(true) {
        discard(state).await;
        return Err(Failure::retry("the caption did not land in the LinkedIn composer"));
    }
    if !d.file.is_empty() {
        eval(state, "(()=>{const b=[...document.querySelectorAll('button')].find(b=>b.getAttribute('aria-label')==='Media');if(b){b.click();return true}return false})()")
            .await
            .map_err(Failure::retry)?;
        if !until(state, "!!document.querySelector('input[type=file]')", 15).await.map_err(Failure::retry)? {
            discard(state).await;
            return Err(Failure::retry("LinkedIn's media picker did not open"));
        }
        drive("files", json!({"selector": "input[type=file]", "files": [d.file]})).await.map_err(Failure::retry)?;
        // The media editor ends in Next (image) or Done; back in the composer,
        // Post is enabled once the upload finishes.
        for _ in 0..3 {
            let _ = eval(state, "(()=>{const b=[...document.querySelectorAll('button')].find(b=>/^\\s*(next|done)\\s*$/i.test(b.innerText||'')&&!b.disabled);if(b){b.click();return true}return false})()").await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if !until(state, &format!("!!{POST_BUTTON_JS}"), 120).await.map_err(Failure::retry)? {
            discard(state).await;
            return Err(Failure::retry("the media did not finish uploading to LinkedIn"));
        }
    }
    Ok(())
}

impl Publisher for LinkedIn {
    fn platform(&self) -> &'static str {
        "linkedin"
    }

    fn check(&self, ctx: &SocialCtx, d: &Draft) -> Result<(), Failure> {
        if !account_ok(&d.account) {
            return Err(Failure::refused(
                StatusCode::BAD_REQUEST,
                format!("LinkedIn posts as Ethan Steininger (the `{PROFILE}` profile); account {:?}, a company page, is not built", d.account),
            ));
        }
        let n = d.caption.trim().chars().count();
        if n == 0 {
            return Err(Failure::refused(StatusCode::BAD_REQUEST, "a LinkedIn post needs a caption"));
        }
        if n > MAX_CHARS {
            return Err(Failure::refused(StatusCode::BAD_REQUEST, format!("a LinkedIn post is at most {MAX_CHARS} characters; this one is {n}")));
        }
        if !d.file.is_empty() && !std::path::Path::new(&d.file).is_file() {
            return Err(Failure::refused(StatusCode::BAD_REQUEST, format!("no such file: {}", d.file)));
        }
        let public_ok = social::env_val(ctx, "AMUX_SOCIAL_PUBLIC").as_deref() == Some("1");
        if !public_ok && !is_draft(d) {
            return Err(Failure::refused(
                StatusCode::FORBIDDEN,
                "LinkedIn has no private post: set AMUX_SOCIAL_PUBLIC=1 (the owner's switch), or privacy \"draft\" to compose and discard",
            ));
        }
        Ok(())
    }

    fn publish<'a>(&'a self, ctx: &'a SocialCtx, state: &'a AppState, _headers: HeaderMap, d: &'a Draft)
        -> BoxFut<'a, Result<Published, Failure>> {
        Box::pin(async move {
            self.check(ctx, d)?;
            compose(state, d).await?;
            if is_draft(d) {
                discard(state).await;
                tracing::info!(platform = "linkedin", chars = d.caption.chars().count(), file = !d.file.is_empty(),
                    verdict = "social_browser_draft_composed", "LinkedIn post composed and discarded (privacy draft)");
                return Err(Failure::refused(StatusCode::OK, "composed in LinkedIn's composer and discarded: privacy was draft, nothing was posted")
                    .with(json!({"draft": true, "composed": true})));
            }
            // From here the platform may receive it: every failure is uncertain.
            let clicked = eval(state, &format!("(()=>{{const b={POST_BUTTON_JS};if(!b)return false;b.click();return true}})()"))
                .await
                .map_err(|e| Failure::uncertain(format!("clicking Post: {e}")))?;
            if clicked != json!(true) {
                discard(state).await;
                return Err(Failure::retry("LinkedIn's Post button was not enabled"));
            }
            let link_js = "(()=>{const a=[...document.querySelectorAll('a[href*=\"/feed/update/urn:li:\"]')].find(a=>/view post/i.test(a.innerText||'')||a.closest('[role=alert],[aria-live]'));return a?a.href:''})()";
            let mut url = String::new();
            for _ in 0..60 {
                if let Ok(Value::String(h)) = eval(state, link_js).await {
                    if !h.is_empty() {
                        url = h;
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            let Some(urn) = url.split("/feed/update/").nth(1).map(|s| s.trim_end_matches('/').split('?').next().unwrap_or("").to_string()).filter(|s| s.starts_with("urn:li:")) else {
                return Err(Failure::uncertain("clicked Post, but LinkedIn showed no \"View post\" link within 30 s: check the feed before resending"));
            };
            tracing::info!(platform = "linkedin", post_id = %urn, verdict = "social_browser_posted", "LinkedIn post published through the browser");
            Ok(Published {
                post_id: urn,
                url,
                visibility: "public".into(),
                title: String::new(),
                notes: vec!["posted through the amux browser as Ethan Steininger".into()],
                extra: json!({"via": "browser", "profile": PROFILE}),
            })
        })
    }

    fn metrics<'a>(&'a self, _ctx: &'a SocialCtx, _state: &'a AppState, _headers: HeaderMap, _post: &'a Value)
        -> BoxFut<'a, Result<Value, String>> {
        Box::pin(async move { Err("LinkedIn metrics are not built for the browser publisher".to_string()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(pairs: &'static [(&'static str, &'static str)]) -> SocialCtx {
        SocialCtx {
            env: std::sync::Arc::new(move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())),
            ..SocialCtx::live()
        }
    }

    fn draft(caption: &str, privacy: Option<&str>) -> Draft {
        Draft { platform: "linkedin".into(), caption: caption.into(), privacy: privacy.map(String::from), ..Default::default() }
    }

    #[test]
    fn checks_need_no_browser_and_the_public_switch_is_the_owners() {
        let shut = ctx(&[]);
        let open = ctx(&[("AMUX_SOCIAL_PUBLIC", "1")]);
        let p = LinkedIn;
        assert!(p.check(&open, &draft("hello from amux", None)).is_ok());
        let e = p.check(&shut, &draft("hello from amux", None)).unwrap_err();
        assert!(e.error.contains("AMUX_SOCIAL_PUBLIC"), "{}", e.error);
        assert!(p.check(&shut, &draft("hello from amux", Some("draft"))).is_ok(), "a draft composes without the switch");
        assert!(p.check(&open, &draft("   ", None)).is_err(), "an empty caption");
        assert!(p.check(&open, &draft(&"x".repeat(MAX_CHARS + 1), None)).is_err(), "over LinkedIn's limit");
        let mut d = draft("hi", None);
        d.account = "company:amux".into();
        assert!(p.check(&open, &d).unwrap_err().error.contains("company page"));
        let mut d = draft("hi", None);
        d.file = "/nonexistent/clip.mp4".into();
        assert!(p.check(&open, &d).unwrap_err().error.contains("no such file"));
    }
}
