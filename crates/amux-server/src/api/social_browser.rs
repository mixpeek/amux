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
//! Two accounts. `ethan` posts on Ethan Steininger's own profile from the feed
//! composer; `amux` posts as the amux company page (id 146906104, which the
//! `ethan` login administers) from the page's admin composer, and the
//! composer's identity is checked to read "amux" before anything is typed.
//! Ethan approved the page for automatic posting; his own profile needs his
//! sign-off per post (amux-gtm, 2026-10-30).
//!
//! The composers are found by role and text, never LinkedIn's generated class
//! names: the feed's div[role=textbox].ProseMirror with a "Media" button, the
//! page's Quill editor ("What do you want to talk about?") in the "Create post
//! modal" dialog with an "Add media" button; both post with a "Post" button.
//! Read 2026-10-30 from the live pages.

use super::social::{self, SocialCtx};
use super::social_publish::{BoxFut, Draft, Failure, Published, Publisher};
use super::AppState;
use axum::http::{HeaderMap, StatusCode};
use serde_json::{json, Value};
use std::time::Duration;

pub struct LinkedIn;

/// The saved browser profile signed in to the posting account.
const PROFILE: &str = "ethan";
/// The one browser session every browser publisher drives, apart from every
/// worker's. One per publisher let LinkedIn's session keep the `ethan` profile
/// and refuse X's start with 409 (2026-10-30, the first live X draft).
const SESSION: &str = "social-browser";
/// One browser publish at a time: a queued group posting to LinkedIn and X
/// shares the profile.
static PUBLISHING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Close the publisher's browser so the profile is free for the next publish,
/// a worker, or a person.
async fn release(state: &AppState) {
    if let Err(e) = super::browser::drive(state, SESSION, "stop", json!({"session": SESSION})).await {
        tracing::warn!(error = %e, verdict = "social_browser_stop_failed", "the social publisher could not close its browser");
    }
}
const FEED: &str = "https://www.linkedin.com/feed/";
/// LinkedIn's limit for a post's text.
const MAX_CHARS: usize = 3000;


fn is_draft(d: &Draft) -> bool {
    d.privacy.as_deref().map(str::trim) == Some("draft")
}

/// Posting as a person is named, never a default: with the public switch on
/// for every platform, a caller asking for "linkedin" could expect a company
/// page and post on Ethan's own profile instead.
fn account_ok(account: &str) -> bool {
    target(account).is_some()
}

/// Where one account's composer lives and how to drive it.
struct Target {
    account: &'static str,
    who: &'static str,
    /// The page that holds (or opens) the composer.
    url: &'static str,
    /// JavaScript returning true once the composer can be opened or is open.
    ready_js: &'static str,
    /// JavaScript that opens the composer, or "" when `url` opens it.
    open_js: &'static str,
    editor: &'static str,
    /// JavaScript returning true when the open composer posts as this account.
    identity_js: &'static str,
    media_label: &'static str,
}

const PERSONAL: Target = Target {
    account: "ethan",
    who: "Ethan Steininger",
    url: FEED,
    ready_js: "!![...document.querySelectorAll('[role=button],button')].find(e=>/^\\s*start a post\\s*$/i.test(e.innerText||''))",
    open_js: "(()=>{const e=[...document.querySelectorAll('[role=button],button')].find(e=>/^\\s*start a post\\s*$/i.test(e.innerText||''));if(!e)return false;e.click();return true})()",
    editor: "div[role=textbox].ProseMirror",
    identity_js: "true",
    media_label: "Media",
};

const PAGE: Target = Target {
    account: "amux",
    who: "the amux company page",
    url: "https://www.linkedin.com/company/146906104/admin/page-posts/published/?share=true",
    ready_js: "!![...document.querySelectorAll('[role=dialog]')].find(d=>/create post modal/i.test(d.innerText||''))",
    open_js: "",
    editor: "[role=dialog] div.ql-editor[role=textbox][aria-placeholder^='What do you want']",
    identity_js: "(()=>{const d=[...document.querySelectorAll('[role=dialog]')].find(d=>/create post modal/i.test(d.innerText||''));return !!d&&[...d.querySelectorAll('button')].some(b=>/^\\s*amux\\b/i.test(b.innerText||''))})()",
    media_label: "Add media",
};

