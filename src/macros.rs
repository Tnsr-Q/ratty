//! The macros organ: recorded AI-channel choreography (#16, M3.7).
//!
//! A **macro** is a relative-timestamped sequence of canonical
//! [`RattyAiCommand`]s, tapped off the lowering path between `macro.record`
//! and `macro.stop`. It captures *only* the AI channel — never terminal
//! text, raw OSC bytes, or PTY input — which is what distinguishes it from a
//! transmission (a transmission is a byte stream; a macro is a command
//! stream). Playback re-injects the captured commands through the *same*
//! validation and lowering path, under the caller's **current**
//! capabilities: nothing is baked in at record time, so a capability lost
//! since recording fails at play time, explicitly.
//!
//! ## Recording is a tap, not a mode
//!
//! [`apply_macro_commands`] reads the same [`AiCommand`] stream every other
//! organ reads. It owns the `macro.*` control acks and, in the same pass,
//! captures the caller's own recordable commands into an active recording.
//! Because the capture is a tap, the enclosed commands still execute
//! normally the frame they arrive — their own appliers read the same
//! messages independently. `macro.*` and `reset` are handled in explicit
//! arms and so are never captured; the control-plane class (`react`/rule.*)
//! is filtered in the tap (#21 amendment). Ack `tok=` correlation tokens are
//! transport metadata and are dropped before capture.
//!
//! ## Ownership, validation, and the "after validation" boundary
//!
//! The tap captures a command into the recording keyed by the command's own
//! ingress namespace — an agent only ever records its own choreography, into
//! its own registry. Per-command *target* validation (does this object id
//! exist, is this asset loadable, does this id fall in the caller's range)
//! is inherently distributed across the other organs' appliers and is
//! **re-applied at playback**, where a stale command fails explicitly into
//! the caller's `state.errors` ring like any fire-and-forget command. This
//! is the faithful reading of decision 1's "captured after … validation":
//! the recording holds parse- and ownership-checked canonical commands, and
//! playback is the second, authoritative validation gate.
//!
//! ## Playback
//!
//! [`drive_macro_playback`] is a [`Time`]-driven scheduler. It re-injects
//! due commands into the [`AiCommand`] stream **token-less** (mirroring
//! `drain_bookmark_jumps`), preserving recorded relative deltas by default;
//! `rate=` scales the clock and `mode=instant` drops the delays while
//! preserving command order. Every playback respects a per-frame execution
//! budget ([`MAX_PLAYBACK_COMMANDS_PER_FRAME`]).
//!
//! ## Slots, privilege, and the scene lock
//!
//! Each agent has at most one active recording *or* playback (the per-agent
//! single slot); a second operation on a busy slot rejects `busy`. Different
//! agents run concurrently — their commands stay inside their own object
//! namespaces (#12). A macro that captured any scene-global command (mode,
//! warp, reset) is classified **privileged** at record time and must acquire
//! the exclusive scene lock to play — the first concrete edge of the
//! cross-organ scene-arbitration question the M3 map carries as fog.
//!
//! ## Storage and the trust boundary
//!
//! Session macros are per-agent, in-memory, and die with the session —
//! browser-equal by construction. The wire can never touch a filesystem
//! path: `macro.export;to=` and `macro.run;path=` reject
//! `wire-filesystem-access` (extending #12's untrusted-byte-stream rule to
//! the macro surface). Durable macros enter the **trusted** registry only
//! through a trusted-tier act ([`MacroRegistry::insert_trusted`], called by
//! config / CLI / UI / controller code) and can never be mutated from the
//! wire; unqualified `macro.play` resolves session first, then trusted, with
//! `scope=` or an immutable content-hash reference to defeat shadowing.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::prelude::*;
use serde_json::{Value, json};

use crate::ai::{AiCommand, CommandOrigin};
use crate::identity::{TerminalId, TerminalIdentity};
use crate::osc::{MacroScope, RattyAiCommand};
use crate::query::codes;
use crate::query_channel::{AckOutcome, DiagnosticsSink, ack_commit, reject};
use crate::runtime::IngressSource;

/// Upper bound on stored macros per agent namespace: an honest failure
/// instead of an unbounded registry driven by untrusted output.
pub const MAX_MACROS_PER_NAMESPACE: usize = 32;

/// Upper bound, in bytes, on a macro name (matches the bookmark bound).
pub const MAX_MACRO_NAME_BYTES: usize = 64;

/// Upper bound on the commands one macro may capture. A recording that would
/// exceed it is poisoned and discarded at `macro.stop`; the prior macro (if
/// any) survives untouched.
pub const MAX_COMMANDS_PER_MACRO: usize = 256;

/// Upper bound, in seconds, on a recording's wall-clock span. A recording
/// whose next captured command lands past this is poisoned and discarded at
/// `macro.stop`.
pub const MAX_RECORDING_SECS: f32 = 300.0;

/// Upper bound on the playback rate multiplier. A finite positive `rate=`
/// above this is clamped so a pathological multiplier cannot turn the
/// per-frame budget into a busy-loop trigger.
pub const MAX_PLAYBACK_RATE: f32 = 1000.0;

/// Commands re-injected per frame across *all* active playbacks — the
/// per-frame execution budget decision 2 requires (especially for
/// `mode=instant`, which drops delays and would otherwise dump a whole macro
/// in one frame).
pub const MAX_PLAYBACK_COMMANDS_PER_FRAME: usize = 64;

/// The macro artifact format this build records and replays.
pub const MACRO_VERSION: u32 = 1;

/// One captured command with its offset from the recording's start. The ack
/// `tok=` token is transport metadata and is never stored here.
#[derive(Debug, Clone)]
struct MacroStep {
    /// Seconds since the recording began.
    offset: f32,
    /// The captured canonical command.
    command: RattyAiCommand,
}

/// A finalized, replayable macro. Stored behind an [`Arc`] so a playback can
/// pin the exact version it resolved at start — a mid-playback replace swaps
/// the registry's `Arc` and never mutates a running playback.
#[derive(Debug)]
pub struct Macro {
    /// Artifact format version ([`MACRO_VERSION`]).
    v: u32,
    /// The captured steps, in capture order.
    steps: Vec<MacroStep>,
    /// Whether the macro contains a scene-global command and so needs the
    /// exclusive scene lock to play (classified at record time).
    privileged: bool,
    /// Whether every captured step is a rule-safe action (#21) — the
    /// classification, computed at finalize beside `privileged`, that
    /// gates `macro.play` as a reactive rule action. A rule-safe macro is
    /// never privileged (scene-global commands are not rule-safe).
    rule_safe: bool,
    /// Content id (hex) over the canonical steps — stable for equal content,
    /// used as the immutable `hash=` play reference. Not a cryptographic
    /// hash; a within-session collision across ≤32 macros is astronomically
    /// unlikely.
    hash: String,
}

impl Macro {
    /// The artifact format version.
    pub fn version(&self) -> u32 {
        self.v
    }

    /// The number of captured commands.
    pub fn step_count(&self) -> usize {
        self.steps.len()
    }

    /// Whether the macro needs the exclusive scene lock to play.
    pub fn is_privileged(&self) -> bool {
        self.privileged
    }

    /// Whether every step is a rule-safe action, so a reactive rule may
    /// fire this macro (#21).
    pub fn is_rule_safe(&self) -> bool {
        self.rule_safe
    }

    /// The macro's immutable content id (the `hash=` play reference).
    pub fn hash(&self) -> &str {
        &self.hash
    }
}

/// A recording in progress: the transient half of an active slot.
#[derive(Debug)]
struct ActiveRecording {
    /// The name the finalized macro will be stored under.
    name: String,
    /// `Time::elapsed` when recording began, for relative offsets.
    started: Duration,
    /// Captured steps so far.
    steps: Vec<MacroStep>,
    /// Set once any scene-global command is captured.
    privileged: bool,
    /// Cleared once any non-rule-safe action is captured (starts true; an
    /// empty macro is trivially rule-safe).
    rule_safe: bool,
    /// Set to a rejection code once a limit is exceeded; a poisoned
    /// recording captures nothing more and is discarded at `macro.stop`.
    poisoned: Option<&'static str>,
}

