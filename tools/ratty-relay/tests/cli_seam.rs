//! The attach-seam verification #46 was chartered to perform
//! (`docs/research/relay-design.md`, "Attach"): does clap deliver a bare
//! `--` through ratty's `-e`/`--command` capture into the relay's argv?
//!
//! The first test replicates ratty's own `Cli` parsing attributes
//! (src/cli.rs — same clap 4.5, same `trailing_var_arg` +
//! `num_args = 1..` + `allow_hyphen_values`) so the behavior pinned here
//! is the behavior the real binary exhibits, without touching the crate.

use clap::Parser;

/// ratty's `Cli`, reduced to the fields that shape parsing (src/cli.rs).
#[derive(Debug, Parser)]
#[command(name = "ratty", trailing_var_arg = true)]
struct RattyCliReplica {
    #[arg(short = 'c', long = "config-file")]
    config_file: Option<std::path::PathBuf>,

    #[arg(
        short = 'e',
        long = "command",
        num_args = 1..,
        allow_hyphen_values = true
    )]
    command: Option<Vec<String>>,
}

#[test]
fn ratty_dash_e_passes_the_bare_separator_through() {
    // `ratty -e ratty-relay host --listen ADDR -- zsh`
    let cli = RattyCliReplica::parse_from([
        "ratty",
        "-e",
        "ratty-relay",
        "host",
        "--listen",
        "127.0.0.1:7877",
        "--",
        "zsh",
    ]);
    let command = cli.command.expect("-e captured a command vector");
    // The pinned answer to the design's open question: with
    // `allow_hyphen_values` + greedy `num_args`, clap keeps consuming
    // through the bare `--`, so the relay receives it as a literal argv
    // element and must tolerate it (resolve_command strips a leading one
    // after subcommand parsing).
    assert_eq!(
        command,
        vec![
            "ratty-relay",
            "host",
            "--listen",
            "127.0.0.1:7877",
            "--",
            "zsh"
        ]
    );
}

// ---- the relay's own argv handling, same shapes the docs advertise ----

#[derive(Debug, Parser)]
#[command(name = "ratty-relay")]
struct RelayCliReplica {
    #[command(subcommand)]
    mode: RelayMode,
}

#[derive(Debug, clap::Subcommand)]
enum RelayMode {
    #[command(trailing_var_arg = true)]
    Host {
        #[arg(long, default_value = "127.0.0.1:7877")]
        listen: String,
        #[arg(long, conflicts_with = "command")]
        cmd: Option<String>,
        #[arg(allow_hyphen_values = true)]
        command: Vec<String>,
    },
}

#[test]
fn relay_host_accepts_the_advertised_argv_shapes() {
    // As launched through ratty (the separator survives -e, above).
    let cli = RelayCliReplica::parse_from([
        "ratty-relay",
        "host",
        "--listen",
        "127.0.0.1:7877",
        "--",
        "zsh",
        "-l",
    ]);
    let RelayMode::Host {
        listen, command, ..
    } = cli.mode;
    assert_eq!(listen, "127.0.0.1:7877");
    // clap treats the first bare `--` as the positional escape; everything
    // after lands in the trailing capture — including hyphen values.
    assert_eq!(command, vec!["zsh", "-l"]);

    // Directly, without a separator, hyphen-led shell args still parse.
    let cli = RelayCliReplica::parse_from(["ratty-relay", "host", "zsh", "-l"]);
    let RelayMode::Host { command, .. } = cli.mode;
    assert_eq!(command, vec!["zsh", "-l"]);

    // The `--cmd` fallback for embeddings that cannot pass trailing args.
    let cli = RelayCliReplica::parse_from(["ratty-relay", "host", "--cmd", "zsh -l"]);
    let RelayMode::Host { cmd, command, .. } = cli.mode;
    assert_eq!(cmd.as_deref(), Some("zsh -l"));
    assert!(command.is_empty());
}
