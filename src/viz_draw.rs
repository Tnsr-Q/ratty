//! Chart geometry for the viz family: the vello underlay and the
//! data-driven child poses.
//!
//! The #20 split: **lines, axes, labels, gauge arcs, and timeline tracks
//! lower to vello paths** appended onto the terminal's scene inside the
//! anchored footprint, while **keyed items — bars, series markers, needle
//! tips, event spans — stay Bevy meshes** so the whole M3.5 effects
//! machinery (diff by key, self-expiring animations, `died` removal)
//! applies to chart kinds unchanged.
//!
//! Both halves are computed here from the same plot insets and the same
//! normalization helpers (`crate::viz::{bar_axis_max, line_chart_ranges,
//! timeline_window, gauge_fraction}`), so a mesh and the path under it can
//! never drift apart.
//!
//! Coordinates: underlay ops live in *footprint space* — `(0,0)` the
//! anchored rect's top-left, `(1,1)` its bottom-right, y down, matching
//! texture pixels. Child poses convert into the root's `[-0.5, 0.5]`
//! space (y up) that [`crate::systems::sync_viz_objects`] scales by the
//! footprint's pixel extent. Stroke widths and text heights are fractions
//! of the footprint height.
//!
//! Labels draw through a built-in vector stroke font (uppercase, digits,
//! and chart punctuation): `parley_ratatui` exposes no arbitrary-text API
//! into the scene, and glyphs-as-polylines keep labels honest vello paths
//! with a fitting CRT lineage. Text is truncated to its budget at append
//! time, where the footprint's pixel aspect is known.

use bevy::math::Vec3;
use parley_ratatui::vello::Scene;
use parley_ratatui::vello::kurbo::{Affine, Arc as KurboArc, BezPath, Rect as KurboRect, Stroke};
use parley_ratatui::vello::peniko::{Color as PenikoColor, Fill};

use crate::viz::{
    ChartBarV1, ChartGaugeV1, ChartLineV1, TimelineV1, VizPaletteSlot, VizPayload, VizSlot,
    bar_axis_max, gauge_fraction, line_chart_ranges, line_series_palette, timeline_window,
};

// ── Layout constants ──

/// Plot-area insets, as fractions of the footprint.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PlotInsets {
    /// Left margin (axis labels, lane names).
    pub left: f32,
    /// Right margin.
    pub right: f32,
    /// Top margin (title row).
    pub top: f32,
    /// Bottom margin (x labels, time axis).
    pub bottom: f32,
}

impl PlotInsets {
    /// The plot rect as `(x0, y0, width, height)` in footprint space.
    pub(crate) fn plot(&self) -> (f32, f32, f32, f32) {
        (
            self.left,
            self.top,
            (1.0 - self.left - self.right).max(0.05),
            (1.0 - self.top - self.bottom).max(0.05),
        )
    }
}

/// `chart.bar.v1` insets.
pub(crate) const BAR_INSETS: PlotInsets = PlotInsets {
    left: 0.13,
    right: 0.03,
    top: 0.16,
    bottom: 0.17,
};

/// `chart.line.v1` insets.
pub(crate) const LINE_INSETS: PlotInsets = PlotInsets {
    left: 0.13,
    right: 0.03,
    top: 0.16,
    bottom: 0.15,
};

/// `timeline.v1` insets — the wide left margin carries lane names.
pub(crate) const TIMELINE_INSETS: PlotInsets = PlotInsets {
    left: 0.20,
    right: 0.03,
    top: 0.16,
    bottom: 0.15,
};

/// Minimum bar height as a fraction of the plot height, so zero-value
/// bars stay visible (an empty queue is still a queue).
const BAR_MIN_HEIGHT_FRACTION: f32 = 0.05;

/// Fraction of its column a bar occupies.
const BAR_WIDTH_FRACTION: f32 = 0.66;

/// Mesh depth for bars and spans in root-local space (matches the M3.5
/// grid depth).
const CHART_MESH_DEPTH: f32 = 0.8;

/// Minimum span width so instant timeline events stay visible.
const SPAN_MIN_WIDTH: f32 = 0.008;

/// Fraction of its lane a timeline span occupies vertically.
const SPAN_HEIGHT_FRACTION: f32 = 0.55;

/// Gauge dial center height (footprint space, y down).
const GAUGE_CENTER_Y: f32 = 0.62;

/// Width-per-height of one stroke-font glyph cell, advance included.
const GLYPH_ASPECT: f32 = 0.72;

// ── Draw ops ──

/// Horizontal anchoring for an underlay text op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextAlign {
    /// `pos` is the text's left edge.
    Left,
    /// `pos` is the text's center.
    Center,
    /// `pos` is the text's right edge.
    Right,
}

/// One resolution-independent underlay drawing op in footprint space.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum VizDrawOp {
    /// A filled axis-aligned rectangle.
    Fill {
        /// Top-left corner.
        min: (f32, f32),
        /// Bottom-right corner.
        max: (f32, f32),
        /// Straight-alpha sRGB color.
        color: [f32; 4],
    },
    /// A stroked polyline (a single segment is a two-point polyline).
    Polyline {
        /// Vertices in footprint space.
        points: Vec<(f32, f32)>,
        /// Stroke width as a fraction of the footprint height.
        width: f32,
        /// Straight-alpha sRGB color.
        color: [f32; 4],
    },
    /// A stroked elliptical arc.
    Arc {
        /// Ellipse center.
        center: (f32, f32),
        /// Ellipse radii (x, y) in footprint fractions.
        radii: (f32, f32),
        /// Start angle in radians (y-down screen convention).
        start: f32,
        /// Sweep in radians.
        sweep: f32,
        /// Stroke width as a fraction of the footprint height.
        width: f32,
        /// Straight-alpha sRGB color.
        color: [f32; 4],
    },
    /// A stroke-font text run.
    Text {
        /// Anchor position (interpretation set by `align`); the y is the
        /// glyph box top.
        pos: (f32, f32),
        /// Glyph height as a fraction of the footprint height.
        height: f32,
        /// The text; lowered to uppercase glyphs at append.
        text: String,
        /// Straight-alpha sRGB color.
        color: [f32; 4],
        /// Horizontal anchoring.
        align: TextAlign,
        /// Maximum run width as a fraction of the footprint width; the
        /// run is truncated to fit at append time (aspect-dependent).
        max_width: Option<f32>,
    },
}

