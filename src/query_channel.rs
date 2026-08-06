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
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use serde_json::{Value, json};

use crate::effects::AiEffects;
use crate::inline::{InlineAnchor, InlineObject, RgpInlineObject, TerminalInlineObjects};
use crate::model::{CursorModelChoice, CursorSettings};
use crate::osc::{ACK_TOKEN_KEY, ai_object_namespace};
use crate::query::{self, QueryEnvelope, WireErrorReply, codes};
use crate::runtime::{IngressSource, TerminalRuntime};
use crate::scene::{StageTween, TerminalPlaneView, TerminalPlaneWarp, TerminalPresentation};
use crate::sound::SoundState;

/// Diagnostics retained per agent namespace (a bounded ring; older entries
/// are dropped, mirroring the bounded-resource posture of the object caps).
pub const MAX_DIAGNOSTICS_PER_NAMESPACE: usize = 32;

/// Byte cap on one stored diagnostic message (see
/// [`TerminalDiagnostics::record`]).
pub const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 256;

/// JSON payload budget per reply, chosen so the framed, base64url-expanded
/// sequence stays under [`query::MAX_REPLY_SEQUENCE_BYTES`].
const REPLY_PAYLOAD_BUDGET: usize = 2700;

/// The v1 query ops this build answers, advertised by `caps`.
///
/// New ops are added here additively and never grow new CLI subcommands.
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
    "state.viz",
    "state.bookmarks",
    "state.rules",
    "state.sensors",
    "state.presence",
    "state.terminals",
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
    /// The outcome code: the rejection code when `ok` is false, or a
    /// success qualifier (e.g. `deferred` for a pre-unlock ambient set)
    /// when `ok` is true. The wire carries `code=` independently of `ok=`.
    pub code: Option<&'static str>,
    /// Structured ack payload (the reply's `data=`): the execution handle,
    /// queue position and estimated wait for a long-running operation
    /// (#18), or the new terminal's handle on an immediate-commit
    /// `term.spawn` (#49). `None` for every other ack.
    pub payload: Option<serde_json::Value>,
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
    /// Monotone counter for execution handles minted this session (#18).
    next_execution: u64,
}

impl Default for QuerySession {
    fn default() -> Self {
        Self {
            nonce: random_u64(),
            next_execution: 0,
        }
    }
}

impl QuerySession {
    /// The session nonce as fixed-width hex (the `caps` `session` field).
    pub fn nonce_hex(&self) -> String {
        format!("{:016x}", self.nonce)
    }

    /// Mints a session-unique execution handle: `<nonce-hex>-<seq>` (#18).
    /// Handles use the base64url alphabet (hex, `-`, digits) so they ride
    /// wire payload values and JSON unescaped. The random nonce prefix
    /// makes cross-restart collisions negligible; within a session the
    /// counter is monotone and handles are never reused.
    pub fn mint_execution_id(&mut self) -> String {
        self.next_execution += 1;
        format!("{:016x}-{}", self.nonce, self.next_execution)
    }

    /// Whether `id` was minted by THIS session. A handle from a previous
    /// process fails here and answers `unknown-id` — explicit staleness,
    /// mirroring how session-scoped pagination cursors fail decode instead
    /// of silently returning wrong data.
    pub fn owns_execution_id(&self, id: &str) -> bool {
        id.strip_prefix(&self.nonce_hex())
            .is_some_and(|rest| rest.starts_with('-'))
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

/// One terminal seat's bounded rejection diagnostics, populated at the
/// same sites as the existing rejection `warn!`s and read back through
/// `state.errors` (callers see their own ring only).
///
/// A component, not a resource: the diagnostics registry is wholly
/// session-half (#56 decision 5 names it), so the ring lives on the seat
/// and dies with it — a recycled namespace slot's next tenant starts with
/// `Default`, never the corpse's error ring. The ring capacity is still
/// [`MAX_DIAGNOSTICS_PER_NAMESPACE`] and the `caps` limit key keeps its
/// wire name `errors_per_namespace` — it still truthfully means per-caller
/// ring capacity, and renaming it would be wire surface.
#[derive(Component, Default)]
pub struct TerminalDiagnostics {
    /// Monotone per-terminal record sequence. Byte-identical to the old
    /// global counter at N=1 (only the boot terminal ever recorded).
    seq: u64,
    ring: VecDeque<DiagRecord>,
}

impl TerminalDiagnostics {
    /// Records a rejection into this terminal's ring.
    pub fn record(&mut self, action: &'static str, code: &'static str, mut message: String) {
        // Messages can embed wire-controlled strings (a bad mode tag, a
        // bad asset name) that arrive with no length cap of their own.
        // Truncating at the storage boundary bounds ring memory and
        // guarantees every `state.errors` record fits a size-bounded
        // reply page — an oversized record would otherwise poison the op.
        if message.len() > MAX_DIAGNOSTIC_MESSAGE_BYTES {
            let mut end = MAX_DIAGNOSTIC_MESSAGE_BYTES;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
            message.push('…');
        }
        self.seq += 1;
        if self.ring.len() >= MAX_DIAGNOSTICS_PER_NAMESPACE {
            self.ring.pop_front();
        }
        self.ring.push_back(DiagRecord {
            seq: self.seq,
            action,
            code,
            message,
        });
    }
}

/// The write half of the per-terminal diagnostics: resolves a stamped
/// [`IngressSource`] to its arrival seat's [`TerminalDiagnostics`] ring.
///
/// Resolution keys on [`crate::identity::TerminalId`] (the stamp rule,
/// #56 decision 17), so a recycled namespace can never capture a dead
/// terminal's in-flight rejections; a record whose arrival terminal no
/// longer exists is dropped with a `warn!`, never rerouted.
#[derive(SystemParam)]
pub struct DiagnosticsSink<'w, 's> {
    seats: Query<
        'w,
        's,
        (
            &'static crate::identity::TerminalIdentity,
            &'static mut TerminalDiagnostics,
        ),
    >,
}

impl DiagnosticsSink<'_, '_> {
    /// Records a rejection into the arrival terminal's ring.
    pub(crate) fn record(
        &mut self,
        source: IngressSource,
        action: &'static str,
        code: &'static str,
        message: String,
    ) {
        let terminal = source.terminal();
        let Some((_, mut diagnostics)) = self
            .seats
            .iter_mut()
            .find(|(identity, _)| identity.id() == terminal)
        else {
            warn!(
                "ratty-query: {action} diagnostic dropped: arrival terminal \
                 {terminal:?} no longer exists"
            );
            return;
        };
        diagnostics.record(action, code, message);
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
            payload: None,
        });
    }
}

/// Writes a commit ack qualified by an outcome code (e.g. `deferred`)
/// when the command opted in with `tok=`. The command committed — `ok=1`
/// — but with a qualification the caller should read; this is not an
/// error path and records no diagnostic.
pub(crate) fn ack_commit_qualified(
    acks: &mut MessageWriter<AckOutcome>,
    source: IngressSource,
    ack_token: &Option<String>,
    code: &'static str,
) {
    if let Some(token) = ack_token {
        acks.write(AckOutcome {
            source,
            token: token.clone(),
            ok: true,
            code: Some(code),
            payload: None,
        });
    }
}

/// Writes a commit ack carrying a structured `data=` payload but NO
/// status code — the shape an immediate-commit operation needs when its
/// result includes a value the caller cannot otherwise learn.
///
/// Distinct from [`ack_commit_long_running`], which forces a status
/// qualifier into `code=`. `term.spawn` needs exactly this and must NOT
/// use that one: `protocols/query.md` makes absence from
/// `state.executions` the completion signal, so `code=started` on a handle
/// deliberately kept out of that roster would tell a conforming caller the
/// spawn had FINISHED while it was still spawning (#56 decision 19).
/// Its only caller today is the native `term.spawn` path; the wasm build
/// refuses that verb (the page API owns lifecycle there), so the helper
/// stays compiled on both targets rather than drifting behind a `cfg` —
/// the same posture `spawn_focused_terminal` takes.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn ack_commit_with_payload(
    acks: &mut MessageWriter<AckOutcome>,
    source: IngressSource,
    ack_token: &Option<String>,
    payload: serde_json::Value,
) {
    if let Some(token) = ack_token {
        acks.write(AckOutcome {
            source,
            token: token.clone(),
            ok: true,
            code: None,
            payload: Some(payload),
        });
    }
}

/// Writes the single ack for an admitted long-running operation (#18):
/// `ok=1`, a status qualifier code ([`codes::STARTED`] / [`codes::QUEUED`]),
/// and a structured `data=` payload carrying the execution handle, queue
/// position, and estimated wait. Exactly one ack per command, emitted at
/// admission — completion is observed by polling `state.executions`, never
/// pushed (`t=e` stays reserved).
pub(crate) fn ack_commit_long_running(
    acks: &mut MessageWriter<AckOutcome>,
    source: IngressSource,
    ack_token: &Option<String>,
    status: &'static str,
    payload: serde_json::Value,
) {
    if let Some(token) = ack_token {
        acks.write(AckOutcome {
            source,
            token: token.clone(),
            ok: true,
            code: Some(status),
            payload: Some(payload),
        });
    }
}

/// Records a rejection diagnostic and, when the command opted in with
/// `tok=`, writes the matching error ack. Call this beside the existing
/// rejection `warn!`s — it supplements them, it never replaces them.
pub(crate) fn reject(
    diagnostics: &mut DiagnosticsSink,
    acks: &mut MessageWriter<AckOutcome>,
    source: IngressSource,
    ack_token: &Option<String>,
    action: &'static str,
    code: &'static str,
    message: String,
) {
    diagnostics.record(source, action, code, message);
    if let Some(token) = ack_token {
        acks.write(AckOutcome {
            source,
            token: token.clone(),
            ok: false,
            code: Some(code),
            payload: None,
        });
    }
}

/// The per-organ registries a query projection may read, bundled into one
/// [`SystemParam`] so [`answer_queries`] stays under the system-parameter
/// arity limit as organs accumulate.
#[derive(SystemParam)]
pub struct OrganRegistries<'w> {
    viz: Res<'w, crate::viz::VizRegistry>,
    sound: Res<'w, SoundState>,
    bookmarks: Res<'w, crate::bookmarks::BookmarkRegistry>,
    macros: Res<'w, crate::macros::MacroRegistry>,
    reactive: Res<'w, crate::reactive::ReactiveRegistry>,
    avatar: Res<'w, crate::avatar::AvatarState>,
    presence: Res<'w, crate::presence::PresenceRegistry>,
    terminals: Res<'w, crate::terminals::TerminalRoster>,
    config: Res<'w, crate::config::AppConfig>,
    time: Res<'w, Time>,
}

