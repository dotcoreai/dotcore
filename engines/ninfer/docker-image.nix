{ fetchFromGitHub }:

{
  dockerfile = {
    # Upstream publishes only source; its own Dockerfile builds the image. See AGENTS.md for the
    # pin's constraints before updating it.
    context = fetchFromGitHub {
      owner = "Neroued";
      repo = "ninfer";
      rev = "40dec5ff312809d162cbe41dd5682e054e0bcc00";
      hash = "sha256-jtcZARoSaaHd0UZtPcrIxCxlAUlKggRaFhPbuqXs60E=";
    };
    # Compiled only for the GB202 chip (sm_120a) of the RTX 5090 and RTX PRO 6000.
    platforms = [ "linux/amd64" ];
    records = ./docker-image.stored.json;
  };
}
