# First spectator demo, live — what it proved and what it strained

Execution asset for [wayfinder ticket #47](https://github.com/Tnsr-Q/ratty/issues/47)
(map [#42](https://github.com/Tnsr-Q/ratty/issues/42)), the demo tier B exists
for. Run 2026-08-04, at-screen with @Tnsr-Q. **This is a record, not a
recommendation** — the browser-story grilling
([#57](https://github.com/Tnsr-Q/ratty/issues/57)) blocks on this ticket so it
can cite real `feed()` traffic instead of spike claims; everything it may want
to cite is here.

## The run

One real session multicast to two simultaneous read-only spectators — one
native, one web — including a late join, carrying the full presence family
live. The cast, exactly as executed:

- **Primary**: `ratty -e ratty-relay host --session demo -- bash -c
  'sleep 30; …; PRESENCE_DEMO_ACK=1 presence-demo.sh; exec /bin/zsh'` —
  the committed Gate 1 driver (`tools/ratty-ai/examples/presence-demo.sh`),
  self-starting inside the hosted shell, in `--ack` capture mode (the mode
  the driver documents as "the mode that proves the traffic for the relay").
- **Web spectator** (`tools/ratty-relay/examples/web-spectator.html`,
  [PR #82](https://github.com/Tnsr-Q/ratty/pull/82)): connected from the
  session's start in a real browser — the wasm terminal on WebGPU, fed by
  the relay's binary frames through `RattySession.feed()`.
- **Native spectator**: `ratty -e ratty-relay watch ws://127.0.0.1:7877`,
  launched deliberately mid-run as the **late joiner**, catching up through
  the replay-ring snapshot between the `snapshot-begin`/`snapshot-end`
  brackets.

The same traffic had run once before — Gate 1
([#41](https://github.com/Tnsr-Q/ratty/issues/41)) drove it into a single
ratty a week earlier. That is the point: the driver is the relay's
requirements capture, so the demo replays known traffic through a new
transport. From the primary window the two runs are visually identical;
every new claim below lives on the spectator surfaces.

## What the demo proved (first time live for each)

1. **The stage-2 presence engine ran against a real primary.** #62 shipped
   it with 226 unit tests and no live run. Here the 778 poll → gated
   synthesis → mirrored `r0.*` rosters path carried carets, name labels,
   and notes onto both spectator surfaces, tracking the primary through
   the acts — confirmed at-screen.
2. **The wasm terminal played relay traffic through `feed()`.** The #55
   spike proved `feed()` mechanics on a throwaway branch; nothing committed
   could do it until PR #82. The harness booted clean on real WebGPU
   (BrowserWebGpu adapter, empty error console) and rendered the session
   live. The `site/player/backend-wasm.js` glue pattern it mirrors held
   without modification.
3. **Late join over the replay ring works end-to-end**, twice: the web
   harness against a two-minute-old session in the pre-flight smoke test
   (hello → snapshot brackets → live), and the native watcher mid-run in
   the sitting itself.
4. **The #81 label scrim renders on every surface.** The gate run a week
   ago is what *surfaced* the bare-label bug (#78); this run is the first
   time labels-over-text rendered legibly on the primary, the native
   spectator, and the wasm build in one frame of reference.
5. **Rejections stay caller-local.** Act 7's deliberate duplicate join
   surfaced primary-side only (`WARN ratty::presence: user.join rejected:
   participant 'alice' exists`, primary log 07:32:28); the relay's
   control-plane excision kept every spectator surface clean, as #25's
   scope demands.

## What it strained — findings, in severity order

1. **Session end takes the native spectator's terminal down with it.**
   When the primary closed, the watcher's process tree followed: `watch`
   exits its read loop on socket close, and the spectator ratty's
   AppExit-on-any-disconnect behavior (`src/systems.rs:231-237`, the #54
   finding) closes the *spectator's* window. A spectator whose window
   vanishes because someone else's session ended is the single-terminal
   assumption observed relay-side. Already Phase-1 build-order material
   (#56 decision 20); recorded here as the first live sighting from the
   spectator's chair.
2. **A stray `\x1b\` (bare ST) arrives at relay-host session start.**
   `WARN ratty::runtime: unhandled terminal escape sequence: \x1b\` fired
   at startup in both independent casts (05:24:51 and 07:31:23), only when
   the session runs under `ratty-relay host`. Something on the interposed
   seam emits an unpaired string terminator once, at startup. Benign in
   both runs — but the #55 spike established that terminator handling on
   this parser is where the bodies are buried, so it should not stay
   unexplained. Filed as
   [#84](https://github.com/Tnsr-Q/ratty/issues/84).
3. **The committed wasm bundle is a stale-binary hazard.** `site/pkg/`
   held a Jul 27 bundle — pre-#81 — and the demo's first pre-flight caught
   it: the web spectator would have rendered pre-scrim labels and read as
   a rendering defect. This generalizes the gate-driver hazard ("a binary
   predating the organ") to the web surface: **`site/build-wasm.sh` runs
   before any at-screen web claim, every time.**
4. **The three-surface choreography is operator-error-prone by hand.** The
   first attempt at the sitting ran the driver in the wrong shell — OSC
   777 into the outer terminal is silently swallowed, and nothing points
   at the mistake. The reproducible form is the scripted cast used above
   (the hosted command self-starts the driver after a fixed delay, then
   `exec`s a shell so the session outlives the script). Recorded as the
   canonical invocation.
5. **Harness friction, minor:** the Connect button is one-shot per page
   load (`{once: true}`), so a click against a not-yet-running relay
   spends it — recovery is a page reload. Cosmetic; fix if the harness
   ever graduates past example status.

## What remains unmeasured — stated so it is not mistaken for verified

- **The blocking-778 inward pressure** (#62's filed strain: ratty answers
  the poll with a blocking write onto the relay's stdin). No primary stall
  was reported during the sittings, but nothing instrumented it either —
  "not observed" here is weaker than "bounded," and the strain stays open
  exactly as #62 filed it.
- **The 250 ms poll sampling floor** (a join/leave pair inside one
  interval is invisible to spectators) was not deliberately exercised; the
  driver's pacing never produces one.

## For #57, on a plate

The evidence the browser-story grilling blocked for: real relay traffic
through the real `feed()` surface, committed harness, live WebGPU, late
join over the ring, presence mirrored via the out-of-band
`presence-mirror` control frames with connection-ephemeral teardown
(`watch.rs` and the harness implement the same trade independently). The
`site/` player glue (`backend-wasm.js`) needed zero changes to coexist
with a second `feed()` consumer pattern — the pane-0 contract #53 staged
for #57 starts from a surface this demo just exercised.
