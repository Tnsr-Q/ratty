# Silk Transmission Format

Silk is a container format for **transmissions**: replayable, self-contained
recordings of the ratty language — terminal byte streams carrying text, ANSI
control sequences, and [Ratty Graphics Protocol](graphics.md) (RGP) commands.

A transmission is authored once and played anywhere a ratty byte stream can be
consumed: native Ratty (via `ratty -e silk play <cast>`), a browser-hosted
Ratty build, a text-only fallback player, or any future renderer that speaks
the same bytes. Silk deliberately knows nothing about rendering; it only
carries *bytes and time*.

The name: in *Charlotte's Web*, messages appear in a web because someone
patiently spins silk. Transmissions are spun the same way — one timed byte
event at a time.

## Design Goals

- **Renderer-agnostic**: the payload is the terminal byte stream itself, not
  a scene graph. Anything that emulates a terminal (plus, optionally, RGP)
  can play a transmission.
- **Self-contained**: 3D assets travel *inside* the stream via RGP
  payload-based registration. A `.silk` file needs no sidecar files.
- **Degradable**: a Silk cast is a strict superset of the
  [asciinema v2](https://docs.asciinema.org/manual/asciicast/v2/) file format.
  Stock asciinema players replay the text portions; RGP escape sequences are
  simply unrecognized escapes to them.
- **Forward-compatible**: unknown header keys and unknown event codes are
  ignored, mirroring RGP's ignore-unknown posture.

## Container

A `.silk` file is JSON Lines (UTF-8, LF-terminated):

- **Line 1** — a JSON object: the header.
- **Every following line** — a JSON array: one event, `[time, code, data]`.

```jsonl
{"version": 2, "width": 104, "height": 32, "title": "Upside-Down Orchard", "x_ratty": {"format": "silk/1", "agent": "hermes/loom-7", "mood": "hyperreal-pastoral", "mode": "plane3d", "warp": 0.35, "loop": true}}
[0.0, "o", "[2J[H"]
[0.12, "o", "_ratty;g;r;id=1;fmt=obj;source=payload;more=1;name=tree.obj;<base64>\\"]
[0.12, "o", "_ratty;g;r;id=1;fmt=obj;source=payload;more=0;<base64>\\"]
[0.30, "o", "_ratty;g;p;id=1;row=10;col=60;w=24;h=14;rx=180;animate=0\\"]
[0.33, "m", "chapter:orchard"]
[1.00, "o", "_ratty;g;u;id=1;ry=4.5\\"]
```

## Header

Standard asciinema v2 fields (all honored):

| Field | Type | Meaning |
|---|---|---|
| `version` | int | Always `2` (asciinema compatibility). |
| `width` | int | Terminal grid columns the cast was authored for. |
| `height` | int | Terminal grid rows. |
| `title` | string | Human-readable transmission title. |
| `theme` | object | Optional `{fg, bg, palette}` hex colors. |
| `idle_time_limit` | float | Optional cap applied to inter-event gaps. |

Silk metadata lives in one namespaced object, `x_ratty`. Standard players
ignore it. All fields are optional except `format`:

| Field | Type | Meaning |
|---|---|---|
| `format` | string | `"silk/1"`. Major version gates parsing. |
| `agent` | string | Authoring agent identity (e.g. `hermes/loom-7`). |
| `mood` | string | Free-vocabulary art-direction tag. |
| `mode` | string | Opening presentation: `flat2d`, `plane3d`, `mobius3d`. |
| `warp` | float | Opening warp amount, `0.0..=1.0`. |
| `view` | object | Opening camera: `{yaw, pitch, zoom}`. |
| `loop` | bool | Player should loop the transmission. |
| `checksum` | string | Optional `sha256:<hex>` of all event lines. |

Unknown `x_ratty` keys MUST be ignored by players.

Stage directives (`mode`, `warp`, `view`) describe the *opening* state. A
renderer that cannot honor them (a flat text player) simply ignores them.

## Events

Each event is `[time, code, data]`:

- `time` — float, **absolute seconds since transmission start**. Times MUST
  be monotonically non-decreasing. The player owns the clock: seeking,
  speed scaling, looping, and idle capping are player concerns. Consumers of
  the byte stream (the terminal) never see time.
- `code` — event type:
  - `"o"` — output: `data` is a string of bytes for the terminal (text, ANSI,
    RGP, Kitty graphics). This is the only code required for playback.
  - `"m"` — marker: `data` is a label (e.g. `chapter:orchard`). Players MAY
    surface markers for navigation. Never fed to the terminal.
  - `"i"` — input (reserved): expected user input for future interactive
    transmissions. Players without interactivity MUST ignore it.
  - Unknown codes MUST be ignored.
- `data` — string. JSON string escaping (`` for ESC) keeps arbitrary
  control bytes legal.

## Rules for RGP inside Silk

1. **Assets travel in-band.** Objects are registered with
   `source=payload` chunked base64 registration (see
   [graphics.md](graphics.md), Register Object Asset). `path=` registration
   is allowed **only** for assets embedded in Ratty itself
   (e.g. `CairoSpinyMouse.obj`); anything else would break self-containment.
2. **Chunk discipline.** A `more=1` chunk run for an object id MUST be
   terminated by a `more=0` chunk before that id is placed, and MUST NOT be
   interleaved with register chunks for a different id.
3. **Register before place.** An id MUST be registered (final chunk sent)
   at an earlier or equal `time` than its first placement.
4. **Animation is streamed.** Motion beyond RGP's built-in `animate=1`
   spin/bob is expressed as timed `u` (update) events — typically 30 per
   second during a tween. Authors SHOULD prefer live-update fields
   (`px/py/pz`, `rx/ry/rz`, `sx/sy/sz`, `scale`, `animate`) in high-frequency
   updates; `depth`, `color`, and `brightness` force the renderer to respawn
   the object and belong in scene setup, not per-frame motion.
5. **Grid bounds.** Placement anchors SHOULD lie within the header
   `width`/`height` grid.

## Conformance

**Players** MUST: parse the header; deliver `"o"` event data to the terminal
byte stream in order, pacing by `time`; ignore unknown codes and unknown
header keys. Players SHOULD honor `loop`, `idle_time_limit`, markers, and the
opening stage directives when the renderer supports them.

**Validators** MUST reject: malformed JSONL, a missing or non-object header,
non-monotonic times, and RGP rule violations (1)–(3) above.

## Media type and naming

- Extension: `.silk`. Suggested interim media type:
  `application/x-silk-cast+json-lines`.
- One transmission per directory, with its compiled cast committed alongside
  its source: `transmissions/<slug>/{scene.json, cast.silk, assets/}`.

## Relationship to the Ratty Graphics Protocol

Silk carries RGP; it never interprets it. When RGP grows (for example a
future camera/stage verb), transmissions gain the capability with **no format
change** — the new sequences are just more `"o"` bytes. The `x_ratty` stage
directives exist only because RGP v1 has no in-band way to set the opening
presentation; if that arrives, headers keep working and the directives become
optional conveniences.
