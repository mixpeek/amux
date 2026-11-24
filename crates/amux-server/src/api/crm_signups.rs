//! amux Cloud signups into the CRM (owner 2026-11-24: "ensure they're all tested
//! in an actual gtm cycle for amux cloud signup"). Until this, a signup lived only
//! in Stripe and Hetzner, so no worker could see one and nothing in the CRM ran on it.
//!
//! The signup function (cloud/start/worker.mjs) answers GET /api/signups with
//! every order, behind its ops key. The crm-watch job pulls it every 10 minutes
//! (AMUX_CRM_SIGNUPS_SECS) from each place named in AMUX_CRM_SIGNUPS, a comma list
//! of mode=url (live=https://cloud.amux.io,test=https://amux-cloud-test.<sub>.workers.dev),
//! with the key in ~/.amux/cloud/ops-key-<mode>. Each order becomes, idempotently:
//!   - a person by email (source amux-cloud:<mode>, tags amux-cloud and, in test mode, test),
//!   - the company of the email's domain (none for free mail), linked to the person,
//!   - a deal "amux Cloud <slug>" keyed by its Stripe subscription, its stage
//!     following the subscription (trial, won, lost, lead while unpaid),
//!   - one activity line per status the order reaches,
//!   - a crm.signup event (change new or status), so a GTM worker subscribes to it
//!     and plans the onboarding touch, which the owner approves like any other.
//!
//! Verdicts: crm_signups_imported, crm_signups_fetch_failed, crm_signups_unreadable.

use super::crm_accounts::{ev, now_secs};
use super::AppState;
use crate::db::board_store::next_issue_id;
use crate::db::WriteOutcome;
use amux_core::revision::MutationKind;
use serde_json::{json, Value};
use std::sync::Mutex;

const FREE_MAIL: &[&str] = &[
    "gmail.com", "googlemail.com", "outlook.com", "hotmail.com", "live.com", "msn.com", "yahoo.com", "icloud.com", "me.com",
    "mac.com", "aol.com", "proton.me", "protonmail.com", "pm.me", "hey.com", "gmx.com", "gmx.de", "yandex.com", "zoho.com",
    "fastmail.com", "qq.com", "163.com",
];

pub(crate) fn is_free_mail(domain: &str) -> bool {
    FREE_MAIL.contains(&domain)
}

/// A Stripe subscription status as a deal stage.
fn stage_of(status: &str) -> &'static str {
    match status {
        "trialing" => "trial",
        "active" | "past_due" => "won",
        "canceled" | "incomplete_expired" | "unpaid" => "lost",
        _ => "lead",
    }
}

