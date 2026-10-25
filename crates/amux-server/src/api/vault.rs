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
//! (Stripe Elements, Braintree hosted fields) is filled by attaching to that
//! frame over CDP (`fill_with_frames`), only for frames served by a payment
//! provider (`PAYMENT_FRAME_HOSTS`, extended by AMUX_VAULT_PAYMENT_FRAME_HOSTS).
//! A field still unreached is reported back as `not_found`, never guessed at.

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

/// The one audit writer for every vault item kind (cards here, secrets in
/// vault_secrets.rs), so there is one file to read.
pub(crate) fn audit_line(home: &Path, entry: Value) {
    audit(home, entry)
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
    if item["kind"].as_str() == Some("login") {
        return login_public_view(item);
    }
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

/// A worker that does not act for the owner. An admin-group worker acts with
/// the owner's reach (CLAUDE.md), so it adds items as active and may edit
/// them; every other worker is held to the pending/refused path below.
/// `admin_origin` logs each admin pass (`admin_group_owner_gate`).
fn non_owner_worker(headers: &HeaderMap, gate: &str) -> bool {
    is_worker_origin(headers) && super::session_verbs::admin_origin(headers, gate).is_none()
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
// Login items: a username and password amux types into a sign-in page itself
// ---------------------------------------------------------------------------
//
// Same shape as a card (AC-475, 2026-10-06). A worker on the Wexus host had
// the NetSuite password in a file, correctly refused to read it or type it
// into a form, and had no other way in. The vault fixes that the way it fixes
// checkout: the worker opens the sign-in page in its amux browser and names
// the item; amux types the values in. The password never reaches a prompt, a
// transcript, a log line or an API response.
//
// The rule that makes it safe to hand a worker this verb: an item carries the
// ORIGINS it may be typed into, and amux checks the page's own host before
// filling. A worker that navigates somewhere else and asks for a fill is
// refused, so the verb cannot be used to post the password to a page of the
// worker's choosing.

/// Host-suffix match: `netsuite.com` allows `system.netsuite.com` and
/// `1234567.app.netsuite.com`, never `netsuite.com.evil.example`.
pub(crate) fn host_allowed(host: &str, origins: &[String]) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    origins.iter().any(|o| {
        let o = o.trim().trim_start_matches("https://").trim_start_matches("http://")
            .trim_start_matches("*.").trim_end_matches('/').to_ascii_lowercase();
        !o.is_empty() && (host == o || host.ends_with(&format!(".{o}")))
    })
}

fn mask_username(u: &str) -> String {
    match u.split_once('@') {
        Some((local, domain)) => {
            let first: String = local.chars().take(1).collect();
            format!("{first}***@{domain}")
        }
        None => {
            let first: String = u.chars().take(1).collect();
            format!("{first}***")
        }
    }
}

fn login_public_view(item: &Value) -> Value {
    let f = &item["fields"];
    let held: Vec<&str> = ["username", "password"]
        .into_iter()
        .filter(|k| f[*k].as_str().is_some_and(|v| !v.is_empty()))
        .collect();
    json!({
        "id": item["id"],
        "name": item["name"],
        "kind": "login",
        "username_hint": mask_username(f["username"].as_str().unwrap_or("")),
        "origins": item["rules"]["origins"],
        "fields_held": held,
        "rules": item["rules"],
        "status": item["status"],
        "created": item["created"],
        "created_by": item["created_by"],
    })
}

fn create_login(home: &Path, headers: &HeaderMap, name: String, body: &Value) -> Response {
    let f = &body["fields"];
    let username = f["username"].as_str().unwrap_or("").trim().to_string();
    let password = f["password"].as_str().unwrap_or("").to_string();
    if username.is_empty() || password.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({"error": "fields.username and fields.password are required"}));
    }
    let origins: Vec<String> = body["rules"]["origins"].as_array().map(|a| {
        a.iter().filter_map(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
    }).unwrap_or_default();
    if origins.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({
            "error": "rules.origins is required: the hosts this login may be typed into, e.g. [\"netsuite.com\"]",
            "why": "amux checks the page's host against it before every fill, so a worker cannot send the password to a page of its choosing"}));
    }
    let from_worker = non_owner_worker(headers, "vault_create");
    let by = origin_name(headers);
    let item = json!({
        "id": new_id(),
        "name": name,
        "kind": "login",
        "fields": {"username": username, "password": password},
        "rules": {"origins": origins},
        "status": if from_worker { "pending_owner_activation" } else { "active" },
        "created": now_f64(),
        "created_by": by,
    });
    let mut items = load_items(home);
    items.push(item.clone());
    if let Err(e) = save_items(home, &items) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": format!("could not save vault: {e}")}));
    }
    audit(home, json!({"ts": now_f64(), "event": "item_added", "item": item["id"], "kind": "login", "by": by,
        "status": item["status"], "rules": item["rules"]}));
    tracing::info!(item = %item["id"], by = %by, status = %item["status"], measured = true,
        n_considered = 1, verdict = "vault_login_added", "vault login added (values not logged)");
    (StatusCode::CREATED, Json(json!({"ok": true, "item": login_public_view(&item)}))).into_response()
}

pub(crate) fn login_fill_script(username: &str, password: &str, submit: bool) -> String {
    let u = serde_json::to_string(username).unwrap_or_else(|_| "\"\"".into());
    let p = serde_json::to_string(password).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"(() => {{
  const U = {u}, P = {p}, SUBMIT = {submit};
  const vis = el => el && !el.disabled && el.offsetParent !== null && el.type !== 'hidden';
  const pick = sels => {{ for (const s of sels) for (const el of document.querySelectorAll(s)) if (vis(el)) return el; return null; }};
  const set = (el, val) => {{
    const d = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value');
    el.focus(); d.set.call(el, val);
    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
  }};
  const user = pick(['input[autocomplete="username"]', 'input[type="email"]', 'input[name*="email" i]', 'input[id*="email" i]',
                     'input[name*="user" i]', 'input[id*="user" i]', 'input[name*="login" i]']);
  const pass = pick(['input[autocomplete="current-password"]', 'input[type="password"]']);
  const filled = [], not_found = [];
  if (user && user !== pass) {{ set(user, U); filled.push('username'); }} else not_found.push('username');
  if (pass) {{ set(pass, P); filled.push('password'); }} else not_found.push('password');
  let submitted = false;
  if (SUBMIT && filled.length) {{
    // Pick by the .type PROPERTY, not a [type=submit] selector: a <button> with
    // no type attribute is a submit button by default and the selector misses
    // it (Oracle's sign-in, 2026-10-06). Visible only, the field's own form
    // first: pages carry hidden forms whose buttons do nothing useful.
    const field = pass || user;
    const isSubmit = el => el.tagName === 'BUTTON' ? el.type === 'submit' : (el.tagName === 'INPUT' && el.type === 'submit');
    const words = /^(next|continue|sign ?in|log ?in|submit|verify)$/i;
    const all = [...document.querySelectorAll('button, input[type="submit"]')].filter(vis);
    const own = field.form ? all.filter(b => b.form === field.form && isSubmit(b)) : [];
    const btn = own[0] || all.find(b => isSubmit(b) && words.test((b.innerText || b.value || '').trim()))
      || all.find(b => words.test((b.innerText || b.value || '').trim())) || all.find(isSubmit);
    if (btn) {{ btn.click(); submitted = true; }}
    else if (field.form && vis(field)) {{ field.form.requestSubmit ? field.form.requestSubmit() : field.form.submit(); submitted = true; }}
  }}
  return {{ filled, not_found, submitted, url: location.href }};
}})()"#
    )
}

