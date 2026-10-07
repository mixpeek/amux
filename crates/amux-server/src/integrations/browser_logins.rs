//! Which saved browser profile is signed in where, and which one a worker
//! should use (2026-10-07).
//!
//! # Why
//!
//! 77 profiles, and a worker picking one had only `signed_in_to`, derived
//! from WHICH HOSTS HAD ANY COOKIE. A tracking cookie and a dead session both
//! read as "signed in": mixpeek-finances took `default` to pay a vendor in
//! Brex because it listed brex.com, landed on the login page, and spent four
//! attempts finding out. Nothing told a worker which profile was meant to be
//! used at all, and `start` with no profile picked the grab-bag `default`.
//!
//! # What this module decides
//!
//! - A LOGIN is an unexpired cookie whose name looks like a session or auth
//!   token (`is_auth_cookie`), minus known tracking cookies. `signed_in_to` is
//!   the sites with at least one, and each carries when it expires, so "signed
//!   in" can be checked without launching anything and goes false on its own
//!   when the session lapses.
//! - A profile's ROLE is declared in the registry (`profiles.json` `role`):
//!   `primary` (the default identity), `personal`, `restricted`, `customer`,
//!   `test`, `deprecated`. Unset is `""` (treated like an ordinary profile).
//! - The DEFAULT profile for a worker that names none: the scoped setting
//!   `AMUX_BROWSER_DEFAULT_PROFILE` (worker > group > global), else the
//!   registry's `primary`, else `default` (the behaviour before this).
//! - `resolve_for_site`: the one profile a worker should use for a URL.
//!
//! All of it is pure over data the caller reads, so it is tested without a
//! browser.

use serde::Serialize;
use std::collections::BTreeMap;

/// Seconds between 1601-01-01 (Chrome's cookie epoch) and 1970-01-01.
const CHROME_EPOCH_OFFSET_S: i64 = 11_644_473_600;

/// Registrable site for a cookie host: `.dash.cloudflare.com` ->
/// `cloudflare.com`, `portal.example.co.uk` -> `example.co.uk`. Same rule as
/// the chrome-cdp profile sync, so both sides name sites alike. None for IPs,
/// localhost and single-label hosts.
pub fn site_of(host: &str) -> Option<String> {
    let h = host.trim().trim_start_matches('.').trim_end_matches('.').to_ascii_lowercase();
    if !h.contains('.') || h.contains(':') || h.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let p: Vec<&str> = h.split('.').collect();
    const SECOND_LEVEL: &[&str] = &["co", "com", "org", "net", "gov", "ac", "edu", "ne", "or"];
    if p.len() >= 3 && p[p.len() - 1].len() == 2 && SECOND_LEVEL.contains(&p[p.len() - 2]) {
        return Some(p[p.len() - 3..].join("."));
    }
    Some(p[p.len() - 2..].join("."))
}

/// Does `site` (a registrable domain) fall under `domain` as recorded in the
/// registry (which may be a host like `dash.cloudflare.com` or a site)?
pub fn domain_covers(domain: &str, site: &str) -> bool {
    let d = domain.trim().trim_start_matches('.').to_ascii_lowercase();
    let s = site.to_ascii_lowercase();
    !d.is_empty() && (d == s || d.ends_with(&format!(".{s}")) || s.ends_with(&format!(".{d}")))
}

/// Cookie names that are tracking or bot-management, never a login.
const NOT_AUTH: &[&str] = &[
    "__cf_bm", "_cfuvid", "cf_clearance", "__cflb", "_ga", "_gid", "_gcl_au", "_fbp", "_fbc",
    "__stripe_mid", "__stripe_sid", "m", "ajs_anonymous_id", "ajs_user_id", "_hjsessionuser",
    "_hjsession", "intercom-device-id", "datadome", "aws-waf-token", "nid", "aec", "ar_debug",
];

/// Does this cookie look like it carries a signed-in session? Name-based
/// because a cookie's flags do not distinguish a session from a tracker
/// (Cloudflare's `__cf_bm` is httpOnly and secure). The pattern covers the
/// common spellings: `user_session` (GitHub), `auth_token` (X), `SID` /
/// `__Secure-1PSID` (Google), `li_at` (LinkedIn), `sessionid`, `jwt`, ...
pub fn is_auth_cookie(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if NOT_AUTH.contains(&n.as_str()) || n.starts_with("_ga_") || n.starts_with("_hj") {
        return false;
    }
    const MARKERS: &[&str] = &[
        "sess", "auth", "token", "login", "logged", "sid", "jwt", "remember", "li_at", "access",
        "refresh", "user", "account", "identity", "csrf_state", "_secure-",
    ];
    MARKERS.iter().any(|m| n.contains(m))
}

