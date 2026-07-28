#!/usr/bin/env bash
# Collaboration presence, live: the full OSC 777 `ratty:user.*` / `ratty:note*`
# family driven against a running terminal, paced so a human can watch each
# claim land.
#
# Requires `ratty-ai` on PATH (cargo install --path tools/ratty-ai) and to be
# run *inside* a ratty terminal, whose renderer intercepts the OSC 777 output.
# The same commands drive the browser build through feed().
#
#   ratty -e tools/ratty-ai/examples/presence-demo.sh
#
# ── This script has two jobs ──────────────────────────────────────────────
#
# 1. **The M3 close-out's presence gate** (#41 Gate 1). Every act echoes what
#    to look at before it emits, so the checklist is the transcript. The knobs
#    under judgment all live in `src/presence.rs:610-646`.
#
# 2. **The tier-B relay's requirements capture** (#47, #45's design). This is
#    the exact traffic `tools/ratty-relay` must carry end-to-end. Each act
#    below names the relay invariant it exercises, so a relay change that
#    breaks one has an obvious reproducer:
#
#      Act 2  `replace=true`           → the mirror emits replace on every
#                                        synthesized join/note (revision
#                                        continues, no `already-exists`)
#      Act 3  off-grid cursor          → clamp is renderer-side; the mirror
#                                        must pass the stored value untouched
#      Act 5  48-byte id               → `r<ns>.<id>` overflows the 48-byte cap
#                                        and must truncate + hash, not drop
#      Act 6  fresh→expired flip       → fresh-at-*emit* filtering; an expired
#                                        row must never reach a spectator
#      Act 6  renew revives            → a revived row reappears within one
#                                        poll interval
#      Act 7  rejection                → rejections are caller-local; nothing
#                                        about them crosses to a spectator
#      Act 8  leave / note.remove      → removals emit before additions so a
#                                        capped roster frees its slot first
#
# ── Pacing ────────────────────────────────────────────────────────────────
#
# PRESENCE_DEMO_BEAT scales the narration pauses (default 1.0). The lease
# waits in Act 6 are deliberately *not* scaled — a TTL is wall-clock, so
# shortening them would only mean watching a row that has not expired yet.
#
#   PRESENCE_DEMO_BEAT=0.25 ratty -e tools/ratty-ai/examples/presence-demo.sh
#
set -euo pipefail

BEAT_SCALE="${PRESENCE_DEMO_BEAT:-1.0}"
beat() { sleep "$(awk -v a="${1:-0.9}" -v s="$BEAT_SCALE" 'BEGIN{printf "%.2f", a*s}')"; }

# A 48-byte id — exactly the terminal's `MAX_PRESENCE_ID_BYTES`. Legal here,
# and the reason the relay's mirror cannot rewrite ids naively.
SWARM_ID='swarm.agent.0123456789.0123456789.0123456789.abc'

# Every mutation is `--ack`ed: exit 0 means the terminal committed it, so
# `set -e` turns any unexpected rejection into a failed run rather than a
# demo that quietly drew nothing.

echo "── presence: the collaboration organ, live ──────────────────────────"
echo
echo "watch the plane. nothing on it yet."
ratty-ai --ack mode 3d
ratty-ai --ack warp 0.25
beat 1.6

# ── Act 1 ── carets and labels ───────────────────────────────────────────
echo
echo "[1] two participants join and report cursors."
echo "    WATCH: a caret per participant, each with its name label beside it,"
echo "           both in that participant's OWN color (teal / orange)."
ratty-ai --ack user join alice --name "Alice W" --color '#00ffcc' --ttl 120
ratty-ai --ack user join bob   --name "Bob"     --color '#ff8a3d' --ttl 120
beat 1.0
echo "    (joined, but nothing drawn yet — a participant with no reported"
echo "     cursor renders nothing. now the cursors:)"
beat 1.2
ratty-ai --ack user cursor alice -x 8  -y 4
ratty-ai --ack user cursor bob   -x 30 -y 9
beat 2.0

echo
echo "    same carets through all three presentation modes."
echo "    WATCH: flat = screen-space; 3d/mobius = pinned to the warped"
echo "           surface, above it but under any RGP object."
ratty-ai --ack mode flat;   beat 1.8
ratty-ai --ack mode 3d;     beat 1.8
ratty-ai --ack mode mobius; beat 2.2
ratty-ai --ack mode 3d;     beat 1.4

# ── Act 2 ── replace clears the cursor ───────────────────────────────────
echo
echo "[2] alice re-joins with replace=true — a fresh join states complete"
echo "    new state, so the cursor CLEARS."
echo "    WATCH: alice's caret and label disappear; bob's stay."
beat 1.2
ratty-ai --ack user join alice --name "Alice (renamed)" --color '#c56bff' \
  --ttl 120 --replace
