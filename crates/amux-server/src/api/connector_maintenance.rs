//! Credential upkeep is infrastructure, independent of board/autofix policy.
//! Never changes owner definitions, account pins, scopes, or disconnected grants.
use super::*;

const REFRESH_WINDOW: f64 = 600.0;
const UNKNOWN_EXPIRY_REFRESH_AGE: f64 = 86_400.0;

pub(super) async fn maintain(ctx: &ConnectorsCtx, family: &str, account: &str) -> Value {
    let path = if family == "google" && !store_path(&ctx.home, family, account).exists() {
        ctx.home
            .join("gmail-tokens")
            .join(format!("{account}.json"))
    } else {
        store_path(&ctx.home, family, account)
    };
    let tf: Value = match crate::integrations::secure_store::read_json::<Value>(&path) {
        Ok(v) if v.is_object() => v,
        _ => {
            return json!({"family":family,"account":account,"status":"storage_error","measured":false});
        }
    };
    if tf["disconnected"] == true {
        return json!({"family":family,"account":account,"status":"disconnected","measured":false});
    }
    let renewable = tf["refresh_token"]
        .as_str()
        .is_some_and(|r| !r.trim().is_empty());
    let due = match tf["expires_at"].as_f64() {
        Some(exp) => exp - now_ts() <= REFRESH_WINDOW,
        None => grant_last_ok_age_days(&path)
            .is_some_and(|age| age * 86_400.0 >= UNKNOWN_EXPIRY_REFRESH_AGE),
    };
    if !due || !renewable {
        return json!({"family":family,"account":account,"status":if due && tf["expires_at"].as_f64().is_some() {"needs_reauth"} else {"not_due"},"measured":false});
    }
    let response = mint_with_lifetime(ctx, family, family, account, "", REFRESH_WINDOW).await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .ok();
    let body: Value = body
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    // Only computed status is retained. Provider details/URLs can echo secrets.
    let verdict = if status.is_success() {
        "ok"
    } else if body["status"] == "needs_reauth" {
        "needs_reauth"
    } else {
        "error"
    };
    if !status.is_success() {
        tracing::warn!(
            family,
            account,
            http_status = status.as_u16(),
            verdict = "connector_maintenance_failed",
            "renewable grant upkeep failed; existing saved state retained"
        );
    }
    json!({"family":family,"account":account,"status":verdict,"http":status.as_u16(),"measured":true})
}

