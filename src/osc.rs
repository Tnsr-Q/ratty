//! OSC 777 control channel: the `ratty-ai` command protocol.
//!
//! Where the RGP module (APC) carries the 3D *graphics payload*, OSC 777
//! carries *control and telemetry* — mode/warp/effects/presence/operator
//! commands an external agent drives through the `ratty-ai` CLI. The wire
//! form is a single OSC sequence:
//!
//! ```text
//! ESC ] 777 ; ratty:<action> ; <payload> BEL
//! ```
//!
//! `<payload>` is `k=v&k=v…`, each value percent-encoded, plus an optional
//! leading bare (`=`-less) positional token. The terminal reaches this
//! through vt100's `Callbacks::unhandled_osc`, which delivers the
//! `;`-split params with `params[0] == b"777"`.
//!
//! Any command may opt into a delivery ack by adding a `tok=<token>`
//! payload key: the terminal replies over OSC 778 (`t=r;kind=ack`) once
//! the command is rejected or its immediate state mutation commits. (The
//! query-channel design named this key `id=`, but `id=` is already the
//! object id on every `object.*` command, so the ack key is `tok=`.)
//! Commands without `tok=` stay fire-and-forget.
//!
//! This module is dependency-free (std only) so the `ratty-ai` CLI can
//! include it verbatim, the same way `tools/silk` includes `rgp.rs` — the
//! parser can then never drift from the terminal's real behavior.

/// OSC numeric code claimed by the `ratty-ai` control channel.
pub const RATTY_AI_OSC: &[u8] = b"777";
/// Namespace prefix distinguishing ratty commands from other OSC 777 users.
pub const RATTY_AI_PREFIX: &str = "ratty:";
/// Reserved payload key carrying the optional ack correlation token.
pub const ACK_TOKEN_KEY: &str = "tok";

/// First id in the AI-owned object range (high bit set).
///
/// The object id space is partitioned: ids below this value belong to
/// transmissions and system surfaces (RGP registrations, Kitty images); ids
/// at or above it are owned by the AI channel and may only be created,
/// updated, or removed through `object.*` commands.
pub const AI_OBJECT_ID_MIN: u32 = 0x8000_0000;

/// Bits of an AI-range id carrying the per-namespace object index. The seven
/// bits between the high bit and the index hold the agent namespace.
pub const AI_OBJECT_INDEX_BITS: u32 = 24;

/// Extracts the agent namespace from an object id, or `None` when the id is
/// outside the AI-owned range.
pub fn ai_object_namespace(id: u32) -> Option<u8> {
    (id >= AI_OBJECT_ID_MIN).then_some(((id >> AI_OBJECT_INDEX_BITS) & 0x7F) as u8)
}

// ── Viz wire limits ──
//
// These bounds are part of the `viz.*` wire contract, so they live in this
// shared std-only module: the terminal enforces them at decode and the
// `ratty-ai` collectors normalize under them at encode — compiling the same
// constants on both ends means the two can never drift.

/// Upper bound on one *decoded* `viz.set` payload, in bytes. The terminal
/// enforces it before allocating (bounding the memory a hostile sequence
/// can pin); collectors bound their `--top` output so a worst-case snapshot
/// provably encodes under it. The terminal side additionally const-asserts
/// that the 4/3 base64url expansion plus envelope survives its OSC
/// watchdog.
pub const MAX_VIZ_PAYLOAD_BYTES: usize = 32 * 1024;

/// Upper bound on the items in one snapshot (`ps`/`fs`/`net` items, `git`
/// branches). Bounds per-entry render work and memory against a hostile
/// emitter packing the byte budget with tiny items.
pub const MAX_VIZ_ITEMS_PER_SNAPSHOT: usize = 256;

/// Upper bound, in bytes, on any single label string inside a payload
/// (names, paths, states, capture provenance) and on `viz.effect` keys.
/// Bounds stored-string memory and keeps every projection record small
/// enough for size-bounded reply pages.
pub const MAX_VIZ_LABEL_BYTES: usize = 128;

/// Upper bound on the series in a `chart.line.v1` snapshot and the lanes in
/// a `timeline.v1` snapshot. More than a handful of series is unreadable in
/// a cell-anchored footprint anyway; the bound keeps a hostile emitter from
/// packing the byte budget with empty track groups.
pub const MAX_VIZ_SERIES_PER_SNAPSHOT: usize = 8;

/// Upper bound on the points in one `chart.line.v1` series.
pub const MAX_VIZ_POINTS_PER_SERIES: usize = 256;

/// Upper bound on the points across every series of one `chart.line.v1`
/// snapshot. Sized so a max-point payload of short labels still encodes
/// under [`MAX_VIZ_PAYLOAD_BYTES`]; the byte cap remains the hard wall.
pub const MAX_VIZ_POINTS_PER_SNAPSHOT: usize = 1024;
/// Classification of a registered semantic sound kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundKindClass {
    /// A short event sound played once via `sound.play`.
    OneShot,
    /// A looping scene bed driven via `sound.ambient.set`.
    Ambient,
}

/// The registered semantic sound kinds and their classes.
///
/// Sound has a semantic basis, not a decorative one: `chime` marks a task
/// or transition completing, `alert` demands attention (an error), `pulse`
/// is a heartbeat/progress tick, `click` is an acknowledgment, and
/// `ambient.hum` is the scene bed. The wire only ever names these kinds —
/// never paths or URLs; the terminal resolves kinds against its embedded
/// registry. The list lives in this shared module so authoring tools
/// (`tools/silk`, `ratty-ai`) validate kinds without the audio feature.
pub const SOUND_KINDS: &[(&str, SoundKindClass)] = &[
    ("chime", SoundKindClass::OneShot),
    ("alert", SoundKindClass::OneShot),
    ("pulse", SoundKindClass::OneShot),
    ("click", SoundKindClass::OneShot),
    ("ambient.hum", SoundKindClass::Ambient),
];

/// The shared wire vocabulary of avatar model registry ids (#23 §3).
///
/// Immutable and append-only: an id, once shipped, never changes meaning
/// or disappears. Wire commands reference these ids only — never file
/// names, paths, or URLs (#12). Which embedded asset backs an id is
/// terminal-side detail the wire never sees (see
/// `crate::avatar::AVATAR_REGISTRY`; a test pins the lockstep). The list
/// lives in this shared module so `ratty-ai` validates ids without the
/// terminal.
pub const AVATAR_MODELS: &[&str] = &["mascot"];

/// The scope of an `avatar.speech.clear` (#23 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechClearScope {
    /// The caller's own pending utterance (no `ns=`/`scope=`).
    Own,
    /// One namespace's pending utterance (`ns=<n>`). Equal to the
    /// caller's own namespace this is ownership-scoped like `Own`; any
    /// other namespace is scene-global. Classification is conservative:
    /// `ns=` classifies privileged even for the recorder's own namespace,
    /// because the caller is unknown at classification time — a macro
    /// author who wants an unprivileged recording uses the bare form.
    Namespace(u8),
    /// The whole queue plus the active utterance (`scope=all`).
    All,
}

/// Looks up the class of a registered sound kind, or `None` when the kind
/// is not registered.
pub fn sound_kind_class(kind: &str) -> Option<SoundKindClass> {
    SOUND_KINDS
        .iter()
        .find(|(name, _)| *name == kind)
        .map(|(_, class)| *class)
}

/// Which macro registry a `macro.play` resolves against (#16 decision 4).
///
/// A bare `macro.play` (no `scope`) resolves the caller's session registry
/// first, then the trusted promoted registry; an explicit scope pins the
/// lookup to one tier, defeating shadowing ambiguity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacroScope {
    /// The caller's per-agent, session-lifetime registry.
    Session,
    /// The trusted, wire-immutable promoted registry.
    Trusted,
}

/// A command parsed from an OSC 777 control sequence.
///
/// Variants are grouped by subsystem. The first block is reachable today
/// through Ratty's existing resources; later blocks describe the operator
/// console and are handled as their subsystems are built.
#[derive(Debug, Clone, PartialEq)]
pub enum RattyAiCommand {
    // ── Presentation & objects (lower onto existing RGP/stage machinery) ──
    /// Set the presentation mode (`2d`, `3d`, `mobius`).
    SetMode {
        /// Requested mode string.
        mode: String,
    },
    /// Set the plane warp amount.
    SetWarp {
        /// Warp intensity `0..=1`.
        intensity: f32,
    },
    /// Spawn a 3D object from an embedded asset name.
    SpawnObject {
        /// Caller-chosen object id; must lie in the AI-owned range
        /// (see [`AI_OBJECT_ID_MIN`]) and the caller's namespace.
        id: u32,
        /// Embedded asset name (never a filesystem path; the wire is
        /// untrusted).
        path: String,
        /// Anchor column.
        x: u16,
        /// Anchor row.
        y: u16,
        /// Uniform scale.
        scale: f32,
        /// Spin rate (rad/s).
        spin: f32,
        /// Brightness multiplier.
        brightness: f32,
        /// Replace an existing live object under the same id instead of
        /// failing the spawn (ids are otherwise never reused in a session).
        replace: bool,
    },
    /// Remove one placed object by id.
    RemoveObject {
        /// Object id.
        id: u32,
    },
    /// Remove every placed object.
    ClearObjects,
    /// Update a placed object's transform/style.
    UpdateObject {
        /// Object id.
        id: u32,
        /// New anchor column.
        x: Option<u16>,
        /// New anchor row.
        y: Option<u16>,
        /// New uniform scale.
        scale: Option<f32>,
        /// New spin rate.
        spin: Option<f32>,
        /// New brightness.
        brightness: Option<f32>,
    },
    /// Update the terminal cursor model/animation.
    UpdateCursor {
        /// Cursor model asset.
        model: Option<String>,
        /// Spin rate.
        spin: Option<f32>,
        /// Bob speed.
        bob_speed: Option<f32>,
        /// Bob amplitude.
        bob_amp: Option<f32>,
        /// Brightness.
        brightness: Option<f32>,
        /// Visibility.
        visible: Option<bool>,
    },
    /// Reset the scene to its default presentation.
    Reset,

