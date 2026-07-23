//! The `viz.*` payload schemas and decoder — the std-only wire half of the
//! visualization family.
//!
//! This module is dependency-free (std + serde only) so authoring tools can
//! include it verbatim the way `tools/ratty-ai` and `tools/silk` include
//! `osc.rs` and `query.rs`: `silk validate` re-runs the terminal's *real*
//! `decode_viz_payload`, and a payload it accepts can never be rejected by
//! the terminal (or vice versa). Everything bevy-flavored — the registry,
//! components, palette, lowering, and systems — stays in `crate::viz`,
//! which re-exports this module wholesale.
//!
//! Schema conventions (shared by every kind):
//!
//! - `capture` provenance is **required** — ratty never implies liveness it
//!   was not given.
//! - Unknown JSON *fields* are ignored (serde's default) so schemas evolve
//!   additively; unknown enum *values* (a state tag, an entry kind) reject
//!   `bad-payload` — a closed vocabulary is part of the schema.
//! - Identity fields (keys, names, ids) are required; magnitude fields
//!   default — except where a default would claim knowledge the emitter
//!   never sent (`net.v1 up`, a gauge's `value`), which stay required.
//! - Every numeric magnitude must be finite; `NaN`/`±inf` (including JSON
//!   numbers overflowing the target float) reject `bad-payload` rather
//!   than poisoning child transforms downstream.
//! - Every size limit is hard-rejected with `too-large`. Nesting depth is
//!   bounded by `serde_json`'s built-in recursion limit (128).

use serde::Deserialize;

use crate::osc::{
    MAX_VIZ_ITEMS_PER_SNAPSHOT, MAX_VIZ_LABEL_BYTES, MAX_VIZ_PAYLOAD_BYTES,
    MAX_VIZ_POINTS_PER_SERIES, MAX_VIZ_POINTS_PER_SNAPSHOT, MAX_VIZ_SERIES_PER_SNAPSHOT,
};
use crate::query::{B64DecodeError, b64url_decode, codes};

/// The registered, versioned payload kinds this build decodes and renders.
/// The version is part of the name; anything else rejects `bad-kind`.
pub const REGISTERED_VIZ_KINDS: &[&str] = &[
    "ps.v1",
    "fs.v1",
    "git.v1",
    "net.v1",
    "chart.bar.v1",
    "chart.line.v1",
    "chart.gauge.v1",
    "timeline.v1",
];

// ── Shared schema pieces ──

/// Capture provenance carried by every snapshot. Required: ratty never
/// implies liveness it was not given — a transmission shipping synthetic
/// data declares itself here, and a collector stamps its real source.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct VizCapture {
    /// Where the data came from (e.g. `ratty-ai ps/sysinfo darwin`, or
    /// `authored` for transmission-shipped data).
    pub source: String,
    /// When it was captured (RFC 3339 recommended; opaque on the wire).
    pub ts: String,
}

/// The closed state vocabulary the chart-family kinds color by. These are
/// the semantic palette slots by their wire names; an unknown state rejects
/// `bad-payload` (a closed vocabulary is part of the schema — an emitter
/// learns about a typo from the ack, not from a silently gray chart).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VizStateTag {
    /// Actively doing work / the current selection.
    Active,
    /// Alive but idle.
    Idle,
    /// A state worth attention.
    Alert,
    /// A container of other things.
    Container,
    /// No particular state.
    #[default]
    Neutral,
}

// ── Telemetry kinds (M3.5) ──

/// `ps.v1`: a process snapshot.
#[derive(Debug, Clone, Deserialize)]
pub struct PsV1 {
    /// Capture provenance.
    pub capture: VizCapture,
    /// Process items, keyed by pid.
    #[serde(default)]
    pub items: Vec<PsItem>,
}

/// One `ps.v1` process entry.
#[derive(Debug, Clone, Deserialize)]
pub struct PsItem {
    /// Process id — the stable domain key `viz.effect` targets.
    pub pid: u32,
    /// Process name.
    pub name: String,
    /// CPU usage percentage.
    #[serde(default)]
    pub cpu: f32,
    /// Resident memory in bytes.
    #[serde(default)]
    pub mem: u64,
    /// Scheduler state tag (e.g. `running`, `sleeping`).
    #[serde(default)]
    pub state: String,
}

/// `fs.v1`: a bounded filesystem-walk snapshot.
#[derive(Debug, Clone, Deserialize)]
pub struct FsV1 {
    /// Capture provenance.
    pub capture: VizCapture,
    /// The walked root path.
    pub root: String,
    /// Walk entries, keyed by path.
    #[serde(default)]
    pub items: Vec<FsItem>,
}

/// One `fs.v1` walk entry.
#[derive(Debug, Clone, Deserialize)]
pub struct FsItem {
    /// Path relative to the root — the stable domain key.
    pub path: String,
    /// Whether the entry is a file or a directory.
    pub kind: FsEntryKind,
    /// Size in bytes.
    #[serde(default)]
    pub size: u64,
    /// Depth below the walked root.
    #[serde(default)]
    pub depth: u8,
}

