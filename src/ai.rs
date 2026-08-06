//! `ratty-ai` command handling: the Bevy side of the OSC 777 control channel.
//!
//! [`crate::osc`] parses OSC 777 sequences into [`RattyAiCommand`]s inside the
//! parser callbacks; [`crate::systems::pump_pty_output`] drains them and emits
//! them as [`AiCommand`] messages. The handler systems here act on those
//! messages. Because the parser runs inside a Bevy system, no cross-thread
//! channel is needed — the messages are produced and consumed on the same
//! thread, the same frame.
//!
//! Every command carries the [`IngressSource`] its bytes arrived through.
//! Authority derives from that ingress context, never from the byte stream:
//! object commands may only touch ids inside the caller's AI namespace (see
//! [`crate::osc::ai_object_namespace`]), and `object.add`/`cursor` asset
//! names resolve against the embedded registry only — these commands never
//! read a filesystem path.
//!
//! Presentation (mode / warp / reset), objects, and the cursor lower onto
//! real subsystems; the remaining operator-console commands are logged until
//! their subsystems are built, so nothing is silently dropped and nothing is
//! faked as working.

use std::collections::HashSet;

use bevy::ecs::message::{Message, MessageReader, MessageWriter};
use bevy::prelude::*;

use crate::config::AppConfig;
use crate::inline::{AiUpdateOutcome, InlineStyle, TerminalInlineObjects};
use crate::model::{
    CursorModelChoice, CursorSettings, ObjectLoadOptions, embedded_object_loadable,
    load_embedded_object_source,
};
use crate::osc::{RattyAiCommand, ai_object_namespace};
use crate::runtime::IngressSource;
use crate::scene::{
    MobiusTransition, StageTween, TerminalPlaneView, TerminalPlaneWarp, TerminalPresentation,
    TerminalPresentationMode, apply_stage_mode_change,
};
use crate::terminal::TerminalRedrawState;

/// Upper bound on live AI objects per agent namespace: an honest failure
/// instead of an unbounded registry driven by untrusted output.
pub const MAX_AI_OBJECTS_PER_NAMESPACE: usize = 64;

/// Upper bound on distinct object ids an AI session may ever spawn. The
/// never-reuse ledger records every id used so a removed id cannot come
/// back, so it must be capped: without this, a stream of add/remove pairs
/// on fresh ids would grow the ledger without bound.
pub const MAX_AI_OBJECT_IDS_PER_SESSION: usize = 4096;

/// Why a command entered the stream — its *causal* origin, orthogonal to
/// the authority-bearing [`IngressSource`] (a rule-fired command still runs
/// under its owner's namespace and authority). Injectors stamp their own
/// variant; two consumers read it:
///
/// - the macro recorder tap skips [`CommandOrigin::Rule`] commands, so an
///   active recording never absorbs reactive fire noise as authored
///   choreography, and
/// - the reactive organ refuses rule-consumable input (sensor publishes,
///   rule mutations) that did not arrive as [`CommandOrigin::Wire`] — the
///   #21 chain-depth-one guard, belt-and-suspenders beside the structural
///   closure (sensor/rule commands are control-plane, so neither macros
///   nor rule actions can ever carry them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOrigin {
    /// Parsed at live ingress (the PTY or the wasm virtual channel).
    Wire,
    /// Re-injected by a caller-started macro playback.
    Macro,
    /// Relowered by a bookmark jump.
    Bookmark,
    /// Fired by a reactive rule — directly, or replayed by a rule-started
    /// macro playback (the causal execution context is inherited, #21).
    Rule,
}

/// A `ratty-ai` control command delivered to the Bevy world.
///
/// Wraps [`RattyAiCommand`] (which stays dependency-free so the `ratty-ai`
/// CLI can share the parser) together with the trusted ingress context the
/// command's bytes arrived through.
#[derive(Message, Debug, Clone)]
pub struct AiCommand {
    /// Where the command's bytes physically entered the terminal.
    pub source: IngressSource,
    /// The `tok=` ack opt-in token, when the command carried one. Exactly
    /// one handler system owns each command's ack (see the per-variant
    /// comments in the handlers); commands without a token stay
    /// fire-and-forget. Correlation tokens are transport metadata — the
    /// macro recorder never captures them, and injected commands are
    /// always token-less.
    pub ack_token: Option<String>,
    /// The causal origin of the command (see [`CommandOrigin`]).
    pub origin: CommandOrigin,
    /// The parsed command.
    pub command: RattyAiCommand,
}

/// Emitted for every AI object removed via `object.remove`, `object.clear`,
/// or `reset`, so registries, replay, and persistence observe removals.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AiObjectRemoved {
    /// The removed object's id.
    pub id: u32,
}

