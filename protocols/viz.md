# Ratty Visualization Protocol (OSC 777 viz family)

Where `object.*` commands place individual 3D assets over OSC 777 and
the [Ratty Query Protocol](query.md) (OSC 778) reads the scene back, the
`viz.*` family carries *data visualizations*: typed, versioned snapshots
of processes, filesystems, repositories, and network interfaces that the
terminal renders as anchored keyed grids on the same surface the
[Ratty Graphics Protocol](graphics.md) objects live on. Trusted
collectors in the `ratty-ai` CLI (`ps`, `fs`, `git`, `net`, and the
`kill` watcher) gather data locally under the invoking user's own
permissions and lower it onto the wire; the terminal only ever renders
what it is handed.

## Design goals

- **The wire never executes, reads, or enumerates.** A viz command is a
  pure render directive: it never causes the terminal to run a command,
  read a file, enumerate processes, or open a network resource. All
  gathering happens in the trusted CLI, before the bytes reach the
  terminal.
- **Typed, versioned kinds.** Every payload conforms to a registered
  schema, and the schema version is part of the kind name (`ps.v1`); an
  unknown version is an unknown kind. Unknown JSON *fields* are ignored
  so schemas can evolve additively.
- **Atomic upsert.** A same-kind `viz.set` on a live id replaces the
  snapshot wholesale — a watcher refresh is one atomic upsert, never a
  partial mutation.
- **Effects target domain keys, never entities.** A `viz.effect` names
  a pid, a path, a branch, an interface — stable semantic keys — and
  tolerates keys the current snapshot no longer carries.
- **Capture honesty.** Every snapshot must carry `capture` provenance
  (source + timestamp). Ratty never implies liveness it was not given;
  transmissions may ship synthetic payloads, and honesty sits with the
  emitter.
- **Bounded everything.** Payload bytes, item counts, label lengths,
  per-namespace visualization counts, and effect queues are all capped,
  and the caps are advertised in the 778 `caps` reply.

## Transport

`viz.*` rides the OSC 777 control channel, one command per sequence:

```text
ESC ] 777 ; ratty:viz.set    ; id=<u32>&kind=<kind>&data=<b64url-json>[&x=<col>&y=<row>][&cols=<n>&rows=<n>][&replace=true][&tok=<token>] BEL
ESC ] 777 ; ratty:viz.effect ; id=<u32>&key=<domain-key>&effect=<name>[&tok=<token>] BEL
ESC ] 777 ; ratty:viz.remove ; id=<u32>[&tok=<token>] BEL
```

- `id=` is a caller-chosen u32 in the AI-owned range (`>= 0x8000_0000`)
  whose namespace bits must match the caller's ingress namespace,
  exactly as for `object.*`; violations reject `not-owner`. Identity
  comes from ingress, never from the bytes.
- `data=` is unpadded base64url of a UTF-8 JSON document (the base64url
  alphabet never needs escaping); other values embed `;`, `&`, or `=`
  percent-encoded.