async fn fill_login(home: &Path, id: &str, item: &Value, session: &str, body: &Value) -> Response {
    let submit = body["submit"].as_bool().unwrap_or(false);
    let origins: Vec<String> = item["rules"]["origins"].as_array()
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
    let mut a = json!({"ts": now_f64(), "event": "fill", "kind": "login", "item": id, "session": session, "submit": submit});
    if item["status"].as_str() != Some("active") {
        a["decision"] = json!("refused");
        a["why"] = json!("not active");
        audit(home, a);
        return err(StatusCode::FORBIDDEN, json!({"ok": false,
            "error": "this vault login is not active yet: the owner activates it from the dashboard"}));
    }
    let (_page, mut cdp) = match super::browser::connect_session(session, None).await {
        Ok(x) => x,
        Err(_) => {
            a["decision"] = json!("fill_failed");
            a["why"] = json!("no browser page");
            audit(home, a);
            return err(StatusCode::BAD_GATEWAY, json!({"ok": false,
                "error": format!("no amux browser page for session '{session}': open the sign-in page with /api/browser/start first")}));
        }
    };
    // Read the host from the page itself, not from anything the caller says.
    let host = match cdp.eval("location.hostname", 10).await {
        Ok(v) => v.as_str().unwrap_or("").to_string(),
        Err(e) => {
            a["decision"] = json!("fill_failed");
            a["why"] = json!(e.to_string());
            audit(home, a);
            return err(StatusCode::BAD_GATEWAY, json!({"ok": false, "error": format!("could not read the page's host: {e}")}));
        }
    };
    a["host"] = json!(host);
    if !host_allowed(&host, &origins) {
        a["decision"] = json!("refused_origin");
        audit(home, a);
        tracing::warn!(item = %id, session = %session, host = %host, measured = true, n_considered = 1,
            verdict = "vault_login_origin_refused", "vault login fill refused: page host is not one of the item's origins");
        return err(StatusCode::FORBIDDEN, json!({"ok": false,
            "error": format!("this login may only be typed into {origins:?}; the page is on {host}"),
            "next": "navigate the browser to the sign-in page for one of those hosts, then retry"}));
    }
    let f = &item["fields"];
    let script = login_fill_script(f["username"].as_str().unwrap_or(""), f["password"].as_str().unwrap_or(""), submit);
    match cdp.eval(&script, 15).await {
        Ok(r) => {
            let filled = r.get("filled").cloned().unwrap_or(json!([]));
            let not_found = r.get("not_found").cloned().unwrap_or(json!([]));
            a["decision"] = json!("filled");
            a["filled"] = filled.clone();
            a["not_found"] = not_found.clone();
            a["submitted"] = r.get("submitted").cloned().unwrap_or(json!(false));
            audit(home, a);
            tracing::info!(item = %id, session = %session, host = %host, measured = true, n_considered = 1,
                verdict = "vault_login_filled", "vault login filled into the worker's page (values not logged)");
            Json(json!({"ok": true, "host": host, "filled": filled, "not_found": not_found,
                "submitted": r.get("submitted"),
                "note": "values were typed into the page by amux and are not returned. A two-step sign-in (email, then password) needs a second fill on the second page."}))
                .into_response()
        }
        Err(e) => {
            a["decision"] = json!("fill_failed");
            a["why"] = json!(e.to_string());
            audit(home, a);
            err(StatusCode::BAD_GATEWAY, json!({"ok": false, "error": e.to_string()}))
        }
    }
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
        "items": items.iter().map(|i| {
            let mut v = public_view(i);
            if i["kind"].as_str() != Some("login") {
                // What each window has spent so far, beside the limits it is
                // checked against (vault_spend.rs), so the owner sees headroom.
                v["spent_usd"] = super::vault_spend::spent_summary(&home, i["id"].as_str().unwrap_or(""));
            }
            v
        }).collect::<Vec<_>>(),
    }))
    .into_response()
}

