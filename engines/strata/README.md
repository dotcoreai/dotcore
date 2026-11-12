# Strata engine

[Strata](https://github.com/Niko1221/Strata), for large mixture-of-experts models on NVIDIA GPUs,
splitting the model between GPU and host memory.

## Good to know

* An NVIDIA GPU and driver 580 or newer.
* The experts that don't fit on the GPU stay in pinned host memory, so a deployment can use as much
  host memory as GPU memory.
* The machine downloads the engine the first time a model uses it.
* Strata runs without an API key: anything that can reach its port can use the model.

Maintaining it: [`AGENTS.md`](AGENTS.md).
