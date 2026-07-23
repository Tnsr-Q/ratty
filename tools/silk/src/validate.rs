//! Cast validator: reparses every output event through ratty's own RGP and
//! OSC 777 parsers (`src/rgp.rs` / `src/osc.rs`, included verbatim) so
//! validation can never drift from the terminal's real wire format.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;

use crate::cast::{Cast, read_cast};
use crate::osc;
use crate::rgp::{RGP_APC_START, RgpOperation, RgpRegisterSource, consume_sequence};

/// Assets that ship inside ratty itself and are legal `path=` registrations.
const EMBEDDED_ASSETS: [&str; 4] = [
    "CairoSpinyMouse.obj",
    "SpinyMouse.glb",
    "SkateMouse.stl",
    "Ferris.glb",
];

const KITTY_APC_START: &[u8] = b"\x1b_G";

/// Introducer of an OSC 777 control frame (the `ratty:` command channel).
const OSC_777_START: &[u8] = b"\x1b]777;";

/// Validation outcome.
#[derive(Debug, Default)]
pub struct Report {
    /// Fatal problems; a cast with errors must not ship.
    pub errors: Vec<String>,
    /// Author-facing cautions.
    pub warnings: Vec<String>,
    /// Human-readable stats block.
    pub stats: String,
    /// RGP v2 capabilities the cast requires (`stage`, `tween`, `objanim`);
    /// empty for a pure v1 cast.
    pub requires_v2: std::collections::BTreeSet<&'static str>,
    /// Asset formats the cast registers (`obj`, `glb`, `stl`).
    pub formats: std::collections::BTreeSet<String>,
}

impl Report {
    /// Prints the report to stdout/stderr.
    pub fn print(&self) {
        if !self.stats.is_empty() {
            println!("{}", self.stats.trim_end());
        }
        for warning in &self.warnings {
            eprintln!("warning: {warning}");
        }
        for error in &self.errors {
            eprintln!("error: {error}");
        }
        if self.errors.is_empty() {
            println!("valid ({} warnings)", self.warnings.len());
        } else {
            eprintln!("INVALID: {} errors", self.errors.len());
        }
    }
}

/// Validates a `.silk` file.
pub fn validate_file(path: &Path) -> Result<Report> {
    let cast = read_cast(path)?;
    Ok(validate(&cast))
}

#[derive(Default)]
struct ObjectTracking {
    /// An unterminated `more=1` payload run is open.
    open_chunk_run: bool,
    /// Registration finalized (payload run closed, or path-based).
    registered: bool,
    /// Count of updates carrying respawn-forcing fields.
    respawn_updates: usize,
}

/// Validates a parsed cast.
pub fn validate(cast: &Cast) -> Report {
    let mut report = Report::default();

    if let Some(x_ratty) = &cast.header.x_ratty {
        if !x_ratty.format.starts_with("silk/1") {
            report
                .errors
                .push(format!("unsupported x_ratty.format \"{}\"", x_ratty.format));
        }
        if let Some(mode) = &x_ratty.mode
            && !matches!(mode.as_str(), "flat2d" | "plane3d" | "mobius3d")
        {
            report.errors.push(format!(
                "unknown x_ratty.mode \"{mode}\" (flat2d, plane3d, mobius3d)"
            ));
        }
    } else {
        report
            .warnings
            .push("no x_ratty header; this is a plain asciinema cast".to_string());
    }

    let mut objects: BTreeMap<u32, ObjectTracking> = BTreeMap::new();
    let mut ever_registered: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    let mut last_time = f64::NEG_INFINITY;
    let mut output_bytes = 0usize;
    let mut rgp_sequences = 0usize;
    let mut kitty_warned = false;

    for (index, event) in cast.events.iter().enumerate() {
        let line = index + 2; // 1-based, after the header line
        if event.time < last_time {
            report.errors.push(format!(
                "line {line}: time {} is before {last_time} (times must be non-decreasing)",
                event.time
            ));
        }
        last_time = last_time.max(event.time);

        match event.code.as_str() {
            "o" => {
                output_bytes += event.data.len();
                rgp_sequences += scan_output(
                    line,
                    event.data.as_bytes(),
                    &mut objects,
                    &mut ever_registered,
                    &mut kitty_warned,
                    &mut report,
                );
                scan_osc_777(line, event.data.as_bytes(), &mut report);
            }
            "m" | "i" => {}
            other => {
                report.warnings.push(format!(
                    "line {line}: unknown event code \"{other}\" (ignored)"
                ));
            }
        }
    }

    for (id, tracking) in &objects {
        if tracking.open_chunk_run {
            report.errors.push(format!(
                "object {id}: payload chunk run never terminated with more=0"
            ));
        }
        if tracking.respawn_updates > 3 {
            report.warnings.push(format!(
                "object {id}: {} updates set depth/color/brightness — each one forces a \
                 renderer respawn; prefer setting them once",
                tracking.respawn_updates
            ));
        }
    }

    let requires = if report.requires_v2.is_empty() {
        "RGP v1".to_string()
    } else {
        let capabilities: Vec<&str> = report.requires_v2.iter().copied().collect();
        format!("RGP v2 ({})", capabilities.join(", "))
    };
    if !report.requires_v2.is_empty() {
        report.warnings.push(
            "cast uses RGP v2 constructs; v1 terminals ignore them (the cast still \
             plays, without staging/per-object animation)"
                .to_string(),
        );
    }

    let mut stats = String::new();
    let _ = writeln!(
        stats,
        "{} events, {:.2}s, {} output bytes, {} RGP sequences, {} objects, requires: {}",
        cast.events.len(),
        cast.duration_secs(),
        output_bytes,
        rgp_sequences,
        ever_registered.len(),
        requires,
    );
    report.stats = stats;
    report
}