/// Session-lifetime AI object id bookkeeping.
///
/// Ids are never reused within a session: once spawned, an id stays reserved
/// even after removal. `replace=true` replaces a live object under its id;
/// it does not free the id for a different lifetime.
#[derive(Resource, Default)]
pub struct AiObjectRegistry {
    used: HashSet<u32>,
}

impl AiObjectRegistry {
    /// Despawn sweep (#56 decision 17 / the #56 rider: "recycling is safe
    /// only while the object-id ledger dies with its terminal"): frees a
    /// dead terminal's id space. Ids embed the namespace, so without this
    /// the recycled slot's next tenant would inherit the corpse's
    /// exhausted ledger — never-reuse is a per-terminal-lifetime
    /// guarantee, not a per-slot one.
    pub(crate) fn sweep_namespace(&mut self, namespace: u8) {
        self.used
            .retain(|id| crate::osc::ai_object_namespace(*id) != Some(namespace));
    }

    /// Test-only: reserves an id, for the cross-module despawn-sweep test.
    #[cfg(test)]
    pub(crate) fn test_reserve(&mut self, id: u32) {
        self.used.insert(id);
    }

    /// Test-only: whether an id is reserved.
    #[cfg(test)]
    pub(crate) fn test_is_used(&self, id: u32) -> bool {
        self.used.contains(&id)
    }
}

/// Registers the AI command message and its handler systems.
pub struct RattyAiPlugin;

impl Plugin for RattyAiPlugin {
    fn build(&self, app: &mut App) {
        // Ordered after the RGP stage systems (not just pump_pty_output) so
        // that when an RGP `c` stage sequence and an OSC stage command arrive
        // in the same PTY chunk, the explicit AI command deterministically
        // wins the shared stage resources rather than racing an arbitrary
        // Bevy schedule tiebreak. apply_terminal_presentation is in turn
        // ordered after this system (see plugin.rs).
        app.add_message::<AiCommand>()
            .add_message::<AiObjectRemoved>()
            .add_message::<crate::query_channel::QueryRequest>()
            .add_message::<crate::query_channel::AckOutcome>()
            .init_resource::<AiObjectRegistry>()
            .init_resource::<crate::query_channel::QuerySession>()
            .add_systems(
                Update,
                apply_ai_commands
                    .after(crate::systems::pump_pty_output)
                    .after(crate::systems::apply_rgp_stage)
                    .after(crate::systems::animate_stage_tween),
            )
            // Ordered before sync_inline_objects so object mutations spawn
            // and despawn their entities the same frame they arrive.
            .add_systems(
                Update,
                apply_ai_object_commands
                    .after(crate::systems::pump_pty_output)
                    .before(crate::systems::sync_inline_objects),
            )
            // The query channel answers after every command applier so a
            // same-chunk write-then-read observes the write, and acks are
            // emitted after the outcome they report is decided.
            .add_systems(
                Update,
                crate::query_channel::answer_queries
                    .after(crate::systems::pump_pty_output)
                    .after(apply_ai_commands)
                    .after(apply_ai_object_commands)
                    .after(crate::effects::apply_ai_effect_commands)
                    .after(crate::viz::apply_viz_commands)
                    .after(crate::sound::apply_sound_commands)
                    .after(crate::bookmarks::apply_bookmark_commands)
                    .after(crate::macros::apply_macro_commands)
                    .after(crate::reactive::apply_reactive_commands)
                    .after(crate::reactive::evaluate_rules)
                    .after(crate::avatar::drive_avatar_speech)
                    .after(crate::avatar::apply_avatar_commands)
                    .after(crate::presence::apply_presence_commands),
            );
    }
}