/// A playback in progress: the transient half of an active slot.
#[derive(Debug)]
struct ActivePlayback {
    /// The ingress context to re-inject the commands under — the same source
    /// the `macro.play` arrived through, so replay runs under the caller's
    /// current authority.
    source: IngressSource,
    /// The causal origin stamped on every re-injected step: `Macro` for a
    /// caller-started playback, `Rule` when a reactive rule started it —
    /// the inherited execution context of #21.
    origin: CommandOrigin,
    /// The pinned macro version resolved at start.
    macro_: Arc<Macro>,
    /// Clock multiplier (validated finite and positive, clamped at
    /// [`MAX_PLAYBACK_RATE`]).
    rate: f32,
    /// Whether to drop recorded delays (order preserved, budget respected).
    instant: bool,
    /// `Time::elapsed` when playback began.
    started: Duration,
    /// Index of the next step to emit.
    next_index: usize,
    /// Whether this playback holds the exclusive scene lock (released when it
    /// finishes or is cancelled).
    scene_locked: bool,
    /// The session-unique execution handle minted at admission (#18),
    /// reported in the started ack and in `state.executions`.
    execution_id: String,
}

impl ActivePlayback {
    /// Collects the steps due at `scaled_elapsed` (the real elapsed times the
    /// rate), advancing `next_index`, up to `budget` commands. `mode=instant`
    /// ignores timing and drains in order. Returns the commands to re-inject.
    fn collect_due(&mut self, scaled_elapsed: f32, budget: usize) -> Vec<RattyAiCommand> {
        let mut due = Vec::new();
        while self.next_index < self.macro_.steps.len() && due.len() < budget {
            let step = &self.macro_.steps[self.next_index];
            if !self.instant && step.offset > scaled_elapsed {
                break;
            }
            due.push(step.command.clone());
            self.next_index += 1;
        }
        due
    }

    /// Whether every step has been emitted.
    fn finished(&self) -> bool {
        self.next_index >= self.macro_.steps.len()
    }
}

/// The active operation occupying an agent's single slot.
#[derive(Debug)]
enum SlotState {
    /// A recording is capturing commands.
    Recording(ActiveRecording),
    /// A playback is re-injecting commands.
    Playing(ActivePlayback),
}

/// The single started-ack estimate for a committed `macro.play` (#18):
/// wall milliseconds for timed playback, frames for `mode=instant` —
/// Bevy's `Time` promises no future frame duration at admission, so
/// frames are the honest unit there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackEta {
    /// Timed playback: the last step's offset divided by the clamped rate.
    Millis(u64),
    /// Instant playback: `ceil(steps / MAX_PLAYBACK_COMMANDS_PER_FRAME)`.
    Frames(u64),
}

/// A read-only projection of an agent's active slot for `state.executions`.
#[derive(Debug)]
pub struct ExecutionView {
    /// `"recording"` or `"playback"`.
    pub kind: &'static str,
    /// Playback only: the execution handle minted at admission (#18). A
    /// recording carries none — its `macro.record` ack was an immediate
    /// commit, not a long-running admission.
    pub id: Option<String>,
    /// The macro name (the target for a playback, the pending name for a
    /// recording).
    pub name: String,
    /// Whether the operation is privileged (recording: privileged so far;
    /// playback: the pinned macro is privileged).
    pub privileged: bool,
    /// Recording: commands captured so far. Playback: total commands.
    pub commands: usize,
    /// Playback only: commands emitted so far.
    pub played: Option<usize>,
    /// Playback only: whether it is an instant playback.
    pub instant: Option<bool>,
    /// Playback only: the clock multiplier.
    pub rate: Option<f32>,
    /// Whether the operation holds the exclusive scene lock.
    pub scene_locked: bool,
}

/// One terminal seat's session-half macro state (#56 decision 5): the
/// session registry plus the agent's single active slot. A component on
/// the seat entity, born `Default`-fresh at spawn and destroyed by
/// despawn — a recycled namespace slot's next tenant can never inherit
/// session macros or a running slot, by construction rather than by
/// cleanup discipline.
#[derive(Component, Default)]
pub struct TerminalMacros {
    /// Session macros by name (the seat entity replaced the old namespace
    /// key). Dies with the terminal; cleared by `reset`.
    session: HashMap<String, Arc<Macro>>,
    /// The agent's single active recording or playback.
    slot: Option<SlotState>,
}

/// The trusted/config half of the macro organ plus the exclusive scene
/// lock — the globals that survive any terminal (#56 decision 5); the
/// session half lives on each seat as [`TerminalMacros`].
#[derive(Resource, Default)]
pub struct MacroRegistry {
    /// Trusted promoted macros, keyed by name. Wire-immutable and durable —
    /// only [`insert_trusted`](Self::insert_trusted) writes here, and `reset`
    /// spares them.
    trusted: HashMap<String, Arc<Macro>>,
    /// The terminal currently holding the exclusive scene lock, if any —
    /// scene-wide arbitration (#56 decision 16): only one privileged
    /// playback across all agents may hold it at a time. Keyed by
    /// [`TerminalId`], never the namespace (the decision-17 stamp rule's
    /// live instance): ids never recycle, so a stale holder can never
    /// alias a recycled slot's next tenant even if a release is missed;
    /// the despawn sweep additionally releases a dead holder's lock so
    /// the scene cannot wedge.
    scene_lock: Option<TerminalId>,
}

