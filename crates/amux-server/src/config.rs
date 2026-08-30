//! Server configuration: `~/.amux/server.env` + environment + defaults
//! (RR-0020, Invariant 2).
//!
//! Precedence (highest wins): process environment > server.env > defaults.
//! This mirrors the Python server's `os.environ.setdefault` semantics so the
//! same server.env file drives both servers during the strangler-fig
//! migration. The four-tier Org/Global/Group/Worker resolution for
//! worker-scoped config happens in amux-core's scope module against DB rows;
//! this file only handles PROCESS-level configuration.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The port a server binds when nothing says otherwise.
///
/// 8823 originally meant "not 8822, which Python owns". Python retired
/// (792ce1f) and the installed service now answers 8822 AND 8824 — but the
/// value stays, for a different and still-live reason: `cargo run -p
/// amux-server` on a dev machine must not collide with the running service.
/// The launchd agent sets `AMUX_RS_PORT` explicitly; nothing in production
/// depends on this default.
///
/// The CLIENT default deliberately differs (`DEFAULT_CLIENT_URL` in amux-cli
/// points at the installed port, 8824): a client's job is to reach the server
/// that IS running, and pointing it here is what made every bare `amux-rs`
/// invocation fail with a connection error indistinguishable from the server
/// being down (AMUX-2672).
pub const DEFAULT_PORT: u16 = 8823;

/// The port THIS server is actually answering on — the one a client should be
/// told to call.
///
/// Reads the same `AMUX_RS_PORT` that [`ServerConfig::load`] does, which is
/// safe from anywhere because that load exports server.env into the process env
/// (setdefault) before anything else runs. Deliberately NOT the legacy port and
/// NOT a literal.
///
/// This exists because the literal was the bug. `session_verbs` hardcoded
/// `AMUX_URL=https://localhost:8822` into every tmux lane it started, which
/// pinned two deployments to one number at once: the local install (8824) could
/// not retire the legacy address while new sessions kept minting it, and the
/// cloud image had to bind 8822 *because* of the hardcode — its Dockerfile said
/// so in a comment, naming this exact line. One env-derived accessor lets each
/// deployment answer for itself, with no build-time branch (the single-codebase
/// rule) and nothing to keep in step.
pub fn canonical_port() -> u16 {
    std::env::var("AMUX_RS_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// `~/.amux` — sessions dir, DB, TLS material, tokens.
    pub amux_home: PathBuf,
    pub port: u16,
    /// Path to the SQLite database (shared with the Python server).
    pub db_path: PathBuf,
    /// Everything from server.env plus process env overlays, for
    /// worker-environment assembly later.
    pub env: BTreeMap<String, String>,
}

/// Names the keys THIS server exported from `server.env`, carried across the
/// self-adoption exec so the successor can tell its own exports apart from
/// values the process genuinely supplied (AMUX-3612).
///
/// Internal plumbing, not configuration: stripped from [`ServerConfig::env`] so
/// it never reaches a worker environment.
pub const ENV_FROM_FILE_MARKER: &str = "AMUX_ENV_FROM_FILE";

/// What one load should do to the process environment, computed WITHOUT
/// touching it.
///
/// Separated from [`ServerConfig::load`] on this file's own standing advice:
/// `set_var` is global and `cargo test` runs in parallel, so the rules are
/// tested over injected inputs rather than by mutating the environment and
/// hoping no sibling test reads it mid-flight.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct EnvPlan {
    /// Keys to `set_var`, with the value server.env currently gives them.
    pub export: BTreeMap<String, String>,
    /// Keys we exported on an earlier boot that server.env no longer sets.
    pub unset: Vec<String>,
    /// The new marker value for the successor.
    pub marker: String,
}

impl EnvPlan {
    /// Keys whose value comes from the FILE rather than from the process, so
    /// the "process wins" overlay must leave them alone.
    fn file_owned(&self) -> std::collections::BTreeSet<&str> {
        self.export.keys().map(String::as_str).collect()
    }

