# Recipes shared by every Docker image folder, named after the folder's image; recipes run from the
# repository root so the flake resolves.

image_root := parent_directory(parent_directory(source_directory()))

# Image name: the importing folder's name, such as `n8n`.
image := file_name(module_directory())

# Flake attribute path of the folder's Docker image derivation, such as `apps.n8n.docker`.
image_package := file_name(parent_directory(module_directory())) + "." + image + ".docker"

# Load the image into the current Docker daemon through its Docker image derivation and print the result: reference, image ID and resolution path.
[group('image')]
image-load:
    cd {{quote(image_root)}} && nix run .#{{image_package}}

# Print the image's recorded metadata (source, platform, identity) without contacting Docker.
[group('image')]
image-info:
    cd {{quote(image_root)}} && nix run .#{{image_package}} -- info
