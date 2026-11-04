{ fetchFromGitHub }:

{
  dockerfile = {
    # Upstream publishes only source; its own Dockerfile builds the image. See AGENTS.md for the
    # pin's constraints before updating it.
    context = fetchFromGitHub {
      owner = "Neroued";
      repo = "ninfer";
      rev = "a754fcf3bf524494aa927c2f54e0cbfb58f569ab";
      hash = "sha256-Yn3SxVbVm1Dug0mDcpjQHtXoJsVXo4S6SPd5v3GHqLE=";
    };
    # Compiled only for the GB202 chip (sm_120a) of the RTX 5090 and RTX PRO 6000.
    platforms = [ "linux/amd64" ];
    records = ./docker-image.stored.json;
  };
}