    /// The subset of [`export`](Self::export) that actually needs writing.
    ///
    /// `load` is NOT a boot-only function: `invariants::monitor` calls
    /// `from_process_env()` on every sweep just to find the home dir, so this
    /// runs every ~15s for the life of the process. Before the marker existed
    /// that was harmless, because every key was already present and the
    /// setdefault guard skipped it; refreshing marked keys unconditionally
    /// would have turned a boot-time mutation into a periodic one.
    ///
    /// That matters beyond tidiness: `setenv` concurrent with another thread's
    /// `getenv` is a data race in the platform libc, and this server is heavily
    /// threaded. Writing only on a real change puts the steady state back to
    /// zero mutations and confines the exposure to the moment somebody actually
    /// edits server.env.
    ///
    /// The upside is real and was not designed for: because the monitor reloads
    /// on a timer, a server.env edit to a marked key now takes effect within
    /// about 30 seconds, with no redeploy and no restart. Verified live on
    /// 2026-08-24 with `/health`'s `build` bracketed to prove no redeploy
    /// happened, against the two unmarked keys as a paired control.
    fn writes<'a>(&'a self, live: &dyn Fn(&str) -> Option<String>) -> Vec<(&'a str, &'a str)> {
        self.export
            .iter()
            .filter(|(k, v)| live(k).as_deref() != Some(v.as_str()))
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }
}

/// # Why a marker exists at all
///
/// Self-adoption re-execs in place (`lib.rs`, AMUX-3458) and
/// `Command::new(exe)` inherits the parent's whole environment. Combined with
/// the setdefault rule below, that pinned every exported value for the lifetime
/// of the process LINEAGE: a key exported on some earlier boot is "already
/// present" forever after, so editing server.env and waiting for the builder to
/// redeploy changed nothing, indefinitely, with the file plainly showing the new
/// value. `config.env_reaches_process` had been failing on AMUX_OWNER_PHONE and
/// GRANOLA_API_KEY with no way to self-heal.
///
/// The marker restores the distinction the inherited env destroys. A key the
/// PROCESS supplies still wins, which is the property setdefault exists to
/// protect; a key WE exported is refreshed from the file on every load.
///
/// # The limit, stated because it is not fixable from here
///
/// A lineage that started before this shipped carries no marker, so its
/// pre-existing exports are indistinguishable from launchd's own environment and
/// are left alone. Those need one real `launchctl kickstart` to clear. Guessing
/// instead would mean treating every server.env key as ours on an unmarked boot,
/// which would flip `AMUX_RS_PORT` out from under a launchd agent that sets it
/// explicitly. Going forward is worth having; a one-time restart is cheap; a
/// port change nobody asked for is not.
pub(crate) fn plan_env(
    file: &BTreeMap<String, String>,
    file_exists: bool,
    process_env: &BTreeMap<String, String>,
    live: &dyn Fn(&str) -> bool,
    prev_marker: Option<&str>,
) -> EnvPlan {
    let prev: std::collections::BTreeSet<&str> = prev_marker
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .collect();
    let mut plan = EnvPlan::default();
    for (k, v) in file {
        if k == ENV_FROM_FILE_MARKER {
            continue;
        }
        let elsewhere = process_env.contains_key(k) || live(k);
        // Export when nothing else supplies it, OR when the only thing
        // supplying it is our own earlier export.
        if !elsewhere || prev.contains(k.as_str()) {
            plan.export.insert(k.clone(), v.clone());
        }
    }
    // A key we exported that server.env no longer sets must be withdrawn, or
    // DELETING a line is exactly as ineffective as changing one, which is the
    // same bug pointing the other way.
    //
    // Gated on the file existing: `parse_env_file` returns an empty map for a
    // missing file and for an empty one alike, and an unreadable server.env
    // must never silently wipe the process config. The loud direction is to
    // leave things as they are.
    if file_exists {
        plan.unset = prev
            .iter()
            .filter(|k| !file.contains_key(**k))
            .map(|k| k.to_string())
            .collect();
    }
    plan.marker = plan.export.keys().cloned().collect::<Vec<_>>().join(",");
    plan
}

