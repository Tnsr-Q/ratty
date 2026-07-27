//! The gated presence mirror — stage 2's model (`docs/research/relay-design.md`,
//! "Stage 2 … Q's presence engine in the same wrapper").
//!
//! Pure state: rows in (from a 778 `state.presence` walk of the primary),
//! synthesized OSC 777 presence commands out. No I/O, no clock of its own —
//! every entry point takes the instant it should reason about, so emit-time
//! freshness re-evaluation is a property of the type rather than a habit of
//! its callers.
//!
//! The invariants this module carries, each from the locked design:
//!
//! - **Only rows fresh at emit time are ever synthesized.** The walk filters
//!   on the reply's explicit `fresh` flag (the relay's own namespace 0 comes
//!   back including expired rows — `protocols/presence.md:118-122`), and
//!   every emission re-checks the row's deadline against the caller's `now`.
//!   A spectator can therefore never learn of a row that was not lawfully
//!   public when it was sent.
//! - **Ids are rewritten `r<ns>.<id>`, cap-aware.** The terminal's id cap is
//!   48 bytes; an unguarded rewrite rejects `too-large` at the spectator and
//!   that participant silently never appears. Over-cap ids truncate with a
//!   short hash suffix, collision-checked against the mirrored set.
//! - **Every synthesized `user.join`/`note` carries `replace=true`** — a
//!   replace continues the revision, so an observer reads "same identity,
//!   new state", never "brand new" (`protocols/presence.md:83-90`), and a
//!   rejoiner's surviving rows cannot reject `already-exists`.
//! - **Removals are emitted before additions** so a roster at the 16-row cap
//!   frees its slot before a new id needs one (`protocols/presence.md:92-97`).
//!
//! Synthesized presence is fan-out-only: the caller never pushes it into the
//! replay ring, so replayed history can never resurrect it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::osc;

/// Mirrors the crate-side `MAX_PRESENCE_ID_BYTES` (`src/presence.rs:129`).
/// `src/presence.rs` is a Bevy module, so unlike `osc.rs` it cannot be
/// `#[path]`-shared — this is a re-typed constant, pinned by
/// `mirror_ids_respect_the_cap` and cited here so a crate-side change has an
/// obvious landing site.
pub const MAX_MIRROR_ID_BYTES: usize = 48;

/// Mirrors `MAX_PRESENCE_PARTICIPANTS_PER_NAMESPACE` and
/// `MAX_PRESENCE_NOTES_PER_NAMESPACE` (`src/presence.rs:122,125`).
///
/// Every mirrored row lands in the spectator's namespace 0, so a
/// multi-namespace primary flattens into one 16/16 roster — an accepted cost
/// of the option, irrelevant for a single-PTY demo. Modelling the cap here is
/// what makes it *honest*: a join past it would reject `namespace-cap` at the
/// spectator and that participant would silently never appear, so the mirror
/// declines to mint and counts the refusal instead. The row is retried on
/// every later walk, so a freed slot is taken within one poll interval.
pub const MAX_MIRROR_PARTICIPANTS: usize = 16;
pub const MAX_MIRROR_NOTES: usize = 16;

/// How many mirrored ids the leave-preamble remembers. "Every id ever
/// broadcast" is unbounded over a long session, and this project bounds
/// everything; four times the 32-row ceiling a spectator can physically hold
/// keeps the preamble covering every id any spectator could still be holding
/// while the set stays fixed-size. Live rows are never forgotten, and what is
/// dropped is counted.
const EVER_MEMORY: usize = 128;

/// Mirrors `MAX_PRESENCE_TTL_SECS` (`src/presence.rs:145`). The terminal
/// clamps anyway; emitting inside the range keeps the wire honest about what
/// it asked for.
const MAX_TTL_SECS: f32 = 3600.0;

/// The smallest TTL that survives formatting. `pin_ttl` rejects a
/// non-positive `ttl=` outright (`src/presence.rs:530-541`), and three
/// decimal places floor anything under a millisecond to `0.000`, so a row
/// with a sliver of freshness left asks for a sliver rather than a reject.
/// The terminal then floors it to its own 1 s minimum — the disclosed
/// overshoot bound (`MIN_PRESENCE_TTL_SECS`, `src/presence.rs:142`).
const MIN_EMITTED_TTL_SECS: f32 = 0.001;

/// How far *later* a row's computed deadline must move before the mirror
/// re-issues its `user.join`/`note`.
///
/// In steady state the deadline is nominally constant — age grows exactly as
/// the poll instant advances — but each sample is `issue instant + remaining`
/// and the remaining arrived a round trip later, so the estimate wobbles with
/// the reply latency. Only a later deadline is a real mutation (renew,
/// replace, a fresh lease); an earlier one is noise, and is absorbed by
/// tightening the stored deadline rather than restating the row.
const DEADLINE_EPSILON: Duration = Duration::from_millis(500);

/// Consecutive walks a row must be missing from before it is torn down, when
/// the walk was paginated. Resumed cursors are monotone-by-key, not
/// snapshot-stable: a roster that mutates mid-walk can shift later rows
/// across the cursor boundary and omit them (`protocols/presence.md:128-134`).
/// A single-page walk is one reply at one instant, so absence there is
/// immediate and exact.
const MISSING_STRIKES: u8 = 2;

/// How many salted attempts a truncated id gets before the row is dropped.
const REWRITE_ATTEMPTS: u16 = 64;

/// Which roster a mirrored id lives in. Participants and notes are separate
/// id spaces at the terminal, so the same string can legitimately name one of
/// each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Participant,
    Note,
}

/// The kind-specific fields of a roster row.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Participant {
        name: String,
        color: String,
        cursor: Option<(u16, u16)>,
    },
    Note {
        text: String,
        x: u16,
        y: u16,
    },
}

impl Body {
    pub fn kind(&self) -> Kind {
        match self {
            Body::Participant { .. } => Kind::Participant,
            Body::Note { .. } => Kind::Note,
        }
    }

