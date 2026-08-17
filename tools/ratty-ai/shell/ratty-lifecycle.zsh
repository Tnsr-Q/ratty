# Shell-lifecycle sensors for ratty (zsh).
#
#   source /path/to/ratty/tools/ratty-ai/shell/ratty-lifecycle.zsh
#
# Publishes three caller-owned sensors into your terminal's own namespace,
# which reactive rules can then watch (see examples/shell-effects.sh):
#
#   shell.running   1 while a command runs, 0 at the prompt
#   shell.fail      pulses to 1 when a command exits non-zero
#   shell.ok        pulses to 1 when a command exits zero
#
# ONLY THESE THREE NUMBERS LEAVE. The hook reads your command line (zsh
# hands it to `preexec`) and never sends it, or any part of it, anywhere —
# sensors carry a name and a float, and there is no wire shape that could
# carry the text even if this wanted to.
#
# Why `ok` and `fail` are separate sensors rather than one exit code: rules
# activate on a THRESHOLD CROSSING, so a value that stays put fires once and
# then never again. Two consecutive failures must produce two edges, which
# means something has to return to 0 in between — that is the `preexec`
# reset below. A single `shell.exit` holding 1 across both failures would
# fire on the first and go quiet on the second.

# One publish is one short-lived process writing one escape sequence to the
# tty. Five per command sits inside the terminal's 16/sec sensor budget for
# normal use; a tight loop of fast commands can outrun it, and the terminal
# then answers `rate-limited` and drops the sample rather than lying.
_ratty_publish() {
  # Never let a sensor failure touch the prompt: no output, no exit status.
  # `>/dev/tty`, NOT `>/dev/null`. Without `--ack` ratty-ai writes the escape
  # sequence to STDOUT, so redirecting stdout to /dev/null does not silence
  # the command — it cancels it, and the hook goes quietly dead. Sending it
  # to the controlling terminal is what both reaches ratty AND keeps the
  # bytes out of whatever the user is capturing. `--ack` would also reach
  # the tty, but it blocks waiting for a reply, which is intolerable on a
  # path that runs before every prompt.
  command ratty-ai sensor publish "$1" "$2" --ttl "$3" >/dev/tty 2>/dev/null || true
}

_ratty_preexec() {
  [[ -n ${RATTY_LIFECYCLE_OFF:-} ]] && return 0
  _ratty_typeset_guard=1
  # TTL 3600: `running` must outlive the command itself, or a long build
  # goes dormant mid-flight and the very rule meant to catch it stops
  # seeing it. The other two are transient by design.
  _ratty_publish shell.running 1 3600
  # The reset that makes the next verdict an edge rather than a plateau.
  _ratty_publish shell.ok 0 3600
  _ratty_publish shell.fail 0 3600
}

_ratty_precmd() {
  # $? FIRST — anything else here overwrites the status we came for.
  local status_code=$?
  [[ -n ${RATTY_LIFECYCLE_OFF:-} ]] && return 0
  # zsh runs precmd before the first prompt too, when no command has run.
  # Without this guard every new shell would report a success it did not
  # earn.
  [[ -z ${_ratty_typeset_guard:-} ]] && return 0
  unset _ratty_typeset_guard

  _ratty_publish shell.running 0 3600
  if (( status_code == 0 )); then
    _ratty_publish shell.ok 1 10
  else
    _ratty_publish shell.fail 1 10
  fi
}

# `add-zsh-hook` appends, so this composes with whatever else already
# hooks preexec/precmd rather than replacing it.
autoload -Uz add-zsh-hook
add-zsh-hook preexec _ratty_preexec
add-zsh-hook precmd _ratty_precmd
