//! The avatar organ (#23, M3.10): a scene-owned presence singleton with a
//! fair bounded speech queue.
//!
//! The avatar belongs to the **ratty scene**, not to whichever agent spoke
//! last. Ordinary agents submit attributed speech and gestures; only the
//! trusted avatar-scene capability ([`crate::capability::SceneCapability`],
//! granted by `[trust.local] avatar_scene`) may change the representation,
//! hide/show globally, clear another agent's speech, or cancel the whole
//! queue. Attribution authority is always the ingress-derived namespace —
//! the optional `from=` label is presentation decoration, never identity.
//!
//! `avatar.speak` is a long-running operation (#18): its single ack is
//! `ok=1;code=started|queued` with an execution handle, queue position,
//! and estimated wait; completion is observed by polling
//! `state.executions` (absence = finished), never pushed.
//!
//! ## The fair queue
//!
//! One active utterance; at most [`MAX_PENDING_UTTERANCES_GLOBAL`] pending
//! globally and [`MAX_PENDING_UTTERANCES_PER_AGENT`] per agent; FIFO
//! within an agent. Rotation across agents is **seniority-snapshot
//! round-robin**: when the current rotation pass empties, the next pass
//! snapshots every namespace with pending speech, ordered by how long
//! its oldest pending utterance has waited (admission sequence). An
//! utterance admitted mid-pass is not eligible until the next snapshot.
//! This gives a provable start bound: once admitted, at most the other
//! pending namespaces at admission time (≤ 3 under the caps) are ever
//! served before you — anything admitted later has a younger sequence and
//! lands behind you in every future snapshot — so the worst-case wait is
//! `remaining(active) + 3 × MAX_UTTERANCE_MS`, regardless of other
//! agents' behavior. (Plain cyclic-namespace rotation does not have this
//! bound: adversarial admissions inside the cyclic gap can stretch it
//! ~85×.)
//!
//! Today only [`IngressSource::Local`] (namespace 0) exists, so
//! multi-agent fairness is exercised by registry-level tests with
//! explicit namespaces until an authenticated ingress lands — the same
//! stance as the per-namespace macro slots.
//!
//! Honest limitations: durations advance on `Res<Time>`, so on a hidden
//! wasm tab expiry and promotion stall with everything else; the queue
//! order never changes.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::prelude::*;
use serde_json::{Value, json};

use crate::ai::AiCommand;
use crate::capability::SceneCapability;
use crate::config::AppConfig;
use crate::osc::{RattyAiCommand, SpeechClearScope};
use crate::query::codes;
use crate::query_channel::{
    AckOutcome, AiDiagnostics, QuerySession, ack_commit, ack_commit_long_running, reject,
};

// ── Limits (advertised in `caps.limits`) ──

/// Byte cap on one utterance's text (percent-decoded UTF-8) — #23 §2.
pub const MAX_AVATAR_TEXT_BYTES: usize = 1000;
/// Byte cap on the optional `from=` speaker label.
pub const MAX_AVATAR_SPEAKER_BYTES: usize = 64;
/// Utterance duration clamp floor (the locked 750 ms).
pub const MIN_UTTERANCE_MS: u64 = 750;
/// Utterance duration clamp ceiling (the locked 15 s).
pub const MAX_UTTERANCE_MS: u64 = 15_000;
/// Reading-speed heuristic for text-derived durations (~200 wpm).
pub const UTTERANCE_MS_PER_CHAR: u64 = 60;
/// Global pending cap (the active utterance is not counted).
pub const MAX_PENDING_UTTERANCES_GLOBAL: usize = 4;
/// Per-agent pending cap.
pub const MAX_PENDING_UTTERANCES_PER_AGENT: usize = 1;
/// Bound on the `dx`/`dy` anchor offset, logical px (clamped, not rejected
/// — the sound organ's wire semantics: the wire requests, it never owns).
pub const AVATAR_OFFSET_MAX_PX: f32 = 512.0;
/// HUD scale clamp floor.
pub const AVATAR_SCALE_MIN: f32 = 0.25;
/// HUD scale clamp ceiling.
pub const AVATAR_SCALE_MAX: f32 = 4.0;

// ── Registry (#12: immutable ids over embedded bytes) ──

/// One entry of the terminal-side avatar model registry: how an immutable
/// wire registry id (see [`crate::osc::AVATAR_MODELS`]) resolves to an
/// embedded asset and its presentation normalization. Ids stay in
/// lockstep with the wire vocabulary (a test pins this); files and scales
/// are terminal-side detail the wire never sees.
#[derive(Debug, Clone, Copy)]
pub struct AvatarModelSpec {
    /// The canonical registry id.
    pub id: &'static str,
    /// Embedded asset file name under `assets/objects/` (never a path).
    pub file: &'static str,
    /// The `embedded://` URL the asset server loads (seeded by
    /// [`crate::model::seed_embedded_scene_assets`] on both targets).
    pub embedded_url: &'static str,
    /// Uniform display scale applied to the spawned scene root.
    pub scale: f32,
}

/// The curated avatar registry. M3 ships exactly one validated model
/// (#23 §3, build-audited); rows are append-only.
pub const AVATAR_REGISTRY: &[AvatarModelSpec] = &[AvatarModelSpec {
    id: "mascot",
    file: "SpinyMouse.glb",
    embedded_url: "embedded://objects/SpinyMouse.glb",
    scale: 1.0,
}];

/// Looks up the registry entry for an id, or `None` when unregistered.
pub fn avatar_model_spec(id: &str) -> Option<&'static AvatarModelSpec> {
    AVATAR_REGISTRY.iter().find(|spec| spec.id == id)
}

// ── Anchors and choreographies ──

/// The nine named screen-space HUD anchors (#23 §3): corners, edge
/// midpoints, and center. The avatar is presence furniture — window-fixed
/// through terminal scrolling and camera movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvatarAnchor {
    /// `top-left`.
    TopLeft,
    /// `top`.
    Top,
    /// `top-right`.
    TopRight,
    /// `left`.
    Left,
    /// `center`.
    Center,
    /// `right`.
    Right,
    /// `bottom-left`.
    BottomLeft,
    /// `bottom`.
    Bottom,
    /// `bottom-right` — the locked default.
    BottomRight,
}

