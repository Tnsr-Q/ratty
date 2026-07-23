//! Caller-namespaced view bookmarks (#20's straggler block).
//!
//! A bookmark stores **only versioned public view state** — today the
//! presentation mode and plane warp, the two things wire commands can
//! set — under a name scoped to the caller's ingress namespace. It never
//! captures objects, macros, or private scene state, so replaying one
//! can never resurrect content the caller no longer owns.
//!
//! `bookmark.jump` validates the stored snapshot and reapplies it
//! **through normal command lowering**: the handler enqueues the same
//! `SetMode`/`SetWarp` commands the wire would carry, and the ordinary
//! appliers run them under the caller's current capabilities the same
//! frame. There is no privileged restore path to drift from the real
//! handlers.
//!
//! Collision rule (#16): storing an existing name rejects
//! `already-exists` unless `mode=replace` was supplied; jumping to an
//! absent name rejects `unknown-id`. `reset` clears every bookmark
//! silently along with the rest of the session state.

use std::collections::HashMap;

use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::prelude::*;

use crate::ai::AiCommand;
use crate::osc::RattyAiCommand;
use crate::query::codes;
use crate::query_channel::{AckOutcome, AiDiagnostics, ack_commit};
use crate::runtime::IngressSource;
use crate::scene::{TerminalPlaneWarp, TerminalPresentation, TerminalPresentationMode};

/// Upper bound on stored bookmarks per agent namespace: an honest
/// failure instead of an unbounded registry driven by untrusted output.
pub const MAX_BOOKMARKS_PER_NAMESPACE: usize = 16;

/// Upper bound, in bytes, on a bookmark name.
pub const MAX_BOOKMARK_NAME_BYTES: usize = 64;

/// The bookmark snapshot format this build stores and replays.
pub const BOOKMARK_VERSION: u32 = 1;

/// One stored view snapshot. Only versioned public view state — the
/// fields a wire command can set — ever lives here.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewBookmark {
    /// Snapshot format version ([`BOOKMARK_VERSION`]).
    pub v: u32,
    /// The presentation mode, as its canonical wire name.
    pub mode: &'static str,
    /// The plane warp intensity in `0..=1`.
    pub warp: f32,
}

/// The canonical wire name for a presentation mode (the reverse of the
/// `mode` command's parser, pinned by test against it).
pub(crate) fn mode_wire_name(mode: TerminalPresentationMode) -> &'static str {
    match mode {
        TerminalPresentationMode::Flat2d => "2d",
        TerminalPresentationMode::Plane3d => "3d",
        TerminalPresentationMode::Mobius3d => "mobius",
    }
}

/// Stored bookmarks, keyed by (namespace, name).
#[derive(Resource, Default)]
pub struct BookmarkRegistry {
    entries: HashMap<(u8, String), ViewBookmark>,
}

impl BookmarkRegistry {
    /// The bookmark stored under `name` in `namespace`.
    pub fn get(&self, namespace: u8, name: &str) -> Option<&ViewBookmark> {
        self.entries.get(&(namespace, name.to_string()))
    }

    /// Number of bookmarks stored in `namespace`.
    pub fn namespace_len(&self, namespace: u8) -> usize {
        self.entries
            .keys()
            .filter(|(entry_namespace, _)| *entry_namespace == namespace)
            .count()
    }

    /// Iterates `namespace`'s bookmarks in arbitrary order.
    pub fn iter_namespace(&self, namespace: u8) -> impl Iterator<Item = (&str, &ViewBookmark)> {
        self.entries
            .iter()
            .filter(move |((entry_namespace, _), _)| *entry_namespace == namespace)
            .map(|((_, name), bookmark)| (name.as_str(), bookmark))
    }

