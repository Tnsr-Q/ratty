//! Bevy plugin wiring for the terminal application.

use bevy::prelude::*;

use crate::direct_render::DirectTerminalRenderPlugin;
use crate::focus::{
    FocusGained, FocusLost, FocusRequest, FocusedTerminal, drain_focus_requests,
    focus_boot_terminal,
};
use crate::inline::{
    TerminalInlineObjectPlane, TerminalInlineObjectSprite, TerminalInlineObjects, TerminalRgpObject,
};
use crate::keyboard::{TerminalClipboard, TerminalKeyBindings, handle_keyboard_input};
use crate::mouse::{TerminalSelection, handle_mouse_input};
use crate::present::TerminalPresentPlugin;
use crate::scene::{
    MobiusTransition, StageTween, TerminalPlaneView, TerminalPresentation,
    TerminalPresentationMode, TerminalSpawnRequested, apply_terminal_presentation, setup_scene,
    spawn_requested_terminals,
};
use crate::systems::{
    TerminalRedrawSet, animate_inline_kitty_planes, animate_mobius_transition, animate_stage_tween,
    animate_terminal_plane_warp, apply_inline_objects, apply_instance_brightness,
    apply_rgp_restyle, apply_rgp_stage, finish_terminal_model_load, handle_window_resize,
    pump_pty_output, render_terminal_widget, request_exit_on_primary_window_close,
    shutdown_terminal_runtime_on_exit, sync_asset_to_terminal_cursor, sync_inline_objects,
    sync_rgp_objects, sync_terminal_materials,
};

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
        // Embedded glTF scenes ride Bevy's `embedded://` memory source on
        // both targets; seed it before any Startup system can load one.
        crate::model::seed_embedded_scene_assets(app.world_mut());
        app.init_resource::<TerminalSelection>()
            .init_resource::<crate::model::CursorSettings>()
            .init_resource::<TerminalKeyBindings>()
            .init_resource::<StageTween>()
            .init_resource::<FocusedTerminal>()
            .add_message::<FocusRequest>()
            .add_message::<FocusGained>()
            .add_message::<FocusLost>()
            .add_message::<TerminalSpawnRequested>()
            .init_non_send::<TerminalClipboard>()
            .add_systems(Startup, setup_scene)
            // Boot lifecycle policy (#56 decision 8): startup focuses the
            // boot seat through the same request bus as every other writer,
            // and a Startup drain applies it before the first Update frame —
            // without it, keystrokes delivered during frame 1 would find no
            // focus (the Update drain runs after the keyboard) and drop.
            // The Update drain re-reads the boot request next frame and
            // no-ops (the target is already current).
            .add_systems(Startup, focus_boot_terminal.after(setup_scene))
            .add_systems(Startup, drain_focus_requests.after(focus_boot_terminal))
            // The user-spawn drain (decision 8): after the chord's emitter,
            // before the focus drain — the ordering edge auto-inserts a
            // command flush, so the child seat is live (and focusable) the
            // same frame its chord was pressed.
            // Also before the query channel: this system writes the wire
            // roster through `TerminalSpawnParams`, and the query channel
            // reads it. Without the edge, whether a same-frame
            // `state.terminals` observes a chord spawn is a schedule
            // tiebreak.
            .add_systems(
                Update,
                spawn_requested_terminals
                    .after(handle_keyboard_input)
                    .before(drain_focus_requests)
                    .before(crate::query_channel::answer_queries),
            )
            // The single focus writer (invariant 2), after every request
            // emitter in the tree and before every focus consumer: the
            // render set (blink, materials), the presentation applier
            // (plane visibility), and the cursor sync behind them.
            .add_systems(
                Update,
                drain_focus_requests
                    .after(handle_keyboard_input)
                    .after(handle_mouse_input)
                    .after(crate::ai::apply_ai_commands)
                    .before(apply_terminal_presentation)
                    .before(TerminalRedrawSet),
            )
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
                    .run_if(|objects: Query<&TerminalInlineObjects>| {
                        // Any seat with pending stage moves wakes the
                        // applier; it drains per seat.
                        objects.iter().any(|objects| objects.has_pending_stage())
                    }),
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
                         mobius_transition: Res<MobiusTransition>,
                         focus: Res<FocusedTerminal>| {
                            // Focus is a presentation input since M4.4:
                            // plane visibility is focused-1:1, so a focus
                            // move must re-run the applier (the drain only
                            // marks the resource changed on real
                            // transitions).
                            presentation.is_changed()
                                || plane_view.is_changed()
                                || mobius_transition.is_changed()
                                || focus.is_changed()
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
                apply_rgp_restyle.after(sync_inline_objects).run_if(
                    |focus: Res<FocusedTerminal>, objects: Query<&TerminalInlineObjects>| {
                        // Only the FOCUSED seat's queued restyles wake the
                        // applier — it only ever drains that seat, and an
                        // unfocused seat's restyles are subsumed by the
                        // resync its next focus flip triggers. Waking on any
                        // seat would busy-run the system every frame until
                        // that flip came.
                        focus
                            .get()
                            .and_then(|focused| objects.get(focused).ok())
                            .is_some_and(|objects| objects.has_restyle_objects())
                    },
                ),
            )
            .add_systems(
                Update,
                apply_instance_brightness
                    .after(sync_rgp_objects)
                    .after(apply_rgp_restyle),
            )
            .add_systems(
                Update,
                // Reads focus (the morphing seat's redraw): after the drain,
                // so the flip frame repaints the right texture.
                animate_mobius_transition
                    .after(drain_focus_requests)
                    .run_if(
                        |presentation: Res<TerminalPresentation>,
                         mobius_transition: Res<MobiusTransition>| {
                            presentation.mode == TerminalPresentationMode::Mobius3d
                                || mobius_transition.active
                        },
                    ),
            )
            .add_systems(
                Update,
                // Reads focus (only the staged seat's meshes rebuild, and a
                // flip forces one rebuild): after the drain, so the flip
                // frame rebuilds the newly staged pair.
                animate_terminal_plane_warp
                    .after(drain_focus_requests)
                    .run_if(|presentation: Res<TerminalPresentation>| {
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
            .add_plugins(crate::avatar::AvatarPlugin)
            .add_plugins(crate::presence::PresencePlugin)
            .add_plugins(crate::bookmarks::BookmarksPlugin)
            .add_plugins(crate::macros::MacrosPlugin)
            .add_plugins(crate::reactive::ReactivePlugin)
            .add_plugins(crate::viz::VizPlugin)
            .add_plugins(crate::effects::AiEffectsPlugin)
            .add_plugins(crate::sound::SoundPlugin)
            .add_plugins(crate::terminals::TerminalsPlugin)
            .add_plugins(DirectTerminalRenderPlugin)
            .add_plugins(TerminalPresentPlugin);
        // THE single despawn-sweep site (#56 decision 17), registered
        // after the organ plugins so every registry it sweeps exists. Its
        // pool release is the only one, paired with the spawner's only
        // allocation site — see the observer's doc for the free-is-last
        // invariant.
        app.add_observer(crate::scene::sweep_despawned_terminal);
    }
}