/// Add an item. From the dashboard or an admin-group worker it is active at
/// once. From any other worker (a request carrying X-Amux-Session) it is stored PENDING: nothing can use it
/// until the owner activates it, so a worker cannot mint itself a spendable
/// card.
async fn create(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let home = amux_home();
    let name = body["name"].as_str().unwrap_or("").trim().to_string();
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({"error": "name is required"}));
    }
    if body["kind"].as_str() == Some("login") {
        return create_login(&home, &headers, name, &body);
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
    if !limit.is_finite() || limit < 0.0 {
        return err(StatusCode::BAD_REQUEST, json!({"error": "max_usd_without_approval must be >= 0"}));
    }
    let spend_limits = match body["rules"].get("spend_limits_usd") {
        None | Some(Value::Null) => json!({}),
        Some(p) => match super::vault_spend::merge_limits(&json!({}), p) {
            Ok(m) => m,
            Err(e) => return err(StatusCode::BAD_REQUEST, json!({"error": e, "field": "spend_limits_usd"})),
        },
    };
    let from_worker = non_owner_worker(&headers, "vault_create");
    let by = origin_name(&headers);
    let item = json!({
        "id": new_id(),
        "name": name,
        "kind": "card",
        "fields": fields,
        "rules": {"max_usd_without_approval": limit, "spend_limits_usd": spend_limits},
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

/// Owner or an admin-group worker: activate a pending item, or change its rules.
async fn update(headers: HeaderMap, AxPath(id): AxPath<String>, Json(body): Json<Value>) -> Response {
    if non_owner_worker(&headers, "vault_update") {
        return err(StatusCode::FORBIDDEN, json!({"error": "a worker may not change a vault item or its rules",
            "why": "the rules exist to bound what workers can spend; only the owner (the dashboard) or an admin-group worker sets them"}));
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
    if let Some(p) = body["rules"].get("spend_limits_usd") {
        match super::vault_spend::merge_limits(&item["rules"]["spend_limits_usd"], p) {
            Ok(m) => item["rules"]["spend_limits_usd"] = m,
            Err(e) => return err(StatusCode::BAD_REQUEST, json!({"error": e, "field": "spend_limits_usd"})),
        }
    }
    let view = public_view(item);
    if let Err(e) = save_items(&home, &items) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": format!("could not save vault: {e}")}));
    }
    audit(&home, json!({"ts": now_f64(), "event": "item_updated", "item": id, "by": origin_name(&headers), "change": body}));
    Json(json!({"ok": true, "item": view})).into_response()
}

/// Owner or an admin-group worker: remove an item.
async fn remove(headers: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    if non_owner_worker(&headers, "vault_delete") {
        return err(StatusCode::FORBIDDEN, json!({"error": "a worker may not delete a vault item",
            "why": "only the owner (the dashboard) or an admin-group worker removes vault items"}));
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
    audit(&home, json!({"ts": now_f64(), "event": "item_removed", "item": id, "by": origin_name(&headers)}));
    Json(json!({"ok": true})).into_response()
}

/// The page script that types a card into the current checkout. Values travel
/// only to Chrome; the script returns WHICH fields it filled, never values.
/// The script lives in `vault_fill.js` so a browser test runs these exact bytes.
pub(crate) const FILL_SCRIPT_JS: &str = include_str!("vault_fill.js");

pub(crate) fn fill_script(values: &Value) -> String {
    FILL_SCRIPT_JS.replacen("__AMUX_FILL_VALUES__", &values.to_string(), 1)
}

/// Hosts whose iframes may receive card fields: the payment providers'
/// hosted-field frames. A cross-origin frame from anyone else (an ad, a chat
/// widget, analytics) never does. AMUX_VAULT_PAYMENT_FRAME_HOSTS adds more,
/// comma-separated.
pub(crate) const PAYMENT_FRAME_HOSTS: [&str; 4] = ["stripe.com", "braintreegateway.com", "braintree-api.com", "adyen.com"];

pub(crate) fn payment_frame_hosts() -> Vec<String> {
    let mut v: Vec<String> = PAYMENT_FRAME_HOSTS.iter().map(|h| h.to_string()).collect();
    if let Ok(extra) = std::env::var("AMUX_VAULT_PAYMENT_FRAME_HOSTS") {
        v.extend(extra.split(',').map(|h| h.trim().to_ascii_lowercase()).filter(|h| !h.is_empty()));
    }
    v
}

/// The host of `url` when it is one of `allowed` or under one.
pub(crate) fn payment_frame_host(url: &str, allowed: &[String]) -> Option<String> {
    let host = url.split("://").nth(1)?.split(['/', '?', '#']).next()?;
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = host.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).map_or(host, |(h, _)| h).to_ascii_lowercase();
    allowed.iter().any(|a| host == *a || host.ends_with(&format!(".{a}"))).then_some(host)
}

/// The card keys a payment frame may hold. Address and email stay on the page.
const FRAME_KEYS: [&str; 7] = ["number", "exp", "exp_month", "exp_year", "cvc", "name", "zip"];

fn keys_of(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|k| k.as_str().map(str::to_string)).collect()
}

/// Merge the page's fill result with each payment frame's, into the shape
/// `fill_outcome` reads: a key filled anywhere is filled; a mismatch counts
/// only if no frame filled that key; not_found is what the page could not find
/// and no frame filled or mismatched.
pub(crate) fn merge_frame_fills(top: &Value, frames: &[(String, Value)], skipped: &[String]) -> Value {
    let mut filled = keys_of(&top["filled"]);
    for (_, r) in frames {
        for k in keys_of(&r["filled"]) {
            if !filled.contains(&k) {
                filled.push(k);
            }
        }
    }
    let mut mismatched: Vec<Value> = Vec::new();
    for m in top["mismatched"].as_array().into_iter().flatten()
        .chain(frames.iter().flat_map(|(_, r)| r["mismatched"].as_array().into_iter().flatten()))
    {
        let k = m["key"].as_str().unwrap_or("").to_string();
        if !filled.contains(&k) && !mismatched.iter().any(|x| x["key"] == m["key"]) {
            mismatched.push(m.clone());
        }
    }
    let not_found: Vec<String> = keys_of(&top["not_found"]).into_iter()
        .filter(|k| !filled.contains(k) && !mismatched.iter().any(|m| m["key"].as_str() == Some(k)))
        .collect();
    let mut out = top.clone();
    out["filled"] = json!(filled);
    out["mismatched"] = json!(mismatched);
    out["not_found"] = json!(not_found);
    out["frames"] = json!(frames.iter().map(|(h, r)| json!({"host": h, "filled": r["filled"]})).collect::<Vec<_>>());
    out["frames_skipped"] = json!(skipped);
    out
}

/// Fill the page, then any card field still empty inside the page's payment
/// iframes. Each frame runs the same script; a field that rejects a value set
/// by script is typed into by Chrome itself (Input.insertText), which is what
/// Stripe's and Braintree's hosted fields accept.
pub(crate) async fn fill_with_frames(
    cdp: &mut crate::integrations::browser::CdpClient,
    values: &Value,
    allowed: &[String],
) -> anyhow::Result<Value> {
    let top = cdp.eval(&fill_script(values), 15).await?;
    let done = keys_of(&top["filled"]);
    let mut want = serde_json::Map::new();
    for k in FRAME_KEYS {
        let v = &values[k];
        if !done.iter().any(|d| d == k || (k.starts_with("exp") && d.starts_with("exp")))
            && v.as_str().is_some_and(|s| !s.is_empty())
        {
            want.insert(k.to_string(), v.clone());
        }
    }
    if want.is_empty() {
        return Ok(merge_frame_fills(&top, &[], &[]));
    }
    let attached = cdp.attach_frames(std::time::Duration::from_millis(600)).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, verdict = "vault_frames_unmeasured", "could not attach to the page's iframes");
        Vec::new()
    });
    let want = Value::Object(want);
    let (mut frames, mut skipped) = (Vec::new(), Vec::new());
    for (sid, url) in &attached {
        let Some(host) = payment_frame_host(url, allowed) else {
            skipped.push(url.split('/').take(3).collect::<Vec<_>>().join("/"));
            continue;
        };
        let mut r = match cdp.eval_in(sid, &fill_script(&want), 15).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(host = %host, error = %e, verdict = "vault_frame_fill_failed", "payment frame did not run the fill");
                continue;
            }
        };
        // Real typing for what a script-set value did not stick in.
        let mut still = Vec::new();
        for m in r["mismatched"].as_array().cloned().unwrap_or_default() {
            let key = m["key"].as_str().unwrap_or("").to_string();
            let val = want[&key].as_str().unwrap_or("").to_string();
            let focused = cdp.eval_in(sid, &fill_script(&json!({"__focus": key})), 10).await
                .ok().and_then(|f| f["focused"].as_bool()).unwrap_or(false);
            if focused {
                let _ = cdp.call_in(Some(sid), "Input.insertText", json!({"text": val}),
                    std::time::Duration::from_secs(10), None).await;
            }
            let ok = focused && cdp.eval_in(sid, &fill_script(&json!({"__check": {"key": key, "val": val}})), 10).await
                .ok().and_then(|c| c["took"].as_bool()).unwrap_or(false);
            if ok {
                if let Some(f) = r["filled"].as_array_mut() {
                    f.push(json!(key));
                }
            } else {
                still.push(m);
            }
        }
        r["mismatched"] = json!(still);
        frames.push((host, r));
    }
    let merged = merge_frame_fills(&top, &frames, &skipped);
    tracing::info!(attached = attached.len(), reached = frames.len(), skipped = ?skipped,
        filled = %merged["filled"], measured = true, n_considered = attached.len(),
        verdict = "vault_fill_frames", "filled card fields inside payment iframes (values not logged)");
    Ok(merged)
}

/// What a fill result says about the form, read the same way by both callers.
/// A fill that typed nothing, or wrote a field that did not keep its value, is
/// not a filled form (GE-1121: "order form filled by the harness (fields: )"
/// exited 0 with nothing in the page).
pub(crate) struct FillOutcome {
    pub filled: Value,
    pub not_found: Value,
    pub mismatched: Value,
    pub complete: bool,
    pub why: String,
}

