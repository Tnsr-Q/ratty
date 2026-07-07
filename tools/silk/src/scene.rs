//! Scene DSL: the agent-facing source format compiled into `.silk` casts.
//!
//! A scene is JSON with `meta`, `stage`, and a `cast` of timed steps. Each
//! step carries `at` (absolute seconds) plus exactly one verb field:
//! `print`, `register`, `place`, `update`, `tween`, `camera`, `delete`,
//! `marker`, or `clear`. Rows and columns are 0-based terminal cells;
//! `place.row`/`col` are the CENTER of the placement, matching RGP
//! semantics.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::cast::{Theme, View};

/// A full scene document.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scene {
    /// Transmission metadata.
    pub meta: Meta,
    /// Stage (terminal grid + opening presentation).
    #[serde(default)]
    pub stage: Stage,
    /// Timed steps.
    pub cast: Vec<Step>,
}

/// Transmission metadata.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    /// Transmission title.
    pub title: String,
    /// Authoring agent identity (e.g. `hermes/loom-7`).
    #[serde(default)]
    pub agent: Option<String>,
    /// Art-direction mood tag.
    #[serde(default)]
    pub mood: Option<String>,
}

/// Stage configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stage {
    /// Terminal grid columns.
    #[serde(default = "default_cols")]
    pub cols: u16,
    /// Terminal grid rows.
    #[serde(default = "default_rows")]
    pub rows: u16,
    /// Opening presentation mode: `flat2d`, `plane3d`, `mobius3d`.
    #[serde(default)]
    pub mode: Option<String>,
    /// Opening warp amount, `0.0..=1.0`.
    #[serde(default)]
    pub warp: Option<f32>,
    /// Opening camera view.
    #[serde(default)]
    pub view: Option<View>,
    /// Terminal theme.
    #[serde(default)]
    pub theme: Option<Theme>,
    /// Player should loop the transmission.
    #[serde(default, rename = "loop")]
    pub loop_: bool,
    /// Cap applied to inter-event gaps at playback.
    #[serde(default)]
    pub idle_time_limit: Option<f64>,
}

impl Default for Stage {
    fn default() -> Self {
        Self {
            cols: default_cols(),
            rows: default_rows(),
            mode: None,
            warp: None,
            view: None,
            theme: None,
            loop_: false,
            idle_time_limit: None,
        }
    }
}

fn default_cols() -> u16 {
    104
}

fn default_rows() -> u16 {
    32
}

/// One timed step. Exactly one verb field must be set.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Absolute seconds since transmission start.
    pub at: f64,
    /// Print styled text at a cell position.
    #[serde(default)]
    pub print: Option<PrintArgs>,
    /// Register a 3D object asset.
    #[serde(default)]
    pub register: Option<RegisterArgs>,
    /// Place a registered object at a cell anchor.
    #[serde(default)]
    pub place: Option<PlaceArgs>,
    /// Update a placed object's style/transform.
    #[serde(default)]
    pub update: Option<UpdateArgs>,
    /// Interpolate live-update fields over a duration.
    #[serde(default)]
    pub tween: Option<TweenArgs>,
    /// Stage/camera move (RGP v2 `c` verb).
    #[serde(default)]
    pub camera: Option<CameraArgs>,
    /// Delete one object (`{"id": N}`) or all (`"all"`).
    #[serde(default)]
    pub delete: Option<DeleteArg>,
    /// Insert a navigation marker.
    #[serde(default)]
    pub marker: Option<String>,
    /// Clear the screen and home the cursor.
    #[serde(default)]
    pub clear: Option<bool>,
}

impl Step {
    /// Ensures exactly one verb is present.
    pub fn verb_count(&self) -> usize {
        usize::from(self.print.is_some())
            + usize::from(self.register.is_some())
            + usize::from(self.place.is_some())
            + usize::from(self.update.is_some())
            + usize::from(self.tween.is_some())
            + usize::from(self.camera.is_some())
            + usize::from(self.delete.is_some())
            + usize::from(self.marker.is_some())
            + usize::from(self.clear.is_some())
    }
}