impl ServerConfig {
    /// Load configuration. Pure given its inputs — callers pass the home dir
    /// and process env so tests can drive it hermetically.
    pub fn load(home: PathBuf, process_env: &BTreeMap<String, String>) -> Self {
        let env_path = home.join("server.env");
        let mut env = parse_env_file(&env_path);
        // PYTHON-PARITY SETDEFAULT, for real: export server.env values into
        // the PROCESS env when the process doesn't already set them. The doc
        // above always claimed setdefault semantics, but values only reached
        // the Config struct — every `std::env::var()` read site (the
        // AMUX_RS_SCHEDULER gate, AMUX_HERDR_SESSION, the caps/knobs) saw
        // nothing, so server.env flags silently didn't work (live incident
        // 2026-08-09: scheduler stayed in shadow mode with the flag set).
        //
        // ...with one correction: "the process doesn't already set them" was
        // measuring the wrong thing across a self-adoption exec. See
        // [`plan_env`] for what the marker restores and what it cannot.
        let prev_marker = process_env
            .get(ENV_FROM_FILE_MARKER)
            .cloned()
            .or_else(|| std::env::var(ENV_FROM_FILE_MARKER).ok());
        let plan = plan_env(
            &env,
            env_path.exists(),
            process_env,
            &|k| std::env::var_os(k).is_some(),
            prev_marker.as_deref(),
        );
        // Only real changes are written — see `EnvPlan::writes`. This function
        // runs on a timer, not just at boot.
        for (k, v) in plan.writes(&|k| std::env::var(k).ok()) {
            std::env::set_var(k, v);
        }
        for k in &plan.unset {
            if std::env::var_os(k).is_some() {
                std::env::remove_var(k);
            }
        }
        if std::env::var(ENV_FROM_FILE_MARKER).ok().as_deref() != Some(plan.marker.as_str()) {
            std::env::set_var(ENV_FROM_FILE_MARKER, &plan.marker);
        }
        // Process wins over server.env (same rule as Python's setdefault) —
        // EXCEPT for keys the process only holds because we put them there.
        let ours = plan.file_owned();
        for (k, v) in process_env {
            if !ours.contains(k.as_str()) {
                env.insert(k.clone(), v.clone());
            }
        }
        // Never let the marker reach a worker environment: it is this server's
        // bookkeeping about its own exec, and a lane inheriting it would report
        // its own env as file-owned.
        env.remove(ENV_FROM_FILE_MARKER);
        let port = env
            .get("AMUX_RS_PORT")
            .and_then(|p| p.parse().ok())
            .unwrap_or(DEFAULT_PORT);
        let db_path = env
            .get("AMUX_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("amux.db"));
        ServerConfig {
            amux_home: home,
            port,
            db_path,
            env,
        }
    }

    pub fn from_process_env() -> Self {
        let process_env: BTreeMap<String, String> = std::env::vars().collect();
        Self::load(amux_home(), &process_env)
    }

    pub fn tls_dir(&self) -> PathBuf {
        self.amux_home.join("tls")
    }

    /// Python's `_AUTH_TOKEN_FILE` (amux-server.py:700): `auth_token`,
    /// UNDERSCORE. This crate shipped reading `auth-token` (dash) and minted
    /// its own token there, so every client holding the real shared token got
    /// 401s from this server while the auth docstring claimed the file was
    /// shared. The stale dash-file may still exist on machines that ran the
    /// old build; nothing reads it anymore.
    pub fn auth_token_path(&self) -> PathBuf {
        self.amux_home.join("auth_token")
    }
}

/// THE amux home: `$AMUX_HOME`, legacy `$CC_HOME`, else `~/.amux`.
///
/// One resolver, because there were TEN and they did not agree (AMUX-2919).
/// Nine private copies plus `from_process_env` had drifted into three distinct
/// behaviours, and the divergences were the interesting part:
///
///   * **`AMUX_HOME=""` was treated as SET by nine of the ten.**
///     `std::env::var` returns `Ok("")` for an exported-but-empty variable, so
///     `PathBuf::from("")` produced an EMPTY path and every `amux_home().join(x)`
///     silently became the RELATIVE path `x` — writing the DB, tls dir and auth
///     token wherever the process happened to be cwd'd. Only api/settings.rs
///     checked for empty. That check is now the shared one.
///   * **`CC_HOME` was honoured by exactly one of the ten** (api/settings.rs,
///     which serves settings/journal/history). So with `CC_HOME` set and
///     `AMUX_HOME` unset, settings read one home while groups, dictation, push
///     and the rest read another — one server, two data directories. `CC_HOME`
///     is unset on this machine today, so unifying on it changes nothing now
///     and closes the split-brain if anything ever sets it. The bash `amux` CLI
///     already honours it (amux:35), which is where the divergence would bite.
///   * **The `$HOME`-missing fallback split** `unwrap_or_default()` (→ the
///     relative `.amux`) against `PathBuf::from("/")` (→ `/.amux`). Now `/.amux`
///     everywhere: an absolute path is wrong loudly, a relative one is wrong
///     silently.
///
/// api/settings.rs's docstring claimed it matched `from_process_env`; it did
/// not, in both of the ways above. That claim is now true because there is only
/// one implementation left to be true about (ethos rule 6).
pub fn amux_home() -> PathBuf {
    resolve_home(|k| std::env::var(k).ok())
}

/// The resolution itself, over an injected lookup.
///
/// Split out so the rules above are testable WITHOUT setting process env:
/// `std::env::set_var` is global and `cargo test` runs threads in parallel, so
/// an env-mutating test races every other test that reads a home — which is
/// most of them. A test that must run single-threaded to be correct is one
/// that will be made green by `--test-threads=1` and quietly stop
/// discriminating (ethos rule 7).
///
/// SCOPE OF THE MITIGATIONS, stated so this warning is not read as
/// fully-handled (AMUX-3415): `test_env::set_home`'s LOCK closes the
/// mutator-vs-mutator half only — reader sites do not take it, so a
/// guardless concurrent reader can still observe a guard's fixture home.
/// This injected-lookup seam is the full fix where it can be used; the
/// guard is the accepted fallback where it cannot.
fn resolve_home(get: impl Fn(&str) -> Option<String>) -> PathBuf {
    for var in ["AMUX_HOME", "CC_HOME"] {
        match get(var) {
            Some(h) if !h.is_empty() => return PathBuf::from(h),
            _ => {}
        }
    }
    match get("HOME") {
        Some(h) if !h.is_empty() => PathBuf::from(h).join(".amux"),
        _ => PathBuf::from("/").join(".amux"),
    }
}

#[cfg(test)]
mod home_resolution_tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn amux_home_wins_and_cc_home_is_the_legacy_fallback() {
        assert_eq!(
            resolve_home(env(&[("AMUX_HOME", "/a"), ("CC_HOME", "/c"), ("HOME", "/h")])),
            PathBuf::from("/a")
        );
        assert_eq!(
            resolve_home(env(&[("CC_HOME", "/c"), ("HOME", "/h")])),
            PathBuf::from("/c"),
            "CC_HOME was honoured by exactly one of the ten old copies — settings read \
             one home while groups/dictation/push read another"
        );
        assert_eq!(resolve_home(env(&[("HOME", "/h")])), PathBuf::from("/h/.amux"));
    }

