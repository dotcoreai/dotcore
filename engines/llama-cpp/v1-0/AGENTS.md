# llama.cpp v1.0.0 agentic instructions

## How it is built

* Same as [`engines/llama-cpp/AGENTS.md`](../AGENTS.md): the lock pins upstream's v1.0.0 CUDA 13 image
  by digest, and `package.nix` installs the v1.0.0 macOS tarball. Nothing is updated here.
* The releases are temporary and exist only for Clef Flash. From llama.cpp v1.1.0 (#32754), clef's
  decision head is built for the micro-batch's maximum questions and options, so its memory grows
  with the square of the context.

## Retire

* On each llama.cpp bump, read the `CHECK` comments here and in the catalog releases. Once a
  release sizes the head by its spans again, move Clef Flash to the current llama.cpp releases and
  re-check its sizing (`model-sizing` skill).
* Then delete this folder, its module and `image-load` entry in [`engines/Justfile`](../../Justfile),
  its two entries in `flake.nix`, and the `llama-cpp-v1-0.yaml` and `llama-cpp-macos-v1-0.yaml`
  catalog releases.
