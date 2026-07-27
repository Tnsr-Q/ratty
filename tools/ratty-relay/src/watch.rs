//! `ratty-relay watch` — the native spectator client.
//!
//! Runs under `ratty -e ratty-relay watch URL`: prints received binary
//! frames to stdout (into that ratty's parser) and reads-and-discards
//! stdin, sinking keystrokes, the spectator parser's auto-replies, and 778
//! acks. No byte ever travels upstream — read-only holds mechanically.
//!
//! Spectator instances are **connection-ephemeral** (stage 2). The client
//! tracks every mirrored presence row the relay has announced and, on **any**
//! socket close — clean `end`, drop, or the relay dying outright — writes
//! `user.leave` / `note.remove` for each before exiting. Mirror teardown
//! therefore rides the spectator side of the link and survives every
//! relay-side failure mode; a mirrored row cannot outlive the connection it
//! arrived on.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::thread;

use anyhow::{Context, Result};
use tungstenite::Message;

use crate::frames::Control;
use crate::presence::MirrorId;

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

    let mut mirrored: BTreeSet<MirrorId> = BTreeSet::new();
    let mut stdout = std::io::stdout().lock();
    loop {
        let msg = match ws.read() {
            Ok(msg) => msg,
            Err(_) => {
                eprintln!("\r\n[relay] connection closed\r");
                break;
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
                Some(Control::PresenceMirror { add, remove, clear }) => {
                    if clear {
                        // The primary reset; the forwarded bytes already
                        // cleared this instance's rosters.
                        mirrored.clear();
                    }
                    for id in remove {
                        mirrored.remove(&id);
                    }
                    mirrored.extend(add);
                }
                Some(Control::End { reason }) => {
                    eprintln!("\r\n[relay] session ended ({reason})\r");
                    break;
                }
                Some(Control::ResetNotice)
                | Some(Control::SnapshotBegin)
                | Some(Control::SnapshotEnd)
                | None => {}
            },
            Message::Close(_) => {
                eprintln!("\r\n[relay] connection closed\r");
                break;
            }
            _ => {}
        }
    }

    tear_down_mirrors(&mut stdout, &mirrored);
    Ok(0)
}

/// Every mirrored row this spectator was told it holds, removed from its own
/// namespace-0 roster. Rows the relay already cleared answer `unknown-id` —
/// harmless noise in this instance's error ring, and the conservative side of
/// the trade: an over-count costs a rejected command, an under-count would
/// leave a mirrored row standing after the link is gone.
fn tear_down_mirrors(out: &mut impl Write, mirrored: &BTreeSet<MirrorId>) {
    if mirrored.is_empty() {
        return;
    }
    for id in mirrored {
        let _ = out.write_all(id.removal().to_wire().as_bytes());
    }
    let _ = out.flush();
    eprintln!(
        "[relay] released {} mirrored presence rows\r",
        mirrored.len()
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teardown_releases_every_tracked_row() {
        let mut out: Vec<u8> = Vec::new();
        let mirrored = BTreeSet::from([MirrorId::participant("r0.ann"), MirrorId::note("r0.n1")]);
        tear_down_mirrors(&mut out, &mirrored);
        let text = String::from_utf8(out).expect("wire bytes are utf-8");
        assert!(text.contains("ratty:user.leave;id=r0.ann"), "{text:?}");
        assert!(text.contains("ratty:note.remove;id=r0.n1"), "{text:?}");
    }

    #[test]
    fn teardown_with_nothing_tracked_writes_nothing() {
        let mut out: Vec<u8> = Vec::new();
        tear_down_mirrors(&mut out, &BTreeSet::new());
        assert!(out.is_empty());
    }

    /// The tracked set is what the relay announced, folded exactly as the
    /// read loop folds it.
    #[test]
    fn mirror_frames_fold_into_the_tracked_set() {
        let mut mirrored: BTreeSet<MirrorId> = BTreeSet::new();
        let frames = [
            Control::PresenceMirror {
                add: vec![MirrorId::participant("r0.ann"), MirrorId::note("r0.n1")],
                remove: Vec::new(),
                clear: false,
            },
            Control::PresenceMirror {
                add: Vec::new(),
                remove: vec![MirrorId::note("r0.n1")],
                clear: false,
            },
        ];
        for frame in frames {
            if let Control::PresenceMirror { add, remove, clear } = frame {
                if clear {
                    mirrored.clear();
                }
                for id in remove {
                    mirrored.remove(&id);
                }
                mirrored.extend(add);
            }
        }
        assert_eq!(mirrored, BTreeSet::from([MirrorId::participant("r0.ann")]));

        // A reset drops everything: the forwarded bytes cleared the rosters.
        if let Control::PresenceMirror { clear: true, .. } = (Control::PresenceMirror {
            add: Vec::new(),
            remove: Vec::new(),
            clear: true,
        }) {
            mirrored.clear();
        }
        assert!(mirrored.is_empty());
    }
}
