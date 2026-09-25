//! Vault: payment cards and other secrets that workers USE but never READ,
//! each with its own spending rules (Ethan 2026-09-24: "we should have a CC
//! capability ... have each vault item have rules. the rule for this is no
//! purchase over $100 without explicit approval").
//!
//! # Where it fits
//!
//! This is the credential broker's next step (APM-1/APM-2), built the way
//! APM-1 recommended: an extension of connectors, not a ninth primitive.
//! Connectors already give workers secret-blind access to OAuth accounts
//! (`connector_entitlement_check`, 6cb57b75). A card is the same idea for
//! checkout forms: the worker names the item, the amount and the merchant;
//! amux applies the item's rules and, if they pass, types the card into the
//! worker's amux browser page itself. The number never enters a prompt, a
//! transcript, a log or an API response.
//!
//! # The rules
//!
//! `max_usd_without_approval`: a charge above it mints a GRANT (the existing
//! owner-approval path, grants.rs). Only a request with no worker origin, which
//! is the dashboard, can approve it. Approval writes a single-use allowance for
//! exactly (worker, item, amount, merchant); the worker's retry consumes it.
//! A charge at or under the limit fills straight away. Every decision is
//! appended to `~/.amux/logs/vault-audit.jsonl` without secret values.
//!
//! # What this is, honestly
//!
//! Same caveat grants.rs states: an accident guardrail against a worker spending
//! more than it was allowed, not a boundary against a hostile process on this
//! machine. The store is a 0600 file in `~/.amux/vault/`, the same trust level
//! as `server.env`, where credential values already live. The amount is what the
//! worker DECLARES; the audit line records it so a mismatch with the actual
//! charge is findable. A card field inside a cross-origin payment iframe
//! (Stripe Elements and similar) cannot be reached from the page and is
//! reported back as `not_found`, never guessed at.

use crate::config::{amux_home, now_f64};
use axum::extract::Path as AxPath;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// Secret fields a card item may hold. Everything else on an item is metadata.
/// No PIN: an online checkout never asks for one, and a PIN only unlocks
/// in-person and ATM use, so holding it would widen the blast radius for no use.
const CARD_FIELDS: [&str; 11] = [
    "number", "exp_month", "exp_year", "cvc", "name", "address", "city", "state", "zip",
    "country", "email",
];

pub(crate) fn vault_dir(home: &Path) -> PathBuf {
    home.join("vault")
}

fn items_path(home: &Path) -> PathBuf {
    vault_dir(home).join("items.json")
}

fn audit_path(home: &Path) -> PathBuf {
    home.join("logs").join("vault-audit.jsonl")
}

pub(crate) fn load_items(home: &Path) -> Vec<Value> {
    std::fs::read_to_string(items_path(home))
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .unwrap_or_default()
}

/// Atomic, private write: temp file at 0600 in the same dir, then rename.
fn save_items(home: &Path, items: &[Value]) -> std::io::Result<()> {
    let dir = vault_dir(home);
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let tmp = dir.join(format!(".items.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(items).unwrap_or_default())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, items_path(home))
}

fn audit(home: &Path, entry: Value) {
    use std::io::Write;
    let path = audit_path(home);
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{}", entry);
    }
}

fn digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

pub(crate) fn card_brand(number: &str) -> &'static str {
    let d = digits(number);
    let p2: u32 = d.get(..2).and_then(|x| x.parse().ok()).unwrap_or(0);
    let p4: u32 = d.get(..4).and_then(|x| x.parse().ok()).unwrap_or(0);
    if d.starts_with('4') {
        "Visa"
    } else if (51..=55).contains(&p2) || (2221..=2720).contains(&p4) {
        "Mastercard"
    } else if p2 == 34 || p2 == 37 {
        "Amex"
    } else if d.starts_with("6011") || d.starts_with("65") {
        "Discover"
    } else {
        "Card"
    }
}

