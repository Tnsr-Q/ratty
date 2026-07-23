//! The `viz.*` family: typed data-visualization snapshots over OSC 777.
//!
//! Trusted collectors in the `ratty-ai` CLI (`ps`, `fs`, `git`, `net`, the
//! `kill` watcher) gather data locally under the invoking user's own
//! permissions, then lower it onto the wire as `viz.set` snapshots and
//! `viz.effect` annotations. The terminal side — this module — only ever
//! *renders* what it is handed: **a viz command never causes the terminal
//! to execute a command, read a file, enumerate processes, or open a
//! network resource.** Transmissions may ship synthetic payloads; honesty
//! about liveness sits with the emitter, which is why every snapshot must
//! carry [`VizCapture`] provenance.
//!
//! Payloads ride the wire as unpadded base64url JSON (decoded here with
//! [`crate::query::b64url_decode`] under hard limits, never the RGP
//! standard-base64 codec) and are validated against per-kind versioned
//! schemas. The schema version is part of the registered kind name
//! (`ps.v1`); an unknown version is an unknown kind.
//!
//! Semantics that deliberately diverge from `object.add` (documented in
//! `protocols/viz.md`):
//!
//! - A same-kind `viz.set` on a live caller-owned id is an **atomic
//!   upsert** — a watcher refresh replaces the snapshot wholesale.
//! - Changing the kind of a live id requires `replace=true`; without it
//!   the command rejects `kind-mismatch`.
//! - After `viz.remove` the id **may be reused** (watchers restart under
//!   stable ids) — there is no never-reuse ledger; the per-namespace cap
//!   ([`MAX_VIZ_PER_NAMESPACE`]) alone bounds the registry.
//! - `viz.effect` targets stable *domain keys* (a pid, a path, a branch,
//!   an interface name), never entities; a known id with an absent key
//!   still acks ok and renders nothing — a kill racing a snapshot refresh
//!   is not an error.

use std::collections::{HashMap, HashSet, VecDeque};

use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::prelude::*;

use crate::ai::AiCommand;
use crate::osc::{RattyAiCommand, ai_object_namespace};
use crate::query::codes;
use crate::query_channel::{AckOutcome, AiDiagnostics, ack_commit};

// The payload/item/label limits are part of the wire contract and live in
// the shared std-only `osc` module so the `ratty-ai` collectors compile
// the exact same numbers; re-exported here because this module owns their
// enforcement (decode limits below).
pub use crate::osc::{
    MAX_VIZ_ITEMS_PER_SNAPSHOT, MAX_VIZ_LABEL_BYTES, MAX_VIZ_PAYLOAD_BYTES,
    MAX_VIZ_POINTS_PER_SERIES, MAX_VIZ_POINTS_PER_SNAPSHOT, MAX_VIZ_SERIES_PER_SNAPSHOT,
};
// The schemas and decoder live in the std-only `viz_wire` sibling module so
// `tools/silk` can include and re-run the terminal's real decoder;
// re-exported wholesale so this module remains the viz family's one door.
pub use crate::viz_wire::*;

// A `viz.set` payload rides a single OSC 777 sequence, and the OSC
// watchdog (`crate::inline::MAX_OSC_SEQUENCE_BYTES`) truncates anything
// longer into a garbage tail with *no error ack* — the failure would be
// silent. base64url expands 3 payload bytes into 4 characters; 1 KiB of
// headroom generously covers the envelope (action, id, kind, anchor
// params, tok=). This must hold or the decode limit advertises payloads
// the wire cannot actually carry.
const _: () =
    assert!(MAX_VIZ_PAYLOAD_BYTES.div_ceil(3) * 4 + 1024 <= crate::inline::MAX_OSC_SEQUENCE_BYTES);

/// Upper bound on live visualizations per agent namespace: an honest
/// failure instead of an unbounded registry driven by untrusted output.
/// Ids may be reused after `viz.remove`, so unlike the object ledger this
/// cap is the *only* bound on the registry.
pub const MAX_VIZ_PER_NAMESPACE: usize = 32;

/// Upper bound on queued effects per visualization. The queue drains every
/// frame the renderer runs; the cap only matters when a hostile stream
/// floods effects faster than frames — the oldest queued effect is dropped
/// (newest wins; effects are ephemeral presentation, not state).
pub const MAX_VIZ_PENDING_EFFECTS: usize = 16;

/// Seconds a `viz.effect` animation runs before it expires on its own.
/// Effects are bounded presentation, never state: every animation restores
/// (or, for `died`, removes) its child after exactly this long, so a
/// hostile effect stream cannot pin a child in a mutated pose.
pub const VIZ_EFFECT_SECONDS: f32 = 0.8;

/// Default footprint width in cells when `viz.set` places an anchor
/// without `cols=`.
pub const DEFAULT_VIZ_COLUMNS: u16 = 24;

/// Default footprint height in cells when `viz.set` places an anchor
/// without `rows=`.
pub const DEFAULT_VIZ_ROWS: u16 = 8;

// ── Effects ──

/// The registered `viz.effect` names. Effects are bounded, self-expiring
/// animations on a keyed child; they never mutate snapshot data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VizEffectKind {
    /// The keyed item was confirmed gone (shrink and fade, then hide the
    /// child until the next snapshot).
    Died,
    /// The keyed item survived a kill attempt (brief shake).
    Survived,
    /// Permission was denied (color flash).
    Denied,
    /// The keyed item was already gone (color flash).
    Missing,
    /// The watcher timed out before observing an outcome (color flash).
    Timeout,
    /// Draw attention to the keyed item (pulse).
    Highlight,
}

impl VizEffectKind {
    /// Parses a wire effect name, or `None` when unregistered.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "died" => Self::Died,
            "survived" => Self::Survived,
            "denied" => Self::Denied,
            "missing" => Self::Missing,
            "timeout" => Self::Timeout,
            "highlight" => Self::Highlight,
            _ => return None,
        })
    }

    /// The wire name of this effect.
    pub fn name(self) -> &'static str {
        match self {
            Self::Died => "died",
            Self::Survived => "survived",
            Self::Denied => "denied",
            Self::Missing => "missing",
            Self::Timeout => "timeout",
            Self::Highlight => "highlight",
        }
    }
}

/// One queued keyed effect awaiting the renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedVizEffect {
    /// The domain key the effect targets (pid / path / branch / iface, as
    /// a string). May name nothing in the current snapshot — effects
    /// tolerate absent targets by design.
    pub key: String,
    /// The registered effect.
    pub effect: VizEffectKind,
}

// ── Render vocabulary ──
//
// Every kind lowers onto keyed children (one small mesh per item) with a
// normalized magnitude, a palette slot, and a placement slot. The M3.5
// telemetry kinds place children on a near-square grid; the M3.6 chart
// kinds place them at data-driven positions inside a plot area whose
// static geometry (axes, gridlines, labels, tracks) draws as vello paths
// in `crate::viz_draw`.

/// The fixed material palette viz children draw from. Slots are semantic
/// (what the color *means*), so every kind maps states onto the same
/// small, consistent set instead of inventing per-kind colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VizPaletteSlot {
    /// Actively doing work / the current selection (running process,
    /// checked-out branch, interface that is up).
    Active,
    /// Alive but idle (sleeping process).
    Idle,
    /// A state worth attention (zombie/stopped process, interface down).
    Alert,
    /// A container of other things (directory).
    Container,
    /// No particular state (file, unrecognized process state).
    Neutral,
}

impl VizPaletteSlot {
    /// The slot's base color.
    pub fn color(self) -> Color {
        match self {
            Self::Active => Color::srgb(0.45, 0.75, 0.45),
            Self::Idle => Color::srgb(0.45, 0.55, 0.70),
            Self::Alert => Color::srgb(0.80, 0.35, 0.30),
            Self::Container => Color::srgb(0.80, 0.65, 0.30),
            Self::Neutral => Color::srgb(0.62, 0.62, 0.66),
        }
    }
}

