# Screen-global vs per-runtime arbitration

> **Superseded in part — the lock happened.** #56 resolved on 2026-07-29 in one
> dated resolution:
> <https://github.com/Tnsr-Q/ratty/issues/56#issuecomment-5114257976>.
> **Read it before acting on this document; where the two disagree, the
> resolution wins.** Items 1, 3, 4, 5, 7, 8 and 9 were ratified substantially as
> written. What changed:
>
> - **§4's rationale for `SceneStage` defaulting to granted is replaced.** The
>   conclusion stands; "directing your own camera is single-tenant
>   self-direction" does not — at N>1 the camera is by definition not
>   single-tenant, so as stated it argues for the opposite. The surviving reason:
>   this class is **ungated today** (`SceneCapability` appears nowhere in
>   `src/ai.rs`), so default-grant is a *tightening*, while default-DENY would be
>   a tightening plus a behavior break. That makes it the same principle as #50's
>   default-DENY, not a contradiction: both pick the default that does not change
>   shipped behavior.
> - **§4 left undefined what happens when a verb spans both halves.** `mode` has
>   an ungated shape half and a gated view half. #56 decision 13: **whole-command
>   refusal** — no `SceneStage`, `mode` acks `not-permitted` and applies neither.
> - **§1's effects render "knob" is not a knob.** The compositor is a single
>   8000×8000 flat-color sprite (`src/effects.rs:341-364`); strict per-surface
>   cannot be expressed against it at all and means abandoning the overlay for
>   per-plane material tinting. Focused-wash is locked for v1 and strict
>   per-surface is **reclassified as future work**, joining return #1. Stated
>   gap: under focused-wash an **unfocused agent's mood does not render at all**.
> - **§3's offer to pin the old warp/camera-tween coupling is declined.** Pinning
>   it would let an **ungated** `warp` cancel **`SceneStage`-gated** state, and
>   "pin it for the arrival runtime" does not localize anything because every
>   command has an arrival runtime. Channel-scoped cancellation is the rule.
> - **§5 never stated `state.terminals`' scope**, and its rows carry `creator`.
>   #56 decision 15: scene-scoped for embodiment fields, **`creator` own-scoped**.
> - Two things are now stated as decisions rather than left implicit: the scene
>   lock stays **fail-fast** (a real cross-runtime rejection at N>1), and the
>   holder key's 7 bits, the 128-slot pool and the wire's `& 0x7F` **must agree**.

