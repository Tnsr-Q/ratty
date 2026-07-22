//! The soul: ephemeral screen-space effects and AI presence.
//!
//! `flash` / `pulse` / `tint` are the terminal's emotional punctuation;
//! `think` / `confidence` / `mood` are its inner weather. All of them
//! resolve, every frame, into a single translucent color washed over the
//! whole surface by a dedicated overlay camera — so the terminal visibly
//! *feels* something while an agent works, in flat and warped modes alike.
//!
//! The effects are driven by the same OSC 777 [`AiCommand`] messages the
//! stage handlers read (each Bevy message reader has its own cursor, so
//! this reads them independently of `apply_ai_commands`).

use bevy::camera::visibility::RenderLayers;
use bevy::ecs::message::MessageReader;
use bevy::prelude::*;

use crate::ai::AiCommand;
use crate::osc::RattyAiCommand;
use crate::terminal::TerminalRedrawState;

/// Render layer the effect overlay lives on, isolated from the main scene.
const EFFECT_LAYER: usize = 1;
/// The overlay sprite is oversized and centered so it covers any window.
const OVERLAY_SIZE: f32 = 8000.0;
/// Ceiling on the composite alpha so the terminal is never fully obscured.
const MAX_ALPHA: f32 = 0.82;

/// Marks the effect overlay camera so the presentation pass leaves it alone.
#[derive(Component)]
pub struct AiEffectCamera;

/// Marks the fullscreen sprite the composite color is written to.
#[derive(Component)]
struct AiEffectSprite;

/// The AI's declared mood, setting the ambient color and breathing rhythm.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mood {
    /// Warm and bright, quick breath.
    Excited,
    /// Cool and dim, slow breath.
    Cautious,
    /// Unstable, shifting hue.
    Confused,
    /// Near-neutral, minimal.
    Focused,
    /// Bright gold.
    Celebratory,
}

impl Mood {
    /// Parses a mood tag; unknown tags map to `None`.
    pub fn parse(mood: &str) -> Option<Self> {
        match mood {
            "excited" => Some(Self::Excited),
            "cautious" => Some(Self::Cautious),
            "confused" => Some(Self::Confused),
            "focused" => Some(Self::Focused),
            "celebratory" => Some(Self::Celebratory),
            _ => None,
        }
    }

    /// The wire tag this mood parses from (the inverse of [`Mood::parse`]).
    pub fn tag(self) -> &'static str {
        match self {
            Self::Excited => "excited",
            Self::Cautious => "cautious",
            Self::Confused => "confused",
            Self::Focused => "focused",
            Self::Celebratory => "celebratory",
        }
    }

    /// Ambient color at time `clock` (Confused shimmers; the rest are steady).
    fn color(self, clock: f32) -> [f32; 3] {
        match self {
            Self::Excited => [1.0, 0.80, 0.40],
            Self::Cautious => [0.35, 0.42, 0.55],
            Self::Confused => [
                (0.69 + 0.16 * (clock * 3.3).sin()).clamp(0.0, 1.0),
                (0.43 + 0.13 * (clock * 5.7).sin()).clamp(0.0, 1.0),
                (0.66 + 0.11 * (clock * 2.1).sin()).clamp(0.0, 1.0),
            ],
            Self::Focused => [0.86, 0.84, 0.72],
            Self::Celebratory => [0.90, 0.75, 0.34],
        }
    }

    /// Breaths per unit time; drives the thinking glow.
    fn breath_rate(self) -> f32 {
        match self {
            Self::Excited => 2.2,
            Self::Cautious => 0.8,
            Self::Confused => 1.7,
            Self::Focused => 1.1,
            Self::Celebratory => 2.6,
        }
    }
}

/// A decaying color flash.
#[derive(Clone, Copy)]
struct Flash {
    color: [f32; 3],
    remaining: f32,
    total: f32,
}

/// A decaying rhythmic pulse.
#[derive(Clone, Copy)]
struct Pulse {
    intensity: f32,
    remaining: f32,
    total: f32,
}

/// The live emotional state of the terminal.
#[derive(Resource, Default)]
pub struct AiEffects {
    clock: f32,
    flash: Option<Flash>,
    pulse: Option<Pulse>,
    tint: Option<([f32; 3], f32)>,
    thinking: bool,
    confidence: Option<f32>,
    mood: Option<Mood>,
}