/// Applies queued `ratty-ai` commands to the presentation resources.
///
/// Mode/warp/reset lower onto the same machinery the RGP `c` verb uses, so
/// they take effect the frame they arrive. Commands whose subsystem does not
/// exist yet are logged rather than dropped.
#[allow(clippy::too_many_arguments)]
pub fn apply_ai_commands(
    mut commands: MessageReader<AiCommand>,
    mut presentation: ResMut<TerminalPresentation>,
    mut seats: Query<(
        &crate::identity::TerminalIdentity,
        &mut TerminalPlaneWarp,
        &mut TerminalRedrawState,
    )>,
    mut plane_view: ResMut<TerminalPlaneView>,
    mut mobius: ResMut<MobiusTransition>,
    mut stage_tween: ResMut<StageTween>,
    mut acks: MessageWriter<crate::query_channel::AckOutcome>,
    mut diagnostics: crate::query_channel::DiagnosticsSink,
) {
    use crate::query::codes;
    use crate::query_channel::{ack_commit, reject};

    for AiCommand {
        source,
        ack_token,
        command,
        ..
    } in commands.read()
    {
        match command {
            RattyAiCommand::SetMode { mode } => {
                let Some(target) = parse_mode(mode) else {
                    warn!("ratty-ai: unknown mode '{mode}' (2d, 3d, mobius)");
                    reject(
                        &mut diagnostics,
                        &mut acks,
                        *source,
                        ack_token,
                        "mode",
                        codes::BAD_MODE,
                        format!("unknown mode '{mode}' (2d, 3d, mobius)"),
                    );
                    continue;
                };
                // Requesting the already-active mode is an idempotent
                // commit, so it acks ok either way. A mode flip is
                // scene-global: every seat's texture cursor restyles.
                if apply_stage_mode_change(target, &mut presentation, &plane_view, &mut mobius) {
                    stage_tween.stop();
                    for (_, _, mut redraw) in seats.iter_mut() {
                        redraw.request();
                    }
                }
                ack_commit(&mut acks, *source, ack_token);
            }
            RattyAiCommand::SetWarp { intensity } => {
                // Warp is the ARRIVAL terminal's own surface shape (#52's
                // per-terminal half, arrival-is-the-address): the command
                // morphs its own plane, never a neighbor's. Liveness gates
                // EVERY side effect — a dead arrival's command must not
                // even stop a running camera tween.
                let Some((_, mut plane_warp, mut redraw)) = seats
                    .iter_mut()
                    .find(|(identity, ..)| identity.id() == source.terminal())
                else {
                    warn!("apply_ai_commands: warp dropped: its arrival terminal is gone");
                    continue;
                };
                // An explicit warp command wins over a running camera tween.
                stage_tween.stop();
                plane_warp.amount = intensity.clamp(0.0, 1.0);
                redraw.request();
                ack_commit(&mut acks, *source, ack_token);
            }
            RattyAiCommand::Reset => {
                // Full session reset, scene-wide by contract (the object
                // ledger and presence reset globally too): every seat's
                // warp flattens and every seat repaints.
                presentation.mode = TerminalPresentationMode::Flat2d;
                *plane_view = TerminalPlaneView::default();
                mobius.stop();
                stage_tween.stop();
                for (_, mut plane_warp, mut redraw) in seats.iter_mut() {
                    plane_warp.amount = 0.0;
                    redraw.request();
                }
                // Reset is handled by three systems; this one owns its
                // single ack (objects and effects reset silently).
                ack_commit(&mut acks, *source, ack_token);
            }
            // The soul: flash/pulse/tint/think/confidence/mood are handled by
            // the effects overlay (crate::effects), which reads the same
            // AiCommand messages independently and owns their acks.
            RattyAiCommand::Flash { .. }
            | RattyAiCommand::Pulse { .. }
            | RattyAiCommand::Tint { .. }
            | RattyAiCommand::Think { .. }
            | RattyAiCommand::Confidence { .. }
            | RattyAiCommand::Mood { .. } => {}
            // Objects and the cursor are handled by apply_ai_object_commands,
            // which reads the same AiCommand messages independently and owns
            // their acks.
            RattyAiCommand::SpawnObject { .. }
            | RattyAiCommand::RemoveObject { .. }
            | RattyAiCommand::ClearObjects
            | RattyAiCommand::UpdateObject { .. }
            | RattyAiCommand::UpdateCursor { .. } => {}
            // Data visualizations are handled by crate::viz::apply_viz_commands,
            // which reads the same AiCommand messages independently and owns
            // their acks.
            RattyAiCommand::VizSet { .. }
            | RattyAiCommand::VizEffect { .. }
            | RattyAiCommand::VizRemove { .. } => {}
            // The sound organ (crate::sound::apply_sound_commands) reads the
            // same AiCommand messages independently and owns the sound.*
            // acks — in every build: feature-off it rejects `unsupported`
            // itself, so this catch-all must never double-ack them.
            RattyAiCommand::SoundPlay { .. }
            | RattyAiCommand::SoundAmbientSet { .. }
            | RattyAiCommand::SoundAmbientStop { .. } => {}
            // View bookmarks are handled by crate::bookmarks, which reads
            // the same AiCommand messages independently and owns their
            // acks.
            RattyAiCommand::Bookmark { .. } | RattyAiCommand::BookmarkJump { .. } => {}
            // The macros organ (crate::macros::apply_macro_commands) reads
            // the same AiCommand messages independently: it owns the
            // macro.* acks and taps recordable commands off this stream.
            RattyAiCommand::MacroRecord { .. }
            | RattyAiCommand::MacroStop
            | RattyAiCommand::MacroPlay { .. }
            | RattyAiCommand::MacroExport { .. }
            | RattyAiCommand::MacroRun { .. } => {}
            // The reactive organ (crate::reactive::apply_reactive_commands)
            // reads the same AiCommand messages independently and owns the
            // rule.*/sensor.* acks.
            RattyAiCommand::RuleSet { .. }
            | RattyAiCommand::RuleRemove { .. }
            | RattyAiCommand::RuleEnable { .. }
            | RattyAiCommand::RuleDisable { .. }
            | RattyAiCommand::SensorPublish { .. }
            | RattyAiCommand::SensorRemove { .. } => {}
            // The avatar organ (crate::avatar::apply_avatar_commands) reads
            // the same AiCommand messages independently and owns the
            // avatar.* acks, so this catch-all must never double-ack them.
            RattyAiCommand::AvatarSet { .. }
            | RattyAiCommand::AvatarShow
            | RattyAiCommand::AvatarGesture { .. }
            | RattyAiCommand::AvatarSpeak { .. }
            | RattyAiCommand::AvatarStopSpeaking
            | RattyAiCommand::AvatarCancel { .. }
            | RattyAiCommand::AvatarSpeechClear { .. }
            | RattyAiCommand::AvatarHide => {}
            // The collaboration-presence organ
            // (crate::presence::apply_presence_commands) reads the same
            // AiCommand messages independently and owns the user.*/note
            // acks, so this catch-all must never double-ack them.
            RattyAiCommand::UserJoin { .. }
            | RattyAiCommand::UserRenew { .. }
            | RattyAiCommand::UserCursor { .. }
            | RattyAiCommand::UserLeave { .. }
            | RattyAiCommand::Note { .. }
            | RattyAiCommand::NoteRemove { .. } => {}
            other => {
                debug!("ratty-ai: command received, handler not yet built: {other:?}");
                reject(
                    &mut diagnostics,
                    &mut acks,
                    *source,
                    ack_token,
                    "command",
                    codes::UNSUPPORTED,
                    "command parsed but its subsystem is not built yet".to_string(),
                );
            }
        }
    }
}