/// The `fs.v1` entry kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsEntryKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
}

/// `git.v1`: a repository snapshot.
#[derive(Debug, Clone, Deserialize)]
pub struct GitV1 {
    /// Capture provenance.
    pub capture: VizCapture,
    /// Repository path or name.
    pub repo: String,
    /// Branches, keyed by name.
    #[serde(default)]
    pub branches: Vec<GitBranchInfo>,
    /// Working-tree status counts.
    #[serde(default)]
    pub status: GitStatusCounts,
    /// Commits ahead of upstream.
    #[serde(default)]
    pub ahead: u32,
    /// Commits behind upstream.
    #[serde(default)]
    pub behind: u32,
}

/// One `git.v1` branch entry.
#[derive(Debug, Clone, Deserialize)]
pub struct GitBranchInfo {
    /// Branch name — the stable domain key.
    pub name: String,
    /// Whether this is the checked-out branch.
    #[serde(default)]
    pub current: bool,
}

/// `git.v1` working-tree status counts.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct GitStatusCounts {
    /// Staged changes.
    #[serde(default)]
    pub staged: u32,
    /// Unstaged changes.
    #[serde(default)]
    pub unstaged: u32,
    /// Untracked files.
    #[serde(default)]
    pub untracked: u32,
}

/// `net.v1`: a network-interface counter snapshot. Interfaces, not
/// sockets — a portable, honest v1; per-connection detail can arrive
/// additively as a future kind.
#[derive(Debug, Clone, Deserialize)]
pub struct NetV1 {
    /// Capture provenance.
    pub capture: VizCapture,
    /// Interface counters, keyed by interface name.
    #[serde(default)]
    pub items: Vec<NetInterface>,
}

/// One `net.v1` interface entry.
#[derive(Debug, Clone, Deserialize)]
pub struct NetInterface {
    /// Interface name — the stable domain key.
    pub iface: String,
    /// Received bytes.
    #[serde(default)]
    pub rx_bytes: u64,
    /// Transmitted bytes.
    #[serde(default)]
    pub tx_bytes: u64,
    /// Whether the interface is up. Required — defaulting a link state
    /// would claim knowledge the emitter did not send.
    pub up: bool,
}

// ── Chart kinds (M3.6) ──
//
// Numeric fields are f64 on the wire: timeline instants at unix-epoch
// scale lose whole minutes to an f32 mantissa. Lowering normalizes
// window-relative values, where f32 is plenty.

/// `chart.bar.v1`: labeled category bars against an optional fixed axis.
#[derive(Debug, Clone, Deserialize)]
pub struct ChartBarV1 {
    /// Capture provenance.
    pub capture: VizCapture,
    /// Chart title, drawn above the plot area.
    #[serde(default)]
    pub title: Option<String>,
    /// Unit suffix for the axis label (e.g. `msgs`, `%`).
    #[serde(default)]
    pub unit: Option<String>,
    /// Fixed axis maximum. When present, bar heights are `value / max` and
    /// survive refreshes without re-normalizing (a stable watch axis); must
    /// be finite and positive. When absent, bars normalize within the
    /// snapshot.
    #[serde(default)]
    pub max: Option<f64>,
    /// Bars, keyed by `key`.
    #[serde(default)]
    pub items: Vec<ChartBarItem>,
}

/// One `chart.bar.v1` bar.
#[derive(Debug, Clone, Deserialize)]
pub struct ChartBarItem {
    /// The stable domain key `viz.effect` targets.
    pub key: String,
    /// Display label; defaults to the key.
    #[serde(default)]
    pub label: Option<String>,
    /// Bar magnitude. Must be finite and non-negative — signed bars are a
    /// future kind, not a silent render surprise.
    #[serde(default)]
    pub value: f64,
    /// Semantic state coloring the bar.
    #[serde(default)]
    pub state: VizStateTag,
}

/// `chart.line.v1`: one or more point series over a shared x-domain.
#[derive(Debug, Clone, Deserialize)]
pub struct ChartLineV1 {
    /// Capture provenance.
    pub capture: VizCapture,
    /// Chart title, drawn above the plot area.
    #[serde(default)]
    pub title: Option<String>,
    /// Fixed y-axis minimum; supplied together with `y_max` or not at all.
    #[serde(default)]
    pub y_min: Option<f64>,
    /// Fixed y-axis maximum; supplied together with `y_min` or not at all.
    #[serde(default)]
    pub y_max: Option<f64>,
    /// Series, keyed by name — the stable domain key `viz.effect` targets.
    #[serde(default)]
    pub series: Vec<ChartSeries>,
}