/// The publicly observable slice of [`AiEffects`]: what any agent can see
/// on screen, projected for the query channel's `state.scene` op. Internal
/// timers and clock state stay private.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AiEffectsPublic {
    /// Whether the thinking indicator is on.
    pub thinking: bool,
    /// The confidence aura level, when set.
    pub confidence: Option<f32>,
    /// The active mood tag, when set.
    pub mood: Option<&'static str>,
    /// Whether a flash is currently decaying.
    pub flash: bool,
    /// Whether a pulse is currently decaying.
    pub pulse: bool,
    /// Whether a steady tint is applied.
    pub tint: bool,
}

impl AiEffects {
    /// The publicly visible effect state (everything here is already
    /// observable on screen; timers and internals are not exposed).
    pub fn public_state(&self) -> AiEffectsPublic {
        AiEffectsPublic {
            thinking: self.thinking,
            confidence: self.confidence,
            mood: self.mood.map(Mood::tag),
            flash: self.flash.is_some(),
            pulse: self.pulse.is_some(),
            tint: self.tint.is_some(),
        }
    }

    fn set_flash(&mut self, color: [f32; 3], duration: f32) {
        let total = duration.max(0.05);
        self.flash = Some(Flash {
            color,
            remaining: total,
            total,
        });
    }

    fn set_pulse(&mut self, intensity: f32, duration: f32) {
        let total = duration.max(0.05);
        self.pulse = Some(Pulse {
            intensity: intensity.clamp(0.0, 1.0),
            remaining: total,
            total,
        });
    }

    fn set_tint(&mut self, color: [f32; 3], opacity: f32) {
        let opacity = opacity.clamp(0.0, 1.0);
        self.tint = if opacity <= f32::EPSILON {
            None
        } else {
            Some((color, opacity))
        };
    }

    /// Clears every effect (the `reset` command).
    fn clear(&mut self) {
        let clock = self.clock;
        *self = Self::default();
        self.clock = clock;
    }

    /// Advances timers by `dt`, dropping finished flash/pulse effects.
    fn advance(&mut self, dt: f32) {
        self.clock += dt;
        if let Some(flash) = &mut self.flash {
            flash.remaining -= dt;
            if flash.remaining <= 0.0 {
                self.flash = None;
            }
        }
        if let Some(pulse) = &mut self.pulse {
            pulse.remaining -= dt;
            if pulse.remaining <= 0.0 {
                self.pulse = None;
            }
        }
    }

    /// Whether a time-varying effect needs a redraw every frame. Steady
    /// effects (tint, confidence, mood) get their single redraw when set and
    /// then persist without churn.
    fn animating(&self) -> bool {
        self.thinking
            || self.flash.is_some()
            || self.pulse.is_some()
            || self.mood == Some(Mood::Confused)
    }

    /// Resolves every active effect into one overlay color and alpha.
    ///
    /// Contributions are alpha-weighted and averaged, so the wash reads as a
    /// blend of the active feelings rather than any single one dominating.
    fn overlay(&self) -> (Color, f32) {
        let mut accum = [0.0_f32; 3];
        let mut alpha = 0.0_f32;
        let mut add = |color: [f32; 3], weight: f32| {
            for channel in 0..3 {
                accum[channel] += color[channel] * weight;
            }
            alpha += weight;
        };

        if let Some(mood) = self.mood {
            add(mood.color(self.clock), 0.05);
        }
        if let Some(level) = self.confidence {
            add(confidence_color(level), 0.08);
        }
        if let Some((color, opacity)) = self.tint {
            add(color, opacity);
        }
        if self.thinking {
            let rate = self.mood.map(Mood::breath_rate).unwrap_or(1.3);
            let breath = 0.5 + 0.5 * (self.clock * rate).sin();
            let color = self
                .mood
                .map(|m| m.color(self.clock))
                .unwrap_or([0.86, 0.84, 0.72]);
            add(color, breath * 0.07);
        }
        if let Some(pulse) = self.pulse {
            let envelope = (pulse.remaining / pulse.total).clamp(0.0, 1.0);
            let oscillation = 0.5 + 0.5 * (self.clock * 6.0).sin();
            add(
                [1.0, 1.0, 1.0],
                pulse.intensity * envelope * oscillation * 0.22,
            );
        }
        if let Some(flash) = self.flash {
            let envelope = (flash.remaining / flash.total).clamp(0.0, 1.0);
            add(flash.color, envelope * 0.6);
        }

        let rgb = if alpha > 1e-4 {
            [accum[0] / alpha, accum[1] / alpha, accum[2] / alpha]
        } else {
            [0.0, 0.0, 0.0]
        };
        let alpha = alpha.clamp(0.0, MAX_ALPHA);
        (Color::srgba(rgb[0], rgb[1], rgb[2], alpha), alpha)
    }
}

