//! Mobius view transition state and timing.

use bevy::prelude::*;

use super::{TerminalPlaneView, TerminalPresentationMode};

/// Animated transition into the Mobius-strip terminal view.
#[derive(Resource)]
pub struct MobiusTransition {
    /// Indicates the transition is active.
    pub active: bool,
    /// Elapsed transition time in seconds.
    pub elapsed_secs: f32,
    /// Current transition direction.
    pub direction: MobiusTransitionDirection,
    /// Source mode before entering the Mobius view.
    pub source_mode: TerminalPresentationMode,
    /// Source zoom before entering the Mobius view.
    pub source_zoom: f32,
    /// Source camera yaw before entering the Mobius view.
    pub source_yaw: f32,
    /// Source camera pitch before entering the Mobius view.
    pub source_pitch: f32,
    /// Source camera pan offset before entering the Mobius view.
    pub source_camera_offset: Vec2,
    /// Camera zoom at the start of the active transition.
    pub start_zoom: f32,
    /// Camera zoom at the end of the active transition.
    pub end_zoom: f32,
    /// Camera yaw at the start of the active transition.
    pub start_yaw: f32,
    /// Camera pitch at the start of the active transition.
    pub start_pitch: f32,
    /// Camera pan offset at the start of the active transition.
    pub start_camera_offset: Vec2,
    /// Camera yaw at the end of the active transition.
    pub end_yaw: f32,
    /// Camera pitch at the end of the active transition.
    pub end_pitch: f32,
    /// Camera pan offset at the end of the active transition.
    pub end_camera_offset: Vec2,
}

/// Direction of the Mobius transition.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MobiusTransitionDirection {
    /// Entering the Mobius view.
    Entering,
    /// Leaving the Mobius view.
    Exiting,
}

impl MobiusTransition {
    /// Zoom-out phase duration in seconds.
    pub const ZOOM_OUT_SECS: f32 = 0.2;
    /// View-reset phase duration in seconds while exiting.
    pub const VIEW_RESET_SECS: f32 = 0.2;
    /// Strip morph phase duration in seconds.
    pub const MORPH_SECS: f32 = 0.9;
    /// Final zoom multiplier applied when the transition completes.
    pub const TARGET_ZOOM_MULTIPLIER: f32 = 1.0;

    /// Starts the entry transition from a source mode and zoom level.
    pub fn begin_enter(
        &mut self,
        source_mode: TerminalPresentationMode,
        plane_view: &TerminalPlaneView,
    ) {
        self.active = true;
        self.elapsed_secs = 0.0;
        self.direction = MobiusTransitionDirection::Entering;
        self.source_mode = source_mode;
        self.source_zoom = plane_view.zoom;
        self.source_yaw = plane_view.yaw;
        self.source_pitch = plane_view.pitch;
        self.source_camera_offset = plane_view.camera_offset;
        self.start_zoom = plane_view.zoom;
        self.end_zoom = plane_view.zoom.max(Self::TARGET_ZOOM_MULTIPLIER);
        self.start_yaw = plane_view.yaw;
        self.start_pitch = plane_view.pitch;
        self.start_camera_offset = plane_view.camera_offset;
        self.end_yaw = plane_view.yaw;
        self.end_pitch = plane_view.pitch;
        self.end_camera_offset = plane_view.camera_offset;
    }

    /// Starts the exit transition back to the source mode.
    pub fn begin_exit(&mut self, plane_view: &TerminalPlaneView, current_zoom: f32) {
        self.active = true;
        self.elapsed_secs = 0.0;
        self.direction = MobiusTransitionDirection::Exiting;
        self.start_zoom = current_zoom;
        self.end_zoom = self.source_zoom.max(0.1);
        self.start_yaw = plane_view.yaw;
        self.start_pitch = plane_view.pitch;
        self.start_camera_offset = plane_view.camera_offset;
        self.end_yaw = self.source_yaw;
        self.end_pitch = self.source_pitch;
        self.end_camera_offset = self.source_camera_offset;
    }

    /// Stops the transition and resets its timer.
    pub fn stop(&mut self) {
        self.active = false;
        self.elapsed_secs = 0.0;
    }

