# Ratty Macros Protocol (OSC 777 macro family)

Where the [Ratty Graphics Protocol](graphics.md) carries the 3D payload and
the [Ratty Query Protocol](query.md) (OSC 778) is the read side, the
`macro.*` family rides the OSC 777 control channel to give an agent (or a
transmission) a way to **record its own choreography and replay it**. A
macro is a relative-timestamped sequence of canonical control commands,
tapped off the lowering path between `macro.record` and `macro.stop` and
replayed through the *same* validation and lowering path the live wire uses.

A macro is not a transmission. A [transmission](silk.md) captures
*everything* — it is a byte stream. A macro captures *only the AI control
channel* — a command stream. They share a serialization family and nothing
else.

## Design goals

- **Recording is a tap, not a mode.** The commands between `macro.record`
  and `macro.stop` execute normally the frame they arrive; recording
  observes the same live stream every other organ reads. There is no
  separate "record mode" that changes how a command behaves.
- **Replay is re-validation, never a privileged restore.** Playback
  re-injects the captured commands through the ordinary appliers, under the
  caller's **current** capabilities. Nothing is baked in at record time; a
  command that is no longer valid (a lost capability, a target that no
  longer exists) fails at play time, explicitly, into `state.errors` — it is
  never silently forced through.
- **Capture the channel, not the transport.** Terminal text, raw OSC bytes,
  and PTY input are never captured. Neither is the control-plane class —
  `macro.*` itself, reactive/`rule.*` registration, and query/transport
  envelopes (which are OSC 778 and never reach this channel). Ack `tok=`
  correlation tokens are transport metadata and are dropped before capture.
