#!/usr/bin/env bash
# Context-aware auto-effects: the terminal reacts to what your shell is doing.
#
# Registers four reactive rules over the sensors that
# `shell/ratty-lifecycle.{zsh,bash}` publishes. Run this once per session,
# then source the hook and just use your shell — builds, tests, git, all of
# it drives the scene with no further commands.
#
#   ratty-ai ... requires being run INSIDE a ratty terminal.
#
#   ./tools/ratty-ai/examples/shell-effects.sh
#   source tools/ratty-ai/shell/ratty-lifecycle.zsh   # or .bash
#
# Nothing here needs a capability grant: sensors and rules are caller-owned
# and scoped to your own namespace by construction.
set -uo pipefail

TIMEOUT="${RATTY_TIMEOUT:-2000}"

command -v ratty-ai >/dev/null 2>&1 || {
  echo "ratty-ai is not on PATH — cargo install --path tools/ratty-ai" >&2
  exit 1
}

# ── Which namespace are we? ──────────────────────────────────────────────
#
# A rule may only reference `sys.*` or the caller's OWN `agent.<ns>.*`, but
# `sensor publish` takes a bare suffix and lets the terminal prefix it —
# so nothing has told us our ordinal. There is no `whoami` on the wire.
#
# So: publish a sensor, then read the roster back and see what the terminal
# called it. The full name carries the namespace we were never told.
#
# The retry is load-bearing, not defensive padding: the publish is a byte
# written to the tty and the terminal applies it on its next frame, so a
# query fired immediately after can legitimately answer before the sensor
# exists. Asking once made this example flaky on a real terminal.
ratty-ai sensor publish shell.probe 0 --ttl 30 >/dev/tty 2>/dev/null
NS=""
for _ in 1 2 3 4 5 6 7 8 9 10; do
  NS=$(ratty-ai --timeout "$TIMEOUT" query state.sensors 2>/dev/null \
       | tr ',' '\n' | sed -n 's/.*"name":"agent\.\([0-9][0-9]*\)\.shell\.probe".*/\1/p' | head -1)
  [ -n "$NS" ] && break
  sleep 0.2
done

if [ -z "${NS:-}" ]; then
  echo "could not determine this terminal's namespace." >&2
  echo "  are you running inside ratty? (state.sensors did not answer)" >&2
  exit 1
fi
echo "namespace: $NS"

rule() {
  local name="$1"; shift
  if ratty-ai --ack --timeout "$TIMEOUT" rule set --name "$name" --replace "$@"; then
    echo "  registered  $name"
  else
    echo "  !! REFUSED  $name (exit $?)" >&2
  fi
}

echo "registering rules over agent.$NS.shell.*"

# A command began. Cooldown keeps a burst of quick commands from strobing.
rule shell-start \
  --sensor "agent.$NS.shell.running" --above 0.5 --cooldown 0.4 \
  --do 'pulse;intensity=0.5&duration=0.35'

# Still running 20 seconds later — this is the "long-running command" feel,
# and `--debounce` is the whole mechanism: the condition must HOLD for 20s
# before it activates. No duration sensor and no timer needed.
rule shell-busy \
  --sensor "agent.$NS.shell.running" --above 0.5 --debounce 20 --cooldown 30 \
  --do 'mood;mood=focused'

# It worked.
rule shell-ok \
  --sensor "agent.$NS.shell.ok" --above 0.5 --cooldown 0.3 \
  --do 'flash;color=%238a9a7b&duration=0.3'

# It did not.
rule shell-fail \
  --sensor "agent.$NS.shell.fail" --above 0.5 --cooldown 0.3 \
  --do 'flash;color=%23cd3131&duration=0.45'

ratty-ai sensor remove shell.probe >/dev/tty 2>/dev/null || true

cat <<TEXT

Now source the hook and use your shell normally:

  source tools/ratty-ai/shell/ratty-lifecycle.zsh    # zsh
  source tools/ratty-ai/shell/ratty-lifecycle.bash   # bash

  true          # green flash
  false         # red flash
  sleep 25      # the mood turns focused while it runs

Inspect or undo:

  ratty-ai query state.rules --pretty
  ratty-ai query state.sensors --pretty
  for r in shell-start shell-busy shell-ok shell-fail; do ratty-ai rule remove "\$r"; done

Turn the hook off without unsourcing it: export RATTY_LIFECYCLE_OFF=1
TEXT
