# ratty-ai Integration Guide

## What You Need to Add to Ratty

### 1. VT Parser Hook (~5 lines)

In your existing OSC handler (where you already handle `OSC 0` and `OSC 2` for title changes), add:

```rust
// src/terminal/osc.rs (or wherever your OSC handler lives)
use crate::osc_parser::{parse_osc_777, RattyAiCommand};

fn handle_osc(&mut self, params: &[u16], data: &str) {
    match params.get(0) {
        Some(0) | Some(2) => self.set_title(data),
        Some(777) => {
            if let Some(cmd) = parse_osc_777(data) {
                self.ai_event_sender.send(cmd);
            }
        }
        _ => {}
    }
}
```

### 2. Event Bridge (~10 lines)

Bridge from your terminal thread to Bevy's ECS:

```rust
// src/bridge.rs
use bevy::prelude::*;
use crate::osc_parser::RattyAiCommand;

pub struct TerminalToBevyBridge {
    pub receiver: crossbeam::channel::Receiver<RattyAiCommand>,
}

pub fn bridge_system(
    bridge: Res<TerminalToBevyBridge>,
    mut events: EventWriter<RattyAiCommand>,
) {
    while let Ok(cmd) = bridge.receiver.try_recv() {
        events.send(cmd);
    }
}
```

### 3. Plugin Registration (~3 lines)

```rust
// src/main.rs or your App builder
use ratty_ai_handler::RattyAiPlugin;

app.add_plugins(RattyAiPlugin);
```

That's it. No new threads, no sockets, no file descriptors.

## File Structure After Integration

```
ratty/
├── Cargo.toml
├── src/
│   ├── main.rs
│   ├── terminal/
│   │   ├── mod.rs
│   │   ├── vt_parser.rs          <-- add OSC 777 hook here
│   │   └── osc.rs                <-- existing title handler
│   ├── rgp/                      <-- existing Ratty Graphics Protocol
│   │   ├── mod.rs
│   │   └── widget.rs
│   ├── ai/                       <-- NEW: ratty-ai integration
│   │   ├── mod.rs
│   │   ├── osc_parser.rs         <-- parses OSC 777 into events
│   │   └── bevy_handler.rs       <-- Bevy systems that act on events
│   └── ...
```

## Testing

```bash
# In Ratty, run:
ratty-ai mode 3d --dry-run
# Should print: ESC ] 777 ; ratty : mode ; 3d BEL

# Then without dry-run:
ratty-ai mode 3d
# Ratty should switch to 3D mode immediately

ratty-ai warp 0.5
# Terminal should warp

ratty-ai flash --color "#ff0000" --duration 1.0
# Screen should flash red
```
