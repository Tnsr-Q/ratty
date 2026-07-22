# Ratty Query Protocol (OSC 778)

Where OSC 777 lets an agent *write* the scene (commands, fire-and-forget)
and the [Ratty Graphics Protocol](graphics.md) carries the 3D payload,
OSC 778 is the *read* side: a query/reply channel that returns live
terminal state as structured JSON, plus the return path for opt-in
command acknowledgements. Together they close the loop the ecosystem
vision describes — an agent writes the scene over 777 and reads it back
over 778, so embodied coordination (orbit, avoid, approach) is possible
at all.

The canonical envelope codec lives in `src/query.rs` (std-only), shared
verbatim by the terminal, the `ratty-ai` CLI, and the wasm build — the
implementations cannot drift.

## Design goals

- **No escaping questions, ever.** Envelope fields are strict ASCII
  metadata; every structured or user-controlled value rides inside an
  unpadded base64url JSON payload.
- **Identity from ingress, never in-band.** A query cannot name its own
  agent or session; authority derives from the transport the bytes
  arrived through, exactly as for OSC 777 commands.
- **Observation is not control.** Reading another agent's public
  projection confers no authority to mutate it.
- **Bounded everything.** Query size, decoded payload size, reply size,
  and diagnostics retention are all capped; large collections paginate
  with opaque cursors.
- **Honest answers only.** Ops whose subsystem does not exist yet
  (macros, executions) answer empty or `unsupported` — never fabricated.

## Transport

Both directions are single OSC sequences, ST-terminated:

```text
query:  ESC ] 778 ; v=1 ; t=q ; id=<token> ; op=<op> ; data=<b64url-json> ESC \
reply:  ESC ] 778 ; v=1 ; t=r ; id=<token> [; kind=ack] ; ok=1|0 [; code=<error>] [; data=<b64url-json>] ESC \
```

- `id=<token>` — client-generated correlation token: 1–64 chars of the
  base64url alphabet (`A–Z a–z 0–9 - _`). The CLI uses 128-bit random hex.
- `data=` — optional unpadded base64url of a UTF-8 JSON document.
- Correlation is by (session, agent, token); the session is the transport
  (one PTY = one session), the agent is the ingress namespace, and the
  reply goes **only** to the originating transport. Clients match on the
  token.
- Replies are size-bounded (≤ 4 KiB framed); queries are refused above
  8 KiB (`too-large`), decoded payloads above 4 KiB.
- `t=e` is **reserved**: no unsolicited events in v1. A future
  capability-scoped subscription protocol will claim it.
- Unknown envelope keys are ignored (additive evolution); a breaking
  change bumps `v=`.

## Command acks (OSC 777 `tok=`)

Any OSC 777 command may opt into a delivery ack by adding a `tok=<token>`
payload key:

```text
client sends:  ESC ] 777 ; ratty:object.add ; id=2147483649&path=rat.obj&tok=a1b2 BEL
ratty replies: ESC ] 778 ; v=1 ; t=r ; id=a1b2 ; kind=ack ; ok=1 ESC \
```

