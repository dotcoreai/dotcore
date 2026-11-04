#!/usr/bin/env bash
set -Eeuo pipefail

# Real-package wiring and normal-shutdown smoke; backend semantics have their own tests.

work=$(mktemp -d "${TMPDIR:?}/dotcore-package.XXXXXXXX")
launcher=${DOTCORE_PACKAGE_LAUNCHER:?launcher is required}
launcher_pid=''
add_pid=''
cleanup() {
  local status=$?
  trap - EXIT INT TERM
  if [[ -n "$launcher_pid" ]]; then
    kill -TERM "$launcher_pid" 2>/dev/null || true
    wait "$launcher_pid" 2>/dev/null || true
  fi
  if [[ -n "$add_pid" ]]; then
    kill -TERM "$add_pid" 2>/dev/null || true
    wait "$add_pid" 2>/dev/null || true
  fi
  if ((status != 0)); then
    cat "$work"/*.err 2>/dev/null >&2 || true
  fi
  rm -rf -- "$work"
  exit "$status"
}
trap cleanup EXIT INT TERM
fail() {
  echo "package smoke: $*" >&2
  exit 1
}

export HOME="$work/home" XDG_STATE_HOME="$work/state" XDG_CACHE_HOME="$work/cache"
export XDG_RUNTIME_DIR="$work/runtime" DOTCORE_SITE_DIR="$work/site"
export SSL_CERT_FILE="${DOTCORE_PACKAGE_CA_BUNDLE:?CA bundle is required}"
# Unassignable inherited listeners make startup fail unless the flags every run passes win.
export DOTCORE_MANAGEMENT_ADDR=192.0.2.1:1 DOTCORE_INFERENCE_ADDR=192.0.2.1:1
mkdir -m 700 "$HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME" "$XDG_RUNTIME_DIR"

# An unattended first launch must leave the missing site and private stage untouched.
if env -u DOTCORE_CATALOG_DIR "$launcher" >"$work/first.out" 2>"$work/first.err"; then
  fail 'missing site was initialized without a terminal'
fi
[[ ! -e "$DOTCORE_SITE_DIR" ]] || fail 'first launch created a site'
[[ -z $(find "$XDG_RUNTIME_DIR" -mindepth 1 -print -quit) ]] || fail 'first launch left a private stage'
grep -Fq 'site setup did not complete; services were not started' "$work/first.err" ||
  fail 'missing first-run refusal diagnostic'

grep -Fxq "dotcore: starting as manager (node folder: $DOTCORE_SITE_DIR)" "$work/first.err" ||
  fail 'manager role line missing'

# Root path flags are validated before any service starts.
expect_usage_failure() {
  local label=$1 message=$2
  shift 2
  local status=0
  "$launcher" "$@" >"$work/flag.out" 2>"$work/flag.err" || status=$?
  ((status == 2)) || fail "$label exited with $status, expected 2"
  grep -Fxq "$message" "$work/flag.err" || fail "$label diagnostic: $(cat "$work/flag.err")"
}
expect_usage_failure 'repeated site dir' \
  'dotcore: --site-dir requires one directory and may be supplied only once' \
  --site-dir "$work/a" --site-dir "$work/b"
expect_usage_failure 'missing cache dir value' \
  'dotcore: --cache-dir requires one directory and may be supplied only once' \
  --cache-dir

# A worker folder is a joined node: the role is reported and the manager-only catalog flag refused.
joined_site="$work/joined-site"
mkdir -m 700 "$joined_site"
: >"$joined_site/worker.yaml"
status=0
"$launcher" --site-dir "$joined_site" --catalog-dir "$work/catalog" \
  >"$work/joined.out" 2>"$work/joined.err" || status=$?
((status == 2)) || fail "joined node with --catalog-dir exited with $status, expected 2"
grep -Fxq "dotcore: starting as joined node (node folder: $joined_site)" "$work/joined.err" ||
  fail "joined role line missing: $(cat "$work/joined.err")"
grep -Fxq 'dotcore: --catalog-dir applies to a manager; a joined node has no master catalog' "$work/joined.err" ||
  fail "joined catalog-dir diagnostic: $(cat "$work/joined.err")"

# An unknown backend role fails closed instead of defaulting to a manager. The backend it asked
# already saw the source the launcher was built from.
status=0
DOTCORE_PACKAGE_SOURCE_FILE="$work/role.source" \
  "${DOTCORE_PACKAGE_UNKNOWN_ROLE_LAUNCHER:?unknown role fixture is required}" \
  >"$work/role.out" 2>"$work/role.err" || status=$?
((status == 1)) || fail "unknown role exited with $status, expected 1"
grep -Fxq 'dotcore: backend reported an unknown node role: bogus' "$work/role.err" ||
  fail "unknown role diagnostic: $(cat "$work/role.err")"
[[ "$(cat "$work/role.source")" == "${DOTCORE_PACKAGE_FIXTURE_SOURCE:?fixture source is required}" ]] ||
  fail "packaged source handed to the backend: $(cat "$work/role.source")"

# A successful initialization command that publishes no site inputs must not start services.
init_fixture=${DOTCORE_PACKAGE_INIT_FIXTURE_LAUNCHER:?initialization fixture is required}
markers="$work/no-publication-markers"
declined_root="$work/no-publication"
mkdir -m 700 "$markers"

run_no_publication() {
  local site_dir=$1
  env -u DOTCORE_CATALOG_DIR DOTCORE_SITE_DIR="$site_dir" \
    DOTCORE_PACKAGE_MARKER_DIR="$markers" \
    "$init_fixture" >"$work/no-publication.out" 2>"$work/no-publication.err"
}

assert_no_publication_refusal() {
  local label=$1
  local detail
  detail="stderr=$(cat "$work/no-publication.err") stdout=$(cat "$work/no-publication.out")"
  grep -Fq 'site setup did not complete; services were not started' "$work/no-publication.err" ||
    fail "$label did not report incomplete setup; $detail"
  ! grep -Fq 'readiness' "$work/no-publication.err" || fail "$label reached backend readiness; $detail"
  [[ -z $(find "$markers" -mindepth 1 -print -quit) ]] || fail "$label started a service command"
  [[ -z $(find "$XDG_RUNTIME_DIR" -mindepth 1 -print -quit) ]] || fail "$label left a private stage"
}

if run_no_publication "$declined_root/.dotcore/site"; then
  fail 'declined initialization started services without a published site'
fi
assert_no_publication_refusal 'declined initialization'
[[ ! -e "$declined_root" ]] || fail 'declined initialization created the site tree'

for published in site.yaml node.yaml; do
  partial_root="$work/partial-$published"
  partial_site="$partial_root/.dotcore/site"
  mkdir -p "$partial_site"
  printf '%s\n' "$published contents" >"$partial_site/$published"
  if run_no_publication "$partial_site"; then
    fail "partial publication of $published started services"
  fi
  assert_no_publication_refusal "partial publication of $published"
  [[ "$(cat "$partial_site/$published")" == "$published contents" ]] ||
    fail "partial publication of $published changed its input"
  [[ ! -e "$partial_site.lock" ]] || fail "partial publication of $published created a site lock"
  rm -rf -- "$partial_root"
done

# `site init` sees the private, management and inference addresses it would lock the host to:
# inherited ones, and --private-addr; a flag replaces its own inherited variable.
init_env="$work/init-listener-addrs"
expect_init_env() {
  local expected=$1 seen
  shift
  rm -f -- "$init_env"
  env -u DOTCORE_CATALOG_DIR -u DOTCORE_PRIVATE_ADDR -u DOTCORE_MANAGEMENT_ADDR -u DOTCORE_INFERENCE_ADDR \
    DOTCORE_SITE_DIR="$declined_root/.dotcore/site" DOTCORE_PACKAGE_INIT_ENV_FILE="$init_env" \
    "$@" >/dev/null 2>&1 || true
  seen=$(cat "$init_env")
  [[ "$seen" == "$expected" ]] || fail "site init saw '$seen', expected '$expected' (arguments: $*)"
}
inherited=(DOTCORE_PRIVATE_ADDR=127.0.0.1:6003 DOTCORE_MANAGEMENT_ADDR=127.0.0.1:6001
  DOTCORE_INFERENCE_ADDR=127.0.0.1:6002)
expect_init_env 'unset unset unset' "$init_fixture"
expect_init_env '127.0.0.1:6003 127.0.0.1:6001 127.0.0.1:6002' "${inherited[@]}" "$init_fixture"
expect_init_env '0.0.0.0:50053 unset unset' "${inherited[@]}" "$init_fixture" \
  --private-addr 0.0.0.0:50053 --management-port 6101 --inference-port 6102
rm -rf -- "$markers"
mkdir -m 700 "$DOTCORE_SITE_DIR"
cp -R "${DOTCORE_PACKAGE_SITE_FIXTURE:?Site fixture is required}"/. "$DOTCORE_SITE_DIR/"
chmod -R u+rwX "$DOTCORE_SITE_DIR"
site_before=$(sha256sum "$DOTCORE_SITE_DIR/site.yaml" "$DOTCORE_SITE_DIR/node.yaml")
cp -R "$DOTCORE_SITE_DIR" "$work/site-before"

# Asks the management listener on a loopback port for the catalog over gRPC-Web, as the console
# does through Caddy.
list_models() {
  printf '\0\0\0\0\0' | curl -fsS --max-time 5 -H 'Content-Type: application/grpc-web+proto' \
    -H 'x-grpc-web: 1' --data-binary @- \
    "http://127.0.0.1:$1/dotcore.catalog.v1.CatalogService/ListModels"
}

# The startup banner names the console and the API at the node host, and none of the internal
# listeners or paths; redirected to a file, it carries no color codes.
assert_banner() {
  local log=$1 host=$2 public=$3 banner line
  banner=$(sed -n '/Dotcore is running/,/Ctrl+C to stop/p' "$log")
  [[ -n "$banner" ]] || fail "no startup banner: $(tail -n 20 "$log")"
  for line in "  Dotcore is running" "  Console     http://$host:$public" \
    "  OpenAI API  http://$host:$public/v1" \
    "  Warning: no login or TLS yet. Anyone who can reach $host" \
    "  can use your models and change your site."; do
    grep -Fxq -- "$line" <<< "$banner" || fail "banner lacks '$line': $banner"
  done
  ! grep -q $'\e' <<< "$banner" || fail "redirected banner has color codes: $banner"
  ! grep -Eq 'Management API|Manager private listener|Direct inference API|Master catalog|Cache:' <<< "$banner" ||
    fail "banner names internal listeners or paths: $banner"
}

# The listeners bind only the node host (127.0.0.1 in these sites). Linux routes all of 127.0.0.0/8
# to this machine, so another loopback address must refuse them; macOS configures no such address.
assert_node_host_only() {
  local port
  if [[ "$(uname -s)" != Linux ]]; then
    echo "package smoke: skipping the other-address check: this system has no second loopback address" >&2
    return 0
  fi
  for port in "$@"; do
    ! curl -s --max-time 2 -o /dev/null "http://127.0.0.2:$port/" ||
      fail "the listener on port $port answered on 127.0.0.2, not only on the node host"
  done
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

# Retry only a diagnosed bind collision; other startup failures must stay visible.
ready=false
for ((retry = 0; retry < 5; retry++)); do
  read -r public inference management < <(ports)
  env -u DOTCORE_CATALOG_DIR "$launcher" --public-port "$public" --inference-port "$inference" \
    --management-port "$management" \
    >"$work/launcher.out" 2>"$work/launcher.err" &
  launcher_pid=$!
  for ((attempt = 0; attempt < 300; attempt++)); do
    if curl -fs --max-time 1 "http://127.0.0.1:$public/" >"$work/index" &&
      curl -fs --max-time 1 "http://127.0.0.1:$public/v1/models" >"$work/public-models" &&
      curl -fs --max-time 1 "http://127.0.0.1:$inference/v1/models" >"$work/direct-models" &&
      grep -Fq 'Dotcore is running' "$work/launcher.err"; then
      ready=true
      break
    fi
    if ! kill -0 "$launcher_pid" 2>/dev/null; then break; fi
    sleep .1
  done
  if [[ "$ready" == true ]]; then break; fi
  if kill -0 "$launcher_pid" 2>/dev/null; then fail 'launcher timed out before readiness'; fi
  wait "$launcher_pid" || true
  launcher_pid=''
  if ((retry < 4)) && grep -Eiq 'address already in use|EADDRINUSE' "$work/launcher.err"; then
    [[ -z $(find "$XDG_RUNTIME_DIR" -mindepth 1 -print -quit) ]] || fail 'bind failure left a private stage'
    continue
  fi
  fail 'launcher exited before readiness'
done
[[ "$ready" == true ]] || fail 'bind-collision retries exhausted'
assert_node_host_only "$public" "$inference"

# Exercise the packaged Caddy routes and both selected listeners, not backend semantics.
grep -Fqi '<!doctype html>' "$work/index" || fail 'production frontend was not served'
[[ $(cat "$work/public-models") == "$(cat "$work/direct-models")" ]] || fail 'public and direct inference disagree'
python3 - "$work/public-models" <<'PY'
import json
from pathlib import Path
import sys
assert json.loads(Path(sys.argv[1]).read_text()) == {"object": "list", "data": []}, "fresh route table changed"
PY
curl -fsS "http://127.0.0.1:$public/config.js" |
  grep -Fq 'window.__DOTCORE_CONFIG__ = {"managementOrigin":"same-origin","inferenceOrigin":"same-origin"};' ||
  fail 'packaged frontend configuration is not same-origin'
list_models "$public" >"$work/list-models" || fail 'Management did not respond through Caddy'
grep -aqE 'grpc-status: ?0' "$work/list-models" || fail 'Management did not respond through Caddy'
list_models "$management" | grep -aqE 'grpc-status: ?0' || fail 'launcher ignored --management-port'
[[ $(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:$public/console") == 200 ]] ||
  fail 'Console fallback is unavailable'
[[ $(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:$public/v1/unknown") == 404 ]] ||
  fail 'unknown inference path fell through to the frontend'
catalog_model=$(sed -nE 's/^modelId: "(.*)"$/\1/p' \
  "$(find "${DOTCORE_PACKAGE_CATALOG:?Catalog is required}/models" -name '*.yaml' | sort | head -n 1)")
[[ -n "$catalog_model" ]] || fail 'packaged catalog has no model to look for'
grep -aFq "$catalog_model" "$work/list-models" || fail "packaged catalog was not selected: no $catalog_model"
assert_banner "$work/launcher.err" 127.0.0.1 "$public"

# A normal stop must release the private stage and every listener.
kill -TERM "$launcher_pid"
wait "$launcher_pid" || fail 'launcher did not stop normally'
launcher_pid=''
[[ -z $(find "$XDG_RUNTIME_DIR" -mindepth 1 -print -quit) ]] || fail 'private frontend stage remains'
[[ "$site_before" == "$(sha256sum "$DOTCORE_SITE_DIR/site.yaml" "$DOTCORE_SITE_DIR/node.yaml")" ]] ||
  fail "existing site inputs changed: $(diff -ru "$work/site-before" "$DOTCORE_SITE_DIR" | head -n 40) launcher stderr: $(tail -n 5 "$work/launcher.err")"
python3 - "$public" "$inference" "$management" <<'PY'
import socket
import sys

public, inference, management = map(int, sys.argv[1:4])
for host, port in [("0.0.0.0", public), ("0.0.0.0", inference), ("127.0.0.1", management)]:
    with socket.socket() as listener:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind((host, port))
        listener.listen()
PY

# A node added with a private address records its port; the manager binds that port when the
# launcher gets it as --private-addr or inherits it, and the site default otherwise.
dotc=${DOTCORE_PACKAGE_BACKEND_COMMAND:?backend command is required}
port_site="$work/port-site"
port_node="$work/port-node"
port_token="$work/port-join.token"
env -u DOTCORE_PRIVATE_ADDR -u DOTCORE_MANAGEMENT_ADDR -u DOTCORE_INFERENCE_ADDR \
  "$dotc" site init --non-interactive \
  --site-dir "$port_site" --host 127.0.0.1 --inference simulated --applications simulated \
  >"$work/port-init.out" 2>"$work/port-init.err" ||
  fail "site init failed: $(cat "$work/port-init.err")"
read -r private_port _ _ < <(ports)

DOTCORE_PRIVATE_ADDR="127.0.0.1:$private_port" "$dotc" node add --site-dir "$port_site" --yes \
  --token-file "$port_token" --timeout 120s >"$work/port-add.out" 2>"$work/port-add.err" &
add_pid=$!
for ((attempt = 0; attempt < 300; attempt++)); do
  if [[ -s "$port_token" ]] || ! kill -0 "$add_pid" 2>/dev/null; then break; fi
  sleep .1
done
[[ -s "$port_token" ]] || fail "node add published no token: $(cat "$work/port-add.err")"
env -u DOTCORE_PRIVATE_ADDR "$dotc" node join --yes --site-dir "$port_node" --name 'Port node' \
  --host 127.0.0.3 --inference simulated --applications simulated --token-file "$port_token" \
  >"$work/port-join.out" 2>"$work/port-join.err" ||
  fail "node join failed: $(cat "$work/port-join.err")"
wait "$add_pid" || fail "node add failed: $(cat "$work/port-add.err")"
add_pid=''
recorded_port=$(sed -nE 's|^ *privateEndpoint: https://[^:/]+:([0-9]+)/?$|\1|p' "$port_node/worker.yaml")
[[ "$recorded_port" == "$private_port" ]] ||
  fail "worker.yaml recorded manager private port ${recorded_port:-<none>}, expected $private_port"

# Starts the manager on the port site in one listener mode and sets bound_port once its private
# listener accepts connections and the launcher printed its summary. Every listener binds 127.0.0.1,
# the only loopback address macOS configures by default. A diagnosed bind collision retries.
bound_port=''
start_port_manager() {
  local mode=$1 log="$work/port-$1.err" public inference management bound='' ready retry
  local -a run
  for ((retry = 0; retry < 5; retry++)); do
    read -r public inference management < <(ports)
    case "$mode" in
      flagged)
        run=(env -u DOTCORE_PRIVATE_ADDR "$launcher" --public-port "$public" --inference-port "$inference"
          --management-port "$management" --private-addr "127.0.0.1:$private_port") ;;
      inherited)
        run=(env DOTCORE_PUBLIC_PORT="$public" DOTCORE_INFERENCE_ADDR="127.0.0.1:$inference"
          DOTCORE_MANAGEMENT_ADDR="127.0.0.1:$management" DOTCORE_PRIVATE_ADDR="127.0.0.1:$private_port"
          "$launcher") ;;
      default)
        run=(env -u DOTCORE_PRIVATE_ADDR "$launcher" --public-port "$public" --inference-port "$inference"
          --management-port "$management") ;;
    esac
    env -u DOTCORE_CATALOG_DIR DOTCORE_SITE_DIR="$port_site" "${run[@]}" >/dev/null 2>"$log" &
    launcher_pid=$!
    ready=false
    for ((attempt = 0; attempt < 300; attempt++)); do
      # Backend logs color their fields even without a terminal.
      bound=$(sed -E $'s/\x1b\\[[0-9;]*m//g' "$log" |
        sed -nE 's/.*manager private listener bound.* address=[^ ]*:([0-9]+).*/\1/p' | head -n 1)
      if [[ -n "$bound" ]] && grep -Fq 'Dotcore is running' "$log" &&
        python3 -c 'import socket, sys; socket.create_connection(("127.0.0.1", int(sys.argv[1])), 1).close()' "$bound" 2>/dev/null; then
        ready=true
        break
      fi
      if ! kill -0 "$launcher_pid" 2>/dev/null; then break; fi
      sleep .1
    done
    if "$ready"; then break; fi
    if kill -0 "$launcher_pid" 2>/dev/null; then
      fail "$mode manager bound no reachable private listener: $(grep -a 'private listener' "$log" || tail -n 20 "$log")"
    fi
    wait "$launcher_pid" || true
    launcher_pid=''
    if ((retry < 4)) && grep -Eiq 'address already in use|EADDRINUSE' "$log"; then continue; fi
    fail "$mode manager exited before readiness: $(tail -n 20 "$log")"
  done
  "$ready" || fail "$mode manager bind-collision retries exhausted"

  # An inherited run serves every listener where its variables say, and its summary names them.
  if [[ "$mode" == inherited ]]; then
    curl -fsS --max-time 5 "http://127.0.0.1:$public/" | grep -Fqi '<!doctype html>' ||
      fail 'inherited DOTCORE_PUBLIC_PORT does not serve the console'
    curl -fsS --max-time 5 "http://127.0.0.1:$inference/v1/models" >/dev/null ||
      fail 'inherited DOTCORE_INFERENCE_ADDR does not serve inference'
    list_models "$public" | grep -aqE 'grpc-status: ?0' ||
      fail 'inherited DOTCORE_MANAGEMENT_ADDR is not reached through Caddy'
    list_models "$management" | grep -aqE 'grpc-status: ?0' ||
      fail 'inherited DOTCORE_MANAGEMENT_ADDR is not where management listens'
    assert_banner "$log" 127.0.0.1 "$public"
  fi

  kill -TERM "$launcher_pid"
  wait "$launcher_pid" || fail "$mode manager did not stop normally"
  launcher_pid=''
  bound_port=$bound
}

start_port_manager flagged
[[ "$bound_port" == "$recorded_port" ]] ||
  fail "manager private listener bound port $bound_port with --private-addr, joined node recorded $recorded_port"

# The dev shell case: an inherited DOTCORE_PRIVATE_ADDR alone gives the manager the recorded port.
start_port_manager inherited
[[ "$bound_port" == "$recorded_port" ]] ||
  fail "manager private listener bound port $bound_port with an inherited address, joined node recorded $recorded_port"

# With neither, the manager falls back to 50053, so the comparisons above detect a mismatch.
start_port_manager default
[[ "$bound_port" == 50053 && "$bound_port" != "$recorded_port" ]] ||
  fail "manager without a private address bound port $bound_port, expected 50053"
