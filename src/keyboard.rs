//! Keyboard input handling.

use bevy::ecs::system::SystemParam;
use bevy::ecs::world::FromWorld;
use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;
use bevy::window::{PrimaryWindow, Window};

#[cfg(not(target_arch = "wasm32"))]
use arboard::Clipboard;

use crate::config::{AppConfig, BindingAction, FontConfig, KeyBindingConfig};
use crate::focus::{FocusOrigin, FocusRequest, FocusedTerminal};
use crate::identity::{TerminalId, TerminalIdentity};
use crate::mouse::{TerminalSelection, encode_mouse_wheel};
use crate::runtime::TerminalRuntime;
use crate::scene::{
    MobiusTransition, StageTween, TerminalPlaneBackLayoutQuery, TerminalPlaneLayoutQuery,
    TerminalPlaneView, TerminalPlaneWarp, TerminalPresentation, TerminalPresentationMode,
    TerminalViewport, sync_terminal_layout,
};
use crate::terminal::{TerminalRedrawState, TerminalSurface, render_scale_for_window};

/// Clipboard bridge for terminal copy and paste.
///
/// On wasm there is no synchronous clipboard; copy/paste degrade to no-ops
/// (the async Web Clipboard API can be layered on by the embedder later).
pub struct TerminalClipboard {
    #[cfg(not(target_arch = "wasm32"))]
    clipboard: Option<Clipboard>,
}

impl FromWorld for TerminalClipboard {
    fn from_world(_world: &mut World) -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            clipboard: Clipboard::new().ok(),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl TerminalClipboard {
    fn copy(&mut self, text: &str) {
        let Some(clipboard) = self.clipboard.as_mut() else {
            warn!("clipboard unavailable for copy");
            return;
        };

        if let Err(error) = clipboard.set_text(text.to_owned()) {
            warn!("failed to copy terminal selection to clipboard: {error}");
        }
    }

    fn paste(&mut self) -> Option<String> {
        let clipboard = self.clipboard.as_mut()?;
        clipboard.get_text().ok()
    }
}

#[cfg(target_arch = "wasm32")]
impl TerminalClipboard {
    fn copy(&mut self, _text: &str) {
        warn!("clipboard copy is not supported in the web build yet");
    }

    fn paste(&mut self) -> Option<String> {
        None
    }
}

/// Resolved terminal key bindings.
#[derive(Resource)]
pub struct TerminalKeyBindings {
    bindings: Vec<KeyBinding>,
}

impl FromWorld for TerminalKeyBindings {
    fn from_world(world: &mut World) -> Self {
        let app_config = world.resource::<AppConfig>();
        let mut bindings = vec![
            KeyBinding::new(
                KeyCode::Enter,
                BindingModifiers {
                    control: true,
                    alt: true,
                    ..default()
                },
                BindingAction::Toggle3DMode,
            ),
            KeyBinding::new(
                KeyCode::KeyM,
                BindingModifiers {
                    control: true,
                    alt: true,
                    ..default()
                },
                BindingAction::ToggleMobiusMode,
            ),
            KeyBinding::new(
                KeyCode::PageUp,
                BindingModifiers {
                    alt: true,
                    ..default()
                },
                BindingAction::ScrollPageUp,
            ),
            KeyBinding::new(
                KeyCode::PageDown,
                BindingModifiers {
                    alt: true,
                    ..default()
                },
                BindingAction::ScrollPageDown,
            ),
            KeyBinding::new(
                KeyCode::ArrowUp,
                BindingModifiers {
                    alt: true,
                    ..default()
                },
                BindingAction::ScrollUp,
            ),
            KeyBinding::new(
                KeyCode::ArrowDown,
                BindingModifiers {
                    alt: true,
                    ..default()
                },
                BindingAction::ScrollDown,
            ),
            KeyBinding::new(
                KeyCode::ArrowUp,
                BindingModifiers {
                    control: true,
                    alt: true,
                    ..default()
                },
                BindingAction::IncreaseWarp,
            ),
            KeyBinding::new(
                KeyCode::ArrowDown,
                BindingModifiers {
                    control: true,
                    alt: true,
                    ..default()
                },
                BindingAction::DecreaseWarp,
            ),
            KeyBinding::new(
                KeyCode::KeyC,
                BindingModifiers {
                    control: true,
                    alt: true,
                    ..default()
                },
                BindingAction::Copy,
            ),
            KeyBinding::new(
                KeyCode::KeyV,
                BindingModifiers {
                    control: true,
                    alt: true,
                    ..default()
                },
                BindingAction::Paste,
            ),
            KeyBinding::new(
                KeyCode::Equal,
                BindingModifiers {
                    control: true,
                    ..default()
                },
                BindingAction::IncreaseFontSize,
            ),
            KeyBinding::new(
                KeyCode::NumpadAdd,
                BindingModifiers {
                    control: true,
                    ..default()
                },
                BindingAction::IncreaseFontSize,
            ),
            KeyBinding::new(
                KeyCode::Minus,
                BindingModifiers {
                    control: true,
                    ..default()
                },
                BindingAction::DecreaseFontSize,
            ),
            KeyBinding::new(
                KeyCode::NumpadSubtract,
                BindingModifiers {
                    control: true,
                    ..default()
                },
                BindingAction::DecreaseFontSize,
            ),
            KeyBinding::new(
                KeyCode::Digit0,
                BindingModifiers {
                    control: true,
                    alt: true,
                    ..default()
                },
                BindingAction::ResetFontSize,
            ),
            KeyBinding::new(
                KeyCode::Numpad0,
                BindingModifiers {
                    control: true,
                    alt: true,
                    ..default()
                },
                BindingAction::ResetFontSize,
            ),
            // The N-gate (#56 decision 10's rider): without this chord the
            // first N=2 has no way to focus terminal #2 at all — 3D cell
            // picking is M4.6, deliberately off the critical path.
            KeyBinding::new(
                KeyCode::Tab,
                BindingModifiers {
                    control: true,
                    alt: true,
                    ..default()
                },
                BindingAction::FocusCycle,
            ),
        ];

        for binding in &app_config.bindings.keys {
            let Some(binding) = KeyBinding::from_config(binding) else {
                warn!(
                    "ignoring invalid key binding: key={} with={}",
                    binding.key, binding.with
                );
                continue;
            };

            if let Some(index) = bindings
                .iter()
                .position(|existing| existing.same_trigger(&binding))
            {
                bindings.remove(index);
            }

            if binding.action != BindingAction::None {
                bindings.push(binding);
            }
        }

        Self { bindings }
    }
}

impl TerminalKeyBindings {
    fn action_for(&self, key_code: KeyCode, modifiers: BindingModifiers) -> Option<BindingAction> {
        self.bindings
            .iter()
            .filter(|binding| binding.key_code == key_code && binding.modifiers.matches(modifiers))
            .max_by_key(|binding| binding.modifiers.count())
            .map(|binding| binding.action)
    }
}

/// Keyboard modifier state.
#[derive(Default)]
pub struct TerminalKeyboard {
    pub(crate) ctrl_pressed: bool,
    pub(crate) left_alt_pressed: bool,
    pub(crate) right_alt_pressed: bool,
    pub(crate) shift_pressed: bool,
    pub(crate) super_pressed: bool,
}

impl TerminalKeyboard {
    fn modifiers(&self) -> BindingModifiers {
        BindingModifiers {
            control: self.ctrl_pressed,
            alt: self.left_alt_pressed,
            shift: self.shift_pressed,
            super_key: self.super_pressed,
        }
    }

