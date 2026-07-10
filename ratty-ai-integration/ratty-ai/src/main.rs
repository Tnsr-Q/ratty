// ═══════════════════════════════════════════════════════════
// STEP 3: Register the Plugin in your Bevy App
// ═══════════════════════════════════════════════════════════
//
// File: src/main.rs (or wherever you build your Bevy App)
//
// Just add the plugin. That's it.

use bevy::prelude::*;
use crate::ai::{
    bridge::{bridge_system, setup_ai_bridge},
    bevy_handler::RattyAiPlugin,
};

fn main() {
    let mut app = App::new();

    // 1. Setup the bridge (creates channel, returns sender for terminal)
    let ai_tx = setup_ai_bridge(&mut app);

    // 2. Add your existing plugins
    app.add_plugins(DefaultPlugins)
       // ... your existing Ratty plugins ...

    // 3. ADD THIS ONE LINE:
       .add_plugins(RattyAiPlugin);

    // 4. Add the bridge system to Update schedule
    app.add_systems(Update, bridge_system);

    // 5. Pass ai_tx to your terminal thread / VtParser
    //    (however you currently construct it)
    //
    //    let terminal = Terminal::new(ai_tx, ...);
    //    std::thread::spawn(move || terminal.run());

    app.run();
}

// ── That's literally it. Three additions ──
// 1. setup_ai_bridge(&mut app)           → creates channel
// 2. .add_plugins(RattyAiPlugin)          → handles all commands
// 3. bridge_system in Update             → ferry events every frame