    /// Everything except the lease — what a re-issue exists to restate. The
    /// cursor is deliberately excluded: it moves on its own command, and a
    /// replace would only clear it.
    fn identity_eq(&self, other: &Body) -> bool {
        match (self, other) {
            (
                Body::Participant {
                    name: a, color: b, ..
                },
                Body::Participant {
                    name: c, color: d, ..
                },
            ) => a == c && b == d,
            (
                Body::Note {
                    text: a,
                    x: b,
                    y: c,
                },
                Body::Note {
                    text: d,
                    x: e,
                    y: f,
                },
            ) => a == d && b == e && c == f,
            _ => false,
        }
    }
}

/// One row of a `state.presence` walk, already filtered to `fresh: true`.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub ns: u8,
    pub id: String,
    /// `ttl_secs - age_secs` at the instant the reply was produced.
    pub remaining: f32,
    pub body: Body,
}

impl Row {
    pub fn kind(&self) -> Kind {
        self.body.kind()
    }

    /// Decode one row of the documented `state.presence` projection
    /// (`src/presence.rs:563-597`). Returns `None` for a row that is not
    /// fresh (the caller's own namespace comes back including expired rows)
    /// and for any shape the projection does not produce — an unrecognized
    /// row is never guessed at.
    pub fn from_json(value: &Value) -> Option<Row> {
        if !value.get("fresh")?.as_bool()? {
            return None;
        }
        let ns = u8::try_from(value.get("ns")?.as_u64()?).ok()?;
        let id = value.get("id")?.as_str()?.to_string();
        let ttl = value.get("ttl_secs")?.as_f64()? as f32;
        let age = value.get("age_secs")?.as_f64()? as f32;
        if !ttl.is_finite() || !age.is_finite() {
            return None;
        }
        let body = match value.get("kind")?.as_str()? {
            "participant" => Body::Participant {
                name: value.get("name")?.as_str()?.to_string(),
                color: value.get("color")?.as_str()?.to_string(),
                cursor: match value.get("cursor")? {
                    Value::Null => None,
                    cursor => Some((json_u16(cursor.get("x")?)?, json_u16(cursor.get("y")?)?)),
                },
            },
            "note" => Body::Note {
                text: value.get("text")?.as_str()?.to_string(),
                x: json_u16(value.get("x")?)?,
                y: json_u16(value.get("y")?)?,
            },
            _ => return None,
        };
        Some(Row {
            ns,
            id,
            remaining: ttl - age,
            body,
        })
    }
}

fn json_u16(value: &Value) -> Option<u16> {
    u16::try_from(value.as_u64()?).ok()
}

/// A synthesized presence command, in the terminal's own wire grammar.
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Join {
        id: String,
        name: String,
        color: String,
        ttl: f32,
    },
    Cursor {
        id: String,
        x: u16,
        y: u16,
    },
    Note {
        id: String,
        text: String,
        x: u16,
        y: u16,
        ttl: f32,
    },
    Leave {
        id: String,
    },
    NoteRemove {
        id: String,
    },
}

impl Cmd {
    /// The OSC 777 sequence, built with the terminal's own encoder
    /// (`osc::percent_encode` + `osc::osc_sequence`, BEL-terminated) so the
    /// relay and the terminal can never disagree about the grammar. Param
    /// order matches `tools/ratty-ai`'s builder.
    pub fn to_wire(&self) -> String {
        let enc = osc::percent_encode;
        match self {
            Cmd::Join {
                id,
                name,
                color,
                ttl,
            } => osc::osc_sequence(
                "user.join",
                &format!(
                    "id={}&name={}&color={}&ttl={}&replace=true",
                    enc(id),
                    enc(name),
                    enc(color),
                    fmt_ttl(*ttl)
                ),
            ),
            Cmd::Cursor { id, x, y } => {
                osc::osc_sequence("user.cursor", &format!("id={}&x={x}&y={y}", enc(id)))
            }
            Cmd::Note {
                id,
                text,
                x,
                y,
                ttl,
            } => osc::osc_sequence(
                "note",
                &format!(
                    "id={}&text={}&x={x}&y={y}&ttl={}&replace=true",
                    enc(id),
                    enc(text),
                    fmt_ttl(*ttl)
                ),
            ),
            Cmd::Leave { id } => osc::osc_sequence("user.leave", &format!("id={}", enc(id))),
            Cmd::NoteRemove { id } => osc::osc_sequence("note.remove", &format!("id={}", enc(id))),
        }
    }

    /// Which mirrored id this command adds to, or removes from, a spectator's
    /// applied set. `Cursor` touches neither — it mutates a row that a `Join`
    /// already announced.
    pub fn mirror_delta(&self) -> Option<(bool, MirrorId)> {
        match self {
            Cmd::Join { id, .. } => Some((true, MirrorId::participant(id))),
            Cmd::Note { id, .. } => Some((true, MirrorId::note(id))),
            Cmd::Leave { id } => Some((false, MirrorId::participant(id))),
            Cmd::NoteRemove { id } => Some((false, MirrorId::note(id))),
            Cmd::Cursor { .. } => None,
        }
    }
}

/// One mirrored roster id, as announced out-of-band to spectators.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MirrorId {
    pub kind: Kind,
    pub id: String,
}

impl MirrorId {
    pub fn participant(id: &str) -> MirrorId {
        MirrorId {
            kind: Kind::Participant,
            id: id.to_string(),
        }
    }

    pub fn note(id: &str) -> MirrorId {
        MirrorId {
            kind: Kind::Note,
            id: id.to_string(),
        }
    }

    /// The command that removes this row from a spectator's roster — the
    /// teardown every spectator runs on socket close, and the relay's own
    /// leave-preamble and session-end sweep.
    pub fn removal(&self) -> Cmd {
        match self.kind {
            Kind::Participant => Cmd::Leave {
                id: self.id.clone(),
            },
            Kind::Note => Cmd::NoteRemove {
                id: self.id.clone(),
            },
        }
    }
}

/// What a late joiner needs, captured without a clock: the ids ever
/// broadcast (the leave-preamble) and the live rows with their deadlines, so
/// freshness is decided when the snapshot is *rendered*, not when it was
/// published.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub ever: Vec<MirrorId>,
    pub live: Vec<(MirrorId, Body, Instant)>,
}

