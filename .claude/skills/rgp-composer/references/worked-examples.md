# Worked examples

## 1. Orchard, Inverted (the reference transmission, pure v1)

`transmissions/orchard-upside-down/scene.json` — study the real file; this
is its anatomy.

**Meta and stage** — identity, mood, and the opening state:

```json
{
  "meta": { "title": "Orchard, Inverted",
            "agent": "loom/prototype-0",
            "mood": "hyperreal-pastoral" },
  "stage": { "cols": 104, "rows": 32,
             "mode": "plane3d", "warp": 0.35,
             "theme": { "fg": "#dcd7ba", "bg": "#1f1f28" },
             "loop": true }
}
```

The stage opens already warped — the audience arrives inside the universe,
not at a menu. `loop: true` makes the closing line a promise.

**The opening beat** — clear, then architecture:

```json
{ "at": 0.0,  "clear": true },
{ "at": 0.2,  "print": { "row": 1, "col": 3,
    "text": "TRANSMISSION 001 :: ORCHARD, INVERTED",
    "fg": "#e4bf56", "bold": true } },
{ "at": 0.55, "print": { "row": 2, "col": 3,
    "text": "mood: hyperreal-pastoral // spun by loom/prototype-0",
    "fg": "#727169" } }
```

**Register early, place on cue.** Both trees register (same OBJ, second
gets `"name": "tree-far.obj"` for diagnostics) before the first is placed.
The near tree lands as the poem's first line arrives:

```json
{ "at": 1.0, "place": { "id": 1, "row": 13, "col": 74, "w": 28, "h": 16,
                         "rx": 180, "scale": 1.15, "brightness": 1.1 } }
```

`rx: 180` IS the inversion — the tree grows downward as a transform, so the
gesture is legible in the source. Center anchor row 13 col 74 puts it in
the right half; the verse keeps the left column.

**Motion as weather** — the orchard turns in two long breaths:

```json
{ "at": 2.0, "tween": { "id": 1, "dur": 6.5, "fps": 30, "ease": "in-out",
                         "to": { "ry": 360 } } },
{ "at": 9.0, "tween": { "id": 1, "dur": 5.5, "fps": 30, "ease": "in-out",
                         "to": { "ry": 540, "py": 0.55 } } }
```

The second tween continues from the first's end state (the compiler tracks
it), rising `py` as it turns — fruit falling into the sky.

**Two clocks**: the far tree uses `animate: true` (the house rhythm — the
global spin), while the near tree is tween-driven — "the far tree spins on
the house rhythm. the near one keeps its own time."

**The loop seam** — delete everything at the end; the cast opens with
`clear`, so the restart is clean:

```json
{ "at": 19.2, "print": { "row": 27, "col": 3,
    "text": "the loop is a promise. begin again.", "fg": "#e4bf56" } },
{ "at": 21.0, "delete": "all" }
```

## 2. A v2 camera choreography (fragment)

The camera is the universe's slow attention. Establish instantly at 0.0,
glide once, settle before the loop:

```json
{ "at": 0.0,  "camera": { "mode": "plane3d", "warp": 0.2,
                           "yaw": 0.18, "pitch": 0.08, "zoom": 1.0 } },
{ "at": 8.0,  "camera": { "warp": 0.55, "pitch": 0.16,
                           "dur": 5.0, "ease": "in-out" } },
{ "at": 18.0, "camera": { "warp": 0.3, "pitch": 0.08,
                           "dur": 3.0, "ease": "out" } }
```

The `at: 0.0` establishing shot makes the cast self-contained (it wins over
whatever the header says, and works on any v2 player with only `feed()`).
Each glide is one `c` sequence — the engine interpolates at frame rate.

## 3. Per-object animation (v2 fragment)

Three of the same mesh, desynchronized into a landscape:

```json
{ "at": 3.0, "place": { "id": 1, "row": 10, "col": 60, "w": 20, "h": 12,
    "animate": true, "spin": 0.35, "bob": 1.1, "bobamp": 0.05 } },
{ "at": 3.4, "place": { "id": 2, "row": 22, "col": 46, "w": 12, "h": 7,
    "animate": true, "spin": 0.35, "phase": 2.1, "scale": 0.7,
    "brightness": 0.8 } },
{ "at": 3.8, "place": { "id": 3, "row": 24, "col": 84, "w": 10, "h": 6,
    "animate": true, "spin": 0.5, "phase": 4.2, "scale": 0.55,
    "brightness": 0.65 } }
```

Same `spin` + different `phase` = a family breathing out of step; a
different `spin` on the third lets it drift apart over the loop. Scale and
brightness fall off together — that is depth.

**Pose-hold**: to freeze an animated object mid-gesture, update it to
`{"spin": 0, "bob": 0}` — it holds exactly where it is (rates integrate;
zero rate means no further motion, no snap).

**Accelerating a spin smoothly** — rates are tweenable once set:

```json
{ "at": 12.0, "tween": { "id": 3, "dur": 4.0, "ease": "in",
                          "to": { "spin": 3.0 } } }
```

## 4. Patterns worth stealing

- **Summon order**: print the line that names a thing, then place the
  thing 0.2–0.4s later. The words conjure; the object obeys.
- **Row hygiene**: reusing a row for shorter text leaves the old tail —
  add `"el": true` to the print (erase to end of line, in the print's
  colors).
- **The parenthetical register**: ash-gray (`#727169`) stage whispers in
  parentheses read as the universe's marginalia.
- **Markers as chapters**: `{ "at": 1.1, "marker": "chapter:orchard" }` —
  free navigation for players, free structure for readers of the cast.
- **Validate paranoia**: after any edit, recompile AND revalidate. The
  committed cast and scene must stay byte-consistent (golden tests enforce
  it for shipped transmissions).
