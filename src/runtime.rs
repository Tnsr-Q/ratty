//! Terminal transport (PTY or virtual byte channel) and parser state.

use std::collections::HashSet;
#[cfg(not(target_arch = "wasm32"))]
use std::env;
use std::io::Write;
#[cfg(not(target_arch = "wasm32"))]
use std::io::{ErrorKind, Read};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
#[cfg(not(target_arch = "wasm32"))]
use std::thread::{self, JoinHandle};

#[cfg(not(target_arch = "wasm32"))]
use anyhow::Context;
use bevy::platform::cell::SyncCell;
use bevy::prelude::Resource;
#[cfg(not(target_arch = "wasm32"))]
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use vt100::{Callbacks, Parser, Screen};

use crate::config::AppConfig;
use crate::osc::{RattyAiCommand, parse_osc};

/// Command-line runtime overrides.
#[derive(Debug, Clone, Default)]
pub struct RuntimeOptions {
    /// Command and arguments to execute instead of the configured shell.
    pub command: Option<Vec<String>>,
    /// Working directory used for the spawned PTY command.
    pub working_dir: Option<PathBuf>,
}

/// Callback state for unhandled parser sequences.
#[derive(Default)]
pub struct TerminalParserCallbacks {
    seen_csi: HashSet<String>,
    seen_escape: HashSet<String>,
    pending_replies: Vec<Vec<u8>>,
    pending_ai: Vec<RattyAiCommand>,
    kitty_keyboard_flags: u8,
    modify_other_keys: Option<u8>,
}

impl TerminalParserCallbacks {
    /// Drains any terminal replies queued by parser callbacks.
    pub fn take_replies(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pending_replies)
    }

    /// Drains any `ratty-ai` OSC 777 control commands queued by the parser.
    pub fn take_ai_commands(&mut self) -> Vec<RattyAiCommand> {
        std::mem::take(&mut self.pending_ai)
    }

    /// Returns the active kitty keyboard enhancement flags.
    pub fn kitty_keyboard_flags(&self) -> u8 {
        self.kitty_keyboard_flags
    }

    /// Returns the active xterm `modifyOtherKeys` level.
    pub fn modify_other_keys(&self) -> Option<u8> {
        self.modify_other_keys
    }
}

impl Callbacks for TerminalParserCallbacks {
    fn unhandled_csi(
        &mut self,
        screen: &mut Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        // CSI 0 c = primary device attributes request.
        if i1.is_none() && i2.is_none() && c == 'c' && params.len() == 1 && params[0] == [0] {
            self.pending_replies.push(b"\x1b[?1;2c".to_vec());
            return;
        }

        // CSI 5 n = device status report request.
        if i1.is_none() && i2.is_none() && c == 'n' && params.len() == 1 && params[0] == [5] {
            self.pending_replies.push(b"\x1b[0n".to_vec());
            return;
        }

        // CSI 6 n = cursor position report request.
        if i1.is_none() && i2.is_none() && c == 'n' && params.len() == 1 && params[0] == [6] {
            let (row, col) = screen.cursor_position();
            self.pending_replies
                .push(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes());
            return;
        }

        // CSI ? u = kitty keyboard protocol flag query. Reply with the currently active flags so
        // apps can detect whether enhanced key reporting is enabled.
        if i1 == Some(b'?') && i2.is_none() && c == 'u' && params.is_empty() {
            self.pending_replies
                .push(format!("\x1b[?{}u", self.kitty_keyboard_flags).into_bytes());
            return;
        }

        // CSI > flags u = enable kitty keyboard protocol flags for subsequent key reports.
        if i1 == Some(b'>') && i2.is_none() && c == 'u' && params.len() == 1 && params[0].len() == 1
        {
            self.kitty_keyboard_flags = params[0][0].min(u8::MAX as u16) as u8;
            return;
        }

        // CSI < 1 u = pop kitty keyboard enhancement state and fall back to legacy reporting.
        if i1 == Some(b'<') && i2.is_none() && c == 'u' && params.len() == 1 && params[0] == [1] {
            self.kitty_keyboard_flags = 0;
            return;
        }

        // CSI > 4 ; level m = xterm modifyOtherKeys mode. We track the current level so keys like
        // Ctrl+Enter can be encoded in the form the foreground app asked for.
        if i1 == Some(b'>') && i2.is_none() && c == 'm' {
            match params {
                [resource, level] if *resource == [4] && level.len() == 1 => {
                    self.modify_other_keys = Some(level[0].min(u8::MAX as u16) as u8);
                    return;
                }
                [resource] if *resource == [4] => {
                    self.modify_other_keys = None;
                    return;
                }
                _ => {}
            }
        }

        // CSI ? 7 h / CSI ? 7 l toggle line wrapping. Ratty does not model the mode yet, but
        // treating it as known avoids noisy warnings for shells and TUIs that flip it frequently.
        if i1 == Some(b'?')
            && i2.is_none()
            && params.len() == 1
            && params[0] == [7]
            && matches!(c, 'h' | 'l')
        {
            return;
        }

        let mut sequence = String::from("\u{1b}[");
        if let Some(i1) = i1 {
            sequence.push(i1 as char);
        }
        if let Some(i2) = i2 {
            sequence.push(i2 as char);
        }
        for (idx, param) in params.iter().enumerate() {
            if idx > 0 {
                sequence.push(';');
            }
            for (j, value) in param.iter().enumerate() {
                if j > 0 {
                    sequence.push(':');
                }
                sequence.push_str(&value.to_string());
            }
        }
        sequence.push(c);

        if self.seen_csi.insert(sequence.clone()) {
            bevy::log::warn!("unhandled terminal CSI sequence: {sequence}");
        }
    }

