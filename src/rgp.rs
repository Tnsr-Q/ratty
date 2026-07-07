//! Ratty Graphics Protocol parsing.

use base64::Engine as _;

/// Ratty Graphics Protocol APC prefix.
pub const RGP_APC_START: &[u8] = b"\x1b_ratty;g;";
const ST: &[u8] = b"\x1b\\";
const C1_ST: u8 = 0x9c;

/// Placement style for an RGP object.
#[derive(Clone, Copy, Default)]
pub struct RgpPlacementStyle {
    /// Enables default animation.
    pub animate: bool,
    /// Scale multiplier.
    pub scale: f32,
    /// Extrusion depth.
    pub depth: f32,
    /// Optional object color.
    pub color: Option<[u8; 3]>,
    /// Brightness multiplier.
    pub brightness: f32,
    /// Translation offset relative to the anchor.
    pub offset: [f32; 3],
    /// Rotation in degrees.
    pub rotation: [f32; 3],
    /// Non-uniform scale.
    pub scale3: [f32; 3],
    /// Spin speed in radians per second; `None` uses the terminal's
    /// configured animation speed.
    pub spin: Option<f32>,
    /// Bob speed in radians per second; `None` uses the terminal's
    /// configured animation speed.
    pub bob: Option<f32>,
    /// Bob amplitude as a fraction of the cell height; `None` uses the
    /// terminal's configured amplitude.
    pub bob_amplitude: Option<f32>,
    /// Constant phase offset in radians applied to spin and bob.
    pub phase: f32,
}

/// Partial update for an RGP object placement.
#[derive(Clone, Copy, Default)]
pub struct RgpPlacementUpdate {
    /// Updates the default animation flag.
    pub animate: Option<bool>,
    /// Updates the uniform scale multiplier.
    pub scale: Option<f32>,
    /// Updates the extrusion depth.
    pub depth: Option<f32>,
    /// Updates the object color.
    pub color: Option<[u8; 3]>,
    /// Updates the brightness multiplier.
    pub brightness: Option<f32>,
    /// Updates the translation offset relative to the anchor.
    pub offset: [Option<f32>; 3],
    /// Updates the rotation in degrees.
    pub rotation: [Option<f32>; 3],
    /// Updates the non-uniform scale.
    pub scale3: [Option<f32>; 3],
    /// Updates the spin speed in radians per second.
    pub spin: Option<f32>,
    /// Updates the bob speed in radians per second.
    pub bob: Option<f32>,
    /// Updates the bob amplitude as a fraction of the cell height.
    pub bob_amplitude: Option<f32>,
    /// Updates the phase offset in radians.
    pub phase: Option<f32>,
}

/// Register source for an RGP object.
pub enum RgpRegisterSource {
    /// Path-based object registration.
    Path {
        /// Asset path known to Ratty.
        path: String,
    },
    /// Payload-based object registration.
    Payload {
        /// Optional payload name.
        name: Option<String>,
        /// Whether more chunks follow.
        more: bool,
        /// Decoded payload bytes.
        data: Vec<u8>,
    },
}

/// Registration-time object loading options.
#[derive(Clone, Copy)]
pub struct RgpRegisterOptions {
    /// Normalizes OBJ meshes into a centered unit-size coordinate space.
    pub normalize: bool,
}

impl Default for RgpRegisterOptions {
    fn default() -> Self {
        Self { normalize: true }
    }
}

/// Presentation mode requested by the `c` verb.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RgpStageMode {
    /// Flat 2D presentation.
    Flat2d,
    /// Warped 3D plane presentation.
    Plane3d,
    /// Möbius strip presentation.
    Mobius3d,
}

/// Easing curve for stage tweens.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RgpEase {
    /// Constant-rate interpolation.
    Linear,
    /// Accelerating interpolation.
    In,
    /// Decelerating interpolation.
    Out,
    /// Smoothstep interpolation.
    #[default]
    InOut,
}

