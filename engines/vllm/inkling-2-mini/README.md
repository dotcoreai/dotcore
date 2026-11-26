# vLLM engine for Inkling

[vLLM](https://github.com/vllm-project/vllm), in an upstream nightly build that runs
looped-architecture models such as Inkling-2-Mini, for NVIDIA GPUs on Linux.

## Good to know

* Linux only, x86_64 and arm64, with an NVIDIA GPU and driver 580 or newer.
* Only models that need this build approve it; every other model keeps the stable vLLM engine.
* The machine downloads the engine the first time a model uses it.
* It comes from an upstream nightly image, so it is replaced by the stable engine once that one
  runs the same models.

Maintaining it: [`AGENTS.md`](AGENTS.md).
