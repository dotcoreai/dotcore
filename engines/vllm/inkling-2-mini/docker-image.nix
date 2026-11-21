# Upstream's pre-release Inkling-2-Mini image, pinned by digest.
# WAIT: vLLM PR #69028 (https://github.com/vllm-project/vllm/pull/69028) in a stable release, then
# move Inkling-2-Mini to the stable vLLM engine and delete this folder and its catalog release.
{ }:

{
  registry.lock = ./docker-image.lock.json;
}
