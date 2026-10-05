//! Keep the listening socket open across a self-adoption exec.
//!
//! Every commit makes the builder install a new binary, and the running server
//! `exec`s it in place (AMUX-3458). Before this module the old image closed the
//! listener on exec and the new one bound it again after booting, so for about
//! a second every connect was REFUSED and the dashboard and desktop app showed
//! "disconnected" on every change to amux (Ethan, 2026-10-05).
//!
//! Now the listener fd survives the exec. Connections that arrive while the new
//! image boots wait in the kernel backlog and are accepted once it serves, so a
//! rebuild costs clients a slow request instead of a refused one. Live streams
//! (SSE, terminal websockets) still end with the old image; the client
//! reconnects to a port that never closed.
//!
//! Close-on-exec is cleared only at the moment of the self-adoption exec and
//! set again as soon as the successor adopts the fd, so worker processes the
//! server spawns never inherit the listening socket. The successor adopts only
//! when `AMUX_LISTEN_PID` is its own pid (exec keeps the pid; a child that
//! inherited the env does not) and the fd is a TCP stream socket on the
//! configured port. Anything else is logged and the server binds normally.

use std::net::{SocketAddr, TcpListener};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::sync::OnceLock;

pub const FD_ENV: &str = "AMUX_LISTEN_FD";
pub const PID_ENV: &str = "AMUX_LISTEN_PID";

static LISTENER_FD: OnceLock<RawFd> = OnceLock::new();

/// The inherited listener when the previous image handed one over, else a
/// fresh bind. The second value names which, for the boot log.
pub fn adopt_or_bind(addr: SocketAddr) -> std::io::Result<(TcpListener, &'static str)> {
    let (listener, source) = match adopt(addr.port()) {
        Some(listener) => (listener, "inherited"),
        None => (TcpListener::bind(addr)?, "bound"),
    };
    // std binds with a backlog of 128. Connections queue here for the whole
    // boot of the next image, with ~50 lanes polling, so use tokio's 1024.
    // listen(2) on a listening socket only updates the backlog.
    // SAFETY: plain syscall on an fd this function owns.
    unsafe { libc::listen(listener.as_raw_fd(), 1024) };
    let _ = LISTENER_FD.set(listener.as_raw_fd());
    Ok((listener, source))
}

/// Mark the listener for inheritance and name it in the successor's env. Call
/// immediately before `exec`; call [`exec_failed`] if the exec returns.
pub fn prepare_exec(cmd: &mut std::process::Command) {
    if let Some(&fd) = LISTENER_FD.get() {
        if set_cloexec(fd, false) {
            cmd.env(FD_ENV, fd.to_string())
                .env(PID_ENV, std::process::id().to_string());
        }
    }
}

/// The exec did not happen: stop leaking the listener into future children.
pub fn exec_failed() {
    if let Some(&fd) = LISTENER_FD.get() {
        set_cloexec(fd, true);
    }
}

fn adopt(port: u16) -> Option<TcpListener> {
    let raw = std::env::var(FD_ENV).ok()?;
    let reject = |reason: &str| {
        tracing::warn!(
            verdict = "listener_handoff_rejected",
            reason,
            fd = %raw,
            port,
            "listener handoff not adopted; binding a fresh listener (clients see a refused-connect gap this boot)"
        );
    };
    if std::env::var(PID_ENV).ok() != Some(std::process::id().to_string()) {
        // Inherited env from a parent server, not a handoff to this process.
        return None;
    }
    let Ok(fd) = raw.parse::<RawFd>() else {
        reject("fd is not a number");
        return None;
    };
    if !is_stream_socket(fd) {
        reject("fd is not a TCP stream socket");
        return None;
    }
    set_cloexec(fd, true);
    // SAFETY: the fd was checked above to be an open stream socket, and the
    // pid match means the previous image of THIS process handed it over, so
    // nothing else owns it.
    let listener = unsafe { TcpListener::from_raw_fd(fd) };
    match listener.local_addr() {
        Ok(local) if local.port() == port => Some(listener),
        Ok(_) => {
            // Ours, but on the wrong port: drop (close) it and bind afresh.
            reject("fd listens on a different port");
            None
        }
        Err(_) => {
            reject("fd has no local address");
            None
        }
    }
}

fn is_stream_socket(fd: RawFd) -> bool {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SO_TYPE rather than SO_ACCEPTCONN: macOS answers ENOPROTOOPT for the
    // latter. Listening state needs no check because adopt_or_bind calls
    // listen(2) on whatever it keeps.
    // SAFETY: getsockopt writes at most `len` bytes into `value`; an invalid
    // or non-socket fd returns -1 without touching it.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut value as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    rc == 0 && value == libc::SOCK_STREAM
}

fn set_cloexec(fd: RawFd, on: bool) -> bool {
    // SAFETY: F_GETFD/F_SETFD only read and write the descriptor flags.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            return false;
        }
        let next = if on {
            flags | libc::FD_CLOEXEC
        } else {
            flags & !libc::FD_CLOEXEC
        };
        libc::fcntl(fd, libc::F_SETFD, next) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cloexec(fd: RawFd) -> bool {
        unsafe { libc::fcntl(fd, libc::F_GETFD) & libc::FD_CLOEXEC != 0 }
    }

    #[test]
    fn a_tcp_socket_is_recognised_and_a_plain_fd_is_not() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(is_stream_socket(l.as_raw_fd()));
        let f = std::fs::File::open("/dev/null").unwrap();
        assert!(!is_stream_socket(f.as_raw_fd()));
    }

    #[test]
    fn cloexec_is_cleared_for_the_exec_and_restored_after() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let fd = l.as_raw_fd();
        assert!(cloexec(fd), "std sockets start close-on-exec");
        assert!(set_cloexec(fd, false));
        assert!(!cloexec(fd));
        assert!(set_cloexec(fd, true));
        assert!(cloexec(fd));
    }
}
