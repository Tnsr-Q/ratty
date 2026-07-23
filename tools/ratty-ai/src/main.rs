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
    /// Collect a process snapshot and publish it as a `ps.v1` viz.
    ///
    /// Gathers locally via sysinfo under the invoking user's own
    /// permissions, keeps the top N processes by CPU, and lowers the
    /// snapshot onto `viz.set` — the terminal only renders what it is
    /// handed. Bare invocations upsert the stable ps slot; `--watch`
    /// republishes fresh snapshots under the same id.
    Ps {
        /// Visualization id (decimal, AI-owned range). Defaults to the
        /// stable ps slot 2147483904 (0x8000_0100).
        #[arg(long)]
        id: Option<u32>,
        /// Keep the top N processes by CPU.
        #[arg(long, default_value_t = 32,
              value_parser = clap::value_parser!(u64).range(1..=MAX_COLLECTOR_TOP))]
        top: u64,
        /// Republish a fresh snapshot every N seconds (min 1) under the
        /// same id until interrupted. Only the first snapshot places the
        /// anchor — refreshes never move the view.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        watch: Option<u64>,
        #[command(flatten)]
        anchor: AnchorArgs,
    },
    /// Collect a bounded filesystem walk and publish it as an `fs.v1` viz.
    ///
    /// Walks breadth-first under the invoking user's own permissions:
    /// never follows symlinks, skips unreadable directories (counted in
    /// the capture provenance), and stops at a hard entry cap. Keeps the
    /// top N entries by size.
    Fs {
        /// Root path to walk.
        #[arg(default_value = ".")]
        path: std::path::PathBuf,
        /// Maximum depth below the root (direct children are depth 1).
        #[arg(long, default_value_t = 3,
              value_parser = clap::value_parser!(u8).range(1..))]
        depth: u8,
        /// Keep the top N entries by size.
        #[arg(long, default_value_t = 64,
              value_parser = clap::value_parser!(u64).range(1..=MAX_COLLECTOR_TOP))]
        top: u64,
        /// Visualization id (decimal, AI-owned range). Defaults to the
        /// stable fs slot 2147483905 (0x8000_0101).
        #[arg(long)]
        id: Option<u32>,
        /// Republish a fresh snapshot every N seconds (min 1) under the
        /// same id until interrupted.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        watch: Option<u64>,
        #[command(flatten)]
        anchor: AnchorArgs,
    },
    /// Collect a repository snapshot and publish it as a `git.v1` viz.
    ///
    /// Shells out to `git` (branch list, porcelain status counts,
    /// ahead/behind from rev-list) under the invoking user's own
    /// permissions. A missing repo or git binary exits 2.
    Git {
        /// Repository path.
        #[arg(long, default_value = ".")]
        repo: std::path::PathBuf,
        /// Visualization id (decimal, AI-owned range). Defaults to the
        /// stable git slot 2147483906 (0x8000_0102).
        #[arg(long)]
        id: Option<u32>,
        /// Republish a fresh snapshot every N seconds (min 1) under the
        /// same id until interrupted.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        watch: Option<u64>,
        #[command(flatten)]
        anchor: AnchorArgs,
    },
    /// Collect interface counters and publish them as a `net.v1` viz.
    ///
    /// Interface byte counters via sysinfo (interfaces, not sockets — an
    /// honest portable v1), link state from IFF_UP on Unix. Keeps the top
    /// N interfaces by total traffic.
    Net {
        /// Visualization id (decimal, AI-owned range). Defaults to the
        /// stable net slot 2147483907 (0x8000_0103).
        #[arg(long)]
        id: Option<u32>,
        /// Keep the top N interfaces by total traffic.
        #[arg(long, default_value_t = 64,
              value_parser = clap::value_parser!(u64).range(1..=MAX_COLLECTOR_TOP))]
        top: u64,
        /// Republish a fresh snapshot every N seconds (min 1) under the
        /// same id until interrupted.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        watch: Option<u64>,
        #[command(flatten)]
        anchor: AnchorArgs,
    },
    /// Signal a process, watch the outcome, and report it honestly as a
    /// `viz.effect` on the ps visualization.
    ///
    /// Identity is pinned to (pid, start time) before signaling and
    /// re-verified afterwards, so PID reuse can never claim a death that
    /// did not happen; the wire only ever carries the *observed* outcome
    /// as the effect name (`died`, `survived`, `denied`, `missing`,
    /// `timeout`) — there is no `kill` verb on the wire. SIGTERM by
    /// default; `--sigkill` opts into SIGKILL. No confirmation prompt:
    /// the invoking user already holds /bin/kill authority.
    ///
    /// Exit codes: 0 the signal was delivered and the exit was observed
    /// (died) · 10 the process survived SIGTERM · 11 permission denied ·
    /// 12 no such process (or its identity changed) · 13 outcome
    /// unobserved within the timeout. `--dry-run` signals nothing and
    /// prints the sequence a confirmed death would emit.
    Kill {
        /// PID to signal.
        pid: u32,
        /// Send SIGKILL instead of the default SIGTERM.
        #[arg(long)]
        sigkill: bool,
        /// How long to watch for the outcome, in milliseconds.
        #[arg(long, default_value_t = 5000)]
        timeout_ms: u64,
        /// Visualization id whose keyed child receives the effect.
        /// Defaults to the stable ps slot 2147483904 (0x8000_0100).
        #[arg(long)]
        id: Option<u32>,
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

/// Anchor placement shared by the collector subcommands. `x`/`y` place the
/// visualization's *top-left* cell (unlike `object.add`'s centered anchor)
/// and must arrive together; a brand-new visualization without them stays
/// hidden until placed. Only the first snapshot of a `--watch` loop sends
/// the anchor — refreshes omit it so a scrolled view is never snapped back.
#[derive(clap::Args)]
struct AnchorArgs {
    /// Anchor column of the top-left cell (place with --y).
    #[arg(short, long, requires = "y")]
    x: Option<u16>,
    /// Anchor row of the top-left cell (place with --x).
    #[arg(short, long, requires = "x")]
    y: Option<u16>,
    /// Footprint width in cells (needs an anchor placed or already live).
    #[arg(long)]
    cols: Option<u16>,
    /// Footprint height in cells (needs an anchor placed or already live).
    #[arg(long)]
    rows: Option<u16>,
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
        Commands::Ps { .. }
        | Commands::Fs { .. }
        | Commands::Git { .. }
        | Commands::Net { .. }
        | Commands::Kill { .. } => {
            unreachable!("collectors gather locally and are handled before command_to_osc")
        }
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

// ── Collectors ──
//
// The `ps`/`fs`/`git`/`net` subcommands and the `kill` watcher gather
// locally, under the invoking user's own permissions, then lower the
// result onto the `viz.*` wire — the terminal never executes, reads, or
// enumerates anything in response to the bytes. Snapshots are normalized
// and bounded here so a worst-case payload provably encodes under the
// shared wire limit (`osc::MAX_VIZ_PAYLOAD_BYTES`, pinned by tests), and
// every snapshot carries capture provenance: ratty never implies liveness
// it was not given.

/// Default `ps` viz id: a fixed, documented slot in the AI id partition's
/// namespace 0, so bare invocations upsert one stable visualization
/// instead of scattering new ids. `--id` overrides.
const DEFAULT_PS_VIZ_ID: u32 = 0x8000_0100;
/// Default `fs` viz id (see [`DEFAULT_PS_VIZ_ID`]).
const DEFAULT_FS_VIZ_ID: u32 = 0x8000_0101;
/// Default `git` viz id (see [`DEFAULT_PS_VIZ_ID`]).
const DEFAULT_GIT_VIZ_ID: u32 = 0x8000_0102;
/// Default `net` viz id (see [`DEFAULT_PS_VIZ_ID`]).
const DEFAULT_NET_VIZ_ID: u32 = 0x8000_0103;

/// Hard cap on every collector's `--top`: with worst-case labels (128
/// bytes of `"` escaping to 256 in JSON) a snapshot of this many items
/// stays under `osc::MAX_VIZ_PAYLOAD_BYTES` for every kind — pinned by
/// `worst_case_snapshots_fit_the_wire_budget`.
const MAX_COLLECTOR_TOP: u64 = 64;

/// Byte cap on normalized scheduler-state strings — a small fixed
/// vocabulary, bounded tighter than free-form labels so the worst-case
/// payload math stays comfortable.
const MAX_STATE_BYTES: usize = 32;

/// Hard cap on directory entries recorded by the `fs` walk before top-N
/// selection: bounds memory and wall time on huge trees. Hitting it is
/// declared in the capture provenance.
const MAX_FS_WALK_ENTRIES: usize = 4096;

/// Poll cadence while `kill` watches for the outcome.
#[cfg(unix)]
const KILL_POLL: Duration = Duration::from_millis(100);

/// Replaces control characters (never legitimate in a label, potentially
/// hostile in terminal-bound data) and truncates to `max` bytes on a char
/// boundary.
fn clean_label_to(value: &str, max: usize) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    if cleaned.len() <= max {
        return cleaned;
    }
    let mut end = max;
    while end > 0 && !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    cleaned[..end].to_string()
}

/// [`clean_label_to`] at the shared wire label bound.
fn clean_label(value: &str) -> String {
    clean_label_to(value, osc::MAX_VIZ_LABEL_BYTES)
}

/// Current UTC time in RFC 3339 (`2026-07-22T12:34:56Z`), std-only. A
/// pre-epoch clock honestly reports the epoch rather than panicking.
fn rfc3339_utc_now() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    rfc3339_utc(seconds)
}

