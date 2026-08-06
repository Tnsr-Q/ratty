#!/usr/bin/env bash
# Terminals on the wire, live: the full OSC 777 `ratty:term.*` family driven
# against a running terminal — the closed-loop 778 demo for M4.5 (#49).
#
# Requires `ratty-ai` on PATH (cargo install --path tools/ratty-ai) and to be
# run *inside* a ratty terminal, whose renderer intercepts the OSC 777 output.
#
#   ratty -e tools/ratty-ai/examples/terminals-demo.sh
#
# ── What this gates ───────────────────────────────────────────────────────
#
# The family IS the demo (#58's M4.5 exit criterion). Every act names the
# invariant it exercises, so a change that breaks one has an obvious
# reproducer:
#
#   Act 1  default DENY         → an ungranted spawn is refused, always acked
#   Act 2  the spawn handle     → ok=1, NO code=started, handle in data.id
#   Act 3  readiness            → state.terminals carries the row and its state
#   Act 4  the real grid        → cols/rows resize for real and read back
#   Act 5  the refused geometry → x=/y=/scale= answer `unsupported`
#   Act 6  the grid ceiling     → an absurd grid answers `bad-command`
#   Act 7  the separate gate    → focus is denied while lifecycle is granted
#   Act 8  the live cap         → spawning past max_live answers `terminal-cap`
#   Act 9  wire-unkillable      → closing terminal #1 answers `not-owner`
#   Act 10 the close            → the creator closes its creations; roster drains
#
# ── Two modes ─────────────────────────────────────────────────────────────
#
# TERMINALS_DEMO_ACK=0 (default) — **visual-gate mode**. Mutations are
#   fire-and-forget, so the scene is the instrument: watch terminals appear,
#   resize and vanish. Acts that PROVE a refusal still --ack regardless of
#   mode, because an unobserved rejection proves nothing at all.
#
# TERMINALS_DEMO_ACK=1 — **capture mode**. Every mutation is --acked, so
#   exit 0 with zero failures means the terminal genuinely committed each
#   step. This is the mode that proves the family end to end.
#
# ── Failure policy (why this script does NOT `set -e`) ────────────────────
#
# Every ack and every 778 read is a round trip, and a round trip can time
# out for reasons unrelated to what is being gated. Under `set -e` one
# timeout mid-run would kill the terminal and rob the human of every later
# act. So: no step aborts the run. Failures are announced, counted, and
# re-reported in a summary; the script exits non-zero only at the end.
#
# Known strain, so a timeout here is not mistaken for a gate failure: ratty
# answers 778 with a blocking write on the Bevy main thread into a PTY
# buffer smaller than the 4 KiB reply cap. **Exit 5 is the only code that
# proves a rejection** — 3 is a timeout, which is transport, never a
# verdict. Acts 1 and 9 are told apart by CODE alone (`not-permitted`
# vs `not-owner`), which is exactly why those two refusals stay distinct.
#
# ── Grants ────────────────────────────────────────────────────────────────
#
# Both capabilities default DENY. Act 1 proves that and is the only act
# that works on a stock config; everything after it needs, in ratty.toml:
#
#   [trust.local]
#   terminal_lifecycle = true
#   # terminal_focus stays false — Act 7 proves the split
#
set -uo pipefail        # deliberately not -e; see "Failure policy" above

BEAT_SCALE="${TERMINALS_DEMO_BEAT:-1.0}"
ACK_MODE="${TERMINALS_DEMO_ACK:-0}"
REPLY_TIMEOUT="${TERMINALS_DEMO_TIMEOUT:-5000}"

beat() { sleep "$(awk -v a="${1:-0.9}" -v s="$BEAT_SCALE" 'BEGIN{printf "%.2f", a*s}')"; }

FAILURES=0
fail() { FAILURES=$((FAILURES + 1)); echo "    !! $*"; }

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

# A command that MUST be refused. Always acked regardless of mode: an
# unobserved rejection proves nothing. Exit 5 is the only pass.
expect_refusal() {
  local what="$1"; shift
  local output status
  output=$(ratty-ai --ack --json --timeout "$REPLY_TIMEOUT" "$@" 2>&1)
  status=$?
  case "$status" in
    5) echo "    refused, as it must be — $what"; echo "    $output" ;;
    0) fail "$what was ACCEPTED; it must be refused" ;;
    3) fail "$what timed out — transport, not a verdict; rerun before believing it" ;;
    *) fail "$what exited $status (expected 5)" ;;
  esac
}

# One 778 read. Never fatal — a timeout here is the known strain, not a
# verdict on anything this script gates.
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

# Captures a spawn's handle from its ack payload. Echoes the handle on
# stdout, or nothing if the spawn did not commit.
spawn_capture() {
  local output status
  output=$(ratty-ai --ack --json --timeout "$REPLY_TIMEOUT" term spawn 2>/dev/null)
  status=$?
  if [ "$status" -ne 0 ]; then
    fail "term spawn exited $status (5 = refused; check trust.terminal_lifecycle)"
    return 1
  fi
  # The handle is data.id in the reply. Kept to sed so the demo has no jq
  # dependency; the shape is <hex>-<seq>, base64url-safe by construction.
  printf '%s' "$output" | sed -n 's/.*"id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1
}