/// A rejection: the wire code plus a human message for the `state.errors`
/// ring. Registry methods return this; the system turns it into a `reject`.
type MacroReject = (&'static str, String);

impl TerminalMacros {
    /// Number of session macros this terminal stores.
    pub fn session_len(&self) -> usize {
        self.session.len()
    }

    /// Whether this terminal's slot is an active playback (the
    /// `drive_macro_playback` run condition, any seat).
    pub fn has_active_playback(&self) -> bool {
        matches!(self.slot, Some(SlotState::Playing(_)))
    }

    /// Iterates this terminal's session macros in arbitrary order.
    pub fn iter_session(&self) -> impl Iterator<Item = (&str, &Macro)> {
        self.session
            .iter()
            .map(|(name, macro_)| (name.as_str(), macro_.as_ref()))
    }

    /// A projection of this terminal's active slot, if any.
    pub fn execution_view(&self) -> Option<ExecutionView> {
        match self.slot.as_ref()? {
            SlotState::Recording(rec) => Some(ExecutionView {
                kind: "recording",
                id: None,
                name: rec.name.clone(),
                privileged: rec.privileged,
                commands: rec.steps.len(),
                played: None,
                instant: None,
                rate: None,
                scene_locked: false,
            }),
            SlotState::Playing(pb) => Some(ExecutionView {
                kind: "playback",
                id: Some(pb.execution_id.clone()),
                name: String::new(),
                privileged: pb.macro_.privileged,
                commands: pb.macro_.steps.len(),
                played: Some(pb.next_index),
                instant: Some(pb.instant),
                rate: Some(pb.rate),
                scene_locked: pb.scene_locked,
            }),
        }
    }

    /// Starts a recording in this terminal's slot. Validates the name, the
    /// single-slot invariant, the collision rule, and the per-terminal cap.
    /// `source` is the arrival context (its namespace names the cap in the
    /// rejection message, unchanged wire-visible text).
    fn start_recording(
        &mut self,
        source: IngressSource,
        name: &str,
        replace: bool,
        now: Duration,
    ) -> Result<(), MacroReject> {
        let namespace = source.namespace();
        if name.is_empty() {
            return Err((codes::BAD_PAYLOAD, "name= must be non-empty".to_string()));
        }
        if name.len() > MAX_MACRO_NAME_BYTES {
            return Err((
                codes::TOO_LARGE,
                format!("name exceeds {MAX_MACRO_NAME_BYTES} bytes"),
            ));
        }
        if self.slot.is_some() {
            return Err((
                codes::BUSY,
                "a recording or playback is already active for this agent".to_string(),
            ));
        }
        let exists = self.session.contains_key(name);
        if exists && !replace {
            return Err((
                codes::ALREADY_EXISTS,
                format!("macro '{name}' exists (pass mode=replace to overwrite it)"),
            ));
        }
        if !exists && self.session_len() >= MAX_MACROS_PER_NAMESPACE {
            return Err((
                codes::NAMESPACE_CAP,
                format!("namespace {namespace} is at its {MAX_MACROS_PER_NAMESPACE}-macro limit"),
            ));
        }
        self.slot = Some(SlotState::Recording(ActiveRecording {
            name: name.to_string(),
            started: now,
            steps: Vec::new(),
            privileged: false,
            rule_safe: true,
            poisoned: None,
        }));
        Ok(())
    }

    /// Taps a recordable command into this terminal's active recording, if
    /// any. Enforces the commands-per-macro and wall-clock limits by
    /// poisoning the recording (discarded at `macro.stop`) rather than
    /// truncating silently.
    fn capture(&mut self, command: &RattyAiCommand, now: Duration) {
        let Some(SlotState::Recording(rec)) = self.slot.as_mut() else {
            return;
        };
        if rec.poisoned.is_some() {
            return;
        }
        if rec.steps.len() >= MAX_COMMANDS_PER_MACRO {
            rec.poisoned = Some(codes::TOO_LARGE);
            return;
        }
        let offset = now.saturating_sub(rec.started).as_secs_f32();
        if offset > MAX_RECORDING_SECS {
            rec.poisoned = Some(codes::TOO_LARGE);
            return;
        }
        if command.is_scene_global() {
            rec.privileged = true;
        }
        if !command.is_rule_safe_action() {
            rec.rule_safe = false;
        }
        rec.steps.push(MacroStep {
            offset,
            command: command.clone(),
        });
    }

    /// Finalizes an active recording (saving it, transactionally) or
    /// cancels an active playback. `nothing-active` when the slot is idle.
    /// Returns whether a scene-locked playback was cancelled — the caller
    /// ([`MacroRegistry::stop`]) releases the global lock; this component
    /// half never touches it.
    fn stop_slot(&mut self) -> Result<bool, MacroReject> {
        match self.slot.take() {
            Some(SlotState::Recording(rec)) => {
                if let Some(code) = rec.poisoned {
                    // The prior macro (if any) is untouched: it was never
                    // removed, so replacement stays transactional.
                    return Err((
                        code,
                        "recording exceeded a limit and was discarded".to_string(),
                    ));
                }
                let name = rec.name.clone();
                let hash = content_hash(&rec.steps, rec.privileged);
                let macro_ = Arc::new(Macro {
                    v: MACRO_VERSION,
                    steps: rec.steps,
                    privileged: rec.privileged,
                    rule_safe: rec.rule_safe,
                    hash,
                });
                self.session.insert(name, macro_);
                Ok(false)
            }
            Some(SlotState::Playing(pb)) => Ok(pb.scene_locked),
            None => Err((
                codes::NOTHING_ACTIVE,
                "no active recording or playback to stop".to_string(),
            )),
        }
    }
}

impl MacroRegistry {
    /// Promotes a macro into the durable, wire-immutable trusted registry.
    /// This is the trusted-tier entry point (config / CLI / UI / controller);
    /// the wire can never reach it. A macro carrying any macro-control
    /// command is rejected (no recursion, belt-and-suspenders beside the tap
    /// that already refuses to capture `macro.*`), and so is one carrying
    /// execution control — a session-scoped handle is meaningless in a
    /// durable artifact (#18).
    pub fn insert_trusted(
        &mut self,
        name: String,
        steps_source: &Macro,
    ) -> Result<(), &'static str> {
        if steps_source
            .steps
            .iter()
            .any(|step| step.command.is_macro_control())
        {
            return Err("a trusted macro may not contain macro-control commands");
        }
        if steps_source
            .steps
            .iter()
            .any(|step| step.command.is_execution_control())
        {
            return Err("a trusted macro may not contain execution-control commands");
        }
        self.trusted.insert(
            name,
            Arc::new(Macro {
                v: steps_source.v,
                steps: steps_source.steps.clone(),
                privileged: steps_source.privileged,
                // Derived state stays derived: recompute rather than trust
                // the caller's flag.
                rule_safe: steps_source
                    .steps
                    .iter()
                    .all(|step| step.command.is_rule_safe_action()),
                hash: steps_source.hash.clone(),
            }),
        );
        Ok(())
    }

    /// Iterates the trusted macros in arbitrary order.
    pub fn iter_trusted(&self) -> impl Iterator<Item = (&str, &Macro)> {
        self.trusted
            .iter()
            .map(|(name, macro_)| (name.as_str(), macro_.as_ref()))
    }

    /// Resolves a macro by name under the given scope. `None` resolves the
    /// caller's session registry first, then the trusted registry. Shared
    /// with the reactive organ, which pins a `macro.play` rule action at
    /// `rule.set` (#21).
    pub(crate) fn resolve(
        &self,
        seat: &TerminalMacros,
        name: &str,
        scope: Option<MacroScope>,
    ) -> Option<Arc<Macro>> {
        let session = || seat.session.get(name);
        let trusted = || self.trusted.get(name);
        match scope {
            Some(MacroScope::Session) => session(),
            Some(MacroScope::Trusted) => trusted(),
            None => session().or_else(trusted),
        }
        .cloned()
    }

    /// Resolves a macro by its immutable content id, searching the caller's
    /// session macros then the trusted registry (never another agent's
    /// session). Shared with the reactive organ (#21).
    pub(crate) fn resolve_by_hash(&self, seat: &TerminalMacros, hash: &str) -> Option<Arc<Macro>> {
        seat.session
            .values()
            .chain(self.trusted.values())
            .find(|macro_| macro_.hash == hash)
            .cloned()
    }

    /// Releases the scene lock on behalf of `holder`. The invariant —
    /// `scene_lock == Some(t)` iff terminal `t`'s slot is a scene-locked
    /// playback — makes any other release a bug, surfaced in debug.
    fn release_scene_lock(&mut self, holder: TerminalId) {
        debug_assert_eq!(
            self.scene_lock,
            Some(holder),
            "scene_lock invariant: only the holder's playback can release it"
        );
        self.scene_lock = None;
    }

    /// Finalizes `seat`'s active recording or cancels its active playback,
    /// releasing the scene lock when the cancelled playback held it.
    fn stop(
        &mut self,
        seat: &mut TerminalMacros,
        source: IngressSource,
    ) -> Result<(), MacroReject> {
        let released = seat.stop_slot()?;
        if released {
            self.release_scene_lock(source.terminal());
        }
        Ok(())
    }

    /// Starts a playback in `seat`'s slot. Enforces the single slot, validates
    /// the rate, resolves and pins the macro version, and acquires the
    /// exclusive scene lock for a privileged macro. On success returns the
    /// started-ack estimate; `execution_id` is the caller-minted handle
    /// (a mint consumed by a rejected admission is simply discarded —
    /// handles need uniqueness, not density).
    #[allow(clippy::too_many_arguments)]
    fn start_playback(
        &mut self,
        seat: &mut TerminalMacros,
        source: IngressSource,
        origin: CommandOrigin,
        name: &str,
        hash: Option<&str>,
        rate: f32,
        instant: bool,
        scope: Option<MacroScope>,
        execution_id: String,
        now: Duration,
    ) -> Result<PlaybackEta, MacroReject> {
        if seat.slot.is_some() {
            return Err((
                codes::BUSY,
                "a recording or playback is already active for this agent".to_string(),
            ));
        }
        if !(rate.is_finite() && rate > 0.0) {
            return Err((
                codes::BAD_PAYLOAD,
                "rate must be a finite value greater than 0".to_string(),
            ));
        }
        let rate = rate.min(MAX_PLAYBACK_RATE);
        let macro_ = match hash {
            Some(hash) => self.resolve_by_hash(seat, hash),
            None => self.resolve(seat, name, scope),
        };
        let Some(macro_) = macro_ else {
            return Err((
                codes::UNKNOWN_ID,
                "no macro resolves under the given name/hash and scope".to_string(),
            ));
        };
        // A rule-fired macro.play re-checks rule-safety at fire time: the
        // registry may have changed since the rule pinned its target (#21).
        if origin == CommandOrigin::Rule && !macro_.rule_safe {
            return Err((
                codes::NOT_PERMITTED,
                "a rule may only play a rule-safe macro (every step in the \
                 allowlisted choreography class)"
                    .to_string(),
            ));
        }
        let scene_locked = if macro_.privileged {
            if self.scene_lock.is_some() {
                return Err((
                    codes::SCENE_LOCKED,
                    "a privileged macro cannot play while the exclusive scene lock is held"
                        .to_string(),
                ));
            }
            // The lock keys on the arrival TerminalId (the stamp rule) —
            // the stamp was minted by the allocator and is the same id the
            // applier resolved this seat by, so it cannot lie.
            self.scene_lock = Some(source.terminal());
            true
        } else {
            false
        };
        let eta = if instant {
            let steps = macro_.steps.len();
            PlaybackEta::Frames(steps.div_ceil(MAX_PLAYBACK_COMMANDS_PER_FRAME) as u64)
        } else {
            let last_offset = macro_.steps.last().map_or(0.0, |step| step.offset);
            PlaybackEta::Millis((f64::from(last_offset / rate) * 1000.0).round() as u64)
        };
        seat.slot = Some(SlotState::Playing(ActivePlayback {
            source,
            // A rule-started playback keeps firing under the rule's
            // causal context; everything else replays as `Macro`.
            origin: if origin == CommandOrigin::Rule {
                CommandOrigin::Rule
            } else {
                CommandOrigin::Macro
            },
            macro_,
            rate,
            instant,
            started: now,
            next_index: 0,
            scene_locked,
            execution_id,
        }));
        Ok(eta)
    }

    /// Session reset, scoped to the ARRIVAL terminal (#56 decision 15's
    /// arrival-runtime attach): cancel its slot, drop its session macros,
    /// and release the scene lock iff that terminal holds it. Byte-identical
    /// to the old global clear at N=1 (one terminal is every terminal); at
    /// N>1 another agent's reset can no longer cancel this seat's held lock
    /// or drop its macros — decision 12's cross-runtime-interference ban.
    /// Trusted macros are durable and survive. Called from the `reset`
    /// command's tap; that command owns its ack elsewhere.
    fn reset(&mut self, seat: &mut TerminalMacros, terminal: TerminalId) {
        seat.session.clear();
        seat.slot = None;
        if self.scene_lock == Some(terminal) {
            self.scene_lock = None;
        }
    }

    /// Despawn-sweep release (#56 decision 17's named leak): a terminal
    /// dying mid-privileged-playback must not wedge the scene for the
    /// recycled slot's next tenant. The sweep is liveness; the
    /// `TerminalId` re-key is safety even under a sweep bug — a leaked id
    /// can never equal any future terminal's, whereas a leaked namespace
    /// aliased the slot's next tenant.
    pub(crate) fn sweep_terminal(&mut self, terminal: TerminalId) {
        if self.scene_lock == Some(terminal) {
            self.scene_lock = None;
        }
    }
}

