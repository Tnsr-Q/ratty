//! Browser embedding: the wasm-bindgen session API.
//!
//! [`start`] boots the same Bevy app `main.rs` builds natively, but drives
//! the terminal through a virtual byte channel instead of a PTY. JavaScript
//! feeds transmission bytes with [`RattySession::feed`] and reads back
//! whatever the terminal writes (RGP support replies, cursor position
//! reports) with [`RattySession::drain_input`].

#![cfg(target_arch = "wasm32")]

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use bevy::render::RenderPlugin;
use bevy::render::settings::{RenderCreation, WgpuSettings, WgpuSettingsPriority};
use bevy::window::WindowResolution;
use bevy::winit::WinitSettings;
use wasm_bindgen::prelude::*;

use crate::config::AppConfig;
use crate::plugin::TerminalPlugin;
use crate::runtime::TerminalRuntime;
use crate::scene::{
    MobiusTransition, TerminalPlaneView, TerminalPlaneWarp, TerminalPresentation,
    TerminalPresentationMode,
};
use crate::terminal::{TerminalRedrawState, TerminalSurface};

/// Stage controls queued from JS, drained once per frame by
/// [`drain_web_controls`].
#[derive(Default)]
struct WebControlState {
    mode: Option<TerminalPresentationMode>,
    warp: Option<f32>,
    view: Option<(f32, f32, f32)>,
}

#[derive(Resource, Clone, Default)]
struct WebControlQueue(Arc<Mutex<WebControlState>>);

/// A running browser terminal session.
#[wasm_bindgen]
pub struct RattySession {
    feed: Sender<Vec<u8>>,
    input: Receiver<Vec<u8>>,
    controls: Arc<Mutex<WebControlState>>,
}

#[wasm_bindgen]
impl RattySession {
    /// Feeds terminal output bytes (text, ANSI, RGP, Kitty) into the session.
    pub fn feed(&self, bytes: &[u8]) {
        let _ = self.feed.send(bytes.to_vec());
    }

    /// Feeds UTF-8 text.
    pub fn feed_text(&self, text: &str) {
        let _ = self.feed.send(text.as_bytes().to_vec());
    }

    /// Drains bytes the terminal wrote back as input (keystrokes, RGP
    /// support replies). Returns an empty buffer when nothing is pending.
    pub fn drain_input(&self) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(chunk) = self.input.try_recv() {
            out.extend_from_slice(&chunk);
        }
        out
    }

    /// Sets the presentation mode: `flat2d`, `plane3d`, or `mobius3d`.
    pub fn set_mode(&self, mode: &str) {
        let mode = match mode {
            "flat2d" => TerminalPresentationMode::Flat2d,
            "plane3d" => TerminalPresentationMode::Plane3d,
            "mobius3d" => TerminalPresentationMode::Mobius3d,
            _ => return,
        };
        if let Ok(mut controls) = self.controls.lock() {
            controls.mode = Some(mode);
        }
    }

    /// Sets the warp amount (`0.0..=1.0`).
    pub fn set_warp(&self, amount: f32) {
        if let Ok(mut controls) = self.controls.lock() {
            controls.warp = Some(amount);
        }
    }

    /// Sets the 3D camera view (yaw and pitch in radians, orthographic zoom).
    pub fn set_view(&self, yaw: f32, pitch: f32, zoom: f32) {
        if let Ok(mut controls) = self.controls.lock() {
            controls.view = Some((yaw, pitch, zoom));
        }
    }
}

