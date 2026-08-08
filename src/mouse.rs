//! Mouse input handling and selection state.
//!
//! Since M4.6 the pointer routes through `pick_cell` (#56 decision 10):
//! hit-vs-miss is the discriminator between content interaction and
//! camera interaction. A press whose pick HITS a terminal focuses it
//! (focus-follows-click) and still delivers (click-through); a press that
//! MISSES every terminal is a camera gesture and never a focus change
//! (invariant 8: no focus from a missing pick). Capture beats hover beats
//! focus (invariant 7): a press capture pins its drag to one terminal
//! through the infinite-plane clamp; the wheel targets the hovered
//! terminal; neither moves focus.

use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::system::SystemParam;
use bevy::input::ButtonState;
use bevy::input::mouse::{MouseButton, MouseButtonInput, MouseScrollUnit, MouseWheel};
use bevy::prelude::*;
use bevy::window::{CursorMoved, PrimaryWindow, Window};
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

use crate::focus::{FocusOrigin, FocusRequest, FocusedTerminal};
use crate::pick::{PickParams, cell_from_uv};
use crate::runtime::TerminalRuntime;
use crate::scene::{
    MobiusTransition, StageTween, TerminalPlaneView, TerminalPresentation,
    TerminalPresentationMode, TerminalViewport,
};
use crate::terminal::TerminalSurface;

/// Distance in pixels the pointer must move with a pending selection to start dragging.
const SELECTION_DRAG_THRESHOLD: f32 = 4.0;

/// Active terminal text selection.
///
/// One selection exists screen-wide (scene composition is #42's return),
/// but it BELONGS to the terminal it was made on (#56 decision 11:
/// selections survive focus loss — on their own terminal): `owner`
/// records the seat the press-capture started on — since M4.6 the PICKED
/// seat, wherever focus sits — and every consumer (render, copy,
/// typing-clear) attributes through it. `None` means unowned (a fresh
/// state, or a test-made selection) and falls back to focused-only.
#[derive(Resource, Clone, Default)]
pub struct TerminalSelection {
    start: Option<UVec2>,
    end: Option<UVec2>,
    pending_start: Option<UVec2>,
    pending_position: Option<Vec2>,
    dragging: bool,
    cursor_position: Option<Vec2>,
    owner: Option<Entity>,
}

/// Per-seat mouse-protocol forwarding state (#56 decision 3's promotion
/// from a screen-global `Local`): press capture is KEYED by the
/// press-target seat — a press stamps the capture, motion and release
/// route to the captured terminal's writer and encoding regardless of
/// what the pointer crosses, and releasing the last button clears it.
/// The dedupe cell rides in the keyed state, so hover-motion dedupe never
/// leaks between terminals either.
#[derive(Component, Default)]
pub(crate) struct ForwardedMouseState {
    left_pressed: bool,
    middle_pressed: bool,
    right_pressed: bool,
    last_cell: Option<UVec2>,
}

impl ForwardedMouseState {
    fn any_pressed(&self) -> bool {
        self.left_pressed || self.middle_pressed || self.right_pressed
    }

    fn pressed_mut(&mut self, button: MouseButton) -> Option<&mut bool> {
        match button {
            MouseButton::Left => Some(&mut self.left_pressed),
            MouseButton::Middle => Some(&mut self.middle_pressed),
            MouseButton::Right => Some(&mut self.right_pressed),
            _ => None,
        }
    }

    /// The motion button code: the held button, or 3 (none) for pure
    /// hover motion — both offset by 32 per the protocol.
    fn motion_code(&self) -> u16 {
        if self.left_pressed {
            32
        } else if self.middle_pressed {
            33
        } else if self.right_pressed {
            34
        } else {
            35
        }
    }
}

/// Per-seat wheel remainder (#56 decision 3's promotion from a
/// screen-global `Local`): the pixel remainder is denominated in the
/// seat's own char height, so it must never leak between terminals with
/// different font metrics.
#[derive(Component, Default)]
pub(crate) struct LocalScrollState {
    pixel_remainder: f32,
}

#[cfg(test)]
impl LocalScrollState {
    pub(crate) fn remainder(&self) -> f32 {
        self.pixel_remainder
    }
}

/// Normalized selection bounds.
#[derive(Copy, Clone)]
pub struct SelectionBounds {
    /// First selected row.
    pub start_row: u32,
    /// Last selected row.
    pub end_row: u32,
    /// First selected column.
    pub start_col: u32,
    /// Last selected column.
    pub end_col: u32,
}

impl SelectionBounds {
    /// Returns whether a cell is inside the bounds.
    pub fn contains(&self, row: u16, col: u16) -> bool {
        let row = row as u32;
        let col = col as u32;

        if row < self.start_row || row > self.end_row {
            return false;
        }

        if self.start_row == self.end_row {
            return col >= self.start_col && col <= self.end_col;
        }

        if row == self.start_row {
            return col >= self.start_col;
        }

        if row == self.end_row {
            return col <= self.end_col;
        }

        true
    }
}

impl TerminalSelection {
    /// Returns normalized selection bounds.
    pub fn normalized_bounds(&self) -> Option<SelectionBounds> {
        let start = self.start?;
        let end = self.end.unwrap_or(start);
        Some(SelectionBounds {
            start_row: start.y.min(end.y),
            end_row: start.y.max(end.y),
            start_col: start.x.min(end.x),
            end_col: start.x.max(end.x),
        })
    }

    /// Starts a selection at a cell.
    pub fn begin(&mut self, cell: UVec2) -> bool {
        let changed = self.start != Some(cell) || self.end != Some(cell) || !self.dragging;
        self.start = Some(cell);
        self.end = Some(cell);
        self.pending_start = None;
        self.pending_position = None;
        self.dragging = true;
        changed
    }

    /// Arms a selection at a cell without making it visible until the pointer is dragged.
    pub fn begin_pending(&mut self, cell: UVec2, position: Vec2) -> bool {
        let changed = self.start.is_some() || self.end.is_some() || self.dragging;
        self.start = None;
        self.end = None;
        self.pending_start = Some(cell);
        self.pending_position = Some(position);
        self.dragging = false;
        changed
    }

    /// Updates the selection end cell.
    pub fn update(&mut self, cell: UVec2) -> bool {
        if self.dragging && self.end != Some(cell) {
            self.end = Some(cell);
            return true;
        }
        false
    }

    /// Updates the selection from a pointer position.
    pub fn update_from_cursor(&mut self, cell: UVec2, position: Vec2) -> bool {
        if self.dragging {
            return self.update(cell);
        }

        let Some(start) = self.pending_start else {
            return false;
        };
        let Some(origin) = self.pending_position else {
            return false;
        };

        if position.distance(origin) < SELECTION_DRAG_THRESHOLD {
            return false;
        }

        self.start = Some(start);
        self.end = Some(cell);
        self.pending_start = None;
        self.pending_position = None;
        self.dragging = true;
        true
    }

    /// Ends an in-progress selection.
    pub fn end(&mut self) -> bool {
        let changed = self.dragging;
        self.pending_start = None;
        self.pending_position = None;
        self.dragging = false;
        changed
    }

    /// Clears the selection.
    pub fn clear(&mut self) -> bool {
        let changed = self.start.is_some()
            || self.end.is_some()
            || self.pending_start.is_some()
            || self.pending_position.is_some()
            || self.dragging;
        self.start = None;
        self.end = None;
        self.pending_start = None;
        self.pending_position = None;
        self.dragging = false;
        self.cursor_position = None;
        self.owner = None;
        changed
    }

    /// Stamps the seat this selection belongs to (the press-capture
    /// terminal); set where the drag begins.
    pub(crate) fn set_owner(&mut self, owner: Entity) {
        self.owner = Some(owner);
    }

