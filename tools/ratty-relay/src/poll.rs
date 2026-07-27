//! Stage 2's OSC 778 client: poll the primary, diff, synthesize.
//!
//! The wrapper already owns the tty interposition, so the query loop's hard
//! half — excising the relay's own replies from a stream that must keep
//! flowing — comes nearly free (`docs/research/relay-design.md`, "The
//! recommendation: the gated tee"). Three pieces live here:
//!
//! - [`Downstream`] — stdout plus the gate, behind one lock. The output pump
//!   writes the shell's bytes through it; the driver injects its queries
//!   through it, and **only at a recognized sequence boundary** — a
//!   mid-sequence injection corrupts the primary's parse. Holding the write
//!   and the gate's tokenization under the same lock is what makes
//!   "at a boundary" mean the same thing to both callers.
//! - [`InputDemux`] — the input direction. Ratty answers a query by writing
//!   the reply into the PTY it owns, so replies arrive as *input*
//!   (`protocols/query.md:45-48`). The demux lifts out the frames the relay
//!   itself asked for and forwards every other byte untouched — including
//!   another 778 client's replies, which are none of the relay's business.
//! - [`Driver`] — the loop: walk `state.presence`, fold the walk into the
//!   [`Mirror`], fan out the synthesized commands, and run the deadline
//!   timers on every tick regardless of what the query plane is doing.
//!
//! The relay is the session's sole 778 client by construction. A concurrent
//! naive client (ratty-ai in raw `/dev/tty` mode) can still eat replies —
//! a known cost of the option, recorded in the design.

use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::gate::{Gate, GateStats, Seg};
use crate::presence::{Mirror, MirrorStats, Row};
use crate::query;
use crate::server::Fanout;

/// The OSC 778 sequence opener, on the input direction.
const REPLY_PREFIX: &[u8] = b"\x1b]778;";

/// How long a partial `ESC ] 7 7 8 ;` prefix may be withheld before it is
/// let through as ordinary input. A lone ESC keypress is a legitimate byte
/// that must reach the shell promptly (vim), so the hold is bounded by time,
/// not by the arrival of a next byte.
pub const PREFIX_HOLD: Duration = Duration::from_millis(5);

/// How long a started-but-unterminated 778 frame may be withheld. Longer
/// than [`PREFIX_HOLD`] because these bytes cannot be a keystroke, and
/// shorter than forever because a stalled frame must never wedge input.
pub const FRAME_HOLD: Duration = Duration::from_millis(250);

/// Bound on a withheld frame: twice the reply cap, so a legitimate reply is
/// never cut in half. Past it the frame is abandoned and its tail swallowed
/// to the terminator — see [`InputDemux::abandon`] for why the bytes are
/// dropped rather than released to the shell. The same figure bounds the
/// swallow, so a terminator that never arrives cannot eat the session.
const MAX_HELD_BYTES: usize = 2 * query::MAX_REPLY_SEQUENCE_BYTES;

/// Correlation tokens remembered for late-reply suppression. A walk the
/// driver has abandoned may still be answered; that answer must be eaten,
/// not typed into the shell.
const TOKEN_MEMORY: usize = 64;

/// How often the driver wakes regardless of query traffic. Deadline timers
/// run on this cadence, so a lease expires off a spectator's roster within
/// one tick of its true expiry even when the query plane is stalled.
const TICK: Duration = Duration::from_millis(50);

/// How long a single page of a walk may go unanswered.
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

/// Ceiling on the unanswered-query backoff, and the shift that reaches it.
const MAX_BACKOFF: Duration = Duration::from_secs(8);
const BACKOFF_SHIFT_CAP: u32 = 8;

/// Poll cycles between `state.namespaces` cross-checks. The aggregate's
/// fresh counts are compared against the walk's row count purely as an
/// honesty signal: a mismatch is the paginated-walk shear the query protocol
/// warns about (`protocols/presence.md:128-134`), reported in the session's
/// closing stats rather than mid-session over the user's screen.
const NAMESPACE_CHECK_EVERY: u64 = 4;

