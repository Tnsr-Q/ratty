# Upstreaming map (fork → orhun/ratty)

This fork stays **upstream-clean**: everything that belongs to the site
lives in new directories (`site/`, `transmissions/`, `tools/`,
`graphify-out/`, `.claude/`), while changes to ratty core are kept
PR-shaped — isolated commits with tests and protocol docs — so they can be
offered upstream. `website/**` and `.github/workflows/website.yml` are
never touched. This document is the map of what to offer, in what order,
and how to frame it. Actual submission is the repo owner's call.

## PR 1 — virtual terminal transport + web build (the wasm seam)

**What**: `TerminalRuntime::virtual_channel()` (unbounded byte channel in
place of the PTY), `AppConfig::from_toml_str`, platform-gated clipboard and
path discovery, embedded fallback fonts, `[target.'cfg(...)']` dependency
tables, and `src/web.rs` (wasm-bindgen session: `feed`/`drain_input`/
`set_*`). Together these make ratty embeddable — and compile to
wasm32 + WebGPU, which is how https://tnsr-q.github.io/ratty/ runs the real
terminal in a browser.

**Framing**: lead with testing/replay/demos (a virtual transport makes the
runtime unit-testable and drives recorded demos), present the web build as
the headline application. Mention the working Pages deployment as proof.

**Also riding along**: `ratatui = { default-features = false }` in
Cargo.toml — the direct dependency was unused (everything goes through the
`parley_ratatui` re-export) and default features pull crossterm, which
doesn't build on wasm. Standalone and upstreamable on its own if PR 1 is
split.

## PR 2 — RGP v2: the `c` stage/camera verb + per-object animation

**What**: the protocol work on this fork's `main`:

- `feat(rgp): parse the v2 stage verb and per-object animation keys`
- `feat(scene): apply RGP stage updates with engine-side tweening`
- `feat(rgp): per-object spin, bob, bobamp and phase animation`
- `docs(silk): rewrite the RGP implementation truths for v2` (the
  `protocols/graphics.md` changes ride the first three)

**Framing**: in-band control of presentation mode/warp/camera is on the
creator's stated roadmap; this delivers it with engine-side `dur`/`ease`
tweening, boring interrupt rules (user input always wins), full v1
back-compat (unknown-verb tolerance already in the protocol's design), a
bit-exact v1 animation path locked by tests, and the support reply bumped
to `v=2` with appended capability keys. The `apply_stage_mode_change`
helper also fixes an absolute-vs-toggle quirk in the web `set_mode`.

**Depends on**: PR 1 only for `src/web.rs` touches (one small refactor);
the parser/scene work is independent and could be reordered ahead.

## PR 3 — in-memory GLB asset source (not yet implemented)

**What**: replace `model.rs`'s materialize-GLB-payloads-to-disk with a
Bevy `MemoryAssetReader` (`bevy::asset::io::memory::Dir`) source. Fixes a
real native concern (two ratty instances can race on the cache path) and
would unlock GLB payloads on wasm (the web build currently supports
OBJ/STL payloads, which parse fully in memory).

**Status**: designed, not built. Build it in the fork first; the seam is
the `std::fs::write(&asset_file, …)` sites in `src/model.rs` (currently
around lines 283-290 and 422-428) and the asset-source registration in
`main.rs`.

## Good-first-issue report

The root README calls the widget crate `ratatui-rgp`, but the crate in
`widget/` is named `ratatui-ratty` — a docs inconsistency worth an issue
(found during corpus extraction).

## Not for upstream

`site/`, `transmissions/`, `tools/silk/`, `graphify-out/`, `.claude/`,
`.github/workflows/web.yml`, `assets/fonts/` (embedded-font licensing is
fine — Bitstream Vera — but the font choice is a fork concern until PR 1
lands). `tools/silk` could eventually become its own crate/repo, and
`src/rgp.rs` could be extracted into a `ratty-protocol` crate shared by
terminal and tools — worth proposing in the PR 2 discussion.