/// Formats Unix seconds as RFC 3339 UTC (no chrono: the days-to-civil
/// conversion below is exact).
fn rfc3339_utc(unix_seconds: u64) -> String {
    let (year, month, day) = civil_from_days((unix_seconds / 86_400) as i64);
    let rem = unix_seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Converts days since 1970-01-01 to a (year, month, day) civil date —
/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// The `capture` provenance object every snapshot carries: a cleaned,
/// bounded source description plus an RFC 3339 timestamp.
fn capture_json(source: &str) -> serde_json::Value {
    serde_json::json!({ "source": clean_label(source), "ts": rfc3339_utc_now() })
}

/// Builds a `viz.set` payload. `anchor` is only supplied for the first
/// emission of a watch loop: an upsert without `x`/`y` keeps the live
/// anchor, so refreshes never move (or un-scroll) the view.
fn viz_set_wire(id: u32, kind: &str, data: &str, anchor: Option<&AnchorArgs>) -> String {
    let mut payload = Payload::default()
        .field("id", id)
        .field("kind", kind)
        .field("data", data);
    if let Some(anchor) = anchor {
        payload = payload
            .opt("x", anchor.x)
            .opt("y", anchor.y)
            .opt("cols", anchor.cols)
            .opt("rows", anchor.rows);
    }
    payload.build()
}

/// Builds a `viz.effect` payload targeting a stable domain key.
fn viz_effect_wire(id: u32, key: &str, effect: &str) -> String {
    Payload::default()
        .field("id", id)
        .field("key", key)
        .field("effect", effect)
        .build()
}

/// Dispatches the four snapshot collectors: build the gatherer for the
/// subcommand, then run the shared emit-and-watch loop.
fn run_collector(cli: &Cli) -> ExitCode {
    match &cli.command {
        Commands::Ps {
            id,
            top,
            watch,
            anchor,
        } => {
            let mut sys = sysinfo::System::new();
            let mut warmed = false;
            let top = *top as usize;
            emit_snapshots(
                cli,
                id.unwrap_or(DEFAULT_PS_VIZ_ID),
                "ps.v1",
                *watch,
                anchor,
                move || Ok(gather_ps(&mut sys, &mut warmed, top)),
            )
        }
        Commands::Fs {
            path,
            depth,
            top,
            id,
            watch,
            anchor,
        } => {
            let path = path.clone();
            let (depth, top) = (*depth, *top as usize);
            emit_snapshots(
                cli,
                id.unwrap_or(DEFAULT_FS_VIZ_ID),
                "fs.v1",
                *watch,
                anchor,
                move || gather_fs(&path, depth, top),
            )
        }
        Commands::Git {
            repo,
            id,
            watch,
            anchor,
        } => {
            let repo = repo.clone();
            emit_snapshots(
                cli,
                id.unwrap_or(DEFAULT_GIT_VIZ_ID),
                "git.v1",
                *watch,
                anchor,
                move || gather_git(&repo),
            )
        }
        Commands::Net {
            id,
            top,
            watch,
            anchor,
        } => {
            let mut networks = sysinfo::Networks::new();
            let top = *top as usize;
            emit_snapshots(
                cli,
                id.unwrap_or(DEFAULT_NET_VIZ_ID),
                "net.v1",
                *watch,
                anchor,
                move || Ok(gather_net(&mut networks, top)),
            )
        }
        _ => unreachable!("run_collector only handles the collector subcommands"),
    }
}

/// The shared collector loop: gather → bound-check → encode → `viz.set`,
/// then optionally sleep and repeat under `--watch` (Ctrl-C exits; the
/// tty is only ever raw inside an `--ack` roundtrip, which restores it).
/// Gather failures report through [`emit_failure`] and exit 2; wire
/// failures carry their own exit codes.
fn emit_snapshots(
    cli: &Cli,
    id: u32,
    kind: &str,
    watch: Option<u64>,
    anchor: &AnchorArgs,
    mut gather: impl FnMut() -> Result<serde_json::Value, String>,
) -> ExitCode {
    let mut first = true;
    loop {
        let snapshot = match gather() {
            Ok(snapshot) => snapshot,
            Err(message) => {
                emit_failure(cli.json, "bad-input", &message);
                return exit_codes::bad_input();
            }
        };
        let bytes = match serde_json::to_vec(&snapshot) {
            Ok(bytes) => bytes,
            Err(error) => {
                emit_failure(
                    cli.json,
                    "bad-input",
                    &format!("could not encode the snapshot: {error}"),
                );
                return exit_codes::bad_input();
            }
        };
        if bytes.len() > osc::MAX_VIZ_PAYLOAD_BYTES {
            // Unreachable while the --top caps hold (pinned by tests); an
            // honest failure beats a silently truncated wire.
            emit_failure(
                cli.json,
                "too-large",
                &format!(
                    "snapshot is {} bytes; the wire caps decoded payloads at {}",
                    bytes.len(),
                    osc::MAX_VIZ_PAYLOAD_BYTES
                ),
            );
            return exit_codes::bad_input();
        }
        let data = query::b64url_encode(&bytes);
        let payload = viz_set_wire(id, kind, &data, first.then_some(anchor));
        if let Err(exit) = emit_command(cli, "viz.set", payload) {
            return exit;
        }
        first = false;
        match watch {
            None => return exit_codes::OK,
            Some(seconds) => std::thread::sleep(Duration::from_secs(seconds)),
        }
    }
}

/// One raw process sample before normalization.
struct PsSample {
    pid: u32,
    name: String,
    cpu: f32,
    mem: u64,
    state: String,
}

/// Gathers a process snapshot via sysinfo. The first call warms the CPU
/// counters (two refreshes separated by sysinfo's minimum interval);
/// watch refreshes measure usage since the previous tick.
fn gather_ps(sys: &mut sysinfo::System, warmed: &mut bool, top: usize) -> serde_json::Value {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate};
    let refresh = ProcessRefreshKind::nothing().with_cpu().with_memory();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
    if !*warmed {
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        *warmed = true;
    }
    let samples = sys
        .processes()
        .values()
        .map(|process| PsSample {
            pid: process.pid().as_u32(),
            name: process.name().to_string_lossy().into_owned(),
            cpu: process.cpu_usage(),
            mem: process.memory(),
            state: process.status().to_string().to_lowercase(),
        })
        .collect();
    ps_snapshot(samples, top)
}