- `tok=` opts into a delivery ack over OSC 778, exactly as documented
  in the [query protocol](query.md#command-acks-osc-777-tok); commands
  without it stay fire-and-forget and report failures through the
  caller's `state.errors` ring.

Limits, enforced at decode and advertised in the 778 `caps` reply:

- A decoded payload is capped at **32 KiB** (`viz_payload_bytes`);
  larger rejects `too-large`. The 4/3 base64url expansion plus envelope
  is statically asserted to fit under the terminal's 64 KiB OSC
  watchdog, so every payload the decode limit admits can actually ride
  one sequence.
- One snapshot holds at most **256** keyed items (`viz_items`).
- Any label string inside a payload (names, paths, states, capture
  fields) and any `viz.effect` key is capped at **128 bytes**.
- A namespace holds at most **32** live visualizations
  (`viz_per_namespace`); a fresh id beyond that rejects
  `namespace-cap`.
- At most **16** effects queue per visualization; a full queue drops
  the oldest entry (newest wins — effects are ephemeral presentation,
  not state).

The payload, item, and label limits are part of the wire contract and
live in the shared std-only wire module compiled identically by the
terminal and the `ratty-ai` collectors, so the two can never drift.

## Ops

### 1. `viz.set` — publish or refresh a snapshot

Fields:

- `id`: caller-owned visualization id *(required)*
- `kind`: registered payload kind, e.g. `ps.v1` *(required)*
- `data`: unpadded base64url JSON conforming to the kind's schema
  *(required)*
- `x`, `y`: anchor column/row of the **top-left** cell; supplied
  together or not at all
- `cols`, `rows`: footprint extent in cells; defaults `24`×`8` when
  first placed
- `replace`: literal `true` allows replacing a live visualization of a
  *different* kind
- `tok`: optional ack token

Semantics:

- **Anchoring diverges from `object.add`:** `x`/`y` is the footprint's
  top-left cell, not its center — footprints are caller-sized, so a
  centered anchor would be ambiguous. One of `x`/`y` without the other
  rejects `bad-payload`.
- A brand-new visualization sent without `x`/`y` is registered but
  stays hidden until a later `viz.set` places it. `cols`/`rows` need an
  anchor (placed now or already live) and at least one cell each;
  otherwise `bad-payload`.
- A same-kind `viz.set` on a live caller-owned id is an **atomic
  upsert**: the snapshot is replaced wholesale and acked `ok=1`. An
  upsert without `x`/`y` keeps the existing anchor — a watcher refresh
  never moves or un-scrolls the view — while `cols`/`rows` alone may
  resize the live footprint.
- Changing the kind of a live id requires `replace=true`; without it
  the command rejects `kind-mismatch`.
- A fresh id claims a namespace slot and rejects `namespace-cap` at the
  32-visualization limit; upserts never count against the cap.
- Queued effects survive a snapshot refresh: they drain the same frame
  and tolerate keys the new snapshot no longer carries.

### 2. `viz.effect` — annotate a domain key

Effects are bounded, self-expiring animations on one keyed item of a
snapshot — presentation, never state. They exist so a watcher can
narrate an *observed outcome* (a process died, a signal was denied)
without republishing data it does not have.

Fields:

- `id`: target visualization id *(required)*
- `key`: the domain key inside the snapshot — a pid as a decimal
  string, a path, a branch name, an interface name *(required)*
- `effect`: registered effect name *(required)*
- `tok`: optional ack token

Registered effects (an unknown name rejects `bad-payload`):

| effect      | meaning                              | rendering                                              |
| ----------- | ------------------------------------ | ------------------------------------------------------ |
| `died`      | the keyed item was confirmed gone    | shrink + darken, then the child disappears until the next snapshot |
| `survived`  | the item survived a kill attempt     | decaying shake                                         |
| `denied`    | permission was denied                | red flash                                              |
| `missing`   | the item was already gone            | gray flash                                             |
| `timeout`   | the outcome went unobserved          | amber flash                                            |
| `highlight` | draw attention to the item           | brief swell                                            |

Semantics:

- An unknown viz id rejects `unknown-id`. A **known id whose snapshot
  lacks the key still acks `ok=1` and renders nothing** — a kill racing
  a snapshot refresh is not an error.
- Every animation expires on its own after 0.8 seconds, ending exactly
  on the item's rest pose and palette color; `died` removes the child
  instead, until a later snapshot carries the key again.
- Queueing an effect bumps the entry's revision, so a `viz.effect`
  followed by `state.viz` in the same chunk observes the queued effect
  (the pending-effect count drains to zero once rendered).

### 3. `viz.remove` — remove a visualization

Removes the record and its rendering; an id with no live visualization
rejects `unknown-id`.

**The id is immediately reusable** — a deliberate divergence from
`object.*`'s never-reuse ledger, because watchers restart under stable,
documented ids. The per-namespace cap is the registry's only bound.

### Reset

`ratty:reset` clears every visualization along with the rest of the
scene. It is not a viz op and sends no per-visualization acks — the
reset's single ack covers it.

## Anchors and scroll

Viz anchors live in terminal cell space and shift with terminal scroll
exactly like inline object anchors: rows shift up as content scrolls,
and an anchor scrolled fully off the top is dropped while the snapshot
is kept — the visualization hides until a later placing `viz.set`
re-anchors it.

## Rendering model

The v1 render vocabulary is deliberately small: every kind lowers onto
a keyed grid of magnitude bars inside the anchored footprint — one
small mesh per item, in every presentation mode. Bar heights are the
snapshot's normalized magnitudes (cpu for `ps`, log-scaled size for
`fs`, log-scaled rx+tx for `net`; `git` weights the checked-out branch
over the rest), and colors come from a small semantic palette (active /
idle / alert / container / neutral). Magnitudes normalize *within* the
snapshot: the tallest bar is the snapshot's largest item, not an
absolute unit. M3.6 grows real chart kinds on this same substrate.

