# ninfer agentic instructions

## How it is built

* [`docker-image.nix`](docker-image.nix) builds upstream's own Dockerfile at a pinned commit; no
  Dockerfile is vendored here. It targets `sm_120a`, and the package exists only for
  `x86_64-linux`.
* The `nvidia-gb202` hardware class lists, by reported GPU name, the cards that build runs on. A
  card joins ninfer only through that list; GPU memory alone never qualifies it.
* The node loads the stored image when it has one, otherwise builds it locally. The build pulls the
  public `nvidia/cuda:13.1.2-devel-ubuntu24.04` and `-runtime-ubuntu24.04` images and the source
  from GitHub.
* A present build of the same recipe is reused. To force a rebuild when no stored image exists,
  remove its source tag (`dotcore-nix/ninfer:<recipe hash>`); with a stored image, the load copies
  it instead.
* The CUDA base images are tag-pinned, so one commit can build different bytes over time; a
  deployment keeps the loader store path it pinned.
* The image is never pushed; its recipes are not part of root `just build`, `test` or `check`
  (heavy, GPU-specific build).
* Shared parameters: [`engines/AGENTS.md`](../AGENTS.md) says where their flags and rules live.

## Update

1. Review upstream changes since the pin: the Dockerfile, `CMakeLists.txt`,
   `cmake/Dependencies.cmake`, the flag schema in `src/product/config/schema.cpp`,
   `docs/startup-configuration.md`. ninfer rejects unknown flags, so a renamed flag fails startup.
   * A force-pushed upstream branch can drop the pinned commit; pick one that still exists.
   * The adapter sizes the device context pool in bytes from the configuration's pool record
     sizes. Check them against upstream's pool layout (`src/core/context_pool.cpp` and the model's
     state and KV layout) when it changes.
2. Set `rev` in [`docker-image.nix`](docker-image.nix) to the full 40-character SHA and update
   `hash` (`nix flake prefetch github:Neroued/ninfer/<sha>`).
3. `just image-store` on an x86_64 Linux host: it updates the `linux/amd64` entry of
   `docker-image.stored.json`; commit it.
4. Set the release id (`ninfer-<short-sha>`) and version in the catalog declaration, and the release
   id in the models that approve it.

## Validate

1. `just image-test`: prints the absolute `ninfer-serve` path and checks it is executable. It
   cannot launch it (that needs `libcuda.so.1` from a GPU host).
2. Run `ldd` on `ninfer-serve` inside the image: `libcuda.so.1` is the only library allowed to be
   missing.
3. Launch the profile with `just dev::workload-launch` and the custom argument token
   `--log-level=debug`, and read its startup `context` lines with `dotc dev workload logs`: the
   block size must equal the configuration's state record size, and the token estimate must cover
   the total context.
4. `just backend::test-nvidia-5090` on an RTX 5090 that no other workload uses (from the root)
   deploys the engine from the workspace's development source through its loader and serves the
   profile. It needs the profile's `.ninfer` artifact in the shared cache and about 32 GB of free
   GPU memory.