impl AvatarAnchor {
    /// Every anchor with its wire name, in vocabulary order.
    pub const ALL: &[(AvatarAnchor, &'static str)] = &[
        (AvatarAnchor::TopLeft, "top-left"),
        (AvatarAnchor::Top, "top"),
        (AvatarAnchor::TopRight, "top-right"),
        (AvatarAnchor::Left, "left"),
        (AvatarAnchor::Center, "center"),
        (AvatarAnchor::Right, "right"),
        (AvatarAnchor::BottomLeft, "bottom-left"),
        (AvatarAnchor::Bottom, "bottom"),
        (AvatarAnchor::BottomRight, "bottom-right"),
    ];

    /// Parses a wire anchor name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(_, wire)| *wire == name)
            .map(|(anchor, _)| *anchor)
    }

    /// The wire name.
    pub fn as_str(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(anchor, _)| *anchor == self)
            .map(|(_, wire)| *wire)
            .expect("every anchor is in ALL")
    }

    /// Normalized window coordinates, origin top-left, y down: `{0,.5,1}²`.
    pub fn normalized(self) -> Vec2 {
        match self {
            AvatarAnchor::TopLeft => Vec2::new(0.0, 0.0),
            AvatarAnchor::Top => Vec2::new(0.5, 0.0),
            AvatarAnchor::TopRight => Vec2::new(1.0, 0.0),
            AvatarAnchor::Left => Vec2::new(0.0, 0.5),
            AvatarAnchor::Center => Vec2::new(0.5, 0.5),
            AvatarAnchor::Right => Vec2::new(1.0, 0.5),
            AvatarAnchor::BottomLeft => Vec2::new(0.0, 1.0),
            AvatarAnchor::Bottom => Vec2::new(0.5, 1.0),
            AvatarAnchor::BottomRight => Vec2::new(1.0, 1.0),
        }
    }
}

/// The named root-transform choreographies (#23 §1: no skinning, no
/// animation-graph exposure in M3 — the command surface stays semantic so
/// a later skinned milestone replaces the mascot behind the same verbs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvatarChoreography {
    /// A gentle vertical bounce.
    Bob,
    /// A ±z sway.
    Tilt,
    /// An eased lean toward the bubble and back.
    Lean,
    /// Two quick x-rotation dips.
    Nod,
    /// One full y-turn.
    Spin,
}

impl AvatarChoreography {
    /// Every choreography with its wire name.
    pub const ALL: &[(AvatarChoreography, &'static str)] = &[
        (AvatarChoreography::Bob, "bob"),
        (AvatarChoreography::Tilt, "tilt"),
        (AvatarChoreography::Lean, "lean"),
        (AvatarChoreography::Nod, "nod"),
        (AvatarChoreography::Spin, "spin"),
    ];

    /// Parses a wire gesture name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(_, wire)| *wire == name)
            .map(|(which, _)| *which)
    }

    /// The wire name.
    pub fn as_str(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(which, _)| *which == self)
            .map(|(_, wire)| *wire)
            .expect("every choreography is in ALL")
    }

    /// One-shot envelope length, seconds.
    pub fn duration_secs(self) -> f32 {
        match self {
            AvatarChoreography::Bob => 1.0,
            AvatarChoreography::Tilt => 1.2,
            AvatarChoreography::Lean => 1.2,
            AvatarChoreography::Nod => 0.9,
            AvatarChoreography::Spin => 1.4,
        }
    }
}

/// A one-shot gesture in progress on the mascot root.
#[derive(Debug, Clone, Copy)]
pub struct ActiveGesture {
    /// The choreography playing.
    pub which: AvatarChoreography,
    /// `Time::elapsed` at commit.
    pub started: Duration,
}

// ── Speech queue ──

/// One admitted utterance; everything here is pinned at admission.
#[derive(Debug, Clone)]
pub struct Utterance {
    /// Execution handle ([`QuerySession::mint_execution_id`]).
    pub id: String,
    /// Owning agent namespace — the attribution authority.
    pub namespace: u8,
    /// Admission sequence: the seniority key for rotation snapshots.
    seq: u64,
    /// The text. Projected only to its owner (`state.executions`); never
    /// to other agents (`state.scene` carries no text).
    pub text: String,
    /// Optional presentation label (`from=`), decoration only.
    pub from: Option<String>,
    /// Pinned display duration (750 ms – 15 s).
    pub duration: Duration,
}

/// The utterance holding the voice.
#[derive(Debug, Clone)]
pub struct ActiveUtterance {
    /// The utterance.
    pub utterance: Utterance,
    /// `Time::elapsed` when it took the voice.
    pub started: Duration,
}

/// How an admitted `avatar.speak` entered the organ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeakAdmission {
    /// Took the voice immediately.
    Started,
    /// Admitted to the queue at this position (1 = next after the active
    /// utterance) with this estimated wait.
    Queued {
        /// Utterances (active + pending) served before this one, assuming
        /// no cancellations and no earlier-seniority admissions (there
        /// can be none — see the module doc).
        position: usize,
        /// Estimated wait until this utterance takes the voice.
        eta: Duration,
    },
}

