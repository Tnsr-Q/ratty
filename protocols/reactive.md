# Ratty Reactive Protocol (OSC 777 rule/sensor families)

Where the [Ratty Macros Protocol](macros.md) records choreography an agent
authored, the reactive organ lets an agent **register behavior that reacts
to the world**: a *rule* is data — a registered-sensor reference plus a
typed condition with debounce, hysteresis, and cooldown, and one
allowlisted choreography action — and rules **fire on state transitions,
never every frame**. The old flat `react` stub action is retired; the
surface is the `rule.*` and `sensor.*` families on the OSC 777 control
channel, with all status read back over [OSC 778](query.md).

## Design goals

- **Registering a rule never makes ratty inspect the host.** Sensor values
  arrive only from the config-gated native adapter or from callers
  publishing into their own namespace. The declarative sensor→action
  mapping is exactly what lets event-stream sources (agent telemetry,
  external collectors) replace local sensors later without touching the
  rule model.
- **Choreography only, chain depth one.** Rule actions come from a closed
  allowlist with no spawn/remove/re-anchor blast radius. Recursion is
  closed structurally: `rule.*`/`sensor.*` are control-plane — never
  recordable into macros, never valid rule actions — so nothing a rule can
  fire can ever publish a sensor or mutate a rule. Belt-and-suspenders,
  every fire carries rule-origin provenance (inherited by a rule-started
  macro playback): the macro recorder tap skips rule-origin commands, and
  the organ refuses rule-consumable input that did not arrive from live
  ingress.
- **Never a false value.** Samples carry a sequence number, a
  terminal-side timestamp, and a TTL. A stale or removed sensor makes
  dependent rules **dormant** — evaluation pauses, state freezes — rather
  than evaluating old data as if it were current.
- **Rules and sensors arrive in any order.** A rule may reference a sensor
  that does not exist yet: it registers **unbound** and binds
  automatically when a compatible sensor appears.
- **Two ownership tiers.** Wire rules are session-scoped and caller-owned;
  trusted config rules are persistent, cannot be mutated from the wire,
  and survive `reset`.
- **Bounded everything.** Rules and sensors per namespace, name lengths,
  publish rates, and fires per frame are capped and advertised in
  `caps().limits`.
- **Same rule engine native and wasm.** Only the native system-sensor
  adapter differs, and its absence is reported honestly.

## Transport

Reactive commands are ordinary OSC 777 control sequences:

```text
ESC ] 777 ; ratty:rule.set      ; name=<s>&sensor=<ref>&(above=<f32>|below=<f32>)[&clear=<f32>][&debounce=<f32>][&cooldown=<f32>][&mode=replace]&do=<action>[&tok=<t>] BEL
ESC ] 777 ; ratty:rule.remove   ; name=<s>[&tok=<t>] BEL
ESC ] 777 ; ratty:rule.enable   ; name=<s>[&tok=<t>] BEL
ESC ] 777 ; ratty:rule.disable  ; name=<s>[&tok=<t>] BEL
ESC ] 777 ; ratty:sensor.publish; name=<s>&value=<f32>[&ttl=<f32>][&seq=<u64>][&tok=<t>] BEL
ESC ] 777 ; ratty:sensor.remove ; name=<s>[&tok=<t>] BEL
```

- **Exactly one of `above=`/`below=`** is required; both or neither is a
  bad command. Trigger numbers parse **strictly** — a malformed value is a
  bad command, never a silently-absent field.
- `do=` carries the action in the same `<action>[;<payload>]` grammar as
  the wire itself, percent-encoded so its `;`/`&`/`=` survive the outer
  payload (e.g. `do=flash%3Bcolor%3D%2523ff0000`). The `rule.*` and
  `sensor.*` families are rejected inside `do=` by name, before parsing.
- `mode=` is the same closed vocabulary as `macro.record`: absent
  registers fresh, `replace` overwrites (an existing name otherwise
  rejects `already-exists`); replacement resets the rule's transition
  state.
