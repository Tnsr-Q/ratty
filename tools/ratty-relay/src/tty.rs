//! Unix tty plumbing shared by `host` and `watch`: raw mode with restore on
//! drop, winsize queries, and the SIGWINCH self-pipe.

#![cfg(unix)]

use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

/// Puts a tty into raw mode; restores the saved termios on drop. No-op when
/// the fd is not a tty.
pub struct RawGuard {
    fd: RawFd,
    saved: Option<libc::termios>,
}

impl RawGuard {
    pub fn new(fd: RawFd) -> Self {
        // SAFETY: isatty/tcgetattr/cfmakeraw/tcsetattr on a caller-owned fd.
        unsafe {
            if libc::isatty(fd) != 1 {
                return Self { fd, saved: None };
            }
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) != 0 {
                return Self { fd, saved: None };
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return Self { fd, saved: None };
            }
            Self {
                fd,
                saved: Some(saved),
            }
        }
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        if let Some(saved) = self.saved {
            // SAFETY: restoring the termios captured in `new`.
            unsafe {
                libc::tcsetattr(self.fd, libc::TCSANOW, &saved);
            }
        }
    }
}

/// The (cols, rows) of the tty behind `fd`, or `None` off-tty.
pub fn winsize(fd: RawFd) -> Option<(u16, u16)> {
    // SAFETY: TIOCGWINSZ into a zeroed winsize.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            Some((ws.ws_col, ws.ws_row))
        } else {
            None
        }
    }
}

static SIGWINCH_PIPE_W: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_sigwinch(_: libc::c_int) {
    let fd = SIGWINCH_PIPE_W.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: write(2) is async-signal-safe; one byte wakes the reader.
        unsafe {
            libc::write(fd, [1u8].as_ptr().cast(), 1);
        }
    }
}

/// Installs a SIGWINCH handler and returns the read end of a self-pipe; one
/// byte arrives per resize. Call once.
pub fn sigwinch_pipe() -> io::Result<RawFd> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: pipe(2) into a two-slot array; both ends owned here.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        SIGWINCH_PIPE_W.store(fds[1], Ordering::Relaxed);
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_sigwinch as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = libc::SA_RESTART;
        if libc::sigaction(libc::SIGWINCH, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(fds[0])
}

/// Blocking read of one wake-up byte from the self-pipe.
pub fn wait_sigwinch(pipe_r: RawFd) -> bool {
    let mut byte = [0u8; 1];
    // SAFETY: blocking read(2) on the pipe's read end.
    unsafe { libc::read(pipe_r, byte.as_mut_ptr().cast(), 1) == 1 }
}

/// Read straight off the descriptor, bypassing std's buffering.
///
/// The input pump must be able to ask "is there more?" with `poll(2)`, and a
/// `BufReader` would answer for the kernel: bytes already sitting in std's
/// buffer are invisible to `poll`, so a withheld 778 prefix could sit behind
/// input that had in fact already arrived.
pub fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        // SAFETY: read(2) into a caller-owned buffer of the length given.
        let read = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if read >= 0 {
            return Ok(read as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Wait for `fd` to become readable, up to `timeout`. `Ok(false)` is the
/// timeout — the caller's cue to release anything it is withholding.
pub fn poll_readable(fd: RawFd, timeout: Duration) -> io::Result<bool> {
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    loop {
        let mut fds = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll(2) over a single caller-owned descriptor.
        let ready = unsafe { libc::poll(&mut fds, 1, millis) };
        if ready >= 0 {
            return Ok(ready > 0);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}
