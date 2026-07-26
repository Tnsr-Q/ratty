# Ratty Avatar Protocol

The avatar organ (#23, M3.10): a **scene-owned presence singleton** — one
mascot, one voice — with a fair bounded speech queue. The avatar belongs
to the ratty scene, not to whichever agent spoke last. This document is
the wire contract; the OSC 777 grammar rules (percent-encoded `k=v&…`
payloads, `tok=` acks) are shared with every family in
[query.md](query.md).

## Command family (OSC 777 `ratty:`)

```text
ESC ] 777 ; ratty:avatar.set          ; [model=<id>][&position=<anchor>][&dx=<px>][&dy=<px>][&scale=<f32>][&tok=…] BEL
ESC ] 777 ; ratty:avatar.show         ; [tok=…] BEL
ESC ] 777 ; ratty:avatar.hide         ; [tok=…] BEL
ESC ] 777 ; ratty:avatar.gesture      ; gesture=bob|tilt|lean|nod|spin[&tok=…] BEL
ESC ] 777 ; ratty:avatar.speak        ; text=<pct ≤1000B>[&from=<pct ≤64B>][&duration=<secs>][&tok=…] BEL
ESC ] 777 ; ratty:avatar.stop         ; [tok=…] BEL
ESC ] 777 ; ratty:avatar.cancel       ; id=<execution-handle>[&tok=…] BEL
ESC ] 777 ; ratty:avatar.speech.clear ; [ns=<n>|scope=all][&tok=…] BEL
```

- `avatar.set` is a **partial update** (the `cursor` precedent): every key
  optional, and any invalid field rejects the whole command atomically.
  `model=` is an immutable registry id (`mascot`) — never a filename,
  path, or URL (#12); the backing asset is terminal-side detail. Numeric
  offsets/scale **clamp** to their documented bounds and still commit
  (the wire requests, it never owns); a malformed numeric is a
  `bad-command`.
- `position=` names one of the **nine anchors**: `top-left`, `top`,
  `top-right`, `left`, `center`, `right`, `bottom-left`, `bottom`,
  `bottom-right`. The default is `bottom-right` — state, not parser.
- `avatar.speech.clear`: bare clears the caller's own pending utterance;
  `ns=<n>` clears one namespace's; `scope=all` cancels the whole queue
  including the active utterance. `ns=` and `scope=` are mutually
  exclusive; `scope=` accepts only `all`.

## The capability split (#23 §2)

| Tier | Commands | Gate |
|---|---|---|
| **Trusted scene-level** | `avatar.set`, `avatar.show`, `avatar.hide`, `avatar.speech.clear;ns=<other>`, `avatar.speech.clear;scope=all` | `[trust.local] avatar_scene` (default `true`), read through the capability spine (`SceneCapability::AvatarScene`) |
| **Ordinary** | `avatar.speak`, `avatar.gesture`, `avatar.stop`, `avatar.cancel` (owner-only), bare/own-`ns=` `avatar.speech.clear` | none |

A privileged attempt without the grant answers `ok=0;code=not-permitted`.
The grant is **out-of-band trust only**: the config file at load (the
embedding page's TOML on wasm) — no wire command can read or write it,
and the wire can never self-escalate. One PTY is one principal: granting
`[trust.local] avatar_scene` grants it to every process writing this
terminal (the `allow_scene_ambient` posture); single-tenant sessions
direct their own scene, multi-writer operators revoke it. `caps.trust`
reports the caller's live grants.

```toml
[trust.local]
# Avatar scene control for the local ingress principal.
avatar_scene = true
```

## Speech: a long-running operation (#18)

`avatar.speak` acks exactly once, at admission:

- `ok=1;code=started;data={"id":…,"position":0,"eta_ms":0}` — took the
  voice now, or
- `ok=1;code=queued;data={"id":…,"position":P,"eta_ms":E}` — admitted to
  the queue, or
- `ok=0` with an explicit error (below).

`id` is the execution handle, inspected later through
`state.executions`; **absence from `state.executions` is the completion
signal** (no tombstones, no events — `t=e` stays reserved). `position`
counts the utterances (active + pending) served before this one; 0 means
speaking now. Estimates are pinned at admission: cancellations shorten
them; nothing can lengthen them (see the fairness bound).

**Attribution**: every utterance carries its speaker — the
ingress-derived namespace, stamped at apply time. The optional `from=`
label is presentation decoration only; a byte stream cannot claim an
identity.

**Duration** is pinned at admission: derived from the text
(~60 ms/char) when `duration=` is absent, clamped to **750 ms – 15 s**
either way; a non-finite or non-positive `duration=` rejects
`bad-payload`.

### The fair queue

One active utterance; at most **4 pending globally**; at most **1
pending per agent**; FIFO within an agent. Rotation across agents is
**seniority-snapshot round-robin**: when the current rotation pass
empties, the next pass snapshots every namespace with pending speech,
ordered by how long its oldest pending utterance has waited; an
utterance admitted mid-pass waits for the next snapshot. This makes the
start bound provable: once admitted, only the namespaces already pending
at admission (≤ 3) are ever served before you, so the worst-case wait is
`remaining(active) + 3 × 15 s`, regardless of other agents' behavior.

### Errors

| Code | Meaning |
|---|---|
| `busy` | the global pending cap (4) is full |
| `agent-queue-full` | the caller's single pending slot is taken |
| `text-too-long` | `text=` over 1000 UTF-8 bytes |
| `too-large` | `from=` over 64 bytes |
| `bad-payload` | non-finite or non-positive `duration=` |
| `nothing-active` | `avatar.stop` with nothing live; `avatar.speak`/`avatar.gesture` while the avatar is hidden |
| `unknown-id` | `avatar.cancel` handle that names nothing live (finished, cancelled, or minted by a previous session) |
| `not-owner` | `avatar.cancel` on another agent's utterance |
| `not-permitted` | privileged form without the avatar-scene capability |
| `unknown-model` / `unknown-anchor` / `unknown-gesture` | name outside the closed vocabularies |

### Cancellation

An agent cancels only its own speech: `avatar.stop` (own current or
queued), `avatar.cancel;id=` (own, by handle). Privileged `avatar.hide`
and `scene reset` cancel the current utterance and clear the queue;
`avatar.set` never touches the queue — changing the mascot never
silently destroys valid queued speech.

## Reading back (OSC 778)

- **`state.scene.avatar`** (scene-global public state): `visible`,
  `speaking`, `speaker` (namespace or null), `execution` (active handle
  or null), `queue_depth`, plus `model` and `position`. **No utterance
  text appears here** — not even the active speaker's.
- **`state.executions`** (caller-owned): the caller's own active and
  queued utterances in full (`id`, `status`, `text`, `from`,
  `duration_ms`, `remaining_ms`/`position`+`eta_ms`), merged with the
  macro slot. Other agents' queued text is structurally unexposable.
- **`caps`**: `limits.avatar_*` (text/speaker bytes, duration clamps,
  queue caps, offset bound), `avatar_models` (the registry vocabulary),
  and `trust` (the caller's live capability grants).

## Classification (#16 / #21)

- `avatar.set`, `avatar.show`, `avatar.hide`, and wide
  `avatar.speech.clear` are **scene-global**: a macro containing one
  classifies privileged and needs the exclusive scene lock to play.
  (`ns=` classifies privileged even for the recorder's own namespace —
  the caller is unknown at classification time; use the bare form for an
  unprivileged recording.)
- `avatar.speak` and `avatar.gesture` are recordable choreography, but
  **no avatar command is rule-safe**: speak consumes the shared voice
  (cross-agent blast radius), so reactive rules can never fire any
  `avatar.*`, directly or via `macro.play` (the rule-safe finalize +
  fire-time recheck).
- `avatar.stop`/`avatar.cancel` are **execution control**: session-scoped
  handles are transport-epoch metadata, so the recorder tap skips them
  and the trusted-macro loader refuses them.
- `reset` clears the avatar (hide + cancel + defaults) **ungated**:
  reset is return-to-baseline, not scene control — a caller cannot
  choose a representation or selectively clear one agent, and
  hidden-and-empty is the default state (the ungated ambient-bed reset
  fade precedent).

## Assets (#23 §3)

The registry id `mascot` resolves to an embedded, **build-audited** GLB
(`build/glb_audit.rs`: byte/triangle/material/texture caps, PNG-only,
self-contained, `extensionsRequired` rejected, decoded-runtime
estimate). Embedded glTF loads through the in-memory `embedded://` asset
source on **both** targets — the avatar never answers `unsupported` on
the web.

## Presentation

The avatar is **presence furniture, fixed through terminal scrolling and
camera movement, in every mode** — structurally: the mascot renders
through its own fixed-pose orthographic camera on an isolated render
layer, and the bubble draws through its own vello scene → texture →
screen-space overlay quad; the RGP `c`-verb machinery only ever writes
the terminal plane camera, so neither can warp, yaw, zoom, or
Möbius-twist. The camera stack is 0 flat-2D → 1 terminal-3D → 5 bubble →
6 mascot → 10 effects wash (the mood wash still tints the avatar like
everything else).

- **Bubble**: real shaped prose (parley + the embedded DejaVu faces,
  identical on native and wasm — mixed case, full Unicode, wrapped),
  with an accent-bordered scrim, a tail toward the mascot, and a bold
  **attribution chip visible from frame one**. The typewriter reveal is
  cluster-bounded: ligatures never half-draw, RTL reveals in reading
  order, combining marks appear atomically, and progress (a byte offset)
  survives a mid-utterance resize or DPI change. Over-tall text clips
  behind a visible marker; the full text stays queryable.
- **Gestures** (`bob`, `tilt`, `lean`, `nod`, `spin`): named
  root-transform choreographies plus a persistent subtle idle bob — no
  skinning, no animation graph in M3, so a later skinned milestone
  replaces the mascot behind the same verbs.
- **Speaking glow**: a radial disc behind the mascot, driven by exactly
  the utterance envelope (250 ms ramp, gentle pulse, 400 ms fade).
- Idle cost is zero: the overlay texture pair shrinks to 1×1 and the
  quad hides when nothing is speaking.

## CLI

```sh
ratty-ai avatar set --model mascot --position bottom-right --dx 12 --scale 1.5
ratty-ai avatar show / hide
ratty-ai avatar gesture nod
ratty-ai --ack --json avatar speak "Deploy finished" --from ci --duration 4
ratty-ai avatar stop
ratty-ai avatar cancel <handle>
ratty-ai avatar speech-clear [--ns 2 | --all]
ratty-ai state scene    # → .avatar
```

With `--ack --json`, `avatar speak` prints the started/queued ack
including `data` (handle, position, eta).

## wasm

Full parity, no gates: the organ is pure ECS state on `Res<Time>`, the
grant arrives via the embedder's config TOML, and the mascot asset rides
the memory-backed `embedded://` source. Honest limitation: while a tab
is hidden, rAF throttling defers `Time` — utterance expiry and promotion
stall together with everything else; durations stretch in wall time, the
queue order never changes.
