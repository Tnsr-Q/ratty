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

/// Looks up the class of a registered sound kind, or `None` when the kind
/// is not registered.
pub fn sound_kind_class(kind: &str) -> Option<SoundKindClass> {
    SOUND_KINDS
        .iter()
        .find(|(name, _)| *name == kind)
        .map(|(_, class)| *class)
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

    // ── Collaboration ──
    /// A remote user joined.
    UserJoin {
        /// User name.
        name: String,
        /// Cursor color.
        color: String,
    },
    /// A remote user left.
    UserLeave {
        /// User name.
        name: String,
    },
    /// Update a remote user's cursor.
    UserCursor {
        /// User name.
        name: String,
        /// Cursor column.
        x: u16,
        /// Cursor row.
        y: u16,
    },
    /// Place a floating annotation.
    Note {
        /// Note text.
        text: String,
        /// Anchor column.
        x: u16,
        /// Anchor row.
        y: u16,
        /// Expiry (e.g. `1h`).
        expires: String,
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

    // ── Avatar ──
    /// Show the AI avatar.
    AvatarSet {
        /// Avatar model.
        model: String,
        /// Screen position.
        position: String,
    },
    /// Trigger an avatar gesture.
    AvatarGesture {
        /// Gesture name.
        gesture: String,
    },
    /// Make the avatar speak.
    AvatarSpeak {
        /// Speech text.
        text: String,
    },
    /// Hide the avatar.
    AvatarHide,

    // ── Macros ──
    /// Begin recording a macro.
    MacroRecord {
        /// Macro name.
        name: String,
    },
    /// Stop recording.
    MacroStop,
    /// Replay a recorded macro.
    MacroPlay {
        /// Macro name.
        name: String,
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

    // ── Reactive ──
    /// Register a system-metric-driven effect.
    React {
        /// Effect name.
        effect: String,
        /// CPU% threshold.
        cpu_high: Option<f32>,
        /// Memory% threshold.
        memory_high: Option<f32>,
        /// Battery% threshold.
        battery_low: Option<f32>,
    },
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

        // Collaboration
        "user.join" => RattyAiCommand::UserJoin {
            name: p.string("name")?,
            color: p.string_or("color", "#00ff00"),
        },
        "user.leave" => RattyAiCommand::UserLeave {
            name: p.string("name")?,
        },
        "user.cursor" => RattyAiCommand::UserCursor {
            name: p.string("name")?,
            x: p.u16("x", 0),
            y: p.u16("y", 0),
        },
        "note" => RattyAiCommand::Note {
            text: p.string("text")?,
            x: p.u16("x", 0),
            y: p.u16("y", 0),
            expires: p.string_or("expires", "1h"),
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

        // Avatar
        "avatar.set" => RattyAiCommand::AvatarSet {
            model: p.string_or("model", "ai-helper.glb"),
            position: p.string_or("position", "top-right"),
        },
        "avatar.gesture" => RattyAiCommand::AvatarGesture {
            gesture: p.string_or("gesture", "wave"),
        },
        "avatar.speak" => RattyAiCommand::AvatarSpeak {
            text: p.string("text")?,
        },
        "avatar.hide" => RattyAiCommand::AvatarHide,

        // Macros
        "macro.record" => RattyAiCommand::MacroRecord {
            name: p.string("name")?,
        },
        "macro.stop" => RattyAiCommand::MacroStop,
        "macro.play" => RattyAiCommand::MacroPlay {
            name: p.string("name")?,
        },
        "macro.export" => RattyAiCommand::MacroExport {
            name: p.string("name")?,
            to: p.string_or("to", "macro.ratty"),
        },
        "macro.run" => RattyAiCommand::MacroRun {
            path: p.string("path")?,
        },

        // Reactive
        "react" => RattyAiCommand::React {
            effect: p.string("effect")?,
            cpu_high: p.opt("cpu_high"),
            memory_high: p.opt("memory_high"),
            battery_low: p.opt("battery_low"),
        },

        _ => return None,
    })
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
        let payload = format!("text={}&x=15&y=10&expires=1h", percent_encode(text));
        let command = parse_command(&format!("ratty:note;{payload}")).expect("note parses");
        assert_eq!(
            command,
            RattyAiCommand::Note {
                text: text.to_string(),
                x: 15,
                y: 10,
                expires: "1h".to_string(),
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
            "ratty:user.join;name=alice&color=%2300ff00",
            "ratty:user.leave;name=alice",
            "ratty:user.cursor;name=alice&x=1&y=2",
            "ratty:note;text=hi&x=1&y=2",
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
            "ratty:macro.stop",
            "ratty:macro.play;name=deploy",
            "ratty:macro.export;name=deploy&to=d.ratty",
            "ratty:macro.run;path=d.ratty",
            "ratty:react;effect=warp-intense&cpu_high=90",
        ];
        for case in cases {
            assert!(parse_command(case).is_some(), "failed to parse `{case}`");
        }
    }

    /// The M3.5 stub actions retired by #20 fail parse like any unknown
    /// action (a `tok=` caller gets `bad-command`), and the bookmark
    /// `mode=` qualifier is a closed vocabulary.
    #[test]
    fn retired_actions_and_bad_bookmark_modes_fail_parse() {
        for case in [
            "ratty:chart;data=%5B1%2C2%5D",
            "ratty:timeline;input=x",
            "ratty:screenshot;path=s.png",
            "ratty:history;last=50",
            "ratty:jump;name=x",
            "ratty:bookmark;name=x&mode=append",
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
}