    fn unhandled_osc(&mut self, _: &mut Screen, params: &[&[u8]]) {
        // OSC 777 with the `ratty:` namespace is the ratty-ai control
        // channel; anything else (other OSC 777 users, unknown OSC codes)
        // is ignored. Commands fire inside `pump_pty_output` on the Bevy
        // thread, so they queue here and drain there — no channel needed.
        if let Some(command) = parse_osc(params) {
            self.pending_ai.push(command);
        }
    }

    fn unhandled_escape(&mut self, _: &mut Screen, i1: Option<u8>, i2: Option<u8>, b: u8) {
        let mut sequence = String::from("\u{1b}");
        if let Some(i1) = i1 {
            sequence.push(i1 as char);
        }
        if let Some(i2) = i2 {
            sequence.push(i2 as char);
        }
        sequence.push(b as char);

        if self.seen_escape.insert(sequence.clone()) {
            bevy::log::warn!("unhandled terminal escape sequence: {sequence}");
        }
    }
}

/// Running terminal transport and parser state.
///
/// The transport is either a real PTY (native) or a virtual byte channel
/// (see [`TerminalRuntime::virtual_channel`]); everything downstream only
/// consumes [`TerminalRuntime::try_recv`], [`TerminalRuntime::write_input`],
/// and [`TerminalRuntime::parser`], so the two are interchangeable.
///
/// The `!Sync` PTY handles (the output channel receiver and the master) live
/// in [`SyncCell`]s so the runtime qualifies as a regular [`Resource`] and
/// systems using it are not pinned to the main thread.
#[derive(Resource)]
pub struct TerminalRuntime {
    /// Terminal output channel (PTY reader or virtual feed).
    rx: SyncCell<Receiver<Vec<u8>>>,
    /// Terminal input writer (PTY writer or virtual input forwarder).
    pub writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    /// PTY master handle.
    #[cfg(not(target_arch = "wasm32"))]
    master: SyncCell<Option<Box<dyn MasterPty + Send>>>,
    /// Child process handle.
    #[cfg(not(target_arch = "wasm32"))]
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    /// PTY reader thread.
    #[cfg(not(target_arch = "wasm32"))]
    reader_thread: Option<JoinHandle<()>>,
    /// Terminal parser.
    pub parser: Parser<TerminalParserCallbacks>,
    scrollback_len: usize,
    /// Indicates transport shutdown.
    pub pty_disconnected: bool,
    shutdown_started: bool,
}