## Payload kinds (v1)

Rules shared by all kinds: `capture` is **required**; unknown JSON
fields are ignored (additive evolution); identity fields (keys, names)
are required while magnitude fields default; every size limit is
hard-rejected with `too-large`.

**Domain keys are unique within a snapshot.** A well-formed emitter
never repeats a key (pid, path, branch, interface). If a snapshot does
repeat one, the renderer is **first-occurrence-wins**: the first item
with a given key renders and later items sharing it are dropped, so the
entity tree and its key ledger can never diverge. The `state.viz`
`item_count` is the **raw** payload item count, so a snapshot carrying
duplicates reports the pre-dedup number — the read-back never hides
that the wire carried repeats.

```json
"capture": { "source": "ratty-ai ps/sysinfo macos; top 5 of 732 by cpu",
             "ts": "2026-07-22T17:03:11Z" }
```

`source` declares where the data came from — including any truncation
or gathering caveats — and `ts` is the capture timestamp (RFC 3339
recommended; opaque on the wire). Both are bounded at 128 bytes.

### `ps.v1` — processes

```json
{ "capture": { "source": "…", "ts": "…" },
  "items": [ { "pid": 1234, "name": "ratd", "cpu": 12.5,
               "mem": 104857600, "state": "running" } ] }
```

Domain key: the pid as a decimal string. `pid` and `name` are required;
`cpu` (percent), `mem` (bytes), and `state` default. Bars scale by cpu;
`state` picks the palette slot (`run…` active; `zombie`/`stop…`/`dead…`
alert; empty neutral; anything else idle).

### `fs.v1` — filesystem walk

```json
{ "capture": { "source": "…", "ts": "…" }, "root": "/Users/rat/src",
  "items": [ { "path": "target", "kind": "dir", "size": 0, "depth": 1 } ] }
```

Domain key: the path. `root`, each `path`, and each `kind` (`file` |
`dir`) are required; `size` (bytes) and `depth` default. Bars scale by
log-scaled size; directories color as containers.

### `git.v1` — repository

```json
{ "capture": { "source": "…", "ts": "…" }, "repo": "ratty",
  "branches": [ { "name": "main", "current": true }, { "name": "dev" } ],
  "status": { "staged": 1, "unstaged": 2, "untracked": 0 },
  "ahead": 0, "behind": 3 }
```

Domain key: the branch name. `repo` and each branch `name` are
required; `current`, the `status` counts, `ahead`, and `behind`
default. The item count is the branch count; the checked-out branch
renders tallest and active.

### `net.v1` — interface counters

```json
{ "capture": { "source": "…", "ts": "…" },
  "items": [ { "iface": "en0", "rx_bytes": 123456789,
               "tx_bytes": 9876543, "up": true } ] }
```

Interfaces, not sockets — a portable, honest v1; per-connection detail
can arrive additively as a future kind. Domain key: the interface name.
`iface` and `up` are required — a defaulted link state would claim
knowledge the emitter never sent — while `rx_bytes`/`tx_bytes` default.
Bars scale by log-scaled rx+tx; down interfaces color as alerts.