/// What anyone may see about an item: never a secret value.
pub(crate) fn public_view(item: &Value) -> Value {
    let f = &item["fields"];
    let number = f["number"].as_str().unwrap_or("");
    let d = digits(number);
    let last4 = if d.len() >= 4 { d[d.len() - 4..].to_string() } else { String::new() };
    json!({
        "id": item["id"],
        "name": item["name"],
        "kind": item["kind"],
        "brand": card_brand(number),
        "last4": last4,
        "exp": format!("{}/{}", f["exp_month"].as_str().unwrap_or("??"),
            f["exp_year"].as_str().unwrap_or("??").chars().rev().take(2).collect::<Vec<_>>().into_iter().rev().collect::<String>()),
        "fields_held": CARD_FIELDS.iter().filter(|k| f[**k].as_str().is_some_and(|v| !v.is_empty())).collect::<Vec<_>>(),
        "rules": item["rules"],
        "status": item["status"],
        "created": item["created"],
        "created_by": item["created_by"],
    })
}

/// Normalise an expiry like "09/28", "9/2028", "2028-09" or "0928" to
/// ("09", "2028").
pub(crate) fn parse_exp(s: &str) -> Option<(String, String)> {
    let parts: Vec<String> = s
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    let (m, y) = match parts.as_slice() {
        [a, b] if a.len() == 4 => (b.clone(), a.clone()),
        [a, b] => (a.clone(), b.clone()),
        [one] if one.len() == 4 => (one[..2].to_string(), one[2..].to_string()),
        [one] if one.len() == 6 => (one[..2].to_string(), one[2..].to_string()),
        _ => return None,
    };
    let m: u32 = m.parse().ok()?;
    if !(1..=12).contains(&m) {
        return None;
    }
    let y = if y.len() == 2 { format!("20{y}") } else { y };
    Some((format!("{m:02}"), y))
}

/// The limit a charge is checked against. `None` means the item has no limit,
/// which this module never creates: a card without a rule gets one.
pub(crate) fn approval_limit_usd(item: &Value) -> Option<f64> {
    item["rules"]["max_usd_without_approval"].as_f64()
}

#[derive(Debug, PartialEq)]
pub(crate) enum SpendDecision {
    Allowed,
    NeedsApproval { limit: f64 },
    Refused(String),
}

pub(crate) fn decide(item: &Value, amount_usd: f64, currency: &str) -> SpendDecision {
    if item["status"].as_str() != Some("active") {
        return SpendDecision::Refused(
            "this vault item is not active yet: the owner activates it from the dashboard".into(),
        );
    }
    if !amount_usd.is_finite() || amount_usd <= 0.0 {
        return SpendDecision::Refused("amount_usd must be a positive number".into());
    }
    if !currency.eq_ignore_ascii_case("usd") {
        // A foreign-currency charge cannot be compared to a USD rule without a
        // rate this module does not have, so it always needs a human.
        return SpendDecision::NeedsApproval { limit: 0.0 };
    }
    match approval_limit_usd(item) {
        Some(limit) if amount_usd > limit => SpendDecision::NeedsApproval { limit },
        Some(_) => SpendDecision::Allowed,
        None => SpendDecision::NeedsApproval { limit: 0.0 },
    }
}

/// The allowance key an approval writes and a retry consumes: one worker, one
/// item, one amount (to the cent), one merchant.
pub(crate) fn spend_target(item_id: &str, amount_usd: f64, merchant: &str) -> String {
    let cents = (amount_usd * 100.0).round() as i64;
    let m: String = merchant
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(60)
        .collect();
    format!("vault_{item_id}_{cents}_{m}")
}

fn is_worker_origin(headers: &HeaderMap) -> bool {
    ["x-amux-session", "x-amux-worker"].iter().any(|h| {
        headers
            .get(*h)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| !s.trim().is_empty())
    })
}

fn origin_name(headers: &HeaderMap) -> String {
    headers
        .get("x-amux-session")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("owner")
        .to_string()
}

fn err(code: StatusCode, v: Value) -> Response {
    (code, Json(v)).into_response()
}

