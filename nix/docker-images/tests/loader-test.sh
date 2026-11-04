# shellcheck shell=bash
# Runs the real loader of the registry ($REGISTRY) and Dockerfile ($DOCKERFILE, and $RECORDED with a
# current store build record) fixtures against the fake Docker and Nix programs, asserting exit
# status, stdout, and the exact Docker calls per scenario. It writes samples of the metadata and
# result formats the backend reads to $SAMPLES.

registry_json="$REGISTRY/share/dotcore/image.json"
dockerfile_json="$DOCKERFILE/share/dotcore/image.json"
registry_ref=$(jq -r .reference "$registry_json")
dockerfile_ref=$(jq -r .reference "$dockerfile_json")
platform=$(jq -r .platform "$registry_json")
recipe=$(jq -r .dockerfile.recipeHash "$dockerfile_json")
source="$(jq -r .registry.repository "$registry_json")@$(jq -r .registry.digest "$registry_json")"
stored_paths=$(jq -r '.stored.blobs | map(.path) | join(" ")' "$registry_json")
recorded_layout=$(jq -r .stored.layout "$RECORDED/share/dotcore/image.json")
host=""

# Launch reference of a package: `dotcore-nix/<store path name>:<store path hash>`.
launch_of() {
  local package
  package=$(basename "$1")
  echo "dotcore-nix/${package#*-}:${package%%-*}"
}
registry_launch=$(launch_of "$REGISTRY")
dockerfile_launch=$(launch_of "$DOCKERFILE")
recorded_launch=$(launch_of "$RECORDED")

export FAKE_STATE
failures=0

# Starts a scenario with an empty daemon expecting the fixture's platform and diff IDs.
scenario() {
  current=$1
  FAKE_STATE=$(mktemp -d)
  mkdir "$FAKE_STATE/images"
  echo "$platform" >"$FAKE_STATE/platform"
  jq -c '.identity.diffIds' "$registry_json" >"$FAKE_STATE/layers"
  : >"$FAKE_STATE/calls"
}

# Runs a loader, capturing its exit status, stdout, and stderr.
run() {
  set +e
  "$@" >"$FAKE_STATE/stdout" 2>"$FAKE_STATE/stderr"
  status=$?
  set -e
}

fail() {
  echo "FAIL [$current]: $*" >&2
  echo "--- stderr" >&2
  cat "$FAKE_STATE/stderr" >&2
  echo "--- calls" >&2
  cat "$FAKE_STATE/calls" >&2
  failures=$((failures + 1))
}

expect_status() {
  [ "$status" = "$1" ] || fail "exit status $status, expected $1"
}

expect_stdout() {
  [ "$(cat "$FAKE_STATE/stdout")" = "$1" ] || fail "stdout '$(cat "$FAKE_STATE/stdout")', expected '$1'"
}

# Expects the load result line for launch reference $1, resolution path $2, and identity
# verification $3, naming the image the fake daemon holds under it.
expect_result() {
  local id
  id=$(jq -r .Id "$FAKE_STATE/images/$(printf '%s' "$1" | tr '/:@' '___').json")
  expect_stdout "$(jq -cn --arg reference "$1" --arg id "$id" --arg path "$2" --argjson verified "$3" --arg platform "$platform" \
    '{reference: $reference, imageId: $id, path: $path, identityVerified: $verified, platform: $platform}')"
}

expect_layers() {
  local layers
  layers=$(jq -c .RootFS.Layers "$FAKE_STATE/images/$(printf '%s' "$1" | tr '/:@' '___').json")
  [ "$layers" = "$2" ] || fail "image layers $layers, expected $2"
}

expect_stderr() {
  grep -qF -- "$1" "$FAKE_STATE/stderr" || fail "stderr lacks '$1'"
}

# Compares the calls, with the loader's temporary directory normalized, to the expected lines.
expect_calls() {
  local expected actual
  expected=$(printf '%s\n' "$@")
  actual=$(sed -E -e 's#--out-link [^ ]+#--out-link WORK#' -e 's#[^ ]+/image( |$)#WORKDIR/image\1#' "$FAKE_STATE/calls")
  [ "$actual" = "$expected" ] || fail "calls differ:$(diff <(echo "$expected") <(echo "$actual") || true)"
}

