# llama.cpp agentic instructions

## How it is built

* Linux: [`docker-image.lock.json`](docker-image.lock.json) pins upstream's CUDA image by digest.
  The release declares the driver floor.
* macOS: [`package.nix`](package.nix) installs upstream's macOS arm64 release tarball unmodified;
  rewriting its Mach-O files would break their signatures.
* The package builds with the repository flake's nixpkgs, so a root nixpkgs bump can change its
  store path; existing deployments keep the store path they pinned.
* Nothing here is part of root `just build`, `test` or `check`.

## Update

* Target upstream's newest stable release (`v<semver>`), not a nightly. It shares its commit with a
  nightly build tag (`b<build>`): both artifacts are published under that build, and it is the
  release `version` (note the semver in a comment).
* Lock the release's CUDA image by its index digest, never by tag. Images can lag the tarballs, and
  not every nightly gets one.
* The macOS package `version` must equal its release's `version`; the catalog check fails
  otherwise.
* Releases are immutable: declare new ones with the date convention of `llama-cpp.yaml` and
  `llama-cpp-macos.yaml`, and move every model approving the old ones.

## Validate

* `just smoke` on a Mac checks the package and the build number it reports.
* The backend's hardware test recipes on each platform in the root "Platform scope":
  `test-nvidia` (NVIDIA hosts), `test-gb10` (GB10) and `test-apple` (Mac).