pub(crate) fn fill_outcome(r: &Value) -> FillOutcome {
    let filled = r.get("filled").cloned().filter(Value::is_array).unwrap_or(json!([]));
    let not_found = r.get("not_found").cloned().filter(Value::is_array).unwrap_or(json!([]));
    let mismatched = r.get("mismatched").cloned().filter(Value::is_array).unwrap_or(json!([]));
    let n_filled = filled.as_array().map_or(0, Vec::len);
    let n_bad = mismatched.as_array().map_or(0, Vec::len);
    let why = if n_filled == 0 && n_bad == 0 {
        "no payment field on this page took a value: the form was not filled. Payment iframes are reached only when served by a payment provider (frames, frames_skipped); check payment_iframes and not_found".to_string()
    } else if n_bad > 0 {
        let keys: Vec<&str> = mismatched.as_array().into_iter().flatten()
            .filter_map(|m| m["key"].as_str()).collect();
        format!("{} field(s) did not keep the value written to them ({}): the form is not correctly filled", n_bad, keys.join(", "))
    } else {
        String::new()
    };
    FillOutcome { filled, not_found, mismatched, complete: why.is_empty(), why }
}

/// The money gate, shared by `fill` and `complete_order`: the item's rules,
/// then its spending limits, then the approval threshold, then the atomic
/// check-and-record in the spend ledger (vault_spend.rs). `Ok` carries the
/// ledger id of the recorded charge; `Err` is the response to return, already
/// audited and logged.
#[allow(clippy::too_many_arguments)]
fn gate_charge(home: &Path, id: &str, item: &Value, session: &str, amount: f64, currency: &str,
    merchant: &str, purpose: &str, path: &str, base_audit: &Value) -> Result<String, Box<Response>> {
    let refuse_limit = |b: &super::vault_spend::Breach| {
        let mut a = base_audit.clone();
        a["decision"] = json!("refused_spend_limit");
        a["window"] = json!(b.window);
        a["limit_usd"] = json!(b.limit_usd);
        a["spent_usd"] = json!(b.spent_usd);
        audit(home, a);
        tracing::warn!(item = %id, session = %session, amount, merchant = %merchant, window = b.window,
            limit_usd = b.limit_usd, spent_usd = b.spent_usd, path, measured = true, n_considered = 1,
            verdict = "vault_spend_limit_exceeded",
            "vault charge refused: it would exceed the card's spending limit (values not logged)");
        err(StatusCode::FORBIDDEN, b.body(id))
    };
    let decision = decide(item, amount, currency);
    if let SpendDecision::Refused(why) = &decision {
        let mut a = base_audit.clone();
        a["decision"] = json!("refused");
        a["why"] = json!(why);
        audit(home, a);
        return Err(Box::new(err(StatusCode::FORBIDDEN, json!({"ok": false, "error": why}))));
    }
    // A spending limit is a hard cap: check it before an approval is spent or
    // requested, so the owner is never asked to approve what a limit refuses.
    if let Some(b) = super::vault_spend::check(home, item, amount) {
        return Err(Box::new(refuse_limit(&b)));
    }
    if let SpendDecision::NeedsApproval { limit } = decision {
        let target = spend_target(id, amount, merchant);
        if super::grants::take_allowance(home, session, &target).is_none() {
            let view = public_view(item);
            let summary = format!(
                "{session} wants to charge {currency} {amount:.2} to {} ending {} at {merchant}: {purpose} (rule: approval above ${limit:.0})",
                view["brand"].as_str().unwrap_or("card"), view["last4"].as_str().unwrap_or("????"));
            let grant = super::grants::create_grant(home, "vault_spend", session, &summary, json!({
                "origin": session, "target": target, "item": id, "amount_usd": amount,
                "currency": currency, "merchant": merchant, "purpose": purpose,
            }));
            let mut a = base_audit.clone();
            a["decision"] = json!("needs_approval");
            a["grant"] = json!(grant);
            audit(home, a);
            tracing::warn!(item = %id, session = %session, amount, merchant = %merchant, path,
                measured = true, n_considered = 1, verdict = "vault_spend_needs_approval",
                "vault charge above the item's limit; owner approval requested");
            return Err(Box::new(err(StatusCode::FORBIDDEN, json!({
                "ok": false, "requires_approval": true, "grant_id": grant, "limit_usd": limit,
                "error": format!("a charge of {amount:.2} {currency} is above this card's ${limit:.0} limit and needs the owner's explicit approval"),
                "next": "the owner approves it in the dashboard (Grants). Then retry this exact request (same amount and merchant) once; the approval is single-use and expires in 1h.",
            }))));
        }
    }
    let charge = super::vault_spend::Charge { amount_usd: amount, merchant, purpose, session, path };
    match super::vault_spend::reserve(home, item, &charge) {
        Ok(spend) => Ok(spend),
        Err(super::vault_spend::ReserveError::Limit(b)) => Err(Box::new(refuse_limit(&b))),
        Err(super::vault_spend::ReserveError::Io(e)) => {
            let mut a = base_audit.clone();
            a["decision"] = json!("refused_ledger_unwritable");
            a["why"] = json!(e);
            audit(home, a);
            tracing::warn!(item = %id, session = %session, error = %e, measured = true, n_considered = 1,
                verdict = "vault_spend_ledger_unwritable",
                "vault charge refused: the spend ledger could not be written, so the limits cannot be enforced");
            Err(Box::new(err(StatusCode::SERVICE_UNAVAILABLE, json!({"ok": false,
                "error": format!("the spend ledger could not be written ({e}); no charge goes through unrecorded")}))))
        }
    }
}