    /// Whether `seat` may treat this selection as its own: it owns it, or
    /// the selection is unowned (fresh, or test-made) and falls back to
    /// whoever asks. A selection owned elsewhere is standing state — it
    /// must neither render on, be copied from, nor be cleared by another
    /// terminal (#56 decision 11).
    pub(crate) fn owned_by_or_unowned(&self, seat: Entity) -> bool {
        self.owner.is_none_or(|owner| owner == seat)
    }

    /// Whether a press capture (armed or dragging) is in flight.
    fn capturing(&self) -> bool {
        self.dragging || self.pending_start.is_some()
    }

    /// Stores the current pointer position.
    pub fn set_cursor_position(&mut self, position: Vec2) {
        self.cursor_position = Some(position);
    }

    /// Returns the current pointer position.
    pub fn cursor_position(&self) -> Option<Vec2> {
        self.cursor_position
    }

    /// Returns the selected screen text.
    pub fn selected_text(&self, screen: &vt100::Screen) -> Option<String> {
        let bounds = self.normalized_bounds()?;

        let (_, cols) = screen.size();
        let mut out = String::new();

        let start_row = bounds.start_row as u16;
        let end_row = bounds.end_row as u16;
        let start_col = bounds.start_col as u16;
        let end_col = bounds.end_col as u16;

        for row in start_row..=end_row {
            let row_start = if row == start_row { start_col } else { 0 };
            let row_end = if row == end_row {
                end_col.min(cols.saturating_sub(1))
            } else {
                cols.saturating_sub(1)
            };

            for col in row_start..=row_end {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }

                let symbol = if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                };
                out.push_str(symbol);
            }

            if row != end_row {
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push('\n');
            }
        }

        Some(out)
    }
}