/// Answers queued OSC 778 queries and flushes command acks.
///
/// Ordered after `pump_pty_output` and every command-applying system so a
/// same-chunk "write then read" observes the write, and the ack for a
/// command precedes the reply to a query that followed it. Replies exit
/// through the ARRIVAL terminal's own [`TerminalRuntime::write_input`],
/// resolved by `TerminalId` from the request's stamped [`IngressSource`]
/// (the stamp rule) — never broadcast, never another seat's transport. An
/// ack or reply whose arrival terminal died is dropped with a warn.
#[allow(clippy::too_many_arguments)]
pub fn answer_queries(
    mut queries: MessageReader<QueryRequest>,
    mut acks: MessageReader<AckOutcome>,
    transports: Query<(
        &crate::identity::TerminalIdentity,
        &TerminalRuntime,
        &TerminalPlaneWarp,
        &TerminalInlineObjects,
    )>,
    session: Res<QuerySession>,
    registry: Res<crate::identity::TerminalRegistry>,
    seat_state: Query<(
        &crate::identity::TerminalIdentity,
        &TerminalDiagnostics,
        &crate::macros::TerminalMacros,
        &crate::reactive::TerminalReactive,
        &AiEffects,
    )>,
    presentation: Res<TerminalPresentation>,
    plane_view: Res<TerminalPlaneView>,
    stage_tween: Res<StageTween>,
    cursor: Res<CursorSettings>,
    organs: OrganRegistries,
) {
    // The reply transport is the arrival terminal's own runtime, resolved
    // per message by TerminalId (the stamp rule, #56 decision 17).
    let transport_of = |terminal: crate::identity::TerminalId| {
        transports
            .iter()
            .find(|(identity, ..)| identity.id() == terminal)
    };

    // The `state.terminals` rows, resolved once against the world: the
    // roster holds handle/creator/state, and the seat holds the namespace
    // and the live grid. `creator_ns` is deliberately resolved HERE rather
    // than stored — a creator's namespace recycles when it dies, so a
    // stored ordinal would re-parent orphans to strangers (the stamp
    // rule). A row whose seat has not flushed yet reports a null grid
    // rather than a guess.
    let terminal_rows: Vec<crate::terminals::TerminalRowSnapshot> = organs
        .terminals
        .iter()
        .filter_map(|(id, row)| {
            // The namespace is knowable without the seat entity — the
            // registry holds the lease — so a `spawning` row is still
            // fully addressable in the reply. A row with no lease is an
            // invariant violation (the sweep drops both together), so it
            // is reported and skipped, never defaulted to namespace 0.
            let Some(ns) = registry.namespace_of(id) else {
                warn!("answer_queries: roster row for {id:?} has no namespace lease; skipping");
                return None;
            };
            let seat = transport_of(id);
            let grid = seat.map(|(_, runtime, ..)| runtime.parser.screen().size());
            Some(crate::terminals::TerminalRowSnapshot {
                id,
                handle: row.handle.clone(),
                // Derived, so no observer can disagree with the world:
                // a row whose seat has not flushed reads `spawning`.
                state: row.wire_state(seat.is_some()),
                ns,
                creator: row.creator,
                // Resolved NOW, never stored: a creator's namespace
                // returns to the pool when it dies, so a stored ordinal
                // would re-parent orphans to strangers (the stamp rule).
                creator_ns: row
                    .creator
                    .and_then(|creator| registry.namespace_of(creator)),
                cols: grid.map(|(_, cols)| cols),
                rows: grid.map(|(rows, _)| rows),
            })
        })
        .collect();

    // Acks first: a same-chunk "command with tok= then query" reads its
    // ack before the query reply, in mutation order.
    for AckOutcome {
        source,
        token,
        ok,
        code,
        payload,
    } in acks.read()
    {
        let Some((_, runtime, ..)) = transport_of(source.terminal()) else {
            warn!(
                "answer_queries: ack dropped: arrival terminal {:?} no longer exists",
                source.terminal()
            );
            continue;
        };
        let json = payload.as_ref().map(serde_json::Value::to_string);
        send_reply(
            runtime,
            *source,
            token,
            true,
            *ok,
            *code,
            json.as_deref().map(str::as_bytes),
        );
    }

    for QueryRequest { source, item } in queries.read() {
        let Some((_, runtime, plane_warp, inline_objects)) = transport_of(source.terminal()) else {
            warn!(
                "answer_queries: query dropped: arrival terminal {:?} no longer exists",
                source.terminal()
            );
            continue;
        };
        match item {
            QueryItem::Error(error) => {
                send_reply(
                    runtime,
                    *source,
                    &error.token,
                    error.ack,
                    false,
                    Some(error.code),
                    None,
                );
            }
            QueryItem::Query(envelope) => {
                // The caller's own session-half state (diagnostics ring,
                // session macros), resolved by TerminalId (the stamp rule)
                // — a query whose arrival terminal died is dropped loudly,
                // never answered from another seat's state.
                let Some((_, seat_diagnostics, seat_macros, seat_reactive, seat_effects)) =
                    seat_state
                        .iter()
                        .find(|(identity, ..)| identity.id() == source.terminal())
                else {
                    warn!(
                        "answer_queries: query dropped: arrival terminal {:?} no longer exists",
                        source.terminal()
                    );
                    continue;
                };
                let ctx = QueryCtx {
                    session: &session,
                    inline_objects,
                    diagnostics: seat_diagnostics,
                    seat_macros,
                    seat_reactive,
                    presentation: &presentation,
                    plane_warp,
                    plane_view: &plane_view,
                    stage_tween: &stage_tween,
                    cursor: &cursor,
                    // The ARRIVAL terminal's own effects (#56 decision 14's
                    // read side): a mood that does not render — unfocused
                    // under focused-wash — is still observable.
                    effects: seat_effects,
                    viz: &organs.viz,
                    sound: &organs.sound,
                    bookmarks: &organs.bookmarks,
                    macros: &organs.macros,
                    reactive: &organs.reactive,
                    avatar: &organs.avatar,
                    presence: &organs.presence,
                    config: &organs.config,
                    now: organs.time.elapsed(),
                    grid: runtime.parser.screen().size(),
                    terminals: &terminal_rows,
                };
                match answer(envelope, *source, &ctx) {
                    Ok(value) => {
                        let payload = value.to_string();
                        send_reply(
                            runtime,
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
                            runtime,
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
        IngressSource::Local(_) => runtime.write_input(&bytes),
    }
}

/// Borrowed view of everything a query op may read.
struct QueryCtx<'a> {
    session: &'a QuerySession,
    inline_objects: &'a TerminalInlineObjects,
    diagnostics: &'a TerminalDiagnostics,
    /// The caller's session-half macro state (its session registry and
    /// active slot); the trusted half stays in `macros`.
    seat_macros: &'a crate::macros::TerminalMacros,
    /// The caller's session-half reactive state (its wire rules and
    /// sensors); the trusted half stays in `reactive`.
    seat_reactive: &'a crate::reactive::TerminalReactive,
    presentation: &'a TerminalPresentation,
    plane_warp: &'a TerminalPlaneWarp,
    plane_view: &'a TerminalPlaneView,
    stage_tween: &'a StageTween,
    cursor: &'a CursorSettings,
    effects: &'a AiEffects,
    viz: &'a crate::viz::VizRegistry,
    sound: &'a SoundState,
    bookmarks: &'a crate::bookmarks::BookmarkRegistry,
    macros: &'a crate::macros::MacroRegistry,
    reactive: &'a crate::reactive::ReactiveRegistry,
    avatar: &'a crate::avatar::AvatarState,
    presence: &'a crate::presence::PresenceRegistry,
    /// Trusted config, for the `caps` capability-grant projection.
    config: &'a crate::config::AppConfig,
    /// `Time::elapsed` at answer time, for sensor freshness projections.
    now: std::time::Duration,
    /// Live grid size as `(rows, cols)`, from the parser screen.
    grid: (u16, u16),
    /// Every live terminal, resolved against the world for
    /// `state.terminals`.
    terminals: &'a [crate::terminals::TerminalRowSnapshot],
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
        "caps" => Ok(caps(ctx, source)),
        "state.scene" => Ok(scene_state(ctx)),
        "state.objects" => own_objects(ctx, source, &data),
        "state.visible_objects" => visible_objects(ctx, &data),
        "state.neighbors" => neighbors(ctx, source, &data),
        "state.namespaces" => Ok(namespaces(ctx)),
        // The caller's session macros plus the trusted macros (paginated);
        // and the caller's own active recording/playback.
        "state.macros" => paginate(
            ctx,
            crate::macros::macros_state_items(ctx.seat_macros, ctx.macros),
            &data,
        ),
        // The caller's own executions: the macro slot plus the caller's
        // avatar utterances (own active and own queued). Private
        // per-agent; absence of a handle is the completion signal (#18).
        "state.executions" => {
            let mut value = crate::macros::executions_state_value(ctx.seat_macros);
            if let Some(items) = value["items"].as_array_mut() {
                items.extend(
                    ctx.avatar
                        .speech
                        .execution_items(source.namespace(), ctx.now),
                );
            }
            Ok(value)
        }
        "state.errors" => errors(ctx, source, &data),
        "state.viz" => viz_state(ctx, source, &data),
        "state.bookmarks" => Ok(bookmarks_state(ctx, source)),
        // The caller's wire rules plus the trusted rules, and the system
        // sensors plus the caller's own wire sensors (both paginated).
        "state.rules" => paginate(
            ctx,
            crate::reactive::rules_state_items(ctx.seat_reactive, ctx.reactive, ctx.now),
            &data,
        ),
        "state.sensors" => paginate(
            ctx,
            crate::reactive::sensors_state_items(ctx.seat_reactive, ctx.reactive, ctx.now),
            &data,
        ),
        // The collaboration-presence rosters (#25), paginated: rosters
        // cannot ride state.namespaces — one maxed namespace row would
        // blow the page budget, and that op is unpaginated. Three-tier
        // scoped: the caller's own namespace in full including expired
        // rows; foreign namespaces fresh-only.
        "state.presence" => paginate(
            ctx,
            crate::presence::presence_state_items(ctx.presence, source.namespace(), ctx.now),
            &data,
        ),
        // The terminal roster (#49). Paginated even though the live cap
        // defaults to 4: `state.namespaces` is this file's own cautionary
        // note about unpaginated ops, and the cap is configurable to 128.
        "state.terminals" => paginate(
            ctx,
            crate::terminals::terminals_state_items(ctx.terminals, source),
            &data,
        ),
        _ => Err(codes::UNSUPPORTED_OP),
    }
}

/// `caps`: protocol discovery — the 778 analog of the RGP support reply.
/// Keys are append-only so older clients keep parsing newer replies.
fn caps(ctx: &QueryCtx<'_>, source: IngressSource) -> Value {
    json!({
        "v": 1,
        "session": ctx.session.nonce_hex(),
        "ops": SUPPORTED_OPS,
        // The #57 pane-0 contract: the widget renders exactly one grid, and
        // any future pane-addressed content MUST degrade to it. Hosts
        // introspect this key instead of inferring from behavior; it grows
        // only when a multi-grid browser story actually ships (#86).
        //
        // Terminals are NOT panes — that is the whole #22 ruling — so N
        // live terminals leave this at 1. The risk here is a well-meaning
        // change, not a deliberate one; the milestone test asserts it
        // still reads 1 with two terminals live.
        "panes": 1,
        "ack": { "key": ACK_TOKEN_KEY },
        "limits": {
            "max_query_bytes": query::MAX_QUERY_SEQUENCE_BYTES,
            "max_query_data_bytes": query::MAX_QUERY_DATA_BYTES,
            "max_reply_bytes": query::MAX_REPLY_SEQUENCE_BYTES,
            "objects_per_namespace": crate::ai::MAX_AI_OBJECTS_PER_NAMESPACE,
            "ids_per_session": crate::ai::MAX_AI_OBJECT_IDS_PER_SESSION,
            "errors_per_namespace": MAX_DIAGNOSTICS_PER_NAMESPACE,
            "viz_per_namespace": crate::viz::MAX_VIZ_PER_NAMESPACE,
            "viz_payload_bytes": crate::viz::MAX_VIZ_PAYLOAD_BYTES,
            "viz_items": crate::viz::MAX_VIZ_ITEMS_PER_SNAPSHOT,
            "sound_voices": crate::sound::MAX_SOUND_VOICES,
            "sound_plays_per_sec": crate::sound::SOUND_PLAYS_PER_SEC,
            "terminal_spawns_per_sec": crate::identity::TERMINAL_SPAWNS_PER_SEC,
            "terminal_focus_per_sec": crate::identity::TERMINAL_FOCUS_PER_SEC,
            "terminal_min_axis": crate::identity::MIN_TERMINAL_AXIS,
            "terminal_max_axis": crate::identity::MAX_TERMINAL_AXIS,
            "terminal_max_cells": crate::identity::MAX_TERMINAL_CELLS,
            "viz_series": crate::viz::MAX_VIZ_SERIES_PER_SNAPSHOT,
            "viz_points_per_series": crate::viz::MAX_VIZ_POINTS_PER_SERIES,
            "viz_points": crate::viz::MAX_VIZ_POINTS_PER_SNAPSHOT,
            "bookmarks_per_namespace": crate::bookmarks::MAX_BOOKMARKS_PER_NAMESPACE,
            "bookmark_name_bytes": crate::bookmarks::MAX_BOOKMARK_NAME_BYTES,
            "macros_per_namespace": crate::macros::MAX_MACROS_PER_NAMESPACE,
            "macro_name_bytes": crate::macros::MAX_MACRO_NAME_BYTES,
            "commands_per_macro": crate::macros::MAX_COMMANDS_PER_MACRO,
            "macro_recording_secs": crate::macros::MAX_RECORDING_SECS,
            "macro_playback_per_frame": crate::macros::MAX_PLAYBACK_COMMANDS_PER_FRAME,
            "rules_per_namespace": crate::reactive::MAX_RULES_PER_NAMESPACE,
            "rule_name_bytes": crate::reactive::MAX_RULE_NAME_BYTES,
            "rule_fires_per_frame": crate::reactive::MAX_RULE_FIRES_PER_FRAME,
            "sensors_per_namespace": crate::reactive::MAX_SENSORS_PER_NAMESPACE,
            "sensor_name_bytes": crate::reactive::MAX_SENSOR_SUFFIX_BYTES,
            "sensor_publishes_per_sec": crate::reactive::SENSOR_PUBLISHES_PER_SEC,
            "sensor_default_ttl_secs": crate::reactive::DEFAULT_SENSOR_TTL_SECS,
            "avatar_text_bytes": crate::avatar::MAX_AVATAR_TEXT_BYTES,
            "avatar_speaker_bytes": crate::avatar::MAX_AVATAR_SPEAKER_BYTES,
            "avatar_utterance_min_ms": crate::avatar::MIN_UTTERANCE_MS,
            "avatar_utterance_max_ms": crate::avatar::MAX_UTTERANCE_MS,
            "avatar_queue_global": crate::avatar::MAX_PENDING_UTTERANCES_GLOBAL,
            "avatar_queue_per_agent": crate::avatar::MAX_PENDING_UTTERANCES_PER_AGENT,
            "avatar_offset_max_px": crate::avatar::AVATAR_OFFSET_MAX_PX,
            "presence_participants_per_namespace":
                crate::presence::MAX_PRESENCE_PARTICIPANTS_PER_NAMESPACE,
            "presence_notes_per_namespace": crate::presence::MAX_PRESENCE_NOTES_PER_NAMESPACE,
            "presence_id_bytes": crate::presence::MAX_PRESENCE_ID_BYTES,
            "presence_name_bytes": crate::presence::MAX_PRESENCE_NAME_BYTES,
            "presence_note_text_bytes": crate::presence::MAX_PRESENCE_NOTE_TEXT_BYTES,
            "presence_default_ttl_secs": crate::presence::DEFAULT_PRESENCE_TTL_SECS,
            "presence_note_default_ttl_secs": crate::presence::DEFAULT_PRESENCE_NOTE_TTL_SECS,
            "presence_min_ttl_secs": crate::presence::MIN_PRESENCE_TTL_SECS,
            "presence_max_ttl_secs": crate::presence::MAX_PRESENCE_TTL_SECS,
        },
        "viz_kinds": crate::viz::REGISTERED_VIZ_KINDS,
        // The terminals organ (#49), append-only beside `viz_kinds`.
        // `spawn_fields` and `place_fields` are the honesty contract: they
        // name exactly which payload keys the applier will act on, so a
        // caller learns the geometry refusal from `caps` rather than from
        // an `unsupported` ack. Empty means "this verb takes no fields".
        "terminals": {
            "live": ctx.terminals.len(),
            "max": crate::identity::max_live_terminals(&ctx.config.terminal),
            "pool": crate::identity::MAX_LIVE_TERMINALS,
            "verbs": ["spawn", "place", "focus", "close"],
            "spawn_fields": [],
            "place_fields": ["cols", "rows"],
        },
        "avatar_models": crate::osc::AVATAR_MODELS,
        // #23 honesty: the scene-level capabilities THIS caller's ingress
        // tier carries, derived from trusted config — discoverable before
        // attempting, never a promise of anything else.
        "trust": {
            "avatar_scene": crate::capability::SceneCapability::AvatarScene
                .granted_to(source, ctx.config),
            "scene_ambient": crate::capability::SceneCapability::SceneAmbient
                .granted_to(source, ctx.config),
            // Both default DENY (#49): a caller must be able to read the
            // grant before attempting the verb, so a refusal is never a
            // surprise.
            "terminal_lifecycle": crate::capability::SceneCapability::TerminalLifecycle
                .granted_to(source, ctx.config),
            "terminal_focus": crate::capability::SceneCapability::TerminalFocus
                .granted_to(source, ctx.config),
        },
        // #18 honesty: whether the config-gated native sensor adapter is
        // active in this process (always false on wasm), and the sensors
        // it is currently supplying. Both are live truth, never a promise.
        "sensors": {
            "system_adapter": ctx.reactive.system_sensors_enabled(),
            "system": ctx.reactive.live_system_sensors(),
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
    let audio = ctx.sound.public_state();
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
        // Append-only (M3.9): the sound organ's public state. Feature-off
        // builds report `enabled: false` honestly — the key shape is
        // feature-independent. Unlock status is polled here, never pushed.
        "audio": {
            "enabled": audio.enabled,
            "unlocked": audio.unlocked,
            "ambient": {
                "kind": audio.ambient_kind,
                "phase": audio.ambient_phase,
            },
            "voices": audio.voices,
        },
        // Append-only (M3.10): the avatar organ's public state (#23 §2's
        // five fields plus placement). No utterance text ever appears
        // here — text is owner-scoped to `state.executions`.
        "avatar": ctx.avatar.public_scene(),
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
fn neighbors(
    ctx: &QueryCtx<'_>,
    source: IngressSource,
    data: &Value,
) -> Result<Value, &'static str> {
    let radius = data
        .get("radius")
        .and_then(Value::as_f64)
        .filter(|r| r.is_finite() && *r > 0.0 && *r <= 65_535.0)
        .ok_or(codes::BAD_PAYLOAD)?;
    let (rows, _) = ctx.grid;

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
        // Read scope: the caller may center on its own objects in any
        // state, but a foreign object's position is public only while it
        // is visible — and a hidden foreign object's very existence is
        // not readable, so anything else answers a flat unknown-id (never
        // a distinguishable exists-but-hidden state).
        let owned = ai_object_namespace(id) == Some(source.namespace());
        let anchor = ctx.inline_objects.anchors.get(&id);
        if owned {
            if !ctx.inline_objects.objects.contains_key(&id) {
                return Err(codes::UNKNOWN_ID);
            }
        } else {
            let visible = ctx.inline_objects.objects.contains_key(&id)
                && anchor.is_some_and(|anchor| anchor_visible(anchor, rows));
            if !visible {
                return Err(codes::UNKNOWN_ID);
            }
        }
        let anchor = anchor.ok_or(codes::NO_ANCHOR)?;
        (
            f64::from(anchor.row) + f64::from(anchor.rows) / 2.0,
            f64::from(anchor.col) + f64::from(anchor.columns) / 2.0,
            Some(id),
        )
    } else {
        return Err(codes::BAD_PAYLOAD);
    };
    let (center_row, center_col, center_id) = center;

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
/// agent namespace plus the transmission/system partition, and (#25,
/// append-only) fresh collaboration participant/note counts. Presence
/// counts are fresh rows only — public = rendered, so an expired row is
/// not publicly visible here — and a namespace appears only when it has
/// something public (objects or fresh presence): expired-only foreign
/// namespaces never leak through the aggregate. Rosters ride the
/// paginated `state.presence`, never this unpaginated op.
fn namespaces(ctx: &QueryCtx<'_>) -> Value {
    let mut per_namespace: HashMap<u8, (usize, usize, usize)> = HashMap::new();
    let mut transmission = 0_usize;
    for id in ctx.inline_objects.objects.keys() {
        match ai_object_namespace(*id) {
            Some(namespace) => per_namespace.entry(namespace).or_default().0 += 1,
            None => transmission += 1,
        }
    }
    for (namespace, (participants, notes)) in ctx.presence.fresh_counts(ctx.now) {
        let entry = per_namespace.entry(namespace).or_default();
        entry.1 = participants;
        entry.2 = notes;
    }
    let mut namespaces: Vec<_> = per_namespace.into_iter().collect();
    namespaces.sort_by_key(|(namespace, _)| *namespace);
    json!({
        "transmission": transmission,
        "namespaces": namespaces
            .into_iter()
            .map(|(namespace, (objects, participants, notes))| json!({
                "ns": namespace,
                "objects": objects,
                "participants": participants,
                "notes": notes,
            }))
            .collect::<Vec<_>>(),
    })
}

/// `state.errors`: the caller's own rejection diagnostics, oldest first.
/// Sorted by sequence number; paginated.
fn errors(ctx: &QueryCtx<'_>, _source: IngressSource, data: &Value) -> Result<Value, &'static str> {
    // `ctx.diagnostics` IS the caller's own ring: it was resolved from the
    // arrival terminal's seat, so no namespace lookup exists to get wrong.
    let items: Vec<(u64, Value)> = ctx
        .diagnostics
        .ring
        .iter()
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
        .collect();
    paginate(ctx, items, data)
}

/// The viz visibility rule, mirroring [`anchor_visible`]: anchored and the
/// footprint's row range intersects the live grid.
fn viz_anchor_visible(anchor: &crate::viz::VizAnchor, grid_rows: u16) -> bool {
    let start = i32::from(anchor.row);
    let end = start + i32::from(anchor.rows);
    start < i32::from(grid_rows) && end > 0
}

/// `state.viz`: visualization records under the three-tier read scope —
/// the caller's own records in full (capture provenance plus effect-queue
/// length), foreign namespaces' public projections only while visible (a
/// hidden foreign visualization's existence is not readable). Payload
/// read-back is deliberately summary-level in v1: `item_count`, never
/// item dumps or raw payloads. Sorted by id; paginated.
fn viz_state(
    ctx: &QueryCtx<'_>,
    source: IngressSource,
    data: &Value,
) -> Result<Value, &'static str> {
    let (rows, _) = ctx.grid;
    let namespace = source.namespace();
    let mut items: Vec<(u64, Value)> = ctx
        .viz
        .iter()
        .filter_map(|(id, entry)| {
            let owned = ai_object_namespace(id) == Some(namespace);
            let visible = entry
                .anchor
                .is_some_and(|anchor| viz_anchor_visible(&anchor, rows));
            if !owned && !visible {
                return None;
            }
            let mut value = json!({
                "id": id,
                "owner": ai_object_namespace(id),
                "kind": entry.payload.kind(),
                "revision": entry.revision,
                "visible": visible,
                "anchor": entry.anchor.map_or(Value::Null, |anchor| json!({
                    "row": anchor.row,
                    "col": anchor.col,
                    "cols": anchor.cols,
                    "rows": anchor.rows,
                })),
                "item_count": entry.payload.item_count(),
            });
            if owned {
                let capture = entry.payload.capture();
                value["capture"] = json!({
                    "source": capture.source,
                    "ts": capture.ts,
                });
                value["pending_effects"] = json!(entry.pending_effects.len());
            }
            Some((u64::from(id), value))
        })
        .collect();
    items.sort_by_key(|(key, _)| *key);
    paginate(ctx, items, data)
}

/// `state.bookmarks`: the caller's own view bookmarks, by name. Bookmarks
/// live in the caller's session namespace and are never projected to
/// other callers — there is no foreign-visibility tier and no pagination
/// (the per-namespace cap keeps the reply pages under budget).
fn bookmarks_state(ctx: &QueryCtx<'_>, source: IngressSource) -> Value {
    let mut items: Vec<(&str, Value)> = ctx
        .bookmarks
        .iter_namespace(source.namespace())
        .map(|(name, bookmark)| {
            (
                name,
                json!({
                    "name": name,
                    "v": bookmark.v,
                    "mode": bookmark.mode,
                    "warp": bookmark.warp,
                }),
            )
        })
        .collect();
    items.sort_by_key(|(name, _)| *name);
    json!({ "items": items.into_iter().map(|(_, value)| value).collect::<Vec<_>>() })
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
    use crate::osc::RattyAiCommand;
    use crate::query::{ParsedReply, ReplyScanner, parse_reply_body, query_sequence};
    use crate::runtime::VirtualTerminalHost;
    use crate::scene::TerminalPresentationMode;
    use crate::sound::apply_sound_commands;
    use crate::systems::pump_pty_output;
    use crate::terminal::TerminalRedrawState;

    /// A headless app wired exactly like the real pipeline: virtual
    /// transport → pump → object handler → sound handler → query answerer,
    /// chained so one `update()` is one closed loop.
    fn test_app() -> (App, VirtualTerminalHost) {
        let config = AppConfig::default();
        let (runtime, host) = TerminalRuntime::virtual_channel(&config, IngressSource::test_boot());
        let mut app = App::new();
        app.insert_resource(config);
        // The terminal seat, mirroring main()/setup_scene's spawns for the
        // components this harness needs.
        let seat = app
            .world_mut()
            .spawn((
                TerminalInlineObjects::default(),
                TerminalPlaneWarp::default(),
                TerminalRedrawState::default(),
                runtime,
                crate::identity::TerminalIdentity::test_boot(),
                crate::identity::terminal_session_state(),
            ))
            .id();
        // The boot seat as the real spawner would leave it: its lease
        // taken and bound, and a wire row carrying a minted handle. A
        // fresh registry's first allocation IS `test_boot()` (id 1,
        // namespace 0), so the two agree.
        app.init_resource::<QuerySession>();
        let mut registry = crate::identity::TerminalRegistry::default();
        let identity = registry.allocate().expect("a fresh registry has slots");
        registry
            .bind(identity.id(), seat)
            .expect("the lease is live");
        app.insert_resource(registry);
        let handle = app
            .world_mut()
            .resource_mut::<QuerySession>()
            .mint_execution_id();
        let mut roster = crate::terminals::TerminalRoster::default();
        roster.insert(identity.id(), handle, None, false);
        app.insert_resource(roster);
        app.init_resource::<AiObjectRegistry>();
        app.init_resource::<CursorSettings>();
        app.init_resource::<QuerySession>();
        app.init_resource::<crate::viz::VizRegistry>();
        app.init_resource::<SoundState>();
        app.init_resource::<crate::bookmarks::BookmarkRegistry>();
        app.init_resource::<crate::macros::MacroRegistry>();
        app.init_resource::<crate::avatar::AvatarState>();
        app.init_resource::<crate::config::AppConfig>();
        app.init_resource::<Time>();
        app.insert_resource(TerminalPresentation {
            mode: TerminalPresentationMode::Flat2d,
        });
        app.init_resource::<TerminalPlaneView>();
        app.init_resource::<StageTween>();
        app.add_message::<AppExit>();
        app.add_message::<AiCommand>();
        app.add_message::<AiObjectRemoved>();
        app.add_message::<QueryRequest>();
        app.add_message::<AckOutcome>();
        app.init_resource::<crate::bookmarks::PendingBookmarkJumps>();
        app.init_resource::<crate::reactive::ReactiveRegistry>();
        app.init_resource::<crate::presence::PresenceRegistry>();
        app.add_systems(
            Update,
            (
                pump_pty_output,
                crate::macros::apply_macro_commands,
                crate::macros::drive_macro_playback,
                crate::reactive::apply_reactive_commands,
                crate::reactive::evaluate_rules,
                apply_ai_object_commands,
                crate::viz::apply_viz_commands,
                apply_sound_commands,
                crate::effects::apply_ai_effect_commands,
                crate::bookmarks::apply_bookmark_commands,
                crate::bookmarks::drain_bookmark_jumps,
                crate::avatar::drive_avatar_speech,
                crate::avatar::apply_avatar_commands,
                crate::presence::apply_presence_commands,
                answer_queries,
            )
                .chain(),
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

    /// Adds a second seat the way the real spawner would: a lease from
    /// the app's own registry, bound to the seat, plus a roster row with a
    /// minted handle. Returns its identity, transport host and handle.
    fn add_seat(
        app: &mut App,
        creator: Option<crate::identity::TerminalId>,
    ) -> (
        crate::identity::TerminalIdentity,
        VirtualTerminalHost,
        String,
    ) {
        let identity = app
            .world_mut()
            .resource_mut::<crate::identity::TerminalRegistry>()
            .allocate()
            .expect("the test pool is nowhere near 128 seats");
        let (runtime, host) = TerminalRuntime::virtual_channel(
            &crate::config::AppConfig::default(),
            identity.ingress(),
        );
        let seat = app
            .world_mut()
            .spawn((
                TerminalInlineObjects::default(),
                TerminalPlaneWarp::default(),
                TerminalRedrawState::default(),
                runtime,
                identity,
                crate::identity::terminal_session_state(),
            ))
            .id();
        app.world_mut()
            .resource_mut::<crate::identity::TerminalRegistry>()
            .bind(identity.id(), seat)
            .expect("the just-allocated lease is live");
        let handle = app
            .world_mut()
            .resource_mut::<QuerySession>()
            .mint_execution_id();
        app.world_mut()
            .resource_mut::<crate::terminals::TerminalRoster>()
            .insert(identity.id(), handle.clone(), creator, creator.is_some());
        (identity, host, handle)
    }

    /// `state.terminals` enumerates every live terminal as tier-1
    /// scene-global state — the quads are visibly on screen — with the
    /// live grid resolved from each seat's own parser.
    #[test]
    fn state_terminals_lists_every_seat_with_its_live_grid() {
        let (mut app, host) = test_app();
        let reply = run_query(&mut app, &host, "q1", "state.terminals", None);
        assert!(reply.ok);
        let items = payload(&reply)["items"]
            .as_array()
            .expect("items array")
            .clone();
        assert_eq!(items.len(), 1, "the boot seat");
        assert_eq!(items[0]["state"], json!("ready"));
        assert_eq!(items[0]["ns"], json!(0), "the boot seat leases namespace 0");
        assert!(
            items[0]["id"].as_str().is_some_and(|id| !id.is_empty()),
            "every terminal is addressable by handle"
        );
        // Placement is reported as live truth, not as a promise: nothing
        // in this build renders a per-terminal position or scale.
        assert_eq!(items[0]["x"], json!(0.0));
        assert_eq!(items[0]["scale"], json!(1.0));
        assert!(
            items[0]["cols"].as_u64().is_some(),
            "the grid comes from the seat's own parser screen"
        );

        let (id_b, _host_b, handle_b) = add_seat(&mut app, None);
        assert_eq!(
            app.world_mut()
                .query::<&crate::identity::TerminalIdentity>()
                .iter(app.world())
                .count(),
            2,
            "seat count asserted (#58 rider)"
        );
        let reply = run_query(&mut app, &host, "q2", "state.terminals", None);
        let items = payload(&reply)["items"]
            .as_array()
            .expect("items array")
            .clone();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1]["id"], json!(handle_b), "rows list in mint order");
        assert_eq!(items[1]["ns"], json!(id_b.namespace()));
    }

    /// `creator` is the one own-scoped field (#56 decision 15), and its
    /// value is the creator's namespace ordinal. Foreign queriers see the
    /// key absent — never null, which would be a distinguishable
    /// "exists but hidden" marker.
    #[test]
    fn the_creator_field_is_own_scoped_and_never_leaks_under_another_key() {
        let (mut app, host_a) = test_app();
        let boot = crate::identity::TerminalIdentity::test_boot().id();
        let (_id_b, host_b, handle_b) = add_seat(&mut app, Some(boot));
        let (_id_c, host_c, _handle_c) = add_seat(&mut app, None);

        // A created B, so A sees the creator field on B's row.
        let reply = run_query(&mut app, &host_a, "qa", "state.terminals", None);
        let items = payload(&reply)["items"].as_array().expect("items").clone();
        let row_b = items
            .iter()
            .find(|row| row["id"] == json!(handle_b))
            .expect("B is listed");
        assert_eq!(
            row_b["creator"],
            json!(0),
            "the creator's namespace ordinal, resolved live"
        );

        // C did not create B, so the key is absent from C's view.
        let reply = run_query(&mut app, &host_c, "qc", "state.terminals", None);
        let items = payload(&reply)["items"].as_array().expect("items").clone();
        let row_b = items
            .iter()
            .find(|row| row["id"] == json!(handle_b))
            .expect("B is still listed — the ROW is public, only creator is scoped");
        assert!(
            row_b.get("creator").is_none(),
            "absent when foreign, never a null that says 'someone owns this'"
        );

        // And it never rides any other key, for anyone: the namespace is
        // a stable enumerable address, so a second spelling would defeat
        // the scoping entirely.
        for (host, token) in [(&host_a, "qa2"), (&host_b, "qb2"), (&host_c, "qc2")] {
            let reply = run_query(&mut app, host, token, "state.terminals", None);
            let items = payload(&reply)["items"].as_array().expect("items").clone();
            for row in &items {
                assert!(
                    row.get("creator_ns").is_none(),
                    "creator_ns is a snapshot field, never a wire key"
                );
            }
        }
    }

    const ID: u32 = 0x8000_0001;

    /// The M4.4 reply-routing criterion (declared in PR #91): at N=2,
    /// acks and replies exit over the ARRIVAL terminal's own transport,
    /// resolved by TerminalId — never the other seat's, never broadcast.
    #[test]
    fn replies_route_over_the_arrival_terminals_transport_at_n2() {
        let (mut app, host_a) = test_app();
        let mut registry = crate::identity::TerminalRegistry::default();
        let _boot = registry.allocate().expect("boot lease");
        let id_b = registry.allocate().expect("second lease");
        let (runtime_b, host_b) =
            TerminalRuntime::virtual_channel(&crate::config::AppConfig::default(), id_b.ingress());
        app.world_mut().spawn((
            TerminalInlineObjects::default(),
            TerminalPlaneWarp::default(),
            TerminalRedrawState::default(),
            runtime_b,
            id_b,
            crate::identity::terminal_session_state(),
        ));
        assert_eq!(
            app.world_mut()
                .query::<&crate::identity::TerminalIdentity>()
                .iter(app.world())
                .count(),
            2,
            "seat count asserted (#58 rider)"
        );

        // The closed loop over B: a tok='d effect command plus a query in
        // one chunk — B's transport carries B's ack and reply, A's stays
        // silent.
        let chunk = format!(
            "\x1b]777;ratty:think;state=start&tok=tb\x07{}",
            query_sequence("qb", "state.scene", None)
        );
        host_b
            .feed_tx
            .send(chunk.into_bytes())
            .expect("virtual feed accepts bytes");
        app.update();
        let replies_b = drain_replies(&host_b);
        assert_eq!(replies_b.len(), 2, "B gets its ack and its reply");
        assert!(replies_b[0].ack && replies_b[0].ok, "the think committed");
        assert_eq!(replies_b[1].token, "qb");
        assert_eq!(
            payload(&replies_b[1])["effects"]["thinking"],
            json!(true),
            "state.scene projects the ARRIVAL terminal's own effects"
        );
        assert!(
            drain_replies(&host_a).is_empty(),
            "A's transport carries none of B's traffic"
        );

        // Symmetric: A's query answers over A, and A's own effects are
        // untouched by B's think.
        let reply_a = run_query(&mut app, &host_a, "qa", "state.scene", None);
        assert!(reply_a.ok);
        assert_eq!(
            payload(&reply_a)["effects"]["thinking"],
            json!(false),
            "A's projection is A's state, not B's"
        );
        assert!(
            drain_replies(&host_b).is_empty(),
            "B's transport carries none of A's traffic"
        );
    }

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
        // The #57 pane-0 contract key: exactly one rendered grid until the
        // browser fork (#86) ships more.
        assert_eq!(caps["panes"], json!(1));
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
        assert_eq!(
            caps["limits"]["viz_per_namespace"],
            json!(crate::viz::MAX_VIZ_PER_NAMESPACE)
        );
        assert_eq!(
            caps["limits"]["viz_payload_bytes"],
            json!(crate::viz::MAX_VIZ_PAYLOAD_BYTES)
        );
        assert_eq!(
            caps["limits"]["viz_items"],
            json!(crate::viz::MAX_VIZ_ITEMS_PER_SNAPSHOT)
        );
        assert_eq!(
            caps["limits"]["rules_per_namespace"],
            json!(crate::reactive::MAX_RULES_PER_NAMESPACE)
        );
        assert_eq!(
            caps["limits"]["sensors_per_namespace"],
            json!(crate::reactive::MAX_SENSORS_PER_NAMESPACE)
        );
        assert_eq!(
            caps["limits"]["rule_fires_per_frame"],
            json!(crate::reactive::MAX_RULE_FIRES_PER_FRAME)
        );
        assert_eq!(
            caps["limits"]["sensor_publishes_per_sec"],
            json!(crate::reactive::SENSOR_PUBLISHES_PER_SEC)
        );
        assert_eq!(
            caps["limits"]["presence_participants_per_namespace"],
            json!(crate::presence::MAX_PRESENCE_PARTICIPANTS_PER_NAMESPACE)
        );
        assert_eq!(
            caps["limits"]["presence_notes_per_namespace"],
            json!(crate::presence::MAX_PRESENCE_NOTES_PER_NAMESPACE)
        );
        assert_eq!(
            caps["limits"]["presence_default_ttl_secs"],
            json!(crate::presence::DEFAULT_PRESENCE_TTL_SECS)
        );
        // #18 honesty: no config grant in the default test app, so the
        // native adapter reports absent and supplies nothing.
        assert_eq!(caps["sensors"]["system_adapter"], json!(false));
        assert_eq!(caps["sensors"]["system"], json!([]));
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
        let world = app.world_mut();
        let mut inline_query = world.query::<&mut TerminalInlineObjects>();
        let mut inline = inline_query
            .single_mut(world)
            .expect("exactly one terminal seat");
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
            let world = app.world_mut();
            let mut inline_query = world.query::<&mut TerminalInlineObjects>();
            let mut inline = inline_query
                .single_mut(world)
                .expect("exactly one terminal seat");
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
            let world = app.world_mut();
            let mut inline_query = world.query::<&mut TerminalInlineObjects>();
            let mut inline = inline_query
                .single_mut(world)
                .expect("exactly one terminal seat");
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
            let world = app.world_mut();
            let mut inline_query = world.query::<&mut TerminalInlineObjects>();
            let mut inline = inline_query
                .single_mut(world)
                .expect("exactly one terminal seat");
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
        assert_eq!(
            aggregate["namespaces"],
            json!([{ "ns": 0, "objects": 2, "participants": 0, "notes": 0 }])
        );
    }

    #[test]
    fn oversized_wire_strings_cannot_poison_the_error_ring() {
        let (mut app, host) = test_app();
        // A mode command whose positional is wire-controlled junk far over
        // the diagnostic cap — the stored message must truncate so
        // state.errors stays answerable. (SetMode is handled by
        // apply_ai_commands, which this test app does not register, so
        // record the rejection directly at the storage boundary.)
        let junk = "x".repeat(4096);
        {
            let world = app.world_mut();
            let mut seats = world.query::<&mut TerminalDiagnostics>();
            seats
                .single_mut(world)
                .expect("the scaffold seat carries its diagnostics ring")
                .record("mode", codes::BAD_MODE, format!("unknown mode '{junk}'"));
        }
        let reply = run_query(&mut app, &host, "q1", "state.errors", None);
        assert!(reply.ok, "the errors op survives an oversized message");
        let page = payload(&reply);
        let items = page["items"].as_array().expect("items");
        assert_eq!(items.len(), 1);
        let message = items[0]["message"].as_str().expect("message");
        assert!(message.len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES + '…'.len_utf8());
        assert!(message.ends_with('…'));
    }

    #[test]
    fn neighbors_center_scope_hides_foreign_hidden_objects() {
        let (mut app, host) = test_app();
        let foreign_id = 0x8100_0001; // namespace 1; the caller is namespace 0.
        {
            let world = app.world_mut();
            let mut inline_query = world.query::<&mut TerminalInlineObjects>();
            let mut inline = inline_query
                .single_mut(world)
                .expect("exactly one terminal seat");
            let object = || {
                InlineObject::RgpObject(RgpInlineObject::Gltf {
                    asset_path: "objects/x.glb".into(),
                    handle: None,
                })
            };
            // A foreign object anchored far off-grid: exists, not visible.
            inline.ai_insert_object(foreign_id, object(), 10, 500, InlineStyle::default());
            // The caller's own off-grid object.
            inline.ai_insert_object(ID, object(), 10, 500, InlineStyle::default());
        }
        // Foreign + hidden and foreign + never-existed are indistinguishable.
        for (token, id) in [("q1", u64::from(foreign_id)), ("q2", 0x8100_0002_u64)] {
            let reply = run_query(
                &mut app,
                &host,
                token,
                "state.neighbors",
                Some(json!({ "object": id, "radius": 5 })),
            );
            assert!(!reply.ok);
            assert_eq!(reply.code.as_deref(), Some(codes::UNKNOWN_ID));
        }
        // The caller's own hidden-but-anchored object is a usable center.
        let reply = run_query(
            &mut app,
            &host,
            "q3",
            "state.neighbors",
            Some(json!({ "object": ID, "radius": 5 })),
        );
        assert!(reply.ok, "own objects may center a neighbors query");
    }

    #[test]
    fn macros_and_executions_start_empty() {
        let (mut app, host) = test_app();
        for (token, op) in [("q1", "state.macros"), ("q2", "state.executions")] {
            let reply = run_query(&mut app, &host, token, op, None);
            assert!(reply.ok);
            assert_eq!(payload(&reply)["items"], json!([]));
        }
    }

    #[test]
    fn state_macros_lists_a_macro_recorded_over_the_wire() {
        let (mut app, host) = test_app();
        // Record a one-command macro over OSC 777: the bracket plus one
        // scene-global command tapped off the stream between them.
        let chunk = "\x1b]777;ratty:macro.record;name=deploy\x07\
             \x1b]777;ratty:mode;3d\x07\
             \x1b]777;ratty:macro.stop\x07";
        host.feed_tx
            .send(chunk.as_bytes().to_vec())
            .expect("virtual feed accepts bytes");
        app.update();

        let reply = run_query(&mut app, &host, "q1", "state.macros", None);
        assert!(reply.ok);
        let page = payload(&reply);
        let items = page["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1, "the recorded macro is listed");
        assert_eq!(items[0]["name"], json!("deploy"));
        assert_eq!(items[0]["scope"], json!("session"));
        assert_eq!(items[0]["commands"], json!(1));
        assert_eq!(
            items[0]["privileged"],
            json!(true),
            "a captured scene-global command marks the macro privileged"
        );
    }

    #[test]
    fn rules_and_sensors_start_empty() {
        let (mut app, host) = test_app();
        for (token, op) in [("q1", "state.rules"), ("q2", "state.sensors")] {
            let reply = run_query(&mut app, &host, token, op, None);
            assert!(reply.ok);
            assert_eq!(payload(&reply)["items"], json!([]));
        }
    }

    /// The M3.8 closed loop: register a rule and publish its sensor over
    /// OSC 777 in one chunk, watch the fire lower the same frame, and read
    /// the rule/sensor state back over OSC 778.
    #[test]
    fn closed_loop_rule_set_sensor_publish_fire_over_777_and_778() {
        let (mut app, host) = test_app();
        let chunk = "\x1b]777;ratty:rule.set;name=hot&sensor=agent.0.load&above=80&do=think&tok=r1\x07\
             \x1b]777;ratty:sensor.publish;name=load&value=95&tok=p1\x07";
        host.feed_tx
            .send(chunk.as_bytes().to_vec())
            .expect("virtual feed accepts bytes");
        app.update();
        let replies = drain_replies(&host);
        assert_eq!(replies.len(), 2, "one ack per tok= command");
        assert!(replies[0].ok && replies[0].ack, "rule.set commits");
        assert!(replies[1].ok && replies[1].ack, "sensor.publish commits");
        // The transition fired `think` and the effects applier lowered it
        // in the same frame — onto the arrival seat's own effects
        // component (decision 14's routing).
        assert!(
            app.world_mut()
                .query::<&AiEffects>()
                .single(app.world())
                .expect("exactly one seat carries effects")
                .public_state()
                .thinking,
            "the fired action lowered the same frame"
        );

        let reply = run_query(&mut app, &host, "q1", "state.rules", None);
        assert!(reply.ok);
        let page = payload(&reply);
        let items = page["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1);
        let rule = &items[0];
        assert_eq!(rule["name"], json!("hot"));
        assert_eq!(rule["scope"], json!("session"));
        assert_eq!(rule["sensor"], json!("agent.0.load"));
        assert_eq!(rule["action"], json!("think"));
        assert_eq!(rule["bound"], json!(true));
        assert_eq!(rule["dormant"], json!(false));
        assert_eq!(rule["active"], json!(true));
        assert_eq!(rule["fires"], json!(1));

        let reply = run_query(&mut app, &host, "q2", "state.sensors", None);
        assert!(reply.ok);
        let page = payload(&reply);
        let items = page["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1);
        let sensor = &items[0];
        assert_eq!(sensor["name"], json!("agent.0.load"));
        assert_eq!(sensor["value"], json!(95.0));
        assert_eq!(sensor["seq"], json!(1));
        assert_eq!(sensor["fresh"], json!(true));
        assert_eq!(sensor["source"], json!("wire"));
        assert_eq!(sensor["rules"], json!(1));
    }

    /// A denied rule action rejects with its code over the ack path and
    /// lands in the caller's error ring — the wire contract for the #21
    /// allowlist.
    #[test]
    fn rule_set_with_a_denied_action_acks_not_permitted() {
        let (mut app, host) = test_app();
        let chunk =
            "\x1b]777;ratty:rule.set;name=bad&sensor=sys.cpu&above=85&do=object.clear&tok=t1\x07";
        host.feed_tx
            .send(chunk.as_bytes().to_vec())
            .expect("virtual feed accepts bytes");
        app.update();
        let replies = drain_replies(&host);
        assert_eq!(replies.len(), 1);
        assert!(!replies[0].ok);
        assert_eq!(replies[0].code.as_deref(), Some(codes::NOT_PERMITTED));

        let reply = run_query(&mut app, &host, "q1", "state.errors", None);
        let page = payload(&reply);
        let items = page["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["action"], json!("rule.set"));
        assert_eq!(items[0]["code"], json!(codes::NOT_PERMITTED));
    }

    /// A synthetic `ps.v1` snapshot as its wire `data=` value.
    fn viz_ps_data(pids: &[u32]) -> String {
        let payload = json!({
            "capture": { "source": "test/synthetic", "ts": "2026-07-22T00:00:00Z" },
            "items": pids
                .iter()
                .map(|pid| json!({
                    "pid": pid,
                    "name": format!("proc{pid}"),
                    "cpu": 1.5,
                    "mem": 1024,
                    "state": "running",
                }))
                .collect::<Vec<_>>(),
        });
        query::b64url_encode(payload.to_string().as_bytes())
    }

    /// The milestone's closed loop: a collector-style `viz.set` with
    /// `tok=` acks over 778 and its snapshot reads back through
    /// `state.viz`; a kill-watcher-style `viz.effect` acks and queues; a
    /// `viz.remove` acks and the record is gone.
    #[test]
    fn closed_loop_viz_set_effect_remove_over_777_and_778() {
        let (mut app, host) = test_app();
        // One chunk: a tok='d viz.set followed by a state.viz query. The
        // ack must arrive first and the query must observe the snapshot.
        let data = viz_ps_data(&[1234, 4321]);
        let set =
            format!("\x1b]777;ratty:viz.set;id={ID}&kind=ps.v1&data={data}&x=10&y=5&tok=set1\x07");
        let query = query_sequence("q1", "state.viz", None);
        host.feed_tx
            .send(format!("{set}{query}").into_bytes())
            .expect("virtual feed accepts bytes");
        app.update();
        let replies = drain_replies(&host);
        assert_eq!(replies.len(), 2, "one ack, one query reply");
        assert_eq!(replies[0].token, "set1");
        assert!(replies[0].ack, "the command reply is kind=ack");
        assert!(replies[0].ok, "the snapshot committed");
        assert!(replies[1].ok);
        let page = payload(&replies[1]);
        let items = page["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item["id"], json!(ID));
        assert_eq!(item["owner"], json!(0));
        assert_eq!(item["kind"], json!("ps.v1"));
        assert_eq!(item["visible"], json!(true));
        assert_eq!(item["item_count"], json!(2));
        assert_eq!(item["anchor"]["row"], json!(5));
        assert_eq!(item["anchor"]["col"], json!(10));
        assert_eq!(item["capture"]["source"], json!("test/synthetic"));
        assert_eq!(item["pending_effects"], json!(0));
        let revision = item["revision"].as_u64().expect("revision");
        assert!(revision >= 1);

        // The kill watcher reports its observed outcome as an effect on
        // the pid domain key.
        host.feed_tx
            .send(
                format!("\x1b]777;ratty:viz.effect;id={ID}&key=1234&effect=died&tok=fx1\x07")
                    .into_bytes(),
            )
            .expect("virtual feed accepts bytes");
        app.update();
        let replies = drain_replies(&host);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].token, "fx1");
        assert!(replies[0].ok, "effects on live ids commit");
        let reply = run_query(&mut app, &host, "q2", "state.viz", None);
        let page = payload(&reply);
        assert_eq!(page["items"][0]["pending_effects"], json!(1));
        assert!(
            page["items"][0]["revision"].as_u64().expect("revision") > revision,
            "the effect bumped the revision"
        );

        // Remove: acked, and the registry answers honestly empty.
        host.feed_tx
            .send(format!("\x1b]777;ratty:viz.remove;id={ID}&tok=rm1\x07").into_bytes())
            .expect("virtual feed accepts bytes");
        app.update();
        let replies = drain_replies(&host);
        assert_eq!(replies.len(), 1);
        assert!(replies[0].ok);
        let reply = run_query(&mut app, &host, "q3", "state.viz", None);
        assert_eq!(payload(&reply)["items"], json!([]));
    }

    /// The chart-kind closed loop over the wire: an authored
    /// `chart.bar.v1` snapshot rides `viz.set` with `tok=`, reads back
    /// through `state.viz` with its kind, count, and provenance, and a
    /// hostile follow-up (an unknown state tag) rejects `bad-payload`
    /// without touching the live snapshot.
    #[test]
    fn closed_loop_chart_kind_over_777_and_778() {
        let (mut app, host) = test_app();
        let chart_data = |state: &str| {
            let value = json!({
                "capture": { "source": "authored", "ts": "authored" },
                "title": "queue",
                "max": 10.0,
                "items": [
                    { "key": "a", "value": 3.0, "state": state },
                    { "key": "b", "value": 7.5 },
                ],
            });
            crate::query::b64url_encode(value.to_string().as_bytes())
        };
        let set = format!(
            "\x1b]777;ratty:viz.set;id={ID}&kind=chart.bar.v1&data={}&x=4&y=2&cols=30&rows=10&tok=c1\x07",
            chart_data("active")
        );
        let query = query_sequence("q1", "state.viz", None);
        host.feed_tx
            .send(format!("{set}{query}").into_bytes())
            .expect("virtual feed accepts bytes");
        app.update();
        let replies = drain_replies(&host);
        assert_eq!(replies.len(), 2, "one ack, one query reply");
        assert!(replies[0].ok, "the chart committed");
        let page = payload(&replies[1]);
        let item = &page["items"][0];
        assert_eq!(item["kind"], json!("chart.bar.v1"));
        assert_eq!(item["item_count"], json!(2));
        assert_eq!(item["capture"]["source"], json!("authored"));
        assert_eq!(item["anchor"]["cols"], json!(30));

        // A hostile refresh with an unregistered state tag rejects and
        // leaves the live snapshot untouched.
        let bad = format!(
            "\x1b]777;ratty:viz.set;id={ID}&kind=chart.bar.v1&data={}&tok=c2\x07",
            chart_data("exploding")
        );
        host.feed_tx
            .send(bad.into_bytes())
            .expect("virtual feed accepts bytes");
        app.update();
        let replies = drain_replies(&host);
        assert_eq!(replies.len(), 1);
        assert!(!replies[0].ok);
        assert_eq!(replies[0].code.as_deref(), Some(codes::BAD_PAYLOAD));
        let reply = run_query(&mut app, &host, "q2", "state.viz", None);
        assert_eq!(
            payload(&reply)["items"][0]["item_count"],
            json!(2),
            "a rejected refresh changes nothing"
        );
    }

    /// The bookmark closed loop over the wire: store with `tok=`, read
    /// back through `state.bookmarks`, collide without `mode=replace`,
    /// and jump — whose relowered `SetMode`/`SetWarp` ride the normal
    /// command stream.
    #[test]
    fn closed_loop_bookmark_store_read_jump_over_777_and_778() {
        let (mut app, host) = test_app();
        // Warp the view so the stored snapshot has something to remember.
        let world = app.world_mut();
        let mut warp_query = world.query::<&mut TerminalPlaneWarp>();
        warp_query
            .single_mut(world)
            .expect("exactly one terminal seat")
            .amount = 0.5;
        host.feed_tx
            .send(b"\x1b]777;ratty:bookmark;name=dock&tok=b1\x07".to_vec())
            .expect("virtual feed accepts bytes");
        app.update();
        let replies = drain_replies(&host);
        assert_eq!(replies.len(), 1);
        assert!(replies[0].ok, "the bookmark stored");

        let reply = run_query(&mut app, &host, "q1", "state.bookmarks", None);
        assert_eq!(
            payload(&reply)["items"],
            json!([{ "name": "dock", "v": 1, "mode": "2d", "warp": 0.5 }]),
            "the caller reads back exactly what it stored"
        );

        // A colliding store without mode=replace rejects already-exists.
        host.feed_tx
            .send(b"\x1b]777;ratty:bookmark;name=dock&tok=b2\x07".to_vec())
            .expect("virtual feed accepts bytes");
        app.update();
        let replies = drain_replies(&host);
        assert_eq!(replies.len(), 1);
        assert!(!replies[0].ok);
        assert_eq!(replies[0].code.as_deref(), Some(codes::ALREADY_EXISTS));

        // Change the live view, then jump back: the relowered commands
        // land on the normal AiCommand stream (the mode/warp appliers are
        // exercised by their own tests; here the loop pins the plumbing).
        let world = app.world_mut();
        let mut warp_query = world.query::<&mut TerminalPlaneWarp>();
        warp_query
            .single_mut(world)
            .expect("exactly one terminal seat")
            .amount = 0.75;
        app.world_mut()
            .resource_mut::<Messages<AiCommand>>()
            .clear();
        host.feed_tx
            .send(b"\x1b]777;ratty:bookmark.jump;name=dock&tok=j1\x07".to_vec())
            .expect("virtual feed accepts bytes");
        app.update();
        let replies = drain_replies(&host);
        assert_eq!(replies.len(), 1);
        assert!(replies[0].ok, "the jump validated and relowered");
        let mut messages = app.world_mut().resource_mut::<Messages<AiCommand>>();
        let relowered: Vec<String> = messages
            .drain()
            .filter_map(|message| match message.command {
                RattyAiCommand::SetMode { mode } => Some(format!("mode={mode}")),
                RattyAiCommand::SetWarp { intensity } => Some(format!("warp={intensity}")),
                _ => None,
            })
            .collect();
        assert_eq!(relowered, vec!["mode=2d", "warp=0.5"]);
    }

    #[test]
    fn state_viz_scopes_foreign_records_to_visible_public_projections() {
        let (mut app, host) = test_app();
        let anchor = crate::viz::VizAnchor {
            row: 2,
            col: 2,
            cols: 10,
            rows: 4,
        };
        let payload_for = |pid: u32| {
            crate::viz::decode_viz_payload("ps.v1", &viz_ps_data(&[pid]))
                .expect("synthetic payload decodes")
        };
        {
            let mut viz = app.world_mut().resource_mut::<crate::viz::VizRegistry>();
            // The caller's own, unplaced (hidden) visualization.
            viz.upsert(ID, payload_for(1), None);
            // A foreign visible one and a foreign hidden one.
            viz.upsert(0x8100_0001, payload_for(2), Some(anchor));
            viz.upsert(0x8100_0002, payload_for(3), None);
        }
        let reply = run_query(&mut app, &host, "q1", "state.viz", None);
        assert!(reply.ok);
        let page = payload(&reply);
        let items = page["items"].as_array().expect("items array");
        assert_eq!(
            items.len(),
            2,
            "a hidden foreign visualization's existence is not readable"
        );
        // The caller's own record: hidden but listed, with the private
        // tier (capture provenance, effect queue length).
        assert_eq!(items[0]["id"], json!(ID));
        assert_eq!(items[0]["visible"], json!(false));
        assert!(items[0]["capture"].is_object());
        assert!(items[0]["pending_effects"].is_u64());
        // The foreign visible record: public projection only.
        assert_eq!(items[1]["id"], json!(0x8100_0001_u32));
        assert_eq!(items[1]["owner"], json!(1));
        assert_eq!(items[1]["visible"], json!(true));
        assert_eq!(items[1]["item_count"], json!(1));
        assert!(
            items[1].get("capture").is_none(),
            "capture provenance is owner-only"
        );
        assert!(items[1].get("pending_effects").is_none());
    }

    /// The M3.9 closed loop: a locked one-shot drops honestly, a locked
    /// ambient set defers (ok=1;code=deferred), the first user gesture
    /// unlocks and starts the retained bed — observable only by polling
    /// `state.scene` (there are no push events) — and stop fades it out.
    #[test]
    fn sound_locked_drop_deferred_ambient_unlock_and_poll() {
        let (mut app, host) = test_app();
        {
            // The decision layer is under test in every feature matrix;
            // pin the backend-present bit and start locked (the browser
            // pre-unlock path — the normal first-load path on the site).
            let mut sound = app.world_mut().resource_mut::<SoundState>();
            sound.enabled = true;
            sound.unlocked = false;
        }
        // One chunk: a tok='d one-shot (dropped) then a tok='d ambient
        // set (deferred). Acks arrive in command order.
        host.feed_tx
            .send(
                b"\x1b]777;ratty:sound.play;kind=chime&tok=t1\x07\
                  \x1b]777;ratty:sound.ambient.set;kind=ambient.hum&tok=t2\x07"
                    .to_vec(),
            )
            .expect("virtual feed accepts bytes");
        app.update();
        let replies = drain_replies(&host);
        assert_eq!(replies.len(), 2);
        assert!(replies[0].ack && !replies[0].ok);
        assert_eq!(replies[0].code.as_deref(), Some(codes::AUDIO_LOCKED));
        assert!(replies[1].ack && replies[1].ok, "deferred still commits");
        assert_eq!(replies[1].code.as_deref(), Some(codes::DEFERRED));

        // Poll while locked: nothing audible, the retained bed is private.
        let reply = run_query(&mut app, &host, "q1", "state.scene", None);
        let scene = payload(&reply);
        assert_eq!(scene["audio"]["enabled"], json!(true));
        assert_eq!(scene["audio"]["unlocked"], json!(false));
        assert_eq!(scene["audio"]["ambient"]["kind"], json!(null));
        assert_eq!(scene["audio"]["ambient"]["phase"], json!("idle"));
        assert_eq!(scene["audio"]["voices"], json!(0));

        // The first user gesture unlocks; the retained bed fades in.
        app.world_mut().resource_mut::<SoundState>().unlock();
        let reply = run_query(&mut app, &host, "q2", "state.scene", None);
        let scene = payload(&reply);
        assert_eq!(scene["audio"]["unlocked"], json!(true));
        assert_eq!(scene["audio"]["ambient"]["kind"], json!("ambient.hum"));
        assert_eq!(scene["audio"]["ambient"]["phase"], json!("crossfading"));

        // Stop is an idempotent commit; the bed fades out.
        host.feed_tx
            .send(b"\x1b]777;ratty:sound.ambient.stop;tok=t3\x07".to_vec())
            .expect("virtual feed accepts bytes");
        app.update();
        let replies = drain_replies(&host);
        assert_eq!(replies.len(), 1);
        assert!(replies[0].ok);
        let reply = run_query(&mut app, &host, "q3", "state.scene", None);
        assert_eq!(
            payload(&reply)["audio"]["ambient"]["phase"],
            json!("fading-out")
        );
    }

    #[test]
    fn sound_limits_are_advertised_in_caps() {
        let (mut app, host) = test_app();
        let reply = run_query(&mut app, &host, "q1", "caps", None);
        let caps = payload(&reply);
        assert_eq!(
            caps["limits"]["sound_voices"],
            json!(crate::sound::MAX_SOUND_VOICES)
        );
        assert_eq!(
            caps["limits"]["sound_plays_per_sec"],
            json!(crate::sound::SOUND_PLAYS_PER_SEC)
        );
    }

    #[test]
    fn closed_loop_avatar_speak_over_777_and_778() {
        // The M3.10 rider demo: write over 777, read back over 778 — one
        // chunk carrying show, a long-running speak, and the two reads.
        let (mut app, host) = test_app();
        let set = "\x1b]777;ratty:avatar.set;tok=a1\x07";
        let speak = "\x1b]777;ratty:avatar.speak;text=Deploy%20finished&tok=s1\x07";
        let scene = query_sequence("q1", "state.scene", None);
        let execs = query_sequence("q2", "state.executions", None);
        host.feed_tx
            .send(format!("{set}{speak}{scene}{execs}").into_bytes())
            .expect("virtual feed accepts bytes");
        app.update();

        let replies = drain_replies(&host);
        assert_eq!(replies.len(), 4, "two acks, two query replies");
        assert!(replies[0].ok && replies[0].ack, "avatar.set committed");

        let ack = &replies[1];
        assert_eq!(ack.token, "s1");
        assert!(ack.ack && ack.ok);
        assert_eq!(ack.code.as_deref(), Some(codes::STARTED));
        let data: Value = serde_json::from_slice(&ack.data).expect("ack data is JSON");
        let handle = data["id"].as_str().expect("handle").to_string();
        assert_eq!(data["position"], json!(0));

        let scene = payload(&replies[2]);
        assert_eq!(scene["avatar"]["visible"], json!(true));
        assert_eq!(scene["avatar"]["speaking"], json!(true));
        assert_eq!(scene["avatar"]["speaker"], json!(0));
        assert_eq!(scene["avatar"]["execution"], json!(handle));
        assert_eq!(scene["avatar"]["queue_depth"], json!(0));

        let execs = payload(&replies[3]);
        let items = execs["items"].as_array().expect("items");
        assert!(
            items
                .iter()
                .any(|item| item["id"] == json!(handle) && item["status"] == json!("active")),
            "the started handle is inspectable via state.executions"
        );
    }

    #[test]
    fn caps_reports_trust_grants_and_avatar_limits() {
        let (mut app, host) = test_app();
        let reply = run_query(&mut app, &host, "q1", "caps", None);
        let caps = payload(&reply);
        assert_eq!(caps["trust"]["avatar_scene"], json!(true));
        assert_eq!(caps["trust"]["scene_ambient"], json!(true));
        // Both terminal grants are readable and both default DENY.
        assert_eq!(caps["trust"]["terminal_lifecycle"], json!(false));
        assert_eq!(caps["trust"]["terminal_focus"], json!(false));
        // The terminals organ advertises its shape before a caller
        // attempts a verb: `spawn_fields: []` and `place_fields` are the
        // honesty contract for the fields the appliers refuse.
        assert_eq!(caps["terminals"]["live"], json!(1));
        assert_eq!(caps["terminals"]["max"], json!(4));
        assert_eq!(caps["terminals"]["pool"], json!(128));
        assert_eq!(caps["terminals"]["spawn_fields"], json!([]));
        assert_eq!(caps["terminals"]["place_fields"], json!(["cols", "rows"]));
        assert_eq!(
            caps["limits"]["terminal_max_axis"],
            json!(crate::identity::MAX_TERMINAL_AXIS)
        );
        // Terminals are not panes (#22): the #57 pane-0 contract holds
        // until #86 ships, no matter how many terminals are live.
        assert_eq!(caps["panes"], json!(1));
        assert_eq!(
            caps["limits"]["avatar_text_bytes"],
            json!(crate::avatar::MAX_AVATAR_TEXT_BYTES)
        );
        assert_eq!(
            caps["limits"]["avatar_queue_global"],
            json!(crate::avatar::MAX_PENDING_UTTERANCES_GLOBAL)
        );
        assert_eq!(caps["avatar_models"], json!(["mascot"]));
    }

    #[test]
    fn execution_handles_are_session_scoped_and_never_reused() {
        let mut session = QuerySession::default();
        let first = session.mint_execution_id();
        let second = session.mint_execution_id();
        assert_ne!(first, second, "handles are never reused in a session");
        assert!(first.starts_with(&session.nonce_hex()));
        assert!(session.owns_execution_id(&first));
        assert!(session.owns_execution_id(&second));
        // A handle minted by another process (different nonce) is foreign:
        // it answers unknown-id with an honest previous-session message
        // instead of silently matching.
        let foreign = format!("{:016x}-1", u64::MAX);
        assert!(!session.owns_execution_id(&foreign));
        // A bare nonce with no counter suffix is not a handle.
        assert!(!session.owns_execution_id(&session.nonce_hex()));
    }

    /// The M3.11 closed loop: join + cursor + note over OSC 777 with
    /// `tok=` acks, the aggregate and the roster read back over 778, the
    /// lease expiring visibly (`fresh: false`, counts drop) rather than
    /// vanishing, and `user.leave`/`note.remove` actually removing rows.
    #[test]
    fn closed_loop_presence_over_777_and_778() {
        let (mut app, host) = test_app();
        let chunk = "\x1b]777;ratty:user.join;id=alice&color=%2300ffcc&ttl=2&tok=j1\x07\
             \x1b]777;ratty:user.cursor;id=alice&x=3&y=4&tok=c1\x07\
             \x1b]777;ratty:note;id=n1&text=review%20this&x=5&y=6&ttl=2&tok=n1\x07";
        host.feed_tx
            .send(chunk.as_bytes().to_vec())
            .expect("virtual feed accepts bytes");
        app.update();
        let replies = drain_replies(&host);
        assert_eq!(replies.len(), 3, "one ack per tok= command");
        for (index, token) in ["j1", "c1", "n1"].iter().enumerate() {
            assert_eq!(replies[index].token, *token);
            assert!(replies[index].ack && replies[index].ok, "{token} commits");
        }

        // The aggregate: one namespace row with fresh counts (#25).
        let reply = run_query(&mut app, &host, "q1", "state.namespaces", None);
        assert_eq!(
            payload(&reply)["namespaces"],
            json!([{ "ns": 0, "objects": 0, "participants": 1, "notes": 1 }])
        );

        // The roster: participant rows precede note rows.
        let reply = run_query(&mut app, &host, "q2", "state.presence", None);
        let page = payload(&reply);
        let items = page["items"].as_array().expect("items array");
        assert_eq!(items.len(), 2);
        let participant = &items[0];
        assert_eq!(participant["kind"], json!("participant"));
        assert_eq!(participant["ns"], json!(0));
        assert_eq!(participant["id"], json!("alice"));
        assert_eq!(
            participant["name"],
            json!("alice"),
            "name defaulted to the id"
        );
        assert_eq!(participant["color"], json!("#00ffcc"));
        assert_eq!(participant["cursor"], json!({ "x": 3, "y": 4 }));
        assert_eq!(participant["fresh"], json!(true));
        assert_eq!(participant["revision"], json!(2), "join then cursor");
        assert_eq!(participant["ttl_secs"], json!(2.0));
        let note = &items[1];
        assert_eq!(note["kind"], json!("note"));
        assert_eq!(note["id"], json!("n1"));
        assert_eq!(note["text"], json!("review this"));
        assert_eq!(note["x"], json!(5));
        assert_eq!(note["y"], json!(6));
        assert_eq!(note["revision"], json!(1));

        // Past the TTL: expiry is visible in the caller's own roster —
        // fresh: false, never a silent vanish — while the public
        // aggregate drops the namespace (nothing rendered = nothing
        // public).
        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(std::time::Duration::from_secs(3));
        let reply = run_query(&mut app, &host, "q3", "state.presence", None);
        let page = payload(&reply);
        let items = page["items"].as_array().expect("items array");
        assert_eq!(items.len(), 2, "expired rows stay queryable");
        assert_eq!(items[0]["fresh"], json!(false));
        assert_eq!(items[1]["fresh"], json!(false));
        let reply = run_query(&mut app, &host, "q4", "state.namespaces", None);
        assert_eq!(payload(&reply)["namespaces"], json!([]));

        // Removal is explicit: leave/note.remove work on expired rows
        // and only then do the rows leave the registry.
        let chunk = "\x1b]777;ratty:user.leave;id=alice&tok=l1\x07\
             \x1b]777;ratty:note.remove;id=n1&tok=r1\x07";
        host.feed_tx
            .send(chunk.as_bytes().to_vec())
            .expect("virtual feed accepts bytes");
        app.update();
        let replies = drain_replies(&host);
        assert_eq!(replies.len(), 2);
        assert!(
            replies[0].ok && replies[1].ok,
            "removal of expired rows commits"
        );
        let reply = run_query(&mut app, &host, "q5", "state.presence", None);
        assert_eq!(payload(&reply)["items"], json!([]));
    }

    /// A maxed namespace roster exceeds one reply page — the reason
    /// `state.presence` paginates instead of riding `state.namespaces`.
    #[test]
    fn presence_rosters_paginate_within_the_page_budget() {
        let (mut app, host) = test_app();
        let mut chunk = String::new();
        for index in 0..16 {
            chunk.push_str(&format!(
                "\x1b]777;ratty:user.join;id=participant-{index:02}\x07"
            ));
            chunk.push_str(&format!(
                "\x1b]777;ratty:note;id=note-{index:02}&text=annotation%20{index:02}&x=1&y=2\x07"
            ));
        }
        host.feed_tx
            .send(chunk.into_bytes())
            .expect("virtual feed accepts bytes");
        app.update();
        drain_replies(&host);

        let mut collected = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let data = cursor.as_ref().map(|c| json!({ "cursor": c }));
            let token = format!("q{pages}");
            let reply = run_query(&mut app, &host, &token, "state.presence", data);
            assert!(reply.ok);
            let page = payload(&reply);
            for item in page["items"].as_array().expect("items") {
                collected.push(item["id"].as_str().expect("id").to_string());
            }
            pages += 1;
            assert!(pages < 32, "pagination must terminate");
            match page["cursor"].as_str() {
                Some(next) => cursor = Some(next.to_string()),
                None => break,
            }
        }
        assert!(pages > 1, "a maxed roster exceeds one size-bounded page");
        let expected: Vec<String> = (0..16)
            .map(|index| format!("participant-{index:02}"))
            .chain((0..16).map(|index| format!("note-{index:02}")))
            .collect();
        assert_eq!(collected, expected, "every row exactly once, in order");
    }
}