/// A worker asks amux to put a vault card into its amux browser page for a
/// stated charge. The rules decide; the number never comes back.
async fn fill(headers: HeaderMap, AxPath(id): AxPath<String>, Json(body): Json<Value>) -> Response {
    let home = amux_home();
    let session = body["session"].as_str().map(str::to_string).unwrap_or_else(|| origin_name(&headers));
    if let Some(item) = load_items(&home).into_iter().find(|i| i["id"].as_str() == Some(id.as_str())) {
        if item["kind"].as_str() == Some("login") {
            return fill_login(&home, &id, &item, &session, &body).await;
        }
    }
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
    let spend = match gate_charge(&home, &id, &item, &session, amount, &currency, &merchant, &purpose, "fill", &base_audit) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    // Fill the worker's own amux browser page.
    let f = &item["fields"];
    let yy: String = f["exp_year"].as_str().unwrap_or("").chars().rev().take(2).collect::<Vec<_>>().into_iter().rev().collect();
    let values = json!({
        "number": f["number"], "exp": format!("{}/{}", f["exp_month"].as_str().unwrap_or(""), yy),
        "exp_month": f["exp_month"], "exp_year": f["exp_year"], "cvc": f["cvc"], "name": f["name"],
        "address": f["address"], "city": f["city"], "state": f["state"], "zip": f["zip"],
        "country": f["country"], "email": f["email"],
    });
    let mut reached_page = true;
    let result = match super::browser::connect_session(&session, None).await {
        Ok((_page, mut cdp)) => fill_with_frames(&mut cdp, &values, &payment_frame_hosts()).await.map_err(|e| e.to_string()),
        Err(_) => {
            reached_page = false;
            Err(format!("no amux browser page for session '{session}': open the checkout with /api/browser/start first"))
        }
    };
    let mut a = base_audit;
    a["spend"] = json!(spend);
    match result {
        Ok(r) => {
            let o = fill_outcome(&r);
            release_if_nothing_typed(&home, &spend, &o, &mut a);
            a["decision"] = json!(if o.complete { "filled" } else { "fill_incomplete" });
            a["filled"] = o.filled.clone();
            a["not_found"] = o.not_found.clone();
            a["mismatched"] = o.mismatched.clone();
            a["page"] = r.get("url").cloned().unwrap_or(Value::Null);
            audit(&home, a);
            if !o.complete {
                tracing::warn!(item = %id, session = %session, amount, merchant = %merchant,
                    filled = %o.filled, not_found = %o.not_found, mismatched = %o.mismatched,
                    measured = true, n_considered = 1, verdict = "vault_fill_incomplete",
                    "vault card fill did not fill the form (values not logged)");
                return err(StatusCode::UNPROCESSABLE_ENTITY, json!({"ok": false, "error": o.why,
                    "filled": o.filled, "not_found": o.not_found, "mismatched": o.mismatched,
                    "payment_iframes": r.get("payment_iframes"), "frames": r.get("frames"),
                    "frames_skipped": r.get("frames_skipped"), "page": r.get("url")}));
            }
            tracing::info!(item = %id, session = %session, amount, merchant = %merchant,
                measured = true, n_considered = 1, verdict = "vault_card_filled",
                "vault card filled into the worker's page (values not logged)");
            Json(json!({"ok": true, "filled": o.filled, "not_found": o.not_found,
                "payment_iframes": r.get("payment_iframes"),
                "frames": r.get("frames"),
                "note": "values were typed into the page by amux and are not returned. Card fields inside a payment provider's iframe are filled there (frames); anything unreached shows up in not_found."}))
                .into_response()
        }
        Err(e) => {
            a["decision"] = json!("fill_failed");
            a["why"] = json!(e);
            if !reached_page {
                super::vault_spend::release(&home, &spend, "no browser page: the card never reached a page");
                a["spend_released"] = json!(true);
            }
            audit(&home, a);
            err(StatusCode::BAD_GATEWAY, json!({"ok": false, "error": e}))
        }
    }
}

/// A fill that typed no field cannot have moved money, so its reservation is
/// released. Any field typed keeps the charge counted: amux cannot see whether
/// the checkout was submitted afterwards, and over-counting is the safe side.
fn release_if_nothing_typed(home: &Path, spend: &str, o: &FillOutcome, a: &mut Value) {
    if o.filled.as_array().is_none_or(Vec::is_empty) {
        super::vault_spend::release(home, spend, "the fill typed no field");
        a["spend_released"] = json!(true);
    }
}

/// JS that navigates a checkout flow: detects the current page type (product,
/// cart, checkout) and advances to the next step. Returns a JSON object
/// describing what it did and what page it ended on.
fn checkout_navigate_script(quantity: u64) -> String {
    format!(
        r#"(async () => {{
  const sleep = ms => new Promise(r => setTimeout(r, ms));
  const result = {{ steps: [], page_type: "unknown", url: location.href }};

  // Detect page type by looking for known patterns
  const hasCheckoutForm = !!document.querySelector(
    '[autocomplete="cc-number"], input[name*="cardnumber" i], #payment, .wc-credit-card-form, .payment_methods'
  );
  const hasCartTable = !!document.querySelector('.woocommerce-cart-form, .cart_totals, [class*="cart-item"]');
  const hasTicketForm = !!document.querySelector(
    '#tribe-tickets__tickets-form, .tribe-tickets__tickets-form, [class*="ticket-form"], .eventbrite-widget'
  );
  const hasBuyButton = !!document.querySelector(
    'button[class*="checkout"], .checkout-button, a[href*="checkout"], .wc-proceed-to-checkout'
  );

  if (hasCheckoutForm) {{
    result.page_type = "checkout";
    result.steps.push("already on checkout page");
    return result;
  }}

  if (hasCartTable) {{
    result.page_type = "cart";
    result.steps.push("on cart page, looking for checkout button");
    const checkoutBtn = document.querySelector(
      '.checkout-button, a[href*="checkout"], .wc-proceed-to-checkout a, a.checkout-button'
    );
    if (checkoutBtn) {{
      checkoutBtn.click();
      result.steps.push("clicked proceed to checkout");
      await sleep(3000);
      result.url = location.href;
      result.page_type = "checkout";
      return result;
    }}
    result.steps.push("no checkout button found on cart");
    return result;
  }}

  if (hasTicketForm) {{
    result.page_type = "ticket_form";
    // Set quantity
    const qtyInput = document.querySelector(
      'input[type="number"][id*="quantity"], input[type="number"][class*="quantity"]'
    );
    if (qtyInput) {{
      const nativeSet = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value');
      qtyInput.focus();
      nativeSet.set.call(qtyInput, '{qty}');
      qtyInput.dispatchEvent(new Event('input', {{ bubbles: true }}));
      qtyInput.dispatchEvent(new Event('change', {{ bubbles: true }}));
      qtyInput.blur();
      result.steps.push("set ticket quantity to {qty}");
    }}

    // Click "Get Tickets" or similar submit button
    const submitBtn = document.querySelector(
      'button[type="submit"]:not([disabled]), .tribe-tickets__buy button, button[class*="submit"]'
    );
    const allButtons = [...document.querySelectorAll('button')];
    const getTicketsBtn = submitBtn || allButtons.find(b =>
      /get tickets|buy|add to cart|purchase/i.test(b.textContent)
    );
    if (getTicketsBtn) {{
      getTicketsBtn.click();
      result.steps.push("clicked: " + (getTicketsBtn.textContent || "").trim().slice(0, 40));
      // Wait for navigation or AJAX
      await sleep(3000);
      result.url = location.href;

      // Check if we ended up on cart or checkout
      if (location.href.includes('/cart') || location.href.includes('/basket')) {{
        result.page_type = "cart";
        // Auto-proceed to checkout from cart
        await sleep(1000);
        const checkoutBtn2 = document.querySelector(
          '.checkout-button, a[href*="checkout"], .wc-proceed-to-checkout a'
        );
        if (checkoutBtn2) {{
          checkoutBtn2.click();
          result.steps.push("auto-proceeded to checkout from cart");
          await sleep(3000);
          result.url = location.href;
          result.page_type = "checkout";
        }}
      }} else if (location.href.includes('/checkout')) {{
        result.page_type = "checkout";
      }}
    }} else {{
      result.steps.push("no submit button found on ticket form");
    }}
    return result;
  }}

  // Generic product page: look for add-to-cart
  const addBtn = document.querySelector(
    'button[name="add-to-cart"], .single_add_to_cart_button, button[class*="add-to-cart"]'
  );
  if (addBtn) {{
    result.page_type = "product";
    addBtn.click();
    result.steps.push("clicked add to cart");
    await sleep(3000);
    result.url = location.href;
  }}

  return result;
}})()
"#,
        qty = quantity
    )
}