impl RgpEase {
    /// Applies the curve to normalized progress `t` in `0..=1`.
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Linear => t,
            Self::In => t * t,
            Self::Out => 1.0 - (1.0 - t) * (1.0 - t),
            Self::InOut => t * t * (3.0 - 2.0 * t),
        }
    }
}

/// Stage/camera update carried by the `c` verb.
///
/// Every field is optional and absolute; absent keys leave the terminal's
/// current presentation untouched.
#[derive(Clone, Copy, Default)]
pub struct RgpStageUpdate {
    /// Requested presentation mode.
    pub mode: Option<RgpStageMode>,
    /// Plane warp amount, clamped to `0.0..=1.0` at apply time.
    pub warp: Option<f32>,
    /// Camera yaw in radians.
    pub yaw: Option<f32>,
    /// Camera pitch in radians.
    pub pitch: Option<f32>,
    /// Camera zoom, clamped to `0.1..=4.0` at apply time.
    pub zoom: Option<f32>,
    /// Tween duration in seconds; absent or non-positive applies instantly.
    /// Never applies to `mode` changes, which are always instant.
    pub dur: Option<f32>,
    /// Easing curve for the tween; defaults to [`RgpEase::InOut`].
    pub ease: Option<RgpEase>,
}

/// Consumes an RGP APC sequence.
pub fn consume_sequence(sequence: &[u8]) -> Option<RgpOperation> {
    if !sequence.starts_with(RGP_APC_START) {
        return None;
    }

    let content_end = if sequence.ends_with(&[C1_ST]) {
        sequence.len() - 1
    } else if sequence.ends_with(ST) {
        sequence.len() - 2
    } else {
        return None;
    };
    let content = std::str::from_utf8(&sequence[RGP_APC_START.len()..content_end]).ok()?;
    let mut parts = content.split(';');
    let verb = parts.next()?;
    let mut id = None;
    let mut format = None;
    let mut path = None;
    let mut source = None;
    let mut more = None;
    let mut name = None;
    let mut row = None;
    let mut col = None;
    let mut width = None;
    let mut height = None;
    let mut animate = None;
    let mut scale = None;
    let mut depth = None;
    let mut color = None;
    let mut brightness = None;
    let mut px = None;
    let mut py = None;
    let mut pz = None;
    let mut rx = None;
    let mut ry = None;
    let mut rz = None;
    let mut sx = None;
    let mut sy = None;
    let mut sz = None;
    let mut normalize = None;
    let mut spin = None;
    let mut bob = None;
    let mut bob_amplitude = None;
    let mut phase = None;
    let mut mode = None;
    let mut warp = None;
    let mut yaw = None;
    let mut pitch = None;
    let mut zoom = None;
    let mut dur = None;
    let mut ease = None;
    let mut payload = None;
    for part in parts.filter(|part| !part.is_empty()) {
        let Some((key, value)) = part.split_once('=') else {
            if verb == "r" && source.as_deref() == Some("payload") {
                payload = Some(part.to_string());
                break;
            }
            continue;
        };
        match key {
            "id" => id = value.parse().ok(),
            "fmt" => format = Some(value.to_string()),
            "path" => path = Some(value.to_string()),
            "source" => source = Some(value.to_string()),
            "more" => more = parse_bool(value),
            "name" => name = Some(value.to_string()),
            "row" => row = value.parse().ok(),
            "col" => col = value.parse().ok(),
            "w" => width = value.parse().ok(),
            "h" => height = value.parse().ok(),
            "animate" => animate = parse_bool(value),
            "scale" => scale = value.parse().ok(),
            "depth" => depth = value.parse().ok(),
            "color" | "tint" => color = parse_color(value),
            "brightness" => brightness = value.parse().ok(),
            "px" => px = value.parse().ok(),
            "py" => py = value.parse().ok(),
            "pz" => pz = value.parse().ok(),
            "rx" => rx = value.parse().ok(),
            "ry" => ry = value.parse().ok(),
            "rz" => rz = value.parse().ok(),
            "sx" => sx = value.parse().ok(),
            "sy" => sy = value.parse().ok(),
            "sz" => sz = value.parse().ok(),
            "normalize" => normalize = parse_bool(value),
            "spin" => spin = parse_finite(value),
            "bob" => bob = parse_finite(value),
            "bobamp" => bob_amplitude = parse_finite(value),
            "phase" => phase = parse_finite(value),
            "mode" => mode = parse_stage_mode(value),
            "warp" => warp = parse_finite(value),
            "yaw" => yaw = parse_finite(value),
            "pitch" => pitch = parse_finite(value),
            "zoom" => zoom = parse_finite(value),
            "dur" => dur = parse_finite(value),
            "ease" => ease = parse_ease(value),
            _ if verb == "r" && source.as_deref() == Some("payload") => {
                payload = Some(part.to_string());
                break;
            }
            _ => {}
        }
    }

    match verb {
        "s" => Some(RgpOperation::SupportQuery),
        "r" => Some(RgpOperation::Register {
            object_id: id?,
            format: format?,
            options: RgpRegisterOptions {
                normalize: normalize.unwrap_or(true),
            },
            source: if let Some(path) = path {
                RgpRegisterSource::Path { path }
            } else {
                if source.as_deref() != Some("payload") {
                    return None;
                }
                let data = base64::engine::general_purpose::STANDARD
                    .decode(payload.unwrap_or_default())
                    .ok()?;
                RgpRegisterSource::Payload {
                    name,
                    more: more.unwrap_or(false),
                    data,
                }
            },
        }),
        "p" => Some(RgpOperation::Place {
            object_id: id?,
            anchor: RgpAnchor {
                row: row?,
                col: col?,
                columns: width?,
                rows: height?,
                style: RgpPlacementStyle {
                    animate: animate.unwrap_or(false),
                    scale: scale.unwrap_or(1.0),
                    depth: depth.unwrap_or(0.0),
                    color,
                    brightness: brightness.unwrap_or(1.0),
                    offset: [px.unwrap_or(0.0), py.unwrap_or(0.0), pz.unwrap_or(0.0)],
                    rotation: [rx.unwrap_or(0.0), ry.unwrap_or(0.0), rz.unwrap_or(0.0)],
                    scale3: [sx.unwrap_or(1.0), sy.unwrap_or(1.0), sz.unwrap_or(1.0)],
                    spin,
                    bob,
                    bob_amplitude,
                    phase: phase.unwrap_or(0.0),
                },
            },
        }),
        "u" => Some(RgpOperation::Update {
            object_id: id?,
            update: RgpPlacementUpdate {
                animate,
                scale,
                depth,
                color,
                brightness,
                offset: [px, py, pz],
                rotation: [rx, ry, rz],
                scale3: [sx, sy, sz],
                spin,
                bob,
                bob_amplitude,
                phase,
            },
        }),
        "d" => Some(RgpOperation::Delete { object_id: id }),
        "c" => Some(RgpOperation::Stage {
            update: RgpStageUpdate {
                mode,
                warp,
                yaw,
                pitch,
                zoom,
                dur,
                ease,
            },
        }),
        _ => Some(RgpOperation::Ignored),
    }
}

