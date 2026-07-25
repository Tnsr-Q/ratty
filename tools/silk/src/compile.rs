//! Scene → cast compiler.
//!
//! Tracks per-object transform state so tweens interpolate from the object's
//! real current values, and emits *minimal* RGP `u` sequences (only the
//! tweened fields) — full-field updates would set `depth`/`color`/
//! `brightness` and force the renderer to respawn the object every frame.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use ratatui_ratty::{ObjectFormat, RattyGraphic, RattyGraphicSettings};

use crate::cast::{Cast, Event, Header, XRatty};
use crate::osc;
use crate::query;
use crate::scene::{
    AiArgs, CameraArgs, MacroArgs, PlaceArgs, PrintArgs, RegisterArgs, Scene, SoundArgs, Step,
    TWEENABLE_FIELDS, TweenArgs, UpdateArgs, VizArgs,
};
use crate::viz_wire;

/// Compilation stats for reporting.
pub struct Stats {
    /// Number of events in the cast.
    pub events: usize,
    /// Duration of the cast in seconds.
    pub duration_secs: f64,
    /// Serialized cast size in bytes.
    pub bytes: usize,
}

/// Compiles a scene file to a cast file.
pub fn compile_file(scene_path: &Path, output: &Path) -> Result<Stats> {
    let source = fs::read_to_string(scene_path)?;
    let scene: Scene = serde_json::from_str(&source).context("invalid scene JSON")?;
    let scene_dir = scene_path.parent().unwrap_or_else(|| Path::new("."));
    let cast = compile(&scene, scene_dir)?;
    let jsonl = cast.to_jsonl()?;
    fs::write(output, &jsonl)?;
    Ok(Stats {
        events: cast.events.len(),
        duration_secs: cast.duration_secs(),
        bytes: jsonl.len(),
    })
}

/// Compiles a parsed scene into a cast. Asset `file` paths resolve against
/// `scene_dir`.
pub fn compile(scene: &Scene, scene_dir: &Path) -> Result<Cast> {
    let header = Header {
        version: 2,
        width: scene.stage.cols,
        height: scene.stage.rows,
        title: Some(scene.meta.title.clone()),
        theme: scene.stage.theme.clone(),
        idle_time_limit: scene.stage.idle_time_limit,
        x_ratty: Some(XRatty {
            format: "silk/1".to_string(),
            agent: scene.meta.agent.clone(),
            mood: scene.meta.mood.clone(),
            mode: scene.stage.mode.clone(),
            warp: scene.stage.warp,
            view: scene.stage.view,
            loop_: scene.stage.loop_.then_some(true),
            site_name: scene.meta.site_name.clone(),
            checksum: None,
        }),
    };

    let mut steps: Vec<&Step> = scene.cast.iter().collect();
    steps.sort_by(|a, b| a.at.total_cmp(&b.at));

    let mut compiler = Compiler {
        scene_dir,
        events: Vec::new(),
        objects: BTreeMap::new(),
    };
    for (index, step) in steps.iter().enumerate() {
        compiler
            .step(step)
            .with_context(|| format!("cast step {} (at={})", index + 1, step.at))?;
    }
    // Events within one step are emitted in order; steps are sorted by time,
    // so the whole stream is monotonic. Sort stably anyway as a guarantee.
    compiler.events.sort_by(|a, b| a.time.total_cmp(&b.time));

    Ok(Cast {
        header,
        events: compiler.events,
    })
}

/// Live transform state tracked per object for tween interpolation.
///
/// The v2 animation rates are `Option` because their wire-level defaults
/// are the terminal's *config* values, which the compiler cannot know —
/// tweening one before it has been set explicitly is an authoring error.
#[derive(Clone, Copy)]
struct ObjectState {
    px: f64,
    py: f64,
    pz: f64,
    rx: f64,
    ry: f64,
    rz: f64,
    sx: f64,
    sy: f64,
    sz: f64,
    scale: f64,
    spin: Option<f64>,
    bob: Option<f64>,
    bobamp: Option<f64>,
    phase: Option<f64>,
}

impl Default for ObjectState {
    fn default() -> Self {
        Self {
            px: 0.0,
            py: 0.0,
            pz: 0.0,
            rx: 0.0,
            ry: 0.0,
            rz: 0.0,
            sx: 1.0,
            sy: 1.0,
            sz: 1.0,
            scale: 1.0,
            spin: None,
            bob: None,
            bobamp: None,
            phase: None,
        }
    }
}

impl ObjectState {
    fn get(&self, field: &str) -> Option<f64> {
        match field {
            "px" => Some(self.px),
            "py" => Some(self.py),
            "pz" => Some(self.pz),
            "rx" => Some(self.rx),
            "ry" => Some(self.ry),
            "rz" => Some(self.rz),
            "sx" => Some(self.sx),
            "sy" => Some(self.sy),
            "sz" => Some(self.sz),
            "scale" => Some(self.scale),
            "spin" => self.spin,
            "bob" => self.bob,
            "bobamp" => self.bobamp,
            "phase" => self.phase,
            _ => None,
        }
    }

    fn set(&mut self, field: &str, value: f64) {
        match field {
            "px" => self.px = value,
            "py" => self.py = value,
            "pz" => self.pz = value,
            "rx" => self.rx = value,
            "ry" => self.ry = value,
            "rz" => self.rz = value,
            "sx" => self.sx = value,
            "sy" => self.sy = value,
            "sz" => self.sz = value,
            "scale" => self.scale = value,
            "spin" => self.spin = Some(value),
            "bob" => self.bob = Some(value),
            "bobamp" => self.bobamp = Some(value),
            "phase" => self.phase = Some(value),
            _ => {}
        }
    }
}

struct Compiler<'a> {
    scene_dir: &'a Path,
    events: Vec<Event>,
    objects: BTreeMap<u32, ObjectState>,
}