/// One row of a Chrome `cookies` table, as much as this module needs.
#[derive(Debug, Clone)]
pub struct CookieRow {
    pub host: String,
    pub name: String,
    /// Chrome `expires_utc`: microseconds since 1601, 0 for a session cookie.
    pub expires_utc: i64,
    pub persistent: bool,
}

impl CookieRow {
    /// Unix seconds this cookie expires, None for a session cookie.
    pub fn expires_unix(&self) -> Option<i64> {
        (self.persistent && self.expires_utc > 0).then(|| self.expires_utc / 1_000_000 - CHROME_EPOCH_OFFSET_S)
    }
    pub fn live(&self, now: i64) -> bool {
        self.expires_unix().is_none_or(|e| e > now)
    }
}

/// A site this profile holds a live login for.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SiteLogin {
    pub site: String,
    /// Unix seconds the LAST of its login cookies expires; None when every one
    /// is a session cookie (lives until the browser closes).
    pub expires_at: Option<i64>,
    pub days_left: Option<f64>,
    pub login_cookies: usize,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct JarSummary {
    /// Unexpired cookies. Expired rows Chrome has not purged yet are not
    /// cookies a site will accept, so they are counted separately.
    pub cookies: i64,
    pub cookies_expired: i64,
    /// Sites with a live login, most login cookies first.
    pub logins: Vec<SiteLogin>,
    /// Hosts with any live cookie, most first (the old meaning, for context).
    pub hosts: Vec<String>,
}

impl JarSummary {
    pub fn signed_in_to(&self) -> Vec<String> {
        self.logins.iter().map(|l| l.site.clone()).collect()
    }
    pub fn login_for(&self, site: &str) -> Option<&SiteLogin> {
        self.logins.iter().find(|l| l.site == site)
    }
}

pub fn summarize(rows: &[CookieRow], now: i64) -> JarSummary {
    let mut out = JarSummary::default();
    let mut hosts: BTreeMap<String, usize> = BTreeMap::new();
    let mut sites: BTreeMap<String, (usize, Option<i64>, bool)> = BTreeMap::new();
    for r in rows {
        if !r.live(now) {
            out.cookies_expired += 1;
            continue;
        }
        out.cookies += 1;
        *hosts.entry(r.host.trim_start_matches('.').to_string()).or_default() += 1;
        if !is_auth_cookie(&r.name) {
            continue;
        }
        let Some(site) = site_of(&r.host) else { continue };
        let e = sites.entry(site).or_insert((0, None, false));
        e.0 += 1;
        match r.expires_unix() {
            Some(t) => e.1 = Some(e.1.map_or(t, |p: i64| p.max(t))),
            None => e.2 = true,
        }
    }
    let mut logins: Vec<SiteLogin> = sites
        .into_iter()
        .map(|(site, (n, exp, _session))| SiteLogin {
            site,
            expires_at: exp,
            days_left: exp.map(|e| (((e - now) as f64 / 86_400.0) * 10.0).round() / 10.0),
            login_cookies: n,
        })
        .collect();
    logins.sort_by(|a, b| b.login_cookies.cmp(&a.login_cookies).then(a.site.cmp(&b.site)));
    out.logins = logins;
    let mut hv: Vec<(String, usize)> = hosts.into_iter().collect();
    hv.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out.hosts = hv.into_iter().take(12).map(|(h, _)| h).collect();
    out
}

/// Read a Chrome cookie DB (a COPY: a listing must never disturb a login).
pub fn read_rows(db: &std::path::Path) -> rusqlite::Result<Vec<CookieRow>> {
    let conn = rusqlite::Connection::open(db)?;
    // Older or hand-built jars may lack the expiry columns; a missing column
    // reads as "persistent, no recorded expiry" (live), never as an error.
    let cols: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('cookies')")?
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok)
        .collect();
    let has = |c: &str| cols.iter().any(|x| x == c);
    let sql = format!(
        "SELECT host_key, name, {}, {} FROM cookies",
        if has("expires_utc") { "expires_utc" } else { "0" },
        if has("is_persistent") { "is_persistent" } else { "1" }
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok(CookieRow {
            host: r.get(0)?,
            name: r.get(1)?,
            expires_utc: r.get::<_, i64>(2).unwrap_or(0),
            persistent: r.get::<_, i64>(3).unwrap_or(1) != 0,
        })
    })?;
    rows.collect()
}