    fn insert(&mut self, namespace: u8, name: String, bookmark: ViewBookmark) {
        self.entries.insert((namespace, name), bookmark);
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Jump commands awaiting relowering: the applier both reads and (via
/// this buffer) produces `AiCommand`s, and a single system cannot hold
/// the message resource mutably and immutably at once. Drained into the
/// normal command stream by [`drain_bookmark_jumps`] the same frame.
#[derive(Resource, Default)]
pub struct PendingBookmarkJumps(Vec<(IngressSource, RattyAiCommand)>);

/// Registers the bookmark registry and its appliers.
///
/// Ordering: the applier runs after `pump_pty_output` (commands apply
/// the frame they arrive); the drain runs after it and **before
/// `apply_ai_commands`**, so a jump's relowered `SetMode`/`SetWarp` are
/// applied — with all their normal validation — in the same frame, and
/// a same-chunk `state.scene` observes the restored view
/// (`answer_queries` is ordered after both in
/// [`crate::ai::RattyAiPlugin`]).
pub struct BookmarksPlugin;

impl Plugin for BookmarksPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<BookmarkRegistry>()
            .init_resource::<PendingBookmarkJumps>()
            .add_systems(
                Update,
                apply_bookmark_commands.after(crate::systems::pump_pty_output),
            )
            .add_systems(
                Update,
                drain_bookmark_jumps
                    .after(apply_bookmark_commands)
                    .before(crate::ai::apply_ai_commands)
                    .run_if(|pending: Res<PendingBookmarkJumps>| !pending.0.is_empty()),
            );
    }
}

/// Applies queued `bookmark`/`bookmark.jump` commands. Owns their acks;
/// `reset` clears the registry silently (its single ack belongs to
/// `apply_ai_commands`).
pub fn apply_bookmark_commands(
    mut commands: MessageReader<AiCommand>,
    mut registry: ResMut<BookmarkRegistry>,
    presentation: Res<TerminalPresentation>,
    plane_warp: Res<TerminalPlaneWarp>,
    mut pending: ResMut<PendingBookmarkJumps>,
    mut acks: MessageWriter<AckOutcome>,
    mut diagnostics: ResMut<AiDiagnostics>,
) {
    for AiCommand {
        source,
        ack_token,
        command,
    } in commands.read()
    {
        macro_rules! reject {
            ($action:literal, $code:expr, $($message:tt)+) => {
                crate::query_channel::reject(
                    &mut diagnostics,
                    &mut acks,
                    *source,
                    ack_token,
                    $action,
                    $code,
                    format!($($message)+),
                )
            };
        }
        match command {
            RattyAiCommand::Bookmark { name, replace } => {
                let namespace = source.namespace();
                if name.is_empty() {
                    warn!("ratty-ai: bookmark rejected: empty name");
                    reject!("bookmark", codes::BAD_PAYLOAD, "name= must be non-empty");
                    continue;
                }
                if name.len() > MAX_BOOKMARK_NAME_BYTES {
                    warn!(
                        "ratty-ai: bookmark rejected: name exceeds \
                         {MAX_BOOKMARK_NAME_BYTES} bytes"
                    );
                    reject!(
                        "bookmark",
                        codes::TOO_LARGE,
                        "name exceeds {MAX_BOOKMARK_NAME_BYTES} bytes"
                    );
                    continue;
                }
                let exists = registry.get(namespace, name).is_some();
                if exists && !replace {
                    warn!("ratty-ai: bookmark rejected: '{name}' exists (pass mode=replace)");
                    reject!(
                        "bookmark",
                        codes::ALREADY_EXISTS,
                        "bookmark '{name}' exists (pass mode=replace to overwrite it)"
                    );
                    continue;
                }
                if !exists && registry.namespace_len(namespace) >= MAX_BOOKMARKS_PER_NAMESPACE {
                    warn!(
                        "ratty-ai: bookmark rejected: namespace {namespace} is at its \
                         {MAX_BOOKMARKS_PER_NAMESPACE}-bookmark limit"
                    );
                    reject!(
                        "bookmark",
                        codes::NAMESPACE_CAP,
                        "namespace {namespace} is at its \
                         {MAX_BOOKMARKS_PER_NAMESPACE}-bookmark limit"
                    );
                    continue;
                }
                registry.insert(
                    namespace,
                    name.clone(),
                    ViewBookmark {
                        v: BOOKMARK_VERSION,
                        mode: mode_wire_name(presentation.mode),
                        warp: plane_warp.amount.clamp(0.0, 1.0),
                    },
                );
                ack_commit(&mut acks, *source, ack_token);
            }
            RattyAiCommand::BookmarkJump { name } => {
                let namespace = source.namespace();
                let Some(bookmark) = registry.get(namespace, name) else {
                    warn!("ratty-ai: bookmark.jump rejected: no bookmark '{name}'");
                    reject!(
                        "bookmark.jump",
                        codes::UNKNOWN_ID,
                        "no bookmark '{name}' in the caller's namespace"
                    );
                    continue;
                };
                // Stored by this session, so always the current version —
                // kept as an honest reject rather than a debug assert so a
                // future persisted format can never replay blind.
                if bookmark.v != BOOKMARK_VERSION {
                    warn!(
                        "ratty-ai: bookmark.jump rejected: '{name}' is version {} \
                         (this build replays {BOOKMARK_VERSION})",
                        bookmark.v
                    );
                    reject!(
                        "bookmark.jump",
                        codes::UNSUPPORTED,
                        "bookmark '{name}' is version {} (this build replays \
                         {BOOKMARK_VERSION})",
                        bookmark.v
                    );
                    continue;
                }
                // Reapply through normal command lowering: the ordinary
                // appliers run these (validation included) this same
                // frame, under the caller's current capabilities.
                pending.0.push((
                    *source,
                    RattyAiCommand::SetMode {
                        mode: bookmark.mode.to_string(),
                    },
                ));
                pending.0.push((
                    *source,
                    RattyAiCommand::SetWarp {
                        intensity: bookmark.warp,
                    },
                ));
                ack_commit(&mut acks, *source, ack_token);
            }
            RattyAiCommand::Reset => {
                // Reset's single ack belongs to apply_ai_commands; the
                // bookmark registry clears silently.
                registry.clear();
            }
            _ => {}
        }
    }
}

/// Writes relowered jump commands into the normal `AiCommand` stream,
/// token-less (the jump already acked; the relowered commands report any
/// failure through the caller's `state.errors` ring like any other
/// fire-and-forget command).
pub fn drain_bookmark_jumps(
    mut pending: ResMut<PendingBookmarkJumps>,
    mut commands: MessageWriter<AiCommand>,
) {
    for (source, command) in pending.0.drain(..) {
        commands.write(AiCommand {
            source,
            ack_token: None,
            command,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::message::Messages;

    fn test_app() -> App {
        let mut app = App::new();
        app.init_resource::<BookmarkRegistry>();
        app.init_resource::<PendingBookmarkJumps>();
        app.init_resource::<AiDiagnostics>();
        app.insert_resource(TerminalPresentation {
            mode: TerminalPresentationMode::Flat2d,
        });
        app.init_resource::<TerminalPlaneWarp>();
        app.add_message::<AiCommand>();
        app.add_message::<AckOutcome>();
        app.add_systems(Update, (apply_bookmark_commands, drain_bookmark_jumps).chain());
        app
    }

    fn send_tok(app: &mut App, command: RattyAiCommand) -> (bool, Option<&'static str>) {
        app.world_mut()
            .resource_mut::<Messages<AiCommand>>()
            .write(AiCommand {
                source: IngressSource::Local,
                ack_token: Some("t".to_string()),
                command,
            });
        app.update();
        let mut messages = app.world_mut().resource_mut::<Messages<AckOutcome>>();
        let outcomes: Vec<AckOutcome> = messages.drain().collect();
        assert_eq!(outcomes.len(), 1, "exactly one ack per tok= command");
        (outcomes[0].ok, outcomes[0].code)
    }

    fn bookmark(name: &str, replace: bool) -> RattyAiCommand {
        RattyAiCommand::Bookmark {
            name: name.to_string(),
            replace,
        }
    }

    #[test]
    fn limits_are_pinned() {
        assert_eq!(MAX_BOOKMARKS_PER_NAMESPACE, 16);
        assert_eq!(MAX_BOOKMARK_NAME_BYTES, 64);
        assert_eq!(BOOKMARK_VERSION, 1);
    }

    #[test]
    fn wire_names_round_trip_through_the_mode_parser() {
        // The stored mode must replay through the real `mode` handler;
        // this pins the reverse map to the parser it feeds.
        for mode in [
            TerminalPresentationMode::Flat2d,
            TerminalPresentationMode::Plane3d,
            TerminalPresentationMode::Mobius3d,
        ] {
            let name = mode_wire_name(mode);
            assert_eq!(
                crate::ai::parse_mode(name),
                Some(mode),
                "'{name}' replays onto its own mode"
            );
        }
    }

    #[test]
    fn store_collide_replace_and_jump() {
        let mut app = test_app();
        let (ok, _) = send_tok(&mut app, bookmark("dock", false));
        assert!(ok);
        // Same name again: the #16 collision rule.
        let (ok, code) = send_tok(&mut app, bookmark("dock", false));
        assert!(!ok);
        assert_eq!(code, Some(codes::ALREADY_EXISTS));
        let (ok, _) = send_tok(&mut app, bookmark("dock", true));
        assert!(ok, "mode=replace overwrites");

        // Jumping relowers SetMode + SetWarp into the command stream.
        // Clear older command messages first so the drained stream holds
        // exactly the jump's frame: the jump itself, then its two
        // relowered commands, in order.
        app.world_mut()
            .resource_mut::<Messages<AiCommand>>()
            .clear();
        let (ok, _) = send_tok(
            &mut app,
            RattyAiCommand::BookmarkJump {
                name: "dock".to_string(),
            },
        );
        assert!(ok);
        let mut messages = app.world_mut().resource_mut::<Messages<AiCommand>>();
        let relowered: Vec<String> = messages
            .drain()
            .map(|message| format!("{:?}", message.command))
            .collect();
        assert_eq!(relowered.len(), 3, "{relowered:?}");
        assert!(relowered[1].starts_with("SetMode"), "{relowered:?}");
        assert!(relowered[2].starts_with("SetWarp"), "{relowered:?}");

        // Jumping to an absent name is an honest failure.
        let (ok, code) = send_tok(
            &mut app,
            RattyAiCommand::BookmarkJump {
                name: "ghost".to_string(),
            },
        );
        assert!(!ok);
        assert_eq!(code, Some(codes::UNKNOWN_ID));
    }

    #[test]
    fn names_are_validated_and_the_namespace_is_bounded() {
        let mut app = test_app();
        let (ok, code) = send_tok(&mut app, bookmark("", false));
        assert!(!ok);
        assert_eq!(code, Some(codes::BAD_PAYLOAD));
        let (ok, code) = send_tok(
            &mut app,
            bookmark(&"x".repeat(MAX_BOOKMARK_NAME_BYTES + 1), false),
        );
        assert!(!ok);
        assert_eq!(code, Some(codes::TOO_LARGE));

        for index in 0..MAX_BOOKMARKS_PER_NAMESPACE {
            let (ok, _) = send_tok(&mut app, bookmark(&format!("b{index}"), false));
            assert!(ok);
        }
        let (ok, code) = send_tok(&mut app, bookmark("overflow", false));
        assert!(!ok);
        assert_eq!(code, Some(codes::NAMESPACE_CAP));
        // Replacing at the cap is not a new slot.
        let (ok, _) = send_tok(&mut app, bookmark("b0", true));
        assert!(ok, "replace never counts against the cap");
    }

    #[test]
    fn reset_clears_bookmarks_silently() {
        let mut app = test_app();
        let (ok, _) = send_tok(&mut app, bookmark("dock", false));
        assert!(ok);
        app.world_mut()
            .resource_mut::<Messages<AiCommand>>()
            .write(AiCommand {
                source: IngressSource::Local,
                ack_token: Some("reset-tok".to_string()),
                command: RattyAiCommand::Reset,
            });
        app.update();
        let mut messages = app.world_mut().resource_mut::<Messages<AckOutcome>>();
        let outcomes: Vec<AckOutcome> = messages.drain().collect();
        assert!(outcomes.is_empty(), "the bookmark applier never acks reset");
        assert_eq!(
            app.world().resource::<BookmarkRegistry>().namespace_len(0),
            0,
            "reset clears every bookmark"
        );
    }
}
