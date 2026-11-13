# llama.cpp agentic instructions

## How it is built

* Linux: [`docker-image.lock.json`](docker-image.lock.json) pins the upstream image by digest; the
  node pulls it on first use or loads its stored image. The release declares the driver floor.
* macOS: `engines.llama-cpp.native` ([`package.nix`](package.nix)), `aarch64-darwin` only. The
  release tarball is installed unmodified (rewriting the Mach-O files would break their
  signatures): tools and dylibs under `libexec/llama-cpp/`, linked into `bin/`; the main program is
  `llama-server`.
* The package builds with the repository flake's nixpkgs, so a root nixpkgs bump can change its
  store path; existing deployments keep the store path they pinned.
* Nothing here is part of root `just build`, `test` or `check`.
* Shared parameters: [`engines/AGENTS.md`](../AGENTS.md) says where their flags and rules live.

## Update

1. Target the newest stable release (`v<semver>`, such as `v1.0.0`), not a nightly. Each stable
   release is also a nightly build tag (`b<build>`) on the same commit; both artifacts are keyed on
   that build, and nightlies don't all get an image.
2. Linux image: tagged `server-cuda13-v<semver>-b<build>-<sha7>` (the `v<semver>` release page
   names the commit). Lock it by its image index digest, so the lock stays immutable:
   `just image-lock ghcr.io/ggml-org/llama.cpp@sha256:<index digest>`. Images appear after the
   release tarballs, so a new release can lack one for a while.
3. macOS package: the asset `llama-b<build>-bin-macos-arm64.tar.gz` sits on the `b<build>` release,
   not the `v<semver>` one. Set `version` in [`package.nix`](package.nix) to `b<build>`, and
   `src.hash` to the asset's SHA-256 in SRI form (`sha256-...`): the release asset's `digest`, or
   `nix store prefetch-file` on its URL; convert hex with `nix hash convert --hash-algo sha256 <hex>`.
4. Declare new releases with `b<build>` as `version` and the semver in a comment:
   `llama-cpp-linux-<date>` in `llama-cpp.yaml` and `llama-cpp-<date>` in `llama-cpp-macos.yaml`,
   after the build's publish date, and move every model approving the old releases to them. Releases
   are immutable, and the macOS package `version` must equal its release's `version`
   (`checks.dotcore-catalog` fails otherwise); keep an older release by pinning it to the
   `github:dotcoreai/dotcore/<rev>` revision that built it.

## Validate

1. `just smoke` (on a Mac): prints the package store path and `llama-server --version`; the build
   number must match `b<build>`. No model or GPU needed.
2. Linux image, on an NVIDIA host: `just image-load`, then from the root
   `just dev::workload-launch <model> <profile> --device nvidia.com/gpu=0` with a small model
   approving the new release; the workload answers a request.
3. `just backend::test-nvidia` on an NVIDIA host (from the root) deploys the engine from the
   workspace's development source through its loader, as a node does.
