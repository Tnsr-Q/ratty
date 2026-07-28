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
# ── Two modes ─────────────────────────────────────────────────────────────
#
# PRESENCE_DEMO_ACK=0 (default) — **visual-gate mode**. Mutations are
#   fire-and-forget: one OSC 777 write each, no round trip. This is the mode
#   to judge the scene in. Rejections are silent by design; the scene is the
#   instrument, and the error ring is read explicitly in Act 7.
#
# PRESENCE_DEMO_ACK=1 — **capture mode**. Every mutation is `--ack`ed, so
#   exit 0 means the terminal genuinely committed it. Slower and far more
#   round-trips, but it is the mode that proves the traffic for the relay.
#
# ── Failure policy (why this script does NOT `set -e`) ────────────────────
#
# Every ack and every 778 read is a round trip, and a round trip can time out
# for reasons that have nothing to do with what is being gated. Under `set -e`
# a single timeout mid-run kills the terminal and robs the human of every
# later act — which is exactly what happened on the first live attempt: the
# Act 3 read timed out at ~26s and Acts 4-8 never drew.
#
# So: no step is allowed to abort the run. Failures are announced loudly,
# counted, and re-reported in a summary at the end, and the script exits
# non-zero if the count is not zero. Honest failure, without destroying the
# live run it exists to serve.
#
# Known strain, so a timeout here is not mistaken for a gate failure: ratty
# answers 778 with a **blocking `write_all` on the Bevy main thread**
# (`src/runtime.rs:566`, `src/systems.rs:197`) into a PTY whose buffer is far
# smaller than the 4 KiB reply cap. #62 filed this against the relay's stdin;
# the first live run of this script reproduced it on a plain PTY with no
# relay in sight, which is a broader claim than #62 makes. Symptom: an
# occasional `unhandled terminal escape sequence: \x1b\` in ratty's log as
# leftover reply bytes echo back after the reader exits.
#
# ── Pacing ────────────────────────────────────────────────────────────────
#
# PRESENCE_DEMO_BEAT scales the narration pauses (default 1.0). The lease
# waits in Act 6 are deliberately *not* scaled — a TTL is wall-clock, so
# shortening them would only mean watching a row that has not expired yet.
#
#   PRESENCE_DEMO_BEAT=0.25 ratty -e tools/ratty-ai/examples/presence-demo.sh
#
set -uo pipefail        # deliberately not -e; see "Failure policy" above

BEAT_SCALE="${PRESENCE_DEMO_BEAT:-1.0}"
ACK_MODE="${PRESENCE_DEMO_ACK:-0}"
REPLY_TIMEOUT="${PRESENCE_DEMO_TIMEOUT:-5000}"

beat() { sleep "$(awk -v a="${1:-0.9}" -v s="$BEAT_SCALE" 'BEGIN{printf "%.2f", a*s}')"; }

FAILURES=0
fail() { FAILURES=$((FAILURES + 1)); echo "    !! $*"; }

# One presence mutation. Acked or not depending on the mode; never fatal.
# `ratty-ai` exit codes: 3 timeout, 4 malformed, 5 the terminal said ok=0,
# 6 transport — a rejection and a timeout are NOT the same thing and this
# never conflates them.
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

# One 778 read. Never fatal — a timeout here is the known strain above, not
# a verdict on anything this script is gating.
show() {
  local status
  ratty-ai --timeout "$REPLY_TIMEOUT" query "$1" --pretty
  status=$?
  case "$status" in
    0) ;;
    3) fail "778 read '$1' timed out — the known blocking-write strain, not a gate failure" ;;
    *) fail "778 read '$1' failed, exit $status" ;;
  esac
}

# A 48-byte id — exactly the terminal's `MAX_PRESENCE_ID_BYTES`. Legal here,
# and the reason the relay's mirror cannot rewrite ids naively.
SWARM_ID='swarm.agent.0123456789.0123456789.0123456789.abc'