/// Wire code + human message, the `MacroReject` pattern.
type AvatarReject = (&'static str, String);

/// The fair bounded speech queue (see the module doc for the rotation
/// discipline and its bound).
#[derive(Debug, Default)]
pub struct SpeechQueue {
    active: Option<ActiveUtterance>,
    /// Per-agent FIFO queues. `VecDeque` keeps FIFO-within-agent
    /// structural if the per-agent cap ever rises above 1.
    queues: HashMap<u8, VecDeque<Utterance>>,
    /// Invariant: Σ `queues[*].len()` — bounded by the global cap.
    pending_total: usize,
    /// Namespaces still owed a turn in the current rotation pass.
    pass: VecDeque<u8>,
    /// Monotone admission counter (the seniority key).
    next_seq: u64,
}

impl SpeechQueue {
    /// Admits one utterance. Check order: agent cap → global cap. Starts
    /// immediately iff nothing is active AND nothing is pending — a
    /// same-frame cancel never lets a new admission jump the queue.
    pub fn admit(
        &mut self,
        mut utterance: Utterance,
        now: Duration,
    ) -> Result<SpeakAdmission, AvatarReject> {
        let namespace = utterance.namespace;
        if self
            .queues
            .get(&namespace)
            .is_some_and(|q| q.len() >= MAX_PENDING_UTTERANCES_PER_AGENT)
        {
            return Err((
                codes::AGENT_QUEUE_FULL,
                "this agent already has its maximum pending utterances queued".to_string(),
            ));
        }
        utterance.seq = self.next_seq;
        self.next_seq += 1;
        if self.active.is_none() && self.pending_total == 0 {
            self.active = Some(ActiveUtterance {
                utterance,
                started: now,
            });
            return Ok(SpeakAdmission::Started);
        }
        if self.pending_total >= MAX_PENDING_UTTERANCES_GLOBAL {
            return Err((
                codes::BUSY,
                "the avatar speech queue is at its global pending cap".to_string(),
            ));
        }
        let seq = utterance.seq;
        self.queues.entry(namespace).or_default().push_back(utterance);
        self.pending_total += 1;
        let (position, eta) = self
            .forecast(namespace, seq, now)
            .expect("the just-admitted utterance is pending");
        Ok(SpeakAdmission::Queued { position, eta })
    }

    /// Simulates the rotation forward to the utterance `(namespace, seq)`:
    /// how many utterances are served before it and their summed wait.
    /// Exact under no-cancellation (later admissions cannot move ahead —
    /// the seniority-snapshot property).
    fn forecast(&self, namespace: u8, seq: u64, now: Duration) -> Option<(usize, Duration)> {
        let mut position = 0usize;
        let mut eta = Duration::ZERO;
        if let Some(active) = &self.active {
            position += 1;
            eta += (active.started + active.utterance.duration).saturating_sub(now);
        }
        let mut cursors: HashMap<u8, usize> = HashMap::new();
        let mut pass: VecDeque<u8> = self.pass.clone();
        loop {
            if pass.is_empty() {
                pass = self.snapshot_pass(&cursors);
                if pass.is_empty() {
                    return None;
                }
            }
            let ns = pass.pop_front().expect("pass is non-empty");
            let cursor = cursors.entry(ns).or_insert(0);
            let Some(utterance) = self.queues.get(&ns).and_then(|q| q.get(*cursor)) else {
                continue;
            };
            *cursor += 1;
            if utterance.namespace == namespace && utterance.seq == seq {
                return Some((position, eta));
            }
            position += 1;
            eta += utterance.duration;
        }
    }

    /// The next rotation pass: every namespace with pending speech (past
    /// the simulation `cursors`), ordered by its oldest pending
    /// utterance's admission sequence — seniority, not namespace number.
    fn snapshot_pass(&self, cursors: &HashMap<u8, usize>) -> VecDeque<u8> {
        let mut entries: Vec<(u64, u8)> = self
            .queues
            .iter()
            .filter_map(|(ns, queue)| {
                let cursor = cursors.get(ns).copied().unwrap_or(0);
                queue.get(cursor).map(|u| (u.seq, *ns))
            })
            .collect();
        entries.sort_unstable();
        entries.into_iter().map(|(_, ns)| ns).collect()
    }

    /// Promotes the rotation-next pending utterance to the voice.
    fn promote(&mut self, now: Duration) {
        self.active = None;
        loop {
            if self.pass.is_empty() {
                self.pass = self.snapshot_pass(&HashMap::new());
                if self.pass.is_empty() {
                    return;
                }
            }
            let Some(ns) = self.pass.pop_front() else {
                return;
            };
            let Some(queue) = self.queues.get_mut(&ns) else {
                continue;
            };
            let Some(utterance) = queue.pop_front() else {
                continue;
            };
            if queue.is_empty() {
                self.queues.remove(&ns);
            }
            self.pending_total -= 1;
            self.active = Some(ActiveUtterance {
                utterance,
                started: now,
            });
            return;
        }
    }

    /// Frame tick: expires the active utterance at its pinned duration,
    /// then promotes; also promotes when a cancel left the voice free.
    pub fn tick(&mut self, now: Duration) {
        let expired = self
            .active
            .as_ref()
            .is_some_and(|active| now >= active.started + active.utterance.duration);
        if expired || (self.active.is_none() && self.pending_total > 0) {
            self.promote(now);
        }
    }

    /// `avatar.stop`: cancels the caller's active utterance if the caller
    /// is the current speaker, else the caller's queued utterance.
    /// Promotion of the next speaker happens in the drive tick (≤ 1 frame
    /// later), never here — a same-frame speak cannot jump the queue.
    pub fn stop_speaking(&mut self, namespace: u8) -> Result<(), AvatarReject> {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.utterance.namespace == namespace)
        {
            self.active = None;
            return Ok(());
        }
        if let Some(queue) = self.queues.get_mut(&namespace)
            && queue.pop_front().is_some()
        {
            if queue.is_empty() {
                self.queues.remove(&namespace);
            }
            self.pending_total -= 1;
            return Ok(());
        }
        Err((
            codes::NOTHING_ACTIVE,
            "nothing of this agent's is speaking or queued".to_string(),
        ))
    }

    /// `avatar.cancel;id=`: cancels one utterance by execution handle;
    /// only the owner's handles are cancellable.
    pub fn cancel(&mut self, namespace: u8, execution_id: &str) -> Result<(), AvatarReject> {
        if let Some(active) = &self.active
            && active.utterance.id == execution_id
        {
            if active.utterance.namespace != namespace {
                return Err((
                    codes::NOT_OWNER,
                    "that execution belongs to another agent".to_string(),
                ));
            }
            self.active = None;
            return Ok(());
        }
        for (ns, queue) in &mut self.queues {
            if let Some(index) = queue.iter().position(|u| u.id == execution_id) {
                if *ns != namespace {
                    return Err((
                        codes::NOT_OWNER,
                        "that execution belongs to another agent".to_string(),
                    ));
                }
                queue.remove(index);
                self.pending_total -= 1;
                let empty = queue.is_empty();
                let ns = *ns;
                if empty {
                    self.queues.remove(&ns);
                }
                return Ok(());
            }
        }
        Err((
            codes::UNKNOWN_ID,
            "no live execution carries that handle (finished, cancelled, or never started)"
                .to_string(),
        ))
    }

    /// Drops one namespace's pending utterances (its active utterance, if
    /// any, keeps the voice — clearing speech is queue authority).
    pub fn clear_namespace(&mut self, namespace: u8) {
        if let Some(queue) = self.queues.remove(&namespace) {
            self.pending_total -= queue.len();
        }
    }

    /// Cancels the current utterance and clears the whole queue.
    pub fn clear_all(&mut self) {
        self.active = None;
        self.queues.clear();
        self.pending_total = 0;
        self.pass.clear();
    }

    /// Whether an utterance is live or pending (the drive run-condition).
    pub fn has_speech(&self) -> bool {
        self.active.is_some() || self.pending_total > 0
    }

    /// The active utterance, if any.
    pub fn active(&self) -> Option<&ActiveUtterance> {
        self.active.as_ref()
    }

    /// Pending utterances across all agents (the active one not counted).
    pub fn queue_depth(&self) -> usize {
        self.pending_total
    }

    /// The caller's own executions for `state.executions`: own active and
    /// own queued items, text included (owner-scoped — other agents never
    /// see it).
    pub fn execution_items(&self, namespace: u8, now: Duration) -> Vec<Value> {
        let mut items = Vec::new();
        if let Some(active) = &self.active
            && active.utterance.namespace == namespace
        {
            let remaining = (active.started + active.utterance.duration).saturating_sub(now);
            let mut item = json!({
                "kind": "utterance",
                "id": active.utterance.id,
                "status": "active",
                "text": active.utterance.text,
                "duration_ms": active.utterance.duration.as_millis() as u64,
                "remaining_ms": remaining.as_millis() as u64,
            });
            if let Some(from) = &active.utterance.from {
                item["from"] = json!(from);
            }
            items.push(item);
        }
        if let Some(queue) = self.queues.get(&namespace) {
            for utterance in queue {
                let (position, eta) = self
                    .forecast(namespace, utterance.seq, now)
                    .expect("queued utterances forecast");
                let mut item = json!({
                    "kind": "utterance",
                    "id": utterance.id,
                    "status": "queued",
                    "text": utterance.text,
                    "duration_ms": utterance.duration.as_millis() as u64,
                    "position": position,
                    "eta_ms": eta.as_millis() as u64,
                });
                if let Some(from) = &utterance.from {
                    item["from"] = json!(from);
                }
                items.push(item);
            }
        }
        items
    }
}

