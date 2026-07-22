//! `ratty-ai` — pure-CLI control for the Ratty terminal emulator.
//!
//! No sockets, no daemon, no temp files. Each subcommand prints one OSC 777
//! escape sequence to stdout; Ratty intercepts it and acts. Because it is
//! just stdout, it composes with the whole shell — `make && ratty-ai flash
//! green || ratty-ai flash red` — works over SSH, and the identical bytes
//! drive the browser build through `feed()`.
//!
//! The wire format and its encoding live in the terminal's own `src/osc.rs`
//! (OSC 777 commands) and `src/query.rs` (OSC 778 queries and replies),
//! included here verbatim so the CLI and the terminal share one source of
//! truth (the same trick `tools/silk` uses with `rgp.rs`).
//!
//! `query`/`state` (and any command run with `--ack`) additionally *read*:
//! they open the controlling tty raw, emit the sequence there, and wait for
//! the correlated OSC 778 reply. Exit codes for those paths are stable:
//! `0` success · `2` bad arguments/input JSON · `3` timeout · `4` malformed
//! reply · `5` the terminal returned `ok=0` · `6` tty/transport failure.

use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

#[allow(dead_code)] // The CLI uses the encoder half; the parser half is exercised by tests.
#[path = "../../../src/osc.rs"]
mod osc;

#[allow(dead_code)] // Shared wire module; the CLI uses the client half.
#[path = "../../../src/query.rs"]
mod query;

/// Exit codes for the reply-reading paths (`query`, `state`, `--ack`):
/// `0` success, `2` bad arguments/input JSON (clap usage errors also exit
/// 2), `3` timeout, `4` malformed reply, `5` the terminal answered `ok=0`,
/// `6` tty/transport failure.
mod exit_codes {
    use std::process::ExitCode;

    pub const OK: ExitCode = ExitCode::SUCCESS;

    pub fn bad_input() -> ExitCode {
        ExitCode::from(2)
    }

    pub fn timeout() -> ExitCode {
        ExitCode::from(3)
    }

    pub fn malformed_reply() -> ExitCode {
        ExitCode::from(4)
    }

    pub fn reply_error() -> ExitCode {
        ExitCode::from(5)
    }

    pub fn transport() -> ExitCode {
        ExitCode::from(6)
    }
}

