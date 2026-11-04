{ lib }:

{
  dockerfile = {
    # Only the build inputs, so documentation and test edits keep the recipe and its stored images.
    # A plain source set has the same store path on every system, so Linux and macOS hosts agree
    # on the linux/arm64 recipe.
    context = lib.fileset.toSource {
      root = ./.;
      fileset = lib.fileset.unions [
        ./Dockerfile
        ./entrypoint.sh
        ./extra-model-paths.yaml
        ./custom_nodes
      ];
    };
    # ComfyUI publishes no image: the Dockerfile fetches its source at this commit.
    buildArgs = {
      COMFYUI_REV = "483dae4326356b81de05d27aca3f433dd323cc25";
    };
    platforms = [
      "linux/amd64"
      "linux/arm64"
    ];
    records = ./docker-image.stored.json;
  };
}