#[cfg(test)]
impl MacroRegistry {
    /// Test-only: pins the scene lock to a holder, for the despawn-sweep
    /// test outside this module (the field stays private — production
    /// acquisition is `start_playback` alone).
    pub(crate) fn test_hold_scene_lock(&mut self, terminal: TerminalId) {
        self.scene_lock = Some(terminal);
    }

    /// Test-only: the current lock holder.
    pub(crate) fn test_scene_lock(&self) -> Option<TerminalId> {
        self.scene_lock
    }
}

#[cfg(test)]
impl TerminalMacros {
    /// Test-only: records and finalizes a macro from the given commands at
    /// t=0, for cross-organ tests (the reactive rule-action pinning).
    pub(crate) fn test_record(
        &mut self,
        source: IngressSource,
        name: &str,
        commands: &[RattyAiCommand],
    ) {
        self.start_recording(source, name, false, Duration::ZERO)
            .expect("test recording starts");
        for command in commands {
            self.capture(command, Duration::ZERO);
        }
        let released = self.stop_slot().expect("test recording finalizes");
        assert!(!released, "a recording never holds the scene lock");
    }
}

/// A content id (hex) over a macro's canonical steps. Deterministic for equal
/// content — `DefaultHasher` is fixed-keyed — so an immutable `hash=`
/// reference addresses the same content every time.
fn content_hash(steps: &[MacroStep], privileged: bool) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    MACRO_VERSION.hash(&mut hasher);
    privileged.hash(&mut hasher);
    for step in steps {
        // `to_bits` is exact; the Debug string canonicalizes the command.
        step.offset.to_bits().hash(&mut hasher);
        format!("{:?}", step.command).hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// Registers the macro registry and its systems.
///
/// Ordering: `apply_macro_commands` runs after `pump_pty_output` (it taps the
/// frame's commands and owns the `macro.*` acks). `drive_macro_playback` runs
/// after it and **before every command applier**, so a due step re-injected
/// this frame is validated and lowered the same frame by the ordinary
/// handlers. `answer_queries` is ordered after `apply_macro_commands` in
/// [`crate::ai::RattyAiPlugin`], so a same-chunk `state.macros` observes the
/// slot.
pub struct MacrosPlugin;

impl Plugin for MacrosPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MacroRegistry>()
            .add_systems(
                Update,
                apply_macro_commands.after(crate::systems::pump_pty_output),
            )
            .add_systems(
                Update,
                drive_macro_playback
                    .after(apply_macro_commands)
                    .before(crate::ai::apply_ai_commands)
                    .before(crate::ai::apply_ai_object_commands)
                    .before(crate::viz::apply_viz_commands)
                    .before(crate::sound::apply_sound_commands)
                    .before(crate::effects::apply_ai_effect_commands)
                    .before(crate::bookmarks::apply_bookmark_commands)
                    .before(crate::avatar::apply_avatar_commands)
                    .run_if(|seats: Query<&TerminalMacros>| {
                        seats.iter().any(TerminalMacros::has_active_playback)
                    }),
            );
    }
}

/// Handles the `macro.*` control commands (owning their acks) and taps the
/// caller's recordable choreography into an active recording — one pass over
/// the shared [`AiCommand`] stream.
pub fn apply_macro_commands(
    time: Res<Time>,
    mut commands: MessageReader<AiCommand>,
    mut registry: ResMut<MacroRegistry>,
    mut seats: Query<(&TerminalIdentity, &mut TerminalMacros)>,
    mut session: ResMut<crate::query_channel::QuerySession>,
    mut acks: MessageWriter<AckOutcome>,
    mut diagnostics: DiagnosticsSink,
) {
    let now = time.elapsed();
    for AiCommand {
        source,
        ack_token,
        origin,
        command,
    } in commands.read()
    {
        // Arrival resolution keys on TerminalId (the stamp rule): a
        // command whose arrival terminal died earlier this frame is
        // dropped loudly, never rerouted — a same-frame recycled
        // namespace cannot capture it.
        let Some((_, mut seat)) = seats
            .iter_mut()
            .find(|(identity, _)| identity.id() == source.terminal())
        else {
            warn!(
                "apply_macro_commands: command dropped: arrival terminal {:?} no longer exists",
                source.terminal()
            );
            continue;
        };
        macro_rules! reject {
            ($action:literal, $code:expr, $message:expr) => {
                reject(
                    &mut diagnostics,
                    &mut acks,
                    *source,
                    ack_token,
                    $action,
                    $code,
                    $message,
                )
            };
        }
        match command {
            RattyAiCommand::MacroRecord { name, replace } => {
                match seat.start_recording(*source, name, *replace, now) {
                    Ok(()) => ack_commit(&mut acks, *source, ack_token),
                    Err((code, message)) => {
                        warn!("ratty-ai: macro.record rejected: {message}");
                        reject!("macro.record", code, message);
                    }
                }
            }
            RattyAiCommand::MacroStop => match registry.stop(&mut seat, *source) {
                Ok(()) => ack_commit(&mut acks, *source, ack_token),
                Err((code, message)) => {
                    warn!("ratty-ai: macro.stop rejected: {message}");
                    reject!("macro.stop", code, message);
                }
            },
            RattyAiCommand::MacroPlay {
                name,
                hash,
                rate,
                instant,
                scope,
            } => {
                let execution_id = session.mint_execution_id();
                match registry.start_playback(
                    &mut seat,
                    *source,
                    *origin,
                    name,
                    hash.as_deref(),
                    *rate,
                    *instant,
                    *scope,
                    execution_id.clone(),
                    now,
                ) {
                    // macro.play is a long-running operation (#18): its one
                    // ack is `ok=1;code=started` with the execution handle
                    // and the admission-pinned estimate. It never queues —
                    // slot collisions reject `busy` above.
                    Ok(eta) => {
                        let mut payload = json!({ "id": execution_id, "position": 0 });
                        match eta {
                            PlaybackEta::Millis(ms) => payload["eta_ms"] = json!(ms),
                            PlaybackEta::Frames(frames) => {
                                payload["eta_frames"] = json!(frames);
                            }
                        }
                        crate::query_channel::ack_commit_long_running(
                            &mut acks,
                            *source,
                            ack_token,
                            codes::STARTED,
                            payload,
                        );
                    }
                    Err((code, message)) => {
                        warn!("ratty-ai: macro.play rejected: {message}");
                        reject!("macro.play", code, message);
                    }
                }
            }
            RattyAiCommand::MacroExport { .. } => {
                warn!("ratty-ai: macro.export rejected: the wire never writes a filesystem path");
                reject!(
                    "macro.export",
                    codes::WIRE_FILESYSTEM,
                    "macro.export never writes a filesystem path; promotion is a trusted-tier act"
                        .to_string()
                );
            }
            RattyAiCommand::MacroRun { .. } => {
                warn!("ratty-ai: macro.run rejected: the wire never reads a filesystem path");
                reject!(
                    "macro.run",
                    codes::WIRE_FILESYSTEM,
                    "macro.run never reads a filesystem path; the terminal byte stream is untrusted"
                        .to_string()
                );
            }
            RattyAiCommand::Reset => {
                // Session reset, arrival-terminal scoped. Reset's single ack
                // belongs to apply_ai_commands; the macro state clears
                // silently.
                registry.reset(&mut seat, source.terminal());
            }
            other => {
                // Recorder tap. macro.* and reset are handled above and never
                // reach here; the control-plane class (rule.*/sensor.*) is
                // excluded (#21), execution control (avatar.stop/cancel) is
                // excluded because session-scoped handles are transport-epoch
                // metadata (#18), and so are rule-*fired* commands — reactive
                // noise is not authored choreography. Everything else is
                // recordable, captured into the caller's own active
                // recording (if any).
                if other.is_control_plane()
                    || other.is_execution_control()
                    || *origin == CommandOrigin::Rule
                {
                    continue;
                }
                seat.capture(other, now);
            }
        }
    }
}