/// Host-side handles for a virtual terminal transport created by
/// [`TerminalRuntime::virtual_channel`].
///
/// The host plays the role the PTY plays in a native session: it produces
/// terminal output bytes and consumes whatever the terminal writes back
/// (keystrokes, RGP support replies, cursor position reports).
pub struct VirtualTerminalHost {
    /// Producer for terminal output bytes (text, ANSI, RGP, Kitty).
    pub feed_tx: Sender<Vec<u8>>,
    /// Consumer for bytes the terminal writes back as input.
    pub input_rx: Receiver<Vec<u8>>,
}

/// [`Write`] adapter that forwards terminal input into the virtual host's
/// channel instead of a PTY.
struct VirtualInputWriter {
    tx: Sender<Vec<u8>>,
}

impl Write for VirtualInputWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Unbounded channel: never blocks. A closed host just discards.
        let _ = self.tx.send(buf.to_vec());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Returns the default shell for the current platform.
///
/// On Windows this prefers Git for Windows' `bash.exe` when it can be found
/// (most users running terminal apps on Windows want a POSIX shell so the
/// Ratatui demos behave the same as on Linux/macOS), then `%COMSPEC%` (the
/// resolved command processor), and finally `cmd.exe`. On other platforms
/// it falls back to `/bin/sh`.
#[cfg(not(target_arch = "wasm32"))]
fn default_shell() -> String {
    #[cfg(windows)]
    {
        if let Some(bash) = find_git_bash() {
            return bash;
        }
        env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
    }
    #[cfg(not(windows))]
    {
        "/bin/sh".to_string()
    }
}

/// Looks for a Git for Windows `bash.exe` in the well-known install
/// locations, then on `PATH`. Returns the first match.
///
/// `usr/bin/bash.exe` is the MSYS shell bundled with Git for Windows;
/// `bin/bash.exe` is the shim used by the Git Bash launcher. Either works
/// as a PTY shell.
#[cfg(windows)]
fn find_git_bash() -> Option<String> {
    use std::path::PathBuf;

    // Flat candidate table keeps every probe path on one footing: each entry
    // is `(env_var, subpath_under_that_directory)`. New install layouts (Git
    // via Scoop, Chocolatey, custom installers) only need another row here.
    const CANDIDATES: &[(&str, &str)] = &[
        ("ProgramW6432", "Git/bin/bash.exe"),
        ("ProgramW6432", "Git/usr/bin/bash.exe"),
        ("ProgramFiles", "Git/bin/bash.exe"),
        ("ProgramFiles", "Git/usr/bin/bash.exe"),
        ("ProgramFiles(x86)", "Git/bin/bash.exe"),
        ("ProgramFiles(x86)", "Git/usr/bin/bash.exe"),
        ("LOCALAPPDATA", "Programs/Git/bin/bash.exe"),
        ("LOCALAPPDATA", "Programs/Git/usr/bin/bash.exe"),
    ];

    for (env_var, sub) in CANDIDATES {
        let Ok(base) = env::var(env_var) else {
            continue;
        };
        let candidate = PathBuf::from(base).join(sub);
        if candidate.is_file() {
            return candidate.into_os_string().into_string().ok();
        }
    }

    // Final fallback: walk PATH so custom installs (Scoop shims, etc.) work.
    if let Ok(path) = env::var("PATH") {
        for entry in env::split_paths(&path) {
            let candidate = entry.join("bash.exe");
            if candidate.is_file() {
                return candidate.into_os_string().into_string().ok();
            }
        }
    }

    None
}