    /// Translates a keyboard event into terminal input bytes.
    pub fn handle_event_with_modes(
        &mut self,
        event: &KeyboardInput,
        application_cursor: bool,
        kitty_keyboard_flags: u8,
        modify_other_keys: Option<u8>,
    ) -> Option<Vec<u8>> {
        match event.key_code {
            KeyCode::ControlLeft | KeyCode::ControlRight => {
                self.ctrl_pressed = event.state == ButtonState::Pressed;
                return None;
            }
            KeyCode::AltLeft => {
                self.left_alt_pressed = event.state == ButtonState::Pressed;
                return None;
            }
            KeyCode::AltRight => {
                self.right_alt_pressed = event.state == ButtonState::Pressed;
                return None;
            }
            KeyCode::ShiftLeft | KeyCode::ShiftRight => {
                self.shift_pressed = event.state == ButtonState::Pressed;
                return None;
            }
            KeyCode::SuperLeft | KeyCode::SuperRight => {
                self.super_pressed = event.state == ButtonState::Pressed;
                return None;
            }
            _ => {}
        }

        if event.state != ButtonState::Pressed {
            return None;
        }

        Some(translate_key(
            event.key_code,
            KeyTranslationContext {
                logical_key: &event.logical_key,
                text: event.text.as_deref(),
                ctrl_pressed: self.ctrl_pressed,
                alt_pressed: self.left_alt_pressed,
                alt_gr_pressed: self.right_alt_pressed,
                shift_pressed: self.shift_pressed,
                application_cursor,
                kitty_keyboard_flags,
                modify_other_keys,
            },
        ))
    }
}

/// Keyboard input system parameters.
#[derive(SystemParam)]
pub struct KeyboardSystemParams<'w, 's> {
    keys: Res<'w, ButtonInput<KeyCode>>,
    selection: ResMut<'w, TerminalSelection>,
    plane_warp: Query<'w, 's, &'static mut TerminalPlaneWarp>,
    plane_view: ResMut<'w, TerminalPlaneView>,
    presentation: ResMut<'w, TerminalPresentation>,
    mobius_transition: ResMut<'w, MobiusTransition>,
    stage_tween: ResMut<'w, StageTween>,
    clipboard: NonSendMut<'w, TerminalClipboard>,
    runtime: Query<'w, 's, &'static mut TerminalRuntime>,
    terminal: Query<'w, 's, &'static mut TerminalSurface>,
    primary_window: Query<'w, 's, &'static Window, With<PrimaryWindow>>,
    viewport: Query<'w, 's, &'static mut TerminalViewport>,
    plane_query: TerminalPlaneLayoutQuery<'w, 's>,
    plane_back_query: TerminalPlaneBackLayoutQuery<'w, 's>,
    bindings: Res<'w, TerminalKeyBindings>,
    redraw: Query<'w, 's, &'static mut TerminalRedrawState>,
    focus: Res<'w, FocusedTerminal>,
    focus_requests: MessageWriter<'w, FocusRequest>,
    seats: Query<'w, 's, (Entity, &'static TerminalIdentity)>,
    _marker: std::marker::PhantomData<&'s ()>,
}

/// Handles terminal keyboard input.
pub fn handle_keyboard_input(
    mut keyboard_events: MessageReader<KeyboardInput>,
    mut keyboard: Local<TerminalKeyboard>,
    mut params: KeyboardSystemParams,
) {
    // The routing split (#56 decision 7, invariant 4): terminal chords and
    // the byte path aim at the FOCUSED seat; scene chords write
    // scene-global state and keep working with zero terminals focused.
    // With no focus, byte input drops — there is nowhere honest to send
    // it, and no fallback-to-terminal-#1 resurrects the singleton
    // (invariant 8).
    let focused = params.focus.get();

    for event in keyboard_events.read() {
        let binding_key_code = navigation_key_code(&event.logical_key).unwrap_or(event.key_code);
        let modifiers = current_modifiers(&params.keys).union(keyboard.modifiers());
        if event.state == ButtonState::Pressed
            && let Some(action) = params.bindings.action_for(binding_key_code, modifiers)
        {
            if event.repeat
                && !matches!(
                    action,
                    BindingAction::IncreaseFontSize
                        | BindingAction::DecreaseFontSize
                        | BindingAction::ResetFontSize
                        | BindingAction::ScrollPageUp
                        | BindingAction::ScrollPageDown
                        | BindingAction::ScrollUp
                        | BindingAction::ScrollDown
                        | BindingAction::IncreaseWarp
                        | BindingAction::DecreaseWarp
                )
            {
                continue;
            }

            match action {
                BindingAction::None => {}
                BindingAction::Toggle3DMode => {
                    params.presentation.toggle_plane_mode();
                    params.mobius_transition.stop();
                    if params.stage_tween.active {
                        params.stage_tween.stop();
                    }
                    params.selection.clear();
                    // A mode flip restyles every seat's texture cursor.
                    for mut redraw in params.redraw.iter_mut() {
                        redraw.request();
                    }
                    continue;
                }
                BindingAction::ToggleMobiusMode => {
                    if params.stage_tween.active {
                        params.stage_tween.stop();
                    }
                    if params.presentation.mode == TerminalPresentationMode::Mobius3d {
                        let current_zoom = if params.mobius_transition.active {
                            params.mobius_transition.current_zoom()
                        } else {
                            params.plane_view.zoom
                        };
                        params
                            .mobius_transition
                            .begin_exit(&params.plane_view, current_zoom);
                    } else {
                        let previous_mode = params.presentation.mode;
                        params.presentation.toggle_mobius_mode();
                        params
                            .mobius_transition
                            .begin_enter(previous_mode, &params.plane_view);
                    }
                    params.selection.clear();
                    for mut redraw in params.redraw.iter_mut() {
                        redraw.request();
                    }
                    continue;
                }
                BindingAction::ScrollPageUp
                | BindingAction::ScrollPageDown
                | BindingAction::ScrollUp
                | BindingAction::ScrollDown => {
                    // A terminal chord: no focus, no-op.
                    let Some(focused) = focused else {
                        continue;
                    };
                    let amount = match action {
                        BindingAction::ScrollPageUp | BindingAction::ScrollPageDown => {
                            let Ok(terminal) = params.terminal.get_mut(focused) else {
                                continue;
                            };
                            usize::from(terminal.rows.saturating_sub(1).max(1))
                        }
                        BindingAction::ScrollUp | BindingAction::ScrollDown => 1,
                        _ => unreachable!(),
                    };
                    let direction = match action {
                        BindingAction::ScrollPageUp | BindingAction::ScrollUp => 1isize,
                        BindingAction::ScrollPageDown | BindingAction::ScrollDown => -1isize,
                        _ => unreachable!(),
                    };

                    let Ok(mut runtime) = params.runtime.get_mut(focused) else {
                        continue;
                    };
                    let mouse_mode = runtime.parser.screen().mouse_protocol_mode();
                    if params.presentation.mode == TerminalPresentationMode::Flat2d
                        && mouse_mode != vt100::MouseProtocolMode::None
                    {
                        let encoding = runtime.parser.screen().mouse_protocol_encoding();
                        let (row, col) = runtime.parser.screen().cursor_position();
                        let cell = UVec2::new(col as u32, row as u32);
                        for _ in 0..amount {
                            runtime.write_input(&encode_mouse_wheel(
                                cell,
                                direction.is_positive(),
                                encoding,
                            ));
                        }
                    } else {
                        let screen = runtime.parser.screen_mut();
                        let current = screen.scrollback();
                        let next = if direction.is_positive() {
                            current.saturating_add(amount)
                        } else {
                            current.saturating_sub(amount)
                        };
                        screen.set_scrollback(next);
                        params.selection.clear();
                        if let Ok(mut redraw) = params.redraw.get_mut(focused) {
                            redraw.request();
                        }
                    }
                    continue;
                }
                BindingAction::IncreaseWarp | BindingAction::DecreaseWarp => {
                    // Warp is the focused seat's surface shape (#52's
                    // per-terminal half): user input aimed at the terminal
                    // the user is addressing.
                    let Some(focused) = focused else {
                        continue;
                    };
                    let delta = if action == BindingAction::IncreaseWarp {
                        0.08
                    } else {
                        -0.08
                    };
                    if params.stage_tween.active {
                        params.stage_tween.stop();
                    }
                    let Ok(mut plane_warp) = params.plane_warp.get_mut(focused) else {
                        continue;
                    };
                    plane_warp.adjust(delta);
                    if let Ok(mut redraw) = params.redraw.get_mut(focused) {
                        redraw.request();
                    }
                    continue;
                }
                BindingAction::Copy => {
                    // Copy reads the FOCUSED terminal's selection against
                    // its own screen; one clipboard.
                    let Some(focused) = focused else {
                        continue;
                    };
                    let Ok(runtime) = params.runtime.get_mut(focused) else {
                        continue;
                    };
                    if let Some(text) = params.selection.selected_text(runtime.parser.screen())
                        && !text.is_empty()
                    {
                        params.clipboard.copy(&text);
                    }
                    if params.selection.clear()
                        && let Ok(mut redraw) = params.redraw.get_mut(focused)
                    {
                        redraw.request();
                    }
                    continue;
                }
                BindingAction::Paste => {
                    let Some(focused) = focused else {
                        continue;
                    };
                    let Ok(runtime) = params.runtime.get_mut(focused) else {
                        continue;
                    };
                    if let Some(text) = params.clipboard.paste() {
                        write_paste(&runtime, &text);
                    } else {
                        warn!("failed to read clipboard contents for paste");
                    }
                    if params.selection.clear()
                        && let Ok(mut redraw) = params.redraw.get_mut(focused)
                    {
                        redraw.request();
                    }
                    continue;
                }
                BindingAction::IncreaseFontSize
                | BindingAction::DecreaseFontSize
                | BindingAction::ResetFontSize => {
                    let Some(focused) = focused else {
                        continue;
                    };
                    let Ok(mut terminal) = params.terminal.get_mut(focused) else {
                        continue;
                    };
                    let resized = match action {
                        BindingAction::IncreaseFontSize => terminal.adjust_font_size(1),
                        BindingAction::DecreaseFontSize => terminal.adjust_font_size(-1),
                        BindingAction::ResetFontSize => {
                            let target = FontConfig::default().size;
                            let delta = target - terminal.font_size();
                            delta != 0 && terminal.adjust_font_size(delta)
                        }
                        _ => false,
                    };
                    if resized {
                        let Ok(window) = params.primary_window.single() else {
                            continue;
                        };
                        let layout = terminal.resize_to_fit(
                            window.resolution.size().max(Vec2::ONE),
                            render_scale_for_window(window),
                        );
                        let pty_pixels = layout.pty_pixels();
                        let Ok(mut runtime) = params.runtime.get_mut(focused) else {
                            continue;
                        };
                        runtime.resize(
                            layout.cols,
                            layout.rows,
                            pty_pixels.x as u16,
                            pty_pixels.y as u16,
                        );
                        let Ok(mut viewport) = params.viewport.get_mut(focused) else {
                            continue;
                        };
                        sync_terminal_layout(
                            focused,
                            layout,
                            &mut viewport,
                            &mut params.plane_query,
                            &mut params.plane_back_query,
                        );
                        if let Ok(mut redraw) = params.redraw.get_mut(focused) {
                            redraw.request();
                        }
                    }
                    continue;
                }
                BindingAction::FocusCycle => {
                    // The N-gate (#56 decision 10's rider): a user-class
                    // request at the least-recently-focused live seat.
                    // With one terminal (or none) the target is None and
                    // the chord is a no-op.
                    let live: Vec<(Entity, TerminalId)> = params
                        .seats
                        .iter()
                        .map(|(entity, identity)| (entity, identity.id()))
                        .collect();
                    if let Some(target) = params.focus.cycle_target(&live) {
                        params.focus_requests.write(FocusRequest {
                            target: Some(target),
                            origin: FocusOrigin::Keybinding,
                        });
                    }
                    continue;
                }
            }
        }

        // Typing clears the FOCUSED terminal's selection only; with no
        // focus nothing receives the keystroke, so nothing clears.
        if let Some(focused) = focused
            && event.state == ButtonState::Pressed
            && !is_modifier_key(binding_key_code)
            && params.selection.clear()
            && let Ok(mut redraw) = params.redraw.get_mut(focused)
        {
            redraw.request();
        }

        // Translation always runs — the Local's physical modifier state
        // (invariant 6) must never desync while no terminal is focused —
        // but the produced bytes only travel when a focused runtime
        // exists to receive them.
        let modes = focused.and_then(|focused| {
            params.runtime.get_mut(focused).ok().map(|runtime| {
                (
                    runtime.parser.screen().application_cursor(),
                    runtime.kitty_keyboard_flags(),
                    runtime.modify_other_keys(),
                )
            })
        });
        let (application_cursor, kitty_flags, modify_other_keys) =
            modes.unwrap_or((false, 0, None));
        if let Some(input) = keyboard.handle_event_with_modes(
            event,
            application_cursor,
            kitty_flags,
            modify_other_keys,
        ) && let Some(focused) = focused
            && let Ok(mut runtime) = params.runtime.get_mut(focused)
        {
            let screen = runtime.parser.screen_mut();
            if screen.scrollback() != 0 {
                screen.set_scrollback(0);
                if let Ok(mut redraw) = params.redraw.get_mut(focused) {
                    redraw.request();
                }
            }
            runtime.write_input(&input);
        }
    }
}

/// Writes clipboard text to the terminal, wrapping it in the
/// bracketed-paste sentinels only when the program has enabled the
/// mode (`CSI ? 2004 h`); otherwise the normalized bytes go bare.
fn write_paste(runtime: &TerminalRuntime, text: &str) {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if runtime.parser.screen().bracketed_paste() {
        let mut bytes = Vec::from(b"\x1b[200~".as_slice());
        bytes.extend_from_slice(normalized.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        runtime.write_input(&bytes);
    } else {
        runtime.write_input(normalized.as_bytes());
    }
}

fn current_modifiers(keys: &ButtonInput<KeyCode>) -> BindingModifiers {
    BindingModifiers {
        control: keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight]),
        alt: keys.pressed(KeyCode::AltLeft),
        shift: keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]),
        super_key: keys.any_pressed([KeyCode::SuperLeft, KeyCode::SuperRight]),
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct BindingModifiers {
    control: bool,
    alt: bool,
    shift: bool,
    super_key: bool,
}

impl BindingModifiers {
    fn matches(self, current: Self) -> bool {
        (!self.control || current.control)
            && (!self.alt || current.alt)
            && (!self.shift || current.shift)
            && (!self.super_key || current.super_key)
    }

