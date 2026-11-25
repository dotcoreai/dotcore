# Strata agentic instructions

## How it is built

* [`Dockerfile`](Dockerfile) has a stage per platform: linux/amd64 is upstream's CUDA 13 image pinned
  by digest; linux/arm64 is upstream's signed control and engine packages on Ubuntu. Both run the
  same release, so they bump together.
* Upstream's entrypoint always turns an API key on, so the deployment's command is our wrapper,
  [`start.py`](start.py) (`dotcore-strata-start --help`). The adapter owns which arguments it gets.
* The wrapper has three engine paths, picked by its arguments, never guessed from the files. On
  the main engine: a single GGUF read in place, and a pack with a separate draft layer. With
  `--inkling-pack`, the Inkling engine (`strata-inkling`) reads a ready-made Inkling pack in place
  and drafts with the drafter inside it; the main engine's options are refused there.
* It packs split GGUF shards with upstream's `terminal/tools/iq_pack.py` without converting
  tensors, so it only takes files that store them as the engine reads them (ISTA-DASLab's GSQ-RCO
  files do, Unsloth's don't). It doesn't use upstream's converting mode, which needs the GGUF
  library both images ship in `terminal/third_party/strata-gguf-py`.
* The Inkling tokenizer is built from the `tokenizer.json` and `chat_template.jinja` beside the
  pack: reading it from the pack needs a zstd module the image's Python lacks.
* The Inkling engine lowers its context to what fits in free GPU memory without failing, and its
  `/health` may still report the asked value. The wrapper turns its host-RAM KV spill and its
  smaller-chunk start retry off, so an undersized estimate shrinks the context or fails. Check the engine log for an auto-sizing line when a
  context looks too large.
* Nothing here is part of root `just build`, `test` or `check`.
* Shared parameters: [`engines/AGENTS.md`](../AGENTS.md) says where their flags and rules live.

## Update

1. Read the digest of the release's `cuda13` tag:
   `docker buildx imagetools inspect ghcr.io/niko1221/strata:<version>-cuda13`.
2. Verify the release's `strata-feed-linux-aarch64.json` Ed25519 signature against the key in
   upstream's `terminal/strata_control/update_keys.json` (`nix run nixpkgs#openssl -- pkeyutl -verify -pubin -keyform DER -rawin` over the
   feed's exact bytes and its `.sig` file, since the dev shell has no `openssl`; or upstream's `tools/docker/fetch-engines.py`). The
   `.sig` file is JSON with the base64 signature in `value`; the key's raw bytes need the Ed25519
   DER prefix `302a300506032b6570032100` to form a PEM. A failing feed stops the bump. Take the arm64 package names and
   hashes from the verified feed only.
3. Review the upstream changes the wrapper depends on: the run configuration `setup.py` and
   `terminal/strata_control/setup_config.py` write for each engine path (`install_inkling2` for
   Inkling), `terminal/serve/server.py`'s arguments and `/health`, `terminal/serve/inkling_kind.py`,
   `terminal/tools/strata_tokenizer.py`, `terminal/tools/inkling_tokenizer.py`,
   `terminal/tools/iq_pack.py` and its `native_experts.txt`, the image's paths, data files and
   environment, and the pinned model files, including the Inkling packs' Hugging Face revision (a
   changed digest means a new artifact).
4. Update the amd64 digest and the arm64 release, package names (control, engine and the
   `strata-inkling` shape engine), engine folders and checksums in [`Dockerfile`](Dockerfile)
   together. The arm64 folders match the amd64 image's, under `components/`.
5. Any `Dockerfile` or `start.py` change makes both records in `docker-image.stored.json` stale:
   run `just image-store` on an x86_64 Linux host and on a GB10 host and commit both; see
   [Dockerfile images](../../docs/development/images.md#dockerfile-images) for remote runs. A remote
   `just dev::remote::run` leaves its record in the remote workspace: copy its `linux/arm64` entry
   into the local file, then run `just dev::docker-images-check`.
6. Declare a new engine release in the catalog and move the models approving the old one to it.

## Validate

1. `just image-test` on both platforms: the wrapper's refusals, its run configuration for each
   model file layout ([`test_start.py`](test_start.py)), then the keyless mock engine.
2. From the root, through `just dev::remote::run`: `just backend::test-nvidia-5090 strata` and
   `just backend::test-gb10 strata`. Check the host's free memory first; the tests need it.
