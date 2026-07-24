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

use crate::ai::AiCommand;
use crate::osc::{MacroScope, RattyAiCommand};
use crate::query::codes;
use crate::query_channel::{AckOutcome, AiDiagnostics, ack_commit, reject};
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

/// A read-only projection of an agent's active slot for `state.executions`.
#[derive(Debug)]
pub struct ExecutionView {
    /// `"recording"` or `"playback"`.
    pub kind: &'static str,
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

/// The per-agent macro registries, slots, and the exclusive scene lock —
/// decision 3's session store, the trusted promoted store, and (as the
/// implementation sketch put it) the slot state plus the scene lock, all in
/// one resource.
#[derive(Resource, Default)]
pub struct MacroRegistry {
    /// Per-agent session macros, keyed by (namespace, name). Dies with the
    /// session; cleared by `reset`.
    session: HashMap<(u8, String), Arc<Macro>>,
    /// Trusted promoted macros, keyed by name. Wire-immutable and durable —
    /// only [`insert_trusted`](Self::insert_trusted) writes here, and `reset`
    /// spares them.
    trusted: HashMap<String, Arc<Macro>>,
    /// Each agent's single active slot, keyed by namespace.
    slots: HashMap<u8, SlotState>,
    /// The namespace currently holding the exclusive scene lock, if any. Only
    /// one privileged playback across all agents may hold it at a time.
    scene_lock: Option<u8>,
}

/// A rejection: the wire code plus a human message for the `state.errors`
/// ring. Registry methods return this; the system turns it into a `reject`.
type MacroReject = (&'static str, String);

impl MacroRegistry {
    /// Number of session macros stored in `namespace`.
    pub fn session_len(&self, namespace: u8) -> usize {
        self.session
            .keys()
            .filter(|(entry_namespace, _)| *entry_namespace == namespace)
            .count()
    }

    /// Whether any agent has an active playback (the `drive_macro_playback`
    /// run condition).
    pub fn has_active_playback(&self) -> bool {
        self.slots
            .values()
            .any(|slot| matches!(slot, SlotState::Playing(_)))
    }

    /// Promotes a macro into the durable, wire-immutable trusted registry.
    /// This is the trusted-tier entry point (config / CLI / UI / controller);
    /// the wire can never reach it. A macro carrying any macro-control
    /// command is rejected (no recursion, belt-and-suspenders beside the tap
    /// that already refuses to capture `macro.*`).
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
        self.trusted.insert(
            name,
            Arc::new(Macro {
                v: steps_source.v,
                steps: steps_source.steps.clone(),
                privileged: steps_source.privileged,
                hash: steps_source.hash.clone(),
            }),
        );
        Ok(())
    }

    /// Iterates `namespace`'s session macros in arbitrary order.
    pub fn iter_session(&self, namespace: u8) -> impl Iterator<Item = (&str, &Macro)> {
        self.session
            .iter()
            .filter(move |((entry_namespace, _), _)| *entry_namespace == namespace)
            .map(|((_, name), macro_)| (name.as_str(), macro_.as_ref()))
    }

    /// Iterates the trusted macros in arbitrary order.
    pub fn iter_trusted(&self) -> impl Iterator<Item = (&str, &Macro)> {
        self.trusted
            .iter()
            .map(|(name, macro_)| (name.as_str(), macro_.as_ref()))
    }

    /// A projection of `namespace`'s active slot, if any.
    pub fn execution_view(&self, namespace: u8) -> Option<ExecutionView> {
        match self.slots.get(&namespace)? {
            SlotState::Recording(rec) => Some(ExecutionView {
                kind: "recording",
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

    /// Resolves a macro by name under the given scope. `None` resolves the
    /// caller's session registry first, then the trusted registry.
    fn resolve(&self, namespace: u8, name: &str, scope: Option<MacroScope>) -> Option<Arc<Macro>> {
        let session = || self.session.get(&(namespace, name.to_string()));
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
    /// session).
    fn resolve_by_hash(&self, namespace: u8, hash: &str) -> Option<Arc<Macro>> {
        self.session
            .iter()
            .filter(|((entry_namespace, _), _)| *entry_namespace == namespace)
            .map(|(_, macro_)| macro_)
            .chain(self.trusted.values())
            .find(|macro_| macro_.hash == hash)
            .cloned()
    }

    /// Starts a recording for `source`. Validates the name, the single-slot
    /// invariant, the collision rule, and the per-namespace cap.
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
        if self.slots.contains_key(&namespace) {
            return Err((
                codes::BUSY,
                "a recording or playback is already active for this agent".to_string(),
            ));
        }
        let exists = self.session.contains_key(&(namespace, name.to_string()));
        if exists && !replace {
            return Err((
                codes::ALREADY_EXISTS,
                format!("macro '{name}' exists (pass mode=replace to overwrite it)"),
            ));
        }
        if !exists && self.session_len(namespace) >= MAX_MACROS_PER_NAMESPACE {
            return Err((
                codes::NAMESPACE_CAP,
                format!("namespace {namespace} is at its {MAX_MACROS_PER_NAMESPACE}-macro limit"),
            ));
        }
        self.slots.insert(
            namespace,
            SlotState::Recording(ActiveRecording {
                name: name.to_string(),
                started: now,
                steps: Vec::new(),
                privileged: false,
                poisoned: None,
            }),
        );
        Ok(())
    }

    /// Taps a recordable command into `source`'s active recording, if any.
    /// Enforces the commands-per-macro and wall-clock limits by poisoning the
    /// recording (discarded at `macro.stop`) rather than truncating silently.
    fn capture(&mut self, source: IngressSource, command: &RattyAiCommand, now: Duration) {
        let Some(SlotState::Recording(rec)) = self.slots.get_mut(&source.namespace()) else {
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
        rec.steps.push(MacroStep {
            offset,
            command: command.clone(),
        });
    }

    /// Finalizes an active recording (saving it, transactionally) or cancels
    /// an active playback. `nothing-active` when the slot is idle.
    fn stop(&mut self, source: IngressSource) -> Result<(), MacroReject> {
        let namespace = source.namespace();
        match self.slots.remove(&namespace) {
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
                    hash,
                });
                self.session.insert((namespace, name), macro_);
                Ok(())
            }
            Some(SlotState::Playing(pb)) => {
                if pb.scene_locked {
                    self.scene_lock = None;
                }
                Ok(())
            }
            None => Err((
                codes::NOTHING_ACTIVE,
                "no active recording or playback to stop".to_string(),
            )),
        }
    }

    /// Starts a playback for `source`. Enforces the single slot, validates
    /// the rate, resolves and pins the macro version, and acquires the
    /// exclusive scene lock for a privileged macro.
    #[allow(clippy::too_many_arguments)]
    fn start_playback(
        &mut self,
        source: IngressSource,
        name: &str,
        hash: Option<&str>,
        rate: f32,
        instant: bool,
        scope: Option<MacroScope>,
        now: Duration,
    ) -> Result<(), MacroReject> {
        let namespace = source.namespace();
        if self.slots.contains_key(&namespace) {
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
            Some(hash) => self.resolve_by_hash(namespace, hash),
            None => self.resolve(namespace, name, scope),
        };
        let Some(macro_) = macro_ else {
            return Err((
                codes::UNKNOWN_ID,
                "no macro resolves under the given name/hash and scope".to_string(),
            ));
        };
        let scene_locked = if macro_.privileged {
            if self.scene_lock.is_some() {
                return Err((
                    codes::SCENE_LOCKED,
                    "a privileged macro cannot play while the exclusive scene lock is held"
                        .to_string(),
                ));
            }
            self.scene_lock = Some(namespace);
            true
        } else {
            false
        };
        self.slots.insert(
            namespace,
            SlotState::Playing(ActivePlayback {
                source,
                macro_,
                rate,
                instant,
                started: now,
                next_index: 0,
                scene_locked,
            }),
        );
        Ok(())
    }

    /// Full session reset: cancel active slots, drop every session macro, and
    /// release the scene lock. Trusted macros are durable and survive. Called
    /// from the `reset` command's tap; that command owns its ack elsewhere.
    fn reset(&mut self) {
        self.session.clear();
        self.slots.clear();
        self.scene_lock = None;
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
                    .run_if(|registry: Res<MacroRegistry>| registry.has_active_playback()),
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
    mut acks: MessageWriter<AckOutcome>,
    mut diagnostics: ResMut<AiDiagnostics>,
) {
    let now = time.elapsed();
    for AiCommand {
        source,
        ack_token,
        command,
    } in commands.read()
    {
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
                match registry.start_recording(*source, name, *replace, now) {
                    Ok(()) => ack_commit(&mut acks, *source, ack_token),
                    Err((code, message)) => {
                        warn!("ratty-ai: macro.record rejected: {message}");
                        reject!("macro.record", code, message);
                    }
                }
            }
            RattyAiCommand::MacroStop => match registry.stop(*source) {
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
            } => match registry.start_playback(
                *source,
                name,
                hash.as_deref(),
                *rate,
                *instant,
                *scope,
                now,
            ) {
                Ok(()) => ack_commit(&mut acks, *source, ack_token),
                Err((code, message)) => {
                    warn!("ratty-ai: macro.play rejected: {message}");
                    reject!("macro.play", code, message);
                }
            },
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
                // Full session reset. Reset's single ack belongs to
                // apply_ai_commands; the macro registry clears silently.
                registry.reset();
            }
            other => {
                // Recorder tap. macro.* and reset are handled above and never
                // reach here; the control-plane class (react/rule.*) is
                // excluded (#21). Everything else is recordable choreography,
                // captured into the caller's own active recording (if any).
                if other.is_control_plane() {
                    continue;
                }
                registry.capture(*source, other, now);
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
    mut commands: MessageWriter<AiCommand>,
) {
    let now = time.elapsed();
    let mut spent = 0_usize;
    let mut finished: Vec<u8> = Vec::new();
    for (namespace, slot) in registry.slots.iter_mut() {
        let SlotState::Playing(playback) = slot else {
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
                command,
            });
        }
        if playback.finished() {
            finished.push(*namespace);
        }
    }
    for namespace in finished {
        if let Some(SlotState::Playing(playback)) = registry.slots.remove(&namespace)
            && playback.scene_locked
        {
            registry.scene_lock = None;
        }
    }
}

/// `state.macros`: the caller's session macros plus the trusted macros, each
/// tagged with its scope. Deterministically ordered and paginated so a large
/// registry never overflows a reply page.
pub fn macros_state_items(registry: &MacroRegistry, namespace: u8) -> Vec<(u64, Value)> {
    let mut rows: Vec<(String, Value)> = Vec::new();
    for (name, macro_) in registry.iter_session(namespace) {
        rows.push((
            format!("session\u{0}{name}"),
            json!({
                "name": name,
                "scope": "session",
                "v": macro_.version(),
                "commands": macro_.step_count(),
                "privileged": macro_.is_privileged(),
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
pub fn executions_state_value(registry: &MacroRegistry, namespace: u8) -> Value {
    let items: Vec<Value> = registry
        .execution_view(namespace)
        .map(|view| {
            let mut value = json!({
                "kind": view.kind,
                "name": view.name,
                "privileged": view.privileged,
                "commands": view.commands,
                "scene_locked": view.scene_locked,
            });
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

    const NS0: IngressSource = IngressSource::Local;

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
        registry
            .start_recording(NS0, "deploy", false, t(0.0))
            .expect("record starts");
        // Two ordinary commands at t=0 and t=1.5.
        registry.capture(NS0, &spawn(0x8000_0001), t(0.0));
        registry.capture(NS0, &spawn(0x8000_0002), t(1.5));
        registry.stop(NS0).expect("finalize");

        let macro_ = registry.resolve(0, "deploy", None).expect("stored");
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
        registry
            .start_recording(NS0, "m", false, t(0.0))
            .expect("registry op ok");
        // A scene-global command records and marks the macro privileged.
        registry.capture(NS0, &mode("3d"), t(0.0));
        registry.stop(NS0).expect("registry op ok");
        let macro_ = registry.resolve(0, "m", None).expect("registry op ok");
        assert_eq!(macro_.step_count(), 1);
        assert!(macro_.is_privileged(), "mode is scene-global → privileged");
    }

    #[test]
    fn single_slot_rejects_busy() {
        let mut registry = MacroRegistry::default();
        registry
            .start_recording(NS0, "a", false, t(0.0))
            .expect("registry op ok");
        let (code, _) = registry
            .start_recording(NS0, "b", false, t(0.0))
            .expect_err("second op is busy");
        assert_eq!(code, codes::BUSY);
        let (code, _) = registry
            .start_playback(NS0, "a", None, 1.0, false, None, t(0.0))
            .expect_err("play while recording is busy");
        assert_eq!(code, codes::BUSY);
    }

    #[test]
    fn collision_rule_and_transactional_replace() {
        let mut registry = MacroRegistry::default();
        registry
            .start_recording(NS0, "x", false, t(0.0))
            .expect("registry op ok");
        registry.capture(NS0, &spawn(0x8000_0001), t(0.0));
        registry.stop(NS0).expect("registry op ok");

        // Same name without replace: rejected.
        let (code, _) = registry
            .start_recording(NS0, "x", false, t(0.0))
            .expect_err("already exists");
        assert_eq!(code, codes::ALREADY_EXISTS);

        // Replace: the old macro survives until the new one finalizes.
        registry
            .start_recording(NS0, "x", true, t(0.0))
            .expect("registry op ok");
        assert_eq!(
            registry
                .resolve(0, "x", None)
                .expect("registry op ok")
                .step_count(),
            1,
            "old version is intact mid-recording"
        );
        registry.capture(NS0, &spawn(0x8000_0002), t(0.0));
        registry.capture(NS0, &spawn(0x8000_0003), t(0.0));
        registry.stop(NS0).expect("registry op ok");
        assert_eq!(
            registry
                .resolve(0, "x", None)
                .expect("registry op ok")
                .step_count(),
            2,
            "finalize swaps to the new version"
        );
    }

    #[test]
    fn cancelled_replace_preserves_the_old_version() {
        let mut registry = MacroRegistry::default();
        registry
            .start_recording(NS0, "x", false, t(0.0))
            .expect("registry op ok");
        registry.capture(NS0, &spawn(0x8000_0001), t(0.0));
        registry.stop(NS0).expect("registry op ok");
        let old_hash = registry
            .resolve(0, "x", None)
            .expect("registry op ok")
            .hash()
            .to_string();

        // A replace recording that never finalizes (poisoned) leaves the old
        // version untouched.
        registry
            .start_recording(NS0, "x", true, t(0.0))
            .expect("registry op ok");
        for index in 0..=MAX_COMMANDS_PER_MACRO as u32 {
            registry.capture(NS0, &spawn(0x8000_0100 + index), t(0.0));
        }
        let (code, _) = registry.stop(NS0).expect_err("poisoned recording");
        assert_eq!(code, codes::TOO_LARGE);
        assert_eq!(
            registry
                .resolve(0, "x", None)
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
        for index in 0..MAX_MACROS_PER_NAMESPACE {
            registry
                .start_recording(NS0, &format!("m{index}"), false, t(0.0))
                .expect("registry op ok");
            registry.stop(NS0).expect("registry op ok");
        }
        let (code, _) = registry
            .start_recording(NS0, "overflow", false, t(0.0))
            .expect_err("at the cap");
        assert_eq!(code, codes::NAMESPACE_CAP);
        // Replacing an existing name at the cap is not a new slot.
        registry
            .start_recording(NS0, "m0", true, t(0.0))
            .expect("registry op ok");
    }

    #[test]
    fn name_validation() {
        let mut registry = MacroRegistry::default();
        let (code, _) = registry
            .start_recording(NS0, "", false, t(0.0))
            .expect_err("empty");
        assert_eq!(code, codes::BAD_PAYLOAD);
        let (code, _) = registry
            .start_recording(NS0, &"x".repeat(MAX_MACRO_NAME_BYTES + 1), false, t(0.0))
            .expect_err("too long");
        assert_eq!(code, codes::TOO_LARGE);
    }

    #[test]
    fn stop_when_idle_is_nothing_active() {
        let mut registry = MacroRegistry::default();
        let (code, _) = registry.stop(NS0).expect_err("nothing to stop");
        assert_eq!(code, codes::NOTHING_ACTIVE);
    }

    #[test]
    fn playback_collects_due_steps_and_finishes() {
        let mut registry = MacroRegistry::default();
        registry
            .start_recording(NS0, "seq", false, t(0.0))
            .expect("registry op ok");
        registry.capture(NS0, &spawn(0x8000_0001), t(0.0));
        registry.capture(NS0, &spawn(0x8000_0002), t(1.0));
        registry.capture(NS0, &spawn(0x8000_0003), t(2.0));
        registry.stop(NS0).expect("registry op ok");

        registry
            .start_playback(NS0, "seq", None, 1.0, false, None, t(10.0))
            .expect("registry op ok");
        let SlotState::Playing(pb) = registry.slots.get_mut(&0).expect("registry op ok") else {
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
        registry
            .start_recording(NS0, "big", false, t(0.0))
            .expect("registry op ok");
        for index in 0..5 {
            registry.capture(NS0, &spawn(0x8000_0001 + index), t(index as f32));
        }
        registry.stop(NS0).expect("registry op ok");
        registry
            .start_playback(NS0, "big", None, 1.0, true, None, t(0.0))
            .expect("registry op ok");
        let SlotState::Playing(pb) = registry.slots.get_mut(&0).expect("registry op ok") else {
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
        registry
            .start_recording(NS0, "m", false, t(0.0))
            .expect("registry op ok");
        registry.stop(NS0).expect("registry op ok");
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let (code, _) = registry
                .start_playback(NS0, "m", None, bad, false, None, t(0.0))
                .expect_err("bad rate");
            assert_eq!(code, codes::BAD_PAYLOAD);
        }
    }

    #[test]
    fn privileged_playback_acquires_and_releases_the_scene_lock() {
        let mut registry = MacroRegistry::default();
        registry
            .start_recording(NS0, "warp", false, t(0.0))
            .expect("registry op ok");
        registry.capture(NS0, &mode("3d"), t(0.0));
        registry.stop(NS0).expect("registry op ok");
        registry
            .start_playback(NS0, "warp", None, 1.0, false, None, t(0.0))
            .expect("registry op ok");
        assert_eq!(
            registry.scene_lock,
            Some(0),
            "privileged play takes the lock"
        );
        // Cancelling the playback releases the lock for the next privileged
        // operation.
        registry.stop(NS0).expect("registry op ok");
        assert_eq!(registry.scene_lock, None);
    }

    #[test]
    fn privileged_playback_rejected_while_scene_lock_held() {
        let mut registry = MacroRegistry::default();
        registry
            .start_recording(NS0, "warp", false, t(0.0))
            .expect("registry op ok");
        registry.capture(NS0, &mode("3d"), t(0.0));
        registry.stop(NS0).expect("registry op ok");
        registry
            .start_recording(NS0, "plain", false, t(0.0))
            .expect("registry op ok");
        registry.capture(NS0, &spawn(0x8000_0001), t(0.0));
        registry.stop(NS0).expect("registry op ok");

        // Simulate another agent holding the exclusive scene lock. (Only one
        // ingress source exists today, so the cross-agent contender is
        // modelled by pinning the lock field directly.)
        registry.scene_lock = Some(5);
        let (code, _) = registry
            .start_playback(NS0, "warp", None, 1.0, false, None, t(0.0))
            .expect_err("privileged play blocked by the held lock");
        assert_eq!(code, codes::SCENE_LOCKED);
        // A non-privileged macro is unaffected by the held scene lock.
        registry
            .start_playback(NS0, "plain", None, 1.0, false, None, t(0.0))
            .expect("a non-privileged macro ignores the scene lock");
    }

    #[test]
    fn scope_defeats_shadowing_and_hash_addresses_directly() {
        let mut registry = MacroRegistry::default();
        // A session macro and a trusted macro share the name "deploy".
        registry
            .start_recording(NS0, "deploy", false, t(0.0))
            .expect("registry op ok");
        registry.capture(NS0, &spawn(0x8000_0001), t(0.0));
        registry.stop(NS0).expect("registry op ok");
        let session_hash = registry
            .resolve(0, "deploy", None)
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
        };
        let trusted_hash = trusted.hash().to_string();
        registry
            .insert_trusted("deploy".to_string(), &trusted)
            .expect("registry op ok");

        // Unqualified resolves session first.
        assert_eq!(
            registry
                .resolve(0, "deploy", None)
                .expect("registry op ok")
                .step_count(),
            1
        );
        // scope=trusted defeats the shadow.
        assert_eq!(
            registry
                .resolve(0, "deploy", Some(MacroScope::Trusted))
                .expect("registry op ok")
                .step_count(),
            2
        );
        // Hash addresses the exact content across registries.
        assert_eq!(
            registry
                .resolve_by_hash(0, &trusted_hash)
                .expect("registry op ok")
                .step_count(),
            2
        );
        assert_eq!(
            registry
                .resolve_by_hash(0, &session_hash)
                .expect("registry op ok")
                .step_count(),
            1
        );
    }

    #[test]
    fn trusted_rejects_macro_control_and_survives_reset() {
        let mut registry = MacroRegistry::default();
        let bad = Macro {
            v: MACRO_VERSION,
            steps: vec![MacroStep {
                offset: 0.0,
                command: RattyAiCommand::MacroStop,
            }],
            privileged: false,
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
            hash: "g".to_string(),
        };
        registry
            .insert_trusted("good".to_string(), &good)
            .expect("registry op ok");
        // A session macro to be cleared, and an active slot to be cancelled.
        registry
            .start_recording(NS0, "s", false, t(0.0))
            .expect("registry op ok");
        registry.reset();
        assert!(registry.resolve(0, "s", None).is_none(), "session cleared");
        assert!(registry.slots.is_empty(), "slots cancelled");
        assert!(
            registry
                .resolve(0, "good", Some(MacroScope::Trusted))
                .is_some(),
            "trusted survives reset"
        );
    }

    #[test]
    fn state_projections_reflect_slots_and_stored_macros() {
        let mut registry = MacroRegistry::default();
        // Idle: no executions.
        let idle = executions_state_value(&registry, 0);
        assert_eq!(idle["items"].as_array().expect("array").len(), 0);

        // An active recording projects as a "recording" execution.
        registry
            .start_recording(NS0, "rec", false, t(0.0))
            .expect("record");
        registry.capture(NS0, &spawn(0x8000_0001), t(0.0));
        let exec = executions_state_value(&registry, 0);
        assert_eq!(exec["items"][0]["kind"], "recording");
        assert_eq!(exec["items"][0]["commands"], 1);
        registry.stop(NS0).expect("finalize");

        // The finalized macro appears in state.macros, scoped session.
        let items = macros_state_items(&registry, 0);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].1["name"], "rec");
        assert_eq!(items[0].1["scope"], "session");
        assert_eq!(items[0].1["commands"], 1);
    }

    fn app_test() -> App {
        let mut app = App::new();
        app.init_resource::<MacroRegistry>();
        app.init_resource::<AiDiagnostics>();
        app.init_resource::<Time>();
        app.add_message::<AiCommand>();
        app.add_message::<AckOutcome>();
        app.add_systems(Update, (apply_macro_commands, drive_macro_playback).chain());
        app
    }

    fn send(app: &mut App, ack: Option<&str>, command: RattyAiCommand) {
        app.world_mut()
            .resource_mut::<Messages<AiCommand>>()
            .write(AiCommand {
                source: IngressSource::Local,
                ack_token: ack.map(str::to_string),
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

        // stop finalizes the macro with the single captured command.
        send(&mut app, Some("s"), RattyAiCommand::MacroStop);
        assert!(drain_acks(&mut app)[0].ok, "stop finalizes");
        assert_eq!(
            app.world()
                .resource::<MacroRegistry>()
                .resolve(0, "x", None)
                .map(|macro_| macro_.step_count()),
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
            app.world()
                .resource::<MacroRegistry>()
                .execution_view(0)
                .is_none(),
            "a finished playback clears the slot"
        );
    }
}