pub(super) async fn pass(ctx: &ConnectorsCtx) -> Value {
    let mut results = Vec::new();
    let mut google: std::collections::BTreeSet<String> =
        store_accounts(&ctx.home, "google").into_iter().collect();
    google.extend(connected_accounts_in(&ctx.home));
    for account in google {
        results.push(maintain(ctx, "google", &account).await);
    }
    for account in store_accounts(&ctx.home, "slack") {
        results.push(maintain(ctx, "slack", &account).await);
    }
    // Follow the same registry → account lock order as explicit custom mint and
    // disconnect. A deleted/redeclared connector cannot be revived by this pass.
    let registry = declared_oauth::registry_lease(&ctx.home).await;
    match registry {
        Ok(_lease) => match load_custom_checked(&ctx.home) {
            Ok(definitions) => {
                for definition in definitions.iter().filter(|d| d.kind == "oauth2") {
                    for account in store_accounts(&ctx.home, &definition.id) {
                        results.push(maintain(ctx, &definition.id, &account).await);
                    }
                }
            }
            Err(_) => results.push(json!({"status":"registry_unreadable","measured":false})),
        },
        Err(_) => results.push(json!({"status":"registry_busy","measured":false})),
    }
    if results.iter().any(|r| r["status"] == "ok") {
        ROLLUP_CACHE.lock().expect("rollup cache").remove(&ctx.home);
    }
    let rollup = accounts_rollup(&ctx.http, &ctx.home, false).await;
    let file_env = parse_env_file(&ctx.home.join("server.env"));
    let mut keys = Vec::new();
    for provider in REGISTRY {
        if let Auth::ApiKey { key_env } = provider.auth {
            if let Some(key) = env_val(&file_env, key_env) {
                let url = if provider.id == "telegram" {
                    format!("https://api.telegram.org/bot{key}/getMe")
                } else {
                    provider.test_url.into()
                };
                let mut check = provider_canary(ctx, provider.id, &url, &key).await;
                check["connector"] = json!(provider.id);
                record_test(&ctx.home, provider.id, &check);
                keys.push(check);
            }
        }
    }
    // Declared API keys are tested by the same redacted canary as the Test button.
    for definition in load_custom(&ctx.home)
        .iter()
        .filter(|d| d.kind == "api_key")
    {
        if let Some(key) = env_val(&file_env, &definition.key_env) {
            let mut check = if definition.test_url.is_empty() {
                json!({"status":"unverified","measured":false})
            } else {
                provider_canary(ctx, &definition.id, &definition.test_url, &key).await
            };
            check["connector"] = json!(definition.id);
            if check.get("ok").is_some() {
                record_test(&ctx.home, &definition.id, &check);
            }
            keys.push(check);
        }
    }
    let accounts: Vec<Value> = rollup["accounts"].as_array().into_iter().flatten().map(|account| {
        let canary: Map<String, Value> = account["canary"].as_object().into_iter().flatten().map(|(id, leg)| {
            let status = json!({"status":leg["status"],"measured":leg["measured"],"http":leg["http"],"checked_at":leg["checked_at"]});
            (id.clone(), status)
        }).collect();
        json!({"account":account["account"],"families":account["families"],"needs_reauth":account["needs_reauth"],"canary":canary})
    }).collect();
    let failures = results
        .iter()
        .filter(|r| {
            matches!(
                r["status"].as_str(),
                Some(
                    "error"
                        | "needs_reauth"
                        | "storage_error"
                        | "registry_unreadable"
                        | "registry_busy"
                )
            )
        })
        .count()
        + keys.iter().filter(|k| k["ok"] == false).count()
        + accounts
            .iter()
            .flat_map(|a| a["canary"].as_object().into_iter().flat_map(|c| c.values()))
            .filter(|leg| {
                matches!(
                    leg["status"].as_str(),
                    Some(
                        "api_error"
                            | "unreachable"
                            | "needs_reauth"
                            | "not_connected"
                            | "storage_error"
                            | "error"
                    )
                )
            })
            .count();
    let mut report = json!({"failures":failures,"measured":true,"n_considered":results.len()+keys.len(),"checked_at":now_ts(),"refresh_window_s":REFRESH_WINDOW,"grants":results,"api_keys":keys,"accounts":accounts,"needs_reauth":rollup["needs_reauth"],"durable":false});
    let path = ctx.home.join("connectors/maintenance.json");
    report["durable"] = json!(true);
    if crate::integrations::secure_store::write(&path, report.to_string().as_bytes()).is_err() {
        report["durable"] = json!(false);
        tracing::warn!(
            verdict = "connector_maintenance_report_not_durable",
            "connector maintenance receipt was not committed"
        );
    }
    tracing::info!(
        measured = true,
        n_considered = report["n_considered"].as_u64().unwrap_or(0),
        durable = report["durable"].as_bool().unwrap_or(false),
        failures = failures,
        verdict = "connector_maintenance_pass",
        "connector credential upkeep completed"
    );
    report
}

pub(super) async fn view(Extension(ctx): Extension<Arc<ConnectorsCtx>>) -> Response {
    match crate::integrations::secure_store::read_json::<Value>(&ctx.home.join("connectors/maintenance.json")) {
        Ok(mut report) if report.is_object() => {
            let age = report["checked_at"].as_f64().map(|at|(now_ts()-at).max(0.0));
            report["age_s"] = json!(age);
            report["stale"] = json!(age.is_none_or(|age|age>600.0));
            Json(report).into_response()
        },
        _ => Json(json!({"measured":false,"n_considered":0,"why_unmeasured":"no readable durable maintenance receipt"})).into_response(),
    }
}