/// 128-bit random hex, the scheme both existing 778 clients use
/// (`tools/ratty-ai/src/main.rs:2734`, `src/web.rs:73`).
pub fn generate_token() -> String {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).expect("system entropy is available");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The write side of the tee: the primary's stdout and the gate that
/// tokenizes it, under one lock so injection can ask a truthful question.
pub struct Downstream {
    out: std::io::Stdout,
    gate: Gate,
}

impl Downstream {
    pub fn new() -> Downstream {
        Downstream {
            out: std::io::stdout(),
            gate: Gate::new(),
        }
    }

    /// One PTY chunk: verbatim to the primary, then through the gate. The
    /// returned segments are the spectators' copy — the caller broadcasts
    /// them outside this lock.
    pub fn forward(&mut self, chunk: &[u8]) -> std::io::Result<Vec<Seg>> {
        self.out.write_all(chunk)?;
        self.out.flush()?;
        Ok(self.gate.push(chunk))
    }

    /// Inject relay-originated bytes into the stream the primary parses.
    /// Refuses unless the gate says the stream is between sequences; the
    /// caller retries on the next tick rather than interleaving.
    ///
    /// Injected queries deliberately do **not** pass through the gate: they
    /// are the relay's own traffic to the primary, never part of the shell's
    /// output, and never anything a spectator sees.
    pub fn inject(&mut self, bytes: &[u8]) -> bool {
        if !self.gate.at_boundary() {
            return false;
        }
        if self.out.write_all(bytes).is_err() {
            return false;
        }
        let _ = self.out.flush();
        true
    }

    pub fn stats(&self) -> GateStats {
        self.gate.stats
    }
}

impl Default for Downstream {
    fn default() -> Self {
        Downstream::new()
    }
}

/// The correlation tokens this relay has issued, bounded and shared with the
/// input demux so it knows which replies are its own.
#[derive(Default)]
pub struct OurTokens {
    live: HashSet<String>,
    order: VecDeque<String>,
}

impl OurTokens {
    pub fn register(&mut self, token: &str) {
        if self.live.insert(token.to_string()) {
            self.order.push_back(token.to_string());
        }
        while self.order.len() > TOKEN_MEMORY {
            if let Some(old) = self.order.pop_front() {
                self.live.remove(&old);
            }
        }
    }

    pub fn contains(&self, token: &str) -> bool {
        self.live.contains(token)
    }
}

/// What one chunk of input produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Feed {
    /// Bytes to forward to the shell, in order.
    pub forward: Vec<u8>,
    /// Complete 778 reply bodies the relay asked for.
    pub frames: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DemuxState {
    Ground,
    /// Matched this many bytes of [`REPLY_PREFIX`].
    Prefix(usize),
    Frame,
    /// A frame abandoned mid-flight. Its remaining bytes are swallowed to the
    /// terminator rather than released: the head has already been dropped, so
    /// letting the tail through would type a reply's `data=` payload into the
    /// shell one fragment later — the same leak, just quieter. Bounded, so a
    /// terminator that never arrives cannot eat the user's input forever.
    Discard(usize),
}

/// Lifts the relay's own 778 replies out of the input direction and forwards
/// everything else byte-for-byte.
pub struct InputDemux {
    state: DemuxState,
    held: Vec<u8>,
    esc_pending: bool,
    held_since: Option<Instant>,
    /// Frames withheld past their hold bound and let through as raw input.
    pub stalled: u64,
}

impl Default for InputDemux {
    fn default() -> Self {
        InputDemux::new()
    }
}

impl InputDemux {
    pub fn new() -> InputDemux {
        InputDemux {
            state: DemuxState::Ground,
            held: Vec::new(),
            esc_pending: false,
            held_since: None,
            stalled: 0,
        }
    }

    /// Feed one input chunk. `mine` decides whether a complete reply belongs
    /// to this relay; anything else is forwarded verbatim so a concurrent
    /// 778 client still gets its answer.
    pub fn push(&mut self, chunk: &[u8], now: Instant, mine: &dyn Fn(&str) -> bool) -> Feed {
        let mut feed = Feed::default();
        for &byte in chunk {
            self.step(byte, now, mine, &mut feed);
        }
        feed
    }

