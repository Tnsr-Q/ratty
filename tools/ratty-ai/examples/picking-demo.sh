#!/usr/bin/env bash
# Cell picking in 3D, live: the M4.6 gate driver (#58) — the at-screen
# feel check choreographed act by act, with the wire half closed-loop
# over OSC 777/778 (mode and warp changes land, and state.scene reads
# them back committed).
#
# Requires `ratty-ai` on PATH (cargo install --path tools/ratty-ai) and to
# be run *inside* a ratty terminal:
#
#   ratty -e tools/ratty-ai/examples/picking-demo.sh
#
# ── What this gates ───────────────────────────────────────────────────────
#
# Picking is pointer-driven, so the mouse half of every act is HUMAN
# judgment — this script stages each scene over the wire, proves the stage
# committed over 778, tells the human exactly what to try, and counts
# their verdicts. The #58 exit criteria, act by act:
#
#   Act 1  pick accuracy, Plane3d   → selection starts under the pointer
#   Act 2  pick accuracy, warped    → the highlight tracks the pointer
#                                     across the animated bulge (the mesh
#                                     IS the chart)
#   Act 3  drag clamp               → dragging off the plane clamps to the
#                                     edge cell and never drops the drag
#   Act 4  the Möbius far half      → a glyph on the away-facing half
#                                     picks (RayCastBackfaces always-on)
#   Act 5  hit-vs-miss              → void drags orbit/pan, void wheel
#                                     zooms; wheel OVER the surface
#                                     scrolls scrollback instead
#   Act 6  no focus from a miss     → clicking the void never steals the
#                                     keyboard
#
# ── The stale-binary hazard (read before believing a failure) ─────────────
#
# A `target/release/ratty` built before M4.6 renders this scene perfectly
# and just… rotates when you click the plane — mimicking a picking defect.
# REBUILD RELEASE FIRST (gate-driver lesson, M3.11): the preflight below
# can prove the wire is alive, but no wire read can prove the mouse code
# vintage. If Act 1 fails, check the binary date before filing anything.
#
# ── Failure policy ────────────────────────────────────────────────────────
#
# Deliberately no `set -e` (relay-demo lesson): every 778 read is a round
# trip that can time out for transport reasons. No step aborts the run;
# failures are counted and summarized, exit non-zero only at the end.
# Exit 5 from ratty-ai is the only code that proves a rejection; 3 is a
# timeout — transport, never a verdict.
#
# ── Grants ────────────────────────────────────────────────────────────────
#
# Scene stage writes (mode/warp) ride the same trust config as every RGP
# `c`: on a stock local config they are user-adjacent and permitted; if
# your config locks the stage, grant it before running.
#
set -uo pipefail

BEAT_SCALE="${PICKING_DEMO_BEAT:-1.0}"
ACK_MODE="${PICKING_DEMO_ACK:-0}"
REPLY_TIMEOUT="${PICKING_DEMO_TIMEOUT:-5000}"

beat() { sleep "$(awk -v a="${1:-0.9}" -v s="$BEAT_SCALE" 'BEGIN{printf "%.2f", a*s}')"; }

FAILURES=0
fail() { FAILURES=$((FAILURES + 1)); echo "    !! $*" >&2; }
note() { echo "    $*" >&2; }
act()  { echo; echo "== $*" >&2; }

# One mutation, acked or not depending on the mode; never fatal.
emit() {
  local status
  if [ "$ACK_MODE" = "1" ]; then
    ratty-ai --ack --timeout "$REPLY_TIMEOUT" "$@"
    status=$?
    case "$status" in
      0) ;;
      5) fail "REJECTED by the terminal: ratty-ai $*" ;;
      3) fail "ack timed out — transport, not a rejection: ratty-ai $*" ;;
      *) fail "ratty-ai exited $status on: ratty-ai $*" ;;
    esac
  else
    ratty-ai "$@" || fail "could not emit: ratty-ai $*"
  fi
}