// ── Scene state ──

/// Scene-global avatar state: placement, visibility, the live gesture,
/// and the speech queue. Mutating commands above the ordinary tier are
/// capability-gated at apply time and classify privileged in macros (#16).
#[derive(Resource)]
pub struct AvatarState {
    /// Whether the avatar is shown (`avatar.set`/`show` vs `hide`).
    pub shown: bool,
    /// The curated model in effect.
    pub model: &'static AvatarModelSpec,
    /// The HUD anchor in effect (default: the locked `bottom-right`).
    pub anchor: AvatarAnchor,
    /// Clamped anchor offset, logical px, +y down.
    pub offset: Vec2,
    /// Clamped HUD scale.
    pub scale: f32,
    /// The one-shot gesture in progress, if any.
    pub gesture: Option<ActiveGesture>,
    /// Set when the model changed and the mascot entity tree must be
    /// despawned and rebuilt (the cursor `needs_respawn` precedent).
    pub needs_respawn: bool,
    /// The fair bounded speech queue.
    pub speech: SpeechQueue,
}

impl Default for AvatarState {
    fn default() -> Self {
        Self {
            shown: false,
            model: &AVATAR_REGISTRY[0],
            anchor: AvatarAnchor::BottomRight,
            offset: Vec2::ZERO,
            scale: 1.0,
            gesture: None,
            needs_respawn: false,
            speech: SpeechQueue::default(),
        }
    }
}

impl AvatarState {
    /// The `state.scene` projection (#23 §2's five public fields). No
    /// utterance text — text leaves the organ only through owner-scoped
    /// `state.executions` items.
    pub fn public_scene(&self) -> Value {
        let active = self.speech.active();
        json!({
            "visible": self.shown,
            "speaking": active.is_some(),
            "speaker": active.map(|a| a.utterance.namespace),
            "execution": active.map(|a| a.utterance.id.clone()),
            "queue_depth": self.speech.queue_depth(),
            "model": self.model.id,
            "position": self.anchor.as_str(),
        })
    }

    /// The `reset` tap: return to baseline — hidden, default placement,
    /// queue cancelled. The reset ack belongs to `apply_ai_commands`;
    /// this clears silently, like every organ's reset tap.
    pub fn reset(&mut self) {
        let respawn = self.shown;
        *self = Self::default();
        // A shown mascot must despawn; the flag survives the reset value.
        self.needs_respawn = respawn;
    }
}

/// Derives the pinned display duration at admission (#23 §2): from the
/// text when unsupplied, clamped to the locked 750 ms – 15 s either way.
/// A supplied non-finite or non-positive duration is a `bad-payload`.
fn pin_duration(text: &str, requested: Option<f32>) -> Result<Duration, AvatarReject> {
    let ms = match requested {
        None => text.chars().count() as u64 * UTTERANCE_MS_PER_CHAR,
        Some(secs) => {
            if !(secs.is_finite() && secs > 0.0) {
                return Err((
                    codes::BAD_PAYLOAD,
                    "duration must be a finite value greater than 0".to_string(),
                ));
            }
            (f64::from(secs) * 1000.0).round() as u64
        }
    };
    Ok(Duration::from_millis(ms.clamp(MIN_UTTERANCE_MS, MAX_UTTERANCE_MS)))
}

// ── Systems ──

/// Expires and promotes utterances on the frame clock. Ordered before
/// [`apply_avatar_commands`] so a same-frame expiry frees the voice
/// before new admissions.
pub fn drive_avatar_speech(time: Res<Time>, mut avatar: ResMut<AvatarState>) {
    avatar.speech.tick(time.elapsed());
}

