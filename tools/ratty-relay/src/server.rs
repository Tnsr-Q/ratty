//! WebSocket fan-out: one accept loop, one thread per spectator.
//!
//! Invariants from the design ("Fan-out wire"):
//!
//! - the server ignores every inbound client frame — no code path from
//!   spectator to the shell exists;
//! - per-client bounded queues; a full queue drops that client into the
//!   disconnect-and-rejoin path, never backpressuring the pump;
//! - a joining client receives `hello`, `snapshot-begin`, the ring bytes, the
//!   synthesized presence section, then `snapshot-end` under the same lock
//!   the broadcast takes, so live frames can never interleave before the
//!   snapshot completes;
//! - synthesized presence is **fan-out-only**: it never enters the ring, so
//!   replayed history can never resurrect a mirrored row.

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tungstenite::{Message, WebSocket};

use crate::frames::Control;
use crate::gate::Seg;
use crate::presence::{Cmd, MirrorId, Snapshot};
use crate::ring::Ring;

/// Outgoing queue depth per spectator. The snapshot occupies a couple of
/// slots; a spectator that falls this many messages behind is dropped.
const CLIENT_QUEUE: usize = 1024;

enum Outgoing {
    Bin(Vec<u8>),
    Text(String),
    Close,
}

struct Client {
    tx: SyncSender<Outgoing>,
    id: u64,
}

struct Shared {
    ring: Ring,
    clients: Vec<Client>,
    cols: u16,
    rows: u16,
    seq: u64,
    ended: bool,
    /// The presence engine's live model, republished as it changes. Held
    /// here — rather than shared with the poll driver — so a joining client
    /// renders it under this same lock with no chance of a lock inversion,
    /// and so freshness is re-evaluated at *join* time rather than at
    /// publish time.
    presence: Snapshot,
}

pub struct Fanout {
    shared: Arc<Mutex<Shared>>,
    session: String,
    next_client: AtomicU64,
}