    // ── Ephemeral visual effects (the film stack) ──
    /// Flash the whole surface a color briefly.
    Flash {
        /// `#rrggbb` color.
        color: String,
        /// Seconds.
        duration: f32,
    },
    /// Pulse the surface brightness.
    Pulse {
        /// Peak intensity.
        intensity: f32,
        /// Seconds.
        duration: f32,
    },
    /// Tint the surface with a translucent color.
    Tint {
        /// `#rrggbb` color.
        color: String,
        /// Opacity `0..=1`.
        opacity: f32,
    },

    // ── AI presence ──
    /// Toggle/set the "thinking" indicator (`start`/`end`/`toggle`).
    Think {
        /// Requested state.
        state: String,
    },
    /// Set the confidence aura level `0..=1`.
    Confidence {
        /// Confidence.
        level: f32,
    },
    /// Set the mood (`excited`/`cautious`/`confused`/`focused`/`celebratory`).
    Mood {
        /// Mood tag.
        mood: String,
    },

    // ── Data visualization ──
    //
    // The M3.5 stub actions `chart`, `timeline`, `history`, and
    // `screenshot { path }` are retired (#20): charts and timelines are
    // registered `viz.set` payload *kinds* (`chart.bar.v1`,
    // `chart.line.v1`, `chart.gauge.v1`, `timeline.v1`), history is a
    // trusted `ratty-ai` collector publishing `timeline.v1` — the
    // terminal never reads shell history on wire command — and screen
    // capture, when it lands, will be pathless capture-to-artifact: the
    // wire never chooses or writes filesystem paths.
    /// Publish or refresh a data-visualization snapshot (`viz.set`).
    ///
    /// The payload is trusted-collector data lowered onto the wire by the
    /// `ratty-ai` CLI (or authored synthetically by a transmission); the
    /// terminal only ever *renders* it — a viz command never causes the
    /// terminal to execute, read, or enumerate anything.
    VizSet {
        /// Caller-chosen visualization id; must lie in the AI-owned range
        /// (see [`AI_OBJECT_ID_MIN`]) and the caller's namespace.
        id: u32,
        /// Registered, versioned payload kind (e.g. `ps.v1`). The version
        /// is part of the kind name; unknown kinds are rejected.
        kind: String,
        /// Raw unpadded-base64url JSON payload. Carried opaquely here —
        /// this module stays dependency-free — and decoded terminal-side
        /// under hard size limits.
        data: String,
        /// Raw anchor column (top-left) when the snapshot places itself.
        /// Carried unparsed off this std-only module and parsed as a `u16`
        /// terminal-side, so a *present* but malformed value rejects
        /// `bad-payload` instead of silently coercing to absent (which
        /// would drop the placement the caller asked for and still ack ok).
        x: Option<String>,
        /// Raw anchor row (top-left); parsed terminal-side, like `x`.
        y: Option<String>,
        /// Raw footprint width in cells; parsed terminal-side, like `x`.
        cols: Option<String>,
        /// Raw footprint height in cells; parsed terminal-side, like `x`.
        rows: Option<String>,
        /// Allow replacing a live visualization of a *different* kind under
        /// the same id (same-kind sets are always atomic upserts).
        replace: bool,
    },
    /// Trigger a bounded, self-expiring effect on a domain key inside a
    /// visualization (`viz.effect`). Effects target stable domain keys
    /// (pid / path / branch / interface), never entities; a known id with
    /// an absent key renders nothing and still acks ok.
    VizEffect {
        /// Target visualization id.
        id: u32,
        /// Domain key inside the snapshot (e.g. a pid as a string).
        key: String,
        /// Registered effect name (e.g. `died`, `survived`, `highlight`).
        effect: String,
    },
    /// Remove a visualization (`viz.remove`). Unlike `object.*` ids, a
    /// removed viz id may be reused — watchers restart under stable ids.
    VizRemove {
        /// Target visualization id.
        id: u32,
    },

    // ── Panes ──
    /// Split the terminal into panes.
    SplitPane {
        /// `vertical` or `horizontal`.
        direction: String,
        /// Split ratio.
        ratio: f32,
    },
    /// Focus a pane by id.
    FocusPane {
        /// Pane id.
        pane: u8,
    },
    /// Resize a pane.
    ResizePane {
        /// Pane id.
        pane: u8,
        /// New width in cells.
        width: Option<u16>,
        /// New height in cells.
        height: Option<u16>,
    },
    /// Close a pane.
    ClosePane {
        /// Pane id.
        pane: u8,
    },

    // ── View bookmarks ──
    /// Store the current public view state (mode, warp) under a
    /// caller-namespaced name. An existing name rejects `already-exists`
    /// unless `mode=replace` (#16's collision rule).
    Bookmark {
        /// Bookmark name.
        name: String,
        /// Whether `mode=replace` was supplied.
        replace: bool,
    },
    /// Atomically reapply a stored view bookmark through normal command
    /// lowering (`bookmark.jump`). Never restores objects, macros, or
    /// private scene state.
    BookmarkJump {
        /// Bookmark name.
        name: String,
    },

    // ── Collaboration (#25) ──
    /// Create a collaboration participant (`user.join`). Identity is the
    /// pair (ingress source namespace, caller-local `id=`): the namespace
    /// comes from the transport at apply time, never from the wire, so a
    /// stream can only ever populate its own roster — nothing in-band is
    /// trusted about identity. An existing id — fresh *or* expired —
    /// rejects `already-exists` unless `replace=true` (#16's collision
    /// rule); leases follow the sensor TTL model (#21).
    UserJoin {
        /// Caller-local participant id, the identity key within the
        /// caller's namespace. Charset/length validate at apply.
        id: String,
        /// Display name (defaults to the id). Rendering metadata only,
        /// never authentication.
        name: String,
        /// Cursor color; strict `#rrggbb` validates at apply (colors are
        /// identity-adjacent, so no lenient effects-style coercion).
        color: String,
        /// Lease TTL in f32 seconds (like `sensor.publish`); absent picks
        /// the participant default, and the terminal clamps the range.
        ttl: Option<f32>,
        /// Whether `replace=true` was supplied (overwrite an existing id).
        replace: bool,
    },
    /// Renew a participant's lease (`user.renew`), optionally re-pinning
    /// its TTL. Renewing an expired participant revives it — expiry is
    /// computed lazily and stays visible (#21), never deletes, so the row
    /// is still there to revive. Unknown ids reject `unknown-id`.
    UserRenew {
        /// Caller-local participant id.
        id: String,
        /// New lease TTL in f32 seconds; absent keeps the stored TTL.
        ttl: Option<f32>,
    },
    /// Update a participant's cursor cell (`user.cursor`). Any successful
    /// mutation of one's own participant also refreshes its lease.
    UserCursor {
        /// Caller-local participant id.
        id: String,
        /// Cursor column (cell). Required and strictly numeric: a typo'd
        /// coordinate is a bad command, never a silent `(0, 0)`.
        x: u16,
        /// Cursor row (cell); strict like `x`.
        y: u16,
    },
    /// Remove a participant (`user.leave`) — with `note.remove` and
    /// `reset`, the only ways a row actually leaves the registry (lease
    /// expiry alone never deletes). Unknown ids reject `unknown-id`.
    UserLeave {
        /// Caller-local participant id.
        id: String,
    },
    /// Place a floating annotation pinned to a grid cell (`note`). Keyed
    /// (namespace, `id=`) with the same collision and lease semantics as
    /// `user.join`. The M3 `expires=` free-string key is retired: strict
    /// `ttl=` seconds replaces it.
    Note {
        /// Caller-local note id.
        id: String,
        /// Note text (byte-capped at apply).
        text: String,
        /// Anchor column (cell); required and strictly numeric like
        /// `user.cursor`.
        x: u16,
        /// Anchor row (cell); strict like `x`.
        y: u16,
        /// Lease TTL in f32 seconds; absent picks the note default.
        ttl: Option<f32>,
        /// Whether `replace=true` was supplied (overwrite an existing id).
        replace: bool,
    },
    /// Remove a note (`note.remove`). Unknown ids reject `unknown-id`.
    NoteRemove {
        /// Caller-local note id.
        id: String,
    },

    // ── Sound ──
    /// Play a registered one-shot sound (`sound.play`).
    SoundPlay {
        /// Registered semantic kind (see [`SOUND_KINDS`]); never a path.
        kind: String,
        /// Requested gain; the terminal clamps to the kind's registry
        /// maximum (the wire requests, it never owns the mixer).
        gain: Option<f32>,
    },
    /// Set (or crossfade to) the scene ambient bed (`sound.ambient.set`).
    SoundAmbientSet {
        /// Registered ambient kind (see [`SOUND_KINDS`]).
        kind: String,
        /// Requested gain; clamped to the kind's registry maximum.
        gain: Option<f32>,
        /// Crossfade duration in milliseconds; clamped terminal-side.
        xfade: Option<u32>,
    },
    /// Fade out and clear the scene ambient bed (`sound.ambient.stop`).
    SoundAmbientStop {
        /// Fade-out duration in milliseconds; clamped terminal-side.
        fade: Option<u32>,
    },

    // ── Avatar (#23) ──
    /// Show the avatar and/or update its HUD placement (`avatar.set`).
    /// Partial-update semantics like `cursor`: every key is optional and
    /// an invalid field rejects the whole command. Scene-global: requires
    /// the avatar-scene capability.
    AvatarSet {
        /// Curated mascot registry id (see [`AVATAR_MODELS`]); never a
        /// path (#12). Unknown ids reject `unknown-model` at apply.
        model: Option<String>,
        /// One of the nine named HUD anchors (`position=`). Unknown names
        /// reject `unknown-anchor` at apply.
        position: Option<String>,
        /// Bounded pixel offset from the anchor, logical px, +y down;
        /// clamped terminal-side.
        dx: Option<f32>,
        /// See `dx`.
        dy: Option<f32>,
        /// Independent HUD scale; clamped terminal-side.
        scale: Option<f32>,
    },
    /// Show a hidden avatar without changing its representation
    /// (`avatar.show`). Scene-global: requires the avatar-scene capability.
    AvatarShow,
    /// One-shot named root-transform choreography (`avatar.gesture`).
    /// Ordinary tier. Unknown names reject `unknown-gesture` at apply.
    AvatarGesture {
        /// Choreography name (`bob`, `tilt`, `lean`, `nod`, `spin`).
        gesture: String,
    },
    /// Present an utterance through the bubble/typewriter overlay
    /// (`avatar.speak`). Ordinary tier; a long-running operation (#18):
    /// the ack is the started/queued form with an execution handle.
    AvatarSpeak {
        /// UTF-8 prose, byte-capped terminal-side (`text-too-long` over).
        text: String,
        /// Optional visible speaker label (`from=`), byte-capped. A
        /// presentation label only: attribution authority is always the
        /// ingress-derived namespace — a stream cannot claim an identity.
        from: Option<String>,
        /// Display duration in seconds. Derived from the text when absent;
        /// clamped terminal-side either way; pinned at admission.
        duration: Option<f32>,
    },
    /// Cancel the caller's own current or queued utterance (`avatar.stop`).
    AvatarStopSpeaking,
    /// Cancel one utterance by execution handle (`avatar.cancel;id=`);
    /// only the owner's handles are cancellable (`not-owner` otherwise).
    AvatarCancel {
        /// The execution handle from the started/queued ack.
        execution_id: String,
    },
    /// Clear queued avatar speech (`avatar.speech.clear`). The bare form
    /// is ownership-scoped; wider scopes are scene-global and require the
    /// avatar-scene capability.
    AvatarSpeechClear {
        /// The clear scope.
        scope: SpeechClearScope,
    },
    /// Hide the avatar globally (`avatar.hide`), cancelling the current
    /// utterance and clearing the whole queue (#23 §2). Scene-global:
    /// requires the avatar-scene capability.
    AvatarHide,

