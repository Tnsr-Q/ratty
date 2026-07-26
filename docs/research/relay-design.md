# Relay design: the tier-B `ratty-relay`

Research asset resolving [wayfinder ticket #45](https://github.com/Tnsr-Q/ratty/issues/45)
(map [#42](https://github.com/Tnsr-Q/ratty/issues/42)), designed entirely
inside the tier-B lock from [#25](https://github.com/Tnsr-Q/ratty/issues/25)
(final comment, "Resolution — Collaboration organ design (locked)", grilled
2026-07-14), quoted verbatim because every sentence is a constraint:

> Named transport experiment (B): a `ratty-relay` daemon (in `tools/`,
> outside the crate) that multicasts one session's **public snapshot and
> subsequent updates** to N read-only spectators — native viewers or the
> web widget via `feed()`. It accepts **no viewer commands**.
> Upstream-clean; built when a demo needs to be real.

**Recommendation only.** The walking skeleton is
[#46](https://github.com/Tnsr-Q/ratty/issues/46); the demo is
[#47](https://github.com/Tnsr-Q/ratty/issues/47). Nothing here changes
ratty core, adds an `IngressSource` variant (spectator-only needs none,
per the lock), or reopens #25. Where a finding presses on the lock it is
flagged under the escape clause ("Rendered = public, outside the crate,"
below) as a proposed amendment — never worked around.

The delta this ticket designs: the groundwork priced option B as
multicasting the session's *"output stream"*
(`docs/research/collaboration-groundwork.md:66-70`); the lock narrowed the
payload to *"public snapshot and subsequent updates."* The distance
between those two phrases — the rendered=public filter — is the whole
design problem.

## What the crate gives a tap (verified on `claude/m3.11-presence`)

Facts every option below stands on:

- **No output tee exists.** `pump_pty_output` is the sole consumer of the
  PTY output channel; each chunk feeds the parser and is dropped — no
  copy, forward, or snapshot-as-bytes export (`src/systems.rs:166-244`;
  reader thread and bounded channel at `src/runtime.rs:525-540`).
- **The only attach seam is `ratty --command`** — ratty will run an
  arbitrary child as its session process (`src/cli.rs:24-31`,
  `src/runtime.rs:479-486`). A second process cannot attach to a running
  native session: no socket, registry, or multi-consumer channel exists
  anywhere in `src/`.
- **`IngressSource` has exactly one variant, `Local`**; the doc comment
  reserves future relay/bridge variants, assigned out-of-band
  (`src/runtime.rs:36-50`). Everything a spectator's ratty ingests lands
  in its own namespace 0 — one effective principal per PTY
  (`src/presence.rs:93-95`).
- **OSC 778 is the only read facility**, `caps` + 13 `state.*` ops, all
  JSON projections — no op returns raw screen bytes
  (`src/query_channel.rs:56-69`). Replies route by the request's stamped
  ingress source, "never broadcast" (`src/query_channel.rs:328,464-466`),
  so natively a reply surfaces only as input on the session's own PTY
  slave. No unsolicited events: `t=e` is reserved in v1
  (`protocols/query.md:51-52`); state is polled, never pushed
  (`src/web.rs:283`).
- **`feed()` takes exactly the PTY byte stream** — "text, ANSI, RGP,
  Kitty" (`src/web.rs:163-171`); ratty-ai's header states the identical
  bytes drive the browser build (`tools/ratty-ai/src/main.rs:1-7`).
- **In-session 778 clients are crude**: ratty-ai opens `/dev/tty` raw and
  consumes-and-discards unrelated input while awaiting its correlated
  reply (`tools/ratty-ai/src/main.rs:2816-2818,2896-2924`) — two naive
  clients on one tty eat each other's replies.
- **Ratty performs no networking**: relays and bridges live outside the
  process and speak ordinary OSC 777 when they arrive
  (`src/presence.rs:16-20`).
- **The privacy line is explicit.** Rendered = public: everything
  presence draws is public by definition; everything expired is private
  to its owner (`protocols/presence.md:37-39`). An expired foreign row's
  existence must not leak — stated twice (`protocols/presence.md:121`,
  `protocols/query.md:238`). Never readable cross-namespace: private
  style fields, provenance, hidden objects, diagnostics, command
  history, capability grants (`protocols/query.md:110-127`).
- **Presence is wire-origin-only and never recorded**: the macro tap
  skips the six-command family and the applier rejects non-live-ingress
  origins with `not-permitted`, because a replayed `user.join` forges
  liveness (`protocols/presence.md:153-161`; `src/presence.rs:25-28`).

## The option space, priced

Three capture taps exist, named for where they tap. Scores are on the
five axes #45 asked for; fatal risks are listed in full — they drive the
recommendation more than the scores do.

### T. Output tee — interpose below the shell, forward the bytes

The host launches `ratty -e ratty-relay host --listen ADDR -- zsh`
(unquoted — argv shape per "Attach" below): a script(1)-style wrapper
allocates an inner PTY, execs the
real shell, pumps bidirectionally, and tees the output direction —
verbatim to ratty's parser and, seq-numbered, into a WebSocket fan-out.
Input direction (keystrokes, parser auto-replies, 778 replies —
`src/systems.rs:196-198`, `protocols/query.md:45-48`) is never teed.
Native spectator: a stock `ratty -e ratty-relay view --connect URL`
writing received bytes to stdout and discarding stdin. Web:
`ws.onmessage → session.feed(bytes)`.

- **Fidelity: high, with holes.** Byte-identical parser input for
  spectators attached from the start at matching geometry — text, RGP,
  Kitty, presence render exactly as the primary does. Degraded for late
  joiners (no snapshot primitive exists in the crate) and mismatched
  cols×rows (the stream encodes the primary's grid).
- **Public compliance: low — the structural flaw.** See fatal risks.
- **Zero core changes: perfect.** Both ends ride the existing
  `--command` seam; `feed()` already accepts the stream.
- **Wasm parity: high.** `feed()` is documented to take exactly these
  bytes (`src/web.rs:163`, `tools/ratty-ai/src/main.rs:7`).
- **Complexity: low naive; high once honest** — honest compliance needs a
  per-command-family semantic filter plus snapshot synthesis, an
  unversioned shadow copy of crate semantics.

Fatal risks:

- **Namespace collapse (compliance-fatal by construction).** Every
  replayed 777 applies in the spectator's ratty as live local ingress in
  namespace 0 — the namespace the spectator *owns*. Own-namespace read
  scope is "in full including expired rows"
  (`protocols/presence.md:118-122`, `protocols/query.md:110-127`), so a
  spectator-local agent gets owner-tier 778 access to mirrored primary
  state — including expired presence rows. "An expired foreign row's
  existence must not leak" (`protocols/presence.md:121`) is violated by
  design, not by bug.
- **Presence replay is definitionally liveness forgery** — the exact
  thing the macro-recorder exclusion exists to prevent
  (`protocols/presence.md:153-156`); lease clocks re-stamp at spectator
  apply time, so spectators render fresh what the primary expired.
- **Late-join presence cannot repopulate from live traffic at all**:
  `user.renew`/`user.cursor` on absent ids reject `unknown-id`
  (`protocols/presence.md:107`, verified in the errors table). Only
  stateful synthesis rebuilds a roster.
- **No snapshot primitive** (`src/query_channel.rs:56-69` is JSON-only):
  late join is either unbounded history replay (re-runs timed silk/RGP,
  re-ages leases) or a relay-side text-only reconstruction with an empty
  3D scene.
- **Mid-escape-sequence tear**: raw chunks cannot be dropped for a slow
  client without permanently corrupting its parser; the only safe
  overflow policy is disconnect-and-rejoin.
- **Non-rendered traffic leaks**: `tok=` correlation tokens and agent
  query sequences (written to `/dev/tty`, arriving on the output
  direction) ride the tee. The generative command stream is a strict
  superset of the rendered projection.

### R. Relay-hosted PTY — the relay owns the shell, gates semantically

`ratty -e ratty-relay host -- zsh`: the relay owns the shell's
PTY, mirrors the stream through its own vt100 parser (ratty's own screen
dep, `Cargo.toml:83`) plus a shadow semantic model (presence lease math,
an RGP object cache), and passes bytes through a **public gate**:
rendered content verbatim; 778 dropped; 777 classified against a shipped
catalog — hidden-object creation withheld until reveal, provenance
scrubbed, known-failed commands dropped. Late join is synthesized from
the shadow: vt100 repaint + cached-creation-byte re-issue + presence
synthesis at emit-time freshness.

- **Fidelity: 4/5** — bit-identical live tail; late join loses scrollback
  and mid-flight animation; a gate misclassification desyncs spectators.
- **Public compliance: 3/5** — compliant only through a hand-maintained
  shadow catalog outside the crate; drift is a silent leak class. This is
  the escape-clause pressure point.
- **Zero core changes: 5/5** (same `--command` seam; independent vt100).
- **Wasm parity: 5/5** (one WS wire, identical gated bytes both ends).
- **Complexity: high — the option's worst axis.** PTY middleman +
  escape-sequence classifier + shadow presence/RGP model + asset cache +
  snapshot synthesizer + fan-out. The largest build of the three.

Fatal risks:

- **The raw stream is a strict superset of the public projection**
  (hidden-object creation bytes, provenance in creation payloads,
  rejected commands — `protocols/query.md:110-127`), and compliance rests
  entirely on a relay-side catalog with **no mechanical coupling to the
  crate**: any new crate-side op ships to spectators unclassified before
  the catalog updates. The only mechanical fix (per-op public flags via
  `caps`, or a public-projection export) is a core change — amendment
  territory.
- **The relay sits in the primary's critical path**: it owns the shell's
  PTY, so a crash wedges the demo's main session, not just viewers.
- **Hidden-then-revealed desync**: withholding wrong leaks existence;
  replaying wrong desyncs spectators on ids they never received.
- **Late-join re-issue needs interpretive blessing**: full rendered
  appearance draws on fields (notably colors) the cross-namespace 778
  projection deliberately withholds (`protocols/query.md:124-127`) —
  needs @Tnsr-Q's ruling that rendered appearance is public even where
  the structured read model hides it.
- **Spectator-side expired-row retention is unfixable without core
  changes** (same namespace-0 collapse as T).

### Q. OSC-778 query loop — poll the structured public projection

The session shell launches under `ratty-relay wrap -- $SHELL` (778
replies surface only as PTY-slave input, so the relay must interpose from
session start). The relay is a persistent 778 client: every ~250 ms it
re-walks `state.presence` from the start (pagination is monotone, not
snapshot-stable — `protocols/presence.md:128-134`), keeps only
`fresh:true`, diffs by revision, and **synthesizes** plain OSC 777
presence bytes for spectators — ids rewritten `r<ns>.<id>`, `ttl` set to
remaining freshness, deadline timers firing synthetic
`user.leave`/`note.remove` between polls.

- **Fidelity: low-partial — demo-fatal alone.** The presence overlay is
  faithful (quantized to the poll rate), but no 778 op returns screen
  text or output bytes (`src/query_channel.rs:56-69`): spectators watch
  cursors over an **empty terminal**.
- **Public compliance: high — the strongest of any tap.** Core
  pre-filters foreign namespaces to fresh-only; the explicit `fresh` flag
  filters the relay's own namespace 0; emit-time deadline re-evaluation
  closes the mid-batch expiry window; synthesized `ttl` bounds even the
  relay-crash case.
- **Zero core changes: perfect** — a pure protocol client of the shipped
  777/778 surface.
- **Wasm parity: high** — identical bytes into `feed()`; the wasm primary
  is *easier* (`session.query()` promises replace the tty interposer).
- **Complexity: moderate-high** — the poll/diff/synthesize core is simple
  and testable; the tty interposer (reply excision from a shared input
  stream) is the hard, load-bearing 20%.

Fatal risks:

- **Base-screen blindness**: if the demo means "see the session," Q fails
  outright; the only cure is a new screen-dump/subscription op — a core
  change the lock forbids, and one the escape clause (scoped strictly to
  rendered=public *filtering* proving impossible outside the crate)
  would not even cover.
- **No late attach**: replies route to the originating transport
  (`protocols/query.md:45-48`), so only sessions wrapped from launch can
  ever be relayed.
- **Reply-channel contention**: a concurrent naive 778 client (ratty-ai
  in raw `/dev/tty` mode) can eat the relay's replies and vice versa
  (`tools/ratty-ai/src/main.rs:2816-2818`).
- **Spectator namespace flattening**: all synthesized rows land in
  namespace 0, so a multi-namespace primary can overflow the 16/16 caps
  (`protocols/presence.md:92-97`), and a spectator-local writer can evict
  mirrored ids via `replace=true` (integrity, not confidentiality).
- **Sampling loss is structural**: join→leave inside one poll interval is
  invisible forever; cursor motion decimates to the poll rate.

## The recommendation: the gated tee (T-transport + Q-presence)

**Staged combination.** The two champions' fatal weaknesses are exactly
each other's cores: T is the only way spectators see the actual session
(byte-identical parser input — what a demo audience watches) but its
presence forwarding is forgery-by-construction; Q is the only
mechanically exact rendered=public filter but is blind to the screen. And
Q's genuinely hard component — the tty interposer with reply excision —
is the same wrapper T must build anyway, so the combination is cheaper
than either honest standalone.

- **Stage 1 = #46 (walking skeleton): T's transport, control-silent.**
  The tee forwards everything rendered (text, ANSI, SGR, Kitty, RGP)
  verbatim, with the **crate's entire control-plane and
  execution-control classes excised** — the maintained closed
  classification of `protocols/macros.md:81-86`: the presence family
  `user.join`, `user.renew`, `user.cursor`, `user.leave`, `note`,
  `note.remove` (`protocols/presence.md:43-50`), `macro.*`, the
  reactive `rule.*`/`sensor.*` families, and
  `avatar.stop`/`avatar.cancel` — plus **all OSC 778 traffic** (request
  plane, never rendered) and **`tok=` stripped** from any forwarded 777
  (`protocols/query.md:56-71`). The non-presence slices are excised for
  the same structural reasons: a forwarded `rule.set` installs a live
  reactive rule that *fires on spectator instances*, `macro.*` mutates
  spectator macro registries, rule/macro definitions are
  own-namespace-private state the foreign 778 projection never exposes
  (the never-readable class of `protocols/query.md:110-127`), and
  `avatar.stop`/`avatar.cancel` address session-scoped transport-epoch
  handles that are meaningless off-session — the exact grounds the
  macro recorder skips them on. The excision rule is still a fixed,
  documented closed list — anchored to the crate's own maintained
  classification table rather than an ad-hoc one (the same
  catalog-drift caveat that sinks R feeds the amendment below), and
  mechanically reusable: the relay's `#[path]`-shared filter classifies
  with the crate's own predicates, `is_control_plane` (composing
  `is_macro_control`, `is_reactive_control`, `is_presence_control`) and
  `is_execution_control` (`src/osc.rs:678-791`). Stage 1 ships zero
  control-plane bytes: nothing presence — or any control-plane family —
  can leak. The filter itself is stage 1's honest complexity: a
  stateful streaming OSC/APC boundary scanner (PTY chunks split
  sequences — 16 KiB reads, `src/runtime.rs:525-540` — and presence
  commands are BEL-terminated while 778 replies are ST-terminated),
  specified under "What #46 builds".
- **Stage 2 (its own chartered build ticket, demo'd by #47): Q's
  presence engine in the same wrapper.** The wrapper already owns the
  tty interposition, so the 778 poll loop's reply excision comes nearly
  free. Poll `state.presence` ~250 ms, keep only `fresh:true`,
  synthesize the family into the fan-out: ids rewritten `r<ns>.<id>`
  **cap-aware** (the id cap is 48 bytes, `src/presence.rs:129` — an id
  the prefix would push past it truncates with a short hash suffix,
  collision-checked against the mirrored set; an unguarded rewrite
  rejects `too-large` at the spectator and that participant silently
  never appears), name/color copied (rendering metadata, never
  authentication — the #25 lock), `ttl` = remaining freshness, deadline
  timers firing synthetic `user.leave`/`note.remove` at lease expiry.
  All synthesized `user.join`/`note` carry **`replace=true`** —
  idempotent under live/snapshot collision, and honest: a replace
  continues the revision, so observers correctly read "same identity,
  new state", never "brand new" (`protocols/presence.md:83-90`).
  Synthesized presence is **fan-out-only** — generated per-frame from
  the poll model, never entering the ring buffer — so replayed history
  can never resurrect it. `user.leave` removes the registry row
  (`protocols/presence.md:70-80` — rows leave only through
  leave/remove/reset), which closes the namespace-0 collapse that kills
  T **while the link lives**; the disconnect edge is closed on the
  *spectator* side of the link — watch and the web harness track every
  mirrored id they apply and synthesize local leaves on any socket
  close (see "Fan-out wire") — and a spectator process that dies
  outright is bounded by ttl render-decay. The claim in its full
  bounded form is under "Rendered = public" below.

Each stage is independently compliant. Zero core changes, no
`IngressSource` variant, everything in `tools/ratty-relay`.

### Attach

`ratty -e ratty-relay host [--listen ADDR] -- zsh`, **unquoted**:
`-e`/`--command` takes the trailing argv elements as the command vector
(`num_args = 1..`, `src/cli.rs:24-31` — the seam) and ratty execs the
first element directly with no shell splitting
(`src/runtime.rs:479-486`), so a single quoted string would be taken as
a program literally *named* `ratty-relay host …` and ENOENT at spawn.
#46 must verify that clap's bare `--` survives into the command vector
under `trailing_var_arg`/`allow_hyphen_values` (`src/cli.rs:16,29`); if
it does not, `ratty-relay` grows a `--cmd "<string>"` flag it
shell-splits itself. The wrapper allocates an inner PTY,
execs the shell, pumps both directions script(1)-style; SIGWINCH mirrors
winsize inward and emits a structured resize control frame. Input
direction is never teed — 778 replies stay on the originating transport
(`protocols/query.md:45-48`). The relay's own tok-correlated 778 queries
are excised from the tee and their replies from the input-forward.
**There is no late attach** — the crate offers none (reader-verified
absence of any socket/attach seam in `src/`) — so a demo session must be
started under the relay. Demo-day note, not a defect.

### Fan-out wire

One WebSocket server. **Binary frames** carry gated output chunks; **JSON
text frames** carry control metadata out-of-band — `hello {session, cols,
rows, seq}`, `resize`, `reset-notice`, `snapshot-begin/end`, `end` —
matching the #25 doctrine that structured context travels out-of-band,
never inline in the byte stream. Web spectator: `ws.onmessage →
session.feed(bytes)` (`src/web.rs:163-171`) — parity by construction.
Native spectator: `ratty -e ratty-relay watch URL` — prints
received frames to stdout (into that ratty's parser) and reads-and-
discards stdin, sinking keystrokes, the spectator parser's auto-replies,
and 778 acks. Spectator instances are **connection-ephemeral**: watch
tracks every mirrored presence id it has applied and, on **any** socket
close — clean `end` or drop — synthesizes `user.leave`/`note.remove` to
stdout for each before exiting; the web harness does the same through
`feed()` and tears down the widget session. Mirror teardown thus rides
the spectator side of the link and survives every relay-side failure
mode (crash, stall, drop-on-overflow). The server ignores every inbound
client frame; **no code
path from spectator to the shell exists** — "no viewer commands" holds
mechanically, not by policy. Per-client bounded queues; a full queue
drops that client (into the rejoin path below), never backpressures the
pump — the relay sits in the primary's critical path, so slow-spectator
backpressure must be structurally impossible.

### Late-join snapshot semantics

The snapshot is a synthesized composite emitted at a single instant, the
live gated stream buffered during synthesis and flushed after
`snapshot-end`:

1. `hello` control frame `{session, cols, rows, seq}` — geometry is the
   primary's; the watch client letterboxes or warns (the byte stream
   encodes the primary's grid; ratty has no external resize mechanism).
2. **Screen + 3D scene**: replay of the relay's gated ring buffer
   anchored at the last `reset`/full-clear. Replay from a known-clear
   state reproduces terminal text *and* public RGP objects exactly
   (subject to the asset-delivery scope bound under "Rendered =
   public": payload-mode RGP tees complete; a `path=` registration
   renders only where the spectator build embeds the same named asset)
   — and the presence family is **structurally absent from all replayed
   history** (it was excised before the ring, and synthesized presence
   never enters it), so history replay can never resurrect an expired
   row or forge liveness. Byte-history replay of presence is banned
   permanently, not deferred.
3. **Presence** (stage 2): a leave-preamble, then synthesis from the
   poll model. The relay first emits `user.leave`/`note.remove` for
   **every id it has ever broadcast**, before any join — a rejoining
   spectator (the overflow path lands here) still holds mirrored rows,
   including ids the relay no longer tracks, and the preamble clears
   them (on a first-join spectator the preamble is `unknown-id` noise
   in its error ring only; rejects have no state effect). Then
   `user.join` (+`user.cursor`) and `note`, all with **`replace=true`**
   — a rejoiner's surviving rows, fresh or expired, would otherwise
   reject `already-exists` and leave its roster permanently stale
   (`protocols/presence.md:83-90`); the replace continues the revision,
   so observers correctly see "same identity, new state" — for exactly
   the rows fresh **at emit time** (leases are lazily computed, so
   freshness is re-evaluated at emit, not snapshot time —
   `protocols/presence.md:70-80`), `ttl` set to remaining seconds
   (floored at apply to the 1 s clamp — the overshoot bound under
   "Reset and session end"). Rows expired at emit time are never
   emitted.

Degraded fallback when the ring exceeds its cap: a vt100
`contents_formatted()` repaint from the relay's mirror (full text
screen, correct SGR and cursor; the crate's own screen dep,
`Cargo.toml:83`) plus presence synthesis, with the 3D scene empty until
objects are next touched — honest, and disclosed in the `hello` frame.
In the #46 skeleton, before that repaint lands, the no-anchor case
(a session scrolled past the ring cap without ever clearing) degrades
one step further: a synthetic anchor — ED2 + cursor-home — and a live
tail from a blank screen, disclosed via a `degraded` flag in `hello`;
replaying a cap-truncated ring from a non-clear anchor is never an
option (it is the mid-sequence tear T is faulted for).
Known cosmetic limit of ring replay: timed silk/RGP playback re-runs at
join; end state converges. Slow-spectator overflow lands in this same
functional path via disconnect-and-rejoin.

### Reset and session end

- **Reset**: the primary's `reset` rides the tee verbatim — its effect is
  rendered, so spectators reset in lockstep, rosters included (`reset`
  clears every roster silently, `protocols/presence.md:162-164`; the
  forwarded bytes do the same on each spectator instance). The relay
  treats it as the history barrier: drop the ring to the new anchor,
  clear the vt100 mirror, clear the presence model **without** emitting
  synthetic leaves — spectator rosters were already cleared, and
  synthetic leaves would only generate `unknown-id` noise in spectator
  error rings (`protocols/presence.md:107`).
- **Session end** (shell EOF or relay shutdown): emit synthesized
  `user.leave`/`note.remove` for all mirrored rows, forward one
  plain-text `[relay] session ended` banner frame plus an `end` control
  frame, close all sockets with reason `session-ended` (widget shows
  disconnect; watch client prints the notice and exits); the wrapper
  exits with the shell's status so the primary sees a normal child exit.
  **Primary death**: if ratty itself dies, the relay's stdio hits
  EOF/SIGHUP; it runs this same teardown path, skipping the banner, and
  exits.
- **Backstop**: if the relay dies uncleanly, no further leaves arrive
  from it — teardown falls to the spectators' own close-time leave
  synthesis (the sockets drop with the relay). For any mirror that
  nonetheless survives, every synthesized `ttl` was remaining-freshness,
  so it stops rendering within the primary's own public window **plus
  the 1 s apply-side clamp floor plus one poll interval (~250 ms)** —
  `pin_ttl` clamps a sub-second remainder up to 1 s
  (`MIN_PRESENCE_TTL_SECS`, `src/presence.rs:141-142,530-534`). Decay
  is a rendering bound, not deletion — which is why the
  connection-ephemeral rule above, not this backstop, carries the
  teardown.

### Spectator auth

**No writer authentication and no per-viewer identity, because there are
zero writers among viewers.** The #25 lock binds authentication to
writers-before-ingress; spectators are pure readers with no upstream byte
path (watch discards stdin; the server ignores inbound frames), so the
rule imposes nothing on them, and no source identities are assigned
because spectators never enter ingress — hence no `IngressSource`
variant anywhere, exactly as the lock states. The relay itself is the
session-owner-launched transport holding the tty by construction (one
effective principal with the session, namespace 0 —
`src/presence.rs:93-95`); its only upstream writes are its own 778 reads
on the query plane. **Viewing is gated, not identified**: bind localhost
or a 0600 unix socket by default; remote demos require a relay-minted
random bearer token presented in the WS handshake `Authorization` header
— never in the URL (URL tokens leak via logs and history) — TLS behind a
flag. Display names in mirrored presence remain rendering metadata,
never authentication (#25 lock, `protocols/presence.md:15-19`).

### Why not the others alone

- **Q alone**: base-screen blindness is demo-fatal; the cure is a new
  screen op — a core change the lock forbids, and one the escape clause
  (scoped to rendered=public filtering) would not even cover.
  Its presence engine — the strongest compliance mechanism of any option
  — is salvaged wholesale as stage 2, where its hardest component is
  already paid for.
- **R alone**: the largest build of the three, exceeding the 1–2-session
  bound, and compliance still rests on an uncoupled catalog where every
  new crate op is a silent leak until classified. The combination keeps
  R's live-tail fidelity at a fraction of the machinery; R's asset-cache
  late-join remains a future upgrade if ring replay proves insufficient.
- **T alone**: compliance-fatal by construction (namespace collapse,
  liveness forgery, expired-row retention), and its late-join presence
  story is broken, not degraded — `renew`/`cursor` on absent ids reject
  `unknown-id` (`protocols/presence.md:107`), so rosters never
  repopulate from live traffic. Closing the gaps grows T into either
  this combination or R.

Accepted costs, shared by every viable option and documented rather than
hidden: the relay is in the primary's critical path (mitigated by bounded
queues + drop policy); sessions must start under the relay (no late
attach exists); geometry is the primary's; namespace flattening caps a
multi-namespace primary at the spectator's 16/16
(`protocols/presence.md:92-97`) — irrelevant for a single-PTY demo.

## What #46 builds, what it defers

The walking skeleton's stated bound: **native viewer = a plain ratty
running the relay client under its PTY; web spectator = a minimal static
harness feeding `feed()`.** Within that bound, #46 builds stage 1
end-to-end:

- `tools/ratty-relay` with `host` (interposer + tee + fan-out) and
  `watch` (print-to-stdout, discard-stdin) modes, both riding
  `ratty -e`/`--command` (`src/cli.rs:24-31`; verify the bare-`--`
  passthrough noted under "Attach").
- The excision filter: the crate's full control-plane and
  execution-control classification (`protocols/macros.md:81-86`) — the
  presence family (`protocols/presence.md:43-50`), `macro.*`, the
  reactive `rule.*`/`sensor.*` families, and
  `avatar.stop`/`avatar.cancel` — plus all OSC 778 and `tok=` stripping
  on forwarded 777. Host mode **`#[path]`-shares the terminal's own OSC
  recognition**, per #46's directive and the ratty-ai precedent
  (`#[path = "../../../src/osc.rs"]`, `tools/ratty-ai/src/main.rs:27`):
  classification calls the crate's own predicates —
  `is_control_plane` (composing `is_macro_control`,
  `is_reactive_control`, `is_presence_control`) and
  `is_execution_control` (`src/osc.rs:678-791`) — never a re-typed
  command list.
- The streaming OSC/APC boundary scanner the filter runs inside: PTY
  reads are 16 KiB chunks with no sequence alignment
  (`src/runtime.rs:525-540`), presence commands are BEL-terminated
  (`protocols/presence.md:43-50`) while 778 replies are ST-terminated
  (`protocols/query.md:38-39`), so the tee recognizes **both
  terminators**, withholds bytes until each sequence classifies, and
  forwards everything else untouched; the same scanner serves excision,
  `tok=` stripping, reset detection for the ring anchor, and injection
  — relay-originated 778 requests enter the output stream **only at
  recognized sequence boundaries** (mid-sequence injection corrupts the
  primary's parse). Required stage-1 test: a **differential test
  against the crate parser** under random chunk splits and both
  terminators — a tokenization divergence here is precisely a presence
  leak.
- WS fan-out with `hello`/`resize`/`reset-notice`/`end` control frames,
  bounded per-client queues, drop-client-never-backpressure.
- Ring buffer anchored at reset/full-clear; late join via ring replay;
  disconnect-and-rejoin as the overflow policy; the defined no-anchor
  behavior (synthetic ED2 + cursor-home anchor, blank-start live tail,
  `degraded` flag in `hello` — see "Late-join snapshot semantics").
  Alternatively #46 may pull the vt100 `contents_formatted()` repaint
  forward (a dozen lines against the crate's own vt100 0.16,
  `Cargo.toml:83`); either keeps the skeleton bound.
- The minimal static web harness: a page that opens the WS, pipes
  binary frames into `RattySession.feed()` (`src/web.rs:163-171`), and
  calls `session.drain_input()` on an interval, discarding the result —
  the spectator parser's auto-replies otherwise accumulate forever in
  the unbounded input channel (`src/web.rs:175-181`,
  `src/runtime.rs:342-346`). Nothing more — hosting, embedding, and the
  real site widget belong to the browser-story lane (#53's pane
  research feeding #57's choice).

Deferred (each to a named home, in order):

- **The Q presence engine — stage 2 — needs a chartered build home, and
  this document recommends creating one.** #47 is chartered as an
  at-screen live-run task (the map's venue exception — not an
  engineering session), and its demo *requires* live presence — carets,
  name labels, notes — so "defer to #47" would fail #47 on its own
  terms, and "#47 or later" doubly so. Recommendation: charter **one
  new AFK task ticket on map #42 — working title "Build the relay
  presence engine (relay stage 2)"** — living entirely in
  `tools/ratty-relay` (plus the watch/harness id-tracking and
  close-time leave synthesis), **blocked by #46 and blocking #47**.
  Contents: poll/diff/synthesize, cap-aware id rewriting, ttl clamping,
  deadline timers, `replace=true` synthesis, the snapshot
  leave-preamble, session-end and close-time leaves. The one fallback —
  folding stage 2 into #46 — applies only iff the skeleton lands well
  under its 1–2-session bound; the default is the separate ticket,
  keeping #47 pure demo-run. #46 spectators see no presence at all,
  lawfully.
- The vt100-mirror repaint fallback for oversized rings (ring replay
  plus the synthetic blank anchor are the skeleton's only snapshot
  paths; the repaint lands when the demo needs long-running sessions).
- Remote hardening: bearer-token gate and TLS flag (the skeleton binds
  localhost/unix only).
- Letterboxing polish in `watch` (the skeleton warns on geometry
  mismatch).
- R's asset-cache late-join upgrade — only if ring replay proves
  insufficient in practice.

## Rendered = public, outside the crate

The compliance argument, in two halves — one clean, one that triggers a
finding:

**Presence: the escape clause does not fire.** The public projection of
presence is *exactly the fresh set* — rendered = public is stated as a
design goal (`protocols/presence.md:37-39`) and enforced by the renderer
(fresh rows render, expired rows do not,
`protocols/presence.md:184-196`). The 778 read surface makes that set
computable wholly outside the crate: foreign namespaces arrive
pre-filtered to fresh-only by core, and the relay's own namespace 0 —
delivered in full including expired rows
(`protocols/presence.md:118-122`) — filters on the explicit `fresh`
flag. Emit-time deadline re-evaluation closes the mid-batch expiry
window (leases are lazy: `fresh = (now − updated) ≤ ttl`,
`protocols/presence.md:70-80`). The hard invariant — an expired foreign
row's existence must not leak (`protocols/presence.md:121`,
`protocols/query.md:238`) — holds **by construction for what a
spectator can ever learn**: stage 1 ships zero presence bytes, and
stage 2 synthesizes only rows fresh — public — at emit time, so no
spectator ever learns of a row that was not lawfully public when sent.
Retention of lawfully-seen mirrors is the *bounded* half of the claim,
not an absolute: **closed while connected** (deadline-timer leaves
remove rows before they expire in place); **closed on disconnect by
client-side leave synthesis** (watch and the harness track applied ids
and synthesize leaves on any socket close — spectator instances are
connection-ephemeral, so relay crash and drop-on-overflow tear mirrors
down with the connection); **bounded by ttl render-decay if the
spectator process itself dies** — its mirrored rows then expire in
place in its namespace 0 and stay queryable there until that process
exits (no sweep ever deletes a record,
`protocols/presence.md:70-80`), a retention window that dies with the
process. The fresh-overshoot bound is the apply-side clamp: remaining
freshness under 1 s floors up to 1 s (`MIN_PRESENCE_TTL_SECS`,
`src/presence.rs:141-142`), plus up to one poll interval of staleness.

**The general scene stream: a bounded finding, filed as a proposed
amendment.** The raw output stream is a strict superset of the rendered
projection: hidden-object creation bytes, provenance fields in creation
payloads, and commands that failed (and thus never rendered) all ride it,
and all are never-readable cross-namespace
(`protocols/query.md:110-127`). Filtering them exactly requires per-op
semantic knowledge with **no mechanical coupling to the crate** — a
shadow catalog that silently leaks on every crate-side addition until
manually updated. That is the escape-clause finding, and per #45 it goes
to @Tnsr-Q as a proposed amendment — a per-op public/private
classification exposed via `caps`, or a public-projection export — never
as a relay-side workaround catalog.

**The exact trigger condition**: the escape clause fires the moment a
relayed session must carry content whose public projection differs from
its byte stream — concretely, the first demo or spectator use that
requires (a) hidden-object creation with later reveal, (b) creation
payloads whose provenance fields must be scrubbed, (c) suppression of
commands the crate rejected, or (d) spectator-visible behavior that
depends on control-plane or execution-control traffic (`macro.*`,
`rule.*`/`sensor.*`, `avatar.stop`/`avatar.cancel`) — stage 1 excises
those families wholesale, so a demo that needs them seen cannot be
served by filtering. Until then it does not fire: #46/#47 scope-limit
the demo session to content rendered verbatim (no hidden objects, no
cross-namespace-private reliance, no control-plane/execution-control
traffic the spectator view depends on), which is a documented scope
bound, not a filter. One further asset-delivery bound rides with it:
relayed demo sessions use **payload-mode (inline base64) RGP** — a
`path=` registration names an embedded build asset, never a filesystem
path (`src/osc.rs:185-192`, enforced at `src/inline.rs:876`), so it
renders only on spectator builds that embed the same named asset:
true for same-build native viewers, unverified for a size-trimmed web
bundle. Path-mode is therefore a known, disclosed fidelity hole, not a
filtered one; a demo may use it only with every spectator build's
embedded-asset set confirmed. Kitty needs no bound — ratty's Kitty
support is inline-base64 only (`src/kitty.rs`), so teed Kitty is
delivery-complete by construction. If the demo lane cannot live inside
these bounds, the amendment is the only path — the lock's escape clause
says so explicitly.

## Requirements capture

`protocols/presence.md` is **normative now** for everything this design
excises and synthesizes — the six-command family and grammar
(`:43-50`), lease math (`:70-80`), collision/revision rules (`:83-90`),
caps (`:92-97`), errors (`:99-112`), read scope (`:116-134`), and
classification (`:148-164`) — alongside `protocols/query.md` for the 778
envelope, ack, and read-tier contracts, and
`protocols/macros.md`'s command-classification table (`:81-86`), which
**defines** the stage-1 excision: the filter is that table's
control-plane and execution-control classes, nothing more, nothing
less. When the
[#41](https://github.com/Tnsr-Q/ratty/issues/41) close-out driver lands,
it supersedes this capture as the requirements driver; this document and
the #46 implementation should be re-checked against it, and any
divergence resolved in #41's favor.

## Open questions, routed

- **→ the browser-story lane
  ([#53](https://github.com/Tnsr-Q/ratty/issues/53)'s pane-topology
  research feeding [#57](https://github.com/Tnsr-Q/ratty/issues/57)'s
  choice)**: how `hello` geometry maps onto the widget's layout — a
  pane-topology question, which is what that lane actually prices and
  chooses. The rest of the post-harness browser work — how the real
  site widget hosts the WS client, reconnect/rejoin UX, whether the
  harness graduates into the site — follows the pane choice and
  belongs to whichever ticket implements it. #46 deliberately ships
  only the `ws.onmessage → feed()` proof.
- **→ [#47](https://github.com/Tnsr-Q/ratty/issues/47)'s at-screen
  session with @Tnsr-Q, answers captured in
  `docs/research/relay-demo-lessons.md`**: the judgment calls this
  recommendation load-bears — accepting the relay in the primary's
  critical path (drop policy as sole mitigation); accepting
  no-late-attach as an operational constraint rather than a gap; the
  amendment framing for the general-stream filter (per-op `caps`
  classification vs public-projection export); whether
  rendered-appearance-public should extend to fields the structured
  read model withholds (R's late-join color question — moot for the
  combination unless the asset-cache upgrade lands); and the
  disconnect-and-rejoin overflow policy under real demo load. #57's
  charter is exclusively the three-option browser-story choice — these
  calls are not on its agenda, and no relay-judgment grilling exists
  anywhere on the map — but #57 formally consumes the lessons doc, so
  answers recorded there reach the one venue that already reads them.
  If #47's venue proves too thin for the heavier calls, charter a small
  relay grilling instead.
- **→ @Tnsr-Q, as the amendment above**: the general-scene-stream
  finding — the only part of this design that touches the lock, filed as
  the escape clause prescribes.
