# The browser story

Research asset for [wayfinder ticket #53](https://github.com/Tnsr-Q/ratty/issues/53)
(map [#42](https://github.com/Tnsr-Q/ratty/issues/42)). **Recommendation only** —
the lock is HITL at [#57](https://github.com/Tnsr-Q/ratty/issues/57), which
convenes with this doc, the [#55](https://github.com/Tnsr-Q/ratty/issues/55)
mux spike's findings, the [#47](https://github.com/Tnsr-Q/ratty/issues/47)
spectator demo's lessons doc (`docs/research/relay-demo-lessons.md`), and the
#56-locked spine all on its table. The "#55 spike charter" section below
proposes a conscious re-charter of that spike's de-risk set — see its
preamble — sharpened so it produces evidence instead of vibes.

Locked precedent this doc builds on, not around:

- **#22 (graduation ruling):** placement, not splits; the tmux split-tree and its
  `u8` pane ids are retired (`pane.*` OSC is superseded-pending,
  `src/osc.rs:351-375`); and "the browser question travels with the new map
  (per-pane feeds vs. in-band framing vs. single-runtime widget) — undecided
  here, on purpose."
- **#25 (presence, tier-B relay):** the web spectator is
  `ws.onmessage → session.feed(bytes)`. The lock's out-of-band rule is scoped
  to identity and trust: relays and bridges "authenticate writers before
  ingress, assign source identities, and pass them out-of-band through a
  trusted structured channel — never inline in the byte stream" — summarized
  as *nothing in-band is trusted about identity*. The broader "structured
  context travels out-of-band" phrasing is the relay doc's gloss on that lock
  (`docs/research/relay-design.md:336-337`), not the lock text itself.
- **#13 (panes feasibility, PR #28):** the three options originate verbatim in
  `docs/research/panes-feasibility.md:45-58`; native is the easy half, "the
  browser is the hard half."

## The widget API today (verified surface)

The full `#[wasm_bindgen]` surface is one free function and one class; JS never
sees framing below `feed`:

- `start(canvas_selector, config_toml) -> Result<RattySession, JsValue>`
  (`src/web.rs:315`) — boots one Bevy App into one canvas; winit spawns onto the
  browser event loop and returns, so the handle is live immediately
  (`src/web.rs:396-398`).
- `RattySession`: `feed(&[u8])` (`:164`), `feed_text(&str)` (`:169`),
  `drain_input() -> Vec<u8>` (`:175`), `query(op, data, timeout_ms) -> Promise`
  (`:192`), `set_mode` / `set_warp` / `set_view` (`:248-272`), `user_gesture()`
  (`:284`). `feed` is an unbounded mpsc send that never blocks.
- **There is no resize, no stop, and no pane or session id anywhere.** Grid size
  comes from config at `start()` (`src/runtime.rs:431-432`), then
  `fit_canvas_to_parent` → `WindowResized` → `handle_window_resize` →
  `resize_to_fit` (`src/systems.rs:287-333`); on wasm `runtime.resize` only
  reflows the vt100 parser (`src/runtime.rs:585-607`). The page controls size
  via CSS only.

Constraints that bound every option below:

- **One `start()` per page, documented in code.** `PENDING_QUERIES` is a
  page-global thread-local whose comment states "sessions are not expected to
  coexist (one `start()` per page), and disposal rejects everything"
  (`src/web.rs:57-63`). `Drop` drains the whole map — a second session's
  disposal would reject the first's in-flight queries (`src/web.rs:291-304`) —
  and `free()` never stops the Bevy app or releases the canvas; no teardown
  path exists.
- **The singleton is the Bevy resource spine, not the channel.**
  `TerminalRuntime::virtual_channel` (`src/runtime.rs:430-458`) is already
  multi-instantiable and cheap: two unbounded mpsc pairs plus one vt100 parser
  per instance, unbounded specifically so a single-threaded wasm embedder can
  never block (`src/runtime.rs:426-429`). What is singleton-shaped is the spine
  inserted at `start()` (`src/web.rs:342-343`): `Res/ResMut<TerminalRuntime>`
  at 10 use sites across `systems.rs`, `keyboard.rs`, `mouse.rs`,
  `query_channel.rs`, `scene/mod.rs`; `TerminalSurface` referenced in 9 files;
  plus satellites (`TerminalInlineObjects`, `VizRegistry`, redraw state,
  presence, sound).
- **Silk → feed byte path.** A `.silk` cast is JSONL: one header line, then
  strict `[time, code, data]` 3-tuples (`tools/silk/src/cast.rs:152`). Only
  `"o"` events reach the terminal; `SilkPlayer.tick()` fans each payload to
  every attached backend (`site/player/silk-player.js:60-73`); the wasm backend
  encodes and calls `session.feed` (`site/player/backend-wasm.js:130-140`).
  Multi-consumer of *one* stream exists today; multi-stream does not.
- **rAF economics.** Frames run on `WinitSettings::continuous()`
  (`src/web.rs:345-346`); hidden-tab throttling defers query replies and
  timeouts together (`src/web.rs:127-151`); `feed()` buffers unboundedly while
  throttled and `pump_pty_output` drains everything in one wake frame
  (`src/systems.rs:180-239`). Presence leases stall with everything else — one
  `Res<Time>` per widget, not per pane (`protocols/presence.md:198-205`).
- **WebGPU is mandatory** — "never fall back to WebGL2" (`src/web.rs:365-373`);
  the backend gates on `navigator.gpu` (`site/player/backend-wasm.js:16-18`).
  Audio unlock is a session-level gesture contract
  (`site/player/backend-wasm.js:51-80`, `src/web.rs:376-388`).

One pricing rule applies before the options diverge: **the Bevy singleton-spine
migration to per-pane entities is identical work under (a) and (b), and (c)
merely sequences it.** It is priced once, here, and excluded from each option's
marginal cost — #55 should measure the framing layer alone, not re-price the
spine.

## The three options, priced

### (a) Per-pane feeds — `feed(pane_id, bytes)` as an additive superset

**Design.** Not a signature break if staged: `RattySession`'s four methods keep
exact single-pane semantics (aliasing the primary pane), and one exported class
is added — `RattyPane`, minted by `open_pane(id: &str, config_toml)`, with its
own `feed`/`feed_text`/`drain_input`/`query`/`close()`. Ids are #22/#12-style
namespaced strings, never the retired `u8`. Each `open_pane` instantiates a
`virtual_channel` seam and a pane entity in the one App on the one canvas —
placement ops, not splits, decide where it renders. Stage and audio stay
session-scoped. `PENDING_QUERIES` becomes pane-keyed so `close()` rejects only
its own queries — fixing today's disposal-rejects-everything wart rather than
widening it. In silk, pane bytes ride the **event code** (`[time, "o@<id>",
data]`), never a fourth tuple element — verified lawful inside silk/1: the
validator warn-and-ignores unknown codes (`tools/silk/src/validate.rs:135-139`)
and players MUST ignore them (`protocols/silk.md:95-103`), so old players
lawfully drop non-primary panes and render pane 0. Per-pane geometry rides
`x_ratty.panes` under the unknown-keys-MUST-be-ignored rule.

**API stability.** No signature breaks; the site player and tier-B spectator
run unchanged — the feasibility doc's "widget API break" overstates the cost
once (a) is staged as a strict superset. The honest change is *obligations*:
multi-pane hosts must drain N `drain_input`s (the relay doc's accumulate-forever
hazard, `docs/research/relay-design.md:537`, multiplied by N), adopt per-pane
query disposal, and align ids with the not-yet-locked placement scheme
(#49's command family, locked at #56).

**Silk compat.** Lawful inside silk/1 — no `silk/2` bump (the validator
hard-errors on any other format, `tools/silk/src/validate.rs:85-89`), the
strict 3-tuple survives (asciinema-v2 superset intact). Byte-reproducibility
holds: authored string ids only, no nonces/session ids/wall-clock; a
single-pane transmission emits nothing new, so the golden byte-identical
recompile (`tools/silk/src/compile.rs:1416-1437`) never moves. Real cost:
scene-DSL pane fields are a hard version skew under `deny_unknown_fields` (old
compilers reject new `scene.json`) — a tooling break the container's
lawfulness does not cover — and the validator must grow per-pane scan state.

**Wasm constraints.** One `Time`, one canvas, one AudioContext: the rAF stall
stays whole-widget and coherent. Marginal cost per pane: one vt100 parser plus
two unbounded channels; hidden-tab throttling buffers N backlogs and the wake
frame drains all of them at once — today's single-pane spike times N is the
real number to measure. The spine migration is shared with (b), so (a)'s
marginal wasm price is the bindgen surface, pane-keyed query routing, and
JS-side stream routing.

**Fatal risks.**

- **Sequencing:** pane ids must be namespaced placement ids in the style #22
  ruled — but #22 ruled the style only; the concrete scheme is owned by #49
  (the replacement command family and placement semantics) and locked into the
  spine at #56. Shipping earlier resurrects the retired `u8` shape or invents
  a third scheme — (a) cannot honestly land before the #49→#56 placement-id
  lock.
- **Skew:** `deny_unknown_fields` in `tools/silk/src/scene.rs` makes any
  multi-pane authoring field a hard reject for older compilers.
- **Exported demux:** every host — site player, relay harness, future
  embedders — learns pane routing and per-pane `drain_input`; one forgotten
  pane's unbounded input channel grows forever. If the ecosystem vision is
  many dumb embedders, (b)'s crate-side demux keeps hosts simpler.
- **Wake stampede:** N unbounded backlogs applied in one rAF frame on tab wake
  — unmeasured, and exactly the spectator scenario #47 exercises.
- **Query routing may not decompose:** `PENDING_QUERIES` /
  `try_resolve_pending` are page-global by design (`src/web.rs:57-63`,
  `src/query_channel.rs:444-445`); if reply correlation cannot be pane-keyed
  cleanly, the `RattyPane` disposal contract is a lie.
- **Loop reset undefined across panes:** silk's reset is one escape sequence
  into one channel (`site/player/backend-wasm.js:108-114`); without a
  deterministic per-pane reset order, looping multi-pane transmissions desync.
- **Narrowing differentiator:** the prerequisite spine migration is (b)'s too —
  if #55 shows crate-side mux is trivial, (a)'s remaining edge is API honesty
  and per-pane parser isolation vs (b)'s zero host changes.

### (b) In-band mux framing — one `feed()`, pane-addressed frames, crate-side demux

**Design.** A new APC family beside RGP's `\x1b_ratty;g;` (`src/rgp.rs:6`):
`\x1b_ratty;m;1;<pane-id>;<base64-payload>\x1b\\` — fixed version literal,
namespaced string pane id (never the retired `u8`), payload capped (~4 KiB raw)
to bound demux buffers. **Pane 0 is never framed** — its bytes flow bare,
byte-identical to today. Frames may split pane-level escape sequences (each
pane's parser reassembles, as PTY chunk tears are handled today); the frame APC
itself must not tear across silk events (existing validator rule,
`tools/silk/src/validate.rs:200-204`). Demux is crate-side: `feed()` unchanged,
a stateful scanner ahead of the parsers routes bare bytes to pane 0 and decoded
frames to pane-N `virtual_channel`s. The grammar ships crate-side via the
existing `#[path]` sharing pattern (`tools/silk/src/main.rs:16-34`) — mandated
by the conformance *posture*, not by existing behavior: the precedent set for
777 frames is that validators surface what ratty silently drops
(`protocols/silk.md:243-246`), but today's validator silently passes unknown
ratty-APC families — its APC scan checks only the RGP and Kitty prefixes
(`tools/silk/src/validate.rs:189-221`) — so a multi-pane cast would validate
clean with zero grammar sharing. Adding `m`-frame decoding to the validator is
therefore part of (b)'s price, not a preexisting mechanic. Pane-N
auto-replies are framed on the way out; `query()` gains an optional `pane`
field inside its JsValue data (default 0) — no signature change.

**API stability.** Zero public break: all eight bindgen members keep exact
signatures; the one-per-page invariant is preserved because there is still
exactly one `RattySession` with N internal channels. But the instability is
*relocated, not eliminated*: a versioned private frame grammar becomes
API-in-fact for any mux-aware host (a native primary demuxing framed
auto-replies out of `drain_input`), just not API-in-signature.

**Silk compat.** Best of the three: no container change at all. Multi-pane
frames are "just more `o` bytes" — silk's stated growth posture
(`protocols/silk.md:255-262`). No `silk/2` bump, no fourth tuple element, no
header break; placement geometry rides `x_ratty.panes` under MUST-ignore.
Determinism holds by construction (no session ids/nonces/wall-clock; canonical
base64; fixed version literal); single-pane compilation emits zero frames, so
the goldens recompile byte-identically. Degradation is lawful *in posture*: an
unaware player renders pane 0 and drops unknown APCs — but see fatal risks.

**Wasm constraints.** The coherent case for hidden tabs: a single stream
preserves total cross-pane byte order through throttling and the one-frame wake
drain; (a)'s N independent feeds guarantee no cross-pane ordering at all.
Gesture/audio untouched. Memory: N parsers, N unbounded pairs, plus one
*bounded* demux buffer (an unterminated frame hits the cap and errors rather
than buffering forever). New cost: base64 decode on the rAF frame path for
panes ≥ 1 — measurable, and exactly what #55 should run against charter
item 2's traffic corpus.

**Fatal risks.**

- **Doctrinal exposure, not settled inversion:** #25's lock forces
  *identity/source assignment* out-of-band — relays and bridges "authenticate
  writers before ingress, assign source identities, and pass them out-of-band
  through a trusted structured channel — never inline in the byte stream." The
  general "structured context travels out-of-band" phrasing is the relay doc's
  gloss on that lock (`docs/research/relay-design.md:336-337`), not the lock
  text. Pane addressing MAY be ruled analogous to the identity context #25
  keeps out-of-band — a #57 judgment call, not a settled ban. The steel-man —
  framing is transport routing at the same layer as ANSI itself, not an
  identity or authority claim — is in fact closer to the lock's literal scope
  than the generalized reading; the call is #57's to make, *with* #55's
  evidence.
- **"Private protocol" is a fiction** once (b) ships whole: the freeze begins
  when validator `m`-frame decoding (a deliberate new feature, added via the
  `#[path]` pattern) and the first multi-pane golden land. From that point,
  golden byte-identity freezes the grammar against the crate and every
  golden-tested transmission (3 of the 5 committed today — the golden test
  covers orchard-upside-down, predator-and-frame, and soul,
  `tools/silk/src/compile.rs:1422-1424`, despite its comment claiming every
  committed transmission). Grammar changes become migration events. This is
  tmux control mode's documented cost — "a protocol to version and parse"
  (`docs/research/panes-feasibility.md:68-70`) — in full, not reduced.
- **Pane-0 corruption:** a second stateful scanner sits in front of vt100; a
  frame-boundary bug leaks pane-N bytes into the visible pane — the worst
  possible failure mode. The relay doc's mid-sequence-tear hazard
  (`docs/research/relay-design.md:134-136`) relocates from battle-tested vt100
  into new code on the hot path.
- **Pane-id shape hazard:** freezing an id scheme before the #49→#56 placement
  lock risks a version bump against committed transmissions.
- **base64 bloat:** ~33% wire and file cost plus encode/decode for every
  pane ≥ 1 byte — unmeasured at real `feed()` rates.
- **Reply attribution moves, it does not vanish:** framed pane-N auto-replies
  interleaved with bare pane-0 replies must be split and routed by the native
  primary/relay. The cost (a) pays in the widget, (b) pays in the harness.
- **Lawful degrade is inferred, not proven:** that the current vt100 ingest
  drops unknown `ratty;m` APCs cleanly, pane 0 byte-identical, follows from
  silk/RGP posture but has never been run against the actual parser.

### (c) Browser gets one pane — wasm renders pane 0, multi is native-only

**Design.** Ship nothing on the wasm surface — but lock pane-0 as a *specified
contract*, not a shrug. Written down in three places: (1)
`docs/research/panes-feasibility.md` gains a pane-0 contract section — the
widget renders exactly one grid, and any future pane-addressed content MUST
degrade to pane 0; (2) `protocols/silk.md` notes multi-pane does not exist in
silk/1 and any future extension must ride the unknown-event-code MUST-ignore
seam so shipped players degrade lawfully; (3) the relay doc gains a new
pane-mirror section. One optional additive: a `panes: 1` key appended to the
existing `caps` reply — the codebase's stated discovery surface, whose keys
are append-only "so older clients keep parsing newer replies"
(`src/query_channel.rs:565-568`) — zero bindgen change, so hosts introspect
instead of inferring. Native multi-pane
proceeds under #22's placement model; the spine migrates under native pressure
first, and the browser wire/API fork is taken later against a migrated spine
plus #55/#47 evidence instead of today's singleton codebase.

**API stability.** Perfect: zero signature changes; `backend-wasm.js` and
`silk-player.js` untouched; no deprecation cycle. The known gaps (no resize,
no real teardown, one `start()` per page) stay exactly as documented with zero
added pressure — where (a) entangles disposal across panes and (b) silently
versions the semantic contract of the bytes inside `feed()`.

**Silk compat.** Perfect, trivially: no header change, strict 3-tuple stays, no
version bump, goldens untouched because the compiler emits nothing new. The
forward path is preserved rather than foreclosed — the MUST-ignore seam means
a future extension degrades to pane 0 on (c) widgets, which is the exact
degradation story (a) and (b) need anyway. The honest caveat lives in the
fatal risks.

**Wasm constraints.** Strongest of the three: one parser, one pair of
unbounded channels (1× the hidden-tab backlog hazard instead of N×), no
cross-pane stall-coherence question, no per-pane audio arbitration, one
WebGPU App with one constraint instead of N-panes-one-device.

**Fatal risks.**

- **Deferral calcifies:** the site player and spectator demos are ratty's
  primary distribution surface; if native multi-pane ships and the browser
  stays pane-0, web becomes permanently second-class and "revisit trigger"
  becomes a euphemism for "never." Mitigation is procedural only — if #57
  locks the staged shape, the lock must carry the revisit triggers and a
  named Stage-2 venue, with the fork already priced by #55.
- **(c) freezes silk, not just the widget:** `pane.*` 777 frames still
  *validate* today — they are decodable by the terminal's own shared parser,
  and `check_osc_777` passes every decodable non-sound/non-viz command
  untouched (`tools/silk/src/validate.rs:447-467`) — but they are
  superseded-pending and lower to nothing (no terminal renders panes), and a
  fourth tuple element fails cast parsing (`tools/silk/src/cast.rs:152`). So
  there is no way to author multi-pane content that *renders*: the freeze is
  doctrinal (#22's superseded-pending ruling), not validator-mechanical.
  Choosing (c) is a capability freeze on the art format, and it must be
  stated in the lock, not discovered later.
- **Relay pane-selection is real unpaid design work:** which pane a spectator
  of a multi-pane primary sees, focus-switch mid-session, and geometry changes
  with no external resize all need reset-notice/snapshot semantics written
  (the proposed relay-doc pane-mirror section). (c) is only "trivially
  compatible" with the relay if that section exists.
- **The spine migrates with no browser constraint in the room** — it may bake
  in shapes (per-pane presence namespaces, clocks, disposal) that make the
  eventual fork more expensive than today's pricing suggests. Charter item 1
  keeps this measured rather than assumed.
- **No multiple-widget escape hatch:** page-global `PENDING_QUERIES`, one
  `start()` per page, no teardown, one WebGPU App — "just embed two canvases"
  is not available, so pane-0-only is genuinely absolute until the fork.

## The recommendation

**A staged lock, offered to #57.** This is the doc's recommendation among the
three chartered options — #57's charter is the three-way choice, and #57
remains free to lock (a) or (b) outright; nothing here restructures its
decision.

**Stage 1 — lock option (c) now, as a specified contract.** Concretely:

1. The pane-0 contract section in `docs/research/panes-feasibility.md`: the
   widget renders exactly one grid; any future pane-addressed content MUST
   degrade to pane 0.
2. The `protocols/silk.md` note: multi-pane does not exist in silk/1; any
   future extension rides the unknown-event-code MUST-ignore seam so shipped
   players degrade lawfully.
3. The relay-doc pane-mirror section: optional `hello` `pane: {id, of}` field
   beside the existing `degraded` flag; pane-switch mid-session =
   `reset-notice` + snapshot, carried on the relay wire's out-of-band JSON
   control frames (`docs/research/relay-design.md:331-341`).
4. One additive `panes: 1` key on the existing `caps` reply — not a new op.
   `caps` is the codebase's stated discovery surface: first of the
   `SUPPORTED_OPS` (`src/query_channel.rs:55-70`), "the front door; everything
   else is additive" (`protocols/query.md:372`), surfaced as the site
   backend's own console example (`site/player/backend-wasm.js:101`), and its
   reply is one append-only map built in a single place
   (`src/query_channel.rs:565-568`) so older clients keep parsing newer
   replies. Zero bindgen change.

One piece of pre-work rides the lock rather than the fork: the pane-keyed
disposal fix — today's page-global `PENDING_QUERIES` `Drop` rejects every
in-flight query on disposal (`src/web.rs:57-63, 291-304`) — is chartered as
pre-work when either fork candidate is scheduled, whichever wins.

**Stage 2 — the (a)-vs-(b) fork.** This doc recommends deferring it, but the
deferral is offered, not imposed: the fork may be locked at #57 itself if
#56's placement-id shape plus #55's item-2/3 numbers make it decidable.
Otherwise #57 locks (c) with the deferral recorded as the decision, and the
fork re-enters at the named Stage-2 venue once the revisit triggers fire,
decided against #55's measured evidence and recorded spectator traffic rather
than paper pricing.

**If #57 locks the staged shape, its lock text should:** (i) record the
preconditions it verified and the post-lock revisit triggers as enforceable
conditions, with the Stage-2 venue named; (ii) state (c)'s honest cost — it
freezes multi-pane out of the silk art format, not just out of the widget;
(iii) treat the #25 question — is pane addressing analogous to the
identity/source context that lock keeps out-of-band? — as a genuine
interpretive call: rule on it with #55's item-3 evidence in hand and record
the ruling as its own decision, or explicitly leave it open for the Stage-2
venue; either way, it must not be settled as a side effect.

**Why.** At writing time, neither (a) nor (b) can ship: both need pane ids in
the namespaced style #22 ruled, whose concrete scheme #49 owns and #56 locks,
and both sit on the same prerequisite spine migration. But #57 does not
convene at writing time — by its own charter the #56-locked spine, the #55
findings, and #47's lessons doc are already on its table, so "not ready
today" is not by itself an argument for deferral there. The staged case is
narrower. Each fork option freezes a shape — (a) freezes host obligations on
the public wasm surface; (b) freezes a "private" frame grammar into golden
byte-identity and validator coupling once validator support and a multi-pane
golden land — so the fork should be taken exactly when the evidence makes it
decidable, and not sooner. Meanwhile (c)
is not a null choice: pane-0 degradation is the target *both* other options
must specify anyway (verified: the validator warn-and-ignores unknown event
codes, so an event-code extension lawfully reduces to pane 0 on shipped
players). Locking (c) first converts "fallback semantics reverse-engineered
later" into "primary contract specified now" — work on the critical path of
every future option — at zero API/silk/wasm cost, value that survives
whichever way the fork goes. The real (a)-vs-(b)
discriminator is narrow once (a) is staged as a superset: where demux lives
(every host's JS vs one crate-side scanner), where the versioned surface lives
(public per-pane obligations vs golden-frozen grammar), pane-0 corruption risk,
cross-pane ordering through hidden-tab wake, and base64 overhead at real rates
— all empirical questions the charter below measures and paper cannot. If
those numbers plus #56's id shape make the fork decidable at #57, this doc's
pricing supports taking it there; if they do not, locking (c) with the
deferral recorded as the decision is the honest floor, not a dodge. This
staging also honors the locked precedents: #22 said the browser question
travels undecided on purpose; #25's tier-B spectator runs unchanged under
Stage 1.

### Preconditions and revisit triggers

If #57 locks the staged shape, these split in two. The first three are
**preconditions #57's own charter already puts on its table** — it verifies
them at lock time; it does not wait on them:

1. **The placement-id scheme** — #22 ruled the style (namespaced ids,
   ownership) and is resolved; the concrete command family and placement
   semantics are #49's, locked into the spine at #56. Neither (a) nor (b) can
   freeze an id shape before this exists.
2. **#55's findings** — the two-seams coexistence result, the framing A/B,
   and the pane-0-corruption fuzz (charter items 1-3).
3. **#47's lessons doc** (`docs/research/relay-demo-lessons.md`) — including
   whether spectator demand for more than one browser grid showed up.

The **genuine post-#57 revisit triggers** — the conditions under which a
deferred fork re-enters at the Stage-2 venue — are, with the combination
stated explicitly (re-entry fires on **A AND (B OR C)**):

- **A. The singleton-spine migration lands** under native multi-pane
  pressure — the fork is then priced against a migrated spine; the migration
  must be reviewed for shapes that would silently bias or tax the browser
  fork.
- **B. A transmission author needs more than one grid in a cast** — the silk
  capability freeze bites in practice; fires on the first real request, not
  speculation.
- **C. Browser-spectator demand for more than one grid arrives after #47** —
  same standard: a real request, not speculation.

**Anti-calcification clause:** if B or C fires before A, the spine migration
gets scheduled as work rather than the fork remaining frozen — "revisit
trigger" must not become a euphemism for "never" on ratty's primary
distribution surface.

**The Stage-2 venue must be named in the lock.** This doc recommends a rider
on #57 in the #26 pattern — a named follow-up HITL lock whose charter is the
(a)-vs-(b) choice against the triggers above; failing that, an entry in map
#42's "Not yet specified" list with triggers B/C as its forcing function. A
deferral with no venue is how deferral calcifies.

### Rejected alternatives

- **(a) locked now** — cannot land before the placement-id lock (#49's
  command family, locked at #56; #22 ruled only the style); staged as a
  superset it has no signature break, but it exports demux and N-fold
  `drain_input` obligations to every host, and its remaining differentiators
  vs (b) are precisely what #55 should measure. Remains a live candidate — at
  #57 itself if the evidence decides it, or at the Stage-2 venue.
- **(b) locked now** — the "private protocol" freezes against the crate and
  every golden-tested transmission (3 of the 5 committed today) once
  validator `m`-frame support and the first multi-pane golden land; pane
  addressing may be ruled analogous to the identity context #25 keeps
  out-of-band (a #57 judgment call to make *with* #55's evidence, not a
  settled ban); and its worst failure mode — scanner bug leaking bytes into
  pane 0 — is unproven safe. Remains a live candidate with
  real advantages (zero host changes, total-order preservation, relay parity
  by construction) — at #57 itself if the evidence decides it, or at the
  Stage-2 venue.
- **(c) unconditional, without enforced triggers and stated costs** — bare
  deferral calcifies web into second-class, silently freezes the art format,
  and has no multiple-widget escape hatch. Acceptable only as the staged
  position above: costs stated in the lock, relay pane-mirror section written,
  fork priced by #55, preconditions verified at the lock, triggers
  enforceable, venue named.

## The #55 spike charter (a proposed re-charter)

#55 was chartered as the smallest wasm experiment that de-risks the browser
research's *leading option*, with the shape picked by this doc's
recommendation. Under the staged recommendation there is no single leading
option to de-risk, so that framing no longer fits — this section routes
through #55's own pick-what-the-research-makes-decisive clause and proposes
its items as the spike's de-risk set: a re-charter #55's claimer should adopt
consciously, not inherit silently. Items 1-3 are the decisive core
(coexistence, the A/B discriminator, the (b) kill-condition fuzz); items 4-8
are optional extensions the spike may drop under its smallest-experiment
bound. Each item names its pass/fail or its number, so
the output is evidence, not vibes. Item 1 is deliberately separated: it is the
shared cost, and should not be re-priced inside either framing shape.

1. **Spine coexistence (shared cost — price once, separately from framing).**
   Can two `TerminalRuntime::virtual_channel` seams (`src/runtime.rs:430-458`,
   already multi-instantiable) coexist in one wasm Bevy App behind a minimal
   resource shim that renders two grids in one canvas with correct input focus?
   The census of what resists pane keying already exists and is not re-run
   here: #48 (closed, PR #60) produced the authoritative four-fates result —
   114 symbols: 65 per-runtime, 32 screen-global, 15 routed to #52, 2 to
   #50 — and verified the seam constructor-grade. This item is confined to
   the two-seams-one-App coexistence question and the wasm-specific shim
   cost. Identical work under (a) and (b); prices revisit trigger A. Out of
   scope for the spike: pricing the full spine migration.
2. **Framing-layer A/B under real traffic (the discriminator).** Replay
   spectator `feed()` traffic — recorded #47 traffic if the demo has run;
   otherwise traffic captured from the #46 harness, or a synthetic corpus at
   #46-measured rates — through both shapes: JS-side demux (pane-tagged WS
   messages / silk event codes routed to per-pane feed) vs crate-side demux
   (mux-framed single `feed()`). Measure CPU per MB demuxed, added code size
   each side, wire/file bloat single-pane vs 2-pane (base64 ~33% for (b)),
   and rAF frame-time decode cost at realistic rates. This doc recommends #47
   record its spectator `feed()` traffic as a committed artifact, so this
   item and item 5 can run against real bytes.
3. **Pane-0 corruption kill-condition for (b).** Fuzz the crate-side frame
   scanner with frames torn at every byte offset (mid-APC header, mid-base64,
   across the C1 `0x9C` ST) and adversarial bare streams with near-miss
   prefixes. Pass/fail is absolute: zero bytes may ever leak into pane 0.
   Separately confirm the *current* shipped widget's vt100 ingest drops
   unknown `ratty;m` APCs cleanly with pane 0 byte-identical — the
   lawful-degrade claim is inferred from silk/RGP posture, not yet proven.
4. **Lawful degrade + golden byte-identity, empirically — no pane-aware
   compiler gets built inside a throwaway spike.** Hand-author `"o@x"`-style
   cast bytes and confirm today's player and raw tap reduce them to pane-0
   rendering (the validator side is already confirmed warn-and-ignore,
   `tools/silk/src/validate.rs:135-139` — confirm the player). Run a no-op
   recompile of the committed goldens with today's compiler — all five
   committed transmissions, not just the three the golden test covers
   (`tools/silk/src/compile.rs:1422-1424`, against its own "every committed
   transmission" comment at `:1416-1417`), closing that latent coverage gap
   in passing. Confirm `src/osc.rs` crate-parser work is required only if
   placement ops ride the `ratty:` 777 namespace (where the validator
   hard-errors on undecodable frames, `protocols/silk.md:243-246`) — (a)'s
   event codes and (b)'s APC frames both route around it.
5. **Hidden-tab economics at N panes.** Feed 2+ panes with the item-2 corpus
   for T seconds under rAF throttling, wake, and measure per-pane unbounded
   mpsc backlog growth rate, peak heap across N vt100 parsers plus demux
   buffer, and single-frame drain time (`pump_pty_output` drains everything
   in one wake frame, `src/systems.rs:180-239`) against frame budget. Also:
   does cross-pane byte ordering survive the wake under each shape ((b)
   preserves total order by construction; (a)'s N independent feeds guarantee
   nothing)?
6. **Reply attribution and `drain_input` at N panes.** With two panes emitting
   auto-replies (cursor reports, RGP support), do framed pane-N replies and
   bare pane-0 replies interleave losslessly through one output channel, and
   can a native primary demux and route them to the right PTY? Does the
   spectator harness's drain-and-discard obligation
   (`docs/research/relay-design.md:537`) survive unchanged? Quantify channel
   growth when one pane is never drained. Can `PENDING_QUERIES` /
   `try_resolve_pending` be pane-keyed so one pane's `close()` rejects only its
   own in-flight queries — and what is the minimal disposal fix *any*
   multi-pane option requires, given today's page-global Drop-rejects-all
   (`src/web.rs:57-63, 291-304`)? (Homed: the Stage-1 lock charters this fix
   as fork pre-work, whether or not the spike runs this item.)
7. **Loop reset across panes.** Today's reset (`ratty;g;d` + clear into one
   channel, `site/player/backend-wasm.js:108-114`) is global. Which
   deterministic per-pane sequence (close vs clear vs replay `open_pane`)
   survives a multi-loop soak without desync, is global reset correct for a
   mux stream, and can silk express the answer without new event codes?
8. **Relay pane-mirror ((c)'s Stage-1 composition question) — optional,
   simulated.** A multi-pane native primary does not exist and is post-map
   execution, so this runs as a simulated two-stream mirror driven by the
   harness — or is routed to the relay lane (#62/#47) outright. Mirror one of
   two synthetic pane streams to a web spectator via `feed()`, then switch
   the mirrored stream mid-session — do `reset-notice` + snapshot
   (out-of-band JSON control frames on the relay wire) suffice to avoid grid
   corruption, given ratty has no external resize and pane geometries may
   differ?

## How the relay web-spectator path composes

The tier-B lock (#25) is `ws.onmessage → session.feed(bytes)`, binary frames
for gated bytes, JSON text frames for out-of-band control
(`docs/research/relay-design.md:95, 331-341`). Per option:

- **With (a):** single-pane spectating is untouched (`feed()` aliases the
  primary pane). Multi-pane puts topology in the existing out-of-band JSON
  text frames and a short pane-id prefix on binary frames — WS gives message
  boundaries for free, so harness demux is a few lines of JS routing to
  `pane.feed()`, and an escape sequence torn across messages within one pane
  lands in that pane's stateful parser exactly as today. No crate-side
  scanner, no protocol inside the byte stream — but the harness inherits
  per-pane `drain_input` and disposal obligations.
- **With (b):** parity by construction — multi-pane frames are just more bytes
  through the one wire and the one `feed()`; the harness changes not at all.
  The cost surfaces elsewhere: the native primary must demux framed pane-N
  auto-replies out of `drain_input`, and the interpretive question is live —
  the relay wire keeps identity and control metadata out-of-band (glossed in
  its doc as "structured context", `docs/research/relay-design.md:336-337`),
  while (b) puts pane routing in-band. Whether pane routing is analogous to
  the identity context #25 forces out-of-band is #57's call.
- **With (c):** trivially compatible *once the pane-mirror section is written*
  (Stage-1 deliverable 3): the harness picks one pane's byte stream (pane 0 or
  focused) from a multi-pane native primary and declares it in `hello` via an
  optional `pane: {id, of}` field beside `degraded`; pane-switch mid-session
  requires `reset-notice` + snapshot because ratty has no external resize and
  geometry may differ (`docs/research/relay-design.md`, late-join snapshot
  semantics). That is a relay-doc edit, not a widget change.
- **Under the recommendation:** if #57 locks the staged shape, Stage 1
  changes nothing on the wire — #47's demo and the #46 `ws.onmessage →
  feed()` proof run as-is; the pane-mirror section is written as contract,
  exercised (simulated) by charter item 8. The fork — at #57 if decidable
  there, else at the Stage-2 venue — then chooses between (a)'s harness-side
  routing and (b)'s wire-transparent framing with item 2's numbers in hand.

## What this deliberately does NOT decide (the #57 list)

Carried to #57 — which decides each, or records its deferral as a decision:

- **The (a)-vs-(b) fork itself.** This doc recommends the staged shape but
  offers it: #57 may lock the fork outright if the evidence on its table —
  #56's id shape, #55's items 2/3, #47's lessons — makes it decidable. If #57
  defers, the deferral is the recorded decision and the Stage-2 venue is
  named in the lock. This doc prices; it does not choose the fork.
- **The #25 interpretive call** — whether pane addressing is analogous to the
  identity/source context #25's lock keeps out-of-band (which would kill (b))
  or transport-layer routing at the ANSI layer (which permits it). #25's lock
  text is scoped to identity and trust; the broader "structured context"
  reading is the relay doc's gloss (`docs/research/relay-design.md:336-337`).
  #57 makes the call with charter item 3's evidence on the table, or
  explicitly leaves it open for Stage 2 — either way as a recorded decision,
  not a side effect.
- **The pane-id shape.** #22 ruled the style (namespaced ids, ownership); the
  concrete scheme is #49's, locked into the spine at #56. Nothing here may
  freeze an id scheme first.
- **Effects/presence screen-global vs per-pane** — owned by open ticket #52
  (screen-global vs per-runtime arbitration), where #48's census already
  routed 15 symbols; the feasibility doc's fog row
  (`docs/research/panes-feasibility.md:39`) is the historical statement of
  the same question. Revisit trigger = #52's resolution; the presence/sound
  screen-scoping in every option above must be re-checked against it.
- **The spine-migration design** (per-pane components vs slotmap resource,
  per-pane clocks, disposal shape) — native work under #22's placement model,
  reviewed at revisit trigger A for browser-fork bias.
- **Anything #55 measures.** The charter's eight items are questions, not
  conclusions; where this doc leans (e.g. (b)'s ordering coherence, (a)'s
  disposal fix), the lean is falsifiable by the spike and says so.
- **#57's inputs are formal:** it consumes this doc, the #55 findings,
  `docs/research/relay-demo-lessons.md` (#47), and the #56-locked spine
  (which consumes #49) — the lock happens with all four on the table, not
  before.