    // ── Macros ──
    /// Begin recording a macro. An existing name rejects `already-exists`
    /// unless `mode=replace` (#16's collision rule); replacement is
    /// transactional — the prior macro survives until `macro.stop` finalizes.
    MacroRecord {
        /// Macro name.
        name: String,
        /// Whether `mode=replace` was supplied.
        replace: bool,
    },
    /// Stop recording (finalize) or cancel an active playback.
    MacroStop,
    /// Replay a recorded macro.
    MacroPlay {
        /// Macro name. Empty when `hash` addresses the macro directly.
        name: String,
        /// Immutable content-hash reference (hex). When present it resolves
        /// the macro directly across both registries, defeating shadowing
        /// ambiguity regardless of `scope`.
        hash: Option<String>,
        /// Playback rate multiplier (`2.0` = double speed, `0.5` = half).
        /// `1.0` is the recorded tempo. Validated at apply time.
        rate: f32,
        /// Instant playback: drop the recorded inter-command delays,
        /// preserving command order and the per-frame execution budget.
        instant: bool,
        /// Resolution scope. `None` resolves the caller's session registry
        /// first, then the trusted registry; an explicit scope defeats
        /// shadowing ambiguity.
        scope: Option<MacroScope>,
    },
    /// Export a macro to a file.
    MacroExport {
        /// Macro name.
        name: String,
        /// Destination path.
        to: String,
    },
    /// Run a macro file.
    MacroRun {
        /// Macro file path.
        path: String,
    },

    // ── Reactive (#21) ──
    /// Register (or `mode=replace`) a declarative rule: a sensor-referencing
    /// trigger plus one allowlisted choreography action (`rule.set`).
    RuleSet {
        /// Rule name, unique within the caller's session tier.
        name: String,
        /// Referenced sensor: `sys.*` or the caller's own `agent.<ns>.*`.
        /// The sensor may not exist yet — the rule registers unbound and
        /// binds automatically when a compatible sensor appears.
        sensor: String,
        /// Activation threshold: active when the sample is at or above it.
        /// Exactly one of `above`/`below` is present (enforced at parse).
        above: Option<f32>,
        /// Activation threshold: active when the sample is at or below it.
        below: Option<f32>,
        /// Hysteresis release threshold. Defaults to the activation
        /// threshold (no hysteresis); validated on the correct side of it.
        clear: Option<f32>,
        /// Seconds the raw condition must hold before the rule activates.
        debounce: Option<f32>,
        /// Minimum seconds between fires; a transition inside the cooldown
        /// is latched but its fire is suppressed (and counted).
        cooldown: Option<f32>,
        /// Whether `mode=replace` was supplied (#16's collision rule).
        replace: bool,
        /// The allowlisted action fired on an inactive→active transition,
        /// in the same `<action>[;<payload>]` grammar as the wire itself
        /// (carried percent-encoded in `do=`). Parsed structurally here;
        /// the allowlist is the registry's semantic check.
        action: Box<RattyAiCommand>,
    },
    /// Remove one of the caller's session rules (`rule.remove`).
    RuleRemove {
        /// Rule name.
        name: String,
    },
    /// Enable one of the caller's session rules (`rule.enable`).
    RuleEnable {
        /// Rule name.
        name: String,
    },
    /// Disable one of the caller's session rules (`rule.disable`). The
    /// rule's transition state freezes exactly as sensor dormancy freezes
    /// it; the stored rule is untouched.
    RuleDisable {
        /// Rule name.
        name: String,
    },
    /// Publish (upserting on first use) a caller-owned wire sensor sample
    /// inside the caller's `agent.<ns>.*` namespace (`sensor.publish`).
    SensorPublish {
        /// Sensor name suffix; the terminal prefixes `agent.<ns>.`.
        name: String,
        /// The sample value (validated finite at apply).
        value: f32,
        /// Freshness lifetime in seconds; a sample older than its TTL makes
        /// dependent rules dormant rather than supplying a false value.
        ttl: Option<f32>,
        /// Optional caller-supplied sequence number; must be strictly
        /// increasing per sensor (stale/duplicate sequences reject).
        seq: Option<u64>,
    },
    /// Remove one of the caller's wire sensors (`sensor.remove`);
    /// dependent rules fall back to the unbound state.
    SensorRemove {
        /// Sensor name suffix.
        name: String,
    },
}

impl RattyAiCommand {
    /// Whether this is a macro-control command (`macro.*`). These are never
    /// captured into a recording, and one encountered mid-playback is
    /// rejected: a macro can neither record nor play macros (#16 no
    /// recursion).
    pub fn is_macro_control(&self) -> bool {
        matches!(
            self,
            Self::MacroRecord { .. }
                | Self::MacroStop
                | Self::MacroPlay { .. }
                | Self::MacroExport { .. }
                | Self::MacroRun { .. }
        )
    }

    /// Whether this is a reactive-control command (`rule.*` / `sensor.*`).
    /// Rule mutation is control, never choreography; sensor samples are
    /// trigger *input* — recording either into a macro would let a replay
    /// mutate rules or feed the evaluator stale telemetry, so the whole
    /// family is control-plane (#21).
    pub fn is_reactive_control(&self) -> bool {
        matches!(
            self,
            Self::RuleSet { .. }
                | Self::RuleRemove { .. }
                | Self::RuleEnable { .. }
                | Self::RuleDisable { .. }
                | Self::SensorPublish { .. }
                | Self::SensorRemove { .. }
        )
    }

    /// Whether this is a collaboration-presence command (`user.*` and
    /// `note`/`note.remove`, #25). Presence identity is ingress truth — a
    /// participant exists because a live stream said so *now* — so a
    /// macro-replayed `user.join` would forge liveness and a replayed
    /// `user.leave` would evict a real participant. The whole family is
    /// control-plane, excluded from macro recording, and the applier
    /// additionally rejects any non-wire origin. (Distinct from the
    /// effects Think/Confidence/Mood family that older docs call "AI
    /// presence": that one is recordable choreography.)
    pub fn is_presence_control(&self) -> bool {
        matches!(
            self,
            Self::UserJoin { .. }
                | Self::UserRenew { .. }
                | Self::UserCursor { .. }
                | Self::UserLeave { .. }
                | Self::Note { .. }
                | Self::NoteRemove { .. }
        )
    }

    /// Whether this command belongs to the control-plane class excluded from
    /// macro recording (#16 plus the #21 and #25 amendments): macro-control
    /// commands, reactive control (`rule.*` / `sensor.*`), and collaboration
    /// presence (`user.*` / `note*`). Query and transport envelopes are OSC
    /// 778, never `RattyAiCommand`s, so they are excluded by construction,
    /// and `tok=` correlation tokens are transport metadata stripped before
    /// capture. Everything else is recordable choreography.
    pub fn is_control_plane(&self) -> bool {
        self.is_macro_control() || self.is_reactive_control() || self.is_presence_control()
    }

    /// Whether this command belongs to the rule-action allowlist's directly
    /// safe class (#21): scene-choreography with no spawn/remove/re-anchor
    /// blast radius — the effects/presence family, `sound.play`,
    /// `viz.effect`, and live-field `object.update` (no `x`/`y`: re-anchor
    /// is respawn-class). `macro.play` is the one indirect action and is
    /// deliberately *not* here: a rule may fire it only when the pinned
    /// macro is itself rule-safe (every step in this class), which is
    /// registry state checked at `rule.set` and again at fire time. A macro
    /// is classified **rule-safe** at finalize exactly as *privileged* is:
    /// every captured step satisfies this predicate.
    pub fn is_rule_safe_action(&self) -> bool {
        match self {
            Self::Flash { .. }
            | Self::Pulse { .. }
            | Self::Tint { .. }
            | Self::Think { .. }
            | Self::Confidence { .. }
            | Self::Mood { .. }
            | Self::SoundPlay { .. }
            | Self::VizEffect { .. } => true,
            Self::UpdateObject { x, y, .. } => x.is_none() && y.is_none(),
            _ => false,
        }
    }

    /// Whether this command mutates scene-global presentation state (mode,
    /// warp, reset, avatar scene control) rather than staying inside the
    /// caller's own object namespace. A recording that contains one is
    /// classified *privileged* and needs the exclusive scene lock to play
    /// (#16 privileged macros).
    pub fn is_scene_global(&self) -> bool {
        match self {
            Self::SetMode { .. } | Self::SetWarp { .. } | Self::Reset => true,
            // Avatar scene control (#23): representation, global
            // visibility, and cross-agent queue authority are shared
            // scene state.
            Self::AvatarSet { .. } | Self::AvatarShow | Self::AvatarHide => true,
            // Clearing one's own utterance is ownership-scoped; any wider
            // scope is scene-global (arg-dependent classification has
            // precedent: rule-safe `object.update` keys on x/y).
            Self::AvatarSpeechClear { scope } => !matches!(scope, SpeechClearScope::Own),
            _ => false,
        }
    }