- `sensor.publish` names carry only the **suffix**; the terminal prefixes
  the caller's `agent.<ns>.` — a caller can never publish into `sys.*` or
  another agent's namespace, by construction. `seq=` (optional) must be
  strictly increasing per sensor or the publish rejects `stale-seq`;
  omitted sequences auto-increment. `ttl=` is clamped to the advertised
  bounds; absent uses the default (~10 s).

## Trigger semantics

A rule's condition state is evaluated only while its sensor is **bound
and fresh**:

| State | Meaning |
| --- | --- |
| **unbound** | no sensor of the referenced name exists; the rule waits |
| **dormant** | the sensor exists but its latest sample is past TTL |
| **inactive / active** | the latched condition state; the inactive→active edge fires |

- **Threshold.** `above=` activates at or above; `below=` at or below.
- **Debounce.** The raw condition must hold continuously for `debounce=`
  seconds before the rule activates. A dip resets the clock; dormancy
  resets the clock.
- **Hysteresis.** While active, the rule deactivates only when the value
  crosses `clear=` on the release side (`clear <= above`, `clear >=
  below`; defaults to the threshold). Deactivation never fires.
- **Cooldown.** A transition inside `cooldown=` seconds of the last fire
  (or past the per-frame fire budget) is **latched but suppressed** — the
  state stays honest, the fire is skipped and counted. The cooldown has a
  floor (~0.25 s) so a rule can never outrun the action-side rate limits.
- **Registering into a true condition fires.** A rule whose condition
  already holds at registration activates once the debounce matures — its
  own state transitioned. Disabling a rule freezes its transition state
  exactly as dormancy does.
- Fires re-enter the ordinary command stream token-less and lower the
  same frame through the normal appliers (a rule-fired `macro.play`
  starts its playback next frame). Action failures land in the owner's
  `state.errors` ring like any fire-and-forget command.

## The action allowlist

| Allowed | Notes |
| --- | --- |
| `flash`, `pulse`, `tint`, `think`, `confidence`, `mood` | the effects/presence family; always commits |
| `sound.play` | registered one-shot kinds; subject to the sound organ's own gates |
| `viz.effect` | semantic-key effects on a caller-owned visualization |
| `object.update` (no `x`/`y`) | the live-field tier — scale/spin/brightness on a caller-owned object; re-anchor is respawn-class and disqualifies |
| `macro.play` | **only of a rule-safe macro** (below); the resolved version is pinned by content hash at `rule.set` |

Denied to rules: spawn, remove, clear, re-anchor, visualization snapshot
replacement, ambient audio, capture, scene-global presentation
(`mode`/`warp`/`reset`), asset registration, rule/sensor mutation, macro
recording, and query transport. `object.update`/`viz.effect` targets must
be caller-owned ids at `rule.set` (`not-owner` otherwise).

**Rule-safe macros.** *Rule-safe* joins *privileged* as a macro
classification computed at finalize ([macros.md](macros.md)): a macro is
rule-safe when **every** captured step is in the direct allowlist above.
A rule-safe macro is never privileged, so a rule can never contend for
the scene lock. `rule.set` resolves the macro (name/scope or hash),
rejects `unknown-id` if absent and `not-permitted` if not rule-safe, then
pins the resolved content hash — later name shadowing cannot swap the
target — and playback re-checks rule-safety at fire time. `state.macros`
rows now carry `rule_safe` alongside `privileged`.

## Sensors

- **Native adapter** (`sys.cpu`, `sys.memory`, `sys.battery` — percent
  values 0..=100): config-gated **off by default**; enabling it is a
  trusted-config act (`[reactive] system_sensors = true`), sampled at a
  config-bounded cadence (default ~2 s, sample TTL 3× cadence). Platform
  sensors that cannot be supplied honestly are **absent, never
  fabricated** — a desktop without a battery simply never publishes
  `sys.battery`, and the first CPU sample is skipped rather than
  published as a meaningless 0 %.
