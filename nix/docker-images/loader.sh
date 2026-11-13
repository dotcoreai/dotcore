# shellcheck shell=bash
# Loader for one Docker image derivation. IMAGE_JSON (prepended by the library) describes the image;
# DOCKER_IMAGE_PACKAGE (set by the package wrapper) is the package's store path.
#
#   load (default)  make the image available in the Docker daemon under its launch reference and
#                   print the result as one JSON line: launch reference, image ID, resolution path,
#                   whether the identity was verified, and platform
#   store-build     Dockerfile images: build, add the image to the Nix store, print the record entry
#   info            print the image metadata

meta() {
  jq -r "$1" "$IMAGE_JSON"
}

log() {
  printf 'docker-image-%s: %s\n' "$name" "$*" >&2
}

fail() {
  log "error: $*"
  exit 1
}

name=$(meta .name)
kind=$(meta .kind)
platform=$(meta .platform)
reference=$(meta .reference)

# Launch reference: derived from the package store path alone (`<hash>-<name>`), so the backend
# computes it from a pinned store path without running the loader.
[ -n "${DOCKER_IMAGE_PACKAGE:-}" ] || fail "DOCKER_IMAGE_PACKAGE is unset; run the package's bin/ program"
package=$(basename "$DOCKER_IMAGE_PACKAGE")
launch_reference="dotcore-nix/${package#*-}:${package%%-*}"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

main() {
  case "${1:-load}" in
  load) load ;;
  store-build) store_build ;;
  info) cat "$IMAGE_JSON" ;;
  *) fail "unknown command '$1' (expected load, store-build, or info)" ;;
  esac
}

# Resolution order: already present with the expected identity, stored image from the Nix store or
# a substituter, present build of the recipe (Dockerfile images whose stored image is unavailable),
# source.
load() {
  connect

  local image path
  image=$(inspect) || image=""

  if [ -n "$image" ] && matches "$image" identity; then
    log "present: $reference"
    path=present
  elif stored_available; then
    log "stored: copying from the Nix store"
    copy_stored
    verify identity
    path=stored
  elif [ -n "$image" ] && matches "$image" recipe; then
    log "present: $reference (local build; the stored image is unavailable)"
    path=present
  else
    from_source
    verify recipe
    case "$kind" in
    registry) path=pull ;;
    dockerfile) path=build ;;
    esac
  fi

  docker tag "$reference" "$launch_reference"
  result "$image" "$path"
}

store_build() {
  [ "$kind" = dockerfile ] || fail "store-build only applies to Dockerfile images"
  require_nix
  connect

  local image
  if image=$(inspect) && matches "$image" recipe; then
    log "present: $reference"
  else
    build_local
    verify recipe
  fi

  log "exporting to the Nix store"
  mkdir "$work/image"
  docker save "$reference" | tar -x -C "$work/image"
  local hash path
  hash=$(nix hash path --algo sha256 "$work/image")
  path=$(nix store add --name "$(meta .dockerfile.storeName)" --mode nar --hash-algo sha256 "$work/image")

  # Rooted so garbage collection keeps it for as long as the roots folder keeps the link. Recipes in
  # a checkout pass DOTCORE_DOCKER_IMAGE_ROOTS_DIR so the root goes with the checkout; one link per
  # image and platform means a rebuild replaces the previous build's root instead of adding one.
  local roots="${DOTCORE_DOCKER_IMAGE_ROOTS_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/dotcore/docker-images}"
  mkdir -p "$roots"
  nix build --out-link "$roots/docker-image-$name-${platform//\//-}" "$path"
  log "stored: $path (GC root in $roots)"

  inspect | jq --arg platform "$platform" --arg recipeHash "$(meta .dockerfile.recipeHash)" --arg hash "$hash" \
    '{($platform): {recipeHash: $recipeHash, hash: $hash, diffIds: .RootFS.Layers}}'
}

# Targets DOCKER_HOST, or the active Docker context when it is unset, as the Docker CLI and buildx
# both do.
connect() {
  docker version --format '{{.Server.Version}}' >/dev/null 2>&1 ||
    fail "Docker daemon unreachable at ${DOCKER_HOST:-the $(docker context show 2>/dev/null || echo default) context}"
}

inspect() {
  docker image inspect --format '{{json .}}' "$reference" 2>/dev/null
}