/// Applies queued `avatar.*` commands: the decision layer. Owns the ack
/// for every avatar command (one-owner invariant); also taps `Reset`.
pub fn apply_avatar_commands(
    time: Res<Time>,
    mut commands: MessageReader<AiCommand>,
    mut avatar: ResMut<AvatarState>,
    app_config: Res<AppConfig>,
    mut session: ResMut<QuerySession>,
    mut acks: MessageWriter<AckOutcome>,
    mut diagnostics: ResMut<AiDiagnostics>,
) {
    let now = time.elapsed();
    for AiCommand {
        source,
        ack_token,
        command,
        ..
    } in commands.read()
    {
        macro_rules! reject {
            ($action:literal, $code:expr, $($message:tt)+) => {{
                let message = format!($($message)+);
                warn!("ratty-ai: {} rejected: {message}", $action);
                reject(
                    &mut diagnostics,
                    &mut acks,
                    *source,
                    ack_token,
                    $action,
                    $code,
                    message,
                );
            }};
        }
        // The scene-capability gate (#23 §2), mirroring the ambient gate:
        // trusted config only, the wire can never grant it to itself.
        macro_rules! require_scene {
            ($action:literal) => {
                if !SceneCapability::AvatarScene.granted_to(*source, &app_config) {
                    reject!(
                        $action,
                        codes::NOT_PERMITTED,
                        "avatar scene control requires the avatar-scene capability \
                         ([trust.local] avatar_scene); ordinary agents may speak \
                         and gesture only"
                    );
                    continue;
                }
            };
        }
        match command {
            RattyAiCommand::AvatarSet {
                model,
                position,
                dx,
                dy,
                scale,
            } => {
                require_scene!("avatar.set");
                // Atomic partial update (the cursor precedent): validate
                // everything, then commit everything.
                let spec = match model {
                    None => None,
                    Some(id) => match avatar_model_spec(id) {
                        Some(spec) => Some(spec),
                        None => {
                            reject!(
                                "avatar.set",
                                codes::UNKNOWN_MODEL,
                                "model '{id}' is not a registered avatar model \
                                 (registry: mascot)"
                            );
                            continue;
                        }
                    },
                };
                let anchor = match position {
                    None => None,
                    Some(name) => match AvatarAnchor::parse(name) {
                        Some(anchor) => Some(anchor),
                        None => {
                            reject!(
                                "avatar.set",
                                codes::UNKNOWN_ANCHOR,
                                "position '{name}' is not one of the nine named anchors"
                            );
                            continue;
                        }
                    },
                };
                if let Some(spec) = spec
                    && !std::ptr::eq(spec, avatar.model)
                {
                    avatar.model = spec;
                    avatar.needs_respawn = true;
                }
                if let Some(anchor) = anchor {
                    avatar.anchor = anchor;
                }
                if let Some(dx) = dx {
                    avatar.offset.x = dx.clamp(-AVATAR_OFFSET_MAX_PX, AVATAR_OFFSET_MAX_PX);
                }
                if let Some(dy) = dy {
                    avatar.offset.y = dy.clamp(-AVATAR_OFFSET_MAX_PX, AVATAR_OFFSET_MAX_PX);
                }
                if let Some(scale) = scale {
                    avatar.scale = scale.clamp(AVATAR_SCALE_MIN, AVATAR_SCALE_MAX);
                }
                avatar.shown = true;
                ack_commit(&mut acks, *source, ack_token);
            }
            RattyAiCommand::AvatarShow => {
                require_scene!("avatar.show");
                avatar.shown = true;
                ack_commit(&mut acks, *source, ack_token);
            }
            RattyAiCommand::AvatarHide => {
                require_scene!("avatar.hide");
                // The locked #23 §2 semantics: hide cancels the current
                // utterance and clears the queue.
                avatar.shown = false;
                avatar.gesture = None;
                avatar.speech.clear_all();
                ack_commit(&mut acks, *source, ack_token);
            }
            RattyAiCommand::AvatarGesture { gesture } => {
                if !avatar.shown {
                    reject!(
                        "avatar.gesture",
                        codes::NOTHING_ACTIVE,
                        "the avatar is hidden (avatar.set or avatar.show first)"
                    );
                    continue;
                }
                let Some(which) = AvatarChoreography::parse(gesture) else {
                    reject!(
                        "avatar.gesture",
                        codes::UNKNOWN_GESTURE,
                        "gesture '{gesture}' is not a named choreography \
                         (bob, tilt, lean, nod, spin)"
                    );
                    continue;
                };
                avatar.gesture = Some(ActiveGesture {
                    which,
                    started: now,
                });
                ack_commit(&mut acks, *source, ack_token);
            }
            RattyAiCommand::AvatarSpeak {
                text,
                from,
                duration,
            } => {
                if !avatar.shown {
                    reject!(
                        "avatar.speak",
                        codes::NOTHING_ACTIVE,
                        "the avatar is hidden (avatar.set or avatar.show first)"
                    );
                    continue;
                }
                if text.len() > MAX_AVATAR_TEXT_BYTES {
                    reject!(
                        "avatar.speak",
                        codes::TEXT_TOO_LONG,
                        "utterance text is {} bytes, over the {MAX_AVATAR_TEXT_BYTES}-byte cap",
                        text.len()
                    );
                    continue;
                }
                if let Some(from) = from
                    && from.len() > MAX_AVATAR_SPEAKER_BYTES
                {
                    reject!(
                        "avatar.speak",
                        codes::TOO_LARGE,
                        "from label is {} bytes, over the {MAX_AVATAR_SPEAKER_BYTES}-byte cap",
                        from.len()
                    );
                    continue;
                }
                let pinned = match pin_duration(text, *duration) {
                    Ok(duration) => duration,
                    Err((code, message)) => {
                        reject!("avatar.speak", code, "{message}");
                        continue;
                    }
                };
                let id = session.mint_execution_id();
                let utterance = Utterance {
                    id: id.clone(),
                    namespace: source.namespace(),
                    seq: 0, // assigned at admission
                    text: text.clone(),
                    from: from.clone(),
                    duration: pinned,
                };
                match avatar.speech.admit(utterance, now) {
                    Ok(SpeakAdmission::Started) => ack_commit_long_running(
                        &mut acks,
                        *source,
                        ack_token,
                        codes::STARTED,
                        json!({ "id": id, "position": 0, "eta_ms": 0 }),
                    ),
                    Ok(SpeakAdmission::Queued { position, eta }) => ack_commit_long_running(
                        &mut acks,
                        *source,
                        ack_token,
                        codes::QUEUED,
                        json!({
                            "id": id,
                            "position": position,
                            "eta_ms": eta.as_millis() as u64,
                        }),
                    ),
                    // A mint consumed by a rejected admission is simply
                    // discarded — handles need uniqueness, not density.
                    Err((code, message)) => reject!("avatar.speak", code, "{message}"),
                }
            }
            RattyAiCommand::AvatarStopSpeaking => {
                match avatar.speech.stop_speaking(source.namespace()) {
                    Ok(()) => ack_commit(&mut acks, *source, ack_token),
                    Err((code, message)) => reject!("avatar.stop", code, "{message}"),
                }
            }
            RattyAiCommand::AvatarCancel { execution_id } => {
                // Stale-session pre-screen: a handle minted by a previous
                // process answers unknown-id with an honest message.
                if !session.owns_execution_id(execution_id) {
                    reject!(
                        "avatar.cancel",
                        codes::UNKNOWN_ID,
                        "handle '{execution_id}' was not minted by this session"
                    );
                    continue;
                }
                match avatar.speech.cancel(source.namespace(), execution_id) {
                    Ok(()) => ack_commit(&mut acks, *source, ack_token),
                    Err((code, message)) => reject!("avatar.cancel", code, "{message}"),
                }
            }
            RattyAiCommand::AvatarSpeechClear { scope } => {
                let own = matches!(scope, SpeechClearScope::Own)
                    || matches!(scope, SpeechClearScope::Namespace(ns) if *ns == source.namespace());
                if !own {
                    require_scene!("avatar.speech.clear");
                }
                match scope {
                    SpeechClearScope::Own => avatar.speech.clear_namespace(source.namespace()),
                    SpeechClearScope::Namespace(ns) => avatar.speech.clear_namespace(*ns),
                    SpeechClearScope::All => avatar.speech.clear_all(),
                }
                // Idempotent commit (the ClearObjects precedent).
                ack_commit(&mut acks, *source, ack_token);
            }
            RattyAiCommand::Reset => {
                // Return-to-baseline, not scene control: a caller cannot
                // choose a representation or selectively clear one agent,
                // and hidden+empty IS the default state — so reset stays
                // ungated, the exact posture of the ambient bed's ungated
                // reset fade. Reset's single ack belongs to
                // apply_ai_commands; this tap is silent.
                avatar.reset();
            }
            _ => {}
        }
    }
}