/// Normalizes raw samples into the `ps.v1` snapshot: top N by CPU (ties:
/// memory, then pid, for determinism), labels cleaned and bounded,
/// truncation declared in the capture provenance.
fn ps_snapshot(mut samples: Vec<PsSample>, top: usize) -> serde_json::Value {
    let total = samples.len();
    samples.sort_by(|a, b| {
        b.cpu
            .total_cmp(&a.cpu)
            .then_with(|| b.mem.cmp(&a.mem))
            .then_with(|| a.pid.cmp(&b.pid))
    });
    samples.truncate(top);
    let mut source = format!("ratty-ai ps/sysinfo {}", std::env::consts::OS);
    if total > samples.len() {
        source.push_str(&format!("; top {} of {total} by cpu", samples.len()));
    }
    let items: Vec<serde_json::Value> = samples
        .iter()
        .map(|sample| {
            serde_json::json!({
                "pid": sample.pid,
                "name": clean_label(&sample.name),
                "cpu": round_tenth(sample.cpu),
                "mem": sample.mem,
                "state": clean_label_to(&sample.state, MAX_STATE_BYTES),
            })
        })
        .collect();
    serde_json::json!({ "capture": capture_json(&source), "items": items })
}

/// Rounds a CPU percentage to one decimal for a stable, compact wire form
/// (a raw f32→f64 conversion prints artifacts like `0.10000000149`).
/// Non-finite garbage maps to 0 — JSON has no NaN and the terminal would
/// reject the resulting `null`.
fn round_tenth(value: f32) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    (f64::from(value) * 10.0).round() / 10.0
}

/// One filesystem walk entry before normalization.
struct FsSample {
    path: String,
    dir: bool,
    size: u64,
    depth: u8,
}

/// The raw result of a bounded walk.
#[derive(Default)]
struct FsWalk {
    entries: Vec<FsSample>,
    skipped: usize,
    capped: bool,
}

/// Bounded breadth-first walk: depth-limited, entry-capped, never follows
/// symlinks (they are recorded as plain entries, never traversed), and
/// unreadable directories are skipped but counted. Breadth-first so the
/// entry cap favors shallow coverage over one deep subtree.
fn walk_fs(root: &std::path::Path, max_depth: u8) -> Result<FsWalk, String> {
    use std::collections::VecDeque;
    let mut walk = FsWalk::default();
    if !root.is_dir() {
        return Err(format!("{} is not a readable directory", root.display()));
    }
    let mut queue = VecDeque::from([(root.to_path_buf(), 0_u8)]);
    while let Some((dir, depth)) = queue.pop_front() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                walk.skipped += 1;
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                walk.skipped += 1;
                continue;
            };
            if walk.entries.len() >= MAX_FS_WALK_ENTRIES {
                walk.capped = true;
                return Ok(walk);
            }
            let Ok(file_type) = entry.file_type() else {
                walk.skipped += 1;
                continue;
            };
            let child_depth = depth.saturating_add(1);
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            if file_type.is_dir() {
                walk.entries.push(FsSample {
                    path: rel,
                    dir: true,
                    size: 0,
                    depth: child_depth,
                });
                if child_depth < max_depth {
                    queue.push_back((path, child_depth));
                }
            } else {
                // Files and symlinks (DirEntry::metadata never traverses a
                // symlink); an unreadable size records honestly as 0.
                let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
                walk.entries.push(FsSample {
                    path: rel,
                    dir: false,
                    size,
                    depth: child_depth,
                });
            }
        }
    }
    Ok(walk)
}