/// A worker asks the harness to complete the order on the page it is looking
/// at. The worker never names a vault item or touches any credential API; it
/// states the charge and the harness resolves the rest. This is the seam that
/// keeps the model out of the credential loop: the worker's action is "complete
/// my order" and the harness's action is "apply the vault item's fields."
///
/// When `url` is provided, the harness drives the entire flow: it starts a
/// browser, navigates to the URL, walks through the checkout (add to cart,
/// proceed to checkout), fills all fields from the vault, and optionally
/// submits. The worker only supplies intent (amount, merchant, purpose) and
/// never touches a form. This is the Rye/Pattern-2 approach: the model is
/// completely out of the browser loop for purchases.
///
/// Resolves the FIRST active vault item (there is typically one card). If
/// multiple items exist, `item_name` narrows it.
async fn complete_order(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let home = amux_home();
    let session = body["session"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| origin_name(&headers));
    let amount = body["amount_usd"].as_f64().unwrap_or(f64::NAN);
    let currency = body["currency"].as_str().unwrap_or("USD").to_string();
    let merchant = body["merchant"].as_str().unwrap_or("").trim().to_string();
    let purpose = body["purpose"].as_str().unwrap_or("").trim().to_string();
    let item_name = body["item_name"].as_str().unwrap_or("").trim().to_lowercase();
    let url = body["url"].as_str().unwrap_or("").trim().to_string();
    let quantity = body["quantity"].as_u64().unwrap_or(1).max(1);
    let auto_submit = body["auto_submit"].as_bool().unwrap_or(false);
    let profile = body["profile"].as_str().unwrap_or("default").to_string();
    if merchant.is_empty() || purpose.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            json!({"error": "merchant and purpose are required"}),
        );
    }
    let items = load_items(&home);
    let item = if item_name.is_empty() {
        items
            .iter()
            .find(|i| i["status"].as_str() == Some("active") && i["kind"].as_str() != Some("login"))
    } else {
        items.iter().find(|i| {
            i["status"].as_str() == Some("active")
                && i["kind"].as_str() != Some("login")
                && i["name"]
                    .as_str()
                    .is_some_and(|n| n.to_lowercase().contains(&item_name))
        })
    };
    let Some(item) = item.cloned() else {
        return err(
            StatusCode::NOT_FOUND,
            json!({"error": "no active vault item found",
                   "hint": "the owner activates items from the dashboard"}),
        );
    };
    let id = item["id"].as_str().unwrap_or("").to_string();
    let base_audit = json!({"ts": now_f64(), "event": "complete_order", "item": id,
        "session": session, "amount_usd": amount, "currency": currency,
        "merchant": merchant, "purpose": purpose, "url": url});
    let spend = match gate_charge(&home, &id, &item, &session, amount, &currency, &merchant, &purpose, "complete_order", &base_audit) {
        Ok(s) => s,
        Err(r) => return *r,
    };

    // If a URL is provided, drive the full checkout flow server-side
    let cdp_result = if !url.is_empty() {
        checkout_with_url(&home, &session, &url, &profile, quantity).await
    } else {
        // Legacy path: connect to existing browser session
        match super::browser::connect_session(&session, None).await {
            Ok((_page, cdp)) => Ok(cdp),
            Err(_) => Err(format!(
                "no browser page for session '{session}': pass a 'url' to let the harness \
                 drive the browser, or navigate to the checkout page first"
            )),
        }
    };

    let mut cdp = match cdp_result {
        Ok(cdp) => cdp,
        Err(e) => {
            let mut a = base_audit;
            a["decision"] = json!("browser_failed");
            a["why"] = json!(e);
            a["spend"] = json!(spend);
            a["spend_released"] = json!(true);
            super::vault_spend::release(&home, &spend, "no browser page: the card never reached a page");
            audit(&home, a);
            return err(StatusCode::BAD_GATEWAY, json!({"ok": false, "error": e}));
        }
    };

    let f = &item["fields"];
    let yy: String = f["exp_year"]
        .as_str()
        .unwrap_or("")
        .chars()
        .rev()
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let values = json!({
        "number": f["number"],
        "exp": format!("{}/{}", f["exp_month"].as_str().unwrap_or(""), yy),
        "exp_month": f["exp_month"], "exp_year": f["exp_year"],
        "cvc": f["cvc"], "name": f["name"],
        "address": f["address"], "city": f["city"], "state": f["state"],
        "zip": f["zip"], "country": f["country"], "email": f["email"],
    });
    let fill_result = fill_with_frames(&mut cdp, &values, &payment_frame_hosts())
        .await
        .map_err(|e| e.to_string());

    // Optionally click submit/place-order
    let fill_complete = fill_result.as_ref().map(|r| fill_outcome(r).complete).unwrap_or(false);
    let submit_result = if auto_submit {
        if fill_complete {
            let submit_js = r#"(async () => {
  const sleep = ms => new Promise(r => setTimeout(r, ms));
  const btn = document.querySelector(
    '#place_order, button[name="woocommerce_checkout_place_order"], ' +
    'input[type="submit"][name="woocommerce_checkout_place_order"], ' +
    'button[type="submit"][class*="place-order"], ' +
    'button[type="submit"][class*="checkout"]'
  ) || [...document.querySelectorAll('button[type="submit"]')].find(
    b => /place order|complete|pay now|submit order|confirm/i.test(b.textContent)
  );
  if (!btn) return { submitted: false, reason: "no submit button found" };
  btn.click();
  await sleep(2000);
  return { submitted: true, url: location.href };
})()"#;
            cdp.eval(submit_js, 30).await.ok()
        } else {
            None
        }
    } else {
        None
    };

    let mut a = base_audit;
    a["spend"] = json!(spend);
    match fill_result {
        Ok(r) => {
            let o = fill_outcome(&r);
            release_if_nothing_typed(&home, &spend, &o, &mut a);
            a["decision"] = json!(if o.complete { "filled" } else { "fill_incomplete" });
            a["filled"] = o.filled.clone();
            a["not_found"] = o.not_found.clone();
            a["mismatched"] = o.mismatched.clone();
            a["page"] = r.get("url").cloned().unwrap_or(Value::Null);
            if let Some(ref sr) = submit_result {
                a["submit"] = sr.clone();
            }
            audit(&home, a);
            if !o.complete {
                tracing::warn!(item = %id, session = %session, amount, merchant = %merchant,
                    filled = %o.filled, not_found = %o.not_found, mismatched = %o.mismatched,
                    auto_submit, measured = true, n_considered = 1, verdict = "order_fill_incomplete",
                    "order form NOT filled by the harness; nothing was submitted (values not logged)");
                return err(StatusCode::UNPROCESSABLE_ENTITY, json!({"ok": false, "error": o.why,
                    "filled": o.filled, "not_found": o.not_found, "mismatched": o.mismatched,
                    "payment_iframes": r.get("payment_iframes"), "frames": r.get("frames"),
                    "frames_skipped": r.get("frames_skipped"), "page": r.get("url"),
                    "submitted": false}));
            }
            tracing::info!(item = %id, session = %session, amount, merchant = %merchant,
                measured = true, n_considered = 1, verdict = "order_completed",
                "order form filled by the harness (values not logged)");
            let mut resp = json!({"ok": true, "filled": o.filled, "not_found": o.not_found,
                "payment_iframes": r.get("payment_iframes"),
                "note": "the harness filled the order form. Values are not returned."});
            if let Some(sr) = submit_result {
                resp["submit"] = sr;
            }
            Json(resp).into_response()
        }
        Err(e) => {
            a["decision"] = json!("fill_failed");
            a["why"] = json!(e);
            audit(&home, a);
            err(StatusCode::BAD_GATEWAY, json!({"ok": false, "error": e}))
        }
    }
}

