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
use serde::Deserialize;

use crate::ai::AiCommand;
use crate::osc::{RattyAiCommand, ai_object_namespace};
use crate::query::{B64DecodeError, b64url_decode, codes};
use crate::query_channel::{AckOutcome, AiDiagnostics, ack_commit};

/// Upper bound on one *decoded* `viz.set` payload, enforced before the
/// bytes are allocated ([`crate::query::b64url_decode`]). Bounds the
/// memory a single hostile sequence can pin and — via the const assert
/// below — guarantees a maximal legitimate payload survives the OSC
/// watchdog intact.
pub const MAX_VIZ_PAYLOAD_BYTES: usize = 32 * 1024;

// A `viz.set` payload rides a single OSC 777 sequence, and the OSC
// watchdog (`crate::inline::MAX_OSC_SEQUENCE_BYTES`) truncates anything
// longer into a garbage tail with *no error ack* — the failure would be
// silent. base64url expands 3 payload bytes into 4 characters; 1 KiB of
// headroom generously covers the envelope (action, id, kind, anchor
// params, tok=). This must hold or the decode limit advertises payloads
// the wire cannot actually carry.
const _: () =
    assert!(MAX_VIZ_PAYLOAD_BYTES.div_ceil(3) * 4 + 1024 <= crate::inline::MAX_OSC_SEQUENCE_BYTES);

/// Upper bound on the items in one snapshot (`ps`/`fs`/`net` items, `git`
/// branches). Bounds per-entry render work and memory against a hostile
/// emitter packing the byte budget with tiny items.
pub const MAX_VIZ_ITEMS_PER_SNAPSHOT: usize = 256;

/// Upper bound, in bytes, on any single label string inside a payload
/// (names, paths, states, capture provenance) and on `viz.effect` keys.
/// Bounds stored-string memory and keeps every projection record small
/// enough for size-bounded reply pages.
pub const MAX_VIZ_LABEL_BYTES: usize = 128;

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

/// The registered, versioned payload kinds this build decodes and renders.
/// The version is part of the name; anything else rejects `bad-kind`.
pub const REGISTERED_VIZ_KINDS: &[&str] = &["ps.v1", "fs.v1", "git.v1", "net.v1"];

// ── Payload schemas ──
//
// Unknown JSON fields are deliberately *ignored* (serde's default) so the
// wire can evolve additively; over-limit sizes are hard-rejected. Identity
// fields (pids, paths, names) and capture provenance are required;
// magnitude fields default.

/// Capture provenance carried by every snapshot. Required: ratty never
/// implies liveness it was not given — a transmission shipping synthetic
/// data declares itself here, and a collector stamps its real source.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct VizCapture {
    /// Where the data came from (e.g. `ratty-ai ps/sysinfo darwin`).
    pub source: String,
    /// When it was captured (RFC 3339 recommended; opaque on the wire).
    pub ts: String,
}

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
}

impl VizPayload {
    /// The registered kind name this payload decoded as.
    pub fn kind(&self) -> &'static str {
        match self {
            VizPayload::Ps(_) => "ps.v1",
            VizPayload::Fs(_) => "fs.v1",
            VizPayload::Git(_) => "git.v1",
            VizPayload::Net(_) => "net.v1",
        }
    }

    /// The capture provenance every payload carries.
    pub fn capture(&self) -> &VizCapture {
        match self {
            VizPayload::Ps(payload) => &payload.capture,
            VizPayload::Fs(payload) => &payload.capture,
            VizPayload::Git(payload) => &payload.capture,
            VizPayload::Net(payload) => &payload.capture,
        }
    }

    /// Number of keyed items in the snapshot (`git` counts branches).
    pub fn item_count(&self) -> usize {
        match self {
            VizPayload::Ps(payload) => payload.items.len(),
            VizPayload::Fs(payload) => payload.items.len(),
            VizPayload::Git(payload) => payload.branches.len(),
            VizPayload::Net(payload) => payload.items.len(),
        }
    }

    /// Enforces the size limits the schema types cannot express: item
    /// counts and label byte lengths.
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
        }
        Ok(())
    }
}

fn check_label(field: &'static str, value: &str) -> Result<(), VizDecodeError> {
    if value.len() > MAX_VIZ_LABEL_BYTES {
        return Err(VizDecodeError {
            code: codes::TOO_LARGE,
            message: format!("{field} exceeds {MAX_VIZ_LABEL_BYTES} bytes"),
        });
    }
    Ok(())
}

fn check_items(count: usize) -> Result<(), VizDecodeError> {
    if count > MAX_VIZ_ITEMS_PER_SNAPSHOT {
        return Err(VizDecodeError {
            code: codes::TOO_LARGE,
            message: format!("snapshot exceeds {MAX_VIZ_ITEMS_PER_SNAPSHOT} items"),
        });
    }
    Ok(())
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
/// bounded item counts and label lengths.
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
        B64DecodeError::TooLarge => VizDecodeError {
            code: codes::TOO_LARGE,
            message: format!("decoded payload exceeds {MAX_VIZ_PAYLOAD_BYTES} bytes"),
        },
        B64DecodeError::BadChar | B64DecodeError::BadLength => VizDecodeError {
            code: codes::BAD_PAYLOAD,
            message: "data= is not unpadded base64url".to_string(),
        },
    })?;
    let payload = match kind {
        "ps.v1" => VizPayload::Ps(parse_json(&bytes)?),
        "fs.v1" => VizPayload::Fs(parse_json(&bytes)?),
        "git.v1" => VizPayload::Git(parse_json(&bytes)?),
        "net.v1" => VizPayload::Net(parse_json(&bytes)?),
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
// The v1 vocabulary is deliberately small: every kind lowers onto keyed
// grid cells (one small mesh per item) with a normalized magnitude and a
// palette slot. M3.6 grows real chart kinds on the same substrate.

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

/// One snapshot item lowered onto the shared grid-render vocabulary: a
/// stable domain key, a normalized magnitude, and a palette slot.
#[derive(Debug, Clone, PartialEq)]
pub struct VizChildSpec {
    /// The item's stable semantic key (pid / path / branch / iface as a
    /// string) — the same key `viz.effect` targets.
    pub key: String,
    /// Magnitude in `0.0..=1.0`, normalized *within the snapshot* (the
    /// tallest bar is the snapshot's largest item, not an absolute unit).
    pub magnitude: f32,
    /// The semantic palette slot for the item's state.
    pub palette: VizPaletteSlot,
}

/// Lowers a decoded payload onto the shared grid vocabulary, in item
/// order. Magnitudes are normalized within the snapshot (cpu for `ps`,
/// log-scaled size for `fs`, log-scaled rx+tx for `net`; `git` branches
/// weight the checked-out branch over the rest).
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
                })
                .collect()
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
                let (x, y, cols, rows, replace) = (*x, *y, *cols, *rows, *replace);
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
                x: Some(10),
                y: Some(5),
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
                x,
                y,
                cols,
                rows,
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