/// The straight-alpha sRGB components of a palette slot.
fn slot_rgba(slot: VizPaletteSlot, alpha: f32) -> [f32; 4] {
    let srgba = slot.color().to_srgba();
    [srgba.red, srgba.green, srgba.blue, alpha]
}

/// The translucent backdrop that keeps chart geometry readable over
/// terminal text.
const SCRIM: [f32; 4] = [0.02, 0.02, 0.04, 0.86];
/// Axis and frame lines.
const FRAME: [f32; 4] = [0.62, 0.64, 0.68, 0.9];
/// Interior gridlines and lane separators.
const GRID: [f32; 4] = [0.40, 0.42, 0.46, 0.45];
/// Axis tick labels and lane names.
const LABEL: [f32; 4] = [0.66, 0.68, 0.72, 1.0];
/// Titles.
const TITLE: [f32; 4] = [0.82, 0.84, 0.88, 1.0];

const AXIS_WIDTH: f32 = 0.014;
const GRID_WIDTH: f32 = 0.008;
const SERIES_WIDTH: f32 = 0.018;
const TITLE_HEIGHT: f32 = 0.105;
const LABEL_HEIGHT: f32 = 0.075;

// ── Child poses ──

/// Converts a footprint-space point (y down) into root-local space
/// (`[-0.5, 0.5]`, y up).
fn foot_to_root(fx: f32, fy: f32) -> (f32, f32) {
    (fx - 0.5, 0.5 - fy)
}

/// The shared dial geometry for gauge `index` of `count`, as
/// `(center_x, center_y, radius_x, radius_y)` in footprint space. Used by
/// both the underlay arcs and the needle-tip pose.
pub(crate) fn gauge_dial_geometry(index: usize, count: usize) -> (f32, f32, f32, f32) {
    let count = count.max(1);
    let slot_width = 1.0 / count as f32;
    let cx = (index as f32 + 0.5) * slot_width;
    let rx = (slot_width * 0.42).min(0.30);
    (cx, GAUGE_CENTER_Y, rx, 0.34)
}

/// The dial angle for a needle fraction: `0` points left, `1` points
/// right, sweeping over the top (y-down screen convention).
fn gauge_angle(fraction: f32) -> f32 {
    std::f32::consts::PI * (1.0 + fraction.clamp(0.0, 1.0))
}

/// Root-local rest pose for a chart-slot child at `index` of `count`.
/// Grid slots are the caller's business ([`crate::systems`] keeps the
/// M3.5 near-square math); this covers the data-positioned chart slots.
pub(crate) fn chart_child_pose(
    slot: VizSlot,
    index: usize,
    count: usize,
    magnitude: f32,
) -> (Vec3, Vec3) {
    match slot {
        VizSlot::Grid => {
            // Unreachable from the renderer (it dispatches grid slots to
            // the legacy math); centered and small as an honest fallback.
            (Vec3::ZERO, Vec3::splat(0.1))
        }
        VizSlot::Bar => {
            let (px, py, pw, ph) = BAR_INSETS.plot();
            let count = count.max(1);
            let column = pw / count as f32;
            let height = ph
                * (BAR_MIN_HEIGHT_FRACTION
                    + (1.0 - BAR_MIN_HEIGHT_FRACTION) * magnitude.clamp(0.0, 1.0));
            let cx = px + (index as f32 + 0.5) * column;
            let cy = py + ph - height * 0.5;
            let (x, y) = foot_to_root(cx, cy);
            (
                Vec3::new(x, y, 0.0),
                Vec3::new(column * BAR_WIDTH_FRACTION, height, CHART_MESH_DEPTH),
            )
        }
        VizSlot::Marker { x, y } => {
            let (px, py, pw, ph) = LINE_INSETS.plot();
            let cx = px + x.clamp(0.0, 1.0) * pw;
            let cy = py + (1.0 - y.clamp(0.0, 1.0)) * ph;
            let (rx, ry) = foot_to_root(cx, cy);
            (Vec3::new(rx, ry, 0.0), Vec3::new(0.035, 0.06, 0.5))
        }
        VizSlot::Needle { fraction } => {
            let (cx, cy, rx, ry) = gauge_dial_geometry(index, count);
            let angle = gauge_angle(fraction);
            let tip_x = cx + rx * angle.cos();
            let tip_y = cy + ry * angle.sin();
            let (x, y) = foot_to_root(tip_x, tip_y);
            (Vec3::new(x, y, 0.0), Vec3::new(0.035, 0.06, 0.5))
        }
        VizSlot::Span {
            lane,
            lane_count,
            t0,
            t1,
        } => {
            let (px, py, pw, ph) = TIMELINE_INSETS.plot();
            let lane_height = ph / lane_count.max(1) as f32;
            let cy = py + (lane as f32 + 0.5) * lane_height;
            let x0 = px + t0.clamp(0.0, 1.0) * pw;
            let x1 = px + t1.clamp(0.0, 1.0) * pw;
            let width = (x1 - x0).max(SPAN_MIN_WIDTH);
            // Center on the span, clamped so a window-edge instant's
            // minimum width stays inside the plot.
            let cx = (x0 + width * 0.5).min(px + pw - width * 0.5);
            let (x, y) = foot_to_root(cx, cy);
            (
                Vec3::new(x, y, 0.0),
                Vec3::new(width, lane_height * SPAN_HEIGHT_FRACTION, CHART_MESH_DEPTH),
            )
        }
    }
}