impl Compiler<'_> {
    fn out(&mut self, time: f64, data: String) {
        self.events.push(Event {
            time,
            code: "o".to_string(),
            data,
        });
    }

    fn step(&mut self, step: &Step) -> Result<()> {
        if step.verb_count() != 1 {
            bail!(
                "each step must have exactly one verb (print/register/place/\
                 update/tween/camera/ai/sound/viz/macro/delete/marker/clear)"
            );
        }
        if let Some(print) = &step.print {
            self.print(step.at, print)?;
        } else if let Some(register) = &step.register {
            self.register(step.at, register)?;
        } else if let Some(place) = &step.place {
            self.place(step.at, place)?;
        } else if let Some(update) = &step.update {
            self.update(step.at, update)?;
        } else if let Some(tween) = &step.tween {
            self.tween(step.at, tween)?;
        } else if let Some(camera) = &step.camera {
            self.camera(step.at, camera)?;
        } else if let Some(ai) = &step.ai {
            self.ai(step.at, ai)?;
        } else if let Some(sound) = &step.sound {
            self.sound(step.at, sound)?;
        } else if let Some(viz) = &step.viz {
            self.viz(step.at, viz)?;
        } else if let Some(macro_) = &step.macro_ {
            self.macro_block(step.at, macro_)?;
        } else if let Some(delete) = &step.delete {
            match delete.id()? {
                Some(id) => {
                    self.objects.remove(&id);
                    self.out(step.at, format!("\x1b_ratty;g;d;id={id}\x1b\\"));
                }
                None => {
                    self.objects.clear();
                    self.out(step.at, "\x1b_ratty;g;d\x1b\\".to_string());
                }
            }
        } else if let Some(marker) = &step.marker {
            self.events.push(Event {
                time: step.at,
                code: "m".to_string(),
                data: marker.clone(),
            });
        } else if step.clear.is_some() {
            self.out(step.at, "\x1b[2J\x1b[H".to_string());
        }
        Ok(())
    }

    fn print(&mut self, at: f64, print: &PrintArgs) -> Result<()> {
        let mut data = format!("\x1b[{};{}H", print.row + 1, print.col + 1);
        if print.bold {
            data.push_str("\x1b[1m");
        }
        if let Some(fg) = &print.fg {
            let [r, g, b] = parse_hex_color(fg)?;
            data.push_str(&format!("\x1b[38;2;{r};{g};{b}m"));
        }
        if let Some(bg) = &print.bg {
            let [r, g, b] = parse_hex_color(bg)?;
            data.push_str(&format!("\x1b[48;2;{r};{g};{b}m"));
        }
        data.push_str(&print.text);
        if print.el {
            // Erase to end of line while the SGR state (background) is
            // still active, so the erased cells take the print's colors.
            data.push_str("\x1b[K");
        }
        data.push_str("\x1b[0m");
        self.out(at, data);
        Ok(())
    }

    fn register(&mut self, at: f64, register: &RegisterArgs) -> Result<()> {
        let format = match register.fmt.as_deref() {
            Some("obj") => Some(ObjectFormat::Obj),
            Some("glb") => Some(ObjectFormat::Glb),
            Some("stl") => Some(ObjectFormat::Stl),
            Some(other) => bail!("unsupported fmt \"{other}\" (obj, glb, stl)"),
            None => None,
        };

        match (&register.file, &register.path) {
            (Some(file), None) => {
                let resolved = self.scene_dir.join(file);
                let bytes = fs::read(&resolved)
                    .with_context(|| format!("failed to read asset {}", resolved.display()))?;
                let file_name = file
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| anyhow!("asset file has no name"))?;
                let mut settings = RattyGraphicSettings::new(file_name.to_string()).id(register.id);
                if let Some(format) = format {
                    settings = settings.format(format);
                }
                if let Some(normalize) = register.normalize {
                    settings = settings.normalize(normalize);
                }
                let graphic = RattyGraphic::new(settings);
                let name = register.name.as_deref().or(Some(file_name));
                for sequence in graphic.register_payload_sequences_with_name(&bytes, name) {
                    self.out(at, sequence);
                }
            }
            (None, Some(path)) => {
                let mut settings = RattyGraphicSettings::new(path.clone()).id(register.id);
                if let Some(format) = format {
                    settings = settings.format(format);
                }
                if let Some(normalize) = register.normalize {
                    settings = settings.normalize(normalize);
                }
                self.out(at, RattyGraphic::new(settings).register_sequence());
            }
            (Some(_), Some(_)) => bail!("register takes either file or path, not both"),
            (None, None) => bail!("register needs file (embedded payload) or path (ratty asset)"),
        }
        Ok(())
    }

    fn place(&mut self, at: f64, place: &PlaceArgs) -> Result<()> {
        let mut fields = vec![
            format!("id={}", place.id),
            format!("row={}", place.row),
            format!("col={}", place.col),
            format!("w={}", place.w),
            format!("h={}", place.h),
        ];
        let mut state = ObjectState::default();
        if let Some(animate) = place.animate {
            fields.push(format!("animate={}", u8::from(animate)));
        }
        if let Some(scale) = place.scale {
            fields.push(format!("scale={}", fmt_f32(scale)));
            state.scale = f64::from(scale);
        }
        if let Some(depth) = place.depth {
            fields.push(format!("depth={}", fmt_f32(depth)));
        }
        if let Some(color) = &place.color {
            let [r, g, b] = parse_hex_color(color)?;
            fields.push(format!("color={r:02x}{g:02x}{b:02x}"));
        }
        if let Some(brightness) = place.brightness {
            fields.push(format!("brightness={}", fmt_f32(brightness)));
        }
        for (key, value) in [
            ("px", place.px),
            ("py", place.py),
            ("pz", place.pz),
            ("rx", place.rx),
            ("ry", place.ry),
            ("rz", place.rz),
            ("sx", place.sx),
            ("sy", place.sy),
            ("sz", place.sz),
            ("spin", place.spin),
            ("bob", place.bob),
            ("bobamp", place.bobamp),
            ("phase", place.phase),
        ] {
            if let Some(value) = value {
                fields.push(format!("{key}={}", fmt_f32(value)));
                state.set(key, f64::from(value));
            }
        }
        self.out(at, format!("\x1b_ratty;g;p;{}\x1b\\", fields.join(";")));
        self.objects.insert(place.id, state);
        Ok(())
    }

    fn update(&mut self, at: f64, update: &UpdateArgs) -> Result<()> {
        let mut fields = vec![format!("id={}", update.id)];
        if let Some(animate) = update.animate {
            fields.push(format!("animate={}", u8::from(animate)));
        }
        if let Some(depth) = update.depth {
            fields.push(format!("depth={}", fmt_f32(depth)));
        }
        if let Some(color) = &update.color {
            let [r, g, b] = parse_hex_color(color)?;
            fields.push(format!("color={r:02x}{g:02x}{b:02x}"));
        }
        if let Some(brightness) = update.brightness {
            fields.push(format!("brightness={}", fmt_f32(brightness)));
        }
        let state = self.objects.entry(update.id).or_default();
        for (key, value) in [
            ("scale", update.scale),
            ("px", update.px),
            ("py", update.py),
            ("pz", update.pz),
            ("rx", update.rx),
            ("ry", update.ry),
            ("rz", update.rz),
            ("sx", update.sx),
            ("sy", update.sy),
            ("sz", update.sz),
            ("spin", update.spin),
            ("bob", update.bob),
            ("bobamp", update.bobamp),
            ("phase", update.phase),
        ] {
            if let Some(value) = value {
                fields.push(format!("{key}={}", fmt_f32(value)));
                state.set(key, f64::from(value));
            }
        }
        if fields.len() == 1 {
            bail!("update for id={} sets no fields", update.id);
        }
        self.out(at, format!("\x1b_ratty;g;u;{}\x1b\\", fields.join(";")));
        Ok(())
    }

    fn tween(&mut self, at: f64, tween: &TweenArgs) -> Result<()> {
        if tween.to.is_empty() {
            bail!("tween for id={} has an empty \"to\"", tween.id);
        }
        for field in tween.to.keys() {
            if !TWEENABLE_FIELDS.contains(&field.as_str()) {
                let hint = if matches!(field.as_str(), "depth" | "color" | "brightness") {
                    " (forces a renderer respawn every frame; set it once via update instead)"
                } else {
                    ""
                };
                bail!("tween field \"{field}\" is not tweenable{hint}");
            }
        }
        if tween.dur <= 0.0 {
            bail!("tween dur must be positive");
        }
        let fps = tween.fps.unwrap_or(30.0);
        if fps <= 0.0 {
            bail!("tween fps must be positive");
        }
        let ease = parse_ease(tween.ease.as_deref())?;

        let from = *self.objects.entry(tween.id).or_default();
        let mut starts = BTreeMap::new();
        for field in tween.to.keys() {
            let Some(start) = from.get(field) else {
                bail!(
                    "tween field \"{field}\" has no current value for id={} — set it \
                     explicitly via place/update first (its wire default is the \
                     terminal's config, which the compiler cannot know)",
                    tween.id
                );
            };
            starts.insert(field.clone(), start);
        }
        let steps = ((tween.dur * fps).ceil() as usize).max(1);
        for step in 1..=steps {
            let progress = step as f64 / steps as f64;
            let eased = ease.apply(progress);
            let mut fields = vec![format!("id={}", tween.id)];
            for (field, target) in &tween.to {
                let start = starts[field];
                let value = start + (target - start) * eased;
                fields.push(format!("{field}={}", fmt_f64(value)));
            }
            self.out(
                at + tween.dur * progress,
                format!("\x1b_ratty;g;u;{}\x1b\\", fields.join(";")),
            );
        }

        let state = self.objects.entry(tween.id).or_default();
        for (field, target) in &tween.to {
            state.set(field, *target);
        }
        Ok(())
    }

    /// Emits exactly one RGP v2 `c` sequence. Camera tweens are engine-side
    /// (`dur`/`ease` ride along in the sequence): the terminal interpolates
    /// at frame rate, which is smoother and hundreds of times smaller than
    /// per-frame `c` spam — which would also self-cancel, since each `c`
    /// replaces the previous stage tween.
    fn camera(&mut self, at: f64, camera: &CameraArgs) -> Result<()> {
        let mut fields = Vec::new();
        if let Some(mode) = &camera.mode {
            if !matches!(mode.as_str(), "flat2d" | "plane3d" | "mobius3d") {
                bail!("unknown camera mode \"{mode}\" (flat2d, plane3d, mobius3d)");
            }
            fields.push(format!("mode={mode}"));
        }
        if let Some(warp) = camera.warp {
            if !(0.0..=1.0).contains(&warp) {
                bail!("camera warp {warp} out of range 0.0..=1.0");
            }
            fields.push(format!("warp={}", fmt_f32(warp)));
        }
        if let Some(yaw) = camera.yaw {
            fields.push(format!("yaw={}", fmt_f32(yaw)));
        }
        if let Some(pitch) = camera.pitch {
            fields.push(format!("pitch={}", fmt_f32(pitch)));
        }
        if let Some(zoom) = camera.zoom {
            if !(0.1..=4.0).contains(&zoom) {
                bail!("camera zoom {zoom} out of range 0.1..=4.0");
            }
            fields.push(format!("zoom={}", fmt_f32(zoom)));
        }
        if fields.is_empty() {
            bail!("camera step sets no fields");
        }
        let tweening = match camera.dur {
            Some(dur) if dur < 0.0 => bail!("camera dur must not be negative"),
            Some(dur) if dur > 0.0 => {
                // `dur` only tweens the stage fields; a lone mode change
                // has nothing to tween.
                if camera.mode.is_some() && fields.len() == 1 {
                    bail!("camera dur has no effect on a mode-only step (mode is always instant)");
                }
                fields.push(format!("dur={}", fmt_f64(dur)));
                true
            }
            _ => false,
        };
        if let Some(ease) = &camera.ease {
            if !tweening {
                bail!("camera ease requires a positive dur");
            }
            let wire = match ease.as_str() {
                "linear" => "linear",
                "in" => "in",
                "out" => "out",
                "in-out" => "inout",
                other => bail!("unknown camera ease \"{other}\" (linear, in, out, in-out)"),
            };
            fields.push(format!("ease={wire}"));
        }
        self.out(at, format!("\x1b_ratty;g;c;{}\x1b\\", fields.join(";")));
        Ok(())
    }

    /// Emits one OSC 777 sequence per present AI field, at the step's time,
    /// using ratty's own encoder so the bytes match what the terminal parses.
    fn ai(&mut self, at: f64, ai: &AiArgs) -> Result<()> {
        let mut emitted = false;
        let mut emit = |compiler: &mut Self, action: &str, payload: String| {
            compiler.out(at, osc::osc_sequence(action, &payload));
            emitted = true;
        };
        let enc = osc::percent_encode;

        if let Some(mood) = &ai.mood {
            if osc::parse_command(&format!("ratty:mood;mood={mood}")).is_none() {
                bail!("ai.mood produced an unparseable command for \"{mood}\"");
            }
            emit(self, "mood", format!("mood={}", enc(mood)));
        }
        if let Some(think) = &ai.think {
            if !matches!(think.as_str(), "start" | "end" | "toggle") {
                bail!("ai.think must be start, end, or toggle (got \"{think}\")");
            }
            emit(self, "think", format!("state={}", enc(think)));
        }
        if let Some(level) = ai.confidence {
            if !(0.0..=1.0).contains(&level) {
                bail!("ai.confidence {level} out of range 0.0..=1.0");
            }
            emit(self, "confidence", format!("level={}", fmt_f32(level)));
        }
        if let Some(color) = &ai.flash {
            parse_hex_color(color)?;
            let duration = ai.flash_duration.unwrap_or(0.5);
            emit(
                self,
                "flash",
                format!("color={}&duration={}", enc(color), fmt_f32(duration)),
            );
        }
        if let Some(intensity) = ai.pulse {
            let duration = ai.pulse_duration.unwrap_or(1.0);
            emit(
                self,
                "pulse",
                format!(
                    "intensity={}&duration={}",
                    fmt_f32(intensity),
                    fmt_f32(duration)
                ),
            );
        }
        if let Some(color) = &ai.tint {
            parse_hex_color(color)?;
            let opacity = ai.tint_opacity.unwrap_or(0.1);
            emit(
                self,
                "tint",
                format!("color={}&opacity={}", enc(color), fmt_f32(opacity)),
            );
        }
        if ai.reset.unwrap_or(false) {
            emit(self, "reset", String::new());
        }
        if !emitted {
            bail!("ai step sets no fields");
        }
        Ok(())
    }

    /// Emits exactly one OSC 777 `sound.*` sequence, validating the kind
    /// against the registry ratty itself compiles (`osc::SOUND_KINDS`) so
    /// unknown kinds are hard compile errors. Never emits `tok=`: a cast has
    /// no return channel, so an ack request would be meaningless.
    fn sound(&mut self, at: f64, sound: &SoundArgs) -> Result<()> {
        let verbs = usize::from(sound.play.is_some())
            + usize::from(sound.ambient.is_some())
            + usize::from(sound.stop.is_some());
        if verbs != 1 {
            bail!("sound step must set exactly one of play, ambient, or stop");
        }
        if let Some(gain) = sound.gain
            && !(0.0..=1.0).contains(&gain)
        {
            bail!("sound.gain {gain} out of range 0.0..=1.0");
        }
        let xfade_ms = sound
            .xfade
            .map(|xfade| -> Result<u32> {
                if !xfade.is_finite() || xfade < 0.0 {
                    bail!("sound.xfade {xfade} must be a non-negative number of seconds");
                }
                let ms = (xfade * 1000.0).round();
                if ms > f64::from(u32::MAX) {
                    bail!("sound.xfade {xfade}s overflows the wire's millisecond field");
                }
                Ok(ms as u32)
            })
            .transpose()?;
        let enc = osc::percent_encode;

        if let Some(kind) = &sound.play {
            match osc::sound_kind_class(kind) {
                Some(osc::SoundKindClass::OneShot) => {}
                Some(osc::SoundKindClass::Ambient) => bail!(
                    "sound.play kind \"{kind}\" is an ambient bed; use sound.ambient \
                     (one-shots: {})",
                    sound_kind_list(osc::SoundKindClass::OneShot),
                ),
                None => bail!(
                    "unknown sound kind \"{kind}\" (one-shots: {})",
                    sound_kind_list(osc::SoundKindClass::OneShot),
                ),
            }
            if xfade_ms.is_some() {
                bail!("sound.xfade has no effect on a one-shot play");
            }
            let mut payload = format!("kind={}", enc(kind));
            if let Some(gain) = sound.gain {
                payload.push_str(&format!("&gain={}", fmt_f32(gain)));
            }
            self.emit_sound(at, "sound.play", &payload)?;
        } else if let Some(kind) = &sound.ambient {
            match osc::sound_kind_class(kind) {
                Some(osc::SoundKindClass::Ambient) => {}
                Some(osc::SoundKindClass::OneShot) => bail!(
                    "sound.ambient kind \"{kind}\" is a one-shot; use sound.play \
                     (ambient beds: {})",
                    sound_kind_list(osc::SoundKindClass::Ambient),
                ),
                None => bail!(
                    "unknown sound kind \"{kind}\" (ambient beds: {})",
                    sound_kind_list(osc::SoundKindClass::Ambient),
                ),
            }
            let mut payload = format!("kind={}", enc(kind));
            if let Some(gain) = sound.gain {
                payload.push_str(&format!("&gain={}", fmt_f32(gain)));
            }
            if let Some(ms) = xfade_ms {
                payload.push_str(&format!("&xfade={ms}"));
            }
            self.emit_sound(at, "sound.ambient.set", &payload)?;
        } else {
            if sound.stop != Some(true) {
                bail!("sound.stop must be true (omit it otherwise)");
            }
            if sound.gain.is_some() {
                bail!("sound.gain has no effect on stop");
            }
            let payload = xfade_ms.map(|ms| format!("fade={ms}")).unwrap_or_default();
            self.emit_sound(at, "sound.ambient.stop", &payload)?;
        }
        Ok(())
    }

    /// Round-trips a sound command through ratty's own parser before
    /// emitting it — the compiler can never emit what the terminal cannot
    /// decode (the CLI-and-silk-share-osc.rs contract).
    fn emit_sound(&mut self, at: f64, action: &str, payload: &str) -> Result<()> {
        let command = if payload.is_empty() {
            format!("ratty:{action}")
        } else {
            format!("ratty:{action};{payload}")
        };
        if osc::parse_command(&command).is_none() {
            bail!("sound step produced an unparseable command {command:?}");
        }
        self.out(at, osc::osc_sequence(action, payload));
        Ok(())
    }

    /// Emits exactly one `viz.set` sequence. The inline data is validated
    /// with the terminal's own decoder (`viz_wire::decode_viz_payload`)
    /// against the exact bytes that ride the wire, so a compiled cast can
    /// never carry a viz payload the terminal rejects. A missing
    /// `capture` is stamped `authored` — deterministically, keeping
    /// compiled casts byte-reproducible — and one the author supplied is
    /// never overwritten.
    fn viz(&mut self, at: f64, viz: &VizArgs) -> Result<()> {
        let Some(id) = viz.id else {
            bail!("viz.id is required (a caller-owned id in the AI range)");
        };
        if osc::ai_object_namespace(id).is_none() {
            bail!(
                "viz.id {id:#010x} is below the AI-owned range \
                 ({:#010x}..); transmissions own ids there",
                osc::AI_OBJECT_ID_MIN
            );
        }
        let Some(kind) = &viz.kind else {
            bail!(
                "viz.kind is required (registered: {:?})",
                viz_wire::REGISTERED_VIZ_KINDS
            );
        };
        let Some(data) = &viz.data else {
            bail!("viz.data is required (the kind's schema-conforming JSON, inline)");
        };
        if viz.x.is_some() != viz.y.is_some() {
            bail!("viz.x and viz.y place together; got one without the other");
        }
        if (viz.cols.is_some() || viz.rows.is_some()) && viz.x.is_none() {
            bail!("viz.cols/viz.rows need an anchor (supply viz.x and viz.y)");
        }
        if viz.cols == Some(0) || viz.rows == Some(0) {
            bail!("viz.cols and viz.rows must be at least 1");
        }
        let mut data = data.clone();
        if let Some(object) = data.as_object_mut()
            && !object.contains_key("capture")
        {
            object.insert(
                "capture".to_string(),
                serde_json::json!({ "source": "authored", "ts": "authored" }),
            );
        }
        let bytes = serde_json::to_vec(&data).context("encoding viz.data")?;
        if bytes.len() > osc::MAX_VIZ_PAYLOAD_BYTES {
            bail!(
                "viz.data is {} bytes; the wire caps decoded payloads at {}",
                bytes.len(),
                osc::MAX_VIZ_PAYLOAD_BYTES
            );
        }
        let encoded = query::b64url_encode(&bytes);
        if let Err(error) = viz_wire::decode_viz_payload(kind, &encoded) {
            bail!(
                "viz step rejected by ratty's decoder ({}): {}",
                error.code,
                error.message
            );
        }
        // base64url never needs escaping; the other values are numeric.
        let mut payload = format!("id={id}&kind={}&data={encoded}", osc::percent_encode(kind));
        if let (Some(x), Some(y)) = (viz.x, viz.y) {
            payload.push_str(&format!("&x={x}&y={y}"));
        }
        if let Some(cols) = viz.cols {
            payload.push_str(&format!("&cols={cols}"));
        }
        if let Some(rows) = viz.rows {
            payload.push_str(&format!("&rows={rows}"));
        }
        if viz.replace == Some(true) {
            payload.push_str("&replace=true");
        }
        let command = format!("ratty:viz.set;{payload}");
        if osc::parse_command(&command).is_none() {
            bail!("viz step produced an unparseable command {command:?}");
        }
        self.out(at, osc::osc_sequence("viz.set", &payload));
        Ok(())
    }

    /// Emits the `macro.record … macro.stop` bracket around the enclosed
    /// choreography — pure sugar over the same wire. The enclosed steps
    /// compile to their ordinary sequences and, played between the bracket,
    /// are recorded by the terminal exactly once. Forbids a nested `macro`
    /// block (no recursion) and a `reset` inside the block (it would cancel
    /// the very recording), and keeps the bracket monotonic: `record` at the
    /// block time, `stop` after the last enclosed event.
    fn macro_block(&mut self, at: f64, block: &MacroArgs) -> Result<()> {
        if block.name.is_empty() {
            bail!("macro.name is required (non-empty)");
        }
        for step in &block.cast {
            if step.verb_count() != 1 {
                bail!("each macro step must have exactly one verb");
            }
            if step.macro_.is_some() {
                bail!("a macro block may not nest another macro block (no recursion)");
            }
            if step.ai.as_ref().is_some_and(|ai| ai.reset.unwrap_or(false)) {
                bail!("a reset inside a macro block would cancel the recording");
            }
            if step.at < at {
                bail!(
                    "macro step at={} precedes the block's record at={at} \
                     (enclosed steps must play inside the bracket)",
                    step.at
                );
            }
        }

        let mut record_payload = format!("name={}", osc::percent_encode(&block.name));
        if block.replace {
            record_payload.push_str("&mode=replace");
        }
        // Validate the bracket parses as the terminal would read it.
        if osc::parse_command(&format!("ratty:macro.record;{record_payload}")).is_none() {
            bail!(
                "macro.record for \"{}\" produced an unparseable command",
                block.name
            );
        }
        self.out(at, osc::osc_sequence("macro.record", &record_payload));

        // Compile the enclosed choreography in time order so the whole
        // stream stays monotonic; the terminal captures it as it plays.
        let mut enclosed: Vec<&Step> = block.cast.iter().collect();
        enclosed.sort_by(|a, b| a.at.total_cmp(&b.at));
        let mut stop_at = at;
        for step in enclosed {
            self.step(step)?;
            stop_at = stop_at.max(step.at);
        }
        self.out(stop_at, osc::osc_sequence("macro.stop", ""));
        Ok(())
    }
}