impl From<VizStateTag> for VizPaletteSlot {
    /// The chart-family state vocabulary is the palette by its wire names.
    fn from(tag: VizStateTag) -> Self {
        match tag {
            VizStateTag::Active => Self::Active,
            VizStateTag::Idle => Self::Idle,
            VizStateTag::Alert => Self::Alert,
            VizStateTag::Container => Self::Container,
            VizStateTag::Neutral => Self::Neutral,
        }
    }
}

/// The palette rotation `chart.line.v1` series fall back to when they
/// carry no explicit state, so neighboring series stay distinguishable.
/// `Alert` is deliberately absent — red stays an explicit claim.
pub const SERIES_PALETTE_CYCLE: [VizPaletteSlot; 4] = [
    VizPaletteSlot::Active,
    VizPaletteSlot::Idle,
    VizPaletteSlot::Container,
    VizPaletteSlot::Neutral,
];

/// Where a keyed child sits inside its visualization's footprint. `Grid`
/// is the M3.5 near-square vocabulary; the chart kinds place children at
/// data-driven positions inside the plot area. `crate::viz_draw` maps
/// these normalized coordinates into root-local space with the same
/// insets it draws the vello underlay with, so meshes and paths can
/// never drift apart.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VizSlot {
    /// A near-square grid cell by item order (the M3.5 telemetry kinds).
    Grid,
    /// One bar in a single-row category chart, rising to the child's
    /// magnitude.
    Bar,
    /// A series end-marker at plot coordinates, each normalized to
    /// `0..=1` (y up).
    Marker {
        /// Normalized x within the plot area.
        x: f32,
        /// Normalized y within the plot area.
        y: f32,
    },
    /// A gauge needle-tip at `fraction` around its dial.
    Needle {
        /// Normalized dial position in `0..=1`.
        fraction: f32,
    },
    /// A span on a timeline lane covering `t0..=t1` of the window.
    Span {
        /// Lane index, top to bottom.
        lane: usize,
        /// Total lane count in the snapshot.
        lane_count: usize,
        /// Normalized span start in `0..=1`.
        t0: f32,
        /// Normalized span end in `0..=1`.
        t1: f32,
    },
}

/// One snapshot item lowered onto the shared render vocabulary: a stable
/// domain key, a normalized magnitude, a palette slot, and a placement
/// slot.
#[derive(Debug, Clone, PartialEq)]
pub struct VizChildSpec {
    /// The item's stable semantic key (pid / path / branch / iface /
    /// series name / event id as a string) — the same key `viz.effect`
    /// targets.
    pub key: String,
    /// Magnitude in `0.0..=1.0`, normalized *within the snapshot* (the
    /// tallest bar is the snapshot's largest item, not an absolute unit)
    /// unless the kind carries a fixed axis (`chart.bar.v1 max`, gauge
    /// ranges), which pins the scale across refreshes.
    pub magnitude: f32,
    /// The semantic palette slot for the item's state.
    pub palette: VizPaletteSlot,
    /// Where the child sits inside the footprint.
    pub slot: VizSlot,
}

/// The `chart.bar.v1` axis maximum: the fixed `max` when the payload
/// carries one, else the snapshot's largest value (never below epsilon).
/// Shared with the underlay so the drawn axis label and the mesh heights
/// can never disagree.
pub(crate) fn bar_axis_max(bar: &ChartBarV1) -> f64 {
    bar.max.unwrap_or_else(|| {
        bar.items
            .iter()
            .map(|item| item.value)
            .fold(0.0_f64, f64::max)
    })
    .max(f64::EPSILON)
}

/// The `chart.line.v1` plot ranges as `((x_min, x_max), (y_min, y_max))`:
/// x always spans the data; y honors the fixed pair when present. Shared
/// with the underlay so polylines and series markers land on identical
/// coordinates.
pub(crate) fn line_chart_ranges(line: &ChartLineV1) -> ((f64, f64), (f64, f64)) {
    let mut x_min = f64::INFINITY;
    let mut x_max = f64::NEG_INFINITY;
    let mut y_min = f64::INFINITY;
    let mut y_max = f64::NEG_INFINITY;
    for point in line.series.iter().flat_map(|series| &series.points) {
        x_min = x_min.min(point.x);
        x_max = x_max.max(point.x);
        y_min = y_min.min(point.y);
        y_max = y_max.max(point.y);
    }
    if x_min > x_max {
        (x_min, x_max) = (0.0, 1.0);
    }
    if let (Some(low), Some(high)) = (line.y_min, line.y_max) {
        (y_min, y_max) = (low, high);
    } else if y_min > y_max {
        (y_min, y_max) = (0.0, 1.0);
    }
    ((x_min, x_max), (y_min, y_max))
}

/// The `timeline.v1` window as `(start, end)`: the fixed pair when
/// present, else derived from the events (`min t` to `max t + dur`).
/// Shared with the underlay so the drawn window labels match the spans.
pub(crate) fn timeline_window(timeline: &TimelineV1) -> (f64, f64) {
    if let (Some(t0), Some(t1)) = (timeline.t0, timeline.t1) {
        return (t0, t1);
    }
    let mut start = f64::INFINITY;
    let mut end = f64::NEG_INFINITY;
    for event in timeline.lanes.iter().flat_map(|lane| &lane.events) {
        start = start.min(event.t);
        end = end.max(event.t + event.dur);
    }
    if start > end { (0.0, 1.0) } else { (start, end) }
}

/// Normalizes `value` into `low..=high` as `0..=1`, mapping a degenerate
/// range onto its center.
fn range_normalized(value: f64, low: f64, high: f64) -> f32 {
    let span = high - low;
    if span <= f64::EPSILON {
        return 0.5;
    }
    (((value - low) / span) as f32).clamp(0.0, 1.0)
}

/// The gauge needle fraction: `value` inside `min..=max`, clamped — the
/// dial pins, the printed value stays honest.
pub(crate) fn gauge_fraction(item: &ChartGaugeItem) -> f32 {
    range_normalized(item.value, item.min, item.max)
}