    /// The bug that made this consolidation worth doing. An exported-but-empty
    /// variable yields `Ok("")`, and nine of the ten copies mapped that
    /// straight to `PathBuf::from("")` — an EMPTY path, so every `.join(x)`
    /// became the RELATIVE path `x` and the DB, tls dir and auth token landed
    /// wherever the process was cwd'd. Silent, and cwd-dependent.
    #[test]
    fn an_exported_but_empty_var_is_not_a_home() {
        assert_eq!(
            resolve_home(env(&[("AMUX_HOME", ""), ("HOME", "/h")])),
            PathBuf::from("/h/.amux")
        );
        assert_eq!(
            resolve_home(env(&[("AMUX_HOME", ""), ("CC_HOME", ""), ("HOME", "/h")])),
            PathBuf::from("/h/.amux")
        );
        // The shape the old code produced, asserted as NOT happening: a
        // relative path is the failure mode, so name it explicitly.
        let got = resolve_home(env(&[("AMUX_HOME", ""), ("HOME", "/h")]));
        assert!(got.is_absolute(), "an empty AMUX_HOME must not yield a relative home: {got:?}");
    }

    /// `$HOME` missing split the old copies too: `unwrap_or_default()` gave the
    /// relative `.amux`, `PathBuf::from("/")` gave `/.amux`. Absolute wins —
    /// wrong loudly beats wrong silently.
    #[test]
    fn a_missing_home_still_resolves_absolute() {
        let got = resolve_home(env(&[]));
        assert_eq!(got, PathBuf::from("/.amux"));
        assert!(got.is_absolute());
    }
}

