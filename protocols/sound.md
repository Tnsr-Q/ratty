# Ratty Sound Protocol (OSC 777 sound family)

Where the [Ratty Graphics Protocol](graphics.md) carries the 3D payload
and the [Ratty Query Protocol](query.md) (OSC 778) is the read side, the
`sound.*` family rides the OSC 777 control channel: an agent or a
transmission *requests* semantic sounds — one-shot event marks and a
single scene-owned ambient bed — and the terminal decides, acks, and
plays. Transmissions author it through the typed `sound` step in
[Silk](silk.md).

Sound has a semantic basis, not a decorative one: one-shots mark state
transitions and coordination events, the ambient bed carries scene mood.
There is no general-purpose audio player here — no paths, no URLs, no
mixer access from the wire.

## Design goals

- **Semantic basis, not decoration.** Every registered kind names an
  event meaning (completion, attention, heartbeat, acknowledgment, scene
  bed); the wire says *what happened*, the terminal decides how it
  sounds.
- **The wire requests; config owns the mixer.** Commands carry per-play
  gain requests that the terminal clamps to the kind's registry maximum.
  Master gain, mute, and the scene-ambient capability live only in
  trusted `[audio]` config — no wire command can write them.
- **Registered kinds only — never paths or URLs.** Kinds resolve against
  an embedded registry compiled into the binary; the sound wire cannot
  name a file, and nothing in the sound path touches the filesystem.
- **Honest acks, decided once.** Every command is decided and acked the
  same frame it lands; there are no later events (`t=e` stays reserved).
  A one-shot that will not play is *rejected*, never silently queued — a
  chime played late would lie about when the event happened. Unlock
  status is polled state (`state.scene`), never pushed.
- **Bounded everything.** Global and per-namespace voice caps, per-
  namespace rate limits, and clamped fade durations; the caps are
  advertised in `caps().limits`.
- **Same semantics native and wasm.** One decision table everywhere; the
  only platform difference is that browsers start locked under autoplay
  policy and native builds are born unlocked.

## Transport

Sound commands are ordinary OSC 777 control sequences (see the module
doc in `src/osc.rs` for the channel itself):

```text
ESC ] 777 ; ratty:sound.play         ; kind=<kind>[&gain=<0.0-1.0>][&tok=<token>] BEL
ESC ] 777 ; ratty:sound.ambient.set  ; kind=<kind>[&gain=<0.0-1.0>][&xfade=<ms>][&tok=<token>] BEL
ESC ] 777 ; ratty:sound.ambient.stop ; [fade=<ms>][&tok=<token>] BEL
```

- `kind=` is required on `sound.play` and `sound.ambient.set`; a
  `tok=`-carrying sequence without it fails to parse and acks
  `bad-command`, like any other malformed 777 command.
- `gain=`, `xfade=`, and `fade=` are optional; a malformed numeric value
  is dropped per-key (the kind's registry default gain / the default
  fade applies), matching RGP field behavior.
- `xfade=`/`fade=` are milliseconds, clamped terminal-side to
  `100..=5000` (default `1500`).
- `tok=` opts into the delivery ack over OSC 778 exactly as documented
  in [query.md](query.md) (Command acks); commands without it stay
  fire-and-forget, and their rejections land in the caller's
  `state.errors` ring.

The subsystem sits behind the `sound` cargo feature (on by default).
The wire parser, kind registry, decision layer, and state projection are
compiled into every build; only the playback backend is gated. A build
without the feature parses `sound.*` normally and rejects every command
with an honest `unsupported`.

## Kind registry

The wire only ever names these registered semantic kinds. The shared
name list lives in `src/osc.rs` (`SOUND_KINDS`) so authoring tools
(`ratty-ai`, `silk`) validate kinds without the audio feature; gains and
asset resolution live terminal-side in `src/sound.rs`
(`SOUND_REGISTRY`).