impl TerminalRuntime {
    /// Creates a runtime backed by a virtual byte channel instead of a PTY.
    ///
    /// The returned [`VirtualTerminalHost`] feeds terminal output through
    /// `feed_tx` and drains terminal input from `input_rx`. Channels are
    /// unbounded so producing never blocks — a blocking send would hang a
    /// single-threaded (e.g. wasm) embedder permanently.
    pub fn virtual_channel(config: &AppConfig) -> (Self, VirtualTerminalHost) {
        let cols = config.terminal.default_cols;
        let rows = config.terminal.default_rows;
        let (feed_tx, rx) = mpsc::channel::<Vec<u8>>();
        let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>();

        let runtime = Self {
            rx: SyncCell::new(rx),
            writer: Arc::new(Mutex::new(Some(
                Box::new(VirtualInputWriter { tx: input_tx }) as Box<dyn Write + Send>,
            ))),
            #[cfg(not(target_arch = "wasm32"))]
            master: SyncCell::new(None),
            #[cfg(not(target_arch = "wasm32"))]
            child: None,
            #[cfg(not(target_arch = "wasm32"))]
            reader_thread: None,
            parser: Parser::new_with_callbacks(
                rows,
                cols,
                config.terminal.scrollback,
                TerminalParserCallbacks::default(),
            ),
            scrollback_len: config.terminal.scrollback,
            pty_disconnected: false,
            shutdown_started: false,
        };
        (runtime, VirtualTerminalHost { feed_tx, input_rx })
    }

    /// Spawns the shell PTY runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if the PTY cannot be created or the shell cannot be spawned.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn spawn(config: &AppConfig, options: &RuntimeOptions) -> anyhow::Result<Self> {
        let cols = config.terminal.default_cols;
        let rows = config.terminal.default_rows;
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to create PTY pair")?;

        let mut cmd = if let Some(command) = &options.command {
            let mut command = command.iter();
            let program = command
                .next()
                .context("command override must contain at least one argument")?;
            let mut cmd = CommandBuilder::new(program);
            cmd.args(command);
            cmd
        } else {
            let shell = config
                .shell
                .program
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .or_else(|| env::var("SHELL").ok())
                .unwrap_or_else(default_shell);
            let mut cmd = CommandBuilder::new(shell);
            cmd.args(&config.shell.args);
            cmd
        };