/// Applies queued object and cursor commands, enforcing AI-range id
/// ownership per ingress source.
///
/// All failures are explicit `warn!`s (the query channel gives them a real
/// return path when it lands); nothing is silently dropped.
#[allow(clippy::too_many_arguments)]
pub fn apply_ai_object_commands(
    mut commands: MessageReader<AiCommand>,
    mut seats: Query<(
        &crate::identity::TerminalIdentity,
        &mut TerminalInlineObjects,
        &mut TerminalRedrawState,
    )>,
    mut registry: ResMut<AiObjectRegistry>,
    mut removals: MessageWriter<AiObjectRemoved>,
    mut cursor: ResMut<CursorSettings>,
    app_config: Res<AppConfig>,
    mut acks: MessageWriter<crate::query_channel::AckOutcome>,
    mut diagnostics: crate::query_channel::DiagnosticsSink,
) {
    use crate::query::codes;
    use crate::query_channel::ack_commit;

    for AiCommand {
        source,
        ack_token,
        command,
        ..
    } in commands.read()
    {
        // Every rejection below both warns (unchanged behavior) and lands
        // in the caller's `state.errors` ring via `reject`; `tok=` commands
        // additionally get their error ack.
        macro_rules! reject {
            ($action:literal, $code:expr, $($message:tt)+) => {
                crate::query_channel::reject(
                    &mut diagnostics,
                    &mut acks,
                    *source,
                    ack_token,
                    $action,
                    $code,
                    format!($($message)+),
                )
            };
        }
        // Object commands operate on the ARRIVAL terminal's inline
        // registry (arrival is the address); a command whose arrival
        // terminal died is dropped with a warn, never rerouted.
        macro_rules! arrival_seat {
            ($action:literal) => {
                match seats
                    .iter_mut()
                    .find(|(identity, ..)| identity.id() == source.terminal())
                {
                    Some((_, inline_objects, redraw)) => (inline_objects, redraw),
                    None => {
                        warn!(concat!(
                            "ratty-ai: ",
                            $action,
                            " dropped: its arrival terminal is gone"
                        ));
                        continue;
                    }
                }
            };
        }
        match command {
            RattyAiCommand::SpawnObject {
                id,
                path,
                x,
                y,
                scale,
                spin,
                brightness,
                replace,
            } => {
                let id = *id;
                let (mut inline_objects, mut redraw) = arrival_seat!("object.add");
                if ai_object_namespace(id) != Some(source.namespace()) {
                    warn!(
                        "ratty-ai: object.add rejected: id {id:#010x} is outside the caller's \
                         AI range/namespace ({})",
                        source.namespace()
                    );
                    reject!(
                        "object.add",
                        codes::NOT_OWNER,
                        "id {id:#010x} is outside the caller's AI range/namespace ({})",
                        source.namespace()
                    );
                    continue;
                }
                let live = inline_objects.contains_object(id);
                if live && !replace {
                    warn!(
                        "ratty-ai: object.add rejected: id {id:#010x} already exists \
                         (pass replace=true to replace it)"
                    );
                    reject!(
                        "object.add",
                        codes::ALREADY_EXISTS,
                        "id {id:#010x} already exists (pass replace=true to replace it)"
                    );
                    continue;
                }
                if !live {
                    if registry.used.contains(&id) {
                        warn!(
                            "ratty-ai: object.add rejected: id {id:#010x} was already used \
                             this session; ids are never reused"
                        );
                        reject!(
                            "object.add",
                            codes::ID_REUSED,
                            "id {id:#010x} was already used this session; ids are never reused"
                        );
                        continue;
                    }
                    if registry.used.len() >= MAX_AI_OBJECT_IDS_PER_SESSION {
                        warn!(
                            "ratty-ai: object.add rejected: the session id budget \
                             ({MAX_AI_OBJECT_IDS_PER_SESSION}) is exhausted"
                        );
                        reject!(
                            "object.add",
                            codes::SESSION_BUDGET,
                            "the session id budget ({MAX_AI_OBJECT_IDS_PER_SESSION}) is exhausted"
                        );
                        continue;
                    }
                    if inline_objects.ai_namespace_len(source.namespace())
                        >= MAX_AI_OBJECTS_PER_NAMESPACE
                    {
                        warn!(
                            "ratty-ai: object.add rejected: namespace {} is at its \
                             {MAX_AI_OBJECTS_PER_NAMESPACE}-object limit",
                            source.namespace()
                        );
                        reject!(
                            "object.add",
                            codes::NAMESPACE_CAP,
                            "namespace {} is at its {MAX_AI_OBJECTS_PER_NAMESPACE}-object limit",
                            source.namespace()
                        );
                        continue;
                    }
                }
                let object = match load_embedded_object_source(path, ObjectLoadOptions::default()) {
                    Ok((source_name, object_source)) => {
                        info!("ratty-ai: object.add {id:#010x} loaded {source_name}");
                        object_source.into()
                    }
                    Err(error) => {
                        warn!("ratty-ai: object.add rejected: {error:#}");
                        reject!("object.add", codes::BAD_ASSET, "{error:#}");
                        continue;
                    }
                };
                let style = InlineStyle {
                    animate: *spin != 0.0,
                    scale: *scale,
                    depth: 0.0,
                    color: None,
                    brightness: *brightness,
                    offset: Vec3::ZERO,
                    rotation: Vec3::ZERO,
                    scale3: Vec3::ONE,
                    spin: (*spin != 0.0).then_some(*spin),
                    bob: None,
                    bob_amplitude: None,
                    phase: 0.0,
                };
                registry.used.insert(id);
                inline_objects.ai_insert_object(id, object, *x, *y, style);
                redraw.request();
                ack_commit(&mut acks, *source, ack_token);
            }
            RattyAiCommand::UpdateObject {
                id,
                x,
                y,
                scale,
                spin,
                brightness,
            } => {
                let id = *id;
                let (mut inline_objects, mut redraw) = arrival_seat!("object.update");
                if ai_object_namespace(id) != Some(source.namespace()) {
                    warn!(
                        "ratty-ai: object.update rejected: id {id:#010x} is outside the \
                         caller's AI range/namespace ({})",
                        source.namespace()
                    );
                    reject!(
                        "object.update",
                        codes::NOT_OWNER,
                        "id {id:#010x} is outside the caller's AI range/namespace ({})",
                        source.namespace()
                    );
                    continue;
                }
                match inline_objects.ai_update_object(id, *x, *y, *scale, *spin, *brightness) {
                    AiUpdateOutcome::Applied => {
                        redraw.request();
                        ack_commit(&mut acks, *source, ack_token);
                    }
                    AiUpdateOutcome::UnknownId => {
                        warn!("ratty-ai: object.update rejected: no object with id {id:#010x}");
                        reject!(
                            "object.update",
                            codes::UNKNOWN_ID,
                            "no object with id {id:#010x}"
                        );
                    }
                    AiUpdateOutcome::NoAnchor => {
                        warn!(
                            "ratty-ai: object.update rejected: object {id:#010x} scrolled \
                             away; object.add with replace=true re-anchors it"
                        );
                        reject!(
                            "object.update",
                            codes::NO_ANCHOR,
                            "object {id:#010x} scrolled away; object.add with replace=true \
                             re-anchors it"
                        );
                    }
                }
            }
            RattyAiCommand::RemoveObject { id } => {
                let id = *id;
                let (mut inline_objects, mut redraw) = arrival_seat!("object.remove");
                if ai_object_namespace(id) != Some(source.namespace()) {
                    warn!(
                        "ratty-ai: object.remove rejected: id {id:#010x} is outside the \
                         caller's AI range/namespace ({})",
                        source.namespace()
                    );
                    reject!(
                        "object.remove",
                        codes::NOT_OWNER,
                        "id {id:#010x} is outside the caller's AI range/namespace ({})",
                        source.namespace()
                    );
                    continue;
                }
                if inline_objects.ai_remove_object(id) {
                    removals.write(AiObjectRemoved { id });
                    redraw.request();
                    ack_commit(&mut acks, *source, ack_token);
                } else {
                    warn!("ratty-ai: object.remove rejected: no object with id {id:#010x}");
                    reject!(
                        "object.remove",
                        codes::UNKNOWN_ID,
                        "no object with id {id:#010x}"
                    );
                }
            }
            RattyAiCommand::ClearObjects => {
                // Scoped to the caller's namespace and idempotent; full-scene
                // destruction is exclusively reset's job.
                let (mut inline_objects, mut redraw) = arrival_seat!("object.clear");
                let removed = inline_objects.ai_clear_namespace(source.namespace());
                if !removed.is_empty() {
                    for id in removed {
                        removals.write(AiObjectRemoved { id });
                    }
                    redraw.request();
                }
                ack_commit(&mut acks, *source, ack_token);
            }
            RattyAiCommand::UpdateCursor {
                model,
                spin,
                bob_speed,
                bob_amp,
                brightness,
                visible,
            } => {
                // Liveness first: the cursor settings are scene-global, and
                // a dead arrival's command must not half-apply them before
                // being reported dropped.
                if !seats
                    .iter()
                    .any(|(identity, ..)| identity.id() == source.terminal())
                {
                    warn!("ratty-ai: cursor dropped: its arrival terminal is gone");
                    continue;
                }
                // The command is atomic: a bad model name rejects the whole
                // update rather than partially applying the other fields,
                // so the ack's reject-or-commit is the truth. (Previously
                // the other fields still committed under a warn.)
                if let Some(name) = model
                    && !embedded_object_loadable(name)
                {
                    warn!(
                        "ratty-ai: cursor rejected: model '{name}' is not a loadable embedded \
                         asset (wire model swaps resolve embedded names only)"
                    );
                    reject!(
                        "cursor",
                        codes::BAD_ASSET,
                        "model '{name}' is not a loadable embedded asset (wire model swaps \
                         resolve embedded names only)"
                    );
                    continue;
                }
                if let Some(name) = model {
                    let choice = CursorModelChoice::Embedded(name.clone());
                    if cursor.model != choice {
                        cursor.model = choice;
                        cursor.needs_respawn = true;
                    }
                }
                if let Some(spin) = spin {
                    cursor.spin_speed = *spin;
                }
                if let Some(bob_speed) = bob_speed {
                    cursor.bob_speed = *bob_speed;
                }
                if let Some(bob_amp) = bob_amp {
                    cursor.bob_amplitude = *bob_amp;
                }
                if let Some(brightness) = brightness
                    && cursor.brightness != *brightness
                {
                    cursor.brightness = *brightness;
                    // Brightness is baked into cloned materials at spawn.
                    cursor.needs_respawn = true;
                }
                if let Some(visible) = visible {
                    cursor.visible = *visible;
                }
                // The settings are scene-global — `visible` flips the
                // texture-side block cursor on EVERY seat, so every seat
                // repaints, not just the arrival.
                for (_, _, mut redraw) in seats.iter_mut() {
                    redraw.request();
                }
                ack_commit(&mut acks, *source, ack_token);
            }
            RattyAiCommand::Reset => {
                // Full-scene destruction of AI-owned objects across every
                // namespace and every seat; used ids stay reserved (the
                // session continues). Reset's single ack belongs to
                // apply_ai_commands.
                for (_, mut inline_objects, mut redraw) in seats.iter_mut() {
                    let removed = inline_objects.ai_clear_all();
                    for id in removed {
                        removals.write(AiObjectRemoved { id });
                    }
                    redraw.request();
                }
                cursor.reset_to_config(&app_config);
            }
            _ => {}
        }
    }
}