The ack fires once, after the command is rejected (`ok=0` with the
rejection's `code=`) or its immediate state mutation commits (`ok=1`).
Commands without `tok=` stay fire-and-forget; their failures land in the
caller's `state.errors` ring instead of the input stream of programs that
did not ask. A `tok=`-carrying sequence that fails to parse still gets
its error ack (`code=bad-command`).

> The locked design named this key `id=`, but `id=` is already the
> required object id on every `object.*` command — the wire cannot carry
> both meanings, so the ack key is `tok=`.

Correlation tokens are transport metadata: the future macro recorder
(M3.7) records canonical commands and must never capture them.

## Read scope

An agent may read three tiers, and only three:

1. **Scene-global public state** — mode, camera, grid, warp, public
   effects, cursor presentation, protocol capabilities.
2. **Its own namespace in full** — complete object records (including
   scrolled-away objects and private style fields) and its own error
   diagnostics.
3. **Other agents' public render projections** — the minimal structured
   facts of what is visibly on screen: public object id, owner
   namespace, kind, anchor cell, transform/offset, scale, rotation/spin,
   brightness/visibility, bounds, current revision.

Never readable across namespaces: colors and private style fields, asset
source details and provenance, hidden or unspawned objects, diagnostics,
command history, capability grants. Projections are derived from ECS
state — internal registries and Bevy `Entity` values are never exposed.
Nothing in the read path touches the filesystem.

## Ops (v1)

Every op replies `ok=1` with a JSON payload, or `ok=0` with a `code=`.
Paginated ops accept `{"cursor": "<opaque>"}` in `data=` and return a
`cursor` field while more items remain. Cursors are bound to the session
nonce: a cursor from another process fails with `bad-cursor`.

### 1. `caps` — discovery

The 778 analog of the RGP support reply; keys are append-only.

```json
{ "v": 1, "session": "9f2c4e0d1a6b8c3e", "ops": ["caps", "state.scene", …],
  "ack": { "key": "tok" },
  "limits": { "max_query_bytes": 8192, "max_query_data_bytes": 4096,
              "max_reply_bytes": 4096, "objects_per_namespace": 64,
              "ids_per_session": 4096, "errors_per_namespace": 32 } }
```

### 2. `state.scene` — scene-global public state

Mode, warp, camera view (yaw/pitch/zoom/offset — drag internals are
private), grid size, tween activity, cursor presentation, public effects
(thinking/confidence/mood/flash/pulse/tint).

### 3. `state.objects` — the caller's complete records *(paginated)*

Every object in the caller's namespace, anchored or not, with full style
fields and per-object `revision`. Sorted by id.

### 4. `state.visible_objects` — public projections *(paginated)*

The public projection of everything visibly on screen — every namespace
plus transmission-owned objects (`owner: null`). Visibility is the
renderer's own rule: an anchored object whose rows intersect the grid.

### 5. `state.neighbors` — projections within a radius *(paginated)*

```json
{ "object": 2147483649, "radius": 8 }         // around one of the visible objects
{ "center": { "row": 10, "col": 40 }, "radius": 8 }
```

Distance is Euclidean between anchor centers, in cells; each item
carries `distance`. Items stay id-sorted for stable pagination — sort by
`distance` client-side for rank order. The center object itself is
excluded. An object center must be caller-owned or currently visible:
a hidden foreign id answers a flat `unknown-id` (its existence is not
readable), while the caller's own scrolled-away object answers
`no-anchor`.

### 6. `state.namespaces` — aggregate presence

Live object counts per agent namespace plus the transmission partition.

### 7. `state.macros` / `state.executions` — honestly empty

`{"items": []}` until the macro subsystem (M3.7) lands. Acked `macro.*`
commands reply `ok=0; code=unsupported` today.

### 8. `state.errors` — the caller's rejection ring *(paginated)*

The last 32 rejections in the caller's namespace: `seq`, `action`,
`code`, `message` — the query-channel return path for every failure that
used to be only a terminal-side `warn!`.

## Error codes

Append-only, kebab-case, carried in `code=`: `bad-envelope`,
`bad-version`, `too-large`, `bad-payload`, `unsupported-op`,
`unsupported`, `bad-command`, `bad-cursor`, `not-owner`, `unknown-id`,
`no-anchor`, `already-exists`, `id-reused`, `session-budget`,
`namespace-cap`, `bad-asset`, `bad-mode`, `internal` — plus the
client-side `timeout` and `disposed`.

## Client surfaces

**Native CLI** — one generic verb plus sugar; new ops never grow new
subcommands:

```sh
ratty-ai query <op> [--data <json>] [--data-file <path|->]
                    [--timeout <ms>] [--json] [--pretty] [--tty <path>]
ratty-ai state [path]        # lowers to `query state.<path>`; bare = scene
ratty-ai --ack <command …>   # opt into the command ack and wait for it
```

The CLI opens the controlling (or `--tty`) terminal raw, emits the query
there, and reads until the token-matched reply or the timeout, ignoring
unrelated bytes and unmatched replies; terminal state is restored on
every exit path including signals. Exit codes are stable: `0` success ·
`2` bad arguments/input JSON · `3` timeout · `4` malformed reply · `5`
the terminal returned `ok=0` · `6` tty/transport failure. `--json` emits
`{"ok":false,"code","message"}` on failures.

**Wasm** — a thin convenience over the same Rust parser (no second
protocol implementation in JavaScript):

```js
const caps = await session.query("caps", null, 2000);
const near = await session.query("state.neighbors", { object: id, radius: 8 }, 2000);
```

Failures reject with an `Error` carrying a `code` property. Locked
pending-map semantics: duplicate active token is an internal error;
disposal rejects all pending promises; timeout removes and rejects; late
or unmatched replies are ignored. `query()` adds no authority — session
identity, namespace, projection rules, and size limits all still apply.

## Example session

```text
agent:  ESC ] 778 ; v=1 ; t=q ; id=q1 ; op=caps ESC \
ratty:  ESC ] 778 ; v=1 ; t=r ; id=q1 ; ok=1 ; data=<b64url {caps…}> ESC \
agent:  ESC ] 777 ; ratty:object.add ; id=2147483649&path=rat.obj&x=20&y=8&tok=a1 BEL
ratty:  ESC ] 778 ; v=1 ; t=r ; id=a1 ; kind=ack ; ok=1 ESC \
agent:  ESC ] 778 ; v=1 ; t=q ; id=q2 ; op=state.neighbors ; data=<b64url {"object":2147483649,"radius":8}> ESC \
ratty:  ESC ] 778 ; v=1 ; t=r ; id=q2 ; ok=1 ; data=<b64url {items…}> ESC \
```

## Summary

OSC 778 gives agents eyes to match 777's hands: acked writes, structured
reads of the scene and of each other's public projections, bounded
replies with opaque cursors, and one shared Rust codec across terminal,
CLI, and browser. `caps` is the front door; everything else is additive.
