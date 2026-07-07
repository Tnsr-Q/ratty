//! Silk cast container: the asciinema-v2-superset JSON Lines format.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

/// Cast header (line 1 of a `.silk` file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Header {
    /// Always `2` — asciinema v2 compatibility.
    pub version: u32,
    /// Terminal grid columns.
    pub width: u16,
    /// Terminal grid rows.
    pub height: u16,
    /// Transmission title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional terminal theme.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<Theme>,
    /// Optional cap applied to inter-event gaps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_time_limit: Option<f64>,
    /// Namespaced Silk metadata; ignored by stock asciinema players.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x_ratty: Option<XRatty>,
}

/// asciinema-compatible theme colors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    /// Foreground color (`#rrggbb`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    /// Background color (`#rrggbb`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    /// Colon-separated 8- or 16-color palette.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub palette: Option<String>,
}

/// Silk metadata namespace (`x_ratty`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XRatty {
    /// Format tag, `silk/1`.
    pub format: String,
    /// Authoring agent identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Art-direction mood tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mood: Option<String>,
    /// Opening presentation mode: `flat2d`, `plane3d`, `mobius3d`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Opening warp amount.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warp: Option<f32>,
    /// Opening camera view.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view: Option<View>,
    /// Player should loop the transmission.
    #[serde(rename = "loop", skip_serializing_if = "Option::is_none")]
    pub loop_: Option<bool>,
    /// Optional `sha256:<hex>` checksum of all event lines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
}

/// Opening camera parameters.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct View {
    /// Camera yaw in radians.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yaw: Option<f32>,
    /// Camera pitch in radians.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pitch: Option<f32>,
    /// Orthographic zoom.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zoom: Option<f32>,
}

/// One timed event: `[time, code, data]`.
#[derive(Debug, Clone)]
pub struct Event {
    /// Absolute seconds since transmission start.
    pub time: f64,
    /// Event code: `o`, `m`, `i`, or future codes.
    pub code: String,
    /// Event payload.
    pub data: String,
}

/// A parsed cast.
#[derive(Debug, Clone)]
pub struct Cast {
    /// The header line.
    pub header: Header,
    /// All events, in file order.
    pub events: Vec<Event>,
}

impl Cast {
    /// Total duration: the time of the last event.
    pub fn duration_secs(&self) -> f64 {
        self.events.last().map(|event| event.time).unwrap_or(0.0)
    }

    /// Serializes the cast to `.silk` JSON Lines.
    pub fn to_jsonl(&self) -> Result<String> {
        let mut out = serde_json::to_string(&self.header)?;
        out.push('\n');
        for event in &self.events {
            let line =
                serde_json::to_string(&(event.time, event.code.as_str(), event.data.as_str()))?;
            out.push_str(&line);
            out.push('\n');
        }
        Ok(out)
    }
}

/// Reads and parses a `.silk` file.
pub fn read_cast(path: &Path) -> Result<Cast> {
    parse_cast(&fs::read_to_string(path)?)
}

/// Parses `.silk` JSON Lines content.
pub fn parse_cast(content: &str) -> Result<Cast> {
    let mut lines = content.lines().enumerate();
    let (_, header_line) = lines.next().ok_or_else(|| anyhow!("empty cast file"))?;
    let header: Header = serde_json::from_str(header_line).context("invalid header (line 1)")?;
    if header.version != 2 {
        bail!("unsupported cast version {} (expected 2)", header.version);
    }

    let mut events = Vec::new();
    for (index, line) in lines {
        if line.trim().is_empty() {
            continue;
        }
        let (time, code, data): (f64, String, String) = serde_json::from_str(line)
            .with_context(|| format!("invalid event (line {})", index + 1))?;
        events.push(Event { time, code, data });
    }

    Ok(Cast { header, events })
}

/// One entry of `index.json`.
#[derive(Debug, Serialize)]
struct IndexEntry {
    slug: String,
    title: Option<String>,
    agent: Option<String>,
    mood: Option<String>,
    mode: Option<String>,
    #[serde(rename = "loop")]
    loop_: Option<bool>,
    duration_secs: f64,
    events: usize,
    bytes: usize,
    cast: String,
}

/// Scans `dir` for `<slug>/cast.silk` files and writes `dir/index.json`.
///
/// Returns the number of transmissions indexed.
pub fn write_index(dir: &Path) -> Result<usize> {
    let mut entries = Vec::new();
    for dir_entry in fs::read_dir(dir)? {
        let dir_entry = dir_entry?;
        if !dir_entry.file_type()?.is_dir() {
            continue;
        }
        let slug = dir_entry.file_name().to_string_lossy().into_owned();
        let cast_path = dir_entry.path().join("cast.silk");
        if !cast_path.is_file() {
            continue;
        }
        let cast = read_cast(&cast_path)
            .with_context(|| format!("failed to parse {}", cast_path.display()))?;
        let x_ratty = cast.header.x_ratty.as_ref();
        entries.push(IndexEntry {
            title: cast.header.title.clone(),
            agent: x_ratty.and_then(|x| x.agent.clone()),
            mood: x_ratty.and_then(|x| x.mood.clone()),
            mode: x_ratty.and_then(|x| x.mode.clone()),
            loop_: x_ratty.and_then(|x| x.loop_),
            duration_secs: cast.duration_secs(),
            events: cast.events.len(),
            bytes: fs::metadata(&cast_path)?.len() as usize,
            cast: format!("{slug}/cast.silk"),
            slug,
        });
    }
    entries.sort_by(|a, b| a.slug.cmp(&b.slug));

    let count = entries.len();
    let json = serde_json::to_string_pretty(&entries)?;
    fs::write(dir.join("index.json"), format!("{json}\n"))?;
    Ok(count)
}
