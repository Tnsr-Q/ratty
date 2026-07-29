# The replacement command family: terminals on the wire (`term.*`)

> **Superseded in part — the lock happened.** #56 resolved on 2026-07-29 in one
> dated resolution:
> <https://github.com/Tnsr-Q/ratty/issues/56#issuecomment-5114257976>.
> **Read it before acting on this document; where the two disagree, the
> resolution wins.** The family shape, creator-scope plus default-DENY, terminal
> #1 wire-unkillable by construction, control-plane + wire-origin-only, and the
> pane four staying `unsupported` were all confirmed. What changed:
>
> - **§4's spawn ack breaks a shipped invariant and is replaced.**
>   `protocols/query.md:102-104` states that **absence from `state.executions` is
>   the completion signal**, so acking `code=started` while deliberately keeping
>   the handle out of that roster makes a conforming caller poll, find nothing,
>   and conclude the spawn **finished** — while it is still spawning. #56
>   decision 19: `term.spawn` is **not** an #18 long-running operation. It acks
>   `ok=1` immediate-commit carrying the handle, and readiness is the explicit
>   `state` field on the `state.terminals` row (`spawning → ready`). Stated
>   affirmatively so nobody later "restores consistency" by adding `code=started`
>   back — a terminal is not transport-epoch metadata.
> - **§3's single `TerminalLifecycle` bit is split in two.** #51 established that
>   focus is the keystroke-capture primitive, not a convenience verb; under one
>   bit an operator granting workspace choreography (spawn + place — this
>   document's own named future work) must also grant keystroke redirection. #56
>   decision 18: **`TerminalLifecycle`** (spawn/close/place) and
>   **`TerminalFocus`** (focus), both default DENY. `place` deliberately stays in
>   the lifecycle bit rather than folding under the default-*granted*
>   `SceneStage`.
> - **Reconciliation item 5 is void.** #56 decision 2 split `TerminalId`
>   (monotonic, never reused) from a **recyclable** 128-slot namespace pool, so
>   you can no longer exhaust by lifetime and the `session-budget` vocabulary is
>   not borrowed for terminals. The pool size **is** the live cap; the initial
>   config default is **4**, gated on a shared parley `FontContext` before rising.
>   Recycling is safe only under decision 17's rule: **every persisted stamp keys
>   on `TerminalId`, never on the namespace.**
> - **Reconciliation item 1's conclusion stands but its supporting argument is
>   stale** — it cited `term=` staying unsquatted, and #56 decision 6 retired
>   `term=` entirely.
> - **Item 4 never stated `state.terminals`' scope**, though its rows carry
>   `creator`. #56 decision 15: scene-scoped for embodiment fields, **`creator`
>   own-scoped**.
> - **§8 handed focus behavior back to #51, which had handed spawn-auto-focus and
>   close-fallback here** — neither ticket owned them. #56 decision 8 adopts them.

Research asset for [wayfinder ticket #49](https://github.com/Tnsr-Q/ratty/issues/49)
(map [#42](https://github.com/Tnsr-Q/ratty/issues/42)). **Recommendation only —
the lock happens at the spine grilling
([#56](https://github.com/Tnsr-Q/ratty/issues/56)).** Inputs: the per-runtime
census (`docs/research/per-runtime-spine.md`, its #49 handoff), the #50
addressing recommendation (`docs/research/addressing-and-trust.md`, Model A),
and the [#22](https://github.com/Tnsr-Q/ratty/issues/22) graduation ruling.
Where this document and the #50 draft differ, the delta is flagged in
[Assumptions for #56](#assumptions-for-56); neither locks until the grilling.

Precedent locked upstream — built on here, never reopened:

- **Superseded-pending, frozen wire** (#22 ruling): `SplitPane` / `FocusPane` /
  `ResizePane` / `ClosePane` stay in the enum with wire shapes frozen exactly
  as committed (`src/osc.rs:351-375`; parse arms `:953-968`), rejecting
  `codes::UNSUPPORTED` via the `apply_ai_commands` catch-all
  (`src/ai.rs:313-325`). "No new capability is built against the split-tree
  surface." Placement, not splits.
- **Model A — arrival is the address** (#50 recommendation, same grilling):
  no content-plane command ever names a runtime; the #49 lifecycle verbs are
  the sole targeted surface, carrying terminal-assigned
  `<session-nonce-hex>-<seq>` handles (#18 shape — references, never
  authority, never `Entity`, never reused); the terminal owns the
  handle-to-ordinal map (assumption 12); the lifecycle grant defaults to
  DENY (flagged knob).
- **The wire never touches a filesystem or process argument** (#12;
  `src/macros.rs:25-68` extends it: `macro.export;to=` / `macro.run;path=`
  reject `wire-filesystem-access`).
- **Acks**: any 777 command opts in with `tok=` (`src/osc.rs:17-22`, `:33`);
  the ack fires once, after rejection or immediate-mutation commit; exactly
  one handler system owns each command's ack (`src/ai.rs:86-92`);
  long-running operations ack `started`/`queued` with a handle
  (`src/query_channel.rs:258-280`).
- **Explicit failure, never silent** (#21): dead/unknown handles answer
  `unknown-id` (`src/query_channel.rs:150-157` is the staleness template).
- **No in-band identity** (#25): sources are assigned pre-ingress,
  constructor-side (`src/runtime.rs:36-50`).

## The question, precisely — and what the inputs hand over

The census hands #49 a spawner-shaped target: creation is `RuntimeOptions`
plus a placement; focus is a write to the (undesigned) focus authority; close
is a despawn running the shutdown that `shutdown_terminal_runtime_on_exit`
does once today (`src/systems.rs:154`) — with the hard constraint that
creation must invoke the *whole* per-terminal spawner (runtime + surface +
textures + planes + organ components), `setup_scene`'s split being the
prerequisite (census, "What each downstream ticket takes"). The #50 draft
hands over the verbs' trust shape: they join the scene-global class, gain a
new per-family capability gate recommended default-DENY, and carry #18
handles ("What #49 and #52 should take"). The #22 ruling hands over the
charter seed — terminal quads with "#12-style namespaced ids and ownership,"
refined by #50's Model A into #18 handles for the targeted verbs — and the
frozen disposition of the four pane commands.

What remains for this document: the verb set and exact wire shapes; how
handles bind to entities and to #12-style ownership; ack/error semantics
including the long-running spawn and the exactly-one-handler transition; the
placement vocabulary; and the landing-day story for the four frozen pane
commands.

## 1. The verb set and wire shapes

**Family name: `term.*`** — four actions, in the shipped
`ratty:<action>;<payload>` grammar, `<payload>` being `k=v&k=v…` with
percent-encoded values (`src/osc.rs:6-15`):

```
ratty:term.spawn[;x=<f32>&y=<f32>&scale=<f32>&cols=<u16>&rows=<u16>&tok=<t>]
ratty:term.place[;id=<handle>&x=&y=&scale=&cols=&rows=&tok=<t>]
ratty:term.focus[;id=<handle>&tok=<t>]
ratty:term.close[;id=<handle>&tok=<t>]
```

Enum variants (the shared std-only module, next to the frozen pane block):

```rust
// ── Terminals (#49; supersedes the frozen pane block above) ──
TermSpawn  { x: Option<f32>, y: Option<f32>, scale: Option<f32>,
             cols: Option<u16>, rows: Option<u16> },
TermPlace  { id: Option<String>, x: Option<f32>, y: Option<f32>,
             scale: Option<f32>, cols: Option<u16>, rows: Option<u16> },
TermFocus  { id: Option<String> },
TermClose  { id: Option<String> },
```

Wire-shape rules, each from a shipped precedent:

- **Which verbs carry handles.** `term.place` / `term.focus` / `term.close`
  *accept* an optional `id=<handle>`; **absent `id=` targets the carrying
  terminal** (the absent-target-equals-arrival default the #50 draft cites
  from `macro.play` scope resolution). `term.spawn` *returns* a handle and
  never accepts one — there is no caller-chosen terminal identity, which
  sidesteps #16's `already-exists` collision machinery entirely
  (terminal-assigned handles are the #50 draft's stated default). If callers
  may ever *name* terminals, #16 applies verbatim — noted, not designed.
- **The handle key is `id=`, not `term=`.** `avatar.cancel;id=` is the
  shipped precedent for a session-scoped handle riding `id=`
  (`src/osc.rs:541-546`). Critically, `term=` stays unclaimed: it is the
  #50 draft's *recorded escape* — Model E's future envelope key, extracted
  in `parse_control` before command parse if conductor orchestration is
  ever proven needed. A `term.*` family whose own payloads used `term=`
  would squat the escape and make its later addition a silent
  reinterpretation of shipped bytes (`Payload::parse` stores every `k=v`
  and commands read only known keys, `src/osc.rs:1272-1284`). `id=` keeps
  the escape clean.
- **Handles validate at apply, not parse.** `id=` rides as an opaque string
  (the `avatar.cancel` shape); unknown, foreign-session, or dead handles
  reject `unknown-id` at apply — mirroring
  `QuerySession::owns_execution_id`'s explicit-staleness rule
  (`src/query_channel.rs:150-157`).
- **Numerics are strict.** Every numeric key uses the `opt_strict` pattern
  (`src/osc.rs:997-1008`, the M3 retirement of lenient default-0): absent
  picks the terminal-side default, present-but-malformed fails the whole
  parse — the envelope still acks `bad-command` because `tok=` is extracted
  before command parse (`src/osc.rs:845-860`).
- **Partial-update atomicity.** `term.place` follows `avatar.set`: every key
  optional, one invalid field rejects the whole command
  (`src/osc.rs:497-501`); an all-absent `term.place` is a vacuous commit,
  acked ok.
- **No runtime arguments on `term.spawn`, ever.** No command, no cwd, no
  env: `RuntimeOptions.command` / `working_dir` (`src/runtime.rs:28-34`)
  are CLI/config-side seeds only — the wire can never touch a filesystem
  path or choose an executable (#12; the `src/macros.rs:25-68` extension).
  A spawned terminal runs the config-default shell via the existing
  constructor (`TerminalRuntime::spawn(&AppConfig, &RuntimeOptions)`,
  `src/runtime.rs:466`, with default options; `virtual_channel` on wasm,
  `:430`). Absent `cols=`/`rows=` fall back to
  `config.terminal.default_cols/default_rows` — the exact defaulting both
  constructors already perform (`src/runtime.rs:431-432`, `:467-468`).
- **`tok=` is orthogonal on all four**, as on every command
  (`src/osc.rs:17-22`).

**Read side.** One new 778 op enumerates the roster: this document
recommends **`state.terminals`** (the #50 draft wrote `state.runtimes`; the
naming must reconcile at #56 — see Assumptions). Rows are tier-1
scene-global public state in #18's read scope (the quads are visibly on
screen; #18's boundary "visibility grants observation, not control" does the
work — knowing a handle grants nothing):

```
{ "id": "<handle>", "state": "spawning|ready|closing",
  "ns": <ordinal>, "creator": <ordinal|null>,
  "x": …, "y": …, "scale": …, "cols": …, "rows": … }
```

`caps` gains one append-only `terminals` key advertising the feature and its
limits (the documented append-only rule, `src/query_channel.rs:565-566`; the
`viz_kinds` precedent, `:620`). The `namespace` self-identity key is #50's
(its assumption 11), not duplicated here.

## 2. Handles, entities, and ownership

**The binding.** One new screen-global resource — call it
`TerminalRegistry` — owns the roster: `handle → (Entity, ordinal,
creator_namespace, state)`. This is the terminal-side handle-to-ordinal map
the #50 draft's assumption 12 requires (entries die with the runtime;
handles never encode ordinals). It is legitimately screen-global — it *is*
the cross-terminal roster — and it is the only new singleton the family
adds. Internally the routing key stays the terminal `Entity` (census
decomposition: scene joins speak `Entity`; generational ids make a
despawned terminal's stale `Entity` fail lookups safely). Handles are
minted through the existing app-global
`QuerySession::mint_execution_id` (`src/query_channel.rs:140-148`) — same
`<nonce-hex>-<seq>` shape, same never-reused guarantee, uniqueness shared
with execution handles, zero new mint code. The runtime's trust ordinal is
minted separately by the spawn constructor, out-of-band, per #50 (the wire
never chooses it); handle and ordinal are two identifier spaces bound only
in this registry.

**Does a spawned terminal belong to the creating caller's namespace like
objects do?** Both answers, weighed:

- **Creator-owns (the #12 transplant).** The spawner's namespace owns the
  spawned terminal's *lifecycle*: only the creator may `place`/`focus`/
  `close` it by handle, enforced like `object.*` ownership — a check at the
  lowering layer rejecting `not-owner` (`src/ai.rs:384-393` is the
  template). For: least authority; the conductor pattern (spawn K workers,
  manage them) works within it; and it *expresses* the per-pair authority
  the #50 draft proved inexpressible as a *grant* ("A may close B but not
  C" — inexpressible under wire-immutable class-keyed config) — because
  creator-scope is not a grant, it is an ownership check over data recorded
  at spawn time, exactly #12's mechanism. A load-bearing corollary:
  terminal #1 has no creator (user/CLI-constructed, census spawner
  policy), so under creator-scope **the boot terminal is unaddressable by
  any wire caller — wire-unkillable by construction**. Against: it
  re-imports a single-owner worldview one level up (the ecosystem vision's
  pane note warns against exactly that,
  `docs/ecosystem-vision.md:121-122`), and it needs an orphan rule.
- **Scene-owned (no per-caller ownership).** Terminals are shared workspace
  owned by the scene/user; the verbs are pure capability-gated scene-global
  operations like `avatar.set` — any granted caller may address any
  terminal by handle. For: honest about the class-grant coarseness the #50
  draft states; no ownership table, no orphan rule; terminals-as-peers
  matches the vision. Against: any granted stream may close any terminal —
  including the user's primary session — and no least-authority refinement
  is possible later without *narrowing*, which the season's own rule
  forbids (#18: "widening is compatible; narrowing is not").

**Recommendation: creator-scope layered inside the default-DENY gate — both
checks required.** Handle-carrying forms require (1) the family capability
and (2) creator match; bare forms target self and require only the
capability. The migration asymmetry decides it exactly as it decided Model
A: starting narrow (deny + creator-scope) and widening later (dropping the
creator check for granted callers, or per-verb) is additive; starting wide
and narrowing breaks shipped conductors. Orphan rule: a creator's death
does **not** cascade-close its children (they are principals, possibly
with a user inside); orphans become creator-less like terminal #1 —
wire-unaddressable, closed by the user or process exit. Cascade-close as a
spawn-time option is future work, not designed here.

**Lifecycle is owned; contents are not.** The precise answer to the ticket's
question: a spawned terminal belongs to the creator's namespace *as a
lifecycle resource* — like an object — but its **content universe does
not**: under Model A each spawned runtime is a new principal with its own
namespace ordinal and its own object/macro/sensor rows, and the creator
cannot reach inside it without holding its channel (content reach stays
channel possession; the read tiers gain no runtime axis — #50 assumptions
1, 8). Owning the quad is not owning the agent inside it.

## 3. Capability, classification, and the control plane

The #50 draft assigns the verbs two separate mechanisms — scene-global
*classification* (`is_scene_global`, `src/osc.rs:768-781`) plus a new
per-family capability *gate* on the `require_scene` pattern
(`src/avatar/mod.rs:751-762`, the `granted_to` call at `:753`). Adopted
verbatim, plus one addition this document argues is mandatory:

- **Gate: `SceneCapability::TerminalLifecycle`**, derived purely from
  `(IngressSource, AppConfig)` (`src/capability.rs:51-58`), configured at
  `[trust.local] terminal_lifecycle`, **default DENY** — the #50 draft's
  flagged recommendation, deliberately unlike the avatar-scene bit's
  default-granted posture (`src/capability.rs:86-91`). One bit covers all
  four verbs (splitting spawn from the targeted verbs is a #56 knob).
  Refusals reject `not-permitted`, the `require_scene` code
  (`src/query.rs:116`). The whole family is gated — including bare
  self-forms: even self-place and self-focus mutate shared scene
  composition and input routing, and relaxing bare forms later is a
  compatible widening.
- **Classification: all four variants return `true` from
  `is_scene_global`** — the #50 line, kept.
- **Addition: the family is control-plane and wire-origin-only.** A new
  `is_terminal_control()` arm folds into `is_control_plane()`
  (`src/osc.rs:727-736` — the #16 exclusion list, already amended once by
  #21 and once by #25), and the applier refuses any
  `CommandOrigin != Wire` exactly as presence does
  (`src/presence.rs:1148-1152`, the refusal at `:1186`). Rationale, in
  #25's own terms: **principal lifecycle is ingress truth.** A replayed
  `term.spawn` executes a process and burns an ordinal from the
  128-lifetime session budget (#50 assumption 5 — a looping macro could
  exhaust the namespace space); a replayed `term.close` would kill a live
  session a user may be typing into; and the handle-carrying forms are
  transport-epoch metadata that #18 already bars from recordings and
  trusted macros (`is_execution_control` rationale, `src/osc.rs:783-791`;
  the recorder tap skips control-plane and execution-control commands,
  `src/macros.rs:880-881`; the trusted loader refuses execution-control
  steps, `src/macros.rs:344-359`).
- **Consequence, stated honestly:** because the family can never be
  recorded, the privileged-macro/scene-lock machinery that
  `is_scene_global` feeds (`src/macros.rs:528` is its one consumer) never
  actually sees a `term.*` — the classification is belt-and-suspenders
  against future injection paths, and the operative protections are the
  gate plus the origin refusal. This supersedes the #50 draft's stated
  *reason* for scene-global membership (privileged classification at
  playback assumed the verbs were recordable) with a strictly stronger
  guarantee; flagged in Assumptions for #56 to reconcile.
- **Not rule-safe** falls out for free: `is_rule_safe_action` is a closed
  allowlist (`src/osc.rs:748-761`); `term.*` is not in it, so rules cannot
  fire terminal lifecycle even indirectly (rule-fired macros can never
  contain it either — it cannot be recorded).
- **Deliberately sacrificed in v1: workspace macros.** A trusted macro
  that opens a three-terminal layout ("spawn, spawn, spawn — each with
  inline placement") is a real future want, and inline placement on
  `term.spawn` was shaped partly so it could work handle-free. It still
  cannot, under wire-origin-only. The relaxation path is narrow and
  additive — admit `TermSpawn`/bare `TermPlace` into *trusted* macros only,
  keeping session recordings excluded — and belongs to a future ticket,
  not v1.

## 4. Ack and error semantics on the #18 path

All four verbs follow the shipped contract: `tok=` opt-in, exactly one ack
per command, emitted after rejection or immediate-mutation commit
(`src/osc.rs:17-22`); token-less commands stay fire-and-forget with
failures visible in the caller's `state.errors` ring
(`AiDiagnostics.record`, `src/query_channel.rs:282-304`).

**`term.spawn` is the family's long-running operation** (the ticket's
started/handle pattern; the #50 draft: "Spawn returns the handle in its ack
payload (the #18 pattern)"):

- Admission is synchronous: gate check, argument validation, ordinal-budget
  check, live-cap check. Any failure acks `ok=0` immediately — no handle is
  minted (the avatar precedent: a mint consumed by a rejected admission is
  discarded, `src/avatar/mod.rs:932-933`; here the mint simply happens
  after admission).
- On admission: mint the handle, insert the registry row as
  `state=spawning`, construct through the whole per-terminal spawner
  (census constraint), and ack exactly once via
  `ack_commit_long_running` (`src/query_channel.rs:258-280`):
  `ok=1;code=started;data={"id":"<handle>"}` — the started qualifier is the
  shipped `codes::STARTED` (`src/query.rs:147`).
- **One handle, one row, one lifecycle:** the started payload's `id` is the
  *terminal* handle, and status is polled through `state.terminals` (the
  row's `state` flips `spawning → ready`), not through `state.executions`.
  This narrows #18's "inspected through explicit status queries" to the
  family's own roster op rather than minting a parallel execution row that
  would vanish at readiness — a deliberate divergence from the avatar
  pattern, flagged for #56.
- Failure after admission (PTY spawn fails, surface allocation fails) is
  never silent (#21): the row leaves the roster, subsequent handle use
  answers `unknown-id`, and the failure lands in the *spawner's*
  diagnostics ring with a `term.spawn` action record.
- A token-less spawn still works: the creator discovers the handle from
  `state.terminals` (rows carry `creator`).

**`term.place`, `term.focus`, `term.close` are immediate-commit acks** —
`ack_commit` (`src/query_channel.rs:220-235`). Close commits when the row
transitions `ready → closing` and teardown is irrevocably scheduled
(despawn runs the per-entity shutdown that `src/systems.rs:154` runs once
today). Closing a still-`spawning` row is legal and cancels the spawn —
one verb covers mid-flight cancellation; no `term.cancel` exists.

**The self-close flush constraint.** Replies follow ingress (#18/#50
assumption 7). For `term.close` with absent `id=`, the ack's destination
is the dying terminal's own input stream — so teardown must complete
*after* the frame's reply flush (`answer_queries` drains acks into the
origin runtime's `write_input`): the ack is the last bytes that stream
receives. An implementation constraint, stated here so it is tested, not
discovered.

**Error catalog** — shipped codes only, plus at most one addition:

| Code | When |
| --- | --- |
| `bad-command` (`src/query.rs:72`) | parse failure (malformed strict numeric, unknown action) — acked because `tok=` extraction precedes command parse (`src/osc.rs:845-860`) |
| `not-permitted` (`:116`) | the `terminal_lifecycle` gate refuses the caller's class |
| `not-owner` (`:76`) | granted caller, handle names a terminal it did not create |
| `unknown-id` (`:81`) | dead, foreign-session, or never-minted handle |
| `session-budget` (`:89`) | ordinal exhaustion — the 128-lifetimes-per-session ceiling (#50 assumption 5), reusing the session-budget vocabulary of the object-id ledger (`src/ai.rs:424`) |
| `started` (`:147`) | the `ok=1` spawn qualifier |

If #54's performance data demands a live-terminal cap in trusted config,
its code is minted at implementation under the per-family catalog
discipline; nothing else new.

## 5. Exactly one handler: the ack transition

**Today, the four pane variants are precisely the catch-all's residue.**
Every other variant group has an explicit (mostly empty) arm in
`apply_ai_commands` naming its ack owner (`src/ai.rs:244-312`); only
`SplitPane`/`FocusPane`/`ResizePane`/`ClosePane` reach `other =>`, which
rejects `codes::UNSUPPORTED` with "command parsed but its subsystem is not
built yet" (`src/ai.rs:313-325`).

**The landing-day sequence, one commit:**

1. A new organ module (`src/terminals.rs`) adds
   `apply_terminal_commands`, reading the same `AiCommand` stream and
   owning the `term.*` acks — the per-organ pattern verbatim.
2. The same commit adds the four `Term*` variants **and** their empty arm
   in `apply_ai_commands` with the standard comment ("reads the same
   AiCommand messages independently and owns their acks, so this catch-all
   must never double-ack them" — the `src/ai.rs:292-294` wording).
3. **Harden the catch-all into a designed compile-break:** replace
   `other =>` with an explicit
   `SplitPane | FocusPane | ResizePane | ClosePane` arm carrying the same
   `UNSUPPORTED` reject — the match becomes wildcard-free, so *every*
   future enum variant is a compile error at this match until its ack
   ownership is explicitly decided. This is the codebase's own idiom
   (`SceneCapability::granted_to`'s deliberately wildcard-free match,
   `src/capability.rs:51-58`; `send_reply`'s routing match) applied to the
   one place the no-double-ack invariant currently rests on comments and
   tests alone. There is no double-ack window because the variants, the
   empty arm, and the applier land atomically — and after this commit the
   compiler closes the window for every family that follows.
4. `answer_queries` ordering gains one line —
   `.after(apply_terminal_commands)` in the `src/ai.rs:153-168` list — so
   a same-chunk spawn-then-query observes the row and acks flush in
   command order.
5. An every-build round-trip test pins exactly one reply per token for a
   `tok=`-carrying `term.spawn` and for a frozen `pane.split` (the sound
   organ's feature-off no-double-ack posture, `src/ai.rs:264-267`, is the
   template).

## 6. Placement semantics: quads in the scene, not splits

**Vocabulary.** Terminals are *not* cell-anchored: the object/viz families
anchor to cells of a carrying grid (`SpawnObject.x/y` are anchor
column/row, `src/osc.rs:193-196`), but a terminal *is* a grid — it has
nothing to anchor to. The shipped geometry for a terminal's placement is
`TerminalViewport` — size in logical pixels, center in world space
(`src/scene/mod.rs:66-73`), applied to plane transforms by
`sync_terminal_layout` and made per-entity by the census (its
`TerminalViewport` row). So the wire vocabulary is scene-space:

- `x=`/`y=` — world-space center of the quad (f32, the
  `TerminalViewport.center` axes). Exact units/ranges pin at
  implementation against `sync_terminal_layout`; the *shape* is two strict
  f32 keys.
- `scale=` — uniform quad scale (f32, default 1.0), the `object.add`
  convention (`src/osc.rs:197-198`).
- `cols=`/`rows=` — the PTY grid (u16), the viz footprint vocabulary
  (`viz.set;cols=/rows=`, `src/osc.rs:322-325`) — deliberately *not* the
  frozen pane family's `width=`/`height=` (`src/osc.rs:363-370`).

**Per-field cost honesty** (#12's pattern: per-field costs documented, not
hidden): `x`/`y`/`scale` lower onto the quad transform — live, cheap,
render-only. `cols`/`rows` is a real PTY resize — `runtime.resize` plus
SIGWINCH-class reflow (the path `handle_window_resize` drives today,
`src/systems.rs:322-328`) — respawn-class in spirit; callers should treat
it like `object.update`'s re-anchor, not like its live fields.

**What follows the object/viz conventions is the *wire discipline*, not the
coordinate space**: strict present-but-malformed rejection, optional keys
with terminal-side defaults, atomic partial update, and #12-style
creator ownership of the placed thing (section 2). The id vocabulary
diverges from #12 by design: #18 handles, terminal-assigned, because a
terminal is a principal and its identity must not be wire-mintable
(section 1; the #22 charter's "#12-style namespaced ids" is refined, not
contradicted — ownership semantics survive, the id *format* is #18's).

**Nothing anywhere is a split.** No `direction=`, no `ratio=`, no implicit
geometry derived from another terminal's quad. Default placement when
`term.spawn` omits `x=`/`y=` is a terminal-side layout policy — and *whose*
policy that is (auto-layout vs user arrangement vs the spawner) is
explicitly #52's arbitration edge, not designed here.

## 7. The superseded four: disposition on landing day

**Recommendation: `pane.split` / `pane.focus` / `pane.resize` /
`pane.close` stay `UNSUPPORTED` forever. No re-lowering, ever.**

The alternative — re-lowering the old bytes onto the new family
(`pane.split → term.spawn`, `pane.focus;pane=N → term.focus`) — fails
three ways:

1. **It violates the #22 ruling directly.** "No new capability is built
   against the split-tree surface" — re-lowering *is* new capability
   against that surface: bytes that reject today would begin mutating the
   scene, a silent behavior change on a wire #22 froze precisely so its
   meaning could never drift.
2. **The mapping cannot even be written.** `SplitPane` carries no pane id
   at all (`src/osc.rs:351-356` — direction and ratio only), and
   `FocusPane`/`ResizePane`/`ClosePane` carry a raw `u8`
   (`src/osc.rs:357-375`) with no binding to anything: mapping u8 ordinals
   onto handles would mint a third identifier space — positional, racy,
   renumbering on close — exactly the bespoke pre-#12 shape the season
   retired.
3. **Author intent does not survive the translation.** A script emitting
   `pane.split;ratio=0.3` wants a tmux split of the current viewport;
   `term.spawn` creates an independent placed principal. Guessing is worse
   than rejecting.

**Mechanics of "forever":** the enum variants and parse arms stay exactly
as committed (frozen wire — deleting the parse arms would *change* shipped
behavior, downgrading a clean `unsupported` to the unknown-action
`bad-command` path). The ack code stays `codes::UNSUPPORTED` (#22 wrote
`code=unimplemented`; it landed as the #18 catalog's `unsupported` — the
code is the contract and does not change). The only permitted edit is the
one section 5 already makes: moving the four out of `other =>` into their
own explicit arm, whose reject *message* becomes honest — "superseded by
`term.*`" instead of "subsystem is not built yet," which becomes a lie the
day the subsystem exists. Messages are prose, not contract: diagnostics
already truncate them at the storage boundary
(`src/query_channel.rs:193-205`); codes are what clients parse.

The protocol doc for the new family (`protocols/` gains a `terminals.md`
beside the other organ contracts) documents the pane family in a
"superseded surface" appendix: shapes, the permanent `unsupported`
disposition, and the pointer to `term.*`.

## 8. What stays open — #51 and #52 boundaries

**#51 (focus and input routing) takes focus *behavior*; #49 fixes only the
wire shape.** Locked here if this document survives the grilling:
`term.focus[;id=<handle>][;tok=]`, gate + creator-scope, control-plane,
`ack ok=1` when the focus-authority write commits, the standard error
catalog. Explicitly *not* designed here: what the focus authority is (the
census's `FocusedTerminal` is a named placeholder — resource vs component
vs per-window, and its cardinality, is #51's first question); what focus
does to keyboard/mouse routing (`handle_keyboard_input` writes
unconditionally today, census row); focus-follows-click and its interplay
with wire focus; and whether/when a wire focus may take focus *from the
user* — the user-input-wins precedent (`src/web.rs:429-433`, cited per the
census) suggests the answer, #51 owns it. If #51 lands an
arbitration-denied outcome, its ack code (`not-permitted` vs a new
qualifier) is #51's to pin against the catalog.

**#52 (arbitration) takes every placement-vs-scene-global edge:**

- Default placement policy for `term.spawn` when placement keys are absent
  — who lays out N spawned quads (auto-layout, spawner, user).
- Contention over a quad the user has arranged: does `term.place` lose to
  user drag (the "user input wins" precedent extended to layout)?
- Overlap/occlusion/z policy between quads, and camera framing on spawn
  and close (the stage/camera cluster the census routes to #52 wholesale).
- Whether presentation mode and warp are per-terminal at all (census open
  question) — which decides whether `term.place` ever grows
  per-quad-warp keys. It does not grow them here.
- The `MacroRegistry::scene_lock` scalar stays #52's; under section 3 the
  `term.*` family never reaches playback, so no new lock interaction is
  created.

## Assumptions for #56 (reconciliation list)

1. **Family noun**: `term.*` on 777, `state.terminals` on 778. The #50
   draft wrote `state.runtimes` (`docs/research/addressing-and-trust.md:292`,
   `:538`); one noun must win at #56. This document argues `term`: the
   wire never says "runtime" anywhere today, and the Model E escape key was
   already named `term=` — same noun, and section 1's `id=` choice keeps
   that escape unsquatted either way.
2. **Ownership**: creator-scope *plus* default-DENY gate (section 2) — a
   strict-superset check over the #50 draft's gate-only statement. #50
   called per-pair authority inexpressible *as a grant*; creator-scope
   expresses the creation-graph slice of it as #12-style ownership data.
   Terminal #1 wire-unkillable falls out; confirm both.
3. **Control-plane + wire-origin-only** (section 3) supersedes the #50
   draft's recordability-assuming rationale for `is_scene_global`
   membership; membership is retained as defense-in-depth. Reconcile the
   two documents' wording.
4. **Spawn ack carries the terminal handle and status is polled via
   `state.terminals`**, not `state.executions` — a narrowing of the #18
   long-running pattern to the family roster. Confirm.
5. **`session-budget` reused for ordinal exhaustion**; a live-terminal cap
   waits for #54's performance envelope.
6. **The wildcard-free catch-all** (section 5, step 3) — a one-commit
   hardening that makes ack ownership compile-checked for every future
   family. Independent of `term.*`; could land first.
7. **Workspace macros** (trusted-tier `term.spawn` with inline placement)
   are named future work, excluded from v1 by wire-origin-only.
8. **Prerequisites, per the census**: `setup_scene`'s split and the
   per-terminal spawner exist before any of this lands (#54 proves the
   seam multiplies; #49's family is the wire onto that spawner, never onto
   bare runtime construction).