# ── Preflight ────────────────────────────────────────────────────────────
#
# Ask the terminal whether it has a terminals organ at all before spending
# a minute pretending it does. A ratty built before M4.5 answers every
# `ratty:term.*` write with `unsupported` and spawns nothing — which looks
# exactly like a denied capability and is not one.
#
# A caps read that FAILS is not treated as a missing organ: that is the
# transport strain, and it must not block the demo. (That distinction is
# the gate-driver hazard this project has already been bitten by once.)
preflight() {
  local caps status
  caps=$(ratty-ai --timeout "$REPLY_TIMEOUT" query caps 2>/dev/null)
  status=$?
  if [ "$status" -ne 0 ]; then
    echo "    (could not read caps, exit $status — continuing; if nothing spawns,"
    echo "     suspect the binary before suspecting the organ)"
    return 0
  fi
  case "$caps" in
    *'"state.terminals"'*) ;;
    *)
      echo "    !! this ratty has no terminals organ (no state.terminals in caps)."
      echo "       You are running a binary from before M4.5. Rebuild and rerun;"
      echo "       every act below would otherwise answer 'unsupported' and look"
      echo "       like a denied capability."
      return 1
      ;;
  esac
  case "$caps" in
    *'"terminal_lifecycle":true'*|*'"terminal_lifecycle": true'*)
      echo "    trust.terminal_lifecycle is GRANTED — acts 2-10 will commit." ;;
    *)
      echo "    trust.terminal_lifecycle is DENIED (the default). Act 1 proves"
      echo "    that; acts 2-10 will be refused until you grant it in ratty.toml." ;;
  esac
  return 0
}

echo "── terminals on the wire (#49) ──────────────────────────────────────"
echo
preflight || exit 1
beat 1.2

echo
echo "── act 1: both gates default DENY ───────────────────────────────────"
echo "    A spawn without the grant must be refused, not ignored. Watch the"
echo "    code: not-permitted (a capability fact), NOT not-owner."
expect_refusal "an ungranted term.spawn" term spawn
beat 1.5

echo
echo "── act 2: the spawn handle ──────────────────────────────────────────"
echo "    ok=1 with the handle in data.id — and NO code=started. A terminal"
echo "    is not a long-running operation; readiness is the row's state."
FIRST=$(spawn_capture)
if [ -z "${FIRST:-}" ]; then
  echo
  echo "    No handle captured — the rest of the demo needs one."
  echo "    Grant [trust.local] terminal_lifecycle = true and rerun."
  echo
  echo "$FAILURES step(s) did not behave."
  exit 1
fi
echo "    handle: $FIRST"
beat 1.5

echo
echo "── act 3: the roster, and readiness ─────────────────────────────────"
echo "    Every terminal is listed, including the one you are typing in."
echo "    'creator' appears only on rows THIS terminal created."
show state.terminals
beat 2.0

echo
echo "── act 4: the grid is real ──────────────────────────────────────────"
echo "    cols/rows drive a genuine PTY resize on that seat alone."
emit term place --id "$FIRST" --cols 80 --rows 24
beat 1.0
show state.terminals
beat 2.0

echo
echo "── act 5: the geometry is refused ───────────────────────────────────"
echo "    x=/y=/scale= are the frozen wire shape, and nothing in this build"
echo "    renders them. An ok=1 here would be a lie; caps.terminals"
echo "    .place_fields says so before you try."
expect_refusal "term.place with x=" term place --id "$FIRST" --x 5
beat 1.5

echo
echo "── act 6: the grid ceiling ──────────────────────────────────────────"
echo "    A u16 is not a bound: a grid becomes a CPU-side image, so 65535"
echo "    columns would be tens of gigabytes from one escape sequence."
expect_refusal "an absurd grid" term place --id "$FIRST" --cols 65535 --rows 65535
beat 1.5

echo
echo "── act 7: focus is a SEPARATE capability ────────────────────────────"
echo "    Lifecycle granted does not grant keystroke redirection. If this"
echo "    is refused, that split is working; if it commits, you granted"
echo "    terminal_focus too — which is a real thing to grant deliberately."
ratty-ai --ack --json --timeout "$REPLY_TIMEOUT" term focus --id "$FIRST"
case $? in
  0) echo "    focus committed — terminal_focus is granted in your config." ;;
  5) echo "    refused — terminal_focus is denied, which is the default." ;;
  3) fail "focus timed out — transport, not a verdict" ;;
  *) fail "focus exited unexpectedly" ;;
esac
beat 1.5

echo
echo "── act 8: the live cap ──────────────────────────────────────────────"
echo "    max_live defaults to 4 and binds every spawn path — this chord's"
echo "    too. Spawning past it answers terminal-cap, never silence."
SECOND=$(spawn_capture)
beat 0.6
THIRD=$(spawn_capture)
beat 0.6
echo "    now at or near the cap; the next spawn should refuse:"
expect_refusal "a spawn past the live cap" term spawn
beat 1.5

echo
echo "── act 9: terminal #1 is wire-unkillable ────────────────────────────"
echo "    You are typing into it. It has no wire creator, so no wire caller"
echo "    can close it — not even from its own ingress. Watch the code:"
echo "    not-owner (an ownership fact), NOT not-permitted."
expect_refusal "closing the terminal we are typing in" term close
beat 2.0

echo
echo "── act 10: the creator closes its creations ─────────────────────────"
for handle in "$THIRD" "$SECOND" "$FIRST"; do
  [ -n "${handle:-}" ] || continue
  emit term close --id "$handle"
  beat 0.8
done
beat 1.0
echo
echo "    the roster, drained back to the seat you started in:"
show state.terminals
beat 2.0

echo
echo "── done. the scene is as we found it. ───────────────────────────────"
emit mode flat
emit warp 0.0

echo
if [ "$FAILURES" -eq 0 ]; then
  echo "all steps behaved. the scene above is the gate — judge it with your eyes."
  exit 0
fi
echo "$FAILURES step(s) did not behave — see the '!!' lines above."
echo "Round-trip timeouts are the known blocking-write strain (header), not a"
echo "verdict on terminal lifecycle. Anything else is a real defect."
exit 1