        if let Some(working_dir) = &options.working_dir {
            cmd.cwd(working_dir);
        }
        if !config.env.contains_key("TERM") {
            cmd.env("TERM", "xterm-256color");
        }
        for (key, value) in &config.env {
            cmd.env(key, value);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn shell")?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to create PTY writer")?;

        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(16);
        let reader_thread = thread::spawn(move || {
            let mut buf = [0_u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(size) => {
                        if tx.send(buf[..size].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            rx: SyncCell::new(rx),
            writer: Arc::new(Mutex::new(Some(writer))),
            master: SyncCell::new(Some(pair.master)),
            child: Some(child),
            reader_thread: Some(reader_thread),
            parser: Parser::new_with_callbacks(
                rows,
                cols,
                config.terminal.scrollback,
                TerminalParserCallbacks::default(),
            ),
            scrollback_len: config.terminal.scrollback,
            pty_disconnected: false,
            shutdown_started: false,
        })
    }

    /// Receives pending PTY output without blocking.
    pub fn try_recv(&mut self) -> Result<Vec<u8>, TryRecvError> {
        self.rx.get().try_recv()
    }

    /// Writes input bytes to the PTY.
    pub fn write_input(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        if let Ok(mut writer) = self.writer.lock()
            && let Some(writer) = writer.as_mut()
        {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    /// Resizes the transport (PTY when present) and parser screen.
    pub fn resize(&mut self, cols: u16, rows: u16, pw: u16, ph: u16) {
        if cols == 0 || rows == 0 {
            return;
        }

        #[cfg(not(target_arch = "wasm32"))]
        if let Some(master) = self.master.get().as_ref() {
            let _ = master.resize(PtySize {
                rows,
                cols,
                pixel_width: pw,
                pixel_height: ph,
            });
        }
        #[cfg(target_arch = "wasm32")]
        let _ = (pw, ph);

        let (_, old_cols) = self.parser.screen().size();
        if old_cols == cols || self.parser.screen().alternate_screen() {
            self.parser.screen_mut().set_size(rows, cols);
            return;
        }

        let state = self.parser.screen().state_formatted();
        let callbacks = std::mem::take(self.parser.callbacks_mut());
        self.parser = Parser::new_with_callbacks(rows, cols, self.scrollback_len, callbacks);
        self.parser.process(&state);
    }

    /// Returns the active kitty keyboard enhancement flags.
    pub fn kitty_keyboard_flags(&self) -> u8 {
        self.parser.callbacks().kitty_keyboard_flags()
    }

    /// Returns the active xterm `modifyOtherKeys` level.
    pub fn modify_other_keys(&self) -> Option<u8> {
        self.parser.callbacks().modify_other_keys()
    }

    /// Shuts down the transport without blocking the Bevy main thread indefinitely.
    pub fn shutdown(&mut self) {
        if self.shutdown_started {
            return;
        }
        self.shutdown_started = true;
        self.pty_disconnected = true;

        if let Ok(mut writer) = self.writer.lock() {
            writer.take();
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
            }
            self.child.take();
            self.master.get().take();

            if self
                .reader_thread
                .as_ref()
                .is_some_and(JoinHandle::is_finished)
                && let Some(reader_thread) = self.reader_thread.take()
            {
                let _ = reader_thread.join();
            }
        }
    }
}

impl Drop for TerminalRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_channel_round_trips_output_and_input() {
        let config = AppConfig::default();
        let (mut runtime, host) = TerminalRuntime::virtual_channel(&config);

        host.feed_tx
            .send(b"hello".to_vec())
            .expect("virtual feed should accept bytes");
        assert_eq!(
            runtime.try_recv().expect("fed bytes should arrive"),
            b"hello".to_vec()
        );
        assert!(runtime.try_recv().is_err());

        runtime.write_input(b"\x1b[A");
        assert_eq!(
            host.input_rx
                .try_recv()
                .expect("terminal input should reach the host"),
            b"\x1b[A".to_vec()
        );
    }

    #[test]
    fn virtual_channel_feed_never_blocks() {
        let config = AppConfig::default();
        let (_runtime, host) = TerminalRuntime::virtual_channel(&config);
        // Far beyond the old sync_channel(16) bound: must not block or fail.
        for _ in 0..1000 {
            host.feed_tx
                .send(vec![0_u8; 1024])
                .expect("unbounded feed should never fail");
        }
    }

    #[test]
    fn virtual_channel_resize_updates_parser() {
        let config = AppConfig::default();
        let (mut runtime, _host) = TerminalRuntime::virtual_channel(&config);
        runtime.resize(80, 24, 0, 0);
        assert_eq!(runtime.parser.screen().size(), (24, 80));
    }

    #[test]
    fn osc_777_bytes_reach_the_ai_command_queue() {
        // Drive real OSC 777 bytes through vt100 and confirm the
        // unhandled_osc hook parsed and queued the command. BEL-terminated,
        // exactly what the ratty-ai CLI emits.
        let mut parser = Parser::new_with_callbacks(24, 80, 0, TerminalParserCallbacks::default());
        parser.process(b"\x1b]777;ratty:mode;3d\x07");
        parser.process(b"\x1b]777;ratty:warp;intensity=0.5\x07");
        let commands = parser.callbacks_mut().take_ai_commands();
        assert_eq!(
            commands,
            vec![
                RattyAiCommand::SetMode {
                    mode: "3d".to_string()
                },
                RattyAiCommand::SetWarp { intensity: 0.5 },
            ]
        );
        // Drained.
        assert!(parser.callbacks_mut().take_ai_commands().is_empty());
    }

    #[test]
    fn unrelated_osc_is_ignored_by_the_ai_hook() {
        // A window-title OSC (handled natively) and a foreign OSC 777 must
        // not produce ratty-ai commands.
        let mut parser = Parser::new_with_callbacks(24, 80, 0, TerminalParserCallbacks::default());
        parser.process(b"\x1b]0;my title\x07");
        parser.process(b"\x1b]777;other:thing;x=1\x07");
        assert!(parser.callbacks_mut().take_ai_commands().is_empty());
    }
}