connect_calls=(
  "docker version --format {{.Server.Version}} @$host"
)
inspect_registry="docker image inspect --format {{json .}} $registry_ref @$host"
inspect_dockerfile="docker image inspect --format {{json .}} $dockerfile_ref @$host"
nix_build="nix build --out-link WORK $stored_paths"
nix_build_recorded="nix build --out-link WORK $recorded_layout"
tag_registry="docker tag $registry_ref $registry_launch @$host"
tag_dockerfile="docker tag $dockerfile_ref $dockerfile_launch @$host"
tag_recorded="docker tag $dockerfile_ref $recorded_launch @$host"
build_call="docker-buildx build --builder fake --platform $platform --load --label dotcore.recipe=$recipe --tag $dockerfile_ref --file $(jq -r .dockerfile.context "$dockerfile_json")/Dockerfile --build-arg=GREETING=hello $(jq -r .dockerfile.context "$dockerfile_json") @$host"

scenario "registry: lazy pull when the stored image is unavailable"
touch "$FAKE_STATE/nix-fails"
run "$REGISTRY/bin/docker-image-fixture-busybox"
expect_status 0
expect_result "$registry_launch" pull true
expect_stderr "stored image not in the Nix store or any substituter"
expect_calls "${connect_calls[@]}" "$inspect_registry" "$nix_build" \
  "docker pull --platform $platform $source @$host" \
  "docker tag $source $registry_ref @$host" \
  "docker image rm $source @$host" \
  "$inspect_registry" "$tag_registry"

scenario "registry: present image is reused"
touch "$FAKE_STATE/nix-fails"
run "$REGISTRY/bin/docker-image-fixture-busybox"
: >"$FAKE_STATE/calls"
run "$REGISTRY/bin/docker-image-fixture-busybox"
expect_status 0
expect_result "$registry_launch" present true
expect_stderr "present: $registry_ref"
expect_calls "${connect_calls[@]}" "$inspect_registry" "$tag_registry"
mkdir -p "$SAMPLES"
cp "$FAKE_STATE/stdout" "$SAMPLES/load-result.json"
jq . "$registry_json" >"$SAMPLES/registry-image.json"
jq . "$dockerfile_json" >"$SAMPLES/dockerfile-image.json"

# The fake load registers the image from the archive's Docker manifest and config, checking the
# OCI index, so a wrong layout fails the loader's verification.
scenario "registry: stored image is streamed into docker load"
run "$REGISTRY/bin/docker-image-fixture-busybox"
expect_status 0
expect_result "$registry_launch" stored true
expect_stderr "stored: copying from the Nix store"
expect_calls "${connect_calls[@]}" "$inspect_registry" "$nix_build" "docker load @$host" "$inspect_registry" "$tag_registry"
[ ! -s "$FAKE_STATE/links" ] || fail "archive holds links instead of blobs: $(cat "$FAKE_STATE/links")"
[ -e "$FAKE_STATE/archive/oci-layout" ] || fail "archive lacks oci-layout"

scenario "registry: stored image skipped without nix"
run env PATH="$NO_NIX_PATH" "$REGISTRY/bin/docker-image-fixture-busybox"
expect_status 0
expect_result "$registry_launch" pull true
expect_stderr "nix not found; skipping the stored image"
expect_calls "${connect_calls[@]}" "$inspect_registry" \
  "docker pull --platform $platform $source @$host" \
  "docker tag $source $registry_ref @$host" \
  "docker image rm $source @$host" \
  "$inspect_registry" "$tag_registry"

scenario "registry: pulled image with other layers is rejected"
touch "$FAKE_STATE/nix-fails" "$FAKE_STATE/wrong-layers"
run "$REGISTRY/bin/docker-image-fixture-busybox"
expect_status 1
expect_stdout ""
expect_stderr "does not match the expected platform $platform or identity"