/// Gathers an `fs.v1` snapshot from a bounded walk.
fn gather_fs(
    root: &std::path::Path,
    max_depth: u8,
    top: usize,
) -> Result<serde_json::Value, String> {
    let walk = walk_fs(root, max_depth)?;
    Ok(fs_snapshot(&root.display().to_string(), walk, top))
}

/// Normalizes walk results into the `fs.v1` snapshot: top N by size
/// (ties: path, for determinism); truncation, skips, and the walk cap are
/// declared in the capture provenance.
fn fs_snapshot(root: &str, mut walk: FsWalk, top: usize) -> serde_json::Value {
    let total = walk.entries.len();
    walk.entries
        .sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    walk.entries.truncate(top);
    let mut source = String::from("ratty-ai fs/walk");
    if total > walk.entries.len() {
        source.push_str(&format!("; top {} of {total} by size", walk.entries.len()));
    }
    if walk.skipped > 0 {
        source.push_str(&format!("; {} unreadable skipped", walk.skipped));
    }
    if walk.capped {
        source.push_str(&format!("; walk capped at {MAX_FS_WALK_ENTRIES}"));
    }
    let items: Vec<serde_json::Value> = walk
        .entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                "path": clean_label(&entry.path),
                "kind": if entry.dir { "dir" } else { "file" },
                "size": entry.size,
                "depth": entry.depth,
            })
        })
        .collect();
    serde_json::json!({
        "capture": capture_json(&source),
        "root": clean_label(root),
        "items": items,
    })
}

/// Runs one git subcommand in `repo`, returning stdout or a message
/// suitable for [`emit_failure`].
fn git_output(repo: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or("?"),
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parses `git branch --format=%(HEAD)%(refname:short)` output into
/// (name, current) pairs — `%(HEAD)` renders `*` on the checked-out
/// branch and a space elsewhere.
fn parse_git_branches(text: &str) -> Vec<(String, bool)> {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let current = line.starts_with('*');
            let name = line.trim_start_matches(['*', ' ']).to_string();
            (name, current)
        })
        .filter(|(name, _)| !name.is_empty())
        .collect()
}

/// Working-tree counts parsed from `git status --porcelain`.
#[derive(Debug, Default, PartialEq, Eq)]
struct GitCounts {
    staged: u32,
    unstaged: u32,
    untracked: u32,
}

/// Parses porcelain-v1 status lines: `??` is untracked; otherwise a
/// non-space index column counts staged and a non-space worktree column
/// counts unstaged (one line can count both).
fn parse_git_porcelain(text: &str) -> GitCounts {
    let mut counts = GitCounts::default();
    for line in text.lines() {
        let bytes = line.as_bytes();
        if bytes.len() < 2 {
            continue;
        }
        if &bytes[..2] == b"??" {
            counts.untracked += 1;
            continue;
        }
        if bytes[0] != b' ' {
            counts.staged += 1;
        }
        if bytes[1] != b' ' {
            counts.unstaged += 1;
        }
    }
    counts
}

/// Parses `git rev-list --left-right --count @{upstream}...HEAD` output:
/// left = commits only upstream (behind), right = commits only local
/// (ahead). Returns `(ahead, behind)`.
fn parse_ahead_behind(text: &str) -> (u32, u32) {
    let mut numbers = text.split_whitespace();
    let behind = numbers.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    let ahead = numbers.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    (ahead, behind)
}

/// Gathers a `git.v1` snapshot by shelling out to `git` under the
/// invoking user's permissions. Not-a-repo (or a missing git binary) is
/// an error; a missing upstream honestly reports ahead/behind as 0.
fn gather_git(repo: &std::path::Path) -> Result<serde_json::Value, String> {
    git_output(repo, &["rev-parse", "--is-inside-work-tree"])?;
    let branches = parse_git_branches(&git_output(
        repo,
        &["branch", "--list", "--format=%(HEAD)%(refname:short)"],
    )?);
    let counts = parse_git_porcelain(&git_output(repo, &["status", "--porcelain"])?);
    // No upstream is a normal state, not an error.
    let (ahead, behind) = git_output(
        repo,
        &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
    )
    .map(|text| parse_ahead_behind(&text))
    .unwrap_or((0, 0));
    Ok(git_snapshot(
        &repo.display().to_string(),
        branches,
        counts,
        ahead,
        behind,
    ))
}

/// Normalizes into the `git.v1` snapshot: the current branch first, then
/// alphabetical, bounded to the collector cap (declared when it bites).
fn git_snapshot(
    repo: &str,
    mut branches: Vec<(String, bool)>,
    counts: GitCounts,
    ahead: u32,
    behind: u32,
) -> serde_json::Value {
    let total = branches.len();
    branches.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    branches.truncate(MAX_COLLECTOR_TOP as usize);
    let mut source = String::from("ratty-ai git/git");
    if total > branches.len() {
        source.push_str(&format!("; top {} of {total} branches", branches.len()));
    }
    let branches: Vec<serde_json::Value> = branches
        .iter()
        .map(|(name, current)| serde_json::json!({ "name": clean_label(name), "current": current }))
        .collect();
    serde_json::json!({
        "capture": capture_json(&source),
        "repo": clean_label(repo),
        "branches": branches,
        "status": {
            "staged": counts.staged,
            "unstaged": counts.unstaged,
            "untracked": counts.untracked,
        },
        "ahead": ahead,
        "behind": behind,
    })
}

/// One interface sample before normalization.
struct NetSample {
    iface: String,
    rx: u64,
    tx: u64,
    up: bool,
}

