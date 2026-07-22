//! The Bevy side of the OSC 778 query channel.
//!
//! [`crate::query`] parses 778 envelopes inside the parser callbacks;
//! [`crate::systems::pump_pty_output`] drains them and emits them as
//! [`QueryRequest`] messages. [`answer_queries`] — ordered after every
//! command-applying system — resolves each op against ECS-derived
//! projections and writes the reply back through
//! [`TerminalRuntime::write_input`], so a query that arrives in the same
//! chunk as a command observes the command's committed state.
//!
//! Read scope has three tiers (locked in the M3 map):
//!
//! 1. scene-global public state (`state.scene`, `caps`),
//! 2. the caller's own namespace in full (`state.objects`, `state.errors`),
//! 3. other agents' **public render projections** only — the minimal
//!    structured facts of what is visibly on screen.
//!
//! Visibility grants observation, not control: projections never expose
//! Bevy `Entity` values, asset provenance, or another namespace's
//! internals, and reading confers no authority to mutate. Replies are
//! size-bounded; large collections paginate with opaque cursors bound to
//! the session nonce, so a cursor from another process fails decode
//! instead of silently returning wrong data.

use std::collections::{HashMap, VecDeque};

use bevy::ecs::message::{Message, MessageReader, MessageWriter};
use bevy::prelude::*;
use serde_json::{Value, json};

use crate::effects::AiEffects;
use crate::inline::{InlineAnchor, InlineObject, RgpInlineObject, TerminalInlineObjects};
use crate::model::{CursorModelChoice, CursorSettings};
use crate::osc::{ACK_TOKEN_KEY, ai_object_namespace};
use crate::query::{self, QueryEnvelope, WireErrorReply, codes};
use crate::runtime::{IngressSource, TerminalRuntime};
use crate::scene::{StageTween, TerminalPlaneView, TerminalPlaneWarp, TerminalPresentation};

/// Diagnostics retained per agent namespace (a bounded ring; older entries
/// are dropped, mirroring the bounded-resource posture of the object caps).
pub const MAX_DIAGNOSTICS_PER_NAMESPACE: usize = 32;

/// JSON payload budget per reply, chosen so the framed, base64url-expanded
/// sequence stays under [`query::MAX_REPLY_SEQUENCE_BYTES`].
const REPLY_PAYLOAD_BUDGET: usize = 2700;

/// The v1 query ops this build answers, advertised by `caps`.
///
/// `state.macros` and `state.executions` are answered honestly empty until
/// the macro subsystem (M3.7) lands; new ops are added here additively and
/// never grow new CLI subcommands.
pub const SUPPORTED_OPS: &[&str] = &[
    "caps",
    "state.scene",
    "state.objects",
    "state.visible_objects",
    "state.neighbors",
    "state.namespaces",
    "state.macros",
    "state.executions",
    "state.errors",
];

/// One OSC 778 item drained from the parser, delivered to the Bevy world.
#[derive(Message, Debug, Clone)]
pub struct QueryRequest {
    /// The ingress context the bytes arrived through.
    pub source: IngressSource,
    /// The classified item.
    pub item: QueryItem,
}

/// What an OSC 778 sequence classified into at ingress.
#[derive(Debug, Clone)]
pub enum QueryItem {
    /// A well-formed query to answer.
    Query(QueryEnvelope),
    /// An error reply owed for a parse-layer failure.
    Error(WireErrorReply),
}

/// The decided outcome of a `tok=`-carrying OSC 777 command, written by
/// whichever system owns the command's state mutation and drained into a
/// `t=r;kind=ack` reply by [`answer_queries`].
#[derive(Message, Debug, Clone)]
pub struct AckOutcome {
    /// The ingress context the command arrived through.
    pub source: IngressSource,
    /// The command's `tok=` correlation token.
    pub token: String,
    /// Whether the command's immediate state mutation committed.
    pub ok: bool,
    /// The rejection code when `ok` is false.
    pub code: Option<&'static str>,
}

/// Session identity for the query channel.
///
/// The nonce is minted at construction — never accepted from the byte
/// stream, per the no-in-band-identity rule — and scopes pagination
/// cursors to this session: a cursor from a previous process fails decode
/// instead of silently returning wrong data. `caps` exposes it so clients
/// can detect a restart.
#[derive(Resource)]
pub struct QuerySession {
    nonce: u64,
}

impl Default for QuerySession {
    fn default() -> Self {
        Self {
            nonce: random_u64(),
        }
    }
}

impl QuerySession {
    /// The session nonce as fixed-width hex (the `caps` `session` field).
    pub fn nonce_hex(&self) -> String {
        format!("{:016x}", self.nonce)
    }
}

fn random_u64() -> u64 {
    let mut bytes = [0_u8; 8];
    getrandom03::fill(&mut bytes).expect("system entropy is available");
    u64::from_le_bytes(bytes)
}

/// One recorded command rejection.
#[derive(Debug, Clone)]
struct DiagRecord {
    seq: u64,
    action: &'static str,
    code: &'static str,
    message: String,
}

