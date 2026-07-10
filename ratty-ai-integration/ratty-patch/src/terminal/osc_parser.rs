//! OSC 777 Parser — Extended for ratty-ai v2

use bevy::prelude::*;
use std::collections::HashMap;

#[derive(Event, Debug, Clone)]
pub enum RattyAiCommand {
    // Originals
    SetMode { mode: String },
    SetWarp { intensity: f32 },
    SpawnObject { path: String, x: u16, y: u16, scale: f32, spin: f32, brightness: f32, remesh: bool },
    RemoveObject { id: u32 },
    ClearObjects,
    UpdateObject { id: u32, x: Option<u16>, y: Option<u16>, scale: Option<f32>, spin: Option<f32>, brightness: Option<f32> },
    Flash { color: String, duration: f32 },
    Pulse { intensity: f32, duration: f32 },
    UpdateCursor { model: Option<String>, spin: Option<f32>, bob_speed: Option<f32>, bob_amp: Option<f32>, brightness: Option<f32>, visible: Option<bool> },
    Reset,
    Screenshot { path: String },
    Chart { kind: String, x: u16, y: u16, scale: f32, data: String },
    Timeline { x: u16, y: u16, scale: f32, input: String },
    Tint { color: String, opacity: f32 },

    // : Process
    Ps { visualize: bool, highlight: Option<u32>, color: Option<String> },
    Kill { pid: u32, effect: String },

    // : File System
    Cd { path: String, visualize: bool },
    Ls { path: String, visualize: bool },
    Tree { depth: u8, visualize: bool },

    // : Git
    GitBranch { visualize: bool },
    GitDiff { visualize: bool },
    GitMerge { visualize: bool },
    GitStash { visualize: bool },

    // : Network
    Net { visualize: bool, host: Option<String> },

    // : AI State
    Think { state: String },
    Confidence { level: f32 },
    Mood { mood: String },

    // : Panes
    SplitPane { direction: String, ratio: f32 },
    FocusPane { pane: u8 },
    ResizePane { pane: u8, width: Option<u16>, height: Option<u16> },
    ClosePane { pane: u8 },

    // : History
    History { last: usize, visualize: bool },
    Bookmark { name: String },
    Jump { name: String },

    // : Collaboration
    UserJoin { name: String, color: String },
    UserLeave { name: String },
    UserCursor { name: String, x: u16, y: u16 },
    Note { text: String, x: u16, y: u16, expires: String },

    // : Sound
    Sound { kind: String, loop_sound: bool },

    // : Avatar
    AvatarSet { model: String, position: String },
    AvatarGesture { gesture: String },
    AvatarSpeak { text: String },
    AvatarHide,

    // : Macro
    MacroRecord { name: String },
    MacroStop,
    MacroPlay { name: String },
    MacroExport { name: String, to: String },
    MacroRun { path: String },

    // : Reactive
    React { effect: String, cpu_high: Option<f32>, memory_high: Option<f32>, battery_low: Option<f32> },
}

