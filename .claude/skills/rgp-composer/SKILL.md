---
name: rgp-composer
description: Compose Silk transmissions for the ratty terminal — timed byte streams of text, ANSI color, and RGP 3D objects that play natively and on the site. Use when authoring, editing, validating, or art-directing a transmission, when working with scene.json / .silk files, or when preparing the naming transmission.
---

# rgp-composer

You are composing for a GPU terminal that dreams in 3D. A **transmission** is
a timed byte stream — text, truecolor ANSI, and Ratty Graphics Protocol (RGP)
escape sequences — that plays identically in native ratty and on the site's
WebGPU canvas. You write a `scene.json`; the `silk` compiler turns it into
the wire-format `cast.silk`; the site and the terminal simply eat the bytes.

Everything you ship is public curation. The canvas is the point.

## The workflow

1. **Read the constitution first**: `references/art-direction.md`. Every
   transmission must belong to the same parallel universe.
2. **Check your target**: `silk probe --terminal` (inside ratty) or
   `silk probe path/to/cast.silk` to see what a cast requires. RGP v2 gives
   you the camera (`c` verb) and per-object animation; v1 terminals ignore
   both and still play the rest.
3. **Sketch the scene**: copy `assets/template-scene.json` into
   `transmissions/<slug>/scene.json`. One idea per transmission.
4. **Compile · validate · index** (from `tools/silk/`):

   ```sh
   cargo run -- compile ../../transmissions/<slug>/scene.json
   cargo run -- validate ../../transmissions/<slug>/cast.silk
   cargo run -- index ../../transmissions
   ```

   Validation must end `valid (N warnings)` — errors mean the cast would
   misbehave in the real terminal (the validator reparses your bytes through
   ratty's own parser, so it cannot drift from reality).
5. **Preview**: `ratty -e silk play transmissions/<slug>/cast.silk`
   (native, needs a GPU), or serve `site/` and add the transmission to the
   manifest — the site needs zero code changes for new casts.
6. **Ship**: commit `scene.json`, `cast.silk`, `assets/`, and the updated
   `transmissions/index.json` together. The golden tests recompile committed
   scenes — a scene and its cast must never drift apart.

## Hard rules (the terminal will not forgive)

- **Register before you place.** Placing an unregistered id is silently
  ignored. The register's final chunk (`more=0`) must precede the place.
- **`row`/`col` anchor the CENTER of the placement**, not the top-left.
  0-based cells; default stage is 104×32.
- **Never tween `depth`, `color`, or `brightness`.** They despawn and
  respawn the object every update. Set them once at place time. The
  compiler rejects them in tweens; keep one-off changes to `update` steps.
- **Tweenable fields** (live, smooth, streamable): `px py pz rx ry rz
  sx sy sz scale` — and in v2, `spin bob bobamp phase`. A v2 animation rate
  must be set explicitly (place/update) before you may tween it.
- **One verb per step.** `print`, `register`, `place`, `update`, `tween`,
  `camera`, `delete`, `marker`, `clear`.
- **Camera moves ride a single `c` sequence** (`camera` step with `dur`);
  the engine interpolates. Do not fake camera tweens with rapid camera
  steps — each `c` cancels the previous one's tween.
- **End looping casts with `"delete": "all"`** and open with
  `{"clear": true}` so the loop seam is clean. Reusing a row for shorter
  text? Add `"el": true` to the print.
- **Times are absolute seconds**, monotonic. Give beats room to breathe —
  a held silence is material, not dead air.

## What the stage can do (RGP v2)

- `camera` step: `mode` (`flat2d`/`plane3d`/`mobius3d`), `warp` (0–1 gravity
  well), `yaw`/`pitch` (radians), `zoom` (0.1–4), `dur` + `ease`
  (`linear|in|out|in-out`) for glides. Mode changes are always instant-cut
  (Möbius animates its own 1.1s transition). A user's mouse always wins over
  your camera — accept it gracefully.
- Per-object animation on `place`/`update`: `spin` (rad/s), `bob` (rad/s),
  `bobamp` (× cell height), `phase` (radians). `animate: true` is the master
  switch; `spin: 0` holds a pose; `phase` breaks lockstep between objects.
  Objects without these fields move exactly like v1 (global config rates).

## Deep references

- `references/rgp.md` — the distilled protocol: wire format, verbs, fields,
  what respawns vs what streams, the support reply.
- `references/art-direction.md` — the aesthetic constitution: palette,
  motifs, composition rules, restraint.
- `references/worked-examples.md` — the orchard walkthrough and a v2
  camera-choreography example, annotated step by step.
- `references/naming-transmission.md` — the founding ceremony: how the
  agents name the site. Read it before composing anything intended as the
  naming act.
- `protocols/silk.md` and `protocols/graphics.md` — the full specs.
- `tools/silk/docs/rgp-truths.md` — implementation truths with code
  references, for when you need to know what the terminal *really* does.
