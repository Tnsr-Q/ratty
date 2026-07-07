# Graph Report - /home/user/ratty  (2026-07-06)

## Corpus Check
- 53 files · ~73,303 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 1031 nodes · 2323 edges · 32 communities (30 shown, 2 thin omitted)
- Extraction: 94% EXTRACTED · 5% INFERRED · 0% AMBIGUOUS · INFERRED: 126 edges (avg confidence: 0.84)
- Token cost: 399,997 input · 0 output

## Community Hubs (Navigation)
- [[_COMMUNITY_Frame Update Systems|Frame Update Systems]]
- [[_COMMUNITY_Rubiks Cube Demo|Rubiks Cube Demo]]
- [[_COMMUNITY_Inline Objects & RGP Handling|Inline Objects & RGP Handling]]
- [[_COMMUNITY_Configuration & Terminal Surface|Configuration & Terminal Surface]]
- [[_COMMUNITY_Project & Protocol Docs|Project & Protocol Docs]]
- [[_COMMUNITY_Keyboard Input & Bindings|Keyboard Input & Bindings]]
- [[_COMMUNITY_Vello Scene Exchange|Vello Scene Exchange]]
- [[_COMMUNITY_TempleOS Document Editor Demo|TempleOS Document Editor Demo]]
- [[_COMMUNITY_ratatui-ratty Widget API|ratatui-ratty Widget API]]
- [[_COMMUNITY_Mouse Input & Selection|Mouse Input & Selection]]
- [[_COMMUNITY_PTY Runtime|PTY Runtime]]
- [[_COMMUNITY_3D Model Loading|3D Model Loading]]
- [[_COMMUNITY_Debug Cell Renderer|Debug Cell Renderer]]
- [[_COMMUNITY_Draw Demo|Draw Demo]]
- [[_COMMUNITY_Ratty Brand Imagery|Ratty Brand Imagery]]
- [[_COMMUNITY_Scene Setup Params|Scene Setup Params]]
- [[_COMMUNITY_App Entry & Window Icon|App Entry & Window Icon]]
- [[_COMMUNITY_Website Demo Carousel|Website Demo Carousel]]
- [[_COMMUNITY_Mobius Transition|Mobius Transition]]
- [[_COMMUNITY_Present Material Shader|Present Material Shader]]
- [[_COMMUNITY_Scene Setup & Plane Meshes|Scene Setup & Plane Meshes]]
- [[_COMMUNITY_3D Camera View Control|3D Camera View Control]]
- [[_COMMUNITY_Apple Touch Icon Branding|Apple Touch Icon Branding]]
- [[_COMMUNITY_TempleOS Heritage|TempleOS Heritage]]
- [[_COMMUNITY_Big Rat Example|Big Rat Example]]
- [[_COMMUNITY_512px Favicon Branding|512px Favicon Branding]]
- [[_COMMUNITY_32px Favicon Branding|32px Favicon Branding]]
- [[_COMMUNITY_2D Present Pipeline|2D Present Pipeline]]
- [[_COMMUNITY_16px Favicon Branding|16px Favicon Branding]]
- [[_COMMUNITY_Build Script|Build Script]]
- [[_COMMUNITY_Code of Conduct|Code of Conduct]]

## God Nodes (most connected - your core abstractions)
1. `TerminalRuntime` - 39 edges
2. `TerminalSurface` - 35 edges
3. `AppConfig` - 28 edges
4. `TerminalInlineObjects` - 25 edges
5. `KeyboardSystemParams` - 25 edges
6. `MobiusTransition` - 25 edges
7. `SyncInlineParams` - 24 edges
8. `RubiksCube` - 24 edges
9. `Ratty Terminal Emulator` - 22 edges
10. `TempleEditor<'a>` - 21 edges

## Surprising Connections (you probably didn't know these)
- `Bug Report Template` --semantically_similar_to--> `Contribution Guide`  [INFERRED] [semantically similar]
  .github/ISSUE_TEMPLATE/bug_report.md → CONTRIBUTING.md
- `Contribution Guide` --semantically_similar_to--> `CI Format Job`  [INFERRED] [semantically similar]
  CONTRIBUTING.md → .github/workflows/ci.yml
