# RGP distilled (v2)

The Ratty Graphics Protocol, compressed to what a composer needs. Full spec:
`protocols/graphics.md`; implementation truths: `tools/silk/docs/rgp-truths.md`.
You normally write scene.json and let `silk` emit these bytes — read this to
understand what your scenes compile into and what the terminal really does.

## Wire format

```
ESC _ ratty;g;<verb>[;key=value…] ESC \
```

APC-framed, `;`-separated. Terminator is `ESC \` or the C1 byte `0x9c`.
Unknown verbs and keys are **silently ignored** — that is how v2 casts
degrade gracefully on v1 terminals. A malformed sequence is NOT consumed
and leaks into the terminal as text — the validator catches this.

## Verbs

| verb | does | scene.json step |
|---|---|---|
| `s` | support query → one-line reply | (use `silk probe --terminal`) |
| `r` | register asset (path or chunked base64 payload) | `register` |
| `p` | place object at a cell anchor | `place` |
| `u` | update style/transform of a placed object | `update` / `tween` |
| `d` | delete one object (`id=N`) or ALL (no id) | `delete` |
| `c` | stage/camera (v2) | `camera` |

## The respawn split (the most important fact in this file)

`u` fields divide into two castes:

- **Live** (applied every frame to the existing entity — smooth, free,
  streamable): `px py pz` (offset), `rx ry rz` (rotation, degrees),
  `sx sy sz` (non-uniform scale), `scale`, `animate`, and v2's
  `spin bob bobamp phase`.
- **Respawn-forcing** (despawn + rebuild the whole object): `depth`,
  `color`, `brightness`. Set once. Never stream. Never tween.

## Placement

- `row`/`col` = **center** of the `w`×`h` cell span, 0-based.
- Placing an unregistered id is silently ignored — register first, and the
  payload's final `more=0` chunk must have arrived.
- Objects scroll with the text. There is no overlay mode.
- `depth > 0` extrudes flat meshes into solids and adds an automatic
  oblique 3/4-view tilt on top of your rotation.

## Registration

- Formats: `obj`, `glb`, `stl` (`gltf` is an undocumented glb alias).
- Payloads: base64, chunked at 3072 chars, `more=1` … `more=0`. Don't
  interleave two objects' chunk runs.
- OBJ honors **vertex colors** (`v x y z r g b`) — the house painting
  technique. `normalize=1` (default) centers and unit-scales OBJs.
- Embedded-in-ratty paths (self-contained casts may also use these):
  `CairoSpinyMouse.obj`, `SpinyMouse.glb`, `SkateMouse.stl`, `Ferris.glb`.

## Animation (v2)

Gated by `animate=1`. Without v2 fields: global-config spin+tilt+bob,
identical for every object (v1 behavior). With v2 fields:

- `spin` rad/s (Y axis; tilt follows at 0.7×), `bob` rad/s,
  `bobamp` × cell height, `phase` radians added to both channels.
- Rates integrate frame-to-frame: mid-flight changes are smooth, `spin=0`
  **holds** the current angle (pose-hold), a fresh placement starts at
  `phase` deterministically.
- Set-only: you cannot revert a rate to "use the global config".
- A respawn (depth/color/brightness change) resets the accumulated pose.

## Stage / camera (v2, the `c` verb)

```
ESC _ ratty;g;c;mode=plane3d;warp=0.35;yaw=0.18;pitch=0.08;zoom=1.0;dur=2;ease=inout ESC \
```

All fields optional, absolute. Clamps: warp 0–1, zoom 0.1–4; yaw/pitch in
radians. Rules the engine enforces:

- `mode` is always an instant cut (Möbius runs its own 1.1s transition);
  a mode change cancels any running camera tween.
- `dur`/`ease` tween warp/yaw/pitch/zoom engine-side at frame rate.
- A new `c` replaces the previous tween, retargeting from current values.
- **User input wins**: mouse/keyboard/JS controls cancel your tween.
- During a Möbius transition, yaw/pitch/zoom are dropped; warp applies.
- Camera fields sent in flat2d are stored and apply on 3D entry.

## The return channel

The `s` reply is the ONLY renderer→application message:

```
ESC _ ratty;g;s;v=2;fmt=obj|glb|stl;path=1;payload=1;chunk=1;anim=1;depth=1;color=1;brightness=1;transform=1;update=1;normalize=1;stage=1;tween=1;objanim=1 ESC \
```

`stage`/`tween`/`objanim` are the v2 keys. No reply = not ratty.
No events, no clicks, no acks — a transmission is a broadcast, not a dialog.

## Deletion

`d` with no id deletes **everything**, including Kitty images other
programs placed. In a cast that's exactly what you want at the loop seam;
in a live shell, be polite.