    fn union(self, other: Self) -> Self {
        Self {
            control: self.control || other.control,
            alt: self.alt || other.alt,
            shift: self.shift || other.shift,
            super_key: self.super_key || other.super_key,
        }
    }

    fn count(self) -> usize {
        self.control as usize + self.alt as usize + self.shift as usize + self.super_key as usize
    }
}

#[derive(Clone, Copy)]
struct KeyBinding {
    key_code: KeyCode,
    modifiers: BindingModifiers,
    action: BindingAction,
}

impl KeyBinding {
    fn new(key_code: KeyCode, modifiers: BindingModifiers, action: BindingAction) -> Self {
        Self {
            key_code,
            modifiers,
            action,
        }
    }

    fn from_config(config: &KeyBindingConfig) -> Option<Self> {
        let mut modifiers = BindingModifiers::default();
        let mut key_code = None;

        for token in config
            .key
            .split('|')
            .chain(config.with.split('|'))
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            if let Some(modifier) = parse_modifier(token) {
                modifier.apply(&mut modifiers);
                continue;
            }

            if key_code.is_some() {
                return None;
            }

            key_code = parse_key_code(token);
        }

        Some(Self::new(key_code?, modifiers, config.action))
    }

    fn same_trigger(&self, other: &Self) -> bool {
        self.key_code == other.key_code && self.modifiers == other.modifiers
    }
}