/// Re-injects due playback commands into the [`AiCommand`] stream,
/// token-less. Preserves recorded deltas (scaled by `rate`) unless the
/// playback is instant; bounded by the shared per-frame budget. A playback
/// that has emitted its last step is cleared and its scene lock released.
pub fn drive_macro_playback(
    time: Res<Time>,
    mut registry: ResMut<MacroRegistry>,
    mut seats: Query<(&TerminalIdentity, &mut TerminalMacros)>,
    mut commands: MessageWriter<AiCommand>,
) {
    let now = time.elapsed();
    // ONE budget across every seat (the shared per-frame ceiling); query
    // iteration order is arbitrary, exactly as the old HashMap order was —
    // N tests assert aggregates, never cross-terminal ordering.
    let mut spent = 0_usize;
    for (identity, mut seat) in seats.iter_mut() {
        let Some(SlotState::Playing(playback)) = seat.slot.as_mut() else {
            continue;
        };
        if spent >= MAX_PLAYBACK_COMMANDS_PER_FRAME {
            break;
        }
        let budget = MAX_PLAYBACK_COMMANDS_PER_FRAME - spent;
        let scaled = now.saturating_sub(playback.started).as_secs_f32() * playback.rate;
        let due = playback.collect_due(scaled, budget);
        spent += due.len();
        for command in due {
            commands.write(AiCommand {
                source: playback.source,
                ack_token: None,
                origin: playback.origin,
                command,
            });
        }
        if playback.finished() {
            let scene_locked = playback.scene_locked;
            seat.slot = None;
            if scene_locked {
                registry.release_scene_lock(identity.id());
            }
        }
    }
}

