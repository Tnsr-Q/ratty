# `ratty-ai` 🐀

Pure-CLI control for the [Ratty](../../README.md) terminal emulator. Every
command is one escape sequence written to your terminal — no daemon, no
socket, no port. If a shell can run it, an agent can drive Ratty's 3D scene
with it.

```bash
ratty-ai mode 3d                      # tilt the plane into 3D
ratty-ai warp 0.4                     # bend it
ratty-ai flash --color '#7e9cd8'      # flash the surface
ratty-ai query caps --pretty          # ask what this build supports
```

Commands ride **OSC 777**; replies come back over **OSC 778**. The crate
shares the terminal's own parser verbatim through a `#[path]` include, so
the CLI and the terminal can never disagree about the wire format — which
is also why it is `publish = false` and installs from the repo.

## Install

```bash
cargo install --path tools/ratty-ai
```

## Try it without sending anything

`--dry-run` prints the sequence instead of emitting it. It is the honest way
to learn the surface, and it works for every command:

```bash
$ ratty-ai --dry-run mode 3d
ESC]777;ratty:mode;3dBEL
```

## Reading state

`query` reads live state over OSC 778; `state` is sugar for `query state.*`.
**Start with `caps`** — it tells you what this build actually supports,
including which capabilities are granted, so you never have to guess:

```bash
ratty-ai query caps --pretty          # the honest inventory
ratty-ai state                        # sugar for state.scene
ratty-ai state objects                # sugar for state.objects
ratty-ai query state.terminals --pretty
```

## Acks and exit codes

By default a command is fire-and-forget. `--ack` waits for the terminal's
verdict, which is what you want in a script:

```bash
ratty-ai --ack --json term spawn      # exit 0 committed, 5 refused
```

| Code | Meaning |
| --- | --- |
| `0` | committed (or the reply arrived) |
| `2` | bad input — never reached the wire |
| `3` | timeout waiting for a reply |
| `4` | malformed reply |
| `5` | the terminal answered `ok=0` — **it refused** |
| `6` | tty/transport failure |

`5` is a real answer, not a failure of the tool: the terminal considered the
command and said no. `--json` makes failures machine-readable as
`{"ok":false,"code","message"}` without changing exit codes. `query` and
`state` always read a reply, so `--ack` is ignored there.

## The families

| Family | Commands |
| --- | --- |
| Scene | `mode` `warp` `reset` `bookmark` `jump` |
| Effects | `flash` `pulse` `tint` |
| Objects | `object add\|remove\|clear\|update` `cursor` |
| Data viz | `chart` `ps` `fs` `git` `net` `kill` `history` |
| AI presence | `think` `confidence` `mood` `avatar` |
| Sound | `sound play\|ambient` |
| Collaboration | `user join\|renew\|cursor\|leave` `note add\|remove` |
| Reactive | `rule set\|remove\|enable\|disable` `sensor publish` |
| Macros | `macro record\|stop\|play` |
| Terminals | `term spawn\|place\|focus\|close` |
| Reads | `query` `state` |

Every subcommand has `--help`, and it is authoritative — this table is a map,
not a specification.

## Collectors gather; the wire carries data

`ps`, `fs`, `git`, `net`, `history` and `kill` are **collectors**: the CLI
gathers under *your* permissions and publishes typed data as a `viz.set`
snapshot. The terminal never introspects the host on wire command — there is
no `ps` verb, and no `kill` verb, on the wire at all.

```bash
ratty-ai ps --top 12                  # a ps.v1 snapshot
ratty-ai git                          # a git.v1 snapshot
ratty-ai history --last 50            # your shell history as timeline.v1
```

This is the inversion that makes the surface safe to expose to an agent:
untrusted bytes cannot ask the terminal to read your processes, your
filesystem or your shell history. They can only ask it to *draw* data that a
trusted collector already gathered.

## Your shell, driving the scene

The reactive organ watches **sensors** and fires **rules** — but nothing
shipped that published what your shell was doing. `shell/` does:

```bash
./tools/ratty-ai/examples/shell-effects.sh          # register the rules, once
source tools/ratty-ai/shell/ratty-lifecycle.zsh     # or .bash
```