/// `print` verb arguments.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrintArgs {
    /// 0-based row.
    pub row: u16,
    /// 0-based column.
    pub col: u16,
    /// Text to print (single line).
    pub text: String,
    /// Foreground color, `#rrggbb`.
    #[serde(default)]
    pub fg: Option<String>,
    /// Background color, `#rrggbb`.
    #[serde(default)]
    pub bg: Option<String>,
    /// Bold text.
    #[serde(default)]
    pub bold: bool,
    /// Erase to end of line after the text (hygiene when reusing a row).
    #[serde(default)]
    pub el: bool,
}

/// `register` verb arguments.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterArgs {
    /// Object id.
    pub id: u32,
    /// Asset format (`obj`, `glb`, `stl`); inferred from `file` when omitted.
    #[serde(default)]
    pub fmt: Option<String>,
    /// Asset file embedded into the cast as a chunked payload
    /// (relative to the scene file).
    #[serde(default)]
    pub file: Option<PathBuf>,
    /// Ratty-embedded asset path (e.g. `CairoSpinyMouse.obj`) —
    /// only for assets that ship inside ratty itself.
    #[serde(default)]
    pub path: Option<String>,
    /// Payload source name for diagnostics.
    #[serde(default)]
    pub name: Option<String>,
    /// OBJ normalization flag.
    #[serde(default)]
    pub normalize: Option<bool>,
}

/// `place` verb arguments. `row`/`col` are the CENTER cell of the span.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaceArgs {
    /// Registered object id.
    pub id: u32,
    /// Center anchor row (0-based).
    pub row: u16,
    /// Center anchor column (0-based).
    pub col: u16,
    /// Width in cells.
    pub w: u32,
    /// Height in cells.
    pub h: u32,
    /// Enable the built-in spin/bob animation.
    #[serde(default)]
    pub animate: Option<bool>,
    /// Uniform scale multiplier.
    #[serde(default)]
    pub scale: Option<f32>,
    /// Extrusion depth / z push.
    #[serde(default)]
    pub depth: Option<f32>,
    /// Object color, `#rrggbb`.
    #[serde(default)]
    pub color: Option<String>,
    /// Brightness multiplier.
    #[serde(default)]
    pub brightness: Option<f32>,
    /// Translation offset from the anchor.
    #[serde(default)]
    pub px: Option<f32>,
    /// Translation offset from the anchor.
    #[serde(default)]
    pub py: Option<f32>,
    /// Translation offset from the anchor.
    #[serde(default)]
    pub pz: Option<f32>,
    /// Rotation in degrees.
    #[serde(default)]
    pub rx: Option<f32>,
    /// Rotation in degrees.
    #[serde(default)]
    pub ry: Option<f32>,
    /// Rotation in degrees.
    #[serde(default)]
    pub rz: Option<f32>,
    /// Non-uniform scale.
    #[serde(default)]
    pub sx: Option<f32>,
    /// Non-uniform scale.
    #[serde(default)]
    pub sy: Option<f32>,
    /// Non-uniform scale.
    #[serde(default)]
    pub sz: Option<f32>,
    /// Spin speed in radians per second (RGP v2); omitted = global config.
    #[serde(default)]
    pub spin: Option<f32>,
    /// Bob speed in radians per second (RGP v2); omitted = global config.
    #[serde(default)]
    pub bob: Option<f32>,
    /// Bob amplitude as a fraction of cell height (RGP v2).
    #[serde(default)]
    pub bobamp: Option<f32>,
    /// Phase offset in radians applied to spin and bob (RGP v2).
    #[serde(default)]
    pub phase: Option<f32>,
}