- **Wire sensors**: typed (finite f32), clamped, rate-limited
  (token-bucket per namespace), only inside the caller's own
  `agent.<ns>.*` namespace. Browser-equal by construction.
- **Browser/wasm**: no automatic host adapter — `caps` reports it
  honestly (below); wire-published sensors work identically. (On wasm the
  frame clock freezes with hidden tabs, so TTL/debounce inherit
  rAF-throttled time.)
- A rule may reference `sys.*` or the caller's **own** `agent.<ns>.*`
  only; a well-formed foreign reference rejects `not-owner`.

## Trust tiers and config

Wire rules and wire sensors die with the session; `reset` clears them
(system sensors and the trusted tier survive). Trusted rules are seeded
at startup from config — the first config→trusted-registry loader in the
tree — and are fixed for the process lifetime:

```toml
[reactive]
system_sensors = true      # off by default
system_sample_secs = 2.0

[[reactive.rules]]
name = "cpu-alarm"
sensor = "sys.cpu"
above = 85
clear = 70
debounce = 3.0
cooldown = 30.0
action = "flash;color=%23ff0000&duration=0.4"   # the wire `do=` grammar
```

A trusted rule that fails semantic validation (bad action, bad trigger)
is seeded **disabled and marked invalid** — visible in `state.rules` with
the reason, logged loudly at startup, never silently dropped and never a
startup abort. Wire mutation of a trusted name answers a flat
`unknown-id`. (A trusted rule with a `macro.play` action can only resolve
trusted macros — and no trusted-macro config loader exists yet, so such a
rule seeds invalid today.)

## Reading it back (OSC 778)

- **`state.rules`** *(paginated)* — the caller's wire rules plus the
  trusted rules: `name`, `scope` (`session`/`trusted`), `sensor`, `cmp`
  (`above`/`below`), `threshold`, `clear`, `debounce_secs`,
  `cooldown_secs`, `action` (canonical action name), `enabled`, `bound`,
  `dormant`, `active`, `fires`, `suppressed`, `last_fired_secs_ago?`,
  `invalid?`.
- **`state.sensors`** *(paginated)* — the system sensors (the
  scene-global trigger substrate, readable by every caller) plus the
  caller's **own** wire sensors — never another agent's: `name`, `value`,
  `seq`, `age_secs`, `ttl_secs`, `fresh`, `source`
  (`system`/`wire`), and `rules` (caller-visible rules referencing it).
- **`caps`** gains the limits below plus an honesty roster:

```json
"sensors": { "system_adapter": false, "system": [] }
```

`system_adapter` is whether the native adapter is compiled in **and**
granted by config (always `false` on wasm); `system` lists the sensors it
is currently supplying — live truth, never a promise.

## Limits

Advertised in `caps().limits`:

| Key | Meaning |
| --- | --- |
| `rules_per_namespace` | max wire rules per agent |
| `rule_name_bytes` | max rule-name length |
| `rule_fires_per_frame` | max rule fires re-injected per frame |
| `sensors_per_namespace` | max wire sensors per agent |
| `sensor_name_bytes` | max wire sensor-name (suffix) length |
| `sensor_publishes_per_sec` | sustained publish rate per namespace |
| `sensor_default_ttl_secs` | the TTL used when `ttl=` is absent |

## The closed loop

```text
# a caller-owned sensor drives a presence effect
ratty-ai rule set --name hot --sensor agent.0.load --above 80 \
    --clear 60 --cooldown 5 --do 'think;state=start'
ratty-ai sensor publish load 95        # → the rule binds, transitions, fires

# read it back
ratty-ai state rules                   # → [{ name: "hot", bound: true, active: true, fires: 1, … }]
ratty-ai state sensors                 # → [{ name: "agent.0.load", value: 95, fresh: true, … }]
```

## Native and wasm parity

The rule engine, wire sensors, and both query ops are identical on both
targets. The only divergence is the native system adapter, and it is
reported, never assumed: same JSON key shapes everywhere, with `caps`
carrying the live truth.
