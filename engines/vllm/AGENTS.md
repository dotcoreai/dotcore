# vLLM agentic instructions

## How it is built

* [`docker-image.lock.json`](docker-image.lock.json) pins the upstream image by digest; nothing is
  built here. The node pulls it on first use or loads its stored image, and never pushes it. The
  release declares the driver floor.
* Model configurations name the `linux-amd64-cuda` and `linux-arm64-cuda` artifact ids; keep both.
* Nothing here is part of root `just build`, `test` or `check`.
* Shared parameters: [`engines/AGENTS.md`](../AGENTS.md) says where their flags and rules live.

## Update

1. Pick a stable release tag. The unsuffixed `v<version>` tag is the CUDA 13 build; suffixed tags
   such as `-cu129` target other CUDA versions. Confirm from the image config, not the name:
   `docker buildx imagetools inspect --format '{{json .Image}}' vllm/vllm-openai:<tag>`
   (`CUDA_VERSION`, `NVIDIA_REQUIRE_CUDA` per platform).
2. Confirm the vLLM version inside (development builds report `0.1.dev…`):
   `docker run --rm --entrypoint python3 vllm/vllm-openai:<tag> -c 'import vllm; print(vllm.__version__)'`.
3. `just image-lock docker.io/vllm/vllm-openai:<tag>`. If the tag has no index covering both
   platforms, name each: `linux/amd64=<tag> linux/arm64=<arm64 tag>`.
4. Audit the flags the adapter emits (`backend/core/src/model/engine/vllm/plan.rs`) against the
   upstream changes; upstream can also change a default without touching a flag.
5. Declare a new release `vllm-v<major>-<minor>-<patch>` with the new `version`, keep the parameter
   schema id if its parameters are unchanged, and move every model approving the old release to it.

## Validate

1. On an NVIDIA host: `just image-load`, then from the root
   `just dev::workload-launch <model> <profile> --device nvidia.com/gpu=0` with a small model
   approving the new release; the workload answers a request.
2. `just backend::test-nvidia` on an NVIDIA host (from the root): its vLLM test covers text, images
   and tool calls, including forced calls to a tool without parameters. It deploys the engine from
   the workspace's development source through its loader, as a node does.
