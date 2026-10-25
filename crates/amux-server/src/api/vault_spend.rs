//! Spending limits for vault payment methods (Ethan 2026-10-18: "each one have
//! a spending limit (monthly, daily, yearly) that we need to make damn sure
//! amux adheres to").
//!
//! # The rule
//!
//! A card may carry `rules.spend_limits_usd = {daily, monthly, yearly}`, each
//! optional. Every charge amux lets through is written to a ledger
//! (`~/.amux/vault/spend-ledger.jsonl`): item, amount, merchant, purpose,
//! session, time. Before a charge is let through, the item's spend in the
//! current local calendar day, month and year is summed from that ledger, the
//! new amount is added, and if any configured limit would be exceeded the
//! charge is REFUSED. A limit is a hard cap: an owner approval (grants.rs)
//! lifts the per-charge approval threshold and never a spending limit. The
//! owner raises a limit in the Vault tab instead.
//!
//! # Serialised
//!
//! The check and the write are one critical section: an in-process mutex plus
//! an exclusive `flock` on `vault/spend.lock`, so two concurrent orders (or two
//! server processes on one home) cannot both read "under the limit" and both
//! record. `reserve` is the only writer of a `spend` line.
//!
//! # Released spend
//!
//! A reservation is released only when the card values provably never reached
//! a page: no browser to connect to, or a fill that typed no field. Anything
//! else stays counted, because amux cannot see whether the checkout was
//! submitted afterwards, and over-counting is the safe direction.
//!
//! No card value is ever written here: the ledger holds amounts and names.

use crate::config::now_f64;
use chrono::{Datelike, Local, TimeZone};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) const WINDOWS: [&str; 3] = ["daily", "monthly", "yearly"];

fn ledger_path(home: &Path) -> PathBuf {
    super::vault::vault_dir(home).join("spend-ledger.jsonl")
}

fn lock_path(home: &Path) -> PathBuf {
    super::vault::vault_dir(home).join("spend.lock")
}

fn cents(usd: f64) -> i64 {
    (usd * 100.0).round() as i64
}

/// The configured limits, as (window, cents). A missing, null or negative
/// value is "no limit for that window".
pub(crate) fn limits(item: &Value) -> Vec<(&'static str, i64)> {
    let l = &item["rules"]["spend_limits_usd"];
    WINDOWS
        .iter()
        .filter_map(|w| l[*w].as_f64().filter(|v| v.is_finite() && *v >= 0.0).map(|v| (*w, cents(v))))
        .collect()
}

/// Validate a `spend_limits_usd` object from a request. Each window is a
/// number >= 0 (set), null or "" (clear) or absent (unchanged). Returns the
/// merged object.
pub(crate) fn merge_limits(current: &Value, patch: &Value) -> Result<Value, String> {
    let mut out = current.as_object().cloned().unwrap_or_default();
    let Some(p) = patch.as_object() else {
        return Err("spend_limits_usd must be an object like {\"daily\": 50, \"monthly\": 500, \"yearly\": 2000}".into());
    };
    for (k, v) in p {
        if !WINDOWS.contains(&k.as_str()) {
            return Err(format!("unknown spending limit window '{k}': use daily, monthly or yearly"));
        }
        match v {
            Value::Null => {
                out.remove(k);
            }
            Value::String(s) if s.trim().is_empty() => {
                out.remove(k);
            }
            _ => {
                let n = v
                    .as_f64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
                    .ok_or_else(|| format!("the {k} limit must be a number of USD"))?;
                if !n.is_finite() || n < 0.0 {
                    return Err(format!("the {k} limit must be 0 or more"));
                }
                out.insert(k.clone(), json!((n * 100.0).round() / 100.0));
            }
        }
    }
    Ok(Value::Object(out))
}

/// Start of the current local day, month and year, and the start of the next
/// one (when the window resets), as epoch seconds.
pub(crate) fn window_bounds(window: &str, now: f64) -> (f64, f64) {
    let t = Local.timestamp_opt(now as i64, 0).single().unwrap_or_else(Local::now);
    let date = t.date_naive();
    let (start, next) = match window {
        "daily" => (date, date.succ_opt().unwrap_or(date)),
        "monthly" => {
            let s = date.with_day(1).unwrap_or(date);
            let n = if s.month() == 12 {
                s.with_year(s.year() + 1).and_then(|d| d.with_month(1))
            } else {
                s.with_month(s.month() + 1)
            };
            (s, n.unwrap_or(s))
        }
        _ => {
            let s = date.with_ordinal(1).unwrap_or(date);
            (s, s.with_year(s.year() + 1).unwrap_or(s))
        }
    };
    let ts = |d: chrono::NaiveDate| {
        let midnight = d.and_hms_opt(0, 0, 0).expect("midnight exists");
        Local.from_local_datetime(&midnight).earliest().map(|x| x.timestamp() as f64).unwrap_or(0.0)
    };
    (ts(start), ts(next))
}

