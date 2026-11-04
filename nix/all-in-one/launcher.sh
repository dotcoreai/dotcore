public_port=8080
inference_port=50052
management_port=50051
private_addr=""
public_port_set=false
inference_port_set=false
management_port_set=false
private_addr_set=false
site_dir_set=false
catalog_dir_set=false
cache_dir_set=false
# Root path flags are forwarded to every dotc invocation; dotc resolves them over its variables.
path_args=()
dotc_command="@dotcCommand@"
caddy_command="@caddyCommand@"
runtime_config_command="@runtimeConfigCommand@"
# Source this build came from (empty without a revision): the backend builds locked Nix artifacts
# of the same revision from it, so a node needs no access to the Dotcore repository for them.
packaged_source="@packagedSource@"
if [[ -n "$packaged_source" ]]; then
  export DOTCORE_PACKAGED_SOURCE_FLAKE="$packaged_source"
fi
frontend_root="@frontendRoot@"
drain_command="@drainCommand@"

usage() {
  echo "usage: dotcore [--site-dir <dir>] [--catalog-dir <dir>] [--cache-dir <dir>] [--public-port <port>] [--inference-port <port>] [--management-port <port>] [--private-addr <host:port>]" >&2
  echo "Each listener flag falls back to its variable: DOTCORE_PUBLIC_PORT, DOTCORE_INFERENCE_ADDR, DOTCORE_MANAGEMENT_ADDR, DOTCORE_PRIVATE_ADDR." >&2
}

