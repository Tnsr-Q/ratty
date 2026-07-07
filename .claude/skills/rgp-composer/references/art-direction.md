# The aesthetic constitution

This is the house style of the ⟨unnamed⟩ site — the shared parallel universe
every transmission inhabits. It is not a mood board; it is physics.

## The premise

**Charlotte's Web × brutalism × hyperrealistic absurdism.**

A parallel universe that runs on inverted, *polite* physics. Trees grow
downward and gravity files no complaint. Fruit falls into the sky at dawn,
and the sky, being patient, catches it. One-dimensional predators hunt fine
art, but they cannot eat what has volume. Nothing here is random: every
absurdity is calm, considered, and internally consistent — that is the
"hyperrealistic" part. The universe never winks at the audience.

The terminal itself is a creature in this universe, not a display device.
Its warp is a gravity well. Its cursor is a white rat that keeps its own
time. When text stretches near the well, that is the universe bending — a
feature to compose with, never a bug to avoid.

## Voice

- Sentences are small, declarative, and certain. Lowercase is home;
  UPPERCASE is architecture (headers, transmission ids, structural labels).
- The narrator states impossible things as administrative facts:
  "gravity files no complaint."
- Tenderness and brutality sit in the same line. Never sarcasm. Never irony
  that condescends to the universe.
- A transmission is a poem with a body. If the words could stand alone as a
  screenshot, the words are working.

## Palette (Kanagawa-adjacent; hex for `fg`/`bg`/`color`)

| role | hex | use |
|---|---|---|
| void | `#1f1f28` | stage background, always |
| bone | `#dcd7ba` | body text, the narrator |
| gold | `#e4bf56` | structural text: titles, the loop's promise, emphasis |
| ash | `#727169` | metadata, parentheticals, stage whispers |
| blood | `#c4746e` | predators, warnings, the one sharp thing |
| moss | `#8a9a7b` | growth, turning, the orchard's verbs |
| sky | `#7e9cd8` | (sparing) water, sky, the patient catcher |

One accent per transmission carries the emotional weight. The others
support. Never more than three foreground colors in a single frame of text.

## Objects

- **Low-poly, vertex-colored OBJ** is the house material. Paint color into
  the vertices (`v x y z r g b`) — painterly gradients, no textures, no
  materials. The tree in `transmissions/orchard-upside-down/assets/tree.obj`
  is the reference: hex-prism trunk, blobby canopy, two fruit.
- Silhouettes must read at terminal-cell resolution. If it needs a caption
  to be recognized, simplify it.
- Inversion is a verb: `rx: 180` is how a tree grows downward. Use the
  transform, not a pre-rotated mesh, so the gesture stays legible in the
  scene source.
- One-dimensional predators are thin extruded polylines — all length, no
  volume. They move in straight lines; they do not understand curves.

## Composition (the 104×32 stage)

- **Text keeps a column; objects keep the air.** The orchard rule: verse on
  the left (cols 3–45), world on the right. Let them overlap only on
  purpose, once, for a reason.
- Rows 0–2 are architecture (transmission id, mood line). The last rows
  belong to the closing line. The middle is where things live and turn.
- Objects near the gravity well distort with the plane. Placing something
  half-into the well is a legitimate dramatic act.
- Depth sorts meaning: `scale` and `brightness` for near/far, not clutter.
  Two objects of the same mesh at different scales and brightnesses read as
  a landscape.
- Emptiness is load-bearing. If every cell is doing something, nothing is.

## Time

- 20–30 seconds per looping transmission. The loop is a promise — the
  closing line should make return feel intended ("begin again.").
- Lines of a poem arrive 0.4–0.8s apart; a new stanza waits a full second.
  Objects enter after the words that summon them, not before.
- Tweens are weather, not action scenes: 3–7 second `in-out` drifts.
  A 360° `ry` turn over 6.5s is the house tempo (the orchard turns).
- `phase` desynchronizes; different `spin` rates drift siblings apart —
  "the far tree spins on the house rhythm. the near one keeps its own time."
- Use `marker` events to name chapters; players expose them.

## Camera (v2) — restraint doctrine

The camera is the universe's slow attention, not a music video.

- At most **two or three** camera moves per transmission. Establish
  (`at: 0.0`, instant), then one slow glide mid-piece, then perhaps a
  settling before the loop closes.
- Warp glides (`warp` + `dur` 3–6s) read as the well breathing.
  Pitch/yaw drifts stay under ~0.15 rad — the audience should feel it,
  not name it.
- Mode is a scene cut. Cut once, if at all. The Möbius strip is a
  pilgrimage, not a transition effect — save it for transmissions *about*
  recursion.
- The viewer's mouse always outranks your camera (the engine enforces it).
  Compose so a viewer who grabs the stage and lets go still finds the piece
  coherent.

## Mood tags

`meta.mood` is a compound of `texture-noun`: `hyperreal-pastoral`,
`brutal-tender`, `administrative-holy`, `patient-predatory`,
`recursive-calm`. Coin new ones in this grammar; the site displays them
verbatim as exposed metadata (brutalism shows its materials).

## The three tests

Before shipping, a transmission must pass:

1. **The parallel-universe test** — does every element obey the inverted
   physics without explaining itself?
2. **The screenshot test** — is any single paused frame composed enough to
   frame?
3. **The designer test** — would a professional look at it and have no idea
   how it was made? (A terminal, reading a poem in escape codes, dreaming
   in 3D.)
