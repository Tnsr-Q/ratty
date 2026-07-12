# Avatar tech survey

Research asset for [wayfinder ticket #14](https://github.com/Tnsr-Q/ratty/issues/14)
(map [#10](https://github.com/Tnsr-Q/ratty/issues/10)). Findings and options only —
the design decision belongs to the avatar design ticket (#23).

## What exists today (more than expected)

- **glTF scene loading is live.** `ObjectSource::Gltf` loads via
  `asset_server.load(GltfAssetLabel::Scene(0).from_asset(...))`
  (`src/model.rs:137-141`), alongside OBJ and STL. Assets can be embedded
  (`RustEmbed` on `assets/objects/`) or loaded from disk (native).
- **A real PBR pipeline.** Inline objects are `Mesh3d` +
  `MeshMaterial3d<StandardMaterial>` — skinned glTF meshes are compatible
  with this path in principle.
- **`gltf_animation` is already in the Bevy feature set** (`Cargo.toml`) —
  compiled in, currently unused. The runway to animated glTF is shorter than
  the ticket assumed.
- **The command surface is committed.** `AvatarSet { model, position }`,
  `AvatarGesture { gesture }`, `AvatarSpeak { text }`, `AvatarHide`
  (`src/osc.rs:332-351`), all log-only. `position` is a screen-anchored
  string, `model` a name — implying a curated model registry, not arbitrary
  paths (good for both safety and asset budget).
- **A presence layer already exists** (`src/effects.rs`, M3.2): thinking,
  confidence, mood. An avatar is, semantically, a *skin over presence* —
  `docs/ecosystem-vision.md` frames embodiment of internal state as the
  value, "a glTF character is one possible skin over it, not the point."

## Animation in Bevy 0.19

From docs.rs for 0.19: `AnimationPlayer` plays `AnimationClip` assets;
`AnimationTargetId` + `AnimatedBy` bind clip curves to entities;
`RepeatAnimation` controls looping. The docs excerpt surveyed did **not**
surface `AnimationGraph`/blending API for 0.19 — the API has been reworked
across recent Bevy versions, so the design ticket must pin the exact 0.19
idiom against the `bevy_animation` source before committing to blended
gestures. Single-clip playback (idle, wave) is safely within the documented
surface. Skinned meshes work on WebGPU/wasm in Bevy.

## Speech options

| Option | Native | Browser | Cost / honesty |
| --- | --- | --- | --- |
| Text presentation (speech bubble / typewriter overlay + presence glow while "speaking") | yes | yes | No new deps; honest; works today |
| OS/Web TTS (`tts` crate → OS voices; `web_sys` SpeechSynthesis on wasm) | yes | yes (user-gesture gated) | Real audio, zero assets; voice quality varies by platform; two thin backends |
| Local neural TTS (piper-class) | yes | no (model size) | Best quality; ships tens-of-MB models; native-only |

Lipsync: true visemes need phoneme timing from the TTS engine — not
portably available across the options above. The honest tier is
amplitude/event-driven mouth or no mouth at all: drive the existing presence
effects for the duration of speech. Do not fake visemes.

## Asset budget

Deployed Pages site is the constraint. A low-poly rigged glTF (1–5 MB,
embedded via the existing `RustEmbed` pattern or fetched per model name)
is realistic; sculpted/hi-poly characters are not. Compression support
(Draco/meshopt) in `bevy_gltf` 0.19 was not verified in this survey — check
in the design ticket if model size becomes the deciding factor.

## Options for the design ticket (#23)

- **A. Presence-plus (no mesh).** `AvatarSet/Hide` toggle a named visual
  identity built from the existing effects layer; `AvatarSpeak` = text
  presentation + presence glow; `AvatarGesture` = named effect choreography.
  Cheapest; fully honest; browser-equal.
- **B. Static glTF mascot.** A curated model registry (embedded low-poly
  models); gestures are transform animations (bob, tilt, spin) — no
  skinning, no animation-API risk; `AvatarSpeak` as in A.
- **C. Skinned + animated avatar.** Full `AnimationPlayer` clips (idle /
  gesture set), optional TTS. The real thing, gated on pinning the 0.19
  animation idiom and an asset budget.

Recommendation to carry into #23: **B first, C as the follow-on** — B ships
an avatar with zero animation-API risk while C's unknowns (0.19 blending
idiom, model budget) get pinned; A's speak/gesture semantics are reused by
both, so nothing in B is throwaway. Speech: text presentation first, Web/OS
TTS as an additive option behind the same command.