// ── Value formatting ──

/// Formats a magnitude compactly: SI suffixes above 1000, up to two
/// trimmed decimals below.
pub(crate) fn format_value(value: f64) -> String {
    let magnitude = value.abs();
    let (scaled, suffix) = if magnitude >= 1e9 {
        (value / 1e9, "G")
    } else if magnitude >= 1e6 {
        (value / 1e6, "M")
    } else if magnitude >= 1e3 {
        (value / 1e3, "K")
    } else {
        (value, "")
    };
    let text = if suffix.is_empty() && scaled.fract().abs() < 1e-9 {
        format!("{scaled:.0}")
    } else if suffix.is_empty() {
        let trimmed = format!("{scaled:.2}");
        trimmed
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    } else {
        format!("{scaled:.1}")
    };
    format!("{text}{suffix}")
}

/// Formats a timeline axis instant. Values in the unix-epoch range read
/// as a UTC time of day — `ratty-ai history` publishes epoch seconds and
/// `1.8G` would label nothing; anything else formats as a magnitude.
pub(crate) fn format_axis_instant(value: f64) -> String {
    if (1.0e9..4.0e9).contains(&value) {
        let seconds = (value as u64) % 86_400;
        return format!(
            "{:02}:{:02}:{:02}",
            seconds / 3600,
            (seconds % 3600) / 60,
            seconds % 60
        );
    }
    format_value(value)
}

// ── Underlay op builders ──

/// The vello underlay for a payload, or `None` for the grid kinds (their
/// M3.5 look — bare keyed bars over the text — is unchanged).
pub(crate) fn viz_underlay_ops(payload: &VizPayload) -> Option<Vec<VizDrawOp>> {
    match payload {
        VizPayload::Ps(_) | VizPayload::Fs(_) | VizPayload::Git(_) | VizPayload::Net(_) => None,
        VizPayload::ChartBar(bar) => Some(bar_ops(bar)),
        VizPayload::ChartLine(line) => Some(line_ops(line)),
        VizPayload::ChartGauge(gauge) => Some(gauge_ops(gauge)),
        VizPayload::Timeline(timeline) => Some(timeline_ops(timeline)),
    }
}

fn scrim() -> VizDrawOp {
    VizDrawOp::Fill {
        min: (0.0, 0.0),
        max: (1.0, 1.0),
        color: SCRIM,
    }
}

fn segment(a: (f32, f32), b: (f32, f32), width: f32, color: [f32; 4]) -> VizDrawOp {
    VizDrawOp::Polyline {
        points: vec![a, b],
        width,
        color,
    }
}

fn title_op(title: Option<&str>) -> Option<VizDrawOp> {
    title.map(|title| VizDrawOp::Text {
        pos: (0.03, 0.025),
        height: TITLE_HEIGHT,
        text: title.to_string(),
        color: TITLE,
        align: TextAlign::Left,
        max_width: Some(0.94),
    })
}

/// The shared axes-and-gridlines frame for the plot rect.
fn plot_frame(insets: PlotInsets, ops: &mut Vec<VizDrawOp>) {
    let (px, py, pw, ph) = insets.plot();
    ops.push(segment((px, py), (px, py + ph), AXIS_WIDTH, FRAME));
    ops.push(segment(
        (px, py + ph),
        (px + pw, py + ph),
        AXIS_WIDTH,
        FRAME,
    ));
    for step in 1..=3 {
        let y = py + ph * (step as f32 / 4.0);
        ops.push(segment((px, y), (px + pw, y), GRID_WIDTH, GRID));
    }
}

fn bar_ops(bar: &ChartBarV1) -> Vec<VizDrawOp> {
    let mut ops = vec![scrim()];
    ops.extend(title_op(bar.title.as_deref()));
    plot_frame(BAR_INSETS, &mut ops);
    let (px, py, pw, ph) = BAR_INSETS.plot();

    // Axis extremes: the top is the fixed max (or the snapshot's largest
    // value), the bottom is zero.
    let axis_max = bar_axis_max(bar);
    let unit = bar.unit.as_deref().unwrap_or("");
    ops.push(VizDrawOp::Text {
        pos: (px - 0.015, py - LABEL_HEIGHT * 0.4),
        height: LABEL_HEIGHT,
        text: format!("{}{unit}", format_value(axis_max)),
        color: LABEL,
        align: TextAlign::Right,
        max_width: Some(px),
    });
    ops.push(VizDrawOp::Text {
        pos: (px - 0.015, py + ph - LABEL_HEIGHT * 0.6),
        height: LABEL_HEIGHT,
        text: "0".to_string(),
        color: LABEL,
        align: TextAlign::Right,
        max_width: Some(px),
    });

    // One label per bar, centered under its column.
    let count = bar.items.len().max(1);
    let column = pw / count as f32;
    for (index, item) in bar.items.iter().enumerate() {
        let label = item.label.as_deref().unwrap_or(&item.key);
        ops.push(VizDrawOp::Text {
            pos: (px + (index as f32 + 0.5) * column, py + ph + 0.03),
            height: LABEL_HEIGHT,
            text: label.to_string(),
            color: LABEL,
            align: TextAlign::Center,
            max_width: Some(column * 0.95),
        });
    }
    ops
}