    fn step(&mut self, byte: u8, now: Instant, mine: &dyn Fn(&str) -> bool, feed: &mut Feed) {
        match self.state {
            DemuxState::Ground => {
                if byte == REPLY_PREFIX[0] {
                    self.state = DemuxState::Prefix(1);
                    self.held.push(byte);
                    self.held_since = Some(now);
                } else {
                    feed.forward.push(byte);
                }
            }
            DemuxState::Prefix(matched) => {
                if byte == REPLY_PREFIX[matched] {
                    self.held.push(byte);
                    if matched + 1 == REPLY_PREFIX.len() {
                        self.state = DemuxState::Frame;
                        self.esc_pending = false;
                    } else {
                        self.state = DemuxState::Prefix(matched + 1);
                    }
                } else {
                    // Not ours after all: release what we withheld and let
                    // this byte start over from ground (it may be an ESC).
                    feed.forward.append(&mut self.held);
                    self.state = DemuxState::Ground;
                    self.held_since = None;
                    self.step(byte, now, mine, feed);
                }
            }
            DemuxState::Frame => {
                if self.esc_pending {
                    self.esc_pending = false;
                    if byte == b'\\' {
                        self.finish(b"\x1b\\", mine, feed);
                    } else {
                        // vte semantics: a bare ESC ends the string and the
                        // ESC + this byte reprocess as a new sequence.
                        self.finish(b"", mine, feed);
                        self.step(0x1b, now, mine, feed);
                        self.step(byte, now, mine, feed);
                    }
                    return;
                }
                match byte {
                    0x07 => self.finish(b"\x07", mine, feed),
                    0x1b => self.esc_pending = true,
                    _ => {
                        self.held.push(byte);
                        if self.held.len() > MAX_HELD_BYTES {
                            // Past the reply cap and still unterminated. See
                            // `abandon` for why this is dropped rather than
                            // released to the shell.
                            self.abandon();
                        }
                    }
                }
            }
            DemuxState::Discard(seen) => {
                if self.esc_pending {
                    self.esc_pending = false;
                    self.state = DemuxState::Ground;
                    if byte != b'\\' {
                        // Not an ST: the ESC ended the string, and this byte
                        // starts something new.
                        self.step(byte, now, mine, feed);
                    }
                    return;
                }
                match byte {
                    0x07 => self.state = DemuxState::Ground,
                    0x1b => self.esc_pending = true,
                    _ if seen + 1 >= MAX_HELD_BYTES => self.state = DemuxState::Ground,
                    _ => self.state = DemuxState::Discard(seen + 1),
                }
            }
        }
    }

    /// Give up on a frame already past `ESC ] 7 7 8 ;`: drop what was held
    /// and swallow the rest to its terminator.
    ///
    /// It cannot be a keystroke — nothing a keyboard produces matches that
    /// prefix — and releasing it would type a query-plane reply into the
    /// shell, whose `data=` payload carries the caller's own roster including
    /// its `fresh:false` rows; the shell's echo would come back round the
    /// loop as ordinary rendered text, past the gate's 778 excision, onto
    /// every spectator's screen. The cost of dropping is a lost reply: this
    /// relay re-polls, and a concurrent 778 client sees a timeout it already
    /// has to handle.
    fn abandon(&mut self) {
        self.stalled += 1;
        self.held.clear();
        self.esc_pending = false;
        self.held_since = None;
        self.state = DemuxState::Discard(0);
    }

    /// A terminated 778 sequence: consume it if the relay asked for it,
    /// otherwise put every byte back on the wire exactly as it arrived.
    fn finish(&mut self, terminator: &[u8], mine: &dyn Fn(&str) -> bool, feed: &mut Feed) {
        self.state = DemuxState::Ground;
        self.held_since = None;
        let held = std::mem::take(&mut self.held);
        let body = &held[REPLY_PREFIX.len()..];
        if let Ok(text) = std::str::from_utf8(body)
            && query::parse_reply_body(text).is_some_and(|reply| mine(&reply.token))
        {
            feed.frames.push(text.to_string());
            return;
        }
        feed.forward.extend_from_slice(&held);
        feed.forward.extend_from_slice(terminator);
    }