/// Maps a confidence level to an aura color: red (low) → gold → moss (high).
fn confidence_color(level: f32) -> [f32; 3] {
    let level = level.clamp(0.0, 1.0);
    let red = [0.77, 0.45, 0.43];
    let gold = [0.90, 0.75, 0.34];
    let moss = [0.54, 0.60, 0.48];
    let lerp = |a: [f32; 3], b: [f32; 3], t: f32| {
        [
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
        ]
    };
    if level < 0.5 {
        lerp(red, gold, level / 0.5)
    } else {
        lerp(gold, moss, (level - 0.5) / 0.5)
    }
}

/// Parses a `#rrggbb` color to linear-ish f32 channels; defaults to white.
fn parse_color(value: &str) -> [f32; 3] {
    let hex = value.strip_prefix('#').unwrap_or(value);
    if hex.len() == 6
        && let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&hex[0..2], 16),
            u8::from_str_radix(&hex[2..4], 16),
            u8::from_str_radix(&hex[4..6], 16),
        )
    {
        return [
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
        ];
    }
    [1.0, 1.0, 1.0]
}

/// Registers the AI effects overlay and its systems.
pub struct AiEffectsPlugin;

impl Plugin for AiEffectsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AiEffects>()
            .add_systems(Startup, setup_ai_effects)
            .add_systems(
                Update,
                (apply_ai_effect_commands, animate_ai_effects).chain(),
            );
    }
}

/// Spawns the overlay camera and the fullscreen effect sprite.
fn setup_ai_effects(mut commands: Commands) {
    commands.spawn((
        AiEffectCamera,
        Camera2d,
        Camera {
            // Draw last, on top of both the 2D and 3D presentation cameras,
            // and never clear so it composites over the final image.
            order: 10,
            clear_color: ClearColorConfig::None,
            ..default()
        },
        RenderLayers::layer(EFFECT_LAYER),
        Msaa::Off,
    ));
    commands.spawn((
        AiEffectSprite,
        Sprite {
            color: Color::srgba(0.0, 0.0, 0.0, 0.0),
            custom_size: Some(Vec2::splat(OVERLAY_SIZE)),
            ..default()
        },
        Transform::default(),
        RenderLayers::layer(EFFECT_LAYER),
    ));
}

/// Applies `flash`/`pulse`/`tint`/`think`/`confidence`/`mood`/`reset`
/// commands to the effect state.
///
/// This system owns the ack for the six effect commands (they always
/// commit). `reset` is acked by `apply_ai_commands`, which owns that
/// command's single ack.
pub(crate) fn apply_ai_effect_commands(
    mut commands: MessageReader<AiCommand>,
    mut effects: ResMut<AiEffects>,
    mut acks: MessageWriter<crate::query_channel::AckOutcome>,
    mut redraw: ResMut<TerminalRedrawState>,
) {
    let mut changed = false;
    for AiCommand {
        source,
        ack_token,
        command,
    } in commands.read()
    {
        match command {
            RattyAiCommand::Flash { color, duration } => {
                effects.set_flash(parse_color(color), *duration);
                changed = true;
            }
            RattyAiCommand::Pulse {
                intensity,
                duration,
            } => {
                effects.set_pulse(*intensity, *duration);
                changed = true;
            }
            RattyAiCommand::Tint { color, opacity } => {
                effects.set_tint(parse_color(color), *opacity);
                changed = true;
            }
            RattyAiCommand::Think { state } => {
                effects.thinking = match state.as_str() {
                    "start" => true,
                    "end" => false,
                    _ => !effects.thinking,
                };
                changed = true;
            }
            RattyAiCommand::Confidence { level } => {
                effects.confidence = Some(level.clamp(0.0, 1.0));
                changed = true;
            }
            RattyAiCommand::Mood { mood } => {
                effects.mood = Mood::parse(mood);
                changed = true;
            }
            RattyAiCommand::Reset => {
                effects.clear();
                changed = true;
                continue;
            }
            _ => continue,
        }
        crate::query_channel::ack_commit(&mut acks, *source, ack_token);
    }
    if changed {
        redraw.request();
    }
}