/// Bounded per-namespace rejection diagnostics, populated at the same
/// sites as the existing rejection `warn!`s and read back through
/// `state.errors` (callers see their own namespace only).
#[derive(Resource, Default)]
pub struct AiDiagnostics {
    seq: u64,
    rings: HashMap<u8, VecDeque<DiagRecord>>,
}

impl AiDiagnostics {
    /// Records a rejection for the given namespace.
    pub fn record(
        &mut self,
        namespace: u8,
        action: &'static str,
        code: &'static str,
        message: String,
    ) {
        self.seq += 1;
        let ring = self.rings.entry(namespace).or_default();
        if ring.len() >= MAX_DIAGNOSTICS_PER_NAMESPACE {
            ring.pop_front();
        }
        ring.push_back(DiagRecord {
            seq: self.seq,
            action,
            code,
            message,
        });
    }
}

/// Writes a commit ack when the command opted in with `tok=`.
pub(crate) fn ack_commit(
    acks: &mut MessageWriter<AckOutcome>,
    source: IngressSource,
    ack_token: &Option<String>,
) {
    if let Some(token) = ack_token {
        acks.write(AckOutcome {
            source,
            token: token.clone(),
            ok: true,
            code: None,
        });
    }
}

/// Records a rejection diagnostic and, when the command opted in with
/// `tok=`, writes the matching error ack. Call this beside the existing
/// rejection `warn!`s — it supplements them, it never replaces them.
pub(crate) fn reject(
    diagnostics: &mut AiDiagnostics,
    acks: &mut MessageWriter<AckOutcome>,
    source: IngressSource,
    ack_token: &Option<String>,
    action: &'static str,
    code: &'static str,
    message: String,
) {
    diagnostics.record(source.namespace(), action, code, message);
    if let Some(token) = ack_token {
        acks.write(AckOutcome {
            source,
            token: token.clone(),
            ok: false,
            code: Some(code),
        });
    }
}

/// Answers queued OSC 778 queries and flushes command acks.
///
/// Ordered after `pump_pty_output` and every command-applying system so a
/// same-chunk "write then read" observes the write, and the ack for a
/// command precedes the reply to a query that followed it. Replies exit
/// through [`TerminalRuntime::write_input`], routed by the request's
/// stamped [`IngressSource`] — never broadcast.
#[allow(clippy::too_many_arguments)]
pub fn answer_queries(
    mut queries: MessageReader<QueryRequest>,
    mut acks: MessageReader<AckOutcome>,
    runtime: Res<TerminalRuntime>,
    session: Res<QuerySession>,
    inline_objects: Res<TerminalInlineObjects>,
    diagnostics: Res<AiDiagnostics>,
    presentation: Res<TerminalPresentation>,
    plane_warp: Res<TerminalPlaneWarp>,
    plane_view: Res<TerminalPlaneView>,
    stage_tween: Res<StageTween>,
    cursor: Res<CursorSettings>,
    effects: Res<AiEffects>,
) {
    // Acks first: a same-chunk "command with tok= then query" reads its
    // ack before the query reply, in mutation order.
    for AckOutcome {
        source,
        token,
        ok,
        code,
    } in acks.read()
    {
        send_reply(&runtime, *source, token, true, *ok, *code, None);
    }

    for QueryRequest { source, item } in queries.read() {
        match item {
            QueryItem::Error(error) => {
                send_reply(
                    &runtime,
                    *source,
                    &error.token,
                    error.ack,
                    false,
                    Some(error.code),
                    None,
                );
            }
            QueryItem::Query(envelope) => {
                let ctx = QueryCtx {
                    session: &session,
                    inline_objects: &inline_objects,
                    diagnostics: &diagnostics,
                    presentation: &presentation,
                    plane_warp: &plane_warp,
                    plane_view: &plane_view,
                    stage_tween: &stage_tween,
                    cursor: &cursor,
                    effects: &effects,
                    grid: runtime.parser.screen().size(),
                };
                match answer(envelope, *source, &ctx) {
                    Ok(value) => {
                        let payload = value.to_string();
                        send_reply(
                            &runtime,
                            *source,
                            &envelope.token,
                            false,
                            true,
                            None,
                            Some(payload.as_bytes()),
                        );
                    }
                    Err(code) => {
                        send_reply(
                            &runtime,
                            *source,
                            &envelope.token,
                            false,
                            false,
                            Some(code),
                            None,
                        );
                    }
                }
            }
        }
    }
}

