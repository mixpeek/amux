//! Brex tokenized-card scaffolding (Ethan, 2026-09-12: "can we use brex api to
//! create a tokenized cc for amux with a clear budget daily weekly monthly and
//! per trx"). DISABLED by default and safe to ship dark.
//!
//! WHY THIS SHAPE. A Brex spend limit is ONE recurring period per limit
//! (Brex exposes weekly/monthly/quarterly/yearly/one-time via its Budgets &
//! Limits API; daily is not confirmed). The four budgets Ethan asked for
//! (per-transaction, daily, weekly, monthly) do NOT all fit one native Brex
//! control, so this module owns the fine-grained enforcement: Brex holds the
//! coarsest native cap, and amux tallies each charge from the transaction
//! WEBHOOK and FREEZES the card the moment any of the four thresholds would be
//! crossed. [`BudgetGuard`] is that decision, kept pure so it is fully tested
//! offline; the network client is a thin, HARD-GATED wrapper around it.
//!
//! NOTHING HERE SPENDS OR PROVISIONS until the owner sets AMUX_BREX_ENABLED=1
//! and AMUX_BREX_TOKEN. Every client method refuses without both, and the
//! request bodies against Brex's own API are marked VERIFY because Brex's
//! reference is a client-rendered SPA that could not be read at build time;
//! confirm them against a live sandbox account before flipping the flag.

use serde::Serialize;

/// Cents; `None` means "no limit on this dimension".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    pub per_txn: Option<i64>,
    pub daily: Option<i64>,
    pub weekly: Option<i64>,
    pub monthly: Option<i64>,
}

/// Spend already posted to the card in each rolling window, in cents. These are
/// the sums BEFORE the charge under evaluation is added.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tallies {
    pub today: i64,
    pub week: i64,
    pub month: i64,
}

/// The guard's answer for one incoming charge. `Freeze` names WHICH dimension
/// tripped and by how much, so the log and the human both see a computed reason
/// rather than a bare "blocked" (ethos rule 4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    Allow,
    Freeze { dimension: &'static str, limit_cents: i64, would_be_cents: i64 },
}

impl Verdict {
    pub fn is_freeze(&self) -> bool {
        matches!(self, Verdict::Freeze { .. })
    }
}

/// Pure four-dimension budget decision. A refund (negative `amount_cents`) can
/// never trip a cap, and a dimension with no limit is never evaluated. The
/// per-transaction check uses the charge alone; the period checks use the
/// window total INCLUDING this charge, so a single big charge and a slow drip
/// are both caught at the same boundary.
#[derive(Clone, Copy, Debug)]
pub struct BudgetGuard {
    pub limits: Limits,
}

impl BudgetGuard {
    pub fn new(limits: Limits) -> Self {
        Self { limits }
    }

    pub fn evaluate(&self, tallies: Tallies, amount_cents: i64) -> Verdict {
        // A credit/refund only lowers exposure; it is always allowed and never
        // freezes the card.
        if amount_cents <= 0 {
            return Verdict::Allow;
        }
        if let Some(limit) = self.limits.per_txn {
            if amount_cents > limit {
                return Verdict::Freeze { dimension: "per_transaction", limit_cents: limit, would_be_cents: amount_cents };
            }
        }
        for (limit, spent, dim) in [
            (self.limits.daily, tallies.today, "daily"),
            (self.limits.weekly, tallies.week, "weekly"),
            (self.limits.monthly, tallies.month, "monthly"),
        ] {
            if let Some(limit) = limit {
                let would_be = spent.saturating_add(amount_cents);
                if would_be > limit {
                    return Verdict::Freeze { dimension: dim, limit_cents: limit, would_be_cents: would_be };
                }
            }
        }
        Verdict::Allow
    }
}

/// Runtime configuration, read from the environment (server.env). Absent or
/// `AMUX_BREX_ENABLED` not truthy means the whole feature is off.
#[derive(Clone, Debug)]
pub struct BrexConfig {
    pub enabled: bool,
    pub sandbox: bool,
    pub token: Option<String>,
    pub card_id: Option<String>,
    pub webhook_secret: Option<String>,
    pub limits: Limits,
}

fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => {
            let v = v.trim();
            !(v == "0" || v.eq_ignore_ascii_case("false") || v.is_empty())
        }
        Err(_) => default,
    }
}