    /// How long the caller may still block before it must call
    /// [`Self::flush`], or `None` when nothing is withheld and it may block
    /// indefinitely. The bound is absolute — measured from the moment the
    /// hold began, not from the last byte — so a trickle of input cannot
    /// keep a candidate withheld indefinitely by resetting the clock.
    pub fn hold_deadline(&self, now: Instant) -> Option<Duration> {
        let since = self.held_since?;
        let bound = match self.state {
            // Nothing is withheld in either: `Discard` swallows as it goes.
            DemuxState::Ground | DemuxState::Discard(_) => return None,
            DemuxState::Prefix(_) => PREFIX_HOLD,
            DemuxState::Frame => FRAME_HOLD,
        };
        Some((since + bound).saturating_duration_since(now))
    }

    /// Resolve whatever is withheld once its hold bound elapses, and return
    /// the bytes that should still reach the shell.
    ///
    /// The two states resolve in opposite directions, and the asymmetry is
    /// the whole point:
    ///
    /// - A partial **prefix** may well be ordinary input — a lone ESC
    ///   keypress is indistinguishable from the start of a 778 frame until
    ///   the next byte arrives — so it is released. Withholding a keystroke
    ///   is the harm here.
    /// - A started **frame** has already matched `ESC ] 7 7 8 ;` in full, so
    ///   it is abandoned rather than released (see [`Self::abandon`]).
    pub fn flush(&mut self) -> Vec<u8> {
        if self.state == DemuxState::Frame {
            self.abandon();
            return Vec::new();
        }
        self.esc_pending = false;
        self.state = DemuxState::Ground;
        self.held_since = None;
        std::mem::take(&mut self.held)
    }
}

/// Closing counters for the session status line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PollStats {
    pub walks: u64,
    pub pages: u64,
    pub timeouts: u64,
    pub rejects: u64,
    /// Replies discarded because a `reset` landed while the walk was in
    /// flight — they describe a registry the primary has since wiped.
    pub stale: u64,
    /// Walks whose row count disagreed with the `state.namespaces`
    /// aggregate — the paginated-walk shear, surfaced rather than smoothed.
    pub divergences: u64,
    pub mirror: MirrorStats,
}

/// The poll/diff/synthesize loop.
pub struct Driver {
    pub down: Arc<Mutex<Downstream>>,
    pub fanout: Arc<Fanout>,
    pub replies: Receiver<String>,
    pub tokens: Arc<Mutex<OurTokens>>,
    pub shutdown: Arc<AtomicBool>,
    /// Bumped by the output pump on every 777 `reset`.
    pub reset_epoch: Arc<AtomicU64>,
    pub interval: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Presence,
    Namespaces,
}

enum Walk {
    Idle,
    /// Built, waiting for a sequence boundary to inject at.
    Pending {
        stage: Stage,
        token: String,
        bytes: Vec<u8>,
        epoch: u64,
    },
    /// Injected, awaiting the correlated reply.
    Awaiting {
        stage: Stage,
        token: String,
        deadline: Instant,
        /// The reset epoch the walk was issued under. A reply that outlives
        /// its epoch describes a registry the primary has since wiped, and
        /// applying it would re-announce rows the `reset` destroyed — with
        /// no teardown path left, because `Mirror::reset` has already
        /// dropped every record of them.
        epoch: u64,
    },
}

