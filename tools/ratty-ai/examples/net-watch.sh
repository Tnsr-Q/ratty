#!/usr/bin/env bash
# Network awareness, both halves, honestly labeled:
#
#   sys.network            (terminal-side)  link presence — an interface
#                          carries a routable address. Passive; the
#                          terminal sends nothing off-host. Needs
#                          `[reactive] system_sensors = true` in ratty.toml.
#
#   agent.<ns>.net.reachable  (this script)  the stronger claim — a probe
#                          actually answered. Egress happens ONLY because
#                          you ran this loop; that split is the collector
#                          doctrine applied to packets, and it is why the
#                          terminal does not probe for you.
#
# Registers a rule over each, then loops probing until interrupted:
#
#   ./tools/ratty-ai/examples/net-watch.sh
#
# Knobs: RATTY_PROBE_HOST (default 1.1.1.1), RATTY_PROBE_SECS (default 5).
set -uo pipefail

TIMEOUT="${RATTY_TIMEOUT:-2000}"
PROBE_HOST="${RATTY_PROBE_HOST:-1.1.1.1}"
PROBE_SECS="${RATTY_PROBE_SECS:-5}"

command -v ratty-ai >/dev/null 2>&1 || {
  echo "ratty-ai is not on PATH — cargo install --path tools/ratty-ai" >&2
  exit 1
}

# ── Namespace discovery (the shell-effects.sh pattern, same reasons) ────
# A rule may only reference the caller's own `agent.<ns>.*`, publishes take
# a bare suffix, and there is no whoami — so publish a probe sensor and
# read the full name back. The retry is load-bearing: a publish applies on
# the terminal's next frame.
ratty-ai sensor publish net.reachable 0 --ttl 30 >/dev/tty 2>/dev/null
NS=""
for _ in 1 2 3 4 5 6 7 8 9 10; do
  NS=$(ratty-ai --timeout "$TIMEOUT" query state.sensors 2>/dev/null \
       | tr ',' '\n' | sed -n 's/.*"name":"agent\.\([0-9][0-9]*\)\.net\.reachable".*/\1/p' | head -1)
  [ -n "$NS" ] && break
  sleep 0.2
done
if [ -z "$NS" ]; then
  echo "could not determine this terminal's namespace — are you inside ratty?" >&2
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

# Link vanished entirely (cable out, wifi off, airplane mode). sys.* needs
# no namespace and no grant beyond the config bit; if system_sensors is
# off this rule registers and sits dormant — rules over stale sensors
# never fire, they wait.
rule net-link-down \
  --sensor sys.network --below 50 --debounce 2 --cooldown 20 \
  --do 'tint;color=%23e5c07b&duration=1.2'

# Link up but the probe stopped answering (router up, ISP down) — the case
# link state cannot see, and the reason this loop exists.
rule net-unreachable \
  --sensor "agent.$NS.net.reachable" --below 0.5 --debounce 2 --cooldown 20 \
  --do 'flash;color=%23cd3131&duration=0.6'

# TTL 3× the interval, same shape as the native adapter's own sample TTL:
# if this loop dies, the sensor goes stale and the rule goes dormant —
# never a frozen "reachable" lie.
TTL=$((PROBE_SECS * 3))

# ping's reply-wait flag is platform-divergent: macOS -W takes
# MILLISECONDS, Linux takes SECONDS. `-W 2` on macOS is a 2 ms window —
# a slow-but-alive link would read as offline forever.
case "$(uname)" in
  Darwin) PING_WAIT=(-W 2000) ;;
  *) PING_WAIT=(-W 2) ;;
esac

echo "probing $PROBE_HOST every ${PROBE_SECS}s (ttl ${TTL}s) — ctrl-c to stop"
while :; do
  if ping -c 1 "${PING_WAIT[@]}" "$PROBE_HOST" >/dev/null 2>&1; then
    ratty-ai sensor publish net.reachable 1 --ttl "$TTL" >/dev/tty 2>/dev/null
  else
    ratty-ai sensor publish net.reachable 0 --ttl "$TTL" >/dev/tty 2>/dev/null
  fi
  sleep "$PROBE_SECS"
done
