# Sound survey

Research asset for [wayfinder ticket #15](https://github.com/Tnsr-Q/ratty/issues/15)
(map [#10](https://github.com/Tnsr-Q/ratty/issues/10)). Findings and options only —
the design decision belongs to the sound design ticket (#24).

## What exists today

- **Nothing.** No audio dependency anywhere: Bevy is built with
  `default-features = false` and the feature list (`Cargo.toml:31-50`) does
  **not** include `bevy_audio`. No rodio/kira in the tree. Silk has no audio
  step.
- **The command surface is deliberately small.** One command:
  `Sound { kind: String, loop_sound: bool }` (`src/osc.rs:323-330`),
  log-only. `kind` is a *named sound*, not a file path — the enum frames
  sound as semantic events (chime, alert, ambient…), which matches the
  ecosystem vision's rule that sound gets a semantic basis, not decoration.
  Note the gap: with `loop_sound: true` there is no stop/volume command —
  the design must either add commands (enum change, CLI change) or scope
  looping sounds as replace-on-next-Sound.

## Runtime options

| Option | Bevy 0.19 fit | Control surface | wasm | Notes |
| --- | --- | --- | --- | --- |
| `bevy_audio` (feature flag, rodio-backed) | built-in | play/volume, coarse; stopping loops is awkward | works | Smallest diff: one feature flag |
| `bevy_kira_audio` 0.26 | explicit 0.19 support (repo compat table) | channels, pause/stop/volume/tweens | web builds supported | Requires `bevy_audio` **off** — ratty already satisfies this |
| Hand-rolled per-platform (rodio native + `web_sys` AudioContext) | n/a | maximal | yes | Two implementations to keep honest; avoid |

`loop_sound` management (start a loop, stop it later) is exactly the case
where `bevy_audio`'s coarse control gets painful and kira's channels are
comfortable.

## Browser constraint: autoplay policy

Browsers suspend `AudioContext` until a user gesture. Implication: the
first `Sound` command before any click/keypress on the Pages site will
silently do nothing unless the widget resumes the context on first
interaction. The design must include a resume-on-gesture hook and decide
the honest behavior for sounds requested pre-gesture (drop vs queue-one).
Whether the site's current transmission flow already begins with a user
click was not verified in this survey — check in the design ticket, since
it may satisfy the gate for free.

## Asset strategy

- OSC-777 payloads cannot reasonably carry audio bytes; `kind` names must
  resolve to **bundled assets** — the `RustEmbed` pattern used for
  `assets/objects/` extends naturally to `assets/sounds/` with small
  `.ogg` files (kira: ogg/mp3/flac/wav).
- Transmissions: a silk `sound` step (compiling to the `Sound` command) is
  the natural authoring surface, mirroring how the `ai` step lowered onto
  effects. Same named-kind vocabulary in both paths — no transmission-only
  audio formats.
- Upstream-clean: put audio behind a cargo feature so the upstream PR shape
  stays clean and audio-less builds keep working.

## Options for the design ticket (#24)

- **A. `bevy_audio` minimal** — feature flag on, tiny embedded kind set,
  `loop_sound` semantics = replace-on-next-Sound. Smallest diff; weakest
  control; loop stopping stays awkward.
- **B. `bevy_kira_audio`** — one new dependency, channels give real
  start/stop/volume; same embedded kind set; web supported. Add a
  `SoundStop`-shaped command (enum + CLI) or a reserved `kind: "stop"`.
- **C. Defer audio, keep honest stubs** — if the naming ceremony / site
  polish outranks sound, the log-only stub is already honest.

Recommendation to carry into #24: **B** — the committed `loop_sound` field
already implies more control than `bevy_audio` offers comfortably, kira 0.26
matches Bevy 0.19 with web support, and ratty's feature set is already
kira-compatible (`bevy_audio` off). Pair it with a small semantic kind set
(chime, alert, ambient, pulse) embedded as ogg, a silk `sound` step, and an
explicit resume-on-gesture rule for the site.
