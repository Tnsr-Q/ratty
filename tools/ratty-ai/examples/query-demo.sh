#!/usr/bin/env bash
# The closed loop, live: write the scene over OSC 777, read it back over
# OSC 778 as structured JSON.
#
# Requires `ratty-ai` on PATH (cargo install --path tools/ratty-ai) and to
# be run *inside* a ratty terminal. The same queries drive the browser
# build through RattySession.query().
#
#   ratty -e tools/ratty-ai/examples/query-demo.sh
#
set -euo pipefail

echo "what does this terminal speak?"
ratty-ai query caps --pretty
sleep 1.5

echo
echo "write: a mouse on the desk (acked — exit 0 means it committed)"
ratty-ai --ack object add --id 2147483649 --path SkateMouse.stl --x 20 --y 8 --spin 1.5
ratty-ai --ack mode 3d

echo
echo "read it back:"
ratty-ai query state.objects --pretty
sleep 1.5

echo
echo "who else is on screen, near our mouse?"
ratty-ai query state.neighbors --data '{"object": 2147483649, "radius": 20}' --pretty
sleep 1.5

echo
echo "the scene, as any agent may see it:"
ratty-ai state scene --pretty
sleep 1.5

echo
echo "a rejected write shows up in our error ring:"
ratty-ai --ack object add --id 2147483649 --path SkateMouse.stl --x 5 --y 5 \
  || echo "  (rejected as expected: already-exists)"
ratty-ai query state.errors --pretty

sleep 2
ratty-ai reset