impl Snapshot {
    /// The presence section of a late-join snapshot
    /// (`docs/research/relay-design.md`, "Late-join snapshot semantics" §3):
    /// a leave-preamble for every id ever broadcast, then `replace=true`
    /// joins and notes for exactly the rows fresh **at this instant**.
    pub fn render(&self, now: Instant) -> Vec<Cmd> {
        let mut cmds: Vec<Cmd> = self.ever.iter().map(MirrorId::removal).collect();
        for (mirror, body, deadline) in &self.live {
            let Some(ttl) = remaining_secs(*deadline, now) else {
                continue;
            };
            emit_row(&mut cmds, &mirror.id, body, ttl);
        }
        cmds
    }

    /// Every live mirrored row, for the session-end sweep and for seeding a
    /// joining spectator's applied set.
    pub fn live_ids(&self) -> Vec<MirrorId> {
        self.live.iter().map(|(id, _, _)| id.clone()).collect()
    }
}

/// Running counters, for the host status line and tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MirrorStats {
    pub joined: u64,
    pub reissued: u64,
    pub left: u64,
    pub expired: u64,
    /// Source rows whose id is not one the terminal would accept.
    pub skipped_id: u64,
    /// Source rows dropped because no free mirrored id could be minted.
    pub skipped_collision: u64,
    /// Source rows dropped because the spectator's 16-row roster is full.
    pub skipped_cap: u64,
    /// Mirrored ids that needed the truncate+hash rewrite.
    pub truncated: u64,
    /// Ids aged out of the leave-preamble's bounded memory.
    pub forgotten: u64,
}

struct Entry {
    mirror: MirrorId,
    /// The deadline the timers act on. Only ever tightens between real
    /// mutations, so the relay leads the primary's own expiry.
    deadline: Instant,
    /// The deadline as last *stated*, and the baseline the move test compares
    /// against. Kept separate from `deadline` so noise-driven tightening
    /// cannot narrow the epsilon and turn the next upward wobble into a
    /// spurious restatement.
    nominal: Instant,
    /// The body as last announced — a re-issue restates exactly this.
    body: Body,
    /// The cursor as last announced. A `replace=true` join clears the cursor
    /// at the terminal (`protocols/presence.md:83-90`), so a re-issue resets
    /// this to `None` and the cursor is restated after it.
    cursor_sent: Option<(u16, u16)>,
    missing: u8,
}

/// The mirrored roster: source rows on the primary ↔ synthesized rows on the
/// spectators.
#[derive(Default)]
pub struct Mirror {
    entries: BTreeMap<(Kind, u8, String), Entry>,
    taken: BTreeSet<MirrorId>,
    ever: BTreeSet<MirrorId>,
    /// Insertion order over `ever`, so the bounded memory ages out oldest
    /// first.
    ever_order: VecDeque<MirrorId>,
    pub stats: MirrorStats,
}

impl Mirror {
    pub fn new() -> Mirror {
        Mirror::default()
    }

    /// Fold one complete `state.presence` walk into the mirror.
    ///
    /// Two instants, deliberately distinct:
    ///
    /// - `at` is when the walk was **issued**. Deadlines derive from it, so a
    ///   lease computed off a page that took a round trip to arrive lands
    ///   *early* rather than late — the relay leads expiry, never trails it.
    /// - `now` is when the batch is **emitted**. Every emission is gated on
    ///   it, so a row whose lease ran out between issuing the query and
    ///   folding the answer is never stated. This is the emit-time
    ///   re-evaluation the design requires (`protocols/presence.md:70-80` —
    ///   leases are lazily computed, so freshness is decided at emit).
    ///
    /// `complete` is true when the walk was a single reply — one page at one
    /// instant, so a row's absence is exact. A paginated walk is not
    /// snapshot-stable, so absence there must repeat before it is believed.
    pub fn apply_walk(
        &mut self,
        rows: &[Row],
        at: Instant,
        now: Instant,
        complete: bool,
    ) -> Vec<Cmd> {
        let seen: BTreeSet<(Kind, u8, String)> = rows
            .iter()
            .map(|row| (row.kind(), row.ns, row.id.clone()))
            .collect();

        // Removals first, and computed first: a roster at its 16-row cap must
        // free the slot before an addition needs one
        // (`protocols/presence.md:92-97`).
        let mut gone: Vec<(Kind, u8, String)> = Vec::new();
        for (key, entry) in &mut self.entries {
            if seen.contains(key) {
                entry.missing = 0;
                continue;
            }
            entry.missing = entry.missing.saturating_add(1);
            if complete || entry.missing >= MISSING_STRIKES {
                gone.push(key.clone());
            }
        }
        let mut cmds = Vec::new();
        for key in gone {
            if let Some(entry) = self.entries.remove(&key) {
                self.taken.remove(&entry.mirror);
                self.stats.left += 1;
                cmds.push(entry.mirror.removal());
            }
        }

        for row in rows {
            // Freshness at emit time, not at poll time. `None` here is
            // exactly the expired case; it is skipped, never floored to a
            // token TTL.
            let Some(deadline) = deadline_of(row, at) else {
                continue;
            };
            let Some(ttl) = remaining_secs(deadline, now) else {
                continue;
            };
            let key = (row.kind(), row.ns, row.id.clone());

            let Some(entry) = self.entries.get_mut(&key) else {
                let Some(mirror) = self.mint(row) else {
                    continue;
                };
                self.remember(mirror.clone());
                self.taken.insert(mirror.clone());
                self.stats.joined += 1;
                emit_row(&mut cmds, &mirror.id, &row.body, ttl);
                self.entries.insert(
                    key,
                    Entry {
                        mirror,
                        deadline,
                        nominal: deadline,
                        body: row.body.clone(),
                        cursor_sent: cursor_of(&row.body),
                        missing: 0,
                    },
                );
                continue;
            };

            let cursor = cursor_of(&row.body);
            // Only a deadline that moved *later* is news. A row's deadline is
            // `issue instant + remaining`, and the remaining came back a
            // round trip after the query went out, so the estimate wobbles by
            // whatever the reply latency wobbles by. Restating on that noise
            // is pure wire chatter; restating on a genuine extension —
            // `user.renew`, a `replace`, a fresh lease — is the point.
            let moved = deadline > entry.nominal + DEADLINE_EPSILON;
            // A cursor that went away is a mutation the cursor command cannot
            // express — `user.cursor` can only move a caret, never retract
            // one. Only a `replace=true` join clears it, so a Some → None
            // transition takes the re-issue path.
            let cursor_cleared = cursor.is_none() && entry.cursor_sent.is_some();
            if moved || cursor_cleared || !entry.body.identity_eq(&row.body) {
                // A real mutation on the primary — restate the row, and with
                // it the lease. `replace=true` continues the revision.
                self.stats.reissued += 1;
                entry.deadline = deadline;
                entry.nominal = deadline;
                entry.body = row.body.clone();
                entry.cursor_sent = cursor;
                emit_row(&mut cmds, &entry.mirror.id, &row.body, ttl);
            } else {
                // Jitter that pulled the estimate *in* is kept: the relay's
                // own timer should only ever fire early relative to the
                // primary, never late. It ratchets no further than the
                // latency noise, and errs on the side of a row leaving a
                // spectator's roster a beat before the primary drops it.
                entry.deadline = entry.deadline.min(deadline);
                if let Some(cursor) = cursor
                    && entry.cursor_sent != Some(cursor)
                {
                    entry.cursor_sent = Some(cursor);
                    entry.body = row.body.clone();
                    cmds.push(Cmd::Cursor {
                        id: entry.mirror.id.clone(),
                        x: cursor.0,
                        y: cursor.1,
                    });
                }
            }
        }
        cmds
    }