/// Lowers a decoded payload onto the shared render vocabulary, in item
/// order. Grid magnitudes are normalized within the snapshot (cpu for
/// `ps`, log-scaled size for `fs`, log-scaled rx+tx for `net`; `git`
/// branches weight the checked-out branch); chart kinds normalize against
/// their axis (`bar_axis_max`, `line_chart_ranges`, gauge ranges,
/// `timeline_window`).
pub fn viz_child_specs(payload: &VizPayload) -> Vec<VizChildSpec> {
    match payload {
        VizPayload::Ps(ps) => {
            let max_cpu = ps
                .items
                .iter()
                .map(|item| item.cpu)
                .fold(0.0_f32, f32::max)
                .max(f32::EPSILON);
            ps.items
                .iter()
                .map(|item| VizChildSpec {
                    key: item.pid.to_string(),
                    magnitude: (item.cpu / max_cpu).clamp(0.0, 1.0),
                    palette: ps_state_palette(&item.state),
                    slot: VizSlot::Grid,
                })
                .collect()
        }
        VizPayload::Fs(fs) => {
            let max_size = fs.items.iter().map(|item| item.size).max().unwrap_or(0);
            fs.items
                .iter()
                .map(|item| VizChildSpec {
                    key: item.path.clone(),
                    magnitude: log_normalized(item.size, max_size),
                    palette: match item.kind {
                        FsEntryKind::Dir => VizPaletteSlot::Container,
                        FsEntryKind::File => VizPaletteSlot::Neutral,
                    },
                    slot: VizSlot::Grid,
                })
                .collect()
        }
        VizPayload::Git(git) => git
            .branches
            .iter()
            .map(|branch| VizChildSpec {
                key: branch.name.clone(),
                magnitude: if branch.current { 1.0 } else { 0.55 },
                palette: if branch.current {
                    VizPaletteSlot::Active
                } else {
                    VizPaletteSlot::Neutral
                },
                slot: VizSlot::Grid,
            })
            .collect(),
        VizPayload::Net(net) => {
            let max_total = net
                .items
                .iter()
                .map(|item| item.rx_bytes.saturating_add(item.tx_bytes))
                .max()
                .unwrap_or(0);
            net.items
                .iter()
                .map(|item| VizChildSpec {
                    key: item.iface.clone(),
                    magnitude: log_normalized(
                        item.rx_bytes.saturating_add(item.tx_bytes),
                        max_total,
                    ),
                    palette: if item.up {
                        VizPaletteSlot::Active
                    } else {
                        VizPaletteSlot::Alert
                    },
                    slot: VizSlot::Grid,
                })
                .collect()
        }
        VizPayload::ChartBar(bar) => {
            let axis_max = bar_axis_max(bar);
            bar.items
                .iter()
                .map(|item| VizChildSpec {
                    key: item.key.clone(),
                    magnitude: ((item.value / axis_max) as f32).clamp(0.0, 1.0),
                    palette: item.state.into(),
                    slot: VizSlot::Bar,
                })
                .collect()
        }
        VizPayload::ChartLine(line) => {
            let ((x_min, x_max), (y_min, y_max)) = line_chart_ranges(line);
            line.series
                .iter()
                .enumerate()
                .map(|(index, series)| {
                    // The marker sits on the series' last point — the
                    // freshest reading — or centers on an empty series.
                    let (x, y) = series.points.last().map_or((0.5, 0.5), |point| {
                        (
                            range_normalized(point.x, x_min, x_max),
                            range_normalized(point.y, y_min, y_max),
                        )
                    });
                    VizChildSpec {
                        key: series.name.clone(),
                        magnitude: y,
                        palette: series.state.map_or(
                            SERIES_PALETTE_CYCLE[index % SERIES_PALETTE_CYCLE.len()],
                            VizPaletteSlot::from,
                        ),
                        slot: VizSlot::Marker { x, y },
                    }
                })
                .collect()
        }
        VizPayload::ChartGauge(gauge) => gauge
            .items
            .iter()
            .map(|item| {
                let fraction = gauge_fraction(item);
                VizChildSpec {
                    key: item.key.clone(),
                    magnitude: fraction,
                    palette: item.state.into(),
                    slot: VizSlot::Needle { fraction },
                }
            })
            .collect(),
        VizPayload::Timeline(timeline) => {
            let (window_start, window_end) = timeline_window(timeline);
            let lane_count = timeline.lanes.len();
            let mut specs = Vec::new();
            for (lane, lane_data) in timeline.lanes.iter().enumerate() {
                for event in &lane_data.events {
                    // An event wholly outside an explicit window renders
                    // nothing — clipping is presentation, and effects on
                    // its id tolerate the absent child like any other
                    // absent key.
                    if event.t + event.dur < window_start || event.t > window_end {
                        continue;
                    }
                    let t0 = range_normalized(event.t, window_start, window_end);
                    let t1 = range_normalized(event.t + event.dur, window_start, window_end);
                    specs.push(VizChildSpec {
                        key: event.id.clone(),
                        magnitude: 1.0,
                        palette: event.state.into(),
                        slot: VizSlot::Span {
                            lane,
                            lane_count,
                            t0,
                            t1,
                        },
                    });
                }
            }
            specs
        }
    }
}

/// Maps a process scheduler-state tag onto the palette.
fn ps_state_palette(state: &str) -> VizPaletteSlot {
    let lower = state.to_ascii_lowercase();
    if lower.starts_with("run") {
        VizPaletteSlot::Active
    } else if lower.starts_with("zombie") || lower.starts_with("stop") || lower.starts_with("dead")
    {
        VizPaletteSlot::Alert
    } else if lower.is_empty() {
        VizPaletteSlot::Neutral
    } else {
        VizPaletteSlot::Idle
    }
}

/// Log-scale normalization for byte magnitudes: `ln(1+value)/ln(1+max)`,
/// so a snapshot mixing kilobytes and gigabytes still shows the small
/// items instead of flattening them against the largest.
fn log_normalized(value: u64, max: u64) -> f32 {
    let denominator = ((max as f64) + 1.0).ln();
    if denominator <= f64::EPSILON {
        return 0.0;
    }
    ((((value as f64) + 1.0).ln() / denominator) as f32).clamp(0.0, 1.0)
}

// ── Render components ──

/// Root entity of one visualization's render tree, keyed by viz id, with
/// the live child ledger the renderer diffs snapshots against (the
/// handle-caching-in-state pattern the inline objects use).
#[derive(Component)]
pub struct VizObjectRoot {
    /// The visualization id this root renders.
    pub viz_id: u32,
    /// Live keyed children: semantic key → child record. Snapshot
    /// refreshes diff against this map so only changed children respawn.
    pub(crate) children: HashMap<String, VizChildRecord>,
}

/// One live keyed child of a viz root: its entity plus the cached
/// material handle, current palette slot, and base pose. The ledger is the
/// single source of truth for a child's rest pose — effect animations copy
/// it at start and restore it on expiry, so a rebuild that re-lays-out a
/// mid-animation child updates the restore target, never the drifting
/// animated transform.
#[derive(Debug, Clone)]
pub(crate) struct VizChildRecord {
    /// The child mesh entity.
    pub(crate) entity: Entity,
    /// The child's bespoke material (mutated in place on state changes
    /// and during effect animations; freed when the handle drops).
    pub(crate) material: Handle<StandardMaterial>,
    /// The palette slot the material currently shows.
    pub(crate) palette: VizPaletteSlot,
    /// Root-local rest translation from the grid layout.
    pub(crate) base_translation: Vec3,
    /// Root-local rest scale from the grid layout.
    pub(crate) base_scale: Vec3,
}

/// The semantic key a viz child renders — carried on the child so effect
/// expiry can clean the parent ledger without a reverse lookup.
#[derive(Component)]
pub struct VizKeyedItem {
    /// The domain key (pid / path / branch / iface as a string).
    pub key: String,
}

/// A running keyed-effect animation. Inserted when the renderer drains a
/// queued effect, advanced every frame, and removed (restoring the base
/// pose) after [`VIZ_EFFECT_SECONDS`]; `died` despawns the child instead.
#[derive(Component)]
pub struct VizEffectAnim {
    /// The registered effect being animated.
    pub effect: VizEffectKind,
    /// Seconds since the animation started.
    pub elapsed: f32,
    /// Child translation restored on expiry.
    pub base_translation: Vec3,
    /// Child scale restored on expiry.
    pub base_scale: Vec3,
}

// ── Registry ──

/// Grid anchor for a visualization: top-left cell plus footprint extent.
/// Anchors shift with terminal scroll exactly like inline object anchors
/// and are dropped (payload kept) once fully off the top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VizAnchor {
    /// Top-left row.
    pub row: u16,
    /// Top-left column.
    pub col: u16,
    /// Footprint width in cells.
    pub cols: u16,
    /// Footprint height in cells.
    pub rows: u16,
}

/// One live visualization record.
#[derive(Debug, Clone)]
pub struct VizEntry {
    /// The decoded, validated payload ([`VizPayload::kind`] is the kind).
    pub payload: VizPayload,
    /// Grid anchor; `None` while unplaced or after scrolling off the top
    /// (the renderer hides anchor-less visualizations).
    pub anchor: Option<VizAnchor>,
    /// Revision from the registry-wide monotonic mutation counter.
    pub revision: u64,
    /// Bounded queue of keyed effects awaiting the renderer; when full
    /// the oldest entry is dropped ([`MAX_VIZ_PENDING_EFFECTS`]).
    pub pending_effects: VecDeque<QueuedVizEffect>,
}

