# Sourced by the development recipes that start, stop, or clean up a workspace Process Compose stack;
# not executed on its own, so it carries no shebang.
#
# A socket file outlives a Process Compose server killed without cleanup, so a stack counts as running
# only while its endpoint answers, read through the same client and transport the stop commands use.
# Only the client's own connection failure reads as a stopped stack: a client that cannot run, a command
# that moved, or an endpoint that accepts without answering stay unknown, so a running stack is never
# taken for the leftovers of a dead one.

# Seconds a single probe may take before its endpoint counts as unknown.
pc_probe_timeout=5

# Prints live, stale, or unknown for one endpoint: live when the client answers, stale when the client
# reports a refused or absent connection, unknown for any other client failure.
pc_endpoint_state() {
  local failure

  if failure="$(pc_probe "$1")"; then
    printf 'live\n'
    return 0
  fi

  case "$failure" in
    *"connect: connection refused"*|*"connect: no such file or directory"*) printf 'stale\n' ;;
    *) printf 'unknown\n' ;;
  esac
}

# Returns once none of the given stacks runs, so a recipe never acts under a stack that is still there. A
# running one is stopped after one question listing them all, or at once with the advance confirmation;
# without a terminal to ask in, it refuses with the message its owner chose. An endpoint it cannot read
# fails closed rather than open, before anything is stopped. Callers also source confirm.sh. Arguments: the
# subject named in messages, then for each stack its endpoint, its name, and the refusal naming how to stop it.
pc_stop_running_or_exit() {
  local subject="$1" socket name refusal index
  local sockets=() names=() refusals=() summary=()
  shift

  while test "$#" -gt 0; do
    socket="$1" name="$2" refusal="$3"
    shift 3
    case "$(pc_endpoint_state "$socket")" in
      stale) ;;
      live)
        sockets+=("$socket")
        names+=("$name")
        refusals+=("$refusal")
        ;;
      *)
        echo "cannot tell whether a development stack is running at $socket; enter the workspace direnv shell and retry" >&2
        exit 2
        ;;
    esac
  done
  test "${#sockets[@]}" -gt 0 || return 0

  # Without a terminal the owner's refusal says more than a generic request for confirmation.
  if test "${DOTCORE_YES:-}" != 1 && ! has_terminal; then
    printf '%s, or rerun with DOTCORE_YES=1 to stop it\n' "${refusals[@]}" >&2
    exit 2
  fi
  for name in "${names[@]}"; do summary+=("* the $name"); done
  confirm_or_exit "$subject" "Stop them and continue?" "These stacks are running and must be stopped first:" "${summary[@]}"

  # The client returns once the server is gone, so a single read afterwards tells whether it stopped; a
  # stack that exited on its own fails the stop request yet reads stopped, so only that read decides.
  for index in "${!sockets[@]}"; do
    process-compose --use-uds --unix-socket "${sockets[$index]}" down || true
    if test "$(pc_endpoint_state "${sockets[$index]}")" != stale; then
      echo "the ${names[$index]} still runs after the stop; ${refusals[$index]}" >&2
      exit 1
    fi
  done
}

# Removes one endpoint's socket file when nothing answers on it, and leaves a live or unreadable one
# alone. A stack can start between the guards and the removal, so this reads the endpoint again.
pc_remove_stale_socket() {
  case "$(pc_endpoint_state "$1")" in
    stale) rm -f -- "$1" ;;
  esac
}

# Runs one probe, holding its output back and printing the client's failure text instead, bounded so a
# listener that accepts without answering cannot hang a recipe where `timeout` is available.
pc_probe() {
  if command -v timeout >/dev/null 2>&1; then
    timeout "$pc_probe_timeout" process-compose --use-uds --unix-socket "$1" project state 2>&1 >/dev/null
  else
    process-compose --use-uds --unix-socket "$1" project state 2>&1 >/dev/null
  fi
}
