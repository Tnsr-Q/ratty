//! Silk: compiler, validator, player, and indexer for `.silk` transmissions.
//!
//! A transmission is a replayable ratty byte stream — text, ANSI, and Ratty
//! Graphics Protocol sequences with timing — per `protocols/silk.md`.

mod cast;
mod compile;
mod play;
mod scene;
mod validate;

// Reuse ratty's own RGP parser verbatim so the validator can never drift from
// the terminal's actual wire format.
#[expect(dead_code, reason = "shared parser module; silk uses a subset")]
#[path = "../../../src/rgp.rs"]
mod rgp;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "silk",
    version,
    about = "Compose, validate, replay, and index Silk transmissions (.silk)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compile a scene.json into a .silk cast
    Compile {
        /// Scene source file (scene.json)
        scene: PathBuf,
        /// Output cast path (defaults to cast.silk next to the scene)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Validate a .silk cast and print stats
    Validate {
        /// Cast file to validate
        cast: PathBuf,
    },
    /// Replay a cast to stdout with real timing (run inside ratty: `ratty -e silk play <cast>`)
    Play {
        /// Cast file to play
        cast: PathBuf,
        /// Playback speed multiplier
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
        /// Loop the transmission until interrupted
        #[arg(long, default_value_t = false)]
        r#loop: bool,
    },
    /// Build an index.json over a directory of transmissions
    Index {
        /// Directory containing <slug>/cast.silk transmissions
        dir: PathBuf,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("silk: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Command::Compile { scene, output } => {
            let output = output.unwrap_or_else(|| {
                scene
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new("."))
                    .join("cast.silk")
            });
            let stats = compile::compile_file(&scene, &output)
                .with_context(|| format!("failed to compile {}", scene.display()))?;
            println!(
                "compiled {} -> {} ({} events, {:.2}s, {} bytes)",
                scene.display(),
                output.display(),
                stats.events,
                stats.duration_secs,
                stats.bytes,
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Validate { cast } => {
            let report = validate::validate_file(&cast)
                .with_context(|| format!("failed to read {}", cast.display()))?;
            report.print();
            if report.errors.is_empty() {
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(ExitCode::FAILURE)
            }
        }
        Command::Play {
            cast,
            speed,
            r#loop,
        } => {
            play::play_file(&cast, speed, r#loop)
                .with_context(|| format!("failed to play {}", cast.display()))?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Index { dir } => {
            let count = cast::write_index(&dir)
                .with_context(|| format!("failed to index {}", dir.display()))?;
            println!(
                "indexed {count} transmissions -> {}/index.json",
                dir.display()
            );
            Ok(ExitCode::SUCCESS)
        }
    }
}
