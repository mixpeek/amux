//! Keep renewable credentials usable even when no worker or board job runs.
//!
//! Every pass refreshes each renewable grant that is about to expire
//! (`api::connectors::maintenance_pass`). A grant that can no longer be
//! refreshed needs the owner, so the pass then TELLS the owner
//! ([`reauth_alerts`]): two Gmail grants (esteininger21, beefinethan) died with
//! invalid_grant around 2026-10-17 and stayed dead until the owner re-consented
//! by hand on 2026-11-08, because nothing reached him. The one signal was an
//! autofix board card.
use crate::api::alerts::AlertChannels;
use crate::api::connectors::ConnectorsCtx;
use crate::api::AppState;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
const JOB: &str = super::registry::ids::CONNECTOR_MAINTENANCE;
const TICK_SECS: u64 = 300;
/// While a grant stays broken, the owner is reminded this often.
const REMIND_S: f64 = 86_400.0;
const ALERTS_FILE: &str = "connectors/reauth-alerts.json";

pub fn spawn(state: AppState) -> super::PeriodicTask {
    let secs = std::env::var(super::per_job_disable_var(JOB))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(TICK_SECS);
    let ctx = Arc::new(ConnectorsCtx {
        home: crate::api::session_verbs::home(),
        http: Arc::new(crate::integrations::email::ReqwestTransport::new()),
    });
    spawn_with(ctx, secs, Some(state))
}

/// Private process tests supply their own home and actual HTTP provider.
pub fn spawn_with(ctx: Arc<ConnectorsCtx>, secs: u64, state: Option<AppState>) -> super::PeriodicTask {
    super::spawn_periodic(JOB, secs.max(1), move || {
        let ctx = ctx.clone();
        let state = state.clone();
        async move {
            let report = crate::api::connectors::maintenance_pass(&ctx).await;
            if let Some(state) = state {
                reauth_alerts(&crate::api::alerts::RealChannels, &state, &ctx, &report, crate::config::now_f64()).await;
            }
        }
    })
}

fn enabled(home: &Path) -> bool {
    !crate::api::settings::effective_env(home, "AMUX_CONNECTOR_REAUTH_ALERT").is_some_and(|v| {
        matches!(v.trim().trim_matches('"').to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off")
    })
}

/// The family a reconnect goes through, from the rollup's `reconnect` hint
/// (`POST /api/connectors/<family>/auth?account=...`).
fn reconnect_family(entry: &Value) -> String {
    entry["reconnect"]
        .as_str()
        .and_then(|r| r.strip_prefix("POST /api/connectors/"))
        .and_then(|r| r.split('/').next())
        .filter(|f| !f.is_empty())
        .unwrap_or("google")
        .to_string()
}

/// The owner's one-tap link: the dashboard opens Connectors and offers the
/// reconnect for that account (`#reconnect=<family>:<account>` in app.js).
fn reconnect_hash(family: &str, account: &str) -> String {
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| if b.is_ascii_alphanumeric() || b"-_.@".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
            .collect()
    };
    format!("/#reconnect={}:{}", enc(family), enc(account))
}

fn load(home: &Path) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(home.join(ALERTS_FILE))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn save(home: &Path, map: &serde_json::Map<String, Value>) {
    let path = home.join(ALERTS_FILE);
    if let Err(e) = crate::integrations::secure_store::write(&path, Value::Object(map.clone()).to_string().as_bytes()) {
        tracing::warn!(error = %e, verdict = "connector_reauth_alert_state_not_saved",
            "the reauth alert record was not written; the next pass may alert again");
    }
}