    /// The deadline timers: rows whose lease has run out between polls leave
    /// the spectators' rosters rather than expiring in place there. Removing
    /// the row is what closes the namespace-0 retention the raw tee cannot.
    pub fn expire(&mut self, now: Instant) -> Vec<Cmd> {
        let due: Vec<(Kind, u8, String)> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.deadline <= now)
            .map(|(key, _)| key.clone())
            .collect();
        let mut cmds = Vec::new();
        for key in due {
            if let Some(entry) = self.entries.remove(&key) {
                self.taken.remove(&entry.mirror);
                self.stats.expired += 1;
                cmds.push(entry.mirror.removal());
            }
        }
        cmds
    }

    /// The primary reset: every spectator roster was already cleared by the
    /// forwarded `reset` bytes (`protocols/presence.md:162-164`), so the
    /// model drops silently — synthetic leaves would only be `unknown-id`
    /// noise in spectator error rings. The ever-broadcast set clears too:
    /// those ids no longer exist anywhere downstream.
    pub fn reset(&mut self) {
        self.entries.clear();
        self.taken.clear();
        self.ever.clear();
        self.ever_order.clear();
    }

    /// Add a mirrored id to the bounded leave-preamble memory. A live row is
    /// never aged out — the preamble exists to clear rows a rejoining
    /// spectator may still hold, and a live row is the likeliest of those.
    fn remember(&mut self, mirror: MirrorId) {
        if !self.ever.insert(mirror.clone()) {
            return;
        }
        self.ever_order.push_back(mirror);
        // Bounded rotation: live rows go to the back rather than being
        // forgotten, and the pass gives up once it has seen every entry, so
        // an all-live queue terminates instead of spinning.
        let mut rotations = 0;
        while self.ever_order.len() > EVER_MEMORY && rotations <= self.ever_order.len() {
            let Some(oldest) = self.ever_order.pop_front() else {
                break;
            };
            if self.taken.contains(&oldest) {
                self.ever_order.push_back(oldest);
                rotations += 1;
                continue;
            }
            self.ever.remove(&oldest);
            self.stats.forgotten += 1;
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            ever: self.ever.iter().cloned().collect(),
            live: self
                .entries
                .values()
                .map(|entry| (entry.mirror.clone(), entry.body.clone(), entry.deadline))
                .collect(),
        }
    }

    /// Mint the caller-local id's mirrored form: `r<ns>.<id>`, truncated with
    /// a salted hash suffix when the prefix would push it past the terminal's
    /// 48-byte cap, and collision-checked against the mirrored set. `r1.0.x`
    /// is reachable from both `(1, "0.x")` and `(10, "x")`, so even the
    /// untruncated form is checked.
    fn mint(&mut self, row: &Row) -> Option<MirrorId> {
        if !valid_source_id(&row.id) {
            self.stats.skipped_id += 1;
            return None;
        }
        let kind = row.kind();
        let cap = match kind {
            Kind::Participant => MAX_MIRROR_PARTICIPANTS,
            Kind::Note => MAX_MIRROR_NOTES,
        };
        if self.taken.iter().filter(|held| held.kind == kind).count() >= cap {
            // The spectator's roster is full: a join here rejects
            // `namespace-cap` and that row silently never appears. Decline
            // and count it; the next walk retries once a slot frees.
            self.stats.skipped_cap += 1;
            return None;
        }
        let prefix = format!("r{}.", row.ns);
        let direct = format!("{prefix}{}", row.id);
        if direct.len() <= MAX_MIRROR_ID_BYTES {
            let candidate = MirrorId {
                kind,
                id: direct.clone(),
            };
            if !self.taken.contains(&candidate) {
                return Some(candidate);
            }
        }
        for salt in 0..REWRITE_ATTEMPTS {
            let suffix = format!("-{:08x}", fnv1a32(row.ns, &row.id, salt));
            let head = MAX_MIRROR_ID_BYTES.saturating_sub(prefix.len() + suffix.len());
            let candidate = MirrorId {
                kind,
                id: format!("{prefix}{}{suffix}", &row.id[..head.min(row.id.len())]),
            };
            if !self.taken.contains(&candidate) {
                self.stats.truncated += 1;
                return Some(candidate);
            }
        }
        // Honest failure: no free id, so this participant does not appear —
        // counted rather than emitted as an id the spectator would reject.
        self.stats.skipped_collision += 1;
        None
    }
}

