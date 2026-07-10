//! `ratty-ai` command handling: the Bevy side of the OSC 777 control channel.
//!
//! [`crate::osc`] parses OSC 777 sequences into [`RattyAiCommand`]s inside the
//! parser callbacks; [`crate::systems::pump_pty_output`] drains them and emits
//! them as [`AiCommand`] messages. The handler systems here act on those
//! messages. Because the parser runs inside a Bevy system, no cross-thread
//! channel is needed — the messages are produced and consumed on the same
//! thread, the same frame.
//!
//! This first slice implements the commands that lower directly onto Ratty's
//! existing presentation resources (mode / warp / reset); the remaining
//! operator-console commands are logged until their subsystems are built, so
//! nothing is silently dropped and nothing is faked as working.

use bevy::ecs::message::{Message, MessageReader};
use bevy::prelude::*;

use crate::osc::RattyAiCommand;
use crate::scene::{
    MobiusTransition, StageTween, TerminalPlaneView, TerminalPlaneWarp, TerminalPresentation,
    TerminalPresentationMode, apply_stage_mode_change,
};
use crate::terminal::TerminalRedrawState;

/// A `ratty-ai` control command delivered to the Bevy world.
///
/// Wraps [`RattyAiCommand`] (which stays dependency-free so the `ratty-ai`
/// CLI can share the parser) so it can travel as a Bevy message.
#[derive(Message, Debug, Clone)]
pub struct AiCommand(pub RattyAiCommand);

/// Registers the AI command message and its handler systems.
pub struct RattyAiPlugin;

impl Plugin for RattyAiPlugin {
    fn build(&self, app: &mut App) {
        // Ordered after the RGP stage systems (not just pump_pty_output) so
        // that when an RGP `c` stage sequence and an OSC stage command arrive
        // in the same PTY chunk, the explicit AI command deterministically
        // wins the shared stage resources rather than racing an arbitrary
        // Bevy schedule tiebreak. apply_terminal_presentation is in turn
        // ordered after this system (see plugin.rs).
        app.add_message::<AiCommand>().add_systems(
            Update,
            apply_ai_commands
                .after(crate::systems::pump_pty_output)
                .after(crate::systems::apply_rgp_stage)
                .after(crate::systems::animate_stage_tween),
        );
    }
}

/// Applies queued `ratty-ai` commands to the presentation resources.
///
/// Mode/warp/reset lower onto the same machinery the RGP `c` verb uses, so
/// they take effect the frame they arrive. Commands whose subsystem does not
/// exist yet are logged rather than dropped.
pub fn apply_ai_commands(
    mut commands: MessageReader<AiCommand>,
    mut presentation: ResMut<TerminalPresentation>,
    mut plane_warp: ResMut<TerminalPlaneWarp>,
    mut plane_view: ResMut<TerminalPlaneView>,
    mut mobius: ResMut<MobiusTransition>,
    mut stage_tween: ResMut<StageTween>,
    mut redraw: ResMut<TerminalRedrawState>,
) {
    for AiCommand(command) in commands.read() {
        match command {
            RattyAiCommand::SetMode { mode } => {
                let Some(target) = parse_mode(mode) else {
                    warn!("ratty-ai: unknown mode '{mode}' (2d, 3d, mobius)");
                    continue;
                };
                if apply_stage_mode_change(target, &mut presentation, &plane_view, &mut mobius) {
                    stage_tween.stop();
                    redraw.request();
                }
            }
            RattyAiCommand::SetWarp { intensity } => {
                // An explicit warp command wins over a running camera tween.
                stage_tween.stop();
                plane_warp.amount = intensity.clamp(0.0, 1.0);
                redraw.request();
            }
            RattyAiCommand::Reset => {
                presentation.mode = TerminalPresentationMode::Flat2d;
                *plane_view = TerminalPlaneView::default();
                plane_warp.amount = 0.0;
                mobius.stop();
                stage_tween.stop();
                redraw.request();
            }
            // The soul: flash/pulse/tint/think/confidence/mood are handled by
            // the effects overlay (crate::effects), which reads the same
            // AiCommand messages independently.
            RattyAiCommand::Flash { .. }
            | RattyAiCommand::Pulse { .. }
            | RattyAiCommand::Tint { .. }
            | RattyAiCommand::Think { .. }
            | RattyAiCommand::Confidence { .. }
            | RattyAiCommand::Mood { .. } => {}
            other => {
                debug!("ratty-ai: command received, handler not yet built: {other:?}");
            }
        }
    }
}

/// Maps a CLI mode string to a presentation mode.
fn parse_mode(mode: &str) -> Option<TerminalPresentationMode> {
    match mode {
        "2d" | "flat" | "flat2d" => Some(TerminalPresentationMode::Flat2d),
        "3d" | "plane" | "plane3d" => Some(TerminalPresentationMode::Plane3d),
        "mobius" | "mobius3d" => Some(TerminalPresentationMode::Mobius3d),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_strings_map_to_presentation_modes() {
        assert_eq!(parse_mode("3d"), Some(TerminalPresentationMode::Plane3d));
        assert_eq!(parse_mode("2d"), Some(TerminalPresentationMode::Flat2d));
        assert_eq!(
            parse_mode("mobius"),
            Some(TerminalPresentationMode::Mobius3d)
        );
        assert_eq!(parse_mode("cube"), None);
    }
}