/// Registers the avatar organ.
pub struct AvatarPlugin;

impl Plugin for AvatarPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AvatarState>()
            .add_systems(
                Update,
                drive_avatar_speech
                    .before(apply_avatar_commands)
                    .run_if(|avatar: Res<AvatarState>| avatar.speech.has_speech()),
            )
            .add_systems(
                Update,
                apply_avatar_commands.after(crate::systems::pump_pty_output),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::CommandOrigin;
    use crate::runtime::IngressSource;
    use bevy::ecs::message::Messages;

    const NS0: IngressSource = IngressSource::Local;

    fn t(secs: f32) -> Duration {
        Duration::from_secs_f32(secs)
    }

    fn utterance(ns: u8, id: &str, secs: f32) -> Utterance {
        Utterance {
            id: id.to_string(),
            namespace: ns,
            seq: 0,
            text: format!("utterance {id}"),
            from: None,
            duration: t(secs),
        }
    }

    #[test]
    fn registry_stays_in_lockstep_with_the_wire_vocabulary() {
        let registry_ids: Vec<&str> = AVATAR_REGISTRY.iter().map(|spec| spec.id).collect();
        assert_eq!(registry_ids, crate::osc::AVATAR_MODELS);
        for spec in AVATAR_REGISTRY {
            assert!(
                crate::model::embedded_object_loadable(spec.file),
                "{} backs a registry id but is not a loadable embedded asset",
                spec.file
            );
            assert!(spec.file.ends_with(".glb"));
            assert_eq!(spec.embedded_url, format!("embedded://objects/{}", spec.file));
        }
    }

    #[test]
    fn anchors_round_trip_and_default_is_bottom_right() {
        assert_eq!(AvatarAnchor::ALL.len(), 9);
        for (anchor, wire) in AvatarAnchor::ALL {
            assert_eq!(AvatarAnchor::parse(wire), Some(*anchor));
            assert_eq!(anchor.as_str(), *wire);
        }
        assert_eq!(AvatarAnchor::parse("middle"), None);
        let state = AvatarState::default();
        assert!(!state.shown);
        assert_eq!(state.anchor, AvatarAnchor::BottomRight);
        assert_eq!(state.scale, 1.0);
        assert_eq!(state.model.id, "mascot");
    }

    #[test]
    fn durations_pin_at_admission_with_the_locked_clamps() {
        // Text-derived: floor, proportional, ceiling.
        assert_eq!(pin_duration("hi", None).expect("pins"), t(0.75));
        assert_eq!(
            pin_duration(&"x".repeat(100), None).expect("pins"),
            Duration::from_millis(6000)
        );
        assert_eq!(pin_duration(&"x".repeat(900), None).expect("pins"), t(15.0));
        // Explicit: clamped both ways; garbage rejects bad-payload.
        assert_eq!(pin_duration("hi", Some(0.1)).expect("pins"), t(0.75));
        assert_eq!(pin_duration("hi", Some(60.0)).expect("pins"), t(15.0));
        assert_eq!(pin_duration("hi", Some(4.0)).expect("pins"), t(4.0));
        assert_eq!(pin_duration("hi", Some(0.0)).expect_err("rejects").0, codes::BAD_PAYLOAD);
        assert_eq!(
            pin_duration("hi", Some(f32::NAN)).expect_err("rejects").0,
            codes::BAD_PAYLOAD
        );
    }

    #[test]
    fn admission_respects_both_caps_and_reports_positions() {
        let mut q = SpeechQueue::default();
        // Idle queue: the first speak takes the voice.
        assert_eq!(
            q.admit(utterance(0, "a", 10.0), t(0.0)).expect("starts"),
            SpeakAdmission::Started
        );
        // Second from the same agent queues at position 1 (behind the
        // active utterance only) — the verifier-pinned semantics.
        match q.admit(utterance(0, "b", 5.0), t(1.0)).expect("queues") {
            SpeakAdmission::Queued { position, eta } => {
                assert_eq!(position, 1);
                assert_eq!(eta, t(9.0), "remaining active time");
            }
            other => panic!("expected Queued, got {other:?}"),
        }
        // Third from the same agent: per-agent slot taken.
        assert_eq!(
            q.admit(utterance(0, "c", 5.0), t(1.0)).expect_err("full").0,
            codes::AGENT_QUEUE_FULL
        );
        // Distinct agents fill the global cap of 4 pending...
        for (ns, id) in [(1, "d"), (2, "e"), (3, "f")] {
            q.admit(utterance(ns, id, 5.0), t(1.0)).expect("queues");
        }
        // ...and the fifth pending rejects busy.
        assert_eq!(
            q.admit(utterance(4, "g", 5.0), t(1.0)).expect_err("busy").0,
            codes::BUSY
        );
        assert_eq!(q.queue_depth(), 4);
    }

    #[test]
    fn rotation_is_seniority_snapshot_not_namespace_number() {
        let mut q = SpeechQueue::default();
        q.admit(utterance(5, "active", 1.0), t(0.0)).expect("starts");
        // ns 7 queues first, then ns 1 — a lower namespace number.
        q.admit(utterance(7, "senior", 1.0), t(0.1)).expect("queues");
        q.admit(utterance(1, "junior", 1.0), t(0.2)).expect("queues");
        // Seniority wins: ns 7 speaks before ns 1.
        q.tick(t(1.5));
        assert_eq!(q.active().expect("promoted").utterance.id, "senior");
        q.tick(t(3.0));
        assert_eq!(q.active().expect("promoted").utterance.id, "junior");
    }

    #[test]
    fn adversarial_admissions_cannot_stretch_the_start_bound() {
        // The module-doc bound: once admitted, only the namespaces pending
        // at admission time are ever served before you. An agent admitted
        // later — even with a smaller namespace number — lands behind.
        let mut q = SpeechQueue::default();
        q.admit(utterance(9, "active", 1.0), t(0.0)).expect("starts");
        q.admit(utterance(8, "mine", 1.0), t(0.1)).expect("queues");
        // Adversary: after my admission, a stream of new agents with
        // smaller namespace numbers keeps arriving.
        q.admit(utterance(0, "later-a", 15.0), t(0.2)).expect("queues");
        q.admit(utterance(1, "later-b", 15.0), t(0.3)).expect("queues");
        // First promotion after the active expires must be mine — nothing
        // pending at my admission time remains.
        q.tick(t(1.5));
        assert_eq!(q.active().expect("promoted").utterance.id, "mine");
    }

    #[test]
    fn stop_and_cancel_enforce_ownership_and_honesty() {
        let mut q = SpeechQueue::default();
        assert_eq!(
            q.stop_speaking(0).expect_err("idle").0,
            codes::NOTHING_ACTIVE
        );
        q.admit(utterance(0, "a", 10.0), t(0.0)).expect("starts");
        q.admit(utterance(1, "b", 5.0), t(0.1)).expect("queues");
        // Another agent cannot cancel my utterance by handle.
        assert_eq!(q.cancel(1, "a").expect_err("owned").0, codes::NOT_OWNER);
        assert_eq!(q.cancel(0, "zzz").expect_err("unknown").0, codes::UNKNOWN_ID);
        // The owner cancels its queued utterance.
        q.cancel(1, "b").expect("own queued cancels");
        assert_eq!(q.queue_depth(), 0);
        // stop cancels the active voice; promotion waits for the tick —
        // a same-frame speak cannot jump the queue.
        q.admit(utterance(2, "c", 5.0), t(0.2)).expect("queues");
        q.stop_speaking(0).expect("own active stops");
        assert!(q.active().is_none());
        match q.admit(utterance(3, "d", 5.0), t(0.3)).expect("queues") {
            SpeakAdmission::Queued { .. } => {}
            other => panic!("a same-frame admission must queue, got {other:?}"),
        }
        q.tick(t(0.4));
        assert_eq!(q.active().expect("promoted").utterance.id, "c");
    }

    #[test]
    fn execution_items_are_owner_scoped_and_forecast() {
        let mut q = SpeechQueue::default();
        q.admit(utterance(0, "a", 10.0), t(0.0)).expect("starts");
        q.admit(utterance(1, "b", 5.0), t(2.0)).expect("queues");
        let mine = q.execution_items(0, t(2.0));
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0]["status"], json!("active"));
        assert_eq!(mine[0]["remaining_ms"], json!(8000));
        let theirs = q.execution_items(1, t(2.0));
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0]["status"], json!("queued"));
        assert_eq!(theirs[0]["position"], json!(1));
        assert_eq!(theirs[0]["eta_ms"], json!(8000));
        // No cross-namespace leakage.
        assert!(q.execution_items(2, t(2.0)).is_empty());
    }

    // ── App-level: acks, gating, attribution ──

    fn app_test(avatar_scene: bool) -> App {
        let mut app = App::new();
        let mut config = AppConfig::default();
        config.trust.local.avatar_scene = avatar_scene;
        app.insert_resource(config);
        app.init_resource::<AvatarState>();
        app.init_resource::<AiDiagnostics>();
        app.init_resource::<QuerySession>();
        app.init_resource::<Time>();
        app.add_message::<AiCommand>();
        app.add_message::<AckOutcome>();
        app.add_systems(
            Update,
            (drive_avatar_speech, apply_avatar_commands).chain(),
        );
        app
    }

    fn send(app: &mut App, ack: Option<&str>, command: RattyAiCommand) {
        app.world_mut()
            .resource_mut::<Messages<AiCommand>>()
            .write(AiCommand {
                source: NS0,
                ack_token: ack.map(str::to_string),
                origin: CommandOrigin::Wire,
                command,
            });
        app.update();
    }

    fn drain_acks(app: &mut App) -> Vec<AckOutcome> {
        app.world_mut()
            .resource_mut::<Messages<AckOutcome>>()
            .drain()
            .collect()
    }

    fn bare_set() -> RattyAiCommand {
        RattyAiCommand::AvatarSet {
            model: None,
            position: None,
            dx: None,
            dy: None,
            scale: None,
        }
    }

    fn speak(text: &str) -> RattyAiCommand {
        RattyAiCommand::AvatarSpeak {
            text: text.to_string(),
            from: None,
            duration: None,
        }
    }

    #[test]
    fn revoked_capability_gates_exactly_the_privileged_forms() {
        let mut app = app_test(false);
        for (token, command) in [
            ("t1", bare_set()),
            ("t2", RattyAiCommand::AvatarShow),
            ("t3", RattyAiCommand::AvatarHide),
            (
                "t4",
                RattyAiCommand::AvatarSpeechClear {
                    scope: SpeechClearScope::Namespace(1),
                },
            ),
            (
                "t5",
                RattyAiCommand::AvatarSpeechClear {
                    scope: SpeechClearScope::All,
                },
            ),
        ] {
            send(&mut app, Some(token), command);
            let acks = drain_acks(&mut app);
            assert!(!acks[0].ok, "{token} must reject");
            assert_eq!(acks[0].code, Some(codes::NOT_PERMITTED), "{token}");
        }
        // Ownership-scoped forms stay ordinary: bare clear and own-ns
        // clear commit even without the grant.
        for (token, command) in [
            (
                "t6",
                RattyAiCommand::AvatarSpeechClear {
                    scope: SpeechClearScope::Own,
                },
            ),
            (
                "t7",
                RattyAiCommand::AvatarSpeechClear {
                    scope: SpeechClearScope::Namespace(0),
                },
            ),
        ] {
            send(&mut app, Some(token), command);
            let acks = drain_acks(&mut app);
            assert!(acks[0].ok, "{token} is ownership-scoped");
        }
    }

    #[test]
    fn speak_flow_started_queued_and_full_with_explicit_errors() {
        let mut app = app_test(true);
        send(&mut app, Some("set"), bare_set());
        assert!(drain_acks(&mut app)[0].ok);

        send(&mut app, Some("s1"), speak("Deploy finished"));
        let acks = drain_acks(&mut app);
        assert_eq!(acks[0].code, Some(codes::STARTED));
        let payload = acks[0].payload.as_ref().expect("started payload");
        assert_eq!(payload["position"], json!(0));
        assert!(payload["id"].is_string());

        send(&mut app, Some("s2"), speak("My turn next"));
        let acks = drain_acks(&mut app);
        assert_eq!(acks[0].code, Some(codes::QUEUED));
        let payload = acks[0].payload.as_ref().expect("queued payload");
        assert_eq!(payload["position"], json!(1));
        assert!(payload["eta_ms"].is_u64());

        // The verifier-pinned accounting: the *third* speak from the same
        // agent hits the per-agent slot.
        send(&mut app, Some("s3"), speak("One more"));
        let acks = drain_acks(&mut app);
        assert!(!acks[0].ok);
        assert_eq!(acks[0].code, Some(codes::AGENT_QUEUE_FULL));

        // Explicit validation errors.
        send(&mut app, Some("s4"), speak(&"x".repeat(1001)));
        assert_eq!(drain_acks(&mut app)[0].code, Some(codes::TEXT_TOO_LONG));
        send(
            &mut app,
            Some("s5"),
            RattyAiCommand::AvatarSpeak {
                text: "hi".to_string(),
                from: Some("y".repeat(65)),
                duration: None,
            },
        );
        assert_eq!(drain_acks(&mut app)[0].code, Some(codes::TOO_LARGE));
    }

    #[test]
    fn unknown_names_reject_with_their_explicit_codes() {
        let mut app = app_test(true);
        send(
            &mut app,
            Some("m"),
            RattyAiCommand::AvatarSet {
                model: Some("ai-helper.glb".to_string()),
                position: None,
                dx: None,
                dy: None,
                scale: None,
            },
        );
        assert_eq!(drain_acks(&mut app)[0].code, Some(codes::UNKNOWN_MODEL));
        send(
            &mut app,
            Some("a"),
            RattyAiCommand::AvatarSet {
                model: None,
                position: Some("middle".to_string()),
                dx: None,
                dy: None,
                scale: None,
            },
        );
        assert_eq!(drain_acks(&mut app)[0].code, Some(codes::UNKNOWN_ANCHOR));
        // An invalid field rejects the whole command atomically.
        assert!(!app.world().resource::<AvatarState>().shown);

        send(&mut app, Some("set"), bare_set());
        drain_acks(&mut app);
        send(
            &mut app,
            Some("g"),
            RattyAiCommand::AvatarGesture {
                gesture: "wave".to_string(),
            },
        );
        assert_eq!(drain_acks(&mut app)[0].code, Some(codes::UNKNOWN_GESTURE));
        send(
            &mut app,
            Some("g2"),
            RattyAiCommand::AvatarGesture {
                gesture: "nod".to_string(),
            },
        );
        assert!(drain_acks(&mut app)[0].ok);
    }

    #[test]
    fn hidden_avatar_rejects_speech_and_gestures_honestly() {
        let mut app = app_test(true);
        send(&mut app, Some("s"), speak("hello"));
        let acks = drain_acks(&mut app);
        assert_eq!(acks[0].code, Some(codes::NOTHING_ACTIVE));
        send(
            &mut app,
            Some("g"),
            RattyAiCommand::AvatarGesture {
                gesture: "bob".to_string(),
            },
        );
        assert_eq!(drain_acks(&mut app)[0].code, Some(codes::NOTHING_ACTIVE));
    }

    #[test]
    fn placement_clamps_commit_and_attribution_is_the_namespace() {
        let mut app = app_test(true);
        send(
            &mut app,
            Some("set"),
            RattyAiCommand::AvatarSet {
                model: Some("mascot".to_string()),
                position: Some("top-left".to_string()),
                dx: Some(9000.0),
                dy: Some(-9000.0),
                scale: Some(100.0),
            },
        );
        assert!(drain_acks(&mut app)[0].ok, "clamps commit, never reject");
        {
            let state = app.world().resource::<AvatarState>();
            assert!(state.shown);
            assert_eq!(state.anchor, AvatarAnchor::TopLeft);
            assert_eq!(state.offset, Vec2::new(AVATAR_OFFSET_MAX_PX, -AVATAR_OFFSET_MAX_PX));
            assert_eq!(state.scale, AVATAR_SCALE_MAX);
        }
        // The from= label is decoration; attribution authority is the
        // ingress namespace.
        send(
            &mut app,
            Some("s"),
            RattyAiCommand::AvatarSpeak {
                text: "hi".to_string(),
                from: Some("Fancy Agent".to_string()),
                duration: None,
            },
        );
        drain_acks(&mut app);
        let state = app.world().resource::<AvatarState>();
        let active = state.speech.active().expect("speaking");
        assert_eq!(active.utterance.namespace, 0);
        assert_eq!(active.utterance.from.as_deref(), Some("Fancy Agent"));
    }

    #[test]
    fn hide_cancels_speech_and_reset_returns_to_baseline_silently() {
        let mut app = app_test(true);
        send(&mut app, Some("set"), bare_set());
        send(&mut app, Some("s1"), speak("one"));
        send(&mut app, Some("s2"), speak("two"));
        drain_acks(&mut app);

        send(&mut app, Some("h"), RattyAiCommand::AvatarHide);
        assert!(drain_acks(&mut app)[0].ok);
        {
            let state = app.world().resource::<AvatarState>();
            assert!(!state.shown);
            assert!(!state.speech.has_speech(), "hide cancels current and queue");
        }

        // Reset clears silently: no avatar-owned ack (reset's single ack
        // belongs to apply_ai_commands, which is not in this harness).
        send(&mut app, Some("set2"), bare_set());
        drain_acks(&mut app);
        send(&mut app, None, RattyAiCommand::Reset);
        assert!(drain_acks(&mut app).is_empty(), "the reset tap is silent");
        let state = app.world().resource::<AvatarState>();
        assert!(!state.shown);
        assert_eq!(state.anchor, AvatarAnchor::BottomRight);
    }

    #[test]
    fn expiry_promotes_across_agents_on_the_frame_clock() {
        let mut q = SpeechQueue::default();
        q.admit(utterance(0, "a", 1.0), t(0.0)).expect("starts");
        q.admit(utterance(1, "b", 1.0), t(0.5)).expect("queues");
        q.tick(t(0.9));
        assert_eq!(q.active().expect("still speaking").utterance.id, "a");
        q.tick(t(1.0));
        assert_eq!(q.active().expect("promoted").utterance.id, "b");
        q.tick(t(2.0));
        assert!(q.active().is_none());
        assert!(!q.has_speech());
    }
}
