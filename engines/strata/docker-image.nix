{ lib }:

{
  dockerfile = {
    # Only the build inputs, so documentation and test edits keep the recipe and its stored image.
    context = lib.fileset.toSource {
      root = ./.;
      fileset = lib.fileset.unions [
        ./Dockerfile
        ./start.py
      ];
    };
    # Upstream publishes its prebuilt images for x86_64 only.
    platforms = [ "linux/amd64" ];
    records = ./docker-image.stored.json;
  };
}
