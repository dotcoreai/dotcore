# Recipes of a Dockerfile Docker image folder: the shared image recipes plus the store build.

import 'image.just'

# Add this host's local build of the image to the Nix store, kept from garbage collection until `just dev::clean` or the checkout's removal, and merge its record for this host's platform into docker-image.stored.json.
[group('image')]
image-store:
    #!/usr/bin/env sh
    set -eu
    file={{quote(module_directory() / "docker-image.stored.json")}}
    cd {{quote(image_root)}}
    record=$(DOTCORE_DOCKER_IMAGE_ROOTS_DIR={{quote(image_root / ".dotcore" / "docker-images")}} nix run .#{{image_package}} -- store-build)
    { [ -e "$file" ] && cat "$file" || echo '{}'; } | jq --argjson record "$record" '. * $record' >"$file.tmp"
    mv "$file.tmp" "$file"
    echo "recorded $(echo "$record" | jq -r 'keys[0]') in $file"