| Kind          | Meaning                        | Class    | Default gain | Max gain |
| ------------- | ------------------------------ | -------- | ------------ | -------- |
| `chime`       | task / transition complete     | one-shot | 0.8          | 1.0      |
| `alert`       | attention / error              | one-shot | 0.9          | 1.0      |
| `pulse`       | heartbeat / progress tick      | one-shot | 0.7          | 1.0      |
| `click`       | acknowledgment                 | one-shot | 0.6          | 1.0      |
| `ambient.hum` | scene bed                      | ambient  | 0.5          | 0.8      |

Classes are enforced: a one-shot kind through `sound.ambient.set` (or an
ambient kind through `sound.play`) rejects `bad-kind`.

Each kind resolves to a small embedded `.ogg` under `assets/sounds/`
(about 37 KiB for the whole set), shipped inside the binary the way
`assets/objects/` models are. Budgets are enforced at build time —
192 KiB per asset, 512 KiB for the package — so the set cannot silently
bloat. One-shots are mastered around −6 dBFS and the ambient bed around
−18 dBFS; the bed loops seamlessly.

## Ops

Each op is decided top-to-bottom against its table the frame it arrives,
and the ack (when `tok=` was given) fires once with the row's outcome.

### 1. `sound.play` — one-shot event sound

Plays a registered one-shot kind once, at the requested gain clamped to
the kind's registry maximum.

| # | Condition (checked in order)                       | Ack    | `code=`        |
| - | -------------------------------------------------- | ------ | -------------- |
| 1 | binary built without the `sound` feature           | `ok=0` | `unsupported`  |
| 2 | `kind` is not a registered kind                    | `ok=0` | `bad-kind`     |
| 3 | `kind` is an ambient bed (use `sound.ambient.set`) | `ok=0` | `bad-kind`     |
| 4 | audio is locked (browser, pre-gesture)             | `ok=0` | `audio-locked` |
| 5 | the caller's rate bucket is empty                  | `ok=0` | `rate-limited` |
| 6 | the global voice cap (16) is full                  | `ok=0` | `voice-cap`    |
| 7 | the caller's namespace voice cap (8) is full       | `ok=0` | `voice-cap`    |
| 8 | otherwise: gain clamped, voice committed           | `ok=1` | —              |

- **Locked means dropped (row 4).** A one-shot is evental: played after
  the unlock gesture it would misreport when the event happened, so
  pre-unlock one-shots are rejected honestly, never queued.
- **Rate limit (row 5).** A per-namespace token bucket: capacity 8
  (burst), refilling at 4 plays/second — the `sound_plays_per_sec`
  advertised in `caps().limits`.
- **Voice caps (rows 6–7).** A committed play occupies a voice until its
  instance ends; 16 voices globally, 8 per namespace, advertised as
  `sound_voices`.

### 2. `sound.ambient.set` — set or crossfade the scene bed

Sets the single scene-owned ambient slot to a registered ambient kind,
crossfading from whatever played before.

| # | Condition (checked in order)                          | Ack    | `code=`         |
| - | ----------------------------------------------------- | ------ | --------------- |
| 1 | binary built without the `sound` feature              | `ok=0` | `unsupported`   |
| 2 | `kind` is not a registered kind                       | `ok=0` | `bad-kind`      |
| 3 | `kind` is a one-shot (use `sound.play`)               | `ok=0` | `bad-kind`      |
| 4 | config denies the capability (`allow_scene_ambient`)  | `ok=0` | `not-permitted` |
| 5 | audio is locked: LATEST request retained              | `ok=1` | `deferred`      |
| 6 | same kind already playing or crossfading              | `ok=1` | — (no restart)  |
| 7 | otherwise: bed crossfades over the clamped `xfade`    | `ok=1` | —               |

- **Deferred is a qualified success (row 5).** A bed is stateful, not
  evental — a late start is honest. The request commits as retained
  state (`ok=1` with the qualifier `code=deferred`, the one code that
  rides a success) and fades in after the first user gesture unlocks
  audio. Only the LATEST pre-unlock request is retained; there is no
  later notification — poll `state.scene`.
- **Capability (row 4).** The scene-ambient capability is granted by
  trusted config only (`[audio] allow_scene_ambient`, default `true`);
  the wire can never grant it to itself. Transmissions and agents share
  the same local ingress today, so config is the only trusted tier;
  authenticated ingress tiers can carry the grant later.