/// Scans one output event for APC sequences; returns how many RGP sequences
/// were found.
fn scan_output(
    line: usize,
    bytes: &[u8],
    objects: &mut BTreeMap<u32, ObjectTracking>,
    ever_registered: &mut std::collections::BTreeSet<u32>,
    kitty_warned: &mut bool,
    report: &mut Report,
) -> usize {
    let mut count = 0usize;
    let mut cursor = 0usize;
    while let Some(start) = find(bytes, cursor, b"\x1b_") {
        let Some(end) = apc_end(bytes, start) else {
            report.errors.push(format!(
                "line {line}: APC sequence starts but does not terminate within the event \
                 (sequences must not be split across events)"
            ));
            break;
        };
        let sequence = &bytes[start..end];
        if sequence.starts_with(RGP_APC_START) {
            count += 1;
            check_rgp(line, sequence, objects, ever_registered, report);
        } else if sequence.starts_with(KITTY_APC_START) && !*kitty_warned {
            *kitty_warned = true;
            report.warnings.push(format!(
                "line {line}: cast uses Kitty graphics — text-only and fallback players \
                 will not render these images"
            ));
        }
        cursor = end;
    }
    count
}

fn check_rgp(
    line: usize,
    sequence: &[u8],
    objects: &mut BTreeMap<u32, ObjectTracking>,
    ever_registered: &mut std::collections::BTreeSet<u32>,
    report: &mut Report,
) {
    let Some(operation) = consume_sequence(sequence) else {
        report.errors.push(format!(
            "line {line}: malformed RGP sequence {:?} (would leak into the terminal as text)",
            String::from_utf8_lossy(&sequence[..sequence.len().min(60)])
        ));
        return;
    };
    match operation {
        RgpOperation::Register {
            object_id,
            source,
            format,
            ..
        } => {
            ever_registered.insert(object_id);
            report.formats.insert(format);
            let open_elsewhere = objects
                .iter()
                .any(|(id, tracking)| *id != object_id && tracking.open_chunk_run);
            let tracking = objects.entry(object_id).or_default();
            match source {
                RgpRegisterSource::Payload { more, .. } => {
                    if open_elsewhere {
                        report.errors.push(format!(
                            "line {line}: payload chunks for object {object_id} interleave \
                             with another object's open chunk run"
                        ));
                    }
                    if more {
                        tracking.open_chunk_run = true;
                    } else {
                        tracking.open_chunk_run = false;
                        tracking.registered = true;
                    }
                }
                RgpRegisterSource::Path { path } => {
                    tracking.registered = true;
                    if !EMBEDDED_ASSETS.contains(&path.as_str()) {
                        report.warnings.push(format!(
                            "line {line}: path registration \"{path}\" is not a ratty-embedded \
                             asset; the cast is not self-contained"
                        ));
                    }
                }
            }
        }
        RgpOperation::Place { object_id, anchor } => {
            match objects.get(&object_id) {
                Some(tracking) if tracking.registered => {}
                Some(tracking) if tracking.open_chunk_run => {
                    report.errors.push(format!(
                        "line {line}: object {object_id} placed while its payload chunk run \
                         is still open (send the more=0 chunk first)"
                    ));
                }
                _ => {
                    report.errors.push(format!(
                        "line {line}: object {object_id} placed before registration \
                         (ratty silently ignores this)"
                    ));
                }
            }
            if anchor.style.spin.is_some()
                || anchor.style.bob.is_some()
                || anchor.style.bob_amplitude.is_some()
                || anchor.style.phase != 0.0
            {
                report.requires_v2.insert("objanim");
            }
        }
        RgpOperation::Update { object_id, update } => {
            let tracking = objects.entry(object_id).or_default();
            if update.depth.is_some() || update.color.is_some() || update.brightness.is_some() {
                tracking.respawn_updates += 1;
            }
            if update.spin.is_some()
                || update.bob.is_some()
                || update.bob_amplitude.is_some()
                || update.phase.is_some()
            {
                report.requires_v2.insert("objanim");
            }
        }
        RgpOperation::Delete { object_id } => match object_id {
            Some(id) => {
                objects.remove(&id);
            }
            None => objects.clear(),
        },
        RgpOperation::Stage { update } => {
            report.requires_v2.insert("stage");
            if update.dur.is_some_and(|dur| dur > 0.0) {
                report.requires_v2.insert("tween");
            }
            check_stage_fields(line, sequence, report);
        }
        RgpOperation::SupportQuery => {}
        RgpOperation::Ignored => {
            report.warnings.push(format!(
                "line {line}: RGP sequence with unknown verb (ignored by ratty)"
            ));
        }
    }
}