/// AI-facing control client for Ratty's 3D terminal scene.
#[derive(Parser)]
#[command(name = "ratty-ai", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    /// Print the escape sequence in readable form instead of emitting it.
    #[arg(long, global = true)]
    dry_run: bool,
    /// Request a delivery ack (over OSC 778) for this command and wait for
    /// it: exit 0 when the command committed, 5 when it was rejected.
    /// Ignored by `query`/`state`, which always read a reply.
    #[arg(long, global = true)]
    ack: bool,
    /// Reply timeout in milliseconds for `query`, `state`, and `--ack`.
    #[arg(long, global = true, default_value_t = 2000)]
    timeout: u64,
    /// Machine mode: failures print `{"ok":false,"code","message"}` JSON on
    /// stdout (exit codes unchanged).
    #[arg(long, global = true)]
    json: bool,
    /// TTY device to use instead of the controlling terminal.
    #[arg(long, global = true)]
    tty: Option<std::path::PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// Query live terminal state over OSC 778 (`caps`, `state.*`).
    ///
    /// The decoded JSON payload prints on stdout. New 778 ops never grow
    /// new subcommands — discover them with `ratty-ai query caps`.
    Query {
        /// Query op (e.g. `state.scene`, `state.objects`, `caps`).
        op: String,
        /// Inline JSON payload for ops that take parameters.
        #[arg(long, conflicts_with = "data_file")]
        data: Option<String>,
        /// Read the JSON payload from a file, or `-` for stdin.
        #[arg(long)]
        data_file: Option<String>,
        /// Pretty-print the decoded reply.
        #[arg(long)]
        pretty: bool,
    },
    /// Sugar for `query state.<path>`; bare `state` reads `state.scene`.
    State {
        /// State path (`scene`, `objects`, `visible_objects`, `neighbors`,
        /// `namespaces`, `errors`, …).
        path: Option<String>,
        /// Pretty-print the decoded reply.
        #[arg(long)]
        pretty: bool,
    },
    /// Manage inline 3D objects.
    #[command(subcommand)]
    Object(ObjectAction),
    /// Set presentation mode (`2d`, `3d`, `mobius`).
    Mode {
        /// Mode name.
        mode: String,
    },
    /// Set plane warp amount (`0.0`–`1.0`).
    Warp {
        /// Warp intensity.
        intensity: f32,
    },
    /// Flash the surface a color briefly.
    Flash {
        /// `#rrggbb` color.
        #[arg(short, long, default_value = "#ffffff")]
        color: String,
        /// Duration in seconds.
        #[arg(short, long, default_value = "0.5")]
        duration: f32,
    },
    /// Pulse the surface brightness.
    Pulse {
        /// Peak intensity.
        #[arg(short, long, default_value = "0.8")]
        intensity: f32,
        /// Duration in seconds.
        #[arg(short, long, default_value = "1.0")]
        duration: f32,
    },
    /// Tint the surface with a translucent color.
    Tint {
        /// `#rrggbb` color.
        color: String,
        /// Opacity `0.0`–`1.0`.
        #[arg(short, long, default_value = "0.1")]
        opacity: f32,
    },
    /// Update the cursor model and animation.
    Cursor {
        /// Cursor model asset.
        #[arg(short, long)]
        model: Option<String>,
        /// Spin rate.
        #[arg(short, long)]
        spin: Option<f32>,
        /// Bob speed.
        #[arg(long)]
        bob_speed: Option<f32>,
        /// Bob amplitude.
        #[arg(long)]
        bob_amp: Option<f32>,
        /// Brightness.
        #[arg(long)]
        brightness: Option<f32>,
        /// Visibility.
        #[arg(long)]
        visible: Option<bool>,
    },
    /// Reset the scene to defaults.
    Reset,
    /// Capture a screenshot (handler pending).
    Screenshot {
        /// Output path.
        #[arg(short, long, default_value = "ratty-screenshot.png")]
        output: String,
    },
    /// Render inline data as a chart (data from `--data` or stdin).
    Chart {
        /// Chart kind.
        #[arg(short, long, default_value = "bar")]
        kind: String,
        /// Anchor column.
        #[arg(short, long, default_value = "0")]
        x: u16,
        /// Anchor row.
        #[arg(short, long, default_value = "0")]
        y: u16,
        /// Scale.
        #[arg(short, long, default_value = "1.0")]
        scale: f32,
        /// Inline data; reads stdin when omitted.
        #[arg(short, long)]
        data: Option<String>,
    },
    /// Render piped input as a 3D timeline (reads stdin).
    Timeline {
        /// Anchor column.
        #[arg(short, long, default_value = "0")]
        x: u16,
        /// Anchor row.
        #[arg(short, long, default_value = "0")]
        y: u16,
        /// Scale.
        #[arg(short, long, default_value = "1.0")]
        scale: f32,
    },
    /// Visualize running processes.
    Ps {
        /// Draw the visualization.
        #[arg(short, long)]
        visualize: bool,
        /// PID to highlight.
        #[arg(long)]
        highlight: Option<u32>,
        /// Highlight color.
        #[arg(long)]
        color: Option<String>,
    },
    /// Kill a process with a visual effect.
    Kill {
        /// PID.
        pid: u32,
        /// Effect (`explode`/`shrink`/`dissolve`).
        #[arg(short, long, default_value = "explode")]
        effect: String,
    },
    /// Enter a directory as a 3D space.
    Cd {
        /// Target path.
        path: String,
        /// Draw the visualization.
        #[arg(short, long)]
        visualize: bool,
    },
    /// List a directory as floating icons.
    Ls {
        /// Target path.
        #[arg(short, long, default_value = ".")]
        path: String,
        /// Draw the visualization.
        #[arg(short, long)]
        visualize: bool,
    },
    /// Render a directory tree as branching 3D structure.
    Tree {
        /// Recursion depth.
        #[arg(short, long, default_value = "3")]
        depth: u8,
        /// Draw the visualization.
        #[arg(short, long)]
        visualize: bool,
    },
    /// Git visualizations.
    #[command(subcommand)]
    Git(GitAction),
    /// Visualize network connections.
    Net {
        /// Draw the visualization.
        #[arg(short, long)]
        visualize: bool,
        /// Specific host.
        #[arg(long)]
        host: Option<String>,
    },
    /// Show the AI is thinking.
    Think {
        /// Begin thinking.
        #[arg(short, long)]
        start: bool,
        /// End thinking.
        #[arg(short, long)]
        end: bool,
    },
    /// Set the AI confidence level (`0.0`–`1.0`).
    Confidence {
        /// Confidence.
        level: f32,
    },
    /// Set the AI mood.
    Mood {
        /// Mood.
        #[arg(value_enum)]
        mood: MoodArg,
    },
    /// Split the terminal into panes.
    Split {
        /// `vertical` or `horizontal`.
        #[arg(short, long, default_value = "vertical")]
        direction: String,
        /// Split ratio.
        #[arg(short, long, default_value = "0.5")]
        ratio: f32,
    },
    /// Focus a pane.
    Focus {
        /// Pane id.
        pane: u8,
    },
    /// Resize a pane.
    Resize {
        /// Pane id.
        pane: u8,
        /// New width in cells.
        #[arg(short, long)]
        width: Option<u16>,
        /// New height in cells.
        #[arg(long)]
        height: Option<u16>,
    },
    /// Close a pane.
    Close {
        /// Pane id.
        pane: u8,
    },
    /// Visualize command history.
    History {
        /// How many recent entries.
        #[arg(short, long, default_value = "50")]
        last: usize,
        /// Draw the visualization.
        #[arg(short, long)]
        visualize: bool,
    },
    /// Bookmark the current state.
    Bookmark {
        /// Bookmark name.
        #[arg(short, long)]
        name: String,
    },
    /// Jump to a bookmark.
    Jump {
        /// Bookmark name.
        name: String,
    },
    /// Remote-user presence.
    #[command(subcommand)]
    User(UserAction),
    /// Place a floating annotation.
    Note {
        /// Note text.
        text: String,
        /// Anchor column.
        #[arg(short, long, default_value = "0")]
        x: u16,
        /// Anchor row.
        #[arg(short, long, default_value = "0")]
        y: u16,
        /// Expiry (e.g. `1h`).
        #[arg(short, long, default_value = "1h")]
        expires: String,
    },
    /// Play a sound.
    Sound {
        /// Sound kind.
        #[arg(value_enum)]
        kind: SoundKind,
        /// Loop the sound.
        #[arg(short, long)]
        loop_sound: bool,
    },
    /// AI presence avatar.
    #[command(subcommand)]
    Avatar(AvatarAction),
    /// Record and replay command macros.
    #[command(subcommand)]
    Macro(MacroAction),
    /// Register a system-metric-driven effect.
    React {
        /// CPU% threshold.
        #[arg(long)]
        on_cpu_high: Option<f32>,
        /// Memory% threshold.
        #[arg(long)]
        on_memory_high: Option<f32>,
        /// Battery% threshold.
        #[arg(long)]
        on_battery_low: Option<f32>,
        /// Effect name.
        #[arg(short, long)]
        effect: String,
    },
}

