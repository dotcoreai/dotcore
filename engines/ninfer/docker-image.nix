{ fetchFromGitHub }:

{
  dockerfile = {
    # Upstream publishes only source; its own Dockerfile builds the image. See README.md for the
    # pin's constraints before updating it.
    context = fetchFromGitHub {
      owner = "Neroued";
      repo = "ninfer";
      rev = "193675826f62ed229d99154ace3075ba3a6cd968";
      hash = "sha256-aKzu3so6YolRoOPafrFxBWhch993qOxuOQokZWMSUVU=";
    };
    # Compiled only for the RTX 5090 (sm_120a).
    platforms = [ "linux/amd64" ];
    records = ./docker-image.stored.json;
  };
}