/// Mouse input system parameters.
#[derive(SystemParam)]
pub struct MouseSystemParams<'w, 's> {
    primary_window: Query<'w, 's, (Entity, &'static Window), With<PrimaryWindow>>,
    focus: Res<'w, FocusedTerminal>,
    focus_requests: MessageWriter<'w, FocusRequest>,
    runtime: Query<'w, 's, &'static mut TerminalRuntime>,
    terminal: Query<'w, 's, &'static TerminalSurface>,
    viewport: Query<'w, 's, &'static TerminalViewport>,
    presentation: Res<'w, TerminalPresentation>,
    mobius_transition: Res<'w, MobiusTransition>,
    plane_view: ResMut<'w, TerminalPlaneView>,
    stage_tween: ResMut<'w, StageTween>,
    selection: ResMut<'w, TerminalSelection>,
    redraw: Query<'w, 's, &'static mut crate::terminal::TerminalRedrawState>,
    forwarded: Query<'w, 's, (Entity, &'static mut ForwardedMouseState)>,
    scroll: Query<'w, 's, &'static mut LocalScrollState>,
}

/// `pick_cell` (#56 decision 10's seam): pointer → the terminal under it
/// and the cell within it. The 3D modes raycast the STAGED front planes
/// and read the hit's interpolated UV against the OWNER's grid; in Flat2d
/// the focused-1:1 quad owns the whole window, so the entity selector is
/// the focus authority and `position_to_cell` survives as the 2D backend.
fn pick_cell(
    pointer: Vec2,
    mode: TerminalPresentationMode,
    pick: &mut PickParams,
    focus: &FocusedTerminal,
    window_size: Vec2,
    viewports: &Query<&'static TerminalViewport>,
    terminals: &Query<&'static TerminalSurface>,
) -> Option<(Entity, UVec2)> {
    if mode.is_3d() {
        let (seat, uv) = pick.pick_3d(pointer)?;
        let surface = terminals.get(seat).ok()?;
        Some((seat, cell_from_uv(uv, surface.cols, surface.rows)))
    } else {
        let seat = focus.get()?;
        let surface = terminals.get(seat).ok()?;
        let viewport = viewports.get(seat).ok()?;
        let cell = position_to_cell(pointer, window_size, viewport, surface)?;
        Some((seat, cell))
    }
}

/// The capture chart: while a press is captured the CAPTURED seat always
/// yields a cell — the infinite-plane clamp in the 3D modes, the viewport
/// clamp in Flat2d (xterm clamps drags outside the window to the nearest
/// cell). `None` freezes the drag at its last cell (parallel ray, or the
/// camera has not rendered yet).
fn capture_cell(
    pointer: Vec2,
    seat: Entity,
    mode: TerminalPresentationMode,
    pick: &mut PickParams,
    window_size: Vec2,
    viewports: &Query<&'static TerminalViewport>,
    terminals: &Query<&'static TerminalSurface>,
) -> Option<UVec2> {
    let surface = terminals.get(seat).ok()?;
    if mode.is_3d() {
        let uv = pick.capture_uv(pointer, seat)?;
        Some(cell_from_uv(uv, surface.cols, surface.rows))
    } else {
        let viewport = viewports.get(seat).ok()?;
        let margin = (window_size - viewport.size).max(Vec2::ZERO) * 0.5;
        let clamped = pointer.max(margin).min(margin + viewport.size - Vec2::ONE);
        position_to_cell(clamped, window_size, viewport, surface)
    }
}

/// Keeps a paused camera gesture's drag anchors at the pointer's current
/// position while a capture consumes the motion, so the gesture resumes
/// without a lurch when the capture clears.
fn refresh_gesture_anchors(plane_view: &mut TerminalPlaneView, position: Vec2) {
    if plane_view.rotating {
        plane_view.last_rotate_cursor = Some(position);
    }
    if plane_view.panning {
        plane_view.last_pan_cursor = Some(position);
    }
}

/// The seat holding the forward capture, if any: at most one seat has
/// pressed buttons (a press stamps it; capture routes every later press
/// to it until the last release).
fn forward_capture(
    forwarded: &Query<(Entity, &'static mut ForwardedMouseState)>,
) -> Option<Entity> {
    forwarded
        .iter()
        .find_map(|(entity, state)| state.any_pressed().then_some(entity))
}

/// Handles terminal mouse input.
pub(crate) fn handle_mouse_input(
    mut cursor_events: MessageReader<CursorMoved>,
    mut button_events: MessageReader<MouseButtonInput>,
    mut wheel_events: MessageReader<MouseWheel>,
    mut params: MouseSystemParams,
    mut pick: PickParams,
) {
    let MouseSystemParams {
        primary_window,
        focus,
        focus_requests,
        runtime,
        terminal,
        viewport,
        presentation,
        mobius_transition,
        plane_view,
        stage_tween,
        selection,
        redraw,
        forwarded,
        scroll,
    } = &mut params;
    // Zero terminals: nothing to pick, forward to, or orbit around — the
    // whole system no-ops (the pre-M4.6 zero-focus contract, kept).
    if terminal.is_empty() {
        return;
    }
    let Ok((primary_window, window)) = primary_window.single() else {
        return;
    };
    let window_size = window.resolution.size().max(Vec2::ONE);
    let mode = presentation.mode;
    let is_3d = mode.is_3d();
    let mobius_animating = mode == TerminalPresentationMode::Mobius3d && mobius_transition.active;

    for event in cursor_events.read() {
        if event.window != primary_window {
            continue;
        }

        selection.set_cursor_position(event.position);
        if mobius_animating {
            // Mid-morph the flat chart is approximate, so a captured drag
            // freezes at its last cell (#56 decision 10) and uncaptured
            // motion stays dropped — no cell updates either way.
            continue;
        }

        // Capture beats hover (invariant 7): a held forward capture
        // routes motion to its seat wherever the pointer goes. A camera
        // gesture paused by a mid-gesture hitting press keeps its anchors
        // fresh here, so the camera resumes from the pointer's CURRENT
        // position instead of lurching across the capture interlude.
        if let Some(seat) = forward_capture(forwarded) {
            refresh_gesture_anchors(plane_view, event.position);
            let Some(cell) = capture_cell(
                event.position,
                seat,
                mode,
                &mut pick,
                window_size,
                viewport,
                terminal,
            ) else {
                continue;
            };
            let Ok(runtime) = runtime.get_mut(seat) else {
                continue;
            };
            let mouse_mode = runtime.parser.screen().mouse_protocol_mode();
            let mouse_encoding = runtime.parser.screen().mouse_protocol_encoding();
            let Ok((_, mut state)) = forwarded.get_mut(seat) else {
                continue;
            };
            if state.last_cell == Some(cell) {
                continue;
            }
            if matches!(
                mouse_mode,
                MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
            ) {
                let code = state.motion_code();
                runtime.write_input(&encode_mouse_event(cell, code, false, mouse_encoding));
                state.last_cell = Some(cell);
            }
            continue;
        }

        // Selection drag capture — the press-capture seat pins the drag
        // for its whole life (#56 decision 11), clamped so leaving the
        // surface never loses the drag.
        if selection.capturing()
            && let Some(owner) = selection.owner
        {
            refresh_gesture_anchors(plane_view, event.position);
            if let Some(cell) = capture_cell(
                event.position,
                owner,
                mode,
                &mut pick,
                window_size,
                viewport,
                terminal,
            ) && selection.update_from_cursor(cell, event.position)
                && let Ok(mut redraw) = redraw.get_mut(owner)
            {
                redraw.request();
            }
            continue;
        }

        // Camera gestures continue while held (they started on a miss).
        if is_3d && (plane_view.rotating || plane_view.panning) {
            if plane_view.rotating {
                if let Some(last) = plane_view.last_rotate_cursor {
                    let delta = event.position - last;
                    if stage_tween.active {
                        stage_tween.stop();
                    }
                    plane_view.yaw += delta.x * 0.005;
                    plane_view.pitch -= delta.y * 0.005;
                    if let Some(seat) = focus.get()
                        && let Ok(mut redraw) = redraw.get_mut(seat)
                    {
                        redraw.request();
                    }
                }
                plane_view.last_rotate_cursor = Some(event.position);
            } else {
                if let Some(last) = plane_view.last_pan_cursor {
                    let delta = event.position - last;
                    if stage_tween.active {
                        stage_tween.stop();
                    }
                    plane_view.camera_offset.x -= delta.x * plane_view.zoom;
                    plane_view.camera_offset.y += delta.y * plane_view.zoom;
                    if let Some(seat) = focus.get()
                        && let Ok(mut redraw) = redraw.get_mut(seat)
                    {
                        redraw.request();
                    }
                }
                plane_view.last_pan_cursor = Some(event.position);
            }
            continue;
        }

        // Hover motion: an AnyMotion-tracking terminal under the pointer
        // receives buttonless motion (code 35), deduped in its own state.
        if let Some((seat, cell)) = pick_cell(
            event.position,
            mode,
            &mut pick,
            focus,
            window_size,
            viewport,
            terminal,
        ) {
            let Ok(runtime) = runtime.get_mut(seat) else {
                continue;
            };
            if runtime.parser.screen().mouse_protocol_mode() == MouseProtocolMode::AnyMotion {
                let mouse_encoding = runtime.parser.screen().mouse_protocol_encoding();
                let Ok((_, mut state)) = forwarded.get_mut(seat) else {
                    continue;
                };
                if state.last_cell != Some(cell) {
                    runtime.write_input(&encode_mouse_event(cell, 35, false, mouse_encoding));
                    state.last_cell = Some(cell);
                }
            }
        }
    }

    for event in button_events.read() {
        if event.window != primary_window {
            continue;
        }
        // Only L/M/R have a mouse-protocol encoding; other buttons still
        // pick and focus (the lock says ANY hitting press), they just
        // deliver nothing.
        let code = button_code(event.button);
        let pointer = window.cursor_position().or(selection.cursor_position());

        if mobius_animating {
            // Releases still land mid-morph — a capture must never stick
            // through a transition — at the frozen last cell; presses and
            // gestures stay dropped.
            if event.state == ButtonState::Released {
                release_capture(event.button, None, runtime, forwarded);
                if event.button == MouseButton::Left {
                    let _ = selection.end();
                    plane_view.rotating = false;
                }
                if event.button == MouseButton::Right {
                    plane_view.panning = false;
                }
            }
            continue;
        }

        match event.state {
            ButtonState::Pressed => {
                // Capture beats hover: later buttons join the forward
                // capture at the captured seat, wherever the pointer is.
                if let Some(seat) = forward_capture(forwarded) {
                    let cell = pointer.and_then(|pointer| {
                        capture_cell(
                            pointer,
                            seat,
                            mode,
                            &mut pick,
                            window_size,
                            viewport,
                            terminal,
                        )
                    });
                    let Ok(runtime) = runtime.get_mut(seat) else {
                        continue;
                    };
                    let mouse_mode = runtime.parser.screen().mouse_protocol_mode();
                    let mouse_encoding = runtime.parser.screen().mouse_protocol_encoding();
                    let Ok((_, mut state)) = forwarded.get_mut(seat) else {
                        continue;
                    };
                    if let Some(held) = state.pressed_mut(event.button) {
                        *held = true;
                    }
                    if mouse_mode != MouseProtocolMode::None
                        && let Some(code) = code
                        && let Some(cell) = cell.or(state.last_cell)
                    {
                        runtime.write_input(&encode_mouse_event(cell, code, false, mouse_encoding));
                        state.last_cell = Some(cell);
                    }
                    continue;
                }
                // During a selection drag, additional buttons are inert
                // (the Flat2d precedent).
                if selection.capturing() {
                    continue;
                }

                if let Some((seat, cell)) = pointer.and_then(|pointer| {
                    pick_cell(
                        pointer,
                        mode,
                        &mut pick,
                        focus,
                        window_size,
                        viewport,
                        terminal,
                    )
                }) {
                    // Focus-follows-click: any hitting press — and the
                    // press still delivers to that terminal
                    // (click-through: xterm, kitty and tmux panes all
                    // forward the focusing click).
                    focus_requests.write(FocusRequest {
                        target: Some(seat),
                        origin: FocusOrigin::PointerPress,
                    });
                    let Ok(runtime) = runtime.get_mut(seat) else {
                        continue;
                    };
                    let mouse_mode = runtime.parser.screen().mouse_protocol_mode();
                    let mouse_encoding = runtime.parser.screen().mouse_protocol_encoding();
                    if mouse_mode != MouseProtocolMode::None {
                        if let Some(code) = code {
                            runtime.write_input(&encode_mouse_event(
                                cell,
                                code,
                                false,
                                mouse_encoding,
                            ));
                            let Ok((_, mut state)) = forwarded.get_mut(seat) else {
                                continue;
                            };
                            if let Some(held) = state.pressed_mut(event.button) {
                                *held = true;
                            }
                            state.last_cell = Some(cell);
                        }
                    } else if event.button == MouseButton::Left
                        && let Some(pointer) = pointer
                    {
                        // `begin_pending` reports whether VISIBLE state
                        // changed (a fresh arm changes nothing on
                        // screen); the owner stamp must not ride on it —
                        // the armed capture routes the coming drag. The
                        // press-capture terminal owns the selection for
                        // its whole life (#56 decision 11): the PICKED
                        // seat, not the focused one.
                        let changed = selection.begin_pending(cell, pointer);
                        selection.set_owner(seat);
                        if changed && let Ok(mut redraw) = redraw.get_mut(seat) {
                            redraw.request();
                        }
                    }
                } else if is_3d {
                    // A miss is a camera gesture and never a focus change
                    // — clicking the void neither unfocuses nor steals
                    // (invariant 8: no focus from a missing pick).
                    match event.button {
                        MouseButton::Left => {
                            plane_view.rotating = true;
                            plane_view.last_rotate_cursor = selection.cursor_position();
                        }
                        MouseButton::Right => {
                            plane_view.panning = true;
                            plane_view.last_pan_cursor = selection.cursor_position();
                        }
                        _ => {}
                    }
                }
            }
            ButtonState::Released => {
                let released_capture = release_capture(
                    event.button,
                    pointer.and_then(|pointer| {
                        forward_capture(forwarded).and_then(|seat| {
                            capture_cell(
                                pointer,
                                seat,
                                mode,
                                &mut pick,
                                window_size,
                                viewport,
                                terminal,
                            )
                        })
                    }),
                    runtime,
                    forwarded,
                );
                if !released_capture && event.button == MouseButton::Left {
                    let _ = selection.end();
                }
                if is_3d {
                    if event.button == MouseButton::Left {
                        plane_view.rotating = false;
                        plane_view.last_rotate_cursor = selection.cursor_position();
                    }
                    if event.button == MouseButton::Right {
                        plane_view.panning = false;
                        plane_view.last_pan_cursor = selection.cursor_position();
                    }
                }
            }
        }
    }

    for event in wheel_events.read() {
        if mobius_animating {
            continue;
        }

        let delta = match event.unit {
            MouseScrollUnit::Line => event.y * 0.1,
            MouseScrollUnit::Pixel => event.y * 0.001,
        };
        let pointer = window.cursor_position().or(selection.cursor_position());

        // Scroll follows hover, not focus (invariant 7) — and the wheel
        // never emits a FocusRequest. In Flat2d the focused-1:1 quad owns
        // the whole window, so the hovered terminal is the focused one
        // wherever the pointer sits (today's N=1 behavior, byte-for-byte).
        let hover: Option<(Entity, Option<UVec2>)> = if is_3d {
            pointer
                .and_then(|pointer| {
                    pick_cell(
                        pointer,
                        mode,
                        &mut pick,
                        focus,
                        window_size,
                        viewport,
                        terminal,
                    )
                })
                .map(|(seat, cell)| (seat, Some(cell)))
        } else {
            focus.get().map(|seat| {
                let cell = pointer.and_then(|pointer| {
                    let surface = terminal.get(seat).ok()?;
                    let viewport = viewport.get(seat).ok()?;
                    position_to_cell(pointer, window_size, viewport, surface)
                });
                (seat, cell)
            })
        };

        if let Some((seat, cell)) = hover {
            let Ok(mut runtime) = runtime.get_mut(seat) else {
                continue;
            };
            let mouse_mode = runtime.parser.screen().mouse_protocol_mode();
            let mouse_encoding = runtime.parser.screen().mouse_protocol_encoding();
            if mouse_mode != MouseProtocolMode::None {
                if delta != 0.0
                    && let Some(cell) = cell
                {
                    runtime.write_input(&encode_mouse_event(
                        cell,
                        if delta > 0.0 { 64 } else { 65 },
                        false,
                        mouse_encoding,
                    ));
                }
            } else if !runtime.parser.screen().alternate_screen() {
                let amount = match event.unit {
                    MouseScrollUnit::Line => event.y.round() as isize,
                    MouseScrollUnit::Pixel => {
                        let Ok(surface) = terminal.get(seat) else {
                            continue;
                        };
                        let char_height = surface.char_dimensions().y;
                        let Ok(mut scroll) = scroll.get_mut(seat) else {
                            continue;
                        };
                        scroll.pixel_remainder += event.y / char_height;
                        let amount = scroll.pixel_remainder.trunc() as isize;
                        scroll.pixel_remainder -= amount as f32;
                        amount
                    }
                };

                if amount != 0 {
                    let screen = runtime.parser.screen_mut();
                    let current = screen.scrollback() as isize;
                    let next = (current + amount).max(0) as usize;
                    screen.set_scrollback(next);
                    if selection.owned_by_or_unowned(seat) {
                        selection.clear();
                    }
                    if let Ok(mut redraw) = redraw.get_mut(seat) {
                        redraw.request();
                    }
                }
            }
        } else if is_3d && delta != 0.0 {
            // Hovering the void: camera zoom keeps today's binding.
            if stage_tween.active {
                stage_tween.stop();
            }
            plane_view.zoom = (plane_view.zoom - delta).clamp(0.1, 4.0);
            if let Some(seat) = focus.get()
                && let Ok(mut redraw) = redraw.get_mut(seat)
            {
                redraw.request();
            }
        }
    }
}

/// Releases `button` from the forward capture, delivering the release to
/// the captured seat at `cell` (falling back to the frozen last cell) and
/// clearing the capture when the last button lifts. Returns whether a
/// capture held this button.
fn release_capture(
    button: MouseButton,
    cell: Option<UVec2>,
    runtime: &mut Query<&'static mut TerminalRuntime>,
    forwarded: &mut Query<(Entity, &'static mut ForwardedMouseState)>,
) -> bool {
    let Some(seat) = forward_capture(forwarded) else {
        return false;
    };
    let Some(code) = button_code(button) else {
        return false;
    };
    let cell = {
        let Ok((_, mut state)) = forwarded.get_mut(seat) else {
            return false;
        };
        let Some(held) = state.pressed_mut(button) else {
            return false;
        };
        if !*held {
            return false;
        }
        *held = false;
        let cell = cell.or(state.last_cell);
        // The dedupe cell SURVIVES the capture (the xterm precedent, and
        // the pre-M4.6 byte stream): post-release sub-cell hover motion
        // under AnyMotion stays silent until the pointer crosses into a
        // new cell.
        state.last_cell = cell;
        cell
    };
    if let Some(cell) = cell
        && let Ok(runtime) = runtime.get_mut(seat)
    {
        let mouse_mode = runtime.parser.screen().mouse_protocol_mode();
        let mouse_encoding = runtime.parser.screen().mouse_protocol_encoding();
        if mouse_mode != MouseProtocolMode::None {
            runtime.write_input(&encode_mouse_event(cell, code, true, mouse_encoding));
        }
    }
    true
}

fn button_code(button: MouseButton) -> Option<u16> {
    match button {
        MouseButton::Left => Some(0),
        MouseButton::Middle => Some(1),
        MouseButton::Right => Some(2),
        _ => None,
    }
}

fn encode_mouse_event(
    cell: UVec2,
    button_code: u16,
    release: bool,
    encoding: MouseProtocolEncoding,
) -> Vec<u8> {
    let col = cell.x + 1;
    let row = cell.y + 1;
    match encoding {
        MouseProtocolEncoding::Sgr => {
            let final_byte = if release { 'm' } else { 'M' };
            format!("\x1b[<{button_code};{col};{row}{final_byte}").into_bytes()
        }
        MouseProtocolEncoding::Default | MouseProtocolEncoding::Utf8 => {
            let code = if release { 3 } else { button_code }.saturating_add(32);
            let x = (col + 32).min(u8::MAX as u32) as u8;
            let y = (row + 32).min(u8::MAX as u32) as u8;
            vec![0x1b, b'[', b'M', code as u8, x, y]
        }
    }
}

pub(crate) fn encode_mouse_wheel(
    cell: UVec2,
    up: bool,
    encoding: MouseProtocolEncoding,
) -> Vec<u8> {
    encode_mouse_event(cell, if up { 64 } else { 65 }, false, encoding)
}

fn position_to_cell(
    position: Vec2,
    window_size: Vec2,
    viewport: &TerminalViewport,
    terminal: &TerminalSurface,
) -> Option<UVec2> {
    if viewport.size.x <= 0.0 || viewport.size.y <= 0.0 {
        return None;
    }

    let cols = terminal.cols.max(1) as f32;
    let rows = terminal.rows.max(1) as f32;
    let cell_width = viewport.size.x / cols;
    let cell_height = viewport.size.y / rows;
    if cell_width <= 0.0 || cell_height <= 0.0 {
        return None;
    }

    let margin = (window_size - viewport.size).max(Vec2::ZERO) * 0.5;
    let local_position = position - margin;
    if local_position.x < 0.0
        || local_position.y < 0.0
        || local_position.x >= viewport.size.x
        || local_position.y >= viewport.size.y
    {
        return None;
    }

    let x = local_position.x.clamp(0.0, viewport.size.x - 1.0);
    let y = local_position.y.clamp(0.0, viewport.size.y - 1.0);
    let col = (x / cell_width).floor() as u32;
    let row = (y / cell_height).floor() as u32;

    Some(UVec2::new(
        col.min(terminal.cols.saturating_sub(1) as u32),
        row.min(terminal.rows.saturating_sub(1) as u32),
    ))
}

#[cfg(test)]
mod tests {
    use bevy::camera::primitives::{Aabb, MeshAabb};
    use bevy::camera::{OrthographicProjection, RenderTargetInfo};
    use bevy::ecs::message::Messages;
    use bevy::ecs::system::RunSystemOnce;
    use bevy::picking::mesh_picking::ray_cast::RayCastBackfaces;
    use bevy::window::PrimaryWindow;

    use super::*;
    use crate::config::AppConfig;
    use crate::focus::{FocusGained, FocusLost, drain_focus_requests};
    use crate::identity::{TerminalIdentity, TerminalRegistry};
    use crate::runtime::VirtualTerminalHost;
    use crate::scene::{TerminalOwner, TerminalPlane, TerminalPlaneCamera, terminal_plane_mesh};
    use crate::terminal::TerminalRedrawState;

    /// `Window::default()`'s logical resolution, which the hand-built
    /// camera below mirrors.
    const WINDOW: Vec2 = Vec2::new(1280.0, 720.0);
    /// Every test plane's world footprint (transform scale).
    const PLANE: Vec2 = Vec2::new(600.0, 500.0);

    /// The stage camera with the computed state a render frame would
    /// populate, hand-set so `Camera::viewport_to_world` works headless:
    /// orthographic, 1 world unit per logical pixel, at (0, 0, 800)
    /// looking down −Z — the `setup_scene_stage` pose.
    fn spawn_stage_camera(world: &mut World) {
        let mut ortho = OrthographicProjection::default_3d();
        ortho.near = -2000.0;
        ortho.far = 2000.0;
        let mut projection = Projection::Orthographic(ortho);
        projection.update(WINDOW.x, WINDOW.y);
        let mut camera = Camera::default();
        camera.computed.clip_from_view = projection.get_clip_from_view();
        camera.computed.target_info = Some(RenderTargetInfo {
            physical_size: UVec2::new(WINDOW.x as u32, WINDOW.y as u32),
            scale_factor: 1.0,
        });
        let transform = Transform::from_xyz(0.0, 0.0, 800.0).looking_at(Vec3::ZERO, Vec3::Y);
        world.spawn((
            TerminalPlaneCamera,
            camera,
            projection,
            GlobalTransform::from(transform),
        ));
    }

    /// A picking world: the hand-computed stage camera, two virtual seats
    /// whose hosts expose every byte the mouse writes, and one front
    /// plane per seat — seat A's at world x = −320, seat B's at +320, so
    /// pointer x < 640 is over A, > 640 over B, and the seam between
    /// them (plus the strip above/below) misses both.
    fn picking_world(
        mode: TerminalPresentationMode,
    ) -> (
        World,
        (Entity, VirtualTerminalHost),
        (Entity, VirtualTerminalHost),
    ) {
        bevy::tasks::ComputeTaskPool::get_or_init(bevy::tasks::TaskPool::default);
        let mut world = World::new();
        world.insert_resource(AppConfig::default());
        world.init_resource::<Assets<Mesh>>();
        world.insert_resource(TerminalPresentation { mode });
        world.insert_resource(TerminalPlaneView::default());
        world.insert_resource(MobiusTransition::default());
        world.insert_resource(StageTween::default());
        world.init_resource::<TerminalSelection>();
        world.init_resource::<FocusedTerminal>();
        world.init_resource::<Messages<CursorMoved>>();
        world.init_resource::<Messages<MouseButtonInput>>();
        world.init_resource::<Messages<MouseWheel>>();
        world.init_resource::<Messages<FocusRequest>>();
        world.init_resource::<Messages<FocusGained>>();
        world.init_resource::<Messages<FocusLost>>();
        world.spawn((Window::default(), PrimaryWindow));
        spawn_stage_camera(&mut world);

        let mut registry = TerminalRegistry::default();
        let seat_at = |world: &mut World, registry: &mut TerminalRegistry, x: f32| {
            let identity = registry.allocate().expect("test lease");
            let (runtime, host) =
                TerminalRuntime::virtual_channel(&AppConfig::default(), identity.ingress());
            let surface = TerminalSurface::new(&AppConfig::default())
                .expect("surface construction is CPU-only");
            let seat = world
                .spawn((
                    identity,
                    runtime,
                    surface,
                    TerminalViewport {
                        size: PLANE,
                        center: Vec2::ZERO,
                    },
                    TerminalRedrawState::default(),
                    ForwardedMouseState::default(),
                    LocalScrollState::default(),
                ))
                .id();
            registry
                .bind(identity.id(), seat)
                .expect("the just-allocated lease is live");
            let mesh_handle = world
                .resource_mut::<Assets<Mesh>>()
                .add(terminal_plane_mesh(32, 20));
            let aabb: Aabb = world
                .resource::<Assets<Mesh>>()
                .get(&mesh_handle)
                .expect("mesh lives")
                .compute_aabb()
                .expect("plane mesh has an Aabb");
            let transform = Transform {
                translation: Vec3::new(x, 0.0, 0.0),
                rotation: Quat::IDENTITY,
                scale: PLANE.extend(1.0),
            };
            world.spawn((
                TerminalPlane,
                TerminalOwner(seat),
                Mesh3d(mesh_handle),
                RayCastBackfaces,
                transform,
                GlobalTransform::from(transform),
                Visibility::Visible,
                aabb,
            ));
            (seat, host)
        };
        let a = seat_at(&mut world, &mut registry, -320.0);
        let b = seat_at(&mut world, &mut registry, 320.0);
        world.insert_resource(registry);
        (world, a, b)
    }

    fn assert_two_seats(world: &mut World) {
        assert_eq!(
            world.query::<&TerminalIdentity>().iter(world).count(),
            2,
            "seat count asserted (#58 rider)"
        );
    }

    fn primary_window(world: &mut World) -> Entity {
        world
            .query_filtered::<Entity, With<PrimaryWindow>>()
            .single(world)
            .expect("one primary window")
    }

    fn run_mouse(world: &mut World) {
        world
            .run_system_once(handle_mouse_input)
            .expect("mouse handler runs");
        // run_system_once never advances message buffers; drop consumed
        // events so the next call cannot re-read them.
        world.resource_mut::<Messages<CursorMoved>>().clear();
        world.resource_mut::<Messages<MouseButtonInput>>().clear();
        world.resource_mut::<Messages<MouseWheel>>().clear();
    }

    fn move_cursor(world: &mut World, position: Vec2) {
        let window = primary_window(world);
        world
            .resource_mut::<Messages<CursorMoved>>()
            .write(CursorMoved {
                window,
                position,
                delta: None,
            });
        run_mouse(world);
    }

    fn button(world: &mut World, button: MouseButton, state: ButtonState) {
        let window = primary_window(world);
        world
            .resource_mut::<Messages<MouseButtonInput>>()
            .write(MouseButtonInput {
                button,
                state,
                window,
            });
        run_mouse(world);
    }

    fn wheel(world: &mut World, unit: MouseScrollUnit, y: f32) {
        let window = primary_window(world);
        world
            .resource_mut::<Messages<MouseWheel>>()
            .write(MouseWheel {
                unit,
                x: 0.0,
                y,
                window,
                phase: bevy::input::touch::TouchPhase::Moved,
            });
        run_mouse(world);
    }

    fn drain_focus_requests_now(world: &mut World) -> Vec<FocusRequest> {
        world
            .resource_mut::<Messages<FocusRequest>>()
            .drain()
            .collect()
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
        world.resource_mut::<Messages<FocusRequest>>().clear();
    }

    fn grid(world: &mut World, seat: Entity) -> (u16, u16) {
        let surface = world.get::<TerminalSurface>(seat).expect("seat surface");
        (surface.cols, surface.rows)
    }

    /// The flat-plane pick chart in test space: pointer → world (1:1
    /// logical pixels, y flipped) → plane-local → UV → cell. Doubles as
    /// the capture clamp expectation for off-plane pointers.
    fn expected_cell(pointer: Vec2, plane_x: f32, cols: u16, rows: u16) -> UVec2 {
        let world_x = pointer.x - WINDOW.x / 2.0;
        let world_y = WINDOW.y / 2.0 - pointer.y;
        let u = (world_x - plane_x) / PLANE.x + 0.5;
        let v = 0.5 - world_y / PLANE.y;
        cell_from_uv(Vec2::new(u, v), cols, rows)
    }

    fn enable_sgr_mouse(world: &mut World, seat: Entity, mode_seq: &[u8]) {
        let mut runtime = world.get_mut::<TerminalRuntime>(seat).expect("runtime");
        runtime.parser.process(mode_seq);
        runtime.parser.process(b"\x1b[?1006h");
    }

    fn sgr(code: u16, cell: UVec2, release: bool) -> Vec<u8> {
        encode_mouse_event(cell, code, release, MouseProtocolEncoding::Sgr)
    }

    /// Off-center, off-grid-line pointers over each plane's interior.
    const OVER_A: Vec2 = Vec2::new(317.0, 371.0);
    const OVER_B: Vec2 = Vec2::new(963.0, 371.0);
    /// The seam between the planes: over neither.
    const OVER_VOID: Vec2 = Vec2::new(640.0, 360.0);

    /// The lock says ANY button press whose pick hits emits the request
    /// (review-verified gap: left-only emission survived the old test) —
    /// and each press still delivers, click-through.
    #[test]
    fn every_hitting_button_requests_focus_and_delivers_click_through() {
        let (mut world, (seat_a, host_a), (seat_b, host_b)) =
            picking_world(TerminalPresentationMode::Plane3d);
        assert_two_seats(&mut world);
        enable_sgr_mouse(&mut world, seat_b, b"\x1b[?1000h");
        let (cols, rows) = grid(&mut world, seat_b);
        let cell = expected_cell(OVER_B, 320.0, cols, rows);

        move_cursor(&mut world, OVER_B);
        for (mouse_button, code) in [
            (MouseButton::Left, 0),
            (MouseButton::Middle, 1),
            (MouseButton::Right, 2),
        ] {
            button(&mut world, mouse_button, ButtonState::Pressed);
            assert_eq!(
                drain_focus_requests_now(&mut world),
                vec![FocusRequest {
                    target: Some(seat_b),
                    origin: FocusOrigin::PointerPress,
                }],
                "a hitting {mouse_button:?} press emits exactly one request at the picked seat"
            );
            assert_eq!(
                host_b.input_rx.try_recv().expect("B receives the press"),
                sgr(code, cell, false),
                "the focusing {mouse_button:?} click still delivers (click-through)"
            );
            // Release so the next button is a fresh hit, not a capture join.
            button(&mut world, mouse_button, ButtonState::Released);
            assert_eq!(
                host_b.input_rx.try_recv().expect("B receives the release"),
                sgr(code, cell, true)
            );
        }
        assert!(
            host_a.input_rx.try_recv().is_err(),
            "the un-hit terminal receives nothing"
        );
        let _ = seat_a;
    }

    #[test]
    fn a_missing_press_is_a_camera_gesture_and_never_focus() {
        let (mut world, _a, _b) = picking_world(TerminalPresentationMode::Plane3d);
        assert_two_seats(&mut world);

        move_cursor(&mut world, OVER_VOID);
        button(&mut world, MouseButton::Left, ButtonState::Pressed);

        assert!(
            drain_focus_requests_now(&mut world).is_empty(),
            "a miss emits NO FocusRequest — not even target: None (invariant 8)"
        );
        assert!(
            world.resource::<TerminalPlaneView>().rotating,
            "the miss press keeps today's orbit binding"
        );
        button(&mut world, MouseButton::Left, ButtonState::Released);
        assert!(!world.resource::<TerminalPlaneView>().rotating);

        button(&mut world, MouseButton::Right, ButtonState::Pressed);
        assert!(
            drain_focus_requests_now(&mut world).is_empty(),
            "a missing right press requests nothing either"
        );
        assert!(
            world.resource::<TerminalPlaneView>().panning,
            "the miss right press keeps today's pan binding"
        );
        button(&mut world, MouseButton::Right, ButtonState::Released);
        assert!(!world.resource::<TerminalPlaneView>().panning);

        let zoom_before = world.resource::<TerminalPlaneView>().zoom;
        wheel(&mut world, MouseScrollUnit::Line, 1.0);
        assert!(
            world.resource::<TerminalPlaneView>().zoom < zoom_before,
            "wheel over the void keeps today's zoom binding"
        );
    }

    #[test]
    fn selection_drag_clamps_off_the_plane_and_survives_focus_loss() {
        let (mut world, (seat_a, _host_a), (seat_b, _host_b)) =
            picking_world(TerminalPresentationMode::Plane3d);
        assert_two_seats(&mut world);
        let (cols, rows) = grid(&mut world, seat_a);

        move_cursor(&mut world, OVER_A);
        button(&mut world, MouseButton::Left, ButtonState::Pressed);
        assert_eq!(
            world.resource::<TerminalSelection>().owner,
            Some(seat_a),
            "the press-capture seat owns the selection (#56 decision 11)"
        );
        assert!(
            world
                .resource::<TerminalSelection>()
                .pending_start
                .is_some()
        );

        // Past the drag threshold, still on the plane.
        move_cursor(&mut world, OVER_A + Vec2::new(30.0, 0.0));
        assert!(world.resource::<TerminalSelection>().dragging);

        // Far beyond the plane's left edge: the infinite-plane clamp
        // pins the drag to column 0 instead of dropping it.
        let off_left = Vec2::new(2.0, OVER_A.y);
        move_cursor(&mut world, off_left);
        assert_eq!(
            world.resource::<TerminalSelection>().end,
            Some(expected_cell(off_left, -320.0, cols, rows)),
            "drag clamp holds (#58 exit criterion)"
        );
        assert_eq!(
            world.resource::<TerminalSelection>().end.map(|cell| cell.x),
            Some(0),
            "the clamped cell is the first column"
        );

        // Selections survive focus loss, on their own terminal.
        focus(&mut world, seat_b);
        assert_eq!(
            world.resource::<TerminalSelection>().owner,
            Some(seat_a),
            "focus is routing, not ownership — the selection stays A's"
        );
        assert!(world.resource::<TerminalSelection>().dragging);

        // Focused-1:1 would now HIDE A's plane; the capture deliberately
        // ignores visibility (capture beats hover beats focus), so the
        // drag keeps charting cells on the hidden plane.
        let plane_a = world
            .query_filtered::<(Entity, &TerminalOwner), With<TerminalPlane>>()
            .iter(&world)
            .find_map(|(entity, owner)| (owner.0 == seat_a).then_some(entity))
            .expect("A's front plane exists");
        world.entity_mut(plane_a).insert(Visibility::Hidden);
        let after_hide = OVER_A + Vec2::new(60.0, 30.0);
        move_cursor(&mut world, after_hide);
        assert_eq!(
            world.resource::<TerminalSelection>().end,
            Some(expected_cell(after_hide, -320.0, cols, rows)),
            "a mid-drag focus loss (hidden plane) never breaks the drag"
        );

        button(&mut world, MouseButton::Left, ButtonState::Released);
        assert!(!world.resource::<TerminalSelection>().dragging);
    }

    #[test]
    fn capture_routes_motion_and_release_to_the_press_seat() {
        let (mut world, (seat_a, host_a), (seat_b, host_b)) =
            picking_world(TerminalPresentationMode::Plane3d);
        assert_two_seats(&mut world);
        enable_sgr_mouse(&mut world, seat_a, b"\x1b[?1002h");
        let (cols, rows) = grid(&mut world, seat_a);

        move_cursor(&mut world, OVER_A);
        button(&mut world, MouseButton::Left, ButtonState::Pressed);
        let press_cell = expected_cell(OVER_A, -320.0, cols, rows);
        assert_eq!(
            host_a.input_rx.try_recv().expect("A receives the press"),
            sgr(0, press_cell, false)
        );

        // The pointer crosses onto B's plane mid-drag: motion still
        // routes to A, clamped to A's chart — capture beats hover.
        move_cursor(&mut world, OVER_B);
        let clamped = expected_cell(OVER_B, -320.0, cols, rows);
        assert_eq!(
            clamped.x,
            u32::from(cols) - 1,
            "the pointer over B clamps to A's last column"
        );
        assert_eq!(
            host_a.input_rx.try_recv().expect("A receives the motion"),
            sgr(32, clamped, false),
            "held-button motion routes to the captured seat"
        );

        button(&mut world, MouseButton::Left, ButtonState::Released);
        assert_eq!(
            host_a.input_rx.try_recv().expect("A receives the release"),
            sgr(0, clamped, true),
            "the release routes to the capture regardless of pointer"
        );
        assert!(
            !world
                .get::<ForwardedMouseState>(seat_a)
                .expect("state")
                .any_pressed(),
            "releasing the last button clears the capture (bounded state)"
        );
        assert!(
            host_b.input_rx.try_recv().is_err(),
            "B saw none of a drag that visually crossed its plane"
        );
        assert_eq!(
            drain_focus_requests_now(&mut world),
            vec![FocusRequest {
                target: Some(seat_a),
                origin: FocusOrigin::PointerPress,
            }],
            "only the initial hitting press requested focus"
        );
        let _ = seat_b;
    }

    #[test]
    fn wheel_follows_hover_not_focus() {
        let (mut world, (seat_a, _host_a), (seat_b, _host_b)) =
            picking_world(TerminalPresentationMode::Plane3d);
        assert_two_seats(&mut world);
        focus(&mut world, seat_a);

        // Give B scrollback to consume.
        {
            let mut runtime = world.get_mut::<TerminalRuntime>(seat_b).expect("runtime");
            for _ in 0..200 {
                runtime.parser.process(b"x\r\n");
            }
        }

        // A standing selection on A (#56 decision 11): scrolling B must
        // not destroy it (review-verified gap: an unconditional clear
        // survived the old test).
        {
            let mut selection = world.resource_mut::<TerminalSelection>();
            selection.begin(UVec2::new(3, 3));
            selection.set_owner(seat_a);
        }

        let zoom_before = world.resource::<TerminalPlaneView>().zoom;
        move_cursor(&mut world, OVER_B);
        wheel(&mut world, MouseScrollUnit::Line, 3.0);

        assert!(
            world.resource::<TerminalSelection>().start.is_some(),
            "A's standing selection survives B's scroll (decision 11)"
        );

        let b_scrollback = world
            .get::<TerminalRuntime>(seat_b)
            .expect("runtime")
            .parser
            .screen()
            .scrollback();
        assert!(
            b_scrollback > 0,
            "the wheel scrolled the HOVERED terminal's scrollback"
        );
        assert_eq!(
            world
                .get::<TerminalRuntime>(seat_a)
                .expect("runtime")
                .parser
                .screen()
                .scrollback(),
            0,
            "the FOCUSED terminal did not scroll"
        );
        assert_eq!(
            world.resource::<TerminalPlaneView>().zoom,
            zoom_before,
            "wheel over a plane no longer zooms the camera"
        );
        assert!(
            drain_focus_requests_now(&mut world).is_empty(),
            "the wheel never moves focus (invariant 7)"
        );
        assert!(
            world.resource::<FocusedTerminal>().is_focused(seat_a),
            "focus stayed where it was"
        );

        // Scrolling the OWNER's own scrollback clears its selection.
        {
            let mut runtime = world.get_mut::<TerminalRuntime>(seat_a).expect("runtime");
            for _ in 0..200 {
                runtime.parser.process(b"x\r\n");
            }
        }
        move_cursor(&mut world, OVER_A);
        wheel(&mut world, MouseScrollUnit::Line, 3.0);
        assert!(
            world.resource::<TerminalSelection>().start.is_none(),
            "the owner's own scroll clears its selection"
        );
    }

    #[test]
    fn wheel_forwards_to_a_protocol_terminal_at_the_hovered_cell() {
        let (mut world, (_seat_a, host_a), (seat_b, host_b)) =
            picking_world(TerminalPresentationMode::Plane3d);
        assert_two_seats(&mut world);
        enable_sgr_mouse(&mut world, seat_b, b"\x1b[?1000h");
        let (cols, rows) = grid(&mut world, seat_b);

        move_cursor(&mut world, OVER_B);
        wheel(&mut world, MouseScrollUnit::Line, 1.0);
        assert_eq!(
            host_b.input_rx.try_recv().expect("B receives the wheel"),
            sgr(64, expected_cell(OVER_B, 320.0, cols, rows), false),
            "the wheel forwards at the hovered terminal's own cell"
        );
        assert!(host_a.input_rx.try_recv().is_err());
    }

    #[test]
    fn scroll_remainder_stays_per_seat() {
        let (mut world, (seat_a, _host_a), (seat_b, _host_b)) =
            picking_world(TerminalPresentationMode::Plane3d);
        assert_two_seats(&mut world);

        // Give A scrollback so a whole-line step can land.
        {
            let mut runtime = world.get_mut::<TerminalRuntime>(seat_a).expect("runtime");
            for _ in 0..200 {
                runtime.parser.process(b"x\r\n");
            }
        }
        let char_height = world
            .get::<TerminalSurface>(seat_a)
            .expect("surface")
            .char_dimensions()
            .y;
        let point6 = char_height * 0.6;

        move_cursor(&mut world, OVER_A);
        wheel(&mut world, MouseScrollUnit::Pixel, point6);
        move_cursor(&mut world, OVER_B);
        wheel(&mut world, MouseScrollUnit::Pixel, point6);
        move_cursor(&mut world, OVER_A);
        wheel(&mut world, MouseScrollUnit::Pixel, point6);

        assert_eq!(
            world
                .get::<TerminalRuntime>(seat_a)
                .expect("runtime")
                .parser
                .screen()
                .scrollback(),
            1,
            "A's two 0.6-line ticks crossed one whole line"
        );
        let b_remainder = world
            .get::<LocalScrollState>(seat_b)
            .expect("state")
            .remainder();
        assert!(
            (b_remainder - 0.6).abs() < 1e-3,
            "B's remainder is its own — no leak between seats (got {b_remainder})"
        );
    }

    #[test]
    fn mid_morph_the_drag_freezes_and_release_still_lands() {
        let (mut world, (seat_a, _host_a), _b) = picking_world(TerminalPresentationMode::Mobius3d);
        assert_two_seats(&mut world);
        let (cols, rows) = grid(&mut world, seat_a);

        move_cursor(&mut world, OVER_A);
        button(&mut world, MouseButton::Left, ButtonState::Pressed);
        let drag_to = OVER_A + Vec2::new(30.0, 0.0);
        move_cursor(&mut world, drag_to);
        assert!(world.resource::<TerminalSelection>().dragging);
        let frozen = world.resource::<TerminalSelection>().end;
        assert_eq!(frozen, Some(expected_cell(drag_to, -320.0, cols, rows)));

        world.resource_mut::<MobiusTransition>().active = true;
        move_cursor(&mut world, OVER_A + Vec2::new(120.0, 40.0));
        assert_eq!(
            world.resource::<TerminalSelection>().end,
            frozen,
            "mid-morph the drag freezes at its last cell (#56 decision 10)"
        );
        button(&mut world, MouseButton::Left, ButtonState::Released);
        assert!(
            !world.resource::<TerminalSelection>().dragging,
            "the release still lands mid-morph — a capture never sticks"
        );
    }

    #[test]
    fn flat2d_wheel_scrolls_the_focused_terminal_anywhere_in_the_window() {
        let (mut world, (seat_a, _host_a), _b) = picking_world(TerminalPresentationMode::Flat2d);
        assert_two_seats(&mut world);
        focus(&mut world, seat_a);
        {
            let mut runtime = world.get_mut::<TerminalRuntime>(seat_a).expect("runtime");
            for _ in 0..200 {
                runtime.parser.process(b"x\r\n");
            }
        }

        // The pointer sits in the window margin, outside the centered
        // viewport: Flat2d hover is the focused-1:1 quad, so the wheel
        // still scrolls — today's N=1 behavior, byte-for-byte.
        move_cursor(&mut world, Vec2::new(5.0, 5.0));
        wheel(&mut world, MouseScrollUnit::Line, 2.0);
        assert_eq!(
            world
                .get::<TerminalRuntime>(seat_a)
                .expect("runtime")
                .parser
                .screen()
                .scrollback(),
            2
        );
    }

    /// The Flat2d cell chart in test space: the viewport centered in the
    /// window, floored to cells — `position_to_cell`'s math.
    fn flat2d_cell(pointer: Vec2, cols: u16, rows: u16) -> UVec2 {
        let margin = (WINDOW - PLANE) * 0.5;
        let local = pointer - margin;
        UVec2::new(
            ((local.x / (PLANE.x / cols as f32)) as u32).min(cols as u32 - 1),
            ((local.y / (PLANE.y / rows as f32)) as u32).min(rows as u32 - 1),
        )
    }

    /// The shipped default mode's whole press path, end to end
    /// (review-verified gap: the Flat2d pick arm could return None with
    /// every test green). The focused-1:1 quad is the hit entity; press
    /// focuses, arms, drags, releases — and with a protocol on, the
    /// press delivers instead.
    #[test]
    fn flat2d_press_drag_release_selects_on_the_focused_quad() {
        let (mut world, (seat_a, host_a), _b) = picking_world(TerminalPresentationMode::Flat2d);
        assert_two_seats(&mut world);
        focus(&mut world, seat_a);
        let (cols, rows) = grid(&mut world, seat_a);

        let start = Vec2::new(700.0, 400.0);
        move_cursor(&mut world, start);
        button(&mut world, MouseButton::Left, ButtonState::Pressed);
        assert_eq!(
            drain_focus_requests_now(&mut world),
            vec![FocusRequest {
                target: Some(seat_a),
                origin: FocusOrigin::PointerPress,
            }],
            "the Flat2d quad is a hit — the press requests focus"
        );
        assert_eq!(world.resource::<TerminalSelection>().owner, Some(seat_a));

        let drag_to = Vec2::new(780.0, 430.0);
        move_cursor(&mut world, drag_to);
        assert!(world.resource::<TerminalSelection>().dragging);
        assert_eq!(
            world.resource::<TerminalSelection>().end,
            Some(flat2d_cell(drag_to, cols, rows)),
            "the drag tracks position_to_cell's chart"
        );
        button(&mut world, MouseButton::Left, ButtonState::Released);
        assert!(!world.resource::<TerminalSelection>().dragging);

        // With a protocol on, the press delivers instead of selecting.
        enable_sgr_mouse(&mut world, seat_a, b"\x1b[?1000h");
        move_cursor(&mut world, start);
        button(&mut world, MouseButton::Left, ButtonState::Pressed);
        assert_eq!(
            host_a.input_rx.try_recv().expect("A receives the press"),
            sgr(0, flat2d_cell(start, cols, rows), false)
        );
        button(&mut world, MouseButton::Left, ButtonState::Released);
    }

    /// The Flat2d capture clamp (declared divergence from pre-M4.6,
    /// which went SILENT in the margin and dropped release bytes there,
    /// leaving apps with a stuck button): a captured drag into the
    /// window margin emits clamped edge-cell motion, and the release
    /// lands at the clamped cell.
    #[test]
    fn flat2d_capture_clamps_margin_drags_and_lands_the_release() {
        let (mut world, (seat_a, host_a), _b) = picking_world(TerminalPresentationMode::Flat2d);
        assert_two_seats(&mut world);
        focus(&mut world, seat_a);
        enable_sgr_mouse(&mut world, seat_a, b"\x1b[?1002h");
        let (cols, rows) = grid(&mut world, seat_a);

        let start = Vec2::new(700.0, 400.0);
        move_cursor(&mut world, start);
        button(&mut world, MouseButton::Left, ButtonState::Pressed);
        assert_eq!(
            host_a.input_rx.try_recv().expect("press byte"),
            sgr(0, flat2d_cell(start, cols, rows), false)
        );

        // Deep into the left margin: the capture clamp pins the pointer
        // into the viewport, so motion forwards at the first column.
        let margin_pos = Vec2::new(5.0, 400.0);
        let margin = (WINDOW - PLANE) * 0.5;
        let clamped = flat2d_cell(Vec2::new(margin.x, margin_pos.y), cols, rows);
        assert_eq!(clamped.x, 0, "the clamp lands on the first column");
        move_cursor(&mut world, margin_pos);
        assert_eq!(
            host_a.input_rx.try_recv().expect("clamped motion byte"),
            sgr(32, clamped, false)
        );

        // The release in the margin still lands (pre-M4.6 dropped it —
        // a stuck button for the app).
        button(&mut world, MouseButton::Left, ButtonState::Released);
        assert_eq!(
            host_a.input_rx.try_recv().expect("clamped release byte"),
            sgr(0, clamped, true)
        );
        assert!(
            !world
                .get::<ForwardedMouseState>(seat_a)
                .expect("state")
                .any_pressed(),
            "the capture cleared"
        );
    }
}
