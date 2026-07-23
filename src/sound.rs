//! The sound organ: the Bevy side of the `sound.*` OSC 777 family.
//!
//! Sound has a semantic basis, not a decorative one (see
//! [`crate::osc::SOUND_KINDS`]): one-shots mark state transitions and
//! coordination events; a single scene-owned ambient bed carries mood. The
//! wire *requests* playback by registered kind only — never paths or URLs —
//! and never owns the mixer: master gain and mute live in trusted config
//! (`[audio]`, [`crate::config::AudioConfig`]).
//!
//! This module is deliberately split in two layers:
//!
//! * [`apply_sound_commands`] — the **decision layer**, compiled in every
//!   build. It reads the shared [`AiCommand`] stream, decides the full
//!   accept/reject table (locked, rate limit, voice caps, capability,
//!   kind), mutates [`SoundState`], and owns ALL `sound.*` acks. Without
//!   the `sound` cargo feature it rejects every sound command with an
//!   honest `unsupported`.
//! * The playback layer (bevy_kira_audio) — behind the `sound` cargo
//!   feature. It consumes the committed state and is the only code that
//!   touches an audio device.
//!
//! Every ack is decided and fired once, the same frame the command lands;
//! there are no later events (`t=e` is reserved). Audio-unlock status and
//! the ambient slot are polled through `state.scene`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;

use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::input::ButtonState;
use bevy::input::keyboard::KeyboardInput;
use bevy::prelude::*;
use rust_embed::RustEmbed;

use crate::ai::AiCommand;
use crate::config::AppConfig;
use crate::osc::{RattyAiCommand, SoundKindClass};
use crate::query::codes;
use crate::query_channel::{AckOutcome, AiDiagnostics, ack_commit, ack_commit_qualified};

/// Global cap on simultaneously live one-shot voices: an honest failure
/// instead of unbounded mixer load driven by untrusted output.
pub const MAX_SOUND_VOICES: usize = 16;

/// Per-namespace cap on simultaneously live one-shot voices, so one agent
/// cannot exhaust the global voice budget for everyone else.
pub const MAX_SOUND_VOICES_PER_NAMESPACE: usize = 8;

/// One-shot rate-limit burst per namespace (the token-bucket capacity).
pub const SOUND_PLAY_BURST: u32 = 8;

/// Sustained one-shot plays per second per namespace (the token-bucket
/// refill rate), advertised in `caps().limits` as `sound_plays_per_sec`.
pub const SOUND_PLAYS_PER_SEC: u32 = 4;

/// Lower clamp on ambient crossfade/fade durations, in milliseconds.
pub const AMBIENT_XFADE_MIN_MS: u32 = 100;

/// Upper clamp on ambient crossfade/fade durations, in milliseconds.
pub const AMBIENT_XFADE_MAX_MS: u32 = 5000;

/// Default ambient crossfade/fade duration, in milliseconds.
pub const AMBIENT_XFADE_DEFAULT_MS: u32 = 1500;

/// One entry of the terminal-side sound registry: how a registered
/// semantic kind resolves to an embedded asset and its gain envelope.
#[derive(Debug, Clone, Copy)]
pub struct SoundSpec {
    /// The canonical registered kind name (matches [`crate::osc::SOUND_KINDS`]).
    pub kind: &'static str,
    /// The kind's class (one-shot or ambient).
    pub class: SoundKindClass,
    /// Embedded asset file name under `assets/sounds/` (never a path).
    pub file: &'static str,
    /// Gain used when the wire supplies none.
    pub default_gain: f32,
    /// Upper clamp on any wire-requested gain for this kind.
    pub max_gain: f32,
}

/// The terminal-side sound registry. The *names and classes* must stay in
/// lockstep with the shared wire list [`crate::osc::SOUND_KINDS`] (a test
/// pins this); files, default gains, and clamps are terminal-side detail
/// the wire never sees.
pub const SOUND_REGISTRY: &[SoundSpec] = &[
    SoundSpec {
        kind: "chime",
        class: SoundKindClass::OneShot,
        file: "chime.ogg",
        default_gain: 0.8,
        max_gain: 1.0,
    },
    SoundSpec {
        kind: "alert",
        class: SoundKindClass::OneShot,
        file: "alert.ogg",
        default_gain: 0.9,
        max_gain: 1.0,
    },
    SoundSpec {
        kind: "pulse",
        class: SoundKindClass::OneShot,
        file: "pulse.ogg",
        default_gain: 0.7,
        max_gain: 1.0,
    },
    SoundSpec {
        kind: "click",
        class: SoundKindClass::OneShot,
        file: "click.ogg",
        default_gain: 0.6,
        max_gain: 1.0,
    },
    SoundSpec {
        kind: "ambient.hum",
        class: SoundKindClass::Ambient,
        file: "ambient-hum.ogg",
        default_gain: 0.5,
        max_gain: 0.8,
    },
];

/// Looks up the registry entry for a kind, or `None` when unregistered.
pub fn sound_spec(kind: &str) -> Option<&'static SoundSpec> {
    SOUND_REGISTRY.iter().find(|spec| spec.kind == kind)
}

/// The embedded sound assets, the audio sibling of `EmbeddedObjects`
/// (model.rs). Budgets are enforced by build.rs: 192 KiB per asset,
/// 512 KiB for the set.
#[derive(RustEmbed)]
#[folder = "assets/sounds/"]
struct EmbeddedSounds;

/// Returns the embedded ogg bytes for a registry file name.
///
/// Only the bare `file_name()` is honored — path components (`..`,
/// absolute roots) cannot escape the registry, mirroring
/// [`crate::model::load_embedded_object_source`]. The wire never reaches
/// this directly: registered kinds resolve through [`SOUND_REGISTRY`]
/// first, and paths/URLs do not exist on the wire at all.
pub fn embedded_sound_bytes(name: &str) -> Option<Cow<'static, [u8]>> {
    let file_name = Path::new(name).file_name()?.to_str()?;
    EmbeddedSounds::get(file_name).map(|file| file.data)
}

/// A committed one-shot voice, counted against the voice caps from the
/// frame its `sound.play` commits until the playback layer observes the
/// instance end and removes it.
#[derive(Debug, Clone, Copy)]
pub struct SoundVoice {
    /// Monotonic voice id, unique for the session. The playback layer keys
    /// its kira instance bookkeeping on it so ends free the right slot.
    pub id: u64,
    /// The namespace that requested the play (for the per-namespace cap).
    pub namespace: u8,
    /// The registered kind (canonical registry string).
    pub kind: &'static str,
    /// Final per-play gain after the registry clamp, before master gain.
    pub gain: f32,
    /// Whether the playback layer has started the backing instance yet.
    pub started: bool,
}

