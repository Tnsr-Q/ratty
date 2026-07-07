# RGP implementation truths

Facts about how ratty *actually* behaves that are not written in
`protocols/graphics.md`. Sourced from the implementation; every claim has a
file reference. Read this before authoring transmissions or extending the
protocol. The knowledge graph (`graphify-out/`) answers "where is X?";
this document answers "what will X really do?".

## Anchoring and placement

- **`row`/`col` in `p` are the CENTER of the placement**, not the top-left.
  The terminal converts center → top-left as `row - ceil((h-1)/2)`,
  `col - ceil((w-1)/2)` (`src/inline.rs:354-359`). The `ratatui-ratty`
  widget's `place_sequence(area)` does the inverse conversion from a
  top-left `Rect` (`widget/src/lib.rs:302-303`).
- Rows and columns are 0-based terminal cells.
- **Placing an unregistered id is silently ignored** (`src/inline.rs:353`).
  Register (final chunk included) before you place.
- RGP objects always scroll with the text. There is no overlay /
  non-scrolling mode (contrast Kitty Unicode-placeholder images).

## Updates: live vs respawn

- `u` fields `px/py/pz`, `rx/ry/rz`, `sx/sy/sz`, `scale`, `animate` — and
  the v2 animation fields `spin/bob/bobamp/phase` — are read live every
  frame by `sync_rgp_objects` — **zero-cost, smooth, ideal for per-frame
  streaming** (`src/systems.rs:1130-1200`, `src/inline.rs:375-381`).
- `u` fields **`depth`, `color`, `brightness` set a dirty flag that despawns
  and respawns the whole object** (`src/inline.rs:375-381`). Never stream
  them per-frame; set them at placement or in a one-off update.
- Because of that, the widget's `update_sequence()` — which always emits
  `depth`, `color`, and `brightness` (`widget/src/lib.rs:332-354`) — forces a
  respawn on *every* call. The `silk` compiler emits minimal `u` sequences
  (only the tweened fields) instead.
- `color` via `u` is set-only: you cannot clear a color back to "unset".
  The same applies to `spin`, `bob`, and `bobamp` — once set, an object
  cannot return to "use the configured global rate"
  (`src/inline.rs:685-731`).

## Registration and payloads

- Payload chunks are 3072 base64 chars in the widget
  (`widget/src/lib.rs:9`) — divisible by 4, so every chunk decodes
  standalone; ratty decodes each chunk independently and concatenates bytes
  (`src/rgp.rs:181-183`, `src/inline.rs:401-465`).
- A mid-stream `fmt` change for the same id drops the chunk with a warning
  (`src/inline.rs:419-425`).
- `normalize` affects **OBJ only** — centers on the bounding-box center and
  scales by the largest axis (`src/model.rs:516-525`). STL/GLB ignore it.
- **OBJ vertex colors are honored** (`v x y z r g b`,
  `src/model.rs:534-543`) — paint per-vertex color into the OBJ itself; no
  materials needed. This is how the Rubik's cube demo colors stickers.
- `fmt=gltf` works in the loader as a glb alias even though the capability
  string only advertises `obj|glb|stl` (`src/model.rs:215,262,284`).
- `tint` is an undocumented alias for `color` (`src/rgp.rs:147`).
- GLB payloads are **materialized to disk** under ratty's cache dir because
  Bevy's scene loader wants a path (`src/model.rs:331-375`). OBJ and STL
  parse fully in memory.

## Animation

- The built-in animation is spin (Y) + tilt (X, always 0.7× the spin angle)
  + bob, gated by `animate=1` (`src/systems.rs:1198-1243`,
  `rgp_object_animation`).
- **v2 makes the rates per-object**: `spin=`/`bob=` (rad/s),
  `bobamp=` (fraction of cell height), `phase=` (radians, offsets both
  channels). Absent fields fall back to the global config
  `[cursor.animation] spin_speed=1.4, bob_speed=2.2, bob_amplitude=0.08`
  (`src/config.rs:477-497`) — and an object using no v2 fields moves
  **bit-identically to v1** (locked by test).
- Continuity mechanics: each object entity carries an integrated
  `RgpAnimationState` accumulator (`src/inline.rs:22-37`). Objects on the
  v1 path keep the absolute-time expressions while the accumulator tracks
  them in lockstep, so the first `u;spin=` takes over without a snap, and
  later rate changes integrate smoothly. `spin=0` therefore HOLDS the
  current angle (pose-hold), and a fresh v2 placement starts
  deterministically at `phase` instead of at wall-clock position.
- **A respawn (depth/color/brightness update, or re-place) resets the
  accumulator to zero** — expect the pose to jump along with the respawn.
- The 3D cursor still animates from the global config only
  (`src/systems.rs:1601+`); RGP fields never affect it.
