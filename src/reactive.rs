//! The reactive organ: registered declarative rules over sensors (#21, M3.8).
//!
//! A **rule** is data: a trigger — a registered-sensor reference plus a typed
//! condition with debounce, hysteresis, and cooldown — and one allowlisted
//! choreography **action**. Rules fire on state *transitions* (inactive →
//! active), never every frame. The old flat `React{}` command is retired;
//! the wire surface is `rule.set` / `rule.remove` / `rule.enable` /
//! `rule.disable` plus the caller-owned sensor commands `sensor.publish` /
//! `sensor.remove`.
//!
//! ## Sensors: trusted adapter + caller-owned wire sensors
//!
//! **Registering a rule never makes ratty inspect the host.** Sensor values
//! arrive from exactly two places: the config-gated native adapter
//! (`sys.cpu` / `sys.memory` / `sys.battery`, off by default — enabling it
//! is a trusted-config act; absent by construction on wasm and honestly
//! reported so), and caller-published wire sensors inside the caller's own
//! `agent.<ns>.*` namespace — typed, clamped, rate-limited, the
//! event-stream on-ramp that later collectors will use. Samples carry a
//! sequence number, a terminal-side timestamp, and a TTL; a stale or
//! removed sensor makes dependent rules **dormant** rather than supplying a
//! false value. A rule may reference a sensor before it exists: it
//! registers unbound and binds automatically when a compatible sensor
//! appears — rules and sensors may arrive in any order.
//!
//! ## Actions: choreography only, chain depth one
//!
//! The allowlist is the effects/presence family, `sound.play`,
//! `viz.effect`, live-field `object.update` on caller-owned objects (never
//! `x`/`y` — re-anchor is respawn-class), and `macro.play` of a
//! **rule-safe** macro (see [`crate::macros`]; the classification is
//! computed at finalize beside *privileged*, and a `rule.set` pins the
//! resolved macro's content hash so later name shadowing cannot swap the
//! target). Everything else — spawn/remove/clear, re-anchor, viz snapshot
//! replacement, scene-global presentation, control-plane commands — is
//! denied at `rule.set`.
//!
//! Recursion is closed structurally: `rule.*`/`sensor.*` are control-plane
//! (never recordable into macros, never valid rule actions), so no
//! choreography a rule can fire can ever publish a sensor or mutate a rule.
//! Belt-and-suspenders, every rule fire is stamped
//! [`CommandOrigin::Rule`] — inherited by a rule-started macro playback —
//! and this organ refuses rule-consumable input that did not arrive as
//! [`CommandOrigin::Wire`], while the macro recorder tap skips rule-origin
//! commands. Reactive chain depth is one; externally driven reactions are
//! preserved.
//!
//! ## Ownership tiers
//!
//! Wire rules are session-scoped and caller-owned (keyed by ingress
//! namespace); `reset` clears them along with wire sensors. Trusted config
//! rules ([`crate::config::ReactiveConfig`]) are seeded at startup, cannot
//! be mutated from the wire, and survive `reset`. A trusted rule that
//! fails semantic validation is seeded disabled and marked invalid —
//! queryable, never silently dropped, never a startup abort.
//!
//! ## Status
//!
//! Everything is queryable over OSC 778: `state.sensors` (freshness,
//! sequence, TTL, bindings) and `state.rules` (bound/dormant/active,
//! fire and suppression counts), both caller-scoped; `caps` advertises the
//! limits and the live system-sensor roster.

use std::collections::HashMap;
use std::time::Duration;

use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::prelude::*;
use serde_json::{Value, json};

use crate::ai::{AiCommand, CommandOrigin};
use crate::config::{AppConfig, TrustedRuleConfig};
use crate::macros::MacroRegistry;
use crate::osc::{RattyAiCommand, ai_object_namespace};
use crate::query::codes;
use crate::query_channel::{AckOutcome, DiagnosticsSink, ack_commit, reject};
use crate::runtime::IngressSource;

/// Upper bound on stored rules per agent namespace: an honest failure
/// instead of an unbounded registry driven by untrusted output.
pub const MAX_RULES_PER_NAMESPACE: usize = 32;

/// Upper bound on trusted config rules seeded at startup; overflow is an
/// explicit startup error log, never a silent truncation.
pub const MAX_TRUSTED_RULES: usize = 32;

/// Upper bound on wire sensors per agent namespace.
pub const MAX_SENSORS_PER_NAMESPACE: usize = 16;

/// Upper bound, in bytes, on a rule name (matches the macro bound).
pub const MAX_RULE_NAME_BYTES: usize = 64;

/// Upper bound, in bytes, on a wire sensor name suffix (the full name is
/// `agent.<ns>.<suffix>` and stays under the reference bound below).
pub const MAX_SENSOR_SUFFIX_BYTES: usize = 48;

/// Upper bound, in bytes, on a rule's sensor reference.
pub const MAX_SENSOR_REF_BYTES: usize = 64;

/// Rule fires re-injected per frame across all rules — the evaluator's
/// per-frame execution budget. A transition beyond the budget is latched
/// but its fire is suppressed (and counted), mirroring cooldown.
pub const MAX_RULE_FIRES_PER_FRAME: usize = 16;

/// Wire sensor publish burst per namespace (the token-bucket capacity).
pub const SENSOR_PUBLISH_BURST: u32 = 32;

/// Sustained wire sensor publishes per second per namespace (the
/// token-bucket refill rate).
pub const SENSOR_PUBLISHES_PER_SEC: u32 = 16;

/// Default TTL for a wire sensor sample, in seconds.
pub const DEFAULT_SENSOR_TTL_SECS: f32 = 10.0;

/// Lower clamp on a sample TTL, in seconds.
pub const MIN_SENSOR_TTL_SECS: f32 = 0.5;

/// Upper clamp on a sample TTL, in seconds.
pub const MAX_SENSOR_TTL_SECS: f32 = 3600.0;

/// Default rule cooldown, in seconds.
pub const DEFAULT_RULE_COOLDOWN_SECS: f32 = 1.0;

/// Lower clamp on a rule cooldown, in seconds — a floor so a rule can
/// never fire faster than the tightest action-side rate limits absorb.
pub const MIN_RULE_COOLDOWN_SECS: f32 = 0.25;

/// Upper clamp on debounce and cooldown durations, in seconds.
pub const MAX_RULE_SECS: f32 = 3600.0;

/// Default system-sensor sampling cadence, in seconds.
pub const DEFAULT_SYSTEM_SAMPLE_SECS: f32 = 2.0;

/// Lower clamp on the system-sensor sampling cadence, in seconds.
pub const MIN_SYSTEM_SAMPLE_SECS: f32 = 0.5;

/// Upper clamp on the system-sensor sampling cadence, in seconds.
pub const MAX_SYSTEM_SAMPLE_SECS: f32 = 60.0;

/// The sensor names the native adapter may supply. `sys.battery` is
/// published only on hosts that actually report a battery — honest
/// absence, never fabricated.
pub const SYSTEM_SENSOR_NAMES: &[&str] = &["sys.cpu", "sys.memory", "sys.battery"];

/// Where a sensor's samples come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorSource {
    /// The config-gated native adapter (`sys.*`).
    System,
    /// A caller-published wire sensor, owned by the given namespace.
    Wire(u8),
}

/// The latest sample of one registered sensor.
#[derive(Debug, Clone)]
struct SensorRecord {
    /// The latest value (validated finite at publish).
    value: f32,
    /// Monotonic per-sensor sequence number.
    seq: u64,
    /// `Time::elapsed` when the latest sample arrived.
    updated: Duration,
    /// Freshness lifetime; past it, dependent rules go dormant.
    ttl: Duration,
    /// Where the samples come from.
    source: SensorSource,
}

impl SensorRecord {
    fn fresh(&self, now: Duration) -> bool {
        now.saturating_sub(self.updated) <= self.ttl
    }
}

/// A rule trigger's comparison direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleCmp {
    /// Active when the sample is at or above the threshold.
    Above,
    /// Active when the sample is at or below the threshold.
    Below,
}

/// One registered rule: the declarative config plus its transition state.
#[derive(Debug, Clone)]
struct RuleRecord {
    /// The ingress context rule fires run under (the registering caller's
    /// for wire rules; local for trusted config rules).
    source: IngressSource,
    /// Referenced sensor name (full form).
    sensor: String,
    /// Comparison direction.
    cmp: RuleCmp,
    /// Activation threshold.
    threshold: f32,
    /// Hysteresis release threshold (on the release side of `threshold`).
    clear: f32,
    /// How long the raw condition must hold before activation.
    debounce: Duration,
    /// Minimum time between fires.
    cooldown: Duration,
    /// The validated, allowlisted action.
    action: RattyAiCommand,
    /// Whether the rule participates in evaluation.
    enabled: bool,
    /// When the raw condition first became continuously true (debounce
    /// tracking); `None` while the raw condition is false.
    raw_since: Option<Duration>,
    /// The latched condition state; the inactive→active edge fires.
    active: bool,
    /// When the rule last fired.
    last_fired: Option<Duration>,
    /// Fires so far.
    fires: u64,
    /// Transitions whose fire was suppressed (cooldown or frame budget).
    suppressed: u64,
    /// Set when a trusted config rule failed semantic validation: the
    /// human-readable reason, projected by `state.rules`. Invalid rules
    /// never evaluate.
    invalid: Option<String>,
}

/// Per-namespace wire-sensor publish token bucket
/// ([`SENSOR_PUBLISH_BURST`] capacity, [`SENSOR_PUBLISHES_PER_SEC`]
/// refill), mirroring the sound organ's play bucket.
#[derive(Debug, Clone, Copy)]
struct PublishBucket {
    tokens: f64,
    last: f64,
}

impl PublishBucket {
    fn new(now: f64) -> Self {
        Self {
            tokens: f64::from(SENSOR_PUBLISH_BURST),
            last: now,
        }
    }