/// Phase of the scene ambient bed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AmbientPhase {
    /// No bed is playing.
    #[default]
    Idle,
    /// The bed is playing steadily.
    Playing,
    /// The bed is fading in / crossfading to a new kind.
    Crossfading,
    /// The bed is fading out toward silence.
    FadingOut,
}

impl AmbientPhase {
    /// The wire projection tag for `state.scene`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Playing => "playing",
            Self::Crossfading => "crossfading",
            Self::FadingOut => "fading-out",
        }
    }
}

/// The desired ambient bed: a registered kind at a clamped gain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AmbientTrack {
    /// The registered ambient kind (canonical registry string).
    pub kind: &'static str,
    /// Bed gain after the registry clamp, before master gain.
    pub gain: f32,
}

/// The single scene-owned ambient slot. The decision layer writes it; the
/// playback layer reads it to drive the actual fades.
#[derive(Debug, Default)]
pub struct AmbientSlot {
    /// The bed the scene is playing (or fading out of).
    pub current: Option<AmbientTrack>,
    /// The slot's phase.
    pub phase: AmbientPhase,
    /// Duration of the running fade/crossfade, in milliseconds.
    pub xfade_ms: u32,
    /// The LATEST ambient request retained while audio is locked; it fades
    /// in after the first user gesture (a bed is stateful, not evental —
    /// a late start is honest, unlike a late one-shot).
    pub retained_pre_unlock: Option<AmbientTrack>,
}

/// Per-namespace one-shot token bucket ([`SOUND_PLAY_BURST`] capacity,
/// [`SOUND_PLAYS_PER_SEC`] refill).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PlayBucket {
    tokens: f64,
    last: f64,
}

impl PlayBucket {
    fn new(now: f64) -> Self {
        Self {
            tokens: f64::from(SOUND_PLAY_BURST),
            last: now,
        }
    }

    /// Refills by elapsed time, then takes one token if available.
    fn try_take(&mut self, now: f64) -> bool {
        let elapsed = (now - self.last).max(0.0);
        self.tokens = (self.tokens + elapsed * f64::from(SOUND_PLAYS_PER_SEC))
            .min(f64::from(SOUND_PLAY_BURST));
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// The sound organ's state, maintained by the always-compiled decision
/// layer and consumed by the feature-gated playback layer.
#[derive(Resource, Debug)]
pub struct SoundState {
    /// Whether the playback backend is compiled into this binary (the
    /// `sound` cargo feature). The decision layer rejects everything with
    /// `unsupported` when false, so acks stay honest.
    pub(crate) enabled: bool,
    /// Whether audio may audibly play. Native builds are born unlocked;
    /// wasm builds start locked under browser autoplay policy and unlock
    /// on the first user gesture (see [`SoundState::unlock`]).
    pub(crate) unlocked: bool,
    /// The single scene-owned ambient slot.
    pub(crate) ambient: AmbientSlot,
    /// Per-namespace one-shot rate-limit buckets. Bounded by construction:
    /// namespaces are 7-bit, so at most 128 entries ever exist.
    pub(crate) play_buckets: HashMap<u8, PlayBucket>,
    /// Live one-shot voices, bounded by [`MAX_SOUND_VOICES`]. The decision
    /// layer pushes committed plays; the playback layer starts them and
    /// removes them when their instances end.
    pub(crate) voices: Vec<SoundVoice>,
    /// The next [`SoundVoice::id`] to assign (monotonic, never reused).
    pub(crate) next_voice_id: u64,
}

impl Default for SoundState {
    fn default() -> Self {
        Self {
            enabled: cfg!(feature = "sound"),
            unlocked: !cfg!(target_arch = "wasm32"),
            ambient: AmbientSlot::default(),
            play_buckets: HashMap::new(),
            voices: Vec::new(),
            next_voice_id: 0,
        }
    }
}

/// The publicly observable slice of [`SoundState`], projected into the
/// `state.scene` `audio` key. Internal buckets and retained pre-unlock
/// state stay private; unlock status is polled state, never pushed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoundPublic {
    /// Whether the playback backend is compiled into this binary.
    pub enabled: bool,
    /// Whether audio is unlocked (native: from birth; wasm: post-gesture).
    pub unlocked: bool,
    /// The audible ambient bed's kind, when one is playing or fading.
    pub ambient_kind: Option<&'static str>,
    /// The ambient slot's phase tag.
    pub ambient_phase: &'static str,
    /// Live one-shot voice count.
    pub voices: usize,
}

impl SoundState {
    /// The publicly visible sound state for the query channel.
    pub fn public_state(&self) -> SoundPublic {
        SoundPublic {
            enabled: self.enabled,
            unlocked: self.unlocked,
            ambient_kind: self.ambient.current.map(|track| track.kind),
            ambient_phase: self.ambient.phase.as_str(),
            voices: self.voices.len(),
        }
    }

    /// Unlocks audio after a genuine user gesture (or at native startup —
    /// native constructs already-unlocked). Promotes the retained
    /// pre-unlock ambient request, if any, into a fade-in. Emits no event:
    /// clients observe the transition by polling `state.scene`.
    pub fn unlock(&mut self) {
        self.unlocked = true;
        if let Some(track) = self.ambient.retained_pre_unlock.take() {
            self.ambient.current = Some(track);
            self.ambient.phase = AmbientPhase::Crossfading;
            self.ambient.xfade_ms = AMBIENT_XFADE_DEFAULT_MS;
        }
    }

    /// The `ratty:reset` semantics: fade the ambient bed out, clear the
    /// retained pre-unlock request and the rate-limit buckets, and let
    /// in-flight one-shots finish. `unlocked` is a user-gesture fact, not
    /// scene state, so reset never re-locks audio.
    pub fn reset(&mut self) {
        if self.ambient.current.is_some() {
            self.ambient.phase = AmbientPhase::FadingOut;
            self.ambient.xfade_ms = AMBIENT_XFADE_DEFAULT_MS;
        }
        self.ambient.retained_pre_unlock = None;
        self.play_buckets.clear();
    }

    /// Takes one rate-limit token for `namespace` at time `now` (seconds).
    fn try_take_play_token(&mut self, namespace: u8, now: f64) -> bool {
        self.play_buckets
            .entry(namespace)
            .or_insert_with(|| PlayBucket::new(now))
            .try_take(now)
    }