pub fn parse_osc_777(data: &str) -> Option<RattyAiCommand> {
    let rest = data.strip_prefix("ratty:")?;
    let (action, payload_enc) = rest.split_once(';').unwrap_or((rest, ""));
    let payload = urlencoding::decode(payload_enc).ok()?;
    let p = parse_params(&payload);

    match action {
        // Originals
        "mode" => Some(RattyAiCommand::SetMode { mode: payload.to_string() }),
        "warp" => Some(RattyAiCommand::SetWarp { intensity: p.get("intensity").and_then(|s| s.parse().ok()).unwrap_or(0.0) }),
        "object.add" => Some(RattyAiCommand::SpawnObject { path: p.get("path")?.to_string(), x: get_u16(&p, "x"), y: get_u16(&p, "y"), scale: get_f32(&p, "scale", 1.0), spin: get_f32(&p, "spin", 0.0), brightness: get_f32(&p, "brightness", 1.0), remesh: p.get("remesh").map(|s| s == "true").unwrap_or(false) }),
        "object.remove" => Some(RattyAiCommand::RemoveObject { id: p.get("id")?.parse().ok()? }),
        "object.clear" => Some(RattyAiCommand::ClearObjects),
        "object.update" => Some(RattyAiCommand::UpdateObject { id: p.get("id")?.parse().ok()?, x: p.get("x").and_then(|s| s.parse().ok()), y: p.get("y").and_then(|s| s.parse().ok()), scale: p.get("scale").and_then(|s| s.parse().ok()), spin: p.get("spin").and_then(|s| s.parse().ok()), brightness: p.get("brightness").and_then(|s| s.parse().ok()) }),
        "flash" => Some(RattyAiCommand::Flash { color: p.get("color").unwrap_or("#ffffff").to_string(), duration: get_f32(&p, "duration", 0.5) }),
        "pulse" => Some(RattyAiCommand::Pulse { intensity: get_f32(&p, "intensity", 0.8), duration: get_f32(&p, "duration", 1.0) }),
        "cursor" => Some(RattyAiCommand::UpdateCursor { model: p.get("model").map(|s| s.to_string()), spin: p.get("spin").and_then(|s| s.parse().ok()), bob_speed: p.get("bob_speed").and_then(|s| s.parse().ok()), bob_amp: p.get("bob_amp").and_then(|s| s.parse().ok()), brightness: p.get("brightness").and_then(|s| s.parse().ok()), visible: p.get("visible").map(|s| s == "true") }),
        "reset" => Some(RattyAiCommand::Reset),
        "screenshot" => Some(RattyAiCommand::Screenshot { path: p.get("path").unwrap_or("ratty-screenshot.png").to_string() }),
        "chart" => Some(RattyAiCommand::Chart { kind: p.get("kind").unwrap_or("bar").to_string(), x: get_u16(&p, "x"), y: get_u16(&p, "y"), scale: get_f32(&p, "scale", 1.0), data: p.get("data").unwrap_or("[]").to_string() }),
        "timeline" => Some(RattyAiCommand::Timeline { x: get_u16(&p, "x"), y: get_u16(&p, "y"), scale: get_f32(&p, "scale", 1.0), input: p.get("input").unwrap_or("").to_string() }),
        "tint" => Some(RattyAiCommand::Tint { color: payload.to_string(), opacity: get_f32(&p, "opacity", 0.1) }),

        // : Process
        "ps" => Some(RattyAiCommand::Ps { visualize: p.get("visualize").map(|s| s == "true").unwrap_or(false), highlight: p.get("highlight").and_then(|s| s.parse().ok()), color: p.get("color").map(|s| s.to_string()) }),
        "kill" => Some(RattyAiCommand::Kill { pid: p.get("pid")?.parse().ok()?, effect: p.get("effect").unwrap_or("explode").to_string() }),

        // : File System
        "cd" => Some(RattyAiCommand::Cd { path: p.get("path")?.to_string(), visualize: p.get("visualize").map(|s| s == "true").unwrap_or(false) }),
        "ls" => Some(RattyAiCommand::Ls { path: p.get("path").unwrap_or(".").to_string(), visualize: p.get("visualize").map(|s| s == "true").unwrap_or(false) }),
        "tree" => Some(RattyAiCommand::Tree { depth: p.get("depth").and_then(|s| s.parse().ok()).unwrap_or(3), visualize: p.get("visualize").map(|s| s == "true").unwrap_or(false) }),

        // : Git
        "git.branch" => Some(RattyAiCommand::GitBranch { visualize: p.get("visualize").map(|s| s == "true").unwrap_or(false) }),
        "git.diff" => Some(RattyAiCommand::GitDiff { visualize: p.get("visualize").map(|s| s == "true").unwrap_or(false) }),
        "git.merge" => Some(RattyAiCommand::GitMerge { visualize: p.get("visualize").map(|s| s == "true").unwrap_or(false) }),
        "git.stash" => Some(RattyAiCommand::GitStash { visualize: p.get("visualize").map(|s| s == "true").unwrap_or(false) }),

        // : Network
        "net" => Some(RattyAiCommand::Net { visualize: p.get("visualize").map(|s| s == "true").unwrap_or(false), host: p.get("host").map(|s| s.to_string()) }),

        // : AI State
        "think" => Some(RattyAiCommand::Think { state: p.get("state").unwrap_or("toggle").to_string() }),
        "confidence" => Some(RattyAiCommand::Confidence { level: get_f32(&p, "level", 0.5) }),
        "mood" => Some(RattyAiCommand::Mood { mood: p.get("mood").unwrap_or("focused").to_string() }),

        // : Panes
        "pane.split" => Some(RattyAiCommand::SplitPane { direction: p.get("direction").unwrap_or("vertical").to_string(), ratio: get_f32(&p, "ratio", 0.5) }),
        "pane.focus" => Some(RattyAiCommand::FocusPane { pane: p.get("pane")?.parse().ok()? }),
        "pane.resize" => Some(RattyAiCommand::ResizePane { pane: p.get("pane")?.parse().ok()?, width: p.get("width").and_then(|s| s.parse().ok()), height: p.get("height").and_then(|s| s.parse().ok()) }),
        "pane.close" => Some(RattyAiCommand::ClosePane { pane: p.get("pane")?.parse().ok()? }),

        // : History
        "history" => Some(RattyAiCommand::History { last: p.get("last").and_then(|s| s.parse().ok()).unwrap_or(50), visualize: p.get("visualize").map(|s| s == "true").unwrap_or(false) }),
        "bookmark" => Some(RattyAiCommand::Bookmark { name: p.get("name")?.to_string() }),
        "jump" => Some(RattyAiCommand::Jump { name: p.get("name")?.to_string() }),

        // : Collaboration
        "user.join" => Some(RattyAiCommand::UserJoin { name: p.get("name")?.to_string(), color: p.get("color").unwrap_or("#00ff00").to_string() }),
        "user.leave" => Some(RattyAiCommand::UserLeave { name: p.get("name")?.to_string() }),
        "user.cursor" => Some(RattyAiCommand::UserCursor { name: p.get("name")?.to_string(), x: get_u16(&p, "x"), y: get_u16(&p, "y") }),
        "note" => Some(RattyAiCommand::Note { text: p.get("text")?.to_string(), x: get_u16(&p, "x"), y: get_u16(&p, "y"), expires: p.get("expires").unwrap_or("1h").to_string() }),

        // : Sound
        "sound" => Some(RattyAiCommand::Sound { kind: p.get("kind").unwrap_or("click").to_string(), loop_sound: p.get("loop").map(|s| s == "true").unwrap_or(false) }),

        // : Avatar
        "avatar.set" => Some(RattyAiCommand::AvatarSet { model: p.get("model").unwrap_or("ai-helper.glb").to_string(), position: p.get("position").unwrap_or("top-right").to_string() }),
        "avatar.gesture" => Some(RattyAiCommand::AvatarGesture { gesture: p.get("gesture").unwrap_or("wave").to_string() }),
        "avatar.speak" => Some(RattyAiCommand::AvatarSpeak { text: p.get("text")?.to_string() }),
        "avatar.hide" => Some(RattyAiCommand::AvatarHide),

        // : Macro
        "macro.record" => Some(RattyAiCommand::MacroRecord { name: p.get("name")?.to_string() }),
        "macro.stop" => Some(RattyAiCommand::MacroStop),
        "macro.play" => Some(RattyAiCommand::MacroPlay { name: p.get("name")?.to_string() }),
        "macro.export" => Some(RattyAiCommand::MacroExport { name: p.get("name")?.to_string(), to: p.get("to").unwrap_or("macro.ratty").to_string() }),
        "macro.run" => Some(RattyAiCommand::MacroRun { path: p.get("path")?.to_string() }),

        // : Reactive
        "react" => Some(RattyAiCommand::React { effect: p.get("effect")?.to_string(), cpu_high: p.get("cpu_high").and_then(|s| s.parse().ok()), memory_high: p.get("memory_high").and_then(|s| s.parse().ok()), battery_low: p.get("battery_low").and_then(|s| s.parse().ok()) }),

        _ => None,
    }
}

fn parse_params(payload: &str) -> HashMap<String, String> {
    let mut map = HashMap::();
    for pair in payload.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    map
}

fn get_u16(p: &HashMap<String, String>, key: &str) -> u16 {
    p.get(key).and_then(|s| s.parse().ok()).unwrap_or(0)
}

fn get_f32(p: &HashMap<String, String>, key: &str, default: f32) -> f32 {
    p.get(key).and_then(|s| s.parse().ok()).unwrap_or(default)
}
Some(777) => {
    if let Some(cmd) = parse_osc_777(data) {
        let _ = self.ai_command_tx.try_send(cmd);
    }
}