/// Writes one reply to the transport the request arrived through. On wasm,
/// a reply whose token belongs to a pending `RattySession.query()` promise
/// resolves that promise instead of entering the byte stream.
fn send_reply(
    runtime: &TerminalRuntime,
    source: IngressSource,
    token: &str,
    ack: bool,
    ok: bool,
    code: Option<&str>,
    payload: Option<&[u8]>,
) {
    #[cfg(target_arch = "wasm32")]
    if crate::web::try_resolve_pending(token, ack, ok, code, payload) {
        return;
    }

    let bytes = query::reply_sequence(token, ack, ok, code, payload);
    let bytes = if bytes.len() > query::MAX_REPLY_SEQUENCE_BYTES {
        // Pagination keeps replies under the bound; if an op ever slips
        // through, fail the query loudly rather than stall the PTY with an
        // oversized blocking write.
        warn!(
            "ratty-query: reply for token {token} exceeded {} bytes; replying {}",
            query::MAX_REPLY_SEQUENCE_BYTES,
            codes::INTERNAL
        );
        query::reply_sequence(token, ack, false, Some(codes::INTERNAL), None)
    } else {
        bytes
    };
    // One transport per runtime today; the match keeps routing keyed to
    // the stamped ingress source so future transports cannot broadcast.
    match source {
        IngressSource::Local => runtime.write_input(&bytes),
    }
}

/// Borrowed view of everything a query op may read.
struct QueryCtx<'a> {
    session: &'a QuerySession,
    inline_objects: &'a TerminalInlineObjects,
    diagnostics: &'a AiDiagnostics,
    presentation: &'a TerminalPresentation,
    plane_warp: &'a TerminalPlaneWarp,
    plane_view: &'a TerminalPlaneView,
    stage_tween: &'a StageTween,
    cursor: &'a CursorSettings,
    effects: &'a AiEffects,
    /// Live grid size as `(rows, cols)`, from the parser screen.
    grid: (u16, u16),
}

/// Resolves one query op to its JSON payload, or an error code.
fn answer(
    envelope: &QueryEnvelope,
    source: IngressSource,
    ctx: &QueryCtx<'_>,
) -> Result<Value, &'static str> {
    let data: Value = if envelope.data.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&envelope.data).map_err(|_| codes::BAD_PAYLOAD)?
    };

    match envelope.op.as_str() {
        "caps" => Ok(caps(ctx)),
        "state.scene" => Ok(scene_state(ctx)),
        "state.objects" => own_objects(ctx, source, &data),
        "state.visible_objects" => visible_objects(ctx, &data),
        "state.neighbors" => neighbors(ctx, &data),
        "state.namespaces" => Ok(namespaces(ctx)),
        // The macro subsystem is M3.7; until it lands there are honestly
        // no macros and no executions. Never fabricate.
        "state.macros" | "state.executions" => Ok(json!({ "items": [] })),
        "state.errors" => errors(ctx, source, &data),
        _ => Err(codes::UNSUPPORTED_OP),
    }
}

/// `caps`: protocol discovery — the 778 analog of the RGP support reply.
/// Keys are append-only so older clients keep parsing newer replies.
fn caps(ctx: &QueryCtx<'_>) -> Value {
    json!({
        "v": 1,
        "session": ctx.session.nonce_hex(),
        "ops": SUPPORTED_OPS,
        "ack": { "key": ACK_TOKEN_KEY },
        "limits": {
            "max_query_bytes": query::MAX_QUERY_SEQUENCE_BYTES,
            "max_query_data_bytes": query::MAX_QUERY_DATA_BYTES,
            "max_reply_bytes": query::MAX_REPLY_SEQUENCE_BYTES,
            "objects_per_namespace": crate::ai::MAX_AI_OBJECTS_PER_NAMESPACE,
            "ids_per_session": crate::ai::MAX_AI_OBJECT_IDS_PER_SESSION,
            "errors_per_namespace": MAX_DIAGNOSTICS_PER_NAMESPACE,
        },
    })
}

/// `state.scene`: scene-global public state. The camera's drag-interaction
/// fields and effect timers are private and not projected.
fn scene_state(ctx: &QueryCtx<'_>) -> Value {
    use crate::scene::TerminalPresentationMode as Mode;
    let mode = match ctx.presentation.mode {
        Mode::Flat2d => "flat2d",
        Mode::Plane3d => "plane3d",
        Mode::Mobius3d => "mobius3d",
    };
    let effects = ctx.effects.public_state();
    let (rows, cols) = ctx.grid;
    json!({
        "mode": mode,
        "warp": ctx.plane_warp.amount,
        "view": {
            "yaw": ctx.plane_view.yaw,
            "pitch": ctx.plane_view.pitch,
            "zoom": ctx.plane_view.zoom,
            "offset": [ctx.plane_view.camera_offset.x, ctx.plane_view.camera_offset.y],
        },
        "grid": { "cols": cols, "rows": rows },
        "tween_active": ctx.stage_tween.active,
        "cursor": {
            "visible": ctx.cursor.visible,
            "brightness": ctx.cursor.brightness,
            "spin": ctx.cursor.spin_speed,
            "bob_speed": ctx.cursor.bob_speed,
            "bob_amplitude": ctx.cursor.bob_amplitude,
            "model": match &ctx.cursor.model {
                CursorModelChoice::Config => "config".to_string(),
                CursorModelChoice::Embedded(name) => format!("embedded:{name}"),
            },
        },
        "effects": {
            "thinking": effects.thinking,
            "confidence": effects.confidence,
            "mood": effects.mood,
            "flash": effects.flash,
            "pulse": effects.pulse,
            "tint": effects.tint,
        },
    })
}