fn new_id() -> String {
    let t = (now_f64() * 1e6) as u128;
    let pid = std::process::id() as u128;
    format!("vlt_{:x}", t ^ (pid << 40))
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

async fn list() -> Response {
    let home = amux_home();
    let items = load_items(&home);
    Json(json!({
        "measured": true,
        "n_considered": items.len(),
        "items": items.iter().map(public_view).collect::<Vec<_>>(),
    }))
    .into_response()
}

/// Add an item. From the dashboard it is active at once. From a worker (a
/// request carrying X-Amux-Session) it is stored PENDING: nothing can use it
/// until the owner activates it, so a worker cannot mint itself a spendable
/// card.
async fn create(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let home = amux_home();
    let name = body["name"].as_str().unwrap_or("").trim().to_string();
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({"error": "name is required"}));
    }
    let src = body["fields"].as_object().cloned().unwrap_or_default();
    let mut fields = Map::new();
    for k in CARD_FIELDS {
        if let Some(v) = src.get(k).and_then(Value::as_str) {
            fields.insert(k.to_string(), json!(v.trim()));
        }
    }
    if let Some(exp) = src.get("exp").and_then(Value::as_str) {
        match parse_exp(exp) {
            Some((m, y)) => {
                fields.insert("exp_month".into(), json!(m));
                fields.insert("exp_year".into(), json!(y));
            }
            None => return err(StatusCode::BAD_REQUEST, json!({"error": "exp is not a recognisable expiry"})),
        }
    }
    let number = digits(fields.get("number").and_then(Value::as_str).unwrap_or(""));
    if !(12..=19).contains(&number.len()) {
        return err(StatusCode::BAD_REQUEST, json!({"error": "fields.number must be a 12-19 digit card number"}));
    }
    fields.insert("number".into(), json!(number));
    // Every card gets a rule; the default is the owner's stated one.
    let limit = body["rules"]["max_usd_without_approval"].as_f64().unwrap_or(100.0);
    let from_worker = is_worker_origin(&headers);
    let by = origin_name(&headers);
    let item = json!({
        "id": new_id(),
        "name": name,
        "kind": "card",
        "fields": fields,
        "rules": {"max_usd_without_approval": limit},
        "status": if from_worker { "pending_owner_activation" } else { "active" },
        "created": now_f64(),
        "created_by": by,
    });
    let mut items = load_items(&home);
    items.push(item.clone());
    if let Err(e) = save_items(&home, &items) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": format!("could not save vault: {e}")}));
    }
    let view = public_view(&item);
    audit(&home, json!({"ts": now_f64(), "event": "item_added", "item": item["id"], "by": by,
        "status": item["status"], "rules": item["rules"]}));
    tracing::info!(item = %item["id"], by = %by, status = %item["status"], measured = true,
        n_considered = 1, verdict = "vault_item_added", "vault item added (values not logged)");
    (StatusCode::CREATED, Json(json!({"ok": true, "item": view}))).into_response()
}

/// Owner only: activate a pending item, or change its rules.
async fn update(headers: HeaderMap, AxPath(id): AxPath<String>, Json(body): Json<Value>) -> Response {
    if is_worker_origin(&headers) {
        return err(StatusCode::FORBIDDEN, json!({"error": "a worker may not change a vault item or its rules",
            "why": "the rules exist to bound what workers can spend; only the owner (the dashboard) sets them"}));
    }
    let home = amux_home();
    let mut items = load_items(&home);
    let Some(item) = items.iter_mut().find(|i| i["id"].as_str() == Some(id.as_str())) else {
        return err(StatusCode::NOT_FOUND, json!({"error": "no such vault item"}));
    };
    if body["activate"].as_bool() == Some(true) {
        item["status"] = json!("active");
    }
    if let Some(limit) = body["rules"]["max_usd_without_approval"].as_f64() {
        if !limit.is_finite() || limit < 0.0 {
            return err(StatusCode::BAD_REQUEST, json!({"error": "max_usd_without_approval must be >= 0"}));
        }
        item["rules"]["max_usd_without_approval"] = json!(limit);
    }
    let view = public_view(item);
    if let Err(e) = save_items(&home, &items) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": format!("could not save vault: {e}")}));
    }
    audit(&home, json!({"ts": now_f64(), "event": "item_updated", "item": id, "change": body}));
    Json(json!({"ok": true, "item": view})).into_response()
}

