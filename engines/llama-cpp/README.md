# llama.cpp engine

[llama.cpp](https://github.com/ggml-org/llama.cpp), for NVIDIA GPUs on Linux and Apple Silicon
Macs.

## Good to know

* On Linux, it needs an NVIDIA GPU with driver 580 or newer. The machine downloads the engine the
  first time a model uses it.
* On a Mac, models run directly on the GPU, outside Docker.
* The Linux and Mac versions are updated separately, so they can differ.

Maintaining it: [`AGENTS.md`](AGENTS.md).