/// The spec's visibility rule, reproduced from `sync_inline_objects`'
/// renderable predicate: an object is visible when it has an anchor whose
/// row range intersects the live grid.
fn anchor_visible(anchor: &InlineAnchor, grid_rows: u16) -> bool {
    let start = anchor.row as i32;
    let end = start + anchor.rows as i32;
    start < grid_rows as i32 && end > 0
}

fn object_kind(object: &InlineObject) -> &'static str {
    match object {
        InlineObject::KittyImage(_) => "image",
        InlineObject::RgpObject(RgpInlineObject::Stl { .. }) => "stl",
        InlineObject::RgpObject(RgpInlineObject::Obj { .. }) => "obj",
        InlineObject::RgpObject(RgpInlineObject::Gltf { .. }) => "gltf",
    }
}

fn vec3(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

/// The public render projection of one object — exactly the tier-3 field
/// list locked in the design: id, owner namespace, kind, anchor cell,
/// transform/offset, scale, rotation/spin, brightness/visibility, bounds,
/// current revision. Never colors, asset names, provenance, or entities.
fn public_projection(ctx: &QueryCtx<'_>, id: u32, object: &InlineObject) -> Value {
    let anchor = ctx.inline_objects.anchors.get(&id);
    let (rows, _) = ctx.grid;
    let mut value = json!({
        "id": id,
        "owner": ai_object_namespace(id),
        "kind": object_kind(object),
        "visible": anchor.is_some_and(|a| anchor_visible(a, rows)),
        "revision": ctx.inline_objects.revision(id),
        "anchor": Value::Null,
        "offset": Value::Null,
        "scale": Value::Null,
        "scale3": Value::Null,
        "rotation": Value::Null,
        "spin": Value::Null,
        "brightness": Value::Null,
    });
    if let Some(anchor) = anchor {
        let style = anchor.style;
        value["anchor"] = json!({
            "row": anchor.row,
            "col": anchor.col,
            "cols": anchor.columns,
            "rows": anchor.rows,
        });
        value["offset"] = vec3(style.offset);
        value["scale"] = json!(style.scale);
        value["scale3"] = vec3(style.scale3);
        value["rotation"] = vec3(style.rotation);
        value["spin"] = json!(style.spin);
        value["brightness"] = json!(style.brightness);
    }
    value
}

/// The caller's own full record: the public projection plus the private
/// style fields only the owner may read.
fn own_record(ctx: &QueryCtx<'_>, id: u32, object: &InlineObject) -> Value {
    let mut value = public_projection(ctx, id, object);
    let style = ctx.inline_objects.anchors.get(&id).map(|a| a.style);
    value["color"] = json!(style.and_then(|s| s.color));
    value["depth"] = json!(style.map(|s| s.depth));
    value["animate"] = json!(style.map(|s| s.animate));
    value["bob"] = json!(style.and_then(|s| s.bob));
    value["bob_amplitude"] = json!(style.and_then(|s| s.bob_amplitude));
    value["phase"] = json!(style.map(|s| s.phase));
    value
}

/// `state.objects`: the caller's complete object records, including
/// anchor-less (scrolled-away) objects. Sorted by id; paginated.
fn own_objects(
    ctx: &QueryCtx<'_>,
    source: IngressSource,
    data: &Value,
) -> Result<Value, &'static str> {
    let namespace = source.namespace();
    let mut items: Vec<(u64, Value)> = ctx
        .inline_objects
        .objects
        .iter()
        .filter(|(id, _)| ai_object_namespace(**id) == Some(namespace))
        .map(|(id, object)| (u64::from(*id), own_record(ctx, *id, object)))
        .collect();
    items.sort_by_key(|(key, _)| *key);
    paginate(ctx, items, data)
}

