//! Runtime-declared OAuth connectors use the same broker, leases and durable
//! grant store as builtins. No provider-specific fallback to Slack/Google.
use super::*;
use std::{collections::BTreeMap, fs::File, path::Path as FsPath};

pub(super) async fn registry_lease(home: &FsPath) -> std::io::Result<File> {
    let path = custom_store_path(home);
    tokio::task::spawn_blocking(move || crate::integrations::secure_store::lock(&path))
        .await
        .map_err(std::io::Error::other)?
}

pub(super) fn is_disconnected(path: &FsPath) -> bool {
    crate::integrations::secure_store::read_json::<Value>(path)
        .ok()
        .is_some_and(|v| v["disconnected"] == true)
}

pub(super) fn redirect_uri(home: &FsPath, id: &str) -> Result<String, String> {
    let env = parse_env_file(&home.join("server.env"));
    let base = env_val(&env, "CONNECTORS_REDIRECT_ORIGIN").unwrap_or_else(origin);
    let url = reqwest::Url::parse(&base).map_err(|_| "invalid CONNECTORS_REDIRECT_ORIGIN")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("CONNECTORS_REDIRECT_ORIGIN must be an HTTPS origin".into());
    }
    Ok(format!(
        "{}/api/connectors/{id}/callback",
        base.trim_end_matches('/')
    ))
}

pub(super) async fn begin(ctx: &ConnectorsCtx, d: &Def, account: Option<&str>) -> Response {
    if d.kind != "oauth2" {
        return Json(
            json!({"ok":true,"auth":"apikey","note":"paste the key; no browser grant needed"}),
        )
        .into_response();
    }
    let account = account.unwrap_or("").trim();
    if !oauth_store::valid_account(account) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok":false,"error":"name a valid account for this grant"})),
        )
            .into_response();
    }
    let env = parse_env_file(&ctx.home.join("server.env"));
    let Some(client_id) = env_val(&env, &d.env_keys[0]) else {
        return (
            StatusCode::CONFLICT,
            Json(json!({"ok":false,"error":"client credentials not set"})),
        )
            .into_response();
    };
    if env_val(&env, &d.env_keys[1]).is_none() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"ok":false,"error":"client credentials not set"})),
        )
            .into_response();
    }
    let redirect = match redirect_uri(&ctx.home, &d.id) {
        Ok(uri) => uri,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response();
        }
    };
    let mut url = match reqwest::Url::parse(&d.authorize_url) {
        Ok(url) => url,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"invalid authorize URL"})),
            )
                .into_response();
        }
    };
    let state = token_urlsafe(24);
    let verifier = token_urlsafe(64);
    let challenge = base64url_nopad(&Sha256::digest(verifier.as_bytes()));
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", &client_id),
        ("redirect_uri", &redirect),
        ("scope", &d.scopes),
        ("state", &state),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
    ]);
    // Bind the exact redirect and endpoint to this pending request. A config
    // change during consent must fail, never exchange a code with new settings.
    let bound = json!({"verifier":verifier,"redirect_uri":redirect,"token_url":d.token_url,"client_id":client_id}).to_string();
    if let Err(error) = pending_save(&ctx.home, &state, &d.id, account, Some(&bound)) {
        return storage_error(error);
    }
    tracing::info!(connector = %d.id, account, verdict="declared_oauth_started", "declared connector grant started with PKCE");
    Json(json!({"ok":true,"auth":"oauth2","authorize_url":url.as_str(),"redirect_uri":redirect,"account":account,"family":d.id,"scopes":d.scopes,"grant":describe_grant(&d.scopes)})).into_response()
}