#[derive(Subcommand)]
enum ObjectAction {
    /// Place an object from an embedded asset name.
    Add {
        /// Object id (decimal). Must lie in the AI-owned range — ids from
        /// 2147483648 (the high bit) upward — within the caller's namespace.
        #[arg(long)]
        id: u32,
        /// Embedded asset name (wire commands never resolve filesystem
        /// paths).
        #[arg(short, long)]
        path: String,
        /// Anchor column.
        #[arg(short, long, default_value = "0")]
        x: u16,
        /// Anchor row.
        #[arg(short, long, default_value = "0")]
        y: u16,
        /// Uniform scale.
        #[arg(short, long, default_value = "1.0")]
        scale: f32,
        /// Spin rate.
        #[arg(long, default_value = "0.0")]
        spin: f32,
        /// Brightness.
        #[arg(long, default_value = "1.0")]
        brightness: f32,
        /// Replace a live object under the same id (ids are otherwise never
        /// reused within a session).
        #[arg(long)]
        replace: bool,
    },
    /// Remove an object by id.
    Remove {
        /// Object id.
        id: u32,
    },
    /// Remove every object.
    Clear,
    /// Update an object's transform/style.
    Update {
        /// Object id.
        id: u32,
        /// New anchor column.
        #[arg(short, long)]
        x: Option<u16>,
        /// New anchor row.
        #[arg(short, long)]
        y: Option<u16>,
        /// New scale.
        #[arg(short, long)]
        scale: Option<f32>,
        /// New spin.
        #[arg(long)]
        spin: Option<f32>,
        /// New brightness.
        #[arg(long)]
        brightness: Option<f32>,
    },
}

#[derive(Subcommand)]
enum GitAction {
    /// Branches as 3D rivers.
    Branch {
        /// Draw the visualization.
        #[arg(short, long)]
        visualize: bool,
    },
    /// Diff as before/after.
    Diff {
        /// Draw the visualization.
        #[arg(short, long)]
        visualize: bool,
    },
    /// Merge visualization.
    Merge {
        /// Draw the visualization.
        #[arg(short, long)]
        visualize: bool,
    },
    /// Stash as a compressed cube.
    Stash {
        /// Draw the visualization.
        #[arg(short, long)]
        visualize: bool,
    },
}

#[derive(Subcommand)]
enum UserAction {
    /// A user joins.
    Join {
        /// User name.
        #[arg(short, long)]
        name: String,
        /// Cursor color.
        #[arg(short, long, default_value = "#00ff00")]
        color: String,
    },
    /// A user leaves.
    Leave {
        /// User name.
        #[arg(short, long)]
        name: String,
    },
    /// Move a user's cursor.
    Cursor {
        /// User name.
        #[arg(short, long)]
        name: String,
        /// Cursor column.
        #[arg(short, long)]
        x: u16,
        /// Cursor row.
        #[arg(short, long)]
        y: u16,
    },
}

#[derive(Subcommand)]
enum AvatarAction {
    /// Show the avatar.
    Set {
        /// Avatar model.
        #[arg(short, long, default_value = "ai-helper.glb")]
        model: String,
        /// Screen position.
        #[arg(short, long, default_value = "top-right")]
        position: String,
    },
    /// Trigger a gesture.
    Gesture {
        /// Gesture.
        #[arg(value_enum)]
        gesture: GestureArg,
    },
    /// Speak text.
    Speak {
        /// Speech text.
        text: String,
    },
    /// Hide the avatar.
    Hide,
}