/// `state.macros`: the caller's session macros plus the trusted macros, each
/// tagged with its scope. Deterministically ordered and paginated so a large
/// registry never overflows a reply page.
pub fn macros_state_items(seat: &TerminalMacros, registry: &MacroRegistry) -> Vec<(u64, Value)> {
    let mut rows: Vec<(String, Value)> = Vec::new();
    for (name, macro_) in seat.iter_session() {
        rows.push((
            format!("session\u{0}{name}"),
            json!({
                "name": name,
                "scope": "session",
                "v": macro_.version(),
                "commands": macro_.step_count(),
                "privileged": macro_.is_privileged(),
                "rule_safe": macro_.is_rule_safe(),
                "hash": macro_.hash(),
            }),
        ));
    }
    for (name, macro_) in registry.iter_trusted() {
        rows.push((
            format!("trusted\u{0}{name}"),
            json!({
                "name": name,
                "scope": "trusted",
                "v": macro_.version(),
                "commands": macro_.step_count(),
                "privileged": macro_.is_privileged(),
                "rule_safe": macro_.is_rule_safe(),
                "hash": macro_.hash(),
            }),
        ));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    // A stable-order index keys pagination; a cursor is a best-effort
    // snapshot boundary over a registry that mutates rarely mid-query.
    rows.into_iter()
        .enumerate()
        .map(|(index, (_, value))| (index as u64, value))
        .collect()
}

/// `state.executions`: the caller's own active slot (0 or 1) — executions are
/// private per-agent, never projected to other callers.
pub fn executions_state_value(seat: &TerminalMacros) -> Value {
    let items: Vec<Value> = seat
        .execution_view()
        .map(|view| {
            let mut value = json!({
                "kind": view.kind,
                "name": view.name,
                "privileged": view.privileged,
                "commands": view.commands,
                "scene_locked": view.scene_locked,
            });
            if let Some(id) = view.id {
                value["id"] = json!(id);
            }
            if let Some(played) = view.played {
                value["played"] = json!(played);
            }
            if let Some(instant) = view.instant {
                value["instant"] = json!(instant);
            }
            if let Some(rate) = view.rate {
                value["rate"] = json!(rate);
            }
            value
        })
        .into_iter()
        .collect();
    json!({ "items": items })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::message::Messages;

    const NS0: IngressSource = IngressSource::test_boot();

    fn t(secs: f32) -> Duration {
        Duration::from_secs_f32(secs)
    }

    fn mode(target: &str) -> RattyAiCommand {
        RattyAiCommand::SetMode {
            mode: target.to_string(),
        }
    }

    fn spawn(id: u32) -> RattyAiCommand {
        RattyAiCommand::SpawnObject {
            id,
            path: "rat.obj".to_string(),
            x: 0,
            y: 0,
            scale: 1.0,
            spin: 0.0,
            brightness: 1.0,
            replace: false,
        }
    }

    #[test]
    fn limits_are_pinned() {
        assert_eq!(MAX_MACROS_PER_NAMESPACE, 32);
        assert_eq!(MAX_MACRO_NAME_BYTES, 64);
        assert_eq!(MAX_COMMANDS_PER_MACRO, 256);
        assert_eq!(MAX_PLAYBACK_COMMANDS_PER_FRAME, 64);
        assert_eq!(MACRO_VERSION, 1);
    }

    #[test]
    fn record_capture_stop_preserves_relative_deltas() {
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        seat.start_recording(NS0, "deploy", false, t(0.0))
            .expect("record starts");
        // Two ordinary commands at t=0 and t=1.5.
        seat.capture(&spawn(0x8000_0001), t(0.0));
        seat.capture(&spawn(0x8000_0002), t(1.5));
        registry.stop(&mut seat, NS0).expect("finalize");

        let macro_ = registry.resolve(&seat, "deploy", None).expect("stored");
        assert_eq!(macro_.step_count(), 2);
        assert!(!macro_.is_privileged(), "no scene-global command captured");
        assert_eq!(macro_.steps[0].offset, 0.0);
        assert_eq!(macro_.steps[1].offset, 1.5);
    }

    #[test]
    fn control_plane_is_never_captured() {
        // The tap is class-filtered by apply_macro_commands, but capture must
        // also never store a control-plane command if reached directly.
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        seat.start_recording(NS0, "m", false, t(0.0))
            .expect("registry op ok");
        // A scene-global command records and marks the macro privileged.
        seat.capture(&mode("3d"), t(0.0));
        registry.stop(&mut seat, NS0).expect("registry op ok");
        let macro_ = registry.resolve(&seat, "m", None).expect("registry op ok");
        assert_eq!(macro_.step_count(), 1);
        assert!(macro_.is_privileged(), "mode is scene-global → privileged");
    }

    #[test]
    fn avatar_commands_classify_and_filter_like_their_classes() {
        // avatar.set is scene-global → a macro containing it is privileged;
        // avatar.speak is ordinary choreography → recordable but not
        // rule-safe; execution control never enters a trusted macro (#18).
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        seat.test_record(
            NS0,
            "scene",
            &[RattyAiCommand::AvatarSet {
                model: Some("mascot".to_string()),
                position: None,
                dx: None,
                dy: None,
                scale: None,
            }],
        );
        let scene = registry.resolve(&seat, "scene", None).expect("stored");
        assert!(scene.is_privileged(), "avatar.set → privileged");

        seat.test_record(
            NS0,
            "speech",
            &[RattyAiCommand::AvatarSpeak {
                text: "hi".to_string(),
                from: None,
                duration: None,
            }],
        );
        let speech = registry.resolve(&seat, "speech", None).expect("stored");
        assert!(!speech.is_privileged(), "speak is ownership-scoped");
        assert!(!speech.is_rule_safe(), "speak consumes the shared voice");

        seat.test_record(NS0, "cancelier", &[RattyAiCommand::AvatarStopSpeaking]);
        let cancelier = registry.resolve(&seat, "cancelier", None).expect("stored");
        assert_eq!(
            registry
                .insert_trusted("t".to_string(), &cancelier)
                .expect_err("execution control refused"),
            "a trusted macro may not contain execution-control commands"
        );
    }

    #[test]
    fn single_slot_rejects_busy() {
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        seat.start_recording(NS0, "a", false, t(0.0))
            .expect("registry op ok");
        let (code, _) = seat
            .start_recording(NS0, "b", false, t(0.0))
            .expect_err("second op is busy");
        assert_eq!(code, codes::BUSY);
        let (code, _) = registry
            .start_playback(
                &mut seat,
                NS0,
                CommandOrigin::Wire,
                "a",
                None,
                1.0,
                false,
                None,
                "exec-test".to_string(),
                t(0.0),
            )
            .expect_err("play while recording is busy");
        assert_eq!(code, codes::BUSY);
    }

    #[test]
    fn collision_rule_and_transactional_replace() {
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        seat.start_recording(NS0, "x", false, t(0.0))
            .expect("registry op ok");
        seat.capture(&spawn(0x8000_0001), t(0.0));
        registry.stop(&mut seat, NS0).expect("registry op ok");

        // Same name without replace: rejected.
        let (code, _) = seat
            .start_recording(NS0, "x", false, t(0.0))
            .expect_err("already exists");
        assert_eq!(code, codes::ALREADY_EXISTS);

        // Replace: the old macro survives until the new one finalizes.
        seat.start_recording(NS0, "x", true, t(0.0))
            .expect("registry op ok");
        assert_eq!(
            registry
                .resolve(&seat, "x", None)
                .expect("registry op ok")
                .step_count(),
            1,
            "old version is intact mid-recording"
        );
        seat.capture(&spawn(0x8000_0002), t(0.0));
        seat.capture(&spawn(0x8000_0003), t(0.0));
        registry.stop(&mut seat, NS0).expect("registry op ok");
        assert_eq!(
            registry
                .resolve(&seat, "x", None)
                .expect("registry op ok")
                .step_count(),
            2,
            "finalize swaps to the new version"
        );
    }

    #[test]
    fn cancelled_replace_preserves_the_old_version() {
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        seat.start_recording(NS0, "x", false, t(0.0))
            .expect("registry op ok");
        seat.capture(&spawn(0x8000_0001), t(0.0));
        registry.stop(&mut seat, NS0).expect("registry op ok");
        let old_hash = registry
            .resolve(&seat, "x", None)
            .expect("registry op ok")
            .hash()
            .to_string();

        // A replace recording that never finalizes (poisoned) leaves the old
        // version untouched.
        seat.start_recording(NS0, "x", true, t(0.0))
            .expect("registry op ok");
        for index in 0..=MAX_COMMANDS_PER_MACRO as u32 {
            seat.capture(&spawn(0x8000_0100 + index), t(0.0));
        }
        let (code, _) = registry
            .stop(&mut seat, NS0)
            .expect_err("poisoned recording");
        assert_eq!(code, codes::TOO_LARGE);
        assert_eq!(
            registry
                .resolve(&seat, "x", None)
                .expect("registry op ok")
                .hash()
                .to_string(),
            old_hash,
            "the discarded replace never touched the stored macro"
        );
    }

    #[test]
    fn namespace_cap_is_enforced_at_record_start() {
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        for index in 0..MAX_MACROS_PER_NAMESPACE {
            seat.start_recording(NS0, &format!("m{index}"), false, t(0.0))
                .expect("registry op ok");
            registry.stop(&mut seat, NS0).expect("registry op ok");
        }
        let (code, _) = seat
            .start_recording(NS0, "overflow", false, t(0.0))
            .expect_err("at the cap");
        assert_eq!(code, codes::NAMESPACE_CAP);
        // Replacing an existing name at the cap is not a new slot.
        seat.start_recording(NS0, "m0", true, t(0.0))
            .expect("registry op ok");
    }

    #[test]
    fn name_validation() {
        let mut seat = TerminalMacros::default();
        let (code, _) = seat
            .start_recording(NS0, "", false, t(0.0))
            .expect_err("empty");
        assert_eq!(code, codes::BAD_PAYLOAD);
        let (code, _) = seat
            .start_recording(NS0, &"x".repeat(MAX_MACRO_NAME_BYTES + 1), false, t(0.0))
            .expect_err("too long");
        assert_eq!(code, codes::TOO_LARGE);
    }

    #[test]
    fn stop_when_idle_is_nothing_active() {
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        let (code, _) = registry.stop(&mut seat, NS0).expect_err("nothing to stop");
        assert_eq!(code, codes::NOTHING_ACTIVE);
    }

    #[test]
    fn playback_collects_due_steps_and_finishes() {
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        seat.start_recording(NS0, "seq", false, t(0.0))
            .expect("registry op ok");
        seat.capture(&spawn(0x8000_0001), t(0.0));
        seat.capture(&spawn(0x8000_0002), t(1.0));
        seat.capture(&spawn(0x8000_0003), t(2.0));
        registry.stop(&mut seat, NS0).expect("registry op ok");

        registry
            .start_playback(
                &mut seat,
                NS0,
                CommandOrigin::Wire,
                "seq",
                None,
                1.0,
                false,
                None,
                "exec-test".to_string(),
                t(10.0),
            )
            .expect("registry op ok");
        let SlotState::Playing(pb) = seat.slot.as_mut().expect("registry op ok") else {
            panic!("playing");
        };
        // At +0.0 only the first step is due.
        assert_eq!(pb.collect_due(0.0, 64).len(), 1);
        // At +1.5 the second is due, not the third.
        assert_eq!(pb.collect_due(1.5, 64).len(), 1);
        assert!(!pb.finished());
        // At +2.0 the third is due; playback is drained.
        assert_eq!(pb.collect_due(2.0, 64).len(), 1);
        assert!(pb.finished());
    }

    #[test]
    fn instant_playback_ignores_timing_but_respects_budget() {
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        seat.start_recording(NS0, "big", false, t(0.0))
            .expect("registry op ok");
        for index in 0..5 {
            seat.capture(&spawn(0x8000_0001 + index), t(index as f32));
        }
        registry.stop(&mut seat, NS0).expect("registry op ok");
        registry
            .start_playback(
                &mut seat,
                NS0,
                CommandOrigin::Wire,
                "big",
                None,
                1.0,
                true,
                None,
                "exec-test".to_string(),
                t(0.0),
            )
            .expect("registry op ok");
        let SlotState::Playing(pb) = seat.slot.as_mut().expect("registry op ok") else {
            panic!("playing");
        };
        // Instant ignores offsets; a budget of 2 caps the frame's emission.
        assert_eq!(pb.collect_due(0.0, 2).len(), 2);
        assert_eq!(pb.collect_due(0.0, 64).len(), 3, "the rest next frame");
        assert!(pb.finished());
    }

    #[test]
    fn rate_is_validated() {
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        seat.start_recording(NS0, "m", false, t(0.0))
            .expect("registry op ok");
        registry.stop(&mut seat, NS0).expect("registry op ok");
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let (code, _) = registry
                .start_playback(
                    &mut seat,
                    NS0,
                    CommandOrigin::Wire,
                    "m",
                    None,
                    bad,
                    false,
                    None,
                    "exec-test".to_string(),
                    t(0.0),
                )
                .expect_err("bad rate");
            assert_eq!(code, codes::BAD_PAYLOAD);
        }
    }

    #[test]
    fn privileged_playback_acquires_and_releases_the_scene_lock() {
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        seat.start_recording(NS0, "warp", false, t(0.0))
            .expect("registry op ok");
        seat.capture(&mode("3d"), t(0.0));
        registry.stop(&mut seat, NS0).expect("registry op ok");
        registry
            .start_playback(
                &mut seat,
                NS0,
                CommandOrigin::Wire,
                "warp",
                None,
                1.0,
                false,
                None,
                "exec-test".to_string(),
                t(0.0),
            )
            .expect("registry op ok");
        assert_eq!(
            registry.scene_lock,
            Some(NS0.terminal()),
            "privileged play takes the lock, keyed by the arrival TerminalId \
             (the stamp rule), never the namespace"
        );
        // Cancelling the playback releases the lock for the next privileged
        // operation.
        registry.stop(&mut seat, NS0).expect("registry op ok");
        assert_eq!(registry.scene_lock, None);
    }

    #[test]
    fn privileged_playback_rejected_while_scene_lock_held() {
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        seat.start_recording(NS0, "warp", false, t(0.0))
            .expect("registry op ok");
        seat.capture(&mode("3d"), t(0.0));
        registry.stop(&mut seat, NS0).expect("registry op ok");
        seat.start_recording(NS0, "plain", false, t(0.0))
            .expect("registry op ok");
        seat.capture(&spawn(0x8000_0001), t(0.0));
        registry.stop(&mut seat, NS0).expect("registry op ok");

        // Simulate another terminal holding the exclusive scene lock. (The
        // cross-agent contender is modelled by pinning the lock field
        // directly to a foreign TerminalId.)
        registry.scene_lock = Some(TerminalId::from_raw(5));
        let (code, _) = registry
            .start_playback(
                &mut seat,
                NS0,
                CommandOrigin::Wire,
                "warp",
                None,
                1.0,
                false,
                None,
                "exec-test".to_string(),
                t(0.0),
            )
            .expect_err("privileged play blocked by the held lock");
        assert_eq!(code, codes::SCENE_LOCKED);
        // A non-privileged macro is unaffected by the held scene lock.
        registry
            .start_playback(
                &mut seat,
                NS0,
                CommandOrigin::Wire,
                "plain",
                None,
                1.0,
                false,
                None,
                "exec-test".to_string(),
                t(0.0),
            )
            .expect("a non-privileged macro ignores the scene lock");
    }

    #[test]
    fn scope_defeats_shadowing_and_hash_addresses_directly() {
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        // A session macro and a trusted macro share the name "deploy".
        seat.start_recording(NS0, "deploy", false, t(0.0))
            .expect("registry op ok");
        seat.capture(&spawn(0x8000_0001), t(0.0));
        registry.stop(&mut seat, NS0).expect("registry op ok");
        let session_hash = registry
            .resolve(&seat, "deploy", None)
            .expect("registry op ok")
            .hash()
            .to_string();

        let steps = vec![
            MacroStep {
                offset: 0.0,
                command: spawn(0x8000_0009),
            },
            MacroStep {
                offset: 0.0,
                command: spawn(0x8000_000a),
            },
        ];
        let trusted = Macro {
            v: MACRO_VERSION,
            hash: content_hash(&steps, false),
            steps,
            privileged: false,
            rule_safe: true,
        };
        let trusted_hash = trusted.hash().to_string();
        registry
            .insert_trusted("deploy".to_string(), &trusted)
            .expect("registry op ok");

        // Unqualified resolves session first.
        assert_eq!(
            registry
                .resolve(&seat, "deploy", None)
                .expect("registry op ok")
                .step_count(),
            1
        );
        // scope=trusted defeats the shadow.
        assert_eq!(
            registry
                .resolve(&seat, "deploy", Some(MacroScope::Trusted))
                .expect("registry op ok")
                .step_count(),
            2
        );
        // Hash addresses the exact content across registries.
        assert_eq!(
            registry
                .resolve_by_hash(&seat, &trusted_hash)
                .expect("registry op ok")
                .step_count(),
            2
        );
        assert_eq!(
            registry
                .resolve_by_hash(&seat, &session_hash)
                .expect("registry op ok")
                .step_count(),
            1
        );
    }

    #[test]
    fn trusted_rejects_macro_control_and_survives_reset() {
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        let bad = Macro {
            v: MACRO_VERSION,
            steps: vec![MacroStep {
                offset: 0.0,
                command: RattyAiCommand::MacroStop,
            }],
            privileged: false,
            rule_safe: false,
            hash: "z".to_string(),
        };
        assert!(
            registry.insert_trusted("bad".to_string(), &bad).is_err(),
            "no recursion: a trusted macro may not contain macro.*"
        );

        let good = Macro {
            v: MACRO_VERSION,
            steps: vec![MacroStep {
                offset: 0.0,
                command: spawn(0x8000_0001),
            }],
            privileged: false,
            rule_safe: false,
            hash: "g".to_string(),
        };
        registry
            .insert_trusted("good".to_string(), &good)
            .expect("registry op ok");
        // A session macro to be cleared, and an active slot to be cancelled.
        seat.start_recording(NS0, "s", false, t(0.0))
            .expect("registry op ok");
        registry.reset(&mut seat, NS0.terminal());
        assert!(
            registry.resolve(&seat, "s", None).is_none(),
            "session cleared"
        );
        assert!(seat.slot.is_none(), "the slot is cancelled");
        assert!(
            registry
                .resolve(&seat, "good", Some(MacroScope::Trusted))
                .is_some(),
            "trusted survives reset"
        );
    }

    /// Reset is scoped to the ARRIVAL terminal (#56 decision 15's
    /// arrival-attach, decision 12's cross-runtime-interference ban):
    /// byte-identical to the old global clear at N=1, but at N>1 one
    /// agent's `reset` must not release a lock a FOREIGN terminal holds.
    /// Fails under the old unconditional `scene_lock = None`.
    #[test]
    fn reset_spares_a_foreign_terminals_scene_lock() {
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        let foreign = TerminalId::from_raw(5);
        registry.scene_lock = Some(foreign);
        seat.start_recording(NS0, "s", false, t(0.0))
            .expect("registry op ok");
        registry.reset(&mut seat, NS0.terminal());
        assert!(seat.slot.is_none(), "the arrival seat's slot is cancelled");
        assert_eq!(
            registry.scene_lock,
            Some(foreign),
            "a foreign holder's lock survives another terminal's reset"
        );
        // The arrival terminal's own held lock IS released.
        registry.scene_lock = Some(NS0.terminal());
        registry.reset(&mut seat, NS0.terminal());
        assert_eq!(
            registry.scene_lock, None,
            "the arrival terminal's own lock releases on its reset"
        );
    }

    #[test]
    fn state_projections_reflect_slots_and_stored_macros() {
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        // Idle: no executions.
        let idle = executions_state_value(&seat);
        assert_eq!(idle["items"].as_array().expect("array").len(), 0);

        // An active recording projects as a "recording" execution.
        seat.start_recording(NS0, "rec", false, t(0.0))
            .expect("record");
        seat.capture(&spawn(0x8000_0001), t(0.0));
        let exec = executions_state_value(&seat);
        assert_eq!(exec["items"][0]["kind"], "recording");
        assert_eq!(exec["items"][0]["commands"], 1);
        registry.stop(&mut seat, NS0).expect("finalize");

        // The finalized macro appears in state.macros, scoped session.
        let items = macros_state_items(&seat, &registry);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].1["name"], "rec");
        assert_eq!(items[0].1["scope"], "session");
        assert_eq!(items[0].1["commands"], 1);
    }

    #[test]
    fn rule_safety_is_sealed_at_finalize_and_gates_rule_origin_playback() {
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        // A macro of pure choreography (flash) is rule-safe.
        seat.start_recording(NS0, "soft", false, t(0.0))
            .expect("registry op ok");
        seat.capture(
            &RattyAiCommand::Flash {
                color: "#ff0000".to_string(),
                duration: 0.4,
            },
            t(0.0),
        );
        registry.stop(&mut seat, NS0).expect("registry op ok");
        let soft = registry
            .resolve(&seat, "soft", None)
            .expect("registry op ok");
        assert!(soft.is_rule_safe(), "pure choreography is rule-safe");
        assert!(!soft.is_privileged());

        // A spawn (respawn-class) breaks rule-safety; so does anything
        // scene-global.
        seat.start_recording(NS0, "hard", false, t(0.0))
            .expect("registry op ok");
        seat.capture(&spawn(0x8000_0001), t(0.0));
        registry.stop(&mut seat, NS0).expect("registry op ok");
        let hard = registry
            .resolve(&seat, "hard", None)
            .expect("registry op ok");
        assert!(!hard.is_rule_safe(), "spawn is not rule-safe");

        // A rule-fired play of the unsafe macro rejects at fire time; the
        // safe macro plays and its steps inherit the rule origin.
        let (code, _) = registry
            .start_playback(
                &mut seat,
                NS0,
                CommandOrigin::Rule,
                "hard",
                None,
                1.0,
                false,
                None,
                "exec-test".to_string(),
                t(0.0),
            )
            .expect_err("rule-origin play of a non-rule-safe macro rejects");
        assert_eq!(code, codes::NOT_PERMITTED);
        registry
            .start_playback(
                &mut seat,
                NS0,
                CommandOrigin::Rule,
                "soft",
                None,
                1.0,
                false,
                None,
                "exec-test".to_string(),
                t(0.0),
            )
            .expect("registry op ok");
        let SlotState::Playing(pb) = seat.slot.as_ref().expect("slot exists") else {
            panic!("playing");
        };
        assert_eq!(
            pb.origin,
            CommandOrigin::Rule,
            "a rule-started playback inherits the rule's causal context"
        );
    }

    #[test]
    fn insert_trusted_recomputes_rule_safety_from_the_steps() {
        let mut registry = MacroRegistry::default();
        let seat = TerminalMacros::default();
        let steps = vec![MacroStep {
            offset: 0.0,
            command: spawn(0x8000_0001),
        }];
        // The caller's flag lies; the registry recomputes from the steps.
        let lying = Macro {
            v: MACRO_VERSION,
            hash: content_hash(&steps, false),
            steps,
            privileged: false,
            rule_safe: true,
        };
        registry
            .insert_trusted("promoted".to_string(), &lying)
            .expect("registry op ok");
        assert!(
            !registry
                .resolve(&seat, "promoted", Some(MacroScope::Trusted))
                .expect("registry op ok")
                .is_rule_safe(),
            "derived state stays derived"
        );
    }

    fn app_test() -> App {
        let mut app = App::new();
        app.init_resource::<MacroRegistry>();
        app.world_mut().spawn((
            crate::identity::TerminalIdentity::test_boot(),
            crate::identity::terminal_session_state(),
        ));
        app.init_resource::<crate::query_channel::QuerySession>();
        app.init_resource::<Time>();
        app.add_message::<AiCommand>();
        app.add_message::<AckOutcome>();
        app.add_systems(Update, (apply_macro_commands, drive_macro_playback).chain());
        app
    }

    /// Resolves a session macro through the scaffold seat's component plus
    /// the trusted registry (the component-era `resource().resolve(0, ..)`).
    fn resolve_session(app: &mut App, name: &str) -> Option<Arc<Macro>> {
        let world = app.world_mut();
        let mut seats = world.query::<&TerminalMacros>();
        let seat = seats.single(world).expect("one seat");
        world.resource::<MacroRegistry>().resolve(seat, name, None)
    }

    fn send(app: &mut App, ack: Option<&str>, command: RattyAiCommand) {
        app.world_mut()
            .resource_mut::<Messages<AiCommand>>()
            .write(AiCommand {
                source: IngressSource::test_boot(),
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

    #[test]
    fn macro_play_acks_started_with_execution_handle_and_eta() {
        // #18 retrofit: macro.play is a long-running operation whose one
        // ack is ok=1;code=started with the handle and admission estimate.
        let mut app = app_test();
        send(
            &mut app,
            None,
            RattyAiCommand::MacroRecord {
                name: "x".to_string(),
                replace: false,
            },
        );
        send(&mut app, None, mode("3d"));
        send(&mut app, None, RattyAiCommand::MacroStop);
        drain_acks(&mut app);

        let nonce = app
            .world()
            .resource::<crate::query_channel::QuerySession>()
            .nonce_hex();
        send(
            &mut app,
            Some("p1"),
            RattyAiCommand::MacroPlay {
                name: "x".to_string(),
                hash: None,
                rate: 1.0,
                instant: false,
                scope: None,
            },
        );
        let acks = drain_acks(&mut app);
        let ack = acks.first().expect("play acks");
        assert!(ack.ok, "play commits");
        assert_eq!(ack.code, Some(codes::STARTED));
        let payload = ack.payload.as_ref().expect("started ack carries data");
        let id = payload["id"].as_str().expect("handle is a string");
        assert!(
            id.starts_with(&format!("{nonce}-")),
            "handle {id} is session-scoped"
        );
        assert_eq!(payload["position"], json!(0));
        assert!(payload["eta_ms"].is_u64(), "timed playback estimates in ms");

        // The zero-offset playback finished during the same update; the
        // freed slot admits an instant playback, whose honest estimate is
        // frames — Time promises no future frame duration at admission.
        send(
            &mut app,
            Some("p2"),
            RattyAiCommand::MacroPlay {
                name: "x".to_string(),
                hash: None,
                rate: 1.0,
                instant: true,
                scope: None,
            },
        );
        let acks = drain_acks(&mut app);
        let ack = acks.first().expect("instant play acks");
        assert_eq!(ack.code, Some(codes::STARTED));
        let payload = ack.payload.as_ref().expect("data present");
        assert!(payload["eta_frames"].is_u64());
        assert!(payload.get("eta_ms").is_none());
    }

    #[test]
    fn executions_projection_carries_the_playback_handle() {
        let mut registry = MacroRegistry::default();
        let mut seat = TerminalMacros::default();
        seat.test_record(NS0, "seq", &[mode("3d")]);
        registry
            .start_playback(
                &mut seat,
                NS0,
                CommandOrigin::Wire,
                "seq",
                None,
                1.0,
                false,
                None,
                "cafe-1".to_string(),
                t(0.0),
            )
            .expect("playback starts");
        let value = executions_state_value(&seat);
        assert_eq!(value["items"][0]["id"], json!("cafe-1"));
        assert_eq!(value["items"][0]["kind"], json!("playback"));
    }

    #[test]
    fn closed_loop_record_capture_stop_play_over_the_message_stream() {
        let mut app = app_test();

        // record;name=x acks a commit and opens a recording.
        send(
            &mut app,
            Some("r"),
            RattyAiCommand::MacroRecord {
                name: "x".to_string(),
                replace: false,
            },
        );
        let acks = drain_acks(&mut app);
        assert_eq!(acks.len(), 1);
        assert!(acks[0].ok, "record commits");

        // An ordinary recordable command is tapped off the stream — it needs
        // no applier here; the tap captures it directly.
        send(&mut app, None, spawn(0x8000_0001));

        // A rule-fired command in the same stream is reactive noise, not
        // authored choreography: the tap must skip it (#21).
        app.world_mut()
            .resource_mut::<Messages<AiCommand>>()
            .write(AiCommand {
                source: IngressSource::test_boot(),
                ack_token: None,
                origin: CommandOrigin::Rule,
                command: spawn(0x8000_0002),
            });
        app.update();

        // stop finalizes the macro with the single captured command — the
        // rule-origin spawn was never captured.
        send(&mut app, Some("s"), RattyAiCommand::MacroStop);
        assert!(drain_acks(&mut app)[0].ok, "stop finalizes");
        assert_eq!(
            resolve_session(&mut app, "x").map(|macro_| macro_.step_count()),
            Some(1),
        );

        // Clear the backlog, then play: the captured command replays into the
        // AiCommand stream exactly once, token-less.
        app.world_mut()
            .resource_mut::<Messages<AiCommand>>()
            .clear();
        send(
            &mut app,
            Some("p"),
            RattyAiCommand::MacroPlay {
                name: "x".to_string(),
                hash: None,
                rate: 1.0,
                instant: false,
                scope: None,
            },
        );
        assert!(drain_acks(&mut app)[0].ok, "play commits");
        let stream: Vec<AiCommand> = app
            .world_mut()
            .resource_mut::<Messages<AiCommand>>()
            .drain()
            .collect();
        let injected: Vec<&AiCommand> = stream
            .iter()
            .filter(|command| {
                command.ack_token.is_none()
                    && matches!(command.command, RattyAiCommand::SpawnObject { .. })
            })
            .collect();
        assert_eq!(
            injected.len(),
            1,
            "the captured command replays once, token-less"
        );
        // The playback drained its single step, so the slot is released.
        assert!(
            {
                let world = app.world_mut();
                let mut seats = world.query::<&TerminalMacros>();
                seats
                    .single(world)
                    .expect("one seat")
                    .execution_view()
                    .is_none()
            },
            "a finished playback clears the slot"
        );
    }

    /// Collaboration presence (#25) is control-plane like rule.*/sensor.*
    /// (see `command_classes_gate_recording_and_privilege` in osc.rs):
    /// presence identity is ingress truth, so a macro-replayed
    /// `user.join` would forge liveness and a replayed `user.leave`
    /// would evict a real participant. The tap must skip the family.
    #[test]
    fn presence_commands_classify_and_filter_like_their_classes() {
        let mut app = app_test();
        send(
            &mut app,
            None,
            RattyAiCommand::MacroRecord {
                name: "m".to_string(),
                replace: false,
            },
        );
        // Presence control in the recorded stream: never captured.
        send(
            &mut app,
            None,
            RattyAiCommand::UserJoin {
                id: "alice".to_string(),
                name: "alice".to_string(),
                color: "#00ff00".to_string(),
                ttl: None,
                replace: false,
            },
        );
        send(
            &mut app,
            None,
            RattyAiCommand::NoteRemove {
                id: "n1".to_string(),
            },
        );
        // An ordinary recordable command between them still captures.
        send(&mut app, None, spawn(0x8000_0001));
        send(&mut app, None, RattyAiCommand::MacroStop);
        let macro_ = resolve_session(&mut app, "m").expect("stored");
        assert_eq!(
            macro_.step_count(),
            1,
            "only the ordinary spawn was captured; the presence family is \
             control-plane and never enters a recording"
        );
    }
}