/// One `chart.line.v1` series.
#[derive(Debug, Clone, Deserialize)]
pub struct ChartSeries {
    /// Series name — the stable domain key.
    pub name: String,
    /// Semantic state coloring the series; when absent, series color by
    /// position from the non-alert palette so neighbors stay distinct.
    #[serde(default)]
    pub state: Option<VizStateTag>,
    /// Points, rendered as a polyline in the order given.
    #[serde(default)]
    pub points: Vec<ChartPoint>,
}

/// One `chart.line.v1` point.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ChartPoint {
    /// X coordinate (any unit; the snapshot's x-range normalizes it).
    pub x: f64,
    /// Y coordinate.
    pub y: f64,
}

/// `chart.gauge.v1`: one or more value dials.
#[derive(Debug, Clone, Deserialize)]
pub struct ChartGaugeV1 {
    /// Capture provenance.
    pub capture: VizCapture,
    /// Gauges, keyed by `key`.
    #[serde(default)]
    pub items: Vec<ChartGaugeItem>,
}

/// One `chart.gauge.v1` dial.
#[derive(Debug, Clone, Deserialize)]
pub struct ChartGaugeItem {
    /// The stable domain key `viz.effect` targets.
    pub key: String,
    /// Display label; defaults to the key.
    #[serde(default)]
    pub label: Option<String>,
    /// The gauge value. Required — a defaulted reading would claim
    /// knowledge the emitter never sent. Values outside `min..=max` render
    /// a clamped dial with the raw value printed honestly.
    pub value: f64,
    /// Dial minimum (defaults 0).
    #[serde(default)]
    pub min: f64,
    /// Dial maximum (defaults 1). Must exceed `min`.
    #[serde(default = "default_gauge_max")]
    pub max: f64,
    /// Unit suffix printed after the value (e.g. `%`).
    #[serde(default)]
    pub unit: Option<String>,
    /// Semantic state coloring the dial.
    #[serde(default)]
    pub state: VizStateTag,
}

fn default_gauge_max() -> f64 {
    1.0
}

/// `timeline.v1`: instant and span events on named lanes over a shared
/// time window.
#[derive(Debug, Clone, Deserialize)]
pub struct TimelineV1 {
    /// Capture provenance.
    pub capture: VizCapture,
    /// Timeline title, drawn above the plot area.
    #[serde(default)]
    pub title: Option<String>,
    /// Fixed window start; supplied together with `t1` or not at all.
    /// When absent, the window derives from the events.
    #[serde(default)]
    pub t0: Option<f64>,
    /// Fixed window end; supplied together with `t0` or not at all.
    #[serde(default)]
    pub t1: Option<f64>,
    /// Lanes, in display order top to bottom.
    #[serde(default)]
    pub lanes: Vec<TimelineLane>,
}

/// One `timeline.v1` lane.
#[derive(Debug, Clone, Deserialize)]
pub struct TimelineLane {
    /// Lane name, drawn beside the track.
    pub name: String,
    /// Events on this lane.
    #[serde(default)]
    pub events: Vec<TimelineEvent>,
}

/// One `timeline.v1` event.
#[derive(Debug, Clone, Deserialize)]
pub struct TimelineEvent {
    /// Event id — the stable domain key `viz.effect` targets, unique
    /// across the whole snapshot.
    pub id: String,
    /// Display label.
    #[serde(default)]
    pub label: Option<String>,
    /// Event instant (any unit shared with the window; typically unix
    /// seconds).
    pub t: f64,
    /// Event duration; `0` (the default) is an instant event. Must be
    /// finite and non-negative.
    #[serde(default)]
    pub dur: f64,
    /// Semantic state coloring the event.
    #[serde(default)]
    pub state: VizStateTag,
}

// ── Decoded payload ──

/// A decoded, validated `viz.set` payload.
#[derive(Debug, Clone)]
pub enum VizPayload {
    /// A `ps.v1` process snapshot.
    Ps(PsV1),
    /// An `fs.v1` filesystem snapshot.
    Fs(FsV1),
    /// A `git.v1` repository snapshot.
    Git(GitV1),
    /// A `net.v1` interface-counter snapshot.
    Net(NetV1),
    /// A `chart.bar.v1` category-bar chart.
    ChartBar(ChartBarV1),
    /// A `chart.line.v1` series chart.
    ChartLine(ChartLineV1),
    /// A `chart.gauge.v1` dial set.
    ChartGauge(ChartGaugeV1),
    /// A `timeline.v1` lane-event timeline.
    Timeline(TimelineV1),
}