/// Live data visualizations keyed by caller-owned id, with granular
/// rebuild/removal key sets for the renderer.
///
/// Deliberate divergences from the object registries: ids may be reused
/// after `viz.remove` (no never-reuse ledger — watchers restart under
/// stable ids), and mutations ride the per-id key sets exclusively —
/// **never** the inline scene-wide dirty flag, so a snapshot refresh can
/// never respawn a transmission's scene.
#[derive(Resource, Default)]
pub struct VizRegistry {
    entries: HashMap<u32, VizEntry>,
    /// One monotonic counter across all entries, so revisions also order
    /// mutations between visualizations (mirrors the inline registry).
    mutation_seq: u64,
    rebuild: HashSet<u32>,
    removed: HashSet<u32>,
}

impl VizRegistry {
    /// Whether a live visualization exists under `id`.
    pub fn contains(&self, id: u32) -> bool {
        self.entries.contains_key(&id)
    }

    /// The live entry under `id`.
    pub fn get(&self, id: u32) -> Option<&VizEntry> {
        self.entries.get(&id)
    }

    /// Iterates live entries in arbitrary order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &VizEntry)> {
        self.entries.iter().map(|(id, entry)| (*id, entry))
    }

    /// Number of live visualizations.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry holds no live visualizations.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of live visualizations whose id lies in the given AI
    /// namespace.
    pub fn namespace_len(&self, namespace: u8) -> usize {
        self.entries
            .keys()
            .filter(|id| ai_object_namespace(**id) == Some(namespace))
            .count()
    }

    /// The entry's current revision, or 0 when the id has no live record.
    pub fn revision(&self, id: u32) -> u64 {
        self.entries.get(&id).map_or(0, |entry| entry.revision)
    }

    /// Whether any live visualization is anchored (the scroll-tracking
    /// gate in `pump_pty_output`).
    pub fn has_anchors(&self) -> bool {
        self.entries.values().any(|entry| entry.anchor.is_some())
    }

    /// Inserts or atomically replaces the entry under `id`, stamping a
    /// fresh revision and queueing a granular rebuild. Pending effects
    /// survive a snapshot refresh (they drain the same frame and tolerate
    /// keys the new snapshot no longer carries).
    pub(crate) fn upsert(&mut self, id: u32, payload: VizPayload, anchor: Option<VizAnchor>) {
        self.mutation_seq += 1;
        let pending_effects = self
            .entries
            .remove(&id)
            .map(|entry| entry.pending_effects)
            .unwrap_or_default();
        self.entries.insert(
            id,
            VizEntry {
                payload,
                anchor,
                revision: self.mutation_seq,
                pending_effects,
            },
        );
        self.removed.remove(&id);
        self.rebuild.insert(id);
    }

    /// Queues a keyed effect on a live visualization, bumping its
    /// revision. Returns `false` when no visualization exists under `id`;
    /// whether the key names anything in the snapshot is deliberately not
    /// checked here — effects tolerate absent targets.
    pub(crate) fn queue_effect(&mut self, id: u32, key: String, effect: VizEffectKind) -> bool {
        let Some(entry) = self.entries.get_mut(&id) else {
            return false;
        };
        if entry.pending_effects.len() >= MAX_VIZ_PENDING_EFFECTS {
            entry.pending_effects.pop_front();
        }
        entry
            .pending_effects
            .push_back(QueuedVizEffect { key, effect });
        self.mutation_seq += 1;
        entry.revision = self.mutation_seq;
        true
    }

    /// Removes a visualization, queueing a granular despawn. Returns
    /// whether it existed. The id becomes immediately reusable.
    pub(crate) fn remove(&mut self, id: u32) -> bool {
        let existed = self.entries.remove(&id).is_some();
        if existed {
            self.rebuild.remove(&id);
            self.removed.insert(id);
        }
        existed
    }

    /// Clears every visualization (the `reset` command), queueing
    /// granular despawns for all of them.
    pub(crate) fn clear_all(&mut self) {
        self.removed.extend(self.entries.keys().copied());
        self.entries.clear();
        self.rebuild.clear();
    }

    /// Drains ids whose render trees must be rebuilt from the current
    /// entry state. Renderer contract: an id with no existing tree spawns
    /// one from [`VizRegistry::get`]; an id with a live tree is *diffed*
    /// by semantic key — unchanged children keep their entities, changed
    /// children mutate in place, and only added/dropped keys spawn or
    /// despawn.
    pub fn take_rebuilds(&mut self) -> HashSet<u32> {
        std::mem::take(&mut self.rebuild)
    }

    /// Drains ids whose render trees must be despawned (the entry is
    /// gone). Disjoint from [`VizRegistry::take_rebuilds`]: a removed id
    /// re-added before the renderer ran lands only in the rebuild set.
    pub fn take_removals(&mut self) -> HashSet<u32> {
        std::mem::take(&mut self.removed)
    }

    /// Whether the rebuild pass has queued work: pending rebuilds,
    /// removals, or undrained effects. The renderer's `run_if` gate, so
    /// idle frames skip the pass entirely.
    pub fn has_render_work(&self) -> bool {
        !self.rebuild.is_empty()
            || !self.removed.is_empty()
            || self
                .entries
                .values()
                .any(|entry| !entry.pending_effects.is_empty())
    }

    /// Drains the queued effects for `id` (empty when the id is dead).
    pub(crate) fn take_pending_effects(&mut self, id: u32) -> VecDeque<QueuedVizEffect> {
        self.entries
            .get_mut(&id)
            .map(|entry| std::mem::take(&mut entry.pending_effects))
            .unwrap_or_default()
    }

    /// Applies upward terminal scroll to anchors, mirroring the inline
    /// registry: rows shift up, and an anchor scrolled fully off the top
    /// is dropped while the payload is kept (the renderer hides it; a
    /// later placing `viz.set` re-anchors it). No rebuilds are queued —
    /// the renderer positions from anchors per-frame.
    pub(crate) fn apply_scroll(&mut self, rows_scrolled: u16) {
        if rows_scrolled == 0 {
            return;
        }
        for entry in self.entries.values_mut() {
            let Some(anchor) = entry.anchor else {
                continue;
            };
            let new_row = i32::from(anchor.row) - i32::from(rows_scrolled);
            if new_row + i32::from(anchor.rows) <= 0 {
                entry.anchor = None;
            } else {
                entry.anchor = Some(VizAnchor {
                    row: new_row.max(0) as u16,
                    ..anchor
                });
            }
        }
    }
}

// ── Plugin & applier ──

/// Registers the [`VizRegistry`], the `viz.*` command applier, and the
/// renderer systems.
///
/// The applier runs after `pump_pty_output` so commands apply the frame
/// they arrive, and `answer_queries` is ordered after it (see
/// [`crate::ai::RattyAiPlugin`]) so a same-chunk write-then-read observes
/// the write. The renderer is three systems in `crate::systems`, each with
/// its own `run_if` so idle frames cost nothing:
///
/// - `rebuild_viz_objects` (after the applier *and* after
///   `answer_queries`, so a same-chunk `state.viz` still sees queued
///   effects) drains the granular rebuild/removal sets and queued effects
///   — never a scene-wide dirty flag — spawning/despawning roots and
///   diffing keyed children.
/// - `sync_viz_objects` (after the rebuild pass, so same-frame spawns are
///   positioned at once) places every root from its anchor per frame.
/// - `animate_viz_effects` advances and expires effect animations.
pub struct VizPlugin;

impl Plugin for VizPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VizRegistry>()
            .add_systems(
                Update,
                apply_viz_commands.after(crate::systems::pump_pty_output),
            )
            .add_systems(
                Update,
                // After `answer_queries` as well: the rebuild pass drains
                // each entry's pending-effect queue, and a same-chunk
                // `viz.effect` + `state.viz` must observe the queued
                // effect before the renderer consumes it.
                crate::systems::rebuild_viz_objects
                    .after(apply_viz_commands)
                    .after(crate::query_channel::answer_queries)
                    .run_if(|registry: Res<VizRegistry>| registry.has_render_work()),
            )
            .add_systems(
                Update,
                crate::systems::sync_viz_objects
                    .after(crate::systems::rebuild_viz_objects)
                    .run_if(|roots: Query<(), With<VizObjectRoot>>| !roots.is_empty()),
            )
            .add_systems(
                Update,
                crate::systems::animate_viz_effects
                    .after(crate::systems::rebuild_viz_objects)
                    .run_if(|animated: Query<(), With<VizEffectAnim>>| !animated.is_empty()),
            );
    }
}