/// Advances the effect clock and writes the composite color to the sprite,
/// keeping the frame alive while a time-varying effect is running.
fn animate_ai_effects(
    time: Res<Time>,
    mut effects: ResMut<AiEffects>,
    mut sprite: Query<&mut Sprite, With<AiEffectSprite>>,
    mut redraw: ResMut<TerminalRedrawState>,
) {
    effects.advance(time.delta_secs());
    let (color, _) = effects.overlay();
    if let Ok(mut sprite) = sprite.single_mut() {
        // Only touch the component when the value actually moves, so an idle
        // terminal does not churn change detection.
        if sprite.color != color {
            sprite.color = color;
        }
    }
    // Keep the frame alive only while something is actually moving; the
    // last frame of a decaying effect was already requested the prior frame,
    // so the fade-out's final transparent frame still renders.
    if effects.animating() {
        redraw.request();
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn alpha(effects: &AiEffects) -> f32 {
        effects.overlay().1
    }

    #[test]
    fn idle_is_fully_transparent() {
        let effects = AiEffects::default();
        assert_eq!(alpha(&effects), 0.0);
        assert!(!effects.animating());
    }

    #[test]
    fn flash_decays_to_nothing() {
        let mut effects = AiEffects::default();
        effects.set_flash([1.0, 0.0, 0.0], 1.0);
        let start = alpha(&effects);
        assert!(start > 0.0);
        effects.advance(0.5);
        let mid = alpha(&effects);
        assert!(mid < start && mid > 0.0);
        effects.advance(0.6); // past the end
        assert!(effects.flash.is_none());
        assert_eq!(alpha(&effects), 0.0);
    }

    #[test]
    fn tint_opacity_zero_clears() {
        let mut effects = AiEffects::default();
        effects.set_tint([0.0, 1.0, 0.0], 0.3);
        assert!(effects.tint.is_some());
        effects.set_tint([0.0, 1.0, 0.0], 0.0);
        assert!(effects.tint.is_none());
    }

    #[test]
    fn confidence_color_runs_red_to_moss() {
        assert_eq!(confidence_color(0.0), [0.77, 0.45, 0.43]);
        assert_eq!(confidence_color(0.5), [0.90, 0.75, 0.34]);
        assert_eq!(confidence_color(1.0), [0.54, 0.60, 0.48]);
        // Monotonic green channel from anxious red toward calm moss.
        assert!(confidence_color(0.9)[1] > confidence_color(0.1)[1]);
    }

    #[test]
    fn thinking_breathes_but_stays_subtle() {
        let mut effects = AiEffects::default();
        effects.thinking = true;
        let mut seen_low = false;
        let mut seen_high = false;
        for _ in 0..200 {
            effects.advance(0.02);
            let a = alpha(&effects);
            assert!(a <= MAX_ALPHA);
            if a < 0.01 {
                seen_low = true;
            }
            if a > 0.03 {
                seen_high = true;
            }
        }
        assert!(seen_low && seen_high, "breathing should oscillate");
    }

    #[test]
    fn composite_never_exceeds_the_ceiling() {
        let mut effects = AiEffects::default();
        effects.set_flash([1.0, 1.0, 1.0], 5.0);
        effects.set_pulse(1.0, 5.0);
        effects.set_tint([1.0, 1.0, 1.0], 1.0);
        effects.thinking = true;
        effects.confidence = Some(0.0);
        effects.mood = Some(Mood::Excited);
        assert!(alpha(&effects) <= MAX_ALPHA + 1e-6);
    }

    #[test]
    fn reset_clears_everything_but_keeps_the_clock() {
        let mut effects = AiEffects::default();
        effects.advance(3.0);
        effects.set_tint([1.0, 0.0, 0.0], 0.5);
        effects.thinking = true;
        effects.mood = Some(Mood::Confused);
        effects.clear();
        assert_eq!(alpha(&effects), 0.0);
        assert!(effects.tint.is_none() && effects.mood.is_none() && !effects.thinking);
        assert_eq!(effects.clock, 3.0);
    }

    #[test]
    fn mood_parse_round_trips_known_tags() {
        assert_eq!(Mood::parse("excited"), Some(Mood::Excited));
        assert_eq!(Mood::parse("celebratory"), Some(Mood::Celebratory));
        assert_eq!(Mood::parse("stoic"), None);
    }

    #[test]
    fn parse_color_reads_hex_and_defaults_white() {
        assert_eq!(parse_color("#ff0000"), [1.0, 0.0, 0.0]);
        assert_eq!(parse_color("00ff00"), [0.0, 1.0, 0.0]);
        assert_eq!(parse_color("nonsense"), [1.0, 1.0, 1.0]);
    }
}