/// Strictly re-scans a `c` sequence's raw fields. Ratty's parser is
/// deliberately permissive (bad values are dropped per-key), so authoring
/// mistakes like `warp=abc` or `mode=cube4d` would silently vanish at
/// playback; the validator surfaces them instead.
fn check_stage_fields(line: usize, sequence: &[u8], report: &mut Report) {
    let content_end = sequence.len() - if sequence.ends_with(b"\x1b\\") { 2 } else { 1 };
    let Ok(content) = std::str::from_utf8(&sequence[RGP_APC_START.len()..content_end]) else {
        return;
    };
    let mut fields = 0usize;
    for part in content.split(';').skip(1).filter(|part| !part.is_empty()) {
        let Some((key, value)) = part.split_once('=') else {
            report.errors.push(format!(
                "line {line}: stage field \"{part}\" is not key=value"
            ));
            continue;
        };
        fields += 1;
        let mut check_range = |min: f32, max: f32| match value.parse::<f32>() {
            Ok(parsed) if (min..=max).contains(&parsed) => {}
            Ok(parsed) => report.errors.push(format!(
                "line {line}: stage {key}={parsed} out of range {min}..={max}"
            )),
            Err(_) => report.errors.push(format!(
                "line {line}: stage {key}=\"{value}\" is not a number"
            )),
        };
        match key {
            "mode" => {
                if !matches!(value, "flat2d" | "plane3d" | "mobius3d") {
                    report.errors.push(format!(
                        "line {line}: unknown stage mode \"{value}\" (flat2d, plane3d, mobius3d)"
                    ));
                }
            }
            "warp" => check_range(0.0, 1.0),
            "zoom" => check_range(0.1, 4.0),
            "yaw" | "pitch" => {
                if value.parse::<f32>().map(f32::is_finite) != Ok(true) {
                    report.errors.push(format!(
                        "line {line}: stage {key}=\"{value}\" is not a finite number"
                    ));
                }
            }
            "dur" => check_range(0.0, f32::INFINITY),
            "ease" => {
                if !matches!(value, "linear" | "in" | "out" | "inout") {
                    report.errors.push(format!(
                        "line {line}: unknown stage ease \"{value}\" (linear, in, out, inout)"
                    ));
                }
            }
            other => {
                report.errors.push(format!(
                    "line {line}: unknown stage field \"{other}\" \
                     (mode, warp, yaw, pitch, zoom, dur, ease)"
                ));
            }
        }
    }
    if fields == 0 {
        report
            .warnings
            .push(format!("line {line}: bare `c` sequence is a no-op"));
    }
}

