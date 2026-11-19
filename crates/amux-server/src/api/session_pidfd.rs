//! PID reuse resistant signaling for a tmux pane's owned process tree.
//!
//! The lane lifecycle lock fences server starts/stops. The caller also holds
//! the metadata write lock while validating session ids and signaling this
//! tree, so an identity hook cannot replace the claim between those steps.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ProcessIdentity {
    pid: i32,
    start_ticks: u64,
}

impl ProcessIdentity {
    pub(super) fn read(pid: i32) -> io::Result<Self> {
        if pid <= 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid process id",
            ));
        }
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let rest = stat
            .rsplit_once(')')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid proc stat"))?
            .1;
        let start_ticks = rest
            .split_whitespace()
            .nth(19)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing process start time")
            })?;
        Ok(Self { pid, start_ticks })
    }
}

struct BoundProcess(OwnedFd);

impl BoundProcess {
    fn open(identity: &ProcessIdentity) -> io::Result<Self> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        if ProcessIdentity::read(identity.pid)? != *identity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "process identity changed",
            ));
        }
        let process = Self(fd);
        process.ensure_alive()?;
        Ok(process)
    }

    fn ensure_alive(&self) -> io::Result<()> {
        let mut pfd = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pfd, 1, 0) };
        if ready < 0 {
            return Err(io::Error::last_os_error());
        }
        if ready != 0 {
            return Err(io::Error::new(io::ErrorKind::NotFound, "process exited"));
        }
        Ok(())
    }

    fn signal(&self, signal: i32) -> io::Result<()> {
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

fn process_rows() -> io::Result<Vec<(i32, i32)>> {
    let entries = std::fs::read_dir("/proc")?;
    let mut rows = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        if pid <= 1 {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let _state = fields.next();
        let Some(ppid) = fields.next().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        rows.push((pid, ppid));
    }
    Ok(rows)
}

fn descendants(root: i32, rows: &[(i32, i32)]) -> Vec<i32> {
    let mut result = Vec::new();
    let mut parents = vec![root];
    while let Some(parent) = parents.pop() {
        for &(pid, ppid) in rows {
            if ppid == parent && pid > 1 && pid != root && !result.contains(&pid) {
                result.push(pid);
                parents.push(pid);
            }
        }
    }
    result
}

struct Frozen(Vec<(i32, BoundProcess)>);
impl Drop for Frozen {
    fn drop(&mut self) {
        for (_, process) in &self.0 {
            let _ = process.signal(libc::SIGCONT);
        }
    }
}

struct ResumeRoot(BoundProcess);
impl Drop for ResumeRoot {
    fn drop(&mut self) {
        let _ = self.0.signal(libc::SIGCONT);
    }
}

/// Stop pane children but retain the pane shell. Handles are opened and
/// identity-checked before signaling; pidfd signals cannot land on a reused pid.
pub(super) fn stop_pane_children(root: i32) -> anyhow::Result<()> {
    anyhow::ensure!(root > 1, "invalid pane process id");
    let root_identity = ProcessIdentity::read(root)?;
    let root_process = ResumeRoot(BoundProcess::open(&root_identity)?);
    let mut frozen = Frozen(Vec::new());
    // Freeze the pane shell first, then each discovered generation of children.
    // The repeated scan closes the fork window before any child is killed.
    root_process.0.signal(libc::SIGSTOP)?;
    loop {
        let rows = process_rows()?;
        let known: std::collections::HashSet<i32> = frozen.0.iter().map(|(pid, _)| *pid).collect();
        let fresh: Vec<i32> = descendants(root, &rows)
            .into_iter()
            .filter(|pid| !known.contains(pid))
            .collect();
        if fresh.is_empty() {
            break;
        }
        for pid in fresh {
            match ProcessIdentity::read(pid).and_then(|id| BoundProcess::open(&id)) {
                Ok(process) => {
                    process.signal(libc::SIGSTOP)?;
                    frozen.0.push((pid, process));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    // The root is retained. Children are signaled through the handles frozen
    // above, in reverse discovery order so descendants go first.
    for (_, process) in frozen.0.iter().rev() {
        match process.signal(libc::SIGKILL) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => return Err(error.into()),
        }
    }
    frozen.0.clear();
    std::thread::sleep(std::time::Duration::from_millis(100));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};

    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn stale_process_identity_cannot_signal_replacement() {
        let mut child = ChildGuard(Command::new("sleep").arg("30").spawn().unwrap());
        let mut stale = ProcessIdentity::read(child.0.id() as i32).unwrap();
        stale.start_ticks = stale.start_ticks.wrapping_add(1);
        assert!(BoundProcess::open(&stale).is_err());
        assert!(child.0.try_wait().unwrap().is_none());
    }

    #[test]
    fn stopped_pane_children_leave_root_alive() {
        let mut root = ChildGuard(
            Command::new("sh")
                .args(["-c", "sleep 30 & echo $!; wait"])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        use std::io::BufRead;
        let child_pid: i32 = std::io::BufReader::new(root.0.stdout.take().unwrap())
            .lines()
            .next()
            .unwrap()
            .unwrap()
            .parse()
            .unwrap();
        stop_pane_children(root.0.id() as i32).unwrap();
        assert!(
            root.0.try_wait().unwrap().is_none(),
            "pane shell must remain alive"
        );
        let child_state = std::fs::read_to_string(format!("/proc/{child_pid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next().map(str::to_owned))
            });
        assert!(
            child_state
                .as_deref()
                .is_none_or(|state| state == "Z" || state == "X"),
            "owned child must be terminated; observed state {child_state:?}"
        );
    }
}