    /// Whether this command addresses live execution state by
    /// session-scoped handle (or implicitly, the caller's own current
    /// operation). Handles are transport-epoch metadata like `tok=`
    /// correlation tokens — meaningless in a recording or a durable
    /// trusted macro — so the recorder tap skips these and the trusted
    /// loader refuses them (#18).
    pub fn is_execution_control(&self) -> bool {
        matches!(self, Self::AvatarStopSpeaking | Self::AvatarCancel { .. })
    }
}

/// A `ratty:`-namespaced OSC 777 sequence parsed at terminal ingress.
///
/// `command` is `None` when the sequence claimed the namespace but did not
/// parse into a command (unknown action, missing required key). The ack
/// token is extracted at the envelope layer, *before* command parsing can
/// fail, so a malformed command that opted in still gets its error ack.
#[derive(Debug, Clone, PartialEq)]
pub struct AiControl {
    /// The parsed command, when the sequence parsed.
    pub command: Option<RattyAiCommand>,
    /// The `tok=` ack opt-in token, when present and valid.
    pub ack_token: Option<String>,
}

/// Parses an OSC sequence delivered by vt100 as `;`-split params.
///
/// Returns `None` for any OSC not claiming code `777` with the `ratty:`
/// namespace, so unrelated OSC 777 users pass through untouched.
pub fn parse_osc(params: &[&[u8]]) -> Option<RattyAiCommand> {
    parse_osc_control(params)?.command
}

/// Parses an OSC sequence delivered by vt100 as `;`-split params, keeping
/// the ack token even when the command itself fails to parse. This is the
/// terminal's ingress entry point; [`parse_osc`] is the command-only view.
pub fn parse_osc_control(params: &[&[u8]]) -> Option<AiControl> {
    let first = params.first()?;
    if *first != RATTY_AI_OSC {
        return None;
    }
    // Rejoin the remaining params — vt100 split the whole sequence on `;`,
    // but our grammar is `ratty:<action> ; <payload>`, so the action and
    // payload arrive as separate params.
    let rest: Vec<&[u8]> = params[1..].to_vec();
    let joined = rest
        .iter()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect::<Vec<_>>()
        .join(";");
    parse_control(&joined)
}

/// Parses the post-`777;` command string (`ratty:<action>;<payload>`).
///
/// Exposed for the CLI and tests; [`parse_osc`] is the terminal entry point.
pub fn parse_command(data: &str) -> Option<RattyAiCommand> {
    parse_control(data)?.command
}

/// Parses the post-`777;` command string, separating the ack opt-in token
/// from the command so envelope-level failures can still be acked.
pub fn parse_control(data: &str) -> Option<AiControl> {
    let rest = data.strip_prefix(RATTY_AI_PREFIX)?;
    let (action, payload) = rest.split_once(';').unwrap_or((rest, ""));
    let p = Payload::parse(payload);
    let ack_token = p.string(ACK_TOKEN_KEY).filter(|token| {
        !token.is_empty()
            && token.len() <= 64
            && token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    });
    Some(AiControl {
        command: parse_action(action, &p),
        ack_token,
    })
}

fn parse_action(action: &str, p: &Payload) -> Option<RattyAiCommand> {
    Some(match action {
        // Presentation & objects
        "mode" => RattyAiCommand::SetMode {
            mode: p.positional_or_default(),
        },
        "warp" => RattyAiCommand::SetWarp {
            intensity: p.f32("intensity", 0.0),
        },
        "object.add" => RattyAiCommand::SpawnObject {
            id: p.parse_req("id")?,
            path: p.string("path")?,
            x: p.u16("x", 0),
            y: p.u16("y", 0),
            scale: p.f32("scale", 1.0),
            spin: p.f32("spin", 0.0),
            brightness: p.f32("brightness", 1.0),
            replace: p.flag("replace"),
        },
        "object.remove" => RattyAiCommand::RemoveObject {
            id: p.parse_req("id")?,
        },
        "object.clear" => RattyAiCommand::ClearObjects,
        "object.update" => RattyAiCommand::UpdateObject {
            id: p.parse_req("id")?,
            x: p.opt("x"),
            y: p.opt("y"),
            scale: p.opt("scale"),
            spin: p.opt("spin"),
            brightness: p.opt("brightness"),
        },
        "cursor" => RattyAiCommand::UpdateCursor {
            model: p.string("model"),
            spin: p.opt("spin"),
            bob_speed: p.opt("bob_speed"),
            bob_amp: p.opt("bob_amp"),
            brightness: p.opt("brightness"),
            visible: p.bool_opt("visible"),
        },
        "reset" => RattyAiCommand::Reset,

        // Effects
        "flash" => RattyAiCommand::Flash {
            color: p.string_or("color", "#ffffff"),
            duration: p.f32("duration", 0.5),
        },
        "pulse" => RattyAiCommand::Pulse {
            intensity: p.f32("intensity", 0.8),
            duration: p.f32("duration", 1.0),
        },
        "tint" => RattyAiCommand::Tint {
            color: p.string_or("color", "#ffffff"),
            opacity: p.f32("opacity", 0.1),
        },

        // Presence
        "think" => RattyAiCommand::Think {
            state: p.string_or("state", "toggle"),
        },
        "confidence" => RattyAiCommand::Confidence {
            level: p.f32("level", 0.5),
        },
        "mood" => RattyAiCommand::Mood {
            mood: p.string_or("mood", "focused"),
        },

        // Data viz — charts and timelines ride `viz.set` as registered
        // payload kinds; the retired `chart`/`timeline`/`screenshot`
        // stub actions fail parse like any unknown action.
        "viz.set" => RattyAiCommand::VizSet {
            id: p.parse_req("id")?,
            kind: p.string("kind")?,
            data: p.string("data")?,
            // Placement params ride raw (like `data`) and are parsed
            // terminal-side so a malformed present value is an explicit
            // reject, not a silent unplacement.
            x: p.string("x"),
            y: p.string("y"),
            cols: p.string("cols"),
            rows: p.string("rows"),
            replace: p.flag("replace"),
        },
        "viz.effect" => RattyAiCommand::VizEffect {
            id: p.parse_req("id")?,
            key: p.string("key")?,
            effect: p.string("effect")?,
        },
        "viz.remove" => RattyAiCommand::VizRemove {
            id: p.parse_req("id")?,
        },

        // Panes
        "pane.split" => RattyAiCommand::SplitPane {
            direction: p.string_or("direction", "vertical"),
            ratio: p.f32("ratio", 0.5),
        },
        "pane.focus" => RattyAiCommand::FocusPane {
            pane: p.parse_req("pane")?,
        },
        "pane.resize" => RattyAiCommand::ResizePane {
            pane: p.parse_req("pane")?,
            width: p.opt("width"),
            height: p.opt("height"),
        },
        "pane.close" => RattyAiCommand::ClosePane {
            pane: p.parse_req("pane")?,
        },

        // View bookmarks — the retired `history` and `jump` stub actions
        // fail parse like any unknown action (`ratty-ai history` is a
        // collector now, and jumping is `bookmark.jump`).
        "bookmark" => {
            // `mode` is a closed vocabulary: absent stores fresh,
            // `replace` overwrites; anything else is a bad command, not
            // a silently-ignored qualifier.
            let replace = match p.string("mode").as_deref() {
                None => false,
                Some("replace") => true,
                Some(_) => return None,
            };
            RattyAiCommand::Bookmark {
                name: p.string("name")?,
                replace,
            }
        }
        "bookmark.jump" => RattyAiCommand::BookmarkJump {
            name: p.string("name")?,
        },

        // Collaboration presence (#25). Identity keys (`id=`) ride as
        // strings; charset/length/color/ttl-range checks stay at apply
        // time for their explicit ack codes — but every numeric parses
        // strictly (a typo'd coordinate or ttl is a bad command, never a
        // silently different one), retiring the lenient default-0 path
        // these commands carried through M3.
        "user.join" => {
            // The display name defaults to the id at parse so both wire
            // ends agree without a second defaulting site at apply.
            let id = p.string("id")?;
            let name = p.string_or("name", &id);
            RattyAiCommand::UserJoin {
                id,
                name,
                color: p.string_or("color", "#00ff00"),
                ttl: p.opt_strict("ttl").ok()?,
                replace: p.flag("replace"),
            }
        }
        "user.renew" => RattyAiCommand::UserRenew {
            id: p.string("id")?,
            ttl: p.opt_strict("ttl").ok()?,
        },
        "user.cursor" => RattyAiCommand::UserCursor {
            id: p.string("id")?,
            x: p.parse_req("x")?,
            y: p.parse_req("y")?,
        },
        "user.leave" => RattyAiCommand::UserLeave {
            id: p.string("id")?,
        },
        "note" => RattyAiCommand::Note {
            id: p.string("id")?,
            text: p.string("text")?,
            x: p.parse_req("x")?,
            y: p.parse_req("y")?,
            ttl: p.opt_strict("ttl").ok()?,
            replace: p.flag("replace"),
        },
        "note.remove" => RattyAiCommand::NoteRemove {
            id: p.string("id")?,
        },

        // Sound. (The bare `sound` stub action is retired: nothing ever
        // handled it, so its removal breaks no working caller.)
        "sound.play" => RattyAiCommand::SoundPlay {
            kind: p.string("kind")?,
            gain: p.opt("gain"),
        },
        "sound.ambient.set" => RattyAiCommand::SoundAmbientSet {
            kind: p.string("kind")?,
            gain: p.opt("gain"),
            xfade: p.opt("xfade"),
        },
        "sound.ambient.stop" => RattyAiCommand::SoundAmbientStop {
            fade: p.opt("fade"),
        },

        // Avatar (#23). Vocabulary validation (model/anchor/gesture names)
        // stays at apply time so unknown names get their explicit ack
        // codes, not a generic bad-command — the object.add split.
        "avatar.set" => RattyAiCommand::AvatarSet {
            model: p.string("model"),
            position: p.string("position"),
            dx: p.opt_strict("dx").ok()?,
            dy: p.opt_strict("dy").ok()?,
            scale: p.opt_strict("scale").ok()?,
        },
        "avatar.show" => RattyAiCommand::AvatarShow,
        "avatar.gesture" => RattyAiCommand::AvatarGesture {
            gesture: p.string("gesture")?,
        },
        "avatar.speak" => RattyAiCommand::AvatarSpeak {
            text: p.string("text")?,
            from: p.string("from"),
            // Strict: a typo'd duration is a bad command, never a
            // silently absent field (the rule.set opt_strict posture).
            duration: p.opt_strict("duration").ok()?,
        },
        "avatar.stop" => RattyAiCommand::AvatarStopSpeaking,
        "avatar.cancel" => RattyAiCommand::AvatarCancel {
            execution_id: p.string("id")?,
        },
        "avatar.speech.clear" => {
            // `ns=` and `scope=` are mutually exclusive closed
            // vocabularies; `scope=` accepts only `all`.
            let ns = p.string("ns");
            let scope_key = p.string("scope");
            let scope = match (ns, scope_key.as_deref()) {
                (None, None) => SpeechClearScope::Own,
                (Some(ns), None) => SpeechClearScope::Namespace(ns.parse().ok()?),
                (None, Some("all")) => SpeechClearScope::All,
                _ => return None,
            };
            RattyAiCommand::AvatarSpeechClear { scope }
        }
        "avatar.hide" => RattyAiCommand::AvatarHide,

        // Macros
        "macro.record" => {
            // `mode` is the same closed vocabulary as `bookmark`: absent
            // records fresh, `replace` overwrites transactionally.
            let replace = match p.string("mode").as_deref() {
                None => false,
                Some("replace") => true,
                Some(_) => return None,
            };
            RattyAiCommand::MacroRecord {
                name: p.string("name")?,
                replace,
            }
        }
        "macro.stop" => RattyAiCommand::MacroStop,
        "macro.play" => {
            // Address by `name` or by immutable `hash`; at least one is
            // required. `mode` and `scope` are closed vocabularies — an
            // unknown value is a bad command, not a silently-ignored tag.
            let name = p.string_or("name", "");
            let hash = p.string("hash");
            if name.is_empty() && hash.is_none() {
                return None;
            }
            let instant = match p.string("mode").as_deref() {
                None => false,
                Some("instant") => true,
                Some(_) => return None,
            };
            let scope = match p.string("scope").as_deref() {
                None => None,
                Some("session") => Some(MacroScope::Session),
                Some("trusted") => Some(MacroScope::Trusted),
                Some(_) => return None,
            };
            RattyAiCommand::MacroPlay {
                name,
                hash,
                rate: p.f32("rate", 1.0),
                instant,
                scope,
            }
        }
        "macro.export" => RattyAiCommand::MacroExport {
            name: p.string("name")?,
            to: p.string_or("to", "macro.ratty"),
        },
        "macro.run" => RattyAiCommand::MacroRun {
            path: p.string("path")?,
        },

        // Reactive (#21). (The M3 `react` stub action is retired: nothing
        // ever handled it, so its removal breaks no working caller.)
        "rule.set" => {
            // `mode` is the same closed vocabulary as `macro.record`.
            let replace = match p.string("mode").as_deref() {
                None => false,
                Some("replace") => true,
                Some(_) => return None,
            };
            // Trigger numbers parse strictly: a typo'd threshold is a bad
            // command, never a silently-absent field (the `Payload::opt`
            // hazard would turn `above=8O` into "no threshold").
            let above: Option<f32> = p.opt_strict("above").ok()?;
            let below: Option<f32> = p.opt_strict("below").ok()?;
            // Exactly one comparison side.
            if above.is_some() == below.is_some() {
                return None;
            }
            RattyAiCommand::RuleSet {
                name: p.string("name")?,
                sensor: p.string("sensor")?,
                above,
                below,
                clear: p.opt_strict("clear").ok()?,
                debounce: p.opt_strict("debounce").ok()?,
                cooldown: p.opt_strict("cooldown").ok()?,
                replace,
                action: Box::new(parse_rule_action(&p.string("do")?)?),
            }
        }
        "rule.remove" => RattyAiCommand::RuleRemove {
            name: p.string("name")?,
        },
        "rule.enable" => RattyAiCommand::RuleEnable {
            name: p.string("name")?,
        },
        "rule.disable" => RattyAiCommand::RuleDisable {
            name: p.string("name")?,
        },
        "sensor.publish" => RattyAiCommand::SensorPublish {
            name: p.string("name")?,
            value: p.parse_req("value")?,
            ttl: p.opt_strict("ttl").ok()?,
            seq: p.opt_strict("seq").ok()?,
        },
        "sensor.remove" => RattyAiCommand::SensorRemove {
            name: p.string("name")?,
        },

        _ => return None,
    })
}