# ── Preflight ────────────────────────────────────────────────────────────
#
# Ask the terminal whether it has a presence organ at all before spending
# seventy seconds pretending it does. A ratty built before 2026-07-26
# (commit 025954e, which added `src/presence.rs` and the `state.presence`
# op) answers every `ratty:user.*` write with nothing and draws nothing --
# an empty plane that looks exactly like a rendering defect and is not one.
# One live run was lost to a stale binary; that is what this costs one
# round trip to prevent.
#
# A caps read that fails outright is NOT treated as a missing organ -- that
# is the transport strain, and it must not block the demo.
preflight() {
  local caps status
  caps=$(ratty-ai --timeout "$REPLY_TIMEOUT" query caps 2>/dev/null)
  status=$?
  if [ "$status" -ne 0 ]; then
    echo "    (could not read caps, exit $status — continuing; if the plane"
    echo "     stays empty, suspect the binary before suspecting the renderer)"
    return 0
  fi
  case "$caps" in
    *'"state.presence"'*) return 0 ;;
  esac
  echo
  echo "── STOP: this ratty has no presence organ ───────────────────────────"
  echo
  echo "  \`caps\` does not advertise state.presence, so every user.* and note"
  echo "  write below would be ignored and the plane would stay empty. That"
  echo "  is a stale binary, not a gate failure — nothing here is judgeable."
  echo
  echo "  The organ landed 2026-07-26 (025954e). Rebuild and re-run:"
  echo
  echo "      cargo build --release"
  echo "      ./target/release/ratty -e tools/ratty-ai/examples/presence-demo.sh"
  echo
  exit 2
}
preflight

echo "── presence: the collaboration organ, live ──────────────────────────"
echo
echo "watch the plane. nothing on it yet."
emit mode 3d
emit warp 0.25
beat 1.6

# ── Act 1 ── carets and labels ───────────────────────────────────────────
echo
echo "[1] two participants join and report cursors."
echo "    WATCH: a caret per participant, each with its name label beside it,"
echo "           both in that participant's OWN color (teal / orange)."
emit user join alice --name "Alice W" --color '#00ffcc' --ttl 120
emit user join bob   --name "Bob"     --color '#ff8a3d' --ttl 120
beat 1.0
echo "    (joined, but nothing drawn yet — a participant with no reported"
echo "     cursor renders nothing. now the cursors:)"
beat 1.2
# Alice sits at (8,4) on purpose: that cell is under this script's own
# narration, so her label composites over live text while Bob's (30,9) and
# Carol's (52,9) land on empty plane. That contrast is the whole point --
# it is what surfaced the label-legibility finding on the first clean run,
# and moving her somewhere prettier would retire the only case that tests
# it. Keep her over the text.
emit user cursor alice -x 8  -y 4
emit user cursor bob   -x 30 -y 9
beat 2.0

echo
echo "    same carets through all three presentation modes."
echo "    WATCH: flat = screen-space; 3d/mobius = pinned to the warped"
echo "           surface, above it but under any RGP object."
emit mode flat;   beat 1.8
emit mode 3d;     beat 1.8
emit mode mobius; beat 2.2
emit mode 3d;     beat 1.4

# ── Act 2 ── replace clears the cursor ───────────────────────────────────
echo
echo "[2] alice re-joins with replace=true — a fresh join states complete"
echo "    new state, so the cursor CLEARS."
echo "    WATCH: alice's caret and label disappear; bob's stay."
beat 1.2
emit user join alice --name "Alice (renamed)" --color '#c56bff' \
  --ttl 120 --replace
beat 2.2
echo "    ...and back, in the new color."
echo "    WATCH: alice returns in purple, under the new name."
emit user cursor alice -x 8 -y 4
beat 2.2

# ── Act 3 ── clamp-to-edge ───────────────────────────────────────────────
echo
echo "[3] bob's cursor goes far off-grid (x=9999, y=9999)."
echo "    WATCH: his caret clamps to the nearest edge cell — bottom-right —"
echo "           rather than vanishing or drawing off-plane."
beat 1.2
emit user cursor bob -x 9999 -y 9999
beat 2.4
echo "    the stored value is untouched; only rendering clamps:"
show state.presence
beat 2.6
emit user cursor bob -x 30 -y 9
beat 1.4

# ── Act 4 ── note underlays ──────────────────────────────────────────────
echo
echo "[4] a note lands on the grid."
echo "    WATCH: a filled underlay panel behind the text with a thin accent"
echo "           border — the SAME border hue for every note, never the"
echo "           author's color (the wire carries no color on \`note\`)."
beat 1.2
emit note add n1 "REVIEW THIS LINE" -x 6 -y 14 --ttl 300
beat 2.4
echo "    replaced in place (same id, new text):"
emit note add n1 "REVIEWED - SHIPS TODAY" -x 6 -y 14 --ttl 300 --replace
beat 2.4

