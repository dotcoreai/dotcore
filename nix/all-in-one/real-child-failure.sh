#!/usr/bin/env bash
set -Eeuo pipefail

# Prove the packaged supervisor cleans up after either real child dies post-readiness.

work=$(mktemp -d "${TMPDIR:?}/dotcore-child-failure.XXXXXXXX")
launcher_pid=''
owned_pids=()
owned_commands=()
cleanup() {
  local status=$? pid command index
  trap - EXIT INT TERM
  if [[ -n "$launcher_pid" ]]; then
    kill -TERM "$launcher_pid" 2>/dev/null || true
    wait "$launcher_pid" 2>/dev/null || true
  fi
  if ((status != 0)); then
    for index in "${!owned_pids[@]}"; do
      pid=${owned_pids[index]}
      command=$(ps -ww -o command= -p "$pid" 2>/dev/null || true)
      if [[ "$command" == *"${owned_commands[index]}"* ]]; then
        kill -KILL "$pid" 2>/dev/null || true
      fi
    done
    for log in "$work"/*/launcher-*.err; do
      [[ -f "$log" ]] && cat "$log" >&2
    done
  fi
  rm -rf -- "$work"
  exit "$status"
}
trap cleanup EXIT INT TERM
fail() { echo "real-child failure: $*" >&2; exit 1; }

# Run each death after both real services are reachable. Never signal by name or port.
scenario() {
  local role=$1 root="$work/$1" pid_dir log public inference ready=false retry
  local backend_pid caddy_pid drain_pid management target status diagnostic
  local -a stage
  mkdir -m 700 "$root"
  mkdir -m 700 "$root/home" "$root/state" "$root/cache" "$root/runtime" "$root/site"
  cp -R "${DOTCORE_FAILURE_SITE_FIXTURE:?Site fixture is required}"/. "$root/site/"
  chmod -R u+rwX "$root/site"

  for ((retry = 0; retry < 5; retry++)); do
    pid_dir="$root/pids-$retry"
    log="$root/launcher-$retry.err"
    mkdir -m 700 "$pid_dir"
    read -r public inference management < <(ports)
    env HOME="$root/home" XDG_STATE_HOME="$root/state" XDG_CACHE_HOME="$root/cache" \
      XDG_RUNTIME_DIR="$root/runtime" DOTCORE_SITE_DIR="$root/site" DOTCORE_FAILURE_PID_DIR="$pid_dir" \
      SSL_CERT_FILE="${DOTCORE_FAILURE_CA_BUNDLE:?CA bundle is required}" \
      "${DOTCORE_FAILURE_LAUNCHER:?launcher is required}" --public-port "$public" --inference-port "$inference" \
      --management-port "$management" \
      >"$root/launcher.out" 2>"$log" &
    launcher_pid=$!
    if wait_ready "$public" "$inference" "$pid_dir" "$log"; then
      ready=true
      break
    fi
    wait "$launcher_pid" || true
    launcher_pid=''
    if ((retry < 4)) && grep -Eiq 'address already in use|EADDRINUSE' "$log"; then
      [[ -z $(find "$root/runtime" -mindepth 1 -print -quit) ]] || fail 'bind failure left a private stage'
      continue
    fi
    fail "$role launcher exited before readiness"
  done
  [[ "$ready" == true ]] || fail "$role exhausted bind-collision retries"

  backend_pid=$(recorded_child backend "${DOTCORE_FAILURE_BACKEND_COMMAND:?backend is required}" "$pid_dir")
  caddy_pid=$(recorded_child caddy "${DOTCORE_FAILURE_CADDY_COMMAND:?Caddy is required}" "$pid_dir")
  drain_pid=$(recorded_child drain "${DOTCORE_FAILURE_DRAIN_COMMAND:?drain is required}" "$pid_dir")
  [[ "$backend_pid" != "$caddy_pid" && "$backend_pid" != "$drain_pid" && "$caddy_pid" != "$drain_pid" ]] \
    || fail 'ambiguous child PIDs'
  owned_pids=("$backend_pid" "$caddy_pid" "$drain_pid")
  owned_commands=("$DOTCORE_FAILURE_BACKEND_COMMAND" "$DOTCORE_FAILURE_CADDY_COMMAND" "$DOTCORE_FAILURE_DRAIN_COMMAND")
  stage=("$root/runtime"/dotcore.*/frontend)
  [[ ${#stage[@]} -eq 1 && -f "${stage[0]}/config.js" ]] || fail 'missing or ambiguous private frontend stage'
  printf '\0\0\0\0\0' | curl -fs --max-time 2 -H 'Content-Type: application/grpc-web+proto' \
    -H 'x-grpc-web: 1' --data-binary @- \
    "http://127.0.0.1:$management/dotcore.catalog.v1.CatalogService/ListModels" | grep -aqE 'grpc-status: ?0' \
    || fail 'launcher ignored --management-port'

  if [[ "$role" == backend ]]; then
    target=$backend_pid
    diagnostic=backend
    recorded_child backend "$DOTCORE_FAILURE_BACKEND_COMMAND" "$pid_dir" >/dev/null
  else
    target=$caddy_pid
    diagnostic=Caddy
    recorded_child caddy "$DOTCORE_FAILURE_CADDY_COMMAND" "$pid_dir" >/dev/null
  fi
  kill -KILL "$target"
  wait_for_exit
  set +e
  wait "$launcher_pid"
  status=$?
  set -e
  launcher_pid=''
  [[ "$status" -eq 1 ]] || fail "$role death yielded launcher status $status, expected 1"
  grep -Fq "dotcore: $diagnostic exited unexpectedly" "$log" || fail "missing $diagnostic exit diagnostic"
  for pid in "$backend_pid" "$caddy_pid" "$drain_pid"; do
    ! kill -0 "$pid" 2>/dev/null || fail "launcher-owned process $pid remains"
  done
  [[ ! -e "${stage[0]}" ]] || fail 'private frontend stage remains'
  rebind "$public" "$inference" "$management"
  owned_pids=()
  owned_commands=()
}

ports() {
  python3 - <<'PY'
import socket
with socket.socket() as public, socket.socket() as inference, socket.socket() as management:
    public.bind(("0.0.0.0", 0))
    inference.bind(("0.0.0.0", 0))
    management.bind(("127.0.0.1", 0))
    print(public.getsockname()[1], inference.getsockname()[1], management.getsockname()[1])
PY
}

recorded_child() {
  local role=$1 expected=$2 pid_file=$3/$1.pid pid parent command
  [[ -f "$pid_file" && $(wc -l < "$pid_file") -eq 1 ]] || fail "missing or ambiguous $role PID"
  pid=$(cat "$pid_file")
  [[ "$pid" =~ ^[0-9]+$ ]] || fail "invalid $role PID"
  kill -0 "$pid" 2>/dev/null || fail "$role PID is not running"
  parent=$(ps -o ppid= -p "$pid" | tr -d '[:space:]')
  [[ "$parent" == "$launcher_pid" ]] || fail "$role is not a direct launcher child"
  command=$(ps -ww -o command= -p "$pid")
  [[ "$command" == *"$expected"* ]] || fail "$role PID does not run $expected: $command"
  printf '%s\n' "$pid"
}

wait_ready() {
  local public=$1 inference=$2 pid_dir=$3 log=$4 attempt
  for ((attempt = 0; attempt < 300; attempt++)); do
    if curl --fail --silent --max-time 1 "http://127.0.0.1:$public/" >/dev/null \
      && curl --fail --silent --max-time 1 "http://127.0.0.1:$public/v1/models" >/dev/null \
      && curl --fail --silent --max-time 1 "http://127.0.0.1:$inference/v1/models" >/dev/null \
      && [[ -s "$pid_dir/backend.pid" && -s "$pid_dir/caddy.pid" && -s "$pid_dir/drain.pid" ]] \
      && grep -Fq 'Dotcore is running' "$log" \
      && printf '\0\0\0\0\0' | curl -fs --max-time 2 -H 'Content-Type: application/grpc-web+proto' \
        -H 'x-grpc-web: 1' --data-binary @- \
        "http://127.0.0.1:$public/dotcore.catalog.v1.CatalogService/ListModels" | grep -aqE 'grpc-status: ?0'; then
      return 0
    fi
    if ! kill -0 "$launcher_pid" 2>/dev/null; then return 1; fi
    sleep .1
  done
  fail 'timed out waiting for real HTTP and child readiness'
}

wait_for_exit() {
  local attempt state
  for ((attempt = 0; attempt < 200; attempt++)); do
    state=$(ps -o stat= -p "$launcher_pid" 2>/dev/null | tr -d '[:space:]' || true)
    [[ -z "$state" || "$state" == Z* ]] && return 0
    sleep .1
  done
  fail 'launcher did not exit after its child was killed'
}

rebind() {
  python3 - "$@" <<'PY'
import socket
import sys
for host, port in [("0.0.0.0", int(sys.argv[1])), ("0.0.0.0", int(sys.argv[2])), ("127.0.0.1", int(sys.argv[3]))]:
    with socket.socket() as listener:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind((host, port))
        listener.listen()
PY
}

scenario backend
scenario caddy