/// A row's absolute expiry, or `None` if it is already stale.
fn deadline_of(row: &Row, at: Instant) -> Option<Instant> {
    if !row.remaining.is_finite() || row.remaining <= 0.0 {
        return None;
    }
    Some(at + Duration::from_secs_f32(row.remaining.min(MAX_TTL_SECS)))
}

/// Seconds of lease left at `now`, or `None` once the deadline has passed.
fn remaining_secs(deadline: Instant, now: Instant) -> Option<f32> {
    if deadline <= now {
        return None;
    }
    Some(deadline.saturating_duration_since(now).as_secs_f32())
}

fn cursor_of(body: &Body) -> Option<(u16, u16)> {
    match body {
        Body::Participant { cursor, .. } => *cursor,
        Body::Note { .. } => None,
    }
}

/// One row's full statement: the `replace=true` join or note, plus the cursor
/// the join just cleared.
fn emit_row(out: &mut Vec<Cmd>, id: &str, body: &Body, ttl: f32) {
    match body {
        Body::Participant {
            name,
            color,
            cursor,
        } => {
            out.push(Cmd::Join {
                id: id.to_string(),
                name: name.clone(),
                color: color.clone(),
                ttl,
            });
            if let Some((x, y)) = cursor {
                out.push(Cmd::Cursor {
                    id: id.to_string(),
                    x: *x,
                    y: *y,
                });
            }
        }
        Body::Note { text, x, y } => out.push(Cmd::Note {
            id: id.to_string(),
            text: text.clone(),
            x: *x,
            y: *y,
            ttl,
        }),
    }
}

/// The terminal's id charset (`src/presence.rs:475-480`): non-empty ASCII
/// `[A-Za-z0-9_.-]`. A row that fails this could not have come from a healthy
/// registry; the mirror counts it rather than minting an id off it.
fn valid_source_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

/// FNV-1a over (namespace, id, salt), folded to 32 bits. Stable across runs
/// and platforms — a mirrored id must not change identity between polls.
fn fnv1a32(ns: u8, id: &str, salt: u16) -> u32 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mix = |byte: u8, hash: &mut u64| {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    };
    mix(ns, &mut hash);
    for byte in id.as_bytes() {
        mix(*byte, &mut hash);
    }
    mix((salt >> 8) as u8, &mut hash);
    mix(salt as u8, &mut hash);
    (hash ^ (hash >> 32)) as u32
}

