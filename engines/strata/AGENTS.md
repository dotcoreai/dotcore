# Strata agentic instructions

## How it is built

* [`Dockerfile`](Dockerfile) has a stage per platform and adds [`start.py`](start.py) as
  `dotcore-strata-start` to either:
  * linux/amd64: upstream's published CUDA 13 image, pinned by digest;
  * linux/arm64: upstream's control and CUDA 13 engine packages from the same release, pinned by the
    hashes upstream's signed `strata-feed-linux-aarch64.json` lists, on an Ubuntu base, with
    upstream's image layout and environment except its `/opt/strata/seed` folder (the bundled
    llama.cpp `gguf-py` that pack conversion falls back to).
* Both platforms run the same release with the same wrapper and paths, so they bump together.
* The node loads the stored image when it has one, otherwise builds the `Dockerfile` on the first
  deployment.
* Upstream's entrypoint always turns an API key on, so the image's entrypoint is only its init, and
  the deployment's command is the wrapper. The wrapper extracts the tokenizer and writes Strata's run
  configuration under `/data`, then execs Strata's server keyless and offline on the port it is
  given. `dotcore-strata-start --help` lists its arguments; it refuses values it can't use.
* The adapter owns which arguments a deployment gets, including the model's architecture and MTP
  draft depth from its catalog configuration. The wrapper fixes what isn't a model or deployment
  choice: the native engine path (no pack, no separate draft layer), the engine's own prompt chunk
  sizing and keyless offline serving.
* Context is per request: the engine keeps one session of that size per parallel request.
* Without an expert VRAM cap, Strata fills whatever GPU memory is free at start; with one, the
  experts beyond it stay in pinned host memory.
* `/data` holds the tokenizer, the run configuration and the engine log, all root-owned.
* Readiness: the port refuses connections until the model is loaded, then `/health` reports
  `"loaded": true`.
* The image is never pushed; its recipes are not part of root `just build`, `test` or `check`.
* Shared parameters: [`engines/AGENTS.md`](../AGENTS.md) says where their flags and rules live.

## Update

1. Read the digest of the release's `cuda13` tag:
   `docker buildx imagetools inspect ghcr.io/niko1221/strata:<version>-cuda13`.
2. Download the release's `strata-feed-linux-aarch64.json` and `.json.sig`, and verify the Ed25519
   signature against the key in upstream's `strata_control/update_keys.json`
   (`openssl pkeyutl -verify`, or upstream's `docker/fetch-engines.py`). A feed that fails stops the
   bump. Take the control and `cuda13` engine package names and sha256 hashes from the verified feed
   only.
3. Review upstream changes since the pin, which the wrapper depends on:
   * the run configuration that `install_own_path` writes in `setup.py` (keys, engine arguments);
   * `serve/server.py`'s arguments, `/health` body and keyless serving;
   * `tools/strata_tokenizer.py` and the image's paths (`/opt/strata/terminal`, the bundled Python,
     the engine component);
   * upstream's `Dockerfile`: the apt packages, environment and layout the arm64 stage copies;
   * the pinned model files (`QWEN36_FILES` in `setup.py`): a changed digest means a new artifact.
4. Qwen3.8 models convert into a pack on first start. An engine bump may change the pack format and
   force that conversion again, which takes minutes and tens of GB.
5. In [`Dockerfile`](Dockerfile), replace the digest on the amd64 `FROM` line, and the release,
   package names, engine folder and `--checksum` hashes of the arm64 stage, together. Then
   `just image-test` on an x86_64 Linux host and on a GB10 host.
6. Changing `Dockerfile` or `start.py` makes both build records in `docker-image.stored.json` stale:
   run `just image-store` on an x86_64 Linux host and on a GB10 host, then commit both entries; see
   [Dockerfile images](../../docs/development/images.md#dockerfile-images) for running it remotely.
7. Declare a new engine release in the catalog and move the models approving the old one to it.

## Validate

1. `just image-test` on each platform: the wrapper's refusals, then Strata's server keyless and
   offline with its mock engine, until `/health` reports it loaded.
2. On an NVIDIA host with the model's GGUF cached, check free GPU and host memory, then deploy a
   model on Strata: it reaches Ready and answers a chat completion with the deployment's model
   name.
