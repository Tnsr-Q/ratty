# Ratty Terminals Protocol

The terminals organ (#49, M4.5): **terminals on the wire** — creating,
resizing, focusing and closing terminal seats over OSC 777. This is the
wire onto the per-terminal spawner M4.3 built, and the replacement for the
frozen `pane.*` family (see [Superseded surface](#superseded-surface)). The
grammar rules (percent-encoded `k=v&…` payloads, `tok=` acks) are shared
with every family in [query.md](query.md).

**The trust boundary**: two capabilities, **both default DENY**, plus
creator-scoped ownership layered inside them — a caller needs the grant
*and* must have created the terminal it names by handle.

- `[trust.local] terminal_lifecycle` covers `term.spawn` / `term.place` /
  `term.close`. Unlike `avatar_scene`, it does not default granted: these
  verbs fork processes and destroy live sessions, and one PTY is one
  principal, so a grant reaches every process writing that terminal.
- `[trust.local] terminal_focus` covers `term.focus` alone (#56 decision
  18). Focus is the **keystroke-capture primitive**, not a convenience
  verb: granting it means the wire may redirect where the human's typing
  lands, **continuously** — not once, and not reversibly by the grant. It
  is rate-limited, never one-shot. Split from lifecycle so an operator can
  grant workspace choreography without granting keystroke redirection, or
  the reverse.
- **Terminal #1 is wire-unkillable by construction**, and so is every
  user-spawned seat and every orphan: close authority is creator-scoped,
  and those rows have no creator. This is not a special case in the code —
  it is the same clause.

The wire can never choose a command, a working directory, an environment,
or a filesystem path. `RuntimeOptions` does not appear in this family's
enum at all (#12): a spawned terminal runs the config-default shell,
because nothing in the command could say otherwise.

## Design goals

- **Placement, not splits.** A terminal is a grid, not a cell in someone
  else's grid. There is no `direction=`, no `ratio=`, and no geometry
  derived from another terminal's quad (the #22 ruling).
- **Lifecycle is ingress truth.** A replayed `term.spawn` would fork a
  process; a replayed `term.close` would kill a session someone is typing
  into. The whole family is control-plane: never recorded, never
  replayable, never fireable by a rule.
- **Handles are references, never authority.** Knowing a handle grants
  nothing. `state.terminals` publishes every row's id scene-scoped
  precisely because visibility grants observation, not control (#18).
- **Refuse rather than pretend.** Where the wire shape carries a field
  this build cannot honor, the applier rejects it with `unsupported` and
  `caps` says so up front. An `ok=1` on something that did not happen is
  worse than a refusal, and refusals widen compatibly.
- **Bounded by construction.** A live cap, a 128-slot pool, a grid
  ceiling, and per-terminal rate budgets — each with an explicit code.

## Command family (OSC 777 `ratty:`)

```text
ESC ] 777 ; ratty:term.spawn [;tok=…] BEL
ESC ] 777 ; ratty:term.place ;[id=<handle>&][cols=<u16>&][rows=<u16>&][tok=…] BEL
ESC ] 777 ; ratty:term.focus ;[id=<handle>&][tok=…] BEL
ESC ] 777 ; ratty:term.close ;[id=<handle>&][tok=…] BEL
```

`term.place` additionally *parses* `x=`, `y=` and `scale=` (f32), and
`term.spawn` parses all five of `x`/`y`/`scale`/`cols`/`rows`. Both
**reject** those keys — see [Honest limitations](#honest-limitations).
They stay in the wire shape because the shape is frozen; they are parsed so
a caller gets an explicit refusal instead of a silent discard.

Rules, each from a shipped precedent:

- **`id=` is the handle key, never `term=`.** `term=` is the recorded
  envelope-key escape for a future conductor model; a family squatting it
  would make its later addition a silent reinterpretation of shipped bytes.
- **Handles validate at apply, not parse.** An unknown, foreign-session or
  dead handle answers `unknown-id` at apply time.
- **Absent `id=` targets the carrying terminal** (arrival is the address).
  A present-but-**empty** `id=` is a handle nobody minted — deliberately
  *not* the bare form, so a typo cannot silently retarget the caller's own
  seat.
- **Numerics are strict.** A present-but-malformed number fails the whole
  parse and answers `bad-command` (the envelope still acks, because `tok=`
  is extracted before command parse).
- **Partial update is atomic.** One invalid field on `term.place` rejects
  the whole command; an all-absent `term.place` is a vacuous commit, acked
  ok.

## Semantics

### Handles

A handle is `<session-nonce-hex>-<seq>`, minted from the same app-global
counter as execution handles, so no two live handles of any family collide.
Handles are never reused. A handle from a previous process fails to
resolve and answers `unknown-id` — explicit staleness, never a silent
mis-target.

Every terminal has exactly one handle, including the boot terminal and
chord-spawned ones: `state.terminals` enumerates the whole world rather
than lying by omission.

### Creator scope

A terminal's row records the `TerminalId` of the terminal whose ingress
asked for it — never its namespace, because namespaces recycle and a
recycled slot's next tenant would inherit the relationship (#56 decision
17, the stamp rule). On the wire that field renders as the creator's
*current* namespace ordinal, resolved at read time.

- Handle-carrying `term.place` / `term.focus` / `term.close` require
  creator match.
- Bare forms target the arrival terminal and need only the capability —
  except `term.close`, which refuses creator-less rows even from their own
  ingress (that is the wire-unkillable clause).
- **Orphans**: a creator's death does **not** cascade-close its children —
  they are principals in their own right, possibly with a user inside.
  Their `creator` is cleared instead, which makes them wire-unaddressable
  under the same clause that protects terminal #1.

### Lifecycle

```text
spawning ──▶ ready ──▶ closing ──▶ (gone)
```

`state` on a `state.terminals` row is the readiness signal. `term.spawn`
is **not** a long-running operation and never acks `code=started`: under
[query.md](query.md)'s rule that absence from `state.executions` means
*finished*, a started ack on a handle deliberately kept out of that roster
would tell a conforming caller the spawn had completed while it was still
spawning (#56 decision 19).

`spawning` means the row exists and its seat entity has not flushed into
the world yet — a real but very short window, derived from the world rather
than stored. Closing a still-`spawning` terminal is legal and cancels the
spawn; there is no `term.cancel`.

### Bounds

| Bound | Default | Where |
| --- | --- | --- |
| Live terminals | 4 | `[terminal] max_live`, clamped to `1..=128` |
| Namespace pool | 128 | Protocol ceiling — object ids carry the namespace in seven bits |
| Grid per axis | `2..=512` | vt100 underflows below two; above, one command allocates an unbounded image |
| Grid area | 100 000 cells | The per-axis ceiling alone still admits 262 144 |
| Spawns | 1/s, burst 4 | Per arrival terminal |
| Focus moves | 4/s, burst 8 | Per arrival terminal |

The live cap binds **every** spawn path — the `Ctrl+Alt+T` chord and the
wire alike — because it is checked at the single allocation site. Raising
it past 4 waits on a shared font context: every live terminal carries its
own font stack and CPU-side texture today.

The rate budgets exist because the cap bounds *concurrency*, not *rate*:
closes are deferred a frame and one PTY chunk can carry arbitrarily many
commands, so a spawn/close cycle would otherwise fork processes at frame
rate while never exceeding the cap.

## Errors

| Code | When |
| --- | --- |
| `bad-command` | Parse failure (a malformed strict numeric), or a grid outside `2..=512` per axis or over 100 000 cells |
| `not-permitted` | The capability is not granted, or the command did not arrive as live wire ingress |
| `not-owner` | A handle naming a terminal the caller did not create, or any close aimed at a terminal with no wire creator |
| `unknown-id` | A dead, foreign-session, never-minted or empty handle; or a target that is still `spawning` |
| `terminal-cap` | No room for another terminal: the configured live cap or the 128-slot pool |
| `rate-limited` | The per-terminal spawn or focus budget is exhausted |
| `unsupported` | Any field on `term.spawn`; `x`/`y`/`scale` on `term.place`; `term.spawn` on wasm |
| `internal` | The transport or surface failed to construct after admission |

`terminal-cap` covers both the configured cap and the protocol pool
deliberately: the caller's remedy is identical — close something — and
with a default of 4 the pool wall is unreachable through the wire. The
operator distinction survives in the log, where the two are separate error
values.

## Reading back (OSC 778)

`state.terminals` lists every live terminal, paginated, in mint order:

```json
{ "id": "<handle>", "state": "spawning|ready|closing",
  "ns": 3, "creator": 0,
  "cols": 104, "rows": 32,
  "x": 0.0, "y": 0.0, "scale": 1.0 }
```

Rows are tier-1 scene-global public state: the quads are visibly on screen,
so enumerating them observes nothing a viewer cannot already see.

**`creator` is own-scoped** (#56 decision 15). It appears only when the
querier IS the creator; for everyone else the **key is absent**, never
`null` — a null would itself be a distinguishable "someone owns this"
marker. Its value is the creator's namespace ordinal, and it appears under
no other key, because a namespace is a stable enumerable address and a
second spelling would defeat the scoping.

`caps` gains a `terminals` object:

```json
"terminals": { "live": 2, "max": 4, "pool": 128,
               "verbs": ["spawn", "place", "focus", "close"],
               "spawn_fields": [], "place_fields": ["cols", "rows"] }
```

`spawn_fields` and `place_fields` are the honesty contract: they name
exactly which payload keys the appliers act on, so a caller learns the
refusals from discovery rather than from an ack. `caps.trust` carries
`terminal_lifecycle` and `terminal_focus` so a caller can read its grant
before attempting a verb. `caps.limits` carries the rate and grid ceilings.

**`caps.panes` stays `1`.** Terminals are not panes — that is the whole #22
ruling — and the #57 pane-0 contract holds until #86 ships, no matter how
many terminals are live.

## Classification

Every `term.*` command is:

- **Control-plane** — the macro recorder never captures one, and the relay
  gate excises it before a spectator sees a byte.
- **Wire-origin-only** — the applier refuses macro, bookmark and rule
  origins outright, belt-and-suspenders beside the structural closure.
- **Refused by the trusted macro loader** — the one lever that survives a
  hand-authored trusted macro, since that path copies `privileged` from the
  caller verbatim.
- **Not rule-safe** — the rule allowlist is closed, so a reactive rule can
  never fire terminal lifecycle, directly or through a macro.
- **Scene-global** — belt-and-suspenders only, since nothing recordable can
  carry the family; kept so a future injection path inherits the right
  class by default.

**Deliberately sacrificed in v1: workspace macros.** A trusted macro that
opens a three-terminal layout is a real want, and it still cannot work
under wire-origin-only. The relaxation path is narrow and additive — admit
`term.spawn` into *trusted* macros only, keeping session recordings
excluded — and belongs to a future ticket.

## CLI

```bash
ratty-ai --ack --json term spawn          # data.id is the new handle
ratty-ai term place --id "$H" --cols 80 --rows 24
ratty-ai term focus --id "$H"
ratty-ai term close --id "$H"
```

`--ack` opts into the reply; exit code 5 means the terminal answered
`ok=0`, and it is the only code that proves a rejection (3 is a transport
timeout, never a verdict).

## Rendering

A terminal's quad is drawn by the shared stage. The **focused** terminal
renders 1:1; unfocused seats keep their textures live but are not
composed as independent quads in this build. Cell picking in 3D is M4.6.

## Native and wasm parity

`term.place`, `term.focus` and `term.close` behave identically on both
targets. **`term.spawn` refuses on wasm** with `unsupported`: terminal
lifecycle on the web belongs to the page API (#53's canvas, #86's fork),
and the PTY constructor does not exist on that target at all.

## Honest limitations

- **Placement geometry is not rendered.** `x`/`y`/`scale` are refused on
  both `term.place` and `term.spawn`. Nothing in this build lowers them:
  the focused terminal draws centered and 1:1, every layout pass rewrites
  viewport centres to the origin, the flat present path is a fullscreen
  triangle with no placement uniform, and mouse hit-testing hardcodes
  centering — so a "moved" terminal's clicks would land on the wrong cells.
  N-plane 3D composition is the #42 map's return, not this milestone's.
- **`term.spawn` takes no grid.** A new seat is sized from the window
  unconditionally by the dressing path, so an accepted `cols=` would be an
  `ok=1` on a grid that never happened. Spawn, wait for `state=ready`, then
  `term.place;cols=&rows=`.
- **A wire-set grid is advisory.** The next window resize or focused
  font-size step recomputes it from the window. There is no pin flag; what
  a pinned terminal should do when the window shrinks below its grid is a
  scene-composition question, not a wire one.
- **`spawning` is a one-frame state**, not an admission queue.
- **A native self-close ack is best-effort.** The ack is written into the
  dying terminal's PTY master before the despawn, but the child may be
  killed before it reads. On wasm there is no race — the reply resolves
  in-process. Deferring one system guarantees the write, not the read.
- **A handle is a name, not a secret.** Handles embed the session nonce,
  which `caps` publishes, and `state.terminals` publishes every row's id
  scene-scoped anyway. Ownership is enforced by the creator check, never by
  handle secrecy.
- **One PTY is one principal.** A grant reaches every process writing that
  terminal. Ratty cannot split it finer and does not pretend to.

## Superseded surface

`pane.split` / `pane.focus` / `pane.resize` / `pane.close` are **frozen and
permanently `unsupported`** (#22). Their wire shapes stay exactly as
committed and their parse arms remain, because deleting them would *change*
shipped behavior — downgrading a clean `unsupported` to the unknown-action
`bad-command` path.

```text
ESC ] 777 ; ratty:pane.split  ; direction=<vertical|horizontal>&ratio=<f32>  → unsupported
ESC ] 777 ; ratty:pane.focus  ; pane=<u8>                                    → unsupported
ESC ] 777 ; ratty:pane.resize ; pane=<u8>[&width=<u16>][&height=<u16>]       → unsupported
ESC ] 777 ; ratty:pane.close  ; pane=<u8>                                    → unsupported
```

They are **not** re-lowered onto `term.*`, and never will be. The mapping
cannot even be written: `pane.split` carries no pane id at all, and the
other three carry a raw `u8` bound to nothing, so mapping them onto handles
would mint a third identifier space — positional, racy, renumbering on
close. Author intent does not survive the translation either: a script
emitting `pane.split;ratio=0.3` wants a tmux split of the current viewport,
while `term.spawn` creates an independent placed principal. Guessing is
worse than rejecting.

Note the vocabulary difference: `term.place` uses `cols=`/`rows=`, the viz
footprint convention, deliberately *not* the pane family's
`width=`/`height=`.

See also: [query.md](query.md) (envelope, acks, error codes),
[presence.md](presence.md) (the other ingress-truth family),
[macros.md](macros.md) (why this family is never recorded).
