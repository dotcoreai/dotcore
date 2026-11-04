# Recipes of a registry Docker image folder: the shared image recipes plus the lock refresh.

import 'image.just'

# Regenerate docker-image.lock.json from an upstream `repository:tag` or `repository@sha256:…` (default: the lock's own), for the lock's platforms or the given `platform[=tag|sha256:…]` arguments.
[group('image')]
image-lock reference="" *platforms:
    #!/usr/bin/env bash
    set -euo pipefail
    lock={{quote(module_directory() / "docker-image.lock.json")}}
    reference={{quote(reference)}}
    platforms=({{platforms}})

    if [ -z "$reference" ]; then
      [ -e "$lock" ] || { echo "no $lock yet: name an upstream reference" >&2; exit 1; }
      repository=$(jq -r .repository "$lock")
      ref=$(jq -r .ref "$lock")
    elif [[ "$reference" == *@* ]]; then
      repository=${reference%@*}
      ref=${reference#*@}
    elif [[ "${reference##*/}" == *:* ]]; then
      repository=${reference%:*}
      ref=${reference##*:}
    else
      echo "$reference names no tag or digest: use repository:tag or repository@sha256:…" >&2
      exit 1
    fi

    if [ "${#platforms[@]}" -eq 0 ]; then
      [ -e "$lock" ] || { echo "no $lock yet: name the platforms to lock, such as linux/amd64 linux/arm64" >&2; exit 1; }
      mapfile -t platforms < <(jq -r '.platforms | keys[]' "$lock")
    fi

    cd {{quote(image_root)}}
    # The new lock replaces the current one only once the lock tool succeeds.
    if ! nix run .#tools.docker-images-lock -- "$repository" "$ref" "${platforms[@]}" >"$lock.tmp"; then
      rm -f "$lock.tmp"
      exit 1
    fi
    mv "$lock.tmp" "$lock"
    package=$(nix eval --raw .#{{image_package}}.outPath)
    echo "locked $repository $ref (${platforms[*]}) into $lock; package $package"
