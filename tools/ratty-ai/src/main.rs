//! `ratty-ai` — pure-CLI control for the Ratty terminal emulator.
//!
//! No sockets, no daemon, no temp files. Each subcommand prints one OSC 777
//! escape sequence to stdout; Ratty intercepts it and acts. Because it is
//! just stdout, it composes with the whole shell — `make && ratty-ai flash
//! green || ratty-ai flash red` — works over SSH, and the identical bytes
//! drive the browser build through `feed()`.
//!
//! The wire format and its encoding live in the terminal's own `src/osc.rs`,
//! included here verbatim so the CLI and the terminal share one source of
//! truth (the same trick `tools/silk` uses with `rgp.rs`).

use std::io::{Read, Write};

use clap::{Parser, Subcommand, ValueEnum};

#[allow(dead_code)] // The CLI uses the encoder half; the parser half is exercised by tests.
#[path = "../../../src/osc.rs"]
mod osc;

/// AI-facing control client for Ratty's 3D terminal scene.
#[derive(Parser)]
#[command(name = "ratty-ai", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    /// Print the escape sequence in readable form instead of emitting it.
    #[arg(long, global = true)]
    dry_run: bool,
}

#[derive(Subcommand)]
enum Commands {
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
    /// Place an object from an asset path.
    Add {
        /// Asset path.
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
        Commands::Object(action) => match action {
            ObjectAction::Add {
                path,
                x,
                y,
                scale,
                spin,
                brightness,
            } => (
                "object.add".into(),
                p().field("path", path)
                    .field("x", x)
                    .field("y", y)
                    .field("scale", scale)
                    .field("spin", spin)
                    .field("brightness", brightness)
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

fn main() {
    let cli = Cli::parse();

    let stdin = if reads_stdin(&cli.command) {
        let mut buffer = String::new();
        let _ = std::io::stdin().read_to_string(&mut buffer);
        buffer.trim().to_string()
    } else {
        String::new()
    };

    let (action, payload) = command_to_osc(&cli.command, &stdin);
    let sequence = osc::osc_sequence(&action, &payload);

    if cli.dry_run {
        // Readable form for testing and inspection.
        println!("{}", sequence.replace('\x1b', "ESC").replace('\x07', "BEL"));
    } else {
        print!("{sequence}");
        let _ = std::io::stdout().flush();
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
            path: "rat.obj".into(),
            x: 10,
            y: 5,
            scale: 1.5,
            spin: 2.0,
            brightness: 1.0,
        });
        assert_eq!(
            round_trip(&command, ""),
            RattyAiCommand::SpawnObject {
                path: "rat.obj".into(),
                x: 10,
                y: 5,
                scale: 1.5,
                spin: 2.0,
                brightness: 1.0,
            }
        );
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
}
