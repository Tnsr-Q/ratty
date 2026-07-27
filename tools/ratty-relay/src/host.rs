//! `ratty-relay host` — the script(1)-style interposer.
//!
//! Runs under `ratty -e ratty-relay host [--listen ADDR] -- zsh`: allocates
//! an inner PTY, execs the shell, pumps both directions, and tees the
//! output direction — verbatim to ratty's parser (stdout), gated through
//! the control-silent filter into the WebSocket fan-out. SIGWINCH mirrors
//! the outer winsize inward and emits a `resize` control frame.
//!
//! The input direction is never teed. Since stage 2 it is *demultiplexed*:
//! ratty answers the relay's own OSC 778 queries by writing into the PTY it
//! owns, so those replies arrive here as input and are lifted out before the
//! shell can see them (`protocols/query.md:45-48`). Keystrokes, the parser's
//! auto-replies, and any other client's 778 traffic pass through untouched.

#![cfg(unix)]

use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Mutex, mpsc::Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::gate::Seg;
use crate::poll::{Downstream, Driver, InputDemux, OurTokens};
use crate::server::Fanout;
use crate::tty;

pub struct HostArgs {
    pub listen: String,
    pub session: String,
    pub ring_cap: usize,
    /// `state.presence` poll cadence, or `None` to run stage-1 silent — no
    /// queries injected, no presence synthesized, the input direction
    /// forwarded byte-for-byte with no demux in the way.
    pub poll: Option<Duration>,
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
    let master_writer = master
        .take_writer()
        .context("failed to take the PTY writer")?;

    let primary_gone = Arc::new(AtomicBool::new(false));
    let shutdown = Arc::new(AtomicBool::new(false));
    let reset_epoch = Arc::new(AtomicU64::new(0));
    let tokens = Arc::new(Mutex::new(OurTokens::default()));
    let down = Arc::new(Mutex::new(Downstream::new()));
    let (reply_tx, reply_rx) = channel::<String>();
    // 778 frames the demux withheld past their hold bound and dropped. Owned
    // by the input pump, surfaced in the closing status line so a session
    // that hit the fail-closed path says so.
    let stalled_replies = Arc::new(AtomicU64::new(0));

    // Input pump: ratty (our stdin) → inner PTY, less the relay's own 778
    // replies. EOF here means ratty itself died — teardown skips the banner
    // on that path.
    {
        let input_gone = Arc::clone(&primary_gone);
        let demux = args.poll.map(|_| InputDemux::new());
        let tokens = Arc::clone(&tokens);
        let stalled = Arc::clone(&stalled_replies);
        thread::spawn(move || {
            pump_input(stdin_fd, master_writer, demux, &tokens, &reply_tx, &stalled);
            input_gone.store(true, Ordering::Relaxed);
        });
    }

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

    // The presence engine (stage 2). Off entirely when `--poll-ms 0`.
    let driver = args.poll.map(|interval| {
        let driver = Driver {
            down: Arc::clone(&down),
            fanout: Arc::clone(&fanout),
            replies: reply_rx,
            tokens: Arc::clone(&tokens),
            shutdown: Arc::clone(&shutdown),
            reset_epoch: Arc::clone(&reset_epoch),
            interval,
        };
        thread::spawn(move || driver.run())
    });

