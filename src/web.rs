//! Browser embedding: the wasm-bindgen session API.
//!
//! [`start`] boots the same Bevy app `main.rs` builds natively, but drives
//! the terminal through a virtual byte channel instead of a PTY. JavaScript
//! feeds transmission bytes with [`RattySession::feed`] and reads back
//! whatever the terminal writes (RGP support replies, cursor position
//! reports) with [`RattySession::drain_input`].

#![cfg(target_arch = "wasm32")]

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use bevy::render::RenderPlugin;
use bevy::render::settings::{RenderCreation, WgpuSettings, WgpuSettingsPriority};
use bevy::window::WindowResolution;
use bevy::winit::WinitSettings;
use js_sys::{Function, Promise, Reflect};
use wasm_bindgen::prelude::*;

use crate::config::AppConfig;
use crate::plugin::TerminalPlugin;
use crate::runtime::TerminalRuntime;
use crate::scene::{
    MobiusTransition, StageTween, TerminalPlaneView, TerminalPlaneWarp, TerminalPresentation,
    TerminalPresentationMode, apply_stage_mode_change,
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

/// A `RattySession.query()` promise awaiting its OSC 778 reply.
struct PendingQuery {
    resolve: Function,
    reject: Function,
    timeout_ms: f64,
    /// Stamped by [`expire_query_promises`] on its first sweep, so the
    /// timeout is measured on the same clock that resolves replies.
    deadline: Option<f64>,
}

thread_local! {
    // Wasm is single-threaded: RattySession methods and the Bevy schedule
    // interleave on the JS main thread, so a thread-local map is the whole
    // synchronization story. One map per page — sessions are not expected
    // to coexist (one `start()` per page), and disposal rejects everything.
    static PENDING_QUERIES: RefCell<HashMap<String, PendingQuery>> = RefCell::new(HashMap::new());
}

/// Builds the JS `Error` a failed query rejects with; the stable error
/// code also rides on the error's `code` property.
fn query_error(code: &str, message: &str) -> JsValue {
    let error = js_sys::Error::new(&format!("{code}: {message}"));
    let _ = Reflect::set(&error, &JsValue::from_str("code"), &JsValue::from_str(code));
    error.into()
}

fn generate_token() -> String {
    let mut bytes = [0_u8; 16];
    getrandom03::fill(&mut bytes).expect("system entropy is available");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Resolves or rejects the pending `query()` promise for `token`, if one
/// exists. Called by the query channel's reply writer *instead of* pushing
/// the reply bytes into the input stream — on wasm the promise is the
/// originating-transport reply path for session-issued queries. Raw 778
/// bytes fed by hand (not via `query()`) still reply through
/// `drain_input` as on native.
pub(crate) fn try_resolve_pending(
    token: &str,
    _ack: bool,
    ok: bool,
    code: Option<&str>,
    payload: Option<&[u8]>,
) -> bool {
    let Some(pending) = PENDING_QUERIES.with(|map| map.borrow_mut().remove(token)) else {
        return false;
    };
    if ok {
        let value = match payload {
            None => JsValue::NULL,
            Some(bytes) => {
                match std::str::from_utf8(bytes).ok().map(js_sys::JSON::parse) {
                    Some(Ok(value)) => value,
                    // Structurally impossible for terminal-built replies;
                    // reject rather than hand JS a broken value.
                    _ => {
                        let _ = pending.reject.call1(
                            &JsValue::NULL,
                            &query_error(
                                crate::query::codes::INTERNAL,
                                "reply payload was not valid JSON",
                            ),
                        );
                        return true;
                    }
                }
            }
        };
        let _ = pending.resolve.call1(&JsValue::NULL, &value);
    } else {
        let code = code.unwrap_or(crate::query::codes::INTERNAL);
        let _ = pending.reject.call1(
            &JsValue::NULL,
            &query_error(code, "the terminal returned ok=0"),
        );
    }
    true
}

/// Rejects pending queries whose deadline passed. Runs on Bevy's frame
/// clock: while the tab is hidden and rAF is throttled, both replies and
/// timeouts are deferred together.
fn expire_query_promises(time: Res<Time>) {
    let now = time.elapsed_secs_f64();
    PENDING_QUERIES.with(|map| {
        let mut map = map.borrow_mut();
        let mut expired = Vec::new();
        for (token, entry) in map.iter_mut() {
            match entry.deadline {
                None => entry.deadline = Some(now + entry.timeout_ms.max(0.0) / 1000.0),
                Some(deadline) if now >= deadline => expired.push(token.clone()),
                Some(_) => {}
            }
        }
        for token in expired {
            if let Some(entry) = map.remove(&token) {
                let _ = entry.reject.call1(
                    &JsValue::NULL,
                    &query_error(crate::query::codes::TIMEOUT, "query timed out"),
                );
            }
        }
    });
}

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

    /// Sends an OSC 778 query and resolves with the decoded JSON payload.
    ///
    /// `data` is any JSON-serializable value (or `undefined`/`null` for
    /// ops without parameters); the envelope, correlation, decoding, and
    /// error mapping all live in Rust — JS never builds or parses 778
    /// frames. Failures reject with an `Error` whose `code` property is
    /// the stable wire code (`timeout`, `unsupported-op`, `disposed`, …).
    /// `query()` adds no authority: session identity, namespace scope,
    /// projection rules, and size limits apply exactly as on the wire.
    pub fn query(&self, op: &str, data: JsValue, timeout_ms: f64) -> Promise {
        // Bad arguments never reach the wire: a `;` would inject envelope
        // fields into the strict-ASCII envelope.
        if !crate::query::valid_op(op) {
            return Promise::reject(&query_error(
                crate::query::codes::BAD_PAYLOAD,
                "op must be non-empty printable ASCII without ';'",
            ));
        }
        let data_json = if data.is_undefined() || data.is_null() {
            None
        } else {
            match js_sys::JSON::stringify(&data)
                .ok()
                .and_then(|text| text.as_string())
            {
                Some(text) => Some(text),
                None => {
                    return Promise::reject(&query_error(
                        crate::query::codes::BAD_PAYLOAD,
                        "data is not JSON-serializable",
                    ));
                }
            }
        };

        let token = generate_token();
        // Locked semantics: a duplicate active token is an internal error
        // (unreachable with 128-bit random tokens, but defined).
        if PENDING_QUERIES.with(|map| map.borrow().contains_key(&token)) {
            return Promise::reject(&query_error(
                crate::query::codes::INTERNAL,
                "duplicate active token",
            ));
        }

        let sequence =
            crate::query::query_sequence(&token, op, data_json.as_deref().map(str::as_bytes));
        let feed = self.feed.clone();
        Promise::new(&mut |resolve, reject| {
            PENDING_QUERIES.with(|map| {
                map.borrow_mut().insert(
                    token.clone(),
                    PendingQuery {
                        resolve,
                        reject,
                        timeout_ms,
                        deadline: None,
                    },
                );
            });
            let _ = feed.send(sequence.clone().into_bytes());
        })
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

impl Drop for RattySession {
    /// Session disposal (`free()` from JS) rejects every outstanding query
    /// promise; nothing hangs unresolved.
    fn drop(&mut self) {
        PENDING_QUERIES.with(|map| {
            for (_, entry) in map.borrow_mut().drain() {
                let _ = entry.reject.call1(
                    &JsValue::NULL,
                    &query_error(crate::query::codes::DISPOSED, "session disposed"),
                );
            }
        });
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
    // Deterministic stage-writer order: apply_rgp_stage → apply_ai_commands →
    // drain_web_controls → apply_terminal_presentation. JS controls are the
    // most explicit user input, so they run last among the writers and are
    // read the same frame (before, not after, the presentation pass).
    .add_systems(
        Update,
        drain_web_controls
            .after(crate::ai::apply_ai_commands)
            .before(crate::scene::apply_terminal_presentation),
    )
    // Timeouts sweep after the reply writer so a reply arriving on the
    // deadline frame wins over its timeout.
    .add_systems(
        Update,
        expire_query_promises.after(crate::query_channel::answer_queries),
    );

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
    mut stage_tween: ResMut<StageTween>,
    mut redraw: ResMut<TerminalRedrawState>,
) {
    let pending = match queue.0.lock() {
        Ok(mut controls) => std::mem::take(&mut *controls),
        Err(_) => return,
    };

    // JS controls are user input: they win over any scripted stage tween.
    if (pending.mode.is_some() || pending.warp.is_some() || pending.view.is_some())
        && stage_tween.active
    {
        stage_tween.stop();
    }

    if let Some(mode) = pending.mode
        && apply_stage_mode_change(mode, &mut presentation, &view, &mut mobius)
    {
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
