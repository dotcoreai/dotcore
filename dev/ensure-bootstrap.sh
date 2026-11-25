#!/usr/bin/env bash
# Bootstraps one area (frontend or backend) when its bootstrap check fails, so recipes that need it
# never fail for lack of setup. Parallel callers in a workspace bootstrap an area once: the others wait
# for the lock and find the check passing. Argument: the area name.
set -euo pipefail

# Seconds a caller waits for another caller's bootstrap before giving up.
lock_wait=900

main() {
  local area="${1:?usage: ensure-bootstrap.sh <frontend|backend>}"
  local root reason

  root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  cd "$root"
  source "$root/dev/pc-endpoint.sh"

  if just "$area::bootstrap-check" >/dev/null 2>&1; then
    return 0
  fi

  mkdir -p .direnv
  exec 9>".direnv/bootstrap-$area.lock"
  flock -w "$lock_wait" 9 || { echo "gave up waiting for another $area bootstrap to finish" >&2; exit 1; }

  # Another caller may have bootstrapped while this one waited.
  if reason="$(just "$area::bootstrap-check" 2>&1)"; then
    return 0
  fi

  refuse_under_running_stack "$area"
  echo "${reason%%;*}; bootstrapping it now" >&2
  just "$area::bootstrap"
}

# Reinstalling frontend dependencies replaces the folder the running stack's Vite server reads.
refuse_under_running_stack() {
  if [ "$1" != frontend ] || [ ! -d frontend/node_modules ] || [ -z "${PC_SOCKET_PATH:-}" ]; then
    return 0
  fi

  case "$(pc_endpoint_state "$PC_SOCKET_PATH")" in
    stale) ;;
    live)
      echo "frontend dependencies are outdated and the development stack is running; run \`just dev::down\`, then retry" >&2
      exit 1
      ;;
    *)
      echo "frontend dependencies are outdated and cannot tell whether the development stack is running at $PC_SOCKET_PATH; enter the workspace direnv shell and retry" >&2
      exit 1
      ;;
  esac
}

main "$@"