/// Starts ratty rendering into the canvas selected by `canvas_selector` and
/// returns the live session handle. `config_toml` optionally overrides the
/// default configuration (same TOML schema as `ratty.toml`).
///
/// # Errors
///
/// Returns a JS error when the config TOML or the terminal surface cannot be
/// initialized.
#[wasm_bindgen]
pub fn start(canvas_selector: &str, config_toml: Option<String>) -> Result<RattySession, JsValue> {
    console_error_panic_hook::set_once();

    let app_config = match config_toml.as_deref() {
        Some(toml) => AppConfig::from_toml_str(toml)
            .map_err(|error| JsValue::from_str(&format!("{error:#}")))?,
        None => AppConfig::default(),
    };

    let (runtime, host) = TerminalRuntime::virtual_channel(&app_config);
    let terminal = TerminalSurface::new(&app_config)
        .map_err(|error| JsValue::from_str(&format!("{error:#}")))?;
    let controls = WebControlQueue::default();
    let session = RattySession {
        feed: host.feed_tx,
        input: host.input_rx,
        controls: Arc::clone(&controls.0),
    };

    let mut app = App::new();
    app.insert_resource(ClearColor(Color::srgba_u8(
        app_config.theme.background[0],
        app_config.theme.background[1],
        app_config.theme.background[2],
        (app_config.window.opacity.clamp(0.0, 1.0) * 255.0).round() as u8,
    )))
    .insert_resource(app_config.clone())
    .insert_resource(runtime)
    .insert_resource(terminal)
    .insert_resource(controls)
    // Continuous updates: transmissions animate even without input focus.
    .insert_resource(WinitSettings::continuous())
    .add_plugins(
        DefaultPlugins
            .set(WindowPlugin {
                primary_window: Some(Window {
                    canvas: Some(canvas_selector.to_string()),
                    fit_canvas_to_parent: true,
                    // Keep browser shortcuts (devtools, reload) working; the
                    // page decides which keys reach the terminal.
                    prevent_default_event_handling: false,
                    resolution: WindowResolution::new(
                        app_config.window.width,
                        app_config.window.height,
                    ),
                    transparent: app_config.window.opacity < 1.0,
                    ..default()
                }),
                ..default()
            })
            .set(RenderPlugin {
                render_creation: RenderCreation::Automatic(Box::new(WgpuSettings {
                    // Vello's compute rasterizer needs real WebGPU; never
                    // fall back to WebGL2 limits.
                    priority: WgpuSettingsPriority::WebGPU,
                    ..default()
                })),
                ..default()
            }),
    )
    .add_plugins(TerminalPlugin)
    .add_systems(Update, drain_web_controls);

    // Bevy's winit runner on wasm spawns onto the browser event loop and
    // returns, so the session handle is live after this call.
    app.run();

    Ok(session)
}

/// Applies queued JS stage controls to the same resources the keyboard and
/// mouse mutate, mirroring the keyboard's mode-toggle semantics (Möbius
/// enters and exits through its camera transition).
fn drain_web_controls(
    queue: Res<WebControlQueue>,
    mut presentation: ResMut<TerminalPresentation>,
    mut warp: ResMut<TerminalPlaneWarp>,
    mut view: ResMut<TerminalPlaneView>,
    mut mobius: ResMut<MobiusTransition>,
    mut redraw: ResMut<TerminalRedrawState>,
) {
    let pending = match queue.0.lock() {
        Ok(mut controls) => std::mem::take(&mut *controls),
        Err(_) => return,
    };

    if let Some(mode) = pending.mode
        && mode != presentation.mode
    {
        let current = presentation.mode;
        if current == TerminalPresentationMode::Mobius3d {
            let current_zoom = if mobius.active {
                mobius.current_zoom()
            } else {
                view.zoom
            };
            mobius.begin_exit(&view, current_zoom);
        } else if mode == TerminalPresentationMode::Mobius3d {
            presentation.mode = TerminalPresentationMode::Mobius3d;
            mobius.begin_enter(current, &view);
        } else {
            presentation.mode = mode;
            mobius.stop();
        }
        redraw.request();
    }

    if let Some(amount) = pending.warp {
        warp.amount = amount.clamp(0.0, 1.0);
        redraw.request();
    }

    if let Some((yaw, pitch, zoom)) = pending.view {
        view.yaw = yaw;
        view.pitch = pitch;
        view.zoom = zoom.clamp(0.1, 4.0);
        redraw.request();
    }
}