# Prints the value as an absolute path, resolving a relative one against the working directory.
absolute_path() {
  case "$1" in
    /*) printf '%s\n' "$1" ;;
    *) printf '%s/%s\n' "$PWD" "$1" ;;
  esac
}

valid_port() {
  local value="$1"
  [[ "$value" =~ ^[0-9]+$ ]] \
    && ((10#$value >= 1 && 10#$value <= 65535))
}

valid_socket_address() {
  local value="$1"
  [[ "$value" =~ ^[^[:space:]:]+:[0-9]+$ ]] \
    && valid_port "${value##*:}"
}

while (($# > 0)); do
  case "$1" in
    --site-dir)
      if "$site_dir_set" || (($# < 2)) || [[ -z "$2" ]]; then
        echo "dotcore: --site-dir requires one directory and may be supplied only once" >&2
        usage
        exit 2
      fi
      path_args+=(--site-dir "$(absolute_path "$2")")
      site_dir_set=true
      shift 2
      ;;
    --catalog-dir)
      if "$catalog_dir_set" || (($# < 2)) || [[ -z "$2" ]]; then
        echo "dotcore: --catalog-dir requires one directory and may be supplied only once" >&2
        usage
        exit 2
      fi
      path_args+=(--catalog-dir "$(absolute_path "$2")")
      catalog_dir_set=true
      shift 2
      ;;
    --cache-dir)
      if "$cache_dir_set" || (($# < 2)) || [[ -z "$2" ]]; then
        echo "dotcore: --cache-dir requires one directory and may be supplied only once" >&2
        usage
        exit 2
      fi
      path_args+=(--cache-dir "$(absolute_path "$2")")
      cache_dir_set=true
      shift 2
      ;;
    --public-port)
      if "$public_port_set" || (($# < 2)) || ! valid_port "$2"; then
        echo "dotcore: --public-port requires one non-zero TCP port and may be supplied only once" >&2
        usage
        exit 2
      fi
      public_port="$2"
      public_port_set=true
      shift 2
      ;;
    --inference-port)
      if "$inference_port_set" || (($# < 2)) || ! valid_port "$2"; then
        echo "dotcore: --inference-port requires one non-zero TCP port and may be supplied only once" >&2
        usage
        exit 2
      fi
      inference_port="$2"
      inference_port_set=true
      shift 2
      ;;
    --management-port)
      if "$management_port_set" || (($# < 2)) || ! valid_port "$2"; then
        echo "dotcore: --management-port requires one non-zero TCP port and may be supplied only once" >&2
        usage
        exit 2
      fi
      management_port="$2"
      management_port_set=true
      shift 2
      ;;
    --private-addr)
      if "$private_addr_set" || (($# < 2)) || ! valid_socket_address "$2"; then
        echo "dotcore: --private-addr requires one host:port with a non-zero TCP port and may be supplied only once" >&2
        usage
        exit 2
      fi
      private_addr="$2"
      private_addr_set=true
      shift 2
      ;;
    *)
      echo "dotcore: unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

runtime_root=""
stage=""
startup_pid=""
backend_pid=""
caddy_pid=""
drain_pid=""
cleaned=false

process_running() {
  [[ -n "$1" ]] && kill -0 "$1" 2>/dev/null
}

# ShellCheck cannot see trap-based function invocation.
# shellcheck disable=SC2329
cleanup() {
  local pid
  local attempt

  if "$cleaned"; then
    return
  fi
  cleaned=true
  trap - INT TERM

  for pid in "$startup_pid" "$caddy_pid" "$backend_pid" "$drain_pid"; do
    if process_running "$pid"; then
      kill -TERM "$pid" 2>/dev/null || true
    fi
  done

  for ((attempt = 0; attempt < 50; attempt++)); do
    if ! process_running "$startup_pid" \
      && ! process_running "$caddy_pid" \
      && ! process_running "$backend_pid" \
      && ! process_running "$drain_pid"; then
      break
    fi
    sleep 0.1
  done

  for pid in "$startup_pid" "$caddy_pid" "$backend_pid" "$drain_pid"; do
    if process_running "$pid"; then
      kill -KILL "$pid" 2>/dev/null || true
    fi
    if [[ -n "$pid" ]]; then
      wait "$pid" 2>/dev/null || true
    fi
  done

  if [[ -n "$runtime_root" ]]; then
    rm -rf -- "$runtime_root"
  fi
}

# ShellCheck cannot see trap-based function invocation.
# shellcheck disable=SC2329
signal_shutdown() {
  echo "Dotcore shutdown requested; stopping services" >&2
  cleanup
  exit 0
}

trap cleanup EXIT
trap signal_shutdown INT TERM

run_tracked() {
  local status

  "$@" &
  startup_pid=$!
  set +e
  wait "$startup_pid"
  status=$?
  set -e
  startup_pid=""
  return "$status"
}

select_runtime_parent() {
  local candidate

  for candidate in "${XDG_RUNTIME_DIR:-}" "${TMPDIR:-}" /tmp; do
    if [[ "$candidate" == /* && -d "$candidate" && -w "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done

  echo "dotcore: no absolute writable runtime directory is available" >&2
  return 1
}

# Prints the host:port of a reported http://<IPv4>:<port> startup address.
socket_address() {
  local address="$1"
  local label="$2"
  local socket host port octet
  local -a octets

  socket="${address#http://}"
  if [[ "$address" != http://* || "$socket" != *:* ]]; then
    echo "dotcore: malformed $label startup address: $address" >&2
    return 1
  fi
  host="${socket%:*}"
  port="${socket##*:}"
  IFS=. read -r -a octets <<< "$host"
  if ((${#octets[@]} != 4)) || ! valid_port "$port"; then
    echo "dotcore: malformed $label startup address: $address" >&2
    return 1
  fi
  for octet in "${octets[@]}"; do
    if [[ ! "$octet" =~ ^[0-9]+$ ]] || ((10#$octet > 255)); then
      echo "dotcore: malformed $label startup address: $address" >&2
      return 1
    fi
  done
  printf '%s\n' "$socket"
}

# Prints the address Caddy dials for a reported listener; a wildcard listener is dialed on loopback.
upstream_address() {
  local socket="$1"
  if [[ "${socket%:*}" == 0.0.0.0 ]]; then
    printf '127.0.0.1:%s\n' "${socket##*:}"
  else
    printf '%s\n' "$socket"
  fi
}

read_startup_line() {
  local expected="$1"
  local timeout="$2"
  local line

  if ! IFS= read -r -t "$timeout" line <&3; then
    echo "dotcore: backend readiness ended or timed out while waiting for $expected" >&2
    return 1
  fi
  printf '%s\n' "$line"
}

# Reports a backend that never became ready, which includes one that cannot listen on its addresses,
# and exits.
readiness_failed() {
  echo "dotcore: the backend did not start; if it cannot listen on $DOTCORE_INFERENCE_ADDR, use a node host that is an address of this machine, or set DOTCORE_INFERENCE_ADDR" >&2
  exit 1
}

remaining_timeout() {
  local elapsed remaining
  elapsed=$(($(date +%s) - readiness_started))
  remaining=$((readiness_deadline - elapsed))
  if ((remaining < 0)); then
    remaining=0
  fi
  printf '%s\n' "$remaining"
}

# Prints what to open and what to do next once the manager serves. The URLs name the node host,
# unless an inherited inference address moved the listeners to another specific address. Colors
# follow the rule of dotc's guided prompts: a terminal, NO_COLOR unset or empty, a usable TERM.
print_banner() {
  local url_host="$node_host" site_display="$site_dir"
  local bold='' dim='' cyan='' green='' yellow='' reset=''

  if "$inference_addr_inherited" && [[ "$public_host" != 0.0.0.0 ]] \
    && [[ "${DOTCORE_INFERENCE_ADDR%:*}" != "$node_host" ]]; then
    url_host="$public_host"
  fi
  if [[ -n "${HOME:-}" && "$site_dir" == "$HOME"/* ]]; then
    site_display="~${site_dir#"$HOME"}"
  fi
  if [[ -t 2 && -z "${NO_COLOR:-}" && -n "${TERM:-}" && "$TERM" != dumb ]]; then
    bold=$'\e[1m' dim=$'\e[2m' cyan=$'\e[36m' green=$'\e[32m' yellow=$'\e[33m' reset=$'\e[0m'
  fi

  cat >&2 <<EOF

  ${bold}${green}Dotcore is running${reset}

  Console     ${cyan}http://$url_host:$public_port${reset}
  OpenAI API  ${cyan}http://$url_host:$public_port/v1${reset}

  ${bold}Next steps${reset}
    1. Open the console and deploy a model from Models > Catalog
    2. Point any OpenAI-compatible client at the API URL above
    3. Add a machine: stop Dotcore, then run ${bold}${cyan}dotc node add${reset}

  ${yellow}Warning: no login or TLS yet. Anyone who can reach $url_host
  can use your models and change your site.${reset}

  ${dim}Site folder: $site_display - Ctrl+C to stop${reset}

EOF
}

# Starts the backend for a joined node folder, accepts its private-only startup output, and never
# returns: it waits for the backend and exits with its outcome.
start_joined_node() {
  local fifo="$runtime_root/backend-readiness"
  local banner private_line status

  if "$private_addr_set"; then
    echo "dotcore: --private-addr applies to a manager; a joined node uses the address recorded in its node folder" >&2
    exit 2
  fi
  if "$catalog_dir_set"; then
    echo "dotcore: --catalog-dir applies to a manager; a joined node has no master catalog" >&2
    exit 2
  fi

  mkfifo "$fifo"
  # The node folder records the private address peers dial, so an inherited override must not win.
  unset DOTCORE_MANAGEMENT_ADDR DOTCORE_INFERENCE_ADDR DOTCORE_FRONTEND_ORIGIN DOTCORE_PRIVATE_ADDR

  "$dotc_command" ${path_args[@]+"${path_args[@]}"} server start >"$fifo" &
  backend_pid=$!
  exec 3<"$fifo"
  rm -f "$fifo"

  readiness_deadline="${DOTCORE_STARTUP_TIMEOUT_SECONDS:-60}"
  if ! valid_port "$readiness_deadline"; then
    echo "dotcore: invalid internal readiness deadline" >&2
    exit 1
  fi
  readiness_started="$(date +%s)"

  banner="$(read_startup_line "startup banner" "$(remaining_timeout)")" || exit 1
  if [[ "$banner" != "Dotcore server starting" ]]; then
    echo "dotcore: malformed backend readiness banner: $banner" >&2
    exit 1
  fi
  private_line="$(read_startup_line "private address" "$(remaining_timeout)")" || exit 1
  if [[ "$private_line" != DOTCORE_PRIVATE_ADDRESS=https://* ]]; then
    echo "dotcore: malformed backend readiness private line: $private_line" >&2
    exit 1
  fi

  "$drain_command" <&3 >&2 &
  drain_pid=$!
  exec 3<&-

  cat >&2 <<NODE_READY
Dotcore joined node is serving its private listener at ${private_line#DOTCORE_PRIVATE_ADDRESS=}
Node folder: $site_dir
NODE_READY

  set +e
  wait "$backend_pid"
  status=$?
  set -e
  echo "dotcore: backend exited with status $status" >&2
  exit 1
}

if [[ -z "${DOTCORE_CATALOG_DIR+x}" ]]; then
  export DOTCORE_CATALOG_DIR=@catalog@
fi

runtime_parent="$(select_runtime_parent)"
old_umask="$(umask)"
umask 077
runtime_root="$(mktemp -d "$runtime_parent/dotcore.XXXXXXXX")"
chmod 0700 "$runtime_root"
umask "$old_umask"

config_file="$runtime_root/effective-config.json"
if ! run_tracked "$dotc_command" ${path_args[@]+"${path_args[@]}"} config show --format json >"$config_file"; then
  echo "dotcore: cannot inspect effective process paths" >&2
  exit 1
fi
config_json="$(cat "$config_file")"
site_dir="$(jq -er '.siteDir | strings' <<< "$config_json")"
role="$(jq -er '.role | strings' <<< "$config_json")" || role=""

# The backend owns role detection. A joined node folder runs the backend alone: it has no console,
# Caddy, or public listeners, only the private listener its folder declares.
case "$role" in
  manager)
    echo "dotcore: starting as manager (node folder: $site_dir)" >&2
    ;;
  joined-node)
    echo "dotcore: starting as joined node (node folder: $site_dir)" >&2
    start_joined_node
    ;;
  *)
    echo "dotcore: backend reported an unknown node role: ${role:-<missing>}" >&2
    exit 1
    ;;
esac

# Each listener takes its flag, else its inherited variable, else the bundle default. Setup sees
# the inherited addresses and --private-addr, since a specific one locks the node host; a flag's
# own variable is cleared so a value the server will not bind cannot lock it.
if ! "$public_port_set" && [[ -n "${DOTCORE_PUBLIC_PORT:-}" ]]; then
  if ! valid_port "$DOTCORE_PUBLIC_PORT"; then
    echo "dotcore: DOTCORE_PUBLIC_PORT requires one non-zero TCP port" >&2
    usage
    exit 2
  fi
  public_port="$DOTCORE_PUBLIC_PORT"
fi
if "$management_port_set"; then
  unset DOTCORE_MANAGEMENT_ADDR
fi
if "$inference_port_set"; then
  unset DOTCORE_INFERENCE_ADDR
fi
if "$private_addr_set"; then
  export DOTCORE_PRIVATE_ADDR="$private_addr"
fi

if ! run_tracked "$dotc_command" ${path_args[@]+"${path_args[@]}"} site init --if-missing --interactive; then
  echo "dotcore: site setup did not complete; services were not started" >&2
  exit 1
fi

if [[ ! -f "$site_dir/site.yaml" || ! -f "$site_dir/node.yaml" ]]; then
  echo "dotcore: site setup did not complete; services were not started" >&2
  exit 1
fi

# The public listeners bind the node host the site records, which setup chooses on a first run.
if ! run_tracked "$dotc_command" ${path_args[@]+"${path_args[@]}"} config show --format json >"$config_file"; then
  echo "dotcore: cannot read the node host of the site" >&2
  exit 1
fi
if ! node_host="$(jq -er '.nodeHost | strings' "$config_file")"; then
  echo "dotcore: the site at $site_dir records no node host; run dotc site validate --site-dir $site_dir" >&2
  exit 1
fi

stage="$runtime_root/frontend"
mkdir -m 0700 "$stage"

if ! run_tracked cp -R "$frontend_root"/. "$stage/" \
  || ! run_tracked chmod -R u+rwX "$stage"; then
  echo "dotcore: cannot populate the private frontend stage" >&2
  exit 1
fi

export DOTCORE_MANAGEMENT_ORIGIN=same-origin
export DOTCORE_INFERENCE_ORIGIN=same-origin
export VITE_DOTCORE_MANAGEMENT_ORIGIN=same-origin
export VITE_DOTCORE_INFERENCE_ORIGIN=same-origin
if ! run_tracked "$runtime_config_command" "$stage" "config.js"; then
  echo "dotcore: cannot generate frontend runtime configuration" >&2
  exit 1
fi

readiness_fifo="$stage/backend-readiness"
mkfifo "$readiness_fifo"
# Exported after setup so the bundle's loopback management default never locks the node host.
if [[ -z "${DOTCORE_MANAGEMENT_ADDR:-}" ]]; then
  export DOTCORE_MANAGEMENT_ADDR="127.0.0.1:$management_port"
fi
inference_addr_inherited=false
if [[ -z "${DOTCORE_INFERENCE_ADDR:-}" ]]; then
  export DOTCORE_INFERENCE_ADDR="$node_host:$inference_port"
else
  inference_addr_inherited=true
fi
unset DOTCORE_FRONTEND_ORIGIN

"$dotc_command" ${path_args[@]+"${path_args[@]}"} server start >"$readiness_fifo" &
backend_pid=$!
exec 3<"$readiness_fifo"
rm -f "$readiness_fifo"

readiness_deadline="${DOTCORE_STARTUP_TIMEOUT_SECONDS:-60}"
if ! valid_port "$readiness_deadline"; then
  echo "dotcore: invalid internal readiness deadline" >&2
  exit 1
fi
readiness_started="$(date +%s)"

line_one="$(read_startup_line "startup banner" "$(remaining_timeout)")" || readiness_failed
if [[ "$line_one" != "Dotcore server starting" ]]; then
  echo "dotcore: malformed backend readiness banner: $line_one" >&2
  exit 1
fi

line_two="$(read_startup_line "Management address" "$(remaining_timeout)")" || readiness_failed
if [[ "$line_two" != DOTCORE_MANAGEMENT_ADDRESS=* ]]; then
  echo "dotcore: malformed backend readiness management line: $line_two" >&2
  exit 1
fi
management_socket="$(socket_address "${line_two#DOTCORE_MANAGEMENT_ADDRESS=}" "management")" || exit 1

line_three="$(read_startup_line "inference address" "$(remaining_timeout)")" || readiness_failed
if [[ "$line_three" != DOTCORE_INFERENCE_ADDRESS=* ]]; then
  echo "dotcore: malformed backend readiness inference line: $line_three" >&2
  exit 1
fi
inference_socket="$(socket_address "${line_three#DOTCORE_INFERENCE_ADDRESS=}" "inference")" || exit 1

"$drain_command" <&3 >&2 &
drain_pid=$!
exec 3<&-

export DOTCORE_PUBLIC_PORT="$public_port"
# The console listens where inference does: the node host as the backend resolved it, so a name
# never binds a different address than the backend's, or the address an explicit setting names.
public_host="${inference_socket%:*}"
export DOTCORE_FRONTEND_STATIC_SERVER_ROOT="$stage"
DOTCORE_MANAGEMENT_UPSTREAM="http://$(upstream_address "$management_socket")"
DOTCORE_INFERENCE_UPSTREAM="http://$(upstream_address "$inference_socket")"
export DOTCORE_MANAGEMENT_UPSTREAM DOTCORE_INFERENCE_UPSTREAM

if ! DOTCORE_MACHINE_HOST="$public_host" run_tracked "$caddy_command" validate --config @caddyfile@ --adapter caddyfile; then
  echo "dotcore: Caddy configuration validation failed" >&2
  exit 1
fi

DOTCORE_MACHINE_HOST="$public_host" "$caddy_command" run --config @caddyfile@ --adapter caddyfile &
caddy_pid=$!
sleep 0.1
if ! process_running "$caddy_pid"; then
  wait "$caddy_pid" || true
  echo "dotcore: Caddy exited during startup; check that $public_host:$public_port is an address of this machine and free" >&2
  exit 1
fi

print_banner

set +e
wait -n "$backend_pid" "$caddy_pid"
child_status=$?
set -e
if ! process_running "$backend_pid"; then
  echo "dotcore: backend exited unexpectedly with status $child_status" >&2
else
  echo "dotcore: Caddy exited unexpectedly with status $child_status" >&2
fi
exit 1