    /// Returns the current zoom-out progress from `0.0` to `1.0` while entering.
    pub fn enter_zoom_progress(&self) -> f32 {
        (self.elapsed_secs / Self::ZOOM_OUT_SECS).clamp(0.0, 1.0)
    }

    /// Returns the current Mobius morph progress from `0.0` to `1.0` while entering.
    pub fn enter_morph_progress(&self) -> f32 {
        ((self.elapsed_secs - Self::ZOOM_OUT_SECS) / Self::MORPH_SECS).clamp(0.0, 1.0)
    }

    /// Returns the current Mobius morph progress for the active direction.
    pub fn morph_progress(&self) -> f32 {
        match self.direction {
            MobiusTransitionDirection::Entering => self.enter_morph_progress(),
            MobiusTransitionDirection::Exiting => {
                1.0 - ((self.elapsed_secs - Self::VIEW_RESET_SECS) / Self::MORPH_SECS)
                    .clamp(0.0, 1.0)
            }
        }
    }

    /// Returns the current animated camera zoom.
    pub fn current_zoom(&self) -> f32 {
        match self.direction {
            MobiusTransitionDirection::Entering => {
                let t = ease_in_out(self.enter_zoom_progress());
                self.start_zoom + (self.end_zoom - self.start_zoom) * t
            }
            MobiusTransitionDirection::Exiting => {
                let t = (self.elapsed_secs / Self::VIEW_RESET_SECS).clamp(0.0, 1.0);
                let t = ease_in_out(t);
                self.start_zoom + (self.end_zoom - self.start_zoom) * t
            }
        }
    }

    /// Returns the current animated camera yaw.
    pub fn current_yaw(&self) -> f32 {
        let t = match self.direction {
            MobiusTransitionDirection::Entering => 0.0,
            MobiusTransitionDirection::Exiting => {
                ease_in_out((self.elapsed_secs / Self::VIEW_RESET_SECS).clamp(0.0, 1.0))
            }
        };
        self.start_yaw + (self.end_yaw - self.start_yaw) * t
    }

    /// Returns the current animated camera pitch.
    pub fn current_pitch(&self) -> f32 {
        let t = match self.direction {
            MobiusTransitionDirection::Entering => 0.0,
            MobiusTransitionDirection::Exiting => {
                ease_in_out((self.elapsed_secs / Self::VIEW_RESET_SECS).clamp(0.0, 1.0))
            }
        };
        self.start_pitch + (self.end_pitch - self.start_pitch) * t
    }

    /// Returns the current animated camera pan offset.
    pub fn current_camera_offset(&self) -> Vec2 {
        let t = match self.direction {
            MobiusTransitionDirection::Entering => 0.0,
            MobiusTransitionDirection::Exiting => {
                ease_in_out((self.elapsed_secs / Self::VIEW_RESET_SECS).clamp(0.0, 1.0))
            }
        };
        self.start_camera_offset.lerp(self.end_camera_offset, t)
    }

    /// Returns whether the full transition has finished.
    pub fn finished(&self) -> bool {
        self.elapsed_secs
            >= match self.direction {
                MobiusTransitionDirection::Entering => Self::ZOOM_OUT_SECS + Self::MORPH_SECS,
                MobiusTransitionDirection::Exiting => Self::VIEW_RESET_SECS + Self::MORPH_SECS,
            }
    }
}

impl Default for MobiusTransition {
    fn default() -> Self {
        Self {
            active: false,
            elapsed_secs: 0.0,
            direction: MobiusTransitionDirection::Entering,
            source_mode: TerminalPresentationMode::Flat2d,
            source_zoom: 1.0,
            source_yaw: 0.0,
            source_pitch: 0.0,
            source_camera_offset: Vec2::ZERO,
            start_zoom: 0.0,
            end_zoom: 0.0,
            start_yaw: 0.0,
            start_pitch: 0.0,
            start_camera_offset: Vec2::ZERO,
            end_yaw: 0.0,
            end_pitch: 0.0,
            end_camera_offset: Vec2::ZERO,
        }
    }
}

fn ease_in_out(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}