## Collectors (`ratty-ai`)

The trusted side of the family: four snapshot collectors and the `kill`
watcher, all gathering locally under the invoking user's own
permissions and honoring `--dry-run` / `--ack` / `--json` / `--tty`
like every other `ratty-ai` command.

```sh
ratty-ai ps  [--id N] [--top 32] [--watch <secs>] [-x <col> -y <row>] [--cols N] [--rows N]
ratty-ai fs  [PATH] [--depth 3] [--top 64] [--id N] [--watch <secs>] [-x … -y …]
ratty-ai git [--repo PATH] [--id N] [--watch <secs>] [-x … -y …]
ratty-ai net [--id N] [--top 64] [--watch <secs>] [-x … -y …]
ratty-ai kill <pid> [--sigkill] [--timeout-ms 5000] [--id N]
```

- **Stable default slots.** Each collector defaults to a fixed id in
  namespace 0 of the AI partition, so bare invocations upsert a stable
  slot: `ps` 2147483904 (`0x8000_0100`), `fs` 2147483905
  (`0x8000_0101`), `git` 2147483906 (`0x8000_0102`), `net` 2147483907
  (`0x8000_0103`). `--id` overrides.
- **`--top` is hard-capped at 64** on every collector: a hard cap
  chosen with ample provable headroom — a snapshot of 64 worst-case
  labels stays under the 32 KiB payload limit for all four kinds (pinned
  by test), not the largest such N.
- **`--watch <secs>` (min 1)** republishes fresh snapshots under the
  same id until interrupted. Only the first emission sends the anchor;
  refreshes are upserts that keep the live anchor, so a view the user
  scrolled is never snapped back.
- **Provenance is machine-visible.** Truncation and gathering caveats
  land in `capture.source`: `ratty-ai ps/sysinfo macos; top 32 of 732
  by cpu` · `ratty-ai fs/walk; top 64 of 4096 by size; 2 unreadable
  skipped; walk capped at 4096; 3 paths truncated` · `ratty-ai
  net/sysinfo linux; up=IFF_UP` (or `up=has-address` where the IFF_UP
  link state is unavailable). A path longer than the 128-byte label
  bound is truncated to a hash-disambiguated key (so distinct
  over-long paths never collapse to one domain key), and the count of
  such paths is declared here.
- The `fs` walk is bounded: breadth-first, depth-limited (direct
  children are depth 1), capped at 4096 entries, never follows
  symlinks, skips-but-counts unreadable directories, and records
  directory sizes honestly as 0 (unmeasured). `git` shells out to the
  `git` binary; a missing repo or binary exits 2.

### `kill` — the closed loop

`ratty-ai kill` signals a process, watches the outcome, and reports it
honestly as a `viz.effect` on the ps visualization's pid key. Identity
is pinned to (pid, start time) before signaling and re-verified while
watching, so PID reuse can never claim a death that did not happen.
SIGTERM by default, SIGKILL only with `--sigkill`; no confirmation
prompt — the invoking user already holds `/bin/kill` authority.

**The wire never carries a kill verb.** The only bytes emitted are the
observed outcome:

| outcome                                                            | effect     | exit code |
| ------------------------------------------------------------------ | ---------- | --------- |
| confirmed exit — a zombie counts (it exited), as does a reused pid (the original is gone) | `died` | 0 |
| alive with the same identity when the SIGTERM watch ended          | `survived` | 10        |
| the signal was refused (EPERM)                                     | `denied`   | 11        |
| no such process when signaling                                     | `missing`  | 12        |
| still listed past the SIGKILL deadline — outcome unobserved        | `timeout`  | 13        |

The 10+ range can never collide with the transport exit codes (`2`–`6`)
that `--ack` may produce, and a `viz.effect` delivery failure only
overrides a would-be-`0` exit — `0` never lies about the process or
about the delivery.