    fn try_take(&mut self, now: f64) -> bool {
        let elapsed = (now - self.last).max(0.0);
        self.tokens = (self.tokens + elapsed * f64::from(SENSOR_PUBLISHES_PER_SEC))
            .min(f64::from(SENSOR_PUBLISH_BURST));
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// A rejection: the wire code plus a human message for the `state.errors`
/// ring. Registry methods return this; the system turns it into a `reject`.
type ReactiveReject = (&'static str, String);

/// The declarative trigger half of a `rule.set`, validated by
/// [`ReactiveRegistry::set_rule`]. Thresholds arrive exactly-one-sided
/// from the parser; everything else is optional with organ defaults.
#[derive(Debug, Clone, Copy)]
pub struct RuleSpec {
    /// Activation threshold: at or above.
    pub above: Option<f32>,
    /// Activation threshold: at or below.
    pub below: Option<f32>,
    /// Hysteresis release threshold.
    pub clear: Option<f32>,
    /// Debounce seconds.
    pub debounce: Option<f32>,
    /// Cooldown seconds.
    pub cooldown: Option<f32>,
}

/// The sensor and rule registries, publish rate buckets, and the adapter
/// capability flag — one resource, the reactive analog of
/// [`crate::macros::MacroRegistry`].
#[derive(Resource, Default)]
pub struct ReactiveRegistry {
    /// Registered sensors by full name (`sys.*`, `agent.<ns>.*`).
    sensors: HashMap<String, SensorRecord>,
    /// Wire rules, keyed by (namespace, name). Session-scoped; cleared by
    /// `reset`.
    session: HashMap<(u8, String), RuleRecord>,
    /// Trusted config rules, keyed by name. Wire-immutable; survive
    /// `reset`.
    trusted: HashMap<String, RuleRecord>,
    /// Per-namespace publish rate buckets (bounded: namespaces are 7-bit).
    publish_buckets: HashMap<u8, PublishBucket>,
    /// Whether the native system-sensor adapter is compiled in *and*
    /// granted by config — set once at startup, reported by `caps`.
    system_enabled: bool,
}

impl ReactiveRegistry {
    /// Number of wire rules stored in `namespace`.
    pub fn session_len(&self, namespace: u8) -> usize {
        self.session
            .keys()
            .filter(|(entry_namespace, _)| *entry_namespace == namespace)
            .count()
    }

    /// Whether any rule can evaluate (the evaluator's run condition).
    pub fn has_evaluable_rules(&self) -> bool {
        self.session
            .values()
            .chain(self.trusted.values())
            .any(|rule| rule.enabled && rule.invalid.is_none())
    }

    /// Whether the native system-sensor adapter is active in this process.
    pub fn system_sensors_enabled(&self) -> bool {
        self.system_enabled
    }

    /// The live system-supplied sensor names, for the honest `caps`
    /// roster (empty until the first sample, empty forever on wasm or
    /// when ungranted).
    pub fn live_system_sensors(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .sensors
            .iter()
            .filter(|(_, record)| record.source == SensorSource::System)
            .map(|(name, _)| name.as_str())
            .collect();
        names.sort_unstable();
        names
    }

    /// Validates a rule's sensor reference for `namespace`: `sys.*` or the
    /// caller's own `agent.<ns>.*`; a foreign agent namespace is
    /// `not-owner`, anything else malformed.
    fn validate_sensor_ref(namespace: u8, sensor: &str) -> Result<(), ReactiveReject> {
        if sensor.len() > MAX_SENSOR_REF_BYTES {
            return Err((
                codes::TOO_LARGE,
                format!("sensor reference exceeds {MAX_SENSOR_REF_BYTES} bytes"),
            ));
        }
        if let Some(suffix) = sensor.strip_prefix("sys.") {
            if !valid_sensor_component(suffix) {
                return Err((
                    codes::BAD_PAYLOAD,
                    format!("malformed sensor reference '{sensor}'"),
                ));
            }
            return Ok(());
        }
        if let Some(rest) = sensor.strip_prefix("agent.") {
            // A well-formed agent ref is `agent.<7-bit ns>.<suffix>`;
            // anything else is malformed, and only a well-formed *foreign*
            // ref is a `not-owner`.
            let parsed = rest.split_once('.').and_then(|(ns_str, suffix)| {
                let ns: u8 = ns_str.parse().ok().filter(|ns| *ns <= 0x7F)?;
                // Canonical spelling only ("agent.7.", never "agent.007."
                // or "agent.+7.") — sensors are keyed by the canonical
                // string, so a non-canonical ref could never bind.
                if ns_str != ns.to_string() {
                    return None;
                }
                valid_sensor_component(suffix).then_some(ns)
            });
            let Some(ns) = parsed else {
                return Err((
                    codes::BAD_PAYLOAD,
                    format!("malformed sensor reference '{sensor}'"),
                ));
            };
            if ns != namespace {
                return Err((
                    codes::NOT_OWNER,
                    format!(
                        "a rule may only reference sys.* or the caller's own agent.{namespace}.*"
                    ),
                ));
            }
            return Ok(());
        }
        Err((
            codes::BAD_PAYLOAD,
            format!("sensor references are sys.* or agent.{namespace}.* (got '{sensor}')"),
        ))
    }

    /// Validates the trigger numbers and the allowlisted action, resolving
    /// and hash-pinning a `macro.play` action. Shared by the wire path and
    /// the trusted config seed.
    fn validate_rule(
        namespace: u8,
        sensor: &str,
        spec: RuleSpec,
        mut action: RattyAiCommand,
        macros: &MacroRegistry,
    ) -> Result<(RuleCmp, f32, f32, Duration, Duration, RattyAiCommand), ReactiveReject> {
        Self::validate_sensor_ref(namespace, sensor)?;
        let (cmp, threshold) = match (spec.above, spec.below) {
            (Some(threshold), None) => (RuleCmp::Above, threshold),
            (None, Some(threshold)) => (RuleCmp::Below, threshold),
            // The parser enforces exactly-one; config rules reach here
            // unchecked.
            _ => {
                return Err((
                    codes::BAD_PAYLOAD,
                    "exactly one of above/below is required".to_string(),
                ));
            }
        };
        if !threshold.is_finite() {
            return Err((
                codes::BAD_PAYLOAD,
                "the threshold must be finite".to_string(),
            ));
        }
        let clear = spec.clear.unwrap_or(threshold);
        if !clear.is_finite() {
            return Err((codes::BAD_PAYLOAD, "clear= must be finite".to_string()));
        }
        let hysteresis_valid = match cmp {
            RuleCmp::Above => clear <= threshold,
            RuleCmp::Below => clear >= threshold,
        };
        if !hysteresis_valid {
            return Err((
                codes::BAD_PAYLOAD,
                "clear= must sit on the release side of the threshold".to_string(),
            ));
        }
        let debounce = match spec.debounce {
            None => Duration::ZERO,
            Some(value) if value.is_finite() && value >= 0.0 => {
                Duration::from_secs_f32(value.min(MAX_RULE_SECS))
            }
            Some(_) => {
                return Err((
                    codes::BAD_PAYLOAD,
                    "debounce= must be a finite value at or above 0".to_string(),
                ));
            }
        };
        let cooldown = match spec.cooldown {
            None => Duration::from_secs_f32(DEFAULT_RULE_COOLDOWN_SECS),
            Some(value) if value.is_finite() && value >= 0.0 => {
                Duration::from_secs_f32(value.clamp(MIN_RULE_COOLDOWN_SECS, MAX_RULE_SECS))
            }
            Some(_) => {
                return Err((
                    codes::BAD_PAYLOAD,
                    "cooldown= must be a finite value at or above 0".to_string(),
                ));
            }
        };

        // The action allowlist (#21): direct choreography, or macro.play of
        // a rule-safe macro — resolved now and pinned by content hash so
        // later name shadowing cannot swap the target.
        match &mut action {
            RattyAiCommand::MacroPlay {
                name, hash, scope, ..
            } => {
                let resolved = match hash.as_deref() {
                    Some(hash) => macros.resolve_by_hash(namespace, hash),
                    None => macros.resolve(namespace, name, *scope),
                };
                let Some(resolved) = resolved else {
                    return Err((
                        codes::UNKNOWN_ID,
                        "the rule's macro does not resolve; a rule action pins its macro at \
                         rule.set, so record it first"
                            .to_string(),
                    ));
                };
                if !resolved.is_rule_safe() {
                    return Err((
                        codes::NOT_PERMITTED,
                        "a rule may only play a rule-safe macro (every step in the allowlisted \
                         choreography class)"
                            .to_string(),
                    ));
                }
                *hash = Some(resolved.hash().to_string());
                // The scope served its purpose at resolution; the pinned
                // hash addresses the exact version from here on.
                *scope = None;
            }
            action if action.is_rule_safe_action() => {
                let target = match action {
                    RattyAiCommand::UpdateObject { id, .. } => Some(*id),
                    RattyAiCommand::VizEffect { id, .. } => Some(*id),
                    _ => None,
                };
                if let Some(id) = target
                    && ai_object_namespace(id) != Some(namespace)
                {
                    return Err((
                        codes::NOT_OWNER,
                        format!(
                            "action id {id:#010x} is outside the caller's AI range/namespace \
                             ({namespace})"
                        ),
                    ));
                }
            }
            _ => {
                return Err((
                    codes::NOT_PERMITTED,
                    "rule actions are choreography only: effects/presence, sound.play, \
                     viz.effect, live-field object.update, or macro.play of a rule-safe macro"
                        .to_string(),
                ));
            }
        }
        Ok((cmp, threshold, clear, debounce, cooldown, action))
    }

    /// Registers (or `mode=replace`s) a wire rule for `source`.
    #[allow(clippy::too_many_arguments)]
    fn set_rule(
        &mut self,
        source: IngressSource,
        name: &str,
        sensor: &str,
        spec: RuleSpec,
        action: RattyAiCommand,
        replace: bool,
        macros: &MacroRegistry,
    ) -> Result<(), ReactiveReject> {
        let namespace = source.namespace();
        if name.is_empty() {
            return Err((codes::BAD_PAYLOAD, "name= must be non-empty".to_string()));
        }
        if name.len() > MAX_RULE_NAME_BYTES {
            return Err((
                codes::TOO_LARGE,
                format!("name exceeds {MAX_RULE_NAME_BYTES} bytes"),
            ));
        }
        let exists = self.session.contains_key(&(namespace, name.to_string()));
        if exists && !replace {
            return Err((
                codes::ALREADY_EXISTS,
                format!("rule '{name}' exists (pass mode=replace to overwrite it)"),
            ));
        }
        if !exists && self.session_len(namespace) >= MAX_RULES_PER_NAMESPACE {
            return Err((
                codes::NAMESPACE_CAP,
                format!("namespace {namespace} is at its {MAX_RULES_PER_NAMESPACE}-rule limit"),
            ));
        }
        let (cmp, threshold, clear, debounce, cooldown, action) =
            Self::validate_rule(namespace, sensor, spec, action, macros)?;
        // Replacement resets the transition state: a replaced rule is a
        // fresh rule.
        self.session.insert(
            (namespace, name.to_string()),
            RuleRecord {
                source,
                sensor: sensor.to_string(),
                cmp,
                threshold,
                clear,
                debounce,
                cooldown,
                action,
                enabled: true,
                raw_since: None,
                active: false,
                last_fired: None,
                fires: 0,
                suppressed: 0,
                invalid: None,
            },
        );
        Ok(())
    }

    /// Removes one of `source`'s wire rules. Trusted rules are not
    /// wire-addressable: a trusted name answers a flat `unknown-id`.
    fn remove_rule(&mut self, source: IngressSource, name: &str) -> Result<(), ReactiveReject> {
        self.session
            .remove(&(source.namespace(), name.to_string()))
            .map(|_| ())
            .ok_or_else(|| (codes::UNKNOWN_ID, format!("no session rule named '{name}'")))
    }

    /// Enables or disables one of `source`'s wire rules. Disabling freezes
    /// the rule's transition state exactly as sensor dormancy does.
    fn set_rule_enabled(
        &mut self,
        source: IngressSource,
        name: &str,
        enabled: bool,
    ) -> Result<(), ReactiveReject> {
        let rule = self
            .session
            .get_mut(&(source.namespace(), name.to_string()))
            .ok_or_else(|| (codes::UNKNOWN_ID, format!("no session rule named '{name}'")))?;
        rule.enabled = enabled;
        // Disabling freezes the latch exactly as dormancy does — and
        // dormancy resets the debounce clock: time spent unobserved must
        // never count as continuous hold time.
        rule.raw_since = None;
        Ok(())
    }

    /// Publishes a caller-owned wire sensor sample. Upserts on first use;
    /// enforces the namespace cap, rate limit, finiteness, TTL clamps, and
    /// strictly-increasing caller-supplied sequences.
    fn publish_wire_sensor(
        &mut self,
        source: IngressSource,
        suffix: &str,
        value: f32,
        ttl: Option<f32>,
        seq: Option<u64>,
        now: Duration,
    ) -> Result<(), ReactiveReject> {
        let namespace = source.namespace();
        if suffix.is_empty() || !valid_sensor_component(suffix) {
            return Err((
                codes::BAD_PAYLOAD,
                "sensor names are ascii [A-Za-z0-9_.-], non-empty".to_string(),
            ));
        }
        if suffix.len() > MAX_SENSOR_SUFFIX_BYTES {
            return Err((
                codes::TOO_LARGE,
                format!("sensor name exceeds {MAX_SENSOR_SUFFIX_BYTES} bytes"),
            ));
        }
        if !value.is_finite() {
            return Err((codes::BAD_PAYLOAD, "value= must be finite".to_string()));
        }
        let ttl = match ttl {
            None => Duration::from_secs_f32(DEFAULT_SENSOR_TTL_SECS),
            Some(value) if value.is_finite() && value > 0.0 => {
                Duration::from_secs_f32(value.clamp(MIN_SENSOR_TTL_SECS, MAX_SENSOR_TTL_SECS))
            }
            Some(_) => {
                return Err((
                    codes::BAD_PAYLOAD,
                    "ttl= must be a finite value above 0".to_string(),
                ));
            }
        };
        if !self
            .publish_buckets
            .entry(namespace)
            .or_insert_with(|| PublishBucket::new(now.as_secs_f64()))
            .try_take(now.as_secs_f64())
        {
            return Err((
                codes::RATE_LIMITED,
                format!(
                    "sensor publishes are limited to {SENSOR_PUBLISHES_PER_SEC}/s \
                     (burst {SENSOR_PUBLISH_BURST}) per namespace"
                ),
            ));
        }
        let name = format!("agent.{namespace}.{suffix}");
        let existing = self.sensors.get(&name);
        if existing.is_none() && self.wire_sensor_count(namespace) >= MAX_SENSORS_PER_NAMESPACE {
            return Err((
                codes::NAMESPACE_CAP,
                format!("namespace {namespace} is at its {MAX_SENSORS_PER_NAMESPACE}-sensor limit"),
            ));
        }
        let current_seq = existing.map(|record| record.seq).unwrap_or(0);
        let seq = match seq {
            // Saturating: a caller parked just below the ceiling cannot
            // drive an overflow panic (debug) or a wrap to 0 that would
            // silently reset the strictly-increasing guarantee (release).
            None => current_seq.saturating_add(1),
            // u64::MAX is rejected outright so the sequence always has
            // headroom for the auto-increment path.
            Some(u64::MAX) => {
                return Err((
                    codes::BAD_PAYLOAD,
                    "seq= must be below u64::MAX".to_string(),
                ));
            }
            Some(seq) if seq > current_seq => seq,
            Some(seq) => {
                return Err((
                    codes::STALE_SEQ,
                    format!("seq {seq} is not above the sensor's current {current_seq}"),
                ));
            }
        };
        self.sensors.insert(
            name,
            SensorRecord {
                value,
                seq,
                updated: now,
                ttl,
                source: SensorSource::Wire(namespace),
            },
        );
        Ok(())
    }

    /// Removes one of `source`'s wire sensors; dependent rules fall back
    /// to the unbound state.
    fn remove_wire_sensor(
        &mut self,
        source: IngressSource,
        suffix: &str,
    ) -> Result<(), ReactiveReject> {
        let name = format!("agent.{}.{suffix}", source.namespace());
        self.sensors.remove(&name).map(|_| ()).ok_or_else(|| {
            (
                codes::UNKNOWN_ID,
                format!("no sensor named '{name}' in the caller's namespace"),
            )
        })
    }

    /// Publishes a system-adapter sample (trusted internal path — no caps,
    /// no rate limits; the adapter is config-gated and cadence-bounded).
    pub fn publish_system_sensor(&mut self, name: &str, value: f32, ttl: Duration, now: Duration) {
        let seq = self
            .sensors
            .get(name)
            .map(|record| record.seq)
            .unwrap_or(0)
            .saturating_add(1);
        self.sensors.insert(
            name.to_string(),
            SensorRecord {
                value,
                seq,
                updated: now,
                ttl,
                source: SensorSource::System,
            },
        );
    }

    /// Seeds one trusted config rule. Semantic failures seed the rule
    /// disabled and marked invalid — queryable, never silently dropped.
    pub fn insert_trusted_rule(
        &mut self,
        config: &TrustedRuleConfig,
        macros: &MacroRegistry,
        source: IngressSource,
    ) -> Result<(), ReactiveReject> {
        let name = config.name.clone();
        if name.is_empty() || name.len() > MAX_RULE_NAME_BYTES {
            return Err((
                codes::BAD_PAYLOAD,
                format!("trusted rule names are 1..={MAX_RULE_NAME_BYTES} bytes"),
            ));
        }
        if self.trusted.contains_key(&name) {
            return Err((
                codes::ALREADY_EXISTS,
                format!("a trusted rule named '{name}' is already seeded; the first entry wins"),
            ));
        }
        let spec = RuleSpec {
            above: config.above,
            below: config.below,
            clear: config.clear,
            debounce: config.debounce,
            cooldown: config.cooldown,
        };
        // Trusted rules run under the seeding terminal's ingress context
        // (the boot terminal — the only principal today); their sensor
        // references face the same syntax validation as wire rules,
        // scoped to that namespace.
        let parsed_action = crate::osc::parse_rule_action(&config.action);
        let validated = parsed_action
            .ok_or_else(|| {
                (
                    codes::BAD_PAYLOAD,
                    format!("action '{}' does not parse", config.action),
                )
            })
            .and_then(|action| {
                Self::validate_rule(source.namespace(), &config.sensor, spec, action, macros)
            });
        match validated {
            Ok((cmp, threshold, clear, debounce, cooldown, action)) => {
                self.trusted.insert(
                    name,
                    RuleRecord {
                        source,
                        sensor: config.sensor.clone(),
                        cmp,
                        threshold,
                        clear,
                        debounce,
                        cooldown,
                        action,
                        enabled: true,
                        raw_since: None,
                        active: false,
                        last_fired: None,
                        fires: 0,
                        suppressed: 0,
                        invalid: None,
                    },
                );
                Ok(())
            }
            Err((code, message)) => {
                // Seed the husk so `state.rules` shows the failure.
                self.trusted.insert(
                    name,
                    RuleRecord {
                        source,
                        sensor: config.sensor.clone(),
                        cmp: RuleCmp::Above,
                        threshold: 0.0,
                        clear: 0.0,
                        debounce: Duration::ZERO,
                        cooldown: Duration::from_secs_f32(DEFAULT_RULE_COOLDOWN_SECS),
                        action: RattyAiCommand::Think {
                            state: "toggle".to_string(),
                        },
                        enabled: false,
                        raw_since: None,
                        active: false,
                        last_fired: None,
                        fires: 0,
                        suppressed: 0,
                        invalid: Some(message.clone()),
                    },
                );
                Err((code, message))
            }
        }
    }

    /// Wire sensors owned by `namespace`.
    fn wire_sensor_count(&self, namespace: u8) -> usize {
        self.sensors
            .values()
            .filter(|record| record.source == SensorSource::Wire(namespace))
            .count()
    }

    /// Evaluates every enabled rule against the sensor registry, advancing
    /// transition state and returning the fires (owner ingress + action)
    /// this pass produced, bounded by [`MAX_RULE_FIRES_PER_FRAME`].
    ///
    /// Semantics, in order:
    ///
    /// - **unbound** (no sensor of that name) and **dormant** (sensor past
    ///   its TTL): evaluation pauses; the transition state is frozen and
    ///   the debounce clock resets.
    /// - **debounce**: the raw condition must hold continuously for the
    ///   debounce duration before the rule activates.
    /// - **transition**: the inactive→active edge fires. Registering a
    ///   rule while its condition already holds fires once the debounce
    ///   matures — the rule's own state transitions.
    /// - **hysteresis**: while active, the rule deactivates only when the
    ///   value crosses `clear` (release side); deactivation never fires.
    /// - **cooldown / frame budget**: a transition inside the cooldown or
    ///   past the per-frame fire budget is latched but its fire is
    ///   suppressed and counted.
    fn evaluate(&mut self, now: Duration) -> Vec<(IngressSource, RattyAiCommand)> {
        let mut fired = Vec::new();
        let mut budget = MAX_RULE_FIRES_PER_FRAME;
        let Self {
            sensors,
            session,
            trusted,
            ..
        } = self;
        for rule in session.values_mut().chain(trusted.values_mut()) {
            if !rule.enabled || rule.invalid.is_some() {
                continue;
            }
            let Some(sensor) = sensors.get(&rule.sensor) else {
                // Unbound: the rule waits for its sensor to appear.
                rule.raw_since = None;
                continue;
            };
            if !sensor.fresh(now) {
                // Dormant: never evaluate a stale value as if it were true.
                rule.raw_since = None;
                continue;
            }
            let value = sensor.value;
            if rule.active {
                let released = match rule.cmp {
                    RuleCmp::Above => value < rule.clear,
                    RuleCmp::Below => value > rule.clear,
                };
                if released {
                    rule.active = false;
                    rule.raw_since = None;
                }
                continue;
            }
            let raw = match rule.cmp {
                RuleCmp::Above => value >= rule.threshold,
                RuleCmp::Below => value <= rule.threshold,
            };
            if !raw {
                rule.raw_since = None;
                continue;
            }
            let since = *rule.raw_since.get_or_insert(now);
            if now.saturating_sub(since) < rule.debounce {
                continue;
            }
            // The transition: latch active, then decide the fire.
            rule.active = true;
            rule.raw_since = None;
            let cooled = rule
                .last_fired
                .is_none_or(|last| now.saturating_sub(last) >= rule.cooldown);
            if !cooled || budget == 0 {
                rule.suppressed += 1;
                continue;
            }
            budget -= 1;
            rule.fires += 1;
            rule.last_fired = Some(now);
            fired.push((rule.source, rule.action.clone()));
        }
        fired
    }

    /// Full session reset: clear wire rules, wire sensors, and rate
    /// buckets. Trusted rules and system sensors are trusted-tier state
    /// and survive; called from the `reset` tap, which acks elsewhere.
    fn reset(&mut self) {
        self.session.clear();
        self.publish_buckets.clear();
        self.sensors
            .retain(|_, record| record.source == SensorSource::System);
    }
}

/// Whether a sensor name component is well-formed: ascii alphanumerics
/// plus `_`, `-`, and `.`.
fn valid_sensor_component(component: &str) -> bool {
    !component.is_empty()
        && component
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

/// Registers the reactive registry and its systems.
///
/// Ordering: `apply_reactive_commands` runs after `pump_pty_output` (it
/// owns the `rule.*`/`sensor.*` acks) and after `apply_macro_commands`, so
/// a same-chunk `macro.stop` → `rule.set;do=macro.play` resolves the
/// freshly finalized macro. `evaluate_rules` runs after it and before the
/// choreography appliers, so a same-chunk `sensor.publish` can fire a rule
/// whose action lowers the same frame. (A rule-fired `macro.play` is the
/// one deliberate exception: `apply_macro_commands` has already run this
/// frame, so the playback starts next frame — invisible for a timed
/// choreography artifact.) `answer_queries` is ordered after both in
/// [`crate::ai::RattyAiPlugin`].
pub struct ReactivePlugin;

impl Plugin for ReactivePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ReactiveRegistry>()
            .add_systems(Startup, seed_reactive_from_config)
            .add_systems(
                Update,
                apply_reactive_commands
                    .after(crate::systems::pump_pty_output)
                    .after(crate::macros::apply_macro_commands),
            )
            .add_systems(
                Update,
                evaluate_rules
                    .after(apply_reactive_commands)
                    .before(crate::ai::apply_ai_commands)
                    .before(crate::ai::apply_ai_object_commands)
                    .before(crate::viz::apply_viz_commands)
                    .before(crate::sound::apply_sound_commands)
                    .before(crate::effects::apply_ai_effect_commands)
                    .before(crate::bookmarks::apply_bookmark_commands)
                    .run_if(|registry: Res<ReactiveRegistry>| registry.has_evaluable_rules()),
            );
        #[cfg(not(target_arch = "wasm32"))]
        app.add_systems(
            Update,
            sample_system_sensors
                .after(apply_reactive_commands)
                .before(evaluate_rules)
                .run_if(|config: Res<AppConfig>| config.reactive.system_sensors),
        );
    }
}

/// Seeds the trusted tier from config at startup: the adapter capability
/// flag and the persistent trusted rules. The first production
/// config-to-trusted-registry loader in the codebase — macros left theirs
/// as an explicit human act; reactive's design (#21) makes config rules a
/// first-class tier.
pub fn seed_reactive_from_config(
    config: Res<AppConfig>,
    macros: Res<MacroRegistry>,
    mut registry: ResMut<ReactiveRegistry>,
    seats: Query<&crate::identity::TerminalIdentity>,
) {
    // Trusted rules seed under the boot terminal's ingress context. This
    // Startup system runs while exactly one seat exists (main()/start()
    // spawn it pre-run); a world without it is broken, exactly like
    // setup_scene's seat expect.
    let identity = seats.single().expect("exactly one terminal seat at boot");
    // The cfg! term wins over config on wasm: an embedder can set the
    // flag, but no adapter exists to grant.
    registry.system_enabled = cfg!(not(target_arch = "wasm32")) && config.reactive.system_sensors;
    for (index, rule) in config.reactive.rules.iter().enumerate() {
        if index >= MAX_TRUSTED_RULES {
            error!(
                "ratty-reactive: [[reactive.rules]] exceeds the {MAX_TRUSTED_RULES}-rule limit; \
                 the rest are not seeded"
            );
            break;
        }
        if let Err((code, message)) =
            registry.insert_trusted_rule(rule, &macros, identity.ingress())
        {
            error!(
                "ratty-reactive: trusted rule '{}' is invalid ({code}): {message}; seeded \
                 disabled — see state.rules",
                rule.name
            );
        }
    }
}

/// Handles the `rule.*`/`sensor.*` control commands (owning their acks)
/// from the shared [`AiCommand`] stream. Rule-consumable input must arrive
/// as [`CommandOrigin::Wire`] — the #21 chain-depth guard.
#[allow(clippy::too_many_arguments)]
pub fn apply_reactive_commands(
    time: Res<Time>,
    mut commands: MessageReader<AiCommand>,
    mut registry: ResMut<ReactiveRegistry>,
    macros: Res<MacroRegistry>,
    mut acks: MessageWriter<AckOutcome>,
    mut diagnostics: DiagnosticsSink,
) {
    let now = time.elapsed();
    for AiCommand {
        source,
        ack_token,
        origin,
        command,
    } in commands.read()
    {
        macro_rules! reject {
            ($action:literal, $code:expr, $message:expr) => {
                reject(
                    &mut diagnostics,
                    &mut acks,
                    *source,
                    ack_token,
                    $action,
                    $code,
                    $message,
                )
            };
        }
        macro_rules! guard_wire_origin {
            ($action:literal) => {
                if *origin != CommandOrigin::Wire {
                    warn!(
                        "ratty-reactive: {} rejected: rule-consumable input must arrive from \
                         live ingress (chain depth 1)",
                        $action
                    );
                    reject!(
                        $action,
                        codes::NOT_PERMITTED,
                        "rule/sensor control must originate from live ingress (chain depth 1)"
                            .to_string()
                    );
                    continue;
                }
            };
        }
        match command {
            RattyAiCommand::RuleSet {
                name,
                sensor,
                above,
                below,
                clear,
                debounce,
                cooldown,
                replace,
                action,
            } => {
                guard_wire_origin!("rule.set");
                let spec = RuleSpec {
                    above: *above,
                    below: *below,
                    clear: *clear,
                    debounce: *debounce,
                    cooldown: *cooldown,
                };
                match registry.set_rule(
                    *source,
                    name,
                    sensor,
                    spec,
                    (**action).clone(),
                    *replace,
                    &macros,
                ) {
                    Ok(()) => ack_commit(&mut acks, *source, ack_token),
                    Err((code, message)) => {
                        warn!("ratty-reactive: rule.set rejected: {message}");
                        reject!("rule.set", code, message);
                    }
                }
            }
            RattyAiCommand::RuleRemove { name } => {
                guard_wire_origin!("rule.remove");
                match registry.remove_rule(*source, name) {
                    Ok(()) => ack_commit(&mut acks, *source, ack_token),
                    Err((code, message)) => {
                        warn!("ratty-reactive: rule.remove rejected: {message}");
                        reject!("rule.remove", code, message);
                    }
                }
            }
            RattyAiCommand::RuleEnable { name } => {
                guard_wire_origin!("rule.enable");
                match registry.set_rule_enabled(*source, name, true) {
                    Ok(()) => ack_commit(&mut acks, *source, ack_token),
                    Err((code, message)) => {
                        warn!("ratty-reactive: rule.enable rejected: {message}");
                        reject!("rule.enable", code, message);
                    }
                }
            }
            RattyAiCommand::RuleDisable { name } => {
                guard_wire_origin!("rule.disable");
                match registry.set_rule_enabled(*source, name, false) {
                    Ok(()) => ack_commit(&mut acks, *source, ack_token),
                    Err((code, message)) => {
                        warn!("ratty-reactive: rule.disable rejected: {message}");
                        reject!("rule.disable", code, message);
                    }
                }
            }
            RattyAiCommand::SensorPublish {
                name,
                value,
                ttl,
                seq,
            } => {
                guard_wire_origin!("sensor.publish");
                match registry.publish_wire_sensor(*source, name, *value, *ttl, *seq, now) {
                    Ok(()) => ack_commit(&mut acks, *source, ack_token),
                    Err((code, message)) => {
                        warn!("ratty-reactive: sensor.publish rejected: {message}");
                        reject!("sensor.publish", code, message);
                    }
                }
            }
            RattyAiCommand::SensorRemove { name } => {
                guard_wire_origin!("sensor.remove");
                match registry.remove_wire_sensor(*source, name) {
                    Ok(()) => ack_commit(&mut acks, *source, ack_token),
                    Err((code, message)) => {
                        warn!("ratty-reactive: sensor.remove rejected: {message}");
                        reject!("sensor.remove", code, message);
                    }
                }
            }
            RattyAiCommand::Reset => {
                // Full session reset. Reset's single ack belongs to
                // apply_ai_commands; the reactive registry clears its wire
                // tier silently.
                registry.reset();
            }
            _ => {}
        }
    }
}

/// Evaluates every enabled rule against the sensor registry and fires
/// transitions ([`ReactiveRegistry::evaluate`] holds the semantics),
/// re-injecting allowlisted actions into the [`AiCommand`] stream
/// token-less with [`CommandOrigin::Rule`] — the same mechanism as macro
/// playback, ordered before the choreography appliers so a fire lowers the
/// same frame.
pub fn evaluate_rules(
    time: Res<Time>,
    mut registry: ResMut<ReactiveRegistry>,
    mut commands: MessageWriter<AiCommand>,
) {
    for (source, command) in registry.evaluate(time.elapsed()) {
        commands.write(AiCommand {
            source,
            ack_token: None,
            origin: CommandOrigin::Rule,
            command,
        });
    }
}

/// The native system-sensor adapter: samples CPU, memory, and battery at
/// the config-bounded cadence and publishes `sys.*` sensors. Native-only —
/// wasm has no host adapter, honestly absent from the caps listing — and
/// gated off by default in config (`[reactive] system_sensors`).
#[cfg(not(target_arch = "wasm32"))]
pub fn sample_system_sensors(
    time: Res<Time>,
    config: Res<AppConfig>,
    mut registry: ResMut<ReactiveRegistry>,
    mut sampler: Local<Option<system_adapter::SystemSampler>>,
) {
    let now = time.elapsed();
    let cadence = sample_cadence_secs(&config.reactive);
    let sampler = sampler.get_or_insert_with(system_adapter::SystemSampler::new);
    if !sampler.due(now, Duration::from_secs_f32(cadence)) {
        return;
    }
    // Sensors stay fresh across a few missed cadences (a busy frame, a
    // paused loop) but go stale — dormant rules, never false values — if
    // the adapter stops.
    let ttl = Duration::from_secs_f32((cadence * 3.0).max(MIN_SENSOR_TTL_SECS));
    for (name, value) in sampler.sample() {
        registry.publish_system_sensor(name, value, ttl, now);
    }
}

/// The effective system-sensor sampling cadence: the configured value
/// clamped to the documented bounds, with a non-finite value (TOML admits
/// `nan`/`inf`) falling back to the default rather than poisoning the
/// clamp and panicking `Duration::from_secs_f32`.
pub fn sample_cadence_secs(config: &crate::config::ReactiveConfig) -> f32 {
    let configured = config
        .system_sample_secs
        .filter(|value| value.is_finite())
        .unwrap_or(DEFAULT_SYSTEM_SAMPLE_SECS);
    configured.clamp(MIN_SYSTEM_SAMPLE_SECS, MAX_SYSTEM_SAMPLE_SECS)
}

/// The sysinfo/starship-battery sampling state behind the native adapter.
#[cfg(not(target_arch = "wasm32"))]
pub mod system_adapter {
    use std::time::Duration;

    /// Owns the platform handles and the cadence clock.
    pub struct SystemSampler {
        sys: sysinfo::System,
        battery: Option<starship_battery::Manager>,
        last: Option<Duration>,
        /// CPU usage needs two refreshes before the first honest reading;
        /// the first sample is skipped rather than published as 0%.
        cpu_primed: bool,
    }

    impl Default for SystemSampler {
        fn default() -> Self {
            Self::new()
        }
    }

    impl SystemSampler {
        /// Constructs the platform handles. A host without a usable
        /// battery manager simply never publishes `sys.battery`.
        pub fn new() -> Self {
            Self {
                sys: sysinfo::System::new(),
                battery: starship_battery::Manager::new().ok(),
                last: None,
                cpu_primed: false,
            }
        }

        /// Whether a cadence period has elapsed; advances the clock when
        /// it has.
        pub fn due(&mut self, now: Duration, cadence: Duration) -> bool {
            let due = self
                .last
                .is_none_or(|last| now.saturating_sub(last) >= cadence);
            if due {
                self.last = Some(now);
            }
            due
        }

        /// Takes one sample of every honestly-available sensor, as
        /// `(name, percent 0..=100)` pairs.
        pub fn sample(&mut self) -> Vec<(&'static str, f32)> {
            let mut samples = Vec::with_capacity(3);
            self.sys.refresh_cpu_usage();
            if self.cpu_primed {
                samples.push(("sys.cpu", self.sys.global_cpu_usage()));
            }
            self.cpu_primed = true;
            self.sys.refresh_memory();
            let total = self.sys.total_memory();
            if total > 0 {
                let used = self.sys.used_memory() as f32;
                samples.push(("sys.memory", used / total as f32 * 100.0));
            }
            if let Some(manager) = &self.battery
                && let Ok(mut batteries) = manager.batteries()
                && let Some(Ok(battery)) = batteries.next()
            {
                samples.push(("sys.battery", battery.state_of_charge().value * 100.0));
            }
            samples
        }
    }
}

/// The canonical action name of an allowlisted rule action, for
/// `state.rules` projection rows.
fn action_name(action: &RattyAiCommand) -> &'static str {
    match action {
        RattyAiCommand::Flash { .. } => "flash",
        RattyAiCommand::Pulse { .. } => "pulse",
        RattyAiCommand::Tint { .. } => "tint",
        RattyAiCommand::Think { .. } => "think",
        RattyAiCommand::Confidence { .. } => "confidence",
        RattyAiCommand::Mood { .. } => "mood",
        RattyAiCommand::SoundPlay { .. } => "sound.play",
        RattyAiCommand::VizEffect { .. } => "viz.effect",
        RattyAiCommand::UpdateObject { .. } => "object.update",
        RattyAiCommand::MacroPlay { .. } => "macro.play",
        _ => "invalid",
    }
}

fn rule_row(name: &str, scope: &'static str, rule: &RuleRecord, now: Duration) -> Value {
    let mut row = json!({
        "name": name,
        "scope": scope,
        "sensor": rule.sensor,
        "cmp": match rule.cmp { RuleCmp::Above => "above", RuleCmp::Below => "below" },
        "threshold": rule.threshold,
        "clear": rule.clear,
        "debounce_secs": rule.debounce.as_secs_f32(),
        "cooldown_secs": rule.cooldown.as_secs_f32(),
        "action": action_name(&rule.action),
        "enabled": rule.enabled,
        "active": rule.active,
        "fires": rule.fires,
        "suppressed": rule.suppressed,
    });
    if let Some(last) = rule.last_fired {
        row["last_fired_secs_ago"] = json!(now.saturating_sub(last).as_secs_f32());
    }
    if let Some(reason) = &rule.invalid {
        row["invalid"] = json!(reason);
    }
    row
}

/// `state.rules`: the caller's wire rules plus the trusted rules, each
/// tagged with its scope and live binding state. Deterministically ordered
/// and paginated.
pub fn rules_state_items(
    registry: &ReactiveRegistry,
    namespace: u8,
    now: Duration,
) -> Vec<(u64, Value)> {
    let binding = |rule: &RuleRecord| {
        let sensor = registry.sensors.get(&rule.sensor);
        let bound = sensor.is_some();
        let dormant = sensor.is_some_and(|record| !record.fresh(now));
        (bound, dormant)
    };
    let mut rows: Vec<(String, Value)> = Vec::new();
    for ((entry_namespace, name), rule) in &registry.session {
        if *entry_namespace != namespace {
            continue;
        }
        let (bound, dormant) = binding(rule);
        let mut row = rule_row(name, "session", rule, now);
        row["bound"] = json!(bound);
        row["dormant"] = json!(dormant);
        rows.push((format!("session\u{0}{name}"), row));
    }
    for (name, rule) in &registry.trusted {
        let (bound, dormant) = binding(rule);
        let mut row = rule_row(name, "trusted", rule, now);
        row["bound"] = json!(bound);
        row["dormant"] = json!(dormant);
        rows.push((format!("trusted\u{0}{name}"), row));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.into_iter()
        .enumerate()
        .map(|(index, (_, value))| (index as u64, value))
        .collect()
}

/// `state.sensors`: the system sensors (scene-global trigger substrate,
/// readable by every caller) plus the caller's own wire sensors — never
/// another agent's. Each row carries freshness and how many caller-visible
/// rules reference it. Deterministically ordered and paginated.
pub fn sensors_state_items(
    registry: &ReactiveRegistry,
    namespace: u8,
    now: Duration,
) -> Vec<(u64, Value)> {
    let visible_rule_refs = |sensor_name: &str| {
        registry
            .session
            .iter()
            .filter(|((entry_namespace, _), _)| *entry_namespace == namespace)
            .map(|(_, rule)| rule)
            .chain(registry.trusted.values())
            .filter(|rule| rule.sensor == sensor_name)
            .count()
    };
    let mut rows: Vec<(String, Value)> = registry
        .sensors
        .iter()
        .filter(|(_, record)| match record.source {
            SensorSource::System => true,
            SensorSource::Wire(owner) => owner == namespace,
        })
        .map(|(name, record)| {
            (
                name.clone(),
                json!({
                    "name": name,
                    "value": record.value,
                    "seq": record.seq,
                    "age_secs": now.saturating_sub(record.updated).as_secs_f32(),
                    "ttl_secs": record.ttl.as_secs_f32(),
                    "fresh": record.fresh(now),
                    "source": match record.source {
                        SensorSource::System => "system",
                        SensorSource::Wire(_) => "wire",
                    },
                    "rules": visible_rule_refs(name),
                }),
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.into_iter()
        .enumerate()
        .map(|(index, (_, value))| (index as u64, value))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::message::Messages;

    const NS0: IngressSource = IngressSource::test_boot();

    fn t(secs: f32) -> Duration {
        Duration::from_secs_f32(secs)
    }

    fn think() -> RattyAiCommand {
        RattyAiCommand::Think {
            state: "start".to_string(),
        }
    }

    fn spec_above(threshold: f32) -> RuleSpec {
        RuleSpec {
            above: Some(threshold),
            below: None,
            clear: None,
            debounce: None,
            cooldown: None,
        }
    }

    fn set_simple(
        registry: &mut ReactiveRegistry,
        macros: &MacroRegistry,
        name: &str,
        sensor: &str,
        spec: RuleSpec,
    ) -> Result<(), ReactiveReject> {
        registry.set_rule(NS0, name, sensor, spec, think(), false, macros)
    }

    fn publish(registry: &mut ReactiveRegistry, suffix: &str, value: f32, now: Duration) {
        registry
            .publish_wire_sensor(NS0, suffix, value, None, None, now)
            .expect("registry op ok");
    }

    /// One pass of the real evaluator, returning just the fired actions.
    fn evaluate_at(registry: &mut ReactiveRegistry, now: Duration) -> Vec<RattyAiCommand> {
        registry
            .evaluate(now)
            .into_iter()
            .map(|(_, command)| command)
            .collect()
    }

    #[test]
    fn limits_are_pinned() {
        assert_eq!(MAX_RULES_PER_NAMESPACE, 32);
        assert_eq!(MAX_SENSORS_PER_NAMESPACE, 16);
        assert_eq!(MAX_RULE_NAME_BYTES, 64);
        assert_eq!(MAX_SENSOR_SUFFIX_BYTES, 48);
        assert_eq!(MAX_SENSOR_REF_BYTES, 64);
        assert_eq!(MAX_RULE_FIRES_PER_FRAME, 16);
        assert_eq!(SENSOR_PUBLISH_BURST, 32);
        assert_eq!(SENSOR_PUBLISHES_PER_SEC, 16);
        assert_eq!(DEFAULT_SENSOR_TTL_SECS, 10.0);
        assert_eq!(MIN_RULE_COOLDOWN_SECS, 0.25);
        assert_eq!(DEFAULT_SYSTEM_SAMPLE_SECS, 2.0);
    }

    #[test]
    fn rule_validation_rejects_bad_names_refs_and_numbers() {
        let macros = MacroRegistry::default();
        let mut registry = ReactiveRegistry::default();
        let (code, _) =
            set_simple(&mut registry, &macros, "", "sys.cpu", spec_above(1.0)).expect_err("empty");
        assert_eq!(code, codes::BAD_PAYLOAD);
        let long = "x".repeat(MAX_RULE_NAME_BYTES + 1);
        let (code, _) = set_simple(&mut registry, &macros, &long, "sys.cpu", spec_above(1.0))
            .expect_err("too long");
        assert_eq!(code, codes::TOO_LARGE);
        // Foreign agent namespace: not-owner. Malformed refs: bad-payload.
        let (code, _) = set_simple(&mut registry, &macros, "r", "agent.5.x", spec_above(1.0))
            .expect_err("foreign");
        assert_eq!(code, codes::NOT_OWNER);
        for bad_ref in [
            "cpu",
            "agent.x",
            "sys.",
            "agent.0.",
            "agent.0.bad name",
            // Non-canonical namespace spellings could never bind (sensors
            // are keyed by the canonical string), so they are malformed.
            "agent.00.load",
            "agent.+0.load",
        ] {
            let (code, _) = set_simple(&mut registry, &macros, "r", bad_ref, spec_above(1.0))
                .expect_err("malformed");
            assert_eq!(code, codes::BAD_PAYLOAD, "ref '{bad_ref}'");
        }
        // Non-finite numbers and inverted hysteresis reject.
        let (code, _) = set_simple(&mut registry, &macros, "r", "sys.cpu", spec_above(f32::NAN))
            .expect_err("nan threshold");
        assert_eq!(code, codes::BAD_PAYLOAD);
        let inverted = RuleSpec {
            clear: Some(90.0),
            ..spec_above(80.0)
        };
        let (code, _) =
            set_simple(&mut registry, &macros, "r", "sys.cpu", inverted).expect_err("inverted");
        assert_eq!(code, codes::BAD_PAYLOAD);
        let bad_cooldown = RuleSpec {
            cooldown: Some(-1.0),
            ..spec_above(80.0)
        };
        let (code, _) = set_simple(&mut registry, &macros, "r", "sys.cpu", bad_cooldown)
            .expect_err("negative cooldown");
        assert_eq!(code, codes::BAD_PAYLOAD);
    }

    #[test]
    fn collision_replace_and_namespace_cap() {
        let macros = MacroRegistry::default();
        let mut registry = ReactiveRegistry::default();
        set_simple(&mut registry, &macros, "r", "sys.cpu", spec_above(1.0))
            .expect("registry op ok");
        let (code, _) = set_simple(&mut registry, &macros, "r", "sys.cpu", spec_above(1.0))
            .expect_err("collision");
        assert_eq!(code, codes::ALREADY_EXISTS);
        registry
            .set_rule(NS0, "r", "sys.cpu", spec_above(2.0), think(), true, &macros)
            .expect("replace overwrites");
        for index in 1..MAX_RULES_PER_NAMESPACE {
            set_simple(
                &mut registry,
                &macros,
                &format!("r{index}"),
                "sys.cpu",
                spec_above(1.0),
            )
            .expect("registry op ok");
        }
        let (code, _) = set_simple(
            &mut registry,
            &macros,
            "overflow",
            "sys.cpu",
            spec_above(1.0),
        )
        .expect_err("at the cap");
        assert_eq!(code, codes::NAMESPACE_CAP);
        // Replacing at the cap is not a new slot.
        registry
            .set_rule(NS0, "r", "sys.cpu", spec_above(3.0), think(), true, &macros)
            .expect("registry op ok");
    }

    #[test]
    fn action_allowlist_is_enforced_at_rule_set() {
        let macros = MacroRegistry::default();
        let mut registry = ReactiveRegistry::default();
        // Denied classes reject not-permitted.
        for denied in [
            RattyAiCommand::SetMode {
                mode: "3d".to_string(),
            },
            RattyAiCommand::Reset,
            RattyAiCommand::ClearObjects,
            RattyAiCommand::SpawnObject {
                id: 0x8000_0001,
                path: "rat.obj".to_string(),
                x: 0,
                y: 0,
                scale: 1.0,
                spin: 0.0,
                brightness: 1.0,
                replace: false,
            },
            RattyAiCommand::VizSet {
                id: 0x8000_0001,
                kind: "ps.v1".to_string(),
                data: "e30".to_string(),
                x: None,
                y: None,
                cols: None,
                rows: None,
                replace: false,
            },
            RattyAiCommand::SoundAmbientSet {
                kind: "ambient.hum".to_string(),
                gain: None,
                xfade: None,
            },
            RattyAiCommand::MacroRecord {
                name: "m".to_string(),
                replace: false,
            },
            // Re-anchor is respawn-class.
            RattyAiCommand::UpdateObject {
                id: 0x8000_0001,
                x: Some(5),
                y: None,
                scale: None,
                spin: None,
                brightness: None,
            },
        ] {
            let (code, _) = registry
                .set_rule(NS0, "r", "sys.cpu", spec_above(1.0), denied, false, &macros)
                .expect_err("denied action");
            assert_eq!(code, codes::NOT_PERMITTED);
        }
        // Caller-owned-id validation on targeted actions.
        let foreign = RattyAiCommand::UpdateObject {
            id: 0x8100_0001,
            x: None,
            y: None,
            scale: None,
            spin: Some(1.0),
            brightness: None,
        };
        let (code, _) = registry
            .set_rule(
                NS0,
                "r",
                "sys.cpu",
                spec_above(1.0),
                foreign,
                false,
                &macros,
            )
            .expect_err("foreign target");
        assert_eq!(code, codes::NOT_OWNER);
        // The full direct allowlist commits.
        for (index, allowed) in [
            think(),
            RattyAiCommand::Flash {
                color: "#ff0000".to_string(),
                duration: 0.4,
            },
            RattyAiCommand::SoundPlay {
                kind: "chime".to_string(),
                gain: None,
            },
            RattyAiCommand::VizEffect {
                id: 0x8000_0001,
                key: "k".to_string(),
                effect: "highlight".to_string(),
            },
            RattyAiCommand::UpdateObject {
                id: 0x8000_0001,
                x: None,
                y: None,
                scale: Some(1.5),
                spin: Some(2.0),
                brightness: Some(0.8),
            },
        ]
        .into_iter()
        .enumerate()
        {
            registry
                .set_rule(
                    NS0,
                    &format!("ok{index}"),
                    "sys.cpu",
                    spec_above(1.0),
                    allowed,
                    false,
                    &macros,
                )
                .expect("allowlisted action commits");
        }
    }

    #[test]
    fn macro_play_actions_pin_a_rule_safe_macro_by_hash() {
        let mut macros = MacroRegistry::default();
        let mut registry = ReactiveRegistry::default();
        let play = |name: &str| RattyAiCommand::MacroPlay {
            name: name.to_string(),
            hash: None,
            rate: 1.0,
            instant: false,
            scope: None,
        };
        // Unresolvable macro: rejected (a rule pins its target at set).
        let (code, _) = registry
            .set_rule(
                NS0,
                "r",
                "sys.cpu",
                spec_above(1.0),
                play("ghost"),
                false,
                &macros,
            )
            .expect_err("no macro");
        assert_eq!(code, codes::UNKNOWN_ID);
        // A rule-safe macro pins by hash; a non-rule-safe one rejects.
        macros.test_record(NS0, "soft", &[think()]);
        macros.test_record(
            NS0,
            "hard",
            &[RattyAiCommand::SpawnObject {
                id: 0x8000_0001,
                path: "rat.obj".to_string(),
                x: 0,
                y: 0,
                scale: 1.0,
                spin: 0.0,
                brightness: 1.0,
                replace: false,
            }],
        );
        let (code, _) = registry
            .set_rule(
                NS0,
                "r",
                "sys.cpu",
                spec_above(1.0),
                play("hard"),
                false,
                &macros,
            )
            .expect_err("not rule-safe");
        assert_eq!(code, codes::NOT_PERMITTED);
        registry
            .set_rule(
                NS0,
                "r",
                "sys.cpu",
                spec_above(1.0),
                play("soft"),
                false,
                &macros,
            )
            .expect("registry op ok");
        let rule = registry.session.get(&(0, "r".to_string())).expect("stored");
        let RattyAiCommand::MacroPlay { hash, .. } = &rule.action else {
            panic!("macro.play action");
        };
        assert!(hash.is_some(), "the resolved macro version is hash-pinned");
    }

    #[test]
    fn transitions_fire_once_with_hysteresis_and_debounce() {
        let macros = MacroRegistry::default();
        let mut registry = ReactiveRegistry::default();
        let spec = RuleSpec {
            above: Some(80.0),
            below: None,
            clear: Some(70.0),
            debounce: Some(1.0),
            cooldown: Some(0.25),
        };
        set_simple(&mut registry, &macros, "cpu", "agent.0.load", spec).expect("registry op ok");

        // Below threshold: nothing.
        publish(&mut registry, "load", 50.0, t(0.0));
        assert!(evaluate_at(&mut registry, t(0.0)).is_empty());
        // Above threshold, debounce not yet mature: nothing.
        publish(&mut registry, "load", 90.0, t(1.0));
        assert!(evaluate_at(&mut registry, t(1.0)).is_empty());
        // Mature: fires exactly once.
        assert_eq!(evaluate_at(&mut registry, t(2.0)).len(), 1);
        // Still above: latched active, no re-fire every frame.
        assert!(evaluate_at(&mut registry, t(2.1)).is_empty());
        // Dropping to 75 is inside the hysteresis band: still active.
        publish(&mut registry, "load", 75.0, t(3.0));
        assert!(evaluate_at(&mut registry, t(3.0)).is_empty());
        publish(&mut registry, "load", 90.0, t(3.5));
        assert!(
            evaluate_at(&mut registry, t(3.5)).is_empty(),
            "re-rise inside the band is not a transition"
        );
        // Crossing clear releases; the next mature rise fires again.
        publish(&mut registry, "load", 60.0, t(4.0));
        assert!(evaluate_at(&mut registry, t(4.0)).is_empty());
        publish(&mut registry, "load", 95.0, t(5.0));
        assert!(evaluate_at(&mut registry, t(5.0)).is_empty(), "debounce");
        assert_eq!(evaluate_at(&mut registry, t(6.0)).len(), 1);
        // An interrupted debounce resets the clock.
        publish(&mut registry, "load", 60.0, t(7.0));
        evaluate_at(&mut registry, t(7.0));
        publish(&mut registry, "load", 95.0, t(8.0));
        assert!(evaluate_at(&mut registry, t(8.0)).is_empty());
        publish(&mut registry, "load", 60.0, t(8.5));
        evaluate_at(&mut registry, t(8.5));
        publish(&mut registry, "load", 95.0, t(9.0));
        assert!(
            evaluate_at(&mut registry, t(9.0)).is_empty(),
            "the hold restarts after the dip"
        );
    }

    #[test]
    fn cooldown_suppresses_but_latches_transitions() {
        let macros = MacroRegistry::default();
        let mut registry = ReactiveRegistry::default();
        let spec = RuleSpec {
            above: Some(80.0),
            below: None,
            clear: None,
            debounce: None,
            cooldown: Some(10.0),
        };
        set_simple(&mut registry, &macros, "cpu", "agent.0.load", spec).expect("registry op ok");
        publish(&mut registry, "load", 90.0, t(0.0));
        assert_eq!(evaluate_at(&mut registry, t(0.0)).len(), 1);
        // Release and re-rise inside the cooldown: the transition latches
        // (state is honest) but the fire is suppressed and counted.
        publish(&mut registry, "load", 10.0, t(1.0));
        evaluate_at(&mut registry, t(1.0));
        publish(&mut registry, "load", 90.0, t(2.0));
        assert!(evaluate_at(&mut registry, t(2.0)).is_empty());
        let rule = registry
            .session
            .get(&(0, "cpu".to_string()))
            .expect("stored");
        assert!(rule.active, "the suppressed transition still latched");
        assert_eq!(rule.fires, 1);
        assert_eq!(rule.suppressed, 1);
        // Past the cooldown, the next transition fires.
        publish(&mut registry, "load", 10.0, t(11.0));
        evaluate_at(&mut registry, t(11.0));
        publish(&mut registry, "load", 90.0, t(12.0));
        assert_eq!(evaluate_at(&mut registry, t(12.0)).len(), 1);
    }

    #[test]
    fn stale_sensors_make_rules_dormant_and_unbound_rules_bind_late() {
        let macros = MacroRegistry::default();
        let mut registry = ReactiveRegistry::default();
        // The rule references a sensor that does not exist yet: unbound,
        // registered, waiting.
        set_simple(
            &mut registry,
            &macros,
            "r",
            "agent.0.tokens",
            spec_above(5.0),
        )
        .expect("rules and sensors may arrive in any order");
        assert!(evaluate_at(&mut registry, t(0.0)).is_empty());
        // The sensor appears above threshold: binds and fires.
        publish(&mut registry, "tokens", 9.0, t(1.0));
        assert_eq!(evaluate_at(&mut registry, t(1.0)).len(), 1);
        // TTL expiry: dormant, never evaluating the stale value. The rule
        // stays latched (no false release either).
        let past_ttl = t(1.0 + DEFAULT_SENSOR_TTL_SECS + 1.0);
        assert!(evaluate_at(&mut registry, past_ttl).is_empty());
        let rule = registry.session.get(&(0, "r".to_string())).expect("rule");
        assert!(rule.active, "dormancy freezes state");
        // A fresh low sample releases; a fresh high one is a new
        // transition.
        publish(&mut registry, "tokens", 1.0, past_ttl + t(1.0));
        evaluate_at(&mut registry, past_ttl + t(1.0));
        publish(&mut registry, "tokens", 9.0, past_ttl + t(2.0));
        assert_eq!(evaluate_at(&mut registry, past_ttl + t(2.0)).len(), 1);
        // Removing the sensor returns the rule to unbound.
        registry
            .remove_wire_sensor(NS0, "tokens")
            .expect("registry op ok");
        assert!(evaluate_at(&mut registry, past_ttl + t(3.0)).is_empty());
    }

    #[test]
    fn sensor_publish_validates_and_rate_limits() {
        let mut registry = ReactiveRegistry::default();
        // Malformed names, values, TTLs.
        for (suffix, value, ttl) in [
            ("bad name", 1.0, None),
            ("", 1.0, None),
            ("ok", f32::NAN, None),
            ("ok", 1.0, Some(f32::INFINITY)),
            ("ok", 1.0, Some(0.0)),
        ] {
            let (code, _) = registry
                .publish_wire_sensor(NS0, suffix, value, ttl, None, t(0.0))
                .expect_err("invalid publish");
            assert_eq!(code, codes::BAD_PAYLOAD);
        }
        let long = "x".repeat(MAX_SENSOR_SUFFIX_BYTES + 1);
        let (code, _) = registry
            .publish_wire_sensor(NS0, &long, 1.0, None, None, t(0.0))
            .expect_err("too long");
        assert_eq!(code, codes::TOO_LARGE);
        // Stale/duplicate caller sequences reject; omitted seq increments.
        registry
            .publish_wire_sensor(NS0, "tokens", 1.0, None, Some(5), t(0.0))
            .expect("registry op ok");
        let (code, _) = registry
            .publish_wire_sensor(NS0, "tokens", 2.0, None, Some(5), t(0.1))
            .expect_err("duplicate seq");
        assert_eq!(code, codes::STALE_SEQ);
        registry
            .publish_wire_sensor(NS0, "tokens", 2.0, None, None, t(0.2))
            .expect("registry op ok");
        assert_eq!(
            registry.sensors["agent.0.tokens"].seq, 6,
            "omitted seq continues the sequence"
        );
        // The namespace sensor cap (the bucket allows a burst past it).
        for index in 1..MAX_SENSORS_PER_NAMESPACE {
            registry
                .publish_wire_sensor(NS0, &format!("s{index}"), 1.0, None, None, t(1.0))
                .expect("registry op ok");
        }
        let (code, _) = registry
            .publish_wire_sensor(NS0, "overflow", 1.0, None, None, t(1.0))
            .expect_err("at the cap");
        assert_eq!(code, codes::NAMESPACE_CAP);
        // Updates to an existing sensor are not a new slot.
        registry
            .publish_wire_sensor(NS0, "tokens", 3.0, None, None, t(1.1))
            .expect("registry op ok");
        // Exhausting the token bucket rate-limits.
        let mut limited = false;
        for _ in 0..(SENSOR_PUBLISH_BURST * 2) {
            if let Err((code, _)) =
                registry.publish_wire_sensor(NS0, "tokens", 3.0, None, None, t(1.2))
            {
                assert_eq!(code, codes::RATE_LIMITED);
                limited = true;
                break;
            }
        }
        assert!(limited, "the publish bucket caps the burst");
    }

    #[test]
    fn trusted_rules_survive_reset_and_are_not_wire_mutable() {
        let macros = MacroRegistry::default();
        let mut registry = ReactiveRegistry::default();
        let config = TrustedRuleConfig {
            name: "cpu-alarm".to_string(),
            sensor: "sys.cpu".to_string(),
            above: Some(85.0),
            below: None,
            clear: Some(70.0),
            debounce: Some(3.0),
            cooldown: Some(30.0),
            action: "flash;color=%23ff0000&duration=0.4".to_string(),
        };
        registry
            .insert_trusted_rule(&config, &macros, IngressSource::test_boot())
            .expect("registry op ok");
        // Wire mutation of a trusted name answers flat unknown-id.
        let (code, _) = registry
            .remove_rule(NS0, "cpu-alarm")
            .expect_err("not wire-mutable");
        assert_eq!(code, codes::UNKNOWN_ID);
        let (code, _) = registry
            .set_rule_enabled(NS0, "cpu-alarm", false)
            .expect_err("not wire-mutable");
        assert_eq!(code, codes::UNKNOWN_ID);
        // A wire rule and sensor die on reset; the trusted rule survives.
        set_simple(&mut registry, &macros, "mine", "agent.0.x", spec_above(1.0))
            .expect("registry op ok");
        publish(&mut registry, "x", 1.0, t(0.0));
        registry.reset();
        assert!(registry.session.is_empty(), "wire rules cleared");
        assert!(
            !registry.sensors.contains_key("agent.0.x"),
            "wire sensors cleared"
        );
        assert!(
            registry.trusted.contains_key("cpu-alarm"),
            "trusted survives reset"
        );
    }

    #[test]
    fn invalid_trusted_rules_seed_disabled_and_queryable() {
        let macros = MacroRegistry::default();
        let mut registry = ReactiveRegistry::default();
        let bad = TrustedRuleConfig {
            name: "broken".to_string(),
            sensor: "sys.cpu".to_string(),
            above: Some(85.0),
            below: None,
            clear: None,
            debounce: None,
            cooldown: None,
            // Denied class: spawn is not choreography.
            action: "object.add;id=2147483648&path=rat.obj".to_string(),
        };
        let (code, _) = registry
            .insert_trusted_rule(&bad, &macros, IngressSource::test_boot())
            .expect_err("invalid action");
        assert_eq!(code, codes::NOT_PERMITTED);
        let rule = registry.trusted.get("broken").expect("husk seeded");
        assert!(!rule.enabled);
        assert!(rule.invalid.is_some());
        // Projected with the invalid reason; never evaluated.
        let rows = rules_state_items(&registry, 0, t(0.0));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1["scope"], "trusted");
        assert!(rows[0].1["invalid"].is_string());
        registry.publish_system_sensor("sys.cpu", 99.0, t(60.0), t(0.0));
        assert!(evaluate_at(&mut registry, t(0.0)).is_empty());
    }

    #[test]
    fn state_projections_scope_sensors_and_report_bindings() {
        let macros = MacroRegistry::default();
        let mut registry = ReactiveRegistry::default();
        registry.publish_system_sensor("sys.cpu", 42.0, t(60.0), t(0.0));
        publish(&mut registry, "tokens", 7.0, t(0.0));
        // A foreign wire sensor must not be listed for namespace 0. (Only
        // one ingress exists today; the foreign record is pinned directly,
        // like the macro scene-lock tests.)
        registry.sensors.insert(
            "agent.5.secret".to_string(),
            SensorRecord {
                value: 1.0,
                seq: 1,
                updated: t(0.0),
                ttl: t(10.0),
                source: SensorSource::Wire(5),
            },
        );
        set_simple(
            &mut registry,
            &macros,
            "r",
            "agent.0.tokens",
            spec_above(5.0),
        )
        .expect("registry op ok");
        let rows = sensors_state_items(&registry, 0, t(1.0));
        let names: Vec<&str> = rows
            .iter()
            .map(|(_, row)| row["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, vec!["agent.0.tokens", "sys.cpu"]);
        let tokens = &rows[0].1;
        assert_eq!(tokens["source"], "wire");
        assert_eq!(tokens["rules"], 1);
        assert_eq!(tokens["fresh"], true);

        let rule_rows = rules_state_items(&registry, 0, t(1.0));
        assert_eq!(rule_rows.len(), 1);
        let row = &rule_rows[0].1;
        assert_eq!(row["scope"], "session");
        assert_eq!(row["bound"], true);
        assert_eq!(row["dormant"], false);
        assert_eq!(row["action"], "think");
        assert_eq!(row["enabled"], true);
    }

    #[test]
    fn sensor_seq_saturates_at_the_ceiling_instead_of_wrapping() {
        let mut registry = ReactiveRegistry::default();
        // The sentinel itself is rejected outright.
        let (code, _) = registry
            .publish_wire_sensor(NS0, "x", 1.0, None, Some(u64::MAX), t(0.0))
            .expect_err("u64::MAX seq is rejected");
        assert_eq!(code, codes::BAD_PAYLOAD);
        // Parked just below the ceiling, the auto-increment path saturates
        // instead of panicking (debug) or wrapping to 0 (release).
        registry
            .publish_wire_sensor(NS0, "x", 1.0, None, Some(u64::MAX - 1), t(0.1))
            .expect("registry op ok");
        registry
            .publish_wire_sensor(NS0, "x", 2.0, None, None, t(0.2))
            .expect("registry op ok");
        registry
            .publish_wire_sensor(NS0, "x", 3.0, None, None, t(0.3))
            .expect("registry op ok");
        assert_eq!(
            registry.sensors["agent.0.x"].seq,
            u64::MAX,
            "the sequence saturates; low sequences stay rejected"
        );
        let (code, _) = registry
            .publish_wire_sensor(NS0, "x", 4.0, None, Some(5), t(0.4))
            .expect_err("a low explicit seq is still stale");
        assert_eq!(code, codes::STALE_SEQ);
    }

    #[test]
    fn disable_resets_the_debounce_clock_like_dormancy() {
        let macros = MacroRegistry::default();
        let mut registry = ReactiveRegistry::default();
        let spec = RuleSpec {
            above: Some(80.0),
            below: None,
            clear: None,
            debounce: Some(10.0),
            cooldown: None,
        };
        set_simple(&mut registry, &macros, "r", "agent.0.load", spec).expect("registry op ok");
        // The raw condition starts holding at t=0…
        publish(&mut registry, "load", 90.0, t(0.0));
        assert!(evaluate_at(&mut registry, t(0.0)).is_empty());
        // …the rule is disabled at t=1 (the hold was only 1s observed)…
        registry
            .set_rule_enabled(NS0, "r", false)
            .expect("registry op ok");
        // …and re-enabled much later with the condition still high. The
        // unobserved gap must not count as continuous hold time.
        registry
            .set_rule_enabled(NS0, "r", true)
            .expect("registry op ok");
        publish(&mut registry, "load", 95.0, t(100.0));
        assert!(
            evaluate_at(&mut registry, t(100.0)).is_empty(),
            "the debounce restarts after an unobserved gap"
        );
        assert!(
            evaluate_at(&mut registry, t(110.0)).len() == 1,
            "a fresh mature hold fires"
        );
    }

    #[test]
    fn sample_cadence_survives_hostile_config_values() {
        let mut config = crate::config::ReactiveConfig::default();
        assert_eq!(sample_cadence_secs(&config), DEFAULT_SYSTEM_SAMPLE_SECS);
        config.system_sample_secs = Some(f32::NAN);
        assert_eq!(
            sample_cadence_secs(&config),
            DEFAULT_SYSTEM_SAMPLE_SECS,
            "a NaN cadence falls back instead of poisoning the clamp"
        );
        config.system_sample_secs = Some(f32::INFINITY);
        assert_eq!(sample_cadence_secs(&config), DEFAULT_SYSTEM_SAMPLE_SECS);
        config.system_sample_secs = Some(0.0);
        assert_eq!(sample_cadence_secs(&config), MIN_SYSTEM_SAMPLE_SECS);
        config.system_sample_secs = Some(1e9);
        assert_eq!(sample_cadence_secs(&config), MAX_SYSTEM_SAMPLE_SECS);
    }

    #[test]
    fn duplicate_trusted_rule_names_reject_loudly_first_wins() {
        let macros = MacroRegistry::default();
        let mut registry = ReactiveRegistry::default();
        let first = TrustedRuleConfig {
            name: "alarm".to_string(),
            sensor: "sys.cpu".to_string(),
            above: Some(85.0),
            below: None,
            clear: None,
            debounce: None,
            cooldown: None,
            action: "think".to_string(),
        };
        let second = TrustedRuleConfig {
            above: Some(10.0),
            ..first.clone()
        };
        registry
            .insert_trusted_rule(&first, &macros, IngressSource::test_boot())
            .expect("registry op ok");
        let (code, _) = registry
            .insert_trusted_rule(&second, &macros, IngressSource::test_boot())
            .expect_err("duplicate names reject");
        assert_eq!(code, codes::ALREADY_EXISTS);
        assert_eq!(
            registry.trusted["alarm"].threshold, 85.0,
            "the first entry wins"
        );
    }

    /// Exercises the real platform handles: the first CPU sample is
    /// skipped (never a fabricated 0%), and everything supplied is an
    /// honest percentage.
    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn system_sampler_supplies_honest_percentages() {
        let mut sampler = system_adapter::SystemSampler::new();
        assert!(sampler.due(t(0.0), t(2.0)), "first sample is due at once");
        assert!(!sampler.due(t(1.0), t(2.0)), "cadence gates the second");
        assert!(sampler.due(t(2.5), t(2.0)));
        let first = sampler.sample();
        assert!(
            first.iter().all(|(name, _)| *name != "sys.cpu"),
            "the unprimed CPU reading is skipped, not published as 0%"
        );
        let second = sampler.sample();
        assert!(
            second.iter().any(|(name, _)| *name == "sys.cpu"),
            "the primed CPU reading is supplied"
        );
        for (name, value) in first.iter().chain(&second) {
            assert!(
                (0.0..=100.0).contains(value),
                "{name} is a percentage, got {value}"
            );
        }
    }

    #[test]
    fn reactive_config_section_parses_and_seeds_trusted_rules() {
        let config = AppConfig::from_toml_str(
            r#"
[reactive]
system_sensors = true
system_sample_secs = 5.0

[[reactive.rules]]
name = "cpu-alarm"
sensor = "sys.cpu"
above = 85
clear = 70
debounce = 3.0
cooldown = 30.0
action = "flash;color=%23ff0000&duration=0.4"
"#,
        )
        .expect("the [reactive] section parses");
        assert!(config.reactive.system_sensors);
        assert_eq!(config.reactive.system_sample_secs, Some(5.0));
        assert_eq!(config.reactive.rules.len(), 1);

        let macros = MacroRegistry::default();
        let mut registry = ReactiveRegistry::default();
        registry
            .insert_trusted_rule(
                &config.reactive.rules[0],
                &macros,
                IngressSource::test_boot(),
            )
            .expect("the example rule seeds");
        let rule = registry.trusted.get("cpu-alarm").expect("seeded");
        assert!(rule.enabled);
        assert!(matches!(rule.action, RattyAiCommand::Flash { .. }));
        assert_eq!(rule.threshold, 85.0);
        assert_eq!(rule.clear, 70.0);
        // The default config keeps the adapter off and the rule list
        // empty — the locked #21 posture.
        let defaults = AppConfig::default();
        assert!(!defaults.reactive.system_sensors);
        assert!(defaults.reactive.rules.is_empty());
    }

    // ── Message-stream tier ──

    fn app_test() -> App {
        let mut app = App::new();
        app.init_resource::<ReactiveRegistry>();
        app.init_resource::<MacroRegistry>();
        app.world_mut().spawn((
            crate::identity::TerminalIdentity::test_boot(),
            crate::identity::terminal_session_state(),
        ));
        app.init_resource::<Time>();
        app.add_message::<AiCommand>();
        app.add_message::<AckOutcome>();
        app.add_systems(Update, (apply_reactive_commands, evaluate_rules).chain());
        app
    }

    fn send(app: &mut App, ack: Option<&str>, origin: CommandOrigin, command: RattyAiCommand) {
        app.world_mut()
            .resource_mut::<Messages<AiCommand>>()
            .write(AiCommand {
                source: IngressSource::test_boot(),
                ack_token: ack.map(str::to_string),
                origin,
                command,
            });
        app.update();
    }

    fn drain_acks(app: &mut App) -> Vec<AckOutcome> {
        app.world_mut()
            .resource_mut::<Messages<AckOutcome>>()
            .drain()
            .collect()
    }

    #[test]
    fn closed_loop_rule_set_publish_fire_over_the_message_stream() {
        let mut app = app_test();
        send(
            &mut app,
            Some("r"),
            CommandOrigin::Wire,
            RattyAiCommand::RuleSet {
                name: "hot".to_string(),
                sensor: "agent.0.load".to_string(),
                above: Some(80.0),
                below: None,
                clear: None,
                debounce: None,
                cooldown: None,
                replace: false,
                action: Box::new(think()),
            },
        );
        let acks = drain_acks(&mut app);
        assert_eq!(acks.len(), 1);
        assert!(acks[0].ok, "rule.set commits");

        // Clear the backlog, then publish above threshold: the fire enters
        // the AiCommand stream exactly once, token-less, rule-origin.
        app.world_mut()
            .resource_mut::<Messages<AiCommand>>()
            .clear();
        send(
            &mut app,
            Some("p"),
            CommandOrigin::Wire,
            RattyAiCommand::SensorPublish {
                name: "load".to_string(),
                value: 95.0,
                ttl: None,
                seq: None,
            },
        );
        assert!(drain_acks(&mut app)[0].ok, "publish commits");
        let stream: Vec<AiCommand> = app
            .world_mut()
            .resource_mut::<Messages<AiCommand>>()
            .drain()
            .collect();
        let fired: Vec<&AiCommand> = stream
            .iter()
            .filter(|message| matches!(message.command, RattyAiCommand::Think { .. }))
            .collect();
        assert_eq!(fired.len(), 1, "one transition, one fire");
        assert!(fired[0].ack_token.is_none(), "fires are token-less");
        assert_eq!(fired[0].origin, CommandOrigin::Rule);
        let registry = app.world().resource::<ReactiveRegistry>();
        let rule = registry.session.get(&(0, "hot".to_string())).expect("rule");
        assert_eq!(rule.fires, 1);
        assert!(rule.active);
    }

    #[test]
    fn non_wire_origin_control_is_rejected_by_the_chain_guard() {
        let mut app = app_test();
        // A rule-origin sensor publish can never feed the evaluator.
        send(
            &mut app,
            None,
            CommandOrigin::Rule,
            RattyAiCommand::SensorPublish {
                name: "load".to_string(),
                value: 95.0,
                ttl: None,
                seq: None,
            },
        );
        assert!(
            app.world()
                .resource::<ReactiveRegistry>()
                .sensors
                .is_empty(),
            "chain depth 1: rule-origin telemetry is refused"
        );
        // A macro-origin rule.set can never mutate rules.
        send(
            &mut app,
            None,
            CommandOrigin::Macro,
            RattyAiCommand::RuleSet {
                name: "smuggled".to_string(),
                sensor: "sys.cpu".to_string(),
                above: Some(1.0),
                below: None,
                clear: None,
                debounce: None,
                cooldown: None,
                replace: false,
                action: Box::new(think()),
            },
        );
        assert!(
            app.world()
                .resource::<ReactiveRegistry>()
                .session
                .is_empty(),
            "chain depth 1: non-wire rule mutation is refused"
        );
    }
}