- **Same-kind is idempotent (row 6).** Setting the kind that is already
  playing (or fading in) acks `ok=1` without restarting the loop or
  touching the running fade — this is what keeps looping transmissions
  seamless. A same-kind set on a bed that is *fading out* resurrects it
  (row 7: it crossfades back in).

### 3. `sound.ambient.stop` — fade the bed out

Fades the ambient bed to silence over the clamped `fade` and clears any
retained pre-unlock request. Always commits (feature-on): stopping
silence is an idempotent `ok=1`, and a stop while already fading out
leaves the running fade untouched.

### Reset

`ratty:reset` resets the sound organ silently (its single ack belongs to
the scene applier): the bed fades out, the retained pre-unlock request
clears, and in-flight one-shots finish. The per-namespace rate buckets
are deliberately left untouched — they are an anti-abuse accumulator, not
scene state, and refilling them on reset would let a script interleave
`ratty:reset` with `sound.play` to sustain far above the advertised rate.
Unlock status is a user-gesture fact, not scene state — reset never
re-locks (or unlocks) audio.

## Unlock gating (browser autoplay)

Native builds are born unlocked. Wasm builds start locked under browser
autoplay policy, and pre-unlock is the *normal* first-load path on the
site — the first transmission autoplays with no gesture, its one-shots
drop with `audio-locked`, and its ambient request defers.

Unlock happens on the first genuine user gesture, from either source:

- the page calls `session.user_gesture()` — the site installs one-time
  `pointerdown`/`keydown` window listeners at boot that also resume the
  browser's suspended `AudioContext`;
- the first real key press through the terminal's keyboard stream (a
  keystroke IS a gesture — the defensive path for embedders).

Both are frame-ordered before the decision layer, so the gesture
frame's own `sound.*` commands already see unlocked audio. On unlock
the retained ambient request (if any) fades in over the default 1500 ms.
The transition is observable only by polling `state.scene` — nothing is
pushed.

## Config — the trusted mixer tier

```toml
[audio]
master_gain = 0.8          # 0.0..=1.0, applied to all playback
muted = false              # silences playback without changing state
allow_scene_ambient = true # grants the sound.ambient.set capability
```

Master gain and mute are applied at playback time only, so config stays
authoritative over the mixer at every moment; per-play gains and the
decision table are unaffected by them. All fields are optional
(`#[serde(default)]` — additive-safe).

## Degradation without an audio device

`enabled` reports whether the playback backend is *compiled in*, not
whether an audio device is *present*. On a host with no output device (a
headless server, some CI), the backend keeps no audio manager and never
processes play commands, so a one-shot's backing instance never
materializes — the playback layer's normal "voice ends, free its slot"
signal never arrives.

So the decision layer reaps a committed voice a bounded time after it
commits (`VOICE_MAX_LIFETIME_SECS`, two seconds — every one-shot is well
under a second) rather than waiting for an instance end that will never
come. The caps therefore self-heal: plays keep acking per the decision
table instead of the 16-voice cap wedging shut and rejecting every later
play with `voice-cap`. `state.scene` still reports `enabled=true` (the
backend is built) and, once unlocked, `unlocked=true`, even though nothing
is audible — the honest bit an ack ever promises is that the *decision*
committed, not that a speaker moved.

## Queryable state and caps

`state.scene` (OSC 778) carries the sound organ's public state under the
append-only `audio` key:

```json
"audio": { "enabled": true, "unlocked": false,
           "ambient": { "kind": null, "phase": "idle" },
           "voices": 0 }
```

- `enabled` — whether the playback backend is compiled into this binary;
  feature-off builds report `false` honestly (the key shape is
  feature-independent). It does not promise an audio device is present —
  see [Degradation without an audio device](#degradation-without-an-audio-device).
- `unlocked` — the autoplay gate; poll this after issuing a deferred
  ambient request.
- `ambient.kind` / `ambient.phase` — the bed's registered kind (or
  `null`) and phase: `idle`, `playing`, `crossfading`, or `fading-out`.