    /// Live voices owned by `namespace`.
    fn namespace_voices(&self, namespace: u8) -> usize {
        self.voices
            .iter()
            .filter(|voice| voice.namespace == namespace)
            .count()
    }
}

/// Registers the sound organ's state and decision layer.
///
/// The decision layer is always compiled; the kira playback layer (behind
/// the `sound` cargo feature) is layered on top and only ever reads state
/// this plugin's applier committed.
pub struct SoundPlugin;

impl Plugin for SoundPlugin {
    fn build(&self, app: &mut App) {
        // Message registrations are idempotent; RattyAiPlugin also adds
        // these (and InputPlugin adds KeyboardInput), but registering here
        // keeps the plugin self-contained.
        app.add_message::<AiCommand>()
            .add_message::<AckOutcome>()
            .add_message::<KeyboardInput>()
            .init_resource::<SoundState>()
            .add_systems(
                Update,
                apply_sound_commands.after(crate::systems::pump_pty_output),
            )
            // A keystroke is a user gesture: unlock before this frame's
            // decisions so the gesture's own commands already play.
            .add_systems(
                Update,
                unlock_on_keyboard_input.before(apply_sound_commands),
            );
        #[cfg(feature = "sound")]
        {
            // The playback layer: kira behind the decision layer. Its
            // systems run after the applier so a committed play sounds the
            // same frame (bevy_kira_audio processes commands in PostUpdate).
            app.add_plugins(bevy_kira_audio::AudioPlugin)
                .init_resource::<playback::SoundAssets>()
                .init_resource::<playback::SoundPlayback>()
                .add_systems(Startup, playback::load_sound_assets)
                .add_systems(
                    Update,
                    (playback::drive_one_shot_voices, playback::drive_ambient_bed)
                        .chain()
                        .after(apply_sound_commands),
                );
        }
    }
}

/// Applies queued `sound.*` commands: the decision layer.
///
/// Reads the same [`AiCommand`] stream as the other appliers and owns the
/// ack for `sound.play`, `sound.ambient.set`, and `sound.ambient.stop`
/// (the one-owner invariant; `reset` is acked by `apply_ai_commands` and
/// handled silently here). Every outcome is decided and acked this frame —
/// there are no later events; clients poll `state.scene`.
pub fn apply_sound_commands(
    mut commands: MessageReader<AiCommand>,
    mut state: ResMut<SoundState>,
    app_config: Res<AppConfig>,
    time: Res<Time>,
    mut acks: MessageWriter<AckOutcome>,
    mut diagnostics: ResMut<AiDiagnostics>,
) {
    let now = time.elapsed_secs_f64();
    for AiCommand {
        source,
        ack_token,
        command,
    } in commands.read()
    {
        // Every rejection warns (matching the other appliers), lands in
        // the caller's `state.errors` ring, and — for tok= commands —
        // fires the matching error ack.
        macro_rules! reject {
            ($action:literal, $code:expr, $($message:tt)+) => {{
                let message = format!($($message)+);
                warn!("ratty-ai: {} rejected: {message}", $action);
                crate::query_channel::reject(
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
        // The honest feature-off path: the command parsed, but this binary
        // has no playback backend, so nothing will ever sound.
        macro_rules! require_enabled {
            ($action:literal) => {
                if !state.enabled {
                    reject!(
                        $action,
                        codes::UNSUPPORTED,
                        "the sound subsystem is not compiled into this binary \
                         (build with the `sound` feature)"
                    );
                    continue;
                }
            };
        }
        match command {
            RattyAiCommand::SoundPlay { kind, gain } => {
                require_enabled!("sound.play");
                let Some(spec) = sound_spec(kind) else {
                    reject!(
                        "sound.play",
                        codes::BAD_KIND,
                        "'{kind}' is not a registered sound kind"
                    );
                    continue;
                };
                if spec.class != SoundKindClass::OneShot {
                    reject!(
                        "sound.play",
                        codes::BAD_KIND,
                        "'{kind}' is an ambient bed; use sound.ambient.set"
                    );
                    continue;
                }
                if !state.unlocked {
                    // A one-shot is evental: played late it would lie about
                    // when the event happened, so it is dropped, honestly.
                    reject!(
                        "sound.play",
                        codes::AUDIO_LOCKED,
                        "one-shot dropped: audio locked"
                    );
                    continue;
                }
                let namespace = source.namespace();
                if !state.try_take_play_token(namespace, now) {
                    reject!(
                        "sound.play",
                        codes::RATE_LIMITED,
                        "namespace {namespace} exceeded {SOUND_PLAYS_PER_SEC} plays/s \
                         (burst {SOUND_PLAY_BURST})"
                    );
                    continue;
                }
                if state.voices.len() >= MAX_SOUND_VOICES {
                    reject!(
                        "sound.play",
                        codes::VOICE_CAP,
                        "the global {MAX_SOUND_VOICES}-voice cap is full"
                    );
                    continue;
                }
                if state.namespace_voices(namespace) >= MAX_SOUND_VOICES_PER_NAMESPACE {
                    reject!(
                        "sound.play",
                        codes::VOICE_CAP,
                        "namespace {namespace} is at its \
                         {MAX_SOUND_VOICES_PER_NAMESPACE}-voice limit"
                    );
                    continue;
                }
                // Server-side clamp: the wire requests, the registry rules.
                let gain = gain.unwrap_or(spec.default_gain).clamp(0.0, spec.max_gain);
                let id = state.next_voice_id;
                state.next_voice_id += 1;
                state.voices.push(SoundVoice {
                    id,
                    namespace,
                    kind: spec.kind,
                    gain,
                    started: false,
                });
                ack_commit(&mut acks, *source, ack_token);
            }
            RattyAiCommand::SoundAmbientSet { kind, gain, xfade } => {
                require_enabled!("sound.ambient.set");
                let Some(spec) = sound_spec(kind) else {
                    reject!(
                        "sound.ambient.set",
                        codes::BAD_KIND,
                        "'{kind}' is not a registered sound kind"
                    );
                    continue;
                };
                if spec.class != SoundKindClass::Ambient {
                    reject!(
                        "sound.ambient.set",
                        codes::BAD_KIND,
                        "'{kind}' is a one-shot; use sound.play"
                    );
                    continue;
                }
                // The scene-ambient capability comes from trusted config
                // only; the wire can never grant it to itself.
                if !app_config.audio.allow_scene_ambient {
                    reject!(
                        "sound.ambient.set",
                        codes::NOT_PERMITTED,
                        "scene ambient audio is disabled by config \
                         ([audio] allow_scene_ambient)"
                    );
                    continue;
                }
                let gain = gain.unwrap_or(spec.default_gain).clamp(0.0, spec.max_gain);
                if !state.unlocked {
                    // A bed is stateful, not evental: retain the LATEST
                    // request; it fades in after the first user gesture.
                    // Committed-but-pending, acked once, qualified — the
                    // fade-in itself is observable only via state.scene.
                    state.ambient.retained_pre_unlock = Some(AmbientTrack {
                        kind: spec.kind,
                        gain,
                    });
                    ack_commit_qualified(&mut acks, *source, ack_token, codes::DEFERRED);
                    continue;
                }
                let same_kind_live = state
                    .ambient
                    .current
                    .is_some_and(|track| track.kind == spec.kind)
                    && matches!(
                        state.ambient.phase,
                        AmbientPhase::Playing | AmbientPhase::Crossfading
                    );
                if !same_kind_live {
                    // Crossfade to the new bed (or fade in from silence /
                    // resurrect a fading-out bed).
                    state.ambient.current = Some(AmbientTrack {
                        kind: spec.kind,
                        gain,
                    });
                    state.ambient.phase = AmbientPhase::Crossfading;
                    state.ambient.xfade_ms = xfade
                        .unwrap_or(AMBIENT_XFADE_DEFAULT_MS)
                        .clamp(AMBIENT_XFADE_MIN_MS, AMBIENT_XFADE_MAX_MS);
                }
                // Same-kind set on a live bed is an idempotent commit (no
                // restart) so loop replays are clean.
                ack_commit(&mut acks, *source, ack_token);
            }
            RattyAiCommand::SoundAmbientStop { fade } => {
                require_enabled!("sound.ambient.stop");
                state.ambient.retained_pre_unlock = None;
                if state.ambient.current.is_some() && state.ambient.phase != AmbientPhase::FadingOut
                {
                    state.ambient.phase = AmbientPhase::FadingOut;
                    state.ambient.xfade_ms = fade
                        .unwrap_or(AMBIENT_XFADE_DEFAULT_MS)
                        .clamp(AMBIENT_XFADE_MIN_MS, AMBIENT_XFADE_MAX_MS);
                }
                // Stopping silence is an idempotent commit.
                ack_commit(&mut acks, *source, ack_token);
            }
            RattyAiCommand::Reset => {
                // Reset is handled by several systems; apply_ai_commands
                // owns its single ack, so the sound organ resets silently.
                state.reset();
            }
            _ => {}
        }
    }
}

/// Unlocks audio on the first genuine keystroke: a key press IS a user
/// gesture (the defensive unlock path for embedders that feed real
/// keyboard input; the browser page additionally forwards pointer
/// gestures through `RattySession::user_gesture`). Native builds are born
/// unlocked, so this is a no-op outside wasm. Ordered before
/// [`apply_sound_commands`] so the gesture frame's own `sound.*` commands
/// already see the unlocked state.
fn unlock_on_keyboard_input(mut keys: MessageReader<KeyboardInput>, mut state: ResMut<SoundState>) {
    if state.unlocked {
        return;
    }
    if keys.read().any(|event| event.state == ButtonState::Pressed) {
        state.unlock();
    }
}

/// The kira playback layer: the only code that touches an audio device.
///
/// It strictly *follows* [`SoundState`] — the decision layer commits
/// voices and ambient phases, and these systems realize them: starting
/// one-shot instances, freeing voice caps when instances end, and driving
/// the ambient bed's crossfade/fade-out tweens (advancing
/// [`AmbientPhase::Crossfading`] to `Playing` and `FadingOut` to `Idle`).
/// It never acks anything — every ack was fired at decision time.
///
/// Master gain and mute from `[audio]` config are applied here, at
/// playback time, so trusted config stays authoritative over the mixer.
/// The layer is headless-tolerant: without an audio device bevy_kira_audio
/// keeps a `None` backend and queues channel commands, instances never
/// materialize, and un-started voices stay counted — the voice caps then
/// bound the queue instead of a timeout lying about playback.
#[cfg(feature = "sound")]
mod playback {
    use std::collections::HashMap;
    use std::io::Cursor;
    use std::time::Duration;

    use bevy::prelude::*;
    use bevy_kira_audio::prelude::{
        Audio, AudioControl, AudioInstance, AudioSource, AudioTween, Decibels, PlaybackState,
        StaticSoundData,
    };

    use super::{AmbientPhase, SOUND_REGISTRY, SoundState, embedded_sound_bytes};
    use crate::config::{AppConfig, AudioConfig};

    /// Handles for the decoded embedded sound registry, keyed by kind.
    #[derive(Resource, Default)]
    pub struct SoundAssets {
        handles: HashMap<&'static str, Handle<AudioSource>>,
    }

    /// One live one-shot's kira bookkeeping.
    struct OneShotPlayback {
        /// The instance handle returned when the play was issued.
        handle: Handle<AudioInstance>,
        /// Whether the instance has been observed in [`Assets`] yet. The
        /// asset appears once the channel processes the play command and
        /// is cleaned up again after the instance stops; this flag tells
        /// "not started yet" apart from "ended and cleaned up".
        observed: bool,
    }

    /// The live ambient bed's kira bookkeeping.
    struct AmbientPlayback {
        /// The registered kind the instance loops.
        kind: &'static str,
        /// The looping instance's handle.
        handle: Handle<AudioInstance>,
        /// The volume (dB) the bed currently targets, for retune detection.
        target_db: f32,
        /// When (in [`Time`] seconds) the running fade tween completes.
        deadline: f64,
        /// Whether a stop tween has been issued for this instance.
        stopping: bool,
    }

    /// Playback-side bookkeeping, keyed by [`super::SoundVoice::id`].
    #[derive(Resource, Default)]
    pub struct SoundPlayback {
        one_shots: HashMap<u64, OneShotPlayback>,
        ambient: Option<AmbientPlayback>,
    }

    /// Converts a post-registry-clamp amplitude gain into kira decibels
    /// under the trusted config master: mute floors to silence, master
    /// gain scales multiplicatively. Applied at playback time only, so
    /// `[audio]` config stays authoritative over the mixer.
    pub(super) fn effective_db(gain: f32, audio: &AudioConfig) -> Decibels {
        if audio.muted {
            return Decibels::SILENCE;
        }
        let amplitude = (gain * audio.master_gain).clamp(0.0, 1.0);
        if amplitude <= 0.0 {
            return Decibels::SILENCE;
        }
        Decibels((20.0 * amplitude.log10()).max(Decibels::SILENCE.0))
    }

    /// Decodes every registry ogg out of the embedded set into kira sound
    /// data at startup. A failure is a build defect (the assets ship inside
    /// the binary): it is logged loudly and the kind stays unplayable.
    pub fn load_sound_assets(
        mut sources: ResMut<Assets<AudioSource>>,
        mut assets: ResMut<SoundAssets>,
    ) {
        for spec in SOUND_REGISTRY {
            let Some(bytes) = embedded_sound_bytes(spec.file) else {
                error!(
                    "sound: embedded asset '{}' for kind '{}' is missing",
                    spec.file, spec.kind
                );
                continue;
            };
            match StaticSoundData::from_cursor(Cursor::new(bytes.into_owned())) {
                Ok(sound) => {
                    let handle = sources.add(AudioSource { sound });
                    assets.handles.insert(spec.kind, handle);
                }
                Err(error) => error!(
                    "sound: embedded asset '{}' for kind '{}' failed to decode: {error}",
                    spec.file, spec.kind
                ),
            }
        }
    }

    /// Starts committed one-shot voices and frees their cap slots when
    /// their kira instances end.
    pub fn drive_one_shot_voices(
        mut state: ResMut<SoundState>,
        mut playback: ResMut<SoundPlayback>,
        assets: Res<SoundAssets>,
        audio: Res<Audio>,
        instances: Res<Assets<AudioInstance>>,
        config: Res<AppConfig>,
    ) {
        let mut ended: Vec<u64> = Vec::new();
        for voice in state.voices.iter_mut().filter(|voice| !voice.started) {
            voice.started = true;
            let Some(source) = assets.handles.get(voice.kind) else {
                // Decode failed at startup (already logged); free the slot
                // so a broken asset cannot pin the voice caps.
                ended.push(voice.id);
                continue;
            };
            let volume = effective_db(voice.gain, &config.audio);
            let handle = audio.play(source.clone()).with_volume(volume).handle();
            playback.one_shots.insert(
                voice.id,
                OneShotPlayback {
                    handle,
                    observed: false,
                },
            );
        }
        for (id, one_shot) in playback.one_shots.iter_mut() {
            match instances.get(&one_shot.handle) {
                Some(instance) => {
                    if matches!(instance.state(), PlaybackState::Stopped) {
                        ended.push(*id);
                    } else {
                        one_shot.observed = true;
                    }
                }
                // Missing asset: either still queued (never observed — leave
                // it) or ended and cleaned up in PreUpdate (observed — free
                // the voice).
                None => {
                    if one_shot.observed {
                        ended.push(*id);
                    }
                }
            }
        }
        if !ended.is_empty() {
            for id in &ended {
                playback.one_shots.remove(id);
            }
            state.voices.retain(|voice| !ended.contains(&voice.id));
        }
    }

    /// Retunes a live bed toward a new target volume (a config master or
    /// mute change) with a short tween; a no-op while the target holds.
    fn retune_bed(
        ambient: &mut AmbientPlayback,
        target: Decibels,
        instances: &mut Assets<AudioInstance>,
    ) {
        if (ambient.target_db - target.0).abs() < f32::EPSILON {
            return;
        }
        if let Some(mut instance) = instances.get_mut(&ambient.handle) {
            instance.set_decibels(target, AudioTween::default());
            ambient.target_db = target.0;
        }
    }

    /// Drives the scene ambient bed: starts and crossfades instances per
    /// the slot's phase, advances `Crossfading` to `Playing` and
    /// `FadingOut` to `Idle` when their tweens complete, and applies
    /// config master changes live.
    pub fn drive_ambient_bed(
        time: Res<Time>,
        mut state: ResMut<SoundState>,
        mut playback: ResMut<SoundPlayback>,
        assets: Res<SoundAssets>,
        audio: Res<Audio>,
        mut instances: ResMut<Assets<AudioInstance>>,
        config: Res<AppConfig>,
    ) {
        let now = time.elapsed_secs_f64();
        match state.ambient.phase {
            AmbientPhase::Idle => {
                // Defensive: a live instance under an idle slot should not
                // exist; silence it if it ever does.
                if let Some(orphan) = playback.ambient.take()
                    && let Some(mut instance) = instances.get_mut(&orphan.handle)
                {
                    instance.stop(AudioTween::default());
                }
            }
            AmbientPhase::Crossfading => {
                let Some(track) = state.ambient.current else {
                    // Defensive: crossfading toward nothing — clear.
                    state.ambient.phase = AmbientPhase::Idle;
                    return;
                };
                let xfade = Duration::from_millis(u64::from(state.ambient.xfade_ms));
                let target = effective_db(track.gain, &config.audio);
                let same_bed = playback
                    .ambient
                    .as_ref()
                    .is_some_and(|ambient| ambient.kind == track.kind && !ambient.stopping);
                if same_bed {
                    if let Some(ambient) = playback.ambient.as_mut() {
                        retune_bed(ambient, target, &mut instances);
                        if now >= ambient.deadline {
                            state.ambient.phase = AmbientPhase::Playing;
                        }
                    }
                    return;
                }
                // A new bed (or a resurrected same-kind bed whose stop
                // tween already started): fade the old instance out and a
                // fresh loop in — the two overlap as the crossfade.
                if let Some(old) = playback.ambient.take()
                    && let Some(mut instance) = instances.get_mut(&old.handle)
                {
                    instance.stop(AudioTween::linear(xfade));
                }
                let Some(source) = assets.handles.get(track.kind) else {
                    error!(
                        "sound: no decoded asset for ambient kind '{}'; clearing the bed",
                        track.kind
                    );
                    state.ambient.current = None;
                    state.ambient.phase = AmbientPhase::Idle;
                    return;
                };
                let handle = audio
                    .play(source.clone())
                    .looped()
                    .fade_in(AudioTween::linear(xfade))
                    .with_volume(target)
                    .handle();
                playback.ambient = Some(AmbientPlayback {
                    kind: track.kind,
                    handle,
                    target_db: target.0,
                    deadline: now + f64::from(state.ambient.xfade_ms) / 1000.0,
                    stopping: false,
                });
            }
            AmbientPhase::Playing => {
                let Some(track) = state.ambient.current else {
                    // Defensive: playing nothing — clear.
                    state.ambient.phase = AmbientPhase::Idle;
                    return;
                };
                let target = effective_db(track.gain, &config.audio);
                if let Some(ambient) = playback.ambient.as_mut() {
                    retune_bed(ambient, target, &mut instances);
                }
            }
            AmbientPhase::FadingOut => {
                let fade_ms = state.ambient.xfade_ms;
                match playback.ambient.as_mut() {
                    Some(ambient) => {
                        if !ambient.stopping {
                            if let Some(mut instance) = instances.get_mut(&ambient.handle) {
                                instance.stop(AudioTween::linear(Duration::from_millis(
                                    u64::from(fade_ms),
                                )));
                            }
                            ambient.stopping = true;
                            ambient.deadline = now + f64::from(fade_ms) / 1000.0;
                        }
                        if now >= ambient.deadline {
                            // Belt and braces: the tween should be done —
                            // make the stop unconditional before dropping
                            // the handle.
                            if let Some(mut instance) = instances.get_mut(&ambient.handle) {
                                instance.stop(AudioTween::default());
                            }
                            playback.ambient = None;
                            state.ambient.current = None;
                            state.ambient.phase = AmbientPhase::Idle;
                        }
                    }
                    None => {
                        // Nothing audible ever existed (asset failure or a
                        // backend-less run): the fade completes at once.
                        state.ambient.current = None;
                        state.ambient.phase = AmbientPhase::Idle;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::message::Messages;

    use crate::runtime::IngressSource;

    /// Collects [`AckOutcome`] messages so tests can assert on them.
    #[derive(Resource, Default)]
    struct AckLog(Vec<AckOutcome>);

    fn collect_acks(mut reader: MessageReader<AckOutcome>, mut log: ResMut<AckLog>) {
        for message in reader.read() {
            log.0.push(message.clone());
        }
    }

    fn test_app() -> App {
        test_app_with(AppConfig::default())
    }

    fn test_app_with(config: AppConfig) -> App {
        let mut app = App::new();
        app.insert_resource(config);
        app.init_resource::<SoundState>();
        app.init_resource::<AiDiagnostics>();
        app.init_resource::<Time>();
        app.init_resource::<AckLog>();
        app.add_message::<AiCommand>();
        app.add_message::<AckOutcome>();
        app.add_systems(Update, (apply_sound_commands, collect_acks).chain());
        // The decision layer is under test in every feature matrix; flip
        // the backend-present bit explicitly so `--no-default-features`
        // exercises the same table.
        app.world_mut().resource_mut::<SoundState>().enabled = true;
        app
    }

    fn send(app: &mut App, token: &str, command: RattyAiCommand) {
        app.world_mut()
            .resource_mut::<Messages<AiCommand>>()
            .write(AiCommand {
                source: IngressSource::Local,
                ack_token: Some(token.to_string()),
                command,
            });
        app.update();
    }

    fn play(app: &mut App, token: &str, kind: &str, gain: Option<f32>) {
        send(
            app,
            token,
            RattyAiCommand::SoundPlay {
                kind: kind.into(),
                gain,
            },
        );
    }

    fn last_ack(app: &App) -> AckOutcome {
        app.world()
            .resource::<AckLog>()
            .0
            .last()
            .expect("an ack was written")
            .clone()
    }

    fn state(app: &App) -> &SoundState {
        app.world().resource::<SoundState>()
    }

    #[test]
    fn default_state_reports_the_compiled_feature_honestly() {
        let state = SoundState::default();
        assert_eq!(state.enabled, cfg!(feature = "sound"));
        assert!(state.unlocked, "native builds are born unlocked");
        assert_eq!(state.public_state().ambient_phase, "idle");
    }

    #[test]
    fn registry_matches_the_shared_kind_list() {
        assert_eq!(SOUND_REGISTRY.len(), crate::osc::SOUND_KINDS.len());
        for (kind, class) in crate::osc::SOUND_KINDS {
            let spec = sound_spec(kind).expect("every shared kind has a registry entry");
            assert_eq!(spec.class, *class, "class agrees for '{kind}'");
            assert!(spec.file.ends_with(".ogg"), "'{kind}' resolves to an ogg");
            assert!(
                spec.default_gain <= spec.max_gain && spec.max_gain <= 1.0,
                "'{kind}' gains are sane"
            );
        }
    }

    #[test]
    fn unknown_and_wrong_class_kinds_reject_bad_kind() {
        let mut app = test_app();
        play(&mut app, "t1", "kazoo", None);
        let ack = last_ack(&app);
        assert!(!ack.ok);
        assert_eq!(ack.code, Some(codes::BAD_KIND));

        // An ambient kind through sound.play is also bad-kind.
        play(&mut app, "t2", "ambient.hum", None);
        assert_eq!(last_ack(&app).code, Some(codes::BAD_KIND));

        // A one-shot kind through ambient.set: same, mirrored.
        send(
            &mut app,
            "t3",
            RattyAiCommand::SoundAmbientSet {
                kind: "click".into(),
                gain: None,
                xfade: None,
            },
        );
        assert_eq!(last_ack(&app).code, Some(codes::BAD_KIND));
        assert!(state(&app).voices.is_empty());
    }

    #[test]
    fn locked_one_shots_drop_with_audio_locked() {
        let mut app = test_app();
        app.world_mut().resource_mut::<SoundState>().unlocked = false;
        play(&mut app, "t1", "chime", None);
        let ack = last_ack(&app);
        assert!(!ack.ok, "the chime did not and will not play");
        assert_eq!(ack.code, Some(codes::AUDIO_LOCKED));
        assert!(state(&app).voices.is_empty());
    }

    #[test]
    fn committed_plays_clamp_gain_to_the_registry_max() {
        let mut app = test_app();
        play(&mut app, "t1", "chime", Some(5.0));
        let ack = last_ack(&app);
        assert!(ack.ok);
        assert_eq!(ack.code, None);
        let voices = &state(&app).voices;
        assert_eq!(voices.len(), 1);
        assert_eq!(voices[0].kind, "chime");
        assert_eq!(voices[0].gain, 1.0, "gain clamps to the registry max");
        assert!(!voices[0].started, "playback starts it later");
    }

    #[test]
    fn the_play_burst_rate_limits_within_one_frame() {
        let mut app = test_app();
        for index in 0..SOUND_PLAY_BURST {
            play(&mut app, &format!("t{index}"), "click", None);
            let ack = last_ack(&app);
            // The 8-voice namespace cap and the 8-token burst coincide;
            // every play up to the burst commits.
            assert!(ack.ok, "play {index} within the burst commits");
        }
        play(&mut app, "t-final", "click", None);
        let ack = last_ack(&app);
        assert!(!ack.ok);
        assert_eq!(
            ack.code,
            Some(codes::RATE_LIMITED),
            "the bucket empties before the voice caps are consulted"
        );
    }

    #[test]
    fn play_buckets_refill_over_time() {
        let mut bucket = PlayBucket::new(0.0);
        for _ in 0..SOUND_PLAY_BURST {
            assert!(bucket.try_take(0.0));
        }
        assert!(!bucket.try_take(0.0), "the burst is spent");
        assert!(!bucket.try_take(0.1), "0.1s refills less than one token");
        assert!(bucket.try_take(0.5), "0.5s refills two tokens at 4/s");
        assert!(bucket.try_take(0.5));
        assert!(!bucket.try_take(0.5));
    }

    #[test]
    fn voice_caps_reject_before_the_mixer_overloads() {
        let mut app = test_app();
        // Fill the caller's namespace to its cap without spending rate
        // tokens (voices seeded directly; the caps are the subject here).
        {
            let mut state = app.world_mut().resource_mut::<SoundState>();
            for index in 0..MAX_SOUND_VOICES_PER_NAMESPACE {
                state.voices.push(SoundVoice {
                    id: index as u64,
                    namespace: 0,
                    kind: "click",
                    gain: 0.5,
                    started: true,
                });
            }
        }
        play(&mut app, "t1", "chime", None);
        let ack = last_ack(&app);
        assert!(!ack.ok);
        assert_eq!(ack.code, Some(codes::VOICE_CAP), "per-namespace cap");

        // Fill the global cap with foreign namespaces: same code.
        {
            let mut state = app.world_mut().resource_mut::<SoundState>();
            state.voices.clear();
            for index in 0..MAX_SOUND_VOICES {
                state.voices.push(SoundVoice {
                    id: 100 + index as u64,
                    namespace: 1 + (index % 2) as u8,
                    kind: "click",
                    gain: 0.5,
                    started: true,
                });
            }
        }
        play(&mut app, "t2", "chime", None);
        let ack = last_ack(&app);
        assert!(!ack.ok);
        assert_eq!(ack.code, Some(codes::VOICE_CAP), "global cap");
    }

    #[test]
    fn ambient_set_defers_while_locked_and_retains_latest_only() {
        let mut app = test_app();
        app.world_mut().resource_mut::<SoundState>().unlocked = false;
        send(
            &mut app,
            "t1",
            RattyAiCommand::SoundAmbientSet {
                kind: "ambient.hum".into(),
                gain: Some(0.9),
                xfade: None,
            },
        );
        let ack = last_ack(&app);
        assert!(ack.ok, "deferred is a committed (qualified) success");
        assert_eq!(ack.code, Some(codes::DEFERRED));
        {
            let state = state(&app);
            assert_eq!(
                state.ambient.retained_pre_unlock,
                Some(AmbientTrack {
                    kind: "ambient.hum",
                    gain: 0.8,
                }),
                "the retained gain is registry-clamped"
            );
            assert!(state.ambient.current.is_none(), "nothing plays yet");
        }
        // Unlock promotes the retained request into a fade-in.
        app.world_mut().resource_mut::<SoundState>().unlock();
        let state = state(&app);
        assert_eq!(
            state.ambient.current.map(|track| track.kind),
            Some("ambient.hum")
        );
        assert_eq!(state.ambient.phase, AmbientPhase::Crossfading);
        assert!(state.ambient.retained_pre_unlock.is_none());
    }

    #[test]
    fn first_key_press_unlocks_and_promotes_the_retained_ambient() {
        use bevy::input::keyboard::Key;

        fn key_event(state: ButtonState) -> KeyboardInput {
            KeyboardInput {
                key_code: KeyCode::Enter,
                logical_key: Key::Enter,
                state,
                text: None,
                repeat: false,
                window: Entity::PLACEHOLDER,
            }
        }

        let mut app = App::new();
        app.add_message::<KeyboardInput>();
        app.init_resource::<SoundState>();
        app.add_systems(Update, unlock_on_keyboard_input);
        {
            let mut state = app.world_mut().resource_mut::<SoundState>();
            state.unlocked = false;
            state.ambient.retained_pre_unlock = Some(AmbientTrack {
                kind: "ambient.hum",
                gain: 0.5,
            });
        }
        // A key release alone is not a gesture.
        app.world_mut()
            .resource_mut::<Messages<KeyboardInput>>()
            .write(key_event(ButtonState::Released));
        app.update();
        assert!(!app.world().resource::<SoundState>().unlocked);
        // The first press unlocks and fades the retained bed in.
        app.world_mut()
            .resource_mut::<Messages<KeyboardInput>>()
            .write(key_event(ButtonState::Pressed));
        app.update();
        let state = app.world().resource::<SoundState>();
        assert!(state.unlocked, "a key press is a user gesture");
        assert_eq!(
            state.ambient.current.map(|track| track.kind),
            Some("ambient.hum")
        );
        assert_eq!(state.ambient.phase, AmbientPhase::Crossfading);
        assert!(state.ambient.retained_pre_unlock.is_none());
    }

    #[test]
    fn ambient_without_the_capability_rejects_not_permitted() {
        let config = AppConfig {
            audio: crate::config::AudioConfig {
                allow_scene_ambient: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut app = test_app_with(config);
        send(
            &mut app,
            "t1",
            RattyAiCommand::SoundAmbientSet {
                kind: "ambient.hum".into(),
                gain: None,
                xfade: None,
            },
        );
        let ack = last_ack(&app);
        assert!(!ack.ok);
        assert_eq!(ack.code, Some(codes::NOT_PERMITTED));
    }

    #[test]
    fn same_kind_ambient_set_is_an_idempotent_no_restart() {
        let mut app = test_app();
        send(
            &mut app,
            "t1",
            RattyAiCommand::SoundAmbientSet {
                kind: "ambient.hum".into(),
                gain: None,
                xfade: Some(50),
            },
        );
        {
            let state = state(&app);
            assert_eq!(state.ambient.phase, AmbientPhase::Crossfading);
            assert_eq!(
                state.ambient.xfade_ms, AMBIENT_XFADE_MIN_MS,
                "xfade clamps up to the minimum"
            );
        }
        // The same kind again: ok, no restart, xfade untouched.
        send(
            &mut app,
            "t2",
            RattyAiCommand::SoundAmbientSet {
                kind: "ambient.hum".into(),
                gain: None,
                xfade: Some(99_999),
            },
        );
        let ack = last_ack(&app);
        assert!(ack.ok);
        assert_eq!(ack.code, None, "a live same-kind set is a plain commit");
        assert_eq!(state(&app).ambient.xfade_ms, AMBIENT_XFADE_MIN_MS);
    }

    #[test]
    fn ambient_stop_is_idempotent_and_resurrectable() {
        let mut app = test_app();
        // Stopping silence commits.
        send(
            &mut app,
            "t1",
            RattyAiCommand::SoundAmbientStop { fade: None },
        );
        assert!(last_ack(&app).ok);
        assert_eq!(state(&app).ambient.phase, AmbientPhase::Idle);

        send(
            &mut app,
            "t2",
            RattyAiCommand::SoundAmbientSet {
                kind: "ambient.hum".into(),
                gain: None,
                xfade: None,
            },
        );
        send(
            &mut app,
            "t3",
            RattyAiCommand::SoundAmbientStop { fade: Some(99_999) },
        );
        {
            let state = state(&app);
            assert_eq!(state.ambient.phase, AmbientPhase::FadingOut);
            assert_eq!(
                state.ambient.xfade_ms, AMBIENT_XFADE_MAX_MS,
                "fade clamps down to the maximum"
            );
        }
        // A same-kind set on a fading-out bed resurrects it (loop replays).
        send(
            &mut app,
            "t4",
            RattyAiCommand::SoundAmbientSet {
                kind: "ambient.hum".into(),
                gain: None,
                xfade: None,
            },
        );
        assert_eq!(state(&app).ambient.phase, AmbientPhase::Crossfading);
    }

    #[test]
    fn reset_is_silent_and_clears_retained_state_and_buckets() {
        let mut app = test_app();
        // An unlocked play seeds a rate bucket and a voice.
        play(&mut app, "t0", "chime", None);
        assert!(!state(&app).play_buckets.is_empty());
        // Then lock (the wasm shape) and retain an ambient request.
        app.world_mut().resource_mut::<SoundState>().unlocked = false;
        send(
            &mut app,
            "t1",
            RattyAiCommand::SoundAmbientSet {
                kind: "ambient.hum".into(),
                gain: None,
                xfade: None,
            },
        );
        let acks_before = app.world().resource::<AckLog>().0.len();

        // Reset arrives with a token — the sound organ still stays silent
        // (apply_ai_commands owns reset's single ack).
        send(&mut app, "treset", RattyAiCommand::Reset);
        assert_eq!(
            app.world().resource::<AckLog>().0.len(),
            acks_before,
            "reset writes no sound ack"
        );
        let state = state(&app);
        assert!(state.ambient.retained_pre_unlock.is_none());
        assert!(state.play_buckets.is_empty(), "buckets clear on reset");
        assert_eq!(state.voices.len(), 1, "in-flight one-shots finish");
        assert!(!state.unlocked, "reset never re-locks or unlocks audio");
    }

    #[test]
    fn disabled_builds_reject_unsupported() {
        let mut app = test_app();
        app.world_mut().resource_mut::<SoundState>().enabled = false;
        play(&mut app, "t1", "chime", None);
        let ack = last_ack(&app);
        assert!(!ack.ok);
        assert_eq!(ack.code, Some(codes::UNSUPPORTED));
        send(
            &mut app,
            "t2",
            RattyAiCommand::SoundAmbientSet {
                kind: "ambient.hum".into(),
                gain: None,
                xfade: None,
            },
        );
        assert_eq!(last_ack(&app).code, Some(codes::UNSUPPORTED));
        send(
            &mut app,
            "t3",
            RattyAiCommand::SoundAmbientStop { fade: None },
        );
        assert_eq!(last_ack(&app).code, Some(codes::UNSUPPORTED));
    }

    #[test]
    fn voice_ids_are_monotonic_and_never_reused() {
        let mut app = test_app();
        play(&mut app, "t1", "chime", None);
        play(&mut app, "t2", "click", None);
        let voices = &state(&app).voices;
        assert_eq!(voices[0].id, 0);
        assert_eq!(voices[1].id, 1);
        assert_eq!(state(&app).next_voice_id, 2);
    }

    #[test]
    fn every_registry_kind_resolves_to_embedded_bytes() {
        for spec in SOUND_REGISTRY {
            let bytes = embedded_sound_bytes(spec.file)
                .unwrap_or_else(|| panic!("'{}' is embedded", spec.file));
            assert!(!bytes.is_empty(), "'{}' has content", spec.file);
            assert!(
                bytes.starts_with(b"OggS"),
                "'{}' is an ogg container",
                spec.file
            );
        }
    }

    #[test]
    fn embedded_sound_resolution_honors_the_file_name_only() {
        let direct = embedded_sound_bytes("chime.ogg").expect("direct name resolves");
        let traversal = embedded_sound_bytes("../../secrets/chime.ogg").expect("file_name() only");
        assert_eq!(direct, traversal, "path components are ignored");
        assert!(embedded_sound_bytes("kazoo.ogg").is_none());
        assert!(embedded_sound_bytes("").is_none());
        assert!(embedded_sound_bytes("..").is_none());
    }

    #[test]
    fn embedded_sounds_match_the_registry_and_stay_within_budgets() {
        // The same budgets build.rs enforces, re-checked here so the
        // no-default-features matrix guards them too.
        const PER_ASSET_BUDGET: usize = 192 * 1024;
        const PACKAGE_BUDGET: usize = 512 * 1024;
        let mut embedded: Vec<String> = EmbeddedSounds::iter().map(|f| f.to_string()).collect();
        embedded.sort();
        let mut registry: Vec<String> = SOUND_REGISTRY.iter().map(|s| s.file.to_string()).collect();
        registry.sort();
        assert_eq!(
            embedded, registry,
            "the embedded set and the registry name the same files"
        );
        let mut total = 0;
        for spec in SOUND_REGISTRY {
            let len = embedded_sound_bytes(spec.file)
                .map(|bytes| bytes.len())
                .unwrap_or_default();
            assert!(
                len <= PER_ASSET_BUDGET,
                "'{}' is {len} bytes, within the per-asset budget",
                spec.file
            );
            total += len;
        }
        assert!(total <= PACKAGE_BUDGET, "{total} bytes within the package");
    }

    #[cfg(feature = "sound")]
    #[test]
    fn every_registry_ogg_decodes_for_kira() {
        use bevy_kira_audio::prelude::StaticSoundData;

        for spec in SOUND_REGISTRY {
            let bytes = embedded_sound_bytes(spec.file).expect("embedded");
            let sound = StaticSoundData::from_cursor(std::io::Cursor::new(bytes.into_owned()))
                .unwrap_or_else(|error| panic!("'{}' decodes: {error}", spec.file));
            let seconds = sound.duration().as_secs_f64();
            match spec.class {
                SoundKindClass::OneShot => assert!(
                    (0.03..1.0).contains(&seconds),
                    "one-shot '{}' is {seconds}s",
                    spec.kind
                ),
                SoundKindClass::Ambient => assert!(
                    (4.0..=6.5).contains(&seconds),
                    "ambient '{}' is {seconds}s",
                    spec.kind
                ),
            }
        }
    }

    #[cfg(feature = "sound")]
    #[test]
    fn effective_db_honors_master_gain_and_mute() {
        use bevy_kira_audio::prelude::Decibels;

        use super::playback::effective_db;
        use crate::config::AudioConfig;

        let audio = AudioConfig {
            master_gain: 1.0,
            ..Default::default()
        };
        assert!(
            effective_db(1.0, &audio).0.abs() < 1e-4,
            "full gain under a unity master is 0 dB"
        );
        let attenuated = effective_db(0.5, &audio).0;
        assert!(
            (attenuated - -6.02).abs() < 0.01,
            "half amplitude is about -6 dB, got {attenuated}"
        );

        let default_master = AudioConfig::default();
        assert!(
            effective_db(1.0, &default_master).0 < 0.0,
            "the default 0.8 master attenuates"
        );

        let muted = AudioConfig {
            muted: true,
            ..Default::default()
        };
        assert_eq!(effective_db(1.0, &muted), Decibels::SILENCE);
        assert_eq!(effective_db(0.0, &default_master), Decibels::SILENCE);
    }
}