- `Contribution Guide` --semantically_similar_to--> `CI Clippy Lint Job`  [INFERRED] [semantically similar]
  CONTRIBUTING.md → .github/workflows/ci.yml
- `CRT Terminal Illustration` --conceptually_related_to--> `Ratty Terminal Emulator`  [INFERRED]
  website/assets/images/ratty-social-card.png → README.md
- `Rat Theme` --conceptually_related_to--> `Ratty Terminal Emulator`  [INFERRED]
  website/assets/images/ratty-social-card.png → README.md

## Import Cycles
- 1-file cycle: `src/keyboard.rs -> src/keyboard.rs`
- 1-file cycle: `src/mouse.rs -> src/mouse.rs`
- 2-file cycle: `src/mouse.rs -> src/terminal.rs -> src/mouse.rs`

## Hyperedges (group relationships)
- **RGP Verb Command Surface** — protocols_graphics_support_query, protocols_graphics_register_object_asset, protocols_graphics_place_object, protocols_graphics_update_object, protocols_graphics_delete_object [EXTRACTED 1.00]
- **cargo-dist Release Pipeline** — _github_workflows_release_plan, _github_workflows_release_build_local_artifacts, _github_workflows_release_build_global_artifacts, _github_workflows_release_host, _github_workflows_release_announce [EXTRACTED 1.00]
- **GPU Text Rendering Stack** — readme_vt100, readme_ratatui, readme_parley_ratatui, readme_vello, readme_bevy, readme_wgpu, readme_rendering_pipeline [EXTRACTED 1.00]

## Communities (32 total, 2 thin omitted)

### Community 0 - "Frame Update Systems"
Cohesion: 0.05
Nodes (97): AppExit, AssetMut, ChildOf, CursorTransformQuery, Mesh3d, MessageWriter, ParleyBackend, Quat (+89 more)

### Community 1 - "Rubiks Cube Demo"
Cohesion: 0.06
Nodes (38): Add, Event, Mul, Output, VecDeque, ActiveTurn, Axis, CubeObj (+30 more)

### Community 2 - "Inline Objects & RGP Handling"
Cohesion: 0.05
Nodes (58): CB, apc_end(), apply_rgp_update(), apply_vec3_update(), InlineAnchor, InlineObject, InlineStyle, KittyInlineObject (+50 more)

### Community 3 - "Configuration & Terminal Surface"
Cohesion: 0.05
Nodes (53): BTreeMap, Cell, D, Error, AppConfig, BindingsConfig, CursorAnimationConfig, CursorConfig (+45 more)

### Community 4 - "Project & Protocol Docs"
Cohesion: 0.05
Nodes (68): Sponsorship Funding Channels, Bug Report Template, Feature Request Template, Pull Request Template, Publish on crates.io Workflow, CI Cross-Target Check Job, CI Clippy Lint Job, CI Format Job (+60 more)

### Community 5 - "Keyboard Input & Bindings"
Cohesion: 0.08
Nodes (43): ButtonInput, Clipboard, FromWorld, Key, KeyboardInput, KeyCode, NonSendMut, PhantomData (+35 more)

### Community 6 - "Vello Scene Exchange"
Cohesion: 0.08
Nodes (39): Extract, GpuImage, GpuRenderer, PenikoColor, RenderAssets, RenderDevice, RenderQueue, Scene (+31 more)

### Community 7 - "TempleOS Document Editor Demo"
Cohesion: 0.10
Nodes (25): Protocol, discover_obj_assets(), DocCell, emit_sequence(), initial_lines(), main(), ObjectPlacement, place_at_anchor() (+17 more)

### Community 8 - "ratatui-ratty Widget API"
Cohesion: 0.09
Nodes (13): Into, ObjectFormat, RattyGraphic<'a>, RattyGraphicSettings, RattyGraphicSettings<'a>, Buffer, Cow, Option (+5 more)

### Community 9 - "Mouse Input & Selection"
Cohesion: 0.08
Nodes (31): CursorMoved, MouseButtonInput, MouseProtocolEncoding, MouseWheel, encode_mouse_event(), encode_mouse_wheel(), ForwardedMouseState, handle_mouse_input() (+23 more)

### Community 10 - "PTY Runtime"
Cohesion: 0.08
Nodes (28): Box, Callbacks, Child, Drop, HashSet, JoinHandle, MasterPty, Receiver (+20 more)