/// Drive the full checkout flow: start browser, navigate, walk through
/// add-to-cart and checkout pages, and return a connected CDP client on the
/// checkout page ready for payment field filling.
async fn checkout_with_url(
    home: &Path,
    session: &str,
    url: &str,
    profile: &str,
    quantity: u64,
) -> Result<crate::integrations::browser::CdpClient, String> {
    use crate::integrations::browser as chrome;

    // Start a browser on this profile if one isn't already running
    if chrome::port_for_session(session).is_none() {
        chrome::start(home, profile, url, session, session, true)
            .await
            .map_err(|e| format!("failed to start browser: {e}"))?;
    }

    // Connect to the browser
    let page = chrome::resolve_page(session, Some(url))
        .await
        .map_err(|e| format!("failed to resolve browser page: {e:?}"))?;
    let mut cdp = chrome::CdpClient::connect(&page.ws_url)
        .await
        .map_err(|e| format!("failed to connect CDP: {e}"))?;

    // Navigate to the URL if not already there
    let current = cdp
        .eval("location.href", 5)
        .await
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    if !current.contains(url.split('/').nth(2).unwrap_or("")) {
        cdp.call(
            "Page.navigate",
            json!({"url": url}),
            std::time::Duration::from_secs(30),
        )
        .await
        .map_err(|e| format!("navigation failed: {e}"))?;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }

    // Run the checkout navigation script
    let nav_result = cdp
        .eval(&checkout_navigate_script(quantity), 30)
        .await
        .map_err(|e| format!("checkout navigation failed: {e}"))?;

    tracing::info!(
        session,
        url,
        nav_result = %nav_result,
        measured = true,
        n_considered = 1,
        verdict = "checkout_navigated",
        "checkout flow navigated by the harness"
    );

    // Wait for the checkout page to settle
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    Ok(cdp)
}

