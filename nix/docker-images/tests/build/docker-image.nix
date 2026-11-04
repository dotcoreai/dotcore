{ lib }:

{
  dockerfile = {
    context = lib.fileset.toSource {
      root = ./.;
      fileset = ./Dockerfile;
    };
    buildArgs.GREETING = "hello";
    platforms = [
      "linux/amd64"
      "linux/arm64"
    ];
    records = ./docker-image.stored.json;
  };
}