pub(super) async fn complete(
    ctx: &ConnectorsCtx,
    d: &Def,
    code: String,
    account: String,
    bound: Option<String>,
) -> Response {
    if d.kind != "oauth2" {
        return cb_page(
            StatusCode::BAD_REQUEST,
            "<h2>This connector no longer supports OAuth.</h2>".into(),
        );
    }
    let binding: Value = bound
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or(Value::Null);
    let env = parse_env_file(&ctx.home.join("server.env"));
    let client_id = env_val(&env, &d.env_keys[0]).unwrap_or_default();
    let secret = env_val(&env, &d.env_keys[1]).unwrap_or_default();
    let redirect = redirect_uri(&ctx.home, &d.id).unwrap_or_default();
    if d.kind != "oauth2"
        || secret.is_empty()
        || binding["client_id"] != client_id
        || binding["redirect_uri"] != redirect
        || binding["token_url"] != d.token_url
    {
        tracing::warn!(connector=%d.id, verdict="declared_oauth_binding_changed", "OAuth settings changed during consent; start a new grant");
        return cb_page(
            StatusCode::BAD_REQUEST,
            "<h2>Connector settings changed. Start a new connection.</h2>".into(),
        );
    }
    let path = match oauth_store::family_path(&ctx.home, &d.id, &account) {
        Ok(path) => path,
        Err(error) => return storage_error(error),
    };
    let _lease = match oauth_store::lock(&ctx.home, &d.id, &account).await {
        Ok(lease) => lease,
        Err(error) => return storage_error(error),
    };
    let form = vec![
        ("grant_type".into(), "authorization_code".into()),
        ("code".into(), code),
        ("client_id".into(), client_id.clone()),
        ("client_secret".into(), secret.clone()),
        ("redirect_uri".into(), redirect),
        (
            "code_verifier".into(),
            binding["verifier"].as_str().unwrap_or_default().into(),
        ),
    ];
    let reply = ctx.http.post_form(&d.token_url, &form).await;
    let (status, body) = match reply {
        Ok(reply) => reply,
        Err(error) => {
            tracing::warn!(connector=%d.id,%error,verdict="declared_oauth_exchange_failed", "declared connector exchange transport failed");
            return cb_page(
                StatusCode::BAD_GATEWAY,
                "<h2>Token exchange failed. Start a new connection.</h2>".into(),
            );
        }
    };
    let access = body["access_token"].as_str().unwrap_or_default();
    if !(200..300).contains(&status) || access.trim().is_empty() || body["ok"] == false {
        // The response can contain secrets; never render/log the provider body.
        tracing::warn!(connector=%d.id,status,verdict="declared_oauth_exchange_refused", "declared connector exchange refused or malformed");
        return cb_page(
            StatusCode::BAD_GATEWAY,
            "<h2>Token exchange refused or malformed. Start a new connection.</h2>".into(),
        );
    }
    let lifetime = body["expires_in"].as_f64();
    if lifetime.is_some_and(|s| !s.is_finite() || s <= 0.0) {
        return cb_page(
            StatusCode::BAD_GATEWAY,
            "<h2>Invalid token lifetime.</h2>".into(),
        );
    }
    let store = json!({"token":access,"refresh_token":body["refresh_token"],"expires_at":lifetime.map(|s|now_ts()+s),"token_uri":d.token_url,"client_id":client_id,"client_secret":secret,"scopes":body["scope"].as_str().unwrap_or(&d.scopes),"identity_unverified":true});
    if let Err(error) = write_store_file(&path, &store) {
        return storage_error(error);
    }
    ROLLUP_CACHE.lock().expect("rollup cache").remove(&ctx.home);
    tracing::info!(connector=%d.id,account,verdict="declared_oauth_committed", "declared connector grant durably saved; account label is owner supplied");
    cb_page(
        StatusCode::OK,
        format!(
            "<h2>Connected {}</h2><p>Saved for account {}. Close this tab and refresh Connectors.</p>",
            html_escape(&d.label),
            html_escape(&account)
        ),
    )
}

pub(super) async fn mint(ctx: &ConnectorsCtx, d: &Def, account: Option<&str>) -> Response {
    if d.kind != "oauth2" {
        return (StatusCode::BAD_REQUEST,Json(json!({"ok":false,"status":"unsupported","detail":"API-key connectors use their key directly; no OAuth bearer to mint"}))).into_response();
    }
    let accounts = store_accounts(&ctx.home, &d.id);
    let account = match account.map(str::trim).filter(|a| !a.is_empty()) {
        Some(account) if accounts.iter().any(|a| a.eq_ignore_ascii_case(account)) => accounts.iter().find(|a| a.eq_ignore_ascii_case(account)).unwrap().clone(),
        Some(_) => return (StatusCode::NOT_FOUND,Json(json!({"ok":false,"status":"needs_auth","detail":"no stored grant for this account"}))).into_response(),
        None if accounts.len() == 1 => accounts[0].clone(),
        None => return (StatusCode::BAD_REQUEST,Json(json!({"ok":false,"status":"needs_auth","stored_accounts":accounts,"detail":"connect an account, or name one with ?account= when several are saved"}))).into_response(),
    };
    mint_from_user_grant(ctx, &d.id, &d.id, &account, &d.scopes).await
}

async fn minted_json(ctx: &ConnectorsCtx, d: &Def, account: Option<&str>) -> Value {
    let response = mint(ctx, d, account).await;
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let mut value: Value =
        serde_json::from_slice(&bytes).unwrap_or(json!({"ok":false,"status":"error"}));
    value["http"] = json!(status);
    value
}

pub(super) async fn test(ctx: &ConnectorsCtx, d: &Def) -> Response {
    // Testing an OAuth client ID as a bearer used to fail even after consent.
    let accounts = store_accounts(&ctx.home, &d.id);
    if accounts.is_empty() {
        return Json(json!({"ok":false,"status":"needs_auth","measured":false})).into_response();
    }
    let mut results = Vec::new();
    for account in accounts {
        results.push(probe(ctx, d, &account).await);
    }
    let ok = results.iter().all(|r| r["status"] == "ok");
    Json(json!({"ok":ok,"status":if ok {"connected"} else {"error"},"measured":results.iter().all(|r|r["measured"]==true),"accounts":results,"detail":if ok {"all saved accounts passed"} else {"one or more saved accounts failed"}})).into_response()
}

