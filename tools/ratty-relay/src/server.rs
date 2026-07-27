//! WebSocket fan-out: one accept loop, one thread per spectator.
//!
//! Invariants from the design ("Fan-out wire"):
//!
//! - the server ignores every inbound client frame — no code path from
//!   spectator to the shell exists;
//! - per-client bounded queues; a full queue drops that client into the
//!   disconnect-and-rejoin path, never backpressuring the pump;
//! - a joining client receives `hello`, `snapshot-begin`, the ring bytes,
//!   then `snapshot-end` under the same lock the broadcast takes, so live
//!   frames can never interleave before the snapshot completes.

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use tungstenite::{Message, WebSocket};

use crate::frames::Control;
use crate::gate::Seg;
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
                Seg::Data(bytes) | Seg::Anchor { bytes } => bytes.clone(),
            };
            shared.seq += 1;
            Self::send_all(&mut shared, Outgoing::Bin(bytes));
            if matches!(seg, Seg::Anchor { .. }) {
                Self::send_all(&mut shared, Outgoing::Text(Control::ResetNotice.to_json()));
            }
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

    /// Session teardown: optional plain-text banner, `end` control frame,
    /// then all sockets close.
    pub fn end(&self, reason: &str, banner: bool) {
        let mut shared = self.shared.lock().expect("fanout lock");
        if shared.ended {
            return;
        }
        shared.ended = true;
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
