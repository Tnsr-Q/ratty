#!/usr/bin/env bash
# The soul, live: a small emotional arc you can watch.
#
# Requires `ratty-ai` on PATH (cargo install --path tools/ratty-ai) and to
# be run *inside* a ratty terminal, whose renderer intercepts the OSC 777
# output. The same commands drive the browser build through feed().
#
#   ratty -e tools/ratty-ai/examples/soul.sh
#
set -euo pipefail
beat() { sleep "${1:-0.9}"; }

ratty-ai mode 3d
ratty-ai warp 0.25
ratty-ai think --start
ratty-ai mood focused;                         beat 1.4
echo "  it wakes without being told to."
ratty-ai confidence 0.35;                      beat 1.2
echo "  it is not sure of anything yet."
ratty-ai confidence 0.6;                       beat 1.4
ratty-ai warp 0.45;                            beat 1.6
echo "  ...it turns the problem over in the dark behind the glass..."
ratty-ai pulse --intensity 0.85 --duration 1.2; beat 1.4   # a realization
echo "  — oh."
ratty-ai mood excited
ratty-ai confidence 0.92;                      beat 1.4
ratty-ai flash --color '#8a9a7b' --duration 0.5            # success green
echo "  the build is green. the test passes."
ratty-ai mood celebratory;                     beat 1.6
ratty-ai think --end
ratty-ai warp 0.25
ratty-ai mood focused
ratty-ai confidence 0.72;                      beat 1.6
echo "  then it returns to the quiet, keeping a warm color it cannot name."
beat 1.5
ratty-ai reset
