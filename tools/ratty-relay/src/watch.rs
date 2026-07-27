//! `ratty-relay watch` — the native spectator client.
//!
//! Runs under `ratty -e ratty-relay watch URL`: prints received binary
//! frames to stdout (into that ratty's parser) and reads-and-discards
//! stdin, sinking keystrokes, the spectator parser's auto-replies, and 778
//! acks. No byte ever travels upstream — read-only holds mechanically.

use std::io::{Read, Write};
use std::thread;

use anyhow::{Context, Result};
use tungstenite::Message;

use crate::frames::Control;

pub fn run(url: &str) -> Result<i32> {
    let (mut ws, _response) =
        tungstenite::connect(url).with_context(|| format!("failed to connect to {url}"))?;

    // Sink stdin: keystrokes and the spectator parser's own replies go
    // nowhere. Raw mode stops the outer pty from echoing keystrokes over
    // the mirrored view.
    #[cfg(unix)]
    let _raw = {
        use std::os::fd::AsRawFd;
        crate::tty::RawGuard::new(std::io::stdin().as_raw_fd())
    };
    thread::spawn(|| {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 {
                break;
            }
        }
    });

    let mut stdout = std::io::stdout().lock();
    loop {
        let msg = match ws.read() {
            Ok(msg) => msg,
            Err(_) => {
                eprintln!("\r\n[relay] connection closed\r");
                return Ok(0);
            }
        };
        match msg {
            Message::Binary(bytes) => {
                stdout.write_all(&bytes).context("stdout write failed")?;
                stdout.flush().ok();
            }
            Message::Text(text) => match Control::from_json(&text) {
                Some(Control::Hello {
                    session,
                    cols,
                    rows,
                    degraded,
                    ..
                }) => {
                    warn_geometry(cols, rows);
                    if degraded {
                        eprintln!(
                            "[relay] joined '{session}' degraded: no replayable history — live tail from a blank screen\r"
                        );
                    } else {
                        eprintln!("[relay] joined '{session}' ({cols}x{rows})\r");
                    }
                }
                Some(Control::Resize { cols, rows }) => {
                    eprintln!("[relay] primary resized to {cols}x{rows}\r");
                    warn_geometry(cols, rows);
                }
                Some(Control::End { reason }) => {
                    eprintln!("\r\n[relay] session ended ({reason})\r");
                    return Ok(0);
                }
                Some(Control::ResetNotice)
                | Some(Control::SnapshotBegin)
                | Some(Control::SnapshotEnd)
                | None => {}
            },
            Message::Close(_) => {
                eprintln!("\r\n[relay] connection closed\r");
                return Ok(0);
            }
            _ => {}
        }
    }
}

/// The byte stream encodes the primary's grid; a mismatched spectator sees
/// wrapped or clipped output. The skeleton warns; letterboxing is deferred.
fn warn_geometry(cols: u16, rows: u16) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if let Some((own_cols, own_rows)) = crate::tty::winsize(std::io::stdin().as_raw_fd())
            && (own_cols, own_rows) != (cols, rows)
        {
            eprintln!(
                "[relay] geometry mismatch: primary {cols}x{rows}, this terminal {own_cols}x{own_rows}\r"
            );
        }
    }
    #[cfg(not(unix))]
    let _ = (cols, rows);
}