/// RGP anchor placement.
#[derive(Clone, Copy)]
pub struct RgpAnchor {
    /// Anchor row.
    pub row: u16,
    /// Anchor column.
    pub col: u16,
    /// Object width in cells.
    pub columns: u32,
    /// Object height in cells.
    pub rows: u32,
    /// Placement style.
    pub style: RgpPlacementStyle,
}

/// Parsed RGP operation.
pub enum RgpOperation {
    /// Support query.
    SupportQuery,
    /// Object registration.
    Register {
        /// Object identifier.
        object_id: u32,
        /// Declared object format.
        format: String,
        /// Registration-time object loading options.
        options: RgpRegisterOptions,
        /// Register source.
        source: RgpRegisterSource,
    },
    /// Object placement.
    Place {
        /// Object identifier.
        object_id: u32,
        /// Placement anchor.
        anchor: RgpAnchor,
    },
    /// Object update.
    Update {
        /// Object identifier.
        object_id: u32,
        /// Partial style/transform update.
        update: RgpPlacementUpdate,
    },
    /// Object deletion.
    Delete {
        /// Optional object identifier.
        object_id: Option<u32>,
    },
    /// Stage/camera update.
    Stage {
        /// Partial stage update.
        update: RgpStageUpdate,
    },
    /// Ignored operation.
    Ignored,
}

