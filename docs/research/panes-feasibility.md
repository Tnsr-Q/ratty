# Panes feasibility study

Research asset for [wayfinder ticket #13](https://github.com/Tnsr-Q/ratty/issues/13)
(map [#10](https://github.com/Tnsr-Q/ratty/issues/10)). Findings and options only —
the design decision belongs to the panes design ticket (#22).

## What exists today

- **One terminal, singleton-shaped.** `TerminalRuntime` is a Bevy `Resource`
  (`src/runtime.rs:207`) owning one transport (native PTY via `portable_pty`,
  or a virtual byte channel on wasm), one `Parser<TerminalParserCallbacks>`,
  one writer, one reader thread. `TerminalSurface`, `TerminalPresentation`,
  redraw state, selection, and cursor state are likewise singletons.
- **A clean transport seam already exists.** The runtime docs state it
  plainly: everything downstream consumes only `try_recv` / `write_input` /
  `parser`, so PTY and virtual channel are interchangeable
  (`VirtualTerminalHost` = `feed_tx` + `input_rx`). Multiplexing means
  N instances of this seam, not a new abstraction.
- **The command surface is already committed.** `RattyAiCommand` carries
  `SplitPane { direction, ratio }`, `FocusPane { pane: u8 }`,
  `ResizePane { pane, width, height }`, `ClosePane { pane }`
  (`src/osc.rs:242-268`) — a tmux-like binary-split tree with `u8` pane ids.
  Currently log-only.

## The core refactor

Pane = (transport + parser + grid surface + presentation quad + redraw
state). Everything singleton-shaped must become per-pane. Inventory of
affected singletons (from the knowledge graph + reads):

| Today | Under panes |
| --- | --- |
| `TerminalRuntime` resource | Per-pane component (or a `Panes` resource holding a slotmap of runtimes) |
| `TerminalSurface`, `TerminalPresentation` | Per-pane entity + quad |
| Keyboard input → the one writer | Focus-routed to the focused pane's writer |
| Mouse hit-testing / selection | Pane-local coordinates after quad picking |
| Cursor model | Per-pane (or focused-pane-only) |
| Inline objects (`TerminalInlineObjects`) | Already per-terminal in shape; needs a pane owner |
| Effects / presence (`src/effects.rs`) | Decide: screen-global vs per-pane (feeds the cross-organ-arbitration fog on the map) |
| OSC-777 AI channel | Arrives via one pane's stream; commands may target other panes — needs an addressing rule |

Native is the easy half: `portable_pty` happily opens N PTYs; one reader
thread per pane matches the current pattern.

## The browser is the hard half

Wasm has no PTY; the widget is fed via `feed()` into **the** virtual
channel. Options for what a pane means there:

- **Per-pane feeds** — widget API becomes `feed(pane_id, bytes)`; the host
  (site JS, transmissions player) addresses panes explicitly. Requires a
  widget API break and a silk framing extension for multi-pane
  transmissions.
- **In-band mux framing** — one stream carries pane-addressed frames
  (tmux-control-mode style). No API break, but invents a private protocol
  inside the byte stream.
- **Browser gets one pane** — panes are native-only initially; wasm renders
  pane 0. Honest, cheap, defers the question.

## Prior art (architecture lessons, not dependencies)

- **zellij** — client/server split; panes are first-class server objects;
  rendering is a client concern. Lesson: keep mux state out of the renderer.
- **wezterm** — a mux server owning a pane tree of `Domain`s; local and
  remote panes are the same trait. Lesson: the transport seam ratty already
  has is the right cut for "pane," and remote panes fall out of it later
  (relevant to collaboration).
- **tmux control mode** — panes multiplexed in-band over one channel with a
  text protocol; proof the in-band-framing option works but shows its cost
  (a protocol to version and parse).

## Cost estimate

This touches most systems in `src/systems.rs` plus input, mouse, scene, and
the widget API — the largest organ by blast radius. Phased shape if pursued:

1. N virtual terminals native-only, no layout (prove per-pane runtime).
2. Split-tree layout + focus routing + `SplitPane`/`FocusPane` lowering.
3. Browser story (whichever option the design picks).
4. 3D layout play (panes as separate quads in space — the interesting part).

Phases 1–2 alone look like multiple sessions each.

## Options for the design ticket (#22)

- **A. Full pane tree** — the committed OSC surface, native + browser.
  Recommend graduating panes to its own wayfinder map if chosen; it is too
  large for one design-plus-build pass.
- **B. Fixed split (2 panes max), native-first** — lowers all four OSC
  commands against a degenerate tree; browser renders pane 0. Fits the
  existing milestone rhythm; leaves A's door open.
- **C. Defer** — park panes until the query channel and collaboration
  designs clarify the multi-agent story (panes-as-agents may reshape what a
  pane even is — see `docs/ecosystem-vision.md`, "many agents means many
  terminals in one space").

Recommendation to carry into #22: **B**, with an explicit check against the
ecosystem vision before committing — if terminals-as-agents is the real
destination, B's split-tree may be the wrong shape and C is honest.