/// Gathers a `net.v1` snapshot: interface byte counters from sysinfo,
/// link state from IFF_UP via getifaddrs on Unix (address presence
/// elsewhere — the heuristic is declared in the capture provenance).
fn gather_net(networks: &mut sysinfo::Networks, top: usize) -> serde_json::Value {
    networks.refresh(true);
    #[cfg(unix)]
    let up_map = unix_iff_up_map();
    let samples: Vec<NetSample> = networks
        .iter()
        .map(|(name, data)| {
            #[cfg(unix)]
            let up = up_map.get(name.as_str()).copied().unwrap_or(false);
            #[cfg(not(unix))]
            let up = !data.ip_networks().is_empty();
            NetSample {
                iface: name.clone(),
                rx: data.total_received(),
                tx: data.total_transmitted(),
                up,
            }
        })
        .collect();
    net_snapshot(samples, top)
}

/// Link state per interface from `getifaddrs`: an interface is up when
/// any of its address entries carries IFF_UP. Interfaces sysinfo lists
/// but getifaddrs does not are reported down — never guessed up.
#[cfg(unix)]
fn unix_iff_up_map() -> std::collections::HashMap<String, bool> {
    let mut map = std::collections::HashMap::new();
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs allocates a list we walk read-only and free
    // exactly once below; a non-zero return means nothing was allocated.
    if unsafe { libc::getifaddrs(&mut ifap) } != 0 {
        return map;
    }
    let mut cursor = ifap;
    while !cursor.is_null() {
        // SAFETY: cursor is a valid node of the list getifaddrs returned.
        let entry = unsafe { &*cursor };
        if !entry.ifa_name.is_null() {
            // SAFETY: ifa_name is a NUL-terminated C string per contract.
            let name = unsafe { std::ffi::CStr::from_ptr(entry.ifa_name) }
                .to_string_lossy()
                .into_owned();
            let up = entry.ifa_flags & (libc::IFF_UP as libc::c_uint) != 0;
            let slot = map.entry(name).or_insert(false);
            *slot = *slot || up;
        }
        cursor = entry.ifa_next;
    }
    // SAFETY: ifap came from a successful getifaddrs and is freed once.
    unsafe { libc::freeifaddrs(ifap) };
    map
}

/// Normalizes into the `net.v1` snapshot: top N by total traffic (ties:
/// name), truncation and the link-state source declared in the capture
/// provenance.
fn net_snapshot(mut samples: Vec<NetSample>, top: usize) -> serde_json::Value {
    let total = samples.len();
    samples.sort_by(|a, b| {
        let (a_total, b_total) = (a.rx.saturating_add(a.tx), b.rx.saturating_add(b.tx));
        b_total.cmp(&a_total).then_with(|| a.iface.cmp(&b.iface))
    });
    samples.truncate(top);
    let link = if cfg!(unix) {
        "up=IFF_UP"
    } else {
        "up=has-address"
    };
    let mut source = format!("ratty-ai net/sysinfo {}; {link}", std::env::consts::OS);
    if total > samples.len() {
        source.push_str(&format!("; top {} of {total} by traffic", samples.len()));
    }
    let items: Vec<serde_json::Value> = samples
        .iter()
        .map(|sample| {
            serde_json::json!({
                "iface": clean_label(&sample.iface),
                "rx_bytes": sample.rx,
                "tx_bytes": sample.tx,
                "up": sample.up,
            })
        })
        .collect();
    serde_json::json!({ "capture": capture_json(&source), "items": items })
}

/// The observed outcome of a kill watch — the only thing the wire, the
/// exit code, and the user ever hear. The animation never claims a death
/// that was not observed.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillOutcome {
    /// The signal was delivered and the process's exit was observed
    /// (including its pid resurfacing under a different start time).
    Died,
    /// Still alive with the same identity when the SIGTERM watch ended —
    /// the process ignored or survived the signal.
    Survived,
    /// The kernel refused the signal (EPERM).
    Denied,
    /// No process with that pid — a stale pid is never signaled blind.
    Missing,
    /// The outcome could not be observed within the timeout (a SIGKILLed
    /// process still listed is stuck beyond userspace, e.g. D-state).
    Timeout,
}

#[cfg(unix)]
impl KillOutcome {
    /// The registered `viz.effect` name for this outcome.
    fn effect(self) -> &'static str {
        match self {
            KillOutcome::Died => "died",
            KillOutcome::Survived => "survived",
            KillOutcome::Denied => "denied",
            KillOutcome::Missing => "missing",
            KillOutcome::Timeout => "timeout",
        }
    }

    /// The documented kill exit code, grouped at 10+ so it can never
    /// collide with the transport/reply codes 2-6 an `--ack` can produce.
    fn exit(self) -> ExitCode {
        match self {
            KillOutcome::Died => exit_codes::OK,
            KillOutcome::Survived => ExitCode::from(10),
            KillOutcome::Denied => ExitCode::from(11),
            KillOutcome::Missing => ExitCode::from(12),
            KillOutcome::Timeout => ExitCode::from(13),
        }
    }

    /// Human-readable report for stderr / `--json`.
    fn describe(self, pid: u32, signal: &str) -> String {
        match self {
            KillOutcome::Died => format!("pid {pid} exited after {signal}"),
            KillOutcome::Survived => format!("pid {pid} survived {signal}"),
            KillOutcome::Denied => format!("{signal} to pid {pid} was denied (EPERM)"),
            KillOutcome::Missing => format!("no process with pid {pid}"),
            KillOutcome::Timeout => {
                format!("outcome of {signal} to pid {pid} unobserved within the timeout")
            }
        }
    }
}

