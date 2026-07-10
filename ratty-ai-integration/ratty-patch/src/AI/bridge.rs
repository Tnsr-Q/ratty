// ═══════════════════════════════════════════════════════════
// STEP 2: The Bridge — Terminal thread → Bevy ECS
// ═══════════════════════════════════════════════════════════
//
// File: src/ai/bridge.rs (new file)
//
// This lives in the Bevy world. Every frame it drains the
// crossbeam channel from the terminal thread and re-emits
// the commands as Bevy events.

use bevy::prelude::*;
use crossbeam::channel::{bounded, Receiver, Sender};
use crate::ai::osc_parser::RattyAiCommand;

/// Resource holding the receiver side of the channel.
/// The terminal thread holds the Sender.
#[derive(Resource)]
pub struct RattyAiBridge {
    pub receiver: Receiver<RattyAiCommand>,
}

/// System that runs every frame in Bevy's Update schedule.
/// Drains all pending commands from the terminal thread and
/// re-emits them as Bevy events so systems can react.
pub fn bridge_system(
    bridge: Res<RattyAiBridge>,
    mut events: EventWriter<RattyAiCommand>,
) {
    // Drain the entire channel — non-blocking
    while let Ok(cmd) = bridge.receiver.try_recv() {
        events.send(cmd);
    }
}

/// Call this once when building your Bevy App.
/// Creates the channel, stores the receiver as a resource,
/// and returns the Sender for the terminal thread to use.
pub fn setup_ai_bridge(app: &mut App) -> Sender<RattyAiCommand> {
    let (tx, rx) = bounded::<RattyAiCommand>(1024);
    app.insert_resource(RattyAiBridge { receiver: rx });
    tx
}

// ── How to wire this in your terminal setup ──
//
// In src/main.rs (or wherever you spawn the terminal):
//
//   let ai_tx = setup_ai_bridge(&mut app);
//
//   // Pass ai_tx into your VtParser when you create it:
//   let vt_parser = VtParser::new(
//       // ... existing args ...
//       ai_tx,  // <-- ADD THIS
//   );