/// Returns the RGP support reply sequence.
///
/// New capability keys are appended so v1 reply parsers that scan known
/// keys keep working: `stage` (the `c` verb) and `tween` (`dur`/`ease`
/// on `c`).
pub fn support_reply() -> Vec<u8> {
    b"\x1b_ratty;g;s;v=2;fmt=obj|glb|stl;path=1;payload=1;chunk=1;anim=1;depth=1;color=1;brightness=1;transform=1;update=1;normalize=1;stage=1;tween=1\x1b\\".to_vec()
}

fn parse_color(value: &str) -> Option<[u8; 3]> {
    let value = value.strip_prefix('#').unwrap_or(value);
    if value.len() != 6 {
        return None;
    }

    Some([
        u8::from_str_radix(&value[0..2], 16).ok()?,
        u8::from_str_radix(&value[2..4], 16).ok()?,
        u8::from_str_radix(&value[4..6], 16).ok()?,
    ])
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

fn parse_finite(value: &str) -> Option<f32> {
    value.parse().ok().filter(|parsed: &f32| parsed.is_finite())
}

fn parse_stage_mode(value: &str) -> Option<RgpStageMode> {
    match value {
        "flat2d" => Some(RgpStageMode::Flat2d),
        "plane3d" => Some(RgpStageMode::Plane3d),
        "mobius3d" => Some(RgpStageMode::Mobius3d),
        _ => None,
    }
}

fn parse_ease(value: &str) -> Option<RgpEase> {
    match value {
        "linear" => Some(RgpEase::Linear),
        "in" => Some(RgpEase::In),
        "out" => Some(RgpEase::Out),
        "inout" => Some(RgpEase::InOut),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> Option<RgpOperation> {
        let sequence = format!("\x1b_ratty;g;{content}\x1b\\");
        consume_sequence(sequence.as_bytes())
    }

    fn stage(content: &str) -> RgpStageUpdate {
        match parse(content) {
            Some(RgpOperation::Stage { update }) => update,
            _ => panic!("`{content}` did not parse to a stage operation"),
        }
    }

    #[test]
    fn stage_verb_parses_every_field() {
        let update = stage("c;mode=plane3d;warp=0.4;yaw=0.18;pitch=-0.2;zoom=1.5;dur=2.5;ease=out");
        assert_eq!(update.mode, Some(RgpStageMode::Plane3d));
        assert_eq!(update.warp, Some(0.4));
        assert_eq!(update.yaw, Some(0.18));
        assert_eq!(update.pitch, Some(-0.2));
        assert_eq!(update.zoom, Some(1.5));
        assert_eq!(update.dur, Some(2.5));
        assert_eq!(update.ease, Some(RgpEase::Out));
    }

    #[test]
    fn stage_verb_parses_partial_fields() {
        let update = stage("c;warp=0.5");
        assert_eq!(update.warp, Some(0.5));
        assert!(update.mode.is_none());
        assert!(update.yaw.is_none());
        assert!(update.pitch.is_none());
        assert!(update.zoom.is_none());
        assert!(update.dur.is_none());
        assert!(update.ease.is_none());
    }

    #[test]
    fn bare_stage_verb_is_a_legal_no_op() {
        let update = stage("c");
        assert!(update.mode.is_none());
        assert!(update.warp.is_none());
        assert!(update.yaw.is_none());
        assert!(update.pitch.is_none());
        assert!(update.zoom.is_none());
        assert!(update.dur.is_none());
        assert!(update.ease.is_none());
    }

    #[test]
    fn stage_verb_drops_malformed_values() {
        let update = stage("c;mode=weird;warp=abc;zoom=NaN;dur=inf;ease=bounce;pitch=0.3");
        assert!(update.mode.is_none());
        assert!(update.warp.is_none());
        assert!(update.zoom.is_none());
        assert!(update.dur.is_none());
        assert!(update.ease.is_none());
        assert_eq!(update.pitch, Some(0.3));
    }

    #[test]
    fn unknown_verbs_stay_ignored() {
        assert!(matches!(parse("q;id=1"), Some(RgpOperation::Ignored)));
    }

    #[test]
    fn ease_curves_hit_endpoints_and_stay_monotonic() {
        for ease in [RgpEase::Linear, RgpEase::In, RgpEase::Out, RgpEase::InOut] {
            assert_eq!(ease.apply(0.0), 0.0);
            assert_eq!(ease.apply(1.0), 1.0);
            let mut previous = 0.0;
            for step in 1..=100 {
                let value = ease.apply(step as f32 / 100.0);
                assert!(value >= previous, "{ease:?} decreased at step {step}");
                previous = value;
            }
        }
    }

    #[test]
    fn place_parses_animation_keys() {
        let Some(RgpOperation::Place { anchor, .. }) =
            parse("p;id=1;row=4;col=6;w=8;h=4;animate=1;spin=0.6;bob=1.2;bobamp=0.05;phase=1.57")
        else {
            panic!("place sequence did not parse");
        };
        assert_eq!(anchor.style.spin, Some(0.6));
        assert_eq!(anchor.style.bob, Some(1.2));
        assert_eq!(anchor.style.bob_amplitude, Some(0.05));
        assert_eq!(anchor.style.phase, 1.57);
    }

    #[test]
    fn place_without_animation_keys_keeps_v1_defaults() {
        let Some(RgpOperation::Place { anchor, .. }) = parse("p;id=1;row=4;col=6;w=8;h=4") else {
            panic!("place sequence did not parse");
        };
        let style = anchor.style;
        assert!(style.spin.is_none());
        assert!(style.bob.is_none());
        assert!(style.bob_amplitude.is_none());
        assert_eq!(style.phase, 0.0);
        // The pre-v2 defaults must be untouched.
        assert!(!style.animate);
        assert_eq!(style.scale, 1.0);
        assert_eq!(style.depth, 0.0);
        assert!(style.color.is_none());
        assert_eq!(style.brightness, 1.0);
        assert_eq!(style.offset, [0.0, 0.0, 0.0]);
        assert_eq!(style.rotation, [0.0, 0.0, 0.0]);
        assert_eq!(style.scale3, [1.0, 1.0, 1.0]);
    }

    #[test]
    fn update_parses_partial_animation_keys() {
        let Some(RgpOperation::Update { update, .. }) = parse("u;id=1;spin=2.0;phase=0.5") else {
            panic!("update sequence did not parse");
        };
        assert_eq!(update.spin, Some(2.0));
        assert_eq!(update.phase, Some(0.5));
        assert!(update.bob.is_none());
        assert!(update.bob_amplitude.is_none());
        assert!(update.animate.is_none());
        assert!(update.scale.is_none());
    }

    #[test]
    fn support_reply_is_a_single_parseable_line() {
        let reply = support_reply();
        assert!(!reply.contains(&b'\n'));
        let text = std::str::from_utf8(&reply).expect("reply is UTF-8");
        let content = text
            .strip_prefix("\x1b_ratty;g;")
            .and_then(|rest| rest.strip_suffix("\x1b\\"))
            .expect("reply framed as an RGP APC sequence");
        let mut parts = content.split(';');
        assert_eq!(parts.next(), Some("s"));
        for part in parts {
            assert!(part.contains('='), "`{part}` is not a key=value pair");
        }
        for capability in ["v=2", "stage=1", "tween=1"] {
            assert!(
                content.split(';').any(|part| part == capability),
                "reply must advertise `{capability}`"
            );
        }
    }
}
