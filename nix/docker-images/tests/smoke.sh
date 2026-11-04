# shellcheck shell=bash
# Smoke test of the Docker image loaders against the real Docker daemon, with tiny fixtures: a lazy
# registry pull, a stored registry image loaded with no pull, a local Dockerfile build, and the store
# build mode followed by a load of the recorded stored image. Removes only the fixture tags it
# creates. Expects the BUSYBOX and BUILD loaders (BUSYBOX's stored image realized) and SMOKE_IMAGE, a
# Nix file building the Dockerfile fixture loader from a records file, in the environment.

busybox_ref=$(jq -r .reference "$BUSYBOX/share/dotcore/image.json")
busybox_source="$(jq -r .registry.repository "$BUSYBOX/share/dotcore/image.json")@$(jq -r .registry.digest "$BUSYBOX/share/dotcore/image.json")"
build_ref=$(jq -r .reference "$BUILD/share/dotcore/image.json")

# Launch reference of a package: `dotcore-nix/<store path name>:<store path hash>`.
launch_of() {
  local package
  package=$(basename "$1")
  echo "dotcore-nix/${package#*-}:${package%%-*}"
}
busybox_launch=$(launch_of "$BUSYBOX")
build_launch=$(launch_of "$BUILD")
recorded_launch=""

work=$(mktemp -d)
trap 'rm -rf "$work"; docker image rm "$busybox_ref" "$build_ref" "$busybox_launch" "$build_launch" $recorded_launch >/dev/null 2>&1 || true' EXIT

step() {
  printf '\n== %s\n' "$*" >&2
}

fail() {
  echo "SMOKE FAILED: $*" >&2
  exit 1
}

# Runs a loader, keeping its stdout and stderr, and the image events it caused.
load() {
  local since
  since=$(date +%s)
  local status=0
  "$@" >"$work/stdout" 2>"$work/stderr" || status=$?
  if [ "$status" != 0 ]; then
    cat "$work/stderr" >&2
    fail "loader exited with $status"
  fi
  cat "$work/stderr" >&2
  # A deadline one second ahead bounds the read while events of the last second arrive.
  docker events --since "$since" --until "$(($(date +%s) + 1))" --filter type=image --format '{{.Action}}' >"$work/events"
}

# Expects the load result to name launch reference $1, resolution path $2, and identity verification $3,
# with the image ID Docker reports for the reference.
expect_result() {
  local expected
  expected=$(jq -cn --arg reference "$1" --arg path "$2" --argjson verified "$3" \
    --arg id "$(docker image inspect --format '{{.Id}}' "$1")" \
    '{reference: $reference, imageId: $id, path: $path, identityVerified: $verified}')
  [ "$(jq -c 'del(.platform)' "$work/stdout")" = "$expected" ] || fail "printed '$(cat "$work/stdout")', expected '$expected'"
}

expect_log() {
  grep -qF -- "$1" "$work/stderr" || fail "loader log lacks '$1'"
}

expect_no_pull() {
  ! grep -qx pull "$work/events" || fail "the loader pulled from the registry"
}

remove_tag() {
  docker image rm "$@" >/dev/null 2>&1 || true
}

nix_free_path=$(printf '%s' "$PATH" | tr ':' '\n' | while read -r dir; do [ -x "$dir/nix" ] || printf '%s:' "$dir"; done)
nix_free_path=${nix_free_path%:}

step "registry image: lazy pull when Nix is unavailable"
remove_tag "$busybox_ref" "$busybox_launch" "$busybox_source"
load env PATH="$nix_free_path" "$BUSYBOX/bin/docker-image-fixture-busybox"
expect_result "$busybox_launch" pull true
expect_log "nix not found; skipping the stored image"
expect_log "pulling $busybox_source"
! docker image inspect "$busybox_source" >/dev/null 2>&1 || fail "the pull reference $busybox_source remains"

step "registry image: present image is reused"
load "$BUSYBOX/bin/docker-image-fixture-busybox"
expect_result "$busybox_launch" present true
expect_log "present: $busybox_ref"
expect_no_pull

step "registry image: stored image loads with no pull"
remove_tag "$busybox_ref" "$busybox_launch" "$busybox_source"
load "$BUSYBOX/bin/docker-image-fixture-busybox"
expect_result "$busybox_launch" stored true
expect_log "stored: copying from the Nix store"
expect_no_pull
docker run --rm "$busybox_launch" true || fail "stored registry image does not run"

step "Dockerfile image: local build without a record"
remove_tag "$build_ref" "$build_launch"
load "$BUILD/bin/docker-image-fixture-build"
expect_result "$build_launch" build false
expect_log "building from"

step "Dockerfile image: present build is reused"
load "$BUILD/bin/docker-image-fixture-build"
expect_result "$build_launch" present false
expect_log "present: $build_ref"

step "Dockerfile image: store build mode"
"$BUILD/bin/docker-image-fixture-build" store-build >"$work/records.json" || fail "store build failed"
jq . "$work/records.json" >&2
recorded=$(nix-build --no-out-link "$SMOKE_IMAGE" --arg records "$work/records.json") || fail "loader with the record does not build"

step "Dockerfile image: recorded stored image loads with no build"
recorded_launch=$(launch_of "$recorded")
remove_tag "$build_ref" "$build_launch"
load "$recorded/bin/docker-image-fixture-build"
expect_result "$recorded_launch" stored true
expect_log "stored: copying from the Nix store"
[ "$(docker run --rm "$recorded_launch" cat /greeting)" = hello ] || fail "stored Dockerfile image lacks its build output"

printf '\nsmoke test passed\n' >&2
