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
use crate::scene::{
    PlaceArgs, PrintArgs, RegisterArgs, Scene, Step, TWEENABLE_FIELDS, TweenArgs, UpdateArgs,
};

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
        }
    }
}

impl ObjectState {
    fn get(&self, field: &str) -> f64 {
        match field {
            "px" => self.px,
            "py" => self.py,
            "pz" => self.pz,
            "rx" => self.rx,
            "ry" => self.ry,
            "rz" => self.rz,
            "sx" => self.sx,
            "sy" => self.sy,
            "sz" => self.sz,
            "scale" => self.scale,
            _ => 0.0,
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
                "each step must have exactly one verb \
                 (print/register/place/update/tween/delete/marker/clear)"
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
        let ease = match tween.ease.as_deref() {
            None | Some("linear") => Ease::Linear,
            Some("in-out") => Ease::InOut,
            Some(other) => bail!("unknown ease \"{other}\" (linear, in-out)"),
        };

        let from = *self.objects.entry(tween.id).or_default();
        let steps = ((tween.dur * fps).ceil() as usize).max(1);
        for step in 1..=steps {
            let progress = step as f64 / steps as f64;
            let eased = ease.apply(progress);
            let mut fields = vec![format!("id={}", tween.id)];
            for (field, target) in &tween.to {
                let start = from.get(field);
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
}

enum Ease {
    Linear,
    InOut,
}

impl Ease {
    fn apply(&self, t: f64) -> f64 {
        match self {
            Self::Linear => t,
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
}