fn env_cents(key: &str) -> Option<i64> {
    std::env::var(key).ok().and_then(|v| v.trim().parse::<i64>().ok()).filter(|n| *n > 0)
}

fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|s| !s.is_empty())
}

impl BrexConfig {
    pub fn from_env() -> Self {
        Self {
            enabled: env_bool("AMUX_BREX_ENABLED", false),
            // Default to SANDBOX so a misconfiguration cannot hit production Brex.
            sandbox: env_bool("AMUX_BREX_SANDBOX", true),
            token: env_str("AMUX_BREX_TOKEN"),
            card_id: env_str("AMUX_BREX_CARD_ID"),
            webhook_secret: env_str("AMUX_BREX_WEBHOOK_SECRET"),
            limits: Limits {
                per_txn: env_cents("AMUX_BREX_LIMIT_PER_TXN_CENTS"),
                daily: env_cents("AMUX_BREX_LIMIT_DAILY_CENTS"),
                weekly: env_cents("AMUX_BREX_LIMIT_WEEKLY_CENTS"),
                monthly: env_cents("AMUX_BREX_LIMIT_MONTHLY_CENTS"),
            },
        }
    }

    /// Ready to make an authenticated call: switched on AND holding a token.
    pub fn live(&self) -> bool {
        self.enabled && self.token.is_some()
    }

    pub fn base_url(&self) -> &'static str {
        // VERIFY against Brex before enabling. Production is platform.brexapis.com;
        // the staging host is the documented sandbox. Both are behind the flag.
        if self.sandbox {
            "https://platform.staging.brexapis.com"
        } else {
            "https://platform.brexapis.com"
        }
    }

    pub fn guard(&self) -> BudgetGuard {
        BudgetGuard::new(self.limits)
    }
}

/// Hard-gated client. Every method returns Err before touching the network
/// unless the feature is live (enabled + token). The exact request bodies are
/// marked VERIFY: they follow Brex's documented shape as closely as could be
/// confirmed, but must be checked against a live sandbox account before use.
pub struct BrexClient {
    cfg: BrexConfig,
    http: reqwest::Client,
}

impl BrexClient {
    pub fn new(cfg: BrexConfig) -> Self {
        Self {
            cfg,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
        }
    }