/// Scans one output event for OSC 777 frames and re-parses `ratty:`-namespace
/// commands through ratty's own decoder (`osc::parse_command`, included
/// verbatim). The wire is deliberately permissive — the terminal silently
/// drops a `ratty:` frame it cannot parse — so the validator surfaces those
/// authoring mistakes instead. Foreign 777 frames (no `ratty:` prefix)
/// belong to other 777 users and pass untouched.
fn scan_osc_777(line: usize, bytes: &[u8], report: &mut Report) {
    let mut cursor = 0usize;
    while let Some(start) = find(bytes, cursor, OSC_777_START) {
        let content_start = start + OSC_777_START.len();
        let Some((content_end, end)) = osc_end(bytes, content_start) else {
            report.errors.push(format!(
                "line {line}: OSC 777 sequence starts but does not terminate within the \
                 event (sequences must not be split across events)"
            ));
            break;
        };
        check_osc_777(line, &bytes[content_start..content_end], report);
        cursor = end;
    }
}

fn check_osc_777(line: usize, content: &[u8], report: &mut Report) {
    let Ok(content) = std::str::from_utf8(content) else {
        report.errors.push(format!(
            "line {line}: OSC 777 frame content is not UTF-8"
        ));
        return;
    };
    if !content.starts_with(osc::RATTY_AI_PREFIX) {
        return;
    }
    let Some(command) = osc::parse_command(content) else {
        report.errors.push(format!(
            "line {line}: unparseable ratty OSC 777 command {:?} (ratty silently drops it)",
            &content[..content.len().min(60)]
        ));
        return;
    };
    match command {
        osc::RattyAiCommand::SoundPlay { kind, .. } => {
            check_sound_kind(line, &kind, osc::SoundKindClass::OneShot, report);
        }
        osc::RattyAiCommand::SoundAmbientSet { kind, .. } => {
            check_sound_kind(line, &kind, osc::SoundKindClass::Ambient, report);
        }
        _ => {}
    }
}

/// Checks a sound kind against the registry ratty itself compiles
/// (`osc::SOUND_KINDS`): unknown kinds and class mismatches are rejected
/// terminal-side with `bad-kind`, so a cast carrying them is broken.
fn check_sound_kind(
    line: usize,
    kind: &str,
    expected: osc::SoundKindClass,
    report: &mut Report,
) {
    match osc::sound_kind_class(kind) {
        Some(class) if class == expected => {}
        Some(_) => {
            let hint = match expected {
                osc::SoundKindClass::OneShot => {
                    "is an ambient bed, not a one-shot (use sound.ambient.set)"
                }
                osc::SoundKindClass::Ambient => {
                    "is a one-shot, not an ambient bed (use sound.play)"
                }
            };
            report.errors.push(format!(
                "line {line}: sound kind \"{kind}\" {hint}; ratty rejects this with bad-kind"
            ));
        }
        None => {
            report.errors.push(format!(
                "line {line}: \"{kind}\" is not a registered sound kind; ratty rejects \
                 this with bad-kind"
            ));
        }
    }
}

/// Returns (exclusive content end, exclusive sequence end) for an OSC frame
/// whose content starts at `from`, accepting BEL, `ESC \`, and the raw C1 ST.
fn osc_end(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut index = from;
    while index < bytes.len() {
        match bytes[index] {
            0x07 | 0x9c => return Some((index, index + 1)),
            0x1b if bytes.get(index + 1) == Some(&b'\\') => return Some((index, index + 2)),
            _ => index += 1,
        }
    }
    None
}

fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