/// Parses a `do=` rule action: the same `<action>[;<payload>]` grammar as
/// the wire itself (percent-encoding shields the inner `;`/`&`/`=` from the
/// outer payload). Rejecting the `rule.*`/`sensor.*` families **by name,
/// before the recursive parse**, both encodes "rules cannot create or
/// modify rules" (#21) and structurally bounds `do=` nesting at one level;
/// every other action parses here and faces the registry's allowlist.
///
/// Exposed so trusted config rules lower their `action` strings through the
/// identical grammar.
pub fn parse_rule_action(spec: &str) -> Option<RattyAiCommand> {
    let (action, payload) = spec.split_once(';').unwrap_or((spec, ""));
    if action.starts_with("rule.") || action.starts_with("sensor.") {
        return None;
    }
    parse_action(action, &Payload::parse(payload))
}

/// Builds the full OSC 777 sequence (BEL-terminated) for an action and its
/// already-formatted `k=v&…` payload. Used by the `ratty-ai` CLI.
pub fn osc_sequence(action: &str, payload: &str) -> String {
    if payload.is_empty() {
        format!("\x1b]{};{RATTY_AI_PREFIX}{action}\x07", "777")
    } else {
        format!("\x1b]{};{RATTY_AI_PREFIX}{action};{payload}\x07", "777")
    }
}

/// Percent-encodes a value so it survives the `;`/`&`/`=` grammar.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Percent-decodes a value; invalid escapes are preserved verbatim.
pub fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// A parsed OSC payload: an optional bare positional plus `k=v` params, with
/// every value percent-decoded. Splitting happens before decoding so an
/// encoded `&`/`=` inside a value never breaks the grammar.
struct Payload {
    positional: Option<String>,
    params: std::collections::HashMap<String, String>,
}

impl Payload {
    fn parse(payload: &str) -> Self {
        let mut positional = None;
        let mut params = std::collections::HashMap::new();
        for token in payload.split('&').filter(|t| !t.is_empty()) {
            match token.split_once('=') {
                Some((key, value)) => {
                    params.insert(key.to_string(), percent_decode(value));
                }
                None => positional = Some(percent_decode(token)),
            }
        }
        Self { positional, params }
    }

    fn positional_or_default(&self) -> String {
        self.positional.clone().unwrap_or_default()
    }

    fn string(&self, key: &str) -> Option<String> {
        self.params.get(key).cloned()
    }

    fn string_or(&self, key: &str, default: &str) -> String {
        self.params
            .get(key)
            .cloned()
            .unwrap_or_else(|| default.to_string())
    }

    fn opt<T: std::str::FromStr>(&self, key: &str) -> Option<T> {
        self.params.get(key).and_then(|s| s.parse().ok())
    }

    /// A strictly-parsed optional value: `Ok(None)` when the key is absent,
    /// `Ok(Some(v))` when it parses, `Err(())` when present but malformed —
    /// for fields where [`Payload::opt`]'s silent fallback would turn a
    /// typo into a semantically different command.
    fn opt_strict<T: std::str::FromStr>(&self, key: &str) -> Result<Option<T>, ()> {
        match self.params.get(key) {
            None => Ok(None),
            Some(raw) => raw.parse().map(Some).map_err(|_| ()),
        }
    }

    fn parse_req<T: std::str::FromStr>(&self, key: &str) -> Option<T> {
        self.params.get(key)?.parse().ok()
    }

    fn f32(&self, key: &str, default: f32) -> f32 {
        self.opt(key).unwrap_or(default)
    }

    fn u16(&self, key: &str, default: u16) -> u16 {
        self.opt(key).unwrap_or(default)
    }

    fn flag(&self, key: &str) -> bool {
        self.params.get(key).map(|s| s == "true").unwrap_or(false)
    }