/// Tell the owner about every grant that can no longer be refreshed: once
/// when it breaks, then once a day while it stays broken, and nothing once it
/// works again.
///
/// A candidate from the pass's report is CONFIRMED with an uncached rollup
/// before anyone is told. The report can read a cached verdict from before a
/// re-consent (2026-11-08: both accounts still read needs_reauth minutes after
/// the owner reconnected them), and an alert on that would be false.
pub(crate) async fn reauth_alerts(
    channels: &dyn AlertChannels,
    state: &AppState,
    ctx: &ConnectorsCtx,
    report: &Value,
    now: f64,
) -> Value {
    let home = ctx.home.as_path();
    if !enabled(home) {
        return json!({"measured": false, "why_unmeasured": "AMUX_CONNECTOR_REAUTH_ALERT is off"});
    }
    let mut record = load(home);
    let candidates = report["needs_reauth"].as_array().cloned().unwrap_or_default();
    let confirmed: Vec<Value> = if candidates.is_empty() {
        Vec::new()
    } else {
        let fresh = crate::api::connectors::accounts_rollup(&ctx.http, home, true).await;
        fresh["needs_reauth"].as_array().cloned().unwrap_or_default()
    };
    let broken: std::collections::BTreeSet<String> = confirmed
        .iter()
        .filter_map(|e| e["account"].as_str().map(str::to_string))
        .collect();
    // Healed: say so in the log and forget it, so a later break alerts again.
    let healed: Vec<String> = record.keys().filter(|a| !broken.contains(*a)).cloned().collect();
    for account in &healed {
        let since = record[account]["since"].as_f64().unwrap_or(now);
        tracing::info!(account = %account, broken_for_h = ((now - since) / 3600.0).round(), measured = true,
            n_considered = 1, verdict = "connector_reauth_healed", "a grant that needed the owner works again");
        record.remove(account);
    }
    let mut sent = Vec::new();
    for entry in &confirmed {
        let Some(account) = entry["account"].as_str() else { continue };
        let prev = record.get(account).cloned();
        let last = prev.as_ref().and_then(|p| p["last_alert"].as_f64());
        if last.is_some_and(|t| now - t < REMIND_S) {
            continue;
        }
        let family = reconnect_family(entry);
        let hash = reconnect_hash(&family, account);
        let services = entry["broken"].as_array().map(|b| {
            b.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")
        }).unwrap_or_default();
        let lasted = entry["last_ok_age_days"].as_f64().map(|d| format!(" It last refreshed {:.1} days ago.", d)).unwrap_or_default();
        let since = prev.as_ref().and_then(|p| p["since"].as_f64()).unwrap_or(now);
        let reminder = prev.is_some();
        let subject = format!("amux: reconnect {account}");
        let push_text = format!(
            "{account} can no longer be refreshed ({services}). Workers using it fail until you approve once. Tap to reconnect."
        );
        let link = crate::api::static_files::public_links::public_url(home)
            .map(|b| format!("{}{hash}", b.trim_end_matches('/')));
        let body = format!(
            "{account} can no longer be refreshed by amux ({services}), so every worker that uses it fails until you approve it once.{lasted}{}\n\nReconnect:\n\n{}\n\nThat opens Connectors in amux and starts the reconnect; Google then asks you to approve.",
            if reminder { format!(" It has been broken for {:.0} hours.", (now - since) / 3600.0) } else { String::new() },
            link.clone().unwrap_or_else(|| "open amux, go to Connectors, and press Reconnect beside this account (set AMUX_PUBLIC_URL in server.env to get a direct link here)".into()),
        );
        let push = crate::push::send_all(state, "amux", &push_text, "", "connector-reauth", &hash).await;
        let push = crate::api::alerts::push_delivery_verdict(&push);
        let (email_ok, email_detail) = match crate::api::alerts::owner_email_destination(home) {
            // The owner's inbox can be the broken account itself; it then cannot
            // carry this, and push is the channel that does.
            Some((to, _)) if to.eq_ignore_ascii_case(account) => (false, format!("not sent: {to} is the account that is broken")),
            Some((to, _)) => channels.email(&to, &subject, &body).await,
            None => (false, "no AMUX_OWNER_EMAIL and no connected Gmail account".into()),
        };
        let delivered = push.is_ok() || email_ok;
        let verdict = if delivered { "connector_reauth_alerted" } else { "connector_reauth_alert_undelivered" };
        tracing::warn!(account = %account, family = %family, reminder, delivered,
            push = %match &push { Ok(()) => "sent".to_string(), Err(e) => format!("failed: {e}") },
            email = %email_detail, measured = true, n_considered = 1, verdict,
            "a grant needs the owner's re-approval");
        if delivered {
            record.insert(account.to_string(), json!({"since": since, "last_alert": now, "family": family,
                "alerts": prev.as_ref().and_then(|p| p["alerts"].as_u64()).unwrap_or(0) + 1}));
            sent.push(json!({"account": account, "reminder": reminder}));
        }
    }
    if !healed.is_empty() || !sent.is_empty() {
        save(home, &record);
    }
    json!({"measured": true, "n_considered": confirmed.len(), "candidates": candidates.len(), "alerted": sent, "healed": healed})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reconnect_link_names_the_family_and_account() {
        let e = json!({"account": "a@gmail.com", "reconnect": "POST /api/connectors/google/auth?account=a@gmail.com → open authorize_url"});
        assert_eq!(reconnect_family(&e), "google");
        assert_eq!(reconnect_hash("google", "a+b@gmail.com"), "/#reconnect=google:a%2Bb@gmail.com");
        assert_eq!(reconnect_family(&json!({"account": "x"})), "google");
    }
}