    fn preflight(&self) -> anyhow::Result<&str> {
        if !self.cfg.enabled {
            anyhow::bail!("brex integration disabled (set AMUX_BREX_ENABLED=1)");
        }
        self.cfg.token.as_deref().ok_or_else(|| anyhow::anyhow!("brex token missing (set AMUX_BREX_TOKEN)"))
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let token = self.preflight()?;
        let url = format!("{}{}", self.cfg.base_url(), path);
        let resp = self.http.post(&url).bearer_auth(token).json(&body).send().await?;
        let status = resp.status();
        let value: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            anyhow::bail!("brex POST {path} -> {status}: {value}");
        }
        Ok(value)
    }

    /// Create a virtual (tokenized) card. VERIFY the path and body against Brex's
    /// Team/Cards API. Kept minimal on purpose: this scaffolding never issues a
    /// live card in tests, and the owner reviews the body before enabling.
    pub async fn create_virtual_card(&self, holder_name: &str, monthly_cap_cents: Option<i64>) -> anyhow::Result<serde_json::Value> {
        let mut body = serde_json::json!({
            "card_type": "VIRTUAL",
            "card_name": format!("amux-{holder_name}"),
        });
        // Brex holds the coarsest native cap; amux enforces the finer windows.
        if let Some(cents) = monthly_cap_cents {
            body["spend_controls"] = serde_json::json!({
                "spend_limit": {"amount": cents, "currency": "USD"},
                "spend_limit_interval": "MONTHLY",
            });
        }
        self.post("/v2/cards", body).await
    }

    /// Freeze (lock) the managed card. This is the enforcement action the budget
    /// guard triggers. VERIFY the path: Brex uses a lock/terminate action on the
    /// card resource.
    pub async fn freeze_card(&self, card_id: &str, reason: &str) -> anyhow::Result<serde_json::Value> {
        self.post(&format!("/v2/cards/{card_id}/lock"), serde_json::json!({"reason": reason})).await
    }

    pub async fn unfreeze_card(&self, card_id: &str) -> anyhow::Result<serde_json::Value> {
        self.post(&format!("/v2/cards/{card_id}/unlock"), serde_json::json!({})).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cents(dollars: i64) -> i64 { dollars * 100 }

    #[test]
    fn no_limits_always_allows() {
        let g = BudgetGuard::new(Limits::default());
        assert_eq!(g.evaluate(Tallies { today: cents(9999), week: cents(9999), month: cents(9999) }, cents(5000)), Verdict::Allow);
    }

    #[test]
    fn a_refund_never_freezes_even_over_a_limit() {
        let g = BudgetGuard::new(Limits { per_txn: Some(cents(50)), daily: Some(cents(100)), ..Default::default() });
        assert_eq!(g.evaluate(Tallies { today: cents(90), ..Default::default() }, -cents(500)), Verdict::Allow);
    }

    #[test]
    fn per_transaction_cap_is_the_charge_alone() {
        let g = BudgetGuard::new(Limits { per_txn: Some(cents(100)), ..Default::default() });
        assert_eq!(g.evaluate(Tallies::default(), cents(100)), Verdict::Allow); // exactly at the cap is fine
        assert_eq!(
            g.evaluate(Tallies::default(), cents(101)),
            Verdict::Freeze { dimension: "per_transaction", limit_cents: cents(100), would_be_cents: cents(101) }
        );
    }

    #[test]
    fn a_slow_drip_trips_the_daily_window_not_the_per_txn() {
        // Each charge is under the per-txn cap, but their sum crosses daily.
        let g = BudgetGuard::new(Limits { per_txn: Some(cents(100)), daily: Some(cents(250)), ..Default::default() });
        assert_eq!(g.evaluate(Tallies { today: cents(200), ..Default::default() }, cents(50)), Verdict::Allow); // 250 == cap
        assert_eq!(
            g.evaluate(Tallies { today: cents(200), ..Default::default() }, cents(51)),
            Verdict::Freeze { dimension: "daily", limit_cents: cents(250), would_be_cents: cents(251) }
        );
    }

    #[test]
    fn the_tightest_window_wins_and_is_named() {
        // Under daily and weekly, but the monthly window is what this charge crosses.
        let g = BudgetGuard::new(Limits {
            per_txn: Some(cents(1000)), daily: Some(cents(1000)), weekly: Some(cents(5000)), monthly: Some(cents(10000)),
        });
        let v = g.evaluate(Tallies { today: cents(100), week: cents(1000), month: cents(9950) }, cents(60));
        assert_eq!(v, Verdict::Freeze { dimension: "monthly", limit_cents: cents(10000), would_be_cents: cents(10010) });
    }

    #[test]
    fn per_transaction_is_checked_before_the_windows() {
        // A charge over BOTH per-txn and daily is reported as per-transaction,
        // the most specific and actionable dimension.
        let g = BudgetGuard::new(Limits { per_txn: Some(cents(100)), daily: Some(cents(100)), ..Default::default() });
        assert_eq!(
            g.evaluate(Tallies { today: cents(90), ..Default::default() }, cents(500)),
            Verdict::Freeze { dimension: "per_transaction", limit_cents: cents(100), would_be_cents: cents(500) }
        );
    }

    #[test]
    fn disabled_config_is_not_live_and_client_refuses() {
        let cfg = BrexConfig {
            enabled: false, sandbox: true, token: Some("t".into()), card_id: None, webhook_secret: None, limits: Limits::default(),
        };
        assert!(!cfg.live());
        assert!(BrexClient::new(cfg).preflight().is_err());
    }

    #[test]
    fn live_requires_both_flag_and_token() {
        let base = BrexConfig { enabled: true, sandbox: true, token: None, card_id: None, webhook_secret: None, limits: Limits::default() };
        assert!(!base.live(), "enabled without a token is not live");
        let with_token = BrexConfig { token: Some("t".into()), ..base };
        assert!(with_token.live());
    }

    #[test]
    fn sandbox_is_the_default_host() {
        let cfg = BrexConfig { enabled: true, sandbox: true, token: Some("t".into()), card_id: None, webhook_secret: None, limits: Limits::default() };
        assert!(cfg.base_url().contains("staging"), "a misconfiguration must not reach production Brex");
    }
}