# Checks an image against the expected platform, recipe label, and recorded identity. `identity`
# requires the recorded diff IDs whenever the image has them; `recipe` accepts any local build of a
# Dockerfile recipe, whose bytes are node-specific.
matches() {
  jq -e --slurpfile m "$IMAGE_JSON" --arg check "$2" '
    $m[0] as $m
    | (.Os + "/" + .Architecture) == $m.platform
      and (if $m.kind == "dockerfile" then (.Config.Labels["dotcore.recipe"] // "") == $m.dockerfile.recipeHash else true end)
      and (if ($m.kind == "registry" or $check == "identity") and $m.identity != null
           then .RootFS.Layers == $m.identity.diffIds else true end)
  ' <<<"$1" >/dev/null
}

# Inspects the loaded image into the caller's `image` and checks it.
verify() {
  image=$(inspect) || fail "$reference missing after loading"
  matches "$image" "$1" ||
    fail "$reference does not match the expected platform $platform or identity: $(jq -c '{os: .Os, architecture: .Architecture, diffIds: .RootFS.Layers, labels: .Config.Labels}' <<<"$image")"
}

# Prints the load result. The identity is verified only when one is recorded and the image has it.
result() {
  local verified=false
  if [ "$(meta '.identity != null')" = true ] && matches "$1" identity; then
    verified=true
  fi
  jq -cn --argjson image "$1" --arg path "$2" --argjson verified "$verified" \
    --arg reference "$launch_reference" --arg platform "$platform" \
    '{reference: $reference, imageId: $image.Id, path: $path, identityVerified: $verified, platform: $platform}'
}

# Realizes the stored image from the local store or a substituter, never building or fetching it
# through Nix: a bare store path can only be substituted.
stored_available() {
  [ "$(meta '.stored != null')" = true ] || return 1

  if ! command -v nix >/dev/null; then
    log "nix not found; skipping the stored image"
    return 1
  fi

  local paths
  mapfile -t paths < <(meta '.stored.layout // empty, (.stored.blobs // [] | .[].path)')
  if ! nix build --out-link "$work/stored" "${paths[@]}" 2>"$work/nix.log"; then
    if grep -q "no substituter" "$work/nix.log"; then
      log "stored image not in the Nix store or any substituter"
    else
      log "stored image not available: $(tail -n 1 "$work/nix.log")"
    fi
    return 1
  fi
}

# Streams the stored image into the daemon as a `docker save` archive, dereferencing store links so
# no image copy is written. Registry images are assembled from their blob paths and pinned manifest.
copy_stored() {
  local layout
  layout=$(meta '.stored.layout // empty')

  if [ -z "$layout" ]; then
    layout="$work/layout"
    assemble "$layout"
  fi

  tar -c -h -C "$layout" . | docker load >&2
}

# Builds the archive layout `docker save` produces: an OCI image layout for containerd image stores
# plus the Docker manifest the classic image store reads, both naming the stable reference.
assemble() {
  local layout=$1 manifest digest path
  manifest=$(meta .stored.manifest)
  digest=$(sha256sum "$manifest" | cut -d' ' -f1)

  mkdir -p "$layout/blobs/sha256"
  cp "$manifest" "$layout/blobs/sha256/$digest"
  while read -r blob path; do
    ln -s "$path" "$layout/blobs/sha256/${blob#sha256:}"
  done < <(meta '.stored.blobs[] | "\(.digest) \(.path)"')

  echo '{"imageLayoutVersion":"1.0.0"}' >"$layout/oci-layout"
  jq -n --arg digest "sha256:$digest" --argjson size "$(wc -c <"$manifest")" \
    --arg mediaType "$(jq -r .mediaType "$manifest")" --arg reference "$reference" '
    {schemaVersion: 2, mediaType: "application/vnd.oci.image.index.v1+json", manifests: [{
      mediaType: $mediaType, digest: $digest, size: $size,
      annotations: {
        "io.containerd.image.name": ("docker.io/" + $reference),
        "org.opencontainers.image.ref.name": ($reference | split(":") | last)
      }
    }]}' >"$layout/index.json"
  jq --arg reference "$reference" '[{
    Config: ("blobs/sha256/" + (.config.digest | ltrimstr("sha256:"))),
    RepoTags: [$reference],
    Layers: [.layers[].digest | "blobs/sha256/" + ltrimstr("sha256:")]
  }]' "$manifest" >"$layout/manifest.json"
}

from_source() {
  case "$kind" in
  registry)
    local source
    source="$(meta .registry.repository)@$(meta .registry.digest)"
    log "pulling $source"
    docker pull --platform "$platform" "$source" >&2
    docker tag "$source" "$reference"
    # The image keeps its source tag; dropping the pull's digest reference leaves only
    # dotcore-nix names on it, so cleanup can free it once no loader uses it.
    docker image rm "$source" >/dev/null || log "could not remove the pull reference $source"
    ;;
  dockerfile) build_local ;;
  esac
}

build_local() {
  local context args builder=default
  context=$(meta .dockerfile.context)
  # Each Docker context's own builder builds on its daemon, whatever builder the user selected;
  # DOCKER_HOST selects the `default` context.
  if [ -z "${DOCKER_HOST:-}" ]; then
    builder=$(docker context show)
  fi
  mapfile -t args < <(meta '(.dockerfile.buildArgs | to_entries[] | "--build-arg=\(.key)=\(.value)"), (.dockerfile.target // empty | "--target=\(.)")')

  log "building from $context"
  docker-buildx build \
    --builder "$builder" \
    --platform "$platform" \
    --load \
    --label "dotcore.recipe=$(meta .dockerfile.recipeHash)" \
    --tag "$reference" \
    --file "$context/$(meta .dockerfile.file)" \
    "${args[@]}" \
    "$context" >&2
}

require_nix() {
  command -v nix >/dev/null || fail "nix not found on PATH"
}

main "$@"