/// Applies queued `viz.*` commands to the [`VizRegistry`], enforcing
/// AI-range id ownership per ingress source. Owns the `viz.set` /
/// `viz.effect` / `viz.remove` acks; `reset` clears the registry silently
/// (its single ack belongs to `apply_ai_commands`).
pub fn apply_viz_commands(
    mut commands: MessageReader<AiCommand>,
    mut registry: ResMut<VizRegistry>,
    mut acks: MessageWriter<AckOutcome>,
    mut diagnostics: ResMut<AiDiagnostics>,
) {
    for AiCommand {
        source,
        ack_token,
        command,
    } in commands.read()
    {
        // Every rejection below both warns and lands in the caller's
        // `state.errors` ring; `tok=` commands additionally get their
        // error ack.
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
            RattyAiCommand::VizSet {
                id,
                kind,
                data,
                x,
                y,
                cols,
                rows,
                replace,
            } => {
                let id = *id;
                let replace = *replace;
                // Placement params arrive raw off the std-only wire module.
                // Parse each here; a *present* value that is not a u16 cell
                // coordinate is an explicit `bad-payload` (naming the param)
                // rather than a silent unplacement that would ack ok while
                // dropping the placement the caller asked for.
                macro_rules! cell_param {
                    ($raw:expr, $name:literal) => {
                        match $raw {
                            Some(text) => match text.parse::<u16>() {
                                Ok(value) => Some(value),
                                Err(_) => {
                                    warn!(
                                        "ratty-ai: viz.set rejected: {}={text:?} is not a u16 \
                                         cell coordinate",
                                        $name
                                    );
                                    reject!(
                                        "viz.set",
                                        codes::BAD_PAYLOAD,
                                        "{}= must be a u16 cell coordinate; got '{text}'",
                                        $name
                                    );
                                    continue;
                                }
                            },
                            None => None,
                        }
                    };
                }
                let x = cell_param!(x, "x");
                let y = cell_param!(y, "y");
                let cols = cell_param!(cols, "cols");
                let rows = cell_param!(rows, "rows");
                if ai_object_namespace(id) != Some(source.namespace()) {
                    warn!(
                        "ratty-ai: viz.set rejected: id {id:#010x} is outside the caller's \
                         AI range/namespace ({})",
                        source.namespace()
                    );
                    reject!(
                        "viz.set",
                        codes::NOT_OWNER,
                        "id {id:#010x} is outside the caller's AI range/namespace ({})",
                        source.namespace()
                    );
                    continue;
                }
                if x.is_some() != y.is_some() {
                    warn!("ratty-ai: viz.set rejected: x= and y= place together");
                    reject!(
                        "viz.set",
                        codes::BAD_PAYLOAD,
                        "x= and y= place together; got one without the other"
                    );
                    continue;
                }
                if cols == Some(0) || rows == Some(0) {
                    warn!("ratty-ai: viz.set rejected: zero-cell footprint");
                    reject!(
                        "viz.set",
                        codes::BAD_PAYLOAD,
                        "cols= and rows= must be at least 1"
                    );
                    continue;
                }
                let payload = match decode_viz_payload(kind, data) {
                    Ok(payload) => payload,
                    Err(error) => {
                        warn!("ratty-ai: viz.set rejected: {}", error.message);
                        reject!("viz.set", error.code, "{}", error.message);
                        continue;
                    }
                };
                let live = registry.get(id);
                match live {
                    // A live id of a different kind: replacement must be
                    // explicit (same-kind sets are always atomic upserts).
                    Some(entry) if entry.payload.kind() != payload.kind() && !replace => {
                        warn!(
                            "ratty-ai: viz.set rejected: id {id:#010x} is live as kind '{}' \
                             (pass replace=true to replace it with '{}')",
                            entry.payload.kind(),
                            payload.kind()
                        );
                        reject!(
                            "viz.set",
                            codes::KIND_MISMATCH,
                            "id {id:#010x} is live as kind '{}' (pass replace=true to replace \
                             it with '{}')",
                            entry.payload.kind(),
                            payload.kind()
                        );
                        continue;
                    }
                    // A fresh id claims a namespace slot; upserts never
                    // count against the cap.
                    None if registry.namespace_len(source.namespace()) >= MAX_VIZ_PER_NAMESPACE => {
                        warn!(
                            "ratty-ai: viz.set rejected: namespace {} is at its \
                             {MAX_VIZ_PER_NAMESPACE}-visualization limit",
                            source.namespace()
                        );
                        reject!(
                            "viz.set",
                            codes::NAMESPACE_CAP,
                            "namespace {} is at its {MAX_VIZ_PER_NAMESPACE}-visualization limit",
                            source.namespace()
                        );
                        continue;
                    }
                    _ => {}
                }
                let existing_anchor = live.and_then(|entry| entry.anchor);
                let anchor = if let (Some(col), Some(row)) = (x, y) {
                    Some(VizAnchor {
                        row,
                        col,
                        cols: cols
                            .or(existing_anchor.map(|anchor| anchor.cols))
                            .unwrap_or(DEFAULT_VIZ_COLUMNS),
                        rows: rows
                            .or(existing_anchor.map(|anchor| anchor.rows))
                            .unwrap_or(DEFAULT_VIZ_ROWS),
                    })
                } else if let Some(anchor) = existing_anchor {
                    // An upsert without placement keeps the anchor (a
                    // watcher refresh must not move or unplace the view);
                    // a bare footprint change is allowed.
                    Some(VizAnchor {
                        cols: cols.unwrap_or(anchor.cols),
                        rows: rows.unwrap_or(anchor.rows),
                        ..anchor
                    })
                } else {
                    if cols.is_some() || rows.is_some() {
                        warn!("ratty-ai: viz.set rejected: footprint without an anchor");
                        reject!(
                            "viz.set",
                            codes::BAD_PAYLOAD,
                            "cols=/rows= need a placed anchor (supply x= and y=)"
                        );
                        continue;
                    }
                    None
                };
                registry.upsert(id, payload, anchor);
                ack_commit(&mut acks, *source, ack_token);
            }
            RattyAiCommand::VizEffect { id, key, effect } => {
                let id = *id;
                if ai_object_namespace(id) != Some(source.namespace()) {
                    warn!(
                        "ratty-ai: viz.effect rejected: id {id:#010x} is outside the caller's \
                         AI range/namespace ({})",
                        source.namespace()
                    );
                    reject!(
                        "viz.effect",
                        codes::NOT_OWNER,
                        "id {id:#010x} is outside the caller's AI range/namespace ({})",
                        source.namespace()
                    );
                    continue;
                }
                let Some(parsed) = VizEffectKind::parse(effect) else {
                    warn!("ratty-ai: viz.effect rejected: unknown effect '{effect}'");
                    reject!(
                        "viz.effect",
                        codes::BAD_PAYLOAD,
                        "unknown effect '{effect}' (died, survived, denied, missing, timeout, \
                         highlight)"
                    );
                    continue;
                };
                if key.len() > MAX_VIZ_LABEL_BYTES {
                    warn!("ratty-ai: viz.effect rejected: key exceeds {MAX_VIZ_LABEL_BYTES} bytes");
                    reject!(
                        "viz.effect",
                        codes::TOO_LARGE,
                        "key exceeds {MAX_VIZ_LABEL_BYTES} bytes"
                    );
                    continue;
                }
                if registry.queue_effect(id, key.clone(), parsed) {
                    // A known id whose snapshot lacks the key still
                    // commits: effects tolerate absent targets by design
                    // (a kill racing a snapshot refresh is not an error)
                    // and render nothing.
                    ack_commit(&mut acks, *source, ack_token);
                } else {
                    warn!("ratty-ai: viz.effect rejected: no visualization with id {id:#010x}");
                    reject!(
                        "viz.effect",
                        codes::UNKNOWN_ID,
                        "no visualization with id {id:#010x}"
                    );
                }
            }
            RattyAiCommand::VizRemove { id } => {
                let id = *id;
                if ai_object_namespace(id) != Some(source.namespace()) {
                    warn!(
                        "ratty-ai: viz.remove rejected: id {id:#010x} is outside the caller's \
                         AI range/namespace ({})",
                        source.namespace()
                    );
                    reject!(
                        "viz.remove",
                        codes::NOT_OWNER,
                        "id {id:#010x} is outside the caller's AI range/namespace ({})",
                        source.namespace()
                    );
                    continue;
                }
                if registry.remove(id) {
                    ack_commit(&mut acks, *source, ack_token);
                } else {
                    warn!("ratty-ai: viz.remove rejected: no visualization with id {id:#010x}");
                    reject!(
                        "viz.remove",
                        codes::UNKNOWN_ID,
                        "no visualization with id {id:#010x}"
                    );
                }
            }
            RattyAiCommand::Reset => {
                // Reset's single ack belongs to apply_ai_commands; the viz
                // registry clears silently, like the object and effect
                // handlers.
                registry.clear_all();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::message::Messages;

    use crate::query::b64url_encode;
    use crate::runtime::IngressSource;
    use serde_json::json;

    const ID: u32 = 0x8000_0001;

    fn encode(value: serde_json::Value) -> String {
        b64url_encode(value.to_string().as_bytes())
    }

    fn ps_payload(pids: &[u32]) -> serde_json::Value {
        json!({
            "capture": { "source": "test/synthetic", "ts": "2026-07-22T00:00:00Z" },
            "items": pids
                .iter()
                .map(|pid| json!({ "pid": pid, "name": format!("proc{pid}") }))
                .collect::<Vec<_>>(),
        })
    }

    fn decoded_ps(pids: &[u32]) -> VizPayload {
        decode_viz_payload("ps.v1", &encode(ps_payload(pids))).expect("ps payload decodes")
    }

    // ── Decode & limits ──

    #[test]
    fn limits_are_pinned() {
        assert_eq!(MAX_VIZ_PAYLOAD_BYTES, 32 * 1024);
        assert_eq!(MAX_VIZ_ITEMS_PER_SNAPSHOT, 256);
        assert_eq!(MAX_VIZ_LABEL_BYTES, 128);
        assert_eq!(MAX_VIZ_PER_NAMESPACE, 32);
        assert_eq!(MAX_VIZ_PENDING_EFFECTS, 16);
    }

    /// The render defaults protocols/viz.md commits to, drift-guarded
    /// alongside the size caps (regression: F7). The collector-side
    /// defaults (slot ids, `--top` cap, walk cap) are pinned by a sibling
    /// test in `tools/ratty-ai`.
    #[test]
    fn render_defaults_are_pinned() {
        assert_eq!(VIZ_EFFECT_SECONDS, 0.8, "viz.md: effects expire in 0.8s");
        assert_eq!(DEFAULT_VIZ_COLUMNS, 24, "viz.md: default footprint 24×8");
        assert_eq!(DEFAULT_VIZ_ROWS, 8, "viz.md: default footprint 24×8");
    }

    #[test]
    fn registered_kinds_list_matches_the_decoder() {
        // "e30" is base64url for "{}": every registered kind must reach
        // its schema decoder (failing bad-payload on the missing capture,
        // never bad-kind).
        for kind in REGISTERED_VIZ_KINDS {
            let error = decode_viz_payload(kind, "e30").expect_err("empty object fails schema");
            assert_eq!(error.code, codes::BAD_PAYLOAD, "{kind} is registered");
        }
        let error = decode_viz_payload("ps.v2", "e30").expect_err("unknown version");
        assert_eq!(error.code, codes::BAD_KIND);
    }

    #[test]
    fn ps_v1_decodes_and_ignores_unknown_fields() {
        let payload = json!({
            "capture": { "source": "test", "ts": "now", "future": true },
            "items": [
                { "pid": 42, "name": "ratd", "cpu": 12.5, "mem": 1024,
                  "state": "running", "future_field": [1, 2] },
            ],
            "schema_extension": { "ignored": true },
        });
        let decoded = decode_viz_payload("ps.v1", &encode(payload)).expect("decodes");
        let VizPayload::Ps(ps) = decoded else {
            panic!("expected ps.v1");
        };
        assert_eq!(ps.capture.source, "test");
        assert_eq!(ps.items.len(), 1);
        assert_eq!(ps.items[0].pid, 42);
        assert_eq!(ps.items[0].cpu, 12.5);
        assert_eq!(ps.items[0].state, "running");
    }

    #[test]
    fn capture_is_required() {
        let error = decode_viz_payload("ps.v1", &encode(json!({ "items": [] })))
            .expect_err("capture is mandatory provenance");
        assert_eq!(error.code, codes::BAD_PAYLOAD);
    }

    #[test]
    fn malformed_base64_and_json_reject_bad_payload() {
        let error = decode_viz_payload("ps.v1", "!!!").expect_err("not base64url");
        assert_eq!(error.code, codes::BAD_PAYLOAD);
        let error = decode_viz_payload("ps.v1", &b64url_encode(b"not json")).expect_err("not JSON");
        assert_eq!(error.code, codes::BAD_PAYLOAD);
    }

    #[test]
    fn oversized_payloads_reject_too_large_before_allocation() {
        let oversized = vec![b'x'; MAX_VIZ_PAYLOAD_BYTES + 1];
        let big = b64url_encode(&oversized);
        let error = decode_viz_payload("ps.v1", &big).expect_err("over the decode limit");
        assert_eq!(error.code, codes::TOO_LARGE);
    }

    #[test]
    fn item_and_label_limits_reject_too_large() {
        let pids: Vec<u32> = (0..(MAX_VIZ_ITEMS_PER_SNAPSHOT as u32 + 1)).collect();
        let error =
            decode_viz_payload("ps.v1", &encode(ps_payload(&pids))).expect_err("over the item cap");
        assert_eq!(error.code, codes::TOO_LARGE);

        let payload = json!({
            "capture": { "source": "test", "ts": "now" },
            "items": [{ "pid": 1, "name": "x".repeat(MAX_VIZ_LABEL_BYTES + 1) }],
        });
        let error = decode_viz_payload("ps.v1", &encode(payload)).expect_err("over the label cap");
        assert_eq!(error.code, codes::TOO_LARGE);
    }

    #[test]
    fn non_finite_cpu_rejects_bad_payload() {
        // `3.5e38` is valid JSON but exceeds f32::MAX, decoding to +inf;
        // inf/inf then yields NaN magnitudes. The terminal must reject the
        // payload, not render NaN child transforms (regression: F1).
        for hostile in [3.5e38_f64, f64::MAX, 1e39] {
            let payload = json!({
                "capture": { "source": "test", "ts": "now" },
                "items": [{ "pid": 1, "name": "greedy", "cpu": hostile }],
            });
            let error = decode_viz_payload("ps.v1", &encode(payload))
                .expect_err("a non-finite cpu is rejected");
            assert_eq!(error.code, codes::BAD_PAYLOAD, "cpu {hostile} rejects");
        }
        // The sanity floor: a finite cpu still decodes and lowers to a
        // finite magnitude.
        let ok = json!({
            "capture": { "source": "test", "ts": "now" },
            "items": [{ "pid": 1, "name": "calm", "cpu": 12.5 }],
        });
        let decoded = decode_viz_payload("ps.v1", &encode(ok)).expect("finite cpu decodes");
        assert!(
            viz_child_specs(&decoded)
                .iter()
                .all(|spec| spec.magnitude.is_finite()),
            "finite cpu yields finite magnitudes"
        );
    }

    #[test]
    fn fs_v1_validates_entry_kinds() {
        let payload = |kind: &str| {
            json!({
                "capture": { "source": "test", "ts": "now" },
                "root": "/tmp",
                "items": [{ "path": "a", "kind": kind, "size": 10, "depth": 1 }],
            })
        };
        let decoded = decode_viz_payload("fs.v1", &encode(payload("dir"))).expect("dir decodes");
        let VizPayload::Fs(fs) = decoded else {
            panic!("expected fs.v1");
        };
        assert_eq!(fs.items[0].kind, FsEntryKind::Dir);
        let error =
            decode_viz_payload("fs.v1", &encode(payload("symlink"))).expect_err("unknown kind");
        assert_eq!(error.code, codes::BAD_PAYLOAD);
    }

    #[test]
    fn git_v1_defaults_optional_counts() {
        let payload = json!({
            "capture": { "source": "test", "ts": "now" },
            "repo": "ratty",
            "branches": [{ "name": "main", "current": true }, { "name": "dev" }],
        });
        let decoded = decode_viz_payload("git.v1", &encode(payload)).expect("decodes");
        let VizPayload::Git(git) = decoded else {
            panic!("expected git.v1");
        };
        assert_eq!(git.branches.len(), 2);
        assert!(git.branches[0].current);
        assert!(!git.branches[1].current);
        assert_eq!(git.status.staged, 0);
        assert_eq!(git.ahead, 0);
        assert_eq!(VizPayload::Git(git).item_count(), 2, "git counts branches");
    }

    #[test]
    fn net_v1_is_interface_counters_and_requires_link_state() {
        let payload = json!({
            "capture": { "source": "test", "ts": "now" },
            "items": [{ "iface": "en0", "rx_bytes": 10, "tx_bytes": 20, "up": true }],
        });
        let decoded = decode_viz_payload("net.v1", &encode(payload)).expect("decodes");
        let VizPayload::Net(net) = decoded else {
            panic!("expected net.v1");
        };
        assert_eq!(net.items[0].iface, "en0");
        assert!(net.items[0].up);

        // `up` is required: a defaulted link state would be a claim the
        // emitter never made.
        let payload = json!({
            "capture": { "source": "test", "ts": "now" },
            "items": [{ "iface": "en0" }],
        });
        let error = decode_viz_payload("net.v1", &encode(payload)).expect_err("up required");
        assert_eq!(error.code, codes::BAD_PAYLOAD);
    }

    // ── Registry ──

    #[test]
    fn rebuild_and_removal_sets_are_granular_and_disjoint() {
        let mut registry = VizRegistry::default();
        registry.upsert(ID, decoded_ps(&[1]), None);
        registry.upsert(ID + 1, decoded_ps(&[2]), None);
        assert_eq!(
            registry.take_rebuilds(),
            HashSet::from([ID, ID + 1]),
            "each upsert queues its own rebuild"
        );
        assert!(registry.take_rebuilds().is_empty(), "take drains");
        assert!(registry.remove(ID));
        assert_eq!(registry.take_removals(), HashSet::from([ID]));
        // Remove-then-re-add before the renderer runs: rebuild only.
        assert!(registry.remove(ID + 1));
        registry.upsert(ID + 1, decoded_ps(&[3]), None);
        assert!(registry.take_removals().is_empty());
        assert_eq!(registry.take_rebuilds(), HashSet::from([ID + 1]));
        // Reset queues removals for everything live.
        registry.clear_all();
        assert_eq!(registry.take_removals(), HashSet::from([ID + 1]));
        assert!(registry.is_empty());
    }

    #[test]
    fn effect_queue_is_bounded_newest_wins() {
        let mut registry = VizRegistry::default();
        registry.upsert(ID, decoded_ps(&[1]), None);
        for index in 0..(MAX_VIZ_PENDING_EFFECTS + 3) {
            assert!(registry.queue_effect(ID, format!("k{index}"), VizEffectKind::Highlight));
        }
        let entry = registry.get(ID).expect("entry lives");
        assert_eq!(entry.pending_effects.len(), MAX_VIZ_PENDING_EFFECTS);
        let last = entry.pending_effects.back().expect("non-empty");
        assert_eq!(last.key, format!("k{}", MAX_VIZ_PENDING_EFFECTS + 2));
        let first = entry.pending_effects.front().expect("non-empty");
        assert_eq!(first.key, "k3", "the oldest entries were dropped");
    }

    #[test]
    fn scroll_shifts_anchors_and_drops_offscreen_ones_keeping_payloads() {
        let mut registry = VizRegistry::default();
        let anchor = |row: u16, rows: u16| {
            Some(VizAnchor {
                row,
                col: 4,
                cols: 10,
                rows,
            })
        };
        registry.upsert(ID, decoded_ps(&[1]), anchor(5, 4));
        registry.upsert(ID + 1, decoded_ps(&[2]), anchor(1, 2));
        let revision_before = registry.revision(ID);
        registry.apply_scroll(3);
        let shifted = registry.get(ID).expect("payload kept");
        assert_eq!(shifted.anchor.expect("still anchored").row, 2);
        let dropped = registry.get(ID + 1).expect("payload kept");
        assert!(dropped.anchor.is_none(), "fully off-top drops the anchor");
        assert_eq!(
            registry.revision(ID),
            revision_before,
            "scroll is a derived change, not a record mutation"
        );
    }

    // ── Applier ──

    fn test_app() -> App {
        let mut app = App::new();
        app.init_resource::<VizRegistry>();
        app.init_resource::<AiDiagnostics>();
        app.add_message::<AiCommand>();
        app.add_message::<AckOutcome>();
        app.add_systems(Update, apply_viz_commands);
        app
    }

    /// Sends a `tok=`-carrying command and returns its single decided ack.
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

    fn viz_set(id: u32, kind: &str, data: String) -> RattyAiCommand {
        RattyAiCommand::VizSet {
            id,
            kind: kind.to_string(),
            data,
            x: None,
            y: None,
            cols: None,
            rows: None,
            replace: false,
        }
    }

    fn registry(app: &App) -> &VizRegistry {
        app.world().resource::<VizRegistry>()
    }

    #[test]
    fn same_kind_set_is_an_atomic_upsert_that_keeps_the_anchor() {
        let mut app = test_app();
        let (ok, _) = send_tok(
            &mut app,
            RattyAiCommand::VizSet {
                id: ID,
                kind: "ps.v1".to_string(),
                data: encode(ps_payload(&[1, 2])),
                x: Some("10".to_string()),
                y: Some("5".to_string()),
                cols: None,
                rows: None,
                replace: false,
            },
        );
        assert!(ok);
        let first_revision = registry(&app).revision(ID);
        let entry = registry(&app).get(ID).expect("live");
        assert_eq!(entry.payload.item_count(), 2);
        assert_eq!(
            entry.anchor,
            Some(VizAnchor {
                row: 5,
                col: 10,
                cols: DEFAULT_VIZ_COLUMNS,
                rows: DEFAULT_VIZ_ROWS,
            })
        );

        // A watcher refresh: same kind, no placement — wholesale payload
        // replace, anchor kept, revision bumped.
        let (ok, _) = send_tok(
            &mut app,
            viz_set(ID, "ps.v1", encode(ps_payload(&[1, 2, 3]))),
        );
        assert!(ok);
        let entry = registry(&app).get(ID).expect("live");
        assert_eq!(entry.payload.item_count(), 3, "snapshot replaced wholesale");
        assert_eq!(
            entry.anchor.expect("anchor kept").col,
            10,
            "an unplaced refresh never moves the view"
        );
        assert!(registry(&app).revision(ID) > first_revision);
    }

    #[test]
    fn kind_change_requires_replace() {
        let mut app = test_app();
        let (ok, _) = send_tok(&mut app, viz_set(ID, "ps.v1", encode(ps_payload(&[1]))));
        assert!(ok);
        let git = json!({
            "capture": { "source": "test", "ts": "now" },
            "repo": "ratty",
        });
        let (ok, code) = send_tok(&mut app, viz_set(ID, "git.v1", encode(git.clone())));
        assert!(!ok);
        assert_eq!(code, Some(codes::KIND_MISMATCH));
        assert_eq!(
            registry(&app).get(ID).expect("live").payload.kind(),
            "ps.v1",
            "a rejected set changes nothing"
        );

        let (ok, _) = send_tok(
            &mut app,
            RattyAiCommand::VizSet {
                id: ID,
                kind: "git.v1".to_string(),
                data: encode(git),
                x: None,
                y: None,
                cols: None,
                rows: None,
                replace: true,
            },
        );
        assert!(ok, "replace=true allows the kind change");
        assert_eq!(
            registry(&app).get(ID).expect("live").payload.kind(),
            "git.v1"
        );
    }

    #[test]
    fn removed_ids_may_be_reused() {
        let mut app = test_app();
        let (ok, _) = send_tok(&mut app, viz_set(ID, "ps.v1", encode(ps_payload(&[1]))));
        assert!(ok);
        let (ok, _) = send_tok(&mut app, RattyAiCommand::VizRemove { id: ID });
        assert!(ok);
        assert!(!registry(&app).contains(ID));
        // Deliberate divergence from the object never-reuse ledger:
        // watchers restart under stable ids.
        let (ok, _) = send_tok(&mut app, viz_set(ID, "ps.v1", encode(ps_payload(&[2]))));
        assert!(ok, "a removed viz id is immediately reusable");
        // Removing an unknown id is an honest failure.
        let (ok, code) = send_tok(&mut app, RattyAiCommand::VizRemove { id: ID + 1 });
        assert!(!ok);
        assert_eq!(code, Some(codes::UNKNOWN_ID));
    }

    #[test]
    fn ownership_and_namespace_cap_are_enforced() {
        let mut app = test_app();
        // Below the AI range, then a foreign namespace: both not-owner.
        for id in [42_u32, 0x8100_0001] {
            let (ok, code) = send_tok(&mut app, viz_set(id, "ps.v1", encode(ps_payload(&[1]))));
            assert!(!ok);
            assert_eq!(code, Some(codes::NOT_OWNER));
        }
        // Fill namespace 0 to its cap, bypassing the wire.
        {
            let mut registry = app.world_mut().resource_mut::<VizRegistry>();
            for index in 0..MAX_VIZ_PER_NAMESPACE as u32 {
                registry.upsert(0x8000_0100 + index, decoded_ps(&[index]), None);
            }
        }
        let (ok, code) = send_tok(&mut app, viz_set(ID, "ps.v1", encode(ps_payload(&[1]))));
        assert!(!ok);
        assert_eq!(code, Some(codes::NAMESPACE_CAP));
        // An upsert of a live id is not a new slot and still commits.
        let (ok, _) = send_tok(
            &mut app,
            viz_set(0x8000_0100, "ps.v1", encode(ps_payload(&[9]))),
        );
        assert!(ok, "upserts never count against the cap");
    }

    #[test]
    fn effects_tolerate_absent_keys_but_not_absent_ids() {
        let mut app = test_app();
        let (ok, _) = send_tok(&mut app, viz_set(ID, "ps.v1", encode(ps_payload(&[1234]))));
        assert!(ok);
        // A key the snapshot no longer carries: still ok (kill vs refresh
        // races are not errors); the effect queues and renders nothing.
        let (ok, _) = send_tok(
            &mut app,
            RattyAiCommand::VizEffect {
                id: ID,
                key: "9999".to_string(),
                effect: "died".to_string(),
            },
        );
        assert!(ok);
        assert_eq!(
            registry(&app).get(ID).expect("live").pending_effects.len(),
            1
        );
        // An absent viz id is an honest failure.
        let (ok, code) = send_tok(
            &mut app,
            RattyAiCommand::VizEffect {
                id: ID + 1,
                key: "1".to_string(),
                effect: "died".to_string(),
            },
        );
        assert!(!ok);
        assert_eq!(code, Some(codes::UNKNOWN_ID));
        // An unregistered effect name is rejected.
        let (ok, code) = send_tok(
            &mut app,
            RattyAiCommand::VizEffect {
                id: ID,
                key: "1234".to_string(),
                effect: "explode".to_string(),
            },
        );
        assert!(!ok);
        assert_eq!(code, Some(codes::BAD_PAYLOAD));
    }

    #[test]
    fn placement_params_are_validated() {
        let mut app = test_app();
        let base = |x: Option<u16>, y: Option<u16>, cols: Option<u16>, rows: Option<u16>| {
            RattyAiCommand::VizSet {
                id: ID,
                kind: "ps.v1".to_string(),
                data: encode(ps_payload(&[1])),
                x: x.map(|v| v.to_string()),
                y: y.map(|v| v.to_string()),
                cols: cols.map(|v| v.to_string()),
                rows: rows.map(|v| v.to_string()),
                replace: false,
            }
        };
        let (ok, code) = send_tok(&mut app, base(Some(3), None, None, None));
        assert!(!ok, "x without y is rejected");
        assert_eq!(code, Some(codes::BAD_PAYLOAD));
        let (ok, code) = send_tok(&mut app, base(Some(3), Some(4), Some(0), None));
        assert!(!ok, "zero-cell footprints are rejected");
        assert_eq!(code, Some(codes::BAD_PAYLOAD));
        let (ok, code) = send_tok(&mut app, base(None, None, Some(10), None));
        assert!(!ok, "a footprint needs an anchor");
        assert_eq!(code, Some(codes::BAD_PAYLOAD));
        assert!(!registry(&app).contains(ID), "rejected sets insert nothing");
        // A *present* placement value that is not a u16 rejects rather than
        // silently coercing to absent (regression: F2) — the cell parse
        // rejects at the first present-but-invalid param, before the
        // pairing check, so setting only the target param exercises it.
        let malformed = |name: &str, value: &str| RattyAiCommand::VizSet {
            id: ID,
            kind: "ps.v1".to_string(),
            data: encode(ps_payload(&[1])),
            x: (name == "x").then(|| value.to_string()),
            y: (name == "y").then(|| value.to_string()),
            cols: (name == "cols").then(|| value.to_string()),
            rows: (name == "rows").then(|| value.to_string()),
            replace: false,
        };
        for param in ["x", "y", "cols", "rows"] {
            let (ok, code) = send_tok(&mut app, malformed(param, "abc"));
            assert!(!ok, "{param}=abc is rejected, not silently dropped");
            assert_eq!(code, Some(codes::BAD_PAYLOAD));
            assert!(
                !registry(&app).contains(ID),
                "a malformed {param} inserts nothing"
            );
        }
        // 70000 overflows a u16 and rejects like any other non-u16.
        let (ok, code) = send_tok(&mut app, malformed("x", "70000"));
        assert!(!ok, "an out-of-range coordinate rejects");
        assert_eq!(code, Some(codes::BAD_PAYLOAD));
        let (ok, _) = send_tok(&mut app, base(Some(3), Some(4), Some(10), Some(2)));
        assert!(ok);
        assert_eq!(
            registry(&app).get(ID).expect("live").anchor,
            Some(VizAnchor {
                row: 4,
                col: 3,
                cols: 10,
                rows: 2
            })
        );
    }

    #[test]
    fn reset_clears_the_registry_silently() {
        let mut app = test_app();
        let (ok, _) = send_tok(&mut app, viz_set(ID, "ps.v1", encode(ps_payload(&[1]))));
        assert!(ok);
        // Reset carries a token, but the viz applier must NOT ack it —
        // apply_ai_commands owns reset's single ack.
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
        assert!(outcomes.is_empty(), "the viz applier never acks reset");
        assert!(
            registry(&app).is_empty(),
            "reset clears every visualization"
        );
    }
}