beat 2.2
echo "    ...and back, in the new color."
echo "    WATCH: alice returns in purple, under the new name."
ratty-ai --ack user cursor alice -x 8 -y 4
beat 2.2

# ── Act 3 ── clamp-to-edge ───────────────────────────────────────────────
echo
echo "[3] bob's cursor goes far off-grid (x=9999, y=9999)."
echo "    WATCH: his caret clamps to the nearest edge cell — bottom-right —"
echo "           rather than vanishing or drawing off-plane."
beat 1.2
ratty-ai --ack user cursor bob -x 9999 -y 9999
beat 2.4
echo "    the stored value is untouched; only rendering clamps:"
ratty-ai query state.presence --pretty
beat 2.6
ratty-ai --ack user cursor bob -x 30 -y 9
beat 1.4

# ── Act 4 ── note underlays ──────────────────────────────────────────────
echo
echo "[4] a note lands on the grid."
echo "    WATCH: a filled underlay panel behind the text with a thin accent"
echo "           border — the SAME border hue for every note, never the"
echo "           author's color (the wire carries no color on \`note\`)."
beat 1.2
ratty-ai --ack note add n1 "REVIEW THIS LINE" -x 6 -y 14 --ttl 300
beat 2.4
echo "    replaced in place (same id, new text):"
ratty-ai --ack note add n1 "REVIEWED - SHIPS TODAY" -x 6 -y 14 --ttl 300 --replace
beat 2.4

# ── Act 5 ── the long id ─────────────────────────────────────────────────
echo
echo "[5] a swarm agent joins under a 48-byte id (the cap, exactly)."
echo "    WATCH: the label truncates at 16 glyphs; the full id stays"
echo "           queryable. This is the row that makes the relay's id"
echo "           rewrite overflow — capture, not decoration."
beat 1.2
ratty-ai --ack user join "$SWARM_ID" --name "SWARM AGENT SEVENTEEN" \
  --color '#8a9a7b' --ttl 120
ratty-ai --ack user cursor "$SWARM_ID" -x 46 -y 4
beat 2.6

# ── Act 6 ── leases: the fresh→expired flip ──────────────────────────────
echo
echo "[6] leases. a short-TTL note and a short-TTL participant."
echo "    WATCH: both draw now, then disappear on their own — on an"
echo "           otherwise idle terminal. The flip itself must request the"
echo "           redraw; nothing here touches them again."
beat 1.2
ratty-ai --ack note add n2 "THIS NOTE EXPIRES" -x 6 -y 17 --ttl 5
ratty-ai --ack user join carol --name "Carol" --color '#ffd166' --ttl 5
ratty-ai --ack user cursor carol -x 52 -y 9
echo "    (both fresh — 5s leases, counting down. hands off the keyboard.)"
sleep 3                     # deliberately unscaled: a TTL is wall-clock
echo "    ...2s left..."
sleep 4
echo
echo "    WATCH: carol's caret and the second note are GONE from the scene."
echo "           But they are not deleted — no sweep ever deletes a record."
echo "           Here they are, still queryable, honestly \`fresh: false\`:"
ratty-ai query state.presence --pretty
beat 3.0

echo
echo "    renew revives carol — the row was still there, honestly expired."
echo "    WATCH: her caret comes BACK at the cell she left it."
beat 1.2
ratty-ai --ack user renew carol --ttl 120
beat 2.6

# ── Act 7 ── the error ring ──────────────────────────────────────────────
echo
echo "[7] one deliberately rejected command: joining an id that already"
echo "    exists, without replace=true."
echo "    WATCH: nothing on the scene changes. A rejection is caller-local."
beat 1.2
if ratty-ai --ack user join alice --name "Impostor" --color '#ff0000'; then
  echo "    !! FAILED: expected 'already-exists' and the command committed."
  echo "       That is a defect, not a calibration — spawn a repair issue."
  exit 1
fi
echo "    (rejected as expected: already-exists)"
beat 1.0
echo "    and it landed in our own error ring:"
ratty-ai query state.errors --pretty
beat 3.0

# ── Act 8 ── teardown ────────────────────────────────────────────────────
echo
echo "[8] leaving. removals free their roster slot for real."
echo "    WATCH: each caret and note goes as its owner leaves."
beat 1.2
ratty-ai --ack note remove n1
beat 0.8
ratty-ai --ack note remove n2
beat 0.8
ratty-ai --ack user leave carol
beat 0.8
ratty-ai --ack user leave "$SWARM_ID"
beat 0.8
ratty-ai --ack user leave bob
beat 0.8
ratty-ai --ack user leave alice
beat 1.4

echo
echo "    the rosters, empty — left rows are gone for good, unlike expired ones:"
ratty-ai query state.presence --pretty
beat 2.0

echo
echo "── done. the scene is as we found it. ───────────────────────────────"
ratty-ai --ack mode flat
ratty-ai --ack warp 0.0