scenario "registry: pull failure"
touch "$FAKE_STATE/nix-fails" "$FAKE_STATE/pull-fails"
run "$REGISTRY/bin/docker-image-fixture-busybox"
expect_status 1
expect_stdout ""
expect_stderr "pull failed"

scenario "registry: unreachable daemon"
touch "$FAKE_STATE/daemon-down"
run "$REGISTRY/bin/docker-image-fixture-busybox"
expect_status 1
expect_stderr "Docker daemon unreachable at the fake context"

scenario "registry: caller's DOCKER_HOST is used as is"
touch "$FAKE_STATE/nix-fails"
run env DOCKER_HOST=unix:///custom.sock "$REGISTRY/bin/docker-image-fixture-busybox"
expect_status 0
expect_calls "docker version --format {{.Server.Version}} @unix:///custom.sock" \
  "docker image inspect --format {{json .}} $registry_ref @unix:///custom.sock" "$nix_build" \
  "docker pull --platform $platform $source @unix:///custom.sock" \
  "docker tag $source $registry_ref @unix:///custom.sock" \
  "docker image rm $source @unix:///custom.sock" \
  "docker image inspect --format {{json .}} $registry_ref @unix:///custom.sock" \
  "docker tag $registry_ref $registry_launch @unix:///custom.sock"

scenario "dockerfile: local build without a store build record"
run "$DOCKERFILE/bin/docker-image-fixture-build"
expect_status 0
expect_result "$dockerfile_launch" build false
expect_stderr "building from"
expect_calls "${connect_calls[@]}" "$inspect_dockerfile" "docker context show @$host" "$build_call" "$inspect_dockerfile" "$tag_dockerfile"

scenario "dockerfile: present build of the recipe is reused"
run "$DOCKERFILE/bin/docker-image-fixture-build"
: >"$FAKE_STATE/calls"
run "$DOCKERFILE/bin/docker-image-fixture-build"
expect_status 0
expect_result "$dockerfile_launch" present false
expect_calls "${connect_calls[@]}" "$inspect_dockerfile" "$tag_dockerfile"

scenario "dockerfile: image of another recipe under the tag is rebuilt"
jq -n --arg platform "$platform" '{Os: "linux", Architecture: ($platform | split("/")[1]), RootFS: {Layers: []}, Config: {Labels: {"dotcore.recipe": "other"}}}' \
  >"$FAKE_STATE/images/$(printf '%s' "$dockerfile_ref" | tr '/:@' '___').json"
run "$DOCKERFILE/bin/docker-image-fixture-build"
expect_status 0
expect_result "$dockerfile_launch" build false
expect_calls "${connect_calls[@]}" "$inspect_dockerfile" "docker context show @$host" "$build_call" "$inspect_dockerfile" "$tag_dockerfile"

scenario "dockerfile: recorded stored image is streamed into docker load"
run "$RECORDED/bin/docker-image-fixture-build"
expect_status 0
expect_result "$recorded_launch" stored true
expect_layers "$dockerfile_ref" "$RECORDED_DIFF_IDS"
expect_calls "${connect_calls[@]}" "$inspect_dockerfile" "$nix_build_recorded" "docker load @$host" "$inspect_dockerfile" "$tag_recorded"

scenario "dockerfile: present image with the recorded identity is reused"
run "$RECORDED/bin/docker-image-fixture-build"
: >"$FAKE_STATE/calls"
run "$RECORDED/bin/docker-image-fixture-build"
expect_status 0
expect_result "$recorded_launch" present true
expect_calls "${connect_calls[@]}" "$inspect_dockerfile" "$tag_recorded"

scenario "dockerfile: local build with other bytes is replaced by the recorded stored image"
run "$DOCKERFILE/bin/docker-image-fixture-build"
: >"$FAKE_STATE/calls"
run "$RECORDED/bin/docker-image-fixture-build"
expect_status 0
expect_result "$recorded_launch" stored true
expect_layers "$dockerfile_ref" "$RECORDED_DIFF_IDS"
expect_calls "${connect_calls[@]}" "$inspect_dockerfile" "$nix_build_recorded" "docker load @$host" "$inspect_dockerfile" "$tag_recorded"