#[derive(Clone, Copy)]
enum ParsedModifier {
    Control,
    Alt,
    Shift,
    Super,
}

impl ParsedModifier {
    fn apply(self, modifiers: &mut BindingModifiers) {
        match self {
            Self::Control => modifiers.control = true,
            Self::Alt => modifiers.alt = true,
            Self::Shift => modifiers.shift = true,
            Self::Super => modifiers.super_key = true,
        }
    }
}

struct KeyTranslationContext<'a> {
    logical_key: &'a Key,
    text: Option<&'a str>,
    ctrl_pressed: bool,
    alt_pressed: bool,
    alt_gr_pressed: bool,
    shift_pressed: bool,
    application_cursor: bool,
    kitty_keyboard_flags: u8,
    modify_other_keys: Option<u8>,
}

fn translate_key(key_code: KeyCode, ctx: KeyTranslationContext<'_>) -> Vec<u8> {
    let mut bytes = Vec::new();

    if ctx.alt_gr_pressed
        && let Some(text) = printable_text(ctx.text, ctx.logical_key)
    {
        bytes.extend_from_slice(text.as_bytes());
        return bytes;
    }

    if ctx.ctrl_pressed
        && let Some(ctrl) = ctrl_keycode_byte(key_code)
    {
        if ctx.alt_pressed {
            bytes.push(0x1b);
        }
        bytes.push(ctrl);
        return bytes;
    }

    // Kitty flag bit 0 requests disambiguated escape codes, which gives us an unambiguous
    // encoding for modified special keys such as Ctrl+Enter.
    let kitty_disambiguate = ctx.kitty_keyboard_flags & 1 != 0;
    if let Some(sequence) = encode_modified_special_key(
        key_code,
        ctx.ctrl_pressed,
        ctx.alt_pressed,
        ctx.shift_pressed,
        kitty_disambiguate,
        ctx.modify_other_keys,
    ) {
        bytes.extend_from_slice(&sequence);
        return bytes;
    }

    if ctx.alt_pressed {
        bytes.push(0x1b);
    }

    let navigation_key = NavigationKey::from_key_code(key_code)
        .or_else(|| NavigationKey::from_logical_key(ctx.logical_key));
    if let Some(key) = navigation_key {
        bytes.extend_from_slice(&key.encode(ctx.ctrl_pressed, ctx.application_cursor));
        return bytes;
    }

    match key_code {
        KeyCode::Enter | KeyCode::NumpadEnter => bytes.push(b'\r'),
        KeyCode::Tab => bytes.push(b'\t'),
        KeyCode::Space => bytes.push(b' '),
        KeyCode::Backspace => bytes.push(0x7f),
        KeyCode::Escape => bytes.push(0x1b),
        _ => {
            if let Some(text) = printable_text(ctx.text, ctx.logical_key) {
                bytes.extend_from_slice(text.as_bytes());
            }
        }
    }

    bytes
}