/// `ratty-ai kill`: signal, watch, and report the observed outcome as a
/// `viz.effect` on the ps visualization's pid key. The wire never carries
/// a kill verb — only the observed outcome.
fn run_kill(cli: &Cli) -> ExitCode {
    let Commands::Kill {
        pid,
        sigkill,
        timeout_ms,
        id,
    } = &cli.command
    else {
        unreachable!("run_kill only handles the kill subcommand");
    };
    let (pid, sigkill) = (*pid, *sigkill);
    let viz_id = id.unwrap_or(DEFAULT_PS_VIZ_ID);
    // pid 0 signals the whole process group and pids beyond i32 would
    // wrap negative (kill(2) group semantics) — refused, never
    // reinterpreted.
    if pid == 0 || pid > i32::MAX as u32 {
        emit_failure(
            cli.json,
            "bad-input",
            "pid must be between 1 and 2147483647",
        );
        return exit_codes::bad_input();
    }
    if cli.dry_run {
        // Signals nothing: prints the sequence a confirmed death would
        // emit (documented in --help).
        return match emit_command(
            cli,
            "viz.effect",
            viz_effect_wire(viz_id, &pid.to_string(), "died"),
        ) {
            Ok(()) => exit_codes::OK,
            Err(exit) => exit,
        };
    }
    #[cfg(not(unix))]
    {
        let _ = (sigkill, timeout_ms);
        emit_failure(
            cli.json,
            "unsupported",
            "kill needs a Unix platform (signals and process identity)",
        );
        exit_codes::bad_input()
    }
    #[cfg(unix)]
    {
        let signal = if sigkill { "SIGKILL" } else { "SIGTERM" };
        let outcome = watch_kill(pid, sigkill, Duration::from_millis(*timeout_ms));
        let wire = emit_command(
            cli,
            "viz.effect",
            viz_effect_wire(viz_id, &pid.to_string(), outcome.effect()),
        );
        match outcome {
            KillOutcome::Died => {
                if cli.json {
                    println!(
                        "{}",
                        serde_json::json!({ "ok": true, "outcome": "died", "pid": pid })
                    );
                }
            }
            other => emit_failure(cli.json, other.effect(), &other.describe(pid, signal)),
        }
        // A wire failure only overrides a would-be success: exit 0 never
        // lies about either the process outcome or the effect delivery.
        match (outcome, wire) {
            (KillOutcome::Died, Err(exit)) => exit,
            (outcome, _) => outcome.exit(),
        }
    }
}

/// Signals `pid` and watches for the outcome. Identity is (pid, start
/// time): captured immediately before signaling and re-verified while
/// watching, so a reused pid is evidence the original died — never
/// grounds to claim it survived.
#[cfg(unix)]
fn watch_kill(pid: u32, sigkill: bool, timeout: Duration) -> KillOutcome {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let target = Pid::from_u32(pid);
    let refresh = ProcessRefreshKind::nothing();
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[target]), true, refresh);
    let Some(process) = sys.process(target) else {
        return KillOutcome::Missing;
    };
    let start_time = process.start_time();
    let signal = if sigkill {
        libc::SIGKILL
    } else {
        libc::SIGTERM
    };
    // The identity check above sits as close to the syscall as userspace
    // gets; the residual reuse window is why identity is re-verified
    // after signaling too.
    // SAFETY: plain kill(2); pid was bounds-checked to a positive i32.
    if unsafe { libc::kill(pid as libc::pid_t, signal) } != 0 {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => KillOutcome::Missing,
            // EPERM, or anything unexpected: the signal was refused.
            _ => KillOutcome::Denied,
        };
    }
    let deadline = std::time::Instant::now() + timeout;
    loop {
        std::thread::sleep(KILL_POLL);
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[target]), true, refresh);
        match sys.process(target) {
            None => return KillOutcome::Died,
            Some(live) if live.start_time() != start_time => return KillOutcome::Died,
            Some(live) if live.status() == sysinfo::ProcessStatus::Zombie => {
                // Exited but unreaped: the process is dead; only the
                // table entry remains.
                return KillOutcome::Died;
            }
            Some(_) if std::time::Instant::now() >= deadline => {
                return if sigkill {
                    KillOutcome::Timeout
                } else {
                    KillOutcome::Survived
                };
            }
            Some(_) => {}
        }
    }
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
        Commands::Ps { .. } | Commands::Fs { .. } | Commands::Git { .. } | Commands::Net { .. } => {
            run_collector(&cli)
        }
        Commands::Kill { .. } => run_kill(&cli),
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
    match emit_command(cli, &action, payload) {
        Ok(()) => exit_codes::OK,
        Err(exit) => exit,
    }
}