// ---- roles and choice -------------------------------------------------------

/// Registry roles, best first for the resolver.
pub const ROLES: &[&str] = &["primary", "personal", "restricted", "customer", "", "test", "deprecated"];

pub fn role_rank(role: &str) -> usize {
    ROLES.iter().position(|r| *r == role).unwrap_or(4)
}

/// What the resolver knows about one profile.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub name: String,
    pub role: String,
    pub domains: Vec<String>,
    pub allowed: bool,
    pub jar: JarSummary,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Choice {
    pub profile: String,
    /// Why this one, in a sentence a worker can act on.
    pub why: String,
    /// The live login on that site, when there is one.
    pub login: Option<SiteLogin>,
    /// False when the chosen profile has no live login for the site: the
    /// worker should expect a sign-in page.
    pub signed_in: bool,
    pub alternatives: Vec<String>,
}

/// The one profile to use for `site`. Order: a profile the worker may use
/// that holds a LIVE login for the site, best role first; then one whose
/// registry lists the site; then the default. Deprecated and test profiles
/// are only chosen when nothing else covers the site.
pub fn resolve_for_site(site: &str, cands: &[Candidate], default_profile: &str) -> Choice {
    let usable: Vec<&Candidate> = cands.iter().filter(|c| c.allowed).collect();
    let mut live: Vec<&Candidate> = usable.iter().copied().filter(|c| c.jar.login_for(site).is_some()).collect();
    live.sort_by(|a, b| {
        role_rank(&a.role).cmp(&role_rank(&b.role)).then_with(|| {
            let ea = a.jar.login_for(site).and_then(|l| l.expires_at).unwrap_or(i64::MAX);
            let eb = b.jar.login_for(site).and_then(|l| l.expires_at).unwrap_or(i64::MAX);
            eb.cmp(&ea)
        })
    });
    let alternatives = |chosen: &str| -> Vec<String> {
        live.iter().map(|c| c.name.clone()).filter(|n| n != chosen).take(5).collect()
    };
    if let Some(best) = live.first() {
        let login = best.jar.login_for(site).cloned();
        return Choice {
            profile: best.name.clone(),
            why: format!(
                "{} holds a live login for {site}{}",
                best.name,
                if best.role.is_empty() { String::new() } else { format!(" (role: {})", best.role) }
            ),
            signed_in: true,
            alternatives: alternatives(&best.name),
            login,
        };
    }
    let mut registered: Vec<&Candidate> = usable
        .iter()
        .copied()
        .filter(|c| c.domains.iter().any(|d| domain_covers(d, site)))
        .collect();
    registered.sort_by_key(|c| role_rank(&c.role));
    if let Some(best) = registered.first() {
        return Choice {
            profile: best.name.clone(),
            why: format!(
                "{} is registered for {site} but holds no live login for it: expect a sign-in page \
                 (its session expired or was never saved)",
                best.name
            ),
            signed_in: false,
            alternatives: registered.iter().skip(1).map(|c| c.name.clone()).take(5).collect(),
            login: None,
        };
    }
    Choice {
        profile: default_profile.to_string(),
        why: format!(
            "no profile you may use holds a login for {site}; using the default profile \
             ({default_profile}). Sign in there, or ask the owner to sign in once in Chrome so \
             the login sync carries it over"
        ),
        signed_in: false,
        alternatives: Vec::new(),
        login: None,
    }
}

/// The registry's primary profile, if one is declared.
pub fn primary_from_registry(reg: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let mut v: Vec<&String> = reg
        .iter()
        .filter(|(_, e)| e.get("role").and_then(|r| r.as_str()) == Some("primary"))
        .map(|(k, _)| k)
        .collect();
    v.sort();
    v.first().map(|s| s.to_string())
}

