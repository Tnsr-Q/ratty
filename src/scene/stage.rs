//! Engine-side interpolation for RGP `c;dur=` stage moves.

use bevy::prelude::*;

use crate::rgp::RgpEase;

/// One tweened scalar channel of a stage move.
#[derive(Clone, Copy)]
pub struct StageChannel {
    /// Value when the tween started.
    pub start: f32,
    /// Target value.
    pub end: f32,
}

impl StageChannel {
    /// Samples the channel at eased progress `t` in `0.0..=1.0`.
    pub fn sample(self, t: f32) -> f32 {
        self.start + (self.end - self.start) * t
    }
}

/// Engine-side tween state for `c;dur=` stage moves.
///
/// One tween runs at a time; a new `c` replaces it entirely, retargeting
/// from the current live values. Only requested fields carry a channel —
/// absent fields stay wherever they are.
#[derive(Resource, Default)]
pub struct StageTween {
    /// Indicates the tween is running.
    pub active: bool,
    /// Elapsed seconds.
    pub elapsed_secs: f32,
    /// Total duration in seconds; always positive while active.
    pub duration_secs: f32,
    /// Easing curve shared by all channels.
    pub ease: RgpEase,
    /// Plane warp channel.
    pub warp: Option<StageChannel>,
    /// Camera yaw channel.
    pub yaw: Option<StageChannel>,
    /// Camera pitch channel.
    pub pitch: Option<StageChannel>,
    /// Camera zoom channel.
    pub zoom: Option<StageChannel>,
}

impl StageTween {
    /// Stops the tween, freezing values where they currently are.
    pub fn stop(&mut self) {
        self.active = false;
        self.elapsed_secs = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_samples_endpoints_exactly() {
        let channel = StageChannel {
            start: 0.2,
            end: 0.8,
        };
        assert_eq!(channel.sample(0.0), 0.2);
        assert_eq!(channel.sample(1.0), 0.8);
        assert_eq!(channel.sample(0.5), 0.5);
    }

    #[test]
    fn stop_freezes_and_resets_the_timer() {
        let mut tween = StageTween {
            active: true,
            elapsed_secs: 0.7,
            duration_secs: 2.0,
            ease: RgpEase::Linear,
            warp: Some(StageChannel {
                start: 0.0,
                end: 1.0,
            }),
            ..Default::default()
        };
        tween.stop();
        assert!(!tween.active);
        assert_eq!(tween.elapsed_secs, 0.0);
        // Channels stay in place so a retarget can read them if needed.
        assert!(tween.warp.is_some());
    }
}