impl VizPayload {
    /// The registered kind name this payload decoded as.
    pub fn kind(&self) -> &'static str {
        match self {
            VizPayload::Ps(_) => "ps.v1",
            VizPayload::Fs(_) => "fs.v1",
            VizPayload::Git(_) => "git.v1",
            VizPayload::Net(_) => "net.v1",
            VizPayload::ChartBar(_) => "chart.bar.v1",
            VizPayload::ChartLine(_) => "chart.line.v1",
            VizPayload::ChartGauge(_) => "chart.gauge.v1",
            VizPayload::Timeline(_) => "timeline.v1",
        }
    }

    /// The capture provenance every payload carries.
    pub fn capture(&self) -> &VizCapture {
        match self {
            VizPayload::Ps(payload) => &payload.capture,
            VizPayload::Fs(payload) => &payload.capture,
            VizPayload::Git(payload) => &payload.capture,
            VizPayload::Net(payload) => &payload.capture,
            VizPayload::ChartBar(payload) => &payload.capture,
            VizPayload::ChartLine(payload) => &payload.capture,
            VizPayload::ChartGauge(payload) => &payload.capture,
            VizPayload::Timeline(payload) => &payload.capture,
        }
    }

    /// Number of keyed items in the snapshot — the things `viz.effect` can
    /// target (`git` counts branches, `chart.line` counts series,
    /// `timeline` counts events across every lane).
    pub fn item_count(&self) -> usize {
        match self {
            VizPayload::Ps(payload) => payload.items.len(),
            VizPayload::Fs(payload) => payload.items.len(),
            VizPayload::Git(payload) => payload.branches.len(),
            VizPayload::Net(payload) => payload.items.len(),
            VizPayload::ChartBar(payload) => payload.items.len(),
            VizPayload::ChartLine(payload) => payload.series.len(),
            VizPayload::ChartGauge(payload) => payload.items.len(),
            VizPayload::Timeline(payload) => {
                payload.lanes.iter().map(|lane| lane.events.len()).sum()
            }
        }
    }

    /// Enforces the constraints the schema types cannot express: item
    /// counts, label byte lengths, finite magnitudes, and ordered ranges.
    fn validate(&self) -> Result<(), VizDecodeError> {
        let capture = self.capture();
        check_label("capture.source", &capture.source)?;
        check_label("capture.ts", &capture.ts)?;
        check_items(self.item_count())?;
        match self {
            VizPayload::Ps(payload) => {
                for item in &payload.items {
                    check_label("name", &item.name)?;
                    check_label("state", &item.state)?;
                    // A hostile `cpu` such as `3.5e38` (valid JSON, above
                    // f32::MAX) decodes to a non-finite f32; left unchecked
                    // it produces NaN magnitudes that poison child
                    // transforms and every later effect animation. The
                    // terminal does not trust the emitter — reject here
                    // rather than sanitize silently.
                    if !item.cpu.is_finite() {
                        return Err(VizDecodeError {
                            code: codes::BAD_PAYLOAD,
                            message: format!(
                                "pid {} carries a non-finite cpu; magnitudes must be finite",
                                item.pid
                            ),
                        });
                    }
                }
            }
            VizPayload::Fs(payload) => {
                check_label("root", &payload.root)?;
                for item in &payload.items {
                    check_label("path", &item.path)?;
                }
            }
            VizPayload::Git(payload) => {
                check_label("repo", &payload.repo)?;
                for branch in &payload.branches {
                    check_label("branch name", &branch.name)?;
                }
            }
            VizPayload::Net(payload) => {
                for item in &payload.items {
                    check_label("iface", &item.iface)?;
                }
            }
            VizPayload::ChartBar(payload) => {
                check_opt_label("title", payload.title.as_deref())?;
                check_opt_label("unit", payload.unit.as_deref())?;
                if let Some(max) = payload.max {
                    check_finite("max", max)?;
                    if max <= 0.0 {
                        return Err(bad_payload("max must be positive"));
                    }
                }
                for item in &payload.items {
                    check_label("key", &item.key)?;
                    check_opt_label("label", item.label.as_deref())?;
                    check_finite("value", item.value)?;
                    if item.value < 0.0 {
                        return Err(bad_payload(format!(
                            "bar '{}' carries a negative value; chart.bar.v1 is non-negative",
                            item.key
                        )));
                    }
                }
            }
            VizPayload::ChartLine(payload) => {
                check_opt_label("title", payload.title.as_deref())?;
                check_axis_pair("y_min", payload.y_min, "y_max", payload.y_max)?;
                if payload.series.len() > MAX_VIZ_SERIES_PER_SNAPSHOT {
                    return Err(too_large(format!(
                        "snapshot exceeds {MAX_VIZ_SERIES_PER_SNAPSHOT} series"
                    )));
                }
                let mut total_points = 0_usize;
                for series in &payload.series {
                    check_label("series name", &series.name)?;
                    if series.points.len() > MAX_VIZ_POINTS_PER_SERIES {
                        return Err(too_large(format!(
                            "series '{}' exceeds {MAX_VIZ_POINTS_PER_SERIES} points",
                            series.name
                        )));
                    }
                    total_points += series.points.len();
                    for point in &series.points {
                        check_finite("x", point.x)?;
                        check_finite("y", point.y)?;
                    }
                }
                if total_points > MAX_VIZ_POINTS_PER_SNAPSHOT {
                    return Err(too_large(format!(
                        "snapshot exceeds {MAX_VIZ_POINTS_PER_SNAPSHOT} points"
                    )));
                }
            }
            VizPayload::ChartGauge(payload) => {
                for item in &payload.items {
                    check_label("key", &item.key)?;
                    check_opt_label("label", item.label.as_deref())?;
                    check_opt_label("unit", item.unit.as_deref())?;
                    check_finite("value", item.value)?;
                    check_finite("min", item.min)?;
                    check_finite("max", item.max)?;
                    if item.min >= item.max {
                        return Err(bad_payload(format!(
                            "gauge '{}' needs min < max",
                            item.key
                        )));
                    }
                    if !(item.max - item.min).is_finite() {
                        return Err(bad_payload(format!(
                            "gauge '{}': max - min overflows; the range must be finite",
                            item.key
                        )));
                    }
                }
            }
            VizPayload::Timeline(payload) => {
                check_opt_label("title", payload.title.as_deref())?;
                check_axis_pair("t0", payload.t0, "t1", payload.t1)?;
                if payload.lanes.len() > MAX_VIZ_SERIES_PER_SNAPSHOT {
                    return Err(too_large(format!(
                        "snapshot exceeds {MAX_VIZ_SERIES_PER_SNAPSHOT} lanes"
                    )));
                }
                for lane in &payload.lanes {
                    check_label("lane name", &lane.name)?;
                    for event in &lane.events {
                        check_label("event id", &event.id)?;
                        check_opt_label("event label", event.label.as_deref())?;
                        check_finite("t", event.t)?;
                        check_finite("dur", event.dur)?;
                        if event.dur < 0.0 {
                            return Err(bad_payload(format!(
                                "event '{}' carries a negative dur",
                                event.id
                            )));
                        }
                        // Two finite values whose sum overflows would put
                        // infinity into the derived window and NaN into
                        // every normalized span downstream.
                        if !(event.t + event.dur).is_finite() {
                            return Err(bad_payload(format!(
                                "event '{}': t + dur overflows; the span end must be finite",
                                event.id
                            )));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn bad_payload(message: impl Into<String>) -> VizDecodeError {
    VizDecodeError {
        code: codes::BAD_PAYLOAD,
        message: message.into(),
    }
}

fn too_large(message: impl Into<String>) -> VizDecodeError {
    VizDecodeError {
        code: codes::TOO_LARGE,
        message: message.into(),
    }
}

fn check_label(field: &'static str, value: &str) -> Result<(), VizDecodeError> {
    if value.len() > MAX_VIZ_LABEL_BYTES {
        return Err(too_large(format!(
            "{field} exceeds {MAX_VIZ_LABEL_BYTES} bytes"
        )));
    }
    Ok(())
}

fn check_opt_label(field: &'static str, value: Option<&str>) -> Result<(), VizDecodeError> {
    match value {
        Some(value) => check_label(field, value),
        None => Ok(()),
    }
}

fn check_items(count: usize) -> Result<(), VizDecodeError> {
    if count > MAX_VIZ_ITEMS_PER_SNAPSHOT {
        return Err(too_large(format!(
            "snapshot exceeds {MAX_VIZ_ITEMS_PER_SNAPSHOT} items"
        )));
    }
    Ok(())
}

fn check_finite(field: &'static str, value: f64) -> Result<(), VizDecodeError> {
    if !value.is_finite() {
        return Err(bad_payload(format!(
            "{field} is non-finite; magnitudes must be finite"
        )));
    }
    Ok(())
}

/// Validates an optional fixed-range pair: both present or both absent,
/// finite, and strictly ordered.
fn check_axis_pair(
    low_name: &'static str,
    low: Option<f64>,
    high_name: &'static str,
    high: Option<f64>,
) -> Result<(), VizDecodeError> {
    match (low, high) {
        (None, None) => Ok(()),
        (Some(low_value), Some(high_value)) => {
            check_finite(low_name, low_value)?;
            check_finite(high_name, high_value)?;
            if low_value >= high_value {
                return Err(bad_payload(format!("{low_name} must be below {high_name}")));
            }
            // Two finite extremes whose span overflows would divide by
            // infinity downstream.
            if !(high_value - low_value).is_finite() {
                return Err(bad_payload(format!(
                    "{high_name} - {low_name} overflows; the range must be finite"
                )));
            }
            Ok(())
        }
        _ => Err(bad_payload(format!(
            "{low_name}= and {high_name}= are supplied together or not at all"
        ))),
    }
}

/// A rejected viz payload: the stable append-only reject code
/// (`bad-kind`, `bad-payload`, or `too-large`) plus a human-readable
/// detail for the diagnostics ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VizDecodeError {
    /// The reject code (see [`crate::query::codes`]).
    pub code: &'static str,
    /// Detail for `state.errors`; may embed wire-derived text (the
    /// diagnostics ring truncates at its own storage boundary).
    pub message: String,
}

fn parse_json<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, VizDecodeError> {
    serde_json::from_slice(bytes).map_err(|error| VizDecodeError {
        code: codes::BAD_PAYLOAD,
        message: format!("payload does not match the kind schema: {error}"),
    })
}

/// Decodes and validates a `viz.set` payload: registered kind, unpadded
/// base64url under [`MAX_VIZ_PAYLOAD_BYTES`], schema-conforming JSON,
/// bounded item counts and label lengths, finite magnitudes.
///
/// # Errors
///
/// Returns a [`VizDecodeError`] carrying the stable reject code:
/// `bad-kind` for an unregistered kind (an unknown schema version is the
/// same case), `too-large` for any exceeded size limit, `bad-payload` for
/// malformed base64url or schema-violating JSON.
pub fn decode_viz_payload(kind: &str, data: &str) -> Result<VizPayload, VizDecodeError> {
    // Kind gates first so an oversized payload of an unknown kind still
    // reports the more actionable error.
    if !REGISTERED_VIZ_KINDS.contains(&kind) {
        return Err(VizDecodeError {
            code: codes::BAD_KIND,
            message: format!("unknown viz kind '{kind}' (registered: {REGISTERED_VIZ_KINDS:?})"),
        });
    }
    let bytes = b64url_decode(data, MAX_VIZ_PAYLOAD_BYTES).map_err(|error| match error {
        B64DecodeError::TooLarge => too_large(format!(
            "decoded payload exceeds {MAX_VIZ_PAYLOAD_BYTES} bytes"
        )),
        B64DecodeError::BadChar | B64DecodeError::BadLength => {
            bad_payload("data= is not unpadded base64url")
        }
    })?;
    let payload = match kind {
        "ps.v1" => VizPayload::Ps(parse_json(&bytes)?),
        "fs.v1" => VizPayload::Fs(parse_json(&bytes)?),
        "git.v1" => VizPayload::Git(parse_json(&bytes)?),
        "net.v1" => VizPayload::Net(parse_json(&bytes)?),
        "chart.bar.v1" => VizPayload::ChartBar(parse_json(&bytes)?),
        "chart.line.v1" => VizPayload::ChartLine(parse_json(&bytes)?),
        "chart.gauge.v1" => VizPayload::ChartGauge(parse_json(&bytes)?),
        "timeline.v1" => VizPayload::Timeline(parse_json(&bytes)?),
        other => {
            // Unreachable while REGISTERED_VIZ_KINDS and this match move
            // together; kept as an honest error rather than a panic.
            return Err(VizDecodeError {
                code: codes::BAD_KIND,
                message: format!("kind '{other}' is registered but has no decoder"),
            });
        }
    };
    payload.validate()?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::b64url_encode;
    use serde_json::json;

    fn encode(value: serde_json::Value) -> String {
        b64url_encode(value.to_string().as_bytes())
    }

    fn capture() -> serde_json::Value {
        json!({ "source": "test/synthetic", "ts": "2026-07-23T00:00:00Z" })
    }

    #[test]
    fn chart_limits_are_pinned() {
        assert_eq!(MAX_VIZ_SERIES_PER_SNAPSHOT, 8);
        assert_eq!(MAX_VIZ_POINTS_PER_SERIES, 256);
        assert_eq!(MAX_VIZ_POINTS_PER_SNAPSHOT, 1024);
    }

    #[test]
    fn chart_bar_decodes_defaults_and_counts_items() {
        let payload = json!({
            "capture": capture(),
            "title": "queue depth",
            "unit": "msgs",
            "max": 100.0,
            "items": [
                { "key": "ingest", "value": 42.5, "state": "active" },
                { "key": "publish", "label": "Publish", "future": true },
            ],
        });
        let decoded = decode_viz_payload("chart.bar.v1", &encode(payload)).expect("decodes");
        assert_eq!(decoded.kind(), "chart.bar.v1");
        assert_eq!(decoded.item_count(), 2);
        let VizPayload::ChartBar(bar) = decoded else {
            panic!("expected chart.bar.v1");
        };
        assert_eq!(bar.items[1].value, 0.0, "value defaults");
        assert_eq!(bar.items[1].state, VizStateTag::Neutral, "state defaults");
        assert_eq!(bar.items[1].label.as_deref(), Some("Publish"));
    }

    #[test]
    fn chart_bar_rejects_negative_values_and_bad_axis() {
        let negative = json!({
            "capture": capture(),
            "items": [{ "key": "a", "value": -1.0 }],
        });
        let error = decode_viz_payload("chart.bar.v1", &encode(negative)).expect_err("negative");
        assert_eq!(error.code, codes::BAD_PAYLOAD);

        for max in [0.0, -3.0] {
            let bad_max = json!({ "capture": capture(), "max": max });
            let error = decode_viz_payload("chart.bar.v1", &encode(bad_max))
                .expect_err("non-positive max");
            assert_eq!(error.code, codes::BAD_PAYLOAD);
        }
    }

    #[test]
    fn chart_state_vocabulary_is_closed() {
        let payload = json!({
            "capture": capture(),
            "items": [{ "key": "a", "state": "exploding" }],
        });
        let error = decode_viz_payload("chart.bar.v1", &encode(payload)).expect_err("unknown tag");
        assert_eq!(error.code, codes::BAD_PAYLOAD);
    }

    #[test]
    fn non_finite_chart_magnitudes_reject() {
        // 1e400 overflows f64 to +inf inside serde_json; every chart-family
        // magnitude must reject it rather than poison the lowering.
        let bar = json!({ "capture": capture(), "items": [{ "key": "a", "value": 1e308 }] });
        assert!(decode_viz_payload("chart.bar.v1", &encode(bar)).is_ok(), "finite f64 max is fine");
        for raw in [
            format!(
                r#"{{ "capture": {}, "items": [{{ "key": "a", "value": 1e400 }}] }}"#,
                capture()
            ),
            format!(
                r#"{{ "capture": {}, "max": 1e400, "items": [] }}"#,
                capture()
            ),
        ] {
            let error = decode_viz_payload("chart.bar.v1", &b64url_encode(raw.as_bytes()))
                .expect_err("overflowing magnitude");
            assert_eq!(error.code, codes::BAD_PAYLOAD, "{raw}");
        }
    }

    #[test]
    fn chart_line_series_and_point_caps_reject_too_large() {
        let series: Vec<serde_json::Value> = (0..(MAX_VIZ_SERIES_PER_SNAPSHOT + 1))
            .map(|index| json!({ "name": format!("s{index}") }))
            .collect();
        let error = decode_viz_payload(
            "chart.line.v1",
            &encode(json!({ "capture": capture(), "series": series })),
        )
        .expect_err("over the series cap");
        assert_eq!(error.code, codes::TOO_LARGE);

        let points: Vec<serde_json::Value> = (0..(MAX_VIZ_POINTS_PER_SERIES + 1))
            .map(|index| json!({ "x": index as f64, "y": 0.0 }))
            .collect();
        let error = decode_viz_payload(
            "chart.line.v1",
            &encode(json!({
                "capture": capture(),
                "series": [{ "name": "s", "points": points }],
            })),
        )
        .expect_err("over the per-series cap");
        assert_eq!(error.code, codes::TOO_LARGE);

        // Five series of 250 points each stay under the per-series cap but
        // cross the snapshot total.
        let per_series: Vec<serde_json::Value> = (0..250)
            .map(|index| json!({ "x": index as f64, "y": 0.0 }))
            .collect();
        let series: Vec<serde_json::Value> = (0..5)
            .map(|index| json!({ "name": format!("s{index}"), "points": per_series }))
            .collect();
        let error = decode_viz_payload(
            "chart.line.v1",
            &encode(json!({ "capture": capture(), "series": series })),
        )
        .expect_err("over the snapshot point total");
        assert_eq!(error.code, codes::TOO_LARGE);
    }

    #[test]
    fn chart_line_axis_pair_is_validated() {
        let lone = json!({ "capture": capture(), "y_min": 0.0 });
        let error = decode_viz_payload("chart.line.v1", &encode(lone)).expect_err("lone y_min");
        assert_eq!(error.code, codes::BAD_PAYLOAD);

        let inverted = json!({ "capture": capture(), "y_min": 2.0, "y_max": 1.0 });
        let error = decode_viz_payload("chart.line.v1", &encode(inverted)).expect_err("inverted");
        assert_eq!(error.code, codes::BAD_PAYLOAD);

        let ok = json!({ "capture": capture(), "y_min": 0.0, "y_max": 1.0 });
        assert!(decode_viz_payload("chart.line.v1", &encode(ok)).is_ok());
    }

    #[test]
    fn chart_line_requires_point_coordinates() {
        let missing_y = json!({
            "capture": capture(),
            "series": [{ "name": "s", "points": [{ "x": 1.0 }] }],
        });
        let error = decode_viz_payload("chart.line.v1", &encode(missing_y)).expect_err("no y");
        assert_eq!(error.code, codes::BAD_PAYLOAD);
    }

    #[test]
    fn chart_gauge_requires_value_and_ordered_range() {
        let missing_value = json!({
            "capture": capture(),
            "items": [{ "key": "w" }],
        });
        let error =
            decode_viz_payload("chart.gauge.v1", &encode(missing_value)).expect_err("no value");
        assert_eq!(error.code, codes::BAD_PAYLOAD);

        let inverted = json!({
            "capture": capture(),
            "items": [{ "key": "w", "value": 0.5, "min": 1.0, "max": 0.0 }],
        });
        let error = decode_viz_payload("chart.gauge.v1", &encode(inverted)).expect_err("inverted");
        assert_eq!(error.code, codes::BAD_PAYLOAD);

        let ok = json!({
            "capture": capture(),
            "items": [
                { "key": "w", "value": 0.62 },
                { "key": "t", "value": 45.0, "min": 0.0, "max": 100.0, "unit": "%" },
            ],
        });
        let decoded = decode_viz_payload("chart.gauge.v1", &encode(ok)).expect("decodes");
        assert_eq!(decoded.item_count(), 2);
        let VizPayload::ChartGauge(gauge) = decoded else {
            panic!("expected chart.gauge.v1");
        };
        assert_eq!(gauge.items[0].min, 0.0, "min defaults 0");
        assert_eq!(gauge.items[0].max, 1.0, "max defaults 1");
    }

    #[test]
    fn timeline_counts_events_across_lanes_and_validates_window() {
        let ok = json!({
            "capture": capture(),
            "t0": 0.0, "t1": 60.0,
            "lanes": [
                { "name": "layer-0", "events": [
                    { "id": "e1", "t": 3.0, "dur": 1.5 },
                    { "id": "e2", "t": 10.0 },
                ] },
                { "name": "layer-1", "events": [
                    { "id": "e3", "t": 4.0, "state": "alert" },
                ] },
            ],
        });
        let decoded = decode_viz_payload("timeline.v1", &encode(ok)).expect("decodes");
        assert_eq!(decoded.item_count(), 3, "events across every lane");

        let lone = json!({ "capture": capture(), "t1": 60.0 });
        let error = decode_viz_payload("timeline.v1", &encode(lone)).expect_err("lone t1");
        assert_eq!(error.code, codes::BAD_PAYLOAD);

        let negative_dur = json!({
            "capture": capture(),
            "lanes": [{ "name": "l", "events": [{ "id": "e", "t": 1.0, "dur": -1.0 }] }],
        });
        let error =
            decode_viz_payload("timeline.v1", &encode(negative_dur)).expect_err("negative dur");
        assert_eq!(error.code, codes::BAD_PAYLOAD);
    }

    #[test]
    fn timeline_lane_cap_and_event_item_cap_reject_too_large() {
        let lanes: Vec<serde_json::Value> = (0..(MAX_VIZ_SERIES_PER_SNAPSHOT + 1))
            .map(|index| json!({ "name": format!("l{index}") }))
            .collect();
        let error = decode_viz_payload(
            "timeline.v1",
            &encode(json!({ "capture": capture(), "lanes": lanes })),
        )
        .expect_err("over the lane cap");
        assert_eq!(error.code, codes::TOO_LARGE);

        // Events spread across lanes still share the snapshot item cap.
        let events: Vec<serde_json::Value> = (0..(MAX_VIZ_ITEMS_PER_SNAPSHOT / 2 + 1))
            .map(|index| json!({ "id": format!("e{index}"), "t": index as f64 }))
            .collect();
        let error = decode_viz_payload(
            "timeline.v1",
            &encode(json!({
                "capture": capture(),
                "lanes": [
                    { "name": "a", "events": events },
                    { "name": "b", "events": events },
                ],
            })),
        )
        .expect_err("over the item cap");
        assert_eq!(error.code, codes::TOO_LARGE);
    }

    #[test]
    fn finite_pairs_whose_span_overflows_reject() {
        // Individually finite extremes whose difference (or sum) is
        // infinite would put NaN into every normalized span downstream.
        let event_overflow = json!({
            "capture": capture(),
            "lanes": [{ "name": "l", "events": [
                { "id": "e", "t": f64::MAX, "dur": f64::MAX },
            ] }],
        });
        let error =
            decode_viz_payload("timeline.v1", &encode(event_overflow)).expect_err("t+dur overflow");
        assert_eq!(error.code, codes::BAD_PAYLOAD);

        let window_overflow = json!({
            "capture": capture(),
            "t0": -f64::MAX, "t1": f64::MAX,
        });
        let error = decode_viz_payload("timeline.v1", &encode(window_overflow))
            .expect_err("window span overflow");
        assert_eq!(error.code, codes::BAD_PAYLOAD);

        let axis_overflow = json!({
            "capture": capture(),
            "y_min": -f64::MAX, "y_max": f64::MAX,
        });
        let error = decode_viz_payload("chart.line.v1", &encode(axis_overflow))
            .expect_err("y span overflow");
        assert_eq!(error.code, codes::BAD_PAYLOAD);

        let gauge_overflow = json!({
            "capture": capture(),
            "items": [{ "key": "g", "value": 0.0, "min": -f64::MAX, "max": f64::MAX }],
        });
        let error = decode_viz_payload("chart.gauge.v1", &encode(gauge_overflow))
            .expect_err("gauge span overflow");
        assert_eq!(error.code, codes::BAD_PAYLOAD);
    }

    #[test]
    fn every_chart_kind_requires_capture() {
        for kind in [
            "chart.bar.v1",
            "chart.line.v1",
            "chart.gauge.v1",
            "timeline.v1",
        ] {
            let error =
                decode_viz_payload(kind, &encode(json!({}))).expect_err("capture is mandatory");
            assert_eq!(error.code, codes::BAD_PAYLOAD, "{kind}");
        }
    }
}