## Read-back (`state.viz`)

The 778 op `state.viz` (see the [query protocol](query.md)) projects
visualization records under the standard three-tier read scope: the
caller's own records in full — public fields plus `capture` provenance
and the pending-effect count — while foreign visualizations appear only
while visible, as public projections; a hidden foreign visualization's
existence is not readable. Payload read-back is deliberately
summary-level in v1: `item_count`, never item dumps or raw payloads.
The `caps` reply advertises `viz_per_namespace`, `viz_payload_bytes`,
and `viz_items`.

## Error codes

Shared with the query channel's append-only, kebab-case registry;
carried in the rejection ack's `code=` and in `state.errors`:

- `not-owner` — the id lies outside the caller's AI range/namespace
- `bad-kind` — unregistered payload kind (unknown versions included)
- `kind-mismatch` — the live id holds a different kind and
  `replace=true` was absent
- `bad-payload` — malformed base64url, schema-violating JSON, unpaired
  `x=`/`y=`, a footprint without an anchor, a zero-cell footprint, or
  an unregistered effect name
- `too-large` — a payload, item-count, label, or effect-key limit was
  exceeded
- `namespace-cap` — the namespace is at its 32-visualization limit
- `unknown-id` — `viz.effect`/`viz.remove` named an id with no live
  visualization

## Example session

The ps → kill closed loop, as it looks on the wire:

```text
# `ratty-ai ps --top 32 -x 10 -y 5 --ack`: publish a process snapshot
agent:  ESC ] 777 ; ratty:viz.set ; id=2147483904&kind=ps.v1&data=<b64url {ps.v1 …}>&x=10&y=5&tok=v1 BEL
ratty:  ESC ] 778 ; v=1 ; t=r ; id=v1 ; kind=ack ; ok=1 ESC \

# read it back
agent:  ESC ] 778 ; v=1 ; t=q ; id=q1 ; op=state.viz ESC \
ratty:  ESC ] 778 ; v=1 ; t=r ; id=q1 ; ok=1 ; data=<b64url {"items":[{"id":2147483904,
        "owner":0,"kind":"ps.v1","revision":12,"visible":true,
        "anchor":{"row":5,"col":10,"cols":24,"rows":8},"item_count":32,
        "capture":{"source":"ratty-ai ps/sysinfo macos; top 32 of 732 by cpu","ts":"…"},
        "pending_effects":0}]}> ESC \

# `ratty-ai kill 1234` observed the exit; the wire carries only the outcome
agent:  ESC ] 777 ; ratty:viz.effect ; id=2147483904&key=1234&effect=died&tok=k1 BEL
ratty:  ESC ] 778 ; v=1 ; t=r ; id=k1 ; kind=ack ; ok=1 ESC \

# the 1234 bar shrinks away; the next watch refresh no longer carries it
agent:  ESC ] 777 ; ratty:viz.set ; id=2147483904&kind=ps.v1&data=<b64url {refresh …}> BEL

# tear down
agent:  ESC ] 777 ; ratty:viz.remove ; id=2147483904&tok=r1 BEL
ratty:  ESC ] 778 ; v=1 ; t=r ; id=r1 ; kind=ack ; ok=1 ESC \
```

At the CLI, the whole loop is two commands:

```sh
ratty-ai ps --top 32 -x 10 -y 5 --watch 2 &
ratty-ai kill 1234        # SIGTERM, watch, report the observed outcome
```

## Summary

The viz family turns the terminal into a live telemetry surface without
ever making it a sensor: trusted collectors gather under the user's own
authority, typed versioned snapshots ride the wire with honest capture
provenance, the terminal renders keyed magnitude grids anchored in cell
space, and bounded self-expiring effects narrate observed outcomes on
stable domain keys. `viz.set` publishes, `viz.effect` annotates,
`viz.remove` tears down, and `state.viz` reads it all back over OSC
778 — the same closed loop the object family established, now for data.