async fn probe(ctx: &ConnectorsCtx, d: &Def, account: &str) -> Value {
    let minted = minted_json(ctx, d, Some(account)).await;
    if minted["ok"] != true {
        return json!({"account":account,"status":minted["status"].as_str().unwrap_or("error"),"http":minted["http"],"measured":false,"checked_at":now_ts()});
    }
    if d.test_url.is_empty() {
        return json!({"account":account,"status":"unverified","measured":false,"why_unmeasured":"no test_url declared","checked_at":now_ts()});
    }
    match ctx
        .http
        .get(&d.test_url, minted["access_token"].as_str())
        .await
    {
        Ok((status, _)) => {
            json!({"account":account,"status":if (200..300).contains(&status){"ok"}else if status==401{"needs_reauth"}else{"api_error"},"http":status,"measured":true,"checked_at":now_ts()})
        }
        Err(_) => {
            json!({"account":account,"status":"unreachable","measured":false,"checked_at":now_ts()})
        }
    }
}

pub(super) async fn rollup(
    http: &Arc<dyn HttpTransport>,
    home: &FsPath,
    rows: &mut BTreeMap<String, Map<String, Value>>,
    canaries: &mut BTreeMap<String, Map<String, Value>>,
) {
    let ctx = ConnectorsCtx {
        http: http.clone(),
        home: home.into(),
    };
    for c in load_custom(home).iter().filter(|c| c.kind == "oauth2") {
        let d = Def::from(c);
        for account in store_accounts(home, &d.id) {
            let health = probe(&ctx, &d, &account).await;
            if health["status"] == "needs_reauth" {
                tracing::warn!(connector=%d.id, account, verdict="declared_oauth_needs_reauth", "declared account failed its provider canary; reconnect required");
            }
            rows.entry(account.clone())
                .or_default()
                .insert(d.id.clone(), health["status"].clone());
            canaries
                .entry(account)
                .or_default()
                .insert(d.id.clone(), health);
        }
    }
}

fn cancel_pending(home: &FsPath, id: &str, account: Option<&str>) -> std::io::Result<()> {
    let path = pending_path(home);
    let _lease = crate::integrations::secure_store::lock(&path)?;
    let mut pending: Map<String, Value> = crate::integrations::secure_store::read_json(&path)?;
    pending.retain(|_, v| !(v["family"] == id && account.is_none_or(|a| v["account"] == a)));
    crate::integrations::secure_store::write(&path, Value::Object(pending).to_string().as_bytes())
}

pub(super) async fn forget_grants(home: &FsPath, id: &str) -> std::io::Result<()> {
    cancel_pending(home, id, None)?;
    for account in store_accounts(home, id) {
        let _lease = oauth_store::lock(home, id, &account).await?;
        let path = oauth_store::family_path(home, id, &account)?;
        write_store_file(&path, &json!({"disconnected":true}))?;
    }
    ROLLUP_CACHE.lock().expect("rollup cache").remove(home);
    Ok(())
}

pub(super) async fn disconnect(
    Extension(ctx): Extension<Arc<ConnectorsCtx>>,
    Path((id, account)): Path<(String, String)>,
) -> Response {
    if !oauth_store::valid_account(&account) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid account"})),
        )
            .into_response();
    }
    let _registry = match registry_lease(&ctx.home).await {
        Ok(lease) => lease,
        Err(error) => return storage_error(error),
    };
    if !def_of(&ctx.home, &id).is_some_and(|d| !d.builtin && d.kind == "oauth2") {
        return (StatusCode::BAD_REQUEST,Json(json!({"error":"this endpoint disconnects declared OAuth connectors; use the provider's existing disconnect flow for builtins"}))).into_response();
    }
    let _lease = match oauth_store::lock(&ctx.home, &id, &account).await {
        Ok(lease) => lease,
        Err(error) => return storage_error(error),
    };
    if let Err(error) = cancel_pending(&ctx.home, &id, Some(&account)) {
        return storage_error(error);
    }
    let path = oauth_store::family_path(&ctx.home, &id, &account)
        .expect("validated account and connector");
    if let Err(error) = write_store_file(&path, &json!({"disconnected":true})) {
        return storage_error(error);
    }
    ROLLUP_CACHE.lock().expect("rollup cache").remove(&ctx.home);
    tracing::info!(
        connector = id,
        account,
        verdict = "declared_oauth_disconnected",
        "saved grant and outstanding consent forgotten; provider-side revocation remains separate"
    );
    Json(json!({"ok":true,"account":account,"disconnected":true,"provider_revoked":false}))
        .into_response()
}
