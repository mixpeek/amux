//! dev.to (Forem) publisher: an article from a markdown file, and its views,
//! reactions and comments back.
//!
//! The key is the `devto` connector credential `DEVTO_API_KEY`. A second
//! account is `DEVTO_API_KEY_<ACCOUNT>` (see [`social::credential`]), so a new
//! account is a credential, not code. Without `AMUX_SOCIAL_PUBLIC=1` the
//! article is saved as an unpublished draft and the reply says why.

use super::social::{self, SocialCtx};
use super::social_publish::{BoxFut, Draft, Failure, Published, Publisher};
use super::AppState;
use axum::http::{HeaderMap, StatusCode};
use serde_json::{json, Value};

pub struct DevTo;

/// dev.to keeps the first four tags, lowercase alphanumerics only.
fn devto_tags(tags: &[String]) -> Vec<String> {
    tags.iter()
        .map(|t| t.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .take(4)
        .collect()
}

fn key(ctx: &SocialCtx, d_account: &str) -> Result<String, Failure> {
    social::credential(ctx, "DEVTO_API_KEY", d_account).ok_or_else(|| {
        Failure::refused(
            StatusCode::BAD_REQUEST,
            "no dev.to key: set the devto connector's DEVTO_API_KEY (or DEVTO_API_KEY_<ACCOUNT> for another account)",
        )
    })
}

fn forem_error(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"].as_str().map(String::from))
        .unwrap_or_else(|| body.chars().take(300).collect())
}

impl Publisher for DevTo {
    fn platform(&self) -> &'static str {
        "devto"
    }

    fn check(&self, ctx: &SocialCtx, d: &Draft) -> Result<(), Failure> {
        key(ctx, &d.account)?;
        if d.title.as_deref().map(str::trim).unwrap_or("").is_empty() {
            return Err(Failure::refused(StatusCode::BAD_REQUEST, "a dev.to article needs a title"));
        }
        if d.file.is_empty() && d.caption.trim().is_empty() {
            return Err(Failure::refused(StatusCode::BAD_REQUEST, "a dev.to article needs a markdown file or a caption for its body"));
        }
        if !d.file.is_empty() && !std::path::Path::new(&d.file).is_file() {
            return Err(Failure::refused(StatusCode::BAD_REQUEST, format!("no such file: {}", d.file)));
        }
        Ok(())
    }

    fn publish<'a>(&'a self, ctx: &'a SocialCtx, _state: &'a AppState, _headers: HeaderMap, d: &'a Draft)
        -> BoxFut<'a, Result<Published, Failure>> {
        Box::pin(async move {
            let key = key(ctx, &d.account)?;
            let body = if d.file.is_empty() {
                d.caption.clone()
            } else {
                tokio::fs::read_to_string(&d.file)
                    .await
                    .map_err(|e| Failure::refused(StatusCode::BAD_REQUEST, format!("read {}: {e}", d.file)))?
            };
            let title = d.title.clone().unwrap_or_default().trim().to_string();
            let wants_public = d.privacy.as_deref().unwrap_or("public") != "private";
            let public_ok = social::env_val(ctx, "AMUX_SOCIAL_PUBLIC").as_deref() == Some("1");
            let published = wants_public && public_ok;
            let mut article = json!({"title": title, "body_markdown": body, "published": published, "tags": devto_tags(&d.tags)});
            if !d.file.is_empty() && !d.caption.trim().is_empty() {
                article["description"] = json!(d.caption.chars().take(300).collect::<String>());
            }
            let res = ctx
                .http
                .post(format!("{}/articles", ctx.devto_api))
                .header("api-key", key)
                .header("accept", "application/vnd.forem.api-v1+json")
                .json(&json!({"article": article}))
                .send()
                .await
                .map_err(|e| Failure::from_send("dev.to publish", &e))?;
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            if status.as_u16() == 429 || status.is_server_error() {
                return Err(Failure::retry(format!("dev.to publish: HTTP {status}: {}", forem_error(&text))));
            }
            if !status.is_success() {
                return Err(Failure::refused(StatusCode::BAD_GATEWAY, format!("dev.to refused the article: HTTP {status}: {}", forem_error(&text))));
            }
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let Some(id) = v["id"].as_i64().map(|n| n.to_string()).or_else(|| v["id"].as_str().map(String::from)) else {
                return Err(Failure::uncertain("dev.to answered without an article id"));
            };
            let mut notes = Vec::new();
            if wants_public && !public_ok {
                notes.push("saved as an unpublished draft: AMUX_SOCIAL_PUBLIC is not 1".to_string());
            }
            let live = v["published"].as_bool().unwrap_or(published);
            Ok(Published {
                url: v["url"].as_str().unwrap_or("").to_string(),
                post_id: id,
                visibility: if live { "public".into() } else { "draft".into() },
                title,
                notes,
                extra: json!({"published": live}),
            })
        })
    }

    fn metrics<'a>(&'a self, ctx: &'a SocialCtx, _state: &'a AppState, _headers: HeaderMap, post: &'a Value)
        -> BoxFut<'a, Result<Value, String>> {
        Box::pin(async move {
            let key = key(ctx, post["account"].as_str().unwrap_or("")).map_err(|f| f.error)?;
            let id = post["post_id"].as_str().unwrap_or("");
            // /articles/me/all is the only read that carries page views, and it
            // includes drafts. Pages of 1000 are the API's maximum.
            for page in 1..=10 {
                let res = ctx
                    .http
                    .get(format!("{}/articles/me/all?per_page=1000&page={page}", ctx.devto_api))
                    .header("api-key", &key)
                    .header("accept", "application/vnd.forem.api-v1+json")
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                let status = res.status();
                let text = res.text().await.unwrap_or_default();
                if !status.is_success() {
                    return Err(format!("HTTP {status}: {}", forem_error(&text)));
                }
                let items: Vec<Value> = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                if let Some(a) = items.iter().find(|a| a["id"].to_string().trim_matches('"') == id) {
                    let n = |k: &str| a[k].as_i64();
                    let (likes, comments) = (n("public_reactions_count"), n("comments_count"));
                    return Ok(json!({
                        "views": n("page_views_count"), "likes": likes, "comments": comments,
                        "engagement": match (likes, comments) { (Some(l), Some(c)) => Some(l + c), _ => None },
                        "published": a["published"], "url": a["url"],
                        "age_secs": social::now() - post["posted_at"].as_i64().unwrap_or(0),
                        "sources": {"articles_me": {"measured": true}},
                    }));
                }
                if items.len() < 1000 {
                    break;
                }
            }
            Err(format!("dev.to has no article {id} for this key"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::devto_tags;

    #[test]
    fn tags_are_cut_to_what_devto_keeps() {
        let t: Vec<String> = ["AI Agents", "rust", "dev-tools", "#tmux", "five"].iter().map(|s| s.to_string()).collect();
        assert_eq!(devto_tags(&t), ["aiagents", "rust", "devtools", "tmux"]);
    }
}
