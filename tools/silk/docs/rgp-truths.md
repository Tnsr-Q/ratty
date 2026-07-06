# RGP implementation truths

Facts about how ratty *actually* behaves that are not written in
`protocols/graphics.md`. Sourced from the implementation; every claim has a
file reference. Read this before authoring transmissions or extending the
protocol. The knowledge graph (`graphify-out/`) answers "where is X?";
this document answers "what will X really do?".

## Anchoring and placement

- **`row`/`col` in `p` are the CENTER of the placement**, not the top-left.
  The terminal converts center → top-left as `row - ceil((h-1)/2)`,
  `col - ceil((w-1)/2)` (`src/inline.rs:328-333`). The `ratatui-ratty`
  widget's `place_sequence(area)` does the inverse conversion from a
  top-left `Rect` (`widget/src/lib.rs:302-303`).
- Rows and columns are 0-based terminal cells.
- **Placing an unregistered id is silently ignored** (`src/inline.rs:327`).
  Register (final chunk included) before you place.
- RGP objects always scroll with the text. There is no overlay /
  non-scrolling mode (contrast Kitty Unicode-placeholder images).

## Updates: live vs respawn

- `u` fields `px/py/pz`, `rx/ry/rz`, `sx/sy/sz`, `scale`, `animate` are read
  live every frame by `sync_rgp_objects` — **zero-cost, smooth, ideal for
  per-frame streaming** (`src/systems.rs:1112-1141`).
- `u` fields **`depth`, `color`, `brightness` set a dirty flag that despawns
  and respawns the whole object** (`src/inline.rs:349-355`). Never stream
  them per-frame; set them at placement or in a one-off update.
- Because of that, the widget's `update_sequence()` — which always emits
  `depth`, `color`, and `brightness` (`widget/src/lib.rs:332-354`) — forces a
  respawn on *every* call. The `silk` compiler emits minimal `u` sequences
  (only the tweened fields) instead.
- `color` via `u` is set-only: you cannot clear a color back to "unset"
  (`src/inline.rs:661-663`).

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

- There is exactly ONE built-in animation: spin (Y) + tilt (X, 0.7× spin
  rate) + bob, all gated by `animate=1` (`src/systems.rs:1129-1141`).
- Its parameters are **global**, shared with the 3D cursor:
  `[cursor.animation] spin_speed=1.4, bob_speed=2.2, bob_amplitude=0.08`
  (`src/config.rs:455-475`). Two animated objects always move in lockstep
  (no per-object phase).
- Any richer motion = the application streams timed `u` updates
  (the `rubiks_cube.rs` / `draw.rs` pattern; `silk`'s `tween` mechanizes it,
  default 30 fps).
- `depth > 0` triggers an automatic "oblique" 3/4-view tilt
  (`RotY(0.75)*RotX(0.35)`) on top of your explicit rotation
  (`src/systems.rs:1118-1122`), extrudes flat (z≈0) meshes into solids with
  thickness `depth * 0.03` (`src/systems.rs:1284-1390`), and pushes the
  object toward the camera.

## Deletion

- `d` with no id deletes **all** inline objects — including Kitty images
  from other applications (`src/inline.rs:201-206`). Use with intent.
- There is no placement-only delete; `d;id=N` removes the whole object and
  its pending chunks.

## The wire and the return channel

- Terminator: `ESC \` or the single C1 byte `0x9c` (`src/rgp.rs:89-95`).
- The **only** renderer→application channel is the `s` support-query reply,
  written back to the PTY input (`src/systems.rs:177-180`). No events, no
  clicks, no acks. Current capability string: `v=1;fmt=obj|glb|stl;path=1;`
  `payload=1;chunk=1;anim=1;depth=1;color=1;brightness=1;transform=1;`
  `update=1;normalize=1` (`src/rgp.rs:282-284`).
- Unknown verbs and unknown keys are silently ignored
  (`src/rgp.rs:163,224`) — the protocol is forward-compatible by design.
- A malformed RGP sequence is NOT consumed as RGP: it falls through to the
  Kitty parser and then to vt100, i.e. garbage can leak into the terminal
  (`src/inline.rs:213-219`).

## Stage (not reachable via RGP v1)

- Presentation mode (`Flat2d`/`Plane3d`/`Mobius3d`), warp amount, and camera
  (yaw/pitch/zoom/offset) are keyboard/mouse-driven resources
  (`src/scene/mod.rs:74-159`) with **no protocol control**. Silk carries
  opening values in the `x_ratty` header; a future RGP `c` (camera/stage)
  verb is the planned in-band mechanism.
- Warp is a time-pulsed radial "gravity well" behind the terminal plane
  (`src/systems.rs:1652-1662`); Möbius mode is a parametric ribbon whose
  radius/width/twist also scale with the warp amount
  (`src/systems.rs:1689-1710`).
- Terminal planes are unlit; RGP objects use fixed PBR material values
  (roughness 0.88, reflectance 0.18, metallic 0) with `color` as base color
  and `brightness` as a one-time material multiplier
  (`src/systems.rs:948-961, 1212-1282`).

## Grid and naming trivia

- The terminal grid is clamped to at least 2×2 (vt100 underflow guards,
  `src/terminal.rs:178-187`).
- Default grid is 104×32 (`src/config.rs:153-172`) — Silk's default stage.
- The widget crate is named `ratatui-ratty` (`widget/Cargo.toml`), though
  the root README refers to it as `ratatui-rgp` — an upstream docs
  inconsistency, found during corpus extraction.