/// One pull's rows into the CRM. Returns the crm.signup events to emit.
pub(crate) fn import(conn: &rusqlite::Connection, mode: &str, rows: &[Value], now: i64) -> rusqlite::Result<Vec<Value>> {
    let mut events = Vec::new();
    let source = format!("amux-cloud:{mode}");
    for r in rows {
        let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
        let (sub, slug, email, status) = (s("sub"), s("slug"), s("email").to_lowercase(), s("status"));
        if sub.is_empty() || !email.contains('@') {
            continue;
        }
        let stage = stage_of(&status);
        let url = s("url");
        let domain = email.rsplit('@').next().unwrap_or("").to_string();

        // The company: by domain, made when missing; none for free mail.
        let mut account = (String::new(), String::new());
        if !domain.is_empty() && !FREE_MAIL.contains(&domain.as_str()) {
            let hit: Option<(String, String)> = conn
                .query_row("SELECT id,name FROM crm_accounts WHERE lower(domain)=?1 AND deleted IS NULL", [&domain], |x| Ok((x.get(0)?, x.get(1)?)))
                .ok();
            account = match hit {
                Some(a) => a,
                None => {
                    let id = next_issue_id(conn, "ACCT")?;
                    conn.execute(
                        "INSERT INTO crm_accounts (id,name,domain,website,source,created,updated) VALUES (?1,?2,?2,?3,?4,?5,?5)",
                        rusqlite::params![&id, &domain, format!("https://{domain}"), &source, now],
                    )?;
                    (id, domain.clone())
                }
            };
        }

        // The person: by email, made when missing.
        let found: Option<(String, String, String, String)> = conn
            .query_row(
                "SELECT id,name,status,account_id FROM crm_contacts WHERE lower(email)=?1 AND deleted IS NULL",
                [&email],
                |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?)),
            )
            .ok();
        let person_status = if stage == "won" || stage == "trial" { "customer" } else { "lead" };
        let external = json!({ "stripe_sub": sub, "cloud_slug": slug, "cloud_url": url, "cloud_mode": mode }).to_string();
        let (cid, name) = match found {
            Some((id, name, st, acct)) => {
                // Never downgrade a status the owner set; fill what is empty.
                let st2 = if st.is_empty() || st == "lead" { person_status.to_string() } else { st };
                let acct2 = if acct.is_empty() { account.0.clone() } else { acct };
                conn.execute("UPDATE crm_contacts SET status=?2, account_id=?3, updated=?4 WHERE id=?1", rusqlite::params![&id, &st2, &acct2, now])?;
                (id, name)
            }
            None => {
                let id = next_issue_id(conn, "PPL")?;
                let name = email.split('@').next().unwrap_or(&email).split('+').next().unwrap_or("").to_string();
                let name = if name.is_empty() { email.clone() } else { name };
                conn.execute(
                    "INSERT INTO crm_contacts (id,name,company,email,notes,created,updated,account_id,status,source,external) \
                     VALUES (?1,?2,?3,?4,?5,?6,?6,?7,?8,?9,?10)",
                    rusqlite::params![&id, &name, &account.1, &email, format!("amux Cloud server {url}"), now, &account.0, person_status, &source, &external],
                )?;
                (id, name)
            }
        };
        for t in ["amux-cloud"].into_iter().chain((mode == "test").then_some("test")) {
            conn.execute("INSERT OR IGNORE INTO crm_tags (contact_id, tag) VALUES (?1, ?2)", [&cid, t])?;
        }

        // The deal: one per subscription.
        let deal: Option<(String, String)> = conn
            .query_row(
                "SELECT id,stage FROM crm_deals WHERE json_extract(external,'$.stripe_sub')=?1 AND deleted IS NULL",
                [&sub],
                |x| Ok((x.get(0)?, x.get(1)?)),
            )
            .ok();
        let amount = r.get("amount_usd").and_then(Value::as_f64);
        let (did, change, previous) = match deal {
            Some((id, prev)) => {
                if prev != stage {
                    conn.execute("UPDATE crm_deals SET stage=?2, amount=COALESCE(?3,amount), updated=?4 WHERE id=?1", rusqlite::params![&id, stage, amount, now])?;
                }
                (id, "status", prev)
            }
            None => {
                let id = next_issue_id(conn, "DEAL")?;
                conn.execute(
                    "INSERT INTO crm_deals (id,title,account_id,contact_ids,stage,amount,next_step,notes,source,external,created,updated) \
                     VALUES (?1,?2,?3,?4,?5,?6,'onboard: plan the welcome touch',?7,?8,?9,?10,?10)",
                    rusqlite::params![&id, format!("amux Cloud {slug}"), &account.0, json!([cid]).to_string(), stage, amount,
                        format!("plan {} via {}", s("plan"), s("source")), &source, &external, now],
                )?;
                (id, "new", String::new())
            }
        };

        // One activity line per status the order reaches; a repeat is ignored,
        // which is what makes a pull idempotent.
        let added = conn.execute(
            "INSERT OR IGNORE INTO crm_interactions (id,contact_id,date,type,notes,created,updated,direction,subject,message_id,deal_id) \
             VALUES (?1,?2,?3,'other',?4,?5,?5,'in',?6,?7,?8)",
            rusqlite::params![ulid::Ulid::new().to_string(), &cid, chrono::Local::now().format("%Y-%m-%d").to_string(),
                format!("{url} plan {} status {status}", s("plan")), now,
                if change == "new" { format!("signed up for amux Cloud ({})", s("plan")) } else { format!("amux Cloud subscription {status}") },
                format!("cloud:{sub}:{status}"), &did],
        )?;
        if added > 0 {
            events.push(json!({
                "contact_id": cid, "name": name, "email": email, "account_id": account.0, "account_name": account.1,
                "deal_id": did, "stage": stage, "previous_stage": previous, "change": change, "status": status,
                "slug": slug, "url": url, "plan": s("plan"), "amount_usd": amount, "mode": mode, "source": s("source"),
                "ready": r.get("ready").cloned().unwrap_or(Value::Null), "activation": s("activation"),
            }));
        }
    }
    Ok(events)
}

static EVENTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());

