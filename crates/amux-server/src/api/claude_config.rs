//! The Claude config folder each worker's Claude runs under.
//!
//! Claude Code keeps its transcripts (`projects/`), live-process files
//! (`sessions/<pid>.json`), task lists and settings under `CLAUDE_CONFIG_DIR`,
//! or `~/.claude` when that is unset. A worker launched through a wrapper
//! (`AMUX_CLAUDE_CMD`) that sets it per repo, to run some repos on a second
//! account, wrote everything under that folder while every amux reader looked
//! in `~/.claude`. Measured 2026-11-10: a worker on `~/.claude-team` had a
//! 3.4 MB transcript, history read "readable history unavailable", no
//! conversation id was ever recorded, and that folder's settings carried none
//! of amux's status hooks, so the worker never reported its state either.
//!
//! The wrapper is the only thing that knows the folder, so it is read from the
//! running process: the `worker-resources` job already maps every pane process
//! to its worker every 30 s and calls [`observe`] with each pane's direct
//! children. A pid is read once. The folder is stored in the worker's meta as
//! `cc_config_dir` and [`for_session`] answers from it, so readers stay sync
//! and cheap.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::json;

use super::session_verbs::{claude_home, load_meta, meta_str, update_meta};

/// Meta key holding the config folder a worker's Claude last ran under. Empty
/// or absent means the default, [`claude_home`].
pub(crate) const META_KEY: &str = "cc_config_dir";

/// The config folder `name`'s Claude runs under: the one observed on its live
/// process, else [`claude_home`]. A recorded folder that no longer exists is
/// ignored rather than trusted.
pub(crate) fn for_session(name: &str) -> PathBuf {
    resolve(&meta_str(&load_meta(name), META_KEY), &claude_home())
}

fn resolve(recorded: &str, default: &Path) -> PathBuf {
    let recorded = recorded.trim();
    if recorded.is_empty() {
        return default.to_path_buf();
    }
    let p = PathBuf::from(recorded);
    if p.is_dir() {
        p
    } else {
        default.to_path_buf()
    }
}

/// Every folder a reader should try for `name`, its own first. A worker that
/// moved accounts keeps older transcripts under the folder it left.
pub(crate) fn search_order(name: &str) -> Vec<PathBuf> {
    let own = for_session(name);
    let default = claude_home();
    if own == default {
        vec![own]
    } else {
        vec![own, default]
    }
}

/// `CLAUDE_CONFIG_DIR` from a process's environment entries (`KEY=value`).
/// `Some("")` when the variable is absent or blank: the process uses the
/// default folder, which is a reading, not a failure.
pub(crate) fn config_dir_from_env<'a>(vars: impl IntoIterator<Item = &'a str>) -> String {
    vars.into_iter()
        .filter_map(|v| v.strip_prefix("CLAUDE_CONFIG_DIR="))
        .last()
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

/// The environment block of macOS's `KERN_PROCARGS2` buffer: a native-endian
/// `argc`, the exec path, NUL padding, `argc` arguments, then the environment,
/// each NUL-terminated. Arguments are skipped by count, so an argument that
/// happens to contain `CLAUDE_CONFIG_DIR=` is never read as the environment.
pub(crate) fn procargs2_env(buf: &[u8]) -> Option<Vec<String>> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
    if argc < 0 {
        return None;
    }
    let mut rest = &buf[4..];
    // Exec path, then the padding NULs after it.
    let end = rest.iter().position(|&b| b == 0)?;
    rest = &rest[end..];
    let start = rest.iter().position(|&b| b != 0)?;
    rest = &rest[start..];
    let mut fields = rest.split(|&b| b == 0);
    for _ in 0..argc {
        fields.next()?;
    }
    Some(
        fields
            .take_while(|f| !f.is_empty())
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect(),
    )
}

/// `CLAUDE_CONFIG_DIR` of a live process, `None` when its environment could
/// not be read (gone, another user's, or an unsupported OS).
#[cfg(target_os = "macos")]
pub(crate) fn process_config_dir(pid: u32) -> Option<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size: libc::size_t = 0;
    // SAFETY: a size query with a null buffer, then a read into a buffer of
    // exactly the size the kernel reported; both pass valid pointers and lengths.
    unsafe {
        if libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) != 0 {
            return None;
        }
        let mut buf = vec![0u8; size];
        if libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) != 0 {
            return None;
        }
        buf.truncate(size);
        let env = procargs2_env(&buf)?;
        Some(config_dir_from_env(env.iter().map(String::as_str)))
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn process_config_dir(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let vars: Vec<String> = raw
        .split(|&b| b == 0)
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .collect();
    Some(config_dir_from_env(vars.iter().map(String::as_str)))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn process_config_dir(_pid: u32) -> Option<String> {
    None
}

/// What one observation changed, for the caller's log and for tests.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Observed {
    /// This pid was read before; nothing was done.
    Seen,
    /// The environment could not be read.
    Unreadable,
    /// The folder matches what meta already records.
    Unchanged,
    /// Meta now records a different folder.
    Recorded { was: String, now: String },
}

