# vLLM for Inkling-2-Mini agentic instructions

## How it is built

* [`docker-image.lock.json`](docker-image.lock.json) pins upstream's `inkling-2-mini` image by index
  digest; nothing is built here. The node pulls it on first use or loads its stored image, and never
  pushes it. The release declares the driver floor.
* The image is a day-zero build for one model family, not a stable release, and its labels carry no
  source revision: `ai.vllm.build.commit` reads `unknown` and `ai.vllm.image.tag` reads
  `local/vllm-openai:dev`. The installed vLLM package is the only source pointer, and it carries a
  short commit prefix in its version.
* Its prebuilt kernels target datacenter Blackwell (`TORCH_CUDA_ARCH_LIST=10.0+PTX 10.3+PTX`), so a
  model whose kernels are missing for a consumer card fails there with `no kernel image is available
  for execution on the device`; the models this release approves are the ones known to run on it.
* Model configurations name the `linux-amd64-cuda` and `linux-arm64-cuda` artifact ids; keep both.
* Nothing here is part of root `just build`, `test` or `check`.
* Shared parameters: [`engines/AGENTS.md`](../../AGENTS.md) says where their flags and rules live.
* The release is temporary. Its `CHECK` comments say which upstream release it waits for. Once a
  stable image runs the same models and passes the same qualification, move the models to the stable
  release, then delete this folder, its module and `image-load` entry in
  [`engines/Justfile`](../../Justfile), its `engineImages` entry in `flake.nix`, and its catalog
  release.

## Update

1. Read the `CHECK` comments first: if a stable vLLM release now runs the models, retire this
   engine instead (How it is built).
2. Read the index digest of the current `inkling-2-mini` tag:
   `docker buildx imagetools inspect docker.io/vllm/vllm-openai:inkling-2-mini`. Pin that digest, not
   the tag, which upstream can move.
3. Confirm from each platform's image config, not the name: `CUDA_VERSION` and `NVIDIA_REQUIRE_CUDA`
   (`docker buildx imagetools inspect ... --format '{{json .Image}}'`).
4. Confirm the vLLM build inside:
   `docker run --rm --entrypoint python3 <image> -c 'import vllm; print(vllm.__version__)'`.
5. `just image-lock docker.io/vllm/vllm-openai@sha256:… linux/amd64 linux/arm64`.
6. Audit the flags the adapter emits (`backend/core/src/model/engine/vllm/plan.rs`) against this
   build, and check its KV block layout: model sizing counts the blocks each request holds, so a
   launch's logged KV capacity in tokens must match what the model's sizing equation predicts.
7. Set the release's `version` to the vLLM build from step 4, with the measured driver floors.

## Validate

1. On each platform: `just image-load`, then inspect the loaded image for the CUDA version and
   driver requirement, and start it against the model it exists for.
2. From the root, run the gated suite of each platform the release declares, as
   [`engines/vllm/AGENTS.md`](../AGENTS.md) describes.