#[derive(Subcommand)]
enum MacroAction {
    /// Begin recording.
    Record {
        /// Macro name.
        #[arg(short, long)]
        name: String,
    },
    /// Stop recording.
    Stop,
    /// Replay a macro.
    Play {
        /// Macro name.
        name: String,
    },
    /// Export a macro to a file.
    Export {
        /// Macro name.
        #[arg(short, long)]
        name: String,
        /// Destination path.
        #[arg(short, long, default_value = "macro.ratty")]
        to: String,
    },
    /// Run a macro file.
    Run {
        /// Macro file path.
        path: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum MoodArg {
    Excited,
    Cautious,
    Confused,
    Focused,
    Celebratory,
}

#[derive(Clone, Copy, ValueEnum)]
enum SoundKind {
    Click,
    Error,
    Success,
    Warning,
    Ambient,
    Notify,
}

#[derive(Clone, Copy, ValueEnum)]
enum GestureArg {
    Point,
    Think,
    Celebrate,
    Wave,
    Nod,
    Shake,
}

/// Accumulates a `k=v&…` payload with every value percent-encoded, so the
/// terminal's split-then-decode parser recovers each value intact.
#[derive(Default)]
struct Payload(Vec<String>);

impl Payload {
    fn field(mut self, key: &str, value: impl std::fmt::Display) -> Self {
        self.0
            .push(format!("{key}={}", osc::percent_encode(&value.to_string())));
        self
    }

    fn opt(mut self, key: &str, value: Option<impl std::fmt::Display>) -> Self {
        if let Some(value) = value {
            self.0
                .push(format!("{key}={}", osc::percent_encode(&value.to_string())));
        }
        self
    }

    fn build(self) -> String {
        self.0.join("&")
    }
}

/// Maps a parsed CLI command to its `(action, payload)`. `stdin` supplies
/// piped input for `chart`/`timeline`.
fn command_to_osc(command: &Commands, stdin: &str) -> (String, String) {
    let p = Payload::default;
    match command {
        Commands::Query { .. } | Commands::State { .. } => {
            unreachable!("query/state are handled before command_to_osc")
        }
        Commands::Object(action) => match action {
            ObjectAction::Add {
                id,
                path,
                x,
                y,
                scale,
                spin,
                brightness,
                replace,
            } => (
                "object.add".into(),
                p().field("id", id)
                    .field("path", path)
                    .field("x", x)
                    .field("y", y)
                    .field("scale", scale)
                    .field("spin", spin)
                    .field("brightness", brightness)
                    .opt("replace", replace.then_some("true"))
                    .build(),
            ),
            ObjectAction::Remove { id } => ("object.remove".into(), p().field("id", id).build()),
            ObjectAction::Clear => ("object.clear".into(), String::new()),
            ObjectAction::Update {
                id,
                x,
                y,
                scale,
                spin,
                brightness,
            } => (
                "object.update".into(),
                p().field("id", id)
                    .opt("x", *x)
                    .opt("y", *y)
                    .opt("scale", *scale)
                    .opt("spin", *spin)
                    .opt("brightness", *brightness)
                    .build(),
            ),
        },
        Commands::Mode { mode } => ("mode".into(), osc::percent_encode(mode)),
        Commands::Warp { intensity } => ("warp".into(), p().field("intensity", intensity).build()),
        Commands::Flash { color, duration } => (
            "flash".into(),
            p().field("color", color)
                .field("duration", duration)
                .build(),
        ),
        Commands::Pulse {
            intensity,
            duration,
        } => (
            "pulse".into(),
            p().field("intensity", intensity)
                .field("duration", duration)
                .build(),
        ),
        Commands::Tint { color, opacity } => (
            "tint".into(),
            p().field("color", color).field("opacity", opacity).build(),
        ),
        Commands::Cursor {
            model,
            spin,
            bob_speed,
            bob_amp,
            brightness,
            visible,
        } => (
            "cursor".into(),
            p().opt("model", model.as_ref())
                .opt("spin", *spin)
                .opt("bob_speed", *bob_speed)
                .opt("bob_amp", *bob_amp)
                .opt("brightness", *brightness)
                .opt("visible", *visible)
                .build(),
        ),
        Commands::Reset => ("reset".into(), String::new()),
        Commands::Screenshot { output } => ("screenshot".into(), p().field("path", output).build()),
        Commands::Chart {
            kind,
            x,
            y,
            scale,
            data,
        } => {
            let data = data.clone().unwrap_or_else(|| stdin.to_string());
            (
                "chart".into(),
                p().field("kind", kind)
                    .field("x", x)
                    .field("y", y)
                    .field("scale", scale)
                    .field("data", data)
                    .build(),
            )
        }
        Commands::Timeline { x, y, scale } => (
            "timeline".into(),
            p().field("x", x)
                .field("y", y)
                .field("scale", scale)
                .field("input", stdin)
                .build(),
        ),
        Commands::Ps {
            visualize,
            highlight,
            color,
        } => (
            "ps".into(),
            p().field("visualize", visualize)
                .opt("highlight", *highlight)
                .opt("color", color.as_ref())
                .build(),
        ),
        Commands::Kill { pid, effect } => (
            "kill".into(),
            p().field("pid", pid).field("effect", effect).build(),
        ),
        Commands::Cd { path, visualize } => (
            "cd".into(),
            p().field("path", path)
                .field("visualize", visualize)
                .build(),
        ),
        Commands::Ls { path, visualize } => (
            "ls".into(),
            p().field("path", path)
                .field("visualize", visualize)
                .build(),
        ),
        Commands::Tree { depth, visualize } => (
            "tree".into(),
            p().field("depth", depth)
                .field("visualize", visualize)
                .build(),
        ),
        Commands::Git(action) => match action {
            GitAction::Branch { visualize } => (
                "git.branch".into(),
                p().field("visualize", visualize).build(),
            ),
            GitAction::Diff { visualize } => {
                ("git.diff".into(), p().field("visualize", visualize).build())
            }
            GitAction::Merge { visualize } => (
                "git.merge".into(),
                p().field("visualize", visualize).build(),
            ),
            GitAction::Stash { visualize } => (
                "git.stash".into(),
                p().field("visualize", visualize).build(),
            ),
        },
        Commands::Net { visualize, host } => (
            "net".into(),
            p().field("visualize", visualize)
                .opt("host", host.as_ref())
                .build(),
        ),
        Commands::Think { start, end } => {
            let state = if *start {
                "start"
            } else if *end {
                "end"
            } else {
                "toggle"
            };
            ("think".into(), p().field("state", state).build())
        }
        Commands::Confidence { level } => (
            "confidence".into(),
            p().field("level", level.clamp(0.0, 1.0)).build(),
        ),
        Commands::Mood { mood } => ("mood".into(), p().field("mood", mood_str(*mood)).build()),
        Commands::Split { direction, ratio } => (
            "pane.split".into(),
            p().field("direction", direction)
                .field("ratio", ratio)
                .build(),
        ),
        Commands::Focus { pane } => ("pane.focus".into(), p().field("pane", pane).build()),
        Commands::Resize {
            pane,
            width,
            height,
        } => (
            "pane.resize".into(),
            p().field("pane", pane)
                .opt("width", *width)
                .opt("height", *height)
                .build(),
        ),
        Commands::Close { pane } => ("pane.close".into(), p().field("pane", pane).build()),
        Commands::History { last, visualize } => (
            "history".into(),
            p().field("last", last)
                .field("visualize", visualize)
                .build(),
        ),
        Commands::Bookmark { name } => ("bookmark".into(), p().field("name", name).build()),
        Commands::Jump { name } => ("jump".into(), p().field("name", name).build()),
        Commands::User(action) => match action {
            UserAction::Join { name, color } => (
                "user.join".into(),
                p().field("name", name).field("color", color).build(),
            ),
            UserAction::Leave { name } => ("user.leave".into(), p().field("name", name).build()),
            UserAction::Cursor { name, x, y } => (
                "user.cursor".into(),
                p().field("name", name).field("x", x).field("y", y).build(),
            ),
        },
        Commands::Note {
            text,
            x,
            y,
            expires,
        } => (
            "note".into(),
            p().field("text", text)
                .field("x", x)
                .field("y", y)
                .field("expires", expires)
                .build(),
        ),
        Commands::Sound { kind, loop_sound } => (
            "sound".into(),
            p().field("kind", sound_str(*kind))
                .field("loop", loop_sound)
                .build(),
        ),
        Commands::Avatar(action) => match action {
            AvatarAction::Set { model, position } => (
                "avatar.set".into(),
                p().field("model", model)
                    .field("position", position)
                    .build(),
            ),
            AvatarAction::Gesture { gesture } => (
                "avatar.gesture".into(),
                p().field("gesture", gesture_str(*gesture)).build(),
            ),
            AvatarAction::Speak { text } => {
                ("avatar.speak".into(), p().field("text", text).build())
            }
            AvatarAction::Hide => ("avatar.hide".into(), String::new()),
        },
        Commands::Macro(action) => match action {
            MacroAction::Record { name } => {
                ("macro.record".into(), p().field("name", name).build())
            }
            MacroAction::Stop => ("macro.stop".into(), String::new()),
            MacroAction::Play { name } => ("macro.play".into(), p().field("name", name).build()),
            MacroAction::Export { name, to } => (
                "macro.export".into(),
                p().field("name", name).field("to", to).build(),
            ),
            MacroAction::Run { path } => ("macro.run".into(), p().field("path", path).build()),
        },
        Commands::React {
            on_cpu_high,
            on_memory_high,
            on_battery_low,
            effect,
        } => (
            "react".into(),
            p().field("effect", effect)
                .opt("cpu_high", *on_cpu_high)
                .opt("memory_high", *on_memory_high)
                .opt("battery_low", *on_battery_low)
                .build(),
        ),
    }
}

fn mood_str(mood: MoodArg) -> &'static str {
    match mood {
        MoodArg::Excited => "excited",
        MoodArg::Cautious => "cautious",
        MoodArg::Confused => "confused",
        MoodArg::Focused => "focused",
        MoodArg::Celebratory => "celebratory",
    }
}

fn sound_str(kind: SoundKind) -> &'static str {
    match kind {
        SoundKind::Click => "click",
        SoundKind::Error => "error",
        SoundKind::Success => "success",
        SoundKind::Warning => "warning",
        SoundKind::Ambient => "ambient",
        SoundKind::Notify => "notify",
    }
}

