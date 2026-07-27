# OSC-777 addressing and per-runtime trust

Research asset for [wayfinder ticket #50](https://github.com/Tnsr-Q/ratty/issues/50)
(map [#42](https://github.com/Tnsr-Q/ratty/issues/42)). **Recommendation only —
the lock happens at the spine grilling
([#56](https://github.com/Tnsr-Q/ratty/issues/56)).** The per-runtime census
(`docs/research/per-runtime-spine.md`, branch `claude/per-runtime-spine`) hands
this ticket its two wire-surface rows; per the coordination contract, the
[Runtime-identity assumptions](#runtime-identity-assumptions-for-56) section
states every identity assumption explicitly so #56 can reconcile them against
the census.

Precedent locked upstream — built on here, never reopened:

- **Authority = ingress context, never wire bytes.** The canonical statement is
  `IngressSource`'s doc comment (`src/runtime.rs:36-43`): authority derives from
  *where bytes physically entered*; a stream cannot claim an identity. The
  [#25](https://github.com/Tnsr-Q/ratty/issues/25) resolution locks the
  corollary: ratty never trusts a namespace/owner/author field embedded in
  OSC-777 bytes.
- **One PTY = one effective principal.** A local transport cannot attribute
  bytes to individual writer processes (`src/runtime.rs:40-42`,
  `src/presence.rs:93-95`).
- **Sources are assigned out-of-band, pre-ingress.** A relay/bridge
  authenticates writers *before* their bytes reach the parser; the source
  identity is a constructor argument to the transport seam, never anything
  parsed (#25 resolution; `src/runtime.rs:42-43`).
- **A transport is a constructor** ([#22](https://github.com/Tnsr-Q/ratty/issues/22)):
  N runtimes are N instances of the transport seam; a third transport is a
  third constructor. Placement, not splits.

## The question, precisely — and what the census handed over

`docs/research/panes-feasibility.md:40` states the original problem: OSC-777
"arrives via one pane's stream; commands may target other panes — needs an
addressing rule." Under the census's entity-per-runtime proposal the sharpened
form is: **one byte stream, N terminal runtimes — how does a 777 command name
its target, and who is the principal behind it?**

State of the wire today:

- `IngressSource` is a closed, fieldless one-variant `Copy` enum — `Local`,
  covering both the native PTY and the wasm virtual channel
  (`src/runtime.rs:44-50`). `namespace()` is an exhaustive match with one arm,
  `Local → 0` (`src/runtime.rs:55-59`).
- The source is stamped once per parser instance onto
  `TerminalParserCallbacks.source` and copied onto every parsed 777 command,
  778 query, and wire error; it survives resize via `std::mem::take`
  (`src/runtime.rs:64-77`).
- The namespace u8 lands in the AI object-id layout: ids ≥ `0x8000_0000` are
  AI-owned, 7 bits at `>>24` carry the namespace, 24 bits index per namespace —
  the address space physically supports 128 namespaces × 16.7M objects
  (`src/osc.rs:41-51`).
- Because every local writer maps to namespace 0, N local PTYs collapse into
  one wire principal today. Concrete collisions the census names:
  `AiDiagnostics` rings keyed by bare namespace u8 silently merge
  (`src/query_channel.rs:179-183`), `ReactiveRegistry`'s halves share the same
  u8-collision problem (census row for `src/reactive.rs:279`), and the object-id
  ledger's ids are only unique per runtime.

What the census hands #50, exactly:

- **Two wire-surface rows.** (1) `IngressSource` (`src/runtime.rs:45`) — are N
  local PTYs N distinct principals with N AI-object namespaces, or one shared
  `Local`? The per-parser stamp "is the natural identity carrier either way."
  (2) `apply_avatar_commands` (`src/avatar/mod.rs:717`, capability gate `:753`)
  — the organ stays one, but speaker attribution, fair-queue keying, and
  per-source `SceneCapability` grants become 777 addressing once N wires exist.
- **Four handoff items**: how a 777-created viz acquires terminal ownership;
  which terminal a `RattySession.query()` feeds; the N-namespace-0-universes
  question; the wire shape of a runtime-qualified `IngressSource` stamp.
- **Three open questions naming #50**: internal routing key (`Entity`) vs wire
  identity — does `IngressSource` grow a runtime field, or do commands carry
  `(Entity, IngressSource)` side by side, with stored-source invalidation on
  runtime death attached as a lifecycle constraint; whether cross-terminal
  references (macro on A invoking bookmark on B) *stay* impossible — "a real
  #50/#49 choice, not a free consequence"; and trusted-config replication (one
  copy vs per-spawn re-seed).

Where a target-runtime id could physically attach (the option space the models
below are drawn from):

1. A reserved envelope payload key beside `tok=` — `parse_control` already
   extracts `tok=` at the envelope layer before command parse
   (`src/osc.rs:33`, `:845-860`), an exact template for the 777 side; the 778
   side parses its own `;`-separated envelope in `src/query.rs`'s `parse_778`
   (`src/query.rs:322`), a second extraction point. Hazard: an old terminal
   silently self-applies — on 778 because unknown envelope keys are ignored
   by documented rule (`protocols/query.md:53-54`); on 777 because
   `Payload::parse` stores every `k=v` and commands read only known keys
   (`src/osc.rs:1272-1284`), a parser-construction fact documented only
   family-locally (e.g. `protocols/presence.md:63`).
2. A privileged control family holding sticky per-transport state.
3. Per-command targets on the #49 family only (as `pane.*` once carried a u8
   `pane=`, the only in-band target id ever shipped — now rejected
   `UNSUPPORTED` by the catch-all, `src/osc.rs:351-375`, `src/ai.rs:313-325`).
4. No wire bytes at all: each runtime is its own transport
   (`ratty-ai --tty <path>`, `protocols/query.md:328-339` — "one PTY = one
   session", `:45-46`), so targeting-by-transport already works.

Precedent for in-band foreign *reference* exists exactly once:
`avatar.speech.clear;ns=<u8>` — classified scene-global (`src/osc.rs:774-778`)
*and* capability-gated at apply (`require_scene`, `src/avatar/mod.rs:753`),
the one shipped command carrying both properties, addressing-never-authority
(`src/osc.rs:130-143`). Precedent for
the default: absent target = the carrying runtime (`macro.play` scope
resolution).

## Three models, priced

### Model E — explicit target key (`term=`, absent = arrival runtime)

**Design.** Targeting rides the envelope, not per-command grammar — but that
is two mechanisms, not one: on 777, a second reserved payload key beside
`tok=` extracted in `parse_control` (exact template, `src/osc.rs:845-860`);
on 778, a new envelope field in `src/query.rs`'s `parse_778`
(`src/query.rs:322`) — the `;`-separated envelope grammar (`v=`, `t=`, `id=`,
`op=`, `data=`; `protocols/query.md:37-39`) is a different parser from the
777 payload. Zero per-command grammar churn either way. Runtime ids are
terminal-assigned `<session-nonce-hex>-<seq>`
handles (the #18 execution-handle shape, `protocols/query.md:90-104`) — never
Bevy `Entity` (`protocols/query.md:112-128` forbids), never reused, detectably
stale, sidestepping #16 collisions. Absent `term=` = today's semantics
verbatim. `term=` is a reference, never authority (the
`avatar.speech.clear;ns=` precedent); cross-runtime writes gate on a
`CrossTerminal` capability derived purely from `(IngressSource, AppConfig)`
like `SceneCapability::granted_to` (`src/capability.rs:51-58`). Replies follow
ingress, never the target.

**Strengths.** Structural back-compat (absent key = today's code path); one
envelope-layer mechanism, already-shipped template; honors the #25 lock with
shipped precedent; single-stream conductor orchestration becomes possible;
turns the census's cross-terminal-reach question into an explicit default-deny
config choice; handle-style ids satisfy #21's expiry-visible rule for stored
sources.

**Weaknesses.** Old binaries silently self-apply `term=`-carrying commands
(unknown payload keys ignored — `caps` feature-detection is advisory only);
capability granularity is per transport *class*, not per pair, because grants
are load-time and wire-immutable (`src/capability.rs:13-16`, `:49-50`);
scene-global families (`is_scene_global`, `src/osc.rs:768`) make `term=`
semantically ambiguous until #52 lands scoping; a source's state scatters
across runtimes (ns-A rows and diagnostics living in runtime B), doubling the
778 addressing surface; every replay path gains handle-staleness checks.

**Fatal risks.** (1) *Shared-Local ownership bypass*: if #56 resolves the
census question as N-locals-share-ns-0, then `term=B` plus a grant passes the
`NOT_OWNER` check (`src/ai.rs:384`) on another principal's objects — both are
ns 0. E is only safe conditioned on N distinct namespaces. (2) The
old-terminal silent misroute is unfixable retroactively — the ignoring is
shipped behavior on both surfaces (documented wire contract for the 778
envelope, `protocols/query.md:53-54`; parser construction for 777 payloads,
`src/osc.rs:1272-1284`), two independent parsers both dropping the key —
which strengthens, not weakens, the hazard; the failure mode is a
destructive command (`object.clear`, `reset`) applied to the wrong runtime.
(3) Per-pair grants, if genuinely demanded, are inexpressible without
violating wire-immutable config or naming ids that don't exist at load time.
(4) `term=` squats a reserved word in every family's payload namespace
forever, and collision is silent reinterpretation (extraction precedes command
parse, `src/osc.rs:845-860`). (5) Ordering hazard: E locks addressing shape at
#56 before #52 locks scene-global scoping.

### Model S — sticky session context (`runtime.select` / `runtime.spawn` control family)

**Design.** `ratty:runtime.select;rt=<handle>` sets the carrying transport's
sticky target, resolved ingress-side in `pump_pty_output`
(`src/systems.rs:166`), which consumes the parser drains
(`src/runtime.rs:86-103`); `runtime.spawn` constructs a new
transport + runtime, arg-less (#12 no-wire-filesystem, `src/macros.rs:25-68`),
handle returned in the ack payload. Data commands untouched — the census's 65
per-runtime symbols need no wire change. `runtime.*` is control-plane:
wire-origin-only, never macro-recorded (the `src/presence.rs:22-27` pattern),
so stored replays never hold runtime handles. `IngressSource` stays fieldless;
`Entity` rides side-by-side as routing. Prior art: tmux control mode, wezterm
Domains.

**Strengths.** Zero data-plane churn; one choke point for trust and routing;
select-time failure is *loud* — envelope-layer `tok=` yields an error ack even
on old terminals where the action fails parse (`src/osc.rs:797-859`), versus
the silently-ignored envelope key; byte-for-byte back-compat trivially
provable; clean replay story by construction.

**Weaknesses.** The wire becomes stateful — a captured stream is no longer
self-describing; chatty for fan-out; correctness depends on agents ack-gating
and resyncing, which the protocol cannot enforce; `runtime.spawn` is process
execution from the wire; S keeps shared ns-0, so it does not itself fix the
diagnostics/reactive merges.

**Fatal risks.** (1) *Shared-context cross-contamination under the locked
one-PTY-one-principal collapse*: multiple uncoordinated local writers share
one sticky context — writer A selects runtime B and writer C's interleaved
commands silently land on B; ratty cannot distinguish the writers even in
principle (`src/runtime.rs:40-42`). tmux only avoids this because one client
owns the control channel. Inherent to any sticky-state model. (2) *Raw-byte
replays are wire-origin*: a silk transmission played through the parser
legitimately carries `CommandOrigin::Wire`, so an embedded `runtime.select`
hijacks the transport's context for everything after it — the control-plane
origin refusal does not cover playback. (3) Restart desync: handles die with
the session nonce, so correctness depends on stateful client bookkeeping —
the failure class the stateless 777 grammar had avoided.

### Model A — arrival is the address (no target field; addressing by possession)

**Design.** The 777 grammar is unchanged (`src/osc.rs:6-15`); `tok=` stays the
only reserved envelope key. No content-plane family ever names a runtime: a
command applies to the runtime whose transport parsed it — the semantics every
existing stream already has. One transport instance = one principal = one
runtime. `IngressSource` becomes data-carrying per transport class:
`Local(RuntimeId)`, a 7-bit ordinal assigned by the spawn constructor
out-of-band (#22: a transport is a constructor; #25: sources assigned before
ingress, never parsed); `namespace()` returns the ordinal; the first runtime
is `Local(0)`, preserving today's stamping exactly. To drive runtime B, hold a
channel to B (`ratty-ai --tty <path>` natively; on wasm, a byte channel bound
to B's transport instance — how N channels surface in the page API is #53's
question, assumption 10). The sole targeted surface is the scene-global
class — `is_scene_global` classification (`src/osc.rs:768`) plus per-family
capability gating on the `require_scene` pattern (`src/avatar/mod.rs:753`),
two distinct mechanisms that `avatar.speech.clear;ns=` alone ships both of —
which #49's lifecycle verbs join carrying `<nonce>-<seq>` handles.

**Strengths.** Zero wire delta — the 1-runtime byte stream *is* the N-runtime
byte stream, so the misroute hazard structurally cannot occur (there is
nothing new for an old terminal to ignore); purest fit to the locked
precedent — addressing and authority are the same mechanism (channel
possession), so no per-family reference-vs-authority boundary needs policing;
#12 `NOT_OWNER` (`src/ai.rs:384`), #16 collision rules, #21 lease semantics
survive verbatim over (namespace, caller-local id); the census's ns-0 merges
dissolve via per-runtime namespaces with zero new enforcement code; reply
routing is degenerate-simple (ingress = target by construction); principal-
uniform with remotes (the wezterm Domain lesson,
`docs/research/panes-feasibility.md:64-67`); answers all three census open
questions crisply.

**Weaknesses.** One agent orchestrating K runtimes needs K open channels; no
in-band foreign observation (the read tiers never gain a runtime axis); the
scene-global escape hatch bears growing scope-creep pressure; possession is
all-or-nothing; conductor patterns (macro-on-A-drives-bookmark-on-B) are
impossible in the content plane; the #49 lifecycle verbs routed into the
gated class inherit E's grant coarseness verbatim — class-granular,
per-pair inexpressible (see "Who may speak 777").

**Fatal risks (managed, stated for #56).** (1) If #49's product requirement
turns out to be single-stream conductor orchestration, A cannot express it —
the escape is Model E layered on later (see recommendation). (2) Namespace
exhaustion: 7 bits × retire-on-death-no-reuse caps runtime lifetimes at 128
per session — failure is an explicit spawn error, never silent, but the
ceiling is dictated by the `src/osc.rs:41-51` id layout. (3) The security
floor lives outside the compile-checked seam: a wasm embedding that
multiplexes N runtimes through one JS byte channel collapses them to one
principal — but that *is* the locked one-channel-one-principal rule stated
honestly, and it afflicts E and S identically. The demux boundary is where
principals are minted, never wire bytes; how N transport channels surface in
the page API is #53's fork, not decided here (assumption 10). (4) The
no-reuse invariant is load-bearing: reuse
would let a stored replay write into a namespace now owned by a different
live runtime — the invariant must be locked, not the registry made clever.

## The recommendation: Model A — arrival is the address

**Why A.** Three structural facts decide it:

1. **E subsumes A.** Model E's own safety precondition is N distinct
   namespaces — otherwise `term=B` plus a grant bypasses `NOT_OWNER`
   (`src/ai.rs:384`) on shared ns-0. So E = A's principal spine + a wire key.
   The asymmetry: A→E is additive later (a `caps` bit plus the ignore
   behavior on both parsers — documented for the 778 envelope,
   `protocols/query.md:53-54`; implicit parser fact for 777 payloads,
   `src/osc.rs:1272-1284`); E→A is
   impossible (`term=` is squatted forever, and the old-terminal silent
   misroute cannot be fixed retroactively). Locking A defers E's hazards
   until #49 proves the need; locking E pays them all now, plus the #52
   ordering hazard.
2. **S is disqualified twice**, both inherent: sticky context under the
   one-PTY-one-principal collapse cross-contaminates uncoordinated writers,
   and raw-byte playback legitimately carries `CommandOrigin::Wire`, so a
   recorded `runtime.select` hijacks the context. It also keeps shared ns-0
   and makes the wire stateful.
3. **A scores best on every axis that matters here**: byte-for-byte
   migration is structural, the ownership machinery survives verbatim, the
   ns-0 merges dissolve for free, the designed compile-breaks
   (`src/capability.rs:22-26` no-wildcard match; `send_reply`'s singleton
   match, `src/query_channel.rs:463-467`) fire and resolve mechanically, and
   implementation is the smallest of the three.

**Wire syntax.** None new on the content plane. The grammar stays
`ESC ] 777 ; ratty:<action> ; <payload> BEL` with `tok=` the only reserved
envelope key (`src/osc.rs:6-15`, `:33`). Wire-visible runtime references
appear only in the gated scene-global/#49 class (classification plus
per-family gate — the two mechanisms, see the #49 takeaways) as
terminal-assigned `<session-nonce-hex>-<seq>` handles (#18 shape: never
reused, detectably stale, never `Entity`), returned in spawn acks and
enumerable via a `state.runtimes` 778 op (the `state.neighbors` pattern).
`caps` gains two keys under its documented append-only rule
(`src/query_channel.rs:565-568`): the feature advertisement (the `viz_kinds`
precedent, `src/query_channel.rs:620`) and the `namespace` self-identity key
(assumption 11).

**Ownership and leases.** Registries stay keyed (namespace, caller-local id);
under A the runtime axis and the namespace axis are the same axis. #12's
`NOT_OWNER` check (`ai_object_namespace(id) != source.namespace()`,
`src/ai.rs:384`, `:492`, `:535`), #16's already-exists-unless-replace and
revision lineage (`protocols/presence.md`), and #21's
fresh/expired-never-deleted leases continue verbatim. Presence stays
control-plane and wire-origin-only (`src/presence.rs:22-27`). Stored sources
(macro/rule/bookmark replays) referencing a dead runtime fail explicitly —
retired ordinals make the stale stamp detectable, satisfying #21's
expiry-visible-never-silent rule and answering the census's stored-source
lifecycle question.

**Reply routing.** Replies and acks follow *ingress*, never any target —
which under A are the same runtime by construction. The designed
compile-break at `send_reply` (`src/query_channel.rs:463-467`, "the match
keeps routing keyed to the stamped ingress source so future transports cannot
broadcast") becomes a stamp→writer lookup; `answer_queries` de-singletons on
the same `(Entity, IngressSource)` stamp; the buses stay single and
source-stamped (census). `QuerySession` nonce/handles stay app-global; wasm
`PENDING_QUERIES` is token-keyed and runtime-agnostic (the static at
`src/web.rs:62`, resolved via `try_resolve_pending`,
`src/query_channel.rs:444-447`).

**Migration.** Today's wire is valid byte-for-byte, structurally: no new
bytes exist, so no old terminal can misroute anything, and the first runtime
stamps `Local(0)` — identical to today's `namespace() → 0`. The
no-`caps`-gate claim is scoped to runtime #1: old tooling hard-coding ns-0
ids at `0x8000_0000` is untouched on the first runtime; *inside a spawned
runtime* the same tooling gets `NOT_OWNER` on every id (the 7-bit namespace
field is checked against ingress, `src/ai.rs:384`), so agents there must
feature-detect the `caps` `namespace` key and discover their ordinal before
minting a single id (assumption 11). The compile-breaks (`namespace()`,
`granted_to`, `send_reply`) enumerate every code decision the variant change
forces; nothing resolves silently.

**The recorded escape.** Model E's `term=` envelope key remains the
documented additive escape if #49's product answer demands single-stream
conductor orchestration. It layers onto this same spine later without
reopening anything locked here, because E's runtime-identity assumptions are
a strict superset of A's. It must not be locked before #49 proves the need.
One dependency to surface at #56: the additive story leans on the 777
unknown-payload-key ignore behavior, which is parser fact (`Payload::parse`,
`src/osc.rs:1272-1284`) documented only family-locally
(`protocols/presence.md:63`) — unlike the 778 envelope's documented rule
(`protocols/query.md:53-54`), there is no protocol-wide 777 statement. #56
should decide whether to promote it to documented wire contract.

## Runtime-identity assumptions (for #56)

Stated explicitly per the coordination contract, for reconciliation against
the census. Three identity layers (items 1–3), each assigned by a different
party, plus the invariants binding them (items 4–12):

1. **Principal = transport instance.** A local runtime and its principal are
   1:1; N local PTYs are N principals with N disjoint namespace universes —
   resolving the census's open question *against* shared-ns-0. The
   one-PTY-one-principal lock (`src/runtime.rs:40-42`) is about writers
   behind *one* channel, not about distinct transport instances; #22's
   N-instances-of-the-seam already sanctions N principals.
2. **Internal routing key = the terminal's Bevy `Entity`**, assigned by the
   app, never wire-visible (`protocols/query.md:112-128` ban). Commands are
   stamped `(Entity, IngressSource)` on the single shared buses: `Entity`
   routes, `IngressSource` authorizes.
3. **Trust identity = `IngressSource::Local(RuntimeId)`**, a 7-bit ordinal
   carried as data, minted by the spawn-side transport constructor
   out-of-band (#22; #25: never parsed). `namespace()` returns the ordinal;
   the first runtime is `Local(0)`, preserving today's stamping byte-for-byte
   (`src/runtime.rs:55-59`).
4. **The per-parser stamp remains the sole identity carrier**
   (`src/runtime.rs:64-77`), surviving resize via `std::mem::take`.
5. **Ordinals are session-scoped, retired on runtime death, never reused
   within a session.** No-reuse is load-bearing: it guarantees a stale stored
   stamp can never alias a live runtime. Consequence: at most 128 runtime
   lifetimes per session (7 bits, `src/osc.rs:41-51`); exhaustion is an
   explicit spawn error.
6. **Runtime identity never appears as *authority* or a *routing target*
   in content-plane families.** Not "never in any form" — ordinals are
   wire-visible in three shipped surfaces this recommendation preserves,
   and all three are references or ownership bookkeeping, never trusted:
   (a) every `object.*` id carries the caller's ordinal in bits 24–30
   (`src/osc.rs:41-51`) — a wire-carried *ownership claim*, checked
   against ingress and rejected on mismatch (#12, `src/ai.rs:384`), never
   believed; (b) the read side exposes owner namespaces per row
   (`protocols/query.md:118-122`; `state.namespaces`, `:218-222`); (c) the
   gated scene-global class carries two reference vocabularies — the
   shipped `ns=<u8>` namespace form (`avatar.speech.clear;ns=`,
   `src/osc.rs:130-143`), grandfathered forever because it is shipped
   wire, and #49's `<session-nonce-hex>-<seq>` handles (#18 shape —
   references, never authority, never `Entity`, never reused), mandated
   for all new targeted verbs. Wire-visible, never trusted.
7. **Replies/acks follow ingress, never a target** — under A the same
   runtime by construction.
8. **Cross-runtime content reach stays impossible by construction** (census
   open question 2 answered as: yes, it stays impossible; reach = channel
   possession; the read tiers gain no runtime axis).
9. **Trusted config does not replicate per spawn** (census open question 3):
   one `AppConfig` copy, wire-immutable (`src/capability.rs:13-16`,
   `:49-50`); grants are keyed per transport *class* with the ordinal as
   data, so spawning cannot mint privilege.
10. **The wasm assumption is transport-shaped only: one transport instance
    (one virtual byte channel / `VirtualTerminalHost`,
    `src/runtime.rs:329`, `:430`) per runtime principal.** One byte
    channel = one principal, whatever the page API looks like; an
    embedding that funnels N runtimes through one channel collapses them
    to one principal — the one-channel-one-principal rule applied
    honestly, not a new hazard. How N transports surface in the page API —
    N `RattySession`s, per-terminal selectors on
    `feed()`/`drain_input()`/`query()`, or an in-band mux — is exactly
    #53's fork (census open question; today's shipped contract is one
    session per page, `src/web.rs:60-62`, `:154-159`), and #50 does not
    decide it. The consequence #50 hands #53: whatever the page-API shape,
    the demux boundary is where principals are minted, never wire bytes —
    N sessions or per-terminal selectors preserve N principals; an in-band
    mux preserves them only if its framing is transport-layer, parsed
    *pre-ingress* by trusted embedding code (#25's out-of-band rule); mux
    framing inside the terminal byte stream would be wire bytes claiming
    identity, so under that shape the N runtimes honestly collapse to one
    principal. Conditional on #53's choice, the census's handoff item
    answers itself: `query()` feeds whichever runtime the session/channel
    is bound to.
11. **Self-identity is discoverable in-band: `caps` gains an append-only
    `namespace` (or `whoami`) key reporting the caller's ordinal.** Under
    A the ordinal is load-bearing on the content plane — an agent must
    know N to mint a single valid object id, because the id's 7-bit
    namespace field is checked against ingress (#12, `src/ai.rs:384`,
    `:492`, `:535`; layout `src/osc.rs:41-51`) — yet the spawn ack returns
    the new handle to the *spawner's* stream, never to writers on the new
    runtime, and the shipped `caps` reply carries no identity field
    (keys enumerated at `protocols/query.md:137-160`). The recommendation
    follows the `viz_kinds` precedent: `caps` keys are append-only by
    documented contract ("Keys are append-only so older clients keep
    parsing newer replies", `src/query_channel.rs:565-568`; `viz_kinds` at
    `:620`), and the value is `0` on the first runtime — semantically
    identical to today's `namespace() → 0`. Agents inside spawned runtimes
    must feature-detect the key before minting ids; old tooling is
    untouched on the first runtime only (see Migration).
12. **The terminal owns the handle-to-ordinal map.** The
    `<session-nonce-hex>-<seq>` wire handle (item 6) and the 7-bit trust
    ordinal (item 3) are two identifier spaces naming the same runtime.
    The binding table lives terminal-side only; entries die with the
    runtime; handles never encode or derive from ordinals — knowing a
    handle grants nothing and reveals no namespace.

## Who may speak 777, per runtime

Grounded in the trust spine, unchanged in shape:

- **Grants are per transport class, ordinal as data.** The capability
  derivation stays pure and total over `(IngressSource, AppConfig)`
  (`src/capability.rs:13-16`); the deliberately wildcard-free exhaustive
  match (`src/capability.rs:22-26`, `:51-58`) still stops compilation when a
  variant is added, forcing each transport class's grants to be decided
  explicitly, out-of-band. A grant to `[trust.local]` is a grant to every
  local runtime — per-pair granularity ("A may write B but not C") is
  inexpressible under wire-immutable load-time config. On the *content*
  plane Model A does not need it, because reach is possession. Stated as an
  explicit assumption for #56, because the coarseness is symmetric with
  Model E's, not escaped: the #49 lifecycle verbs (close/focus by handle)
  routed into the gated class ARE cross-runtime reach, and under
  class-keyed grants any granted local runtime may close any other local
  runtime — "A may close B but not C" is inexpressible under A exactly as
  under E. Recommendation: the lifecycle grant bit defaults to DENY —
  unlike the shipped avatar-scene bit, which defaults to granted
  (`avatar_scene_defaults_to_granted`, `src/capability.rs:86-91`) — a #56
  knob, flagged here.
- **Within a runtime, ownership is by construction.** Records stay keyed by
  (ingress namespace, caller-local id), namespace stamped by the transport at
  apply time — a stream can only populate its own rows; no owner check
  exists to bypass (`src/presence.rs:12-18`). Display names/colors remain
  rendering metadata, never authentication.
- **Control-plane families stay wire-origin-only** (`src/presence.rs:22-27`):
  replay cannot forge liveness/eviction, and under A there is no
  runtime-targeting control family for replay to hijack (the Model S fatal).
- **Read scope stays the three namespace-based tiers**
  (`protocols/query.md:112-128`): scene-global public, own namespace in full
  including expired rows, foreign namespaces fresh-only
  (`src/presence.rs:52-58`). The census's "new tier question" — is another
  runtime's state a foreign public projection or unreadable? — resolves under
  A as **not wire-addressable**: observe another runtime by holding its
  channel, or via tier-B relay spectation. No fourth tier is minted.
- **Per-runtime diagnostics come for free**: `AiDiagnostics` rings and
  `ReactiveRegistry` halves keyed by namespace u8
  (`src/query_channel.rs:179-183`) separate cleanly once each runtime is its
  own ordinal — no Entity-qualification of namespace-keyed structures needed.

## Future principals: relay and remote — options only

Full writer authentication stays **in the map's fog**, exactly as #25 left
it (`docs/research/collaboration-groundwork.md`, "Deferred"). Its forcing
function is intact and mechanical: any new principal class is a new
`IngressSource` variant, and the wildcard-free matches (`namespace()`,
`granted_to`, `send_reply`) stop compilation until that class's namespace and
grants are decided explicitly (`src/capability.rs:22-26`) — the first real
tier-C bridge cannot be built without answering the auth question. All
options below are principal-uniform (the wezterm Domain lesson,
`docs/research/panes-feasibility.md:64-67`): a remote runtime differs from a
local PTY only in which constructor-assigned variant it carries, never in
addressing mechanics.

- **Tier B (`ratty-relay`, tools/)**: read-only spectation of the public
  snapshot; accepts no viewer commands (locked, #25). Therefore: no
  ratty-side writer transport, no `IngressSource` variant, no namespace
  consumed.
- **Tier C (bridge) options**, one to be chosen when the fog lifts:
  - (a) one transport instance per authenticated remote writer — the bridge
    authenticates W pre-ingress, constructs a ratty-side transport bound at
    construction to one runtime (or a dedicated spawned one), stamped
    `Relay { writer }` with its own namespace and `[trust.relay]` class
    grants; reach = channels granted, mechanics identical to local;
  - (b) one transport instance per bridge, all of that bridge's remote
    writers collapsing to one principal — honest (mirrors the local-PTY
    collapse), cheaper on the namespace pool;
  - (c) the bridge granted zero scene-global capability — content-plane
    writes land only in its own namespace universe.
- **Namespace budget options for #56**: one shared 7-bit pool across classes,
  vs a static partition (locals low, relay-minted high). Either way the
  ceiling (128 lifetimes/session, retire-no-reuse) is dictated by the
  `src/osc.rs:41-51` id layout and must be acknowledged.
- **Remote reach into local runtimes**: whether any remote class may exercise
  the gated scene-global/#49 targeted verbs against local runtimes is the
  same class-level config decision as everything else in the spine —
  default deny.

## What #49 and #52 should take

**#49 (replacement commands — spawn/focus/close):**

- The lifecycle verbs take two separate mechanisms, not one: (1) they join
  the `is_scene_global` *classification* (`src/osc.rs:768`) — buying
  privileged-macro classification and the exclusive scene lock at playback
  (#16); and (2) they gain a NEW per-family *capability gate* on the
  `require_scene` pattern (`src/avatar/mod.rs:751-753`), its grant bit
  decided in trusted config like `[trust.local] avatar_scene`, recommended
  default deny (see "Who may speak 777"). The mechanisms are genuinely
  separate today: only the avatar members of the scene-global class are
  gated (`SceneCapability` never appears in `src/ai.rs`, so `mode`/`warp`/
  `reset` apply ungated for any local wire, `src/osc.rs:770`);
  `avatar.speech.clear;ns=` is the one shipped precedent carrying both
  properties. The verbs carry terminal-assigned `<nonce>-<seq>` handles —
  references, never authority. Spawn returns the handle in its ack payload
  (the #18 pattern); a `state.runtimes` 778 op enumerates live handles;
  `caps` advertises the feature. Dead/unknown handles ack `unknown-id`
  explicitly (#21) — never silent.
- Spawn is arg-less: config-default shell only, no command/path arguments —
  the wire can never touch a filesystem path (#12, `src/macros.rs:25-68`).
  The spawn constructor mints the `RuntimeId` ordinal out-of-band; the wire
  never chooses it.
- If callers may *name* terminals, #16's already-exists-unless-replace
  applies verbatim; terminal-assigned handles sidestep it entirely and are
  the default.
- **Content verbs stay out of the gated class.** The scene-global escape
  hatch is the model's one exception and bears standing scope-creep
  pressure; every future cross-runtime want must be either classified into
  it with handles or declared impossible. If the product answer is genuinely
  single-stream conductor orchestration, take Model E's `term=` key as the
  documented additive layer — do not widen the gated class instead.

**#52 (arbitration):**

- The scene-global cluster (stage `mode`/`warp`/`reset`'s stage half, the
  order-10 effects wash, the sound mixer and ambient slot, avatar) now
  arbitrates among N principals carrying *distinct* `Local(ordinal)` stamps.
  Fair-queue keying and the avatar speaker attribution — the wire-surface
  row the census routed to #50 (`src/avatar/mod.rs:717`), resolved here
  into per-runtime ordinals — can key the ordinal directly instead of a
  collapsed ns-0; `SoundState.play_buckets` (`src/sound.rs:280-282`)
  separates per-principal for free — the census ruled the ns-0 merge
  acceptable for a shared speaker, and under A #52 gets real per-principal
  buckets anyway.
- **No `term=` semantics question exists for #52 to inherit.** Model E's
  ordering hazard (defining targeted semantics on scene-global families
  before #52 locks their scoping) dissolves: scene-global families remain
  untargeted, and #52 decides scoping freely.
- `reset`'s mixed halves split cleanly: its object/bookmark/cursor halves
  are runtime-scoped and apply to the arrival runtime; its stage half is
  #52's to arbitrate. That split is this document's recommendation — the
  census's only `reset` note is the macro-slot clear in its `MacroRegistry`
  row; it never re-scopes `reset`'s halves.