/// Maps a CLI mode string to a presentation mode.
pub(crate) fn parse_mode(mode: &str) -> Option<TerminalPresentationMode> {
    match mode {
        "2d" | "flat" | "flat2d" => Some(TerminalPresentationMode::Flat2d),
        "3d" | "plane" | "plane3d" => Some(TerminalPresentationMode::Plane3d),
        "mobius" | "mobius3d" => Some(TerminalPresentationMode::Mobius3d),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::message::Messages;

    use crate::inline::{InlineObject, RgpInlineObject};

    #[test]
    fn mode_strings_map_to_presentation_modes() {
        assert_eq!(parse_mode("3d"), Some(TerminalPresentationMode::Plane3d));
        assert_eq!(parse_mode("2d"), Some(TerminalPresentationMode::Flat2d));
        assert_eq!(
            parse_mode("mobius"),
            Some(TerminalPresentationMode::Mobius3d)
        );
        assert_eq!(parse_mode("cube"), None);
    }

    /// Collects [`AiObjectRemoved`] messages so tests can assert on them.
    #[derive(Resource, Default)]
    struct RemovedLog(Vec<u32>);

    fn collect_removals(mut reader: MessageReader<AiObjectRemoved>, mut log: ResMut<RemovedLog>) {
        for message in reader.read() {
            log.0.push(message.id);
        }
    }

    fn test_app() -> App {
        let mut app = App::new();
        app.insert_resource(AppConfig::default());
        app.world_mut().spawn((
            TerminalInlineObjects::default(),
            crate::terminal::TerminalRedrawState::default(),
            crate::identity::TerminalIdentity::test_boot(),
            crate::identity::terminal_session_state(),
        ));
        app.init_resource::<AiObjectRegistry>();
        app.init_resource::<CursorSettings>();
        app.init_resource::<crate::viz::VizRegistry>();
        app.init_resource::<RemovedLog>();
        app.add_message::<AiCommand>();
        app.add_message::<AiObjectRemoved>();
        app.add_message::<crate::query_channel::AckOutcome>();
        app.add_systems(Update, (apply_ai_object_commands, collect_removals).chain());
        app
    }

    fn send(app: &mut App, command: RattyAiCommand) {
        app.world_mut()
            .resource_mut::<Messages<AiCommand>>()
            .write(AiCommand {
                source: IngressSource::test_boot(),
                ack_token: None,
                origin: CommandOrigin::Wire,
                command,
            });
        app.update();
    }

    fn spawn(app: &mut App, id: u32, replace: bool) {
        spawn_at(app, id, 10, replace);
    }

    fn spawn_at(app: &mut App, id: u32, x: u16, replace: bool) {
        send(
            app,
            RattyAiCommand::SpawnObject {
                id,
                path: "SkateMouse.stl".into(),
                x,
                y: 5,
                scale: 1.0,
                spin: 0.0,
                brightness: 1.0,
                replace,
            },
        );
    }

    fn contains(app: &mut App, id: u32) -> bool {
        let world = app.world_mut();
        let mut query = world.query::<&TerminalInlineObjects>();
        query
            .single(world)
            .expect("exactly one terminal seat")
            .contains_object(id)
    }

    fn anchor_col(app: &mut App, id: u32) -> u16 {
        let world = app.world_mut();
        let mut query = world.query::<&TerminalInlineObjects>();
        query
            .single(world)
            .expect("exactly one terminal seat")
            .anchors
            .get(&id)
            .expect("anchor exists")
            .col
    }

    fn gltf_object() -> InlineObject {
        InlineObject::RgpObject(RgpInlineObject::Gltf {
            asset_path: "objects/x.glb".into(),
            handle: None,
        })
    }

    const ID: u32 = 0x8000_0001;

    #[test]
    fn spawn_enforces_range_uniqueness_and_no_reuse() {
        let mut app = test_app();
        // Below the AI range: rejected.
        spawn(&mut app, 42, false);
        assert!(!contains(&mut app, 42));
        // In range and namespace 0: spawns.
        spawn(&mut app, ID, false);
        assert!(contains(&mut app, ID));
        // Wrong namespace for the local source: rejected.
        spawn(&mut app, 0x8100_0001, false);
        assert!(!contains(&mut app, 0x8100_0001));
        // Removal emits the per-object event; the id never comes back.
        send(&mut app, RattyAiCommand::RemoveObject { id: ID });
        assert!(!contains(&mut app, ID));
        assert_eq!(app.world().resource::<RemovedLog>().0, vec![ID]);
        spawn(&mut app, ID, false);
        assert!(
            !contains(&mut app, ID),
            "ids are never reused within a session"
        );
    }

    #[test]
    fn replace_requires_the_flag_and_emits_no_removal() {
        let mut app = test_app();
        spawn_at(&mut app, ID, 10, false);
        assert_eq!(
            anchor_col(&mut app, ID),
            4,
            "anchored at x=10 (col = x - 6)"
        );
        // A non-replace spawn on the live id is rejected — the object does
        // not move to the new position.
        spawn_at(&mut app, ID, 50, false);
        assert_eq!(
            anchor_col(&mut app, ID),
            4,
            "collision without replace is a no-op"
        );
        // With replace=true it re-anchors to the new position.
        spawn_at(&mut app, ID, 50, true);
        assert_eq!(anchor_col(&mut app, ID), 44, "replace re-anchors to x=50");
        assert!(contains(&mut app, ID));
        assert!(
            app.world().resource::<RemovedLog>().0.is_empty(),
            "replace keeps the id live; no removal event"
        );
    }

    #[test]
    fn unknown_asset_names_fail_the_spawn() {
        let mut app = test_app();
        send(
            &mut app,
            RattyAiCommand::SpawnObject {
                id: ID,
                path: "/etc/passwd".into(),
                x: 0,
                y: 0,
                scale: 1.0,
                spin: 0.0,
                brightness: 1.0,
                replace: false,
            },
        );
        assert!(
            !contains(&mut app, ID),
            "wire spawns resolve embedded names only"
        );
    }

    #[test]
    fn clear_is_namespace_scoped_and_reset_is_global() {
        let mut app = test_app();
        spawn(&mut app, ID, false);
        {
            let world = app.world_mut();
            let mut inline_query = world.query::<&mut TerminalInlineObjects>();
            let mut inline = inline_query
                .single_mut(world)
                .expect("exactly one terminal seat");
            // Another agent's object and a transmission-owned one.
            inline.ai_insert_object(0x8100_0001, gltf_object(), 0, 0, InlineStyle::default());
            inline.objects.insert(7, gltf_object());
        }
        send(&mut app, RattyAiCommand::ClearObjects);
        assert!(!contains(&mut app, ID));
        assert!(
            contains(&mut app, 0x8100_0001),
            "clear never crosses namespaces"
        );
        assert!(
            contains(&mut app, 7),
            "clear never touches transmission objects"
        );
        assert_eq!(app.world().resource::<RemovedLog>().0, vec![ID]);

        send(&mut app, RattyAiCommand::Reset);
        assert!(
            !contains(&mut app, 0x8100_0001),
            "reset destroys all AI objects"
        );
        assert!(contains(&mut app, 7), "reset spares transmission objects");
    }

    #[test]
    fn namespace_cap_rejects_spawns() {
        let mut app = test_app();
        {
            let world = app.world_mut();
            let mut inline_query = world.query::<&mut TerminalInlineObjects>();
            let mut inline = inline_query
                .single_mut(world)
                .expect("exactly one terminal seat");
            for i in 0..MAX_AI_OBJECTS_PER_NAMESPACE as u32 {
                inline.ai_insert_object(
                    0x8000_0100 + i,
                    gltf_object(),
                    0,
                    0,
                    InlineStyle::default(),
                );
            }
        }
        spawn(&mut app, ID, false);
        assert!(
            !contains(&mut app, ID),
            "the per-namespace object cap is enforced"
        );
    }

    #[test]
    fn cursor_updates_apply_and_reset_restores_config() {
        let mut app = test_app();
        // A bad model name rejects the whole command atomically: none of
        // the other fields apply (the ack's reject-or-commit is the truth).
        send(
            &mut app,
            RattyAiCommand::UpdateCursor {
                model: Some("not-embedded.obj".into()),
                spin: Some(9.0),
                bob_speed: None,
                bob_amp: None,
                brightness: Some(0.5),
                visible: Some(false),
            },
        );
        {
            let cursor = app.world().resource::<CursorSettings>();
            let config = AppConfig::default();
            assert_eq!(
                cursor.model,
                CursorModelChoice::Config,
                "unknown embedded names never swap the model"
            );
            assert_eq!(
                cursor.spin_speed, config.cursor.animation.spin_speed,
                "a rejected cursor command applies nothing"
            );
            assert_eq!(cursor.brightness, config.cursor.model.brightness);
            assert!(cursor.visible);
        }
        send(
            &mut app,
            RattyAiCommand::UpdateCursor {
                model: None,
                spin: Some(9.0),
                bob_speed: None,
                bob_amp: None,
                brightness: Some(0.5),
                visible: Some(false),
            },
        );
        {
            let cursor = app.world().resource::<CursorSettings>();
            assert_eq!(cursor.spin_speed, 9.0);
            assert_eq!(cursor.brightness, 0.5);
            assert!(cursor.needs_respawn, "brightness is baked; needs a rebuild");
            assert!(!cursor.visible);
        }
        send(
            &mut app,
            RattyAiCommand::UpdateCursor {
                model: Some("SkateMouse.stl".into()),
                spin: None,
                bob_speed: None,
                bob_amp: None,
                brightness: None,
                visible: None,
            },
        );
        assert_eq!(
            app.world().resource::<CursorSettings>().model,
            CursorModelChoice::Embedded("SkateMouse.stl".into())
        );

        send(&mut app, RattyAiCommand::Reset);
        let config = AppConfig::default();
        let cursor = app.world().resource::<CursorSettings>();
        assert_eq!(cursor.model, CursorModelChoice::Config);
        assert_eq!(cursor.visible, config.cursor.model.visible);
        assert_eq!(cursor.spin_speed, config.cursor.animation.spin_speed);
        assert!(cursor.needs_respawn, "reset rebuilds the swapped model");
    }
}