fn gesture_str(gesture: GestureArg) -> &'static str {
    match gesture {
        GestureArg::Point => "point",
        GestureArg::Think => "think",
        GestureArg::Celebrate => "celebrate",
        GestureArg::Wave => "wave",
        GestureArg::Nod => "nod",
        GestureArg::Shake => "shake",
    }
}

/// Whether a command needs piped stdin for its payload.
fn reads_stdin(command: &Commands) -> bool {
    matches!(
        command,
        Commands::Timeline { .. } | Commands::Chart { data: None, .. }
    )
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match &cli.command {
        Commands::Query {
            op,
            data,
            data_file,
            pretty,
        } => run_query(&cli, op, data.as_deref(), data_file.as_deref(), *pretty),
        Commands::State { path, pretty } => {
            let op = format!("state.{}", path.as_deref().unwrap_or("scene"));
            run_query(&cli, &op, None, None, *pretty)
        }
        _ => run_command(&cli),
    }
}

/// The classic fire-and-forget path, plus the `--ack` wait when requested.
fn run_command(cli: &Cli) -> ExitCode {
    let stdin = if reads_stdin(&cli.command) {
        let mut buffer = String::new();
        let _ = std::io::stdin().read_to_string(&mut buffer);
        buffer.trim().to_string()
    } else {
        String::new()
    };
    let (action, payload) = command_to_osc(&cli.command, &stdin);

    if !cli.ack {
        let sequence = osc::osc_sequence(&action, &payload);
        if cli.dry_run {
            // Readable form for testing and inspection.
            println!("{}", readable(&sequence));
        } else {
            print!("{sequence}");
            let _ = std::io::stdout().flush();
        }
        return exit_codes::OK;
    }

    // --ack: opt in with a correlation token and wait for the kind=ack
    // reply on the tty (the command goes to the tty too, so it reaches
    // ratty even when stdout is piped).
    let token = generate_token();
    let payload = if payload.is_empty() {
        format!("{}={token}", osc::ACK_TOKEN_KEY)
    } else {
        format!("{payload}&{}={token}", osc::ACK_TOKEN_KEY)
    };
    let sequence = osc::osc_sequence(&action, &payload);
    if cli.dry_run {
        println!("{}", readable(&sequence));
        return exit_codes::OK;
    }

    match roundtrip(cli, sequence.as_bytes(), &token) {
        Ok(reply) if reply.ok => {
            if cli.json {
                println!("{}", serde_json::json!({ "ok": true }));
            }
            exit_codes::OK
        }
        Ok(reply) => {
            let code = reply.code.unwrap_or_else(|| "error".to_string());
            emit_failure(cli.json, &code, "the terminal rejected the command");
            exit_codes::reply_error()
        }
        Err(exit) => exit,
    }
}

