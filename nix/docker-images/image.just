# Recipes shared by every Docker image folder, named after the folder's image; recipes run from the
# repository root so the flake resolves.

image_root := parent_directory(parent_directory(source_directory()))

# Area and image name from the importing folder's path: `apps/n8n` is image `n8n` in area `apps`,
# and a variant nested under its engine, `engines/vllm/inkling-2-mini`, is `vllm-inkling-2-mini`.
image_folder := trim_start_match(module_directory(), image_root + "/")
image_area := replace_regex(image_folder, '/.*', '')
image := replace(trim_start_match(image_folder, image_area + "/"), "/", "-")

# Flake attribute path of the folder's Docker image derivation, such as `apps.n8n.docker`.
image_package := image_area + "." + image + ".docker"

# Load the image into the current Docker daemon through its Docker image derivation and print the result: reference, image ID and resolution path.
[group('image')]
image-load:
    cd {{quote(image_root)}} && nix run .#{{image_package}}

# Print the image's recorded metadata (source, platform, identity) without contacting Docker.
[group('image')]
image-info:
    cd {{quote(image_root)}} && nix run .#{{image_package}} -- info
