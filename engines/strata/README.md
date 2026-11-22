# Strata engine

[Strata](https://github.com/Niko1221/Strata), for large mixture-of-experts models on NVIDIA GPUs,
splitting the model between GPU memory, host memory and disk.

## Good to know

* An NVIDIA GPU and driver 580 or newer.
* For most models, advanced deployment parameters place the experts. The hottest go in the GPU's
  budget. On a dedicated GPU, models that support it pin the next ones in a host memory budget,
  and the rest are read from the model files on disk. Other models keep every expert the GPU
  doesn't hold in pinned host memory. Defaults come from each model's sizing.
* Inkling models place their experts themselves: the GPU takes as many as fit, and the rest stay
  in host memory.
* A smaller host memory budget saves memory but makes long prompts and decoding slower.
* The machine downloads the engine the first time a model uses it.
* Strata runs without an API key: anything that can reach its port can use the model.

Maintaining it: [`AGENTS.md`](AGENTS.md).