impl Driver {
    pub fn run(self) -> PollStats {
        let mut mirror = Mirror::new();
        let mut stats = PollStats::default();
        let mut walk = Walk::Idle;
        let mut rows: Vec<Row> = Vec::new();
        let mut pages: u64 = 0;
        let mut started = Instant::now();
        let mut next_poll = Instant::now();
        let mut seen_epoch = self.reset_epoch.load(Ordering::Relaxed);
        // Consecutive unanswered queries. Injection is the one pressure the
        // relay puts *inward* on the primary, so it backs off rather than
        // re-polling on cadence into a session that is not answering.
        let mut silent: u32 = 0;

        while !self.shutdown.load(Ordering::Relaxed) {
            let now = Instant::now();

            // The primary reset. Spectator rosters were cleared by the
            // forwarded bytes, so the model drops silently — and any walk in
            // flight describes a registry that no longer exists.
            let epoch = self.reset_epoch.load(Ordering::Relaxed);
            if epoch != seen_epoch {
                seen_epoch = epoch;
                mirror.reset();
                walk = Walk::Idle;
                rows.clear();
                pages = 0;
                next_poll = now;
                self.fanout.presence(&[], mirror.snapshot());
            }

            // Deadline timers, every tick: a lease that runs out between
            // polls leaves the spectators' rosters rather than expiring in
            // place there.
            let expired = mirror.expire(now);
            if !expired.is_empty() {
                self.fanout.presence(&expired, mirror.snapshot());
            }

            match &walk {
                Walk::Idle if now >= next_poll => {
                    started = now;
                    rows.clear();
                    pages = 0;
                    walk = self.build(Stage::Presence, None, seen_epoch);
                }
                Walk::Pending {
                    stage,
                    token,
                    bytes,
                    epoch,
                } => {
                    if self
                        .down
                        .lock()
                        .expect("downstream lock")
                        .inject(bytes.as_slice())
                    {
                        walk = Walk::Awaiting {
                            stage: *stage,
                            token: token.clone(),
                            deadline: now + QUERY_TIMEOUT,
                            epoch: *epoch,
                        };
                    }
                }
                Walk::Awaiting { deadline, .. } if now >= *deadline => {
                    stats.timeouts += 1;
                    silent = silent.saturating_add(1);
                    walk = Walk::Idle;
                    next_poll = now + self.backoff(silent);
                }
                _ => {}
            }

            let frame = match self.replies.recv_timeout(TICK) {
                Ok(frame) => frame,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            let Some(reply) = query::parse_reply_body(&frame) else {
                continue;
            };
            let Walk::Awaiting {
                stage,
                token,
                epoch,
                ..
            } = &walk
            else {
                continue; // a late answer to a walk already abandoned
            };
            if &reply.token != token {
                continue;
            }
            let (stage, epoch) = (*stage, *epoch);
            // The primary answered, so the inward path is clear.
            silent = 0;
            if epoch != self.reset_epoch.load(Ordering::Relaxed) {
                // A `reset` landed while this walk was in flight: the reply
                // describes a registry that no longer exists, and the mirror
                // is about to be dropped anyway.
                stats.stale += 1;
                walk = Walk::Idle;
                next_poll = Instant::now();
                continue;
            }
            if !reply.ok {
                stats.rejects += 1;
                walk = Walk::Idle;
                next_poll = Instant::now() + self.interval;
                continue;
            }
            let payload = std::str::from_utf8(&reply.data)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(text).ok());
            let Some(payload) = payload else {
                stats.rejects += 1;
                walk = Walk::Idle;
                next_poll = Instant::now() + self.interval;
                continue;
            };

            match stage {
                Stage::Presence => {
                    pages += 1;
                    stats.pages += 1;
                    if let Some(items) = payload.get("items").and_then(Value::as_array) {
                        rows.extend(items.iter().filter_map(Row::from_json));
                    }
                    if let Some(cursor) = payload.get("cursor").and_then(Value::as_str) {
                        walk = self.build(Stage::Presence, Some(cursor), epoch);
                        continue;
                    }
                    // The walk is complete. Deadlines derive from the instant
                    // it *started* — so a multi-page walk errs early and the
                    // relay leads expiry — while emission is gated on *now*,
                    // so a row whose lease ran out during the round trip is
                    // never stated.
                    stats.walks += 1;
                    let cmds = mirror.apply_walk(&rows, started, Instant::now(), pages == 1);
                    self.fanout.presence(&cmds, mirror.snapshot());

                    if stats.walks.is_multiple_of(NAMESPACE_CHECK_EVERY) {
                        walk = self.build(Stage::Namespaces, None, epoch);
                    } else {
                        walk = Walk::Idle;
                        next_poll = Instant::now() + self.interval;
                    }
                }
                Stage::Namespaces => {
                    if let Some(total) = fresh_total(&payload)
                        && total != rows.len()
                    {
                        stats.divergences += 1;
                    }
                    walk = Walk::Idle;
                    next_poll = Instant::now() + self.interval;
                }
            }
        }

        // Teardown leaves ride `Fanout::end`, which renders them from the
        // published model — so they still go out if this loop never gets
        // here.
        stats.mirror = mirror.stats;
        stats
    }