- **The wire never touches the filesystem.** `macro.export;to=` and
  `macro.run;path=` are rejected `wire-filesystem-access`. The terminal byte
  stream is untrusted (see [#12](graphics.md)); promotion to durable storage
  is a trusted-tier act, never a wire command.
- **Bounded everything.** Commands-per-macro, macros-per-namespace, and
  recording wall-clock are capped; playback (especially `mode=instant`)
  respects a per-frame execution budget. The caps are advertised in
  `caps().limits`.
- **Same semantics native and wasm.** The session registry is in-memory and
  dies with the session on both — browser-equal by construction.

## Transport

Macro commands are ordinary OSC 777 control sequences (see the module doc in
`src/osc.rs` for the channel itself):

```text
ESC ] 777 ; ratty:macro.record ; name=<name>[&mode=replace][&tok=<token>] BEL
ESC ] 777 ; ratty:macro.stop    ; [tok=<token>] BEL
ESC ] 777 ; ratty:macro.play    ; (name=<name>|hash=<hex>)[&rate=<f32>][&mode=instant][&scope=session|trusted][&tok=<token>] BEL
ESC ] 777 ; ratty:macro.export  ; name=<name>&to=<path>   → always rejected
ESC ] 777 ; ratty:macro.run     ; path=<path>             → always rejected
```

- `macro.record` requires `name=`. `mode=` is a closed vocabulary — absent
  records fresh, `replace` overwrites transactionally; any other value is a
  bad command (a `tok=` caller gets `bad-command`), matching `bookmark`.
- `macro.play` requires **one** of `name=` or `hash=`. `mode=` (absent or
  `instant`) and `scope=` (absent, `session`, or `trusted`) are closed
  vocabularies. `rate=` is any finite value `> 0.0` (clamped to a ceiling);
  a non-positive or non-finite rate rejects `bad-payload`.
- `tok=` opts into the delivery ack over OSC 778 exactly as the other
  families document. The recorder never captures a `tok=` token.

## Command classification

Every parsed control command falls into exactly one recording class:

| Class | Members | Recorded? | Privilege |
| --- | --- | --- | --- |
| **Control-plane** | `macro.*`, reactive/`rule.*` registration | never | — |
| **Scene-global** | `mode`, `warp`, `reset` | yes | marks the macro **privileged** |
| **Choreography** | everything else (objects, cursor, viz, sound, effects, bookmarks, presence…) | yes | stays inside the caller's namespace |

A macro that captured **any** scene-global command is classified
*privileged* at record time. A privileged macro must acquire the terminal's
single **exclusive scene lock** to play; if the lock is held it rejects
`scene-locked`. This is the first concrete edge of the cross-organ scene
arbitration the M3 map otherwise carries as fog: one scene-global playback at
a time, across all agents.

> **`reset` inside a recording.** `reset` is a scene-global command, but on
> the terminal it is handled — not captured — and it *cancels* any active
> recording (it is a full session reset). A recording therefore never
> contains a `reset`. The [Silk](silk.md) `macro` block rejects an enclosed
> `reset` at compile time for exactly this reason.

## Timing and concurrency

- **Default playback preserves the recorded relative deltas.** `rate=2.0`
  plays at double speed, `rate=0.5` at half. `mode=instant` drops the
  intentional delays entirely, **preserving command order** and respecting
  the per-frame budget.
- **Per-agent single slot.** An agent may have at most one active recording
  *or* playback. Starting an operation while the slot is busy rejects
  `busy`. Different agents operate concurrently — their commands stay inside
  their own object namespaces (see [#12](graphics.md)).
- **`macro.stop` finalizes or cancels.** It finalizes an active recording
  (saving it) or cancels an active playback. With nothing active it rejects
  `nothing-active`.
- **No recursion.** A macro can neither record nor play macros: `macro.*` is
  never captured, and a trusted macro is validated to contain none.

## Storage and the trust boundary

- **Session registry.** Per-agent, in-memory, keyed by name; dies with the
  session. `reset` clears it (and cancels active slots and releases the
  scene lock).
- **Trusted registry.** Durable, wire-immutable, keyed by name; survives
  `reset`. Macros enter it only through a trusted-tier act (config / CLI /
  UI / controller) — never from the wire. Playback of a trusted macro still
  passes the caller's current ownership and validation checks.
- **Resolution.** A bare `macro.play;name=` resolves the caller's session
  registry first, then the trusted registry. `scope=session` or
  `scope=trusted` pins the lookup; `hash=<hex>` addresses a macro by its
  immutable content id directly (across both registries), defeating any
  shadowing ambiguity. **Playback pins the resolved version at start** — a
  mid-playback replace never mutates a running playback.
- **Transactional replace.** `macro.record;name=X&mode=replace` keeps the
  previous `X` intact until the new recording finalizes at `macro.stop`; a
  cancelled or limit-exceeded recording leaves the old version untouched.

## Reading macros back (OSC 778)

Two query ops project the macro state under the query channel's three-tier
read scope:

- **`state.macros`** — the caller's session macros plus the trusted macros,
  each tagged `scope` (`session`/`trusted`), with `name`, `v`, `commands`
  (captured count), `privileged`, and `hash` (the immutable id). Paginated.
- **`state.executions`** — the caller's *own* active recording or playback
  (executions are private per-agent, never projected to other callers):
  `kind` (`recording`/`playback`), `commands`, `privileged`,
  `scene_locked`, and for a playback `played`, `instant`, and `rate`.

## Limits

Advertised in `caps().limits`:

| Key | Meaning |
| --- | --- |
| `macros_per_namespace` | max stored session macros per agent |
| `macro_name_bytes` | max macro-name length |
| `commands_per_macro` | max captured commands in one macro |
| `macro_recording_secs` | max recording wall-clock span |
| `macro_playback_per_frame` | max commands re-injected per frame |

A recording that would exceed `commands_per_macro` or `macro_recording_secs`
is *poisoned* and discarded at `macro.stop` (rejecting `too-large`), leaving
any prior macro of the same name intact.

## Authoring in Silk

Transmissions may define session-scoped macros through the ordinary
`macro.record … macro.stop` bracket, with no special powers — the macros die
with the session like any wire-defined macro. [Silk](silk.md) sugars this as
a `macro` block:

```json
{ "at": 1.0, "macro": { "name": "greet", "replace": false, "cast": [
    { "at": 1.0, "ai": { "flash": "#00ff00" } },
    { "at": 2.5, "sound": { "play": "chime" } }
] } }
```

It compiles to exactly the bracket — `macro.record` at the block time, the
enclosed choreography, then `macro.stop` after the last enclosed event — and
adds no new wire authority. The enclosed choreography executes **once** while
being recorded. A nested `macro` block (recursion) or an enclosed `reset`
(it would cancel the recording) is a compile error. Promotion to durable /
trusted storage remains an explicit human act.

## The closed loop

```text
# record a two-step greeting, then replay it at double speed
ratty-ai macro record --name greet
ratty-ai flash --color '#00ff00'
ratty-ai sound play chime
ratty-ai macro stop
ratty-ai macro play greet --rate 2.0

# read it back
ratty-ai state macros            # → [{ name: "greet", scope: "session", commands: 2, … }]
ratty-ai state executions        # → [] once the playback drains
```

## Native and wasm parity

The registry is in-memory on both targets and dies with the session, so a
browser session and a native session behave identically. Playback re-injects
into the same in-process command stream on both; there is no cross-thread
channel and no persistence layer for the wire to reach.