/// Parse a KEY=VALUE env file. Supports `#` comments, blank lines, single or
/// double quoted values, and `export ` prefixes — the shapes that appear in
/// real server.env files today.
pub fn parse_env_file(path: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(content) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let mut v = v.trim();
        if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
            || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
        {
            v = &v[1..v.len() - 1];
        }
        if !k.is_empty() {
            out.insert(k.to_string(), v.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }
    /// Nothing in the live process env, so only the injected maps decide.
    fn no_live(_: &str) -> bool {
        false
    }

    /// AMUX-3612. The bug: self-adoption re-execs with the parent's whole
    /// environment, so setdefault saw every previously-exported key as
    /// "already present" and server.env edits could never land, forever.
    ///
    /// All four arms in one test on one set of inputs, because the value of the
    /// marker is precisely that it TELLS TWO IDENTICAL-LOOKING CASES APART. A
    /// test of the refresh alone would pass against a version that clobbers
    /// genuine process env, which is the property setdefault exists to protect
    /// and the one thing worse than the bug.
    #[test]
    fn a_key_we_exported_is_refreshed_but_a_key_the_process_owns_is_not() {
        let file = map(&[
            ("STALE", "new-from-file"),  // we exported it last boot: refresh
            ("THEIRS", "file-value"),    // launchd set it: leave alone
            ("FRESH", "brand-new"),      // nobody has it: export
        ]);
        // What the successor inherited across the exec. STALE and THEIRS look
        // IDENTICAL here: both are simply present. Only the marker separates
        // them, which is the whole point.
        let inherited = map(&[("STALE", "old-value"), ("THEIRS", "launchd-value")]);

        let plan = plan_env(&file, true, &inherited, &no_live, Some("STALE"));

        assert_eq!(
            plan.export.get("STALE").map(String::as_str),
            Some("new-from-file"),
            "a key WE exported must be refreshed from the file, or a server.env edit never lands"
        );
        assert!(
            !plan.export.contains_key("THEIRS"),
            "a key the PROCESS supplies must still win — that is what setdefault is for"
        );
        assert_eq!(
            plan.export.get("FRESH").map(String::as_str),
            Some("brand-new"),
            "the original setdefault behaviour must survive"
        );
        // The overlay must leave our own keys alone, or the refresh above is
        // undone one loop later and the struct reports the stale value while
        // the process env holds the new one.
        let ours = plan.file_owned();
        assert!(ours.contains("STALE") && ours.contains("FRESH"));
        assert!(!ours.contains("THEIRS"));
    }

    /// `load` runs on a TIMER, not just at boot: `invariants::monitor` calls
    /// `from_process_env()` every sweep to find the home dir. So refreshing
    /// marked keys unconditionally would `setenv` every ~15s forever, and
    /// `setenv` racing another thread's `getenv` is a data race in the platform
    /// libc on a server this threaded.
    ///
    /// Steady state must be ZERO writes; a genuine edit must be exactly one.
    #[test]
    fn a_refresh_writes_only_when_the_value_actually_changed() {
        let file = map(&[("A", "v1"), ("B", "v2")]);
        let inherited = map(&[("A", "v1"), ("B", "v2")]);
        let plan = plan_env(&file, true, &inherited, &no_live, Some("A,B"));

        // Both are ours, so both must stay file-owned or the overlay clobbers
        // them — ownership and writing are different questions.
        assert_eq!(plan.file_owned().len(), 2);

        let settled = |k: &str| inherited.get(k).cloned();
        assert!(
            plan.writes(&settled).is_empty(),
            "steady state must write nothing: {:?}",
            plan.writes(&settled)
        );

        // One key edited in the file: exactly one write, and it is that key.
        let drifted = |k: &str| if k == "B" { Some("stale".to_string()) } else { inherited.get(k).cloned() };
        assert_eq!(plan.writes(&drifted), vec![("B", "v2")]);
    }

    /// Deleting a line from server.env has to work too, or this is the same
    /// bug pointing the other way.
    #[test]
    fn a_key_removed_from_the_file_is_withdrawn_but_only_if_the_file_is_readable() {
        let file = map(&[("KEPT", "v")]);
        let inherited = map(&[("KEPT", "v"), ("GONE", "old")]);

        let plan = plan_env(&file, true, &inherited, &no_live, Some("KEPT,GONE"));
        assert_eq!(plan.unset, vec!["GONE".to_string()], "a key the file no longer sets must be withdrawn");

        // THE DESTRUCTIVE CASE. `parse_env_file` returns an empty map for a
        // MISSING file and an EMPTY one alike, so without the existence gate an
        // unreadable server.env would withdraw every key the server had ever
        // exported. Same inputs, file_exists=false, and nothing may be unset.
        let plan = plan_env(&BTreeMap::new(), false, &inherited, &no_live, Some("KEPT,GONE"));
        assert!(
            plan.unset.is_empty(),
            "an unreadable server.env must not wipe the process config: {:?}",
            plan.unset
        );
    }

    /// An unmarked lineage is one that started before this shipped. Its exports
    /// are indistinguishable from launchd's own environment, so they are left
    /// alone and need a real restart. Pinned so nobody "fixes" it into guessing:
    /// treating every file key as ours on an unmarked boot would flip
    /// AMUX_RS_PORT out from under the launchd agent that sets it explicitly.
    #[test]
    fn an_unmarked_lineage_is_left_alone_rather_than_guessed_at() {
        let file = map(&[("AMUX_RS_PORT", "9999"), ("AMUX_OWNER_PHONE", "new")]);
        let inherited = map(&[("AMUX_RS_PORT", "8824"), ("AMUX_OWNER_PHONE", "stale")]);
        let plan = plan_env(&file, true, &inherited, &no_live, None);
        assert!(
            plan.export.is_empty(),
            "with no marker, nothing may be assumed ours: {:?}",
            plan.export
        );
        assert!(plan.unset.is_empty());
    }

    /// The marker has to survive the exec naming exactly what we exported, or
    /// the next load is unmarked again and the fix lasts one boot.
    #[test]
    fn the_marker_names_what_was_exported_and_nothing_else() {
        let file = map(&[("A", "1"), ("B", "2"), ("THEIRS", "3")]);
        let inherited = map(&[("THEIRS", "set-by-launchd")]);
        let plan = plan_env(&file, true, &inherited, &no_live, None);
        assert_eq!(plan.marker, "A,B");
        // Feeding the marker back in must be stable: the same file and an env
        // that now carries our exports yields the same answer, not a shrinking
        // set that forgets a key every boot.
        let after = map(&[("THEIRS", "set-by-launchd"), ("A", "1"), ("B", "2")]);
        let plan2 = plan_env(&file, true, &after, &no_live, Some(&plan.marker));
        assert_eq!(plan2.marker, "A,B", "the export set must be stable across generations");
        assert_eq!(plan2.export.get("A").map(String::as_str), Some("1"));
    }

    #[test]
    fn parses_env_file_shapes() {
        let dir = std::env::temp_dir().join(format!("amux-cfg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("server.env");
        std::fs::write(
            &p,
            "# comment\nAMUX_S3_BUCKET=my-bucket\nQUOTED=\"has spaces\"\nexport EXPORTED='single'\n\nBROKEN_LINE\n",
        )
        .unwrap();
        let env = parse_env_file(&p);
        assert_eq!(env.get("AMUX_S3_BUCKET").unwrap(), "my-bucket");
        assert_eq!(env.get("QUOTED").unwrap(), "has spaces");
        assert_eq!(env.get("EXPORTED").unwrap(), "single");
        assert!(!env.contains_key("BROKEN_LINE"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn process_env_beats_server_env() {
        let dir = std::env::temp_dir().join(format!("amux-cfg-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("server.env"), "AMUX_RS_PORT=9000\nA=file\n").unwrap();
        let mut penv = BTreeMap::new();
        penv.insert("AMUX_RS_PORT".to_string(), "9001".to_string());
        let cfg = ServerConfig::load(dir.clone(), &penv);
        assert_eq!(cfg.port, 9001);
        assert_eq!(cfg.env.get("A").unwrap(), "file");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn defaults_when_nothing_set() {
        let dir = std::env::temp_dir().join(format!("amux-cfg-test3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = ServerConfig::load(dir.clone(), &BTreeMap::new());
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert_eq!(cfg.db_path, dir.join("amux.db"));
        std::fs::remove_dir_all(&dir).ok();
    }
}

// ── Shared env parsing (AMUX-2919) ─────────────────────────────────────────
// These were duplicated verbatim across runtime_jobs/board_drive.rs,
// runtime_jobs/autofix.rs and api/git_guard.rs. Unlike `amux_home` above, the
// copies genuinely WERE identical, so this consolidation is mechanical.

/// `$KEY` parsed as f64, trimmed, falling back to `default`.
pub fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// `$KEY` parsed as i64, trimmed, falling back to `default`.
pub fn env_i64(key: &str, default: i64) -> i64 {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// Unix epoch seconds as f64. Three identical copies (board_drive, alerts,
/// session_verbs).
///
/// NOT to be conflated with the two `now_secs()` functions, which return
/// DIFFERENT TYPES from different clocks — api/upload.rs returns u64 from
/// SystemTime, api/board.rs returns i64 from chrono::Utc. They share a name and
/// nothing else; merging them on the strength of the name is the mistake this
/// comment exists to prevent.
pub fn now_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