    fn build(&self, stage: Stage, cursor: Option<&str>, epoch: u64) -> Walk {
        let token = generate_token();
        self.tokens.lock().expect("token lock").register(&token);
        let data = cursor.map(|cursor| format!("{{\"cursor\":\"{cursor}\"}}"));
        let op = match stage {
            Stage::Presence => "state.presence",
            Stage::Namespaces => "state.namespaces",
        };
        let bytes =
            query::query_sequence(&token, op, data.as_deref().map(str::as_bytes)).into_bytes();
        Walk::Pending {
            stage,
            token,
            bytes,
            epoch,
        }
    }

    /// Back off after consecutive unanswered queries, doubling from the poll
    /// interval to [`MAX_BACKOFF`].
    ///
    /// A query is the relay's own traffic *into* the primary, and ratty
    /// answers it with a blocking write onto the relay's stdin. If the shell
    /// has stopped draining its input, that write is where the primary's
    /// main loop stalls — so a relay that keeps polling on cadence turns a
    /// transient stall into a standing one. Silence is the signal to stop
    /// pushing; the first answered query clears it.
    fn backoff(&self, silent: u32) -> Duration {
        let scaled = self
            .interval
            .saturating_mul(1u32 << silent.min(BACKOFF_SHIFT_CAP));
        scaled.min(MAX_BACKOFF)
    }
}

