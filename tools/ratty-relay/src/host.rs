//! `ratty-relay host` — the script(1)-style interposer.
//!
//! Runs under `ratty -e ratty-relay host [--listen ADDR] -- zsh`: allocates
//! an inner PTY, execs the shell, pumps both directions, and tees the
//! output direction — verbatim to ratty's parser (stdout), gated through
//! the control-silent filter into the WebSocket fan-out. The input
//! direction (keystrokes, parser auto-replies, 778 replies) is never teed.
//! SIGWINCH mirrors the outer winsize inward and emits a `resize` control
//! frame.

#![cfg(unix)]

use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{Context, Result, bail};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::gate::Gate;
use crate::server::Fanout;
use crate::tty;

pub struct HostArgs {
    pub listen: String,
    pub session: String,
    pub ring_cap: usize,
    pub command: Vec<String>,
}

pub fn run(args: HostArgs) -> Result<i32> {
    let command = resolve_command(args.command)?;

    let stdin_fd = std::io::stdin().as_raw_fd();
    let (cols, rows) = tty::winsize(stdin_fd).unwrap_or_else(|| {
        eprintln!("[relay] stdin is not a tty; assuming 80x24");
        (80, 24)
    });

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open the inner PTY")?;

    let mut builder = CommandBuilder::new(&command[0]);
    builder.args(&command[1..]);
    if let Ok(cwd) = std::env::current_dir() {
        builder.cwd(cwd);
    }
    let mut child = pair
        .slave
        .spawn_command(builder)
        .with_context(|| format!("failed to spawn {:?}", command[0]))?;
    drop(pair.slave);

    let fanout = Fanout::new(args.session, args.ring_cap, cols, rows);
    let bound = fanout.listen(&args.listen)?;
    eprintln!("[relay] listening on ws://{bound} — command: {command:?}\r");

    // Raw outer tty so every byte passes through unmodified, like script(1).
    let _raw = tty::RawGuard::new(stdin_fd);

    let master = pair.master;
    let mut master_reader = master
        .try_clone_reader()
        .context("failed to clone the PTY reader")?;
    let mut master_writer = master
        .take_writer()
        .context("failed to take the PTY writer")?;

    let primary_gone = Arc::new(AtomicBool::new(false));

    // Input pump: ratty (our stdin) → inner PTY. Never teed. EOF here means
    // ratty itself died — teardown skips the banner on that path.
    let input_gone = Arc::clone(&primary_gone);
    thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 16 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if master_writer.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = master_writer.flush();
                }
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        input_gone.store(true, Ordering::Relaxed);
    });

    // SIGWINCH: mirror the outer winsize inward + resize control frame.
    {
        let fanout = Arc::clone(&fanout);
        let pipe_r = tty::sigwinch_pipe().context("failed to install SIGWINCH handler")?;
        thread::spawn(move || {
            while tty::wait_sigwinch(pipe_r) {
                if let Some((cols, rows)) = tty::winsize(stdin_fd) {
                    let _ = master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                    fanout.resize(cols, rows);
                }
            }
        });
    }

    // Output pump: inner PTY → verbatim to stdout (ratty's parser) and
    // gated into the fan-out. Runs on the main thread until child EOF.
    let mut gate = Gate::new();
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match master_reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                stdout
                    .write_all(&buf[..n])
                    .context("failed to forward to the primary")?;
                stdout.flush().ok();
                let segs = gate.push(&buf[..n]);
                if !segs.is_empty() {
                    fanout.broadcast(&segs);
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    let status = child.wait().map(|s| s.exit_code() as i32).unwrap_or(1);
    let stats = gate.stats;
    // Primary death (stdin EOF/SIGHUP path) skips the banner; a normal
    // session end announces itself to spectators.
    fanout.end("session-ended", !primary_gone.load(Ordering::Relaxed));
    eprintln!(
        "\r\n[relay] session ended (excised: {} control-plane, {} query-plane, {} oversize; tok stripped: {})\r",
        stats.excised_control, stats.excised_778, stats.excised_overflow, stats.stripped_tok
    );
    Ok(status)
}

/// The command vector: trailing args (after `--`), the `--cmd` fallback, or
/// `$SHELL`. The cli-seam test pins how clap delivers the trailing form.
pub fn resolve_command(mut command: Vec<String>) -> Result<Vec<String>> {
    // clap's trailing capture may retain the literal `--` separator.
    if command.first().map(String::as_str) == Some("--") {
        command.remove(0);
    }
    if command.is_empty() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        command = vec![shell];
    }
    if command[0].is_empty() {
        bail!("empty command");
    }
    Ok(command)
}