fn line_ops(line: &ChartLineV1) -> Vec<VizDrawOp> {
    let mut ops = vec![scrim()];
    ops.extend(title_op(line.title.as_deref()));
    plot_frame(LINE_INSETS, &mut ops);
    let (px, py, pw, ph) = LINE_INSETS.plot();
    let ((x_min, x_max), (y_min, y_max)) = line_chart_ranges(line);

    // Axis extreme labels.
    for (value, y, nudge) in [
        (y_max, py, -LABEL_HEIGHT * 0.4),
        (y_min, py + ph, -LABEL_HEIGHT * 0.6),
    ] {
        ops.push(VizDrawOp::Text {
            pos: (px - 0.015, y + nudge),
            height: LABEL_HEIGHT,
            text: format_value(value),
            color: LABEL,
            align: TextAlign::Right,
            max_width: Some(px),
        });
    }
    for (value, x, align) in [
        (x_min, px, TextAlign::Left),
        (x_max, px + pw, TextAlign::Right),
    ] {
        ops.push(VizDrawOp::Text {
            pos: (x, py + ph + 0.03),
            height: LABEL_HEIGHT,
            text: format_axis_instant(value),
            color: LABEL,
            align,
            max_width: Some(pw * 0.45),
        });
    }

    // The series polylines — the same normalization the markers use.
    let span_x = |x: f64| crate::viz::range_normalized(x, x_min, x_max);
    let span_y = |y: f64| crate::viz::range_normalized(y, y_min, y_max);
    for (index, series) in line.series.iter().enumerate() {
        let color = slot_rgba(line_series_palette(series, index), 0.95);
        if series.points.len() >= 2 {
            ops.push(VizDrawOp::Polyline {
                points: series
                    .points
                    .iter()
                    .map(|point| (px + span_x(point.x) * pw, py + (1.0 - span_y(point.y)) * ph))
                    .collect(),
                width: SERIES_WIDTH,
                color,
            });
        }
        // Series name, stacked below the title at the plot's right edge.
        ops.push(VizDrawOp::Text {
            pos: (
                px + pw - 0.01,
                py + 0.02 + index as f32 * (LABEL_HEIGHT + 0.02),
            ),
            height: LABEL_HEIGHT,
            text: series.name.clone(),
            color,
            align: TextAlign::Right,
            max_width: Some(pw * 0.4),
        });
    }
    ops
}

fn gauge_ops(gauge: &ChartGaugeV1) -> Vec<VizDrawOp> {
    let mut ops = vec![scrim()];
    let count = gauge.items.len().max(1);
    for (index, item) in gauge.items.iter().enumerate() {
        let (cx, cy, rx, ry) = gauge_dial_geometry(index, count);
        let fraction = gauge_fraction(item);
        let color = slot_rgba(item.state.into(), 1.0);
        // The full track, faint, then the value arc over it.
        ops.push(VizDrawOp::Arc {
            center: (cx, cy),
            radii: (rx, ry),
            start: std::f32::consts::PI,
            sweep: std::f32::consts::PI,
            width: AXIS_WIDTH,
            color: GRID,
        });
        if fraction > 0.0 {
            ops.push(VizDrawOp::Arc {
                center: (cx, cy),
                radii: (rx, ry),
                start: std::f32::consts::PI,
                sweep: std::f32::consts::PI * fraction,
                width: SERIES_WIDTH * 1.4,
                color,
            });
        }
        // Dial extremes.
        for (value, x, align) in [
            (item.min, cx - rx, TextAlign::Center),
            (item.max, cx + rx, TextAlign::Center),
        ] {
            ops.push(VizDrawOp::Text {
                pos: (x, cy + 0.04),
                height: LABEL_HEIGHT * 0.9,
                text: format_value(value),
                color: LABEL,
                align,
                max_width: Some(rx * 1.4),
            });
        }
        // The raw value — honest even when the dial clamps — then the
        // label beneath.
        let unit = item.unit.as_deref().unwrap_or("");
        ops.push(VizDrawOp::Text {
            pos: (cx, cy - ry * 0.35),
            height: TITLE_HEIGHT,
            text: format!("{}{unit}", format_value(item.value)),
            color: TITLE,
            align: TextAlign::Center,
            max_width: Some(2.0 * rx * 0.95),
        });
        ops.push(VizDrawOp::Text {
            pos: (cx, cy + 0.04 + LABEL_HEIGHT),
            height: LABEL_HEIGHT,
            text: item.label.as_deref().unwrap_or(&item.key).to_string(),
            color: LABEL,
            align: TextAlign::Center,
            max_width: Some(2.0 * rx * 0.95),
        });
    }
    ops
}

fn timeline_ops(timeline: &TimelineV1) -> Vec<VizDrawOp> {
    let mut ops = vec![scrim()];
    ops.extend(title_op(timeline.title.as_deref()));
    let (px, py, pw, ph) = TIMELINE_INSETS.plot();
    let (window_start, window_end) = timeline_window(timeline);

    // The time axis and its window labels.
    ops.push(segment(
        (px, py + ph),
        (px + pw, py + ph),
        AXIS_WIDTH,
        FRAME,
    ));
    for step in 1..=3 {
        let x = px + pw * (step as f32 / 4.0);
        ops.push(segment((x, py), (x, py + ph), GRID_WIDTH, GRID));
    }
    for (value, x, align) in [
        (window_start, px, TextAlign::Left),
        (window_end, px + pw, TextAlign::Right),
    ] {
        ops.push(VizDrawOp::Text {
            pos: (x, py + ph + 0.03),
            height: LABEL_HEIGHT,
            text: format_axis_instant(value),
            color: LABEL,
            align,
            max_width: Some(pw * 0.45),
        });
    }

    // Lane tracks and names.
    let lane_count = timeline.lanes.len().max(1);
    let lane_height = ph / lane_count as f32;
    for (index, lane) in timeline.lanes.iter().enumerate() {
        let center = py + (index as f32 + 0.5) * lane_height;
        if index > 0 {
            let separator = py + index as f32 * lane_height;
            ops.push(segment(
                (px, separator),
                (px + pw, separator),
                GRID_WIDTH,
                GRID,
            ));
        }
        ops.push(VizDrawOp::Text {
            pos: (px - 0.015, center - LABEL_HEIGHT * 0.5),
            height: LABEL_HEIGHT,
            text: lane.name.clone(),
            color: LABEL,
            align: TextAlign::Right,
            max_width: Some(px - 0.02),
        });
    }
    ops
}

