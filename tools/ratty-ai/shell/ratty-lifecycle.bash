# Shell-lifecycle sensors for ratty (bash).
#
#   source /path/to/ratty/tools/ratty-ai/shell/ratty-lifecycle.bash
#
# The bash twin of ratty-lifecycle.zsh — same three sensors, same contract,
# same "only three numbers leave" guarantee:
#
#   shell.running   1 while a command runs, 0 at the prompt
#   shell.fail      pulses to 1 when a command exits non-zero
#   shell.ok        pulses to 1 when a command exits zero
#
# See the zsh file's header for why `ok` and `fail` are separate sensors
# (rules fire on threshold CROSSINGS, so repeated failures need repeated
# edges, which needs a reset).
#
# bash has no `preexec`, so this uses the DEBUG trap — which fires before
# EVERY command, including each command inside a function or a pipeline.
# The `_ratty_armed` latch below is what turns that firehose back into one
# event per prompt.

_ratty_publish() {
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
  # Already armed: this DEBUG firing is an inner command of the one we
  # already reported, not a new prompt-level command.
  [[ -n ${_ratty_armed:-} ]] && return 0
  # PROMPT_COMMAND's own execution trips DEBUG as well; ignore it, or every
  # prompt redraw would read as a command the user ran.
  [[ ${BASH_COMMAND:-} == "$PROMPT_COMMAND" ]] && return 0
  _ratty_armed=1

  _ratty_publish shell.running 1 3600
  _ratty_publish shell.ok 0 3600
  _ratty_publish shell.fail 0 3600
}

# Runs FIRST in the PROMPT_COMMAND chain, for one reason: to grab $? before
# any other integration's prompt hook overwrites it.
_ratty_capture() {
  _ratty_status=$?
}

# Runs LAST in the chain. Publishing here rather than in `_ratty_capture`
# is what keeps the DEBUG latch honest: nothing else in PROMPT_COMMAND runs
# after `_ratty_armed` is cleared, so no prompt hook can be mistaken for a
# command the user typed.
_ratty_precmd() {
  [[ -n ${RATTY_LIFECYCLE_OFF:-} ]] && return 0
  # Nothing ran since the last prompt (an empty line, or a fresh shell).
  [[ -z ${_ratty_armed:-} ]] && return 0
  unset _ratty_armed

  _ratty_publish shell.running 0 3600
  if (( ${_ratty_status:-0} == 0 )); then
    _ratty_publish shell.ok 1 10
  else
    _ratty_publish shell.fail 1 10
  fi
}

trap '_ratty_preexec' DEBUG
# Bracket whatever is already there rather than clobbering it — replacing
# PROMPT_COMMAND is how shell integrations break each other's prompts.
if [[ $PROMPT_COMMAND != *_ratty_precmd* ]]; then
  if [[ -z ${PROMPT_COMMAND:-} ]]; then
    PROMPT_COMMAND='_ratty_capture;_ratty_precmd'
  else
    PROMPT_COMMAND="_ratty_capture;${PROMPT_COMMAND};_ratty_precmd"
  fi
fi