fn read_ledger(home: &Path) -> Vec<Value> {
    std::fs::read_to_string(ledger_path(home))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

/// Cents spent on `item` at or after `since`, released reservations excluded.
fn spent_since(ledger: &[Value], item: &str, since: f64) -> i64 {
    let released: std::collections::HashSet<&str> = ledger
        .iter()
        .filter(|e| e["ev"] == "release")
        .filter_map(|e| e["id"].as_str())
        .collect();
    ledger
        .iter()
        .filter(|e| e["ev"] == "spend" && e["item"].as_str() == Some(item))
        .filter(|e| e["ts"].as_f64().unwrap_or(0.0) >= since)
        .filter(|e| !e["id"].as_str().is_some_and(|id| released.contains(id)))
        .map(|e| cents(e["amount_usd"].as_f64().unwrap_or(0.0)))
        .sum()
}

/// What an item has spent in each window right now (for the dashboard).
pub(crate) fn spent_summary(home: &Path, item_id: &str) -> Value {
    let ledger = read_ledger(home);
    let now = now_f64();
    let mut out = serde_json::Map::new();
    for w in WINDOWS {
        let (start, _) = window_bounds(w, now);
        out.insert(w.to_string(), json!(spent_since(&ledger, item_id, start) as f64 / 100.0));
    }
    Value::Object(out)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Breach {
    pub window: &'static str,
    pub limit_usd: f64,
    pub spent_usd: f64,
    pub amount_usd: f64,
    pub resets_at: f64,
}

impl Breach {
    pub(crate) fn body(&self, item: &str) -> Value {
        let remaining = ((self.limit_usd - self.spent_usd) * 100.0).round().max(0.0) / 100.0;
        json!({
            "ok": false, "limit_exceeded": true, "item": item, "window": self.window,
            "limit_usd": self.limit_usd, "spent_usd": self.spent_usd, "amount_usd": self.amount_usd,
            "remaining_usd": remaining, "resets_at": self.resets_at,
            "error": format!("refused: a charge of ${:.2} would take this card's {} spend to ${:.2}, over its ${:.2} {} limit (${:.2} left)",
                self.amount_usd, self.window, self.spent_usd + self.amount_usd, self.limit_usd, self.window, remaining),
            "next": "nothing was filled. The owner can raise this card's limit in the Vault (Payment Methods); an approval does not lift a spending limit",
        })
    }
}

/// The first configured window this charge would exceed, given `ledger`.
fn first_breach(ledger: &[Value], item: &Value, amount_usd: f64, now: f64) -> Option<Breach> {
    let id = item["id"].as_str().unwrap_or("");
    let amount = cents(amount_usd);
    for (window, limit) in limits(item) {
        let (start, next) = window_bounds(window, now);
        let spent = spent_since(ledger, id, start);
        if spent + amount > limit {
            return Some(Breach {
                window,
                limit_usd: limit as f64 / 100.0,
                spent_usd: spent as f64 / 100.0,
                amount_usd: amount as f64 / 100.0,
                resets_at: next,
            });
        }
    }
    None
}

/// Read-only pre-check, used before an approval is consumed or requested so
/// the owner is never asked to approve a charge a limit will refuse anyway.
/// `reserve` repeats the check under the lock; this one only saves a step.
pub(crate) fn check(home: &Path, item: &Value, amount_usd: f64) -> Option<Breach> {
    first_breach(&read_ledger(home), item, amount_usd, now_f64())
}

struct Held {
    _guard: std::sync::MutexGuard<'static, ()>,
    _file: std::fs::File,
}

fn hold(home: &Path) -> std::io::Result<Held> {
    static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    let guard = L.get_or_init(Default::default).lock().unwrap_or_else(|p| p.into_inner());
    let dir = super::vault::vault_dir(home);
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(lock_path(home))?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: a valid open fd; the lock is released when `file` drops.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(Held { _guard: guard, _file: file })
}

fn append(home: &Path, entry: &Value) -> std::io::Result<()> {
    let path = ledger_path(home);
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    writeln!(f, "{entry}")?;
    f.sync_data()
}

#[derive(Debug)]
pub(crate) enum ReserveError {
    Limit(Breach),
    Io(String),
}

/// What a charge is for, as recorded on its ledger line.
pub(crate) struct Charge<'a> {
    pub amount_usd: f64,
    pub merchant: &'a str,
    pub purpose: &'a str,
    pub session: &'a str,
    pub path: &'a str,
}

/// Check every limit and record the charge, atomically. Returns the ledger id
/// to `release` if the values never reach a page. Every let-through charge is
/// recorded, limits or not, so a limit set later counts what came before it.
pub(crate) fn reserve(home: &Path, item: &Value, c: &Charge) -> Result<String, ReserveError> {
    let _held = hold(home).map_err(|e| ReserveError::Io(e.to_string()))?;
    let now = now_f64();
    if let Some(b) = first_breach(&read_ledger(home), item, c.amount_usd, now) {
        return Err(ReserveError::Limit(b));
    }
    let mut rnd = [0u8; 6];
    {
        use p256::elliptic_curve::rand_core::{OsRng, RngCore};
        OsRng.fill_bytes(&mut rnd);
    }
    let id = format!("spd_{}", hex::encode(rnd));
    let entry = json!({"ev": "spend", "id": id, "ts": now, "item": item["id"], "amount_usd": c.amount_usd,
        "currency": "USD", "merchant": c.merchant, "purpose": c.purpose, "session": c.session, "path": c.path});
    append(home, &entry).map_err(|e| ReserveError::Io(e.to_string()))?;
    Ok(id)
}

/// Un-count a reservation whose card values never reached a page.
pub(crate) fn release(home: &Path, id: &str, why: &str) {
    let r = hold(home).and_then(|_held| append(home, &json!({"ev": "release", "id": id, "ts": now_f64(), "why": why})));
    if let Err(e) = r {
        tracing::warn!(spend = id, error = %e, measured = true, n_considered = 1, verdict = "vault_spend_release_failed",
            "vault: could not release a spend reservation; it stays counted against the card's limits");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, limits: Value) -> Value {
        json!({"id": id, "status": "active", "kind": "card",
            "rules": {"max_usd_without_approval": 1e9, "spend_limits_usd": limits}})
    }

    fn charge(amount: f64) -> Charge<'static> {
        Charge { amount_usd: amount, merchant: "Test", purpose: "test", session: "t", path: "test" }
    }

    #[test]
    fn concurrent_reservations_never_exceed_a_limit() {
        // 40 threads each try to reserve $10 against a $50 daily limit, five
        // rounds over fresh homes. Exactly five must win every round.
        for round in 0..5 {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().to_path_buf();
            let item = card(&format!("vlt_c{round}"), json!({"daily": 50}));
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(40));
            let wins: usize = std::thread::scope(|s| {
                let hs: Vec<_> = (0..40)
                    .map(|_| {
                        let (home, item, barrier) = (home.clone(), item.clone(), barrier.clone());
                        s.spawn(move || {
                            barrier.wait();
                            reserve(&home, &item, &charge(10.0)).is_ok() as usize
                        })
                    })
                    .collect();
                hs.into_iter().map(|h| h.join().unwrap()).sum()
            });
            assert_eq!(wins, 5, "round {round}: {wins} charges of $10 got under a $50 limit");
            let spent = spent_summary(&home, item["id"].as_str().unwrap());
            assert_eq!(spent["daily"], json!(50.0));
        }
    }

    #[test]
    fn limits_merge_set_clear_and_reject() {
        let cur = json!({"daily": 10.0, "monthly": 100.0});
        let m = merge_limits(&cur, &json!({"daily": null, "yearly": "2000", "monthly": 250.556})).unwrap();
        assert_eq!(m, json!({"monthly": 250.56, "yearly": 2000.0}));
        assert!(merge_limits(&cur, &json!({"weekly": 5})).is_err());
        assert!(merge_limits(&cur, &json!({"daily": -1})).is_err());
        assert!(merge_limits(&cur, &json!({"daily": "lots"})).is_err());
        assert!(limits(&json!({"rules": {}})).is_empty(), "no limits configured is no limits");
    }

    #[test]
    fn released_spend_is_not_counted_and_items_are_separate() {
        let dir = tempfile::tempdir().unwrap();
        let a = card("vlt_a", json!({"daily": 20}));
        let b = card("vlt_b", json!({"daily": 20}));
        let id = reserve(dir.path(), &a, &charge(20.0)).unwrap();
        assert!(matches!(reserve(dir.path(), &a, &charge(0.01)), Err(ReserveError::Limit(_))));
        assert!(reserve(dir.path(), &b, &charge(20.0)).is_ok(), "card b has its own budget");
        release(dir.path(), &id, "test");
        assert!(reserve(dir.path(), &a, &charge(20.0)).is_ok(), "a released reservation frees its amount");
    }

    #[test]
    fn windows_are_ordered_and_contain_now() {
        let now = now_f64();
        for w in WINDOWS {
            let (s, n) = window_bounds(w, now);
            assert!(s <= now && now < n, "{w}: {s} <= {now} < {n}");
        }
        let (ds, _) = window_bounds("daily", now);
        let (ms, _) = window_bounds("monthly", now);
        let (ys, _) = window_bounds("yearly", now);
        assert!(ys <= ms && ms <= ds);
    }
}