fn target(account: &str) -> Option<&'static Target> {
    [&PERSONAL, &PAGE].into_iter().find(|t| t.account == account.trim())
}

/// Run a JavaScript expression in the LinkedIn session's page.
async fn eval(state: &AppState, script: &str) -> Result<Value, String> {
    eval_in(state, SESSION, script).await
}

/// Run a JavaScript expression in `session`'s page and return its value.
async fn eval_in(state: &AppState, session: &str, script: &str) -> Result<Value, String> {
    let v = super::browser::drive(state, session, "eval", json!({"script": script})).await?;
    Ok(v.get("result").cloned().or_else(|| v.pointer("/data/result").cloned()).unwrap_or(Value::Null))
}

/// Poll `script` in the LinkedIn session until it returns true, up to `secs`.
async fn until(state: &AppState, script: &str, secs: u64) -> Result<bool, String> {
    until_in(state, SESSION, script, secs).await
}

async fn until_in(state: &AppState, session: &str, script: &str, secs: u64) -> Result<bool, String> {
    for _ in 0..secs * 2 {
        if eval_in(state, session, script).await? == json!(true) {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(false)
}

const POST_BUTTON_JS: &str = "(()=>{const b=[...document.querySelectorAll('button')].find(b=>/^\\s*post\\s*$/i.test(b.innerText||''));return b&&!b.disabled?b:null})()";

/// Close the composer without posting, discarding what was typed.
async fn discard(state: &AppState) {
    let _ = super::browser::drive(state, SESSION, "key", json!({"key": "Escape"})).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let _ = eval(state, "(()=>{const b=[...document.querySelectorAll('button')].find(b=>/^\\s*discard\\s*$/i.test(b.innerText||''));if(b){b.click();return true}return false})()").await;
}

/// Open `t`'s composer with the caption (and file) in it. Nothing is sent.
async fn compose(state: &AppState, t: &Target, d: &Draft) -> Result<(), Failure> {
    let drive = |verb: &'static str, body: Value| super::browser::drive(state, SESSION, verb, body);
    drive("start", json!({"profile": PROFILE, "url": t.url, "session": SESSION})).await.map_err(Failure::retry)?;
    drive("navigate", json!({"url": t.url})).await.map_err(Failure::retry)?;
    if !until(state, t.ready_js, 30).await.map_err(Failure::retry)? {
        return Err(Failure::refused(
            StatusCode::FAILED_DEPENDENCY,
            format!("LinkedIn showed no composer for {} at {}: is the `{PROFILE}` browser profile signed in (and an admin of the page)?", t.who, t.url),
        ));
    }
    if !t.open_js.is_empty() {
        eval(state, t.open_js).await.map_err(Failure::retry)?;
    }
    let editor = t.editor;
    if !until(state, &format!("!!document.querySelector({})", json!(editor)), 15).await.map_err(Failure::retry)? {
        return Err(Failure::retry("the LinkedIn composer did not open"));
    }
    // Who the post would go out as, before a single character is typed.
    if eval(state, t.identity_js).await.map_err(Failure::retry)? != json!(true) {
        discard(state).await;
        return Err(Failure::refused(
            StatusCode::CONFLICT,
            format!("the LinkedIn composer is not posting as {}; nothing was typed", t.who),
        ));
    }
    drive("click", json!({"selector": editor})).await.map_err(Failure::retry)?;
    drive("type", json!({"text": d.caption})).await.map_err(Failure::retry)?;
    let head: String = d.caption.trim().chars().take(40).collect();
    let typed = eval(state, &format!("(document.querySelector({})?.innerText||'').includes({})", json!(editor), json!(head)))
        .await
        .map_err(Failure::retry)?;
    if typed != json!(true) {
        discard(state).await;
        return Err(Failure::retry("the caption did not land in the LinkedIn composer"));
    }
    if !d.file.is_empty() {
        eval(state, &format!("(()=>{{const b=[...document.querySelectorAll('button')].find(b=>b.getAttribute('aria-label')==={});if(b){{b.click();return true}}return false}})()", json!(t.media_label)))
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
                format!(
                    "name the LinkedIn account: \"amux\" posts as the amux company page, \"ethan\" on Ethan Steininger's own profile (his sign-off per post); {:?} is neither",
                    d.account
                ),
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
            let _one = PUBLISHING.lock().await;
            let out = linkedin_publish(state, d).await;
            release(state).await;
            out
        })
    }

    fn metrics<'a>(&'a self, _ctx: &'a SocialCtx, _state: &'a AppState, _headers: HeaderMap, _post: &'a Value)
        -> BoxFut<'a, Result<Value, String>> {
        Box::pin(async move { Err("LinkedIn metrics are not built for the browser publisher".to_string()) })
    }
}

