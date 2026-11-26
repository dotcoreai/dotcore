# llama.cpp engine v1.0.0

[llama.cpp](https://github.com/ggml-org/llama.cpp) v1.0.0, kept for the Clef Flash decision model,
for NVIDIA GPUs on Linux and Apple Silicon Macs.

## Good to know

* Only Clef Flash uses it; every other model runs the current llama.cpp engine.
* Newer llama.cpp releases need far more memory for Clef Flash at its usual contexts, so it stays on
  this version until a release fixes that.
* On Linux, it needs an NVIDIA GPU with driver 580 or newer. The machine downloads the engine the
  first time the model uses it.

Maintaining it: [`AGENTS.md`](AGENTS.md).