/// The fresh participant + note count across every namespace, from a
/// `state.namespaces` reply (`src/query_channel.rs:921-933`).
fn fresh_total(payload: &Value) -> Option<usize> {
    let namespaces = payload.get("namespaces")?.as_array()?;
    let mut total = 0usize;
    for row in namespaces {
        total += usize::try_from(row.get("participants")?.as_u64()?).ok()?;
        total += usize::try_from(row.get("notes")?.as_u64()?).ok()?;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ours(_token: &str) -> bool {
        true
    }

    fn theirs(_token: &str) -> bool {
        false
    }

    fn reply(token: &str, data: &str) -> String {
        query::reply_sequence(token, false, true, None, Some(data.as_bytes()))
            .iter()
            .map(|b| *b as char)
            .collect()
    }

    /// Feed a byte stream through the demux in every split, and assert the
    /// forwarded bytes and lifted frames never depend on chunking — the same
    /// property the gate's differential test pins on the output direction.
    fn demux_all(
        input: &[u8],
        splits: &[usize],
        mine: &dyn Fn(&str) -> bool,
    ) -> (Vec<u8>, Vec<String>) {
        let mut demux = InputDemux::new();
        let now = Instant::now();
        let mut forward = Vec::new();
        let mut frames = Vec::new();
        let mut start = 0;
        for &split in splits {
            let feed = demux.push(&input[start..split], now, mine);
            forward.extend(feed.forward);
            frames.extend(feed.frames);
            start = split;
        }
        let feed = demux.push(&input[start..], now, mine);
        forward.extend(feed.forward);
        frames.extend(feed.frames);
        forward.extend(demux.flush());
        (forward, frames)
    }

    #[test]
    fn plain_input_passes_through_untouched() {
        let input = b"ls -la\r\x1b[Aecho hi\r";
        let (forward, frames) = demux_all(input, &[], &ours);
        assert_eq!(forward, input.to_vec());
        assert!(frames.is_empty());
    }

    #[test]
    fn a_lone_escape_keypress_is_not_swallowed() {
        // vim's ESC must reach the shell. It is withheld only until the hold
        // bound, which the pump enforces by calling `flush`.
        let mut demux = InputDemux::new();
        let now = Instant::now();
        let feed = demux.push(b"\x1b", now, &ours);
        assert!(feed.forward.is_empty(), "held while it might be a 778");
        assert_eq!(demux.hold_deadline(now), Some(PREFIX_HOLD));
        // The bound is absolute: it decays with wall time, so a trickle of
        // input cannot keep the ESC withheld by resetting the clock.
        assert_eq!(demux.hold_deadline(now + PREFIX_HOLD), Some(Duration::ZERO));
        assert_eq!(demux.flush(), b"\x1b".to_vec());
        assert_eq!(demux.hold_deadline(now), None);
    }

    #[test]
    fn a_diverging_prefix_is_released_immediately() {
        // `ESC ] 0 ; title BEL` shares four bytes with the 778 prefix.
        let input = b"\x1b]0;title\x07rest";
        let (forward, frames) = demux_all(input, &[], &ours);
        assert_eq!(forward, input.to_vec());
        assert!(frames.is_empty());
    }

    #[test]
    fn our_own_replies_are_lifted_out_of_the_input_stream() {
        let mut input = b"before".to_vec();
        input.extend_from_slice(reply("abc123", "{\"items\":[]}").as_bytes());
        input.extend_from_slice(b"after");
        let (forward, frames) = demux_all(&input, &[], &ours);
        assert_eq!(forward, b"beforeafter".to_vec());
        assert_eq!(frames.len(), 1);
        let parsed = query::parse_reply_body(&frames[0]).expect("a well-formed reply");
        assert_eq!(parsed.token, "abc123");
        assert!(parsed.ok);
    }

    #[test]
    fn another_clients_reply_is_forwarded_verbatim() {
        // Replies route to the originating transport; a concurrent 778
        // client's answer is not the relay's to eat.
        let mut input = b"x".to_vec();
        input.extend_from_slice(reply("someone-else", "{}").as_bytes());
        let (forward, frames) = demux_all(&input, &[], &theirs);
        assert_eq!(forward, input);
        assert!(frames.is_empty());
    }

    #[test]
    fn bel_terminated_frames_are_recognized_too() {
        let mut input = b"\x1b]778;v=1;t=r;id=tok1;ok=1\x07".to_vec();
        input.extend_from_slice(b"tail");
        let (forward, frames) = demux_all(&input, &[], &ours);
        assert_eq!(forward, b"tail".to_vec());
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn chunk_splits_never_change_the_demuxed_stream() {
        let mut corpus = b"login\r".to_vec();
        corpus.extend_from_slice(reply("aaaa1111", "{\"items\":[]}").as_bytes());
        corpus.extend_from_slice(b"\x1b]0;title\x1b\\typed text\r");
        corpus.extend_from_slice(b"\x1b\x1b[A");
        corpus.extend_from_slice(reply("bbbb2222", "{\"cursor\":\"c\"}").as_bytes());
        corpus.extend_from_slice(b"tail\r");

        let (reference_forward, reference_frames) = demux_all(&corpus, &[], &ours);
        assert_eq!(reference_frames.len(), 2);
        assert!(
            !String::from_utf8_lossy(&reference_forward).contains("778"),
            "a lifted reply leaked into the shell's input"
        );

        // Byte-at-a-time is the pathological split; every prefix boundary is
        // exercised by construction.
        let every: Vec<usize> = (1..corpus.len()).collect();
        let (forward, frames) = demux_all(&corpus, &every, &ours);
        assert_eq!(forward, reference_forward);
        assert_eq!(frames, reference_frames);

        // Plus a spread of deterministic multi-byte splits.
        let mut seed = 0x1234_5678_u64;
        for round in 0..200 {
            let mut splits = Vec::new();
            for _ in 0..8 {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                splits.push(1 + ((seed >> 33) as usize) % (corpus.len() - 1));
            }
            splits.sort_unstable();
            splits.dedup();
            let (forward, frames) = demux_all(&corpus, &splits, &ours);
            assert_eq!(forward, reference_forward, "round {round}: {splits:?}");
            assert_eq!(frames, reference_frames, "round {round}: {splits:?}");
        }
    }

    #[test]
    fn an_oversized_frame_is_dropped_head_and_tail() {
        // Nothing a keyboard produces matches `ESC ] 7 7 8 ;`, so an
        // over-long frame is not input that must flow — it is a reply whose
        // `data=` carries the caller's own roster. Neither the head nor the
        // tail may be typed into the shell, where the echo would come back
        // round as rendered text past the gate's 778 excision.
        let mut demux = InputDemux::new();
        let now = Instant::now();
        let mut oversized = b"\x1b]778;".to_vec();
        oversized.extend(std::iter::repeat_n(b'x', MAX_HELD_BYTES + 8));
        let feed = demux.push(&oversized, now, &ours);
        assert!(
            feed.forward.is_empty(),
            "a reply fragment reached the shell"
        );
        assert!(feed.frames.is_empty());
        assert_eq!(demux.stalled, 1);
        assert_eq!(demux.hold_deadline(now), None);

        // The swallow ends at the terminator, and ordinary input flows again.
        let feed = demux.push(b"more-roster\x07ls -la\r", now, &ours);
        assert_eq!(feed.forward, b"ls -la\r".to_vec());
    }

    #[test]
    fn a_stalled_frame_is_abandoned_on_its_hold_bound() {
        let mut demux = InputDemux::new();
        let now = Instant::now();
        let feed = demux.push(b"\x1b]778;v=1;t=r", now, &ours);
        assert!(feed.forward.is_empty());
        assert_eq!(demux.hold_deadline(now), Some(FRAME_HOLD));
        assert!(demux.flush().is_empty(), "an abandoned frame is not typed");
        assert_eq!(demux.stalled, 1);

        // A late tail is swallowed too, then input resumes.
        let feed = demux.push(b";id=abc;ok=1;data=eyJ4IjoxfQ\x1b\\echo hi\r", now, &ours);
        assert_eq!(feed.forward, b"echo hi\r".to_vec());
    }

    #[test]
    fn an_endless_discard_eventually_lets_input_through() {
        // A terminator that never arrives must not eat the user's session.
        let mut demux = InputDemux::new();
        let now = Instant::now();
        let mut oversized = b"\x1b]778;".to_vec();
        oversized.extend(std::iter::repeat_n(b'x', MAX_HELD_BYTES + 8));
        demux.push(&oversized, now, &ours);
        let feed = demux.push(&[b'y'; 64], now, &ours);
        assert!(
            feed.forward.is_empty(),
            "the swallow covers the frame's tail"
        );
        // Past the bound the swallow gives up rather than eating the session.
        let feed = demux.push(&[b'z'; MAX_HELD_BYTES], now, &ours);
        assert!(!feed.forward.is_empty(), "the swallow must be bounded");
        let feed = demux.push(b"typed\r", now, &ours);
        assert_eq!(feed.forward, b"typed\r".to_vec());
    }

    #[test]
    fn token_memory_is_bounded_but_covers_abandoned_walks() {
        let mut tokens = OurTokens::default();
        for n in 0..(TOKEN_MEMORY * 2) {
            tokens.register(&format!("tok{n}"));
        }
        assert!(tokens.contains(&format!("tok{}", TOKEN_MEMORY * 2 - 1)));
        assert!(
            tokens.contains(&format!("tok{}", TOKEN_MEMORY)),
            "a recently abandoned walk's reply must still be eaten"
        );
        assert!(!tokens.contains("tok0"), "memory is bounded");
        assert_eq!(tokens.live.len(), TOKEN_MEMORY);
    }

    #[test]
    fn injection_waits_for_a_sequence_boundary() {
        let mut down = Downstream::new();
        // Mid-OSC: the gate is withholding a sequence, so nothing may be
        // interleaved into the primary's parse.
        let _ = down.forward(b"\x1b]777;ratty:fla");
        assert!(!down.inject(b"\x1b]778;v=1;t=q;id=a;op=caps\x1b\\"));
        // Sequence closed — ground again.
        let _ = down.forward(b"sh\x07");
        assert!(down.gate.at_boundary());
    }

    #[test]
    fn the_namespaces_aggregate_totals_fresh_rows() {
        let payload = serde_json::json!({
            "transmission": 0,
            "namespaces": [
                { "ns": 0, "objects": 2, "participants": 3, "notes": 1 },
                { "ns": 4, "objects": 0, "participants": 1, "notes": 0 }
            ]
        });
        assert_eq!(fresh_total(&payload), Some(5));
        assert_eq!(fresh_total(&serde_json::json!({})), None);
    }
}