/// What a process's folder means for the worker's recorded one. The default
/// folder is stored as empty so a worker that leaves a wrapper reads as
/// default again.
pub(crate) fn decide(recorded: &str, observed: &str, default: &Path) -> Option<String> {
    let observed = observed.trim();
    let norm = if observed.is_empty() || Path::new(observed) == default {
        String::new()
    } else {
        observed.to_string()
    };
    (norm != recorded.trim()).then_some(norm)
}

/// Read `pid`'s config folder once and record it as `name`'s. A new non-default
/// folder also gets amux's status hooks, the same entries `~/.claude` carries,
/// so the next Claude started there reports its state.
pub(crate) fn observe(name: &str, pid: u32) -> Observed {
    static READ: OnceLock<Mutex<HashMap<u32, String>>> = OnceLock::new();
    {
        let mut read = READ.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        if read.get(&pid).is_some_and(|w| w == name) {
            return Observed::Seen;
        }
        // Bounded: pids are reused and workers come and go.
        if read.len() > 4096 {
            read.clear();
        }
        read.insert(pid, name.to_string());
    }
    let Some(observed) = process_config_dir(pid) else {
        return Observed::Unreadable;
    };
    let default = claude_home();
    let recorded = meta_str(&load_meta(name), META_KEY);
    let Some(now) = decide(&recorded, &observed, &default) else {
        return Observed::Unchanged;
    };
    update_meta(name, &[(META_KEY, json!(now))]);
    tracing::warn!(session = %name, pid, was = %recorded, now = %now, measured = true, n_considered = 1,
        verdict = "claude_config_dir_recorded",
        "the worker's Claude runs under a different config folder; transcript and history readers follow it");
    if !now.is_empty() {
        install_hooks(Path::new(&now));
    }
    Observed::Recorded { was: recorded, now }
}