scenario "dockerfile: local build is kept when the recorded stored image is unavailable"
run "$DOCKERFILE/bin/docker-image-fixture-build"
: >"$FAKE_STATE/calls"
touch "$FAKE_STATE/nix-fails"
run "$RECORDED/bin/docker-image-fixture-build"
expect_status 0
expect_result "$recorded_launch" present false
expect_stderr "local build; the stored image is unavailable"
expect_calls "${connect_calls[@]}" "$inspect_dockerfile" "$nix_build_recorded" "$tag_recorded"

scenario "dockerfile: local build when the recorded stored image is unavailable"
touch "$FAKE_STATE/nix-fails"
run "$RECORDED/bin/docker-image-fixture-build"
expect_status 0
expect_result "$recorded_launch" build false
expect_calls "${connect_calls[@]}" "$inspect_dockerfile" "$nix_build_recorded" "docker context show @$host" "$build_call" "$inspect_dockerfile" "$tag_recorded"

scenario "dockerfile: build failure"
touch "$FAKE_STATE/build-fails"
run "$DOCKERFILE/bin/docker-image-fixture-build"
expect_status 1
expect_stdout ""
expect_stderr "build failed"

scenario "dockerfile: store build prints the record entry"
run env XDG_STATE_HOME="$FAKE_STATE/state" "$DOCKERFILE/bin/docker-image-fixture-build" store-build
expect_status 0
expected=$(jq -cn --arg platform "$platform" --arg recipe "$recipe" --slurpfile layers "$FAKE_STATE/layers" \
  '{($platform): {recipeHash: $recipe, hash: "sha256-fakehash", diffIds: $layers[0]}}')
[ "$(jq -c . "$FAKE_STATE/stdout")" = "$expected" ] || fail "record $(cat "$FAKE_STATE/stdout"), expected $expected"
expect_calls "${connect_calls[@]}" "$inspect_dockerfile" "docker context show @$host" "$build_call" "$inspect_dockerfile" \
  "docker save $dockerfile_ref @$host" \
  "nix hash path --algo sha256 WORKDIR/image" \
  "nix store add --name $(jq -r .dockerfile.storeName "$dockerfile_json") --mode nar --hash-algo sha256 WORKDIR/image" \
  "nix build --out-link WORK $FAKE_STATE/fake-stored" \
  "$inspect_dockerfile"
[ -L "$FAKE_STATE/state/dotcore/docker-images/fake-stored" ] || fail "stored path has no GC root"

scenario "registry: store build is rejected"
run "$REGISTRY/bin/docker-image-fixture-busybox" store-build
expect_status 1
expect_stderr "store-build only applies to Dockerfile images"

# The backend's launch reference unit test uses the same store path.
scenario "launch reference derives from the package store path"
touch "$FAKE_STATE/nix-fails"
run env DOCKER_IMAGE_PACKAGE=/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-fixture-busybox \
  "$REGISTRY_LOADER/bin/docker-image-fixture-busybox"
expect_status 0
expect_result "dotcore-nix/docker-image-fixture-busybox:0123456789abcdfghijklmnpqrsvwxyz" pull true

scenario "launch reference is the same through a link to the program"
touch "$FAKE_STATE/nix-fails"
ln -s "$REGISTRY/bin/docker-image-fixture-busybox" "$FAKE_STATE/linked"
run "$FAKE_STATE/linked"
expect_status 0
expect_result "$registry_launch" pull true

scenario "loader without its package store path"
run "$REGISTRY_LOADER/bin/docker-image-fixture-busybox"
expect_status 1
expect_stdout ""
expect_stderr "DOCKER_IMAGE_PACKAGE is unset"

scenario "unknown command"
run "$REGISTRY/bin/docker-image-fixture-busybox" frobnicate
expect_status 1
expect_stderr "unknown command 'frobnicate'"

if [ "$failures" -gt 0 ]; then
  echo "$failures loader test failure(s)" >&2
  exit 1
fi
echo "loader tests passed"