pub fn routes() -> Router<super::AppState> {
    Router::new()
        .route("/api/vault", get(list).post(create))
        .route("/api/vault/{id}", axum::routing::patch(update).delete(remove))
        .route("/api/vault/{id}/fill", post(fill))
        .route("/api/browser/complete-order", post(complete_order))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payment_frames_are_only_the_providers() {
        let a: Vec<String> = PAYMENT_FRAME_HOSTS.iter().map(|h| h.to_string()).collect();
        let host = |u: &str| payment_frame_host(u, &a);
        assert_eq!(host("https://js.stripe.com/v3/elements-inner-card.html#x").as_deref(), Some("js.stripe.com"));
        assert_eq!(host("https://assets.braintreegateway.com/web/3.97/html/hosted-fields-frame.min.html").as_deref(),
            Some("assets.braintreegateway.com"));
        assert_eq!(host("https://checkoutshopper-live.adyen.com:443/x").as_deref(), Some("checkoutshopper-live.adyen.com"));
        // Lookalikes and everyone else get nothing.
        assert_eq!(host("https://js.stripe.com.evil.example/x"), None);
        assert_eq!(host("https://evilstripe.com/x"), None);
        assert_eq!(host("https://user@evil.example/js.stripe.com"), None);
        assert_eq!(host("https://ads.example.com/frame"), None);
        assert_eq!(host("about:blank"), None);
    }

    #[test]
    fn frame_fills_merge_into_one_outcome() {
        let top = json!({"filled": ["email"], "not_found": ["number", "exp", "cvc", "zip"], "mismatched": [], "url": "u"});
        let frames = vec![
            ("js.stripe.com".to_string(), json!({"filled": ["number", "exp"], "not_found": ["cvc", "zip"], "mismatched": []})),
            ("js.stripe.com".to_string(), json!({"filled": ["cvc"], "not_found": [], "mismatched": [{"key": "number"}]})),
        ];
        let m = merge_frame_fills(&top, &frames, &["https://ads.example".into()]);
        assert_eq!(m["filled"], json!(["email", "number", "exp", "cvc"]));
        // A mismatch in one frame does not count when another frame filled the key.
        assert_eq!(m["mismatched"], json!([]));
        assert_eq!(m["not_found"], json!(["zip"]));
        assert_eq!(m["frames_skipped"], json!(["https://ads.example"]));
        assert!(fill_outcome(&m).complete);
        let none = merge_frame_fills(&json!({"filled": [], "not_found": ["number"], "mismatched": []}), &[], &[]);
        assert!(!fill_outcome(&none).complete);
    }

    /// A real Chromium: a checkout on 127.0.0.1 with a cross-site payment
    /// iframe (localhost, allowed) whose card number keeps only what Chrome
    /// itself types, as hosted fields do, and a cross-site iframe from a host
    /// that is not a payment provider ([::1]), which must receive nothing.
    /// Needs Playwright's headless Chromium (or AMUX_TEST_CHROME); without one
    /// it says so and passes, since CI has none.
    #[tokio::test]
    async fn fills_card_fields_inside_a_cross_origin_payment_iframe() {
        let Some(chrome) = test_chrome() else {
            eprintln!("SKIPPED fills_card_fields_inside_a_cross_origin_payment_iframe: no headless Chromium (set AMUX_TEST_CHROME)");
            return;
        };
        let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let v6 = std::net::TcpListener::bind("[::1]:0").unwrap();
        let (p4, p6) = (v4.local_addr().unwrap().port(), v6.local_addr().unwrap().port());
        let page = format!(r#"<input id="email" type="email"><script>window.__loaded=0</script>
<iframe src="http://localhost:{p4}/pay" onload="window.__loaded++"></iframe>
<iframe src="http://[::1]:{p6}/other" onload="window.__loaded++"></iframe>"#);
        let pay = r#"<input autocomplete="cc-number" id="n"><input autocomplete="cc-exp" id="e"><input autocomplete="cc-csc" id="c">
<script>const n=document.getElementById('n');n.addEventListener('input',e=>{if(!e.isTrusted)n.value='';});
setInterval(()=>{if(n.value&&!n.dataset.t)n.value=''},0);n.addEventListener('beforeinput',e=>{if(e.isTrusted)n.dataset.t=1});</script>"#;
        let other = r#"<input autocomplete="cc-number" id="n"><input autocomplete="cc-csc" id="c">"#;
        for (l, routes) in [(v4, vec![("/page", page.clone()), ("/pay", pay.to_string())]), (v6, vec![("/other", other.to_string())])] {
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                for mut c in l.incoming().flatten() {
                    let mut buf = [0u8; 4096];
                    let n = c.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req.split_whitespace().nth(1).unwrap_or("/");
                    let body = routes.iter().find(|(p, _)| *p == path).map(|(_, b)| b.clone()).unwrap_or_default();
                    let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                }
            });
        }
        let dir = tempfile::tempdir().unwrap();
        let mut child = tokio::process::Command::new(&chrome)
            .args(["--headless", "--site-per-process", "--remote-debugging-port=0", "--no-first-run", "--no-default-browser-check"])
            .arg(format!("--user-data-dir={}", dir.path().display()))
            .arg("about:blank")
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let port = {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(child.stderr.take().unwrap()).lines();
            let mut port = 0u16;
            while let Ok(Ok(Some(l))) = tokio::time::timeout(std::time::Duration::from_secs(20), lines.next_line()).await {
                if let Some(rest) = l.split("ws://127.0.0.1:").nth(1) {
                    port = rest.split('/').next().unwrap().parse().unwrap();
                    break;
                }
            }
            port
        };
        assert!(port > 0, "chrome did not report its DevTools port");
        let tab = crate::integrations::browser::cdp_new_tab(port, "about:blank").await.unwrap();
        let mut cdp = crate::integrations::browser::CdpClient::connect(tab["webSocketDebuggerUrl"].as_str().unwrap()).await.unwrap();
        cdp.call("Page.navigate", json!({"url": format!("http://127.0.0.1:{p4}/page")}), std::time::Duration::from_secs(10)).await.unwrap();
        let mut loaded = false;
        for _ in 0..100 {
            if cdp.eval("window.__loaded", 5).await.ok().and_then(|v| v.as_u64()) == Some(2) { loaded = true; break; }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(loaded, "the checkout and both iframes loaded");
        let values = json!({"number": "4242424242424242", "exp": "03/29", "exp_month": "03", "exp_year": "2029",
            "cvc": "123", "email": "buyer@example.test"});
        let r = fill_with_frames(&mut cdp, &values, &["localhost".to_string()]).await.unwrap();
        let o = fill_outcome(&r);
        assert!(o.complete, "{r}");
        assert_eq!(r["frames"].as_array().map(Vec::len), Some(1), "{r}");
        assert_eq!(r["frames"][0]["host"], "localhost", "{r}");
        assert_eq!(r["frames_skipped"].as_array().map(Vec::len), Some(1), "{r}");
        for k in ["email", "number", "exp", "cvc"] {
            assert!(keys_of(&r["filled"]).contains(&k.to_string()), "{k} not filled: {r}");
        }
        // Read the frames' fields back through their own sessions.
        let ts = cdp.call("Target.getTargets", json!({}), std::time::Duration::from_secs(5)).await.unwrap();
        let mut seen = 0;
        for t in ts["targetInfos"].as_array().unwrap().iter().filter(|t| t["type"] == "iframe") {
            let a = cdp.call("Target.attachToTarget", json!({"targetId": t["targetId"], "flatten": true}),
                std::time::Duration::from_secs(5)).await.unwrap();
            let (sid, url) = (a["sessionId"].as_str().unwrap().to_string(), t["url"].as_str().unwrap_or(""));
            let n = cdp.eval_in(&sid, "document.getElementById('n').value.replace(/\\D/g,'')", 5).await.unwrap();
            if url.contains("localhost") { assert_eq!(n, "4242424242424242", "payment frame"); seen += 1; }
            if url.contains("[::1]") { assert_eq!(n, "", "a non-payment frame got a card number"); seen += 1; }
        }
        assert_eq!(seen, 2, "both frames read back");
        let _ = child.kill().await;
    }

    fn test_chrome() -> Option<std::path::PathBuf> {
        if let Ok(p) = std::env::var("AMUX_TEST_CHROME") {
            return Some(p.into());
        }
        let cache = std::path::PathBuf::from(std::env::var("HOME").ok()?).join("Library/Caches/ms-playwright");
        let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(&cache).ok()?.flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("chromium_headless_shell-"))
            .flat_map(|e| std::fs::read_dir(e.path()).into_iter().flatten().flatten().map(|d| d.path().join("chrome-headless-shell")))
            .filter(|p| p.exists())
            .collect();
        found.sort();
        found.pop()
    }

    #[test]
    fn login_origins_match_host_suffixes_and_nothing_else() {
        let o = vec!["netsuite.com".to_string()];
        assert!(host_allowed("system.netsuite.com", &o));
        assert!(host_allowed("1234567.app.netsuite.com", &o));
        assert!(host_allowed("netsuite.com", &o));
        assert!(!host_allowed("netsuite.com.evil.example", &o));
        assert!(!host_allowed("evilnetsuite.com", &o));
        assert!(!host_allowed("example.com", &o));
        assert!(host_allowed("login.oracle.com", &["https://*.oracle.com/".to_string()]));
        assert!(!host_allowed("anything.com", &[]));
    }

    #[test]
    fn login_view_never_carries_the_password() {
        let item = json!({"id": "vlt_x", "name": "NetSuite", "kind": "login",
            "fields": {"username": "ethan@runmixpeek.com", "password": "hunter2-secret"},
            "rules": {"origins": ["netsuite.com"]}, "status": "active"});
        let v = login_public_view(&item).to_string();
        assert!(!v.contains("hunter2-secret"), "{v}");
        assert!(!v.contains("ethan@"), "username is masked: {v}");
        assert!(v.contains("e***@runmixpeek.com"), "{v}");
        let routed = public_view(&item).to_string();
        assert!(!routed.contains("hunter2-secret"), "public_view must route logins to the login view: {routed}");
    }

    #[test]
    fn the_fill_script_carries_the_values_at_its_one_placeholder() {
        assert_eq!(FILL_SCRIPT_JS.matches("__AMUX_FILL_VALUES__").count(), 1,
            "a second mention (a comment) would take the substitution instead of the code");
        let s = fill_script(&json!({"number": "4242", "name": "a\"b"}));
        assert!(!s.contains("__AMUX_FILL_VALUES__"));
        assert!(s.contains(r#"const v = {"name":"a\"b","number":"4242"};"#) || s.contains(r#"const v = {"number":"4242","name":"a\"b"};"#), "{}", &s[..400.min(s.len())]);
    }

    #[test]
    fn an_empty_or_mismatched_fill_is_not_a_filled_form() {
        let empty = fill_outcome(&json!({"filled": [], "not_found": ["number"], "mismatched": []}));
        assert!(!empty.complete);
        assert!(empty.why.contains("not filled"), "{}", empty.why);
        let bad = fill_outcome(&json!({"filled": ["name"], "not_found": [], "mismatched": [{"key": "number"}, {"key": "exp_month"}]}));
        assert!(!bad.complete);
        assert!(bad.why.contains("number, exp_month"), "{}", bad.why);
        let missing = fill_outcome(&json!({"url": "x"}));
        assert!(!missing.complete, "a result without fields is not a fill");
        let ok = fill_outcome(&json!({"filled": ["number", "cvc"], "not_found": ["email"], "mismatched": []}));
        assert!(ok.complete && ok.why.is_empty());
    }

    #[test]
    fn login_fill_script_json_escapes_values() {
        let s = login_fill_script("a\"b", "p'\"</script>", true);
        assert!(s.contains(r#"const U = "a\"b""#), "{s}");
        assert!(s.contains("SUBMIT = true"));
    }

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
