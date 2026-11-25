# Upstream's official vLLM nightly image (plain `nightly-<commit>`), pinned by digest.
# CHECK(https://github.com/vllm-project/vllm/pull/69028): in a stable vLLM release
#   -> move Inkling-2-Mini to the stable vLLM engine and delete this folder and its catalog release.
{ }:

{
  registry.lock = ./docker-image.lock.json;
}