/// Each configured place, if due: fetch, import, emit.
pub(crate) async fn pull(state: &AppState) {
    let every = crate::config::env_i64("AMUX_CRM_SIGNUPS_SECS", 600);
    let places = std::env::var("AMUX_CRM_SIGNUPS").unwrap_or_default();
    if every <= 0 || places.trim().is_empty() {
        return;
    }
    static LAST: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let now = now_secs();
    if now - LAST.load(std::sync::atomic::Ordering::Relaxed) < every {
        return;
    }
    LAST.store(now, std::sync::atomic::Ordering::Relaxed);
    let Ok(client) = reqwest::Client::builder().timeout(std::time::Duration::from_secs(60)).build() else { return };
    for place in places.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (mode, base) = place.split_once('=').unwrap_or(("live", place));
        let (mode, base) = (mode.trim().to_string(), base.trim().trim_end_matches('/').to_string());
        let key = std::env::var(format!("AMUX_CRM_SIGNUPS_KEY_{}", mode.to_uppercase()))
            .ok()
            .or_else(|| std::fs::read_to_string(crate::config::amux_home().join("cloud").join(format!("ops-key-{mode}"))).ok())
            .map(|k| k.trim().to_string())
            .unwrap_or_default();
        let got = async {
            if key.is_empty() {
                anyhow::bail!("no ops key: AMUX_CRM_SIGNUPS_KEY_{} or ~/.amux/cloud/ops-key-{mode}", mode.to_uppercase());
            }
            let r = client.get(format!("{base}/api/signups")).header("x-ops-key", &key).send().await?;
            if !r.status().is_success() {
                anyhow::bail!("HTTP {}", r.status());
            }
            let v: Value = r.json().await?;
            Ok::<_, anyhow::Error>(v.get("signups").and_then(Value::as_array).cloned().unwrap_or_default())
        }
        .await;
        let rows = match got {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(target: "amux::crm", mode = %mode, url = %base, error = %e, measured = false, n_considered = 0,
                    verdict = "crm_signups_fetch_failed", "amux Cloud signups could not be read");
                continue;
            }
        };
        let n = rows.len();
        let m = mode.clone();
        let res = state
            .store
            .write_async(move |conn| {
                let evs = import(conn, &m, &rows, now_secs())?;
                let applied = !evs.is_empty();
                let pending = evs.iter().flat_map(|e| {
                    [ev("crm_contact", e["contact_id"].as_str().unwrap_or(""), MutationKind::Updated), ev("crm_deal", e["deal_id"].as_str().unwrap_or(""), MutationKind::Updated)]
                }).collect();
                *EVENTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = evs;
                Ok(WriteOutcome { applied, events: pending })
            })
            .await;
        if let Err(e) = res {
            tracing::warn!(target: "amux::crm", mode = %mode, error = %e, measured = false, n_considered = n, verdict = "crm_signups_unreadable", "signups could not be written");
            continue;
        }
        let evs = std::mem::take(&mut *EVENTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        tracing::info!(target: "amux::crm", mode = %mode, changed = evs.len(), measured = true, n_considered = n, verdict = "crm_signups_imported",
            "amux Cloud signups read into the CRM");
        for e in evs {
            super::crm_agent::emit(state, "crm.signup", e).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(sub: &str, email: &str, status: &str) -> Value {
        json!({ "sub": sub, "slug": "acme", "email": email, "plan": "solo", "status": status, "finished": true,
                "url": "https://acme.amux.io", "amount_usd": 29.0, "source": "start" })
    }

    #[test]
    fn a_signup_becomes_a_person_a_company_and_a_deal_once_and_follows_its_status() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate::apply_all(&mut conn).unwrap();
        let evs = import(&conn, "live", &[row("sub_1", "Ada@Acme.io", "trialing")], 1).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!((evs[0]["change"].as_str(), evs[0]["stage"].as_str(), evs[0]["account_name"].as_str()), (Some("new"), Some("trial"), Some("acme.io")));
        let (name, status, acct, src): (String, String, String, String) = conn
            .query_row("SELECT name,status,account_id,source FROM crm_contacts WHERE email='ada@acme.io'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap();
        assert_eq!((name.as_str(), status.as_str(), src.as_str()), ("ada", "customer", "amux-cloud:live"));
        assert!(!acct.is_empty(), "linked to the company");

        assert!(import(&conn, "live", &[row("sub_1", "ada@acme.io", "trialing")], 2).unwrap().is_empty(), "a repeat pull changes nothing");
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM crm_deals", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);

        let evs = import(&conn, "live", &[row("sub_1", "ada@acme.io", "canceled")], 3).unwrap();
        assert_eq!((evs[0]["change"].as_str(), evs[0]["stage"].as_str(), evs[0]["previous_stage"].as_str()), (Some("status"), Some("lost"), Some("trial")));
        let ix: i64 = conn.query_row("SELECT COUNT(*) FROM crm_interactions WHERE message_id LIKE 'cloud:sub_1:%'", [], |r| r.get(0)).unwrap();
        assert_eq!(ix, 2, "one activity line per status");
    }

    #[test]
    fn free_mail_makes_no_company_and_an_owner_status_is_kept_and_test_orders_are_tagged() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate::apply_all(&mut conn).unwrap();
        conn.execute("INSERT INTO crm_contacts (id,name,email,status,created,updated) VALUES ('PPL-9','Bo','bo@gmail.com','partner',1,1)", []).unwrap();
        let evs = import(&conn, "test", &[row("sub_2", "bo@gmail.com", "active")], 1).unwrap();
        assert_eq!((evs[0]["contact_id"].as_str(), evs[0]["account_id"].as_str()), (Some("PPL-9"), Some("")), "the existing person, no company");
        let st: String = conn.query_row("SELECT status FROM crm_contacts WHERE id='PPL-9'", [], |r| r.get(0)).unwrap();
        assert_eq!(st, "partner", "a status the owner set is not overwritten");
        let tags: Vec<String> = conn.prepare("SELECT tag FROM crm_tags WHERE contact_id='PPL-9' ORDER BY tag").unwrap()
            .query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        assert_eq!(tags, vec!["amux-cloud", "test"]);
        assert!(import(&conn, "test", &[json!({"sub": "", "email": "x@y.io"}), json!({"sub": "s", "email": "nope"})], 2).unwrap().is_empty(), "rows without a sub or email are skipped");
    }
}