Research asset for [wayfinder ticket #52](https://github.com/Tnsr-Q/ratty/issues/52)
(map [#42](https://github.com/Tnsr-Q/ratty/issues/42)). **Recommendation only —
the lock happens at the spine grilling
([#56](https://github.com/Tnsr-Q/ratty/issues/56)).** This is the graduated #10
fog entry ("cross-organ arbitration — which subsystem wins when several organs
drive the same visual state (camera, effects, presence)"): the census
(`docs/research/per-runtime-spine.md`) hands this ticket its 15
ambiguous-arbitration rows as **one cluster plus four scalars**, plus its first
bullet — the presence/viz/avatar/inline classification and the `TerminalOwner`
naming — as a recommendation this ticket ratifies or amends. The addressing
recommendation (`docs/research/addressing-and-trust.md`, Model A — arrival is
the address, `IngressSource::Local(RuntimeId)`, namespace = runtime ordinal) is
assumed as the principal spine throughout; every conclusion that leans on it
says so, so #56 can reconcile the two documents in one sitting.

Precedent locked upstream — built on here, never reopened:

- **The writer order and user-input-wins.** The deterministic stage-writer
  order rgp → ai → web → presentation (`src/web.rs:376-388`) and "JS controls
  are user input: they win over any scripted stage tween"
  (`src/web.rs:428-433`). The census names this pair as the precedent #52
  extends. Keyboard already enforces the same rule — every user mode/warp key
  stops a running wire tween (`src/keyboard.rs:381-392`, `:465-466`).
- **Authority = ingress context, never wire bytes** (#25;
  `src/runtime.rs:36-43`). Capability = pure derivation over
  `(IngressSource, AppConfig)`, wildcard-free match, grants only from trusted
  config (`src/capability.rs:22-26`, `:51-58`).
- **Placement, not splits; N runtimes = N instances of the seam** (#22).
- **Embodiment.** "Each visible terminal corresponds to one real agent" —
  internal state projects physically per terminal
  (`docs/ecosystem-vision.md:79-102`); "pane design should not assume a single
  PTY-owner worldview" (`:123`).

## The question, precisely

The census's 15 rows fuse per-runtime input with shared-scene output. The
cluster is the stage/camera state — `TerminalPresentation`,
`TerminalPlaneView`, `StageTween`, `MobiusTransition` — and its five
writers/consumers (`drain_web_controls`, `apply_rgp_stage`,
`animate_stage_tween`, `animate_mobius_transition`,
`apply_terminal_presentation`); the four scalars are
`MacroRegistry::scene_lock`, the `AiEffects` wash, the `AmbientSlot` bed, and
the `SceneCapability` grant table. The census flags one mechanical
prerequisite: `StageTween` and `MobiusTransition` fuse per-plane channels
(warp, morph) with shared-camera channels and "must split at the field level
before ownership can even be assigned." Verified: `StageTween` carries one
`warp` channel beside `yaw`/`pitch`/`zoom` under one replace-on-write clock
(`src/scene/stage.rs:28-46`, replacement semantics in the struct doc
`:25-27`); `MobiusTransition` carries morph timing
(`active`/`elapsed_secs`/`direction`, `src/scene/mobius.rs:10-15`) beside
saved/target camera state (`source_*`/`start_*`/`end_*` zoom/yaw/pitch/offset,
`:18-41`) and the mode-restore field (`source_mode`, `:16-17`).

Two more findings bound the answer. First, the honesty finding carried from
the addressing doc, re-verified on main: `mode`/`warp`/`reset` are classified
scene-global (`is_scene_global`, `src/osc.rs:768-770`) yet apply with **no
capability check** — `SceneCapability` appears nowhere in `src/ai.rs`
(grep-verified; the only gate sites in the tree are
`src/avatar/mod.rs:751-753` and `src/sound.rs:593-595`), and
`apply_ai_commands` reads the stamped source only to route acks while
mutating the shared presentation unconditionally (`src/ai.rs:201-240`).
Second, a structural gift: RGP `c`-verb stage updates are queued inside
`TerminalInlineObjects` (`pending_stage`, `src/inline.rs:85`) and drained by
`apply_rgp_stage` from that same registry (`src/systems.rs:2505-2514`,
`take_stage_updates` at `src/inline.rs:581-583`) — so once the inline
registry is per-entity (§1), **every RGP stage write carries per-runtime
provenance by construction**, before any addressing work lands.

## 1. The organ classification — ratified four of five, effects decided

The census's first bullet, per organ. "Ratify" means: #52 agrees, with the
evidence restated so #56 locks against code, not against the census's say-so.

**Presence — ratify per-runtime.** The registry's own doctrine makes any
other home wrong: identity is ingress truth — a row is keyed (ingress
namespace, caller-local id), the namespace stamped by the transport at apply
time (`src/presence.rs:11-20`); the whole family is control-plane and the
applier refuses non-`Wire` origins (`:22-27`, enforced `:1145-1152`); rows
anchor to and clamp against the carrying stream's grid (`clamped_cell`
against `cols`/`rows`, `:667-671`, `:952-955`). Under Model A the source axis
*is* the runtime axis, so "the roster travels with the channel" is not a new
rule — it is the existing rule with N channels. The applier's roster/redraw
pair (`:1153+`), the marker sync (singleton surface/viewport/presentation
plus `plane_query.single()`, `:909-962`), and the expiry redraw all route per
owning terminal exactly as the census proposes; the shared caret mesh cache
stays a screen-global asset (`marker_mesh`, `:923`). One sharpening:
`PresenceCursorMarker` is keyed (namespace, id) (`:657-665`) — under A the
namespace already names the owning runtime, but the scene join should still
be `TerminalOwner(Entity)` (§2), not an ordinal→entity side table; the
(namespace, id) pair remains the identity, the owner component is the join.

**Viz — ratify per-runtime.** Entries are grid-anchored and scroll-tracked
against the owning grid (`apply_scroll`, `src/viz.rs:812-830`); the registry's
deliberate id-reuse divergence ("watchers restart under stable ids",
`:647-651`) is a *caller-local* contract that only stays honest if the
registry is caller-scoped — per-runtime registries preserve it; a shared one
would let runtime B's `viz.remove` free an id runtime A's watcher expects.
`apply_viz_commands`' one-registry-one-redraw shape (`:896-901`) routes on
the stamped source to the owner's pair.

**Inline — ratify per-runtime, owner named in §2.** The resource is the
per-transport parse accumulator itself — `pending_bytes`, pending RGP
payloads, the stage-update queue, and `KittyParserState` all live inside it
(`src/inline.rs:81-101`); two interleaved byte streams sharing one would
corrupt mid-chunk Kitty transfers (census). Per-runtime by definition,
exactly as censused.

**Avatar — ratify screen-global.** "Scene-global avatar state" is the
declared design (`src/avatar/mod.rs:618-621`); the presentation layer is one
mascot + one bubble per window on isolated overlay cameras (orders 5/6),
structurally immune to RGP camera writes (`src/avatar/present.rs:5-17`,
"one bubble — the active utterance only", `:19-22`). The organ was built for
N speakers before N existed: the speech queue is namespace-fair, keyed by the
attribution namespace (`:340-368`), with the module doc stating multi-agent
fairness waits only on real distinct ingress (`:35-38`). Under Model A the
fair queue and the per-source capability gate key distinct `Local(ordinal)`
stamps with zero redesign — the census's wire-surface row
(`apply_avatar_commands`, routed to #50) comes back resolved: ordinals feed
the existing keying.

**Effects — the census deliberately left this one to #52; the call: state
per-runtime, compositor screen-global.** The state is per-agent by the
system's own semantics: flash/pulse/tint/think/confidence/mood are "the live
emotional state of the terminal" (`src/effects.rs:118-128`) and the ecosystem
table maps exactly these to per-agent embodiment — confidence → aura, mood,
overload → dimming, per visible terminal (`docs/ecosystem-vision.md:83-96`).
A single shared `AiEffects` under N agents makes mood unattributable — the
opposite of "graphical behavior gets a semantic basis"
(`docs/ecosystem-vision.md:97`). So: `AiEffects` re-derives
`Resource` → `Component` on the terminal entity;
`apply_ai_effect_commands` routes each command on its stamped source to the
owner's component (today it reads `source` only for the ack,
`src/effects.rs:380-426`); each runtime's effect state is its own —
ungated, own-body semantics, unchanged wire. The order-10 overlay
camera/sprite pair stays exactly one per scene (census screen-global row;
`setup_ai_effects`, `src/effects.rs:341-364`), and `animate_ai_effects`'
one-sprite assumption (`sprite.single_mut()`, `:443`) becomes the compositor
loop over N per-terminal states. **Render policy — a #56 knob, not an
ownership question:** the recommended default is that the *focused*
terminal's wash owns the whole-window overlay (user attention extends
user-input-wins; degenerate case N=1 reproduces today's whole-surface wash
byte-for-byte) while non-focused terminals' washes tint their own plane
regions; the strict per-surface alternative changes N=1 visuals (today's
oversized overlay covers the letterbox, `OVERLAY_SIZE`,
`src/effects.rs:23-24`) and the all-moods-blended alternative destroys
attribution. Whichever knob #56 picks, the avatar keeps its shipped contract
— "the effects wash (order 10) still tints the avatar like everything else"
(`src/avatar/present.rs:11-13`) — by taking the focused terminal's wash.
Consequence for the read side: `AiEffectsPublic` (projected into
`state.scene` today, `src/effects.rs:130-133`) moves from the scene tier to
each terminal's public projection row — rendered = public, per terminal
(§6).

## 2. The runtime owner of `TerminalInlineObjects`

**Ratify the census's naming, stated precisely.** The owner is the terminal
`Entity` itself: `TerminalInlineObjects` re-derives as a component *on* the
terminal entity (the census spine's Seam group — it is constructed alongside
the runtime and dies with it), so ownership is residence, not a stored key —
no id field to go stale, generational despawn safety for free. Every scene
object the registry spawns — kitty planes, RGP roots, and their descendants
— carries the census's new relationship component `TerminalOwner(Entity)`
pointing back at the terminal entity; that tag is what `sync_image`-side
organ filtering and all five `.single()`-plane projectors route on (census
decomposition; the `.single()` sites at `src/systems.rs:731`, `:1319`,
`:2238`, `:2791`, `src/presence.rs:962`). Do not improve on the name:
`TerminalOwner` matches the `ChildOf`-walk pattern
`apply_instance_brightness` already uses to find a root
(`src/systems.rs:1614-1636` per census), and one relationship component
serving inline, viz, presence, and cursor satellites uniformly is the point.
The bonus stated in the preamble stands: because the stage queue lives inside
this component (`src/inline.rs:85`), per-entity inline registries give every
RGP stage write an unforgeable owner before #50's stamp work even lands.

## 3. The stage cluster: surface shape is the terminal's, the view is the scene's

The mechanical split first, then the contested question, then who writes.

**The field-level split (the census's prerequisite, discharged).**

- `StageTween` splits into a per-entity warp tween (the `warp` channel,
  `src/scene/stage.rs:39`) living beside the owner's `TerminalPlaneWarp`,
  and one scene camera tween (`yaw`/`pitch`/`zoom`, `:41-45`). Today one `c`
  from runtime B replaces runtime A's whole tween mid-flight (replace-on-write,
  `:25-27`) — after the split, warp tweens cannot cross-cancel between
  runtimes; only the shared camera tween remains contended.
- `MobiusTransition` splits into a per-entity **morph** half —
  `active`/`elapsed_secs`/`direction` and the morph-progress math
  (`src/scene/mobius.rs:10-15`, `:109-127`) that feeds each plane's mesh
  rewrite (`animate_terminal_plane_warp` → `apply_plane_warp`,
  `src/systems.rs:2424-2466`) — and a scene **camera** half: the
  save/restore fields and camera lerps (`:18-41`, `:130-175`) plus
  `source_mode` (`:16-17`), consumed by `animate_mobius_transition`'s
  end-of-transition camera restore (`src/systems.rs:2489-2495`).

**Per-plane morph vs whole-scene cut — weighed.**

*Whole-scene cut* (mode stays one scene fact; every plane morphs together;
the transition owns the one camera): preserves today's choreography with no
re-interpretation, and needs no split. But it makes a per-object geometric
property global — under placement-not-splits, terminals are objects in one
space, and "this object is a Möbius strip" is a property of the object, the
same axis as `TerminalPlaneWarp`, which the census already classified
per-entity ("per-entity geometric property", warp row). It also hands any
granted runtime the power to re-frame every other agent's terminal on each
mode write — N agents' cuts fight last-writer-wins across the whole scene —
and it contradicts the embodiment table, where surface behavior is per-agent
state (`docs/ecosystem-vision.md:83-96`).

*Per-plane morph* (recommended): today's one `TerminalPresentation` fuses two
facts, and they separate cleanly against the code:

1. **Surface shape** — per-terminal: flat quad ↔ warped plane ↔ Möbius
   strip, the geometry of that terminal's own mesh pair.
   `TerminalPresentationMode` (`src/scene/mod.rs:76-84`) becomes the
   *value* of a per-entity shape component (resolving the census erratum:
   its vestigial `Resource` derive was never inserted); the warp scalar,
   the warp tween, and the Möbius morph half live beside it. The mesh
   rewrite is already per-mesh-pair (`src/systems.rs:2450-2465`); presence
   markers and RGP projection already parameterize on (mode, warp, morph
   progress) and simply read the owner's
   (`active_mobius_progress`, `src/systems.rs:2641-2654`;
   `marker_pose` args, `src/presence.rs:987-997`).
2. **Scene view** — screen-global: which camera stack presents (the
   flat-2D camera is active only in flat mode while the 3D camera owns the
   clear otherwise, `src/scene/mod.rs:568-585`), the focused-1:1
   presentation (the fullscreen sprite quad vs the placed planes,
   `:527-554`), the camera pose (`TerminalPlaneView`,
   `src/scene/mod.rs:161-179`), the camera tween, and the Möbius camera
   half.

  Today's tri-state maps exactly: `Flat2d` = scene view "focused-1:1 on the
  sole terminal"; `Plane3d`/`Mobius3d` = scene view "free-3D" + the sole
  terminal's shape = plane/strip. A wire `mode` command sets both halves for
  the arrival runtime — byte-identical semantics at N=1. Under N, a
  background terminal entering Möbius morphs its own mesh in place; the
  camera choreography (`begin_enter`'s zoom-out, exit restore) engages only
  when the shaped terminal is the focused one, because its subject is the
  camera, and the camera belongs to the view. The mode-cut-cancels-tween
  rule ("a mode change is a scene cut: it cancels any camera tween",
  `src/systems.rs:2529-2530`) stays on the view half.

**Who may write, per state** (Model A assumed: writer identity =
`Local(ordinal)`, RGP writes owner-attributed by construction):

| State | Home | Writers | Gate | Contention rule |
| --- | --- | --- | --- | --- |
| Surface shape + `TerminalPlaneWarp` + warp tween + morph half | Component(s) on the terminal entity | The owning runtime (its RGP `c` / `warp` / `mode`-shape-half, ungated — its own body); the user on the focused terminal (keyboard toggles, JS per #53's single-terminal contract) | None | Writes within one stream are sequential; no cross-runtime contention exists by construction |
| Scene view (focused-1:1 vs free-3D, camera-stack switch) | Screen-global resource | The user (keyboard/mouse/JS); wire writers only via the new gate (§4) | `SceneCapability::SceneStage` | User first, always; granted wires last-writer-wins inside the locked frame order rgp → ai → web (`src/web.rs:376-388`); a view cut cancels the camera tween |
| `TerminalPlaneView` + camera tween + Möbius camera half | Screen-global | Same as scene view | `SceneCapability::SceneStage` | User input stops wire tweens (`src/web.rs:428-433`, `src/keyboard.rs:381-392`); granted wires replace-on-write, each write attributable to its ordinal |

One margin change stated honestly: today an explicit `warp` stops a running
*camera* tween (`src/ai.rs:223-229`). After the channel split, a warp write
cancels only its own plane's warp tween. At N=1 an agent scripting
`c;dur=…;yaw=…` then `warp` sees the yaw tween complete instead of freeze —
a deliberate, documented delta; #56 may pin the old coupling for the arrival
runtime if strict compat is preferred, but channel-scoped cancellation is
the clean rule.

## 4. The capability story: gate the view, not the body

**Recommendation: the scene-view class gains a third `SceneCapability`
variant — `SceneStage` — on the exact `require_scene` pattern; the
surface-shape class leaves the scene-global classification entirely and
needs no gate.**

- **What gates.** The scene-view halves: `mode`'s view cut, the RGP `c`
  camera channels (yaw/pitch/zoom, immediate or tweened), `reset`'s view
  half, `bookmark.jump`'s view half (checked against the jump's carried
  source at relower — `PendingBookmarkJumps` entries already carry
  `IngressSource`, census), and the Möbius camera choreography. Gate site:
  the appliers, beside the ownership checks, exactly like
  `require_scene!` (`src/avatar/mod.rs:751-763`) and the ambient gate
  (`src/sound.rs:593-595`). Denial acks `not-permitted` — the shipped
  avatar posture.
- **What does not gate.** Warp (now per-plane, always); `mode`'s
  surface-shape half; `reset`'s runtime-scoped halves (its object / viz /
  effects / bookmark / macro-slot / presence taps clear only the arrival
  runtime's components — the addressing doc's `reset` split, ratified
  here); the effects family (per-runtime, §1). The avatar and ambient
  gates stay exactly as shipped.
- **M3 compat — the ticket's hard constraint.** The grant bit defaults to
  **granted** for the local class, on the shipped precedent
  `avatar_scene_defaults_to_granted` — "a single-tenant session directs
  its own scene" (`src/capability.rs:86-91`). At N=1 with default config,
  every byte behaves identically: same commands, same acks, same visuals
  (the first runtime is `Local(0)`, addressing doc). Behavior changes only
  when trusted config explicitly revokes — and then an agent can still
  shape its own surface and feel its own mood while losing only the whole-
  scene camera, mirroring "ordinary agents may speak and gesture only"
  (`src/avatar/mod.rs:757-759`). This intentionally diverges from the #50
  doc's default-DENY recommendation for the *#49 lifecycle* verbs
  (close/spawn by handle): closing another runtime is cross-runtime reach
  and defaults deny; directing the shared camera is single-tenant
  self-direction and defaults grant. Both knobs are #56's.
- **Grant coarseness, restated.** Grants stay per transport class with the
  ordinal as data (`src/capability.rs:13-16`, `:49-58`) — "A may cut the
  scene, B may not" is inexpressible under wire-immutable load-time
  config, the same coarseness the addressing doc flagged for its lifecycle
  verbs. Not a new mechanism; a #56 knob, already on its table.
- **Classification consequence.** With warp and the shape half
  re-scoped runtime-local, `is_scene_global` membership shrinks
  (`src/osc.rs:768-781` — `SetWarp` drops out, `SetMode`/`Reset` keep
  their view halves): a warp-only recording stops classifying
  *privileged* and no longer needs the exclusive scene lock to play.
  Previously finalized macros keep their stored classification —
  stale-conservative (over-locked, never under-locked). `is_scene_global`
  is internal classification, not wire bytes, so this is lawful.

**The four scalars, closed:**

1. **`MacroRegistry::scene_lock`** — stays scene-wide and singular: it
   exists to serialize privileged playback *over shared scene state*, so
   its scope is the shared scene by definition. Its holder key is already a
   namespace u8 (`src/macros.rs:310-314`) — under Model A that names the
   holding runtime with no change. Session macros/slots go per-entity per
   the census split (`reset` clears them, `src/macros.rs:866-870`); the
   playback injection budget stays one shared per-frame bound (it paces the
   app, a screen fact) with the iteration start rotated across active slots
   so one runtime's playback cannot starve the rest — the avatar
   fair-rotation spirit, one line of mechanics.
2. **`AiEffects` wash** — decided in §1: per-runtime state, one
   compositor, focused-wash default with the render-policy knob recorded
   for #56.
3. **`SoundState.ambient` (`AmbientSlot`)** — stays the single scene-owned
   slot; among `SceneAmbient` grantees, **last-writer-wins is ratified as
   the arbitration**, not just the accident: a bed is stateful, and the
   slot already retains the LATEST pre-unlock request as its doctrine
   (`src/sound.rs:227-232`). Under Model A every write is
   ordinal-attributed, so `state.scene` can name whose bed plays; the
   per-namespace one-shot buckets separate per-principal for free
   (`src/sound.rs:280-282`), as the addressing doc predicted.
4. **`SceneCapability::granted_to`** — gains the `SceneStage` variant
   (three capabilities, one spine). Note for #56: the *designed
   compile-break* fires on new `IngressSource` transport classes, not on
   new capability variants; under Model A, `Local` gaining a `RuntimeId`
   field keeps the match total with the grant table still keyed per class
   — no per-ordinal grant rows exist or are wanted (`src/capability.rs:22-26`).

## 5. How the season's protocols attach per-runtime

Each runtime is one embodied entity: the terminal entity carries its stream
(`TerminalRuntime`), its face (`TerminalSurface` + shape/warp), its mood
(`AiEffects`), its visitors (`PresenceRegistry`), its vitals
(`agent.<ns>.*` sensors), and its belongings (object/viz registries) — the
component list *is* the embodiment, and the ecosystem table's per-agent rows
(aura, pulsing border, dimming) land on those components
(`docs/ecosystem-vision.md:83-96`).

**#18 — the three tiers re-homed, projections scene-scoped.**

- **Tier 2 (own state in full)** attaches to the arrival runtime: the
  own-namespace ops (`state.objects` / `state.macros` / `state.executions`
  / `state.errors`, #18 resolution) answer from the arrival terminal's
  per-entity organ components — `OrganRegistries`' seven `Res<>` singletons
  (`src/query_channel.rs:306-320`) become per-entity Query joins keyed by
  the request's stamp, and replies keep following ingress, never broadcast
  (`send_reply`'s designed single-arm match, `src/query_channel.rs:463-467`;
  routing doc `:325-328`). Under Model A, "own namespace" and "arrival
  runtime" are the same axis. `AiDiagnostics`' bare-u8 rings
  (`src/query_channel.rs:175-182`) separate per ordinal for free.
- **Tiers 1 and 3 (scene public + projections) attach to the scene, not
  the runtime.** This is the load-bearing call: `state.visible_objects` /
  `state.neighbors` join across *all* terminal entities' registries,
  public fields only. Three locks force it: #18's tier-3 text is "the
  minimal structured facts of what is visibly on screen" — the screen is
  shared; #22's ruling reads "#18 projections/neighbors describe entities
  in a shared scene"; and the embodiment use cases (orbit/avoid/approach,
  attention on another agent) are cross-agent by definition
  (`docs/ecosystem-vision.md:92-95`). #18's boundary — "visibility grants
  observation, not control" — is exactly what makes cross-runtime
  observation safe with zero new enforcement: write ownership stays at the
  lowering layer. No fourth read tier is minted, and this does not
  contradict the addressing doc's "another runtime's state is not
  wire-addressable": there is still no runtime-*targeted* query op —
  the shared scene is simply what tier 1/3 always described. Owner
  attribution needs no new field: every projection row already carries its
  owner namespace (#18), which under A names the owning runtime.
  Additive: rows gain their owning terminal's scene placement so
  `state.neighbors`' radius/region reads in shared scene space, each row
  keeping its grid anchor relative to its own terminal; `state.scene`
  keeps the surviving screen-global cluster (scene view, avatar public
  state, ambient bed) while per-terminal public state — including
  `AiEffectsPublic`, currently a `state.scene` fact
  (`src/effects.rs:130-133`) — moves to the per-terminal projection.
  `state.namespaces` is scene-scoped (counts across all runtimes); the
  #50-recommended `state.runtimes` op is the embodiment index — one
  handle per visible agent terminal.

**#21 — sensors travel with the body; `sys.*` is the host's.**

- The census's `ReactiveRegistry` split is ratified: `agent.<ns>.*`
  sensors, session rules, and publish buckets (`src/reactive.rs:279-289`)
  go per-entity; `sys.*` rows, trusted rules, and the adapter grant
  (`:290-292`) stay the one global half. Under Model A each runtime's
  sensors are namespace-disjoint by construction — the u8 merge dissolves.
- A runtime's rules may bind its own `agent.<ns>.*` sensors plus the
  shared `sys.*` rows — **cross-runtime sensor references are not
  minted** (possession is reach; a coordinator that wants A reacting to
  B's telemetry feeds A's channel, or publishes through a trusted
  collector into shared rows — #21's own adapter tier, and the ecosystem
  path where "event-stream sources replace local sensors"). Unbound-rule
  semantics are unchanged per-runtime.
- **Trusted config rules fan out to every terminal.** Their inputs are
  host facts (`sys.*`) and their allowlisted actions are choreography; a
  host-level alarm (battery low) that tinted only one of N embodied
  terminals would silently drop the alarm on the rest. Session rules act
  on their own runtime; rule-fired actions inherit the stored runtime-
  qualified source exactly as macros do (census `evaluate_rules` row). If
  #56 wants scoped trusted rules instead, a per-rule scope key in config
  is additive (config, not wire) — recorded as the escape.

**#25 — presence rides the carrying stream; transport stays external per
runtime.**

- Per-entity registry/applier/markers/expiry (§1). #19's
  presence-not-transport finding generalizes verbatim: "anything that
  writes bytes can drive them" becomes *anything that writes bytes into a
  given runtime's channel drives that terminal's presence* — ratty still
  performs no networking; a tier-B relay tees one runtime's session; a
  future write-capable transport is a new `IngressSource` variant whose
  presence lands on the terminal it is constructed against (the fog
  entries already on map #42).
- The wire-origin-only guard survives per runtime unchanged
  (`src/presence.rs:1145-1152`) — replay cannot forge liveness on any
  terminal. Leases stay on the one `Res<Time>` (one clock per app —
  browser-story constraint: one `Time` per widget, not per pane): a hidden
  wasm tab stalls every terminal's leases together, coherently.
- Reads follow the tier rule above: own roster in full including expired
  rows from the arrival runtime; foreign rosters fresh-only across the
  scene (rendered = public, `src/presence.rs:52-60`) — an expired foreign
  row's existence still must not leak, now across N terminals.

## 6. What returns to map #42 — and what does not

Two genuine returns, each with its forcing function; everything else the
slice could classify, it did.

1. **Inter-agent relational choreography.** The ecosystem table's
   *relational* rows — attention beam from A to B, coordination lean toward
   a peer, synchronized flash between agents
   (`docs/ecosystem-vision.md:92-95`) — have **no author** under this
   ticket's scheme: a relation between two embodied terminals is not
   either terminal's own body, and under arrival-is-the-address neither
   endpoint's channel can write the other. It needs either a
   `SceneStage`-class conductor or a first-class relational primitive —
   neither is phraseable before real agents inhabit ≥2 terminals. This
   *extends the existing* "Terminal-as-agent embodiment vocabulary beyond
   the seed" entry rather than adding a new one, sharpened with the
   authorship question; forcing function unchanged (real agents in real
   terminals — concretely, the first inter-agent choreography demand).
2. **Scene composition during focused-1:1.** §3 generalizes `Flat2d` to
   "the focused terminal presents 1:1" but deliberately does not specify
   what the non-focused terminals show (hidden as today's sprite/plane
   toggle implies, docked, miniaturized) or how the free-3D view frames N
   placed planes by default. That is scene-layout design with no code to
   argue against until two planes exist. Forcing function: the first
   execution milestone that renders two terminal entities — the successor
   of #54, whose charter is explicitly no-layout-polish.

**Explicitly not returned** (each already has a home): the focus
authority's shape and cardinality — #51's first question; which terminal
the JS page controls and `set_warp`/`set_mode` target — #53/#57 (the
census's `WebControlQueue` note: JS setters gain a target id only if warp
goes per-plane, which §3 answers *yes*, so the #53 lock inherits that
consequence); per-pair grant granularity, the wash render-policy knob, the
warp/camera-tween coupling margin, and the trusted-rule scope escape — #56
knobs recorded in §§1, 3, 4, 5; multi-runtime silk framing — already on
the map's list; write-capable external transports and their namespaces —
already on the map's list.

## What #56 ratifies (the lockable decisions)

1. Census first bullet: presence, viz, inline per-runtime; avatar
   screen-global — ratified (§1). `TerminalOwner(Entity)` naming —
   ratified; `TerminalInlineObjects` owned by residence on the terminal
   entity (§2).
2. Effects: per-runtime `AiEffects` state, one order-10 compositor;
   focused-wash render default (knob: strict per-surface) (§1).
3. The field-level splits: `StageTween` warp channel per-entity;
   `MobiusTransition` morph half per-entity, camera half scene-side (§3).
4. Presentation splits into per-terminal surface shape (with warp/morph,
   owner-writable ungated) and screen-global scene view (focused-1:1 +
   camera stack + camera pose) (§3) — the census's open question 4
   answered: partially per-terminal.
5. Camera arbitration: user first always (tween-stop precedent extended);
   granted wires last-writer-wins inside the locked writer order; a view
   cut cancels the camera tween (§3).
6. `SceneCapability::SceneStage` gates the scene-view class (mode's view
   half, `c` camera channels, `reset`/`bookmark.jump` view halves);
   default granted for the local class (avatar precedent) so N=1 default
   behavior is byte-identical; `reset`'s runtime halves stay ungated (§4).
7. Scalars: `scene_lock` scene-wide (ordinal-keyed); ambient
   last-writer-wins among grantees; playback budget shared with rotated
   fairness (§4).
8. Protocol attachment: tier 2 = arrival runtime; tiers 1/3 scene-scoped
   (`state.neighbors` in scene space; `AiEffectsPublic` to the per-terminal
   projection); #21 census split + trusted-rule fan-out + no cross-runtime
   sensor references; #25 per-carrying-stream with wire-origin and lease
   clocks unchanged (§5).
9. The two returns to map #42, with their forcing functions (§6).
