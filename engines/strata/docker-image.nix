{ lib }:

{
  dockerfile = {
    # Only the build inputs, so documentation and test edits keep the recipe and its stored images.
    context = lib.fileset.toSource {
      root = ./.;
      fileset = lib.fileset.unions [
        ./Dockerfile
        ./start.py
      ];
    };
    platforms = [
      "linux/amd64"
      "linux/arm64"
    ];
    records = ./docker-image.stored.json;
  };
}
