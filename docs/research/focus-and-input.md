# Focus and input routing for N runtimes

Research asset for [wayfinder ticket #51](https://github.com/Tnsr-Q/ratty/issues/51)
(map [#42](https://github.com/Tnsr-Q/ratty/issues/42)). **Recommendation only —
the lock happens at the spine grilling
([#56](https://github.com/Tnsr-Q/ratty/issues/56)).** The per-runtime census
(`docs/research/per-runtime-spine.md`) hands this ticket the two routing
concepts its input sweep found unowned — keyboard focus and mouse capture —
plus the picking-seam blocker (`per-runtime-spine.md:345-357`), and
deliberately left the focus authority's *shape* undecided: `FocusedTerminal`
is "a named placeholder, not a decided resource"
(`per-runtime-spine.md:277-281`). This document decides the recommendation's
shape; #56 ratifies or rejects it.

Precedent locked upstream — built on here, never reopened:

- **N instances of the seam; placement, not splits** (#22). Terminals are
  scene objects; the routing tuple picking must produce is (entity, cell)
  (`per-runtime-spine.md:215-218`).
- **Arrival is the address** (the #50 recommendation,
  `docs/research/addressing-and-trust.md:258`): no content-plane command
  names a runtime; reach is channel possession. Corollary consumed here:
  focus is an *internal routing concern* — it decides which runtime the
  user's keystrokes reach, never which runtime a wire command reaches.
  Wire bytes arriving on runtime B's transport land on B whether or not B
  is focused; a transmission plays into an unfocused terminal by design.
- **User input wins.** The locked writer order runs JS controls last among
  stage writers because "JS controls are user input: they win"
  (`src/web.rs:376-381`, `:428-433`). Focus arbitration extends this
  precedent, not a new one.
- **Kitty keyboard modes are per-parser already.** The enhancement flags and
  `modifyOtherKeys` level live as fields on `TerminalParserCallbacks`
  (`src/runtime.rs:71-72`), are mutated by CSI handlers inside the parser
  (`CSI > flags u` at `src/runtime.rs:153-158`, pop at `:160-164`,
  `modifyOtherKeys` at `:166-180`, the `CSI ? u` query reply at `:145-151`),
  survive resize because the whole callbacks value moves via
  `std::mem::take` (`src/runtime.rs:73-76`), and surface through
  per-runtime accessors (`src/runtime.rs:106-113`, delegated at
  `:610-616`). `application_cursor` is vt100 `Screen` state, per-parser by
  construction (read at `src/keyboard.rs:550`). Nothing about keyboard
  *translation* is singleton-shaped; only the routing is.

## The question, precisely — and who actually touches focus

Today both input systems write unconditionally to the one runtime:
`handle_keyboard_input` resolves every keystroke against `ResMut<'w,
TerminalRuntime>` (`src/keyboard.rs:338`, writes at `:439`, `:491`, `:522-527`,
`:559`) and `handle_mouse_input` against the same (`src/mouse.rs:240`, writes
at `:342-348`, `:378`, `:404`, `:426-466`, `:507-512`). Under N terminal
entities, something must answer "which one" — per event, every frame.

The verified consumer/producer set, argued from the actual systems:

**Readers of focus (post-N):**

| System | What it needs | Access shape |
| --- | --- | --- |
| `handle_keyboard_input` (`src/keyboard.rs:350`) | THE focused entity, then that entity's (runtime, surface, selection, redraw, viewport, planes) bundle | entity → `Query::get_mut` |
| `render_terminal_widget` per-terminal loop (`src/systems.rs:410`; census `:68`) | is-this-terminal-focused, to gate blink repaints and cursor steadiness | per-entity boolean |
| `sync_asset_to_terminal_cursor` per-terminal (`src/systems.rs:2714`; census `:81`) | is-focused, to select animated vs becalmed cursor style | per-entity boolean |
| Focus-transition redraw (new) | *which two* entities changed roles, to repaint both | gained/lost events |
| #49's read side (`state.runtimes` marking the focused row) | the one focused entity, organ-side | global read |

**Non-readers, verified:** presence does not read focus —
`PresenceMarkerParams` (`src/presence.rs:909-925`) carries registry, surface,
viewport, presentation, warp, and the plane query, no focus input; presence
rows are ingress truth anchored to their carrying terminal (census `:110`)
and render whether or not that terminal is focused. `handle_window_resize`
does not read focus either: under N, each terminal's grid derives from its
own surface geometry (census `:67`), not from who is focused.

**Writers of focus:** mouse pick (focus-follows-click, this document),
the #49 focus verb (wire, gated), user-initiated spawn policy (#49), the
fallback when the focused terminal dies, and — on wasm — a possible JS
control (#53's option). Five writers, one invariant: they never touch the
authority directly; they emit requests into one drain (below).

## The first question: three shapes, priced

### Shape R — screen-global resource, `Option<Entity>`-shaped

**Design.** One app-level resource holding `Option<Entity>`; concretely
Bevy's own `bevy_input_focus::InputFocus` — `current_focus: Option<Entity>`
plus a recorded-changes layer (vendored `bevy_input_focus-0.19.0/src/lib.rs:103-105`)
that emits `FocusGained`/`FocusLost` as `EntityEvent`s on the entities that
changed roles (`bevy_input_focus-0.19.0/src/gained_and_lost.rs:36`, `:53`). The
feature flag exists in the pinned Bevy (`bevy-0.19.0` Cargo.toml:2680) and is
absent from ratty's curated list (ratty `Cargo.toml` bevy features) — one
additive flag. The crate's `InputDispatchPlugin` (observer-bubbled
`FocusedInput` events, `lib.rs:287`) is **not** adopted: ratty keeps its
`MessageReader<KeyboardInput>` loop (`src/keyboard.rs:351-355`); only the
resource and the gained/lost layer are used. A hand-rolled
`FocusedTerminal(Option<Entity>)` with the same two events is the
zero-new-features equivalent — the shape, not the crate, is the
recommendation.

**Strengths.** Cardinality ≤ 1 is enforced by the type — two focused
terminals is *unrepresentable*, in a codebase that prizes designed
compile-breaks over runtime discipline (`src/capability.rs` wildcard-free
matches). "No focus" is an explicit, distinct state (`None`), needed the
frame after the focused terminal dies and when zero terminals exist. The
census's own taxonomy supports it: focus is a screen-global arbitration
fact like `TerminalPresentation`, not a property of a terminal — the
census files the keyboard question as "only the dispatch target multiplies"
(`per-runtime-spine.md:136`), and storing a screen-global fact in
per-entity storage crosses the census's per-runtime/screen-global boundary
in the wrong direction. The transition-event problem (repaint *both*
terminals on a focus move) is solved by machinery that already ships.

**Weaknesses.** Per-terminal systems check `entity == focus.get()` — a
resource read plus compare per row instead of a query-side filter; at
N = dozens this is noise, but it is the less idiomatic join. A dangling
`Entity` can sit in the resource after a despawn until the drain sweeps it —
generational ids make the stale lookup *fail safely*
(`per-runtime-spine.md:222-227`), but "stale" and "none" are distinguishable
only by the sweep.

### Shape M — marker component on the terminal entity

**Design.** A `Focused` unit component; the keyboard writer becomes
`Query<(&mut TerminalRuntime, ...), With<Focused>>::single_mut()`; per-terminal
systems join `Has<Focused>`; transitions fall out of
`Added<Focused>`/`RemovedComponents<Focused>`.

**Strengths.** The join-native shape — per-terminal styling reads focus in
the query row with no resource fetch; despawn self-cleans (no dangling
focus, ever); `single_mut()` makes the keyboard port a near-mechanical
rewrite of today's `SystemParam` list (`src/keyboard.rs:329-347`) with the
four per-terminal members re-derived as one query row.

**Weaknesses.** At-most-one is discipline, not type: any second writer that
inserts without removing mints two focused terminals, and every `single()`
reader starts failing at a distance from the bug. The invariant would be
held by exactly the single-writer drain this design mandates anyway — but
under R the same drain guards an invariant the type *already* enforces,
which is strictly less to get wrong. And "which entity is focused" for
organ-side reads (#49's `state.runtimes`) costs a query where R costs a
resource read.

### Shape W — per-window authority

**Design.** Focus keyed by window: a component on `Window` entities or a
`window → terminal` map.

**Rejected.** There is one window everywhere in the codebase: every input
system filters or fetches `PrimaryWindow`
(`src/keyboard.rs:340`, `src/mouse.rs:239`, `src/systems.rs:270`), exit
latches on the primary window closing (`src/systems.rs:117-139`), and the
census classifies the whole window layer one-per-app (`per-runtime-spine.md:131`,
`:156-159`). Per-window focus is machinery for a cardinality that is fixed
at 1. The hook is latent, not lost: `KeyboardInput` already carries
`window: Entity` (vendored `bevy_input-0.19.0/src/keyboard.rs:139-140` —
ratty ignores it today, reading all events at `src/keyboard.rs:355`, while
mouse already filters at `src/mouse.rs:284`, `:359`); if multi-window ever
lands, R widens to a per-window map behind the same drain, mechanically.

### The recommendation: R, with a single-writer drain

**Shape R** — a screen-global `Option<Entity>` resource (preferably
`bevy_input_focus::InputFocus` for the ready-made gained/lost layer; a local
equivalent is acceptable) — **mutated by exactly one system**, a
`Messages<FocusRequest>` drain:

```
FocusRequest { target: Option<Entity>, origin: FocusOrigin }
FocusOrigin  { PointerPress, WireVerb { .. }, JsControl, SpawnPolicy, Fallback }
```

The drain is the arbitration point and the whole policy surface: it
validates the target is alive (sweeping the dangling-entity weakness at the
same choke point), applies user-class-beats-wire-class within a frame (the
`src/web.rs:428-433` precedent — `PointerPress`/`JsControl` are user input;
`WireVerb` is not), and writes the resource once. `FocusGained`/`FocusLost`
then fire on the two affected entities; observers request those terminals'
redraws (both repaint: cursor style changes on each side) and apply any
focus-loss policy (selection clearing, if #56 wants it — see below). M's
genuine advantages (join ergonomics, despawn self-clean) are real but both
subsumed: the boolean per-terminal read is one compare, and the drain
already owns lifecycle fallback. W is deferred with an explicit upgrade
path, not designed.

Cardinality: **at most one, zero legal.** Zero occurs at startup before any
policy focuses terminal #1, after the focused terminal dies (until
`Fallback` lands), and if all terminals close.

## Keyboard: the unconditional writer becomes focused-runtime routing

`handle_keyboard_input` (`src/keyboard.rs:350`) splits along a line the code
already draws. Per event:

**The chord table stays global; its actions fork.** `TerminalKeyBindings` is
app-level user config (census `per-runtime-spine.md:136`; built from
`AppConfig` at `src/keyboard.rs:77-242`). Its actions sort into:

- *Scene chords* — `Toggle3DMode`, `ToggleMobiusMode`,
  `IncreaseWarp`/`DecreaseWarp` (`src/keyboard.rs:380-412`, `:459-470`)
  write the #52 stage cluster (presentation, mobius, stage tween, warp).
  They are **not** focus-routed; they are user input into contended scene
  state, and #52 owns who else may write it. They keep working with zero
  terminals focused.
- *Terminal chords* — scroll (`:414-457`), copy (`:472-483`), paste
  (`:485-498`), font size (`:500-537`) — route to the focused entity.
  Every read they perform is already per-runtime-shaped: the scroll chord
  reads the mouse protocol mode and encoding from the parser
  (`src/keyboard.rs:431`, `:435`), scrollback from the screen (`:446-453`);
  copy reads the selection against the screen (`:474-475`); font chords
  resize that surface and that PTY (`:504-527`) and re-lay-out that
  terminal's planes (`:528-533` — per-entity under the census's
  `sync_terminal_layout` row, `per-runtime-spine.md:121`).

**The byte path re-targets three reads and one write.** Translation already
parameterizes on per-runtime modes — `handle_event_with_modes` takes
`application_cursor`, `kitty_keyboard_flags`, `modify_other_keys` as
arguments (`src/keyboard.rs:275-281`), and the call site reads all three
from the runtime (`:548-552`). The census's verdict is verified: "only the
routing layer is missing" (`per-runtime-spine.md:87`). Post-N the same
three reads and the `write_input` (`:559`) plus the scrollback snap
(`:554-558`) resolve through `focus → Query::get_mut(entity)`. Nothing
else moves:

- `TerminalKeyboard` (the `Local` holding physical modifier state,
  `src/keyboard.rs:256-262`) stays a `Local` — one physical keyboard
  (census `:137`). This is *correct across focus changes*: hold Ctrl,
  click terminal B, press C — the modifier state is physical, so Ctrl+C
  reaches B. And no per-runtime release state exists to migrate, because
  releases are never encoded — non-modifier release events return `None`
  (`src/keyboard.rs:306-308`); ratty does not implement kitty release
  reporting, so the per-parser flags never demand per-runtime key-state.
- `TerminalClipboard` stays `NonSend` (census `:138`) — one OS clipboard;
  paste bytes route to the focused runtime (`:491`). (Noted in passing,
  not #51's to fix: the bracketed-paste wrapper is applied unconditionally
  today, `src/keyboard.rs:488-490`, rather than consulting the target
  screen's bracketed-paste mode — pre-existing, orthogonal to N.)
- The selection-clear-on-typing rule (`src/keyboard.rs:541-546`) applies to
  the **focused** terminal's selection only: typing into A must not
  disturb B's selection.

**No focus, explicit behavior:** byte input drops (there is nowhere honest
to send it — every window system does the same); scene chords still apply;
terminal chords no-op. No fallback-to-terminal-#1 — that would resurrect
the singleton as a default.

## Mouse: the picking seam

### What exists, verified

`position_to_cell` (`src/mouse.rs:580-617`) is the only screen→cell chart,
and it is Flat2d-only by construction: centered-margin math against the
primary window (`margin = (window_size − viewport.size)/2`,
`src/mouse.rs:598`), THE viewport, THE surface's cols/rows (`:590-591`,
`:613-616`). The 3D modes never resolve cells at all: `forward_mouse`
requires `Flat2d` (`src/mouse.rs:280-281`), and every 3D branch is camera
gesture — rotate/pan on motion (`:293-319`), rotate on left press
(`:381-386`), pan on right (`:469-486`), zoom on wheel (`:536-546`).
Selection likewise only begins in the Flat2d else-branch (`:387-392`).
Under placement-not-splits, terminals live in 3D — so cell picking in 3D is
**new capability, not a port**, exactly as the census's blocker states
(`per-runtime-spine.md:351-355`).

The census points at the projection function, and verification pays off:
`plane_surface_point` (`src/systems.rs:2837-2860`) is the **forward** chart
— plane-local `[-0.5, 0.5]²` → surface point — and what it assumes settles
the inverse problem:

- **Flat2d**: identity in (x, y) (`src/systems.rs:2847`).
- **Plane3d**: a height field — (x, y) pass through unchanged, only z is
  displaced (`:2848-2852`), by a *time-animated* pulse
  (`0.96 + 0.04·sin(t·2.2)`, `:2826`).
- **Mobius3d**: a lerp toward a twisted strip (`:2853-2858`) whose twist is
  also time-animated (`:2869`).

An analytic inverse would be mode-switched and time-dependent. **Do not
invert it. Pick through UVs instead.** The rendered plane meshes are
rebuilt from this same function with vertex positions derived from the UV
channel (`x = uv[0] − 0.5, y = 0.5 − uv[1]`, `src/systems.rs:2678-2679`;
the rebuild *requires* `ATTRIBUTE_UV_0` and bails without it,
`:2667-2669`), the meshes are constructed with explicit UVs
(`src/scene/mod.rs:670`, spawned as the front/back pair at `:383-412`), and
`animate_terminal_plane_warp` rewrites the actual mesh assets every frame
in the 3D modes (`src/systems.rs:2424-2466`). Therefore the mesh on screen
*is* the current-frame surface, and its UV channel *is* the cell chart —
for every mode, including Möbius, with zero inverse math.

The machinery is first-class in the pinned Bevy: `MeshRayCast` (immediate-
mode raycasts, `bevy_picking-0.19.0/src/mesh_picking/ray_cast/mod.rs`) with
an entity filter predicate (`MeshRayCastSettings.filter`, `mod.rs:41-50`)
returns `RayMeshHit` carrying `uv: Option<Vec2>` interpolated from the
mesh's UV attribute (`ray_cast/intersections.rs:10-25`, uv at `:21-22`).
Two feature flags are additive on the existing pin (`bevy_picking`,
`bevy-0.19.0` Cargo.toml:2687; `mesh_picking`, `:2804`) — pure CPU math,
wasm-clean. One real wrinkle: backfaces are culled by default
(`Backfaces::Cull`, `mod.rs:94-103`), which would make the far half of the
one-sided Möbius strip unpickable — the per-entity `RayCastBackfaces`
component (`mod.rs:105-108`) is the shipped fix. Picking targets the front
plane (`TerminalPlane`, `src/scene/mod.rs:33`); the back-face pair
(`TerminalPlaneBack`, `:37`) belongs to the same owner either way.

### The seam, named

```
pick_cell(pointer: Vec2) -> Option<(Entity /* terminal */, UVec2 /* cell */)>
```

- **3D modes**: camera ray from the plane-view camera → `MeshRayCast`
  filtered to terminal-owned planes (the census's `TerminalOwner` tag,
  `per-runtime-spine.md:266-271`) → nearest hit → `hit.uv` →
  `cell = (⌊u·cols⌋, ⌊v·rows⌋)` against the **owner's** grid — each
  terminal's cols/rows are its own (census `:67`).
- **Flat2d** (whatever Flat2d means under N is #52's): `position_to_cell`
  survives as the 2D backend — its signature is already pure over
  `(position, window_size, viewport, terminal)` (`src/mouse.rs:580-585`),
  so it is N-ready the moment viewport/surface are per-entity; only the
  entity selector is new.

Every `write_input` site in `handle_mouse_input` flows through this seam,
exactly as the census demands (`per-runtime-spine.md:92`), and the mouse
protocol mode/encoding reads (`src/mouse.rs:276-277`) re-target from THE
parser to the *hit* terminal's parser — per-parser state, like the kitty
flags.

### Capture, hover, and the routing rules

- **Press capture.** `ForwardedMouseState` (`src/mouse.rs:33-38`) becomes
  keyed by the press-target entity (census `:90`): press stamps the
  capture; motion and release route to the captured terminal's writer and
  encoding regardless of what the pointer crosses; releasing the last
  button clears it. The dedupe cell (`last_cell`, `:322`) rides in the
  keyed state.
- **Drag off the surface.** While captured, a ray that misses the captured
  mesh must still produce a cell (xterm clamps drags below the window to
  the last row). Recommendation: intersect the captured plane's *infinite*
  plane and clamp UV to `[0, 1]` — never a miss during capture. On a
  mid-drag Möbius morph the flat-chart clamp is approximate; freezing at
  the last cell is the honest fallback. Implementation detail, flagged.
- **Scroll follows hover, not focus.** Wheel targets the hovered terminal
  (the macOS/Windows convention), without changing focus.
  `LocalScrollState`'s pixel remainder is denominated in the surface's
  char height (`src/mouse.rs:521-524`) and becomes per-hovered-entity
  state (census `:91`) so remainders never leak between terminals with
  different font metrics.
- **Focus-follows-click.** Any button press whose pick *hits* a terminal
  emits `FocusRequest { target: Some(hit), origin: PointerPress }` — and
  the event still delivers to that terminal (click-through: xterm, kitty,
  and tmux panes all forward the focusing click). A press that *misses*
  every terminal is a camera gesture (rotate/pan/zoom keep today's
  bindings, `src/mouse.rs:293-319`, `:469-486`, `:536-546`) and does
  **not** change focus — clicking the void neither unfocuses nor steals.
  Hit-vs-miss becomes the discriminator between content interaction and
  camera interaction; who else may write the camera during those gestures
  stays #52's (`StageTween` cluster).

## Cursor model: per-runtime, with focus as style

The census already concluded the pose side: `sync_asset_to_terminal_cursor`
writes ONE pose to ALL `CursorModel` entities from THE parser and THE
`.single()` plane (`src/systems.rs:2744-2749`, `:2791`) — "the clearest
one-runtime tell in the file" (`per-runtime-spine.md:81`) — and proposes
each cursor model posing from its own terminal's screen, grid, and plane.
#51's question is existence: per-runtime cursors, or focused-only?

**Per-runtime, and focus selects style, not existence.** Three arguments:

1. **The cursor is wire-mutable per-runtime state.** Each runtime's wire
   styles its own cursor (`CursorSettings` per-entity, census `:96`; the
   `cursor` command lowering at `src/ai.rs:574-628`), and a style change
   can force a full model respawn (`respawn_cursor_model`,
   `src/systems.rs:618-648` — which today despawns *every* root,
   `:635-637`, the very bug the census's owner-keying fixes, `:71`). A
   single focused-only model would restyle/respawn on every focus change
   and would wear runtime A's styling while hovering runtime B — a
   misattribution, the same class of confusion presence identity rules
   exist to prevent.
2. **Consistency with presence.** Remote participants' carets already
   render per-terminal, focused or not (`sync_presence_cursor_markers`,
   `src/presence.rs:934`); the local cursor vanishing from unfocused
   terminals would make the one cursor that is *most* trustworthy the only
   one that disappears.
3. **The cost is bounded and mostly already paid.** On warped/Möbius
   surfaces poses are per-frame *regardless of focus* because the surface
   animates — presence's markers state this exactly
   (`src/presence.rs:928-931`, "the warped surface animates, so poses are
   per-frame like RGP"). Focus-gating poses would not remove that cost;
   it would only desynchronize the cursor from its surface.

**Focused style:** full animation — spin and bob are settings-driven
(`src/systems.rs:2772-2773`) — and live blink participation.
**Unfocused style:** becalmed — zero the spin/bob inputs, keep the model
posed and visible per its own `CursorSettings`; and the texture-side block
cursor (drawn when the model is invisible, `src/systems.rs:454-457`) draws
*steady* instead of blinking.

**What unfocused redraw/blink does — the concrete spine answer:**
`render_terminal_widget` marks a frame dirty for `needs_redraw ||
blink_ticked || !loaded` (`src/systems.rs:431-434`), where the blink tick
is a shared 4 Hz clock (`BLINK_TICK_SECS`, `:385`; `blink_phase` Local,
`:402` — one clock for all N, census `:68`). Per-terminal, the gate
becomes:

```
frame_dirty(i) = needs_redraw(i) || (is_focused(i) && blink_ticked) || !surface_ready(i)
```

Output-driven redraw is untouched — a background `tail -f` keeps painting
its own texture (each runtime's pump dirties only its own redraw, census
`:60`), which is the entire point of N terminals in one space. Blink-driven
repaints go focused-only: an idle unfocused terminal repaints at 0 Hz
instead of 4 Hz, so idle-scene texture work is N-invariant (one terminal
blinks). Honest cost: SGR blink text on unfocused terminals freezes
mid-phase — acceptable, and the same trade kitty makes with
focus-dependent cursor blink. This inequality is #54's to measure, stated
here as the design intent.

## Selection: per-runtime component, screen-global pointer

`TerminalSelection` becomes a component on the terminal entity with the
smuggled window-pointer position split out (census
`per-runtime-spine.md:88`, `:272-275`): `cursor_position` lives on the
resource today (`src/mouse.rs:29`) and is fused into selection updates at
`:179-186` — post-split it is the screen-global `WindowPointer`, and the
drag threshold stays window-pixel math (`SELECTION_DRAG_THRESHOLD`,
`src/mouse.rs:19`, applied at `:141`).

The rest is already N-shaped, verified: the pending/drag state machine
(`begin_pending`/`update_from_cursor`, `src/mouse.rs:109-151`) is
self-contained per selection; `selected_text` takes the screen as an
argument (`src/mouse.rs:189`) — the caller passes the owning runtime's
screen; the selection underlay draws inside the owner's own texture
(`TerminalWidget` consumes it in `render_terminal_widget`,
`src/systems.rs:439-449`). Routing rules:

- A selection drag belongs to its press-capture terminal for its whole
  life — the capture entity from the picking seam, so crossing another
  plane mid-drag never splits a selection across grids.
- Copy reads the **focused** terminal's selection against its screen
  (`src/keyboard.rs:472-483`); one clipboard (census `:138`).
- Typing clears the focused terminal's selection only (`:541-546`).
- **Selections survive focus loss** (recommended default): a selection on
  an unfocused terminal is standing state, like its scrollback — tmux
  keeps per-pane selections the same way. The opposite policy
  (clear-on-`FocusLost`) is one observer on the event the authority
  already emits; a #56 knob, not an architecture question.

## The #49 focus verb, and #53's canvas

**The wire verb lowers to the same bus.** #49's focus verb resolves its
`<session-nonce-hex>-<seq>` handle to an entity (the addressing doc's #18
shape; dead handles ack `unknown-id`, never silently) and emits
`FocusRequest { origin: WireVerb }` into the one drain. It gets both #49
mechanisms from the addressing recommendation — scene-global
classification plus a per-family capability gate
(`docs/research/addressing-and-trust.md:523-535`) — and the drain adds the
third: same-frame user requests beat wire requests (the `src/web.rs:428-433`
precedent).

**#51 sharpens the gate's stakes for #56: the focus verb is an
input-integrity verb, not a convenience verb.** Under this design, focus
determines where the user's *future keystrokes* land (`src/keyboard.rs:559`
re-targeted). A granted wire focus-steal silently redirects typing —
mid-command, mid-password — into a runtime the writer controls; it is the
keystroke-capture primitive, arrived at politely. This independently
reinforces the addressing doc's recommendation that the #49 lifecycle
grant bit **defaults to DENY** (`addressing-and-trust.md:458-462`), and
adds: even where granted, a wire focus change should be *loud* (a redraw
is forced on both terminals by construction; #52 may want more — flagged).
Spawn policy inherits the same argument: wire `runtime.spawn` must not
auto-focus its child; user-initiated spawns (keybinding, CLI) should. The
defaults are #49's to set; the drain is where any of them become one-line
policies.

**#53: focus without a window manager is the same design.** The authority
is Bevy-world state, not OS-window state — nothing in it references a
window except the input events themselves. The same systems run on wasm
unconditionally (`handle_keyboard_input`/`handle_mouse_input` registered
without cfg, `src/plugin.rs:56-57`); one canvas is one `PrimaryWindow`, the
model the census already keeps (`per-runtime-spine.md:128`, `:131`). The
canvas-level question — whether the canvas has DOM focus and receives key
events at all — is the embedder's, outside the world. If the page UI wants
programmatic focus (site tabs selecting a terminal), that is a JS control
through the existing `WebControlQueue` (census `:130`) emitting
`FocusRequest { origin: JsControl }` — user-input class, per the shipped
"JS controls are user input" precedent (`src/web.rs:376-381`). And #53's
transport fork (N sessions vs per-terminal selectors) stays orthogonal by
construction: **transports route bytes, focus routes keystrokes** — a page
must be able to `feed()` runtime B while the user types into A, or
transmissions could never play into an unfocused terminal.

## Focus invariants (for #56)

1. **One authority, `Option<Entity>`-shaped, screen-global** (Shape R);
   at most one focused terminal, zero legal.
2. **One writer**: the `FocusRequest` drain is the only mutation site;
   every origin (pointer, wire verb, JS control, spawn policy, fallback)
   is a request, never a write.
3. **User beats wire** within a frame, extending `src/web.rs:428-433`.
4. **Focus is routing, never authority** — it never widens what a wire may
   do (Model A: reach is channel possession); it only aims the user's
   keyboard. Wire delivery ignores focus entirely.
5. **Focus transitions are loud**: `FocusGained`/`FocusLost` fire on both
   entities; both terminals repaint (cursor style flips on each).
6. **Translation modes travel with the parser** (`src/runtime.rs:71-76`) —
   the keyboard Local carries only physical modifier state, valid across
   focus changes; no focus change can misencode a key.
7. **Capture beats hover beats focus for the mouse**: press capture pins a
   drag to one terminal; wheel follows hover; neither moves focus except a
   hitting press.
8. **No implicit focus**: no fallback-to-terminal-#1 on `None`, no focus
   from wire spawn, no focus from a missing pick.

## What this document deliberately does NOT decide

- **Whether Flat2d survives N, and what it displays** — #52 owns
  presentation; the input consequences are stated per mode only, and the
  2D picking backend stays pure and ready either way.
- **Camera ownership during miss-gestures** and the whole stage cluster —
  #52 (`StageTween`, `TerminalPlaneView`).
- **The focus verb's wire grammar, ack shapes, and grant defaults** —
  #49/#50 own the vocabulary; #51 supplies the lowering target
  (`FocusRequest`) and the default-deny argument.
- **Spawn-auto-focus and close-time focus fallback policy** — #49;
  recommendation recorded (user spawns focus, wire spawns do not; on
  focused-terminal death the drain's `Fallback` applies most-recently-
  focused, with plain `None` as the acceptable minimal alternative).
- **Selection clear-on-focus-loss** — a one-observer #56 knob; survive is
  the recommended default.
- **The page-API shape for N transports** — #53's fork; only its
  orthogonality to focus is asserted here.
- **Per-terminal keybindings** — not reopened; the census's screen-global
  classification stands (`per-runtime-spine.md:136`).
- **Multi-window** — Shape W rejected for now with the upgrade path named
  (the latent `KeyboardInput.window` field).
- **Performance envelope** — #54 measures; this document hands it two
  claims to verify: idle-scene repaints are N-invariant under focused-only
  blink, and per-frame UV raycasts against N warped meshes are cheap
  enough at interactive N.
