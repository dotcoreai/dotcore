# shellcheck shell=bash
# Fake Docker CLI (and standalone buildx) for loader tests. State lives in $FAKE_STATE: one inspect
# document per image reference under images/, failure switches as marker files, and every call
# appended to calls with the daemon it targets.

state=$FAKE_STATE
echo "$(basename "$0") $* @${DOCKER_HOST:-}" >>"$state/calls"

image_file() {
  printf '%s/images/%s.json' "$state" "$(printf '%s' "$1" | tr '/:@' '___')"
}

# Stores the inspect document on stdin as image $1, with an ID derived from its content.
store_image() {
  local document
  document=$(jq -cS 'del(.Id)')
  jq --arg id "sha256:$(printf '%s' "$document" | sha256sum | cut -d' ' -f1)" '{Id: $id} + .' <<<"$document" >"$(image_file "$1")"
}

# Writes an image named $1 for platform $2 with diff IDs $3 (JSON) and recipe label $4.
write_image() {
  jq -n --arg platform "$2" --argjson layers "$3" --arg recipe "$4" '{
    Os: ($platform | split("/")[0]), Architecture: ($platform | split("/")[1]),
    RootFS: {Layers: $layers},
    Config: {Labels: (if $recipe == "" then null else {"dotcore.recipe": $recipe} end)}
  }' | store_image "$1"
}

# Diff IDs a source produces: the expected ones unless the test asks for a mismatch.
layers() {
  if [ -e "$state/wrong-layers" ]; then echo '["sha256:wrong"]'; else cat "$state/layers"; fi
}

if [ "$(basename "$0")" = docker-buildx ]; then
  [ ! -e "$state/build-fails" ] || { echo "build failed" >&2; exit 1; }
  prev="" tag="" label="" platform=""
  for arg in "$@"; do
    case "$prev" in
      --tag) tag=$arg ;;
      --label) label=${arg#dotcore.recipe=} ;;
      --platform) platform=$arg ;;
    esac
    prev=$arg
  done
  write_image "$tag" "$platform" "$(layers)" "$label"
  exit 0
fi

case "${1:-} ${2:-}" in
  "context show")
    echo "fake"
    ;;
  "version --format")
    [ ! -e "$state/daemon-down" ] || exit 1
    echo "29.0.0"
    ;;
  "image inspect")
    file=$(image_file "${!#}")
    [ -e "$file" ] || { echo "No such image: ${!#}" >&2; exit 1; }
    cat "$file"
    ;;
  "pull --platform")
    [ ! -e "$state/pull-fails" ] || { echo "pull failed" >&2; exit 1; }
    write_image "$4" "$3" "$(layers)" ""
    ;;
  "tag "*)
    cp "$(image_file "$2")" "$(image_file "$3")"
    ;;
  "image rm")
    file=$(image_file "$3")
    [ -e "$file" ] || { echo "No such image: $3" >&2; exit 1; }
    rm "$file"
    ;;
  "load ")
    # Unpacks the archive like Docker: the Docker manifest names the image and its config, whose
    # platform, diff IDs, and labels become the loaded image. The OCI index must agree.
    archive="$state/archive"
    rm -rf "$archive"
    mkdir "$archive"
    tar -x -f - -C "$archive"
    find "$archive" -type l >"$state/links"
    entry=$(jq -c '.[0]' "$archive/manifest.json")
    reference=$(jq -r '.RepoTags[0]' <<<"$entry")
    config="$archive/$(jq -r .Config <<<"$entry")"
    for layer in $(jq -r '.Layers[]' <<<"$entry"); do
      [ -f "$archive/$layer" ] || { echo "archive lacks layer $layer" >&2; exit 1; }
    done
    descriptor=$(jq -c '.manifests[0]' "$archive/index.json")
    blob="$archive/blobs/sha256/$(jq -r '.digest | ltrimstr("sha256:")' <<<"$descriptor")"
    [ "sha256:$(sha256sum "$blob" | cut -d' ' -f1)" = "$(jq -r .digest <<<"$descriptor")" ] ||
      { echo "index manifest digest mismatch" >&2; exit 1; }
    [ "$(jq -r '.annotations["io.containerd.image.name"]' <<<"$descriptor")" = "docker.io/$reference" ] ||
      { echo "index names another image" >&2; exit 1; }
    jq '{Os: .os, Architecture: .architecture, RootFS: {Layers: .rootfs.diff_ids}, Config: {Labels: .config.Labels}}' \
      "$config" | store_image "$reference"
    echo "Loaded image: $reference"
    ;;
  "save "*)
    mkdir -p "$state/save"
    echo "saved $2" >"$state/save/index.json"
    tar -c -C "$state/save" .
    ;;
  *)
    echo "fake docker: unexpected call: $*" >&2
    exit 2
    ;;
esac