/// Determine the text to send for a key event.
fn printable_text<'a>(text: Option<&'a str>, logical_key: &'a Key) -> Option<&'a str> {
    text.or_else(|| match logical_key {
        Key::Character(chars) => Some(chars.as_str()),
        _ => None,
    })
    .filter(|text| !text.is_empty())
}

#[derive(Clone, Copy)]
enum NavigationKey {
    ArrowUp,
    ArrowDown,
    ArrowRight,
    ArrowLeft,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
}

impl NavigationKey {
    fn from_key_code(key_code: KeyCode) -> Option<Self> {
        match key_code {
            KeyCode::ArrowUp => Some(Self::ArrowUp),
            KeyCode::ArrowDown => Some(Self::ArrowDown),
            KeyCode::ArrowRight => Some(Self::ArrowRight),
            KeyCode::ArrowLeft => Some(Self::ArrowLeft),
            KeyCode::Home => Some(Self::Home),
            KeyCode::End => Some(Self::End),
            KeyCode::PageUp => Some(Self::PageUp),
            KeyCode::PageDown => Some(Self::PageDown),
            KeyCode::Insert => Some(Self::Insert),
            KeyCode::Delete => Some(Self::Delete),
            _ => None,
        }
    }

    fn from_logical_key(logical_key: &Key) -> Option<Self> {
        // Keypad navigation with numlock disabled often arrives as a Numpad physical key paired
        // with a logical navigation key such as Home or PageUp. Use the logical meaning so keypad
        // navigation behaves like the dedicated navigation cluster.
        match logical_key {
            Key::ArrowUp => Some(Self::ArrowUp),
            Key::ArrowDown => Some(Self::ArrowDown),
            Key::ArrowRight => Some(Self::ArrowRight),
            Key::ArrowLeft => Some(Self::ArrowLeft),
            Key::Home => Some(Self::Home),
            Key::End => Some(Self::End),
            Key::PageUp => Some(Self::PageUp),
            Key::PageDown => Some(Self::PageDown),
            Key::Insert => Some(Self::Insert),
            Key::Delete => Some(Self::Delete),
            _ => None,
        }
    }

