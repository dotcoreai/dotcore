# Sourced by any recipe that must ask the person running it before an action; not executed on its own, so it
# carries no shebang.
#
# A recipe asks here when nothing else will: for example when the action runs on a remote host, where no
# terminal exists to prompt in. The question goes to the terminal the recipe runs in, before the action starts.
# The advance confirmation that destructive recipes share answers it up front for a run without a terminal; it
# is the same answer given early, not a safeguard.

# Succeeds when both the input and the messages are a terminal, so a question has someone to ask.
has_terminal() {
  test -t 0 && test -t 2
}

# Shows the summary lines and asks one yes or no question in the terminal; succeeds on a yes and fails on
# anything else. The caller checks for a terminal first. Arguments: the question, then the summary lines.
ask_yes_no() {
  local question="$1" answer
  shift

  printf '%s\n' "$@" >&2
  read -r -p "$question [y/N] " answer || answer=""
  case "$answer" in
    y|Y|yes|Yes|YES) return 0 ;;
  esac
  return 1
}

# Shows a summary and asks one yes or no question in the terminal; returns once confirmed. An advance
# confirmation returns without asking. A declined question exits 130, and a run with no terminal to ask in
# exits 2. Arguments: the subject named in messages, the question, then the summary lines.
confirm_or_exit() {
  local subject="$1" question="$2"
  shift 2

  case "${DOTCORE_YES:-}" in
    1) return 0 ;;
    ''|0) ;;
    *) echo 'DOTCORE_YES must be 1 or unset' >&2; exit 2 ;;
  esac

  # A piped answer is no one at a terminal, so both the input and the messages must be one.
  if ! has_terminal; then
    echo "$subject needs confirmation and there is no terminal to ask in; run it in a terminal, or rerun with DOTCORE_YES=1 to confirm in advance" >&2
    exit 2
  fi

  if ask_yes_no "$question" "$@"; then
    return 0
  fi
  echo "$subject cancelled; nothing was changed" >&2
  exit 130
}
