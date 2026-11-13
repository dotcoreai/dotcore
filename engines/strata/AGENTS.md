# Strata agentic instructions

## How it is built

* [`Dockerfile`](Dockerfile) has a stage per platform: linux/amd64 is upstream's CUDA 13 image pinned
  by digest; linux/arm64 is upstream's signed control and engine packages on Ubuntu, without
  upstream's `/opt/strata/seed` folder. Both run the same release, so they bump together.
* Upstream's entrypoint always turns an API key on, so the deployment's command is our wrapper,
  [`start.py`](start.py) (`dotcore-strata-start --help`). The adapter owns which arguments it gets.
* The wrapper has two engine paths: a single GGUF read in place, and a pack with a separate draft
  layer. It packs split GGUF shards with upstream's `tools/iq_pack.py` without converting tensors,
  so it only takes files that store them as the engine reads them (ISTA-DASLab's GSQ-RCO files do,
  Unsloth's don't). Upstream's converting mode also needs the `seed` folder the arm64 image lacks.
* Nothing here is part of root `just build`, `test` or `check`.
* Shared parameters: [`engines/AGENTS.md`](../AGENTS.md) says where their flags and rules live.

## Update

1. Read the digest of the release's `cuda13` tag:
   `docker buildx imagetools inspect ghcr.io/niko1221/strata:<version>-cuda13`.
2. Verify the release's `strata-feed-linux-aarch64.json` Ed25519 signature against the key in
   upstream's `strata_control/update_keys.json` (`openssl pkeyutl -verify`, or upstream's
   `docker/fetch-engines.py`). A failing feed stops the bump. Take the arm64 package names and
   hashes from the verified feed only.
3. Review the upstream changes the wrapper depends on: the run configuration `setup.py` and
   `strata_control/setup_config.py` write for each engine path, `serve/server.py`'s arguments and
   `/health`, `tools/strata_tokenizer.py`, `tools/iq_pack.py` and its `native_experts.txt`, the
   image's paths, data files and environment, and the pinned model files (a changed digest means a
   new artifact).
4. Update the amd64 digest and the arm64 release, package names, engine folder and checksums in
   [`Dockerfile`](Dockerfile) together.
5. Any `Dockerfile` or `start.py` change makes both records in `docker-image.stored.json` stale:
   run `just image-store` on an x86_64 Linux host and on a GB10 host and commit both; see
   [Dockerfile images](../../docs/development/images.md#dockerfile-images) for remote runs.
6. Declare a new engine release in the catalog and move the models approving the old one to it.

## Validate

1. `just image-test` on both platforms: the wrapper's refusals, its run configuration for each
   model file layout ([`test_start.py`](test_start.py)), then the keyless mock engine.
2. From the root, through `just dev::remote::run`: `just backend::test-nvidia-5090 strata` and
   `just backend::test-gb10 strata`. Check the host's free memory first; the tests need it.
