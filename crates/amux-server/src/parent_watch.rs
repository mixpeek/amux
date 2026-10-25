//! A scratch server exits when the process that started it dies.
//!
//! Test harnesses start amux-server with a throwaway `AMUX_HOME` under a temp
//! directory and stop it in teardown. When the harness itself is killed, the
//! teardown never runs: the server is re-parented to launchd and keeps every
//! background loop going (pane captures, tmux spawns, scans of its own DB)
//! against a home nobody will ever read again. Mac CPU RCA 20261018-015302
//! found two of these, 3.5 days old, from a two-server pytest fixture, plus a
//! third tree of children: together they forked as often as the real server,
//! about 29 processes a second, on a machine at 54% sys time from fork churn.
//!
//! The guard is narrow on purpose. It arms only when BOTH hold:
//! - the server was started by a live parent (initial ppid is not 1), so a
//!   launchd or systemd server is never watched, and
//! - `AMUX_HOME` is under a temp directory, so a real server someone started
//!   by hand with `nohup` survives its shell.
//!
//! `AMUX_EXIT_WITH_PARENT=1` arms it for any home; `=0` disarms it.

use std::path::{Path, PathBuf};
use std::time::Duration;

const POLL: Duration = Duration::from_secs(5);

/// Directories a throwaway home lives under. The system temp dir is passed in
/// so a test can name its own.
fn temp_roots(system_temp: &Path) -> Vec<PathBuf> {
    let mut roots = vec![
        system_temp.to_path_buf(),
        PathBuf::from("/tmp"),
        PathBuf::from("/private/tmp"),
        PathBuf::from("/var/folders"),
        PathBuf::from("/private/var/folders"),
    ];
    if let Ok(c) = system_temp.canonicalize() {
        roots.push(c);
    }
    roots
}

/// Why the guard arms, or None when it must not. Pure, so every branch is
/// testable without forking.
pub(crate) fn arm_reason(
    initial_ppid: i32,
    home: &Path,
    system_temp: &Path,
    override_env: Option<&str>,
) -> Option<&'static str> {
    if initial_ppid <= 1 {
        return None;
    }
    match override_env.map(str::trim) {
        Some("0") | Some("false") => return None,
        Some("1") | Some("true") => return Some("AMUX_EXIT_WITH_PARENT"),
        _ => {}
    }
    let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    temp_roots(system_temp)
        .iter()
        .any(|r| home.starts_with(r))
        .then_some("amux_home_under_temp")
}

/// Start the watch on its own OS thread. It never touches the async runtime,
/// so a wedged runtime cannot keep an orphan alive.
pub fn start(home: &Path) {
    #[cfg(unix)]
    {
        // SAFETY: getppid has no preconditions and cannot fail.
        let initial = unsafe { libc::getppid() };
        let over = std::env::var("AMUX_EXIT_WITH_PARENT").ok();
        let Some(reason) = arm_reason(initial, home, &std::env::temp_dir(), over.as_deref()) else {
            return;
        };
        tracing::info!(initial_ppid = initial, reason, home = %home.display(), measured = true, n_considered = 1,
            verdict = "parent_watch_armed", "scratch server: exits when the process that started it dies");
        let home = home.display().to_string();
        let _ = std::thread::Builder::new()
            .name("parent-watch".into())
            .spawn(move || loop {
                std::thread::sleep(POLL);
                // SAFETY: as above.
                let now = unsafe { libc::getppid() };
                if now != initial {
                    tracing::warn!(initial_ppid = initial, ppid = now, reason, home = %home, measured = true, n_considered = 1,
                        verdict = "orphaned_scratch_server_exit",
                        "the process that started this scratch server is gone; exiting instead of running its loops for nobody");
                    std::process::exit(0);
                }
            });
    }
    #[cfg(not(unix))]
    let _ = home;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scratch_home_under_temp_arms_and_a_real_home_does_not() {
        let t = Path::new("/var/folders/0x/abc/T");
        assert_eq!(
            arm_reason(4242, Path::new("/var/folders/0x/abc/T/tmp40wf0hdr/a/amux"), t, None),
            Some("amux_home_under_temp")
        );
        assert_eq!(
            arm_reason(4242, Path::new("/private/tmp/claude-501/x/scratchpad/mdamux"), t, None),
            Some("amux_home_under_temp")
        );
        assert_eq!(arm_reason(4242, Path::new("/Users/ethan/.amux"), t, None), None);
    }

    #[test]
    fn a_server_launchd_or_systemd_started_is_never_watched() {
        let t = Path::new("/tmp");
        assert_eq!(arm_reason(1, Path::new("/tmp/x/amux"), t, None), None);
        assert_eq!(arm_reason(1, Path::new("/tmp/x/amux"), t, Some("1")), None);
        assert_eq!(arm_reason(0, Path::new("/tmp/x/amux"), t, None), None);
    }

    #[test]
    fn the_override_arms_any_home_or_disarms_a_temp_one() {
        let t = Path::new("/tmp");
        assert_eq!(arm_reason(77, Path::new("/srv/amux"), t, Some("1")), Some("AMUX_EXIT_WITH_PARENT"));
        assert_eq!(arm_reason(77, Path::new("/tmp/x/amux"), t, Some("0")), None);
        assert_eq!(arm_reason(77, Path::new("/tmp/x/amux"), t, Some("")), Some("amux_home_under_temp"));
    }

    #[test]
    fn a_custom_system_temp_counts() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("amux");
        std::fs::create_dir_all(&home).unwrap();
        assert_eq!(arm_reason(9, &home, d.path(), None), Some("amux_home_under_temp"));
    }
}