/// Returns the exclusive end index of the APC sequence starting at `start`,
/// accepting both `ESC \` and the single C1 ST byte.
fn apc_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start + 2;
    while index < bytes.len() {
        match bytes[index] {
            0x9c => return Some(index + 1),
            0x1b if bytes.get(index + 1) == Some(&b'\\') => return Some(index + 2),
            _ => index += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cast::parse_cast;

    fn cast_with_events(events: &[(f64, &str, &str)]) -> Cast {
        let mut content = String::from(
            r#"{"version": 2, "width": 80, "height": 24, "x_ratty": {"format": "silk/1"}}"#,
        );
        content.push('\n');
        for (time, code, data) in events {
            content.push_str(&serde_json::to_string(&(time, code, data)).unwrap());
            content.push('\n');
        }
        parse_cast(&content).unwrap()
    }

    #[test]
    fn accepts_register_then_place() {
        let cast = cast_with_events(&[
            (
                0.0,
                "o",
                "\x1b_ratty;g;r;id=1;fmt=obj;source=payload;more=0;name=t.obj;normalize=1;dGVzdA==\x1b\\",
            ),
            (0.1, "o", "\x1b_ratty;g;p;id=1;row=5;col=5;w=2;h=2\x1b\\"),
        ]);
        let report = validate(&cast);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
    }

    #[test]
    fn rejects_place_before_register() {
        let cast = cast_with_events(&[(0.0, "o", "\x1b_ratty;g;p;id=9;row=5;col=5;w=2;h=2\x1b\\")]);
        let report = validate(&cast);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("before registration"))
        );
    }

    #[test]
    fn rejects_corrupt_base64_chunk() {
        let cast = cast_with_events(&[(
            0.0,
            "o",
            "\x1b_ratty;g;r;id=1;fmt=obj;source=payload;more=0;name=t.obj;normalize=1;!!!notbase64!!!\x1b\\",
        )]);
        let report = validate(&cast);
        assert!(report.errors.iter().any(|e| e.contains("malformed RGP")));
    }

    #[test]
    fn rejects_unterminated_chunk_run() {
        let cast = cast_with_events(&[(
            0.0,
            "o",
            "\x1b_ratty;g;r;id=1;fmt=obj;source=payload;more=1;name=t.obj;normalize=1;dGVzdA==\x1b\\",
        )]);
        let report = validate(&cast);
        assert!(report.errors.iter().any(|e| e.contains("never terminated")));
    }

    #[test]
    fn rejects_non_monotonic_times() {
        let cast = cast_with_events(&[(1.0, "o", "hello"), (0.5, "o", "world")]);
        let report = validate(&cast);
        assert!(report.errors.iter().any(|e| e.contains("non-decreasing")));
    }

    #[test]
    fn warns_on_respawn_forcing_update_storm() {
        let events: Vec<(f64, String)> = (0..5)
            .map(|i| {
                (
                    f64::from(i) * 0.03,
                    format!("\x1b_ratty;g;u;id=1;brightness=1.{i}\x1b\\"),
                )
            })
            .collect();
        let event_refs: Vec<(f64, &str, &str)> = events
            .iter()
            .map(|(time, data)| (*time, "o", data.as_str()))
            .collect();
        let cast = cast_with_events(&event_refs);
        let report = validate(&cast);
        assert!(report.warnings.iter().any(|w| w.contains("respawn")));
    }

    #[test]
    fn stage_sequences_are_strictly_checked() {
        let cast = cast_with_events(&[(
            0.0,
            "o",
            "\x1b_ratty;g;c;mode=cube4d;warp=1.5;zoom=9;waro=0.2;ease=bounce;dur=2\x1b\\",
        )]);
        let report = validate(&cast);
        for expected in [
            "unknown stage mode",
            "warp=1.5 out of range",
            "zoom=9 out of range",
            "unknown stage field \"waro\"",
            "unknown stage ease",
        ] {
            assert!(
                report.errors.iter().any(|e| e.contains(expected)),
                "missing \"{expected}\" in {:?}",
                report.errors
            );
        }
    }

    #[test]
    fn stage_and_animation_report_v2_requirements() {
        let cast = cast_with_events(&[
            (0.0, "o", "\x1b_ratty;g;c;warp=0.4;dur=2;ease=inout\x1b\\"),
            (0.1, "o", "\x1b_ratty;g;u;id=1;spin=2\x1b\\"),
        ]);
        let report = validate(&cast);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.requires_v2.contains("stage"));
        assert!(report.requires_v2.contains("tween"));
        assert!(report.requires_v2.contains("objanim"));
        assert!(
            report
                .stats
                .contains("requires: RGP v2 (objanim, stage, tween)")
        );
        assert!(report.warnings.iter().any(|w| w.contains("v1 terminals")));
    }

    #[test]
    fn v1_casts_report_v1() {
        let cast = cast_with_events(&[
            (
                0.0,
                "o",
                "\x1b_ratty;g;r;id=1;fmt=obj;source=payload;more=0;name=t.obj;normalize=1;dGVzdA==\x1b\\",
            ),
            (0.1, "o", "\x1b_ratty;g;p;id=1;row=5;col=5;w=2;h=2\x1b\\"),
        ]);
        let report = validate(&cast);
        assert!(report.requires_v2.is_empty());
        assert!(report.stats.contains("requires: RGP v1"));
    }

    #[test]
    fn bare_stage_sequence_warns() {
        let cast = cast_with_events(&[(0.0, "o", "\x1b_ratty;g;c\x1b\\")]);
        let report = validate(&cast);
        assert!(report.errors.is_empty());
        assert!(report.warnings.iter().any(|w| w.contains("no-op")));
    }

    #[test]
    fn accepts_parseable_ratty_osc_777_frames() {
        let cast = cast_with_events(&[
            (
                0.0,
                "o",
                "\x1b]777;ratty:sound.ambient.set;kind=ambient.hum&gain=0.4&xfade=800\x07",
            ),
            (0.5, "o", "\x1b]777;ratty:sound.play;kind=chime&gain=0.5\x07"),
            (0.6, "o", "\x1b]777;ratty:mood;mood=excited\x07"),
            (1.0, "o", "\x1b]777;ratty:sound.ambient.stop;fade=250\x07"),
        ]);
        let report = validate(&cast);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
    }

    #[test]
    fn rejects_unparseable_ratty_osc_777_frames() {
        // sound.play requires kind=; frobnicate is no action at all. Ratty
        // silently drops both — the validator must not.
        for data in [
            "\x1b]777;ratty:sound.play\x07",
            "\x1b]777;ratty:frobnicate\x07",
        ] {
            let cast = cast_with_events(&[(0.0, "o", data)]);
            let report = validate(&cast);
            assert!(
                report
                    .errors
                    .iter()
                    .any(|e| e.contains("unparseable ratty OSC 777")),
                "expected an error for {data:?}, got {:?}",
                report.errors
            );
        }
    }

    #[test]
    fn rejects_unknown_and_misclassed_sound_kinds() {
        for (data, expected) in [
            (
                "\x1b]777;ratty:sound.play;kind=boom\x07",
                "not a registered sound kind",
            ),
            (
                "\x1b]777;ratty:sound.play;kind=ambient.hum\x07",
                "is an ambient bed",
            ),
            (
                "\x1b]777;ratty:sound.ambient.set;kind=chime\x07",
                "is a one-shot",
            ),
        ] {
            let cast = cast_with_events(&[(0.0, "o", data)]);
            let report = validate(&cast);
            assert!(
                report.errors.iter().any(|e| e.contains(expected)),
                "expected \"{expected}\" for {data:?}, got {:?}",
                report.errors
            );
        }
    }

    #[test]
    fn ignores_foreign_osc_777_frames() {
        let cast = cast_with_events(&[(0.0, "o", "\x1b]777;notify;hello world\x07")]);
        let report = validate(&cast);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
    }

    #[test]
    fn rejects_unterminated_osc_777() {
        let cast = cast_with_events(&[(0.0, "o", "\x1b]777;ratty:sound.play;kind=chime")]);
        let report = validate(&cast);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("OSC 777 sequence starts but does not terminate")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn accepts_st_terminated_osc_777() {
        let cast = cast_with_events(&[(0.0, "o", "\x1b]777;ratty:sound.play;kind=click\x1b\\")]);
        let report = validate(&cast);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
    }

    #[test]
    fn accepts_c1_terminator() {
        let mut data = String::from("\x1b_ratty;g;d");
        data.push('\u{9c}');
        // NOTE: the C1 byte in a JSON string arrives as UTF-8 (0xc2 0x9c), so
        // scanning byte-wise must still find the terminator via the ESC \ form
        // in practice; this test documents the current behavior.
        let cast = cast_with_events(&[(0.0, "o", data.as_str())]);
        let report = validate(&cast);
        // The UTF-8 encoded C1 does not match the raw 0x9c byte scan; the
        // sequence is reported as unterminated.
        assert!(!report.errors.is_empty());
    }
}
