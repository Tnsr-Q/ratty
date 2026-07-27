//! `ratty-relay` — the tier-B spectator relay (ratty #46, map #42).
//!
//! A control-silent output tee: `host` interposes between ratty and the
//! shell, teeing the output direction into a WebSocket fan-out with the
//! crate's entire control-plane and execution-control classes excised (the
//! `#[path]`-shared predicates in `src/osc.rs` — never a re-typed list),
//! all OSC 778 dropped, and `tok=` stripped from forwarded 777. `watch` is
//! the read-only native spectator. Spectators cannot write: the server
//! ignores every inbound frame and `watch` sinks stdin.
//!
//! Stage 2 adds the presence engine: an OSC 778 poll of the primary's
//! `state.presence`, injected only at sequence boundaries and with its own
//! replies excised from the input direction, synthesizing gated presence for
//! spectators — fresh rows only, ids rewritten `r<ns>.<id>`, every join a
//! `replace=true`, and fan-out only, so replayed history can never resurrect
//! a mirrored row. Spectator instances are connection-ephemeral: they tear
//! their mirrors down on any socket close.
//!
//! Design: `docs/research/relay-design.md` (the locked #45 resolution).

use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

#[allow(dead_code)] // Shared wire module; the relay uses the classification half.
#[path = "../../../src/osc.rs"]
mod osc;

#[allow(dead_code)] // Shared wire module; the relay uses the 778 client half.
#[path = "../../../src/query.rs"]
mod query;

mod frames;
mod gate;
#[cfg(unix)]
mod host;
mod poll;
mod presence;
mod ring;
mod server;
#[cfg(unix)]
mod tty;
mod watch;

/// Default fan-out bind: localhost only. Remote hardening (bearer token,
/// TLS) is deferred per the design.
const DEFAULT_LISTEN: &str = "127.0.0.1:7877";

/// Default replay-ring capacity. A session that scrolls past this without
/// a full clear degrades late-joins to a blank-anchor live tail.
const DEFAULT_RING_CAP: usize = 4 * 1024 * 1024;

/// Default `state.presence` poll cadence. The design's figure; it also sets
/// the sampling floor — a join/leave pair inside one interval is invisible,
/// and cursor motion decimates to this rate.
const DEFAULT_POLL_MS: u64 = 250;

#[derive(Parser)]
#[command(
    name = "ratty-relay",
    version,
    about = "Tier-B spectator relay for the Ratty terminal (read-only fan-out)"
)]
struct Cli {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Subcommand)]
enum Mode {
    /// Host a session: interpose on the shell, tee gated output to spectators.
    #[command(trailing_var_arg = true)]
    Host {
        /// Address for the WebSocket fan-out.
        #[arg(long, default_value = DEFAULT_LISTEN)]
        listen: String,
        /// Session name announced in the `hello` frame.
        #[arg(long, default_value = "ratty")]
        session: String,
        /// Replay-ring capacity in bytes.
        #[arg(long, default_value_t = DEFAULT_RING_CAP)]
        ring_cap: usize,
        /// `state.presence` poll interval in milliseconds. `0` disables the
        /// presence engine entirely — stage-1 behaviour, spectators see no
        /// presence at all, lawfully.
        #[arg(long, default_value_t = DEFAULT_POLL_MS)]
        poll_ms: u64,
        /// Fallback for embeddings that cannot pass trailing args: a command
        /// string split on whitespace (no quoting).
        #[arg(long, conflicts_with = "command")]
        cmd: Option<String>,
        /// Command to run (defaults to $SHELL). Everything after `--`.
        #[arg(allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Watch a hosted session read-only (run under `ratty -e`).
    Watch {
        /// WebSocket URL, e.g. ws://127.0.0.1:7877
        url: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.mode {
        Mode::Host {
            listen,
            session,
            ring_cap,
            poll_ms,
            cmd,
            command,
        } => run_host(listen, session, ring_cap, poll_ms, cmd, command),
        Mode::Watch { url } => watch::run(&url),
    };
    match result {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(err) => {
            eprintln!("[relay] error: {err:#}");
            ExitCode::from(1)
        }
    }
}

#[cfg(unix)]
fn run_host(
    listen: String,
    session: String,
    ring_cap: usize,
    poll_ms: u64,
    cmd: Option<String>,
    command: Vec<String>,
) -> anyhow::Result<i32> {
    let command = match cmd {
        Some(line) => line.split_whitespace().map(String::from).collect(),
        None => command,
    };
    host::run(host::HostArgs {
        listen,
        session,
        ring_cap,
        poll: (poll_ms > 0).then(|| Duration::from_millis(poll_ms)),
        command: host::resolve_command(command)?,
    })
}

#[cfg(not(unix))]
fn run_host(
    _listen: String,
    _session: String,
    _ring_cap: usize,
    _poll_ms: u64,
    _cmd: Option<String>,
    _command: Vec<String>,
) -> anyhow::Result<i32> {
    anyhow::bail!("`host` needs a unix PTY; only `watch` is available on this platform")
}