/// A TTL the terminal will accept: finite, above zero, inside the documented
/// clamp, and rendered without exponent notation.
fn fmt_ttl(secs: f32) -> String {
    let value = if secs.is_finite() {
        secs.clamp(MIN_EMITTED_TTL_SECS, MAX_TTL_SECS)
    } else {
        MIN_EMITTED_TTL_SECS
    };
    format!("{value:.3}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Most tests fold a walk at the instant it was issued: `at` and the
    /// emit instant coincide. The cases that care about the gap between them
    /// call `apply_walk` directly.
    fn fold(mirror: &mut Mirror, rows: &[Row], at: Instant, complete: bool) -> Vec<Cmd> {
        mirror.apply_walk(rows, at, at, complete)
    }

    fn participant(ns: u8, id: &str, remaining: f32) -> Row {
        Row {
            ns,
            id: id.to_string(),
            remaining,
            body: Body::Participant {
                name: "Ann".to_string(),
                color: "#00ffcc".to_string(),
                cursor: Some((3, 4)),
            },
        }
    }

    fn note(ns: u8, id: &str, remaining: f32) -> Row {
        Row {
            ns,
            id: id.to_string(),
            remaining,
            body: Body::Note {
                text: "review this".to_string(),
                x: 2,
                y: 6,
            },
        }
    }

    /// Every synthesized command must round-trip through the terminal's own
    /// parser as the presence command it claims to be — the whole point of
    /// building the wire with `osc::osc_sequence`.
    fn reparse(cmd: &Cmd) -> osc::RattyAiCommand {
        let wire = cmd.to_wire();
        let body = wire
            .strip_prefix("\x1b]777;")
            .and_then(|rest| rest.strip_suffix('\x07'))
            .expect("synthesized sequences are BEL-terminated 777s");
        osc::parse_control(body)
            .and_then(|control| control.command)
            .unwrap_or_else(|| panic!("the crate parser rejected {body:?}"))
    }

    #[test]
    fn synthesized_commands_reparse_as_presence_control() {
        let cmds = [
            Cmd::Join {
                id: "r0.ann".into(),
                name: "Ann W".into(),
                color: "#00ffcc".into(),
                ttl: 42.5,
            },
            Cmd::Cursor {
                id: "r0.ann".into(),
                x: 12,
                y: 4,
            },
            Cmd::Note {
                id: "r0.n1".into(),
                text: "a=b & c; done".into(),
                x: 15,
                y: 10,
                ttl: 600.0,
            },
            Cmd::Leave {
                id: "r0.ann".into(),
            },
            Cmd::NoteRemove { id: "r0.n1".into() },
        ];
        for cmd in &cmds {
            assert!(
                reparse(cmd).is_presence_control(),
                "{cmd:?} is not presence-control after a round trip"
            );
        }
    }

    #[test]
    fn joins_and_notes_always_replace_and_carry_their_fields() {
        match reparse(&Cmd::Join {
            id: "r0.ann".into(),
            name: "Ann W".into(),
            color: "#00ffcc".into(),
            ttl: 42.5,
        }) {
            osc::RattyAiCommand::UserJoin {
                id,
                name,
                color,
                ttl,
                replace,
            } => {
                assert_eq!(id, "r0.ann");
                assert_eq!(name, "Ann W");
                assert_eq!(color, "#00ffcc");
                assert_eq!(ttl, Some(42.5));
                assert!(replace, "synthesized joins must carry replace=true");
            }
            other => panic!("expected UserJoin, got {other:?}"),
        }
        // Grammar delimiters inside note text survive percent-encoding.
        match reparse(&Cmd::Note {
            id: "r0.n1".into(),
            text: "a=b & c; done".into(),
            x: 15,
            y: 10,
            ttl: 600.0,
        }) {
            osc::RattyAiCommand::Note {
                id,
                text,
                x,
                y,
                ttl,
                replace,
            } => {
                assert_eq!(id, "r0.n1");
                assert_eq!(text, "a=b & c; done");
                assert_eq!((x, y), (15, 10));
                assert_eq!(ttl, Some(600.0));
                assert!(replace, "synthesized notes must carry replace=true");
            }
            other => panic!("expected Note, got {other:?}"),
        }
    }

    #[test]
    fn ttl_never_formats_to_a_value_the_terminal_rejects() {
        // `pin_ttl` rejects non-positive and non-finite TTLs outright.
        for secs in [0.0, -1.0, 0.000_04, f32::NAN, f32::INFINITY, 1e9] {
            let text = fmt_ttl(secs);
            let parsed: f32 = text.parse().expect("finite decimal");
            assert!(
                parsed.is_finite() && parsed > 0.0 && parsed <= MAX_TTL_SECS,
                "fmt_ttl({secs}) = {text}"
            );
        }
        assert_eq!(fmt_ttl(42.5), "42.500");
    }

    #[test]
    fn mirror_ids_respect_the_cap() {
        let mut mirror = Mirror::new();
        let long = "a".repeat(60);
        let at = Instant::now();
        let cmds = fold(
            &mut mirror,
            &[participant(0, "ann", 30.0), participant(7, &long, 30.0)],
            at,
            true,
        );
        let ids: Vec<String> = cmds
            .iter()
            .filter_map(|cmd| match cmd {
                Cmd::Join { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"r0.ann".to_string()));
        for id in &ids {
            assert!(id.len() <= MAX_MIRROR_ID_BYTES, "{id} exceeds the cap");
            assert!(valid_source_id(id), "{id} is not a legal id");
        }
        assert_eq!(mirror.stats.truncated, 1);
    }

    #[test]
    fn colliding_rewrites_get_distinct_ids() {
        // `r1.0.x` is reachable from both (1, "0.x") and (10, "x") — the
        // untruncated form is collision-checked for exactly this.
        let mut mirror = Mirror::new();
        let at = Instant::now();
        let cmds = fold(
            &mut mirror,
            &[participant(1, "0.x", 30.0), participant(10, "x", 30.0)],
            at,
            true,
        );
        let ids: Vec<String> = cmds
            .iter()
            .filter_map(|cmd| match cmd {
                Cmd::Join { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 2, "both participants must appear");
        assert_ne!(ids[0], ids[1], "colliding sources shared a mirrored id");
        assert_eq!(mirror.stats.skipped_collision, 0);
    }

    #[test]
    fn a_steady_row_is_stated_once_and_not_restated() {
        let mut mirror = Mirror::new();
        let start = Instant::now();
        let first = fold(&mut mirror, &[participant(0, "ann", 60.0)], start, true);
        assert_eq!(first.len(), 2, "join + cursor: {first:?}");

        // One second later the primary reports one second less remaining —
        // the same absolute deadline, so nothing is restated.
        let later = start + Duration::from_secs(1);
        let quiet = fold(&mut mirror, &[participant(0, "ann", 59.0)], later, true);
        assert!(quiet.is_empty(), "steady row was restated: {quiet:?}");
        assert_eq!(mirror.stats.reissued, 0);
    }

    #[test]
    fn reply_latency_jitter_does_not_restate_a_steady_row() {
        // Each walk's deadline estimate is `issue instant + remaining`, and
        // the remaining arrives a round trip later — so the estimate wobbles
        // with the latency. That wobble is not a mutation and must not put a
        // command on the wire.
        let mut mirror = Mirror::new();
        let start = Instant::now();
        fold(&mut mirror, &[participant(0, "ann", 60.0)], start, true);

        // Ten walks a second apart, each reporting an age that swings ±400 ms
        // around the true one — well inside the epsilon, either direction.
        let swing = [0.4_f32, -0.3, 0.35, -0.4, 0.2, -0.25, 0.4, -0.35, 0.3, -0.4];
        for (n, jitter) in swing.iter().enumerate() {
            let elapsed = (n + 1) as f32;
            let issued = start + Duration::from_secs_f32(elapsed);
            let row = participant(0, "ann", 60.0 - elapsed + jitter);
            let cmds = fold(&mut mirror, &[row], issued, true);
            assert!(cmds.is_empty(), "walk {n} restated on jitter: {cmds:?}");
        }
        assert_eq!(mirror.stats.reissued, 0);

        // The stored deadline only ever tightened, so the relay still leads
        // the primary's own expiry rather than trailing it.
        let (_, _, deadline) = mirror.snapshot().live[0].clone();
        assert!(deadline <= start + Duration::from_secs_f32(60.0));
    }

    #[test]
    fn a_renewal_restates_the_row_and_its_cursor() {
        let mut mirror = Mirror::new();
        let start = Instant::now();
        fold(&mut mirror, &[participant(0, "ann", 10.0)], start, true);
        let later = start + Duration::from_secs(1);
        let cmds = fold(&mut mirror, &[participant(0, "ann", 60.0)], later, true);
        assert!(
            matches!(cmds.first(), Some(Cmd::Join { .. })),
            "a renewal must restate the join: {cmds:?}"
        );
        assert!(
            matches!(cmds.get(1), Some(Cmd::Cursor { .. })),
            "a replace clears the cursor, so it must be restated: {cmds:?}"
        );
        assert_eq!(mirror.stats.reissued, 1);
    }

    #[test]
    fn a_moved_cursor_emits_only_a_cursor() {
        let mut mirror = Mirror::new();
        let start = Instant::now();
        fold(&mut mirror, &[participant(0, "ann", 60.0)], start, true);
        let mut moved = participant(0, "ann", 59.0);
        if let Body::Participant { cursor, .. } = &mut moved.body {
            *cursor = Some((9, 9));
        }
        let cmds = fold(&mut mirror, &[moved], start + Duration::from_secs(1), true);
        assert_eq!(
            cmds,
            vec![Cmd::Cursor {
                id: "r0.ann".into(),
                x: 9,
                y: 9
            }]
        );
    }

    #[test]
    fn removals_precede_additions_so_a_capped_roster_has_room() {
        let mut mirror = Mirror::new();
        let at = Instant::now();
        fold(&mut mirror, &[participant(0, "gone", 60.0)], at, true);
        let cmds = fold(&mut mirror, &[participant(0, "fresh", 60.0)], at, true);
        assert!(
            matches!(cmds.first(), Some(Cmd::Leave { id }) if id == "r0.gone"),
            "the leave must lead the batch: {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Join { id, .. } if id == "r0.fresh"))
        );
    }

    #[test]
    fn a_paginated_walk_needs_two_misses_before_tearing_a_row_down() {
        let mut mirror = Mirror::new();
        let at = Instant::now();
        fold(&mut mirror, &[participant(0, "ann", 60.0)], at, false);
        let first_miss = fold(&mut mirror, &[], at, false);
        assert!(
            first_miss.is_empty(),
            "a single miss on a non-atomic walk must not tear down: {first_miss:?}"
        );
        let second_miss = fold(&mut mirror, &[], at, false);
        assert_eq!(
            second_miss,
            vec![Cmd::Leave {
                id: "r0.ann".into()
            }]
        );
    }

    #[test]
    fn a_single_page_walk_tears_down_immediately() {
        let mut mirror = Mirror::new();
        let at = Instant::now();
        fold(&mut mirror, &[note(0, "n1", 60.0)], at, true);
        assert_eq!(
            fold(&mut mirror, &[], at, true),
            vec![Cmd::NoteRemove { id: "r0.n1".into() }]
        );
    }

    #[test]
    fn deadlines_remove_rows_rather_than_letting_them_expire_in_place() {
        let mut mirror = Mirror::new();
        let start = Instant::now();
        fold(
            &mut mirror,
            &[participant(0, "ann", 5.0), note(0, "n1", 60.0)],
            start,
            true,
        );
        assert!(mirror.expire(start + Duration::from_secs(4)).is_empty());
        assert_eq!(
            mirror.expire(start + Duration::from_secs(6)),
            vec![Cmd::Leave {
                id: "r0.ann".into()
            }]
        );
        assert_eq!(mirror.stats.expired, 1);
        // The note's own lease is untouched by the participant's expiry.
        assert_eq!(mirror.snapshot().live.len(), 1);
    }

    #[test]
    fn stale_rows_are_never_synthesized() {
        let mut mirror = Mirror::new();
        let at = Instant::now();
        assert!(
            mirror
                .apply_walk(
                    &[participant(0, "ann", 0.0), note(0, "n1", -3.0)],
                    at,
                    at,
                    true
                )
                .is_empty()
        );
        assert!(mirror.snapshot().live.is_empty());
    }

    #[test]
    fn a_snapshot_leads_with_a_preamble_and_re_evaluates_freshness() {
        let mut mirror = Mirror::new();
        let start = Instant::now();
        fold(
            &mut mirror,
            &[participant(0, "ann", 5.0), participant(0, "bo", 600.0)],
            start,
            true,
        );
        // `ann` left, so its id is no longer live — but it was broadcast, and
        // a rejoining spectator may still hold the row.
        fold(&mut mirror, &[participant(0, "bo", 600.0)], start, true);

        let snapshot = mirror.snapshot();
        let cmds = snapshot.render(start + Duration::from_secs(1));
        let preamble = cmds
            .iter()
            .take_while(|cmd| matches!(cmd, Cmd::Leave { .. } | Cmd::NoteRemove { .. }))
            .count();
        assert_eq!(preamble, 2, "both ever-broadcast ids must be cleared first");
        assert!(
            cmds[preamble..]
                .iter()
                .any(|cmd| matches!(cmd, Cmd::Join { id, .. } if id == "r0.bo")),
            "the live row must follow the preamble: {cmds:?}"
        );
        assert!(
            !cmds[preamble..]
                .iter()
                .any(|cmd| matches!(cmd, Cmd::Join { id, .. } if id == "r0.ann")),
            "a departed row must not be re-joined: {cmds:?}"
        );

        // Rendered after every lease has run out: preamble only.
        let late = snapshot.render(start + Duration::from_secs(3600));
        assert_eq!(late.len(), 2, "expired rows must not render: {late:?}");
    }

    #[test]
    fn reset_drops_the_model_silently() {
        let mut mirror = Mirror::new();
        let at = Instant::now();
        fold(&mut mirror, &[participant(0, "ann", 60.0)], at, true);
        mirror.reset();
        let snapshot = mirror.snapshot();
        assert!(snapshot.live.is_empty());
        assert!(
            snapshot.ever.is_empty(),
            "reset cleared every spectator roster, so nothing is left to clear"
        );
        // A row seen after the reset is a fresh lineage, joined again.
        let cmds = fold(&mut mirror, &[participant(0, "ann", 60.0)], at, true);
        assert!(matches!(cmds.first(), Some(Cmd::Join { .. })));
    }

    #[test]
    fn rows_decode_from_the_documented_projection() {
        let row = Row::from_json(&json!({
            "kind": "participant", "ns": 0, "id": "ann", "name": "Ann",
            "color": "#00ffcc", "cursor": { "x": 3, "y": 4 },
            "fresh": true, "age_secs": 12.0, "ttl_secs": 60.0, "revision": 2
        }))
        .expect("a documented participant row decodes");
        assert_eq!(row.ns, 0);
        assert_eq!(row.id, "ann");
        assert!((row.remaining - 48.0).abs() < 1e-3);
        assert_eq!(
            row.body,
            Body::Participant {
                name: "Ann".into(),
                color: "#00ffcc".into(),
                cursor: Some((3, 4)),
            }
        );

        let note = Row::from_json(&json!({
            "kind": "note", "ns": 2, "id": "n1", "text": "hi", "x": 5, "y": 6,
            "fresh": true, "age_secs": 1.0, "ttl_secs": 300.0, "revision": 1
        }))
        .expect("a documented note row decodes");
        assert_eq!(note.kind(), Kind::Note);
        assert_eq!(
            note.body,
            Body::Note {
                text: "hi".into(),
                x: 5,
                y: 6
            }
        );

        // A cursorless participant is legal; an expired row is not mirrored;
        // an unknown kind is never guessed at.
        assert!(
            Row::from_json(&json!({
                "kind": "participant", "ns": 0, "id": "b", "name": "B", "color": "#00ff00",
                "cursor": null, "fresh": true, "age_secs": 0.0, "ttl_secs": 60.0, "revision": 1
            }))
            .is_some()
        );
        assert!(
            Row::from_json(&json!({
                "kind": "participant", "ns": 0, "id": "b", "name": "B", "color": "#00ff00",
                "cursor": null, "fresh": false, "age_secs": 90.0, "ttl_secs": 60.0, "revision": 1
            }))
            .is_none(),
            "an expired row must never reach the mirror"
        );
        assert!(Row::from_json(&json!({ "kind": "gizmo", "fresh": true })).is_none());
    }

    #[test]
    fn a_row_that_expired_during_the_round_trip_is_never_synthesized() {
        // The walk is issued at `start` and answered a second later. The row
        // had half a second of lease left when the primary produced it, so it
        // is stale by the time the answer lands — freshness is decided at
        // emit, not at poll.
        let mut mirror = Mirror::new();
        let start = Instant::now();
        let answered = start + Duration::from_millis(1_100);
        let cmds = mirror.apply_walk(&[participant(0, "ann", 0.5)], start, answered, true);
        assert!(cmds.is_empty(), "an expired row was stated: {cmds:?}");
        assert!(mirror.snapshot().live.is_empty());

        // The same row with real lease left still crosses.
        let cmds = mirror.apply_walk(&[participant(0, "ann", 60.0)], start, answered, true);
        assert!(matches!(cmds.first(), Some(Cmd::Join { .. })), "{cmds:?}");
    }

    #[test]
    fn the_spectators_roster_cap_is_modelled_rather_than_discovered() {
        // A join past the spectator's 16-row cap rejects `namespace-cap` and
        // that participant silently never appears. The mirror declines and
        // counts instead, and takes the slot as soon as one frees.
        let mut mirror = Mirror::new();
        let at = Instant::now();
        let full: Vec<Row> = (0..MAX_MIRROR_PARTICIPANTS + 1)
            .map(|n| participant(0, &format!("p{n}"), 60.0))
            .collect();
        let cmds = fold(&mut mirror, &full, at, true);
        let joins = cmds
            .iter()
            .filter(|cmd| matches!(cmd, Cmd::Join { .. }))
            .count();
        assert_eq!(joins, MAX_MIRROR_PARTICIPANTS);
        assert_eq!(mirror.stats.skipped_cap, 1);

        // One participant leaves; the refused row takes the freed slot on the
        // very next walk rather than being lost.
        let mut freed: Vec<Row> = full[1..].to_vec();
        freed.rotate_left(0);
        let cmds = fold(&mut mirror, &freed, at, true);
        assert!(
            matches!(cmds.first(), Some(Cmd::Leave { id }) if id == "r0.p0"),
            "the leave must lead so the slot is free: {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd, Cmd::Join { id, .. } if id == "r0.p16")),
            "the refused row must be minted once a slot frees: {cmds:?}"
        );
        assert_eq!(mirror.stats.skipped_cap, 1, "no second refusal");

        // Notes have their own 16 slots — a full participant roster does not
        // starve them.
        let mut notes: Vec<Row> = freed.clone();
        notes.push(note(0, "n1", 60.0));
        let cmds = fold(&mut mirror, &notes, at, true);
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd, Cmd::Note { id, .. } if id == "r0.n1")),
            "{cmds:?}"
        );
    }

    #[test]
    fn a_cursor_cleared_on_the_primary_is_withdrawn() {
        // `user.cursor` can move a caret but never retract one — only a
        // `replace=true` join clears it. A Some -> None transition must
        // therefore take the re-issue path, or the stale caret would live on
        // in every late-join snapshot.
        let mut mirror = Mirror::new();
        let start = Instant::now();
        fold(&mut mirror, &[participant(0, "ann", 60.0)], start, true);

        let mut cleared = participant(0, "ann", 59.9);
        if let Body::Participant { cursor, .. } = &mut cleared.body {
            *cursor = None;
        }
        let later = start + Duration::from_millis(100);
        let cmds = fold(&mut mirror, &[cleared], later, true);
        assert!(
            matches!(cmds.as_slice(), [Cmd::Join { .. }]),
            "a cleared cursor must re-issue the join and nothing else: {cmds:?}"
        );
        assert!(
            !mirror
                .snapshot()
                .render(later)
                .iter()
                .any(|cmd| matches!(cmd, Cmd::Cursor { .. })),
            "a withdrawn caret must not survive into the snapshot"
        );
    }

    #[test]
    fn the_leave_preamble_is_bounded_and_never_forgets_a_live_row() {
        let mut mirror = Mirror::new();
        let at = Instant::now();
        // One row stays for the whole session; the rest churn through.
        for round in 0..EVER_MEMORY * 2 {
            let rows = vec![
                participant(0, "resident", 3600.0),
                participant(0, &format!("churn{round}"), 60.0),
            ];
            fold(&mut mirror, &rows, at, true);
        }
        let snapshot = mirror.snapshot();
        assert!(
            snapshot.ever.len() <= EVER_MEMORY,
            "the preamble grew to {}",
            snapshot.ever.len()
        );
        assert!(
            snapshot
                .ever
                .contains(&MirrorId::participant("r0.resident")),
            "a live row was aged out of the preamble"
        );
        assert!(mirror.stats.forgotten > 0, "nothing was ever bounded away");
    }

    #[test]
    fn an_illegal_source_id_is_counted_not_mirrored() {
        let mut mirror = Mirror::new();
        let cmds = fold(
            &mut mirror,
            &[participant(0, "ann bo", 60.0)],
            Instant::now(),
            true,
        );
        assert!(cmds.is_empty());
        assert_eq!(mirror.stats.skipped_id, 1);
    }
}