# ── Act 5 ── the long id ─────────────────────────────────────────────────
echo
echo "[5] a swarm agent joins under a 48-byte id (the cap, exactly)."
echo "    WATCH: the label truncates at 16 glyphs; the full id stays"
echo "           queryable. This is the row that makes the relay's id"
echo "           rewrite overflow — capture, not decoration."
beat 1.2
emit user join "$SWARM_ID" --name "SWARM AGENT SEVENTEEN" \
  --color '#8a9a7b' --ttl 120
emit user cursor "$SWARM_ID" -x 46 -y 4
beat 2.6

# ── Act 6 ── leases: the fresh→expired flip ──────────────────────────────
echo
echo "[6] leases. a short-TTL note and a short-TTL participant."
echo "    WATCH: both draw now, then disappear on their own — on an"
echo "           otherwise idle terminal. The flip itself must request the"
echo "           redraw; nothing here touches them again."
beat 1.2
emit note add n2 "THIS NOTE EXPIRES" -x 6 -y 17 --ttl 5
emit user join carol --name "Carol" --color '#ffd166' --ttl 5
emit user cursor carol -x 52 -y 9
echo "    (both fresh — 5s leases, counting down. hands off the keyboard.)"
sleep 3                     # deliberately unscaled: a TTL is wall-clock
echo "    ...2s left..."
sleep 4
echo
echo "    WATCH: carol's caret and the second note are GONE from the scene."
echo "           But they are not deleted — no sweep ever deletes a record."
echo "           Here they are, still queryable, honestly \`fresh: false\`:"
show state.presence
beat 3.0

echo
echo "    renew revives carol — the row was still there, honestly expired."
echo "    WATCH: her caret comes BACK at the cell she left it."
beat 1.2
emit user renew carol --ttl 120
beat 2.6

# ── Act 7 ── the error ring ──────────────────────────────────────────────
echo
echo "[7] one deliberately rejected command: joining an id that already"
echo "    exists, without replace=true."
echo "    WATCH: nothing on the scene changes. A rejection is caller-local."
beat 1.2
# Always acked regardless of mode: this is an assertion, not decoration, and
# without the ack there is no way to observe the rejection at all. Exit 5 is
# "the terminal answered ok=0" — the only code that proves a rejection. A
# timeout (3) is NOT a pass: it means the answer never arrived, so treating
# any non-zero exit as success here would let a dead round trip masquerade
# as a working error ring.
ratty-ai --ack --timeout "$REPLY_TIMEOUT" user join alice \
  --name "Impostor" --color '#ff0000'
reject_status=$?
case "$reject_status" in
  5) echo "    (rejected as expected: already-exists)" ;;
  0) fail "Act 7: the duplicate join COMMITTED. Expected already-exists." \
          "That is a defect, not a calibration — spawn a repair issue." ;;
  3) fail "Act 7: INCONCLUSIVE — the ack timed out, so the error ring was" \
          "never exercised. Transport strain, not a presence verdict." ;;
  *) fail "Act 7: INCONCLUSIVE — ratty-ai exited $reject_status, which is" \
          "neither a commit (0) nor a rejection (5)." ;;
esac
beat 1.0
echo "    and it landed in our own error ring:"
show state.errors
beat 3.0

# ── Act 8 ── teardown ────────────────────────────────────────────────────
echo
echo "[8] leaving. removals free their roster slot for real."
echo "    WATCH: each caret and note goes as its owner leaves."
beat 1.2
emit note remove n1
beat 0.8
emit note remove n2
beat 0.8
emit user leave carol
beat 0.8
emit user leave "$SWARM_ID"
beat 0.8
emit user leave bob
beat 0.8
emit user leave alice
beat 1.4

echo
echo "    the rosters, empty — left rows are gone for good, unlike expired ones:"
show state.presence
beat 2.0

echo
echo "── done. the scene is as we found it. ───────────────────────────────"
emit mode flat
emit warp 0.0

# Every act ran regardless of what failed along the way; the verdict lands
# here, once, where it cannot cost anyone the rest of the demo.
echo
if [ "$FAILURES" -eq 0 ]; then
  echo "all steps behaved. the scene above is the gate — judge it with your eyes."
  exit 0
fi
echo "$FAILURES step(s) did not behave — see the '!!' lines above."
echo "Round-trip timeouts are the known blocking-write strain (header), not a"
echo "verdict on presence rendering. Anything else is a real defect."
exit 1