/// Emits one `(action, payload)` command: fire-and-forget on stdout (or
/// readable form under `--dry-run`), or — with `--ack` — over the tty with
/// a correlation token, waiting for the `kind=ack` reply. `Err` carries
/// the failure's exit code so loops (collector `--watch`) can stop on it.
fn emit_command(cli: &Cli, action: &str, payload: String) -> Result<(), ExitCode> {
    if !cli.ack {
        let sequence = osc::osc_sequence(action, &payload);
        if cli.dry_run {
            // Readable form for testing and inspection.
            println!("{}", readable(&sequence));
        } else {
            print!("{sequence}");
            let _ = std::io::stdout().flush();
        }
        return Ok(());
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
    let sequence = osc::osc_sequence(action, &payload);
    if cli.dry_run {
        println!("{}", readable(&sequence));
        return Ok(());
    }

    match roundtrip(cli, sequence.as_bytes(), &token) {
        Ok(reply) if reply.ok => {
            if cli.json {
                println!("{}", serde_json::json!({ "ok": true }));
            }
            Ok(())
        }
        Ok(reply) => {
            let code = reply.code.unwrap_or_else(|| "error".to_string());
            emit_failure(cli.json, &code, "the terminal rejected the command");
            Err(exit_codes::reply_error())
        }
        Err(exit) => Err(exit),
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
    // Bad arguments never reach the wire: a `;` would inject envelope
    // fields, non-printable bytes would break the strict-ASCII envelope.
    if !query::valid_op(op) {
        emit_failure(
            cli.json,
            "bad-input",
            "op must be non-empty printable ASCII without ';'",
        );
        return exit_codes::bad_input();
    }
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

    // ── Collector wire round-trips ──

    /// Frames an `(action, payload)` pair and runs it through the
    /// terminal's parser, like [`round_trip`] but for wires built outside
    /// `command_to_osc` (the collectors gather first).
    fn parse_wire(action: &str, payload: &str) -> RattyAiCommand {
        let sequence = osc::osc_sequence(action, payload);
        let inner = sequence
            .strip_prefix("\x1b]777;")
            .and_then(|s| s.strip_suffix('\x07'))
            .expect("well-framed sequence");
        osc::parse_command(inner).expect("terminal parses what the collector emits")
    }

    #[test]
    fn viz_set_wire_round_trips_and_decodes() {
        let snapshot = serde_json::json!({
            "capture": { "source": "test", "ts": "2026-01-01T00:00:00Z" },
            "items": [
                { "pid": 42, "name": "cargo", "cpu": 1.5, "mem": 1024, "state": "running" },
            ],
        });
        let bytes = serde_json::to_vec(&snapshot).expect("snapshot encodes");
        let data = query::b64url_encode(&bytes);
        let anchor = AnchorArgs {
            x: Some(4),
            y: Some(2),
            cols: Some(24),
            rows: Some(8),
        };
        let payload = viz_set_wire(DEFAULT_PS_VIZ_ID, "ps.v1", &data, Some(&anchor));
        assert_eq!(
            parse_wire("viz.set", &payload),
            RattyAiCommand::VizSet {
                id: DEFAULT_PS_VIZ_ID,
                kind: "ps.v1".into(),
                data: data.clone(),
                x: Some(4),
                y: Some(2),
                cols: Some(24),
                rows: Some(8),
                replace: false,
            }
        );
        // What the terminal decodes is byte-identical to what was
        // gathered.
        let decoded = query::b64url_decode(&data, osc::MAX_VIZ_PAYLOAD_BYTES).expect("decodes");
        assert_eq!(decoded, bytes);
    }

    /// Watch refreshes omit the anchor so an upsert never moves (or
    /// un-scrolls) a live view.
    #[test]
    fn watch_refresh_omits_the_anchor() {
        let payload = viz_set_wire(DEFAULT_FS_VIZ_ID, "fs.v1", "e30", None);
        let RattyAiCommand::VizSet {
            x, y, cols, rows, ..
        } = parse_wire("viz.set", &payload)
        else {
            panic!("expected VizSet");
        };
        assert_eq!((x, y, cols, rows), (None, None, None, None));
    }

    #[test]
    fn kill_effect_wire_round_trips() {
        assert_eq!(
            parse_wire(
                "viz.effect",
                &viz_effect_wire(DEFAULT_PS_VIZ_ID, "1234", "died")
            ),
            RattyAiCommand::VizEffect {
                id: DEFAULT_PS_VIZ_ID,
                key: "1234".into(),
                effect: "died".into(),
            }
        );
    }

    /// The five kill outcomes map exactly onto the terminal's registered
    /// effect names — drift here means invisible effects.
    #[cfg(unix)]
    #[test]
    fn kill_outcomes_map_to_registered_effects() {
        assert_eq!(KillOutcome::Died.effect(), "died");
        assert_eq!(KillOutcome::Survived.effect(), "survived");
        assert_eq!(KillOutcome::Denied.effect(), "denied");
        assert_eq!(KillOutcome::Missing.effect(), "missing");
        assert_eq!(KillOutcome::Timeout.effect(), "timeout");
    }

    // ── Collector normalization ──

    #[test]
    fn labels_are_cleaned_and_bounded() {
        assert_eq!(clean_label("plain"), "plain");
        assert_eq!(clean_label("tab\tbell\x07esc\x1b"), "tab?bell?esc?");
        let long = "x".repeat(300);
        assert_eq!(clean_label(&long).len(), osc::MAX_VIZ_LABEL_BYTES);
        // Truncation lands on a char boundary, never mid-UTF-8.
        let accents = "é".repeat(100); // 2 bytes per char
        let cleaned = clean_label(&accents);
        assert_eq!(cleaned.len(), osc::MAX_VIZ_LABEL_BYTES);
        assert!(cleaned.chars().all(|c| c == 'é'));
        let wide = "🦀".repeat(50); // 4 bytes per char
        assert!(clean_label(&wide).len() <= osc::MAX_VIZ_LABEL_BYTES);
    }

    fn ps_sample(pid: u32, cpu: f32, mem: u64) -> PsSample {
        PsSample {
            pid,
            name: format!("proc{pid}"),
            cpu,
            mem,
            state: "running".into(),
        }
    }

    #[test]
    fn ps_snapshot_keeps_the_top_by_cpu_and_declares_truncation() {
        let samples = vec![
            ps_sample(1, 1.0, 10),
            ps_sample(2, 9.0, 10),
            ps_sample(3, 5.0, 10),
        ];
        let snapshot = ps_snapshot(samples, 2);
        let items = snapshot["items"].as_array().expect("items");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["pid"], 2);
        assert_eq!(items[1]["pid"], 3);
        let source = snapshot["capture"]["source"].as_str().expect("source");
        assert!(
            source.contains("top 2 of 3"),
            "source declares truncation: {source}"
        );
        assert!(
            snapshot["capture"]["ts"]
                .as_str()
                .expect("ts")
                .ends_with('Z')
        );
    }

    #[test]
    fn ps_snapshot_ties_break_deterministically() {
        let samples = vec![
            ps_sample(9, 1.0, 5),
            ps_sample(3, 1.0, 5),
            ps_sample(7, 1.0, 9),
        ];
        let snapshot = ps_snapshot(samples, 3);
        let pids: Vec<u64> = snapshot["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|item| item["pid"].as_u64().expect("pid"))
            .collect();
        // Memory descending breaks the cpu tie, then pid ascending.
        assert_eq!(pids, vec![7, 3, 9]);
    }

    #[test]
    fn cpu_rounds_to_a_tenth_and_never_nan() {
        assert_eq!(round_tenth(1.25), 1.3);
        assert_eq!(round_tenth(0.1), 0.1);
        assert_eq!(round_tenth(f32::NAN), 0.0);
        assert_eq!(round_tenth(f32::INFINITY), 0.0);
    }

    #[test]
    fn rfc3339_formats_known_instants() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(rfc3339_utc(4_102_444_799), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn fs_walk_bounds_depth_and_never_follows_symlinks() {
        // Per-process temp dir so parallel test runs never collide.
        let root = std::env::temp_dir().join(format!("ratty-ai-fs-walk-{}", std::process::id()));
        let deep = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).expect("create test tree");
        std::fs::write(root.join("top.txt"), b"12345").expect("write");
        std::fs::write(root.join("a").join("mid.txt"), b"123").expect("write");
        std::fs::write(deep.join("deep.txt"), b"1").expect("write");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&root, root.join("loop")).expect("symlink");

        let walk = walk_fs(&root, 2).expect("walk succeeds");
        let paths: Vec<&str> = walk.entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"top.txt"), "missing top.txt: {paths:?}");
        assert!(paths.iter().any(|p| p.ends_with("mid.txt")));
        assert!(paths.iter().any(|p| p.ends_with("b")));
        // Depth 2 never reaches a/b/c or its contents.
        assert!(
            !paths
                .iter()
                .any(|p| p.contains("deep.txt") || p.ends_with("c")),
            "depth bound leaked: {paths:?}"
        );
        assert!(walk.entries.iter().all(|e| e.depth <= 2));
        #[cfg(unix)]
        {
            let link = walk
                .entries
                .iter()
                .find(|e| e.path.ends_with("loop"))
                .expect("symlink listed as a plain entry");
            assert!(!link.dir, "symlinks are never traversed as directories");
        }
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn fs_snapshot_ranks_by_size_and_declares_the_walk_story() {
        let walk = FsWalk {
            entries: vec![
                FsSample {
                    path: "small".into(),
                    dir: false,
                    size: 1,
                    depth: 1,
                },
                FsSample {
                    path: "big".into(),
                    dir: false,
                    size: 100,
                    depth: 1,
                },
                FsSample {
                    path: "dir".into(),
                    dir: true,
                    size: 0,
                    depth: 1,
                },
            ],
            skipped: 2,
            capped: true,
        };
        let snapshot = fs_snapshot("/tmp/x", walk, 2);
        let items = snapshot["items"].as_array().expect("items");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["path"], "big");
        assert_eq!(items[0]["kind"], "file");
        let source = snapshot["capture"]["source"].as_str().expect("source");
        assert!(source.contains("top 2 of 3"), "{source}");
        assert!(source.contains("2 unreadable skipped"), "{source}");
        assert!(source.contains("walk capped"), "{source}");
        assert_eq!(snapshot["root"], "/tmp/x");
    }

    #[test]
    fn git_porcelain_counts_staged_unstaged_untracked() {
        let text = " M unstaged.rs\nM  staged.rs\nMM both.rs\n?? new.rs\nA  added.rs\n";
        assert_eq!(
            parse_git_porcelain(text),
            GitCounts {
                staged: 3,
                unstaged: 2,
                untracked: 1,
            }
        );
    }

    #[test]
    fn git_branches_parse_and_current_sorts_first() {
        let branches = parse_git_branches("*main\n dev\n feature/x\n");
        assert_eq!(
            branches,
            vec![
                ("main".to_string(), true),
                ("dev".to_string(), false),
                ("feature/x".to_string(), false),
            ]
        );
        let snapshot = git_snapshot(
            "repo",
            vec![
                ("zeta".into(), false),
                ("main".into(), true),
                ("alpha".into(), false),
            ],
            GitCounts::default(),
            1,
            2,
        );
        let names: Vec<&str> = snapshot["branches"]
            .as_array()
            .expect("branches")
            .iter()
            .map(|branch| branch["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, vec!["main", "alpha", "zeta"]);
        assert_eq!(snapshot["branches"][0]["current"], true);
        assert_eq!(snapshot["ahead"], 1);
        assert_eq!(snapshot["behind"], 2);
    }

    #[test]
    fn ahead_behind_parses_left_right_order() {
        // @{upstream}...HEAD: left = behind, right = ahead.
        assert_eq!(parse_ahead_behind("2\t5\n"), (5, 2));
        assert_eq!(parse_ahead_behind(""), (0, 0));
    }

    #[test]
    fn net_snapshot_ranks_by_total_traffic() {
        let samples = vec![
            NetSample {
                iface: "lo0".into(),
                rx: 10,
                tx: 10,
                up: true,
            },
            NetSample {
                iface: "en0".into(),
                rx: 1000,
                tx: 500,
                up: true,
            },
            NetSample {
                iface: "awdl0".into(),
                rx: 0,
                tx: 0,
                up: false,
            },
        ];
        let snapshot = net_snapshot(samples, 2);
        let items = snapshot["items"].as_array().expect("items");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["iface"], "en0");
        assert_eq!(items[1]["iface"], "lo0");
        assert_eq!(items[0]["up"], true);
        let source = snapshot["capture"]["source"].as_str().expect("source");
        assert!(
            source.contains("up="),
            "link-state source declared: {source}"
        );
    }

    /// The provable-fit contract behind `--top`'s hard cap: snapshots of
    /// [`MAX_COLLECTOR_TOP`] items with worst-case labels (128 bytes of
    /// `"`, escaping to 256 in JSON) and maximal numbers must encode
    /// under the shared decoded-payload limit for every kind. If this
    /// fails, a cap changed incompatibly.
    #[test]
    fn worst_case_snapshots_fit_the_wire_budget() {
        let top = MAX_COLLECTOR_TOP as usize;
        assert!(top <= osc::MAX_VIZ_ITEMS_PER_SNAPSHOT);
        let label = "\"".repeat(osc::MAX_VIZ_LABEL_BYTES);
        let state = "\"".repeat(MAX_STATE_BYTES * 2);

        let ps = ps_snapshot(
            (0..top as u32 * 2)
                .map(|i| PsSample {
                    pid: u32::MAX - i,
                    name: label.clone(),
                    cpu: f32::MAX,
                    mem: u64::MAX,
                    state: state.clone(),
                })
                .collect(),
            top,
        );
        let fs = fs_snapshot(
            &label,
            FsWalk {
                entries: (0..top * 2)
                    .map(|i| FsSample {
                        path: format!("{label}{i}"),
                        dir: i % 2 == 0,
                        size: u64::MAX - i as u64,
                        depth: u8::MAX,
                    })
                    .collect(),
                skipped: usize::MAX,
                capped: true,
            },
            top,
        );
        let git = git_snapshot(
            &label,
            (0..top * 2)
                .map(|i| (format!("{label}{i}"), i == 0))
                .collect(),
            GitCounts {
                staged: u32::MAX,
                unstaged: u32::MAX,
                untracked: u32::MAX,
            },
            u32::MAX,
            u32::MAX,
        );
        let net = net_snapshot(
            (0..top * 2)
                .map(|i| NetSample {
                    iface: format!("{label}{i}"),
                    rx: u64::MAX,
                    tx: u64::MAX,
                    up: i % 2 == 0,
                })
                .collect(),
            top,
        );
        for (kind, snapshot) in [("ps", ps), ("fs", fs), ("git", git), ("net", net)] {
            let bytes = serde_json::to_vec(&snapshot).expect("encodes");
            assert!(
                bytes.len() <= osc::MAX_VIZ_PAYLOAD_BYTES,
                "worst-case {kind} snapshot is {} bytes (cap {})",
                bytes.len(),
                osc::MAX_VIZ_PAYLOAD_BYTES
            );
            // And the framed wire form survives the terminal's OSC
            // watchdog envelope math (const-asserted terminal-side).
            let encoded = query::b64url_encode(&bytes);
            assert!(encoded.len() <= osc::MAX_VIZ_PAYLOAD_BYTES.div_ceil(3) * 4);
        }
    }
}