// ── Stroke font ──

/// Polyline strokes for one glyph on a 4-wide, 6-tall grid (y down).
type GlyphStrokes = &'static [&'static [(i8, i8)]];

/// The hollow box drawn for characters outside the charset.
const GLYPH_UNKNOWN: GlyphStrokes = &[&[(0, 0), (4, 0), (4, 6), (0, 6), (0, 0)]];

/// The strokes for a character, or `None` for space. Lowercase maps to
/// uppercase; anything unknown draws a hollow box.
fn glyph_strokes(character: char) -> Option<GlyphStrokes> {
    let upper = character.to_ascii_uppercase();
    let strokes: GlyphStrokes = match upper {
        ' ' => return None,
        'A' => &[&[(0, 6), (0, 2), (2, 0), (4, 2), (4, 6)], &[(0, 3), (4, 3)]],
        'B' => &[
            &[(0, 0), (0, 6)],
            &[(0, 0), (3, 0), (4, 1), (4, 2), (3, 3), (0, 3)],
            &[(3, 3), (4, 4), (4, 5), (3, 6), (0, 6)],
        ],
        'C' => &[&[
            (4, 1),
            (3, 0),
            (1, 0),
            (0, 1),
            (0, 5),
            (1, 6),
            (3, 6),
            (4, 5),
        ]],
        'D' => &[
            &[(0, 0), (0, 6)],
            &[(0, 0), (3, 0), (4, 1), (4, 5), (3, 6), (0, 6)],
        ],
        'E' => &[&[(4, 0), (0, 0), (0, 6), (4, 6)], &[(0, 3), (3, 3)]],
        'F' => &[&[(4, 0), (0, 0), (0, 6)], &[(0, 3), (3, 3)]],
        'G' => &[&[
            (4, 1),
            (3, 0),
            (1, 0),
            (0, 1),
            (0, 5),
            (1, 6),
            (3, 6),
            (4, 5),
            (4, 3),
            (2, 3),
        ]],
        'H' => &[&[(0, 0), (0, 6)], &[(4, 0), (4, 6)], &[(0, 3), (4, 3)]],
        'I' => &[&[(1, 0), (3, 0)], &[(2, 0), (2, 6)], &[(1, 6), (3, 6)]],
        'J' => &[&[(4, 0), (4, 5), (3, 6), (1, 6), (0, 5)]],
        'K' => &[&[(0, 0), (0, 6)], &[(4, 0), (0, 3), (4, 6)]],
        'L' => &[&[(0, 0), (0, 6), (4, 6)]],
        'M' => &[&[(0, 6), (0, 0), (2, 3), (4, 0), (4, 6)]],
        'N' => &[&[(0, 6), (0, 0), (4, 6), (4, 0)]],
        'O' => &[&[
            (1, 0),
            (3, 0),
            (4, 1),
            (4, 5),
            (3, 6),
            (1, 6),
            (0, 5),
            (0, 1),
            (1, 0),
        ]],
        'P' => &[&[(0, 6), (0, 0), (3, 0), (4, 1), (4, 2), (3, 3), (0, 3)]],
        'Q' => &[
            &[
                (1, 0),
                (3, 0),
                (4, 1),
                (4, 5),
                (3, 6),
                (1, 6),
                (0, 5),
                (0, 1),
                (1, 0),
            ],
            &[(2, 4), (4, 6)],
        ],
        'R' => &[
            &[(0, 6), (0, 0), (3, 0), (4, 1), (4, 2), (3, 3), (0, 3)],
            &[(2, 3), (4, 6)],
        ],
        'S' => &[&[
            (4, 1),
            (3, 0),
            (1, 0),
            (0, 1),
            (0, 2),
            (1, 3),
            (3, 3),
            (4, 4),
            (4, 5),
            (3, 6),
            (1, 6),
            (0, 5),
        ]],
        'T' => &[&[(0, 0), (4, 0)], &[(2, 0), (2, 6)]],
        'U' => &[&[(0, 0), (0, 5), (1, 6), (3, 6), (4, 5), (4, 0)]],
        'V' => &[&[(0, 0), (2, 6), (4, 0)]],
        'W' => &[&[(0, 0), (1, 6), (2, 2), (3, 6), (4, 0)]],
        'X' => &[&[(0, 0), (4, 6)], &[(4, 0), (0, 6)]],
        'Y' => &[&[(0, 0), (2, 3), (4, 0)], &[(2, 3), (2, 6)]],
        'Z' => &[&[(0, 0), (4, 0), (0, 6), (4, 6)]],
        '0' => &[
            &[
                (1, 0),
                (3, 0),
                (4, 1),
                (4, 5),
                (3, 6),
                (1, 6),
                (0, 5),
                (0, 1),
                (1, 0),
            ],
            &[(1, 4), (3, 2)],
        ],
        '1' => &[&[(1, 1), (2, 0), (2, 6)], &[(1, 6), (3, 6)]],
        '2' => &[&[(0, 1), (1, 0), (3, 0), (4, 1), (4, 2), (0, 6), (4, 6)]],
        '3' => &[
            &[(0, 1), (1, 0), (3, 0), (4, 1), (4, 2), (3, 3), (1, 3)],
            &[(3, 3), (4, 4), (4, 5), (3, 6), (1, 6), (0, 5)],
        ],
        '4' => &[&[(3, 6), (3, 0), (0, 4), (4, 4)]],
        '5' => &[&[
            (4, 0),
            (0, 0),
            (0, 3),
            (3, 3),
            (4, 4),
            (4, 5),
            (3, 6),
            (1, 6),
            (0, 5),
        ]],
        '6' => &[&[
            (3, 0),
            (1, 0),
            (0, 1),
            (0, 5),
            (1, 6),
            (3, 6),
            (4, 5),
            (4, 4),
            (3, 3),
            (0, 3),
        ]],
        '7' => &[&[(0, 0), (4, 0), (1, 6)]],
        '8' => &[
            &[
                (1, 0),
                (3, 0),
                (4, 1),
                (4, 2),
                (3, 3),
                (1, 3),
                (0, 2),
                (0, 1),
                (1, 0),
            ],
            &[
                (1, 3),
                (0, 4),
                (0, 5),
                (1, 6),
                (3, 6),
                (4, 5),
                (4, 4),
                (3, 3),
            ],
        ],
        '9' => &[&[
            (1, 6),
            (3, 6),
            (4, 5),
            (4, 1),
            (3, 0),
            (1, 0),
            (0, 1),
            (0, 2),
            (1, 3),
            (4, 3),
        ]],
        '.' => &[&[(2, 5), (2, 6)]],
        ',' => &[&[(2, 5), (1, 6)]],
        '-' => &[&[(1, 3), (3, 3)]],
        '_' => &[&[(0, 6), (4, 6)]],
        '/' => &[&[(0, 6), (4, 0)]],
        ':' => &[&[(2, 1), (2, 2)], &[(2, 4), (2, 5)]],
        '%' => &[
            &[(0, 6), (4, 0)],
            &[(0, 0), (1, 0), (1, 1), (0, 1), (0, 0)],
            &[(3, 5), (4, 5), (4, 6), (3, 6), (3, 5)],
        ],
        '+' => &[&[(2, 1), (2, 5)], &[(0, 3), (4, 3)]],
        '#' => &[
            &[(1, 0), (1, 6)],
            &[(3, 0), (3, 6)],
            &[(0, 2), (4, 2)],
            &[(0, 4), (4, 4)],
        ],
        '(' => &[&[(3, 0), (2, 1), (2, 5), (3, 6)]],
        ')' => &[&[(1, 0), (2, 1), (2, 5), (1, 6)]],
        _ => GLYPH_UNKNOWN,
    };
    Some(strokes)
}