    fn encode(self, ctrl_pressed: bool, application_cursor: bool) -> Vec<u8> {
        match self {
            Self::ArrowUp => {
                if ctrl_pressed {
                    b"\x1b[1;5A".to_vec()
                } else if application_cursor {
                    b"\x1bOA".to_vec()
                } else {
                    b"\x1b[A".to_vec()
                }
            }
            Self::ArrowDown => {
                if ctrl_pressed {
                    b"\x1b[1;5B".to_vec()
                } else if application_cursor {
                    b"\x1bOB".to_vec()
                } else {
                    b"\x1b[B".to_vec()
                }
            }
            Self::ArrowRight => {
                if ctrl_pressed {
                    b"\x1b[1;5C".to_vec()
                } else if application_cursor {
                    b"\x1bOC".to_vec()
                } else {
                    b"\x1b[C".to_vec()
                }
            }
            Self::ArrowLeft => {
                if ctrl_pressed {
                    b"\x1b[1;5D".to_vec()
                } else if application_cursor {
                    b"\x1bOD".to_vec()
                } else {
                    b"\x1b[D".to_vec()
                }
            }
            Self::Home => {
                if application_cursor {
                    b"\x1bOH".to_vec()
                } else {
                    b"\x1b[1~".to_vec()
                }
            }
            Self::End => {
                if application_cursor {
                    b"\x1bOF".to_vec()
                } else {
                    b"\x1b[4~".to_vec()
                }
            }
            Self::PageUp => b"\x1b[5~".to_vec(),
            Self::PageDown => b"\x1b[6~".to_vec(),
            Self::Insert => b"\x1b[2~".to_vec(),
            Self::Delete => b"\x1b[3~".to_vec(),
        }
    }
}

fn navigation_key_code(logical_key: &Key) -> Option<KeyCode> {
    match logical_key {
        Key::ArrowUp => Some(KeyCode::ArrowUp),
        Key::ArrowDown => Some(KeyCode::ArrowDown),
        Key::ArrowRight => Some(KeyCode::ArrowRight),
        Key::ArrowLeft => Some(KeyCode::ArrowLeft),
        Key::Home => Some(KeyCode::Home),
        Key::End => Some(KeyCode::End),
        Key::PageUp => Some(KeyCode::PageUp),
        Key::PageDown => Some(KeyCode::PageDown),
        Key::Insert => Some(KeyCode::Insert),
        Key::Delete => Some(KeyCode::Delete),
        _ => None,
    }
}

fn encode_modified_special_key(
    key_code: KeyCode,
    ctrl_pressed: bool,
    alt_pressed: bool,
    shift_pressed: bool,
    kitty_disambiguate: bool,
    modify_other_keys: Option<u8>,
) -> Option<Vec<u8>> {
    let codepoint = match key_code {
        KeyCode::Enter | KeyCode::NumpadEnter => 13,
        KeyCode::Tab => 9,
        KeyCode::Backspace => 127,
        KeyCode::Escape => 27,
        _ => return None,
    };

    if !ctrl_pressed && !alt_pressed && !shift_pressed {
        return None;
    }

    let modifier_code =
        1 + shift_pressed as u8 + (alt_pressed as u8 * 2) + (ctrl_pressed as u8 * 4);

    // Kitty keyboard protocol uses CSI codepoint ; modifiers u for modified special keys.
    if kitty_disambiguate {
        return Some(format!("\x1b[{};{}u", codepoint, modifier_code).into_bytes());
    }

    // xterm modifyOtherKeys falls back to CSI 27 ; modifiers ; codepoint ~ for the same class of
    // keys when the foreground app explicitly enabled that mode.
    if modify_other_keys.is_some() {
        return Some(format!("\x1b[27;{};{}~", modifier_code, codepoint).into_bytes());
    }

    None
}

fn is_modifier_key(key: KeyCode) -> bool {
    matches!(
        key,
        KeyCode::ControlLeft
            | KeyCode::ControlRight
            | KeyCode::AltLeft
            | KeyCode::AltRight
            | KeyCode::ShiftLeft
            | KeyCode::ShiftRight
            | KeyCode::SuperLeft
            | KeyCode::SuperRight
    )
}

