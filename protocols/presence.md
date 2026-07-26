# Ratty Collaboration Presence Protocol

The collaboration-presence organ (#25, M3.11): **source-agnostic presence
rendering** — remote humans and agent swarms embodied on the shared scene
as cursors, display names, and floating notes. Ratty performs **no
networking**: this is tier A of #25's three-tier map, the rendering
substrate any host can drive. Tier B (a relay process) and tier C (a
comune bridge) live outside this process and, when they arrive, speak
exactly this wire protocol as ordinary OSC 777. This document is the wire
contract; the grammar rules (percent-encoded `k=v&…` payloads, `tok=`
acks) are shared with every family in [query.md](query.md).

**The trust boundary**: the transport authenticates, the scene enforces
ownership *mechanically*, and nothing in-band is trusted about identity.
A participant is keyed by **(ingress source namespace, caller-local
`id=`)** — the namespace comes from the transport at apply time, never
from the byte stream, so a stream can only ever populate its own roster.
Display names and colors are rendering metadata, never authentication.

*Naming*: this organ is **collaboration** presence. The effects
Think/Confidence/Mood family that older docs call "AI presence" is a
different thing — recordable choreography about one agent's internal
state, not keyed identity.

## Design goals

- **Identity is ingress truth.** A participant exists because a live
  stream said so *now*. No replay path (macro, bookmark, rule) can forge
  a join or evict a participant.
- **Leases, never ghosts.** Every row expires on a TTL, but expiry is
  computed lazily and stays *visible*: expired rows keep answering
  queries with `fresh: false`. Rendering hides them; nothing silently
  vanishes from the record.
- **Honest failure.** Bounded rosters, explicit error codes, atomic
  commands — one bad field rejects the whole command, nothing partially
  applies.
- **Rendered = public.** Everything presence draws (name, color, cursor
  cell, note text) is public by definition; everything expired is
  private to its owner.

## Command family (OSC 777 `ratty:`)

```text
ESC ] 777 ; ratty:user.join   ; id=<id>[&name=<pct ≤64B>][&color=#rrggbb][&ttl=<secs>][&replace=true][&tok=…] BEL
ESC ] 777 ; ratty:user.renew  ; id=<id>[&ttl=<secs>][&tok=…] BEL
ESC ] 777 ; ratty:user.cursor ; id=<id>&x=<col>&y=<row>[&tok=…] BEL
ESC ] 777 ; ratty:user.leave  ; id=<id>[&tok=…] BEL
ESC ] 777 ; ratty:note        ; id=<id>&text=<pct ≤256B>&x=<col>&y=<row>[&ttl=<secs>][&replace=true][&tok=…] BEL
ESC ] 777 ; ratty:note.remove ; id=<id>[&tok=…] BEL
```

- `id=` is the caller-local identity key: ASCII `[A-Za-z0-9_.-]`,
  non-empty, ≤ 48 bytes. Identity is the pair (namespace, id); the same
  id under two namespaces is two participants.
- `name=` defaults to the id (at parse, so both wire ends agree); any
  UTF-8, ≤ 64 bytes. `color=` defaults to `#00ff00` and validates
  **strict `#rrggbb`** at apply — colors are identity-adjacent, so there
  is no lenient effects-style coercion to a default.
- Numerics are strict throughout: a malformed `ttl=`, `x=`, or `y=`
  fails the whole command as `bad-command` at parse — never a silently
  different value. Cursor cells and note anchors are **required**, not
  default-0. (The M3 `expires=` free-string key is retired; `ttl=`
  seconds governs. Unknown keys are ignored, so stale callers still
  parse — but only `ttl=` has effect.)
- Cursor/anchor cells address the **live grid** `(col, row)`; the value
  is stored as given and rendering clamps into the grid (clamp-to-edge).

## Semantics

### Leases (#21, the sensor model verbatim)

Every record stores its last-mutation time and a TTL:
`fresh = (now − updated) ≤ ttl`. Defaults: **60 s** for participants,
**300 s** for notes; supplied TTLs must be finite and above 0, clamped
to **1 s – 3600 s**. Any successful mutation of one's own record
(cursor, renew, replace) refreshes the lease. **No sweep ever deletes a
record** — rows leave the registry only through `user.leave`,
`note.remove`, or `reset`. `user.renew` on an expired participant
*revives* it: the row was still there, honestly expired, and honest
lease semantics cut both ways.

### Collisions and revisions (#16)

An existing id — fresh **or** expired — rejects `already-exists` unless
`replace=true`. A replace overwrites every field (the cursor clears — a
fresh join states complete new state) but the **revision continues**:
every record carries a `revision` starting at 1, bumped on every
successful mutation, so observers can tell "same identity, new state"
from "brand new". `user.leave` frees the id; a later re-join starts a
fresh lineage at revision 1.

### Caps

At most **16 participants** and **16 notes** per namespace. Replacing or
renewing an existing id is never a new slot and always succeeds at cap.
Expired rows still occupy their slot — they are queryable state; free a
slot with `user.leave` / `note.remove`.

## Errors

| Code | Meaning |
|---|---|
| `bad-command` | the sequence did not parse (missing required key, malformed strict numeric) |
| `bad-payload` | malformed id charset, empty name, non-`#rrggbb` color, non-finite/non-positive `ttl=`, empty note text |
| `too-large` | id over 48 bytes, name over 64, note text over 256 |
| `already-exists` | `user.join`/`note` on an existing id (fresh or expired) without `replace=true` |
| `unknown-id` | `user.renew`/`user.cursor`/`user.leave`/`note.remove` on an id absent from the caller's namespace |
| `namespace-cap` | a *new* id past the 16-participant / 16-note namespace cap |
| `not-permitted` | a presence command that did not arrive from live ingress (see Classification) |

Rejections land in the caller's `state.errors` ring; `tok=` commands
additionally get their error ack.

## Reading back (OSC 778)

- **`state.presence`** (paginated): the roster rows, participant rows
  before note rows, (namespace, id)-ordered within each kind. Three-tier
  read scope: the caller's **own namespace in full including expired
  rows** (`fresh: false` visible, #21 — your own lease state is your
  own business), foreign namespaces as **fresh rows only** — rendered =
  public, and an expired foreign row's *existence* must not leak. Row
  shapes:
  - participant: `{kind:"participant", ns, id, name, color,
    cursor:{x,y}|null, fresh, age_secs, ttl_secs, revision}`
  - note: `{kind:"note", ns, id, text, x, y, fresh, age_secs, ttl_secs,
    revision}`

  Resumed cursors are monotone-by-key, not snapshot-stable (the
  [query protocol](query.md) contract): a roster that mutates between
  page fetches — including a foreign row expiring, which removes it
  from the visible set — can shift later rows across the cursor
  boundary and omit them from the walk. Rosters small enough for one
  page (the common case) are unaffected; re-query from the start for a
  consistent snapshot.
- **`state.namespaces`** (append-only extension): each namespace row
  gains `participants` and `notes` — **fresh counts only**, and a
  namespace with nothing public (no objects, nothing fresh) is absent
  entirely. Rosters ride the paginated `state.presence`, never this
  unpaginated aggregate: one maxed namespace roster alone exceeds a
  reply page.
- **`caps`**: `limits.presence_*` (roster caps, byte caps, TTL defaults
  and clamps).

There are no execution handles: every presence op completes immediately.

## Classification (#16 / #21 / #25)

The whole family — `user.join`, `user.renew`, `user.cursor`,
`user.leave`, `note`, `note.remove` — is **control-plane**, and
deliberately nothing else: not rule-safe, not scene-global, not
execution control.

- **Never recorded**: the macro recorder tap skips the family. A
  macro-replayed `user.join` would forge liveness — a participant that
  "joined" because a recording was played — and a replayed `user.leave`
  would evict a real participant.
- **Wire-origin only**: belt-and-suspenders beside the structural
  exclusion, the applier refuses any presence command whose origin is
  not live ingress (macro playback, bookmark relowering, rule fire) with
  `not-permitted` — the reactive organ's chain-depth guard, applied to
  identity.
- **`reset`** clears every roster silently (its single ack belongs to
  the scene reset) — presence is wire-tier state with no trusted tier.

## CLI

```sh
ratty-ai user join alice --name "Alice W" --color "#00ffcc" --ttl 120
ratty-ai user renew alice --ttl 120
ratty-ai user cursor alice -x 12 -y 4
ratty-ai user leave alice
ratty-ai note add n1 "review this" -x 12 -y 6 --ttl 600
ratty-ai note remove n1
ratty-ai state presence          # → the roster over 778
ratty-ai state namespaces        # → aggregate counts
```

`--replace` opts into overwrite on `user join` / `note add`; `--ack`
prints the ack (`--json` structured). There are no presence-specific
query subcommands — `state.presence` rides the generic query surface.

## Rendering

Fresh rows render; expired rows do not (that is the visible expiry on
the scene — the row itself stays queryable). Cursor markers are small
unlit caret meshes in the main scene, live in all three presentation
modes (screen-space in the flat view, pinned to the warped surface
through the RGP projection in 3D), with the participant's name label
beside the cell in the participant's color; notes draw as subtle filled
rectangles with an accent border and their text, in-texture, warping
with the plane. The wire carries no color on `note`, so the border is
the fixed presence accent, never per-participant. Out-of-grid cells
clamp to the nearest edge cell at render; the stored value is
untouched. Every roster mutation *and* every fresh→expired flip
requests a terminal redraw, so an expiring note disappears from an
otherwise idle terminal.

## wasm

Full parity, no gates: the organ is pure ECS state on `Res<Time>` and
the read side is the same query channel. Honest limitations:

- While a tab is hidden, rAF throttling defers `Time` — leases stall
  with everything else (the sensor/avatar stance): durations stretch in
  wall time, and nothing expires while unobserved.
- Cursors and notes pin to **live grid cells** (the viewport); they do
  not scroll with the text they were placed beside.
- Name labels and note text render through the vello stroke font:
  uppercase letters, digits, and limited punctuation — other glyphs draw
  as hollow boxes. The full text stays queryable regardless.
- Notes render one line anchored at their cell, truncated at the grid's
  right edge; labels cap at 16 glyphs. A participant without a reported
  cursor renders nothing (marker and label hang off the cursor cell).
- A plain PTY is one effective principal: every local writer shares
  namespace 0. Distinct ids under one namespace are honestly labeled
  distinct *participants*, not authenticated identities — authentication
  is the transport's job (the tier B/C boundary), never in-band.

See also: [query.md](query.md) (the 778 envelope, acks, pagination),
[reactive.md](reactive.md) (the lease and chain-depth doctrines this
organ reuses), [avatar.md](avatar.md) (the scene-owned presence
singleton this organ deliberately is not).