    fn bool_opt(&self, key: &str) -> Option<bool> {
        self.params.get(key).map(|s| s == "true")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_osc_requires_the_777_code_and_ratty_namespace() {
        assert!(parse_osc(&[b"52", b"c", b"data"]).is_none());
        assert!(parse_osc(&[b"777", b"other:thing"]).is_none());
        assert_eq!(
            parse_osc(&[b"777", b"ratty:mode", b"3d"]),
            Some(RattyAiCommand::SetMode {
                mode: "3d".to_string()
            })
        );
    }

    #[test]
    fn mode_uses_the_bare_positional() {
        assert_eq!(
            parse_command("ratty:mode;mobius"),
            Some(RattyAiCommand::SetMode {
                mode: "mobius".to_string()
            })
        );
    }

    #[test]
    fn warp_reads_a_keyed_float() {
        assert_eq!(
            parse_command("ratty:warp;intensity=0.5"),
            Some(RattyAiCommand::SetWarp { intensity: 0.5 })
        );
    }

    #[test]
    fn object_add_fills_defaults_and_requires_id_and_path() {
        assert_eq!(
            parse_command("ratty:object.add;id=2147483648&path=rat.obj&x=10&y=5&spin=2.0"),
            Some(RattyAiCommand::SpawnObject {
                id: 0x8000_0000,
                path: "rat.obj".to_string(),
                x: 10,
                y: 5,
                scale: 1.0,
                spin: 2.0,
                brightness: 1.0,
                replace: false,
            })
        );
        // Missing path, then missing id: both required.
        assert!(parse_command("ratty:object.add;id=2147483648&x=1").is_none());
        assert!(parse_command("ratty:object.add;path=rat.obj").is_none());
    }

    #[test]
    fn ai_object_ids_partition_by_namespace() {
        assert_eq!(ai_object_namespace(0x8000_0001), Some(0));
        assert_eq!(ai_object_namespace(0x8100_0000), Some(1));
        assert_eq!(ai_object_namespace(0xFF00_0000), Some(0x7F));
        // Below the AI range: transmission/system ids have no namespace.
        assert_eq!(ai_object_namespace(0x7FFF_FFFF), None);
        assert_eq!(ai_object_namespace(7), None);
    }

    #[test]
    fn object_update_is_all_optional_but_the_id() {
        assert_eq!(
            parse_command("ratty:object.update;id=1&spin=5.0&brightness=2.0"),
            Some(RattyAiCommand::UpdateObject {
                id: 1,
                x: None,
                y: None,
                scale: None,
                spin: Some(5.0),
                brightness: Some(2.0),
            })
        );
    }

    #[test]
    fn flags_default_false_and_read_true() {
        let replace_of = |payload: &str| {
            let command = parse_command(payload).expect("object.add parses");
            let RattyAiCommand::SpawnObject { replace, .. } = command else {
                panic!("expected SpawnObject");
            };
            replace
        };
        assert!(replace_of(
            "ratty:object.add;id=2147483648&path=rat.obj&replace=true"
        ));
        // Absent flags read false.
        assert!(!replace_of("ratty:object.add;id=2147483648&path=rat.obj"));
        // Only the literal string "true" reads true.
        assert!(!replace_of(
            "ratty:object.add;id=2147483648&path=rat.obj&replace=1"
        ));
    }

    #[test]
    fn viz_set_requires_id_kind_and_data() {
        assert_eq!(
            parse_command(
                "ratty:viz.set;id=2147483648&kind=ps.v1&data=e30&x=10&y=5&cols=24&rows=8"
            ),
            Some(RattyAiCommand::VizSet {
                id: 0x8000_0000,
                kind: "ps.v1".to_string(),
                data: "e30".to_string(),
                x: Some("10".to_string()),
                y: Some("5".to_string()),
                cols: Some("24".to_string()),
                rows: Some("8".to_string()),
                replace: false,
            })
        );
        // Placement values ride raw: parse never validates them (that is
        // the terminal's job), so even a nonsense value still parses here.
        assert_eq!(
            parse_command("ratty:viz.set;id=2147483648&kind=ps.v1&data=e30&x=abc&y=5"),
            Some(RattyAiCommand::VizSet {
                id: 0x8000_0000,
                kind: "ps.v1".to_string(),
                data: "e30".to_string(),
                x: Some("abc".to_string()),
                y: Some("5".to_string()),
                cols: None,
                rows: None,
                replace: false,
            })
        );
        // Placement and footprint are optional; replace is a flag.
        assert_eq!(
            parse_command("ratty:viz.set;id=2147483648&kind=ps.v1&data=e30&replace=true"),
            Some(RattyAiCommand::VizSet {
                id: 0x8000_0000,
                kind: "ps.v1".to_string(),
                data: "e30".to_string(),
                x: None,
                y: None,
                cols: None,
                rows: None,
                replace: true,
            })
        );
        // Missing data, kind, then id: all three are required.
        assert!(parse_command("ratty:viz.set;id=2147483648&kind=ps.v1").is_none());
        assert!(parse_command("ratty:viz.set;id=2147483648&data=e30").is_none());
        assert!(parse_command("ratty:viz.set;kind=ps.v1&data=e30").is_none());
    }

    #[test]
    fn viz_effect_requires_id_key_and_effect() {
        assert_eq!(
            parse_command("ratty:viz.effect;id=2147483648&key=1234&effect=died"),
            Some(RattyAiCommand::VizEffect {
                id: 0x8000_0000,
                key: "1234".to_string(),
                effect: "died".to_string(),
            })
        );
        assert!(parse_command("ratty:viz.effect;id=2147483648&key=1234").is_none());
        assert!(parse_command("ratty:viz.effect;id=2147483648&effect=died").is_none());
        assert!(parse_command("ratty:viz.effect;key=1234&effect=died").is_none());
    }

    #[test]
    fn viz_remove_requires_id() {
        assert_eq!(
            parse_command("ratty:viz.remove;id=2147483648"),
            Some(RattyAiCommand::VizRemove { id: 0x8000_0000 })
        );
        assert!(parse_command("ratty:viz.remove").is_none());
    }

    #[test]
    fn percent_encoded_values_round_trip_through_special_chars() {
        // A note whose text contains the grammar's own delimiters.
        let text = "a=b & c; done";
        let payload = format!("id=n1&text={}&x=15&y=10&ttl=600", percent_encode(text));
        let command = parse_command(&format!("ratty:note;{payload}")).expect("note parses");
        assert_eq!(
            command,
            RattyAiCommand::Note {
                id: "n1".to_string(),
                text: text.to_string(),
                x: 15,
                y: 10,
                ttl: Some(600.0),
                replace: false,
            }
        );
    }

    #[test]
    fn osc_sequence_frames_action_and_payload() {
        assert_eq!(osc_sequence("mode", "3d"), "\x1b]777;ratty:mode;3d\x07");
        assert_eq!(osc_sequence("reset", ""), "\x1b]777;ratty:reset\x07");
    }

    #[test]
    fn unknown_action_is_none() {
        assert!(parse_command("ratty:teleport;x=1").is_none());
    }

    #[test]
    fn sound_play_requires_a_kind_and_reads_optional_gain() {
        assert_eq!(
            parse_command("ratty:sound.play;kind=chime"),
            Some(RattyAiCommand::SoundPlay {
                kind: "chime".to_string(),
                gain: None,
            })
        );
        assert_eq!(
            parse_command("ratty:sound.play;kind=alert&gain=0.5"),
            Some(RattyAiCommand::SoundPlay {
                kind: "alert".to_string(),
                gain: Some(0.5),
            })
        );
        assert!(parse_command("ratty:sound.play").is_none(), "kind required");
    }

    #[test]
    fn sound_ambient_set_and_stop_parse_their_fields() {
        assert_eq!(
            parse_command("ratty:sound.ambient.set;kind=ambient.hum&gain=0.4&xfade=800"),
            Some(RattyAiCommand::SoundAmbientSet {
                kind: "ambient.hum".to_string(),
                gain: Some(0.4),
                xfade: Some(800),
            })
        );
        assert!(
            parse_command("ratty:sound.ambient.set").is_none(),
            "kind required"
        );
        assert_eq!(
            parse_command("ratty:sound.ambient.stop"),
            Some(RattyAiCommand::SoundAmbientStop { fade: None })
        );
        assert_eq!(
            parse_command("ratty:sound.ambient.stop;fade=250"),
            Some(RattyAiCommand::SoundAmbientStop { fade: Some(250) })
        );
    }

    #[test]
    fn the_bare_sound_stub_action_is_retired() {
        assert!(parse_command("ratty:sound;kind=success").is_none());
    }

    #[test]
    fn sound_kind_registry_classifies_registered_kinds() {
        assert_eq!(sound_kind_class("chime"), Some(SoundKindClass::OneShot));
        assert_eq!(sound_kind_class("alert"), Some(SoundKindClass::OneShot));
        assert_eq!(sound_kind_class("pulse"), Some(SoundKindClass::OneShot));
        assert_eq!(sound_kind_class("click"), Some(SoundKindClass::OneShot));
        assert_eq!(
            sound_kind_class("ambient.hum"),
            Some(SoundKindClass::Ambient)
        );
        assert_eq!(sound_kind_class("success"), None, "the stub kinds are gone");
        assert_eq!(sound_kind_class(""), None);
    }

    #[test]
    fn ack_token_rides_any_command() {
        let control = parse_control("ratty:mode;3d&tok=abc-123").expect("ratty namespace");
        assert_eq!(control.ack_token.as_deref(), Some("abc-123"));
        assert_eq!(
            control.command,
            Some(RattyAiCommand::SetMode {
                mode: "3d".to_string()
            })
        );
        // `tok=` never collides with `id=`: object commands keep their id.
        let control = parse_control("ratty:object.add;id=2147483648&path=rat.obj&tok=t1")
            .expect("ratty namespace");
        assert_eq!(control.ack_token.as_deref(), Some("t1"));
        assert!(matches!(
            control.command,
            Some(RattyAiCommand::SpawnObject {
                id: 0x8000_0000,
                ..
            })
        ));
    }

    #[test]
    fn ack_token_survives_command_parse_failure() {
        // Unknown action: no command, but the ack token is recovered so the
        // terminal can reply with an error ack.
        let control = parse_control("ratty:teleport;x=1&tok=t2").expect("ratty namespace");
        assert!(control.command.is_none());
        assert_eq!(control.ack_token.as_deref(), Some("t2"));
        // Missing required key: same.
        let control = parse_control("ratty:object.add;tok=t3").expect("ratty namespace");
        assert!(control.command.is_none());
        assert_eq!(control.ack_token.as_deref(), Some("t3"));
    }

    #[test]
    fn invalid_ack_tokens_are_dropped() {
        let control = parse_control("ratty:reset;tok=has%20space").expect("ratty namespace");
        assert!(control.ack_token.is_none());
        assert_eq!(control.command, Some(RattyAiCommand::Reset));
    }

    #[test]
    fn every_documented_action_parses() {
        // A representative payload per action; guards against a missing arm.
        let cases = [
            "ratty:mode;3d",
            "ratty:warp;intensity=0.3",
            "ratty:object.add;id=2147483648&path=a.obj",
            "ratty:object.remove;id=1",
            "ratty:object.clear",
            "ratty:object.update;id=1",
            "ratty:cursor;spin=2",
            "ratty:reset",
            "ratty:flash;color=%23ff0000&duration=1.0",
            "ratty:pulse;intensity=0.8",
            "ratty:tint;color=%230000ff&opacity=0.1",
            "ratty:think;state=start",
            "ratty:confidence;level=0.9",
            "ratty:mood;mood=excited",
            "ratty:viz.set;id=2147483648&kind=ps.v1&data=e30",
            "ratty:viz.effect;id=2147483648&key=1234&effect=died",
            "ratty:viz.remove;id=2147483648",
            "ratty:pane.split;direction=vertical&ratio=0.3",
            "ratty:pane.focus;pane=2",
            "ratty:pane.resize;pane=1&width=80",
            "ratty:pane.close;pane=2",
            "ratty:bookmark;name=x",
            "ratty:bookmark;name=x&mode=replace",
            "ratty:bookmark.jump;name=x",
            "ratty:user.join;id=alice",
            "ratty:user.join;id=alice&name=Alice%20W&color=%2300ffcc&ttl=120&replace=true",
            "ratty:user.renew;id=alice",
            "ratty:user.renew;id=alice&ttl=30",
            "ratty:user.cursor;id=alice&x=1&y=2",
            "ratty:user.leave;id=alice",
            "ratty:note;id=n1&text=hi&x=1&y=2",
            "ratty:note;id=n1&text=hi&x=1&y=2&ttl=600&replace=true",
            "ratty:note.remove;id=n1",
            "ratty:sound.play;kind=chime",
            "ratty:sound.play;kind=click&gain=0.4",
            "ratty:sound.ambient.set;kind=ambient.hum&xfade=800",
            "ratty:sound.ambient.stop",
            "ratty:sound.ambient.stop;fade=250",
            "ratty:avatar.set;model=a.glb",
            "ratty:avatar.gesture;gesture=point",
            "ratty:avatar.speak;text=hi",
            "ratty:avatar.hide",
            "ratty:macro.record;name=deploy",
            "ratty:macro.record;name=deploy&mode=replace",
            "ratty:macro.stop",
            "ratty:macro.play;name=deploy",
            "ratty:macro.play;name=deploy&rate=2.0&mode=instant&scope=session",
            "ratty:macro.play;name=deploy&scope=trusted",
            "ratty:macro.play;hash=deadbeef",
            "ratty:macro.export;name=deploy&to=d.ratty",
            "ratty:macro.run;path=d.ratty",
            "ratty:rule.set;name=cpu-alarm&sensor=sys.cpu&above=85&do=flash",
            "ratty:rule.set;name=quiet&sensor=agent.0.tokens&below=5&clear=10&debounce=2&cooldown=30&do=sound.play%3Bkind%3Dchime",
            "ratty:rule.set;name=cpu-alarm&sensor=sys.cpu&above=85&mode=replace&do=think",
            "ratty:rule.remove;name=cpu-alarm",
            "ratty:rule.enable;name=cpu-alarm",
            "ratty:rule.disable;name=cpu-alarm",
            "ratty:sensor.publish;name=tokens&value=42.5",
            "ratty:sensor.publish;name=tokens&value=42.5&ttl=30&seq=7",
            "ratty:sensor.remove;name=tokens",
        ];
        for case in cases {
            assert!(parse_command(case).is_some(), "failed to parse `{case}`");
        }
    }

    /// The M3.5 stub actions retired by #20 and the `react` stub retired by
    /// #21 fail parse like any unknown action (a `tok=` caller gets
    /// `bad-command`), and the bookmark `mode=` qualifier is a closed
    /// vocabulary.
    #[test]
    fn retired_actions_and_bad_bookmark_modes_fail_parse() {
        for case in [
            "ratty:chart;data=%5B1%2C2%5D",
            "ratty:timeline;input=x",
            "ratty:screenshot;path=s.png",
            "ratty:history;last=50",
            "ratty:jump;name=x",
            "ratty:react;effect=warp-intense&cpu_high=90",
            "ratty:bookmark;name=x&mode=append",
            // macro.play `mode`/`scope` are closed vocabularies, and one of
            // name= or hash= is required.
            "ratty:macro.play",
            "ratty:macro.play;name=x&mode=fast",
            "ratty:macro.play;name=x&scope=global",
            // macro.record `mode` is the same closed vocabulary as bookmark.
            "ratty:macro.record;name=x&mode=append",
        ] {
            assert!(parse_command(case).is_none(), "`{case}` must not parse");
        }
        let stored = parse_command("ratty:bookmark;name=dock&mode=replace").expect("parses");
        assert_eq!(
            stored,
            RattyAiCommand::Bookmark {
                name: "dock".to_string(),
                replace: true,
            }
        );
        let jump = parse_command("ratty:bookmark.jump;name=dock").expect("parses");
        assert_eq!(
            jump,
            RattyAiCommand::BookmarkJump {
                name: "dock".to_string(),
            }
        );
    }

    #[test]
    fn macro_play_parses_rate_mode_scope_and_hash() {
        let full =
            parse_command("ratty:macro.play;name=deploy&rate=0.5&mode=instant&scope=trusted")
                .expect("parses");
        assert_eq!(
            full,
            RattyAiCommand::MacroPlay {
                name: "deploy".to_string(),
                hash: None,
                rate: 0.5,
                instant: true,
                scope: Some(MacroScope::Trusted),
            }
        );
        // Bare name: recorded tempo, no scope, one full run.
        let bare = parse_command("ratty:macro.play;name=deploy").expect("parses");
        assert_eq!(
            bare,
            RattyAiCommand::MacroPlay {
                name: "deploy".to_string(),
                hash: None,
                rate: 1.0,
                instant: false,
                scope: None,
            }
        );
        // Hash addresses the macro directly; name is empty.
        let by_hash = parse_command("ratty:macro.play;hash=deadbeef").expect("parses");
        assert_eq!(
            by_hash,
            RattyAiCommand::MacroPlay {
                name: String::new(),
                hash: Some("deadbeef".to_string()),
                rate: 1.0,
                instant: false,
                scope: None,
            }
        );
    }

    #[test]
    fn command_classes_gate_recording_and_privilege() {
        // Macro-control, reactive control (rule.*/sensor.*), and
        // collaboration presence (user.*/note*) are control-plane: never
        // captured into a recording (#16, #21, #25).
        for control in [
            parse_command("ratty:macro.record;name=x").expect("parses"),
            parse_command("ratty:macro.stop").expect("parses"),
            parse_command("ratty:macro.play;name=x").expect("parses"),
            parse_command("ratty:macro.export;name=x&to=x").expect("parses"),
            parse_command("ratty:macro.run;path=x").expect("parses"),
            parse_command("ratty:rule.set;name=x&sensor=sys.cpu&above=85&do=flash")
                .expect("parses"),
            parse_command("ratty:rule.remove;name=x").expect("parses"),
            parse_command("ratty:rule.enable;name=x").expect("parses"),
            parse_command("ratty:rule.disable;name=x").expect("parses"),
            parse_command("ratty:sensor.publish;name=x&value=1").expect("parses"),
            parse_command("ratty:sensor.remove;name=x").expect("parses"),
            parse_command("ratty:user.join;id=alice").expect("parses"),
            parse_command("ratty:user.renew;id=alice").expect("parses"),
            parse_command("ratty:user.cursor;id=alice&x=1&y=2").expect("parses"),
            parse_command("ratty:user.leave;id=alice").expect("parses"),
            parse_command("ratty:note;id=n1&text=hi&x=1&y=2").expect("parses"),
            parse_command("ratty:note.remove;id=n1").expect("parses"),
        ] {
            assert!(control.is_control_plane(), "{control:?} is control-plane");
        }
        // Ordinary choreography records.
        let spawn = parse_command("ratty:object.add;id=2147483648&path=rat.obj").expect("parses");
        assert!(!spawn.is_control_plane(), "object.add records");
        assert!(!spawn.is_scene_global(), "object.add stays namespaced");
        // Scene-global commands mark a recording privileged — including
        // avatar scene control (#23).
        for global in [
            parse_command("ratty:mode;3d").expect("parses"),
            parse_command("ratty:warp;intensity=0.5").expect("parses"),
            parse_command("ratty:reset").expect("parses"),
            parse_command("ratty:avatar.set;model=mascot").expect("parses"),
            parse_command("ratty:avatar.show").expect("parses"),
            parse_command("ratty:avatar.hide").expect("parses"),
            parse_command("ratty:avatar.speech.clear;ns=0").expect("parses"),
            parse_command("ratty:avatar.speech.clear;scope=all").expect("parses"),
        ] {
            assert!(global.is_scene_global(), "{global:?} is scene-global");
        }
        // Ownership-scoped avatar commands stay unprivileged choreography.
        for own in [
            parse_command("ratty:avatar.speak;text=hi").expect("parses"),
            parse_command("ratty:avatar.gesture;gesture=bob").expect("parses"),
            parse_command("ratty:avatar.speech.clear").expect("parses"),
        ] {
            assert!(!own.is_scene_global(), "{own:?} stays ownership-scoped");
            assert!(!own.is_control_plane(), "{own:?} records");
        }
        // Execution control addresses session-scoped handles: skipped by
        // the recorder tap and refused by the trusted loader (#18).
        for exec in [
            parse_command("ratty:avatar.stop").expect("parses"),
            parse_command("ratty:avatar.cancel;id=abc-1").expect("parses"),
        ] {
            assert!(exec.is_execution_control(), "{exec:?} is execution control");
        }
        // No avatar command is in the reactive-rule allowlist (#21).
        for avatar in [
            "ratty:avatar.set;model=mascot",
            "ratty:avatar.show",
            "ratty:avatar.hide",
            "ratty:avatar.gesture;gesture=bob",
            "ratty:avatar.speak;text=hi",
            "ratty:avatar.stop",
            "ratty:avatar.cancel;id=abc-1",
            "ratty:avatar.speech.clear",
        ] {
            let command = parse_command(avatar).expect("parses");
            assert!(
                !command.is_rule_safe_action(),
                "{command:?} must not be rule-safe"
            );
        }
    }

    #[test]
    fn avatar_wire_grammar_parses_and_rejects_strictly() {
        // Partial update: every key optional.
        assert_eq!(
            parse_command("ratty:avatar.set"),
            Some(RattyAiCommand::AvatarSet {
                model: None,
                position: None,
                dx: None,
                dy: None,
                scale: None,
            })
        );
        assert_eq!(
            parse_command(
                "ratty:avatar.set;model=mascot&position=bottom-right&dx=12&dy=-4&scale=2"
            ),
            Some(RattyAiCommand::AvatarSet {
                model: Some("mascot".to_string()),
                position: Some("bottom-right".to_string()),
                dx: Some(12.0),
                dy: Some(-4.0),
                scale: Some(2.0),
            })
        );
        // A typo'd numeric field is a bad command, never silently absent.
        assert_eq!(parse_command("ratty:avatar.set;dx=abc"), None);
        assert_eq!(
            parse_command("ratty:avatar.speak;text=hi&duration=zzz"),
            None
        );
        // speak requires text; cancel requires id; gesture requires gesture.
        assert_eq!(parse_command("ratty:avatar.speak"), None);
        assert_eq!(parse_command("ratty:avatar.cancel"), None);
        assert_eq!(parse_command("ratty:avatar.gesture"), None);
        // speech.clear: ns= and scope= are mutually exclusive; scope only
        // accepts `all`; ns must be a u8.
        assert_eq!(
            parse_command("ratty:avatar.speech.clear;ns=3"),
            Some(RattyAiCommand::AvatarSpeechClear {
                scope: SpeechClearScope::Namespace(3),
            })
        );
        assert_eq!(
            parse_command("ratty:avatar.speech.clear;ns=3&scope=all"),
            None
        );
        assert_eq!(parse_command("ratty:avatar.speech.clear;scope=some"), None);
        assert_eq!(parse_command("ratty:avatar.speech.clear;ns=999"), None);
    }

    #[test]
    fn rule_set_parses_trigger_action_and_closed_vocabularies() {
        // The nested `do=` action rides percent-encoded and re-parses under
        // the ordinary wire grammar.
        let rule = parse_command(
            "ratty:rule.set;name=cpu-alarm&sensor=sys.cpu&above=85&clear=70&debounce=3&cooldown=30&do=flash%3Bcolor%3D%2523ff0000%26duration%3D0.4",
        )
        .expect("parses");
        assert_eq!(
            rule,
            RattyAiCommand::RuleSet {
                name: "cpu-alarm".to_string(),
                sensor: "sys.cpu".to_string(),
                above: Some(85.0),
                below: None,
                clear: Some(70.0),
                debounce: Some(3.0),
                cooldown: Some(30.0),
                replace: false,
                action: Box::new(RattyAiCommand::Flash {
                    color: "#ff0000".to_string(),
                    duration: 0.4,
                }),
            }
        );
        for bad in [
            // The trigger requires exactly one comparison side.
            "ratty:rule.set;name=x&sensor=sys.cpu&do=flash",
            "ratty:rule.set;name=x&sensor=sys.cpu&above=85&below=5&do=flash",
            // Trigger numbers parse strictly — a typo is a bad command.
            "ratty:rule.set;name=x&sensor=sys.cpu&above=8O&do=flash",
            "ratty:rule.set;name=x&sensor=sys.cpu&above=85&debounce=fast&do=flash",
            "ratty:rule.set;name=x&sensor=sys.cpu&above=85&mode=append&do=flash",
            // The action is required and must itself parse.
            "ratty:rule.set;name=x&sensor=sys.cpu&above=85",
            "ratty:rule.set;name=x&sensor=sys.cpu&above=85&do=warpspeed",
            // Rules cannot create or modify rules, or feed sensors — the
            // rule.*/sensor.* families are rejected by name before the
            // recursive parse (this also bounds `do=` nesting at one).
            "ratty:rule.set;name=x&sensor=sys.cpu&above=85&do=rule.remove%3Bname%3Dx",
            "ratty:rule.set;name=x&sensor=sys.cpu&above=85&do=sensor.publish%3Bname%3Dx%26value%3D1",
            // Strict sequence numbers on sensor.publish.
            "ratty:sensor.publish;name=x&value=1&seq=abc",
            "ratty:sensor.publish;name=x&value=hot",
            "ratty:sensor.publish;name=x",
        ] {
            assert!(parse_command(bad).is_none(), "`{bad}` must not parse");
        }
        // `macro.play` parses as a rule action (its rule-safety is registry
        // state validated at rule.set and fire time, not parse state).
        let indirect = parse_command(
            "ratty:rule.set;name=x&sensor=sys.cpu&above=85&do=macro.play%3Bname%3Dcelebrate",
        )
        .expect("parses");
        let RattyAiCommand::RuleSet { action, .. } = indirect else {
            panic!("rule.set parses");
        };
        assert!(matches!(*action, RattyAiCommand::MacroPlay { .. }));
    }

    #[test]
    fn rule_safe_action_class_is_pinned() {
        for safe in [
            "ratty:flash;color=%23ff0000",
            "ratty:pulse;intensity=0.5",
            "ratty:tint;color=%230000ff",
            "ratty:think;state=start",
            "ratty:confidence;level=0.9",
            "ratty:mood;mood=excited",
            "ratty:sound.play;kind=chime",
            "ratty:viz.effect;id=2147483648&key=k&effect=highlight",
            "ratty:object.update;id=2147483648&spin=2&scale=1.5&brightness=0.8",
        ] {
            let command = parse_command(safe).expect("parses");
            assert!(command.is_rule_safe_action(), "{command:?} is rule-safe");
        }
        for unsafe_ in [
            // Re-anchor is respawn-class: x/y disqualify object.update.
            "ratty:object.update;id=2147483648&x=10",
            "ratty:object.add;id=2147483648&path=rat.obj",
            "ratty:object.remove;id=2147483648",
            "ratty:object.clear",
            "ratty:viz.set;id=2147483648&kind=ps.v1&data=e30",
            "ratty:viz.remove;id=2147483648",
            "ratty:sound.ambient.set;kind=ambient.hum",
            "ratty:mode;3d",
            "ratty:warp;intensity=0.5",
            "ratty:reset",
            "ratty:bookmark.jump;name=x",
            // macro.play is the indirect action: not in the direct class.
            "ratty:macro.play;name=x",
        ] {
            let command = parse_command(unsafe_).expect("parses");
            assert!(
                !command.is_rule_safe_action(),
                "{command:?} is not directly rule-safe"
            );
        }
    }

    #[test]
    fn presence_wire_grammar_parses_and_rejects_strictly() {
        // Bare join: the display name defaults to the id, the color to
        // green, and the lease TTL to the terminal-side default.
        assert_eq!(
            parse_command("ratty:user.join;id=alice"),
            Some(RattyAiCommand::UserJoin {
                id: "alice".to_string(),
                name: "alice".to_string(),
                color: "#00ff00".to_string(),
                ttl: None,
                replace: false,
            })
        );
        // Full join: every optional pinned.
        assert_eq!(
            parse_command(
                "ratty:user.join;id=alice&name=Alice%20W&color=%23ff8800&ttl=120&replace=true"
            ),
            Some(RattyAiCommand::UserJoin {
                id: "alice".to_string(),
                name: "Alice W".to_string(),
                color: "#ff8800".to_string(),
                ttl: Some(120.0),
                replace: true,
            })
        );
        assert_eq!(
            parse_command("ratty:user.renew;id=alice&ttl=30"),
            Some(RattyAiCommand::UserRenew {
                id: "alice".to_string(),
                ttl: Some(30.0),
            })
        );
        assert_eq!(
            parse_command("ratty:user.cursor;id=alice&x=4&y=7"),
            Some(RattyAiCommand::UserCursor {
                id: "alice".to_string(),
                x: 4,
                y: 7,
            })
        );
        assert_eq!(
            parse_command("ratty:user.leave;id=alice"),
            Some(RattyAiCommand::UserLeave {
                id: "alice".to_string(),
            })
        );
        assert_eq!(
            parse_command("ratty:note.remove;id=n1"),
            Some(RattyAiCommand::NoteRemove {
                id: "n1".to_string(),
            })
        );
        for bad in [
            // `id=` is the identity key on every presence command.
            "ratty:user.join",
            "ratty:user.renew",
            "ratty:user.cursor;x=1&y=2",
            "ratty:user.leave",
            "ratty:note;text=hi&x=1&y=2",
            "ratty:note.remove",
            // Numerics parse strictly: a typo'd ttl or coordinate is a
            // bad command, never a silently different one.
            "ratty:user.join;id=alice&ttl=fast",
            "ratty:user.renew;id=alice&ttl=3O",
            "ratty:user.cursor;id=alice&x=abc&y=2",
            "ratty:user.cursor;id=alice&x=1&y=70000",
            "ratty:note;id=n1&text=hi&x=1&y=2&ttl=1h",
            // Cursor cells and note anchors are required, not default-0.
            "ratty:user.cursor;id=alice&y=2",
            "ratty:user.cursor;id=alice&x=1",
            "ratty:note;id=n1&text=hi&y=2",
            "ratty:note;id=n1&text=hi&x=1",
            "ratty:note;id=n1&x=1&y=2",
        ] {
            assert!(parse_command(bad).is_none(), "`{bad}` must not parse");
        }
    }

    /// The M3 `expires=` free-string key is retired: the payload parser
    /// ignores unknown keys, so a stale caller still parses — but only
    /// strict `ttl=` seconds governs the lease.
    #[test]
    fn retired_note_expires_key_is_ignored_and_ttl_governs() {
        assert_eq!(
            parse_command("ratty:note;id=n1&text=hi&x=1&y=2&expires=1h"),
            Some(RattyAiCommand::Note {
                id: "n1".to_string(),
                text: "hi".to_string(),
                x: 1,
                y: 2,
                ttl: None,
                replace: false,
            })
        );
        assert_eq!(
            parse_command("ratty:note;id=n1&text=hi&x=1&y=2&expires=1h&ttl=600"),
            Some(RattyAiCommand::Note {
                id: "n1".to_string(),
                text: "hi".to_string(),
                x: 1,
                y: 2,
                ttl: Some(600.0),
                replace: false,
            })
        );
    }

    /// Pins the collaboration-presence class (#25): control-plane (a
    /// replayed join would forge liveness), and nothing else — not
    /// rule-safe, not scene-global, not execution control.
    #[test]
    fn collaboration_presence_class_is_pinned() {
        for spec in [
            "ratty:user.join;id=alice",
            "ratty:user.renew;id=alice",
            "ratty:user.cursor;id=alice&x=1&y=2",
            "ratty:user.leave;id=alice",
            "ratty:note;id=n1&text=hi&x=1&y=2",
            "ratty:note.remove;id=n1",
        ] {
            let command = parse_command(spec).expect("parses");
            assert!(
                command.is_presence_control(),
                "{command:?} is presence control"
            );
            assert!(command.is_control_plane(), "{command:?} is control-plane");
            assert!(
                !command.is_rule_safe_action(),
                "{command:?} must not be rule-safe"
            );
            assert!(
                !command.is_scene_global(),
                "{command:?} stays namespace-scoped"
            );
            assert!(
                !command.is_execution_control(),
                "{command:?} is not execution control"
            );
        }
        // The effects family older docs call "AI presence" is a different
        // organ: recordable choreography, not presence control.
        for spec in ["ratty:think;state=start", "ratty:mood;mood=excited"] {
            let command = parse_command(spec).expect("parses");
            assert!(
                !command.is_presence_control(),
                "{command:?} is effects choreography, not presence control"
            );
        }
    }
}