### Community 11 - "3D Model Loading"
Cohesion: 0.15
Nodes (36): Model, build_meshes(), EmbeddedObjects, ensure_scene_asset_path(), InlineObject, load_obj_meshes_from_bytes(), load_obj_meshes_from_path(), load_object_source() (+28 more)

### Community 12 - "Debug Cell Renderer"
Cohesion: 0.13
Nodes (20): IntoIterator, Item, Rgba, ansi_index_to_rgba(), blend_rgba(), CellDebugImageRenderer, CellDebugImageRenderer<'a>, CellRect (+12 more)

### Community 13 - "Draw Demo"
Cohesion: 0.17
Nodes (16): BTreeSet, DrawingApp, DrawingApp<'a>, main(), DefaultTerminal, Frame, KeyEvent, MouseEvent (+8 more)

### Community 14 - "Ratty Brand Imagery"
Cohesion: 0.14
Nodes (24): Cheese-Yellow Brand Palette, Ratatui Brand Style, Ratty Android Chrome Favicon (192x192), Ratty Keyboard Wordmark, Ratty Terminal Emulator Project, Retro CRT Terminal Illustration, TempleOS-Inspired Retro Aesthetic, Terminal Command Prompt On Screen (+16 more)

### Community 15 - "Scene Setup Params"
Cohesion: 0.10
Nodes (23): Camera, Camera2d, ParamSet, PlaneBackTransformQuery, PlaneBackVisibilityQuery, PlaneCameraQuery, PlaneMaterialQuery, PlaneVisibilityQuery (+15 more)

### Community 16 - "App Entry & Window Icon"
Cohesion: 0.12
Nodes (21): Icon, NonSend, Cli, Option, PathBuf, String, Vec, apply_window_icon() (+13 more)

### Community 17 - "Website Demo Carousel"
Cohesion: 0.16
Nodes (18): applyDemo(), copyElement, dataElement, descriptionElement, hideVideoLoading(), leftButton, loadingScreen, primaryVideo (+10 more)

### Community 18 - "Mobius Transition"
Cohesion: 0.18
Nodes (6): ease_in_out(), MobiusTransition, MobiusTransitionDirection, Default, Self, Vec2

### Community 19 - "Present Material Shader"
Cohesion: 0.15
Nodes (11): Material2d, Material2dKey, MeshVertexBufferLayoutRef, RenderPipelineDescriptor, ShaderRef, SpecializedMeshPipelineError, Handle, Image (+3 more)

### Community 20 - "Scene Setup & Plane Meshes"
Cohesion: 0.26
Nodes (10): MeshMaterial2d, create_terminal_image(), Handle, Mesh, Self, setup_scene(), terminal_plane_mesh(), TerminalPlaneBack (+2 more)

### Community 21 - "3D Camera View Control"
Cohesion: 0.20
Nodes (6): apply_terminal_presentation(), Default, Option, Res, Vec2, TerminalPlaneView

### Community 22 - "Apple Touch Icon Branding"
Cohesion: 0.29
Nodes (10): Cheese-Yellow Color Palette, Apple Touch Icon Role (iOS Home Screen), Ratatui Brand Gold Color, Ratty Apple Touch Icon, Ratty Brand Identity, Keyboard Keys Spelling Ratty, Ratty Terminal Emulator Project, Retro Computing Aesthetic (+2 more)

### Community 23 - "TempleOS Heritage"
Cohesion: 0.31
Nodes (10): 16-Color VGA Pixel Art, Divine Judgment Symbolism, DolDoc, Kitty Graphics Protocol, RGP 3D Objects, Sword and Scales Motif, TempleOS, TempleOS Logo (+2 more)

### Community 24 - "Big Rat Example"
Cohesion: 0.36
Nodes (8): clamp_rect(), fill_background(), main(), Buffer, DefaultTerminal, Rect, Result, run()

### Community 25 - "512px Favicon Branding"
Cohesion: 0.43
Nodes (8): Swiss Cheese Wedge Motif, Website PWA Favicon Set, Rat Theme, Ratty Android Chrome 512x512 Favicon, Ratty Terminal Emulator, Ratty Wordmark, Retro CRT Terminal Motif, Pixel-Art Terminal Prompt Glyph