impl Fanout {
    pub fn new(session: String, ring_cap: usize, cols: u16, rows: u16) -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(Mutex::new(Shared {
                ring: Ring::new(ring_cap),
                clients: Vec::new(),
                cols,
                rows,
                seq: 0,
                ended: false,
                presence: Snapshot::default(),
            })),
            session,
            next_client: AtomicU64::new(1),
        })
    }

    /// Bind and start the accept loop. Returns the bound address.
    pub fn listen(self: &Arc<Self>, addr: &str) -> Result<std::net::SocketAddr> {
        let listener = TcpListener::bind(addr).with_context(|| format!("failed to bind {addr}"))?;
        let bound = listener.local_addr()?;
        let fanout = Arc::clone(self);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let fanout = Arc::clone(&fanout);
                thread::spawn(move || {
                    let _ = fanout.serve_client(stream);
                });
            }
        });
        Ok(bound)
    }

    /// Broadcast gated segments from the pump. Never blocks on a slow
    /// spectator: a full queue marks that client dropped.
    pub fn broadcast(&self, segs: &[Seg]) {
        let mut shared = self.shared.lock().expect("fanout lock");
        for seg in segs {
            shared.ring.push(seg);
            let bytes = match seg {
                Seg::Data(bytes) | Seg::Anchor { bytes, .. } => bytes.clone(),
            };
            shared.seq += 1;
            Self::send_all(&mut shared, Outgoing::Bin(bytes));
            if matches!(seg, Seg::Anchor { .. }) {
                Self::send_all(&mut shared, Outgoing::Text(Control::ResetNotice.to_json()));
            }
            // The forwarded `reset` bytes just cleared every spectator's
            // rosters (`protocols/presence.md:162-164`), so the mirror the
            // relay is tracking on their behalf goes with them — no
            // synthetic leaves, which would only be `unknown-id` noise.
            if matches!(
                seg,
                Seg::Anchor {
                    roster_reset: true,
                    ..
                }
            ) {
                shared.presence = Snapshot::default();
                Self::send_all(
                    &mut shared,
                    Outgoing::Text(
                        Control::PresenceMirror {
                            add: Vec::new(),
                            remove: Vec::new(),
                            clear: true,
                        }
                        .to_json(),
                    ),
                );
            }
        }
    }

    /// Advance the presence engine: fan out one batch of synthesized
    /// commands **and** install the model a late joiner is handed, under a
    /// single lock.
    ///
    /// The atomicity is the point. Split across two acquisitions, a spectator
    /// whose `serve_client` won the lock in between would be handed a model
    /// that disagrees with the byte stream in both directions — a departed
    /// row re-joined into its roster from the stale snapshot, or a new row it
    /// was registered too late to receive and that no later walk restates
    /// (a steady row's deadline does not move, so nothing re-emits it). One
    /// acquisition makes both impossible.
    ///
    /// Deliberately does **not** touch the ring: synthesized presence is
    /// generated from the poll model, never replayed, so history can never
    /// resurrect a row.
    ///
    /// Mirror-set frames bracket the bytes conservatively — additions
    /// announced before the join lands, removals after it clears — so a
    /// spectator's tracked set is never smaller than what it applied.
    pub fn presence(&self, cmds: &[Cmd], snapshot: Snapshot) {
        let mut added: Vec<MirrorId> = Vec::new();
        let mut removed: Vec<MirrorId> = Vec::new();
        let mut bytes = Vec::new();
        for cmd in cmds {
            bytes.extend_from_slice(cmd.to_wire().as_bytes());
            match cmd.mirror_delta() {
                Some((true, id)) => added.push(id),
                Some((false, id)) => removed.push(id),
                None => {}
            }
        }

        let mut shared = self.shared.lock().expect("fanout lock");
        if shared.ended {
            return;
        }
        shared.presence = snapshot;
        if bytes.is_empty() {
            return;
        }
        if !added.is_empty() {
            let frame = Control::PresenceMirror {
                add: added,
                remove: Vec::new(),
                clear: false,
            };
            Self::send_all(&mut shared, Outgoing::Text(frame.to_json()));
        }
        Self::send_all(&mut shared, Outgoing::Bin(bytes));
        if !removed.is_empty() {
            let frame = Control::PresenceMirror {
                add: Vec::new(),
                remove: removed,
                clear: false,
            };
            Self::send_all(&mut shared, Outgoing::Text(frame.to_json()));
        }
    }

    /// The primary's grid changed.
    pub fn resize(&self, cols: u16, rows: u16) {
        let mut shared = self.shared.lock().expect("fanout lock");
        shared.cols = cols;
        shared.rows = rows;
        let frame = Control::Resize { cols, rows }.to_json();
        Self::send_all(&mut shared, Outgoing::Text(frame));
    }

    /// Session teardown: synthesized leaves for every mirrored row, the
    /// optional plain-text banner, the `end` control frame, then all sockets
    /// close.
    ///
    /// The leaves are rendered from the stored model rather than asked of the
    /// poll driver, so they still go out when the driver is wedged or gone —
    /// and spectators tear their own mirrors down on the socket close
    /// regardless, which is what makes the relay's own death survivable.
    pub fn end(&self, reason: &str, banner: bool) {
        let mut shared = self.shared.lock().expect("fanout lock");
        if shared.ended {
            return;
        }
        shared.ended = true;
        let mirrored = shared.presence.live_ids();
        if !mirrored.is_empty() {
            let bytes: Vec<u8> = mirrored
                .iter()
                .flat_map(|id| id.removal().to_wire().into_bytes())
                .collect();
            Self::send_all(&mut shared, Outgoing::Bin(bytes));
            let frame = Control::PresenceMirror {
                add: Vec::new(),
                remove: mirrored,
                clear: false,
            };
            Self::send_all(&mut shared, Outgoing::Text(frame.to_json()));
            shared.presence = Snapshot::default();
        }
        if banner {
            Self::send_all(
                &mut shared,
                Outgoing::Bin(b"\r\n[relay] session ended\r\n".to_vec()),
            );
        }
        let frame = Control::End {
            reason: reason.to_string(),
        }
        .to_json();
        Self::send_all(&mut shared, Outgoing::Text(frame));
        Self::send_all(&mut shared, Outgoing::Close);
        shared.clients.clear();
    }

    fn send_all(shared: &mut Shared, msg: Outgoing) {
        // Clone-per-client of the payload keeps the common (small-frame)
        // path simple; large frames are rare (snapshots go direct).
        let mut dropped: Vec<u64> = Vec::new();
        for client in &shared.clients {
            let cloned = match &msg {
                Outgoing::Bin(b) => Outgoing::Bin(b.clone()),
                Outgoing::Text(t) => Outgoing::Text(t.clone()),
                Outgoing::Close => Outgoing::Close,
            };
            match client.tx.try_send(cloned) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                    dropped.push(client.id);
                }
            }
        }
        shared.clients.retain(|c| !dropped.contains(&c.id));
    }

    fn serve_client(self: Arc<Self>, stream: TcpStream) -> Result<()> {
        stream.set_nodelay(true).ok();
        let ws = tungstenite::accept(stream).context("websocket handshake failed")?;
        // Timeout applies after the handshake: the client pump polls reads
        // at 50 ms purely to service pings and detect closure.
        ws.get_ref()
            .set_read_timeout(Some(Duration::from_millis(50)))?;
        let (tx, rx) = sync_channel::<Outgoing>(CLIENT_QUEUE);
        let id = self.next_client.fetch_add(1, Ordering::Relaxed);

        {
            // Join under the broadcast lock: hello + snapshot, then
            // register for live frames. Nothing can interleave.
            let mut shared = self.shared.lock().expect("fanout lock");
            if shared.ended {
                return Ok(());
            }
            let hello = Control::Hello {
                session: self.session.clone(),
                cols: shared.cols,
                rows: shared.rows,
                seq: shared.seq,
                degraded: shared.ring.degraded(),
            };
            tx.send(Outgoing::Text(hello.to_json())).ok();
            tx.send(Outgoing::Text(Control::SnapshotBegin.to_json()))
                .ok();
            let snapshot = shared.ring.snapshot();
            if !snapshot.is_empty() {
                tx.send(Outgoing::Bin(snapshot)).ok();
            }
            // The presence section: a leave-preamble for every id ever
            // broadcast (a rejoining spectator may still hold rows the relay
            // no longer tracks; on a first join it is `unknown-id` noise in
            // that spectator's own error ring), then `replace=true` joins and
            // notes for exactly the rows fresh at **this** instant.
            let presence = shared.presence.render(Instant::now());
            if !presence.is_empty() {
                let live = shared.presence.live_ids();
                if !live.is_empty() {
                    let frame = Control::PresenceMirror {
                        add: live,
                        remove: Vec::new(),
                        clear: false,
                    };
                    tx.send(Outgoing::Text(frame.to_json())).ok();
                }
                let bytes: Vec<u8> = presence
                    .iter()
                    .flat_map(|cmd| cmd.to_wire().into_bytes())
                    .collect();
                tx.send(Outgoing::Bin(bytes)).ok();
            }
            tx.send(Outgoing::Text(Control::SnapshotEnd.to_json())).ok();
            shared.clients.push(Client { tx, id });
        }

        pump_client(ws, rx);

        let mut shared = self.shared.lock().expect("fanout lock");
        shared.clients.retain(|c| c.id != id);
        Ok(())
    }
}

