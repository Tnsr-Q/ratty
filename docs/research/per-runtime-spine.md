# The per-runtime spine

Research asset for [wayfinder ticket #48](https://github.com/Tnsr-Q/ratty/issues/48)
(map [#42](https://github.com/Tnsr-Q/ratty/issues/42)). Census and decomposition
proposal — the design decisions belong to #49 (replacement command family),
#51 (focus & input routing), and #52 (screen-global vs per-runtime
arbitration); the wire-identity rows route to #50 (OSC-777/778 addressing);
#54 proves the N-seams claim natively.

## The locked frame (#22)

Everything below is argued inside the precedent the panes design locked:

- **Multiplexing = N instances of the proven seam.** Everything downstream of
  a runtime consumes only `try_recv` / `write_input` / `parser`
  (`src/runtime.rs:290-295`); N terminals means N instances of that seam, not
  a new abstraction.
- **A third transport is a constructor, not a refactor.** PTY and virtual
  channel are already interchangeable behind the seam; ssh/relay would be a
  third constructor returning `Self`.
- **Placement, not splits.** Terminals are objects placed in one 3D space,
  not a tmux split tree.
- **Keep mux state out of the renderer** (the zellij lesson).

The census verifies the frame holds: outside `runtime.rs`, only
`try_recv`/`write_input`/`parser`/`resize` and the keyboard-mode accessors
are touched (`keyboard.rs:551-552` goes through the runtime, never the
transport), reply routing already drains inside the owning runtime's pump
with a per-parser `IngressSource` stamp (`src/runtime.rs:76`), and the `Arc`
writer never escapes. The consumed surface also includes `runtime.shutdown()`
(`src/systems.rs:154`) and the public `pty_disconnected` flag read and
written in `pump_pty_output` (`src/systems.rs:232-233`) — both
runtime-level, not transport-level, so the frame holds. The seam is real.
What is *not* N-ready is everything parked around it as an app singleton —
that is what this census maps.

## The census: 114 symbols, four fates

Every `Resource`, `NonSend`, `static`, channel, texture, camera, and
singleton-consuming system that assumes one terminal, sorted into:

- **per-runtime** (65) — becomes a component on (or satellite of) a terminal
  entity, or a system routed per terminal entity;
- **screen-global** (32) — legitimately one per app/window/scene, unchanged
  or gaining only a routing selector;
- **ambiguous-arbitration** (15) — contended between N runtimes and the
  user; handed to **#52** as a cluster, not decided here;
- **wire-surface** (2) — identity/addressing questions handed to **#50**.

Two census conventions: struct cites point at the derive-attribute line,
and `cfg(test)` collectors are out of census scope (this covers
`src/ai.rs`'s `RemovedLog` and `src/sound.rs`'s `AckLog`).

### Per-runtime (65)

| Symbol | Kind | Where | Today | Proposed home |
| --- | --- | --- | --- | --- |
| `TerminalRuntime` | Resource | `src/runtime.rs:301` | The transport-and-parser bundle (rx, writer, PTY handles, vt100 parser incl. per-parser keyboard state), inserted once (`src/main.rs:66`, `src/web.rs:342`) | Component on the terminal entity — the seam instance itself; N entities = N reader threads + channels |
| `TerminalSurface` | Resource | `src/terminal.rs:78` | The one render surface: backend, renderer, texture triple, private font/theme copy | Component paired 1:1 with its runtime; per-terminal font zoom falls out of the private copy (`src/terminal.rs:144-153`) |
| `TerminalRedrawState` | Resource | `src/terminal.rs:54` | Single `needs_redraw` flag, many writers, taken once per frame (`src/systems.rs:427`) | Component beside the surface; scene-global writers fan out to all, runtime output dirties only its own |
| `TerminalSurface::sync_image` | method | `src/terminal.rs:253` | Reads/iterates ALL `VizRegistry` (`:287-316`) and ALL `PresenceRegistry` (`:317-324`) entries into the one texture (read-only pass; the drained sets are consumed in `rebuild_viz_objects`) | Runs once per surface; registries filtered organ-side by owning-terminal tag — never in the render path |
| `RuntimeOptions` | value / constructor argument | `src/runtime.rs:29` | One command override + working_dir built from the CLI (`src/main.rs:47-50`) | Already a constructor argument — only the CLI-seeds-terminal-#1 policy is new |
| `VirtualTerminalHost` | channel | `src/runtime.rs:329` | One feed/input pair per `virtual_channel()` call (`:433-434`); exactly one on wasm (`src/web.rs:324`) | One pair per virtual runtime by construction; only the holder becomes a collection |
| `TerminalFrameDirty` | Resource | `src/systems.rs:375` | Single redrawn-this-update bool gating `TerminalRedrawSet` | Component per terminal entity (set at `src/systems.rs:434`) |
| `pump_pty_output` | system | `src/systems.rs:166` | The #22 seam body, once: `try_recv` → parser → `write_input` replies | Loop per entity over its (runtime, inline, viz, redraw) tuple; body (`:181-239`) touches nothing else but the message writers |
| `shutdown_terminal_runtime_on_exit` | system | `src/systems.rs:142` | One `runtime.shutdown()` on AppExit | Iterate and shut down all N runtimes |
| `handle_window_resize` | system | `src/systems.rs:287` | Maps window size 1:1 onto THE PTY grid (`:325`) | Each terminal's cols/rows derive from its own surface geometry; resize fan-out routed per entity |
| `render_terminal_widget` | system | `src/systems.rs:410` | Draws THE parser screen into THE surface, publishes to THE exchange (`:439-467`) | Iterate terminal entities; the blink-phase `Local` is a shared clock and stays one |
| `sync_terminal_materials` | system | `src/systems.rs:494` | Global rebind of THE image handles onto plane/back/present materials (`:517-540`) | Route each terminal's handles to its own plane/sprite materials |
| `finish_terminal_model_load` | system | `src/systems.rs:561` | App-wide latches deferring THE one cursor-model spawn (`:578-599`) | Per-terminal lifecycle latch; the cursor spawn belongs to one terminal's plane |
| `respawn_cursor_model` | system | `src/systems.rs:618` | Despawns ALL `CursorModel` roots on `needs_respawn` (`:632-647`) | Key on the owning terminal so one `cursor` command cannot despawn every terminal's cursor |
| `apply_inline_objects` | system | `src/systems.rs:340` | Flips ALL inline visibility from THE single presentation mode (`:351-369`) | Route on each inline entity's owner + that terminal's presentation |
| `sync_inline_objects` | system | `src/systems.rs:679` | `plane_query.single()` (`:731`) parents kitty planes to THE plane | Scope to one terminal's registry, grid dims, and plane; the `.single()` is the hard assumption |
| `animate_inline_kitty_planes` | system | `src/systems.rs:1032` | THE global mode/warp applied to every kitty plane (`:1039-1053`) | Look up warp/mode through each plane's owning terminal |
| `sync_rgp_objects` | system | `src/systems.rs:1249` | Projects every RGP root onto THE plane (`.single()` at `:1319`) | Resolve each root's anchor + projection through its owning terminal's registry/grid/warp/plane |
| `apply_rgp_restyle` | system | `src/systems.rs:1515` | Drains THE global restyle set against THE anchor map (`:1524-1556`) | Per-terminal inline registry; object ids are only unique per runtime |
| `apply_instance_brightness` | system | `src/systems.rs:1599` | Brightness from THE anchor map + THE cursor settings (`:1614-1636`) | The `ChildOf` walk already finds a root entity — the lookup goes through the root's owner instead of two singletons |
| `rebuild_viz_objects` | system | `src/systems.rs:1840` | Drains THE global viz removal/rebuild/effect sets | Drain the owning terminal's registry |
| `sync_viz_objects` | system | `src/systems.rs:2202` | `.single()` plane (`:2238`), THE cell size | Route per owning terminal exactly like `sync_rgp_objects` |
| `animate_terminal_plane_warp` | system | `src/systems.rs:2424` | Rewrites THE front/back mesh pair from THE warp/mode (`:2448-2465`) | Per-plane mesh lookups (who *sets* warp/mode is #52's; the meshes are per-terminal) |
| `sync_asset_to_terminal_cursor` | system | `src/systems.rs:2714` | ONE pose written to ALL `CursorModel` entities (`:2744-2749`; `.single()` at `:2791`) | Each cursor model poses from its own terminal's parser screen, grid, and plane — the clearest one-runtime tell in the file |
| `DirectTerminalSceneExchange` | channel | `src/direct_render.rs:83` | Single-slot pending-frame mailbox cloned into the render app (`:38`) | Per-terminal exchange (or entity-keyed); publish/take (`:138-157`) is otherwise N-ready |
| `ExtractedDirectTerminalFrame` | Resource | `src/direct_render.rs:113` | One extracted frame slot per render frame | Keyed collection; the retain-on-unprepared-GPU-image logic (`:302-329`) holds one slot per terminal |
| `TerminalImages` | texture | `src/direct_render.rs:64` | THE render/present texture-handle pair | Pair per terminal entity; constructors (`:172`, `:208`) already take width/height/label |
| `extract_terminal_frame` | system | `src/direct_render.rs:278` | Moves THE pending frame into THE slot | Iterate per-terminal exchanges into keyed extracted slots; recycle logic is per-slot already |
| `render_terminal_frame` | system | `src/direct_render.rs:289` | Renders THE one extracted frame; shared `GpuRenderer` (`:331-368`) | Loop over N extracted frames; the body is already frame-scoped, only the source is singular |
| `handle_keyboard_input` | system | `src/keyboard.rs:350` | Writes every keystroke/paste/scroll/resize to THE runtime (`:439`, `:491`, `:559`, `:522-527`) | Route on the focused terminal entity (#51); `translate_key` already parameterizes on per-runtime modes (`:550-552`) — only the routing layer is missing |
| `TerminalSelection` | Resource | `src/mouse.rs:23` | Cells of THE grid plus a smuggled screen-global pointer position (`:29`, fused at `:179-186`) | Per-entity component; the window pointer splits out into a screen-global |
| `handle_mouse_input` | system | `src/mouse.rs:252` | Forwards encoded mouse events to THE runtime; mode/encoding from THE parser (`:276-277`) | Route on the picked/hovered entity; encoding is already per-runtime-shaped; camera rotate/pan/zoom branches (`:297-319`, `:475-486`, `:536-546`) stay scene-side |
| `ForwardedMouseState` | system Local | `src/mouse.rs:33` | Pressed buttons + `last_cell` forwarded to THE runtime | Capture state keyed by the press-target entity — press/motion/release must stick to one terminal across a drag |
| `LocalScrollState` | system Local | `src/mouse.rs:41` | Pixel remainder denominated in THE surface's char height (`:521-524`) | Per-hovered-entity; must not leak between terminals with different font metrics |
| `position_to_cell` | fn | `src/mouse.rs:580` | Centered-2D window→cell math (`:598`); the 3D modes never pick cells at all | The picking seam: raycast against per-entity plane transforms returning (entity, cell) — every `write_input` site flows through it (#51) |
| `KittyParserState` | Resource field | `src/kitty.rs:16` | Per-byte-stream APC parse state parked in the singleton inline registry (`src/inline.rs:86`) | Travels with each terminal's inline registry; two mid-chunk transfers (`m=1` at `src/kitty.rs:91`) would interleave and corrupt otherwise |
| `AiObjectRegistry` | Resource | `src/ai.rs:113` | One never-reuse id ledger + one 4096-id session budget (`:424`) | Per-entity; a "session" is one runtime's transport epoch — ids and budget must not be contended across terminals |
| `TerminalInlineObjects` | Resource | `src/inline.rs:82` | THE per-transport parse accumulator (pending_bytes/kitty/RGP runs, `:83-100`) + per-grid anchor table | Component constructed alongside each runtime — per-runtime by definition |
| `CursorSettings` | Resource | `src/model.rs:83` | One cursor model/animation state, config-seeded (`:125-129`) | Per-entity; each runtime's wire mutates its own (`src/ai.rs:574-628`); config stays the shared seed |
| `apply_ai_commands` | system | `src/ai.rs:179` | `ResMut` of six stage singletons (`:181-186`) | Route on each command's stamped source to the owning entity's stage/redraw components |
| `apply_ai_object_commands` | system | `src/ai.rs:335` | One inline registry, ledger, cursor, redraw each (`:337-342`) | Route on source to that runtime's four; namespace checks stay per-ingress within the runtime |
| `AiDiagnostics` | Resource | `src/query_channel.rs:179` | Rejection rings keyed by bare namespace u8 (`:181`) | Per-entity (or key gains runtime identity) — two runtimes' local callers are both namespace 0 and their rings silently merge today |
| `OrganRegistries` | SystemParam | `src/query_channel.rs:310` | `Res<>` of seven organ singletons — structurally one of each | Per-terminal-entity Query joins keyed by the request's runtime; config and time legitimately stay global |
| `answer_queries` | system | `src/query_channel.rs:330` | `send_reply` collapses to the single `runtime.write_input` (`:465-467`) | Route each request/ack by stamped source to the origin runtime's writer and organ state — reply routing is the transport seam |
| `MacroRegistry` | Resource | `src/macros.rs:301` | Session macros (`:304`) + slots (`:310`) + trusted config map (`:308`) — three fates in one resource | **Split**: session macros/slots per-entity (reset clears them, `:866-870`); the trusted map is wire-immutable config and stays global |
| `apply_macro_commands` | system | `src/macros.rs:757` | Taps THE shared command stream into THE registry (`:760-761`) | Route on source to the owning runtime's registry/slots; the handle mint stays on the global `QuerySession` |
| `drive_macro_playback` | system | `src/macros.rs:896` | One registry's slots, re-injecting under stored sources (`:916-921`), one shared frame budget (`:908`) | Iterate per-entity slot sets and re-stamp with the stored runtime-qualified source; budget scope is a #52-flavored detail |
| `ReactiveRegistry` | Resource | `src/reactive.rs:279` | Agent sensors (`:281`) + session rules (`:284`) + trusted rules (`:287`) + publish buckets (`:289`) mixed | **Split**: agent/session halves per-entity (same u8-collision problem as `AiDiagnostics`); trusted rules + `sys.*` half stay global |
| `apply_reactive_commands` | system | `src/reactive.rs:1030` | One registry, chain-depth-one wire-origin guard (`:1033-1034`) | Route on source to the owner's rule/sensor session state; macro hash-pinning resolves against the same runtime's macros |
| `evaluate_rules` | system | `src/reactive.rs:1185` | Drains `registry.evaluate()` re-stamping stored sources (`:1190-1197`) | Evaluate each runtime's session/trusted rules against its agent sensors plus the shared `sys.*` rows |
| `BookmarkRegistry` | Resource | `src/bookmarks.rs:67` | (namespace, name) keys; snapshots capture the singleton mode/warp (`:216-220`) | Per-entity; snapshots become that terminal's own stage components — a bookmark belongs to one terminal |
| `apply_bookmark_commands` | system | `src/bookmarks.rs:141` | Snapshots the singleton presentation/warp pair (`:143-146`) | Route on source; snapshot the owner's components, not one global pair |
| `PresenceRegistry` | Resource | `src/presence.rs:208` | Rows anchored to THE grid, clamped against THE surface's cols/rows | Per-entity; presence identity is ingress truth (joins must be `CommandOrigin::Wire`, `:1148-1151`) and rows anchor to the carrying stream's grid |
| `apply_presence_commands` | system | `src/presence.rs:1153` | One roster, one redraw target (`:1156-1157`) | Route on source to the owner's registry; redraw only that terminal's texture |
| `sync_presence_cursor_markers` | system | `src/presence.rs:934` | Singleton surface/viewport/presentation + `.single()` plane (`:962`, params `:909`) | Join each marker to its owner's plane/grid/presentation; `PresenceCursorMarker` (`:658`) gains a runtime owner key beside (namespace, id) |
| `request_presence_expiry_redraw` | system | `src/presence.rs:1088` | One drawn-stamp `Local`, one redraw write (`:1091-1092`) | Stamp and redraw per texture whose registry decayed — the fresh→expired flip repaints the specific terminal |
| `VizRegistry` | Resource | `src/viz.rs:652` | Entries anchored in THE grid's cells, scroll-tracked to THE grid (`:812`) | Per-entity; underlays draw into the owner's vello texture (`src/terminal.rs:304`, `src/direct_render.rs:265`) |
| `apply_viz_commands` | system | `src/viz.rs:896` | One registry + one redraw (`:901-905`) | Route each command on its originating runtime to that runtime's registry and that terminal's redraw |
| `TerminalPlaneMeshes` | Resource | `src/scene/mod.rs:44` | THE front/back plane mesh handles, inserted once (`:385`) | On/beside the terminal entity; Mobius rebuilds re-mesh per surface |
| `TerminalPlaneWarp` | Resource | `src/scene/mod.rs:53` | One scalar warp deforming THE plane | Per-entity geometric property; stage writes route by originating runtime (writer policy is #52's) |
| `TerminalViewport` | Resource | `src/scene/mod.rs:67` | One viewport size/center, rewritten by `sync_terminal_layout` (`:467`) | Per-entity texture geometry — each terminal has its own logical size and placement |
| `ModelLoadState` | Resource | `src/scene/mod.rs:197` | Models-loaded + first-frame-uploaded fused in one resource | **Split**: the first-frame gate goes per surface (consumers `src/systems.rs:398`, `:547`, `:607`); models-loaded stays app-wide |
| `setup_scene` | system | `src/scene/mod.rs:283` | One Startup body binds THE runtime, texture, sprite, plane pair, cameras, and inserts seven resources (`:363-457`) | **Split**: a screen-global camera/scene spawner + a per-terminal spawner invoked once per constructed runtime |
| `sync_terminal_layout` | system | `src/scene/mod.rs:461` | One layout applied to every plane transform (`:470-476`) | Route each terminal's layout to that entity's viewport and planes |
| `TERMINAL_TEXTURE_LABEL` / `TERMINAL_RENDER_TEXTURE_LABEL` | texture names | `src/config.rs:24`, `:26` | Two fixed label strings — name-based identity for exactly one texture pair (consumed `src/scene/mod.rs:337`, `:344`) | Per-entity labels or handle-only identity carried on the terminal entity |

### Screen-global (32)

| Symbol | Kind | Where | Today | Proposed home |
| --- | --- | --- | --- | --- |
| `RattySession` | wasm-bindgen JS handle | `src/web.rs:154-159` | One JS page handle: the channel pair + controls Arc ("one start() per page", `:60-62`); never inserted into the Bevy world — returned to JS (`:400`) | Settled today: one session per page (gesture unlock, stage controls, disposal). How N reaches the page API — per-terminal selectors, N sessions, or an in-band mux — is exactly ticket #53's question |
| `PENDING_QUERIES` | static | `src/web.rs:62` | token→promise map; 128-bit random tokens (`:73-77`, `:85-92`), frame-swept (`:130`) | Unchanged — tokens are runtime-agnostic; only a query's outbound feed target is N-sensitive |
| `WebControlQueue` | Resource | `src/web.rs:45` | JS stage-control queue (`:36-42`), drained once per frame | One per scene; JS setters gain a target id only if warp goes per-plane |
| `request_exit_on_primary_window_close` | system | `src/systems.rs:116-139` | Latches `AppExit` when THE primary window is asked to close | One window, unchanged by N; it triggers the `AppExit` that the per-entity shutdown iteration consumes |
| `rebuild_viz_objects::unit_mesh` | static (Local) | `src/systems.rs:1825` | Cached shared unit-cube mesh handle | Shared immutable asset caches — `unit_mesh`, presence `marker_mesh` (`src/presence.rs:923`), the avatar text system (`src/avatar/present.rs:339`) — all N terminals share the handles |
| `DirectTerminalRenderState` | Resource | `src/direct_render.rs:100` | The one Vello `GpuRenderer` | Already serves two scenes per frame (`:49-57`); N terminals are N sequential submissions |
| `AvatarOverlayExchange` | channel | `src/direct_render.rs:396` | Single-slot exchange for the bubble overlay scene | Per-screen HUD (screen-space quad, `:371-377`), not a per-terminal surface |
| `ExtractedAvatarOverlayFrame` | Resource | `src/direct_render.rs:450` | One extracted overlay frame | One avatar overlay per screen; unchanged by N |
| `TerminalKeyBindings` | Resource | `src/keyboard.rs:73` | Chord→action table from config (`:77-242`; `src/plugin.rs:49`) | App-level user config; only the dispatch target multiplies |
| `TerminalKeyboard` | system Local | `src/keyboard.rs:256` | Physical modifier state (used at `:352`) | One physical keyboard; `handle_event_with_modes` (`:275`) already takes per-runtime modes as arguments |
| `TerminalClipboard` | NonSend | `src/keyboard.rs:27` | arboard bridge (`src/plugin.rs:52`) | One OS clipboard; which selection is copied / which runtime gets the paste is routing in the system, not here |
| `AiCommand` bus | channel | `src/ai.rs:83` | One `Messages<AiCommand>` bus; items stamped `IngressSource` | One bus, stamp gains runtime identity — per-runtime buses would N-plicate every organ system instead of letting them route |
| `Messages<QueryRequest>` | message bus | `src/query_channel.rs:73-78` (registered `src/ai.rs:130`) | One 778-item bus; every item carries `pub source: IngressSource` | Same fate as the `AiCommand` bus: one bus, source-stamped routing |
| `Messages<AckOutcome>` | message bus | `src/query_channel.rs:93-100` (registered `src/ai.rs:131`) | One ack-outcome bus; every item carries `pub source: IngressSource` | Same fate as the `AiCommand` bus: one bus, source-stamped routing |
| `Messages<AiObjectRemoved>` | message bus | `src/ai.rs:100-104` (registered `:129`) | One removal-notification bus; items carry only the removed id — no source stamp | One bus, but flagged: needs a runtime stamp before per-runtime registries land |
| `QuerySession` | Resource | `src/query_channel.rs:119` | Process-lifetime nonce + monotone handle counter (`:125-147`) | Its guarantees (anti-staleness, handle uniqueness) are per-app; nothing terminal-scoped |
| `ReactiveRegistry` `sys.*` + `system_enabled` | Resource half | `src/reactive.rs:292` | Host adapter grant (set at `:1007`) + `sys.cpu/memory/battery` rows | One host, one grant; publish once, read by every runtime's rules |
| `seed_reactive_from_config` | system | `src/reactive.rs:1000` | Startup one-shot seeding trusted rules + adapter flag | Writes only the config-derived global halves; re-runs at terminal spawn only if trusted rules replicate per-runtime |
| `sample_system_sensors` | system | `src/reactive.rs:1205` | Cadence sampler owning the sysinfo/battery handles (`:1209`, `:1244-1251`) | One host, one platform-handle set; publishes to the shared `sys.*` store regardless of N |
| `PendingBookmarkJumps` | Resource | `src/bookmarks.rs:107` | Relowering buffer drained same-frame (`:284`); entries carry `IngressSource` | One buffer routes N correctly the moment the stamp names the runtime |
| `AiEffectCamera` + `AiEffectSprite` | camera | `src/effects.rs:341` | Order-10 fullscreen wash compositor (`:346-353`) | One top-of-stack compositor per scene; only what *feeds* it is contended (#52) |
| `SoundState` | Resource | `src/sound.rs:267` | Mixer decision state; namespace-keyed buckets and voices (`:276-287`) | One mixer state serves N runtimes; `unlocked` is a user-gesture fact. Its per-namespace voice buckets merge under ns-0 exactly like `AiDiagnostics`' rings — acceptable for a shared speaker (global loudness arbitration is the point), with the principal-model question routed to #50 |
| `playback::SoundAssets` | Resource | `src/sound.rs:731` | Embedded sound registry handles (loaded at `:798`) | Process-wide asset cache |
| `playback::SoundPlayback` | Resource | `src/sound.rs:762` | Kira bookkeeping keyed by session-monotonic voice ids (`:761-774`) | Backend for the one physical mixer; its ambient field inherits the `AmbientSlot` arbitration |
| `apply_sound_commands` | system | `src/sound.rs:443` | Merged `sound.*` stream → one `SoundState` | Stays one system — namespaced caps already handle multiple emitters; `ambient.set` inherits #52's call |
| `AvatarState` | Resource | `src/avatar/mod.rs:621` | "Scene-global avatar state" (`:618`) with the namespace-fair speech queue (`:638-639`) | One mascot per screen by design; speaker identity rides the already-stamped command source |
| `AvatarOverlayImages` | texture | `src/avatar/present.rs:87` | Bubble overlay texture pair (1x1 idle) | One HUD bubble per window ("one bubble — the active utterance only", `:19-20`) |
| `AvatarBubbleCamera` + `AvatarMascotCamera` | camera | `src/avatar/present.rs:96` | Order-5/6 HUD camera stack on an isolated layer | One per window; deliberately isolated from RGP camera writes (`:9-11`) |
| `AppConfig` | Resource | `src/config.rs:31` | Trusted startup config, wire-immutable (inserted `src/main.rs:65`) | One per app; its terminal/shell/cursor sections become spawn-time seeds each runtime copies |
| `ClearColor` | Resource | `src/main.rs:59` | Window clear from theme background + opacity (`:59-64`) | One window, one clear color; per-terminal background lives on each surface's material |
| `AppWindowIcon` | NonSend | `src/main.rs:68` | winit icon applied to the primary window (`:113`) | OS window chrome — one per app |
| `WinitSettings` | Resource | `src/main.rs:72` | `continuous()` so background PTY output keeps pumping | App scheduling policy; N runtimes strengthen the justification, still one setting |

### Ambiguous-arbitration (15) — handed to #52

These are not undecided out of laziness: each fuses per-runtime input with
shared-scene output, and #52 owns the who-wins scheme. The existing
precedent to extend is the locked writer order rgp→ai→web→presentation
(`src/web.rs:376-394`) and "JS controls are user input: they win"
(`src/web.rs:429-433`).

| Symbol | Kind | Where | Today | The #52 question |
| --- | --- | --- | --- | --- |
| `drain_web_controls` | system | `src/web.rs:407` | JS controls override scripted tweens across seven singletons | Who wins over the contended stage state; independently its `ResMut<TerminalRedrawState>` (`:414`) becomes routed fan-out per surface regardless |
| `animate_mobius_transition` | system | `src/systems.rs:2469` | Advances THE mode morph; on exit restores camera into THE plane view (`:2488-2495`) | A per-plane morph writing the shared camera — per-terminal morph or whole-scene cut, and who owns the camera during it |
| `apply_rgp_stage` | system | `src/systems.rs:2505` | One runtime's wire drives shared stage/camera/tween (`:2514-2597`) | With N runtimes issuing `c` sequences, who may move the camera/mode |
| `animate_stage_tween` | system | `src/systems.rs:2602` | The one tween feeds warp + camera every frame (`:2613-2629`) | If warp goes per-terminal its warp channel splits per-runtime; the camera channels stay contended |
| `MacroRegistry::scene_lock` | Resource field | `src/macros.rs:313` | One exclusive privileged-playback lock across all agents | Does a privileged playback lock its own terminal or the whole scene |
| `AiEffects` | Resource | `src/effects.rs:119` | One whole-screen emotional wash ("tints the avatar like everything else", `src/avatar/present.rs:11-13`) | Per-runtime commands, one order-10 overlay — whose mood owns the wash |
| `SoundState.ambient` (`AmbientSlot`) | Resource field | `src/sound.rs:278` | The single scene-owned ambient slot (struct `:221`) | Which runtime's bed plays when N ambient-capable wires write a last-writer-wins slot |
| `TerminalPresentation` | Resource | `src/scene/mod.rs:99` | One active mode; any runtime's stage command writes it absolutely (`:132`) | It selects which scene camera renders — scene-level fact, per-runtime writers |
| `TerminalPlaneView` | camera | `src/scene/mod.rs:161` | One 3D camera; mouse, keyboard, and the RGP `c` verb all write | One camera looking at N planes: user drags vs N runtimes' writes — the canonical case |
| `StageTween` | Resource | `src/scene/stage.rs:28` | One `c;dur=` tween, replace-on-write, fusing warp + yaw/pitch/zoom | Needs field-level splitting (per-plane vs shared-camera channels) before ownership can even be assigned; today N runtimes would silently cancel each other |
| `MobiusTransition` | Resource | `src/scene/mobius.rs:8` | Per-surface morph timing entangled with saved/target camera fields | Split the per-runtime half from the camera half, then assign the rest |
| `apply_ai_effect_commands` | system | `src/effects.rs:373` | Merged stream → one `AiEffects` + one redraw | Routing follows the `AiEffects` decision: per-surface tints route on source; a single wash needs a blend policy |
| `animate_ai_effects` | system | `src/effects.rs:435` | `sprite.single_mut()` one-overlay assumption (`:443`) | Iterate N per-surface effect states, or one sprite fed by an arbitrated blend |
| `apply_terminal_presentation` | system | `src/scene/mod.rs:496` | Consumes the whole contended cluster; assumes one camera_2d/camera_3d (`:252-263`) | Its shape follows whatever #52 decides for the scene camera |
| `SceneCapability::granted_to` | method | `src/capability.rs:51` | Exhaustive wildcard-free match over one-variant `IngressSource` + two config grant bits (`src/config.rs:546`) | The designed compile-break: per-principal grant tables, and who wins when several grantees drive one scene |

### Wire-surface (2) — handed to #50

| Symbol | Kind | Where | Today | The #50 question |
| --- | --- | --- | --- | --- |
| `IngressSource` | enum | `src/runtime.rs:45` | Closed one-variant trust catalog; `Local` (`:49`) owns AI namespace 0 (`:57`); stamped per parser instance (`:76`) | Whether N local PTYs are N distinct principals with N AI-object namespaces, or one shared `Local` — the stamp is the natural identity carrier either way |
| `apply_avatar_commands` | system | `src/avatar/mod.rs:717` | Per-source capability gating via `SceneCapability::AvatarScene.granted_to` (`:753`) | The organ stays one, but speaker attribution, fair-queue keying, and per-source grants become 777 addressing once N wires exist |

Zero-row files worth naming: `src/rgp.rs` is a stateless protocol parser and
`src/viz_draw.rs` is pure draw functions — no Resource, no static, no
singleton assumption. They are allies of the "constructor, not refactor"
claim: two whole subsystems already multiplex for free.

## The decomposition: entity-per-runtime, not a registry

Two candidate shapes, both named in the panes study: **components on a
terminal entity** vs **a `Terminals` resource holding a slotmap of
runtimes**. Argued against the actual rows:

**The per-runtime group is dominated by scene joins, and scene joins speak
Entity.** Five systems end at `plane_query.single()`
(`src/systems.rs:731`, `:1319`, `:2238`, `:2791`; `src/presence.rs:962`) —
every projector (inline, RGP, viz, cursor, presence markers) must resolve "which
terminal owns this scene object." `apply_instance_brightness` already walks
`ChildOf` ancestry to a root entity (`src/systems.rs:1614-1636`); the
natural completion is root → owner component → terminal entity's components,
one Query join. A slotmap forces every one of those ~15 systems to carry a
second identity universe (entity ↔ key side tables) parallel to the one Bevy
already maintains.

**Picking returns Entity.** The #51 blocker — `position_to_cell` becoming a
raycast — produces a hit entity natively. (entity, cell) is the routing
tuple; a slotmap would immediately convert it back to a key.

**Lifecycle is the injector problem, and entities already solve half of
it.** `drive_macro_playback`, `evaluate_rules`, and `drain_bookmark_jumps`
replay *stored* sources; a runtime dying must invalidate or reroute them.
`Entity` ids are generational — a despawned terminal's id fails lookups
safely, and component-removal observers give a place to sweep stored state.
A slotmap's generational keys re-implement exactly this, one layer removed
from where the satellites (planes, viz roots, cursor models, markers,
inline sprites) actually live.

**The touch count is the same either way.** All 65 per-runtime rows get
edited regardless of shape — every system gains either a Query or a key
parameter. The slotmap's apparent advantage (resources keep their `run_if`
shapes) is void: the wiring sweep shows the gates at `src/plugin.rs:65`,
`:71`, `:82-89`, `:96`, `:128`, `:134`, `:144-150`, `:154` must become
Query-based conditions or in-system per-entity filtering anyway, or one
terminal's pending state runs (or starves) every terminal's system.

**Placement, not splits, settles it.** Terminals-in-space means terminals
are scene objects with transforms. Scene objects are entities. Splitting a
terminal's identity between a resource-held slotmap and its scene entities
reintroduces the two-worlds problem the #22 precedent exists to prevent.

The registry's honest advantages — one iteration site, stable ordering,
trivial render-world keying — survive in miniature: the render world keys
extracted frames by the main-world `Entity` (the standard Bevy
extracted-view pattern), and ordering, where it matters, is explicit
sorting, not map order.

### The spine, named

**The terminal entity** (spawned by the per-terminal half of `setup_scene`),
carrying today's types re-derived `Resource` → `Component`, names kept:

- *Seam*: `TerminalRuntime`, `TerminalInlineObjects` (which already contains
  `KittyParserState`)
- *Surface*: `TerminalSurface`, `TerminalRedrawState`, `TerminalFrameDirty`,
  `DirectTerminalSceneExchange` (entity-keyed at extraction),
  per-entity texture handles replacing the fixed labels
- *Geometry*: `TerminalViewport`, `TerminalPlaneMeshes`, `TerminalPlaneWarp`
- *Interaction*: `TerminalSelection` (pointer position removed),
  `CursorSettings`
- *Organ state*: `VizRegistry`, `PresenceRegistry`, `AiObjectRegistry`,
  `AiDiagnostics`, `BookmarkRegistry`, and the session halves of the two
  split registries: `MacroSession` (from `MacroRegistry`) and
  `ReactiveSession` (from `ReactiveRegistry`)
- *Lifecycle*: `SurfaceReady` (the per-surface half of `ModelLoadState`)

**New relationship component**: `TerminalOwner(Entity)` on every satellite —
plane pair, inline sprites/planes, kitty planes, viz roots, RGP roots,
cursor models, presence markers. This is the tag `sync_image`'s organ-side
filtering routes on.

**New screen-global resources**: `WindowPointer` (the position split out of
`TerminalSelection`), `TrustedMacros` and `TrustedRules`/`SystemSensors`
(the global halves of the split registries), `ModelsLoaded` (the app-wide
half of `ModelLoadState`).

**A focus authority** must also exist — the census calls it
`FocusedTerminal` for reference, but that is a named placeholder, not a
decided resource: whether it is a resource, a component, or per-window —
and its cardinality — is #51's first question.

**The spawner**: `spawn_terminal(&mut Commands, &AppConfig, RuntimeOptions)
-> Entity` — constructs the runtime via the existing constructor, builds the
surface and texture pair (`new_terminal_image` /
`new_terminal_render_image`, `src/direct_render.rs:172`, `:208`, already
plain constructors), spawns the plane pair with `TerminalOwner`, inserts the
component bundle. The CLI seeds terminal #1's `RuntimeOptions`; #49's
commands seed the rest.

## The seam story

**What `virtual_channel` proves.** `TerminalRuntime::virtual_channel`
(`src/runtime.rs:430-458`) builds a complete runtime — channel, writer,
parser with callbacks — from `&AppConfig` alone, touching zero global state,
and returns `(Self, VirtualTerminalHost)`. The native constructor
`spawn(&AppConfig, &RuntimeOptions)` (`src/runtime.rs:466`) has the same
shape. Both are exercised without an app
(`virtual_channel_round_trips_output_and_input`, `src/runtime.rs:662`).
Nothing in construction resists N: N terminals is N
constructor calls, and a third transport (ssh, relay, replay) is a third
constructor returning `Self` — the #22 precedent, verified.

**Identity is already per-instance.** Each parser's
`TerminalParserCallbacks` stamps its own `IngressSource` onto every parsed
777/778 (`src/runtime.rs:76`); pending replies and wire errors drain inside
the owning runtime's pump; the `Arc` writer never escapes `runtime.rs`.
Reply routing needs no global registry — it needs the stamp to *name the
runtime*, which is #50's surface.

**Why the renderer stays mux-free.** The render world sees only published
frames: `update_direct_terminal_frame` (`src/direct_render.rs:245`) is
already parameterized by (exchange, images) — a constructor, not a refactor
— and the one `GpuRenderer` already submits two scenes per frame
sequentially (`src/direct_render.rs:49-57`), so N terminals are N
submissions through the same renderer. The two places mux state could leak
in are closed by construction: (1) `sync_image`'s registry drains become
organ-side filters on `TerminalOwner` tags *before* the texture path, so the
pure draw layers (`src/viz_draw.rs`, `presence_underlays` at
`src/presence.rs:696`) never learn N exists; (2) extraction keys frames by
entity, and `render_terminal_frame` loops over frames whose bodies are
already frame-scoped. The zellij lesson holds without a single renderer
branch on terminal count.

## What each downstream ticket takes

**#49 — replacement command family.** The committed pane commands
(`SplitPane`/`FocusPane`/`ResizePane`/`ClosePane`, `src/osc.rs:351-375`,
parse arms `:954-966`) assume a split tree with u8 pane ids — the wrong
shape under placement-not-splits. They are superseded-pending:
token-carrying invocations reject `codes::UNSUPPORTED` via the ai.rs
catch-all (`src/ai.rs:315-325`); tokenless ones log. Design the replacement
against the spawner: creation =
`RuntimeOptions` (command, cwd) + a placement (transform), focus = writing
the focus authority (the census's `FocusedTerminal` placeholder — its shape
is #51's first question), close = despawn (which runs the shutdown that
`shutdown_terminal_runtime_on_exit` currently does once). Constraint from
the census: creation must invoke the *whole* per-terminal spawner (runtime +
surface + textures + planes + organ components), not just runtime
construction — `setup_scene`'s split is the prerequisite. Addressing of the
new commands' targets is #50's vocabulary.

**#51 — focus & input routing.** Takes the two routing concepts the input
sweep found unowned: keyboard focus (`handle_keyboard_input` writes
unconditionally today — a focus authority must exist and something must set
it; the census's `FocusedTerminal` is a placeholder name for it, and
whether it is a resource, a component, or per-window — and its cardinality
— is #51's first question) and mouse capture (`ForwardedMouseState` becomes
entity-keyed so
press/motion/release stick to one terminal across a drag;
`LocalScrollState` becomes per-hovered-entity). Its hard blocker is the
picking seam: `position_to_cell` (`src/mouse.rs:580`) is centered-2D math on
the primary window and the 3D modes never resolve cells at all —
terminals-in-space needs raycast picking producing (entity, cell) to feed
selection, mouse-protocol encoding, and focus-follows-click alike. What #51
does *not* need: any renderer or parser knowledge — encoding decisions
already derive from each runtime's parser modes.

**#52 — arbitration.** Receives the 15 ambiguous rows as **one cluster plus
four scalars**, not 15 independent questions. The cluster is the
stage/camera state (`TerminalPresentation`, `TerminalPlaneView`,
`StageTween`, `MobiusTransition`) and its five writers/consumers
(`drain_web_controls`, `apply_rgp_stage`, `animate_stage_tween`,
`animate_mobius_transition`, `apply_terminal_presentation`) — and the
prerequisite is mechanical, not policy: `StageTween` and `MobiusTransition`
both fuse per-plane channels (warp, morph) with shared-camera channels
(yaw/pitch/zoom/offset) and must split at the field level before ownership
can be assigned. The four scalars: `MacroRegistry::scene_lock`, the
`AiEffects` wash, the `AmbientSlot` bed, and the `SceneCapability` grant
table (whose wildcard-free match is the codebase's designed compile-break).
The precedent to extend: the locked writer order rgp→ai→web→presentation
and "user input wins" (`src/web.rs:376-394`, `:429-433`). The census's
classification of presence/viz/avatar/inline (and the `TerminalOwner`
naming) discharges #52's first bullet as a recommendation that #52 ratifies
at the spine grilling.

**#54 — native N-seams spike.** Prove the constructor claim mechanically:
two native runtimes on two terminal entities, no layout polish, no organ
routing. The spike's checklist, straight from the census: (1) `Resource` →
`Component` derives for the seam/surface rows; (2) `pump_pty_output`,
`render_terminal_widget`, `sync_terminal_materials`, and the
extract/render pair become loops over queries/keyed frames; (3) per-entity
texture identity replacing the fixed labels (`src/config.rs:24-26`); (4) the
`run_if` gates in `src/plugin.rs` become Query-based conditions; (5)
`setup_scene` splits. Honest spike scoping: organs, arbitration, and
addressing stay pinned to terminal #1 — the spike measures whether the seam
multiplies and what N Vello submissions per frame cost, nothing else.

**#50 — addressing (context, not gated here).** Takes the two wire-surface
rows plus the addressing edges the per-runtime rows exposed: how a
777-created viz acquires terminal ownership (`sync_image` filtering), which
terminal a `RattySession.query()` feeds, whether N local PTYs are N
principals with N namespace-0 universes, and what the runtime-qualified
`IngressSource` stamp looks like on the wire.

## Open questions

- **Internal routing key vs wire identity.** Inside the app the routing key
  is the terminal `Entity`; on the wire it cannot be (entities are not
  stable across sessions). Does `IngressSource` grow a runtime field, or do
  commands carry (Entity, IngressSource) side by side? The injectors that
  replay *stored* sources (macros, rules, bookmarks) make this a lifecycle
  question: what invalidates a stored source when its runtime dies?
  Deferred to #50 with the lifecycle constraint recorded here.
- **Cross-terminal references become impossible by construction.**
  Per-runtime registries fix the bare-u8 namespace collision, but also mean
  a macro on terminal A can never invoke a bookmark taken on terminal B.
  Today that is vacuously true (one terminal); whether it should *stay*
  impossible is a real #50/#49 choice, not a free consequence.
- **The wasm N-story is unresolved.** `virtual_channel()` makes N seams
  cheap on wasm, but how N reaches the page API — per-terminal selectors on
  `feed()`/`drain_input()`/`query()`, N `RattySession`s, or an in-band mux —
  is the same fork the panes study hit and is exactly #53's question. Not
  designed here.
- **Is presentation mode per-terminal at all?** The census marks
  `TerminalPresentation` ambiguous deliberately: Mobius as a per-plane morph
  vs a whole-scene cut changes which half of `MobiusTransition` survives the
  split. #52 owns it, but the answer feeds back into how much of the stage
  cluster goes per-entity.
- **Performance envelope is unmeasured.** N Vello scenes through one
  `GpuRenderer` per frame is architecturally fine (sequential submission is
  today's pattern) but the cost curve is unknown — #54's job to measure
  before #49 promises cheap terminal spawns.
- **Where trusted config replicates.** The global halves of the split
  registries (trusted macros, trusted rules) could stay one copy read by all
  runtimes, or re-seed per terminal at spawn (`seed_reactive_from_config`
  re-running). One copy is proposed here; the alternative only matters if
  per-terminal trust divergence is ever wanted — flagged for #50's
  principal model.

## Errata carried from verification

- The census brief's `TerminalSizing` does not exist anywhere
  (grep-verified); `src/terminal.rs:54` is `TerminalRedrawState`.
- `TerminalPresentationMode` (`src/scene/mod.rs:76`) derives `Resource` but
  is never inserted as one — vestigial; the live resource is
  `TerminalPresentation` (`src/scene/mod.rs:99`).