- `phase` is the tool for breaking lockstep between objects; different
  `spin` rates drift them apart over time.
- Any richer motion = the application streams timed `u` updates
  (the `rubiks_cube.rs` / `draw.rs` pattern; `silk`'s `tween` mechanizes it,
  default 30 fps — and in v2 the tween can target the animation rates too).
- `depth > 0` triggers an automatic "oblique" 3/4-view tilt
  (`RotY(0.75)*RotX(0.35)`) on top of your explicit rotation
  (`src/systems.rs:1140-1145`), extrudes flat (z≈0) meshes into solids with
  thickness `depth * 0.03`, and pushes the object toward the camera.

## Deletion

- `d` with no id deletes **all** inline objects — including Kitty images
  from other applications (`src/inline.rs:201-206`). Use with intent.
- There is no placement-only delete; `d;id=N` removes the whole object and
  its pending chunks.

## The wire and the return channel

- Terminator: `ESC \` or the single C1 byte `0x9c` (`src/rgp.rs:89-95`).
- The **only** renderer→application channel is the `s` support-query reply,
  written back to the PTY input (`src/systems.rs:177-181`). No events, no
  clicks, no acks. Current capability string: `v=2;fmt=obj|glb|stl;path=1;`
  `payload=1;chunk=1;anim=1;depth=1;color=1;brightness=1;transform=1;`
  `update=1;normalize=1;stage=1;tween=1;objanim=1` (`src/rgp.rs:412-419`).
  New capability keys are appended, never inserted, so key-scanning parsers
  survive version bumps.
- Unknown verbs and unknown keys are silently ignored — the protocol is
  forward-compatible by design, which is exactly how v2 casts degrade
  gracefully on v1 terminals (staging and per-object rates vanish; the
  cast still plays).
- A malformed RGP sequence is NOT consumed as RGP: it falls through to the
  Kitty parser and then to vt100, i.e. garbage can leak into the terminal
  (`src/inline.rs:213-219`).

## Stage (reachable via the v2 `c` verb)

- Presentation mode (`Flat2d`/`Plane3d`/`Mobius3d`), warp amount, and camera
  yaw/pitch/zoom are now **in-band**: the `c` verb queues a stage update on
  the inline-object state (`src/inline.rs:395-398`) that `apply_rgp_stage`
  drains into the same resources the keyboard and mouse mutate
  (`src/systems.rs:1628+`). Camera pan offset (`ox`/`oy`) is deliberately
  NOT in v2 — it is viewport-pixel-dependent, so casts carrying it would
  not be portable.
- Silk still carries opening values in the `x_ratty` header; prefer an
  `at: 0.0` camera step for self-contained v2 casts. Both write the same
  resources — chronological last-writer-wins.
- The tween rules, in priority order (implementation:
  `src/systems.rs:1628-1760`, `src/scene/stage.rs`):
  - `mode` changes are always instant-dispatch and cancel a running stage
    tween; Möbius enter/exit animates on its own fixed clock
    (0.2s + 0.9s phases, `src/scene/mobius.rs:54-61`), and `dur` never
    applies to `mode`.
  - a second `c` replaces the tween wholesale, retargeting re-specified
    fields from their current interpolated values;
  - **user input wins**: mouse rotate/pan/zoom, the warp/mode keys, and
    the web `set_*` API all cancel a running stage tween;
  - while a Möbius transition is animating, `yaw/pitch/zoom` in a `c` are
    dropped (not queued) — `warp` still applies;
  - camera fields sent in `flat2d` are stored and take effect on the next
    3D entry.
- Leaving Möbius via `c;mode=` is absolute: the exit lands on the requested
  mode, not the mode Möbius was entered from
  (`src/scene/mod.rs:127-158`). The keyboard toggle keeps its
  return-to-source semantics.
- Easing: `linear`, `in`, `out`, `inout` (default `inout` — the same
  smoothstep the Möbius transition uses). Eased progress is exactly 1.0 at
  the end, so tweens land on exact target values.
- Warp is a time-pulsed radial "gravity well" behind the terminal plane;
  Möbius mode is a parametric ribbon whose radius/width/twist also scale
  with the warp amount.
- Terminal planes are unlit; RGP objects use fixed PBR material values
  (roughness 0.88, reflectance 0.18, metallic 0) with `color` as base color
  and `brightness` as a one-time material multiplier.

## Grid and naming trivia

- The terminal grid is clamped to at least 2×2 (vt100 underflow guards,
  `src/terminal.rs:178-187`).
- Default grid is 104×32 (`src/config.rs:153-172`) — Silk's default stage.
- The widget crate is named `ratatui-ratty` (`widget/Cargo.toml`), though
  the root README refers to it as `ratatui-rgp` — an upstream docs
  inconsistency, found during corpus extraction.
