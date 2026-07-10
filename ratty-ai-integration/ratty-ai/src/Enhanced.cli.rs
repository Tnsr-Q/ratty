//! ratty-ai v2 — Enhanced CLI control for Ratty terminal emulator
//!
//! New commands: ps, net, git, fs, bookmark, macro, avatar, sound,
//! think, confidence, react, pane, history, note

use clap::{Parser, Subcommand, ValueEnum};
use std::io::{self, Write, Read};

const OSC_START: &str = "\x1b]";
const OSC_END: &str = "\x07";
const OSC_NS: &str = "777;ratty";

#[derive(Parser)]
#[command(name = "ratty-ai", version = "0.2.0")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    #[arg(long, global = true)]
    dry_run: bool,
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Commands {
    // ─── ORIGINAL COMMANDS ───
    #[command(subcommand)]
    Object(ObjectAction),
    Mode { mode: String },
    Warp { intensity: f32 },
    Flash { #[arg(short, long, default_value = "#ffffff")] color: String, #[arg(short, long, default_value = "0.5")] duration: f32 },
    Pulse { #[arg(short, long, default_value = "0.8")] intensity: f32, #[arg(short, long, default_value = "1.0")] duration: f32 },
    Cursor { #[arg(short, long)] model: Option<String>, #[arg(short, long)] spin: Option<f32>, #[arg(long)] bob_speed: Option<f32>, #[arg(long)] bob_amp: Option<f32>, #[arg(long)] brightness: Option<f32>, #[arg(long)] visible: Option<bool> },
    Reset,
    State,
    Screenshot { #[arg(short, long, default_value = "ratty-screenshot.png")] output: String },
    Chart { #[arg(short, long, default_value = "bar")] kind: String, #[arg(short, long, default_value = "0")] x: u16, #[arg(short, long, default_value = "0")] y: u16, #[arg(short, long, default_value = "1.0")] scale: f32, #[arg(short, long)] data: Option<String> },
    Timeline { #[arg(short, long, default_value = "0")] x: u16, #[arg(short, long, default_value = "0")] y: u16, #[arg(short, long, default_value = "1.0")] scale: f32 },
    Tint { color: String, #[arg(short, long, default_value = "0.1")] opacity: f32 },

    // ─── NEW: PROCESS VISUALIZATION ───
    /// Visualize running processes in 3D
    Ps {
        #[arg(short, long)]
        visualize: bool,
        #[arg(short, long)]
        highlight: Option<u32>,
        #[arg(long)]
        color: Option<String>,
    },
    /// Kill a process with visual effect
    Kill {
        pid: u32,
        #[arg(short, long, default_value = "explode")]
        effect: String,
    },

    // ─── NEW: FILE SYSTEM AS 3D SPACE ───
    /// Navigate directory as 3D room
    Cd {
        path: String,
        #[arg(short, long)]
        visualize: bool,
    },
    /// List files as 3D objects
    Ls {
        #[arg(short, long)]
        visualize: bool,
        #[arg(short, long, default_value = ".")]
        path: String,
    },
    /// Directory tree as 3D branching structure
    Tree {
        #[arg(short, long, default_value = "3")]
        depth: u8,
        #[arg(short, long)]
        visualize: bool,
    },

    // ─── NEW: GIT INTEGRATION ───
    /// Visualize git branches as 3D rivers
    Git {
        #[command(subcommand)]
        action: GitAction,
    },

    // ─── NEW: NETWORK VISUALIZATION ───
    /// Show network connections as 3D lines
    Net {
        #[arg(short, long)]
        visualize: bool,
        #[arg(short, long)]
        host: Option<String>,
    },

    // ─── NEW: AI STATE INDICATORS ───
    /// Show AI is thinking
    Think {
        #[arg(short, long)]
        start: bool,
        #[arg(short, long)]
        end: bool,
    },
    /// Set AI confidence level (0.0-1.0)
    Confidence {
        level: f32,
    },
    /// Set AI mood/emotion
    Mood {
        #[arg(value_enum)]
        mood: MoodArg,
    },

    // ─── NEW: PANE / WINDOW MANAGEMENT ───
    /// Split terminal into panes
    Split {
        #[arg(short, long, default_value = "vertical")]
        direction: String,
        #[arg(short, long, default_value = "0.5")]
        ratio: f32,
    },
    /// Focus a pane
    Focus {
        pane: u8,
    },
    /// Resize a pane
    Resize {
        pane: u8,
        #[arg(short, long)]
        width: Option<u16>,
        #[arg(short, long)]
        height: Option<u16>,
    },
    /// Close a pane
    Close {
        pane: u8,
    },

    // ─── NEW: TIME / HISTORY ───
    /// Command history as 3D timeline
    History {
        #[arg(short, long, default_value = "50")]
        last: usize,
        #[arg(short, long)]
        visualize: bool,
    },
    /// Bookmark current state
    Bookmark {
        #[arg(short, long)]
        name: String,
    },
    /// Jump to bookmark
    Jump {
        name: String,
    },

    // ─── NEW: COLLABORATION ───
    /// Another user joins
    User {
        #[command(subcommand)]
        action: UserAction,
    },
    /// Leave a 3D note
    Note {
        text: String,
        #[arg(short, long, default_value = "0")]
        x: u16,
        #[arg(short, long, default_value = "0")]
        y: u16,
        #[arg(short, long, default_value = "1h")]
        expires: String,
    },

    // ─── NEW: SOUND / HAPTICS ───
    /// Play sound effect
    Sound {
        #[arg(value_enum)]
        kind: SoundKind,
        #[arg(short, long)]
        loop_sound: bool,
    },

    // ─── NEW: AI AVATAR ───
    /// Control AI presence avatar
    Avatar {
        #[command(subcommand)]
        action: AvatarAction,
    },

    // ─── NEW: MACRO / SCRIPTING ───
    /// Record and play macros
    Macro {
        #[command(subcommand)]
        action: MacroAction,
    },

    // ─── NEW: ENVIRONMENT REACTIVE ───
    /// React to system events
    React {
        #[arg(short, long)]
        on_cpu_high: Option<f32>,
        #[arg(short, long)]
        on_memory_high: Option<f32>,
        #[arg(short, long)]
        on_battery_low: Option<f32>,
        #[arg(short, long)]
        effect: String,
    },
}

#[derive(Subcommand)]
enum ObjectAction {
    Add { #[arg(short, long)] path: String, #[arg(short, long, default_value = "0")] x: u16, #[arg(short, long, default_value = "0")] y: u16, #[arg(short, long, default_value = "1.0")] scale: f32, #[arg(long, default_value = "0.0")] spin: f32, #[arg(long, default_value = "1.0")] brightness: f32, #[arg(long)] remesh: bool },
    Remove { id: u32 },
    Clear,
    List,
    Update { id: u32, #[arg(short, long)] x: Option<u16>, #[arg(short, long)] y: Option<u16>, #[arg(short, long)] scale: Option<f32>, #[arg(long)] spin: Option<f32>, #[arg(long)] brightness: Option<f32> },
}

#[derive(Subcommand)]
enum GitAction {
    Branch { #[arg(short, long)] visualize: bool },
    Diff { #[arg(short, long)] visualize: bool },
    Merge { #[arg(short, long)] visualize: bool },
    Stash { #[arg(short, long)] visualize: bool },
}

#[derive(Subcommand)]
enum UserAction {
    Join { #[arg(short, long)] name: String, #[arg(short, long, default_value = "#00ff00")] color: String },
    Leave { #[arg(short, long)] name: String },
    Cursor { #[arg(short, long)] name: String, #[arg(short, long)] x: u16, #[arg(short, long)] y: u16 },
}

#[derive(Subcommand)]
enum AvatarAction {
    Set { #[arg(short, long, default_value = "ai-helper.glb")] model: String, #[arg(short, long, default_value = "top-right")] position: String },
    Gesture { #[arg(value_enum)] gesture: GestureArg },
    Speak { text: String },
    Hide,
}

#[derive(Subcommand)]
enum MacroAction {
    Record { #[arg(short, long)] name: String },
    Stop,
    Play { name: String },
    Export { #[arg(short, long)] name: String, #[arg(short, long, default_value = "macro.ratty")] to: String },
    Run { path: String },
}

#[derive(Clone, ValueEnum)]
enum MoodArg { Excited, Cautious, Confused, Focused, Celebratory }

#[derive(Clone, ValueEnum)]
enum SoundKind { Click, Error, Success, Warning, Ambient, Notify }

#[derive(Clone, ValueEnum)]
enum GestureArg { Point, Think, Celebrate, Wave, Nod, Shake }

// ─── OSC emit helpers ───

fn emit(action: &str, payload: &str, dry: bool) {
    let enc = urlencoding::encode(payload);
    let seq = format!("{};{}:{};{}{}", OSC_START, OSC_NS, action, enc, OSC_END);
    if dry {
        eprintln!("[dry-run] {}", seq.replace('\x1b', "ESC").replace('\x07', "BEL"));
    } else {
        print!("{}", seq);
        let _ = io::stdout().flush();
    }
}

fn read_stdin() -> String {
    let mut buf = String::new();
    let _ = io::stdin().read_to_string(&mut buf);
    buf.trim().to_string()
}

// ─── Main ───

fn main() {
    let cli = Cli::parse();

    match cli.command {
        // ── ORIGINALS ──
        Commands::Object(a) => match a {
            ObjectAction::Add { path, x, y, scale, spin, brightness, remesh } => {
                emit("object.add", &format!("path={}&x={}&y={}&scale={:.2}&spin={:.2}&brightness={:.2}&remesh={}", path, x, y, scale, spin, brightness, remesh), cli.dry_run);
            }
            ObjectAction::Remove { id } => emit("object.remove", &format!("id={}", id), cli.dry_run),
            ObjectAction::Clear => emit("object.clear", "", cli.dry_run),
            ObjectAction::List => emit("object.list", "", cli.dry_run),
            ObjectAction::Update { id, x, y, scale, spin, brightness } => {
                let mut p = vec![format!("id={}", id)];
                if let Some(v) = x { p.push(format!("x={}", v)); }
                if let Some(v) = y { p.push(format!("y={}", v)); }
                if let Some(v) = scale { p.push(format!("scale={:.2}", v)); }
                if let Some(v) = spin { p.push(format!("spin={:.2}", v)); }
                if let Some(v) = brightness { p.push(format!("brightness={:.2}", v)); }
                emit("object.update", &p.join("&"), cli.dry_run);
            }
        },
        Commands::Mode { mode } => emit("mode", &mode, cli.dry_run),
        Commands::Warp { intensity } => emit("warp", &format!("{:.2}", intensity), cli.dry_run),
        Commands::Flash { color, duration } => emit("flash", &format!("color={}&duration={:.2}", color, duration), cli.dry_run),
        Commands::Pulse { intensity, duration } => emit("pulse", &format!("intensity={:.2}&duration={:.2}", intensity, duration), cli.dry_run),
        Commands::Cursor { model, spin, bob_speed, bob_amp, brightness, visible } => {
            let mut p = vec![];
            if let Some(v) = model { p.push(format!("model={}", v)); }
            if let Some(v) = spin { p.push(format!("spin={:.2}", v)); }
            if let Some(v) = bob_speed { p.push(format!("bob_speed={:.2}", v)); }
            if let Some(v) = bob_amp { p.push(format!("bob_amp={:.2}", v)); }
            if let Some(v) = brightness { p.push(format!("brightness={:.2}", v)); }
            if let Some(v) = visible { p.push(format!("visible={}", v)); }
            emit("cursor", &p.join("&"), cli.dry_run);
        }
        Commands::Reset => emit("reset", "", cli.dry_run),
        Commands::State => emit("state", "", cli.dry_run),
        Commands::Screenshot { output } => emit("screenshot", &format!("path={}", output), cli.dry_run),
        Commands::Chart { kind, x, y, scale, data } => {
            let d = data.unwrap_or_else(|| read_stdin());
            emit("chart", &format!("kind={}&x={}&y={}&scale={:.2}&data={}", kind, x, y, scale, d), cli.dry_run);
        }
        Commands::Timeline { x, y, scale } => {
            let input = read_stdin();
            emit("timeline", &format!("x={}&y={}&scale={:.2}&input={}", x, y, scale, input), cli.dry_run);
        }
        Commands::Tint { color, opacity } => emit("tint", &format!("color={}&opacity={:.2}", color, opacity), cli.dry_run),

        // ── NEW: PROCESS ──
        Commands::Ps { visualize, highlight, color } => {
            let mut p = vec![format!("visualize={}", visualize)];
            if let Some(pid) = highlight { p.push(format!("highlight={}", pid)); }
            if let Some(c) = color { p.push(format!("color={}", c)); }
            emit("ps", &p.join("&"), cli.dry_run);
        }
        Commands::Kill { pid, effect } => {
            emit("kill", &format!("pid={}&effect={}", pid, effect), cli.dry_run);
        }

        // ── NEW: FILE SYSTEM ──
        Commands::Cd { path, visualize } => {
            emit("cd", &format!("path={}&visualize={}", path, visualize), cli.dry_run);
        }
        Commands::Ls { visualize, path } => {
            emit("ls", &format!("path={}&visualize={}", path, visualize), cli.dry_run);
        }
        Commands::Tree { depth, visualize } => {
            emit("tree", &format!("depth={}&visualize={}", depth, visualize), cli.dry_run);
        }

        // ── NEW: GIT ──
        Commands::Git { action } => match action {
            GitAction::Branch { visualize } => emit("git.branch", &format!("visualize={}", visualize), cli.dry_run),
            GitAction::Diff { visualize } => emit("git.diff", &format!("visualize={}", visualize), cli.dry_run),
            GitAction::Merge { visualize } => emit("git.merge", &format!("visualize={}", visualize), cli.dry_run),
            GitAction::Stash { visualize } => emit("git.stash", &format!("visualize={}", visualize), cli.dry_run),
        }

        // ── NEW: NETWORK ──
        Commands::Net { visualize, host } => {
            let mut p = vec![format!("visualize={}", visualize)];
            if let Some(h) = host { p.push(format!("host={}", h)); }
            emit("net", &p.join("&"), cli.dry_run);
        }

        // ── NEW: AI STATE ──
        Commands::Think { start, end } => {
            if start { emit("think", "state=start", cli.dry_run); }
            else if end { emit("think", "state=end", cli.dry_run); }
            else { emit("think", "state=toggle", cli.dry_run); }
        }
        Commands::Confidence { level } => {
            emit("confidence", &format!("level={:.2}", level.clamp(0.0, 1.0)), cli.dry_run);
        }
        Commands::Mood { mood } => {
            let m = match mood {
                MoodArg::Excited => "excited",
                MoodArg::Cautious => "cautious",
                MoodArg::Confused => "confused",
                MoodArg::Focused => "focused",
                MoodArg::Celebratory => "celebratory",
            };
            emit("mood", &format!("mood={}", m), cli.dry_run);
        }

        // ── NEW: PANES ──
        Commands::Split { direction, ratio } => {
            emit("pane.split", &format!("direction={}&ratio={:.2}", direction, ratio), cli.dry_run);
        }
        Commands::Focus { pane } => {
            emit("pane.focus", &format!("pane={}", pane), cli.dry_run);
        }
        Commands::Resize { pane, width, height } => {
            let mut p = vec![format!("pane={}", pane)];
            if let Some(w) = width { p.push(format!("width={}", w)); }
            if let Some(h) = height { p.push(format!("height={}", h)); }
            emit("pane.resize", &p.join("&"), cli.dry_run);
        }
        Commands::Close { pane } => {
            emit("pane.close", &format!("pane={}", pane), cli.dry_run);
        }

        // ── NEW: HISTORY ──
        Commands::History { last, visualize } => {
            emit("history", &format!("last={}&visualize={}", last, visualize), cli.dry_run);
        }
        Commands::Bookmark { name } => {
            emit("bookmark", &format!("name={}", name), cli.dry_run);
        }
        Commands::Jump { name } => {
            emit("jump", &format!("name={}", name), cli.dry_run);
        }

        // ── NEW: COLLABORATION ──
        Commands::User { action } => match action {
            UserAction::Join { name, color } => emit("user.join", &format!("name={}&color={}", name, color), cli.dry_run),
            UserAction::Leave { name } => emit("user.leave", &format!("name={}", name), cli.dry_run),
            UserAction::Cursor { name, x, y } => emit("user.cursor", &format!("name={}&x={}&y={}", name, x, y), cli.dry_run),
        }
        Commands::Note { text, x, y, expires } => {
            emit("note", &format!("text={}&x={}&y={}&expires={}", text, x, y, expires), cli.dry_run);
        }

        // ── NEW: SOUND ──
        Commands::Sound { kind, loop_sound } => {
            let k = match kind {
                SoundKind::Click => "click",
                SoundKind::Error => "error",
                SoundKind::Success => "success",
                SoundKind::Warning => "warning",
                SoundKind::Ambient => "ambient",
                SoundKind::Notify => "notify",
            };
            emit("sound", &format!("kind={}&loop={}", k, loop_sound), cli.dry_run);
        }

        // ── NEW: AVATAR ──
        Commands::Avatar { action } => match action {
            AvatarAction::Set { model, position } => {
                emit("avatar.set", &format!("model={}&position={}", model, position), cli.dry_run);
            }
            AvatarAction::Gesture { gesture } => {
                let g = match gesture {
                    GestureArg::Point => "point",
                    GestureArg::Think => "think",
                    GestureArg::Celebrate => "celebrate",
                    GestureArg::Wave => "wave",
                    GestureArg::Nod => "nod",
                    GestureArg::Shake => "shake",
                };
                emit("avatar.gesture", &format!("gesture={}", g), cli.dry_run);
            }
            AvatarAction::Speak { text } => {
                emit("avatar.speak", &format!("text={}", text), cli.dry_run);
            }
            AvatarAction::Hide => {
                emit("avatar.hide", "", cli.dry_run);
            }
        }

        // ── NEW: MACRO ──
        Commands::Macro { action } => match action {
            MacroAction::Record { name } => {
                emit("macro.record", &format!("name={}", name), cli.dry_run);
            }
            MacroAction::Stop => {
                emit("macro.stop", "", cli.dry_run);
            }
            MacroAction::Play { name } => {
                emit("macro.play", &format!("name={}", name), cli.dry_run);
            }
            MacroAction::Export { name, to } => {
                emit("macro.export", &format!("name={}&to={}", name, to), cli.dry_run);
            }
            MacroAction::Run { path } => {
                emit("macro.run", &format!("path={}", path), cli.dry_run);
            }
        }

        // ── NEW: REACTIVE ──
        Commands::React { on_cpu_high, on_memory_high, on_battery_low, effect } => {
            let mut p = vec![format!("effect={}", effect)];
            if let Some(v) = on_cpu_high { p.push(format!("cpu_high={:.1}", v)); }
            if let Some(v) = on_memory_high { p.push(format!("memory_high={:.1}", v)); }
            if let Some(v) = on_battery_low { p.push(format!("battery_low={:.1}", v)); }
            emit("react", &p.join("&"), cli.dry_run);
        }
    }
}