/// `query`/`state`: emit an OSC 778 query and print the decoded payload.
fn run_query(
    cli: &Cli,
    op: &str,
    data: Option<&str>,
    data_file: Option<&str>,
    pretty: bool,
) -> ExitCode {
    let data_text = match (data, data_file) {
        (Some(inline), _) => Some(inline.to_string()),
        (None, Some("-")) => {
            let mut buffer = String::new();
            if std::io::stdin().read_to_string(&mut buffer).is_err() {
                emit_failure(
                    cli.json,
                    "bad-input",
                    "could not read the JSON payload from stdin",
                );
                return exit_codes::bad_input();
            }
            Some(buffer)
        }
        (None, Some(path)) => match std::fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(error) => {
                emit_failure(
                    cli.json,
                    "bad-input",
                    &format!("could not read {path}: {error}"),
                );
                return exit_codes::bad_input();
            }
        },
        (None, None) => None,
    };
    // Validate client-side so garbage never reaches the wire.
    let data_json = match data_text {
        None => None,
        Some(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => Some(value.to_string()),
            Err(error) => {
                emit_failure(
                    cli.json,
                    "bad-input",
                    &format!("data is not valid JSON: {error}"),
                );
                return exit_codes::bad_input();
            }
        },
    };

    let token = generate_token();
    let sequence = query::query_sequence(&token, op, data_json.as_deref().map(str::as_bytes));
    if cli.dry_run {
        println!("{}", readable(&sequence));
        return exit_codes::OK;
    }

    match roundtrip(cli, sequence.as_bytes(), &token) {
        Ok(reply) if reply.ok => {
            let payload = if reply.data.is_empty() {
                serde_json::Value::Null
            } else {
                match serde_json::from_slice::<serde_json::Value>(&reply.data) {
                    Ok(value) => value,
                    Err(_) => {
                        emit_failure(
                            cli.json,
                            "malformed",
                            "the reply payload was not valid JSON",
                        );
                        return exit_codes::malformed_reply();
                    }
                }
            };
            let text = if pretty {
                serde_json::to_string_pretty(&payload).expect("a JSON value serializes")
            } else {
                payload.to_string()
            };
            println!("{text}");
            exit_codes::OK
        }
        Ok(reply) => {
            let code = reply.code.unwrap_or_else(|| "error".to_string());
            emit_failure(cli.json, &code, "the terminal returned ok=0");
            exit_codes::reply_error()
        }
        Err(exit) => exit,
    }
}

fn generate_token() -> String {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).expect("system entropy is available");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Readable escape form for `--dry-run` output.
fn readable(sequence: &str) -> String {
    sequence.replace('\x1b', "ESC").replace('\x07', "BEL")
}

/// Reports a failure: JSON on stdout in `--json` mode, stderr otherwise.
fn emit_failure(json: bool, code: &str, message: &str) {
    if json {
        println!(
            "{}",
            serde_json::json!({ "ok": false, "code": code, "message": message })
        );
    } else {
        eprintln!("ratty-ai: {code}: {message}");
    }
}

/// Writes `sequence` to the tty and reads until the reply correlated to
/// `token` arrives, the timeout lapses, or the transport fails.
fn roundtrip(cli: &Cli, sequence: &[u8], token: &str) -> Result<query::ParsedReply, ExitCode> {
    #[cfg(not(unix))]
    {
        let _ = (sequence, token);
        emit_failure(
            cli.json,
            "transport",
            "query/state/--ack need a Unix controlling tty (unsupported on this platform)",
        );
        Err(exit_codes::transport())
    }
    #[cfg(unix)]
    {
        let mut tty = match tty::RawTty::open(cli.tty.as_deref()) {
            Ok(tty) => tty,
            Err(error) => {
                emit_failure(
                    cli.json,
                    "transport",
                    &format!("could not open the tty: {error}"),
                );
                return Err(exit_codes::transport());
            }
        };
        if let Err(error) = tty.write_all(sequence) {
            emit_failure(cli.json, "transport", &format!("tty write failed: {error}"));
            return Err(exit_codes::transport());
        }
        match await_reply(&mut tty, token, Duration::from_millis(cli.timeout)) {
            Ok(reply) => Ok(reply),
            Err(WaitError::Timeout) => {
                emit_failure(
                    cli.json,
                    "timeout",
                    "no reply within the timeout (does this terminal speak OSC 778?)",
                );
                Err(exit_codes::timeout())
            }
            Err(WaitError::Malformed) => {
                emit_failure(cli.json, "malformed", "the correlated reply was malformed");
                Err(exit_codes::malformed_reply())
            }
            Err(WaitError::Transport(message)) => {
                emit_failure(cli.json, "transport", &message);
                Err(exit_codes::transport())
            }
        }
    }
}

