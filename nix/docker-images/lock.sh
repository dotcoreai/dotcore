# shellcheck shell=bash
# Prints the lock file of an upstream Docker image: the tag or digest it was locked from and, per
# platform, the exact manifest bytes, their digest, and the layer diff IDs. A platform given as
# `<platform>=<tag | sha256:digest>` is locked from its own reference, for images published as
# separate single-platform manifests.
#
#   docker-images-lock <repository> <tag | sha256:digest> <platform[=ref]>...
#   docker-images-lock docker.io/n8nio/n8n sha256:4fc4… linux/amd64 linux/arm64 > docker-image.lock.json

if [ "$#" -lt 3 ]; then
  echo "usage: docker-images-lock <repository> <tag | sha256:digest> <platform[=ref]>..." >&2
  exit 2
fi

repository=$1
ref=$2
shift 2

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

reference() {
  case "$1" in
    sha256:*) echo "$repository@$1" ;;
    *) echo "$repository:$1" ;;
  esac
}

entries=()
for argument in "$@"; do
  platform=${argument%%=*}
  if [ "$argument" = "$platform" ]; then
    source=$(reference "$ref")
  else
    source=$(reference "${argument#*=}")
  fi
  os=${platform%/*}
  architecture=${platform#*/}
  file="$work/${os}-${architecture}"

  # Raw bytes are kept in files: the manifest digest covers every byte, including trailing newlines.
  crane manifest "$source" >"$work/top"

  if jq -e '.manifests' "$work/top" >/dev/null; then
    digest=$(jq -r --arg os "$os" --arg architecture "$architecture" '
      [.manifests[] | select(.platform.os == $os and .platform.architecture == $architecture
        and ((.platform.variant // "v8") == "v8" or $architecture != "arm64"))][0].digest // empty
    ' "$work/top")
    [ -n "$digest" ] || { echo "$source has no $platform manifest" >&2; exit 1; }
    crane manifest "$repository@$digest" >"$file"
  else
    cp "$work/top" "$file"
  fi

  jq -e '.mediaType' "$file" >/dev/null || { echo "$source manifest for $platform has no mediaType" >&2; exit 1; }
  digest="sha256:$(sha256sum "$file" | cut -d' ' -f1)"
  config=$(crane blob "$repository@$(jq -r .config.digest "$file")")

  actual=$(jq -r '"\(.os)/\(.architecture)"' <<<"$config")
  [ "$actual" = "$platform" ] || { echo "$repository@$digest is $actual, not $platform" >&2; exit 1; }

  entries+=("$(jq -n --arg platform "$platform" --arg digest "$digest" --rawfile manifest "$file" \
    --argjson diffIds "$(jq -c .rootfs.diff_ids <<<"$config")" \
    '{($platform): {digest: $digest, manifest: $manifest, diffIds: $diffIds}}')")
done

printf '%s\n' "${entries[@]}" | jq -s --arg repository "$repository" --arg ref "$ref" '{repository: $repository, ref: $ref, platforms: add}'