/// Comma-separated registered sound kinds of one class, for error messages.
fn sound_kind_list(class: osc::SoundKindClass) -> String {
    osc::SOUND_KINDS
        .iter()
        .filter(|(_, kind_class)| *kind_class == class)
        .map(|(kind, _)| *kind)
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_ease(name: Option<&str>) -> Result<Ease> {
    match name {
        None | Some("linear") => Ok(Ease::Linear),
        Some("in") => Ok(Ease::In),
        Some("out") => Ok(Ease::Out),
        Some("in-out") => Ok(Ease::InOut),
        Some(other) => bail!("unknown ease \"{other}\" (linear, in, out, in-out)"),
    }
}

enum Ease {
    Linear,
    In,
    Out,
    InOut,
}

impl Ease {
    fn apply(&self, t: f64) -> f64 {
        match self {
            Self::Linear => t,
            Self::In => t * t,
            Self::Out => 1.0 - (1.0 - t) * (1.0 - t),
            Self::InOut => t * t * (3.0 - 2.0 * t),
        }
    }
}

/// Parses `#rrggbb` (or `rrggbb`) into RGB bytes.
pub fn parse_hex_color(value: &str) -> Result<[u8; 3]> {
    let hex = value.strip_prefix('#').unwrap_or(value);
    if hex.len() != 6 {
        bail!("color must be 6 hex digits, got \"{value}\"");
    }
    Ok([
        u8::from_str_radix(&hex[0..2], 16).with_context(|| format!("bad color \"{value}\""))?,
        u8::from_str_radix(&hex[2..4], 16).with_context(|| format!("bad color \"{value}\""))?,
        u8::from_str_radix(&hex[4..6], 16).with_context(|| format!("bad color \"{value}\""))?,
    ])
}

fn fmt_f32(value: f32) -> String {
    fmt_f64(f64::from(value))
}

/// Formats a float compactly (4 decimal places, trailing zeros trimmed) to
/// keep per-frame update sequences small.
fn fmt_f64(value: f64) -> String {
    let mut formatted = format!("{value:.4}");
    if formatted.contains('.') {
        while formatted.ends_with('0') {
            formatted.pop();
        }
        if formatted.ends_with('.') {
            formatted.pop();
        }
    }
    if formatted == "-0" {
        formatted = "0".to_string();
    }
    formatted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_floats_compactly() {
        assert_eq!(fmt_f64(0.0), "0");
        assert_eq!(fmt_f64(1.5), "1.5");
        assert_eq!(fmt_f64(180.0), "180");
        assert_eq!(fmt_f64(0.30000000000000004), "0.3");
        assert_eq!(fmt_f64(-0.00001), "0");
    }

    #[test]
    fn tween_rejects_respawn_forcing_fields() {
        let scene: Scene = serde_json::from_str(
            r#"{
                "meta": {"title": "t"},
                "cast": [
                    {"at": 0.0, "place": {"id": 1, "row": 5, "col": 5, "w": 2, "h": 2}},
                    {"at": 1.0, "tween": {"id": 1, "dur": 1.0, "to": {"depth": 3.0}}}
                ]
            }"#,
        )
        .unwrap();
        let error = compile(&scene, Path::new(".")).unwrap_err();
        assert!(format!("{error:#}").contains("respawn"));
    }

    #[test]
    fn tween_interpolates_from_placed_state() {
        let scene: Scene = serde_json::from_str(
            r#"{
                "meta": {"title": "t"},
                "cast": [
                    {"at": 0.0, "place": {"id": 1, "row": 5, "col": 5, "w": 2, "h": 2, "ry": 90}},
                    {"at": 1.0, "tween": {"id": 1, "dur": 1.0, "fps": 2, "to": {"ry": 180}}}
                ]
            }"#,
        )
        .unwrap();
        let cast = compile(&scene, Path::new(".")).unwrap();
        let updates: Vec<&str> = cast
            .events
            .iter()
            .filter(|event| event.data.contains(";u;"))
            .map(|event| event.data.as_str())
            .collect();
        assert_eq!(
            updates,
            [
                "\x1b_ratty;g;u;id=1;ry=135\x1b\\",
                "\x1b_ratty;g;u;id=1;ry=180\x1b\\"
            ]
        );
    }

    #[test]
    fn camera_step_emits_exactly_one_c_sequence() {
        let scene: Scene = serde_json::from_str(
            r#"{
                "meta": {"title": "t"},
                "cast": [
                    {"at": 0.0, "camera": {"mode": "plane3d", "warp": 0.4, "pitch": 0.12,
                                            "dur": 2.0, "ease": "in-out"}}
                ]
            }"#,
        )
        .unwrap();
        let cast = compile(&scene, Path::new(".")).unwrap();
        assert_eq!(cast.events.len(), 1);
        assert_eq!(
            cast.events[0].data,
            "\x1b_ratty;g;c;mode=plane3d;warp=0.4;pitch=0.12;dur=2;ease=inout\x1b\\"
        );
    }

    #[test]
    fn camera_rejects_ease_without_dur_and_bad_ranges() {
        for (scene_json, expected) in [
            (
                r#"{"meta": {"title": "t"}, "cast": [
                    {"at": 0.0, "camera": {"warp": 0.4, "ease": "linear"}}]}"#,
                "requires a positive dur",
            ),
            (
                r#"{"meta": {"title": "t"}, "cast": [
                    {"at": 0.0, "camera": {"warp": 1.5}}]}"#,
                "out of range",
            ),
            (
                r#"{"meta": {"title": "t"}, "cast": [
                    {"at": 0.0, "camera": {"zoom": 9.0}}]}"#,
                "out of range",
            ),
            (
                r#"{"meta": {"title": "t"}, "cast": [
                    {"at": 0.0, "camera": {"mode": "cube4d"}}]}"#,
                "unknown camera mode",
            ),
            (
                r#"{"meta": {"title": "t"}, "cast": [
                    {"at": 0.0, "camera": {"mode": "plane3d", "dur": 2.0}}]}"#,
                "mode is always instant",
            ),
        ] {
            let scene: Scene = serde_json::from_str(scene_json).unwrap();
            let error = compile(&scene, Path::new(".")).unwrap_err();
            assert!(
                format!("{error:#}").contains(expected),
                "expected \"{expected}\" in: {error:#}"
            );
        }
    }

    #[test]
    fn tween_over_unset_animation_field_errors() {
        let scene: Scene = serde_json::from_str(
            r#"{
                "meta": {"title": "t"},
                "cast": [
                    {"at": 0.0, "place": {"id": 1, "row": 5, "col": 5, "w": 2, "h": 2}},
                    {"at": 1.0, "tween": {"id": 1, "dur": 1.0, "to": {"spin": 3.0}}}
                ]
            }"#,
        )
        .unwrap();
        let error = compile(&scene, Path::new(".")).unwrap_err();
        assert!(format!("{error:#}").contains("set it explicitly via place/update first"));
    }

    #[test]
    fn tween_over_set_animation_field_interpolates() {
        let scene: Scene = serde_json::from_str(
            r#"{
                "meta": {"title": "t"},
                "cast": [
                    {"at": 0.0, "place": {"id": 1, "row": 5, "col": 5, "w": 2, "h": 2,
                                           "animate": true, "spin": 1.0}},
                    {"at": 1.0, "tween": {"id": 1, "dur": 1.0, "fps": 2, "to": {"spin": 3.0}}}
                ]
            }"#,
        )
        .unwrap();
        let cast = compile(&scene, Path::new(".")).unwrap();
        let updates: Vec<&str> = cast
            .events
            .iter()
            .filter(|event| event.data.contains(";u;"))
            .map(|event| event.data.as_str())
            .collect();
        assert_eq!(
            updates,
            [
                "\x1b_ratty;g;u;id=1;spin=2\x1b\\",
                "\x1b_ratty;g;u;id=1;spin=3\x1b\\"
            ]
        );
    }

    #[test]
    fn ai_step_emits_parseable_osc_777() {
        let scene: Scene = serde_json::from_str(
            r#"{
                "meta": {"title": "t"},
                "cast": [
                    {"at": 0.0, "ai": {"mood": "excited", "confidence": 0.9}},
                    {"at": 1.0, "ai": {"flash": "8a9a7b", "flash_duration": 0.4}}
                ]
            }"#,
        )
        .unwrap();
        let cast = compile(&scene, Path::new(".")).unwrap();
        // Every emitted OSC sequence must parse back through the terminal's
        // own decoder — the CLI-and-silk-share-osc.rs contract.
        let commands: Vec<super::osc::RattyAiCommand> = cast
            .events
            .iter()
            .filter(|event| event.data.contains("]777;ratty:"))
            .map(|event| {
                let inner = event
                    .data
                    .strip_prefix("\x1b]777;")
                    .and_then(|s| s.strip_suffix('\x07'))
                    .expect("well-framed osc");
                super::osc::parse_command(inner).expect("terminal parses silk's osc")
            })
            .collect();
        assert_eq!(
            commands,
            vec![
                super::osc::RattyAiCommand::Mood {
                    mood: "excited".into()
                },
                super::osc::RattyAiCommand::Confidence { level: 0.9 },
                super::osc::RattyAiCommand::Flash {
                    color: "8a9a7b".into(),
                    duration: 0.4,
                },
            ]
        );
    }

    #[test]
    fn ai_step_rejects_bad_values() {
        for (json, expected) in [
            (
                r#"{"meta":{"title":"t"},"cast":[{"at":0.0,"ai":{"confidence":1.5}}]}"#,
                "out of range",
            ),
            (
                r#"{"meta":{"title":"t"},"cast":[{"at":0.0,"ai":{"think":"maybe"}}]}"#,
                "start, end, or toggle",
            ),
            (
                r#"{"meta":{"title":"t"},"cast":[{"at":0.0,"ai":{"flash":"notacolor"}}]}"#,
                "color",
            ),
            (
                r#"{"meta":{"title":"t"},"cast":[{"at":0.0,"ai":{}}]}"#,
                "sets no fields",
            ),
        ] {
            let scene: Scene = serde_json::from_str(json).unwrap();
            let error = compile(&scene, Path::new(".")).unwrap_err();
            assert!(
                format!("{error:#}").contains(expected),
                "expected \"{expected}\" in: {error:#}"
            );
        }
    }

    #[test]
    fn sound_step_emits_parseable_osc_777() {
        let scene: Scene = serde_json::from_str(
            r#"{
                "meta": {"title": "t"},
                "cast": [
                    {"at": 0.0, "sound": {"ambient": "ambient.hum", "gain": 0.4, "xfade": 0.8}},
                    {"at": 1.0, "sound": {"play": "chime"}},
                    {"at": 2.0, "sound": {"play": "click", "gain": 0.5}},
                    {"at": 3.0, "sound": {"stop": true, "xfade": 0.25}},
                    {"at": 4.0, "sound": {"stop": true}}
                ]
            }"#,
        )
        .unwrap();
        let cast = compile(&scene, Path::new(".")).unwrap();
        let datas: Vec<&str> = cast
            .events
            .iter()
            .map(|event| event.data.as_str())
            .collect();
        assert_eq!(
            datas,
            [
                "\x1b]777;ratty:sound.ambient.set;kind=ambient.hum&gain=0.4&xfade=800\x07",
                "\x1b]777;ratty:sound.play;kind=chime\x07",
                "\x1b]777;ratty:sound.play;kind=click&gain=0.5\x07",
                "\x1b]777;ratty:sound.ambient.stop;fade=250\x07",
                "\x1b]777;ratty:sound.ambient.stop\x07",
            ]
        );
        // Every emitted OSC sequence must parse back through the terminal's
        // own decoder — the CLI-and-silk-share-osc.rs contract.
        let commands: Vec<super::osc::RattyAiCommand> = cast
            .events
            .iter()
            .map(|event| {
                let inner = event
                    .data
                    .strip_prefix("\x1b]777;")
                    .and_then(|s| s.strip_suffix('\x07'))
                    .expect("well-framed osc");
                super::osc::parse_command(inner).expect("terminal parses silk's osc")
            })
            .collect();
        assert_eq!(
            commands,
            vec![
                super::osc::RattyAiCommand::SoundAmbientSet {
                    kind: "ambient.hum".into(),
                    gain: Some(0.4),
                    xfade: Some(800),
                },
                super::osc::RattyAiCommand::SoundPlay {
                    kind: "chime".into(),
                    gain: None,
                },
                super::osc::RattyAiCommand::SoundPlay {
                    kind: "click".into(),
                    gain: Some(0.5),
                },
                super::osc::RattyAiCommand::SoundAmbientStop { fade: Some(250) },
                super::osc::RattyAiCommand::SoundAmbientStop { fade: None },
            ]
        );
    }

    #[test]
    fn viz_step_emits_parseable_osc_777_and_stamps_authored_capture() {
        let scene: Scene = serde_json::from_str(
            r#"{
                "meta": {"title": "t"},
                "cast": [
                    {"at": 0.0, "viz": {
                        "id": 2147483720, "kind": "chart.bar.v1",
                        "x": 4, "y": 2, "cols": 30, "rows": 10,
                        "data": {"title": "queue", "max": 10.0,
                                 "items": [{"key": "a", "value": 3.0}]}
                    }}
                ]
            }"#,
        )
        .unwrap();
        let cast = compile(&scene, Path::new(".")).unwrap();
        assert_eq!(cast.events.len(), 1);
        let inner = cast.events[0]
            .data
            .strip_prefix("\x1b]777;")
            .and_then(|s| s.strip_suffix('\x07'))
            .expect("well-framed osc");
        let Some(super::osc::RattyAiCommand::VizSet {
            id,
            kind,
            data,
            x,
            y,
            cols,
            rows,
            replace,
        }) = super::osc::parse_command(inner)
        else {
            panic!("terminal parses silk's viz.set");
        };
        assert_eq!(id, 2_147_483_720);
        assert_eq!(kind, "chart.bar.v1");
        assert_eq!((x.as_deref(), y.as_deref()), (Some("4"), Some("2")));
        assert_eq!((cols.as_deref(), rows.as_deref()), (Some("30"), Some("10")));
        assert!(!replace);
        // The payload decodes with the terminal's own decoder, and the
        // compiler stamped deterministic authored provenance.
        let payload = super::viz_wire::decode_viz_payload(&kind, &data)
            .expect("compiled payload always decodes");
        let capture = payload.capture();
        assert_eq!(capture.source, "authored");
        assert_eq!(capture.ts, "authored");
    }

    #[test]
    fn viz_step_keeps_an_authored_capture_and_rejects_bad_values() {
        // An author-supplied capture is never overwritten.
        let scene: Scene = serde_json::from_str(
            r#"{
                "meta": {"title": "t"},
                "cast": [
                    {"at": 0.0, "viz": {
                        "id": 2147483720, "kind": "chart.gauge.v1",
                        "data": {"capture": {"source": "synthetic demo", "ts": "2026-07-23"},
                                 "items": [{"key": "w", "value": 0.5}]}
                    }}
                ]
            }"#,
        )
        .unwrap();
        let cast = compile(&scene, Path::new(".")).unwrap();
        let inner = cast.events[0]
            .data
            .strip_prefix("\x1b]777;")
            .and_then(|s| s.strip_suffix('\x07'))
            .unwrap();
        let Some(super::osc::RattyAiCommand::VizSet { kind, data, .. }) =
            super::osc::parse_command(inner)
        else {
            panic!("parses");
        };
        let payload = super::viz_wire::decode_viz_payload(&kind, &data).expect("decodes");
        assert_eq!(payload.capture().source, "synthetic demo");

        for (viz_json, expected) in [
            (
                r#"{"kind": "chart.bar.v1", "data": {}}"#,
                "viz.id is required",
            ),
            (
                r#"{"id": 42, "kind": "chart.bar.v1", "data": {}}"#,
                "below the AI-owned range",
            ),
            (r#"{"id": 2147483720, "data": {}}"#, "viz.kind is required"),
            (
                r#"{"id": 2147483720, "kind": "chart.bar.v1"}"#,
                "viz.data is required",
            ),
            (
                r#"{"id": 2147483720, "kind": "chart.pie.v1", "data": {}}"#,
                "bad-kind",
            ),
            (
                r#"{"id": 2147483720, "kind": "chart.bar.v1",
                    "data": {"items": [{"key": "a", "value": -1.0}]}}"#,
                "bad-payload",
            ),
            (
                r#"{"id": 2147483720, "kind": "chart.bar.v1", "data": {}, "x": 4}"#,
                "place together",
            ),
            (
                r#"{"id": 2147483720, "kind": "chart.bar.v1", "data": {}, "cols": 10}"#,
                "need an anchor",
            ),
            (
                r#"{"id": 2147483720, "kind": "chart.bar.v1", "data": {},
                    "x": 1, "y": 1, "cols": 0}"#,
                "at least 1",
            ),
        ] {
            let scene: Scene = serde_json::from_str(&format!(
                r#"{{"meta": {{"title": "t"}}, "cast": [{{"at": 0.0, "viz": {viz_json}}}]}}"#
            ))
            .unwrap();
            let error = compile(&scene, Path::new(".")).unwrap_err();
            assert!(
                format!("{error:#}").contains(expected),
                "expected \"{expected}\" in: {error:#}"
            );
        }
    }

    #[test]
    fn sound_step_rejects_bad_values() {
        for (sound_json, expected) in [
            (r#"{}"#, "exactly one of"),
            (
                r#"{"play": "chime", "ambient": "ambient.hum"}"#,
                "exactly one of",
            ),
            (r#"{"play": "boom"}"#, "unknown sound kind"),
            (r#"{"play": "ambient.hum"}"#, "is an ambient bed"),
            (r#"{"ambient": "chime"}"#, "is a one-shot"),
            (r#"{"ambient": "boom"}"#, "unknown sound kind"),
            (r#"{"play": "chime", "gain": 1.5}"#, "out of range"),
            (
                r#"{"play": "chime", "xfade": 0.5}"#,
                "no effect on a one-shot",
            ),
            (r#"{"stop": true, "gain": 0.5}"#, "no effect on stop"),
            (r#"{"stop": false}"#, "must be true"),
            (
                r#"{"ambient": "ambient.hum", "xfade": -1.0}"#,
                "non-negative",
            ),
        ] {
            let json =
                format!(r#"{{"meta":{{"title":"t"}},"cast":[{{"at":0.0,"sound":{sound_json}}}]}}"#);
            let scene: Scene = serde_json::from_str(&json).unwrap();
            let error = compile(&scene, Path::new(".")).unwrap_err();
            assert!(
                format!("{error:#}").contains(expected),
                "expected \"{expected}\" in: {error:#}"
            );
        }
    }

    /// `deny_unknown_fields` is the airtight form of "registry kinds only,
    /// never paths/URLs" and "no tok=, no master gain": the fields do not
    /// exist, so scenes naming them fail to parse at all.
    #[test]
    fn sound_args_have_no_path_tok_or_master_fields() {
        for sound_json in [
            r#"{"play": "chime", "tok": "a1"}"#,
            r#"{"play": "chime", "path": "boom.ogg"}"#,
            r#"{"play": "chime", "url": "https://x/boom.ogg"}"#,
            r#"{"ambient": "ambient.hum", "master_gain": 1.0}"#,
        ] {
            let json =
                format!(r#"{{"meta":{{"title":"t"}},"cast":[{{"at":0.0,"sound":{sound_json}}}]}}"#);
            assert!(
                serde_json::from_str::<Scene>(&json).is_err(),
                "scene with {sound_json} must fail to parse"
            );
        }
    }

    #[test]
    fn print_el_erases_to_end_of_line_inside_sgr() {
        let scene: Scene = serde_json::from_str(
            r#"{
                "meta": {"title": "t"},
                "cast": [
                    {"at": 0.0, "print": {"row": 0, "col": 0, "text": "hi",
                                           "bg": "101010", "el": true}}
                ]
            }"#,
        )
        .unwrap();
        let cast = compile(&scene, Path::new(".")).unwrap();
        assert!(cast.events[0].data.ends_with("hi\x1b[K\x1b[0m"));
    }

    /// Golden proof: every committed transmission must recompile
    /// byte-identically. The orchard is pure v1 (back-compat lock);
    /// predator-and-frame exercises the full v2 surface. If this fails, a
    /// compiler change altered shipped output — that is a regression, not
    /// a test to update casually.
    #[test]
    fn golden_transmissions_compile_byte_identically() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../transmissions");
        for slug in ["orchard-upside-down", "predator-and-frame", "soul"] {
            let scene_path = root.join(slug).join("scene.json");
            let source = fs::read_to_string(&scene_path).expect("read scene");
            let scene: Scene = serde_json::from_str(&source).expect("parse scene");
            let cast = compile(&scene, scene_path.parent().expect("scene dir")).expect("compile");
            let jsonl = cast.to_jsonl().expect("serialize");
            let committed =
                fs::read_to_string(root.join(slug).join("cast.silk")).expect("read cast");
            assert_eq!(
                jsonl, committed,
                "{slug} cast drifted from its committed bytes"
            );
        }
    }

    #[test]
    fn events_are_monotonic() {
        let scene: Scene = serde_json::from_str(
            r#"{
                "meta": {"title": "t"},
                "cast": [
                    {"at": 2.0, "print": {"row": 0, "col": 0, "text": "late"}},
                    {"at": 0.0, "clear": true},
                    {"at": 1.0, "marker": "mid"}
                ]
            }"#,
        )
        .unwrap();
        let cast = compile(&scene, Path::new(".")).unwrap();
        let times: Vec<f64> = cast.events.iter().map(|event| event.time).collect();
        assert_eq!(times, [0.0, 1.0, 2.0]);
    }

    #[test]
    fn macro_block_brackets_the_enclosed_choreography() {
        let scene: Scene = serde_json::from_str(
            r##"{
                "meta": {"title": "t"},
                "cast": [
                    {"at": 1.0, "macro": {"name": "greet", "replace": true, "cast": [
                        {"at": 1.0, "ai": {"flash": "#00ff00"}},
                        {"at": 2.5, "ai": {"pulse": 0.5}}
                    ]}}
                ]
            }"##,
        )
        .unwrap();
        let cast = compile(&scene, Path::new(".")).unwrap();
        // record at the block time, stop after the last enclosed event.
        let record = cast
            .events
            .iter()
            .find(|event| event.data.contains("macro.record"))
            .expect("record emitted");
        let stop = cast
            .events
            .iter()
            .find(|event| event.data.contains("macro.stop"))
            .expect("stop emitted");
        assert_eq!(record.time, 1.0);
        assert!(record.data.contains("name=greet"));
        assert!(record.data.contains("mode=replace"));
        assert_eq!(stop.time, 2.5, "stop follows the last enclosed event");
        // The two enclosed effect commands sit inside the bracket in time.
        let flash = cast
            .events
            .iter()
            .find(|event| event.data.contains("flash"))
            .expect("flash emitted");
        assert!(record.time <= flash.time && flash.time <= stop.time);
    }

    #[test]
    fn macro_block_rejects_nesting_and_reset() {
        let nested: Scene = serde_json::from_str(
            r#"{
                "meta": {"title": "t"},
                "cast": [
                    {"at": 0.0, "macro": {"name": "outer", "cast": [
                        {"at": 0.0, "macro": {"name": "inner", "cast": []}}
                    ]}}
                ]
            }"#,
        )
        .unwrap();
        let error = compile(&nested, Path::new(".")).unwrap_err();
        assert!(format!("{error:#}").contains("nest"));

        let with_reset: Scene = serde_json::from_str(
            r#"{
                "meta": {"title": "t"},
                "cast": [
                    {"at": 0.0, "macro": {"name": "m", "cast": [
                        {"at": 0.0, "ai": {"reset": true}}
                    ]}}
                ]
            }"#,
        )
        .unwrap();
        let error = compile(&with_reset, Path::new(".")).unwrap_err();
        assert!(format!("{error:#}").contains("cancel the recording"));
    }
}
