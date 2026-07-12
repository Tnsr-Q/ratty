# Collaboration groundwork

Research asset for [wayfinder ticket #19](https://github.com/Tnsr-Q/ratty/issues/19)
(map [#10](https://github.com/Tnsr-Q/ratty/issues/10)). Findings and options only —
the design decision belongs to the collaboration design ticket (#25).

## The key insight: ratty already chose "presence, not transport"

The committed collaboration commands (`src/osc.rs:289-321`) are
`UserJoin { name, color }`, `UserLeave { name }`, `UserCursor { name, x, y }`,
`Note { text, x, y, expires }` — all log-only. These are **presence-rendering
commands**: they describe what to *show*, not how bytes move. Anything that
can write to the terminal's input stream (a relay daemon, an ssh session, a
bridge process, a replayed transmission) can drive them. Transport is
external to ratty by construction, and the design should keep it that way —
it mirrors the ecosystem's layer split (`docs/ecosystem-vision.md`).

## The-comune: state as of 2026-07-12

Assessed directly from [Tnsr-Q/The-comune](https://github.com/Tnsr-Q/The-comune)
(README, `src/merge.rs`, `src/node.rs`, tests; summary drafted with a local
model and verified against the source).

- **Self-described M0/M1 scaffold** — deterministic merge-law foundation +
  two-node signed PCKP merge demo. Single crate (`src/`, `tests/`,
  `examples/`), CI added 2026-07-06, last push 2026-07-06. Six open issues
  (merge route, agentzk, PCKP 0.1, PCKP v0.2, demo review, fixes).
- **The ingest pipeline is real**: signature verification against a key
  resolver, content-hash dedup, node-id and schema checks, delta size caps,
  per-source sequence chains with pending parking and equivocation-evidence
  capture, envelope-derived merge metadata. The replication/belief split is
  implemented (belief profiles are local, non-replicated).
- **Nothing is durable.** Graph is an in-memory map, the WAL is a `Vec`,
  chains/pending are in-memory; restart loses everything. The README's own
  "Next" list: SparrowDB `GraphRepository`, NATS/QUIC relay, durable WAL,
  epoch prover.

### Critical defect (verified in source, still present)

`src/merge.rs` documents "no rejection paths: every valid packet folds into
state the same way in every order" — but the code early-returns
`Err(ImmutableWrite)` when an append-only (`fact:`/`episode:`/…) register
already exists, *before* the deterministic first-write-wins comparison
below it can run (that FWW arm is dead code for existing keys). Consequence:
**cross-source writes to the same append-only register are
delivery-order-dependent** — replicas keep whichever arrived first and
reject the other, i.e. permanent divergence. The error also aborts mid-delta,
leaving earlier patches applied (partial application).

The `issue5` permutation test doesn't catch this: its "immutable violation"
comes from the *same source* at sequence 2, chained to the first packet's
hash, so the sequence chain fixes its processing order in every permutation.
The missing test is exactly the one the advisory notes predicted: a
**cross-source, same-UID append-only collision** under permuted delivery.
(Intent was clearly shadowing — `node.rs` even counts "FWW violation
attempts" in `shadowed_writes`.) Fix belongs upstream on The-comune; noted
here because it gates any ratty integration that relies on convergence.

## Options for the minimal collaboration primitive

- **A. Presence rendering (no networking in ratty).** Lower
  `UserJoin/Leave/Cursor/Note` onto the scene: named cursors with colors,
  floating annotations with expiry. Demoable with two local processes and a
  script, or a replayed transmission. No new deps, no trust surface beyond
  what OSC-777 already has, browser-equal.
- **B. Spectator relay (first real transport, outside the crate).** A small
  daemon in `tools/` that multicasts one session's output stream to N
  read-only viewers (native ratty or the web widget via `feed()`).
  Upstream-clean (nothing in ratty core changes), real multi-user, no
  comune dependency.
- **C. Comune-bridged presence (the ecosystem path).** A bridge daemon
  subscribes to PCKP packets and emits OSC-777 presence commands into a
  ratty session — agents' positions/moods rendered from the shared
  knowledge stream. Gated on The-comune reaching durability + a relay
  (its own M2+ list) and on the convergence fix above.

## Deferred (recorded, not designed here)

- Shared *input* (multi-writer terminals) — a different, much larger organ.
- The multi-user trust boundary for who may speak OSC-777 in a shared
  session — already on the map as fog; C makes it concrete (PCKP's
  replication-vs-belief split is the natural frame).

## Recommendation to carry into #25

Lock **A** as the collaboration organ for M3 (pure rendering, honest,
testable, browser-equal). Name **B** as the first transport experiment in
`tools/` when a demo needs to be real. Treat **C** as the ecosystem
integration milestone, explicitly gated on The-comune's durability/relay
milestones and the cross-source convergence fix.
