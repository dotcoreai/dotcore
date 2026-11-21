# vLLM agentic instructions

## How it is built

* [`docker-image.lock.json`](docker-image.lock.json) pins the upstream image by digest; nothing is
  built here. The node pulls it on first use or loads its stored image, and never pushes it. The
  release declares the driver floor.
* Model configurations name the `linux-amd64-cuda` and `linux-arm64-cuda` artifact ids; keep both.
* Nothing here is part of root `just build`, `test` or `check`.
* Shared parameters: [`engines/AGENTS.md`](../AGENTS.md) says where their flags and rules live.
* [`inkling-2-mini/`](inkling-2-mini/) holds a pre-release image for one model family, with its
  own `AGENTS.md`, Just module and catalog release.

## Update

1. Pick a stable release tag. Use the unsuffixed `v<version>` tag, a CUDA 13 build. Confirm from the
   image config, not the name:
   `docker buildx imagetools inspect --format '{{json .Image}}' vllm/vllm-openai:<tag>`
   (`CUDA_VERSION`, `NVIDIA_REQUIRE_CUDA` per platform).
2. Confirm the vLLM version inside (development builds report `0.1.dev…`):
   `docker run --rm --entrypoint python3 vllm/vllm-openai:<tag> -c 'import vllm; print(vllm.__version__)'`.
3. `just image-lock docker.io/vllm/vllm-openai:<tag>`. If the tag has no index covering both
   platforms, name each: `linux/amd64=<tag> linux/arm64=<arm64 tag>`.
4. Audit the flags the adapter emits (`backend/core/src/model/engine/vllm/plan.rs`) against the
   upstream changes; upstream can also change a default without touching a flag.
   Also check the KV block layout at the new tag: model sizing counts the blocks each request
   holds, and upstream can change block sizes or per-request blocks without a flag change. A
   launch's logged KV capacity in tokens must match what the model's sizing equation predicts.
5. Declare a new release `vllm-v<major>-<minor>-<patch>` with the new `version`, keep the parameter
   schema id if its parameters are unchanged, and move every model approving the old release to it.

## Validate

1. On an NVIDIA host: `just image-load`, then from the root
   `just dev::workload-launch <model> <profile> --device nvidia.com/gpu=0` with a small model
   approving the new release; the workload answers a request.
2. From the root, run the gated suites of each platform the release declares. They deploy the
   engine from the workspace's development source through its loader, as a node does:
   * linux/amd64: `just backend::test-nvidia` (text, images and tool calls, including forced calls
     to a tool without parameters), then `just backend::test-nvidia-5090` on an idle RTX 5090
   * linux/arm64: `just backend::test-nvidia` and `just backend::test-gb10` on a GB10 host
