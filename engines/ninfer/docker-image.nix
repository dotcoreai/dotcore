{ fetchFromGitHub }:

{
  dockerfile = {
    # Upstream publishes only source; its own Dockerfile builds the image. See AGENTS.md for the
    # pin's constraints before updating it.
    context = fetchFromGitHub {
      owner = "Neroued";
      repo = "ninfer";
      rev = "13ebb92f6f1d56b676e5300cb19555b2ba24053a";
      hash = "sha256-4g9EAShDAtAI41n638oZyXZPblgYodX5u7l3MflGhKo=";
    };
    # Compiled only for the GB202 chip (sm_120a) of the RTX 5090 and RTX PRO 6000.
    platforms = [ "linux/amd64" ];
    records = ./docker-image.stored.json;
  };
}