fn install_hooks(dir: &Path) {
    // Same guard as ensure_claude_hooks_here: a test server under a throwaway
    // AMUX_HOME must not point a real Claude folder at its observer.
    let home = crate::config::amux_home();
    let default_home = std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".amux"));
    if default_home.as_deref() != Some(home.as_path()) {
        return;
    }
    let settings = dir.join("settings.json");
    match super::native_status::ensure_claude_hooks(&settings, &home.join("native-status.py")) {
        Ok(true) => tracing::warn!(measured = true, n_considered = 1, verdict = "claude_status_hooks_installed",
            settings = %settings.display(),
            "a worker's Claude config folder did not register the status observer; registered it, effective from the worker's next start"),
        Ok(false) => {}
        Err(error) => tracing::warn!(%error, measured = false, n_considered = 1, verdict = "claude_status_hooks_install_failed",
            settings = %settings.display(), "could not register the status observer in a worker's Claude config folder"),
    }
    if let Err(error) = super::native_status::ensure_claude_denies(&settings) {
        tracing::warn!(%error, measured = false, n_considered = 1, verdict = "claude_tool_denies_install_failed",
            settings = %settings.display(), "could not deny the claude.ai Artifact tool in a worker's Claude config folder");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn procargs(argv: &[&str], env: &[&str]) -> Vec<u8> {
        let mut b = (argv.len() as i32).to_ne_bytes().to_vec();
        b.extend_from_slice(b"/usr/local/bin/claude\0\0\0\0");
        for a in argv.iter().chain(env) {
            b.extend_from_slice(a.as_bytes());
            b.push(0);
        }
        b.extend_from_slice(b"\0\0ptr_munge=\0");
        b
    }

    #[test]
    fn procargs2_reads_the_environment_not_the_arguments() {
        let buf = procargs(
            &["claude", "--append-system-prompt", "CLAUDE_CONFIG_DIR=/from/an/argument"],
            &["HOME=/Users/me", "CLAUDE_CONFIG_DIR=/Users/me/.claude-team", "TERM=xterm"],
        );
        let env = procargs2_env(&buf).unwrap();
        assert_eq!(env, ["HOME=/Users/me", "CLAUDE_CONFIG_DIR=/Users/me/.claude-team", "TERM=xterm"]);
        assert_eq!(config_dir_from_env(env.iter().map(String::as_str)), "/Users/me/.claude-team");
    }

    #[test]
    fn procargs2_refuses_a_truncated_buffer() {
        assert_eq!(procargs2_env(&[1, 0]), None);
        let mut buf = 3i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/bin/claude\0\0a\0");
        assert_eq!(procargs2_env(&buf), None, "fewer arguments than argc is not an environment");
    }

    #[test]
    fn an_unset_variable_reads_as_the_default_folder() {
        assert_eq!(config_dir_from_env(["HOME=/h", "PATH=/bin"]), "");
        assert_eq!(config_dir_from_env(["CLAUDE_CONFIG_DIR=  "]), "");
    }

    #[test]
    fn decide_records_only_a_change_and_stores_the_default_as_empty() {
        let default = Path::new("/h/.claude");
        assert_eq!(decide("", "/h/.claude-team", default), Some("/h/.claude-team".into()));
        assert_eq!(decide("/h/.claude-team", "/h/.claude-team", default), None);
        assert_eq!(decide("/h/.claude-team", "", default), Some(String::new()), "a worker that left the wrapper reads as default");
        assert_eq!(decide("", "/h/.claude", default), None, "the default spelled out is still the default");
        assert_eq!(decide("", "", default), None);
    }

    #[test]
    fn a_recorded_folder_is_used_only_while_it_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let default = Path::new("/h/.claude");
        let team = tmp.path().to_string_lossy().into_owned();
        assert_eq!(resolve(&team, default), tmp.path());
        assert_eq!(resolve("", default), default);
        assert_eq!(resolve("/no/such/folder", default), default);
    }

    /// The 2026-11-10 case: a worker whose Claude runs under another
    /// CLAUDE_CONFIG_DIR. Its transcript is found once the folder is recorded,
    /// and not before, which is the history that read "unavailable".
    #[test]
    fn a_worker_on_another_config_folder_has_its_transcript_found() {
        use crate::api::session_verbs::{env_path, lifecycle_transcript_path, project_name, session_jsonl_path, sessions_dir};
        let dir = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(dir.path());
        std::env::set_var("HOME", dir.path());
        std::fs::create_dir_all(sessions_dir()).unwrap();
        let wd = dir.path().join("work");
        std::fs::create_dir_all(&wd).unwrap();
        std::fs::create_dir_all(claude_home().join("projects")).unwrap();
        let team = dir.path().join(".claude-team");
        let project = team.join("projects").join(project_name(&wd.to_string_lossy()));
        std::fs::create_dir_all(&project).unwrap();
        let conv = "0000c0de-1111-4222-8333-000000000a11";
        let transcript = project.join(format!("{conv}.jsonl"));
        std::fs::write(&transcript, "{}\n").unwrap();
        let name = "team-worker";
        std::fs::write(env_path(name), format!("CC_DIR={}\n", wd.display())).unwrap();
        update_meta(name, &[("cc_conversation_id", json!(conv))]);

        assert_eq!(session_jsonl_path(name), None, "unrecorded, the reader looks only in ~/.claude");
        assert_eq!(lifecycle_transcript_path(name, conv), None);

        assert_eq!(
            decide(&meta_str(&load_meta(name), META_KEY), &team.to_string_lossy(), &claude_home()),
            Some(team.to_string_lossy().into_owned())
        );
        update_meta(name, &[(META_KEY, json!(team.to_string_lossy()))]);
        assert_eq!(for_session(name), team);
        assert_eq!(session_jsonl_path(name), Some(transcript.clone()));
        assert_eq!(lifecycle_transcript_path(name, conv), Some(transcript));
    }

    /// The child [`reads_a_live_child_process_environment`] spawns: this test
    /// binary, parked. Not `sleep`: macOS hides a platform binary's
    /// environment from KERN_PROCARGS2, and Claude is not one.
    #[test]
    #[ignore]
    fn env_probe_sleeper() {
        if std::env::var_os("AMUX_ENV_PROBE_SLEEPER").is_some() {
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn reads_a_live_child_process_environment() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "api::claude_config::tests::env_probe_sleeper", "--ignored", "--test-threads=1"])
            .env("AMUX_ENV_PROBE_SLEEPER", "1")
            .env("CLAUDE_CONFIG_DIR", "/tmp/amux-config-dir-probe")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // Until the child has exec'd it still holds this process's environment.
        let mut got = None;
        for _ in 0..100 {
            got = process_config_dir(child.id());
            if got.as_deref().is_some_and(|d| !d.is_empty()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(got.as_deref(), Some("/tmp/amux-config-dir-probe"));
        assert_eq!(process_config_dir(u32::MAX >> 1), None, "a pid that does not exist is unreadable, not default");
    }
}