### Community 26 - "32px Favicon Branding"
Cohesion: 0.39
Nodes (8): Command Line Prompt Motif, Ratty Brand Identity, Ratty CRT Computer Logo, Ratty Favicon 32x32, Ratty Terminal Emulator, Ratty Website, Retro Computing Aesthetic, TempleOS Inspiration

### Community 27 - "2D Present Pipeline"
Cohesion: 0.33
Nodes (5): fullscreen_quad(), App, Mesh, Plugin, TerminalPresentPlugin

### Community 28 - "16px Favicon Branding"
Cohesion: 0.38
Nodes (7): Cheese Computer Motif, Ratty Brand Identity, Ratty Favicon (16x16), Ratty Logo, Ratty Terminal Emulator, Ratty Website, Retro Computing Aesthetic

## Ambiguous Edges - Review These
- `Retro CRT Terminal Illustration` → `TempleOS-Inspired Retro Aesthetic`  [AMBIGUOUS]
  website/assets/favicon/android-chrome-192x192.png · relation: conceptually_related_to
- `Pixel-Art Terminal Prompt Glyph` → `Rat Theme`  [AMBIGUOUS]
  website/assets/favicon/android-chrome-512x512.png · relation: semantically_similar_to
- `Cheese-Yellow Color Palette` → `Ratatui Brand Gold Color`  [AMBIGUOUS]
  website/assets/favicon/apple-touch-icon.png · relation: semantically_similar_to
- `Ratty Terminal Emulator` → `Retro Computing Aesthetic`  [AMBIGUOUS]
  website/assets/favicon/favicon-16x16.png · relation: conceptually_related_to
- `Ratty CRT Computer Logo` → `TempleOS Inspiration`  [AMBIGUOUS]
  website/assets/favicon/favicon-32x32.png · relation: conceptually_related_to
- `Ratty Logo` → `Terminal Emulator Software`  [AMBIGUOUS]
  website/assets/images/ratty-logo.gif · relation: semantically_similar_to
- `Cheese-Wedge CRT Monitor` → `TempleOS-Inspired Retro Aesthetic`  [AMBIGUOUS]
  website/assets/images/ratty-logo.png · relation: conceptually_related_to
- `Terminal Prompt Glyph` → `Rat Theme`  [AMBIGUOUS]
  website/assets/images/ratty-social-card.png · relation: conceptually_related_to
- `Terry A. Davis` → `Sword and Scales Motif`  [AMBIGUOUS]
  widget/assets/TempleOS.jpg · relation: conceptually_related_to

## Knowledge Gaps
- **41 isolated node(s):** `EmbeddedObjects`, `CellDebugImageRenderer`, `TerminalSprite`, `TerminalRedrawSet`, `dataElement` (+36 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **2 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **What is the exact relationship between `Retro CRT Terminal Illustration` and `TempleOS-Inspired Retro Aesthetic`?**
  _Edge tagged AMBIGUOUS (relation: conceptually_related_to) - confidence is low._
- **What is the exact relationship between `Pixel-Art Terminal Prompt Glyph` and `Rat Theme`?**
  _Edge tagged AMBIGUOUS (relation: semantically_similar_to) - confidence is low._
- **What is the exact relationship between `Cheese-Yellow Color Palette` and `Ratatui Brand Gold Color`?**
  _Edge tagged AMBIGUOUS (relation: semantically_similar_to) - confidence is low._
- **What is the exact relationship between `Ratty Terminal Emulator` and `Retro Computing Aesthetic`?**
  _Edge tagged AMBIGUOUS (relation: conceptually_related_to) - confidence is low._
- **What is the exact relationship between `Ratty CRT Computer Logo` and `TempleOS Inspiration`?**
  _Edge tagged AMBIGUOUS (relation: conceptually_related_to) - confidence is low._
- **What is the exact relationship between `Ratty Logo` and `Terminal Emulator Software`?**
  _Edge tagged AMBIGUOUS (relation: semantically_similar_to) - confidence is low._
- **What is the exact relationship between `Cheese-Wedge CRT Monitor` and `TempleOS-Inspired Retro Aesthetic`?**
  _Edge tagged AMBIGUOUS (relation: conceptually_related_to) - confidence is low._