fn parse_key_code(key: &str) -> Option<KeyCode> {
    match key.trim().to_ascii_lowercase().as_str() {
        "a" => Some(KeyCode::KeyA),
        "b" => Some(KeyCode::KeyB),
        "c" => Some(KeyCode::KeyC),
        "d" => Some(KeyCode::KeyD),
        "e" => Some(KeyCode::KeyE),
        "f" => Some(KeyCode::KeyF),
        "g" => Some(KeyCode::KeyG),
        "h" => Some(KeyCode::KeyH),
        "i" => Some(KeyCode::KeyI),
        "j" => Some(KeyCode::KeyJ),
        "k" => Some(KeyCode::KeyK),
        "l" => Some(KeyCode::KeyL),
        "m" => Some(KeyCode::KeyM),
        "n" => Some(KeyCode::KeyN),
        "o" => Some(KeyCode::KeyO),
        "p" => Some(KeyCode::KeyP),
        "q" => Some(KeyCode::KeyQ),
        "r" => Some(KeyCode::KeyR),
        "s" => Some(KeyCode::KeyS),
        "t" => Some(KeyCode::KeyT),
        "u" => Some(KeyCode::KeyU),
        "v" => Some(KeyCode::KeyV),
        "w" => Some(KeyCode::KeyW),
        "x" => Some(KeyCode::KeyX),
        "y" => Some(KeyCode::KeyY),
        "z" => Some(KeyCode::KeyZ),
        "0" => Some(KeyCode::Digit0),
        "1" => Some(KeyCode::Digit1),
        "2" => Some(KeyCode::Digit2),
        "3" => Some(KeyCode::Digit3),
        "4" => Some(KeyCode::Digit4),
        "5" => Some(KeyCode::Digit5),
        "6" => Some(KeyCode::Digit6),
        "7" => Some(KeyCode::Digit7),
        "8" => Some(KeyCode::Digit8),
        "9" => Some(KeyCode::Digit9),
        "f1" => Some(KeyCode::F1),
        "f2" => Some(KeyCode::F2),
        "f3" => Some(KeyCode::F3),
        "f4" => Some(KeyCode::F4),
        "f5" => Some(KeyCode::F5),
        "f6" => Some(KeyCode::F6),
        "f7" => Some(KeyCode::F7),
        "f8" => Some(KeyCode::F8),
        "f9" => Some(KeyCode::F9),
        "f10" => Some(KeyCode::F10),
        "f11" => Some(KeyCode::F11),
        "f12" => Some(KeyCode::F12),
        "up" => Some(KeyCode::ArrowUp),
        "down" => Some(KeyCode::ArrowDown),
        "left" => Some(KeyCode::ArrowLeft),
        "right" => Some(KeyCode::ArrowRight),
        "enter" => Some(KeyCode::Enter),
        "tab" => Some(KeyCode::Tab),
        "space" => Some(KeyCode::Space),
        "backspace" => Some(KeyCode::Backspace),
        "escape" | "esc" => Some(KeyCode::Escape),
        "delete" => Some(KeyCode::Delete),
        "home" => Some(KeyCode::Home),
        "end" => Some(KeyCode::End),
        "pageup" | "page_up" => Some(KeyCode::PageUp),
        "pagedown" | "page_down" => Some(KeyCode::PageDown),
        "equal" | "=" | "plus" | "+" => Some(KeyCode::Equal),
        "minus" | "-" => Some(KeyCode::Minus),
        "numpadadd" | "numpad_add" => Some(KeyCode::NumpadAdd),
        "numpadsubtract" | "numpad_subtract" => Some(KeyCode::NumpadSubtract),
        _ => None,
    }
}

fn parse_modifier(token: &str) -> Option<ParsedModifier> {
    match token.trim().to_ascii_lowercase().as_str() {
        "control" | "ctrl" => Some(ParsedModifier::Control),
        "alt" => Some(ParsedModifier::Alt),
        "shift" => Some(ParsedModifier::Shift),
        "super" | "cmd" | "command" | "meta" => Some(ParsedModifier::Super),
        _ => None,
    }
}