/// `state.visible_objects`: public projections of everything visibly on
/// screen — both partitions, every namespace. Sorted by id; paginated.
fn visible_objects(ctx: &QueryCtx<'_>, data: &Value) -> Result<Value, &'static str> {
    let (rows, _) = ctx.grid;
    let mut items: Vec<(u64, Value)> = ctx
        .inline_objects
        .anchors
        .iter()
        .filter(|(_, anchor)| anchor_visible(anchor, rows))
        .filter_map(|(id, _)| {
            let object = ctx.inline_objects.objects.get(id)?;
            Some((u64::from(*id), public_projection(ctx, *id, object)))
        })
        .collect();
    items.sort_by_key(|(key, _)| *key);
    paginate(ctx, items, data)
}

/// `state.neighbors`: public projections within a radius of a center point
/// or object. Distance is Euclidean between anchor centers, in cells.
/// Items are sorted by id (stable under pagination) and each carries its
/// `distance`; clients sort by distance if they need rank order.
fn neighbors(ctx: &QueryCtx<'_>, data: &Value) -> Result<Value, &'static str> {
    let radius = data
        .get("radius")
        .and_then(Value::as_f64)
        .filter(|r| r.is_finite() && *r > 0.0 && *r <= 65_535.0)
        .ok_or(codes::BAD_PAYLOAD)?;

    let center = if let Some(center) = data.get("center") {
        let row = center
            .get("row")
            .and_then(Value::as_u64)
            .filter(|v| *v <= u64::from(u16::MAX))
            .ok_or(codes::BAD_PAYLOAD)?;
        let col = center
            .get("col")
            .and_then(Value::as_u64)
            .filter(|v| *v <= u64::from(u16::MAX))
            .ok_or(codes::BAD_PAYLOAD)?;
        (row as f64, col as f64, None)
    } else if let Some(id) = data.get("object") {
        let id = id
            .as_u64()
            .filter(|v| *v <= u64::from(u32::MAX))
            .ok_or(codes::BAD_PAYLOAD)? as u32;
        if !ctx.inline_objects.objects.contains_key(&id) {
            return Err(codes::UNKNOWN_ID);
        }
        let anchor = ctx
            .inline_objects
            .anchors
            .get(&id)
            .ok_or(codes::NO_ANCHOR)?;
        (
            f64::from(anchor.row) + f64::from(anchor.rows) / 2.0,
            f64::from(anchor.col) + f64::from(anchor.columns) / 2.0,
            Some(id),
        )
    } else {
        return Err(codes::BAD_PAYLOAD);
    };
    let (center_row, center_col, center_id) = center;

    let (rows, _) = ctx.grid;
    let mut items: Vec<(u64, Value)> = ctx
        .inline_objects
        .anchors
        .iter()
        .filter(|(id, anchor)| Some(**id) != center_id && anchor_visible(anchor, rows))
        .filter_map(|(id, anchor)| {
            let object = ctx.inline_objects.objects.get(id)?;
            let row = f64::from(anchor.row) + f64::from(anchor.rows) / 2.0;
            let col = f64::from(anchor.col) + f64::from(anchor.columns) / 2.0;
            let distance = ((row - center_row).powi(2) + (col - center_col).powi(2)).sqrt();
            if distance > radius {
                return None;
            }
            let mut projection = public_projection(ctx, *id, object);
            projection["distance"] = json!(distance);
            Some((u64::from(*id), projection))
        })
        .collect();
    items.sort_by_key(|(key, _)| *key);
    paginate(ctx, items, data)
}

/// `state.namespaces`: aggregate public presence — live object counts per
/// agent namespace plus the transmission/system partition.
fn namespaces(ctx: &QueryCtx<'_>) -> Value {
    let mut per_namespace: HashMap<u8, usize> = HashMap::new();
    let mut transmission = 0_usize;
    for id in ctx.inline_objects.objects.keys() {
        match ai_object_namespace(*id) {
            Some(namespace) => *per_namespace.entry(namespace).or_default() += 1,
            None => transmission += 1,
        }
    }
    let mut namespaces: Vec<_> = per_namespace.into_iter().collect();
    namespaces.sort_by_key(|(namespace, _)| *namespace);
    json!({
        "transmission": transmission,
        "namespaces": namespaces
            .into_iter()
            .map(|(namespace, objects)| json!({ "ns": namespace, "objects": objects }))
            .collect::<Vec<_>>(),
    })
}

/// `state.errors`: the caller's own rejection diagnostics, oldest first.
/// Sorted by sequence number; paginated.
fn errors(ctx: &QueryCtx<'_>, source: IngressSource, data: &Value) -> Result<Value, &'static str> {
    let items: Vec<(u64, Value)> = ctx
        .diagnostics
        .rings
        .get(&source.namespace())
        .map(|ring| {
            ring.iter()
                .map(|record| {
                    (
                        record.seq,
                        json!({
                            "seq": record.seq,
                            "action": record.action,
                            "code": record.code,
                            "message": record.message,
                        }),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    paginate(ctx, items, data)
}

fn encode_cursor(session: &QuerySession, after: u64) -> String {
    query::b64url_encode(format!("{}:{after}", session.nonce_hex()).as_bytes())
}

fn decode_cursor(session: &QuerySession, cursor: &str) -> Result<u64, &'static str> {
    let raw = query::b64url_decode(cursor, 64).map_err(|_| codes::BAD_CURSOR)?;
    let text = std::str::from_utf8(&raw).map_err(|_| codes::BAD_CURSOR)?;
    let (nonce, after) = text.split_once(':').ok_or(codes::BAD_CURSOR)?;
    if nonce != session.nonce_hex() {
        return Err(codes::BAD_CURSOR);
    }
    after.parse().map_err(|_| codes::BAD_CURSOR)
}

/// Assembles a size-bounded `{items, cursor?}` page from key-sorted items.
///
/// The cursor is the last included sort key, opaque and bound to the
/// session nonce. Between pages the collection may mutate; a resumed
/// cursor skips removed keys and includes newly added ones past it —
/// defined, monotone-by-key behavior rather than a stability promise.
fn paginate(
    ctx: &QueryCtx<'_>,
    items: Vec<(u64, Value)>,
    data: &Value,
) -> Result<Value, &'static str> {
    let after = match data.get("cursor") {
        None | Some(Value::Null) => None,
        Some(Value::String(cursor)) => Some(decode_cursor(ctx.session, cursor)?),
        Some(_) => return Err(codes::BAD_PAYLOAD),
    };

    let remaining: Vec<(u64, Value)> = items
        .into_iter()
        .filter(|(key, _)| after.is_none_or(|a| *key > a))
        .collect();

    let mut included = Vec::new();
    let mut used = 0_usize;
    let mut cursor = None;
    let mut last_key = 0_u64;
    for (key, value) in &remaining {
        let item_len = value.to_string().len() + 1;
        if !included.is_empty() && used + item_len > REPLY_PAYLOAD_BUDGET {
            cursor = Some(encode_cursor(ctx.session, last_key));
            break;
        }
        included.push(value.clone());
        used += item_len;
        last_key = *key;
    }

    let mut page = json!({ "items": included });
    if let Some(cursor) = cursor {
        page["cursor"] = json!(cursor);
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::app::AppExit;

    use crate::ai::{AiCommand, AiObjectRegistry, AiObjectRemoved, apply_ai_object_commands};
    use crate::config::AppConfig;
    use crate::inline::InlineStyle;
    use crate::query::{ParsedReply, ReplyScanner, parse_reply_body, query_sequence};
    use crate::runtime::VirtualTerminalHost;
    use crate::scene::TerminalPresentationMode;
    use crate::systems::pump_pty_output;
    use crate::terminal::TerminalRedrawState;

    /// A headless app wired exactly like the real pipeline: virtual
    /// transport → pump → object handler → query answerer, chained so one
    /// `update()` is one closed loop.
    fn test_app() -> (App, VirtualTerminalHost) {
        let config = AppConfig::default();
        let (runtime, host) = TerminalRuntime::virtual_channel(&config);
        let mut app = App::new();
        app.insert_resource(config);
        app.insert_resource(runtime);
        app.init_resource::<TerminalInlineObjects>();
        app.init_resource::<AiObjectRegistry>();
        app.init_resource::<CursorSettings>();
        app.init_resource::<TerminalRedrawState>();
        app.init_resource::<AiDiagnostics>();
        app.init_resource::<QuerySession>();
        app.init_resource::<AiEffects>();
        app.insert_resource(TerminalPresentation {
            mode: TerminalPresentationMode::Flat2d,
        });
        app.init_resource::<TerminalPlaneWarp>();
        app.init_resource::<TerminalPlaneView>();
        app.init_resource::<StageTween>();
        app.add_message::<AppExit>();
        app.add_message::<AiCommand>();
        app.add_message::<AiObjectRemoved>();
        app.add_message::<QueryRequest>();
        app.add_message::<AckOutcome>();
        app.add_systems(
            Update,
            (pump_pty_output, apply_ai_object_commands, answer_queries).chain(),
        );
        (app, host)
    }

    fn drain_replies(host: &VirtualTerminalHost) -> Vec<ParsedReply> {
        let mut scanner = ReplyScanner::default();
        while let Ok(chunk) = host.input_rx.try_recv() {
            scanner.push(&chunk);
        }
        let mut replies = Vec::new();
        while let Some(frame) = scanner.next_frame() {
            if let Some(reply) = parse_reply_body(&frame) {
                replies.push(reply);
            }
        }
        replies
    }

    fn run_query(
        app: &mut App,
        host: &VirtualTerminalHost,
        token: &str,
        op: &str,
        data: Option<Value>,
    ) -> ParsedReply {
        let data_text = data.map(|value| value.to_string());
        let sequence = query_sequence(token, op, data_text.as_deref().map(str::as_bytes));
        host.feed_tx
            .send(sequence.into_bytes())
            .expect("virtual feed accepts bytes");
        app.update();
        drain_replies(host)
            .into_iter()
            .find(|reply| reply.token == token)
            .expect("a correlated reply arrives")
    }

    fn payload(reply: &ParsedReply) -> Value {
        serde_json::from_slice(&reply.data).expect("reply payload is JSON")
    }

    const ID: u32 = 0x8000_0001;

    #[test]
    fn closed_loop_write_over_777_read_back_over_778() {
        let (mut app, host) = test_app();
        // One chunk: a tok='d spawn followed by a query. The ack must
        // arrive first and the query must observe the committed spawn.
        let spawn = format!(
            "\x1b]777;ratty:object.add;id={ID}&path=SkateMouse.stl&x=10&y=5&tok=acktok\x07"
        );
        let query = query_sequence("qtok", "state.objects", None);
        host.feed_tx
            .send(format!("{spawn}{query}").into_bytes())
            .expect("virtual feed accepts bytes");
        app.update();

        let replies = drain_replies(&host);
        assert_eq!(replies.len(), 2, "one ack, one query reply");
        let ack = &replies[0];
        assert_eq!(ack.token, "acktok");
        assert!(ack.ack, "the command reply is kind=ack");
        assert!(ack.ok, "the spawn committed");

        let reply = &replies[1];
        assert_eq!(reply.token, "qtok");
        assert!(!reply.ack);
        assert!(reply.ok);
        let page = payload(reply);
        let items = page["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item["id"], json!(ID));
        assert_eq!(item["owner"], json!(0));
        assert_eq!(item["kind"], json!("stl"));
        assert_eq!(item["visible"], json!(true));
        assert_eq!(item["revision"], json!(1));
        assert!(item["anchor"]["row"].is_u64());
    }

    #[test]
    fn rejected_commands_ack_with_their_code_and_land_in_state_errors() {
        let (mut app, host) = test_app();
        let spawn =
            format!("\x1b]777;ratty:object.add;id={ID}&path=SkateMouse.stl&x=10&y=5&tok=t1\x07");
        // Same id again without replace: already-exists.
        let dup =
            format!("\x1b]777;ratty:object.add;id={ID}&path=SkateMouse.stl&x=10&y=5&tok=t2\x07");
        host.feed_tx
            .send(format!("{spawn}{dup}").into_bytes())
            .expect("virtual feed accepts bytes");
        app.update();
        let replies = drain_replies(&host);
        assert_eq!(replies.len(), 2);
        assert!(replies[0].ok);
        assert!(!replies[1].ok);
        assert_eq!(replies[1].code.as_deref(), Some(codes::ALREADY_EXISTS));

        let reply = run_query(&mut app, &host, "q1", "state.errors", None);
        assert!(reply.ok);
        let page = payload(&reply);
        let items = page["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["code"], json!(codes::ALREADY_EXISTS));
        assert_eq!(items[0]["action"], json!("object.add"));
    }

    #[test]
    fn caps_advertises_ops_session_and_limits() {
        let (mut app, host) = test_app();
        let reply = run_query(&mut app, &host, "q1", "caps", None);
        assert!(reply.ok);
        let caps = payload(&reply);
        assert_eq!(caps["v"], json!(1));
        assert_eq!(caps["ack"]["key"], json!("tok"));
        assert_eq!(
            caps["session"].as_str().expect("session hex").len(),
            16,
            "the session nonce is fixed-width hex"
        );
        let ops: Vec<&str> = caps["ops"]
            .as_array()
            .expect("ops array")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(ops, SUPPORTED_OPS.to_vec());
        assert_eq!(
            caps["limits"]["objects_per_namespace"],
            json!(crate::ai::MAX_AI_OBJECTS_PER_NAMESPACE)
        );
    }

    #[test]
    fn unsupported_ops_and_malformed_envelopes_reply_ok0() {
        let (mut app, host) = test_app();
        let reply = run_query(&mut app, &host, "q1", "state.panes", None);
        assert!(!reply.ok);
        assert_eq!(reply.code.as_deref(), Some(codes::UNSUPPORTED_OP));

        // A wrong-version envelope with a recoverable token errors through
        // the wire-error path.
        host.feed_tx
            .send(b"\x1b]778;v=9;t=q;id=q2;op=caps\x1b\\".to_vec())
            .expect("virtual feed accepts bytes");
        app.update();
        let replies = drain_replies(&host);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].token, "q2");
        assert!(!replies[0].ok);
        assert_eq!(replies[0].code.as_deref(), Some(codes::BAD_VERSION));
    }

    /// Inserts `count` AI objects for namespace 0 directly into the
    /// registry resource (bypassing the wire — this seeds state, the
    /// queries under test still run the full loop).
    fn seed_objects(app: &mut App, count: u32) {
        let mut inline = app.world_mut().resource_mut::<TerminalInlineObjects>();
        for index in 0..count {
            inline.ai_insert_object(
                ID + index,
                InlineObject::RgpObject(RgpInlineObject::Gltf {
                    asset_path: "objects/x.glb".into(),
                    handle: None,
                }),
                10,
                5,
                InlineStyle::default(),
            );
        }
    }

    #[test]
    fn pagination_walks_every_object_exactly_once() {
        let (mut app, host) = test_app();
        seed_objects(&mut app, 30);

        let mut collected = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let data = cursor.as_ref().map(|c| json!({ "cursor": c }));
            let token = format!("q{pages}");
            let reply = run_query(&mut app, &host, &token, "state.objects", data);
            assert!(reply.ok);
            let page = payload(&reply);
            for item in page["items"].as_array().expect("items") {
                collected.push(item["id"].as_u64().expect("id"));
            }
            pages += 1;
            assert!(pages < 32, "pagination must terminate");
            match page["cursor"].as_str() {
                Some(next) => cursor = Some(next.to_string()),
                None => break,
            }
        }
        assert!(pages > 1, "30 records exceed one size-bounded page");
        let expected: Vec<u64> = (0..30).map(|i| u64::from(ID + i)).collect();
        assert_eq!(collected, expected, "every id exactly once, in order");
    }

    #[test]
    fn foreign_and_stale_cursors_fail_decode() {
        let (mut app, host) = test_app();
        seed_objects(&mut app, 1);
        // A cursor minted under a different session nonce.
        let foreign = query::b64url_encode(b"00000000deadbeef:5");
        let reply = run_query(
            &mut app,
            &host,
            "q1",
            "state.objects",
            Some(json!({ "cursor": foreign })),
        );
        assert!(!reply.ok);
        assert_eq!(reply.code.as_deref(), Some(codes::BAD_CURSOR));
    }

    #[test]
    fn neighbors_filters_by_radius_and_reports_distance() {
        let (mut app, host) = test_app();
        {
            let mut inline = app.world_mut().resource_mut::<TerminalInlineObjects>();
            let object = || {
                InlineObject::RgpObject(RgpInlineObject::Gltf {
                    asset_path: "objects/x.glb".into(),
                    handle: None,
                })
            };
            inline.ai_insert_object(ID, object(), 10, 5, InlineStyle::default());
            inline.ai_insert_object(ID + 1, object(), 14, 5, InlineStyle::default());
            inline.ai_insert_object(ID + 2, object(), 70, 20, InlineStyle::default());
        }
        // Around the first object: the second is ~4 cells away, the third
        // far outside the radius; the center object itself is excluded.
        let reply = run_query(
            &mut app,
            &host,
            "q1",
            "state.neighbors",
            Some(json!({ "object": ID, "radius": 10 })),
        );
        assert!(reply.ok);
        let page = payload(&reply);
        let items = page["items"].as_array().expect("items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], json!(ID + 1));
        let distance = items[0]["distance"].as_f64().expect("distance");
        assert!(
            (distance - 4.0).abs() < 0.01,
            "anchor centers are 4 cells apart"
        );

        // Radius is required.
        let reply = run_query(
            &mut app,
            &host,
            "q2",
            "state.neighbors",
            Some(json!({ "object": ID })),
        );
        assert!(!reply.ok);
        assert_eq!(reply.code.as_deref(), Some(codes::BAD_PAYLOAD));
    }

    #[test]
    fn off_screen_objects_are_invisible_and_excluded_from_visible_set() {
        let (mut app, host) = test_app();
        {
            let mut inline = app.world_mut().resource_mut::<TerminalInlineObjects>();
            let object = || {
                InlineObject::RgpObject(RgpInlineObject::Gltf {
                    asset_path: "objects/x.glb".into(),
                    handle: None,
                })
            };
            inline.ai_insert_object(ID, object(), 10, 5, InlineStyle::default());
            // Far below any real grid.
            inline.ai_insert_object(ID + 1, object(), 10, 500, InlineStyle::default());
        }
        let reply = run_query(&mut app, &host, "q1", "state.visible_objects", None);
        let page = payload(&reply);
        let items = page["items"].as_array().expect("items");
        assert_eq!(items.len(), 1, "only the on-grid object is visible");
        assert_eq!(items[0]["id"], json!(ID));

        // state.objects (own namespace) still lists both, flagged.
        let reply = run_query(&mut app, &host, "q2", "state.objects", None);
        let page = payload(&reply);
        let items = page["items"].as_array().expect("items");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["visible"], json!(true));
        assert_eq!(items[1]["visible"], json!(false));
    }

    #[test]
    fn state_scene_projects_public_state_and_namespaces_aggregate() {
        let (mut app, host) = test_app();
        seed_objects(&mut app, 2);
        {
            let mut inline = app.world_mut().resource_mut::<TerminalInlineObjects>();
            // A transmission-owned object (below the AI range).
            inline.objects.insert(
                7,
                InlineObject::RgpObject(RgpInlineObject::Gltf {
                    asset_path: "objects/x.glb".into(),
                    handle: None,
                }),
            );
        }
        let reply = run_query(&mut app, &host, "q1", "state.scene", None);
        let scene = payload(&reply);
        assert_eq!(scene["mode"], json!("flat2d"));
        assert_eq!(scene["warp"], json!(0.0));
        assert_eq!(scene["effects"]["thinking"], json!(false));
        assert!(scene["grid"]["cols"].is_u64());

        let reply = run_query(&mut app, &host, "q2", "state.namespaces", None);
        let aggregate = payload(&reply);
        assert_eq!(aggregate["transmission"], json!(1));
        assert_eq!(aggregate["namespaces"], json!([{ "ns": 0, "objects": 2 }]));
    }

    #[test]
    fn macros_and_executions_are_honestly_empty() {
        let (mut app, host) = test_app();
        for (token, op) in [("q1", "state.macros"), ("q2", "state.executions")] {
            let reply = run_query(&mut app, &host, token, op, None);
            assert!(reply.ok);
            assert_eq!(payload(&reply)["items"], json!([]));
        }
    }
}
