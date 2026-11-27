# oMLX agentic instructions

## How it is built

* [`package.nix`](package.nix) installs upstream's macOS 26-27 release app unmodified; rewriting its
  Mach-O files would break their signatures. It is unpacked with `7zz`, since `undmg` can't read
  the image.
* [`launcher.py`](launcher.py) is the package launcher, installed as `bin/omlx-launcher` and run by
  the bundle's Python. Adapters emit plain flags only; the launcher writes oMLX's settings files
  into the workload's working directory, links the model snapshot, sets the environment and execs
  the server. Its flags are `--help`; keep every oMLX quirk here or in the adapter.
* The package builds with the repository flake's nixpkgs.
* Nothing here is part of root `just build`, `test` or `check`.

## macOS floor

* The pinned release is the macOS 26-27 build and the launcher refuses to start below macOS 26.
  Artifact selectors carry no OS version, so only one build can be pinned.
* Moving the floor means a different upstream build: change the DMG name in `package.nix`, the
  launcher's default minimum, and every doc that names macOS 26.

## Update

* Take upstream's newest stable release that still publishes a `macos26-27` DMG.
* Set `version` and the DMG `hash` in `package.nix`; the version must equal the catalog release's
  version (the catalog check fails otherwise).
* Rerun the phase checks on a Mac with the new build, under a cleared environment from an empty
  directory like Dotcore's launcher: the server starts and `/health` turns healthy, a chat request
  streams, a too-long prompt returns `context_length_exceeded`, and no file is written outside
  the working directory.
* Check that the settings keys and server flags the launcher writes still exist in the new
  release, and that `ssd_cache_max_size` must still be positive.
* Re-measure the memory overshoot: force a low ceiling under full load and compare peak
  `phys_footprint` to it. The adapter divides its estimate by that constant (1.07 at 0.8.4); update
  it when the measurement moves. Never run long full-load tests on a personal laptop.
* Re-check the refusal margin: oMLX refuses a prompt that would cross 95% of the ceiling, so each
  model's estimate must stay at least 1.126 times its engine memory plus 1.07 times its prompt
  cache (1.07 / 0.95). Update the catalog sizing when the margin or the overshoot changes.
* Re-check the memory guard's refusal forms (HTTP 400 and the in-stream error frame) and the
  adapter's error classification.
* Releases are immutable: declare the new one in `catalog/engines/omlx.yaml` and move every model
  approving the old one.

## Validate

* `just smoke` on a Mac checks the package version, the launcher's generated settings and its
  macOS refusal. It loads no model.
* The backend's `test-apple` recipe on a Mac, in the root "Platform scope".