fn ctrl_keycode_byte(key: KeyCode) -> Option<u8> {
    match key {
        KeyCode::KeyA => Some(0x01),
        KeyCode::KeyB => Some(0x02),
        KeyCode::KeyC => Some(0x03),
        KeyCode::KeyD => Some(0x04),
        KeyCode::KeyE => Some(0x05),
        KeyCode::KeyF => Some(0x06),
        KeyCode::KeyG => Some(0x07),
        KeyCode::KeyH => Some(0x08),
        KeyCode::KeyI => Some(0x09),
        KeyCode::KeyJ => Some(0x0a),
        KeyCode::KeyK => Some(0x0b),
        KeyCode::KeyL => Some(0x0c),
        KeyCode::KeyM => Some(0x0d),
        KeyCode::KeyN => Some(0x0e),
        KeyCode::KeyO => Some(0x0f),
        KeyCode::KeyP => Some(0x10),
        KeyCode::KeyQ => Some(0x11),
        KeyCode::KeyR => Some(0x12),
        KeyCode::KeyS => Some(0x13),
        KeyCode::KeyT => Some(0x14),
        KeyCode::KeyU => Some(0x15),
        KeyCode::KeyV => Some(0x16),
        KeyCode::KeyW => Some(0x17),
        KeyCode::KeyX => Some(0x18),
        KeyCode::KeyY => Some(0x19),
        KeyCode::KeyZ => Some(0x1a),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_without_bracketed_mode_sends_bare_bytes() {
        let config = AppConfig::default();
        let (runtime, host) =
            TerminalRuntime::virtual_channel(&config, crate::runtime::IngressSource::test_boot());
        write_paste(&runtime, "hi\r\nthere\r");
        assert_eq!(
            host.input_rx
                .try_recv()
                .expect("paste should reach the host"),
            b"hi\nthere\n".to_vec()
        );
    }

    #[test]
    fn paste_with_bracketed_mode_wraps_in_sentinels() {
        let config = AppConfig::default();
        let (mut runtime, host) =
            TerminalRuntime::virtual_channel(&config, crate::runtime::IngressSource::test_boot());
        runtime.parser.process(b"\x1b[?2004h");
        write_paste(&runtime, "hi");
        assert_eq!(
            host.input_rx
                .try_recv()
                .expect("paste should reach the host"),
            b"\x1b[200~hi\x1b[201~".to_vec()
        );
    }
}

#[cfg(test)]
mod routing_tests {
    use bevy::ecs::message::Messages;
    use bevy::ecs::system::RunSystemOnce;
    use bevy::input::keyboard::Key;

    use super::*;
    use crate::focus::{FocusGained, FocusLost, drain_focus_requests};
    use crate::identity::TerminalRegistry;
    use crate::runtime::VirtualTerminalHost;
    use crate::scene::TerminalPresentation;

    /// A world with everything `handle_keyboard_input` and the focus
    /// drain need, plus two virtual terminal seats. Seats deliberately
    /// carry no surface/viewport/warp — the typing, cycle and scene-chord
    /// paths under test never touch them.
    fn routing_world() -> (
        World,
        (Entity, VirtualTerminalHost),
        (Entity, VirtualTerminalHost),
    ) {
        let mut world = World::new();
        world.insert_resource(AppConfig::default());
        let bindings = TerminalKeyBindings::from_world(&mut world);
        world.insert_resource(bindings);
        let clipboard = TerminalClipboard::from_world(&mut world);
        world.insert_non_send(clipboard);
        world.init_resource::<TerminalSelection>();
        world.init_resource::<ButtonInput<KeyCode>>();
        world.insert_resource(TerminalPresentation {
            mode: TerminalPresentationMode::Flat2d,
        });
        world.insert_resource(TerminalPlaneView::default());
        world.insert_resource(MobiusTransition::default());
        world.insert_resource(StageTween::default());
        world.init_resource::<FocusedTerminal>();
        world.init_resource::<Messages<KeyboardInput>>();
        world.init_resource::<Messages<FocusRequest>>();
        world.init_resource::<Messages<FocusGained>>();
        world.init_resource::<Messages<FocusLost>>();

        let mut registry = TerminalRegistry::default();
        let seat = |world: &mut World, registry: &mut TerminalRegistry| {
            let identity = registry.allocate().expect("test lease");
            let (runtime, host) =
                TerminalRuntime::virtual_channel(&AppConfig::default(), identity.ingress());
            let entity = world
                .spawn((identity, runtime, TerminalRedrawState::default()))
                .id();
            (entity, host)
        };
        let seat_a = seat(&mut world, &mut registry);
        let seat_b = seat(&mut world, &mut registry);
        (world, seat_a, seat_b)
    }

    fn focus(world: &mut World, target: Entity) {
        world
            .resource_mut::<Messages<FocusRequest>>()
            .write(FocusRequest {
                target: Some(target),
                origin: FocusOrigin::SpawnPolicy,
            });
        world
            .run_system_once(drain_focus_requests)
            .expect("drain runs");
        // run_system_once never advances message buffers; drop the applied
        // request so later assertions see only new emissions.
        world.resource_mut::<Messages<FocusRequest>>().clear();
    }

    fn type_key(world: &mut World, key_code: KeyCode, logical_key: Key, text: Option<&str>) {
        world
            .resource_mut::<Messages<KeyboardInput>>()
            .write(KeyboardInput {
                key_code,
                logical_key,
                state: ButtonState::Pressed,
                text: text.map(Into::into),
                repeat: false,
                window: Entity::PLACEHOLDER,
            });
        world
            .run_system_once(handle_keyboard_input)
            .expect("keyboard handler runs");
        // run_system_once never advances message buffers; drop the consumed
        // event so the next call cannot re-read it.
        world.resource_mut::<Messages<KeyboardInput>>().clear();
    }

    #[test]
    fn typing_follows_focus() {
        let (mut world, (seat_a, host_a), (seat_b, host_b)) = routing_world();
        assert_eq!(
            world.query::<&TerminalIdentity>().iter(&world).count(),
            2,
            "seat count asserted (#58 rider)"
        );

        focus(&mut world, seat_a);
        type_key(
            &mut world,
            KeyCode::KeyA,
            Key::Character("a".into()),
            Some("a"),
        );
        assert_eq!(
            host_a.input_rx.try_recv().expect("A receives its byte"),
            b"a".to_vec()
        );
        assert!(
            host_b.input_rx.try_recv().is_err(),
            "typing into A must not reach B"
        );

        focus(&mut world, seat_b);
        type_key(
            &mut world,
            KeyCode::KeyB,
            Key::Character("b".into()),
            Some("b"),
        );
        assert_eq!(
            host_b.input_rx.try_recv().expect("B receives its byte"),
            b"b".to_vec()
        );
        assert!(
            host_a.input_rx.try_recv().is_err(),
            "the byte path re-targets with focus"
        );
    }

    #[test]
    fn the_focus_cycle_chord_requests_the_lru_seat_and_sends_no_bytes() {
        let (mut world, (seat_a, host_a), (seat_b, host_b)) = routing_world();
        focus(&mut world, seat_a);

        // Hold Ctrl+Alt physically, press Tab.
        {
            let mut keys = world.resource_mut::<ButtonInput<KeyCode>>();
            keys.press(KeyCode::ControlLeft);
            keys.press(KeyCode::AltLeft);
        }
        type_key(&mut world, KeyCode::Tab, Key::Tab, None);

        let requests: Vec<FocusRequest> = world
            .resource_mut::<Messages<FocusRequest>>()
            .drain()
            .collect();
        assert_eq!(
            requests,
            vec![FocusRequest {
                target: Some(seat_b),
                origin: FocusOrigin::Keybinding,
            }],
            "the chord emits one user-class request at the LRU seat"
        );
        assert!(
            host_a.input_rx.try_recv().is_err() && host_b.input_rx.try_recv().is_err(),
            "a consumed chord never leaks bytes to any terminal"
        );
    }

    #[test]
    fn no_focus_drops_bytes_but_scene_chords_still_work() {
        let (mut world, (_, host_a), (_, host_b)) = routing_world();

        // Invariant 8: nothing is focused at this point, deliberately.
        type_key(
            &mut world,
            KeyCode::KeyX,
            Key::Character("x".into()),
            Some("x"),
        );
        assert!(
            host_a.input_rx.try_recv().is_err() && host_b.input_rx.try_recv().is_err(),
            "with no focus, byte input drops — no fallback-to-terminal-#1"
        );

        // Scene chords are user input into scene-global state and keep
        // working with zero terminals focused: Ctrl+Alt+Enter toggles 3D.
        {
            let mut keys = world.resource_mut::<ButtonInput<KeyCode>>();
            keys.press(KeyCode::ControlLeft);
            keys.press(KeyCode::AltLeft);
        }
        type_key(&mut world, KeyCode::Enter, Key::Enter, None);
        assert_eq!(
            world.resource::<TerminalPresentation>().mode,
            TerminalPresentationMode::Plane3d,
            "scene chords survive zero focus"
        );
    }
}