    // Output pump: inner PTY → verbatim to stdout (ratty's parser) and
    // gated into the fan-out. Runs on the main thread until child EOF.
    let mut buf = [0u8; 16 * 1024];
    loop {
        match master_reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let segs = down
                    .lock()
                    .expect("downstream lock")
                    .forward(&buf[..n])
                    .context("failed to forward to the primary")?;
                if segs.is_empty() {
                    continue;
                }
                // A 777 `reset` clears every roster; the presence engine
                // drops its model rather than emitting leaves for rows the
                // forwarded bytes already removed.
                if segs.iter().any(|seg| {
                    matches!(
                        seg,
                        Seg::Anchor {
                            roster_reset: true,
                            ..
                        }
                    )
                }) {
                    reset_epoch.fetch_add(1, Ordering::Relaxed);
                }
                fanout.broadcast(&segs);
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    let status = child.wait().map(|s| s.exit_code() as i32).unwrap_or(1);
    shutdown.store(true, Ordering::Relaxed);
    let poll_stats = driver.and_then(|handle| handle.join().ok());
    let gate_stats = down.lock().expect("downstream lock").stats();

    // Teardown emits synthesized leaves for every mirrored row before the
    // banner. Primary death (stdin EOF/SIGHUP path) skips the banner; a
    // normal session end announces itself to spectators.
    fanout.end("session-ended", !primary_gone.load(Ordering::Relaxed));
    eprintln!(
        "\r\n[relay] session ended (excised: {} control-plane, {} query-plane, {} oversize; tok stripped: {})\r",
        gate_stats.excised_control,
        gate_stats.excised_778,
        gate_stats.excised_overflow,
        gate_stats.stripped_tok
    );
    if let Some(stats) = poll_stats {
        eprintln!(
            "[relay] presence ({} walks, {} pages, {} timeouts, {} rejects, {} stale, \
             {} divergences, {} stalled replies): {} mirrored, {} restated, {} left, \
             {} expired, {} truncated, {} skipped, {} forgotten\r",
            stats.walks,
            stats.pages,
            stats.timeouts,
            stats.rejects,
            stats.stale,
            stats.divergences,
            stalled_replies.load(Ordering::Relaxed),
            stats.mirror.joined,
            stats.mirror.reissued,
            stats.mirror.left,
            stats.mirror.expired,
            stats.mirror.truncated,
            stats.mirror.skipped_id + stats.mirror.skipped_collision + stats.mirror.skipped_cap,
            stats.mirror.forgotten,
        );
    }
    Ok(status)
}

/// Forward the input direction to the shell, lifting out the relay's own 778
/// replies. Withheld bytes are released on their hold bound, so a bare ESC
/// keypress — indistinguishable from the start of a 778 frame until the next
/// byte arrives — reaches the shell promptly instead of waiting on one.
fn pump_input(
    stdin_fd: std::os::fd::RawFd,
    mut sink: Box<dyn Write + Send>,
    mut demux: Option<InputDemux>,
    tokens: &Arc<Mutex<OurTokens>>,
    replies: &Sender<String>,
    stalled: &AtomicU64,
) {
    let mine = |token: &str| {
        tokens
            .lock()
            .map(|tokens| tokens.contains(token))
            .unwrap_or(false)
    };
    let mut buf = [0u8; 16 * 1024];
    loop {
        if let Some(demux) = demux.as_mut()
            && let Some(hold) = demux.hold_deadline(Instant::now())
        {
            let ready = if hold.is_zero() {
                false
            } else {
                match tty::poll_readable(stdin_fd, hold) {
                    Ok(ready) => ready,
                    Err(_) => return,
                }
            };
            if !ready {
                let held = demux.flush();
                stalled.store(demux.stalled, Ordering::Relaxed);
                if !held.is_empty() && sink.write_all(&held).is_err() {
                    return;
                }
                let _ = sink.flush();
                continue;
            }
        }
        let read = match tty::read_fd(stdin_fd, &mut buf) {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        let chunk = &buf[..read];
        match demux.as_mut() {
            None => {
                if sink.write_all(chunk).is_err() {
                    return;
                }
            }
            Some(demux) => {
                let feed = demux.push(chunk, Instant::now(), &mine);
                stalled.store(demux.stalled, Ordering::Relaxed);
                if !feed.forward.is_empty() && sink.write_all(&feed.forward).is_err() {
                    return;
                }
                for frame in feed.frames {
                    // A dead driver is not a reason to stop forwarding the
                    // user's keystrokes; the replies simply go nowhere.
                    let _ = replies.send(frame);
                }
            }
        }
        let _ = sink.flush();
    }
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
