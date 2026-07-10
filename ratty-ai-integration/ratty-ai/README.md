# ratty-ai

Pure CLI control for [Ratty](https://github.com/orhun/ratty) terminal emulator via OSC escape sequences.

**No sockets. No daemons. No temp files. Just stdout.**

## How It Works

`ratty-ai` prints OSC 777 escape sequences to stdout. Ratty intercepts them and acts. The user sees nothing.

```
┌─────────────┐      stdout (OSC)      ┌─────────────┐
│   ratty-ai  │  ─────────────────────►  │    Ratty    │
│  (AI runs)  │                          │  (intercepts│
│             │                          │   & acts)   │
└─────────────┘                          └─────────────┘
```

## Installation

```bash
cargo install --git https://github.com/orhun/ratty ratty-ai
```

## Commands

```bash
# Mode control
ratty-ai mode 3d
ratty-ai mode 2d
ratty-ai mode mobius

# 3D Objects
ratty-ai object add --path rat.obj --x 10 --y 5 --spin 2.0
ratty-ai object remove --id 1
ratty-ai object clear
ratty-ai object update --id 1 --spin 5.0 --brightness 2.0

# Visual feedback
ratty-ai flash --color "#00ff00" --duration 0.3
ratty-ai pulse --intensity 0.8 --duration 2.0
ratty-ai tint "#0000ff" --opacity 0.1

# Cursor
ratty-ai cursor --model rat.obj --spin 3.0 --brightness 0.8
ratty-ai cursor --visible false

# Warp
ratty-ai warp 0.5

# Data viz
ratty-ai chart --data '[1,5,3,8]' --x 10 --y 5
git log --oneline -20 | ratty-ai timeline --x 5 --y 10

# Reset
ratty-ai reset
```

## Pipe-Friendly

```bash
make && ratty-ai flash --color green || ratty-ai flash --color red
tail -f /var/log/syslog | grep ERROR | ratty-ai pulse --color red
```

## Protocol

```
ESC ] 777 ; ratty : <action> ; <url-encoded-payload> BEL




ratty-ai-v2/
├── ratty-ai/                          # CLI crate (AI installs this)
│   ├── Cargo.toml
│   ├── README.md
│   └── src/
│       └── main.rs                    # 500+ lines, all commands
│
├── ratty-patch/                       # Add to Ratty
│   └── src/
│       ├── terminal/
│       │   └── osc_parser.rs          # OSC 777 → 35+ event variants
│       └── rgp/
│           └── bevy_handler.rs        # Bevy systems for every command
│
└── docs/


##### Advanced CLI



# ratty-ai v2

Enhanced CLI control for [Ratty](https://github.com/orhun/ratty) terminal emulator.

## New Commands

### Process Visualization
```bash
ratty-ai ps --visualize                    # Show processes as 3D orbs
ratty-ai ps --highlight 1234 --color red # Highlight specific PID
ratty-ai kill 1234 --effect explode        # Kill with visual effect
```

### File System as 3D Space
```bash
ratty-ai cd /home/user/projects --visualize  # Enter 3D directory
ratty-ai ls --visualize                      # Files as floating icons
ratty-ai tree --depth 3 --visualize          # Branching 3D structure
```

### Git Integration
```bash
ratty-ai git branch --visualize   # Branches as 3D rivers
ratty-ai git diff --visualize     # Before/after 3D comparison
ratty-ai git merge --visualize    # Conflict visualization
ratty-ai git stash --visualize    # Stash as compressed cube
```

### Network
```bash
ratty-ai net --visualize                  # All connections
ratty-ai net --visualize --host google.com # Specific host
```

### AI State
```bash
ratty-ai think --start          # AI thinking indicator
ratty-ai think --end
ratty-ai confidence 0.95        # Confidence aura
ratty-ai mood excited           # excited | cautious | confused | focused | celebratory
```

### Panes
```bash
ratty-ai split --direction vertical --ratio 0.3
ratty-ai focus 2
ratty-ai resize 1 --width 80
ratty-ai close 2
```

### History & Bookmarks
```bash
ratty-ai history --last 50 --visualize
ratty-ai bookmark --name "before-refactor"
ratty-ai jump --name "before-refactor"
```

### Collaboration
```bash
ratty-ai user join --name alice --color "#00ff00"
ratty-ai user cursor --name alice --x 10 --y 5
ratty-ai note "Check line 42" --x 15 --y 10 --expires 1h
```

### Sound
```bash
ratty-ai sound click
ratty-ai sound success
ratty-ai sound error
ratty-ai sound ambient --loop-sound
```

### AI Avatar
```bash
ratty-ai avatar set --model ai-helper.glb --position top-right
ratty-ai avatar gesture point
ratty-ai avatar speak "Build complete"
ratty-ai avatar hide
```

### Macros
```bash
ratty-ai macro record --name deploy
# ... run some ratty-ai commands ...
ratty-ai macro stop
ratty-ai macro play --name deploy
ratty-ai macro export --name deploy --to deploy.ratty
ratty-ai macro run deploy.ratty
```

### Reactive
```bash
ratty-ai react --on-cpu-high 90 --effect warp-intense
ratty-ai react --on-memory-high 80 --effect red-tint
ratty-ai react --on-battery-low 20 --effect dim-pulse
```

## Installation

```bash
cargo install --git https://github.com/orhun/ratty ratty-ai
```

## License

MIT





# AI starts working
ratty-ai think --start
ratty-ai mood focused

# User asks about processes
ratty-ai ps --visualize
ratty-ai confidence 0.85

# AI finds a process to kill
ratty-ai kill 1234 --effect explode
ratty-ai sound success

# User navigates git
ratty-ai git branch --visualize
ratty-ai mood excited

# AI celebrates success
ratty-ai avatar gesture celebrate
ratty-ai sound notify
ratty-ai flash --color green --duration 0.5
ratty-ai think --end

----
What Ratty Needs (Same 3 Steps)
1.  Add OSC 777 handler in VT parser → osc_parser.rs
2.  Bridge events to Bevy ECS
3.  Register plugin → bevy_handler.rs
The patch is larger now (more event variants + systems) but the integration pattern is identical. No new architecture — just more commands.

```

## License

MIT