# One 778 read. Never fatal — a timeout is the known blocking-write
# strain, not a verdict on anything this script gates.
show() {
  local status
  ratty-ai --timeout "$REPLY_TIMEOUT" query "$1" --pretty
  status=$?
  case "$status" in
    0) ;;
    3) fail "778 read '$1' timed out — transport, not a gate failure" ;;
    *) fail "778 read '$1' failed, exit $status" ;;
  esac
}

# One human verdict. Reads /dev/tty so the pipeline never eats the
# answer; with no tty (a headless choreography run) the act is announced
# and the verdict skipped — feel checks cannot be autographed.
CAN_ASK=1
[ -r /dev/tty ] || CAN_ASK=0
ask() {
  if [ "$CAN_ASK" = "0" ]; then
    note "(no tty — verdict skipped: $*)"
    return 0
  fi
  local answer
  read -r -p "    ?? $* [y/n] " answer < /dev/tty
  case "$answer" in
    y | Y | yes) note "confirmed" ;;
    *) fail "human verdict: $*" ;;
  esac
}

# ── Preflight ─────────────────────────────────────────────────────────────
act "Preflight: the 778 channel answers, and you rebuilt release, right?"
show caps
note "(a pre-M4.6 binary passes this preflight and still rotates on click"
note " — the binary's build date is the only witness for the mouse code)"
beat

# ── Act 1: pick accuracy, Plane3d ────────────────────────────────────────
act "Act 1 — Plane3d pick accuracy"
seq 1 200            # scrollback + glyphs to aim at
emit mode 3d
show state.scene
note "Click-drag ACROSS A WORD on the plane. The highlight must begin at"
note "the exact glyph you pressed and follow the pointer cell by cell."
ask "does the selection start under the pointer and track it exactly"

# ── Act 2: pick accuracy under animated warp ─────────────────────────────
act "Act 2 — the warped chart (the mesh IS the chart)"
emit warp 1.0
show state.scene
note "The plane is bulging and pulsing. Drag a selection straight across"
note "the bulge. The highlight must stay under the pointer on the curved"
note "surface — no drift as the pulse animates."
ask "does the highlight track the pointer across the animated bulge"

# ── Act 3: the drag clamp ────────────────────────────────────────────────
act "Act 3 — drag off the surface (the infinite-plane clamp)"
note "Start a drag INSIDE the plane, then pull the pointer far off its"
note "edge. The selection must clamp to the edge column/row — never"
note "vanish, never jump. Release off-plane: the drag ends cleanly."
ask "does the drag clamp hold off the edge"

# ── Act 4: the Möbius far half ───────────────────────────────────────────
act "Act 4 — the Möbius far half (RayCastBackfaces always-on)"
emit warp 0.4
emit mode mobius
beat 1.4             # the morph runs ~1.1s; land after it
show state.scene
note "Find the half of the strip that faces AWAY (its text reads"
note "mirrored). Click-drag a selection on it."
ask "does the far half pick, exactly under the pointer"

# ── Act 5: hit-vs-miss is the discriminator ──────────────────────────────
act "Act 5 — content vs camera"
note "1) Drag the VOID around the strip: orbit. Right-drag void: pan."
note "2) Wheel over the VOID: zoom."
note "3) Wheel over the STRIP: scrollback scrolls (no zoom)."
ask "do void gestures drive the camera while surface wheel scrolls"

# ── Act 6: no focus from a missing pick ──────────────────────────────────
act "Act 6 — clicking the void never steals the keyboard"
note "Click the empty void, then type a few characters. They must land"
note "in this terminal exactly as before the click."
ask "did typing survive the void click untouched"

# ── Reset and summary ────────────────────────────────────────────────────
act "Reset"
emit warp 0.0
emit mode 2d
show state.scene

echo
if [ "$FAILURES" -gt 0 ]; then
  echo "== picking demo: $FAILURES failure(s) — see the !! lines above" >&2
  exit 1
fi
echo "== picking demo: every act confirmed" >&2