Then just use the shell. A command that fails flashes red, one that succeeds
flashes green, and a build still running after twenty seconds turns the mood
focused. Nothing else to type.

Three sensors carry it, published into your terminal's own namespace:

| Sensor | Meaning |
| --- | --- |
| `shell.running` | `1` while a command runs, `0` at the prompt |
| `shell.ok` | pulses to `1` when a command exits zero |
| `shell.fail` | pulses to `1` when a command exits non-zero |

**Only those three numbers leave.** The hook sees your command line — zsh
hands it to `preexec` — and never sends it, or any part of it, anywhere. A
sensor carries a name and a float; there is no wire shape that could carry
the text even if it wanted to.

`ok` and `fail` are separate sensors rather than one exit code because rules
activate on a threshold **crossing**. A value that stays put fires once and
goes quiet, so two consecutive failures need two edges — which is what the
reset at `preexec` is for. For the same reason "long-running command" needs
no duration sensor and no timer: `--debounce 20` on `shell.running` already
means *held for twenty seconds*.

Costs, honestly: five short-lived processes per command, on the path that
runs before every prompt. That sits inside the terminal's 16/sec sensor
budget for normal use; a tight loop of fast commands can outrun it, and the
terminal then answers `rate-limited` and drops the sample rather than lying.
`export RATTY_LIFECYCLE_OFF=1` disables the hook without unsourcing it.

To extend it, publish your own sensors from the same hooks — classifying
commands (a `shell.danger` that pulses before an `rm -rf`), timing them, or
counting them. Any name you like: the terminal prefixes `agent.<ns>.` and
registers it on first use, with no wire change and no capability grant.

## Terminals on the wire

`term.*` spawns and addresses real terminals, and both of its capabilities
**default to DENY**. Check before you expect anything to commit:

```bash
ratty-ai query caps --pretty | grep terminal   # trust.terminal_lifecycle / _focus
```

Grant them in `ratty.toml` under `[trust.local]`. `terminal_focus` is
separate from `terminal_lifecycle` on purpose: focus is the keystroke-capture
primitive, so being allowed to *create* a terminal is not being allowed to
*aim the user's keyboard* at one.

`term spawn` takes no arguments at all — the wire can never choose a command,
a working directory, a position or a grid. Capture the handle from the ack
and address the rest by it:

```bash
id=$(ratty-ai --ack --json term spawn | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
ratty-ai --ack term place --id "$id" --cols 80 --rows 24
ratty-ai --ack term close --id "$id"
```

Only a terminal's creator may close it, and terminals with no wire creator —
the boot terminal, ones you spawned by hand, orphans — refuse every close.
See [`protocols/terminals.md`](../../protocols/terminals.md), and
[`examples/terminals-demo.sh`](examples/terminals-demo.sh) for the whole
family driven end to end.

## What is refused, and why

Some subcommands exist and cannot succeed. That is deliberate — the shapes
stay visible so the refusal is discoverable rather than mysterious:

- **`split`, `focus`, `resize`, `close`** emit the frozen `pane.*` verbs and
  are refused with `unsupported`. Panes were superseded by placement, not
  splits — use `term.*` instead. (The frozen wire shapes stay exactly as
  committed; nothing new is built against them.)
- **`macro export` / `macro run`** are refused: the wire never reads or
  writes a filesystem path. Their own `--help` says so.
- **`term place`** accepts `--cols`/`--rows`, which are live, and refuses
  `x`/`y`/`scale`, which nothing in this build renders. `caps.terminals
  .place_fields` tells you which is which before you try.

When a command is refused you get exit `5` and a stable code — `not-owner`,
`not-permitted`, `unsupported`, `terminal-cap` — never silence.

## Protocol reference

The wire contracts live in [`protocols/`](../../protocols/):
[`query.md`](../../protocols/query.md) ·
[`terminals.md`](../../protocols/terminals.md) ·
[`viz.md`](../../protocols/viz.md) ·
[`avatar.md`](../../protocols/avatar.md) ·
[`reactive.md`](../../protocols/reactive.md) ·
[`sound.md`](../../protocols/sound.md) ·
[`presence.md`](../../protocols/presence.md) ·
[`macros.md`](../../protocols/macros.md) ·
[`graphics.md`](../../protocols/graphics.md)

## License

MIT, same as Ratty.