/// Owner only: remove an item.
async fn remove(headers: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    if is_worker_origin(&headers) {
        return err(StatusCode::FORBIDDEN, json!({"error": "a worker may not delete a vault item"}));
    }
    let home = amux_home();
    let mut items = load_items(&home);
    let before = items.len();
    items.retain(|i| i["id"].as_str() != Some(id.as_str()));
    if items.len() == before {
        return err(StatusCode::NOT_FOUND, json!({"error": "no such vault item"}));
    }
    if let Err(e) = save_items(&home, &items) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": format!("could not save vault: {e}")}));
    }
    audit(&home, json!({"ts": now_f64(), "event": "item_removed", "item": id}));
    Json(json!({"ok": true})).into_response()
}

/// The page script that types a card into the current checkout. Values travel
/// only to Chrome; the script returns WHICH fields it filled, never values.
pub(crate) fn fill_script(values: &Value) -> String {
    format!(
        r#"(() => {{
  const v = {values};
  const pick = (sels) => {{ for (const s of sels) {{ const el = document.querySelector(s); if (el && !el.disabled && el.offsetParent !== null) return el; }} return null; }};
  const set = (el, val) => {{
    const proto = el.tagName === 'SELECT' ? HTMLSelectElement.prototype : HTMLInputElement.prototype;
    const d = Object.getOwnPropertyDescriptor(proto, 'value');
    el.focus(); d.set.call(el, val);
    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
    el.blur();
  }};
  const F = {{
    number: ['[autocomplete="cc-number"]', 'input[name*="cardnumber" i]', 'input[name*="card_number" i]', 'input[id*="cardnumber" i]', 'input[name*="ccnumber" i]'],
    exp: ['[autocomplete="cc-exp"]', 'input[name*="expiry" i]:not([name*="month" i]):not([name*="year" i])', 'input[name="exp" i]'],
    exp_month: ['[autocomplete="cc-exp-month"]', 'select[name*="month" i]', 'input[name*="exp_month" i]'],
    exp_year: ['[autocomplete="cc-exp-year"]', 'select[name*="year" i]', 'input[name*="exp_year" i]'],
    cvc: ['[autocomplete="cc-csc"]', 'input[name*="cvc" i]', 'input[name*="cvv" i]', 'input[name*="csc" i]', 'input[name*="security" i]'],
    name: ['[autocomplete="cc-name"]', 'input[name*="cardholder" i]', 'input[name*="nameoncard" i]', 'input[name*="card_name" i]'],
    address: ['[autocomplete="address-line1"]', '[autocomplete="street-address"]', 'input[name*="address1" i]', 'input[name*="line1" i]'],
    city: ['[autocomplete="address-level2"]', 'input[name*="city" i]'],
    state: ['[autocomplete="address-level1"]', 'select[name*="state" i]', 'input[name*="state" i]'],
    zip: ['[autocomplete="postal-code"]', 'input[name*="zip" i]', 'input[name*="postal" i]'],
    country: ['[autocomplete="country"]', '[autocomplete="country-name"]', 'select[name*="country" i]'],
    email: ['[autocomplete="email"]', 'input[type="email"]'],
  }};
  const filled = [], not_found = [];
  const wantsSplit = !pick(F.exp);
  for (const [k, sels] of Object.entries(F)) {{
    if (v[k] == null || v[k] === '') continue;
    if (k === 'exp' && wantsSplit) continue;
    if ((k === 'exp_month' || k === 'exp_year') && !wantsSplit) continue;
    const el = pick(sels);
    if (el) {{ set(el, v[k]); filled.push(k); }} else not_found.push(k);
  }}
  const iframes = [...document.querySelectorAll('iframe')].filter(f => /stripe|braintree|adyen|checkout|payment|card/i.test(f.src || f.name || '')).length;
  return {{ filled, not_found, payment_iframes: iframes, url: location.href }};
}})()"#
    )
}