/// Single thread per client: drain the outgoing queue, then poll the socket
/// (50 ms read timeout) purely to service pings and detect closure. Every
/// inbound data frame is discarded unread — "no viewer commands" holds
/// mechanically.
fn pump_client(mut ws: WebSocket<TcpStream>, rx: Receiver<Outgoing>) {
    loop {
        let mut closing = false;
        loop {
            match rx.try_recv() {
                Ok(Outgoing::Bin(bytes)) => {
                    if ws.send(Message::Binary(bytes)).is_err() {
                        return;
                    }
                }
                Ok(Outgoing::Text(text)) => {
                    if ws.send(Message::Text(text)).is_err() {
                        return;
                    }
                }
                Ok(Outgoing::Close) => {
                    closing = true;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    closing = true;
                    break;
                }
            }
        }
        if closing {
            let _ = ws.close(None);
            let _ = ws.flush();
            return;
        }
        match ws.read() {
            Ok(_) => {} // inbound frames are discarded; read also services ping/pong
            Err(tungstenite::Error::Io(err))
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presence::{Body, Cmd, MirrorId};

    fn fanout() -> Arc<Fanout> {
        Fanout::new("test".to_string(), 4096, 80, 24)
    }

    /// Register a spectator without a socket, so the fan-out's queue is
    /// observable from a test.
    fn attach(fanout: &Arc<Fanout>) -> Receiver<Outgoing> {
        let (tx, rx) = sync_channel::<Outgoing>(CLIENT_QUEUE);
        let mut shared = fanout.shared.lock().expect("fanout lock");
        shared.clients.push(Client { tx, id: 1 });
        rx
    }

    fn drain(rx: &Receiver<Outgoing>) -> (Vec<u8>, Vec<Control>) {
        let mut bytes = Vec::new();
        let mut frames = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Outgoing::Bin(chunk) => bytes.extend_from_slice(&chunk),
                Outgoing::Text(text) => {
                    frames.extend(Control::from_json(&text));
                }
                Outgoing::Close => {}
            }
        }
        (bytes, frames)
    }

    fn joined(id: &str) -> Cmd {
        Cmd::Join {
            id: id.to_string(),
            name: "Ann".to_string(),
            color: "#00ffcc".to_string(),
            ttl: 30.0,
        }
    }

    #[test]
    fn synthesized_presence_never_enters_the_ring() {
        let fanout = fanout();
        let rx = attach(&fanout);
        fanout.broadcast(&[Seg::Data(b"screen text".to_vec())]);
        fanout.presence(&[joined("r0.ann")], Snapshot::default());

        let (bytes, _) = drain(&rx);
        let wire = String::from_utf8_lossy(&bytes);
        assert!(wire.contains("user.join"), "spectators must see the join");

        // The ring is the late-join replay. Synthesized presence is
        // generated from the poll model, never replayed — so history can
        // never resurrect a mirrored row.
        let ring = fanout.shared.lock().expect("fanout lock").ring.snapshot();
        assert_eq!(ring, b"screen text".to_vec());
    }

    #[test]
    fn presence_brackets_its_bytes_with_mirror_frames() {
        let fanout = fanout();
        let rx = attach(&fanout);
        fanout.presence(
            &[
                Cmd::Leave {
                    id: "r0.gone".to_string(),
                },
                joined("r0.ann"),
            ],
            Snapshot::default(),
        );
        let (_, frames) = drain(&rx);
        assert_eq!(
            frames,
            vec![
                Control::PresenceMirror {
                    add: vec![MirrorId::participant("r0.ann")],
                    remove: Vec::new(),
                    clear: false,
                },
                Control::PresenceMirror {
                    add: Vec::new(),
                    remove: vec![MirrorId::participant("r0.gone")],
                    clear: false,
                },
            ]
        );
    }

    #[test]
    fn the_batch_and_the_model_advance_together() {
        // A spectator joining between the fan-out and the republish would be
        // handed a model that disagrees with the byte stream — in one
        // direction a departed row re-joined from a stale snapshot, in the
        // other a new row it was registered too late to receive and that no
        // later walk restates. One lock acquisition makes both impossible.
        let fanout = fanout();
        let rx = attach(&fanout);
        let deadline = Instant::now() + Duration::from_secs(60);
        let snapshot = Snapshot {
            ever: vec![MirrorId::participant("r0.ann")],
            live: vec![(
                MirrorId::participant("r0.ann"),
                Body::Participant {
                    name: "Ann".to_string(),
                    color: "#00ffcc".to_string(),
                    cursor: None,
                },
                deadline,
            )],
        };
        fanout.presence(&[joined("r0.ann")], snapshot.clone());

        let (bytes, _) = drain(&rx);
        assert!(String::from_utf8_lossy(&bytes).contains("id=r0.ann"));
        assert_eq!(
            fanout.shared.lock().expect("fanout lock").presence,
            snapshot,
            "the model a late joiner is handed must already include the batch"
        );

        // An empty batch is a pure republish — still one acquisition, and it
        // sends nothing.
        fanout.presence(&[], Snapshot::default());
        let (bytes, frames) = drain(&rx);
        assert!(bytes.is_empty() && frames.is_empty());
        assert!(
            fanout
                .shared
                .lock()
                .expect("fanout lock")
                .presence
                .live
                .is_empty()
        );
    }

    #[test]
    fn a_roster_reset_clears_the_published_mirror_and_says_so() {
        let fanout = fanout();
        let rx = attach(&fanout);
        fanout.presence(
            &[],
            Snapshot {
                ever: vec![MirrorId::participant("r0.ann")],
                live: vec![(
                    MirrorId::participant("r0.ann"),
                    Body::Participant {
                        name: "Ann".to_string(),
                        color: "#00ffcc".to_string(),
                        cursor: None,
                    },
                    Instant::now() + Duration::from_secs(60),
                )],
            },
        );
        fanout.broadcast(&[Seg::Anchor {
            bytes: b"\x1b]777;ratty:reset\x07".to_vec(),
            roster_reset: true,
        }]);

        let (_, frames) = drain(&rx);
        assert!(
            frames.contains(&Control::PresenceMirror {
                add: Vec::new(),
                remove: Vec::new(),
                clear: true,
            }),
            "spectators must be told their rosters were cleared: {frames:?}"
        );
        assert!(
            fanout
                .shared
                .lock()
                .expect("fanout lock")
                .presence
                .live
                .is_empty()
        );
    }

    #[test]
    fn a_screen_clear_is_not_a_roster_reset() {
        let fanout = fanout();
        let rx = attach(&fanout);
        fanout.broadcast(&[Seg::Anchor {
            bytes: b"\x1b[2J".to_vec(),
            roster_reset: false,
        }]);
        let (_, frames) = drain(&rx);
        assert_eq!(frames, vec![Control::ResetNotice]);
    }

    #[test]
    fn session_end_releases_every_mirrored_row() {
        let fanout = fanout();
        let rx = attach(&fanout);
        fanout.presence(
            &[],
            Snapshot {
                ever: vec![MirrorId::participant("r0.ann")],
                live: vec![
                    (
                        MirrorId::participant("r0.ann"),
                        Body::Participant {
                            name: "Ann".to_string(),
                            color: "#00ffcc".to_string(),
                            cursor: None,
                        },
                        Instant::now() + Duration::from_secs(60),
                    ),
                    (
                        MirrorId::note("r0.n1"),
                        Body::Note {
                            text: "hi".to_string(),
                            x: 1,
                            y: 2,
                        },
                        Instant::now() + Duration::from_secs(60),
                    ),
                ],
            },
        );
        fanout.end("session-ended", true);

        let (bytes, frames) = drain(&rx);
        let wire = String::from_utf8_lossy(&bytes);
        assert!(wire.contains("ratty:user.leave;id=r0.ann"), "{wire:?}");
        assert!(wire.contains("ratty:note.remove;id=r0.n1"), "{wire:?}");
        assert!(
            frames.iter().any(|frame| matches!(
                frame,
                Control::PresenceMirror { remove, .. } if remove.len() == 2
            )),
            "the spectator's tracked set must be emptied too: {frames:?}"
        );
    }
}