// ── Vello append ──

/// The anchored footprint in texture pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct UnderlayRect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width (at least one pixel).
    pub width: f32,
    /// Height (at least one pixel).
    pub height: f32,
}

fn peniko_color(color: [f32; 4]) -> PenikoColor {
    PenikoColor::from_rgba8(
        (color[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (color[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (color[2].clamp(0.0, 1.0) * 255.0).round() as u8,
        (color[3].clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

fn polyline_path(points: &[(f64, f64)]) -> BezPath {
    let mut path = BezPath::new();
    let mut iter = points.iter();
    if let Some(&(x, y)) = iter.next() {
        path.move_to((x, y));
    }
    for &(x, y) in iter {
        path.line_to((x, y));
    }
    path
}

/// Appends one visualization's underlay onto the terminal scene, mapping
/// footprint-space ops into the anchored pixel rect. Stroke widths clamp
/// at one pixel so hairlines survive small footprints.
pub(crate) fn append_viz_underlay(scene: &mut Scene, rect: UnderlayRect, ops: &[VizDrawOp]) {
    let map = |fx: f32, fy: f32| -> (f64, f64) {
        (
            f64::from(rect.x + fx * rect.width),
            f64::from(rect.y + fy * rect.height),
        )
    };
    let stroke_width = |width: f32| f64::from((width * rect.height).max(1.0));
    for op in ops {
        match op {
            VizDrawOp::Fill { min, max, color } => {
                let (x0, y0) = map(min.0, min.1);
                let (x1, y1) = map(max.0, max.1);
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    peniko_color(*color),
                    None,
                    &KurboRect::new(x0, y0, x1, y1),
                );
            }
            VizDrawOp::Polyline {
                points,
                width,
                color,
            } => {
                if points.len() < 2 {
                    continue;
                }
                let mapped: Vec<(f64, f64)> = points.iter().map(|&(fx, fy)| map(fx, fy)).collect();
                scene.stroke(
                    &Stroke::new(stroke_width(*width)),
                    Affine::IDENTITY,
                    peniko_color(*color),
                    None,
                    &polyline_path(&mapped),
                );
            }
            VizDrawOp::Arc {
                center,
                radii,
                start,
                sweep,
                width,
                color,
            } => {
                let (cx, cy) = map(center.0, center.1);
                let arc = KurboArc::new(
                    (cx, cy),
                    (
                        f64::from(radii.0 * rect.width),
                        f64::from(radii.1 * rect.height),
                    ),
                    f64::from(*start),
                    f64::from(*sweep),
                    0.0,
                );
                scene.stroke(
                    &Stroke::new(stroke_width(*width)),
                    Affine::IDENTITY,
                    peniko_color(*color),
                    None,
                    &arc,
                );
            }
            VizDrawOp::Text {
                pos,
                height,
                text,
                color,
                align,
                max_width,
            } => {
                append_text(scene, rect, *pos, *height, text, *color, *align, *max_width);
            }
        }
    }
}

/// Strokes one text run into the scene. The glyph advance is computed in
/// pixels (where the footprint aspect is known) and the run truncates to
/// its width budget.
#[allow(clippy::too_many_arguments)]
fn append_text(
    scene: &mut Scene,
    rect: UnderlayRect,
    pos: (f32, f32),
    height: f32,
    text: &str,
    color: [f32; 4],
    align: TextAlign,
    max_width: Option<f32>,
) {
    let glyph_height = height * rect.height;
    if glyph_height < 3.0 {
        // Below three pixels a stroke glyph is noise; drop the run.
        return;
    }
    let advance = glyph_height * GLYPH_ASPECT;
    let budget_px = max_width.map(|fraction| fraction * rect.width);
    let mut characters: Vec<char> = text.chars().collect();
    if let Some(budget) = budget_px {
        let fits = (budget / advance).floor().max(0.0) as usize;
        characters.truncate(fits);
    }
    if characters.is_empty() {
        return;
    }
    let run_width = advance * characters.len() as f32;
    let start_x = rect.x + pos.0 * rect.width
        - match align {
            TextAlign::Left => 0.0,
            TextAlign::Center => run_width * 0.5,
            TextAlign::Right => run_width,
        };
    let top_y = rect.y + pos.1 * rect.height;
    let stroke = Stroke::new(f64::from((glyph_height * 0.11).max(1.0)));
    let brush = peniko_color(color);
    for (index, character) in characters.iter().enumerate() {
        let Some(strokes) = glyph_strokes(*character) else {
            continue;
        };
        let origin_x = start_x + advance * index as f32;
        for polyline in strokes {
            let mapped: Vec<(f64, f64)> = polyline
                .iter()
                .map(|&(gx, gy)| {
                    (
                        f64::from(origin_x + (f32::from(gx) / 4.0) * glyph_height * 0.62),
                        f64::from(top_y + (f32::from(gy) / 6.0) * glyph_height),
                    )
                })
                .collect();
            scene.stroke(
                &stroke,
                Affine::IDENTITY,
                brush,
                None,
                &polyline_path(&mapped),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::b64url_encode;
    use crate::viz::{decode_viz_payload, viz_child_specs};
    use serde_json::json;

    fn decode(kind: &str, value: serde_json::Value) -> VizPayload {
        decode_viz_payload(kind, &b64url_encode(value.to_string().as_bytes()))
            .expect("payload decodes")
    }

    fn capture() -> serde_json::Value {
        json!({ "source": "test/synthetic", "ts": "2026-07-23T00:00:00Z" })
    }

    #[test]
    fn chart_poses_stay_inside_the_root_footprint() {
        let payload = decode(
            "chart.bar.v1",
            json!({
                "capture": capture(),
                "max": 10.0,
                "items": [
                    { "key": "a", "value": 0.0 },
                    { "key": "b", "value": 5.0 },
                    { "key": "c", "value": 10.0 },
                ],
            }),
        );
        let specs = viz_child_specs(&payload);
        for (index, spec) in specs.iter().enumerate() {
            let (translation, scale) =
                chart_child_pose(spec.slot, index, specs.len(), spec.magnitude);
            for axis in 0..2 {
                let half = scale[axis] * 0.5;
                assert!(
                    translation[axis] - half >= -0.5 - 1e-4
                        && translation[axis] + half <= 0.5 + 1e-4,
                    "bar {index} axis {axis} stays inside the footprint"
                );
            }
            assert!(scale.y > 0.0, "even a zero bar has visible height");
        }
    }

    #[test]
    fn line_marker_and_polyline_agree_on_the_last_point() {
        let payload = decode(
            "chart.line.v1",
            json!({
                "capture": capture(),
                "series": [{
                    "name": "hits",
                    "points": [
                        { "x": 0.0, "y": 1.0 },
                        { "x": 5.0, "y": 3.0 },
                        { "x": 10.0, "y": 2.0 },
                    ],
                }],
            }),
        );
        let specs = viz_child_specs(&payload);
        let (translation, _) = chart_child_pose(specs[0].slot, 0, 1, specs[0].magnitude);
        let VizPayload::ChartLine(ref line) = payload else {
            panic!("expected chart.line.v1");
        };
        let ops = line_ops(line);
        let polyline_end = ops
            .iter()
            .find_map(|op| match op {
                VizDrawOp::Polyline { points, width, .. } if *width == SERIES_WIDTH => {
                    points.last().copied()
                }
                _ => None,
            })
            .expect("the series lowered to a polyline");
        let (rx, ry) = foot_to_root(polyline_end.0, polyline_end.1);
        assert!(
            (translation.x - rx).abs() < 1e-4 && (translation.y - ry).abs() < 1e-4,
            "marker {translation:?} sits on the polyline end ({rx}, {ry})"
        );
    }

    #[test]
    fn gauge_needle_sits_on_the_dial_arc() {
        let payload = decode(
            "chart.gauge.v1",
            json!({
                "capture": capture(),
                "items": [{ "key": "w", "value": 0.5 }],
            }),
        );
        let specs = viz_child_specs(&payload);
        let (translation, _) = chart_child_pose(specs[0].slot, 0, 1, specs[0].magnitude);
        // fraction 0.5 points straight up from the dial center.
        let (cx, cy, _, ry) = gauge_dial_geometry(0, 1);
        let (expected_x, expected_y) = foot_to_root(cx, cy - ry);
        assert!(
            (translation.x - expected_x).abs() < 1e-4 && (translation.y - expected_y).abs() < 1e-4,
            "a half dial points at the arc top"
        );
    }

    #[test]
    fn timeline_spans_share_the_window_with_the_axis_labels() {
        let payload = decode(
            "timeline.v1",
            json!({
                "capture": capture(),
                "lanes": [{
                    "name": "layer",
                    "events": [
                        { "id": "a", "t": 0.0, "dur": 10.0 },
                        { "id": "b", "t": 90.0, "dur": 10.0 },
                    ],
                }],
            }),
        );
        let specs = viz_child_specs(&payload);
        let (first, _) = chart_child_pose(specs[0].slot, 0, specs.len(), 1.0);
        let (last, _) = chart_child_pose(specs[1].slot, 1, specs.len(), 1.0);
        assert!(first.x < last.x, "events order along the window");
        let VizPayload::Timeline(ref timeline) = payload else {
            panic!("expected timeline.v1");
        };
        let ops = timeline_ops(timeline);
        let labels: Vec<&str> = ops
            .iter()
            .filter_map(|op| match op {
                VizDrawOp::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(labels.contains(&"0"), "window start labels the axis");
        assert!(labels.contains(&"100"), "window end labels the axis");
        assert!(labels.contains(&"layer"), "lane names label the tracks");
    }

    #[test]
    fn underlay_ops_cover_chart_kinds_only() {
        let grid = decode(
            "ps.v1",
            json!({ "capture": capture(), "items": [{ "pid": 1, "name": "a" }] }),
        );
        assert!(
            viz_underlay_ops(&grid).is_none(),
            "grid kinds keep M3.5's look"
        );
        let bar = decode("chart.bar.v1", json!({ "capture": capture(), "items": [] }));
        let ops = viz_underlay_ops(&bar).expect("chart kinds draw an underlay");
        assert!(
            matches!(ops.first(), Some(VizDrawOp::Fill { color, .. }) if *color == SCRIM),
            "the underlay starts with the scrim"
        );
    }

    #[test]
    fn every_charset_glyph_stays_on_the_grid() {
        let charset = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 .,-_/:%+#()";
        for character in charset.chars() {
            let Some(strokes) = glyph_strokes(character) else {
                assert_eq!(character, ' ', "only space draws nothing");
                continue;
            };
            assert!(!strokes.is_empty(), "{character:?} has strokes");
            for polyline in strokes {
                assert!(polyline.len() >= 2, "{character:?} strokes are drawable");
                for &(x, y) in polyline.iter() {
                    assert!(
                        (0..=4).contains(&x) && (0..=6).contains(&y),
                        "{character:?} stroke ({x},{y}) stays on the glyph grid"
                    );
                }
            }
        }
        // The fallback box renders for anything outside the charset, and
        // lowercase maps onto uppercase.
        assert_eq!(glyph_strokes('~'), Some(GLYPH_UNKNOWN));
        assert_eq!(glyph_strokes('k'), glyph_strokes('K'));
    }

    /// CPU-side rasterization smoke: every chart kind's underlay appends
    /// real geometry into a vello scene without panicking — including
    /// stroke-font text, arcs, and a degenerate one-pixel rect (where
    /// text drops below its three-pixel floor rather than dividing by
    /// nothing).
    #[test]
    fn underlays_append_real_vello_geometry() {
        let payloads = [
            decode(
                "chart.bar.v1",
                json!({
                    "capture": capture(),
                    "title": "queue depth", "unit": "msgs", "max": 10.0,
                    "items": [{ "key": "a", "label": "Ingest", "value": 3.0 }],
                }),
            ),
            decode(
                "chart.line.v1",
                json!({
                    "capture": capture(),
                    "title": "hit rate",
                    "series": [{ "name": "l1", "points": [
                        { "x": 0.0, "y": 1.0 }, { "x": 1.0, "y": 2.0 },
                    ] }],
                }),
            ),
            decode(
                "chart.gauge.v1",
                json!({
                    "capture": capture(),
                    "items": [{ "key": "w", "value": 0.62, "unit": "%" }],
                }),
            ),
            decode(
                "timeline.v1",
                json!({
                    "capture": capture(),
                    "title": "layers",
                    "lanes": [{ "name": "layer-0", "events": [
                        { "id": "e1", "t": 0.0, "dur": 1.0 },
                    ] }],
                }),
            ),
        ];
        for payload in &payloads {
            let ops = viz_underlay_ops(payload).expect("chart kinds draw");
            let mut scene = Scene::new();
            append_viz_underlay(
                &mut scene,
                UnderlayRect {
                    x: 12.0,
                    y: 8.0,
                    width: 480.0,
                    height: 160.0,
                },
                &ops,
            );
            let encoding = scene.encoding();
            assert!(
                encoding.n_paths > 1,
                "{}: underlay encodes paths (got {})",
                payload.kind(),
                encoding.n_paths
            );
            // The degenerate rect must not panic; text drops, paths clamp
            // to one-pixel strokes.
            let mut tiny = Scene::new();
            append_viz_underlay(
                &mut tiny,
                UnderlayRect {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0,
                    height: 1.0,
                },
                &ops,
            );
        }
    }

    #[test]
    fn values_format_compactly_and_instants_read_as_utc() {
        assert_eq!(format_value(0.0), "0");
        assert_eq!(format_value(0.62), "0.62");
        assert_eq!(format_value(42.0), "42");
        assert_eq!(format_value(1536.0), "1.5K");
        assert_eq!(format_value(2_500_000.0), "2.5M");
        assert_eq!(format_value(3.2e9), "3.2G");
        assert_eq!(format_axis_instant(45.0), "45");
        // 2026-07-23 17:00:05 UTC.
        assert_eq!(format_axis_instant(1_784_912_405.0), "17:00:05");
    }
}