/// A worker asks amux to put a vault card into its amux browser page for a
/// stated charge. The rules decide; the number never comes back.
async fn fill(headers: HeaderMap, AxPath(id): AxPath<String>, Json(body): Json<Value>) -> Response {
    let home = amux_home();
    let session = body["session"].as_str().map(str::to_string).unwrap_or_else(|| origin_name(&headers));
    let amount = body["amount_usd"].as_f64().unwrap_or(f64::NAN);
    let currency = body["currency"].as_str().unwrap_or("USD").to_string();
    let merchant = body["merchant"].as_str().unwrap_or("").trim().to_string();
    let purpose = body["purpose"].as_str().unwrap_or("").trim().to_string();
    if merchant.is_empty() || purpose.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({"error": "merchant and purpose are required",
            "why": "they are what the owner reads when approving, and what the audit line records"}));
    }
    let items = load_items(&home);
    let Some(item) = items.iter().find(|i| i["id"].as_str() == Some(id.as_str())).cloned() else {
        return err(StatusCode::NOT_FOUND, json!({"error": "no such vault item"}));
    };
    let base_audit = json!({"ts": now_f64(), "event": "fill", "item": id, "session": session,
        "amount_usd": amount, "currency": currency, "merchant": merchant, "purpose": purpose});
    let target = spend_target(&id, amount, &merchant);
    match decide(&item, amount, &currency) {
        SpendDecision::Refused(why) => {
            let mut a = base_audit.clone();
            a["decision"] = json!("refused");
            a["why"] = json!(why);
            audit(&home, a);
            return err(StatusCode::FORBIDDEN, json!({"ok": false, "error": why}));
        }
        SpendDecision::NeedsApproval { limit } => {
            // A matching approval already granted for exactly this charge?
            if super::grants::take_allowance(&home, &session, &target).is_none() {
                let view = public_view(&item);
                let summary = format!(
                    "{session} wants to charge {currency} {amount:.2} to {} ending {} at {merchant}: {purpose} (rule: approval above ${limit:.0})",
                    view["brand"].as_str().unwrap_or("card"), view["last4"].as_str().unwrap_or("????"));
                let grant = super::grants::create_grant(&home, "vault_spend", &session, &summary, json!({
                    "origin": session, "target": target, "item": id, "amount_usd": amount,
                    "currency": currency, "merchant": merchant, "purpose": purpose,
                }));
                let mut a = base_audit.clone();
                a["decision"] = json!("needs_approval");
                a["grant"] = json!(grant);
                audit(&home, a);
                tracing::warn!(item = %id, session = %session, amount, merchant = %merchant,
                    measured = true, n_considered = 1, verdict = "vault_spend_needs_approval",
                    "vault charge above the item's limit; owner approval requested");
                return err(StatusCode::FORBIDDEN, json!({
                    "ok": false, "requires_approval": true, "grant_id": grant, "limit_usd": limit,
                    "error": format!("a charge of {amount:.2} {currency} is above this card's ${limit:.0} limit and needs the owner's explicit approval"),
                    "next": "the owner approves it in the dashboard (Grants). Then retry this exact request (same amount and merchant) once; the approval is single-use and expires in 1h.",
                }));
            }
        }
        SpendDecision::Allowed => {}
    }
    // Fill the worker's own amux browser page.
    let f = &item["fields"];
    let yy: String = f["exp_year"].as_str().unwrap_or("").chars().rev().take(2).collect::<Vec<_>>().into_iter().rev().collect();
    let values = json!({
        "number": f["number"], "exp": format!("{}/{}", f["exp_month"].as_str().unwrap_or(""), yy),
        "exp_month": f["exp_month"], "exp_year": f["exp_year"], "cvc": f["cvc"], "name": f["name"],
        "address": f["address"], "city": f["city"], "state": f["state"], "zip": f["zip"],
        "country": f["country"], "email": f["email"],
    });
    let result = match super::browser::connect_session(&session, None).await {
        Ok((_page, mut cdp)) => cdp.eval(&fill_script(&values), 15).await.map_err(|e| e.to_string()),
        Err(_) => Err(format!("no amux browser page for session '{session}': open the checkout with /api/browser/start first")),
    };
    let mut a = base_audit;
    match result {
        Ok(r) => {
            let filled = r.get("filled").cloned().unwrap_or(json!([]));
            let not_found = r.get("not_found").cloned().unwrap_or(json!([]));
            a["decision"] = json!("filled");
            a["filled"] = filled.clone();
            a["not_found"] = not_found.clone();
            a["page"] = r.get("url").cloned().unwrap_or(Value::Null);
            audit(&home, a);
            tracing::info!(item = %id, session = %session, amount, merchant = %merchant,
                measured = true, n_considered = 1, verdict = "vault_card_filled",
                "vault card filled into the worker's page (values not logged)");
            Json(json!({"ok": true, "filled": filled, "not_found": not_found,
                "payment_iframes": r.get("payment_iframes"),
                "note": "values were typed into the page by amux and are not returned. A field inside a cross-origin payment iframe cannot be reached from the page and shows up in not_found."}))
                .into_response()
        }
        Err(e) => {
            a["decision"] = json!("fill_failed");
            a["why"] = json!(e);
            audit(&home, a);
            err(StatusCode::BAD_GATEWAY, json!({"ok": false, "error": e}))
        }
    }
}

