//! Bevy plugin wiring for the terminal application.

use bevy::prelude::*;

use crate::direct_render::DirectTerminalRenderPlugin;
use crate::inline::{
    TerminalInlineObjectPlane, TerminalInlineObjectSprite, TerminalInlineObjects, TerminalRgpObject,
};
use crate::keyboard::{TerminalClipboard, TerminalKeyBindings, handle_keyboard_input};
use crate::mouse::{TerminalSelection, handle_mouse_input};
use crate::present::TerminalPresentPlugin;
use crate::scene::{
    MobiusTransition, StageTween, TerminalPlaneView, TerminalPresentation,
    TerminalPresentationMode, apply_terminal_presentation, setup_scene,
};
use crate::systems::{
    TerminalFrameDirty, TerminalRedrawSet, animate_inline_kitty_planes, animate_mobius_transition,
    animate_stage_tween, animate_terminal_plane_warp, apply_inline_objects,
    apply_instance_brightness, apply_rgp_restyle, apply_rgp_stage, finish_terminal_model_load,
    handle_window_resize, pump_pty_output, render_terminal_widget,
    request_exit_on_primary_window_close, shutdown_terminal_runtime_on_exit,
    sync_asset_to_terminal_cursor, sync_inline_objects, sync_rgp_objects, sync_terminal_materials,
};
use crate::terminal::TerminalRedrawState;

/// Inline object entities spawned since the visibility pass last ran.
type AddedInlineObjects<'w, 's> = Query<
    'w,
    's,
    (),
    Or<(
        Added<TerminalInlineObjectSprite>,
        Added<TerminalInlineObjectPlane>,
    )>,
>;

/// Main terminal plugin.
pub struct TerminalPlugin;

impl Plugin for TerminalPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TerminalSelection>()
            .init_resource::<crate::model::CursorSettings>()
            .init_resource::<TerminalInlineObjects>()
            .init_resource::<TerminalRedrawState>()
            .init_resource::<TerminalKeyBindings>()
            .init_resource::<TerminalFrameDirty>()
            .init_resource::<StageTween>()
            .init_non_send::<TerminalClipboard>()
            .add_systems(Startup, setup_scene)
            .add_systems(Update, request_exit_on_primary_window_close)
            .add_systems(Update, pump_pty_output)
            .add_systems(Update, handle_keyboard_input)
            .add_systems(Update, handle_mouse_input)
            .add_systems(Update, handle_window_resize)
            .add_systems(
                Update,
                apply_rgp_stage
                    .after(pump_pty_output)
                    .after(handle_keyboard_input)
                    .after(handle_mouse_input)
                    .run_if(|objects: Res<TerminalInlineObjects>| objects.has_pending_stage()),
            )
            .add_systems(
                Update,
                animate_stage_tween
                    .after(apply_rgp_stage)
                    .run_if(|stage_tween: Res<StageTween>| stage_tween.active),
            )
            .add_systems(
                Update,
                apply_terminal_presentation
                    .after(handle_keyboard_input)
                    .after(handle_mouse_input)
                    .after(apply_rgp_stage)
                    .after(animate_stage_tween)
                    .after(crate::ai::apply_ai_commands)
                    .run_if(
                        |presentation: Res<TerminalPresentation>,
                         plane_view: Res<TerminalPlaneView>,
                         mobius_transition: Res<MobiusTransition>| {
                            presentation.is_changed()
                                || plane_view.is_changed()
                                || mobius_transition.is_changed()
                        },
                    ),
            )
            .add_systems(
                Update,
                apply_inline_objects
                    .after(apply_terminal_presentation)
                    .run_if(
                        |presentation: Res<TerminalPresentation>, added: AddedInlineObjects| {
                            presentation.is_changed() || !added.is_empty()
                        },
                    ),
            )
            .configure_sets(
                Update,
                TerminalRedrawSet
                    .after(handle_mouse_input)
                    .after(handle_keyboard_input)
                    .after(handle_window_resize)
                    .after(pump_pty_output),
            )
            .add_systems(
                Update,
                (
                    render_terminal_widget,
                    sync_terminal_materials,
                    finish_terminal_model_load,
                )
                    .chain()
                    .in_set(TerminalRedrawSet),
            )
            .add_systems(Update, sync_inline_objects.after(TerminalRedrawSet))
            .add_systems(
                Update,
                animate_inline_kitty_planes.after(sync_inline_objects),
            )
            .add_systems(
                Update,
                sync_rgp_objects
                    .after(sync_inline_objects)
                    .run_if(|objects: Query<(), With<TerminalRgpObject>>| !objects.is_empty()),
            )
            .add_systems(
                Update,
                apply_rgp_restyle
                    .after(sync_inline_objects)
                    .run_if(|objects: Res<TerminalInlineObjects>| objects.has_restyle_objects()),
            )
            .add_systems(
                Update,
                apply_instance_brightness
                    .after(sync_rgp_objects)
                    .after(apply_rgp_restyle),
            )
            .add_systems(
                Update,
                animate_mobius_transition.run_if(
                    |presentation: Res<TerminalPresentation>,
                     mobius_transition: Res<MobiusTransition>| {
                        presentation.mode == TerminalPresentationMode::Mobius3d
                            || mobius_transition.active
                    },
                ),
            )
            .add_systems(
                Update,
                animate_terminal_plane_warp.run_if(|presentation: Res<TerminalPresentation>| {
                    presentation.mode != TerminalPresentationMode::Flat2d
                }),
            )
            .add_systems(
                Update,
                sync_asset_to_terminal_cursor.after(TerminalRedrawSet),
            )
            .add_systems(
                Update,
                // After the AI lowering so a `cursor` command's model swap
                // rebuilds the same frame; after the initial deferred spawn
                // so its commands are flushed before this system's query
                // runs (else a same-frame swap spawns a second cursor tree);
                // before the pose sync so the new tree is positioned at once.
                crate::systems::respawn_cursor_model
                    .after(crate::ai::apply_ai_object_commands)
                    .after(finish_terminal_model_load)
                    .before(sync_asset_to_terminal_cursor),
            )
            .add_systems(Last, shutdown_terminal_runtime_on_exit)
            .add_plugins(crate::ai::RattyAiPlugin)
            .add_plugins(crate::bookmarks::BookmarksPlugin)
            .add_plugins(crate::macros::MacrosPlugin)
            .add_plugins(crate::viz::VizPlugin)
            .add_plugins(crate::effects::AiEffectsPlugin)
            .add_plugins(crate::sound::SoundPlugin)
            .add_plugins(DirectTerminalRenderPlugin)
            .add_plugins(TerminalPresentPlugin);
    }
}