async fn linkedin_publish(state: &AppState, d: &Draft) -> Result<Published, Failure> {
        {
            let t = target(&d.account).expect("checked");
            compose(state, t, d).await?;
            if is_draft(d) {
                discard(state).await;
                tracing::info!(platform = "linkedin", account = t.account, chars = d.caption.chars().count(), file = !d.file.is_empty(),
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
            // The confirmation's "View post" link, or (a page lands on its post
            // list) the newest post carrying this caption.
            let head: String = d.caption.trim().chars().take(40).collect();
            let link_js = format!(
                "(()=>{{const a=[...document.querySelectorAll('a[href*=\"/feed/update/urn:li:\"]')].find(a=>/view post/i.test(a.innerText||'')||a.closest('[role=alert],[aria-live]'));if(a)return a.href;const p=[...document.querySelectorAll('[data-urn^=\"urn:li:activity:\"]')].find(e=>(e.innerText||'').includes({}));return p?'https://www.linkedin.com/feed/update/'+p.getAttribute('data-urn')+'/':''}})()",
                json!(head)
            );
            let mut url = String::new();
            for _ in 0..60 {
                if let Ok(Value::String(h)) = eval(state, &link_js).await {
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
            tracing::info!(platform = "linkedin", account = t.account, post_id = %urn, verdict = "social_browser_posted", "LinkedIn post published through the browser");
            Ok(Published {
                post_id: urn,
                url,
                visibility: "public".into(),
                title: String::new(),
                notes: vec![format!("posted through the amux browser as {}", t.who)],
                extra: json!({"via": "browser", "profile": PROFILE, "account": t.account}),
            })
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
        Draft { platform: "linkedin".into(), account: "ethan".into(), caption: caption.into(), privacy: privacy.map(String::from), ..Default::default() }
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
        d.account = "amux".into();
        assert!(p.check(&open, &d).is_ok(), "the amux company page posts");
        assert_eq!(target("amux").map(|t| t.url), Some(PAGE.url));
        d.account = "company:amux".into();
        assert!(p.check(&open, &d).unwrap_err().error.contains("is neither"));
        d.account = String::new();
        assert!(p.check(&open, &d).unwrap_err().error.contains("name the LinkedIn account"), "posting as a person is never the default");
        let mut d = draft("hi", None);
        d.file = "/nonexistent/clip.mp4".into();
        assert!(p.check(&open, &d).unwrap_err().error.contains("no such file"));
    }
}

// ── X ──────────────────────────────────────────────────────────────────────

/// X (Twitter) through the same `ethan` profile, which is signed in as
/// @ethansteininger: Ethan's own account, so `account` must say "ethan" and
/// each post is his to approve (amux-gtm, 2026-10-30). The composer is
/// x.com/compose/post, found by data-testid (tweetTextarea_0, fileInput,
/// tweetButton) and read 2026-10-30 from the live page, nothing typed.
/// API v2 posting needs a paid tier, which is the owner's decision.
pub struct X;

const X_HANDLE: &str = "ethansteininger";
const X_COMPOSE: &str = "https://x.com/compose/post";
/// The limit for an account without Premium.
const X_MAX_CHARS: usize = 280;
const X_EDITOR: &str = "[role=dialog] [data-testid=tweetTextarea_0]";
const X_POST_JS: &str = "(()=>{const b=document.querySelector('[role=dialog] [data-testid=tweetButton]');return b&&!b.disabled&&b.getAttribute('aria-disabled')!=='true'?b:null})()";

async fn x_discard(state: &AppState) {
    let _ = eval_in(state, SESSION, "(()=>{const b=document.querySelector('[data-testid=app-bar-close]');if(b)b.click();return !!b})()").await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let _ = eval_in(state, SESSION, "(()=>{const b=[...document.querySelectorAll('[role=button],button')].find(b=>/^\\s*discard\\s*$/i.test(b.innerText||''));if(b){b.click();return true}return false})()").await;
}

async fn x_compose(state: &AppState, d: &Draft) -> Result<(), Failure> {
    let drive = |verb: &'static str, body: Value| super::browser::drive(state, SESSION, verb, body);
    drive("start", json!({"profile": PROFILE, "url": X_COMPOSE, "session": SESSION})).await.map_err(Failure::retry)?;
    drive("navigate", json!({"url": X_COMPOSE})).await.map_err(Failure::retry)?;
    if !until_in(state, SESSION, &format!("!!document.querySelector({})", json!(X_EDITOR)), 30).await.map_err(Failure::retry)? {
        return Err(Failure::refused(
            StatusCode::FAILED_DEPENDENCY,
            format!("X showed no composer at {X_COMPOSE}: is the `{PROFILE}` browser profile signed in?"),
        ));
    }
    let who = format!("(document.querySelector('a[data-testid=AppTabBar_Profile_Link]')?.getAttribute('href')||'').toLowerCase()==='/{X_HANDLE}'");
    if eval_in(state, SESSION, &who).await.map_err(Failure::retry)? != json!(true) {
        x_discard(state).await;
        return Err(Failure::refused(StatusCode::CONFLICT, format!("X is not signed in as @{X_HANDLE}; nothing was typed")));
    }
    drive("click", json!({"selector": X_EDITOR})).await.map_err(Failure::retry)?;
    drive("type", json!({"text": d.caption})).await.map_err(Failure::retry)?;
    let head: String = d.caption.trim().chars().take(40).collect();
    if eval_in(state, SESSION, &format!("(document.querySelector({})?.innerText||'').includes({})", json!(X_EDITOR), json!(head)))
        .await
        .map_err(Failure::retry)?
        != json!(true)
    {
        x_discard(state).await;
        return Err(Failure::retry("the caption did not land in X's composer"));
    }
    if !d.file.is_empty() {
        drive("files", json!({"selector": "[role=dialog] input[data-testid=fileInput]", "files": [d.file]})).await.map_err(Failure::retry)?;
        // Post is enabled again once the upload finishes.
        if !until_in(state, SESSION, &format!("!!{X_POST_JS}"), 180).await.map_err(Failure::retry)? {
            x_discard(state).await;
            return Err(Failure::retry("the media did not finish uploading to X"));
        }
    }
    Ok(())
}

impl Publisher for X {
    fn platform(&self) -> &'static str {
        "x"
    }

    fn check(&self, ctx: &SocialCtx, d: &Draft) -> Result<(), Failure> {
        if d.account.trim() != PROFILE {
            return Err(Failure::refused(
                StatusCode::BAD_REQUEST,
                format!("name the X account: \"ethan\" posts as @{X_HANDLE}, Ethan's own account (his sign-off per post); {:?} is not one", d.account),
            ));
        }
        let n = d.caption.trim().chars().count();
        if n == 0 {
            return Err(Failure::refused(StatusCode::BAD_REQUEST, "an X post needs a caption"));
        }
        if n > X_MAX_CHARS {
            return Err(Failure::refused(StatusCode::BAD_REQUEST, format!("an X post is at most {X_MAX_CHARS} characters; this one is {n}")));
        }
        if !d.file.is_empty() && !std::path::Path::new(&d.file).is_file() {
            return Err(Failure::refused(StatusCode::BAD_REQUEST, format!("no such file: {}", d.file)));
        }
        let public_ok = social::env_val(ctx, "AMUX_SOCIAL_PUBLIC").as_deref() == Some("1");
        if !public_ok && !is_draft(d) {
            return Err(Failure::refused(
                StatusCode::FORBIDDEN,
                "an X post is public: set AMUX_SOCIAL_PUBLIC=1 (the owner's switch), or privacy \"draft\" to compose and discard",
            ));
        }
        Ok(())
    }

    fn publish<'a>(&'a self, ctx: &'a SocialCtx, state: &'a AppState, _headers: HeaderMap, d: &'a Draft)
        -> BoxFut<'a, Result<Published, Failure>> {
        Box::pin(async move {
            self.check(ctx, d)?;
            let _one = PUBLISHING.lock().await;
            let out = x_publish(state, d).await;
            release(state).await;
            out
        })
    }

    fn metrics<'a>(&'a self, _ctx: &'a SocialCtx, _state: &'a AppState, _headers: HeaderMap, _post: &'a Value)
        -> BoxFut<'a, Result<Value, String>> {
        Box::pin(async move { Err("X metrics are not built for the browser publisher".to_string()) })
    }
}

async fn x_publish(state: &AppState, d: &Draft) -> Result<Published, Failure> {
        {
            x_compose(state, d).await?;
            if is_draft(d) {
                x_discard(state).await;
                tracing::info!(platform = "x", account = PROFILE, chars = d.caption.chars().count(), file = !d.file.is_empty(),
                    verdict = "social_browser_draft_composed", "X post composed and discarded (privacy draft)");
                return Err(Failure::refused(StatusCode::OK, "composed in X's composer and discarded: privacy was draft, nothing was posted")
                    .with(json!({"draft": true, "composed": true})));
            }
            let clicked = eval_in(state, SESSION, &format!("(()=>{{const b={X_POST_JS};if(!b)return false;b.click();return true}})()"))
                .await
                .map_err(|e| Failure::uncertain(format!("clicking Post: {e}")))?;
            if clicked != json!(true) {
                x_discard(state).await;
                return Err(Failure::retry("X's Post button was not enabled"));
            }
            // The "Your post was sent" toast links to the post.
            let link_js = format!("(()=>{{const a=[...document.querySelectorAll('[data-testid=toast] a[href*=\"/status/\"]')].find(a=>a.getAttribute('href').toLowerCase().startsWith('/{X_HANDLE}/status/'));return a?a.href:''}})()");
            let mut url = String::new();
            for _ in 0..60 {
                if let Ok(Value::String(h)) = eval_in(state, SESSION, &link_js).await {
                    if !h.is_empty() {
                        url = h;
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            let Some(id) = url.split("/status/").nth(1).map(|s| s.split(['/', '?']).next().unwrap_or("").to_string()).filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())) else {
                return Err(Failure::uncertain(format!("clicked Post, but X showed no link to the post within 30 s: check @{X_HANDLE} before resending")));
            };
            tracing::info!(platform = "x", account = PROFILE, post_id = %id, verdict = "social_browser_posted", "X post published through the browser");
            Ok(Published {
                post_id: id,
                url,
                visibility: "public".into(),
                title: String::new(),
                notes: vec![format!("posted through the amux browser as @{X_HANDLE}")],
                extra: json!({"via": "browser", "profile": PROFILE, "account": PROFILE}),
            })
        }
}

#[cfg(test)]
mod x_tests {
    use super::*;

    fn ctx(pairs: &'static [(&'static str, &'static str)]) -> SocialCtx {
        SocialCtx {
            env: std::sync::Arc::new(move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())),
            ..SocialCtx::live()
        }
    }

    fn draft(account: &str, caption: &str, privacy: Option<&str>) -> Draft {
        Draft { platform: "x".into(), account: account.into(), caption: caption.into(), privacy: privacy.map(String::from), ..Default::default() }
    }

    #[test]
    fn x_names_its_account_and_the_public_switch_is_the_owners() {
        let shut = ctx(&[]);
        let open = ctx(&[("AMUX_SOCIAL_PUBLIC", "1")]);
        assert!(X.check(&open, &draft("ethan", "hello", None)).is_ok());
        assert!(X.check(&open, &draft("", "hello", None)).unwrap_err().error.contains("name the X account"), "posting as a person is never the default");
        assert!(X.check(&open, &draft("amux", "hello", None)).is_err(), "no amux X account is signed in");
        assert!(X.check(&shut, &draft("ethan", "hello", None)).unwrap_err().error.contains("AMUX_SOCIAL_PUBLIC"));
        assert!(X.check(&shut, &draft("ethan", "hello", Some("draft"))).is_ok());
        assert!(X.check(&open, &draft("ethan", &"x".repeat(X_MAX_CHARS + 1), None)).is_err());
    }
}