pub fn routes() -> Router<super::AppState> {
    Router::new()
        .route("/api/vault", get(list).post(create))
        .route("/api/vault/{id}", axum::routing::patch(update).delete(remove))
        .route("/api/vault/{id}/fill", post(fill))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(limit: f64, status: &str) -> Value {
        json!({"id": "vlt_1", "status": status, "rules": {"max_usd_without_approval": limit},
               "fields": {"number": "5500000000000004", "exp_month": "09", "exp_year": "2028"}})
    }

    #[test]
    fn the_hundred_dollar_rule() {
        let c = card(100.0, "active");
        assert_eq!(decide(&c, 99.99, "USD"), SpendDecision::Allowed);
        assert_eq!(decide(&c, 100.0, "usd"), SpendDecision::Allowed);
        assert_eq!(decide(&c, 100.01, "USD"), SpendDecision::NeedsApproval { limit: 100.0 });
        assert_eq!(decide(&c, 5000.0, "USD"), SpendDecision::NeedsApproval { limit: 100.0 });
        // A foreign currency cannot be compared to a USD rule, so a human decides.
        assert_eq!(decide(&c, 1.0, "EUR"), SpendDecision::NeedsApproval { limit: 0.0 });
        assert!(matches!(decide(&c, 0.0, "USD"), SpendDecision::Refused(_)));
        assert!(matches!(decide(&c, f64::NAN, "USD"), SpendDecision::Refused(_)));
        assert!(matches!(decide(&card(100.0, "pending_owner_activation"), 1.0, "USD"), SpendDecision::Refused(_)));
    }

    #[test]
    fn the_public_view_never_carries_a_secret() {
        let mut c = card(100.0, "active");
        c["fields"]["cvc"] = json!("123");
        c["fields"]["name"] = json!("Card Holder");
        let v = public_view(&c).to_string();
        assert!(!v.contains("5500000000000004") && !v.contains("\"123\""), "{v}");
        assert!(v.contains("\"last4\":\"0004\"") && v.contains("Mastercard") && v.contains("09/28"), "{v}");
    }

    #[test]
    fn an_approval_is_for_one_amount_and_one_merchant() {
        assert_eq!(spend_target("vlt_1", 150.0, "OpenAI, Inc."), "vault_vlt_1_15000_openaiinc");
        assert_ne!(spend_target("vlt_1", 150.0, "OpenAI"), spend_target("vlt_1", 150.01, "OpenAI"));
        assert_ne!(spend_target("vlt_1", 150.0, "OpenAI"), spend_target("vlt_1", 150.0, "Anthropic"));
    }

    #[test]
    fn expiries_normalise() {
        for (s, want) in [("09/28", ("09", "2028")), ("9/2028", ("09", "2028")), ("2028-09", ("09", "2028")), ("0928", ("09", "2028"))] {
            assert_eq!(parse_exp(s), Some((want.0.to_string(), want.1.to_string())), "{s}");
        }
        assert_eq!(parse_exp("13/28"), None);
    }
}