- `voices` — live one-shot voice count.

`caps().limits` advertises `sound_voices` (16) and `sound_plays_per_sec`
(4), append-only like every caps key.

## Error codes

Appended to the shared OSC 778 code list (see [query.md](query.md)),
kebab-case, carried in the ack's `code=`:

- `bad-kind` — the kind is not registered, or its class does not match
  the op.
- `audio-locked` — a one-shot arrived while audio is locked; it did not
  and will not play.
- `deferred` — qualifier on an `ok=1` ack: the ambient request committed
  as retained state and fades in after the unlock gesture.
- `rate-limited` — the caller exceeded its per-namespace one-shot rate
  limit.
- `voice-cap` — the global or per-namespace voice cap is full.
- `not-permitted` — the caller's ingress tier does not carry the
  scene-ambient capability (trusted config denies it).
- `unsupported` — the binary was built without the `sound` feature.

## Example session

A browser session, pre-unlock — the site's normal first load:

```text
agent:  ESC ] 777 ; ratty:sound.play ; kind=chime&tok=s1 BEL
ratty:  ESC ] 778 ; v=1 ; t=r ; id=s1 ; kind=ack ; ok=0 ; code=audio-locked ESC \
agent:  ESC ] 777 ; ratty:sound.ambient.set ; kind=ambient.hum&gain=0.6&tok=s2 BEL
ratty:  ESC ] 778 ; v=1 ; t=r ; id=s2 ; kind=ack ; ok=1 ; code=deferred ESC \
agent:  ESC ] 778 ; v=1 ; t=q ; id=q1 ; op=state.scene ESC \
ratty:  ESC ] 778 ; v=1 ; t=r ; id=q1 ; ok=1 ; data=<b64url {… "audio":
        {"enabled":true,"unlocked":false,"ambient":{"kind":null,"phase":"idle"},
         "voices":0} …}> ESC \

        — the user clicks or presses a key: the first genuine gesture —

agent:  ESC ] 778 ; v=1 ; t=q ; id=q2 ; op=state.scene ESC \
ratty:  ESC ] 778 ; v=1 ; t=r ; id=q2 ; ok=1 ; data=<b64url {… "audio":
        {"enabled":true,"unlocked":true,"ambient":{"kind":"ambient.hum",
         "phase":"crossfading"},"voices":0} …}> ESC \
agent:  ESC ] 777 ; ratty:sound.play ; kind=chime&tok=s3 BEL
ratty:  ESC ] 778 ; v=1 ; t=r ; id=s3 ; kind=ack ; ok=1 ESC \
agent:  ESC ] 777 ; ratty:sound.ambient.stop ; fade=800&tok=s4 BEL
ratty:  ESC ] 778 ; v=1 ; t=r ; id=s4 ; kind=ack ; ok=1 ESC \
```

The dropped chime is gone for good; the ambient bed survived the lock as
retained state and faded in on the gesture; the post-unlock chime plays
the frame it commits.

## Client surfaces

The `ratty-ai` CLI mirrors the wire (add `--ack` to wait for the 778
ack):

```sh
ratty-ai sound play <kind> [--gain 0.8]
ratty-ai sound ambient set <kind> [--gain 0.5] [--xfade 1500]
ratty-ai sound ambient stop [--fade 1500]
```

Transmissions use the typed `sound` step in the Silk scene DSL, which
validates kinds at compile time against the same shared registry and
never emits `tok=` (a cast has no return channel) — see
[silk.md](silk.md), Sound inside Silk, for the authoring rules,
including why an ambient bed near `t=0` beats precisely-timed one-shots
in the browser.

## Summary

The sound family gives the 777 channel a voice with the same posture as
the rest of the protocol surface: semantic registered kinds instead of
files, server-side clamps instead of trust, honest same-frame acks
instead of queues, bounded voices and rates advertised in caps, config
as the only mixer authority, and unlock as polled state. A transmission
or agent can mark events and set a mood; it cannot play arbitrary audio,
and it never controls the user's volume.