/// (profile, why) a worker gets when it names none.
pub fn default_profile_for(
    scoped: Option<String>,
    reg: &serde_json::Map<String, serde_json::Value>,
) -> (String, &'static str) {
    if let Some(p) = scoped.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        return (p, "AMUX_BROWSER_DEFAULT_PROFILE");
    }
    if let Some(p) = primary_from_registry(reg) {
        return (p, "the registry's primary profile");
    }
    ("default".to_string(), "no primary profile is declared")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(host: &str, name: &str, expires_unix: Option<i64>) -> CookieRow {
        CookieRow {
            host: host.into(),
            name: name.into(),
            expires_utc: expires_unix.map_or(0, |e| (e + CHROME_EPOCH_OFFSET_S) * 1_000_000),
            persistent: expires_unix.is_some(),
        }
    }

    #[test]
    fn sites_are_registrable_domains() {
        assert_eq!(site_of(".dash.cloudflare.com").as_deref(), Some("cloudflare.com"));
        assert_eq!(site_of("portal.example.co.uk").as_deref(), Some("example.co.uk"));
        assert_eq!(site_of("localhost"), None);
        assert_eq!(site_of("10.0.0.1"), None);
        assert!(domain_covers("dash.cloudflare.com", "cloudflare.com"));
        assert!(domain_covers("brex.com", "brex.com"));
        assert!(!domain_covers("notbrex.com", "brex.com"));
    }

    #[test]
    fn a_tracker_or_an_expired_session_is_not_a_login() {
        let now = 1_800_000_000;
        let jar = summarize(
            &[
                row(".brex.com", "__cf_bm", Some(now + 1800)),         // bot management
                row("dashboard.brex.com", "_ga", Some(now + 9e7 as i64)), // analytics
                row(".brex.com", "session_token", Some(now - 60)),     // expired login
                row(".github.com", "user_session", Some(now + 86_400 * 14)),
                row(".github.com", "logged_in", Some(now + 86_400 * 365)),
                row(".x.com", "auth_token", None),                     // session cookie
            ],
            now,
        );
        assert_eq!(jar.signed_in_to(), vec!["github.com", "x.com"]);
        assert_eq!(jar.cookies, 5);
        assert_eq!(jar.cookies_expired, 1);
        let gh = jar.login_for("github.com").unwrap();
        assert_eq!(gh.expires_at, Some(now + 86_400 * 365));
        assert_eq!(jar.login_for("x.com").unwrap().expires_at, None);
        assert!(jar.login_for("brex.com").is_none(), "brex has only a tracker and an expired session");
    }

    fn cand(name: &str, role: &str, domains: &[&str], allowed: bool, jar: JarSummary) -> Candidate {
        Candidate { name: name.into(), role: role.into(), domains: domains.iter().map(|s| s.to_string()).collect(), allowed, jar }
    }

    #[test]
    fn the_resolver_prefers_a_live_login_then_a_registration_then_the_default() {
        let now = 1_800_000_000;
        let live_brex = summarize(&[row(".brex.com", "session", Some(now + 86_400))], now);
        let dead = summarize(&[row(".brex.com", "__cf_bm", Some(now + 60))], now);
        // The 2026-10-07 shape: `default` lists brex but its session is dead,
        // `brex` is registered and empty, `ethan-money` holds the live login.
        let cands = vec![
            cand("default", "", &[], true, dead.clone()),
            cand("brex", "deprecated", &["brex.com"], true, JarSummary::default()),
            cand("ethan-money", "restricted", &["brex.com"], true, live_brex.clone()),
        ];
        let c = resolve_for_site("brex.com", &cands, "ethan");
        assert_eq!(c.profile, "ethan-money");
        assert!(c.signed_in);

        // The same worker without access to the money profile is told the
        // truth: the registered profile, and that it will see a sign-in page.
        let mut denied = cands.clone();
        denied[2].allowed = false;
        let c = resolve_for_site("brex.com", &denied, "ethan");
        assert_eq!(c.profile, "brex");
        assert!(!c.signed_in);
        assert!(c.why.contains("expect a sign-in page"), "{}", c.why);

        // A site nobody covers falls to the default, saying so.
        let c = resolve_for_site("example.com", &cands, "ethan");
        assert_eq!((c.profile.as_str(), c.signed_in), ("ethan", false));

        // Role breaks a tie between two live logins: primary over deprecated.
        let both = vec![
            cand("old-github", "deprecated", &["github.com"], true,
                 summarize(&[row(".github.com", "user_session", Some(now + 86_400 * 30))], now)),
            cand("ethan", "primary", &[], true,
                 summarize(&[row(".github.com", "user_session", Some(now + 86_400))], now)),
        ];
        let c = resolve_for_site("github.com", &both, "ethan");
        assert_eq!(c.profile, "ethan");
        assert_eq!(c.alternatives, vec!["old-github"]);
    }

    #[test]
    fn the_default_profile_is_scoped_then_primary_then_default() {
        let reg: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({"ethan": {"role": "primary"}, "x": {}})).unwrap();
        assert_eq!(default_profile_for(Some("ethan-personal".into()), &reg).0, "ethan-personal");
        assert_eq!(default_profile_for(None, &reg).0, "ethan");
        assert_eq!(default_profile_for(Some("  ".into()), &serde_json::Map::new()).0, "default");
    }
}