#[cfg(unix)]
enum WaitError {
    Timeout,
    Malformed,
    Transport(String),
}

/// Scans tty input for the token's reply, ignoring unrelated bytes and
/// unmatched replies (user keystrokes and other terminal reports are
/// consumed while the query is outstanding — the spec'd behavior).
#[cfg(unix)]
fn await_reply(
    tty: &mut tty::RawTty,
    token: &str,
    timeout: Duration,
) -> Result<query::ParsedReply, WaitError> {
    use std::time::Instant;

    let deadline = Instant::now() + timeout;
    let mut scanner = query::ReplyScanner::default();
    let mut buf = [0_u8; 1024];
    loop {
        while let Some(frame) = scanner.next_frame() {
            match query::parse_reply_body(&frame) {
                Some(reply) if reply.token == token => return Ok(reply),
                // Unmatched replies are ignored; a frame that names our
                // token but does not parse is a malformed reply.
                Some(_) => {}
                None if frame.contains(&format!("id={token}")) => {
                    return Err(WaitError::Malformed);
                }
                None => {}
            }
        }
        if Instant::now() >= deadline {
            return Err(WaitError::Timeout);
        }
        match tty.read_chunk(&mut buf) {
            // A VMIN=0/VTIME poll tick with nothing pending.
            Ok(0) => {}
            Ok(size) => scanner.push(&buf[..size]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(WaitError::Transport(format!("tty read failed: {error}"))),
        }
    }
}

/// Raw-mode controlling-tty transport for the reply-reading paths.
///
/// Raw mode turns off echo and canonical buffering so reply bytes are
/// readable immediately and invisible to the user, and turns off ISIG so a
/// Ctrl-C cannot kill the process while the terminal is raw. Restoration
/// is covered on every exit path: the `Drop` guard handles ordinary exits
/// and panics, a SIGTERM/SIGHUP/SIGINT/SIGQUIT handler restores and
/// re-raises for external signals, and VMIN=0/VTIME=1 keeps reads from
/// ever blocking past the deadline loop.
#[cfg(unix)]
mod tty {
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read, Write};
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::path::Path;
    use std::sync::OnceLock;

    static SAVED_FOR_SIGNALS: OnceLock<(RawFd, libc::termios)> = OnceLock::new();

    /// Restores the terminal, then re-raises with the default disposition
    /// so the exit status still reflects the signal. Only
    /// async-signal-safe calls: `tcsetattr`, `signal`, `raise`.
    extern "C" fn restore_and_reraise(signal: libc::c_int) {
        if let Some((fd, saved)) = SAVED_FOR_SIGNALS.get() {
            unsafe { libc::tcsetattr(*fd, libc::TCSANOW, saved) };
        }
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }

    /// The controlling (or supplied) tty, held in raw mode until dropped.
    pub struct RawTty {
        file: File,
        fd: RawFd,
        saved: libc::termios,
    }

    impl RawTty {
        pub fn open(path: Option<&Path>) -> io::Result<Self> {
            let path = path.unwrap_or_else(|| Path::new("/dev/tty"));
            let file = OpenOptions::new().read(true).write(true).open(path)?;
            let fd = file.as_raw_fd();
            if unsafe { libc::isatty(fd) } == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("{} is not a tty", path.display()),
                ));
            }
            let mut saved = unsafe { std::mem::zeroed::<libc::termios>() };
            if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut raw = saved;
            unsafe { libc::cfmakeraw(&mut raw) };
            // Poll-style reads: return within 100 ms even with no bytes,
            // so the caller's deadline loop is never stuck in read().
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 1;
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let _ = SAVED_FOR_SIGNALS.set((fd, saved));
            let handler = restore_and_reraise as extern "C" fn(libc::c_int);
            for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT, libc::SIGQUIT] {
                unsafe { libc::signal(signal, handler as libc::sighandler_t) };
            }
            Ok(Self { file, fd, saved })
        }

        pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.file.write_all(bytes)?;
            self.file.flush()
        }

        pub fn read_chunk(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.file.read(buf)
        }
    }

    impl Drop for RawTty {
        fn drop(&mut self) {
            unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use osc::RattyAiCommand;

    /// Round-trips a command through the CLI encoder and the terminal's
    /// parser: `command_to_osc` must produce bytes the parser recovers as
    /// the expected `RattyAiCommand`. This is the CLI↔terminal contract.
    fn round_trip(command: &Commands, stdin: &str) -> RattyAiCommand {
        let (action, payload) = command_to_osc(command, stdin);
        let sequence = osc::osc_sequence(&action, &payload);
        // Strip the OSC framing (ESC ] 777 ; … BEL) back to the inner form
        // the parser's string entry point expects.
        let inner = sequence
            .strip_prefix("\x1b]777;")
            .and_then(|s| s.strip_suffix('\x07'))
            .expect("well-framed sequence");
        osc::parse_command(inner).expect("terminal parses what the CLI emits")
    }

    #[test]
    fn mode_round_trips() {
        assert_eq!(
            round_trip(&Commands::Mode { mode: "3d".into() }, ""),
            RattyAiCommand::SetMode { mode: "3d".into() }
        );
    }

    #[test]
    fn object_add_round_trips_with_defaults() {
        let command = Commands::Object(ObjectAction::Add {
            id: 0x8000_0001,
            path: "rat.obj".into(),
            x: 10,
            y: 5,
            scale: 1.5,
            spin: 2.0,
            brightness: 1.0,
            replace: false,
        });
        assert_eq!(
            round_trip(&command, ""),
            RattyAiCommand::SpawnObject {
                id: 0x8000_0001,
                path: "rat.obj".into(),
                x: 10,
                y: 5,
                scale: 1.5,
                spin: 2.0,
                brightness: 1.0,
                replace: false,
            }
        );
    }

    #[test]
    fn object_add_replace_flag_round_trips() {
        let command = Commands::Object(ObjectAction::Add {
            id: 0x8000_0002,
            path: "rat.obj".into(),
            x: 0,
            y: 0,
            scale: 1.0,
            spin: 0.0,
            brightness: 1.0,
            replace: true,
        });
        let RattyAiCommand::SpawnObject { id, replace, .. } = round_trip(&command, "") else {
            panic!("expected SpawnObject");
        };
        assert_eq!(id, 0x8000_0002);
        assert!(replace);
    }

    #[test]
    fn think_flags_become_state() {
        assert_eq!(
            round_trip(
                &Commands::Think {
                    start: true,
                    end: false
                },
                ""
            ),
            RattyAiCommand::Think {
                state: "start".into()
            }
        );
    }

    #[test]
    fn note_with_delimiters_survives_encoding() {
        // The payload's own grammar chars inside a value must round-trip.
        let command = Commands::Note {
            text: "check x=1 & y=2; done".into(),
            x: 15,
            y: 10,
            expires: "1h".into(),
        };
        assert_eq!(
            round_trip(&command, ""),
            RattyAiCommand::Note {
                text: "check x=1 & y=2; done".into(),
                x: 15,
                y: 10,
                expires: "1h".into(),
            }
        );
    }

    #[test]
    fn timeline_reads_stdin() {
        assert_eq!(
            round_trip(
                &Commands::Timeline {
                    x: 5,
                    y: 10,
                    scale: 1.0
                },
                "abc def"
            ),
            RattyAiCommand::Timeline {
                x: 5,
                y: 10,
                scale: 1.0,
                input: "abc def".into(),
            }
        );
    }

    #[test]
    fn mood_and_sound_enums_map_to_strings() {
        assert_eq!(
            round_trip(
                &Commands::Mood {
                    mood: MoodArg::Excited
                },
                ""
            ),
            RattyAiCommand::Mood {
                mood: "excited".into()
            }
        );
        assert_eq!(
            round_trip(
                &Commands::Sound {
                    kind: SoundKind::Success,
                    loop_sound: false
                },
                ""
            ),
            RattyAiCommand::Sound {
                kind: "success".into(),
                loop_sound: false,
            }
        );
    }

    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    /// The `--ack` token appends to any payload shape and the terminal's
    /// parser recovers both the command and the token — the CLI↔terminal
    /// ack contract.
    #[test]
    fn ack_token_appends_and_round_trips() {
        // Keyed payload.
        let (action, payload) = command_to_osc(&Commands::Mode { mode: "3d".into() }, "");
        let payload = format!("{payload}&{}=abc123", osc::ACK_TOKEN_KEY);
        let sequence = osc::osc_sequence(&action, &payload);
        let inner = sequence
            .strip_prefix("\x1b]777;")
            .and_then(|s| s.strip_suffix('\x07'))
            .expect("well-framed sequence");
        let control = osc::parse_control(inner).expect("ratty namespace");
        assert_eq!(control.ack_token.as_deref(), Some("abc123"));
        assert_eq!(
            control.command,
            Some(RattyAiCommand::SetMode { mode: "3d".into() })
        );

        // Empty payload.
        let (action, payload) = command_to_osc(&Commands::Object(ObjectAction::Clear), "");
        assert!(payload.is_empty());
        let sequence = osc::osc_sequence(&action, &format!("{}=t1", osc::ACK_TOKEN_KEY));
        let inner = sequence
            .strip_prefix("\x1b]777;")
            .and_then(|s| s.strip_suffix('\x07'))
            .expect("well-framed sequence");
        let control = osc::parse_control(inner).expect("ratty namespace");
        assert_eq!(control.ack_token.as_deref(), Some("t1"));
        assert_eq!(control.command, Some(RattyAiCommand::ClearObjects));
    }

    /// The query builder emits exactly what the terminal's 778 gate
    /// accepts — the CLI↔terminal query contract.
    #[test]
    fn query_sequence_round_trips_through_the_terminal_gate() {
        let sequence = query::query_sequence("feedbeef", "state.scene", Some(b"{\"a\":1}"));
        let body = sequence
            .strip_prefix("\x1b]")
            .and_then(|s| s.strip_suffix("\x1b\\"))
            .expect("well-framed sequence");
        let params: Vec<&[u8]> = body.split(';').map(str::as_bytes).collect();
        match query::parse_778(&params) {
            Some(query::Wire778::Query(envelope)) => {
                assert_eq!(envelope.token, "feedbeef");
                assert_eq!(envelope.op, "state.scene");
                assert_eq!(envelope.data, b"{\"a\":1}");
            }
            other => panic!("expected a query, got {other:?}"),
        }
    }
}