/// `update` verb arguments: partial style/transform update.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateArgs {
    /// Placed object id.
    pub id: u32,
    /// See [`PlaceArgs::animate`].
    #[serde(default)]
    pub animate: Option<bool>,
    /// See [`PlaceArgs::scale`].
    #[serde(default)]
    pub scale: Option<f32>,
    /// See [`PlaceArgs::depth`]. Forces a renderer respawn.
    #[serde(default)]
    pub depth: Option<f32>,
    /// See [`PlaceArgs::color`]. Forces a renderer respawn.
    #[serde(default)]
    pub color: Option<String>,
    /// See [`PlaceArgs::brightness`]. Forces a renderer respawn.
    #[serde(default)]
    pub brightness: Option<f32>,
    /// Translation offset from the anchor.
    #[serde(default)]
    pub px: Option<f32>,
    /// Translation offset from the anchor.
    #[serde(default)]
    pub py: Option<f32>,
    /// Translation offset from the anchor.
    #[serde(default)]
    pub pz: Option<f32>,
    /// Rotation in degrees.
    #[serde(default)]
    pub rx: Option<f32>,
    /// Rotation in degrees.
    #[serde(default)]
    pub ry: Option<f32>,
    /// Rotation in degrees.
    #[serde(default)]
    pub rz: Option<f32>,
    /// Non-uniform scale.
    #[serde(default)]
    pub sx: Option<f32>,
    /// Non-uniform scale.
    #[serde(default)]
    pub sy: Option<f32>,
    /// Non-uniform scale.
    #[serde(default)]
    pub sz: Option<f32>,
    /// See [`PlaceArgs::spin`]. Set-only: cannot revert to the global rate.
    #[serde(default)]
    pub spin: Option<f32>,
    /// See [`PlaceArgs::bob`]. Set-only.
    #[serde(default)]
    pub bob: Option<f32>,
    /// See [`PlaceArgs::bobamp`]. Set-only.
    #[serde(default)]
    pub bobamp: Option<f32>,
    /// See [`PlaceArgs::phase`].
    #[serde(default)]
    pub phase: Option<f32>,
}

/// `tween` verb arguments: interpolated live updates.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TweenArgs {
    /// Placed object id.
    pub id: u32,
    /// Duration in seconds.
    pub dur: f64,
    /// Updates per second (default 30).
    #[serde(default)]
    pub fps: Option<f64>,
    /// Easing: `linear` (default) or `in-out`.
    #[serde(default)]
    pub ease: Option<String>,
    /// Target values for live-update fields
    /// (`px py pz rx ry rz sx sy sz scale`).
    pub to: BTreeMap<String, f64>,
}

/// Fields a tween may animate: live-update fields only. `depth`, `color`,
/// and `brightness` force the renderer to despawn/respawn the object every
/// update and are rejected in tweens. The v2 animation rates are live and
/// therefore tweenable (a tween over `spin` accelerates the spin smoothly).
pub const TWEENABLE_FIELDS: [&str; 14] = [
    "px", "py", "pz", "rx", "ry", "rz", "sx", "sy", "sz", "scale", "spin", "bob", "bobamp", "phase",
];

/// `camera` verb arguments: an RGP v2 `c` stage move. All fields optional
/// and absolute; `dur`/`ease` tween warp/yaw/pitch/zoom engine-side (mode
/// changes are always instant).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraArgs {
    /// Presentation mode: `flat2d`, `plane3d`, `mobius3d`.
    #[serde(default)]
    pub mode: Option<String>,
    /// Plane warp amount, `0.0..=1.0`.
    #[serde(default)]
    pub warp: Option<f32>,
    /// Camera yaw in radians.
    #[serde(default)]
    pub yaw: Option<f32>,
    /// Camera pitch in radians.
    #[serde(default)]
    pub pitch: Option<f32>,
    /// Camera zoom, `0.1..=4.0`.
    #[serde(default)]
    pub zoom: Option<f32>,
    /// Tween duration in seconds; omitted = instant.
    #[serde(default)]
    pub dur: Option<f64>,
    /// Easing: `linear`, `in`, `out`, or `in-out` (default `in-out`).
    #[serde(default)]
    pub ease: Option<String>,
}

/// `delete` verb argument.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum DeleteArg {
    /// Delete one object by id.
    Id {
        /// Object id.
        id: u32,
    },
    /// Delete all inline objects (`"all"`).
    All(String),
}

impl DeleteArg {
    /// Returns the target id, or `None` for delete-all.
    pub fn id(&self) -> Result<Option<u32>> {
        match self {
            Self::Id { id } => Ok(Some(*id)),
            Self::All(word) if word == "all" => Ok(None),
            Self::All(word) => bail!("delete must be {{\"id\": N}} or \"all\", got \"{word}\""),
        }
    }
}
